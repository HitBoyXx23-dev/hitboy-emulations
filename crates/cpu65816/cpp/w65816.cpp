#include "w65816.h"
#include <string.h>

namespace {
enum : uint8_t { FC = 1, FZ = 2, FI = 4, FD = 8, FX = 16, FM = 32, FV = 64, FN = 128 };
enum Mode { DP, DPX, DPY, IND, INDX, INDY, INDL, INDLY, ABS, ABSX, ABSY, LONG, LONGX, SR, SRY };

struct EA {
    uint32_t a;
    bool bank0;  // second byte wraps inside bank 0 (direct page / stack relative)
};
}  // namespace

struct W65816 {
    uint16_t A = 0, X = 0, Y = 0, S = 0x01FF, D = 0, PC = 0;
    uint8_t DB = 0, PB = 0, P = 0x34;
    bool E = true;
    bool nmi_pending = false, irq_line = false, waiting = false, stopped = false;
    void* ctx = nullptr;
    w65_read_fn rd = nullptr;
    w65_write_fn wr = nullptr;
    w65_idle_fn idl = nullptr;

    // ------------------------------------------------------------ bus
    inline uint8_t r8(uint32_t a) { return rd(ctx, a & 0xFFFFFF); }
    inline void w8(uint32_t a, uint8_t v) { wr(ctx, a & 0xFFFFFF, v); }
    inline void io() { idl(ctx); }
    inline uint8_t fetch() { uint8_t v = r8(uint32_t(PB) << 16 | PC); PC++; return v; }
    inline uint16_t fetch16() { uint16_t lo = fetch(); return lo | uint16_t(fetch()) << 8; }
    inline uint32_t fetch24() { uint32_t lo = fetch16(); return lo | uint32_t(fetch()) << 16; }

    inline bool m8() const { return P & FM; }
    inline bool x8() const { return P & FX; }
    inline void flag(uint8_t f, bool on) { P = on ? (P | f) : (P & ~f); }
    inline void zn(uint16_t v, bool w) {
        flag(FZ, (w ? v : (v & 0xFF)) == 0);
        flag(FN, w ? (v & 0x8000) : (v & 0x80));
    }

    // Stack: in emulation mode S stays in page 1.
    inline void push(uint8_t v) {
        w8(S, v);
        S = E ? uint16_t(0x100 | uint8_t(S - 1)) : uint16_t(S - 1);
    }
    inline uint8_t pull() {
        S = E ? uint16_t(0x100 | uint8_t(S + 1)) : uint16_t(S + 1);
        return r8(S);
    }
    inline void push16(uint16_t v) { push(v >> 8); push(uint8_t(v)); }
    inline uint16_t pull16() { uint16_t lo = pull(); return lo | uint16_t(pull()) << 8; }

    // Registers with the current width.
    inline void setA(uint16_t v) { if (m8()) A = (A & 0xFF00) | (v & 0xFF); else A = v; }
    inline void setX(uint16_t v) { X = x8() ? (v & 0xFF) : v; }
    inline void setY(uint16_t v) { Y = x8() ? (v & 0xFF) : v; }
    inline uint16_t getA() const { return m8() ? (A & 0xFF) : A; }

    void fix_flags() {
        if (E) {
            P |= FM | FX;
            S = 0x100 | (S & 0xFF);
        }
        if (P & FX) {
            X &= 0xFF;
            Y &= 0xFF;
        }
    }

    // ------------------------------------------------------------ addressing
    uint16_t dp_addr(uint8_t off, uint16_t index) {
        if (D & 0xFF) io();
        // Emulation mode with page-aligned D wraps inside the direct page.
        if (E && (D & 0xFF) == 0) return (D & 0xFF00) | uint8_t(off + index);
        return uint16_t(D + off + index);
    }
    uint16_t dp_ptr16(uint16_t p) {
        uint16_t lo = r8(p);
        uint16_t p1 = (E && (D & 0xFF) == 0) ? uint16_t((p & 0xFF00) | uint8_t(p + 1)) : uint16_t(p + 1);
        return lo | uint16_t(r8(p1)) << 8;
    }
    uint32_t dp_ptr24(uint16_t p) {
        uint32_t lo = r8(p);
        lo |= uint32_t(r8(uint16_t(p + 1))) << 8;
        return lo | uint32_t(r8(uint16_t(p + 2))) << 16;
    }

