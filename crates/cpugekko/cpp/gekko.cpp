// IBM Gekko interpreter. See gekko.h.
#include "gekko.h"
#include <math.h>
#include <stdlib.h>
#include <string.h>

namespace {

enum : uint32_t {
    MSR_EE = 1u << 15, MSR_PR = 1u << 14, MSR_FP = 1u << 13, MSR_IP = 1u << 6, MSR_IR = 1u << 5, MSR_DR = 1u << 4, MSR_RI = 1u << 1,
    XER_SO = 1u << 31, XER_OV = 1u << 30, XER_CA = 1u << 29,
};

enum Spr : uint32_t {
    XER = 1, LR = 8, CTR = 9, DSISR = 18, DAR = 19, DEC = 22, SDR1 = 25, SRR0 = 26, SRR1 = 27,
    TBL_W = 284, TBU_W = 285, PVR = 287, IBAT0U = 528, DBAT0U = 536, GQR0 = 912, HID2 = 920,
    HID0 = 1008, HID1 = 1009, L2CR = 1017,
};

inline uint32_t rotl(uint32_t v, uint32_t n) {
    n &= 31;
    return n ? (v << n) | (v >> (32 - n)) : v;
}

// PowerPC mask with bits mb..me set (bit 0 = MSB), wrapping when mb > me.
inline uint32_t ppc_mask(uint32_t mb, uint32_t me) {
    uint32_t a = 0xFFFFFFFFu >> mb;
    uint32_t b = me == 31 ? 0xFFFFFFFFu : ~(0xFFFFFFFFu >> (me + 1));
    return mb <= me ? (a & b) : (a | b);
}

#if defined(_MSC_VER)
#include <stdlib.h>
inline uint16_t bswap16(uint16_t v) { return _byteswap_ushort(v); }
inline uint32_t bswap32(uint32_t v) { return _byteswap_ulong(v); }
inline uint64_t bswap64(uint64_t v) { return _byteswap_uint64(v); }
#else
inline uint16_t bswap16(uint16_t v) { return __builtin_bswap16(v); }
inline uint32_t bswap32(uint32_t v) { return __builtin_bswap32(v); }
inline uint64_t bswap64(uint64_t v) { return __builtin_bswap64(v); }
#endif

// Big-endian loads and stores of 1, 2, 4 or 8 bytes (the sizes the CPU uses).
inline uint64_t be_read(const uint8_t* p, int size) {
    switch (size) {
        case 1: return p[0];
        case 2: { uint16_t v; memcpy(&v, p, 2); return bswap16(v); }
        case 4: { uint32_t v; memcpy(&v, p, 4); return bswap32(v); }
        case 8: { uint64_t v; memcpy(&v, p, 8); return bswap64(v); }
        default: {
            uint64_t v = 0;
            for (int i = 0; i < size; i++) v = v << 8 | p[i];
            return v;
        }
    }
}

inline void be_write(uint8_t* p, uint64_t v, int size) {
    switch (size) {
        case 1: p[0] = (uint8_t)v; return;
        case 2: { uint16_t x = bswap16((uint16_t)v); memcpy(p, &x, 2); return; }
        case 4: { uint32_t x = bswap32((uint32_t)v); memcpy(p, &x, 4); return; }
        case 8: { uint64_t x = bswap64(v); memcpy(p, &x, 8); return; }
        default:
            for (int i = size - 1; i >= 0; i--) {
                p[i] = (uint8_t)v;
                v >>= 8;
            }
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

// Round to single precision (keeps the double container).
inline double to_single(double d) { return (double)(float)d; }

}  // namespace

struct Gekko {
    GekkoBus bus;
    uint32_t r[32];
    double ps0[32], ps1[32];
    uint32_t pc, npc;
    uint32_t cr, xer, lr, ctr, msr, fpscr;
    uint32_t spr[1024];
    uint32_t sr[16];
    uint64_t tb;
    uint32_t tb_frac;
    uint32_t tb_mul = 1;  // time-base ticks per tick event (when each instruction stands for several)
    uint32_t tb_div;  // instructions per time-base tick (12 at the real clock)
    bool irq_line;
    bool dec_pending;
    bool reserve;
    uint32_t reserve_addr;
    bool exception_taken;
    uint32_t cycles;

    // ---------------------------------------------------------------- memory

    // The last block-address-translation hit for instruction and data accesses. Games stay in
    // one block for long stretches, so most accesses skip the search of the four BAT pairs.
    struct XCache { uint32_t mask, ea_hi, pa_hi, version; bool valid, user; };
    XCache xc[2];
    uint32_t bat_version;

    bool translate(uint32_t ea, bool instr, uint32_t& pa) {
        if (!(msr & (instr ? MSR_IR : MSR_DR))) {
            pa = ea;
            return true;
        }
        XCache& c = xc[instr ? 1 : 0];
        bool user = (msr & MSR_PR) != 0;
        if (c.valid && c.version == bat_version && c.user == user && (ea & c.mask) == c.ea_hi) {
            pa = c.pa_hi | (ea & ~c.mask);
            return true;
        }
        uint32_t base = instr ? IBAT0U : DBAT0U;
        uint32_t valid_bit = (msr & MSR_PR) ? 1 : 2;
        for (int i = 0; i < 4; i++) {
            uint32_t u = spr[base + i * 2], l = spr[base + i * 2 + 1];
            if (!(u & valid_bit)) continue;
            uint32_t bl = (u >> 2) & 0x7FF;
            uint32_t mask = ~(bl << 17) & 0xFFFE0000u;
            if ((ea & mask) == (u & mask)) {
                pa = (l & mask) | (ea & ~mask);
                c.mask = mask; c.ea_hi = u & mask; c.pa_hi = l & mask; c.version = bat_version; c.user = user; c.valid = true;
                return true;
            }
        }
        // No BAT hit: games without page tables still address RAM through its mirrors.
        pa = ea & 0x3FFFFFFF;
        return true;
    }

    uint64_t load(uint32_t ea, int size) {
        uint32_t pa;
        translate(ea, false, pa);
        if (bus.ram && pa + size <= bus.ram_size) return be_read(bus.ram + pa, size);
        return bus.read(bus.ctx, pa, size);
    }

    void store(uint32_t ea, uint64_t v, int size) {
        uint32_t pa;
        translate(ea, false, pa);
        if (reserve && (pa & ~31u) == (reserve_addr & ~31u)) reserve = false;
        if (bus.ram && pa + size <= bus.ram_size)
            be_write(bus.ram + pa, v, size);
        else
            bus.write(bus.ctx, pa, v, size);
    }

    // ---------------------------------------------------------------- flags

    void set_cr_field(int f, uint32_t v) {
        int sh = (7 - f) * 4;
        cr = (cr & ~(0xFu << sh)) | ((v & 0xF) << sh);
    }
    uint32_t cr_field(int f) const { return (cr >> ((7 - f) * 4)) & 0xF; }

    void update_cr0(uint32_t v) {
        uint32_t f = (int32_t)v < 0 ? 8 : (int32_t)v > 0 ? 4 : 2;
        set_cr_field(0, f | ((xer & XER_SO) ? 1 : 0));
    }

    bool cr_bit(int b) const { return (cr >> (31 - b)) & 1; }
    void set_cr_bit(int b, bool v) {
        uint32_t m = 1u << (31 - b);
        cr = v ? (cr | m) : (cr & ~m);
    }

    void set_ca(bool c) { xer = c ? (xer | XER_CA) : (xer & ~XER_CA); }
    void set_ov(bool o) {
        if (o)
            xer |= XER_OV | XER_SO;
        else
            xer &= ~XER_OV;
    }

    // ---------------------------------------------------------------- exceptions

    void exception(uint32_t vector, uint32_t srr0) {
        spr[SRR0] = srr0;
        spr[SRR1] = msr & 0x87C0FFFF;
        msr &= ~(MSR_EE | MSR_PR | MSR_FP | (1u << 11) | (1u << 8) | (1u << 10) | (1u << 9) | MSR_IR | MSR_DR | MSR_RI);
        npc = ((msr & MSR_IP) ? 0xFFF00000u : 0) | vector;
        exception_taken = true;
    }

    // ---------------------------------------------------------------- SPRs

    uint32_t mfspr(uint32_t n) {
        switch (n) {
            case XER: return xer;
            case LR: return lr;
            case CTR: return ctr;
            case 268: return (uint32_t)tb;
            case 269: return (uint32_t)(tb >> 32);
            case PVR: return 0x00083214;
            default: return spr[n & 1023];
        }
    }

    void mtspr(uint32_t n, uint32_t v) {
        switch (n) {
            case XER: xer = v; return;
            case LR: lr = v; return;
            case CTR: ctr = v; return;
            case TBL_W: tb = (tb & 0xFFFFFFFF00000000ull) | v; return;
            case TBU_W: tb = (tb & 0xFFFFFFFFull) | ((uint64_t)v << 32); return;
            default:
                if (n >= 528 && n <= 543) bat_version++;
                spr[n & 1023] = v;
                return;
        }
    }

    // ---------------------------------------------------------------- paired-single quantisation

    double dequant(uint32_t ea, int type, int scale, int& size) {
        double s = ldexp(1.0, -scale);
        switch (type) {
            case 4: size = 1; return (uint8_t)load(ea, 1) * s;
            case 5: size = 2; return (uint16_t)load(ea, 2) * s;
            case 6: size = 1; return (int8_t)load(ea, 1) * s;
            case 7: size = 2; return (int16_t)load(ea, 2) * s;
            default: size = 4; return as_float((uint32_t)load(ea, 4));
        }
    }

    void quant(uint32_t ea, double v, int type, int scale, int& size) {
        double s = ldexp(1.0, scale);
        auto clampi = [](double x, double lo, double hi) { return x < lo ? lo : x > hi ? hi : x; };
        switch (type) {
            case 4: size = 1; store(ea, (uint8_t)clampi(v * s, 0, 255), 1); return;
            case 5: size = 2; store(ea, (uint16_t)clampi(v * s, 0, 65535), 2); return;
            case 6: size = 1; store(ea, (uint8_t)(int8_t)clampi(v * s, -128, 127), 1); return;
            case 7: size = 2; store(ea, (uint16_t)(int16_t)clampi(v * s, -32768, 32767), 2); return;
            default: size = 4; store(ea, float_bits((float)v), 4); return;
        }
    }

    void psq_load(uint32_t ea, int frd, bool w, int gqr) {
        uint32_t g = spr[GQR0 + gqr];
        int type = (g >> 16) & 7, scale = (g >> 24) & 63;
        if (scale & 32) scale -= 64;
        int size;
        ps0[frd] = dequant(ea, type, scale, size);
        ps1[frd] = w ? 1.0 : dequant(ea + size, type, scale, size);
    }

    void psq_store(uint32_t ea, int frs, bool w, int gqr) {
        uint32_t g = spr[GQR0 + gqr];
        int type = g & 7, scale = (g >> 8) & 63;
        if (scale & 32) scale -= 64;
        int size;
        quant(ea, ps0[frs], type, scale, size);
        if (!w) quant(ea + size, ps1[frs], type, scale, size);
    }

    // ---------------------------------------------------------------- floating point

    void fcompare(double a, double b, int crf) {
        uint32_t c = isnan(a) || isnan(b) ? 1 : a < b ? 8 : a > b ? 4 : 2;
        set_cr_field(crf, c);
        fpscr = (fpscr & ~0xF000u) | (c << 12);
    }

    void fp_cr1() { set_cr_field(1, fpscr >> 28); }

    double fres(double x) { return 1.0 / x; }
    double frsqrte(double x) { return 1.0 / sqrt(x); }

    // ---------------------------------------------------------------- branches

    bool bo_ok(uint32_t bo, uint32_t bi) {
        if (!(bo & 4)) ctr--;
        bool ctr_ok = (bo & 4) || ((ctr != 0) ^ ((bo >> 1) & 1));
        bool cond_ok = (bo & 16) || (cr_bit(bi) == ((bo >> 3) & 1));
        return ctr_ok && cond_ok;
    }

    // ---------------------------------------------------------------- execution

    void op19(uint32_t op) {
        uint32_t bo = (op >> 21) & 31, bi = (op >> 16) & 31, bb = (op >> 11) & 31;
        uint32_t xo = (op >> 1) & 1023;
        bool lk = op & 1;
        switch (xo) {
            case 0:  // mcrf
                set_cr_field(bo >> 2, cr_field(bi >> 2));
                return;
            case 16: {  // bclr
                uint32_t target = lr & ~3u;
                if (bo_ok(bo, bi)) {
                    if (lk) lr = pc + 4;
                    npc = target;
                } else if (lk) {
                    lr = pc + 4;
                }
                return;
            }
            case 528: {  // bcctr
                bool cond_ok = (bo & 16) || (cr_bit(bi) == ((bo >> 3) & 1));
                uint32_t target = ctr & ~3u;
                if (lk) lr = pc + 4;
                if (cond_ok) npc = target;
                return;
            }
            case 50:  // rfi
                msr = (msr & ~0x87C0FFFFu) | (spr[SRR1] & 0x87C0FFFF);
                npc = spr[SRR0] & ~3u;
                return;
            case 150: return;  // isync
            case 33: set_cr_bit(bo, !(cr_bit(bi) || cr_bit(bb))); return;
            case 129: set_cr_bit(bo, cr_bit(bi) && !cr_bit(bb)); return;
            case 193: set_cr_bit(bo, cr_bit(bi) != cr_bit(bb)); return;
            case 225: set_cr_bit(bo, !(cr_bit(bi) && cr_bit(bb))); return;
            case 257: set_cr_bit(bo, cr_bit(bi) && cr_bit(bb)); return;
            case 289: set_cr_bit(bo, cr_bit(bi) == cr_bit(bb)); return;
            case 417: set_cr_bit(bo, cr_bit(bi) || !cr_bit(bb)); return;
            case 449: set_cr_bit(bo, cr_bit(bi) || cr_bit(bb)); return;
            default: return;
        }
    }

    void op31(uint32_t op) {
        uint32_t rd = (op >> 21) & 31, ra = (op >> 16) & 31, rb = (op >> 11) & 31;
        uint32_t xo = (op >> 1) & 1023;
        bool rc = op & 1;
        bool oe = op & (1u << 10);
        uint32_t a = r[ra], b = r[rb];
        uint32_t ea0 = (ra ? a : 0) + b;  // indexed address (rA|0) + rB
        // XO-form arithmetic (9-bit opcode + OE).
        switch (xo & 511) {
            case 8: {  // subfc
                uint32_t res = b - a;
                set_ca(b >= a);
                if (oe) set_ov(((a ^ b) & (b ^ res)) >> 31);
                r[rd] = res;
                if (rc) update_cr0(res);
                return;
            }
            case 10: {  // addc
                uint64_t w = (uint64_t)a + b;
                set_ca(w >> 32);
                if (oe) set_ov((~(a ^ b) & (a ^ (uint32_t)w)) >> 31);
                r[rd] = (uint32_t)w;
                if (rc) update_cr0(r[rd]);
                return;
            }
            case 11: r[rd] = (uint32_t)(((uint64_t)a * b) >> 32); if (rc) update_cr0(r[rd]); return;
            case 75: r[rd] = (uint32_t)(((int64_t)(int32_t)a * (int32_t)b) >> 32); if (rc) update_cr0(r[rd]); return;
            case 40: {  // subf
                uint32_t res = b - a;
                if (oe) set_ov(((a ^ b) & (b ^ res)) >> 31);
                r[rd] = res;
                if (rc) update_cr0(res);
                return;
            }
            case 104: {  // neg
                if (oe) set_ov(a == 0x80000000u);
                r[rd] = (uint32_t)(-(int32_t)a);
                if (rc) update_cr0(r[rd]);
                return;
            }
            case 136:
            case 138:
            case 200:
            case 202:
            case 232:
            case 234: {  // subfe adde subfze addze subfme addme
                uint32_t ca = (xer & XER_CA) ? 1 : 0;
                uint32_t x = (xo & 511) == 136 || (xo & 511) == 200 || (xo & 511) == 232 ? ~a : a;
                uint32_t y = (xo & 511) == 136 || (xo & 511) == 138 ? b : (xo & 511) >= 232 ? 0xFFFFFFFFu : 0;
                uint64_t w = (uint64_t)x + y + ca;
                set_ca(w >> 32);
                if (oe) set_ov((~(x ^ y) & (x ^ (uint32_t)w)) >> 31);
                r[rd] = (uint32_t)w;
                if (rc) update_cr0(r[rd]);
                return;
            }
            case 235: {  // mullw
                int64_t m = (int64_t)(int32_t)a * (int32_t)b;
                if (oe) set_ov(m < INT32_MIN || m > INT32_MAX);
                r[rd] = (uint32_t)m;
                if (rc) update_cr0(r[rd]);
                cycles += 2;
                return;
            }
            case 266: {  // add
                uint32_t res = a + b;
                if (oe) set_ov((~(a ^ b) & (a ^ res)) >> 31);
                r[rd] = res;
                if (rc) update_cr0(res);
                return;
            }
            case 459: {  // divwu
                bool bad = b == 0;
                if (oe) set_ov(bad);
                r[rd] = bad ? 0 : a / b;
                if (rc) update_cr0(r[rd]);
                cycles += 18;
                return;
            }
            case 491: {  // divw
                bool bad = b == 0 || (a == 0x80000000u && b == 0xFFFFFFFFu);
                if (oe) set_ov(bad);
                r[rd] = bad ? (((int32_t)a < 0 && b == 0) ? 0xFFFFFFFFu : 0) : (uint32_t)((int32_t)a / (int32_t)b);
                if (rc) update_cr0(r[rd]);
                cycles += 18;
                return;
            }
            default: break;
        }
        uint32_t rs = r[rd];  // X-form: rD field is rS for logic and stores
        switch (xo) {
            case 0:
            case 32: {  // cmp / cmpl
                int crf = rd >> 2;
                uint32_t c;
                if (xo == 0)
                    c = (int32_t)a < (int32_t)b ? 8 : (int32_t)a > (int32_t)b ? 4 : 2;
                else
                    c = a < b ? 8 : a > b ? 4 : 2;
                set_cr_field(crf, c | ((xer & XER_SO) ? 1 : 0));
                return;
            }
            case 4: return;  // tw (traps unused by games)
            case 19: r[rd] = cr; return;
            case 20: r[rd] = (uint32_t)load(ea0 & ~3u, 4); reserve = true; reserve_addr = ea0; return;
            case 23: r[rd] = (uint32_t)load(ea0, 4); return;
            case 55: r[rd] = (uint32_t)load(a + b, 4); r[ra] = a + b; return;
            case 87: r[rd] = (uint32_t)load(ea0, 1); return;
            case 119: r[rd] = (uint32_t)load(a + b, 1); r[ra] = a + b; return;
            case 279: r[rd] = (uint32_t)load(ea0, 2); return;
            case 311: r[rd] = (uint32_t)load(a + b, 2); r[ra] = a + b; return;
            case 343: r[rd] = (uint32_t)(int32_t)(int16_t)load(ea0, 2); return;
            case 375: r[rd] = (uint32_t)(int32_t)(int16_t)load(a + b, 2); r[ra] = a + b; return;
            case 534: {  // lwbrx
                uint32_t v = (uint32_t)load(ea0, 4);
                r[rd] = (v >> 24) | ((v >> 8) & 0xFF00) | ((v << 8) & 0xFF0000) | (v << 24);
                return;
            }
            case 790: {  // lhbrx
                uint32_t v = (uint32_t)load(ea0, 2);
                r[rd] = ((v >> 8) & 0xFF) | ((v & 0xFF) << 8);
                return;
            }
            case 150: {  // stwcx.
                bool ok = reserve;
                if (ok) store(ea0, rs, 4);
                reserve = false;
                set_cr_field(0, (ok ? 2 : 0) | ((xer & XER_SO) ? 1 : 0));
                return;
            }
            case 151: store(ea0, rs, 4); return;
            case 183: store(a + b, rs, 4); r[ra] = a + b; return;
            case 215: store(ea0, rs, 1); return;
            case 247: store(a + b, rs, 1); r[ra] = a + b; return;
            case 407: store(ea0, rs, 2); return;
            case 439: store(a + b, rs, 2); r[ra] = a + b; return;
            case 662: store(ea0, (rs >> 24) | ((rs >> 8) & 0xFF00) | ((rs << 8) & 0xFF0000) | (rs << 24), 4); return;
            case 918: store(ea0, ((rs >> 8) & 0xFF) | ((rs & 0xFF) << 8), 2); return;
            case 24: r[ra] = (b & 32) ? 0 : rs << (b & 31); if (rc) update_cr0(r[ra]); return;
            case 536: r[ra] = (b & 32) ? 0 : rs >> (b & 31); if (rc) update_cr0(r[ra]); return;
            case 792: {  // sraw
                uint32_t n = b & 63;
                int32_t v = (int32_t)rs;
                if (n > 31) {
                    r[ra] = v < 0 ? 0xFFFFFFFFu : 0;
                    set_ca(v < 0);
                } else {
                    r[ra] = (uint32_t)(v >> n);
                    set_ca(v < 0 && n && (rs & ((1u << n) - 1)));
                }
                if (rc) update_cr0(r[ra]);
                return;
            }
            case 824: {  // srawi
                uint32_t n = rb;
                int32_t v = (int32_t)rs;
                r[ra] = (uint32_t)(v >> n);
                set_ca(v < 0 && n && (rs & ((1u << n) - 1)));
                if (rc) update_cr0(r[ra]);
                return;
            }
            case 26: {
                uint32_t v = rs, n = 0;
                while (n < 32 && !(v & 0x80000000u)) {
                    v <<= 1;
                    n++;
                }
                r[ra] = n;
                if (rc) update_cr0(n);
                return;
            }
            case 28: r[ra] = rs & b; if (rc) update_cr0(r[ra]); return;
            case 60: r[ra] = rs & ~b; if (rc) update_cr0(r[ra]); return;
            case 124: r[ra] = ~(rs | b); if (rc) update_cr0(r[ra]); return;
            case 284: r[ra] = ~(rs ^ b); if (rc) update_cr0(r[ra]); return;
            case 316: r[ra] = rs ^ b; if (rc) update_cr0(r[ra]); return;
            case 412: r[ra] = rs | ~b; if (rc) update_cr0(r[ra]); return;
            case 444: r[ra] = rs | b; if (rc) update_cr0(r[ra]); return;
            case 476: r[ra] = ~(rs & b); if (rc) update_cr0(r[ra]); return;
            case 922: r[ra] = (uint32_t)(int32_t)(int16_t)rs; if (rc) update_cr0(r[ra]); return;
            case 954: r[ra] = (uint32_t)(int32_t)(int8_t)rs; if (rc) update_cr0(r[ra]); return;
            case 83: r[rd] = msr; return;
            case 146: msr = rs; return;
            case 144: {  // mtcrf
                uint32_t crm = (op >> 12) & 0xFF;
                uint32_t mask = 0;
                for (int i = 0; i < 8; i++)
                    if (crm & (0x80 >> i)) mask |= 0xF0000000u >> (i * 4);
                cr = (cr & ~mask) | (rs & mask);
                return;
            }
            case 210: sr[ra & 15] = rs; return;
            case 242: sr[b >> 28] = rs; return;
            case 595: r[rd] = sr[ra & 15]; return;
            case 659: r[rd] = sr[b >> 28]; return;
            case 339: r[rd] = mfspr(((op >> 16) & 31) | (((op >> 11) & 31) << 5)); return;
            case 371: r[rd] = mfspr(((op >> 16) & 31) | (((op >> 11) & 31) << 5)); return;
            case 467: mtspr(((op >> 16) & 31) | (((op >> 11) & 31) << 5), rs); return;
            case 512: set_cr_field(rd >> 2, xer >> 28); xer &= 0x0FFFFFFF; return;
            case 535: { double v = as_float((uint32_t)load(ea0, 4)); ps0[rd] = ps1[rd] = v; return; }
            case 567: { double v = as_float((uint32_t)load(a + b, 4)); ps0[rd] = ps1[rd] = v; r[ra] = a + b; return; }
            case 599: ps0[rd] = as_double(load(ea0, 8)); return;
            case 631: ps0[rd] = as_double(load(a + b, 8)); r[ra] = a + b; return;
            case 663: store(ea0, float_bits((float)ps0[rd]), 4); return;
            case 695: store(a + b, float_bits((float)ps0[rd]), 4); r[ra] = a + b; return;
            case 727: store(ea0, double_bits(ps0[rd]), 8); return;
            case 759: store(a + b, double_bits(ps0[rd]), 8); r[ra] = a + b; return;
            case 983: store(ea0, (uint32_t)double_bits(ps0[rd]), 4); return;
            case 1014: {  // dcbz
                uint32_t base = ea0 & ~31u;
                for (int i = 0; i < 32; i += 8) store(base + i, 0, 8);
                return;
            }
            case 597:
            case 533: {  // lswi / lswx
                uint32_t n = xo == 597 ? (rb ? rb : 32) : (xer & 0x7F);
                uint32_t ea = xo == 597 ? (ra ? a : 0) : ea0;
                uint32_t reg = rd - 1;
                for (uint32_t i = 0; i < n; i++) {
                    if (i % 4 == 0) {
                        reg = (reg + 1) & 31;
                        r[reg] = 0;
                    }
                    r[reg] |= (uint32_t)load(ea + i, 1) << (24 - (i % 4) * 8);
                }
                return;
            }
            case 725:
            case 661: {  // stswi / stswx
                uint32_t n = xo == 725 ? (rb ? rb : 32) : (xer & 0x7F);
                uint32_t ea = xo == 725 ? (ra ? a : 0) : ea0;
                uint32_t reg = rd - 1;
                for (uint32_t i = 0; i < n; i++) {
                    if (i % 4 == 0) reg = (reg + 1) & 31;
                    store(ea + i, (r[reg] >> (24 - (i % 4) * 8)) & 0xFF, 1);
                }
                return;
            }
            default: return;  // cache/sync hints (dcbf, dcbi, dcbst, dcbt, icbi, sync, eieio, tlbie)
        }
    }

    void op59(uint32_t op) {
        uint32_t d = (op >> 21) & 31, a = (op >> 16) & 31, b = (op >> 11) & 31, c = (op >> 6) & 31;
        double fa = ps0[a], fb = ps0[b], fc = ps0[c], res;
        switch ((op >> 1) & 31) {
            case 18: res = fa / fb; break;
            case 20: res = fa - fb; break;
            case 21: res = fa + fb; break;
            case 24: res = fres(fb); break;
            case 25: res = fa * fc; break;
            case 28: res = fa * fc - fb; break;
            case 29: res = fa * fc + fb; break;
            case 30: res = -(fa * fc - fb); break;
            case 31: res = -(fa * fc + fb); break;
            default: return;
        }
        res = to_single(res);
        ps0[d] = ps1[d] = res;
        if (op & 1) fp_cr1();
    }

    void op63(uint32_t op) {
        uint32_t d = (op >> 21) & 31, a = (op >> 16) & 31, b = (op >> 11) & 31, c = (op >> 6) & 31;
        double fa = ps0[a], fb = ps0[b], fc = ps0[c];
        uint32_t xa = (op >> 1) & 31;
        if (xa >= 18 && xa != 22 && xa != 24 && xa != 27) {
            double res;
            switch (xa) {
                case 18: res = fa / fb; break;
                case 20: res = fa - fb; break;
                case 21: res = fa + fb; break;
                case 23: res = fa >= 0.0 ? fc : fb; break;
                case 25: res = fa * fc; break;
                case 26: res = frsqrte(fb); break;
                case 28: res = fa * fc - fb; break;
                case 29: res = fa * fc + fb; break;
                case 30: res = -(fa * fc - fb); break;
                case 31: res = -(fa * fc + fb); break;
                default: return;
            }
            ps0[d] = res;
            if (op & 1) fp_cr1();
            return;
        }
        switch ((op >> 1) & 1023) {
            case 0:
            case 32: fcompare(fa, fb, d >> 2); return;
            case 12: ps0[d] = to_single(fb); break;
            case 14:
            case 15: {  // fctiw / fctiwz
                double v = ((op >> 1) & 1023) == 15 ? trunc(fb) : nearbyint(fb);
                int32_t i = isnan(v) ? INT32_MIN : v >= 2147483647.0 ? INT32_MAX : v <= -2147483648.0 ? INT32_MIN : (int32_t)v;
                ps0[d] = as_double(0xFFF8000000000000ull | (uint32_t)i);
                break;
            }
            case 38: fpscr |= 0x80000000u >> d; return;
            case 70: fpscr &= ~(0x80000000u >> d); return;
            case 40: ps0[d] = -fb; break;
            case 64: set_cr_field(d >> 2, (fpscr >> ((7 - (a >> 2)) * 4)) & 0xF); return;
            case 72: ps0[d] = fb; break;
            case 134: {
                int sh = (7 - (d >> 2)) * 4;
                fpscr = (fpscr & ~(0xFu << sh)) | (((op >> 12) & 0xF) << sh);
                return;
            }
            case 136: ps0[d] = -fabs(fb); break;
            case 264: ps0[d] = fabs(fb); break;
            case 583: ps0[d] = as_double(0xFFF8000000000000ull | fpscr); break;
            case 711: {
                uint32_t fm = (op >> 17) & 0xFF;
                uint32_t mask = 0;
                for (int i = 0; i < 8; i++)
                    if (fm & (0x80 >> i)) mask |= 0xF0000000u >> (i * 4);
                fpscr = (fpscr & ~mask) | ((uint32_t)double_bits(fb) & mask);
                return;
            }
            default: return;
        }
        if (op & 1) fp_cr1();
    }

    void op4(uint32_t op) {
        uint32_t d = (op >> 21) & 31, a = (op >> 16) & 31, b = (op >> 11) & 31, c = (op >> 6) & 31;
        uint32_t ra = r[a], rb = r[b];
        // Quantised indexed loads/stores.
        switch ((op >> 1) & 63) {
            case 6:
            case 38: {
                uint32_t ea = (a || ((op >> 1) & 63) == 38 ? ra : 0) + rb;
                psq_load(ea, d, (op >> 10) & 1, (op >> 7) & 7);
                if (((op >> 1) & 63) == 38) r[a] = ea;
                return;
            }
            case 7:
            case 39: {
                uint32_t ea = (a || ((op >> 1) & 63) == 39 ? ra : 0) + rb;
                psq_store(ea, d, (op >> 10) & 1, (op >> 7) & 7);
                if (((op >> 1) & 63) == 39) r[a] = ea;
                return;
            }
            default: break;
        }
        double a0 = ps0[a], a1 = ps1[a], b0 = ps0[b], b1 = ps1[b], c0 = ps0[c], c1 = ps1[c];
        double r0, r1;
        switch ((op >> 1) & 31) {
            case 10: r0 = a0 + b1; r1 = c1; break;                // ps_sum0
            case 11: r0 = c0; r1 = a0 + b1; break;                // ps_sum1
            case 12: r0 = a0 * c0; r1 = a1 * c0; break;           // ps_muls0
            case 13: r0 = a0 * c1; r1 = a1 * c1; break;           // ps_muls1
            case 14: r0 = a0 * c0 + b0; r1 = a1 * c0 + b1; break;  // ps_madds0
            case 15: r0 = a0 * c1 + b0; r1 = a1 * c1 + b1; break;  // ps_madds1
            case 18: r0 = a0 / b0; r1 = a1 / b1; break;
            case 20: r0 = a0 - b0; r1 = a1 - b1; break;
            case 21: r0 = a0 + b0; r1 = a1 + b1; break;
            case 23: r0 = a0 >= 0.0 ? c0 : b0; r1 = a1 >= 0.0 ? c1 : b1; break;
            case 24: r0 = fres(b0); r1 = fres(b1); break;
            case 25: r0 = a0 * c0; r1 = a1 * c1; break;
            case 26: r0 = frsqrte(b0); r1 = frsqrte(b1); break;
            case 28: r0 = a0 * c0 - b0; r1 = a1 * c1 - b1; break;
            case 29: r0 = a0 * c0 + b0; r1 = a1 * c1 + b1; break;
            case 30: r0 = -(a0 * c0 - b0); r1 = -(a1 * c1 - b1); break;
            case 31: r0 = -(a0 * c0 + b0); r1 = -(a1 * c1 + b1); break;
            default:
                switch ((op >> 1) & 1023) {
                    case 0:
                    case 32: fcompare(a0, b0, d >> 2); return;
                    case 64:
                    case 96: fcompare(a1, b1, d >> 2); return;
                    case 40: r0 = -b0; r1 = -b1; break;
                    case 72: r0 = b0; r1 = b1; break;
                    case 136: r0 = -fabs(b0); r1 = -fabs(b1); break;
                    case 264: r0 = fabs(b0); r1 = fabs(b1); break;
                    case 528: r0 = a0; r1 = b0; break;
                    case 560: r0 = a0; r1 = b1; break;
                    case 592: r0 = a1; r1 = b0; break;
                    case 624: r0 = a1; r1 = b1; break;
                    case 1014: {  // dcbz_l (locked cache)
                        uint32_t base = ((a ? ra : 0) + rb) & ~31u;
                        for (int i = 0; i < 32; i += 8) store(base + i, 0, 8);
                        return;
                    }
                    default: return;
                }
                ps0[d] = r0;
                ps1[d] = r1;
                if (op & 1) fp_cr1();
                return;
        }
        ps0[d] = to_single(r0);
        ps1[d] = to_single(r1);
        if (op & 1) fp_cr1();
    }

    void execute(uint32_t op) {
        uint32_t d = (op >> 21) & 31, a = (op >> 16) & 31;
        uint32_t simm = (uint32_t)(int32_t)(int16_t)op;
        uint32_t uimm = op & 0xFFFF;
        uint32_t ra0 = a ? r[a] : 0;
        uint32_t ea = ra0 + simm;
        switch (op >> 26) {
            case 4: return op4(op);
            case 7: r[d] = (uint32_t)((int32_t)r[a] * (int32_t)simm); return;
            case 8: {
                uint32_t res = simm - r[a];
                set_ca(simm >= r[a]);
                r[d] = res;
                return;
            }
            case 10: {
                uint32_t v = r[a];
                set_cr_field(d >> 2, (v < uimm ? 8 : v > uimm ? 4 : 2) | ((xer & XER_SO) ? 1 : 0));
                return;
            }
            case 11: {
                int32_t v = (int32_t)r[a], s = (int32_t)simm;
                set_cr_field(d >> 2, (v < s ? 8 : v > s ? 4 : 2) | ((xer & XER_SO) ? 1 : 0));
                return;
            }
            case 12:
            case 13: {
                uint64_t w = (uint64_t)r[a] + simm;
                set_ca(w >> 32);
                r[d] = (uint32_t)w;
                if ((op >> 26) == 13) update_cr0(r[d]);
                return;
            }
            case 14: r[d] = ra0 + simm; return;
            case 15: r[d] = ra0 + (uimm << 16); return;
            case 16: {  // bc
                uint32_t bo = d, bi = a;
                uint32_t off = (uint32_t)(int32_t)(int16_t)(op & 0xFFFC);
                uint32_t target = (op & 2) ? off : pc + off;
                if (op & 1) lr = pc + 4;
                if (bo_ok(bo, bi)) npc = target;
                return;
            }
            case 17: exception(0xC00, pc + 4); return;
            case 18: {
                uint32_t off = (uint32_t)((int32_t)(op << 6) >> 6) & ~3u;
                if (op & 1) lr = pc + 4;
                npc = (op & 2) ? off : pc + off;
                return;
            }
            case 19: return op19(op);
            case 20: {  // rlwimi
                uint32_t m = ppc_mask((op >> 6) & 31, (op >> 1) & 31);
                r[a] = (rotl(r[d], (op >> 11) & 31) & m) | (r[a] & ~m);
                if (op & 1) update_cr0(r[a]);
                return;
            }
            case 21: {  // rlwinm
                r[a] = rotl(r[d], (op >> 11) & 31) & ppc_mask((op >> 6) & 31, (op >> 1) & 31);
                if (op & 1) update_cr0(r[a]);
                return;
            }
            case 23: {  // rlwnm
                r[a] = rotl(r[d], r[(op >> 11) & 31] & 31) & ppc_mask((op >> 6) & 31, (op >> 1) & 31);
                if (op & 1) update_cr0(r[a]);
                return;
            }
            case 24: r[a] = r[d] | uimm; return;
            case 25: r[a] = r[d] | (uimm << 16); return;
            case 26: r[a] = r[d] ^ uimm; return;
            case 27: r[a] = r[d] ^ (uimm << 16); return;
            case 28: r[a] = r[d] & uimm; update_cr0(r[a]); return;
            case 29: r[a] = r[d] & (uimm << 16); update_cr0(r[a]); return;
            case 31: return op31(op);
            case 32: r[d] = (uint32_t)load(ea, 4); return;
            case 33: r[d] = (uint32_t)load(r[a] + simm, 4); r[a] += simm; return;
            case 34: r[d] = (uint32_t)load(ea, 1); return;
            case 35: r[d] = (uint32_t)load(r[a] + simm, 1); r[a] += simm; return;
            case 36: store(ea, r[d], 4); return;
            case 37: store(r[a] + simm, r[d], 4); r[a] += simm; return;
            case 38: store(ea, r[d], 1); return;
            case 39: store(r[a] + simm, r[d], 1); r[a] += simm; return;
            case 40: r[d] = (uint32_t)load(ea, 2); return;
            case 41: r[d] = (uint32_t)load(r[a] + simm, 2); r[a] += simm; return;
            case 42: r[d] = (uint32_t)(int32_t)(int16_t)load(ea, 2); return;
            case 43: r[d] = (uint32_t)(int32_t)(int16_t)load(r[a] + simm, 2); r[a] += simm; return;
            case 44: store(ea, r[d], 2); return;
            case 45: store(r[a] + simm, r[d], 2); r[a] += simm; return;
            case 46:
                for (uint32_t i = d; i < 32; i++) r[i] = (uint32_t)load(ea + (i - d) * 4, 4);
                return;
            case 47:
                for (uint32_t i = d; i < 32; i++) store(ea + (i - d) * 4, r[i], 4);
                return;
            case 48: { double v = as_float((uint32_t)load(ea, 4)); ps0[d] = ps1[d] = v; return; }
            case 49: { double v = as_float((uint32_t)load(r[a] + simm, 4)); ps0[d] = ps1[d] = v; r[a] += simm; return; }
            case 50: ps0[d] = as_double(load(ea, 8)); return;
            case 51: ps0[d] = as_double(load(r[a] + simm, 8)); r[a] += simm; return;
            case 52: store(ea, float_bits((float)ps0[d]), 4); return;
            case 53: store(r[a] + simm, float_bits((float)ps0[d]), 4); r[a] += simm; return;
            case 54: store(ea, double_bits(ps0[d]), 8); return;
            case 55: store(r[a] + simm, double_bits(ps0[d]), 8); r[a] += simm; return;
            case 56:
            case 57:
            case 60:
            case 61: {  // psq_l(u) / psq_st(u): 12-bit offset
                uint32_t off = (uint32_t)(((int32_t)(op << 20)) >> 20);
                bool upd = (op >> 26) & 1;
                uint32_t e = (upd ? r[a] : ra0) + off;
                bool w = (op >> 15) & 1;
                int i = (op >> 12) & 7;
                if ((op >> 26) < 60)
                    psq_load(e, d, w, i);
                else
                    psq_store(e, d, w, i);
                if (upd) r[a] = e;
                return;
            }
            case 59: return op59(op);
            case 63: return op63(op);
            default: return;
        }
    }

    uint32_t step() {
        cycles = 1;
        exception_taken = false;
        // Time base and decrementer run at 1/12 of the CPU clock.
        if (++tb_frac >= tb_div) {
            tb_frac = 0;
            tb += tb_mul;
            uint32_t dec = spr[DEC];
            spr[DEC] = dec - tb_mul;
            // The decrementer interrupt is raised when the count goes negative and waits
            // until interrupts are enabled.
            if (!(dec & 0x80000000u) && ((dec - tb_mul) & 0x80000000u)) dec_pending = true;
        }
        if (dec_pending && (msr & MSR_EE)) {
            dec_pending = false;
            exception(0x900, pc);
            pc = npc;
            return cycles;
        }
        if (irq_line && (msr & MSR_EE)) {
            exception(0x500, pc);
            pc = npc;
            return cycles;
        }
        uint32_t pa;
        translate(pc, true, pa);
        uint32_t op = (bus.ram && pa + 4 <= bus.ram_size) ? (uint32_t)be_read(bus.ram + pa, 4) : (uint32_t)bus.read(bus.ctx, pa, 4);
        npc = pc + 4;
        execute(op);
        pc = npc;
        return cycles;
    }
};

extern "C" {

Gekko* gekko_new(GekkoBus bus) {
    Gekko* c = (Gekko*)calloc(1, sizeof(Gekko));
    if (!c) return nullptr;
    c->bus = bus;
    gekko_reset(c);
    return c;
}

void gekko_free(Gekko* cpu) { free(cpu); }

void gekko_reset(Gekko* c) {
    GekkoBus bus = c->bus;
    memset(c, 0, sizeof(Gekko));
    c->bus = bus;
    c->tb_div = 12;
    c->pc = 0xFFF00100;  // system reset vector
    c->msr = MSR_IP;
    c->spr[HID2] = 0xE0000000;  // paired singles + locked cache enabled
}

uint32_t gekko_step(Gekko* cpu) { return cpu->step(); }

// Instructions per time-base tick: hosts that run fewer instructions than the real
// chip per second use a smaller number so the clock still keeps real-time pace.
void gekko_set_tb_divisor(Gekko* c, uint32_t d) { c->tb_div = d ? d : 1; }
void gekko_set_tb_multiplier(Gekko* c, uint32_t m) { c->tb_mul = m ? m : 1; }

uint32_t gekko_run(Gekko* c, uint32_t budget) {
    uint32_t used = 0;
    while (used < budget) {
        uint32_t before = c->pc;
        used += c->step();
        // `b .` idle loops wait for an interrupt: skip ahead.
        if (c->pc == before && !c->exception_taken) {
            uint32_t skip = budget - used;
            uint32_t ticks = skip / c->tb_div * c->tb_mul;
            uint32_t dec = c->spr[DEC];
            if (!(dec & 0x80000000u) && ticks > dec) {
                // Stop exactly where the decrementer goes negative.
                ticks = dec + 1;
                c->dec_pending = true;
            }
            c->tb += ticks;
            c->spr[DEC] = dec - ticks;
            used += ticks / c->tb_mul * c->tb_div;
            if (c->dec_pending || c->irq_line) break;
            if (used < budget) used = budget;
        }
    }
    return used;
}

void gekko_set_irq(Gekko* cpu, int asserted) { cpu->irq_line = asserted != 0; }

void gekko_get_regs(const Gekko* c, GekkoRegs* out) {
    memcpy(out->r, c->r, sizeof(c->r));
    out->pc = c->pc;
    out->lr = c->lr;
    out->ctr = c->ctr;
    out->cr = c->cr;
    out->xer = c->xer;
    out->msr = c->msr;
}

void gekko_set_regs(Gekko* c, const GekkoRegs* in) {
    memcpy(c->r, in->r, sizeof(c->r));
    c->pc = in->pc;
    c->lr = in->lr;
    c->ctr = in->ctr;
    c->cr = in->cr;
    c->xer = in->xer;
    c->msr = in->msr;
}

double gekko_fpr(const Gekko* c, int i, int ps1) { return ps1 ? c->ps1[i & 31] : c->ps0[i & 31]; }

void gekko_set_fpr(Gekko* c, int i, double a, double b) {
    c->ps0[i & 31] = a;
    c->ps1[i & 31] = b;
}

uint32_t gekko_spr(const Gekko* c, int n) { return c->spr[n & 1023]; }

void gekko_set_spr(Gekko* c, int n, uint32_t v) {
    if (n >= 528 && n <= 543) c->bat_version++;
    c->spr[n & 1023] = v;
}

}  // extern "C"
