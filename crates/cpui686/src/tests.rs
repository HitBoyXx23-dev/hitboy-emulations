use super::*;

struct Ram {
    m: Vec<u8>,
    out: Vec<(u16, u32)>,
}
impl Bus for Ram {
    fn read8(&mut self, a: u64) -> u8 {
        let Ok(a) = u32::try_from(a) else { return 0xFF };

        self.m.get(a as usize).copied().unwrap_or(0xFF)
    }
    fn write8(&mut self, a: u64, v: u8) {
        let Ok(a) = u32::try_from(a) else { return };

        if let Some(b) = self.m.get_mut(a as usize) {
            *b = v;
        }
    }
    fn io_out(&mut self, port: u16, _size: u8, v: u32) {
        self.out.push((port, v));
    }
}

/// A CPU already in flat 32-bit protected mode, code at 0x1000, stack at 0x8000.
fn flat(code: &[u8]) -> (Cpu, Ram) {
    let mut ram = Ram { m: vec![0; 0x40_0000], out: vec![] };
    ram.m[0x1000..0x1000 + code.len()].copy_from_slice(code);
    // GDT at 0x100: null, code 0x08, data 0x10.
    let gdt: [u64; 3] = [0, 0x00CF_9A00_0000_FFFF, 0x00CF_9200_0000_FFFF];
    for (i, d) in gdt.iter().enumerate() {
        ram.m[0x100 + i * 8..0x108 + i * 8].copy_from_slice(&d.to_le_bytes());
    }
    let mut c = Cpu::new();
    c.gdtr = (0x100, 23);
    c.cr0 |= 1;
    c.load_seg(&mut ram, CS, 8).unwrap();
    for s in [DS, ES, SS, FS, GS] {
        c.load_seg(&mut ram, s, 0x10).unwrap();
    }
    c.rip = 0x1000;
    c.r[ESP] = 0x8000;
    (c, ram)
}

fn run(c: &mut Cpu, ram: &mut Ram, max: usize) {
    for _ in 0..max {
        if c.halted {
            return;
        }
        c.step(ram);
    }
    panic!("did not halt; eip={:#x}", c.rip);
}

#[test]
fn reset_vector() {
    let c = Cpu::new();
    assert_eq!(c.base[CS].wrapping_add(c.rip), 0xFFFF_FFF0);
}

#[test]
fn arithmetic_and_flags() {
    // mov eax,0xFFFFFFFF; add eax,1; setc bl; setz bh; mov ecx,5; imul ecx,ecx,-3; hlt
    let (mut c, mut ram) = flat(&[0xB8, 0xFF, 0xFF, 0xFF, 0xFF, 0x83, 0xC0, 0x01, 0x0F, 0x92, 0xC3, 0x0F, 0x94, 0xC7, 0xB9, 5, 0, 0, 0, 0x6B, 0xC9, 0xFD, 0xF4]);
    run(&mut c, &mut ram, 100);
    assert_eq!(c.r[EAX], 0);
    assert_eq!(c.r[EBX] & 0xFFFF, 0x0101);
    assert_eq!(c.r[ECX] as i32, -15);
}

#[test]
fn loop_call_and_stack() {
    // xor eax,eax; mov ecx,10; l: add eax,ecx; loop l; call f; hlt; f: inc eax; ret
    let (mut c, mut ram) = flat(&[0x31, 0xC0, 0xB9, 10, 0, 0, 0, 0x01, 0xC8, 0xE2, 0xFC, 0xE8, 1, 0, 0, 0, 0xF4, 0x40, 0xC3]);
    run(&mut c, &mut ram, 200);
    assert_eq!(c.r[EAX], 56);
    assert_eq!(c.r[ESP], 0x8000);
}

#[test]
fn divide_and_multiply() {
    // mov eax,100; mov edx,0; mov ecx,7; div ecx; mov ebx,eax; mov eax,-50; cdq; idiv ecx; hlt
    let (mut c, mut ram) = flat(&[0xB8, 100, 0, 0, 0, 0xBA, 0, 0, 0, 0, 0xB9, 7, 0, 0, 0, 0xF7, 0xF1, 0x89, 0xC3, 0xB8, 0xCE, 0xFF, 0xFF, 0xFF, 0x99, 0xF7, 0xF9, 0xF4]);
    run(&mut c, &mut ram, 100);
    assert_eq!(c.r[EBX], 14);
    assert_eq!(c.r[EAX] as i32, -7);
    assert_eq!(c.r[EDX] as i32, -1);
}

#[test]
fn rep_movsb_and_scas() {
    // mov esi,0x2000; mov edi,0x3000; mov ecx,5; rep movsb;
    // mov edi,0x3000; mov al,'l'; mov ecx,5; repne scasb; hlt
    let (mut c, mut ram) = flat(&[
        0xBE, 0, 0x20, 0, 0, 0xBF, 0, 0x30, 0, 0, 0xB9, 5, 0, 0, 0, 0xF3, 0xA4, 0xBF, 0, 0x30, 0, 0, 0xB0, b'l', 0xB9, 5, 0, 0, 0, 0xF2, 0xAE, 0xF4,
    ]);
    ram.m[0x2000..0x2005].copy_from_slice(b"hello");
    run(&mut c, &mut ram, 100);
    assert_eq!(&ram.m[0x3000..0x3005], b"hello");
    assert_eq!(c.r[EDI], 0x3003); // stopped just past the first 'l'
    assert_eq!(c.r[ECX], 2);
}

#[test]
fn shifts_and_bits() {
    // mov eax,0x80000001; rol eax,1; mov ebx,0xF0; bsf ecx,ebx; bsr edx,ebx; sar eax,1 ... hlt
    let (mut c, mut ram) = flat(&[0xB8, 1, 0, 0, 0x80, 0xD1, 0xC0, 0xBB, 0xF0, 0, 0, 0, 0x0F, 0xBC, 0xCB, 0x0F, 0xBD, 0xD3, 0xF4]);
    run(&mut c, &mut ram, 100);
    assert_eq!(c.r[EAX], 3);
    assert_eq!(c.r[ECX], 4);
    assert_eq!(c.r[EDX], 7);
}

#[test]
fn divide_error_goes_through_idt() {
    // IDT at 0x200 with vector 0 -> 0x1100 (interrupt gate). Handler: mov ebx,0xDEAD; hlt
    let (mut c, mut ram) = flat(&[0x31, 0xC9, 0xF7, 0xF1, 0xF4]);
    let gate: u64 = 0x1100 | (8 << 16) | (0x8E00u64 << 32);
    ram.m[0x200..0x208].copy_from_slice(&gate.to_le_bytes());
    ram.m[0x1100..0x1106].copy_from_slice(&[0xBB, 0xAD, 0xDE, 0, 0, 0xF4]);
    c.idtr = (0x200, 0x7FF);
    run(&mut c, &mut ram, 100);
    assert_eq!(c.r[EBX], 0xDEAD);
    // The pushed return address is the faulting DIV itself.
    assert_eq!(ram.m[0x8000 - 12..0x8000 - 8], 0x1002u32.to_le_bytes());
}

#[test]
fn paging_maps_and_faults() {
    // Page directory at 0x10000, table at 0x11000 identity-maps the first 4 MB except
    // page 0x5000 which maps to 0x6000. Then: mov eax,[0x5000]; hlt
    let (mut c, mut ram) = flat(&[0xA1, 0, 0x50, 0, 0, 0xF4]);
    ram.m[0x10000..0x10004].copy_from_slice(&(0x11000u32 | 3).to_le_bytes());
    for p in 0..1024u32 {
        let target = if p == 5 { 0x6000 } else { p << 12 };
        let e = 0x11000 + p as usize * 4;
        ram.m[e..e + 4].copy_from_slice(&(target | 3).to_le_bytes());
    }
    ram.m[0x6000..0x6004].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    c.cr3 = 0x10000;
    c.cr0 |= 0x8000_0000;
    run(&mut c, &mut ram, 10);
    assert_eq!(c.r[EAX], 0x1234_5678);

    // Unmapped page -> #PF with CR2 set.
    let (mut c, mut ram) = flat(&[0xA1, 0, 0, 0x40, 0, 0xF4]);
    ram.m[0x10000..0x10004].copy_from_slice(&(0x11000u32 | 3).to_le_bytes());
    for p in 0..1024u32 {
        let e = 0x11000 + p as usize * 4;
        ram.m[e..e + 4].copy_from_slice(&((p << 12) | 3).to_le_bytes());
    }
    let gate: u64 = 0x1100 | (8 << 16) | (0x8E00u64 << 32);
    ram.m[0x200 + 14 * 8..0x208 + 14 * 8].copy_from_slice(&gate.to_le_bytes());
    ram.m[0x1100] = 0xF4;
    c.idtr = (0x200, 0x7FF);
    c.cr3 = 0x10000;
    c.cr0 |= 0x8000_0000;
    run(&mut c, &mut ram, 10);
    assert_eq!(c.cr2, 0x40_0000);
    assert_eq!(c.rip, 0x1101);
}

#[test]
fn real_mode_to_protected_mode() {
    // Real mode at 0000:7C00:
    //   lgdt [0x7C40]; mov eax,cr0; or al,1; mov cr0,eax; jmp 0x08:0x7C20
    // 0x7C20 (32-bit): mov ax,0x10; mov ds,ax; mov dword [0x9000],0xC0DE; out 0xE9,al; hlt
    let mut ram = Ram { m: vec![0; 0x10_0000], out: vec![] };
    let rm = [0x0F, 0x01, 0x16, 0x40, 0x7C, 0x0F, 0x20, 0xC0, 0x0C, 0x01, 0x0F, 0x22, 0xC0, 0x66, 0xEA, 0x20, 0x7C, 0, 0, 0x08, 0x00];
    ram.m[0x7C00..0x7C00 + rm.len()].copy_from_slice(&rm);
    let pm = [0x66, 0xB8, 0x10, 0x00, 0x8E, 0xD8, 0xC7, 0x05, 0, 0x90, 0, 0, 0xDE, 0xC0, 0, 0, 0xE6, 0xE9, 0xF4];
    ram.m[0x7C20..0x7C20 + pm.len()].copy_from_slice(&pm);
    // GDT descriptor at 0x7C40 -> GDT at 0x7C50.
    ram.m[0x7C40..0x7C42].copy_from_slice(&23u16.to_le_bytes());
    ram.m[0x7C42..0x7C46].copy_from_slice(&0x7C50u32.to_le_bytes());
    let gdt: [u64; 3] = [0, 0x00CF_9A00_0000_FFFF, 0x00CF_9200_0000_FFFF];
    for (i, d) in gdt.iter().enumerate() {
        ram.m[0x7C50 + i * 8..0x7C58 + i * 8].copy_from_slice(&d.to_le_bytes());
    }
    let mut c = Cpu::new();
    c.load_seg(&mut ram, CS, 0).unwrap();
    c.rip = 0x7C00;
    run(&mut c, &mut ram, 100);
    assert!(c.protected() && c.cs32);
    assert_eq!(&ram.m[0x9000..0x9004], &0xC0DEu32.to_le_bytes());
    assert_eq!(ram.out, vec![(0xE9, 0x10)]);
}

#[test]
fn cpuid_reports_pentium_iii() {
    let (mut c, mut ram) = flat(&[0x31, 0xC0, 0x0F, 0xA2, 0xF4]);
    run(&mut c, &mut ram, 10);
    let v: Vec<u8> = [c.r[EBX], c.r[EDX], c.r[ECX]].iter().flat_map(|r| (*r as u32).to_le_bytes()).collect();
    assert_eq!(&v, b"GenuineIntel");
}

