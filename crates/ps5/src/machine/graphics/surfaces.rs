use super::*;

pub(super) struct Surface {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    address: u64,
    bgra: bool,
}

impl Surface {
    pub(super) fn read(mem: &Mem, registers: &BTreeMap<u32, u32>) -> Result<Self, String> {
        let register = |address| registers.get(&address).copied().unwrap_or(0);
        let address = (register(0xa318) as u64) << 8;
        let attributes = register(0xa31d);
        let tile_mode = attributes & 31;
        if !matches!(tile_mode, 8 | 31) { return Err(format!("GPU render-target tile mode {tile_mode:#x} is not implemented; linear aligned/general surfaces only")); }
        let format = register(0xa31c);
        if address == 0 || (format >> 2) & 31 != 10 || (format >> 8) & 7 != 0 || format & 0x6003 != 0 || attributes & 0x3f000 != 0 || register(0xa31b) != 0 { return Err("GPU render-target format, compression, samples or array view is not implemented".into()); }
        let swap = (format >> 11) & 3;
        if swap > 1 { return Err("GPU render-target component order is not implemented".into()); }
        let width = ((register(0xa319) & 0x7ff) + 1) * 8;
        let pixels = ((register(0xa31a) & 0x3fffff) as u64 + 1) * 64;
        if pixels % width as u64 != 0 || pixels > 4194304 { return Err("GPU render-target allocation size is invalid or exceeds the pixel limit".into()); }
        let height = (pixels / width as u64) as u32;
        let mut rgba = vec![0; pixels as usize * 4];
        if !mem.read(address, &mut rgba) { return Err(format!("GPU render target is unmapped at {address:#x}")); }
        if swap == 1 { for pixel in rgba.chunks_exact_mut(4) { pixel.swap(0, 2); } }
        Ok(Self { width, height, rgba, address, bgra: swap == 1 })
    }

    pub(super) fn write(&self, mem: &mut Mem, rgba: &[u8]) -> Result<(), String> {
        if rgba.len() != self.rgba.len() { return Err("GPU render-target output size is invalid".into()); }
        let mut bytes = rgba.to_vec();
        if self.bgra { for pixel in bytes.chunks_exact_mut(4) { pixel.swap(0, 2); } }
        let mut probe = vec![0; bytes.len()];
        if !mem.read(self.address, &mut probe) || !mem.write(self.address, &bytes) { return Err("GPU render-target write is unmapped".into()); }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_surface_round_trips_bgra_without_touching_allocation_guards() {
        let mut mem = Mem::new().unwrap();
        mem.map(0x50000000, 4096, true, false).unwrap();
        mem.write(0x50000000, &[0xab; 1024]);
        let registers = BTreeMap::from([(0xa318, 0x500001), (0xa319, 0), (0xa31a, 0), (0xa31b, 0), (0xa31c, 0x8828), (0xa31d, 8)]);
        mem.write(0x50000100, &[3, 2, 1, 255]);
        let surface = Surface::read(&mem, &registers).unwrap();
        assert_eq!((surface.width, surface.height), (8, 8));
        assert_eq!(&surface.rgba[..4], &[1, 2, 3, 255]);
        let mut output = surface.rgba.clone();
        output[..4].copy_from_slice(&[10, 20, 30, 40]);
        surface.write(&mut mem, &output).unwrap();
        let mut actual = [0; 4];
        mem.read(0x50000100, &mut actual);
        assert_eq!(actual, [30, 20, 10, 40]);
        assert_eq!(mem.read_u64(0x500000f8), 0xabababababababab);
        assert_eq!(mem.read_u64(0x50000200), 0xabababababababab);
        let mut invalid = registers.clone();
        invalid.insert(0xa31d, 10);
        assert!(Surface::read(&mem, &invalid).err().unwrap().contains("tile mode 0xa"));
        invalid.insert(0xa31d, 8);
        invalid.insert(0xa31c, 0x8c28);
        assert!(Surface::read(&mem, &invalid).is_err());
    }
}
