#include "sm83.h"
#include <string.h>

namespace {
enum : uint8_t { FZ = 0x80, FN = 0x40, FH = 0x20, FC = 0x10 };

// Base M-cycles per opcode (conditional branches: not-taken timing).
const uint8_t kCycles[256] = {
    1,3,2,2,1,1,2,1,5,2,2,2,1,1,2,1, 1,3,2,2,1,1,2,1,3,2,2,2,1,1,2,1,
    2,3,2,2,1,1,2,1,2,2,2,2,1,1,2,1, 2,3,2,2,3,3,3,1,2,2,2,2,1,1,2,1,
    1,1,1,1,1,1,2,1,1,1,1,1,1,1,2,1, 1,1,1,1,1,1,2,1,1,1,1,1,1,1,2,1,
    1,1,1,1,1,1,2,1,1,1,1,1,1,1,2,1, 2,2,2,2,2,2,1,2,1,1,1,1,1,1,2,1,
    1,1,1,1,1,1,2,1,1,1,1,1,1,1,2,1, 1,1,1,1,1,1,2,1,1,1,1,1,1,1,2,1,
    1,1,1,1,1,1,2,1,1,1,1,1,1,1,2,1, 1,1,1,1,1,1,2,1,1,1,1,1,1,1,2,1,
    2,3,3,4,3,4,2,4,2,4,3,0,3,6,2,4, 2,3,3,0,3,4,2,4,2,4,3,0,3,0,2,4,
    3,3,2,0,0,4,2,4,4,1,4,0,0,0,2,4, 3,3,2,1,0,4,2,4,3,2,4,1,0,0,2,4,
};
}  // namespace

struct Sm83 {
    uint8_t a = 0x01, f = 0xB0, b = 0x00, c = 0x13, d = 0x00, e = 0xD8, h = 0x01, l = 0x4D;
    uint16_t sp = 0xFFFE, pc = 0x0100;
    bool ime = false, halted = false;
    int ei_delay = 0;
    void* ctx = nullptr;
    sm83_read_fn rd = nullptr;
    sm83_write_fn wr = nullptr;
    uint32_t extra = 0;  // extra M-cycles for taken branches

    inline uint8_t r8(uint16_t a_) { return rd(ctx, a_); }
    inline void w8(uint16_t a_, uint8_t v) { wr(ctx, a_, v); }
    inline uint8_t imm8() { return r8(pc++); }
    inline uint16_t imm16() { uint16_t lo = imm8(); return lo | (uint16_t(imm8()) << 8); }
    inline void push(uint16_t v) { w8(--sp, uint8_t(v >> 8)); w8(--sp, uint8_t(v)); }
    inline uint16_t pop() { uint16_t lo = r8(sp++); return lo | (uint16_t(r8(sp++)) << 8); }

    inline uint16_t bc() const { return uint16_t(b << 8 | c); }
    inline uint16_t de() const { return uint16_t(d << 8 | e); }
    inline uint16_t hl() const { return uint16_t(h << 8 | l); }
    inline void set_hl(uint16_t v) { h = uint8_t(v >> 8); l = uint8_t(v); }

    uint16_t rp(int p) const {
        switch (p) { case 0: return bc(); case 1: return de(); case 2: return hl(); default: return sp; }
    }
    void set_rp(int p, uint16_t v) {
        switch (p) {
            case 0: b = uint8_t(v >> 8); c = uint8_t(v); break;
            case 1: d = uint8_t(v >> 8); e = uint8_t(v); break;
            case 2: set_hl(v); break;
            default: sp = v; break;
        }
    }
    uint8_t reg(int i) {
        switch (i) {
            case 0: return b; case 1: return c; case 2: return d; case 3: return e;
            case 4: return h; case 5: return l; case 6: return r8(hl()); default: return a;
        }
    }
    void set_reg(int i, uint8_t v) {
        switch (i) {
            case 0: b = v; break; case 1: c = v; break; case 2: d = v; break; case 3: e = v; break;
            case 4: h = v; break; case 5: l = v; break; case 6: w8(hl(), v); break; default: a = v; break;
        }
    }
    bool cond(int cc) const {
        switch (cc) { case 0: return !(f & FZ); case 1: return f & FZ; case 2: return !(f & FC); default: return f & FC; }
    }
    inline void flags(bool z, bool n, bool hc, bool cy) {
        f = uint8_t((z ? FZ : 0) | (n ? FN : 0) | (hc ? FH : 0) | (cy ? FC : 0));
    }

    void alu(int op, uint8_t v) {
        uint8_t carry = (f & FC) ? 1 : 0;
        switch (op) {
            case 0: case 1: {
                uint8_t cin = op == 1 ? carry : 0;
                unsigned r = a + v + cin;
                flags(uint8_t(r) == 0, false, ((a & 0xF) + (v & 0xF) + cin) > 0xF, r > 0xFF);
                a = uint8_t(r);
                break;
            }
            case 2: case 3: case 7: {
                uint8_t cin = op == 3 ? carry : 0;
                int r = a - v - cin;
                flags(uint8_t(r) == 0, true, ((a & 0xF) - (v & 0xF) - cin) < 0, r < 0);
                if (op != 7) a = uint8_t(r);
                break;
            }
            case 4: a &= v; flags(a == 0, false, true, false); break;
            case 5: a ^= v; flags(a == 0, false, false, false); break;
            default: a |= v; flags(a == 0, false, false, false); break;
        }
    }

