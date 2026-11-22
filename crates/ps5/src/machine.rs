//! Running a decrypted PlayStation program on our own 64-bit x86 CPU core.
//!
//! The program is loaded the way the system's loader would: segments at their addresses,
//! relocations applied, and every imported system function pointed at a tiny stub
//! (`mov eax, N / trap / ret`). When the CPU executes the trap, the call is handled in Rust
//! (our own version of the system library) using the System V calling convention the program
//! was built for: arguments in rdi, rsi, rdx, rcx, r8, r9; the result in rax.
//!
//! Because the CPU is ours, this works the same on Windows, macOS, Linux, iOS and Android.

use crate::elf::Elf;
use cpui686::{Bus, Cpu, EFER_LME, EFER_LMA, EFER_NXE, EFER_SCE, FS};
use std::collections::HashMap;

mod threads;
mod io;
mod queues;
mod linker;
mod runtime;
mod launch;
mod pools;
mod random;
mod ram;
mod graphics;

#[cfg(all(windows, target_pointer_width = "64"))]
const RAM_SIZE: usize = 8 << 30;
#[cfg(not(all(windows, target_pointer_width = "64")))]
const RAM_SIZE: usize = 1 << 30;
const PML4: u64 = 0x1000;
const STUB_BASE: u64 = 0x7000_0000;
const STACK_TOP: u64 = 0x7_0000_0000;
const STACK_SIZE: u64 = 1 << 20;
const HEAP_BASE: u64 = 0x2_0000_0000;
const TLS_BASE: u64 = 0x6_0000_0000;
const ARG_REGS: [usize; 6] = [7, 6, 2, 1, 8, 9];

// ------------------------------------------------------------------ memory

/// Physical RAM plus the page tables the CPU walks.
pub struct Mem {
    ram: ram::Ram,
    next_frame: u64,
    /// Guest page number -> physical frame, so the host can read and write guest memory.
    pages: HashMap<u64, u64>,
}

impl Bus for Mem {
    fn read8(&mut self, a: u64) -> u8 {
        let Ok(a) = usize::try_from(a) else { return 0xFF };

        *self.ram.get(a as usize).unwrap_or(&0xFF)
    }
    fn write8(&mut self, a: u64, v: u8) {
        let Ok(a) = usize::try_from(a) else { return };

        if let Some(b) = self.ram.get_mut(a as usize) {
            *b = v;
        }
    }
    fn ram_index(&self, a: u64) -> Option<usize> {
        let Ok(a) = usize::try_from(a) else { return None };

        ((a as usize) < self.ram.len()).then_some(a as usize)
    }
    fn ram(&mut self) -> &mut [u8] {
        &mut self.ram
    }
}

impl Mem {
    fn new() -> Result<Mem, String> {
        Ok(Mem { ram: ram::Ram::new(2 << 20, RAM_SIZE)?, next_frame: 0x2000, pages: HashMap::new() })
    }

    fn frame(&mut self) -> Result<u64, String> {
        let f = self.next_frame;
        let required = f as usize + 4096;
        if required > RAM_SIZE {
            return Err("The program asked for more memory than this machine has.".into());
        }
        if required > self.ram.len() {
            let length = required.checked_add((16 << 20) - 1).ok_or("Guest RAM size overflows.")? & !((16 << 20) - 1);
            self.ram.grow(length.min(RAM_SIZE))?;
        }
        self.next_frame += 4096;
        Ok(f)
    }

    fn rd64(&self, phys: u64) -> u64 {
        u64::from_le_bytes(self.ram[phys as usize..phys as usize + 8].try_into().unwrap())
    }

