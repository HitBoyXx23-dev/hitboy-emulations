//! The Xbox 360 machine: guest memory, loaded title, guest threads on our Xenon core and the
//! scheduler. The kernel services themselves live in `kernel.rs`.

use crate::xex::Xex;
use cpuxenon::{Context, Cpu, Event};
use std::collections::HashMap;

/// Hardware registers (GPU etc.) will live here; for now unmapped reads return 0.
pub struct Mmio;

impl cpuxenon::Bus for Mmio {
    fn read(&mut self, _addr: u32, _size: u32) -> u64 {
        0
    }
    fn write(&mut self, _addr: u32, _value: u64, _size: u32) {}
}

pub const PAGE: u32 = 0x1000;
/// Physical RAM, mirrored at 0xA0000000 (64 KB pages), 0xC0000000 (16 MB) and 0xE0000000 (4 KB).
pub const PHYS_SIZE: usize = 512 << 20;
const PHYS_BASES: [u32; 3] = [0xA000_0000, 0xC000_0000, 0xE000_0000];
const VIRT_BASE: u32 = 0x4000_0000;
const VIRT_END: u32 = 0x7F00_0000;

pub const QUANTUM: u32 = 20_000;
/// Guest cycles per 100 ns tick of system time (3.2 GHz core).
pub const CYCLES_PER_TICK: u64 = 320;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadState {
    Ready,
    Suspended,
    Waiting,
    Exited,
}

#[derive(Clone, Debug)]
pub struct Wait {
    pub objects: Vec<u32>,
    pub all: bool,
    /// Absolute guest time (100 ns ticks) when the wait gives up.
    pub deadline: Option<u64>,
}

pub struct Thread {
    pub id: u32,
    pub ctx: Context,
    pub kthread: u32,
    pub pcr: u32,
    pub handle: u32,
    pub state: ThreadState,
    pub wait: Option<Wait>,
    pub tls_dynamic: u32,
    pub suspend: u32,
}

/// Why `Machine::run` returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The budget ran out; call run again.
    Budget,
    /// The title exited (ExTerminateTitleProcess, XamLoaderTerminateTitle...).
    Exited(u32),
    /// Every thread is waiting forever.
    Deadlock,
    /// Something we can't continue from.
    Error(String),
}

pub struct Thunk {
    pub module: &'static str,
    pub ordinal: u16,
    pub name: &'static str,
}

pub struct Machine {
    pub cpu: Cpu<Mmio>,
    _phys: Box<[u8]>,
    chunks: Vec<Box<[u8]>>,
    pub(crate) regions: Vec<(u32, u32)>,
    virt_next: u32,
    pub(crate) phys_next: u32,
    pool_next: u32,
    pool_end: u32,
    pub(crate) threads: Vec<Thread>,
    pub(crate) current: usize,
    next_thread_id: u32,
    pub(crate) handles: HashMap<u32, u32>,
    next_handle: u32,
    pub(crate) mutant_owner: HashMap<u32, u32>,
    pub(crate) thunks: Vec<Thunk>,
    pub(crate) thunk_base: u32,
    pub(crate) exit_thunk: u32,
    pub(crate) variables: HashMap<(&'static str, u16), u32>,
    pub(crate) tls_used: u64,
    pub(crate) static_tls: Option<(u32, u32, u32)>,
    pub(crate) xex: Option<Xex>,
    pub(crate) xex_header: u32,
    pub(crate) ldr_entry: u32,
    pub(crate) unimplemented: HashMap<&'static str, u32>,
    pub cycles: u64,
    pub log: Vec<String>,
    pub(crate) exit_code: Option<u32>,
    pub frames: u64,
}

impl Machine {
    pub fn new() -> Machine {
        let mut cpu = Cpu::new(Mmio);
        let mut phys = vec![0u8; PHYS_SIZE].into_boxed_slice();
        for base in PHYS_BASES {
            // SAFETY: phys is owned by the machine and never reallocated.
            unsafe { cpu.map(base, phys.as_mut_ptr(), PHYS_SIZE) };
        }
        let mut m = Machine {
            cpu,
            _phys: phys,
            chunks: Vec::new(),
            regions: Vec::new(),
            virt_next: VIRT_BASE,
            phys_next: 0x0100_0000,
            pool_next: 0,
            pool_end: 0,
            threads: Vec::new(),
            current: 0,
            next_thread_id: 1,
            handles: HashMap::new(),
            next_handle: 0xF800_0000,
            mutant_owner: HashMap::new(),
            thunks: Vec::new(),
            thunk_base: 0,
            exit_thunk: 0,
            variables: HashMap::new(),
            tls_used: 0,
            static_tls: None,
            xex: None,
            xex_header: 0,
            ldr_entry: 0,
            unimplemented: HashMap::new(),
            cycles: 0,
            log: Vec::new(),
            exit_code: None,
            frames: 0,
        };
        // Kernel thunk area: one 16-byte slot per imported function.
        m.thunk_base = m.alloc_virtual(0x10_0000);
        m.exit_thunk = m.add_thunk("kernel", 0, "ExitThreadReturn");
        m
    }

