// ARM7TDMI interpreter. See arm7.h.
#include "arm7.h"
#include <stdlib.h>
#include <string.h>

namespace {

enum Mode : uint32_t { USR = 0x10, FIQ = 0x11, IRQ = 0x12, SVC = 0x13, ABT = 0x17, UND = 0x1B, SYS = 0x1F };
enum : uint32_t { FN = 1u << 31, FZ = 1u << 30, FC = 1u << 29, FV = 1u << 28, FI = 1u << 7, FT = 1u << 5 };

inline uint32_t ror32(uint32_t v, uint32_t n) {
    n &= 31;
    return n ? (v >> n) | (v << (32 - n)) : v;
}

int bank_of(uint32_t mode) {
    switch (mode & 0x1F) {
        case FIQ: return 1;
        case SVC: return 2;
        case ABT: return 3;
        case IRQ: return 4;
        case UND: return 5;
        default: return 0;
    }
}

}  // namespace

struct Arm7 {
    Arm7Bus bus;
    uint32_t r[16];
    uint32_t cpsr;
    uint32_t spsr[6];
    uint32_t hi_usr[5], hi_fiq[5];  // r8-r12
    uint32_t r13[6], r14[6];
    uint32_t pc;  // address of the next instruction to execute
    bool branched, repeat, irq_line;
    uint32_t cycles;

    bool thumb() const { return cpsr & FT; }
    uint32_t rd32(uint32_t a) { return bus.read(bus.ctx, a & ~3u, 4); }
    uint32_t rd16(uint32_t a) { return bus.read(bus.ctx, a & ~1u, 2); }
    uint32_t rd8(uint32_t a) { return bus.read(bus.ctx, a, 1); }
    void wr32(uint32_t a, uint32_t v) { bus.write(bus.ctx, a & ~3u, v, 4); }
    void wr16(uint32_t a, uint32_t v) { bus.write(bus.ctx, a & ~1u, v & 0xFFFF, 2); }
    void wr8(uint32_t a, uint32_t v) { bus.write(bus.ctx, a, v & 0xFF, 1); }

    // ---------------------------------------------------------------- modes

    void switch_mode(uint32_t nm) {
        uint32_t om = cpsr & 0x1F;
        int ob = bank_of(om), nb = bank_of(nm);
        if (ob != nb) {
            r13[ob] = r[13];
            r14[ob] = r[14];
            if ((ob == 1) != (nb == 1)) {
                uint32_t* save = ob == 1 ? hi_fiq : hi_usr;
                uint32_t* load = nb == 1 ? hi_fiq : hi_usr;
                for (int i = 0; i < 5; i++) {
                    save[i] = r[8 + i];
                    r[8 + i] = load[i];
                }
            }
            r[13] = r13[nb];
            r[14] = r14[nb];
        }
        cpsr = (cpsr & ~0x1Fu) | nm;
    }

    void set_cpsr(uint32_t v) {
        if ((v & 0x1F) != (cpsr & 0x1F)) switch_mode(v & 0x1F);
        cpsr = v;
    }

    uint32_t& cur_spsr() { return spsr[bank_of(cpsr)]; }

    void branch(uint32_t a) {
        pc = thumb() ? (a & ~1u) : (a & ~3u);
        branched = true;
    }

    void set_reg(int i, uint32_t v) {
        if (i == 15)
            branch(v);
        else
            r[i] = v;
    }

    void exception(uint32_t mode, uint32_t vector, uint32_t lr) {
        uint32_t old = cpsr;
        switch_mode(mode);
        spsr[bank_of(mode)] = old;
        r[14] = lr;
        cpsr = (cpsr & ~FT) | FI;
        pc = vector;
        branched = true;
    }

    // ---------------------------------------------------------------- flags

    bool cond(uint32_t c) const {
        bool n = cpsr & FN, z = cpsr & FZ, cy = cpsr & FC, v = cpsr & FV;
        switch (c) {
            case 0x0: return z;
            case 0x1: return !z;
            case 0x2: return cy;
            case 0x3: return !cy;
            case 0x4: return n;
            case 0x5: return !n;
            case 0x6: return v;
            case 0x7: return !v;
            case 0x8: return cy && !z;
            case 0x9: return !cy || z;
            case 0xA: return n == v;
            case 0xB: return n != v;
            case 0xC: return !z && n == v;
            case 0xD: return z || n != v;
            case 0xE: return true;
            default: return false;
        }
    }

