#include "x86.h"

#include <string.h>

namespace {

enum : uint16_t { CF = 1, PF = 4, AF = 16, ZF = 64, SF = 128, TF = 256, IF_ = 512, DF = 1024, OF = 2048 };
enum { rAX, rCX, rDX, rBX, rSP, rBP, rSI, rDI };
enum { sES, sCS, sSS, sDS };

struct ParityTable {
    uint8_t even[256];
    ParityTable() {
        for (int i = 0; i < 256; i++) {
            int b = 0;
            for (int j = 0; j < 8; j++) b += (i >> j) & 1;
            even[i] = (b & 1) == 0;
        }
    }
};
const ParityTable kParity;

}  // namespace

struct X86 {
    uint16_t r[8] = {};
    uint16_t s[4] = {};
    uint16_t ip = 0;
    uint16_t fl = 0xF002;
    uint8_t* mem = nullptr;

    void* ctx = nullptr;
    x86_in_fn in = nullptr;
    x86_out_fn out = nullptr;
    x86_trap_fn trap = nullptr;

    bool halted = false;
    bool stop_req = false;
    uint8_t irq_q[32];
    int irq_head = 0, irq_len = 0;

    // per-instruction decode state
    int seg_ovr = -1;
    int mreg = 0;
    struct { bool reg; int idx; uint16_t seg, off; } ea{};

    // ---------------------------------------------------------------- memory
    static inline uint32_t lin(uint16_t seg, uint16_t off) { return ((uint32_t(seg) << 4) + off) & 0xFFFFF; }
    inline uint8_t rb(uint32_t a) const { return mem[a & 0xFFFFF]; }
    inline void wb(uint32_t a, uint8_t v) {
        a &= 0xFFFFF;
        if (a >= 0xF0000) return;  // BIOS ROM
        mem[a] = v;
    }
    inline uint8_t rb(uint16_t seg, uint16_t off) const { return rb(lin(seg, off)); }
    inline uint16_t rw(uint16_t seg, uint16_t off) const { return rb(seg, off) | (uint16_t(rb(seg, uint16_t(off + 1))) << 8); }
    inline void wb(uint16_t seg, uint16_t off, uint8_t v) { wb(lin(seg, off), v); }
    inline void ww(uint16_t seg, uint16_t off, uint16_t v) { wb(seg, off, uint8_t(v)); wb(seg, uint16_t(off + 1), uint8_t(v >> 8)); }

    inline uint8_t fetch8() { return rb(s[sCS], ip++); }
    inline uint16_t fetch16() { uint16_t v = rw(s[sCS], ip); ip += 2; return v; }
    inline void push(uint16_t v) { r[rSP] -= 2; ww(s[sSS], r[rSP], v); }
    inline uint16_t pop() { uint16_t v = rw(s[sSS], r[rSP]); r[rSP] += 2; return v; }
    inline uint16_t dseg() const { return s[seg_ovr >= 0 ? seg_ovr : sDS]; }

    // ---------------------------------------------------------------- registers
    inline uint8_t get8(int i) const { return i < 4 ? uint8_t(r[i]) : uint8_t(r[i - 4] >> 8); }
    inline void set8(int i, uint8_t v) {
        if (i < 4) r[i] = (r[i] & 0xFF00) | v;
        else r[i - 4] = (r[i - 4] & 0x00FF) | (uint16_t(v) << 8);
    }

