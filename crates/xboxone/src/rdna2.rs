//! Single-lane execution of a small GFX10.3 pixel shader subset for software draw tests.

pub(crate) const MAX_SHADER_BYTES: usize = 64 * 4;

const VOP1_MOV_B32: u32 = 0x7E00_0200;
const EXP_MRT0_RGBA_DONE_VM: u32 = 0xF800_180F;
const S_ENDPGM: u32 = 0xBF81_0000;

enum Source {
    Vector(u8),
    Immediate(u32),
}

impl Source {
    fn decode(
        selector: u32,
        words: &mut impl Iterator<Item = u32>,
        initialized: &[bool; 256],
    ) -> Result<Self, String> {
        let bits = match selector {
            128..=192 => selector - 128,
            193..=208 => (192 - selector as i32) as u32,
            240..=247 => {
                let constants: [f32; 8] = [0.5, -0.5, 1.0, -1.0, 2.0, -2.0, 4.0, -4.0];
                constants[(selector - 240) as usize].to_bits()
            }
            255 => words.next().ok_or("Truncated RDNA2 literal operand.")?,
            256..=511 => {
                let vector = selector as u8;
                require_vector(initialized, vector)?;
                return Ok(Self::Vector(vector));
            }
            _ => return Err(format!("Unsupported RDNA2 source selector {selector:#x}.")),
        };
        Ok(Self::Immediate(bits))
    }

    fn bits(&self, vectors: &[u32; 256]) -> u32 {
        match *self {
            Self::Vector(index) => vectors[index as usize],
            Self::Immediate(bits) => bits,
        }
    }
}

fn require_vector(initialized: &[bool; 256], vector: u8) -> Result<(), String> {
    if !initialized[vector as usize] {
        return Err(format!("RDNA2 shader reads uninitialized v{vector}."));
    }
    Ok(())
}

enum Instruction {
    Move(u8, Source),
    Float(FloatOp, u8, Source, u8),
}

enum FloatOp {
    Add,
    Subtract,
    ReverseSubtract,
    Multiply,
    FusedMultiplyAccumulate,
}

pub(crate) struct PixelShader {
    instructions: Vec<Instruction>,
    output: [u8; 4],
}

impl PixelShader {
    pub(crate) fn decode(code: &[u8]) -> Result<Self, String> {
        if code.is_empty() || code.len() > MAX_SHADER_BYTES || code.len() % 4 != 0 {
            return Err("RDNA2 shader must contain 1 to 64 complete dwords.".into());
        }
        let mut words = code
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()));
        let mut initialized = [false; 256];
        initialized[..4].fill(true);
        let mut instructions = Vec::new();

        while let Some(word) = words.next() {
            if word == EXP_MRT0_RGBA_DONE_VM {
                let output = words
                    .next()
                    .ok_or("Truncated RDNA2 color export.")?
                    .to_le_bytes();
                for register in output {
                    require_vector(&initialized, register)?;
                }
                if words.next() != Some(S_ENDPGM) || words.next().is_some() {
                    return Err("RDNA2 color export must be followed by the final s_endpgm.".into());
                }
                return Ok(Self {
                    instructions,
                    output,
                });
            }
            let dest = (word >> 17) as u8;
            let instruction = if word & 0xFE01_FE00 == VOP1_MOV_B32 {
                let src = Source::decode(word & 0x1FF, &mut words, &initialized)?;
                Instruction::Move(dest, src)
            } else {
                // VOP2 places its seven-bit opcode above the destination register.
                let op = match word >> 25 {
                    3 => FloatOp::Add,
                    4 => FloatOp::Subtract,
                    5 => FloatOp::ReverseSubtract,
                    8 => FloatOp::Multiply,
                    43 => FloatOp::FusedMultiplyAccumulate,
                    _ => {
                        return Err(format!(
                            "Unsupported RDNA2 pixel shader instruction {word:#010x}."
                        ));
                    }
                };
                if matches!(op, FloatOp::FusedMultiplyAccumulate) {
                    require_vector(&initialized, dest)?;
                }
                let rhs = (word >> 9) as u8;
                require_vector(&initialized, rhs)?;
                let lhs = Source::decode(word & 0x1FF, &mut words, &initialized)?;
                Instruction::Float(op, dest, lhs, rhs)
            };
            instructions.push(instruction);
            initialized[dest as usize] = true;
        }
        Err("RDNA2 pixel shader is missing its color export.".into())
    }

    /// The private rectangle ABI supplies normalized pixel-center coordinates in v0/v1,
    /// zero in v2 and one in v3. Arithmetic uses host f32 without guest MODE state.
    pub(crate) fn shade_rgba8(&self, u: f32, v: f32) -> [u8; 4] {
        let mut vectors = [0u32; 256];
        vectors[..4].copy_from_slice(&[u.to_bits(), v.to_bits(), 0, 1.0f32.to_bits()]);
        for instruction in &self.instructions {
            match instruction {
                Instruction::Move(dest, source) => vectors[*dest as usize] = source.bits(&vectors),
                Instruction::Float(op, dest, lhs, rhs) => {
                    let lhs = f32::from_bits(lhs.bits(&vectors));
                    let rhs = f32::from_bits(vectors[*rhs as usize]);
                    let dest = *dest as usize;
                    let result = match op {
                        FloatOp::Add => lhs + rhs,
                        FloatOp::Subtract => lhs - rhs,
                        FloatOp::ReverseSubtract => rhs - lhs,
                        FloatOp::Multiply => lhs * rhs,
                        FloatOp::FusedMultiplyAccumulate => {
                            lhs.mul_add(rhs, f32::from_bits(vectors[dest]))
                        }
                    };
                    vectors[dest] = result.to_bits();
                }
            }
        }
        self.output.map(|register| {
            let value = f32::from_bits(vectors[register as usize]);
            (value.clamp(0.0, 1.0) * 255.0).round_ties_even() as u8
        })
    }
}

#[cfg(test)]
mod tests {
    use super::PixelShader;

    #[test]
    fn multiply_accumulate_rounds_only_after_the_addition() {
        let words: [u32; 7] = [
            0x7E08_02F3, // v_mov_b32_e32 v4, -1.0
            0x5608_0300, // v_fmac_f32_e32 v4, v0, v1
            0x1008_08FF, // v_mul_f32_e32 v4, literal, v4
            0xD680_0000, // -2^46
            0xF800_180F, // exp mrt0, v4, v2, v2, v3 done vm
            0x0302_0204,
            0xBF81_0000, // s_endpgm
        ];
        let code: Vec<_> = words.into_iter().flat_map(u32::to_le_bytes).collect();
        let shader = PixelShader::decode(&code).unwrap();
        let offset = f32::EPSILON;
        // The exact product is 1 - 2^-46. Rounding it before subtracting 1 would erase the result.
        assert_eq!(
            shader.shade_rgba8(1.0 + offset, 1.0 - offset),
            [255, 0, 0, 255]
        );
    }
}
