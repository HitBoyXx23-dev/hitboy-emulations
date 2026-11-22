use super::*;

const INVALID: u64 = 0x80B2_0004;
const NOT_INITIALIZED: u64 = 0x80B2_0005;
const ALREADY_INITIALIZED: u64 = 0x80B2_0006;
const BAD_HANDLE: u64 = 0x80B2_0009;
const BAD_POINTER: u64 = 0x80B2_000A;
const BAD_SIZE: u64 = 0x80B2_000B;
const NOT_SUPPORTED: u64 = 0x80B2_000E;

impl Machine {
    fn initialize_playgo(&mut self) -> u64 {
        let mut parameters = [0; 16];
        if self.arg(0) == 0 || !self.mem.read(self.arg(0), &mut parameters) { return BAD_POINTER; }
        let address = u64::from_le_bytes(parameters[..8].try_into().unwrap());
        let length = u32::from_le_bytes(parameters[8..12].try_into().unwrap()) as u64;
        if address == 0 { return BAD_POINTER; }
        if length < 0x200000 || length > RAM_SIZE as u64 { return BAD_SIZE; }
        let Some(end) = address.checked_add(length - 1) else { return BAD_POINTER; };
        if self.mem.phys(address).is_none() || self.mem.phys(end).is_none()
            || ((address >> 12)..=(end >> 12)).any(|page| self.mem.phys(page << 12).is_none()) { return BAD_POINTER; }
        if self.runtime.playgo_buffer.is_some() { return ALREADY_INITIALIZED; }
        self.runtime.playgo_buffer = Some((address, length));
        0
    }

    pub(super) fn call_playgo(&mut self, name: &str) -> bool {
        let result = match name {
            "scePlayGoInitialize" => self.initialize_playgo(),
            "scePlayGoOpen" => {
                let mut output = [0; 4];
                if self.arg(0) == 0 || !self.mem.read(self.arg(0), &mut output) { BAD_POINTER }
                else if self.arg(1) != 0 { INVALID }
                else if self.runtime.playgo_buffer.is_none() { NOT_INITIALIZED }
                else { NOT_SUPPORTED }
            }
            "scePlayGoGetLocus" => {
                if self.runtime.playgo_buffer.is_none() { NOT_INITIALIZED } else { BAD_HANDLE }
            }
            "scePlayGoTerminate" => {
                if self.runtime.playgo_buffer.take().is_some() { 0 } else { NOT_INITIALIZED }
            }
            _ => return false,
        };
        self.ret(result);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpacked_games_report_unavailable_install_tracking_without_inventing_a_handle() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let buffer = machine.heap_alloc(0x200000, 0x4000);
        let parameters = machine.heap_alloc(32, 16);
        machine.mem.write(parameters, &[0; 32]);
        machine.mem.write_u64(parameters, buffer);
        machine.mem.write(parameters + 8, &0x200000u32.to_le_bytes());
        machine.cpu.r[7] = parameters;
        assert_eq!(machine.initialize_playgo(), 0);
        assert_eq!(machine.initialize_playgo(), ALREADY_INITIALIZED);
        machine.mem.write(parameters + 16, &[0xCC; 8]);
        machine.cpu.r[7] = parameters + 16;
        machine.cpu.r[6] = 0;
        assert!(machine.call_playgo("scePlayGoOpen"));
        assert_eq!(machine.cpu.r[0], NOT_SUPPORTED);
        assert_eq!(machine.mem.read_u64(parameters + 16), 0xCCCCCCCCCCCCCCCC);
        machine.call_playgo("scePlayGoGetLocus");
        assert_eq!(machine.cpu.r[0], BAD_HANDLE);
        machine.call_playgo("scePlayGoTerminate");
        assert_eq!(machine.cpu.r[0], 0);
        machine.call_playgo("scePlayGoGetLocus");
        assert_eq!(machine.cpu.r[0], NOT_INITIALIZED);
        machine.cpu.r[7] = parameters;
        machine.mem.write(parameters + 8, &0x100u32.to_le_bytes());
        assert_eq!(machine.initialize_playgo(), BAD_SIZE);
        machine.mem.write(parameters + 8, &0x200000u32.to_le_bytes());
        machine.mem.write_u64(parameters, u64::MAX - 16);
        assert_eq!(machine.initialize_playgo(), BAD_POINTER);
        assert!(machine.runtime.playgo_buffer.is_none());
    }
}
