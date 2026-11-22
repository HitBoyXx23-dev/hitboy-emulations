use super::*;

const INVALID: u64 = 0x8002_0016;
const BAD_ADDRESS: u64 = 0x8002_000E;
const NO_MEMORY: u64 = 0x8002_000C;
const UNSUPPORTED: u64 = 0x8002_004E;
const RANGE_BASE: u64 = 0x20_0000_0000;

impl Machine {
    pub(super) fn reserve_virtual_range(&mut self) -> u64 {
        let (output, length, flags, alignment) = (self.arg(0), self.arg(1), self.arg(2), self.arg(3));
        let alignment = alignment.max(0x4000);
        let mut bytes = [0; 8];
        if output == 0 || !self.mem.read(output, &mut bytes) { return BAD_ADDRESS; }
        if length == 0 || length & 0x3FFF != 0 || !alignment.is_power_of_two() || flags & !0x90 != 0 { return INVALID; }
        let requested = u64::from_le_bytes(bytes);
        let next = self.runtime.virtual_ranges.iter().map(|(start, length)| start + length).max().unwrap_or(RANGE_BASE);
        let Some(aligned) = next.checked_add(alignment - 1).map(|value| value & !(alignment - 1)) else { return NO_MEMORY; };
        let address = if flags & 0x10 != 0 { requested } else { aligned };
        let Some(end) = address.checked_add(length) else { return INVALID; };
        if address == 0 || address & (alignment - 1) != 0 || end >= 0x8000_0000_0000 { return INVALID; }
        if self.runtime.virtual_ranges.iter().any(|(start, size)| address < start + size && end > *start)
            || self.mem.pages.keys().any(|page| *page >= address >> 12 && *page <= (end - 1) >> 12) { return NO_MEMORY; }
        if !self.mem.write_u64(output, address) { return BAD_ADDRESS; }
        self.runtime.virtual_ranges.push((address, length));
        0
    }

    pub(in crate::machine) fn map_flexible_memory(&mut self) -> u64 {
        let (output, length, protection, flags) = (self.arg(0), self.arg(1), self.arg(2), self.arg(3));
        let mut previous = [0; 8];
        if output == 0 || !self.mem.read(output, &mut previous) { return BAD_ADDRESS; }
        if length == 0 || length & 0x3FFF != 0 || protection & !0x37 != 0 || flags & !0x10 != 0 { return INVALID; }
        if length > RAM_SIZE as u64 { return NO_MEMORY; }
        let address = if flags & 0x10 != 0 {
            let requested = u64::from_le_bytes(previous);
            let Some(end) = requested.checked_add(length) else { return INVALID; };
            if requested == 0 || requested & 0x3FFF != 0 || end >= 0x8000_0000_0000 { return INVALID; }
            if self.mem.pages.keys().any(|page| *page >= requested >> 12 && *page <= (end - 1) >> 12) { return NO_MEMORY; }
            if self.mem.map(requested, length, protection & 2 != 0, protection & 4 != 0).is_err() { return NO_MEMORY; }
            requested
        } else {
            let address = self.heap_alloc(length, 0x4000);
            if address == 0 { return NO_MEMORY; }
            address
        };
        if !self.mem.write_u64(output, address) { return BAD_ADDRESS; }
        self.cpu.flush_tlb();
        0
    }

    pub(in crate::machine) fn unmap_direct_range(&mut self, address: u64, length: u64) -> u64 {
        let Some(end) = address.checked_add(length) else { return INVALID; };
        if !self.mem.unmap(address, length) { return INVALID; }
        let mut remaining = Vec::new();
        for (physical, size, start) in self.runtime.direct_mappings.drain(..) {
            let mapping_end = start + size;
            if start >= end || mapping_end <= address {
                remaining.push((physical, size, start));
            } else {
                if start < address { remaining.push((physical, address - start, start)); }
                if mapping_end > end { remaining.push((physical + end - start, mapping_end - end, end)); }
            }
        }
        self.runtime.direct_mappings = remaining;
        self.cpu.flush_tlb();
        0
    }

