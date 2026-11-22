// NEC VR4300 interpreter. See vr4300.h.
#include "vr4300.h"
#include <math.h>
#include <stdlib.h>
#include <string.h>
#ifdef _MSC_VER
#include <intrin.h>
#endif

namespace {

enum Exc : uint32_t { INT = 0, MOD = 1, TLBL = 2, TLBS = 3, ADEL = 4, ADES = 5, SYS = 8, BP = 9, RI = 10, CPU = 11, OV = 12, TR = 13, FPE = 15 };

inline uint64_t sx32(uint32_t v) { return (uint64_t)(int64_t)(int32_t)v; }

// 64x64 -> 128-bit products (MSVC has no __int128).
inline void mul_u128(uint64_t a, uint64_t b, uint64_t& hi, uint64_t& lo) {
#ifdef _MSC_VER
    lo = _umul128(a, b, &hi);
#else
    unsigned __int128 m = (unsigned __int128)a * b;
    lo = (uint64_t)m;
    hi = (uint64_t)(m >> 64);
#endif
}
inline void mul_s128(int64_t a, int64_t b, uint64_t& hi, uint64_t& lo) {
#ifdef _MSC_VER
    int64_t h;
    lo = (uint64_t)_mul128(a, b, &h);
    hi = (uint64_t)h;
#else
    __int128 m = (__int128)a * b;
    lo = (uint64_t)m;
    hi = (uint64_t)(m >> 64);
#endif
}

inline uint64_t ram_read(const uint8_t* p, int size) {
    switch (size) {
        case 1: return p[0];
        case 2: return (uint64_t)p[0] << 8 | p[1];
        case 4: return (uint64_t)p[0] << 24 | (uint64_t)p[1] << 16 | (uint64_t)p[2] << 8 | p[3];
        default: {
            uint64_t v = 0;
            for (int i = 0; i < 8; i++) v = v << 8 | p[i];
            return v;
        }
    }
}

inline void ram_write(uint8_t* p, uint64_t v, int size) {
    for (int i = size - 1; i >= 0; i--) {
        p[i] = (uint8_t)v;
        v >>= 8;
    }
}

struct TlbEntry {
    uint64_t mask, hi, lo0, lo1;
};

}  // namespace

struct Vr4300 {
    Vr4300Bus bus;
    uint64_t r[32];
    uint64_t hi, lo;
    uint64_t pc, next_pc;
    // delay_slot: the instruction now executing sits in a branch delay slot.
    bool delay_slot, in_delay;
    // Set by branch() for the instruction being executed.
    bool br, br_taken, br_likely;
    uint64_t br_target;
    uint64_t cop0[32];
    TlbEntry tlb[32];
    uint64_t fpr[32];
    uint32_t fcr31;
    uint32_t ip_lines;
    bool ll_bit;
    bool exception_taken;
    uint32_t cycles;
    uint32_t count_half;
    bool yield_now;

    // ---------------------------------------------------------------- exceptions

    void exception(uint32_t code, uint64_t badvaddr = 0, bool tlb_refill = false, uint32_t ce = 0) {
        uint64_t& status = cop0[12];
        uint64_t& cause = cop0[13];
        bool exl = status & 2;
        uint64_t cur = pc;
        if (!exl) {
            cop0[14] = delay_slot ? cur - 4 : cur;
            cause = delay_slot ? (cause | 0x80000000u) : (cause & ~0x80000000ull);
        }
        cause = (cause & ~0x3000007Cull) | (code << 2) | (ce << 28);
        if (code == TLBL || code == TLBS || code == MOD || code == ADEL || code == ADES) {
            cop0[8] = badvaddr;
            cop0[4] = (cop0[4] & ~0x7FFFF0ull) | ((badvaddr >> 9) & 0x7FFFF0);
            cop0[10] = (cop0[10] & 0xFF) | (badvaddr & ~0x1FFFull);
        }
        uint64_t base = (status & (1 << 22)) ? 0xFFFFFFFFBFC00200ull : 0xFFFFFFFF80000000ull;
        uint64_t off = (tlb_refill && !exl) ? 0x000 : 0x180;
        status |= 2;
        pc = base + off;
        next_pc = pc + 4;
        in_delay = false;
        exception_taken = true;
    }

    // ---------------------------------------------------------------- address translation

