use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::c_void;
use std::sync::OnceLock;

use crate::era_mem::PAGE_SIZE_64K;

const PAGE_READWRITE: u32 = 4;
const PAGE_BYTES: usize = PAGE_SIZE_64K as usize;
const MEM_RELEASE: u32 = 0x8000;
const MEM_REPLACE_PLACEHOLDER: u32 = 0x4000;
const MEM_PRESERVE_PLACEHOLDER: u32 = 2;
const MAX_PAGES: usize = (16 * 1024 * 1024 * 1024u64 / PAGE_SIZE_64K) as usize;

#[link(name = "kernel32")]
extern "system" {
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetModuleHandleA(name: *const u8) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn VirtualFree(address: *mut c_void, size: usize, flags: u32) -> i32;
    fn CreateFileMappingW(
        file: *mut c_void,
        security_attributes: *mut c_void,
        protection: u32,
        size_high: u32,
        size_low: u32,
        name: *const u16,
    ) -> *mut c_void;
}

type MapView = unsafe extern "system" fn(
    *mut c_void, *mut c_void, *mut c_void, u64, usize, u32, u32, *mut c_void, u32,
) -> *mut c_void;
type UnmapView = unsafe extern "system" fn(*mut c_void, *mut c_void, u32) -> i32;

struct ViewApi {
    map: MapView,
    unmap: UnmapView,
}

fn view_api() -> Option<&'static ViewApi> {
    static API: OnceLock<Option<ViewApi>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let module = GetModuleHandleA(b"kernelbase.dll\0".as_ptr());
        if module.is_null() {
            return None;
        }
        let map = GetProcAddress(module, b"MapViewOfFile3\0".as_ptr());
        let unmap = GetProcAddress(module, b"UnmapViewOfFile2\0".as_ptr());
        if map.is_null() || unmap.is_null() {
            return None;
        }
        Some(ViewApi {
            map: std::mem::transmute::<*mut c_void, MapView>(map),
            unmap: std::mem::transmute::<*mut c_void, UnmapView>(unmap),
        })
    }).as_ref()
}

struct Page(usize);

impl Page {
    fn allocate() -> Option<Self> {
        let handle = unsafe {
            CreateFileMappingW(
                usize::MAX as *mut c_void,
                std::ptr::null_mut(),
                PAGE_READWRITE,
                0,
                PAGE_SIZE_64K as u32,
                std::ptr::null(),
            )
        };
        (!handle.is_null()).then(|| Self(handle as usize))
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0 as *mut c_void) };
    }
}

#[derive(Default)]
pub(crate) struct PhysicalPages {
    pages: HashMap<usize, Page>,
    windows: BTreeMap<usize, usize>,
    views: HashMap<usize, usize>,
    last_id: usize,
}

impl PhysicalPages {
    fn allocate(&mut self, count: usize) -> Option<Vec<usize>> {
        let last_id = self.last_id.checked_add(count)?;
        if count > MAX_PAGES - self.pages.len() {
            return None;
        }
        self.pages.try_reserve(count).ok()?;
        let mut batch = Vec::new();
        batch.try_reserve_exact(count).ok()?;
        let mut ids = Vec::new();
        ids.try_reserve_exact(count).ok()?;
        for index in 0..count {
            batch.push(Page::allocate()?);
            ids.push(self.last_id + index + 1);
        }
        self.pages.extend(ids.iter().copied().zip(batch));
        self.last_id = last_id;
        Some(ids)
    }

    fn free(&mut self, ids: &[usize]) -> bool {
        let mut seen = HashSet::new();
        if !ids.iter().all(|id| self.pages.contains_key(id) && seen.insert(*id))
            || self.views.values().any(|id| seen.contains(id))
        {
            return false;
        }
        for id in ids {
            self.pages.remove(id);
        }
        true
    }

    pub(crate) fn register_window(&mut self, address: usize, size: usize) {
        self.windows.insert(address, address + size);
    }

    fn window_at(&self, address: usize) -> Option<(usize, usize)> {
        self.windows.range(..=address).next_back().and_then(|(&start, &end)| {
            (address < end).then_some((start, end))
        })
    }

    fn split_window(&mut self, start: usize, boundary: usize, end: usize) -> bool {
        if boundary == start || boundary == end {
            return true;
        }
        if unsafe { VirtualFree(start as *mut c_void, boundary - start, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER) } == 0 {
            return false;
        }
        self.windows.insert(start, boundary);
        self.windows.insert(boundary, end);
        true
    }