    void nz(uint32_t v) { cpsr = (cpsr & ~(FN | FZ)) | (v & FN) | (v ? 0 : FZ); }
    void setc(bool c) { cpsr = c ? (cpsr | FC) : (cpsr & ~FC); }
    void setv(bool v) { cpsr = v ? (cpsr | FV) : (cpsr & ~FV); }

    uint32_t add(uint32_t a, uint32_t b, uint32_t c, bool s) {
        uint64_t w = (uint64_t)a + b + c;
        uint32_t r32 = (uint32_t)w;
        if (s) {
            nz(r32);
            setc(w >> 32);
            setv(~(a ^ b) & (a ^ r32) & 0x80000000u);
        }
        return r32;
    }
    // a - b - !c  (c = carry-in, 1 means no borrow)
    uint32_t sub(uint32_t a, uint32_t b, uint32_t c, bool s) {
        uint64_t w = (uint64_t)a - b - (1 - c);
        uint32_t r32 = (uint32_t)w;
        if (s) {
            nz(r32);
            setc(!(w >> 32));
            setv((a ^ b) & (a ^ r32) & 0x80000000u);
        }
        return r32;
    }

    // ---------------------------------------------------------------- shifter

    uint32_t shift(uint32_t type, uint32_t v, uint32_t n, bool& c, bool imm) {
        switch (type) {
            case 0:  // LSL
                if (n == 0) return v;
                if (n < 32) { c = (v >> (32 - n)) & 1; return v << n; }
                c = n == 32 ? (v & 1) : 0;
                return 0;
            case 1:  // LSR
                if (imm && n == 0) n = 32;
                if (n == 0) return v;
                if (n < 32) { c = (v >> (n - 1)) & 1; return v >> n; }
                c = n == 32 ? (v >> 31) : 0;
                return 0;
            case 2:  // ASR
                if (imm && n == 0) n = 32;
                if (n == 0) return v;
                if (n < 32) { c = (v >> (n - 1)) & 1; return (uint32_t)((int32_t)v >> n); }
                c = v >> 31;
                return (v >> 31) ? 0xFFFFFFFFu : 0;
            default:  // ROR / RRX
                if (imm && n == 0) {
                    bool out = v & 1;
                    uint32_t res = (v >> 1) | ((cpsr & FC) ? 0x80000000u : 0);
                    c = out;
                    return res;
                }
                if (n == 0) return v;
                n &= 31;
                if (n == 0) { c = v >> 31; return v; }
                c = (v >> (n - 1)) & 1;
                return ror32(v, n);
        }
    }

    // ---------------------------------------------------------------- ARM

    void arm(uint32_t op) {
        if (!cond(op >> 28)) return;
        uint32_t bits = (op >> 20) & 0xFF;
        if ((op & 0x0FFFFFF0) == 0x012FFF10) {  // BX
            uint32_t t = r[op & 15];
            cpsr = (t & 1) ? (cpsr | FT) : (cpsr & ~FT);
            branch(t);
            return;
        }
        switch (op >> 25 & 7) {
            case 0:
                if ((op & 0x90) == 0x90) {
                    if ((op & 0x60) == 0) {
                        if ((bits & 0xFC) == 0x00) return mul(op);
                        if ((bits & 0xF8) == 0x08) return mull(op);
                        if ((bits & 0xFB) == 0x10) return swp(op);
                    } else {
                        return halfword(op);
                    }
                }
                return dataproc(op);
            case 1: return dataproc(op);
            case 2:
            case 3:
                if ((op >> 25 & 1) && (op & 0x10)) return exception(UND, 0x04, pc + 4);
                return single(op);
            case 4: return block(op);
            case 5: {
                int32_t off = (int32_t)(op << 8) >> 6;
                if (op & (1u << 24)) r[14] = pc + 4;
                branch(pc + 8 + off);
                return;
            }
            case 6: return;  // coprocessor transfers: none on GBA
            default:
                if (op & (1u << 24)) return swi((op >> 16) & 0xFF);
                return;
        }
    }

    void swi(uint32_t n) {
        uint32_t len = thumb() ? 2 : 4;
        if (bus.swi(bus.ctx, n)) return;
        exception(SVC, 0x08, pc + len);
    }

