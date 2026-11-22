use super::*;

const BAD_POINTER: u32 = 0x8082_0006;
const BAD_SIZE: u32 = 0x8082_0008;

#[derive(Default)]
pub(super) struct Fios {
    parameters: Option<[u8; 176]>,
    files: HashMap<u32, pak::ReadFile>,
    next_handle: u32,
    handle_storage: HashMap<u32, (u64, u64)>,
    opened: u64,
    bytes_read: u64,
    errors: u64,
    paths: Vec<String>,
    recent_paths: std::collections::VecDeque<String>,
    metadata_queries: std::collections::VecDeque<String>,
    directory_queries: std::collections::VecDeque<String>,
}

fn kernel_error(error: u32) -> u32 { 0x8002_0000 | error }

impl Machine {
    fn initialize_fios(&mut self) -> Result<u64, u32> {
        let mut parameters = [0; 176];
        if self.arg(0) == 0 || !self.mem.read(self.arg(0), &mut parameters) { return Err(BAD_POINTER); }
        let header = u32::from_le_bytes(parameters[..4].try_into().unwrap());
        if (header >> 1) & 0x7FFF != 176 { return Err(BAD_SIZE); }
        if parameters[25] >= 4 { return Err(0x8082_0021); }
        self.io.fios.parameters = Some(parameters);
        Ok(0)
    }

    fn fios_path(&self, address: u64) -> Result<String, u32> {
        let mut text = Vec::new();
        for index in 0..4096 {
            let mut byte = [0];
            if address == 0 || !address.checked_add(index).is_some_and(|address| self.mem.read(address, &mut byte)) { return Err(BAD_POINTER); }
            if byte[0] == 0 { return String::from_utf8(text).map_err(|_| kernel_error(22)); }
            text.push(byte[0]);
        }
        Err(kernel_error(63))
    }

    fn fios_directory_exists(&self) -> u64 {
        if self.arg(0) != 0 { return 0; }
        let Ok(path) = self.fios_path(self.arg(1)) else { return 0; };
        match self.guest_path(&path) {
            Ok(resolved) => resolved.is_dir() as u64,
            Err(2) => self.io.archives.directory_exists(Path::new(path.strip_prefix("/app0/").unwrap_or(&path))) as u64,
            Err(_) => 0,
        }
    }

    fn fios_file_size(&self) -> Result<u64, u32> {
        if self.arg(0) != 0 { return Err(kernel_error(45)); }
        let path = self.fios_path(self.arg(1))?;
        self.open_read_only_file(&path).map_err(kernel_error)?.len().map_err(|_| kernel_error(5))
    }

    fn stat_fios_file(&mut self) -> Result<u64, u32> {
        let (attribute, path, output) = (self.arg(0), self.arg(1), self.arg(2));
        if output == 0 || !self.mem.read(output, &mut [0; 80]) { return Err(BAD_POINTER); }
        if attribute != 0 { return Err(kernel_error(45)); }
        let text = self.fios_path(path)?;
        let mut record = [0u8; 80];
        let (size, directory) = match self.guest_path(&text) {
            Ok(path) => {
                let metadata = std::fs::metadata(path).map_err(|_| kernel_error(5))?;
                if !metadata.is_file() && !metadata.is_dir() { return Err(kernel_error(45)); }
                for (index, time) in [metadata.accessed(), metadata.modified(), metadata.created()].into_iter().enumerate() {
                    let nanoseconds = time.ok().and_then(|time| {
                        let value = match time.duration_since(std::time::UNIX_EPOCH) {
                            Ok(duration) => i128::try_from(duration.as_nanos()).ok()?,
                            Err(error) => -i128::try_from(error.duration().as_nanos()).ok()?,
                        };
                        i64::try_from(value).ok()
                    }).unwrap_or(0);
                    record[8 + index * 8..16 + index * 8].copy_from_slice(&nanoseconds.to_le_bytes());
                }
                (if metadata.is_dir() { 0 } else { metadata.len() }, metadata.is_dir())
            }
            Err(2) => {
                let relative = Path::new(text.strip_prefix("/app0/").unwrap_or(&text));
                if let Some(size) = self.io.archives.file_size(relative) { (size, false) }
                else if self.io.archives.directory_exists(relative) { (0, true) }
                else { return Err(kernel_error(2)); }
            }
            Err(error) => return Err(kernel_error(error)),
        };
        record[..8].copy_from_slice(&size.to_le_bytes());
        record[32..36].copy_from_slice(&(2u32 | directory as u32).to_le_bytes());
        let mode = if directory { 0x416Du64 } else { 0x8124u64 };
        record[72..80].copy_from_slice(&mode.to_le_bytes());
        if !self.mem.write(output, &record) { return Err(BAD_POINTER); }
        Ok(0)
    }

