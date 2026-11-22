use super::*;

/// 4 MB of RAM at 0 plus the scratchpad, which is all these tests need.
struct Ram {
    mem: Vec<u8>,
    spr: Vec<u8>,
}

impl Ram {
    fn new() -> Ram {
        Ram { mem: vec![0; 4 << 20], spr: vec![0; SCRATCHPAD_SIZE as usize] }
    }

    fn load(&mut self, at: u32, words: &[u32]) {
        for (i, w) in words.iter().enumerate() {
            let a = at as usize + i * 4;
            self.mem[a..a + 4].copy_from_slice(&w.to_le_bytes());
        }
    }

    fn slice(&mut self, addr: u32, size: u32) -> &mut [u8] {
        if addr >= SCRATCHPAD_BASE {
            let o = (addr - SCRATCHPAD_BASE) as usize;
            &mut self.spr[o..o + size as usize]
        } else {
            let o = (addr as usize) & 0x3F_FFFF;
            &mut self.mem[o..o + size as usize]
        }
    }
}

impl Bus for Ram {
    fn read(&mut self, addr: u32, size: u32) -> u64 {
        let s = self.slice(addr, size);
        let mut b = [0u8; 8];
        b[..size as usize].copy_from_slice(s);
        u64::from_le_bytes(b)
    }

    fn write(&mut self, addr: u32, v: u64, size: u32) {
        self.slice(addr, size).copy_from_slice(&v.to_le_bytes()[..size as usize]);
    }
}

// Instruction encoders.
fn r(funct: u32, rs: u32, rt: u32, rd: u32, sa: u32) -> u32 {
    rs << 21 | rt << 16 | rd << 11 | sa << 6 | funct
}
fn i(op: u32, rs: u32, rt: u32, imm: u32) -> u32 {
    op << 26 | rs << 21 | rt << 16 | (imm & 0xFFFF)
}

fn cpu_at(pc: u32) -> Cpu {
    let mut c = Cpu::new();
    c.cop0[STATUS] = 0x2000_0000 | 0x1000_0000; // coprocessors usable, no BEV, no IE
    c.set_pc(pc);
    c
}

fn run(c: &mut Cpu, ram: &mut Ram, n: u32) {
    for _ in 0..n {
        c.step(ram);
    }
}

#[test]
fn arithmetic_sign_extends_32_bit_results() {
    let mut ram = Ram::new();
    ram.load(
        0x80_0000 & 0x3F_FFFF,
        &[
            i(15, 0, 1, 0x8000),     // lui r1,0x8000       -> 0xFFFFFFFF80000000
            i(9, 1, 2, 0xFFFF),      // addiu r2,r1,-1      -> 0x7FFFFFFF (wraps, sign-extended)
            r(45, 1, 1, 3, 0),       // daddu r3,r1,r1      -> 64-bit add
            r(0, 0, 1, 4, 4),        // sll r4,r1,4
        ],
    );
    let mut c = cpu_at(0x8000_0000 | (0x80_0000 & 0x3F_FFFF));
    run(&mut c, &mut ram, 4);
    assert_eq!(c.reg(1), 0xFFFF_FFFF_8000_0000);
    assert_eq!(c.reg(2), 0x7FFF_FFFF);
    assert_eq!(c.reg(3), 0xFFFF_FFFF_0000_0000);
    assert_eq!(c.reg(4), 0);
}

#[test]
fn delay_slots_and_likely_branches() {
    let mut ram = Ram::new();
    let base = 0x1000;
    ram.load(
        base,
        &[
            i(9, 0, 1, 1),            // 0: addiu r1,r0,1
            i(5, 1, 0, 2),            // 4: bne r1,r0,+2 (target = 0x10)
            i(9, 0, 2, 7),            // 8: delay slot: addiu r2,r0,7 (always runs)
            i(9, 0, 3, 9),            // c: skipped
            i(9, 0, 4, 5),            // 10: addiu r4,r0,5
            i(20, 0, 0, 2),           // 14: beql r0,r0,+2 (taken: delay slot runs)
            i(9, 0, 5, 1),            // 18: delay slot: r5 = 1
            0,
            i(22, 1, 0, 3),           // 20: blezl r1 (r1=1 > 0: not taken: delay slot skipped)
            i(9, 0, 6, 1),            // 24: skipped delay slot
            i(9, 0, 7, 3),            // 28: r7 = 3
        ],
    );
    let mut c = cpu_at(0x8000_0000 | base);
    run(&mut c, &mut ram, 9);
    assert_eq!(c.reg(2), 7, "the delay slot of a taken branch runs");
    assert_eq!(c.reg(3), 0, "the instruction after the delay slot is skipped");
    assert_eq!(c.reg(4), 5);
    assert_eq!(c.reg(5), 1);
    assert_eq!(c.reg(6), 0, "a not-taken likely branch skips its delay slot");
    assert_eq!(c.reg(7), 3);
}

