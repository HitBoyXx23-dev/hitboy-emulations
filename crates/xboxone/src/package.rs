//! Inspecting an Xbox One / Series X|S package: is it readable, is it complete, what does it
//! need from us, and can it run.
//!
//! This works on packages that are already decrypted — Developer Mode apps, and anything you have
//! legitimately taken off your own console. Store and disc titles arrive encrypted, and there is
//! nothing to inspect until they are not, which no PC-side tool can change.

use crate::modules::{self, Need, Provider};
use crate::pe::Pe;
use crate::GameInfo;
use std::path::{Path, PathBuf};

/// How far along a package is. The ordering is deliberate: a bigger value is further along, so
/// a caller can keep the worst one it has seen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// Not an Xbox package.
    #[default]
    NotAPackage,
    /// Encrypted; there is nothing to read.
    Encrypted,
    /// Broken in a way that stops it running.
    Unusable,
    /// Readable, but something is missing or worth knowing.
    Incomplete,
    /// Everything the loader needs is present.
    Complete,
}

/// What provides one imported library.
#[derive(Debug, Clone, PartialEq)]
pub struct Dependency {
    pub dll: String,
    pub functions: usize,
    /// A short description of where the functions come from.
    pub source: String,
    /// True when this PC can satisfy the whole library.
    pub satisfied: bool,
    /// True when only part of it can be satisfied (some names are Xbox-only).
    pub partial: bool,
}

/// What inspecting a package found.
#[derive(Debug, Clone, Default)]
pub struct PackageReport {
    pub root: PathBuf,
    /// Which manifest was used, or empty when there was none.
    pub manifest: String,
    pub game: GameInfo,
    pub executable: PathBuf,
    pub image_size: u32,
    pub sections: usize,
    pub entry: u32,
    pub relocations: usize,
    pub uses_tls: bool,
    pub imports: usize,
    pub dependencies: Vec<Dependency>,
    pub verdict: Verdict,
    pub notes: Vec<String>,
}

impl PackageReport {
    fn note(&mut self, s: impl Into<String>) {
        self.notes.push(s.into());
    }

    /// Libraries nothing can provide, which is what actually stops a title running.
    pub fn blocking(&self) -> Vec<&Dependency> {
        self.dependencies.iter().filter(|d| !d.satisfied).collect()
    }

    /// Libraries we answer ourselves rather than redirecting to Windows.
    pub fn shimmed(&self) -> Vec<&Dependency> {
        self.dependencies.iter().filter(|d| d.source.starts_with("our")).collect()
    }

    pub fn text(&self) -> String {
        let mut s = format!("Xbox One / Series X|S: package");
        if !self.game.name.is_empty() {
            s.push_str(&format!(" \"{}\"", self.game.name));
        }
        if !self.game.version.is_empty() {
            s.push_str(&format!(" {}", self.game.version));
        }
        s.push('\n');
        s.push_str(match self.verdict {
            Verdict::Encrypted => "  Encrypted; it can't be read on this PC.\n",
            Verdict::NotAPackage => "  Not an Xbox One / Series package.\n",
            Verdict::Unusable => "  Not runnable as it is.\n",
            Verdict::Incomplete => "  Readable, but check the notes.\n",
            Verdict::Complete => "  Readable and complete enough to load.\n",
        });
        if !self.dependencies.is_empty() {
            s.push_str(&format!("  {} libraries, {} imported functions.\n", self.dependencies.len(), self.imports));
        }
        for d in &self.dependencies {
            let mark = if d.satisfied {
                "ok"
            } else if d.partial {
                "part"
            } else {
                "no "
            };
            s.push_str(&format!("    [{mark}] {:<22} {:>5} fn  {}\n", d.dll, d.functions, d.source));
        }
        for n in &self.notes {
            s.push_str(&format!("  - {n}\n"));
        }
        s
    }
}

/// Looks for either manifest name, whichever the package actually used. Returns the path, whether
/// it's a GDK or XDK manifest, and the name as it is actually spelled on disk.
pub fn find_manifest(dir: &Path) -> Option<(PathBuf, bool, String)> {
    for name in ["MicrosoftGame.config", "appxmanifest.xml", "AppxManifest.xml"] {
        let p = dir.join(name);
        if p.is_file() {
            let real = std::fs::read_dir(dir)
                .ok()
                .and_then(|it| {
                    it.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .find(|n| n.eq_ignore_ascii_case(name))
                })
                .unwrap_or_else(|| name.to_string());
            return Some((p, name == "MicrosoftGame.config", real));
        }
    }
    None
}

/// True when the file is an encrypted XVC container.
pub fn is_encrypted_file(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 0x208];
    match std::fs::File::open(path) {
        Ok(mut f) => f.read(&mut head).map_or(false, |n| crate::is_encrypted_package(&head[..n])),
        Err(_) => false,
    }
}