    fn open_fios_file(&mut self) -> Result<u64, u32> {
        let (attribute, output, path, options) = (self.arg(0), self.arg(1), self.arg(2), self.arg(3));
        let mut guard = [0; 4];
        if output == 0 || !self.mem.read(output, &mut guard) { return Err(BAD_POINTER); }
        if attribute != 0 { return Err(kernel_error(45)); }
        let mut parameters = [0; 16];
        parameters[0] = 1;
        if options != 0 && !self.mem.read(options, &mut parameters) { return Err(BAD_POINTER); }
        if u16::from_le_bytes(parameters[..2].try_into().unwrap()) > 1 { return Err(kernel_error(30)); }
        let text = self.fios_path(path)?;
        if self.io.fios.paths.len() < 32 { self.io.fios.paths.push(text.clone()); }
        if self.io.fios.recent_paths.len() == 16 { self.io.fios.recent_paths.pop_front(); }
        self.io.fios.recent_paths.push_back(text.clone());
        let file = self.open_read_only_file(&text).map_err(kernel_error)?;
        if self.io.fios.files.len() >= 256 || self.io.fios.next_handle >= 0x4000 { return Err(kernel_error(24)); }
        let storage_size = 16 + text.len() as u64 + 1;
        let storage = self.heap_alloc(storage_size, 8);
        if storage == 0 { return Err(kernel_error(12)); }
        self.mem.write(storage, &parameters);
        self.mem.write(storage + 16, text.as_bytes());
        self.mem.write(storage + storage_size - 1, &[0]);
        let handle = 0x7FFF_8000 | self.io.fios.next_handle;
        if !self.mem.write(output, &handle.to_le_bytes()) {
            self.sizes.remove(&storage);
            return Err(BAD_POINTER);
        }
        self.io.fios.next_handle += 1;
        self.io.fios.handle_storage.insert(handle, (storage, storage_size));
        self.io.fios.files.insert(handle, file);
        self.io.fios.opened += 1;
        Ok(0)
    }

    fn read_fios_file(&mut self) -> Result<u64, u32> {
        let (attribute, handle, output, count) = (self.arg(0), self.arg(1) as u32, self.arg(2), self.arg(3));
        if attribute != 0 { return Err(kernel_error(45)); }
        if count > RAM_SIZE as u64 { return Err(BAD_SIZE); }
        if count != 0 {
            let end = output.checked_add(count - 1).ok_or(BAD_POINTER)?;
            if self.mem.phys(output).is_none() || self.mem.phys(end).is_none()
                || ((output >> 12)..=(end >> 12)).any(|page| self.mem.phys(page << 12).is_none()) { return Err(BAD_POINTER); }
        }
        let file = self.io.fios.files.get_mut(&handle).ok_or(kernel_error(9))?;
        let mut data = vec![0; (count as usize).min(TRANSFER_LIMIT)];
        let mut total = 0;
        while total < count {
            let wanted = (count - total).min(data.len() as u64) as usize;
            let read = match file.read(&mut data[..wanted]) {
                Ok(read) => read,
                Err(_) if total > 0 => return Ok(total),
                Err(_) => return Err(kernel_error(5)),
            };
            if !self.mem.write(output + total, &data[..read]) { return Err(BAD_POINTER); }
            total += read as u64;
            self.io.fios.bytes_read += read as u64;
            if read == 0 { break; }
        }
        Ok(total)
    }

