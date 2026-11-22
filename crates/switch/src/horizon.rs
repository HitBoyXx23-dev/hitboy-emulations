//! A small Horizon OS: paged memory, loading an NSO, and the system calls (SVCs) a program
//! needs first: heap, debug output, thread sleep and exit. Services (graphics, files, input)
//! are reached through `SendSyncRequest` and are not here yet; such calls stop the run with a
//! message that names the call.

use crate::a64::{Cpu, Exit, Mem};
use crate::nso::Nso;
use std::collections::HashMap;

const PAGE: u64 = 0x1000;
pub const HEAP_BASE: u64 = 0x2000_0000;
pub const STACK_TOP: u64 = 0x1000_0000;
pub const STACK_SIZE: u64 = 0x10_0000;
pub const MAIN_BASE: u64 = 0x0800_0000;
pub const MAIN_THREAD_HANDLE: u64 = 0xD000;

#[derive(Default)]
pub struct Memory {
    pages: HashMap<u64, Box<[u8; PAGE as usize]>>,
}

impl Memory {
    pub fn map(&mut self, addr: u64, len: u64) {
        let mut p = addr & !(PAGE - 1);
        while p < addr + len {
            self.pages.entry(p / PAGE).or_insert_with(|| Box::new([0; PAGE as usize]));
            p += PAGE;
        }
    }

    pub fn unmap_from(&mut self, addr: u64, len: u64) {
        let (a, b) = (addr.div_ceil(PAGE), (addr + len).div_ceil(PAGE));
        for p in a..b {
            self.pages.remove(&p);
        }
    }

    pub fn write_bytes(&mut self, addr: u64, data: &[u8]) -> bool {
        data.iter().enumerate().all(|(k, &b)| self.write(addr + k as u64, 1, b as u64))
    }

    pub fn read_bytes(&mut self, addr: u64, len: usize) -> Option<Vec<u8>> {
        (0..len).map(|k| self.read(addr + k as u64, 1).map(|v| v as u8)).collect()
    }
}

impl Mem for Memory {
    fn read(&mut self, addr: u64, size: usize) -> Option<u64> {
        let mut v = 0u64;
        for k in 0..size {
            let a = addr.wrapping_add(k as u64);
            v |= (self.pages.get(&(a / PAGE))?[(a % PAGE) as usize] as u64) << (8 * k);
        }
        Some(v)
    }
    fn write(&mut self, addr: u64, size: usize, value: u64) -> bool {
        for k in 0..size {
            let a = addr.wrapping_add(k as u64);
            match self.pages.get_mut(&(a / PAGE)) {
                Some(p) => p[(a % PAGE) as usize] = (value >> (8 * k)) as u8,
                None => return false,
            }
        }
        true
    }
}

/// A guest thread scheduled cooperatively by Horizon's kernel.
pub struct ThreadState {
    pub id: u64,
    pub handle: u32,
    pub cpu: Cpu,
    pub priority: u64,
    pub ready: bool,
    pub exited: bool,
}

pub struct Process {
    pub cpu: Cpu,
    pub mem: Memory,
    pub heap_size: u64,
    pub debug_output: String,
    pub exit_code: Option<u64>,
    pub program_id: u64,
    /// Kernel objects the program holds handles to (service sessions).
    pub(crate) handles: HashMap<u32, crate::ipc::Service>,
    pub(crate) next_handle: u32,
    /// Cooperative thread table (index `current_thread` is live in `self.cpu`).
    pub(crate) threads: Vec<ThreadState>,
    pub(crate) current_thread: usize,
    pub(crate) next_thread_id: u64,
    /// Optional decrypted RomFS image exposed via `fsp-srv` `OpenDataStorageByCurrentProcess`.
    pub romfs: Option<Vec<u8>>,
    /// Live Switch Npad button bitmask (populated by the frontend player).
    pub pad_buttons: u64,
    /// Live 0RGB framebuffer scanout for desktop display.
    pub framebuffer: Vec<u32>,
    pub fb_size: (usize, usize),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    Exited(u64),
    /// Something the emulator doesn't do yet, described for the user.
    Unsupported(String),
    Fault(u64),
    /// Ran out of the instruction budget (the program is still going).
    Budget,
}

impl Process {
    /// Maps one executable (see `load_modules`).
    pub fn load(nso: &Nso) -> Result<Process, String> {
        Self::load_modules(std::slice::from_ref(nso))
    }