/// Inspects a package folder, or a file inside one.
pub fn inspect(path: &Path) -> PackageReport {
    let mut r = PackageReport { root: path.to_path_buf(), ..Default::default() };

    // A bare encrypted container can be recognised before anything else.
    if path.is_file() && is_encrypted_file(path) {
        r.verdict = Verdict::Encrypted;
        r.note(crate::ENCRYPTED_MESSAGE);
        return r;
    }
    let dir = if path.is_dir() { path.to_path_buf() } else { path.parent().map(Path::to_path_buf).unwrap_or_default() };
    let Some((manifest_path, is_gdk, real)) = find_manifest(&dir) else {
        r.verdict = Verdict::NotAPackage;
        r.note("This folder has no MicrosoftGame.config or appxmanifest.xml, so it isn't an Xbox One / Series package. Choose the package's top folder.");
        return r;
    };
    r.manifest = real;
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(t) => t,
        Err(e) => {
            r.verdict = Verdict::Unusable;
            r.note(format!("Couldn't read the manifest: {e}"));
            return r;
        }
    };
    r.game = if is_gdk { crate::parse_game_config(&text) } else { crate::parse_appx_manifest(&text) };

    if r.game.executable.is_empty() {
        r.verdict = Verdict::Unusable;
        r.note("The manifest doesn't name an executable, so there is nothing to load.");
        return r;
    }
    let exe = dir.join(&r.game.executable);
    r.executable = exe.clone();
    let data = match std::fs::read(&exe) {
        Ok(d) => d,
        Err(e) => {
            r.verdict = Verdict::Unusable;
            r.note(format!("The manifest names {} but it can't be opened: {e}. A dump missing its executable is incomplete.", r.game.executable));
            return r;
        }
    };
    if crate::is_encrypted_package(&data[..data.len().min(0x208)]) {
        r.verdict = Verdict::Encrypted;
        r.note(crate::ENCRYPTED_MESSAGE);
        return r;
    }
    let pe = match crate::pe::parse(&data) {
        Ok(p) => p,
        Err(e) => {
            r.verdict = Verdict::Unusable;
            r.note(format!("{}: {e}", r.game.executable));
            return r;
        }
    };
    if !pe.is_x64() {
        r.verdict = Verdict::Unusable;
        r.note(format!("{} is machine {:#x}, not x86-64. Xbox One and Series X|S only run 64-bit programs.", r.game.executable, pe.machine));
        return r;
    }

    r.image_size = pe.image_size;
    r.sections = pe.sections.len();
    r.entry = pe.entry;
    r.relocations = pe.relocs.len();
    r.uses_tls = pe.tls.is_some();
    r.game.x64 = true;
    r.game.imports = pe.dll_names();
    r.imports = pe.import_count();
    r.dependencies = dependencies(&pe);

    // Finalise the verdict from what is missing.
    let blocking: Vec<String> = r.dependencies.iter().filter(|d| !d.satisfied).map(|d| d.dll.clone()).collect();
    if !blocking.is_empty() {
        r.verdict = Verdict::Incomplete;
        r.note(format!(
            "{} librar{} nothing on this PC provides, so the game stops when it calls one: {}. The graphics backend for these is the next thing to build.",
            blocking.len(),
            if blocking.len() == 1 { "y has" } else { "ies have" },
            blocking.join(", ")
        ));
    } else if pe.image_base == 0 {
        r.verdict = Verdict::Incomplete;
        r.note("The program has no preferred load address, so the loader may not be able to place it.");
    } else {
        r.verdict = Verdict::Complete;
    }
    if r.game.devices.iter().any(|d| d.contains("Series")) {
        r.note("This title targets Xbox Series X|S, so some of its libraries are the newer ones and aren't all covered yet.");
    }
    if !r.game.title_id.is_empty() {
        r.note(format!("Title id {}.", r.game.title_id));
    }
    r
}

/// Works out, for each imported library, whether this PC can supply it.
pub fn dependencies(pe: &Pe) -> Vec<Dependency> {
    let mut out: Vec<Dependency> = Vec::new();
    for m in &pe.imports {
        let n = m.functions.len();
        let (source, satisfied, partial) = match modules::provider(&m.dll) {
            Some(Provider::Windows(sys)) => {
                // A redirected library can still miss a name or two, which only shows up when a
                // function is resolved, so the honest answer here is "most of it".
                (format!("this PC's {sys}"), true, true)
            }
            Some(Provider::Shim { need, .. }) => (format!("our own code ({})", need.label()), true, false),
            Some(Provider::Unavailable) => ("nothing on this PC yet".into(), false, false),
            None => ("unknown library".into(), false, false),
        };
        out.push(Dependency { dll: m.dll.clone(), functions: n, source, satisfied, partial });
    }
    out.sort_by(|a, b| a.dll.to_ascii_lowercase().cmp(&b.dll.to_ascii_lowercase()));
    out
}

