#include "cpu6502.h"
#include <string.h>

namespace {
enum : uint8_t { FC = 1, FZ = 2, FI = 4, FD = 8, FB = 16, FU = 32, FV = 64, FN = 128 };
}

struct Cpu6502 {
    uint8_t A = 0, X = 0, Y = 0, S = 0xFD, P = FI | FU;
    uint16_t PC = 0;
    void* ctx = nullptr;
    cpu_read_fn rd = nullptr;
    cpu_write_fn wr = nullptr;
    bool nmi_pending = false;
    bool irq_line = false;
    uint32_t extra = 0;  // page-cross penalty for the current instruction
    bool decimal = false;  // NMOS decimal mode (off on the NES's 2A03)

    inline uint8_t r(uint16_t a) { return rd(ctx, a); }
    inline void w(uint16_t a, uint8_t v) { wr(ctx, a, v); }
    inline uint16_t r16(uint16_t a) { return r(a) | (uint16_t(r(uint16_t(a + 1))) << 8); }
    inline void push(uint8_t v) { w(0x100 | S--, v); }
    inline uint8_t pop() { return r(0x100 | ++S); }
    inline void zn(uint8_t v) { P = (P & ~(FZ | FN)) | (v ? 0 : FZ) | (v & FN); }
    inline void flag(uint8_t f, bool on) { P = on ? (P | f) : (P & ~f); }

    // ---- addressing modes ----
    inline uint16_t IMM() { return PC++; }
    inline uint16_t ZP() { return r(PC++); }
    inline uint16_t ZPX() { return uint8_t(r(PC++) + X); }
    inline uint16_t ZPY() { return uint8_t(r(PC++) + Y); }
    inline uint16_t ABS() { uint16_t a = r16(PC); PC += 2; return a; }
    inline uint16_t IDX(uint8_t idx, bool penalty) {
        uint16_t b = r16(PC); PC += 2;
        uint16_t a = uint16_t(b + idx);
        if (penalty && ((a ^ b) & 0xFF00)) extra = 1;
        return a;
    }
    inline uint16_t ABX(bool p) { return IDX(X, p); }
    inline uint16_t ABY(bool p) { return IDX(Y, p); }
    inline uint16_t IZX() {
        uint8_t z = uint8_t(r(PC++) + X);
        return r(z) | (uint16_t(r(uint8_t(z + 1))) << 8);
    }
    inline uint16_t IZY(bool p) {
        uint8_t z = r(PC++);
        uint16_t b = r(z) | (uint16_t(r(uint8_t(z + 1))) << 8);
        uint16_t a = uint16_t(b + Y);
        if (p && ((a ^ b) & 0xFF00)) extra = 1;
        return a;
    }

    // ---- operations ----
    inline void ORA(uint8_t v) { A |= v; zn(A); }
    inline void AND(uint8_t v) { A &= v; zn(A); }
    inline void EOR(uint8_t v) { A ^= v; zn(A); }
    inline void ADC(uint8_t v) {
        if (decimal && (P & FD)) {
            // NMOS BCD: Z from the binary sum, N/V from the high nibble before its adjust.
            unsigned c = P & FC;
            unsigned lo = (A & 0xF) + (v & 0xF) + c;
            if (lo > 9) lo += 6;
            unsigned hi = (A >> 4) + (v >> 4) + (lo > 0xF);
            flag(FZ, uint8_t(A + v + c) == 0);
            flag(FN, hi & 8);
            flag(FV, (~(A ^ v) & (A ^ (hi << 4)) & 0x80) != 0);
            if (hi > 9) hi += 6;
            flag(FC, hi > 0xF);
            A = uint8_t((hi << 4) | (lo & 0xF));
            return;
        }
        uint16_t s = A + v + (P & FC);
        flag(FC, s > 0xFF);
        flag(FV, (~(A ^ v) & (A ^ s) & 0x80) != 0);
        A = uint8_t(s); zn(A);
    }
    inline void SBC(uint8_t v) {
        if (decimal && (P & FD)) {
            // NMOS BCD: all flags come from the binary subtraction.
            unsigned b = 1 - (P & FC);
            unsigned bin = unsigned(A) - v - b;
            int lo = int(A & 0xF) - int(v & 0xF) - int(b);
            int hi = int(A >> 4) - int(v >> 4);
            if (lo < 0) { lo -= 6; hi--; }
            if (hi < 0) hi -= 6;
            flag(FC, bin < 0x100);
            flag(FV, ((A ^ v) & (A ^ bin) & 0x80) != 0);
            zn(uint8_t(bin));
            A = uint8_t((hi << 4) | (lo & 0xF));
            return;
        }
        ADC(v ^ 0xFF);
    }
    inline void CMPr(uint8_t reg, uint8_t v) { flag(FC, reg >= v); zn(uint8_t(reg - v)); }
    inline void CMP(uint8_t v) { CMPr(A, v); }
    inline void LDA(uint8_t v) { A = v; zn(A); }
    inline void BIT(uint8_t v) { flag(FZ, (A & v) == 0); P = (P & 0x3F) | (v & 0xC0); }
    inline uint8_t ASL(uint8_t v) { flag(FC, v & 0x80); v = uint8_t(v << 1); zn(v); return v; }
    inline uint8_t LSR(uint8_t v) { flag(FC, v & 1); v >>= 1; zn(v); return v; }
    inline uint8_t ROL(uint8_t v) { uint8_t c = P & FC; flag(FC, v & 0x80); v = uint8_t(v << 1) | c; zn(v); return v; }
    inline uint8_t ROR(uint8_t v) { uint8_t c = uint8_t((P & FC) << 7); flag(FC, v & 1); v = (v >> 1) | c; zn(v); return v; }
    inline uint8_t INC(uint8_t v) { zn(++v); return v; }
    inline uint8_t DEC(uint8_t v) { zn(--v); return v; }

