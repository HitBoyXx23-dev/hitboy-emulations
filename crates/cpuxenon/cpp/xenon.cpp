// IBM Xenon interpreter. See xenon.h.
//
// Runs in 64-bit mode (MSR[SF] = 1) as Xbox 360 titles do: GPRs, CTR and LR are 64 bits,
// CR0 compares full 64-bit results, and effective addresses are truncated to 32 bits.
#include "xenon.h"
#include <math.h>
#include <stdlib.h>
#include <string.h>
#if defined(_MSC_VER)
#include <intrin.h>
#endif

namespace {

enum : uint32_t { XER_SO = 1u << 31, XER_OV = 1u << 30, XER_CA = 1u << 29 };

inline uint64_t rotl64(uint64_t v, uint32_t n) {
    n &= 63;
    return n ? (v << n) | (v >> (64 - n)) : v;
}

// 32-bit rotate in 64-bit mode: the word is doubled into both halves first.
inline uint64_t rotl32(uint64_t v, uint32_t n) {
    uint64_t x = (uint32_t)v;
    return rotl64(x | (x << 32), n & 31);
}

// Bits mb..me set (bit 0 = MSB), wrapping when mb > me.
inline uint64_t mask64(uint32_t mb, uint32_t me) {
    uint64_t a = ~0ull >> mb;
    uint64_t b = me >= 63 ? ~0ull : ~(~0ull >> (me + 1));
    return mb <= me ? (a & b) : (a | b);
}

inline uint64_t be_read(const uint8_t* p, int size) {
    uint64_t v = 0;
    for (int i = 0; i < size; i++) v = v << 8 | p[i];
    return v;
}

inline void be_write(uint8_t* p, uint64_t v, int size) {
    for (int i = size - 1; i >= 0; i--) {
        p[i] = (uint8_t)v;
        v >>= 8;
    }
}

inline float as_float(uint32_t u) {
    float f;
    memcpy(&f, &u, 4);
    return f;
}
inline uint32_t float_bits(float f) {
    uint32_t u;
    memcpy(&u, &f, 4);
    return u;
}
inline double as_double(uint64_t u) {
    double d;
    memcpy(&d, &u, 8);
    return d;
}
inline uint64_t double_bits(double d) {
    uint64_t u;
    memcpy(&u, &d, 8);
    return u;
}

inline uint64_t mulhu64(uint64_t a, uint64_t b) {
#if defined(_MSC_VER)
    uint64_t hi;
    _umul128(a, b, &hi);
    return hi;
#else
    return (uint64_t)(((unsigned __int128)a * b) >> 64);
#endif
}

inline int64_t mulhs64(int64_t a, int64_t b) {
#if defined(_MSC_VER)
    int64_t hi;
    _mul128(a, b, &hi);
    return hi;
#else
    return (int64_t)(((__int128)a * b) >> 64);
#endif
}

inline uint32_t clz32(uint32_t v) {
    uint32_t n = 0;
    if (!v) return 32;
    while (!(v & 0x80000000u)) {
        v <<= 1;
        n++;
    }
    return n;
}

inline uint32_t clz64(uint64_t v) {
    uint32_t hi = (uint32_t)(v >> 32);
    return hi ? clz32(hi) : 32 + clz32((uint32_t)v);
}

struct Vec128 {
    uint32_t w[4];  // w[0] is element 0 (the most significant word)
    uint8_t b(int i) const { return (uint8_t)(w[i >> 2] >> (24 - 8 * (i & 3))); }
    void set_b(int i, uint8_t v) {
        int sh = 24 - 8 * (i & 3);
        w[i >> 2] = (w[i >> 2] & ~(0xFFu << sh)) | ((uint32_t)v << sh);
    }
    uint16_t h(int i) const { return (uint16_t)(w[i >> 1] >> ((i & 1) ? 0 : 16)); }
    void set_h(int i, uint16_t v) {
        int sh = (i & 1) ? 0 : 16;
        w[i >> 1] = (w[i >> 1] & ~(0xFFFFu << sh)) | ((uint32_t)v << sh);
    }
    float f(int i) const { return as_float(w[i]); }
    void set_f(int i, float v) { w[i] = float_bits(v); }
};

inline int32_t sat32(double v) {
    if (v != v) return 0;
    if (v >= 2147483647.0) return INT32_MAX;
    if (v <= -2147483648.0) return INT32_MIN;
    return (int32_t)v;
}

}  // namespace

struct Xenon {
    XenonBus bus;
    uint64_t r[32];
    double f[32];
    Vec128 v[128];
    uint32_t vscr;
    uint32_t pc, npc;
    uint64_t lr, ctr;
    uint32_t cr, xer, fpscr;
    uint64_t tb;
    bool reserve;
    uint32_t reserve_addr;
    int event;
    uint32_t event_op;
    uint32_t cycles;

    // ---------------------------------------------------------------- memory

    uint64_t load(uint32_t ea, int size) {
        uint8_t* p = bus.pages[ea >> 12];
        if (p && (ea & 0xFFF) + size <= 0x1000) return be_read(p + (ea & 0xFFF), size);
        if (p || bus.pages[(ea + size - 1) >> 12]) {
            uint64_t v = 0;
            for (int i = 0; i < size; i++) v = v << 8 | load(ea + i, 1);
            return v;
        }
        return bus.read(bus.ctx, ea, size);
    }

    void store(uint32_t ea, uint64_t val, int size) {
        if (reserve && (ea & ~127u) == (reserve_addr & ~127u)) reserve = false;
        uint8_t* p = bus.pages[ea >> 12];
        if (p && (ea & 0xFFF) + size <= 0x1000) {
            be_write(p + (ea & 0xFFF), val, size);
            return;
        }
        if (p || bus.pages[(ea + size - 1) >> 12]) {
            for (int i = 0; i < size; i++) store(ea + i, val >> (8 * (size - 1 - i)), 1);
            return;
        }
        bus.write(bus.ctx, ea, val, size);
    }

    // ---------------------------------------------------------------- flags

    void set_cr_field(int fld, uint32_t val) {
        int sh = (7 - fld) * 4;
        cr = (cr & ~(0xFu << sh)) | ((val & 0xF) << sh);
    }
    uint32_t cr_field(int fld) const { return (cr >> ((7 - fld) * 4)) & 0xF; }
    bool cr_bit(int b) const { return (cr >> (31 - b)) & 1; }
    void set_cr_bit(int b, bool on) {
        uint32_t m = 1u << (31 - b);
        cr = on ? (cr | m) : (cr & ~m);
    }

    void update_cr0(uint64_t val) {
        int64_t s = (int64_t)val;
        set_cr_field(0, (s < 0 ? 8 : s > 0 ? 4 : 2) | ((xer & XER_SO) ? 1 : 0));
    }

    void set_ca(bool c) { xer = c ? (xer | XER_CA) : (xer & ~XER_CA); }
    bool ca() const { return (xer & XER_CA) != 0; }
    void set_ov(bool o) { xer = o ? (xer | XER_OV | XER_SO) : (xer & ~XER_OV); }

    void stop(int ev, uint32_t op) {
        event = ev;
        event_op = op;
    }

    // ---------------------------------------------------------------- traps

    bool trap_cond(uint32_t to, int64_t a, int64_t b) {
        return ((to & 16) && a < b) || ((to & 8) && a > b) || ((to & 4) && a == b) || ((to & 2) && (uint64_t)a < (uint64_t)b) ||
               ((to & 1) && (uint64_t)a > (uint64_t)b);
    }

    // ---------------------------------------------------------------- branches

    bool bo_ok(uint32_t bo, uint32_t bi) {
        if (!(bo & 4)) ctr--;
        bool ctr_ok = (bo & 4) || ((ctr != 0) ^ ((bo >> 1) & 1));
        bool cond_ok = (bo & 16) || (cr_bit(bi) == ((bo >> 3) & 1));
        return ctr_ok && cond_ok;
    }

    void op19(uint32_t op) {
        uint32_t bo = (op >> 21) & 31, bi = (op >> 16) & 31, bb = (op >> 11) & 31;
        uint32_t xo = (op >> 1) & 1023;
        bool lk = op & 1;
        switch (xo) {
            case 0: set_cr_field(bo >> 2, cr_field(bi >> 2)); return;  // mcrf
            case 16: {                                                 // bclr
                uint32_t target = (uint32_t)lr & ~3u;
                bool take = bo_ok(bo, bi);
                if (lk) lr = pc + 4;
                if (take) npc = target;
                return;
            }
            case 528: {  // bcctr
                bool take = (bo & 16) || (cr_bit(bi) == ((bo >> 3) & 1));
                uint32_t target = (uint32_t)ctr & ~3u;
                if (lk) lr = pc + 4;
                if (take) npc = target;
                return;
            }
            case 150: return;  // isync
            case 33: set_cr_bit(bo, !(cr_bit(bi) || cr_bit(bb))); return;
            case 129: set_cr_bit(bo, cr_bit(bi) && !cr_bit(bb)); return;
            case 193: set_cr_bit(bo, cr_bit(bi) != cr_bit(bb)); return;
            case 225: set_cr_bit(bo, !(cr_bit(bi) && cr_bit(bb))); return;
            case 257: set_cr_bit(bo, cr_bit(bi) && cr_bit(bb)); return;
            case 289: set_cr_bit(bo, cr_bit(bi) == cr_bit(bb)); return;
            case 417: set_cr_bit(bo, cr_bit(bi) || !cr_bit(bb)); return;
            case 449: set_cr_bit(bo, cr_bit(bi) || cr_bit(bb)); return;
            default: stop(XENON_INVALID, op); return;
        }
    }

