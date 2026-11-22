//! Runs a ROM for N frames and writes the screen as a PPM: shot <rom> <frames> <out.ppm>
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let rom = std::fs::read(&a[1]).expect("read rom");
    let frames: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);
    let mut n = n64::N64::from_rom(rom).expect("load");
    for _ in 0..frames {
        n.run_frame();
    }
    let (w, h) = n.size();
    let mut out = format!("P6 {w} {h} 255\n").into_bytes();
    for p in n.frame() {
        out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    std::fs::write(&a[3], out).unwrap();
    eprintln!("{} {}x{} pc={:#x} rsp/rdp ms={:?}", a[1], w, h, n.pc(), n64::profile_ms());
}
