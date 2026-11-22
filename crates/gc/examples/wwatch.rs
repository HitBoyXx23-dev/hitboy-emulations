//! Runs a disc with a write watch on a RAM word: wwatch <iso> <frames> <hex addr> [max]
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut g = gc::GameCube::from_disc(std::path::Path::new(&a[1])).expect("load");
    g.watch(u32::from_str_radix(&a[3], 16).unwrap());
    let max: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(40);
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
        if g.watch_log().len() >= max {
            break;
        }
    }
    for (f, pc, lr, v) in g.watch_log().iter().take(max) {
        println!("frame {f} pc={pc:08X} lr={lr:08X} value={v:X}");
    }
}
