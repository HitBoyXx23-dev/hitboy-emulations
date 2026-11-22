use super::super::Mem;
use std::collections::{BTreeMap, VecDeque};

struct Job {
    buffers: Vec<Vec<u8>>,
    buffer: usize,
    offset: usize,
    size: usize,
}

#[derive(Default)]
struct State {
    registers: BTreeMap<u32, u32>,
    index_type: u32,
    instances: u32,
    register_writes: u64,
    memory_writes: u64,
    barriers: u64,
    interrupts: u64,
    pending_interrupts: u64,
    dma_read_tag_bytes: u64,
    compute_dispatches: u64,
    compute_groups: u64,
    compute_instructions: u64,
    compute_bytes_read: u64,
    compute_bytes_written: u64,
    vertex_shader_vertices: u64,
    vertex_shader_instructions: u64,
    raster_draws: u64,
    raster_triangles: u64,
    raster_covered_samples: u64,
    raster_shaded_samples: u64,
    raster_pixel_wave_instructions: u64,
}

#[derive(Default)]
pub(super) struct Processor {
    jobs: VecDeque<Job>,
    bytes: usize,
    state: State,
    processed: u64,
    completed: u64,
    waiting: Option<String>,
}

enum Step {
    Ready,
    Waiting(String),
}

fn write_memory(mem: &mut Mem, address: u64, bytes: &[u8]) -> Result<(), String> {
    if address % bytes.len() as u64 != 0 {
        return Err(format!("Unaligned GPU fence at {address:#x}"));
    }
    let mut probe = vec![0; bytes.len()];
    if !mem.read(address, &mut probe) || !mem.write(address, bytes) {
        return Err(format!("Unmapped GPU fence at {address:#x}"));
    }
    Ok(())
}

fn set_registers(state: &mut State, start: u32, end: u32, data: &[u32]) -> Result<Step, String> {
    if data.is_empty() || start.checked_add(data.len() as u32).is_none_or(|limit| limit > end) {
        return Err("GPU register packet exceeds its register bank".into());
    }
    for (offset, value) in data.iter().enumerate() {
        state.registers.insert(start + offset as u32, *value);
    }
    state.register_writes += data.len() as u64;
    Ok(Step::Ready)
}

fn read_tag_dma(state: &mut State, mem: &Mem, payload: &[u32]) -> Result<Step, String> {
    if payload.len() != 6 { return Err("Invalid GPU DMA packet length".into()); }
    let control = payload[0];
    let command = payload[5];
    if control & !0x80000000 != 0x60000000 || payload[3] != 0x3022c || payload[4] != 0 || command & !0x1fffff != 0x28200000 {
        return Err("GPU DMA mode is not implemented; only L2 reads to CP_DMA_READ_TAGS are supported".into());
    }
    let length = (command & 0x1fffff) as usize;
    let source = payload[1] as u64 | (payload[2] as u64) << 32;
    let mut bytes = vec![0; length];
    if !mem.read(source, &mut bytes) { return Err(format!("Unmapped GPU DMA source {source:#x} bytes={length}")); }
    state.dma_read_tag_bytes += length as u64;
    Ok(Step::Ready)
}

