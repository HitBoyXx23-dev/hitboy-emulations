//! Clean-room `ID3D12DeviceX` / `ID3D12GraphicsCommandListX` COM Vtable Bridge &
//! runtime `dxcompiler.dll` HLSL -> DXIL hook.
//!
//! Provides real Windows x64 COM-compatible wrappers:
//! - **`ID3D12DeviceX` (64-slot vtable)**:
//!   - Base slots `0..=43` match standard Windows `ID3D12Device` layout and forward directly to an
//!     internally created desktop Windows `ID3D12Device*` when `d3d12.dll` is present.
//!   - `QueryInterface` stays on this Xbox-ABI-safe wrapper instead of escaping to desktop `ID3D12Device1..11`.
//!   - `CreateCommandList` (slot 12) wraps desktop command lists inside our 80-slot `D3D12CmdListX` proxy.
//!   - `CreateRootSignature` (slot 16) sanitizes Xbox-only upper flags before forwarding.
//!   - Xbox extension slots `44..=63` explicitly dispatch frame pacing (`SetFrameIntervalX`,
//!     `ScheduleFrameEventX`, `WaitFrameEventX`), `SuspendX`/`ResumeX`, and slot 49 integer args.
//! - **`ID3D12GraphicsCommandListX` (80-slot vtable)**:
//!   - Base slots `0..=59` match exact Windows `ID3D12GraphicsCommandList` layout (`Close` = 9,
//!     `Reset` = 10, `DrawInstanced` = 12, `DrawIndexedInstanced` = 13, `Dispatch` = 14,
//!     `ResourceBarrier` = 26, `OMSetRenderTargets` = 46, `ClearRenderTargetView` = 48,
//!     `BeginQuery` = 51, `EndQuery` = 52, `ResolveQueryData` = 53, etc.), forwarding directly to
//!     the wrapped desktop `ID3D12GraphicsCommandList*` while un-nesting Xbox occlusion queries.
//!   - Extension slots `60..=79` support `DispatchMesh` and strict `E_NOTIMPL` on unknown slots.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use crate::gpu_bridge::{sanitize_root_signature_flags, D3D12XPipelineTranslator};

const S_OK: i32 = 0;
const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
const E_NOTIMPL: i32 = 0x8000_4001u32 as i32;
const CMDLIST_MAGIC: u64 = 0x5842_434D_444C_5354; // "XBCMDLST"

/// Full 32-slot COM vtable for `ID3D12CommandQueueX`:
/// - Slots `0..=18` match exact standard Windows `ID3D12CommandQueue` layout, unwrapping
///   `D3D12CmdListX*` arguments inside `ExecuteCommandLists` (slot 10) before calling desktop D3D12.
/// - Slot `19` implements Xbox `PresentX` with live frame-time telemetry.
#[repr(C)]
pub struct D3D12CmdQueueXVtable {
    // IUnknown (0..=2)
    pub QueryInterface: unsafe extern "system" fn(*mut D3D12CmdQueueX, *const u8, *mut *mut c_void) -> i32,
    pub AddRef: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> u32,
    pub Release: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> u32,
    // ID3D12Object (3..=6)
    pub GetPrivateData: unsafe extern "system" fn(*mut D3D12CmdQueueX, u64, u64, u64) -> i32,
    pub SetPrivateData: unsafe extern "system" fn(*mut D3D12CmdQueueX, u64, u64, u64) -> i32,
    pub SetPrivateDataInterface: unsafe extern "system" fn(*mut D3D12CmdQueueX, u64, u64) -> i32,
    pub SetName: unsafe extern "system" fn(*mut D3D12CmdQueueX, u64) -> i32,
    // ID3D12DeviceChild (7)
    pub GetDevice: unsafe extern "system" fn(*mut D3D12CmdQueueX, u64, u64) -> i32,
    // ID3D12CommandQueue (8..=18)
    pub UpdateTileMappings: unsafe extern "system" fn(*mut D3D12CmdQueueX, u64, u64, u64, u64, u64, u64, u64, u64),
    pub CopyTileMappings: unsafe extern "system" fn(*mut D3D12CmdQueueX, u64, u64, u64, u64, u64, u64),
    pub ExecuteCommandLists: unsafe extern "system" fn(*mut D3D12CmdQueueX, u32, *const *mut c_void),
    pub SetMarker: unsafe extern "system" fn(*mut D3D12CmdQueueX, u32, u64, u32),
    pub BeginEvent: unsafe extern "system" fn(*mut D3D12CmdQueueX, u32, u64, u32),
    pub EndEvent: unsafe extern "system" fn(*mut D3D12CmdQueueX),
    pub Signal: unsafe extern "system" fn(*mut D3D12CmdQueueX, *mut c_void, u64) -> i32,
    pub Wait: unsafe extern "system" fn(*mut D3D12CmdQueueX, *mut c_void, u64) -> i32,
    pub GetTimestampFrequency: unsafe extern "system" fn(*mut D3D12CmdQueueX, *mut u64) -> i32,
    pub GetClockCalibration: unsafe extern "system" fn(*mut D3D12CmdQueueX, *mut u64, *mut u64) -> i32,
    pub GetDesc: unsafe extern "system" fn(*mut D3D12CmdQueueX, *mut u64) -> *mut u64,
    // Xbox CommandQueue Extensions (19..=31)
    pub PresentX: unsafe extern "system" fn(*mut D3D12CmdQueueX, u32, *const c_void, *const c_void) -> i32,
    pub QueueExt20: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> i32,
    pub QueueExt21: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> i32,
    pub QueueExt22: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> i32,
    pub QueueExt23: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> i32,
    pub QueueExt24: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> i32,
    pub QueueExt25: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> i32,
    pub QueueExt26: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> i32,
    pub QueueExt27: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> i32,
    pub QueueExt28: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> i32,
    pub QueueExt29: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> i32,
    pub QueueExt30: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> i32,
    pub QueueExt31: unsafe extern "system" fn(*mut D3D12CmdQueueX) -> i32,
}

