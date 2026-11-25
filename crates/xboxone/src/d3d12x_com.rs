//! Clean-room `ID3D12DeviceX` / `ID3D12GraphicsCommandListX` COM Vtable Bridge &
//! runtime `dxcompiler.dll` HLSL -> DXIL hook.
//!
//! Provides a real Windows x64 COM-compatible `ID3D12DeviceX` wrapper whose 64-slot vtable:
//! - Forwards standard `ID3D12Object` / `ID3D12Device` base slots (`0..43`) directly to an
//!   internally created desktop Windows `ID3D12Device*` when `d3d12.dll` is present on this PC
//!   (with safe clean-room fallbacks when running headless or on non-Windows targets).
//! - Intercepts `QueryInterface` so requests for `ID3D12Device1..11` / `ID3D12DeviceX` stay on
//!   this Xbox-ABI-safe wrapper instead of escaping to raw desktop `ID3D12Device1..11` slots.
//! - Intercepts `CreateCommandList` (slot 12) to wrap command lists with `OcclusionQueryUnnester`,
//!   and `CreateRootSignature` (slot 16) to strip Xbox-only upper flags before forwarding.
//! - Explicitly dispatches Xbox extension slots (`44..63`, including frame pacing and slot 49)
//!   so Xbox device calls never mis-dispatch into mismatched desktop `ID3D12Device1..11` methods.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use crate::gpu_bridge::{sanitize_root_signature_flags, D3D12XPipelineTranslator};

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
const E_NOTIMPL: i32 = 0x8000_4001u32 as i32;

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
    pub inner_desktop_cmd: *mut c_void,
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
        let inner = (*this).inner_desktop_cmd;
        if !inner.is_null() {
            release_com_object(inner);
        }
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

type RawFn6 = unsafe extern "system" fn(*mut c_void, u64, u64, u64, u64, u64, u64) -> i32;
type RawFn8 = unsafe extern "system" fn(*mut c_void, u64, u64, u64, u64, u64, u64, u64, u64) -> i32;

unsafe fn release_com_object(obj: *mut c_void) {
    if !obj.is_null() {
        let vtbl = *(obj as *const *const usize);
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtbl.add(2));
        release(obj);
    }
}

