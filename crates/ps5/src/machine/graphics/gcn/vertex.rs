use super::*;

impl Wave {
    pub(super) fn raster_source(&self, code: u32, lane: usize, literal: Option<u32>) -> Result<u32, String> {
        match code {
            240..=247 => Ok([0.5f32, -0.5, 1.0, -1.0, 2.0, -2.0, 4.0, -4.0][code as usize - 240].to_bits()),
            256..=511 => Ok(self.vector[(code as usize - 256) * 64 + lane]),
            _ => self.scalar_source(code, literal),
        }
    }

    pub(super) fn scalar_memory(&mut self, mem: &Mem, word: u32) -> Result<(), String> {
        let opcode = (word >> 22) & 31;
        let buffered = opcode >= 8;
        let power = if buffered { opcode - 8 } else { opcode };
        if power > 4 { return Err(format!("GCN scalar memory opcode {opcode:#x} is not implemented")); }
        let count = 1usize << power;
        let destination = ((word >> 15) & 127) as usize;
        let source = ((word >> 9) & 63) as usize * 2;
        let descriptor_size = if buffered { 4 } else { 2 };
        if destination + count > 128 || source + descriptor_size > 128 { return Err("GCN scalar memory register range is invalid".into()); }
        let offset = if word & 256 != 0 { (word & 255) as u64 * 4 } else { self.scalar_source(word & 255, None)? as u64 };
        let mut address = self.scalar[source] as u64 | (self.scalar[source + 1] as u64) << 32;
        let mut bytes = vec![0; count * 4];
        let mut in_bounds = true;
        if buffered {
            address = self.scalar[source] as u64 | ((self.scalar[source + 1] & 0xfff) as u64) << 32;
            let stride = ((self.scalar[source + 1] >> 16) & 0x3fff).max(1) as u64;
            let length = self.scalar[source + 2] as u64 * stride;
            in_bounds = offset.checked_add(bytes.len() as u64).is_some_and(|end| end <= length);
        }
        if !in_bounds { return Err("GCN scalar buffer load exceeds its descriptor range".into()); }
        let address = address.checked_add(offset).ok_or("GCN scalar memory address overflow")?;
        if !mem.read(address, &mut bytes) { return Err(format!("GCN scalar memory is unmapped at {address:#x}")); }
        for (register, value) in self.scalar[destination..destination + count].iter_mut().zip(bytes.chunks_exact(4)) {
            *register = u32::from_le_bytes(value.try_into().unwrap());
        }
        Ok(())
    }

    fn scalar_program_counter(&mut self, word: u32) -> Result<(), String> {
        let source = (word & 255) as usize;
        let destination = ((word >> 16) & 127) as usize;
        let opcode = (word >> 8) & 255;
        if !matches!(opcode, 0x20 | 0x21) || source >= 127 || destination >= 127 { return Err("GCN program-counter register pair is invalid".into()); }
        let next = self.scalar[source] as u64 | (self.scalar[source + 1] as u64) << 32;
        if next & 3 != 0 { return Err("GCN program-counter target is unaligned".into()); }
        if opcode == 0x21 {
            self.scalar[destination] = self.pc as u32;
            self.scalar[destination + 1] = (self.pc >> 32) as u32;
        }
        self.pc = next;
        Ok(())
    }

    pub(super) fn raster_vop1(&mut self, mem: &Mem, word: u32) -> Result<(), String> {
        let opcode = (word >> 9) & 255;
        if !matches!(opcode, 1 | 0x25 | 0x27 | 0x2a) { return Err(format!("GCN raster VOP1 opcode {opcode:#x} is not implemented")); }
        let source = word & 511;
        let literal = if source == 255 { Some(self.fetch(mem)?) } else { None };
        let destination = ((word >> 17) & 255) as usize;
        for lane in 0..64 {
            if !self.active(lane) { continue; }
            let bits = self.raster_source(source, lane, literal)?;
            let value = f32::from_bits(bits);
            self.vector[destination * 64 + lane] = match opcode { 1 => bits, 0x25 => value.exp2().to_bits(), 0x27 => value.log2().to_bits(), _ => value.recip().to_bits() };
        }
        Ok(())
    }