#[test]
fn x87_arithmetic_and_store() {
    // fld dword [0x2000] (2.5); fld dword [0x2004] (4.0); fmulp; fsqrt; fistp dword [0x2008];
    // fldpi; fstp qword [0x2010]; fld1; fld1; fcomip st1; setz al; hlt
    let (mut c, mut ram) = flat(&[
        0xD9, 0x05, 0, 0x20, 0, 0, 0xD9, 0x05, 4, 0x20, 0, 0, 0xDE, 0xC9, 0xD9, 0xFA, 0xDB, 0x1D, 8, 0x20, 0, 0, 0xD9, 0xEB, 0xDD, 0x1D, 0x10, 0x20, 0, 0, 0xD9, 0xE8,
        0xD9, 0xE8, 0xDF, 0xF1, 0x0F, 0x94, 0xC0, 0xF4,
    ]);
    ram.m[0x2000..0x2004].copy_from_slice(&2.5f32.to_le_bytes());
    ram.m[0x2004..0x2008].copy_from_slice(&4.0f32.to_le_bytes());
    run(&mut c, &mut ram, 100);
    // sqrt(10) = 3.16 -> rounds to 3
    assert_eq!(u32::from_le_bytes(ram.m[0x2008..0x200C].try_into().unwrap()), 3);
    assert_eq!(f64::from_le_bytes(ram.m[0x2010..0x2018].try_into().unwrap()), std::f64::consts::PI);
    assert_eq!(c.r[EAX] & 0xFF, 1);
}

#[test]
fn f80_round_trip() {
    for v in [0.0, 1.0, -2.5, 1e300, 1e-300, 5e-324, f64::INFINITY, 123456.789] {
        assert_eq!(fpu::from_f80(&fpu::to_f80(v)), v, "{v}");
    }
    // 1.0 in extended precision: exponent 0x3FFF, explicit integer bit.
    assert_eq!(fpu::to_f80(1.0), [0, 0, 0, 0, 0, 0, 0, 0x80, 0xFF, 0x3F]);
}

#[test]
fn fstsw_after_compare() {
    // fld1; fldz; fcompp (0 < 1 -> C0); fnstsw ax; sahf; setc bl; hlt
    let (mut c, mut ram) = flat(&[0xD9, 0xE8, 0xD9, 0xEE, 0xDE, 0xD9, 0xDF, 0xE0, 0x9E, 0x0F, 0x92, 0xC3, 0xF4]);
    run(&mut c, &mut ram, 100);
    assert_eq!(c.r[EBX] & 0xFF, 1);
    assert_eq!(c.fpu.top, 0);
}

#[test]
fn sse_packed_math() {
    // movups xmm0,[0x2000]; movups xmm1,[0x2010]; mulps xmm0,xmm1; addss xmm0,xmm1;
    // shufps xmm0,xmm0,0x1B (reverse); movups [0x2020],xmm0; cvttss2si eax,xmm0;
    // movmskps ebx,xmm1; hlt
    let (mut c, mut ram) = flat(&[
        0x0F, 0x10, 0x05, 0, 0x20, 0, 0, 0x0F, 0x10, 0x0D, 0x10, 0x20, 0, 0, 0x0F, 0x59, 0xC1, 0xF3, 0x0F, 0x58, 0xC1, 0x0F, 0xC6, 0xC0, 0x1B, 0x0F, 0x11, 0x05, 0x20,
        0x20, 0, 0, 0xF3, 0x0F, 0x2C, 0xC0, 0x0F, 0x50, 0xD9, 0xF4,
    ]);
    for (i, v) in [1.0f32, 2.0, 3.0, 4.0, 10.0, -1.0, 0.5, -2.0].iter().enumerate() {
        ram.m[0x2000 + i * 4..0x2004 + i * 4].copy_from_slice(&v.to_le_bytes());
    }
    run(&mut c, &mut ram, 100);
    let out: Vec<f32> = (0..4).map(|i| f32::from_le_bytes(ram.m[0x2020 + i * 4..0x2024 + i * 4].try_into().unwrap())).collect();
    // [1*10+10, 2*-1, 3*0.5, 4*-2] = [20, -2, 1.5, -8], reversed.
    assert_eq!(out, vec![-8.0, 1.5, -2.0, 20.0]);
    assert_eq!(c.r[EAX] as i32, -8);
    assert_eq!(c.r[EBX], 0b1010);
}

#[test]
fn mmx_saturating_and_pack() {
    // movq mm0,[0x2000]; movq mm1,[0x2008]; paddusb mm0,mm1; movq [0x2010],mm0;
    // packsswb mm1,mm1; movd eax,mm1; emms; hlt
    let (mut c, mut ram) = flat(&[0x0F, 0x6F, 0x05, 0, 0x20, 0, 0, 0x0F, 0x6F, 0x0D, 8, 0x20, 0, 0, 0x0F, 0xDC, 0xC1, 0x0F, 0x7F, 0x05, 0x10, 0x20, 0, 0, 0x0F, 0x63, 0xC9, 0x0F, 0x7E, 0xC8, 0x0F, 0x77, 0xF4]);
    ram.m[0x2000..0x2008].copy_from_slice(&[250, 1, 2, 3, 4, 5, 6, 7]);
    // words: 300, -200, 5, -5
    for (i, w) in [300i16, -200, 5, -5].iter().enumerate() {
        ram.m[0x2008 + i * 2..0x200A + i * 2].copy_from_slice(&w.to_le_bytes());
    }
    run(&mut c, &mut ram, 100);
    assert_eq!(ram.m[0x2010], 255); // 250 + 44 (low byte of 300) saturates
    assert_eq!((c.r[EAX] as u32).to_le_bytes(), [127, 0x80, 5, 0xFB]);
    assert_eq!(c.fpu.empty, 0xFF);
}

#[test]
fn sse2_is_undefined_on_pentium_iii() {
    // 66 0F 58 C1 (addpd) -> #UD through IDT vector 6.
    let (mut c, mut ram) = flat(&[0x66, 0x0F, 0x58, 0xC1, 0xF4]);
    let gate: u64 = 0x1100 | (8 << 16) | (0x8E00u64 << 32);
    ram.m[0x200 + 6 * 8..0x208 + 6 * 8].copy_from_slice(&gate.to_le_bytes());
    ram.m[0x1100] = 0xF4;
    c.idtr = (0x200, 0x7FF);
    run(&mut c, &mut ram, 10);
    assert_eq!(c.rip, 0x1101);
}

// ---------------------------------------------------------------- 64-bit (x86-64 model)

/// A 64-bit CPU in long mode at CPL 0: identity-mapped 4 MB (2 MB pages; the second page is
/// supervisor-only), code at 0x1000, stack at 0x8000, TSS with RSP0 = 0x9000.
fn long_mode(code: &[u8]) -> (Cpu, Ram) {
    let mut ram = Ram { m: vec![0; 0x40_0000], out: vec![] };
    ram.m[0x1000..0x1000 + code.len()].copy_from_slice(code);
    let put = |ram: &mut Ram, a: usize, v: u64| ram.m[a..a + 8].copy_from_slice(&v.to_le_bytes());
    put(&mut ram, 0x10000, 0x11000 | 7); // PML4 -> PDPT
    put(&mut ram, 0x11000, 0x12000 | 7); // PDPT -> PD
    put(&mut ram, 0x12000, 0x87); // 0-2 MB: present, writable, user, 2 MB
    put(&mut ram, 0x12008, 0x0020_0000 | 0x83); // 2-4 MB: supervisor only
    // GDT: kernel code64 0x08, kernel data 0x10, user code32 0x18, user data 0x20,
    // user code64 0x28, TSS 0x30 (16 bytes).
    let gdt = [0u64, 0x00AF_9A00_0000_FFFF, 0x00CF_9200_0000_FFFF, 0x00CF_FA00_0000_FFFF, 0x00CF_F200_0000_FFFF, 0x00AF_FA00_0000_FFFF];
    for (i, d) in gdt.iter().enumerate() {
        put(&mut ram, 0x100 + i * 8, *d);
    }
    let tss: u64 = 0x5000;
    put(&mut ram, 0x130, 0x67 | (tss & 0xFF_FFFF) << 16 | 0x89u64 << 40 | (tss >> 24 & 0xFF) << 56);
    put(&mut ram, 0x138, tss >> 32);
    put(&mut ram, tss as usize + 4, 0x9000); // RSP0
    let mut c = Cpu::new_x64();
    c.gdtr = (0x100, 0x3F);
    c.idtr = (0x6000, 0xFFF);
    c.cr0 |= 1;
    c.cr4 = 0x20; // PAE
    c.cr3 = 0x10000;
    c.efer = EFER_LME | EFER_SCE | EFER_NXE;
    let cr0 = c.cr0 | 0x8000_0000;
    c.write_cr(0, cr0).unwrap();
    assert!(c.long_mode());
    c.load_seg(&mut ram, CS, 0x08).unwrap();
    c.load_seg(&mut ram, SS, 0x10).unwrap();
    c.load_system(&mut ram, 0x30, true).unwrap();
    assert!(c.code64);
    c.rip = 0x1000;
    c.r[ESP] = 0x8000;
    (c, ram)
}

/// 64-bit interrupt gate for `vec` -> `handler` (kernel code segment).
fn gate64(ram: &mut Ram, vec: usize, handler: u64) {
    let lo = (handler & 0xFFFF) | 0x08 << 16 | 0x8E00u64 << 32 | (handler >> 16 & 0xFFFF) << 48;
    let a = 0x6000 + vec * 16;
    ram.m[a..a + 8].copy_from_slice(&lo.to_le_bytes());
    ram.m[a + 8..a + 16].copy_from_slice(&(handler >> 32).to_le_bytes());
}

#[test]
fn long_mode_registers_and_addressing() {
    let code = [
        0x48, 0xB8, 0xF0, 0xDE, 0xBC, 0x9A, 0x78, 0x56, 0x34, 0x12, // mov rax, 0x123456789ABCDEF0
        0x49, 0x89, 0xC1, // mov r9, rax
        0x4D, 0x01, 0xC9, // add r9, r9
        0x41, 0x51, // push r9
        0x5B, // pop rbx
        0x48, 0x8D, 0x0D, 0x10, 0x00, 0x00, 0x00, // lea rcx, [rip+0x10]
        0xBA, 0xFF, 0xFF, 0xFF, 0xFF, // mov edx, -1 (zero-extends)
        0x48, 0x63, 0xF2, // movsxd rsi, edx
        0x40, 0xB7, 0x77, // mov dil, 0x77 (REX byte register)
        0x48, 0xC7, 0x05, 0x00, 0x10, 0x00, 0x00, 0xFE, 0xFF, 0xFF, 0xFF, // mov qword [rip+0x1000], -2
        0xF4,
    ];
    let (mut c, mut ram) = long_mode(&code);
    run(&mut c, &mut ram, 100);
    assert_eq!(c.r[EBX], 0x2468_ACF1_3579_BDE0);
    assert_eq!(c.r[ECX], 0x1000 + 26 + 0x10);
    assert_eq!(c.r[EDX], 0xFFFF_FFFF);
    assert_eq!(c.r[ESI], u64::MAX);
    assert_eq!(c.r[EDI] & 0xFF, 0x77);
    assert_eq!(c.r[ESP], 0x8000);
    // RIP-relative with a trailing imm32 counts from the end of the 11-byte instruction.
    let at = 0x1025 + 11 + 0x1000;
    assert_eq!(u64::from_le_bytes(ram.m[at..at + 8].try_into().unwrap()), (-2i64) as u64);
}

#[test]
fn syscall_sysret_round_trip() {
    let mut code = vec![
        0x48, 0xC7, 0xC1, 0x00, 0x20, 0x00, 0x00, // mov rcx, 0x2000 (user RIP)
        0x49, 0xC7, 0xC3, 0x02, 0x02, 0x00, 0x00, // mov r11, 0x202 (user RFLAGS)
        0x48, 0x0F, 0x07, // sysretq
    ];
    code.resize(0x1000, 0x90);
    code.extend([0xB8, 42, 0, 0, 0, 0x0F, 0x05]); // user @0x2000: mov eax,42; syscall
    code.resize(0x2000, 0x90);
    code.extend([0x48, 0x89, 0xC3, 0xF4]); // kernel @0x3000: mov rbx, rax; hlt
    let (mut c, mut ram) = long_mode(&code);
    c.write_msr(msr::STAR, 0x0018_0008u64 << 32);
    c.write_msr(msr::LSTAR, 0x3000);
    c.write_msr(msr::SFMASK, IF as u64);
    for _ in 0..3 {
        c.step(&mut ram);
    }
    assert_eq!((c.cpl, c.sel[CS], c.sel[SS]), (3, 0x2B, 0x23));
    assert!(c.code64);
    run(&mut c, &mut ram, 20);
    assert_eq!(c.r[EBX], 42);
    assert_eq!(c.cpl, 0);
    assert_eq!(c.r[ECX], 0x2007); // return address saved by SYSCALL
    assert_eq!(c.r[11] & IF as u64, IF as u64);
    assert_eq!(c.eflags & IF, 0);
}