    fn positioned_read_fios_file(&mut self) -> Result<u64, u32> {
        let handle = self.arg(1) as u32;
        let offset = self.arg(4);
        if offset > i64::MAX as u64 { return Err(kernel_error(22)); }
        let file = self.io.fios.files.get_mut(&handle).ok_or(kernel_error(9))?;
        let original = file.stream_position().map_err(|_| kernel_error(5))?;
        file.seek(SeekFrom::Start(offset)).map_err(|_| kernel_error(22))?;
        let result = self.read_fios_file();
        let file = self.io.fios.files.get_mut(&handle).ok_or(kernel_error(9))?;
        file.seek(SeekFrom::Start(original)).map_err(|_| kernel_error(5))?;
        result
    }

    fn seek_fios_file(&mut self) -> Result<u64, u32> {
        let (handle, offset, whence) = (self.arg(0) as u32, self.arg(1) as i64, self.arg(2));
        let position = match whence {
            0 if offset >= 0 => SeekFrom::Start(offset as u64),
            1 => SeekFrom::Current(offset),
            2 => SeekFrom::End(offset),
            _ => return Err(kernel_error(22)),
        };
        self.io.fios.files.get_mut(&handle).ok_or(kernel_error(9))?.seek(position).map_err(|_| kernel_error(22))
    }

    fn close_fios_file(&mut self) -> Result<u64, u32> {
        if self.arg(0) != 0 { return Err(kernel_error(45)); }
        let handle = self.arg(1) as u32;
        if self.io.fios.files.remove(&handle).is_none() { return Err(kernel_error(9)); }
        if let Some((address, length)) = self.io.fios.handle_storage.remove(&handle) {
            self.mem.write(address, &vec![0; length as usize]);
            self.sizes.remove(&address);
        }
        Ok(0)
    }

    fn fios_handle_pointer(&self, path: bool) -> u64 {
        let handle = self.arg(0) as u32;
        if !self.io.fios.files.contains_key(&handle) { return 0; }
        self.io.fios.handle_storage.get(&handle).map_or(0, |(address, _)| address + if path { 16 } else { 0 })
    }

    fn terminate_fios(&mut self) -> Result<u64, u32> {
        let previous = std::mem::take(&mut self.io.fios);
        for (_, (address, length)) in previous.handle_storage {
            self.mem.write(address, &vec![0; length as usize]);
            self.sizes.remove(&address);
        }
        Ok(0)
    }

    pub fn file_service_report(&self) -> String {
        format!("fios_initialized={} opened={} bytes_read={} errors={} handles={} paths={:?} recent_paths={:?} metadata_queries={:?} directory_queries={:?} native_directory_activity={:?}", self.io.fios.parameters.is_some(), self.io.fios.opened, self.io.fios.bytes_read, self.io.fios.errors, self.io.fios.files.len(), self.io.fios.paths, self.io.fios.recent_paths, self.io.fios.metadata_queries, self.io.fios.directory_queries, self.io.directory_activity)
    }

