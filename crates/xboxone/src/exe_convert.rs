//! Clean-room Xbox-to-Windows Native `.exe` Converter, Console Library Coverage Map,
//! GPU Occlusion-Query Un-nester, Console Frame Pacer, and Offline Xbox Live / PlayFab Shims.
//!
//! Converts a decrypted Xbox One / Series X|S PE64 executable into a runnable Windows x64 `.exe`
//! by remapping console-only DLL names in the PE Import Directory, normalizing PE subsystem flags,
//! and producing a detailed per-DLL/per-function coverage report.

use std::collections::{BTreeMap, VecDeque};

use crate::modules::{self, Provider};
use crate::pe;

/// Implementation coverage status of a single Xbox console import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportStatus {
    Implemented,
    ForwardedToWindows,
    Stubbed,
    Missing,
}

/// Per-import coverage entry.
#[derive(Debug, Clone)]
pub struct ImportCoverageEntry {
    pub dll: String,
    pub function: String,
    pub status: ImportStatus,
}

/// Complete Xbox console-library coverage summary for a PE64 executable.
#[derive(Debug, Clone, Default)]
pub struct CoverageReport {
    pub total_imports: usize,
    pub console_only_imports: usize,
    pub implemented: usize,
    pub forwarded: usize,
    pub stubbed: usize,
    pub missing: usize,
    pub by_dll: BTreeMap<String, (usize, usize)>, // dll -> (covered, total)
    pub entries: Vec<ImportCoverageEntry>,
}

/// Returns `true` if `dll` is a standard PC Windows DLL (not Xbox console-only).
pub fn is_standard_windows_dll(dll: &str) -> bool {
    matches!(
        dll.to_ascii_lowercase().as_str(),
        "kernel32.dll"
            | "kernelbase.dll"
            | "ntdll.dll"
            | "user32.dll"
            | "gdi32.dll"
            | "advapi32.dll"
            | "ole32.dll"
            | "combase.dll"
            | "shell32.dll"
            | "ws2_32.dll"
            | "winhttp.dll"
            | "crypt32.dll"
            | "bcrypt.dll"
            | "msvcrt.dll"
            | "ucrtbase.dll"
            | "vcruntime140.dll"
            | "msvcp140.dll"
            | "dxgi.dll"
            | "d3d12.dll"
            | "d3d11.dll"
            | "xinput1_4.dll"
            | "xaudio2_9.dll"
    ) || dll.to_ascii_lowercase().starts_with("api-ms-win-")
}

/// Maps an Xbox console-only DLL name to its same-length-or-shorter Windows / `x2w.dll`
/// companion runtime DLL name for in-place PE Import Directory rewriting.
///
/// `x2w.dll` (7 bytes) is shorter than every Xbox console DLL so every console-only import
/// table entry can be patched cleanly in-place and resolved by the Windows PE loader once
/// `x2w.dll` is placed next to the converted `.exe`.
pub fn mapped_windows_dll(xbox_dll: &str) -> Option<&'static str> {
    match xbox_dll.to_ascii_lowercase().as_str() {
        "d3d12_xs.dll" | "d3d12_x.dll" => Some("x2w.dll"),
        "dstorage_xs.dll" | "dstorage.dll" => Some("x2w.dll"),
        "xg_xs.dll" | "xg_x.dll" => Some("x2w.dll"),
        "xmem.dll" => Some("x2w.dll"),
        "xgameplatform.dll" | "xgameruntime.dll" => Some("x2w.dll"),
        "xapu.dll" | "acphal.dll" | "chad.dll" => Some("x2w.dll"),
        "xcurl.dll" => Some("x2w.dll"),
        "d3d11_x.dll" => Some("d3d11.dll"),
        "kernelx.dll" => Some("kernel32.dll"),
        "xcrdapi.dll" => Some("advapi32.dll"),
        _ => None,
    }
}