    // ------------------------------------------------------------------ memory

    /// Maps a fresh zeroed region at `addr` (page aligned).
    pub fn map_region(&mut self, addr: u32, size: u32) {
        let size = size.div_ceil(PAGE) * PAGE;
        let mut chunk = vec![0u8; size as usize].into_boxed_slice();
        // SAFETY: the chunk is kept alive in self.chunks and never moved.
        unsafe { self.cpu.map(addr, chunk.as_mut_ptr(), size as usize) };
        self.chunks.push(chunk);
        self.regions.push((addr, size));
    }

    pub fn is_mapped(&self, addr: u32) -> bool {
        self.cpu.is_mapped(addr)
    }

    /// Reserves and commits virtual memory (64 KB granularity).
    pub fn alloc_virtual(&mut self, size: u32) -> u32 {
        let size = size.max(1).div_ceil(0x1_0000) * 0x1_0000;
        let addr = self.virt_next;
        if addr as u64 + size as u64 > VIRT_END as u64 {
            return 0;
        }
        self.virt_next += size;
        self.map_region(addr, size);
        addr
    }

    /// Physical memory, returned through the mirror matching the requested page size.
    pub fn alloc_physical(&mut self, size: u32, align: u32, protect: u32) -> u32 {
        let align = align.max(PAGE).next_power_of_two();
        let base = (self.phys_next + align - 1) & !(align - 1);
        let size = size.max(1).div_ceil(PAGE) * PAGE;
        if base as usize + size as usize > PHYS_SIZE {
            return 0;
        }
        self.phys_next = base + size;
        let mirror = if protect & 0x8000_0000 != 0 {
            0xC000_0000
        } else if protect & 0x2000_0000 != 0 {
            0xA000_0000
        } else {
            0xE000_0000
        };
        mirror | base
    }

    /// Small kernel allocations (objects, strings): a bump allocator over virtual memory.
    pub fn alloc_pool(&mut self, size: u32) -> u32 {
        let size = (size.max(4) + 15) & !15;
        if self.pool_next + size > self.pool_end || self.pool_next == 0 {
            let chunk = size.max(0x10_0000);
            self.pool_next = self.alloc_virtual(chunk);
            self.pool_end = self.pool_next + chunk;
        }
        let a = self.pool_next;
        self.pool_next += size;
        a
    }