    /// Maps the modules one after another from `MAIN_BASE` (the first, normally `rtld`, is the
    /// entry point), a stack and a thread-local block, as the console's loader does.
    pub fn load_modules(mods: &[Nso]) -> Result<Process, String> {
        let first = mods.first().ok_or("There is no program to load.")?;
        let mut mem = Memory::default();
        let mut base = MAIN_BASE;
        for nso in mods {
            for (seg, name) in nso.segments.iter().zip(crate::nso::SEGMENT_NAMES) {
                let at = base + seg.mem_off as u64;
                mem.map(at, seg.data.len() as u64);
                if !mem.write_bytes(at, &seg.data) {
                    return Err(format!("Couldn't place the {name} segment."));
                }
            }
            let end = nso.segments.iter().map(|s| s.mem_off as u64 + s.data.len() as u64).max().unwrap_or(0);
            mem.map(base + end, nso.bss_size as u64);
            base = (base + end + nso.bss_size as u64 + 0xFFF) & !0xFFF;
        }
        mem.map(STACK_TOP - STACK_SIZE, STACK_SIZE);
        let tls = 0x0100_0000;
        mem.map(tls, 0x1000);
        let mut cpu = Cpu::new();
        cpu.pc = MAIN_BASE + first.segments[0].mem_off as u64;
        cpu.sp = STACK_TOP;
        cpu.tpidrro_el0 = tls;
        cpu.x[1] = MAIN_THREAD_HANDLE;
        let main_thread = ThreadState {
            id: 1,
            handle: MAIN_THREAD_HANDLE as u32,
            cpu: Cpu::new(),
            priority: 44,
            ready: true,
            exited: false,
        };
        Ok(Process {
            cpu,
            mem,
            heap_size: 0,
            debug_output: String::new(),
            exit_code: None,
            program_id: 0,
            handles: HashMap::new(),
            next_handle: 0xD001,
            threads: vec![main_thread],
            current_thread: 0,
            next_thread_id: 2,
            romfs: None,
            pad_buttons: 0,
            framebuffer: vec![0; 640 * 360],
            fb_size: (640, 360),
        })
    }

    /// Maps the shared frontend 12-bit controller mask to Switch `HidNpadButton` flags.
    pub fn set_buttons(&mut self, buttons: u16) {
        const MAP: [(u16, u64); 12] = [
            (1 << 0, 1 << 0),  // A
            (1 << 1, 1 << 1),  // B
            (1 << 2, 1 << 11), // Minus (Select)
            (1 << 3, 1 << 10), // Plus (Start)
            (1 << 4, 1 << 13), // D-Pad Up
            (1 << 5, 1 << 15), // D-Pad Down
            (1 << 6, 1 << 12), // D-Pad Left
            (1 << 7, 1 << 14), // D-Pad Right
            (1 << 8, 1 << 2),  // X
            (1 << 9, 1 << 3),  // Y
            (1 << 10, 1 << 6), // L
            (1 << 11, 1 << 7), // R
        ];
        let mut mask = 0u64;
        for (src, dst) in MAP {
            if buttons & src != 0 {
                mask |= dst;
            }
        }
        self.pad_buttons = mask;
    }

    /// Switches execution to the next ready thread (round-robin). Returns false if no ready thread remains.
    fn yield_thread(&mut self) -> bool {
        let n = self.threads.len();
        if n <= 1 {
            return !self.threads[0].exited && self.threads[0].ready;
        }
        for step in 1..=n {
            let idx = (self.current_thread + step) % n;
            if self.threads[idx].ready && !self.threads[idx].exited {
                if idx != self.current_thread {
                    std::mem::swap(&mut self.cpu, &mut self.threads[self.current_thread].cpu);
                    self.current_thread = idx;
                    std::mem::swap(&mut self.cpu, &mut self.threads[self.current_thread].cpu);
                }
                return true;
            }
        }
        false
    }

