//! Clean-room Xbox-to-Windows Native Runtime Companion DLL (`x2w.dll`).
//!
//! When an Xbox One / Series X|S PE64 executable is statically converted into a native Windows
//! `.exe` by `xboxone::exe_convert`, console-only DLL imports (`d3d12_xs.dll`, `dstorage_xs.dll`,
//! `xgameruntime.dll`, `xgameplatform.dll`, `xg_xs.dll`, `xmem.dll`, `acphal.dll`, `xapu.dll`,
//! `xcurl.dll`) are rewritten in-place to `x2w.dll`.
//!
//! Dropping `x2w.dll` beside the converted `.exe` allows the Windows PE loader to resolve those
//! Xbox console exports directly to our clean-room implementations without missing-DLL errors.

#![allow(non_snake_case)]

#[cfg(all(windows, target_arch = "x86_64"))]
mod exports {
    use std::ffi::c_void;
    use xboxone::shims;

    // ---- Runtime & Diagnostics ----

    #[no_mangle]
    pub extern "system" fn XGameRuntimeInitialize(package: u64, features: u64) -> i32 {
        shims::XGameRuntimeInitialize(package, features)
    }

    #[no_mangle]
    pub extern "system" fn XGameRuntimeUninitialize() -> i32 {
        shims::XGameRuntimeUninitialize()
    }

    #[no_mangle]
    pub extern "system" fn XErrorReport(error: u32, message: u64) -> i32 {
        shims::XErrorReport(error, message)
    }

    #[no_mangle]
    pub extern "system" fn XGameRuntimePrint(mask: u64, format: u64, args: u64) -> u32 {
        shims::XGameRuntimePrint(mask, format, args)
    }

    #[no_mangle]
    pub extern "system" fn XGameRuntimeSleep(ticks: u64) {
        shims::XGameRuntimeSleep(ticks)
    }

    #[no_mangle]
    pub extern "system" fn XGameFeatureInitialize() -> i32 {
        shims::XGameFeatureInitialize()
    }

    #[no_mangle]
    pub extern "system" fn XGameRuntimeIsFeatureAvailable(feature: u32) -> bool {
        shims::XGameRuntimeIsFeatureAvailable(feature)
    }

    // ---- Memory (XDK / GDK / Era Virtual & Physical Allocators) ----

    #[no_mangle]
    pub extern "system" fn XMemVirtualAlloc(
        address: usize,
        size: usize,
        flags: u32,
        attributes: u64,
        protection: u32,
    ) -> *mut c_void {
        shims::guest_xmem_virtual_alloc(address, size, flags, attributes, protection)
    }

    #[no_mangle]
    pub unsafe extern "system" fn XMemAllocatePhysicalPages(
        attributes: u64,
        count: *mut usize,
        output: *mut usize,
    ) -> i32 {
        unsafe { xboxone::native_pages::allocate_pages(attributes, count, output) }
    }

    #[no_mangle]
    pub unsafe extern "system" fn XMemFreePhysicalPages(count: usize, ids: *const usize) -> i32 {
        unsafe { xboxone::native_pages::free_pages(count, ids) }
    }

    #[no_mangle]
    pub unsafe extern "system" fn XMemMapPhysicalPages(address: usize, count: usize, ids: *const usize) -> usize {
        unsafe { xboxone::native_pages::map_pages(address, count, ids) }
    }

    #[no_mangle]
    pub unsafe extern "system" fn XMemMapPhysicalPagesScatter(
        addresses: *const usize,
        count: u32,
        ids: *const usize,
    ) -> i32 {
        unsafe { xboxone::native_pages::map_pages_scatter(addresses, count, ids) }
    }