    // ---------------------------------------------------------------- integer (opcode 31)

    // a + b + c with carry out of bit 63.
    uint64_t add3(uint64_t a, uint64_t b, uint64_t c, bool& carry) {
        uint64_t s = a + b;
        bool c1 = s < a;
        uint64_t t = s + c;
        carry = c1 || t < s;
        return t;
    }

    bool op31_arith(uint32_t op) {
        uint32_t rd = (op >> 21) & 31, ra = (op >> 16) & 31, rb = (op >> 11) & 31;
        bool rc = op & 1, oe = op & (1u << 10);
        uint64_t a = r[ra], b = r[rb], res;
        bool c;
        switch ((op >> 1) & 511) {
            case 8: res = add3(~a, b, 1, c); set_ca(c); if (oe) set_ov(((~a ^ res) & (b ^ res)) >> 63); break;  // subfc
            case 10: res = add3(a, b, 0, c); set_ca(c); if (oe) set_ov(((a ^ res) & (b ^ res)) >> 63); break;   // addc
            case 136: res = add3(~a, b, ca(), c); set_ca(c); if (oe) set_ov(((~a ^ res) & (b ^ res)) >> 63); break;
            case 138: res = add3(a, b, ca(), c); set_ca(c); if (oe) set_ov(((a ^ res) & (b ^ res)) >> 63); break;
            case 200: res = add3(~a, 0, ca(), c); set_ca(c); if (oe) set_ov(((~a ^ res) & res) >> 63); break;
            case 202: res = add3(a, 0, ca(), c); set_ca(c); if (oe) set_ov(((a ^ res) & res) >> 63); break;
            case 232: res = add3(~a, ~0ull, ca(), c); set_ca(c); if (oe) set_ov(((~a ^ res) & ~res) >> 63); break;
            case 234: res = add3(a, ~0ull, ca(), c); set_ca(c); if (oe) set_ov(((a ^ res) & ~res) >> 63); break;
            case 40: res = b - a; if (oe) set_ov(((~a ^ res) & (b ^ res)) >> 63); break;  // subf
            case 266: res = a + b; if (oe) set_ov(((a ^ res) & (b ^ res)) >> 63); break;  // add
            case 104: res = 0 - a; if (oe) set_ov(a == 0x8000000000000000ull); break;      // neg
            case 9: res = mulhu64(a, b); break;                                            // mulhdu
            case 73: res = (uint64_t)mulhs64((int64_t)a, (int64_t)b); break;                // mulhd
            case 11: res = (uint32_t)(((uint64_t)(uint32_t)a * (uint32_t)b) >> 32); break;  // mulhwu
            case 75: res = (uint64_t)(int64_t)(int32_t)(((int64_t)(int32_t)a * (int32_t)b) >> 32); break;
            case 233: {  // mulld
                res = a * b;
                if (oe) set_ov(mulhs64((int64_t)a, (int64_t)b) != ((int64_t)res >> 63));
                cycles += 4;
                break;
            }
            case 235: {  // mullw
                int64_t m = (int64_t)(int32_t)a * (int32_t)b;
                if (oe) set_ov(m < INT32_MIN || m > INT32_MAX);
                res = (uint64_t)m;
                cycles += 2;
                break;
            }
            case 457: {  // divdu
                bool bad = b == 0;
                if (oe) set_ov(bad);
                res = bad ? 0 : a / b;
                cycles += 30;
                break;
            }
            case 489: {  // divd
                bool bad = b == 0 || (a == 0x8000000000000000ull && b == ~0ull);
                if (oe) set_ov(bad);
                res = bad ? 0 : (uint64_t)((int64_t)a / (int64_t)b);
                cycles += 30;
                break;
            }
            case 459: {  // divwu
                uint32_t x = (uint32_t)a, y = (uint32_t)b;
                if (oe) set_ov(y == 0);
                res = y ? x / y : 0;
                cycles += 18;
                break;
            }
            case 491: {  // divw
                int32_t x = (int32_t)a, y = (int32_t)b;
                bool bad = y == 0 || (x == INT32_MIN && y == -1);
                if (oe) set_ov(bad);
                res = bad ? 0 : (uint32_t)(x / y);
                cycles += 18;
                break;
            }
            default: return false;
        }
        r[rd] = res;
        if (rc) update_cr0(res);
        return true;
    }

    // Byte-granular vector loads/stores used by lvlx/lvrx/stvlx/stvrx.
    void lvx(int vd, uint32_t ea) {
        ea &= ~15u;
        for (int i = 0; i < 4; i++) v[vd].w[i] = (uint32_t)load(ea + i * 4, 4);
    }
    void stvx(int vs, uint32_t ea) {
        ea &= ~15u;
        for (int i = 0; i < 4; i++) store(ea + i * 4, v[vs].w[i], 4);
    }

