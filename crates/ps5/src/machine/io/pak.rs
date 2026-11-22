use super::*;
use std::io::{self, Cursor};

#[derive(Default)]
pub(super) struct Archives {
    root: Option<PathBuf>,
    paths: Vec<PathBuf>,
    entries: HashMap<String, Entry>,
}

struct Entry {
    name: String,
    archive: usize,
    offset: u64,
    size: u64,
    digest: [u8; 20],
}

pub(super) enum ReadFile {
    Host(std::fs::File),
    Archive { file: std::fs::File, start: u64, size: u64, position: u64 },
}

impl From<std::fs::File> for ReadFile {
    fn from(file: std::fs::File) -> Self { Self::Host(file) }
}

impl ReadFile {
    pub(super) fn len(&self) -> io::Result<u64> {
        match self { Self::Host(file) => file.metadata().map(|info| info.len()), Self::Archive { size, .. } => Ok(*size) }
    }
}

impl Read for ReadFile {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Host(file) => file.read(output),
            Self::Archive { file, start, size, position } => {
                let count = output.len().min(size.saturating_sub(*position) as usize);
                if count == 0 { return Ok(0); }
                file.seek(SeekFrom::Start(start.checked_add(*position).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Archive offset overflow"))?))?;
                let count = file.read(&mut output[..count])?;
                *position += count as u64;
                Ok(count)
            }
        }
    }
}

impl Seek for ReadFile {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Host(file) => file.seek(from),
            Self::Archive { size, position, .. } => {
                let value = match from {
                    SeekFrom::Start(value) => value as i128,
                    SeekFrom::Current(value) => *position as i128 + value as i128,
                    SeekFrom::End(value) => *size as i128 + value as i128,
                };
                let value = u64::try_from(value).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid archive seek"))?;
                *position = value;
                Ok(value)
            }
        }
    }
}

fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }

fn number<const N: usize>(input: &mut impl Read) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn string(input: &mut Cursor<Vec<u8>>) -> io::Result<String> {
    let length = i32::from_le_bytes(number(input)?);
    if length == 0 || length.unsigned_abs() > 32768 { return Err(invalid("Invalid archive name length")); }
    let wide = length < 0;
    let mut bytes = vec![0; length.unsigned_abs() as usize * if wide { 2 } else { 1 }];
    input.read_exact(&mut bytes)?;
    if wide {
        if bytes[bytes.len() - 2..] != [0, 0] { return Err(invalid("Unterminated archive name")); }
        let units = bytes[..bytes.len() - 2].chunks_exact(2).map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]])).collect::<Vec<_>>();
        String::from_utf16(&units).map_err(|_| invalid("Invalid archive name encoding"))
    } else {
        if bytes.pop() != Some(0) { return Err(invalid("Unterminated archive name")); }
        String::from_utf8(bytes).map_err(|_| invalid("Invalid archive name encoding"))
    }
}

fn archive_key(value: &str) -> io::Result<String> {
    if value.is_empty() || value.contains('\\') || value.contains(':') || value.contains('\0')
        || Path::new(value).components().any(|part| !matches!(part, Component::Normal(_))) { return Err(invalid("Archive filename escapes its mount")); }
    Ok(value.to_ascii_lowercase())
}

impl Archives {
    pub(super) fn count(&self) -> usize { self.entries.len() }

    pub(super) fn file_size(&self, relative: &Path) -> Option<u64> {
        let key = archive_key(relative.to_str()?).ok()?;
        self.entries.get(&key).map(|entry| entry.size)
    }

    pub(super) fn directory_entries(&self, relative: &Path) -> io::Result<Vec<(String, bool)>> {
        let text = relative.to_str().ok_or_else(|| invalid("Invalid guest directory encoding"))?.trim_end_matches('/');
        let prefix = if text.is_empty() || text == "." { String::new() } else { archive_key(text)? + "/" };
        let mut children = std::collections::BTreeMap::new();
        for (key, entry) in &self.entries {
            let Some(remainder) = key.strip_prefix(&prefix) else { continue; };
            if remainder.is_empty() { continue; }
            let child = remainder.split('/').next().unwrap();
            let original = &entry.name[prefix.len()..prefix.len() + child.len()];
            let directory = remainder.contains('/');
            children.entry(child.to_string()).or_insert_with(|| (original.to_string(), directory));
            if children.len() > 20000 { return Err(invalid("Guest directory has too many entries")); }
        }
        Ok(children.into_values().collect())
    }

    pub(super) fn directory_exists(&self, relative: &Path) -> bool {
        let Some(text) = relative.to_str() else { return false; };
        let Ok(key) = archive_key(text.trim_end_matches('/')) else { return false; };
        let prefix = key + "/";
        self.entries.keys().any(|name| name.starts_with(&prefix))
    }

