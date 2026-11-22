//! Runs a disc (with an optional GC_PAD script), then histograms the PCs of the next N
//! instructions: hot <iso> <frames> <instructions>
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
    for f in 0..a[2].parse::<usize>().unwrap() {
        let b = script.iter().filter(|(at, _, len)| f >= *at && f < at + len).fold(0, |acc, (_, b, _)| acc | b);
        g.set_pad(0, gc::Pad { buttons: b, ..Default::default() });
        g.run_frame();
    }
    let mut h = std::collections::BTreeMap::new();
    for (pc, _) in g.trace(a[3].parse().unwrap()) {
        *h.entry(pc).or_insert(0u32) += 1;
    }
    let mut v: Vec<_> = h.into_iter().collect();
    v.sort_by(|x, y| y.1.cmp(&x.1));
    for (pc, n) in v.iter().take(40) {
        println!("{pc:08X} {n}");
    }
    println!("distinct pcs: {}", v.len());
    println!("{}", g.debug());
}