    pub(super) fn call_fios(&mut self, name: &str) -> bool {
        let result = match name {
            "sceFiosStatSync" => self.stat_fios_file(),
            "sceFiosDirectoryExistsSync" => Ok(self.fios_directory_exists()),
            "sceFiosFileExistsSync" => Ok(self.fios_file_size().is_ok() as u64),
            "sceFiosFileGetSizeSync" => self.fios_file_size(),
            "sceFiosDirectoryCreateSync" => Err(kernel_error(30)),
            "sceFiosIsValidHandle" => Ok(self.io.fios.files.contains_key(&(self.arg(0) as u32)) as u64),
            "sceFiosInitialize" => self.initialize_fios(),
            "sceFiosFHOpenWithModeSync" | "sceFiosFHOpenSync" => self.open_fios_file(),
            "sceFiosFHReadSync" => self.read_fios_file(),
            "sceFiosFHPreadSync" => self.positioned_read_fios_file(),
            "sceFiosFHSeek" => self.seek_fios_file(),
            "sceFiosFHGetSize" => self.io.fios.files.get(&(self.arg(0) as u32)).ok_or(kernel_error(9)).and_then(|file| file.len().map_err(|_| kernel_error(5))),
            "sceFiosFHTell" => self.io.fios.files.get_mut(&(self.arg(0) as u32)).ok_or(kernel_error(9)).and_then(|file| file.stream_position().map_err(|_| kernel_error(5))),
            "sceFiosFHGetOpenParams" => Ok(self.fios_handle_pointer(false)),
            "sceFiosFHGetPath" => Ok(self.fios_handle_pointer(true)),
            "sceFiosFHCloseSync" => self.close_fios_file(),
            "sceFiosTerminate" => self.terminate_fios(),
            _ => return false,
        };
        if matches!(name, "sceFiosStatSync" | "sceFiosFileExistsSync" | "sceFiosFileGetSizeSync") {
            let path = self.fios_path(self.arg(1)).unwrap_or_else(|_| "<invalid>".into());
            let query = format!("{name} attribute={:#x} path={path:?} result={result:x?}", self.arg(0));
            if self.io.fios.metadata_queries.len() == 16 { self.io.fios.metadata_queries.pop_front(); }
            self.io.fios.metadata_queries.push_back(query);
        }
        if name == "sceFiosDirectoryExistsSync" {
            let path = self.fios_path(self.arg(1)).unwrap_or_else(|_| "<invalid>".into());
            let query = format!("attribute={:#x} path={path:?} result={result:x?}", self.arg(0));
            if self.io.fios.directory_queries.len() == 16 { self.io.fios.directory_queries.pop_front(); }
            self.io.fios.directory_queries.push_back(query);
        }
        let value = match result {
            Ok(value) => value,
            Err(error) => { self.io.fios.errors += 1; error as i32 as i64 as u64 }
        };
        self.ret(value);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_reports_real_read_only_metadata_with_native_layout_and_guards() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("hitboy-fios-stat-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("asset.bin");
        std::fs::write(&path, b"content").unwrap();
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1700000000);
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_accessed(time).set_modified(time)).unwrap();
        drop(file);
        machine.set_game_root(&root).unwrap();
        let storage = machine.heap_alloc(256, 16);
        machine.mem.write(storage, b"/app0/asset.bin\0");
        let output = storage + 136;
        machine.mem.write(output - 8, &[0xCC; 96]);
        machine.cpu.r[7] = 0;
        machine.cpu.r[6] = storage;
        machine.cpu.r[2] = output;
        assert_eq!(machine.stat_fios_file(), Ok(0));
        assert_eq!(machine.mem.read_u64(output), 7);
        assert_eq!(machine.mem.read_u64(output + 8), 1700000000000000000);
        assert_eq!(machine.mem.read_u64(output + 16), 1700000000000000000);
        assert_eq!(machine.mem.read_u64(output + 32), 2);
        assert_eq!(machine.mem.read_u64(output + 72), 0x8124);
        assert_eq!(machine.mem.read_u64(output - 8), 0xCCCCCCCCCCCCCCCC);
        assert_eq!(machine.mem.read_u64(output + 80), 0xCCCCCCCCCCCCCCCC);
        machine.mem.write(storage, b"/app0\0");
        assert_eq!(machine.stat_fios_file(), Ok(0));
        assert_eq!(machine.mem.read_u64(output), 0);
        assert_eq!(machine.mem.read_u64(output + 32), 3);
        assert_eq!(machine.mem.read_u64(output + 72), 0x416D);
        machine.mem.write(output, &[0xCC; 80]);
        machine.mem.write(storage, b"/app0/../outside\0");
        assert_eq!(machine.stat_fios_file(), Err(kernel_error(13)));
        assert_eq!(machine.mem.read_u64(output), 0xCCCCCCCCCCCCCCCC);
        machine.mem.write(storage, b"/app0/missing\0");
        assert_eq!(machine.stat_fios_file(), Err(kernel_error(2)));
        machine.cpu.r[7] = 1;
        assert_eq!(machine.stat_fios_file(), Err(kernel_error(45)));
        machine.cpu.r[2] = 0;
        assert_eq!(machine.stat_fios_file(), Err(BAD_POINTER));
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn open_parameters_preserve_non_flag_bytes_in_stable_guest_storage() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("hitboy-fios-params-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("asset.bin");
        std::fs::write(&path, b"content").unwrap();
        machine.set_game_root(&root).unwrap();
        let storage = machine.heap_alloc(256, 16);
        machine.mem.write(storage, b"/app0/asset.bin\0");
        let mut parameters = [0xA5; 16];
        parameters[..2].copy_from_slice(&1u16.to_le_bytes());
        machine.mem.write(storage + 64, &parameters);
        machine.cpu.r[7] = 0;
        machine.cpu.r[6] = storage + 96;
        machine.cpu.r[2] = storage;
        machine.cpu.r[1] = storage + 64;
        assert_eq!(machine.open_fios_file(), Ok(0));
        let handle = machine.mem.read_u64(storage + 96) as u32;
        machine.cpu.r[7] = handle as u64;
        let address = machine.fios_handle_pointer(false);
        let mut stored = [0; 16];
        assert!(machine.mem.read(address, &mut stored));
        assert_eq!(stored, parameters);
        machine.terminate_fios().unwrap();
        assert_eq!(machine.fios_handle_pointer(false), 0);
        assert_eq!(machine.mem.read_u64(address), 0);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn initialization_validates_the_observed_parameter_layout() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let parameters = machine.heap_alloc(176, 16);
        machine.mem.write(parameters, &[0; 176]);
        machine.cpu.r[7] = parameters;
        assert_eq!(machine.initialize_fios(), Err(BAD_SIZE));
        machine.mem.write(parameters, &0x160u32.to_le_bytes());
        assert_eq!(machine.initialize_fios(), Ok(0));
        machine.mem.write(parameters + 25, &[4]);
        assert_eq!(machine.initialize_fios(), Err(0x80820021));
        assert_eq!(machine.io.fios.parameters.unwrap()[25], 0);
        machine.cpu.r[7] = u64::MAX;
        assert_eq!(machine.initialize_fios(), Err(BAD_POINTER));
        assert_eq!(machine.io.fios.parameters.unwrap()[25], 0);
    }

