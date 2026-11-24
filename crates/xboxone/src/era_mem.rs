//! Xbox One / Series X|S memory architecture: Title Virtual Address (VA) management,
//! GPU-visible alias windows, aligned XMem heap pools, and TitleMemoryStatus reporting.

use std::collections::{BTreeMap, BTreeSet};

/// Page granularity for Era / Scarlett virtual memory operations (64 KiB large-block / 4 KiB base).
pub const PAGE_SIZE_4K: u64 = 0x1000;
pub const PAGE_SIZE_64K: u64 = 0x10000;

/// Realistic Series X title-accessible memory budget (13.5 GiB).
pub const SERIES_X_TITLE_RAM_BYTES: u64 = 13_500 * 1024 * 1024;
/// Realistic Xbox One title-accessible memory budget (5.0 GiB).
pub const XBOX_ONE_TITLE_RAM_BYTES: u64 = 5_120 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryProtect {
    NoAccess,
    ReadOnly,
    ReadWrite,
    ExecuteRead,
    ExecuteReadWrite,
    GpuCoherentReadWrite,
}

impl MemoryProtect {
    pub fn from_win32(flags: u32) -> Self {
        match flags & 0xFF {
            0x01 => Self::NoAccess,
            0x02 => Self::ReadOnly,
            0x04 | 0x08 => Self::ReadWrite,
            0x10 | 0x20 => Self::ExecuteRead,
            0x40 | 0x80 => Self::ExecuteReadWrite,
            _ => Self::ReadWrite,
        }
    }

    pub fn is_readable(self) -> bool {
        !matches!(self, Self::NoAccess)
    }

    pub fn is_writable(self) -> bool {
        matches!(self, Self::ReadWrite | Self::ExecuteReadWrite | Self::GpuCoherentReadWrite)
    }

    pub fn is_executable(self) -> bool {
        matches!(self, Self::ExecuteRead | Self::ExecuteReadWrite)
    }
}

/// One reserved/committed virtual region inside the title's 64-bit address space.
#[derive(Debug, Clone)]
pub struct VaRegion {
    pub base: u64,
    pub size: u64,
    pub committed: bool,
    pub protect: MemoryProtect,
    pub gpu_visible: bool,
    pub data: Vec<u8>,
    placeholder: bool,
    physical_window: bool,
}

/// GPU alias mapping: maps a GPU-visible address range onto a title VA backing region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuAliasWindow {
    pub gpu_va: u64,
    pub title_va: u64,
    pub size: u64,
}

/// Snapshot returned by `TitleMemoryStatus` (matches the 64-byte Xbox XDK/GDK struct layout).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TitleMemoryStatus {
    pub length: u32,
    pub reserved: u32,
    pub total_mem: u64,
    pub avail_mem: u64,
    pub legacy_used: u64,
    pub legacy_peak: u64,
    pub legacy_avail: u64,
    pub title_used: u64,
    pub title_avail: u64,
}

/// Sandboxed 64-bit virtual address space for an Xbox One / Series X|S title.
#[derive(Debug, Clone)]
pub struct TitleAddressSpace {
    regions: BTreeMap<u64, VaRegion>,
    gpu_aliases: Vec<GpuAliasWindow>,
    physical_pages: BTreeMap<u64, Vec<u8>>,
    physical_mappings: BTreeMap<u64, u64>,
    next_physical_page: u64,
    next_alloc_va: u64,
    next_gpu_va: u64,
    total_budget: u64,
    peak_committed: u64,
}

impl Default for TitleAddressSpace {
    fn default() -> Self {
        Self::new(SERIES_X_TITLE_RAM_BYTES)
    }
}

impl TitleAddressSpace {
    pub fn new(total_budget: u64) -> Self {
        Self {
            regions: BTreeMap::new(),
            gpu_aliases: Vec::new(),
            physical_pages: BTreeMap::new(),
            physical_mappings: BTreeMap::new(),
            next_physical_page: 16,
            next_alloc_va: 0x0000_0001_0000_0000,
            next_gpu_va: 0x0000_0100_0000_0000,
            total_budget,
            peak_committed: 0,
        }
    }