    fn add(&mut self, path: PathBuf, index_budget: &mut u64) -> io::Result<()> {
        let mut file = std::fs::File::open(&path)?;
        let size = file.metadata()?.len();
        if size < 44 { return Err(invalid("Archive footer is truncated")); }
        file.seek(SeekFrom::End(-44))?;
        let footer = number::<44>(&mut file)?;
        if u32::from_le_bytes(footer[..4].try_into().unwrap()) != 0x5A6F12E1 || u32::from_le_bytes(footer[4..8].try_into().unwrap()) != 3 {
            return Err(invalid("Only unencrypted version 3 archives are supported"));
        }
        let offset = u64::from_le_bytes(footer[8..16].try_into().unwrap());
        let length = u64::from_le_bytes(footer[16..24].try_into().unwrap());
        if length == 0 || length > 128 << 20 || length > *index_budget || offset.checked_add(length).is_none_or(|end| end > size - 44) {
            return Err(invalid("Archive index exceeds its bounds"));
        }
        *index_budget -= length;
        let mut bytes = vec![0; length as usize];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
        if crate::sha1::sha1(&bytes) != footer[24..44] { return Err(invalid("Archive index checksum does not match")); }
        let mut input = Cursor::new(bytes);
        let mount = string(&mut input)?;
        let mount = mount.strip_prefix("../../../").ok_or_else(|| invalid("Unsupported archive mount"))?;
        if !mount.is_empty() { archive_key(mount.trim_end_matches('/'))?; }
        let count = u32::from_le_bytes(number(&mut input)?) as usize;
        if count > input.get_ref().len() / 58 || self.entries.len().saturating_add(count) > 500000 { return Err(invalid("Archive entry count exceeds its bounds")); }
        let archive = self.paths.len();
        for _ in 0..count {
            let name = string(&mut input)?;
            archive_key(&name)?;
            let original_name = format!("{mount}{name}");
            let key = archive_key(&original_name)?;
            if self.entries.get(&key).is_some_and(|entry| entry.archive == archive) { return Err(invalid("Duplicate archive filename")); }
            let entry_offset = u64::from_le_bytes(number(&mut input)?);
            let stored = u64::from_le_bytes(number(&mut input)?);
            let original = u64::from_le_bytes(number(&mut input)?);
            let method = u32::from_le_bytes(number(&mut input)?);
            let digest = number::<20>(&mut input)?;
            if method != 0 { return Err(invalid("Compressed archive entries are not supported")); }
            let flags = number::<1>(&mut input)?[0];
            let _block_size = number::<4>(&mut input)?;
            if flags != 0 || stored != original || original > RAM_SIZE as u64
                || entry_offset.checked_add(53).and_then(|start| start.checked_add(original)).is_none_or(|end| end > offset) {
                return Err(invalid("Unsupported or invalid archive entry"));
            }
            self.entries.insert(key, Entry { name: original_name, archive, offset: entry_offset, size: original, digest });
        }
        if input.position() != length { return Err(invalid("Unexpected archive index data")); }
        self.paths.push(path);
        Ok(())
    }

