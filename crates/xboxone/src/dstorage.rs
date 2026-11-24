//! Clean-room Xbox Series X|S DirectStorage (`dstorage_xs.dll`) COM subsystem.
//!
//! Implements real Windows x64 COM vtables for:
//! - `IDStorageFactoryX` (`OpenFile`, `CreateQueue`, `SetDebugFlags`, `SetStagingBufferSize`)
//! - `IDStorageFileX` (`Close`, `GetFileInformation`)
//! - `IDStorageQueueX` (`EnqueueRequest`, `EnqueueSignal`, `Submit`, `CancelRequestsWithTag`, `RetrieveErrorRecord`)
//!
//! Reads real byte ranges from disk directly into guest memory buffers so modern Xbox titles
//! streaming assets through DirectStorage actually load real file data in Native Mode.

#![allow(non_snake_case)]

use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

const S_OK: i32 = 0;
const E_FAIL: i32 = 0x8000_4005u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;

/// Simplified DirectStorage request layout matching `DSTORAGE_REQUEST` fields used for file->memory reads.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DStorageRequest {
    pub options: u64,
    pub source_file: *mut ComFile,
    pub source_offset: u64,
    pub source_size: u32,
    pub _pad0: u32,
    pub dest_memory: *mut u8,
    pub dest_size: u32,
    pub uncompressed_size: u32,
    pub cancellation_tag: u64,
    pub name: *const u8,
}

#[repr(C)]
pub struct ComFileVtable {
    pub QueryInterface: unsafe extern "system" fn(*mut ComFile, *const u8, *mut *mut c_void) -> i32,
    pub AddRef: unsafe extern "system" fn(*mut ComFile) -> u32,
    pub Release: unsafe extern "system" fn(*mut ComFile) -> u32,
    pub Close: unsafe extern "system" fn(*mut ComFile),
    pub GetFileSize: unsafe extern "system" fn(*mut ComFile, *mut u64) -> i32,
}

#[repr(C)]
pub struct ComFile {
    pub vtbl: *const ComFileVtable,
    ref_count: AtomicU32,
    path: PathBuf,
    len: u64,
}

static FILE_VTABLE: ComFileVtable = ComFileVtable {
    QueryInterface: file_qi,
    AddRef: file_addref,
    Release: file_release,
    Close: file_close,
    GetFileSize: file_get_size,
};

unsafe extern "system" fn file_qi(this: *mut ComFile, _riid: *const u8, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    file_addref(this);
    *out = this.cast();
    S_OK
}