    pub fn align_up(value: u64, align: u64) -> u64 {
        let mask = align.max(1) - 1;
        (value.saturating_add(mask)) & !mask
    }

    /// Reserves and optionally commits a virtual region. Returns the chosen base VA.
    pub fn alloc(&mut self, preferred: Option<u64>, size: u64, align: u64, protect: MemoryProtect, commit: bool) -> Result<u64, String> {
        let align = align.max(PAGE_SIZE_4K);
        if !align.is_power_of_two() {
            return Err("alignment must be a power of two".into());
        }
        let checked_align = |value: u64, alignment: u64| {
            value.checked_add(alignment - 1).map(|v| v & !(alignment - 1))
                .ok_or("Title address range overflow")
        };
        let rounded = checked_align(size.max(PAGE_SIZE_4K), PAGE_SIZE_4K)?;
        let base = match preferred {
            Some(addr) if addr != 0 => {
                let aligned = addr & !(PAGE_SIZE_4K - 1);
                let end = aligned.checked_add(rounded).ok_or("Title address range overflow")?;
                if self.overlaps(aligned, rounded) {
                    return Err(format!("VA range {aligned:#x}..{end:#x} overlaps an existing region"));
                }
                aligned
            }
            _ => {
                let mut cand = checked_align(self.next_alloc_va, align)?;
                while self.overlaps(cand, rounded) {
                    let next = cand.checked_add(rounded).ok_or("Title address range overflow")?;
                    cand = checked_align(next, align)?;
                }
                let next = cand.checked_add(rounded).ok_or("Title address range overflow")?;
                self.next_alloc_va = checked_align(next, PAGE_SIZE_64K)?;
                cand
            }
        };
        let data = if commit { vec![0u8; rounded as usize] } else { Vec::new() };
        let gpu_visible = matches!(protect, MemoryProtect::GpuCoherentReadWrite);
        self.regions.insert(
            base,
            VaRegion {
                base,
                size: rounded,
                committed: commit,
                protect,
                gpu_visible,
                data,
                placeholder: false,
                physical_window: false,
            },
        );
        self.update_peak();
        Ok(base)
    }

    pub fn reserve_placeholder(
        &mut self,
        preferred: Option<u64>,
        size: u64,
        align: u64,
    ) -> Result<u64, String> {
        if size == 0 || size % PAGE_SIZE_64K != 0 || align < PAGE_SIZE_64K
            || preferred.is_some_and(|base| base % align != 0)
        {
            return Err("Placeholder reservations must use aligned 64 KiB blocks".into());
        }
        let base = self.alloc(preferred, size, align, MemoryProtect::NoAccess, false)?;
        self.regions.get_mut(&base).unwrap().placeholder = true;
        Ok(base)
    }

    pub fn replace_placeholder(
        &mut self,
        base: u64,
        size: u64,
        protect: MemoryProtect,
        commit: bool,
    ) -> Result<u64, String> {
        if base % PAGE_SIZE_64K != 0 || size == 0 || size % PAGE_SIZE_64K != 0 {
            return Err("Placeholder replacements must use complete 64 KiB blocks".into());
        }
        let end = base.checked_add(size).ok_or("Placeholder replacement range overflow")?;
        let region = self.find_region(base)
            .filter(|region| region.placeholder && end <= region.base + region.size)
            .ok_or("Replacement is outside a single placeholder")?;
        let original_base = region.base;
        let original_end = region.base + region.size;

        let mut data = Vec::new();
        if commit {
            if size > self.status().avail_mem {
                return Err("Replacement exceeds the title memory budget".into());
            }
            let capacity = usize::try_from(size).map_err(|_| "Replacement is too large")?;
            data.try_reserve_exact(capacity).map_err(|_| "Could not allocate replacement memory")?;
            data.resize(capacity, 0);
        }
        self.regions.remove(&original_base);
        for (start, stop) in [(original_base, base), (end, original_end)] {
            if start < stop {
                self.regions.insert(start, VaRegion {
                    base: start,
                    size: stop - start,
                    committed: false,
                    protect: MemoryProtect::NoAccess,
                    gpu_visible: false,
                    data: Vec::new(),
                    placeholder: true,
                    physical_window: false,
                });
            }
        }
        self.regions.insert(base, VaRegion {
            base,
            size,
            committed: commit,
            protect,
            gpu_visible: matches!(protect, MemoryProtect::GpuCoherentReadWrite),
            data,
            placeholder: false,
            physical_window: false,
        });
        self.update_peak();
        Ok(base)
    }

