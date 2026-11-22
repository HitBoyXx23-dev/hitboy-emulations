use super::super::Mem;
use std::collections::BTreeMap;

mod vertex;
mod half;
mod pixel;
#[cfg(test)]
pub(super) mod fixtures;
pub(super) use vertex::{execute as vertex_exports, Vertex};
pub(super) use pixel::execute as pixel_exports;

#[derive(Default)]
pub(super) struct Stats {
    pub groups: u64,
    pub instructions: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
}

struct Wave {
    scalar: [u32; 128],
    vector: Vec<u32>,
    pc: u64,
}

impl Wave {
    fn fetch(&mut self, mem: &Mem) -> Result<u32, String> {
        let mut bytes = [0; 4];
        if !mem.read(self.pc, &mut bytes) { return Err(format!("GCN instruction fetch failed at {:#x}", self.pc)); }
        self.pc = self.pc.checked_add(4).ok_or("GCN program counter overflow")?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn scalar_source(&self, code: u32, literal: Option<u32>) -> Result<u32, String> {
        match code {
            0..=127 => Ok(self.scalar[code as usize]),
            128..=192 => Ok(code - 128),
            193..=208 => Ok((-(code as i32 - 192)) as u32),
            255 => literal.ok_or_else(|| "GCN literal is unavailable for this instruction".into()),
            _ => Err(format!("GCN scalar source {code} is not implemented")),
        }
    }

    fn source(&self, code: u32, lane: usize) -> Result<u32, String> {
        if code >= 256 { Ok(self.vector[(code as usize - 256) * 64 + lane]) } else { self.scalar_source(code, None) }
    }

    fn active(&self, lane: usize) -> bool {
        let mask = self.scalar[126] as u64 | (self.scalar[127] as u64) << 32;
        mask & (1u64 << lane) != 0
    }

    fn scalar_instruction(&mut self, mem: &Mem, word: u32) -> Result<(), String> {
        let destination = ((word >> 16) & 127) as usize;
        if word & 0xff800000 == 0xbe800000 {
            let opcode = (word >> 8) & 255;
            if opcode != 3 { return Err(format!("GCN SOP1 opcode {opcode:#x} is not implemented")); }
            let code = word & 255;
            let literal = if code == 255 { Some(self.fetch(mem)?) } else { None };
            self.scalar[destination] = self.scalar_source(code, literal)?;
        } else {
            let opcode = (word >> 23) & 127;
            if opcode != 0x26 { return Err(format!("GCN SOP2 opcode {opcode:#x} is not implemented")); }
            let left = word & 255;
            let right = (word >> 8) & 255;
            let literal = if left == 255 || right == 255 { Some(self.fetch(mem)?) } else { None };
            self.scalar[destination] = self.scalar_source(left, literal)?.wrapping_mul(self.scalar_source(right, literal)?);
        }
        Ok(())
    }

    fn vector_instruction(&mut self, mem: &Mem, first: u32) -> Result<(), String> {
        let second = self.fetch(mem)?;
        let opcode = (first >> 17) & 511;
        if opcode != 0x15d || first & 0x1ff00 != 0 || second & 0xf8000000 != 0 {
            return Err(format!("GCN VOP3 opcode {opcode:#x} or modifiers are not implemented"));
        }
        let destination = (first & 255) as usize;
        let sources = [second & 511, (second >> 9) & 511, (second >> 18) & 511];
        for lane in 0..64 {
            if !self.active(lane) { continue; }
            let left = self.source(sources[0], lane)?;
            let right = self.source(sources[1], lane)?;
            let addend = self.source(sources[2], lane)?;
            self.vector[destination * 64 + lane] = left.abs_diff(right).wrapping_add(addend);
        }
        Ok(())
    }

    fn buffer_instruction(&mut self, mem: &mut Mem, first: u32, stats: &mut Stats) -> Result<(), String> {
        let second = self.fetch(mem)?;
        let opcode = (first >> 18) & 127;
        if !matches!(opcode, 3 | 7) || first & 0x3ffff != 0x2000 || second & 0x00e00000 != 0 {
            return Err(format!("GCN buffer opcode {opcode:#x} or addressing mode is not implemented"));
        }
        let address_register = (second & 255) as usize;
        let data_register = ((second >> 8) & 255) as usize;
        if data_register > 252 { return Err("GCN buffer vector register range is invalid".into()); }
        let resource = ((second >> 16) & 31) as usize * 4;
        let descriptor = &self.scalar[resource..resource + 4];
        let stride = (descriptor[1] >> 16) & 0x3fff;
        if stride != 16 || descriptor[1] & 0xc0000000 != 0 || descriptor[3] & 0xc7ffffff != 0x74fac {
            return Err("GCN formatted buffer supports only unswizzled 32x4 UINT descriptors".into());
        }
        let base = descriptor[0] as u64 | ((descriptor[1] & 0xfff) as u64) << 32;
        let records = descriptor[2] as u64;
        let scalar_offset = self.scalar_source(second >> 24, None)? as u64;
        if scalar_offset & 15 != 0 { return Err("GCN formatted buffer byte offset is not element aligned".into()); }
        for lane in 0..64 {
            if !self.active(lane) { continue; }
            let index = self.vector[address_register * 64 + lane] as u64;
            let offset = index * 16 + scalar_offset;
            let in_bounds = index < records && offset + 16 <= records * 16;
            let mut bytes = [0; 16];
            if in_bounds {
                let address = base.checked_add(offset).ok_or("GCN buffer address overflow")?;
                if !mem.read(address, &mut bytes) { return Err(format!("GCN buffer access is unmapped at {address:#x}")); }
                if opcode == 7 {
                    for component in 0..4 {
                        bytes[component * 4..component * 4 + 4].copy_from_slice(&self.vector[(data_register + component) * 64 + lane].to_le_bytes());
                    }
                    if !mem.write(address, &bytes) { return Err("GCN buffer store failed".into()); }
                    stats.bytes_written += 16;
                } else { stats.bytes_read += 16; }
            }
            if opcode == 3 {
                for component in 0..4 {
                    self.vector[(data_register + component) * 64 + lane] = u32::from_le_bytes(bytes[component * 4..component * 4 + 4].try_into().unwrap());
                }
            }
        }
        Ok(())
    }

    fn run(&mut self, mem: &mut Mem, stats: &mut Stats) -> Result<(), String> {
        loop {
            if stats.instructions >= 65536 { return Err("GCN dispatch instruction budget exceeded".into()); }
            let address = self.pc;
            let word = self.fetch(mem)?;
            stats.instructions += 1;
            let result = if word & 0xff800000 == 0xbf800000 {
                match (word >> 16) & 127 {
                    1 if word & 0xffff == 0 => return Ok(()),
                    0 | 12 => Ok(()),
                    opcode => Err(format!("GCN SOPP opcode {opcode:#x} is not implemented")),
                }
            } else if word & 0xff800000 == 0xbe800000 || word & 0xc0000000 == 0x80000000 {
                self.scalar_instruction(mem, word)
            } else if word & 0xfc000000 == 0xd0000000 {
                self.vector_instruction(mem, word)
            } else if word & 0xfc000000 == 0xe0000000 {
                self.buffer_instruction(mem, word, stats)
            } else { Err("GCN instruction encoding is not implemented".into()) };
            result.map_err(|error| format!("{error}; pc={address:#x} word={word:#010x}"))?;
        }
    }
}

pub(super) fn dispatch(mem: &mut Mem, registers: &BTreeMap<u32, u32>, groups: [u32; 3], initiator: u32) -> Result<Stats, String> {
    if initiator != 5 { return Err(format!("GCN dispatch initiator {initiator:#x} is not implemented")); }
    let count = groups.iter().try_fold(1u64, |total, value| total.checked_mul(*value as u64)).ok_or("GCN dispatch size overflow")?;
    if count > 4096 { return Err("GCN dispatch exceeds the group limit".into()); }
    let mut stats = Stats::default();
    if count == 0 { return Ok(stats); }
    let register = |address| registers.get(&address).copied().unwrap_or(0);
    let program = (register(0x2e0c) as u64) << 8 | (register(0x2e0d) as u64) << 40;
    if program == 0 { return Err("GCN compute program is not bound".into()); }
    let dimensions = [register(0x2e07), register(0x2e08), register(0x2e09)];
    if dimensions.iter().any(|value| *value == 0 || *value > 64) { return Err("GCN workgroup dimensions are not implemented".into()); }
    let lanes = dimensions.iter().product::<u32>();
    if lanes > 64 { return Err("GCN multi-wave workgroups are not implemented".into()); }
    let resources = register(0x2e13);
    let users = (resources >> 1) & 31;
    let components = (resources >> 11) & 3;
    if users > 16 || components > 2 || resources & (1 | (1 << 10) | (0x1ff << 15)) != 0 {
        return Err("GCN scratch, LDS or extended workgroup inputs are not implemented".into());
    }
    for z in 0..groups[2] {
        for y in 0..groups[1] {
            for x in 0..groups[0] {
                let mut wave = Wave { scalar: [0; 128], vector: vec![0; 256 * 64], pc: program };
                for index in 0..users { wave.scalar[index as usize] = register(0x2e40 + index); }
                let mut index = users as usize;
                for (bit, value) in [(7, x), (8, y), (9, z)] {
                    if resources & (1 << bit) != 0 { wave.scalar[index] = value; index += 1; }
                }
                let mask = if lanes == 64 { u64::MAX } else { (1u64 << lanes) - 1 };
                wave.scalar[126] = mask as u32;
                wave.scalar[127] = (mask >> 32) as u32;
                for lane in 0..lanes as usize {
                    wave.vector[lane] = lane as u32 % dimensions[0];
                    if components >= 1 { wave.vector[64 + lane] = lane as u32 / dimensions[0] % dimensions[1]; }
                    if components >= 2 { wave.vector[128 + lane] = lane as u32 / (dimensions[0] * dimensions[1]); }
                }
                wave.run(mem, &mut stats).map_err(|error| format!("GCN group=({x},{y},{z}): {error}"))?;
                stats.groups += 1;
            }
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    #[ignore]
    fn real_ot11_shader_copies_its_authentic_captured_buffer() {
        let root = PathBuf::from(std::env::var_os("HITBOY_PS4_COMPUTE_SNAPSHOT").expect("Supply the private compute snapshot directory"));
        let code = std::fs::read(root.join("compute-shader-prefix.bin")).unwrap();
        let input = std::fs::read(root.join("compute-buffer-00.bin")).unwrap();
        let output = std::fs::read(root.join("compute-buffer-01.bin")).unwrap();
        let register_bytes = std::fs::read(root.join("compute-registers.bin")).unwrap();
        assert_eq!((code.len(), input.len(), output.len()), (76, 2048, 2048));
        assert!(input.iter().any(|value| *value != 0));
        assert!(output.iter().all(|value| *value == 0));
        let registers = register_bytes.chunks_exact(8).map(|pair| (u32::from_le_bytes(pair[..4].try_into().unwrap()), u32::from_le_bytes(pair[4..].try_into().unwrap()))).collect::<BTreeMap<_, _>>();
        let program = (registers[&0x2e0c] as u64) << 8 | (registers[&0x2e0d] as u64) << 40;
        let buffer = |index: u32| registers[&(0x2e40 + index * 4)] as u64 | ((registers[&(0x2e41 + index * 4)] & 0xfff) as u64) << 32;
        let mut mem = Mem::new().unwrap();
        mem.map(program & !4095, 8192, true, false).unwrap();
        mem.map(buffer(0), 4096, true, false).unwrap();
        mem.map(buffer(1), 4096, true, false).unwrap();
        mem.write(program, &code);
        mem.write(buffer(0), &input);
        mem.write(buffer(1), &output);
        mem.write_u64(buffer(1) + 2048, 0xabcdef0123456789);
        let stats = dispatch(&mut mem, &registers, [1, 1, 1], 5).unwrap();
        let mut actual = vec![0; input.len()];
        mem.read(buffer(1), &mut actual);
        assert_eq!(actual, input);
        mem.read(buffer(0), &mut actual);
        assert_eq!(actual, input);
        assert_eq!(mem.read_u64(buffer(1) + 2048), 0xabcdef0123456789);
        assert_eq!((stats.groups, stats.instructions, stats.bytes_read, stats.bytes_written), (1, 11, 2048, 2048));
        println!("authentic_shader_bytes={} groups={} instructions={} read={} written={} byte_exact_copy=true rendering_verified=false", code.len(), stats.groups, stats.instructions, stats.bytes_read, stats.bytes_written);
        let mut two_group_registers = registers.clone();
        two_group_registers.insert(0x2e42, 256);
        two_group_registers.insert(0x2e46, 256);
        let mut expanded = input.clone();
        expanded.extend(input.iter().map(|value| value ^ 0x5a));
        mem.map(buffer(1) + 4096, 4096, true, false).unwrap();
        mem.write(buffer(0), &expanded);
        mem.write(buffer(1), &vec![0; 4096]);
        mem.write_u64(buffer(1) + 4096, 0xabcdef0123456789);
        let stats = dispatch(&mut mem, &two_group_registers, [2, 1, 1], 5).unwrap();
        let mut actual = vec![0; expanded.len()];
        mem.read(buffer(1), &mut actual);
        assert_eq!(actual, expanded);
        assert_eq!(mem.read_u64(buffer(1) + 4096), 0xabcdef0123456789);
        assert_eq!((stats.groups, stats.instructions, stats.bytes_read, stats.bytes_written), (2, 22, 4096, 4096));
        println!("descriptor_variation_groups=2 byte_exact_copy=true rendering_verified=false");
    }

    #[test]
    fn scalar_literals_and_vector_sad_use_integer_semantics_and_active_lanes() {
        let mut mem = Mem::new().unwrap();
        mem.map(0x50000000, 4096, true, false).unwrap();
        mem.write(0x50000000, &12u32.to_le_bytes());
        let mut wave = Wave { scalar: [0; 128], vector: vec![0xdeadbeef; 256 * 64], pc: 0x50000000 };
        wave.scalar[126] = 3;
        wave.scalar_instruction(&mem, 0xbeeb03ff).unwrap();
        assert_eq!(wave.scalar[107], 12);
        wave.scalar[8] = u32::MAX;
        wave.pc = 0x50000000;
        wave.scalar_instruction(&mem, 0x936aff08).unwrap();
        assert_eq!(wave.scalar[106], u32::MAX.wrapping_mul(12));
        wave.scalar[106] = 100;
        wave.vector[0] = 7;
        wave.vector[1] = u32::MAX;
        mem.write(0x50000000, &0x0401006au32.to_le_bytes());
        wave.pc = 0x50000000;
        wave.vector_instruction(&mem, 0xd2ba0008).unwrap();
        assert_eq!(wave.vector[8 * 64], 107);
        assert_eq!(wave.vector[8 * 64 + 1], 99);
        assert_eq!(wave.vector[8 * 64 + 2], 0xdeadbeef);
    }

    #[test]
    fn typed_buffer_out_of_bounds_reads_zero_and_stores_preserve_memory() {
        let mut mem = Mem::new().unwrap();
        mem.map(0x50000000, 4096, true, false).unwrap();
        mem.write(0x50000100, &[0xab; 16]);
        let mut wave = Wave { scalar: [0; 128], vector: vec![0xdeadbeef; 256 * 64], pc: 0x50000000 };
        wave.scalar[..4].copy_from_slice(&[0x50000100, 0x107000, 1, 0x18074fac]);
        wave.scalar[126] = 1;
        wave.vector[8 * 64] = 2;
        mem.write(0x50000000, &0x80000008u32.to_le_bytes());
        let mut stats = Stats::default();
        wave.buffer_instruction(&mut mem, 0xe00c2000, &mut stats).unwrap();
        for register in 0..4 { assert_eq!(wave.vector[register * 64], 0); }
        assert_eq!(wave.vector[1], 0xdeadbeef);
        wave.pc = 0x50000000;
        wave.buffer_instruction(&mut mem, 0xe01c2000, &mut stats).unwrap();
        assert_eq!(mem.read_u64(0x50000100), 0xabababababababab);
        assert_eq!((stats.bytes_read, stats.bytes_written), (0, 0));
        wave.vector[8 * 64] = 0;
        wave.scalar[0] = 0x60000000;
        wave.pc = 0x50000000;
        assert!(wave.buffer_instruction(&mut mem, 0xe01c2000, &mut stats).is_err());
        assert_eq!(stats.bytes_written, 0);
    }

    #[test]
    fn unsupported_compute_modes_fail_without_completing_a_dispatch() {
        let mut mem = Mem::new().unwrap();
        let registers = BTreeMap::new();
        assert!(dispatch(&mut mem, &registers, [1, 1, 1], 1).is_err());
        assert!(dispatch(&mut mem, &registers, [4097, 1, 1], 5).is_err());
        assert!(dispatch(&mut mem, &registers, [1, 1, 1], 5).is_err());
        assert_eq!(dispatch(&mut mem, &registers, [0, 1, 1], 5).unwrap().groups, 0);
    }
}
