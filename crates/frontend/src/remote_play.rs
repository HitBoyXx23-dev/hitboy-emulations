//! PlayStation 5 / PlayStation 4 Remote Play integration:
//! - Native LAN auto-discovery via Sony's Device Discovery Protocol (DDP) over UDP 9302 (PS5) and 987 (PS4)
//! - Wake-from-Standby (Rest Mode) via DDP WAKEUP packets
//! - Saved consoles persisted in Settings
//! - One-click launcher with auto-detection for chiaki-ng, Chiaki, and Sony PS Remote Play

use std::collections::BTreeMap;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const PS5_DDP_PORT: u16 = 9302;
pub const PS4_DDP_PORT: u16 = 987;
pub const PS5_DDP_VERSION: &str = "00030010";
pub const PS4_DDP_VERSION: &str = "00020020";

pub const SONY_REMOTE_PLAY_URL: &str = "https://remoteplay.dl.playstation.net/remoteplay/lang/en/";
pub const CHIAKI_NG_URL: &str = "https://github.com/streetpea/chiaki-ng/releases";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ConsoleModel {
    #[default]
    Ps5,
    Ps4,
}

impl ConsoleModel {
    pub fn label(self) -> &'static str {
        match self {
            ConsoleModel::Ps5 => "PS5",
            ConsoleModel::Ps4 => "PS4",
        }
    }

    pub fn ddp_port(self) -> u16 {
        match self {
            ConsoleModel::Ps5 => PS5_DDP_PORT,
            ConsoleModel::Ps4 => PS4_DDP_PORT,
        }
    }

    pub fn ddp_version(self) -> &'static str {
        match self {
            ConsoleModel::Ps5 => PS5_DDP_VERSION,
            ConsoleModel::Ps4 => PS4_DDP_VERSION,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsolePower {
    Ready,
    Standby,
    Unknown(u16),
}

impl ConsolePower {
    pub fn label(self) -> String {
        match self {
            ConsolePower::Ready => "Online (Ready)".into(),
            ConsolePower::Standby => "Rest Mode (Standby)".into(),
            ConsolePower::Unknown(code) => format!("Status {code}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredConsole {
    pub host_ip: String,
    pub host_name: String,
    pub host_id: String,
    pub model: ConsoleModel,
    pub system_version: String,
    pub running_app: Option<String>,
    pub running_app_id: Option<String>,
    pub power: ConsolePower,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SavedRemoteConsole {
    pub name: String,
    pub host: String,
    pub model: ConsoleModel,
    pub host_id: String,
    /// Optional 8-character hex registration key used for Wake-from-Standby.
    pub regist_key: String,
    /// Optional PSN Account ID (base64) for direct chiaki-ng streaming.
    pub psn_account_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientKind {
    ChiakiNg,
    Chiaki,
    SonyRemotePlay,
    Custom,
}

#[derive(Clone, Debug)]
pub struct DetectedClient {
    pub kind: ClientKind,
    pub name: String,
    pub path: PathBuf,
}

/// Builds a Sony DDP discovery search packet for PS5 or PS4.
pub fn ddp_search_packet(model: ConsoleModel) -> String {
    format!(
        "SRCH * HTTP/1.1\ndevice-discovery-protocol-version:{}\n",
        model.ddp_version()
    )
}

/// Builds a Sony DDP wakeup packet to wake a PS5 or PS4 from Rest Mode.
pub fn ddp_wakeup_packet(model: ConsoleModel, regist_key: &str) -> String {
    let cred = regist_key.trim();
    if cred.is_empty() {
        format!(
            "WAKEUP * HTTP/1.1\nclient-type:vr\nauth-type:C\nmodel:w\napp-type:r\ndevice-discovery-protocol-version:{}\n",
            model.ddp_version()
        )
    } else {
        format!(
            "WAKEUP * HTTP/1.1\nclient-type:vr\nauth-type:R\nmodel:w\napp-type:r\nuser-credential:{cred}\ndevice-discovery-protocol-version:{}\n",
            model.ddp_version()
        )
    }
}

/// Parses a Sony DDP HTTP-like UDP response into a `DiscoveredConsole`.
pub fn parse_ddp_response(raw: &str, ip: &str) -> Option<DiscoveredConsole> {
    let mut lines = raw.lines().map(str::trim).filter(|l| !l.is_empty());
    let status_line = lines.next()?;
    if !status_line.starts_with("HTTP/1.1") {
        return None;
    }
    let code: u16 = status_line.split_whitespace().nth(1)?.parse().ok()?;
    let power = match code {
        200 => ConsolePower::Ready,
        620 => ConsolePower::Standby,
        other => ConsolePower::Unknown(other),
    };

    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let host_type = headers.get("host-type").map(String::as_str).unwrap_or("");
    let model = if host_type.eq_ignore_ascii_case("PS4") {
        ConsoleModel::Ps4
    } else {
        ConsoleModel::Ps5
    };
    let host_name = headers
        .get("host-name")
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("PlayStation ({ip})"));
    let host_id = headers.get("host-id").cloned().unwrap_or_default();
    let system_version = headers.get("system-version").cloned().unwrap_or_default();
    let running_app = headers.get("running-app-name").cloned().filter(|s| !s.is_empty());
    let running_app_id = headers.get("running-app-titleid").cloned().filter(|s| !s.is_empty());

    Some(DiscoveredConsole {
        host_ip: ip.to_string(),
        host_name,
        host_id,
        model,
        system_version,
        running_app,
        running_app_id,
        power,
    })
}

/// Broadcasts Sony DDP search packets on the LAN to discover PS5 and PS4 consoles.
pub fn discover_consoles(timeout: Duration) -> Result<Vec<DiscoveredConsole>, String> {
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("Couldn't open UDP discovery socket: {e}"))?;
    sock.set_broadcast(true).map_err(|e| format!("Couldn't enable UDP broadcast: {e}"))?;
    sock.set_read_timeout(Some(Duration::from_millis(120)))
        .map_err(|e| format!("Couldn't configure socket timeout: {e}"))?;

    let ps5_pkt = ddp_search_packet(ConsoleModel::Ps5);
    let ps4_pkt = ddp_search_packet(ConsoleModel::Ps4);
    let ps5_addr: SocketAddr = ([255, 255, 255, 255], PS5_DDP_PORT).into();
    let ps4_addr: SocketAddr = ([255, 255, 255, 255], PS4_DDP_PORT).into();

    let _ = sock.send_to(ps5_pkt.as_bytes(), ps5_addr);
    let _ = sock.send_to(ps4_pkt.as_bytes(), ps4_addr);

    let start = Instant::now();
    let mut resent = false;
    let mut found: BTreeMap<String, DiscoveredConsole> = BTreeMap::new();
    let mut buf = [0u8; 4096];

    while start.elapsed() < timeout {
        if !resent && start.elapsed() >= Duration::from_millis(300) {
            let _ = sock.send_to(ps5_pkt.as_bytes(), ps5_addr);
            let _ = sock.send_to(ps4_pkt.as_bytes(), ps4_addr);
            resent = true;
        }
        match sock.recv_from(&mut buf) {
            Ok((n, src)) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                let ip = src.ip().to_string();
                if let Some(console) = parse_ddp_response(&text, &ip) {
                    let key = if console.host_id.is_empty() {
                        console.host_ip.clone()
                    } else {
                        console.host_id.clone()
                    };
                    found.insert(key, console);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }

    let mut list: Vec<DiscoveredConsole> = found.into_values().collect();
    list.sort_by(|a, b| a.host_name.to_lowercase().cmp(&b.host_name.to_lowercase()));
    Ok(list)
}

/// Sends a DDP WAKEUP packet to wake a PS5/PS4 console from Rest Mode.
pub fn wake_console(host: &str, model: ConsoleModel, regist_key: &str) -> Result<(), String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("Enter the console's IP address first.".into());
    }
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("Couldn't open UDP socket: {e}"))?;
    let target = format!("{host}:{}", model.ddp_port());
    let pkt = ddp_wakeup_packet(model, regist_key);
    sock.send_to(pkt.as_bytes(), &target)
        .map_err(|e| format!("Couldn't send wakeup packet to {target}: {e}"))?;
    Ok(())
}

/// Searches standard install paths and PATH for installed Remote Play clients.
pub fn detect_clients(custom_path: &str) -> Vec<DetectedClient> {
    let mut clients = Vec::new();
    let custom = custom_path.trim();
    if !custom.is_empty() {
        let p = PathBuf::from(custom);
        if p.exists() {
            clients.push(DetectedClient {
                kind: ClientKind::Custom,
                name: format!("Custom ({})", p.file_name().unwrap_or_default().to_string_lossy()),
                path: p,
            });
        }
    }

    let mut add_if_exists = |kind: ClientKind, name: &str, path: PathBuf| {
        if path.exists() && !clients.iter().any(|c| c.path == path) {
            clients.push(DetectedClient { kind, name: name.to_string(), path });
        }
    };

    if cfg!(windows) {
        let local_app = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
        let pf = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        let pf86 = std::env::var_os("ProgramFiles(x86)").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files (x86)"));

        for p in [
            local_app.join("Programs").join("chiaki-ng").join("chiaki-ng.exe"),
            pf.join("chiaki-ng").join("chiaki-ng.exe"),
            pf86.join("chiaki-ng").join("chiaki-ng.exe"),
        ] {
            add_if_exists(ClientKind::ChiakiNg, "chiaki-ng", p);
        }
        for p in [
            pf.join("Chiaki").join("chiaki.exe"),
            pf86.join("Chiaki").join("chiaki.exe"),
        ] {
            add_if_exists(ClientKind::Chiaki, "Chiaki", p);
        }
        for p in [
            pf86.join("Sony").join("PS Remote Play").join("RemotePlay.exe"),
            pf.join("Sony").join("PS Remote Play").join("RemotePlay.exe"),
        ] {
            add_if_exists(ClientKind::SonyRemotePlay, "Sony PS Remote Play", p);
        }
    } else if cfg!(target_os = "macos") {
        add_if_exists(ClientKind::ChiakiNg, "chiaki-ng", PathBuf::from("/Applications/chiaki-ng.app"));
        add_if_exists(ClientKind::Chiaki, "Chiaki", PathBuf::from("/Applications/Chiaki.app"));
        add_if_exists(ClientKind::SonyRemotePlay, "Sony PS Remote Play", PathBuf::from("/Applications/RemotePlay.app"));
    } else {
        for bin in ["chiaki-ng", "chiaki"] {
            if let Some(p) = find_in_path(bin) {
                let (kind, name) = if bin == "chiaki-ng" {
                    (ClientKind::ChiakiNg, "chiaki-ng")
                } else {
                    (ClientKind::Chiaki, "Chiaki")
                };
                add_if_exists(kind, name, p);
            }
        }
    }

    // Also check PATH on Windows for chiaki-ng / chiaki.
    if cfg!(windows) {
        for (bin, kind, name) in [
            ("chiaki-ng.exe", ClientKind::ChiakiNg, "chiaki-ng"),
            ("chiaki.exe", ClientKind::Chiaki, "Chiaki"),
        ] {
            if let Some(p) = find_in_path(bin) {
                add_if_exists(kind, name, p);
            }
        }
    }

    clients
}

fn find_in_path(exe: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let cand = dir.join(exe);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Launches the chosen Remote Play client, passing host/console arguments when supported.
pub fn launch_client(client: &DetectedClient, console: Option<&SavedRemoteConsole>) -> Result<String, String> {
    if cfg!(target_os = "macos") && client.path.extension().is_some_and(|e| e == "app") {
        std::process::Command::new("open")
            .arg(&client.path)
            .spawn()
            .map_err(|e| format!("Couldn't start {}: {e}", client.name))?;
        return Ok(format!("Launched {}.", client.name));
    }

    let mut cmd = std::process::Command::new(&client.path);
    if matches!(client.kind, ClientKind::ChiakiNg | ClientKind::Chiaki) {
        if let Some(c) = console {
            let host = c.host.trim();
            if !host.is_empty() && !c.regist_key.trim().is_empty() && !c.psn_account_id.trim().is_empty() {
                cmd.arg("stream")
                    .arg(if c.model == ConsoleModel::Ps5 { "-5" } else { "-4" })
                    .arg(host)
                    .arg(c.regist_key.trim());
            }
        }
    }
    cmd.spawn()
        .map_err(|e| format!("Couldn't launch {}: {e}", client.name))?;
    match console {
        Some(c) if !c.name.is_empty() => Ok(format!("Launched {} for {} ({}).", client.name, c.name, c.host)),
        _ => Ok(format!("Launched {}.", client.name)),
    }
}

/// Opens a URL in the user's default web browser.
pub fn open_url(url: &str) {
    let _ = if cfg!(windows) {
        crate::quiet("cmd").args(["/C", "start", "", url]).spawn()
    } else if cfg!(target_os = "macos") {
        crate::quiet("open").arg(url).spawn()
    } else {
        crate::quiet("xdg-open").arg(url).spawn()
    };
}

/// Non-blocking UI state for the Remote Play tab.
pub struct RemotePlayState {
    pub scanning: bool,
    pub discovered: Vec<DiscoveredConsole>,
    pub scan_error: Option<String>,
    pub last_scan: Option<Instant>,
    rx: Option<Receiver<Result<Vec<DiscoveredConsole>, String>>>,
    pub clients: Vec<DetectedClient>,
    pub selected_client: usize,
    // Manual add form fields:
    pub add_name: String,
    pub add_host: String,
    pub add_model: ConsoleModel,
    pub add_regist_key: String,
    pub add_psn_id: String,
}

impl RemotePlayState {
    pub fn new(custom_client: &str) -> Self {
        Self {
            scanning: false,
            discovered: Vec::new(),
            scan_error: None,
            last_scan: None,
            rx: None,
            clients: detect_clients(custom_client),
            selected_client: 0,
            add_name: String::new(),
            add_host: String::new(),
            add_model: ConsoleModel::Ps5,
            add_regist_key: String::new(),
            add_psn_id: String::new(),
        }
    }

    pub fn refresh_clients(&mut self, custom_client: &str) {
        self.clients = detect_clients(custom_client);
        if self.selected_client >= self.clients.len() {
            self.selected_client = 0;
        }
    }

    pub fn start_scan(&mut self) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        self.scan_error = None;
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(discover_consoles(Duration::from_millis(1600)));
        });
    }

    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        match rx.try_recv() {
            Ok(Ok(list)) => {
                self.discovered = list;
                self.scanning = false;
                self.last_scan = Some(Instant::now());
                self.rx = None;
                true
            }
            Ok(Err(e)) => {
                self.scan_error = Some(e);
                self.scanning = false;
                self.last_scan = Some(Instant::now());
                self.rx = None;
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.scanning = false;
                self.rx = None;
                true
            }
        }
    }

    pub fn active_client(&self) -> Option<&DetectedClient> {
        self.clients.get(self.selected_client)
    }

    pub fn custom_client_label(path: &Path) -> String {
        path.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ps5_ready_ddp_response() {
        let raw = "HTTP/1.1 200 Ok\r\n\
host-id:1234567890AB\r\n\
host-type:PS5\r\n\
host-name:Living Room PS5\r\n\
host-request-port:997\r\n\
device-discovery-protocol-version:00030010\r\n\
system-version:09600001\r\n\
running-app-titleid:PPSA01494\r\n\
running-app-name:Astro's Playroom\r\n";
        let c = parse_ddp_response(raw, "192.168.1.42").expect("valid PS5 response");
        assert_eq!(c.host_ip, "192.168.1.42");
        assert_eq!(c.host_name, "Living Room PS5");
        assert_eq!(c.host_id, "1234567890AB");
        assert_eq!(c.model, ConsoleModel::Ps5);
        assert_eq!(c.power, ConsolePower::Ready);
        assert_eq!(c.running_app.as_deref(), Some("Astro's Playroom"));
        assert_eq!(c.running_app_id.as_deref(), Some("PPSA01494"));
    }

    #[test]
    fn parses_ps5_standby_response() {
        let raw = "HTTP/1.1 620 Server Standby\n\
host-id:AABBCCDDEEFF\n\
host-type:PS5\n\
host-name:Bedroom PS5\n\
system-version:10000000\n";
        let c = parse_ddp_response(raw, "10.0.0.15").expect("valid standby response");
        assert_eq!(c.power, ConsolePower::Standby);
        assert_eq!(c.model, ConsoleModel::Ps5);
        assert!(c.running_app.is_none());
    }

    #[test]
    fn parses_ps4_response() {
        let raw = "HTTP/1.1 200 Ok\nhost-id:998877665544\nhost-type:PS4\nhost-name:PS4 Pro\n";
        let c = parse_ddp_response(raw, "192.168.1.88").expect("valid PS4 response");
        assert_eq!(c.model, ConsoleModel::Ps4);
        assert_eq!(c.host_name, "PS4 Pro");
    }

    #[test]
    fn formats_ddp_packets() {
        let search = ddp_search_packet(ConsoleModel::Ps5);
        assert!(search.starts_with("SRCH * HTTP/1.1"));
        assert!(search.contains("00030010"));

        let wake = ddp_wakeup_packet(ConsoleModel::Ps5, "01234567");
        assert!(wake.starts_with("WAKEUP * HTTP/1.1"));
        assert!(wake.contains("user-credential:01234567"));
        assert!(wake.contains("00030010"));
    }
}
