use super::*;
use super::vertex::Vertex;

struct PixelWave {
    wave: Wave,
    scc: bool,
    inputs: Vec<[[f32; 4]; 3]>,
    colours: Vec<Option<[f32; 4]>>,
}

impl PixelWave {
    fn pair(&self, source: u32) -> Result<u64, String> {
        match source {
            0..=126 => Ok(self.wave.scalar[source as usize] as u64 | (self.wave.scalar[source as usize + 1] as u64) << 32),
            128..=192 => Ok((source - 128) as u64),
            193..=208 => Ok((-(source as i64 - 192)) as u64),
            _ => Err("GCN pixel scalar pair source is not implemented".into()),
        }
    }

    fn scalar(&mut self, mem: &Mem, word: u32) -> Result<(), String> {
        let destination = ((word >> 16) & 127) as usize;
        if word & 0xff800000 == 0xbe800000 {
            let opcode = (word >> 8) & 255;
            if opcode == 3 { return self.wave.scalar_instruction(mem, word); }
            if !matches!(opcode, 4 | 10) || destination >= 127 { return Err(format!("GCN pixel SOP1 opcode {opcode:#x} is not implemented")); }
            let mut value = self.pair(word & 255)?;
            if opcode == 10 {
                let original = value;
                for group in (0..64).step_by(4) {
                    if original & (15u64 << group) != 0 { value |= 15u64 << group; }
                }
                self.scc = value != 0;
            }
            self.wave.scalar[destination] = value as u32;
            self.wave.scalar[destination + 1] = (value >> 32) as u32;
        } else {
            let opcode = (word >> 23) & 127;
            if opcode != 0x15 || destination >= 127 { return Err(format!("GCN pixel SOP2 opcode {opcode:#x} is not implemented")); }
            let value = self.pair(word & 255)? & !self.pair((word >> 8) & 255)?;
            self.wave.scalar[destination] = value as u32;
            self.wave.scalar[destination + 1] = (value >> 32) as u32;
            self.scc = value != 0;
        }
        Ok(())
    }

    fn interpolate(&mut self, word: u32) -> Result<(), String> {
        let opcode = (word >> 16) & 3;
        if opcode > 1 { return Err("GCN pixel interpolation opcode is not implemented".into()); }
        let source = (word & 255) as usize;
        let channel = ((word >> 8) & 3) as usize;
        let attribute = ((word >> 10) & 63) as usize;
        let destination = ((word >> 18) & 255) as usize;
        let values = self.inputs.get(attribute).ok_or("GCN pixel interpolation input is unavailable")?;
        for lane in 0..64 {
            if !self.wave.active(lane) { continue; }
            let weight = f32::from_bits(self.wave.vector[source * 64 + lane]);
            let value = if opcode == 0 {
                values[0][channel] + weight * (values[1][channel] - values[0][channel])
            } else {
                f32::from_bits(self.wave.vector[destination * 64 + lane]) + weight * (values[2][channel] - values[0][channel])
            };
            self.wave.vector[destination * 64 + lane] = value.to_bits();
        }
        Ok(())
    }

