use cpuvr4300::{Bus, Cpu};
struct Ram(Vec<u8>);
impl Bus for Ram {
    fn read(&mut self, a: u32, size: u32) -> u64 {
        let a = (a & 0xFFFF) as usize;
        (0..size as usize).fold(0, |v, i| (v << 8) | self.0[a + i] as u64)
    }
    fn write(&mut self, _a: u32, _v: u64, _s: u32) {}
    fn ram(&mut self) -> Option<&mut [u8]> {
        Some(&mut self.0)
    }
}
fn main() {
    let mut m = vec![0u8; 0x10000];
    // loop: addiu r1,r1,1 ; bne r0,r0? use: addiu r1,r1,1 ; beq r0,r0,-2 ; nop
    let prog: [u32; 3] = [0x2421_0001, 0x1000_FFFE, 0];
    for (i, w) in prog.iter().enumerate() {
        m[0x1000 + i * 4..0x1004 + i * 4].copy_from_slice(&w.to_be_bytes());
    }
    let mut cpu = Cpu::new(Ram(m));
    let mut r = cpu.regs();
    r.pc = 0xFFFF_FFFF_8000_1000;
    cpu.set_regs(r);
    let t0 = std::time::Instant::now();
    for i in 0..100_000_000u32 {
        cpu.set_ip(i & 0);
    }
    println!("empty FFI call: {:.1} ns", t0.elapsed().as_nanos() as f64 / 1e8);
    // Straight-line NOPs over 48 KiB, jumping back each lap.
    let mut cpu2 = {
        let mut m = vec![0u8; 0x10000];
        let n = 0xC000 / 4;
        let j = 0x0800_0000u32 | (0x1000 >> 2); // j 0x80001000
        m[0x1000 + (n - 2) * 4..0x1000 + (n - 1) * 4].copy_from_slice(&j.to_be_bytes());
        Cpu::new(Ram(m))
    };
    let mut r2 = cpu2.regs();
    r2.pc = 0xFFFF_FFFF_8000_1000;
    cpu2.set_regs(r2);
    let t1 = std::time::Instant::now();
    for _ in 0..100_000_000u32 {
        cpu2.step();
    }
    println!("nop stream: {:.1} ns/step", t1.elapsed().as_nanos() as f64 / 1e8);
    let t = std::time::Instant::now();
    for _ in 0..100_000_000u32 {
        cpu.step();
    }
    let el = t.elapsed();
    println!("100M steps in {:?} ({:.1} ns/step), r1={}", el, el.as_nanos() as f64 / 1e8, cpu.regs().r[1]);
}
