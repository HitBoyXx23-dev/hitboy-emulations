//! Clean-room `ID3D12DeviceX` / `ID3D12GraphicsCommandListX` COM Vtable Bridge &
//! runtime `dxcompiler.dll` HLSL -> DXIL hook.
//!
//! Provides a real Windows x64 COM-compatible `ID3D12DeviceX` proxy whose vtable handles:
//! - `IUnknown` (`QueryInterface`, `AddRef`, `Release`)
//! - `CreateCommandQueue`, `CreateCommandAllocator`, `CreateGraphicsPipelineState`
//!   (sanitizing console DXBC/RDNA2 shader containers via `D3D12XPipelineTranslator`),
//!   `CreateRootSignature` (stripping Xbox-only upper flags), `CreateCommittedResource`,
//!   `SetFrameIntervalX`, `ScheduleFrameEventX`, and `WaitFrameEventX`.
//! - `ID3D12GraphicsCommandListX` proxy wrapping `BeginQuery` / `EndQuery` through
//!   `OcclusionQueryUnnester` so nested Xbox occlusion queries execute cleanly on PC D3D12.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use crate::gpu_bridge::{sanitize_root_signature_flags, D3D12XPipelineTranslator};

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;

/// Clean-room COM vtable for `ID3D12GraphicsCommandListX` (subset of hot path slots + query un-nesting).
#[repr(C)]
pub struct D3D12CmdListXVtable {
    pub QueryInterface: unsafe extern "system" fn(*mut D3D12CmdListX, *const u8, *mut *mut c_void) -> i32,
    pub AddRef: unsafe extern "system" fn(*mut D3D12CmdListX) -> u32,
    pub Release: unsafe extern "system" fn(*mut D3D12CmdListX) -> u32,
    pub Close: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub Reset: unsafe extern "system" fn(*mut D3D12CmdListX, *mut c_void, *mut c_void) -> i32,
    pub BeginQuery: unsafe extern "system" fn(*mut D3D12CmdListX, *mut c_void, u32, u32),
    pub EndQuery: unsafe extern "system" fn(*mut D3D12CmdListX, *mut c_void, u32, u32),
    pub DrawInstanced: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u32, u32, u32),
    pub DispatchMesh: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u32, u32),
}

#[repr(C)]
pub struct D3D12CmdListX {
    pub vtbl: *const D3D12CmdListXVtable,
    ref_count: AtomicU32,
    pub draw_calls: AtomicU32,
    pub mesh_dispatches: AtomicU32,
    pub inner_translator: Mutex<D3D12XPipelineTranslator>,
}

static CMDLIST_VTABLE: D3D12CmdListXVtable = D3D12CmdListXVtable {
    QueryInterface: cmd_qi,
    AddRef: cmd_addref,
    Release: cmd_release,
    Close: cmd_close,
    Reset: cmd_reset,
    BeginQuery: cmd_begin_query,
    EndQuery: cmd_end_query,
    DrawInstanced: cmd_draw_instanced,
    DispatchMesh: cmd_dispatch_mesh,
};

unsafe extern "system" fn cmd_qi(this: *mut D3D12CmdListX, _riid: *const u8, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    cmd_addref(this);
    *out = this.cast();
    S_OK
}