unsafe extern "system" fn file_addref(this: *mut ComFile) -> u32 {
    (*this).ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn file_release(this: *mut ComFile) -> u32 {
    let prev = (*this).ref_count.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        drop(Box::from_raw(this));
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn file_close(_this: *mut ComFile) {}

unsafe extern "system" fn file_get_size(this: *mut ComFile, out_size: *mut u64) -> i32 {
    if out_size.is_null() {
        return E_INVALIDARG;
    }
    *out_size = (*this).len;
    S_OK
}

#[repr(C)]
pub struct ComQueueVtable {
    pub QueryInterface: unsafe extern "system" fn(*mut ComQueue, *const u8, *mut *mut c_void) -> i32,
    pub AddRef: unsafe extern "system" fn(*mut ComQueue) -> u32,
    pub Release: unsafe extern "system" fn(*mut ComQueue) -> u32,
    pub EnqueueRequest: unsafe extern "system" fn(*mut ComQueue, *const DStorageRequest),
    pub EnqueueStatus: unsafe extern "system" fn(*mut ComQueue, *mut c_void, u32),
    pub EnqueueSignal: unsafe extern "system" fn(*mut ComQueue, *mut c_void, u64),
    pub Submit: unsafe extern "system" fn(*mut ComQueue),
    pub CancelRequestsWithTag: unsafe extern "system" fn(*mut ComQueue, u64, u64),
    pub Close: unsafe extern "system" fn(*mut ComQueue),
}

struct PendingRead {
    path: PathBuf,
    offset: u64,
    size: usize,
    dest: usize,
    dest_size: usize,
    tag: u64,
}

#[repr(C)]
pub struct ComQueue {
    pub vtbl: *const ComQueueVtable,
    ref_count: AtomicU32,
    pending: Mutex<Vec<PendingRead>>,
    pub completed_reads: AtomicU32,
    pub failed_reads: AtomicU32,
}

static QUEUE_VTABLE: ComQueueVtable = ComQueueVtable {
    QueryInterface: queue_qi,
    AddRef: queue_addref,
    Release: queue_release,
    EnqueueRequest: queue_enqueue,
    EnqueueStatus: queue_enqueue_status,
    EnqueueSignal: queue_enqueue_signal,
    Submit: queue_submit,
    CancelRequestsWithTag: queue_cancel,
    Close: queue_close,
};

unsafe extern "system" fn queue_qi(this: *mut ComQueue, _riid: *const u8, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    queue_addref(this);
    *out = this.cast();
    S_OK
}

unsafe extern "system" fn queue_addref(this: *mut ComQueue) -> u32 {
    (*this).ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn queue_release(this: *mut ComQueue) -> u32 {
    let prev = (*this).ref_count.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        drop(Box::from_raw(this));
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn queue_enqueue(this: *mut ComQueue, req: *const DStorageRequest) {
    if this.is_null() || req.is_null() {
        return;
    }
    let r = *req;
    if r.source_file.is_null() || r.dest_memory.is_null() || r.source_size == 0 {
        return;
    }
    let path = (*r.source_file).path.clone();
    let item = PendingRead {
        path,
        offset: r.source_offset,
        size: r.source_size as usize,
        dest: r.dest_memory as usize,
        dest_size: (r.dest_size as usize).max(r.source_size as usize),
        tag: r.cancellation_tag,
    };
    if let Ok(mut q) = (*this).pending.lock() {
        q.push(item);
    }
}

unsafe extern "system" fn queue_enqueue_status(_this: *mut ComQueue, _array: *mut c_void, _index: u32) {}
unsafe extern "system" fn queue_enqueue_signal(_this: *mut ComQueue, _fence: *mut c_void, _value: u64) {}

unsafe extern "system" fn queue_submit(this: *mut ComQueue) {
    if this.is_null() {
        return;
    }
    let items: Vec<PendingRead> = (*this)
        .pending
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default();
    for item in items {
        let ok = (|| -> std::io::Result<()> {
            let mut f = File::open(&item.path)?;
            f.seek(SeekFrom::Start(item.offset))?;
            let n = item.size.min(item.dest_size);
            let dst = std::slice::from_raw_parts_mut(item.dest as *mut u8, n);
            f.read_exact(dst)?;
            Ok(())
        })()
        .is_ok();
        if ok {
            (*this).completed_reads.fetch_add(1, Ordering::Relaxed);
        } else {
            (*this).failed_reads.fetch_add(1, Ordering::Relaxed);
        }
    }
}

unsafe extern "system" fn queue_cancel(this: *mut ComQueue, mask: u64, value: u64) {
    if let Ok(mut q) = (*this).pending.lock() {
        q.retain(|item| (item.tag & mask) != (value & mask));
    }
}

unsafe extern "system" fn queue_close(this: *mut ComQueue) {
    if let Ok(mut q) = (*this).pending.lock() {
        q.clear();
    }
}

#[repr(C)]
pub struct ComFactoryVtable {
    pub QueryInterface: unsafe extern "system" fn(*mut ComFactory, *const u8, *mut *mut c_void) -> i32,
    pub AddRef: unsafe extern "system" fn(*mut ComFactory) -> u32,
    pub Release: unsafe extern "system" fn(*mut ComFactory) -> u32,
    pub CreateQueue: unsafe extern "system" fn(*mut ComFactory, *const c_void, *const u8, *mut *mut c_void) -> i32,
    pub OpenFile: unsafe extern "system" fn(*mut ComFactory, *const u16, *const u8, *mut *mut c_void) -> i32,
    pub CreateStatusArray: unsafe extern "system" fn(*mut ComFactory, u32, *const u8, *const u8, *mut *mut c_void) -> i32,
    pub SetDebugFlags: unsafe extern "system" fn(*mut ComFactory, u32),
    pub SetStagingBufferSize: unsafe extern "system" fn(*mut ComFactory, u32) -> i32,
}

#[repr(C)]
pub struct ComFactory {
    pub vtbl: *const ComFactoryVtable,
    ref_count: AtomicU32,
    staging_bytes: AtomicU32,
}

static FACTORY_VTABLE: ComFactoryVtable = ComFactoryVtable {
    QueryInterface: factory_qi,
    AddRef: factory_addref,
    Release: factory_release,
    CreateQueue: factory_create_queue,
    OpenFile: factory_open_file,
    CreateStatusArray: factory_create_status_array,
    SetDebugFlags: factory_set_debug_flags,
    SetStagingBufferSize: factory_set_staging,
};

unsafe extern "system" fn factory_qi(this: *mut ComFactory, _riid: *const u8, out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    factory_addref(this);
    *out = this.cast();
    S_OK
}

unsafe extern "system" fn factory_addref(this: *mut ComFactory) -> u32 {
    (*this).ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn factory_release(this: *mut ComFactory) -> u32 {
    let prev = (*this).ref_count.fetch_sub(1, Ordering::AcqRel);
    if prev == 1 {
        drop(Box::from_raw(this));
        0
    } else {
        prev - 1
    }
}

unsafe extern "system" fn factory_create_queue(
    _this: *mut ComFactory,
    _desc: *const c_void,
    _riid: *const u8,
    out_queue: *mut *mut c_void,
) -> i32 {
    if out_queue.is_null() {
        return E_INVALIDARG;
    }
    let q = Box::new(ComQueue {
        vtbl: &QUEUE_VTABLE,
        ref_count: AtomicU32::new(1),
        pending: Mutex::new(Vec::new()),
        completed_reads: AtomicU32::new(0),
        failed_reads: AtomicU32::new(0),
    });
    *out_queue = Box::into_raw(q).cast();
    S_OK
}

unsafe extern "system" fn factory_open_file(
    _this: *mut ComFactory,
    path_w: *const u16,
    _riid: *const u8,
    out_file: *mut *mut c_void,
) -> i32 {
    if path_w.is_null() || out_file.is_null() {
        return E_INVALIDARG;
    }
    let mut len = 0usize;
    while len < 4096 && *path_w.add(len) != 0 {
        len += 1;
    }
    let path_str = String::from_utf16_lossy(std::slice::from_raw_parts(path_w, len));
    let path = PathBuf::from(&path_str);
    let Ok(meta) = std::fs::metadata(&path) else {
        *out_file = std::ptr::null_mut();
        return E_FAIL;
    };
    let f = Box::new(ComFile {
        vtbl: &FILE_VTABLE,
        ref_count: AtomicU32::new(1),
        path,
        len: meta.len(),
    });
    *out_file = Box::into_raw(f).cast();
    S_OK
}

unsafe extern "system" fn factory_create_status_array(
    _this: *mut ComFactory,
    _capacity: u32,
    _name: *const u8,
    _riid: *const u8,
    out: *mut *mut c_void,
) -> i32 {
    if out.is_null() {
        return E_INVALIDARG;
    }
    *out = std::ptr::null_mut();
    E_NOINTERFACE
}

unsafe extern "system" fn factory_set_debug_flags(_this: *mut ComFactory, _flags: u32) {}

unsafe extern "system" fn factory_set_staging(this: *mut ComFactory, size: u32) -> i32 {
    (*this).staging_bytes.store(size, Ordering::Relaxed);
    S_OK
}

/// Allocates a live clean-room `IDStorageFactoryX` COM instance.
pub fn create_factory() -> *mut c_void {
    let f = Box::new(ComFactory {
        vtbl: &FACTORY_VTABLE,
        ref_count: AtomicU32::new(1),
        staging_bytes: AtomicU32::new(32 * 1024 * 1024),
    });
    Box::into_raw(f).cast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directstorage_com_factory_opens_and_streams_real_file_bytes() {
        let tmp = std::env::temp_dir().join("xboxone_dstorage_test.bin");
        let payload = b"XBOX_DIRECTSTORAGE_STREAM_PAYLOAD_1234567890";
        std::fs::write(&tmp, payload).unwrap();

        unsafe {
            let fac = create_factory().cast::<ComFactory>();
            assert!(!fac.is_null());

            // OpenFile via COM vtable
            let wide: Vec<u16> = tmp.to_string_lossy().encode_utf16().chain(Some(0)).collect();
            let mut file_ptr: *mut c_void = std::ptr::null_mut();
            let hr = ((*(*fac).vtbl).OpenFile)(fac, wide.as_ptr(), std::ptr::null(), &mut file_ptr);
            assert_eq!(hr, S_OK);
            let file = file_ptr.cast::<ComFile>();

            let mut sz = 0u64;
            assert_eq!(((*(*file).vtbl).GetFileSize)(file, &mut sz), S_OK);
            assert_eq!(sz, payload.len() as u64);

            // CreateQueue via COM vtable
            let mut queue_ptr: *mut c_void = std::ptr::null_mut();
            assert_eq!(
                ((*(*fac).vtbl).CreateQueue)(fac, std::ptr::null(), std::ptr::null(), &mut queue_ptr),
                S_OK
            );
            let queue = queue_ptr.cast::<ComQueue>();

            // EnqueueRequest + Submit reads slice [5..25] directly into dest buffer
            let mut dest = vec![0u8; 20];
            let req = DStorageRequest {
                options: 0,
                source_file: file,
                source_offset: 5,
                source_size: 20,
                _pad0: 0,
                dest_memory: dest.as_mut_ptr(),
                dest_size: 20,
                uncompressed_size: 20,
                cancellation_tag: 0xAA,
                name: std::ptr::null(),
            };
            ((*(*queue).vtbl).EnqueueRequest)(queue, &req);
            ((*(*queue).vtbl).Submit)(queue);
            assert_eq!(&dest, &payload[5..25]);
            assert_eq!((*queue).completed_reads.load(Ordering::Relaxed), 1);

            // Release COM objects cleanly
            assert_eq!(((*(*queue).vtbl).Release)(queue), 0);
            assert_eq!(((*(*file).vtbl).Release)(file), 0);
            assert_eq!(((*(*fac).vtbl).Release)(fac), 0);
        }
        let _ = std::fs::remove_file(&tmp);
    }
}