    /// Releases a region by its base address.
    pub fn free(&mut self, base: u64) -> bool {
        if let Some(region) = self.regions.remove(&base) {
            self.physical_mappings.retain(|address, _| *address < base || *address >= base + region.size);
            self.gpu_aliases.retain(|w| w.title_va != base);
            true
        } else {
            false
        }
    }

    pub(crate) fn mark_physical_window(&mut self, base: u64) {
        if let Some(region) = self.regions.get_mut(&base) {
            region.physical_window = true;
            if region.protect == MemoryProtect::NoAccess {
                region.protect = MemoryProtect::ReadWrite;
            }
        }
    }

    pub(crate) fn allocate_physical_pages(&mut self, count: u64) -> Option<Vec<u64>> {
        if count > self.status().avail_mem / PAGE_SIZE_64K {
            return None;
        }
        let end = self.next_physical_page.checked_add(count.checked_mul(16)?)?;
        let mut frames = Vec::new();
        frames.try_reserve_exact(count as usize).ok()?;
        for number in (self.next_physical_page..end).step_by(16) {
            let mut data = Vec::new();
            data.try_reserve_exact(PAGE_SIZE_64K as usize).ok()?;
            data.resize(PAGE_SIZE_64K as usize, 0);
            frames.push((number, data));
        }
        let numbers = frames.iter().map(|(number, _)| *number).collect();
        self.physical_pages.extend(frames);
        self.next_physical_page = end;
        self.update_peak();
        Some(numbers)
    }

    pub(crate) fn free_physical_pages(&mut self, pages: &[u64]) -> bool {
        let unique: BTreeSet<_> = pages.iter().copied().collect();
        if unique.len() != pages.len()
            || unique.iter().any(|page| !self.physical_pages.contains_key(page))
            || self.physical_mappings.values().any(|page| unique.contains(page))
        {
            return false;
        }
        for page in unique {
            self.physical_pages.remove(&page);
        }
        true
    }

    pub(crate) fn map_physical_pages(&mut self, mappings: &[(u64, u64)]) -> bool {
        let mut addresses = BTreeSet::new();
        for &(address, page) in mappings {
            if address % PAGE_SIZE_64K != 0
                || !addresses.insert(address)
                || (page != 0 && !self.physical_pages.contains_key(&page))
                || !self.find_region(address).is_some_and(|region| {
                    region.physical_window && address.checked_add(PAGE_SIZE_64K)
                        .is_some_and(|end| end <= region.base + region.size)
                })
            {
                return false;
            }
        }
        for &(address, page) in mappings {
            if page == 0 {
                self.physical_mappings.remove(&address);
            } else {
                self.physical_mappings.insert(address, page);
            }
        }
        true
    }

    fn physical_slices(&self, address: u64, length: usize, region_end: u64) -> Result<Vec<(u64, usize, usize)>, String> {
        let end = address.checked_add(length as u64).filter(|end| *end <= region_end)
            .ok_or("Physical memory access crosses its reserved window")?;
        let mut slices = Vec::new();
        let mut cursor = address;
        while cursor < end {
            let base = cursor & !(PAGE_SIZE_64K - 1);
            let offset = cursor - base;
            let bytes = (PAGE_SIZE_64K - offset).min(end - cursor);
            let page = self.physical_mappings.get(&base)
                .ok_or_else(|| format!("Unmapped physical page at {base:#x}"))?;
            slices.push((*page, offset as usize, bytes as usize));
            cursor += bytes;
        }
        Ok(slices)
    }