#[repr(C)]
pub struct D3D12CmdQueueX {
    pub vtbl: *const D3D12CmdQueueXVtable,
    ref_count: AtomicU32,
    pub inner_desktop_queue: *mut c_void,
    pub executed_batches: AtomicU32,
    pub executed_lists: AtomicU32,
    pub presents: AtomicU32,
    pub pacer: Mutex<crate::exe_convert::FramePacer>,
}

static CMDQUEUE_VTABLE: D3D12CmdQueueXVtable = D3D12CmdQueueXVtable {
    QueryInterface: q_qi,
    AddRef: q_addref,
    Release: q_release,
    GetPrivateData: q_hr3,
    SetPrivateData: q_hr4,
    SetPrivateDataInterface: q_hr5,
    SetName: q_hr6,
    GetDevice: q_hr7,
    UpdateTileMappings: q_v8,
    CopyTileMappings: q_v9,
    ExecuteCommandLists: q_execute_command_lists,
    SetMarker: q_v11,
    BeginEvent: q_v12,
    EndEvent: q_v13,
    Signal: q_signal,
    Wait: q_wait,
    GetTimestampFrequency: q_get_ts_freq,
    GetClockCalibration: q_get_clock_cal,
    GetDesc: q_get_desc,
    PresentX: q_present_x,
    QueueExt20: q_notimpl,
    QueueExt21: q_notimpl,
    QueueExt22: q_notimpl,
    QueueExt23: q_notimpl,
    QueueExt24: q_notimpl,
    QueueExt25: q_notimpl,
    QueueExt26: q_notimpl,
    QueueExt27: q_notimpl,
    QueueExt28: q_notimpl,
    QueueExt29: q_notimpl,
    QueueExt30: q_notimpl,
    QueueExt31: q_notimpl,
};

unsafe fn forward_q_hr(this: *mut D3D12CmdQueueX, slot: usize, a1: u64, a2: u64, a3: u64) -> i32 {
    let inner = (*this).inner_desktop_queue;
    if inner.is_null() {
        return S_OK;
    }
    let vtbl = *(inner as *const *const usize);
    let func: RawFn6 = std::mem::transmute(*vtbl.add(slot));
    func(inner, a1, a2, a3, 0, 0, 0)
}

unsafe fn forward_q_void(this: *mut D3D12CmdQueueX, slot: usize, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) {
    let inner = (*this).inner_desktop_queue;
    if inner.is_null() {
        return;
    }
    let vtbl = *(inner as *const *const usize);
    let func: VoidFn6 = std::mem::transmute(*vtbl.add(slot));
    func(inner, a1, a2, a3, a4, a5, a6);
}

unsafe extern "system" fn q_qi(this: *mut D3D12CmdQueueX, _riid: *const u8, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    q_addref(this);
    *out = this.cast();
    S_OK
}

unsafe extern "system" fn q_addref(this: *mut D3D12CmdQueueX) -> u32 {
    (*this).ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn q_release(this: *mut D3D12CmdQueueX) -> u32 {
    let prev = (*this).ref_count.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        let inner = (*this).inner_desktop_queue;
        if !inner.is_null() {
            release_com_object(inner);
        }
        drop(Box::from_raw(this));
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn q_hr3(t: *mut D3D12CmdQueueX, a: u64, b: u64, c: u64) -> i32 { forward_q_hr(t, 3, a, b, c) }
unsafe extern "system" fn q_hr4(t: *mut D3D12CmdQueueX, a: u64, b: u64, c: u64) -> i32 { forward_q_hr(t, 4, a, b, c) }
unsafe extern "system" fn q_hr5(t: *mut D3D12CmdQueueX, a: u64, b: u64) -> i32 { forward_q_hr(t, 5, a, b, 0) }
unsafe extern "system" fn q_hr6(t: *mut D3D12CmdQueueX, a: u64) -> i32 { forward_q_hr(t, 6, a, 0, 0) }
unsafe extern "system" fn q_hr7(t: *mut D3D12CmdQueueX, a: u64, b: u64) -> i32 { forward_q_hr(t, 7, a, b, 0) }
unsafe extern "system" fn q_v8(t: *mut D3D12CmdQueueX, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, _g: u64, _h: u64) {
    forward_q_void(t, 8, a, b, c, d, e, f);
}
unsafe extern "system" fn q_v9(t: *mut D3D12CmdQueueX, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) {
    forward_q_void(t, 9, a, b, c, d, e, f);
}

unsafe extern "system" fn q_execute_command_lists(this: *mut D3D12CmdQueueX, num: u32, pp_lists: *const *mut c_void) {
    (*this).executed_batches.fetch_add(1, Ordering::Relaxed);
    (*this).executed_lists.fetch_add(num, Ordering::Relaxed);
    let inner = (*this).inner_desktop_queue;
    if inner.is_null() || num == 0 || pp_lists.is_null() {
        return;
    }
    let slice = std::slice::from_raw_parts(pp_lists, num as usize);
    let mut unwrapped: Vec<*mut c_void> = Vec::with_capacity(slice.len());
    for &ptr in slice {
        if !ptr.is_null() {
            let candidate = ptr.cast::<D3D12CmdListX>();
            if (*candidate).vtbl == &CMDLIST_VTABLE && (*candidate).magic == CMDLIST_MAGIC {
                if !(*candidate).inner_desktop_cmd.is_null() {
                    unwrapped.push((*candidate).inner_desktop_cmd);
                }
            } else {
                unwrapped.push(ptr);
            }
        }
    }
    if !unwrapped.is_empty() {
        let vtbl = *(inner as *const *const usize);
        let func: unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void) =
            std::mem::transmute(*vtbl.add(10));
        func(inner, unwrapped.len() as u32, unwrapped.as_ptr());
    }
}

unsafe extern "system" fn q_v11(t: *mut D3D12CmdQueueX, a: u32, b: u64, c: u32) { forward_q_void(t, 11, a as u64, b, c as u64, 0, 0, 0); }
unsafe extern "system" fn q_v12(t: *mut D3D12CmdQueueX, a: u32, b: u64, c: u32) { forward_q_void(t, 12, a as u64, b, c as u64, 0, 0, 0); }
unsafe extern "system" fn q_v13(t: *mut D3D12CmdQueueX) { forward_q_void(t, 13, 0, 0, 0, 0, 0, 0); }
unsafe extern "system" fn q_signal(t: *mut D3D12CmdQueueX, fence: *mut c_void, value: u64) -> i32 {
    forward_q_hr(t, 14, fence as u64, value, 0)
}
unsafe extern "system" fn q_wait(t: *mut D3D12CmdQueueX, fence: *mut c_void, value: u64) -> i32 {
    forward_q_hr(t, 15, fence as u64, value, 0)
}
unsafe extern "system" fn q_get_ts_freq(t: *mut D3D12CmdQueueX, out_freq: *mut u64) -> i32 {
    if (*t).inner_desktop_queue.is_null() {
        if !out_freq.is_null() {
            *out_freq = 1_000_000_000;
        }
        return S_OK;
    }
    forward_q_hr(t, 16, out_freq as u64, 0, 0)
}
unsafe extern "system" fn q_get_clock_cal(t: *mut D3D12CmdQueueX, gpu_ts: *mut u64, cpu_ts: *mut u64) -> i32 {
    forward_q_hr(t, 17, gpu_ts as u64, cpu_ts as u64, 0)
}
unsafe extern "system" fn q_get_desc(t: *mut D3D12CmdQueueX, out_desc: *mut u64) -> *mut u64 {
    let inner = (*t).inner_desktop_queue;
    if !inner.is_null() && !out_desc.is_null() {
        let vtbl = *(inner as *const *const usize);
        let func: unsafe extern "system" fn(*mut c_void, *mut u64) -> *mut u64 = std::mem::transmute(*vtbl.add(18));
        return func(inner, out_desc);
    }
    out_desc
}
unsafe extern "system" fn q_present_x(
    this: *mut D3D12CmdQueueX,
    _plane_count: u32,
    _plane_params: *const c_void,
    _present_params: *const c_void,
) -> i32 {
    (*this).presents.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut p) = (*this).pacer.lock() {
        p.schedule_present();
    }
    S_OK
}
unsafe extern "system" fn q_notimpl(_this: *mut D3D12CmdQueueX) -> i32 {
    E_NOTIMPL
}

