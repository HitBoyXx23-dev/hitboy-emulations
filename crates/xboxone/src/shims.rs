//! Replacements for the Xbox One / Series X|S system calls that have no Windows counterpart.
//!
//! The Windows libraries (the kernel, Direct3D, the C runtime) are satisfied by the real DLLs in
//! [`crate::modules`]. What is left is Xbox-specific: starting the title runtime, opening saved
//! data, reading the signed-in user, talking to Xbox services. Each call here keeps the Xbox
//! signature and does the closest safe thing on a PC, so a game gets far enough to run its
//! start-up code instead of faulting on the first missing import.
//!
//! Calling convention: Xbox x64 code is Windows x64 — the first four integer arguments go in rcx,
//! rdx, r8, r9 and the caller cleans the stack — so `extern "system"` is already correct and, as
//! with any Windows program, no thunk is needed.
//!
//! Every function here runs on the game's thread, inside the emulator, and must not panic across
//! the boundary: a panic unwinding through the game's frames would be undefined behaviour.

#![allow(non_snake_case)]

#[cfg(not(all(windows, target_arch = "x86_64")))]
mod stub {
    use std::ffi::c_void;
    /// No shims on this host; native mode is unavailable here anyway.
    pub fn resolve(_name: &str) -> Option<*mut c_void> {
        None
    }
    pub fn bound() -> Vec<String> {
        Vec::new()
    }
    pub fn reset() {}
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
pub use stub::*;

#[cfg(all(windows, target_arch = "x86_64"))]
mod win {
    use std::ffi::c_void;
    use std::sync::Mutex;

    /// Xbox's `HRESULT`. Games that get furthest are the ones that treat a failed call as a normal
    /// outcome and carry on, so most shims report failure rather than aborting.
    pub const E_FAIL: i32 = 0x8000_0405u32 as i32;
    pub const S_OK: i32 = 0;

