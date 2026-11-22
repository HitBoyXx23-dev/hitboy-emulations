use super::*;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub(super) struct LaunchState {
    titles: HashMap<String, PathBuf>,
    pending: Option<(String, PathBuf)>,
    active: Option<String>,
    generation: u64,
    original_root: Option<Option<PathBuf>>,
}

fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 9 && bytes[..4].iter().all(u8::is_ascii_uppercase) && bytes[4..].iter().all(u8::is_ascii_digit)
}

impl Machine {
    pub fn register_title(&mut self, path: &Path) -> Result<String, String> {
        let info = crate::metadata::read(path)?;
        if !valid_id(&info.title_id) { return Err("The game needs a valid TITLE_ID in its PS4 metadata.".into()); }
        let file = if path.is_dir() { path.join("eboot.bin") } else { path.to_path_buf() };
        let file = file.canonicalize().map_err(|error| format!("Could not register the game executable: {error}"))?;
        if !file.is_file() { return Err("The registered executable must be a file.".into()); }
        if let Some(previous) = self.launch.titles.get(&info.title_id) {
            if previous != &file { return Err(format!("More than one executable uses title ID {}.", info.title_id)); }
        } else if self.launch.titles.len() >= 256 {
            return Err("The PS4 title registry is full.".into());
        }
        self.launch.titles.insert(info.title_id.clone(), file);
        Ok(info.title_id)
    }

    pub fn title_generation(&self) -> u64 { self.launch.generation }

    pub fn active_title(&self) -> Option<&str> { self.launch.active.as_deref() }

    fn request_launch(&mut self) -> u64 {
        let mut bytes = [0; 10];
        if !self.mem.read(self.arg(0), &mut bytes) { return 0x8002_000E; }
        let Ok(id) = std::str::from_utf8(&bytes[..9]) else { return 0x8002_0016; };
        if bytes[9] != 0 || !valid_id(id) { return 0x8002_0016; }
        if self.arg(1) != 0 {
            let mut argument = [0; 8];
            if !self.mem.read(self.arg(1), &mut argument) { return 0x8002_000E; }
            if argument != [0; 8] { return 0x8002_002D; }
        }
        if self.arg(2) != 0 {
            let mut parameters = [0; 32];
            if !self.mem.read(self.arg(2), &mut parameters) { return 0x8002_000E; }
            if u32::from_le_bytes(parameters[..4].try_into().unwrap()) != 32 { return 0x8002_0016; }
            if u32::from_le_bytes(parameters[4..8].try_into().unwrap()) != 1 || parameters[8..12] != [0; 4] || parameters[16..28] != [0; 12] { return 0x8002_002D; }
        }
        let Some(path) = self.launch.titles.get(id) else { return 0x8002_0002; };
        self.launch.pending = Some((id.to_string(), path.clone()));
        0
    }

    pub(super) fn call_launch(&mut self, name: &str) -> bool {
        if name != "sceSystemServiceLaunchApp" { return false; }
        let result = self.request_launch();
        self.ret(result);
        true
    }

    pub(super) fn apply_launch(&mut self) {
        let Some((id, path)) = self.launch.pending.take() else { return; };
        match Self::load_game(&path) {
            Ok(mut target) => {
                target.launch.titles = std::mem::take(&mut self.launch.titles);
                target.launch.original_root = Some(self.launch.original_root.clone().unwrap_or_else(|| self.game_root()));
                target.launch.active = Some(id);
                target.launch.generation = self.launch.generation.wrapping_add(1);
                target.pad_buttons = self.pad_buttons;
                *self = target;
            }
            Err(_) => self.ret(0x8002_0008),
        }
    }

    pub(super) fn reload_root(&self) -> Option<PathBuf> {
        self.launch.original_root.clone().unwrap_or_else(|| self.game_root())
    }

