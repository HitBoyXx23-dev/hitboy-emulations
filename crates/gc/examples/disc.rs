//! Boots a disc for N frames and reports: disc <iso> <frames> <out.ppm>
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut g = gc::GameCube::from_disc(std::path::Path::new(&a[1])).expect("load");
    eprintln!("title: {}", g.disc_title());
    let frames: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(300);
    let t = std::time::Instant::now();
    for f in 0..frames {
        g.run_frame();
        if f % 60 == 0 {
            eprintln!("frame {f}: pc={:#x} gx={} {:?} {}", g.pc(), g.gx_bytes(), g.gx_stats(), g.debug());
        }
    }
    let (w, h) = g.size();
    let lit = g.frame().iter().filter(|&&p| p & 0xFFFFFF > 0x101010).count();
    eprintln!("{w}x{h}, non-black pixels: {lit}, {:?}", t.elapsed());
    let mut out = format!("P6 {w} {h} 255\n").into_bytes();
    for p in g.frame() {
        out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    std::fs::write(a.get(3).map(String::as_str).unwrap_or("out.ppm"), out).unwrap();
}