#[test]
fn user_write_to_kernel_page_faults_onto_rsp0() {
    let mut code = vec![
        0x48, 0xC7, 0xC1, 0x00, 0x20, 0x00, 0x00, // mov rcx, 0x2000
        0x49, 0xC7, 0xC3, 0x02, 0x00, 0x00, 0x00, // mov r11, 2
        0x48, 0x0F, 0x07, // sysretq
    ];
    code.resize(0x1000, 0x90);
    code.extend([0xC6, 0x04, 0x25, 0x00, 0x00, 0x20, 0x00, 0x55]); // user: mov byte [0x200000], 0x55
    code.resize(0x2000, 0x90);
    code.extend([0x0F, 0x20, 0xD3, 0xF4]); // #PF handler @0x3000: mov rbx, cr2; hlt
    let (mut c, mut ram) = long_mode(&code);
    c.write_msr(msr::STAR, 0x0018_0008u64 << 32);
    gate64(&mut ram, 14, 0x3000);
    run(&mut c, &mut ram, 30);
    assert_eq!(c.r[EBX], 0x20_0000);
    assert_eq!(c.cpl, 0);
    assert_eq!(ram.m[0x20_0000], 0, "the write must not happen");
    // Frame on the RSP0 stack: error code (present | write | user), RIP, CS, RFLAGS, RSP, SS.
    let q = |a: usize| u64::from_le_bytes(ram.m[a..a + 8].try_into().unwrap());
    assert_eq!(c.r[ESP], 0x9000 - 48);
    assert_eq!(q(0x9000 - 48), 0b111);
    assert_eq!(q(0x9000 - 40), 0x2000);
    assert_eq!(q(0x9000 - 32), 0x2B);
    assert_eq!(q(0x9000 - 8), 0x23);
}

#[test]
fn iretq_returns_to_user_mode() {
    // IRETQ frame by hand: SS=0x23, RSP=0x7000, RFLAGS=0x202, CS=0x2B, RIP=0x2000.
    let mut code = vec![0x6A, 0x23, 0x68, 0x00, 0x70, 0x00, 0x00, 0x68, 0x02, 0x02, 0x00, 0x00, 0x6A, 0x2B, 0x68, 0x00, 0x20, 0x00, 0x00, 0x48, 0xCF];
    code.resize(0x1000, 0x90);
    code.extend([0x48, 0x89, 0xE3, 0xEB, 0xFE]); // user: mov rbx, rsp; jmp $
    let (mut c, mut ram) = long_mode(&code);
    for _ in 0..7 {
        c.step(&mut ram);
    }
    assert_eq!((c.cpl, c.rip), (3, 0x2003));
    assert_eq!(c.r[EBX], 0x7000);
    assert_ne!(c.eflags & IF, 0);
}

#[test]
fn sse2_integer_and_double() {
    // Flat 32-bit mode on the 64-bit model:
    // movdqu xmm0,[0x2000]; movdqu xmm1,[0x2010]; paddq xmm0,xmm1; movdqu [0x2020],xmm0;
    // pcmpeqb xmm1,xmm1 (all ones); pmovmskb eax,xmm1;
    // mov ecx,7; cvtsi2sd xmm2,ecx; mulsd xmm2,xmm2; cvttsd2si edx,xmm2; hlt
    let code = [
        0xF3, 0x0F, 0x6F, 0x05, 0x00, 0x20, 0, 0, 0xF3, 0x0F, 0x6F, 0x0D, 0x10, 0x20, 0, 0, 0x66, 0x0F, 0xD4, 0xC1, 0xF3, 0x0F, 0x7F, 0x05, 0x20, 0x20, 0, 0, 0x66, 0x0F, 0x74,
        0xC9, 0x66, 0x0F, 0xD7, 0xC1, 0xB9, 7, 0, 0, 0, 0xF2, 0x0F, 0x2A, 0xD1, 0xF2, 0x0F, 0x59, 0xD2, 0xF2, 0x0F, 0x2C, 0xD2, 0xF4,
    ];
    let (mut c, mut ram) = flat(&code);
    c.model = Model::X86_64;
    for (i, v) in [u64::MAX, 5, 1, 10].iter().enumerate() {
        ram.m[0x2000 + i * 8..0x2008 + i * 8].copy_from_slice(&v.to_le_bytes());
    }
    run(&mut c, &mut ram, 100);
    let q = |a: usize| u64::from_le_bytes(ram.m[a..a + 8].try_into().unwrap());
    assert_eq!((q(0x2020), q(0x2028)), (0, 15));
    assert_eq!(c.r[EAX], 0xFFFF);
    assert_eq!(c.r[EDX], 49);
}

#[test]
fn x64_cpuid_reports_long_mode() {
    let (mut c, mut ram) = flat(&[0xB8, 0x01, 0x00, 0x00, 0x80, 0x0F, 0xA2, 0xF4]);
    c.model = Model::X86_64;
    run(&mut c, &mut ram, 10);
    assert_ne!(c.r[EDX] & (1 << 29), 0);
}

#[test]
fn vex_vector_moves_preserve_extended_registers_and_addresses() {
    let code = [
        0xC5, 0xF8, 0x29, 0x7D, 0xF0,
        0xC5, 0x78, 0x28, 0x45, 0xF0,
        0xC4, 0x41, 0x7A, 0x7F, 0x01,
        0xC4, 0x41, 0x79, 0x6F, 0x09,
        0xF4,
    ];
    let (mut cpu, mut ram) = long_mode(&code);
    cpu.r[5] = 0x3000;
    cpu.r[9] = 0x3100;
    let value = 0x0123456789ABCDEF_FEDCBA9876543210u128;
    cpu.simd.xmm[7] = value;
    run(&mut cpu, &mut ram, 6);
    assert!(cpu.shutdown.is_none());
    assert!(cpu.halted);
    assert_eq!(cpu.simd.xmm[8], value);
    assert_eq!(cpu.simd.xmm[9], value);
    assert_eq!(&ram.m[0x3100..0x3110], &value.to_le_bytes());
}

#[test]
fn vex_moves_reject_unsupported_width_and_reserved_source() {
    for code in [[0xC5, 0xFE, 0x10, 0xC1], [0xC5, 0xF0, 0x28, 0xC1]] {
        let (mut cpu, mut ram) = long_mode(&code);
        assert!(matches!(cpu.exec(&mut ram), Err(UD)));
    }
}

#[test]
fn vex_scalar_moves_merge_registers_and_clear_memory_loads() {
    let code = [0xC5, 0xF2, 0x10, 0xC2, 0xC5, 0xFA, 0x11, 0x03, 0xC5, 0xFA, 0x10, 0x1B, 0xF4];
    let (mut cpu, mut ram) = long_mode(&code);
    cpu.r[3] = 0x3000;
    cpu.simd.xmm[1] = u128::MAX;
    cpu.simd.xmm[2] = 0x12345678;
    cpu.simd.xmm[3] = u128::MAX;
    run(&mut cpu, &mut ram, 5);
    assert_eq!(cpu.simd.xmm[0], (u128::MAX << 32) | 0x12345678);
    assert_eq!(cpu.simd.xmm[3], 0x12345678);
    assert_eq!(&ram.m[0x3000..0x3004], &0x12345678u32.to_le_bytes());
}

#[test]
fn bextr_extracts_fields_and_sets_zero_flag() {
    for (value, control, expected) in [(0xABCDEu64, 0x414u64, 0), (0xABCDE, 0x408, 0xC), (u64::MAX, 0xFF20, 0)] {
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, 0x70, 0xF7, 0xC0]);
        cpu.r[0] = value;
        cpu.r[1] = control;
        cpu.eflags |= CF | OF;
        assert!(cpu.exec(&mut ram).is_ok());
        assert_eq!(cpu.r[0], expected);
        assert_eq!(cpu.eflags & ZF != 0, expected == 0);
        assert_eq!(cpu.eflags & (CF | OF), 0);
    }
}

#[test]
fn vex_scalar_conversion_and_division_use_three_operands() {
    let code = [0xC5, 0xFB, 0x2A, 0xC0, 0xC5, 0xF3, 0x2A, 0xC9, 0xC5, 0xFB, 0x5E, 0xC1, 0xC5, 0xFB, 0x2C, 0xD0, 0xF4];
    let (mut cpu, mut ram) = long_mode(&code);
    cpu.r[0] = 1920;
    cpu.r[1] = 640;
    cpu.simd.xmm[0] = 0x1234u128 << 64;
    run(&mut cpu, &mut ram, 6);
    assert_eq!(cpu.r[2], 3);
    assert_eq!(cpu.simd.xmm[0] >> 64, 0x1234);
}

#[test]
fn vex_add_preserves_aliased_second_operand() {
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF3, 0x58, 0xC0]);
    cpu.simd.xmm[0] = 2.0f64.to_bits() as u128;
    cpu.simd.xmm[1] = (0x1234u128 << 64) | 5.0f64.to_bits() as u128;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(f64::from_bits(cpu.simd.xmm[0] as u64), 7.0);
    assert_eq!(cpu.simd.xmm[0] >> 64, 0x1234);
}

#[test]
fn vex_rounding_uses_immediate_and_mxcsr_modes() {
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x71, 0x0B, 0xC0, 9]);
    cpu.simd.xmm[0] = (-2.25f64).to_bits() as u128;
    cpu.simd.xmm[1] = 0x1234u128 << 64;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(f64::from_bits(cpu.simd.xmm[0] as u64), -3.0);
    assert_eq!(cpu.simd.xmm[0] >> 64, 0x1234);
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x71, 0x0B, 0xC0, 4]);
    cpu.simd.mxcsr |= 2 << 13;
    cpu.simd.xmm[0] = 2.25f64.to_bits() as u128;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(f64::from_bits(cpu.simd.xmm[0] as u64), 3.0);
}

#[test]
fn vex_256_moves_preserve_both_halves_and_128_writes_clear_upper() {
    let code = [0xC5, 0x7C, 0x10, 0x03, 0xC5, 0x7C, 0x11, 0x43, 0x20, 0xC5, 0x78, 0x10, 0x03, 0xF4];
    let (mut cpu, mut ram) = long_mode(&code);
    cpu.r[3] = 0x3000;
    let bytes: Vec<u8> = (0..32).collect();
    ram.m[0x3000..0x3020].copy_from_slice(&bytes);
    run(&mut cpu, &mut ram, 5);
    assert_eq!(&ram.m[0x3020..0x3040], bytes.as_slice());
    assert_eq!(cpu.simd.xmm[8], u128::from_le_bytes(bytes[..16].try_into().unwrap()));
    assert_eq!(cpu.simd.ymm_hi[8], 0);
}

#[test]
fn vzeroupper_keeps_xmm_and_clears_ymm_upper_halves() {
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF8, 0x77]);
    cpu.simd.xmm.fill(42);
    cpu.simd.ymm_hi.fill(u128::MAX);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm, [42; 16]);
    assert_eq!(cpu.simd.ymm_hi, [0; 16]);
}

#[test]
fn vex_integer_move_and_byte_shuffle_broadcast_with_zero_mask() {
    let code = [0xC5, 0xF9, 0x6E, 0xC6, 0xC5, 0xF1, 0xEF, 0xC9, 0xC4, 0xE2, 0x79, 0x00, 0xC1];
    let (mut cpu, mut ram) = long_mode(&code);
    cpu.r[6] = 0x123456AB;
    cpu.simd.ymm_hi[0] = u128::MAX;
    for _ in 0..3 { assert!(cpu.exec(&mut ram).is_ok()); }
    assert_eq!(cpu.simd.xmm[0], u128::from_le_bytes([0xAB; 16]));
    assert_eq!(cpu.simd.ymm_hi[0], 0);
    cpu.simd.xmm[1] = u128::from_le_bytes([0x80; 16]);
    cpu.rip -= 5;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], 0);
}