    fn vector_three(&mut self, mem: &Mem, first: u32) -> Result<(), String> {
        let second = self.wave.fetch(mem)?;
        let opcode = (first >> 17) & 511;
        if !matches!(opcode, 13 | 0x108 | 0x11f | 0x141 | 0x151 | 0x1a7) || first & 0x1f800 != 0 || second & 0x18000000 != 0 { return Err(format!("GCN pixel VOP3 opcode {opcode:#x} or output modifiers are not implemented")); }
        let destination = (first & 255) as usize;
        if opcode == 13 && destination >= 127 { return Err("GCN pixel comparison destination is invalid".into()); }
        let sources = [second & 511, (second >> 9) & 511, (second >> 18) & 511];
        let mut mask = 0u64;
        for lane in 0..64 {
            if !self.wave.active(lane) { continue; }
            let mut values = [0.0f32; 3];
            for component in 0..3 {
                let mut bits = self.wave.raster_source(sources[component], lane, None)?;
                if first & (1 << (8 + component)) != 0 { bits &= 0x7fffffff; }
                if second & (1 << (29 + component)) != 0 { bits ^= 0x80000000; }
                values[component] = f32::from_bits(bits);
            }
            if opcode == 13 {
                if values[0] != values[1] { mask |= 1u64 << lane; }
                continue;
            }
            let value = match opcode {
                0x108 => values[0] * values[1],
                0x11f => values[0] * values[1] + f32::from_bits(self.wave.vector[destination * 64 + lane]),
                0x141 => values[0] * values[1] + values[2],
                0x151 => values[0].min(values[1]).min(values[2]),
                _ => values[0].log2(),
            };
            self.wave.vector[destination * 64 + lane] = value.to_bits();
        }
        if opcode == 13 {
            self.wave.scalar[destination] = mask as u32;
            self.wave.scalar[destination + 1] = (mask >> 32) as u32;
        }
        Ok(())
    }

    fn sample(&mut self, mem: &Mem, first: u32) -> Result<(), String> {
        let second = self.wave.fetch(mem)?;
        if first != 0xf0800f00 || second & 0xfc000000 != 0 { return Err("GCN image sampling mode is not implemented".into()); }
        let resource = ((second >> 16) & 31) as usize * 4;
        let sampler = ((second >> 21) & 31) as usize * 4;
        if resource + 8 > 128 || sampler + 4 > 128 { return Err("GCN image resource register range is invalid".into()); }
        let descriptor = &self.wave.scalar[resource..resource + 8];
        let sampler = &self.wave.scalar[sampler..sampler + 4];
        let width = (descriptor[2] & 0x3fff) + 1;
        let height = ((descriptor[2] >> 14) & 0x3fff) + 1;
        if width != 1 || height != 1 || descriptor[3] >> 28 != 9 || (descriptor[1] >> 20) & 63 != 10 || (descriptor[1] >> 26) & 15 != 0 || descriptor[3] & 0x000ff000 != 0 || descriptor[5] != 0 || descriptor[6] & (1 << 21) != 0 {
            return Err("GCN texture sampling currently supports only uncompressed single-level 1x1 RGBA8 UNORM textures".into());
        }
        if !matches!(sampler[0] & 7, 0 | 2) || !matches!((sampler[0] >> 3) & 7, 0 | 2) { return Err("GCN texture sampler address mode is not implemented".into()); }
        let address = (descriptor[0] as u64 | ((descriptor[1] & 63) as u64) << 32) << 8;
        let mut texel = [0; 4];
        if !mem.read(address, &mut texel) { return Err(format!("GCN texture is unmapped at {address:#x}")); }
        let channels = texel.map(|value| value as f32 / 255.0);
        let destination = ((second >> 8) & 255) as usize;
        let coordinates = (second & 255) as usize;
        if destination > 252 || coordinates > 254 { return Err("GCN texture vector register range is invalid".into()); }
        let selections = (0..4).map(|component| (descriptor[3] >> (component * 3)) & 7).collect::<Vec<_>>();
        if selections.iter().any(|value| !matches!(value, 0 | 1 | 4..=7)) { return Err("GCN texture channel selection is invalid".into()); }
        for lane in 0..64 {
            if !self.wave.active(lane) { continue; }
            for component in 0..2 {
                if !f32::from_bits(self.wave.vector[(coordinates + component) * 64 + lane]).is_finite() { return Err("GCN texture coordinate is non-finite".into()); }
            }
            for (component, selection) in selections.iter().enumerate() {
                let value = match selection { 0 => 0.0, 1 => 1.0, _ => channels[*selection as usize - 4] };
                self.wave.vector[(destination + component) * 64 + lane] = value.to_bits();
            }
        }
        Ok(())
    }