    pub(crate) fn carve_window(&mut self, address: usize, size: usize) -> bool {
        let Some((start, end)) = self.window_at(address) else { return false };
        let Some(limit) = address.checked_add(size).filter(|limit| *limit <= end) else { return false };
        self.split_window(start, address, end) && self.split_window(address, limit, end)
    }

    fn change_view(&mut self, api: &ViewApi, address: usize, id: usize) -> bool {
        let previous = self.views.get(&address).copied().unwrap_or(0);
        if previous == id {
            return true;
        }
        let process = usize::MAX as *mut c_void;
        if previous != 0 {
            if unsafe { (api.unmap)(process, address as *mut c_void, MEM_PRESERVE_PLACEHOLDER) } == 0 {
                return false;
            }
            self.views.remove(&address);
        }
        if id == 0 {
            return true;
        }
        if !self.carve_window(address, PAGE_BYTES) {
            return false;
        }
        let view = unsafe {
            (api.map)(self.pages[&id].0 as *mut c_void, process, address as *mut c_void, 0,
                PAGE_BYTES, MEM_REPLACE_PLACEHOLDER, PAGE_READWRITE, std::ptr::null_mut(), 0)
        };
        if view.is_null() {
            return false;
        }
        self.views.insert(address, id);
        true
    }

    fn map(&mut self, addresses: &[usize], ids: &[usize]) -> bool {
        if addresses.len() != ids.len() {
            return false;
        }
        let Some(api) = view_api() else { return false };
        let mut seen = HashSet::new();
        if !addresses.iter().zip(ids).all(|(&address, &id)| {
            address % PAGE_BYTES == 0 && seen.insert(address)
                && self.window_at(address).is_some_and(|(_, end)| end - address >= PAGE_BYTES)
                && (id == 0 || self.pages.contains_key(&id))
        }) {
            return false;
        }
        let mut prior = Vec::with_capacity(addresses.len());
        for (&address, &id) in addresses.iter().zip(ids) {
            prior.push((address, self.views.get(&address).copied().unwrap_or(0)));
            if !self.change_view(api, address, id) {
                for (address, previous) in prior.into_iter().rev() {
                    self.change_view(api, address, previous);
                }
                return false;
            }
        }
        true
    }

    fn release_window(&mut self, address: usize, size: usize, flags: u32) -> Option<i32> {
        let (start, _) = self.window_at(address)?;
        if start != address || size != 0 || flags != MEM_RELEASE || self.views.contains_key(&address) {
            return Some(0);
        }
        let result = unsafe { VirtualFree(address as *mut c_void, 0, MEM_RELEASE) };
        if result != 0 {
            self.windows.remove(&address);
        }
        Some(result)
    }
}

impl Drop for PhysicalPages {
    fn drop(&mut self) {
        if let Some(api) = view_api() {
            for &address in self.views.keys() {
                unsafe { (api.unmap)(usize::MAX as *mut c_void, address as *mut c_void, MEM_PRESERVE_PLACEHOLDER) };
            }
        }
        for &address in self.windows.keys() {
            unsafe { VirtualFree(address as *mut c_void, 0, MEM_RELEASE) };
        }
    }
}

pub unsafe extern "system" fn guest_virtual_free(address: *mut c_void, size: usize, flags: u32) -> i32 {
    crate::native::with_physical_pages(|pages| pages.release_window(address as usize, size, flags))
        .unwrap_or_else(|| unsafe { VirtualFree(address, size, flags) })
}

unsafe fn read_ids(count: usize, pointer: *const usize) -> Vec<usize> {
    (0..count).map(|index| unsafe { pointer.add(index).read_unaligned() }).collect()
}

unsafe fn update_views(address: usize, count: usize, ids: *const usize, scatter: bool) -> bool {
    if count == 0 {
        return true;
    }
    if count > MAX_PAGES || address == 0 {
        return false;
    }
    let addresses = if scatter {
        unsafe { read_ids(count, address as *const usize) }
    } else {
        let Some(end) = address.checked_add(count * PAGE_BYTES) else { return false };
        (address..end).step_by(PAGE_BYTES).collect()
    };
    let ids = if ids.is_null() { vec![0; count] } else { unsafe { read_ids(count, ids) } };
    crate::native::with_physical_pages(|pages| pages.map(&addresses, &ids))
}

