//! Apple iOS Firmware (.ipsw) inspector.
//!
//! Reads `.ipsw` ZIP archives directly from a `File` without loading the multi-gigabyte
//! archive into memory: parses `Restore.plist` / `BuildManifest.plist` and identifies
//! kernelcaches, bootloaders (iBoot/LLB/iBSS/iBEC), and root filesystem `.dmg` images.

use crate::plist::{self, Value};
use std::fs::File;
use std::io::{Read, Seek};
use std::path::Path;

#[derive(Debug, Clone, Default)]
pub struct KernelProbe {
    pub entry_name: String,
    pub container: String,
    pub arch: String,
    pub segments: usize,
    pub entry_pc: u64,
    pub darwin_banner: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Default)]
pub struct IpswInfo {
    pub product_version: String,
    pub product_build: String,
    pub supported_products: Vec<String>,
    pub kernelcaches: Vec<(String, String)>, // (entry_name, format)
    pub kernel_probe: Option<KernelProbe>,
    pub bootloaders: Vec<String>,
    pub root_dmg: Option<(String, u64)>,
    pub total_files: usize,
}

impl IpswInfo {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "iOS {} (Build {}) firmware package ({} files).",
            if self.product_version.is_empty() { "?" } else { &self.product_version },
            if self.product_build.is_empty() { "?" } else { &self.product_build },
            self.total_files,
        );
        if !self.supported_products.is_empty() {
            let shown: Vec<&str> = self.supported_products.iter().take(6).map(String::as_str).collect();
            let extra = self.supported_products.len().saturating_sub(shown.len());
            if extra > 0 {
                s.push_str(&format!("\nDevices: {} (+{extra} more).", shown.join(", ")));
            } else {
                s.push_str(&format!("\nDevices: {}.", shown.join(", ")));
            }
        }
        if let Some((dmg, sz)) = &self.root_dmg {
            let mb = *sz as f64 / (1024.0 * 1024.0);
            s.push_str(&format!("\nRoot filesystem: {dmg} ({mb:.1} MB)."));
        }
        if !self.kernelcaches.is_empty() {
            let ks: Vec<String> = self
                .kernelcaches
                .iter()
                .take(4)
                .map(|(n, fmt)| format!("{n} [{fmt}]"))
                .collect();
            s.push_str(&format!("\nKernelcache: {}.", ks.join(", ")));
        }
        if !self.bootloaders.is_empty() {
            s.push_str(&format!("\nBootloaders: {} image(s) (iBoot/LLB/iBSS/iBEC).", self.bootloaders.len()));
        }
        if let Some(kp) = &self.kernel_probe {
            s.push_str(&format!("\n\nKernel bring-up ({}): {}", kp.entry_name, kp.detail));
            if let Some(banner) = &kp.darwin_banner {
                s.push_str(&format!("\n{banner}"));
            }
        }
        s.push_str("\n\nFull iPhone OS UI boot still requires emulating Apple's S5L/A-series SoC peripherals (display, NAND/NVMe, SEP, and XNU device tree).");
        s
    }
}

/// Builds one 4-byte-aligned Apple DeviceTree property entry (`name[32] + u32 len + val`).
fn encode_adt_prop(name: &str, value: &[u8]) -> Vec<u8> {
    let padded = (value.len() + 3) & !3;
    let mut out = vec![0u8; 36 + padded];
    let nb = name.as_bytes();
    let n = nb.len().min(31);
    out[..n].copy_from_slice(&nb[..n]);
    out[32..36].copy_from_slice(&(value.len() as u32).to_le_bytes());
    out[36..36 + value.len()].copy_from_slice(value);
    out
}

