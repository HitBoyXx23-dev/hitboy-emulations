use super::*;
use std::collections::HashSet;

const DIRECT_SIZE: u64 = 0x1_4000_0000;
const POOL_BASE: u64 = 0x10_0000_0000;
const INVALID: u64 = 0x8002_0016;
const NO_MEMORY: u64 = 0x8002_000C;
const BAD_ADDRESS: u64 = 0x8002_000E;

#[derive(Default)]
pub(super) struct Pools {
    reserved: Vec<(u64, u64)>,
    pub(super) committed: HashSet<u64>,
    pub(super) capacity: u64,
}

impl Machine {
    fn pool_reserve(&mut self) -> u64 {
        let (requested, length, alignment, flags, output) = (self.arg(0), self.arg(1), self.arg(2), self.arg(3), self.arg(4));
        let alignment = if alignment == 0 { 0x20_0000 } else { alignment };
        if length == 0 || length & 0x1F_FFFF != 0 || !alignment.is_power_of_two() || alignment < 0x20_0000 || self.pools.reserved.len() >= 128 { return INVALID; }
        let next = self.pools.reserved.iter().map(|(start, length)| start + length).max().unwrap_or(POOL_BASE).max(POOL_BASE);
        let address = if requested != 0 { requested } else if flags & 0x10 != 0 { return INVALID; } else {
            let Some(address) = next.checked_add(alignment - 1) else { return NO_MEMORY; };
            address & !(alignment - 1)
        };
        let Some(end) = address.checked_add(length) else { return INVALID; };
        if address & (alignment - 1) != 0 || end >= 0x8000_0000_0000 { return INVALID; }
        if self.pools.reserved.iter().any(|(start, length)| address < start + length && end > *start)
            || self.mem.pages.keys().any(|page| *page >= address >> 12 && *page <= (end - 1) >> 12) { return NO_MEMORY; }
        if !self.mem.write_u64(output, address) { return BAD_ADDRESS; }
        self.pools.reserved.push((address, length));
        0
    }

    fn pool_commit(&mut self, decommit: bool) -> u64 {
        let (address, length) = (self.arg(0), self.arg(1));
        let Some(end) = address.checked_add(length) else { return INVALID; };
        if address == 0 || address & 0xFFFF != 0 || length == 0 || length & 0xFFFF != 0 || length > RAM_SIZE as u64 { return INVALID; }
        if !self.pools.reserved.iter().any(|(start, size)| address >= *start && end <= start + size) { return INVALID; }
        let first = address >> 16;
        let last = end >> 16;
        if decommit {
            if !self.mem.unmap(address, length) { return INVALID; }
            for page in first..last { self.pools.committed.remove(&page); }
        } else {
            let protection = self.arg(3);
            if protection & !0x33 != 0 { return INVALID; }
            let added = (first..last).filter(|page| !self.pools.committed.contains(page)).count() as u64 * 0x10000;
            if self.pools.committed.len() as u64 * 0x10000 + added > self.pools.capacity { return NO_MEMORY; }
            if self.mem.map(address, length, protection & 2 != 0, false).is_err() { return NO_MEMORY; }
            self.pools.committed.extend(first..last);
        }
        self.cpu.flush_tlb();
        0
    }

    pub(super) fn call_pool(&mut self, name: &str) -> bool {
        let result = match name {
            "sceKernelMemoryPoolReserve" => self.pool_reserve(),
            "sceKernelMemoryPoolCommit" => self.pool_commit(false),
            "sceKernelMemoryPoolDecommit" => self.pool_commit(true),
            "sceKernelAvailableDirectMemorySize" | "sceKernelMemoryPoolExpand" => {
                let (start, end, size_or_alignment, alignment_or_output, output) = (self.arg(0), self.arg(1), self.arg(2), self.arg(3), self.arg(4));
                let expand = name == "sceKernelMemoryPoolExpand";
                let alignment = if expand { alignment_or_output } else { size_or_alignment }.max(0x10000);
                if start >= end || !alignment.is_power_of_two() || alignment & 0xFFFF != 0 { INVALID } else {
                    let base = self.direct_next.max(start);
                    match base.checked_add(alignment - 1).map(|address| address & !(alignment - 1)) {
                        Some(address) if address < end.min(DIRECT_SIZE) => {
                            let available = end.min(DIRECT_SIZE) - address;
                            if expand {
                                let size = size_or_alignment;
                                if size == 0 || size & 0xFFFF != 0 { INVALID }
                                else if size > available { NO_MEMORY }
                                else if !self.mem.write_u64(output, address) { BAD_ADDRESS }
                                else { self.direct_next = address + size; self.pools.capacity += size; 0 }
                            } else if !self.mem.write_u64(alignment_or_output, address) || !self.mem.write_u64(output, available) { BAD_ADDRESS } else { 0 }
                        }
                        _ => NO_MEMORY,
                    }
                }
            }
            _ => return false,
        };
        self.ret(result);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_is_sparse_and_only_expanded_memory_can_be_committed() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let output = 0x5000_0000;
        machine.mem.map(output, 4096, true, false).unwrap();
        let before = machine.mem.next_frame;
        machine.cpu.r[7] = 0;
        machine.cpu.r[6] = 0x4_0000_0000;
        machine.cpu.r[2] = 0;
        machine.cpu.r[1] = 0;
        machine.cpu.r[8] = output;
        machine.call_pool("sceKernelMemoryPoolReserve");
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.mem.next_frame, before);
        let address = machine.mem.read_u64(output);
        machine.cpu.r[7] = address;
        machine.cpu.r[6] = 0x10000;
        machine.cpu.r[1] = 3;
        machine.call_pool("sceKernelMemoryPoolCommit");
        assert_eq!(machine.cpu.r[0], NO_MEMORY);
        machine.pools.capacity = 0x10000;
        machine.call_pool("sceKernelMemoryPoolCommit");
        assert_eq!(machine.cpu.r[0], 0);
        assert!(machine.mem.write_u64(address, 42));
        assert_eq!(machine.mem.read_u64(address), 42);
        assert!(!machine.mem.read(address + 0x10000, &mut [0u8; 1]));
        machine.call_pool("sceKernelMemoryPoolDecommit");
        assert_eq!(machine.cpu.r[0], 0);
        assert!(machine.pools.committed.is_empty());
        assert!(!machine.mem.read(address, &mut [0u8; 1]));
    }
}