    void dataproc(uint32_t op) {
        uint32_t opc = (op >> 21) & 15;
        bool s = op & (1u << 20);
        uint32_t rn = (op >> 16) & 15, rd = (op >> 12) & 15;
        bool c = cpsr & FC;
        uint32_t op2;
        bool regshift = false;
        if (op & (1u << 25)) {
            uint32_t rot = ((op >> 8) & 15) * 2;
            op2 = ror32(op & 0xFF, rot);
            if (rot) c = op2 >> 31;
        } else {
            uint32_t rm = op & 15;
            uint32_t type = (op >> 5) & 3;
            if (op & 0x10) {
                regshift = true;
                uint32_t amt = r[(op >> 8) & 15] & 0xFF;
                uint32_t v = rm == 15 ? pc + 12 : r[rm];
                op2 = shift(type, v, amt, c, false);
            } else {
                op2 = shift(type, r[rm], (op >> 7) & 31, c, true);
            }
        }
        if ((opc >> 2) == 2 && !s) return psr(op);  // MRS/MSR live in the TST..CMN space
        uint32_t a = rn == 15 ? (regshift ? pc + 12 : pc + 8) : r[rn];
        uint32_t res = 0;
        bool logical = false, write = true;
        uint32_t cin = (cpsr & FC) ? 1 : 0;
        bool sf = s && rd != 15;
        switch (opc) {
            case 0x0: res = a & op2; logical = true; break;
            case 0x1: res = a ^ op2; logical = true; break;
            case 0x2: res = sub(a, op2, 1, sf); break;
            case 0x3: res = sub(op2, a, 1, sf); break;
            case 0x4: res = add(a, op2, 0, sf); break;
            case 0x5: res = add(a, op2, cin, sf); break;
            case 0x6: res = sub(a, op2, cin, sf); break;
            case 0x7: res = sub(op2, a, cin, sf); break;
            case 0x8: res = a & op2; logical = true; write = false; break;
            case 0x9: res = a ^ op2; logical = true; write = false; break;
            case 0xA: res = sub(a, op2, 1, true); write = false; break;
            case 0xB: res = add(a, op2, 0, true); write = false; break;
            case 0xC: res = a | op2; logical = true; break;
            case 0xD: res = op2; logical = true; break;
            case 0xE: res = a & ~op2; logical = true; break;
            default: res = ~op2; logical = true; break;
        }
        if (logical && s) {
            nz(res);
            setc(c);
        }
        if (s && rd == 15) {
            // Returning from an exception: restore the saved status.
            uint32_t sp = cur_spsr();
            if ((cpsr & 0x1F) != USR && (cpsr & 0x1F) != SYS) set_cpsr(sp);
        }
        if (write) set_reg(rd, res);
    }

    void psr(uint32_t op) {
        bool spsr_sel = op & (1u << 22);
        if (!(op & (1u << 21))) {  // MRS
            r[(op >> 12) & 15] = spsr_sel ? cur_spsr() : cpsr;
            return;
        }
        uint32_t v = (op & (1u << 25)) ? ror32(op & 0xFF, ((op >> 8) & 15) * 2) : r[op & 15];
        uint32_t mask = 0;
        if (op & (1u << 19)) mask |= 0xFF000000u;
        if (op & (1u << 16)) mask |= 0x000000FFu;
        bool priv = (cpsr & 0x1F) != USR;
        if (spsr_sel) {
            if (bank_of(cpsr) != 0) cur_spsr() = (cur_spsr() & ~mask) | (v & mask);
            return;
        }
        if (!priv) mask &= 0xFF000000u;
        set_cpsr((cpsr & ~mask) | (v & mask));
    }

    void mul(uint32_t op) {
        uint32_t rd = (op >> 16) & 15, rn = (op >> 12) & 15, rs = (op >> 8) & 15, rm = op & 15;
        uint32_t res = r[rm] * r[rs];
        if (op & (1u << 21)) res += r[rn];
        r[rd] = res;
        if (op & (1u << 20)) nz(res);
        cycles += 2;
    }

