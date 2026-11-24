//! Native mode: instead of interpreting the program on our CPU core, map it into this
//! process and run its machine code directly on the PC's processor, with every call into a
//! Sony system library redirected to our own replacement written in Rust. Built from scratch,
//! this direct-execution path includes an in-place x86-64 `fs:` TLS & stack-canary binary
//! relinker so ELFs using `PT_TLS` (`__thread`) and `-fstack-protector` (`fs:[0x28]`) run
//! natively on Windows x64 without conflicting with Windows's own segment reservation.
//!
//! Safety: the program's code runs with this app's rights, with no sandbox. It is only
//! started when the player picks Native mode and opens a file, and only for a decrypted
//! x86-64 PlayStation ELF that `Elf::parse` accepts.

/// What a native run produced.
#[derive(Debug, Default)]
pub struct NativeResult {
    pub output: String,
    pub exit_code: i32,
    /// Number of `fs:` TLS / stack-canary instructions rewritten by the binary relinker.
    pub tls_patches: usize,
    /// Number of VideoOut frames submitted natively.
    pub video_flips: u64,
    /// System functions the program called that we don't provide yet.
    pub unimplemented: Vec<String>,
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
pub fn run(_file: &[u8]) -> Result<NativeResult, String> {
    Err("Native mode currently runs on 64-bit Windows only. Use Emulator mode on this system.".into())
}

#[cfg(all(windows, target_arch = "x86_64"))]
pub use win::run;

#[cfg(all(windows, target_arch = "x86_64"))]
mod win {
    use super::NativeResult;
    use crate::elf::{Elf, Reloc};
    use std::sync::Mutex;

    extern "system" {
        fn VirtualAlloc(addr: *mut u8, size: usize, kind: u32, protect: u32) -> *mut u8;
        fn VirtualFree(addr: *mut u8, size: usize, kind: u32) -> i32;
    }
    const COMMIT_RESERVE: u32 = 0x3000;
    const RWX: u32 = 0x40;
    const RELEASE: u32 = 0x8000;

    struct Run {
        names: Vec<String>,
        output: String,
        exit_code: Option<i32>,
        unimplemented: Vec<String>,
        video_flips: u64,
        video_buffers: Vec<u64>,
        video_width: u32,
        video_height: u32,
        direct_allocs: Vec<(usize, usize)>,
        pad_buttons: u32,
    }
    static RUN: Mutex<Option<Run>> = Mutex::new(None);
    static RUN_LOCK: Mutex<()> = Mutex::new(());

    fn with<R>(f: impl FnOnce(&mut Run) -> R) -> R {
        let mut g = RUN.lock().unwrap_or_else(|e| e.into_inner());
        f(g.as_mut().expect("native run active"))
    }