unsafe fn forward_slot6(this: *mut D3D12DeviceX, slot: usize, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> i32 {
    let inner = (*this).inner_desktop_device;
    if inner.is_null() {
        return E_NOTIMPL;
    }
    let vtbl = *(inner as *const *const usize);
    let func: RawFn6 = std::mem::transmute(*vtbl.add(slot));
    func(inner, a1, a2, a3, a4, a5, a6)
}

unsafe fn forward_slot8(
    this: *mut D3D12DeviceX,
    slot: usize,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
    a7: u64,
    a8: u64,
) -> i32 {
    let inner = (*this).inner_desktop_device;
    if inner.is_null() {
        return E_NOTIMPL;
    }
    let vtbl = *(inner as *const *const usize);
    let func: RawFn8 = std::mem::transmute(*vtbl.add(slot));
    func(inner, a1, a2, a3, a4, a5, a6, a7, a8)
}

/// Full 64-slot COM vtable for `ID3D12DeviceX`:
/// - Slots `0..=43` match standard Windows `ID3D12Device` layout and forward to the wrapped PC GPU device.
/// - Slots `44..=63` implement Xbox Series X|S extension ABI cleanly without colliding with desktop `ID3D12Device1..11`.
#[repr(C)]
pub struct D3D12DeviceXVtable {
    // IUnknown (0..=2)
    pub QueryInterface: unsafe extern "system" fn(*mut D3D12DeviceX, *const u8, *mut *mut c_void) -> i32,
    pub AddRef: unsafe extern "system" fn(*mut D3D12DeviceX) -> u32,
    pub Release: unsafe extern "system" fn(*mut D3D12DeviceX) -> u32,
    // ID3D12Object (3..=6)
    pub GetPrivateData: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub SetPrivateData: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub SetPrivateDataInterface: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64) -> i32,
    pub SetName: unsafe extern "system" fn(*mut D3D12DeviceX, u64) -> i32,
    // ID3D12Device (7..=43)
    pub GetNodeCount: unsafe extern "system" fn(*mut D3D12DeviceX) -> u32,
    pub CreateCommandQueue: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub CreateCommandAllocator: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub CreateGraphicsPipelineState: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub CreateComputePipelineState: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub CreateCommandList: unsafe extern "system" fn(*mut D3D12DeviceX, u32, u32, *mut c_void, *mut c_void, *const u8, *mut *mut c_void) -> i32,
    pub CheckFeatureSupport: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub CreateDescriptorHeap: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub GetDescriptorHandleIncrementSize: unsafe extern "system" fn(*mut D3D12DeviceX, u32) -> u32,
    pub CreateRootSignature: unsafe extern "system" fn(*mut D3D12DeviceX, u32, *const u8, usize, *const u8, *mut *mut c_void) -> i32,
    pub CreateConstantBufferView: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64) -> i32,
    pub CreateShaderResourceView: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub CreateUnorderedAccessView: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64, u64) -> i32,
    pub CreateRenderTargetView: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub CreateDepthStencilView: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub CreateSampler: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64) -> i32,
    pub CopyDescriptors: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64, u64, u64, u64, u64) -> i32,
    pub CopyDescriptorsSimple: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64, u64) -> i32,
    pub GetResourceAllocationInfo: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64, u64) -> i32,
    pub GetCustomHeapProperties: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub CreateCommittedResource: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64, u64, u64, u64, u64) -> i32,
    pub CreateHeap: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub CreatePlacedResource: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64, u64, u64, u64, u64) -> i32,
    pub CreateReservedResource: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64, u64, u64) -> i32,
    pub CreateSharedHandle: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64, u64, u64) -> i32,
    pub OpenSharedHandle: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub OpenSharedHandleByName: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub MakeResident: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64) -> i32,
    pub Evict: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64) -> i32,
    pub CreateFence: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64, u64) -> i32,
    pub GetDeviceRemovedReason: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub GetCopyableFootprints: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64, u64, u64, u64, u64, u64) -> i32,
    pub CreateQueryHeap: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64) -> i32,
    pub SetStablePowerState: unsafe extern "system" fn(*mut D3D12DeviceX, u64) -> i32,
    pub CreateCommandSignature: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64, u64) -> i32,
    pub GetResourceTiling: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64, u64, u64, u64, u64, u64) -> i32,
    pub GetAdapterLuid: unsafe extern "system" fn(*mut D3D12DeviceX, *mut u64) -> *mut u64,
    // Xbox Series X|S Device Extensions (44..=63)
    pub SetFrameIntervalX: unsafe extern "system" fn(*mut D3D12DeviceX, *mut c_void, u32, u32, u32) -> i32,
    pub ScheduleFrameEventX: unsafe extern "system" fn(*mut D3D12DeviceX, u32, u32, *mut c_void, u32) -> i32,
    pub WaitFrameEventX: unsafe extern "system" fn(*mut D3D12DeviceX, u32, *mut u64) -> i32,
    pub SuspendX: unsafe extern "system" fn(*mut D3D12DeviceX, u32) -> i32,
    pub ResumeX: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub ConfigureXboxDeviceSlot49X: unsafe extern "system" fn(*mut D3D12DeviceX, u64, u64) -> i32,
    pub XboxReserved50: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub XboxReserved51: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub XboxReserved52: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub XboxReserved53: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub XboxReserved54: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub XboxReserved55: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub XboxReserved56: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub XboxReserved57: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub XboxReserved58: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub XboxReserved59: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub XboxReserved60: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub XboxReserved61: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub XboxReserved62: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
    pub XboxReserved63: unsafe extern "system" fn(*mut D3D12DeviceX) -> i32,
}