    pub(super) fn mount(root: &Path, directory: &Path) -> io::Result<Self> {
        let directory = directory.canonicalize()?;
        if !directory.starts_with(root) { return Err(invalid("Archive directory must stay inside the game folder")); }
        let mut paths = std::fs::read_dir(directory)?.map(|item| item.map(|item| item.path())).collect::<io::Result<Vec<_>>>()?;
        paths.retain(|path| path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("pak")));
        if paths.len() > 32 { return Err(invalid("Too many mounted archives")); }
        paths.sort_by_key(|path| (path.file_name().is_some_and(|name| name.to_string_lossy().to_ascii_lowercase().ends_with("_p.pak")), path.clone()));
        let mut mounted = Self { root: Some(root.to_path_buf()), ..Self::default() };
        let mut index_budget = 256 << 20;
        for path in paths {
            let path = path.canonicalize()?;
            if !path.starts_with(root) { return Err(invalid("An archive escapes the game folder")); }
            mounted.add(path, &mut index_budget)?;
        }
        Ok(mounted)
    }

    pub(super) fn open(&self, relative: &Path) -> io::Result<ReadFile> {
        let key = archive_key(relative.to_str().ok_or_else(|| invalid("Invalid guest path encoding"))?)?;
        let entry = self.entries.get(&key).ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let path = self.paths[entry.archive].canonicalize()?;
        if !self.root.as_ref().is_some_and(|root| path.starts_with(root)) { return Err(invalid("An archive escapes the game folder")); }
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(entry.offset))?;
        let header = number::<53>(&mut file)?;
        if u64::from_le_bytes(header[8..16].try_into().unwrap()) != entry.size
            || u64::from_le_bytes(header[16..24].try_into().unwrap()) != entry.size
            || header[24..28] != [0; 4] || header[28..48] != entry.digest || header[48] != 0 {
            return Err(invalid("Archive data header does not match its index"));
        }
        let mut hasher = crate::sha1::Sha1::new();
        let mut remaining = entry.size;
        let mut buffer = [0; 65536];
        while remaining > 0 {
            let count = remaining.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..count])?;
            hasher.update(&buffer[..count]);
            remaining -= count as u64;
        }
        if hasher.finish() != entry.digest { return Err(invalid("Archive file checksum does not match")); }
        Ok(ReadFile::Archive { file, start: entry.offset + 53, size: entry.size, position: 0 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str, payload: &[u8]) -> Vec<u8> {
        let mut header = vec![0; 53];
        header[8..16].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        header[16..24].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        header[28..48].copy_from_slice(&crate::sha1::sha1(payload));
        let mut data = header.clone();
        data.extend_from_slice(payload);
        let offset = data.len() as u64;
        let mut index = Vec::new();
        let text = |output: &mut Vec<u8>, text: &str| {
            output.extend_from_slice(&((text.len() + 1) as i32).to_le_bytes());
            output.extend_from_slice(text.as_bytes());
            output.push(0);
        };
        text(&mut index, "../../../");
        index.extend_from_slice(&1u32.to_le_bytes());
        text(&mut index, name);
        index.extend_from_slice(&header);
        data.extend_from_slice(&index);
        data.extend_from_slice(&0x5A6F12E1u32.to_le_bytes());
        data.extend_from_slice(&3u32.to_le_bytes());
        data.extend_from_slice(&offset.to_le_bytes());
        data.extend_from_slice(&(index.len() as u64).to_le_bytes());
        data.extend_from_slice(&crate::sha1::sha1(&index));
        data
    }

    #[test]
    fn archived_files_keep_entry_bounds_patch_precedence_and_authentic_payloads() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("hitboy-pak-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let base = root.join("base.pak");
        let patch = root.join("patch_p.pak");
        std::fs::write(&base, fixture("FortniteGame/Game.uproject", b"original")).unwrap();
        std::fs::write(&patch, fixture("FortniteGame/Game.uproject", b"patched")).unwrap();
        let mut machine = Machine::load(&crate::machine::tests::hello_program()).unwrap();
        machine.set_game_root(&root).unwrap();
        assert_eq!(machine.mount_read_only_paks(&root).unwrap(), 1);
        let mut file = machine.open_read_only_file("/app0/fortnitegame/game.uproject").unwrap();
        assert_eq!(file.len().unwrap(), 7);
        let mut bytes = [0xCC; 16];
        assert_eq!(file.read(&mut bytes).unwrap(), 7);
        assert_eq!(&bytes[..7], b"patched");
        assert_eq!(bytes[7], 0xCC);
        assert_eq!(file.read(&mut bytes).unwrap(), 0);
        assert_eq!(file.seek(SeekFrom::End(-2)).unwrap(), 5);
        assert_eq!(file.read(&mut bytes).unwrap(), 2);
        assert_eq!(&bytes[..2], b"ed");
        assert!(file.seek(SeekFrom::Current(-100)).is_err());
        assert_eq!(file.stream_position().unwrap(), 7);
        assert!(machine.open_read_only_file("/app0/../outside").is_err());
        let storage = machine.heap_alloc(512, 16);
        machine.mem.write(storage, b"/app0/fortnitegame/game.uproject\0");
        machine.cpu.r[7] = 0;
        machine.cpu.r[6] = storage + 128;
        machine.cpu.r[2] = storage;
        machine.cpu.r[1] = 0;
        machine.call_io("sceFiosFHOpenSync");
        assert_eq!(machine.cpu.r[0], 0);
        let handle = machine.mem.read_u64(storage + 128) as u32;
        machine.cpu.r[6] = handle as u64;
        machine.cpu.r[2] = storage + 256;
        machine.cpu.r[1] = 100;
        machine.call_io("sceFiosFHReadSync");
        assert_eq!(machine.cpu.r[0], 7);
        let mut original = [0; 7];
        machine.mem.read(storage + 256, &mut original);
        assert_eq!(&original, b"patched");
        machine.cpu.r[7] = 0;
        machine.cpu.r[6] = storage;
        machine.call_io("sceFiosFileExistsSync");
        assert_eq!(machine.cpu.r[0], 1);
        machine.call_io("sceFiosFileGetSizeSync");
        assert_eq!(machine.cpu.r[0], 7);
        machine.mem.write(storage + 384, b"/app0/fortnitegame/missing.uproject\0");
        machine.cpu.r[6] = storage + 384;
        machine.call_io("sceFiosFileExistsSync");
        assert_eq!(machine.cpu.r[0], 0);
        machine.call_io("sceFiosFileGetSizeSync");
        assert!((machine.cpu.r[0] as i64) < 0);
        machine.call_io("sceFiosDirectoryCreateSync");
        assert!((machine.cpu.r[0] as i32) < 0);
        machine.cpu.r[7] = 0;
        machine.cpu.r[6] = storage + 384;
        for (path, expected) in [("/app0/fortnitegame", 1), ("/app0/fortnitegame/missing", 0), ("/app0/../outside", 0), ("/app0/", 1), ("/app0", 1)] {
            machine.mem.write(storage + 384, format!("{path}\0").as_bytes());
            machine.call_io("sceFiosDirectoryExistsSync");
            assert_eq!(machine.cpu.r[0], expected);
        }
        machine.mem.write(storage, b"/app0/fortnitegame/game.uproject\0");
        machine.mem.write(storage + 248, &[0xCC; 96]);
        machine.cpu.r[7] = 0;
        machine.cpu.r[6] = storage;
        machine.cpu.r[2] = storage + 256;
        machine.call_io("sceFiosStatSync");
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.mem.read_u64(storage + 256), 7);
        assert_eq!(machine.mem.read_u64(storage + 288), 2);
        assert_eq!(machine.mem.read_u64(storage + 328), 0x8124);
        assert_eq!(machine.mem.read_u64(storage + 248), 0xCCCCCCCCCCCCCCCC);
        assert_eq!(machine.mem.read_u64(storage + 336), 0xCCCCCCCCCCCCCCCC);
        machine.mem.write(storage, b"/app0/fortnitegame\0");
        machine.call_io("sceFiosStatSync");
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.mem.read_u64(storage + 288), 3);
        let directory = machine.heap_alloc(512, 16);
        machine.mem.write(directory, b"/app0/FortniteGame\0");
        machine.cpu.r[7] = directory;
        machine.cpu.r[6] = 0x20000;
        machine.call_io("sceKernelOpen");
        let descriptor = machine.cpu.r[0];
        assert!((3..1024).contains(&descriptor));
        machine.cpu.r[7] = descriptor;
        machine.cpu.r[6] = directory + 128;
        machine.cpu.r[2] = 256;
        machine.call_io("sceKernelGetdents");
        let count = machine.cpu.r[0] as usize;
        assert!(count > 24 && count <= 256);
        let mut listing = vec![0; count];
        machine.mem.read(directory + 128, &mut listing);
        assert!(listing.windows(b"Game.uproject".len()).any(|window| window == b"Game.uproject"));
        machine.call_io("sceKernelClose");
        machine.mem.write(directory, b"/app0/FortniteGame/Game.uproject\0");
        machine.cpu.r[7] = directory;
        machine.cpu.r[6] = 0;
        machine.call_io("sceKernelOpen");
        let descriptor = machine.cpu.r[0];
        assert!((3..1024).contains(&descriptor));
        machine.cpu.r[7] = descriptor;
        machine.cpu.r[6] = directory + 128;
        machine.cpu.r[2] = 16;
        machine.call_io("sceKernelRead");
        assert_eq!(machine.cpu.r[0], 7);
        let mut payload = [0; 7];
        machine.mem.read(directory + 128, &mut payload);
        assert_eq!(&payload, b"patched");
        machine.call_io("sceKernelClose");
        drop(file);
        drop(machine);
        let mut corrupt = fixture("FortniteGame/Game.uproject", b"patched");
        corrupt[53] ^= 1;
        std::fs::write(&patch, corrupt).unwrap();
        let mounted = Archives::mount(&root, &root).unwrap();
        assert!(mounted.open(Path::new("FortniteGame/Game.uproject")).is_err());
        let mut corrupt = fixture("FortniteGame/Game.uproject", b"patched");
        let last = corrupt.len() - 1;
        corrupt[last] ^= 1;
        std::fs::write(&patch, corrupt).unwrap();
        assert!(Archives::mount(&root, &root).is_err());
        std::fs::write(&patch, fixture("../outside", b"x")).unwrap();
        assert!(Archives::mount(&root, &root).is_err());
        std::fs::remove_file(base).unwrap();
        std::fs::remove_file(patch).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
