use super::*;

mod fios;
mod pak;
mod directories;
mod metadata;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

const TRANSFER_LIMIT: usize = 1 << 20;
const OUTPUT_LIMIT: usize = 1 << 20;

enum GuestFile {
    File(pak::ReadFile),
    Directory(directories::GuestDirectory),
    Console,
    Null,
}

#[derive(Default)]
pub(super) struct Io {
    root: Option<PathBuf>,
    files: HashMap<i32, GuestFile>,
    fios: fios::Fios,
    archives: pak::Archives,
    directory_activity: std::collections::VecDeque<String>,
}

impl Machine {
    pub fn set_game_root(&mut self, root: &Path) -> Result<(), String> {
        let root = if root.as_os_str().is_empty() { Path::new(".") } else { root };
        let root = root.canonicalize().map_err(|error| format!("Could not open the game folder: {error}"))?;
        if !root.is_dir() {
            return Err("The game folder is not a directory.".into());
        }
        if self.io.root.as_ref() != Some(&root) { self.io.archives = pak::Archives::default(); }
        self.io.root = Some(root);
        Ok(())
    }

    pub fn load_game(path: &Path) -> Result<Self, String> {
        let file = if path.is_dir() { path.join("eboot.bin") } else { path.to_path_buf() };
        let root = file.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        let image = std::fs::read(&file).map_err(|error| format!("Could not read the executable: {error}"))?;
        Self::load_game_image(&image, root)
    }

