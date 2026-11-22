//! Raw speed of the PowerPC interpreter: instructions per second on a tight loop that mixes
//! integer ops, loads, stores and a branch.

use cpugekko::{Bus, Cpu, Regs};
use std::time::Instant;

struct Ram(Vec<u8>);

impl Bus for Ram {
    fn read(&mut self, _a: u32, _s: u32) -> u64 {
        0
    }
    fn write(&mut self, _a: u32, _v: u64, _s: u32) {}
    fn ram(&mut self) -> Option<&mut [u8]> {
        Some(&mut self.0)
    }
}

fn main() {
    let mut ram = vec![0u8; 1 << 20];
    // loop: addi r3,r3,1 ; stw r3,0(r4) ; lwz r5,0(r4) ; add r6,r5,r3 ; cmpwi r3,0 ; b loop
    let prog: [u32; 6] = [0x3863_0001, 0x9064_0000, 0x80A4_0000, 0x7CC5_1A14, 0x2C03_0000, 0x4BFF_FFE8];
    for (i, w) in prog.iter().enumerate() {
        ram[0x1000 + i * 4..0x1000 + i * 4 + 4].copy_from_slice(&w.to_be_bytes());
    }
    let mut cpu = Cpu::new(Ram(ram));
    let mut r = Regs::default();
    r.pc = 0x8000_1000;
    r.msr = 0x30; // instruction and data translation on, as games run
    r.r[4] = 0x8000_8000;
    cpu.set_regs(r);
    // Block address translation as the boot ROM sets it up for games.
    for (spr, v) in [(528, 0x8000_1FFF), (529, 0x0000_0002), (530, 0x0000_0000), (531, 0x0000_0000), (536, 0x8000_1FFF), (537, 0x0000_0002), (538, 0xC000_1FFF), (539, 0x0000_002A)] {
        cpu.set_spr(spr, v);
    }
    let t = Instant::now();
    let mut total = 0u64;
    for _ in 0..40_000 {
        total += cpu.run(5000) as u64;
    }
    let s = t.elapsed().as_secs_f64();
    println!("{:.1} million instructions per second ({} in {:.2}s)", total as f64 / s / 1e6, total, s);
}