    #[no_mangle]
    pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
        shims::XMemAlloc(size, attributes)
    }

    #[no_mangle]
    pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
        shims::XMemAllocDefault(size, attributes)
    }

    #[no_mangle]
    pub extern "system" fn XMemFree(ptr: *mut c_void, attributes: u64) {
        shims::XMemFree(ptr, attributes)
    }

    #[no_mangle]
    pub extern "system" fn XMemFreeDefault(ptr: *mut c_void, attributes: u64) {
        shims::XMemFreeDefault(ptr, attributes)
    }

    #[no_mangle]
    pub extern "system" fn XMemSize(ptr: *mut c_void, attributes: u64) -> usize {
        shims::XMemSize(ptr, attributes)
    }

    #[no_mangle]
    pub extern "system" fn XMemSizeDefault(ptr: *mut c_void, attributes: u64) -> usize {
        shims::XMemSizeDefault(ptr, attributes)
    }

    #[no_mangle]
    pub extern "system" fn TitleMemoryStatus(out: u64) -> i32 {
        shims::TitleMemoryStatus(out)
    }

    // ---- Direct3D 12.X / 12.XS & XG Graphics Bridge ----

    #[no_mangle]
    pub extern "system" fn D3D12XBOX_CreateDevice(params: u64, riid: u64, pp_device: u64) -> i32 {
        shims::D3D12XBOX_CreateDevice(params, riid, pp_device)
    }

    /// The converted title imports this spelling; Windows export lookup is case-sensitive.
    #[no_mangle]
    pub extern "system" fn D3D12XboxCreateDevice(params: u64, riid: u64, pp_device: u64) -> i32 {
        shims::D3D12XBOX_CreateDevice(params, riid, pp_device)
    }

    #[no_mangle]
    pub extern "system" fn D3D12ConfigureVirtualMemoryX(config: u64) -> i32 {
        shims::D3D12ConfigureVirtualMemoryX(config)
    }

    #[no_mangle]
    pub extern "system" fn D3D12CreateDevice(
        _adapter: *mut c_void,
        _min_feature_level: u32,
        riid: u64,
        pp_device: u64,
    ) -> i32 {
        shims::D3D12XBOX_CreateDevice(0, riid, pp_device)
    }

    #[no_mangle]
    pub extern "system" fn D3D12EnableExperimentalFeatures(
        num_features: u32,
        iids: *const c_void,
        configs: *mut c_void,
        config_sizes: *mut u32,
    ) -> i32 {
        #[link(name = "kernel32")]
        extern "system" {
            fn LoadLibraryA(name: *const u8) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        }
        let d3d12 = unsafe { LoadLibraryA(b"d3d12.dll\0".as_ptr()) };
        if !d3d12.is_null() {
            let sym = unsafe { GetProcAddress(d3d12, b"D3D12EnableExperimentalFeatures\0".as_ptr()) };
            if !sym.is_null() {
                let func: extern "system" fn(u32, *const c_void, *mut c_void, *mut u32) -> i32 =
                    unsafe { std::mem::transmute(sym) };
                return func(num_features, iids, configs, config_sizes);
            }
        }
        shims::E_FAIL
    }

    #[no_mangle]
    pub extern "system" fn D3D12CreateRootSignatureDeserializer(
        src: *const c_void,
        src_len: usize,
        riid: *const u8,
        out: *mut *mut c_void,
    ) -> i32 {
        #[link(name = "kernel32")]
        extern "system" {
            fn LoadLibraryA(name: *const u8) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        }
        let d3d12 = unsafe { LoadLibraryA(b"d3d12.dll\0".as_ptr()) };
        if !d3d12.is_null() {
            let sym = unsafe { GetProcAddress(d3d12, b"D3D12CreateRootSignatureDeserializer\0".as_ptr()) };
            if !sym.is_null() {
                let func: extern "system" fn(*const c_void, usize, *const u8, *mut *mut c_void) -> i32 =
                    unsafe { std::mem::transmute(sym) };
                return func(src, src_len, riid, out);
            }
        }
        shims::E_FAIL
    }

    #[no_mangle]
    pub extern "system" fn D3D12CreateVersionedRootSignatureDeserializer(
        src: *const c_void,
        src_len: usize,
        riid: *const u8,
        out: *mut *mut c_void,
    ) -> i32 {
        #[link(name = "kernel32")]
        extern "system" {
            fn LoadLibraryA(name: *const u8) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        }
        let d3d12 = unsafe { LoadLibraryA(b"d3d12.dll\0".as_ptr()) };
        if !d3d12.is_null() {
            let sym = unsafe { GetProcAddress(d3d12, b"D3D12CreateVersionedRootSignatureDeserializer\0".as_ptr()) };
            if !sym.is_null() {
                let func: extern "system" fn(*const c_void, usize, *const u8, *mut *mut c_void) -> i32 =
                    unsafe { std::mem::transmute(sym) };
                return func(src, src_len, riid, out);
            }
        }
        shims::E_FAIL
    }

    #[no_mangle]
    pub extern "system" fn D3D12GetInterface(rclsid: *const u8, riid: *const u8, out: *mut *mut c_void) -> i32 {
        #[link(name = "kernel32")]
        extern "system" {
            fn LoadLibraryA(name: *const u8) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        }
        let d3d12 = unsafe { LoadLibraryA(b"d3d12.dll\0".as_ptr()) };
        if !d3d12.is_null() {
            let sym = unsafe { GetProcAddress(d3d12, b"D3D12GetInterface\0".as_ptr()) };
            if !sym.is_null() {
                let func: extern "system" fn(*const u8, *const u8, *mut *mut c_void) -> i32 =
                    unsafe { std::mem::transmute(sym) };
                return func(rclsid, riid, out);
            }
        }
        shims::E_FAIL
    }

    #[no_mangle]
    pub unsafe extern "system" fn XMemVirtualFree(address: *mut c_void, size: usize, flags: u32) -> i32 {
        xboxone::native_pages::guest_virtual_free(address, size, flags)
    }

    #[no_mangle]
    pub unsafe extern "system" fn XMemVirtualProtect(
        address: *mut c_void,
        size: usize,
        new_protect: u32,
        old_protect: *mut u32,
    ) -> i32 {
        #[link(name = "kernel32")]
        extern "system" {
            fn VirtualProtect(addr: *mut c_void, sz: usize, np: u32, op: *mut u32) -> i32;
        }
        VirtualProtect(address, size, new_protect, old_protect)
    }

    #[no_mangle]
    pub extern "system" fn ApuIsMemoryInApuRange(_va: u64) -> bool {
        true
    }

    #[no_mangle]
    pub extern "system" fn ApuMapVirtualAddress(va: u64, out_apu: *mut u32) -> i32 {
        if !out_apu.is_null() {
            unsafe { *out_apu = va as u32 };
        }
        shims::S_OK
    }

    #[no_mangle]
    pub extern "system" fn DStorageCreateCompressionCodec(
        _format: u32,
        _num_threads: u32,
        _riid: *const u8,
        out: *mut *mut c_void,
    ) -> i32 {
        if !out.is_null() {
            unsafe { *out = std::ptr::null_mut() };
        }
        0x8000_4001u32 as i32
    }

    #[no_mangle]
    pub extern "system" fn D3D12SerializeRootSignature(
        desc: u64,
        version: u32,
        out_blob: u64,
        out_err: u64,
    ) -> i32 {
        shims::D3D12SerializeRootSignatureX(desc, version, out_blob, out_err)
    }

    #[no_mangle]
    pub unsafe extern "system" fn D3D12SerializeVersionedRootSignature(
        description: *const c_void,
        blob: *mut *mut c_void,
        error_blob: *mut *mut c_void,
    ) -> i32 {
        type Serializer = unsafe extern "system" fn(
            *const c_void,
            *mut *mut c_void,
            *mut *mut c_void,
        ) -> i32;
        const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x0000_0800;
        static SERIALIZER: std::sync::OnceLock<Option<Serializer>> = std::sync::OnceLock::new();

        #[link(name = "kernel32")]
        extern "system" {
            fn LoadLibraryExA(name: *const u8, file: *mut c_void, flags: u32) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        }

        let serializer = SERIALIZER.get_or_init(|| unsafe {
            let module = LoadLibraryExA(
                b"d3d12.dll\0".as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            );
            if module.is_null() {
                return None;
            }
            let address = GetProcAddress(module, b"D3D12SerializeVersionedRootSignature\0".as_ptr());
            if address.is_null() {
                return None;
            }
            Some(std::mem::transmute::<*mut c_void, Serializer>(address))
        });
        match serializer {
            Some(serialize) => unsafe { serialize(description, blob, error_blob) },
            None => shims::E_FAIL,
        }
    }

    #[no_mangle]
    pub extern "system" fn D3D12GetDebugInterface(riid: u64, out_debug: u64) -> i32 {
        #[link(name = "kernel32")]
        extern "system" {
            fn LoadLibraryA(name: *const u8) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        }
        let d3d12 = unsafe { LoadLibraryA(b"d3d12.dll\0".as_ptr()) };
        if !d3d12.is_null() {
            let sym = unsafe { GetProcAddress(d3d12, b"D3D12GetDebugInterface\0".as_ptr()) };
            if !sym.is_null() {
                let func: extern "system" fn(u64, u64) -> i32 = unsafe { std::mem::transmute(sym) };
                return func(riid, out_debug);
            }
        }
        shims::E_FAIL
    }

    #[no_mangle]
    pub extern "system" fn XDeviceCreate(params: u64, out_device: u64) -> i32 {
        shims::XDeviceCreate(params, out_device)
    }

    #[no_mangle]
    pub extern "system" fn XGComputeOptimalTileMode(
        dim: u32,
        fmt: u32,
        w: u32,
        h: u32,
        d: u32,
        bind: u32,
    ) -> u32 {
        shims::XGComputeOptimalTileMode(dim, fmt, w, h, d, bind)
    }

    #[no_mangle]
    pub extern "system" fn SetFrameIntervalX(device: u64, interval: u32, period: u32, flags: u32) -> i32 {
        shims::SetFrameIntervalX(device, interval, period, flags)
    }

    #[no_mangle]
    pub extern "system" fn ScheduleFrameEventX(device: u64, event: u32, offset: u32, handle: u64, flags: u32) -> i32 {
        shims::ScheduleFrameEventX(device, event, offset, handle, flags)
    }

    #[no_mangle]
    pub extern "system" fn WaitFrameEventX(device: u64, timeout_ms: u32, out_token: u64) -> i32 {
        shims::WaitFrameEventX(device, timeout_ms, out_token)
    }

    // ---- DirectStorage (dstorage_xs.dll) ----

    #[no_mangle]
    pub extern "system" fn DStorageGetFactory(riid: u64, out_factory: u64) -> i32 {
        shims::DStorageGetFactory(riid, out_factory)
    }

    #[no_mangle]
    pub extern "system" fn DStorageSetConfiguration(config: u64) -> i32 {
        shims::DStorageSetConfiguration(config)
    }

    // ---- APU / ACP Hardware Audio & XAudio2 ----

    #[no_mangle]
    pub extern "system" fn ApuAlloc(out_va: u64, out_pa: u64, size: u32, align: u32) -> i32 {
        shims::ApuAlloc(out_va, out_pa, size, align)
    }

    #[no_mangle]
    pub extern "system" fn ApuFree(va: u64) -> i32 {
        shims::ApuFree(va)
    }

    #[no_mangle]
    pub extern "system" fn AcpHalCreate(out_hal: u64) -> i32 {
        shims::AcpHalCreate(out_hal)
    }

    #[no_mangle]
    pub extern "system" fn AcpHalAllocateShapeContexts(ctx: u64) -> i32 {
        shims::AcpHalAllocateShapeContexts(ctx)
    }

    #[no_mangle]
    pub extern "system" fn AcpHalReleaseShapeContexts() -> i32 {
        shims::AcpHalReleaseShapeContexts()
    }

    #[no_mangle]
    pub extern "system" fn XAudio2Create(out_xa2: u64, flags: u32, processor: u32) -> i32 {
        #[link(name = "kernel32")]
        extern "system" {
            fn LoadLibraryA(name: *const u8) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
        }
        let xa = unsafe { LoadLibraryA(b"xaudio2_9.dll\0".as_ptr()) };
        if !xa.is_null() {
            let sym = unsafe { GetProcAddress(xa, b"XAudio2Create\0".as_ptr()) };
            if !sym.is_null() {
                let func: extern "system" fn(u64, u32, u32) -> i32 = unsafe { std::mem::transmute(sym) };
                return func(out_xa2, flags, processor);
            }
        }
        shims::E_FAIL
    }

    // ---- XSystem / XUser / XTaskQueue / XAsync / XGameSave / XStore / XNetworking ----

    #[no_mangle]
    pub extern "system" fn XSystemGetXboxLiveSandboxId(out: u64) -> i32 {
        shims::XSystemGetXboxLiveSandboxId(out)
    }

    #[no_mangle]
    pub extern "system" fn XGameRuntimeGetXboxLiveSandboxId(out: u64) -> i32 {
        shims::XGameRuntimeGetXboxLiveSandboxId(out)
    }

    #[no_mangle]
    pub extern "system" fn XSystemGetDeviceType() -> u32 {
        shims::XSystemGetDeviceType()
    }

    #[no_mangle]
    pub extern "system" fn XSystemGetConsoleId(cap: usize, buf: u64, used: u64) -> i32 {
        shims::XSystemGetConsoleId(cap, buf, used)
    }

    #[no_mangle]
    pub extern "system" fn XSystemGetAppSpecificDeviceId(cap: usize, buf: u64, used: u64) -> i32 {
        shims::XSystemGetAppSpecificDeviceId(cap, buf, used)
    }

    #[no_mangle]
    pub extern "system" fn XSystemIsHandleValid(handle: u64) -> bool {
        shims::XSystemIsHandleValid(handle)
    }

    #[no_mangle]
    pub extern "system" fn XGameGetXboxTitleId(out: u64) -> i32 {
        shims::XGameGetXboxTitleId(out)
    }

    #[no_mangle]
    pub extern "system" fn XPackageGetCurrentProcessPackageIdentifier(cap: usize, buf: u64) -> i32 {
        shims::XPackageGetCurrentProcessPackageIdentifier(cap, buf)
    }

    #[no_mangle]
    pub extern "system" fn XDisplayTryAcquireTimeoutStatus() -> u32 {
        shims::XDisplayTryAcquireTimeoutStatus()
    }

    #[no_mangle]
    pub extern "system" fn XDisplayAcquireTimeoutDeferral(out: u64) -> i32 {
        shims::XDisplayAcquireTimeoutDeferral(out)
    }

    #[no_mangle]
    pub extern "system" fn XDisplayCloseTimeoutDeferralHandle(handle: u64) {
        shims::XDisplayCloseTimeoutDeferralHandle(handle)
    }

    #[no_mangle]
    pub extern "system" fn XDisplayGetHdrMode() -> u32 {
        shims::XDisplayGetHdrMode()
    }

    #[no_mangle]
    pub extern "system" fn XThreadAssertNotTimeSensitive() {
        shims::XThreadAssertNotTimeSensitive()
    }

    #[no_mangle]
    pub extern "system" fn XThreadIsTimeSensitive() -> bool {
        shims::XThreadIsTimeSensitive()
    }

    #[no_mangle]
    pub extern "system" fn XThreadSetTimeSensitive(ts: bool) -> i32 {
        shims::XThreadSetTimeSensitive(ts)
    }

    #[no_mangle]
    pub extern "system" fn XUserGetGamertag(handle: u64, buffer: u64, cap: u32) -> i32 {
        shims::XUserGetGamertag(handle, buffer, cap)
    }

    #[no_mangle]
    pub extern "system" fn XUserGetId(user: u64, out: u64) -> i32 {
        shims::XUserGetId(user, out)
    }

    #[no_mangle]
    pub extern "system" fn XUserGetState(user: u64, out: u64) -> i32 {
        shims::XUserGetState(user, out)
    }

    #[no_mangle]
    pub extern "system" fn XUserGetAgeGroup(user: u64, out: u64) -> i32 {
        shims::XUserGetAgeGroup(user, out)
    }

    #[no_mangle]
    pub extern "system" fn XUserCheckPrivilege(user: u64, options: u32, privilege: u32, has: u64, reason: u64) -> i32 {
        shims::XUserCheckPrivilege(user, options, privilege, has, reason)
    }

    #[no_mangle]
    pub extern "system" fn XUserCloseHandle(user: u64) {
        shims::XUserCloseHandle(user)
    }

    #[no_mangle]
    pub extern "system" fn XUserAddAsync(options: u32, async_block: u64) -> i32 {
        shims::XUserAddAsync(options, async_block)
    }

    #[no_mangle]
    pub extern "system" fn XUserAddResult(async_block: u64, out_user: u64) -> i32 {
        shims::XUserAddResult(async_block, out_user)
    }

    #[no_mangle]
    pub extern "system" fn XUserGetLocale(cap: usize, buf: u64, used: u64) -> i32 {
        shims::XUserGetLocale(cap, buf, used)
    }

    #[no_mangle]
    pub extern "system" fn XUserRegisterForChangeEvent(queue: u64, ctx: u64, cb: u64, out_token: u64) -> i32 {
        shims::XUserRegisterForChangeEvent(queue, ctx, cb, out_token)
    }

    #[no_mangle]
    pub extern "system" fn XUserUnregisterForChangeEvent(token: u64, wait: bool) -> bool {
        shims::XUserUnregisterForChangeEvent(token, wait)
    }

    #[no_mangle]
    pub extern "system" fn XUserDuplicateHandle(handle: u64, out: u64) -> i32 {
        shims::XUserDuplicateHandle(handle, out)
    }

    #[no_mangle]
    pub extern "system" fn XUserGetLocalId(user: u64, out: u64) -> i32 {
        shims::XUserGetLocalId(user, out)
    }

    #[no_mangle]
    pub extern "system" fn XAsyncGetStatus(async_block: u64, wait: bool) -> i32 {
        shims::XAsyncGetStatus(async_block, wait)
    }

    #[no_mangle]
    pub extern "system" fn XAsyncGetResultSize(async_block: u64, out_size: u64) -> i32 {
        shims::XAsyncGetResultSize(async_block, out_size)
    }

    #[no_mangle]
    pub extern "system" fn XAsyncCancel(async_block: u64) {
        shims::XAsyncCancel(async_block)
    }

    #[no_mangle]
    pub extern "system" fn XTaskQueueCreate(work: u32, comp: u32, out: u64) -> i32 {
        shims::XTaskQueueCreate(work, comp, out)
    }

    #[no_mangle]
    pub extern "system" fn XTaskQueueCloseHandle(queue: u64) {
        shims::XTaskQueueCloseHandle(queue)
    }

    #[no_mangle]
    pub extern "system" fn XTaskQueueGetCurrentProcessTaskQueue(out: u64) -> bool {
        shims::XTaskQueueGetCurrentProcessTaskQueue(out)
    }

    #[no_mangle]
    pub extern "system" fn XTaskQueueSubmitCallback(queue: u64, port: u32, ctx: u64, cb: u64) -> i32 {
        shims::XTaskQueueSubmitCallback(queue, port, ctx, cb)
    }

    #[no_mangle]
    pub extern "system" fn XTaskQueueDispatch(queue: u64, port: u32, timeout_ms: u32) -> bool {
        shims::XTaskQueueDispatch(queue, port, timeout_ms)
    }

    #[no_mangle]
    pub extern "system" fn XStorageCreateHandle(user: u64, name: u64, options: u64, out: u64) -> i32 {
        shims::XStorageCreateHandle(user, name, options, out)
    }

    #[no_mangle]
    pub extern "system" fn XGameSaveInitializeProvider(user: u64, scid: u64, sync: bool, out: u64) -> i32 {
        shims::XGameSaveInitializeProvider(user, scid, sync, out)
    }

    #[no_mangle]
    pub extern "system" fn XGameSaveCloseProvider(provider: u64) {
        shims::XGameSaveCloseProvider(provider)
    }

    #[no_mangle]
    pub extern "system" fn XGameSaveGetRemainingQuota(provider: u64, out_quota: u64) -> i32 {
        shims::XGameSaveGetRemainingQuota(provider, out_quota)
    }

    #[no_mangle]
    pub extern "system" fn XGameSaveCreateContainer(provider: u64, name: u64, out: u64) -> i32 {
        shims::XGameSaveCreateContainer(provider, name, out)
    }

    #[no_mangle]
    pub extern "system" fn XGameSaveCloseContainer(container: u64) {
        shims::XGameSaveCloseContainer(container)
    }

    #[no_mangle]
    pub extern "system" fn XGameSaveCreateUpdate(container: u64, display: u64, out: u64) -> i32 {
        shims::XGameSaveCreateUpdate(container, display, out)
    }

    #[no_mangle]
    pub extern "system" fn XGameSaveSubmitBlobWrite(update: u64, name: u64, data: u64, size: usize) -> i32 {
        shims::XGameSaveSubmitBlobWrite(update, name, data, size)
    }

    #[no_mangle]
    pub extern "system" fn XGameSaveSubmitUpdate(update: u64) -> i32 {
        shims::XGameSaveSubmitUpdate(update)
    }

    #[no_mangle]
    pub extern "system" fn XGameSaveCloseUpdate(update: u64) {
        shims::XGameSaveCloseUpdate(update)
    }

    #[no_mangle]
    pub extern "system" fn XGameSaveReadBlobData(
        container: u64,
        name: u64,
        buf: u64,
        size: usize,
        written: u64,
    ) -> i32 {
        shims::XGameSaveReadBlobData(container, name, buf, size, written)
    }

    #[no_mangle]
    pub extern "system" fn XGameSaveSubmitBlobDelete(update: u64, name: u64) -> i32 {
        shims::XGameSaveSubmitBlobDelete(update, name)
    }

    #[no_mangle]
    pub extern "system" fn XGameSaveDeleteContainer(provider: u64, name: u64) -> i32 {
        shims::XGameSaveDeleteContainer(provider, name)
    }

    #[no_mangle]
    pub extern "system" fn XStoreCreateContext(user: u64, out: u64) -> i32 {
        shims::XStoreCreateContext(user, out)
    }

    #[no_mangle]
    pub extern "system" fn XStoreCloseContextHandle(ctx: u64) {
        shims::XStoreCloseContextHandle(ctx)
    }

    #[no_mangle]
    pub extern "system" fn XStoreIsAvailabilityPurchasable(availability: u64) -> bool {
        shims::XStoreIsAvailabilityPurchasable(availability)
    }

    #[no_mangle]
    pub extern "system" fn XStoreAcquireLicenseForPackageSync(ctx: u64, pkg: u64, out: u64) -> i32 {
        shims::XStoreAcquireLicenseForPackageSync(ctx, pkg, out)
    }

    #[no_mangle]
    pub extern "system" fn XGameUiShowMessageDialogAsync(
        async_block: u64,
        title: u64,
        content: u64,
        b1: u64,
        b2: u64,
        b3: u64,
        def_b: u32,
        cancel_b: u32,
    ) -> i32 {
        shims::XGameUiShowMessageDialogAsync(async_block, title, content, b1, b2, b3, def_b, cancel_b)
    }

    #[no_mangle]
    pub extern "system" fn XGameUiShowMessageDialogResult(async_block: u64, out_btn: u64) -> i32 {
        shims::XGameUiShowMessageDialogResult(async_block, out_btn)
    }

    #[no_mangle]
    pub extern "system" fn XLaunchNewGame(exe: u64, args: u64, user: u64) {
        shims::XLaunchNewGame(exe, args, user)
    }

    #[no_mangle]
    pub extern "system" fn XNetworkingGetConnectivityHint(out: u64) -> i32 {
        shims::XNetworkingGetConnectivityHint(out)
    }

    #[no_mangle]
    pub extern "system" fn XNetworkingRegisterConnectivityHintChanged(
        queue: u64,
        ctx: u64,
        cb: u64,
        out_token: u64,
    ) -> i32 {
        shims::XNetworkingRegisterConnectivityHintChanged(queue, ctx, cb, out_token)
    }

    #[no_mangle]
    pub extern "system" fn XNetworkingUnregisterConnectivityHintChanged(token: u64, wait: bool) -> bool {
        shims::XNetworkingUnregisterConnectivityHintChanged(token, wait)
    }

    #[no_mangle]
    pub extern "system" fn XcurlInitialize() -> i32 {
        shims::XcurlInitialize()
    }

    #[no_mangle]
    pub extern "system" fn XcurlUninitialize() -> i32 {
        shims::XcurlUninitialize()
    }

    #[no_mangle]
    pub extern "system" fn BinkOpen(name: u64, flags: u32) -> u64 {
        shims::BinkOpen(name, flags)
    }

    #[no_mangle]
    pub extern "system" fn BinkDoFrame(bink: u64) -> i32 {
        shims::BinkDoFrame(bink)
    }

    #[no_mangle]
    pub extern "system" fn BinkClose(bink: u64) {
        shims::BinkClose(bink)
    }
}