    EA ea(Mode m, bool write) {
        switch (m) {
            case DP: return {dp_addr(fetch(), 0), true};
            case DPX: { uint8_t o = fetch(); io(); return {dp_addr(o, X), true}; }
            case DPY: { uint8_t o = fetch(); io(); return {dp_addr(o, Y), true}; }
            case IND: return {uint32_t(DB) << 16 | dp_ptr16(dp_addr(fetch(), 0)), false};
            case INDX: { uint8_t o = fetch(); io(); return {uint32_t(DB) << 16 | dp_ptr16(dp_addr(o, X)), false}; }
            case INDY: {
                uint16_t ptr = dp_ptr16(dp_addr(fetch(), 0));
                uint32_t base = uint32_t(DB) << 16 | ptr;
                uint32_t a = (base + Y) & 0xFFFFFF;
                if (write || !x8() || ((base ^ a) & 0xFF00)) io();
                return {a, false};
            }
            case INDL: return {dp_ptr24(dp_addr(fetch(), 0)), false};
            case INDLY: return {(dp_ptr24(dp_addr(fetch(), 0)) + Y) & 0xFFFFFF, false};
            case ABS: return {uint32_t(DB) << 16 | fetch16(), false};
            case ABSX:
            case ABSY: {
                uint32_t base = uint32_t(DB) << 16 | fetch16();
                uint32_t a = (base + (m == ABSX ? X : Y)) & 0xFFFFFF;
                if (write || !x8() || ((base ^ a) & 0xFF00)) io();
                return {a, false};
            }
            case LONG: return {fetch24(), false};
            case LONGX: return {(fetch24() + X) & 0xFFFFFF, false};
            case SR: { uint8_t o = fetch(); io(); return {uint16_t(S + o), true}; }
            case SRY: {
                uint8_t o = fetch();
                io();
                uint16_t p = uint16_t(S + o);
                uint16_t ptr = r8(p) | uint16_t(r8(uint16_t(p + 1))) << 8;
                io();
                return {((uint32_t(DB) << 16 | ptr) + Y) & 0xFFFFFF, false};
            }
        }
        return {0, false};
    }

    inline uint32_t next(const EA& e) const { return e.bank0 ? uint16_t(e.a + 1) : ((e.a + 1) & 0xFFFFFF); }
    uint16_t load(const EA& e, bool w) {
        uint16_t lo = r8(e.a);
        return w ? uint16_t(lo | uint16_t(r8(next(e))) << 8) : lo;
    }
    void store(const EA& e, uint16_t v, bool w) {
        w8(e.a, uint8_t(v));
        if (w) w8(next(e), uint8_t(v >> 8));
    }
    uint16_t imm(bool w) { return w ? fetch16() : fetch(); }

