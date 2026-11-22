use super::Machine;

#[cfg(windows)]
fn fill_random(buffer: &mut [u8]) -> bool {
    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(provider: *mut std::ffi::c_void, buffer: *mut u8, length: u32, flags: u32) -> i32;
    }
    unsafe { BCryptGenRandom(std::ptr::null_mut(), buffer.as_mut_ptr(), buffer.len() as u32, 2) >= 0 }
}

#[cfg(unix)]
fn fill_random(buffer: &mut [u8]) -> bool {
    use std::io::Read;
    std::fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(buffer)).is_ok()
}

#[cfg(not(any(windows, unix)))]
fn fill_random(_buffer: &mut [u8]) -> bool { false }

impl Machine {
    pub(super) fn call_random(&mut self, name: &str) -> bool {
        if name != "sceRandomGetRandomNumber" { return false; }
        let (address, length) = (self.arg(0), self.arg(1));
        if length > 64 || address == 0 || !(0..length).all(|offset| address.checked_add(offset).and_then(|value| self.mem.phys(value)).is_some()) {
            self.ret(0x817C0016);
            return true;
        }
        let mut bytes = [0u8; 64];
        if !fill_random(&mut bytes[..length as usize]) {
            self.ret(0x817C00FF);
            return true;
        }
        self.mem.write(address, &bytes[..length as usize]);
        self.ret(0);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_random_fills_only_valid_bounded_guest_buffers() {
        let mut machine = Machine::load(&super::super::tests::hello_program()).unwrap();
        let address = machine.heap_alloc(80, 16);
        machine.mem.write(address, &[0xCC; 80]);
        machine.cpu.r[7] = address;
        machine.cpu.r[6] = 64;
        assert!(machine.call_random("sceRandomGetRandomNumber"));
        assert_eq!(machine.cpu.r[0], 0);
        let mut bytes = [0u8; 80];
        machine.mem.read(address, &mut bytes);
        assert!(bytes[..64].iter().any(|&value| value != 0xCC));
        assert_eq!(&bytes[64..], &[0xCC; 16]);
        machine.cpu.r[6] = 65;
        machine.call_random("sceRandomGetRandomNumber");
        assert_eq!(machine.cpu.r[0], 0x817C0016);
        let mut unchanged = [0u8; 80];
        machine.mem.read(address, &mut unchanged);
        assert_eq!(unchanged, bytes);
        machine.cpu.r[7] = u64::MAX;
        machine.cpu.r[6] = 16;
        machine.call_random("sceRandomGetRandomNumber");
        assert_eq!(machine.cpu.r[0], 0x817C0016);
        assert_eq!(crate::nid::lookup("PI7jIZj4pcE"), Some("sceRandomGetRandomNumber"));
    }
}
