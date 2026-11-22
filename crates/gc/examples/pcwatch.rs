//! Records executions of given PCs (GC_PAD optional): pcwatch <iso> <frames> <hex pc>...
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut g = gc::GameCube::from_disc(std::path::Path::new(&a[1])).expect("load");
    let pcs: Vec<u32> = a[3..].iter().map(|s| u32::from_str_radix(s, 16).unwrap()).collect();
    g.watch_pc_list(&pcs);
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
    for (f, pc, lr, r3) in g.pc_log().iter().skip(g.pc_log().len().saturating_sub(24)) {
        println!("frame {f} pc={pc:08X} lr={lr:08X} r3={r3:X}");
    }
    println!("total hits: {}", g.pc_log().len());
}
