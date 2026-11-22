//! Boots an image headless and prints the text screen: boot <iso|img> <seconds>
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut m = pc::Machine::new(&pc::Config { boot: Some(a[1].clone().into()), ..Default::default() }).expect("start");
    let secs: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let t = std::time::Instant::now();
    // Optional key presses: scan codes, comma-separated, sent after 3 seconds.
    let keys: Vec<u8> = a.get(3).map(|k| k.split(',').filter_map(|x| u8::from_str_radix(x, 16).ok()).collect()).unwrap_or_default();
    for f in 0..secs * 60 {
        if f == 179 && std::env::var("PCTRACE").is_ok() {
            m.set_trace(true);
        }
        if f == 180 {
            for &k in &keys {
                m.key_down(k, if k == 0x1C { 13 } else { 0 });
                m.key_up(k);
            }
        }
        m.run_frame(1.0 / 60.0, std::time::Duration::from_millis(100));
        if m.error().is_some() {
            break;
        }
    }
    eprintln!("{:?} instructions={} error={:?}", t.elapsed(), m.instructions, m.error());
    match m.screen() {
        pc::Screen::Text { cols, cells, .. } => {
            for row in cells.chunks(cols) {
                let line: String = row.iter().map(|c| pc::keys::CP437[(*c as u8) as usize]).collect();
                if !line.trim().is_empty() {
                    println!("{}", line.trim_end());
                }
            }
        }
        pc::Screen::Graphics { width, height, pixels } => {
            let out = std::env::var("PCSHOT").unwrap_or_else(|_| "screen.ppm".into());
            let mut f = format!("P6 {width} {height} 255
").into_bytes();
            for p in &pixels {
                f.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
            }
            std::fs::write(&out, f).unwrap();
            println!("(graphics {width}x{height}, saved to {out})");
        }
    }
    println!("{}", m.debug());
    if let Ok(path) = std::env::var("PCMEM") {
        std::fs::write(path, m.ram()).unwrap();
    }
}