type RawFn6 = unsafe extern "system" fn(*mut c_void, u64, u64, u64, u64, u64, u64) -> i32;
type RawFn8 = unsafe extern "system" fn(*mut c_void, u64, u64, u64, u64, u64, u64, u64, u64) -> i32;
type VoidFn6 = unsafe extern "system" fn(*mut c_void, u64, u64, u64, u64, u64, u64);

unsafe fn release_com_object(obj: *mut c_void) {
    if !obj.is_null() {
        let vtbl = *(obj as *const *const usize);
        let release: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtbl.add(2));
        release(obj);
    }
}

unsafe fn forward_cmd_hr(this: *mut D3D12CmdListX, slot: usize, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> i32 {
    let inner = (*this).inner_desktop_cmd;
    if inner.is_null() {
        return S_OK;
    }
    let vtbl = *(inner as *const *const usize);
    let func: RawFn6 = std::mem::transmute(*vtbl.add(slot));
    func(inner, a1, a2, a3, a4, a5, a6)
}

unsafe fn forward_cmd_void(this: *mut D3D12CmdListX, slot: usize, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) {
    let inner = (*this).inner_desktop_cmd;
    if inner.is_null() {
        return;
    }
    let vtbl = *(inner as *const *const usize);
    let func: VoidFn6 = std::mem::transmute(*vtbl.add(slot));
    func(inner, a1, a2, a3, a4, a5, a6);
}

/// Full 80-slot COM vtable for `ID3D12GraphicsCommandListX`:
/// - Slots `0..=59` match the exact standard Windows `ID3D12GraphicsCommandList` vtable.
/// - Slots `60..=79` provide Mesh Shader / Xbox command-list extensions.
#[repr(C)]
pub struct D3D12CmdListXVtable {
    // IUnknown (0..=2)
    pub QueryInterface: unsafe extern "system" fn(*mut D3D12CmdListX, *const u8, *mut *mut c_void) -> i32,
    pub AddRef: unsafe extern "system" fn(*mut D3D12CmdListX) -> u32,
    pub Release: unsafe extern "system" fn(*mut D3D12CmdListX) -> u32,
    // ID3D12Object (3..=6)
    pub GetPrivateData: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64, u64) -> i32,
    pub SetPrivateData: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64, u64) -> i32,
    pub SetPrivateDataInterface: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64) -> i32,
    pub SetName: unsafe extern "system" fn(*mut D3D12CmdListX, u64) -> i32,
    // ID3D12DeviceChild (7) & ID3D12CommandList (8)
    pub GetDevice: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64) -> i32,
    pub GetType: unsafe extern "system" fn(*mut D3D12CmdListX) -> u32,
    // ID3D12GraphicsCommandList (9..=59)
    pub Close: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub Reset: unsafe extern "system" fn(*mut D3D12CmdListX, *mut c_void, *mut c_void) -> i32,
    pub ClearState: unsafe extern "system" fn(*mut D3D12CmdListX, *mut c_void),
    pub DrawInstanced: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u32, u32, u32),
    pub DrawIndexedInstanced: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u32, u32, i32, u32),
    pub Dispatch: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u32, u32),
    pub CopyBufferRegion: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64, u64, u64, u64),
    pub CopyTextureRegion: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64, u64, u64, u64, u64),
    pub CopyResource: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64),
    pub CopyTiles: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64, u64, u64, u64, u64),
    pub ResolveSubresource: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64, u64, u64, u64),
    pub IASetPrimitiveTopology: unsafe extern "system" fn(*mut D3D12CmdListX, u32),
    pub RSSetViewports: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64),
    pub RSSetScissorRects: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64),
    pub OMSetBlendFactor: unsafe extern "system" fn(*mut D3D12CmdListX, u64),
    pub OMSetStencilRef: unsafe extern "system" fn(*mut D3D12CmdListX, u32),
    pub SetPipelineState: unsafe extern "system" fn(*mut D3D12CmdListX, u64),
    pub ResourceBarrier: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64),
    pub ExecuteBundle: unsafe extern "system" fn(*mut D3D12CmdListX, u64),
    pub SetDescriptorHeaps: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64),
    pub SetComputeRootSignature: unsafe extern "system" fn(*mut D3D12CmdListX, u64),
    pub SetGraphicsRootSignature: unsafe extern "system" fn(*mut D3D12CmdListX, u64),
    pub SetComputeRootDescriptorTable: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64),
    pub SetGraphicsRootDescriptorTable: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64),
    pub SetComputeRoot32BitConstant: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u32, u32),
    pub SetGraphicsRoot32BitConstant: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u32, u32),
    pub SetComputeRoot32BitConstants: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u32, u64, u32),
    pub SetGraphicsRoot32BitConstants: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u32, u64, u32),
    pub SetComputeRootConstantBufferView: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64),
    pub SetGraphicsRootConstantBufferView: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64),
    pub SetComputeRootShaderResourceView: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64),
    pub SetGraphicsRootShaderResourceView: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64),
    pub SetComputeRootUnorderedAccessView: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64),
    pub SetGraphicsRootUnorderedAccessView: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64),
    pub IASetIndexBuffer: unsafe extern "system" fn(*mut D3D12CmdListX, u64),
    pub IASetVertexBuffers: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u32, u64),
    pub SOSetTargets: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u32, u64),
    pub OMSetRenderTargets: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64, i32, u64),
    pub ClearDepthStencilView: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u32, u32, u8, u32, u64),
    pub ClearRenderTargetView: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64, u32, u64),
    pub ClearUnorderedAccessViewUint: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64, u64, u64, u32, u64),
    pub ClearUnorderedAccessViewFloat: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64, u64, u64, u32, u64),
    pub DiscardResource: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64),
    pub BeginQuery: unsafe extern "system" fn(*mut D3D12CmdListX, *mut c_void, u32, u32),
    pub EndQuery: unsafe extern "system" fn(*mut D3D12CmdListX, *mut c_void, u32, u32),
    pub ResolveQueryData: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u32, u32, u32, u64, u64),
    pub SetPredication: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u64, u32),
    pub SetMarker: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64, u32),
    pub BeginEvent: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u64, u32),
    pub EndEvent: unsafe extern "system" fn(*mut D3D12CmdListX),
    pub ExecuteIndirect: unsafe extern "system" fn(*mut D3D12CmdListX, u64, u32, u64, u64, u64, u64),
    // Extensions (60..=79): Mesh dispatch + reserved Xbox slots
    pub DispatchMesh: unsafe extern "system" fn(*mut D3D12CmdListX, u32, u32, u32),
    pub CmdExt61: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt62: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt63: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt64: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt65: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt66: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt67: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt68: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt69: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt70: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt71: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt72: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt73: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt74: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt75: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt76: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt77: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt78: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
    pub CmdExt79: unsafe extern "system" fn(*mut D3D12CmdListX) -> i32,
}

