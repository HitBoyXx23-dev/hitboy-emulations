//! Clean-room PlayStation 5 AGC / AMD RDNA2 PM4 Type-3 GPU Command-Buffer Parser & Translator.
//!
//! Parses standard AMD PM4 Type-3 command packets (`(3 << 30) | ((count - 1) << 16) | (opcode << 8)`)
//! into a portable graphics command list (`SetShReg`, `SetContextReg`, `DrawIndexAuto`, `DrawIndex2`,
//! `DispatchDirect`, `DispatchMeshDirect`, `ReleaseMem`, `WaitRegMem`) for desktop DX12/Vulkan replay.

use std::collections::BTreeMap;

/// Standard AMD PM4 Type-3 packet opcodes (from public AMD RDNA2 / Mesa AMDGPU ISA docs).
pub const IT_NOP: u8 = 0x10;
pub const IT_DISPATCH_DIRECT: u8 = 0x15;
pub const IT_DRAW_INDEX_2: u8 = 0x27;
pub const IT_DRAW_INDEX_AUTO: u8 = 0x2D;
pub const IT_WAIT_REG_MEM: u8 = 0x3C;
pub const IT_EVENT_WRITE: u8 = 0x46;
pub const IT_RELEASE_MEM: u8 = 0x49;
pub const IT_DISPATCH_MESH_DIRECT: u8 = 0x4C;
pub const IT_SET_CONTEXT_REG: u8 = 0x69;
pub const IT_SET_SH_REG: u8 = 0x76;

/// Decoded high-level AGC/PM4 graphics command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgcCommand {
    Nop { dwords: usize },
    SetShReg { reg_offset: u32, values: Vec<u32> },
    SetContextReg { reg_offset: u32, values: Vec<u32> },
    DrawIndexAuto { vertex_count: u32, initiator: u32 },
    DrawIndex2 { max_size: u32, index_base: u64, index_count: u32, initiator: u32 },
    DispatchDirect { dim_x: u32, dim_y: u32, dim_z: u32, initiator: u32 },
    DispatchMeshDirect { dim_x: u32, dim_y: u32, dim_z: u32, initiator: u32 },
    EventWrite { event_type: u32 },
    ReleaseMem { event_type: u32, address: u64, data: u64 },
    WaitRegMem { function: u32, address: u64, reference: u32, mask: u32 },
    Unknown { opcode: u8, payload_len: usize },
}

/// Decoded PM4 command buffer summary with live register state tracking.
#[derive(Debug, Clone, Default)]
pub struct AgcParseResult {
    pub commands: Vec<AgcCommand>,
    pub sh_regs: BTreeMap<u32, u32>,
    pub context_regs: BTreeMap<u32, u32>,
    pub draw_calls: usize,
    pub compute_dispatches: usize,
    pub mesh_dispatches: usize,
}

/// Builds a PM4 Type-3 packet header dword for `opcode` with `payload_dwords` body dwords.
pub fn pm4_type3_header(opcode: u8, payload_dwords: usize) -> u32 {
    let count = payload_dwords.saturating_sub(1) as u32;
    (3u32 << 30) | ((count & 0x3FFF) << 16) | ((opcode as u32) << 8)
}