    pub fn r8(&self, a: u32) -> u8 {
        let p = self.cpu.host(a);
        if p.is_null() {
            0
        } else {
            unsafe { *p }
        }
    }
    pub fn w8(&mut self, a: u32, v: u8) {
        let p = self.cpu.host(a);
        if !p.is_null() {
            unsafe { *p = v }
        }
    }
    pub fn r16(&self, a: u32) -> u16 {
        (self.r8(a) as u16) << 8 | self.r8(a.wrapping_add(1)) as u16
    }
    pub fn r32(&self, a: u32) -> u32 {
        (self.r16(a) as u32) << 16 | self.r16(a.wrapping_add(2)) as u32
    }
    pub fn r64(&self, a: u32) -> u64 {
        (self.r32(a) as u64) << 32 | self.r32(a.wrapping_add(4)) as u64
    }
    pub fn w16(&mut self, a: u32, v: u16) {
        self.w8(a, (v >> 8) as u8);
        self.w8(a.wrapping_add(1), v as u8);
    }
    pub fn w32(&mut self, a: u32, v: u32) {
        self.w16(a, (v >> 16) as u16);
        self.w16(a.wrapping_add(2), v as u16);
    }
    pub fn w64(&mut self, a: u32, v: u64) {
        self.w32(a, (v >> 32) as u32);
        self.w32(a.wrapping_add(4), v as u32);
    }
    pub fn write_bytes(&mut self, a: u32, data: &[u8]) {
        for (i, &b) in data.iter().enumerate() {
            self.w8(a.wrapping_add(i as u32), b);
        }
    }
    pub fn read_bytes(&self, a: u32, n: usize) -> Vec<u8> {
        (0..n).map(|i| self.r8(a.wrapping_add(i as u32))).collect()
    }
    pub fn fill(&mut self, a: u32, n: u32, v: u8) {
        for i in 0..n {
            self.w8(a.wrapping_add(i), v);
        }
    }
    pub fn read_cstr(&self, a: u32) -> String {
        let mut s = Vec::new();
        let mut p = a;
        while s.len() < 4096 {
            let c = self.r8(p);
            if c == 0 {
                break;
            }
            s.push(c);
            p += 1;
        }
        String::from_utf8_lossy(&s).into_owned()
    }
    /// A pooled NUL-terminated copy of `s`.
    pub fn alloc_cstr(&mut self, s: &str) -> u32 {
        let a = self.alloc_pool(s.len() as u32 + 1);
        self.write_bytes(a, s.as_bytes());
        self.w8(a + s.len() as u32, 0);
        a
    }

    // ------------------------------------------------------------------ time

    /// Guest time in 100 ns ticks since boot.
    pub fn now(&self) -> u64 {
        self.cycles / CYCLES_PER_TICK
    }

    /// FILETIME-style system time (100 ns since 1601), starting at 2010-01-01.
    pub fn system_time(&self) -> u64 {
        129_067_776_000_000_000 + self.now()
    }

    // ------------------------------------------------------------------ handles

    pub fn new_handle(&mut self, object: u32) -> u32 {
        let h = self.next_handle;
        self.next_handle += 4;
        self.handles.insert(h, object);
        h
    }

    /// Object pointer for a handle (or the object pointer itself for pseudo handles).
    pub fn object(&self, handle: u32) -> Option<u32> {
        match handle {
            0xFFFF_FFFE => Some(self.threads[self.current].kthread),
            _ => self.handles.get(&handle).copied(),
        }
    }

    // ------------------------------------------------------------------ thunks

    pub fn add_thunk(&mut self, module: &'static str, ordinal: u16, name: &'static str) -> u32 {
        let idx = self.thunks.len() as u32;
        self.thunks.push(Thunk { module, ordinal, name });
        let a = self.thunk_base + idx * 16;
        self.w32(a, 0x3800_0000 | (idx & 0xFFFF)); // li r0, idx
        self.w32(a + 4, 0x4400_0002); // sc
        self.w32(a + 8, 0x4E80_0020); // blr
        self.w32(a + 12, 0x6000_0000); // nop
        a
    }

    /// Writes a thunk for an import in the title's own image at `slot`.
    fn write_import_thunk(&mut self, slot: u32, module: &'static str, ordinal: u16, name: &'static str) {
        let idx = self.thunks.len() as u32;
        self.thunks.push(Thunk { module, ordinal, name });
        self.w32(slot, 0x3800_0000 | (idx & 0xFFFF));
        self.w32(slot + 4, 0x4400_0002);
        self.w32(slot + 8, 0x4E80_0020);
        self.w32(slot + 12, 0x6000_0000);
    }

    // ------------------------------------------------------------------ loading