    // ---------------------------------------------------------------- ModRM
    void modrm() {
        uint8_t m = fetch8();
        int mod = m >> 6, rm = m & 7;
        mreg = (m >> 3) & 7;
        if (mod == 3) { ea.reg = true; ea.idx = rm; return; }
        ea.reg = false;
        uint16_t off = 0;
        int def = sDS;
        switch (rm) {
            case 0: off = r[rBX] + r[rSI]; break;
            case 1: off = r[rBX] + r[rDI]; break;
            case 2: off = r[rBP] + r[rSI]; def = sSS; break;
            case 3: off = r[rBP] + r[rDI]; def = sSS; break;
            case 4: off = r[rSI]; break;
            case 5: off = r[rDI]; break;
            case 6: if (mod == 0) off = fetch16(); else { off = r[rBP]; def = sSS; } break;
            case 7: off = r[rBX]; break;
        }
        if (mod == 1) off += uint16_t(int16_t(int8_t(fetch8())));
        else if (mod == 2) off += fetch16();
        ea.seg = s[seg_ovr >= 0 ? seg_ovr : def];
        ea.off = off;
    }
    inline uint8_t rm8() const { return ea.reg ? get8(ea.idx) : rb(ea.seg, ea.off); }
    inline uint16_t rm16() const { return ea.reg ? r[ea.idx] : rw(ea.seg, ea.off); }
    inline void wrm8(uint8_t v) { if (ea.reg) set8(ea.idx, v); else wb(ea.seg, ea.off, v); }
    inline void wrm16(uint16_t v) { if (ea.reg) r[ea.idx] = v; else ww(ea.seg, ea.off, v); }
    inline uint16_t rmw(bool w) const { return w ? rm16() : rm8(); }
    inline void wrmw(bool w, uint16_t v) { if (w) wrm16(v); else wrm8(uint8_t(v)); }
    inline uint16_t regw(bool w, int i) const { return w ? r[i] : get8(i); }
    inline void setregw(bool w, int i, uint16_t v) { if (w) r[i] = v; else set8(i, uint8_t(v)); }

    // ---------------------------------------------------------------- flags
    inline void flag(uint16_t f, bool on) { fl = on ? (fl | f) : (fl & ~f); }
    inline void szp(uint32_t v, bool w) {
        uint32_t mask = w ? 0xFFFF : 0xFF;
        v &= mask;
        flag(ZF, v == 0);
        flag(SF, v & (w ? 0x8000 : 0x80));
        flag(PF, kParity.even[v & 0xFF]);
    }
    uint16_t add(uint16_t a, uint16_t b, uint16_t c, bool w) {
        uint32_t res = uint32_t(a) + b + c;
        uint32_t sign = w ? 0x8000 : 0x80;
        flag(CF, res > (w ? 0xFFFFu : 0xFFu));
        flag(OF, ((a ^ res) & (b ^ res) & sign) != 0);
        flag(AF, ((a ^ b ^ res) & 0x10) != 0);
        szp(res, w);
        return uint16_t(res);
    }
    uint16_t sub(uint16_t a, uint16_t b, uint16_t c, bool w) {
        uint32_t res = uint32_t(a) - b - c;
        uint32_t sign = w ? 0x8000 : 0x80;
        flag(CF, uint32_t(a) < uint32_t(b) + c);
        flag(OF, ((a ^ b) & (a ^ res) & sign) != 0);
        flag(AF, ((a ^ b ^ res) & 0x10) != 0);
        szp(res, w);
        return uint16_t(res);
    }
    uint16_t logic(uint16_t v, bool w) {
        fl &= ~(CF | OF | AF);
        szp(v, w);
        return v;
    }
    uint16_t alu(int op, uint16_t a, uint16_t b, bool w) {
        uint16_t m = w ? 0xFFFF : 0xFF;
        switch (op) {
            case 0: return add(a, b, 0, w) & m;
            case 1: return logic(a | b, w) & m;
            case 2: return add(a, b, fl & CF, w) & m;
            case 3: return sub(a, b, fl & CF, w) & m;
            case 4: return logic(a & b, w) & m;
            case 5: return sub(a, b, 0, w) & m;
            case 6: return logic(a ^ b, w) & m;
            default: sub(a, b, 0, w); return a;  // CMP
        }
    }
    uint16_t incdec(uint16_t v, bool dec, bool w) {
        uint16_t cf = fl & CF;
        uint16_t res = dec ? sub(v, 1, 0, w) : add(v, 1, 0, w);
        fl = (fl & ~CF) | cf;
        return res & (w ? 0xFFFF : 0xFF);
    }
    uint16_t shift(int op, uint16_t v, uint8_t cnt, bool w) {
        cnt &= 0x1F;
        if (!cnt) return v;
        const int bits = w ? 16 : 8;
        const uint32_t mask = w ? 0xFFFF : 0xFF, sign = w ? 0x8000 : 0x80;
        uint32_t x = v;
        const uint32_t orig = v;
        for (int i = 0; i < cnt; i++) {
            uint32_t c;
            switch (op) {
                case 0: c = (x >> (bits - 1)) & 1; x = ((x << 1) | c) & mask; break;
                case 1: c = x & 1; x = (x >> 1) | (c << (bits - 1)); break;
                case 2: c = (x >> (bits - 1)) & 1; x = ((x << 1) | (fl & CF)) & mask; break;
                case 3: c = x & 1; x = (x >> 1) | (uint32_t(fl & CF) << (bits - 1)); break;
                case 4: case 6: c = (x >> (bits - 1)) & 1; x = (x << 1) & mask; break;
                case 5: c = x & 1; x >>= 1; break;
                default: c = x & 1; x = (x >> 1) | (x & sign); break;
            }
            flag(CF, c);
        }
        const bool msb = (x & sign) != 0;
        switch (op) {
            case 0: case 2: case 4: case 6: flag(OF, msb != bool(fl & CF)); break;
            case 1: case 3: flag(OF, msb != bool(x & (sign >> 1))); break;
            case 5: flag(OF, (orig & sign) != 0); break;
            default: flag(OF, false); break;
        }
        if (op >= 4) szp(x, w);
        return uint16_t(x);
    }
    bool cond(int n) const {
        bool c;
        switch (n >> 1) {
            case 0: c = fl & OF; break;
            case 1: c = fl & CF; break;
            case 2: c = fl & ZF; break;
            case 3: c = (fl & CF) || (fl & ZF); break;
            case 4: c = fl & SF; break;
            case 5: c = fl & PF; break;
            case 6: c = bool(fl & SF) != bool(fl & OF); break;
            default: c = (fl & ZF) || (bool(fl & SF) != bool(fl & OF)); break;
        }
        return (n & 1) ? !c : c;
    }

