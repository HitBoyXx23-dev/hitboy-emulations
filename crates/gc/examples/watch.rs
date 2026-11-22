//! Single-steps from reset and reports when given PCs execute: watch <iso> <steps> <hex pc>...
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut g = gc::GameCube::from_disc(std::path::Path::new(&a[1])).expect("load");
    let steps: u64 = a[2].parse().unwrap();
    let pcs: Vec<u32> = a[3..].iter().filter(|s| s.as_str() != "msr").map(|s| u32::from_str_radix(s, 16).unwrap()).collect();
    for (i, pc, r3, r4, r5, lr, msr) in g.watch_pcs(&pcs, steps) {
        println!("step {i} pc={pc:08X} r3={r3:08X} r4={r4:08X} r5={r5:08X} lr={lr:08X} msr={msr:08X}");
    }
}