    // ------------------------------------------------------------ ALU
    void adc(uint16_t v, bool w) {
        uint32_t a = getA(), c = P & FC, r;
        if (!(P & FD)) {
            r = a + v + c;
        } else if (!w) {
            r = (a & 0x0F) + (v & 0x0F) + c;
            if (r > 0x09) r += 0x06;
            c = r > 0x0F;
            r = (a & 0xF0) + (v & 0xF0) + (c << 4) + (r & 0x0F);
        } else {
            r = (a & 0x000F) + (v & 0x000F) + c;
            if (r > 0x0009) r += 0x0006;
            c = r > 0x000F;
            r = (a & 0x00F0) + (v & 0x00F0) + (c << 4) + (r & 0x000F);
            if (r > 0x009F) r += 0x0060;
            c = r > 0x00FF;
            r = (a & 0x0F00) + (v & 0x0F00) + (c << 8) + (r & 0x00FF);
            if (r > 0x09FF) r += 0x0600;
            c = r > 0x0FFF;
            r = (a & 0xF000) + (v & 0xF000) + (c << 12) + (r & 0x0FFF);
        }
        uint32_t sign = w ? 0x8000 : 0x80;
        flag(FV, (~(a ^ v) & (a ^ r) & sign) != 0);
        if (P & FD) {
            if (!w && r > 0x9F) r += 0x60;
            if (w && r > 0x9FFF) r += 0x6000;
        }
        flag(FC, r > (w ? 0xFFFFu : 0xFFu));
        setA(uint16_t(r));
        zn(uint16_t(r), w);
    }
    void sbc(uint16_t v, bool w) {
        v = uint16_t(~v) & (w ? 0xFFFF : 0xFF);
        uint32_t a = getA(), c = P & FC;
        int32_t r;
        if (!(P & FD)) {
            r = int32_t(a + v + c);
        } else if (!w) {
            r = (a & 0x0F) + (v & 0x0F) + c;
            if (r <= 0x0F) r -= 0x06;
            c = r > 0x0F;
            r = (a & 0xF0) + (v & 0xF0) + (c << 4) + (r & 0x0F);
        } else {
            r = (a & 0x000F) + (v & 0x000F) + c;
            if (r <= 0x000F) r -= 0x0006;
            c = r > 0x000F;
            r = (a & 0x00F0) + (v & 0x00F0) + (c << 4) + (r & 0x000F);
            if (r <= 0x00FF) r -= 0x0060;
            c = r > 0x00FF;
            r = (a & 0x0F00) + (v & 0x0F00) + (c << 8) + (r & 0x00FF);
            if (r <= 0x0FFF) r -= 0x0600;
            c = r > 0x0FFF;
            r = (a & 0xF000) + (v & 0xF000) + (c << 12) + (r & 0x0FFF);
        }
        uint32_t sign = w ? 0x8000 : 0x80;
        flag(FV, (~(a ^ v) & (a ^ uint32_t(r)) & sign) != 0);
        if (P & FD) {
            if (!w && r <= 0xFF) r -= 0x60;
            if (w && r <= 0xFFFF) r -= 0x6000;
        }
        flag(FC, r > int32_t(w ? 0xFFFF : 0xFF));
        setA(uint16_t(r));
        zn(uint16_t(r), w);
    }
    void cmp(uint16_t reg, uint16_t v, bool w) {
        uint32_t r = uint32_t(reg) - v;
        flag(FC, reg >= v);
        zn(uint16_t(r), w);
    }
    void bit(uint16_t v, bool w, bool immediate) {
        flag(FZ, (getA() & v) == 0);
        if (!immediate) {
            flag(FN, w ? (v & 0x8000) : (v & 0x80));
            flag(FV, w ? (v & 0x4000) : (v & 0x40));
        }
    }
    uint16_t asl(uint16_t v, bool w) { flag(FC, v & (w ? 0x8000 : 0x80)); v = uint16_t(v << 1); zn(v, w); return v; }
    uint16_t lsr(uint16_t v, bool w) { flag(FC, v & 1); v = uint16_t((w ? v : (v & 0xFF)) >> 1); zn(v, w); return v; }
    uint16_t rol(uint16_t v, bool w) { uint16_t c = P & FC; flag(FC, v & (w ? 0x8000 : 0x80)); v = uint16_t(v << 1 | c); zn(v, w); return v; }
    uint16_t ror(uint16_t v, bool w) {
        uint16_t c = (P & FC) ? (w ? 0x8000 : 0x80) : 0;
        flag(FC, v & 1);
        v = uint16_t((w ? v : (v & 0xFF)) >> 1 | c);
        zn(v, w);
        return v;
    }

    void rmw(Mode m, int op) {
        bool w = !m8();
        EA e = ea(m, true);
        uint16_t v = load(e, w);
        io();
        switch (op) {
            case 0: v = asl(v, w); break;
            case 1: v = rol(v, w); break;
            case 2: v = lsr(v, w); break;
            case 3: v = ror(v, w); break;
            case 4: v = uint16_t(v - 1); zn(v, w); break;
            case 5: v = uint16_t(v + 1); zn(v, w); break;
            case 6: flag(FZ, (getA() & v) == 0); v |= getA(); break;               // TSB
            default: flag(FZ, (getA() & v) == 0); v &= uint16_t(~getA()); break;  // TRB
        }
        store(e, v, w);
    }

    void branch(bool cond) {
        int8_t off = int8_t(fetch());
        if (!cond) return;
        io();
        uint16_t t = uint16_t(PC + off);
        if (E && ((t ^ PC) & 0xFF00)) io();
        PC = t;
    }

