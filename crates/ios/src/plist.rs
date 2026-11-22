//! Property lists: Apple's XML format and the binary "bplist00" format.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Int(i64),
    Real(f64),
    Bool(bool),
    Data(Vec<u8>),
    Array(Vec<Value>),
    Dict(BTreeMap<String, Value>),
}

impl Value {
    pub fn get(&self, k: &str) -> Option<&Value> {
        match self {
            Value::Dict(d) => d.get(k),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }
}

pub fn parse(data: &[u8]) -> Result<Value, String> {
    if data.starts_with(b"bplist00") {
        parse_binary(data)
    } else {
        parse_xml(&String::from_utf8_lossy(data))
    }
}

// ---------------------------------------------------------------- binary

fn be(b: &[u8]) -> u64 {
    b.iter().fold(0, |v, &x| v << 8 | x as u64)
}

fn parse_binary(d: &[u8]) -> Result<Value, String> {
    if d.len() < 40 {
        return Err("Binary plist is too short.".into());
    }
    let t = &d[d.len() - 32..];
    let (off_size, ref_size) = (t[6] as usize, t[7] as usize);
    let (count, top, table) = (be(&t[8..16]) as usize, be(&t[16..24]) as usize, be(&t[24..32]) as usize);
    if off_size == 0 || ref_size == 0 || table + count * off_size > d.len() {
        return Err("Binary plist is damaged.".into());
    }
    let offsets: Vec<usize> = (0..count).map(|i| be(&d[table + i * off_size..table + (i + 1) * off_size]) as usize).collect();
    fn obj(d: &[u8], offs: &[usize], rs: usize, i: usize, depth: u32) -> Result<Value, String> {
        if depth > 64 {
            return Err("Binary plist nests too deeply.".into());
        }
        let o = *offs.get(i).ok_or("Bad plist object reference.")?;
        let m = *d.get(o).ok_or("Bad plist offset.")?;
        let (kind, info) = (m >> 4, (m & 0xF) as usize);
        // Length: small in the marker, or a following int object.
        let len_at = |o: usize| -> Result<(usize, usize), String> {
            if info != 0xF {
                return Ok((info, o + 1));
            }
            let im = *d.get(o + 1).ok_or("Bad plist length.")?;
            let n = 1usize << (im & 0xF);
            Ok((be(d.get(o + 2..o + 2 + n).ok_or("Bad plist length.")?) as usize, o + 2 + n))
        };
        Ok(match kind {
            0x0 => Value::Bool(info == 9),
            0x1 => {
                let n = 1usize << info;
                Value::Int(be(d.get(o + 1..o + 1 + n).ok_or("Bad int")?) as i64)
            }
            0x2 => {
                let n = 1usize << info;
                let b = d.get(o + 1..o + 1 + n).ok_or("Bad real")?;
                Value::Real(if n == 4 { f32::from_bits(be(b) as u32) as f64 } else { f64::from_bits(be(b)) })
            }
            0x4 => {
                let (n, s) = len_at(o)?;
                Value::Data(d.get(s..s + n).ok_or("Bad data")?.to_vec())
            }
            0x5 => {
                let (n, s) = len_at(o)?;
                Value::String(String::from_utf8_lossy(d.get(s..s + n).ok_or("Bad string")?).into_owned())
            }
            0x6 => {
                let (n, s) = len_at(o)?;
                let b = d.get(s..s + n * 2).ok_or("Bad string")?;
                let u: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                Value::String(String::from_utf16_lossy(&u))
            }
            0xA => {
                let (n, s) = len_at(o)?;
                let mut v = Vec::with_capacity(n);
                for k in 0..n {
                    let r = be(d.get(s + k * rs..s + (k + 1) * rs).ok_or("Bad array")?) as usize;
                    v.push(obj(d, offs, rs, r, depth + 1)?);
                }
                Value::Array(v)
            }
            0xD => {
                let (n, s) = len_at(o)?;
                let mut m = BTreeMap::new();
                for k in 0..n {
                    let kr = be(d.get(s + k * rs..s + (k + 1) * rs).ok_or("Bad dict")?) as usize;
                    let vr = be(d.get(s + (n + k) * rs..s + (n + k + 1) * rs).ok_or("Bad dict")?) as usize;
                    if let Value::String(key) = obj(d, offs, rs, kr, depth + 1)? {
                        m.insert(key, obj(d, offs, rs, vr, depth + 1)?);
                    }
                }
                Value::Dict(m)
            }
            _ => Value::Data(Vec::new()),
        })
    }
    obj(d, &offsets, ref_size, top, 0)
}

// ---------------------------------------------------------------- XML

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

struct Xml<'a> {
    s: &'a str,
    p: usize,
}