    void mull(uint32_t op) {
        uint32_t hi = (op >> 16) & 15, lo = (op >> 12) & 15, rs = (op >> 8) & 15, rm = op & 15;
        uint64_t res;
        if (op & (1u << 22))
            res = (uint64_t)((int64_t)(int32_t)r[rm] * (int64_t)(int32_t)r[rs]);
        else
            res = (uint64_t)r[rm] * r[rs];
        if (op & (1u << 21)) res += ((uint64_t)r[hi] << 32) | r[lo];
        r[lo] = (uint32_t)res;
        r[hi] = (uint32_t)(res >> 32);
        if (op & (1u << 20)) {
            cpsr = (cpsr & ~(FN | FZ)) | ((res >> 63) ? FN : 0) | (res == 0 ? FZ : 0);
        }
        cycles += 3;
    }

    void swp(uint32_t op) {
        uint32_t rn = (op >> 16) & 15, rd = (op >> 12) & 15, rm = op & 15;
        uint32_t a = r[rn], src = r[rm];
        if (op & (1u << 22)) {
            uint32_t v = rd8(a);
            wr8(a, src);
            r[rd] = v;
        } else {
            uint32_t v = ror32(rd32(a), (a & 3) * 8);
            wr32(a, src);
            r[rd] = v;
        }
        cycles += 3;
    }

    void halfword(uint32_t op) {
        bool p = op & (1u << 24), u = op & (1u << 23), imm = op & (1u << 22), w = op & (1u << 21), l = op & (1u << 20);
        uint32_t rn = (op >> 16) & 15, rd = (op >> 12) & 15;
        uint32_t off = imm ? (((op >> 4) & 0xF0) | (op & 0xF)) : r[op & 15];
        uint32_t base = rn == 15 ? pc + 8 : r[rn];
        uint32_t addr = p ? (u ? base + off : base - off) : base;
        uint32_t sh = (op >> 5) & 3;
        uint32_t val = 0;
        if (l) {
            if (sh == 1) {
                val = ror32(rd16(addr), (addr & 1) * 8);
            } else if (sh == 2) {
                val = (uint32_t)(int32_t)(int8_t)rd8(addr);
            } else {
                val = (addr & 1) ? (uint32_t)(int32_t)(int8_t)rd8(addr) : (uint32_t)(int32_t)(int16_t)rd16(addr);
            }
        } else if (sh == 1) {
            wr16(addr, rd == 15 ? pc + 12 : r[rd]);
        }
        if (!p) addr = u ? base + off : base - off;
        if ((!p || w) && !(l && rn == rd)) set_reg(rn, addr);
        if (l) set_reg(rd, val);
        cycles += 2;
    }

    void single(uint32_t op) {
        bool reg = op & (1u << 25), p = op & (1u << 24), u = op & (1u << 23), b = op & (1u << 22), w = op & (1u << 21), l = op & (1u << 20);
        uint32_t rn = (op >> 16) & 15, rd = (op >> 12) & 15;
        uint32_t off;
        if (reg) {
            bool c = cpsr & FC;
            off = shift((op >> 5) & 3, r[op & 15], (op >> 7) & 31, c, true);
        } else {
            off = op & 0xFFF;
        }
        uint32_t base = rn == 15 ? pc + 8 : r[rn];
        uint32_t addr = p ? (u ? base + off : base - off) : base;
        uint32_t val = 0;
        if (l) {
            val = b ? rd8(addr) : ror32(rd32(addr), (addr & 3) * 8);
        } else {
            uint32_t v = rd == 15 ? pc + 12 : r[rd];
            if (b)
                wr8(addr, v);
            else
                wr32(addr, v);
        }
        if (!p) addr = u ? base + off : base - off;
        if ((!p || w) && !(l && rn == rd)) set_reg(rn, addr);
        if (l) set_reg(rd, val);
        cycles += 2;
    }

