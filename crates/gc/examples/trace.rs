//! Runs a disc for N frames, then prints a short instruction trace: trace <iso> <frames> <count>
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut g = gc::GameCube::from_disc(std::path::Path::new(&a[1])).expect("load");
    let frames: usize = a[2].parse().unwrap();
    let count: usize = a[3].parse().unwrap();
    for _ in 0..frames {
        g.run_frame();
    }
    for (pc, w) in g.trace(count) {
        println!("{pc:08X}: {w:08X}");
    }
    for i in 0..32 {
        print!("r{i}={:08X} ", g.gpr(i));
    }
    println!();
}
