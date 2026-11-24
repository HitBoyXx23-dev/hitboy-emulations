//! Clean-room AMD RDNA 2 (GFX10.3 — Xbox Series X|S GPU) shader disassembler & HLSL translator.
//!
//! Written strictly from AMD's public RDNA 2 Instruction Set Architecture Reference Guide.
//! Decodes SOP2, SOP1, SOPC, SOPK, SOPP, VOP1, VOP2, VOP3, VOPC, SMEM, MUBUF/MTBUF,
//! FLAT/GLOBAL, and EXP instructions, classifies shader stages/features, and emits
//! readable disassembly + HLSL with grouped execution-mask blocks (avoiding per-op branches).

/// High-level RDNA2 encoding family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Sop2,
    Sop1,
    Sopc,
    Sopk,
    Sopp,
    Vop1,
    Vop2,
    Vop3,
    Vopc,
    Smem,
    Mubuf,
    Mtbuf,
    Exp,
    Flat,
    Unknown,
}

/// Decoded single RDNA2 instruction (1, 2, or 3 dwords including optional 32-bit literal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedInst {
    pub offset: usize,
    pub size_bytes: usize,
    pub encoding: Encoding,
    pub mnemonic: String,
    pub operands: String,
    pub raw: u32,
}

/// Summary report from analyzing a compiled Xbox Series X|S RDNA2 shader blob.
#[derive(Debug, Clone, Default)]
pub struct ShaderAnalysis {
    pub total_dwords: usize,
    pub instructions: Vec<DecodedInst>,
    pub unknown_count: usize,
    pub uses_exports: bool,
    pub uses_smem: bool,
    pub uses_vmem: bool,
    pub uses_exec_mask: bool,
    pub ended_cleanly: bool,
    pub hlsl: String,
}

fn ssrc_name(s: u32) -> String {
    match s {
        0..=105 => format!("s{s}"),
        106 => "vcc_lo".into(),
        107 => "vcc_hi".into(),
        124 => "null".into(),
        125 => "m0".into(),
        126 => "exec_lo".into(),
        127 => "exec_hi".into(),
        128..=192 => format!("{}", s - 128),
        193..=208 => format!("{}", 192 - s as i32),
        240 => "0.5".into(),
        241 => "-0.5".into(),
        242 => "1.0".into(),
        243 => "-1.0".into(),
        244 => "2.0".into(),
        245 => "-2.0".into(),
        246 => "4.0".into(),
        247 => "-4.0".into(),
        255 => "lit".into(),
        256..=511 => format!("v{}", s - 256),
        _ => format!("src_{s}"),
    }
}