    // ---------------------------------------------------------------- control
    void interrupt(uint8_t n) {
        push(fl);
        fl &= ~(IF_ | TF);
        push(s[sCS]);
        push(ip);
        ip = rw(0, uint16_t(n * 4));
        s[sCS] = rw(0, uint16_t(n * 4 + 2));
    }
    void set_flags_word(uint16_t v) { fl = (v & 0x0FD5) | 0xF002; }

    void string_op(uint8_t op, int rep) {
        const bool w = op & 1;
        const uint16_t d = (fl & DF) ? uint16_t(w ? -2 : -1) : uint16_t(w ? 2 : 1);
        const uint16_t src = dseg();
        for (;;) {
            if (rep && r[rCX] == 0) break;
            bool compare = false;
            switch (op & 0xFE) {
                case 0xA4: {
                    if (w) ww(s[sES], r[rDI], rw(src, r[rSI])); else wb(s[sES], r[rDI], rb(src, r[rSI]));
                    r[rSI] += d; r[rDI] += d; break;
                }
                case 0xA6: {
                    uint16_t a = w ? rw(src, r[rSI]) : rb(src, r[rSI]);
                    uint16_t b = w ? rw(s[sES], r[rDI]) : rb(s[sES], r[rDI]);
                    sub(a, b, 0, w);
                    r[rSI] += d; r[rDI] += d; compare = true; break;
                }
                case 0xAA: if (w) ww(s[sES], r[rDI], r[rAX]); else wb(s[sES], r[rDI], uint8_t(r[rAX])); r[rDI] += d; break;
                case 0xAC: if (w) r[rAX] = rw(src, r[rSI]); else set8(0, rb(src, r[rSI])); r[rSI] += d; break;
                case 0xAE: {
                    uint16_t b = w ? rw(s[sES], r[rDI]) : rb(s[sES], r[rDI]);
                    sub(w ? r[rAX] : get8(0), b, 0, w);
                    r[rDI] += d; compare = true; break;
                }
                case 0x6C: {
                    uint16_t v = in(ctx, r[rDX], w);
                    if (w) ww(s[sES], r[rDI], v); else wb(s[sES], r[rDI], uint8_t(v));
                    r[rDI] += d; break;
                }
                case 0x6E: out(ctx, r[rDX], w ? rw(src, r[rSI]) : rb(src, r[rSI]), w); r[rSI] += d; break;
            }
            if (!rep) break;
            r[rCX]--;
            if (compare && rep == 1 && !(fl & ZF)) break;
            if (compare && rep == 2 && (fl & ZF)) break;
        }
    }

