//! Lists OS threads after N frames with a short backtrace: threads <iso> <frames>
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
    let ok = |p: u32| (0x8000_0000..0x8180_0000).contains(&p);
    let mut t = g.peek(0x8000_00DC);
    let cur = g.peek(0x8000_00E4);
    println!("current thread {cur:08X}, current context {:08X}", g.peek(0x8000_00D4));
    let mut n = 0;
    while ok(t) && n < 40 {
        let state = g.peek(t + 0x2C8) >> 16;
        let prio = g.peek(t + 0x2D0);
        let (srr0, lr, sp) = (g.peek(t + 0x198), g.peek(t + 0x84), g.peek(t + 4));
        println!("thread {t:08X} state={state} prio={prio} pc={srr0:08X} lr={lr:08X} sp={sp:08X} queue={:08X}", g.peek(t + 0x2DC));
        // Back-chain.
        let mut s = sp;
        let mut chain = Vec::new();
        for _ in 0..8 {
            if !ok(s) {
                break;
            }
            let back = g.peek(s);
            let ret = g.peek(back.wrapping_add(4));
            if !ok(back) {
                break;
            }
            chain.push(format!("{ret:08X}"));
            s = back;
        }
        println!("   callers: {}", chain.join(" "));
        t = g.peek(t + 0x2FC);
        n += 1;
    }
}