    uint8_t cb_rot(int op, uint8_t v) {
        uint8_t cy = (f & FC) ? 1 : 0, out;
        switch (op) {
            case 0: out = v >> 7; v = uint8_t(v << 1 | out); break;
            case 1: out = v & 1; v = uint8_t(v >> 1 | out << 7); break;
            case 2: out = v >> 7; v = uint8_t(v << 1 | cy); break;
            case 3: out = v & 1; v = uint8_t(v >> 1 | cy << 7); break;
            case 4: out = v >> 7; v = uint8_t(v << 1); break;
            case 5: out = v & 1; v = uint8_t((v >> 1) | (v & 0x80)); break;
            case 6: out = 0; v = uint8_t(v << 4 | v >> 4); break;
            default: out = v & 1; v >>= 1; break;
        }
        flags(v == 0, false, false, out);
        return v;
    }

    uint32_t cb() {
        uint8_t op = imm8();
        int x = op >> 6, y = (op >> 3) & 7, z = op & 7;
        uint8_t v = reg(z);
        switch (x) {
            case 0: set_reg(z, cb_rot(y, v)); break;
            case 1: f = uint8_t((f & FC) | FH | ((v >> y) & 1 ? 0 : FZ)); return z == 6 ? 12 : 8;
            case 2: set_reg(z, uint8_t(v & ~(1 << y))); break;
            default: set_reg(z, uint8_t(v | (1 << y))); break;
        }
        return z == 6 ? 16 : 8;
    }

    uint16_t add_sp(int8_t d8) {
        uint16_t r = uint16_t(sp + d8);
        flags(false, false, ((sp ^ d8 ^ r) & 0x10) != 0, ((sp ^ d8 ^ r) & 0x100) != 0);
        return r;
    }

    uint32_t step();
    void exec(uint8_t op);
};

void Sm83::exec(uint8_t op) {
    const int x = op >> 6, y = (op >> 3) & 7, z = op & 7, p = y >> 1, q = y & 1;
    switch (x) {
        case 0:
            switch (z) {
                case 0:
                    if (y == 0) return;
                    if (y == 1) { uint16_t nn = imm16(); w8(nn, uint8_t(sp)); w8(uint16_t(nn + 1), uint8_t(sp >> 8)); return; }
                    if (y == 2) { imm8(); w8(0xFF4D, 0xFF); return; }  // STOP: signals the bus (CGB speed switch)
                    {
                        int8_t dd = int8_t(imm8());
                        if (y == 3 || cond(y - 4)) { pc = uint16_t(pc + dd); if (y != 3) extra = 1; }
                    }
                    return;
                case 1:
                    if (!q) { set_rp(p, imm16()); return; }
                    {
                        uint32_t r = hl() + rp(p);
                        f = uint8_t((f & FZ) | (((hl() & 0xFFF) + (rp(p) & 0xFFF)) > 0xFFF ? FH : 0) | (r > 0xFFFF ? FC : 0));
                        set_hl(uint16_t(r));
                    }
                    return;
                case 2: {
                    uint16_t addr = p == 0 ? bc() : p == 1 ? de() : hl();
                    if (!q) w8(addr, a); else a = r8(addr);
                    if (p == 2) set_hl(uint16_t(hl() + 1));
                    if (p == 3) set_hl(uint16_t(hl() - 1));
                    return;
                }
                case 3: set_rp(p, uint16_t(rp(p) + (q ? -1 : 1))); return;
                case 4: { uint8_t v = uint8_t(reg(y) + 1); f = uint8_t((f & FC) | (v ? 0 : FZ) | ((v & 0xF) == 0 ? FH : 0)); set_reg(y, v); return; }
                case 5: { uint8_t v = uint8_t(reg(y) - 1); f = uint8_t((f & FC) | FN | (v ? 0 : FZ) | ((v & 0xF) == 0xF ? FH : 0)); set_reg(y, v); return; }
                case 6: set_reg(y, imm8()); return;
                default:
                    switch (y) {
                        case 0: case 1: case 2: case 3: a = cb_rot(y, a); f &= ~FZ; return;
                        case 4: {  // DAA
                            uint8_t adj = 0; bool cy = f & FC;
                            if (f & FN) {
                                if (f & FH) adj |= 0x06;
                                if (cy) adj |= 0x60;
                                a = uint8_t(a - adj);
                            } else {
                                if ((f & FH) || (a & 0xF) > 9) adj |= 0x06;
                                if (cy || a > 0x99) { adj |= 0x60; cy = true; }
                                a = uint8_t(a + adj);
                            }
                            f = uint8_t((f & FN) | (a ? 0 : FZ) | (cy ? FC : 0));
                            return;
                        }
                        case 5: a = uint8_t(~a); f |= FN | FH; return;
                        case 6: f = uint8_t((f & FZ) | FC); return;
                        default: f = uint8_t((f & FZ) | ((f & FC) ? 0 : FC)); return;
                    }
            }
        case 1:
            if (y == 6 && z == 6) { halted = true; return; }
            set_reg(y, reg(z));
            return;
        case 2: alu(y, reg(z)); return;
        default:
            switch (z) {
                case 0:
                    if (y < 4) { if (cond(y)) { pc = pop(); extra = 3; } return; }
                    if (y == 4) { w8(uint16_t(0xFF00 | imm8()), a); return; }
                    if (y == 5) { sp = add_sp(int8_t(imm8())); return; }
                    if (y == 6) { a = r8(uint16_t(0xFF00 | imm8())); return; }
                    set_hl(add_sp(int8_t(imm8())));
                    return;
                case 1:
                    if (!q) {
                        uint16_t v = pop();
                        if (p == 3) { a = uint8_t(v >> 8); f = uint8_t(v) & 0xF0; } else set_rp(p, v);
                        return;
                    }
                    switch (p) {
                        case 0: pc = pop(); return;
                        case 1: pc = pop(); ime = true; return;
                        case 2: pc = hl(); return;
                        default: sp = hl(); return;
                    }
                case 2:
                    if (y < 4) { uint16_t nn = imm16(); if (cond(y)) { pc = nn; extra = 1; } return; }
                    if (y == 4) { w8(uint16_t(0xFF00 | c), a); return; }
                    if (y == 5) { w8(imm16(), a); return; }
                    if (y == 6) { a = r8(uint16_t(0xFF00 | c)); return; }
                    a = r8(imm16());
                    return;
                case 3:
                    if (y == 0) { pc = imm16(); return; }
                    if (y == 6) { ime = false; ei_delay = 0; return; }
                    if (y == 7) { ei_delay = 2; return; }
                    return;  // CB handled in step(); others are illegal
                case 4:
                    if (y < 4) { uint16_t nn = imm16(); if (cond(y)) { push(pc); pc = nn; extra = 3; } }
                    return;
                case 5:
                    if (!q) { push(p == 3 ? uint16_t(a << 8 | (f & 0xF0)) : rp(p)); return; }
                    if (p == 0) { uint16_t nn = imm16(); push(pc); pc = nn; }
                    return;
                case 6: alu(y, imm8()); return;
                default: push(pc); pc = uint16_t(y * 8); return;
            }
    }
}

