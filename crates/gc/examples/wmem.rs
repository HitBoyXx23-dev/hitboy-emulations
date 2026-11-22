//! Reports writes to a memory word: wmem <iso> <steps> <hex addr> [max]
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut g = gc::GameCube::from_disc(std::path::Path::new(&a[1])).expect("load");
    let steps: u64 = a[2].parse().unwrap();
    let addr = u32::from_str_radix(&a[3], 16).unwrap();
    let max: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(20);
    for (i, pc, old, new, lr) in g.watch_mem(addr, steps, max) {
        println!("step {i} pc={pc:08X} {old:08X} -> {new:08X} lr={lr:08X}");
    }
}