/// Walks an Apple DeviceTree (ADT) binary, clears bit 31 (`kPropTemplate`) on property
/// lengths, and fills key iBoot runtime properties (`cpu0/state = "running"`,
/// `chip-revision = 0x10`, `/chosen/random-seed`, `/chosen/dram-base`, `/chosen/dram-size`,
/// `/chosen/debug-enabled`) just as Apple's iBoot does before handing the DeviceTree to XNU.
pub(crate) fn sanitize_apple_devicetree(dt: &mut Vec<u8>) {
    fn walk(dt: &mut Vec<u8>, pos: &mut usize, depth: usize, in_cpu0: bool) -> Option<()> {
        if depth > 32 || *pos + 8 > dt.len() {
            return None;
        }
        let node_start = *pos;
        let mut n_props = u32::from_le_bytes(dt[*pos..*pos + 4].try_into().ok()?);
        let n_children = u32::from_le_bytes(dt[*pos + 4..*pos + 8].try_into().ok()?);
        *pos += 8;
        let mut is_cpu0 = in_cpu0;
        let mut is_chosen = false;
        let mut node_name = String::new();
        let mut dump_props: Vec<(String, Vec<u8>)> = Vec::new();
        let dt_dump = std::env::var_os("IOS_DT_DUMP").is_some();
        let mut has_dram_base = false;
        let mut has_dram_size = false;
        let mut has_debug_enabled = false;
        let mut state_range: Option<(usize, usize)> = None;
        for _ in 0..n_props {
            if *pos + 36 > dt.len() {
                return None;
            }
            let name_bytes = &dt[*pos..*pos + 32];
            let name_end = name_bytes.iter().position(|&b| b == 0).unwrap_or(32);
            let name = std::str::from_utf8(&name_bytes[..name_end]).unwrap_or("").to_string();
            let raw_len = u32::from_le_bytes(dt[*pos + 32..*pos + 36].try_into().ok()?);
            let clean_len = raw_len & 0x7FFF_FFFF;
            if raw_len != clean_len {
                dt[*pos + 32..*pos + 36].copy_from_slice(&clean_len.to_le_bytes());
            }
            let val_start = *pos + 36;
            let val_len = clean_len as usize;
            let padded = (val_len + 3) & !3;
            *pos = val_start.checked_add(padded)?;
            if *pos > dt.len() {
                return None;
            }
            if name == "name" {
                let s = String::from_utf8_lossy(&dt[val_start..val_start + val_len]).trim_end_matches('\0').to_string();
                node_name = s;
                if val_len >= 4 && &dt[val_start..val_start + 4] == b"cpu0" {
                    is_cpu0 = true;
                } else if val_len >= 6 && &dt[val_start..val_start + 6] == b"chosen" {
                    is_chosen = true;
                }
            }
            if dt_dump {
                dump_props.push((name.clone(), dt[val_start..val_start + val_len].to_vec()));
            }
            if name == "name" {
                // already handled above
            } else if name == "state" && val_len >= 8 {
                state_range = Some((val_start, val_len));
            } else if name == "chip-revision" && val_len >= 4 && dt[val_start..val_start + 4] == [0, 0, 0, 0] {
                dt[val_start..val_start + 4].copy_from_slice(&0x10u32.to_le_bytes());
            } else if name == "random-seed" && val_len > 0 {
                for (k, b) in dt[val_start..val_start + val_len].iter_mut().enumerate() {
                    *b = (k as u8).wrapping_mul(37).wrapping_add(0xA5);
                }
            } else if name == "firmware-version" && val_len >= 16 && dt[val_start] == 0 {
                let ver = b"iBoot-8419.2.4\0";
                dt[val_start..val_start + ver.len()].copy_from_slice(ver);
            } else if is_chosen && name == "dram-base" && val_len >= 8 {
                has_dram_base = true;
                if dt[val_start..val_start + 8] == [0; 8] {
                    dt[val_start..val_start + 8].copy_from_slice(&0x0000_0008_0000_0000u64.to_le_bytes());
                }
            } else if is_chosen && name == "dram-size" && val_len >= 8 {
                has_dram_size = true;
                if dt[val_start..val_start + 8] == [0; 8] {
                    dt[val_start..val_start + 8].copy_from_slice(&(512u64 << 20).to_le_bytes());
                }
            } else if is_chosen && name == "debug-enabled" && val_len >= 4 {
                has_debug_enabled = true;
                dt[val_start..val_start + 4].copy_from_slice(&1u32.to_le_bytes());
            }
        }
        if is_cpu0 {
            if let Some((vs, vl)) = state_range {
                if vl >= 8 {
                    dt[vs..vs + 8].copy_from_slice(b"running\0");
                }
            }
        }
        if is_chosen {
            let mut extra = Vec::new();
            if !has_dram_base {
                extra.extend_from_slice(&encode_adt_prop("dram-base", &0x0000_0008_0000_0000u64.to_le_bytes()));
                n_props += 1;
            }
            if !has_dram_size {
                extra.extend_from_slice(&encode_adt_prop("dram-size", &(512u64 << 20).to_le_bytes()));
                n_props += 1;
            }
            if !has_debug_enabled {
                extra.extend_from_slice(&encode_adt_prop("debug-enabled", &1u32.to_le_bytes()));
                n_props += 1;
            }
            if !extra.is_empty() {
                let insert_at = *pos;
                dt.splice(insert_at..insert_at, extra.iter().copied());
                *pos += extra.len();
                dt[node_start..node_start + 4].copy_from_slice(&n_props.to_le_bytes());
            }
        } else if node_name == "amcc" {
            let mut extra = Vec::new();
            for (k, v) in [
                ("aperture-count", 1u32.to_le_bytes().as_slice()),
                ("aperture-size", 0x10000u32.to_le_bytes().as_slice()),
                ("aperture-phys-addr", 0x0000_0002_0000_0000u64.to_le_bytes().as_slice()),
                ("plane-count", 1u32.to_le_bytes().as_slice()),
                ("plane-stride", 0x40000u32.to_le_bytes().as_slice()),
                ("cache-status-reg-offset", 0x100u32.to_le_bytes().as_slice()),
                ("cache-status-reg-mask", 0x1u32.to_le_bytes().as_slice()),
                ("cache-status-reg-value", 0x0u32.to_le_bytes().as_slice()),
            ] {
                extra.extend_from_slice(&encode_adt_prop(k, v));
                n_props += 1;
            }
            let insert_at = *pos;
            dt.splice(insert_at..insert_at, extra.iter().copied());
            *pos += extra.len();
            dt[node_start..node_start + 4].copy_from_slice(&n_props.to_le_bytes());
        } else if node_name == "amcc-ctrr-a" {
            let mut extra = Vec::new();
            for (k, v) in [
                ("page-size-shift", 14u32),
                ("lower-limit-reg-offset", 0x680),
                ("lower-limit-reg-mask", 0xFFFF_FFFF),
                ("lower-limit-reg-shift", 0),
                ("upper-limit-reg-offset", 0x684),
                ("upper-limit-reg-mask", 0xFFFF_FFFF),
                ("upper-limit-reg-shift", 0),
                ("lock-reg-offset", 0x688),
                ("lock-reg-mask", 0x1),
                ("lock-reg-shift", 0),
                ("lock-reg-value", 0x1),
                ("enable-reg-offset", 0x68C),
                ("enable-reg-mask", 0x1),
                ("enable-reg-shift", 0),
                ("enable-reg-value", 0x1),
                ("write-disable-reg-offset", 0x690),
                ("write-disable-reg-mask", 0x1),
                ("write-disable-reg-shift", 0),
                ("write-disable-reg-value", 0x1),
            ] {
                extra.extend_from_slice(&encode_adt_prop(k, &v.to_le_bytes()));
                n_props += 1;
            }
            let insert_at = *pos;
            dt.splice(insert_at..insert_at, extra.iter().copied());
            *pos += extra.len();
            dt[node_start..node_start + 4].copy_from_slice(&n_props.to_le_bytes());
        }
        if dt_dump && (node_name.contains("lock") || node_name.contains("amcc") || node_name.contains("ctrr") || node_name == "chosen") {
            eprintln!("DT[d={depth}] {node_name} (n_props={n_props}, n_children={n_children}):");
            for (k, v) in &dump_props {
                eprintln!("  {k} ({}B): {:02x?}", v.len(), &v[..v.len().min(16)]);
            }
        }
        for _ in 0..n_children {
            walk(dt, pos, depth + 1, false)?;
        }
        Some(())
    }
    let mut pos = 0;
    let _ = walk(dt, &mut pos, 0, false);
}

