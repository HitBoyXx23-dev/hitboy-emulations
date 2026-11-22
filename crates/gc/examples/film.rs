//! Runs a disc and saves every Kth frame as PPM: film <iso> <frames> <every> <out prefix> [start]
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut g = gc::GameCube::from_disc(std::path::Path::new(&a[1])).expect("load");
    let frames: usize = a[2].parse().unwrap();
    let every: usize = a[3].parse().unwrap();
    let start: usize = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(0);
    // Optional button script: GC_PAD="frame:buttons:frames,..." (buttons in hex).
    let script: Vec<(usize, u16, usize)> = std::env::var("GC_PAD")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let p: Vec<&str> = s.split(':').collect();
            (p[0].parse().unwrap(), u16::from_str_radix(p[1], 16).unwrap(), p[2].parse().unwrap())
        })
        .collect();
    for f in 0..frames {
        let b = script.iter().filter(|(at, _, len)| f >= *at && f < at + len).fold(0, |acc, (_, b, _)| acc | b);
        // The D-pad also drives the main stick, as the app does.
        let x = if b & gc::button::LEFT != 0 { -100 } else if b & gc::button::RIGHT != 0 { 100 } else { 0 };
        g.set_pad(0, gc::Pad { buttons: b, stick: (x, 0), ..Default::default() });
        g.run_frame();
        if f >= start && f % every == 0 {
            let (w, h) = g.size();
            let lit = g.frame().iter().filter(|&&p| p & 0xFFFFFF > 0x101010).count();
            let mut out = format!("P6 {w} {h} 255\n").into_bytes();
            for p in g.frame() {
                out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
            }
            std::fs::write(format!("{}_{f:05}.ppm", a[4]), out).unwrap();
            eprintln!("frame {f}: {w}x{h} lit={lit} stats={:?} {}", g.gx_stats(), g.video_debug());
        }
    }
}