    fn wr64(&mut self, phys: u64, v: u64) {
        self.ram[phys as usize..phys as usize + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Makes `[va, va+len)` usable, allocating frames and page-table entries as needed.
    pub fn map(&mut self, va: u64, len: u64, writable: bool, exec: bool) -> Result<(), String> {
        self.map_backed(va, len, writable, exec, None)
    }

    fn map_backed(&mut self, va: u64, len: u64, writable: bool, exec: bool, mut backing: Option<(&mut HashMap<u64, u64>, u64)>) -> Result<(), String> {
        if len > RAM_SIZE as u64 { return Err("A memory mapping exceeds the machine's RAM limit.".into()); }
        let end = va.checked_add(len.max(1) - 1).ok_or("The memory mapping address overflows.")?;
        let first = va >> 12;
        let last = end >> 12;
        for page in first..=last {
            let pva = page << 12;
            let mut table = PML4;
            for level in (1..4).rev() {
                let idx = (pva >> (12 + 9 * level)) & 511;
                let at = table + idx * 8;
                let e = self.rd64(at);
                if e & 1 == 0 {
                    let f = self.frame()?;
                    self.wr64(at, f as u64 | 7);
                    table = f;
                } else {
                    table = e & 0x000F_FFFF_FFFF_F000;
                }
            }
            let at = table + ((pva >> 12) & 511) * 8;
            let old = self.rd64(at);
            let frame = if let Some((frames, offset)) = backing.as_mut() {
                let physical_page = (*offset >> 12) + page - first;
                if let Some(frame) = frames.get(&physical_page) { *frame } else {
                    let frame = self.frame()?;
                    frames.insert(physical_page, frame);
                    frame
                }
            } else if old & 1 != 0 {
                old & 0x000F_FFFF_FFFF_F000
            } else {
                self.frame()?
            };
            let mut flags = 0x5u64; // present, user
            let was_w = old & 1 != 0 && old & 2 != 0;
            let was_x = old & 1 != 0 && old >> 63 == 0;
            if writable || was_w {
                flags |= 2;
            }
            let x = exec || was_x;
            self.wr64(at, frame as u64 | flags | if x { 0 } else { 1 << 63 });
            self.pages.insert(page, frame);
        }
        Ok(())
    }

    pub fn unmap(&mut self, address: u64, length: u64) -> bool {
        if address & 0xFFF != 0 || length == 0 || length > RAM_SIZE as u64 {
            return false;
        }
        let Some(end) = address.checked_add(length - 1) else { return false };
        for page in (address >> 12)..=(end >> 12) {
            if self.pages.remove(&page).is_none() { continue; }
            let mut table = PML4;
            for level in (1..4).rev() {
                let index = (page >> (9 * level)) & 511;
                table = self.rd64(table + index * 8) & 0x000F_FFFF_FFFF_F000;
            }
            self.wr64(table + (page & 511) * 8, 0);
        }
        true
    }

    fn phys(&self, va: u64) -> Option<usize> {
        self.pages.get(&(va >> 12)).map(|f| *f as usize + (va & 0xFFF) as usize)
    }

    pub fn read(&self, va: u64, buf: &mut [u8]) -> bool {
        for (i, b) in buf.iter_mut().enumerate() {
            match va.checked_add(i as u64).and_then(|address| self.phys(address)) {
                Some(p) => *b = self.ram[p],
                None => return false,
            }
        }
        true
    }

    pub fn write(&mut self, va: u64, data: &[u8]) -> bool {
        for (i, b) in data.iter().enumerate() {
            match va.checked_add(i as u64).and_then(|address| self.phys(address)) {
                Some(p) => self.ram[p] = *b,
                None => return false,
            }
        }
        true
    }

    pub fn read_u64(&self, va: u64) -> u64 {
        let mut b = [0u8; 8];
        self.read(va, &mut b);
        u64::from_le_bytes(b)
    }

    pub fn write_u64(&mut self, va: u64, v: u64) -> bool {
        self.write(va, &v.to_le_bytes())
    }

    /// A NUL-terminated string (capped).
    pub fn read_cstr(&self, va: u64) -> String {
        let mut out = Vec::new();
        for i in 0..4096u64 {
            let mut b = [0u8; 1];
            if !self.read(va + i, &mut b) || b[0] == 0 {
                break;
            }
            out.push(b[0]);
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

// ------------------------------------------------------------------ the machine

#[derive(Debug, PartialEq, Eq)]
pub enum Stop {
    /// The program finished (returned from its entry point or called exit).
    Exited(i32),
    /// The step limit was reached with the program still running.
    Running,
}

/// The screen as the game sees it: registered frame buffers and which one was last flipped.
#[derive(Default, Debug)]
pub struct VideoOut {
    pub buffers: Vec<u64>,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub pixel_format: u32,
    pub current: Option<usize>,
    pub flips: u64,
    pub last_flip_arg: u64,
    pub last_flip_time: u64,
}

pub struct Machine {
    pub cpu: Cpu,
    pub mem: Mem,
    /// Name (or hash, when unknown) of each imported function, by stub number.
    imports: Vec<String>,
    /// Text the program printed.
    pub output: String,
    /// System functions the program called that we don't provide yet (each listed once).
    pub unimplemented: Vec<String>,
    exit_code: Option<i32>,
    process_param: u64,
    runtime_error: Option<String>,
    heap_next: u64,
    heap_mapped: u64,
    sizes: HashMap<u64, u64>,
    pub base: u64,
    pub entry: u64,
    pub calls: u64,
    pub last_call: Option<(String, [u64; 6])>,
    pub pad_buttons: u32,
    /// What the game has put on screen through VideoOut.
    pub video: VideoOut,
    rng: u64,
    direct_next: u64,
    sched: threads::Sched,
    thread_keys: HashMap<u32, u64>,
    thread_values: HashMap<(u64, u32), u64>,
    io: io::Io,
    tls_offsets: HashMap<u64, u64>,
    tls_size: u64,
    queues: queues::Queues,
    pools: pools::Pools,
    runtime: runtime::Runtime,
    launch: launch::LaunchState,
    graphics: graphics::Graphics,
}

fn reloc_sym_addr(sym: u32, base: u64, elf: &Elf, stubs: &[Option<u64>]) -> u64 {
    match elf.symbols.get(sym as usize) {
        Some(s) if s.defined => base + s.value,
        Some(_) => stubs.get(sym as usize).copied().flatten().unwrap_or(0),
        None => 0,
    }
}

impl Machine {
    /// Loads a decrypted program image and prepares to run it from its entry point.
    pub fn load(file: &[u8]) -> Result<Machine, String> {
        Self::load_with_modules(file, &[])
    }

    pub fn load_with_modules(file: &[u8], modules: &[Vec<u8>]) -> Result<Machine, String> {
        let image = crate::image::program_image(file)?;
        let file = image.as_ref();
        let elf = Elf::parse(file)?;
        let mut mem = Mem::new()?;
        // Root of the page tables.
        mem.wr64(PML4, 0);

        let min_va = elf.segments.iter().filter(|s| s.is_loadable()).map(|s| s.vaddr).min().ok_or("The program has nothing to load.")?;
        // Programs linked at address 0 are placed at the usual base.
        let base = if min_va < 0x40_0000 { 0x40_0000 - min_va } else { 0 };

        for s in elf.segments.iter().filter(|s| s.is_loadable()) {
            mem.map(base + s.vaddr, s.mem_size, s.flags & 2 != 0, s.flags & 1 != 0)?;
            let (off, len) = (s.offset as usize, s.file_size as usize);
            let data = file.get(off..off + len).ok_or("A segment runs past the end of the file.")?;
            mem.write(base + s.vaddr, data);
        }

        let mut modules = linker::map_modules(&mut mem, modules)?;
        let exports = linker::exports(&elf, base, &modules);
        let mut globals = HashMap::new();
        let mut imports = Vec::new();
        let stubs = linker::bind(&mut mem, &elf, &exports, &mut imports, &mut globals)?;
        for module in &mut modules {
            module.bindings = linker::bind(&mut mem, &module.elf, &exports, &mut imports, &mut globals)?;
        }
        // Stub used as the program's return address and for exit().
        let exit_idx = imports.len() as u64;
        imports.push("__return_from_entry".into());
        let thread_exit_idx = imports.len() as u64;
        imports.push("__thread_exit".into());
        let stub_bytes = (imports.len() as u64 + 1) * 16;
        mem.map(STUB_BASE, stub_bytes, false, true)?;
        for i in 0..imports.len() as u64 {
            let mut code = [0xCCu8; 16];
            let offset = if i == exit_idx || i == thread_exit_idx {
                code[..3].copy_from_slice(&[0x49, 0x89, 0xC2]);
                3
            } else {
                0
            };
            code[offset] = 0xB8;
            code[offset + 1..offset + 5].copy_from_slice(&(i as u32).to_le_bytes());
            code[offset + 5..offset + 8].copy_from_slice(&[0x0F, 0xFF, 0x01]);
            code[offset + 8] = 0xC3;
            mem.write(STUB_BASE + i * 16, &code);
        }

        let (tls_image, tls_size, tls_offsets) = linker::tls(&elf, file, &modules)?;
        let tls_symbols = linker::tls_symbols(&elf, &modules);
        linker::relocate(&mut mem, &elf, base, &stubs, 1, tls_size, &tls_offsets, &tls_symbols)?;
        for (index, module) in modules.iter().enumerate() {
            linker::relocate(&mut mem, &module.elf, module.base, &module.bindings, index as u64 + 2, tls_size, &tls_offsets, &tls_symbols)?;
        }
        mem.map(TLS_BASE, tls_size + 0x1000, true, false)?;
        mem.write(TLS_BASE, &tls_image);
        let fs_base = TLS_BASE + tls_size;
        mem.write_u64(fs_base, fs_base);
        mem.write_u64(fs_base + 0x28, 0x5A5A_5A5A_1234_5678); // stack protector canary

        // Stack, with the start-up arguments the program's entry point expects.
        mem.map(STACK_TOP - STACK_SIZE, STACK_SIZE, true, false)?;
        let name_at = STACK_TOP - 0x200;
        mem.write(name_at, b"eboot.bin\0");
        let args = STACK_TOP - 0x100;
        mem.write_u64(args, 1); // argc
        mem.write_u64(args + 8, name_at); // argv[0]
        mem.write_u64(args + 16, 0); // argv terminator
        mem.write_u64(args + 24, 0); // envp terminator
        mem.write_u64(args + 32, 0); // auxv terminator
        let rsp = STACK_TOP - 0x400 - 8;
        mem.write_u64(rsp, STUB_BASE + exit_idx * 16); // return address of the entry point

        let entry = base + elf.entry;
        let mut cpu = Cpu::new_x64();
        cpu.cr0 = 0x8000_0033;
        cpu.cr3 = PML4 as u64;
        cpu.cr4 = (1 << 5) | (1 << 9) | (1 << 10); // PAE, SSE
        cpu.efer = EFER_LME | EFER_LMA | EFER_NXE | EFER_SCE;
        cpu.code64 = true;
        cpu.cs32 = false;
        cpu.ss32 = true;
        cpu.cpl = 3;
        cpu.sel[1] = 0x33;
        cpu.sel[2] = 0x2B;
        cpu.base = [0; 6];
        cpu.base[FS] = fs_base;
        cpu.eflags = 0x202;
        cpu.rip = linker::bootstrap(&mut mem, &modules, entry, args, STUB_BASE + exit_idx * 16)?;
        cpu.r[4] = rsp;
        cpu.r[7] = args; // rdi: the argument block
        cpu.r[6] = STUB_BASE + exit_idx * 16; // rsi: the exit function
        cpu.flush_tlb();

        Ok(Machine {
            cpu,
            mem,
            imports,
            output: String::new(),
            unimplemented: Vec::new(),
            exit_code: None,
            process_param: elf.segments.iter().find(|segment| segment.kind == "sce procparam").map_or(0, |segment| base + segment.vaddr),
            runtime_error: None,
            heap_next: HEAP_BASE,
            heap_mapped: HEAP_BASE,
            sizes: HashMap::new(),
            base,
            entry,
            calls: 0,
            last_call: None,
            pad_buttons: 0,
            video: VideoOut::default(),
            rng: 1,
            direct_next: 0,
            sched: threads::Sched::new(tls_image, tls_size, STUB_BASE + thread_exit_idx * 16),
            thread_keys: HashMap::new(),
            thread_values: HashMap::new(),
            io: io::Io::default(),
            tls_offsets,
            tls_size,
            queues: queues::Queues::default(),
            pools: pools::Pools::default(),
            runtime: runtime::Runtime::default(),
            launch: launch::LaunchState::default(),
            graphics: graphics::Graphics::default(),
        })
    }

    fn arg(&self, n: usize) -> u64 {
        self.cpu.r[ARG_REGS[n]]
    }

    fn ret(&mut self, v: u64) {
        if v & 0xFFFF0000 == 0x80020000 && self.output.len() < 16384 {
            if let Some((name, arguments)) = &self.last_call {
                if matches!(name.as_str(), "sceKernelReserveVirtualRange" | "sceKernelBatchMap" | "sceKernelMemoryPoolReserve" | "sceKernelMemoryPoolCommit" | "sceKernelMemoryPoolDecommit" | "sceKernelMemoryPoolExpand" | "sceKernelMapDirectMemory" | "sceKernelMapNamedDirectMemory" | "sceKernelMapFlexibleMemory") {
                    self.output.push_str(&format!("Memory service failed: {name} result={v:#x} arguments={arguments:x?}\n"));
                }
            }
        }
        self.cpu.r[0] = v;
    }

    pub fn memory_service_report(&self) -> String {
        format!("physical_used={:#x} committed_backing={:#x} ram_limit={RAM_SIZE:#x} virtual_pages={} heap_end={:#x} direct_end={:#x} pool_capacity={:#x} pool_committed={} direct_mappings={}", self.mem.next_frame, self.mem.ram.len(), self.mem.pages.len(), self.heap_next, self.direct_next, self.pools.capacity, self.pools.committed.len(), self.runtime.direct_mappings.len())
    }

    /// Argument `n` (0-based), including the ones System V passes on the stack.
    fn arg_ext(&self, n: usize) -> u64 {
        if n < 6 {
            self.arg(n)
        } else {
            self.mem.read_u64(self.cpu.r[4] + 8 * (n as u64 - 5))
        }
    }

    fn put32(&mut self, at: u64, v: u32) {
        self.mem.write(at, &v.to_le_bytes());
    }

    /// The last flipped frame as (width, height, RGBA bytes), if the game has shown one.
    pub fn framebuffer(&self) -> Option<(u32, u32, Vec<u8>)> {
        let v = &self.video;
        let addr = *v.buffers.get(v.current?)?;
        if v.width == 0 || v.height == 0 || v.width > 8192 || v.height > 8192 {
            return None;
        }
        let pitch = v.pitch.max(v.width) as u64;
        let mut out = Vec::with_capacity((v.width * v.height * 4) as usize);
        let mut row = vec![0u8; v.width as usize * 4];
        for y in 0..v.height as u64 {
            if !self.mem.read(addr + y * pitch * 4, &mut row) {
                return None;
            }
            for px in row.chunks_exact(4) {
                // 0x80000000 is B8G8R8A8; everything else we treat as R8G8B8A8.
                if v.pixel_format == 0x8000_0000 {
                    out.extend_from_slice(&[px[2], px[1], px[0], 255]);
                } else {
                    out.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
        }
        Some((v.width, v.height, out))
    }

    // ------------------------------------------------------------ memory services

    fn heap_alloc(&mut self, size: u64, align: u64) -> u64 {
        if size > RAM_SIZE as u64 { return 0; }
        let Some(align) = align.max(16).checked_next_power_of_two() else { return 0 };
        let Some(at) = self.heap_next.checked_add(align - 1).map(|address| address & !(align - 1)) else { return 0 };
        let Some(end) = at.checked_add(size.max(1)) else { return 0 };
        if end > self.heap_mapped {
            let from = self.heap_mapped;
            let need = ((end - from) + 0xFFFF) & !0xFFFF;
            if self.mem.map(from, need, true, false).is_err() {
                return 0;
            }
            self.heap_mapped = from + need;
        }
        self.heap_next = end;
        self.sizes.insert(at, size);
        at
    }

    fn copy(&mut self, dst: u64, src: u64, n: u64) {
        let mut buf = vec![0u8; n.min(1 << 24) as usize];
        if self.mem.read(src, &mut buf) {
            self.mem.write(dst, &buf);
        }
    }

    // ------------------------------------------------------------ system functions

    /// Formats a printf-style string; integer and string arguments start at `first` (the
    /// index of the first variadic register argument).
    fn format(&self, fmt: &str, first: usize) -> String {
        let mut out = String::new();
        let mut arg = first;
        let mut chars = fmt.chars().peekable();
        let next = |arg: &mut usize| -> u64 {
            let v = if *arg < 6 { self.cpu.r[ARG_REGS[*arg]] } else { self.mem.read_u64(self.cpu.r[4] + 8 + (*arg as u64 - 6) * 8) };
            *arg += 1;
            v
        };
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            let mut zero = false;
            let mut width = 0usize;
            let mut long = 0;
            while let Some(&d) = chars.peek() {
                if d == '0' && width == 0 {
                    zero = true;
                    chars.next();
                } else if d.is_ascii_digit() {
                    width = width * 10 + d.to_digit(10).unwrap() as usize;
                    chars.next();
                } else if d == 'l' || d == 'z' || d == 'h' || d == 'j' {
                    long += 1;
                    chars.next();
                } else {
                    break;
                }
            }
            let Some(conv) = chars.next() else { break };
            let text = match conv {
                '%' => "%".to_string(),
                'd' | 'i' => {
                    let v = next(&mut arg);
                    if long > 0 { (v as i64).to_string() } else { (v as i32).to_string() }
                }
                'u' => {
                    let v = next(&mut arg);
                    if long > 0 { v.to_string() } else { (v as u32).to_string() }
                }
                'x' | 'X' => {
                    let v = next(&mut arg);
                    let v = if long > 0 { v } else { v as u32 as u64 };
                    if conv == 'x' { format!("{v:x}") } else { format!("{v:X}") }
                }
                'p' => format!("0x{:x}", next(&mut arg)),
                'c' => ((next(&mut arg) as u8) as char).to_string(),
                's' => self.mem.read_cstr(next(&mut arg)),
                _ => String::new(),
            };
            let pad = width.saturating_sub(text.chars().count());
            if zero && !matches!(conv, 's' | 'c') {
                out.push_str(&"0".repeat(pad));
                out.push_str(&text);
            } else {
                out.push_str(&" ".repeat(pad));
                out.push_str(&text);
            }
        }
        out
    }

    fn call_system(&mut self, idx: usize) {
        self.calls += 1;
        let Some(name) = self.imports.get(idx).cloned() else { return };
        self.last_call = Some((name.clone(), ARG_REGS.map(|register| self.cpu.r[register])));
        self.record_graphics_call(&name);
        if self.call_graphics(&name) || self.call_random(&name) || self.call_launch(&name) || self.call_io(&name) || self.call_equeue(&name) || self.call_pool(&name) || self.call_rwlock(&name) || self.call_thread_attr(&name) || self.call_runtime(&name) { return; }
        match name.as_str() {
            "__return_from_entry" => self.exit_code = Some(self.cpu.r[10] as i32),
            "sceKernelGetProcParam" => self.ret(self.process_param),
            "sceKernelDebugRaiseException" => {
                self.runtime_error = Some(format!("The PS4 program requested debug exception {:#010X} during execution.", self.arg(0) as u32));
            }
            "__error" | "__errno_location" => self.ret(self.cpu.base[FS] + 0x80),
            "__tls_get_addr" => {
                let mut index = [0u8; 16];
                if !self.mem.read(self.arg(0), &mut index) { self.ret(0); return; }
                let id = u64::from_le_bytes(index[..8].try_into().unwrap());
                let offset = u64::from_le_bytes(index[8..].try_into().unwrap());
                let address = self.tls_offsets.get(&id).and_then(|base| self.cpu.base[FS].checked_sub(self.tls_size)?.checked_add(*base)?.checked_add(offset));
                self.ret(address.unwrap_or(0));
            }
            "exit" | "_exit" | "abort" => self.exit_code = Some(if name == "abort" { 134 } else { self.arg(0) as i32 }),
            "__stack_chk_fail" => {
                self.output.push_str("[stack smashing detected]\n");
                self.exit_code = Some(134);
            }
            "malloc" => {
                let a = self.heap_alloc(self.arg(0), 16);
                self.ret(a);
            }
            "calloc" => {
                let a = self.heap_alloc(self.arg(0).saturating_mul(self.arg(1)), 16);
                self.ret(a);
            }
            "memalign" => {
                let a = self.heap_alloc(self.arg(1), self.arg(0));
                self.ret(a);
            }
            "posix_memalign" => {
                let a = self.heap_alloc(self.arg(2), self.arg(1));
                let out = self.arg(0);
                self.mem.write_u64(out, a);
                self.ret(if a == 0 { 12 } else { 0 });
            }
            "realloc" => {
                let (old, size) = (self.arg(0), self.arg(1));
                let new = self.heap_alloc(size, 16);
                if old != 0 {
                    let oldsize = self.sizes.get(&old).copied().unwrap_or(0);
                    self.copy(new, old, oldsize.min(size));
                }
                self.ret(new);
            }
            "free" => {
                let p = self.arg(0);
                self.sizes.remove(&p);
            }
            "memcpy" | "memmove" => {
                let (d, s, n) = (self.arg(0), self.arg(1), self.arg(2));
                self.copy(d, s, n);
                self.ret(d);
            }
            "memset" => {
                let (d, v, n) = (self.arg(0), self.arg(1) as u8, self.arg(2));
                let buf = vec![v; n.min(1 << 24) as usize];
                self.mem.write(d, &buf);
                self.ret(d);
            }
            "memcmp" => {
                let n = self.arg(2).min(1 << 24) as usize;
                let (mut a, mut b) = (vec![0u8; n], vec![0u8; n]);
                self.mem.read(self.arg(0), &mut a);
                self.mem.read(self.arg(1), &mut b);
                let r = a.iter().zip(&b).find(|(x, y)| x != y).map_or(0i64, |(x, y)| *x as i64 - *y as i64);
                self.ret(r as u64);
            }
            "strlen" => {
                let s = self.mem.read_cstr(self.arg(0));
                self.ret(s.len() as u64);
            }
            "strcmp" | "strncmp" => {
                let (a, b) = (self.mem.read_cstr(self.arg(0)), self.mem.read_cstr(self.arg(1)));
                let (a, b) = if name == "strncmp" { (a.chars().take(self.arg(2) as usize).collect::<String>(), b.chars().take(self.arg(2) as usize).collect::<String>()) } else { (a, b) };
                self.ret(match a.cmp(&b) {
                    std::cmp::Ordering::Less => u64::MAX,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                });
            }
            "strcpy" => {
                let s = self.mem.read_cstr(self.arg(1));
                let mut b = s.into_bytes();
                b.push(0);
                self.mem.write(self.arg(0), &b);
                self.ret(self.arg(0));
            }
            "puts" => {
                let s = self.mem.read_cstr(self.arg(0));
                self.output.push_str(&s);
                self.output.push('\n');
                self.ret(1);
            }
            "printf" => {
                let f = self.mem.read_cstr(self.arg(0));
                let s = self.format(&f, 1);
                self.output.push_str(&s);
                self.ret(s.len() as u64);
            }
            "sprintf" | "snprintf" => {
                let (dst, f, first, cap) = if name == "sprintf" { (self.arg(0), self.mem.read_cstr(self.arg(1)), 2, u64::MAX) } else { (self.arg(0), self.mem.read_cstr(self.arg(2)), 3, self.arg(1)) };
                let s = self.format(&f, first);
                let mut b: Vec<u8> = s.bytes().take(cap.saturating_sub(1) as usize).collect();
                b.push(0);
                self.mem.write(dst, &b);
                self.ret(s.len() as u64);
            }
            "sceKernelDebugOutText" => {
                let s = self.mem.read_cstr(self.arg(1));
                self.output.push_str(&s);
                self.ret(0);
            }
            "sceKernelGetProcessTime" | "sceKernelGetProcessTimeCounter" | "sceKernelReadTsc" => {
                let t = self.cpu.tsc / 1000;
                self.ret(t);
            }
            "sceKernelGetProcessTimeCounterFrequency" => self.ret(1_000_000),
            "sceKernelIsNeoMode" => self.ret(0),
            "sceKernelMapFlexibleMemory" => {
                let result = self.map_flexible_memory();
                self.ret(result);
            }
            "strncpy" => {
                let (d, n) = (self.arg(0), self.arg(2).min(1 << 20) as usize);
                let mut b: Vec<u8> = self.mem.read_cstr(self.arg(1)).into_bytes().into_iter().take(n).collect();
                b.resize(n, 0);
                self.mem.write(d, &b);
                self.ret(d);
            }
            "strcat" => {
                let d = self.arg(0);
                let at = d + self.mem.read_cstr(d).len() as u64;
                let mut b = self.mem.read_cstr(self.arg(1)).into_bytes();
                b.push(0);
                self.mem.write(at, &b);
                self.ret(d);
            }
            "strchr" | "strrchr" => {
                let (p, c) = (self.arg(0), self.arg(1) as u8);
                let s = self.mem.read_cstr(p).into_bytes();
                let hit = if c == 0 {
                    Some(s.len())
                } else if name == "strchr" {
                    s.iter().position(|&x| x == c)
                } else {
                    s.iter().rposition(|&x| x == c)
                };
                self.ret(hit.map_or(0, |i| p + i as u64));
            }
            "strstr" => {
                let (p, hay, needle) = (self.arg(0), self.mem.read_cstr(self.arg(0)), self.mem.read_cstr(self.arg(1)));
                self.ret(hay.find(&needle).map_or(0, |i| p + i as u64));
            }
            "atoi" | "strtol" | "strtoul" => {
                let text = self.mem.read_cstr(self.arg(0));
                let base = if name == "atoi" { 10 } else { self.arg(2) as u32 };
                let t = text.trim_start();
                let (neg, t) = match t.strip_prefix('-') {
                    Some(r) => (true, r),
                    None => (false, t.strip_prefix('+').unwrap_or(t)),
                };
                let (base, t) = if (base == 16 || base == 0) && (t.starts_with("0x") || t.starts_with("0X")) {
                    (16, &t[2..])
                } else if base == 0 {
                    (10, t)
                } else {
                    (base.clamp(2, 36), t)
                };
                let digits: String = t.chars().take_while(|c| c.is_digit(base)).collect();
                let v = u64::from_str_radix(&digits, base).unwrap_or(0);
                if name != "atoi" && self.arg(1) != 0 {
                    let consumed = text.len() - t.len() + digits.len();
                    self.mem.write_u64(self.arg(1), self.arg(0) + consumed as u64);
                }
                self.ret(if neg { v.wrapping_neg() } else { v });
            }
            "srand" => self.rng = self.arg(0) as u32 as u64,
            "rand" => {
                self.rng = self.rng.wrapping_mul(1103515245).wrapping_add(12345) & 0x7FFF_FFFF;
                self.ret(self.rng);
            }
            "time" => {
                let t = 1_700_000_000 + self.cpu.tsc / 1_000_000_000;
                if self.arg(0) != 0 {
                    self.mem.write_u64(self.arg(0), t);
                }
                self.ret(t);
            }
            "gettimeofday" | "clock_gettime" => {
                let ns = self.cpu.tsc;
                let (at, sec, sub) = if name == "gettimeofday" { (self.arg(0), 1_700_000_000 + ns / 1_000_000_000, (ns % 1_000_000_000) / 1000) } else { (self.arg(1), ns / 1_000_000_000, ns % 1_000_000_000) };
                if at != 0 {
                    self.mem.write_u64(at, sec);
                    self.mem.write_u64(at + 8, sub);
                }
                self.ret(0);
            }
            "atexit" | "setlocale" | "_init_env" => self.ret(0),
            "usleep" => self.sleep_ns((self.arg(0) as u32 as u64) * 1000),
            "sceKernelUsleep" => self.sleep_ns(self.arg(0).saturating_mul(1000)),
            "sceKernelSleep" => self.sleep_ns(self.arg(0).saturating_mul(1_000_000_000)),
            "nanosleep" => {
                let ns = self.mem.read_u64(self.arg(0)).saturating_mul(1_000_000_000).saturating_add(self.mem.read_u64(self.arg(0) + 8));
                self.sleep_ns(ns);
            }
            "scePthreadYield" | "sched_yield" => self.yield_now(),
            "pthread_create" | "pthread_create_name_np" | "scePthreadCreate" => {
                let result = self.create_thread(name != "pthread_create");
                self.ret(if name == "scePthreadCreate" && result != 0 { 0x8002_0000 | result } else { result });
            }
            "pthread_join" | "scePthreadJoin" => self.join(self.arg(0), self.arg(1)),
            "pthread_exit" | "scePthreadExit" => self.thread_exit(self.arg(0)),
            "__thread_exit" => self.thread_exit(self.cpu.r[10]),
            "_sceKernelSetThreadDtors" => {
                self.sched.thread_dtor = self.arg(0);
                self.ret(0);
            }
            "pthread_self" | "scePthreadSelf" | "scePthreadGetthreadid" => self.ret(self.tid()),
            "pthread_key_create" | "scePthreadKeyCreate" => {
                let native = name == "scePthreadKeyCreate";
                let limit = if native { 256 } else { 1024 };
                let key = (0..limit).find(|key| !self.thread_keys.contains_key(key));
                match key {
                    Some(key) if self.mem.write(self.arg(0), &key.to_le_bytes()) => {
                        self.thread_keys.insert(key, self.arg(1));
                        self.ret(0);
                    }
                    Some(_) => self.ret(if native { 0x8002_0016 } else { 22 }),
                    None => self.ret(if native { 0x8002_0023 } else { 11 }),
                }
            }
            "pthread_key_delete" | "scePthreadKeyDelete" => {
                let key = self.arg(0) as u32;
                if self.thread_keys.remove(&key).is_some() {
                    self.thread_values.retain(|(_, existing), _| *existing != key);
                    self.ret(0);
                } else {
                    self.ret(if name == "scePthreadKeyDelete" { 0x8002_0016 } else { 22 });
                }
            }
            "pthread_getspecific" | "scePthreadGetspecific" => {
                let value = self.thread_values.get(&(self.tid(), self.arg(0) as u32)).copied().unwrap_or(0);
                self.ret(value);
            }
            "pthread_setspecific" | "scePthreadSetspecific" => {
                let key = self.arg(0) as u32;
                if self.thread_keys.contains_key(&key) {
                    self.thread_values.insert((self.tid(), key), self.arg(1));
                    self.ret(0);
                } else {
                    self.ret(if name == "scePthreadSetspecific" { 0x8002_0016 } else { 22 });
                }
            }
            "mmap" => {
                let size = self.arg(1);
                if size == 0 || size > RAM_SIZE as u64 || self.arg(3) & 0x1000 == 0 || self.arg(3) & 0x10 != 0 {
                    self.ret(u64::MAX);
                } else {
                    let address = self.heap_alloc(size, 0x4000);
                    self.ret(if address == 0 { u64::MAX } else { address });
                }
            }
            "munmap" | "sceKernelMunmap" => {
                let result = self.unmap_direct_range(self.arg(0), self.arg(1)) == 0;
                if result {
                    self.ret(0);
                } else if name == "sceKernelMunmap" {
                    self.ret(0x8002_0016);
                } else {
                    self.put32(self.cpu.base[FS] + 0x80, 22);
                    self.ret(u64::MAX);
                }
            }
            "pthread_mutexattr_init" | "pthread_mutexattr_destroy" | "scePthreadMutexattrInit" | "scePthreadMutexattrDestroy" => {
                let valid = self.mem.write(self.arg(0), &0u32.to_le_bytes());
                self.ret(if valid { 0 } else { 22 });
            }
            "pthread_mutexattr_settype" | "scePthreadMutexattrSettype" => {
                let kind = self.arg(1) as u32;
                let valid = kind <= 2 && self.mem.write(self.arg(0), &kind.to_le_bytes());
                self.ret(if valid { 0 } else { 22 });
            }
            "pthread_mutex_init" | "scePthreadMutexInit" | "pthread_mutex_destroy" | "scePthreadMutexDestroy" => self.mutex_destroy(self.arg(0)),
            "pthread_mutex_lock" | "scePthreadMutexLock" => self.mutex_lock(self.arg(0), true),
            "pthread_mutex_trylock" | "scePthreadMutexTrylock" => self.mutex_lock(self.arg(0), false),
            "pthread_mutex_unlock" | "scePthreadMutexUnlock" => self.mutex_unlock(self.arg(0)),
            "pthread_cond_wait" | "scePthreadCondWait" => self.cond_wait(self.arg(0), self.arg(1), None),
            "pthread_cond_timedwait" => self.posix_cond_timedwait(),
            "scePthreadCondTimedwait" => self.cond_wait(self.arg(0), self.arg(1), Some(self.arg(2).saturating_mul(1000))),
            "pthread_cond_signal" | "scePthreadCondSignal" => self.cond_signal(self.arg(0), false),
            "pthread_cond_broadcast" | "scePthreadCondBroadcast" => self.cond_signal(self.arg(0), true),
            "sceKernelCreateSema" => self.sema_create(self.arg(0), self.arg(3), self.arg(4)),
            "sceKernelWaitSema" => {
                let t = self.arg(2);
                let timeout = if t == 0 { None } else { Some(self.mem.read_u64(t) & 0xFFFF_FFFF) };
                self.sema_wait(self.arg(0) & 0xFFFF_FFFF, self.arg(1), timeout);
            }
            "sceKernelSignalSema" => self.sema_signal(self.arg(0) & 0xFFFF_FFFF, self.arg(1)),
            "sceKernelDeleteSema" => self.sema_delete(self.arg(0) & 0xFFFF_FFFF),
            "pthread_cond_init" | "scePthreadCondInit" | "pthread_cond_destroy" | "scePthreadCondDestroy" | "scePthreadAttrInit" | "scePthreadAttrDestroy" | "scePthreadAttrSetstacksize" | "scePthreadAttrSetdetachstate" | "scePthreadAttrSetschedparam" | "scePthreadSetaffinity" | "scePthreadRename" | "pthread_detach" | "scePthreadDetach" => self.ret(0),
            "sceKernelGetDirectMemorySize" => self.ret(0x1_4000_0000),
            "sceKernelAvailableFlexibleMemorySize" => {
                self.mem.write_u64(self.arg(0), 0x1000_0000);
                self.ret(0);
            }
            "sceKernelAllocateDirectMemory" | "sceKernelAllocateMainDirectMemory" => {
                let (len, align, out) = if name == "sceKernelAllocateDirectMemory" { (self.arg(2), self.arg(3), self.arg(5)) } else { (self.arg(0), self.arg(1), self.arg(3)) };
                let align = align.max(0x4000);
                let at = (self.direct_next + align - 1) & !(align - 1);
                self.direct_next = at + len;
                self.mem.write_u64(out, at);
                self.ret(0);
            }
            "sceKernelMapDirectMemory" | "sceKernelMapNamedDirectMemory" => {
                let (out, len, align) = (self.arg(0), self.arg(1), self.arg_ext(5));
                let a = self.heap_alloc(len, align.max(0x4000));
                if a == 0 && self.output.len() < 1 << 20 {
                    self.output.push_str(&format!("PS4 direct mapping failed: bytes={len:#x}, alignment={align:#x}, heap={:#x}, physical_used={:#x}, limit={RAM_SIZE:#x}\n", self.heap_next, self.mem.next_frame));
                }
                self.mem.write_u64(out, a);
                self.ret(if a == 0 { 0x8002_000C } else { 0 });
            }
            "sceKernelReleaseDirectMemory" | "sceSysmoduleLoadModule" | "sceSysmoduleIsLoaded" | "scePadInit" | "sceSystemServiceHideSplashScreen" | "sceCommonDialogInitialize" | "sceVideoOutClose" | "sceVideoOutSetFlipRate" | "sceAudioOutInit" | "sceSaveDataInitialize" => self.ret(0),
            "sceKernelGetCompiledSdkVersion" => {
                self.put32(self.arg(0), 0x0900_0000);
                self.ret(0);
            }
            "sceUserServiceGetInitialUser" | "sceUserServiceGetForegroundUser" => {
                self.put32(self.arg(0), 1);
                self.ret(0);
            }
            "scePadOpen" => self.ret(1),
            "scePadReadState" | "scePadRead" => {
                if name == "scePadRead" && self.arg(2) == 0 {
                    self.ret(0);
                } else {
                    let mut data = [0u8; 120];
                    data[..4].copy_from_slice(&self.pad_buttons.to_le_bytes());
                    data[4..8].fill(128);
                    data[24..28].copy_from_slice(&1.0f32.to_le_bytes());
                    data[76] = 1;
                    data[80..88].copy_from_slice(&(self.cpu.tsc / 1000).to_le_bytes());
                    data[104] = 1;
                    let valid = self.mem.write(self.arg(1), &data);
                    self.ret(if !valid { 0x8092_0006 } else if name == "scePadRead" { 1 } else { 0 });
                }
            }
            "sceSystemServiceGetStatus" => {
                self.mem.write(self.arg(0), &[0u8; 0x80]);
                self.ret(0);
            }
            "sceSystemServiceReceiveEvent" => self.ret(0x80A1_0003), // no event waiting
            "sceVideoOutIsFlipPending" => self.ret(0),
            "sceKernelSendNotificationRequest" => {
                let length = self.arg(2).saturating_sub(45).min(1024) as usize;
                let mut data = vec![0u8; length];
                let address = self.arg(1).checked_add(45);
                let valid = self.arg(2) >= 45 && address.is_some_and(|address| self.mem.read(address, &mut data));
                if valid {
                    let length = data.iter().position(|byte| *byte == 0).unwrap_or(data.len());
                    if self.output.len() < 1 << 20 {
                        self.output.push_str(&String::from_utf8_lossy(&data[..length]));
                        self.output.push('\n');
                    }
                    self.ret(0);
                } else { self.ret(0x8002000E); }
            }
            "sceVideoOutOpen" => self.ret(1),
            "sceVideoOutGetResolutionStatus" => {
                let mut status = [0u8; 48];
                for (index, value) in [1920u32, 1080, 1920, 1080].iter().enumerate() {
                    status[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
                }
                status[16..24].copy_from_slice(&60u64.to_le_bytes());
                let valid = self.mem.write(self.arg(1), &status);
                self.ret(if valid { 0 } else { 0x8002000E });
            }
            "sceVideoOutSetBufferAttribute" => {
                let at = self.arg(0);
                for (k, v) in [self.arg(1), self.arg(2), self.arg(3), self.arg(4), self.arg(5), self.arg_ext(6)].into_iter().enumerate() {
                    self.put32(at + 4 * k as u64, v as u32);
                }
                self.put32(at + 24, 0);
                self.ret(0);
            }
            "sceVideoOutRegisterBuffers" => {
                let (start, list, num, attr) = (self.arg(1) as usize, self.arg(2), self.arg(3) as usize, self.arg(4));
                let mut a = [0u8; 24];
                let valid_range = num != 0 && start.checked_add(num).is_some_and(|end| end <= 16);
                if !valid_range || !self.mem.read(attr, &mut a) {
                    self.ret(0x80020016);
                    return;
                }
                let f = |i: usize| u32::from_le_bytes(a[i * 4..i * 4 + 4].try_into().unwrap());
                let (width, height, pitch) = (f(3), f(4), f(5));
                let mut addresses = vec![0u8; num * 8];
                if width == 0 || height == 0 || width > 8192 || height > 8192 || pitch < width || pitch > 8192 || !self.mem.read(list, &mut addresses) {
                    self.ret(0x80020016);
                    return;
                }
                self.video.pixel_format = f(0);
                self.video.width = width;
                self.video.height = height;
                self.video.pitch = pitch;
                if self.video.buffers.len() < start + num {
                    self.video.buffers.resize(start + num, 0);
                }
                for i in 0..num.min(16) {
                    self.video.buffers[start + i] = self.mem.read_u64(list + 8 * i as u64);
                }
                self.ret(0);
            }
            "sceVideoOutSubmitFlip" => {
                let i = self.arg(1) as i64;
                if i < 0 || i as usize >= self.video.buffers.len() || self.video.buffers[i as usize] == 0 {
                    self.ret(0x8002_0016);
                    return;
                }
                self.video.current = Some(i as usize);
                self.video.last_flip_arg = self.arg(3);
                self.video.last_flip_time = self.cpu.tsc / 1000;
                self.video.flips += 1;
                self.signal_video_flip(self.arg(0));
                self.yield_now();
            }
            "sceVideoOutGetFlipStatus" => {
                let address = self.arg(1);
                let mut st = [0u8; 64];
                if !self.mem.read(address, &mut st) { self.ret(0x8002_000E); return; }
                st.fill(0);
                st[..8].copy_from_slice(&self.video.flips.to_le_bytes());
                st[8..16].copy_from_slice(&self.video.last_flip_time.to_le_bytes());
                st[16..24].copy_from_slice(&self.video.last_flip_time.to_le_bytes());
                st[24..32].copy_from_slice(&self.video.last_flip_arg.to_le_bytes());
                let current = self.video.current.map_or(-1, |index| index as i32);
                st[56..60].copy_from_slice(&current.to_le_bytes());
                self.mem.write(address, &st);
                self.ret(0);
            }
            other => {
                if !self.unimplemented.iter().any(|u| u == other) {
                    self.unimplemented.push(other.to_string());
                }
                let result = if matches!(other, "sceFiosFHOpenWithModeSync" | "sceFiosFHOpenSync" | "sceFiosFHGetSize") { u64::MAX } else { 0 };
                self.ret(result);
            }
        }
    }

    // ------------------------------------------------------------ running

    /// Runs up to `max_steps` instructions.
    pub fn run(&mut self, max_steps: u64) -> Result<Stop, String> {
        self.pump_graphics();
        for _ in 0..max_steps {
            self.apply_launch();
            if let Some(error) = &self.runtime_error { return Err(error.clone()); }
            if let Some(c) = self.exit_code {
                return Ok(Stop::Exited(c));
            }
            self.cpu.step(&mut self.mem);
            if let Some(t) = self.cpu.trap.take() {
                if t == 1 {
                    let idx = self.cpu.r[0] as u32 as usize;
                    self.call_system(idx);
                }
            }
            if let Some(msg) = self.cpu.shutdown.take() {
                return Err(format!("The program crashed at {:#x} ({msg}).", self.cpu.rip));
            }
            if let Some(msg) = self.sched.stuck.take() {
                return Err(format!("The program stopped at {:#x}: {msg}.", self.cpu.rip));
            }
            if self.cpu.halted {
                return Err(format!("The program stopped at {:#x} (it executed a halt).", self.cpu.rip));
            }
        }
        Ok(match self.exit_code {
            Some(c) => Stop::Exited(c),
            None => Stop::Running,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::nid::nid;

    #[test]
    fn physical_addresses_above_four_gibibytes_do_not_wrap() {
        let mut mem = Mem::new().unwrap();
        mem.write8(0x1234, 91);
        assert_eq!(mem.read8(0x1_0000_1234), 0xFF);
        assert_eq!(mem.ram_index(0x1_0000_1234), None);
        mem.write8(0x1_0000_1234, 17);
        assert_eq!(mem.read8(0x1234), 91);
        assert_eq!(mem.read8(u64::MAX), 0xFF);
    }

    /// A tiny PlayStation-style program: calls puts("hello from PS5"), then exit(7). Built
    /// by hand with the layout the loader expects (symbols named NID#lib#module, jump-slot
    /// relocations into a table the program calls through).
    pub(crate) fn hello_program() -> Vec<u8> {
        const BASE: u64 = 0x40_0000;
        // Dynamic-linking data.
        let mut strtab = vec![0u8];
        let add = |s: &str, tab: &mut Vec<u8>| -> u32 {
            let at = tab.len() as u32;
            tab.extend_from_slice(s.as_bytes());
            tab.push(0);
            at
        };
        let lib = add("libc", &mut strtab);
        let s_puts = add(&format!("{}#A#A", nid("puts")), &mut strtab);
        let s_exit = add(&format!("{}#A#A", nid("exit")), &mut strtab);
        let mut dynlib = strtab.clone();
        while dynlib.len() % 8 != 0 {
            dynlib.push(0);
        }
        let symtab_off = dynlib.len();
        for n in [0u32, s_puts, s_exit] {
            dynlib.extend_from_slice(&n.to_le_bytes());
            dynlib.extend_from_slice(&[0x12, 0, 0, 0]);
            dynlib.extend_from_slice(&[0u8; 16]);
        }
        let rela_off = dynlib.len();
        // Code and data layout (virtual addresses).
        let code_va = BASE + 0x1000;
        let got_va = code_va + 0x100;
        let msg_va = code_va + 0x200;
        for (slot, sym) in [(got_va, 1u64), (got_va + 8, 2)] {
            dynlib.extend_from_slice(&slot.to_le_bytes());
            dynlib.extend_from_slice(&((sym << 32) | 7).to_le_bytes());
            dynlib.extend_from_slice(&0i64.to_le_bytes());
        }
        let dynamic: Vec<(i64, u64)> = vec![
            (0x6100_0035, 0),
            (0x6100_0037, strtab.len() as u64),
            (0x6100_0039, symtab_off as u64),
            (0x6100_003B, 24),
            (0x6100_003F, 3 * 24),
            (0x6100_0015, lib as u64),
            (0x6100_0029, rela_off as u64),
            (0x6100_002D, 2 * 24),
            (0, 0),
        ];
        let mut dyn_bytes = Vec::new();
        for (t, v) in dynamic {
            dyn_bytes.extend_from_slice(&t.to_le_bytes());
            dyn_bytes.extend_from_slice(&v.to_le_bytes());
        }

        let mut code = Vec::new();
        // lea rdi, [rip + msg]
        code.extend_from_slice(&[0x48, 0x8D, 0x3D]);
        let next = code_va + code.len() as u64 + 4;
        code.extend_from_slice(&((msg_va as i64 - next as i64) as i32).to_le_bytes());
        // call [rip + got_puts]
        code.extend_from_slice(&[0xFF, 0x15]);
        let next = code_va + code.len() as u64 + 4;
        code.extend_from_slice(&((got_va as i64 - next as i64) as i32).to_le_bytes());
        // mov edi, 7
        code.extend_from_slice(&[0xBF, 7, 0, 0, 0]);
        // call [rip + got_exit]
        code.extend_from_slice(&[0xFF, 0x15]);
        let next = code_va + code.len() as u64 + 4;
        code.extend_from_slice(&((got_va + 8) as i64 - next as i64).to_le_bytes()[..4]);
        code.push(0xF4); // hlt: never reached

        // File layout: headers, dynamic, dynlib, then a page-aligned image holding the code.
        let phnum = 4usize;
        let hdr = 64 + phnum * 56;
        let dyn_off = hdr;
        let lib_off = dyn_off + dyn_bytes.len();
        let image_off = 0x1000usize;
        let image_len = 0x300usize;
        let mut f = vec![0u8; image_off + image_len];
        f[..4].copy_from_slice(b"\x7FELF");
        f[4] = 2;
        f[5] = 1;
        f[16..18].copy_from_slice(&0xFE10u16.to_le_bytes());
        f[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
        f[24..32].copy_from_slice(&code_va.to_le_bytes());
        f[32..40].copy_from_slice(&64u64.to_le_bytes());
        f[54..56].copy_from_slice(&56u16.to_le_bytes());
        f[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());
        let ph = |f: &mut Vec<u8>, i: usize, t: u32, flags: u32, off: usize, va: u64, sz: usize| {
            let o = 64 + i * 56;
            f[o..o + 4].copy_from_slice(&t.to_le_bytes());
            f[o + 4..o + 8].copy_from_slice(&flags.to_le_bytes());
            f[o + 8..o + 16].copy_from_slice(&(off as u64).to_le_bytes());
            f[o + 16..o + 24].copy_from_slice(&va.to_le_bytes());
            f[o + 32..o + 40].copy_from_slice(&(sz as u64).to_le_bytes());
            f[o + 40..o + 48].copy_from_slice(&(sz as u64).to_le_bytes());
        };
        ph(&mut f, 0, 1, 7, image_off, code_va, image_len);
        ph(&mut f, 1, 2, 4, dyn_off, 0, dyn_bytes.len());
        ph(&mut f, 2, 0x6100_0000, 4, lib_off, 0, dynlib.len());
        ph(&mut f, 3, 1, 4, 0, BASE, hdr);
        f[dyn_off..dyn_off + dyn_bytes.len()].copy_from_slice(&dyn_bytes);
        f[lib_off..lib_off + dynlib.len()].copy_from_slice(&dynlib);
        f[image_off..image_off + code.len()].copy_from_slice(&code);
        let msg = b"hello from PS5\0";
        f[image_off + 0x200..image_off + 0x200 + msg.len()].copy_from_slice(msg);
        f
    }

    #[test]
    fn runs_a_program_that_calls_system_functions() {
        let mut m = Machine::load(&hello_program()).expect("loads");
        assert_eq!(m.run(10_000).expect("runs"), Stop::Exited(7));
        assert_eq!(m.output, "hello from PS5\n");
        assert!(m.unimplemented.is_empty());
        assert_eq!(m.calls, 2);
    }

    #[test]
    fn loads_sony_read_only_data_before_running() {
        let mut file = hello_program();
        let header = 64 + 3 * 56;
        file[header..header + 4].copy_from_slice(&0x6100_0010u32.to_le_bytes());
        file[header + 32..header + 40].copy_from_slice(&0x400u64.to_le_bytes());
        file[header + 40..header + 48].copy_from_slice(&0x400u64.to_le_bytes());
        file[0x300..0x30d].copy_from_slice(b"relro loaded\0");
        let message = 0x40_0300i64;
        let next_instruction = 0x40_1007i64;
        file[0x1003..0x1007].copy_from_slice(&((message - next_instruction) as i32).to_le_bytes());

        let mut machine = Machine::load(&file).unwrap();
        assert_eq!(machine.run(10_000).unwrap(), Stop::Exited(7));
        assert_eq!(machine.output, "relro loaded\n");
        assert_eq!(machine.calls, 2);
    }

    #[test]
    fn formats_printf_arguments() {
        let mut m = Machine::load(&hello_program()).unwrap();
        m.mem.map(0x5000_0000, 0x1000, true, false).unwrap();
        m.mem.write(0x5000_0000, b"n=%d s=%s h=%04x%%\0");
        m.mem.write(0x5000_0100, b"abc\0");
        m.cpu.r[6] = (-5i64) as u64;
        m.cpu.r[2] = 0x5000_0100;
        m.cpu.r[1] = 0xBEE;
        assert_eq!(m.format("n=%d s=%s h=%04x%%", 1), "n=-5 s=abc h=0bee%");
    }

    #[test]
    fn thread_specific_keys_keep_values_and_clear_deleted_keys() {
        let mut machine = Machine::load(&hello_program()).unwrap();
        let out = machine.cpu.r[4] + 0x40;
        machine.cpu.r[7] = out;
        machine.cpu.r[6] = 0;
        call(&mut machine, "pthread_key_create");
        assert_eq!(machine.cpu.r[0], 0);
        let key = machine.mem.read_u64(out) & 0xFFFF_FFFF;
        machine.cpu.r[7] = key;
        call(&mut machine, "pthread_getspecific");
        assert_eq!(machine.cpu.r[0], 0);
        machine.cpu.r[6] = 0x1234;
        call(&mut machine, "pthread_setspecific");
        assert_eq!(machine.cpu.r[0], 0);
        call(&mut machine, "pthread_getspecific");
        assert_eq!(machine.cpu.r[0], 0x1234);
        call(&mut machine, "pthread_key_delete");
        call(&mut machine, "pthread_getspecific");
        assert_eq!(machine.cpu.r[0], 0);
        call(&mut machine, "pthread_setspecific");
        assert_eq!(machine.cpu.r[0], 22);
    }

    #[test]
    fn pad_reports_connected_buttons_and_neutral_sticks_without_overwriting() {
        let mut machine = Machine::load(&hello_program()).unwrap();
        let address = machine.cpu.r[4] + 0x40;
        machine.mem.write(address, &[0xAA; 128]);
        machine.pad_buttons = 0x4010;
        machine.cpu.r[6] = address;
        call(&mut machine, "scePadReadState");
        assert_eq!(machine.cpu.r[0], 0);
        let mut data = [0u8; 128];
        assert!(machine.mem.read(address, &mut data));
        assert_eq!(&data[..4], &0x4010u32.to_le_bytes());
        assert_eq!(&data[4..8], &[128; 4]);
        assert_eq!(data[76], 1);
        assert_eq!(&data[120..], &[0xAA; 8]);
    }

    #[test]
    fn errno_returns_writable_thread_local_storage() {
        let mut machine = Machine::load(&hello_program()).unwrap();
        call(&mut machine, "__error");
        let address = machine.cpu.r[0];
        assert_ne!(address, 0);
        assert!(machine.mem.write(address, &22u32.to_le_bytes()));
        call(&mut machine, "__errno_location");
        assert_eq!(machine.cpu.r[0], address);
        assert_eq!(machine.mem.read_u64(address), 22);
    }

    #[test]
    fn anonymous_mmap_returns_accessible_aligned_memory() {
        let mut machine = Machine::load(&hello_program()).unwrap();
        machine.cpu.r[6] = 0x4000;
        machine.cpu.r[1] = 0x1002;
        call(&mut machine, "mmap");
        let address = machine.cpu.r[0];
        assert_ne!(address, u64::MAX);
        assert_eq!(address & 0x3FFF, 0);
        assert!(machine.mem.write(address + 0x3FFF, &[42]));
        machine.cpu.r[6] = 0;
        call(&mut machine, "mmap");
        assert_eq!(machine.cpu.r[0], u64::MAX);
    }

    #[test]
    fn resolution_is_initialized_and_invalid_video_counts_are_rejected() {
        let mut machine = Machine::load(&hello_program()).unwrap();
        let address = machine.cpu.r[4] + 0x40;
        machine.mem.write(address, &[0xAA; 56]);
        machine.cpu.r[6] = address;
        call(&mut machine, "sceVideoOutGetResolutionStatus");
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.mem.read_u64(address), 1920 | (1080u64 << 32));
        assert_eq!(machine.mem.read_u64(address + 48), 0xAAAAAAAAAAAAAAAA);
        machine.cpu.r[6] = 0;
        machine.cpu.r[1] = u64::MAX;
        call(&mut machine, "sceVideoOutRegisterBuffers");
        assert_ne!(machine.cpu.r[0], 0);
        assert!(machine.video.buffers.is_empty());
        assert_eq!(machine.heap_alloc(u64::MAX, 16), 0);
        assert_eq!(machine.heap_alloc(16, u64::MAX), 0);
        assert!(machine.mem.map(u64::MAX - 1, 4096, true, false).is_err());
    }

    #[test]
    fn flip_status_uses_the_native_64_byte_layout_and_rejects_partial_buffers() {
        let mut machine = Machine::load(&hello_program()).unwrap();
        let address = 0x50000000;
        machine.mem.map(address, 4096, true, false).unwrap();
        machine.mem.write(address, &[0xaa; 80]);
        machine.cpu.r[7] = 1;
        machine.cpu.r[6] = address + 8;
        call(&mut machine, "sceVideoOutGetFlipStatus");
        assert_eq!(machine.cpu.r[0], 0);
        let mut index = [0; 4];
        machine.mem.read(address + 64, &mut index);
        assert_eq!(i32::from_le_bytes(index), -1);
        machine.video.buffers.push(address + 256);
        machine.cpu.r[6] = 0;
        machine.cpu.r[1] = (-123i64) as u64;
        machine.cpu.tsc = 987654000;
        call(&mut machine, "sceVideoOutSubmitFlip");
        machine.cpu.r[6] = address + 8;
        call(&mut machine, "sceVideoOutGetFlipStatus");
        assert_eq!(machine.mem.read_u64(address), 0xaaaaaaaaaaaaaaaa);
        assert_eq!(machine.mem.read_u64(address + 8), 1);
        assert_eq!(machine.mem.read_u64(address + 16), 987654);
        assert_eq!(machine.mem.read_u64(address + 24), 987654);
        assert_eq!(machine.mem.read_u64(address + 32), (-123i64) as u64);
        assert_eq!(machine.mem.read_u64(address + 64), 0);
        assert_eq!(machine.mem.read_u64(address + 72), 0xaaaaaaaaaaaaaaaa);
        machine.mem.write(address + 4096 - 32, &[0xbb; 32]);
        machine.cpu.r[6] = address + 4096 - 32;
        call(&mut machine, "sceVideoOutGetFlipStatus");
        assert_eq!(machine.cpu.r[0], 0x8002000e);
        assert_eq!(machine.mem.read_u64(address + 4096 - 32), 0xbbbbbbbbbbbbbbbb);
        machine.cpu.r[6] = 5;
        call(&mut machine, "sceVideoOutSubmitFlip");
        assert_eq!(machine.cpu.r[0], 0x80020016);
        assert_eq!(machine.video.flips, 1);
        assert_eq!(machine.video.current, Some(0));
    }

    #[test]
    fn heap_blocks_are_aligned_and_distinct() {
        let mut m = Machine::load(&hello_program()).unwrap();
        let a = m.heap_alloc(100, 16);
        let b = m.heap_alloc(100, 64);
        assert_ne!(a, b);
        assert_eq!(a % 16, 0);
        assert_eq!(b % 64, 0);
        assert!(m.mem.write(a, &[1, 2, 3]));
    }

    #[test]
    fn libc_helpers() {
        let mut m = Machine::load(&hello_program()).unwrap();
        m.mem.map(0x5000_0000, 0x1000, true, false).unwrap();
        m.mem.write(0x5000_0000, b"hello world\0");
        m.mem.write(0x5000_0100, b"-0x1F rest\0");
        m.cpu.r[7] = 0x5000_0000;
        m.cpu.r[6] = b'o' as u64;
        m.imports.push("strrchr".into());
        m.call_system(m.imports.len() - 1);
        assert_eq!(m.cpu.r[0], 0x5000_0000 + 7);
        m.cpu.r[7] = 0x5000_0100;
        m.cpu.r[6] = 0;
        m.cpu.r[2] = 0;
        m.imports.push("strtol".into());
        m.call_system(m.imports.len() - 1);
        assert_eq!(m.cpu.r[0] as i64, -31);
    }

    #[test]
    fn video_out_flip_gives_a_frame() {
        let mut m = Machine::load(&hello_program()).unwrap();
        m.mem.map(0x5000_0000, 0x1000, true, false).unwrap();
        m.mem.map(0x5100_0000, 0x1000, true, false).unwrap();
        let call = |m: &mut Machine, name: &str| {
            m.imports.push(name.into());
            m.call_system(m.imports.len() - 1);
        };
        // attr at 0x5000_0000: B8G8R8A8, 2x2, pitch 2; one buffer at 0x5100_0000.
        m.cpu.r[7] = 0x5000_0000;
        m.cpu.r[6] = 0x8000_0000;
        m.cpu.r[2] = 0;
        m.cpu.r[1] = 0;
        m.cpu.r[8] = 2;
        m.cpu.r[9] = 2;
        // seventh argument (pitch) lives on the stack above the return address
        let sp = m.cpu.r[4];
        m.mem.write_u64(sp + 8, 2);
        call(&mut m, "sceVideoOutSetBufferAttribute");
        m.mem.write_u64(0x5000_0200, 0x5100_0000);
        m.mem.write(0x5100_0000, &[10, 20, 30, 255, 1, 2, 3, 255, 0, 0, 0, 255, 0, 0, 0, 255]);
        m.cpu.r[7] = 1;
        m.cpu.r[6] = 0;
        m.cpu.r[2] = 0x5000_0200;
        m.cpu.r[1] = 1;
        m.cpu.r[8] = 0x5000_0000;
        call(&mut m, "sceVideoOutRegisterBuffers");
        assert!(m.framebuffer().is_none(), "nothing flipped yet");
        m.cpu.r[7] = 1;
        m.cpu.r[6] = 0;
        call(&mut m, "sceVideoOutSubmitFlip");
        let (w, h, px) = m.framebuffer().unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(&px[..4], &[30, 20, 10, 255], "BGRA is turned into RGBA");
        assert_eq!(m.video.flips, 1);
    }

    fn call(m: &mut Machine, name: &str) {
        let i = match m.imports.iter().position(|n| n == name) {
            Some(i) => i,
            None => {
                m.imports.push(name.into());
                m.imports.len() - 1
            }
        };
        m.call_system(i);
    }

    #[test]
    fn a_created_thread_runs_and_join_collects_its_result() {
        let mut m = Machine::load(&hello_program()).unwrap();
        m.mem.map(0x5000_0000, 0x1000, true, false).unwrap();
        let main_rsp = m.cpu.r[4];
        // scePthreadCreate(&tid, 0, start, arg, name)
        m.cpu.r[6] = 0;
        m.cpu.r[8] = 0;
        m.cpu.r[7] = 0x5000_0000;
        m.cpu.r[2] = 0x40_1234;
        m.cpu.r[1] = 99;
        call(&mut m, "scePthreadCreate");
        assert_eq!(m.cpu.r[0], 0);
        let tid = m.mem.read_u64(0x5000_0000);
        assert_eq!(tid, 2);
        assert_eq!(m.tid(), 1, "creating a thread doesn't switch to it");
        // main joins: it blocks, so the new thread starts at its function with the argument
        m.cpu.r[7] = tid;
        m.cpu.r[6] = 0x5000_0100;
        call(&mut m, "scePthreadJoin");
        assert_eq!((m.tid(), m.cpu.rip, m.cpu.r[7]), (2, 0x40_1234, 99));
        assert_ne!(m.cpu.r[4], main_rsp);
        // the thread returns 77 to the exit stub
        m.cpu.r[0] = 77;
        m.cpu.rip = m.mem.read_u64(m.cpu.r[4]);
        m.cpu.r[4] += 8;
        for _ in 0..3 {
            m.cpu.step(&mut m.mem);
        }
        assert_eq!(m.cpu.trap.take(), Some(1));
        m.call_system(m.cpu.r[0] as usize);
        assert_eq!(m.tid(), 1);
        assert_eq!((m.cpu.r[0], m.cpu.r[4]), (0, main_rsp));
        assert_eq!(m.mem.read_u64(0x5000_0100), 77);
    }

    #[test]
    fn mutex_hands_over_and_condition_wakes_the_waiter() {
        let mut m = Machine::load(&hello_program()).unwrap();
        m.mem.map(0x5000_0000, 0x1000, true, false).unwrap();
        m.cpu.r[7] = 0x5000_0000;
        m.cpu.r[6] = 0;
        m.cpu.r[2] = m.entry;
        call(&mut m, "pthread_create");
        assert_eq!(m.cpu.r[0], 0);
        let (mutex, cond) = (0x5000_0200u64, 0x5000_0300u64);
        // main takes the mutex and waits on the condition (which releases the mutex)
        m.cpu.r[7] = mutex;
        call(&mut m, "pthread_mutex_lock");
        m.cpu.r[7] = cond;
        m.cpu.r[6] = mutex;
        call(&mut m, "pthread_cond_wait");
        assert_eq!(m.tid(), 2, "the worker runs while main waits");
        // the worker locks, signals and unlocks; main then owns the mutex and resumes
        m.cpu.r[7] = mutex;
        call(&mut m, "pthread_mutex_lock");
        assert_eq!(m.cpu.r[0], 0);
        m.cpu.r[7] = cond;
        call(&mut m, "pthread_cond_signal");
        m.cpu.r[7] = mutex;
        call(&mut m, "pthread_mutex_unlock");
        m.cpu.r[0] = 0;
        call(&mut m, "__thread_exit");
        assert_eq!(m.tid(), 1);
        m.cpu.r[7] = mutex;
        call(&mut m, "pthread_mutex_trylock");
        assert_eq!(m.cpu.r[0], 0, "main owns it again (recursive)");
    }

    #[test]
    fn sleep_advances_time_and_a_deadlock_is_reported() {
        let mut m = Machine::load(&hello_program()).unwrap();
        let t0 = m.cpu.tsc;
        m.cpu.r[7] = 2000;
        call(&mut m, "sceKernelUsleep");
        assert!(m.cpu.tsc >= t0 + 2_000_000);
        // joining a thread that never ends, with nothing else to run
        m.mem.map(0x5000_0000, 0x1000, true, false).unwrap();
        m.cpu.r[7] = 0x5000_0000;
        m.cpu.r[6] = 0;
        m.cpu.r[2] = m.entry;
        call(&mut m, "pthread_create");
        assert_eq!(m.cpu.r[0], 0);
        let tid = m.mem.read_u64(0x5000_0000);
        m.cpu.r[7] = tid;
        m.cpu.r[6] = 0;
        call(&mut m, "pthread_join"); // main waits for the worker, which runs
        m.cpu.r[7] = 0x5000_0400;
        m.cpu.r[6] = 0x5000_0400;
        call(&mut m, "pthread_cond_wait"); // the worker waits on a condition nobody signals
        assert!(m.sched.stuck.is_some());
    }
    #[test]
    fn native_thread_keys_share_storage_and_report_sony_errors() {
        let mut machine = Machine::load(&hello_program()).unwrap();
        let output = machine.heap_alloc(8, 8);
        machine.mem.write(output, &[0xCC; 8]);
        machine.cpu.r[7] = output;
        machine.cpu.r[6] = 0;
        call(&mut machine, "scePthreadKeyCreate");
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.mem.read_u64(output), 0xCCCCCCCC00000000);
        machine.cpu.r[7] = 0;
        machine.cpu.r[6] = 0x1234;
        call(&mut machine, "scePthreadSetspecific");
        assert_eq!(machine.cpu.r[0], 0);
        call(&mut machine, "pthread_getspecific");
        assert_eq!(machine.cpu.r[0], 0x1234);
        call(&mut machine, "scePthreadKeyDelete");
        assert_eq!(machine.cpu.r[0], 0);
        call(&mut machine, "scePthreadGetspecific");
        assert_eq!(machine.cpu.r[0], 0);
        call(&mut machine, "scePthreadSetspecific");
        assert_eq!(machine.cpu.r[0], 0x80020016);
        machine.cpu.r[7] = u64::MAX;
        call(&mut machine, "scePthreadKeyCreate");
        assert_eq!(machine.cpu.r[0], 0x80020016);
        assert!(machine.thread_keys.is_empty());
        machine.thread_keys.extend((0..256).map(|key| (key, 0)));
        machine.cpu.r[7] = output;
        call(&mut machine, "scePthreadKeyCreate");
        assert_eq!(machine.cpu.r[0], 0x80020023);
        assert_eq!(crate::nid::lookup("geDaqgH9lTg"), Some("scePthreadKeyCreate"));
    }

    #[test]
    fn invalid_fios_open_fails_without_creating_a_fake_handle() {
        for name in ["sceFiosFHOpenWithModeSync", "sceFiosFHOpenSync", "sceFiosFHGetSize"] {
            let mut machine = Machine::load(&hello_program()).unwrap();
            let output = machine.heap_alloc(8, 8);
            machine.mem.write_u64(output, 0x12345678);
            machine.cpu.r[6] = output;
            call(&mut machine, name);
            assert!((machine.cpu.r[0] as i32) < 0);
            assert_eq!(machine.mem.read_u64(output), 0x12345678);
            assert!(machine.unimplemented.is_empty());
            call(&mut machine, name);
            assert!(machine.unimplemented.is_empty());
        }
    }

    #[test]
    fn native_exit_import_terminates_without_returning_to_the_guest() {
        let mut image = hello_program();
        let old = nid("exit");
        let replacement = nid("_exit");
        let offset = image.windows(old.len()).position(|bytes| bytes == old.as_bytes()).unwrap();
        image[offset..offset + replacement.len()].copy_from_slice(replacement.as_bytes());
        let mut machine = Machine::load(&image).unwrap();
        assert_eq!(crate::nid::lookup("6Z83sYWFlA8"), Some("_exit"));
        assert_eq!(machine.run(10000).unwrap(), Stop::Exited(7));
        assert!(machine.unimplemented.is_empty());
    }

}
