use std::ops::{Deref, DerefMut};

pub struct Ram {
    #[cfg(windows)]
    pointer: std::ptr::NonNull<u8>,
    #[cfg(not(windows))]
    bytes: Vec<u8>,
    length: usize,
    capacity: usize,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn VirtualAlloc(address: *mut u8, size: usize, allocation: u32, protection: u32) -> *mut u8;
    fn VirtualFree(address: *mut u8, size: usize, operation: u32) -> i32;
}

#[cfg(windows)]
unsafe impl Send for Ram {}

impl Ram {
    pub fn new(length: usize, capacity: usize) -> Result<Self, String> {
        if length > capacity || capacity == 0 || capacity > isize::MAX as usize {
            return Err("Invalid guest RAM capacity.".into());
        }
        #[cfg(windows)]
        let mut ram = {
            let pointer = unsafe { VirtualAlloc(std::ptr::null_mut(), capacity, 0x2000, 4) };
            let pointer = std::ptr::NonNull::new(pointer.cast::<u8>())
                .ok_or_else(|| format!("Could not reserve guest RAM: {}", std::io::Error::last_os_error()))?;
            Self { pointer, length: 0, capacity }
        };
        #[cfg(not(windows))]
        let mut ram = Self { bytes: Vec::new(), length: 0, capacity };
        ram.grow(length)?;
        Ok(ram)
    }

    pub fn grow(&mut self, length: usize) -> Result<(), String> {
        if length < self.length || length > self.capacity || length & 4095 != 0 {
            return Err("Invalid guest RAM growth.".into());
        }
        if length == self.length { return Ok(()); }
        #[cfg(windows)]
        {
            let address = unsafe { self.pointer.as_ptr().add(self.length) };
            let result = unsafe { VirtualAlloc(address, length - self.length, 0x1000, 4) };
            if result.is_null() {
                return Err(format!("Could not commit guest RAM: {}", std::io::Error::last_os_error()));
            }
        }
        #[cfg(not(windows))]
        {
            self.bytes.try_reserve_exact(length - self.length)
                .map_err(|error| format!("Could not allocate guest RAM: {error}"))?;
            self.bytes.resize(length, 0);
        }
        self.length = length;
        Ok(())
    }
}

impl Deref for Ram {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        #[cfg(windows)]
        { unsafe { std::slice::from_raw_parts(self.pointer.as_ptr(), self.length) } }
        #[cfg(not(windows))]
        { &self.bytes }
    }
}

impl DerefMut for Ram {
    fn deref_mut(&mut self) -> &mut [u8] {
        #[cfg(windows)]
        { unsafe { std::slice::from_raw_parts_mut(self.pointer.as_ptr(), self.length) } }
        #[cfg(not(windows))]
        { &mut self.bytes }
    }
}

#[cfg(windows)]
impl Drop for Ram {
    fn drop(&mut self) {
        unsafe { VirtualFree(self.pointer.as_ptr(), 0, 0x8000); }
    }
}

#[cfg(test)]
mod tests {
    use super::Ram;

    #[test]
    fn growth_preserves_data_and_zeroes_new_pages() {
        let mut ram = Ram::new(4096, 16384).unwrap();
        ram[4095] = 73;
        ram.grow(12288).unwrap();
        assert_eq!(ram[4095], 73);
        assert!(ram[4096..].iter().all(|byte| *byte == 0));
        assert!(ram.grow(20480).is_err());
        assert!(ram.grow(4096).is_err());
        assert!(ram.grow(12289).is_err());
        assert_eq!(ram.len(), 12288);
    }
}