    pub(super) fn raster_vop2(&mut self, mem: &Mem, word: u32) -> Result<(), String> {
        let opcode = word >> 25;
        if !matches!(opcode, 0 | 3 | 4 | 8 | 0xf | 0x10 | 0x1f | 0x21 | 0x2f) { return Err(format!("GCN raster VOP2 opcode {opcode:#x} is not implemented")); }
        let source = word & 511;
        if opcode == 0x21 && source == 255 { return Err("GCN MADAK cannot also encode a source literal".into()); }
        let literal = if source == 255 || opcode == 0x21 { Some(self.fetch(mem)?) } else { None };
        let second = ((word >> 9) & 255) as usize;
        let destination = ((word >> 17) & 255) as usize;
        let condition = self.scalar[106] as u64 | (self.scalar[107] as u64) << 32;
        for lane in 0..64 {
            if !self.active(lane) { continue; }
            let bits = self.raster_source(source, lane, literal)?;
            let right = self.vector[second * 64 + lane];
            let left = f32::from_bits(bits);
            let second = f32::from_bits(right);
            let value = match opcode {
                0 => if condition & (1u64 << lane) != 0 { right } else { bits },
                3 => (left + second).to_bits(),
                4 => (left - second).to_bits(),
                8 => (left * second).to_bits(),
                0xf => left.min(second).to_bits(),
                0x10 => left.max(second).to_bits(),
                0x1f => (left * second + f32::from_bits(self.vector[destination * 64 + lane])).to_bits(),
                0x21 => (left * second + f32::from_bits(literal.unwrap())).to_bits(),
                _ => half::to_half_rtz(left) as u32 | (half::to_half_rtz(second) as u32) << 16,
            };
            self.vector[destination * 64 + lane] = value;
        }
        Ok(())
    }