#[cfg(all(test, windows, target_arch = "x86_64"))]
mod tests {
    use super::exports::*;

    #[test]
    fn standalone_physical_memory_exports_share_backing_and_release_it() {
        let page_bytes = xboxone::era_mem::PAGE_SIZE_64K as usize;
        let window = XMemVirtualAlloc(0, page_bytes * 2, 0x42000, 0, 1);
        assert!(!window.is_null());
        let first = window as usize;
        let aliases = [first, first + page_bytes];
        let mut count = 1;
        let mut page = 0;
        assert_eq!(unsafe { XMemAllocatePhysicalPages(0, &mut count, &mut page) }, 1);
        assert_ne!(page, 0);

        assert_eq!(unsafe { XMemMapPhysicalPages(first, 1, &page) }, first);
        assert_eq!(unsafe { XMemMapPhysicalPagesScatter(&aliases[1], 1, &page) }, 1);
        unsafe { (first as *mut u64).write(0x1234_5678) };
        assert_eq!(unsafe { (aliases[1] as *const u64).read() }, 0x1234_5678);
        assert_eq!(unsafe { XMemFreePhysicalPages(1, &page) }, 0);

        assert_eq!(unsafe { XMemMapPhysicalPages(first, 2, std::ptr::null()) }, first);
        assert_eq!(unsafe { XMemFreePhysicalPages(1, &page) }, 1);
        for address in aliases {
            assert_ne!(unsafe { xboxone::native_pages::guest_virtual_free(address as *mut _, 0, 0x8000) }, 0);
        }
    }