#[test]
fn bit_counts_handle_zero_and_operand_width() {
    for (prefix, width) in [(vec![0xF3, 0x48], 64u64), (vec![0xF3], 32)] {
        for opcode in [0xBC, 0xBD] {
            for value in [0u64, 1, 0x80000000] {
                let mut code = prefix.clone();
                code.extend_from_slice(&[0x0F, opcode, 0xC1]);
                let (mut cpu, mut ram) = long_mode(&code);
                cpu.r[1] = value;
                assert!(cpu.exec(&mut ram).is_ok());
                let expected = if value == 0 { width } else if opcode == 0xBC { value.trailing_zeros() as u64 } else { value.leading_zeros() as u64 - (64 - width) };
                assert_eq!(cpu.r[0], expected);
                assert_eq!(cpu.eflags & CF != 0, value == 0);
                assert_eq!(cpu.eflags & ZF != 0, expected == 0);
            }
        }
    }
}

#[test]
fn andn_uses_complemented_first_source_and_sets_flags() {
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, 0xF8, 0xF2, 0xCA]);
    cpu.r[0] = 0xF0;
    cpu.r[2] = 0x80000000000000FF;
    cpu.eflags |= CF | OF;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.r[1], 0x800000000000000F);
    assert_eq!(cpu.eflags & (CF | OF | ZF), 0);
    assert_ne!(cpu.eflags & SF, 0);
}

#[test]
fn vex_byte_equality_and_test_set_architectural_flags() {
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xE9, 0x74, 0xC1, 0xC4, 0xE2, 0x79, 0x17, 0xC0]);
    cpu.simd.xmm[1] = u128::from_le_bytes([7; 16]);
    cpu.simd.xmm[2] = u128::from_le_bytes([8; 16]);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], 0);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.eflags & (ZF | CF), ZF | CF);
    cpu.rip -= 5;
    cpu.simd.xmm[0] = u128::MAX;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.eflags & (ZF | CF), CF);
}

#[test]
fn vex_string_index_finds_matches_terminators_and_explicit_mismatches() {
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x79, 0x63, 0xC8, 0]);
    cpu.simd.xmm[1] = b'x' as u128;
    cpu.simd.xmm[0] = u128::from_le_bytes(*b"abcxdefghijklmno");
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.r[1], 3);
    assert_ne!(cpu.eflags & CF, 0);
    assert_eq!(cpu.eflags & ZF, 0);
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x79, 0x63, 0xC0, 0x38]);
    cpu.simd.xmm[0] = u128::from_le_bytes(*b"abcd\0efghijklmno");
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.r[1], 4);
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x79, 0x61, 0xC8, 0x18]);
    cpu.r[0] = 3;
    cpu.r[2] = 3;
    cpu.simd.xmm[1] = u128::from_le_bytes(*b"abcXXXXXXXXXXXXX");
    cpu.simd.xmm[0] = u128::from_le_bytes(*b"axcYYYYYYYYYYYYY");
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.r[1], 1);
    assert_ne!(cpu.eflags & (CF | ZF | SF), 0);
}

#[test]
fn vex_unpack_captures_both_sources_before_overwriting_destination() {
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF1, 0x6C, 0xC0]);
    cpu.simd.xmm[0] = (99u128 << 64) | 42;
    cpu.simd.xmm[1] = (88u128 << 64) | 17;
    cpu.simd.ymm_hi[0] = u128::MAX;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], (42u128 << 64) | 17);
    assert_eq!(cpu.simd.ymm_hi[0], 0);
}

#[test]
fn vex_dword_shuffle_uses_immediate_and_clears_upper_vector() {
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF9, 0x70, 0xC0, 0x44]);
    cpu.simd.xmm[0] = (4u128 << 96) | (3u128 << 64) | (2u128 << 32) | 1;
    cpu.simd.ymm_hi[0] = u128::MAX;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], (2u128 << 96) | (1u128 << 64) | (2u128 << 32) | 1);
    assert_eq!(cpu.simd.ymm_hi[0], 0);
}

#[test]
fn vex_signed_expansion_minimum_shift_and_add_keep_lanes_separate() {
    let code = [0xC4, 0xE2, 0x79, 0x21, 0xF3, 0xC4, 0xE2, 0x49, 0x3B, 0xE1, 0xC5, 0xD9, 0x71, 0xF2, 5, 0xC5, 0xD9, 0xFC, 0xE3];
    let (mut cpu, mut ram) = long_mode(&code);
    cpu.simd.xmm[3] = 0x7F80FF01;
    cpu.simd.xmm[1] = (100u128 << 96) | (100u128 << 64) | (100u128 << 32) | 100;
    cpu.simd.xmm[2] = 1;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[6], (127u128 << 96) | (0xFFFFFF80u128 << 64) | (0xFFFFFFFFu128 << 32) | 1);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[4], (100u128 << 96) | (100u128 << 64) | (100u128 << 32) | 1);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[4], 32);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[4], 0x7F80FF21);
}

#[test]
fn bmi_lowest_bit_operations_handle_zero_width_and_carry() {
    for (wide, control) in [(false, 0x70), (true, 0xF0)] {
        for group in 1..=3 {
            for source in [0u64, 0x58, 0x80000000, 0x8000000000000000] {
                let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, control, 0xF3, 0xC2 | (group << 3)]);
                cpu.r[2] = source;
                let source = if wide { source } else { source as u32 as u64 };
                let result = match group { 1 => source & source.wrapping_sub(1), 2 => source ^ source.wrapping_sub(1), _ => source & source.wrapping_neg() };
                let result = if wide { result } else { result as u32 as u64 };
                assert!(cpu.exec(&mut ram).is_ok());
                assert_eq!(cpu.r[1], result);
                assert_eq!(cpu.eflags & CF != 0, if group == 3 { source != 0 } else { source == 0 });
                assert_eq!(cpu.eflags & ZF != 0, result == 0);
                assert_eq!(cpu.eflags & OF, 0);
            }
        }
    }
}

#[test]
fn vex_lane_permutation_swaps_memory_halves_and_honors_zero_controls() {
    for (mode, expected) in [(0x23, (4, 3)), (0x01, (2, 1)), (0x88, (0, 0))] {
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x7D, 6, 3, mode]);
        cpu.r[3] = 0x3000;
        ram.m[0x3000..0x3010].copy_from_slice(&3u128.to_le_bytes());
        ram.m[0x3010..0x3020].copy_from_slice(&4u128.to_le_bytes());
        cpu.simd.xmm[0] = 1;
        cpu.simd.ymm_hi[0] = 2;
        assert!(cpu.exec(&mut ram).is_ok());
        assert_eq!((cpu.simd.xmm[0], cpu.simd.ymm_hi[0]), expected);
    }
}

#[test]
fn vex_zero_extension_qword_comparison_and_byte_extraction() {
    let code = [0xC4, 0xE2, 0x79, 0x35, 0xC0, 0xC4, 0xE2, 0x71, 0x29, 0xC0, 0xC4, 0xE3, 0x79, 0x14, 0xC0, 0, 0xC4, 0xE3, 0x79, 0x14, 0xC1, 8];
    let (mut cpu, mut ram) = long_mode(&code);
    cpu.simd.xmm[0] = (u64::MAX as u128) << 64 | 0xFFFFFFFF80000000;
    cpu.simd.xmm[1] = (1u128 << 64) | 0x80000000;
    cpu.r[0] = u64::MAX;
    cpu.r[1] = u64::MAX;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], (0xFFFFFFFFu128 << 64) | 0x80000000);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], u64::MAX as u128);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.r[0], 255);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.r[1], 0);
}

#[test]
fn wide_vector_logic_and_extraction_preserve_aliases_and_memory_bounds() {
    let code = [0xC5, 0xFC, 0x54, 0x0B, 0xC4, 0xE3, 0x7D, 0x19, 0xC9, 1, 0xC4, 0xE3, 0x7D, 0x19, 0x03, 1];
    let (mut cpu, mut ram) = long_mode(&code);
    cpu.r[3] = 0x3000;
    ram.m[0x3000..0x3010].copy_from_slice(&0xFFu128.to_le_bytes());
    ram.m[0x3010..0x3020].copy_from_slice(&0xF0F0u128.to_le_bytes());
    cpu.simd.xmm[0] = 0x1234;
    cpu.simd.ymm_hi[0] = 0x1234;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!((cpu.simd.xmm[1], cpu.simd.ymm_hi[1]), (0x34, 0x1030));
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!((cpu.simd.xmm[1], cpu.simd.ymm_hi[1]), (0x1030, 0));
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(&ram.m[0x3000..0x3010], &0x1234u128.to_le_bytes());
    assert_eq!(&ram.m[0x3010..0x3020], &0xF0F0u128.to_le_bytes());
}

#[test]
fn vex_insert_zeros_selected_lanes_and_low_move_stores_only_eight_bytes() {
    let code = [0xC4, 0xE3, 0x71, 0x21, 0xC2, 0x98, 0xC5, 0xF8, 0x13, 0x03];
    let (mut cpu, mut ram) = long_mode(&code);
    cpu.r[3] = 0x3000;
    ram.m[0x3000..0x3010].fill(0xCC);
    cpu.simd.xmm[1] = (4u128 << 96) | (3u128 << 64) | (2u128 << 32) | 1;
    cpu.simd.xmm[2] = 7u128 << 64;
    cpu.simd.ymm_hi[0] = u128::MAX;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], (3u128 << 64) | (7u128 << 32) | 1);
    assert_eq!(cpu.simd.ymm_hi[0], 0);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(&ram.m[0x3000..0x3008], &((7u64 << 32) | 1).to_le_bytes());
    assert_eq!(&ram.m[0x3008..0x3010], &[0xCC; 8]);
}

#[test]
fn streaming_vector_moves_copy_the_exact_width_and_enforce_alignment() {
    for (load, store, width) in [(0x79, 0xF9, 16), (0x7D, 0xFD, 32)] {
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, load, 0x2A, 3, 0xC5, store, 0xE7, 7]);
        cpu.r[3] = 0x3000;
        cpu.r[7] = 0x3040;
        let bytes: Vec<u8> = (0..32).collect();
        ram.m[0x3000..0x3020].copy_from_slice(&bytes);
        ram.m[0x3040..0x3070].fill(0xCC);
        assert!(cpu.exec(&mut ram).is_ok());
        assert!(cpu.exec(&mut ram).is_ok());
        assert_eq!(&ram.m[0x3040..0x3040+width], &bytes[..width]);
        assert_eq!(ram.m[0x3040+width], 0xCC);
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, load, 0x2A, 3]);
        cpu.r[3] = 0x3001;
        assert!(matches!(cpu.exec(&mut ram), Err(Ex(13, _))));
    }
}

#[test]
fn vex_duplicate_moves_select_the_correct_lanes() {
    for (prefix, opcode, expected) in [(0xFA, 0x16, [2u32, 2, 4, 4]), (0xFA, 0x12, [1, 1, 3, 3]), (0xFB, 0x12, [1, 2, 1, 2])] {
        let (mut cpu, mut ram) = long_mode(&[0xC5, prefix, opcode, 0xC0]);
        cpu.simd.xmm[0] = (4u128 << 96) | (3u128 << 64) | (2u128 << 32) | 1;
        cpu.simd.ymm_hi[0] = u128::MAX;
        assert!(cpu.exec(&mut ram).is_ok());
        let expected = expected.iter().enumerate().fold(0u128, |value, (index, lane)| value | (*lane as u128) << (index * 32));
        assert_eq!(cpu.simd.xmm[0], expected);
        assert_eq!(cpu.simd.ymm_hi[0], 0);
    }
}

#[test]
fn wide_unpack_and_lane_insertion_capture_aliased_inputs() {
    let code = [0xC5, 0xFD, 0x14, 0xC9, 0xC4, 0xE3, 0x7D, 0x18, 0xC1, 1, 0xC5, 0xFA, 0x7E, 0xD0];
    let (mut cpu, mut ram) = long_mode(&code);
    cpu.simd.xmm[0] = (2u128 << 64) | 1;
    cpu.simd.ymm_hi[0] = (4u128 << 64) | 3;
    cpu.simd.xmm[1] = (6u128 << 64) | 5;
    cpu.simd.ymm_hi[1] = (8u128 << 64) | 7;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[1], (5u128 << 64) | 1);
    assert_eq!(cpu.simd.ymm_hi[1], (7u128 << 64) | 3);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], (2u128 << 64) | 1);
    assert_eq!(cpu.simd.ymm_hi[0], (5u128 << 64) | 1);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!((cpu.simd.xmm[2], cpu.simd.ymm_hi[2]), (1, 0));
}