    pub(crate) fn can_write_bytes(&self, address: u64, length: u64) -> bool {
        let resolved = self.translate_gpu_va(address).unwrap_or(address);
        self.find_region(resolved).is_some_and(|region| {
            if !region.protect.is_writable() {
                return false;
            }
            if region.physical_window {
                return usize::try_from(length).ok().is_some_and(|length| {
                    self.physical_slices(resolved, length, region.base + region.size).is_ok()
                });
            }
            region.committed && resolved.checked_add(length)
                .is_some_and(|end| end <= region.base + region.size)
        })
    }

    /// Updates page protections on an existing region and returns the previous protection.
    pub fn protect(&mut self, addr: u64, new_protect: MemoryProtect) -> Result<MemoryProtect, String> {
        let (&base, _) = self
            .regions
            .range(..=addr)
            .next_back()
            .filter(|(_, r)| addr < r.base + r.size)
            .ok_or_else(|| format!("unmapped address {addr:#x}"))?;
        let r = self.regions.get_mut(&base).unwrap();
        let old = r.protect;
        r.protect = new_protect;
        Ok(old)
    }

    /// Creates a GPU-visible alias window backed by an existing committed title VA region.
    pub fn map_gpu_alias(&mut self, title_va: u64, size: u64) -> Result<GpuAliasWindow, String> {
        let region = self
            .find_region(title_va)
            .ok_or_else(|| format!("title VA {title_va:#x} is not mapped"))?;
        if !region.committed {
            return Err(format!("title VA {title_va:#x} is reserved but not committed"));
        }
        let end = title_va.checked_add(size).ok_or("GPU alias range overflow")?;
        if end > region.base + region.size {
            return Err(format!("GPU alias range {title_va:#x}..{end:#x} exceeds region bounds"));
        }
        let gpu_va = Self::align_up(self.next_gpu_va, PAGE_SIZE_64K);
        self.next_gpu_va = Self::align_up(gpu_va + size.max(PAGE_SIZE_4K), PAGE_SIZE_64K);
        let win = GpuAliasWindow { gpu_va, title_va, size };
        self.gpu_aliases.push(win);
        Ok(win)
    }

    /// Translates a GPU alias VA back to the backing title VA.
    pub fn translate_gpu_va(&self, gpu_va: u64) -> Option<u64> {
        self.gpu_aliases.iter().find_map(|w| {
            if gpu_va >= w.gpu_va && gpu_va < w.gpu_va + w.size {
                Some(w.title_va + (gpu_va - w.gpu_va))
            } else {
                None
            }
        })
    }

    pub fn read_bytes(&self, addr: u64, out: &mut [u8]) -> Result<(), String> {
        let resolved = self.translate_gpu_va(addr).unwrap_or(addr);
        let r = self
            .find_region(resolved)
            .ok_or_else(|| format!("memory read fault at {addr:#x}"))?;
        if r.physical_window {
            if !r.protect.is_readable() {
                return Err(format!("Unreadable physical memory at {addr:#x}"));
            }
            let mut destination = 0;
            for (page, offset, len) in self.physical_slices(resolved, out.len(), r.base + r.size)? {
                out[destination..destination + len].copy_from_slice(&self.physical_pages[&page][offset..offset + len]);
                destination += len;
            }
            return Ok(());
        }
        if !r.committed || !r.protect.is_readable() {
            return Err(format!("unreadable memory at {addr:#x}"));
        }
        let off = (resolved - r.base) as usize;
        let end = off.checked_add(out.len()).ok_or("read length overflow")?;
        if end > r.data.len() {
            return Err(format!("read at {addr:#x} (+{} bytes) crosses region boundary", out.len()));
        }
        out.copy_from_slice(&r.data[off..end]);
        Ok(())
    }