    #[test]
    fn x2w_exports_initialize_directstorage_d3d12x_and_acp() {
        assert_eq!(XGameRuntimeInitialize(0, 0), 0);
        assert_eq!(XSystemGetDeviceType(), 7);

        let mut fac = 0u64;
        assert_eq!(DStorageGetFactory(0, &mut fac as *mut u64 as u64), 0);
        assert_ne!(fac, 0);

        let mut dev = 0u64;
        let mut cfg = [0u8; 32];
        cfg[4..8].copy_from_slice(&4u32.to_le_bytes());
        assert_eq!(D3D12ConfigureVirtualMemoryX(cfg.as_ptr() as u64), 0);
        assert_eq!(D3D12ConfigureVirtualMemoryX(0), 0x8007_0057u32 as i32);
        assert_eq!(D3D12XBOX_CreateDevice(0, 0, &mut dev as *mut u64 as u64), 0);
        assert_ne!(dev, 0);
        let mut dev2 = 0u64;
        assert_eq!(D3D12XboxCreateDevice(0, 0, &mut dev2 as *mut u64 as u64), 0);
        assert_ne!(dev2, 0);

        let mut hal = 0u64;
        assert_eq!(AcpHalCreate(&mut hal as *mut u64 as u64), 0);
        assert_ne!(hal, 0);

        assert_eq!(XGameRuntimeUninitialize(), 0);
    }