#[test]
fn multiply_divide_and_the_second_unit() {
    let mut ram = Ram::new();
    let base = 0x1000;
    ram.load(
        base,
        &[
            i(9, 0, 1, 0xFFFD),                              // r1 = -3
            i(9, 0, 2, 5),                                   // r2 = 5
            r(24, 1, 2, 3, 0),                               // mult r3,r1,r2: lo = -15, rd = lo
            r(18, 0, 0, 4, 0),                               // mflo r4
            r(16, 0, 0, 5, 0),                               // mfhi r5 (sign extension of high word)
            r(26, 2, 1, 0, 0),                               // div r2,r1: 5 / -3 = -1 rem 2
            r(18, 0, 0, 6, 0),                               // mflo r6
            r(16, 0, 0, 7, 0),                               // mfhi r7
            0x7000_0000 | (2 << 21) | (1 << 16) | 24,        // mult1 r0,r2,r1 (MMI): lo1 = -15
            0x7000_0000 | (8 << 11) | 18,                    // mflo1 r8
        ],
    );
    let mut c = cpu_at(0x8000_0000 | base);
    run(&mut c, &mut ram, 10);
    assert_eq!(c.reg(3) as i64, -15);
    assert_eq!(c.reg(4) as i64, -15);
    assert_eq!(c.reg(5) as i64, -1);
    assert_eq!(c.reg(6) as i64, -1);
    assert_eq!(c.reg(7), 2);
    assert_eq!(c.reg(8) as i64, -15);
}

#[test]
fn quadword_loads_stores_and_the_scratchpad() {
    let mut ram = Ram::new();
    let base = 0x1000;
    ram.load(
        base,
        &[
            i(15, 0, 1, 0x7000),     // lui r1,0x7000 (scratchpad)
            i(9, 0, 2, 0x1234),      // r2 = 0x1234
            i(63, 1, 2, 0),          // sd r2,0(r1)
            i(63, 1, 2, 8),          // sd r2,8(r1)
            i(30, 1, 3, 0),          // lq r3,0(r1)
            i(31, 1, 3, 16),         // sq r3,16(r1)
            i(55, 1, 4, 16),         // ld r4,16(r1)
            i(55, 1, 5, 24),         // ld r5,24(r1)
        ],
    );
    let mut c = cpu_at(0x8000_0000 | base);
    run(&mut c, &mut ram, 8);
    assert_eq!(c.reg(3), 0x1234);
    assert_eq!(c.hi_half[3], 0x1234);
    assert_eq!(c.reg(4), 0x1234);
    assert_eq!(c.reg(5), 0x1234);
}

#[test]
fn syscall_takes_an_exception_and_eret_returns() {
    let mut ram = Ram::new();
    // Handler at 0x80000180 (physical 0x180): r1 += 1; eret
    ram.load(0x180, &[i(9, 1, 1, 1), 0x4200_0018]);
    ram.load(0x1000, &[i(9, 0, 1, 10), r(12, 0, 0, 0, 0), i(9, 1, 1, 100)]);
    let mut c = cpu_at(0x8000_0000 | 0x1000);
    run(&mut c, &mut ram, 2);
    assert_eq!(c.pc, 0x8000_0180, "now in the exception handler");
    assert_eq!(c.cop0[EPC], 0x8000_1004);
    assert_eq!((c.cop0[CAUSE] >> 2) & 31, exc::SYS);
    run(&mut c, &mut ram, 2); // addiu, eret
    assert_eq!(c.pc, 0x8000_1004, "eret returns to the syscall's address");
    // The syscall is re-executed here (a real kernel would add 4 to EPC); step over it.
    c.set_pc(0x8000_1008);
    c.cop0[STATUS] &= !ST_EXL;
    run(&mut c, &mut ram, 1);
    assert_eq!(c.reg(1), 111);
}

#[test]
fn a_fault_in_a_delay_slot_reports_the_branch() {
    let mut ram = Ram::new();
    ram.load(0x1000, &[i(4, 0, 0, 4), r(12, 0, 0, 0, 0)]); // beq r0,r0 ; syscall in the delay slot
    let mut c = cpu_at(0x8000_1000);
    run(&mut c, &mut ram, 2);
    assert_eq!(c.cop0[EPC], 0x8000_1000, "EPC is the branch itself");
    assert!(c.cop0[CAUSE] & (1 << 31) != 0, "Cause.BD is set");
}