    pub fn load_game_image(image: &[u8], root: &Path) -> Result<Self, String> {
        let root = root.canonicalize().map_err(|error| format!("Could not open the game folder: {error}"))?;
        let mut modules = Vec::new();
        let runtime = root.join("sce_module/libc.prx");
        if runtime.is_file() {
            let runtime = runtime.canonicalize().map_err(|error| error.to_string())?;
            if !runtime.starts_with(&root) { return Err("The runtime module must be inside the game folder.".into()); }
            let mut file = std::fs::File::open(runtime).map_err(|error| error.to_string())?.take((64 << 20) + 1);
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).map_err(|error| error.to_string())?;
            if bytes.len() > 64 << 20 { return Err("The runtime module exceeds the supported size.".into()); }
            modules.push(bytes);
        }
        let mut machine = Self::load_with_modules(image, &modules)?;
        machine.set_game_root(&root)?;
        Ok(machine)
    }

    pub fn mount_read_only_paks(&mut self, directory: &Path) -> Result<usize, String> {
        let root = self.io.root.as_ref().ok_or("Select a game folder before mounting archives")?;
        let mounted = pak::Archives::mount(root, directory).map_err(|error| format!("Could not mount game archives: {error}"))?;
        let count = mounted.count();
        self.io.archives = mounted;
        Ok(count)
    }

    fn open_read_only_file(&self, path: &str) -> Result<pak::ReadFile, u32> {
        match self.guest_path(path) {
            Ok(path) => {
                let file = std::fs::File::open(path).map_err(|_| 2u32)?;
                if !file.metadata().map_err(|_| 5u32)?.is_file() { return Err(21); }
                Ok(file.into())
            }
            Err(2) => {
                let relative = if path == "/app0" { "" } else { path.strip_prefix("/app0/").unwrap_or(path) };
                self.io.archives.open(Path::new(relative)).map_err(|error| if error.kind() == std::io::ErrorKind::NotFound { 2 } else { 5 })
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn game_root(&self) -> Option<PathBuf> { self.io.root.clone() }

    pub fn reload(&mut self, image: &[u8]) -> Result<(), String> {
        let mut machine = match &self.reload_root() {
            Some(root) => Self::load_game_image(image, root)?,
            None => Self::load(image)?,
        };
        machine.retain_title_registry(self);
        *self = machine;
        Ok(())
    }

    fn guest_path(&self, path: &str) -> Result<PathBuf, u32> {
        if path.contains('\\') || path.contains(':') {
            return Err(13);
        }
        let relative = if path == "/app0" { "" } else { path.strip_prefix("/app0/").unwrap_or(path) };
        let relative = Path::new(relative);
        if relative.components().any(|component| !matches!(component, Component::Normal(_) | Component::CurDir)) {
            return Err(13);
        }
        let root = self.io.root.as_ref().ok_or(2u32)?;
        let resolved = root.join(relative).canonicalize().map_err(|_| 2u32)?;
        if !resolved.starts_with(root) {
            return Err(13);
        }
        Ok(resolved)
    }

    fn open_guest_file(&mut self) -> Result<u64, u32> {
        let path = self.mem.read_cstr(self.arg(0));
        let flags = self.arg(1);
        let file = match path.as_str() {
            "/dev/console" | "/dev/stdout" | "/dev/stderr" => GuestFile::Console,
            "/dev/null" => GuestFile::Null,
            _ => {
                if flags & (3 | 8 | 0x200 | 0x400) != 0 {
                    return Err(30);
                }
                if let Some(directory) = self.open_guest_directory(&path)? {
                    GuestFile::Directory(directory)
                } else {
                    let file = self.open_read_only_file(&path)?;
                    if flags & 0x20000 != 0 { return Err(20); }
                    GuestFile::File(file)
                }
            }
        };
        let descriptor = (3..1024).find(|descriptor| !self.io.files.contains_key(descriptor)).ok_or(24u32)?;
        self.io.files.insert(descriptor, file);
        Ok(descriptor as u64)
    }

    fn read_guest_file(&mut self, descriptor: i32, address: u64, count: u64) -> Result<u64, u32> {
        if descriptor == 0 {
            return Ok(0);
        }
        let count = count.min(TRANSFER_LIMIT as u64) as usize;
        let mut data = vec![0u8; count];
        if !self.mem.read(address, &mut data) {
            return Err(14);
        }
        let file = self.io.files.get_mut(&descriptor).ok_or(9u32)?;
        let length = match file {
            GuestFile::File(file) => file.read(&mut data).map_err(|_| 5u32)?,
            GuestFile::Directory(_) => return Err(21),
            GuestFile::Null | GuestFile::Console => 0,
        };
        if !self.mem.write(address, &data[..length]) {
            return Err(14);
        }
        Ok(length as u64)
    }

    fn write_guest_file(&mut self, descriptor: i32, address: u64, count: u64) -> Result<u64, u32> {
        let console = descriptor == 1 || descriptor == 2 || matches!(self.io.files.get(&descriptor), Some(GuestFile::Console));
        let null = matches!(self.io.files.get(&descriptor), Some(GuestFile::Null));
        if !console && !null {
            return Err(9);
        }
        let count = count.min(TRANSFER_LIMIT as u64) as usize;
        let mut data = vec![0u8; count];
        if !self.mem.read(address, &mut data) {
            return Err(14);
        }
        if console {
            let remaining = OUTPUT_LIMIT.saturating_sub(self.output.len());
            let text = String::from_utf8_lossy(&data);
            let mut length = remaining.min(text.len());
            while !text.is_char_boundary(length) { length -= 1; }
            self.output.push_str(&text[..length]);
        }
        Ok(count as u64)
    }

    fn vector_guest_io(&mut self, writing: bool) -> Result<u64, u32> {
        let (descriptor, vectors, count) = (self.arg(0) as i32, self.arg(1), self.arg(2));
        if count > 1024 { return Err(22); }
        let mut total = 0;
        for index in 0..count {
            let mut vector = [0u8; 16];
            let address = vectors.checked_add(index * 16).ok_or(14u32)?;
            if !self.mem.read(address, &mut vector) { return Err(14); }
            let address = u64::from_le_bytes(vector[..8].try_into().unwrap());
            let length = u64::from_le_bytes(vector[8..].try_into().unwrap());
            let length = length.min(TRANSFER_LIMIT as u64 - total);
            let transferred = if writing {
                self.write_guest_file(descriptor, address, length)
            } else {
                self.read_guest_file(descriptor, address, length)
            };
            let transferred = match transferred {
                Ok(value) => value,
                Err(_) if total > 0 => return Ok(total),
                Err(error) => return Err(error),
            };
            total += transferred;
            if transferred < length || total == TRANSFER_LIMIT as u64 { break; }
        }
        Ok(total)
    }

    fn seek_guest_file(&mut self) -> Result<u64, u32> {
        let offset = self.arg(1) as i64;
        let position = match self.arg(2) {
            0 if offset >= 0 => SeekFrom::Start(offset as u64),
            1 => SeekFrom::Current(offset),
            2 => SeekFrom::End(offset),
            _ => return Err(22),
        };
        match self.io.files.get_mut(&(self.arg(0) as i32)) {
            Some(GuestFile::File(file)) => file.seek(position).map_err(|_| 22u32),
            Some(_) => Err(29),
            None => Err(9),
        }
    }

    pub(super) fn call_io(&mut self, name: &str) -> bool {
        if self.call_fios(name) { return true; }
        let result = match name {
            "open" | "_open" | "sceKernelOpen" => self.open_guest_file(),
            "fstat" | "_fstat" | "sceKernelFstat" => self.stat_guest_descriptor(),
            "getdents" | "sceKernelGetdents" => self.read_guest_directory(),
            "read" | "_read" | "sceKernelRead" => self.read_guest_file(self.arg(0) as i32, self.arg(1), self.arg(2)),
            "write" | "_write" | "sceKernelWrite" => self.write_guest_file(self.arg(0) as i32, self.arg(1), self.arg(2)),
            "readv" | "_readv" => self.vector_guest_io(false),
            "writev" | "_writev" => self.vector_guest_io(true),
            "lseek" | "_lseek" | "sceKernelLseek" => self.seek_guest_file(),
            "close" | "_close" | "sceKernelClose" => {
                let descriptor = self.arg(0) as i32;
                if self.io.files.remove(&descriptor).is_some() { Ok(0) } else { Err(9) }
            }
            "ioctl" | "_ioctl" => Err(if self.io.files.contains_key(&(self.arg(0) as i32)) || self.arg(0) <= 2 { 25 } else { 9 }),
            _ => return false,
        };
        match result {
            Ok(value) => self.ret(value),
            Err(error) if name.starts_with("sceKernel") => {
                let encoded = 0x8002_0000 | error;
                let value = if matches!(name, "sceKernelLseek" | "sceKernelRead" | "sceKernelWrite") { encoded as i32 as i64 as u64 } else { encoded as u64 };
                self.ret(value);
            }
            Err(error) => {
                self.put32(self.cpu.base[FS] + 0x80, error);
                self.ret(u64::MAX);
            }
        }
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
    fn reads_assets_and_rejects_path_escape_and_writes() {
        let mut machine = machine();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("hitboy-ps4-io-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("asset.bin"), b"abcdef").unwrap();
        machine.set_game_root(&root).unwrap();
        machine.mem.write(0x50000000, b"/app0/asset.bin\0");
        machine.cpu.r[7] = 0x50000000;
        machine.cpu.r[6] = 0;
        assert!(machine.call_io("_open"));
        let descriptor = machine.cpu.r[0] as i32;
        assert!(descriptor >= 3);
        assert_eq!(machine.read_guest_file(descriptor, 0x50000100, 6), Ok(6));
        let mut bytes = [0u8; 6];
        machine.mem.read(0x50000100, &mut bytes);
        assert_eq!(&bytes, b"abcdef");
        machine.cpu.r[7] = descriptor as u64;
        machine.cpu.r[6] = (-2i64) as u64;
        machine.cpu.r[2] = 2;
        assert_eq!(machine.seek_guest_file(), Ok(4));
        assert_eq!(machine.read_guest_file(descriptor, 0x50000100, 6), Ok(2));
        machine.mem.write(0x50000000, b"/app0/../asset.bin\0");
        machine.cpu.r[7] = 0x50000000;
        machine.cpu.r[6] = 0;
        machine.call_io("_open");
        assert_eq!(machine.cpu.r[0], u64::MAX);
        assert_eq!(machine.mem.read_u64(machine.cpu.base[FS] + 0x80), 13);
        machine.mem.write(0x50000000, b"/app0/asset.bin\0");
        machine.cpu.r[6] = 1;
        machine.call_io("_open");
        assert_eq!(machine.mem.read_u64(machine.cpu.base[FS] + 0x80), 30);
        assert_eq!(std::fs::read(root.join("asset.bin")).unwrap(), b"abcdef");
        machine.reload(&crate::machine::tests::hello_program()).unwrap();
        assert!(machine.io.files.is_empty());
        assert_eq!(machine.io.root.as_deref(), Some(root.canonicalize().unwrap().as_path()));
        std::fs::remove_file(root.join("asset.bin")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn output_and_invalid_descriptors_have_real_results() {
        let mut machine = machine();
        machine.mem.write(0x50000000, b"console output");
        assert_eq!(machine.write_guest_file(1, 0x50000000, 14), Ok(14));
        assert_eq!(machine.output, "console output");
        machine.cpu.r[7] = 999;
        machine.cpu.r[6] = 0x50000000;
        machine.cpu.r[2] = 1;
        machine.call_io("read");
        assert_eq!(machine.cpu.r[0], u64::MAX);
        assert_eq!(machine.mem.read_u64(machine.cpu.base[FS] + 0x80), 9);
        machine.call_io("sceKernelRead");
        assert_eq!(machine.cpu.r[0], 0xFFFFFFFF80020009);
        assert_eq!(machine.write_guest_file(1, 0x1234, 1), Err(14));
    }

    #[test]
    fn memory_unmapping_removes_only_requested_pages() {
        let mut machine = machine();
        machine.mem.map(0x51000000, 8192, true, false).unwrap();
        assert!(machine.mem.unmap(0x51000000, 4096));
        assert!(!machine.mem.read(0x51000000, &mut [0u8; 1]));
        assert!(machine.mem.read(0x51001000, &mut [0u8; 1]));
        assert!(!machine.mem.unmap(0x51001001, 4096));
        assert!(!machine.mem.unmap(u64::MAX - 4095, 8192));
    }
    #[test]
    fn native_wide_file_errors_are_negative_and_posix_errno_is_preserved() {
        let mut machine = machine();
        for name in ["sceKernelRead", "sceKernelWrite", "sceKernelLseek"] {
            machine.cpu.r[7] = 999;
            machine.cpu.r[6] = if name == "sceKernelLseek" { 0 } else { 0x50000000 };
            machine.cpu.r[2] = if name == "sceKernelLseek" { 2 } else { 1 };
            assert!(machine.call_io(name));
            assert_eq!(machine.cpu.r[0], 0xFFFFFFFF80020009);
            assert!((machine.cpu.r[0] as i64) < 0, "{name} must not look like a successful byte count or file offset");
        }
        assert!(machine.call_io("lseek"));
        assert_eq!(machine.cpu.r[0], u64::MAX);
        assert_eq!(machine.mem.read_u64(machine.cpu.base[FS] + 0x80) as u32, 9);
        assert!(machine.call_io("sceKernelClose"));
        assert_eq!(machine.cpu.r[0] as u32, 0x80020009);
    }

    #[test]
    fn native_file_seek_preserves_successful_offsets_above_four_gibibytes() {
        let mut machine = machine();
        let path = std::env::temp_dir().join(format!("hitboy-wide-seek-{}.bin", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        machine.io.files.insert(3, GuestFile::File(std::fs::File::open(&path).unwrap().into()));
        machine.cpu.r[7] = 3;
        machine.cpu.r[6] = 0x1_0000_0010;
        machine.cpu.r[2] = 0;
        assert!(machine.call_io("sceKernelLseek"));
        assert_eq!(machine.cpu.r[0], 0x1_0000_0010);
        assert!((machine.cpu.r[0] as i64) > 0);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 3);
        machine.io.files.remove(&3);
        std::fs::remove_file(path).unwrap();
    }

}
