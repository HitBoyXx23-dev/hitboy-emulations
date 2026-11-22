use super::*;

impl Machine {
    fn render_shader_commands(&self, vertex: bool, enhanced: bool) -> Result<Vec<u32>, u64> {
        let address = self.arg(2);
        let mut registers = vec![0u32; if vertex { 7 } else { 12 }];
        if address == 0 {
            if vertex { return Err(FAULT); }
            let mut commands = vec![0xc0017600, 8, 0, 0xc0017600, 9, 0, 0xc0016900, 0x203, 0];
            if enhanced { commands.extend([0xc0016900, 0x8f, 15]); }
            return Ok(commands);
        } else {
            let mut bytes = vec![0; registers.len() * 4];
            if !self.mem.read(address, &mut bytes) { return Err(FAULT); }
            for (destination, source) in registers.iter_mut().zip(bytes.chunks_exact(4)) {
                *destination = u32::from_le_bytes(source.try_into().unwrap());
            }
            if registers[1] != 0 { return Err(INVALID); }
        }
        let mapping: &[u32] = if vertex {
            let modifier = self.arg(3) as u32;
            let allowed = 0x030003c0;
            if modifier & !allowed != 0 { return Err(INVALID); }
            if modifier != 0 { registers[2] = registers[2] & !allowed | modifier; }
            &[0x2c48, 0x2c49, 0x2c4a, 0x2c4b, 0xa1b1, 0xa1c3, 0xa207]
        } else {
            &[0x2c08, 0x2c09, 0x2c0a, 0x2c0b, 0xa1c4, 0xa1c5, 0xa1b3, 0xa1b4, 0xa1b6, 0xa1b8, 0xa203, 0xa08f]
        };
        let mut commands = Vec::with_capacity(mapping.len() * 3);
        for (register, value) in mapping.iter().zip(registers) {
            let (opcode, base) = if *register < 0x3000 { (0x76, 0x2c00) } else { (0x69, 0xa000) };
            commands.extend([0xc0010000 | opcode << 8, register - base, value]);
        }
        Ok(commands)
    }

