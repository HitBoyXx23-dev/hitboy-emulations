use super::*;

fn write_time(record: &mut [u8; 120], offset: usize, time: Result<std::time::SystemTime, std::io::Error>) {
    let Some(time) = time.ok() else { return; };
    let (seconds, nanoseconds) = match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => {
            let Ok(seconds) = i64::try_from(duration.as_secs()) else { return; };
            (seconds, duration.subsec_nanos() as i64)
        }
        Err(error) => {
            let duration = error.duration();
            let Ok(seconds) = i64::try_from(duration.as_secs()) else { return; };
            let fraction = duration.subsec_nanos() as i64;
            let Some(seconds) = seconds.checked_neg().and_then(|value| value.checked_sub((fraction != 0) as i64)) else { return; };
            (seconds, if fraction == 0 { 0 } else { 1000000000 - fraction })
        }
    };
    record[offset..offset + 8].copy_from_slice(&seconds.to_le_bytes());
    record[offset + 8..offset + 16].copy_from_slice(&nanoseconds.to_le_bytes());
}

impl Machine {
    pub(super) fn stat_guest_descriptor(&mut self) -> Result<u64, u32> {
        let (descriptor, output) = (self.arg(0) as i32, self.arg(1));
        if output == 0 || !self.mem.read(output, &mut [0; 120]) { return Err(14); }
        let mut record = [0; 120];
        let (size, mode) = match self.io.files.get(&descriptor) {
            Some(GuestFile::Directory(_)) => (0, 0x416Du16),
            Some(GuestFile::File(file)) => {
                if let pak::ReadFile::Host(file) = file {
                    let metadata = file.metadata().map_err(|_| 5u32)?;
                    write_time(&mut record, 24, metadata.accessed());
                    write_time(&mut record, 40, metadata.modified());
                    write_time(&mut record, 104, metadata.created());
                }
                (file.len().map_err(|_| 5u32)?, 0x8124)
            }
            Some(GuestFile::Null | GuestFile::Console) => (0, 0x21B6),
            None if (0..=2).contains(&descriptor) => (0, 0x21B6),
            None => return Err(9),
        };
        if size > i64::MAX as u64 { return Err(84); }
        record[..4].copy_from_slice(&1u32.to_le_bytes());
        record[4..8].copy_from_slice(&(descriptor as u32 + 1).to_le_bytes());
        record[8..10].copy_from_slice(&mode.to_le_bytes());
        record[10..12].copy_from_slice(&1u16.to_le_bytes());
        record[72..80].copy_from_slice(&size.to_le_bytes());
        record[80..88].copy_from_slice(&((size + 511) / 512).to_le_bytes());
        record[88..92].copy_from_slice(&4096u32.to_le_bytes());
        if !self.mem.write(output, &record) { return Err(14); }
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_stat_reports_file_size_and_times_without_overwriting_guards() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("hitboy-fstat-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("asset.bin");
        std::fs::write(&path, b"content").unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        let time = std::time::UNIX_EPOCH + std::time::Duration::new(1700000000, 123456700);
        file.set_times(std::fs::FileTimes::new().set_modified(time)).unwrap();
        drop(file);
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        machine.set_game_root(&root).unwrap();
        let storage = machine.heap_alloc(512, 16);
        machine.mem.write(storage, b"/app0/asset.bin\0");
        machine.cpu.r[7] = storage;
        machine.cpu.r[6] = 0;
        let descriptor = machine.open_guest_file().unwrap();
        let output = storage + 136;
        machine.mem.write(output - 8, &[0xCC; 136]);
        machine.cpu.r[7] = descriptor;
        machine.cpu.r[6] = output;
        assert_eq!(machine.stat_guest_descriptor(), Ok(0));
        assert_eq!(machine.mem.read_u64(output + 8) & 0xFFFF, 0x8124);
        assert_eq!(machine.mem.read_u64(output + 40), 1700000000);
        assert_eq!(machine.mem.read_u64(output + 48), 123456700);
        assert_eq!(machine.mem.read_u64(output + 72), 7);
        assert_eq!(machine.mem.read_u64(output + 80), 1);
        assert_eq!(machine.mem.read_u64(output - 8), 0xCCCCCCCCCCCCCCCC);
        assert_eq!(machine.mem.read_u64(output + 120), 0xCCCCCCCCCCCCCCCC);
        machine.call_io("sceKernelClose");
        machine.mem.write(output, &[0xCC; 120]);
        assert_eq!(machine.stat_guest_descriptor(), Err(9));
        assert_eq!(machine.mem.read_u64(output), 0xCCCCCCCCCCCCCCCC);
        machine.cpu.r[6] = 0;
        assert_eq!(machine.stat_guest_descriptor(), Err(14));
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
