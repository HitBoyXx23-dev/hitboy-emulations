use super::*;
use std::collections::BTreeMap;

mod pm4;
mod builders;
mod gcn;
mod render_builders;
mod snapshots;
mod raster;
mod surfaces;

const INVALID: u64 = 0x8002_0016;
const FAULT: u64 = 0x8002_000E;
const MAX_BYTES: usize = 16 << 20;
const MAX_BUFFERS: usize = 64;

#[derive(Default)]
pub(super) struct Graphics {
    submissions: u64,
    buffers: u64,
    bytes: u64,
    packets: u64,
    invalid: u64,
    opcodes: BTreeMap<u8, u64>,
    samples: Vec<String>,
    last_error: Option<String>,
    first_buffers: Vec<(u64, Vec<u8>)>,
    first_calls: BTreeMap<String, Vec<u64>>,
    processor: pm4::Processor,
}

struct CommandBuffer {
    constant: bool,
    address: u64,
    bytes: Vec<u8>,
}

fn packet_histogram(bytes: &[u8]) -> Result<BTreeMap<u8, u64>, String> {
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return Err("Command buffer length must be a positive multiple of four".into());
    }
    let mut histogram = BTreeMap::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let header = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        let kind = header >> 30;
        let (opcode, words) = match kind {
            0 => (0, ((header >> 16) & 0x3fff) as usize + 2),
            2 => (2, 1),
            3 => (((header >> 8) & 0xff) as u8, ((header >> 16) & 0x3fff) as usize + 2),
            _ => return Err(format!("Invalid command packet type at byte {offset:#x}")),
        };
        let end = offset + words * 4;
        if end > bytes.len() {
            return Err(format!("Truncated command packet at byte {offset:#x}: needs {} bytes, {} remain", words * 4, bytes.len() - offset));
        }
        *histogram.entry(opcode).or_default() += 1;
        offset = end;
    }
    Ok(histogram)
}

impl Machine {
    fn read_graphics_buffers(&self) -> Result<Vec<CommandBuffer>, (u64, String)> {
        let count = self.arg(0);
        if count == 0 || count > MAX_BUFFERS as u64 {
            return Err((INVALID, "Invalid graphics command buffer count".into()));
        }
        let mut buffers = Vec::new();
        let mut total = 0usize;
        for (addresses, sizes, optional) in [(self.arg(1), self.arg(2), false), (self.arg(3), self.arg(4), true)] {
            if optional && addresses == 0 && sizes == 0 { continue; }
            if addresses == 0 || sizes == 0 {
                return Err((FAULT, "Missing graphics buffer address or size list".into()));
            }
            let mut address_bytes = vec![0; count as usize * 8];
            let mut size_bytes = vec![0; count as usize * 4];
            if !self.mem.read(addresses, &mut address_bytes) || !self.mem.read(sizes, &mut size_bytes) {
                return Err((FAULT, "Unmapped graphics buffer list".into()));
            }
            for index in 0..count as usize {
                let address = u64::from_le_bytes(address_bytes[index * 8..index * 8 + 8].try_into().unwrap());
                let size = u32::from_le_bytes(size_bytes[index * 4..index * 4 + 4].try_into().unwrap()) as usize;
                if optional && size == 0 { continue; }
                if size == 0 || size % 4 != 0 || address % 4 != 0 || size > MAX_BYTES - total {
                    return Err((INVALID, "Invalid or oversized graphics command buffer".into()));
                }
                total += size;
                let mut bytes = vec![0; size];
                if !self.mem.read(address, &mut bytes) {
                    return Err((FAULT, "Unmapped graphics command buffer".into()));
                }
                buffers.push(CommandBuffer { constant: optional, address, bytes });
            }
        }
        Ok(buffers)
    }

    pub fn first_graphics_command_buffers(&self) -> &[(u64, Vec<u8>)] {
        &self.graphics.first_buffers
    }

