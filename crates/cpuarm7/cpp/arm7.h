// ARM7TDMI interpreter core (Game Boy Advance CPU): ARM and Thumb instruction sets,
// banked registers for every mode, IRQ exceptions, and SWI handed to the host.
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct Arm7Bus {
    void* ctx;
    // Addresses are aligned to `size` (1, 2 or 4); the CPU does ARM rotation itself.
    uint32_t (*read)(void* ctx, uint32_t addr, int size);
    void (*write)(void* ctx, uint32_t addr, uint32_t value, int size);
    // Software interrupt: return 1 if the host handled it (high-level BIOS), 0 to take the
    // exception to vector 0x08 as hardware does.
    int (*swi)(void* ctx, uint32_t number);
} Arm7Bus;

typedef struct Arm7 Arm7;

Arm7* arm7_new(Arm7Bus bus);
void arm7_free(Arm7* cpu);
// Resets to the given entry point in System mode with the GBA BIOS's stack layout.
void arm7_reset(Arm7* cpu, uint32_t entry);
// Executes one instruction (or takes a pending IRQ); returns approximate cycles.
uint32_t arm7_step(Arm7* cpu);
void arm7_set_irq(Arm7* cpu, int asserted);
// Current-mode register access (r15 = address of the next instruction to run).
uint32_t arm7_reg(const Arm7* cpu, int i);
void arm7_set_reg(Arm7* cpu, int i, uint32_t v);
uint32_t arm7_cpsr(const Arm7* cpu);
// Rewinds the PC to re-run the instruction just executed (used by waiting BIOS calls).
void arm7_repeat(Arm7* cpu);

// Save and restore every register and internal flag (not the host's callbacks), for save
// states and rewind. `state_size` bytes are written by save and read by load.
unsigned long arm7_state_size(void);
void arm7_save_state(const Arm7* cpu, uint8_t* out);
void arm7_load_state(Arm7* cpu, const uint8_t* in);

#ifdef __cplusplus
}
#endif