    bool translate(uint64_t va, bool write, uint32_t& pa) {
        uint32_t a = (uint32_t)va;
        if ((a & 0xC0000000u) == 0x80000000u) {
            pa = a & 0x1FFFFFFF;
            return true;
        }
        uint32_t asid = cop0[10] & 0xFF;
        for (int i = 0; i < 32; i++) {
            const TlbEntry& e = tlb[i];
            uint32_t pm = (uint32_t)e.mask & 0x01FFE000;
            uint32_t offmask = (pm >> 1) | 0xFFF;
            uint32_t vmask = ~(pm | 0x1FFF);
            if (((uint32_t)e.hi & vmask) != (a & vmask)) continue;
            bool global = (e.lo0 & e.lo1 & 1);
            if (!global && ((uint32_t)e.hi & 0xFF) != asid) continue;
            uint64_t lo = (a & (offmask + 1)) ? e.lo1 : e.lo0;
            if (!(lo & 2)) {
                exception(write ? TLBS : TLBL, va, false);
                return false;
            }
            if (write && !(lo & 4)) {
                exception(MOD, va, false);
                return false;
            }
            uint32_t pfn = (uint32_t)(lo >> 6) & 0xFFFFF;
            pa = ((pfn << 12) & ~offmask) | (a & offmask);
            return true;
        }
        exception(write ? TLBS : TLBL, va, true);
        return false;
    }

    bool load(uint64_t va, int size, uint64_t& v) {
        if (va & (size - 1)) {
            exception(ADEL, va);
            return false;
        }
        uint32_t pa;
        if (!translate(va, false, pa)) return false;
        v = (bus.ram && pa + size <= bus.ram_size) ? ram_read(bus.ram + pa, size) : bus.read(bus.ctx, pa, size);
        return true;
    }

    bool store(uint64_t va, int size, uint64_t v) {
        if (va & (size - 1)) {
            exception(ADES, va);
            return false;
        }
        uint32_t pa;
        if (!translate(va, true, pa)) return false;
        if (bus.ram && pa + size <= bus.ram_size)
            ram_write(bus.ram + pa, v, size);
        else
            bus.write(bus.ctx, pa, v, size);
        return true;
    }

    // ---------------------------------------------------------------- helpers

    void set(uint32_t i, uint64_t v) {
        if (i) r[i] = v;
    }

    void branch(bool cond, uint64_t target, bool likely, bool link = false, uint32_t link_reg = 31) {
        if (link) set(link_reg, pc + 8);
        br = true;
        br_taken = cond;
        br_likely = likely;
        br_target = target;
    }

    bool cop_usable(int n) {
        if (cop0[12] & (1u << (28 + n))) return true;
        exception(CPU, 0, false, n);
        return false;
    }

    // ---------------------------------------------------------------- FPU registers

    bool fr() const { return cop0[12] & (1u << 26); }

    uint32_t fs32(int n) const {
        if (fr()) return (uint32_t)fpr[n];
        return (n & 1) ? (uint32_t)(fpr[n & ~1] >> 32) : (uint32_t)fpr[n];
    }
    void fset32(int n, uint32_t v) {
        if (fr()) {
            fpr[n] = (fpr[n] & 0xFFFFFFFF00000000ull) | v;
        } else if (n & 1) {
            fpr[n & ~1] = (fpr[n & ~1] & 0xFFFFFFFFull) | ((uint64_t)v << 32);
        } else {
            fpr[n] = (fpr[n] & 0xFFFFFFFF00000000ull) | v;
        }
    }
    uint64_t fs64(int n) const { return fr() ? fpr[n] : fpr[n & ~1]; }
    void fset64(int n, uint64_t v) {
        if (fr())
            fpr[n] = v;
        else
            fpr[n & ~1] = v;
    }
    float fsf(int n) const {
        uint32_t u = fs32(n);
        float f;
        memcpy(&f, &u, 4);
        return f;
    }
    double fsd(int n) const {
        uint64_t u = fs64(n);
        double d;
        memcpy(&d, &u, 8);
        return d;
    }
    void fsetf(int n, float f) {
        uint32_t u;
        memcpy(&u, &f, 4);
        fset32(n, u);
    }
    void fsetd(int n, double d) {
        uint64_t u;
        memcpy(&u, &d, 8);
        fset64(n, u);
    }

    double round_mode(double v, int mode) const {
        switch (mode) {
            case 0: return nearbyint(v);
            case 1: return trunc(v);
            case 2: return ceil(v);
            default: return floor(v);
        }
    }

    // ---------------------------------------------------------------- execute