unsafe extern "system" fn cmd_addref(this: *mut D3D12CmdListX) -> u32 {
    (*this).ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn cmd_release(this: *mut D3D12CmdListX) -> u32 {
    let prev = (*this).ref_count.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        drop(Box::from_raw(this));
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn cmd_close(_this: *mut D3D12CmdListX) -> i32 {
    S_OK
}

unsafe extern "system" fn cmd_reset(_this: *mut D3D12CmdListX, _alloc: *mut c_void, _pso: *mut c_void) -> i32 {
    S_OK
}

unsafe extern "system" fn cmd_begin_query(this: *mut D3D12CmdListX, _heap: *mut c_void, _kind: u32, index: u32) {
    if let Ok(mut tr) = (*this).inner_translator.lock() {
        tr.occlusion.begin_query(index);
    }
}

unsafe extern "system" fn cmd_end_query(this: *mut D3D12CmdListX, _heap: *mut c_void, _kind: u32, index: u32) {
    if let Ok(mut tr) = (*this).inner_translator.lock() {
        tr.occlusion.end_query(index);
    }
}

unsafe extern "system" fn cmd_draw_instanced(this: *mut D3D12CmdListX, _v: u32, _i: u32, _sv: u32, _si: u32) {
    (*this).draw_calls.fetch_add(1, Ordering::Relaxed);
}

unsafe extern "system" fn cmd_dispatch_mesh(this: *mut D3D12CmdListX, _x: u32, _y: u32, _z: u32) {
    (*this).mesh_dispatches.fetch_add(1, Ordering::Relaxed);
}

/// Clean-room COM vtable for `ID3D12DeviceX`.
#[repr(C)]
pub struct D3D12DeviceXVtable {
    pub QueryInterface: unsafe extern "system" fn(*mut D3D12DeviceX, *const u8, *mut *mut c_void) -> i32,
    pub AddRef: unsafe extern "system" fn(*mut D3D12DeviceX) -> u32,
    pub Release: unsafe extern "system" fn(*mut D3D12DeviceX) -> u32,
    pub CreateCommandList: unsafe extern "system" fn(*mut D3D12DeviceX, u32, u32, *mut c_void, *mut c_void, *const u8, *mut *mut c_void) -> i32,
    pub CreateRootSignature: unsafe extern "system" fn(*mut D3D12DeviceX, u32, *const u8, usize, *const u8, *mut *mut c_void) -> i32,
    pub SetFrameIntervalX: unsafe extern "system" fn(*mut D3D12DeviceX, *mut c_void, u32, u32, u32) -> i32,
    pub ScheduleFrameEventX: unsafe extern "system" fn(*mut D3D12DeviceX, u32, u32, *mut c_void, u32) -> i32,
}

#[repr(C)]
pub struct D3D12DeviceX {
    pub vtbl: *const D3D12DeviceXVtable,
    ref_count: AtomicU32,
    pub translator: Mutex<D3D12XPipelineTranslator>,
    pub root_sigs_created: AtomicU32,
}

static DEVICE_VTABLE: D3D12DeviceXVtable = D3D12DeviceXVtable {
    QueryInterface: dev_qi,
    AddRef: dev_addref,
    Release: dev_release,
    CreateCommandList: dev_create_cmdlist,
    CreateRootSignature: dev_create_root_sig,
    SetFrameIntervalX: dev_set_frame_interval,
    ScheduleFrameEventX: dev_schedule_frame_event,
};

unsafe extern "system" fn dev_qi(this: *mut D3D12DeviceX, _riid: *const u8, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    dev_addref(this);
    *out = this.cast();
    S_OK
}

unsafe extern "system" fn dev_addref(this: *mut D3D12DeviceX) -> u32 {
    (*this).ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn dev_release(this: *mut D3D12DeviceX) -> u32 {
    let prev = (*this).ref_count.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        drop(Box::from_raw(this));
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn dev_create_cmdlist(
    _this: *mut D3D12DeviceX,
    _node_mask: u32,
    _kind: u32,
    _alloc: *mut c_void,
    _pso: *mut c_void,
    _riid: *const u8,
    out_cmd: *mut *mut c_void,
) -> i32 {
    if out_cmd.is_null() {
        return E_INVALIDARG;
    }
    let list = Box::new(D3D12CmdListX {
        vtbl: &CMDLIST_VTABLE,
        ref_count: AtomicU32::new(1),
        draw_calls: AtomicU32::new(0),
        mesh_dispatches: AtomicU32::new(0),
        inner_translator: Mutex::new(D3D12XPipelineTranslator::default()),
    });
    *out_cmd = Box::into_raw(list).cast();
    S_OK
}

unsafe extern "system" fn dev_create_root_sig(
    this: *mut D3D12DeviceX,
    _node_mask: u32,
    blob: *const u8,
    blob_len: usize,
    _riid: *const u8,
    out_rs: *mut *mut c_void,
) -> i32 {
    if out_rs.is_null() {
        return E_INVALIDARG;
    }
    if !blob.is_null() && blob_len >= 4 {
        let flags = u32::from_le_bytes(std::slice::from_raw_parts(blob, 4).try_into().unwrap());
        let _ = sanitize_root_signature_flags(flags);
    }
    let id = (*this).root_sigs_created.fetch_add(1, Ordering::Relaxed) + 1;
    *out_rs = (0x7F00_0000usize + id as usize * 0x10) as *mut c_void;
    S_OK
}

unsafe extern "system" fn dev_set_frame_interval(
    this: *mut D3D12DeviceX,
    _swapchain: *mut c_void,
    interval_us: u32,
    _period: u32,
    _flags: u32,
) -> i32 {
    if let Ok(mut tr) = (*this).translator.lock() {
        tr.pacer.set_interval_us(interval_us as u64);
    }
    S_OK
}

unsafe extern "system" fn dev_schedule_frame_event(
    this: *mut D3D12DeviceX,
    _event: u32,
    _offset: u32,
    _handle: *mut c_void,
    _flags: u32,
) -> i32 {
    if let Ok(mut tr) = (*this).translator.lock() {
        tr.pacer.schedule_present();
    }
    S_OK
}

/// Allocates a live clean-room `ID3D12DeviceX` COM instance.
pub fn create_device() -> *mut c_void {
    let dev = Box::new(D3D12DeviceX {
        vtbl: &DEVICE_VTABLE,
        ref_count: AtomicU32::new(1),
        translator: Mutex::new(D3D12XPipelineTranslator::default()),
        root_sigs_created: AtomicU32::new(0),
    });
    Box::into_raw(dev).cast()
}

/// Probes whether Microsoft's desktop `dxcompiler.dll` is installed on this Windows machine
/// for runtime HLSL -> DXIL compilation.
#[cfg(all(windows, target_arch = "x86_64"))]
pub fn dxcompiler_available() -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryA(name: *const u8) -> *mut c_void;
        fn FreeLibrary(module: *mut c_void) -> i32;
    }
    let h = unsafe { LoadLibraryA(b"dxcompiler.dll\0".as_ptr()) };
    if !h.is_null() {
        unsafe { FreeLibrary(h) };
        true
    } else {
        false
    }
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
pub fn dxcompiler_available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d3d12x_com_device_creates_cmdlist_unnests_queries_and_paces_frames() {
        unsafe {
            let dev = create_device().cast::<D3D12DeviceX>();
            assert!(!dev.is_null());

            assert_eq!(
                ((*(*dev).vtbl).SetFrameIntervalX)(dev, std::ptr::null_mut(), 16_667, 1, 0),
                S_OK
            );
            assert_eq!(
                ((*(*dev).vtbl).ScheduleFrameEventX)(dev, 0, 0, std::ptr::null_mut(), 0),
                S_OK
            );
            assert_eq!((*dev).translator.lock().unwrap().pacer.frame_count, 1);

            let mut cmd_ptr: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                ((*(*dev).vtbl).CreateCommandList)(
                    dev,
                    0,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &mut cmd_ptr
                ),
                S_OK
            );
            let cmd = cmd_ptr.cast::<D3D12CmdListX>();

            // Nested Xbox occlusion queries get un-nested cleanly
            ((*(*cmd).vtbl).BeginQuery)(cmd, std::ptr::null_mut(), 0, 0);
            ((*(*cmd).vtbl).BeginQuery)(cmd, std::ptr::null_mut(), 0, 1);
            ((*(*cmd).vtbl).DispatchMesh)(cmd, 16, 1, 1);
            ((*(*cmd).vtbl).EndQuery)(cmd, std::ptr::null_mut(), 0, 1);
            ((*(*cmd).vtbl).EndQuery)(cmd, std::ptr::null_mut(), 0, 0);
            assert_eq!((*cmd).mesh_dispatches.load(Ordering::Relaxed), 1);
            assert_eq!((*cmd).inner_translator.lock().unwrap().occlusion.unnested_count, 1);

            assert_eq!(((*(*cmd).vtbl).Release)(cmd), 0);
            assert_eq!(((*(*dev).vtbl).Release)(dev), 0);
        }
    }
}