uint32_t Sm83::step() {
    uint8_t pending = r8(0xFFFF) & r8(0xFF0F) & 0x1F;
    if (pending) {
        halted = false;
        if (ime) {
            int bit = 0;
            while (!(pending & (1 << bit))) bit++;
            w8(0xFF0F, uint8_t(r8(0xFF0F) & ~(1 << bit)));
            ime = false;
            push(pc);
            pc = uint16_t(0x40 + bit * 8);
            return 20;
        }
    }
    if (halted) return 4;
    if (ei_delay && --ei_delay == 0) ime = true;

    extra = 0;
    uint8_t op = imm8();
    if (op == 0xCB) return cb();
    exec(op);
    uint32_t m = kCycles[op];
    return (m ? m : 1) * 4 + extra * 4;
}

extern "C" {

Sm83* sm83_new(void* ctx, sm83_read_fn read, sm83_write_fn write) {
    Sm83* c = new Sm83();
    c->ctx = ctx; c->rd = read; c->wr = write;
    return c;
}
void sm83_free(Sm83* c) { delete c; }
uint32_t sm83_step(Sm83* c) { return c->step(); }
void sm83_get_regs(const Sm83* c, Sm83Regs* o) {
    o->a = c->a; o->f = c->f; o->b = c->b; o->c = c->c; o->d = c->d; o->e = c->e; o->h = c->h; o->l = c->l;
    o->sp = c->sp; o->pc = c->pc; o->ime = c->ime; o->halted = c->halted;
}
void sm83_set_regs(Sm83* c, const Sm83Regs* i) {
    c->a = i->a; c->f = i->f & 0xF0; c->b = i->b; c->c = i->c; c->d = i->d; c->e = i->e; c->h = i->h; c->l = i->l;
    c->sp = i->sp; c->pc = i->pc; c->ime = i->ime; c->halted = i->halted;
}


// ---- state ----
unsigned long sm83_state_size(void) { return (unsigned long)sizeof(Sm83); }
void sm83_save_state(const Sm83* c, uint8_t* out) {
    Sm83 tmp = *c;
    tmp.ctx = {}; tmp.rd = {}; tmp.wr = {};
    memcpy(out, &tmp, sizeof(tmp));
}
void sm83_load_state(Sm83* c, const uint8_t* in) {
    Sm83 keep = *c;
    memcpy((void*)c, in, sizeof(*c));
    c->ctx = keep.ctx;
    c->rd = keep.rd;
    c->wr = keep.wr;
}

}  // extern "C"
