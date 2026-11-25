//! Native mode for Xbox One / Series X|S: instead of interpreting the game's machine code on our
//! own CPU core, map it into this process and let it run directly on the PC's x86-64 processor.
//!
//! This works because an Xbox executable *is* a Windows PE32+ image using the Windows x64 calling
//! convention — the same one this PC uses. So the loader does what `LoadLibrary` does: copy the
//! sections in, apply relocations, then fill the import address table. The one Xbox-shaped part
//! is where those imports come from: `kernelx.dll` resolves out of the real `kernel32.dll`,
//! `d3d12_x.dll` out of `d3d12.dll`, and the Xbox-only libraries get the replacements in
//! [`crate::shims`].
//!
//! Safety: the game's code runs inside this process with this app's rights and no sandbox. It is
//! only reached when the player explicitly picks Native mode and opens a decrypted game.

/// What a native run produced.
#[derive(Debug, Default)]
pub struct NativeResult {
    /// Everything the game printed through the debug output we gave it.
    pub output: String,
    pub exit_code: i32,
    /// Libraries the game imported from, in the order the loader bound them.
    pub modules: Vec<String>,
    /// Xbox-only functions served by our own replacements.
    pub shimmed: Vec<String>,
    /// Imports nobody could supply: either an Xbox-only function we haven't written, or a name
    /// that exists on Xbox but not on Windows. Calling one of these would fault.
    pub unimplemented: Vec<String>,
    /// Whole libraries we could not provide at all.
    pub missing_modules: Vec<String>,
    /// How many import slots got a working address.
    pub resolved: usize,
}

#[derive(Debug, Default)]
pub struct NativeImports {
    pub resolved: usize,
    pub shimmed: Vec<String>,
    pub unimplemented: Vec<String>,
    pub missing_modules: Vec<String>,
}

impl NativeImports {
    pub fn is_complete(&self) -> bool {
        self.unimplemented.is_empty() && self.missing_modules.is_empty()
    }
}