    /// Loads a title's XEX, links its imports to our kernel and creates the main thread.
    pub fn load_xex(&mut self, data: &[u8]) -> Result<(), String> {
        let xex = Xex::parse(data)?;
        let image = xex.image(data)?;
        let base = xex.load_address;
        self.map_region(base, image.len() as u32);
        self.write_bytes(base, &image);

        // Keep the XEX header in guest memory for RtlImageXexHeaderField and friends.
        let hdr_len = (xex.headers_len() as usize).min(data.len());
        self.xex_header = self.alloc_pool(hdr_len as u32);
        self.write_bytes(self.xex_header, &data[..hdr_len]);

        self.ldr_entry = self.alloc_pool(0x80);
        let ldr = self.ldr_entry;
        self.w32(ldr + 0x18, base);
        self.w32(ldr + 0x1C, base);
        self.w32(ldr + 0x20, xex.image_size);
        self.w32(ldr + 0x3C, xex.entry_point);
        self.w32(ldr + 0x58, self.xex_header);

        for lib in &xex.imports {
            let module: &'static str = match lib.name.to_ascii_lowercase().as_str() {
                "xboxkrnl.exe" => "xboxkrnl.exe",
                "xam.xex" => "xam.xex",
                other => {
                    self.log.push(format!("import library {other} isn't provided; its calls will fail"));
                    "unknown"
                }
            };
            for &rec in &lib.records {
                let value = self.r32(rec);
                let ty = value >> 24;
                let ordinal = value as u16;
                let (name, is_var) = crate::exports::lookup(module, ordinal).unwrap_or(("?", false));
                match ty {
                    0 if is_var => {
                        let v = self.variable(module, ordinal, name);
                        self.w32(rec, v);
                    }
                    0 => {}
                    1 => self.write_import_thunk(rec, module, ordinal, name),
                    _ => self.log.push(format!("unknown import record type {ty} at {rec:#x}")),
                }
            }
        }
        if let Some(t) = xex.header_data(data, crate::xex::key::TLS_INFO)? {
            let slots = u32::from_be_bytes([t[0], t[1], t[2], t[3]]);
            let raw_addr = u32::from_be_bytes([t[4], t[5], t[6], t[7]]);
            let data_size = u32::from_be_bytes([t[8], t[9], t[10], t[11]]);
            self.static_tls = Some((raw_addr, data_size.max(slots * 4), u32::from_be_bytes([t[12], t[13], t[14], t[15]])));
        }
        let entry = xex.entry_point;
        let stack = xex.stack_size.max(0x4_0000);
        self.xex = Some(xex);
        self.create_thread(entry, 0, 0, stack, false);
        Ok(())
    }

    /// A raw program for tests: code at `base`, entry = `base`, with `imports` (name list)
    /// written as thunks starting at `thunk_slot`.
    pub fn load_raw(&mut self, base: u32, code: &[u8], imports: &[(&'static str, &'static str)], thunk_slot: u32) -> Result<(), String> {
        self.map_region(base, (code.len() as u32 + 0xFFFF) & !0xFFFF);
        self.write_bytes(base, code);
        for (i, (module, name)) in imports.iter().enumerate() {
            let table = if *module == "xam.xex" { crate::exports::XAM } else { crate::exports::XBOXKRNL };
            let e = table.iter().find(|e| e.1 == *name).ok_or_else(|| format!("no export {name}"))?;
            self.write_import_thunk(thunk_slot + i as u32 * 16, module, e.0, e.1);
        }
        self.create_thread(base, 0, 0, 0x4_0000, false);
        Ok(())
    }

    // ------------------------------------------------------------------ threads

    /// Creates a guest thread starting at `start` with r3 = `a0`, r4 = `a1`. Returns its index.
    pub fn create_thread(&mut self, start: u32, a0: u32, a1: u32, stack_size: u32, suspended: bool) -> usize {
        let stack_size = stack_size.max(0x1_0000).div_ceil(PAGE) * PAGE;
        let stack = self.alloc_virtual(stack_size);
        let kthread = self.alloc_pool(0xAB0);
        let pcr = self.alloc_pool(0x2D8);
        let tls_dynamic = self.alloc_pool(64 * 4);
        let id = self.next_thread_id;
        self.next_thread_id += 1;

        // Static TLS: a copy of the image's TLS template.
        let tls_static = match self.static_tls {
            Some((raw, size, raw_size)) => {
                let a = self.alloc_pool(size + 16);
                let init = self.read_bytes(raw, raw_size.min(size) as usize);
                self.write_bytes(a, &init);
                a
            }
            None => self.alloc_pool(0x100),
        };
        self.w32(pcr, tls_static);
        self.w32(pcr + 0x30, pcr);
        self.w32(pcr + 0x70, stack + stack_size);
        self.w32(pcr + 0x74, stack);
        self.w32(pcr + 0x100, kthread);
        self.w8(kthread, 6); // thread object
        self.w32(kthread + 0x5C, stack + stack_size);
        self.w32(kthread + 0x60, stack);
        self.w32(kthread + 0x68, tls_static);
        self.w32(kthread + 0x14C, id);
        self.w32(kthread + 0x150, start);

        let mut ctx = Context::default();
        let mut saved = Context::default();
        let have_current = !self.threads.is_empty();
        if have_current {
            self.cpu.save(&mut saved);
        }
        let mut r = self.cpu.regs();
        r.r = [0; 32];
        r.r[1] = (stack + stack_size - 0x100) as u64;
        r.r[3] = a0 as u64;
        r.r[4] = a1 as u64;
        r.r[13] = pcr as u64;
        r.lr = self.exit_thunk as u64;
        r.ctr = 0;
        r.cr = 0;
        r.xer = 0;
        r.pc = start;
        self.cpu.set_regs(r);
        self.cpu.save(&mut ctx);
        if have_current {
            self.cpu.load(&saved);
        }
        let handle = self.new_handle(kthread);
        self.threads.push(Thread {
            id,
            ctx,
            kthread,
            pcr,
            handle,
            state: if suspended { ThreadState::Suspended } else { ThreadState::Ready },
            wait: None,
            tls_dynamic,
            suspend: suspended as u32,
        });
        let i = self.threads.len() - 1;
        if !have_current {
            let c = std::mem::take(&mut self.threads[i].ctx);
            self.cpu.load(&c);
            self.threads[i].ctx = c;
            self.current = i;
        }
        i
    }

