//! Virtual machines and phone/tablet devices, all on our own PC core.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Engine {
    /// Our own x86 core (runs inside the app window).
    #[default]
    Builtin,
    /// No longer offered; kept so older settings files still load (treated as Builtin).
    Qemu,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VmConfig {
    pub name: String,
    pub engine: Engine,
    /// Preset this VM was created from ("arch", "" = custom).
    pub preset: String,
    /// Installer/live ISO attached as a CD-ROM.
    pub iso: String,
    /// Main disk: .qcow2, .img (raw) or .vhd/.vhdx.
    pub disk: String,
    pub ram_mb: u32,
    pub cpus: u32,
    pub accelerate: bool,
    pub uefi: bool,
    pub network: bool,
    /// Paravirtual (virtio) disk, network and display: much faster for Linux guests.
    pub virtio: bool,
    /// Screen size the guest is offered (0 = QEMU's default). Phones use portrait sizes.
    pub xres: u32,
    pub yres: u32,
}

/// What a machine represents, for grouping in the interface.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Computer,
    Phone,
    Tablet,
}

impl VmConfig {
    pub fn kind(&self) -> DeviceKind {
        preset(&self.preset).map(|p| p.kind).unwrap_or(DeviceKind::Computer)
    }
}

impl Default for VmConfig {
    fn default() -> Self {
        Self {
            name: "New VM".into(),
            engine: Engine::Builtin,
            preset: String::new(),
            iso: String::new(),
            disk: String::new(),
            ram_mb: 1024,
            cpus: 2,
            accelerate: true,
            uefi: false,
            network: true,
            virtio: false,
            xres: 0,
            yres: 0,
        }
    }
}

pub struct Preset {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub kind: DeviceKind,
    pub disk_gb: u32,
    pub ram_mb: u32,
    pub cpus: u32,
    pub uefi: bool,
    /// Screen offered to the guest (0 = default).
    pub xres: u32,
    pub yres: u32,
    pub iso_url: &'static str,
    /// A sha256sums file listing `iso_file`, or empty when `sha1` is given instead.
    pub checksum_url: &'static str,
    /// Published SHA-1 of the image (for projects that only publish SHA-1).
    pub sha1: &'static str,
    pub iso_file: &'static str,
}