/// Decodes an RDNA2 (GFX10.3) shader binary and translates supported ops to HLSL.
pub fn analyze_rdna2(code: &[u8]) -> Result<ShaderAnalysis, String> {
    if code.is_empty() || code.len() % 4 != 0 {
        return Err("RDNA2 shader binary must be a non-empty multiple of 4 bytes.".into());
    }
    let words: Vec<u32> = code
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    let mut out = ShaderAnalysis {
        total_dwords: words.len(),
        ..ShaderAnalysis::default()
    };

    let mut i = 0usize;
    let mut hlsl_lines = vec![
        "// Auto-translated from RDNA2 (GFX10.3) via clean-room shader bridge".to_string(),
        "struct PSOut { float4 mrt0 : SV_Target0; };".to_string(),
        "PSOut main(float4 v0_in : TEXCOORD0) {".to_string(),
        "    uint exec_lo = 0xFFFFFFFFu;".to_string(),
        "    float v[256];".to_string(),
        "    v[0] = v0_in.x; v[1] = v0_in.y; v[2] = v0_in.z; v[3] = v0_in.w;".to_string(),
    ];
    let mut in_exec_block = false;

    while i < words.len() {
        let w0 = words[i];
        let offset = i * 4;

        // 1. SOPP: 1011_1111_1xxx_xxxx_xxxx_xxxx_xxxx_xxxx
        if (w0 >> 23) == 0b1011_1111_1 {
            let op = (w0 >> 16) & 0x7F;
            let simm16 = (w0 & 0xFFFF) as i16;
            let (mnem, ops) = match op {
                0x00 => ("s_nop", format!("{simm16}")),
                0x01 => {
                    out.ended_cleanly = true;
                    ("s_endpgm", String::new())
                }
                0x02 => ("s_branch", format!("{simm16:+}")),
                0x04 => ("s_cbranch_scc0", format!("{simm16:+}")),
                0x05 => ("s_cbranch_scc1", format!("{simm16:+}")),
                0x06 => ("s_cbranch_vccz", format!("{simm16:+}")),
                0x07 => ("s_cbranch_vccnz", format!("{simm16:+}")),
                0x08 => ("s_cbranch_execz", format!("{simm16:+}")),
                0x09 => ("s_cbranch_execnz", format!("{simm16:+}")),
                0x0A => ("s_barrier", String::new()),
                0x0C => ("s_waitcnt", format!("{:#06x}", w0 & 0xFFFF)),
                0x13 => ("s_waitcnt_depctr", format!("{:#06x}", w0 & 0xFFFF)),
                _ => ("s_sopp", format!("op={op:#x} imm={simm16}")),
            };
            out.instructions.push(DecodedInst {
                offset,
                size_bytes: 4,
                encoding: Encoding::Sopp,
                mnemonic: mnem.into(),
                operands: ops,
                raw: w0,
            });
            i += 1;
            if op == 0x01 {
                break;
            }
            continue;
        }

        // 2. SOP1: 1011_1110_1xxx_xxxx_xxxx_xxxx_xxxx_xxxx
        if (w0 >> 23) == 0b1011_1110_1 {
            let sdst = (w0 >> 16) & 0x7F;
            let op = (w0 >> 8) & 0xFF;
            let ssrc0 = w0 & 0xFF;
            let mut size = 4;
            let src_txt = if ssrc0 == 255 && i + 1 < words.len() {
                size = 8;
                i += 1;
                format!("{:#010x}", words[i])
            } else {
                ssrc_name(ssrc0)
            };
            if sdst == 126 || sdst == 127 {
                out.uses_exec_mask = true;
            }
            let mnem = match op {
                0x03 => "s_mov_b32",
                0x04 => "s_mov_b64",
                0x07 => "s_not_b32",
                0x09 => "s_wqm_b32",
                0x20 => "s_and_saveexec_b32",
                0x24 => "s_and_saveexec_b64",
                0x36 => "s_getpc_b64",
                0x37 => "s_setpc_b64",
                _ => "s_sop1",
            };
            out.instructions.push(DecodedInst {
                offset,
                size_bytes: size,
                encoding: Encoding::Sop1,
                mnemonic: mnem.into(),
                operands: format!("{}, {}", ssrc_name(sdst), src_txt),
                raw: w0,
            });
            i += 1;
            continue;
        }

        // 3. SOPC: 1011_1111_0...
        if (w0 >> 23) == 0b1011_1111_0 {
            let op = (w0 >> 16) & 0x7F;
            let ssrc1 = (w0 >> 8) & 0xFF;
            let ssrc0 = w0 & 0xFF;
            let mut size = 4;
            let s0 = if ssrc0 == 255 && i + 1 < words.len() { size = 8; i += 1; format!("{:#010x}", words[i]) } else { ssrc_name(ssrc0) };
            let s1 = if ssrc1 == 255 && i + 1 < words.len() { size += 4; i += 1; format!("{:#010x}", words[i]) } else { ssrc_name(ssrc1) };
            let mnem = match op {
                0x00 => "s_cmp_eq_i32",
                0x01 => "s_cmp_lg_i32",
                0x02 => "s_cmp_gt_i32",
                0x06 => "s_cmp_eq_u32",
                0x07 => "s_cmp_lg_u32",
                _ => "s_sopc",
            };
            out.instructions.push(DecodedInst {
                offset,
                size_bytes: size,
                encoding: Encoding::Sopc,
                mnemonic: mnem.into(),
                operands: format!("{s0}, {s1}"),
                raw: w0,
            });
            i += 1;
            continue;
        }

        // 4. EXP (64-bit): 1111_1000_...
        if (w0 >> 26) == 0b1111_10 && i + 1 < words.len() {
            let w1 = words[i + 1];
            let target = (w0 >> 4) & 0x3F;
            let en = w0 & 0xF;
            let [v0, v1, v2, v3] = w1.to_le_bytes();
            out.uses_exports = true;
            if in_exec_block {
                hlsl_lines.push("    }".into());
                in_exec_block = false;
            }
            if target == 0 {
                hlsl_lines.push(format!(
                    "    PSOut o; o.mrt0 = float4(v[{v0}], v[{v1}], v[{v2}], v[{v3}]); return o;"
                ));
            }
            out.instructions.push(DecodedInst {
                offset,
                size_bytes: 8,
                encoding: Encoding::Exp,
                mnemonic: "exp".into(),
                operands: format!("target={target} en={en:#x}, v{v0}, v{v1}, v{v2}, v{v3}"),
                raw: w0,
            });
            i += 2;
            continue;
        }

        // 5. SMEM (64-bit): 1111_01...
        if (w0 >> 26) == 0b1111_01 && i + 1 < words.len() {
            let w1 = words[i + 1];
            let op = (w0 >> 18) & 0xFF;
            let sdata = (w0 >> 6) & 0x7F;
            let sbase = (w0 & 0x3F) * 2;
            let soff = w1 & 0x1F_FFFF;
            out.uses_smem = true;
            let mnem = match op {
                0x00 => "s_load_dword",
                0x01 => "s_load_dwordx2",
                0x02 => "s_load_dwordx4",
                0x03 => "s_load_dwordx8",
                0x04 => "s_load_dwordx16",
                0x08 => "s_buffer_load_dword",
                0x0A => "s_buffer_load_dwordx4",
                _ => "s_mem",
            };
            out.instructions.push(DecodedInst {
                offset,
                size_bytes: 8,
                encoding: Encoding::Smem,
                mnemonic: mnem.into(),
                operands: format!("s{sdata}, s[{sbase}:{}], {soff:#x}", sbase + 1),
                raw: w0,
            });
            i += 2;
            continue;
        }

        // 6. VOP3 (64-bit): 1101_01...
        if (w0 >> 26) == 0b1101_01 && i + 1 < words.len() {
            let w1 = words[i + 1];
            let op = (w0 >> 16) & 0x3FF;
            let vdst = w0 & 0xFF;
            let s0 = w1 & 0x1FF;
            let s1 = (w1 >> 9) & 0x1FF;
            let s2 = (w1 >> 18) & 0x1FF;
            let mnem = match op {
                0x103 => "v_add_f32_e64",
                0x108 => "v_mul_f32_e64",
                0x14B => "v_fma_f32",
                0x1FF => "v_lshl_add_u32",
                _ => "v_vop3",
            };
            out.instructions.push(DecodedInst {
                offset,
                size_bytes: 8,
                encoding: Encoding::Vop3,
                mnemonic: mnem.into(),
                operands: format!("v{vdst}, {}, {}, {}", ssrc_name(s0), ssrc_name(s1), ssrc_name(s2)),
                raw: w0,
            });
            i += 2;
            continue;
        }

        // 7. MUBUF (64-bit): 1110_00... | MTBUF (64-bit): 1110_10... | FLAT (64-bit): 1101_11...
        if matches!(w0 >> 26, 0b1110_00 | 0b1110_10 | 0b1101_11) && i + 1 < words.len() {
            let enc = match w0 >> 26 {
                0b1110_00 => Encoding::Mubuf,
                0b1110_10 => Encoding::Mtbuf,
                _ => Encoding::Flat,
            };
            out.uses_vmem = true;
            let mnem = match enc {
                Encoding::Mubuf => "buffer_op",
                Encoding::Mtbuf => "tbuffer_op",
                _ => "global_op",
            };
            out.instructions.push(DecodedInst {
                offset,
                size_bytes: 8,
                encoding: enc,
                mnemonic: mnem.into(),
                operands: format!("w0={w0:#010x}, w1={:#010x}", words[i + 1]),
                raw: w0,
            });
            i += 2;
            continue;
        }

        // 8. VOP1: 0111_111...
        if (w0 >> 25) == 0b0111_111 {
            let vdst = (w0 >> 17) & 0xFF;
            let op = (w0 >> 9) & 0xFF;
            let src0 = w0 & 0x1FF;
            let mut size = 4;
            let src_txt = if src0 == 255 && i + 1 < words.len() {
                size = 8;
                i += 1;
                format!("asfloat({:#010x}u)", words[i])
            } else if src0 >= 256 {
                format!("v[{}]", src0 - 256)
            } else {
                ssrc_name(src0)
            };
            let mnem = match op {
                0x01 => "v_mov_b32",
                0x05 => "v_cvt_f32_i32",
                0x06 => "v_cvt_f32_u32",
                0x08 => "v_cvt_i32_f32",
                0x2A => "v_rcp_f32",
                0x2E => "v_rsq_f32",
                0x33 => "v_sqrt_f32",
                _ => "v_vop1",
            };
            if op == 0x01 {
                if !in_exec_block && out.uses_exec_mask {
                    hlsl_lines.push("    if (exec_lo != 0u) {".into());
                    in_exec_block = true;
                }
                hlsl_lines.push(format!("        v[{vdst}] = {src_txt};"));
            }
            out.instructions.push(DecodedInst {
                offset,
                size_bytes: size,
                encoding: Encoding::Vop1,
                mnemonic: mnem.into(),
                operands: format!("v{vdst}, {src_txt}"),
                raw: w0,
            });
            i += 1;
            continue;
        }

        // 9. VOPC: 0111_110...
        if (w0 >> 25) == 0b0111_110 {
            let op = (w0 >> 17) & 0xFF;
            let vsrc1 = (w0 >> 9) & 0xFF;
            let src0 = w0 & 0x1FF;
            let mut size = 4;
            let s0 = if src0 == 255 && i + 1 < words.len() { size = 8; i += 1; format!("{:#010x}", words[i]) } else { ssrc_name(src0) };
            out.instructions.push(DecodedInst {
                offset,
                size_bytes: size,
                encoding: Encoding::Vopc,
                mnemonic: format!("v_cmp_{op:#04x}"),
                operands: format!("vcc_lo, {s0}, v{vsrc1}"),
                raw: w0,
            });
            i += 1;
            continue;
        }

        // 10. VOP2: bit 31 == 0
        if (w0 >> 31) == 0 {
            let op = (w0 >> 25) & 0x3F;
            let vdst = (w0 >> 17) & 0xFF;
            let vsrc1 = (w0 >> 9) & 0xFF;
            let src0 = w0 & 0x1FF;
            let mut size = 4;
            let s0 = if src0 == 255 && i + 1 < words.len() {
                size = 8;
                i += 1;
                format!("asfloat({:#010x}u)", words[i])
            } else if src0 >= 256 {
                format!("v[{}]", src0 - 256)
            } else {
                ssrc_name(src0)
            };
            let (mnem, expr) = match op {
                3 => ("v_add_f32", Some(format!("{s0} + v[{vsrc1}]"))),
                4 => ("v_sub_f32", Some(format!("{s0} - v[{vsrc1}]"))),
                5 => ("v_subrev_f32", Some(format!("v[{vsrc1}] - {s0}"))),
                8 => ("v_mul_f32", Some(format!("{s0} * v[{vsrc1}]"))),
                15 => ("v_min_f32", Some(format!("min({s0}, v[{vsrc1}])"))),
                16 => ("v_max_f32", Some(format!("max({s0}, v[{vsrc1}])"))),
                43 => ("v_fmac_f32", Some(format!("v[{vdst}] + {s0} * v[{vsrc1}]"))),
                _ => ("v_vop2", None),
            };
            if let Some(rhs) = expr {
                if !in_exec_block && out.uses_exec_mask {
                    hlsl_lines.push("    if (exec_lo != 0u) {".into());
                    in_exec_block = true;
                }
                hlsl_lines.push(format!("        v[{vdst}] = {rhs};"));
            }
            out.instructions.push(DecodedInst {
                offset,
                size_bytes: size,
                encoding: Encoding::Vop2,
                mnemonic: mnem.into(),
                operands: format!("v{vdst}, {s0}, v{vsrc1}"),
                raw: w0,
            });
            i += 1;
            continue;
        }

        // 11. SOP2: 10...
        if (w0 >> 30) == 0b10 {
            let op = (w0 >> 23) & 0x7F;
            let sdst = (w0 >> 16) & 0x7F;
            let ssrc1 = (w0 >> 8) & 0xFF;
            let ssrc0 = w0 & 0xFF;
            let mut size = 4;
            let s0 = if ssrc0 == 255 && i + 1 < words.len() { size = 8; i += 1; format!("{:#010x}", words[i]) } else { ssrc_name(ssrc0) };
            let s1 = if ssrc1 == 255 && i + 1 < words.len() { size += 4; i += 1; format!("{:#010x}", words[i]) } else { ssrc_name(ssrc1) };
            let mnem = match op {
                0x00 => "s_add_u32",
                0x01 => "s_sub_u32",
                0x0E => "s_and_b32",
                0x10 => "s_or_b32",
                0x12 => "s_xor_b32",
                _ => "s_sop2",
            };
            out.instructions.push(DecodedInst {
                offset,
                size_bytes: size,
                encoding: Encoding::Sop2,
                mnemonic: mnem.into(),
                operands: format!("{}, {s0}, {s1}", ssrc_name(sdst)),
                raw: w0,
            });
            i += 1;
            continue;
        }

        // 12. SOPK: 1011...
        if (w0 >> 28) == 0b1011 {
            let op = (w0 >> 23) & 0x1F;
            let sdst = (w0 >> 16) & 0x7F;
            let simm16 = (w0 & 0xFFFF) as i16;
            out.instructions.push(DecodedInst {
                offset,
                size_bytes: 4,
                encoding: Encoding::Sopk,
                mnemonic: format!("s_sopk_{op:#x}"),
                operands: format!("{}, {simm16}", ssrc_name(sdst)),
                raw: w0,
            });
            i += 1;
            continue;
        }

        out.unknown_count += 1;
        out.instructions.push(DecodedInst {
            offset,
            size_bytes: 4,
            encoding: Encoding::Unknown,
            mnemonic: "unknown".into(),
            operands: format!("{w0:#010x}"),
            raw: w0,
        });
        i += 1;
    }

    if in_exec_block {
        hlsl_lines.push("    }".into());
    }
    if !out.uses_exports {
        hlsl_lines.push("    PSOut o; o.mrt0 = float4(v[0], v[1], v[2], v[3]); return o;".into());
    }
    hlsl_lines.push("}".into());
    out.hlsl = hlsl_lines.join("\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disassembles_and_translates_rdna2_shader_to_hlsl() {
        // v_mov_b32 v4, 1.0 (0x7E08_02F2); v_add_f32 v4, v0, v4 (0x0608_0900); exp mrt0 v4..v7; s_endpgm
        let dwords: [u32; 5] = [
            0x7E08_02F2,
            0x0608_0900,
            0xF800_180F,
            u32::from_le_bytes([4, 1, 2, 3]),
            0xBF81_0000,
        ];
        let bytes: Vec<u8> = dwords.iter().flat_map(|w| w.to_le_bytes()).collect();
        let report = analyze_rdna2(&bytes).unwrap();
        assert_eq!(report.unknown_count, 0);
        assert!(report.ended_cleanly);
        assert!(report.uses_exports);
        assert!(report.hlsl.contains("v[4] = 1.0;"));
        assert!(report.hlsl.contains("v[4] = v[0] + v[4];"));
        assert!(report.hlsl.contains("o.mrt0 = float4(v[4], v[1], v[2], v[3]);"));
    }
}