    /// Remembers which shim each import name resolved to, so the loader can report coverage.
    static BOUND: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// The Xbox-only function names bound during the current run.
    pub fn bound() -> Vec<String> {
        BOUND.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn reset() {
        BOUND.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Reads an ASCII string out of the game's memory, which is what the Xbox debug output takes.
    unsafe fn text(p: u64) -> String {
        if p == 0 {
            return String::new();
        }
        let mut len = 0;
        while *((p as *const u8).add(len)) != 0 && len < 1 << 16 {
            len += 1;
        }
        String::from_utf8_lossy(std::slice::from_raw_parts(p as *const u8, len)).into_owned()
    }

    /// Starts the title runtime. On an Xbox this brings up the title's services; here it succeeds
    /// so the game's start-up path continues.
    pub extern "system" fn XGameRuntimeInitialize(_package: u64, _features: u64) -> i32 {
        S_OK
    }

    pub extern "system" fn XGameRuntimeUninitialize() -> i32 {
        S_OK
    }

    /// Xbox's debug output. A game's `OutputDebugStringA` reaches here and is captured for the
    /// report, which is usually the only way to see how far a title got.
    pub extern "system" fn XGameRuntimePrint(_mask: u64, format: u64, _args: u64) -> u32 {
        // Reading the arguments exactly means parsing the format string against a va_list, which
        // needs the game's stack; the shape is captured and the body shown unexpanded.
        let f = unsafe { text(format) };
        crate::native::print_on_this_thread(&f);
        S_OK as u32
    }

    /// Sleeps, using the Xbox unit (100 ns ticks, the same as Windows').
    pub extern "system" fn XGameRuntimeSleep(ticks: u64) {
        std::thread::sleep(std::time::Duration::from_nanos(ticks.min(60_000_000_000)));
    }

    /// Which sandbox the title runs in. Reported as a dev sandbox, the value a side-loaded or
    /// Developer Mode package gets, so a game taking the developer branch behaves as expected.
    pub extern "system" fn XSystemGetXboxLiveSandboxId(out: u64) -> i32 {
        if out != 0 {
            unsafe { std::ptr::write_unaligned(out as *mut u32, 2) }; // DevSandbox
        }
        S_OK
    }

    pub extern "system" fn XGameRuntimeGetXboxLiveSandboxId(out: u64) -> i32 {
        XSystemGetXboxLiveSandboxId(out)
    }

    /// The signed-in user's gamertag. A PC has no Xbox user, but a stable synthetic one lets a
    /// game that only wants a name to carry on.
    pub extern "system" fn XUserGetGamertag(_handle: u64, buffer: u64, cap: u32) -> i32 {
        if buffer == 0 || cap < 16 {
            return E_FAIL;
        }
        unsafe {
            for (i, ch) in "LocalPlayer".encode_utf16().enumerate() {
                std::ptr::write_unaligned((buffer as *mut u16).add(i), ch);
            }
            std::ptr::write_unaligned((buffer as *mut u16).add(11), 0);
        }
        S_OK
    }

    /// Opens a block of the user's saved data. Saved data needs the title's container and the
    /// console's encryption key, so this reports that it is unavailable rather than inventing one.
    pub extern "system" fn XStorageCreateHandle(_user: u64, _name: u64, _options: u64, out: u64) -> i32 {
        if out != 0 {
            unsafe { std::ptr::write_unaligned(out as *mut u64, 0) };
        }
        E_FAIL
    }

    // ---- XSystem / XGame / XPackage ----

    /// Reports Xbox Series X (Anaconda = 7) so GDK titles pick the modern console profile.
    pub extern "system" fn XSystemGetDeviceType() -> u32 {
        7
    }

    /// Writes a stable synthetic console ID string (UTF-8, NUL-terminated).
    pub extern "system" fn XSystemGetConsoleId(cap: usize, buf: u64, used: u64) -> i32 {
        const ID: &[u8] = b"00000000.00000000.00000000.00000000.01\0";
        if used != 0 {
            unsafe { std::ptr::write_unaligned(used as *mut usize, ID.len()) };
        }
        if buf == 0 || cap < ID.len() {
            return E_FAIL;
        }
        unsafe { std::ptr::copy_nonoverlapping(ID.as_ptr(), buf as *mut u8, ID.len()) };
        S_OK
    }

    pub extern "system" fn XSystemIsHandleValid(handle: u64) -> bool {
        handle != 0
    }

    pub extern "system" fn XGameGetXboxTitleId(out: u64) -> i32 {
        if out == 0 {
            return E_FAIL;
        }
        unsafe { std::ptr::write_unaligned(out as *mut u32, 0x1234_5678) };
        S_OK
    }

    pub extern "system" fn XPackageGetCurrentProcessPackageIdentifier(cap: usize, buf: u64) -> i32 {
        const PKG: &[u8] = b"Console.DevTitle_1.0.0.0_x64__0000000000000\0";
        if buf == 0 || cap < PKG.len() {
            return E_FAIL;
        }
        unsafe { std::ptr::copy_nonoverlapping(PKG.as_ptr(), buf as *mut u8, PKG.len()) };
        S_OK
    }

    // ---- XUser ----

    pub extern "system" fn XUserGetId(_user: u64, out: u64) -> i32 {
        if out == 0 {
            return E_FAIL;
        }
        unsafe { std::ptr::write_unaligned(out as *mut u64, 0x0009_0000_0000_0001) };
        S_OK
    }

    pub extern "system" fn XUserGetState(_user: u64, out: u64) -> i32 {
        if out == 0 {
            return E_FAIL;
        }
        unsafe { std::ptr::write_unaligned(out as *mut u32, 0) }; // SignedIn
        S_OK
    }

    pub extern "system" fn XUserGetAgeGroup(_user: u64, out: u64) -> i32 {
        if out == 0 {
            return E_FAIL;
        }
        unsafe { std::ptr::write_unaligned(out as *mut u32, 3) }; // Adult
        S_OK
    }

    pub extern "system" fn XUserCheckPrivilege(
        _user: u64,
        _options: u32,
        _privilege: u32,
        has_privilege: u64,
        reason: u64,
    ) -> i32 {
        if has_privilege != 0 {
            unsafe { std::ptr::write_unaligned(has_privilege as *mut u8, 1) };
        }
        if reason != 0 {
            unsafe { std::ptr::write_unaligned(reason as *mut u32, 0) };
        }
        S_OK
    }

    pub extern "system" fn XUserCloseHandle(_user: u64) {}

    // ---- XTaskQueue ----

    type TaskCallback = extern "system" fn(context: u64, canceled: bool);

    struct TaskQueueTable {
        next_handle: u64,
        queues: std::collections::BTreeMap<u64, Vec<(u32, u64, u64)>>,
    }

    static TASK_QUEUES: Mutex<Option<TaskQueueTable>> = Mutex::new(None);

    fn with_queues<R>(f: impl FnOnce(&mut TaskQueueTable) -> R) -> R {
        let mut g = TASK_QUEUES.lock().unwrap_or_else(|e| e.into_inner());
        let tbl = g.get_or_insert_with(|| {
            let mut queues = std::collections::BTreeMap::new();
            queues.insert(1, Vec::new()); // default process queue
            TaskQueueTable { next_handle: 2, queues }
        });
        f(tbl)
    }

    pub extern "system" fn XTaskQueueCreate(_work_mode: u32, _completion_mode: u32, out: u64) -> i32 {
        if out == 0 {
            return E_FAIL;
        }
        let h = with_queues(|t| {
            let h = t.next_handle;
            t.next_handle += 1;
            t.queues.insert(h, Vec::new());
            h
        });
        unsafe { std::ptr::write_unaligned(out as *mut u64, h) };
        S_OK
    }

    pub extern "system" fn XTaskQueueCloseHandle(queue: u64) {
        if queue > 1 {
            with_queues(|t| {
                t.queues.remove(&queue);
            });
        }
    }

    pub extern "system" fn XTaskQueueGetCurrentProcessTaskQueue(out: u64) -> bool {
        if out == 0 {
            return false;
        }
        with_queues(|_| {});
        unsafe { std::ptr::write_unaligned(out as *mut u64, 1) };
        true
    }

    pub extern "system" fn XTaskQueueSubmitCallback(queue: u64, port: u32, context: u64, callback: u64) -> i32 {
        if callback == 0 {
            return E_FAIL;
        }
        let q = if queue == 0 { 1 } else { queue };
        let ok = with_queues(|t| {
            if let Some(items) = t.queues.get_mut(&q) {
                items.push((port, context, callback));
                true
            } else {
                false
            }
        });
        if ok { S_OK } else { E_FAIL }
    }

    pub extern "system" fn XTaskQueueDispatch(queue: u64, port: u32, _timeout_ms: u32) -> bool {
        let q = if queue == 0 { 1 } else { queue };
        let next = with_queues(|t| {
            let items = t.queues.get_mut(&q)?;
            let pos = items.iter().position(|(p, _, _)| *p == port)?;
            Some(items.remove(pos))
        });
        if let Some((_, ctx, cb)) = next {
            let func: TaskCallback = unsafe { std::mem::transmute(cb as usize) };
            func(ctx, false);
            true
        } else {
            false
        }
    }

    // ---- XAsync & XUserAddAsync ----

    /// Minimal layout of GDK `XAsyncBlock`:
    ///   offset 0:  queue: u64
    ///   offset 8:  context: u64
    ///   offset 16: callback: u64 (extern "system" fn(*mut XAsyncBlock))
    ///   offset 24: internal: [u8; 32]
    pub extern "system" fn XAsyncGetStatus(async_block: u64, _wait: bool) -> i32 {
        if async_block == 0 {
            return E_FAIL;
        }
        S_OK
    }

    pub extern "system" fn XAsyncGetResultSize(async_block: u64, out_size: u64) -> i32 {
        if async_block == 0 || out_size == 0 {
            return E_FAIL;
        }
        unsafe { std::ptr::write_unaligned(out_size as *mut usize, 0) };
        S_OK
    }

    pub extern "system" fn XAsyncCancel(_async_block: u64) {}

    extern "system" fn invoke_async_completion(async_block: u64, _canceled: bool) {
        if async_block == 0 {
            return;
        }
        let cb = unsafe { std::ptr::read_unaligned((async_block + 16) as *const u64) };
        if cb != 0 {
            let func: extern "system" fn(u64) = unsafe { std::mem::transmute(cb as usize) };
            func(async_block);
        }
    }

    /// Signs in the synthetic local user asynchronously, queueing the XAsyncBlock's completion
    /// callback on its XTaskQueue if one was provided.
    pub extern "system" fn XUserAddAsync(_options: u32, async_block: u64) -> i32 {
        if async_block == 0 {
            return E_FAIL;
        }
        let queue = unsafe { std::ptr::read_unaligned(async_block as *const u64) };
        let cb = unsafe { std::ptr::read_unaligned((async_block + 16) as *const u64) };
        if cb != 0 {
            let _ = XTaskQueueSubmitCallback(queue, 1, async_block, invoke_async_completion as *const () as u64);
        }
        S_OK
    }

    pub extern "system" fn XUserAddResult(async_block: u64, out_user: u64) -> i32 {
        if async_block == 0 || out_user == 0 {
            return E_FAIL;
        }
        unsafe { std::ptr::write_unaligned(out_user as *mut u64, 1) };
        S_OK
    }

    // ---- XGameSave (local container/blob store with optional on-disk persistence) ----

    struct SaveStore {
        next_handle: u64,
        providers: std::collections::BTreeSet<u64>,
        containers: std::collections::BTreeMap<u64, String>,
        updates: std::collections::BTreeMap<u64, (String, Vec<(String, Vec<u8>)>)>,
        blobs: std::collections::BTreeMap<(String, String), Vec<u8>>,
        root_dir: Option<std::path::PathBuf>,
    }

    static SAVE_STORE: Mutex<Option<SaveStore>> = Mutex::new(None);

    fn safe_save_segment(raw: &str) -> String {
        let mut s: String = raw
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
            .collect();
        if s.is_empty() || s == "." || s == ".." {
            s = "default".into();
        }
        s
    }

    fn with_saves<R>(f: impl FnOnce(&mut SaveStore) -> R) -> R {
        let mut g = SAVE_STORE.lock().unwrap_or_else(|e| e.into_inner());
        let s = g.get_or_insert_with(|| SaveStore {
            next_handle: 1,
            providers: std::collections::BTreeSet::new(),
            containers: std::collections::BTreeMap::new(),
            updates: std::collections::BTreeMap::new(),
            blobs: std::collections::BTreeMap::new(),
            root_dir: None,
        });
        f(s)
    }

    /// Configures a host folder for persistent `XGameSave` blob storage.
    pub fn set_save_root(dir: Option<std::path::PathBuf>) {
        with_saves(|s| {
            s.root_dir = dir;
            s.blobs.clear();
        });
    }

    pub extern "system" fn XGameSaveInitializeProvider(_user: u64, _scid: u64, _sync: bool, out: u64) -> i32 {
        if out == 0 {
            return E_FAIL;
        }
        let h = with_saves(|s| {
            let h = s.next_handle;
            s.next_handle += 1;
            s.providers.insert(h);
            h
        });
        unsafe { std::ptr::write_unaligned(out as *mut u64, h) };
        S_OK
    }

    pub extern "system" fn XGameSaveCloseProvider(provider: u64) {
        with_saves(|s| {
            s.providers.remove(&provider);
        });
    }

    pub extern "system" fn XGameSaveGetRemainingQuota(provider: u64, out_quota: u64) -> i32 {
        if out_quota == 0 || !with_saves(|s| s.providers.contains(&provider)) {
            return E_FAIL;
        }
        unsafe { std::ptr::write_unaligned(out_quota as *mut i64, 64 * 1024 * 1024) };
        S_OK
    }

    pub extern "system" fn XGameSaveCreateContainer(provider: u64, name: u64, out: u64) -> i32 {
        if out == 0 || name == 0 || !with_saves(|s| s.providers.contains(&provider)) {
            return E_FAIL;
        }
        let cname = unsafe { text(name) };
        let h = with_saves(|s| {
            let h = s.next_handle;
            s.next_handle += 1;
            s.containers.insert(h, cname);
            h
        });
        unsafe { std::ptr::write_unaligned(out as *mut u64, h) };
        S_OK
    }

    pub extern "system" fn XGameSaveCloseContainer(container: u64) {
        with_saves(|s| {
            s.containers.remove(&container);
        });
    }

    pub extern "system" fn XGameSaveCreateUpdate(container: u64, _display_name: u64, out: u64) -> i32 {
        if out == 0 {
            return E_FAIL;
        }
        let h = with_saves(|s| {
            let cname = s.containers.get(&container)?.clone();
            let h = s.next_handle;
            s.next_handle += 1;
            s.updates.insert(h, (cname, Vec::new()));
            Some(h)
        });
        let Some(h) = h else { return E_FAIL };
        unsafe { std::ptr::write_unaligned(out as *mut u64, h) };
        S_OK
    }

    pub extern "system" fn XGameSaveSubmitBlobWrite(update: u64, blob_name: u64, data: u64, size: usize) -> i32 {
        if blob_name == 0 || (size > 0 && data == 0) {
            return E_FAIL;
        }
        let bname = unsafe { text(blob_name) };
        let bytes = if size == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(data as *const u8, size.min(16 << 20)) }.to_vec()
        };
        let ok = with_saves(|s| {
            if let Some((_, list)) = s.updates.get_mut(&update) {
                list.push((bname, bytes));
                true
            } else {
                false
            }
        });
        if ok { S_OK } else { E_FAIL }
    }