#[repr(C)]
pub struct D3D12CmdListX {
    pub vtbl: *const D3D12CmdListXVtable,
    pub magic: u64,
    ref_count: AtomicU32,
    pub inner_desktop_cmd: *mut c_void,
    pub draw_calls: AtomicU32,
    pub compute_dispatches: AtomicU32,
    pub mesh_dispatches: AtomicU32,
    pub inner_translator: Mutex<D3D12XPipelineTranslator>,
}

static CMDLIST_VTABLE: D3D12CmdListXVtable = D3D12CmdListXVtable {
    QueryInterface: cmd_qi,
    AddRef: cmd_addref,
    Release: cmd_release,
    GetPrivateData: cmd_hr3,
    SetPrivateData: cmd_hr4,
    SetPrivateDataInterface: cmd_hr5,
    SetName: cmd_hr6,
    GetDevice: cmd_hr7,
    GetType: cmd_get_type,
    Close: cmd_close,
    Reset: cmd_reset,
    ClearState: cmd_clear_state,
    DrawInstanced: cmd_draw_instanced,
    DrawIndexedInstanced: cmd_draw_indexed,
    Dispatch: cmd_dispatch,
    CopyBufferRegion: cmd_v15,
    CopyTextureRegion: cmd_v16,
    CopyResource: cmd_v17,
    CopyTiles: cmd_v18,
    ResolveSubresource: cmd_v19,
    IASetPrimitiveTopology: cmd_v20,
    RSSetViewports: cmd_v21,
    RSSetScissorRects: cmd_v22,
    OMSetBlendFactor: cmd_v23,
    OMSetStencilRef: cmd_v24,
    SetPipelineState: cmd_v25,
    ResourceBarrier: cmd_v26,
    ExecuteBundle: cmd_v27,
    SetDescriptorHeaps: cmd_v28,
    SetComputeRootSignature: cmd_v29,
    SetGraphicsRootSignature: cmd_v30,
    SetComputeRootDescriptorTable: cmd_v31,
    SetGraphicsRootDescriptorTable: cmd_v32,
    SetComputeRoot32BitConstant: cmd_v33,
    SetGraphicsRoot32BitConstant: cmd_v34,
    SetComputeRoot32BitConstants: cmd_v35,
    SetGraphicsRoot32BitConstants: cmd_v36,
    SetComputeRootConstantBufferView: cmd_v37,
    SetGraphicsRootConstantBufferView: cmd_v38,
    SetComputeRootShaderResourceView: cmd_v39,
    SetGraphicsRootShaderResourceView: cmd_v40,
    SetComputeRootUnorderedAccessView: cmd_v41,
    SetGraphicsRootUnorderedAccessView: cmd_v42,
    IASetIndexBuffer: cmd_v43,
    IASetVertexBuffers: cmd_v44,
    SOSetTargets: cmd_v45,
    OMSetRenderTargets: cmd_v46,
    ClearDepthStencilView: cmd_v47,
    ClearRenderTargetView: cmd_v48,
    ClearUnorderedAccessViewUint: cmd_v49,
    ClearUnorderedAccessViewFloat: cmd_v50,
    DiscardResource: cmd_v51,
    BeginQuery: cmd_begin_query,
    EndQuery: cmd_end_query,
    ResolveQueryData: cmd_v54,
    SetPredication: cmd_v55,
    SetMarker: cmd_v56,
    BeginEvent: cmd_v57,
    EndEvent: cmd_v58,
    ExecuteIndirect: cmd_v59,
    DispatchMesh: cmd_dispatch_mesh,
    CmdExt61: cmd_notimpl,
    CmdExt62: cmd_notimpl,
    CmdExt63: cmd_notimpl,
    CmdExt64: cmd_notimpl,
    CmdExt65: cmd_notimpl,
    CmdExt66: cmd_notimpl,
    CmdExt67: cmd_notimpl,
    CmdExt68: cmd_notimpl,
    CmdExt69: cmd_notimpl,
    CmdExt70: cmd_notimpl,
    CmdExt71: cmd_notimpl,
    CmdExt72: cmd_notimpl,
    CmdExt73: cmd_notimpl,
    CmdExt74: cmd_notimpl,
    CmdExt75: cmd_notimpl,
    CmdExt76: cmd_notimpl,
    CmdExt77: cmd_notimpl,
    CmdExt78: cmd_notimpl,
    CmdExt79: cmd_notimpl,
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

unsafe extern "system" fn cmd_hr3(t: *mut D3D12CmdListX, a: u64, b: u64, c: u64) -> i32 { forward_cmd_hr(t, 3, a, b, c, 0, 0, 0) }
unsafe extern "system" fn cmd_hr4(t: *mut D3D12CmdListX, a: u64, b: u64, c: u64) -> i32 { forward_cmd_hr(t, 4, a, b, c, 0, 0, 0) }
unsafe extern "system" fn cmd_hr5(t: *mut D3D12CmdListX, a: u64, b: u64) -> i32 { forward_cmd_hr(t, 5, a, b, 0, 0, 0, 0) }
unsafe extern "system" fn cmd_hr6(t: *mut D3D12CmdListX, a: u64) -> i32 { forward_cmd_hr(t, 6, a, 0, 0, 0, 0, 0) }
unsafe extern "system" fn cmd_hr7(t: *mut D3D12CmdListX, a: u64, b: u64) -> i32 { forward_cmd_hr(t, 7, a, b, 0, 0, 0, 0) }
unsafe extern "system" fn cmd_get_type(t: *mut D3D12CmdListX) -> u32 {
    let inner = (*t).inner_desktop_cmd;
    if inner.is_null() {
        return 0; // D3D12_COMMAND_LIST_TYPE_DIRECT
    }
    let vtbl = *(inner as *const *const usize);
    let func: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtbl.add(8));
    func(inner)
}