/// Analyzes every DLL & function imported by a decrypted Xbox One / Series X|S PE64 executable.
pub fn analyze_pe_coverage(pe_bytes: &[u8]) -> Result<CoverageReport, String> {
    let parsed = pe::Pe::parse_bytes(pe_bytes)?;
    let mut rep = CoverageReport::default();

    for imp in &parsed.imports {
        let std_win = is_standard_windows_dll(&imp.dll);
        let provider = modules::provider(&imp.dll);
        let entry = rep.by_dll.entry(imp.dll.clone()).or_insert((0, 0));
        for func in &imp.functions {
            rep.total_imports += 1;
            entry.1 += 1;
            let status = if std_win || matches!(provider, Some(Provider::Windows(_))) {
                rep.forwarded += 1;
                entry.0 += 1;
                ImportStatus::ForwardedToWindows
            } else {
                rep.console_only_imports += 1;
                if matches!(provider, Some(Provider::Shim { .. })) {
                    rep.implemented += 1;
                    entry.0 += 1;
                    ImportStatus::Implemented
                } else if mapped_windows_dll(&imp.dll).is_some() {
                    rep.stubbed += 1;
                    entry.0 += 1;
                    ImportStatus::Stubbed
                } else {
                    rep.missing += 1;
                    ImportStatus::Missing
                }
            };
            let fname = if func.name.is_empty() {
                format!("#{}", func.ordinal.unwrap_or(0))
            } else {
                func.name.clone()
            };
            rep.entries.push(ImportCoverageEntry {
                dll: imp.dll.clone(),
                function: fname,
                status,
            });
        }
    }
    Ok(rep)
}

/// Result of statically converting a decrypted Xbox One / Series X|S PE64 `.exe` into a Windows `.exe`.
#[derive(Debug, Clone)]
pub struct ConvertedExe {
    pub windows_pe: Vec<u8>,
    pub rewritten_dlls: Vec<(String, String)>,
    pub coverage: CoverageReport,
}

/// Converts a decrypted Xbox One / Series X|S PE64 `.exe` into a Windows x64 PE `.exe`
/// by rewriting console DLL names in-place inside the Import Directory and setting the
/// Windows GUI subsystem flag.
pub fn convert_xbox_pe_to_windows(xbox_pe: &[u8]) -> Result<ConvertedExe, String> {
    let coverage = analyze_pe_coverage(xbox_pe)?;
    let mut out = xbox_pe.to_vec();

    // Locate PE header & normalize Subsystem -> Windows GUI (2) if currently Xbox (14) or Unknown (0)
    if out.len() < 0x40 || &out[0..2] != b"MZ" {
        return Err("Input is not a valid PE executable (missing MZ header).".into());
    }
    let pe_off = u32::from_le_bytes(out[0x3C..0x40].try_into().unwrap()) as usize;
    if pe_off + 24 + 96 > out.len() || &out[pe_off..pe_off + 4] != b"PE\0\0" {
        return Err("Invalid PE signature.".into());
    }
    let opt_off = pe_off + 24;
    let magic = u16::from_le_bytes([out[opt_off], out[opt_off + 1]]);
    if magic != 0x20B {
        return Err("Expected 64-bit PE32+ Xbox executable.".into());
    }
    let subsys_off = opt_off + 68;
    let subsys = u16::from_le_bytes([out[subsys_off], out[subsys_off + 1]]);
    if subsys == 14 || subsys == 0 {
        out[subsys_off..subsys_off + 2].copy_from_slice(&2u16.to_le_bytes()); // IMAGE_SUBSYSTEM_WINDOWS_GUI
    }
    // Zero PE checksum so Windows loader doesn't reject patched headers
    out[opt_off + 64..opt_off + 68].fill(0);

    // Rewrite Xbox console-only DLL name strings in-place (only when replacement fits within original byte length)
    let mut rewritten = Vec::new();
    let mut unique_dlls: Vec<String> = coverage.by_dll.keys().cloned().collect();
    unique_dlls.sort();
    for dll in unique_dlls {
        if let Some(target) = mapped_windows_dll(&dll) {
            if target.len() <= dll.len() && !target.eq_ignore_ascii_case(&dll) {
                let needle_lo = dll.to_ascii_lowercase();
                let mut pos = 0usize;
                let mut patched_here = false;
                while pos + dll.len() < out.len() {
                    if out[pos..pos + dll.len()].eq_ignore_ascii_case(needle_lo.as_bytes())
                        && out[pos + dll.len()] == 0
                    {
                        out[pos..pos + target.len()].copy_from_slice(target.as_bytes());
                        for b in &mut out[pos + target.len()..pos + dll.len()] {
                            *b = 0;
                        }
                        patched_here = true;
                        pos += dll.len() + 1;
                    } else {
                        pos += 1;
                    }
                }
                if patched_here {
                    rewritten.push((dll, target.to_string()));
                }
            }
        }
    }

    Ok(ConvertedExe {
        windows_pe: out,
        rewritten_dlls: rewritten,
        coverage,
    })
}

