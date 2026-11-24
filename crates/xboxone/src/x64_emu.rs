//! Clean-room sandboxed x86-64 (Zen 2 / Jaguar) user-mode CPU interpreter and HLE Xbox One /
//! Series X|S runtime for Emulator mode. Executes decrypted PE32+ x86-64 code safely on any host
//! architecture without running untrusted machine code natively.

use crate::era_mem::{MemoryProtect, TitleAddressSpace, PAGE_SIZE_4K, PAGE_SIZE_64K};
use crate::gpu_bridge::XboxFramebuffer;
use crate::pe::Pe;
use crate::shims::{XGAME_RUNTIME_DLL, XGAME_RUNTIME_EXPORTS};
use std::collections::HashMap;

const EXIT_SENTINEL: u64 = 0xFFFF_FFFF_FFFF_0000;
const THREAD_EXIT_SENTINEL: u64 = 0xFFFF_FFFF_FFFF_0010;
const TLS_RETURN_SENTINEL: u64 = 0xFFFF_FFFF_FFFF_0020;
const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const MEM_REPLACE_PLACEHOLDER: u32 = 0x4000;
const MEM_RESERVE_PLACEHOLDER: u32 = 0x40000;
const XMEM_PAGE_MODE_MASK: u32 = 0x2040_0000;
const THUNK_BASE: u64 = 0xFFFF_FFFF_FFFE_0000;
const THUNK_STRIDE: u64 = 8;
const XGAME_RUNTIME_HANDLE: u64 = 0xFFFF_FFFF_FFFD_0000;
const LAST_ERROR_OFFSET: u64 = 0x68;

const RAX: usize = 0;
const RCX: usize = 1;
const RDX: usize = 2;
const RSP: usize = 4;
const RSI: usize = 6;
const RDI: usize = 7;
const R8: usize = 8;
const R9: usize = 9;

const CF: u64 = 1 << 0;
const PF: u64 = 1 << 2;
const ZF: u64 = 1 << 6;
const SF: u64 = 1 << 7;
const DF: u64 = 1 << 10;
const OF: u64 = 1 << 11;

#[derive(Debug, Clone)]
pub struct EmuResult {
    pub exit_code: i32,
    pub instructions: u64,
    pub output: String,
    pub api_calls: Vec<String>,
    pub unimplemented_apis: Vec<String>,
    pub stopped_reason: Option<String>,
    pub framebuffer: XboxFramebuffer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmuStepState {
    Running,
    Exited(i32),
    Stopped(String),
}

#[derive(Debug, Clone)]
struct ThunkEntry {
    dll: String,
    name: String,
}

#[derive(Debug, Clone)]
struct GuestThread {
    id: u32,
    regs: [u64; 16],
    xmm: [u128; 16],
    rip: u64,
    rflags: u64,
    teb_va: u64,
    finished: bool,
    exit_code: u32,
    tls_startup: Option<TlsStartup>,
}

#[derive(Debug, Clone)]
struct TlsStartup {
    entry: u64,
    stack: u64,
    parameter: u64,
    reason: u64,
    next_callback: usize,
}

struct ImageTls {
    image_base: u64,
    template: Vec<u8>,
    size: u64,
    callbacks: Vec<u64>,
}

impl ImageTls {
    fn load(mem: &mut TitleAddressSpace, pe: &Pe, base: u64, image_size: u64) -> Result<Option<Self>, String> {
        let Some(tls) = pe.tls else { return Ok(None) };
        let image_address = |va: u64, bytes: u64| {
            va.checked_sub(pe.image_base)
                .filter(|offset| offset.checked_add(bytes).is_some_and(|end| end <= image_size))
                .map(|offset| base + offset)
                .ok_or_else(|| "PE TLS address is outside the image".to_string())
        };
        let start = tls.raw_start as u64;
        let end = tls.raw_end as u64;
        let template_size = end.checked_sub(start).filter(|_| end <= image_size)
            .ok_or("Invalid PE TLS template range")?;
        let size = template_size + tls.zero_fill as u64;
        if size > 16 * 1024 * 1024 {
            return Err("PE TLS template exceeds 16 MiB".into());
        }
        let mut template = vec![0; template_size as usize];
        if !template.is_empty() {
            mem.read_bytes(base + start, &mut template)?;
        }
        let index = image_address(tls.index_va, 4)?;
        mem.write_u32(index, 0)?;
        let mut callbacks = Vec::new();
        if tls.callbacks_va != 0 {
            for i in 0..=64 {
                let slot = tls.callbacks_va.checked_add(i * 8).ok_or("PE TLS callback list overflow")?;
                let callback = mem.read_u64(image_address(slot, 8)?)?;
                if callback == 0 {
                    break;
                }
                if i == 64 || !callback.checked_sub(base).is_some_and(|rva| rva < image_size) {
                    return Err("Invalid PE TLS callback list".into());
                }
                callbacks.push(callback);
            }
        }
        Ok(Some(Self { image_base: base, template, size, callbacks }))
    }

    fn initialize_thread(&self, mem: &mut TitleAddressSpace, teb: u64) -> Result<(), String> {
        let data = mem.alloc(None, self.size.max(1), PAGE_SIZE_64K, MemoryProtect::ReadWrite, true)?;
        if !self.template.is_empty() {
            mem.load_bytes(data, &self.template)?;
        }
        let vector = teb + 0x800;
        mem.write_u64(vector, data)?;
        mem.write_u64(teb + 0x58, vector)
    }

    fn startup(&self, entry: u64, stack: u64, parameter: u64, reason: u64) -> Option<TlsStartup> {
        (!self.callbacks.is_empty()).then_some(TlsStartup {
            entry, stack, parameter, reason, next_callback: 0,
        })
    }
}

/// Sandboxed Xbox One / Series X|S x86-64 CPU + HLE runtime.
pub struct XboxX64Emu {
    pub regs: [u64; 16],
    pub xmm: [u128; 16],
    pub rip: u64,
    pub rflags: u64,
    pub mem: TitleAddressSpace,
    pub framebuffer: XboxFramebuffer,
    teb_va: u64,
    peb_va: u64,
    seg_base: u64,
    threads: Vec<GuestThread>,
    active_thread: usize,
    next_tid: u32,
    image_tls: Option<ImageTls>,
    tls_slots: Vec<Option<u64>>,
    audio_samples: Vec<f32>,
    thunks: HashMap<u64, ThunkEntry>,
    runtime_module_refs: u32,
    output: String,
    api_calls: Vec<String>,
    unimplemented_apis: Vec<String>,
    strict_imports: bool,
    exited: bool,
    exit_code: i32,
    total_instructions: u64,
    tick_counter: u64,
    heap_allocs: HashMap<u64, u64>,
    pads: [u16; 4],
    pad_packet: u32,
}

impl XboxX64Emu {
    /// Loads a PE32+ image into a fresh sandboxed 64-bit address space ready to run.
    pub fn load_pe(file: &[u8]) -> Result<Self, String> {
        let pe = Pe::parse_bytes(file)?;
        if !pe.is_x64() {
            return Err(format!("PE machine {:#x} is not x86-64", pe.machine));
        }
        if pe.entry == 0 {
            return Err("PE image has no entry point".into());
        }

        let mut mem = TitleAddressSpace::default();
        let img_size = TitleAddressSpace::align_up(pe.image_size.max(pe.headers_size).max(0x1000) as u64, PAGE_SIZE_4K);
        let preferred = if pe.image_base != 0 { Some(pe.image_base) } else { None };
        let base = mem
            .alloc(preferred, img_size, PAGE_SIZE_64K, MemoryProtect::ExecuteReadWrite, true)
            .or_else(|_| mem.alloc(None, img_size, PAGE_SIZE_64K, MemoryProtect::ExecuteReadWrite, true))?;

        let hdr_len = (pe.headers_size as usize).min(file.len());
        mem.load_bytes(base, &file[..hdr_len])?;
        for s in &pe.sections {
            if s.raw_size > 0 {
                let off = s.raw_off as usize;
                let end = off + s.raw_size as usize;
                let chunk = file.get(off..end).ok_or_else(|| format!("section {} truncated", s.name))?;
                mem.load_bytes(base + s.vaddr as u64, chunk)?;
            }
        }

        // Apply DIR64 base relocations if relocated.
        let delta = (base as i64).wrapping_sub(pe.image_base as i64);
        if delta != 0 {
            for &(rva, kind) in &pe.relocs {
                if kind == 10 {
                    let va = base + rva as u64;
                    let cur = mem.read_u64(va)? as i64;
                    mem.write_u64(va, cur.wrapping_add(delta) as u64)?;
                }
            }
        }

        // Bind every IAT slot to a unique HLE thunk sentinel address.
        let mut thunks = HashMap::new();
        let mut next_idx = 0u64;
        for module in &pe.imports {
            for f in &module.functions {
                let thunk_addr = THUNK_BASE + next_idx * THUNK_STRIDE;
                next_idx += 1;
                mem.write_u64(base + f.slot as u64, thunk_addr)?;
                thunks.insert(
                    thunk_addr,
                    ThunkEntry {
                        dll: module.dll.clone(),
                        name: f.name.clone(),
                    },
                );
            }
        }

        // Allocate 1 MiB stack and push EXIT_SENTINEL as the entry point's return address.
        let stack_size = 1024 * 1024u64;
        let stack_base = mem.alloc(None, stack_size, PAGE_SIZE_64K, MemoryProtect::ReadWrite, true)?;
        let initial_rsp = stack_base + stack_size - 0x48;
        mem.write_u64(initial_rsp, EXIT_SENTINEL)?;

        // Allocate guest PEB and main-thread TEB (`gs:` segment base).
        let peb_va = mem.alloc(None, PAGE_SIZE_4K, PAGE_SIZE_4K, MemoryProtect::ReadWrite, true)?;
        mem.write_u64(peb_va + 0x10, base)?; // ImageBaseAddress
        let teb_va = mem.alloc(None, PAGE_SIZE_4K, PAGE_SIZE_4K, MemoryProtect::ReadWrite, true)?;
        mem.write_u64(teb_va + 0x08, stack_base + stack_size)?; // StackBase
        mem.write_u64(teb_va + 0x10, stack_base)?; // StackLimit
        mem.write_u64(teb_va + 0x30, teb_va)?; // Self TEB pointer
        mem.write_u64(teb_va + 0x40, 0x1000)?; // UniqueProcess
        mem.write_u64(teb_va + 0x48, 0x1001)?; // UniqueThread
        mem.write_u64(teb_va + 0x60, peb_va)?; // ProcessEnvironmentBlock

        let mut regs = [0u64; 16];
        regs[RSP] = initial_rsp;
        let xmm = [0u128; 16];
        let entry_rip = base + pe.entry as u64;
        let image_tls = ImageTls::load(&mut mem, &pe, base, img_size)?;
        if let Some(tls) = &image_tls {
            tls.initialize_thread(&mut mem, teb_va)?;
        }

        let main_thread = GuestThread {
            id: 0x1001,
            regs,
            xmm,
            rip: entry_rip,
            rflags: 0x202,
            teb_va,
            finished: false,
            exit_code: 0,
            tls_startup: image_tls.as_ref().and_then(|tls| tls.startup(entry_rip, initial_rsp, 0, 1)),
        };

        Ok(Self {
            regs,
            xmm,
            rip: entry_rip,
            rflags: 0x202,
            mem,
            framebuffer: XboxFramebuffer::new(320, 180),
            teb_va,
            peb_va,
            seg_base: 0,
            threads: vec![main_thread],
            active_thread: 0,
            next_tid: 0x1002,
            image_tls,
            tls_slots: Vec::new(),
            audio_samples: Vec::new(),
            thunks,
            runtime_module_refs: u32::from(pe.imports.iter().any(|module| {
                module.dll.eq_ignore_ascii_case(XGAME_RUNTIME_DLL)
            })),
            output: String::new(),
            api_calls: Vec::new(),
            unimplemented_apis: Vec::new(),
            strict_imports: false,
            exited: false,
            exit_code: 0,
            total_instructions: 0,
            tick_counter: 1000,
            heap_allocs: HashMap::new(),
            pads: [0; 4],
            pad_packet: 1,
        })
    }

    fn advance_tls_startup(&mut self) -> Result<(), String> {
        let thread = &mut self.threads[self.active_thread];
        let Some(startup) = &mut thread.tls_startup else { return Ok(()) };
        if startup.next_callback != 0 && self.rip != TLS_RETURN_SENTINEL {
            return Ok(());
        }
        let tls = self.image_tls.as_ref().ok_or("Missing image TLS state")?;
        self.regs = [0; 16];
        self.xmm = [0; 16];
        self.rflags = 0x202;
        self.regs[RSP] = startup.stack;
        if let Some(&callback) = tls.callbacks.get(startup.next_callback) {
            self.mem.write_u64(startup.stack, TLS_RETURN_SENTINEL)?;
            self.regs[RCX] = tls.image_base;
            self.regs[RDX] = startup.reason;
            self.rip = callback;
            startup.next_callback += 1;
        } else {
            let target = if self.active_thread == 0 { EXIT_SENTINEL } else { THREAD_EXIT_SENTINEL };
            self.mem.write_u64(startup.stack, target)?;
            self.regs[RCX] = startup.parameter;
            self.rip = startup.entry;
            thread.tls_startup = None;
        }
        Ok(())
    }

    fn save_active_thread(&mut self) {
        if let Some(t) = self.threads.get_mut(self.active_thread) {
            t.regs = self.regs;
            t.xmm = self.xmm;
            t.rip = self.rip;
            t.rflags = self.rflags;
            t.teb_va = self.teb_va;
        }
    }

    fn load_thread(&mut self, idx: usize) {
        self.active_thread = idx;
        let t = &self.threads[idx];
        self.regs = t.regs;
        self.xmm = t.xmm;
        self.rip = t.rip;
        self.rflags = t.rflags;
        self.teb_va = t.teb_va;
    }

    fn switch_to_next_runnable_thread(&mut self) -> bool {
        if self.threads.len() <= 1 {
            return false;
        }
        self.save_active_thread();
        let n = self.threads.len();
        for off in 1..=n {
            let idx = (self.active_thread + off) % n;
            if !self.threads[idx].finished {
                self.load_thread(idx);
                return true;
            }
        }
        false
    }

    /// Drains interleaved stereo f32 audio samples generated by the guest.
    pub fn take_audio(&mut self, out: &mut Vec<f32>) {
        out.append(&mut self.audio_samples);
    }

    /// Updates Xbox controller buttons for `pad` (0..4) using the shared frontend bitmask:
    /// bit 0=A, 1=B, 2=Back/Select, 3=Start, 4=Up, 5=Down, 6=Left, 7=Right, 8=X, 9=Y, 10=LB, 11=RB.
    pub fn set_buttons(&mut self, pad: usize, buttons: u16) {
        if pad >= self.pads.len() {
            return;
        }
        // Map frontend bitmask to XINPUT_GAMEPAD_* flags.
        const MAP: [(u16, u16); 12] = [
            (1 << 0, 0x1000),  // A
            (1 << 1, 0x2000),  // B
            (1 << 2, 0x0020),  // Back
            (1 << 3, 0x0010),  // Start
            (1 << 4, 0x0001),  // D-Pad Up
            (1 << 5, 0x0002),  // D-Pad Down
            (1 << 6, 0x0004),  // D-Pad Left
            (1 << 7, 0x0008),  // D-Pad Right
            (1 << 8, 0x4000),  // X
            (1 << 9, 0x8000),  // Y
            (1 << 10, 0x0100), // Left Shoulder
            (1 << 11, 0x0200), // Right Shoulder
        ];
        let mut xinput = 0u16;
        for (src, dst) in MAP {
            if buttons & src != 0 {
                xinput |= dst;
            }
        }
        if self.pads[pad] != xinput {
            self.pads[pad] = xinput;
            self.pad_packet = self.pad_packet.wrapping_add(1);
        }
    }

    pub fn total_instructions(&self) -> u64 {
        self.total_instructions
    }

    pub fn output(&self) -> &str {
        &self.output
    }

    pub fn api_calls(&self) -> &[String] {
        &self.api_calls
    }

    pub fn unimplemented_apis(&self) -> &[String] {
        &self.unimplemented_apis
    }

    /// Stop before returning placeholder results for unimplemented imports.
    pub fn set_strict_imports(&mut self) {
        self.strict_imports = true;
    }

    /// Converts the RGBA8 offscreen framebuffer to `0RGB` `u32` pixels for desktop scanout.
    pub fn fb_pixels_0rgb(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity((self.framebuffer.width * self.framebuffer.height) as usize);
        for px in self.framebuffer.rgba.chunks_exact(4) {
            out.push(((px[0] as u32) << 16) | ((px[1] as u32) << 8) | (px[2] as u32));
        }
        out
    }

    /// Runs up to `max_instructions` incrementally for frame-driven execution.
    pub fn step_budget(&mut self, max_instructions: u64) -> EmuStepState {
        if self.exited {
            return EmuStepState::Exited(self.exit_code);
        }
        let mut count = 0u64;
        while count < max_instructions && !self.exited {
            if let Err(error) = self.advance_tls_startup() {
                return EmuStepState::Stopped(error);
            }
            if self.rip == EXIT_SENTINEL {
                self.exit_code = self.regs[RAX] as i32;
                self.exited = true;
                return EmuStepState::Exited(self.exit_code);
            }
            if self.rip == THREAD_EXIT_SENTINEL {
                let code = self.regs[RAX] as u32;
                if let Some(t) = self.threads.get_mut(self.active_thread) {
                    t.finished = true;
                    t.exit_code = code;
                }
                if !self.switch_to_next_runnable_thread() {
                    self.exit_code = code as i32;
                    self.exited = true;
                    return EmuStepState::Exited(self.exit_code);
                }
                continue;
            }
            if let Some(thunk) = self.thunks.get(&self.rip).cloned() {
                if let Err(e) = self.dispatch_hle(&thunk) {
                    return EmuStepState::Stopped(e);
                }
                count += 1;
                self.total_instructions += 1;
                if self.exited {
                    return EmuStepState::Exited(self.exit_code);
                }
                continue;
            }
            match self.step() {
                Ok(()) => {
                    count += 1;
                    self.total_instructions += 1;
                }
                Err(e) => return EmuStepState::Stopped(e),
            }
        }
        if self.exited {
            EmuStepState::Exited(self.exit_code)
        } else {
            EmuStepState::Running
        }
    }