    void interrupt(uint16_t native_vec, uint16_t emu_vec, bool brk) {
        if (!E) push(PB);
        push16(PC);
        push(E ? uint8_t(brk ? (P | 0x10) : (P & ~0x10)) : P);
        P = uint8_t((P | FI) & ~FD);
        PB = 0;
        uint16_t v = E ? emu_vec : native_vec;
        PC = r8(v) | uint16_t(r8(v + 1)) << 8;
    }

    void step();
    void exec(uint8_t op);
};

void W65816::step() {
    if (stopped) { io(); return; }
    if (nmi_pending) {
        nmi_pending = false;
        waiting = false;
        io(); io();
        interrupt(0xFFEA, 0xFFFA, false);
        return;
    }
    if (irq_line && waiting) waiting = false;
    if (waiting) { io(); return; }
    if (irq_line && !(P & FI)) {
        io(); io();
        interrupt(0xFFEE, 0xFFFE, false);
        return;
    }
    exec(fetch());
}

void W65816::exec(uint8_t op) {
    // ---- the eight classic ALU groups share one addressing-mode layout
    {
        Mode mode = DP;
        bool known = true, immediate = false;
        switch (op & 0x1F) {
            case 0x01: mode = INDX; break;
            case 0x03: mode = SR; break;
            case 0x05: mode = DP; break;
            case 0x07: mode = INDL; break;
            case 0x09: immediate = true; break;
            case 0x0D: mode = ABS; break;
            case 0x0F: mode = LONG; break;
            case 0x11: mode = INDY; break;
            case 0x12: mode = IND; break;
            case 0x13: mode = SRY; break;
            case 0x15: mode = DPX; break;
            case 0x17: mode = INDLY; break;
            case 0x19: mode = ABSY; break;
            case 0x1D: mode = ABSX; break;
            case 0x1F: mode = LONGX; break;
            default: known = false; break;
        }
        if (known && op != 0x89) {
            int grp = op >> 5;
            bool w = !m8();
            if (grp == 4) {  // STA
                EA e = ea(mode, true);
                store(e, getA(), w);
                return;
            }
            uint16_t v;
            if (immediate) v = imm(w);
            else { EA e = ea(mode, false); v = load(e, w); }
            switch (grp) {
                case 0: setA(getA() | v); zn(getA(), w); break;
                case 1: setA(getA() & v); zn(getA(), w); break;
                case 2: setA(getA() ^ v); zn(getA(), w); break;
                case 3: adc(v, w); break;
                case 5: setA(v); zn(v, w); break;
                case 6: cmp(getA(), v, w); break;
                default: sbc(v, w); break;
            }
            return;
        }
    }

    const bool mw = !m8(), xw = !x8();
    switch (op) {
        // ---- shifts / inc / dec / test-and-set
        case 0x06: rmw(DP, 0); return;
        case 0x0E: rmw(ABS, 0); return;
        case 0x16: rmw(DPX, 0); return;
        case 0x1E: rmw(ABSX, 0); return;
        case 0x26: rmw(DP, 1); return;
        case 0x2E: rmw(ABS, 1); return;
        case 0x36: rmw(DPX, 1); return;
        case 0x3E: rmw(ABSX, 1); return;
        case 0x46: rmw(DP, 2); return;
        case 0x4E: rmw(ABS, 2); return;
        case 0x56: rmw(DPX, 2); return;
        case 0x5E: rmw(ABSX, 2); return;
        case 0x66: rmw(DP, 3); return;
        case 0x6E: rmw(ABS, 3); return;
        case 0x76: rmw(DPX, 3); return;
        case 0x7E: rmw(ABSX, 3); return;
        case 0xC6: rmw(DP, 4); return;
        case 0xCE: rmw(ABS, 4); return;
        case 0xD6: rmw(DPX, 4); return;
        case 0xDE: rmw(ABSX, 4); return;
        case 0xE6: rmw(DP, 5); return;
        case 0xEE: rmw(ABS, 5); return;
        case 0xF6: rmw(DPX, 5); return;
        case 0xFE: rmw(ABSX, 5); return;
        case 0x04: rmw(DP, 6); return;
        case 0x0C: rmw(ABS, 6); return;
        case 0x14: rmw(DP, 7); return;
        case 0x1C: rmw(ABS, 7); return;
        case 0x0A: io(); setA(asl(getA(), mw)); return;
        case 0x2A: io(); setA(rol(getA(), mw)); return;
        case 0x4A: io(); setA(lsr(getA(), mw)); return;
        case 0x6A: io(); setA(ror(getA(), mw)); return;
        case 0x1A: io(); setA(uint16_t(getA() + 1)); zn(getA(), mw); return;
        case 0x3A: io(); setA(uint16_t(getA() - 1)); zn(getA(), mw); return;

        // ---- BIT / STZ
        case 0x24: bit(load(ea(DP, false), mw), mw, false); return;
        case 0x2C: bit(load(ea(ABS, false), mw), mw, false); return;
        case 0x34: bit(load(ea(DPX, false), mw), mw, false); return;
        case 0x3C: bit(load(ea(ABSX, false), mw), mw, false); return;
        case 0x89: bit(imm(mw), mw, true); return;
        case 0x64: store(ea(DP, true), 0, mw); return;
        case 0x74: store(ea(DPX, true), 0, mw); return;
        case 0x9C: store(ea(ABS, true), 0, mw); return;
        case 0x9E: store(ea(ABSX, true), 0, mw); return;

        // ---- X / Y
        case 0xA2: setX(imm(xw)); zn(X, xw); return;
        case 0xA6: setX(load(ea(DP, false), xw)); zn(X, xw); return;
        case 0xAE: setX(load(ea(ABS, false), xw)); zn(X, xw); return;
        case 0xB6: setX(load(ea(DPY, false), xw)); zn(X, xw); return;
        case 0xBE: setX(load(ea(ABSY, false), xw)); zn(X, xw); return;
        case 0xA0: setY(imm(xw)); zn(Y, xw); return;
        case 0xA4: setY(load(ea(DP, false), xw)); zn(Y, xw); return;
        case 0xAC: setY(load(ea(ABS, false), xw)); zn(Y, xw); return;
        case 0xB4: setY(load(ea(DPX, false), xw)); zn(Y, xw); return;
        case 0xBC: setY(load(ea(ABSX, false), xw)); zn(Y, xw); return;
        case 0x86: store(ea(DP, true), X, xw); return;
        case 0x8E: store(ea(ABS, true), X, xw); return;
        case 0x96: store(ea(DPY, true), X, xw); return;
        case 0x84: store(ea(DP, true), Y, xw); return;
        case 0x8C: store(ea(ABS, true), Y, xw); return;
        case 0x94: store(ea(DPX, true), Y, xw); return;
        case 0xE0: cmp(X, imm(xw), xw); return;
        case 0xE4: cmp(X, load(ea(DP, false), xw), xw); return;
        case 0xEC: cmp(X, load(ea(ABS, false), xw), xw); return;
        case 0xC0: cmp(Y, imm(xw), xw); return;
        case 0xC4: cmp(Y, load(ea(DP, false), xw), xw); return;
        case 0xCC: cmp(Y, load(ea(ABS, false), xw), xw); return;

        // ---- branches
        case 0x10: branch(!(P & FN)); return;
        case 0x30: branch(P & FN); return;
        case 0x50: branch(!(P & FV)); return;
        case 0x70: branch(P & FV); return;
        case 0x90: branch(!(P & FC)); return;
        case 0xB0: branch(P & FC); return;
        case 0xD0: branch(!(P & FZ)); return;
        case 0xF0: branch(P & FZ); return;
        case 0x80: branch(true); return;
        case 0x82: { int16_t off = int16_t(fetch16()); io(); PC = uint16_t(PC + off); return; }

        // ---- jumps and calls
        case 0x4C: PC = fetch16(); return;
        case 0x5C: { uint32_t a = fetch24(); PC = uint16_t(a); PB = uint8_t(a >> 16); return; }
        case 0x6C: { uint16_t p = fetch16(); PC = r8(p) | uint16_t(r8(uint16_t(p + 1))) << 8; return; }
        case 0x7C: {
            uint16_t p = uint16_t(fetch16() + X);
            io();
            uint32_t b = uint32_t(PB) << 16;
            PC = r8(b | p) | uint16_t(r8(b | uint16_t(p + 1))) << 8;
            return;
        }
        case 0xDC: {
            uint16_t p = fetch16();
            uint16_t lo = r8(p) | uint16_t(r8(uint16_t(p + 1))) << 8;
            PB = r8(uint16_t(p + 2));
            PC = lo;
            return;
        }
        case 0x20: { uint16_t t = fetch16(); io(); push16(uint16_t(PC - 1)); PC = t; return; }
        case 0x22: {
            uint16_t t = fetch16();
            push(PB);
            io();
            uint8_t bank = fetch();
            push16(uint16_t(PC - 1));
            PB = bank;
            PC = t;
            return;
        }
        case 0xFC: {
            uint16_t lo = fetch();
            push16(PC);  // PC points at the high operand byte
            uint16_t p = uint16_t((lo | uint16_t(fetch()) << 8) + X);
            io();
            uint32_t b = uint32_t(PB) << 16;
            PC = r8(b | p) | uint16_t(r8(b | uint16_t(p + 1))) << 8;
            return;
        }
        case 0x60: io(); io(); PC = uint16_t(pull16() + 1); io(); return;
        case 0x6B: io(); io(); PC = uint16_t(pull16() + 1); PB = pull(); return;
        case 0x40: {
            io(); io();
            P = pull();
            if (E) P |= FM | FX;
            fix_flags();
            PC = pull16();
            if (!E) PB = pull();
            return;
        }
        case 0x00: fetch(); interrupt(0xFFE6, 0xFFFE, true); return;
        case 0x02: fetch(); interrupt(0xFFE4, 0xFFF4, false); return;

        // ---- stack
        case 0x48: io(); if (mw) push16(A); else push(uint8_t(A)); return;
        case 0x68: io(); io(); if (mw) A = pull16(); else A = (A & 0xFF00) | pull(); zn(getA(), mw); return;
        case 0xDA: io(); if (xw) push16(X); else push(uint8_t(X)); return;
        case 0xFA: io(); io(); X = xw ? pull16() : pull(); zn(X, xw); return;
        case 0x5A: io(); if (xw) push16(Y); else push(uint8_t(Y)); return;
        case 0x7A: io(); io(); Y = xw ? pull16() : pull(); zn(Y, xw); return;
        case 0x08: io(); push(P); return;
        case 0x28: io(); io(); P = pull(); fix_flags(); return;
        case 0x8B: io(); push(DB); return;
        case 0xAB: io(); io(); DB = pull(); zn(DB, false); return;
        case 0x0B: io(); push16(D); return;
        case 0x2B: io(); io(); D = pull16(); zn(D, true); return;
        case 0x4B: io(); push(PB); return;
        case 0xF4: push16(fetch16()); return;
        case 0xD4: push16(dp_ptr16(dp_addr(fetch(), 0))); return;
        case 0x62: { uint16_t off = fetch16(); io(); push16(uint16_t(PC + off)); return; }

        // ---- transfers
        case 0xAA: io(); setX(A); zn(X, xw); return;
        case 0xA8: io(); setY(A); zn(Y, xw); return;
        case 0x8A: io(); setA(X); zn(getA(), mw); return;
        case 0x98: io(); setA(Y); zn(getA(), mw); return;
        case 0x9B: io(); setY(X); zn(Y, xw); return;
        case 0xBB: io(); setX(Y); zn(X, xw); return;
        case 0xBA: io(); setX(S); zn(X, xw); return;
        case 0x9A: io(); S = E ? uint16_t(0x100 | (X & 0xFF)) : X; return;
        case 0x5B: io(); D = A; zn(D, true); return;
        case 0x7B: io(); A = D; zn(A, true); return;
        case 0x1B: io(); S = E ? uint16_t(0x100 | (A & 0xFF)) : A; return;
        case 0x3B: io(); A = S; zn(A, true); return;
        case 0xEB: io(); io(); A = uint16_t(A << 8 | A >> 8); zn(A & 0xFF, false); return;

        // ---- flags
        case 0x18: io(); P &= ~FC; return;
        case 0x38: io(); P |= FC; return;
        case 0x58: io(); P &= ~FI; return;
        case 0x78: io(); P |= FI; return;
        case 0xB8: io(); P &= ~FV; return;
        case 0xD8: io(); P &= ~FD; return;
        case 0xF8: io(); P |= FD; return;
        case 0xC2: { uint8_t v = fetch(); io(); P &= ~v; fix_flags(); return; }
        case 0xE2: { uint8_t v = fetch(); io(); P |= v; fix_flags(); return; }
        case 0xFB: {
            io();
            bool c = P & FC;
            flag(FC, E);
            E = c;
            fix_flags();
            return;
        }

        // ---- register inc / dec
        case 0xE8: io(); setX(uint16_t(X + 1)); zn(X, xw); return;
        case 0xC8: io(); setY(uint16_t(Y + 1)); zn(Y, xw); return;
        case 0xCA: io(); setX(uint16_t(X - 1)); zn(X, xw); return;
        case 0x88: io(); setY(uint16_t(Y - 1)); zn(Y, xw); return;

        // ---- block moves: operands are destination bank, then source bank
        case 0x54:
        case 0x44: {
            uint8_t dst = fetch(), src = fetch();
            DB = dst;
            uint8_t v = r8(uint32_t(src) << 16 | X);
            w8(uint32_t(dst) << 16 | Y, v);
            io(); io();
            int d = op == 0x54 ? 1 : -1;
            setX(uint16_t(X + d));
            setY(uint16_t(Y + d));
            A = uint16_t(A - 1);
            if (A != 0xFFFF) PC = uint16_t(PC - 3);
            return;
        }

        // ---- misc
        case 0xEA: io(); return;
        case 0x42: fetch(); return;  // WDM
        case 0xCB: io(); io(); waiting = true; return;
        case 0xDB: io(); io(); stopped = true; return;
        default: io(); return;
    }
}