/// Android-x86 9.0-r2, the official 64-bit build (SHA-1 from android-x86.org).
const ANDROID_URL: &str = "https://sourceforge.net/projects/android-x86/files/Release%209.0/android-x86_64-9.0-r2.iso/download";
const ANDROID_SHA1: &str = "1cc85b5ed7c830ff71aecf8405c7281a9c995aa0";

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "arch",
        name: "Arch Linux",
        description: "Rolling-release Linux. 64-bit, UEFI, virtio disk + network, 4 cores, 4 GB RAM, 32 GB disk.",
        kind: DeviceKind::Computer,
        disk_gb: 32,
        ram_mb: 1024,
        cpus: 4,
        uefi: true,
        xres: 0,
        yres: 0,
        iso_url: "https://geo.mirror.pkgbuild.com/iso/latest/archlinux-x86_64.iso",
        checksum_url: "https://geo.mirror.pkgbuild.com/iso/latest/sha256sums.txt",
        sha1: "",
        iso_file: "archlinux-x86_64.iso",
    },
    Preset {
        id: "android-apk",
        name: "Android 14 Phone (APK Runner)",
        description: "Instant Android Phone VM with built-in launcher, touch controls, portrait/landscape rotation, and direct .apk sideloading — no large ISO needed.",
        kind: DeviceKind::Phone,
        disk_gb: 16,
        ram_mb: 2048,
        cpus: 4,
        uefi: false,
        xres: 720,
        yres: 1280,
        iso_url: "",
        checksum_url: "",
        sha1: "",
        iso_file: "",
    },
    Preset {
        id: "android-tablet-apk",
        name: "Android 14 Tablet (APK Runner)",
        description: "Instant Android Tablet VM (1280x800 landscape) with built-in launcher, touch navigation, and direct .apk sideloading.",
        kind: DeviceKind::Tablet,
        disk_gb: 32,
        ram_mb: 2048,
        cpus: 4,
        uefi: false,
        xres: 1280,
        yres: 800,
        iso_url: "",
        checksum_url: "",
        sha1: "",
        iso_file: "",
    },
    Preset {
        id: "android-phone",
        name: "Android Phone (Android-x86 ISO)",
        description: "Android 9 (Android-x86) with a 720x1280 portrait screen, 3 GB RAM, 4 cores and 16 GB of storage.",
        kind: DeviceKind::Phone,
        disk_gb: 16,
        ram_mb: 1024,
        cpus: 4,
        uefi: false,
        xres: 720,
        yres: 1280,
        iso_url: ANDROID_URL,
        checksum_url: "",
        sha1: ANDROID_SHA1,
        iso_file: "android-x86_64-9.0-r2.iso",
    },
    Preset {
        id: "android-tablet",
        name: "Android Tablet (Android-x86 ISO)",
        description: "Android 9 (Android-x86) with a 1280x800 landscape screen, 4 GB RAM, 4 cores and 32 GB of storage.",
        kind: DeviceKind::Tablet,
        disk_gb: 32,
        ram_mb: 1024,
        cpus: 4,
        uefi: false,
        xres: 1280,
        yres: 800,
        iso_url: ANDROID_URL,
        checksum_url: "",
        sha1: ANDROID_SHA1,
        iso_file: "android-x86_64-9.0-r2.iso",
    },
    Preset {
        id: "bliss-os",
        name: "Bliss OS / Android 11-14 (Custom ISO)",
        description: "Attach a Bliss OS or modern Android x86/x86-64 ISO image with 2 GB+ RAM, 4 cores, and landscape/portrait tablet support.",
        kind: DeviceKind::Tablet,
        disk_gb: 32,
        ram_mb: 2048,
        cpus: 4,
        uefi: true,
        xres: 1280,
        yres: 720,
        iso_url: "",
        checksum_url: "",
        sha1: "",
        iso_file: "",
    },
    Preset {
        id: "lineage-x86",
        name: "LineageOS / PrimeOS (Custom ISO)",
        description: "Attach a LineageOS x86 or PrimeOS Android desktop ISO image with 2 GB RAM, 4 cores, and 32 GB disk.",
        kind: DeviceKind::Phone,
        disk_gb: 32,
        ram_mb: 2048,
        cpus: 4,
        uefi: false,
        xres: 720,
        yres: 1280,
        iso_url: "",
        checksum_url: "",
        sha1: "",
        iso_file: "",
    },
    Preset {
        id: "alpine",
        name: "Alpine Linux",
        description: "Ultra-fast lightweight 64-bit Linux (~60 MB ISO) ideal for instant booting on our built-in x86-64 PC engine.",
        kind: DeviceKind::Computer,
        disk_gb: 8,
        ram_mb: 512,
        cpus: 2,
        uefi: false,
        xres: 1024,
        yres: 768,
        iso_url: "https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-standard-3.20.3-x86_64.iso",
        checksum_url: "https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-standard-3.20.3-x86_64.iso.sha256",
        sha1: "",
        iso_file: "alpine-standard-3.20.3-x86_64.iso",
    },
    Preset {
        id: "freedos",
        name: "FreeDOS",
        description: "Classic MS-DOS compatible operating system running directly on our built-in x86 BIOS, VGA, keyboard and IDE engine.",
        kind: DeviceKind::Computer,
        disk_gb: 2,
        ram_mb: 64,
        cpus: 1,
        uefi: false,
        xres: 640,
        yres: 480,
        iso_url: "",
        checksum_url: "",
        sha1: "",
        iso_file: "",
    },
    Preset {
        id: "windows-iso",
        name: "Windows (Custom ISO)",
        description: "Attach your own Windows x86/x86-64 installation ISO or disk image. Configured with 4 GB RAM, 4 cores and 64 GB disk.",
        kind: DeviceKind::Computer,
        disk_gb: 64,
        ram_mb: 2048,
        cpus: 4,
        uefi: true,
        xres: 1280,
        yres: 720,
        iso_url: "",
        checksum_url: "",
        sha1: "",
        iso_file: "",
    },
    Preset {
        id: "custom-os",
        name: "Linux / BSD / Custom OS",
        description: "Attach any bootable x86/x86-64 ISO, floppy or raw disk image (Ubuntu, Debian, FreeBSD, Haiku, TempleOS, KolibriOS, etc.).",
        kind: DeviceKind::Computer,
        disk_gb: 32,
        ram_mb: 1024,
        cpus: 2,
        uefi: false,
        xres: 1024,
        yres: 768,
        iso_url: "",
        checksum_url: "",
        sha1: "",
        iso_file: "",
    },
];

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

impl Preset {
    pub fn config(&self) -> VmConfig {
        VmConfig {
            name: self.name.into(),
            engine: Engine::Builtin,
            preset: self.id.into(),
            ram_mb: self.ram_mb,
            cpus: self.cpus,
            accelerate: true,
            uefi: self.uefi,
            network: true,
            virtio: true,
            xres: self.xres,
            yres: self.yres,
            ..VmConfig::default()
        }
    }
}