    pub fn exit_thread(&mut self, code: u32) {
        let t = &mut self.threads[self.current];
        t.state = ThreadState::Exited;
        let k = t.kthread;
        self.w32(k + 4, 1); // signal waiters
        self.w32(k + 0x140, code);
    }

    fn switch_to(&mut self, next: usize) {
        if next == self.current {
            return;
        }
        let mut ctx = std::mem::take(&mut self.threads[self.current].ctx);
        self.cpu.save(&mut ctx);
        self.threads[self.current].ctx = ctx;
        self.current = next;
        let ctx = std::mem::take(&mut self.threads[next].ctx);
        self.cpu.load(&ctx);
        self.threads[next].ctx = ctx;
    }

    /// Whether thread `i` can run now (its wait, if any, is satisfiable or timed out).
    fn runnable(&self, i: usize) -> bool {
        let t = &self.threads[i];
        match t.state {
            ThreadState::Ready => true,
            ThreadState::Waiting => {
                let w = t.wait.as_ref().unwrap();
                w.deadline.is_some_and(|d| self.now() >= d) || self.wait_satisfiable(w, t.kthread)
            }
            _ => false,
        }
    }

    /// Picks the next runnable thread after the current one; advances time when all are
    /// sleeping. Returns false on deadlock or when no threads remain.
    fn schedule(&mut self) -> bool {
        let n = self.threads.len();
        for step in 1..=n {
            let i = (self.current + step) % n;
            if self.runnable(i) {
                self.switch_to(i);
                return true;
            }
        }
        // Everyone waits: jump to the earliest timeout.
        let next = self.threads.iter().filter(|t| t.state == ThreadState::Waiting).filter_map(|t| t.wait.as_ref().and_then(|w| w.deadline)).min();
        match next {
            Some(d) => {
                let now = self.now();
                if d > now {
                    self.cycles += (d - now) * CYCLES_PER_TICK;
                }
                self.schedule()
            }
            None => false,
        }
    }

    // ------------------------------------------------------------------ running

    /// Runs guest code for about `budget` cycles.
    pub fn run(&mut self, budget: u64) -> Stop {
        if self.threads.is_empty() {
            return Stop::Error("Nothing is loaded.".into());
        }
        let end = self.cycles + budget;
        while self.cycles < end {
            if let Some(code) = self.exit_code {
                return Stop::Exited(code);
            }
            if !self.runnable(self.current) && !self.schedule() {
                return if self.threads.iter().all(|t| t.state == ThreadState::Exited) { Stop::Exited(0) } else { Stop::Deadlock };
            }
            // A waiting thread that was picked retries its wait call from the thunk.
            if self.threads[self.current].state == ThreadState::Waiting {
                self.threads[self.current].state = ThreadState::Ready;
            }
            self.cpu.set_timebase(self.cycles / 64);
            let (used, ev) = self.cpu.run(QUANTUM);
            self.cycles += used as u64;
            match ev {
                None => {
                    self.schedule();
                }
                Some(Event::Syscall) => {
                    let idx = self.cpu.regs().r[0] as usize;
                    if let Err(e) = self.syscall(idx) {
                        return Stop::Error(e);
                    }
                    if self.threads[self.current].state != ThreadState::Ready {
                        self.schedule();
                    }
                }
                Some(Event::Trap(op)) => {
                    // Debug-break traps: log and continue after them.
                    let mut r = self.cpu.regs();
                    self.log.push(format!("trap {op:#010x} at {:#010x}", r.pc));
                    r.pc += 4;
                    self.cpu.set_regs(r);
                }
                Some(Event::Invalid(op)) => {
                    let pc = self.cpu.regs().pc;
                    return Stop::Error(format!("Unimplemented CPU instruction {op:#010x} (primary opcode {}) at {pc:#010x}", op >> 26));
                }
            }
        }
        Stop::Budget
    }