#[test]
fn vector_blending_uses_each_mask_sign_bit_even_when_the_mask_is_the_destination() {
    for control in [0x79, 0x7D] {
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, control, 0x4A, 0xD1, 0x20]);
        let left = (4u128 << 96) | (3u128 << 64) | (2u128 << 32) | 1;
        let right = (8u128 << 96) | (7u128 << 64) | (6u128 << 32) | 5;
        cpu.simd.xmm[0] = left;
        cpu.simd.ymm_hi[0] = left;
        cpu.simd.xmm[1] = right;
        cpu.simd.ymm_hi[1] = right;
        cpu.simd.xmm[2] = (0x80000000u128 << 96) | (1u128 << 64) | (0x80000000u128 << 32);
        cpu.simd.ymm_hi[2] = u128::MAX;
        assert!(cpu.exec(&mut ram).is_ok());
        assert_eq!(cpu.simd.xmm[2], (8u128 << 96) | (3u128 << 64) | (6u128 << 32) | 1);
        assert_eq!(cpu.simd.ymm_hi[2], if control == 0x7D { right } else { 0 });
    }
}

#[test]
fn vex_qword_store_targets_xmm_registers_and_only_eight_memory_bytes() {
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF9, 0xD6, 0xC1, 0xC5, 0xF9, 0xD6, 7]);
    cpu.simd.xmm[0] = (u64::MAX as u128) << 64 | 42;
    cpu.simd.xmm[1] = u128::MAX;
    cpu.simd.ymm_hi[1] = u128::MAX;
    cpu.r[1] = 99;
    cpu.r[7] = 0x3000;
    ram.m[0x3000..0x3010].fill(0xCC);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!((cpu.simd.xmm[1], cpu.simd.ymm_hi[1]), (42, 0));
    assert_eq!(cpu.r[1], 99);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(&ram.m[0x3000..0x3008], &42u64.to_le_bytes());
    assert_eq!(&ram.m[0x3008..0x3010], &[0xCC; 8]);
}

#[test]
fn vector_broadcast_duplicates_one_memory_lane_and_rejects_invalid_forms() {
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, 0x7D, 0x1A, 3]);
    cpu.r[3] = 0x3000;
    ram.m[0x3000..0x3010].copy_from_slice(&42u128.to_le_bytes());
    ram.m[0x3010..0x3020].copy_from_slice(&99u128.to_le_bytes());
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!((cpu.simd.xmm[0], cpu.simd.ymm_hi[0]), (42, 42));
    for code in [[0xC4, 0xE2, 0x79, 0x1A, 3], [0xC4, 0xE2, 0x7D, 0x1A, 0xC0]] {
        let (mut cpu, mut ram) = long_mode(&code);
        assert!(matches!(cpu.exec(&mut ram), Err(UD)));
    }
}

#[test]
fn vector_comparison_masks_nan_status_and_scalar_merging_are_preserved() {
    let pack = |values: [u32; 4]| values.iter().enumerate().fold(0u128, |all, (index, value)| all | (*value as u128) << (index * 32));
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF4, 0xC2, 0xC2, 2]);
    cpu.simd.xmm[1] = pack([1f32.to_bits(), 2f32.to_bits(), f32::NAN.to_bits(), 4f32.to_bits()]);
    cpu.simd.xmm[2] = pack([2f32.to_bits(), 2f32.to_bits(), 3f32.to_bits(), f32::NAN.to_bits()]);
    cpu.simd.ymm_hi[1] = pack([1f32.to_bits(); 4]);
    cpu.simd.ymm_hi[2] = pack([2f32.to_bits(); 4]);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], u64::MAX as u128);
    assert_eq!(cpu.simd.ymm_hi[0], u128::MAX);
    assert_ne!(cpu.simd.mxcsr & 1, 0);
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF2, 0xC2, 0xC2, 0]);
    cpu.simd.xmm[1] = u128::MAX << 32 | 1f32.to_bits() as u128;
    cpu.simd.xmm[2] = 1f32.to_bits() as u128;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], u128::MAX);
    assert_eq!(cpu.simd.ymm_hi[0], 0);
}

#[test]
fn signed_dword_expansion_ignores_unused_upper_source_bits() {
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, 0x79, 0x25, 0xC0]);
    cpu.simd.xmm[0] = u128::MAX << 64 | 0x80000000FFFFFFFF;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], (0xFFFFFFFF80000000u128 << 64) | u64::MAX as u128);
}

#[cfg(target_arch = "x86_64")]
#[test]
fn vector_reciprocals_match_native_special_values_and_approximation_bounds() {
    use std::arch::x86_64::{_mm_loadu_ps, _mm_storeu_ps, _mm_rcp_ps, _mm_rsqrt_ps};
    for opcode in [0x52, 0x53] {
        for values in [[4.0, 3.0, 0.0, -0.0], [f32::MAX, f32::from_bits(0x007FFFFF), f32::INFINITY, -1.0], [f32::NAN, f32::from_bits(0x7F800001), f32::NEG_INFINITY, f32::from_bits(0x807FFFFF)]] {
            let mut native = [0f32; 4];
            unsafe {
                let input = _mm_loadu_ps(values.as_ptr());
                let output = if opcode == 0x52 { _mm_rsqrt_ps(input) } else { _mm_rcp_ps(input) };
                _mm_storeu_ps(native.as_mut_ptr(), output);
            }
            let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF8, opcode, 0xC0]);
            cpu.simd.xmm[0] = values.iter().enumerate().fold(0u128, |all, (index, value)| all | (value.to_bits() as u128) << (index * 32));
            let status = cpu.simd.mxcsr;
            assert!(cpu.exec(&mut ram).is_ok());
            assert_eq!(cpu.simd.mxcsr, status);
            for (index, expected) in native.into_iter().enumerate() {
                let value = f32::from_bits((cpu.simd.xmm[0] >> (index * 32)) as u32);
                if expected.is_nan() { assert!(value.is_nan()); }
                else if expected == 0.0 || expected.is_infinite() { assert_eq!(value.to_bits(), expected.to_bits()); }
                else { assert!((value - expected).abs() <= expected.abs() * 0.001); }
            }
        }
    }
}

#[test]
fn unmasked_vector_comparison_reports_invalid_without_overwriting_the_destination() {
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF0, 0xC2, 0xC2, 0]);
    cpu.cr4 |= 1 << 10;
    cpu.simd.mxcsr &= !0x80;
    cpu.simd.xmm[0] = 42;
    cpu.simd.xmm[1] = 0x7F800001;
    cpu.simd.xmm[2] = 1f32.to_bits() as u128;
    assert!(matches!(cpu.exec(&mut ram), Err(Ex(19, _))));
    assert_ne!(cpu.simd.mxcsr & 1, 0);
    assert_eq!(cpu.simd.xmm[0], 42);
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF0, 0xC2, 0xC2, 8]);
    assert!(matches!(cpu.exec(&mut ram), Err(UD)));
}

#[test]
fn immediate_vector_blends_permutations_and_shuffles_select_the_correct_sources() {
    let pack = |values: [u32; 4]| values.iter().enumerate().fold(0u128, |all, (index, value)| all | (*value as u128) << (index * 32));
    let code = [0xC4, 0xE3, 0x79, 0x0C, 0xC1, 0x0A, 0xC4, 0xE3, 0x79, 4, 0xC0, 0x4E, 0xC5, 0xF8, 0xC6, 0xC1, 0x50];
    let (mut cpu, mut ram) = long_mode(&code);
    cpu.simd.xmm[0] = pack([1, 2, 3, 4]);
    cpu.simd.xmm[1] = pack([5, 6, 7, 8]);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], pack([1, 6, 3, 8]));
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], pack([3, 8, 1, 6]));
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], pack([3, 3, 6, 6]));
    assert_eq!(cpu.simd.ymm_hi[0], 0);
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x7D, 5, 0xC1, 9, 0xC5, 0xFD, 0xC6, 0xC9, 9]);
    cpu.simd.xmm[1] = (6u128 << 64) | 5;
    cpu.simd.ymm_hi[1] = (8u128 << 64) | 7;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], (5u128 << 64) | 6);
    assert_eq!(cpu.simd.ymm_hi[0], (8u128 << 64) | 7);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[1], (5u128 << 64) | 5);
    assert_eq!(cpu.simd.ymm_hi[1], (8u128 << 64) | 7);
}

#[test]
fn vex_extractps_preserves_float_bits_and_writes_only_the_selected_dword() {
    let code = [0xC4, 0xE3, 0x79, 0x17, 0xC1, 3, 0xC4, 0xE3, 0x79, 0x17, 7, 2];
    let (mut cpu, mut ram) = long_mode(&code);
    let vector = (0x3F800000u128 << 96) | (0x40A00000u128 << 64) | (0xBF800000u128 << 32) | 0x7FC00000;
    cpu.simd.xmm[0] = vector;
    cpu.simd.ymm_hi[0] = u128::MAX;
    cpu.r[1] = u64::MAX;
    cpu.r[7] = 0x3000;
    ram.m[0x3000..0x3010].fill(0xCC);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.r[1], 0x3F800000);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(&ram.m[0x3000..0x3004], &0x40A00000u32.to_le_bytes());
    assert_eq!(&ram.m[0x3004..0x3010], &[0xCC; 12]);
    assert_eq!(cpu.simd.xmm[0], vector);
    assert_eq!(cpu.simd.ymm_hi[0], u128::MAX);
}

#[test]
fn scalar_double_broadcast_reads_only_eight_bytes_at_the_page_boundary() {
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, 0x7D, 0x19, 3]);
    cpu.r[3] = 0x1FFFF8;
    ram.m[0x1FFFF8..0x200000].copy_from_slice(&42u64.to_le_bytes());
    assert!(cpu.exec(&mut ram).is_ok());
    let expected = (42u128 << 64) | 42;
    assert_eq!(cpu.simd.xmm[0], expected);
    assert_eq!(cpu.simd.ymm_hi[0], expected);
}

#[test]
fn vex_unsigned_widening_matches_native_sse_and_handles_aliases() {
    let source = 0xFFFF80017FFF00FE_FFFFFFFF80017F80u128;
    for (opcode, source_width, destination_width) in [(0x30, 1, 2), (0x31, 1, 4), (0x32, 1, 8), (0x33, 2, 4), (0x34, 2, 8), (0x35, 4, 8)] {
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, 0x79, opcode, 0xC0]);
        cpu.simd.xmm[0] = source;
        cpu.simd.ymm_hi[0] = u128::MAX;
        let source_mask = (1u128 << (source_width * 8)) - 1;
        let mut expected = 0u128;
        for lane in 0..16 / destination_width { expected |= ((source >> (lane * source_width * 8)) & source_mask) << (lane * destination_width * 8); }
        assert!(cpu.exec(&mut ram).is_ok());
        assert_eq!(cpu.simd.xmm[0], expected, "opcode {opcode:x}");
        assert_eq!(cpu.simd.ymm_hi[0], 0);
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("sse4.1") {
            use std::arch::x86_64::*;
            let native: u128 = unsafe {
                let input = std::mem::transmute::<u128, __m128i>(source);
                std::mem::transmute(match opcode {
                    0x30 => _mm_cvtepu8_epi16(input),
                    0x31 => _mm_cvtepu8_epi32(input),
                    0x32 => _mm_cvtepu8_epi64(input),
                    0x33 => _mm_cvtepu16_epi32(input),
                    0x34 => _mm_cvtepu16_epi64(input),
                    _ => _mm_cvtepu32_epi64(input),
                })
            };
            assert_eq!(cpu.simd.xmm[0], native);
        }
    }
}