    void grp3(bool w) {
        modrm();
        uint16_t v = rmw(w);
        switch (mreg) {
            case 0: case 1: logic(v & (w ? fetch16() : fetch8()), w); break;
            case 2: wrmw(w, ~v); break;
            case 3: { uint16_t res = sub(0, v, 0, w); flag(CF, v != 0); wrmw(w, res); break; }
            case 4:
                if (w) {
                    uint32_t p = uint32_t(r[rAX]) * v;
                    r[rAX] = uint16_t(p); r[rDX] = uint16_t(p >> 16);
                    flag(CF, r[rDX] != 0); flag(OF, r[rDX] != 0);
                } else {
                    r[rAX] = uint16_t(get8(0) * v);
                    flag(CF, (r[rAX] >> 8) != 0); flag(OF, (r[rAX] >> 8) != 0);
                }
                break;
            case 5:
                if (w) {
                    int32_t p = int32_t(int16_t(r[rAX])) * int16_t(v);
                    r[rAX] = uint16_t(p); r[rDX] = uint16_t(uint32_t(p) >> 16);
                    bool o = p != int32_t(int16_t(p)); flag(CF, o); flag(OF, o);
                } else {
                    int16_t p = int16_t(int8_t(get8(0)) * int8_t(v));
                    r[rAX] = uint16_t(p);
                    bool o = p != int16_t(int8_t(p)); flag(CF, o); flag(OF, o);
                }
                break;
            case 6:
                if (v == 0) { interrupt(0); break; }
                if (w) {
                    uint32_t n = (uint32_t(r[rDX]) << 16) | r[rAX];
                    uint32_t q = n / v;
                    if (q > 0xFFFF) { interrupt(0); break; }
                    r[rAX] = uint16_t(q); r[rDX] = uint16_t(n % v);
                } else {
                    uint16_t n = r[rAX];
                    uint16_t q = n / v;
                    if (q > 0xFF) { interrupt(0); break; }
                    set8(0, uint8_t(q)); set8(4, uint8_t(n % v));
                }
                break;
            default:
                if (v == 0) { interrupt(0); break; }
                if (w) {
                    int32_t n = int32_t((uint32_t(r[rDX]) << 16) | r[rAX]);
                    int32_t dv = int16_t(v);
                    if (n == INT32_MIN && dv == -1) { interrupt(0); break; }
                    int32_t q = n / dv;
                    if (q > 32767 || q < -32768) { interrupt(0); break; }
                    r[rAX] = uint16_t(q); r[rDX] = uint16_t(n % dv);
                } else {
                    int16_t n = int16_t(r[rAX]);
                    int16_t dv = int8_t(v);
                    int16_t q = n / dv;
                    if (q > 127 || q < -128) { interrupt(0); break; }
                    set8(0, uint8_t(q)); set8(4, uint8_t(n % dv));
                }
                break;
        }
    }

    void exec(uint8_t op, int rep);
    uint32_t run(uint32_t max);
};

