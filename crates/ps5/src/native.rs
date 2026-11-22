//! Native mode: instead of interpreting the program on our CPU core, map it into this
//! process and run its machine code directly on the PC's processor, with every call into a
//! Sony system library redirected to our own replacement written in Rust. This is the same
//! idea as AnyPS5, built from scratch; it is much faster than emulating, but it only works
//! where the host is x86-64 (Windows here), and the program must not use the `fs:` segment
//! (thread-local storage), which Windows reserves. The relinker that rewrites such code
//! isn't built yet, so those programs are refused with a clear message.
//!
//! Safety: the program's code runs with this app's rights, with no sandbox. It is only
//! started when the player picks Native mode and opens a file, and only for a decrypted
//! x86-64 PlayStation ELF that `Elf::parse` accepts.

/// What a native run produced.
#[derive(Debug, Default)]
pub struct NativeResult {
    pub output: String,
    pub exit_code: i32,
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
    }
    static RUN: Mutex<Option<Run>> = Mutex::new(None);

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
        // A 16-byte header remembers the size so realloc and free work.
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
                "memalign" => alloc(a1 as usize),
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
                "sceKernelDebugOutText" => {
                    let s = cstr(a1);
                    with(|r| r.output.push_str(&s));
                    0
                }
                "sceKernelGetProcessTimeCounterFrequency" => 1_000_000,
                "sceKernelGetProcessTime" | "sceKernelGetProcessTimeCounter" | "sceKernelReadTsc" => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64),
                "sceKernelUsleep" | "sceKernelSleep" | "scePthreadYield" | "sceKernelIsNeoMode" => 0,
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
        let elf = Elf::parse(file)?;
        if elf.segments.iter().any(|s| s.kind == "tls") {
            return Err("This program uses thread-local storage through the fs segment, which Windows keeps for itself. Native mode needs the relinker to rewrite that code first, and it isn't built yet. Use Emulator mode.".into());
        }
        let loads: Vec<_> = elf.segments.iter().filter(|s| s.is_loadable()).collect();
        let min_va = loads.iter().map(|s| s.vaddr).min().ok_or("The program has nothing to load.")?;
        let max_va = loads.iter().map(|s| s.vaddr + s.mem_size).max().unwrap_or(0);
        let base = if min_va < 0x40_0000 { 0x40_0000 - min_va } else { 0 };
        let start = (base + min_va) & !0xFFFF;
        let len = ((base + max_va + 0xFFFF) & !0xFFFF) - start;
        let image = unsafe { VirtualAlloc(start as *mut u8, len as usize, COMMIT_RESERVE, RWX) };
        if image as u64 != start {
            return Err(format!("Couldn't reserve the program's fixed address {start:#x} on this PC (something else is using it)."));
        }
        for s in &loads {
            let data = file.get(s.offset as usize..(s.offset + s.file_size) as usize).ok_or("A segment runs past the end of the file.")?;
            unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), (base + s.vaddr) as *mut u8, data.len()) };
        }

        // Import stubs: 128 bytes each (the plain thunk, plus the exit variant where needed).
        let mut names: Vec<String> = Vec::new();
        let mut stubs = vec![0u64; elf.symbols.len()];
        let total = elf.symbols.len() + 3;
        let stub_mem = unsafe { VirtualAlloc(std::ptr::null_mut(), total * 128 + 128, COMMIT_RESERVE, RWX) };
        if stub_mem.is_null() {
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
                _ => None,
            };
            if let Some(v) = v {
                let at = base + offset;
                if at < start || at + 8 > start + len {
                    continue;
                }
                unsafe { *(at as *mut u64) = v };
            }
        }

        // Start-up block: argc, argv[0], terminators.
        let name: &'static [u8] = b"eboot.bin\0";
        let args: &'static mut [u64; 5] = Box::leak(Box::new([1, name.as_ptr() as u64, 0, 0, 0]));
        let (args_p, entry_p) = (args.as_ptr() as u64, base + elf.entry);

        *RUN.lock().unwrap_or_else(|e| e.into_inner()) = Some(Run { names, output: String::new(), exit_code: None, unimplemented: Vec::new() });
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
            VirtualFree(image, 0, RELEASE);
            VirtualFree(stub_mem, 0, RELEASE);
        }
        Ok(NativeResult { output: r.output, exit_code: r.exit_code.unwrap_or(0), unimplemented: r.unimplemented })
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
}
