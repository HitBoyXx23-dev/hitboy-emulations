#include <string.h>
#include "r3000.h"

namespace {
enum Exc : uint32_t {
    INT = 0x00,
    ADEL = 0x04,  // address error on load / fetch
    ADES = 0x05,  // address error on store
    SYS = 0x08,
    BP = 0x09,
    RI = 0x0A,    // reserved instruction
    CPU = 0x0B,   // coprocessor unusable
    OVF = 0x0C,
};
}

struct R3000 {
    R3000Bus bus;
    uint32_t regs[32] = {};
    uint32_t out[32] = {};   // register file after this instruction
    uint32_t hi = 0, lo = 0;
    uint32_t pc = 0xBFC00000, next_pc = 0xBFC00004, cur_pc = 0;
    // Pending load (visible to the instruction after the next one).
    uint32_t ld_reg = 0, ld_val = 0;
    bool branch = false, delay_slot = false;
    // COP0
    uint32_t sr = 0, cause = 0, epc = 0, badvaddr = 0;
    uint32_t cycles = 0;

    inline void set(uint32_t r, uint32_t v) { out[r] = v; out[0] = 0; }

    // Main RAM (2 MB, mirrored over the first 8 MB) is reached directly, without a callback.
    uint8_t* ram = nullptr;
    static inline uint32_t phys(uint32_t a) {
        static const uint32_t masks[8] = {~0u, ~0u, ~0u, ~0u, 0x7FFFFFFFu, 0x1FFFFFFFu, ~0u, ~0u};
        return a & masks[a >> 29];
    }
    inline uint32_t rd(uint32_t a, int size) {
        uint32_t p = phys(a);
        if (ram && p < 0x800000) {
            const uint8_t* m = ram + (p & 0x1FFFFF);
            if (size == 4) { uint32_t v; memcpy(&v, m, 4); return v; }
            if (size == 2) { uint16_t v; memcpy(&v, m, 2); return v; }
            return *m;
        }
        return bus.read(bus.ctx, a, size);
    }
    inline void wr(uint32_t a, uint32_t v, int size) {
        if (sr & 0x10000) return;  // cache isolated: writes hit the cache, not memory
        uint32_t p = phys(a);
        if (ram && p < 0x800000) {
            uint8_t* m = ram + (p & 0x1FFFFF);
            if (size == 4) memcpy(m, &v, 4);
            else if (size == 2) { uint16_t h = (uint16_t)v; memcpy(m, &h, 2); }
            else *m = (uint8_t)v;
            return;
        }
        bus.write(bus.ctx, a, v, size);
    }

    void exception(uint32_t code) {
        uint32_t handler = (sr & (1u << 22)) ? 0xBFC00180 : 0x80000080;
        // Push the interrupt-enable / kernel-mode stack.
        uint32_t mode = sr & 0x3F;
        sr = (sr & ~0x3Fu) | ((mode << 2) & 0x3F);
        cause = (cause & ~0x7Cu) | (code << 2);
        if (delay_slot) {
            epc = cur_pc - 4;
            cause |= 1u << 31;
        } else {
            epc = cur_pc;
            cause &= ~(1u << 31);
        }
        pc = handler;
        next_pc = handler + 4;
    }

    inline void jump(uint32_t target) {
        next_pc = target;
        branch = true;
    }
    inline void branch_rel(uint32_t imm_se) {
        next_pc = pc + (imm_se << 2);
        branch = true;
    }

    uint32_t step();
    void exec(uint32_t ins);
    void cop0(uint32_t ins);
    void cop2(uint32_t ins);
};

uint32_t R3000::step() {
    cycles = 1;
    cur_pc = pc;

    // Hardware interrupt: IEc set and a pending, unmasked cause bit.
    if ((sr & 1) && (sr & cause & 0xFF00)) {
        delay_slot = branch;
        branch = false;
        exception(INT);
        // The pending load still completes.
        set(ld_reg, ld_val);
        ld_reg = ld_val = 0;
        for (int i = 0; i < 32; i++) regs[i] = out[i];
        return cycles;
    }

    if (cur_pc & 3) {
        badvaddr = cur_pc;
        delay_slot = branch;
        branch = false;
        exception(ADEL);
        return cycles;
    }

    uint32_t ins = rd(cur_pc, 4);
    pc = next_pc;
    next_pc = pc + 4;

    // Retire the load from the previous instruction.
    set(ld_reg, ld_val);
    ld_reg = ld_val = 0;

    delay_slot = branch;
    branch = false;
    exec(ins);

    for (int i = 0; i < 32; i++) regs[i] = out[i];
    return cycles;
}