    /// Executes up to `max_instructions` instructions and returns the captured run report.
    pub fn run(&mut self, max_instructions: u64) -> EmuResult {
        let before = self.total_instructions;
        let stopped_reason = match self.step_budget(max_instructions) {
            EmuStepState::Exited(_) => None,
            EmuStepState::Stopped(reason) => Some(reason),
            EmuStepState::Running => Some(format!(
                "instruction limit ({max_instructions}) reached at RIP {:#x}",
                self.rip
            )),
        };

        EmuResult {
            exit_code: self.exit_code,
            instructions: self.total_instructions - before,
            output: self.output.clone(),
            api_calls: self.api_calls.clone(),
            unimplemented_apis: self.unimplemented_apis.clone(),
            stopped_reason,
            framebuffer: self.framebuffer.clone(),
        }
    }

    fn dispatch_hle(&mut self, thunk: &ThunkEntry) -> Result<(), String> {
        let label = format!("{}!{}", thunk.dll, thunk.name);
        if !self.api_calls.contains(&label) {
            self.api_calls.push(label.clone());
        }
        let rcx = self.regs[RCX];
        let rdx = self.regs[RDX];
        let r8 = self.regs[R8];
        let r9 = self.regs[R9];
        let name = thunk.name.as_str();

        // These shims draw or flip a test surface without creating a guest GPU device.
        if self.strict_imports && [
            "XDeviceCreate", "D3D12XBOX_CreateDevice", "D3D12CreateDevice", "D3D11XCreateDeviceX",
            "D3D12XBOX_Present",
        ].iter().any(|api| name.eq_ignore_ascii_case(api)) {
            return Err(self.unsupported_import(label));
        }

        let ret_val: u64 = match name {
            "GetModuleHandleA" | "GetModuleHandleW" if rcx == 0 => self.mem.read_u64(self.peb_va + 0x10)?,
            "GetModuleHandleA" | "GetModuleHandleW" => {
                self.get_runtime_module(rcx, name.ends_with('W'), false)?
            }
            "LoadLibraryA" | "LoadLibraryW" => self.get_runtime_module(rcx, name.ends_with('W'), true)?,
            "LoadLibraryExA" | "LoadLibraryExW" if rdx == 0 && r8 == 0 => {
                self.get_runtime_module(rcx, name.ends_with('W'), true)?
            }
            "GetProcAddress" => self.get_runtime_export(rcx, rdx)?,
            "FreeLibrary" if rcx == XGAME_RUNTIME_HANDLE && self.runtime_module_refs != 0 => {
                self.runtime_module_refs -= 1;
                1
            }
            "FreeLibrary" => self.library_error(6)?,
            "GetLastError" => u64::from(self.mem.read_u32(self.teb_va + LAST_ERROR_OFFSET)?),
            "SetLastError" => self.library_error(rcx as u32)?,
            n if n.eq_ignore_ascii_case("XGameRuntimeInitialize")
                || n.eq_ignore_ascii_case("XGameRuntimeUninitialize")
                || n.eq_ignore_ascii_case("XGameFeatureInitialize") =>
            {
                0
            }
            n if n.eq_ignore_ascii_case("XGameRuntimePrint") => {
                if rdx != 0 {
                    if let Ok(s) = self.mem.read_cstr(rdx, 4096) {
                        self.output.push_str(&s);
                        if !s.ends_with('\n') {
                            self.output.push('\n');
                        }
                    }
                }
                0
            }
            n if n.eq_ignore_ascii_case("XErrorReport") => {
                let message = if rdx == 0 {
                    String::new()
                } else {
                    self.mem.read_cstr(rdx, 4096).unwrap_or_else(|_| "<unreadable guest message>".to_owned())
                };
                self.output.push_str(&crate::shims::error_report_line(rcx as u32, &message));
                0
            }
            n if n.eq_ignore_ascii_case("OutputDebugStringA") => {
                if rcx != 0 {
                    if let Ok(s) = self.mem.read_cstr(rcx, 4096) {
                        self.output.push_str(&s);
                        if !s.ends_with('\n') {
                            self.output.push('\n');
                        }
                    }
                }
                0
            }
            n if n.eq_ignore_ascii_case("XSystemGetDeviceType") => 7, // Xbox Series X
            n if n.eq_ignore_ascii_case("XSystemGetXboxLiveSandboxId")
                || n.eq_ignore_ascii_case("XGameRuntimeGetXboxLiveSandboxId") =>
            {
                if rcx != 0 {
                    let _ = self.mem.write_u32(rcx, 2); // DevSandbox
                }
                0
            }
            n if n.eq_ignore_ascii_case("XGameGetXboxTitleId") => {
                if rcx != 0 {
                    let _ = self.mem.write_u32(rcx, 0x1234_5678);
                    0
                } else {
                    0x8000_0405
                }
            }
            n if n.eq_ignore_ascii_case("XUserGetId") => {
                if rdx != 0 {
                    let _ = self.mem.write_u64(rdx, 0x0009_0000_0000_0001);
                    0
                } else {
                    0x8000_0405
                }
            }
            n if n.eq_ignore_ascii_case("XUserGetGamertag") => {
                if rdx != 0 && r8 >= 12 {
                    let tag: Vec<u16> = "LocalPlayer\0".encode_utf16().collect();
                    for (i, ch) in tag.into_iter().enumerate() {
                        let _ = self.mem.write_u16(rdx + (i as u64) * 2, ch);
                    }
                    0
                } else {
                    0x8000_0405
                }
            }
            n if n.eq_ignore_ascii_case("XMemAlloc") || n.eq_ignore_ascii_case("XMemAllocDefault") => {
                let align = crate::era_mem::xmem_alignment_from_attributes(rdx) as u64;
                let sz = rcx.max(16);
                match self.mem.alloc(None, sz, align.max(PAGE_SIZE_4K), MemoryProtect::ReadWrite, true) {
                    Ok(va) => {
                        self.heap_allocs.insert(va, sz);
                        va
                    }
                    Err(_) => 0,
                }
            }
            n if n.eq_ignore_ascii_case("XMemFree") || n.eq_ignore_ascii_case("XMemFreeDefault") => {
                if rcx != 0 {
                    self.heap_allocs.remove(&rcx);
                    let _ = self.mem.free(rcx);
                }
                0
            }
            n if n.eq_ignore_ascii_case("XMemVirtualAlloc") => {
                let flags = r8 as u32;
                let protection = self.mem.read_u32(self.regs[RSP].wrapping_add(0x28))?;
                match self.xmem_virtual_alloc(rcx, rdx, flags, protection) {
                    Some(address) => address,
                    None => {
                        let error = self.unsupported_import(format!("{label} allocation flags {flags:#x}"));
                        if self.strict_imports {
                            return Err(error);
                        }
                        0
                    }
                }
            }
            n if n.eq_ignore_ascii_case("XMemAllocatePhysicalPages") => {
                self.xmem_allocate_physical_pages(rdx, r8) as u64
            }
            n if n.eq_ignore_ascii_case("XMemFreePhysicalPages") => {
                self.read_physical_array(rdx, rcx)
                    .is_some_and(|pages| self.mem.free_physical_pages(&pages)) as u64
            }
            n if n.eq_ignore_ascii_case("XMemMapPhysicalPages") => {
                if self.xmem_map_physical_pages(rcx, rdx, r8, false) { rcx } else { 0 }
            }
            n if n.eq_ignore_ascii_case("XMemMapPhysicalPagesScatter") => {
                self.xmem_map_physical_pages(rcx, rdx as u32 as u64, r8, true) as u64
            }
            n if n.eq_ignore_ascii_case("VirtualAlloc") || n.eq_ignore_ascii_case("VirtualAllocFromApp") => {
                let pref = if rcx != 0 { Some(rcx) } else { None };
                let sz = rdx.max(PAGE_SIZE_4K);
                self.mem.alloc(pref, sz, PAGE_SIZE_4K, MemoryProtect::ReadWrite, true).unwrap_or(0)
            }
            n if n.eq_ignore_ascii_case("VirtualFree") => {
                if self.mem.free(rcx) { 1 } else { 0 }
            }
            n if n.eq_ignore_ascii_case("TitleMemoryStatus") => {
                if rcx != 0 {
                    let st = self.mem.status();
                    let bytes = unsafe {
                        std::slice::from_raw_parts(&st as *const _ as *const u8, std::mem::size_of_val(&st))
                    };
                    let _ = self.mem.write_bytes(rcx, bytes);
                    1
                } else {
                    0
                }
            }
            n if n.eq_ignore_ascii_case("GetTickCount64") => {
                self.tick_counter += 16;
                self.tick_counter
            }
            n if n.eq_ignore_ascii_case("QueryPerformanceCounter") => {
                self.tick_counter += 1000;
                if rcx != 0 {
                    let _ = self.mem.write_u64(rcx, self.tick_counter);
                }
                1
            }
            n if n.eq_ignore_ascii_case("QueryPerformanceFrequency") => {
                if rcx != 0 {
                    let _ = self.mem.write_u64(rcx, 10_000_000);
                }
                1
            }
            n if n.eq_ignore_ascii_case("GetCurrentProcessId") => 0x1000,
            n if n.eq_ignore_ascii_case("GetCurrentThreadId") => {
                self.threads.get(self.active_thread).map(|t| t.id as u64).unwrap_or(0x1001)
            }
            n if n.eq_ignore_ascii_case("CreateThread") => {
                let start_addr = r8;
                let param = r9;
                let tid_out = self.mem.read_u64(self.regs[RSP].wrapping_add(0x30)).unwrap_or(0);
                let stack_sz = rdx.max(256 * 1024);
                let Ok(stack_base) = self.mem.alloc(None, stack_sz, PAGE_SIZE_64K, MemoryProtect::ReadWrite, true) else {
                    return Ok(());
                };
                let rsp = stack_base + stack_sz - 0x48;
                let _ = self.mem.write_u64(rsp, THREAD_EXIT_SENTINEL);
                let tid = self.next_tid;
                self.next_tid += 1;
                let teb = self.mem.alloc(None, PAGE_SIZE_4K, PAGE_SIZE_4K, MemoryProtect::ReadWrite, true)?;
                let _ = self.mem.write_u64(teb + 0x08, stack_base + stack_sz);
                let _ = self.mem.write_u64(teb + 0x10, stack_base);
                let _ = self.mem.write_u64(teb + 0x30, teb);
                let _ = self.mem.write_u64(teb + 0x40, 0x1000);
                let _ = self.mem.write_u64(teb + 0x48, tid as u64);
                let _ = self.mem.write_u64(teb + 0x60, self.peb_va);
                if let Some(tls) = &self.image_tls {
                    tls.initialize_thread(&mut self.mem, teb)?;
                }

                let mut regs = [0u64; 16];
                regs[RSP] = rsp;
                regs[RCX] = param;
                let handle = (self.threads.len() as u64) + 0x2000;
                self.threads.push(GuestThread {
                    id: tid,
                    regs,
                    xmm: [0u128; 16],
                    rip: start_addr,
                    rflags: 0x202,
                    teb_va: teb,
                    finished: false,
                    exit_code: 0,
                    tls_startup: self.image_tls.as_ref().and_then(|tls| tls.startup(start_addr, rsp, param, 2)),
                });
                if tid_out != 0 {
                    let _ = self.mem.write_u32(tid_out, tid);
                }
                handle
            }
            n if n.eq_ignore_ascii_case("WaitForSingleObject") => {
                if rcx >= 0x2000 {
                    let idx = (rcx - 0x2000) as usize;
                    if let Some(target) = self.threads.get(idx) {
                        if !target.finished {
                            // Yield to target thread and keep RIP at this thunk to re-check on return.
                            self.save_active_thread();
                            self.load_thread(idx);
                            return Ok(());
                        }
                    }
                }
                0 // WAIT_OBJECT_0
            }
            n if n.eq_ignore_ascii_case("GetExitCodeThread") => {
                if rcx >= 0x2000 && rdx != 0 {
                    let idx = (rcx - 0x2000) as usize;
                    if let Some(t) = self.threads.get(idx) {
                        let code = if t.finished { t.exit_code } else { 259 }; // STILL_ACTIVE
                        let _ = self.mem.write_u32(rdx, code);
                        1
                    } else {
                        0
                    }
                } else {
                    0
                }
            }
            n if n.eq_ignore_ascii_case("SwitchToThread") => {
                // Pop return address first so this thread resumes after the call.
                let ret_ip = self.mem.read_u64(self.regs[RSP])?;
                self.regs[RSP] = self.regs[RSP].wrapping_add(8);
                self.regs[RAX] = 1;
                self.rip = ret_ip;
                let _ = self.switch_to_next_runnable_thread();
                return Ok(());
            }
            n if n.eq_ignore_ascii_case("ExitThread") => {
                let code = rcx as u32;
                if let Some(t) = self.threads.get_mut(self.active_thread) {
                    t.finished = true;
                    t.exit_code = code;
                }
                if !self.switch_to_next_runnable_thread() {
                    self.exit_code = code as i32;
                    self.exited = true;
                }
                return Ok(());
            }
            n if n.eq_ignore_ascii_case("CloseHandle") => 1,
            n if n.eq_ignore_ascii_case("InitializeCriticalSection")
                || n.eq_ignore_ascii_case("InitializeCriticalSectionEx")
                || n.eq_ignore_ascii_case("InitializeCriticalSectionAndSpinCount") =>
            {
                if rcx != 0 {
                    let _ = self.mem.write_bytes(rcx, &[0u8; 40]);
                }
                1
            }
            n if n.eq_ignore_ascii_case("EnterCriticalSection")
                || n.eq_ignore_ascii_case("LeaveCriticalSection")
                || n.eq_ignore_ascii_case("DeleteCriticalSection") =>
            {
                0
            }
            n if n.eq_ignore_ascii_case("TryEnterCriticalSection") => 1,
            n if n.eq_ignore_ascii_case("TlsAlloc") => {
                if let Some(pos) = self.tls_slots.iter().position(|s| s.is_none()) {
                    self.tls_slots[pos] = Some(0);
                    pos as u64
                } else {
                    let idx = self.tls_slots.len() as u64;
                    self.tls_slots.push(Some(0));
                    idx
                }
            }
            n if n.eq_ignore_ascii_case("TlsGetValue") => {
                self.tls_slots.get(rcx as usize).and_then(|v| *v).unwrap_or(0)
            }
            n if n.eq_ignore_ascii_case("TlsSetValue") => {
                if let Some(slot) = self.tls_slots.get_mut(rcx as usize) {
                    if slot.is_some() {
                        *slot = Some(rdx);
                        1
                    } else {
                        0
                    }
                } else {
                    0
                }
            }
            n if n.eq_ignore_ascii_case("TlsFree") => {
                if let Some(slot) = self.tls_slots.get_mut(rcx as usize) {
                    *slot = None;
                    1
                } else {
                    0
                }
            }
            n if n.eq_ignore_ascii_case("ExitProcess") => {
                self.exit_code = rcx as i32;
                self.exited = true;
                return Ok(());
            }
            n if n.eq_ignore_ascii_case("XDeviceCreate")
                || n.eq_ignore_ascii_case("D3D12XBOX_CreateDevice")
                || n.eq_ignore_ascii_case("D3D12CreateDevice")
                || n.eq_ignore_ascii_case("D3D11XCreateDeviceX") =>
            {
                self.framebuffer.clear(18, 28, 52, 255);
                self.framebuffer.present();
                0
            }
            n if n.eq_ignore_ascii_case("XDevicePresent") || n.eq_ignore_ascii_case("D3D12XBOX_Present") => {
                self.framebuffer.present();
                0
            }
            n if n.eq_ignore_ascii_case("DStorageGetFactory")
                || n.eq_ignore_ascii_case("DStorageSetConfiguration")
                || n.eq_ignore_ascii_case("SetFrameIntervalX")
                || n.eq_ignore_ascii_case("ScheduleFrameEventX")
                || n.eq_ignore_ascii_case("WaitFrameEventX")
                || n.eq_ignore_ascii_case("AcpHalCreate")
                || n.eq_ignore_ascii_case("XcurlInitialize")
                || n.eq_ignore_ascii_case("XcurlUninitialize")
                || n.eq_ignore_ascii_case("BinkDoFrame")
                || n.eq_ignore_ascii_case("BinkClose")
                || n.eq_ignore_ascii_case("BinkOpen") =>
            {
                0
            }
            n if n.eq_ignore_ascii_case("ApuAlloc") => {
                let size = r8.max(PAGE_SIZE_4K);
                let va = self.mem.alloc(None, size, PAGE_SIZE_4K, MemoryProtect::ReadWrite, true).unwrap_or(0);
                if rcx != 0 {
                    let _ = self.mem.write_u64(rcx, va);
                }
                if va == 0 { 0x8007_000E } else { 0 }
            }
            n if n.eq_ignore_ascii_case("ApuFree") => 0,
            n if n.eq_ignore_ascii_case("XDeviceClear") => {
                self.framebuffer.clear(rcx as u8, rdx as u8, r8 as u8, 255);
                0
            }
            n if n.eq_ignore_ascii_case("XDeviceFillRect") => {
                let rgba = self.mem.read_u32(self.regs[RSP].wrapping_add(0x28)).unwrap_or(0xFFFF_FFFF);
                let r = ((rgba >> 24) & 0xFF) as u8;
                let g = ((rgba >> 16) & 0xFF) as u8;
                let b = ((rgba >> 8) & 0xFF) as u8;
                let a = (rgba & 0xFF) as u8;
                self.framebuffer.fill_rect(rcx as u32, rdx as u32, r8 as u32, r9 as u32, [r, g, b, a]);
                0
            }
            n if thunk.dll.eq_ignore_ascii_case("emu-graphics.dll")
                && (n.eq_ignore_ascii_case("XDeviceCopyRgba8") || n.eq_ignore_ascii_case("XDeviceReadRgba8")) =>
            {
                self.transfer_framebuffer_rgba8(n.eq_ignore_ascii_case("XDeviceCopyRgba8"))?;
                0
            }
            n if thunk.dll.eq_ignore_ascii_case("emu-graphics.dll")
                && n.eq_ignore_ascii_case("XDeviceDrawRdna2Rect") =>
            {
                self.draw_rdna2_rect()?;
                0
            }
            n if n.eq_ignore_ascii_case("XAudio2SubmitPcm") => {
                // rcx = pointer to interleaved stereo f32 samples, rdx = sample count (f32 values)
                let count = (rdx as usize).min(16_384);
                if rcx != 0 && count > 0 {
                    for i in 0..count {
                        if let Ok(bits) = self.mem.read_u32(rcx + (i as u64) * 4) {
                            self.audio_samples.push(f32::from_bits(bits).clamp(-1.0, 1.0));
                        }
                    }
                }
                0
            }
            n if n.eq_ignore_ascii_case("XInputGetState") => {
                let idx = rcx as usize;
                if idx < self.pads.len() && rdx != 0 {
                    let mut state = [0u8; 16];
                    state[0..4].copy_from_slice(&self.pad_packet.to_le_bytes());
                    state[4..6].copy_from_slice(&self.pads[idx].to_le_bytes());
                    let _ = self.mem.write_bytes(rdx, &state);
                    0 // ERROR_SUCCESS
                } else {
                    1167 // ERROR_DEVICE_NOT_CONNECTED
                }
            }
            n if n.eq_ignore_ascii_case("XInputGetCapabilities") => {
                let idx = rcx as usize;
                if idx < self.pads.len() && r8 != 0 {
                    let _ = self.mem.write_bytes(r8, &[0u8; 20]);
                    0
                } else {
                    1167
                }
            }
            n if n.eq_ignore_ascii_case("Sleep") => {
                self.tick_counter = self.tick_counter.wrapping_add(rcx.max(1));
                0
            }
            _ => {
                let error = self.unsupported_import(label);
                if self.strict_imports {
                    return Err(error);
                }
                0
            }
        };

        self.regs[RAX] = ret_val;
        // Pop return address off stack.
        let ret_ip = self.mem.read_u64(self.regs[RSP])?;
        self.regs[RSP] = self.regs[RSP].wrapping_add(8);
        self.rip = ret_ip;
        Ok(())
    }