    void block(uint32_t op) {
        bool p = op & (1u << 24), u = op & (1u << 23), s = op & (1u << 22), w = op & (1u << 21), l = op & (1u << 20);
        uint32_t rn = (op >> 16) & 15;
        uint32_t list = op & 0xFFFF;
        uint32_t base = r[rn];
        int n = 0;
        for (int i = 0; i < 16; i++) n += (list >> i) & 1;
        bool empty = list == 0;
        if (empty) {
            list = 1u << 15;
            n = 16;
        }
        uint32_t start, end;
        if (u) {
            start = p ? base + 4 : base;
            end = base + 4 * n;
        } else {
            start = p ? base - 4 * n : base - 4 * n + 4;
            end = base - 4 * n;
        }
        // S bit without r15 in a load (or any store): access the User-mode registers.
        bool user = s && !(l && (list & 0x8000));
        uint32_t old_mode = cpsr & 0x1F;
        if (user) switch_mode(USR);
        uint32_t a = start;
        bool first = true;
        for (int i = 0; i < 16; i++) {
            if (!(list & (1u << i))) continue;
            if (l) {
                uint32_t v = rd32(a);
                if (i == 15) {
                    if (s) {
                        uint32_t sp = cur_spsr();
                        set_cpsr(sp);
                    }
                    branch(v);
                } else {
                    r[i] = v;
                }
            } else {
                uint32_t v = i == 15 ? pc + 12 : r[i];
                if (i == (int)rn && !first) v = end;
                wr32(a, v);
            }
            first = false;
            a += 4;
        }
        if (user) switch_mode(old_mode);
        if (w && !(l && (list & (1u << rn)))) r[rn] = end;
        cycles += n;
    }

    // ---------------------------------------------------------------- Thumb