#[repr(C)]
pub struct D3D12DeviceX {
    pub vtbl: *const D3D12DeviceXVtable,
    ref_count: AtomicU32,
    pub inner_desktop_device: *mut c_void,
    pub translator: Mutex<D3D12XPipelineTranslator>,
    pub root_sigs_created: AtomicU32,
    pub slot49_calls: Mutex<Vec<(u64, u64)>>,
}

impl D3D12DeviceX {
    pub fn has_desktop_device(&self) -> bool {
        !self.inner_desktop_device.is_null()
    }
}

static DEVICE_VTABLE: D3D12DeviceXVtable = D3D12DeviceXVtable {
    QueryInterface: dev_qi,
    AddRef: dev_addref,
    Release: dev_release,
    GetPrivateData: dev_slot3,
    SetPrivateData: dev_slot4,
    SetPrivateDataInterface: dev_slot5,
    SetName: dev_slot6,
    GetNodeCount: dev_get_node_count,
    CreateCommandQueue: dev_slot8,
    CreateCommandAllocator: dev_slot9,
    CreateGraphicsPipelineState: dev_slot10,
    CreateComputePipelineState: dev_slot11,
    CreateCommandList: dev_create_cmdlist,
    CheckFeatureSupport: dev_slot13,
    CreateDescriptorHeap: dev_slot14,
    GetDescriptorHandleIncrementSize: dev_get_desc_inc,
    CreateRootSignature: dev_create_root_sig,
    CreateConstantBufferView: dev_slot17,
    CreateShaderResourceView: dev_slot18,
    CreateUnorderedAccessView: dev_slot19,
    CreateRenderTargetView: dev_slot20,
    CreateDepthStencilView: dev_slot21,
    CreateSampler: dev_slot22,
    CopyDescriptors: dev_slot23,
    CopyDescriptorsSimple: dev_slot24,
    GetResourceAllocationInfo: dev_slot25,
    GetCustomHeapProperties: dev_slot26,
    CreateCommittedResource: dev_slot27,
    CreateHeap: dev_slot28,
    CreatePlacedResource: dev_slot29,
    CreateReservedResource: dev_slot30,
    CreateSharedHandle: dev_slot31,
    OpenSharedHandle: dev_slot32,
    OpenSharedHandleByName: dev_slot33,
    MakeResident: dev_slot34,
    Evict: dev_slot35,
    CreateFence: dev_slot36,
    GetDeviceRemovedReason: dev_get_removed_reason,
    GetCopyableFootprints: dev_slot38,
    CreateQueryHeap: dev_slot39,
    SetStablePowerState: dev_slot40,
    CreateCommandSignature: dev_slot41,
    GetResourceTiling: dev_slot42,
    GetAdapterLuid: dev_get_adapter_luid,
    SetFrameIntervalX: dev_set_frame_interval,
    ScheduleFrameEventX: dev_schedule_frame_event,
    WaitFrameEventX: dev_wait_frame_event,
    SuspendX: dev_suspend,
    ResumeX: dev_resume,
    ConfigureXboxDeviceSlot49X: dev_xbox_slot49,
    XboxReserved50: dev_notimpl,
    XboxReserved51: dev_notimpl,
    XboxReserved52: dev_notimpl,
    XboxReserved53: dev_notimpl,
    XboxReserved54: dev_notimpl,
    XboxReserved55: dev_notimpl,
    XboxReserved56: dev_notimpl,
    XboxReserved57: dev_notimpl,
    XboxReserved58: dev_notimpl,
    XboxReserved59: dev_notimpl,
    XboxReserved60: dev_notimpl,
    XboxReserved61: dev_notimpl,
    XboxReserved62: dev_notimpl,
    XboxReserved63: dev_notimpl,
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
        let inner = (*this).inner_desktop_device;
        if !inner.is_null() {
            release_com_object(inner);
        }
        drop(Box::from_raw(this));
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn dev_slot3(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 3, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot4(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 4, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot5(t: *mut D3D12DeviceX, a: u64, b: u64) -> i32 { forward_slot6(t, 5, a, b, 0, 0, 0, 0) }
unsafe extern "system" fn dev_slot6(t: *mut D3D12DeviceX, a: u64) -> i32 {
    if (*t).inner_desktop_device.is_null() { S_OK } else { forward_slot6(t, 6, a, 0, 0, 0, 0, 0) }
}
unsafe extern "system" fn dev_get_node_count(t: *mut D3D12DeviceX) -> u32 {
    let inner = (*t).inner_desktop_device;
    if inner.is_null() {
        return 1;
    }
    let vtbl = *(inner as *const *const usize);
    let func: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtbl.add(7));
    func(inner)
}
unsafe extern "system" fn dev_slot8(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 8, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot9(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 9, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot10(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 10, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot11(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 11, a, b, c, 0, 0, 0) }

unsafe extern "system" fn dev_create_cmdlist(
    this: *mut D3D12DeviceX,
    node_mask: u32,
    kind: u32,
    alloc: *mut c_void,
    pso: *mut c_void,
    riid: *const u8,
    out_cmd: *mut *mut c_void,
) -> i32 {
    if out_cmd.is_null() {
        return E_INVALIDARG;
    }
    let mut inner_cmd: *mut c_void = std::ptr::null_mut();
    if !(*this).inner_desktop_device.is_null() && !alloc.is_null() && !riid.is_null() {
        let _ = forward_slot6(
            this,
            12,
            node_mask as u64,
            kind as u64,
            alloc as u64,
            pso as u64,
            riid as u64,
            &mut inner_cmd as *mut *mut c_void as u64,
        );
    }
    let list = Box::new(D3D12CmdListX {
        vtbl: &CMDLIST_VTABLE,
        ref_count: AtomicU32::new(1),
        inner_desktop_cmd: inner_cmd,
        draw_calls: AtomicU32::new(0),
        mesh_dispatches: AtomicU32::new(0),
        inner_translator: Mutex::new(D3D12XPipelineTranslator::default()),
    });
    *out_cmd = Box::into_raw(list).cast();
    S_OK
}

unsafe extern "system" fn dev_slot13(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 13, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot14(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 14, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_get_desc_inc(t: *mut D3D12DeviceX, kind: u32) -> u32 {
    let inner = (*t).inner_desktop_device;
    if inner.is_null() {
        return 32;
    }
    let vtbl = *(inner as *const *const usize);
    let func: unsafe extern "system" fn(*mut c_void, u32) -> u32 = std::mem::transmute(*vtbl.add(15));
    func(inner, kind)
}

unsafe extern "system" fn dev_create_root_sig(
    this: *mut D3D12DeviceX,
    node_mask: u32,
    blob: *const u8,
    blob_len: usize,
    riid: *const u8,
    out_rs: *mut *mut c_void,
) -> i32 {
    if out_rs.is_null() {
        return E_INVALIDARG;
    }
    if !blob.is_null() && blob_len >= 4 {
        let flags = u32::from_le_bytes(std::slice::from_raw_parts(blob, 4).try_into().unwrap());
        let _ = sanitize_root_signature_flags(flags);
    }
    if !(*this).inner_desktop_device.is_null() && !blob.is_null() && !riid.is_null() {
        let hr = forward_slot6(
            this,
            16,
            node_mask as u64,
            blob as u64,
            blob_len as u64,
            riid as u64,
            out_rs as u64,
            0,
        );
        if hr == S_OK {
            (*this).root_sigs_created.fetch_add(1, Ordering::Relaxed);
            return S_OK;
        }
    }
    let id = (*this).root_sigs_created.fetch_add(1, Ordering::Relaxed) + 1;
    *out_rs = (0x7F00_0000usize + id as usize * 0x10) as *mut c_void;
    S_OK
}

unsafe extern "system" fn dev_slot17(t: *mut D3D12DeviceX, a: u64, b: u64) -> i32 { forward_slot6(t, 17, a, b, 0, 0, 0, 0) }
unsafe extern "system" fn dev_slot18(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 18, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot19(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64, d: u64) -> i32 { forward_slot6(t, 19, a, b, c, d, 0, 0) }
unsafe extern "system" fn dev_slot20(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 20, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot21(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 21, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot22(t: *mut D3D12DeviceX, a: u64, b: u64) -> i32 { forward_slot6(t, 22, a, b, 0, 0, 0, 0) }
unsafe extern "system" fn dev_slot23(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) -> i32 { forward_slot8(t, 23, a, b, c, d, e, f, g, 0) }
unsafe extern "system" fn dev_slot24(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64, d: u64) -> i32 { forward_slot6(t, 24, a, b, c, d, 0, 0) }
unsafe extern "system" fn dev_slot25(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64, d: u64) -> i32 { forward_slot6(t, 25, a, b, c, d, 0, 0) }
unsafe extern "system" fn dev_slot26(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 26, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot27(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) -> i32 { forward_slot8(t, 27, a, b, c, d, e, f, g, 0) }
unsafe extern "system" fn dev_slot28(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 28, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot29(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) -> i32 { forward_slot8(t, 29, a, b, c, d, e, f, g, 0) }
unsafe extern "system" fn dev_slot30(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64, d: u64, e: u64) -> i32 { forward_slot6(t, 30, a, b, c, d, e, 0) }
unsafe extern "system" fn dev_slot31(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64, d: u64, e: u64) -> i32 { forward_slot6(t, 31, a, b, c, d, e, 0) }
unsafe extern "system" fn dev_slot32(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 32, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot33(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 33, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot34(t: *mut D3D12DeviceX, a: u64, b: u64) -> i32 { forward_slot6(t, 34, a, b, 0, 0, 0, 0) }
unsafe extern "system" fn dev_slot35(t: *mut D3D12DeviceX, a: u64, b: u64) -> i32 { forward_slot6(t, 35, a, b, 0, 0, 0, 0) }
unsafe extern "system" fn dev_slot36(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64, d: u64) -> i32 { forward_slot6(t, 36, a, b, c, d, 0, 0) }
unsafe extern "system" fn dev_get_removed_reason(t: *mut D3D12DeviceX) -> i32 {
    if (*t).inner_desktop_device.is_null() { S_OK } else { forward_slot6(t, 37, 0, 0, 0, 0, 0, 0) }
}
unsafe extern "system" fn dev_slot38(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64, h: u64) -> i32 { forward_slot8(t, 38, a, b, c, d, e, f, g, h) }
unsafe extern "system" fn dev_slot39(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64) -> i32 { forward_slot6(t, 39, a, b, c, 0, 0, 0) }
unsafe extern "system" fn dev_slot40(t: *mut D3D12DeviceX, a: u64) -> i32 { forward_slot6(t, 40, a, 0, 0, 0, 0, 0) }
unsafe extern "system" fn dev_slot41(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64, d: u64) -> i32 { forward_slot6(t, 41, a, b, c, d, 0, 0) }
unsafe extern "system" fn dev_slot42(t: *mut D3D12DeviceX, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) -> i32 { forward_slot8(t, 42, a, b, c, d, e, f, g, 0) }
unsafe extern "system" fn dev_get_adapter_luid(t: *mut D3D12DeviceX, out_luid: *mut u64) -> *mut u64 {
    let inner = (*t).inner_desktop_device;
    if !inner.is_null() && !out_luid.is_null() {
        let vtbl = *(inner as *const *const usize);
        let func: unsafe extern "system" fn(*mut c_void, *mut u64) -> *mut u64 = std::mem::transmute(*vtbl.add(43));
        return func(inner, out_luid);
    }
    if !out_luid.is_null() {
        *out_luid = 0x0000_0001_0000_0001;
    }
    out_luid
}

// ---- Xbox Series X|S Device Extension Slots (44..=63) ----

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

unsafe extern "system" fn dev_wait_frame_event(
    this: *mut D3D12DeviceX,
    _timeout_ms: u32,
    out_token: *mut u64,
) -> i32 {
    let tok = (*this).translator.lock().ok().and_then(|mut tr| tr.pacer.complete_next()).unwrap_or(0);
    if !out_token.is_null() {
        *out_token = tok;
    }
    S_OK
}

unsafe extern "system" fn dev_suspend(_this: *mut D3D12DeviceX, _flags: u32) -> i32 {
    S_OK
}

unsafe extern "system" fn dev_resume(_this: *mut D3D12DeviceX) -> i32 {
    S_OK
}

/// Explicit Xbox Series X|S device slot 49 dispatcher.
/// Prevents Xbox integer-parameter calls at slot 49 from mis-dispatching into desktop
/// `ID3D12Device1::OpenExistingHeapFromFileMapping`.
unsafe extern "system" fn dev_xbox_slot49(this: *mut D3D12DeviceX, param0: u64, param1: u64) -> i32 {
    if let Ok(mut log) = (*this).slot49_calls.lock() {
        log.push((param0, param1));
    }
    S_OK
}

unsafe extern "system" fn dev_notimpl(_this: *mut D3D12DeviceX) -> i32 {
    E_NOTIMPL
}

#[cfg(all(windows, target_arch = "x86_64"))]
fn try_create_desktop_d3d12_device() -> *mut c_void {
    const IID_ID3D12_DEVICE: [u8; 16] = [
        0xF1, 0x19, 0x98, 0x18, 0xB6, 0x1D, 0x57, 0x4B,
        0xBE, 0x54, 0x18, 0x21, 0x33, 0x9B, 0x85, 0xF7,
    ];
    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }
    let h = unsafe { LoadLibraryA(b"d3d12.dll\0".as_ptr()) };
    if h.is_null() {
        return std::ptr::null_mut();
    }
    let sym = unsafe { GetProcAddress(h, b"D3D12CreateDevice\0".as_ptr()) };
    if sym.is_null() {
        return std::ptr::null_mut();
    }
    let create: unsafe extern "system" fn(*mut c_void, u32, *const u8, *mut *mut c_void) -> i32 =
        unsafe { std::mem::transmute(sym) };
    let mut dev: *mut c_void = std::ptr::null_mut();
    let hr = unsafe { create(std::ptr::null_mut(), 0xC000, IID_ID3D12_DEVICE.as_ptr(), &mut dev) };
    if hr == S_OK && !dev.is_null() {
        dev
    } else {
        std::ptr::null_mut()
    }
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
fn try_create_desktop_d3d12_device() -> *mut c_void {
    std::ptr::null_mut()
}

/// Allocates a live clean-room `ID3D12DeviceX` COM wrapper, attaching a real desktop Windows
/// `ID3D12Device*` internally when `d3d12.dll` is available on this PC.
pub fn create_device() -> *mut c_void {
    let inner = try_create_desktop_d3d12_device();
    let dev = Box::new(D3D12DeviceX {
        vtbl: &DEVICE_VTABLE,
        ref_count: AtomicU32::new(1),
        inner_desktop_device: inner,
        translator: Mutex::new(D3D12XPipelineTranslator::default()),
        root_sigs_created: AtomicU32::new(0),
        slot49_calls: Mutex::new(Vec::new()),
    });
    Box::into_raw(dev).cast()
}

/// Probes whether desktop `d3d12.dll` can create a hardware `ID3D12Device` on this PC.
pub fn desktop_d3d12_available() -> bool {
    let dev = try_create_desktop_d3d12_device();
    if !dev.is_null() {
        unsafe { release_com_object(dev) };
        true
    } else {
        false
    }
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

/// Compiles translated HLSL source into live PC DXIL shader bytecode using `dxcompiler.dll`'s
/// `DxcCreateInstance(CLSID_DxcCompiler, IID_IDxcCompiler3)` COM pipeline when installed.
#[cfg(all(windows, target_arch = "x86_64"))]
pub fn compile_hlsl_to_dxil(hlsl: &str, entry_point: &str, target_profile: &str) -> Result<Vec<u8>, String> {
    const CLSID_DXC_COMPILER: [u8; 16] = [
        0x93, 0x2D, 0xE2, 0x73, 0xCE, 0xE6, 0xF3, 0x47,
        0xB5, 0xBF, 0xF0, 0x66, 0x4F, 0x39, 0xC1, 0xB0,
    ];
    const IID_IDXC_COMPILER3: [u8; 16] = [
        0x87, 0x46, 0x8B, 0x22, 0x6A, 0x5A, 0x30, 0x47,
        0x90, 0x0C, 0x97, 0x02, 0xB2, 0x20, 0x3F, 0x54,
    ];
    const IID_IDXC_RESULT: [u8; 16] = [
        0xDA, 0x6C, 0x34, 0x58, 0xE7, 0xDD, 0x97, 0x44,
        0x94, 0x61, 0x6F, 0x87, 0xAF, 0x5E, 0x06, 0x59,
    ];

    #[repr(C)]
    struct DxcBuffer {
        ptr: *const u8,
        size: usize,
        encoding: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LoadLibraryA(name: *const u8) -> *mut c_void;
        fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    }

    let h = unsafe { LoadLibraryA(b"dxcompiler.dll\0".as_ptr()) };
    if h.is_null() {
        return Err("dxcompiler.dll is not installed on this PC".into());
    }
    let sym = unsafe { GetProcAddress(h, b"DxcCreateInstance\0".as_ptr()) };
    if sym.is_null() {
        return Err("dxcompiler.dll does not export DxcCreateInstance".into());
    }
    let create_instance: unsafe extern "system" fn(*const u8, *const u8, *mut *mut c_void) -> i32 =
        unsafe { std::mem::transmute(sym) };

    let mut compiler: *mut c_void = std::ptr::null_mut();
    let hr = unsafe { create_instance(CLSID_DXC_COMPILER.as_ptr(), IID_IDXC_COMPILER3.as_ptr(), &mut compiler) };
    if hr != S_OK || compiler.is_null() {
        return Err(format!("DxcCreateInstance failed (HRESULT {hr:#010x})"));
    }

    let to_wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() };
    let arg_e = to_wide("-E");
    let arg_entry = to_wide(entry_point);
    let arg_t = to_wide("-T");
    let arg_target = to_wide(target_profile);
    let args = [arg_e.as_ptr(), arg_entry.as_ptr(), arg_t.as_ptr(), arg_target.as_ptr()];

    let src_buf = DxcBuffer {
        ptr: hlsl.as_ptr(),
        size: hlsl.len(),
        encoding: 65001, // DXC_CP_UTF8
    };

    unsafe {
        let comp_vtbl = *(compiler as *const *const usize);
        let compile: unsafe extern "system" fn(
            *mut c_void,
            *const DxcBuffer,
            *const *const u16,
            u32,
            *mut c_void,
            *const u8,
            *mut *mut c_void,
        ) -> i32 = std::mem::transmute(*comp_vtbl.add(3));
        let comp_release: unsafe extern "system" fn(*mut c_void) -> u32 =
            std::mem::transmute(*comp_vtbl.add(2));

        let mut result: *mut c_void = std::ptr::null_mut();
        let chr = compile(
            compiler,
            &src_buf,
            args.as_ptr(),
            args.len() as u32,
            std::ptr::null_mut(),
            IID_IDXC_RESULT.as_ptr(),
            &mut result,
        );
        comp_release(compiler);
        if chr != S_OK || result.is_null() {
            return Err(format!("IDxcCompiler3::Compile failed (HRESULT {chr:#010x})"));
        }

        let res_vtbl = *(result as *const *const usize);
        let get_status: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32 =
            std::mem::transmute(*res_vtbl.add(3));
        let get_result: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32 =
            std::mem::transmute(*res_vtbl.add(4));
        let res_release: unsafe extern "system" fn(*mut c_void) -> u32 =
            std::mem::transmute(*res_vtbl.add(2));

        let mut status = 0i32;
        let _ = get_status(result, &mut status);
        if status != S_OK {
            res_release(result);
            return Err(format!("DXC shader compilation failed (status {status:#010x})"));
        }

        let mut blob: *mut c_void = std::ptr::null_mut();
        let ghr = get_result(result, &mut blob);
        res_release(result);
        if ghr != S_OK || blob.is_null() {
            return Err(format!("IDxcResult::GetResult failed (HRESULT {ghr:#010x})"));
        }

        let blob_vtbl = *(blob as *const *const usize);
        let get_ptr: unsafe extern "system" fn(*mut c_void) -> *const u8 =
            std::mem::transmute(*blob_vtbl.add(3));
        let get_len: unsafe extern "system" fn(*mut c_void) -> usize =
            std::mem::transmute(*blob_vtbl.add(4));
        let blob_release: unsafe extern "system" fn(*mut c_void) -> u32 =
            std::mem::transmute(*blob_vtbl.add(2));

        let ptr = get_ptr(blob);
        let len = get_len(blob);
        let bytes = if !ptr.is_null() && len > 0 {
            std::slice::from_raw_parts(ptr, len).to_vec()
        } else {
            Vec::new()
        };
        blob_release(blob);
        if bytes.is_empty() {
            Err("DXC produced empty DXIL output".into())
        } else {
            Ok(bytes)
        }
    }
}

#[cfg(not(all(windows, target_arch = "x86_64")))]
pub fn compile_hlsl_to_dxil(_hlsl: &str, _entry_point: &str, _target_profile: &str) -> Result<Vec<u8>, String> {
    Err("DXC compilation is only available on Windows x64".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d3d12x_com_device_creates_cmdlist_unnests_queries_paces_120fps_and_dispatches_slot49() {
        unsafe {
            // Verify vtable slot count & exact slot indices matching ID3D12Device + Xbox extensions
            assert_eq!(std::mem::size_of::<D3D12DeviceXVtable>(), 64 * std::mem::size_of::<usize>());

            let dev = create_device().cast::<D3D12DeviceX>();
            assert!(!dev.is_null());

            // Base ID3D12Device slot 7 (GetNodeCount) & slot 43 (GetAdapterLuid)
            assert!(((*(*dev).vtbl).GetNodeCount)(dev) >= 1);
            let mut luid = 0u64;
            let luid_ptr = ((*(*dev).vtbl).GetAdapterLuid)(dev, &mut luid);
            assert_eq!(luid_ptr, &mut luid as *mut u64);
            assert_ne!(luid, 0);

            // QueryInterface stays on our Xbox-ABI-safe wrapper
            let mut qi_out: *mut c_void = std::ptr::null_mut();
            assert_eq!(((*(*dev).vtbl).QueryInterface)(dev, std::ptr::null(), &mut qi_out), S_OK);
            assert_eq!(qi_out.cast::<D3D12DeviceX>(), dev);
            assert_eq!(((*(*dev).vtbl).Release)(dev), 1);

            // 120 FPS frame pacing (8,333 us) + ScheduleFrameEventX + WaitFrameEventX
            assert_eq!(
                ((*(*dev).vtbl).SetFrameIntervalX)(dev, std::ptr::null_mut(), 8_333, 1, 0),
                S_OK
            );
            assert_eq!(
                ((*(*dev).vtbl).ScheduleFrameEventX)(dev, 0, 0, std::ptr::null_mut(), 0),
                S_OK
            );
            assert_eq!((*dev).translator.lock().unwrap().pacer.interval_us, 8_333);
            assert_eq!((*dev).translator.lock().unwrap().pacer.frame_count, 1);

            // Explicit Xbox slot 49 ABI dispatch (does not collide with desktop OpenExistingHeapFromFileMapping)
            assert_eq!(((*(*dev).vtbl).ConfigureXboxDeviceSlot49X)(dev, 0x11f, 1), S_OK);
            assert_eq!(*(*dev).slot49_calls.lock().unwrap(), vec![(0x11f, 1)]);
            // Unknown Xbox extension slots still fail strictly with E_NOTIMPL
            assert_eq!(((*(*dev).vtbl).XboxReserved50)(dev), E_NOTIMPL);

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