    void cop0_op(uint32_t op) {
        uint32_t rt = (op >> 16) & 31, rd = (op >> 11) & 31;
        switch ((op >> 21) & 31) {
            case 0x00: set(rt, sx32((uint32_t)read_cop0(rd))); return;
            case 0x01: set(rt, read_cop0(rd)); return;
            case 0x04: write_cop0(rd, sx32((uint32_t)r[rt])); return;
            case 0x05: write_cop0(rd, r[rt]); return;
            case 0x10:
                switch (op & 63) {
                    case 0x01: {  // TLBR
                        TlbEntry& e = tlb[cop0[0] & 31];
                        cop0[5] = e.mask;
                        cop0[10] = e.hi;
                        uint64_t g = e.lo0 & e.lo1 & 1;
                        cop0[2] = (e.lo0 & ~1ull) | g;
                        cop0[3] = (e.lo1 & ~1ull) | g;
                        return;
                    }
                    case 0x02: tlb_write(cop0[0] & 31); return;
                    case 0x06: tlb_write(random_index()); return;
                    case 0x08: {  // TLBP
                        cop0[0] = 0x80000000u;
                        for (int i = 0; i < 32; i++) {
                            uint32_t pm = (uint32_t)tlb[i].mask & 0x01FFE000;
                            uint32_t vmask = ~(pm | 0x1FFF);
                            bool g = tlb[i].lo0 & tlb[i].lo1 & 1;
                            if (((uint32_t)tlb[i].hi & vmask) == ((uint32_t)cop0[10] & vmask) && (g || (tlb[i].hi & 0xFF) == (cop0[10] & 0xFF))) {
                                cop0[0] = i;
                                break;
                            }
                        }
                        return;
                    }
                    case 0x18: {  // ERET
                        if (cop0[12] & 4) {
                            pc = cop0[30];
                            cop0[12] &= ~4ull;
                        } else {
                            pc = cop0[14];
                            cop0[12] &= ~2ull;
                        }
                        next_pc = pc + 4;
                        in_delay = false;
                        ll_bit = false;
                        exception_taken = true;  // PC already set
                        return;
                    }
                    default: return;
                }
            default: return;
        }
    }

    uint32_t random_index() {
        uint32_t wired = cop0[6] & 31;
        uint32_t span = 32 - wired;
        return wired + (cycles_total % (span ? span : 1));
    }
    uint64_t cycles_total = 0;

    void tlb_write(uint32_t i) {
        TlbEntry& e = tlb[i & 31];
        e.mask = cop0[5] & 0x01FFE000;
        e.hi = cop0[10] & ~(e.mask | 0x1F00ull);
        e.lo0 = cop0[2] & 0x3FFFFFFF;
        e.lo1 = cop0[3] & 0x3FFFFFFF;
    }

    uint64_t read_cop0(uint32_t rd) {
        if (rd == 1) return random_index();
        return cop0[rd];
    }

    void write_cop0(uint32_t rd, uint64_t v) {
        switch (rd) {
            case 9: cop0[9] = v & 0xFFFFFFFF; return;
            case 11:
                cop0[11] = v & 0xFFFFFFFF;
                cop0[13] &= ~0x8000ull;  // writing Compare clears the timer interrupt
                return;
            case 13: cop0[13] = (cop0[13] & ~0x300ull) | (v & 0x300); return;
            case 15: return;
            default: cop0[rd] = v; return;
        }
    }

    void cop1_op(uint32_t op) {
        if (!cop_usable(1)) return;
        uint32_t fmt = (op >> 21) & 31;
        uint32_t ft = (op >> 16) & 31, fs = (op >> 11) & 31, fd = (op >> 6) & 31;
        switch (fmt) {
            case 0x00: set(ft, sx32(fs32(fs))); return;
            case 0x01: set(ft, fs64(fs)); return;
            case 0x02: set(ft, sx32(fs == 31 ? fcr31 : (fs == 0 ? 0xA00 : 0))); return;
            case 0x04: fset32(fs, (uint32_t)r[ft]); return;
            case 0x05: fset64(fs, r[ft]); return;
            case 0x06:
                if (fs == 31) fcr31 = (uint32_t)r[ft] & 0x0183FFFF;
                return;
            case 0x08: {
                bool c = fcr31 & (1u << 23);
                bool want = (op >> 16) & 1, likely = (op >> 17) & 1;
                branch(c == want, pc + 4 + ((int64_t)(int16_t)op << 2), likely);
                return;
            }
            case 0x10: return fpu_s(op, fs, ft, fd);
            case 0x11: return fpu_d(op, fs, ft, fd);
            case 0x14:
            case 0x15: {
                // Integer sources (W, L)
                int64_t v = fmt == 0x14 ? (int64_t)(int32_t)fs32(fs) : (int64_t)fs64(fs);
                switch (op & 63) {
                    case 0x20: fsetf(fd, (float)v); return;
                    case 0x21: fsetd(fd, (double)v); return;
                    default: return;
                }
            }
            default: return;
        }
    }

