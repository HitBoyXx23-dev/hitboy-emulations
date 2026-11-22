#include "spc700.h"
#include <string.h>

namespace {
enum : uint8_t { FC = 1, FZ = 2, FI = 4, FH = 8, FB = 16, FP = 32, FV = 64, FN = 128 };
}

struct Spc700 {
    uint8_t A = 0, X = 0, Y = 0, SP = 0xEF, PSW = 0x02;
    uint16_t PC = 0xFFC0;
    bool stopped = false;
    void* ctx = nullptr;
    spc_read_fn rd = nullptr;
    spc_write_fn wr = nullptr;
    spc_idle_fn idl = nullptr;

    inline uint8_t r8(uint16_t a) { return rd(ctx, a); }
    inline void w8(uint16_t a, uint8_t v) { wr(ctx, a, v); }
    inline void io() { idl(ctx); }
    inline uint8_t fetch() { return r8(PC++); }
    inline uint16_t fetch16() { uint16_t lo = fetch(); return lo | uint16_t(fetch()) << 8; }
    inline void flag(uint8_t f, bool on) { PSW = on ? (PSW | f) : (PSW & ~f); }
    inline void nz(uint8_t v) { flag(FZ, v == 0); flag(FN, v & 0x80); }
    inline void nz16(uint16_t v) { flag(FZ, v == 0); flag(FN, v & 0x8000); }

    // Direct page is page 0 or page 1 depending on P; indexed accesses wrap inside the page.
    inline uint16_t dp(uint8_t off) const { return uint16_t((PSW & FP) ? 0x100 : 0) | off; }
    inline uint16_t rd16dp(uint8_t off) { return r8(dp(off)) | uint16_t(r8(dp(uint8_t(off + 1)))) << 8; }

    inline void push(uint8_t v) { w8(0x100 | SP, v); SP--; }
    inline uint8_t pop() { SP++; return r8(0x100 | SP); }
    inline void push16(uint16_t v) { push(v >> 8); push(uint8_t(v)); }
    inline uint16_t pop16() { uint16_t lo = pop(); return lo | uint16_t(pop()) << 8; }

    uint8_t adc(uint8_t a, uint8_t b) {
        int r = a + b + (PSW & FC);
        flag(FC, r > 0xFF);
        flag(FH, ((a & 0x0F) + (b & 0x0F) + (PSW & FC)) > 0x0F);
        flag(FV, (~(a ^ b) & (a ^ r) & 0x80) != 0);
        nz(uint8_t(r));
        return uint8_t(r);
    }
    uint8_t sbc(uint8_t a, uint8_t b) { return adc(a, uint8_t(~b)); }
    void cmp(uint8_t a, uint8_t b) { int r = a - b; flag(FC, a >= b); nz(uint8_t(r)); }
    uint8_t alu(int op, uint8_t a, uint8_t b) {
        switch (op) {
            case 0: a |= b; nz(a); return a;
            case 1: a &= b; nz(a); return a;
            case 2: a ^= b; nz(a); return a;
            case 3: cmp(a, b); return a;
            case 4: return adc(a, b);
            default: return sbc(a, b);
        }
    }

    void branch(bool c) {
        int8_t off = int8_t(fetch());
        if (c) { io(); io(); PC = uint16_t(PC + off); }
    }

    // mem.bit operands: 13-bit address + 3-bit bit index.
    inline void membit(uint16_t& addr, int& bit) { uint16_t w = fetch16(); addr = w & 0x1FFF; bit = w >> 13; }

    void step();
};