    unsafe fn cstr(p: u64) -> String {
        if p == 0 {
            return String::new();
        }
        let mut n = 0;
        while *(p as *const u8).add(n) != 0 && n < 1 << 20 {
            n += 1;
        }
        String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, n)).into_owned()
    }

    fn format(fmt: &str, args: &[u64]) -> String {
        let mut out = String::new();
        let mut it = args.iter().copied();
        let mut chars = fmt.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            let (mut zero, mut width, mut long) = (false, 0usize, false);
            while let Some(&d) = chars.peek() {
                if d == '0' && width == 0 {
                    zero = true;
                } else if d.is_ascii_digit() {
                    width = width * 10 + d.to_digit(10).unwrap() as usize;
                } else if matches!(d, 'l' | 'z' | 'h' | 'j') {
                    long = true;
                } else {
                    break;
                }
                chars.next();
            }
            let Some(conv) = chars.next() else { break };
            let mut next = || it.next().unwrap_or(0);
            let text = match conv {
                '%' => "%".to_string(),
                'd' | 'i' => if long { (next() as i64).to_string() } else { (next() as i32).to_string() },
                'u' => if long { next().to_string() } else { (next() as u32).to_string() },
                'x' => format!("{:x}", if long { next() } else { next() as u32 as u64 }),
                'X' => format!("{:X}", if long { next() } else { next() as u32 as u64 }),
                'p' => format!("0x{:x}", next()),
                'c' => ((next() as u8) as char).to_string(),
                's' => unsafe { cstr(next()) },
                _ => String::new(),
            };
            let pad = width.saturating_sub(text.chars().count());
            if zero && !matches!(conv, 's' | 'c') {
                out.push_str(&"0".repeat(pad));
            } else {
                out.push_str(&" ".repeat(pad));
            }
            out.push_str(&text);
        }
        out
    }

    fn alloc(size: usize) -> u64 {
        let layout = std::alloc::Layout::from_size_align(size.max(1) + 16, 16).unwrap();
        let p = unsafe { std::alloc::alloc_zeroed(layout) } as u64;
        if p == 0 {
            return 0;
        }
        unsafe { *(p as *mut usize) = size };
        p + 16
    }

    fn release(p: u64) {
        if p == 0 {
            return;
        }
        unsafe {
            let size = *((p - 16) as *const usize);
            std::alloc::dealloc((p - 16) as *mut u8, std::alloc::Layout::from_size_align(size.max(1) + 16, 16).unwrap());
        }
    }

    fn valloc_guest(size: usize) -> u64 {
        let sz = size.max(0x1000);
        let p = unsafe { VirtualAlloc(std::ptr::null_mut(), sz, COMMIT_RESERVE, RWX) };
        if !p.is_null() {
            with(|r| r.direct_allocs.push((p as usize, sz)));
        }
        p as u64
    }

    /// Every imported function lands here (via a generated thunk that converts the System V
    /// argument registers the program uses into this Windows call).
    extern "system" fn native_call(idx: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> u64 {
        let name = with(|r| r.names.get(idx as usize).cloned().unwrap_or_default());
        let args = [a0, a1, a2, a3, a4, a5];
        unsafe {
            match name.as_str() {
                "exit" | "_exit" | "abort" => {
                    with(|r| r.exit_code = Some(if name == "abort" { 134 } else { a0 as i32 }));
                    0
                }
                "__stack_chk_fail" => {
                    with(|r| {
                        r.output.push_str("[stack smashing detected]\n");
                        r.exit_code = Some(134);
                    });
                    0
                }
                "malloc" => alloc(a0 as usize),
                "calloc" => alloc((a0 as usize).saturating_mul(a1 as usize)),
                "memalign" | "aligned_alloc" => alloc(a1 as usize),
                "free" => {
                    release(a0);
                    0
                }
                "realloc" => {
                    let n = alloc(a1 as usize);
                    if a0 != 0 && n != 0 {
                        let old = *((a0 - 16) as *const usize);
                        std::ptr::copy_nonoverlapping(a0 as *const u8, n as *mut u8, old.min(a1 as usize));
                        release(a0);
                    }
                    n
                }
                "memcpy" | "memmove" => {
                    std::ptr::copy(a1 as *const u8, a0 as *mut u8, a2 as usize);
                    a0
                }
                "memset" => {
                    std::ptr::write_bytes(a0 as *mut u8, a1 as u8, a2 as usize);
                    a0
                }
                "memcmp" => {
                    let (x, y) = (std::slice::from_raw_parts(a0 as *const u8, a2 as usize), std::slice::from_raw_parts(a1 as *const u8, a2 as usize));
                    x.iter().zip(y).find(|(p, q)| p != q).map_or(0, |(p, q)| (*p as i64 - *q as i64) as u64)
                }
                "strlen" => cstr(a0).len() as u64,
                "strcmp" => match cstr(a0).cmp(&cstr(a1)) {
                    std::cmp::Ordering::Less => u64::MAX,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                },
                "strncmp" => {
                    let n = a2 as usize;
                    let s0 = cstr(a0);
                    let s1 = cstr(a1);
                    let b0 = &s0.as_bytes()[..s0.len().min(n)];
                    let b1 = &s1.as_bytes()[..s1.len().min(n)];
                    match b0.cmp(b1) {
                        std::cmp::Ordering::Less => u64::MAX,
                        std::cmp::Ordering::Equal => 0,
                        std::cmp::Ordering::Greater => 1,
                    }
                }
                "strncpy" => {
                    let s = cstr(a1);
                    let n = a2 as usize;
                    let copy = s.len().min(n);
                    std::ptr::copy_nonoverlapping(s.as_ptr(), a0 as *mut u8, copy);
                    if copy < n {
                        std::ptr::write_bytes((a0 as *mut u8).add(copy), 0, n - copy);
                    }
                    a0
                }
                "puts" => {
                    let s = cstr(a0);
                    with(|r| {
                        r.output.push_str(&s);
                        r.output.push('\n');
                    });
                    1
                }
                "printf" => {
                    let s = format(&cstr(a0), &args[1..]);
                    let n = s.len() as u64;
                    with(|r| r.output.push_str(&s));
                    n
                }
                "sprintf" => {
                    let s = format(&cstr(a1), &args[2..]);
                    std::ptr::copy_nonoverlapping(s.as_ptr(), a0 as *mut u8, s.len());
                    *(a0 as *mut u8).add(s.len()) = 0;
                    s.len() as u64
                }
                "snprintf" => {
                    let s = format(&cstr(a2), &args[3..]);
                    let n = s.len().min((a1 as usize).saturating_sub(1));
                    if a1 > 0 {
                        std::ptr::copy_nonoverlapping(s.as_ptr(), a0 as *mut u8, n);
                        *(a0 as *mut u8).add(n) = 0;
                    }
                    s.len() as u64
                }
                // ---- Kernel Memory & Time ----
                "mmap" => valloc_guest(a1 as usize),
                "munmap" | "sceKernelMunmap" => 0,
                "sceKernelGetDirectMemorySize" => 4u64 << 30,
                "sceKernelAllocateDirectMemory" | "sceKernelAllocateMainDirectMemory" => {
                    if a4 != 0 {
                        *(a4 as *mut u64) = 0x1000_0000;
                    }
                    0
                }
                "sceKernelMapDirectMemory"
                | "sceKernelMapFlexibleMemory"
                | "sceKernelMapNamedFlexibleMemory" => {
                    if a0 != 0 {
                        let ptr = valloc_guest(a1 as usize);
                        *(a0 as *mut u64) = ptr;
                        if ptr != 0 { 0 } else { 0x8002_000C }
                    } else {
                        0x8002_0016
                    }
                }
                "sceKernelDebugOutText" => {
                    let s = cstr(a1);
                    with(|r| r.output.push_str(&s));
                    0
                }
                "sceKernelGetProcessTimeCounterFrequency" => 1_000_000,
                "sceKernelGetProcessTime"
                | "sceKernelGetProcessTimeCounter"
                | "sceKernelReadTsc" => std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_micros() as u64),
                "sceKernelGettimeofday" | "gettimeofday" => {
                    if a0 != 0 {
                        let us = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_micros() as u64);
                        *(a0 as *mut u64) = us / 1_000_000;
                        *((a0 + 8) as *mut u64) = us % 1_000_000;
                    }
                    0
                }
                "clock_gettime" => {
                    if a1 != 0 {
                        let ns = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_nanos() as u64);
                        *(a1 as *mut u64) = ns / 1_000_000_000;
                        *((a1 + 8) as *mut u64) = ns % 1_000_000_000;
                    }
                    0
                }
                "sceKernelUsleep" | "sceKernelSleep" | "scePthreadYield" | "sceKernelIsNeoMode" => 0,
                // ---- VideoOut ----
                "sceVideoOutOpen" => 1,
                "sceVideoOutClose" | "sceVideoOutSetFlipRate" | "sceVideoOutSetBufferAttribute" => {
                    if name == "sceVideoOutSetBufferAttribute" && a0 != 0 {
                        with(|r| {
                            r.video_width = a4 as u32;
                            r.video_height = a5 as u32;
                        });
                    }
                    0
                }
                "sceVideoOutRegisterBuffers" => {
                    let count = (a3 as usize).min(16);
                    if a2 != 0 {
                        let slice = std::slice::from_raw_parts(a2 as *const u64, count);
                        with(|r| r.video_buffers = slice.to_vec());
                    }
                    0
                }
                "sceVideoOutSubmitFlip" => {
                    with(|r| r.video_flips += 1);
                    0
                }
                "sceVideoOutGetFlipStatus" => {
                    if a1 != 0 {
                        let flips = with(|r| r.video_flips);
                        std::ptr::write_bytes(a1 as *mut u8, 0, 48);
                        *(a1 as *mut u64) = flips;
                    }
                    0
                }
                // ---- Pad / Audio / User / System / Pthreads / Sysmodule ----
                "scePadInit" | "scePadClose" => 0,
                "scePadOpen" => 1,
                "scePadReadState" | "scePadRead" => {
                    if a1 != 0 {
                        std::ptr::write_bytes(a1 as *mut u8, 0, 64);
                        *(a1 as *mut u32) = with(|r| r.pad_buttons);
                        *((a1 + 4) as *mut u8) = 128; // LX center
                        *((a1 + 5) as *mut u8) = 128; // LY center
                        *((a1 + 6) as *mut u8) = 128; // RX center
                        *((a1 + 7) as *mut u8) = 128; // RY center
                        *((a1 + 16) as *mut u8) = 1; // connected
                    }
                    0
                }
                "sceAudioOutInit" | "sceAudioOutClose" | "sceAudioOutSetVolume" => 0,
                "sceAudioOutOpen" => 1,
                "sceAudioOutOutput" => 256,
                "sceUserServiceInitialize"
                | "sceUserServiceTerminate"
                | "sceSystemServiceHideSplashScreen"
                | "sceSysmoduleLoadModule"
                | "sceSysmoduleLoadModuleInternal" => 0,
                "sceUserServiceGetInitialUser" => {
                    if a0 != 0 {
                        *(a0 as *mut i32) = 1;
                    }
                    0
                }
                "sceUserServiceGetUserName" => {
                    if a1 != 0 && a2 >= 8 {
                        let name = b"Player1\0";
                        std::ptr::copy_nonoverlapping(name.as_ptr(), a1 as *mut u8, name.len());
                    }
                    0
                }
                "sceSystemServiceGetStatus" => {
                    if a0 != 0 {
                        std::ptr::write_bytes(a0 as *mut u8, 0, 32);
                    }
                    0
                }
                "sceSystemServiceParamGetInt" => {
                    if a1 != 0 {
                        *(a1 as *mut i32) = 1; // EN-US / default
                    }
                    0
                }
                "scePthreadMutexInit"
                | "scePthreadMutexDestroy"
                | "scePthreadMutexLock"
                | "scePthreadMutexUnlock"
                | "scePthreadMutexTrylock"
                | "pthread_mutex_init"
                | "pthread_mutex_destroy"
                | "pthread_mutex_lock"
                | "pthread_mutex_unlock" => 0,
                other => {
                    with(|r| {
                        if !r.unimplemented.iter().any(|u| u == other) {
                            r.unimplemented.push(other.to_string());
                        }
                    });
                    0
                }
            }
        }
    }

    /// Rewrites `fs:[0]` (ELF Variant II TLS base) and `fs:[0x28]` (stack canary) 9-byte
    /// x86-64 instructions in-place into RIP-relative loads/checks pointing at our appended
    /// guest TCB page (`tcb_va`). Every rewrite is 9 bytes exact (7-byte RIP-relative + 2 NOPs),
    /// preserving all branch targets and instruction boundaries cleanly.
    unsafe fn relink_fs_tls(code_ptr: *mut u8, code_len: usize, tcb_va: u64) -> usize {
        let mut patches = 0usize;
        let code = std::slice::from_raw_parts_mut(code_ptr, code_len);
        let mut i = 0usize;
        while i + 9 <= code.len() {
            // Pattern: 64 (48|4C) (8B|33) <modrm with mod=00, rm=100 (SIB)> 25 <disp32 = 0x00 or 0x28>
            let b0 = code[i];
            let b1 = code[i + 1];
            let b2 = code[i + 2];
            let modrm = code[i + 3];
            let sib = code[i + 4];
            if b0 == 0x64
                && (b1 == 0x48 || b1 == 0x4C)
                && (b2 == 0x8B || b2 == 0x33)
                && (modrm & 0xC7) == 0x04
                && sib == 0x25
            {
                let disp = u32::from_le_bytes([code[i + 5], code[i + 6], code[i + 7], code[i + 8]]);
                let insn_va = code_ptr as u64 + i as u64;
                let rip_next = insn_va + 7; // 7-byte rewritten instruction
                let reg_bits = modrm & 0x38;
                if b2 == 0x8B && disp == 0 {
                    // mov r64, qword ptr fs:[0] -> lea r64, [rip + (tcb_va - rip_next)]; nop; nop
                    let rel = (tcb_va as i64).wrapping_sub(rip_next as i64);
                    if let Ok(rel32) = i32::try_from(rel) {
                        code[i] = b1;
                        code[i + 1] = 0x8D; // LEA
                        code[i + 2] = reg_bits | 0x05; // [rip + disp32]
                        code[i + 3..i + 7].copy_from_slice(&rel32.to_le_bytes());
                        code[i + 7] = 0x90;
                        code[i + 8] = 0x90;
                        patches += 1;
                        i += 9;
                        continue;
                    }
                } else if (b2 == 0x8B || b2 == 0x33) && disp == 0x28 {
                    // mov/xor r64, qword ptr fs:[0x28] -> mov/xor r64, [rip + (canary_va - rip_next)]; nop; nop
                    let canary_va = tcb_va + 0x28;
                    let rel = (canary_va as i64).wrapping_sub(rip_next as i64);
                    if let Ok(rel32) = i32::try_from(rel) {
                        code[i] = b1;
                        code[i + 1] = b2; // MOV (0x8B) or XOR (0x33)
                        code[i + 2] = reg_bits | 0x05; // [rip + disp32]
                        code[i + 3..i + 7].copy_from_slice(&rel32.to_le_bytes());
                        code[i + 7] = 0x90;
                        code[i + 8] = 0x90;
                        patches += 1;
                        i += 9;
                        continue;
                    }
                }
            }
            i += 1;
        }
        patches
    }

    /// Thunk for one import: System V arguments (rdi rsi rdx rcx r8 r9) -> `native_call(idx, ...)`.
    fn thunk(idx: u32) -> [u8; 64] {
        let mut c = vec![
            0x48, 0x83, 0xEC, 0x48, // sub rsp, 0x48
            0x48, 0x89, 0x4C, 0x24, 0x20, // mov [rsp+20h], rcx
            0x4C, 0x89, 0x44, 0x24, 0x28, // mov [rsp+28h], r8
            0x4C, 0x89, 0x4C, 0x24, 0x30, // mov [rsp+30h], r9
            0x49, 0x89, 0xD1, // mov r9, rdx
            0x49, 0x89, 0xF0, // mov r8, rsi
            0x48, 0x89, 0xFA, // mov rdx, rdi
            0xB9, // mov ecx, idx
        ];
        c.extend_from_slice(&idx.to_le_bytes());
        c.extend_from_slice(&[0x48, 0xB8]); // mov rax, native_call
        c.extend_from_slice(&(native_call as *const () as usize as u64).to_le_bytes());
        c.extend_from_slice(&[0xFF, 0xD0, 0x48, 0x83, 0xC4, 0x48, 0xC3]); // call rax; add rsp,0x48; ret
        let mut out = [0xCCu8; 64];
        out[..c.len()].copy_from_slice(&c);
        out
    }

    /// Windows-to-System-V entry: calls entry(rdi = args, rsi = exit function) and remembers
    /// the stack position in `slot` so an exit call can unwind straight back out of the program.
    fn entry_code(slot: u64) -> Vec<u8> {
        let mut c = vec![0x57, 0x56, 0x48, 0x83, 0xEC, 0x28, 0x49, 0xB9]; // push rdi; push rsi; sub rsp,0x28; mov r9, slot
        c.extend_from_slice(&slot.to_le_bytes());
        c.extend_from_slice(&[0x49, 0x89, 0x21]); // mov [r9], rsp
        c.extend_from_slice(&[0x48, 0x89, 0xD0, 0x48, 0x89, 0xCF, 0x4C, 0x89, 0xC6, 0xFF, 0xD0]); // rax=rdx; rdi=rcx; rsi=r8; call rax
        c.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28, 0x5E, 0x5F, 0xC3]); // add rsp,0x28; pop rsi; pop rdi; ret
        c
    }

    /// An exit call: record the exit through the normal thunk, then return from `entry_code`.
    fn exit_code(thunk_at: u64, slot: u64) -> Vec<u8> {
        let mut c = vec![0x48, 0x83, 0xEC, 0x08, 0x48, 0xB8]; // sub rsp,8; mov rax, thunk
        c.extend_from_slice(&thunk_at.to_le_bytes());
        c.extend_from_slice(&[0xFF, 0xD0, 0x48, 0xB8]); // call rax; mov rax, slot
        c.extend_from_slice(&slot.to_le_bytes());
        c.extend_from_slice(&[0x48, 0x8B, 0x20, 0x48, 0x83, 0xC4, 0x28, 0x5E, 0x5F, 0xC3]); // mov rsp,[rax]; add rsp,0x28; pop rsi; pop rdi; ret
        c
    }

    fn sym_addr(sym: u32, base: u64, elf: &Elf, stubs: &[u64]) -> u64 {
        match elf.symbols.get(sym as usize) {
            Some(s) if s.defined => base + s.value,
            Some(_) => stubs.get(sym as usize).copied().unwrap_or(0),
            None => 0,
        }
    }

    pub fn run(file: &[u8]) -> Result<NativeResult, String> {
        let _guard = RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let image = crate::image::program_image(file)?;
        let file = image.as_ref();
        let elf = Elf::parse(file)?;
        let loads: Vec<_> = elf.segments.iter().filter(|s| s.is_loadable()).collect();
        let min_va = loads.iter().map(|s| s.vaddr).min().ok_or("The program has nothing to load.")?;
        let max_va = loads.iter().map(|s| s.vaddr + s.mem_size).max().unwrap_or(0);
        let pref_base = if min_va < 0x40_0000 { 0x40_0000 - min_va } else { 0 };
        let pref_start = (pref_base + min_va) & !0xFFFF;
        let image_span = ((pref_base + max_va + 0xFFFF) & !0xFFFF) - pref_start;

        // Append a dedicated TLS + TCB block inside the same VirtualAlloc so RIP-relative
        // `fs:` relinker rewrites always stay within ±2 GiB of every executable segment.
        let tls_seg = elf.segments.iter().find(|s| s.kind == "tls");
        let tls_mem_size = tls_seg.map(|s| ((s.mem_size + 15) & !15).max(16)).unwrap_or(0);
        let tls_region_size = ((tls_mem_size + 0x2000) & !0xFFF).max(0x2000);
        let total_len = image_span + tls_region_size;

        let mut image = unsafe { VirtualAlloc(pref_start as *mut u8, total_len as usize, COMMIT_RESERVE, RWX) };
        let base = if !image.is_null() && image as u64 == pref_start {
            pref_base
        } else {
            if !image.is_null() {
                unsafe { VirtualFree(image, 0, RELEASE) };
            }
            image = unsafe { VirtualAlloc(std::ptr::null_mut(), total_len as usize, COMMIT_RESERVE, RWX) };
            if image.is_null() {
                return Err(format!("Couldn't allocate {total_len} bytes for native PlayStation image."));
            }
            (image as u64).wrapping_sub(min_va & !0xFFFF)
        };
        let start = image as u64;

        for s in &loads {
            let data = file.get(s.offset as usize..(s.offset + s.file_size) as usize).ok_or("A segment runs past the end of the file.")?;
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), (base + s.vaddr) as *mut u8, data.len()) };
        }

        // Populate Variant II TLS template right below `tcb_va` and write self-pointer + stack canary.
        let tcb_va = start + image_span + tls_mem_size + 0x1000;
        if let Some(tls) = tls_seg {
            if tls.file_size > 0 {
                let data = file.get(tls.offset as usize..(tls.offset + tls.file_size) as usize).ok_or("TLS segment runs past end of file.")?;
                let dst = (tcb_va - tls_mem_size) as *mut u8;
                unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len()) };
            }
        }
        unsafe {
            *(tcb_va as *mut u64) = tcb_va; // fs:[0] = TCB self pointer
            *((tcb_va + 0x28) as *mut u64) = 0x5A5A_C3C3_9696_6969; // fs:[0x28] = stack guard canary
        }

        // Rewrite `fs:[0]` and `fs:[0x28]` instructions inside all executable segments.
        let mut tls_patches = 0usize;
        for s in &loads {
            if (s.flags & 1) != 0 && s.file_size > 0 {
                tls_patches += unsafe { relink_fs_tls((base + s.vaddr) as *mut u8, s.file_size as usize, tcb_va) };
            }
        }

        // Import stubs: 128 bytes each (the plain thunk, plus the exit variant where needed).
        let mut names: Vec<String> = Vec::new();
        let mut stubs = vec![0u64; elf.symbols.len()];
        let total = elf.symbols.len() + 3;
        let stub_mem = unsafe { VirtualAlloc(std::ptr::null_mut(), total * 128 + 128, COMMIT_RESERVE, RWX) };
        if stub_mem.is_null() {
            unsafe { VirtualFree(image, 0, RELEASE) };
            return Err("Out of memory.".into());
        }
        let slot = stub_mem as u64 + (total as u64) * 128;
        let place = |names: &mut Vec<String>, name: String| -> u64 {
            let idx = names.len();
            let at = stub_mem as u64 + idx as u64 * 128;
            let plain = thunk(idx as u32);
            unsafe {
                if matches!(name.as_str(), "exit" | "_exit" | "abort" | "__stack_chk_fail") {
                    std::ptr::copy_nonoverlapping(plain.as_ptr(), (at + 64) as *mut u8, 64);
                    let code = exit_code(at + 64, slot);
                    std::ptr::copy_nonoverlapping(code.as_ptr(), at as *mut u8, code.len());
                } else {
                    std::ptr::copy_nonoverlapping(plain.as_ptr(), at as *mut u8, 64);
                }
            }
            names.push(name);
            at
        };
        for (i, sym) in elf.symbols.iter().enumerate() {
            if sym.defined || i == 0 {
                continue;
            }
            let nid = sym.name.split('#').next().unwrap_or("");
            stubs[i] = place(&mut names, crate::nid::lookup(nid).map(|n| n.to_string()).unwrap_or_else(|| nid.to_string()));
        }
        let exit_at = place(&mut names, "exit".into());
        let tramp = stub_mem as u64 + names.len() as u64 * 128;
        let code = entry_code(slot);
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), tramp as *mut u8, code.len()) };

        for r in &elf.relocs {
            let Reloc { offset, kind, sym, addend } = *r;
            let s = sym_addr(sym, base, &elf, &stubs);
            let v = match kind {
                8 => Some((base as i64 + addend) as u64),
                1 => Some((s as i64 + addend) as u64),
                6 | 7 => Some(s),
                18 => {
                    // R_X86_64_TPOFF64: negative offset from TCB
                    let sym_off = elf.symbols.get(sym as usize).map(|x| x.value as i64).unwrap_or(0);
                    Some((sym_off + addend - tls_mem_size as i64) as u64)
                }
                _ => None,
            };
            if let Some(v) = v {
                let at = base + offset;
                if at < start || at + 8 > start + image_span {
                    continue;
                }
                unsafe { *(at as *mut u64) = v };
            }
        }

        // Start-up block: argc, argv[0], terminators.
        let name: &'static [u8] = b"eboot.bin\0";
        let args: &'static mut [u64; 5] = Box::leak(Box::new([1, name.as_ptr() as u64, 0, 0, 0]));
        let (args_p, entry_p) = (args.as_ptr() as u64, base + elf.entry);

        *RUN.lock().unwrap_or_else(|e| e.into_inner()) = Some(Run {
            names,
            output: String::new(),
            exit_code: None,
            unimplemented: Vec::new(),
            video_flips: 0,
            video_buffers: Vec::new(),
            video_width: 0,
            video_height: 0,
            direct_allocs: Vec::new(),
            pad_buttons: 0,
        });
        let t = std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(move || {
                let f: extern "system" fn(u64, u64, u64) -> u64 = unsafe { std::mem::transmute(tramp as usize) };
                let rc = f(args_p, entry_p, exit_at);
                with(|r| {
                    if r.exit_code.is_none() {
                        r.exit_code = Some(rc as i32);
                    }
                });
            })
            .map_err(|e| e.to_string())?;
        let _ = t.join();
        let r = RUN.lock().unwrap_or_else(|e| e.into_inner()).take().expect("run state");
        unsafe {
            for (ptr, _) in r.direct_allocs {
                VirtualFree(ptr as *mut u8, 0, RELEASE);
            }
            VirtualFree(image, 0, RELEASE);
            VirtualFree(stub_mem, 0, RELEASE);
        }
        Ok(NativeResult {
            output: r.output,
            exit_code: r.exit_code.unwrap_or(0),
            tls_patches,
            video_flips: r.video_flips,
            unimplemented: r.unimplemented,
        })
    }
}

