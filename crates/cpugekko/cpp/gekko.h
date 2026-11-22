// IBM Gekko (GameCube CPU) interpreter: 32-bit PowerPC 750 integer and floating-point units,
// paired-single SIMD with quantised loads/stores (GQRs), BAT address translation, and the
// decrementer, external-interrupt and system-call exceptions.
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct GekkoBus {
    void* ctx;
    // Physical addresses, aligned to `size` (1, 2, 4 or 8 bytes); big-endian values.
    uint64_t (*read)(void* ctx, uint32_t paddr, int size);
    void (*write)(void* ctx, uint32_t paddr, uint64_t value, int size);
    // Optional fast path: main RAM at physical address 0 (may be null).
    uint8_t* ram;
    uint32_t ram_size;
} GekkoBus;

typedef struct Gekko Gekko;

typedef struct GekkoRegs {
    uint32_t r[32];
    uint32_t pc, lr, ctr, cr, xer, msr;
} GekkoRegs;

Gekko* gekko_new(GekkoBus bus);
void gekko_free(Gekko* cpu);
void gekko_reset(Gekko* cpu);
void gekko_set_tb_divisor(Gekko* cpu, uint32_t divisor);
void gekko_set_tb_multiplier(Gekko* cpu, uint32_t multiplier);
// Runs until at least `cycles` have elapsed; returns cycles used.
uint32_t gekko_run(Gekko* cpu, uint32_t cycles);
uint32_t gekko_step(Gekko* cpu);
// External interrupt line (Flipper's PI interrupt).
void gekko_set_irq(Gekko* cpu, int asserted);
void gekko_get_regs(const Gekko* cpu, GekkoRegs* out);
void gekko_set_regs(Gekko* cpu, const GekkoRegs* in);
double gekko_fpr(const Gekko* cpu, int i, int ps1);
void gekko_set_fpr(Gekko* cpu, int i, double ps0, double ps1);
uint32_t gekko_spr(const Gekko* cpu, int n);
void gekko_set_spr(Gekko* cpu, int n, uint32_t v);

#ifdef __cplusplus
}
#endif