void Spc700::step() {
    if (stopped) { io(); return; }
    const uint8_t op = fetch();

    // ---- ALU rows: OR AND EOR CMP ADC SBC, shared addressing layout
    int grp = op >> 5;
    int col = op & 0x1F;
    if (grp <= 5) {
        uint16_t ea;
        switch (col) {
            case 0x04: A = alu(grp, A, r8(dp(fetch()))); return;
            case 0x05: A = alu(grp, A, r8(fetch16())); return;
            case 0x06: io(); A = alu(grp, A, r8(dp(X))); return;
            case 0x07: { uint8_t o = uint8_t(fetch() + X); io(); ea = rd16dp(o); A = alu(grp, A, r8(ea)); return; }
            case 0x08: A = alu(grp, A, fetch()); return;
            case 0x09: {  // dp, dp
                uint8_t s = r8(dp(fetch()));
                uint16_t d = dp(fetch());
                uint8_t r = alu(grp, r8(d), s);
                if (grp != 3) w8(d, r); else io();
                return;
            }
            case 0x14: { uint8_t o = fetch(); io(); A = alu(grp, A, r8(dp(uint8_t(o + X)))); return; }
            case 0x15: { ea = uint16_t(fetch16() + X); io(); A = alu(grp, A, r8(ea)); return; }
            case 0x16: { ea = uint16_t(fetch16() + Y); io(); A = alu(grp, A, r8(ea)); return; }
            case 0x17: { ea = uint16_t(rd16dp(fetch()) + Y); io(); A = alu(grp, A, r8(ea)); return; }
            case 0x18: {  // dp, #imm
                uint8_t imm = fetch();
                uint16_t d = dp(fetch());
                uint8_t r = alu(grp, r8(d), imm);
                if (grp != 3) w8(d, r); else io();
                return;
            }
            case 0x19: {  // (X), (Y)
                io();
                uint8_t s = r8(dp(Y));
                uint16_t d = dp(X);
                uint8_t r = alu(grp, r8(d), s);
                if (grp != 3) w8(d, r); else io();
                return;
            }
            default: break;
        }
    }

    // ---- per-column families
    if ((op & 0x0F) == 0x01) {  // TCALL n
        int n = op >> 4;
        io(); io(); io();
        push16(PC);
        uint16_t v = uint16_t(0xFFDE - 2 * n);
        PC = r8(v) | uint16_t(r8(uint16_t(v + 1))) << 8;
        return;
    }
    if ((op & 0x0F) == 0x02) {  // SET1 / CLR1 dp.bit
        uint16_t d = dp(fetch());
        uint8_t v = r8(d), b = uint8_t(1 << (op >> 5));
        w8(d, (op & 0x10) ? uint8_t(v & ~b) : uint8_t(v | b));
        return;
    }
    if ((op & 0x0F) == 0x03) {  // BBS / BBC dp.bit, rel
        uint8_t v = r8(dp(fetch()));
        io();
        bool set = v & (1 << (op >> 5));
        branch((op & 0x10) ? !set : set);
        return;
    }

    uint16_t a16;
    uint8_t v;
    switch (op) {
        case 0x00: io(); return;

        // mem.bit carry operations
        case 0x0A: case 0x2A: case 0x4A: case 0x6A: case 0x8A: case 0xAA: {
            int bit; membit(a16, bit);
            bool m = (r8(a16) >> bit) & 1;
            bool c = PSW & FC;
            switch (op) {
                case 0x0A: c = c || m; io(); break;
                case 0x2A: c = c || !m; io(); break;
                case 0x4A: c = c && m; break;
                case 0x6A: c = c && !m; break;
                case 0x8A: c = c != m; io(); break;
                default: c = m; break;
            }
            flag(FC, c);
            return;
        }
        case 0xCA: { int bit; membit(a16, bit); v = r8(a16); io(); v = (PSW & FC) ? uint8_t(v | (1 << bit)) : uint8_t(v & ~(1 << bit)); w8(a16, v); return; }
        case 0xEA: { int bit; membit(a16, bit); v = r8(a16); w8(a16, uint8_t(v ^ (1 << bit))); return; }

        // shifts / inc / dec
        case 0x0B: case 0x2B: case 0x4B: case 0x6B: case 0x8B: case 0xAB:
        case 0x0C: case 0x2C: case 0x4C: case 0x6C: case 0x8C: case 0xAC:
        case 0x1B: case 0x3B: case 0x5B: case 0x7B: case 0x9B: case 0xBB: {
            if ((op & 0x0F) == 0x0B && !(op & 0x10)) a16 = dp(fetch());
            else if ((op & 0x0F) == 0x0C) a16 = fetch16();
            else { uint8_t o = fetch(); io(); a16 = dp(uint8_t(o + X)); }
            v = r8(a16);
            switch (op >> 5) {
                case 0: flag(FC, v & 0x80); v = uint8_t(v << 1); break;
                case 1: { bool c = PSW & FC; flag(FC, v & 0x80); v = uint8_t(v << 1 | c); break; }
                case 2: flag(FC, v & 1); v >>= 1; break;
                case 3: { bool c = PSW & FC; flag(FC, v & 1); v = uint8_t(v >> 1 | (c << 7)); break; }
                case 4: v--; break;
                default: v++; break;
            }
            nz(v);
            w8(a16, v);
            return;
        }
        case 0x1C: io(); flag(FC, A & 0x80); A = uint8_t(A << 1); nz(A); return;
        case 0x3C: { io(); bool c = PSW & FC; flag(FC, A & 0x80); A = uint8_t(A << 1 | c); nz(A); return; }
        case 0x5C: io(); flag(FC, A & 1); A >>= 1; nz(A); return;
        case 0x7C: { io(); bool c = PSW & FC; flag(FC, A & 1); A = uint8_t(A >> 1 | (c << 7)); nz(A); return; }
        case 0x9C: io(); A--; nz(A); return;
        case 0xBC: io(); A++; nz(A); return;
        case 0x1D: io(); X--; nz(X); return;
        case 0x3D: io(); X++; nz(X); return;
        case 0xDC: io(); Y--; nz(Y); return;
        case 0xFC: io(); Y++; nz(Y); return;

        // stack
        case 0x0D: io(); push(PSW); io(); return;
        case 0x2D: io(); push(A); io(); return;
        case 0x4D: io(); push(X); io(); return;
        case 0x6D: io(); push(Y); io(); return;
        case 0x8E: io(); io(); PSW = pop(); return;
        case 0xAE: io(); io(); A = pop(); return;
        case 0xCE: io(); io(); X = pop(); return;
        case 0xEE: io(); io(); Y = pop(); return;

        // test-and-set / clear
        case 0x0E: case 0x4E: {
            a16 = fetch16();
            v = r8(a16);
            nz(uint8_t(A - v));
            io();
            w8(a16, op == 0x0E ? uint8_t(v | A) : uint8_t(v & ~A));
            return;
        }

        // control flow
        case 0x0F: io(); io(); push16(PC); push(PSW); PSW = uint8_t((PSW | FB) & ~FI); PC = r8(0xFFDE) | uint16_t(r8(0xFFDF)) << 8; return;
        case 0x1F: { a16 = uint16_t(fetch16() + X); io(); PC = r8(a16) | uint16_t(r8(uint16_t(a16 + 1))) << 8; return; }
        case 0x2F: branch(true); return;
        case 0x3F: { a16 = fetch16(); io(); io(); io(); push16(PC); PC = a16; return; }
        case 0x4F: { uint8_t u = fetch(); io(); io(); push16(PC); PC = 0xFF00 | u; return; }
        case 0x5F: PC = fetch16(); return;
        case 0x6F: io(); io(); PC = pop16(); return;
        case 0x7F: io(); io(); PSW = pop(); PC = pop16(); return;
        case 0x10: branch(!(PSW & FN)); return;
        case 0x30: branch(PSW & FN); return;
        case 0x50: branch(!(PSW & FV)); return;
        case 0x70: branch(PSW & FV); return;
        case 0x90: branch(!(PSW & FC)); return;
        case 0xB0: branch(PSW & FC); return;
        case 0xD0: branch(!(PSW & FZ)); return;
        case 0xF0: branch(PSW & FZ); return;
        case 0x2E: { v = r8(dp(fetch())); io(); branch(v != A); return; }
        case 0xDE: { uint8_t o = fetch(); io(); v = r8(dp(uint8_t(o + X))); io(); branch(v != A); return; }
        case 0x6E: { uint16_t d = dp(fetch()); v = uint8_t(r8(d) - 1); w8(d, v); branch(v != 0); return; }
        case 0xFE: io(); io(); Y--; branch(Y != 0); return;

        // 16-bit word operations on YA
        case 0x1A: case 0x3A: {
            uint8_t o = fetch();
            uint16_t w = rd16dp(o);
            w = uint16_t(op == 0x3A ? w + 1 : w - 1);
            w8(dp(o), uint8_t(w));
            w8(dp(uint8_t(o + 1)), uint8_t(w >> 8));
            nz16(w);
            return;
        }
        case 0x5A: {
            uint16_t w = rd16dp(fetch());
            uint16_t ya = uint16_t(Y << 8 | A);
            flag(FC, ya >= w);
            nz16(uint16_t(ya - w));
            return;
        }
        case 0x7A: case 0x9A: {
            uint16_t w = rd16dp(fetch());
            io();
            uint16_t ya = uint16_t(Y << 8 | A);
            uint32_t r;
            if (op == 0x7A) {
                r = uint32_t(ya) + w;
                flag(FC, r > 0xFFFF);
                flag(FH, ((ya & 0x0FFF) + (w & 0x0FFF)) > 0x0FFF);
                flag(FV, (~(ya ^ w) & (ya ^ r) & 0x8000) != 0);
            } else {
                r = uint32_t(ya) - w;
                flag(FC, ya >= w);
                flag(FH, (ya & 0x0FFF) >= (w & 0x0FFF));
                flag(FV, ((ya ^ w) & (ya ^ r) & 0x8000) != 0);
            }
            A = uint8_t(r);
            Y = uint8_t(r >> 8);
            nz16(uint16_t(r));
            return;
        }
        case 0xBA: { uint16_t w = rd16dp(fetch()); io(); A = uint8_t(w); Y = uint8_t(w >> 8); nz16(w); return; }
        case 0xDA: { uint8_t o = fetch(); r8(dp(o)); w8(dp(o), A); w8(dp(uint8_t(o + 1)), Y); return; }

        // register transfers
        case 0x5D: io(); X = A; nz(X); return;
        case 0x7D: io(); A = X; nz(A); return;
        case 0x9D: io(); X = SP; nz(X); return;
        case 0xBD: io(); SP = X; return;
        case 0xDD: io(); A = Y; nz(A); return;
        case 0xFD: io(); Y = A; nz(Y); return;

        // compares with X / Y
        case 0x1E: cmp(X, r8(fetch16())); return;
        case 0x3E: cmp(X, r8(dp(fetch()))); return;
        case 0x5E: cmp(Y, r8(fetch16())); return;
        case 0x7E: cmp(Y, r8(dp(fetch()))); return;
        case 0xC8: cmp(X, fetch()); return;
        case 0xAD: cmp(Y, fetch()); return;

        // flags
        case 0x20: io(); PSW &= ~FP; return;
        case 0x40: io(); PSW |= FP; return;
        case 0x60: io(); PSW &= ~FC; return;
        case 0x80: io(); PSW |= FC; return;
        case 0xA0: io(); io(); PSW |= FI; return;
        case 0xC0: io(); io(); PSW &= ~FI; return;
        case 0xE0: io(); PSW &= ~(FV | FH); return;
        case 0xED: io(); io(); PSW ^= FC; return;

        // immediate loads and dp moves
        case 0x8D: Y = fetch(); nz(Y); return;
        case 0xCD: X = fetch(); nz(X); return;
        case 0xE8: A = fetch(); nz(A); return;
        case 0x8F: { uint8_t imm = fetch(); uint16_t d = dp(fetch()); r8(d); w8(d, imm); return; }
        case 0xFA: { uint8_t s = r8(dp(fetch())); w8(dp(fetch()), s); return; }

        // loads into A
        case 0xE4: A = r8(dp(fetch())); nz(A); return;
        case 0xE5: A = r8(fetch16()); nz(A); return;
        case 0xE6: io(); A = r8(dp(X)); nz(A); return;
        case 0xE7: { uint8_t o = uint8_t(fetch() + X); io(); A = r8(rd16dp(o)); nz(A); return; }
        case 0xF4: { uint8_t o = fetch(); io(); A = r8(dp(uint8_t(o + X))); nz(A); return; }
        case 0xF5: { a16 = uint16_t(fetch16() + X); io(); A = r8(a16); nz(A); return; }
        case 0xF6: { a16 = uint16_t(fetch16() + Y); io(); A = r8(a16); nz(A); return; }
        case 0xF7: { a16 = uint16_t(rd16dp(fetch()) + Y); io(); A = r8(a16); nz(A); return; }
        case 0xBF: io(); A = r8(dp(X)); X++; io(); nz(A); return;

        // stores from A
        case 0xC4: { uint16_t d = dp(fetch()); r8(d); w8(d, A); return; }
        case 0xC5: { a16 = fetch16(); r8(a16); w8(a16, A); return; }
        case 0xC6: io(); r8(dp(X)); w8(dp(X), A); return;
        case 0xC7: { uint8_t o = uint8_t(fetch() + X); io(); a16 = rd16dp(o); r8(a16); w8(a16, A); return; }
        case 0xD4: { uint8_t o = fetch(); io(); uint16_t d = dp(uint8_t(o + X)); r8(d); w8(d, A); return; }
        case 0xD5: { a16 = uint16_t(fetch16() + X); io(); r8(a16); w8(a16, A); return; }
        case 0xD6: { a16 = uint16_t(fetch16() + Y); io(); r8(a16); w8(a16, A); return; }
        case 0xD7: { a16 = uint16_t(rd16dp(fetch()) + Y); io(); r8(a16); w8(a16, A); return; }
        case 0xAF: io(); io(); w8(dp(X), A); X++; return;

        // X and Y loads / stores
        case 0xF8: X = r8(dp(fetch())); nz(X); return;
        case 0xF9: { uint8_t o = fetch(); io(); X = r8(dp(uint8_t(o + Y))); nz(X); return; }
        case 0xE9: X = r8(fetch16()); nz(X); return;
        case 0xD8: { uint16_t d = dp(fetch()); r8(d); w8(d, X); return; }
        case 0xD9: { uint8_t o = fetch(); io(); uint16_t d = dp(uint8_t(o + Y)); r8(d); w8(d, X); return; }
        case 0xC9: { a16 = fetch16(); r8(a16); w8(a16, X); return; }
        case 0xEB: Y = r8(dp(fetch())); nz(Y); return;
        case 0xFB: { uint8_t o = fetch(); io(); Y = r8(dp(uint8_t(o + X))); nz(Y); return; }
        case 0xEC: Y = r8(fetch16()); nz(Y); return;
        case 0xCB: { uint16_t d = dp(fetch()); r8(d); w8(d, Y); return; }
        case 0xDB: { uint8_t o = fetch(); io(); uint16_t d = dp(uint8_t(o + X)); r8(d); w8(d, Y); return; }
        case 0xCC: { a16 = fetch16(); r8(a16); w8(a16, Y); return; }

        // multiply / divide / decimal / nibble swap
        case 0xCF: {
            for (int i = 0; i < 8; i++) io();
            uint16_t r = uint16_t(Y * A);
            A = uint8_t(r);
            Y = uint8_t(r >> 8);
            nz(Y);
            return;
        }
        case 0x9E: {
            for (int i = 0; i < 11; i++) io();
            uint16_t ya = uint16_t(Y << 8 | A);
            flag(FH, (Y & 15) >= (X & 15));
            flag(FV, Y >= X);
            if (Y < (X << 1)) {
                A = uint8_t(ya / X);
                Y = uint8_t(ya % X);
            } else {
                A = uint8_t(255 - (ya - (X << 9)) / (256 - X));
                Y = uint8_t(X + (ya - (X << 9)) % (256 - X));
            }
            nz(A);
            return;
        }
        case 0xDF:
            io(); io();
            if ((PSW & FC) || A > 0x99) { A += 0x60; PSW |= FC; }
            if ((PSW & FH) || (A & 15) > 9) A += 6;
            nz(A);
            return;
        case 0xBE:
            io(); io();
            if (!(PSW & FC) || A > 0x99) { A -= 0x60; PSW &= ~FC; }
            if (!(PSW & FH) || (A & 15) > 9) A -= 6;
            nz(A);
            return;
        case 0x9F: io(); io(); io(); io(); A = uint8_t(A >> 4 | A << 4); nz(A); return;

        case 0xEF: case 0xFF: stopped = true; return;  // SLEEP / STOP
        default: io(); return;
    }
}