    pub fn write_bytes(&mut self, addr: u64, src: &[u8]) -> Result<(), String> {
        let resolved = self.translate_gpu_va(addr).unwrap_or(addr);
        let (&base, _) = self
            .regions
            .range(..=resolved)
            .next_back()
            .filter(|(_, r)| resolved < r.base + r.size)
            .ok_or_else(|| format!("memory write fault at {addr:#x}"))?;
        let region = &self.regions[&base];
        if region.physical_window {
            if !region.protect.is_writable() {
                return Err(format!("Non-writable physical memory at {addr:#x}"));
            }
            let mut source = 0;
            for (page, offset, len) in self.physical_slices(resolved, src.len(), base + region.size)? {
                self.physical_pages.get_mut(&page).unwrap()[offset..offset + len]
                    .copy_from_slice(&src[source..source + len]);
                source += len;
            }
            return Ok(());
        }
        let r = self.regions.get_mut(&base).unwrap();
        if !r.committed || !r.protect.is_writable() {
            return Err(format!("non-writable memory at {addr:#x}"));
        }
        let off = (resolved - r.base) as usize;
        let end = off.checked_add(src.len()).ok_or("write length overflow")?;
        if end > r.data.len() {
            return Err(format!("write at {addr:#x} (+{} bytes) crosses region boundary", src.len()));
        }
        r.data[off..end].copy_from_slice(src);
        Ok(())
    }

    /// Writes bytes ignoring page protection (used by the loader to populate `.text`/`.rdata`).
    pub fn load_bytes(&mut self, addr: u64, src: &[u8]) -> Result<(), String> {
        let (&base, _) = self
            .regions
            .range(..=addr)
            .next_back()
            .filter(|(_, r)| addr < r.base + r.size)
            .ok_or_else(|| format!("unmapped load target {addr:#x}"))?;
        let r = self.regions.get_mut(&base).unwrap();
        let off = (addr - r.base) as usize;
        let end = off.checked_add(src.len()).ok_or("load length overflow")?;
        if end > r.data.len() {
            return Err(format!("load at {addr:#x} (+{} bytes) exceeds region size", src.len()));
        }
        r.data[off..end].copy_from_slice(src);
        Ok(())
    }

    pub fn read_u8(&self, addr: u64) -> Result<u8, String> {
        let mut b = [0u8; 1];
        self.read_bytes(addr, &mut b)?;
        Ok(b[0])
    }