    void op31(uint32_t op) {
        if (((op >> 2) & 511) == 413) {  // sradi (XS-form)
            uint32_t rs = (op >> 21) & 31, ra = (op >> 16) & 31;
            uint32_t sh = ((op >> 11) & 31) | ((op >> 1) & 1) << 5;
            int64_t s = (int64_t)r[rs];
            r[ra] = (uint64_t)(s >> sh);
            set_ca(s < 0 && sh && (s & ((1ull << sh) - 1)) != 0);
            if (op & 1) update_cr0(r[ra]);
            return;
        }
        if (op31_arith(op)) return;
        uint32_t rd = (op >> 21) & 31, ra = (op >> 16) & 31, rb = (op >> 11) & 31;
        uint32_t xo = (op >> 1) & 1023;
        bool rc = op & 1;
        uint64_t a = r[ra], b = r[rb], rs = r[rd];
        uint32_t ea0 = (uint32_t)((ra ? a : 0) + b);  // (rA|0) + rB
        uint32_t eau = (uint32_t)(a + b);              // update forms
        uint64_t res;
        switch (xo) {
            case 0:
            case 32: {  // cmp / cmpl (L selects 64-bit)
                bool l = (op >> 21) & 1;
                uint32_t c;
                if (xo == 0) {
                    int64_t x = l ? (int64_t)a : (int32_t)a, y = l ? (int64_t)b : (int32_t)b;
                    c = x < y ? 8 : x > y ? 4 : 2;
                } else {
                    uint64_t x = l ? a : (uint32_t)a, y = l ? b : (uint32_t)b;
                    c = x < y ? 8 : x > y ? 4 : 2;
                }
                set_cr_field(rd >> 2, c | ((xer & XER_SO) ? 1 : 0));
                return;
            }
            case 4:  // tw
                if (trap_cond(rd, (int32_t)a, (int32_t)b)) stop(XENON_TRAP, op);
                return;
            case 68:  // td
                if (trap_cond(rd, (int64_t)a, (int64_t)b)) stop(XENON_TRAP, op);
                return;
            case 19:  // mfcr / mfocrf
                r[rd] = cr;
                return;
            case 144: {  // mtcrf / mtocrf
                uint32_t crm = (op >> 12) & 0xFF, m = 0;
                for (int i = 0; i < 8; i++)
                    if (crm & (0x80 >> i)) m |= 0xF0000000u >> (i * 4);
                cr = (cr & ~m) | ((uint32_t)rs & m);
                return;
            }
            case 83: r[rd] = 0x8000000000009030ull; return;  // mfmsr: 64-bit, EE, PR, IR/DR
            case 146:
            case 178: return;  // mtmsr / mtmsrd: interrupt masking is handled by the host
            case 339: {        // mfspr
                uint32_t n = ((op >> 16) & 31) | ((op >> 11) & 31) << 5;
                switch (n) {
                    case 1: r[rd] = xer; return;
                    case 8: r[rd] = lr; return;
                    case 9: r[rd] = ctr; return;
                    case 268: r[rd] = tb; return;
                    case 269: r[rd] = tb >> 32; return;
                    case 256: r[rd] = vrsave; return;
                    case 287: r[rd] = 0x00710800; return;  // PVR
                    default: r[rd] = 0; return;
                }
            }
            case 467: {  // mtspr
                uint32_t n = ((op >> 16) & 31) | ((op >> 11) & 31) << 5;
                switch (n) {
                    case 1: xer = (uint32_t)rs; return;
                    case 8: lr = rs; return;
                    case 9: ctr = rs; return;
                    case 256: vrsave = (uint32_t)rs; return;
                    default: return;
                }
            }
            case 371: {  // mftb
                uint32_t n = ((op >> 16) & 31) | ((op >> 11) & 31) << 5;
                r[rd] = n == 269 ? tb >> 32 : tb;
                return;
            }
            // Loads.
            case 21: r[rd] = load(ea0, 8); return;                  // ldx
            case 53: r[rd] = load(eau, 8); r[ra] = eau; return;     // ldux
            case 23: r[rd] = load(ea0, 4); return;                  // lwzx
            case 55: r[rd] = load(eau, 4); r[ra] = eau; return;
            case 341: r[rd] = (uint64_t)(int32_t)load(ea0, 4); return;  // lwax
            case 373: r[rd] = (uint64_t)(int32_t)load(eau, 4); r[ra] = eau; return;
            case 87: r[rd] = load(ea0, 1); return;
            case 119: r[rd] = load(eau, 1); r[ra] = eau; return;
            case 279: r[rd] = load(ea0, 2); return;
            case 311: r[rd] = load(eau, 2); r[ra] = eau; return;
            case 343: r[rd] = (uint64_t)(int16_t)load(ea0, 2); return;
            case 375: r[rd] = (uint64_t)(int16_t)load(eau, 2); r[ra] = eau; return;
            case 20: r[rd] = load(ea0, 4); reserve = true; reserve_addr = ea0; return;  // lwarx
            case 84: r[rd] = load(ea0, 8); reserve = true; reserve_addr = ea0; return;  // ldarx
            case 534: {  // lwbrx
                uint32_t x = (uint32_t)load(ea0, 4);
                r[rd] = (x >> 24) | ((x >> 8) & 0xFF00) | ((x << 8) & 0xFF0000) | (x << 24);
                return;
            }
            case 790: {  // lhbrx
                uint32_t x = (uint32_t)load(ea0, 2);
                r[rd] = ((x >> 8) & 0xFF) | ((x & 0xFF) << 8);
                return;
            }
            case 532: {  // ldbrx
                uint64_t x = load(ea0, 8), y = 0;
                for (int i = 0; i < 8; i++) y = y << 8 | ((x >> (8 * i)) & 0xFF);
                r[rd] = y;
                return;
            }
            // Stores.
            case 149: store(ea0, rs, 8); return;                   // stdx
            case 181: store(eau, rs, 8); r[ra] = eau; return;      // stdux
            case 151: store(ea0, rs, 4); return;
            case 183: store(eau, rs, 4); r[ra] = eau; return;
            case 215: store(ea0, rs, 1); return;
            case 247: store(eau, rs, 1); r[ra] = eau; return;
            case 407: store(ea0, rs, 2); return;
            case 439: store(eau, rs, 2); r[ra] = eau; return;
            case 150:  // stwcx.
            case 214: {  // stdcx.
                bool ok = reserve && (reserve_addr & ~127u) == (ea0 & ~127u);
                reserve = false;
                if (ok) store(ea0, rs, xo == 150 ? 4 : 8);
                set_cr_field(0, (ok ? 2 : 0) | ((xer & XER_SO) ? 1 : 0));
                return;
            }
            case 662: {  // stwbrx
                uint32_t x = (uint32_t)rs;
                store(ea0, (x >> 24) | ((x >> 8) & 0xFF00) | ((x << 8) & 0xFF0000) | (x << 24), 4);
                return;
            }
            case 918: store(ea0, ((rs >> 8) & 0xFF) | ((rs & 0xFF) << 8), 2); return;  // sthbrx
            case 660: {                                                                  // stdbrx
                uint64_t y = 0;
                for (int i = 0; i < 8; i++) y = y << 8 | ((rs >> (8 * i)) & 0xFF);
                store(ea0, y, 8);
                return;
            }
            // Cache and sync: no-ops except dcbz, which zeroes a line.
            case 1014: {
                uint32_t line = rd == 1 ? 128 : 32;  // dcbz128 on Xenon
                uint32_t base = ea0 & ~(line - 1);
                for (uint32_t i = 0; i < line; i += 8) store(base + i, 0, 8);
                return;
            }
            case 54: case 86: case 246: case 278: case 470: case 598: case 854: case 982: return;
            // Floating-point indexed loads/stores.
            case 535: f[rd] = as_float((uint32_t)load(ea0, 4)); return;
            case 567: f[rd] = as_float((uint32_t)load(eau, 4)); r[ra] = eau; return;
            case 599: f[rd] = as_double(load(ea0, 8)); return;
            case 631: f[rd] = as_double(load(eau, 8)); r[ra] = eau; return;
            case 663: store(ea0, float_bits((float)f[rd]), 4); return;
            case 695: store(eau, float_bits((float)f[rd]), 4); r[ra] = eau; return;
            case 727: store(ea0, double_bits(f[rd]), 8); return;
            case 759: store(eau, double_bits(f[rd]), 8); r[ra] = eau; return;
            case 983: store(ea0, (uint32_t)double_bits(f[rd]), 4); return;  // stfiwx
            // Vector loads/stores.
            case 103: case 359: lvx(rd, ea0); return;
            case 231: case 487: stvx(rd, ea0); return;
            case 7: v[rd].set_b(ea0 & 15, (uint8_t)load(ea0, 1)); return;                // lvebx
            case 39: v[rd].set_h((ea0 & 15) >> 1, (uint16_t)load(ea0 & ~1u, 2)); return;  // lvehx
            case 71: v[rd].w[(ea0 & 15) >> 2] = (uint32_t)load(ea0 & ~3u, 4); return;     // lvewx
            case 135: store(ea0, v[rd].b(ea0 & 15), 1); return;
            case 167: store(ea0 & ~1u, v[rd].h((ea0 & 15) >> 1), 2); return;
            case 199: store(ea0 & ~3u, v[rd].w[(ea0 & 15) >> 2], 4); return;
            case 6:
            case 38: {  // lvsl / lvsr
                uint32_t sh = ea0 & 15;
                for (int i = 0; i < 16; i++) v[rd].set_b(i, (uint8_t)(xo == 6 ? sh + i : 16 - sh + i));
                return;
            }
            case 519: case 775: {  // lvlx
                Vec128 t = {};
                uint32_t n = 16 - (ea0 & 15);
                for (uint32_t i = 0; i < n; i++) t.set_b(i, (uint8_t)load(ea0 + i, 1));
                v[rd] = t;
                return;
            }
            case 551: case 807: {  // lvrx
                Vec128 t = {};
                uint32_t n = ea0 & 15, base = ea0 & ~15u;
                for (uint32_t i = 0; i < n; i++) t.set_b(16 - n + i, (uint8_t)load(base + i, 1));
                v[rd] = t;
                return;
            }
            case 647: case 903: {  // stvlx
                uint32_t n = 16 - (ea0 & 15);
                for (uint32_t i = 0; i < n; i++) store(ea0 + i, v[rd].b(i), 1);
                return;
            }
            case 679: case 935: {  // stvrx
                uint32_t n = ea0 & 15, base = ea0 & ~15u;
                for (uint32_t i = 0; i < n; i++) store(base + i, v[rd].b(16 - n + i), 1);
                return;
            }
            default: break;
        }
        // Logic and shifts: rS is the rD field, result goes to rA.
        switch (xo) {
            case 28: res = rs & b; break;
            case 60: res = rs & ~b; break;
            case 124: res = ~(rs | b); break;
            case 284: res = ~(rs ^ b); break;
            case 316: res = rs ^ b; break;
            case 412: res = rs | ~b; break;
            case 444: res = rs | b; break;
            case 476: res = ~(rs & b); break;
            case 26: res = clz32((uint32_t)rs); break;
            case 58: res = clz64(rs); break;
            case 922: res = (uint64_t)(int16_t)rs; break;
            case 954: res = (uint64_t)(int8_t)rs; break;
            case 986: res = (uint64_t)(int32_t)rs; break;
            case 24: { uint32_t n = b & 63; res = n > 31 ? 0 : (uint32_t)((uint32_t)rs << n); break; }  // slw
            case 536: { uint32_t n = b & 63; res = n > 31 ? 0 : (uint32_t)rs >> n; break; }              // srw
            case 27: { uint32_t n = b & 127; res = n > 63 ? 0 : rs << n; break; }                        // sld
            case 539: { uint32_t n = b & 127; res = n > 63 ? 0 : rs >> n; break; }                       // srd
            case 792: {  // sraw
                int32_t s = (int32_t)rs;
                uint32_t n = b & 63;
                if (n > 31) {
                    res = (uint64_t)(int64_t)(s < 0 ? -1 : 0);
                    set_ca(s < 0);
                } else {
                    res = (uint64_t)(int64_t)(s >> n);
                    set_ca(s < 0 && n && ((uint32_t)s & ((1u << n) - 1)) != 0);
                }
                break;
            }
            case 824: {  // srawi
                int32_t s = (int32_t)rs;
                uint32_t n = rb;
                res = (uint64_t)(int64_t)(s >> n);
                set_ca(s < 0 && n && ((uint32_t)s & ((1u << n) - 1)) != 0);
                break;
            }
            case 794: {  // srad
                int64_t s = (int64_t)rs;
                uint32_t n = b & 127;
                if (n > 63) {
                    res = (uint64_t)(s < 0 ? -1 : 0);
                    set_ca(s < 0);
                } else {
                    res = (uint64_t)(s >> n);
                    set_ca(s < 0 && n && ((uint64_t)s & ((1ull << n) - 1)) != 0);
                }
                break;
            }
            default: stop(XENON_INVALID, op); return;
        }
        r[ra] = res;
        if (rc) update_cr0(res);
    }