    fn map_batch_entry(&mut self, record: &[u8; 32]) -> u64 {
        let address = u64::from_le_bytes(record[0..8].try_into().unwrap());
        let offset = u64::from_le_bytes(record[8..16].try_into().unwrap());
        let length = u64::from_le_bytes(record[16..24].try_into().unwrap());
        let protection = record[24];
        let operation = u32::from_le_bytes(record[28..32].try_into().unwrap());
        let Some(end) = address.checked_add(length) else { return INVALID; };
        if address == 0 || address & 0x3FFF != 0 || length == 0 || length & 0x3FFF != 0
            || end >= 0x8000_0000_0000 || protection & !0x37 != 0 { return INVALID; }
        if length > RAM_SIZE as u64 { return NO_MEMORY; }
        if operation == 1 { return self.unmap_direct_range(address, length); }
        if operation != 0 && operation != 3 { return UNSUPPORTED; }
        if operation == 0 {
            let Some(physical_end) = offset.checked_add(length) else { return INVALID; };
            if offset & 0x3FFF != 0 || physical_end > self.direct_next { return INVALID; }
            if self.runtime.direct_mappings.iter().any(|(physical, size, _)| offset < physical + size && physical_end > *physical) { return UNSUPPORTED; }
        }
        if self.mem.pages.keys().any(|page| *page >= address >> 12 && *page <= (end - 1) >> 12) { return NO_MEMORY; }
        let backing = if operation == 0 { Some((&mut self.runtime.direct_backing, offset)) } else { None };
        if self.mem.map_backed(address, length, protection & 2 != 0, protection & 4 != 0, backing).is_err() { return NO_MEMORY; }
        if operation == 0 { self.runtime.direct_mappings.push((offset, length, address)); }
        self.cpu.flush_tlb();
        0
    }

    pub(super) fn batch_map(&mut self) -> u64 {
        let (entries, count, output) = (self.arg(0), self.arg(1), self.arg(2));
        if count > 4096 { return INVALID; }
        if output != 0 && !self.mem.write(output, &0u32.to_le_bytes()) { return BAD_ADDRESS; }
        for index in 0..count {
            let Some(address) = entries.checked_add(index * 32) else { return BAD_ADDRESS; };
            let mut record = [0; 32];
            if entries == 0 || !self.mem.read(address, &mut record) { return BAD_ADDRESS; }
            let result = self.map_batch_entry(&record);
            if result != 0 {
                if self.output.len() < 16384 {
                    let target = u64::from_le_bytes(record[..8].try_into().unwrap());
                    let offset = u64::from_le_bytes(record[8..16].try_into().unwrap());
                    let length = u64::from_le_bytes(record[16..24].try_into().unwrap());
                    let operation = u32::from_le_bytes(record[28..32].try_into().unwrap());
                    self.output.push_str(&format!("Memory batch failed: operation={operation} target={target:#x} offset={offset:#x} length={length:#x} protection={:#x} result={result:#x}\n", record[24]));
                }
                return result;
            }
            if output != 0 { self.mem.write(output, &((index + 1) as u32).to_le_bytes()); }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_stays_sparse_and_batch_mapping_reports_partial_completion() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let storage = machine.heap_alloc(128, 16);
        machine.mem.write(storage, &[0; 128]);
        machine.cpu.r[7] = storage;
        machine.cpu.r[6] = 0x10000;
        machine.cpu.r[2] = 0x80;
        machine.cpu.r[1] = 0x10000;
        let before = machine.mem.next_frame;
        assert_eq!(machine.reserve_virtual_range(), 0);
        assert_eq!(machine.mem.next_frame, before);
        let address = machine.mem.read_u64(storage);
        assert_eq!(address & 0xFFFF, 0);
        let entries = storage + 16;
        machine.mem.write_u64(entries, address);
        machine.mem.write_u64(entries + 8, 0);
        machine.mem.write_u64(entries + 16, 0x4000);
        machine.mem.write(entries + 24, &[3, 0, 0, 0, 0, 0, 0, 0]);
        machine.direct_next = 0x4000;
        machine.cpu.r[7] = entries;
        machine.cpu.r[6] = 2;
        machine.cpu.r[2] = storage + 112;
        machine.mem.write(storage + 112, &[0xCC; 8]);
        assert_eq!(machine.batch_map(), INVALID);
        assert_eq!(machine.mem.read_u64(storage + 112), 0xCCCCCCCC00000001);
        assert!(machine.mem.write_u64(address, 42));
        assert!(!machine.mem.read(address + 0x4000, &mut [0]));
        machine.cpu.r[7] = entries;
        machine.cpu.r[6] = u64::MAX;
        assert_eq!(machine.batch_map(), INVALID);
        machine.mem.write_u64(entries, address + 0x4000);
        machine.mem.write_u64(entries + 8, 0x4000);
        machine.cpu.r[6] = 1;
        assert_eq!(machine.batch_map(), INVALID);
        assert!(!machine.mem.read(address + 0x4000, &mut [0]));
    }
}

#[cfg(test)]
mod flexible_tests {
    use super::*;