    pub(super) fn raster_compare(&mut self, mem: &Mem, word: u32) -> Result<(), String> {
        let opcode = (word >> 17) & 255;
        if !matches!(opcode, 1 | 4) { return Err(format!("GCN raster comparison {opcode:#x} is not implemented")); }
        let source = word & 511;
        let literal = if source == 255 { Some(self.fetch(mem)?) } else { None };
        let right = ((word >> 9) & 255) as usize;
        let mut mask = 0u64;
        for lane in 0..64 {
            if !self.active(lane) { continue; }
            let left = f32::from_bits(self.raster_source(source, lane, literal)?);
            let right = f32::from_bits(self.vector[right * 64 + lane]);
            if if opcode == 1 { left < right } else { left > right } { mask |= 1u64 << lane; }
        }
        self.scalar[106] = mask as u32;
        self.scalar[107] = (mask >> 32) as u32;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Vertex {
    pub position: [f32; 4],
    pub attributes: BTreeMap<u32, [f32; 4]>,
}

impl Wave {
    fn vertex_buffer(&mut self, mem: &Mem, first: u32) -> Result<(), String> {
        let second = self.fetch(mem)?;
        let opcode = (first >> 18) & 127;
        if !matches!(opcode, 1 | 3) || first & 0x3ffff != 0x2000 || second & 0x00e00000 != 0 { return Err("GCN vertex buffer addressing mode is not implemented".into()); }
        let components = if opcode == 1 { 2 } else { 4 };
        let destination = ((second >> 8) & 255) as usize;
        if destination + components > 256 { return Err("GCN vertex buffer destination exceeds the register file".into()); }
        let resource = ((second >> 16) & 31) as usize * 4;
        let descriptor = &self.scalar[resource..resource + 4];
        let format = (descriptor[3] >> 15) & 15;
        let numeric = (descriptor[3] >> 12) & 7;
        let length = match (format, numeric, components) {
            (11, 7, 2) => 8,
            (14, 7, 4) => 16,
            (10, 0, 4) => 4,
            _ => return Err(format!("GCN vertex buffer format={format} numeric={numeric} components={components} is not implemented")),
        };
        if descriptor[3] >> 30 != 0 || descriptor[1] & 0xc0000000 != 0 { return Err("GCN vertex buffer swizzled addressing is not implemented".into()); }
        let selections = (0..components).map(|component| (descriptor[3] >> (component * 3)) & 7).collect::<Vec<_>>();
        if selections.iter().any(|value| !matches!(value, 0 | 1 | 4..=7)) { return Err("GCN vertex channel selector is invalid".into()); }
        let stride = ((descriptor[1] >> 16) & 0x3fff) as u64;
        if stride < length as u64 || self.scalar_source(second >> 24, None)? != 0 { return Err("GCN vertex buffer stride or scalar offset is not implemented".into()); }
        let base = descriptor[0] as u64 | ((descriptor[1] & 0xfff) as u64) << 32;
        let address_register = (second & 255) as usize;
        for lane in 0..64 {
            if !self.active(lane) { continue; }
            let index = self.vector[address_register * 64 + lane] as u64;
            let mut bytes = vec![0; length];
            let mut channels = [0u32; 4];
            if index < descriptor[2] as u64 {
                let address = base.checked_add(index * stride).ok_or("GCN vertex buffer address overflow")?;
                if !mem.read(address, &mut bytes) { return Err(format!("GCN vertex buffer is unmapped at {address:#x}")); }
                if format == 10 {
                    for component in 0..4 { channels[component] = (bytes[component] as f32 / 255.0).to_bits(); }
                } else {
                    for (component, word) in bytes.chunks_exact(4).enumerate() { channels[component] = u32::from_le_bytes(word.try_into().unwrap()); }
                }
            }
            for (component, selection) in selections.iter().enumerate() {
                self.vector[(destination + component) * 64 + lane] = if index >= descriptor[2] as u64 { 0 } else { match selection { 0 => 0, 1 => 1.0f32.to_bits(), _ => channels[*selection as usize - 4] } };
            }
        }
        Ok(())
    }

    fn vertex_program(&mut self, mem: &mut Mem) -> Result<(BTreeMap<u32, Vec<[u32; 4]>>, u64), String> {
        let mut exports = BTreeMap::<u32, Vec<[u32; 4]>>::new();
        for instructions in 1..=65536 {
            let address = self.pc;
            let word = self.fetch(mem)?;
            let result = if word & 0xff800000 == 0xbf800000 {
                match (word >> 16) & 127 {
                    1 if word & 0xffff == 0 => return Ok((exports, instructions)),
                    0 | 12 => Ok(()),
                    opcode => Err(format!("GCN vertex SOPP opcode {opcode:#x} is not implemented")),
                }
            } else if word & 0xff800000 == 0xbe800000 {
                if matches!((word >> 8) & 255, 0x20 | 0x21) { self.scalar_program_counter(word) } else { self.scalar_instruction(mem, word) }
            } else if word & 0xf8000000 == 0xc0000000 {
                self.scalar_memory(mem, word)
            } else if word & 0xfe000000 == 0x7e000000 {
                self.raster_vop1(mem, word)
            } else if word & 0xfe000000 == 0x7c000000 {
                self.raster_compare(mem, word)
            } else if word & 0xfc000000 == 0xe0000000 {
                self.vertex_buffer(mem, word)
            } else if word & 0xfe000000 == 0xf8000000 {
                let source = self.fetch(mem)?;
                let target = (word >> 4) & 63;
                if word & 0x03ffe400 != 0 || (target != 12 && !(32..64).contains(&target)) {
                    Err("GCN vertex export target or compression is not implemented".into())
                } else {
                    let values = exports.entry(target).or_insert_with(|| vec![[0; 4]; 64]);
                    for lane in 0..64 {
                        if !self.active(lane) { continue; }
                        for component in 0..4 {
                            if word & (1 << component) != 0 {
                                let register = ((source >> (component * 8)) & 255) as usize;
                                values[lane][component] = self.vector[register * 64 + lane];
                            }
                        }
                    }
                    Ok(())
                }
            } else if word >> 31 == 0 {
                self.raster_vop2(mem, word)
            } else { Err(format!("GCN vertex instruction {word:#010x} is not implemented")) };
            result.map_err(|error| format!("{error}; pc={address:#x} word={word:#010x}"))?;
        }
        Err("GCN vertex instruction budget exceeded".into())
    }
}

pub(crate) fn execute(mem: &mut Mem, registers: &BTreeMap<u32, u32>, indices: &[u32]) -> Result<(Vec<Vertex>, u64), String> {
    if indices.len() > 4096 { return Err("GCN indexed draw exceeds the vertex limit".into()); }
    let register = |address| registers.get(&address).copied().unwrap_or(0);
    let program = (register(0x2c48) as u64) << 8 | (register(0x2c49) as u64) << 40;
    if program == 0 { return Err("GCN vertex program is not bound".into()); }
    let resources = register(0x2c4b);
    let users = (resources >> 1) & 31;
    if users > 16 || resources & !0x3e != 0 { return Err("GCN vertex scratch or additional system inputs are not implemented".into()); }
    let mut output = Vec::with_capacity(indices.len());
    let mut instructions = 0;
    for chunk in indices.chunks(64) {
        let mut wave = Wave { scalar: [0; 128], vector: vec![0; 256 * 64], pc: program };
        for index in 0..users { wave.scalar[index as usize] = register(0x2c4c + index); }
        let mask = if chunk.len() == 64 { u64::MAX } else { (1u64 << chunk.len()) - 1 };
        wave.scalar[126] = mask as u32;
        wave.scalar[127] = (mask >> 32) as u32;
        wave.vector[..chunk.len()].copy_from_slice(chunk);
        let (exports, count) = wave.vertex_program(mem)?;
        instructions += count;
        let positions = exports.get(&12).ok_or("GCN vertex program did not export a position")?;
        for lane in 0..chunk.len() {
            let position = positions[lane].map(f32::from_bits);
            if position.iter().any(|value| !value.is_finite()) { return Err("GCN vertex exported a non-finite position".into()); }
            let attributes = exports.range(32..64).map(|(target, values)| (target - 32, values[lane].map(f32::from_bits))).collect();
            output.push(Vertex { position, attributes });
        }
    }
    Ok((output, instructions))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wave() -> Wave { Wave { scalar: [0; 128], vector: vec![0; 256 * 64], pc: 0x50000000 } }

    #[test]
    fn captured_scalar_loads_use_dword_immediates_and_do_not_partially_overwrite_registers() {
        let mut mem = Mem::new().unwrap();
        mem.map(0x50000000, 0x3000, true, false).unwrap();
        let values = (0u32..16).collect::<Vec<_>>();
        mem.write(0x50002000, &values.iter().flat_map(|value| value.to_le_bytes()).collect::<Vec<_>>());
        let mut wave = wave();
        wave.scalar[4..8].copy_from_slice(&[0x50002000, 0x107000, 4, 0x20077fac]);
        wave.scalar_memory(&mem, 0xc284050c).unwrap();
        assert_eq!(&wave.scalar[8..12], &[12, 13, 14, 15]);
        wave.scalar_memory(&mem, 0xc2c00500).unwrap();
        assert_eq!(&wave.scalar[..8], &[0, 1, 2, 3, 4, 5, 6, 7]);
        wave.scalar[4..8].copy_from_slice(&[0x50002000, 0x107000, 4, 0x20077fac]);
        assert!(wave.scalar_memory(&mem, 0xc284050d).is_err());
        assert_eq!(&wave.scalar[8..12], &[12, 13, 14, 15]);
    }

    #[test]
    fn fetch_shader_calls_preserve_the_return_address_when_source_and_destination_alias() {
        let mut wave = wave();
        wave.pc = 0x5000010c;
        wave.scalar[0] = 0x50000200;
        wave.scalar_program_counter(0xbe802100).unwrap();
        assert_eq!(wave.pc, 0x50000200);
        assert_eq!(wave.scalar[0], 0x5000010c);
        wave.scalar_program_counter(0xbe802000).unwrap();
        assert_eq!(wave.pc, 0x5000010c);
    }

    #[test]
    fn captured_float_operations_respect_literals_masks_and_inactive_lanes() {
        let mut mem = Mem::new().unwrap();
        mem.map(0x50000000, 4096, true, false).unwrap();
        let mut wave = wave();
        wave.scalar[126] = 1;
        wave.vector[10 * 64] = 0.25f32.to_bits();
        wave.vector[10 * 64 + 1] = 0xdeadbeef;
        wave.raster_vop1(&mem, 0x7e144f0a).unwrap();
        assert_eq!(f32::from_bits(wave.vector[10 * 64]), -2.0);
        wave.raster_vop1(&mem, 0x7e144b0a).unwrap();
        assert_eq!(f32::from_bits(wave.vector[10 * 64]), 0.25);
        assert_eq!(wave.vector[10 * 64 + 1], 0xdeadbeef);
        wave.vector[2 * 64] = 2.0f32.to_bits();
        mem.write(0x50000000, &0.5f32.to_le_bytes());
        wave.raster_vop2(&mem, 0x42141502).unwrap();
        assert_eq!(f32::from_bits(wave.vector[10 * 64]), 1.0);
        assert_eq!(wave.pc, 0x50000004);
        wave.scalar[12] = 0.5f32.to_bits();
        wave.raster_compare(&mem, 0x7c02140c).unwrap();
        assert_eq!(wave.scalar[106], 1);
        assert_eq!(wave.scalar[107], 0);
    }

    #[test]
    fn captured_fetch_loads_use_float_formats_interleaved_stride_and_record_bounds() {
        let mut mem = Mem::new().unwrap();
        mem.map(0x50000000, 0x3000, true, false).unwrap();
        let values = (0..3).flat_map(|record| (0..16).map(move |component| (record * 10 + component) as f32)).flat_map(|value| value.to_le_bytes()).collect::<Vec<_>>();
        mem.write(0x50000000, &values);
        let mut wave = wave();
        wave.pc = 0x50001000;
        wave.scalar[126] = 7;
        wave.scalar[8..12].copy_from_slice(&[0x50000000, 0x407000, 3, 0x20077fac]);
        wave.scalar[12..16].copy_from_slice(&[0x50000010, 0x407000, 3, 0x2005f22c]);
        wave.vector[..3].copy_from_slice(&[2, 1, 3]);
        wave.vector[4 * 64 + 3] = 0xdeadbeef;
        mem.write(0x50001000, &0x80020400u32.to_le_bytes());
        wave.vertex_buffer(&mem, 0xe00c2000).unwrap();
        for component in 0..4 {
            assert_eq!(f32::from_bits(wave.vector[(4 + component) * 64]), (20 + component) as f32);
            assert_eq!(f32::from_bits(wave.vector[(4 + component) * 64 + 1]), (10 + component) as f32);
            assert_eq!(wave.vector[(4 + component) * 64 + 2], 0);
        }
        assert_eq!(wave.vector[4 * 64 + 3], 0xdeadbeef);
        wave.pc = 0x50001000;
        mem.write(0x50001000, &0x80030800u32.to_le_bytes());
        wave.vertex_buffer(&mem, 0xe0042000).unwrap();
        assert_eq!(f32::from_bits(wave.vector[8 * 64]), 24.0);
        assert_eq!(f32::from_bits(wave.vector[9 * 64]), 25.0);
        wave.scalar[8..12].copy_from_slice(&[0x50000100, 0x407000, 1, 0x20050f2e]);
        wave.scalar[126] = 1;
        wave.vector[0] = 0;
        wave.pc = 0x50001000;
        mem.write(0x50000100, &[0, 64, 128, 255]);
        mem.write(0x50001000, &0x80021800u32.to_le_bytes());
        wave.vertex_buffer(&mem, 0xe00c2000).unwrap();
        for (component, expected) in [128.0 / 255.0, 64.0 / 255.0, 0.0, 1.0].iter().enumerate() {
            assert_eq!(f32::from_bits(wave.vector[(24 + component) * 64]), *expected);
        }
    }

    #[test]
    #[ignore]
    fn real_ot11_vertex_shader_matches_the_captured_projection() {
        let snapshot = fixtures::raster_snapshot();
        let root = snapshot.root;
        let mut mem = snapshot.mem;
        let registers = snapshot.registers;
        let indices = snapshot.indices;
        let code = snapshot.vertex_code;
        let (vertices, instructions) = execute(&mut mem, &registers, &indices).unwrap();
        let matrix = std::fs::read(root.join("raster-vertex-s4-buffer.bin")).unwrap();
        let matrix = matrix.chunks_exact(4).map(|word| f32::from_le_bytes(word.try_into().unwrap())).collect::<Vec<_>>();
        assert_eq!(matrix.len(), 16);
        for (vertex, index) in vertices.iter().zip(indices) {
            let record = std::fs::read(root.join(format!("raster-vertex-s2-table-buffer-2-record-{index}.bin"))).unwrap();
            let x = f32::from_le_bytes(record[..4].try_into().unwrap());
            let y = f32::from_le_bytes(record[4..8].try_into().unwrap());
            for component in 0..4 {
                let expected = matrix[component] * x + (matrix[4 + component] * y + matrix[12 + component]);
                assert_eq!(vertex.position[component].to_bits(), expected.to_bits());
            }
            assert_eq!(vertex.attributes.len(), 6);
        }
        println!("authentic_vertex_shader_bytes={} vertex_count={} wave_instructions={} positions={:?} projection_verified=true rendering_verified=false", code.len(), vertices.len(), instructions, vertices.iter().map(|vertex| vertex.position).collect::<Vec<_>>());
    }

    #[test]
    fn vertex_program_exports_positions_and_attributes_without_fabricating_pixels() {
        let mut mem = Mem::new().unwrap();
        mem.map(0x50000000, 4096, true, false).unwrap();
        let code = [0x7e0202f2u32, 0x7e040280, 0xf80008cf, 0x01020200, 0xf800020f, 0x01020200, 0xbf810000];
        mem.write(0x50000000, &code.iter().flat_map(|word| word.to_le_bytes()).collect::<Vec<_>>());
        let registers = BTreeMap::from([(0x2c48, 0x500000)]);
        let (vertices, instructions) = execute(&mut mem, &registers, &[0.5f32.to_bits(), 1.0f32.to_bits()]).unwrap();
        assert_eq!(instructions, 5);
        assert_eq!(vertices[0].position, [0.5, 0.0, 0.0, 1.0]);
        assert_eq!(vertices[1].position, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(vertices[0].attributes[&0], vertices[0].position);
        mem.write(0x50000000, &0xffffffffu32.to_le_bytes());
        assert!(execute(&mut mem, &registers, &[0]).unwrap_err().contains("not implemented"));
    }
}
