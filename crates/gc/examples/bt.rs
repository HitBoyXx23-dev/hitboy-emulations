//! After N frames (GC_PAD optional) prints the CPU state and a stack backtrace: bt <iso> <frames>
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
    if std::env::var_os("BREAK_NULL").is_some() {
        g.break_on_null_read(true);
    }
    for f in 0..a[2].parse::<usize>().unwrap() {
        let b = script.iter().filter(|(at, _, len)| f >= *at && f < at + len).fold(0, |acc, (_, b, _)| acc | b);
        g.set_pad(0, gc::Pad { buttons: b, ..Default::default() });
        g.run_frame();
        if g.stopped() {
            println!("stopped at frame {f}");
            break;
        }
    }
    let n = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(0usize);
    if n > 0 {
        g.trace(n);
    }
    println!("pc={:08X}", g.pc());
    for i in 0..32 {
        print!("r{i}={:08X} ", g.gpr(i));
        if i % 8 == 7 {
            println!();
        }
    }
    let ok = |p: u32| (0x8000_0000..0x8180_0000).contains(&p);
    let mut sp = g.gpr(1);
    let mut chain = Vec::new();
    for _ in 0..24 {
        if !ok(sp) {
            break;
        }
        let back = g.peek(sp);
        if !ok(back) {
            break;
        }
        chain.push(format!("{:08X}", g.peek(back.wrapping_add(4))));
        sp = back;
    }
    println!("callers: {}", chain.join(" "));
}