    #[test]
    fn fixed_flexible_mapping_uses_requested_address_and_preserves_failures() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let output = machine.heap_alloc(16, 16);
        machine.cpu.r[7] = output;
        machine.cpu.r[6] = 0x4000;
        machine.cpu.r[2] = 3;
        machine.cpu.r[1] = 0x10;
        machine.mem.write_u64(output, RANGE_BASE);
        assert_eq!(machine.map_flexible_memory(), 0);
        assert_eq!(machine.mem.read_u64(output), RANGE_BASE);
        assert!(machine.mem.write_u64(RANGE_BASE, 42));
        assert_eq!(machine.map_flexible_memory(), NO_MEMORY);
        assert_eq!(machine.mem.read_u64(RANGE_BASE), 42);
        machine.cpu.r[6] = RAM_SIZE as u64 + 0x4000;
        assert_eq!(machine.map_flexible_memory(), NO_MEMORY);
        assert_eq!(machine.mem.read_u64(output), RANGE_BASE);
        machine.cpu.r[6] = 0;
        assert_eq!(machine.map_flexible_memory(), INVALID);
        machine.cpu.r[7] = 0;
        assert_eq!(machine.map_flexible_memory(), BAD_ADDRESS);
    }
}

#[cfg(test)]
mod direct_unmap_tests {
    use super::*;

    #[test]
    fn batch_unmap_splits_ranges_and_remapping_preserves_physical_contents() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let address = RANGE_BASE;
        machine.direct_next = 0xC000;
        let mut record = [0; 32];
        record[..8].copy_from_slice(&address.to_le_bytes());
        record[16..24].copy_from_slice(&0xC000u64.to_le_bytes());
        record[24] = 3;
        assert_eq!(machine.map_batch_entry(&record), 0);
        assert!(machine.mem.write_u64(address, 11));
        assert!(machine.mem.write_u64(address + 0x4000, 22));
        assert!(machine.mem.write_u64(address + 0x8000, 33));
        record[..8].copy_from_slice(&(address + 0x4000).to_le_bytes());
        record[8..16].copy_from_slice(&0x4000u64.to_le_bytes());
        record[16..24].copy_from_slice(&0x4000u64.to_le_bytes());
        record[28..32].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(machine.map_batch_entry(&record), 0);
        assert!(!machine.mem.read(address + 0x4000, &mut [0]));
        assert_eq!(machine.mem.read_u64(address), 11);
        assert_eq!(machine.mem.read_u64(address + 0x8000), 33);
        assert_eq!(machine.runtime.direct_mappings, vec![(0, 0x4000, address), (0x8000, 0x4000, address + 0x8000)]);
        let frames = machine.mem.next_frame;
        record[28..32].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(machine.map_batch_entry(&record), 0);
        assert_eq!(machine.mem.read_u64(address + 0x4000), 22);
        assert_eq!(machine.mem.next_frame, frames);
        record[..8].copy_from_slice(&(address + 0x10000).to_le_bytes());
        assert_eq!(machine.map_batch_entry(&record), UNSUPPORTED);
    }
}