    pub(super) fn call_render_builder(&mut self, name: &str) -> bool {
        if !matches!(name, "sceGnmSetVsShader" | "sceGnmSetPsShader" | "sceGnmSetPsShader350" | "sceGnmDrawIndex" | "sceGnmDrawIndexAuto") { return false; }
        let capacity = self.arg(1) as u32 as usize;
        let minimum = match name {
            "sceGnmSetVsShader" => 29,
            "sceGnmSetPsShader" | "sceGnmSetPsShader350" => 40,
            "sceGnmDrawIndex" => 10,
            _ => 7,
        };
        let commands = if !(minimum..=64).contains(&capacity) {
            Err(INVALID)
        } else if name.starts_with("sceGnmSet") {
            self.render_shader_commands(name == "sceGnmSetVsShader", name == "sceGnmSetPsShader350")
        } else {
            let indexed = name == "sceGnmDrawIndex";
            let flags = self.arg(if indexed { 4 } else { 3 }) as u32;
            let address = self.arg(3);
            if flags != 0 { Err(0x8002004e) }
            else if indexed && (address == 0 || address & 1 != 0) { Err(INVALID) }
            else {
                let count = self.arg(2) as u32;
                Ok(if indexed {
                    vec![0xc0042700, count, address as u32, (address >> 32) as u32, count, 0]
                } else { vec![0xc0012d00, count, 2] })
            }
        };
        let result = match commands {
            Ok(mut words) => {
                words.resize(capacity, 0x80000000);
                self.write_graphics_commands(&words)
            },
            Err(error) => error,
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
        machine.cpu.r[7] = 0x50000108;
        machine.cpu.r[2] = 0x50000200;
        machine
    }

    fn submit(machine: &mut Machine, size: usize) {
        let mut commands = vec![0; size * 4];
        assert!(machine.mem.read(0x50000108, &mut commands));
        packet_histogram(&commands).unwrap();
        machine.graphics.processor.enqueue(vec![commands]).unwrap();
        machine.pump_graphics();
    }

    #[test]
    fn vertex_and_pixel_bindings_preserve_guards_and_apply_the_resource_modifier() {
        let mut machine = machine();
        machine.mem.write(0x50000100, &[0xa5; 176]);
        let vertex = [0x203bf9cu32, 0, 0x00ff03ff, 0x90, 0x12, 0x34, 0x56];
        machine.mem.write(0x50000200, &vertex.iter().flat_map(|value| value.to_le_bytes()).collect::<Vec<_>>());
        machine.cpu.r[6] = 29;
        machine.cpu.r[1] = 0x03000040;
        assert!(machine.call_render_builder("sceGnmSetVsShader"));
        assert_eq!(machine.cpu.r[0], 0);
        submit(&mut machine, 29);
        assert!(machine.runtime_error.is_none());
        assert_eq!(machine.graphics.processor.register(0x2c4a), 0x03ff007f);
        assert_eq!(machine.graphics.processor.register(0x2c48), vertex[0]);
        assert_eq!(machine.graphics.processor.register(0xa207), 0x56);
        assert_eq!(machine.graphics.processor.register(0xa1b1), 0x12);
        assert_eq!(machine.graphics.processor.register(0xa1c3), 0x34);
        assert_eq!(machine.mem.read_u64(0x50000100), 0xa5a5a5a5a5a5a5a5);
        assert_eq!(machine.mem.read_u64(0x5000017c), 0xa5a5a5a5a5a5a5a5);
        let pixel = [0x203c000u32, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 15];
        machine.mem.write(0x50000200, &pixel.iter().flat_map(|value| value.to_le_bytes()).collect::<Vec<_>>());
        machine.cpu.r[6] = 40;
        machine.call_render_builder("sceGnmSetPsShader350");
        assert_eq!(machine.cpu.r[0], 0);
        submit(&mut machine, 40);
        assert_eq!(machine.graphics.processor.register(0x2c08), pixel[0]);
        assert_eq!(machine.graphics.processor.register(0xa1b4), 6);
        assert_eq!(machine.graphics.processor.register(0xa08f), 15);
        assert_eq!(machine.video.flips, 0);
        assert_eq!(crate::nid::lookup("5uFKckiJYRM"), Some("sceGnmSetPsShader350"));
    }

    #[test]
    fn indexed_draw_emits_a_valid_packet_and_stops_before_any_unrendered_completion() {
        let mut machine = machine();
        machine.cpu.r[6] = 10;
        machine.cpu.r[2] = 6;
        machine.cpu.r[1] = 0x207208800;
        machine.cpu.r[8] = 0;
        machine.cpu.r[9] = 1;
        machine.call_render_builder("sceGnmDrawIndex");
        assert_eq!(machine.cpu.r[0], 0);
        let mut packet = [0; 24];
        machine.mem.read(0x50000108, &mut packet);
        let words = packet.chunks_exact(4).map(|word| u32::from_le_bytes(word.try_into().unwrap())).collect::<Vec<_>>();
        assert_eq!(words, [0xc0042700, 6, 0x07208800, 2, 6, 0]);
        submit(&mut machine, 10);
        assert!(machine.runtime_error.as_ref().unwrap().contains("opcode 0x27"));
        assert_eq!(machine.video.flips, 0);
    }

    #[test]
    fn invalid_render_inputs_leave_command_memory_untouched() {
        let mut machine = machine();
        machine.mem.write(0x50000108, &[0xab; 160]);
        machine.cpu.r[6] = 29;
        machine.cpu.r[1] = 1;
        machine.call_render_builder("sceGnmSetVsShader");
        assert_eq!(machine.cpu.r[0], INVALID);
        assert_eq!(machine.mem.read_u64(0x50000108), 0xabababababababab);
        machine.cpu.r[6] = 40;
        machine.cpu.r[2] = 0x60000000;
        machine.call_render_builder("sceGnmSetPsShader350");
        assert_eq!(machine.cpu.r[0], FAULT);
        assert_eq!(machine.mem.read_u64(0x50000108), 0xabababababababab);
        machine.cpu.r[7] = 0x50000ffc;
        machine.cpu.r[2] = 0;
        machine.mem.write(0x50000ffc, &[0xef; 4]);
        machine.call_render_builder("sceGnmSetPsShader350");
        assert_eq!(machine.cpu.r[0], FAULT);
        let mut guard = [0; 4];
        machine.mem.read(0x50000ffc, &mut guard);
        assert_eq!(guard, [0xef; 4]);
    }
}