#[test]
fn vex_unsigned_widening_reads_only_its_narrow_source() {
    for (opcode, source_size) in [(0x30, 8), (0x31, 4), (0x32, 2), (0x33, 8), (0x34, 4), (0x35, 8)] {
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, 0x79, opcode, 3]);
        cpu.r[3] = 0x200000 - source_size;
        ram.m[cpu.r[3] as usize..0x200000].fill(0xFF);
        assert!(cpu.exec(&mut ram).is_ok(), "opcode {opcode:x}");
        assert_ne!(cpu.simd.xmm[0], 0);
        assert_eq!(cpu.simd.ymm_hi[0], 0);
    }
}

#[test]
fn vex_unsigned_widening_rejects_reserved_source_and_unsupported_width() {
    for opcode in 0x30..=0x35 {
        for control in [0x71, 0x7D] {
            let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, control, opcode, 0xC0]);
            assert!(matches!(cpu.exec(&mut ram), Err(UD)));
        }
    }
}

#[test]
fn vex_integer_to_float_respects_rounding_and_precision_status() {
    let source = [16777217i32, -16777217, i32::MAX, i32::MIN];
    let input = u128::from_le_bytes(source.into_iter().flat_map(i32::to_le_bytes).collect::<Vec<_>>().try_into().unwrap());
    let expected = [
        [0x4B800000u32, 0xCB800000, 0x4F000000, 0xCF000000],
        [0x4B800000, 0xCB800001, 0x4EFFFFFF, 0xCF000000],
        [0x4B800001, 0xCB800000, 0x4F000000, 0xCF000000],
        [0x4B800000, 0xCB800000, 0x4EFFFFFF, 0xCF000000],
    ];
    for rounding in 0..4 {
        let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF8, 0x5B, 0xC0]);
        cpu.simd.xmm[0] = input;
        cpu.simd.ymm_hi[0] = u128::MAX;
        cpu.simd.mxcsr = 0x1F80 | rounding << 13;
        assert!(cpu.exec(&mut ram).is_ok());
        let result = cpu.simd.xmm[0].to_le_bytes();
        for lane in 0..4 { assert_eq!(&result[lane * 4..lane * 4 + 4], &expected[rounding as usize][lane].to_le_bytes()); }
        assert_eq!(cpu.simd.ymm_hi[0], 0);
        assert_ne!(cpu.simd.mxcsr & 0x20, 0);
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::__m128i;
            let mut old = 0u32;
            let control = 0x1F80u32 | rounding << 13;
            let mut native = std::mem::transmute::<u128, __m128i>(input);
            std::arch::asm!(
                "stmxcsr [{old}]",
                "ldmxcsr [{control}]",
                "cvtdq2ps {vector}, {vector}",
                "ldmxcsr [{old}]",
                old = in(reg) &mut old,
                control = in(reg) &control,
                vector = inout(xmm_reg) native,
                options(nostack)
            );
            assert_eq!(cpu.simd.xmm[0], std::mem::transmute::<__m128i, u128>(native));
        }
    }
}

#[test]
fn vex_integer_to_float_traps_before_overwriting_the_destination() {
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF8, 0x5B, 0xC0]);
    cpu.simd.xmm[0] = 16777217;
    cpu.simd.ymm_hi[0] = u128::MAX;
    cpu.simd.mxcsr &= !0x1000;
    cpu.cr4 |= 1 << 10;
    assert!(matches!(cpu.exec(&mut ram), Err(Ex(19, None))));
    assert_eq!(cpu.simd.xmm[0], 16777217);
    assert_eq!(cpu.simd.ymm_hi[0], u128::MAX);
    assert_ne!(cpu.simd.mxcsr & 0x20, 0);
}

#[test]
#[cfg(target_arch = "x86_64")]
fn vex_horizontal_operations_keep_128_bit_lane_order_and_handle_aliases() {
    for (prefix, width) in [(1, 8), (3, 4)] {
        for wide in [false, true] {
            for subtract in [false, true] {
                let control = 0xF8 | prefix | if wide { 4 } else { 0 };
                let opcode = if subtract { 0x7D } else { 0x7C };
                let (mut cpu, mut ram) = long_mode(&[0xC5, control, opcode, 0xC1]);
                let pack = |values: &[f64]| -> u128 {
                    values.iter().enumerate().fold(0, |result, (index, value)| result | ((if width == 8 { value.to_bits() as u128 } else { (*value as f32).to_bits() as u128 }) << (index * width * 8)))
                };
                let values = if width == 8 { vec![1.0, 4.0] } else { vec![1.0, 4.0, 8.0, 16.0] };
                let other = values.iter().map(|value| value * 10.0).collect::<Vec<_>>();
                cpu.simd.xmm[0] = pack(&values);
                cpu.simd.xmm[1] = pack(&other);
                cpu.simd.ymm_hi[0] = pack(&other);
                cpu.simd.ymm_hi[1] = pack(&values);
                let combine = |a: &[f64], b: &[f64]| a.chunks_exact(2).chain(b.chunks_exact(2)).map(|pair| if subtract { pair[0] - pair[1] } else { pair[0] + pair[1] }).collect::<Vec<_>>();
                assert!(cpu.exec(&mut ram).is_ok());
                assert_eq!(cpu.simd.xmm[0], pack(&combine(&values, &other)));
                assert_eq!(cpu.simd.ymm_hi[0], if wide { pack(&combine(&other, &values)) } else { 0 });
            }
        }
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn vex_horizontal_add_obeys_rounding_and_restores_host_control() {
    let expected = [1.0f64, 1.0, f64::from_bits(1.0f64.to_bits() + 1), 1.0];
    for rounding in 0..4 {
        let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF9, 0x7C, 0xC0]);
        cpu.simd.xmm[0] = 1.0f64.to_bits() as u128 | ((2.0f64.powi(-54).to_bits() as u128) << 64);
        cpu.simd.mxcsr = 0x1F80 | rounding << 13;
        let mut before = 0u32;
        let mut after = 0u32;
        unsafe { std::arch::asm!("stmxcsr [{value}]", value = in(reg) &mut before, options(nostack)); }
        assert!(cpu.exec(&mut ram).is_ok());
        unsafe { std::arch::asm!("stmxcsr [{value}]", value = in(reg) &mut after, options(nostack)); }
        assert_eq!(before, after);
        assert_eq!(cpu.simd.xmm[0] as u64, expected[rounding as usize].to_bits());
        assert_eq!((cpu.simd.xmm[0] >> 64) as u64, expected[rounding as usize].to_bits());
        assert_ne!(cpu.simd.mxcsr & 0x20, 0);
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn vex_horizontal_invalid_exception_preserves_destination() {
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF9, 0x7C, 0xC0]);
    let input = f64::INFINITY.to_bits() as u128 | ((f64::NEG_INFINITY.to_bits() as u128) << 64);
    cpu.simd.xmm[0] = input;
    cpu.simd.ymm_hi[0] = u128::MAX;
    cpu.simd.mxcsr &= !0x80;
    cpu.cr4 |= 1 << 10;
    assert!(matches!(cpu.exec(&mut ram), Err(Ex(19, None))));
    assert_eq!(cpu.simd.xmm[0], input);
    assert_eq!(cpu.simd.ymm_hi[0], u128::MAX);
    assert_ne!(cpu.simd.mxcsr & 1, 0);
}

#[test]
#[cfg(target_arch = "x86_64")]
fn vex_string_masks_match_native_for_polarity_and_output_formats() {
    if !std::arch::is_x86_feature_detected!("sse4.2") { return; }
    use std::arch::x86_64::*;
    let inputs = [
        (*b"abc\0defghijklmno", *b"abX\0defghijklmno"),
        ([0u8; 16], [0u8; 16]),
        ([0xFF; 16], [0xFF; 16]),
    ];
    macro_rules! check {
        ($mode:literal) => {
            for (left, right) in inputs {
                let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x79, 0x62, 0xC1, $mode]);
                cpu.simd.xmm[0] = u128::from_le_bytes(left);
                cpu.simd.xmm[1] = u128::from_le_bytes(right);
                cpu.simd.ymm_hi[0] = u128::MAX;
                cpu.r[1] = 0x12345678;
                let expected = unsafe { _mm_cmpistrm::<$mode>(std::mem::transmute(left), std::mem::transmute(right)) };
                assert!(cpu.exec(&mut ram).is_ok());
                assert_eq!(cpu.simd.xmm[0], unsafe { std::mem::transmute::<__m128i, u128>(expected) }, "mode {:x}", $mode);
                assert_eq!(cpu.simd.ymm_hi[0], 0);
                assert_eq!(cpu.r[1], 0x12345678);
            }
        };
    }
    check!(0x00); check!(0x08); check!(0x18); check!(0x28); check!(0x38); check!(0x48); check!(0x68); check!(0x78);
    check!(0x01); check!(0x09); check!(0x19); check!(0x39); check!(0x49); check!(0x79);
}

#[test]
#[cfg(target_arch = "x86_64")]
fn vex_explicit_string_masks_match_native_with_signed_lengths() {
    if !std::arch::is_x86_feature_detected!("sse4.2") { return; }
    use std::arch::x86_64::*;
    let left = u128::from_le_bytes(*b"hello\0world.....");
    let right = u128::from_le_bytes(*b"help!\0there.....");
    for (a, b) in [(3i32, 5i32), (-7, -4), (i32::MIN, i32::MAX)] {
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x79, 0x60, 0xC1, 0x78]);
        cpu.simd.xmm[0] = left;
        cpu.simd.xmm[1] = right;
        cpu.r[0] = a as u32 as u64;
        cpu.r[2] = b as u32 as u64;
        let expected = unsafe { _mm_cmpestrm::<0x78>(std::mem::transmute(left), a, std::mem::transmute(right), b) };
        assert!(cpu.exec(&mut ram).is_ok());
        assert_eq!(cpu.simd.xmm[0], unsafe { std::mem::transmute::<__m128i, u128>(expected) });
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn vex_signed_widening_matches_native_with_aliases_and_narrow_memory_reads() {
    if !std::arch::is_x86_feature_detected!("sse4.1") { return; }
    use std::arch::x86_64::*;
    let input = 0xFFFFFFFF80000001_7FFFFFFF80817FFFu128;
    for (opcode, size) in [(0x20, 8), (0x21, 4), (0x22, 2), (0x23, 8), (0x24, 4), (0x25, 8)] {
        let native = unsafe {
            let vector = std::mem::transmute::<u128, __m128i>(input);
            std::mem::transmute::<__m128i, u128>(match opcode {
                0x20 => _mm_cvtepi8_epi16(vector),
                0x21 => _mm_cvtepi8_epi32(vector),
                0x22 => _mm_cvtepi8_epi64(vector),
                0x23 => _mm_cvtepi16_epi32(vector),
                0x24 => _mm_cvtepi16_epi64(vector),
                _ => _mm_cvtepi32_epi64(vector),
            })
        };
        for memory in [false, true] {
            let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, 0x79, opcode, if memory { 3 } else { 0xC0 }]);
            cpu.simd.xmm[0] = input;
            cpu.simd.ymm_hi[0] = u128::MAX;
            cpu.r[3] = 0x200000 - size;
            ram.m[cpu.r[3] as usize..0x200000].copy_from_slice(&input.to_le_bytes()[..size as usize]);
            assert!(cpu.exec(&mut ram).is_ok());
            assert_eq!(cpu.simd.xmm[0], native);
            assert_eq!(cpu.simd.ymm_hi[0], 0);
        }
    }
}

#[test]
fn vex_integer_extract_preserves_qwords_and_zero_extends_dwords() {
    let input = 0xFEDCBA9876543210_89ABCDEF01234567u128;
    for (control, width, expected) in [(0xF9, 8, 0xFEDCBA9876543210u64), (0x79, 4, 0xFEDCBA98)] {
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, control, 0x16, 0xCB, 0xFF]);
        cpu.simd.xmm[1] = input;
        cpu.simd.ymm_hi[1] = u128::MAX;
        cpu.r[3] = u64::MAX;
        assert!(cpu.exec(&mut ram).is_ok());
        assert_eq!(cpu.r[3], expected);
        assert_eq!(cpu.simd.xmm[1], input);
        assert_eq!(cpu.simd.ymm_hi[1], u128::MAX);
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, control, 0x16, 0x0B, 0xFF]);
        cpu.simd.xmm[1] = input;
        cpu.r[3] = 0x200000 - width;
        ram.m[0x1FFFF0..0x200000].fill(0xCC);
        assert!(cpu.exec(&mut ram).is_ok());
        assert_eq!(&ram.m[cpu.r[3] as usize..0x200000], &expected.to_le_bytes()[..width as usize]);
        assert!(ram.m[0x1FFFF0..cpu.r[3] as usize].iter().all(|byte| *byte == 0xCC));
    }
}