/// Parses a slice of little-endian PM4 command buffer dwords into structured `AgcCommand`s.
pub fn parse_pm4_stream(dwords: &[u32]) -> Result<AgcParseResult, String> {
    let mut res = AgcParseResult::default();
    let mut pc = 0usize;

    while pc < dwords.len() {
        let hdr = dwords[pc];
        let pkt_type = hdr >> 30;
        if pkt_type == 2 {
            // Type-2 filler NOP (1 dword)
            res.commands.push(AgcCommand::Nop { dwords: 0 });
            pc += 1;
            continue;
        }
        if pkt_type != 3 {
            return Err(format!("Unsupported PM4 packet type {pkt_type} at dword {pc}"));
        }
        let opcode = ((hdr >> 8) & 0xFF) as u8;
        let count_field = ((hdr >> 16) & 0x3FFF) as usize;
        // Per AMD PM4 spec, count_field == 0x3FFF in IT_NOP means 0 payload dwords
        let payload_len = if opcode == IT_NOP && count_field == 0x3FFF {
            0
        } else {
            count_field + 1
        };
        let end = pc.checked_add(1 + payload_len).ok_or("PM4 packet length overflow")?;
        if end > dwords.len() {
            return Err(format!(
                "Truncated PM4 packet opcode {opcode:#04x} at dword {pc} (needs {payload_len} dwords)"
            ));
        }
        let body = &dwords[pc + 1..end];
        pc = end;

        match opcode {
            IT_NOP => res.commands.push(AgcCommand::Nop { dwords: payload_len }),
            IT_SET_SH_REG if !body.is_empty() => {
                let reg_offset = body[0] & 0xFFFF;
                let values = body[1..].to_vec();
                for (i, &v) in values.iter().enumerate() {
                    res.sh_regs.insert(reg_offset + i as u32, v);
                }
                res.commands.push(AgcCommand::SetShReg { reg_offset, values });
            }
            IT_SET_CONTEXT_REG if !body.is_empty() => {
                let reg_offset = body[0] & 0xFFFF;
                let values = body[1..].to_vec();
                for (i, &v) in values.iter().enumerate() {
                    res.context_regs.insert(reg_offset + i as u32, v);
                }
                res.commands.push(AgcCommand::SetContextReg { reg_offset, values });
            }
            IT_DRAW_INDEX_AUTO if body.len() >= 2 => {
                res.draw_calls += 1;
                res.commands.push(AgcCommand::DrawIndexAuto {
                    vertex_count: body[0],
                    initiator: body[1],
                });
            }
            IT_DRAW_INDEX_2 if body.len() >= 5 => {
                res.draw_calls += 1;
                let index_base = (body[1] as u64) | ((body[2] as u64) << 32);
                res.commands.push(AgcCommand::DrawIndex2 {
                    max_size: body[0],
                    index_base,
                    index_count: body[3],
                    initiator: body[4],
                });
            }
            IT_DISPATCH_DIRECT if body.len() >= 4 => {
                res.compute_dispatches += 1;
                res.commands.push(AgcCommand::DispatchDirect {
                    dim_x: body[0],
                    dim_y: body[1],
                    dim_z: body[2],
                    initiator: body[3],
                });
            }
            IT_DISPATCH_MESH_DIRECT if body.len() >= 4 => {
                res.mesh_dispatches += 1;
                res.commands.push(AgcCommand::DispatchMeshDirect {
                    dim_x: body[0],
                    dim_y: body[1],
                    dim_z: body[2],
                    initiator: body[3],
                });
            }
            IT_EVENT_WRITE if !body.is_empty() => {
                res.commands.push(AgcCommand::EventWrite {
                    event_type: body[0] & 0x3F,
                });
            }
            IT_RELEASE_MEM if body.len() >= 6 => {
                let address = (body[2] as u64) | ((body[3] as u64) << 32);
                let data = (body[4] as u64) | ((body[5] as u64) << 32);
                res.commands.push(AgcCommand::ReleaseMem {
                    event_type: body[0] & 0x3F,
                    address,
                    data,
                });
            }
            IT_WAIT_REG_MEM if body.len() >= 6 => {
                let address = (body[1] as u64) | ((body[2] as u64) << 32);
                res.commands.push(AgcCommand::WaitRegMem {
                    function: body[0] & 0x7,
                    address,
                    reference: body[3],
                    mask: body[4],
                });
            }
            _ => res.commands.push(AgcCommand::Unknown { opcode, payload_len }),
        }
    }

    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ps5_pm4_registers_draws_compute_and_mesh_dispatches() {
        let mut stream = Vec::new();
        // Set SH reg 0x200 = [0xAABBCCDD, 0x11223344]
        stream.push(pm4_type3_header(IT_SET_SH_REG, 3));
        stream.extend_from_slice(&[0x200, 0xAABB_CCDD, 0x1122_3344]);
        // Set Context reg 0x100 = [0x0000_0004]
        stream.push(pm4_type3_header(IT_SET_CONTEXT_REG, 2));
        stream.extend_from_slice(&[0x100, 4]);
        // DrawIndexAuto(36 vertices)
        stream.push(pm4_type3_header(IT_DRAW_INDEX_AUTO, 2));
        stream.extend_from_slice(&[36, 0x02]);
        // DispatchMeshDirect(8, 1, 1)
        stream.push(pm4_type3_header(IT_DISPATCH_MESH_DIRECT, 4));
        stream.extend_from_slice(&[8, 1, 1, 0x01]);
        // ReleaseMem fence write
        stream.push(pm4_type3_header(IT_RELEASE_MEM, 6));
        stream.extend_from_slice(&[0x04, 0x00, 0x0000_1000, 0x0000_0001, 7, 0]);

        let parsed = parse_pm4_stream(&stream).unwrap();
        assert_eq!(parsed.draw_calls, 1);
        assert_eq!(parsed.mesh_dispatches, 1);
        assert_eq!(parsed.sh_regs.get(&0x200), Some(&0xAABB_CCDD));
        assert_eq!(parsed.sh_regs.get(&0x201), Some(&0x1122_3344));
        assert_eq!(parsed.context_regs.get(&0x100), Some(&4));
        assert!(matches!(
            parsed.commands.last(),
            Some(AgcCommand::ReleaseMem { address: 0x1_0000_1000, data: 7, .. })
        ));
    }
}