    fn export(&mut self, mem: &Mem, first: u32) -> Result<(), String> {
        let second = self.wave.fetch(mem)?;
        if first & !0x1c0f != 0xf8000000 || !matches!(first & 15, 0 | 15) || first & (1 << 10) == 0 { return Err("GCN pixel export mode is not implemented".into()); }
        if first & 15 == 0 { return Ok(()); }
        for lane in 0..64 {
            if !self.wave.active(lane) { continue; }
            let left = self.wave.vector[(second as usize & 255) * 64 + lane];
            let right = self.wave.vector[((second as usize >> 8) & 255) * 64 + lane];
            let colour = [left as u16, (left >> 16) as u16, right as u16, (right >> 16) as u16].map(half::from_half);
            if colour.iter().any(|value| !value.is_finite()) { return Err("GCN pixel export is non-finite".into()); }
            self.colours[lane] = Some(colour);
        }
        Ok(())
    }
}

impl PixelWave {
    fn run(&mut self, mem: &Mem) -> Result<u64, String> {
        for instructions in 1..=65536 {
            let address = self.wave.pc;
            let word = self.wave.fetch(mem)?;
            let result = if word & 0xff800000 == 0xbf800000 {
                let opcode = (word >> 16) & 127;
                if opcode == 1 && word & 0xffff == 0 { return Ok(instructions); }
                let branch = match opcode {
                    0 | 12 => false,
                    2 => true,
                    4 => !self.scc,
                    5 => self.scc,
                    6 => self.pair(106)? == 0,
                    7 => self.pair(106)? != 0,
                    8 => self.pair(126)? == 0,
                    9 => self.pair(126)? != 0,
                    _ => return Err(format!("GCN pixel SOPP opcode {opcode:#x} is not implemented; pc={address:#x}")),
                };
                if branch { self.wave.pc = self.wave.pc.checked_add_signed((word as i16 as i64) * 4).ok_or("GCN pixel branch address overflow")?; }
                Ok(())
            } else if word & 0xff800000 == 0xbe800000 || word & 0xc0000000 == 0x80000000 {
                self.scalar(mem, word)
            } else if word & 0xf8000000 == 0xc0000000 {
                self.wave.scalar_memory(mem, word)
            } else if word & 0xfc000000 == 0xc8000000 {
                self.interpolate(word)
            } else if word & 0xfc000000 == 0xd0000000 {
                self.vector_three(mem, word)
            } else if word & 0xfc000000 == 0xf0000000 {
                self.sample(mem, word)
            } else if word & 0xfe000000 == 0xf8000000 {
                self.export(mem, word)
            } else if word & 0xfe000000 == 0x7e000000 {
                self.wave.raster_vop1(mem, word)
            } else if word & 0xfe000000 == 0x7c000000 {
                self.wave.raster_compare(mem, word)
            } else if word >> 31 == 0 {
                self.wave.raster_vop2(mem, word)
            } else { Err("GCN pixel instruction encoding is not implemented".into()) };
            result.map_err(|error| format!("{error}; pc={address:#x} word={word:#010x}"))?;
        }
        Err("GCN pixel instruction budget exceeded".into())
    }
}