#[cfg(all(test, windows, target_arch = "x86_64"))]
mod tests {
    #[test]
    fn hello_program_runs_natively() {
        let r = super::run(&crate::machine::tests::hello_program()).unwrap();
        assert_eq!(r.output, "hello from PS5\n");
        assert_eq!(r.exit_code, 7);
    }

    #[test]
    fn relinker_rewrites_fs_tls_and_stack_canary_natively() {
        // Build a minimal PS5 ELF with a PT_TLS segment containing u64(0x2A) at offset -16,
        // and code that reads `fs:[0]` (TLS base), loads `[rax - 16]`, checks `fs:[0x28]`
        // stack canary round-trip, and returns 0x2A!
        let mut elf = vec![0u8; 0x3000];
        elf[0..4].copy_from_slice(b"\x7FELF");
        elf[4] = 2; // 64-bit
        elf[5] = 1; // LE
        elf[6] = 1;
        elf[16..18].copy_from_slice(&0xFE10u16.to_le_bytes()); // ET_SCE_DYNEXEC
        elf[18..20].copy_from_slice(&62u16.to_le_bytes()); // x86-64
        elf[24..32].copy_from_slice(&0x1000u64.to_le_bytes()); // entry
        elf[32..40].copy_from_slice(&64u64.to_le_bytes()); // phoff
        elf[54..56].copy_from_slice(&56u16.to_le_bytes()); // phentsize
        elf[56..58].copy_from_slice(&2u16.to_le_bytes()); // phnum = 2 (PT_LOAD + PT_TLS)

        // PHDR 0: PT_LOAD (offset 0x1000, vaddr 0x1000, size 0x1000, flags PF_R|PF_X=5)
        let p0 = 64;
        elf[p0..p0 + 4].copy_from_slice(&1u32.to_le_bytes());
        elf[p0 + 4..p0 + 8].copy_from_slice(&5u32.to_le_bytes());
        elf[p0 + 8..p0 + 16].copy_from_slice(&0x1000u64.to_le_bytes());
        elf[p0 + 16..p0 + 24].copy_from_slice(&0x1000u64.to_le_bytes());
        elf[p0 + 32..p0 + 40].copy_from_slice(&0x1000u64.to_le_bytes());
        elf[p0 + 40..p0 + 48].copy_from_slice(&0x1000u64.to_le_bytes());

        // PHDR 1: PT_TLS (type=7, offset 0x2000, file_size 16, mem_size 16)
        let p1 = 64 + 56;
        elf[p1..p1 + 4].copy_from_slice(&7u32.to_le_bytes());
        elf[p1 + 4..p1 + 8].copy_from_slice(&4u32.to_le_bytes());
        elf[p1 + 8..p1 + 16].copy_from_slice(&0x2000u64.to_le_bytes());
        elf[p1 + 16..p1 + 24].copy_from_slice(&0x2000u64.to_le_bytes());
        elf[p1 + 32..p1 + 40].copy_from_slice(&16u64.to_le_bytes());
        elf[p1 + 40..p1 + 48].copy_from_slice(&16u64.to_le_bytes());

        // TLS template data at offset 0x2000: u64 = 42
        elf[0x2000..0x2008].copy_from_slice(&42u64.to_le_bytes());

        // Code at offset 0x1000:
        //   mov rcx, qword ptr fs:[0x28]   ; load stack canary (9 bytes)
        //   mov rax, qword ptr fs:[0]      ; load TCB base (9 bytes)
        //   mov eax, dword ptr [rax - 16]  ; read TLS variable (42)
        //   xor rcx, qword ptr fs:[0x28]   ; verify canary == 0 (9 bytes)
        //   add rax, rcx                   ; stays 42 iff canary matched!
        //   ret
        let code = [
            0x64, 0x48, 0x8B, 0x0C, 0x25, 0x28, 0x00, 0x00, 0x00, // mov rcx, fs:[0x28]
            0x64, 0x48, 0x8B, 0x04, 0x25, 0x00, 0x00, 0x00, 0x00, // mov rax, fs:[0]
            0x8B, 0x40, 0xF0,                                     // mov eax, [rax - 16]
            0x64, 0x48, 0x33, 0x0C, 0x25, 0x28, 0x00, 0x00, 0x00, // xor rcx, fs:[0x28]
            0x48, 0x01, 0xC8,                                     // add rax, rcx
            0xC3,                                                 // ret
        ];
        elf[0x1000..0x1000 + code.len()].copy_from_slice(&code);

        let r = super::run(&elf).expect("PS5 ELF with PT_TLS and stack canary must relink and run natively");
        assert_eq!(r.tls_patches, 3, "should rewrite all 3 fs: instructions");
        assert_eq!(r.exit_code, 42);
    }
}