    #[test]
    fn synchronous_reads_complete_requests_larger_than_the_host_transfer_buffer() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("hitboy-fios-read-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let data = (0..TRANSFER_LIMIT + 17).map(|index| (index % 251) as u8).collect::<Vec<_>>();
        let path = root.join("asset.bin");
        std::fs::write(&path, &data).unwrap();
        machine.io.fios.files.insert(7, std::fs::File::open(&path).unwrap().into());
        let output = machine.heap_alloc(data.len() as u64 + 2, 16);
        machine.mem.write(output, &[0xCC]);
        machine.mem.write(output + data.len() as u64 + 1, &[0xDD]);
        machine.cpu.r[7] = 0;
        machine.cpu.r[6] = 7;
        machine.cpu.r[2] = output + 1;
        machine.cpu.r[1] = data.len() as u64;
        assert_eq!(machine.read_fios_file(), Ok(data.len() as u64));
        let mut actual = vec![0; data.len() + 2];
        assert!(machine.mem.read(output, &mut actual));
        assert_eq!(actual[0], 0xCC);
        assert_eq!(&actual[1..data.len() + 1], data);
        assert_eq!(actual[data.len() + 1], 0xDD);
        machine.io.fios.files.clear();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn synchronous_files_preserve_handles_guards_offsets_and_path_boundaries() {
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("hitboy-fios-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("asset.bin");
        let mut source = std::fs::File::create(&path).unwrap();
        use std::io::Write;
        source.write_all(b"abcdef").unwrap();
        drop(source);
        machine.set_game_root(&root).unwrap();
        let storage = machine.heap_alloc(4096, 16);
        machine.mem.write(storage, &[0xCC; 32]);
        machine.mem.write(storage + 64, b"/app0/asset.bin\0");
        machine.cpu.r[7] = 0;
        machine.cpu.r[6] = storage + 8;
        machine.cpu.r[2] = storage + 64;
        machine.cpu.r[1] = 0;
        assert!(machine.call_fios("sceFiosFHOpenSync"));
        assert_eq!(machine.cpu.r[0], 0);
        let handle = machine.mem.read_u64(storage + 8) as u32;
        machine.cpu.r[7] = handle as u64;
        machine.call_fios("sceFiosIsValidHandle");
        assert_eq!(machine.cpu.r[0], 1);
        machine.call_fios("sceFiosFHGetOpenParams");
        let parameters = machine.cpu.r[0];
        assert_ne!(parameters, 0);
        assert_eq!(machine.mem.read_u64(parameters), 1);
        machine.call_fios("sceFiosFHGetOpenParams");
        assert_eq!(machine.cpu.r[0], parameters);
        machine.call_fios("sceFiosFHGetPath");
        assert_eq!(machine.mem.read_cstr(machine.cpu.r[0]), "/app0/asset.bin");
        machine.cpu.r[7] = 0;
        assert_eq!(machine.mem.read_u64(storage), 0xCCCCCCCCCCCCCCCC);
        assert_eq!(machine.mem.read_u64(storage + 8) >> 32, 0xCCCCCCCC);
        machine.cpu.r[6] = handle as u64;
        machine.cpu.r[2] = storage + 128;
        machine.cpu.r[1] = 3;
        assert_eq!(machine.read_fios_file(), Ok(3));
        let mut bytes = [0; 3];
        machine.mem.read(storage + 128, &mut bytes);
        assert_eq!(&bytes, b"abc");
        machine.cpu.r[2] = u64::MAX;
        assert_eq!(machine.read_fios_file(), Err(BAD_POINTER));
        machine.cpu.r[2] = storage + 160;
        machine.cpu.r[1] = 2;
        machine.cpu.r[8] = 1;
        assert_eq!(machine.positioned_read_fios_file(), Ok(2));
        machine.mem.read(storage + 160, &mut bytes[..2]);
        assert_eq!(&bytes[..2], b"bc");
        machine.cpu.r[8] = u64::MAX;
        assert_eq!(machine.positioned_read_fios_file(), Err(kernel_error(22)));
        machine.cpu.r[8] = 0x1_0000_0000;
        assert_eq!(machine.positioned_read_fios_file(), Ok(0));
        machine.cpu.r[7] = handle as u64;
        machine.call_fios("sceFiosFHTell");
        assert_eq!(machine.cpu.r[0], 3);
        machine.call_fios("sceFiosFHGetSize");
        assert_eq!(machine.cpu.r[0], 6);
        machine.cpu.r[6] = 0x1_0000_0000;
        machine.cpu.r[2] = 0;
        assert_eq!(machine.seek_fios_file(), Ok(0x1_0000_0000));
        machine.cpu.r[7] = 0;
        machine.cpu.r[6] = handle as u64;
        machine.cpu.r[2] = storage + 128;
        machine.cpu.r[1] = 4;
        assert_eq!(machine.read_fios_file(), Ok(0));
        machine.mem.read(storage + 128, &mut bytes);
        assert_eq!(bytes[0], b'a');
        machine.call_fios("sceFiosFHCloseSync");
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.read_fios_file(), Err(kernel_error(9)));
        machine.mem.write(storage + 64, b"/app0/../outside\0");
        machine.cpu.r[6] = storage + 8;
        machine.cpu.r[2] = storage + 64;
        machine.cpu.r[1] = 0;
        assert_eq!(machine.open_fios_file(), Err(kernel_error(13)));
        machine.mem.write(storage + 64, b"/app0/asset.bin\0");
        machine.mem.write(storage + 96, &2u16.to_le_bytes());
        machine.cpu.r[1] = storage + 96;
        assert_eq!(machine.open_fios_file(), Err(kernel_error(30)));
        machine.cpu.r[7] = handle as u64;
        machine.call_fios("sceFiosIsValidHandle");
        assert_eq!(machine.cpu.r[0], 0);
        machine.call_fios("sceFiosFHGetOpenParams");
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.mem.read_u64(parameters), 0);
        assert_eq!(machine.io.fios.bytes_read, 5);
        assert!(machine.io.fios.files.is_empty());
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