    void thumb_op(uint32_t op) {
        uint32_t pcv = pc + 4;
        switch (op >> 12) {
            case 0x0:
            case 0x1: {
                uint32_t rd = op & 7, rs = (op >> 3) & 7;
                if ((op >> 11) == 3) {  // ADD/SUB
                    uint32_t v = (op & (1u << 10)) ? ((op >> 6) & 7) : r[(op >> 6) & 7];
                    r[rd] = (op & (1u << 9)) ? sub(r[rs], v, 1, true) : add(r[rs], v, 0, true);
                } else {
                    bool c = cpsr & FC;
                    uint32_t res = shift((op >> 11) & 3, r[rs], (op >> 6) & 31, c, true);
                    r[rd] = res;
                    nz(res);
                    setc(c);
                }
                return;
            }
            case 0x2:
            case 0x3: {
                uint32_t rd = (op >> 8) & 7, imm = op & 0xFF;
                switch ((op >> 11) & 3) {
                    case 0: r[rd] = imm; nz(imm); break;
                    case 1: sub(r[rd], imm, 1, true); break;
                    case 2: r[rd] = add(r[rd], imm, 0, true); break;
                    default: r[rd] = sub(r[rd], imm, 1, true); break;
                }
                return;
            }
            case 0x4: {
                if ((op >> 10) == 0x10) return thumb_alu(op);
                if ((op >> 10) == 0x11) {  // hi register ops / BX
                    uint32_t rd = (op & 7) | ((op >> 4) & 8), rs = (op >> 3) & 15;
                    uint32_t sv = rs == 15 ? pcv : r[rs];
                    uint32_t dv = rd == 15 ? pcv : r[rd];
                    switch ((op >> 8) & 3) {
                        case 0: set_reg(rd, dv + sv); break;
                        case 1: sub(dv, sv, 1, true); break;
                        case 2: set_reg(rd, sv); break;
                        default:
                            cpsr = (sv & 1) ? (cpsr | FT) : (cpsr & ~FT);
                            branch(sv);
                            break;
                    }
                    return;
                }
                // LDR PC-relative
                r[(op >> 8) & 7] = rd32((pcv & ~2u) + (op & 0xFF) * 4);
                cycles += 2;
                return;
            }
            case 0x5: {
                uint32_t rd = op & 7, a = r[(op >> 3) & 7] + r[(op >> 6) & 7];
                switch ((op >> 9) & 7) {
                    case 0: wr32(a, r[rd]); break;
                    case 1: wr16(a, r[rd]); break;
                    case 2: wr8(a, r[rd]); break;
                    case 3: r[rd] = (uint32_t)(int32_t)(int8_t)rd8(a); break;
                    case 4: r[rd] = ror32(rd32(a), (a & 3) * 8); break;
                    case 5: r[rd] = ror32(rd16(a), (a & 1) * 8); break;
                    case 6: r[rd] = rd8(a); break;
                    default: r[rd] = (a & 1) ? (uint32_t)(int32_t)(int8_t)rd8(a) : (uint32_t)(int32_t)(int16_t)rd16(a); break;
                }
                cycles += 2;
                return;
            }
            case 0x6:
            case 0x7: {
                uint32_t rd = op & 7, base = r[(op >> 3) & 7], imm = (op >> 6) & 31;
                bool byte = op & (1u << 12), load = op & (1u << 11);
                uint32_t a = base + (byte ? imm : imm * 4);
                if (load)
                    r[rd] = byte ? rd8(a) : ror32(rd32(a), (a & 3) * 8);
                else if (byte)
                    wr8(a, r[rd]);
                else
                    wr32(a, r[rd]);
                cycles += 2;
                return;
            }
            case 0x8: {
                uint32_t rd = op & 7, a = r[(op >> 3) & 7] + ((op >> 6) & 31) * 2;
                if (op & (1u << 11))
                    r[rd] = ror32(rd16(a), (a & 1) * 8);
                else
                    wr16(a, r[rd]);
                cycles += 2;
                return;
            }
            case 0x9: {
                uint32_t rd = (op >> 8) & 7, a = r[13] + (op & 0xFF) * 4;
                if (op & (1u << 11))
                    r[rd] = ror32(rd32(a), (a & 3) * 8);
                else
                    wr32(a, r[rd]);
                cycles += 2;
                return;
            }
            case 0xA: {
                uint32_t rd = (op >> 8) & 7;
                r[rd] = ((op & (1u << 11)) ? r[13] : (pcv & ~2u)) + (op & 0xFF) * 4;
                return;
            }
            case 0xB: {
                if ((op & 0xFF00) == 0xB000) {
                    uint32_t v = (op & 0x7F) * 4;
                    r[13] = (op & 0x80) ? r[13] - v : r[13] + v;
                    return;
                }
                if ((op & 0x0600) == 0x0400) {  // PUSH/POP
                    bool pop = op & (1u << 11), extra = op & (1u << 8);
                    uint32_t list = op & 0xFF;
                    if (pop) {
                        uint32_t a = r[13];
                        for (int i = 0; i < 8; i++)
                            if (list & (1u << i)) { r[i] = rd32(a); a += 4; }
                        if (extra) { branch(rd32(a)); a += 4; }
                        r[13] = a;
                    } else {
                        int n = extra;
                        for (int i = 0; i < 8; i++) n += (list >> i) & 1;
                        uint32_t a = r[13] - 4 * n;
                        r[13] = a;
                        for (int i = 0; i < 8; i++)
                            if (list & (1u << i)) { wr32(a, r[i]); a += 4; }
                        if (extra) wr32(a, r[14]);
                    }
                    cycles += 2;
                }
                return;
            }
            case 0xC: {  // STMIA/LDMIA
                uint32_t rb = (op >> 8) & 7, list = op & 0xFF, a = r[rb];
                bool load = op & (1u << 11);
                if (list == 0) {
                    if (load) branch(rd32(a)); else wr32(a, pc + 6);
                    r[rb] = a + 0x40;
                    return;
                }
                int n = 0;
                for (int i = 0; i < 8; i++) n += (list >> i) & 1;
                uint32_t end = a + 4 * n;
                bool first = true;
                for (int i = 0; i < 8; i++) {
                    if (!(list & (1u << i))) continue;
                    if (load) r[i] = rd32(a);
                    else wr32(a, (i == (int)rb && !first) ? end : r[i]);
                    first = false;
                    a += 4;
                }
                if (!(load && (list & (1u << rb)))) r[rb] = end;
                cycles += n;
                return;
            }
            case 0xD: {
                uint32_t c = (op >> 8) & 15;
                if (c == 15) return swi(op & 0xFF);
                if (c == 14) return;
                if (cond(c)) branch(pcv + ((int32_t)(int8_t)(op & 0xFF) << 1));
                return;
            }
            case 0xE:
                if (!(op & 0x800)) branch(pcv + ((int32_t)(op << 21) >> 20));
                return;
            default: {
                if (!(op & 0x800)) {
                    r[14] = pcv + ((int32_t)(op << 21) >> 9);
                } else {
                    uint32_t next = pc + 2;
                    uint32_t target = r[14] + ((op & 0x7FF) << 1);
                    r[14] = next | 1;
                    branch(target);
                }
                return;
            }
        }
    }