    // ---------------------------------------------------------------- rotates (opcode 30)

    void op30(uint32_t op) {
        uint32_t rs = (op >> 21) & 31, ra = (op >> 16) & 31;
        uint32_t sh = ((op >> 11) & 31) | ((op >> 1) & 1) << 5;
        uint32_t mb = ((op >> 6) & 31) | ((op >> 5) & 1) << 5;
        uint64_t s = r[rs], res;
        switch ((op >> 2) & 7) {
            case 0: res = rotl64(s, sh) & mask64(mb, 63); break;            // rldicl
            case 1: res = rotl64(s, sh) & mask64(0, mb); break;             // rldicr (mb field is me)
            case 2: res = rotl64(s, sh) & mask64(mb, 63 - sh); break;       // rldic
            case 3: {                                                       // rldimi
                uint64_t m = mask64(mb, 63 - sh);
                res = (rotl64(s, sh) & m) | (r[ra] & ~m);
                break;
            }
            case 4: {  // rldcl / rldcr (MDS-form)
                uint32_t n = (uint32_t)r[(op >> 11) & 31] & 63;
                res = (op >> 1) & 1 ? rotl64(s, n) & mask64(0, mb) : rotl64(s, n) & mask64(mb, 63);
                break;
            }
            default: stop(XENON_INVALID, op); return;
        }
        r[ra] = res;
        if (op & 1) update_cr0(res);
    }

    // ---------------------------------------------------------------- floating point

    void fcompare(double a, double b, int crf) {
        uint32_t c = isnan(a) || isnan(b) ? 1 : a < b ? 8 : a > b ? 4 : 2;
        set_cr_field(crf, c);
        fpscr = (fpscr & ~0xF000u) | (c << 12);
    }

    void op63(uint32_t op, bool single) {
        uint32_t d = (op >> 21) & 31, a = (op >> 16) & 31, b = (op >> 11) & 31, c = (op >> 6) & 31;
        bool rc = op & 1;
        double res;
        auto done = [&](double x) {
            f[d] = single ? (double)(float)x : x;
            if (rc) set_cr_field(1, fpscr >> 28);
        };
        switch ((op >> 1) & 31) {
            case 18: done(f[a] / f[b]); cycles += single ? 16 : 30; return;
            case 20: done(f[a] - f[b]); return;
            case 21: done(f[a] + f[b]); return;
            case 22: done(sqrt(f[b])); return;
            case 23: done(f[a] >= 0.0 ? f[c] : f[b]); return;  // fsel
            case 24: done(1.0 / f[b]); return;                 // fres
            case 25: done(f[a] * f[c]); return;
            case 26: done(1.0 / sqrt(f[b])); return;  // frsqrte
            case 28: done(f[a] * f[c] - f[b]); return;
            case 29: done(f[a] * f[c] + f[b]); return;
            case 30: done(-(f[a] * f[c] - f[b])); return;
            case 31: done(-(f[a] * f[c] + f[b])); return;
            default: break;
        }
        if (single) {
            stop(XENON_INVALID, op);
            return;
        }
        switch ((op >> 1) & 1023) {
            case 0: fcompare(f[a], f[b], d >> 2); return;   // fcmpu
            case 32: fcompare(f[a], f[b], d >> 2); return;  // fcmpo
            case 12: res = (double)(float)f[b]; break;       // frsp
            case 14:
            case 15: {  // fctiw / fctiwz
                double x = ((op >> 1) & 1023) == 15 ? trunc(f[b]) : nearbyint(f[b]);
                f[d] = as_double((uint64_t)(uint32_t)sat32(x));
                if (rc) set_cr_field(1, fpscr >> 28);
                return;
            }
            case 814:
            case 815: {  // fctid / fctidz
                double x = ((op >> 1) & 1023) == 815 ? trunc(f[b]) : nearbyint(f[b]);
                int64_t i = x != x ? INT64_MIN : x >= 9223372036854775807.0 ? INT64_MAX : x <= -9223372036854775808.0 ? INT64_MIN : (int64_t)x;
                f[d] = as_double((uint64_t)i);
                if (rc) set_cr_field(1, fpscr >> 28);
                return;
            }
            case 846: res = (double)(int64_t)double_bits(f[b]); break;  // fcfid
            case 40: res = -f[b]; break;
            case 72: res = f[b]; break;
            case 136: res = -fabs(f[b]); break;
            case 264: res = fabs(f[b]); break;
            case 583: f[d] = as_double(fpscr); return;  // mffs
            case 711: {                                  // mtfsf
                uint32_t fm = (op >> 17) & 0xFF, m = 0;
                for (int i = 0; i < 8; i++)
                    if (fm & (0x80 >> i)) m |= 0xF0000000u >> (i * 4);
                fpscr = (fpscr & ~m) | ((uint32_t)double_bits(f[b]) & m);
                return;
            }
            case 134: {  // mtfsfi
                int sh = (7 - (int)(d >> 2)) * 4;
                fpscr = (fpscr & ~(0xFu << sh)) | (((op >> 12) & 0xF) << sh);
                return;
            }
            case 38: fpscr |= 1u << (31 - d); return;     // mtfsb1
            case 70: fpscr &= ~(1u << (31 - d)); return;  // mtfsb0
            case 64: set_cr_field(d >> 2, (fpscr >> ((7 - (a >> 2)) * 4)) & 0xF); return;  // mcrfs
            default: stop(XENON_INVALID, op); return;
        }
        f[d] = res;
        if (rc) set_cr_field(1, fpscr >> 28);
    }

    // ---------------------------------------------------------------- vector (opcode 4, standard VMX)

    void vcmp_cr6(bool rc, const Vec128& t) {
        if (!rc) return;
        bool all = true, none = true;
        for (int i = 0; i < 4; i++) {
            if (t.w[i] != 0xFFFFFFFFu) all = false;
            if (t.w[i] != 0) none = false;
        }
        set_cr_field(6, (all ? 8 : 0) | (none ? 2 : 0));
    }

    // ---------------------------------------------------------------- packing (shared by VMX and VMX128)

    static int32_t clampi(int32_t x, int32_t lo, int32_t hi) { return x < lo ? lo : x > hi ? hi : x; }

    // Packs the halfwords/words of a then b into bytes/halfwords. kind: 0 modulo, 1 signed
    // saturate, 2 signed-to-unsigned saturate, 3 unsigned saturate.
    Vec128 pack(const Vec128& a, const Vec128& b, bool words, int kind) {
        Vec128 t = {};
        if (words) {
            for (int i = 0; i < 8; i++) {
                uint32_t x = i < 4 ? a.w[i] : b.w[i - 4];
                int32_t s = (int32_t)x;
                uint16_t o = kind == 0 ? (uint16_t)x
                           : kind == 1 ? (uint16_t)(int16_t)clampi(s, -32768, 32767)
                           : kind == 2 ? (uint16_t)clampi(s, 0, 65535)
                                       : (uint16_t)(x > 65535u ? 65535u : x);
                if (kind == 1 && (s < -32768 || s > 32767)) vscr |= 1;
                t.set_h(i, o);
            }
        } else {
            for (int i = 0; i < 16; i++) {
                uint16_t x = i < 8 ? a.h(i) : b.h(i - 8);
                int32_t s = (int16_t)x;
                uint8_t o = kind == 0 ? (uint8_t)x
                          : kind == 1 ? (uint8_t)(int8_t)clampi(s, -128, 127)
                          : kind == 2 ? (uint8_t)clampi(s, 0, 255)
                                      : (uint8_t)(x > 255 ? 255 : x);
                t.set_b(i, o);
            }
        }
        return t;
    }

    // Sign-extends the high (hi) or low eight bytes / four halfwords of b.
    static Vec128 unpack(const Vec128& b, bool halves, bool hi) {
        Vec128 t = {};
        if (halves) {
            for (int i = 0; i < 4; i++) t.w[i] = (uint32_t)(int32_t)(int16_t)b.h(hi ? i : 4 + i);
        } else {
            for (int i = 0; i < 8; i++) t.set_h(i, (uint16_t)(int16_t)(int8_t)b.b(hi ? i : 8 + i));
        }
        return t;
    }

    // ---------------------------------------------------------------- VMX128 (Xenon's 128-register extension)

    static int vd128(uint32_t op) { return ((op >> 21) & 31) | ((op >> 2) & 3) << 5; }
    static int va128(uint32_t op) { return ((op >> 16) & 31) | ((op >> 5) & 1) << 5 | ((op >> 10) & 1) << 6; }
    static int vb128(uint32_t op) { return ((op >> 11) & 31) | (op & 3) << 5; }

