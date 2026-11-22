//! From a game package on disk to a process ready to run: find the Program NCA, unlock it with
//! the user's keys (and the package's ticket when the content has rights), pull out the ExeFS
//! and load its modules.

use crate::horizon::Process;
use crate::keys::Keys;
use crate::nca::{self, Nca, RightsId};
use crate::pfs;
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

/// The ExeFS files (rtld, main, sdk, subsdk*, main.npdm) of the game in `path`.
pub fn exefs_of(path: &Path, keys: &Keys) -> Result<(u64, Vec<(String, Vec<u8>)>), String> {
    let entries = pfs::game_files(path)?;
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut title_keys: HashMap<RightsId, [u8; 16]> = HashMap::new();
    let mut ticket_problem = None;
    for e in entries.iter().filter(|e| e.name.ends_with(".tik")) {
        let bytes = pfs::read_at(&mut f, e.offset, e.size.min(0x10000) as usize)?;
        match nca::parse_ticket(&bytes, keys) {
            Ok((id, k)) => {
                title_keys.insert(id, k);
            }
            Err(msg) => ticket_problem = Some(msg),
        }
    }
    let mut first_error = None;
    for e in entries.iter().filter(|e| e.name.ends_with(".nca")) {
        let mut n = match Nca::open(&mut f, e, keys) {
            Ok(n) => n,
            Err(msg) => {
                first_error.get_or_insert(msg);
                continue;
            }
        };
        if n.content_type != 0 {
            continue;
        }
        if let Err(msg) = n.unlock(keys, &title_keys) {
            first_error.get_or_insert(ticket_problem.clone().filter(|_| n.uses_rights()).unwrap_or(msg));
            continue;
        }
        match n.exefs(&mut f) {
            Ok(files) => return Ok((n.program_id, files)),
            Err(msg) => {
                first_error.get_or_insert(msg);
            }
        }
    }
    Err(first_error.unwrap_or_else(|| "This package has no game program in it (it may be only an update or DLC).".into()))
}

/// A process for the game in `path`, ready to `run`.
pub fn boot_game(path: &Path, keys: &Keys) -> Result<Process, String> {
    let entries = pfs::game_files(path)?;
    let mut f = File::open(path).map_err(|e| e.to_string())?;
    let mut title_keys: HashMap<RightsId, [u8; 16]> = HashMap::new();
    let mut ticket_problem = None;
    for e in entries.iter().filter(|e| e.name.ends_with(".tik")) {
        let bytes = pfs::read_at(&mut f, e.offset, e.size.min(0x10000) as usize)?;
        match nca::parse_ticket(&bytes, keys) {
            Ok((id, k)) => {
                title_keys.insert(id, k);
            }
            Err(msg) => ticket_problem = Some(msg),
        }
    }
    let mut first_error = None;
    for e in entries.iter().filter(|e| e.name.ends_with(".nca")) {
        let mut n = match Nca::open(&mut f, e, keys) {
            Ok(n) => n,
            Err(msg) => {
                first_error.get_or_insert(msg);
                continue;
            }
        };
        if n.content_type != 0 {
            continue;
        }
        if let Err(msg) = n.unlock(keys, &title_keys) {
            first_error.get_or_insert(ticket_problem.clone().filter(|_| n.uses_rights()).unwrap_or(msg));
            continue;
        }
        match n.exefs(&mut f) {
            Ok(files) => {
                let mut p = Process::boot(&files)?;
                p.program_id = n.program_id;
                p.romfs = n.romfs_bytes(&mut f).ok().flatten();
                return Ok(p);
            }
            Err(msg) => {
                first_error.get_or_insert(msg);
            }
        }
    }
    Err(first_error.unwrap_or_else(|| "This package has no game program in it (it may be only an update or DLC).".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nca::tests::{keys_text, make_nca, pfs0};
    use std::io::Write;

    /// A minimal NSO holding `words` as its code.
    fn nso(words: &[u32]) -> Vec<u8> {
        let text: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut f = vec![0u8; 0x100];
        f[..4].copy_from_slice(b"NSO0");
        let seg = |f: &mut Vec<u8>, k: usize, off: usize, size: usize| {
            f[0x10 + k * 0x10..][..4].copy_from_slice(&(off as u32).to_le_bytes());
            f[0x14 + k * 0x10..][..4].copy_from_slice(&((k * 0x1000) as u32).to_le_bytes());
            f[0x18 + k * 0x10..][..4].copy_from_slice(&(size as u32).to_le_bytes());
            f[0x60 + k * 4..][..4].copy_from_slice(&(size as u32).to_le_bytes());
        };
        seg(&mut f, 0, 0x100, text.len());
        seg(&mut f, 1, 0x100 + text.len(), 0);
        seg(&mut f, 2, 0x100 + text.len(), 0);
        f.extend(text);
        f
    }

    #[test]
    fn an_nsp_with_a_program_boots_and_runs() {
        // rtld: movz x0,#42 ; svc #7 (ExitProcess)
        let rtld = nso(&[0xD2800540, 0xD40000E1]);
        let main = nso(&[0xD4000001]);
        let nca = make_nca(&[("rtld", &rtld), ("main", &main), ("main.npdm", b"META")], [0; 16], [0x31; 16]);
        // Wrap the NCA in a PFS0 (.nsp).
        let nsp = pfs0(&[("game.nca", &nca)]);
        let path = std::env::temp_dir().join("switch_launch_test.nsp");
        std::fs::File::create(&path).unwrap().write_all(&nsp).unwrap();
        let keys = Keys::parse(&keys_text("")).unwrap();
        let mut p = boot_game(&path, &keys).unwrap();
        assert_eq!(p.program_id, 0x0100_0000_0000_1000);
        assert_eq!(p.run(100), crate::horizon::Stop::Exited(42));
        assert_eq!(p.cpu.x[1], crate::horizon::MAIN_THREAD_HANDLE);
    }

    #[test]
    fn a_package_without_a_program_says_so() {
        let nsp = pfs0(&[("readme.txt", b"hi")]);
        let path = std::env::temp_dir().join("switch_launch_empty.nsp");
        std::fs::File::create(&path).unwrap().write_all(&nsp).unwrap();
        let keys = Keys::parse(&keys_text("")).unwrap();
        assert!(boot_game(&path, &keys).err().unwrap().contains("no game program"));
    }
}