fn probe_kernel_bytes(entry_name: &str, raw: &[u8], device_tree: Option<&[u8]>) -> KernelProbe {
    let mut probe = KernelProbe {
        entry_name: entry_name.to_string(),
        container: sniff_container(raw).to_string(),
        ..Default::default()
    };

    // Unwrap IM4P/IMG4 if present, then decompress complzss if present.
    let payload: Vec<u8> = if let Ok(im4p) = crate::im4p::parse_im4p(raw) {
        probe.container = format!("IM4P ({})", im4p.fourcc);
        if im4p.payload.starts_with(b"complzss") {
            match crate::im4p::decompress_complzss(&im4p.payload) {
                Ok(dec) => {
                    probe.container.push_str(" + complzss");
                    dec
                }
                Err(e) => {
                    probe.detail = format!("IM4P unwrapped, but complzss decompression failed: {e}");
                    return probe;
                }
            }
        } else if im4p.payload.starts_with(b"bvx2") || im4p.payload.starts_with(b"bvx1") || im4p.payload.starts_with(b"bvxn") || im4p.payload.starts_with(b"bvx-") {
            match crate::im4p::decompress_lzfse(&im4p.payload) {
                Ok(dec) => {
                    probe.container.push_str(" + LZFSE (bvx2)");
                    dec
                }
                Err(e) => {
                    probe.detail = format!("IM4P unwrapped, but LZFSE decompression failed: {e}");
                    return probe;
                }
            }
        } else {
            im4p.payload
        }
    } else if raw.starts_with(b"complzss") {
        match crate::im4p::decompress_complzss(raw) {
            Ok(dec) => dec,
            Err(e) => {
                probe.detail = format!("complzss decompression failed: {e}");
                return probe;
            }
        }
    } else {
        raw.to_vec()
    };

    probe.darwin_banner = crate::im4p::extract_xnu_banner(&payload);

    match crate::macho::parse(&payload) {
        Ok(m) => {
            if let Some(sl) = m.slices.first() {
                probe.arch = sl.arch.clone();
                probe.segments = sl.segments.len();
                probe.entry_pc = sl.entry;
                if matches!(sl.arch.as_str(), "armv6" | "armv7" | "armv7s" | "arm") && !sl.encrypted {
                    match crate::runtime::run_macho(&payload, 50_000) {
                        Ok(r) => {
                            probe.detail = format!(
                                "Unpacked {} XNU Mach-O ({} segments, entry {:#010X}); ran {} bring-up instructions ({}).",
                                sl.arch, sl.segments.len(), sl.entry, r.instructions, r.stop_reason
                            );
                        }
                        Err(e) => {
                            probe.detail = format!("Unpacked {} XNU Mach-O ({} segments, entry {:#010X}); bring-up stopped: {e}", sl.arch, sl.segments.len(), sl.entry);
                        }
                    }
                } else if (sl.arch == "arm64" || sl.arch == "arm64e") && !sl.encrypted {
                    let steps = std::env::var("IOS_XNU_STEPS")
                        .ok()
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(500_000);
                    match crate::runtime::run_macho_arm64(&payload, device_tree, steps) {
                        Ok(r) => {
                            let mut det = format!(
                                "Unpacked {} XNU Mach-O kernel ({} segments, entry {:#018X}, {:.1} MB); AArch64 EL1 core ran {} bring-up instruction(s) (PC {:#018X}, inst {:#010X}, {} sysregs, MMU={}) -> {}.",
                                sl.arch,
                                sl.segments.len(),
                                sl.entry,
                                payload.len() as f64 / (1024.0 * 1024.0),
                                r.instructions,
                                r.final_pc,
                                r.final_inst,
                                r.sysregs_written,
                                r.mmu_enabled,
                                r.stop_reason,
                            );
                            if let Some(dt) = device_tree {
                                det.push_str(&format!("\nAttached Apple DeviceTree ({} KB) in boot_args.", dt.len() / 1024));
                            }
                            if !r.mmio_regions.is_empty() {
                                let regs: Vec<String> = r.mmio_regions.iter().take(6).map(|a| format!("{a:#x}")).collect();
                                det.push_str(&format!("\nApple SoC MMIO probed: {}.", regs.join(", ")));
                            }
                            if !r.uart_output.is_empty() {
                                det.push_str(&format!("\nUART serial output:\n{}", r.uart_output));
                            }
                            probe.detail = det;
                        }
                        Err(e) => {
                            probe.detail = format!(
                                "Unpacked {} XNU Mach-O kernel ({} segments, entry {:#018X}, {:.1} MB); bring-up note: {e}",
                                sl.arch,
                                sl.segments.len(),
                                sl.entry,
                                payload.len() as f64 / (1024.0 * 1024.0),
                            );
                        }
                    }
                } else {
                    probe.detail = format!(
                        "Unpacked {} XNU Mach-O kernel ({} segments, entry {:#018X}, {:.1} MB).",
                        sl.arch,
                        sl.segments.len(),
                        sl.entry,
                        payload.len() as f64 / (1024.0 * 1024.0),
                    );
                }
            }
        }
        Err(_) => {
            if probe.container == "IMG3" || probe.container == "8900" {
                probe.detail = format!("{} container is encrypted with Apple device-specific firmware keys.", probe.container);
            } else {
                probe.detail = format!("Container {} ({} bytes) is not a plaintext Mach-O kernel.", probe.container, payload.len());
            }
        }
    }
    probe
}

