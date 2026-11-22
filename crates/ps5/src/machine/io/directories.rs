use super::*;

struct DirectoryEntry {
    name: String,
    kind: u8,
}

pub(super) struct GuestDirectory {
    path: String,
    entries: Vec<DirectoryEntry>,
    position: usize,
}

impl Machine {
    pub(super) fn open_guest_directory(&self, path: &str) -> Result<Option<GuestDirectory>, u32> {
        let relative = if path == "/app0" { "" } else { path.strip_prefix("/app0/").unwrap_or(path) };
        let relative = Path::new(relative);
        let mut entries = std::collections::BTreeMap::new();
        match self.guest_path(path) {
            Ok(resolved) => {
                if !resolved.is_dir() { return Ok(None); }
                let root = self.io.root.as_ref().ok_or(2u32)?;
                for item in std::fs::read_dir(resolved).map_err(|_| 5u32)? {
                    let item = item.map_err(|_| 5u32)?;
                    let name = item.file_name().into_string().map_err(|_| 63u32)?;
                    if name.len() > 255 { return Err(63); }
                    let resolved = match item.path().canonicalize() { Ok(path) => path, Err(_) => continue };
                    if !resolved.starts_with(root) { continue; }
                    let metadata = std::fs::metadata(resolved).map_err(|_| 5u32)?;
                    let kind = if metadata.is_dir() { 4 } else if metadata.is_file() { 8 } else { continue; };
                    entries.insert(name.to_ascii_lowercase(), DirectoryEntry { name, kind });
                    if entries.len() > 20000 { return Err(24); }
                }
            }
            Err(2) if self.io.archives.directory_exists(relative) => {}
            Err(2) => return Ok(None),
            Err(error) => return Err(error),
        }
        for (name, directory) in self.io.archives.directory_entries(relative).map_err(|_| 22u32)? {
            if name.len() > 255 { return Err(63); }
            entries.entry(name.to_ascii_lowercase()).or_insert(DirectoryEntry { name, kind: if directory { 4 } else { 8 } });
            if entries.len() > 20000 { return Err(24); }
        }
        let mut children = vec![DirectoryEntry { name: ".".into(), kind: 4 }, DirectoryEntry { name: "..".into(), kind: 4 }];
        children.extend(entries.into_values());
        Ok(Some(GuestDirectory { path: path.to_string(), entries: children, position: 0 }))
    }