    void compare(double a, double b, uint32_t cond) {
        bool un = isnan(a) || isnan(b);
        bool lt = !un && a < b, eq = !un && a == b;
        bool c = ((cond & 4) && lt) || ((cond & 2) && eq) || ((cond & 1) && un);
        fcr31 = c ? (fcr31 | (1u << 23)) : (fcr31 & ~(1u << 23));
    }

    void to_int(double v, uint32_t fd, int mode, bool wide) {
        double rv = round_mode(v, mode);
        if (wide)
            fset64(fd, (uint64_t)(int64_t)rv);
        else
            fset32(fd, (uint32_t)(int32_t)rv);
    }

    void fpu_s(uint32_t op, uint32_t fs, uint32_t ft, uint32_t fd) {
        float a = fsf(fs), b = fsf(ft);
        uint32_t f = op & 63;
        int rm = fcr31 & 3;
        switch (f) {
            case 0x00: fsetf(fd, a + b); return;
            case 0x01: fsetf(fd, a - b); return;
            case 0x02: fsetf(fd, a * b); return;
            case 0x03: fsetf(fd, a / b); return;
            case 0x04: fsetf(fd, sqrtf(a)); return;
            case 0x05: fsetf(fd, fabsf(a)); return;
            case 0x06: fset32(fd, fs32(fs)); return;
            case 0x07: fsetf(fd, -a); return;
            case 0x08: to_int(a, fd, 0, true); return;
            case 0x09: to_int(a, fd, 1, true); return;
            case 0x0A: to_int(a, fd, 2, true); return;
            case 0x0B: to_int(a, fd, 3, true); return;
            case 0x0C: to_int(a, fd, 0, false); return;
            case 0x0D: to_int(a, fd, 1, false); return;
            case 0x0E: to_int(a, fd, 2, false); return;
            case 0x0F: to_int(a, fd, 3, false); return;
            case 0x21: fsetd(fd, (double)a); return;
            case 0x24: to_int(a, fd, rm, false); return;
            case 0x25: to_int(a, fd, rm, true); return;
            default:
                if (f >= 0x30) compare(a, b, f & 7);
                return;
        }
    }

    void fpu_d(uint32_t op, uint32_t fs, uint32_t ft, uint32_t fd) {
        double a = fsd(fs), b = fsd(ft);
        uint32_t f = op & 63;
        int rm = fcr31 & 3;
        switch (f) {
            case 0x00: fsetd(fd, a + b); return;
            case 0x01: fsetd(fd, a - b); return;
            case 0x02: fsetd(fd, a * b); return;
            case 0x03: fsetd(fd, a / b); return;
            case 0x04: fsetd(fd, sqrt(a)); return;
            case 0x05: fsetd(fd, fabs(a)); return;
            case 0x06: fset64(fd, fs64(fs)); return;
            case 0x07: fsetd(fd, -a); return;
            case 0x08: to_int(a, fd, 0, true); return;
            case 0x09: to_int(a, fd, 1, true); return;
            case 0x0A: to_int(a, fd, 2, true); return;
            case 0x0B: to_int(a, fd, 3, true); return;
            case 0x0C: to_int(a, fd, 0, false); return;
            case 0x0D: to_int(a, fd, 1, false); return;
            case 0x0E: to_int(a, fd, 2, false); return;
            case 0x0F: to_int(a, fd, 3, false); return;
            case 0x20: fsetf(fd, (float)a); return;
            case 0x24: to_int(a, fd, rm, false); return;
            case 0x25: to_int(a, fd, rm, true); return;
            default:
                if (f >= 0x30) compare(a, b, f & 7);
                return;
        }
    }