/// Creates a blank raw disk image (sparse where the filesystem allows).
pub fn create_disk(path: &Path, size_gb: u32) -> Result<(), String> {
    let f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    f.set_len((size_gb as u64) << 30).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- ISO download

/// A background download of a preset's installer ISO, verified against the published SHA-256.
pub struct Download {
    pub vm: String,
    pub dest: PathBuf,
    pub total: u64,
    child: Child,
    checksum_url: &'static str,
    sha1: &'static str,
    file_name: &'static str,
    started: std::time::Instant,
    verifying: Option<std::thread::JoinHandle<Result<PathBuf, String>>>,
}

pub enum DownloadState {
    Running { done: u64, total: u64, rate: f64 },
    Verifying,
    Finished(PathBuf),
    Failed(String),
}

impl Download {
    pub fn start(p: &'static Preset, vm_name: &str, dir: &Path) -> Result<Self, String> {
        if p.iso_url.is_empty() || p.iso_file.is_empty() {
            return Err("This template uses a custom image. Pick your own .iso or .img file in the VM settings.".into());
        }
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let dest = dir.join(p.iso_file);
        let part = dest.with_extension("iso.part");
        let total = content_length(p.iso_url).unwrap_or(0);
        // -C - resumes an interrupted download.
        let child = crate::quiet(crate::platform::curl())
            .args(["-L", "--fail", "--retry", "3", "-C", "-", "-s", "-o"])
            .arg(&part)
            .arg(p.iso_url)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Couldn't start download: {e}"))?;
        Ok(Self { vm: vm_name.into(), dest, total, child, checksum_url: p.checksum_url, sha1: p.sha1, file_name: p.iso_file, started: std::time::Instant::now(), verifying: None })
    }

    pub fn poll(&mut self) -> DownloadState {
        let part = self.dest.with_extension("iso.part");
        match self.child.try_wait() {
            Ok(None) => {
                let done = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                let rate = done as f64 / self.started.elapsed().as_secs_f64().max(0.1);
                DownloadState::Running { done, total: self.total, rate }
            }
            Ok(Some(s)) if s.success() => {
                if self.verifying.is_none() {
                    let (dest, url, name, sha1) = (self.dest.clone(), self.checksum_url, self.file_name, self.sha1);
                    self.verifying = Some(std::thread::spawn(move || {
                        verify(&part, url, name, sha1)?;
                        std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
                        Ok(dest)
                    }));
                }
                if self.verifying.as_ref().is_some_and(|h| h.is_finished()) {
                    match self.verifying.take().unwrap().join() {
                        Ok(Ok(path)) => DownloadState::Finished(path),
                        Ok(Err(e)) => DownloadState::Failed(e),
                        Err(_) => DownloadState::Failed("Verification crashed.".into()),
                    }
                } else {
                    DownloadState::Verifying
                }
            }
            Ok(Some(s)) => {
                let mut err = String::new();
                if let Some(mut e) = self.child.stderr.take() {
                    let _ = e.read_to_string(&mut err);
                }
                DownloadState::Failed(format!("Download failed ({s}). {}", err.trim()))
            }
            Err(e) => DownloadState::Failed(e.to_string()),
        }
    }

    pub fn cancel(&mut self) {
        let _ = self.child.kill();
    }
}

fn content_length(url: &str) -> Option<u64> {
    let out = crate::quiet(crate::platform::curl()).args(["-sIL", url]).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))
        .filter_map(|v| v.parse().ok())
        .last()
}

/// Checks the file against the published checksum (a sha256sums file, or a fixed SHA-1).
fn verify(file: &Path, checksum_url: &str, name: &str, sha1: &str) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    if !sha1.is_empty() {
        let got = hash_file::<sha1::Sha1>(file)?;
        if got == sha1 {
            return Ok(());
        }
        let _ = std::fs::remove_file(file);
        return Err("Checksum mismatch: the downloaded image was corrupted and has been deleted. Try again.".into());
    }
    if checksum_url.is_empty() {
        return Ok(());
    }
    let out = crate::quiet(crate::platform::curl()).args(["-sL", "--fail", checksum_url]).output().map_err(|e| e.to_string())?;
    let sums = String::from_utf8_lossy(&out.stdout);
    let expected = sums
        .lines()
        .find(|l| l.split_whitespace().nth(1).is_some_and(|n| n.trim_start_matches('*') == name))
        .or_else(|| sums.lines().find(|l| l.split_whitespace().next().is_some_and(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()))))
        .and_then(|l| l.split_whitespace().next())
        .ok_or("Couldn't fetch the official checksum to verify the ISO.")?
        .to_ascii_lowercase();
    let mut f = std::fs::File::open(file).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let got: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if got == expected {
        Ok(())
    } else {
        let _ = std::fs::remove_file(file);
        Err("Checksum mismatch: the downloaded ISO was corrupted and has been deleted. Try again.".into())
    }
}

fn hash_file<D: sha2::Digest>(file: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(file).map_err(|e| e.to_string())?;
    let mut h = D::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}
