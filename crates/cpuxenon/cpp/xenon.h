// IBM Xenon (Xbox 360 CPU) interpreter: one hardware thread of the 64-bit PowerPC core
// running user code. Privileged work (the kernel) is emulated at a high level by the host:
// `sc`, traps and unknown instructions stop the run loop and report an event.
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct XenonBus {
    void* ctx;
    // Guest effective addresses (32-bit), aligned to `size` (1, 2, 4 or 8); big-endian values.
    // Only called for pages missing from `pages`.
    uint64_t (*read)(void* ctx, uint32_t addr, int size);
    void (*write)(void* ctx, uint32_t addr, uint64_t value, int size);
    // 1M entries: host pointer for each 4 KB guest page, or null for the slow path.
    uint8_t** pages;
} XenonBus;

typedef struct Xenon Xenon;

typedef struct XenonRegs {
    uint64_t r[32];
    uint64_t lr, ctr;
    uint32_t cr, xer, pc, fpscr;
} XenonRegs;

enum { XENON_NONE = 0, XENON_SYSCALL = 1, XENON_TRAP = 2, XENON_INVALID = 3 };

Xenon* xenon_new(XenonBus bus);
void xenon_free(Xenon* cpu);
// Runs until `cycles` have elapsed or an event occurs; returns cycles used.
uint32_t xenon_run(Xenon* cpu, uint32_t cycles);
uint32_t xenon_step(Xenon* cpu);
// Returns and clears the last event (XENON_*); `pc` is left after `sc`, at a trap/invalid op.
int xenon_take_event(Xenon* cpu, uint32_t* opcode);
void xenon_get_regs(const Xenon* cpu, XenonRegs* out);
void xenon_set_regs(Xenon* cpu, const XenonRegs* in);
double xenon_fpr(const Xenon* cpu, int i);
void xenon_set_fpr(Xenon* cpu, int i, double v);
// Vector register i (0-127) as four big-endian-order words.
void xenon_vr(const Xenon* cpu, int i, uint32_t out[4]);
void xenon_set_vr(Xenon* cpu, int i, const uint32_t in[4]);
uint64_t xenon_timebase(const Xenon* cpu);
void xenon_set_timebase(Xenon* cpu, uint64_t tb);
// Whole architectural thread state (GPRs, FPRs, 128 vector registers, CR, XER, LR, CTR, FPSCR,
// VSCR, pc), used to switch between guest threads. `buf` must hold xenon_context_size() bytes.
uint32_t xenon_context_size(void);
void xenon_save_context(const Xenon* cpu, void* buf);
void xenon_load_context(Xenon* cpu, const void* buf);

#ifdef __cplusplus
}
#endif