    void special(uint32_t op) {
        uint32_t rs = (op >> 21) & 31, rt = (op >> 16) & 31, rd = (op >> 11) & 31, sa = (op >> 6) & 31;
        uint64_t s = r[rs], t = r[rt];
        switch (op & 63) {
            case 0x00: set(rd, sx32((uint32_t)t << sa)); return;
            case 0x02: set(rd, sx32((uint32_t)t >> sa)); return;
            case 0x03: set(rd, sx32((uint32_t)((int32_t)t >> sa))); return;
            case 0x04: set(rd, sx32((uint32_t)t << (s & 31))); return;
            case 0x06: set(rd, sx32((uint32_t)t >> (s & 31))); return;
            case 0x07: set(rd, sx32((uint32_t)((int32_t)t >> (s & 31)))); return;
            case 0x08: branch(true, s, false); return;
            case 0x09: {
                uint64_t target = s;
                set(rd, pc + 8);
                branch(true, target, false);
                return;
            }
            case 0x0C: exception(SYS); return;
            case 0x0D: exception(BP); return;
            case 0x0F: return;  // SYNC
            case 0x10: set(rd, hi); return;
            case 0x11: hi = s; return;
            case 0x12: set(rd, lo); return;
            case 0x13: lo = s; return;
            case 0x14: set(rd, t << (s & 63)); return;
            case 0x16: set(rd, t >> (s & 63)); return;
            case 0x17: set(rd, (uint64_t)((int64_t)t >> (s & 63))); return;
            case 0x18: {
                int64_t m = (int64_t)(int32_t)s * (int64_t)(int32_t)t;
                lo = sx32((uint32_t)m);
                hi = sx32((uint32_t)((uint64_t)m >> 32));
                cycles += 4;
                return;
            }
            case 0x19: {
                uint64_t m = (uint64_t)(uint32_t)s * (uint32_t)t;
                lo = sx32((uint32_t)m);
                hi = sx32((uint32_t)(m >> 32));
                cycles += 4;
                return;
            }
            case 0x1A: {
                int32_t a = (int32_t)s, b = (int32_t)t;
                if (b == 0) {
                    lo = a < 0 ? 1 : (uint64_t)-1;
                    hi = sx32((uint32_t)a);
                } else if (a == INT32_MIN && b == -1) {
                    lo = sx32((uint32_t)a);
                    hi = 0;
                } else {
                    lo = sx32((uint32_t)(a / b));
                    hi = sx32((uint32_t)(a % b));
                }
                cycles += 36;
                return;
            }
            case 0x1B: {
                uint32_t a = (uint32_t)s, b = (uint32_t)t;
                if (b == 0) {
                    lo = (uint64_t)-1;
                    hi = sx32(a);
                } else {
                    lo = sx32(a / b);
                    hi = sx32(a % b);
                }
                cycles += 36;
                return;
            }
            case 0x1C: mul_s128((int64_t)s, (int64_t)t, hi, lo); cycles += 7; return;
            case 0x1D: mul_u128(s, t, hi, lo); cycles += 7; return;
            case 0x1E: {
                int64_t a = (int64_t)s, b = (int64_t)t;
                if (b == 0) {
                    lo = a < 0 ? 1 : (uint64_t)-1;
                    hi = (uint64_t)a;
                } else if (a == INT64_MIN && b == -1) {
                    lo = (uint64_t)a;
                    hi = 0;
                } else {
                    lo = (uint64_t)(a / b);
                    hi = (uint64_t)(a % b);
                }
                cycles += 68;
                return;
            }
            case 0x1F: {
                if (t == 0) {
                    lo = (uint64_t)-1;
                    hi = s;
                } else {
                    lo = s / t;
                    hi = s % t;
                }
                cycles += 68;
                return;
            }
            case 0x20: {
                uint32_t res = (uint32_t)s + (uint32_t)t;
                if (~((uint32_t)s ^ (uint32_t)t) & ((uint32_t)s ^ res) & 0x80000000u) return exception(OV);
                set(rd, sx32(res));
                return;
            }
            case 0x21: set(rd, sx32((uint32_t)s + (uint32_t)t)); return;
            case 0x22: {
                uint32_t res = (uint32_t)s - (uint32_t)t;
                if (((uint32_t)s ^ (uint32_t)t) & ((uint32_t)s ^ res) & 0x80000000u) return exception(OV);
                set(rd, sx32(res));
                return;
            }
            case 0x23: set(rd, sx32((uint32_t)s - (uint32_t)t)); return;
            case 0x24: set(rd, s & t); return;
            case 0x25: set(rd, s | t); return;
            case 0x26: set(rd, s ^ t); return;
            case 0x27: set(rd, ~(s | t)); return;
            case 0x2A: set(rd, (int64_t)s < (int64_t)t); return;
            case 0x2B: set(rd, s < t); return;
            case 0x2C: {
                uint64_t res = s + t;
                if (~(s ^ t) & (s ^ res) & 0x8000000000000000ull) return exception(OV);
                set(rd, res);
                return;
            }
            case 0x2D: set(rd, s + t); return;
            case 0x2E: {
                uint64_t res = s - t;
                if ((s ^ t) & (s ^ res) & 0x8000000000000000ull) return exception(OV);
                set(rd, res);
                return;
            }
            case 0x2F: set(rd, s - t); return;
            case 0x30: if ((int64_t)s >= (int64_t)t) exception(TR); return;
            case 0x31: if (s >= t) exception(TR); return;
            case 0x32: if ((int64_t)s < (int64_t)t) exception(TR); return;
            case 0x33: if (s < t) exception(TR); return;
            case 0x34: if (s == t) exception(TR); return;
            case 0x36: if (s != t) exception(TR); return;
            case 0x38: set(rd, t << sa); return;
            case 0x3A: set(rd, t >> sa); return;
            case 0x3B: set(rd, (uint64_t)((int64_t)t >> sa)); return;
            case 0x3C: set(rd, t << (sa + 32)); return;
            case 0x3E: set(rd, t >> (sa + 32)); return;
            case 0x3F: set(rd, (uint64_t)((int64_t)t >> (sa + 32))); return;
            default: exception(RI); return;
        }
    }

