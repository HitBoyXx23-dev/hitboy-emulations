//! Boots a PlayStation 2 BIOS dump and reports what it does: boot <bios.bin> [frames]
//! Prints the program counter, the kernel's own debug output, and which hardware registers
//! the BIOS touches that we don't model yet (that list is the to-do list).

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let Some(path) = a.get(1) else {
        eprintln!("usage: boot <bios.bin> [frames]");
        std::process::exit(2);
    };
    let bios = std::fs::read(path).expect("read BIOS");
    let frames: u32 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(120);
    let mut m = ps2::Ps2::new(bios).expect("BIOS");
    for f in 0..frames {
        m.run_frame();
        if f % 20 == 0 {
            println!("frame {f}: EE pc={:#010x} IOP pc={:#010x} instructions={} unimplemented={}", m.pc(), m.iop_pc(), m.cpu.cycles, m.cpu.unimplemented);
        }
    }
    if !m.bus.console_output.is_empty() {
        println!("--- kernel console ---\n{}", m.bus.console_output);
    }
    if std::env::var_os("REGLOG").is_some() {
        for l in &m.bus.reg_log {
            println!("REG {l}");
        }
    }
    if std::env::var_os("MCH").is_some() {
        for l in &m.bus.mch_log {
            println!("MCH {l}");
        }
    }
    println!("--- Emotion Engine registers touched but not modelled (address, reads, writes) ---");
    for (addr, r, w) in m.unhandled().into_iter().take(25) {
        println!("{addr:#010x}  reads {r:>8}  writes {w:>8}");
    }
    println!("--- I/O processor registers touched but not modelled ---");
    let mut v: Vec<_> = m.iop.bus().unhandled.iter().map(|(a, (r, w))| (*a, *r, *w)).collect();
    v.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)));
    for (addr, r, w) in v.into_iter().take(25) {
        println!("{addr:#010x}  reads {r:>8}  writes {w:>8}");
    }
}
