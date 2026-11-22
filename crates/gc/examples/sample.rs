//! After N frames (GC_PAD optional), samples where the CPU is for M more frames: sample <iso> <frames> <more>
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut g = gc::GameCube::from_disc(std::path::Path::new(&a[1])).expect("load");
    let script: Vec<(usize, u16, usize)> = std::env::var("GC_PAD")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let p: Vec<&str> = s.split(':').collect();
            (p[0].parse().unwrap(), u16::from_str_radix(p[1], 16).unwrap(), p[2].parse().unwrap())
        })
        .collect();
    let total = a[2].parse::<usize>().unwrap() + a[3].parse::<usize>().unwrap();
    for f in 0..total {
        if f == a[2].parse::<usize>().unwrap() {
            g.sample_pcs(true);
        }
        let b = script.iter().filter(|(at, _, len)| f >= *at && f < at + len).fold(0, |acc, (_, b, _)| acc | b);
        g.set_pad(0, gc::Pad { buttons: b, ..Default::default() });
        g.run_frame();
    }
    for (pc, n) in g.pc_histogram().iter().take(40) {
        println!("{pc:08X} {n}");
    }
}