    pub fn compute_shader_prefix(&self) -> Option<(u64, Vec<u8>)> {
        if !self.runtime_error.as_deref().is_some_and(|error| error.starts_with("PS4 GPU execution stopped:")) { return None; }
        let processor = &self.graphics.processor;
        let address = (processor.register(0x2e0c) as u64) << 8 | (processor.register(0x2e0d) as u64) << 40;
        if address == 0 { return None; }
        let mut bytes = Vec::new();
        for offset in (0..4096).step_by(4) {
            let mut word = [0; 4];
            if !self.mem.read(address.checked_add(offset as u64)?, &mut word) { break; }
            bytes.extend(word);
            if u32::from_le_bytes(word) == 0xbf810000 { break; }
        }
        if bytes.is_empty() { None } else { Some((address, bytes)) }
    }

    pub fn raster_shader_snapshots(&self) -> Vec<(&'static str, u64, Vec<u8>)> {
        let mut snapshots = Vec::new();
        if !self.runtime_error.as_deref().is_some_and(|error| error.starts_with("PS4 GPU execution stopped:")) { return snapshots; }
        for (name, register) in [("vertex", 0x2c48), ("pixel", 0x2c08)] {
            let processor = &self.graphics.processor;
            let address = (processor.register(register) as u64) << 8 | (processor.register(register + 1) as u64) << 40;
            if address == 0 { continue; }
            let mut bytes = Vec::new();
            for offset in (0..4096).step_by(4) {
                let Some(position) = address.checked_add(offset) else { break; };
                let mut word = [0; 4];
                if !self.mem.read(position, &mut word) { break; }
                bytes.extend(word);
            }
            if !bytes.is_empty() { snapshots.push((name, address, bytes)); }
        }
        snapshots
    }

    pub fn compute_buffer_snapshots(&self) -> Vec<(usize, u64, Vec<u8>)> {
        let mut snapshots = Vec::new();
        if !self.runtime_error.as_deref().is_some_and(|error| error.starts_with("PS4 GPU execution stopped:")) { return snapshots; }
        let processor = &self.graphics.processor;
        if (processor.register(0x2e13) >> 1) & 31 != 8 { return snapshots; }
        for slot in 0..2 {
            let register = 0x2e40 + slot as u32 * 4;
            let low = processor.register(register);
            let high = processor.register(register + 1);
            let records = processor.register(register + 2) as u64;
            if processor.register(register + 3) >> 30 != 0 { continue; }
            let stride = ((high >> 16) & 0x3fff) as u64;
            let length = records * stride.max(1);
            let address = low as u64 | ((high & 0xfff) as u64) << 32;
            if length == 0 || length > 65536 || address == 0 { continue; }
            let mut bytes = vec![0; length as usize];
            if self.mem.read(address, &mut bytes) { snapshots.push((slot, address, bytes)); }
        }
        snapshots
    }

    pub fn graphics_register_snapshot(&self) -> Vec<(u32, u32)> {
        self.graphics.processor.register_snapshot()
    }

    pub fn compute_register_snapshot(&self) -> Vec<(u32, u32)> {
        self.graphics.processor.compute_registers()
    }

    pub fn pending_graphics_command_buffer(&self) -> Option<&[u8]> {
        self.graphics.processor.pending_buffer()
    }

    pub fn graphics_submission_count(&self) -> u64 {
        self.graphics.submissions
    }

    pub(super) fn pump_graphics(&mut self) {
        if self.runtime_error.is_some() { return; }
        let result = self.graphics.processor.pump(&mut self.mem, 4096, self.cpu.tsc / 1000);
        if self.graphics.processor.take_interrupts() != 0 { self.signal_graphics_completion(); }
        if let Err(error) = result {
            self.graphics.last_error = Some(error.clone());
            self.runtime_error = Some(format!("PS4 GPU execution stopped: {error}"));
        }
    }

    pub fn graphics_service_report(&self) -> String {
        let graphics = &self.graphics;
        format!("graphics_submissions={} command_buffers={} command_bytes={} command_packets={} invalid_submissions={} rendering_implemented=false opcodes={:x?} samples={:?} last_error={:?} first_calls={:x?} {}", graphics.submissions, graphics.buffers, graphics.bytes, graphics.packets, graphics.invalid, graphics.opcodes, graphics.samples, graphics.last_error, graphics.first_calls, graphics.processor.report())
    }