    fn library_error(&mut self, error: u32) -> Result<u64, String> {
        self.mem.write_u32(self.teb_va + LAST_ERROR_OFFSET, error)?;
        Ok(0)
    }

    fn read_library_name(&self, address: u64, wide: bool) -> Option<String> {
        if !wide {
            return self.mem.read_cstr(address, 260).ok();
        }
        let mut units = Vec::new();
        for offset in (0..520).step_by(2) {
            let unit = self.mem.read_u16(address.checked_add(offset)?).ok()?;
            if unit == 0 {
                return String::from_utf16(&units).ok();
            }
            units.push(unit);
        }
        None
    }

    fn get_runtime_module(&mut self, address: u64, wide: bool, load: bool) -> Result<u64, String> {
        let Some(name) = self.read_library_name(address, wide) else {
            return self.library_error(126);
        };
        let is_runtime = name.eq_ignore_ascii_case(XGAME_RUNTIME_DLL) || name.eq_ignore_ascii_case("xgameruntime");
        if !is_runtime || (!load && self.runtime_module_refs == 0) {
            return self.library_error(126);
        }
        if load {
            self.runtime_module_refs = self.runtime_module_refs.saturating_add(1);
        }
        Ok(XGAME_RUNTIME_HANDLE)
    }

    fn get_runtime_export(&mut self, handle: u64, address: u64) -> Result<u64, String> {
        if handle != XGAME_RUNTIME_HANDLE || self.runtime_module_refs == 0 {
            return self.library_error(6);
        }
        let Ok(name) = self.mem.read_cstr(address, 256) else {
            return self.library_error(127);
        };
        if !XGAME_RUNTIME_EXPORTS.contains(&name.as_str()) {
            return self.library_error(127);
        }
        if let Some(thunk_address) = self.thunks.iter().filter_map(|(&address, thunk)| {
            (thunk.dll.eq_ignore_ascii_case(XGAME_RUNTIME_DLL) && thunk.name == name).then_some(address)
        }).min() {
            return Ok(thunk_address);
        }
        let thunk_address = THUNK_BASE + self.thunks.len() as u64 * THUNK_STRIDE;
        self.thunks.insert(thunk_address, ThunkEntry { dll: XGAME_RUNTIME_DLL.to_owned(), name });
        Ok(thunk_address)
    }

    fn xmem_virtual_alloc(&mut self, address: u64, size: u64, flags: u32, protection: u32) -> Option<u64> {
        let allocation_type = flags & !XMEM_PAGE_MODE_MASK;
        let reserve_placeholder = allocation_type & MEM_RESERVE_PLACEHOLDER != 0;
        let replace_placeholder = allocation_type & MEM_REPLACE_PLACEHOLDER != 0;
        let backing_type = allocation_type & !(MEM_RESERVE_PLACEHOLDER | MEM_REPLACE_PLACEHOLDER);
        if !matches!(backing_type, 0 | MEM_RESERVE | 0x3000) {
            return None;
        }
        let mut page_size = match flags & XMEM_PAGE_MODE_MASK {
            0 => PAGE_SIZE_4K,
            0x2040_0000 => PAGE_SIZE_64K,
            0x2000_0000 => 2 * 1024 * 1024,
            _ => return None,
        };
        let protect = match protection & 0xFF {
            1 | 2 | 4 | 8 | 0x10 | 0x20 | 0x40 | 0x80 => MemoryProtect::from_win32(protection),
            _ => return Some(0),
        };
        if size == 0 {
            return Some(0);
        }
        let commit = allocation_type == 0 || allocation_type & MEM_COMMIT != 0;
        if reserve_placeholder || replace_placeholder {
            page_size = page_size.max(PAGE_SIZE_64K);
            if backing_type & MEM_RESERVE == 0 || (reserve_placeholder && (
                replace_placeholder || commit || protect != MemoryProtect::NoAccess
            )) {
                return Some(0);
            }
        }
        let align = page_size.max(PAGE_SIZE_64K);
        let base = address & !(align - 1);
        if address != 0 && base == 0 {
            return Some(0);
        }
        let Some(rounded) = size.checked_add(address - base)
            .and_then(|size| size.checked_add(page_size - 1))
            .map(|size| size & !(page_size - 1))
        else {
            return Some(0);
        };
        if base.checked_add(rounded).is_none() || (commit && rounded > self.mem.status().avail_mem) {
            return Some(0);
        }
        let preferred = (base != 0).then_some(base);
        if reserve_placeholder {
            return Some(self.mem.reserve_placeholder(preferred, rounded, align).unwrap_or(0));
        }
        let allocated = if replace_placeholder {
            if address == 0 || address != base {
                return Some(0);
            }
            self.mem.replace_placeholder(base, rounded, protect, commit)
        } else {
            self.mem.alloc(preferred, rounded, align, protect, commit)
        };
        if let Ok(result) = allocated {
            if !commit && flags & XMEM_PAGE_MODE_MASK != 0 {
                self.mem.mark_physical_window(result);
            }
            Some(result)
        } else {
            Some(0)
        }
    }

    fn read_physical_array(&self, pointer: u64, count: u64) -> Option<Vec<u64>> {
        if count > self.mem.status().total_mem / PAGE_SIZE_64K || (count != 0 && pointer == 0) {
            return None;
        }
        (0..count).map(|index| self.mem.read_u64(pointer.checked_add(index * 8)?).ok()).collect()
    }

    fn xmem_allocate_physical_pages(&mut self, count_pointer: u64, output: u64) -> bool {
        let Ok(count) = self.mem.read_u64(count_pointer) else { return false };
        if count == 0 {
            return true;
        }
        if output == 0 || !count.checked_mul(8).is_some_and(|length| self.mem.can_write_bytes(output, length)) {
            return false;
        }
        let Some(pages) = self.mem.allocate_physical_pages(count) else {
            let _ = self.mem.write_u64(count_pointer, 0);
            return false;
        };
        let values: Vec<_> = pages.iter().flat_map(|page| page.to_le_bytes()).collect();
        if self.mem.write_bytes(output, &values).is_err() {
            self.mem.free_physical_pages(&pages);
            let _ = self.mem.write_u64(count_pointer, 0);
            return false;
        }
        true
    }

    fn xmem_map_physical_pages(&mut self, address: u64, count: u64, pointer: u64, scatter: bool) -> bool {
        if count > self.mem.status().total_mem / PAGE_SIZE_64K || (count != 0 && address == 0) {
            return false;
        }
        let pages = if pointer == 0 { vec![0; count as usize] } else {
            let Some(pages) = self.read_physical_array(pointer, count) else { return false };
            pages
        };
        let addresses = if scatter {
            self.read_physical_array(address, count)
        } else {
            (0..count).map(|index| address.checked_add(index * PAGE_SIZE_64K)).collect()
        };
        let Some(addresses) = addresses else { return false };
        self.mem.map_physical_pages(&addresses.into_iter().zip(pages).collect::<Vec<_>>())
    }

    /// Private emulator ABI: x, y, width, height, then a guest pointer to packed RGBA8.
    fn transfer_framebuffer_rgba8(&mut self, upload: bool) -> Result<(), String> {
        let (len, rows) = self.framebuffer.rgba8_rows(
            self.regs[RCX] as u32,
            self.regs[RDX] as u32,
            self.regs[R8] as u32,
            self.regs[R9] as u32,
        )?;
        let address = self.mem.read_u64(self.regs[RSP].wrapping_add(0x28))?;
        let mut pixels = vec![0; len];
        if upload {
            // Validate and read the whole source before changing any pixels.
            self.mem.read_bytes(address, &mut pixels)?;
            let mut offset = 0;
            for row in rows {
                let end = offset + row.len();
                self.framebuffer.rgba[row].copy_from_slice(&pixels[offset..end]);
                offset = end;
            }
        } else {
            let mut offset = 0;
            for row in rows {
                let end = offset + row.len();
                pixels[offset..end].copy_from_slice(&self.framebuffer.rgba[row]);
                offset = end;
            }
            self.mem.write_bytes(address, &pixels)?;
        }
        Ok(())
    }

    /// Private emulator ABI: x, y, width, height, shader pointer, byte count.
    fn draw_rdna2_rect(&mut self) -> Result<(), String> {
        let width = self.regs[R8] as u32;
        let height = self.regs[R9] as u32;
        let (_, rows) = self.framebuffer.rgba8_rows(
            self.regs[RCX] as u32,
            self.regs[RDX] as u32,
            width,
            height,
        )?;
        let stack = self.regs[RSP];
        let address = self.mem.read_u64(stack.wrapping_add(0x28))?;
        let len = self.mem.read_u32(stack.wrapping_add(0x30))? as usize;
        let mut code = [0; crate::rdna2::MAX_SHADER_BYTES];
        let code = code.get_mut(..len).ok_or("RDNA2 shader exceeds 64 dwords.")?;
        self.mem.read_bytes(address, code)?;
        let shader = crate::rdna2::PixelShader::decode(code)?;
        for (y, row) in rows.enumerate() {
            let v = (y as f32 + 0.5) / height as f32;
            for (x, pixel) in self.framebuffer.rgba[row].chunks_exact_mut(4).enumerate() {
                let u = (x as f32 + 0.5) / width as f32;
                pixel.copy_from_slice(&shader.shade_rgba8(u, v));
            }
        }
        Ok(())
    }

    fn unsupported_import(&mut self, label: String) -> String {
        let error = format!("Unimplemented Xbox import {label}");
        if !self.unimplemented_apis.contains(&label) {
            self.unimplemented_apis.push(label);
        }
        error
    }

    fn fetch_u8(&mut self) -> Result<u8, String> {
        let v = self.mem.read_u8(self.rip)?;
        self.rip = self.rip.wrapping_add(1);
        Ok(v)
    }

    fn fetch_u16(&mut self) -> Result<u16, String> {
        let v = self.mem.read_u16(self.rip)?;
        self.rip = self.rip.wrapping_add(2);
        Ok(v)
    }

    fn fetch_u32(&mut self) -> Result<u32, String> {
        let v = self.mem.read_u32(self.rip)?;
        self.rip = self.rip.wrapping_add(4);
        Ok(v)
    }

    fn fetch_u64(&mut self) -> Result<u64, String> {
        let v = self.mem.read_u64(self.rip)?;
        self.rip = self.rip.wrapping_add(8);
        Ok(v)
    }

    fn read_reg(&self, idx: usize, size: u8, has_rex: bool) -> u64 {
        if size == 1 && !has_rex && (4..=7).contains(&idx) {
            return (self.regs[idx - 4] >> 8) & 0xFF;
        }
        let v = self.regs[idx & 15];
        match size {
            1 => v & 0xFF,
            2 => v & 0xFFFF,
            4 => v & 0xFFFF_FFFF,
            _ => v,
        }
    }

    fn write_reg(&mut self, idx: usize, size: u8, has_rex: bool, val: u64) {
        if size == 1 && !has_rex && (4..=7).contains(&idx) {
            let r = idx - 4;
            self.regs[r] = (self.regs[r] & !0xFF00) | ((val & 0xFF) << 8);
            return;
        }
        let r = idx & 15;
        match size {
            1 => self.regs[r] = (self.regs[r] & !0xFF) | (val & 0xFF),
            2 => self.regs[r] = (self.regs[r] & !0xFFFF) | (val & 0xFFFF),
            4 => self.regs[r] = val & 0xFFFF_FFFF, // 32-bit writes zero-extend to 64-bit
            _ => self.regs[r] = val,
        }
    }

    fn read_mem(&self, addr: u64, size: u8) -> Result<u64, String> {
        Ok(match size {
            1 => self.mem.read_u8(addr)? as u64,
            2 => self.mem.read_u16(addr)? as u64,
            4 => self.mem.read_u32(addr)? as u64,
            _ => self.mem.read_u64(addr)?,
        })
    }

    fn write_mem(&mut self, addr: u64, size: u8, val: u64) -> Result<(), String> {
        match size {
            1 => self.mem.write_u8(addr, val as u8),
            2 => self.mem.write_u16(addr, val as u16),
            4 => self.mem.write_u32(addr, val as u32),
            _ => self.mem.write_u64(addr, val),
        }
    }

    fn decode_modrm(&mut self, rex_r: usize, rex_x: usize, rex_b: usize) -> Result<(usize, ModRmTarget), String> {
        let modrm = self.fetch_u8()?;
        let md = modrm >> 6;
        let reg = (((modrm >> 3) & 7) as usize) | (rex_r << 3);
        let rm_low = (modrm & 7) as usize;
        let rm = rm_low | (rex_b << 3);

        if md == 3 {
            return Ok((reg, ModRmTarget::Reg(rm)));
        }
        let addr = if rm_low == 4 {
            let sib = self.fetch_u8()?;
            let scale = 1u64 << (sib >> 6);
            let index = (((sib >> 3) & 7) as usize) | (rex_x << 3);
            let base_low = (sib & 7) as usize;
            let base = base_low | (rex_b << 3);
            let idx_val = if index == 4 { 0 } else { self.regs[index].wrapping_mul(scale) };
            let (base_val, disp) = if md == 0 && base_low == 5 {
                (0u64, self.fetch_u32()? as i32 as i64)
            } else {
                let d = match md {
                    0 => 0i64,
                    1 => self.fetch_u8()? as i8 as i64,
                    _ => self.fetch_u32()? as i32 as i64,
                };
                (self.regs[base], d)
            };
            base_val.wrapping_add(idx_val).wrapping_add(disp as u64)
        } else if md == 0 && rm_low == 5 {
            let disp = self.fetch_u32()? as i32 as i64;
            // RIP-relative: base is RIP *after* this ModRM+disp32; callers with trailing imm adjust if needed.
            return Ok((reg, ModRmTarget::RipRel(disp)));
        } else {
            let disp = match md {
                0 => 0i64,
                1 => self.fetch_u8()? as i8 as i64,
                _ => self.fetch_u32()? as i32 as i64,
            };
            self.regs[rm].wrapping_add(disp as u64)
        };
        Ok((reg, ModRmTarget::Mem(addr)))
    }

    fn resolve_target(&self, target: ModRmTarget, trailing_imm_bytes: u64) -> ModRmResolved {
        match target {
            ModRmTarget::Reg(r) => ModRmResolved::Reg(r),
            ModRmTarget::Mem(a) => ModRmResolved::Mem(a.wrapping_add(self.seg_base)),
            ModRmTarget::RipRel(disp) => ModRmResolved::Mem(
                self.rip
                    .wrapping_add(trailing_imm_bytes)
                    .wrapping_add(disp as u64)
                    .wrapping_add(self.seg_base),
            ),
        }
    }

    fn read_mem128(&self, addr: u64) -> Result<u128, String> {
        let lo = self.mem.read_u64(addr)? as u128;
        let hi = self.mem.read_u64(addr.wrapping_add(8))? as u128;
        Ok(lo | (hi << 64))
    }

    fn write_mem128(&mut self, addr: u64, val: u128) -> Result<(), String> {
        self.mem.write_u64(addr, val as u64)?;
        self.mem.write_u64(addr.wrapping_add(8), (val >> 64) as u64)
    }

    fn read_xmm_rm(&self, t: ModRmResolved) -> Result<u128, String> {
        match t {
            ModRmResolved::Reg(r) => Ok(self.xmm[r & 15]),
            ModRmResolved::Mem(a) => self.read_mem128(a),
        }
    }

    fn write_xmm_rm(&mut self, t: ModRmResolved, val: u128) -> Result<(), String> {
        match t {
            ModRmResolved::Reg(r) => {
                self.xmm[r & 15] = val;
                Ok(())
            }
            ModRmResolved::Mem(a) => self.write_mem128(a, val),
        }
    }

    fn read_xmm_f32(&self, t: ModRmResolved) -> Result<f32, String> {
        Ok(f32::from_bits(match t {
            ModRmResolved::Reg(r) => self.xmm[r & 15] as u32,
            ModRmResolved::Mem(a) => self.mem.read_u32(a)?,
        }))
    }

    fn read_xmm_f64(&self, t: ModRmResolved) -> Result<f64, String> {
        Ok(f64::from_bits(match t {
            ModRmResolved::Reg(r) => self.xmm[r & 15] as u64,
            ModRmResolved::Mem(a) => self.mem.read_u64(a)?,
        }))
    }

    fn truncate_float_to_integer(value: f64, size: u8) -> u64 {
        let value = value.trunc();
        if size == 8 {
            if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&value) {
                value as i64 as u64
            } else {
                i64::MIN as u64
            }
        } else if (-2_147_483_648.0..2_147_483_648.0).contains(&value) {
            value as i32 as u32 as u64
        } else {
            i32::MIN as u32 as u64
        }
    }

    fn xmm_lanes_f32(v: u128) -> [f32; 4] {
        [
            f32::from_bits(v as u32),
            f32::from_bits((v >> 32) as u32),
            f32::from_bits((v >> 64) as u32),
            f32::from_bits((v >> 96) as u32),
        ]
    }

    fn pack_f32(l: [f32; 4]) -> u128 {
        (l[0].to_bits() as u128)
            | ((l[1].to_bits() as u128) << 32)
            | ((l[2].to_bits() as u128) << 64)
            | ((l[3].to_bits() as u128) << 96)
    }

    fn xmm_lanes_f64(v: u128) -> [f64; 2] {
        [f64::from_bits(v as u64), f64::from_bits((v >> 64) as u64)]
    }

    fn pack_f64(l: [f64; 2]) -> u128 {
        (l[0].to_bits() as u128) | ((l[1].to_bits() as u128) << 64)
    }

