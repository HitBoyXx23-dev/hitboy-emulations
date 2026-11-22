//! Runs a .dol for N frames and writes a PPM: shot <dol> <frames> <out.ppm> [buttons]
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let dol = std::fs::read(&a[1]).expect("read");
    let frames: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(60);
    let mut g = gc::GameCube::from_dol(&dol).expect("load");
    let buttons: u16 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
    let t = std::time::Instant::now();
    for f in 0..frames {
        if f > frames / 2 {
            g.set_pad(0, gc::Pad { buttons, ..Default::default() });
        }
        g.run_frame();
    }
    let (w, h) = g.size();
    let mut out = format!("P6 {w} {h} 255\n").into_bytes();
    for p in g.frame() {
        out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    std::fs::write(&a[3], out).unwrap();
    eprintln!("{w}x{h} pc={:#x} gx={} {:?}
{}", g.pc(), g.gx_bytes(), t.elapsed(), g.debug());
}