    /// Makes the current thread block in the call it's making: it re-executes the import
    /// thunk once rescheduled, re-evaluating the wait.
    pub(crate) fn block(&mut self, wait: Wait) {
        let mut r = self.cpu.regs();
        r.pc -= 8; // back to `li r0, idx` of the thunk
        self.cpu.set_regs(r);
        let t = &mut self.threads[self.current];
        t.state = ThreadState::Waiting;
        t.wait = Some(wait);
    }

    pub fn current_thread(&self) -> &Thread {
        &self.threads[self.current]
    }

    pub fn threads(&self) -> &[Thread] {
        &self.threads
    }

    /// Calls whose names we don't implement yet, with how often they were hit.
    pub fn unimplemented_calls(&self) -> Vec<(&'static str, u32)> {
        let mut v: Vec<_> = self.unimplemented.iter().map(|(k, v)| (*k, *v)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    }
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u32 = 0x8200_0000;

    fn d(op: u32, rd: u32, ra: u32, imm: u32) -> u32 {
        op << 26 | rd << 21 | ra << 16 | (imm & 0xFFFF)
    }
    fn bl(from: u32, to: u32) -> u32 {
        18 << 26 | (to.wrapping_sub(from) & 0x03FF_FFFC) | 1
    }

    /// Main thread: TLS slot, spawn a child that writes 0x1234, wait for it, exit with the value.
    #[test]
    fn threads_waits_and_exit() {
        let data = BASE + 0x800;
        let child = BASE + 0x100;
        let slot = |i: u32| BASE + 0x400 + i * 16;
        let mut code = vec![0u32; 0x800 / 4];
        let prog = [
            d(15, 31, 0, data >> 16),
            d(24, 31, 31, data),
            bl(BASE + 0x08, slot(0)),
            d(36, 3, 31, 0),
            d(14, 3, 31, 4),
            d(14, 4, 0, 0),
            d(14, 5, 0, 0),
            d(14, 6, 0, 0),
            d(15, 7, 0, child >> 16),
            d(24, 7, 7, child),
            31 << 26 | 31 << 21 | 8 << 16 | 31 << 11 | 444 << 1, // mr r8, r31
            d(14, 9, 0, 0),
            bl(BASE + 0x30, slot(1)),
            d(32, 3, 31, 4),
            d(14, 4, 0, 0),
            d(14, 5, 0, 0),
            d(14, 6, 0, 0),
            bl(BASE + 0x44, slot(2)),
            d(32, 3, 31, 8),
            bl(BASE + 0x4C, slot(3)),
            18 << 26, // b .
        ];
        code[..prog.len()].copy_from_slice(&prog);
        code[0x40] = d(14, 4, 0, 0x1234);
        code[0x41] = d(36, 4, 3, 8);
        code[0x42] = 0x4E80_0020;
        let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_be_bytes()).collect();

        let mut m = Machine::new();
        let imports = [("xboxkrnl.exe", "KeTlsAlloc"), ("xboxkrnl.exe", "ExCreateThread"), ("xboxkrnl.exe", "NtWaitForSingleObjectEx"), ("xboxkrnl.exe", "ExTerminateTitleProcess")];
        m.load_raw(BASE, &bytes, &imports, BASE + 0x400).unwrap();
        let stop = m.run(10_000_000);
        assert_eq!(stop, Stop::Exited(0x1234), "log: {:?}", m.log);
        assert_eq!(m.r32(data), 0, "first TLS slot");
        assert_eq!(m.threads().len(), 2);
        assert_eq!(m.threads()[1].state, ThreadState::Exited);
    }
}
