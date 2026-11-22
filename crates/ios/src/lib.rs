//! iPhone / iPad apps: reads .ipa packages (a zip with Payload/<App>.app/), the app's
//! Info.plist (XML or binary) and its Mach-O executable (architectures, encryption, the
//! frameworks it links, minimum iOS version). This is the front end of the iOS core.

pub mod im4p;
pub mod ipsw;
pub mod macho;
pub mod plist;
pub mod runtime;

use plist::Value;
use std::io::Read;

#[derive(Debug, Clone)]
pub struct AppInfo {
    pub name: String,
    pub bundle_id: String,
    pub version: String,
    pub min_os: String,
    pub iphone: bool,
    pub ipad: bool,
    pub executable: String,
    pub archs: Vec<String>,
    pub encrypted: bool,
    pub frameworks: Vec<String>,
}

impl AppInfo {
    /// Whether this is the kind of app the core targets first: 32-bit ARM, old iOS, decrypted.
    pub fn classic(&self) -> bool {
        let major: u32 = self.min_os.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(99);
        !self.encrypted && major <= 4 && self.archs.iter().any(|a| a.starts_with("armv6") || a.starts_with("armv7"))
    }

    /// A one-paragraph description for the app's UI.
    pub fn summary(&self) -> String {
        let devices = match (self.iphone, self.ipad) {
            (true, true) => "iPhone and iPad",
            (false, true) => "iPad",
            _ => "iPhone",
        };
        let mut s = format!(
            "{} {} ({}) for {devices}. Needs iOS {} or later, built for {}.",
            self.name,
            self.version,
            self.bundle_id,
            if self.min_os.is_empty() { "?" } else { &self.min_os },
            if self.archs.is_empty() { "an unknown processor".to_string() } else { self.archs.join(" + ") }
        );
        if self.encrypted {
            s.push_str(" It is encrypted (App Store protection), so it can't run until it's decrypted on the device you own.");
        } else if self.classic() {
            s.push_str(" It is a classic 32-bit app: the first kind the iOS core will run.");
        } else {
            s.push_str(" It needs a newer iOS than the core targets first (iPhone OS 2-4).");
        }
        s
    }
}

/// Reads an .ipa file.
pub fn read_ipa(data: &[u8]) -> Result<AppInfo, String> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|_| "This isn't an .ipa file (not a zip archive).".to_string())?;
    let names: Vec<String> = (0..z.len()).filter_map(|i| z.by_index(i).ok().map(|f| f.name().to_string())).collect();
    let plist_name = names
        .iter()
        .find(|n| n.starts_with("Payload/") && n.ends_with(".app/Info.plist") && n.matches('/').count() == 2)
        .ok_or("The .ipa has no Payload/<App>.app/Info.plist.")?
        .clone();
    let app_dir = plist_name.trim_end_matches("Info.plist").to_string();
    let read = |z: &mut zip::ZipArchive<std::io::Cursor<&[u8]>>, n: &str| -> Result<Vec<u8>, String> {
        let mut f = z.by_name(n).map_err(|e| format!("{n}: {e}"))?;
        let mut v = Vec::new();
        f.read_to_end(&mut v).map_err(|e| e.to_string())?;
        Ok(v)
    };
    let info = plist::parse(&read(&mut z, &plist_name)?)?;
    let get = |k: &str| info.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let executable = get("CFBundleExecutable");
    let families: Vec<i64> = info.get("UIDeviceFamily").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_int).collect()).unwrap_or_default();
    let mut app = AppInfo {
        name: [get("CFBundleDisplayName"), get("CFBundleName"), executable.clone()].into_iter().find(|s| !s.is_empty()).unwrap_or_default(),
        bundle_id: get("CFBundleIdentifier"),
        version: [get("CFBundleShortVersionString"), get("CFBundleVersion")].into_iter().find(|s| !s.is_empty()).unwrap_or_default(),
        min_os: get("MinimumOSVersion"),
        iphone: families.is_empty() || families.contains(&1),
        ipad: families.contains(&2),
        executable: executable.clone(),
        archs: Vec::new(),
        encrypted: false,
        frameworks: Vec::new(),
    };
    if !executable.is_empty() {
        let bin = read(&mut z, &format!("{app_dir}{executable}"))?;
        let m = macho::parse(&bin)?;
        app.archs = m.slices.iter().map(|s| s.arch.clone()).collect();
        app.encrypted = m.slices.iter().any(|s| s.encrypted);
        if let Some(s) = m.slices.first() {
            app.frameworks = s.dylibs.clone();
            if app.min_os.is_empty() {
                app.min_os = s.min_os.clone();
            }
        }
    }
    Ok(app)
}

/// Extracts the Mach-O executable from an `.ipa` and runs it on our 32-bit ARM core if it is
/// a decrypted 32-bit ARM iOS app.
pub fn run_ipa(data: &[u8], max_steps: u64) -> Result<(AppInfo, Result<runtime::RunReport, String>), String> {
    let app = read_ipa(data)?;
    if app.executable.is_empty() {
        return Ok((app, Err("This .ipa does not declare a CFBundleExecutable.".into())));
    }
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|_| "Not a zip archive.".to_string())?;
    let names: Vec<String> = (0..z.len()).filter_map(|i| z.by_index(i).ok().map(|f| f.name().to_string())).collect();
    let plist_name = names
        .iter()
        .find(|n| n.starts_with("Payload/") && n.ends_with(".app/Info.plist") && n.matches('/').count() == 2)
        .ok_or("Missing Info.plist.")?
        .clone();
    let app_dir = plist_name.trim_end_matches("Info.plist");
    let mut f = z.by_name(&format!("{app_dir}{}", app.executable)).map_err(|e| e.to_string())?;
    let mut bin = Vec::new();
    f.read_to_end(&mut bin).map_err(|e| e.to_string())?;
    let run_res = runtime::run_macho(&bin, max_steps);
    Ok((app, run_res))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_a_minimal_ipa() {
        let plist = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>Demo</string>
<key>CFBundleIdentifier</key><string>com.example.demo</string>
<key>CFBundleName</key><string>Demo</string>
<key>CFBundleShortVersionString</key><string>1.2</string>
<key>MinimumOSVersion</key><string>3.1</string>
<key>UIDeviceFamily</key><array><integer>1</integer><integer>2</integer></array>
</dict></plist>"#;
        let exe = macho::tests::thin_armv7(false);
        let mut buf = Vec::new();
        {
            let mut z = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let o: zip::write::SimpleFileOptions = Default::default();
            z.start_file("Payload/Demo.app/Info.plist", o).unwrap();
            z.write_all(plist).unwrap();
            z.start_file("Payload/Demo.app/Demo", o).unwrap();
            z.write_all(&exe).unwrap();
            z.finish().unwrap();
        }
        let app = read_ipa(&buf).unwrap();
        assert_eq!(app.name, "Demo");
        assert_eq!(app.bundle_id, "com.example.demo");
        assert_eq!(app.min_os, "3.1");
        assert!(app.iphone && app.ipad);
        assert_eq!(app.archs, vec!["armv7"]);
        assert!(!app.encrypted);
        assert!(app.classic());
        assert_eq!(app.frameworks, vec!["/System/Library/Frameworks/UIKit.framework/UIKit"]);
    }
}