fn indexed_draw(state: &mut State, mem: &mut Mem, payload: &[u32]) -> Result<Step, String> {
    if payload.len() != 5 || payload[4] != 0 || payload[3] > payload[0] { return Err("Invalid or unsupported indexed draw parameters".into()); }
    if state.instances > 1 { return Err("GPU instanced vertex execution is not implemented".into()); }
    let count = payload[3] as usize;
    if count == 0 { return Ok(Step::Ready); }
    if count > 4096 { return Err("GPU indexed draw exceeds the vertex limit".into()); }
    let width = match state.index_type { 0 => 2, 1 => 4, _ => return Err("GPU index format is not implemented".into()) };
    let address = payload[1] as u64 | (payload[2] as u64) << 32;
    if address % width as u64 != 0 { return Err("GPU index buffer is unaligned".into()); }
    let mut bytes = vec![0; count * width];
    if !mem.read(address, &mut bytes) { return Err(format!("GPU index buffer is unmapped at {address:#x}")); }
    let indices = bytes.chunks_exact(width).map(|word| if width == 2 { u16::from_le_bytes(word.try_into().unwrap()) as u32 } else { u32::from_le_bytes(word.try_into().unwrap()) }).collect::<Vec<_>>();
    let (vertices, instructions) = super::gcn::vertex_exports(mem, &state.registers, &indices)?;
    state.vertex_shader_vertices += vertices.len() as u64;
    state.vertex_shader_instructions += instructions;
    let surface = super::surfaces::Surface::read(mem, &state.registers)?;
    let rendered = super::raster::render(mem, &state.registers, &vertices, surface.width, surface.height, &surface.rgba)?;
    surface.write(mem, &rendered.rgba)?;
    state.raster_draws += 1;
    state.raster_triangles += rendered.stats.triangles;
    state.raster_covered_samples += rendered.stats.covered_samples;
    state.raster_shaded_samples += rendered.stats.shaded_samples;
    state.raster_pixel_wave_instructions += rendered.stats.pixel_wave_instructions;
    Ok(Step::Ready)
}

fn execute(state: &mut State, mem: &mut Mem, header: u32, payload: &[u32], system_ticks: u64) -> Result<Step, String> {
    let kind = header >> 30;
    if kind == 2 { return Ok(Step::Ready); }
    if kind == 0 { return set_registers(state, header & 0xffff, 0x10000, payload); }
    if header & 1 != 0 { return Err("Predicated GPU commands are not implemented".into()); }
    let opcode = (header >> 8) & 0xff;
    match opcode {
        0x10 => Ok(Step::Ready),
        0x2a | 0x2f => {
            if payload.len() != 1 { return Err("Invalid GPU index/instance packet length".into()); }
            if opcode == 0x2a { state.index_type = payload[0]; } else { state.instances = payload[0]; }
            Ok(Step::Ready)
        }
        0x68 | 0x69 | 0x76 | 0x79 => {
            if payload.len() < 2 || payload[0] >> 16 != 0 { return Err("Invalid or indexed GPU register packet".into()); }
            let (base, end) = match opcode {
                0x68 => (0x2000, 0x2c00),
                0x69 => (0xa000, 0xa400),
                0x76 => (0x2c00, 0x3000),
                _ => (0xc000, 0xc400),
            };
            set_registers(state, base + payload[0], end, &payload[1..])
        }
        0x47 => {
            if payload.len() != 5 { return Err("Invalid GPU end-of-pipe packet length".into()); }
            let control = payload[2];
            let selection = control >> 29;
            let interrupt = (control >> 24) & 3;
            if !matches!(interrupt, 0 | 2) || (interrupt == 2 && selection == 0) { return Err(format!("GPU end-of-pipe interrupt selection {interrupt} is not implemented for data selection {selection}")); }
            let address = payload[1] as u64 | ((control & 0xff) as u64) << 32;
            let value = payload[3] as u64 | (payload[4] as u64) << 32;
            match selection {
                0 => {},
                1 => { write_memory(mem, address, &(value as u32).to_le_bytes())?; state.memory_writes += 1; },
                2 => { write_memory(mem, address, &value.to_le_bytes())?; state.memory_writes += 1; },
                4 => { write_memory(mem, address, &system_ticks.to_le_bytes())?; state.memory_writes += 1; },
                _ => return Err(format!("GPU end-of-pipe data selection {selection} is not implemented")),
            }
            if interrupt == 2 {
                state.interrupts = state.interrupts.saturating_add(1);
                state.pending_interrupts = state.pending_interrupts.saturating_add(1);
            }
            Ok(Step::Ready)
        }
        0x48 => {
            if payload.len() != 4 { return Err("Invalid GPU end-of-shader packet length".into()); }
            let control = payload[2];
            if control >> 29 != 2 { return Err("GPU end-of-shader GDS transfer is not implemented".into()); }
            let address = payload[1] as u64 | ((control & 0xff) as u64) << 32;
            write_memory(mem, address, &payload[3].to_le_bytes())?;
            state.memory_writes += 1;
            Ok(Step::Ready)
        }
        0x3c => {
            if payload.len() != 6 || payload[0] & !0x117 != 0 || payload[0] & 7 == 7 {
                return Err("Invalid or unsupported GPU wait packet".into());
            }
            let function = payload[0] & 7;
            if function == 0 { return Ok(Step::Ready); }
            let address = payload[1] as u64 | (payload[2] as u64) << 32;
            let value = if payload[0] & 0x10 != 0 {
                let mut bytes = [0; 4];
                if address & 3 != 0 || !mem.read(address, &mut bytes) { return Err(format!("Invalid GPU wait address {address:#x}")); }
                u32::from_le_bytes(bytes)
            } else {
                if address >= 0x10000 { return Err("GPU wait register exceeds the register file".into()); }
                state.registers.get(&(address as u32)).copied().unwrap_or(0)
            };
            let value = value & payload[4];
            let reference = payload[3];
            let ready = match function {
                1 => value < reference, 2 => value <= reference, 3 => value == reference,
                4 => value != reference, 5 => value >= reference, 6 => value > reference,
                _ => false,
            };
            if ready { Ok(Step::Ready) } else { Ok(Step::Waiting(format!("address={address:#x} value={value:#x} reference={reference:#x} mask={:#x} function={function}", payload[4]))) }
        }
        0x15 => {
            if payload.len() != 4 { return Err("Invalid GPU compute dispatch packet length".into()); }
            let stats = super::gcn::dispatch(mem, &state.registers, [payload[0], payload[1], payload[2]], payload[3])
                .map_err(|error| format!("GPU compute opcode 0x15: {error}"))?;
            state.compute_dispatches += 1;
            state.compute_groups += stats.groups;
            state.compute_instructions += stats.instructions;
            state.compute_bytes_read += stats.bytes_read;
            state.compute_bytes_written += stats.bytes_written;
            Ok(Step::Ready)
        }
        0x27 => indexed_draw(state, mem, payload).map_err(|error| format!("GPU indexed draw opcode 0x27: {error}")),
        0x50 => read_tag_dma(state, mem, payload),
        0x58 => {
            if payload.len() != 6 { return Err("Invalid GPU acquire-memory packet length".into()); }
            state.barriers += 1;
            Ok(Step::Ready)
        }
        _ => Err(format!("GPU opcode {opcode:#04x} is not implemented")),
    }
}