fn sniff_container(head: &[u8]) -> &'static str {
    if head.len() >= 4 {
        if &head[..4] == b"3gmI" || &head[..4] == b"Img3" {
            return "IMG3";
        }
        if &head[..4] == b"8900" {
            return "8900";
        }
        let le = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
        if le == 0xFEED_FACE || le == 0xFEED_FACF || le == 0xBEBA_FECA {
            return "Mach-O (raw)";
        }
        if head.windows(4).any(|w| w == b"IM4P" || w == b"IMG4") {
            return "IMG4/IM4P";
        }
    }
    "binary"
}

/// Streams an `.ipsw` archive from disk and summarizes its firmware metadata and images.
pub fn read_ipsw_file(path: &Path) -> Result<IpswInfo, String> {
    let f = File::open(path).map_err(|e| format!("Couldn't open {}: {e}", path.display()))?;
    read_ipsw_reader(f)
}

/// Parses an `.ipsw` archive from any `Read + Seek` stream.
pub fn read_ipsw_reader<R: Read + Seek>(reader: R) -> Result<IpswInfo, String> {
    let mut z = zip::ZipArchive::new(reader).map_err(|_| "This isn't a valid .ipsw firmware archive (not a zip file).".to_string())?;
    let mut info = IpswInfo { total_files: z.len(), ..Default::default() };

    let mut manifest_name: Option<String> = None;
    let mut kernel_names: Vec<String> = Vec::new();
    let mut dt_name: Option<String> = None;
    let mut largest_dmg: Option<(String, u64)> = None;

    for i in 0..z.len() {
        let Ok(entry) = z.by_index(i) else { continue };
        let name = entry.name().to_string();
        let lower = name.to_ascii_lowercase();
        if lower == "restore.plist" {
            manifest_name = Some(name);
        } else if lower == "buildmanifest.plist" && manifest_name.is_none() {
            manifest_name = Some(name);
        } else if lower.contains("kernelcache") && !entry.is_dir() {
            kernel_names.push(name);
        } else if lower.contains("devicetree") && lower.ends_with(".im4p") && dt_name.is_none() && !entry.is_dir() {
            dt_name = Some(name);
        } else if (lower.contains("iboot") || lower.contains("llb") || lower.contains("ibss") || lower.contains("ibec")) && !entry.is_dir() {
            info.bootloaders.push(name);
        } else if lower.ends_with(".dmg") && !entry.is_dir() {
            let sz = entry.size();
            if largest_dmg.as_ref().is_none_or(|(_, best)| sz > *best) {
                largest_dmg = Some((name, sz));
            }
        }
    }
    info.root_dmg = largest_dmg;

    if let Some(mname) = manifest_name {
        if let Ok(mut f) = z.by_name(&mname) {
            let mut raw = Vec::new();
            if f.read_to_end(&mut raw).is_ok() {
                if let Ok(val) = plist::parse(&raw) {
                    if let Some(v) = val.get("ProductVersion").and_then(Value::as_str) {
                        info.product_version = v.to_string();
                    }
                    if let Some(v) = val.get("ProductBuildVersion").and_then(Value::as_str) {
                        info.product_build = v.to_string();
                    }
                    if let Some(arr) = val.get("SupportedProductTypes").and_then(Value::as_array) {
                        info.supported_products = arr.iter().filter_map(Value::as_str).map(String::from).collect();
                    } else if let Some(s) = val.get("ProductType").and_then(Value::as_str) {
                        info.supported_products.push(s.to_string());
                    }
                }
            }
        }
    }

    // Unwrap Apple DeviceTree (IM4P `dtre`), decompress LZFSE/complzss, and clear iBoot template bits.
    let device_tree: Option<Vec<u8>> = dt_name.and_then(|dname| {
        let mut f = z.by_name(&dname).ok()?;
        let mut raw = Vec::new();
        f.read_to_end(&mut raw).ok()?;
        let payload = crate::im4p::parse_im4p(&raw).ok()?.payload;
        let mut dt = if payload.starts_with(b"bvx2") || payload.starts_with(b"bvx1") || payload.starts_with(b"bvxn") || payload.starts_with(b"bvx-") {
            crate::im4p::decompress_lzfse(&payload).ok()?
        } else if payload.starts_with(b"complzss") {
            crate::im4p::decompress_complzss(&payload).ok()?
        } else {
            payload
        };
        sanitize_apple_devicetree(&mut dt);
        Some(dt)
    });

    for (idx, kname) in kernel_names.into_iter().take(8).enumerate() {
        let mut head = [0u8; 64];
        let fmt = if let Ok(mut f) = z.by_name(&kname) {
            let sz = f.size();
            if idx == 0 && sz <= 96 * 1024 * 1024 {
                let mut full = Vec::with_capacity(sz as usize);
                if f.read_to_end(&mut full).is_ok() {
                    let probe = probe_kernel_bytes(&kname, &full, device_tree.as_deref());
                    let c = probe.container.clone();
                    info.kernel_probe = Some(probe);
                    info.kernelcaches.push((kname, c));
                    continue;
                }
            }
            let n = f.read(&mut head).unwrap_or(0);
            sniff_container(&head[..n]).to_string()
        } else {
            "unknown".to_string()
        };
        info.kernelcaches.push((kname, fmt));
    }

    if info.product_version.is_empty() && info.kernelcaches.is_empty() && info.root_dmg.is_none() {
        return Err("This zip archive doesn't look like an Apple iOS .ipsw firmware (no Restore.plist, BuildManifest.plist, or kernelcache found).".into());
    }
    Ok(info)
}

