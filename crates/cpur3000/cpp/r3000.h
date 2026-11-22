// MIPS R3000A (PlayStation CPU) interpreter core: load/branch delay slots,
// COP0 exceptions, unaligned LWL/LWR/SWL/SWR, and COP2 (GTE) register access
// through host callbacks.
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct R3000Bus {
    void* ctx;
    uint32_t (*read)(void* ctx, uint32_t addr, int size);          // size: 1, 2, 4
    void (*write)(void* ctx, uint32_t addr, uint32_t value, int size);
    // COP2 (GTE): data/control register access and commands.
    uint32_t (*gte_read)(void* ctx, int reg);                       // 0-31 data, 32-63 control
    void (*gte_write)(void* ctx, int reg, uint32_t value);
    void (*gte_command)(void* ctx, uint32_t cmd);
} R3000Bus;

typedef struct R3000 R3000;

typedef struct R3000Regs {
    uint32_t r[32];
    uint32_t hi, lo, pc, next_pc;
    uint32_t sr, cause, epc, badvaddr;
} R3000Regs;

R3000* r3k_new(R3000Bus bus);
void r3k_free(R3000* cpu);
void r3k_reset(R3000* cpu);
// Gives the core direct access to 2 MB of main RAM (must stay at a fixed address).
void r3k_set_ram(R3000* cpu, uint8_t* ram);
// Executes one instruction; returns the number of cycles charged (approximate).
uint32_t r3k_step(R3000* cpu);
// Hardware interrupt line (Cause.IP2).
void r3k_set_irq(R3000* cpu, int asserted);
void r3k_get_regs(const R3000* cpu, R3000Regs* out);
void r3k_set_regs(R3000* cpu, const R3000Regs* in);

#ifdef __cplusplus
}
#endif