    pub(super) fn record_graphics_call(&mut self, name: &str) {
        if !(name.starts_with("sceGnm") || name.starts_with("sceVideoOut")) || self.graphics.first_calls.len() >= 32 || self.graphics.first_calls.contains_key(name) {
            return;
        }
        let count = if name == "sceGnmSubmitAndFlipCommandBuffers" { 9 } else { 6 };
        let arguments = (0..count).map(|index| self.arg_ext(index)).collect();
        self.graphics.first_calls.insert(name.into(), arguments);
    }

    pub(super) fn call_graphics(&mut self, name: &str) -> bool {
        if self.call_compute_builder(name) || self.call_render_builder(name) { return true; }
        if !matches!(name, "sceGnmSubmitCommandBuffers" | "sceGnmSubmitAndFlipCommandBuffers") {
            return false;
        }
        self.graphics.submissions += 1;
        let result = self.read_graphics_buffers().and_then(|buffers| {
            let mut decoded = Vec::new();
            for buffer in buffers {
                let histogram = packet_histogram(&buffer.bytes).map_err(|error| (INVALID, error))?;
                decoded.push((buffer, histogram));
            }
            Ok(decoded)
        });
        match result {
            Ok(buffers) => {
                let capture = self.graphics.first_buffers.is_empty();
                let contains_constants = buffers.iter().any(|(buffer, _)| buffer.constant);
                let mut commands = Vec::new();
                for (buffer, histogram) in buffers {
                    self.graphics.buffers += 1;
                    self.graphics.bytes += buffer.bytes.len() as u64;
                    self.graphics.packets += histogram.values().sum::<u64>();
                    if self.graphics.samples.len() < 16 {
                        self.graphics.samples.push(format!("address={:#x} bytes={} opcodes={histogram:x?}", buffer.address, buffer.bytes.len()));
                    }
                    if capture { self.graphics.first_buffers.push((buffer.address, buffer.bytes.clone())); }
                    commands.push(buffer.bytes);
                    for (opcode, count) in histogram {
                        *self.graphics.opcodes.entry(opcode).or_default() += count;
                    }
                }
                if !self.unimplemented.iter().any(|entry| entry == name) {
                    self.unimplemented.push(name.into());
                }
                if contains_constants {
                    self.graphics.last_error = Some("GPU constant-engine execution is not implemented".into());
                    self.runtime_error = self.graphics.last_error.clone();
                    self.ret(0x8002_004E);
                } else if let Err(error) = self.graphics.processor.enqueue(commands) {
                    self.graphics.last_error = Some(error);
                    self.ret(0x8002_000C);
                } else {
                    self.ret(0);
                    self.pump_graphics();
                }
            }
            Err((code, message)) => {
                self.graphics.invalid += 1;
                self.graphics.last_error = Some(message);
                self.ret(code);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    #[test]
    fn decodes_mixed_packets_and_rejects_truncation() {
        let bytes = words(&[0x80000000, 0xc0001000, 0, 0xc0033700, 0, 0, 0, 42]);
        assert_eq!(packet_histogram(&bytes).unwrap(), BTreeMap::from([(2, 1), (0x10, 1), (0x37, 1)]));
        assert!(packet_histogram(&bytes[..bytes.len() - 4]).unwrap_err().contains("Truncated"));
        assert!(packet_histogram(&words(&[0x40000000])).is_err());
        assert!(packet_histogram(&[0; 3]).is_err());
        assert!(packet_histogram(&[]).is_err());
    }

    fn machine() -> Machine {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        machine.mem.map(0x50000000, 8192, true, false).unwrap();
        machine.cpu.r[7] = 1;
        machine.cpu.r[6] = 0x50000000;
        machine.cpu.r[2] = 0x50000008;
        machine.cpu.r[1] = 0;
        machine.cpu.r[8] = 0;
        machine.mem.write_u64(0x50000000, 0x50001000);
        machine.mem.write(0x50000008, &8u32.to_le_bytes());
        machine.mem.write(0x50001000, &words(&[0xc0001000, 0]));
        machine
    }

    #[test]
    fn captures_real_buffers_without_claiming_a_rendered_flip() {
        let mut machine = machine();
        assert!(machine.call_graphics("sceGnmSubmitAndFlipCommandBuffers"));
        assert_eq!(machine.graphics.buffers, 1);
        assert_eq!(machine.graphics.opcodes.get(&0x10), Some(&1));
        assert_eq!(machine.first_graphics_command_buffers(), &[(0x50001000, words(&[0xc0001000, 0]))]);
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.video.flips, 0);
        assert!(machine.unimplemented.contains(&"sceGnmSubmitAndFlipCommandBuffers".into()));
    }

    #[test]
    fn later_invalid_buffers_do_not_partially_record_a_submission() {
        let mut machine = machine();
        machine.cpu.r[7] = 2;
        machine.cpu.r[2] = 0x50000020;
        machine.mem.write_u64(0x50000008, 0x50001010);
        machine.mem.write(0x50000020, &words(&[8, 4]));
        machine.mem.write(0x50001010, &words(&[0xc0011000]));
        machine.call_graphics("sceGnmSubmitCommandBuffers");
        assert_eq!(machine.cpu.r[0], INVALID);
        assert_eq!(machine.graphics.buffers, 0);
        assert!(machine.graphics.opcodes.is_empty());
        machine.mem.write(0x50001010, &words(&[0x80000000]));
        machine.cpu.r[1] = 0x50000040;
        machine.cpu.r[8] = 0x50000050;
        machine.mem.write(0x50000050, &words(&[0, 0]));
        machine.call_graphics("sceGnmSubmitCommandBuffers");
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.graphics.buffers, 2);
        assert_eq!(machine.graphics.packets, 2);
        assert_eq!(machine.video.flips, 0);
    }

    #[test]
    fn a_completed_fence_wakes_a_registered_graphics_waiter_without_a_flip() {
        let mut machine = machine();
        machine.cpu.r[7] = 0x50000300;
        machine.call_equeue("sceKernelCreateEqueue");
        let queue = machine.mem.read_u64(0x50000300);
        machine.cpu.r[7] = queue;
        machine.cpu.r[6] = 0x40;
        machine.cpu.r[2] = 0x1234;
        machine.call_equeue("sceGnmAddEqEvent");
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.deliver_equeue(queue, 0x50000400, 1, 0x50000500), Ok(false));
        assert_eq!(machine.spawn(0x401234, 0, 0x50000600), 0);
        machine.cpu.r[7] = queue;
        machine.cpu.r[6] = 0x50000400;
        machine.cpu.r[2] = 1;
        machine.cpu.r[1] = 0x50000500;
        machine.cpu.r[8] = 0;
        machine.call_equeue("sceKernelWaitEqueue");
        assert_eq!(machine.tid(), 2);
        machine.mem.write(0x50001000, &words(&[0xc0044700, 0x504, 0x50000700, 0x42000000, 0x89abcdef, 0x12345678]));
        machine.mem.write(0x50000008, &24u32.to_le_bytes());
        machine.cpu.r[7] = 1;
        machine.cpu.r[6] = 0x50000000;
        machine.cpu.r[2] = 0x50000008;
        machine.cpu.r[1] = 0;
        machine.cpu.r[8] = 0;
        machine.call_graphics("sceGnmSubmitCommandBuffers");
        assert_eq!(machine.mem.read_u64(0x50000700), 0x1234567889abcdef);
        machine.yield_now();
        assert_eq!(machine.tid(), 1);
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.mem.read_u64(0x50000400), 0x40);
        let mut filter = [0; 2];
        machine.mem.read(0x50000408, &mut filter);
        assert_eq!(i16::from_le_bytes(filter), -14);
        assert_eq!(machine.mem.read_u64(0x50000410), 0x40);
        assert_eq!(machine.mem.read_u64(0x50000418), 0x1234);
        assert_eq!(machine.video.flips, 0);
        assert!(machine.runtime_error.is_none());
        machine.cpu.r[7] = queue;
        machine.cpu.r[6] = 0x40;
        machine.call_equeue("sceGnmDeleteEqEvent");
        assert_eq!(machine.cpu.r[0], 0);
        machine.signal_graphics_completion();
        assert_eq!(machine.deliver_equeue(queue, 0x50000400, 1, 0x50000500), Ok(false));
    }

    #[test]
    fn private_buffer_snapshots_strip_memory_type_bits_and_respect_size_limits() {
        let mut machine = machine();
        machine.mem.write(0x50000200, &[0xac; 64]);
        machine.mem.write(0x50000300, &[0xbd; 64]);
        let commands = words(&[
            0xc0017602, 0x213, 0x10,
            0xc0087602, 0x240, 0x50000200, 0x47000, 16, 0, 0x50000300, 0x47000, 16, 0,
        ]);
        machine.graphics.processor.enqueue(vec![commands]).unwrap();
        machine.pump_graphics();
        assert!(machine.compute_buffer_snapshots().is_empty());
        machine.runtime_error = Some("PS4 GPU execution stopped: test dispatch".into());
        let snapshots = machine.compute_buffer_snapshots();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0], (0, 0x50000200, vec![0xac; 64]));
        assert_eq!(snapshots[1], (1, 0x50000300, vec![0xbd; 64]));
        machine.runtime_error = None;
        machine.graphics.processor.enqueue(vec![words(&[0xc0017602, 0x242, 65536])]).unwrap();
        machine.pump_graphics();
        machine.runtime_error = Some("PS4 GPU execution stopped: test dispatch".into());
        assert_eq!(machine.compute_buffer_snapshots().len(), 1);
    }

    #[test]
    fn raster_snapshots_require_a_gpu_stop_and_remain_bounded() {
        let mut machine = machine();
        machine.mem.map(0x60000000, 8192, true, false).unwrap();
        machine.mem.write(0x60000000, &[0xac; 4096]);
        machine.mem.write(0x60001000, &0xbf810000u32.to_le_bytes());
        machine.graphics.processor.enqueue(vec![words(&[
            0xc0027600, 0x48, 0x600000, 0,
            0xc0027600, 8, 0x600010, 0,
        ])]).unwrap();
        machine.pump_graphics();
        assert!(machine.raster_shader_snapshots().is_empty());
        machine.runtime_error = Some("PS4 GPU execution stopped: test draw".into());
        let snapshots = machine.raster_shader_snapshots();
        assert_eq!(snapshots[0], ("vertex", 0x60000000, vec![0xac; 4096]));
        assert_eq!(snapshots[1].0, "pixel");
        assert_eq!(snapshots[1].1, 0x60001000);
        assert_eq!(snapshots[1].2.len(), 4096);
        assert_eq!(&snapshots[1].2[..4], &0xbf810000u32.to_le_bytes());
        assert!(machine.graphics_register_snapshot().len() <= 4096);
        assert_eq!(machine.video.flips, 0);
    }

    #[test]
    fn invalid_submission_is_atomic_and_does_not_read_unbounded_memory() {
        let mut machine = machine();
        machine.cpu.r[7] = 65;
        machine.call_graphics("sceGnmSubmitCommandBuffers");
        assert_eq!(machine.cpu.r[0], INVALID);
        machine.cpu.r[7] = 1;
        machine.mem.write(0x50000008, &((MAX_BYTES + 4) as u32).to_le_bytes());
        machine.call_graphics("sceGnmSubmitCommandBuffers");
        assert_eq!(machine.cpu.r[0], INVALID);
        machine.mem.write(0x50000008, &8u32.to_le_bytes());
        machine.mem.write_u64(0x50000000, 0x60000000);
        machine.call_graphics("sceGnmSubmitCommandBuffers");
        assert_eq!(machine.cpu.r[0], FAULT);
        assert_eq!(machine.graphics.buffers, 0);
        assert_eq!(machine.graphics.invalid, 3);
    }
}