/// Unpacked XNU kernel Mach-O + sanitized Apple DeviceTree extracted from an `.ipsw` archive.
pub struct BootImages {
    pub title: String,
    pub kernel: Vec<u8>,
    pub device_tree: Option<Vec<u8>>,
    pub banner: Option<String>,
}

/// Extracts the decrypted/decompressed arm64/arm64e XNU kernelcache and sanitized DeviceTree
/// from an `.ipsw` archive so the live iOS runner can boot XNU incrementally.
pub fn extract_boot_images(path: &Path) -> Result<BootImages, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("Couldn't open {}: {e}", path.display()))?;
    let mut z = zip::ZipArchive::new(f).map_err(|e| format!("Not a valid .ipsw archive: {e}"))?;

    let mut manifest_name: Option<String> = None;
    let mut kernel_name: Option<String> = None;
    let mut dt_name: Option<String> = None;

    for i in 0..z.len() {
        let Ok(entry) = z.by_index(i) else { continue };
        let name = entry.name().to_string();
        let lower = name.to_ascii_lowercase();
        if lower == "restore.plist" {
            manifest_name = Some(name);
        } else if lower == "buildmanifest.plist" && manifest_name.is_none() {
            manifest_name = Some(name);
        } else if lower.contains("kernelcache") && kernel_name.is_none() && !entry.is_dir() {
            kernel_name = Some(name);
        } else if lower.contains("devicetree") && lower.ends_with(".im4p") && dt_name.is_none() && !entry.is_dir() {
            dt_name = Some(name);
        }
    }

    let mut title = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    if let Some(mname) = manifest_name {
        if let Ok(mut f) = z.by_name(&mname) {
            let mut raw = Vec::new();
            if f.read_to_end(&mut raw).is_ok() {
                if let Ok(val) = plist::parse(&raw) {
                    let ver = val.get("ProductVersion").and_then(Value::as_str).unwrap_or("");
                    let build = val.get("ProductBuildVersion").and_then(Value::as_str).unwrap_or("");
                    if !ver.is_empty() {
                        title = if build.is_empty() {
                            format!("iOS {ver}")
                        } else {
                            format!("iOS {ver} ({build})")
                        };
                    }
                }
            }
        }
    }

    let device_tree = dt_name.and_then(|dname| {
        let mut f = z.by_name(&dname).ok()?;
        let mut raw = Vec::new();
        f.read_to_end(&mut raw).ok()?;
        let payload = crate::im4p::parse_im4p(&raw).ok()?.payload;
        let mut dt = if payload.starts_with(b"bvx2") || payload.starts_with(b"bvx1") || payload.starts_with(b"bvxn") || payload.starts_with(b"bvx-") {
            crate::im4p::decompress_lzfse(&payload).ok()?
        } else if payload.starts_with(b"complzss") {
            crate::im4p::decompress_complzss(&payload).ok()?
        } else {
            payload
        };
        sanitize_apple_devicetree(&mut dt);
        Some(dt)
    });

    let kname = kernel_name.ok_or("No kernelcache found inside this .ipsw archive.")?;
    let mut kf = z.by_name(&kname).map_err(|e| format!("Couldn't open {kname}: {e}"))?;
    let mut raw = Vec::new();
    kf.read_to_end(&mut raw).map_err(|e| format!("Couldn't read {kname}: {e}"))?;

    let kernel = if let Ok(im4p) = crate::im4p::parse_im4p(&raw) {
        if im4p.payload.starts_with(b"complzss") {
            crate::im4p::decompress_complzss(&im4p.payload)?
        } else if im4p.payload.starts_with(b"bvx2")
            || im4p.payload.starts_with(b"bvx1")
            || im4p.payload.starts_with(b"bvxn")
            || im4p.payload.starts_with(b"bvx-")
        {
            crate::im4p::decompress_lzfse(&im4p.payload)?
        } else {
            im4p.payload
        }
    } else if raw.starts_with(b"complzss") {
        crate::im4p::decompress_complzss(&raw)?
    } else {
        raw
    };

    let banner = crate::im4p::extract_xnu_banner(&kernel);
    Ok(BootImages {
        title,
        kernel,
        device_tree,
        banner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_synthetic_ipsw_archive() {
        let restore = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>ProductVersion</key><string>3.1.3</string>
<key>ProductBuildVersion</key><string>7E18</string>
<key>SupportedProductTypes</key><array><string>iPhone1,2</string></array>
</dict></plist>"#;
        let mut buf = Vec::new();
        {
            let mut z = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let o: zip::write::SimpleFileOptions = Default::default();
            z.start_file("Restore.plist", o).unwrap();
            z.write_all(restore).unwrap();
            z.start_file("kernelcache.release.s5l8900x", o).unwrap();
            z.write_all(b"3gmI\x00\x00\x00\x00").unwrap();
            z.start_file("Firmware/all_flash/iBoot.n82ap.RELEASE.img3", o).unwrap();
            z.write_all(b"3gmI").unwrap();
            z.start_file("018-6482-014.dmg", o).unwrap();
            z.write_all(&[0u8; 2048]).unwrap();
            z.finish().unwrap();
        }
        let info = read_ipsw_reader(std::io::Cursor::new(buf)).expect("valid ipsw");
        assert_eq!(info.product_version, "3.1.3");
        assert_eq!(info.product_build, "7E18");
        assert_eq!(info.supported_products, vec!["iPhone1,2"]);
        assert_eq!(info.kernelcaches, vec![("kernelcache.release.s5l8900x".into(), "IMG3".into())]);
        assert_eq!(info.bootloaders.len(), 1);
        assert_eq!(info.root_dmg, Some(("018-6482-014.dmg".into(), 2048)));
        assert!(info.summary().contains("iOS 3.1.3"));
    }

    #[test]
    fn sanitizes_apple_devicetree_and_populates_chosen() {
        // Root node (1 prop, 2 children: cpu0 and chosen)
        let mut dt = Vec::new();
        dt.extend_from_slice(&1u32.to_le_bytes()); // root n_props
        dt.extend_from_slice(&2u32.to_le_bytes()); // root n_children
        dt.extend_from_slice(&encode_adt_prop("name", b"device-tree\0"));

        // Child 1: cpu0 with template bit 31 set on `state`
        dt.extend_from_slice(&2u32.to_le_bytes());
        dt.extend_from_slice(&0u32.to_le_bytes());
        dt.extend_from_slice(&encode_adt_prop("name", b"cpu0\0"));
        let mut state_prop = encode_adt_prop("state", &[0u8; 8]);
        let raw_len = 8u32 | 0x8000_0000;
        state_prop[32..36].copy_from_slice(&raw_len.to_le_bytes());
        dt.extend_from_slice(&state_prop);

        // Child 2: chosen with zeroed random-seed, missing dram-base/dram-size/debug-enabled,
        // and a nested lock-regs -> amcc -> amcc-ctrr-a hierarchy.
        dt.extend_from_slice(&2u32.to_le_bytes());
        dt.extend_from_slice(&1u32.to_le_bytes()); // 1 child: amcc
        dt.extend_from_slice(&encode_adt_prop("name", b"chosen\0"));
        dt.extend_from_slice(&encode_adt_prop("random-seed", &[0u8; 16]));

        dt.extend_from_slice(&1u32.to_le_bytes());
        dt.extend_from_slice(&1u32.to_le_bytes()); // 1 child: amcc-ctrr-a
        dt.extend_from_slice(&encode_adt_prop("name", b"amcc\0"));

        dt.extend_from_slice(&1u32.to_le_bytes());
        dt.extend_from_slice(&0u32.to_le_bytes());
        dt.extend_from_slice(&encode_adt_prop("name", b"amcc-ctrr-a\0"));

        sanitize_apple_devicetree(&mut dt);

        assert!(dt.windows(8).any(|w| w == b"running\0"), "cpu0/state populated");
        assert!(dt.windows(9).any(|w| w == b"dram-base"), "chosen/dram-base inserted");
        assert!(dt.windows(9).any(|w| w == b"dram-size"), "chosen/dram-size inserted");
        assert!(dt.windows(13).any(|w| w == b"debug-enabled"), "chosen/debug-enabled inserted");
        assert!(dt.windows(14).any(|w| w == b"aperture-count"), "amcc/aperture-count inserted");
        assert!(dt.windows(11).any(|w| w == b"plane-count"), "amcc/plane-count inserted");
        assert!(dt.windows(15).any(|w| w == b"page-size-shift"), "amcc-ctrr-a/page-size-shift inserted");
    }
}
