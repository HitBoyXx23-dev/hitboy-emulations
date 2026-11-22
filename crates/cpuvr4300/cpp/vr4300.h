// NEC VR4300 (Nintendo 64 CPU) interpreter: 64-bit MIPS III with branch delay slots,
// likely branches, COP0 (exceptions, 32-entry TLB, Count/Compare timer) and the COP1 FPU.
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct Vr4300Bus {
    void* ctx;
    // Physical addresses, aligned to `size` (1, 2, 4 or 8 bytes).
    uint64_t (*read)(void* ctx, uint32_t paddr, int size);
    void (*write)(void* ctx, uint32_t paddr, uint64_t value, int size);
    // Optional fast path: big-endian main RAM mapped at physical address 0 (may be null).
    uint8_t* ram;
    uint32_t ram_size;
} Vr4300Bus;

typedef struct Vr4300 Vr4300;

typedef struct Vr4300Regs {
    uint64_t r[32];
    uint64_t hi, lo, pc;
} Vr4300Regs;

Vr4300* vr4300_new(Vr4300Bus bus);
void vr4300_free(Vr4300* cpu);
void vr4300_reset(Vr4300* cpu);
// Executes one instruction (or takes a pending interrupt); returns cycles used.
uint32_t vr4300_step(Vr4300* cpu);
// Runs instructions until at least `cycles` have elapsed (or vr4300_yield is called from a bus
// callback); returns the cycles actually used. A branch to itself (an idle loop) consumes the
// rest of the budget at once.
uint32_t vr4300_run(Vr4300* cpu, uint32_t cycles);
void vr4300_yield(Vr4300* cpu);
// External interrupt lines IP2..IP6 (bit 0 = IP2, the RCP's MI interrupt).
void vr4300_set_ip(Vr4300* cpu, uint32_t lines);
void vr4300_get_regs(const Vr4300* cpu, Vr4300Regs* out);
// Sets general registers and jumps to `pc` (used by the boot code that stands in for the PIF).
void vr4300_set_regs(Vr4300* cpu, const Vr4300Regs* in);
uint64_t vr4300_cop0(const Vr4300* cpu, int reg);
void vr4300_set_cop0(Vr4300* cpu, int reg, uint64_t v);

#ifdef __cplusplus
}
#endif
