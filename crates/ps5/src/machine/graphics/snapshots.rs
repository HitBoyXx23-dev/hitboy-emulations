use super::*;
use std::collections::BTreeSet;

struct Captures<'a> {
    mem: &'a Mem,
    entries: Vec<(String, u64, Vec<u8>)>,
    ranges: BTreeSet<(u64, usize)>,
    bytes: usize,
}

impl Captures<'_> {
    fn add(&mut self, name: String, address: u64, length: usize) {
        if address == 0 || length == 0 || length > 65536 || self.entries.len() >= 64 || length > (2 << 20) - self.bytes || self.ranges.contains(&(address, length)) { return; }
        let mut bytes = vec![0; length];
        if !self.mem.read(address, &mut bytes) { return; }
        self.ranges.insert((address, length));
        self.bytes += length;
        self.entries.push((name, address, bytes));
    }

    fn buffer(&mut self, name: String, descriptor: &[u32], indices: &[u32]) {
        if descriptor.len() < 4 || descriptor[3] >> 30 != 0 || (descriptor[3] >> 15) & 15 == 0 { return; }
        let address = descriptor[0] as u64 | ((descriptor[1] & 0xfff) as u64) << 32;
        let stride = ((descriptor[1] >> 16) & 0x3fff).max(1) as u64;
        let length = descriptor[2] as u64 * stride;
        if length <= 65536 { self.add(name, address, length as usize); return; }
        if stride > 65536 || stride == 1 { return; }
        for index in indices.iter().copied().collect::<BTreeSet<_>>() {
            if index >= descriptor[2] { continue; }
            let Some(record) = address.checked_add(index as u64 * stride) else { continue; };
            self.add(format!("{name}-record-{index}"), record, stride as usize);
        }
    }
}

impl Machine {
    pub fn raster_resource_snapshots(&self) -> Vec<(String, u64, Vec<u8>)> {
        if !self.runtime_error.as_deref().is_some_and(|error| error.starts_with("PS4 GPU execution stopped:")) { return Vec::new(); }
        let mut captures = Captures { mem: &self.mem, entries: Vec::new(), ranges: BTreeSet::new(), bytes: 0 };
        let processor = &self.graphics.processor;
        let mut indices = Vec::new();
        if let Some((address, length, width)) = processor.pending_index_buffer() {
            captures.add("draw-indices".into(), address, length);
            let mut bytes = vec![0; length];
            if self.mem.read(address, &mut bytes) {
                indices = bytes.chunks_exact(width).map(|word| if width == 2 { u16::from_le_bytes(word.try_into().unwrap()) as u32 } else { u32::from_le_bytes(word.try_into().unwrap()) }).collect();
            }
        }
        for (stage, base, resource) in [("vertex", 0x2c4c, 0x2c4b), ("pixel", 0x2c0c, 0x2c0b)] {
            let users = ((processor.register(resource) >> 1) & 31).min(16);
            let registers = (0..users).map(|offset| processor.register(base + offset)).collect::<Vec<_>>();
            for slot in (0..registers.len()).step_by(4) {
                if let Some(descriptor) = registers.get(slot..slot + 4) { captures.buffer(format!("{stage}-s{slot}-buffer"), descriptor, &indices); }
            }
            for slot in (0..registers.len().saturating_sub(1)).step_by(2) {
                if stage != "vertex" { continue; }
                let address = registers[slot] as u64 | (registers[slot + 1] as u64) << 32;
                if address & 3 != 0 { continue; }
                captures.add(format!("{stage}-s{slot}-pointer-prefix"), address, 4096);
                let mut table = [0; 256];
                if !self.mem.read(address, &mut table) { continue; }
                for (index, descriptor) in table.chunks_exact(16).enumerate() {
                    let words = descriptor.chunks_exact(4).map(|word| u32::from_le_bytes(word.try_into().unwrap())).collect::<Vec<_>>();
                    captures.buffer(format!("{stage}-s{slot}-table-buffer-{index}"), &words, &indices);
                }
            }
            for slot in (0..registers.len()).step_by(4) {
                let Some(descriptor) = registers.get(slot..slot + 8) else { continue; };
                if !(8..=15).contains(&(descriptor[3] >> 28)) { continue; }
                let address = (descriptor[0] as u64 | ((descriptor[1] & 0x3f) as u64) << 32) << 8;
                captures.add(format!("{stage}-s{slot}-texture-prefix"), address, 4096);
            }
        }
        captures.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_capture_preserves_real_ranges_and_rejects_unbounded_requests() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        machine.mem.map(0x50000000, 0x7000, true, false).unwrap();
        machine.mem.write(0x50003000, &[0xac; 32]);
        machine.mem.write(0x50004000, &[0xbd; 32]);
        machine.mem.write(0x50005000, &[0xce; 12]);
        let descriptor = [0x50004000u32, 0x40000, 8, 0x20027fac];
        machine.mem.write(0x50002000, &descriptor.iter().flat_map(|word| word.to_le_bytes()).collect::<Vec<_>>());
        let commands = [
            0xc0017600u32, 0x4b, 0x10,
            0xc0087600, 0x4c, 0x50001000, 0, 0x50002000, 0, 0x50003000, 0x100000, 2, 0x20077fac,
            0xc0002a00, 0,
            0xc0042700, 6, 0x50005000, 0, 6, 0,
        ].iter().flat_map(|word| word.to_le_bytes()).collect();
        machine.graphics.processor.enqueue(vec![commands]).unwrap();
        assert!(machine.raster_resource_snapshots().is_empty());
        machine.pump_graphics();
        let captures = machine.raster_resource_snapshots();
        assert!(captures.contains(&("draw-indices".into(), 0x50005000, vec![0xce; 12])));
        assert!(captures.contains(&("vertex-s4-buffer".into(), 0x50003000, vec![0xac; 32])));
        assert!(captures.contains(&("vertex-s2-table-buffer-0".into(), 0x50004000, vec![0xbd; 32])));
        assert!(captures.iter().all(|(_, _, data)| data.len() <= 65536));
        assert_eq!(machine.video.flips, 0);
        let mut bounded = Captures { mem: &machine.mem, entries: Vec::new(), ranges: BTreeSet::new(), bytes: 0 };
        bounded.add("invalid".into(), 0x50000000, 65537);
        bounded.add("unmapped".into(), 0x60000000, 32);
        assert!(bounded.entries.is_empty());
        for length in 1..=65 { bounded.add("bounded".into(), 0x50000000, length); }
        assert_eq!(bounded.entries.len(), 64);
        bounded.add("duplicate".into(), 0x50000000, 1);
        assert_eq!(bounded.entries.len(), 64);
        let mut records = Captures { mem: &machine.mem, entries: Vec::new(), ranges: BTreeSet::new(), bytes: 0 };
        records.buffer("large".into(), &[0x50000000, 0x400000, 50000, 0x20077fac], &[0, 3, 3, u32::MAX]);
        assert_eq!(records.entries.len(), 2);
        assert_eq!(records.entries[0].1, 0x50000000);
        assert_eq!(records.entries[1].1, 0x500000c0);
        assert!(records.entries.iter().all(|(_, _, bytes)| bytes.len() == 64));
    }
}