    /// Builds a process from an ExeFS: `rtld` first, then `main`, `subsdk0..`, `sdk`.
    pub fn boot(files: &[(String, Vec<u8>)]) -> Result<Process, String> {
        let get = |n: &str| files.iter().find(|(f, _)| f == n).map(|(_, b)| b);
        let mut order = vec!["rtld".to_string(), "main".to_string()];
        let mut subs: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).filter(|n| n.starts_with("subsdk")).collect();
        subs.sort();
        order.extend(subs.into_iter().map(String::from));
        order.push("sdk".to_string());
        let mut mods = Vec::new();
        for name in &order {
            match get(name) {
                Some(b) => mods.push(Nso::parse(b).map_err(|e| format!("{name}: {e}"))?),
                None if name == "rtld" || name == "main" => return Err(format!("The ExeFS has no {name}.")),
                None => {}
            }
        }
        Self::load_modules(&mods)
    }

    /// Runs up to `budget` instructions, servicing system calls.
    pub fn run(&mut self, budget: u64) -> Stop {
        for _ in 0..budget {
            match self.cpu.step(&mut self.mem) {
                Ok(()) => {}
                Err(Exit::Svc(n)) => {
                    if let Some(stop) = self.svc(n) {
                        return stop;
                    }
                }
                Err(Exit::Brk(n)) => return Stop::Unsupported(format!("The program hit a breakpoint (brk #{n}) at {:#x}.", self.cpu.pc - 4)),
                Err(Exit::Undefined(pc, w)) => return Stop::Unsupported(format!("Instruction {w:#010x} at {pc:#x} isn't implemented yet.")),
                Err(Exit::Fault(a)) => return Stop::Fault(a),
            }
        }
        Stop::Budget
    }

    /// The run of pages around `addr` that are all mapped, or all unmapped: (base, size, mapped).
    fn region(&mut self, addr: u64) -> (u64, u64, bool) {
        let page = addr / PAGE;
        let mapped = self.mem.pages.contains_key(&page);
        let (mut lo, mut hi) = (page, page + 1);
        while lo > 0 && lo + 0x10000 > page && self.mem.pages.contains_key(&(lo - 1)) == mapped {
            lo -= 1;
        }
        while hi < page + 0x10000 && self.mem.pages.contains_key(&hi) == mapped {
            hi += 1;
        }
        (lo * PAGE, (hi - lo) * PAGE, mapped)
    }

    pub(crate) fn svc(&mut self, n: u32) -> Option<Stop> {
        let x = self.cpu.x;
        match n {
            0x01 => {
                // SetHeapSize(size) -> (result, address)
                let size = x[1];
                if size % 0x20_0000 != 0 || size > 0x1_0000_0000 {
                    self.cpu.x[0] = 0xE601; // invalid size
                } else {
                    if size > self.heap_size {
                        self.mem.map(HEAP_BASE + self.heap_size, size - self.heap_size);
                    } else {
                        self.mem.unmap_from(HEAP_BASE + size, self.heap_size - size);
                    }
                    self.heap_size = size;
                    self.cpu.x[0] = 0;
                    self.cpu.x[1] = HEAP_BASE;
                }
            }
            0x02 | 0x03 => {
                // SetMemoryPermission(addr, size, perm) / SetMemoryAttribute(addr, size, mask, attr)
                self.cpu.x[0] = if x[0] & (PAGE - 1) == 0 && x[1] & (PAGE - 1) == 0 { 0 } else { 0xCC01 };
            }
            0x04 => {
                // MapMemory(dst, src, size): map pages at dst and copy contents from src.
                let (dst, src, size) = (x[0], x[1], x[2]);
                if dst & (PAGE - 1) != 0 || src & (PAGE - 1) != 0 || size & (PAGE - 1) != 0 || size == 0 {
                    self.cpu.x[0] = 0xCC01;
                } else {
                    self.mem.map(dst, size);
                    let pages = size / PAGE;
                    for i in 0..pages {
                        if let Some(data) = self.mem.read_bytes(src + i * PAGE, PAGE as usize) {
                            self.mem.write_bytes(dst + i * PAGE, &data);
                        }
                    }
                    self.cpu.x[0] = 0;
                }
            }
            0x05 => {
                // UnmapMemory(dst, src, size)
                let (dst, size) = (x[0], x[2]);
                if dst & (PAGE - 1) != 0 || size & (PAGE - 1) != 0 {
                    self.cpu.x[0] = 0xCC01;
                } else {
                    self.mem.unmap_from(dst, size);
                    self.cpu.x[0] = 0;
                }
            }
            0x06 => {
                // QueryMemory(info*, _, address) -> (result, page info)
                let (base, size, mapped) = self.region(x[2]);
                let (kind, perm) = if !mapped {
                    (0u32, 0u32)
                } else if base >= HEAP_BASE && base < HEAP_BASE + 0x1_0000_0000 {
                    (5, 3)
                } else if base + size > STACK_TOP - STACK_SIZE && base < STACK_TOP {
                    (0xC, 3)
                } else {
                    (3, 3)
                };
                let mut info = Vec::with_capacity(0x28);
                info.extend_from_slice(&base.to_le_bytes());
                info.extend_from_slice(&size.to_le_bytes());
                for v in [kind, 0, perm, 0, 0, 0] {
                    info.extend_from_slice(&v.to_le_bytes());
                }
                self.cpu.x[0] = if self.mem.write_bytes(x[0], &info[..0x28]) { 0 } else { 0xD401 };
                self.cpu.x[1] = 0;
            }
            0x07 => {
                self.exit_code = Some(x[0]);
                return Some(Stop::Exited(x[0]));
            }
            0x08 => {
                // CreateThread(out_handle_in_x1, entry=x1, arg=x2, stack_top=x3, prio=x4, core=x5)
                let (entry, arg, stack_top, prio) = (x[1], x[2], x[3], x[4]);
                let tid = self.next_thread_id;
                self.next_thread_id += 1;
                let handle = self.next_handle;
                self.next_handle += 1;
                let tls = 0x0100_0000u64 + tid * 0x1000;
                self.mem.map(tls, 0x1000);
                let mut tcpu = Cpu::new();
                tcpu.pc = entry;
                tcpu.sp = stack_top;
                tcpu.x[0] = arg;
                tcpu.tpidrro_el0 = tls;
                self.threads.push(ThreadState { id: tid, handle, cpu: tcpu, priority: prio, ready: false, exited: false });
                self.cpu.x[0] = 0;
                self.cpu.x[1] = handle as u64;
            }
            0x09 => {
                // StartThread(handle)
                let h = x[0] as u32;
                if let Some(t) = self.threads.iter_mut().find(|t| t.handle == h && !t.exited) {
                    t.ready = true;
                    self.cpu.x[0] = 0;
                } else {
                    self.cpu.x[0] = 0xE401;
                }
            }
            0x0A => {
                // ExitThread()
                self.threads[self.current_thread].exited = true;
                self.threads[self.current_thread].ready = false;
                if !self.yield_thread() {
                    self.exit_code = Some(0);
                    return Some(Stop::Exited(0));
                }
            }
            0x0B => {
                // SleepThread(ns): yield cooperatively to another ready thread.
                self.cpu.x[0] = 0;
                self.yield_thread();
            }
            // ArbitrateLock/Unlock, WaitProcessWideKeyAtomic, SignalProcessWideKey
            0x1A | 0x1B | 0x1C | 0x1D => self.cpu.x[0] = 0,
            0x0C => {
                // GetThreadPriority(handle) -> (result, priority)
                let h = x[1] as u32;
                let prio = self
                    .threads
                    .iter()
                    .find(|t| t.handle == h)
                    .map(|t| t.priority)
                    .unwrap_or(self.threads[self.current_thread].priority);
                self.cpu.x[0] = 0;
                self.cpu.x[1] = prio;
            }
            0x0D => {
                let h = x[0] as u32;
                if let Some(t) = self.threads.iter_mut().find(|t| t.handle == h) {
                    t.priority = x[1];
                }
                self.cpu.x[0] = 0;
            }
            0x0F => self.cpu.x[0] = 0, // SetThreadCoreMask
            0x0E => {
                // GetThreadCoreMask(handle) -> (result, ideal_core, affinity_mask)
                self.cpu.x[0] = 0;
                self.cpu.x[1] = 0;
                self.cpu.x[2] = 0xF;
            }
            0x10 => self.cpu.x[0] = 0, // GetCurrentProcessorNumber -> core 0
            0x13 => {
                // MapSharedMemory(handle, addr, size, perm)
                let (addr, size) = (x[1], x[2]);
                if addr & (PAGE - 1) != 0 || size & (PAGE - 1) != 0 || size == 0 {
                    self.cpu.x[0] = 0xCC01;
                } else {
                    self.mem.map(addr, size);
                    self.cpu.x[0] = 0;
                }
            }
            0x14 => {
                // UnmapSharedMemory(handle, addr, size)
                let (addr, size) = (x[1], x[2]);
                self.mem.unmap_from(addr, size);
                self.cpu.x[0] = 0;
            }
            0x15 => {
                // CreateTransferMemory(addr, size, perm) -> (result, handle)
                let h = self.next_handle;
                self.next_handle += 1;
                self.handles.insert(h, crate::ipc::Service::Unknown("TransferMemory".into()));
                self.cpu.x[0] = 0;
                self.cpu.x[1] = h as u64;
            }
            0x16 => self.close_handle(x[0] as u32),
            0x18 => {
                // WaitSynchronization(handles_ptr, count, timeout) -> (result, signaled_index)
                self.cpu.x[0] = 0;
                self.cpu.x[1] = 0;
            }
            0x21 => return self.send_sync_request(x[0] as u32),
            0x22 => {
                // GetThreadId(handle) -> (result, thread_id)
                let h = x[1] as u32;
                let tid = self
                    .threads
                    .iter()
                    .find(|t| t.handle == h)
                    .map(|t| t.id)
                    .unwrap_or(self.threads[self.current_thread].id);
                self.cpu.x[0] = 0;
                self.cpu.x[1] = tid;
            }
            0x1F => return self.connect_named_port(x[1]),
            0x26 => return Some(Stop::Unsupported(format!("The program called Break (reason {:#x}); it stopped itself on an error.", x[0]))),
            0x27 => {
                // OutputDebugString(ptr, len)
                if let Some(b) = self.mem.read_bytes(x[0], (x[1] as usize).min(0x10000)) {
                    self.debug_output.push_str(&String::from_utf8_lossy(&b));
                }
                self.cpu.x[0] = 0;
            }
            0x1E => self.cpu.x[0] = self.cpu.ticks, // GetSystemTick
            0x29 => {
                // GetInfo(type, handle, subtype) -> (result, value)
                let used = self.mem.pages.len() as u64 * PAGE;
                let v = match x[1] {
                    0 => Some(0xF),
                    1 => Some(!0u64),
                    2 => Some(0x4000_0000),
                    3 => Some(0x1000_0000),
                    4 => Some(HEAP_BASE),
                    5 => Some(0x1_0000_0000),
                    6 | 21 => Some(0xF000_0000),
                    7 | 22 => Some(used),
                    11 => Some(0x5357_4954_4348_0001u64.wrapping_add(x[2].wrapping_mul(0x9E37_79B9_7F4A_7C15))), // RandomEntropy
                    12 => Some(0),
                    13 => Some(0x7F_FF00_0000),
                    14 => Some(STACK_TOP - STACK_SIZE),
                    15 => Some(STACK_SIZE),
                    16 => Some(0xF000_0000), // TotalMemorySize
                    17 => Some(used),        // UsedMemorySize
                    18 => Some(self.program_id),
                    20 => Some(0), // UserExceptionContextAddress
                    23 => Some(1),
                    _ => None,
                };
                match v {
                    Some(v) => {
                        self.cpu.x[0] = 0;
                        self.cpu.x[1] = v;
                    }
                    None => self.cpu.x[0] = 0xF001,
                }
            }
            _ => return Some(Stop::Unsupported(format!("System call {n:#x} isn't implemented yet."))),
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nso::{Nso, Segment};

    fn program(words: &[u32], rodata: &[u8]) -> Process {
        let text: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let nso = Nso {
            segments: [Segment { mem_off: 0, data: text }, Segment { mem_off: 0x1000, data: rodata.to_vec() }, Segment { mem_off: 0x2000, data: vec![] }],
            bss_size: 0x1000,
            build_id: [0; 0x20],
        };
        Process::load(&nso).unwrap()
    }

    #[test]
    fn hello_world_prints_and_exits() {
        // x0 = MAIN_BASE+0x1000 (movz low, movk high), x1 = 5; svc 0x27; x0 = 7; svc 7
        let at = MAIN_BASE + 0x1000;
        let words = [
            0xD2800000 | ((at & 0xFFFF) as u32) << 5,
            0xF2A00000 | (((at >> 16) & 0xFFFF) as u32) << 5,
            0xD28000A1,
            0xD40004E1, // svc #0x27
            0xD28000E0,
            0xD40000E1, // svc #7
        ];
        let mut p = program(&words, b"hello");
        assert_eq!(p.run(100), Stop::Exited(7));
        assert_eq!(p.debug_output, "hello");
    }

    #[test]
    fn heap_unknown_calls_and_faults() {
        // x1 = 0x200000 (2 MB); svc 1; store to the returned heap address; exit(7)
        let words = [0xD2A00401, 0xD4000021, 0xD2800042, 0xF9000022, 0xD28000E0, 0xD40000E1];
        let mut p = program(&words, b"");
        assert_eq!(p.run(100), Stop::Exited(7));
        assert_eq!(p.cpu.x[1], HEAP_BASE);
        assert_eq!(p.mem.read(HEAP_BASE, 8), Some(2));
        let mut p = program(&[0xD4001E01], b""); // svc #0xF0
        assert!(matches!(p.run(10), Stop::Unsupported(m) if m.contains("0xf0")));
        let mut p = program(&[0xD2A00043, 0xF9400060], b"");
        assert_eq!(p.run(10), Stop::Fault(0x20000));
    }

    #[test]
    fn memory_mapping_and_thread_svcs_work() {
        let mut p = program(&[], b"");
        // SetHeapSize(2 MB)
        p.cpu.x[1] = 0x20_0000;
        assert!(p.svc(0x01).is_none());
        p.mem.write(HEAP_BASE, 8, 0xCAFE_BABE_1234_5678);

        // MapMemory(dst = HEAP_BASE + 0x40_0000, src = HEAP_BASE, size = 0x1000)
        p.cpu.x[0] = HEAP_BASE + 0x40_0000;
        p.cpu.x[1] = HEAP_BASE;
        p.cpu.x[2] = 0x1000;
        assert!(p.svc(0x04).is_none());
        assert_eq!(p.cpu.x[0], 0);
        assert_eq!(p.mem.read(HEAP_BASE + 0x40_0000, 8), Some(0xCAFE_BABE_1234_5678));

        // CreateTransferMemory -> returns non-zero handle
        p.cpu.x[0] = HEAP_BASE;
        p.cpu.x[1] = 0x1000;
        p.cpu.x[2] = 3;
        assert!(p.svc(0x15).is_none());
        assert_eq!(p.cpu.x[0], 0);
        assert!(p.cpu.x[1] >= 0xD001);

        // GetThreadPriority / GetThreadId / GetCurrentProcessorNumber / GetInfo(RandomEntropy)
        assert!(p.svc(0x0C).is_none());
        assert_eq!((p.cpu.x[0], p.cpu.x[1]), (0, 44));
        assert!(p.svc(0x22).is_none());
        assert_eq!((p.cpu.x[0], p.cpu.x[1]), (0, 1));
        assert!(p.svc(0x10).is_none());
        assert_eq!(p.cpu.x[0], 0);
        p.cpu.x[1] = 11; // RandomEntropy
        p.cpu.x[2] = 0;
        assert!(p.svc(0x29).is_none());
        assert_eq!(p.cpu.x[0], 0);
        assert_ne!(p.cpu.x[1], 0);
    }

    #[test]
    fn cooperative_threads_create_start_yield_and_exit() {
        // Worker at MAIN_BASE + 0x20: stores x0 (arg = 0x77) to [HEAP_BASE], then svc #0x0A (ExitThread)
        // Main at MAIN_BASE: svc #0x0B (SleepThread -> yields to worker), then exit(0)
        let mut words = vec![0xD503201Fu32; 12]; // NOPs
        // Main (offset 0):
        words[0] = 0xD4000161; // svc #0x0B (SleepThread -> yield to worker)
        words[1] = 0xD2800000; // movz x0, #0
        words[2] = 0xD40000E1; // svc #0x07 (ExitProcess)
        // Worker (offset 0x20 = word 8):
        // x1 = HEAP_BASE (0x2000_0000 = movz x1, #0x2000, lsl #16)
        words[8] = 0xD2A40001; // movz x1, #0x2000, lsl #16
        words[9] = 0xF9000020; // str x0, [x1]
        words[10] = 0xD4000141; // svc #0x0A (ExitThread)

        let mut p = program(&words, b"");
        // Map heap so worker can write to HEAP_BASE
        p.cpu.x[1] = 0x20_0000;
        assert!(p.svc(0x01).is_none());

        // CreateThread(entry = MAIN_BASE + 0x20, arg = 0x77, stack = STACK_TOP - 0x4000, prio = 40)
        p.cpu.x[1] = MAIN_BASE + 0x20;
        p.cpu.x[2] = 0x77;
        p.cpu.x[3] = STACK_TOP - 0x4000;
        p.cpu.x[4] = 40;
        assert!(p.svc(0x08).is_none());
        assert_eq!(p.cpu.x[0], 0);
        let worker_h = p.cpu.x[1];

        // StartThread(worker_h)
        p.cpu.x[0] = worker_h;
        assert!(p.svc(0x09).is_none());
        assert_eq!(p.cpu.x[0], 0);

        // Run: main yields -> worker writes 0x77 and exits -> main resumes and exits(0)
        assert_eq!(p.run(50), Stop::Exited(0));
        assert_eq!(p.mem.read(HEAP_BASE, 8), Some(0x77));
    }
}