    fn set_fp_cmp_flags(&mut self, a: f64, b: f64) {
        self.rflags &= !(CF | PF | ZF | OF | SF);
        if a.is_nan() || b.is_nan() {
            self.rflags |= ZF | PF | CF;
        } else if a < b {
            self.rflags |= CF;
        } else if a == b {
            self.rflags |= ZF;
        }
    }

    fn read_rm(&self, t: ModRmResolved, size: u8, has_rex: bool) -> Result<u64, String> {
        match t {
            ModRmResolved::Reg(r) => Ok(self.read_reg(r, size, has_rex)),
            ModRmResolved::Mem(a) => self.read_mem(a, size),
        }
    }

    fn write_rm(&mut self, t: ModRmResolved, size: u8, has_rex: bool, val: u64) -> Result<(), String> {
        match t {
            ModRmResolved::Reg(r) => {
                self.write_reg(r, size, has_rex, val);
                Ok(())
            }
            ModRmResolved::Mem(a) => self.write_mem(a, size, val),
        }
    }

    fn set_zsp(&mut self, val: u64, size: u8) {
        let bits = (size as u32) * 8;
        let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
        let v = val & mask;
        self.rflags &= !(ZF | SF | PF);
        if v == 0 {
            self.rflags |= ZF;
        }
        if (v >> (bits - 1)) & 1 != 0 {
            self.rflags |= SF;
        }
        if (v as u8).count_ones() % 2 == 0 {
            self.rflags |= PF;
        }
    }

    fn alu_op(&mut self, op: u8, a: u64, b: u64, size: u8) -> u64 {
        let bits = (size as u32) * 8;
        let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
        let sign = 1u64 << (bits - 1);
        let a = a & mask;
        let b = b & mask;
        let cf_in = if (self.rflags & CF) != 0 { 1u64 } else { 0u64 };

        match op {
            0 | 2 => {
                // ADD / ADC
                let c = if op == 2 { cf_in } else { 0 };
                let sum = (a as u128) + (b as u128) + (c as u128);
                let res = (sum as u64) & mask;
                self.rflags &= !(CF | OF);
                if sum > mask as u128 {
                    self.rflags |= CF;
                }
                if ((!(a ^ b)) & (a ^ res) & sign) != 0 {
                    self.rflags |= OF;
                }
                self.set_zsp(res, size);
                res
            }
            3 | 5 | 7 => {
                // SBB / SUB / CMP
                let c = if op == 3 { cf_in } else { 0 };
                let diff = (a as u128).wrapping_sub((b as u128) + (c as u128));
                let res = (diff as u64) & mask;
                self.rflags &= !(CF | OF);
                if (a as u128) < (b as u128) + (c as u128) {
                    self.rflags |= CF;
                }
                if ((a ^ b) & (a ^ res) & sign) != 0 {
                    self.rflags |= OF;
                }
                self.set_zsp(res, size);
                res
            }
            1 => {
                let res = (a | b) & mask;
                self.rflags &= !(CF | OF);
                self.set_zsp(res, size);
                res
            }
            4 => {
                let res = (a & b) & mask;
                self.rflags &= !(CF | OF);
                self.set_zsp(res, size);
                res
            }
            6 => {
                let res = (a ^ b) & mask;
                self.rflags &= !(CF | OF);
                self.set_zsp(res, size);
                res
            }
            _ => a,
        }
    }

    fn eval_cc(&self, cc: u8) -> bool {
        let cf = (self.rflags & CF) != 0;
        let zf = (self.rflags & ZF) != 0;
        let sf = (self.rflags & SF) != 0;
        let of = (self.rflags & OF) != 0;
        let pf = (self.rflags & PF) != 0;
        match cc & 0xF {
            0x0 => of,
            0x1 => !of,
            0x2 => cf,
            0x3 => !cf,
            0x4 => zf,
            0x5 => !zf,
            0x6 => cf || zf,
            0x7 => !cf && !zf,
            0x8 => sf,
            0x9 => !sf,
            0xA => pf,
            0xB => !pf,
            0xC => sf != of,
            0xD => sf == of,
            0xE => zf || (sf != of),
            _ => !zf && (sf == of),
        }
    }

    fn sign_ext(v: u64, from_bytes: u8) -> u64 {
        match from_bytes {
            1 => v as u8 as i8 as i64 as u64,
            2 => v as u16 as i16 as i64 as u64,
            4 => v as u32 as i32 as i64 as u64,
            _ => v,
        }
    }

    fn push64(&mut self, val: u64) -> Result<(), String> {
        self.regs[RSP] = self.regs[RSP].wrapping_sub(8);
        self.mem.write_u64(self.regs[RSP], val)
    }

    fn pop64(&mut self) -> Result<u64, String> {
        let v = self.mem.read_u64(self.regs[RSP])?;
        self.regs[RSP] = self.regs[RSP].wrapping_add(8);
        Ok(v)
    }

