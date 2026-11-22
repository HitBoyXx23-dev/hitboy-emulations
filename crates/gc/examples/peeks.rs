//! Runs a disc (GC_PAD script optional) then prints words at a stride: peeks <iso> <frames> <hex addr> <stride> <count>
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
    let base = u32::from_str_radix(&a[3], 16).unwrap();
    let stride: u32 = a[4].parse().unwrap();
    for i in 0..a[5].parse::<u32>().unwrap() {
        let at = base + i * stride;
        println!("{at:08X}: {:08X} {:08X} {:08X}", g.peek(at), g.peek(at + 4), g.peek(at + 8));
    }
}