pub unsafe extern "system" fn map_pages(address: usize, count: usize, ids: *const usize) -> usize {
    if unsafe { update_views(address, count, ids, false) } { address } else { 0 }
}

pub unsafe extern "system" fn map_pages_scatter(addresses: *const usize, count: u32, ids: *const usize) -> i32 {
    i32::from(unsafe { update_views(addresses as usize, count as usize, ids, true) })
}

pub unsafe extern "system" fn allocate_pages(
    _attributes: u64,
    count: *mut usize,
    output: *mut usize,
) -> i32 {
    if count.is_null() {
        return 0;
    }
    let requested = unsafe { count.read_unaligned() };
    if requested == 0 {
        return 1;
    }
    if output.is_null() {
        return 0;
    }
    let pages = crate::native::with_physical_pages(|pages| pages.allocate(requested));
    let Some(ids) = pages else {
        unsafe { count.write_unaligned(0) };
        return 0;
    };
    for (index, id) in ids.into_iter().enumerate() {
        unsafe { output.add(index).write_unaligned(id) };
    }
    1
}

pub unsafe extern "system" fn free_pages(count: usize, ids: *const usize) -> i32 {
    if count == 0 {
        return 1;
    }
    if count > MAX_PAGES || ids.is_null() {
        return 0;
    }
    let ids = unsafe { read_ids(count, ids) };
    i32::from(crate::native::with_physical_pages(|pages| pages.free(&ids)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[link(name = "kernel32")]
    extern "system" {
        fn MapViewOfFile(mapping: *mut c_void, access: u32, offset_high: u32, offset_low: u32, bytes: usize) -> *mut c_void;
        fn UnmapViewOfFile(base: *const c_void) -> i32;
    }

    #[test]
    fn allocates_zeroed_windows_backing_and_rejects_invalid_free_batches() {
        let mut pages = PhysicalPages::default();
        assert!(pages.allocate(MAX_PAGES + 1).is_none());
        let ids = pages.allocate(2).unwrap();
        assert_ne!(ids[0], ids[1]);
        let view = unsafe { MapViewOfFile(pages.pages[&ids[0]].0 as *mut c_void, 2, 0, 0, PAGE_SIZE_64K as usize) };
        assert!(!view.is_null());
        unsafe {
            let last_byte = view.cast::<u8>().add(PAGE_SIZE_64K as usize - 1);
            assert_eq!(last_byte.read(), 0);
            last_byte.write(42);
            assert_eq!(last_byte.read(), 42);
            assert_ne!(UnmapViewOfFile(view), 0);
        }
        assert!(!pages.free(&[ids[0], ids[0]]));
        assert!(!pages.free(&[ids[0], usize::MAX]));
        assert!(pages.free(&ids));
        assert!(pages.pages.is_empty());
        assert!(!pages.free(&ids));
    }

    #[test]
    fn maps_contiguous_and_scattered_aliases_into_a_reserved_window() {
        let mut pages = PhysicalPages::default();
        let ids = pages.allocate(2).unwrap();
        let start = crate::shims::XMemVirtualAlloc(0, 4 * PAGE_BYTES, 0x42000, 7, 1) as usize;
        assert_ne!(start, 0);
        pages.register_window(start, 4 * PAGE_BYTES);
        let middle = start + PAGE_BYTES;
        let last = start + 3 * PAGE_BYTES;
        assert!(pages.carve_window(middle, 2 * PAGE_BYTES));
        assert!(pages.map(&[middle, middle + PAGE_BYTES], &ids));
        assert!(!pages.map(&[start, last], &[ids[0], usize::MAX]));
        assert!(!pages.views.contains_key(&start));
        assert!(pages.map(&[last, start], &ids));
        assert!(!pages.free(&ids));
        unsafe {
            ((middle + PAGE_BYTES - 1) as *mut u8).write(42);
            assert_eq!(((last + PAGE_BYTES - 1) as *const u8).read(), 42);
            (start as *mut u8).write(17);
            assert_eq!(((middle + PAGE_BYTES) as *const u8).read(), 17);
        }
        assert!(pages.map(&[last, start], &[0, 0]));
        assert_eq!(pages.release_window(last, 0, MEM_RELEASE), Some(1));
        assert!(pages.map(&[middle, middle + PAGE_BYTES], &[0, 0]));
        assert!(pages.free(&ids));
    }
}