#[test]
fn vex_integer_extract_rejects_reserved_source_and_256_bit_width() {
    for control in [0xF1, 0xFD] {
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, control, 0x16, 0xCB, 1]);
        cpu.r[3] = 0x1234;
        assert!(matches!(cpu.exec(&mut ram), Err(UD)));
        assert_eq!(cpu.r[3], 0x1234);
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn vex_integer_minimum_and_maximum_match_native_signed_and_unsigned_results() {
    if !std::arch::is_x86_feature_detected!("sse4.1") { return; }
    use std::arch::x86_64::*;
    let left = 0xFFFFFFFF80000001_7FFFFFFF80817FFFu128;
    let right = 0x0123456789ABCDEF_FFFF000080000001u128;
    for opcode in 0x38..=0x3F {
        let expected = unsafe {
            let a = std::mem::transmute::<u128, __m128i>(left);
            let b = std::mem::transmute::<u128, __m128i>(right);
            std::mem::transmute::<__m128i, u128>(match opcode {
                0x38 => _mm_min_epi8(a, b),
                0x39 => _mm_min_epi32(a, b),
                0x3A => _mm_min_epu16(a, b),
                0x3B => _mm_min_epu32(a, b),
                0x3C => _mm_max_epi8(a, b),
                0x3D => _mm_max_epi32(a, b),
                0x3E => _mm_max_epu16(a, b),
                _ => _mm_max_epu32(a, b),
            })
        };
        for memory in [false, true] {
            let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, 0x79, opcode, if memory { 3 } else { 0xC1 }]);
            cpu.simd.xmm[0] = left;
            cpu.simd.xmm[1] = right;
            cpu.simd.ymm_hi[0] = u128::MAX;
            cpu.r[3] = 0x1FFFF0;
            ram.m[0x1FFFF0..0x200000].copy_from_slice(&right.to_le_bytes());
            let flags = cpu.eflags;
            assert!(cpu.exec(&mut ram).is_ok());
            assert_eq!(cpu.simd.xmm[0], expected, "opcode {opcode:x}");
            assert_eq!(cpu.simd.ymm_hi[0], 0);
            assert_eq!(cpu.eflags, flags);
        }
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, 0x7D, opcode, 0xC1]);
        assert!(matches!(cpu.exec(&mut ram), Err(UD)));
    }
}

#[test]
fn scalar_broadcast_uses_four_bytes_and_clears_only_the_required_upper_lanes() {
    let bits = 0xFFC12345u32;
    let expected = (bits as u128) * 0x00000001000000010000000100000001u128;
    for wide in [false, true] {
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, if wide { 0x7D } else { 0x79 }, 0x18, 3]);
        cpu.r[3] = 0x1FFFFC;
        ram.m[0x1FFFFC..0x200000].copy_from_slice(&bits.to_le_bytes());
        cpu.simd.xmm[0] = u128::MAX;
        cpu.simd.ymm_hi[0] = u128::MAX;
        let flags = cpu.eflags;
        assert!(cpu.exec(&mut ram).is_ok());
        assert_eq!(cpu.simd.xmm[0], expected);
        assert_eq!(cpu.simd.ymm_hi[0], if wide { expected } else { 0 });
        assert_eq!(cpu.eflags, flags);
    }
    for (control, operand) in [(0x71, 3), (0x79, 0xC1)] {
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, control, 0x18, operand]);
        assert!(matches!(cpu.exec(&mut ram), Err(UD)));
    }
}

#[test]
fn vector_integer_insertion_preserves_source_lanes_and_clears_upper_destination() {
    let original = 0xFFEEDDCCBBAA99887766554433221100u128;
    for width in [4u32, 8] {
        for lane in 0..16 / width {
            for memory in [false, true] {
                let control = if width == 8 { 0xF1 } else { 0x71 };
                let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, control, 0x22, if memory { 3 } else { 0xC0 }, lane as u8 | 0xF0]);
                let value = 0xFEDCBA9876543210u64;
                cpu.r[0] = value;
                cpu.r[3] = 0x200000 - width as u64;
                ram.m[0x200000 - width as usize..0x200000].copy_from_slice(&value.to_le_bytes()[..width as usize]);
                cpu.simd.xmm[1] = original;
                cpu.simd.ymm_hi[1] = 123;
                cpu.simd.ymm_hi[0] = u128::MAX;
                let flags = cpu.eflags;
                assert!(cpu.exec(&mut ram).is_ok());
                let shift = lane * width * 8;
                let bits = mask(width) as u128;
                assert_eq!(cpu.simd.xmm[0], (original & !(bits << shift)) | (((value as u128) & bits) << shift));
                assert_eq!(cpu.simd.ymm_hi[0], 0);
                assert_eq!(cpu.simd.xmm[1], original);
                assert_eq!(cpu.simd.ymm_hi[1], 123);
                assert_eq!(cpu.eflags, flags);
            }
        }
    }
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x75, 0x22, 0xC0, 1]);
    assert!(matches!(cpu.exec(&mut ram), Err(UD)));
}

#[test]
#[cfg(target_arch = "x86_64")]
fn wide_packed_subtraction_uses_both_halves_and_preserves_destination_on_fault() {
    for width in [4, 8] {
        for memory in [false, true] {
            let (mut cpu, mut ram) = long_mode(&[0xC5, if width == 4 { 0xEC } else { 0xED }, 0x5C, if memory { 3 } else { 0xC1 }]);
            let pack = |value: f64| if width == 4 { (value as f32).to_bits() as u128 * 0x00000001000000010000000100000001 } else { value.to_bits() as u128 * 0x10000000000000001 };
            cpu.simd.xmm[2] = pack(9.0);
            cpu.simd.ymm_hi[2] = pack(5.0);
            cpu.simd.xmm[1] = pack(2.0);
            cpu.simd.ymm_hi[1] = pack(3.0);
            cpu.r[3] = 0x1FFFE0;
            ram.m[0x1FFFE0..0x1FFFF0].copy_from_slice(&pack(2.0).to_le_bytes());
            ram.m[0x1FFFF0..0x200000].copy_from_slice(&pack(3.0).to_le_bytes());
            let flags = cpu.eflags;
            assert!(cpu.exec(&mut ram).is_ok());
            assert_eq!(cpu.simd.xmm[0], pack(7.0));
            assert_eq!(cpu.simd.ymm_hi[0], pack(2.0));
            assert_eq!(cpu.simd.xmm[2], pack(9.0));
            assert_eq!(cpu.simd.ymm_hi[2], pack(5.0));
            assert_eq!(cpu.eflags, flags);
        }
    }
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xEC, 0x5C, 0xC1]);
    cpu.simd.xmm[0] = 42;
    cpu.simd.ymm_hi[0] = 43;
    cpu.simd.xmm[2] = f32::INFINITY.to_bits() as u128;
    cpu.simd.xmm[1] = f32::INFINITY.to_bits() as u128;
    cpu.simd.mxcsr &= !0x80;
    cpu.cr4 |= 1 << 10;
    assert!(matches!(cpu.exec(&mut ram), Err(Ex(19, None))));
    assert_eq!(cpu.simd.xmm[0], 42);
    assert_eq!(cpu.simd.ymm_hi[0], 43);
    assert_ne!(cpu.simd.mxcsr & 1, 0);
}

#[test]
#[cfg(target_arch = "x86_64")]
fn wide_packed_addition_preserves_aliases_and_obeys_guest_rounding() {
    let pack = |value: f32| value.to_bits() as u128 * 0x00000001000000010000000100000001;
    for rounding in 0..4 {
        let (mut cpu, mut ram) = long_mode(&[0xC5, 0xFC, 0x58, 0xC1]);
        cpu.simd.xmm[0] = pack(1.0);
        cpu.simd.ymm_hi[0] = pack(3.0);
        cpu.simd.xmm[1] = pack(f32::EPSILON / 2.0);
        cpu.simd.ymm_hi[1] = pack(4.0);
        cpu.simd.mxcsr = 0x1F80 | (rounding << 13);
        assert!(cpu.exec(&mut ram).is_ok());
        assert_eq!(cpu.simd.xmm[0], pack(if rounding == 2 { f32::from_bits(1.0f32.to_bits() + 1) } else { 1.0 }));
        assert_eq!(cpu.simd.ymm_hi[0], pack(7.0));
        assert_ne!(cpu.simd.mxcsr & 0x20, 0);
    }
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xED, 0x58, 0xC1]);
    let pack = |value: f64| value.to_bits() as u128 * 0x10000000000000001;
    cpu.simd.xmm[2] = pack(9.0);
    cpu.simd.ymm_hi[2] = pack(5.0);
    cpu.simd.xmm[1] = pack(2.0);
    cpu.simd.ymm_hi[1] = pack(3.0);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], pack(11.0));
    assert_eq!(cpu.simd.ymm_hi[0], pack(8.0));
}

#[test]
#[cfg(target_arch = "x86_64")]
fn wide_packed_multiplication_and_division_use_all_lanes_and_report_zero_division() {
    for width in [4, 8] {
        for operation in [0x59, 0x5E] {
            let (mut cpu, mut ram) = long_mode(&[0xC5, if width == 4 { 0xEC } else { 0xED }, operation, 0xC1]);
            let pack = |value: f64| if width == 4 { (value as f32).to_bits() as u128 * 0x00000001000000010000000100000001 } else { value.to_bits() as u128 * 0x10000000000000001 };
            cpu.simd.xmm[2] = pack(12.0);
            cpu.simd.ymm_hi[2] = pack(-8.0);
            cpu.simd.xmm[1] = pack(2.0);
            cpu.simd.ymm_hi[1] = pack(4.0);
            assert!(cpu.exec(&mut ram).is_ok());
            assert_eq!(cpu.simd.xmm[0], pack(if operation == 0x59 { 24.0 } else { 6.0 }));
            assert_eq!(cpu.simd.ymm_hi[0], pack(if operation == 0x59 { -32.0 } else { -2.0 }));
        }
    }
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xEC, 0x5E, 0xC1]);
    cpu.simd.xmm[0] = 42;
    cpu.simd.ymm_hi[0] = 43;
    cpu.simd.xmm[2] = 1.0f32.to_bits() as u128 * 0x00000001000000010000000100000001;
    cpu.simd.ymm_hi[2] = cpu.simd.xmm[2];
    cpu.simd.mxcsr &= !0x200;
    cpu.cr4 |= 1 << 10;
    assert!(matches!(cpu.exec(&mut ram), Err(Ex(19, None))));
    assert_eq!(cpu.simd.xmm[0], 42);
    assert_eq!(cpu.simd.ymm_hi[0], 43);
    assert_ne!(cpu.simd.mxcsr & 4, 0);
}