    pub(super) fn read_guest_directory(&mut self) -> Result<u64, u32> {
        let (descriptor, output, count) = (self.arg(0) as i32, self.arg(1), self.arg(2) as u32 as i32);
        if count <= 0 { return Err(22); }
        let count = (count as usize).min(TRANSFER_LIMIT);
        if output == 0 || !self.mem.read(output, &mut vec![0; count]) { return Err(14); }
        let directory = match self.io.files.get_mut(&descriptor) {
            Some(GuestFile::Directory(directory)) => directory,
            Some(_) => return Err(20),
            None => return Err(9),
        };
        let mut bytes = Vec::new();
        let mut position = directory.position;
        for entry in &directory.entries[position..] {
            let length = (8 + entry.name.len() + 1 + 3) & !3;
            if bytes.len() + length > count { break; }
            let start = bytes.len();
            bytes.resize(start + length, 0);
            bytes[start..start + 4].copy_from_slice(&((position + 1) as u32).to_le_bytes());
            bytes[start + 4..start + 6].copy_from_slice(&(length as u16).to_le_bytes());
            bytes[start + 6] = entry.kind;
            bytes[start + 7] = entry.name.len() as u8;
            bytes[start + 8..start + 8 + entry.name.len()].copy_from_slice(entry.name.as_bytes());
            position += 1;
        }
        if bytes.is_empty() && position < directory.entries.len() { return Err(22); }
        if !self.mem.write(output, &bytes) { return Err(14); }
        let activity = format!("descriptor={descriptor} path={:?} from={} to={position} total={} bytes={}", directory.path, directory.position, directory.entries.len(), bytes.len());
        directory.position = position;
        if self.io.directory_activity.len() == 16 { self.io.directory_activity.pop_front(); }
        self.io.directory_activity.push_back(activity);
        Ok(bytes.len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_directory_records_are_bounded_resumable_and_read_only() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("hitboy-directory-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("folder")).unwrap();
        std::fs::write(root.join("alpha.bin"), b"alpha").unwrap();
        std::fs::write(root.join("caf\u{00e9}.bin"), b"unicode").unwrap();
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        machine.set_game_root(&root).unwrap();
        let storage = machine.heap_alloc(512, 16);
        machine.mem.write(storage, b"/app0\0");
        machine.cpu.r[7] = storage;
        machine.cpu.r[6] = 0x20000;
        assert!(machine.call_io("sceKernelOpen"));
        let descriptor = machine.cpu.r[0];
        assert!((3..1024).contains(&descriptor));
        machine.cpu.r[7] = descriptor;
        machine.cpu.r[6] = storage + 136;
        machine.mem.write(storage + 128, &[0xCC; 136]);
        machine.call_io("sceKernelFstat");
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.mem.read_u64(storage + 144) & 0xFFFF, 0x416D);
        assert_eq!(machine.mem.read_u64(storage + 128), 0xCCCCCCCCCCCCCCCC);
        assert_eq!(machine.mem.read_u64(storage + 256), 0xCCCCCCCCCCCCCCCC);
        machine.cpu.r[7] = descriptor;
        machine.cpu.r[6] = storage + 256;
        machine.cpu.r[2] = 8;
        assert_eq!(machine.read_guest_directory(), Err(22));
        machine.cpu.r[6] = u64::MAX;
        machine.cpu.r[2] = 24;
        assert_eq!(machine.read_guest_directory(), Err(14));
        machine.cpu.r[6] = storage + 256;
        let mut names = Vec::new();
        loop {
            machine.mem.write(storage + 248, &[0xCC; 40]);
            let count = machine.read_guest_directory().unwrap() as usize;
            assert!(count <= 24);
            assert_eq!(machine.mem.read_u64(storage + 248), 0xCCCCCCCCCCCCCCCC);
            assert_eq!(machine.mem.read_u64(storage + 280), 0xCCCCCCCCCCCCCCCC);
            if count == 0 { break; }
            let mut data = vec![0; count];
            machine.mem.read(storage + 256, &mut data);
            let mut position = 0;
            while position < count {
                let length = u16::from_le_bytes(data[position + 4..position + 6].try_into().unwrap()) as usize;
                let name_length = data[position + 7] as usize;
                assert!(length >= 12 && length & 3 == 0 && position + length <= count);
                assert_eq!(data[position + 8 + name_length], 0);
                names.push((String::from_utf8(data[position + 8..position + 8 + name_length].to_vec()).unwrap(), data[position + 6]));
                position += length;
            }
        }
        assert_eq!(names, vec![(".".into(), 4), ("..".into(), 4), ("alpha.bin".into(), 8), ("caf\u{00e9}.bin".into(), 8), ("folder".into(), 4)]);
        assert_eq!(machine.read_guest_file(descriptor as i32, storage + 256, 8), Err(21));
        machine.call_io("sceKernelClose");
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.read_guest_directory(), Err(9));
        machine.mem.write(storage, b"/app0/../outside\0");
        machine.cpu.r[7] = storage;
        machine.cpu.r[6] = 0x20000;
        assert_eq!(machine.open_guest_file(), Err(13));
        machine.mem.write(storage, b"/app0/alpha.bin\0");
        assert_eq!(machine.open_guest_file(), Err(20));
        std::fs::remove_file(root.join("alpha.bin")).unwrap();
        std::fs::remove_file(root.join("caf\u{00e9}.bin")).unwrap();
        std::fs::remove_dir(root.join("folder")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
