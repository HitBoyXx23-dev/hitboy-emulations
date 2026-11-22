//! Runs a disc for N frames and writes the graphics chip's frame buffer: efb <iso> <frames> <out.ppm>
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut g = gc::GameCube::from_disc(std::path::Path::new(&a[1])).expect("load");
    for _ in 0..a[2].parse::<usize>().unwrap() {
        g.run_frame();
    }
    let (w, h, px) = g.efb();
    let mut out = format!("P6 {w} {h} 255\n").into_bytes();
    for p in px {
        out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    std::fs::write(&a[3], out).unwrap();
    eprintln!("{:?} fate={:?}", g.gx_stats(), g.gx_fate());
}