#[test]
fn big_endian_moves_use_exact_widths_extended_registers_and_preserve_flags() {
    let value = 0x0123456789ABCDEFu64;
    for width in [2u32, 4, 8] {
        for load in [false, true] {
            let mut code = Vec::new();
            if width == 2 { code.push(0x66); }
            code.extend_from_slice(&[if width == 8 { 0x4D } else { 0x45 }, 0x0F, 0x38, if load { 0xF0 } else { 0xF1 }, 0x0C, 0x24]);
            let (mut cpu, mut ram) = long_mode(&code);
            cpu.r[12] = 0x200000 - width as u64;
            cpu.r[9] = value;
            let start = cpu.r[12] as usize;
            ram.m[start - 1] = 0xCC;
            ram.m[start + width as usize] = 0xDD;
            ram.m[start..start + width as usize].copy_from_slice(&value.to_le_bytes()[..width as usize]);
            cpu.eflags = 0xAD7;
            let flags = cpu.eflags;
            assert!(cpu.exec(&mut ram).is_ok());
            let swapped = match width { 2 => (value as u16).swap_bytes() as u64, 4 => (value as u32).swap_bytes() as u64, _ => value.swap_bytes() };
            if load {
                assert_eq!(cpu.r[9], if width == 2 { (value & !0xFFFF) | swapped } else { swapped });
            } else {
                assert_eq!(&ram.m[start..start + width as usize], &swapped.to_le_bytes()[..width as usize]);
                assert_eq!(cpu.r[9], value);
            }
            assert_eq!(ram.m[start - 1], 0xCC);
            assert_eq!(ram.m[start + width as usize], 0xDD);
            assert_eq!(cpu.eflags, flags);
        }
    }
    let (mut cpu, mut ram) = long_mode(&[0x0F, 0x38, 0xF0, 0x0D, 0xF8, 7, 0, 0]);
    ram.m[0x1800..0x1804].copy_from_slice(&[1, 2, 3, 4]);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.r[1], 0x01020304);
    for code in [vec![0x0F, 0x38, 0xF0, 0xC1], vec![0xF2, 0x0F, 0x38, 0xF1, 3], vec![0xF0, 0x0F, 0x38, 0xF0, 3]] {
        let (mut cpu, mut ram) = long_mode(&code);
        assert!(matches!(cpu.exec(&mut ram), Err(UD)));
    }
    let (mut cpu, mut ram) = flat(&[0x0F, 0x38, 0xF0, 3]);
    assert!(matches!(cpu.exec(&mut ram), Err(UD)));
    let (mut cpu, mut ram) = flat(&[0x0F, 0x38, 0xF0, 3]);
    cpu.model = Model::X86_64;
    cpu.r[3] = 0x3000;
    ram.m[0x3000..0x3004].copy_from_slice(&[1, 2, 3, 4]);
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.r[0], 0x01020304);
    cpu.r[0] = 1;
    cpu.cpuid();
    assert_ne!(cpu.r[1] & (1 << 22), 0);
}

#[test]
fn word_blends_select_each_lane_preserve_aliases_and_clear_upper_destination() {
    let left = 0x00080007000600050004000300020001u128;
    let right = 0xFFF8FFF7FFF6FFF5FFF4FFF3FFF2FFF1u128;
    for mode in 0..=255u8 {
        for memory in [false, true] {
            let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x79, 0x0E, if memory { 3 } else { 0xC1 }, mode]);
            cpu.simd.xmm[0] = left;
            cpu.simd.xmm[1] = right;
            cpu.simd.ymm_hi[0] = u128::MAX;
            cpu.simd.ymm_hi[1] = 123;
            cpu.r[3] = 0x1FFFF0;
            ram.m[0x1FFFF0..0x200000].copy_from_slice(&right.to_le_bytes());
            let flags = cpu.eflags;
            assert!(cpu.exec(&mut ram).is_ok());
            let mut mask = 0u128;
            for lane in 0..8 { if mode & (1 << lane) != 0 { mask |= 0xFFFFu128 << (lane * 16); } }
            assert_eq!(cpu.simd.xmm[0], (left & !mask) | (right & mask));
            assert_eq!(cpu.simd.ymm_hi[0], 0);
            assert_eq!(cpu.simd.xmm[1], right);
            assert_eq!(cpu.simd.ymm_hi[1], 123);
            assert_eq!(cpu.eflags, flags);
        }
    }
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x71, 0x0E, 5, 0xF6, 7, 0, 0, 0xF0]);
    cpu.simd.xmm[1] = left;
    ram.m[0x1800..0x1810].copy_from_slice(&right.to_le_bytes());
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], (left & u64::MAX as u128) | (right & !(u64::MAX as u128)));
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x75, 0x0E, 0xC1, 0xFF]);
    assert!(matches!(cpu.exec(&mut ram), Err(UD)));
}

#[test]
#[cfg(target_arch = "x86_64")]
fn word_blends_match_native_sse_for_mixed_masks() {
    if !std::arch::is_x86_feature_detected!("sse4.1") { return; }
    use std::arch::x86_64::*;
    let left = 0xDEADBEEF0102030400017FFF8000FFFFu128;
    let right = !left;
    macro_rules! check {
        ($mode:literal) => {
            let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x71, 0x0E, 0xC1, $mode]);
            cpu.simd.xmm[1] = left;
            cpu.simd.xmm[0] = 123;
            let expected = unsafe { _mm_blend_epi16::<$mode>(std::mem::transmute(left), std::mem::transmute(left)) };
            assert!(cpu.exec(&mut ram).is_ok());
            assert_eq!(cpu.simd.xmm[0], unsafe { std::mem::transmute::<__m128i, u128>(expected) });
            let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE3, 0x79, 0x0E, 0xC1, $mode]);
            cpu.simd.xmm[0] = left;
            cpu.simd.xmm[1] = right;
            let expected = unsafe { _mm_blend_epi16::<$mode>(std::mem::transmute(left), std::mem::transmute(right)) };
            assert!(cpu.exec(&mut ram).is_ok());
            assert_eq!(cpu.simd.xmm[0], unsafe { std::mem::transmute::<__m128i, u128>(expected) });
        };
    }
    check!(0); check!(15); check!(240); check!(85); check!(170); check!(255);
}

#[test]
fn vex_float_to_integer_rounding_truncation_and_faults() {
    let input = [1.5f32, -2.5, 2147483648.0, f32::NAN].into_iter().enumerate()
        .fold(0u128, |bits, (index, value)| bits | ((value.to_bits() as u128) << (index * 32)));
    for prefix in [1u8, 2] {
        for rounding in 0..4 {
            let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF8 | prefix, 0x5B, 0xC0]);
            cpu.simd.xmm[0] = input;
            cpu.simd.ymm_hi[0] = u128::MAX;
            cpu.simd.mxcsr = 0x1F80 | rounding << 13;
            assert!(cpu.exec(&mut ram).is_ok());
            assert_eq!(cpu.simd.ymm_hi[0], 0);
            #[cfg(target_arch = "x86_64")]
            unsafe {
                use std::arch::x86_64::__m128;
                let mut saved = 0u32;
                let mut status = 0u32;
                let control = 0x1F80 | rounding << 13;
                let mut native = std::mem::transmute::<u128, __m128>(input);
                macro_rules! convert {
                    ($instruction:literal) => {
                        std::arch::asm!(
                            "stmxcsr [{saved}]", "ldmxcsr [{control}]",
                            $instruction, "stmxcsr [{status}]", "ldmxcsr [{saved}]",
                            saved = in(reg) &mut saved, control = in(reg) &control,
                            status = in(reg) &mut status, vector = inout(xmm_reg) native,
                            options(nostack)
                        );
                    };
                }
                if prefix == 1 { convert!("cvtps2dq {vector}, {vector}"); }
                else { convert!("cvttps2dq {vector}, {vector}"); }
                assert_eq!(cpu.simd.xmm[0], std::mem::transmute::<__m128, u128>(native));
                assert_eq!(cpu.simd.mxcsr & 0x3F, status & 0x3F);
            }
        }
    }
    let (mut cpu, mut ram) = long_mode(&[0xC5, 0xF9, 0x5B, 0xC0]);
    cpu.simd.xmm[0] = input;
    cpu.simd.ymm_hi[0] = u128::MAX;
    cpu.simd.mxcsr &= !0x80;
    cpu.cr4 |= 1 << 10;
    assert!(matches!(cpu.exec(&mut ram), Err(Ex(19, None))));
    assert_eq!(cpu.simd.xmm[0], input);
    assert_eq!(cpu.simd.ymm_hi[0], u128::MAX);
}

#[test]
fn vex_float_to_integer_reads_wide_memory_and_applies_daz() {
    let values = [1.5f32, -2.5, 0.0, -0.0, 3.5, -4.5, f32::from_bits(1), f32::from_bits(0x80000001)];
    for daz in [false, true] {
        let (mut cpu, mut ram) = long_mode(&[0xC5, 0xFD, 0x5B, 0x00]);
        cpu.r[0] = 0x9000;
        for (index, value) in values.iter().enumerate() {
            ram.m[0x9000 + index * 4..0x9004 + index * 4].copy_from_slice(&value.to_le_bytes());
        }
        cpu.simd.mxcsr = 0x1F80 | (2 << 13) | if daz { 0x40 } else { 0 };
        assert!(cpu.exec(&mut ram).is_ok());
        let expected = [2i32, -2, 0, 0, 4, -4, if daz { 0 } else { 1 }, 0];
        let result = [cpu.simd.xmm[0].to_le_bytes(), cpu.simd.ymm_hi[0].to_le_bytes()].concat();
        let expected = expected.into_iter().flat_map(i32::to_le_bytes).collect::<Vec<_>>();
        assert_eq!(result, expected);
    }
}

#[test]
fn vex_dword_multiplication_keeps_low_products_and_supports_aliases() {
    let pack = |values: [u32; 4]| values.into_iter().enumerate().fold(0u128, |result, (index, value)| result | ((value as u128) << (index * 32)));
    let left = pack([0xFFFFFFFF, 0x80000000, 123456789, 0]);
    let right = pack([3, 2, 987654321, 0xFFFFFFFF]);
    let expected = pack([0xFFFFFFFD, 0, 123456789u32.wrapping_mul(987654321), 0]);
    for destination in [0u8, 1, 2] {
        let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, 0x71, 0x40, 0xC2 | (destination << 3)]);
        cpu.simd.xmm[1] = left;
        cpu.simd.xmm[2] = right;
        cpu.simd.ymm_hi[destination as usize] = u128::MAX;
        assert!(cpu.exec(&mut ram).is_ok());
        assert_eq!(cpu.simd.xmm[destination as usize], expected);
        assert_eq!(cpu.simd.ymm_hi[destination as usize], 0);
    }
    let (mut cpu, mut ram) = long_mode(&[0xC4, 0xE2, 0x71, 0x40, 0x00]);
    cpu.r[0] = 0x9000;
    ram.m[0x9000..0x9010].copy_from_slice(&right.to_le_bytes());
    cpu.simd.xmm[1] = left;
    assert!(cpu.exec(&mut ram).is_ok());
    assert_eq!(cpu.simd.xmm[0], expected);
}

#[test]
fn vex_sign_masks_preserve_vectors_flags_and_clear_upper_integer_bits() {
    for double in [false, true] {
        for wide in [false, true] {
            let width = if double { 64 } else { 32 };
            let count = (if wide { 256 } else { 128 }) / width;
            for expected in 0..(1u64 << count) {
                let (mut cpu, mut ram) = long_mode(&[0xC4, 0x41, 0x78 | if double { 1 } else { 0 } | if wide { 4 } else { 0 }, 0x50, 0xC1]);
                let mut vectors = [0u128; 2];
                for lane in 0..count {
                    let payload = if lane & 1 == 0 { 0 } else if double { 0x7FF8000000000001u128 } else { 0x7FC00001 };
                    vectors[lane / (128 / width)] |= (payload | (((expected >> lane) & 1) as u128) << (width - 1)) << ((lane % (128 / width)) * width);
                }
                cpu.simd.xmm[9] = vectors[0];
                cpu.simd.ymm_hi[9] = vectors[1];
                cpu.r[8] = u64::MAX;
                cpu.eflags = 2 | CF | OF;
                let flags = cpu.eflags;
                let mxcsr = cpu.simd.mxcsr;
                assert!(cpu.exec(&mut ram).is_ok());
                assert_eq!(cpu.r[8], expected);
                assert_eq!(cpu.eflags, flags);
                assert_eq!(cpu.simd.mxcsr, mxcsr);
                assert_eq!(cpu.simd.xmm[9], vectors[0]);
                assert_eq!(cpu.simd.ymm_hi[9], vectors[1]);
            }
        }
    }
    for code in [&[0xC5, 0xF8, 0x50, 0x00][..], &[0xC5, 0xF0, 0x50, 0xC1][..], &[0xC5, 0xFA, 0x50, 0xC1][..], &[0xC5, 0xFB, 0x50, 0xC1][..]] {
        let (mut cpu, mut ram) = long_mode(code);
        assert!(matches!(cpu.exec(&mut ram), Err(Ex(6, None))));
    }
}