    void execute(uint32_t op) {
        uint32_t rs = (op >> 21) & 31, rt = (op >> 16) & 31;
        uint64_t s = r[rs], t = r[rt];
        uint64_t imm_se = (uint64_t)(int64_t)(int16_t)op;
        uint64_t imm = op & 0xFFFF;
        uint64_t btarget = pc + 4 + (imm_se << 2);
        uint64_t ea = s + imm_se;
        uint64_t v;
        switch (op >> 26) {
            case 0x00: return special(op);
            case 0x01: {
                uint32_t k = rt;
                bool lt = (int64_t)s < 0;
                switch (k) {
                    case 0x00: return branch(lt, btarget, false);
                    case 0x01: return branch(!lt, btarget, false);
                    case 0x02: return branch(lt, btarget, true);
                    case 0x03: return branch(!lt, btarget, true);
                    case 0x08: if ((int64_t)s >= (int64_t)imm_se) exception(TR); return;
                    case 0x09: if (s >= imm_se) exception(TR); return;
                    case 0x0A: if ((int64_t)s < (int64_t)imm_se) exception(TR); return;
                    case 0x0B: if (s < imm_se) exception(TR); return;
                    case 0x0C: if (s == imm_se) exception(TR); return;
                    case 0x0E: if (s != imm_se) exception(TR); return;
                    case 0x10: return branch(lt, btarget, false, true);
                    case 0x11: return branch(!lt, btarget, false, true);
                    case 0x12: return branch(lt, btarget, true, true);
                    case 0x13: return branch(!lt, btarget, true, true);
                    default: return exception(RI);
                }
            }
            case 0x02: return branch(true, (pc & 0xFFFFFFFFF0000000ull) | ((op & 0x3FFFFFF) << 2), false);
            case 0x03: return branch(true, (pc & 0xFFFFFFFFF0000000ull) | ((op & 0x3FFFFFF) << 2), false, true);
            case 0x04: return branch(s == t, btarget, false);
            case 0x05: return branch(s != t, btarget, false);
            case 0x06: return branch((int64_t)s <= 0, btarget, false);
            case 0x07: return branch((int64_t)s > 0, btarget, false);
            case 0x08: {
                uint32_t res = (uint32_t)s + (uint32_t)imm_se;
                if (~((uint32_t)s ^ (uint32_t)imm_se) & ((uint32_t)s ^ res) & 0x80000000u) return exception(OV);
                return set(rt, sx32(res));
            }
            case 0x09: return set(rt, sx32((uint32_t)s + (uint32_t)imm_se));
            case 0x0A: return set(rt, (int64_t)s < (int64_t)imm_se);
            case 0x0B: return set(rt, s < imm_se);
            case 0x0C: return set(rt, s & imm);
            case 0x0D: return set(rt, s | imm);
            case 0x0E: return set(rt, s ^ imm);
            case 0x0F: return set(rt, sx32((uint32_t)imm << 16));
            case 0x10: return cop0_op(op);
            case 0x11: return cop1_op(op);
            case 0x12: return;  // COP2 does not exist on the N64 CPU
            case 0x14: return branch(s == t, btarget, true);
            case 0x15: return branch(s != t, btarget, true);
            case 0x16: return branch((int64_t)s <= 0, btarget, true);
            case 0x17: return branch((int64_t)s > 0, btarget, true);
            case 0x18: {
                uint64_t res = s + imm_se;
                if (~(s ^ imm_se) & (s ^ res) & 0x8000000000000000ull) return exception(OV);
                return set(rt, res);
            }
            case 0x19: return set(rt, s + imm_se);
            case 0x1A:
            case 0x1B: {  // LDL / LDR
                uint64_t w;
                if (!load(ea & ~7ull, 8, w)) return;
                uint32_t sh = (ea & 7) * 8;
                if (op >> 26 == 0x1A) {
                    uint64_t mask = sh ? ((1ull << sh) - 1) : 0;
                    set(rt, (t & mask) | (w << sh));
                } else {
                    uint32_t rsh = 56 - sh;
                    uint64_t keep = rsh == 0 ? 0 : ~(~0ull >> rsh);
                    set(rt, (t & keep) | (w >> rsh));
                }
                return;
            }
            case 0x20: if (load(ea, 1, v)) set(rt, (uint64_t)(int64_t)(int8_t)v); return;
            case 0x21: if (load(ea, 2, v)) set(rt, (uint64_t)(int64_t)(int16_t)v); return;
            case 0x22:
            case 0x26: {  // LWL / LWR
                uint64_t w;
                if (!load(ea & ~3ull, 4, w)) return;
                uint32_t sh = (ea & 3) * 8;
                uint32_t cur = (uint32_t)t, word = (uint32_t)w;
                uint32_t res;
                if (op >> 26 == 0x22) {
                    uint32_t mask = sh ? ((1u << sh) - 1) : 0;
                    res = (cur & mask) | (word << sh);
                } else {
                    uint32_t rsh = 24 - sh;
                    uint32_t keep = rsh == 0 ? 0 : ~(0xFFFFFFFFu >> rsh);
                    res = (cur & keep) | (word >> rsh);
                    if (rsh != 0) {
                        // LWR only sign-extends when it writes the whole word.
                        set(rt, (t & 0xFFFFFFFF00000000ull) | res);
                        return;
                    }
                }
                set(rt, sx32(res));
                return;
            }
            case 0x23: if (load(ea, 4, v)) set(rt, sx32((uint32_t)v)); return;
            case 0x24: if (load(ea, 1, v)) set(rt, v & 0xFF); return;
            case 0x25: if (load(ea, 2, v)) set(rt, v & 0xFFFF); return;
            case 0x27: if (load(ea, 4, v)) set(rt, v & 0xFFFFFFFF); return;
            case 0x28: store(ea, 1, t & 0xFF); return;
            case 0x29: store(ea, 2, t & 0xFFFF); return;
            case 0x2A:
            case 0x2E: {  // SWL / SWR
                uint64_t w;
                if (!load(ea & ~3ull, 4, w)) return;
                uint32_t sh = (ea & 3) * 8, word = (uint32_t)w, src = (uint32_t)t;
                uint32_t res;
                if (op >> 26 == 0x2A) {
                    uint32_t keep = sh ? ~(0xFFFFFFFFu >> sh) : 0;
                    res = (word & keep) | (src >> sh);
                } else {
                    uint32_t lsh = 24 - sh;
                    uint32_t keep = lsh ? ((1u << lsh) - 1) : 0;
                    res = (word & keep) | (src << lsh);
                }
                store(ea & ~3ull, 4, res);
                return;
            }
            case 0x2B: store(ea, 4, t & 0xFFFFFFFF); return;
            case 0x2C:
            case 0x2D: {  // SDL / SDR
                uint64_t w;
                if (!load(ea & ~7ull, 8, w)) return;
                uint32_t sh = (ea & 7) * 8;
                uint64_t res;
                if (op >> 26 == 0x2C) {
                    uint64_t keep = sh ? ~(~0ull >> sh) : 0;
                    res = (w & keep) | (t >> sh);
                } else {
                    uint32_t lsh = 56 - sh;
                    uint64_t keep = lsh ? ((1ull << lsh) - 1) : 0;
                    res = (w & keep) | (t << lsh);
                }
                store(ea & ~7ull, 8, res);
                return;
            }
            case 0x2F: return;  // CACHE
            case 0x30: if (load(ea, 4, v)) { set(rt, sx32((uint32_t)v)); ll_bit = true; } return;
            case 0x31: if (cop_usable(1) && load(ea, 4, v)) fset32(rt, (uint32_t)v); return;
            case 0x34: if (load(ea, 8, v)) { set(rt, v); ll_bit = true; } return;
            case 0x35: if (cop_usable(1) && load(ea, 8, v)) fset64(rt, v); return;
            case 0x37: if (load(ea, 8, v)) set(rt, v); return;
            case 0x38:
                if (ll_bit) {
                    if (!store(ea, 4, t & 0xFFFFFFFF)) return;
                }
                set(rt, ll_bit ? 1 : 0);
                return;
            case 0x39: if (cop_usable(1)) store(ea, 4, fs32(rt)); return;
            case 0x3C:
                if (ll_bit) {
                    if (!store(ea, 8, t)) return;
                }
                set(rt, ll_bit ? 1 : 0);
                return;
            case 0x3D: if (cop_usable(1)) store(ea, 8, fs64(rt)); return;
            case 0x3F: store(ea, 8, t); return;
            default: exception(RI); return;
        }
    }