    // Loads/stores in opcode 4 (VX128_1): vD128 with GPR addressing. Returns false if not one.
    bool vmx128_mem(uint32_t op) {
        uint32_t ra = (op >> 16) & 31, rb = (op >> 11) & 31;
        uint32_t ea = (uint32_t)((ra ? r[ra] : 0) + r[rb]);
        int vd = vd128(op);
        Vec128 t = {};
        switch (op & 0x7F3) {
            case 3:
            case 67: {  // lvsl128 / lvsr128
                uint32_t sh = ea & 15;
                for (int i = 0; i < 16; i++) t.set_b(i, (uint8_t)((op & 0x7F3) == 3 ? sh + i : 16 - sh + i));
                v[vd] = t;
                return true;
            }
            case 131: v[vd].w[(ea & 15) >> 2] = (uint32_t)load(ea & ~3u, 4); return true;  // lvewx128
            case 195: case 707: lvx(vd, ea); return true;                                // lvx128 / lvxl128
            case 387: store(ea & ~3u, v[vd].w[(ea & 15) >> 2], 4); return true;          // stvewx128
            case 451: case 963: stvx(vd, ea); return true;                               // stvx128 / stvxl128
            case 1027: case 1539: {                                                      // lvlx128
                uint32_t n = 16 - (ea & 15);
                for (uint32_t i = 0; i < n; i++) t.set_b(i, (uint8_t)load(ea + i, 1));
                v[vd] = t;
                return true;
            }
            case 1091: case 1603: {  // lvrx128
                uint32_t n = ea & 15, base = ea & ~15u;
                for (uint32_t i = 0; i < n; i++) t.set_b(16 - n + i, (uint8_t)load(base + i, 1));
                v[vd] = t;
                return true;
            }
            case 1283: case 1795: {  // stvlx128
                uint32_t n = 16 - (ea & 15);
                for (uint32_t i = 0; i < n; i++) store(ea + i, v[vd].b(i), 1);
                return true;
            }
            case 1347: case 1859: {  // stvrx128
                uint32_t n = ea & 15, base = ea & ~15u;
                for (uint32_t i = 0; i < n; i++) store(base + i, v[vd].b(16 - n + i), 1);
                return true;
            }
            default: return false;
        }
    }

    void vsldoi128(uint32_t op) {
        const Vec128 a = v[va128(op)], b = v[vb128(op)];
        int sh = (op >> 6) & 15;
        Vec128 t;
        for (int i = 0; i < 16; i++) {
            int j = i + sh;
            t.set_b(i, j < 16 ? a.b(j) : b.b(j - 16));
        }
        v[vd128(op)] = t;
    }

    void op5(uint32_t op) {
        int d = vd128(op);
        const Vec128 a = v[va128(op)], b = v[vb128(op)], dv = v[d];
        Vec128 t = {};
        if ((op & 0x210) == 0) {  // vperm128: control register v0-v7
            const Vec128 c = v[(op >> 6) & 7];
            for (int i = 0; i < 16; i++) {
                int idx = c.b(i) & 31;
                t.set_b(i, idx < 16 ? a.b(idx) : b.b(idx - 16));
            }
            v[d] = t;
            return;
        }
        switch (op & 0x3D0) {
            case 16: for (int i = 0; i < 4; i++) t.set_f(i, a.f(i) + b.f(i)); break;                 // vaddfp128
            case 80: for (int i = 0; i < 4; i++) t.set_f(i, a.f(i) - b.f(i)); break;                 // vsubfp128
            case 144: for (int i = 0; i < 4; i++) t.set_f(i, a.f(i) * b.f(i)); break;                // vmulfp128
            case 208: for (int i = 0; i < 4; i++) t.set_f(i, a.f(i) * b.f(i) + dv.f(i)); break;      // vmaddfp128
            case 272: for (int i = 0; i < 4; i++) t.set_f(i, a.f(i) * dv.f(i) + b.f(i)); break;      // vmaddcfp128
            case 336: for (int i = 0; i < 4; i++) t.set_f(i, -(a.f(i) * b.f(i) - dv.f(i))); break;   // vnmsubfp128
            case 400: {                                                                               // vmsum3fp128
                float s = a.f(0) * b.f(0) + a.f(1) * b.f(1) + a.f(2) * b.f(2);
                for (int i = 0; i < 4; i++) t.set_f(i, s);
                break;
            }
            case 464: {  // vmsum4fp128
                float s = a.f(0) * b.f(0) + a.f(1) * b.f(1) + a.f(2) * b.f(2) + a.f(3) * b.f(3);
                for (int i = 0; i < 4; i++) t.set_f(i, s);
                break;
            }
            case 512: t = pack(a, b, false, 1); break;  // vpkshss128
            case 576: t = pack(a, b, false, 2); break;  // vpkshus128
            case 640: t = pack(a, b, true, 1); break;   // vpkswss128
            case 704: t = pack(a, b, true, 2); break;   // vpkswus128
            case 768: t = pack(a, b, false, 0); break;  // vpkuhum128
            case 832: t = pack(a, b, false, 3); break;  // vpkuhus128
            case 896: t = pack(a, b, true, 0); break;   // vpkuwum128
            case 960: t = pack(a, b, true, 3); break;   // vpkuwus128
            case 528: for (int i = 0; i < 4; i++) t.w[i] = a.w[i] & b.w[i]; break;
            case 592: for (int i = 0; i < 4; i++) t.w[i] = a.w[i] & ~b.w[i]; break;
            case 656: for (int i = 0; i < 4; i++) t.w[i] = ~(a.w[i] | b.w[i]); break;
            case 720: for (int i = 0; i < 4; i++) t.w[i] = a.w[i] | b.w[i]; break;
            case 784: for (int i = 0; i < 4; i++) t.w[i] = a.w[i] ^ b.w[i]; break;
            case 848: for (int i = 0; i < 4; i++) t.w[i] = (a.w[i] & ~dv.w[i]) | (b.w[i] & dv.w[i]); break;  // vsel128
            case 912: {  // vslo128
                int sh = (b.b(15) >> 3) & 15;
                for (int i = 0; i < 16; i++) t.set_b(i, i + sh < 16 ? a.b(i + sh) : 0);
                break;
            }
            case 976: {  // vsro128
                int sh = (b.b(15) >> 3) & 15;
                for (int i = 0; i < 16; i++) t.set_b(i, i - sh >= 0 ? a.b(i - sh) : 0);
                break;
            }
            default: stop(XENON_INVALID, op); return;
        }
        v[d] = t;
    }

    void op6(uint32_t op) {
        int d = vd128(op);
        const Vec128 a = v[va128(op)], b = v[vb128(op)], dv = v[d];
        uint32_t imm = (op >> 16) & 31;
        Vec128 t = {};
        switch (op & 0x7F0) {  // VX128_3: vD, vB, 5-bit immediate
            case 560: for (int i = 0; i < 4; i++) t.w[i] = (uint32_t)sat32(ldexp(b.f(i), imm)); v[d] = t; return;  // vcfpsxws128
            case 624:  // vcfpuxws128
                for (int i = 0; i < 4; i++) {
                    double x = ldexp(b.f(i), imm);
                    t.w[i] = x != x || x <= 0 ? 0 : x >= 4294967295.0 ? 0xFFFFFFFFu : (uint32_t)x;
                }
                v[d] = t;
                return;
            case 688: for (int i = 0; i < 4; i++) t.set_f(i, ldexpf((float)(int32_t)b.w[i], -(int)imm)); v[d] = t; return;  // vcsxwfp128
            case 752: for (int i = 0; i < 4; i++) t.set_f(i, ldexpf((float)b.w[i], -(int)imm)); v[d] = t; return;           // vcuxwfp128
            case 816: for (int i = 0; i < 4; i++) t.set_f(i, floorf(b.f(i))); v[d] = t; return;
            case 880: for (int i = 0; i < 4; i++) t.set_f(i, nearbyintf(b.f(i))); v[d] = t; return;
            case 944: for (int i = 0; i < 4; i++) t.set_f(i, ceilf(b.f(i))); v[d] = t; return;
            case 1008: for (int i = 0; i < 4; i++) t.set_f(i, truncf(b.f(i))); v[d] = t; return;
            case 1584: for (int i = 0; i < 4; i++) t.set_f(i, 1.0f / b.f(i)); v[d] = t; return;
            case 1648: for (int i = 0; i < 4; i++) t.set_f(i, 1.0f / sqrtf(b.f(i))); v[d] = t; return;
            case 1712: for (int i = 0; i < 4; i++) t.set_f(i, exp2f(b.f(i))); v[d] = t; return;
            case 1776: for (int i = 0; i < 4; i++) t.set_f(i, log2f(b.f(i))); v[d] = t; return;
            case 1840: for (int i = 0; i < 4; i++) t.w[i] = b.w[imm & 3]; v[d] = t; return;  // vspltw128
            case 1904: {                                                                     // vspltisw128
                int32_t s = (int32_t)(imm << 27) >> 27;
                for (int i = 0; i < 4; i++) t.w[i] = (uint32_t)s;
                v[d] = t;
                return;
            }
            default: break;
        }
        if ((op & 0x730) == 0x710) {  // vrlimi128: rotate vB left by z words, insert where IMM bits are set
            uint32_t z = (op >> 6) & 3;
            t = dv;
            for (int i = 0; i < 4; i++)
                if (imm & (8 >> i)) t.w[i] = b.w[(i + z) & 3];
            v[d] = t;
            return;
        }
        if ((op & 0x630) == 0x210) {  // vpermwi128
            uint32_t perm = imm | ((op >> 6) & 7) << 5;
            for (int i = 0; i < 4; i++) t.w[i] = b.w[(perm >> ((3 - i) * 2)) & 3];
            v[d] = t;
            return;
        }
        switch (op & 0x3D0) {  // VX128
            case 80: for (int i = 0; i < 4; i++) { uint32_t n = b.w[i] & 31; t.w[i] = n ? (a.w[i] << n) | (a.w[i] >> (32 - n)) : a.w[i]; } v[d] = t; return;
            case 208: for (int i = 0; i < 4; i++) t.w[i] = a.w[i] << (b.w[i] & 31); v[d] = t; return;
            case 336: for (int i = 0; i < 4; i++) t.w[i] = (uint32_t)((int32_t)a.w[i] >> (b.w[i] & 31)); v[d] = t; return;
            case 464: for (int i = 0; i < 4; i++) t.w[i] = a.w[i] >> (b.w[i] & 31); v[d] = t; return;
            case 640: for (int i = 0; i < 4; i++) t.set_f(i, a.f(i) > b.f(i) ? a.f(i) : b.f(i)); v[d] = t; return;
            case 704: for (int i = 0; i < 4; i++) t.set_f(i, a.f(i) < b.f(i) ? a.f(i) : b.f(i)); v[d] = t; return;
            case 768: t.w[0] = a.w[0]; t.w[1] = b.w[0]; t.w[2] = a.w[1]; t.w[3] = b.w[1]; v[d] = t; return;
            case 832: t.w[0] = a.w[2]; t.w[1] = b.w[2]; t.w[2] = a.w[3]; t.w[3] = b.w[3]; v[d] = t; return;
            case 896: v[d] = unpack(b, false, true); return;   // vupkhsb128
            case 960: v[d] = unpack(b, false, false); return;  // vupklsb128
            default: break;
        }
        bool rc = (op >> 6) & 1;
        switch (op & 0x390) {  // VX128_R compares
            case 0: for (int i = 0; i < 4; i++) t.w[i] = a.f(i) == b.f(i) ? ~0u : 0; break;
            case 128: for (int i = 0; i < 4; i++) t.w[i] = a.f(i) >= b.f(i) ? ~0u : 0; break;
            case 256: for (int i = 0; i < 4; i++) t.w[i] = a.f(i) > b.f(i) ? ~0u : 0; break;
            case 384:  // vcmpbfp128
                for (int i = 0; i < 4; i++) t.w[i] = (a.f(i) <= b.f(i) ? 0 : 0x80000000u) | (a.f(i) >= -b.f(i) ? 0 : 0x40000000u);
                v[d] = t;
                if (rc) set_cr_field(6, (t.w[0] | t.w[1] | t.w[2] | t.w[3]) ? 0 : 2);
                return;
            case 512: for (int i = 0; i < 4; i++) t.w[i] = a.w[i] == b.w[i] ? ~0u : 0; break;
            default: stop(XENON_INVALID, op); return;  // vpkd3d128 / vupkd3d128 not yet
        }
        v[d] = t;
        vcmp_cr6(rc, t);
    }