    inline uint32_t branch(bool cond) {
        int8_t off = int8_t(r(PC++));
        if (!cond) return 2;
        uint16_t t = uint16_t(PC + off);
        uint32_t c = ((t ^ PC) & 0xFF00) ? 4 : 3;
        PC = t;
        return c;
    }
    inline void interrupt(uint16_t vec, bool brk) {
        push(PC >> 8); push(PC & 0xFF);
        uint8_t flags = uint8_t(P | FU);
        flags = brk ? uint8_t(flags | FB) : uint8_t(flags & ~FB);
        push(flags);
        P |= FI;
        PC = r16(vec);
    }

    uint32_t step();
};

uint32_t Cpu6502::step() {
    if (nmi_pending) { nmi_pending = false; interrupt(0xFFFA, false); return 7; }
    if (irq_line && !(P & FI)) { interrupt(0xFFFE, false); return 7; }

    extra = 0;
    const uint8_t op = r(PC++);
    uint16_t a;
    uint8_t v;

// Read-type instructions with the classic 8 addressing modes.
#define GROUP_READ(I, Z, ZX, AB, AX, AY, IX, IY, F) \
    case I:  F(r(IMM())); return 2;               \
    case Z:  F(r(ZP()));  return 3;               \
    case ZX: F(r(ZPX())); return 4;               \
    case AB: F(r(ABS())); return 4;               \
    case AX: F(r(ABX(true))); return 4 + extra;   \
    case AY: F(r(ABY(true))); return 4 + extra;   \
    case IX: F(r(IZX())); return 6;               \
    case IY: F(r(IZY(true))); return 5 + extra;

// Memory read-modify-write (includes the hardware's dummy write of the old value).
#define GROUP_MEM_RMW(Z, ZX, AB, AX, F)                           \
    case Z:  a = ZP();  v = r(a); w(a, v); w(a, F(v)); return 5;  \
    case ZX: a = ZPX(); v = r(a); w(a, v); w(a, F(v)); return 6;  \
    case AB: a = ABS(); v = r(a); w(a, v); w(a, F(v)); return 6;  \
    case AX: a = ABX(false); v = r(a); w(a, v); w(a, F(v)); return 7;

#define GROUP_RMW(ACC, Z, ZX, AB, AX, F) \
    case ACC: A = F(A); return 2;        \
    GROUP_MEM_RMW(Z, ZX, AB, AX, F)

    switch (op) {
        GROUP_READ(0x09, 0x05, 0x15, 0x0D, 0x1D, 0x19, 0x01, 0x11, ORA)
        GROUP_READ(0x29, 0x25, 0x35, 0x2D, 0x3D, 0x39, 0x21, 0x31, AND)
        GROUP_READ(0x49, 0x45, 0x55, 0x4D, 0x5D, 0x59, 0x41, 0x51, EOR)
        GROUP_READ(0x69, 0x65, 0x75, 0x6D, 0x7D, 0x79, 0x61, 0x71, ADC)
        GROUP_READ(0xA9, 0xA5, 0xB5, 0xAD, 0xBD, 0xB9, 0xA1, 0xB1, LDA)
        GROUP_READ(0xC9, 0xC5, 0xD5, 0xCD, 0xDD, 0xD9, 0xC1, 0xD1, CMP)
        GROUP_READ(0xE9, 0xE5, 0xF5, 0xED, 0xFD, 0xF9, 0xE1, 0xF1, SBC)
        case 0xEB: SBC(r(IMM())); return 2;  // unofficial SBC

        GROUP_RMW(0x0A, 0x06, 0x16, 0x0E, 0x1E, ASL)
        GROUP_RMW(0x4A, 0x46, 0x56, 0x4E, 0x5E, LSR)
        GROUP_RMW(0x2A, 0x26, 0x36, 0x2E, 0x3E, ROL)
        GROUP_RMW(0x6A, 0x66, 0x76, 0x6E, 0x7E, ROR)
        GROUP_MEM_RMW(0xE6, 0xF6, 0xEE, 0xFE, INC)
        GROUP_MEM_RMW(0xC6, 0xD6, 0xCE, 0xDE, DEC)

        // stores
        case 0x85: w(ZP(), A); return 3;
        case 0x95: w(ZPX(), A); return 4;
        case 0x8D: w(ABS(), A); return 4;
        case 0x9D: w(ABX(false), A); return 5;
        case 0x99: w(ABY(false), A); return 5;
        case 0x81: w(IZX(), A); return 6;
        case 0x91: w(IZY(false), A); return 6;
        case 0x86: w(ZP(), X); return 3;
        case 0x96: w(ZPY(), X); return 4;
        case 0x8E: w(ABS(), X); return 4;
        case 0x84: w(ZP(), Y); return 3;
        case 0x94: w(ZPX(), Y); return 4;
        case 0x8C: w(ABS(), Y); return 4;

        // LDX / LDY
        case 0xA2: X = r(IMM()); zn(X); return 2;
        case 0xA6: X = r(ZP()); zn(X); return 3;
        case 0xB6: X = r(ZPY()); zn(X); return 4;
        case 0xAE: X = r(ABS()); zn(X); return 4;
        case 0xBE: X = r(ABY(true)); zn(X); return 4 + extra;
        case 0xA0: Y = r(IMM()); zn(Y); return 2;
        case 0xA4: Y = r(ZP()); zn(Y); return 3;
        case 0xB4: Y = r(ZPX()); zn(Y); return 4;
        case 0xAC: Y = r(ABS()); zn(Y); return 4;
        case 0xBC: Y = r(ABX(true)); zn(Y); return 4 + extra;

        // CPX / CPY / BIT
        case 0xE0: CMPr(X, r(IMM())); return 2;
        case 0xE4: CMPr(X, r(ZP())); return 3;
        case 0xEC: CMPr(X, r(ABS())); return 4;
        case 0xC0: CMPr(Y, r(IMM())); return 2;
        case 0xC4: CMPr(Y, r(ZP())); return 3;
        case 0xCC: CMPr(Y, r(ABS())); return 4;
        case 0x24: BIT(r(ZP())); return 3;
        case 0x2C: BIT(r(ABS())); return 4;

        // branches
        case 0x10: return branch(!(P & FN));
        case 0x30: return branch(P & FN);
        case 0x50: return branch(!(P & FV));
        case 0x70: return branch(P & FV);
        case 0x90: return branch(!(P & FC));
        case 0xB0: return branch(P & FC);
        case 0xD0: return branch(!(P & FZ));
        case 0xF0: return branch(P & FZ);

        // jumps / subroutines
        case 0x4C: PC = ABS(); return 3;
        case 0x6C: {  // JMP (ind) with the page-wrap hardware bug
            uint16_t p = ABS();
            uint16_t hi = (p & 0xFF00) | uint8_t(p + 1);
            PC = r(p) | (uint16_t(r(hi)) << 8);
            return 5;
        }
        case 0x20: {
            uint16_t t = ABS();
            uint16_t ret = uint16_t(PC - 1);
            push(ret >> 8); push(ret & 0xFF);
            PC = t;
            return 6;
        }
        case 0x60: { uint16_t lo = pop(); PC = uint16_t((lo | (uint16_t(pop()) << 8)) + 1); return 6; }
        case 0x40: {
            P = uint8_t((pop() & ~FB) | FU);
            uint16_t lo = pop();
            PC = lo | (uint16_t(pop()) << 8);
            return 6;
        }
        case 0x00: PC++; interrupt(0xFFFE, true); return 7;

        // stack
        case 0x48: push(A); return 3;
        case 0x68: A = pop(); zn(A); return 4;
        case 0x08: push(P | FB | FU); return 3;
        case 0x28: P = uint8_t((pop() & ~FB) | FU); return 4;

        // flags
        case 0x18: P &= ~FC; return 2;
        case 0x38: P |= FC; return 2;
        case 0x58: P &= ~FI; return 2;
        case 0x78: P |= FI; return 2;
        case 0xB8: P &= ~FV; return 2;
        case 0xD8: P &= ~FD; return 2;
        case 0xF8: P |= FD; return 2;

        // transfers / register inc-dec
        case 0xAA: X = A; zn(X); return 2;
        case 0x8A: A = X; zn(A); return 2;
        case 0xA8: Y = A; zn(Y); return 2;
        case 0x98: A = Y; zn(A); return 2;
        case 0xBA: X = S; zn(X); return 2;
        case 0x9A: S = X; return 2;
        case 0xE8: zn(++X); return 2;
        case 0xC8: zn(++Y); return 2;
        case 0xCA: zn(--X); return 2;
        case 0x88: zn(--Y); return 2;

        // NOPs (official + common unofficial ones, with correct operand sizes)
        case 0xEA: case 0x1A: case 0x3A: case 0x5A: case 0x7A: case 0xDA: case 0xFA: return 2;
        case 0x80: case 0x82: case 0x89: case 0xC2: case 0xE2: PC += 1; return 2;
        case 0x04: case 0x44: case 0x64: PC += 1; return 3;
        case 0x14: case 0x34: case 0x54: case 0x74: case 0xD4: case 0xF4: PC += 1; return 4;
        case 0x0C: PC += 2; return 4;
        case 0x1C: case 0x3C: case 0x5C: case 0x7C: case 0xDC: case 0xFC: r(ABX(true)); return 4 + extra;

        default:
            // Remaining unofficial opcodes are treated as 2-cycle NOPs for now.
            return 2;
    }
#undef GROUP_READ
#undef GROUP_MEM_RMW
#undef GROUP_RMW
}