void R3000::exec(uint32_t ins) {
    const uint32_t op = ins >> 26;
    const uint32_t rs = (ins >> 21) & 31, rt = (ins >> 16) & 31, rdn = (ins >> 11) & 31;
    const uint32_t sa = (ins >> 6) & 31, funct = ins & 63;
    const uint32_t imm = ins & 0xFFFF;
    const uint32_t imm_se = uint32_t(int32_t(int16_t(imm)));
    const uint32_t s = regs[rs], t = regs[rt];

    auto load = [&](uint32_t r, uint32_t v) {
        // A load cancels an older pending load to the same register.
        if (ld_reg == r) ld_reg = 0;
        ld_reg = r;
        ld_val = v;
        if (r == 0) { ld_reg = 0; ld_val = 0; }
    };

    switch (op) {
        case 0x00:  // SPECIAL
            switch (funct) {
                case 0x00: set(rdn, t << sa); return;
                case 0x02: set(rdn, t >> sa); return;
                case 0x03: set(rdn, uint32_t(int32_t(t) >> sa)); return;
                case 0x04: set(rdn, t << (s & 31)); return;
                case 0x06: set(rdn, t >> (s & 31)); return;
                case 0x07: set(rdn, uint32_t(int32_t(t) >> (s & 31))); return;
                case 0x08: jump(s); return;
                case 0x09: set(rdn, pc + 4); jump(s); return;
                case 0x0C: exception(SYS); return;
                case 0x0D: exception(BP); return;
                case 0x10: set(rdn, hi); return;
                case 0x11: hi = s; return;
                case 0x12: set(rdn, lo); return;
                case 0x13: lo = s; return;
                case 0x18: {
                    int64_t p = int64_t(int32_t(s)) * int64_t(int32_t(t));
                    lo = uint32_t(p); hi = uint32_t(uint64_t(p) >> 32);
                    cycles += 6;
                    return;
                }
                case 0x19: {
                    uint64_t p = uint64_t(s) * uint64_t(t);
                    lo = uint32_t(p); hi = uint32_t(p >> 32);
                    cycles += 6;
                    return;
                }
                case 0x1A: {  // DIV (with the hardware's results for /0 and overflow)
                    int32_t n = int32_t(s), d = int32_t(t);
                    if (d == 0) { hi = uint32_t(n); lo = n >= 0 ? 0xFFFFFFFF : 1; }
                    else if (uint32_t(n) == 0x80000000 && d == -1) { hi = 0; lo = 0x80000000; }
                    else { lo = uint32_t(n / d); hi = uint32_t(n % d); }
                    cycles += 35;
                    return;
                }
                case 0x1B: {
                    if (t == 0) { hi = s; lo = 0xFFFFFFFF; }
                    else { lo = s / t; hi = s % t; }
                    cycles += 35;
                    return;
                }
                case 0x20: {
                    uint32_t r = s + t;
                    if (~(s ^ t) & (s ^ r) & 0x80000000u) { exception(OVF); return; }
                    set(rdn, r);
                    return;
                }
                case 0x21: set(rdn, s + t); return;
                case 0x22: {
                    uint32_t r = s - t;
                    if ((s ^ t) & (s ^ r) & 0x80000000u) { exception(OVF); return; }
                    set(rdn, r);
                    return;
                }
                case 0x23: set(rdn, s - t); return;
                case 0x24: set(rdn, s & t); return;
                case 0x25: set(rdn, s | t); return;
                case 0x26: set(rdn, s ^ t); return;
                case 0x27: set(rdn, ~(s | t)); return;
                case 0x2A: set(rdn, int32_t(s) < int32_t(t)); return;
                case 0x2B: set(rdn, s < t); return;
                default: exception(RI); return;
            }
        case 0x01: {  // REGIMM: BLTZ / BGEZ / BLTZAL / BGEZAL
            bool ge = (rt & 1) != 0;
            bool link = (rt & 0x1E) == 0x10;
            bool take = ge ? int32_t(s) >= 0 : int32_t(s) < 0;
            if (link) set(31, pc + 4);
            if (take) branch_rel(imm_se);
            return;
        }
        case 0x02: jump((pc & 0xF0000000) | ((ins & 0x3FFFFFF) << 2)); return;
        case 0x03: set(31, pc + 4); jump((pc & 0xF0000000) | ((ins & 0x3FFFFFF) << 2)); return;
        case 0x04: if (s == t) branch_rel(imm_se); return;
        case 0x05: if (s != t) branch_rel(imm_se); return;
        case 0x06: if (int32_t(s) <= 0) branch_rel(imm_se); return;
        case 0x07: if (int32_t(s) > 0) branch_rel(imm_se); return;
        case 0x08: {
            uint32_t r = s + imm_se;
            if (~(s ^ imm_se) & (s ^ r) & 0x80000000u) { exception(OVF); return; }
            set(rt, r);
            return;
        }
        case 0x09: set(rt, s + imm_se); return;
        case 0x0A: set(rt, int32_t(s) < int32_t(imm_se)); return;
        case 0x0B: set(rt, s < imm_se); return;
        case 0x0C: set(rt, s & imm); return;
        case 0x0D: set(rt, s | imm); return;
        case 0x0E: set(rt, s ^ imm); return;
        case 0x0F: set(rt, imm << 16); return;
        case 0x10: cop0(ins); return;
        case 0x11: case 0x13: exception(CPU); return;  // COP1/COP3 don't exist
        case 0x12: cop2(ins); return;

        // ---- loads (delayed)
        case 0x20: case 0x21: case 0x23: case 0x24: case 0x25: {
            uint32_t a = s + imm_se;
            int size = (op == 0x20 || op == 0x24) ? 1 : (op == 0x21 || op == 0x25) ? 2 : 4;
            if (a & (size - 1)) { badvaddr = a; exception(ADEL); return; }
            if (sr & 0x10000) { load(rt, 0); return; }
            uint32_t v = rd(a, size);
            switch (op) {
                case 0x20: v = uint32_t(int32_t(int8_t(v))); break;
                case 0x21: v = uint32_t(int32_t(int16_t(v))); break;
                case 0x24: v &= 0xFF; break;
                case 0x25: v &= 0xFFFF; break;
            }
            load(rt, v);
            cycles += 1;
            return;
        }
        case 0x22: case 0x26: {  // LWL / LWR merge with the pending load of the same register
            uint32_t a = s + imm_se;
            uint32_t cur = (ld_reg == rt && rt != 0) ? ld_val : out[rt];
            uint32_t w = rd(a & ~3u, 4);
            uint32_t v;
            if (op == 0x22) {
                switch (a & 3) {
                    case 0: v = (cur & 0x00FFFFFF) | (w << 24); break;
                    case 1: v = (cur & 0x0000FFFF) | (w << 16); break;
                    case 2: v = (cur & 0x000000FF) | (w << 8); break;
                    default: v = w; break;
                }
            } else {
                switch (a & 3) {
                    case 0: v = w; break;
                    case 1: v = (cur & 0xFF000000) | (w >> 8); break;
                    case 2: v = (cur & 0xFFFF0000) | (w >> 16); break;
                    default: v = (cur & 0xFFFFFF00) | (w >> 24); break;
                }
            }
            load(rt, v);
            return;
        }

        // ---- stores
        case 0x28: case 0x29: case 0x2B: {
            uint32_t a = s + imm_se;
            int size = op == 0x28 ? 1 : op == 0x29 ? 2 : 4;
            if (a & (size - 1)) { badvaddr = a; exception(ADES); return; }
            wr(a, size == 4 ? t : size == 2 ? (t & 0xFFFF) : (t & 0xFF), size);
            return;
        }
        case 0x2A: case 0x2E: {  // SWL / SWR
            uint32_t a = s + imm_se;
            uint32_t aligned = a & ~3u;
            uint32_t m = rd(aligned, 4);
            uint32_t v;
            if (op == 0x2A) {
                switch (a & 3) {
                    case 0: v = (m & 0xFFFFFF00) | (t >> 24); break;
                    case 1: v = (m & 0xFFFF0000) | (t >> 16); break;
                    case 2: v = (m & 0xFF000000) | (t >> 8); break;
                    default: v = t; break;
                }
            } else {
                switch (a & 3) {
                    case 0: v = t; break;
                    case 1: v = (m & 0x000000FF) | (t << 8); break;
                    case 2: v = (m & 0x0000FFFF) | (t << 16); break;
                    default: v = (m & 0x00FFFFFF) | (t << 24); break;
                }
            }
            wr(aligned, v, 4);
            return;
        }

        // ---- GTE loads / stores
        case 0x32: {  // LWC2
            uint32_t a = s + imm_se;
            if (a & 3) { badvaddr = a; exception(ADEL); return; }
            bus.gte_write(bus.ctx, int(rt), rd(a, 4));
            return;
        }
        case 0x3A: {  // SWC2
            uint32_t a = s + imm_se;
            if (a & 3) { badvaddr = a; exception(ADES); return; }
            wr(a, bus.gte_read(bus.ctx, int(rt)), 4);
            return;
        }
        default: exception(RI); return;
    }
}