pub(crate) fn execute(mem: &Mem, registers: &BTreeMap<u32, u32>, triangle: [&Vertex; 3], barycentrics: &[[f32; 2]]) -> Result<(Vec<Option<[f32; 4]>>, u64), String> {
    if barycentrics.len() > 64 || barycentrics.iter().flatten().any(|value| !value.is_finite()) { return Err("GCN pixel batch or barycentric values are invalid".into()); }
    if barycentrics.is_empty() { return Ok((Vec::new(), 0)); }
    let register = |address| registers.get(&address).copied().unwrap_or(0);
    let program = (register(0x2c08) as u64) << 8 | (register(0x2c09) as u64) << 40;
    if program == 0 { return Err("GCN pixel program is not bound".into()); }
    let resources = register(0x2c0b);
    let users = (resources >> 1) & 31;
    if users > 16 || resources & !0x3e != 0 || register(0xa1b3) != 2 || register(0xa1b4) != 2 { return Err("GCN pixel scratch or additional system inputs are not implemented".into()); }
    let count = register(0xa1b6);
    if count > 32 { return Err("GCN pixel input layout is not implemented".into()); }
    let mut inputs = Vec::new();
    for input in 0..count {
        let offset = register(0xa191 + input);
        if offset > 31 { return Err("GCN flat or default pixel inputs are not implemented".into()); }
        let mut values = [[0.0; 4]; 3];
        for vertex in 0..3 { values[vertex] = *triangle[vertex].attributes.get(&offset).ok_or("GCN pixel input has no vertex export")?; }
        inputs.push(values);
    }
    let mut wave = Wave { scalar: [0; 128], vector: vec![0; 256 * 64], pc: program };
    for index in 0..users { wave.scalar[index as usize] = register(0x2c0c + index); }
    let mask = if barycentrics.len() == 64 { u64::MAX } else { (1u64 << barycentrics.len()) - 1 };
    wave.scalar[126] = mask as u32;
    wave.scalar[127] = (mask >> 32) as u32;
    for (lane, values) in barycentrics.iter().enumerate() {
        wave.vector[lane] = values[0].to_bits();
        wave.vector[64 + lane] = values[1].to_bits();
    }
    let mut execution = PixelWave { wave, scc: false, inputs, colours: vec![None; 64] };
    let instructions = execution.run(mem)?;
    execution.colours.truncate(barycentrics.len());
    Ok((execution.colours, instructions))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel_wave() -> PixelWave {
        PixelWave { wave: Wave { scalar: [0; 128], vector: vec![0; 256 * 64], pc: 0x50000000 }, scc: false, inputs: Vec::new(), colours: vec![None; 64] }
    }

    #[test]
    fn quad_expansion_and_discard_preserve_upper_lanes_and_restore_exact_coverage() {
        let mem = Mem::new().unwrap();
        let mut execution = pixel_wave();
        execution.wave.scalar[126] = 1;
        execution.wave.scalar[127] = 1 << 5;
        execution.scalar(&mem, 0xbe90047e).unwrap();
        execution.scalar(&mem, 0xbefe0a7e).unwrap();
        assert_eq!(execution.pair(126).unwrap(), 15 | (15u64 << 36));
        execution.wave.scalar[106] = 0;
        execution.wave.scalar[107] = 1 << 5;
        execution.scalar(&mem, 0x8a906a10).unwrap();
        assert_eq!(execution.pair(16).unwrap(), 1);
        assert!(execution.scc);
        execution.scalar(&mem, 0xbefe0410).unwrap();
        assert_eq!(execution.pair(126).unwrap(), 1);
        execution.wave.scalar[106] = 1;
        execution.wave.scalar[107] = 0;
        execution.scalar(&mem, 0x8a906a10).unwrap();
        assert!(!execution.scc);
        execution.scalar(&mem, 0xbefe0480).unwrap();
        assert_eq!(execution.pair(126).unwrap(), 0);
    }

    #[test]
    fn interpolation_and_vop3_modifiers_match_independent_float_results() {
        let mut execution = pixel_wave();
        execution.inputs.push([[1.0; 4], [5.0; 4], [9.0; 4]]);
        execution.wave.scalar[126] = 1;
        execution.wave.vector[0] = 0.25f32.to_bits();
        execution.wave.vector[64] = 0.5f32.to_bits();
        execution.interpolate(0xc8080000).unwrap();
        execution.interpolate(0xc8090001).unwrap();
        assert_eq!(f32::from_bits(execution.wave.vector[2 * 64]), 6.0);
        let mut mem = Mem::new().unwrap();
        mem.map(0x50000000, 4096, true, false).unwrap();
        execution.wave.vector[8 * 64] = 0.25f32.to_bits();
        execution.wave.vector[3 * 64] = 0.5f32.to_bits();
        mem.write(0x50000000, &0x23ca0708u32.to_le_bytes());
        execution.vector_three(&mem, 0xd2820006).unwrap();
        assert_eq!(f32::from_bits(execution.wave.vector[6 * 64]), 0.875);
        execution.wave.pc = 0x50000000;
        execution.wave.vector[2 * 64] = (-0.25f32).to_bits();
        mem.write(0x50000000, &0x00000102u32.to_le_bytes());
        execution.vector_three(&mem, 0xd34e0102).unwrap();
        assert_eq!(f32::from_bits(execution.wave.vector[2 * 64]), -2.0);
    }

    #[test]
    #[ignore]
    fn real_ot11_pixel_shader_shades_interior_and_discards_clipped_fragments() {
        let mut snapshot = fixtures::raster_snapshot();
        assert_eq!(std::fs::metadata(snapshot.root.join("pixel-shader-complete.bin")).unwrap().len(), 548);
        let (mut vertices, _) = vertex::execute(&mut snapshot.mem, &snapshot.registers, &snapshot.indices).unwrap();
        let barycentrics = [[0.25, 0.25], [0.5, 0.25], [-0.1, -0.1], [1.2, 0.0]];
        let (colours, instructions) = execute(&snapshot.mem, &snapshot.registers, [&vertices[0], &vertices[1], &vertices[2]], &barycentrics).unwrap();
        println!("authentic_pixel_shader_bytes=548 wave_instructions={instructions} fragment_results={colours:?} rendering_verified=false");
        let shader_black_floor = half::from_half(0x0400);
        assert_eq!(half::to_half_rtz(f32::from_bits(0x38800006)), 0x0400);
        let black = Some([shader_black_floor, shader_black_floor, shader_black_floor, 1.0]);
        assert_eq!(colours, [black, black, None, None]);
        let (discarded, _) = execute(&snapshot.mem, &snapshot.registers, [&vertices[0], &vertices[1], &vertices[2]], &[[-0.1, -0.1]]).unwrap();
        assert_eq!(discarded, [None]);
        assert_eq!((shader_black_floor * 255.0).round() as u8, 0);
        for vertex in &mut vertices { vertex.attributes.insert(0, [0.25, 0.125, 0.75, 0.5]); }
        let (colours, _) = execute(&snapshot.mem, &snapshot.registers, [&vertices[0], &vertices[1], &vertices[2]], &[[0.25, 0.25]]).unwrap();
        let expected = [0.25f32, 0.125, 0.75].map(|value| 1.055 * value.powf(1.0 / 2.4) - 0.055);
        let actual = colours[0].unwrap();
        println!("controlled_linear_colour={actual:?} srgb_reference={expected:?}");
        for component in 0..3 { assert!((actual[component] - expected[component]).abs() < 0.001); }
        assert_eq!(actual[3], 0.5);
        let descriptor = 0x2c18;
        let constants = snapshot.registers[&descriptor] as u64 | ((snapshot.registers[&(descriptor + 1)] & 0xfff) as u64) << 32;
        snapshot.mem.write(constants + 4, &1.0f32.to_le_bytes());
        let (colours, _) = execute(&snapshot.mem, &snapshot.registers, [&vertices[0], &vertices[1], &vertices[2]], &[[0.25, 0.25]]).unwrap();
        assert_eq!(colours[0], Some([0.25, 0.125, 0.75, 0.5]));
        snapshot.registers.entry(0x2c0e).and_modify(|value| *value |= 1);
        let error = execute(&snapshot.mem, &snapshot.registers, [&vertices[0], &vertices[1], &vertices[2]], &[[0.25, 0.25]]).unwrap_err();
        assert!(error.contains("1x1 RGBA8"), "{error}");
    }
}
