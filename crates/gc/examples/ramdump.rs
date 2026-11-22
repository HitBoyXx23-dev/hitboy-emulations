//! Runs a disc for N frames and writes a RAM range: ramdump <iso> <frames> <hex addr> <len> <out>
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut g = gc::GameCube::from_disc(std::path::Path::new(&a[1])).expect("load");
    for _ in 0..a[2].parse::<usize>().unwrap() {
        g.run_frame();
    }
    let addr = usize::from_str_radix(&a[3], 16).unwrap() & 0x01FF_FFFF;
    let len: usize = a[4].parse().unwrap();
    std::fs::write(&a[5], &g.ram()[addr..addr + len]).unwrap();
}