impl<'a> Xml<'a> {
    /// Next tag name (without < >), skipping text, comments and declarations.
    fn next_tag(&mut self) -> Option<&'a str> {
        loop {
            let start = self.s[self.p..].find('<')? + self.p;
            let end = self.s[start..].find('>')? + start;
            self.p = end + 1;
            let tag = &self.s[start + 1..end];
            if tag.starts_with('?') || tag.starts_with('!') {
                continue;
            }
            return Some(tag);
        }
    }
    fn text_until(&mut self, close: &str) -> &'a str {
        let end = self.s[self.p..].find(close).map(|i| i + self.p).unwrap_or(self.s.len());
        let t = &self.s[self.p..end];
        self.p = (end + close.len()).min(self.s.len());
        t
    }
    fn value(&mut self, tag: &'a str) -> Result<Value, String> {
        let name = tag.trim_end_matches('/').split_whitespace().next().unwrap_or("");
        let empty = tag.ends_with('/');
        Ok(match name {
            "string" if empty => Value::String(String::new()),
            "string" => Value::String(unescape(self.text_until("</string>"))),
            "integer" => Value::Int(self.text_until("</integer>").trim().parse().unwrap_or(0)),
            "real" => Value::Real(self.text_until("</real>").trim().parse().unwrap_or(0.0)),
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            "data" => Value::Data(Vec::new()),
            "array" if empty => Value::Array(vec![]),
            "array" => {
                let mut v = Vec::new();
                while let Some(t) = self.next_tag() {
                    if t == "/array" {
                        break;
                    }
                    v.push(self.value(t)?);
                }
                Value::Array(v)
            }
            "dict" if empty => Value::Dict(BTreeMap::new()),
            "dict" => {
                let mut m = BTreeMap::new();
                while let Some(t) = self.next_tag() {
                    if t == "/dict" {
                        break;
                    }
                    if t == "key" {
                        let k = unescape(self.text_until("</key>"));
                        let vt = self.next_tag().ok_or("Plist ends inside a dict.")?;
                        m.insert(k, self.value(vt)?);
                    }
                }
                Value::Dict(m)
            }
            other => return Err(format!("Unexpected plist element <{other}>.")),
        })
    }
}

fn parse_xml(s: &str) -> Result<Value, String> {
    let mut x = Xml { s, p: 0 };
    loop {
        let t = x.next_tag().ok_or("Empty plist.")?;
        if t.starts_with("plist") {
            continue;
        }
        return x.value(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_dict() {
        let v = parse(b"<?xml version=\"1.0\"?><plist><dict><key>A</key><string>x &amp; y</string><key>B</key><integer>7</integer><key>C</key><true/></dict></plist>").unwrap();
        assert_eq!(v.get("A").unwrap().as_str(), Some("x & y"));
        assert_eq!(v.get("B").unwrap().as_int(), Some(7));
        assert_eq!(v.get("C"), Some(&Value::Bool(true)));
    }

    #[test]
    fn binary_dict() {
        // {"Name": "Hi", "N": 5} as bplist00.
        let mut d = b"bplist00".to_vec();
        let objs: Vec<Vec<u8>> = vec![
            vec![0xD2, 1, 2, 3, 4],         // dict of 2: keys 1,2 values 3,4
            [vec![0x54], b"Name".to_vec()].concat(),
            [vec![0x51], b"N".to_vec()].concat(),
            [vec![0x52], b"Hi".to_vec()].concat(),
            vec![0x10, 5],
        ];
        let mut offs = Vec::new();
        for o in &objs {
            offs.push(d.len() as u8);
            d.extend(o);
        }
        let table = d.len();
        d.extend(&offs);
        let mut trailer = vec![0u8; 6];
        trailer.push(1); // offset size
        trailer.push(1); // ref size
        trailer.extend(5u64.to_be_bytes());
        trailer.extend(0u64.to_be_bytes());
        trailer.extend((table as u64).to_be_bytes());
        d.extend(trailer);
        let v = parse(&d).unwrap();
        assert_eq!(v.get("Name").unwrap().as_str(), Some("Hi"));
        assert_eq!(v.get("N").unwrap().as_int(), Some(5));
    }
}
