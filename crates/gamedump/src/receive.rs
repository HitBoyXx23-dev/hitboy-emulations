//! A small receiver that lets files from your own devices land on this PC over your home
//! network: `gamedump receive`. It serves one web page (open it in the browser on an Xbox,
//! PlayStation, Switch, phone or another PC, pick files, press Send) and saves what arrives
//! into a folder. It is not a dumper: it can only receive files that the sending device's
//! browser or app is allowed to read, such as ones on a USB drive or in a Developer Mode app's
//! own storage.
//!
//! Safety: the page and the upload address contain a random secret, so only someone who has
//! the address can send; it only runs while the command runs; names are cleaned so nothing can
//! be written outside the chosen folder; existing files are never overwritten.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};

/// Biggest single file accepted unless the user raises it (a 50 GB disc image fits).
pub const DEFAULT_MAX_FILE: u64 = 64 << 30;

pub struct Receiver {
    listener: TcpListener,
    pub token: String,
    pub dir: PathBuf,
    /// Largest single file accepted, in bytes.
    pub max_file: u64,
}

fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for _ in 0..2 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// This PC's address on the local network (no packet is sent; the OS just picks the route).
pub fn lan_address() -> Option<String> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Turns what the sender called the file into a plain, safe file name.
pub fn clean_name(raw: &str) -> Option<String> {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = last.chars().filter(|c| !c.is_control() && !"<>:\"|?*".contains(*c)).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim().to_string();
    if cleaned.is_empty() || cleaned.len() > 200 {
        None
    } else {
        Some(cleaned)
    }
}

/// A name in `dir` that doesn't exist yet ("game.iso", "game (1).iso", ...).
fn free_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..10_000).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap_or_else(|| dir.join(format!("{stem} (new){ext}")))
}

const PAGE: &str = r#"<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Send files to your PC</title>
<style>body{font:18px system-ui,sans-serif;background:#10131c;color:#e8ecf6;max-width:640px;margin:40px auto;padding:0 20px}
button,input{font:inherit}button{background:#4169e1;color:#fff;border:0;border-radius:8px;padding:12px 22px;margin-top:14px}
li{margin:6px 0}.ok{color:#5fd38d}.bad{color:#ff7b7b}small{color:#9aa4bd}</style>
<h1>Send files to your PC</h1>
<p>Pick files from this device (for example from a USB drive) and press Send. They are saved in the folder the receiving app chose.</p>
<input id="f" type="file" multiple><br><button id="go">Send</button>
<ul id="log"></ul><small>Only send files you own. This page can only send files this device lets its browser read.</small>
<script>
const log=document.getElementById('log');
document.getElementById('go').onclick=async()=>{
 for(const file of document.getElementById('f').files){
  const li=document.createElement('li');li.textContent=file.name+' ...';log.appendChild(li);
  await new Promise(done=>{
   const x=new XMLHttpRequest();
   x.open('PUT','up/'+encodeURIComponent(file.name));
   x.upload.onprogress=e=>{if(e.lengthComputable)li.textContent=file.name+' '+Math.round(100*e.loaded/e.total)+'%'};
   x.onload=()=>{li.textContent=file.name+(x.status==201?' - sent':' - failed: '+x.responseText);li.className=x.status==201?'ok':'bad';done()};
   x.onerror=()=>{li.textContent=file.name+' - connection lost';li.className='bad';done()};
   x.send(file);
  });
 }
};
</script>"#;

fn reply(s: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len());
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
}

/// Answers a request we won't accept. The sender may already be pushing the file's bytes, and
/// closing with them unread would reset the connection before it saw our answer, so after
/// replying we read (and discard) a little of what is still arriving.
fn refuse(s: &mut TcpStream, status: &str, text: &str) {
    reply(s, status, "text/plain", text.as_bytes());
    let _ = s.shutdown(std::net::Shutdown::Write);
    let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(300)));
    let mut sink = [0u8; 8192];
    let mut left: usize = 1 << 20;
    while left > 0 {
        match s.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(n) => left = left.saturating_sub(n),
        }
    }
}

impl Receiver {
    /// Starts listening on all network interfaces (port 0 picks a free one).
    pub fn bind(dir: &Path, port: u16) -> std::io::Result<Receiver> {
        std::fs::create_dir_all(dir)?;
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        Ok(Receiver { listener, token: random_token(), dir: dir.to_path_buf(), max_file: DEFAULT_MAX_FILE })
    }

    pub fn port(&self) -> u16 {
        self.listener.local_addr().map_or(0, |a| a.port())
    }

    /// Serves forever; `saved` is called with each finished file.
    pub fn run(self, saved: impl Fn(&Path, u64) + Send + Sync + 'static) {
        let saved = std::sync::Arc::new(saved);
        let (token, dir, max_file) = (self.token.clone(), self.dir.clone(), self.max_file);
        for conn in self.listener.incoming().flatten() {
            let (token, dir, saved) = (token.clone(), dir.clone(), saved.clone());
            std::thread::spawn(move || {
                let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(60)));
                handle(conn, &token, &dir, max_file, &*saved);
            });
        }
    }
}