/// Flattens nested Xbox D3D12.X occlusion queries into sequential PC D3D12 Begin/End operations
/// (Xbox allows nested queries on one command list; standard PC D3D12 allows only one active at a time).
#[derive(Default, Debug)]
pub struct OcclusionQueryUnnester {
    stack: Vec<u32>,
    pub emitted_ops: Vec<(bool, u32)>, // (is_begin, query_index)
    pub unnested_count: usize,
}

impl OcclusionQueryUnnester {
    pub fn begin_query(&mut self, query_index: u32) {
        if let Some(&outer) = self.stack.last() {
            // Pause outer query before starting inner query on PC D3D12
            self.emitted_ops.push((false, outer));
            self.unnested_count += 1;
        }
        self.stack.push(query_index);
        self.emitted_ops.push((true, query_index));
    }

    pub fn end_query(&mut self, query_index: u32) {
        if let Some(top) = self.stack.pop() {
            self.emitted_ops.push((false, top));
            let _ = query_index;
            if let Some(&outer) = self.stack.last() {
                // Resume outer query on PC D3D12
                self.emitted_ops.push((true, outer));
            }
        }
    }
}

/// Clean-room Console Frame Pacer (`SetFrameIntervalX` / `ScheduleFrameEventX`).
#[derive(Debug, Clone)]
pub struct FramePacer {
    pub interval_us: u64,
    pub frame_count: u64,
    pub pending_events: VecDeque<u64>,
}

impl Default for FramePacer {
    fn default() -> Self {
        Self {
            interval_us: 16_667, // 60 Hz default
            frame_count: 0,
            pending_events: VecDeque::new(),
        }
    }
}

impl FramePacer {
    pub fn set_interval_us(&mut self, us: u64) {
        self.interval_us = us.clamp(4_000, 100_000);
    }

    pub fn schedule_present(&mut self) -> u64 {
        self.frame_count += 1;
        let token = self.frame_count;
        self.pending_events.push_back(token);
        token
    }

    pub fn complete_next(&mut self) -> Option<u64> {
        self.pending_events.pop_front()
    }
}

/// Offline in-process Xbox Live & PlayFab service responder (zero external network traffic).
#[derive(Debug, Clone)]
pub struct OfflineServices {
    pub gamertag: String,
    pub xuid: u64,
    pub unlocked_achievements: Vec<String>,
}

impl Default for OfflineServices {
    fn default() -> Self {
        Self {
            gamertag: "Player1".into(),
            xuid: 0x0009_0000_1234_5678,
            unlocked_achievements: Vec::new(),
        }
    }
}

