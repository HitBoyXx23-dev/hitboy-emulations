//! `malloc` and friends for C++ code compiled to WebAssembly, backed by Rust's allocator.
//! Each block carries a 16-byte header with its size so `free` and `realloc` can find it.

#[cfg(target_arch = "wasm32")]
mod imp {
    use std::alloc::{alloc, alloc_zeroed, dealloc, realloc as rs_realloc, Layout};

    const HEADER: usize = 16;

    #[no_mangle]
    pub unsafe extern "C" fn malloc(size: usize) -> *mut u8 {
        let total = size + HEADER;
        let p = alloc(Layout::from_size_align_unchecked(total, 16));
        if p.is_null() {
            return p;
        }
        *(p as *mut usize) = total;
        p.add(HEADER)
    }

    #[no_mangle]
    pub unsafe extern "C" fn calloc(n: usize, size: usize) -> *mut u8 {
        let total = n * size + HEADER;
        let p = alloc_zeroed(Layout::from_size_align_unchecked(total, 16));
        if p.is_null() {
            return p;
        }
        *(p as *mut usize) = total;
        p.add(HEADER)
    }

    #[no_mangle]
    pub unsafe extern "C" fn free(p: *mut u8) {
        if p.is_null() {
            return;
        }
        let base = p.sub(HEADER);
        let total = *(base as *mut usize);
        dealloc(base, Layout::from_size_align_unchecked(total, 16));
    }

    #[no_mangle]
    pub unsafe extern "C" fn realloc(p: *mut u8, size: usize) -> *mut u8 {
        if p.is_null() {
            return malloc(size);
        }
        let base = p.sub(HEADER);
        let total = *(base as *mut usize);
        let new_total = size + HEADER;
        let q = rs_realloc(base, Layout::from_size_align_unchecked(total, 16), new_total);
        if q.is_null() {
            return q;
        }
        *(q as *mut usize) = new_total;
        q.add(HEADER)
    }

    #[no_mangle]
    pub extern "C" fn abort() -> ! {
        std::process::abort()
    }
}

/// Call once so the linker keeps the exported symbols.
pub fn init() {}