    void thumb_alu(uint32_t op) {
        uint32_t rd = op & 7, rs = (op >> 3) & 7;
        uint32_t a = r[rd], b = r[rs];
        bool c = cpsr & FC;
        uint32_t cin = c ? 1 : 0;
        switch ((op >> 6) & 15) {
            case 0x0: r[rd] = a & b; nz(r[rd]); break;
            case 0x1: r[rd] = a ^ b; nz(r[rd]); break;
            case 0x2: r[rd] = shift(0, a, b & 0xFF, c, false); nz(r[rd]); setc(c); break;
            case 0x3: r[rd] = shift(1, a, b & 0xFF, c, false); nz(r[rd]); setc(c); break;
            case 0x4: r[rd] = shift(2, a, b & 0xFF, c, false); nz(r[rd]); setc(c); break;
            case 0x5: r[rd] = add(a, b, cin, true); break;
            case 0x6: r[rd] = sub(a, b, cin, true); break;
            case 0x7: r[rd] = shift(3, a, b & 0xFF, c, false); nz(r[rd]); setc(c); break;
            case 0x8: nz(a & b); break;
            case 0x9: r[rd] = sub(0, b, 1, true); break;
            case 0xA: sub(a, b, 1, true); break;
            case 0xB: add(a, b, 0, true); break;
            case 0xC: r[rd] = a | b; nz(r[rd]); break;
            case 0xD: r[rd] = a * b; nz(r[rd]); cycles += 2; break;
            case 0xE: r[rd] = a & ~b; nz(r[rd]); break;
            default: r[rd] = ~b; nz(r[rd]); break;
        }
    }

    // ---------------------------------------------------------------- step

    uint32_t step() {
        cycles = 1;
        if (irq_line && !(cpsr & FI)) {
            exception(IRQ, 0x18, pc + 4);
            return 3;
        }
        branched = false;
        repeat = false;
        uint32_t here = pc;
        if (thumb()) {
            uint32_t op = rd16(pc);
            r[15] = pc + 4;
            thumb_op(op);
            if (!branched) pc = repeat ? here : here + 2;
        } else {
            uint32_t op = rd32(pc);
            r[15] = pc + 8;
            arm(op);
            if (!branched) pc = repeat ? here : here + 4;
        }
        r[15] = pc;
        return cycles;
    }
};

extern "C" {

Arm7* arm7_new(Arm7Bus bus) {
    Arm7* c = (Arm7*)calloc(1, sizeof(Arm7));
    if (!c) return nullptr;
    c->bus = bus;
    arm7_reset(c, 0x08000000);
    return c;
}

void arm7_free(Arm7* cpu) { free(cpu); }

void arm7_reset(Arm7* c, uint32_t entry) {
    Arm7Bus bus = c->bus;
    memset(c, 0, sizeof(Arm7));
    c->bus = bus;
    c->cpsr = SYS;
    // Stack pointers as left by the GBA BIOS.
    c->r13[0] = 0x03007F00;
    c->r13[4] = 0x03007FA0;
    c->r13[2] = 0x03007FE0;
    c->r[13] = 0x03007F00;
    c->pc = entry;
    c->r[15] = entry;
}

uint32_t arm7_step(Arm7* cpu) { return cpu->step(); }

void arm7_set_irq(Arm7* cpu, int asserted) { cpu->irq_line = asserted != 0; }

uint32_t arm7_reg(const Arm7* cpu, int i) { return i == 15 ? cpu->pc : cpu->r[i & 15]; }

void arm7_set_reg(Arm7* cpu, int i, uint32_t v) {
    if (i == 15) {
        cpu->pc = v;
        cpu->r[15] = v;
    } else {
        cpu->r[i & 15] = v;
    }
}

uint32_t arm7_cpsr(const Arm7* cpu) { return cpu->cpsr; }

void arm7_repeat(Arm7* cpu) { cpu->repeat = true; }


// ---- state ----
unsigned long arm7_state_size(void) { return (unsigned long)sizeof(Arm7); }
void arm7_save_state(const Arm7* c, uint8_t* out) {
    Arm7 tmp = *c;
    tmp.bus = {};
    memcpy(out, &tmp, sizeof(tmp));
}
void arm7_load_state(Arm7* c, const uint8_t* in) {
    Arm7 keep = *c;
    memcpy((void*)c, in, sizeof(*c));
    c->bus = keep.bus;
}

}  // extern "C"