    uint32_t step() {
        cycles = 1;
        cycles_total++;
        // Count ticks at half the CPU clock; matching Compare raises IP7.
        if (++count_half & 1) {
            uint32_t c = (uint32_t)cop0[9] + 1;
            cop0[9] = c;
            if (c == (uint32_t)cop0[11]) cop0[13] |= 0x8000;
        }
        cop0[13] = (cop0[13] & ~0x7C00ull) | ((uint64_t)(ip_lines & 31) << 10);
        exception_taken = false;
        delay_slot = in_delay;
        uint64_t status = cop0[12];
        if ((status & 1) && !(status & 6) && (cop0[13] & status & 0xFF00)) {
            exception(INT);
            return cycles;
        }
        if (pc & 3) {
            exception(ADEL, pc);
            return cycles;
        }
        uint32_t pa;
        if (!translate(pc, false, pa)) return cycles;
        uint32_t op = (bus.ram && pa + 4 <= bus.ram_size) ? (uint32_t)ram_read(bus.ram + pa, 4) : (uint32_t)bus.read(bus.ctx, pa, 4);
        br = false;
        execute(op);
        r[0] = 0;
        if (exception_taken) return cycles;
        uint64_t slot = next_pc;
        if (br && br_taken) {
            pc = slot;
            next_pc = br_target;
            in_delay = true;
        } else if (br && br_likely) {
            pc = slot + 4;  // likely branch not taken: its delay slot is skipped
            next_pc = pc + 4;
            in_delay = false;
        } else {
            pc = slot;
            next_pc = slot + 4;
            in_delay = br;
        }
        return cycles;
    }
};