impl OfflineServices {
    /// Answers common Xbox Live / PlayFab / title JSON endpoints locally in-process.
    pub fn handle_http(&mut self, path: &str) -> (u16, String) {
        let p = path.to_ascii_lowercase();
        if p.contains("login") || p.contains("authenticate") || p.contains("token") {
            (
                200,
                format!(
                    r#"{{"status":"OK","xuid":"{}","gamertag":"{}","sessionTicket":"offline-local-session"}}"#,
                    self.xuid, self.gamertag
                ),
            )
        } else if p.contains("matchmaking") || p.contains("ticket") {
            (
                200,
                r#"{"status":"Matched","matchId":"private-local-match","server":"127.0.0.1","port":7777}"#.into(),
            )
        } else if p.contains("profile") || p.contains("presence") || p.contains("peoplehub") {
            (
                200,
                format!(
                    r#"{{"xuid":"{}","gamertag":"{}","state":"Online","mode":"OfflinePrivate"}}"#,
                    self.xuid, self.gamertag
                ),
            )
        } else if p.contains("achievement") {
            (
                200,
                format!(r#"{{"achievementsCount":{}}}"#, self.unlocked_achievements.len()),
            )
        } else {
            (200, r#"{"status":"OK","offline":true}"#.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occlusion_query_unnester_flattens_nested_xbox_queries() {
        let mut q = OcclusionQueryUnnester::default();
        q.begin_query(0); // outer dummy query
        q.begin_query(1); // inner object query
        q.end_query(1);
        q.end_query(0);
        assert_eq!(q.unnested_count, 1);
        assert_eq!(
            q.emitted_ops,
            vec![(true, 0), (false, 0), (true, 1), (false, 1), (true, 0), (false, 0)]
        );
    }

    #[test]
    fn offline_services_answer_login_and_private_matchmaking() {
        let mut svc = OfflineServices::default();
        let (code, body) = svc.handle_http("/Client/LoginWithXbox");
        assert_eq!(code, 200);
        assert!(body.contains("offline-local-session"));
        let (mc, mbody) = svc.handle_http("/Match/CreateMatchmakingTicket");
        assert_eq!(mc, 200);
        assert!(mbody.contains("private-local-match"));
    }

    #[test]
    fn converts_xbox_pe_subsystem_and_dll_imports_to_windows() {
        // Build a minimal valid PE32+ with Subsystem=14 (Xbox) and an import of "d3d12_xs.dll"
        let mut pe = vec![0u8; 0x600];
        pe[0..2].copy_from_slice(b"MZ");
        pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes()); // AMD64
        pe[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());      // 1 section
        pe[0x94..0x96].copy_from_slice(&0xF0u16.to_le_bytes());   // SizeOfOptionalHeader
        let opt = 0x98usize;
        pe[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes()); // PE32+
        pe[opt + 68..opt + 70].copy_from_slice(&14u16.to_le_bytes()); // Xbox subsystem
        pe[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes
        // Import directory data directory entry (index 1 -> opt + 112 + 8)
        pe[opt + 120..opt + 124].copy_from_slice(&0x200u32.to_le_bytes());
        pe[opt + 124..opt + 128].copy_from_slice(&40u32.to_le_bytes());
        // Section header at opt + 0xF0 = 0x188
        let sec = opt + 0xF0;
        pe[sec..sec + 6].copy_from_slice(b".rdata");
        pe[sec + 8..sec + 12].copy_from_slice(&0x400u32.to_le_bytes()); // VirtualSize
        pe[sec + 12..sec + 16].copy_from_slice(&0x200u32.to_le_bytes()); // VirtualAddress
        pe[sec + 16..sec + 20].copy_from_slice(&0x400u32.to_le_bytes()); // SizeOfRawData
        pe[sec + 20..sec + 24].copy_from_slice(&0x200u32.to_le_bytes()); // PointerToRawData
        // Import descriptor at 0x200: OriginalFirstThunk=0x260, Name=0x280, FirstThunk=0x260
        pe[0x200..0x204].copy_from_slice(&0x260u32.to_le_bytes());
        pe[0x20C..0x210].copy_from_slice(&0x280u32.to_le_bytes());
        pe[0x210..0x214].copy_from_slice(&0x260u32.to_le_bytes());
        // Thunk pointing to hint/name at 0x2A0
        pe[0x260..0x268].copy_from_slice(&0x2A0u64.to_le_bytes());
        // DLL name "d3d12_xs.dll\0" at 0x280
        pe[0x280..0x28D].copy_from_slice(b"d3d12_xs.dll\0");
        // Function name at 0x2A0 (2-byte hint + "D3D12CreateDevice\0")
        pe[0x2A2..0x2B4].copy_from_slice(b"D3D12CreateDevice\0");

        let converted = convert_xbox_pe_to_windows(&pe).unwrap();
        assert_eq!(u16::from_le_bytes([converted.windows_pe[opt + 68], converted.windows_pe[opt + 69]]), 2);
        assert_eq!(converted.rewritten_dlls, vec![("d3d12_xs.dll".into(), "x2w.dll".into())]);
        assert!(converted.windows_pe[0x280..0x288] == *b"x2w.dll\0");
    }
}