    void op4(uint32_t op) {
        if ((op & 3) == 3 && !(op & 0x20) && vmx128_mem(op)) return;
        if (op & 0x10) {
            vsldoi128(op);
            return;
        }
        uint32_t d = (op >> 21) & 31, a = (op >> 16) & 31, b = (op >> 11) & 31, c = (op >> 6) & 31;
        const Vec128 va = v[a], vb = v[b], vc = v[c];
        Vec128 t = {};
        switch (op & 63) {  // VA-form
            case 46: for (int i = 0; i < 4; i++) t.set_f(i, va.f(i) * vc.f(i) + vb.f(i)); v[d] = t; return;       // vmaddfp
            case 47: for (int i = 0; i < 4; i++) t.set_f(i, -(va.f(i) * vc.f(i) - vb.f(i))); v[d] = t; return;    // vnmsubfp
            case 42: for (int i = 0; i < 4; i++) t.w[i] = (va.w[i] & ~vc.w[i]) | (vb.w[i] & vc.w[i]); v[d] = t; return;  // vsel
            case 43:  // vperm
                for (int i = 0; i < 16; i++) {
                    int idx = vc.b(i) & 31;
                    t.set_b(i, idx < 16 ? va.b(idx) : vb.b(idx - 16));
                }
                v[d] = t;
                return;
            case 44: {  // vsldoi
                int sh = (op >> 6) & 15;
                for (int i = 0; i < 16; i++) {
                    int j = i + sh;
                    t.set_b(i, j < 16 ? va.b(j) : vb.b(j - 16));
                }
                v[d] = t;
                return;
            }
            default: break;
        }
        uint32_t xo10 = op & 1023;
        bool rc = (op >> 10) & 1;
        switch (xo10) {  // VC-form compares
            case 198: for (int i = 0; i < 4; i++) t.w[i] = va.f(i) == vb.f(i) ? ~0u : 0; v[d] = t; vcmp_cr6(rc, t); return;
            case 454: for (int i = 0; i < 4; i++) t.w[i] = va.f(i) >= vb.f(i) ? ~0u : 0; v[d] = t; vcmp_cr6(rc, t); return;
            case 710: for (int i = 0; i < 4; i++) t.w[i] = va.f(i) > vb.f(i) ? ~0u : 0; v[d] = t; vcmp_cr6(rc, t); return;
            case 966:  // vcmpbfp
                for (int i = 0; i < 4; i++) t.w[i] = (va.f(i) <= vb.f(i) ? 0 : 0x80000000u) | (va.f(i) >= -vb.f(i) ? 0 : 0x40000000u);
                v[d] = t;
                if (rc) set_cr_field(6, (t.w[0] | t.w[1] | t.w[2] | t.w[3]) ? 0 : 2);
                return;
            case 134: for (int i = 0; i < 4; i++) t.w[i] = va.w[i] == vb.w[i] ? ~0u : 0; v[d] = t; vcmp_cr6(rc, t); return;
            case 902: for (int i = 0; i < 4; i++) t.w[i] = (int32_t)va.w[i] > (int32_t)vb.w[i] ? ~0u : 0; v[d] = t; vcmp_cr6(rc, t); return;
            case 646: for (int i = 0; i < 4; i++) t.w[i] = va.w[i] > vb.w[i] ? ~0u : 0; v[d] = t; vcmp_cr6(rc, t); return;
            case 6: for (int i = 0; i < 16; i++) t.set_b(i, va.b(i) == vb.b(i) ? 0xFF : 0); v[d] = t; vcmp_cr6(rc, t); return;
            case 70: for (int i = 0; i < 8; i++) t.set_h(i, va.h(i) == vb.h(i) ? 0xFFFF : 0); v[d] = t; vcmp_cr6(rc, t); return;
            default: break;
        }
        switch (op & 2047) {  // VX-form
            case 10: for (int i = 0; i < 4; i++) t.set_f(i, va.f(i) + vb.f(i)); break;
            case 74: for (int i = 0; i < 4; i++) t.set_f(i, va.f(i) - vb.f(i)); break;
            case 1034: for (int i = 0; i < 4; i++) t.set_f(i, va.f(i) > vb.f(i) ? va.f(i) : vb.f(i)); break;
            case 1098: for (int i = 0; i < 4; i++) t.set_f(i, va.f(i) < vb.f(i) ? va.f(i) : vb.f(i)); break;
            case 266: for (int i = 0; i < 4; i++) t.set_f(i, 1.0f / vb.f(i)); break;
            case 330: for (int i = 0; i < 4; i++) t.set_f(i, 1.0f / sqrtf(vb.f(i))); break;
            case 394: for (int i = 0; i < 4; i++) t.set_f(i, exp2f(vb.f(i))); break;
            case 458: for (int i = 0; i < 4; i++) t.set_f(i, log2f(vb.f(i))); break;
            case 522: for (int i = 0; i < 4; i++) t.set_f(i, nearbyintf(vb.f(i))); break;
            case 586: for (int i = 0; i < 4; i++) t.set_f(i, truncf(vb.f(i))); break;
            case 650: for (int i = 0; i < 4; i++) t.set_f(i, ceilf(vb.f(i))); break;
            case 714: for (int i = 0; i < 4; i++) t.set_f(i, floorf(vb.f(i))); break;
            case 842: for (int i = 0; i < 4; i++) t.set_f(i, ldexpf((float)(int32_t)vb.w[i], -(int)a)); break;  // vcfsx
            case 778: for (int i = 0; i < 4; i++) t.set_f(i, ldexpf((float)vb.w[i], -(int)a)); break;           // vcfux
            case 970: for (int i = 0; i < 4; i++) t.w[i] = (uint32_t)sat32(ldexp(vb.f(i), a)); break;           // vctsxs
            case 906:  // vctuxs
                for (int i = 0; i < 4; i++) {
                    double x = ldexp(vb.f(i), a);
                    t.w[i] = x != x || x <= 0 ? 0 : x >= 4294967295.0 ? 0xFFFFFFFFu : (uint32_t)x;
                }
                break;
            case 1028: for (int i = 0; i < 4; i++) t.w[i] = va.w[i] & vb.w[i]; break;
            case 1092: for (int i = 0; i < 4; i++) t.w[i] = va.w[i] & ~vb.w[i]; break;
            case 1156: for (int i = 0; i < 4; i++) t.w[i] = va.w[i] | vb.w[i]; break;
            case 1284: for (int i = 0; i < 4; i++) t.w[i] = ~(va.w[i] | vb.w[i]); break;
            case 1220: for (int i = 0; i < 4; i++) t.w[i] = va.w[i] ^ vb.w[i]; break;
            case 0: for (int i = 0; i < 16; i++) t.set_b(i, va.b(i) + vb.b(i)); break;
            case 64: for (int i = 0; i < 8; i++) t.set_h(i, va.h(i) + vb.h(i)); break;
            case 128: for (int i = 0; i < 4; i++) t.w[i] = va.w[i] + vb.w[i]; break;
            case 1024: for (int i = 0; i < 16; i++) t.set_b(i, va.b(i) - vb.b(i)); break;
            case 1088: for (int i = 0; i < 8; i++) t.set_h(i, va.h(i) - vb.h(i)); break;
            case 1152: for (int i = 0; i < 4; i++) t.w[i] = va.w[i] - vb.w[i]; break;
            case 386: for (int i = 0; i < 4; i++) t.w[i] = (int32_t)va.w[i] > (int32_t)vb.w[i] ? va.w[i] : vb.w[i]; break;
            case 898: for (int i = 0; i < 4; i++) t.w[i] = (int32_t)va.w[i] < (int32_t)vb.w[i] ? va.w[i] : vb.w[i]; break;
            case 130: for (int i = 0; i < 4; i++) t.w[i] = va.w[i] > vb.w[i] ? va.w[i] : vb.w[i]; break;
            case 642: for (int i = 0; i < 4; i++) t.w[i] = va.w[i] < vb.w[i] ? va.w[i] : vb.w[i]; break;
            case 388: for (int i = 0; i < 4; i++) t.w[i] = va.w[i] << (vb.w[i] & 31); break;
            case 644: for (int i = 0; i < 4; i++) t.w[i] = va.w[i] >> (vb.w[i] & 31); break;
            case 900: for (int i = 0; i < 4; i++) t.w[i] = (uint32_t)((int32_t)va.w[i] >> (vb.w[i] & 31)); break;
            case 324: for (int i = 0; i < 8; i++) t.set_h(i, (uint16_t)(va.h(i) << (vb.h(i) & 15))); break;
            case 580: for (int i = 0; i < 8; i++) t.set_h(i, (uint16_t)(va.h(i) >> (vb.h(i) & 15))); break;
            case 836: for (int i = 0; i < 8; i++) t.set_h(i, (uint16_t)((int16_t)va.h(i) >> (vb.h(i) & 15))); break;
            case 260: for (int i = 0; i < 16; i++) t.set_b(i, (uint8_t)(va.b(i) << (vb.b(i) & 7))); break;
            case 516: for (int i = 0; i < 16; i++) t.set_b(i, (uint8_t)(va.b(i) >> (vb.b(i) & 7))); break;
            case 132: for (int i = 0; i < 4; i++) { uint32_t n = vb.w[i] & 31; t.w[i] = n ? (va.w[i] << n) | (va.w[i] >> (32 - n)) : va.w[i]; } break;  // vrlw
            case 524: for (int i = 0; i < 16; i++) t.set_b(i, vb.b(a & 15)); break;  // vspltb
            case 588: for (int i = 0; i < 8; i++) t.set_h(i, vb.h(a & 7)); break;    // vsplth
            case 652: for (int i = 0; i < 4; i++) t.w[i] = vb.w[a & 3]; break;      // vspltw
            case 780: { int8_t s = (int8_t)((int32_t)(a << 27) >> 27); for (int i = 0; i < 16; i++) t.set_b(i, (uint8_t)s); break; }
            case 844: { int16_t s = (int16_t)((int32_t)(a << 27) >> 27); for (int i = 0; i < 8; i++) t.set_h(i, (uint16_t)s); break; }
            case 908: { int32_t s = (int32_t)(a << 27) >> 27; for (int i = 0; i < 4; i++) t.w[i] = (uint32_t)s; break; }
            case 12: for (int i = 0; i < 8; i++) { t.set_b(2 * i, va.b(i)); t.set_b(2 * i + 1, vb.b(i)); } break;       // vmrghb
            case 76: for (int i = 0; i < 4; i++) { t.set_h(2 * i, va.h(i)); t.set_h(2 * i + 1, vb.h(i)); } break;       // vmrghh
            case 140: t.w[0] = va.w[0]; t.w[1] = vb.w[0]; t.w[2] = va.w[1]; t.w[3] = vb.w[1]; break;                     // vmrghw
            case 268: for (int i = 0; i < 8; i++) { t.set_b(2 * i, va.b(8 + i)); t.set_b(2 * i + 1, vb.b(8 + i)); } break;
            case 332: for (int i = 0; i < 4; i++) { t.set_h(2 * i, va.h(4 + i)); t.set_h(2 * i + 1, vb.h(4 + i)); } break;
            case 396: t.w[0] = va.w[2]; t.w[1] = vb.w[2]; t.w[2] = va.w[3]; t.w[3] = vb.w[3]; break;                     // vmrglw
            case 1036: {  // vslo
                int sh = (vb.b(15) >> 3) & 15;
                for (int i = 0; i < 16; i++) t.set_b(i, i + sh < 16 ? va.b(i + sh) : 0);
                break;
            }
            case 1100: {  // vsro
                int sh = (vb.b(15) >> 3) & 15;
                for (int i = 0; i < 16; i++) t.set_b(i, i - sh >= 0 ? va.b(i - sh) : 0);
                break;
            }
            case 14: t = pack(va, vb, false, 0); break;   // vpkuhum
            case 78: t = pack(va, vb, true, 0); break;    // vpkuwum
            case 142: t = pack(va, vb, false, 3); break;  // vpkuhus
            case 206: t = pack(va, vb, true, 3); break;   // vpkuwus
            case 270: t = pack(va, vb, false, 2); break;  // vpkshus
            case 334: t = pack(va, vb, true, 2); break;   // vpkswus
            case 398: t = pack(va, vb, false, 1); break;  // vpkshss
            case 462: t = pack(va, vb, true, 1); break;   // vpkswss
            case 526: t = unpack(vb, false, true); break;   // vupkhsb
            case 590: t = unpack(vb, true, true); break;    // vupkhsh
            case 654: t = unpack(vb, false, false); break;  // vupklsb
            case 718: t = unpack(vb, true, false); break;   // vupklsh
            case 1540: t.w[3] = vscr; break;           // mfvscr
            case 1604: vscr = vb.w[3]; return;          // mtvscr
            default: stop(XENON_INVALID, op); return;
        }
        v[d] = t;
    }