impl Processor {
    pub(super) fn enqueue(&mut self, buffers: Vec<Vec<u8>>) -> Result<(), String> {
        if buffers.is_empty() || buffers.len() > super::MAX_BUFFERS || self.jobs.len() >= 32 {
            return Err("GPU command queue exceeds its batch limit".into());
        }
        let size = buffers.iter().try_fold(0usize, |total, bytes| total.checked_add(bytes.len())).ok_or("GPU command queue size overflow")?;
        if size > super::MAX_BYTES - self.bytes { return Err("GPU command queue exceeds its byte limit".into()); }
        for buffer in &buffers { super::packet_histogram(buffer)?; }
        self.bytes += size;
        self.jobs.push_back(Job { buffers, buffer: 0, offset: 0, size });
        Ok(())
    }

    pub(super) fn pump(&mut self, mem: &mut Mem, mut budget: usize, system_ticks: u64) -> Result<(), String> {
        self.waiting = None;
        while budget != 0 {
            let Some(job) = self.jobs.front_mut() else { return Ok(()); };
            if job.buffer == job.buffers.len() {
                self.bytes -= job.size;
                self.jobs.pop_front();
                self.completed += 1;
                continue;
            }
            let bytes = &job.buffers[job.buffer];
            if job.offset == bytes.len() { job.buffer += 1; job.offset = 0; continue; }
            let header = u32::from_le_bytes(bytes[job.offset..job.offset + 4].try_into().unwrap());
            let length = if header >> 30 == 2 { 4 } else { (((header >> 16) & 0x3fff) as usize + 2) * 4 };
            let payload = bytes[job.offset + 4..job.offset + length].chunks_exact(4).map(|word| u32::from_le_bytes(word.try_into().unwrap())).collect::<Vec<_>>();
            match execute(&mut self.state, mem, header, &payload, system_ticks).map_err(|error| format!("buffer={} offset={:#x}: {error}", job.buffer, job.offset))? {
                Step::Ready => { job.offset += length; self.processed += 1; budget -= 1; },
                Step::Waiting(reason) => { self.waiting = Some(reason); return Ok(()); },
            }
        }
        Ok(())
    }