    pub extern "system" fn XGameSaveSubmitUpdate(update: u64) -> i32 {
        let ok = with_saves(|s| {
            let (cname, list) = s.updates.get_mut(&update)?;
            let cname = cname.clone();
            let root = s.root_dir.clone();
            let pending: Vec<(String, Vec<u8>)> = list.drain(..).collect();
            for (bname, bytes) in pending {
                if let Some(ref root_dir) = root {
                    let cdir = root_dir.join(safe_save_segment(&cname));
                    if std::fs::create_dir_all(&cdir).is_ok() {
                        let _ = std::fs::write(cdir.join(format!("{}.bin", safe_save_segment(&bname))), &bytes);
                    }
                }
                s.blobs.insert((cname.clone(), bname), bytes);
            }
            Some(())
        });
        if ok.is_some() { S_OK } else { E_FAIL }
    }

    pub extern "system" fn XGameSaveCloseUpdate(update: u64) {
        with_saves(|s| {
            s.updates.remove(&update);
        });
    }

    pub extern "system" fn XSystemGetAppSpecificDeviceId(cap: usize, buf: u64, used: u64) -> i32 {
        const DID: &[u8] = b"XBOX-LOCAL-DEVICE-0001\0";
        if used != 0 {
            unsafe { std::ptr::write_unaligned(used as *mut usize, DID.len()) };
        }
        if buf == 0 || cap < DID.len() {
            return E_FAIL;
        }
        unsafe { std::ptr::copy_nonoverlapping(DID.as_ptr(), buf as *mut u8, DID.len()) };
        S_OK
    }

    pub extern "system" fn XDisplayTryAcquireTimeoutStatus() -> u32 {
        0
    }

    // ---- Locale / Store / GameUI / Networking ----

    pub extern "system" fn XUserGetLocale(cap: usize, buf: u64, used: u64) -> i32 {
        const LOC: &[u8] = b"en-US\0";
        if used != 0 {
            unsafe { std::ptr::write_unaligned(used as *mut usize, LOC.len()) };
        }
        if buf == 0 || cap < LOC.len() {
            return E_FAIL;
        }
        unsafe { std::ptr::copy_nonoverlapping(LOC.as_ptr(), buf as *mut u8, LOC.len()) };
        S_OK
    }

