//! Operating-system differences in one place: where settings and games live, how to run
//! curl, and how to show a folder.

use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// Settings and small files: %APPDATA% on Windows, ~/Library/Application Support on macOS,
/// $XDG_CONFIG_HOME (~/.config) on Linux.
pub fn config_base() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(home)
    } else if cfg!(target_os = "macos") {
        home().join("Library").join("Application Support")
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config"))
    }
}

/// Large data (games, BIOS): %LOCALAPPDATA% on Windows, ~/Library/Application Support on
/// macOS, $XDG_DATA_HOME (~/.local/share) on Linux.
pub fn data_base() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(config_base)
    } else if cfg!(target_os = "macos") {
        home().join("Library").join("Application Support")
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local").join("share"))
    }
}

/// curl ships with Windows 10+ as curl.exe and with macOS and most Linux systems as curl.
pub fn curl() -> &'static str {
    if cfg!(windows) {
        "curl.exe"
    } else {
        "curl"
    }
}

/// Opens the system file browser with `path` selected (or the folder itself).
pub fn reveal(path: &Path) {
    let r = if cfg!(windows) {
        if path.is_dir() {
            crate::quiet("explorer").arg(path).spawn()
        } else {
            crate::quiet("explorer").arg("/select,").arg(path).spawn()
        }
    } else if cfg!(target_os = "macos") {
        if path.is_dir() {
            crate::quiet("open").arg(path).spawn()
        } else {
            crate::quiet("open").arg("-R").arg(path).spawn()
        }
    } else {
        let dir = if path.is_dir() { path } else { path.parent().unwrap_or(path) };
        crate::quiet("xdg-open").arg(dir).spawn()
    };
    let _ = r;
}