    pub(super) fn retain_title_registry(&mut self, previous: &mut Self) {
        self.launch.titles = std::mem::take(&mut previous.launch.titles);
        self.launch.generation = previous.launch.generation.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_launch_executes_registered_program_and_retains_registry_on_reset() {
        let root = std::env::temp_dir().join(format!("hitboy-launch-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let image = super::super::tests::hello_program();
        let mut target = image.clone();
        let offset = target.windows(5).position(|bytes| bytes == [0xBF, 7, 0, 0, 0]).unwrap();
        target[offset + 1] = 77;
        std::fs::write(root.join("eboot.bin"), target).unwrap();
        std::fs::write(root.join("param.sfo"), crate::metadata::tests::fixture("CUSA07669")).unwrap();
        let mut machine = Machine::load(&image).unwrap();
        machine.register_title(&root).unwrap();
        let id = machine.heap_alloc(10, 8);
        machine.mem.write(id, b"CUSA07669\0");
        machine.cpu.r[ARG_REGS[0]] = id;
        machine.cpu.r[ARG_REGS[1]] = 0;
        machine.cpu.r[ARG_REGS[2]] = 0;
        assert!(machine.call_launch("sceSystemServiceLaunchApp"));
        assert_eq!(machine.cpu.r[0], 0);
        assert_eq!(machine.run(1000).unwrap(), Stop::Exited(77));
        assert_eq!(machine.active_title(), Some("CUSA07669"));
        assert_eq!(machine.title_generation(), 1);
        machine.reload(&image).unwrap();
        assert!(machine.launch.titles.contains_key("CUSA07669"));
        assert_eq!(machine.title_generation(), 2);
        assert_eq!(machine.game_root(), None);
        assert_eq!(machine.run(1000).unwrap(), Stop::Exited(7));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_and_missing_titles_fail_without_switching_programs() {
        let mut machine = Machine::load(&super::super::tests::hello_program()).unwrap();
        let id = machine.heap_alloc(10, 8);
        machine.mem.write(id, b"CUSA07669\0");
        machine.cpu.r[ARG_REGS[0]] = id;
        machine.cpu.r[ARG_REGS[1]] = 0;
        machine.cpu.r[ARG_REGS[2]] = 0;
        assert_eq!(machine.request_launch(), 0x8002_0002);
        machine.mem.write(id, b"../bad/id\0");
        assert_eq!(machine.request_launch(), 0x8002_0016);
        machine.apply_launch();
        assert_eq!(machine.run(1000).unwrap(), Stop::Exited(7));
        assert_eq!(machine.title_generation(), 0);
    }
    #[test]
    #[ignore]
    fn registered_real_game_reaches_its_own_runtime() {
        let path = std::env::var_os("HITBOY_PS4_TARGET").expect("Set HITBOY_PS4_TARGET to an unpacked game folder.");
        let mut machine = Machine::load(&super::super::tests::hello_program()).unwrap();
        let title = machine.register_title(Path::new(&path)).unwrap();
        let id = machine.heap_alloc(10, 8);
        machine.mem.write(id, format!("{title}\0").as_bytes());
        machine.cpu.r[ARG_REGS[0]] = id;
        machine.cpu.r[ARG_REGS[1]] = 0;
        machine.cpu.r[ARG_REGS[2]] = 0;
        assert_eq!(machine.request_launch(), 0);
        let result = machine.run(100_000);
        assert_eq!(machine.active_title(), Some(title.as_str()));
        assert_eq!(machine.title_generation(), 1);
        println!("Loaded target {title}; result {result:?}; calls {}; missing functions {:?}", machine.calls, machine.unimplemented);
        assert!(machine.calls > 0);
    }

    #[test]
    fn damaged_target_is_rejected_without_replacing_caller() {
        let root = std::env::temp_dir().join(format!("hitboy-invalid-launch-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("eboot.bin"), b"invalid executable").unwrap();
        std::fs::write(root.join("param.sfo"), crate::metadata::tests::fixture("CUSA07669")).unwrap();
        let mut machine = Machine::load(&super::super::tests::hello_program()).unwrap();
        machine.register_title(&root).unwrap();
        machine.launch.pending = Some(("CUSA07669".into(), root.join("eboot.bin")));
        machine.apply_launch();
        assert_eq!(machine.cpu.r[0], 0x8002_0008);
        assert_eq!(machine.title_generation(), 0);
        assert_eq!(machine.run(1000).unwrap(), Stop::Exited(7));
        std::fs::remove_dir_all(root).unwrap();
    }

}