    pub extern "system" fn XStoreCreateContext(_user: u64, out: u64) -> i32 {
        if out == 0 {
            return E_FAIL;
        }
        unsafe { std::ptr::write_unaligned(out as *mut u64, 1) };
        S_OK
    }

    pub extern "system" fn XStoreCloseContextHandle(_ctx: u64) {}

    pub extern "system" fn XStoreIsAvailabilityPurchasable(_availability: u64) -> bool {
        false
    }

    pub extern "system" fn XStoreAcquireLicenseForPackageSync(_ctx: u64, _pkg: u64, out: u64) -> i32 {
        if out != 0 {
            unsafe { std::ptr::write_unaligned(out as *mut u64, 0) };
        }
        E_FAIL
    }

    pub extern "system" fn XGameUiShowMessageDialogAsync(
        async_block: u64,
        _title: u64,
        _content: u64,
        _first_btn: u64,
        _second_btn: u64,
        _third_btn: u64,
        _default_btn: u32,
        _cancel_btn: u32,
    ) -> i32 {
        if async_block == 0 {
            return E_FAIL;
        }
        let queue = unsafe { std::ptr::read_unaligned(async_block as *const u64) };
        let cb = unsafe { std::ptr::read_unaligned((async_block + 16) as *const u64) };
        if cb != 0 {
            let _ = XTaskQueueSubmitCallback(queue, 1, async_block, invoke_async_completion as *const () as u64);
        }
        S_OK
    }

    pub extern "system" fn XGameUiShowMessageDialogResult(async_block: u64, out_btn: u64) -> i32 {
        if async_block == 0 || out_btn == 0 {
            return E_FAIL;
        }
        unsafe { std::ptr::write_unaligned(out_btn as *mut u32, 0) };
        S_OK
    }

    pub extern "system" fn XLaunchNewGame(_exe: u64, _args: u64, _user: u64) {}

    /// Writes an offline-ready `XNetworkingConnectivityHint` (24 bytes):
    /// connectivityLevel = None (1), dataLimit = None (0), ianaInterfaceType = 0, networkInitialized = true.
    pub extern "system" fn XNetworkingGetConnectivityHint(out: u64) -> i32 {
        if out == 0 {
            return E_FAIL;
        }
        let mut hint = [0u8; 24];
        hint[0..4].copy_from_slice(&1u32.to_le_bytes()); // ConnectivityLevel::None (offline)
        hint[16] = 1; // networkInitialized = true
        unsafe { std::ptr::copy_nonoverlapping(hint.as_ptr(), out as *mut u8, hint.len()) };
        S_OK
    }

    pub extern "system" fn XNetworkingRegisterConnectivityHintChanged(_queue: u64, _ctx: u64, _cb: u64, out_token: u64) -> i32 {
        if out_token != 0 {
            unsafe { std::ptr::write_unaligned(out_token as *mut u64, 1) };
        }
        S_OK
    }

    pub extern "system" fn XNetworkingUnregisterConnectivityHintChanged(_token: u64, _wait: bool) -> bool {
        true
    }

    // ---- XDK Memory Allocator (XMemAlloc / XMemFree / XMemSize / TitleMemoryStatus) ----

    static XMEM_ALLOCS: Mutex<Option<std::collections::HashMap<usize, (usize, usize)>>> = Mutex::new(None);

    pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
        let align = crate::era_mem::xmem_alignment_from_attributes(attributes);
        let len = size.max(align);
        let Ok(layout) = std::alloc::Layout::from_size_align(len, align) else {
            return std::ptr::null_mut();
        };
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if !ptr.is_null() {
            let mut g = XMEM_ALLOCS.lock().unwrap_or_else(|e| e.into_inner());
            g.get_or_insert_with(std::collections::HashMap::new).insert(ptr as usize, (len, align));
        }
        ptr as *mut c_void
    }

    pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
        XMemAlloc(size, attributes)
    }

    pub extern "system" fn XMemFree(ptr: *mut c_void, _attributes: u64) {
        if ptr.is_null() {
            return;
        }
        let meta = {
            let mut g = XMEM_ALLOCS.lock().unwrap_or_else(|e| e.into_inner());
            g.as_mut().and_then(|m| m.remove(&(ptr as usize)))
        };
        if let Some((len, align)) = meta {
            if let Ok(layout) = std::alloc::Layout::from_size_align(len, align) {
                unsafe { std::alloc::dealloc(ptr as *mut u8, layout) };
            }
        }
    }

    pub extern "system" fn XMemFreeDefault(ptr: *mut c_void, attributes: u64) {
        XMemFree(ptr, attributes);
    }

    pub extern "system" fn XMemSize(ptr: *mut c_void, _attributes: u64) -> usize {
        if ptr.is_null() {
            return 0;
        }
        let g = XMEM_ALLOCS.lock().unwrap_or_else(|e| e.into_inner());
        g.as_ref().and_then(|m| m.get(&(ptr as usize)).map(|(len, _)| *len)).unwrap_or(0)
    }

    pub extern "system" fn XMemSizeDefault(ptr: *mut c_void, attributes: u64) -> usize {
        XMemSize(ptr, attributes)
    }

    /// Populates Xbox's 64-byte `TITLEMEMORYSTATUS` structure with Series X title memory limits.
    pub extern "system" fn TitleMemoryStatus(out: u64) -> i32 {
        if out == 0 {
            return 0;
        }
        let used: u64 = {
            let g = XMEM_ALLOCS.lock().unwrap_or_else(|e| e.into_inner());
            g.as_ref().map(|m| m.values().map(|(l, _)| *l as u64).sum()).unwrap_or(0)
        };
        let avail = crate::era_mem::SERIES_X_TITLE_RAM_BYTES.saturating_sub(used);
        let st = crate::era_mem::TitleMemoryStatus {
            length: std::mem::size_of::<crate::era_mem::TitleMemoryStatus>() as u32,
            reserved: 0,
            total_mem: crate::era_mem::SERIES_X_TITLE_RAM_BYTES,
            avail_mem: avail,
            legacy_used: used,
            legacy_peak: used,
            legacy_avail: avail,
            title_used: used,
            title_avail: avail,
        };
        unsafe { std::ptr::write_unaligned(out as *mut crate::era_mem::TitleMemoryStatus, st) };
        1
    }

    // ---- Extended XGameSave Read / Delete ----

    /// Reads a previously written blob out of the local save container store.
    pub extern "system" fn XGameSaveReadBlobData(
        container: u64,
        blob_name: u64,
        out_buf: u64,
        buf_size: usize,
        out_written: u64,
    ) -> i32 {
        if blob_name == 0 {
            return E_FAIL;
        }
        let bname = unsafe { text(blob_name) };
        let data = with_saves(|s| {
            let cname = s.containers.get(&container)?.clone();
            if let Some(bytes) = s.blobs.get(&(cname.clone(), bname.clone())) {
                return Some(bytes.clone());
            }
            if let Some(ref root_dir) = s.root_dir {
                let path = root_dir
                    .join(safe_save_segment(&cname))
                    .join(format!("{}.bin", safe_save_segment(&bname)));
                if let Ok(bytes) = std::fs::read(&path) {
                    s.blobs.insert((cname, bname), bytes.clone());
                    return Some(bytes);
                }
            }
            None
        });
        let Some(bytes) = data else { return E_FAIL };
        if out_written != 0 {
            unsafe { std::ptr::write_unaligned(out_written as *mut usize, bytes.len()) };
        }
        if out_buf == 0 || buf_size < bytes.len() {
            return E_FAIL;
        }
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf as *mut u8, bytes.len()) };
        S_OK
    }

    pub extern "system" fn XGameSaveSubmitBlobDelete(update: u64, blob_name: u64) -> i32 {
        if blob_name == 0 {
            return E_FAIL;
        }
        let bname = unsafe { text(blob_name) };
        let ok = with_saves(|s| {
            let (cname, _) = s.updates.get(&update)?;
            let cname = cname.clone();
            if let Some(ref root_dir) = s.root_dir {
                let path = root_dir
                    .join(safe_save_segment(&cname))
                    .join(format!("{}.bin", safe_save_segment(&bname)));
                let _ = std::fs::remove_file(path);
            }
            s.blobs.remove(&(cname, bname));
            Some(())
        });
        if ok.is_some() { S_OK } else { E_FAIL }
    }

    pub extern "system" fn XGameSaveDeleteContainer(provider: u64, name: u64) -> i32 {
        if name == 0 || !with_saves(|s| s.providers.contains(&provider)) {
            return E_FAIL;
        }
        let cname = unsafe { text(name) };
        with_saves(|s| {
            if let Some(ref root_dir) = s.root_dir {
                let _ = std::fs::remove_dir_all(root_dir.join(safe_save_segment(&cname)));
            }
            s.blobs.retain(|(c, _), _| c != &cname);
        });
        S_OK
    }

    // ---- XGameFeature / XDisplay / XThread / XUser extras / Xbox Graphics Bridge ----

    pub extern "system" fn XGameFeatureInitialize() -> i32 {
        S_OK
    }

    pub extern "system" fn XGameRuntimeIsFeatureAvailable(_feature: u32) -> bool {
        true
    }

    pub extern "system" fn XDisplayAcquireTimeoutDeferral(out: u64) -> i32 {
        if out != 0 {
            unsafe { std::ptr::write_unaligned(out as *mut u64, 1) };
        }
        S_OK
    }

    pub extern "system" fn XDisplayCloseTimeoutDeferralHandle(_handle: u64) {}

    pub extern "system" fn XDisplayGetHdrMode() -> u32 {
        0 // SDR
    }

    pub extern "system" fn XThreadAssertNotTimeSensitive() {}

    pub extern "system" fn XThreadIsTimeSensitive() -> bool {
        false
    }

    pub extern "system" fn XThreadSetTimeSensitive(_time_sensitive: bool) -> i32 {
        S_OK
    }

    pub extern "system" fn XUserRegisterForChangeEvent(_queue: u64, _ctx: u64, _cb: u64, out_token: u64) -> i32 {
        if out_token != 0 {
            unsafe { std::ptr::write_unaligned(out_token as *mut u64, 1) };
        }
        S_OK
    }

    pub extern "system" fn XUserUnregisterForChangeEvent(_token: u64, _wait: bool) -> bool {
        true
    }

    pub extern "system" fn XUserDuplicateHandle(handle: u64, out: u64) -> i32 {
        if out == 0 {
            return E_FAIL;
        }
        unsafe { std::ptr::write_unaligned(out as *mut u64, handle) };
        S_OK
    }

    pub extern "system" fn XUserGetLocalId(_user: u64, out: u64) -> i32 {
        if out == 0 {
            return E_FAIL;
        }
        unsafe { std::ptr::write_unaligned(out as *mut u64, 1) };
        S_OK
    }

    pub extern "system" fn XDeviceCreate(_params: u64, out_device: u64) -> i32 {
        if out_device == 0 {
            return E_FAIL;
        }
        unsafe { std::ptr::write_unaligned(out_device as *mut u64, 0xD3D1_2001) };
        S_OK
    }

    pub extern "system" fn XGComputeOptimalTileMode(
        _resource_dim: u32,
        _format: u32,
        _width: u32,
        _height: u32,
        _depth: u32,
        _bind_flags: u32,
    ) -> u32 {
        crate::gpu_bridge::XgTileMode::Thin2D as u32
    }

    /// Minimal D3D12XBOX_CreateDevice bridge: forwards to desktop `d3d12.dll!D3D12CreateDevice`
    /// with default adapter and D3D_FEATURE_LEVEL_12_0 when available.
    pub extern "system" fn D3D12XBOX_CreateDevice(_params: u64, riid: u64, pp_device: u64) -> i32 {
        if pp_device == 0 {
            return E_FAIL;
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn LoadLibraryA(name: *const u8) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        }
        let d3d12 = unsafe { LoadLibraryA(b"d3d12.dll\0".as_ptr()) };
        if d3d12.is_null() {
            return E_FAIL;
        }
        let sym = unsafe { GetProcAddress(d3d12, b"D3D12CreateDevice\0".as_ptr()) };
        if sym.is_null() {
            return E_FAIL;
        }
        let create: extern "system" fn(*mut c_void, u32, u64, u64) -> i32 = unsafe { std::mem::transmute(sym) };
        // D3D_FEATURE_LEVEL_12_0 = 0xC000
        create(std::ptr::null_mut(), 0xC000, riid, pp_device)
    }

    pub extern "system" fn D3D11XCreateDeviceX(_params: u64, pp_device: u64, pp_context: u64) -> i32 {
        if pp_device != 0 {
            unsafe { std::ptr::write_unaligned(pp_device as *mut u64, 0) };
        }
        if pp_context != 0 {
            unsafe { std::ptr::write_unaligned(pp_context as *mut u64, 0) };
        }
        E_FAIL
    }

    /// The Xbox-only function a game imported, by the name it used, or None if we don't have it.
    pub fn resolve(name: &str) -> Option<*mut c_void> {
        let p: *mut c_void = match name {
            _ if name.eq_ignore_ascii_case("XGameRuntimeInitialize") => XGameRuntimeInitialize as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameRuntimeUninitialize") => XGameRuntimeUninitialize as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameRuntimePrint") => XGameRuntimePrint as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameRuntimeSleep") => XGameRuntimeSleep as *mut c_void,
            _ if name.eq_ignore_ascii_case("XSystemGetXboxLiveSandboxId") => XSystemGetXboxLiveSandboxId as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameRuntimeGetXboxLiveSandboxId") => XGameRuntimeGetXboxLiveSandboxId as *mut c_void,
            _ if name.eq_ignore_ascii_case("XSystemGetDeviceType") => XSystemGetDeviceType as *mut c_void,
            _ if name.eq_ignore_ascii_case("XSystemGetConsoleId") => XSystemGetConsoleId as *mut c_void,
            _ if name.eq_ignore_ascii_case("XSystemGetAppSpecificDeviceId") => XSystemGetAppSpecificDeviceId as *mut c_void,
            _ if name.eq_ignore_ascii_case("XSystemIsHandleValid") => XSystemIsHandleValid as *mut c_void,
            _ if name.eq_ignore_ascii_case("XDisplayTryAcquireTimeoutStatus") => XDisplayTryAcquireTimeoutStatus as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameGetXboxTitleId") => XGameGetXboxTitleId as *mut c_void,
            _ if name.eq_ignore_ascii_case("XPackageGetCurrentProcessPackageIdentifier") => XPackageGetCurrentProcessPackageIdentifier as *mut c_void,
            _ if name.eq_ignore_ascii_case("XUserGetGamertag") => XUserGetGamertag as *mut c_void,
            _ if name.eq_ignore_ascii_case("XUserGetId") => XUserGetId as *mut c_void,
            _ if name.eq_ignore_ascii_case("XUserGetState") => XUserGetState as *mut c_void,
            _ if name.eq_ignore_ascii_case("XUserGetAgeGroup") => XUserGetAgeGroup as *mut c_void,
            _ if name.eq_ignore_ascii_case("XUserCheckPrivilege") => XUserCheckPrivilege as *mut c_void,
            _ if name.eq_ignore_ascii_case("XUserCloseHandle") => XUserCloseHandle as *mut c_void,
            _ if name.eq_ignore_ascii_case("XUserAddAsync") => XUserAddAsync as *mut c_void,
            _ if name.eq_ignore_ascii_case("XUserAddResult") => XUserAddResult as *mut c_void,
            _ if name.eq_ignore_ascii_case("XAsyncGetStatus") => XAsyncGetStatus as *mut c_void,
            _ if name.eq_ignore_ascii_case("XAsyncGetResultSize") => XAsyncGetResultSize as *mut c_void,
            _ if name.eq_ignore_ascii_case("XAsyncCancel") => XAsyncCancel as *mut c_void,
            _ if name.eq_ignore_ascii_case("XStorageCreateHandle") => XStorageCreateHandle as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameSaveInitializeProvider") => XGameSaveInitializeProvider as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameSaveCloseProvider") => XGameSaveCloseProvider as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameSaveGetRemainingQuota") => XGameSaveGetRemainingQuota as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameSaveCreateContainer") => XGameSaveCreateContainer as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameSaveCloseContainer") => XGameSaveCloseContainer as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameSaveCreateUpdate") => XGameSaveCreateUpdate as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameSaveSubmitBlobWrite") => XGameSaveSubmitBlobWrite as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameSaveSubmitUpdate") => XGameSaveSubmitUpdate as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameSaveCloseUpdate") => XGameSaveCloseUpdate as *mut c_void,
            _ if name.eq_ignore_ascii_case("XTaskQueueCreate") => XTaskQueueCreate as *mut c_void,
            _ if name.eq_ignore_ascii_case("XTaskQueueCloseHandle") => XTaskQueueCloseHandle as *mut c_void,
            _ if name.eq_ignore_ascii_case("XTaskQueueGetCurrentProcessTaskQueue") => XTaskQueueGetCurrentProcessTaskQueue as *mut c_void,
            _ if name.eq_ignore_ascii_case("XTaskQueueSubmitCallback") => XTaskQueueSubmitCallback as *mut c_void,
            _ if name.eq_ignore_ascii_case("XTaskQueueDispatch") => XTaskQueueDispatch as *mut c_void,
            _ if name.eq_ignore_ascii_case("XMemAlloc") => XMemAlloc as *mut c_void,
            _ if name.eq_ignore_ascii_case("XMemAllocDefault") => XMemAllocDefault as *mut c_void,
            _ if name.eq_ignore_ascii_case("XMemFree") => XMemFree as *mut c_void,
            _ if name.eq_ignore_ascii_case("XMemFreeDefault") => XMemFreeDefault as *mut c_void,
            _ if name.eq_ignore_ascii_case("XMemSize") => XMemSize as *mut c_void,
            _ if name.eq_ignore_ascii_case("XMemSizeDefault") => XMemSizeDefault as *mut c_void,
            _ if name.eq_ignore_ascii_case("TitleMemoryStatus") => TitleMemoryStatus as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameSaveReadBlobData") => XGameSaveReadBlobData as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameSaveSubmitBlobDelete") => XGameSaveSubmitBlobDelete as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameSaveDeleteContainer") => XGameSaveDeleteContainer as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameFeatureInitialize") => XGameFeatureInitialize as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameRuntimeIsFeatureAvailable") => XGameRuntimeIsFeatureAvailable as *mut c_void,
            _ if name.eq_ignore_ascii_case("XDisplayAcquireTimeoutDeferral") => XDisplayAcquireTimeoutDeferral as *mut c_void,
            _ if name.eq_ignore_ascii_case("XDisplayCloseTimeoutDeferralHandle") => XDisplayCloseTimeoutDeferralHandle as *mut c_void,
            _ if name.eq_ignore_ascii_case("XDisplayGetHdrMode") => XDisplayGetHdrMode as *mut c_void,
            _ if name.eq_ignore_ascii_case("XThreadAssertNotTimeSensitive") => XThreadAssertNotTimeSensitive as *mut c_void,
            _ if name.eq_ignore_ascii_case("XThreadIsTimeSensitive") => XThreadIsTimeSensitive as *mut c_void,
            _ if name.eq_ignore_ascii_case("XThreadSetTimeSensitive") => XThreadSetTimeSensitive as *mut c_void,
            _ if name.eq_ignore_ascii_case("XUserRegisterForChangeEvent") => XUserRegisterForChangeEvent as *mut c_void,
            _ if name.eq_ignore_ascii_case("XUserUnregisterForChangeEvent") => XUserUnregisterForChangeEvent as *mut c_void,
            _ if name.eq_ignore_ascii_case("XUserDuplicateHandle") => XUserDuplicateHandle as *mut c_void,
            _ if name.eq_ignore_ascii_case("XUserGetLocalId") => XUserGetLocalId as *mut c_void,
            _ if name.eq_ignore_ascii_case("XDeviceCreate") => XDeviceCreate as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGComputeOptimalTileMode") => XGComputeOptimalTileMode as *mut c_void,
            _ if name.eq_ignore_ascii_case("D3D12XBOX_CreateDevice") => D3D12XBOX_CreateDevice as *mut c_void,
            _ if name.eq_ignore_ascii_case("D3D11XCreateDeviceX") => D3D11XCreateDeviceX as *mut c_void,
            _ if name.eq_ignore_ascii_case("XUserGetLocale") => XUserGetLocale as *mut c_void,
            _ if name.eq_ignore_ascii_case("XStoreCreateContext") => XStoreCreateContext as *mut c_void,
            _ if name.eq_ignore_ascii_case("XStoreCloseContextHandle") => XStoreCloseContextHandle as *mut c_void,
            _ if name.eq_ignore_ascii_case("XStoreIsAvailabilityPurchasable") => XStoreIsAvailabilityPurchasable as *mut c_void,
            _ if name.eq_ignore_ascii_case("XStoreAcquireLicenseForPackageSync") => XStoreAcquireLicenseForPackageSync as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameUiShowMessageDialogAsync") => XGameUiShowMessageDialogAsync as *mut c_void,
            _ if name.eq_ignore_ascii_case("XGameUiShowMessageDialogResult") => XGameUiShowMessageDialogResult as *mut c_void,
            _ if name.eq_ignore_ascii_case("XLaunchNewGame") => XLaunchNewGame as *mut c_void,
            _ if name.eq_ignore_ascii_case("XNetworkingGetConnectivityHint") => XNetworkingGetConnectivityHint as *mut c_void,
            _ if name.eq_ignore_ascii_case("XNetworkingRegisterConnectivityHintChanged") => XNetworkingRegisterConnectivityHintChanged as *mut c_void,
            _ if name.eq_ignore_ascii_case("XNetworkingUnregisterConnectivityHintChanged") => XNetworkingUnregisterConnectivityHintChanged as *mut c_void,
            _ => return None,
        };
        BOUND.lock().unwrap_or_else(|e| e.into_inner()).push(name.to_string());
        Some(p)
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
pub use win::*;