    // ---------------------------------------------------------------- main decoder

    void execute(uint32_t op) {
        uint32_t rd = (op >> 21) & 31, ra = (op >> 16) & 31;
        int64_t simm = (int16_t)(op & 0xFFFF);
        uint64_t uimm = op & 0xFFFF;
        uint64_t a0 = ra ? r[ra] : 0;
        uint32_t ea = (uint32_t)(a0 + simm), eau = (uint32_t)(r[ra] + simm);
        switch (op >> 26) {
            case 2:  // tdi
                if (trap_cond(rd, (int64_t)r[ra], simm)) stop(XENON_TRAP, op);
                return;
            case 3:  // twi
                if (trap_cond(rd, (int32_t)r[ra], (int32_t)simm)) stop(XENON_TRAP, op);
                return;
            case 4: op4(op); return;
            case 5: op5(op); return;
            case 6: op6(op); return;
            case 7: r[rd] = (uint64_t)((int64_t)r[ra] * simm); cycles += 2; return;  // mulli
            case 8: {                                                               // subfic
                bool c;
                r[rd] = add3(~r[ra], (uint64_t)simm, 1, c);
                set_ca(c);
                return;
            }
            case 10: {  // cmpli
                bool l = (op >> 21) & 1;
                uint64_t x = l ? r[ra] : (uint32_t)r[ra];
                set_cr_field(rd >> 2, (x < uimm ? 8 : x > uimm ? 4 : 2) | ((xer & XER_SO) ? 1 : 0));
                return;
            }
            case 11: {  // cmpi
                bool l = (op >> 21) & 1;
                int64_t x = l ? (int64_t)r[ra] : (int32_t)r[ra];
                set_cr_field(rd >> 2, (x < simm ? 8 : x > simm ? 4 : 2) | ((xer & XER_SO) ? 1 : 0));
                return;
            }
            case 12:
            case 13: {  // addic / addic.
                bool c;
                r[rd] = add3(r[ra], (uint64_t)simm, 0, c);
                set_ca(c);
                if (op >> 26 == 13) update_cr0(r[rd]);
                return;
            }
            case 14: r[rd] = a0 + simm; return;                     // addi
            case 15: r[rd] = a0 + (uint64_t)(simm << 16); return;   // addis
            case 16: {                                              // bc
                int64_t bd = (int16_t)(op & 0xFFFC);
                uint32_t target = (op & 2) ? (uint32_t)bd : pc + (uint32_t)bd;
                if (bo_ok(rd, ra)) npc = target;
                if (op & 1) lr = pc + 4;
                return;
            }
            case 17:  // sc
                npc = pc + 4;
                stop(XENON_SYSCALL, op);
                return;
            case 18: {  // b
                int32_t li = (int32_t)((op & 0x03FFFFFC) << 6) >> 6;
                npc = (op & 2) ? (uint32_t)li : pc + li;
                if (op & 1) lr = pc + 4;
                return;
            }
            case 19: op19(op); return;
            case 20: {  // rlwimi
                uint32_t sh = (op >> 11) & 31, mb = (op >> 6) & 31, me = (op >> 1) & 31;
                uint64_t m = mask64(mb + 32, me + 32);
                r[ra] = (rotl32(r[rd], sh) & m) | (r[ra] & ~m);
                if (op & 1) update_cr0(r[ra]);
                return;
            }
            case 21: {  // rlwinm
                uint32_t sh = (op >> 11) & 31, mb = (op >> 6) & 31, me = (op >> 1) & 31;
                r[ra] = rotl32(r[rd], sh) & mask64(mb + 32, me + 32);
                if (op & 1) update_cr0(r[ra]);
                return;
            }
            case 23: {  // rlwnm
                uint32_t sh = (uint32_t)r[(op >> 11) & 31] & 31, mb = (op >> 6) & 31, me = (op >> 1) & 31;
                r[ra] = rotl32(r[rd], sh) & mask64(mb + 32, me + 32);
                if (op & 1) update_cr0(r[ra]);
                return;
            }
            case 24: r[ra] = r[rd] | uimm; return;
            case 25: r[ra] = r[rd] | (uimm << 16); return;
            case 26: r[ra] = r[rd] ^ uimm; return;
            case 27: r[ra] = r[rd] ^ (uimm << 16); return;
            case 28: r[ra] = r[rd] & uimm; update_cr0(r[ra]); return;
            case 29: r[ra] = r[rd] & (uimm << 16); update_cr0(r[ra]); return;
            case 30: op30(op); return;
            case 31: op31(op); return;
            case 32: r[rd] = load(ea, 4); return;
            case 33: r[rd] = load(eau, 4); r[ra] = eau; return;
            case 34: r[rd] = load(ea, 1); return;
            case 35: r[rd] = load(eau, 1); r[ra] = eau; return;
            case 36: store(ea, r[rd], 4); return;
            case 37: store(eau, r[rd], 4); r[ra] = eau; return;
            case 38: store(ea, r[rd], 1); return;
            case 39: store(eau, r[rd], 1); r[ra] = eau; return;
            case 40: r[rd] = load(ea, 2); return;
            case 41: r[rd] = load(eau, 2); r[ra] = eau; return;
            case 42: r[rd] = (uint64_t)(int16_t)load(ea, 2); return;
            case 43: r[rd] = (uint64_t)(int16_t)load(eau, 2); r[ra] = eau; return;
            case 44: store(ea, r[rd], 2); return;
            case 45: store(eau, r[rd], 2); r[ra] = eau; return;
            case 46:  // lmw
                for (uint32_t i = rd; i < 32; i++, ea += 4) r[i] = load(ea, 4);
                return;
            case 47:  // stmw
                for (uint32_t i = rd; i < 32; i++, ea += 4) store(ea, r[i], 4);
                return;
            case 48: f[rd] = as_float((uint32_t)load(ea, 4)); return;
            case 49: f[rd] = as_float((uint32_t)load(eau, 4)); r[ra] = eau; return;
            case 50: f[rd] = as_double(load(ea, 8)); return;
            case 51: f[rd] = as_double(load(eau, 8)); r[ra] = eau; return;
            case 52: store(ea, float_bits((float)f[rd]), 4); return;
            case 53: store(eau, float_bits((float)f[rd]), 4); r[ra] = eau; return;
            case 54: store(ea, double_bits(f[rd]), 8); return;
            case 55: store(eau, double_bits(f[rd]), 8); r[ra] = eau; return;
            case 58: {  // ld / ldu / lwa (DS-form)
                int64_t ds = (int16_t)(op & 0xFFFC);
                uint32_t e = (uint32_t)(a0 + ds), eu = (uint32_t)(r[ra] + ds);
                switch (op & 3) {
                    case 0: r[rd] = load(e, 8); return;
                    case 1: r[rd] = load(eu, 8); r[ra] = eu; return;
                    case 2: r[rd] = (uint64_t)(int32_t)load(e, 4); return;
                    default: stop(XENON_INVALID, op); return;
                }
            }
            case 59: op63(op, true); return;
            case 62: {  // std / stdu
                int64_t ds = (int16_t)(op & 0xFFFC);
                uint32_t e = (uint32_t)(a0 + ds), eu = (uint32_t)(r[ra] + ds);
                switch (op & 3) {
                    case 0: store(e, r[rd], 8); return;
                    case 1: store(eu, r[rd], 8); r[ra] = eu; return;
                    default: stop(XENON_INVALID, op); return;
                }
            }
            case 63: op63(op, false); return;
            default: stop(XENON_INVALID, op); return;
        }
    }