void R3000::cop0(uint32_t ins) {
    uint32_t rs = (ins >> 21) & 31, rt = (ins >> 16) & 31, rdn = (ins >> 11) & 31;
    switch (rs) {
        case 0x00: {  // MFC0 (delayed like a load)
            uint32_t v;
            switch (rdn) {
                case 8: v = badvaddr; break;
                case 12: v = sr; break;
                case 13: v = cause; break;
                case 14: v = epc; break;
                case 15: v = 0x00000002; break;  // PRId
                default: v = 0; break;
            }
            ld_reg = rt; ld_val = v;
            if (rt == 0) ld_reg = ld_val = 0;
            return;
        }
        case 0x04: {  // MTC0
            uint32_t v = regs[rt];
            switch (rdn) {
                case 12: sr = v; break;
                case 13: cause = (cause & ~0x300u) | (v & 0x300); break;  // only software interrupt bits
                default: break;
            }
            return;
        }
        case 0x10:
            if ((ins & 63) == 0x10) {  // RFE: pop the mode stack
                uint32_t mode = sr & 0x3F;
                sr = (sr & ~0xFu) | (mode >> 2);
            } else {
                exception(RI);
            }
            return;
        default: exception(RI); return;
    }
}

void R3000::cop2(uint32_t ins) {
    uint32_t rs = (ins >> 21) & 31, rt = (ins >> 16) & 31, rdn = (ins >> 11) & 31;
    if (ins & (1u << 25)) {  // GTE command
        bus.gte_command(bus.ctx, ins & 0x1FFFFFF);
        return;
    }
    switch (rs) {
        case 0x00: ld_reg = rt; ld_val = bus.gte_read(bus.ctx, int(rdn)); break;        // MFC2
        case 0x02: ld_reg = rt; ld_val = bus.gte_read(bus.ctx, int(rdn) + 32); break;   // CFC2
        case 0x04: bus.gte_write(bus.ctx, int(rdn), regs[rt]); break;                    // MTC2
        case 0x06: bus.gte_write(bus.ctx, int(rdn) + 32, regs[rt]); break;               // CTC2
        default: exception(RI); break;
    }
    if (ld_reg == 0) ld_val = 0;
}