void X86::exec(uint8_t op, int rep) {
    // ALU block 00-3F (except prefixes / misc handled below)
    if (op < 0x40 && (op & 7) < 6) {
        const int aop = op >> 3;
        const bool w = op & 1;
        switch (op & 7) {
            case 0: case 1: {
                modrm();
                uint16_t res = alu(aop, rmw(w), regw(w, mreg), w);
                if (aop != 7) wrmw(w, res);
                return;
            }
            case 2: case 3: {
                modrm();
                uint16_t res = alu(aop, regw(w, mreg), rmw(w), w);
                if (aop != 7) setregw(w, mreg, res);
                return;
            }
            case 4: { uint16_t res = alu(aop, get8(0), fetch8(), false); if (aop != 7) set8(0, uint8_t(res)); return; }
            case 5: { uint16_t res = alu(aop, r[rAX], fetch16(), true); if (aop != 7) r[rAX] = res; return; }
        }
    }

    switch (op) {
        case 0x06: push(s[sES]); return;
        case 0x07: s[sES] = pop(); return;
        case 0x0E: push(s[sCS]); return;
        case 0x16: push(s[sSS]); return;
        case 0x17: s[sSS] = pop(); return;
        case 0x1E: push(s[sDS]); return;
        case 0x1F: s[sDS] = pop(); return;
        case 0x0F: {
            uint8_t b = fetch8();
            if (b == 0xFF) { uint8_t n = fetch8(); if (trap) trap(ctx, this, n); return; }
            ip -= 2;  // back to the 0F for the fault
            interrupt(6);
            return;
        }
        case 0x27: {  // DAA
            uint8_t al = get8(0), old = al; bool ocf = fl & CF;
            if ((al & 0x0F) > 9 || (fl & AF)) { al += 6; flag(AF, true); } else flag(AF, false);
            if (old > 0x99 || ocf) { al += 0x60; flag(CF, true); } else flag(CF, false);
            set8(0, al); szp(al, false); return;
        }
        case 0x2F: {  // DAS
            uint8_t al = get8(0), old = al; bool ocf = fl & CF;
            if ((al & 0x0F) > 9 || (fl & AF)) { al -= 6; flag(AF, true); } else flag(AF, false);
            if (old > 0x99 || ocf) { al -= 0x60; flag(CF, true); } else flag(CF, false);
            set8(0, al); szp(al, false); return;
        }
        case 0x37: case 0x3F: {  // AAA / AAS
            bool adj = (get8(0) & 0x0F) > 9 || (fl & AF);
            if (adj) {
                if (op == 0x37) r[rAX] += 0x106;
                else { set8(0, uint8_t(get8(0) - 6)); set8(4, uint8_t(get8(4) - 1)); }
            }
            flag(AF, adj); flag(CF, adj);
            set8(0, get8(0) & 0x0F);
            return;
        }
    }
    if (op >= 0x40 && op <= 0x47) { r[op & 7] = incdec(r[op & 7], false, true); return; }
    if (op >= 0x48 && op <= 0x4F) { r[op & 7] = incdec(r[op & 7], true, true); return; }
    if (op >= 0x50 && op <= 0x57) { push(r[op & 7]); return; }
    if (op >= 0x58 && op <= 0x5F) { r[op & 7] = pop(); return; }
    if (op >= 0x70 && op <= 0x7F) { int8_t d = int8_t(fetch8()); if (cond(op & 0xF)) ip += d; return; }
    if (op >= 0x91 && op <= 0x97) { uint16_t t = r[rAX]; r[rAX] = r[op & 7]; r[op & 7] = t; return; }
    if (op >= 0xB0 && op <= 0xB7) { set8(op & 7, fetch8()); return; }
    if (op >= 0xB8 && op <= 0xBF) { r[op & 7] = fetch16(); return; }
    if (op >= 0xD8 && op <= 0xDF) { modrm(); return; }  // x87 escape: no FPU

    switch (op) {
        case 0x60: { uint16_t sp = r[rSP]; push(r[rAX]); push(r[rCX]); push(r[rDX]); push(r[rBX]); push(sp); push(r[rBP]); push(r[rSI]); push(r[rDI]); return; }
        case 0x61: { r[rDI] = pop(); r[rSI] = pop(); r[rBP] = pop(); pop(); r[rBX] = pop(); r[rDX] = pop(); r[rCX] = pop(); r[rAX] = pop(); return; }
        case 0x62: modrm(); return;  // BOUND: no range check
        case 0x68: push(fetch16()); return;
        case 0x6A: push(uint16_t(int16_t(int8_t(fetch8())))); return;
        case 0x69: case 0x6B: {
            modrm();
            int32_t a = int16_t(rm16());
            int32_t b = op == 0x69 ? int16_t(fetch16()) : int8_t(fetch8());
            int32_t p = a * b;
            r[mreg] = uint16_t(p);
            bool o = p != int32_t(int16_t(p)); flag(CF, o); flag(OF, o);
            return;
        }
        case 0x6C: case 0x6D: case 0x6E: case 0x6F: string_op(op, rep ? 1 : 0); return;

        case 0x80: case 0x81: case 0x82: case 0x83: {
            const bool w = op & 1;
            modrm();
            uint16_t imm = op == 0x81 ? fetch16() : (op == 0x83 ? uint16_t(int16_t(int8_t(fetch8()))) : fetch8());
            uint16_t res = alu(mreg, rmw(w), imm, w);
            if (mreg != 7) wrmw(w, res);
            return;
        }
        case 0x84: case 0x85: { bool w = op & 1; modrm(); logic(rmw(w) & regw(w, mreg), w); return; }
        case 0x86: case 0x87: { bool w = op & 1; modrm(); uint16_t a = rmw(w); wrmw(w, regw(w, mreg)); setregw(w, mreg, a); return; }
        case 0x88: modrm(); wrm8(get8(mreg)); return;
        case 0x89: modrm(); wrm16(r[mreg]); return;
        case 0x8A: modrm(); set8(mreg, rm8()); return;
        case 0x8B: modrm(); r[mreg] = rm16(); return;
        case 0x8C: modrm(); wrm16(s[mreg & 3]); return;
        case 0x8D: modrm(); r[mreg] = ea.off; return;
        case 0x8E: modrm(); s[mreg & 3] = rm16(); return;
        case 0x8F: { modrm(); uint16_t v = pop(); wrm16(v); return; }
        case 0x90: return;
        case 0x98: r[rAX] = uint16_t(int16_t(int8_t(get8(0)))); return;
        case 0x99: r[rDX] = (r[rAX] & 0x8000) ? 0xFFFF : 0; return;
        case 0x9A: { uint16_t o = fetch16(), sg = fetch16(); push(s[sCS]); push(ip); s[sCS] = sg; ip = o; return; }
        case 0x9B: return;
        case 0x9C: push(fl); return;
        case 0x9D: set_flags_word(pop()); return;
        case 0x9E: fl = (fl & 0xFF00) | (get8(4) & 0xD5) | 2; return;
        case 0x9F: set8(4, uint8_t(fl)); return;
        case 0xA0: set8(0, rb(dseg(), fetch16())); return;
        case 0xA1: r[rAX] = rw(dseg(), fetch16()); return;
        case 0xA2: wb(dseg(), fetch16(), get8(0)); return;
        case 0xA3: ww(dseg(), fetch16(), r[rAX]); return;
        case 0xA4: case 0xA5: case 0xAA: case 0xAB: case 0xAC: case 0xAD: string_op(op, rep ? 1 : 0); return;
        case 0xA6: case 0xA7: case 0xAE: case 0xAF: string_op(op, rep); return;
        case 0xA8: logic(get8(0) & fetch8(), false); return;
        case 0xA9: logic(r[rAX] & fetch16(), true); return;

        case 0xC0: case 0xC1: { bool w = op & 1; modrm(); uint8_t c = fetch8(); wrmw(w, shift(mreg, rmw(w), c, w)); return; }
        case 0xD0: case 0xD1: { bool w = op & 1; modrm(); wrmw(w, shift(mreg, rmw(w), 1, w)); return; }
        case 0xD2: case 0xD3: { bool w = op & 1; modrm(); wrmw(w, shift(mreg, rmw(w), get8(1), w)); return; }
        case 0xC2: { uint16_t n = fetch16(); ip = pop(); r[rSP] += n; return; }
        case 0xC3: ip = pop(); return;
        case 0xC4: case 0xC5: {
            modrm();
            r[mreg] = rw(ea.seg, ea.off);
            s[op == 0xC4 ? sES : sDS] = rw(ea.seg, uint16_t(ea.off + 2));
            return;
        }
        case 0xC6: modrm(); wrm8(fetch8()); return;
        case 0xC7: modrm(); wrm16(fetch16()); return;
        case 0xC8: {
            uint16_t size = fetch16();
            uint8_t level = fetch8() & 31;
            push(r[rBP]);
            uint16_t frame = r[rSP];
            if (level > 0) {
                for (int i = 1; i < level; i++) { r[rBP] -= 2; push(rw(s[sSS], r[rBP])); }
                push(frame);
            }
            r[rBP] = frame;
            r[rSP] -= size;
            return;
        }
        case 0xC9: r[rSP] = r[rBP]; r[rBP] = pop(); return;
        case 0xCA: { uint16_t n = fetch16(); ip = pop(); s[sCS] = pop(); r[rSP] += n; return; }
        case 0xCB: ip = pop(); s[sCS] = pop(); return;
        case 0xCC: interrupt(3); return;
        case 0xCD: interrupt(fetch8()); return;
        case 0xCE: if (fl & OF) interrupt(4); return;
        case 0xCF: ip = pop(); s[sCS] = pop(); set_flags_word(pop()); return;
        case 0xD4: {
            uint8_t base = fetch8();
            if (!base) { interrupt(0); return; }
            uint8_t al = get8(0);
            set8(4, al / base); set8(0, al % base); szp(get8(0), false);
            return;
        }
        case 0xD5: {
            uint8_t base = fetch8();
            set8(0, uint8_t(get8(0) + get8(4) * base)); set8(4, 0); szp(get8(0), false);
            return;
        }
        case 0xD6: set8(0, (fl & CF) ? 0xFF : 0); return;
        case 0xD7: set8(0, rb(dseg(), uint16_t(r[rBX] + get8(0)))); return;

        case 0xE0: case 0xE1: case 0xE2: case 0xE3: {
            int8_t d = int8_t(fetch8());
            bool jump;
            if (op == 0xE3) jump = r[rCX] == 0;
            else {
                r[rCX]--;
                jump = r[rCX] != 0;
                if (op == 0xE1) jump = jump && (fl & ZF);
                if (op == 0xE0) jump = jump && !(fl & ZF);
            }
            if (jump) ip += d;
            return;
        }
        case 0xE4: set8(0, uint8_t(in(ctx, fetch8(), 0))); return;
        case 0xE5: r[rAX] = in(ctx, fetch8(), 1); return;
        case 0xE6: out(ctx, fetch8(), get8(0), 0); return;
        case 0xE7: out(ctx, fetch8(), r[rAX], 1); return;
        case 0xE8: { int16_t d = int16_t(fetch16()); push(ip); ip += d; return; }
        case 0xE9: { int16_t d = int16_t(fetch16()); ip += d; return; }
        case 0xEA: { uint16_t o = fetch16(), sg = fetch16(); ip = o; s[sCS] = sg; return; }
        case 0xEB: { int8_t d = int8_t(fetch8()); ip += d; return; }
        case 0xEC: set8(0, uint8_t(in(ctx, r[rDX], 0))); return;
        case 0xED: r[rAX] = in(ctx, r[rDX], 1); return;
        case 0xEE: out(ctx, r[rDX], get8(0), 0); return;
        case 0xEF: out(ctx, r[rDX], r[rAX], 1); return;

        case 0xF4: halted = true; return;
        case 0xF5: fl ^= CF; return;
        case 0xF6: grp3(false); return;
        case 0xF7: grp3(true); return;
        case 0xF8: fl &= ~CF; return;
        case 0xF9: fl |= CF; return;
        case 0xFA: fl &= ~IF_; return;
        case 0xFB: fl |= IF_; return;
        case 0xFC: fl &= ~DF; return;
        case 0xFD: fl |= DF; return;
        case 0xFE: {
            modrm();
            if (mreg < 2) wrm8(uint8_t(incdec(rm8(), mreg == 1, false)));
            return;
        }
        case 0xFF: {
            modrm();
            switch (mreg) {
                case 0: wrm16(incdec(rm16(), false, true)); return;
                case 1: wrm16(incdec(rm16(), true, true)); return;
                case 2: { uint16_t t = rm16(); push(ip); ip = t; return; }
                case 3: {
                    uint16_t o = rw(ea.seg, ea.off), sg = rw(ea.seg, uint16_t(ea.off + 2));
                    push(s[sCS]); push(ip); s[sCS] = sg; ip = o; return;
                }
                case 4: ip = rm16(); return;
                case 5: ip = rw(ea.seg, ea.off); s[sCS] = rw(ea.seg, uint16_t(ea.off + 2)); return;
                default: push(rm16()); return;
            }
        }
        default:
            interrupt(6);  // invalid opcode
            return;
    }
}

