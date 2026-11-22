//! Runs a disc for N frames then prints memory words: peek <iso> <frames> <hex addr> <count>
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let mut g = gc::GameCube::from_disc(std::path::Path::new(&a[1])).expect("load");
    for _ in 0..a[2].parse::<usize>().unwrap() {
        g.run_frame();
    }
    let addr = u32::from_str_radix(&a[3], 16).unwrap();
    for i in 0..a[4].parse::<u32>().unwrap() {
        println!("{:08X}: {:08X}", addr + i * 4, g.peek(addr + i * 4));
    }
}
