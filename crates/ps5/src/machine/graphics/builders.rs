use super::*;

impl Machine {
    pub(super) fn write_graphics_commands(&mut self, words: &[u32]) -> u64 {
        let address = self.arg(0);
        if address & 3 != 0 { return INVALID; }
        let bytes = words.iter().flat_map(|word| word.to_le_bytes()).collect::<Vec<_>>();
        let mut probe = vec![0; bytes.len()];
        if !self.mem.read(address, &mut probe) || !self.mem.write(address, &bytes) { return FAULT; }
        0
    }

    pub(super) fn call_compute_builder(&mut self, name: &str) -> bool {
        if !matches!(name, "sceGnmDispatchDirect" | "sceGnmSetCsShader" | "sceGnmSetCsShaderWithModifier") { return false; }
        let capacity = self.arg(1) as u32 as usize;
        let result = if name == "sceGnmDispatchDirect" {
            if !(5..=64).contains(&capacity) { INVALID } else if self.arg(5) as u32 != 0 { 0x8002_004E } else {
                let mut words = vec![0xc0031502, self.arg(2) as u32, self.arg(3) as u32, self.arg(4) as u32, 5];
                words.resize(capacity, 0x80000000);
                self.write_graphics_commands(&words)
            }
        } else if !(13..=64).contains(&capacity) {
            INVALID
        } else if name == "sceGnmSetCsShaderWithModifier" && self.arg(3) as u32 != 0 {
            0x8002_004E
        } else {
            let mut bytes = [0; 28];
            if !self.mem.read(self.arg(2), &mut bytes) { FAULT } else {
                let registers = bytes.chunks_exact(4).map(|word| u32::from_le_bytes(word.try_into().unwrap())).collect::<Vec<_>>();
                let mut words = vec![
                    0xc0027602, 0x20c, registers[0], registers[1],
                    0xc0027602, 0x212, registers[2], registers[3],
                    0xc0037602, 0x207, registers[4], registers[5], registers[6],
                ];
                words.resize(capacity, 0x80000000);
                self.write_graphics_commands(&words)
            }
        };
        self.ret(result);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> Machine {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        machine.mem.map(0x50000000, 4096, true, false).unwrap();
        machine
    }

    #[test]
    fn shader_binding_writes_real_register_packets_and_keeps_buffer_guards() {
        let mut machine = machine();
        machine.mem.write(0x50000100, &[0xa5; 116]);
        let registers = [0x123450u32, 0, 0x2c0040, 0x98, 64, 1, 1];
        let bytes = registers.iter().flat_map(|word| word.to_le_bytes()).collect::<Vec<_>>();
        machine.mem.write(0x50000200, &bytes);
        machine.cpu.r[7] = 0x50000108;
        machine.cpu.r[6] = 25;
        machine.cpu.r[2] = 0x50000200;
        machine.cpu.r[1] = 0;
        assert!(machine.call_compute_builder("sceGnmSetCsShaderWithModifier"));
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.mem.read_u64(0x50000100), 0xa5a5a5a5a5a5a5a5);
        assert_eq!(machine.mem.read_u64(0x5000016c), 0xa5a5a5a5a5a5a5a5);
        let mut commands = vec![0; 100];
        machine.mem.read(0x50000108, &mut commands);
        machine.graphics.processor.enqueue(vec![commands]).unwrap();
        machine.pump_graphics();
        assert!(machine.runtime_error.is_none());
        for (address, value) in [(0x2e0c, registers[0]), (0x2e12, registers[2]), (0x2e13, registers[3]), (0x2e07, 64), (0x2e08, 1), (0x2e09, 1)] {
            assert_eq!(machine.graphics.processor.register(address), value);
        }
        assert_eq!(machine.video.flips, 0);
    }

    #[test]
    fn dispatch_emits_compute_packet_but_does_not_fake_shader_execution() {
        let mut machine = machine();
        machine.cpu.r[7] = 0x50000100;
        machine.cpu.r[6] = 9;
        machine.cpu.r[2] = 7;
        machine.cpu.r[1] = 8;
        machine.cpu.r[8] = 9;
        machine.cpu.r[9] = 0;
        machine.call_compute_builder("sceGnmDispatchDirect");
        assert_eq!(machine.cpu.r[0], 0);
        let mut commands = vec![0; 36];
        machine.mem.read(0x50000100, &mut commands);
        assert!(packet_histogram(&commands).unwrap().contains_key(&0x15));
        assert_eq!(machine.mem.read_u64(0x50000104), 7 | (8u64 << 32));
        machine.graphics.processor.enqueue(vec![commands]).unwrap();
        machine.pump_graphics();
        assert!(machine.runtime_error.as_ref().unwrap().contains("opcode 0x15"));
        assert_eq!(machine.video.flips, 0);
    }

    #[test]
    fn unsupported_modifiers_and_invalid_buffers_do_not_partially_write_commands() {
        let mut machine = machine();
        machine.mem.write(0x50000100, &[0xab; 100]);
        machine.cpu.r[7] = 0x50000100;
        machine.cpu.r[6] = 9;
        machine.cpu.r[9] = 1;
        machine.call_compute_builder("sceGnmDispatchDirect");
        assert_eq!(machine.cpu.r[0], 0x8002004e);
        assert_eq!(machine.mem.read_u64(0x50000100), 0xabababababababab);
        machine.cpu.r[9] = 0;
        machine.cpu.r[7] = 0x50000ffc;
        machine.mem.write(0x50000ffc, &[0xef; 4]);
        machine.call_compute_builder("sceGnmDispatchDirect");
        assert_eq!(machine.cpu.r[0], FAULT);
        let mut guard = [0; 4];
        machine.mem.read(0x50000ffc, &mut guard);
        assert_eq!(guard, [0xef; 4]);
        machine.cpu.r[7] = 0x50000100;
        machine.cpu.r[6] = 25;
        machine.cpu.r[1] = 1;
        machine.call_compute_builder("sceGnmSetCsShaderWithModifier");
        assert_eq!(machine.cpu.r[0], 0x8002004e);
        assert_eq!(machine.mem.read_u64(0x50000100), 0xabababababababab);
    }
}