/// Xbox libraries a package needs that we would have to implement ourselves, hardest first.
pub fn still_to_build(pe: &Pe) -> Vec<(String, Need)> {
    let mut v: Vec<(String, Need)> = Vec::new();
    for m in &pe.imports {
        if let Some(Provider::Shim { need, .. }) = modules::provider(&m.dll) {
            v.push((m.dll.clone(), need));
        }
    }
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pe::sample_pe;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xbox_pkg_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A Developer Mode style package: a manifest and a 64-bit executable.
    fn gdk_package(dir: &Path, exe: &[u8]) {
        std::fs::write(
            dir.join("MicrosoftGame.config"),
            r#"<Game configVersion="1"><Identity Name="Contoso.Demo" Version="1.4.0.0"/>
<ExecutableList><Executable Name="Demo.exe" TargetDeviceFamily="Scarlett" Id="Game"/></ExecutableList>
<ShellVisuals DefaultDisplayName="Demo Game" PublisherDisplayName="Contoso"/><TitleId>ABCD1234</TitleId></Game>"#,
        )
        .unwrap();
        std::fs::write(dir.join("Demo.exe"), exe).unwrap();
    }

    #[test]
    fn reads_a_developer_mode_package() {
        let d = temp("devmode");
        gdk_package(&d, &sample_pe(&["kernelx.dll", "d3d12_x.dll", "xgameruntime.dll"]));
        let r = inspect(&d);
        assert_eq!(r.verdict, Verdict::Complete, "report was:\n{}", r.text());
        assert_eq!(r.game.name, "Demo Game");
        assert_eq!(r.game.devices, vec!["Xbox Series X|S"]);
        assert_eq!(r.game.title_id, "ABCD1234");
        assert_eq!(r.imports, 3);
        assert!(r.executable.ends_with("Demo.exe"));
        // kernelx and d3d12_x redirect to Windows; xgameruntime is ours.
        let kx = r.dependencies.iter().find(|d| d.dll == "kernelx.dll").unwrap();
        assert!(kx.satisfied && kx.source.contains("kernel32"));
        let xg = r.dependencies.iter().find(|d| d.dll == "xgameruntime.dll").unwrap();
        assert!(xg.satisfied && xg.source.starts_with("our"));
    }

    #[test]
    fn a_library_nothing_provides_is_reported_as_blocking() {
        let d = temp("blocking");
        gdk_package(&d, &sample_pe(&["kernelx.dll", "xboxnet.dll"]));
        let r = inspect(&d);
        assert_eq!(r.verdict, Verdict::Incomplete);
        assert_eq!(r.blocking().len(), 1);
        assert_eq!(r.blocking()[0].dll, "xboxnet.dll");
        assert!(r.text().contains("xboxnet.dll"));
    }

    #[test]
    fn a_missing_executable_is_incomplete_not_a_crash() {
        let d = temp("noexe");
        std::fs::write(d.join("MicrosoftGame.config"), r#"<Game><ExecutableList><Executable Name="Gone.exe" Id="G"/></ExecutableList></Game>"#).unwrap();
        let r = inspect(&d);
        assert_eq!(r.verdict, Verdict::Unusable);
        assert!(r.text().contains("Gone.exe"));
    }

    #[test]
    fn an_encrypted_container_is_refused_with_the_reason() {
        let d = temp("enc");
        let mut xvc = vec![0u8; 0x600];
        xvc[0x200..0x208].copy_from_slice(b"msft-xvd");
        std::fs::write(d.join("game.xvc"), &xvc).unwrap();
        let r = inspect(&d.join("game.xvc"));
        assert_eq!(r.verdict, Verdict::Encrypted);
        assert!(r.text().contains("no PC can unlock"));
    }

    #[test]
    fn a_plain_folder_is_not_a_package() {
        let d = temp("plain");
        std::fs::write(d.join("readme.txt"), "hello").unwrap();
        let r = inspect(&d);
        assert_eq!(r.verdict, Verdict::NotAPackage);
    }

    #[test]
    fn dependencies_come_back_sorted() {
        let pe = crate::pe::parse(&sample_pe(&["kernelx.dll", "d3d12_x.dll", "bcrypt.dll"])).unwrap();
        let deps = dependencies(&pe);
        let names: Vec<&str> = deps.iter().map(|d| d.dll.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_by_key(|n| n.to_ascii_lowercase());
        assert_eq!(names, sorted);
    }

    #[test]
    fn an_xdk_package_is_also_understood() {
        let d = temp("xdk");
        std::fs::write(d.join("appxmanifest.xml"), r#"<Package><Identity Name="T.Game" Version="2.0.0.0"/><Properties><DisplayName>Legacy</DisplayName></Properties><Application Executable="t.exe"/></Package>"#).unwrap();
        std::fs::write(d.join("t.exe"), sample_pe(&["kernelx.dll"])).unwrap();
        let r = inspect(&d);
        assert_eq!(r.verdict, Verdict::Complete, "{}", r.text());
        assert_eq!(r.game.name, "Legacy");
        assert_eq!(r.game.kit, "XDK");
    }
}