extern "C" {

Vr4300* vr4300_new(Vr4300Bus bus) {
    Vr4300* c = (Vr4300*)calloc(1, sizeof(Vr4300));
    if (!c) return nullptr;
    c->bus = bus;
    vr4300_reset(c);
    return c;
}

void vr4300_free(Vr4300* cpu) { free(cpu); }

void vr4300_reset(Vr4300* c) {
    Vr4300Bus bus = c->bus;
    memset(c, 0, sizeof(Vr4300));
    c->bus = bus;
    c->pc = 0xFFFFFFFFBFC00000ull;
    c->next_pc = c->pc + 4;
    c->cop0[12] = 0x34000000;  // CU0, CU1, FR
    c->cop0[15] = 0x00000B22;  // PRId
    c->cop0[16] = 0x7006E463;  // Config
    c->cop0[6] = 0;
}

uint32_t vr4300_step(Vr4300* cpu) { return cpu->step(); }

uint32_t vr4300_run(Vr4300* c, uint32_t budget) {
    uint32_t used = 0;
    c->yield_now = false;
    while (used < budget && !c->yield_now) {
        uint64_t before = c->pc;
        uint32_t n = c->step();
        used += n;
        // A taken branch back to itself with an empty delay slot spins until an interrupt:
        // skip the remaining time in one go (Count still advances).
        if (c->in_delay && c->next_pc == before && used < budget) {
            uint32_t skip = budget - used;
            uint32_t slot;
            if (c->translate(c->pc, false, slot) && c->bus.ram && slot + 4 <= c->bus.ram_size && ram_read(c->bus.ram + slot, 4) == 0) {
                uint32_t cnt = (uint32_t)c->cop0[9], cmp = (uint32_t)c->cop0[11];
                uint32_t ticks = skip / 2;
                if (cmp - cnt - 1 < ticks) c->cop0[13] |= 0x8000;
                c->cop0[9] = (uint32_t)(cnt + ticks);
                used = budget;
            }
        }
    }
    return used;
}

void vr4300_yield(Vr4300* cpu) { cpu->yield_now = true; }

void vr4300_set_ip(Vr4300* cpu, uint32_t lines) { cpu->ip_lines = lines; }

void vr4300_get_regs(const Vr4300* c, Vr4300Regs* out) {
    memcpy(out->r, c->r, sizeof(c->r));
    out->hi = c->hi;
    out->lo = c->lo;
    out->pc = c->pc;
}

void vr4300_set_regs(Vr4300* c, const Vr4300Regs* in) {
    memcpy(c->r, in->r, sizeof(c->r));
    c->r[0] = 0;
    c->hi = in->hi;
    c->lo = in->lo;
    c->pc = in->pc;
    c->next_pc = in->pc + 4;
    c->in_delay = false;
}

uint64_t vr4300_cop0(const Vr4300* c, int reg) { return c->cop0[reg & 31]; }

void vr4300_set_cop0(Vr4300* c, int reg, uint64_t v) { c->cop0[reg & 31] = v; }

}  // extern "C"