unsafe extern "system" fn cmd_close(this: *mut D3D12CmdListX) -> i32 {
    forward_cmd_hr(this, 9, 0, 0, 0, 0, 0, 0)
}

unsafe extern "system" fn cmd_reset(this: *mut D3D12CmdListX, alloc: *mut c_void, pso: *mut c_void) -> i32 {
    forward_cmd_hr(this, 10, alloc as u64, pso as u64, 0, 0, 0, 0)
}

unsafe extern "system" fn cmd_clear_state(this: *mut D3D12CmdListX, pso: *mut c_void) {
    forward_cmd_void(this, 11, pso as u64, 0, 0, 0, 0, 0);
}

unsafe extern "system" fn cmd_draw_instanced(this: *mut D3D12CmdListX, v: u32, i: u32, sv: u32, si: u32) {
    (*this).draw_calls.fetch_add(1, Ordering::Relaxed);
    forward_cmd_void(this, 12, v as u64, i as u64, sv as u64, si as u64, 0, 0);
}

unsafe extern "system" fn cmd_draw_indexed(this: *mut D3D12CmdListX, idx: u32, inst: u32, sidx: u32, bv: i32, sinst: u32) {
    (*this).draw_calls.fetch_add(1, Ordering::Relaxed);
    forward_cmd_void(this, 13, idx as u64, inst as u64, sidx as u64, bv as u64, sinst as u64, 0);
}

unsafe extern "system" fn cmd_dispatch(this: *mut D3D12CmdListX, x: u32, y: u32, z: u32) {
    (*this).compute_dispatches.fetch_add(1, Ordering::Relaxed);
    forward_cmd_void(this, 14, x as u64, y as u64, z as u64, 0, 0, 0);
}