    uint32_t vrsave;

    uint32_t step() {
        uint32_t op = (uint32_t)load(pc, 4);
        npc = pc + 4;
        uint32_t start = cycles;
        cycles++;
        execute(op);
        if (event == XENON_TRAP || event == XENON_INVALID) {
            // Leave pc on the faulting instruction so the host can inspect or skip it.
            return cycles - start;
        }
        pc = npc;
        return cycles - start;
    }
};

extern "C" {

Xenon* xenon_new(XenonBus bus) {
    Xenon* c = (Xenon*)calloc(1, sizeof(Xenon));
    if (!c) return nullptr;
    c->bus = bus;
    return c;
}

void xenon_free(Xenon* c) { free(c); }

uint32_t xenon_run(Xenon* c, uint32_t budget) {
    uint32_t start = c->cycles;
    while (c->cycles - start < budget && c->event == XENON_NONE) c->step();
    uint32_t used = c->cycles - start;
    c->tb += used / 64 + 1;  // 50 MHz timebase against a 3.2 GHz core
    return used;
}

uint32_t xenon_step(Xenon* c) { return c->step(); }

int xenon_take_event(Xenon* c, uint32_t* opcode) {
    int e = c->event;
    if (opcode) *opcode = c->event_op;
    c->event = XENON_NONE;
    return e;
}

void xenon_get_regs(const Xenon* c, XenonRegs* o) {
    memcpy(o->r, c->r, sizeof(c->r));
    o->lr = c->lr;
    o->ctr = c->ctr;
    o->cr = c->cr;
    o->xer = c->xer;
    o->pc = c->pc;
    o->fpscr = c->fpscr;
}

void xenon_set_regs(Xenon* c, const XenonRegs* i) {
    memcpy(c->r, i->r, sizeof(c->r));
    c->lr = i->lr;
    c->ctr = i->ctr;
    c->cr = i->cr;
    c->xer = i->xer;
    c->pc = i->pc;
    c->fpscr = i->fpscr;
}

double xenon_fpr(const Xenon* c, int i) { return c->f[i & 31]; }
void xenon_set_fpr(Xenon* c, int i, double v) { c->f[i & 31] = v; }
void xenon_vr(const Xenon* c, int i, uint32_t out[4]) { memcpy(out, c->v[i & 127].w, 16); }
void xenon_set_vr(Xenon* c, int i, const uint32_t in[4]) { memcpy(c->v[i & 127].w, in, 16); }
uint64_t xenon_timebase(const Xenon* c) { return c->tb; }
void xenon_set_timebase(Xenon* c, uint64_t tb) { c->tb = tb; }

namespace {
struct Context {
    uint64_t r[32];
    double f[32];
    Vec128 v[128];
    uint64_t lr, ctr;
    uint32_t cr, xer, fpscr, vscr, vrsave, pc;
};
}  // namespace

uint32_t xenon_context_size(void) { return sizeof(Context); }

void xenon_save_context(const Xenon* c, void* buf) {
    Context* x = (Context*)buf;
    memcpy(x->r, c->r, sizeof(x->r));
    memcpy(x->f, c->f, sizeof(x->f));
    memcpy(x->v, c->v, sizeof(x->v));
    x->lr = c->lr;
    x->ctr = c->ctr;
    x->cr = c->cr;
    x->xer = c->xer;
    x->fpscr = c->fpscr;
    x->vscr = c->vscr;
    x->vrsave = c->vrsave;
    x->pc = c->pc;
}

void xenon_load_context(Xenon* c, const void* buf) {
    const Context* x = (const Context*)buf;
    memcpy(c->r, x->r, sizeof(x->r));
    memcpy(c->f, x->f, sizeof(x->f));
    memcpy(c->v, x->v, sizeof(x->v));
    c->lr = x->lr;
    c->ctr = x->ctr;
    c->cr = x->cr;
    c->xer = x->xer;
    c->fpscr = x->fpscr;
    c->vscr = x->vscr;
    c->vrsave = x->vrsave;
    c->pc = x->pc;
    c->reserve = false;
}

}  // extern "C"