#[test]
fn the_tlb_maps_user_pages() {
    let mut ram = Ram::new();
    // Map virtual 0x0040_0000 (even page) -> physical 0x2000; the odd page -> 0x3000.
    ram.load(0x2000, &[i(9, 0, 1, 77)]);
    let mut c = cpu_at(0x0040_0000);
    c.cop0[ENTRYHI] = 0x0040_0000;
    c.cop0[ENTRYLO0] = (0x2000 >> 12) << 6 | 0b0000_0111; // PFN, C=0, D, V, G
    c.cop0[ENTRYLO1] = (0x3000 >> 12) << 6 | 0b0000_0111;
    c.cop0[PAGEMASK] = 0;
    c.cop0[INDEX] = 3;
    c.cop0[WIRED] = 0;
    // tlbwi through the instruction itself.
    ram.load(0x1000, &[0x4200_0002]);
    c.set_pc(0x8000_1000);
    run(&mut c, &mut ram, 1);
    c.set_pc(0x0040_0000);
    run(&mut c, &mut ram, 1);
    assert_eq!(c.reg(1), 77, "fetched through the TLB");
    // An unmapped user address raises a refill exception to the TLB vector.
    let mut d = cpu_at(0x0050_0000);
    run(&mut d, &mut ram, 1);
    assert_eq!((d.cop0[CAUSE] >> 2) & 31, exc::TLBL);
    assert_eq!(d.pc, 0x8000_0000);
}

#[test]
fn floating_point_follows_playstation_2_rules() {
    let mut ram = Ram::new();
    let base = 0x1000;
    ram.load(
        base,
        &[
            i(15, 0, 1, 0x3F80),         // lui r1,0x3F80 (1.0f)
            r(0, 0, 0, 0, 0) | 0,        // nop
            0x4480_0000 | (1 << 16),     // mtc1 r1,f0   (rs=4, rt=1, fs=0)
            0x4480_0800 | (1 << 16),     // mtc1 r1,f1
            0x4600_0000 | (16 << 21) | (1 << 16) | (0 << 11) | (2 << 6), // add.s f2,f0,f1
            i(15, 0, 3, 0x7F7F),         // lui r3,0x7F7F
            i(13, 3, 3, 0xFFFF),         // ori r3,r3,0xFFFF (largest finite)
            0x4480_1800 | (3 << 16),     // mtc1 r3,f3
            0x4600_0000 | (16 << 21) | (3 << 16) | (3 << 11) | (4 << 6) | 2, // mul.s f4,f3,f3 (overflow)
            0x4400_0000 | (2 << 11) | (5 << 16) , // mfc1 r5,f2
        ],
    );
    let mut c = cpu_at(0x8000_0000 | base);
    run(&mut c, &mut ram, 10);
    assert_eq!(f32::from_bits(c.fpr[2]), 2.0);
    assert_eq!(c.fpr[4], 0x7F7F_FFFF, "overflow clamps to the largest finite value");
    assert_eq!(c.reg(5), 0x4000_0000);
}

#[test]
fn parallel_instructions_work_on_all_lanes() {
    let mut ram = Ram::new();
    let base = 0x1000;
    let mmi0 = |sel: u32, rs: u32, rt: u32, rd: u32| 0x7000_0000 | rs << 21 | rt << 16 | rd << 11 | sel << 6 | 8;
    let mmi3 = |sel: u32, rs: u32, rt: u32, rd: u32| 0x7000_0000 | rs << 21 | rt << 16 | rd << 11 | sel << 6 | 41;
    ram.load(
        base,
        &[
            i(9, 0, 1, 1),        // r1 = 1
            i(9, 0, 2, 2),        // r2 = 2
            i(55, 0, 0, 0) & 0,   // nop (placeholder)
            mmi0(0, 1, 2, 3),     // paddw r3,r1,r2
            mmi3(18, 1, 2, 4),    // por r4,r1,r2
        ],
    );
    let mut c = cpu_at(0x8000_0000 | base);
    // Fill the upper halves so lanes 2 and 3 are exercised too.
    c.hi_half[1] = 0x0000_0010_0000_0020;
    c.hi_half[2] = 0x0000_0001_0000_0001;
    run(&mut c, &mut ram, 5);
    assert_eq!(c.reg(3), 3);
    assert_eq!(c.hi_half[3], 0x0000_0011_0000_0021);
    assert_eq!(c.reg(4), 3);
}