uint32_t X86::run(uint32_t max) {
    stop_req = false;
    uint32_t n = 0;
    while (n < max && !stop_req) {
        if (irq_len && (fl & IF_)) {
            uint8_t v = irq_q[irq_head];
            irq_head = (irq_head + 1) % 32;
            irq_len--;
            halted = false;
            interrupt(v);
        }
        if (halted) break;
        seg_ovr = -1;
        int rep = 0;
        uint8_t op;
        for (;;) {
            op = fetch8();
            switch (op) {
                case 0x26: seg_ovr = sES; continue;
                case 0x2E: seg_ovr = sCS; continue;
                case 0x36: seg_ovr = sSS; continue;
                case 0x3E: seg_ovr = sDS; continue;
                case 0xF0: continue;
                case 0xF2: rep = 2; continue;
                case 0xF3: rep = 1; continue;
            }
            break;
        }
        exec(op, rep);
        n++;
    }
    return n;
}

extern "C" {

X86* x86_new(void* ctx, x86_in_fn in, x86_out_fn out, x86_trap_fn trap) {
    X86* c = new X86();
    c->mem = new uint8_t[X86_MEM_SIZE];
    memset(c->mem, 0, X86_MEM_SIZE);
    c->ctx = ctx; c->in = in; c->out = out; c->trap = trap;
    return c;
}
void x86_free(X86* c) {
    delete[] c->mem;
    delete c;
}
uint8_t* x86_mem(X86* c) { return c->mem; }
void x86_get_regs(const X86* c, X86Regs* o) {
    o->ax = c->r[rAX]; o->cx = c->r[rCX]; o->dx = c->r[rDX]; o->bx = c->r[rBX];
    o->sp = c->r[rSP]; o->bp = c->r[rBP]; o->si = c->r[rSI]; o->di = c->r[rDI];
    o->es = c->s[sES]; o->cs = c->s[sCS]; o->ss = c->s[sSS]; o->ds = c->s[sDS];
    o->ip = c->ip; o->flags = c->fl;
}
void x86_set_regs(X86* c, const X86Regs* i) {
    c->r[rAX] = i->ax; c->r[rCX] = i->cx; c->r[rDX] = i->dx; c->r[rBX] = i->bx;
    c->r[rSP] = i->sp; c->r[rBP] = i->bp; c->r[rSI] = i->si; c->r[rDI] = i->di;
    c->s[sES] = i->es; c->s[sCS] = i->cs; c->s[sSS] = i->ss; c->s[sDS] = i->ds;
    c->ip = i->ip; c->fl = (i->flags & 0x0FD5) | 0xF002;
}
void x86_irq(X86* c, uint8_t v) {
    if (c->irq_len < 32) {
        c->irq_q[(c->irq_head + c->irq_len) % 32] = v;
        c->irq_len++;
    }
}
int x86_halted(const X86* c) { return c->halted; }
uint32_t x86_run(X86* c, uint32_t max) { return c->run(max); }
void x86_stop(X86* c) { c->stop_req = true; }

}  // extern "C"