    pub fn read_u16(&self, addr: u64) -> Result<u16, String> {
        let mut b = [0u8; 2];
        self.read_bytes(addr, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    pub fn read_u32(&self, addr: u64) -> Result<u32, String> {
        let mut b = [0u8; 4];
        self.read_bytes(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    pub fn read_u64(&self, addr: u64) -> Result<u64, String> {
        let mut b = [0u8; 8];
        self.read_bytes(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    pub fn write_u8(&mut self, addr: u64, v: u8) -> Result<(), String> {
        self.write_bytes(addr, &[v])
    }

    pub fn write_u16(&mut self, addr: u64, v: u16) -> Result<(), String> {
        self.write_bytes(addr, &v.to_le_bytes())
    }

    pub fn write_u32(&mut self, addr: u64, v: u32) -> Result<(), String> {
        self.write_bytes(addr, &v.to_le_bytes())
    }

    pub fn write_u64(&mut self, addr: u64, v: u64) -> Result<(), String> {
        self.write_bytes(addr, &v.to_le_bytes())
    }

    pub fn read_cstr(&self, addr: u64, max_len: usize) -> Result<String, String> {
        let mut out = Vec::new();
        for i in 0..max_len {
            let b = self.read_u8(addr + i as u64)?;
            if b == 0 {
                break;
            }
            out.push(b);
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    pub fn committed_bytes(&self) -> u64 {
        self.regions.values().filter(|r| r.committed).map(|r| r.size).sum::<u64>()
            + self.physical_pages.len() as u64 * PAGE_SIZE_64K
    }

    pub fn status(&self) -> TitleMemoryStatus {
        let used = self.committed_bytes();
        let avail = self.total_budget.saturating_sub(used);
        TitleMemoryStatus {
            length: std::mem::size_of::<TitleMemoryStatus>() as u32,
            reserved: 0,
            total_mem: self.total_budget,
            avail_mem: avail,
            legacy_used: used,
            legacy_peak: self.peak_committed,
            legacy_avail: avail,
            title_used: used,
            title_avail: avail,
        }
    }

    fn overlaps(&self, start: u64, size: u64) -> bool {
        let end = start.saturating_add(size);
        self.regions.values().any(|r| start < r.base + r.size && r.base < end)
    }

    fn find_region(&self, addr: u64) -> Option<&VaRegion> {
        self.regions
            .range(..=addr)
            .next_back()
            .map(|(_, r)| r)
            .filter(|r| addr < r.base + r.size)
    }

    fn update_peak(&mut self) {
        let used = self.committed_bytes();
        if used > self.peak_committed {
            self.peak_committed = used;
        }
    }
}

/// Decodes the requested alignment in bytes from Xbox `XMemAlloc` attributes.
pub fn xmem_alignment_from_attributes(attributes: u64) -> usize {
    // Low bits [4:0] encode alignment selector on Xbox One / Series XDK/GDK:
    // 0 = default (16B), 1 = 4KB, 2 = 64KB, 3 = 16B, 4 = 32B, 5 = 64B, 6 = 128B, 7 = 256B, 8 = 4KB.
    match attributes & 0x1F {
        1 | 8 => 4096,
        2 => 65536,
        4 => 32,
        5 => 64,
        6 => 128,
        7 => 256,
        _ => 16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_va_alloc_read_write_and_gpu_alias() {
        let mut space = TitleAddressSpace::default();
        let va = space.alloc(None, 0x2000, PAGE_SIZE_64K, MemoryProtect::GpuCoherentReadWrite, true).unwrap();
        assert_eq!(va % PAGE_SIZE_64K, 0);
        space.write_u64(va + 0x100, 0xDEAD_BEEF_CAFE_BABE).unwrap();

        let alias = space.map_gpu_alias(va, 0x2000).unwrap();
        assert_eq!(space.translate_gpu_va(alias.gpu_va + 0x100), Some(va + 0x100));
        assert_eq!(space.read_u64(alias.gpu_va + 0x100).unwrap(), 0xDEAD_BEEF_CAFE_BABE);

        // Writing through GPU alias is immediately visible at the Title VA.
        space.write_u32(alias.gpu_va + 0x200, 0x1234_5678).unwrap();
        assert_eq!(space.read_u32(va + 0x200).unwrap(), 0x1234_5678);

        let st = space.status();
        assert_eq!(st.length, 64);
        assert_eq!(st.total_mem, SERIES_X_TITLE_RAM_BYTES);
        assert!(st.title_used >= 0x2000);
        assert!(space.free(va));
        assert!(space.translate_gpu_va(alias.gpu_va).is_none());
    }

    #[test]
    fn enforces_page_protections_and_bounds() {
        let mut space = TitleAddressSpace::default();
        let va = space.alloc(Some(0x1_4000_0000), 0x1000, PAGE_SIZE_4K, MemoryProtect::ReadOnly, true).unwrap();
        space.load_bytes(va, &[0x90, 0xC3]).unwrap();
        assert_eq!(space.read_u8(va).unwrap(), 0x90);
        assert!(space.write_u8(va, 0xCC).is_err());

        let old = space.protect(va, MemoryProtect::ReadWrite).unwrap();
        assert_eq!(old, MemoryProtect::ReadOnly);
        space.write_u8(va, 0xCC).unwrap();
        assert_eq!(space.read_u8(va).unwrap(), 0xCC);
        assert!(space.read_u64(va + 0xFFC).is_err());
    }

    #[test]
    fn xmem_attribute_alignment_decoding() {
        assert_eq!(xmem_alignment_from_attributes(0), 16);
        assert_eq!(xmem_alignment_from_attributes(1), 4096);
        assert_eq!(xmem_alignment_from_attributes(2), 65536);
        assert_eq!(xmem_alignment_from_attributes(6), 128);
    }
}