    #[test]
    fn converts_and_executes_clean_room_xbox_pe_natively_with_x2w_dll() {
        // Locate freshly built x2w.dll next to the test runner or in target/{debug,release}
        let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let ws_root = manifest_dir.parent().unwrap().parent().unwrap();
        let dll_src = [
            ws_root.join("target").join("debug").join("x2w.dll"),
            ws_root.join("target").join("release").join("x2w.dll"),
        ]
        .into_iter()
        .find(|p| p.is_file());
        let Some(dll_src) = dll_src else {
            return; // skip if cdylib not yet built in this profile
        };

        // Synthesize a valid 64-bit Xbox Series X|S PE32+ executable (Subsystem = 14)
        // importing xgameruntime.dll, dstorage_xs.dll, d3d12_xs.dll, acphal.dll, kernel32.dll
        let mut pe = vec![0u8; 0xC00];
        let w16 = |b: &mut [u8], off: usize, v: u16| b[off..off + 2].copy_from_slice(&v.to_le_bytes());
        let w32 = |b: &mut [u8], off: usize, v: u32| b[off..off + 4].copy_from_slice(&v.to_le_bytes());
        let w64 = |b: &mut [u8], off: usize, v: u64| b[off..off + 8].copy_from_slice(&v.to_le_bytes());

        pe[0..2].copy_from_slice(b"MZ");
        w32(&mut pe, 0x3C, 0x80);
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        w16(&mut pe, 0x84, 0x8664); // AMD64
        w16(&mut pe, 0x86, 2);      // 2 sections (.text, .rdata)
        w16(&mut pe, 0x94, 0xF0);   // SizeOfOptionalHeader
        w16(&mut pe, 0x96, 0x0022); // EXECUTABLE_IMAGE | LARGE_ADDRESS_AWARE

        let opt = 0x98usize;
        w16(&mut pe, opt, 0x20B);                 // PE32+
        w32(&mut pe, opt + 16, 0x1000);           // AddressOfEntryPoint
        w64(&mut pe, opt + 24, 0x0001_4000_0000); // ImageBase
        w32(&mut pe, opt + 32, 0x1000);           // SectionAlignment
        w32(&mut pe, opt + 36, 0x200);            // FileAlignment
        w16(&mut pe, opt + 40, 6);                // MajorOSVersion
        w16(&mut pe, opt + 48, 6);                // MajorSubsystemVersion
        w32(&mut pe, opt + 56, 0x3000);           // SizeOfImage
        w32(&mut pe, opt + 60, 0x200);            // SizeOfHeaders
        w16(&mut pe, opt + 68, 14);               // IMAGE_SUBSYSTEM_XBOX
        w16(&mut pe, opt + 70, 0x8160);           // DYNAMIC_BASE | HIGH_ENTROPY_VA | NX_COMPAT | TERMINAL_SERVER_AWARE
        w64(&mut pe, opt + 72, 0x100000);         // SizeOfStackReserve
        w64(&mut pe, opt + 80, 0x1000);           // SizeOfStackCommit
        w64(&mut pe, opt + 88, 0x100000);         // SizeOfHeapReserve
        w64(&mut pe, opt + 96, 0x1000);           // SizeOfHeapCommit
        w32(&mut pe, opt + 108, 16);              // NumberOfRvaAndSizes
        // DataDirectory[1] = Import Directory at RVA 0x2000
        w32(&mut pe, opt + 120, 0x2000);
        w32(&mut pe, opt + 124, 120);

        // Section 1: .text (RVA 0x1000, file 0x200, size 0x200)
        let s1 = opt + 0xF0;
        pe[s1..s1 + 5].copy_from_slice(b".text");
        w32(&mut pe, s1 + 8, 0x200);
        w32(&mut pe, s1 + 12, 0x1000);
        w32(&mut pe, s1 + 16, 0x200);
        w32(&mut pe, s1 + 20, 0x200);
        w32(&mut pe, s1 + 36, 0x6000_0020);

        // Section 2: .rdata (RVA 0x2000, file 0x400, size 0x800, RW for IAT fixups)
        let s2 = s1 + 40;
        pe[s2..s2 + 6].copy_from_slice(b".rdata");
        w32(&mut pe, s2 + 8, 0x800);
        w32(&mut pe, s2 + 12, 0x2000);
        w32(&mut pe, s2 + 16, 0x800);
        w32(&mut pe, s2 + 20, 0x400);
        w32(&mut pe, s2 + 36, 0xC000_0040);

        // Helper mapping .rdata RVA (0x2000..) -> file offset (0x400..)
        let rdata_off = |rva: u32| -> usize { 0x400 + (rva - 0x2000) as usize };

        // Write DLL names & Hint/Name entries in .rdata
        let mut str_rva = 0x2300u32;
        let mut put_cstr = |b: &mut [u8], s: &str| -> u32 {
            let rva = str_rva;
            let off = rdata_off(rva);
            b[off..off + s.len()].copy_from_slice(s.as_bytes());
            b[off + s.len()] = 0;
            str_rva = (str_rva + s.len() as u32 + 2) & !1;
            rva
        };
        let mut hn_rva = 0x2400u32;
        let mut put_hn = |b: &mut [u8], name: &str| -> u64 {
            let rva = hn_rva;
            let off = rdata_off(rva);
            b[off] = 0;
            b[off + 1] = 0;
            b[off + 2..off + 2 + name.len()].copy_from_slice(name.as_bytes());
            b[off + 2 + name.len()] = 0;
            hn_rva = (hn_rva + 2 + name.len() as u32 + 2) & !1;
            rva as u64
        };

        let dll_xgr = put_cstr(&mut pe, "xgameruntime.dll");
        let dll_ds = put_cstr(&mut pe, "dstorage_xs.dll");
        let dll_d3d = put_cstr(&mut pe, "d3d12_xs.dll");
        let dll_acp = put_cstr(&mut pe, "acphal.dll");
        let dll_k32 = put_cstr(&mut pe, "kernel32.dll");

        let hn_init = put_hn(&mut pe, "XGameRuntimeInitialize");
        let hn_devtype = put_hn(&mut pe, "XSystemGetDeviceType");
        let hn_ds = put_hn(&mut pe, "DStorageGetFactory");
        let hn_d3d = put_hn(&mut pe, "D3D12XBOX_CreateDevice");
        let hn_acp = put_hn(&mut pe, "AcpHalCreate");
        let hn_exit = put_hn(&mut pe, "ExitProcess");

        // Write 5 Import Descriptors at RVA 0x2000 (each 20 bytes)
        let descs: [(u32, u32, u32, &[u64]); 5] = [
            (0x2100, 0x2200, dll_xgr, &[hn_init, hn_devtype]),
            (0x2118, 0x2218, dll_ds, &[hn_ds]),
            (0x2128, 0x2228, dll_d3d, &[hn_d3d]),
            (0x2138, 0x2238, dll_acp, &[hn_acp]),
            (0x2148, 0x2248, dll_k32, &[hn_exit]),
        ];
        for (i, (ilt, iat, name_rva, thunks)) in descs.iter().enumerate() {
            let d = rdata_off(0x2000 + (i as u32) * 20);
            w32(&mut pe, d, *ilt);
            w32(&mut pe, d + 12, *name_rva);
            w32(&mut pe, d + 16, *iat);
            for (j, &t) in thunks.iter().enumerate() {
                w64(&mut pe, rdata_off(*ilt + (j as u32) * 8), t);
                w64(&mut pe, rdata_off(*iat + (j as u32) * 8), t);
            }
        }

        // Emit x86-64 entry point at RVA 0x1000 (file offset 0x200)
        let mut code: Vec<u8> = Vec::new();
        let emit_call_iat = |c: &mut Vec<u8>, iat_rva: u32| {
            c.extend_from_slice(&[0xFF, 0x15]); // call [rip + disp32]
            let next_rip = 0x1000u32 + c.len() as u32 + 4;
            let disp = (iat_rva as i32) - (next_rip as i32);
            c.extend_from_slice(&disp.to_le_bytes());
        };
        code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x38]); // sub rsp, 0x38
        // XGameRuntimeInitialize(0, 0)
        code.extend_from_slice(&[0x31, 0xC9, 0x31, 0xD2]);
        emit_call_iat(&mut code, 0x2200);
        // DStorageGetFactory(0, &local)
        code.extend_from_slice(&[0x31, 0xC9, 0x48, 0x8D, 0x54, 0x24, 0x20]);
        emit_call_iat(&mut code, 0x2218);
        // D3D12XBOX_CreateDevice(0, 0, &local)
        code.extend_from_slice(&[0x31, 0xC9, 0x31, 0xD2, 0x4C, 0x8D, 0x44, 0x24, 0x20]);
        emit_call_iat(&mut code, 0x2228);
        // AcpHalCreate(&local)
        code.extend_from_slice(&[0x48, 0x8D, 0x4C, 0x24, 0x20]);
        emit_call_iat(&mut code, 0x2238);
        // eax = XSystemGetDeviceType() (7); ecx = eax * 6 (42); ExitProcess(42)
        emit_call_iat(&mut code, 0x2208);
        code.extend_from_slice(&[0x6B, 0xC8, 0x06]); // imul ecx, eax, 6
        emit_call_iat(&mut code, 0x2248);
        pe[0x200..0x200 + code.len()].copy_from_slice(&code);

        // Convert Xbox PE -> Native Windows .exe
        let converted = xboxone::exe_convert::convert_xbox_pe_to_windows(&pe).unwrap();
        assert_eq!(converted.rewritten_dlls.len(), 4);

        let dir = std::env::temp_dir().join(format!("x2w_native_e2e_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe_out = dir.join("xbox_series_x_test_windows.exe");
        let dll_out = dir.join("x2w.dll");
        std::fs::write(&exe_out, &converted.windows_pe).unwrap();
        std::fs::copy(&dll_src, &dll_out).unwrap();

        let status = std::process::Command::new(&exe_out).status().expect("spawn converted native .exe");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(status.code(), Some(42));
    }
}