extern "C" {

Cpu6502* cpu6502_new(void* ctx, cpu_read_fn read, cpu_write_fn write) {
    Cpu6502* c = new Cpu6502();
    c->ctx = ctx; c->rd = read; c->wr = write;
    return c;
}
void cpu6502_free(Cpu6502* cpu) { delete cpu; }
void cpu6502_reset(Cpu6502* c) {
    c->A = c->X = c->Y = 0;
    c->S = 0xFD;
    c->P = FI | FU;
    c->nmi_pending = false;
    c->PC = c->r16(0xFFFC);
}
void cpu6502_nmi(Cpu6502* c) { c->nmi_pending = true; }
void cpu6502_set_decimal(Cpu6502* c, int enabled) { c->decimal = enabled != 0; }
void cpu6502_set_irq(Cpu6502* c, int asserted) { c->irq_line = asserted != 0; }
uint32_t cpu6502_step(Cpu6502* c) { return c->step(); }
void cpu6502_get_regs(const Cpu6502* c, Cpu6502Regs* o) {
    o->a = c->A; o->x = c->X; o->y = c->Y; o->sp = c->S; o->p = c->P; o->pc = c->PC;
}
void cpu6502_set_regs(Cpu6502* c, const Cpu6502Regs* i) {
    c->A = i->a; c->X = i->x; c->Y = i->y; c->S = i->sp; c->P = i->p; c->PC = i->pc;
}


// ---- state ----
unsigned long cpu6502_state_size(void) { return (unsigned long)sizeof(Cpu6502); }
void cpu6502_save_state(const Cpu6502* c, uint8_t* out) {
    Cpu6502 tmp = *c;
    tmp.ctx = {}; tmp.rd = {}; tmp.wr = {};
    memcpy(out, &tmp, sizeof(tmp));
}
void cpu6502_load_state(Cpu6502* c, const uint8_t* in) {
    Cpu6502 keep = *c;
    memcpy((void*)c, in, sizeof(*c));
    c->ctx = keep.ctx;
    c->rd = keep.rd;
    c->wr = keep.wr;
}

}  // extern "C"