fn handle(mut s: TcpStream, token: &str, dir: &Path, max_file: u64, saved: &dyn Fn(&Path, u64)) {
    // Read the request head.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 16 * 1024 {
            return reply(&mut s, "431 Request Header Fields Too Large", "text/plain", b"too large");
        }
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut parts = lines.next().unwrap_or("").split(' ');
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let length: Option<u64> = lines.filter_map(|l| l.split_once(':')).find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.trim().parse().ok());
    let path = target.split('?').next().unwrap_or("");
    let Some(rest) = path.strip_prefix('/').and_then(|p| p.strip_prefix(token)) else {
        return reply(&mut s, "404 Not Found", "text/plain", b"not found");
    };
    match (method, rest) {
        ("GET", "" | "/") => reply(&mut s, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()),
        ("PUT", r) if r.starts_with("/up/") => {
            let Some(name) = clean_name(&percent_decode(&r[4..])) else {
                return refuse(&mut s, "400 Bad Request", "bad file name");
            };
            let Some(len) = length else {
                return refuse(&mut s, "411 Length Required", "length required");
            };
            if len > max_file {
                return refuse(&mut s, "413 Payload Too Large", "file too large");
            }
            let dest = free_path(dir, &name);
            let part = dest.with_extension("part");
            let Ok(mut f) = std::fs::File::create(&part) else {
                return reply(&mut s, "500 Internal Server Error", "text/plain", b"can't write");
            };
            let mut got = (buf.len() - head_end).min(len as usize) as u64;
            let ok = f.write_all(&buf[head_end..head_end + got as usize]).is_ok();
            let mut ok = ok;
            while ok && got < len {
                let want = ((len - got) as usize).min(chunk.len());
                match s.read(&mut chunk[..want]) {
                    Ok(0) | Err(_) => ok = false,
                    Ok(n) => {
                        ok = f.write_all(&chunk[..n]).is_ok();
                        got += n as u64;
                    }
                }
            }
            drop(f);
            if ok && got == len && std::fs::rename(&part, &dest).is_ok() {
                saved(&dest, len);
                reply(&mut s, "201 Created", "text/plain", b"ok");
            } else {
                let _ = std::fs::remove_file(&part);
                reply(&mut s, "500 Internal Server Error", "text/plain", b"upload incomplete");
            }
        }
        _ => reply(&mut s, "404 Not Found", "text/plain", b"not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_made_safe() {
        assert_eq!(clean_name("../../Windows/evil.exe").as_deref(), Some("evil.exe"));
        assert_eq!(clean_name("C:\\x\\game.iso").as_deref(), Some("game.iso"));
        assert_eq!(clean_name("..").as_deref(), None);
        assert_eq!(clean_name(".hidden").as_deref(), Some("hidden"));
        assert_eq!(clean_name("a<b>.bin").as_deref(), Some("ab.bin"));
    }

    #[test]
    fn the_size_limit_can_be_raised_or_lowered() {
        let dir = std::env::temp_dir().join(format!("recv-limit-{}", std::process::id()));
        let mut r = Receiver::bind(&dir, 0).unwrap();
        r.max_file = 4;
        let (port, token) = (r.port(), r.token.clone());
        std::thread::spawn(move || r.run(|_, _| {}));
        let send = |body: &[u8]| -> String {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(s, "PUT /{token}/up/x.bin HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
            s.write_all(body).unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        };
        let got = send(b"12345"); assert!(got.starts_with("HTTP/1.1 413"), "got: {got:?}");
        assert!(send(b"1234").starts_with("HTTP/1.1 201"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_can_be_sent_and_the_secret_is_required() {
        let dir = std::env::temp_dir().join(format!("recv-test-{}", std::process::id()));
        let r = Receiver::bind(&dir, 0).unwrap();
        let (port, token) = (r.port(), r.token.clone());
        std::thread::spawn(move || r.run(|_, _| {}));
        let send = |path: &str, body: &[u8]| -> String {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            write!(s, "PUT {path} HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n", body.len()).unwrap();
            s.write_all(body).unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        };
        assert!(send("/wrongtoken/up/a.bin", b"1234").starts_with("HTTP/1.1 404"));
        assert!(send(&format!("/{token}/up/..%2F..%2Fgame%20one.bin"), b"hello world").starts_with("HTTP/1.1 201"));
        assert_eq!(std::fs::read(dir.join("game one.bin")).unwrap(), b"hello world");
        // Sending the same name again never overwrites.
        assert!(send(&format!("/{token}/up/game%20one.bin"), b"second").starts_with("HTTP/1.1 201"));
        assert_eq!(std::fs::read(dir.join("game one (1).bin")).unwrap(), b"second");
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET /{token}/ HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut page = String::new();
        s.read_to_string(&mut page).unwrap();
        assert!(page.contains("Send files to your PC"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
