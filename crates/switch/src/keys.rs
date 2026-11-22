//! The user's Switch keys (prod.keys), dumped from their own console: "name = hex" lines.

use std::collections::HashMap;

pub struct Keys(HashMap<String, Vec<u8>>);

/// Keys needed to open game content (NCA header key plus at least one application key-area key).
const REQUIRED: &[&str] = &["header_key"];

impl Keys {
    pub fn parse(text: &str) -> Result<Keys, String> {
        let mut map = HashMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            let Some((name, value)) = line.split_once('=') else { continue };
            let hex: String = value.trim().chars().filter(|c| !c.is_whitespace()).collect();
            if hex.len() % 2 != 0 {
                return Err(format!("Key \"{}\" has an odd number of hex digits.", name.trim()));
            }
            let bytes = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16)).collect::<Result<Vec<u8>, _>>().map_err(|_| format!("Key \"{}\" isn't valid hex.", name.trim()))?;
            map.insert(name.trim().to_ascii_lowercase(), bytes);
        }
        let k = Keys(map);
        for r in REQUIRED {
            if k.get(r).is_none() {
                return Err(format!("The keys file is missing \"{r}\". Dump prod.keys again from your own Switch."));
            }
        }
        if !k.0.keys().any(|n| n.starts_with("key_area_key_application_")) {
            return Err("The keys file has no key_area_key_application keys. Dump prod.keys again from your own Switch.".into());
        }
        Ok(k)
    }

    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.0.get(name).map(Vec::as_slice)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_validates() {
        let text = "; comment\nheader_key = 00112233445566778899AABBCCDDEEFF00112233445566778899AABBCCDDEEFF\nkey_area_key_application_00 = 0102030405060708090A0B0C0D0E0F10\n";
        let k = Keys::parse(text).unwrap();
        assert_eq!(k.get("header_key").unwrap().len(), 32);
        assert_eq!(k.get("key_area_key_application_00").unwrap()[0], 1);
        assert!(Keys::parse("titlekek_00 = 00").is_err(), "missing header_key");
        assert!(Keys::parse("header_key = zz").is_err(), "bad hex");
    }
}