extern "C" {

R3000* r3k_new(R3000Bus bus) {
    R3000* c = new R3000();
    c->bus = bus;
    return c;
}
void r3k_free(R3000* c) { delete c; }
void r3k_set_ram(R3000* c, uint8_t* ram) { c->ram = ram; }
void r3k_reset(R3000* c) {
    for (int i = 0; i < 32; i++) c->regs[i] = c->out[i] = 0;
    c->pc = 0xBFC00000;
    c->next_pc = 0xBFC00004;
    c->sr = 0x00400000;  // BEV set: exceptions vector into the BIOS
    c->cause = 0;
    c->ld_reg = c->ld_val = 0;
    c->branch = c->delay_slot = false;
}
uint32_t r3k_step(R3000* c) { return c->step(); }
void r3k_set_irq(R3000* c, int asserted) {
    if (asserted) c->cause |= 0x400; else c->cause &= ~0x400u;
}
void r3k_get_regs(const R3000* c, R3000Regs* o) {
    for (int i = 0; i < 32; i++) o->r[i] = c->regs[i];
    o->hi = c->hi; o->lo = c->lo; o->pc = c->pc; o->next_pc = c->next_pc;
    o->sr = c->sr; o->cause = c->cause; o->epc = c->epc; o->badvaddr = c->badvaddr;
}
void r3k_set_regs(R3000* c, const R3000Regs* i) {
    for (int k = 0; k < 32; k++) c->regs[k] = c->out[k] = i->r[k];
    c->regs[0] = c->out[0] = 0;
    c->hi = i->hi; c->lo = i->lo; c->pc = i->pc; c->next_pc = i->next_pc;
    c->sr = i->sr; c->cause = i->cause; c->epc = i->epc; c->badvaddr = i->badvaddr;
}

// ---- state ----
unsigned long r3k_state_size(void) { return (unsigned long)sizeof(R3000); }
void r3k_save_state(const R3000* c, uint8_t* out) {
    R3000 tmp = *c;
    tmp.bus = {};
    memcpy(out, (const void*)&tmp, sizeof(tmp));
}
void r3k_load_state(R3000* c, const uint8_t* in) {
    R3000Bus keep = c->bus;
    memcpy((void*)c, in, sizeof(*c));
    c->bus = keep;
}

}  // extern "C"