extern "C" {

W65816* w65_new(void* ctx, w65_read_fn read, w65_write_fn write, w65_idle_fn idle) {
    W65816* c = new W65816();
    c->ctx = ctx; c->rd = read; c->wr = write; c->idl = idle;
    return c;
}
void w65_free(W65816* c) { delete c; }
void w65_reset(W65816* c) {
    c->E = true;
    c->P = 0x34;
    c->D = 0;
    c->DB = 0;
    c->PB = 0;
    c->S = 0x01FF;
    c->X &= 0xFF;
    c->Y &= 0xFF;
    c->waiting = c->stopped = c->nmi_pending = false;
    c->PC = c->r8(0xFFFC) | uint16_t(c->r8(0xFFFD)) << 8;
}
void w65_step(W65816* c) { c->step(); }
void w65_nmi(W65816* c) { c->nmi_pending = true; }
void w65_set_irq(W65816* c, int asserted) { c->irq_line = asserted != 0; }
void w65_get_regs(const W65816* c, W65Regs* o) {
    o->a = c->A; o->x = c->X; o->y = c->Y; o->s = c->S; o->d = c->D; o->pc = c->PC;
    o->db = c->DB; o->pb = c->PB; o->p = c->P; o->e = c->E; o->waiting = c->waiting; o->stopped = c->stopped;
}
void w65_set_regs(W65816* c, const W65Regs* i) {
    c->A = i->a; c->X = i->x; c->Y = i->y; c->S = i->s; c->D = i->d; c->PC = i->pc;
    c->DB = i->db; c->PB = i->pb; c->P = i->p; c->E = i->e != 0; c->waiting = i->waiting; c->stopped = i->stopped;
}


// ---- state ----
unsigned long w65_state_size(void) { return (unsigned long)sizeof(W65816); }
void w65_save_state(const W65816* c, uint8_t* out) {
    W65816 tmp = *c;
    tmp.ctx = {}; tmp.rd = {}; tmp.wr = {}; tmp.idl = {};
    memcpy(out, &tmp, sizeof(tmp));
}
void w65_load_state(W65816* c, const uint8_t* in) {
    W65816 keep = *c;
    memcpy((void*)c, in, sizeof(*c));
    c->ctx = keep.ctx;
    c->rd = keep.rd;
    c->wr = keep.wr;
    c->idl = keep.idl;
}

}  // extern "C"
