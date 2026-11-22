use cpur3000::{Bus, Cpu};
struct Ram(Vec<u8>);
impl Bus for Ram {
    fn read(&mut self, a: u32, size: u32) -> u32 {
        let a = (a & 0xFFFF) as usize;
        (0..size as usize).fold(0, |v, i| v | (self.0[a + i] as u32) << (8 * i))
    }
    fn write(&mut self, _a: u32, _v: u32, _s: u32) {}
    fn gte_read(&mut self, _: u32) -> u32 { 0 }
    fn gte_write(&mut self, _: u32, _: u32) {}
    fn gte_command(&mut self, _: u32) {}
}
fn main() {
    let mut m = vec![0u8; 0x10000];
    let prog: [u32; 3] = [0x2421_0001, 0x1000_FFFE, 0];
    for (i, w) in prog.iter().enumerate() {
        m[0x1000 + i * 4..0x1004 + i * 4].copy_from_slice(&w.to_le_bytes());
    }
    let mut cpu = Cpu::new(Ram(m));
    let mut r = cpu.regs();
    r.pc = 0x8000_1000;
    r.next_pc = 0x8000_1004;
    cpu.set_regs(r);
    let t = std::time::Instant::now();
    for _ in 0..100_000_000u32 {
        cpu.step();
    }
    let el = t.elapsed();
    println!("r3000: {:.1} ns/step", el.as_nanos() as f64 / 1e8);
}