    pub(super) fn take_interrupts(&mut self) -> u64 {
        std::mem::take(&mut self.state.pending_interrupts)
    }

    pub(super) fn register(&self, address: u32) -> u32 {
        self.state.registers.get(&address).copied().unwrap_or(0)
    }

    pub(super) fn register_snapshot(&self) -> Vec<(u32, u32)> {
        self.state.registers.iter().take(4096).map(|(address, value)| (*address, *value)).collect()
    }

    pub(super) fn compute_registers(&self) -> Vec<(u32, u32)> {
        self.state.registers.range(0x2e00..0x2f00).map(|(address, value)| (*address, *value)).collect()
    }

    pub(super) fn pending_index_buffer(&self) -> Option<(u64, usize, usize)> {
        let job = self.jobs.front()?;
        let bytes = job.buffers.get(job.buffer)?.get(job.offset..job.offset.checked_add(24)?)?;
        let words = bytes.chunks_exact(4).map(|word| u32::from_le_bytes(word.try_into().unwrap())).collect::<Vec<_>>();
        if words[0] != 0xc0042700 { return None; }
        let width = match self.state.index_type & 3 { 0 => 2, 1 => 4, _ => return None };
        let count = words[4].min(words[1]) as usize;
        let length = count.checked_mul(width)?;
        if length == 0 || length > 65536 { return None; }
        Some((words[2] as u64 | (words[3] as u64) << 32, length, width))
    }

    pub(super) fn pending_buffer(&self) -> Option<&[u8]> {
        let job = self.jobs.front()?;
        job.buffers.get(job.buffer).map(Vec::as_slice)
    }