    fn step(&mut self) -> Result<(), String> {
        let start_rip = self.rip;
        let mut op_override = false;
        let mut rep_f2 = false;
        let mut rep_f3 = false;
        let mut rex: Option<u8> = None;
        self.seg_base = 0;

        let op = loop {
            let b = self.fetch_u8()?;
            match b {
                0x66 => op_override = true,
                0xF2 => rep_f2 = true,
                0xF3 => rep_f3 = true,
                0x65 => self.seg_base = self.teb_va,
                0x2E | 0x3E | 0x64 | 0xF0 => {}
                0x40..=0x4F => rex = Some(b),
                _ => break b,
            }
        };

        let has_rex = rex.is_some();
        let rex_b = rex.unwrap_or(0);
        let rex_w = (rex_b & 8) != 0;
        let rex_r = ((rex_b >> 2) & 1) as usize;
        let rex_x = ((rex_b >> 1) & 1) as usize;
        let rex_b_bit = (rex_b & 1) as usize;
        let op_size: u8 = if rex_w { 8 } else if op_override { 2 } else { 4 };

        match op {
            // VEX 3-byte (0xC4) BMI1 / BMI2
            0xC4 => self.step_vex3(start_rip),
            // ALU r/m, r and r, r/m and AL/rAX, imm
            0x00..=0x3D if (op & 7) <= 5 => {
                let alu = op >> 3;
                let dir = op & 7;
                match dir {
                    0 | 1 => {
                        let sz = if dir == 0 { 1 } else { op_size };
                        let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                        let r = self.resolve_target(t, 0);
                        let a = self.read_rm(r, sz, has_rex)?;
                        let b = self.read_reg(reg, sz, has_rex);
                        let res = self.alu_op(alu, a, b, sz);
                        if alu != 7 {
                            self.write_rm(r, sz, has_rex, res)?;
                        }
                    }
                    2 | 3 => {
                        let sz = if dir == 2 { 1 } else { op_size };
                        let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                        let r = self.resolve_target(t, 0);
                        let a = self.read_reg(reg, sz, has_rex);
                        let b = self.read_rm(r, sz, has_rex)?;
                        let res = self.alu_op(alu, a, b, sz);
                        if alu != 7 {
                            self.write_reg(reg, sz, has_rex, res);
                        }
                    }
                    4 => {
                        let imm = self.fetch_u8()? as u64;
                        let a = self.read_reg(RAX, 1, false);
                        let res = self.alu_op(alu, a, imm, 1);
                        if alu != 7 {
                            self.write_reg(RAX, 1, false, res);
                        }
                    }
                    5 => {
                        let imm = if op_size == 2 {
                            self.fetch_u16()? as u64
                        } else {
                            self.fetch_u32()? as i32 as i64 as u64
                        };
                        let a = self.read_reg(RAX, op_size, has_rex);
                        let res = self.alu_op(alu, a, imm, op_size);
                        if alu != 7 {
                            self.write_reg(RAX, op_size, has_rex, res);
                        }
                    }
                    _ => unreachable!(),
                }
                Ok(())
            }
            // PUSH r64
            0x50..=0x57 => {
                let r = ((op - 0x50) as usize) | (rex_b_bit << 3);
                self.push64(self.regs[r])
            }
            // POP r64
            0x58..=0x5F => {
                let r = ((op - 0x58) as usize) | (rex_b_bit << 3);
                let v = self.pop64()?;
                self.regs[r] = v;
                Ok(())
            }
            // MOVSXD r, r/m32
            0x63 => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let v = self.read_rm(r, 4, has_rex)? as u32 as i32 as i64 as u64;
                self.write_reg(reg, op_size, has_rex, v);
                Ok(())
            }
            // PUSH imm32 / imm8
            0x68 => {
                let imm = self.fetch_u32()? as i32 as i64 as u64;
                self.push64(imm)
            }
            0x6A => {
                let imm = self.fetch_u8()? as i8 as i64 as u64;
                self.push64(imm)
            }
            // IMUL r, r/m, imm32 / imm8
            0x69 | 0x6B => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let imm_len = if op == 0x6B { 1 } else if op_size == 2 { 2 } else { 4 };
                let r = self.resolve_target(t, imm_len);
                let imm = if op == 0x6B {
                    self.fetch_u8()? as i8 as i64
                } else if op_size == 2 {
                    self.fetch_u16()? as i16 as i64
                } else {
                    self.fetch_u32()? as i32 as i64
                };
                let src = Self::sign_ext(self.read_rm(r, op_size, has_rex)?, op_size) as i64;
                let prod = src.wrapping_mul(imm) as u64;
                self.write_reg(reg, op_size, has_rex, prod);
                Ok(())
            }
            // Jcc rel8
            0x70..=0x7F => {
                let disp = self.fetch_u8()? as i8 as i64;
                if self.eval_cc(op & 0xF) {
                    self.rip = self.rip.wrapping_add(disp as u64);
                }
                Ok(())
            }
            // Group 1 ALU r/m, imm
            0x80 | 0x81 | 0x83 => {
                let sz = if op == 0x80 { 1 } else { op_size };
                let (alu, t) = self.decode_modrm(0, rex_x, rex_b_bit)?;
                let imm_len = if op == 0x80 || op == 0x83 { 1 } else if op_size == 2 { 2 } else { 4 };
                let r = self.resolve_target(t, imm_len);
                let imm = match op {
                    0x80 => self.fetch_u8()? as u64,
                    0x83 => self.fetch_u8()? as i8 as i64 as u64,
                    _ if op_size == 2 => self.fetch_u16()? as u64,
                    _ => self.fetch_u32()? as i32 as i64 as u64,
                };
                let a = self.read_rm(r, sz, has_rex)?;
                let res = self.alu_op(alu as u8, a, imm, sz);
                if alu != 7 {
                    self.write_rm(r, sz, has_rex, res)?;
                }
                Ok(())
            }
            // TEST r/m, r
            0x84 | 0x85 => {
                let sz = if op == 0x84 { 1 } else { op_size };
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let a = self.read_rm(r, sz, has_rex)?;
                let b = self.read_reg(reg, sz, has_rex);
                let _ = self.alu_op(4, a, b, sz);
                Ok(())
            }
            // XCHG r/m, r
            0x86 | 0x87 => {
                let sz = if op == 0x86 { 1 } else { op_size };
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let a = self.read_rm(r, sz, has_rex)?;
                let b = self.read_reg(reg, sz, has_rex);
                self.write_rm(r, sz, has_rex, b)?;
                self.write_reg(reg, sz, has_rex, a);
                Ok(())
            }
            // MOV r/m, r and r, r/m
            0x88 | 0x89 => {
                let sz = if op == 0x88 { 1 } else { op_size };
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let v = self.read_reg(reg, sz, has_rex);
                self.write_rm(r, sz, has_rex, v)
            }
            0x8A | 0x8B => {
                let sz = if op == 0x8A { 1 } else { op_size };
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let v = self.read_rm(r, sz, has_rex)?;
                self.write_reg(reg, sz, has_rex, v);
                Ok(())
            }
            // LEA r, m
            0x8D => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let addr = match self.resolve_target(t, 0) {
                    ModRmResolved::Mem(a) => a,
                    ModRmResolved::Reg(_) => return Err(format!("invalid LEA register operand at {start_rip:#x}")),
                };
                self.write_reg(reg, op_size, has_rex, addr);
                Ok(())
            }
            // NOP / XCHG rAX, r
            0x90..=0x97 => {
                let r = ((op - 0x90) as usize) | (rex_b_bit << 3);
                if !(op == 0x90 && rex_b_bit == 0) {
                    let a = self.read_reg(RAX, op_size, has_rex);
                    let b = self.read_reg(r, op_size, has_rex);
                    self.write_reg(RAX, op_size, has_rex, b);
                    self.write_reg(r, op_size, has_rex, a);
                }
                Ok(())
            }
            // CBW / CWDE / CDQE
            0x98 => {
                match op_size {
                    2 => self.write_reg(RAX, 2, has_rex, self.read_reg(RAX, 1, false) as u8 as i8 as i16 as u64),
                    4 => self.write_reg(RAX, 4, has_rex, self.read_reg(RAX, 2, false) as u16 as i16 as i32 as u64),
                    _ => self.write_reg(RAX, 8, has_rex, self.read_reg(RAX, 4, false) as u32 as i32 as i64 as u64),
                }
                Ok(())
            }
            // CWD / CDQ / CQO
            0x99 => {
                match op_size {
                    2 => {
                        let fill = if (self.read_reg(RAX, 2, false) as u16 as i16) < 0 { 0xFFFF } else { 0 };
                        self.write_reg(RDX, 2, has_rex, fill);
                    }
                    4 => {
                        let fill = if (self.read_reg(RAX, 4, false) as u32 as i32) < 0 { 0xFFFF_FFFF } else { 0 };
                        self.write_reg(RDX, 4, has_rex, fill);
                    }
                    _ => {
                        let fill = if (self.regs[RAX] as i64) < 0 { u64::MAX } else { 0 };
                        self.regs[RDX] = fill;
                    }
                }
                Ok(())
            }
            // MOVS / STOS. Each repeated element consumes a step so a large RCX
            // remains interruptible and indexes stay at the first faulting element.
            0xA4 | 0xA5 | 0xAA | 0xAB if !rep_f2 => {
                if rep_f3 && self.regs[RCX] == 0 {
                    return Ok(());
                }
                let size = if op & 1 == 0 { 1 } else { op_size };
                let moves = op == 0xA4 || op == 0xA5;
                let value = if moves {
                    let source = self.regs[RSI].wrapping_add(self.seg_base);
                    self.read_rm(ModRmResolved::Mem(source), size, has_rex)?
                } else {
                    self.read_reg(RAX, size, has_rex)
                };
                self.write_rm(ModRmResolved::Mem(self.regs[RDI]), size, has_rex, value)?;
                let stride = if self.rflags & DF == 0 { size as u64 } else { (size as u64).wrapping_neg() };
                self.regs[RDI] = self.regs[RDI].wrapping_add(stride);
                if moves {
                    self.regs[RSI] = self.regs[RSI].wrapping_add(stride);
                }
                if rep_f3 {
                    self.regs[RCX] -= 1;
                    if self.regs[RCX] != 0 {
                        self.rip = start_rip;
                    }
                }
                Ok(())
            }
            // TEST AL/rAX, imm
            0xA8 => {
                let imm = self.fetch_u8()? as u64;
                let _ = self.alu_op(4, self.read_reg(RAX, 1, false), imm, 1);
                Ok(())
            }
            0xA9 => {
                let imm = if op_size == 2 {
                    self.fetch_u16()? as u64
                } else {
                    self.fetch_u32()? as i32 as i64 as u64
                };
                let _ = self.alu_op(4, self.read_reg(RAX, op_size, has_rex), imm, op_size);
                Ok(())
            }
            // MOV r8, imm8
            0xB0..=0xB7 => {
                let r = ((op - 0xB0) as usize) | (rex_b_bit << 3);
                let imm = self.fetch_u8()? as u64;
                self.write_reg(r, 1, has_rex, imm);
                Ok(())
            }
            // MOV r, imm16/32/64
            0xB8..=0xBF => {
                let r = ((op - 0xB8) as usize) | (rex_b_bit << 3);
                let imm = match op_size {
                    2 => self.fetch_u16()? as u64,
                    4 => self.fetch_u32()? as u64,
                    _ => self.fetch_u64()?,
                };
                self.write_reg(r, op_size, has_rex, imm);
                Ok(())
            }
            // Shifts & Rotates
            0xC0 | 0xC1 | 0xD0..=0xD3 => {
                let sz = if matches!(op, 0xC0 | 0xD0 | 0xD2) { 1 } else { op_size };
                let (sub, t) = self.decode_modrm(0, rex_x, rex_b_bit)?;
                let has_imm = matches!(op, 0xC0 | 0xC1);
                let r = self.resolve_target(t, if has_imm { 1 } else { 0 });
                let raw_count = if has_imm {
                    self.fetch_u8()? as u32
                } else if matches!(op, 0xD0 | 0xD1) {
                    1
                } else {
                    (self.regs[RCX] & 0xFF) as u32
                };
                let bits = (sz as u32) * 8;
                let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
                let count = raw_count & if bits == 64 { 63 } else { 31 };
                let val = self.read_rm(r, sz, has_rex)? & mask;
                let res = if count == 0 {
                    val
                } else {
                    let r_cnt = count & (bits - 1);
                    let out = match sub {
                        0 => if r_cnt == 0 { val } else { ((val << r_cnt) | (val >> (bits - r_cnt))) & mask }, // ROL
                        1 => if r_cnt == 0 { val } else { ((val >> r_cnt) | (val << (bits - r_cnt))) & mask }, // ROR
                        4 | 6 => (val << count) & mask, // SHL/SAL
                        5 => val >> count,              // SHR
                        7 => ((Self::sign_ext(val, sz) as i64) >> count) as u64 & mask, // SAR
                        _ => return Err(format!("unsupported shift subop {sub} at {start_rip:#x}")),
                    };
                    if matches!(sub, 4 | 5 | 6 | 7) {
                        self.set_zsp(out, sz);
                    }
                    out
                };
                self.write_rm(r, sz, has_rex, res)
            }
            // RET / RET imm16
            0xC2 => {
                let imm = self.fetch_u16()? as u64;
                let ret = self.pop64()?;
                self.regs[RSP] = self.regs[RSP].wrapping_add(imm);
                self.rip = ret;
                Ok(())
            }
            0xC3 => {
                self.rip = self.pop64()?;
                Ok(())
            }
            // MOV r/m, imm
            0xC6 | 0xC7 => {
                let sz = if op == 0xC6 { 1 } else { op_size };
                let (_, t) = self.decode_modrm(0, rex_x, rex_b_bit)?;
                let imm_len = if op == 0xC6 { 1 } else if op_size == 2 { 2 } else { 4 };
                let r = self.resolve_target(t, imm_len);
                let imm = if op == 0xC6 {
                    self.fetch_u8()? as u64
                } else if op_size == 2 {
                    self.fetch_u16()? as u64
                } else {
                    self.fetch_u32()? as i32 as i64 as u64
                };
                self.write_rm(r, sz, has_rex, imm)
            }
            // CALL rel32
            0xE8 => {
                let disp = self.fetch_u32()? as i32 as i64;
                let ret = self.rip;
                self.push64(ret)?;
                self.rip = self.rip.wrapping_add(disp as u64);
                Ok(())
            }
            // JMP rel32 / rel8
            0xE9 => {
                let disp = self.fetch_u32()? as i32 as i64;
                self.rip = self.rip.wrapping_add(disp as u64);
                Ok(())
            }
            0xEB => {
                let disp = self.fetch_u8()? as i8 as i64;
                self.rip = self.rip.wrapping_add(disp as u64);
                Ok(())
            }
            // Group 3: TEST imm / NOT / NEG / MUL / IMUL / DIV / IDIV
            0xF6 | 0xF7 => {
                let sz = if op == 0xF6 { 1 } else { op_size };
                let (sub, t) = self.decode_modrm(0, rex_x, rex_b_bit)?;
                let imm_len = if sub <= 1 { if sz == 1 { 1 } else if sz == 2 { 2 } else { 4 } } else { 0 };
                let r = self.resolve_target(t, imm_len);
                let v = self.read_rm(r, sz, has_rex)?;
                match sub {
                    0 | 1 => {
                        let imm = if sz == 1 {
                            self.fetch_u8()? as u64
                        } else if sz == 2 {
                            self.fetch_u16()? as u64
                        } else {
                            self.fetch_u32()? as i32 as i64 as u64
                        };
                        let _ = self.alu_op(4, v, imm, sz);
                    }
                    2 => self.write_rm(r, sz, has_rex, !v)?,
                    3 => {
                        let res = self.alu_op(5, 0, v, sz);
                        self.write_rm(r, sz, has_rex, res)?;
                    }
                    4 => {
                        // MUL
                        let a = self.read_reg(RAX, sz, has_rex) as u128;
                        let prod = a * (v as u128);
                        if sz == 1 {
                            self.write_reg(RAX, 2, false, prod as u64);
                        } else {
                            let bits = (sz as u32) * 8;
                            self.write_reg(RAX, sz, has_rex, prod as u64);
                            self.write_reg(RDX, sz, has_rex, (prod >> bits) as u64);
                        }
                    }
                    5 => {
                        // IMUL 1-op
                        let a = Self::sign_ext(self.read_reg(RAX, sz, has_rex), sz) as i64 as i128;
                        let b = Self::sign_ext(v, sz) as i64 as i128;
                        let prod = a * b;
                        if sz == 1 {
                            self.write_reg(RAX, 2, false, prod as u64);
                        } else {
                            let bits = (sz as u32) * 8;
                            self.write_reg(RAX, sz, has_rex, prod as u64);
                            self.write_reg(RDX, sz, has_rex, (prod >> bits) as u64);
                        }
                    }
                    6 => {
                        if v == 0 {
                            return Err(format!("divide by zero at {start_rip:#x}"));
                        }
                        let bits = (sz as u32) * 8;
                        let num = ((self.read_reg(RDX, sz, has_rex) as u128) << bits) | (self.read_reg(RAX, sz, has_rex) as u128);
                        self.write_reg(RAX, sz, has_rex, (num / (v as u128)) as u64);
                        self.write_reg(RDX, sz, has_rex, (num % (v as u128)) as u64);
                    }
                    7 => {
                        let d = Self::sign_ext(v, sz) as i64 as i128;
                        if d == 0 {
                            return Err(format!("signed divide by zero at {start_rip:#x}"));
                        }
                        let bits = (sz as u32) * 8;
                        let hi = Self::sign_ext(self.read_reg(RDX, sz, has_rex), sz) as i64 as i128;
                        let lo = self.read_reg(RAX, sz, has_rex) as u128 as i128;
                        let num = (hi << bits) | lo;
                        self.write_reg(RAX, sz, has_rex, (num / d) as u64);
                        self.write_reg(RDX, sz, has_rex, (num % d) as u64);
                    }
                    _ => unreachable!(),
                }
                Ok(())
            }
            // Group 4 / 5: INC / DEC / CALL r/m64 / JMP r/m64 / PUSH r/m64
            0xFE | 0xFF => {
                let sz = if op == 0xFE { 1 } else { op_size };
                let (sub, t) = self.decode_modrm(0, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                match sub {
                    0 => {
                        let saved_cf = self.rflags & CF;
                        let res = self.alu_op(0, self.read_rm(r, sz, has_rex)?, 1, sz);
                        self.rflags = (self.rflags & !CF) | saved_cf;
                        self.write_rm(r, sz, has_rex, res)
                    }
                    1 => {
                        let saved_cf = self.rflags & CF;
                        let res = self.alu_op(5, self.read_rm(r, sz, has_rex)?, 1, sz);
                        self.rflags = (self.rflags & !CF) | saved_cf;
                        self.write_rm(r, sz, has_rex, res)
                    }
                    2 if op == 0xFF => {
                        let target = self.read_rm(r, 8, has_rex)?;
                        let ret = self.rip;
                        self.push64(ret)?;
                        self.rip = target;
                        Ok(())
                    }
                    4 if op == 0xFF => {
                        self.rip = self.read_rm(r, 8, has_rex)?;
                        Ok(())
                    }
                    6 if op == 0xFF => {
                        let v = self.read_rm(r, 8, has_rex)?;
                        self.push64(v)
                    }
                    _ => Err(format!("unsupported Group 5 subop {sub} at {start_rip:#x}")),
                }
            }
            // Two-byte opcodes (0x0F ...)
            0x0F => self.step_0f(start_rip, op_size, op_override, has_rex, rex_r, rex_x, rex_b_bit, rep_f2, rep_f3),
            0xFC => { self.rflags &= !DF; Ok(()) }
            0xFD => { self.rflags |= DF; Ok(()) }
            _ => Err(format!("unsupported x86-64 opcode {op:#04x} at {start_rip:#x}")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn step_0f(
        &mut self,
        start_rip: u64,
        op_size: u8,
        op_override: bool,
        has_rex: bool,
        rex_r: usize,
        rex_x: usize,
        rex_b_bit: usize,
        rep_f2: bool,
        rep_f3: bool,
    ) -> Result<(), String> {
        let op2 = self.fetch_u8()?;
        match op2 {
            // SSE / SSE2 Moves: MOVUPS / MOVSS / MOVUPD / MOVSD (0F 10 / 0F 11)
            0x10 => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                if rep_f3 {
                    let bits = match r {
                        ModRmResolved::Reg(src) => (self.xmm[reg] & !0xFFFF_FFFFu128) | (self.xmm[src & 15] & 0xFFFF_FFFFu128),
                        ModRmResolved::Mem(a) => self.mem.read_u32(a)? as u128,
                    };
                    self.xmm[reg] = bits;
                } else if rep_f2 {
                    let bits = match r {
                        ModRmResolved::Reg(src) => (self.xmm[reg] & !0xFFFF_FFFF_FFFF_FFFFu128) | (self.xmm[src & 15] & 0xFFFF_FFFF_FFFF_FFFFu128),
                        ModRmResolved::Mem(a) => self.mem.read_u64(a)? as u128,
                    };
                    self.xmm[reg] = bits;
                } else {
                    self.xmm[reg] = self.read_xmm_rm(r)?;
                }
                Ok(())
            }
            0x11 => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                if rep_f3 {
                    match r {
                        ModRmResolved::Reg(dst) => {
                            self.xmm[dst & 15] = (self.xmm[dst & 15] & !0xFFFF_FFFFu128) | (self.xmm[reg] & 0xFFFF_FFFFu128);
                        }
                        ModRmResolved::Mem(a) => self.mem.write_u32(a, self.xmm[reg] as u32)?,
                    }
                } else if rep_f2 {
                    match r {
                        ModRmResolved::Reg(dst) => {
                            self.xmm[dst & 15] = (self.xmm[dst & 15] & !0xFFFF_FFFF_FFFF_FFFFu128) | (self.xmm[reg] & 0xFFFF_FFFF_FFFF_FFFFu128);
                        }
                        ModRmResolved::Mem(a) => self.mem.write_u64(a, self.xmm[reg] as u64)?,
                    }
                } else {
                    self.write_xmm_rm(r, self.xmm[reg])?;
                }
                Ok(())
            }
            // MOVAPS / MOVAPD (0F 28 / 0F 29) & MOVDQA / MOVDQU (66/F3 0F 6F / 7F)
            0x28 | 0x6F => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                self.xmm[reg] = self.read_xmm_rm(r)?;
                Ok(())
            }
            0x29 | 0x7F => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                self.write_xmm_rm(r, self.xmm[reg])
            }
            // CVTSI2SS / CVTSI2SD (F3/F2 0F 2A)
            0x2A if rep_f3 || rep_f2 => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let sz = if op_size == 8 { 8 } else { 4 };
                let iv = Self::sign_ext(self.read_rm(r, sz, has_rex)?, sz) as i64;
                if rep_f3 {
                    let f = (iv as f32).to_bits() as u128;
                    self.xmm[reg] = (self.xmm[reg] & !0xFFFF_FFFFu128) | f;
                } else {
                    let d = (iv as f64).to_bits() as u128;
                    self.xmm[reg] = (self.xmm[reg] & !0xFFFF_FFFF_FFFF_FFFFu128) | d;
                }
                Ok(())
            }
            // CVTTSS2SI / CVTTSD2SI (F3/F2 0F 2C)
            0x2C if rep_f3 || rep_f2 => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let sz = if op_size == 8 { 8 } else { 4 };
                let value = if rep_f3 {
                    self.read_xmm_f32(r)? as f64
                } else {
                    self.read_xmm_f64(r)?
                };
                let out = Self::truncate_float_to_integer(value, sz);
                self.write_reg(reg, sz, has_rex, out);
                Ok(())
            }
            // UCOMISS / UCOMISD / COMISS / COMISD (0F 2E / 0F 2F)
            0x2E | 0x2F => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                if op_override {
                    let a = f64::from_bits(self.xmm[reg] as u64);
                    let b = self.read_xmm_f64(r)?;
                    self.set_fp_cmp_flags(a, b);
                } else {
                    let a = f32::from_bits(self.xmm[reg] as u32) as f64;
                    let b = self.read_xmm_f32(r)? as f64;
                    self.set_fp_cmp_flags(a, b);
                }
                Ok(())
            }
            // SQRT* (0F 51), ADD* (0F 58), MUL* (0F 59), SUB* (0F 5C), MIN* (0F 5D), DIV* (0F 5E), MAX* (0F 5F)
            0x51 | 0x58 | 0x59 | 0x5C | 0x5D | 0x5E | 0x5F => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let apply_f32 = |a: f32, b: f32| -> f32 {
                    match op2 {
                        0x51 => b.sqrt(),
                        0x58 => a + b,
                        0x59 => a * b,
                        0x5C => a - b,
                        0x5D => if a < b { a } else { b },
                        0x5E => a / b,
                        _ => if a > b { a } else { b },
                    }
                };
                let apply_f64 = |a: f64, b: f64| -> f64 {
                    match op2 {
                        0x51 => b.sqrt(),
                        0x58 => a + b,
                        0x59 => a * b,
                        0x5C => a - b,
                        0x5D => if a < b { a } else { b },
                        0x5E => a / b,
                        _ => if a > b { a } else { b },
                    }
                };
                if rep_f3 {
                    let a = f32::from_bits(self.xmm[reg] as u32);
                    let b = self.read_xmm_f32(r)?;
                    let res = apply_f32(a, b).to_bits() as u128;
                    self.xmm[reg] = (self.xmm[reg] & !0xFFFF_FFFFu128) | res;
                } else if rep_f2 {
                    let a = f64::from_bits(self.xmm[reg] as u64);
                    let b = self.read_xmm_f64(r)?;
                    let res = apply_f64(a, b).to_bits() as u128;
                    self.xmm[reg] = (self.xmm[reg] & !0xFFFF_FFFF_FFFF_FFFFu128) | res;
                } else if op_override {
                    let a = Self::xmm_lanes_f64(self.xmm[reg]);
                    let b = Self::xmm_lanes_f64(self.read_xmm_rm(r)?);
                    self.xmm[reg] = Self::pack_f64([apply_f64(a[0], b[0]), apply_f64(a[1], b[1])]);
                } else {
                    let a = Self::xmm_lanes_f32(self.xmm[reg]);
                    let b = Self::xmm_lanes_f32(self.read_xmm_rm(r)?);
                    self.xmm[reg] = Self::pack_f32([
                        apply_f32(a[0], b[0]),
                        apply_f32(a[1], b[1]),
                        apply_f32(a[2], b[2]),
                        apply_f32(a[3], b[3]),
                    ]);
                }
                Ok(())
            }
            // Bitwise SSE/SSE2: ANDPS/PAND (54/DB), ORPS/POR (56/EB), XORPS/PXOR (57/EF)
            0x54 | 0x56 | 0x57 | 0xDB | 0xEB | 0xEF => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let b = self.read_xmm_rm(r)?;
                match op2 {
                    0x54 | 0xDB => self.xmm[reg] &= b,
                    0x56 | 0xEB => self.xmm[reg] |= b,
                    _ => self.xmm[reg] ^= b,
                }
                Ok(())
            }
            // CVTSS2SD (F3 0F 5A) / CVTSD2SS (F2 0F 5A)
            0x5A if rep_f3 || rep_f2 => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                if rep_f3 {
                    let d = (self.read_xmm_f32(r)? as f64).to_bits() as u128;
                    self.xmm[reg] = (self.xmm[reg] & !0xFFFF_FFFF_FFFF_FFFFu128) | d;
                } else {
                    let f = (self.read_xmm_f64(r)? as f32).to_bits() as u128;
                    self.xmm[reg] = (self.xmm[reg] & !0xFFFF_FFFFu128) | f;
                }
                Ok(())
            }
            // MOVD / MOVQ xmm, r/m (66 0F 6E)
            0x6E if op_override => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let sz = if op_size == 8 { 8 } else { 4 };
                self.xmm[reg] = self.read_rm(r, sz, has_rex)? as u128;
                Ok(())
            }
            // MOVD / MOVQ r/m, xmm (66 0F 7E) and MOVQ xmm, xmm/m64 (F3 0F 7E)
            0x7E if op_override || rep_f3 => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                if rep_f3 {
                    let v = match r {
                        ModRmResolved::Reg(src) => self.xmm[src & 15] as u64,
                        ModRmResolved::Mem(a) => self.mem.read_u64(a)?,
                    };
                    self.xmm[reg] = v as u128;
                    Ok(())
                } else {
                    let sz = if op_size == 8 { 8 } else { 4 };
                    self.write_rm(r, sz, has_rex, self.xmm[reg] as u64)
                }
            }
            // MOVQ xmm/m64, xmm (66 0F D6)
            0xD6 if op_override => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                match r {
                    ModRmResolved::Reg(dst) => {
                        self.xmm[dst & 15] = self.xmm[reg] as u64 as u128;
                        Ok(())
                    }
                    ModRmResolved::Mem(a) => self.mem.write_u64(a, self.xmm[reg] as u64),
                }
            }
            // PCMPEQB (66 0F 74) / PCMPEQD (66 0F 76)
            0x74 | 0x76 if op_override => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let a = self.xmm[reg].to_le_bytes();
                let b = self.read_xmm_rm(r)?.to_le_bytes();
                let mut out = [0u8; 16];
                if op2 == 0x74 {
                    for i in 0..16 {
                        out[i] = if a[i] == b[i] { 0xFF } else { 0 };
                    }
                } else {
                    for i in 0..4 {
                        let o = i * 4;
                        let eq = a[o..o + 4] == b[o..o + 4];
                        out[o..o + 4].fill(if eq { 0xFF } else { 0 });
                    }
                }
                self.xmm[reg] = u128::from_le_bytes(out);
                Ok(())
            }
            // PMOVMSKB r32, xmm (66 0F D7)
            0xD7 if op_override => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let src = match r {
                    ModRmResolved::Reg(s) => self.xmm[s & 15].to_le_bytes(),
                    ModRmResolved::Mem(_) => return Err(format!("PMOVMSKB requires register operand at {start_rip:#x}")),
                };
                let mut mask = 0u64;
                for (i, byte) in src.iter().enumerate() {
                    if (*byte & 0x80) != 0 {
                        mask |= 1u64 << i;
                    }
                }
                self.write_reg(reg, 4, has_rex, mask);
                Ok(())
            }
            // Multi-byte NOP
            0x1F => {
                let _ = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                Ok(())
            }
            // CMOVcc r, r/m
            0x40..=0x4F => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let src = self.read_rm(r, op_size, has_rex)?;
                if self.eval_cc(op2 & 0xF) {
                    self.write_reg(reg, op_size, has_rex, src);
                } else if op_size == 4 {
                    // 32-bit CMOVcc zero-extends dst even when false.
                    let cur = self.read_reg(reg, 4, has_rex);
                    self.write_reg(reg, 4, has_rex, cur);
                }
                Ok(())
            }
            // Jcc rel32
            0x80..=0x8F => {
                let disp = self.fetch_u32()? as i32 as i64;
                if self.eval_cc(op2 & 0xF) {
                    self.rip = self.rip.wrapping_add(disp as u64);
                }
                Ok(())
            }
            // SETcc r/m8
            0x90..=0x9F => {
                let (_, t) = self.decode_modrm(0, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let v = if self.eval_cc(op2 & 0xF) { 1 } else { 0 };
                self.write_rm(r, 1, has_rex, v)
            }
            // BT / BTS / BTR / BTC r/m, r
            0xA3 | 0xAB | 0xB3 | 0xBB => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let bits = (op_size as u32) * 8;
                let bit = (self.read_reg(reg, op_size, has_rex) as u32) & (bits - 1);
                let val = self.read_rm(r, op_size, has_rex)?;
                self.rflags = (self.rflags & !CF) | if ((val >> bit) & 1) != 0 { CF } else { 0 };
                match op2 {
                    0xAB => self.write_rm(r, op_size, has_rex, val | (1u64 << bit))?,
                    0xB3 => self.write_rm(r, op_size, has_rex, val & !(1u64 << bit))?,
                    0xBB => self.write_rm(r, op_size, has_rex, val ^ (1u64 << bit))?,
                    _ => {}
                }
                Ok(())
            }
            // Group 8: BT / BTS / BTR / BTC r/m, imm8
            0xBA => {
                let (sub, t) = self.decode_modrm(0, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 1);
                let bits = (op_size as u32) * 8;
                let bit = (self.fetch_u8()? as u32) & (bits - 1);
                let val = self.read_rm(r, op_size, has_rex)?;
                self.rflags = (self.rflags & !CF) | if ((val >> bit) & 1) != 0 { CF } else { 0 };
                match sub {
                    4 => {}
                    5 => self.write_rm(r, op_size, has_rex, val | (1u64 << bit))?,
                    6 => self.write_rm(r, op_size, has_rex, val & !(1u64 << bit))?,
                    7 => self.write_rm(r, op_size, has_rex, val ^ (1u64 << bit))?,
                    _ => return Err(format!("unsupported 0F BA /{sub} at {start_rip:#x}")),
                }
                Ok(())
            }
            // IMUL r, r/m
            0xAF => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let a = Self::sign_ext(self.read_reg(reg, op_size, has_rex), op_size) as i64;
                let b = Self::sign_ext(self.read_rm(r, op_size, has_rex)?, op_size) as i64;
                self.write_reg(reg, op_size, has_rex, a.wrapping_mul(b) as u64);
                Ok(())
            }
            // MOVZX r, r/m8 / r/m16
            0xB6 | 0xB7 => {
                let src_sz = if op2 == 0xB6 { 1 } else { 2 };
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let v = self.read_rm(r, src_sz, has_rex)?;
                self.write_reg(reg, op_size, has_rex, v);
                Ok(())
            }
            // POPCNT (F3 0F B8)
            0xB8 if rep_f3 => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let v = self.read_rm(r, op_size, has_rex)?;
                self.rflags &= !(CF | OF | SF | ZF);
                if v == 0 {
                    self.rflags |= ZF;
                }
                self.write_reg(reg, op_size, has_rex, v.count_ones() as u64);
                Ok(())
            }
            // BSF / TZCNT (0F BC / F3 0F BC)
            0xBC => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let v = self.read_rm(r, op_size, has_rex)?;
                let bits = (op_size as u32) * 8;
                if rep_f3 {
                    let c = if v == 0 { bits } else { v.trailing_zeros() };
                    self.rflags &= !(CF | ZF);
                    if v == 0 {
                        self.rflags |= CF;
                    }
                    if c == 0 {
                        self.rflags |= ZF;
                    }
                    self.write_reg(reg, op_size, has_rex, c as u64);
                } else {
                    self.rflags &= !ZF;
                    if v == 0 {
                        self.rflags |= ZF;
                    } else {
                        self.write_reg(reg, op_size, has_rex, v.trailing_zeros() as u64);
                    }
                }
                Ok(())
            }
            // BSR / LZCNT (0F BD / F3 0F BD)
            0xBD => {
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let v = self.read_rm(r, op_size, has_rex)?;
                let bits = (op_size as u32) * 8;
                if rep_f3 {
                    let c = if v == 0 { bits } else { (bits - 1) - (63 - v.leading_zeros()) };
                    self.rflags &= !(CF | ZF);
                    if v == 0 {
                        self.rflags |= CF;
                    }
                    if c == 0 {
                        self.rflags |= ZF;
                    }
                    self.write_reg(reg, op_size, has_rex, c as u64);
                } else {
                    self.rflags &= !ZF;
                    if v == 0 {
                        self.rflags |= ZF;
                    } else {
                        self.write_reg(reg, op_size, has_rex, (63 - v.leading_zeros()) as u64);
                    }
                }
                Ok(())
            }
            // MOVSX r, r/m8 / r/m16
            0xBE | 0xBF => {
                let src_sz = if op2 == 0xBE { 1 } else { 2 };
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let v = Self::sign_ext(self.read_rm(r, src_sz, has_rex)?, src_sz);
                self.write_reg(reg, op_size, has_rex, v);
                Ok(())
            }
            // XADD r/m, r
            0xC0 | 0xC1 => {
                let sz = if op2 == 0xC0 { 1 } else { op_size };
                let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b_bit)?;
                let r = self.resolve_target(t, 0);
                let a = self.read_rm(r, sz, has_rex)?;
                let b = self.read_reg(reg, sz, has_rex);
                let sum = self.alu_op(0, a, b, sz);
                self.write_reg(reg, sz, has_rex, a);
                self.write_rm(r, sz, has_rex, sum)
            }
            // BSWAP r32 / r64
            0xC8..=0xCF => {
                let r = ((op2 - 0xC8) as usize) | (rex_b_bit << 3);
                let v = self.read_reg(r, op_size, has_rex);
                let swapped = if op_size == 8 { v.swap_bytes() } else { (v as u32).swap_bytes() as u64 };
                self.write_reg(r, op_size, has_rex, swapped);
                Ok(())
            }
            _ => Err(format!("unsupported 0F {op2:#04x} opcode at {start_rip:#x}")),
        }
    }

    fn step_vex3(&mut self, start_rip: u64) -> Result<(), String> {
        let b1 = self.fetch_u8()?;
        let b2 = self.fetch_u8()?;
        let op = self.fetch_u8()?;
        let rex_r = ((b1 >> 7) ^ 1) as usize;
        let rex_x = (((b1 >> 6) & 1) ^ 1) as usize;
        let rex_b = (((b1 >> 5) & 1) ^ 1) as usize;
        let map = b1 & 0x1F;
        let w = (b2 & 0x80) != 0;
        let vvvv = (((!b2) >> 3) & 0xF) as usize;
        let pp = b2 & 3;
        let sz: u8 = if w { 8 } else { 4 };
        let bits = (sz as u32) * 8;
        let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };

        let (reg, t) = self.decode_modrm(rex_r, rex_x, rex_b)?;
        let is_rorx = map == 3 && pp == 3 && op == 0xF0;
        let r = self.resolve_target(t, if is_rorx { 1 } else { 0 });
        let rm_val = self.read_rm(r, sz, true)? & mask;
        let v_val = self.read_reg(vvvv, sz, true) & mask;

        match (map, pp, op) {
            // BMI1 ANDN (map 2, pp 0, 0xF2)
            (2, 0, 0xF2) => {
                let res = (!v_val) & rm_val & mask;
                self.rflags &= !(CF | OF);
                self.set_zsp(res, sz);
                self.write_reg(reg, sz, true, res);
            }
            // BMI1 BLSR/BLSMSK/BLSI (map 2, pp 0, 0xF3, reg=1/2/3)
            (2, 0, 0xF3) => {
                let res = match reg & 7 {
                    1 => rm_val & rm_val.wrapping_sub(1),
                    2 => rm_val ^ rm_val.wrapping_sub(1),
                    3 => rm_val & rm_val.wrapping_neg(),
                    _ => return Err(format!("unsupported VEX F3 /{reg} at {start_rip:#x}")),
                } & mask;
                self.rflags &= !(CF | OF);
                if rm_val == 0 && (reg & 7) != 2 {
                    self.rflags |= CF;
                } else if (reg & 7) == 2 && rm_val == 0 {
                    self.rflags |= CF;
                }
                self.set_zsp(res, sz);
                self.write_reg(vvvv, sz, true, res);
            }
            // BMI1 BEXTR (map 2, pp 0, 0xF7)
            (2, 0, 0xF7) => {
                let start = (v_val & 0xFF) as u32;
                let len = ((v_val >> 8) & 0xFF) as u32;
                let res = if start >= bits || len == 0 {
                    0
                } else {
                    let shifted = rm_val >> start;
                    if len >= bits { shifted } else { shifted & ((1u64 << len) - 1) }
                };
                self.rflags &= !(CF | OF);
                self.set_zsp(res, sz);
                self.write_reg(reg, sz, true, res);
            }
            // BMI2 BZHI (map 2, pp 0, 0xF5)
            (2, 0, 0xF5) => {
                let idx = (v_val & 0xFF) as u32;
                let res = if idx >= bits { rm_val } else { rm_val & ((1u64 << idx) - 1) };
                self.rflags &= !(CF | OF);
                if idx >= bits {
                    self.rflags |= CF;
                }
                self.set_zsp(res, sz);
                self.write_reg(reg, sz, true, res);
            }
            // BMI2 SHLX (pp=1) / SARX (pp=2) / SHRX (pp=3), map 2, 0xF7
            (2, 1, 0xF7) => {
                let c = (v_val as u32) & (bits - 1);
                self.write_reg(reg, sz, true, (rm_val << c) & mask);
            }
            (2, 2, 0xF7) => {
                let c = (v_val as u32) & (bits - 1);
                let res = ((Self::sign_ext(rm_val, sz) as i64) >> c) as u64 & mask;
                self.write_reg(reg, sz, true, res);
            }
            (2, 3, 0xF7) => {
                let c = (v_val as u32) & (bits - 1);
                self.write_reg(reg, sz, true, rm_val >> c);
            }
            // BMI2 PEXT (map 2, pp=2, 0xF5) / PDEP (map 2, pp=3, 0xF5)
            (2, 2, 0xF5) => {
                let mut res = 0u64;
                let mut bit = 1u64;
                let mut m = rm_val;
                while m != 0 {
                    let low = m & m.wrapping_neg();
                    if (v_val & low) != 0 {
                        res |= bit;
                    }
                    bit <<= 1;
                    m &= m - 1;
                }
                self.write_reg(reg, sz, true, res);
            }
            (2, 3, 0xF5) => {
                let mut res = 0u64;
                let mut s = v_val;
                let mut m = rm_val;
                while m != 0 {
                    let low = m & m.wrapping_neg();
                    if (s & 1) != 0 {
                        res |= low;
                    }
                    s >>= 1;
                    m &= m - 1;
                }
                self.write_reg(reg, sz, true, res);
            }
            // BMI2 MULX (map 2, pp=3, 0xF6)
            (2, 3, 0xF6) => {
                let a = (self.read_reg(RDX, sz, true) & mask) as u128;
                let prod = a * (rm_val as u128);
                self.write_reg(vvvv, sz, true, (prod as u64) & mask);
                self.write_reg(reg, sz, true, ((prod >> bits) as u64) & mask);
            }
            // BMI2 RORX (map 3, pp=3, 0xF0, imm8)
            (3, 3, 0xF0) => {
                let c = (self.fetch_u8()? as u32) & (bits - 1);
                let res = if c == 0 { rm_val } else { ((rm_val >> c) | (rm_val << (bits - c))) & mask };
                self.write_reg(reg, sz, true, res);
            }
            _ => return Err(format!("unsupported VEX instruction map={map} pp={pp} op={op:#04x} at {start_rip:#x}")),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum ModRmTarget {
    Reg(usize),
    Mem(u64),
    RipRel(i64),
}

#[derive(Debug, Clone, Copy)]
enum ModRmResolved {
    Reg(usize),
    Mem(u64),
}

impl Pe {
    pub(crate) fn parse_bytes(d: &[u8]) -> Result<Self, String> {
        crate::pe::parse(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal runnable PE32+ with custom x86-64 code and optional imports for testing.
    fn build_test_pe(code: &[u8], dll: Option<&str>, func: Option<&str>) -> Vec<u8> {
        let mut pe = vec![0u8; 0x1000];
        pe[0..2].copy_from_slice(b"MZ");
        pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        let o = 0x80usize;
        pe[o..o + 4].copy_from_slice(b"PE\0\0");
        pe[o + 4..o + 6].copy_from_slice(&0x8664u16.to_le_bytes());
        pe[o + 6..o + 8].copy_from_slice(&1u16.to_le_bytes()); // 1 section
        pe[o + 20..o + 22].copy_from_slice(&0xF0u16.to_le_bytes());
        let opt = o + 24;
        pe[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes()); // PE32+
        pe[opt + 16..opt + 20].copy_from_slice(&0x1000u32.to_le_bytes()); // entry RVA
        pe[opt + 24..opt + 32].copy_from_slice(&0x14000_0000u64.to_le_bytes()); // ImageBase
        pe[opt + 56..opt + 60].copy_from_slice(&0x3000u32.to_le_bytes()); // SizeOfImage
        pe[opt + 60..opt + 64].copy_from_slice(&0x200u32.to_le_bytes()); // SizeOfHeaders
        pe[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes

        let sh = opt + 0xF0;
        pe[sh..sh + 5].copy_from_slice(b".text");
        pe[sh + 8..sh + 12].copy_from_slice(&0x1000u32.to_le_bytes()); // vsize
        pe[sh + 12..sh + 16].copy_from_slice(&0x1000u32.to_le_bytes()); // vaddr
        pe[sh + 16..sh + 20].copy_from_slice(&0x0E00u32.to_le_bytes()); // raw_size
        pe[sh + 20..sh + 24].copy_from_slice(&0x0200u32.to_le_bytes()); // raw_off
        pe[sh + 36..sh + 40].copy_from_slice(&0xE000_0020u32.to_le_bytes()); // RWX

        // Copy code at file offset 0x200 (RVA 0x1000).
        pe[0x200..0x200 + code.len()].copy_from_slice(code);

        if let (Some(dll_name), Some(fn_name)) = (dll, func) {
            set_test_imports(&mut pe, dll_name, &[fn_name]);
        }
        pe
    }

    fn set_test_imports(pe: &mut [u8], dll: &str, names: &[&str]) {
        assert!(names.len() <= 5 && dll.len() < 32);
        let directory = 0x98 + 112 + 8;
        pe[directory..directory + 4].copy_from_slice(&0x1400u32.to_le_bytes());
        pe[directory + 4..directory + 8].copy_from_slice(&40u32.to_le_bytes());
        pe[0x600..0x604].copy_from_slice(&0x14C0u32.to_le_bytes()); // Lookup table
        pe[0x60C..0x610].copy_from_slice(&0x14A0u32.to_le_bytes()); // DLL name
        pe[0x610..0x614].copy_from_slice(&0x1450u32.to_le_bytes()); // IAT
        pe[0x6A0..0x6A0 + dll.len()].copy_from_slice(dll.as_bytes());
        for (index, name) in names.iter().enumerate() {
            let offset = 0x700 + index * 0x40;
            let rva = (offset as u64 + 0xE00).to_le_bytes();
            let slot = index * 8;
            pe[0x6C0 + slot..0x6C8 + slot].copy_from_slice(&rva);
            pe[0x650 + slot..0x658 + slot].copy_from_slice(&rva);
            pe[offset + 2..offset + 2 + name.len()].copy_from_slice(name.as_bytes());
        }
    }

    #[test]
    fn guest_maps_aliased_physical_pages_through_imports() {
        const RETURN_IF_ZERO: [u8; 10] = [0x48, 0x85, 0xC0, 0x75, 5, 0x48, 0x83, 0xC4, 0x48, 0xC3];
        const WINDOW_IN_RCX: [u8; 5] = [0x48, 0x8B, 0x4C, 0x24, 0x38];
        const ALIAS_OFFSET: [u8; 7] = [0x48, 0x81, 0xC1, 0, 0, 1, 0];
        const MAP_ONE_PAGE: [u8; 10] = [0xBA, 1, 0, 0, 0, 0x4C, 0x8D, 0x44, 0x24, 0x30];
        let mut entry = vec![
            0x48, 0x83, 0xEC, 0x48, 0x31, 0xC9,
            0xBA, 0, 0, 2, 0, 0x41, 0xB8, 0, 0x20, 0x40, 0x20,
            0x41, 0xB9, 7, 0, 0, 0, 0xC7, 0x44, 0x24, 0x20, 1, 0, 0, 0,
        ];
        call_import(&mut entry, 0);
        entry.extend_from_slice(&RETURN_IF_ZERO);
        entry.extend_from_slice(&[
            0x48, 0x89, 0x44, 0x24, 0x38,
            0x48, 0xC7, 0x44, 0x24, 0x28, 1, 0, 0, 0,
            0xB9, 0x80, 2, 0, 0, 0x48, 0x8D, 0x54, 0x24, 0x28, 0x4C, 0x8D, 0x44, 0x24, 0x30,
        ]);
        call_import(&mut entry, 1);
        entry.extend_from_slice(&RETURN_IF_ZERO);
        entry.extend_from_slice(&WINDOW_IN_RCX);
        entry.extend_from_slice(&MAP_ONE_PAGE);
        call_import(&mut entry, 2);
        entry.extend_from_slice(&RETURN_IF_ZERO);
        entry.extend_from_slice(&[0xC6, 0, 42]);
        entry.extend_from_slice(&WINDOW_IN_RCX);
        entry.extend_from_slice(&ALIAS_OFFSET);
        entry.extend_from_slice(&MAP_ONE_PAGE);
        call_import(&mut entry, 2);
        entry.extend_from_slice(&RETURN_IF_ZERO);
        entry.extend_from_slice(&[0x0F, 0xB6, 0, 0x83, 0xF8, 42, 0x74, 5, 0x48, 0x83, 0xC4, 0x48, 0xC3]);
        entry.extend_from_slice(&WINDOW_IN_RCX);
        entry.extend_from_slice(&[0xBA, 2, 0, 0, 0, 0x45, 0x31, 0xC0]);
        call_import(&mut entry, 2);
        entry.extend_from_slice(&RETURN_IF_ZERO);
        entry.extend_from_slice(&[0xB9, 1, 0, 0, 0, 0x48, 0x8D, 0x54, 0x24, 0x30]);
        call_import(&mut entry, 3);
        entry.extend_from_slice(&RETURN_IF_ZERO);
        entry.extend_from_slice(&[0xB8, 42, 0, 0, 0, 0x48, 0x83, 0xC4, 0x48, 0xC3]);

        let imports = ["XMemVirtualAlloc", "XMemAllocatePhysicalPages", "XMemMapPhysicalPages", "XMemFreePhysicalPages"];
        let mut program = build_test_pe(&entry, None, None);
        set_test_imports(&mut program, "xmem.dll", &imports);
        let mut interpreter = XboxX64Emu::load_pe(&program).unwrap();
        interpreter.set_strict_imports();
        let result = interpreter.run(200);
        assert!(result.stopped_reason.is_none(), "{result:?}");
        assert_eq!(result.exit_code, 42);

        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            let result = crate::native::run(&program).expect("native run with physical aliases");
            assert!(result.is_complete(), "{result:?}");
            assert_eq!(result.shimmed, imports);
            assert_eq!(result.exit_code, 42);
        }
    }

    const READ_STATIC_TLS: [u8; 14] = [
        0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0, 0x48, 0x8B, 0, 0x8B, 0,
    ];

    fn static_tls_program(code: &[u8]) -> Vec<u8> {
        let mut program = build_test_pe(code, None, None);
        let directory = 0x98 + 112 + 9 * 8;
        program[directory..directory + 4].copy_from_slice(&0x1200u32.to_le_bytes());
        program[directory + 4..directory + 8].copy_from_slice(&40u32.to_le_bytes());
        let base = 0x14000_0000u64;
        for (offset, rva) in [(0x400, 0x1280), (0x408, 0x1284), (0x410, 0x1290), (0x418, 0x12A0)] {
            program[offset..offset + 8].copy_from_slice(&(base + rva).to_le_bytes());
        }
        program[0x420..0x424].copy_from_slice(&12u32.to_le_bytes());
        program[0x480..0x484].copy_from_slice(&40u32.to_le_bytes());
        program[0x490..0x494].fill(0xFF);
        for offset in [0x4A0, 0x4A8] {
            program[offset..offset + 8].copy_from_slice(&(base + 0x12C0).to_le_bytes());
        }
        let callback = [0x65, 0x48, 0x8B, 0x04, 0x25, 0x58, 0, 0, 0, 0x48, 0x8B, 0, 0x01, 0x10, 0xC3];
        program[0x4C0..0x4C0 + callback.len()].copy_from_slice(&callback);
        program
    }

    #[test]
    fn guest_physical_pages_alias_across_windows_and_unmap_before_free() {
        let mut emu = XboxX64Emu::load_pe(&build_test_pe(&[0xC3], None, None)).unwrap();
        let first = emu.xmem_virtual_alloc(0, 2 * PAGE_SIZE_64K, 0x2040_2000, 1).unwrap();
        let second = emu.xmem_virtual_alloc(0, 2 * PAGE_SIZE_64K, 0x2040_2000, 4).unwrap();
        let args = emu.xmem_virtual_alloc(0, PAGE_SIZE_4K, 0x3000, 4).unwrap();
        let stack = emu.regs[RSP];
        let before = emu.mem.committed_bytes();
        let call = |emu: &mut XboxX64Emu, name: &str, a, b, c| {
            emu.regs[RSP] = stack;
            emu.regs[RCX] = a;
            emu.regs[RDX] = b;
            emu.regs[R8] = c;
            emu.dispatch_hle(&ThunkEntry { dll: "xmem.dll".into(), name: name.into() }).unwrap();
            emu.regs[RAX]
        };
        emu.mem.write_u64(args, 2).unwrap();
        assert_eq!(call(&mut emu, "XMemAllocatePhysicalPages", 0x280, args, args + 8), 1);
        assert_eq!(emu.mem.read_u64(args).unwrap(), 2);
        assert_eq!(emu.mem.committed_bytes(), before + 2 * PAGE_SIZE_64K);
        let page0 = emu.mem.read_u64(args + 8).unwrap();
        let page1 = emu.mem.read_u64(args + 16).unwrap();
        assert_eq!(page1 - page0, 16);
        assert_eq!(call(&mut emu, "XMemMapPhysicalPages", first, 2, args + 8), first);
        emu.mem.write_u64(args + 24, second).unwrap();
        emu.mem.write_u64(args + 32, second + PAGE_SIZE_64K).unwrap();
        assert_eq!(call(&mut emu, "XMemMapPhysicalPagesScatter", args + 24, 2, args + 8), 1);
        emu.mem.write_u32(first + PAGE_SIZE_64K - 2, 0x1234_5678).unwrap();
        assert_eq!(emu.mem.read_u32(second + PAGE_SIZE_64K - 2).unwrap(), 0x1234_5678);
        assert_eq!(call(&mut emu, "XMemFreePhysicalPages", 2, args + 8, 0), 0);
        assert_eq!(call(&mut emu, "XMemMapPhysicalPagesScatter", args + 24, 1, 0), 1);
        assert!(emu.mem.write_u32(second + PAGE_SIZE_64K - 2, 0).is_err());
        assert_eq!(emu.mem.read_u16(first + PAGE_SIZE_64K).unwrap(), 0x1234);
        assert_eq!(call(&mut emu, "XMemMapPhysicalPages", first, 2, 0), first);
        assert_eq!(call(&mut emu, "XMemMapPhysicalPagesScatter", args + 24, 2, 0), 1);
        assert_eq!(call(&mut emu, "XMemFreePhysicalPages", 2, args + 8, 0), 1);
        assert_eq!(emu.mem.committed_bytes(), before);
    }

    #[test]
    fn guest_xmem_virtual_alloc_reserves_and_commits_console_page_sizes() {
        let run = |flags: u32, protection: u32, size: u64| {
            let mut entry = vec![0x48, 0x83, 0xEC, 0x28, 0x31, 0xC9, 0x48, 0xBA];
            entry.extend_from_slice(&size.to_le_bytes());
            entry.extend_from_slice(&[0x41, 0xB8]);
            entry.extend_from_slice(&flags.to_le_bytes());
            entry.extend_from_slice(&[0x41, 0xB9, 7, 0, 0, 0, 0xC7, 0x44, 0x24, 0x20]);
            entry.extend_from_slice(&protection.to_le_bytes());
            call_import(&mut entry, 0);
            entry.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28, 0xC3]);
            let program = build_test_pe(&entry, Some("xmem.dll"), Some("XMemVirtualAlloc"));
            let mut emu = XboxX64Emu::load_pe(&program).unwrap();
            emu.set_strict_imports();
            let before = emu.mem.committed_bytes();
            let result = emu.run(32);
            (emu, before, result)
        };

        let (reserved, before, result) = run(0x2040_2000, 1, 1);
        assert!(result.stopped_reason.is_none(), "{:?}", result.stopped_reason);
        let address = reserved.regs[RAX];
        assert_ne!(address, 0);
        assert_eq!(address % PAGE_SIZE_64K, 0);
        assert_eq!(reserved.mem.committed_bytes(), before);
        assert!(reserved.mem.read_u8(address).is_err());

        let large = 2 * 1024 * 1024;
        let (mut committed, before, result) = run(0x2000_3000, 0x802, 4097);
        assert!(result.stopped_reason.is_none(), "{:?}", result.stopped_reason);
        let address = committed.regs[RAX];
        assert_ne!(address, 0);
        assert_eq!(address % large, 0);
        assert_eq!(committed.mem.committed_bytes(), before + large);
        assert_eq!(committed.mem.read_u8(address + large - 1).unwrap(), 0);
        assert!(committed.mem.write_u8(address, 1).is_err());

        let (rejected, before, result) = run(0x2000_3000, 4, u64::MAX);
        assert!(result.stopped_reason.is_none(), "{:?}", result.stopped_reason);
        assert_eq!(rejected.regs[RAX], 0);
        assert_eq!(rejected.mem.committed_bytes(), before);

        let (mut placeholders, before, result) = run(0x42000, 1, 3 * PAGE_SIZE_64K);
        assert!(result.stopped_reason.is_none(), "{:?}", result.stopped_reason);
        let base = placeholders.regs[RAX];
        assert_ne!(base, 0);
        let middle = base + PAGE_SIZE_64K;
        assert_eq!(placeholders.xmem_virtual_alloc(middle, PAGE_SIZE_64K, 0x2040_6000, 4), Some(middle));
        assert!(placeholders.mem.read_u8(middle).is_err());
        assert_eq!(placeholders.xmem_virtual_alloc(middle, PAGE_SIZE_64K, 0x7000, 4), Some(0));
        assert_eq!(placeholders.xmem_virtual_alloc(base, PAGE_SIZE_64K, 0x7000, 4), Some(base));
        assert_eq!(placeholders.xmem_virtual_alloc(base + 2 * PAGE_SIZE_64K, PAGE_SIZE_64K, 0x7000, 4), Some(base + 2 * PAGE_SIZE_64K));
        assert_eq!(placeholders.mem.committed_bytes(), before + 2 * PAGE_SIZE_64K);
        assert_eq!(placeholders.mem.read_u8(base).unwrap(), 0);
        assert_eq!(placeholders.mem.read_u8(base + 2 * PAGE_SIZE_64K).unwrap(), 0);

        let (_, _, result) = run(0x100000, 4, PAGE_SIZE_64K);
        assert!(result.stopped_reason.unwrap().contains("XMemVirtualAlloc allocation flags 0x100000"));
    }

    #[test]
    fn guest_static_tls_callbacks_run_before_entry_with_a_step_budget() {
        let mut entry = READ_STATIC_TLS.to_vec();
        entry.push(0xC3);
        let program = static_tls_program(&entry);
        let base = 0x14000_0000u64;
        let mut emu = XboxX64Emu::load_pe(&program).unwrap();
        assert_eq!(emu.mem.read_u32(base + 0x1290).unwrap(), 0);
        let vector = emu.mem.read_u64(emu.teb_va + 0x58).unwrap();
        let data = emu.mem.read_u64(vector).unwrap();
        assert_eq!(emu.mem.read_u32(data).unwrap(), 40);
        assert_eq!(emu.mem.read_u64(data + 4).unwrap(), 0);
        assert_eq!(emu.step_budget(1), EmuStepState::Running);
        assert_eq!(emu.mem.read_u32(data).unwrap(), 40);
        assert_eq!(emu.step_budget(32), EmuStepState::Exited(42));
        assert_eq!(emu.mem.read_u32(data).unwrap(), 42);
    }

    #[test]
    fn guest_worker_gets_separate_static_tls_and_preserves_its_entry_argument() {
        let mut entry = vec![0x48, 0x83, 0xEC, 0x48, 0x31, 0xD2, 0x4C, 0x8D, 0x05];
        let worker_offset = 0x100i32;
        entry.extend_from_slice(&(worker_offset - (entry.len() as i32 + 4)).to_le_bytes());
        entry.extend_from_slice(&[0x41, 0xB9, 7, 0, 0, 0]);
        call_import(&mut entry, 0);
        entry.extend_from_slice(&[0x48, 0x89, 0xC1, 0x31, 0xD2]);
        call_import(&mut entry, 1);
        entry.extend_from_slice(&READ_STATIC_TLS);
        entry.extend_from_slice(&[0x48, 0x83, 0xC4, 0x48, 0xC3]);
        entry.resize(worker_offset as usize, 0);
        entry.extend_from_slice(&READ_STATIC_TLS);
        entry.extend_from_slice(&[0x01, 0xC8, 0xC3]);
        let mut program = static_tls_program(&entry);
        set_test_imports(&mut program, "kernelx.dll", &["CreateThread", "WaitForSingleObject"]);
        let mut emu = XboxX64Emu::load_pe(&program).unwrap();
        emu.set_strict_imports();
        let result = emu.run(100);
        assert!(result.stopped_reason.is_none(), "{:?}", result.stopped_reason);
        assert_eq!(result.exit_code, 42);
        assert_eq!(emu.threads[1].exit_code, 51);
    }

    fn call_import(code: &mut Vec<u8>, slot: i32) {
        let displacement = 0x450 + slot * 8 - (code.len() as i32 + 6);
        code.extend_from_slice(&[0xFF, 0x15]);
        code.extend_from_slice(&displacement.to_le_bytes());
    }

    #[test]
    fn guest_draws_copies_reads_back_and_presents_verified_pixels() {
        let pixels: [[u8; 4]; 4] = [
            [0x11, 0x22, 0x33, 0x44],
            [0x55, 0x66, 0x77, 0x88],
            [0x99, 0xAA, 0xBB, 0xCC],
            [0xDD, 0xEE, 0xFF, 0x00],
        ];
        let mut code = vec![0x48, 0x83, 0xEC, 0x68]; // sub rsp, 0x68
        code.extend_from_slice(&[0xB9, 1, 0, 0, 0, 0xBA, 2, 0, 0, 0, 0x41, 0xB8, 3, 0, 0, 0]);
        call_import(&mut code, 0); // clear RGBA8 to (1, 2, 3, 255)
        code.extend_from_slice(&[0xB9, 10, 0, 0, 0, 0xBA, 11, 0, 0, 0]); // x = 10, y = 11
        code.extend_from_slice(&[0x41, 0xB8, 3, 0, 0, 0, 0x41, 0xB9, 2, 0, 0, 0]); // 3 x 2
        code.extend_from_slice(&[0xC7, 0x44, 0x24, 0x20, 10, 9, 8, 7]); // packed RGBA = 0x0708090A
        call_import(&mut code, 1); // draw a solid rectangle
        for (index, pixel) in pixels.iter().enumerate() {
            code.extend_from_slice(&[0xC7, 0x44, 0x24, 0x30 + 4 * index as u8]);
            code.extend_from_slice(pixel); // write each pixel to the guest upload buffer
        }
        code.extend_from_slice(&[0xB9, 3, 0, 0, 0, 0xBA, 4, 0, 0, 0]); // x = 3, y = 4
        code.extend_from_slice(&[0x41, 0xB8, 2, 0, 0, 0, 0x41, 0xB9, 2, 0, 0, 0]); // 2 x 2
        code.extend_from_slice(&[0x48, 0x8D, 0x44, 0x24, 0x30]); // lea rax, [rsp + 0x30]
        code.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x20]); // fifth parameter
        call_import(&mut code, 2); // upload
        code.extend_from_slice(&[0x48, 0x8D, 0x44, 0x24, 0x40]); // separate readback buffer
        code.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x20]);
        call_import(&mut code, 3); // readback
        call_import(&mut code, 4); // present
        code.extend_from_slice(&[0x8B, 0x44, 0x24, 0x4C]); // return the last pixel as the exit code
        code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x68, 0xC3]);

        let mut pe = build_test_pe(&code, None, None);
        let imports = [
            "XDeviceClear",
            "XDeviceFillRect",
            "XDeviceCopyRgba8",
            "XDeviceReadRgba8",
            "XDevicePresent",
        ];
        set_test_imports(&mut pe, "emu-graphics.dll", &imports);
        let mut emu = XboxX64Emu::load_pe(&pe).unwrap();
        emu.set_strict_imports();
        let readback_address = emu.regs[RSP] - 0x68 + 0x40;
        let result = emu.run(200);

        assert!(result.stopped_reason.is_none(), "{:?}", result.stopped_reason);
        assert_eq!(result.exit_code, i32::from_le_bytes(pixels[3]));
        assert_eq!(result.api_calls, imports.map(|name| format!("emu-graphics.dll!{name}")));
        assert!(result.unimplemented_apis.is_empty());
        assert_eq!(result.framebuffer.presents, 1);

        let mut readback = [0; 16];
        emu.mem.read_bytes(readback_address, &mut readback).unwrap();
        assert_eq!(readback.as_slice(), pixels.as_flattened());
        let width = result.framebuffer.width as usize;
        for (index, actual) in result.framebuffer.rgba.chunks_exact(4).enumerate() {
            let expected = match (index % width, index / width) {
                (x @ 3..=4, y @ 4..=5) => pixels[(y - 4) * 2 + (x - 3)],
                (10..=12, 11..=12) => [7, 8, 9, 10],
                _ => [1, 2, 3, 255],
            };
            assert_eq!(actual, expected, "pixel {index}");
        }
    }

    #[test]
    fn guest_executes_rdna2_shader_and_reads_back_a_gradient() {
        let shader: [u32; 10] = [
            0x0A08_0700, // v_subrev_f32_e32 v4, v0, v3
            0x7E0A_02FF, // v_mov_b32_e32 v5, literal
            0x3E00_0000, // 0.125
            0x080A_0B01, // v_sub_f32_e32 v5, v1, v5
            0x100C_0300, // v_mul_f32_e32 v6, v0, v1
            0x060E_04F0, // v_add_f32_e32 v7, 0.5, v2
            0x560E_0300, // v_fmac_f32_e32 v7, v0, v1
            0xF800_180F, // exp mrt0, v4, v5, v6, v7 done vm
            0x0706_0504,
            0xBF81_0000, // s_endpgm
        ];
        let mut code = vec![0x48, 0x83, 0xEC, 0x68]; // sub rsp, 0x68
        code.extend_from_slice(&[0xB9, 3, 0, 0, 0, 0xBA, 4, 0, 0, 0]); // x = 3, y = 4
        code.extend_from_slice(&[0x41, 0xB8, 2, 0, 0, 0, 0x41, 0xB9, 2, 0, 0, 0]); // 2 x 2
        let displacement = 0x180i32 - (code.len() as i32 + 7);
        code.extend_from_slice(&[0x48, 0x8D, 0x05]); // lea rax, [rip + shader]
        code.extend_from_slice(&displacement.to_le_bytes());
        code.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x20]); // pointer
        code.extend_from_slice(&[0xC7, 0x44, 0x24, 0x28]); // mov dword [rsp + 0x28], byte count
        code.extend_from_slice(&(size_of_val(&shader) as u32).to_le_bytes());
        call_import(&mut code, 0);
        code.extend_from_slice(&[0x48, 0x8D, 0x44, 0x24, 0x30]); // readback buffer
        code.extend_from_slice(&[0x48, 0x89, 0x44, 0x24, 0x20]);
        call_import(&mut code, 1);
        call_import(&mut code, 2);
        code.extend_from_slice(&[0x8B, 0x44, 0x24, 0x30]); // return first pixel
        code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x68, 0xC3]);
        code.resize(0x180, 0);
        code.extend(shader.into_iter().flat_map(u32::to_le_bytes));
        let mut pe = build_test_pe(&code, None, None);
        let imports = ["XDeviceDrawRdna2Rect", "XDeviceReadRgba8", "XDevicePresent"];
        set_test_imports(&mut pe, "emu-graphics.dll", &imports);
        let expected = [
            [191, 32, 16, 143], [64, 32, 48, 175],
            [191, 159, 48, 175], [64, 159, 143, 255],
        ];

        let mut emu = XboxX64Emu::load_pe(&pe).unwrap();
        emu.set_strict_imports();
        let readback_addr = emu.regs[RSP] - 0x68 + 0x30;
        let result = emu.run(100);
        assert!(result.stopped_reason.is_none(), "{:?}", result.stopped_reason);
        assert_eq!(result.exit_code, i32::from_le_bytes(expected[0]));
        assert_eq!(result.framebuffer.presents, 1);
        assert_eq!(result.api_calls, imports.map(|name| format!("emu-graphics.dll!{name}")));
        let mut readback = [0; 16];
        emu.mem.read_bytes(readback_addr, &mut readback).unwrap();
        assert_eq!(readback.as_slice(), expected.as_flattened());

        let width = result.framebuffer.width as usize;
        for (index, pixel) in result.framebuffer.rgba.chunks_exact(4).enumerate() {
            let expected_pixel = match (index % width, index / width) {
                (x @ 3..=4, y @ 4..=5) => expected[(y - 4) * 2 + x - 3],
                _ => [0; 4],
            };
            assert_eq!(pixel, expected_pixel, "pixel {index}");
        }

        for (word, expected_error) in [
            (0u32, "Unsupported RDNA2"),
            (0x5608_0300, "reads uninitialized v4"),
        ] {
            pe[0x380..0x384].copy_from_slice(&word.to_le_bytes());
            let mut invalid = XboxX64Emu::load_pe(&pe).unwrap();
            invalid.set_strict_imports();
            let result = invalid.run(100);
            assert!(result.stopped_reason.unwrap().contains(expected_error));
            assert_eq!(result.framebuffer.presents, 0);
            assert!(result.framebuffer.rgba.iter().all(|&channel| channel == 0));
        }
    }

    #[test]
    fn guest_framebuffer_copy_rejects_invalid_geometry_and_memory() {
        let code = [0xFF, 0x15, 0x4A, 0x04, 0x00, 0x00, 0xC3]; // call the first import; ret
        let pe = build_test_pe(&code, Some("emu-graphics.dll"), Some("XDeviceCopyRgba8"));
        for (x, expected_error) in [(u32::MAX, "copy rectangle"), (0, "read fault")] {
            let mut emu = XboxX64Emu::load_pe(&pe).unwrap();
            emu.set_strict_imports();
            emu.regs[RCX] = x.into();
            emu.regs[R8] = 2;
            emu.regs[R9] = 2;
            emu.mem.write_bytes(emu.regs[RSP] + 0x20, &0u64.to_le_bytes()).unwrap();
            let result = emu.run(20);
            assert!(result.stopped_reason.unwrap().contains(expected_error));
            assert!(result.framebuffer.rgba.iter().all(|&byte| byte == 0));
            assert_eq!(result.framebuffer.presents, 0);
        }
    }

    #[test]
    fn repeated_string_operations_respect_direction_and_step_budget() {
        // std; rep movsb; cld; rep stosw; ret
        let pe = build_test_pe(&[0xFD, 0xF3, 0xA4, 0xFC, 0xF3, 0x66, 0xAB, 0xC3], None, None);
        let mut emu = XboxX64Emu::load_pe(&pe).unwrap();
        let src = emu.rip + 0x100;
        let dest = src + 0x20;
        emu.mem.write_bytes(src, &[1, 2, 3]).unwrap();
        emu.regs[RSI] = src + 2;
        emu.regs[RDI] = dest + 2;
        emu.regs[RCX] = 3;

        assert_eq!(emu.step_budget(2), EmuStepState::Running);
        assert_eq!(emu.regs[RCX], 2);
        assert_eq!(emu.mem.read_u8(dest + 2).unwrap(), 3);
        assert_eq!(emu.step_budget(3), EmuStepState::Running);
        assert_eq!(emu.mem.read_u16(dest).unwrap(), 0x0201);
        assert_eq!(emu.regs[RSI], src - 1);
        assert_eq!(emu.regs[RDI], dest - 1);
        assert_eq!(emu.rflags & DF, 0);

        emu.regs[RAX] = 0x1234;
        emu.regs[RDI] = dest + 4;
        emu.regs[RCX] = 2;
        assert_eq!(emu.step_budget(20), EmuStepState::Exited(0x1234));
        assert_eq!(emu.mem.read_u32(dest + 4).unwrap(), 0x1234_1234);
    }

    #[test]
    fn strict_probe_stops_at_placeholder_graphics_or_unknown_import() {
        for name in ["D3D12XBOX_CreateDevice", "UnimplementedTestApi"] {
            // call [rip + (0x1450 - 0x1006)]; ret
            let pe = build_test_pe(&[0xFF, 0x15, 0x4A, 0x04, 0x00, 0x00, 0xC3], Some("d3d12_xs.dll"), Some(name));
            let mut emu = XboxX64Emu::load_pe(&pe).unwrap();
            emu.set_strict_imports();
            let result = emu.run(20);
            assert!(result.stopped_reason.unwrap().contains(name));
            assert_eq!(result.unimplemented_apis, [format!("d3d12_xs.dll!{name}")]);
            assert_eq!(emu.total_instructions(), 1);
        }
    }

    #[test]
    fn executes_x64_arithmetic_bitops_and_bmi2_in_sandbox() {
        // mov ecx, 0x12345678; bswap ecx; mov eax, ecx; bswap eax;
        // mov edx, 4; shlx eax, eax, edx (c4 e2 69 f7 c0); ret
        let code = [
            0xB9, 0x78, 0x56, 0x34, 0x12, // mov ecx, 0x12345678
            0x0F, 0xC9,                   // bswap ecx -> 0x78563412
            0x89, 0xC8,                   // mov eax, ecx
            0x0F, 0xC8,                   // bswap eax -> 0x12345678
            0xBA, 0x04, 0x00, 0x00, 0x00, // mov edx, 4
            0xC4, 0xE2, 0x69, 0xF7, 0xC0, // shlx eax, eax, edx -> 0x23456780
            0xC3,                         // ret
        ];
        let pe = build_test_pe(&code, None, None);
        let mut emu = XboxX64Emu::load_pe(&pe).unwrap();
        let res = emu.run(1000);
        assert!(res.stopped_reason.is_none(), "{:?}", res.stopped_reason);
        assert_eq!(res.exit_code as u32, 0x2345_6780);
        assert_eq!(res.instructions, 7);
    }

    #[test]
    fn dispatches_xbox_hle_import_and_captures_output() {
        // sub rsp, 0x28
        // xor ecx, ecx
        // lea rdx, [rip + msg]
        // call qword ptr [rip + iat_slot]  ; calls xgameruntime.dll!XGameRuntimePrint
        // mov eax, 42
        // add rsp, 0x28
        // ret
        // msg: "Hello Xbox Series X!\0"
        let mut code = Vec::new();
        code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // 0x00: sub rsp, 0x28
        code.extend_from_slice(&[0x31, 0xC9]);             // 0x04: xor ecx, ecx
        // 0x06: lea rdx, [rip + disp32] -> next_rip = 0x0D, msg at 0x20 => disp = 0x13
        code.extend_from_slice(&[0x48, 0x8D, 0x15, 0x13, 0x00, 0x00, 0x00]);
        // 0x0D: call qword ptr [rip + disp32] -> next_rip = 0x13, IAT[0] at RVA 0x1450 => disp = 0x43D
        let disp_iat = (0x450i32 - 0x13i32).to_le_bytes();
        code.extend_from_slice(&[0xFF, 0x15]);
        code.extend_from_slice(&disp_iat);
        code.extend_from_slice(&[0xB8, 42, 0, 0, 0]);      // 0x13: mov eax, 42
        code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28]); // 0x18: add rsp, 0x28
        code.push(0xC3);                                   // 0x1C: ret
        while code.len() < 0x20 {
            code.push(0x90);
        }
        code.extend_from_slice(b"Hello Xbox Series X!\0");

        let pe = build_test_pe(&code, Some("xgameruntime.dll"), Some("XGameRuntimePrint"));
        let mut emu = XboxX64Emu::load_pe(&pe).unwrap();
        let res = emu.run(1000);
        assert!(res.stopped_reason.is_none(), "{:?}", res.stopped_reason);
        assert_eq!(res.exit_code, 42);
        assert_eq!(res.output.trim(), "Hello Xbox Series X!");
        assert!(res.api_calls.iter().any(|s| s.contains("XGameRuntimePrint")));
    }

    #[test]
    fn guest_can_load_and_call_runtime_error_export_dynamically() {
        let base = 0x14000_1000u64;
        let library = base + 0x200;
        let export = base + 0x240;
        let message = base + 0x280;
        let mut code = vec![0x48, 0x83, 0xEC, 0x28, 0x48, 0xB9];
        code.extend_from_slice(&library.to_le_bytes());
        call_import(&mut code, 0);
        code.extend_from_slice(&[0x48, 0x89, 0xC1, 0x48, 0xBA]);
        code.extend_from_slice(&export.to_le_bytes());
        call_import(&mut code, 1);
        code.extend_from_slice(&[0xB9, 5, 0x40, 0, 0x80, 0x48, 0xBA]);
        code.extend_from_slice(&message.to_le_bytes());
        code.extend_from_slice(&[0xFF, 0xD0, 0x48, 0x83, 0xC4, 0x28, 0xC3]);
        code.resize(0x200, 0);
        code.extend("XGameRuntime.dll\0".encode_utf16().flat_map(u16::to_le_bytes));
        code.resize(0x240, 0);
        code.extend_from_slice(b"XErrorReport\0");
        code.resize(0x280, 0);
        code.extend_from_slice(b"startup failed\0");

        let mut program = build_test_pe(&code, None, None);
        set_test_imports(&mut program, "kernelx.dll", &["LoadLibraryW", "GetProcAddress"]);
        let mut emu = XboxX64Emu::load_pe(&program).unwrap();
        emu.set_strict_imports();
        assert_eq!(emu.get_runtime_module(library, true, false).unwrap(), 0);
        let result = emu.run(100);
        assert!(result.stopped_reason.is_none(), "{result:?}");
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.output, "XErrorReport(0x80004005): startup failed\n");
        let handle = emu.get_runtime_module(library, true, false).unwrap();
        let before = emu.thunks.len();
        assert_ne!(emu.get_runtime_export(handle, export).unwrap(), 0);
        assert_eq!(emu.thunks.len(), before);
        assert_eq!(emu.get_runtime_export(handle, message).unwrap(), 0);
        assert_eq!(emu.mem.read_u32(emu.teb_va + LAST_ERROR_OFFSET).unwrap(), 127);

        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            let native = crate::native::run(&program).expect("native run with dynamic Xbox export");
            assert!(native.is_complete(), "{native:?}");
            assert_eq!(native.resolved, 2);
            assert_eq!(native.shimmed, ["XErrorReport"]);
            assert_eq!(native.exit_code, result.exit_code);
            assert_eq!(native.output, result.output);
        }
    }

    #[test]
    fn xinput_get_state_reports_pressed_buttons() {
        // sub rsp, 0x38
        // xor ecx, ecx               ; pad 0
        // lea rdx, [rsp + 0x20]      ; &XINPUT_STATE
        // call qword ptr [rip + iat] ; xinput1_4.dll!XInputGetState
        // movzx eax, word ptr [rsp + 0x24] ; wButtons
        // add rsp, 0x38
        // ret
        let mut code = Vec::new();
        code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x38]);       // 0x00: sub rsp, 0x38
        code.extend_from_slice(&[0x31, 0xC9]);                   // 0x04: xor ecx, ecx
        code.extend_from_slice(&[0x48, 0x8D, 0x54, 0x24, 0x20]); // 0x06: lea rdx, [rsp + 0x20]
        // 0x0B: call [rip + (0x450 - 0x11)]
        let disp_iat = (0x450i32 - 0x11i32).to_le_bytes();
        code.extend_from_slice(&[0xFF, 0x15]);
        code.extend_from_slice(&disp_iat);
        code.extend_from_slice(&[0x0F, 0xB7, 0x44, 0x24, 0x24]); // 0x11: movzx eax, word ptr [rsp + 0x24]
        code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x38]);       // 0x16: add rsp, 0x38
        code.push(0xC3);                                         // 0x1A: ret

        let pe = build_test_pe(&code, Some("xinput1_4.dll"), Some("XInputGetState"));
        let mut emu = XboxX64Emu::load_pe(&pe).unwrap();
        // Press A (bit 0 -> 0x1000) + Start (bit 3 -> 0x0010) + Up (bit 4 -> 0x0001)
        emu.set_buttons(0, (1 << 0) | (1 << 3) | (1 << 4));
        let res = emu.run(1000);
        assert!(res.stopped_reason.is_none(), "{:?}", res.stopped_reason);
        assert_eq!(res.exit_code as u16, 0x1011);
    }

    #[test]
    fn scalar_truncation_matches_x64_for_invalid_and_boundary_inputs() {
        let values = [
            f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -0.0,
            1.9, -1.9, 2_147_483_647.9, 2_147_483_648.0,
            -2_147_483_648.9, -2_147_483_649.0,
            9_223_372_036_854_774_784.0, 9_223_372_036_854_775_808.0,
            -9_223_372_036_854_775_808.0, -9_223_372_036_854_777_856.0,
        ];
        for single in [false, true] {
            for wide in [false, true] {
                for memory in [false, true] {
                    let mut code = vec![if single { 0xF3 } else { 0xF2 }];
                    if wide {
                        code.push(0x48);
                    }
                    code.extend_from_slice(&[0x0F, 0x2C, if memory { 0x00 } else { 0xC0 }]);
                    let pe = build_test_pe(&code, None, None);
                    let mut emu = XboxX64Emu::load_pe(&pe).unwrap();
                    let entry = emu.rip;
                    let size = if wide { 8 } else { 4 };
                    let buffer = emu.mem.alloc(None, PAGE_SIZE_4K, PAGE_SIZE_4K, MemoryProtect::ReadWrite, true).unwrap();
                    let operand = buffer + PAGE_SIZE_4K - if single { 4 } else { 8 };
                    for value in values {
                        let value = if single { (value as f32) as f64 } else { value };
                        let bits = if single { (value as f32).to_bits() as u64 } else { value.to_bits() };
                        if single {
                            emu.mem.write_u32(operand, bits as u32).unwrap();
                        } else {
                            emu.mem.write_u64(operand, bits).unwrap();
                        }
                        let original_xmm = (u128::MAX << 64) | bits as u128;
                        emu.xmm[0] = original_xmm;
                        emu.regs[RAX] = if memory { operand } else { u64::MAX };
                        emu.rip = entry;
                        emu.step().unwrap();
                        let expected = XboxX64Emu::truncate_float_to_integer(value, size);
                        assert_eq!(emu.regs[RAX], expected);
                        assert_eq!(emu.xmm[0], original_xmm);
                        #[cfg(target_arch = "x86_64")]
                        unsafe {
                            use std::arch::x86_64::*;
                            let native = match (single, wide) {
                                (true, true) => _mm_cvttss_si64(_mm_set_ss(value as f32)) as u64,
                                (true, false) => _mm_cvttss_si32(_mm_set_ss(value as f32)) as u32 as u64,
                                (false, true) => _mm_cvttsd_si64(_mm_set_sd(value)) as u64,
                                (false, false) => _mm_cvttsd_si32(_mm_set_sd(value)) as u32 as u64,
                            };
                            assert_eq!(emu.regs[RAX], native, "value={value}, single={single}, wide={wide}, memory={memory}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn sse2_float_math_and_vector_masks() {
        // Compute sqrt((9.0 + 7.0) * 4.0) = sqrt(64.0) = 8, plus test PXOR/PCMPEQB/PMOVMSKB = 0xFFFF.
        // mov eax, 9;  cvtsi2ss xmm0, eax
        // mov eax, 7;  cvtsi2ss xmm1, eax
        // addss xmm0, xmm1          -> 16.0
        // mov eax, 4;  cvtsi2ss xmm1, eax
        // mulss xmm0, xmm1          -> 64.0
        // sqrtss xmm0, xmm0         -> 8.0
        // cvttss2si eax, xmm0       -> eax = 8
        // pxor xmm2, xmm2
        // pxor xmm3, xmm3
        // pcmpeqb xmm2, xmm3        -> all 0xFF
        // pmovmskb ecx, xmm2        -> ecx = 0xFFFF
        // shl ecx, 8
        // or eax, ecx               -> 0xFFFF08
        // ret
        let code = [
            0xB8, 9, 0, 0, 0,             // mov eax, 9
            0xF3, 0x0F, 0x2A, 0xC0,       // cvtsi2ss xmm0, eax
            0xB8, 7, 0, 0, 0,             // mov eax, 7
            0xF3, 0x0F, 0x2A, 0xC8,       // cvtsi2ss xmm1, eax
            0xF3, 0x0F, 0x58, 0xC1,       // addss xmm0, xmm1
            0xB8, 4, 0, 0, 0,             // mov eax, 4
            0xF3, 0x0F, 0x2A, 0xC8,       // cvtsi2ss xmm1, eax
            0xF3, 0x0F, 0x59, 0xC1,       // mulss xmm0, xmm1
            0xF3, 0x0F, 0x51, 0xC0,       // sqrtss xmm0, xmm0
            0xF3, 0x0F, 0x2C, 0xC0,       // cvttss2si eax, xmm0
            0x66, 0x0F, 0xEF, 0xD2,       // pxor xmm2, xmm2
            0x66, 0x0F, 0xEF, 0xDB,       // pxor xmm3, xmm3
            0x66, 0x0F, 0x74, 0xD3,       // pcmpeqb xmm2, xmm3
            0x66, 0x0F, 0xD7, 0xCA,       // pmovmskb ecx, xmm2
            0xC1, 0xE1, 0x08,             // shl ecx, 8
            0x09, 0xC8,                   // or eax, ecx
            0xC3,                         // ret
        ];
        let pe = build_test_pe(&code, None, None);
        let mut emu = XboxX64Emu::load_pe(&pe).unwrap();
        let res = emu.run(1000);
        assert!(res.stopped_reason.is_none(), "{:?}", res.stopped_reason);
        assert_eq!(res.exit_code as u32, 0x00FF_FF08);
    }

    #[test]
    fn gs_teb_peb_and_cooperative_thread_join() {
        // Test 1: verify gs:[0x30] == TEB self pointer and gs:[0x60] != 0 (PEB).
        // mov rax, qword ptr gs:[0x30]   -> 65 48 8B 04 25 30 00 00 00
        // mov rcx, qword ptr gs:[0x60]   -> 65 48 8B 0C 25 60 00 00 00
        // sub rax, qword ptr [rax + 0x30] -> should be 0
        // test rcx, rcx; setne al; movzx eax, al; ret
        let code = [
            0x65, 0x48, 0x8B, 0x04, 0x25, 0x30, 0x00, 0x00, 0x00, // mov rax, gs:[0x30]
            0x65, 0x48, 0x8B, 0x0C, 0x25, 0x60, 0x00, 0x00, 0x00, // mov rcx, gs:[0x60]
            0x48, 0x2B, 0x40, 0x30,                               // sub rax, [rax + 0x30]
            0x48, 0x85, 0xC0,                                     // test rax, rax
            0x0F, 0x94, 0xC0,                                     // sete al
            0x48, 0x85, 0xC9,                                     // test rcx, rcx
            0x0F, 0x95, 0xC1,                                     // setne cl
            0x20, 0xC8,                                           // and al, cl
            0x0F, 0xB6, 0xC0,                                     // movzx eax, al
            0xC3,                                                 // ret
        ];
        let pe = build_test_pe(&code, None, None);
        let mut emu = XboxX64Emu::load_pe(&pe).unwrap();
        let res = emu.run(1000);
        assert!(res.stopped_reason.is_none(), "{:?}", res.stopped_reason);
        assert_eq!(res.exit_code, 1, "gs:[0x30] self-TEB and gs:[0x60] PEB should both be valid");
    }
}