#[cfg(test)]
mod tests {
    #[cfg(all(windows, target_arch = "x86_64"))]
    use super::*;

    #[test]
    fn resolves_the_startup_calls() {
        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            reset();
            assert!(resolve("XGameRuntimeInitialize").is_some());
            assert!(resolve("xgameRuntimeinitialize").is_some());
            assert!(resolve("NoSuchXboxCall").is_none());
            assert!(bound().contains(&"XGameRuntimeInitialize".to_string()));
        }
    }

    #[test]
    fn the_game_runtime_starts_and_stops() {
        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            assert_eq!(XGameRuntimeInitialize(0, 0), S_OK);
            assert_eq!(XGameRuntimeUninitialize(), S_OK);
        }
    }

    #[test]
    fn saved_data_reports_that_it_is_unavailable() {
        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            let mut out = 1u64;
            assert_eq!(XStorageCreateHandle(0, 0, 0, &mut out as *mut u64 as u64), E_FAIL);
            assert_eq!(out, 0);
        }
    }

    #[test]
    fn sandbox_is_reported_as_developer() {
        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            let mut id = 0u32;
            assert_eq!(XSystemGetXboxLiveSandboxId(&mut id as *mut u32 as u64), S_OK);
            assert_eq!(id, 2);
        }
    }

    #[test]
    fn a_gamertag_is_provided() {
        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            let mut buf = [0u16; 32];
            assert_eq!(XUserGetGamertag(0, buf.as_mut_ptr() as u64, 32), S_OK);
            let end = buf.iter().position(|&c| c == 0).unwrap_or(0);
            assert_eq!(String::from_utf16_lossy(&buf[..end]), "LocalPlayer");
        }
    }

    #[test]
    fn task_queue_submits_and_dispatches_callbacks() {
        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            extern "system" fn cb(ctx: u64, canceled: bool) {
                assert!(!canceled);
                unsafe { *(ctx as *mut u32) += 7 };
            }
            let mut q = 0u64;
            assert_eq!(XTaskQueueCreate(0, 0, &mut q as *mut u64 as u64), S_OK);
            assert!(XSystemIsHandleValid(q));
            let mut val = 35u32;
            assert_eq!(XTaskQueueSubmitCallback(q, 0, &mut val as *mut u32 as u64, cb as *const () as u64), S_OK);
            assert!(XTaskQueueDispatch(q, 0, 0));
            assert_eq!(val, 42);
            assert!(!XTaskQueueDispatch(q, 0, 0));
            XTaskQueueCloseHandle(q);
        }
    }

    #[test]
    fn system_and_user_queries_succeed() {
        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            assert_eq!(XSystemGetDeviceType(), 7);
            let mut xuid = 0u64;
            assert_eq!(XUserGetId(1, &mut xuid as *mut u64 as u64), S_OK);
            assert_ne!(xuid, 0);
            let mut has = 0u8;
            let mut reason = 99u32;
            assert_eq!(XUserCheckPrivilege(1, 0, 0, &mut has as *mut u8 as u64, &mut reason as *mut u32 as u64), S_OK);
            assert_eq!(has, 1);
            assert_eq!(reason, 0);
            let mut cid = [0u8; 64];
            let mut used = 0usize;
            assert_eq!(XSystemGetConsoleId(cid.len(), cid.as_mut_ptr() as u64, &mut used as *mut usize as u64), S_OK);
            assert!(used > 10);
            assert!(resolve("XTaskQueueCreate").is_some());
            assert!(resolve("XSystemGetDeviceType").is_some());
            assert!(resolve("XUserAddAsync").is_some());
            assert!(resolve("XMemAlloc").is_some());
        }
    }

    #[test]
    fn user_add_async_and_xmem_alloc_work_end_to_end() {
        #[cfg(all(windows, target_arch = "x86_64"))]
        {
            #[repr(C)]
            struct AsyncBlock {
                queue: u64,
                context: u64,
                callback: u64,
                internal: [u8; 32],
            }
            extern "system" fn on_done(block: u64) {
                let b = unsafe { &*(block as *const AsyncBlock) };
                let mut user = 0u64;
                assert_eq!(XUserAddResult(block, &mut user as *mut u64 as u64), S_OK);
                unsafe { *(b.context as *mut u64) = user };
            }
            let mut q = 0u64;
            assert_eq!(XTaskQueueCreate(0, 0, &mut q as *mut u64 as u64), S_OK);
            let mut signed_in_user = 0u64;
            let mut blk = AsyncBlock {
                queue: q,
                context: &mut signed_in_user as *mut u64 as u64,
                callback: on_done as *const () as u64,
                internal: [0; 32],
            };
            assert_eq!(XUserAddAsync(0, &mut blk as *mut AsyncBlock as u64), S_OK);
            assert!(XTaskQueueDispatch(q, 1, 0));
            assert_eq!(signed_in_user, 1);
            XTaskQueueCloseHandle(q);

            let p = XMemAlloc(256, 0);
            assert!(!p.is_null());
            XMemFree(p, 0);

            let mut loc = [0u8; 16];
            let mut used = 0usize;
            assert_eq!(XUserGetLocale(loc.len(), loc.as_mut_ptr() as u64, &mut used as *mut usize as u64), S_OK);
            assert_eq!(&loc[..5], b"en-US");
            let mut hint = [0u8; 24];
            assert_eq!(XNetworkingGetConnectivityHint(hint.as_mut_ptr() as u64), S_OK);
            assert_eq!(hint[16], 1);
            let mut store = 0u64;
            assert_eq!(XStoreCreateContext(1, &mut store as *mut u64 as u64), S_OK);
            assert_eq!(store, 1);

            // XGameSave full container/update/blob round-trip
            let mut prov = 0u64;
            assert_eq!(XGameSaveInitializeProvider(1, 0, false, &mut prov as *mut u64 as u64), S_OK);
            let mut quota = 0i64;
            assert_eq!(XGameSaveGetRemainingQuota(prov, &mut quota as *mut i64 as u64), S_OK);
            assert_eq!(quota, 64 * 1024 * 1024);
            let cname = b"slot0\0";
            let mut cont = 0u64;
            assert_eq!(XGameSaveCreateContainer(prov, cname.as_ptr() as u64, &mut cont as *mut u64 as u64), S_OK);
            let mut upd = 0u64;
            assert_eq!(XGameSaveCreateUpdate(cont, cname.as_ptr() as u64, &mut upd as *mut u64 as u64), S_OK);
            let bname = b"save.bin\0";
            let payload = [1u8, 2, 3, 4];
            assert_eq!(XGameSaveSubmitBlobWrite(upd, bname.as_ptr() as u64, payload.as_ptr() as u64, payload.len()), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(upd), S_OK);

            let mut readback = [0u8; 8];
            let mut written = 0usize;
            assert_eq!(
                XGameSaveReadBlobData(
                    cont,
                    bname.as_ptr() as u64,
                    readback.as_mut_ptr() as u64,
                    readback.len(),
                    &mut written as *mut usize as u64,
                ),
                S_OK
            );
            assert_eq!(written, 4);
            assert_eq!(&readback[..4], &payload);

            assert_eq!(XGameSaveSubmitBlobDelete(upd, bname.as_ptr() as u64), S_OK);
            assert_eq!(
                XGameSaveReadBlobData(cont, bname.as_ptr() as u64, readback.as_mut_ptr() as u64, readback.len(), 0),
                E_FAIL
            );

            XGameSaveCloseUpdate(upd);
            XGameSaveCloseContainer(cont);
            XGameSaveCloseProvider(prov);

            // Aligned XMemAlloc + XMemSize + TitleMemoryStatus + Graphics shims
            let aligned = XMemAlloc(128, 1); // 4 KiB aligned
            assert!(!aligned.is_null());
            assert_eq!((aligned as usize) % 4096, 0);
            assert_eq!(XMemSize(aligned, 1), 4096);
            let mut mem_status = crate::era_mem::TitleMemoryStatus::default();
            assert_eq!(TitleMemoryStatus(&mut mem_status as *mut _ as u64), 1);
            assert_eq!(mem_status.total_mem, crate::era_mem::SERIES_X_TITLE_RAM_BYTES);
            assert!(mem_status.title_used >= 4096);
            XMemFree(aligned, 1);
            assert_eq!(XMemSize(aligned, 1), 0);

            let mut dev = 0u64;
            assert_eq!(XDeviceCreate(0, &mut dev as *mut u64 as u64), S_OK);
            assert_ne!(dev, 0);
            assert_eq!(XGComputeOptimalTileMode(3, 28, 256, 256, 1, 8), crate::gpu_bridge::XgTileMode::Thin2D as u32);
        }
    }

    #[test]
    fn xgamesave_persists_blobs_to_disk_and_reloads() {
        #[cfg(windows)]
        {
            use win::*;
            let root = std::env::temp_dir().join(format!("xone_save_persist_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            set_save_root(Some(root.clone()));

            let mut prov = 0u64;
            let mut cont = 0u64;
            let mut upd = 0u64;
            let cname = b"ProfileSlot1\0";
            let bname = b"progress.dat\0";
            let payload = b"LEVEL=7;SCORE=9900";
            assert_eq!(XGameSaveInitializeProvider(1, 0, false, &mut prov as *mut u64 as u64), S_OK);
            assert_eq!(XGameSaveCreateContainer(prov, cname.as_ptr() as u64, &mut cont as *mut u64 as u64), S_OK);
            assert_eq!(XGameSaveCreateUpdate(cont, cname.as_ptr() as u64, &mut upd as *mut u64 as u64), S_OK);
            assert_eq!(XGameSaveSubmitBlobWrite(upd, bname.as_ptr() as u64, payload.as_ptr() as u64, payload.len()), S_OK);
            assert_eq!(XGameSaveSubmitUpdate(upd), S_OK);
            XGameSaveCloseUpdate(upd);
            XGameSaveCloseContainer(cont);
            XGameSaveCloseProvider(prov);

            // Clear in-memory cache and reload from disk.
            set_save_root(Some(root.clone()));
            assert_eq!(XGameSaveInitializeProvider(1, 0, false, &mut prov as *mut u64 as u64), S_OK);
            assert_eq!(XGameSaveCreateContainer(prov, cname.as_ptr() as u64, &mut cont as *mut u64 as u64), S_OK);
            let mut buf = [0u8; 64];
            let mut written = 0usize;
            assert_eq!(
                XGameSaveReadBlobData(
                    cont,
                    bname.as_ptr() as u64,
                    buf.as_mut_ptr() as u64,
                    buf.len(),
                    &mut written as *mut usize as u64,
                ),
                S_OK
            );
            assert_eq!(&buf[..written], payload);
            assert_eq!(XGameSaveDeleteContainer(prov, cname.as_ptr() as u64), S_OK);
            XGameSaveCloseContainer(cont);
            XGameSaveCloseProvider(prov);

            set_save_root(None);
            let _ = std::fs::remove_dir_all(&root);
        }
    }
}