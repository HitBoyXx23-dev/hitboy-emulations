//! Clean-room sandboxed x86-64 (Zen 2 / Jaguar) user-mode CPU interpreter and HLE Xbox One /
//! Series X|S runtime for Emulator mode. Executes decrypted PE32+ x86-64 code safely on any host
//! architecture without running untrusted machine code natively.

use crate::era_mem::{MemoryProtect, TitleAddressSpace, PAGE_SIZE_4K, PAGE_SIZE_64K};
use crate::gpu_bridge::XboxFramebuffer;
use crate::pe::Pe;
use std::collections::HashMap;

const EXIT_SENTINEL: u64 = 0xFFFF_FFFF_FFFF_0000;
const THUNK_BASE: u64 = 0xFFFF_FFFF_FFFE_0000;
const THUNK_STRIDE: u64 = 8;

const RAX: usize = 0;
const RCX: usize = 1;
const RDX: usize = 2;
const RSP: usize = 4;
const R8: usize = 8;
const R9: usize = 9;

const CF: u64 = 1 << 0;
const PF: u64 = 1 << 2;
const ZF: u64 = 1 << 6;
const SF: u64 = 1 << 7;
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

#[derive(Debug, Clone)]
struct ThunkEntry {
    dll: String,
    name: String,
}

/// Sandboxed Xbox One / Series X|S x86-64 CPU + HLE runtime.
pub struct XboxX64Emu {
    pub regs: [u64; 16],
    pub rip: u64,
    pub rflags: u64,
    pub mem: TitleAddressSpace,
    pub framebuffer: XboxFramebuffer,
    thunks: HashMap<u64, ThunkEntry>,
    output: String,
    api_calls: Vec<String>,
    unimplemented_apis: Vec<String>,
    exited: bool,
    exit_code: i32,
    tick_counter: u64,
    heap_allocs: HashMap<u64, u64>,
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

        let mut regs = [0u64; 16];
        regs[RSP] = initial_rsp;

        Ok(Self {
            regs,
            rip: base + pe.entry as u64,
            rflags: 0x202,
            mem,
            framebuffer: XboxFramebuffer::new(320, 180),
            thunks,
            output: String::new(),
            api_calls: Vec::new(),
            unimplemented_apis: Vec::new(),
            exited: false,
            exit_code: 0,
            tick_counter: 1000,
            heap_allocs: HashMap::new(),
        })
    }

    /// Executes up to `max_instructions` instructions and returns the captured run report.
    pub fn run(&mut self, max_instructions: u64) -> EmuResult {
        let mut count = 0u64;
        let mut stopped_reason = None;

        while count < max_instructions && !self.exited {
            if self.rip == EXIT_SENTINEL {
                self.exit_code = self.regs[RAX] as i32;
                self.exited = true;
                break;
            }
            if let Some(thunk) = self.thunks.get(&self.rip).cloned() {
                if let Err(e) = self.dispatch_hle(&thunk) {
                    stopped_reason = Some(e);
                    break;
                }
                count += 1;
                continue;
            }
            match self.step() {
                Ok(()) => count += 1,
                Err(e) => {
                    stopped_reason = Some(e);
                    break;
                }
            }
        }
        if !self.exited && stopped_reason.is_none() && count >= max_instructions {
            stopped_reason = Some(format!("instruction limit ({max_instructions}) reached at RIP {:#x}", self.rip));
        }

        EmuResult {
            exit_code: self.exit_code,
            instructions: count,
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
        let _r9 = self.regs[R9];
        let name = thunk.name.as_str();

        let ret_val: u64 = match name {
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
            n if n.eq_ignore_ascii_case("GetCurrentThreadId") => 0x1001,
            n if n.eq_ignore_ascii_case("ExitProcess") => {
                self.exit_code = rcx as i32;
                self.exited = true;
                return Ok(());
            }
            n if n.eq_ignore_ascii_case("XDeviceCreate")
                || n.eq_ignore_ascii_case("D3D12XBOX_CreateDevice")
                || n.eq_ignore_ascii_case("D3D12CreateDevice") =>
            {
                self.framebuffer.clear(16, 20, 32, 255);
                self.framebuffer.present();
                0
            }
            _ => {
                if !self.unimplemented_apis.contains(&label) {
                    self.unimplemented_apis.push(label);
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
            ModRmTarget::Mem(a) => ModRmResolved::Mem(a),
            ModRmTarget::RipRel(disp) => ModRmResolved::Mem(self.rip.wrapping_add(trailing_imm_bytes).wrapping_add(disp as u64)),
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

        let op = loop {
            let b = self.fetch_u8()?;
            match b {
                0x66 => op_override = true,
                0xF2 => rep_f2 = true,
                0xF3 => rep_f3 = true,
                0x2E | 0x3E | 0x64 | 0x65 | 0xF0 => {}
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
            0x0F => self.step_0f(start_rip, op_size, has_rex, rex_r, rex_x, rex_b_bit, rep_f2, rep_f3),
            _ => Err(format!("unsupported x86-64 opcode {op:#04x} at {start_rip:#x}")),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn step_0f(
        &mut self,
        start_rip: u64,
        op_size: u8,
        has_rex: bool,
        rex_r: usize,
        rex_x: usize,
        rex_b_bit: usize,
        _rep_f2: bool,
        rep_f3: bool,
    ) -> Result<(), String> {
        let op2 = self.fetch_u8()?;
        match op2 {
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

        if dll.is_some() {
            // Import directory at RVA 0x1400
            pe[opt + 112 + 8..opt + 112 + 12].copy_from_slice(&0x1400u32.to_le_bytes());
            pe[opt + 112 + 12..opt + 112 + 16].copy_from_slice(&40u32.to_le_bytes());
        }

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
            // File offset for RVA 0x1400 is 0x600.
            // Import descriptor at RVA 0x1400 (file 0x600)
            // OFT at RVA 0x1440 (file 0x640), IAT at RVA 0x1450 (file 0x650)
            // DLL name at RVA 0x1460 (file 0x660), Hint/Name at RVA 0x1480 (file 0x680)
            pe[0x600..0x604].copy_from_slice(&0x1440u32.to_le_bytes()); // OFT
            pe[0x60C..0x610].copy_from_slice(&0x1460u32.to_le_bytes()); // DLL Name RVA
            pe[0x610..0x614].copy_from_slice(&0x1450u32.to_le_bytes()); // IAT RVA
            pe[0x640..0x648].copy_from_slice(&0x1480u64.to_le_bytes()); // OFT[0]
            pe[0x650..0x658].copy_from_slice(&0x1480u64.to_le_bytes()); // IAT[0]
            pe[0x660..0x660 + dll_name.len()].copy_from_slice(dll_name.as_bytes());
            pe[0x682..0x682 + fn_name.len()].copy_from_slice(fn_name.as_bytes());
        }
        pe
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
}