unsafe extern "system" fn cmd_v15(t: *mut D3D12CmdListX, a: u64, b: u64, c: u64, d: u64, e: u64) { forward_cmd_void(t, 15, a, b, c, d, e, 0); }
unsafe extern "system" fn cmd_v16(t: *mut D3D12CmdListX, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) { forward_cmd_void(t, 16, a, b, c, d, e, f); }
unsafe extern "system" fn cmd_v17(t: *mut D3D12CmdListX, a: u64, b: u64) { forward_cmd_void(t, 17, a, b, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v18(t: *mut D3D12CmdListX, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) { forward_cmd_void(t, 18, a, b, c, d, e, f); }
unsafe extern "system" fn cmd_v19(t: *mut D3D12CmdListX, a: u64, b: u64, c: u64, d: u64, e: u64) { forward_cmd_void(t, 19, a, b, c, d, e, 0); }
unsafe extern "system" fn cmd_v20(t: *mut D3D12CmdListX, a: u32) { forward_cmd_void(t, 20, a as u64, 0, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v21(t: *mut D3D12CmdListX, a: u32, b: u64) { forward_cmd_void(t, 21, a as u64, b, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v22(t: *mut D3D12CmdListX, a: u32, b: u64) { forward_cmd_void(t, 22, a as u64, b, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v23(t: *mut D3D12CmdListX, a: u64) { forward_cmd_void(t, 23, a, 0, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v24(t: *mut D3D12CmdListX, a: u32) { forward_cmd_void(t, 24, a as u64, 0, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v25(t: *mut D3D12CmdListX, a: u64) { forward_cmd_void(t, 25, a, 0, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v26(t: *mut D3D12CmdListX, a: u32, b: u64) { forward_cmd_void(t, 26, a as u64, b, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v27(t: *mut D3D12CmdListX, a: u64) { forward_cmd_void(t, 27, a, 0, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v28(t: *mut D3D12CmdListX, a: u32, b: u64) { forward_cmd_void(t, 28, a as u64, b, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v29(t: *mut D3D12CmdListX, a: u64) { forward_cmd_void(t, 29, a, 0, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v30(t: *mut D3D12CmdListX, a: u64) { forward_cmd_void(t, 30, a, 0, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v31(t: *mut D3D12CmdListX, a: u32, b: u64) { forward_cmd_void(t, 31, a as u64, b, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v32(t: *mut D3D12CmdListX, a: u32, b: u64) { forward_cmd_void(t, 32, a as u64, b, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v33(t: *mut D3D12CmdListX, a: u32, b: u32, c: u32) { forward_cmd_void(t, 33, a as u64, b as u64, c as u64, 0, 0, 0); }
unsafe extern "system" fn cmd_v34(t: *mut D3D12CmdListX, a: u32, b: u32, c: u32) { forward_cmd_void(t, 34, a as u64, b as u64, c as u64, 0, 0, 0); }
unsafe extern "system" fn cmd_v35(t: *mut D3D12CmdListX, a: u32, b: u32, c: u64, d: u32) { forward_cmd_void(t, 35, a as u64, b as u64, c, d as u64, 0, 0); }
unsafe extern "system" fn cmd_v36(t: *mut D3D12CmdListX, a: u32, b: u32, c: u64, d: u32) { forward_cmd_void(t, 36, a as u64, b as u64, c, d as u64, 0, 0); }
unsafe extern "system" fn cmd_v37(t: *mut D3D12CmdListX, a: u32, b: u64) { forward_cmd_void(t, 37, a as u64, b, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v38(t: *mut D3D12CmdListX, a: u32, b: u64) { forward_cmd_void(t, 38, a as u64, b, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v39(t: *mut D3D12CmdListX, a: u32, b: u64) { forward_cmd_void(t, 39, a as u64, b, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v40(t: *mut D3D12CmdListX, a: u32, b: u64) { forward_cmd_void(t, 40, a as u64, b, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v41(t: *mut D3D12CmdListX, a: u32, b: u64) { forward_cmd_void(t, 41, a as u64, b, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v42(t: *mut D3D12CmdListX, a: u32, b: u64) { forward_cmd_void(t, 42, a as u64, b, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v43(t: *mut D3D12CmdListX, a: u64) { forward_cmd_void(t, 43, a, 0, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v44(t: *mut D3D12CmdListX, a: u32, b: u32, c: u64) { forward_cmd_void(t, 44, a as u64, b as u64, c, 0, 0, 0); }
unsafe extern "system" fn cmd_v45(t: *mut D3D12CmdListX, a: u32, b: u32, c: u64) { forward_cmd_void(t, 45, a as u64, b as u64, c, 0, 0, 0); }
unsafe extern "system" fn cmd_v46(t: *mut D3D12CmdListX, a: u32, b: u64, c: i32, d: u64) { forward_cmd_void(t, 46, a as u64, b, c as u64, d, 0, 0); }
unsafe extern "system" fn cmd_v47(t: *mut D3D12CmdListX, a: u64, b: u32, c: u32, d: u8, e: u32, f: u64) { forward_cmd_void(t, 47, a, b as u64, c as u64, d as u64, e as u64, f); }
unsafe extern "system" fn cmd_v48(t: *mut D3D12CmdListX, a: u64, b: u64, c: u32, d: u64) { forward_cmd_void(t, 48, a, b, c as u64, d, 0, 0); }
unsafe extern "system" fn cmd_v49(t: *mut D3D12CmdListX, a: u64, b: u64, c: u64, d: u64, e: u32, f: u64) { forward_cmd_void(t, 49, a, b, c, d, e as u64, f); }
unsafe extern "system" fn cmd_v50(t: *mut D3D12CmdListX, a: u64, b: u64, c: u64, d: u64, e: u32, f: u64) { forward_cmd_void(t, 50, a, b, c, d, e as u64, f); }
unsafe extern "system" fn cmd_v51(t: *mut D3D12CmdListX, a: u64, b: u64) { forward_cmd_void(t, 51, a, b, 0, 0, 0, 0); }

unsafe extern "system" fn cmd_begin_query(this: *mut D3D12CmdListX, heap: *mut c_void, kind: u32, index: u32) {
    let ops = if let Ok(mut tr) = (*this).inner_translator.lock() {
        let start = tr.occlusion.emitted_ops.len();
        tr.occlusion.begin_query(index);
        tr.occlusion.emitted_ops[start..].to_vec()
    } else {
        Vec::new()
    };
    if !(*this).inner_desktop_cmd.is_null() && !heap.is_null() {
        for (is_begin, idx) in ops {
            forward_cmd_void(this, if is_begin { 52 } else { 53 }, heap as u64, kind as u64, idx as u64, 0, 0, 0);
        }
    }
}

unsafe extern "system" fn cmd_end_query(this: *mut D3D12CmdListX, heap: *mut c_void, kind: u32, index: u32) {
    let ops = if let Ok(mut tr) = (*this).inner_translator.lock() {
        let start = tr.occlusion.emitted_ops.len();
        tr.occlusion.end_query(index);
        tr.occlusion.emitted_ops[start..].to_vec()
    } else {
        Vec::new()
    };
    if !(*this).inner_desktop_cmd.is_null() && !heap.is_null() {
        for (is_begin, idx) in ops {
            forward_cmd_void(this, if is_begin { 52 } else { 53 }, heap as u64, kind as u64, idx as u64, 0, 0, 0);
        }
    }
}

unsafe extern "system" fn cmd_v54(t: *mut D3D12CmdListX, a: u64, b: u32, c: u32, d: u32, e: u64, f: u64) { forward_cmd_void(t, 54, a, b as u64, c as u64, d as u64, e, f); }
unsafe extern "system" fn cmd_v55(t: *mut D3D12CmdListX, a: u64, b: u64, c: u32) { forward_cmd_void(t, 55, a, b, c as u64, 0, 0, 0); }
unsafe extern "system" fn cmd_v56(t: *mut D3D12CmdListX, a: u32, b: u64, c: u32) { forward_cmd_void(t, 56, a as u64, b, c as u64, 0, 0, 0); }
unsafe extern "system" fn cmd_v57(t: *mut D3D12CmdListX, a: u32, b: u64, c: u32) { forward_cmd_void(t, 57, a as u64, b, c as u64, 0, 0, 0); }
unsafe extern "system" fn cmd_v58(t: *mut D3D12CmdListX) { forward_cmd_void(t, 58, 0, 0, 0, 0, 0, 0); }
unsafe extern "system" fn cmd_v59(t: *mut D3D12CmdListX, a: u64, b: u32, c: u64, d: u64, e: u64, f: u64) { forward_cmd_void(t, 59, a, b as u64, c, d, e, f); }

unsafe extern "system" fn cmd_dispatch_mesh(this: *mut D3D12CmdListX, _x: u32, _y: u32, _z: u32) {
    (*this).mesh_dispatches.fetch_add(1, Ordering::Relaxed);
}

unsafe extern "system" fn cmd_notimpl(_this: *mut D3D12CmdListX) -> i32 {
    E_NOTIMPL
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
unsafe extern "system" fn dev_slot8(t: *mut D3D12DeviceX, desc: u64, riid: u64, out_q: u64) -> i32 {
    if out_q == 0 {
        return E_INVALIDARG;
    }
    let mut inner_q: *mut c_void = std::ptr::null_mut();
    if !(*t).inner_desktop_device.is_null() && desc != 0 && riid != 0 {
        let hr = forward_slot6(t, 8, desc, riid, &mut inner_q as *mut *mut c_void as u64, 0, 0, 0);
        if hr != S_OK {
            return hr;
        }
    }
    let q = Box::new(D3D12CmdQueueX {
        vtbl: &CMDQUEUE_VTABLE,
        ref_count: AtomicU32::new(1),
        inner_desktop_queue: inner_q,
        executed_batches: AtomicU32::new(0),
        executed_lists: AtomicU32::new(0),
        presents: AtomicU32::new(0),
        pacer: Mutex::new(crate::exe_convert::FramePacer::default()),
    });
    *(out_q as *mut *mut c_void) = Box::into_raw(q).cast();
    S_OK
}
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
        magic: CMDLIST_MAGIC,
        ref_count: AtomicU32::new(1),
        inner_desktop_cmd: inner_cmd,
        draw_calls: AtomicU32::new(0),
        compute_dispatches: AtomicU32::new(0),
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
        encoding: 65001,
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
            assert_eq!(std::mem::size_of::<D3D12DeviceXVtable>(), 64 * std::mem::size_of::<usize>());
            assert_eq!(std::mem::size_of::<D3D12CmdListXVtable>(), 80 * std::mem::size_of::<usize>());
            assert_eq!(std::mem::size_of::<D3D12CmdQueueXVtable>(), 32 * std::mem::size_of::<usize>());

            let dev = create_device().cast::<D3D12DeviceX>();
            assert!(!dev.is_null());

            // Base ID3D12Device slot 7 (GetNodeCount) & slot 43 (GetAdapterLuid)
            assert!(((*(*dev).vtbl).GetNodeCount)(dev) >= 1);
            let mut luid = 0u64;
            let luid_ptr = ((*(*dev).vtbl).GetAdapterLuid)(dev, &mut luid);
            assert_eq!(luid_ptr, &mut luid as *mut u64);
            assert_ne!(luid, 0);

            // If desktop hardware D3D12 is present on this PC, test real CommandQueue + CommandAllocator + CommandList + Fence
            let mut queue_ptr: *mut c_void = std::ptr::null_mut();
            let mut alloc: *mut c_void = std::ptr::null_mut();
            let mut fence: *mut c_void = std::ptr::null_mut();
            // IID_ID3D12CommandQueue = {0ec870a6-5d7e-4c22-8cfc-5baae07616ed}
            let iid_queue: [u8; 16] = [
                0xA6, 0x70, 0xC8, 0x0E, 0x7E, 0x5D, 0x22, 0x4C,
                0x8C, 0xFC, 0x5B, 0xAA, 0xE0, 0x76, 0x16, 0xED,
            ];
            // IID_ID3D12CommandAllocator = {6102dee4-af59-4b09-b999-b44d73f09b24}
            let iid_alloc: [u8; 16] = [
                0xE4, 0xDE, 0x02, 0x61, 0x59, 0xAF, 0x09, 0x4B,
                0xB9, 0x99, 0xB4, 0x4D, 0x73, 0xF0, 0x9B, 0x24,
            ];
            // IID_ID3D12GraphicsCommandList = {5b160d0f-ac1b-4185-8ba8-b3ae42a5a455}
            let iid_cmdlist: [u8; 16] = [
                0x0F, 0x0D, 0x16, 0x5B, 0x1B, 0xAC, 0x85, 0x41,
                0x8B, 0xA8, 0xB3, 0xAE, 0x42, 0xA5, 0xA4, 0x55,
            ];
            // IID_ID3D12Fence = {0a753dcf-c4d8-4b91-adf6-be5a60d95a76}
            let iid_fence: [u8; 16] = [
                0xCF, 0x3D, 0x75, 0x0A, 0xD8, 0xC4, 0x91, 0x4B,
                0xAD, 0xF6, 0xBE, 0x5A, 0x60, 0xD9, 0x5A, 0x76,
            ];
            #[repr(C)]
            struct QueueDesc {
                kind: u32,
                priority: i32,
                flags: u32,
                node_mask: u32,
            }
            let qdesc = QueueDesc { kind: 0, priority: 0, flags: 0, node_mask: 0 };
            assert_eq!(
                ((*(*dev).vtbl).CreateCommandQueue)(
                    dev,
                    &qdesc as *const QueueDesc as u64,
                    iid_queue.as_ptr() as u64,
                    &mut queue_ptr as *mut *mut c_void as u64,
                ),
                S_OK
            );
            let queue = queue_ptr.cast::<D3D12CmdQueueX>();
            assert!(!queue.is_null());

            if (*dev).has_desktop_device() {
                assert!(!(*queue).inner_desktop_queue.is_null());
                let hr_alloc = ((*(*dev).vtbl).CreateCommandAllocator)(
                    dev,
                    0, // DIRECT
                    iid_alloc.as_ptr() as u64,
                    &mut alloc as *mut *mut c_void as u64,
                );
                assert_eq!(hr_alloc, S_OK);
                assert!(!alloc.is_null());

                let hr_fence = ((*(*dev).vtbl).CreateFence)(
                    dev,
                    0,
                    0,
                    iid_fence.as_ptr() as u64,
                    &mut fence as *mut *mut c_void as u64,
                );
                assert_eq!(hr_fence, S_OK);
                assert!(!fence.is_null());
            }

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

            // Explicit Xbox slot 49 ABI dispatch
            assert_eq!(((*(*dev).vtbl).ConfigureXboxDeviceSlot49X)(dev, 0x11f, 1), S_OK);
            assert_eq!(*(*dev).slot49_calls.lock().unwrap(), vec![(0x11f, 1)]);
            assert_eq!(((*(*dev).vtbl).XboxReserved50)(dev), E_NOTIMPL);

            let mut cmd_ptr: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                ((*(*dev).vtbl).CreateCommandList)(
                    dev,
                    0,
                    0,
                    alloc,
                    std::ptr::null_mut(),
                    if alloc.is_null() { std::ptr::null() } else { iid_cmdlist.as_ptr() },
                    &mut cmd_ptr
                ),
                S_OK
            );
            let cmd = cmd_ptr.cast::<D3D12CmdListX>();
            if !alloc.is_null() {
                assert!(!(*cmd).inner_desktop_cmd.is_null());
            }

            // Close -> ExecuteCommandLists (unwraps D3D12CmdListX to real desktop GPU!) -> Signal -> PresentX
            assert_eq!(((*(*cmd).vtbl).Close)(cmd), S_OK);
            let list_arr = [cmd_ptr];
            ((*(*queue).vtbl).ExecuteCommandLists)(queue, 1, list_arr.as_ptr());
            assert_eq!((*queue).executed_batches.load(Ordering::Relaxed), 1);
            assert_eq!((*queue).executed_lists.load(Ordering::Relaxed), 1);
            if !fence.is_null() {
                assert_eq!(((*(*queue).vtbl).Signal)(queue, fence, 1), S_OK);
            }
            assert_eq!(((*(*queue).vtbl).PresentX)(queue, 1, std::ptr::null(), std::ptr::null()), S_OK);
            assert_eq!((*queue).presents.load(Ordering::Relaxed), 1);
            assert_eq!(((*(*queue).vtbl).QueueExt20)(queue), E_NOTIMPL);

            if !alloc.is_null() {
                assert_eq!(((*(*cmd).vtbl).Reset)(cmd, alloc, std::ptr::null_mut()), S_OK);
            }
            ((*(*cmd).vtbl).BeginQuery)(cmd, std::ptr::null_mut(), 0, 0);
            ((*(*cmd).vtbl).BeginQuery)(cmd, std::ptr::null_mut(), 0, 1);
            ((*(*cmd).vtbl).DispatchMesh)(cmd, 16, 1, 1);
            ((*(*cmd).vtbl).EndQuery)(cmd, std::ptr::null_mut(), 0, 1);
            ((*(*cmd).vtbl).EndQuery)(cmd, std::ptr::null_mut(), 0, 0);
            assert_eq!((*cmd).mesh_dispatches.load(Ordering::Relaxed), 1);
            assert_eq!((*cmd).inner_translator.lock().unwrap().occlusion.unnested_count, 1);
            assert_eq!(((*(*cmd).vtbl).CmdExt61)(cmd), E_NOTIMPL);

            assert_eq!(((*(*cmd).vtbl).Release)(cmd), 0);
            if !fence.is_null() {
                release_com_object(fence);
            }
            if !alloc.is_null() {
                release_com_object(alloc);
            }
            assert_eq!(((*(*queue).vtbl).Release)(queue), 0);
            assert_eq!(((*(*dev).vtbl).Release)(dev), 0);
        }
    }
}