    pub(super) fn report(&self) -> String {
        format!("gpu_executed_packets={} gpu_completed_batches={} gpu_queued_bytes={} gpu_register_writes={} gpu_fence_writes={} gpu_barriers={} gpu_interrupts={} gpu_dma_read_tag_bytes={} gpu_compute_dispatches={} gpu_compute_groups={} gpu_compute_instructions={} gpu_compute_bytes_read={} gpu_compute_bytes_written={} gpu_vertex_shader_vertices={} gpu_vertex_shader_instructions={} gpu_raster_draws={} gpu_raster_triangles={} gpu_raster_covered_samples={} gpu_raster_shaded_samples={} gpu_raster_pixel_wave_instructions={} gpu_waiting={:?}", self.processed, self.completed, self.bytes, self.state.register_writes, self.state.memory_writes, self.state.barriers, self.state.interrupts, self.state.dma_read_tag_bytes, self.state.compute_dispatches, self.state.compute_groups, self.state.compute_instructions, self.state.compute_bytes_read, self.state.compute_bytes_written, self.state.vertex_shader_vertices, self.state.vertex_shader_instructions, self.state.raster_draws, self.state.raster_triangles, self.state.raster_covered_samples, self.state.raster_shaded_samples, self.state.raster_pixel_wave_instructions, self.waiting)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(data: &[u32]) -> Vec<u8> { data.iter().flat_map(|word| word.to_le_bytes()).collect() }

    #[test]
    fn fences_retire_in_order_and_waits_resume_without_replaying_prior_writes() {
        let mut mem = Mem::new().unwrap();
        mem.map(0x200000000, 4096, true, false).unwrap();
        let mut processor = Processor::default();
        processor.enqueue(vec![words(&[
            0xc0044700, 0x528, 0, 0x40000002, 0x12345678, 0x9abcdef0,
            0xc0053c00, 0x13, 16, 2, 7, 0xffffffff, 10,
            0xc0016900, 0x40, 99,
        ])]).unwrap();
        processor.pump(&mut mem, 4096, 0).unwrap();
        assert_eq!(mem.read_u64(0x200000000), 0x9abcdef012345678);
        assert_eq!(processor.state.memory_writes, 1);
        assert_eq!(processor.processed, 1);
        assert!(processor.waiting.is_some());
        mem.write(0x200000010, &7u32.to_le_bytes());
        processor.pump(&mut mem, 4096, 0).unwrap();
        assert_eq!(processor.state.memory_writes, 1);
        assert_eq!(processor.state.registers.get(&0xa040), Some(&99));
        assert_eq!(processor.completed, 1);
        assert_eq!(processor.bytes, 0);
    }

    #[test]
    fn unsupported_draws_do_not_retire_later_fences_and_invalid_writes_are_atomic() {
        let mut mem = Mem::new().unwrap();
        mem.map(0x200000000, 4096, true, false).unwrap();
        mem.write_u64(0x200000ff8, 0x1111222233334444);
        let mut processor = Processor::default();
        processor.enqueue(vec![words(&[0xc0012d00, 3, 2, 0xc0044700, 0x528, 0, 0x40000002, 1, 0])]).unwrap();
        assert!(processor.pump(&mut mem, 4096, 0).unwrap_err().contains("opcode 0x2d"));
        assert_eq!(mem.read_u64(0x200000000), 0);
        assert_eq!(processor.processed, 0);
        assert!(write_memory(&mut mem, 0x200000ffc, &1u64.to_le_bytes()).is_err());
        assert_eq!(mem.read_u64(0x200000ff8), 0x1111222233334444);
        assert!(write_memory(&mut mem, 0x200001000, &1u64.to_le_bytes()).is_err());
    }

    #[test]
    #[ignore]
    fn captured_ot11_setup_completes_without_draw_commands() {
        let path = std::env::var_os("HITBOY_PS4_PM4_CAPTURE").expect("Supply the private OT11 first-command capture");
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(bytes.len(), 2628);
        let mut mem = Mem::new().unwrap();
        mem.map(0x2ad7ff000, 8192, true, false).unwrap();
        let mut processor = Processor::default();
        processor.enqueue(vec![bytes]).unwrap();
        processor.pump(&mut mem, 4096, 12345678).unwrap();
        assert_eq!(processor.processed, 161);
        assert_eq!(processor.state.memory_writes, 7);
        assert_eq!(mem.read_u64(0x2ad800568), 1);
        assert_eq!(mem.read_u64(0x2ad7fff00), 12345678);
        assert_eq!(processor.completed, 1);
        assert_eq!(processor.take_interrupts(), 1);
        assert_eq!(processor.take_interrupts(), 0);
        assert_eq!(processor.bytes, 0);
        println!("{} setup_only=true rendering_implemented=false", processor.report());
    }

    #[test]
    fn system_timestamps_use_the_shared_clock_when_the_packet_executes() {
        let mut mem = Mem::new().unwrap();
        mem.map(0x200000000, 4096, true, false).unwrap();
        let mut processor = Processor::default();
        processor.enqueue(vec![words(&[
            0xc0044700, 0x504, 0, 0x80000002, 99, 99,
            0xc0044700, 0x504, 8, 0x80000002, 99, 99,
        ])]).unwrap();
        processor.pump(&mut mem, 1, 12345678).unwrap();
        assert_eq!(mem.read_u64(0x200000000), 12345678);
        assert_eq!(mem.read_u64(0x200000008), 0);
        processor.pump(&mut mem, 4096, 12345999).unwrap();
        assert_eq!(mem.read_u64(0x200000008), 12345999);
        assert_eq!(processor.state.memory_writes, 2);
        assert_eq!(processor.completed, 1);
    }

    #[test]
    fn completion_interrupts_follow_successful_fence_writes_only() {
        let mut mem = Mem::new().unwrap();
        mem.map(0x200000000, 4096, true, false).unwrap();
        let event = words(&[0xc0044700, 0x504, 0, 0x42000002, 0x12345678, 0x9abcdef0]);
        let mut processor = Processor::default();
        processor.enqueue(vec![event.clone()]).unwrap();
        processor.pump(&mut mem, 4096, 0).unwrap();
        assert_eq!(mem.read_u64(0x200000000), 0x9abcdef012345678);
        assert_eq!(processor.take_interrupts(), 1);
        assert_eq!(processor.take_interrupts(), 0);
        let mut unmapped = Mem::new().unwrap();
        let mut processor = Processor::default();
        processor.enqueue(vec![event.clone()]).unwrap();
        assert!(processor.pump(&mut unmapped, 4096, 0).is_err());
        assert_eq!(processor.take_interrupts(), 0);
        let mut commands = words(&[0xc0012d00, 3, 2]);
        commands.extend(event);
        let mut processor = Processor::default();
        processor.enqueue(vec![commands]).unwrap();
        assert!(processor.pump(&mut mem, 4096, 0).is_err());
        assert_eq!(processor.take_interrupts(), 0);
    }

    #[test]
    fn read_tag_dma_validates_memory_and_never_becomes_a_generic_copy() {
        let mut mem = Mem::new().unwrap();
        mem.map(0x200000000, 4096, true, false).unwrap();
        let source = (0..132).map(|value| value as u8).collect::<Vec<_>>();
        mem.write(0x200000000, &source);
        let mut state = State::default();
        assert!(matches!(read_tag_dma(&mut state, &mem, &[0x60000000, 0, 2, 0x3022c, 0, 0x28200084]), Ok(Step::Ready)));
        assert_eq!(state.dma_read_tag_bytes, 132);
        assert_eq!(state.memory_writes, 0);
        let mut after = vec![0; source.len()];
        mem.read(0x200000000, &mut after);
        assert_eq!(after, source);
        assert!(read_tag_dma(&mut state, &mem, &[0x60000000, 0x1000, 2, 0x3022c, 0, 0x28200084]).is_err());
        assert_eq!(state.dma_read_tag_bytes, 132);
        assert!(read_tag_dma(&mut state, &mem, &[0x60000000, 0, 2, 0x100, 2, 0x84]).is_err());
        assert_eq!(state.memory_writes, 0);
    }

    #[test]
    #[ignore]
    fn captured_ot11_compute_batch_reaches_dispatch_after_read_tag_dma() {
        let path = std::env::var_os("HITBOY_PS4_COMPUTE_CAPTURE").expect("Supply the private OT11 pending compute command buffer");
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(bytes.len(), 34120);
        let mut mem = Mem::new().unwrap();
        mem.map(0x2ad7ff000, 4096, true, false).unwrap();
        mem.map(0x2ad860000, 0x9000, true, false).unwrap();
        mem.map(0x203bf9000, 4096, true, false).unwrap();
        let mut processor = Processor::default();
        processor.enqueue(vec![bytes]).unwrap();
        let error = processor.pump(&mut mem, 4096, 12345678).unwrap_err();
        assert!(error.contains("offset=0x700"), "{error}");
        assert!(error.contains("opcode 0x15"), "{error}");
        assert_eq!(processor.state.dma_read_tag_bytes, 132);
        assert_eq!(processor.completed, 0);
        assert_eq!(processor.register(0x2e0c), 0x203bf9c);
        println!("{} synthetic_source_ram=true stopped={error}", processor.report());
    }

    #[test]
    #[ignore]
    fn authentic_compute_commands_execute_shader_before_retiring_its_fence() {
        let root = std::path::PathBuf::from(std::env::var_os("HITBOY_PS4_COMPUTE_SNAPSHOT").expect("Supply the private compute snapshot directory"));
        let commands = std::fs::read(root.join("pending-commands.pm4")).unwrap();
        let code = std::fs::read(root.join("compute-shader-prefix.bin")).unwrap();
        let input = std::fs::read(root.join("compute-buffer-00.bin")).unwrap();
        assert_eq!((commands.len(), code.len(), input.len()), (34120, 76, 2048));
        let mut mem = Mem::new().unwrap();
        for (address, size) in [(0x2ad7ff000, 4096), (0x2ad860000, 0x9000), (0x203bf9000, 4096), (0x20741d000, 4096), (0x205403000, 4096)] {
            mem.map(address, size, true, false).unwrap();
        }
        mem.write(0x203bf9c00, &code);
        mem.write(0x20741d000, &input);
        let mut processor = Processor::default();
        processor.enqueue(vec![commands[..0x784].to_vec()]).unwrap();
        processor.pump(&mut mem, 4096, 12345678).unwrap();
        let mut output = vec![0; input.len()];
        mem.read(0x205403000, &mut output);
        assert_eq!(output, input);
        let mut fence = [0; 4];
        assert!(mem.read(0x2ad860730, &mut fence));
        assert_eq!(u32::from_le_bytes(fence), 1);
        assert_eq!(processor.completed, 1);
        assert_eq!(processor.bytes, 0);
        assert_eq!(processor.state.compute_dispatches, 1);
        assert_eq!(processor.state.compute_instructions, 11);
        assert_eq!(processor.state.compute_bytes_read, 2048);
        assert_eq!(processor.state.compute_bytes_written, 2048);
        println!("{} authentic_first_dispatch_byte_exact=true rendering_verified=false", processor.report());
    }

    #[test]
    #[ignore]
    fn authentic_draw_commits_linear_pixels_before_its_fence_and_rejects_tiled_targets() {
        let mut snapshot = super::super::gcn::fixtures::raster_snapshot();
        let command = std::fs::read(snapshot.root.join("pending-commands.pm4")).unwrap();
        let mut command = command[0xc48..0xc60].to_vec();
        command.extend(words(&[0xc0044700, 0x504, 0, 0x42000003, 1, 0]));
        snapshot.mem.map(0x300000000, 4096, true, false).unwrap();
        let mut rejected = Processor::default();
        rejected.state.registers = snapshot.registers.clone();
        rejected.state.index_type = 1;
        rejected.enqueue(vec![command.clone()]).unwrap();
        let error = rejected.pump(&mut snapshot.mem, 4096, 0).unwrap_err();
        assert!(error.contains("tile mode 0xa"), "{error}");
        assert_eq!(rejected.state.raster_draws, 0);
        assert_eq!(snapshot.mem.read_u64(0x300000000), 0);
        assert_eq!(rejected.take_interrupts(), 0);
        let target = 0x300010000;
        let pixel_count = 1920 * 1088;
        let initial = [255, 0, 255, 0].repeat(pixel_count);
        snapshot.mem.map(target, initial.len() as u64, true, false).unwrap();
        snapshot.mem.write(target, &initial);
        snapshot.registers.insert(0xa318, (target >> 8) as u32);
        snapshot.registers.entry(0xa31d).and_modify(|value| *value = (*value & !31) | 8);
        let mut processor = Processor::default();
        processor.state.registers = snapshot.registers;
        processor.state.index_type = 1;
        processor.enqueue(vec![command]).unwrap();
        processor.pump(&mut snapshot.mem, 4096, 0).unwrap();
        assert_eq!(processor.state.raster_draws, 1);
        assert_eq!(processor.state.raster_shaded_samples, 1920 * 1080);
        assert_eq!(processor.completed, 1);
        assert_eq!(snapshot.mem.read_u64(0x300000000), 1);
        assert_eq!(processor.take_interrupts(), 1);
        let surface = super::super::surfaces::Surface::read(&snapshot.mem, &processor.state.registers).unwrap();
        let visible = 1920 * 1080 * 4;
        assert!(surface.rgba[..visible].chunks_exact(4).all(|pixel| pixel == [0, 0, 0, 255]));
        assert_eq!(&surface.rgba[visible..], &initial[visible..]);
        println!("{} isolated_linear_target=true authentic_shaders=true padding_preserved=true fence_after_pixels=true game_presentation_verified=false gameplay_verified=false", processor.report());
    }

    #[test]
    fn queue_limits_and_invalid_packets_leave_the_queue_unchanged() {
        let mut processor = Processor::default();
        assert!(processor.enqueue(vec![words(&[0xc0044700, 0])]).is_err());
        assert_eq!(processor.bytes, 0);
        for _ in 0..32 { processor.enqueue(vec![words(&[0x80000000])]).unwrap(); }
        assert!(processor.enqueue(vec![words(&[0x80000000])]).is_err());
        assert_eq!(processor.jobs.len(), 32);
    }
}