extern "C" {
Spc700* spc_new(void* ctx, spc_read_fn read, spc_write_fn write, spc_idle_fn idle) {
    Spc700* c = new Spc700();
    c->ctx = ctx; c->rd = read; c->wr = write; c->idl = idle;
    return c;
}
void spc_free(Spc700* c) { delete c; }
void spc_step(Spc700* c) { c->step(); }
void spc_get_regs(const Spc700* c, SpcRegs* o) {
    o->a = c->A; o->x = c->X; o->y = c->Y; o->sp = c->SP; o->psw = c->PSW; o->pc = c->PC; o->stopped = c->stopped;
}
void spc_set_regs(Spc700* c, const SpcRegs* i) {
    c->A = i->a; c->X = i->x; c->Y = i->y; c->SP = i->sp; c->PSW = i->psw; c->PC = i->pc; c->stopped = i->stopped;
}

// ---- state ----
unsigned long spc_state_size(void) { return (unsigned long)sizeof(Spc700); }
void spc_save_state(const Spc700* c, uint8_t* out) {
    Spc700 tmp = *c;
    tmp.ctx = {}; tmp.rd = {}; tmp.wr = {}; tmp.idl = {};
    memcpy(out, &tmp, sizeof(tmp));
}
void spc_load_state(Spc700* c, const uint8_t* in) {
    Spc700 keep = *c;
    memcpy((void*)c, in, sizeof(*c));
    c->ctx = keep.ctx;
    c->rd = keep.rd;
    c->wr = keep.wr;
    c->idl = keep.idl;
}

}