impl NativeResult {
    /// True when nothing important was left unresolved.
    pub fn is_complete(&self) -> bool {
        self.unimplemented.is_empty() && self.missing_modules.is_empty()
    }
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
pub fn run(_file: &[u8]) -> Result<NativeResult, String> {
    Err("Native mode runs the game on this PC's own processor, so it needs 64-bit Windows. Use Emulator mode on this system.".into())
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
pub fn check_imports(_file: &[u8]) -> Result<NativeImports, String> {
    Err("Checking native Xbox imports requires 64-bit Windows.".into())
}

#[cfg(all(windows, target_arch = "x86_64"))]
pub use win::*;

#[cfg(all(windows, target_arch = "x86_64"))]
mod win {
    use super::{NativeImports, NativeResult};
    use crate::modules::{self, Provider};
    use crate::native_pages::PhysicalPages;
    use crate::pe::{Pe, PAGE_EXECUTE_READ, PAGE_READWRITE};
    use crate::shims::{XGAME_RUNTIME_DLL, XGAME_RUNTIME_EXPORTS};
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::Mutex;

    // The Windows loader calls we need. Declared by hand so this crate stays dependency-free.
    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(addr: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
        fn VirtualFree(addr: *mut c_void, size: usize, kind: u32) -> i32;
        fn VirtualProtect(addr: *mut c_void, size: usize, protect: u32, old: *mut u32) -> i32;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        fn GetModuleHandleA(name: *const u8) -> *mut c_void;
        fn GetModuleHandleW(name: *const u16) -> *mut c_void;
        fn LoadLibraryA(name: *const u8) -> *mut c_void;
        fn LoadLibraryW(name: *const u16) -> *mut c_void;
        fn LoadLibraryExA(name: *const u8, file: *mut c_void, flags: u32) -> *mut c_void;
        fn LoadLibraryExW(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
        fn SetLastError(error: u32);
        fn RtlAddFunctionTable(function_table: *mut c_void, entry_count: u32, base_address: u64) -> u8;
        fn RtlDeleteFunctionTable(function_table: *mut c_void) -> u8;
    }

    const MEM_COMMIT_RESERVE: u32 = 0x3000;
    const MEM_RELEASE: u32 = 0x8000;
    /// Alignment the Windows loader gives every section.
    const SECTION_ALIGN: u64 = 0x1000;
    const RUNTIME_HANDLE: usize = 0xFFFF_FFFF_FFFD_0000;
    const XBOX_SHIM_HANDLE: usize = 0xFFFF_FFFF_FFFC_0000;

    /// State shared with the shim dispatcher while a game is running.
    #[derive(Default)]
    struct Run {
        modules: Vec<String>,
        shimmed: Vec<String>,
        unimplemented: Vec<String>,
        missing: Vec<String>,
        output: String,
        exit_code: Option<i32>,
        resolved: usize,
        runtime_refs: u32,
        physical_pages: PhysicalPages,
    }

    impl Run {
        fn note(&mut self, what: String) {
            if !self.unimplemented.contains(&what) {
                self.unimplemented.push(what);
            }
        }
    }

    static RUN: Mutex<Option<Run>> = Mutex::new(None);

/// Only one game may be loaded at a time: the image base, the captured output and the shim table
/// are all process-wide, exactly as they are for a real console running one title.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn with<R>(f: impl FnOnce(&mut Run) -> R) -> R {
        let mut g = RUN.lock().unwrap_or_else(|e| e.into_inner());
        f(g.as_mut().expect("a native run is active"))
    }

    pub(crate) fn with_physical_pages<R>(f: impl FnOnce(&mut PhysicalPages) -> R) -> R {
        static STANDALONE: std::sync::OnceLock<Mutex<PhysicalPages>> = std::sync::OnceLock::new();
        let mut state = RUN.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(run) = state.as_mut() {
            return f(&mut run.physical_pages);
        }
        drop(state);
        let mut pages = STANDALONE.get_or_init(Mutex::default).lock().unwrap_or_else(|e| e.into_inner());
        f(&mut pages)
    }

    /// The libraries a running game pulled in, so a shim can tell where a call came from.
    pub fn modules_loaded() -> Vec<String> {
        RUN.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|r| r.modules.clone()).unwrap_or_default()
    }

    /// Appends a line to the game's captured output, as if it went to the console.
    pub fn print(s: &str) {
        print_on_this_thread(s);
    }

    /// Appends a line to the game's captured output, as if it went to the console. Safe to call
    /// from any shim running on the game's thread.
    pub fn print_on_this_thread(s: &str) {
        with(|r| {
            r.output.push_str(s);
            if !s.ends_with('\n') {
                r.output.push('\n');
            }
        });
    }

    fn wide(name: &str) -> Vec<u16> {
        name.encode_utf16().chain(Some(0)).collect()
    }

    unsafe fn name_matches(address: usize, wide: bool, expected: &str, ignore_case: bool) -> bool {
        address != 0 && expected.bytes().chain(Some(0)).enumerate().all(|(index, byte)| {
            let value = if wide {
                unsafe { (address as *const u16).add(index).read_unaligned() }
            } else {
                u16::from(unsafe { (address as *const u8).add(index).read() })
            };
            if ignore_case && value <= 0x7f {
                (value as u8).eq_ignore_ascii_case(&byte)
            } else {
                value == u16::from(byte)
            }
        })
    }

    unsafe fn is_runtime_name(address: usize, wide: bool) -> bool {
        unsafe { name_matches(address, wide, XGAME_RUNTIME_DLL, true)
            || name_matches(address, wide, "xgameruntime", true) }
    }

    fn runtime_handle(load: bool) -> *mut c_void {
        let loaded = with(|run| {
            if load {
                run.runtime_refs = run.runtime_refs.saturating_add(1);
            }
            run.runtime_refs != 0
        });
        if loaded {
            RUNTIME_HANDLE as *mut c_void
        } else {
            unsafe { SetLastError(126) };
            std::ptr::null_mut()
        }
    }

    unsafe fn read_guest_name(address: usize, wide: bool) -> String {
        if address == 0 {
            return String::new();
        }
        let mut out = String::new();
        for index in 0..260usize {
            let v = if wide {
                unsafe { (address as *const u16).add(index).read_unaligned() }
            } else {
                u16::from(unsafe { (address as *const u8).add(index).read() })
            };
            if v == 0 {
                break;
            }
            if let Some(ch) = char::from_u32(u32::from(v)) {
                out.push(ch);
            }
        }
        out
    }

    /// Maps an Xbox console DLL name passed to `LoadLibrary` / `GetModuleHandle` into either
    /// its Windows counterpart or our virtual Xbox console shim handle.
    fn resolve_console_dll(raw_name: &str, load: bool) -> Option<*mut c_void> {
        let base = raw_name
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(raw_name)
            .to_ascii_lowercase();
        let with_ext = if base.ends_with(".dll") { base } else { format!("{base}.dll") };
        if with_ext == XGAME_RUNTIME_DLL {
            return Some(runtime_handle(load));
        }
        match modules::provider(&with_ext) {
            Some(Provider::Windows(win_dll)) if !win_dll.eq_ignore_ascii_case(&with_ext) => {
                let w = wide(win_dll);
                let h = if load {
                    unsafe { LoadLibraryW(w.as_ptr()) }
                } else {
                    unsafe { GetModuleHandleW(w.as_ptr()) }
                };
                if !h.is_null() {
                    return Some(h);
                }
                Some(XBOX_SHIM_HANDLE as *mut c_void)
            }
            Some(Provider::Shim { .. }) => Some(XBOX_SHIM_HANDLE as *mut c_void),
            _ => None,
        }
    }

    unsafe extern "system" fn guest_get_module_handle_a(name: *const u8) -> *mut c_void {
        if unsafe { is_runtime_name(name as usize, false) } {
            runtime_handle(false)
        } else {
            let s = unsafe { read_guest_name(name as usize, false) };
            if let Some(h) = resolve_console_dll(&s, false) {
                return h;
            }
            unsafe { GetModuleHandleA(name) }
        }
    }

    unsafe extern "system" fn guest_get_module_handle_w(name: *const u16) -> *mut c_void {
        if unsafe { is_runtime_name(name as usize, true) } {
            runtime_handle(false)
        } else {
            let s = unsafe { read_guest_name(name as usize, true) };
            if let Some(h) = resolve_console_dll(&s, false) {
                return h;
            }
            unsafe { GetModuleHandleW(name) }
        }
    }

    unsafe extern "system" fn guest_load_library_a(name: *const u8) -> *mut c_void {
        if unsafe { is_runtime_name(name as usize, false) } {
            runtime_handle(true)
        } else {
            let s = unsafe { read_guest_name(name as usize, false) };
            if let Some(h) = resolve_console_dll(&s, true) {
                return h;
            }
            unsafe { LoadLibraryA(name) }
        }
    }

    unsafe extern "system" fn guest_load_library_w(name: *const u16) -> *mut c_void {
        if unsafe { is_runtime_name(name as usize, true) } {
            runtime_handle(true)
        } else {
            let s = unsafe { read_guest_name(name as usize, true) };
            if let Some(h) = resolve_console_dll(&s, true) {
                return h;
            }
            unsafe { LoadLibraryW(name) }
        }
    }

    fn load_runtime_ex(file: *mut c_void, flags: u32) -> *mut c_void {
        if file.is_null() && flags == 0 {
            runtime_handle(true)
        } else {
            unsafe { SetLastError(87) };
            std::ptr::null_mut()
        }
    }

    unsafe extern "system" fn guest_load_library_ex_a(name: *const u8, file: *mut c_void, flags: u32) -> *mut c_void {
        if unsafe { is_runtime_name(name as usize, false) } {
            load_runtime_ex(file, flags)
        } else {
            let s = unsafe { read_guest_name(name as usize, false) };
            if let Some(h) = resolve_console_dll(&s, true) {
                return h;
            }
            unsafe { LoadLibraryExA(name, file, flags) }
        }
    }

    unsafe extern "system" fn guest_load_library_ex_w(name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void {
        if unsafe { is_runtime_name(name as usize, true) } {
            load_runtime_ex(file, flags)
        } else {
            let s = unsafe { read_guest_name(name as usize, true) };
            if let Some(h) = resolve_console_dll(&s, true) {
                return h;
            }
            unsafe { LoadLibraryExW(name, file, flags) }
        }
    }

    unsafe extern "system" fn guest_get_proc_address(module: *mut c_void, name: *const u8) -> *mut c_void {
        if module as usize == XBOX_SHIM_HANDLE {
            if name as usize > u16::MAX as usize {
                let sym = unsafe { read_guest_name(name as usize, false) };
                if let Some(address) = crate::shims::resolve(&sym) {
                    with(|run| {
                        if !run.shimmed.iter().any(|n| n == &sym) {
                            run.shimmed.push(sym.clone());
                        }
                    });
                    return address;
                }
            }
            unsafe { SetLastError(127) };
            return std::ptr::null_mut();
        }
        if module as usize != RUNTIME_HANDLE {
            let address = unsafe { GetProcAddress(module, name) };
            if !address.is_null() {
                return if address == VirtualFree as *mut c_void {
                    crate::native_pages::guest_virtual_free as *mut c_void
                } else {
                    address
                };
            }
            // Fallback: Xbox-specific extensions queried on redirected Windows DLLs (e.g. d3d12_xs -> d3d12.dll)
            if name as usize > u16::MAX as usize {
                let sym = unsafe { read_guest_name(name as usize, false) };
                if let Some(shim_addr) = crate::shims::resolve(&sym) {
                    with(|run| {
                        if !run.shimmed.iter().any(|n| n == &sym) {
                            run.shimmed.push(sym.clone());
                        }
                    });
                    return shim_addr;
                }
            }
            return std::ptr::null_mut();
        }
        if !with(|run| run.runtime_refs != 0) {
            unsafe { SetLastError(6) };
            return std::ptr::null_mut();
        }
        if name as usize > u16::MAX as usize {
            for &export in XGAME_RUNTIME_EXPORTS {
                if unsafe { name_matches(name as usize, false, export, false) } {
                    if let Some(address) = crate::shims::resolve(export) {
                        with(|run| {
                            if !run.shimmed.iter().any(|name| name == export) {
                                run.shimmed.push(export.to_owned());
                            }
                        });
                        return address;
                    }
                }
            }
        }
        unsafe { SetLastError(127) };
        std::ptr::null_mut()
    }

    unsafe extern "system" fn guest_free_library(module: *mut c_void) -> i32 {
        if module as usize == XBOX_SHIM_HANDLE {
            return 1;
        }
        if module as usize != RUNTIME_HANDLE {
            return unsafe { FreeLibrary(module) };
        }
        let released = with(|run| {
            if run.runtime_refs == 0 {
                return false;
            }
            run.runtime_refs -= 1;
            true
        });
        if !released {
            unsafe { SetLastError(6) };
        }
        i32::from(released)
    }

    fn guest_loader_export(name: &str) -> Option<*mut c_void> {
        Some(match name {
            "GetModuleHandleA" => guest_get_module_handle_a as *mut c_void,
            "GetModuleHandleW" => guest_get_module_handle_w as *mut c_void,
            "LoadLibraryA" => guest_load_library_a as *mut c_void,
            "LoadLibraryW" => guest_load_library_w as *mut c_void,
            "LoadLibraryExA" => guest_load_library_ex_a as *mut c_void,
            "LoadLibraryExW" => guest_load_library_ex_w as *mut c_void,
            "GetProcAddress" => guest_get_proc_address as *mut c_void,
            "FreeLibrary" => guest_free_library as *mut c_void,
            "VirtualFree" => crate::native_pages::guest_virtual_free as *mut c_void,
            _ => return None,
        })
    }

    /// Resolves one export by name, or by ordinal when the name starts with '#'.
    fn resolve(module: *mut c_void, symbol: &str) -> Option<*mut c_void> {
        if module.is_null() {
            return None;
        }
        if let Some(ord_str) = symbol.strip_prefix('#') {
            let ord: u16 = ord_str.parse().ok()?;
            let p = unsafe { GetProcAddress(module, ord as usize as *const u8) };
            return (!p.is_null()).then_some(p);
        }
        let buf = format!("{symbol}\0").into_bytes();
        let p = unsafe { GetProcAddress(module, buf.as_ptr()) };
        (!p.is_null()).then_some(p)
    }

    /// A mapping the game image lives in, released when it is dropped.
    struct Image {
        base: u64,
    }

    impl Image {
        /// Reserves `size` bytes at `wanted`, falling back to any OS-chosen address when the
        /// image carries base relocations (`.reloc`).
        fn reserve(wanted: u64, size: usize, allow_relocate: bool) -> Result<Image, String> {
            let p = unsafe { VirtualAlloc(wanted as *mut c_void, size, MEM_COMMIT_RESERVE, PAGE_READWRITE) };
            if !p.is_null() {
                if p as u64 == wanted || allow_relocate {
                    return Ok(Image { base: p as u64 });
                }
                unsafe { VirtualFree(p, 0, MEM_RELEASE) };
                return Err(format!(
                    "The game was built to load at {wanted:#x} without relocations, but this PC is already using that address."
                ));
            }
            if allow_relocate {
                let any = unsafe { VirtualAlloc(std::ptr::null_mut(), size, MEM_COMMIT_RESERVE, PAGE_READWRITE) };
                if !any.is_null() {
                    return Ok(Image { base: any as u64 });
                }
            }
            Err(format!(
                "Windows would not give the game {size:#x} bytes at {wanted:#x}. Close other programs and try again."
            ))
        }

        fn rva(&self, rva: u32) -> u64 {
            self.base + rva as u64
        }
    }

    impl Drop for Image {
        fn drop(&mut self) {
            unsafe { VirtualFree(self.base as *mut c_void, 0, MEM_RELEASE) };
        }
    }

    fn align_up(v: u64, to: u64) -> u64 {
        (v + to - 1) & !(to - 1)
    }

    /// Copies headers and sections into a writable mapping (protections are applied after
    /// relocations run so relocated pointers in `.text`/`.rdata` can be patched safely).
    fn copy_sections(pe: &Pe, data: &[u8], img: &Image) -> Result<(), String> {
        let headers = (pe.headers_size as usize).min(data.len());
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), img.base as *mut u8, headers) };
        for s in &pe.sections {
            let dst = img.rva(s.vaddr) as *mut u8;
            if s.raw_size > 0 {
                let from = s.raw_off as usize;
                let chunk = data.get(from..from + s.raw_size as usize).ok_or_else(|| format!("Section {} runs past the end of the file.", s.name))?;
                unsafe { std::ptr::copy_nonoverlapping(chunk.as_ptr(), dst, chunk.len()) };
            }
            // Whatever the file didn't cover is zeroed, which is how .bss works.
            let have = s.raw_size.min(s.vsize);
            if s.vsize > have {
                unsafe { std::ptr::write_bytes(dst.add(have as usize), 0, (s.vsize - have) as usize) };
            }
        }
        Ok(())
    }

    fn protect_sections(pe: &Pe, img: &Image) -> Result<(), String> {
        for s in &pe.sections {
            let dst = img.rva(s.vaddr) as *mut u8;
            let prot = s.protect();
            if s.vsize.max(s.raw_size) == 0 {
                continue;
            }
            if prot != PAGE_READWRITE {
                let mut old = 0;
                let len = align_up(s.vsize.max(s.raw_size) as u64, SECTION_ALIGN) as usize;
                if unsafe { VirtualProtect(dst as *mut c_void, len, prot, &mut old) } == 0 {
                    return Err(format!("Windows would not set section {} to the protection its header asks for.", s.name));
                }
            }
        }
        Ok(())
    }

    /// Rewrites the image's own addresses for the address it actually got. `delta` is the
    /// difference between where it was linked and where it landed.
    fn relocate(pe: &Pe, img: &Image, delta: i64) {
        if delta == 0 {
            return;
        }
        for (rva, kind) in &pe.relocs {
            // 10 is IMAGE_REL_BASED_DIR64, the only kind a 64-bit image uses.
            if *kind == 10 {
                let at = img.rva(*rva) as *mut i64;
                unsafe { *at = (*at).wrapping_add(delta) };
            }
        }
    }

    /// Fills the import address table: Windows libraries from the real DLL, Xbox-only libraries
    /// from our shims. Returns how many slots got a working address.
    fn bind_imports(pe: &Pe, img: Option<&Image>, out: &mut Run) -> usize {
        let mut filled = 0;
        // Handles stay loaded for the whole run so resolved addresses keep working.
        let mut handles: HashMap<String, *mut c_void> = HashMap::new();
        let write_slot = |rva: u32, address: *mut c_void| {
            if let Some(img) = img {
                unsafe { *(img.rva(rva) as *mut *mut c_void) = address };
            }
        };
        for module in &pe.imports {
            if !out.modules.contains(&module.dll) {
                out.modules.push(module.dll.clone());
            }
            match modules::provider(&module.dll) {
                Some(Provider::Windows(sys)) => {
                    let h = *handles.entry(sys.to_string()).or_insert_with(|| unsafe {
                        if img.is_some() {
                            LoadLibraryW(wide(sys).as_ptr())
                        } else {
                            const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x800;
                            LoadLibraryExW(wide(sys).as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32)
                        }
                    });
                    if h.is_null() {
                        if !out.missing.contains(&module.dll) {
                            out.missing.push(module.dll.clone());
                        }
                        continue;
                    }
                    for f in &module.functions {
                        // Xbox kernels and graphics DLLs carry a few console-specific exports
                        // alongside the standard Windows ones; check our shims when Windows doesn't
                        // export the symbol.
                        let name = f.name.trim_start_matches('_');
                        let guest_loader = (sys == "kernel32.dll").then(|| guest_loader_export(name)).flatten();
                        let p = guest_loader.or_else(|| resolve(h, &f.name)).or_else(|| resolve(h, name));
                        match p {
                            Some(p) => {
                                write_slot(f.slot, p);
                                filled += 1;
                            }
                            None => match crate::shims::resolve(&f.name) {
                                Some(p) => {
                                    write_slot(f.slot, p);
                                    filled += 1;
                                    if !out.shimmed.contains(&f.name) {
                                        out.shimmed.push(f.name.clone());
                                    }
                                }
                                None => out.note(format!("{}!{}", module.dll, f.name)),
                            },
                        }
                    }
                }
                Some(Provider::Shim { .. }) => {
                    for f in &module.functions {
                        match crate::shims::resolve(&f.name) {
                            Some(p) => {
                                write_slot(f.slot, p);
                                filled += 1;
                                if !out.shimmed.contains(&f.name) {
                                    out.shimmed.push(f.name.clone());
                                }
                            }
                            None => out.note(format!("{}!{}", module.dll, f.name)),
                        }
                    }
                }
                Some(Provider::Unavailable) => {
                    if !out.missing.contains(&module.dll) {
                        out.missing.push(module.dll.clone());
                    }
                }
                None => out.note(module.dll.clone()),
            }
        }
        if img.is_none() {
            for handle in handles.into_values().filter(|handle| !handle.is_null()) {
                unsafe { FreeLibrary(handle) };
            }
        }
        filled
    }

    pub fn check_imports(file: &[u8]) -> Result<NativeImports, String> {
        let _slot = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let pe = crate::pe::parse(file)?;
        if !pe.is_x64() {
            return Err(format!("Native Xbox import checks require x86-64; this image uses {:#x}.", pe.machine));
        }
        let mut report = Run::default();
        let resolved = bind_imports(&pe, None, &mut report);
        Ok(NativeImports {
            resolved,
            shimmed: report.shimmed,
            unimplemented: report.unimplemented,
            missing_modules: report.missing,
        })
    }

    fn tls_callbacks(pe: &Pe, image: &Image, image_size: usize) -> Result<Vec<u64>, String> {
        let Some(tls) = pe.tls.filter(|tls| tls.callbacks_va != 0) else { return Ok(Vec::new()) };
        let offset = tls.callbacks_va.checked_sub(pe.image_base)
            .ok_or("PE TLS callback table precedes the image")?;
        let mut callbacks = Vec::new();
        for index in 0..=64 {
            let slot = offset.checked_add(index * 8)
                .filter(|slot| slot.checked_add(8).is_some_and(|end| end <= image_size as u64))
                .ok_or("PE TLS callback table runs past the image")?;
            let address = unsafe { ((image.base + slot) as *const u64).read_unaligned() };
            if address == 0 {
                return Ok(callbacks);
            }
            let relative = address.checked_sub(image.base)
                .filter(|rva| *rva < image_size as u64)
                .ok_or("PE TLS callback address is outside the image")?;
            let executable = pe.sections.iter().any(|section| {
                section.protect() == PAGE_EXECUTE_READ
                    && relative.checked_sub(u64::from(section.vaddr))
                        .is_some_and(|offset| offset < u64::from(section.vsize.max(section.raw_size)))
            });
            if !executable {
                return Err("PE TLS callback address is outside executable sections".into());
            }
            callbacks.push(address);
        }
        Err("PE TLS callback table has more than 64 entries".into())
    }

    fn run_tls_callbacks(callbacks: &[u64], base: u64) {
        for &address in callbacks {
            let callback: extern "system" fn(*mut c_void, u32, *mut c_void) =
                unsafe { std::mem::transmute(address as usize) };
            callback(base as *mut c_void, 1, std::ptr::null_mut());
        }
    }

    pub fn run(file: &[u8]) -> Result<NativeResult, String> {
        let _slot = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        crate::shims::reset();
        let pe = crate::pe::parse(file)?;
        if !pe.is_x64() {
            return Err(format!("This program is machine {:#x}, not x86-64. Xbox One and Series X|S only run 64-bit programs.", pe.machine));
        }
        if pe.entry == 0 {
            return Err("This program has no entry point.".into());
        }

        let size = align_up(pe.image_size.max(pe.headers_size).max(0x1000) as u64, SECTION_ALIGN) as usize;
        let img = Image::reserve(pe.image_base, size, !pe.relocs.is_empty())?;
        copy_sections(&pe, file, &img)?;
        relocate(&pe, &img, (img.base as i64).wrapping_sub(pe.image_base as i64));

        let mut state = Run {
            modules: Vec::new(),
            shimmed: Vec::new(),
            unimplemented: Vec::new(),
            missing: Vec::new(),
            output: String::new(),
            exit_code: None,
            resolved: 0,
            runtime_refs: u32::from(pe.imports.iter().any(|module| module.dll.eq_ignore_ascii_case(XGAME_RUNTIME_DLL))),
            physical_pages: PhysicalPages::default(),
        };
        state.resolved = bind_imports(&pe, Some(&img), &mut state);
        let callbacks = tls_callbacks(&pe, &img, size)?;
        protect_sections(&pe, &img)?;

        let base = img.base;
        // Register x64 SEH unwind entries (.pdata) so exceptions inside the title unwind cleanly.
        let seh_table = if pe.exception_rva != 0 && pe.exception_size >= 12 {
            let tbl = (base + pe.exception_rva as u64) as *mut c_void;
            let count = pe.exception_size / 12;
            if unsafe { RtlAddFunctionTable(tbl, count, base) } != 0 {
                Some(tbl)
            } else {
                None
            }
        } else {
            None
        };

        *RUN.lock().unwrap_or_else(|e| e.into_inner()) = Some(state);
        let entry_addr = base + pe.entry as u64;
        let t = std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(move || {
                run_tls_callbacks(&callbacks, base);
                // Xbox entry points take no arguments and return the process exit code.
                let f: extern "system" fn() -> i32 = unsafe { std::mem::transmute(entry_addr as usize) };
                let rc = f();
                with(|r| {
                    if r.exit_code.is_none() {
                        r.exit_code = Some(rc);
                    }
                });
            });
        let thread_result = t.map(|thread| thread.join());
        let r = RUN.lock().unwrap_or_else(|e| e.into_inner()).take().expect("run state");
        if let Some(tbl) = seh_table {
            unsafe { RtlDeleteFunctionTable(tbl) };
        }
        thread_result.map_err(|e| e.to_string())?
            .map_err(|_| "The native title thread panicked.".to_string())?;
        Ok(NativeResult {
            output: r.output,
            exit_code: r.exit_code.unwrap_or(0),
            modules: r.modules,
            shimmed: r.shimmed,
            unimplemented: r.unimplemented,
            missing_modules: r.missing,
            resolved: r.resolved,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A PE32+ whose entry point calls one imported function and returns, so the test proves
        /// the whole path: sections mapped, imports bound, machine code executed.
        ///
        /// The exported name is passed in, so a test can choose which kernel32 function to call.
        fn sample_program(func: &str, image_base: u64) -> Vec<u8> {
            let mut d = full_program("hello from the Xbox One core", image_base);
            // full_program puts the hint/name at .data+0x100, so the string itself is 2 bytes
            // later (past the hint) at .data+0x102, which is file offset 0x400+0x102.
            let name_off = 0x400 + 0x102;
            d[name_off..name_off + 24].fill(0);
            d[name_off..name_off + func.len()].copy_from_slice(func.as_bytes());
            d
        }

        /// A PE whose entry point is only `mov eax, 7; ret`, with no imports. Used to check that
        /// running mapped code at all works before blaming the import machinery.
        fn no_call_program(image_base: u64) -> Vec<u8> {
            const TEXT_RVA: u32 = 0x1000;
            let mut d = vec![0u8; 0x400];
            d[0..2].copy_from_slice(b"MZ");
            d[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
            d[0x80..0x84].copy_from_slice(b"PE\0\0");
            let coff = 0x84;
            d[coff..coff + 2].copy_from_slice(&0x8664u16.to_le_bytes());
            d[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
            d[coff + 16..coff + 18].copy_from_slice(&240u16.to_le_bytes());
            let opt = coff + 20;
            d[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes());
            d[opt + 16..opt + 20].copy_from_slice(&TEXT_RVA.to_le_bytes());
            d[opt + 24..opt + 32].copy_from_slice(&image_base.to_le_bytes());
            d[opt + 56..opt + 60].copy_from_slice(&0x3000u32.to_le_bytes());
            d[opt + 60..opt + 64].copy_from_slice(&0x200u32.to_le_bytes());
            d[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes());
            let sh = opt + 240;
            d[sh..sh + 5].copy_from_slice(b".text");
            d[sh + 8..sh + 12].copy_from_slice(&0x200u32.to_le_bytes());
            d[sh + 12..sh + 16].copy_from_slice(&TEXT_RVA.to_le_bytes());
            d[sh + 16..sh + 20].copy_from_slice(&0x200u32.to_le_bytes());
            d[sh + 20..sh + 24].copy_from_slice(&0x200u32.to_le_bytes());
            d[sh + 36..sh + 40].copy_from_slice(&0x6000_0020u32.to_le_bytes());
            let code = [0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3]; // mov eax, 7; ret
            d[0x200..0x200 + code.len()].copy_from_slice(&code);
            d
        }

        /// The original two-section program, kept for the full-pipeline test.
        fn full_program(msg: &str, image_base: u64) -> Vec<u8> {
            const TEXT_RVA: u32 = 0x1000;
            const DATA_RVA: u32 = 0x2000;
            let mut d = vec![0u8; 0x600];
            d[0..2].copy_from_slice(b"MZ");
            d[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
            d[0x80..0x84].copy_from_slice(b"PE\0\0");
            let coff = 0x84;
            d[coff..coff + 2].copy_from_slice(&0x8664u16.to_le_bytes()); // AMD64
            d[coff + 2..coff + 4].copy_from_slice(&2u16.to_le_bytes()); // 2 sections
            d[coff + 16..coff + 18].copy_from_slice(&240u16.to_le_bytes());
            let opt = coff + 20;
            d[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes()); // PE32+
            d[opt + 16..opt + 20].copy_from_slice(&TEXT_RVA.to_le_bytes()); // entry
            d[opt + 24..opt + 32].copy_from_slice(&image_base.to_le_bytes());
            d[opt + 32..opt + 36].copy_from_slice(&0x1000u32.to_le_bytes()); // SectionAlignment
            d[opt + 36..opt + 40].copy_from_slice(&0x200u32.to_le_bytes()); // FileAlignment
            d[opt + 56..opt + 60].copy_from_slice(&0x3000u32.to_le_bytes()); // SizeOfImage
            d[opt + 60..opt + 64].copy_from_slice(&0x200u32.to_le_bytes()); // SizeOfHeaders
            d[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes
            // Import directory -> RVA 0x1100 (inside .text).
            d[opt + 112 + 8..opt + 112 + 12].copy_from_slice(&(TEXT_RVA + 0x100).to_le_bytes());
            let sh = opt + 240;
            // .text at RVA 0x1000, file 0x200.
            d[sh..sh + 5].copy_from_slice(b".text");
            d[sh + 8..sh + 12].copy_from_slice(&0x200u32.to_le_bytes()); // vsize
            d[sh + 12..sh + 16].copy_from_slice(&TEXT_RVA.to_le_bytes());
            d[sh + 16..sh + 20].copy_from_slice(&0x200u32.to_le_bytes()); // raw size
            d[sh + 20..sh + 24].copy_from_slice(&0x200u32.to_le_bytes()); // raw off
            d[sh + 36..sh + 40].copy_from_slice(&0x6000_0020u32.to_le_bytes()); // CODE|EXEC|READ
            // .data at RVA 0x2000, file 0x400.
            let sh2 = sh + 40;
            d[sh2..sh2 + 5].copy_from_slice(b".data");
            d[sh2 + 8..sh2 + 12].copy_from_slice(&0x200u32.to_le_bytes());
            d[sh2 + 12..sh2 + 16].copy_from_slice(&DATA_RVA.to_le_bytes());
            d[sh2 + 16..sh2 + 20].copy_from_slice(&0x200u32.to_le_bytes());
            d[sh2 + 20..sh2 + 24].copy_from_slice(&0x400u32.to_le_bytes());
            d[sh2 + 36..sh2 + 40].copy_from_slice(&0x4000_0040u32.to_le_bytes()); // INIT|READ|WRITE

            let text = 0x200usize;
            let data = 0x400usize;
            // The import: kernelx.dll!OutputDebugStringA.
            // Message string at .data+0.
            d[data..data + msg.len()].copy_from_slice(msg.as_bytes());
            // Hint/name "OutputDebugStringA" at .data+0x100.
            let name_off = data + 0x100;
            let mut name = b"OutputDebugStringA".to_vec();
            name.push(0);
            d[name_off + 2..name_off + 2 + 18].copy_from_slice(&name[..18]);
            // Thunk (lookup) at .data+0x180 pointing at the hint/name.
            let thunk_off = data + 0x180;
            let hint_rva = DATA_RVA + 0x100;
            d[thunk_off..thunk_off + 8].copy_from_slice(&(hint_rva as u64).to_le_bytes());
            // DLL name "kernelx.dll" at .data+0x1C0.
            let dll_off = data + 0x1C0;
            d[dll_off..dll_off + 11].copy_from_slice(b"kernelx.dll");
            // Import descriptor at .text+0x100.
            let desc = text + 0x100;
            d[desc..desc + 4].copy_from_slice(&(DATA_RVA + 0x180).to_le_bytes()); // OriginalFirstThunk
            d[desc + 12..desc + 16].copy_from_slice(&(DATA_RVA + 0x1C0).to_le_bytes()); // Name
            d[desc + 16..desc + 20].copy_from_slice(&(DATA_RVA + 0x1C0).to_le_bytes()); // FirstThunk (reuse)

            // Entry code. rcx = message; call [rip+disp32] resolves via the IAT at DATA_RVA+0x1C0.
            let mut code = Vec::new();
            // Reserve the 32 bytes of shadow space every Windows x64 call needs, plus 8 more to
            // keep the stack 16-byte aligned for the callee, exactly as a compiler emits.
            code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 0x28
            code.extend_from_slice(&[0x48, 0x8D, 0x0D]); // lea rcx, [rip+disp32]
            // `lea` is 7 bytes (3 emitted, 4 to come), so RIP for it is len+4 from here.
            let disp_msg = (DATA_RVA + 0 - (TEXT_RVA + code.len() as u32 + 4)) as i32;
            code.extend_from_slice(&disp_msg.to_le_bytes());
            // call qword ptr [rip+disp32] -> indirect through FirstThunk slot at DATA_RVA+0x1C0.
            code.extend_from_slice(&[0xFF, 0x15]);
            let after_call = TEXT_RVA + code.len() as u32 + 4;
            let disp_iat = ((DATA_RVA + 0x1C0) - after_call) as i32;
            code.extend_from_slice(&disp_iat.to_le_bytes());
            code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28]); // add rsp, 0x28
            code.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00]); // mov eax, 7
            code.extend_from_slice(&[0xC3]); // ret
            d[text..text + code.len()].copy_from_slice(&code);
            d
        }

        #[test]
        fn runs_a_bare_entry_point() {
            let base = 0x0000_0002_0000_0000u64;
            if Image::reserve(base, 0x3000, false).is_err() {
                eprintln!("skipped: {base:#x} is in use");
                return;
            }
            let r = run(&no_call_program(base)).expect("native run");
            assert_eq!(r.exit_code, 7);
        }

        #[test]
        fn reads_the_import_without_calling_it() {
            // Same program, but the entry only loads the IAT slot into rax and returns it. This
            // tells binding and the table itself apart from the call into Windows.
            let base = 0x0000_0002_1000_0000u64;
            if Image::reserve(base, 0x3000, false).is_err() {
                eprintln!("skipped: {base:#x} is in use");
                return;
            }
            let program = load_only_program(base);
            let r = run(&program).expect("native run");
            assert!(r.resolved >= 1, "{r:?}");
            assert_eq!(r.exit_code, 0);
        }

        /// An entry point that reads the IAT slot (without calling it) and returns 0 if non-null.
        fn load_only_program(image_base: u64) -> Vec<u8> {
            let mut p = full_program("", image_base);
            // Overwrite .text with: mov rax, [rip+disp]; test rax,rax; setne al; movzx eax,al; ret
            const TEXT_RVA: u32 = 0x1000;
            let at = 0x200usize;
            let mut code = Vec::new();
            code.extend_from_slice(&[0x48, 0x8B, 0x05]); // mov rax, [rip+disp32]
            let disp = ((TEXT_RVA + 0x1C0) - (TEXT_RVA + code.len() as u32 + 4)) as i32;
            code.extend_from_slice(&disp.to_le_bytes());
            code.extend_from_slice(&[0x85, 0xC0, 0x0F, 0x95, 0xC0, 0x0F, 0xB6, 0xC0, 0xC3]);
            p[at..at + code.len()].copy_from_slice(&code);
            p
        }

        #[test]
        fn calls_a_real_windows_function_from_guest_code() {
            // The whole point of the Xbox mapping: a guest calling `kernelx.dll!X` reaches the
            // real Windows implementation. GetCurrentProcessId takes no arguments, so this
            // covers binding, the indirect call, and returning cleanly.
            let base = 0x0000_0002_2000_0000u64;
            if Image::reserve(base, 0x3000, false).is_err() {
                eprintln!("skipped: {base:#x} is in use on this machine");
                return;
            }
            let r = run(&sample_program("GetCurrentProcessId", base)).expect("native run");
            assert_eq!(r.exit_code, 7);
            assert!(r.resolved >= 1, "the import should have resolved: {r:?}");
        }

        #[test]
        fn runs_an_x64_program_through_real_windows_imports() {
            // Put the image somewhere free, then hand the address to the sample.
            let base = 0x0000_0002_3000_0000u64;
            let program = sample_program("OutputDebugStringA", base);
            let pe = crate::pe::parse(&program).unwrap();
            assert!(pe.is_x64());
            assert_eq!(pe.dll_names(), vec!["kernelx.dll"]);
            assert_eq!(pe.imports[0].functions[0].name, "OutputDebugStringA");
            // Only run when the preferred address really is free, so the test never fights
            // another program for it.
            if Image::reserve(base, 0x3000, false).is_err() {
                eprintln!("skipped: {base:#x} is in use on this machine");
                return;
            }
            let r = run(&program).expect("native run");
            assert_eq!(r.exit_code, 7);
            assert!(r.resolved >= 1, "the Windows import should have been resolved: {r:?}");
            assert!(r.missing_modules.is_empty());
        }

        #[test]
        fn executes_code_sharing_a_page_with_the_import_table() {
            let mut program = sample_program("GetCurrentProcessId", 0x0000_0002_4000_0000);
            program[0x310..0x314].copy_from_slice(&0x11C0u32.to_le_bytes());
            program[0x20D..0x211].copy_from_slice(&(0x11C0i32 - 0x1011).to_le_bytes());
            let result = run(&program).expect("native run with executable IAT section");
            assert_eq!(result.exit_code, 7);
            assert_eq!(result.resolved, 1);
            assert!(result.is_complete());
        }

        #[test]
        fn binds_import_tables_in_separate_sections() {
            let mut program = sample_program("GetCurrentProcessId", 0x0000_0002_6000_0000);
            program[0x314..0x318].copy_from_slice(&0x11A0u32.to_le_bytes());
            program[0x320..0x324].copy_from_slice(&0x1160u32.to_le_bytes());
            program[0x324..0x328].copy_from_slice(&0x11C0u32.to_le_bytes());
            program[0x360..0x36C].copy_from_slice(b"kernelx.dll\0");
            program[0x382..0x396].copy_from_slice(b"GetCurrentProcessId\0");
            program[0x3A0..0x3A8].copy_from_slice(&0x1180u64.to_le_bytes());
            program[0x20D..0x211].copy_from_slice(&(0x11C0i32 - 0x1011).to_le_bytes());
            let result = run(&program).expect("native run with import tables in separate sections");
            assert_eq!(result.exit_code, 7);
            assert_eq!(result.resolved, 2);
            assert!(result.is_complete());
            let mut emu = crate::x64_emu::XboxX64Emu::load_pe(&program).unwrap();
            emu.set_strict_imports();
            let interpreted = emu.run(100);
            assert!(interpreted.stopped_reason.is_none(), "{:?}", interpreted.stopped_reason);
            assert_eq!(interpreted.exit_code, result.exit_code);
        }

        #[test]
        fn guest_uses_xbox_memory_allocator_through_native_import_table() {
            let mut program = full_program("", 0x0000_0002_8000_0000);
            program[0x502..0x513].copy_from_slice(b"XMemVirtualAlloc\0");
            program[0x5C0..0x5CC].fill(0);
            program[0x5C0..0x5C9].copy_from_slice(b"xmem.dll\0");
            program[0x310..0x314].copy_from_slice(&0x21D0u32.to_le_bytes());
            program[0x314..0x318].copy_from_slice(&0x11A0u32.to_le_bytes());
            program[0x320..0x324].copy_from_slice(&0x1160u32.to_le_bytes());
            program[0x324..0x328].copy_from_slice(&0x11C0u32.to_le_bytes());
            program[0x360..0x36C].copy_from_slice(b"kernelx.dll\0");
            program[0x382..0x38E].copy_from_slice(b"VirtualFree\0");
            program[0x3A0..0x3A8].copy_from_slice(&0x1180u64.to_le_bytes());

            let entry = [
                0x48, 0x83, 0xEC, 0x28,
                0x31, 0xC9,
                0xBA, 1, 0, 0, 0,
                0x41, 0xB8, 0, 0x30, 0, 0x20,
                0x41, 0xB9, 7, 0, 0, 0,
                0xC7, 0x44, 0x24, 0x20, 4, 8, 0, 0,
                0xFF, 0x15, 0xAB, 0x11, 0, 0,
                0x48, 0x85, 0xC0,
                0x74, 0x1F,
                0x48, 0x89, 0xC1,
                0xA9, 0xFF, 0xFF, 0x1F, 0,
                0x75, 0x15,
                0xC6, 0x80, 0xFF, 0xFF, 0x1F, 0, 42,
                0x31, 0xD2,
                0x41, 0xB8, 0, 0x80, 0, 0,
                0xFF, 0x15, 0x77, 1, 0, 0,
                0x48, 0x83, 0xC4, 0x28,
                0xC3,
            ];
            program[0x200..0x200 + entry.len()].copy_from_slice(&entry);

            let result = run(&program).expect("native run with Xbox allocation");
            assert!(result.is_complete(), "{result:?}");
            assert_eq!(result.shimmed, ["XMemVirtualAlloc"]);
            assert_eq!(result.exit_code, 1);
        }

        #[test]
        fn guest_allocates_and_frees_native_physical_pages() {
            let mut program = sample_program("XMemAllocatePhysicalPages", 0x0000_0002_C000_0000);
            let library = b"xmem.dll\0";
            program[0x5C0..0x5C0 + library.len()].copy_from_slice(library);
            program[0x310..0x314].copy_from_slice(&0x21D0u32.to_le_bytes());
            program[0x588..0x590].copy_from_slice(&0x2140u64.to_le_bytes());
            let free = b"XMemFreePhysicalPages\0";
            program[0x542..0x542 + free.len()].copy_from_slice(free);

            let mut entry = vec![
                0x48, 0x83, 0xEC, 0x38,
                0x48, 0xC7, 0x44, 0x24, 0x20, 1, 0, 0, 0,
                0xB9, 0x80, 2, 0, 0,
                0x48, 0x8D, 0x54, 0x24, 0x20,
                0x4C, 0x8D, 0x44, 0x24, 0x28,
                0xFF, 0x15,
            ];
            let displacement = 0x21D0i32 - (0x1000 + entry.len() as i32 + 4);
            entry.extend_from_slice(&displacement.to_le_bytes());
            entry.extend_from_slice(&[
                0x85, 0xC0, 0x74, 0x10,
                0xB9, 1, 0, 0, 0,
                0x48, 0x8D, 0x54, 0x24, 0x28,
                0xFF, 0x15,
            ]);
            let displacement = 0x21D8i32 - (0x1000 + entry.len() as i32 + 4);
            entry.extend_from_slice(&displacement.to_le_bytes());
            entry.extend_from_slice(&[0x48, 0x83, 0xC4, 0x38, 0xC3]);
            program[0x200..0x200 + entry.len()].copy_from_slice(&entry);

            let result = run(&program).expect("native run with Xbox physical pages");
            assert!(result.is_complete(), "{result:?}");
            assert_eq!(result.shimmed, ["XMemAllocatePhysicalPages", "XMemFreePhysicalPages"]);
            assert_eq!(result.exit_code, 1);
        }

        #[test]
        fn guest_runtime_error_is_captured_in_both_launch_modes() {
            let mut program = full_program("startup failed", 0x0000_0002_9000_0000);
            let dll = b"xgameruntime.dll\0";
            let import = b"XErrorReport\0";
            program[0x5C0..0x5C0 + dll.len()].copy_from_slice(dll);
            program[0x502..0x502 + import.len()].copy_from_slice(import);
            let entry = [
                0x48, 0x83, 0xEC, 0x28,
                0xB9, 0x05, 0x40, 0, 0x80,
                0x48, 0x8D, 0x15, 0xF0, 0x0F, 0, 0,
                0xFF, 0x15, 0xAA, 0x11, 0, 0,
                0x48, 0x83, 0xC4, 0x28,
                0xC3,
            ];
            program[0x200..0x200 + entry.len()].copy_from_slice(&entry);

            let mut interpreter = crate::x64_emu::XboxX64Emu::load_pe(&program).unwrap();
            interpreter.set_strict_imports();
            let interpreted = interpreter.run(100);
            assert!(interpreted.stopped_reason.is_none(), "{interpreted:?}");

            let native = run(&program).expect("native run with Xbox error reporting");
            assert!(native.is_complete(), "{native:?}");
            assert_eq!(native.shimmed, ["XErrorReport"]);
            assert_eq!(native.exit_code, 0);
            assert_eq!(interpreted.exit_code, native.exit_code);
            assert_eq!(native.output, "XErrorReport(0x80004005): startup failed\n");
            assert_eq!(interpreted.output, native.output);
        }

        #[test]
        fn native_import_preflight_never_calls_the_entry_point() {
            let base = 0x0000_0002_B000_0000u64;
            for (function, available) in [("VirtualFree", true), ("UnknownXboxExport", false)] {
                let mut program = sample_program(function, base);
                program[0x200..0x202].copy_from_slice(&[0x0F, 0x0B]);
                let report = check_imports(&program).unwrap();
                assert_eq!(report.is_complete(), available);
                assert_eq!(report.resolved, usize::from(available));
                if !available {
                    assert_eq!(report.unimplemented, ["kernelx.dll!UnknownXboxExport"]);
                }
            }
        }

        #[test]
        fn preserves_writable_data_sharing_a_page_with_the_import_table() {
            let mut program = full_program("", 0x0000_0002_5000_0000);
            let data_flags = 0x188 + 40 + 36;
            program[data_flags..data_flags + 4].copy_from_slice(&0xC000_0040u32.to_le_bytes());
            let code = [
                0xC7, 0x05, 0xF6, 0x0F, 0, 0, 42, 0, 0, 0,
                0x8B, 0x05, 0xF0, 0x0F, 0, 0, 0xC3,
            ];
            program[0x200..0x200 + code.len()].copy_from_slice(&code);
            let result = run(&program).expect("native run with writable IAT section");
            assert_eq!(result.exit_code, 42);
            assert_eq!(result.resolved, 1);
            assert!(result.is_complete());
        }

        #[test]
        fn runs_process_attach_callback_on_the_native_entry_thread() {
            let base = 0x0000_0002_7000_0000u64;
            let mut program = full_program("", base);
            let data_flags = 0x188 + 40 + 36;
            program[data_flags..data_flags + 4].copy_from_slice(&0xC000_0040u32.to_le_bytes());
            let directory = 0x98 + 112 + 9 * 8;
            program[directory..directory + 4].copy_from_slice(&0x2040u32.to_le_bytes());
            program[directory + 4..directory + 8].copy_from_slice(&40u32.to_le_bytes());
            program[0x450..0x458].copy_from_slice(&(base + 0x2030).to_le_bytes());
            program[0x458..0x460].copy_from_slice(&(base + 0x2080).to_le_bytes());
            program[0x480..0x488].copy_from_slice(&(base + 0x1040).to_le_bytes());
            let entry = [
                0x65, 0x8B, 0x04, 0x25, 0x48, 0, 0, 0, 0x2B, 0x05, 0xF2, 0x0F, 0, 0, 0xC3,
            ];
            let callback = [
                0x65, 0x8B, 0x04, 0x25, 0x48, 0, 0, 0, 0x89, 0x81, 0x00, 0x20, 0, 0, 0xC3,
            ];
            program[0x200..0x200 + entry.len()].copy_from_slice(&entry);
            program[0x240..0x240 + callback.len()].copy_from_slice(&callback);
            let result = run(&program).expect("native run with a TLS callback");
            assert_eq!(result.exit_code, 0);
            assert!(result.is_complete());
        }

        #[test]
        fn rejects_tls_callbacks_outside_the_mapped_executable_sections() {
            let base = 0x0000_0002_A000_0000u64;
            let mut program = full_program("", base);
            let directory = 0x98 + 112 + 9 * 8;
            program[directory..directory + 4].copy_from_slice(&0x2040u32.to_le_bytes());
            program[directory + 4..directory + 8].copy_from_slice(&40u32.to_le_bytes());

            program[0x458..0x460].copy_from_slice(&(base + 0x3000).to_le_bytes());
            assert!(run(&program).unwrap_err().contains("callback table runs past the image"));

            program[0x458..0x460].copy_from_slice(&(base + 0x2080).to_le_bytes());
            program[0x480..0x488].copy_from_slice(&(base + 0x3000).to_le_bytes());
            assert!(run(&program).unwrap_err().contains("callback address is outside the image"));

            program[0x480..0x488].copy_from_slice(&(base + 0x2000).to_le_bytes());
            assert!(run(&program).unwrap_err().contains("callback address is outside executable sections"));
        }

        #[test]
        fn relocatable_reserve_falls_back_when_preferred_base_is_busy() {
            // Address 0x1000 is never available on Windows; allow_relocate = true must still succeed.
            let img = Image::reserve(0x1000, 0x3000, true).expect("OS fallback allocation");
            assert_ne!(img.base, 0);
            assert_ne!(img.base, 0x1000);
        }
    }
}
