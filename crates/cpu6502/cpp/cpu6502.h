// MOS 6502 interpreter core (decimal mode optional: off for the NES 2A03).
// Exposed through a plain C ABI so any host language can drive it.
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef uint8_t (*cpu_read_fn)(void* ctx, uint16_t addr);
typedef void (*cpu_write_fn)(void* ctx, uint16_t addr, uint8_t value);

typedef struct Cpu6502 Cpu6502;

typedef struct Cpu6502Regs {
    uint8_t a, x, y, sp, p;
    uint16_t pc;
} Cpu6502Regs;

Cpu6502* cpu6502_new(void* ctx, cpu_read_fn read, cpu_write_fn write);
void cpu6502_free(Cpu6502* cpu);
void cpu6502_reset(Cpu6502* cpu);
void cpu6502_nmi(Cpu6502* cpu);
// Enables NMOS decimal-mode ADC/SBC (the 6502/6507; the NES's 2A03 lacks it).
void cpu6502_set_decimal(Cpu6502* cpu, int enabled);
void cpu6502_set_irq(Cpu6502* cpu, int asserted);
// Executes one instruction (or services a pending interrupt); returns CPU cycles used.
uint32_t cpu6502_step(Cpu6502* cpu);
void cpu6502_get_regs(const Cpu6502* cpu, Cpu6502Regs* out);
void cpu6502_set_regs(Cpu6502* cpu, const Cpu6502Regs* in);

// Save and restore every register and internal flag (not the host's callbacks), for save
// states and rewind. `state_size` bytes are written by save and read by load.
unsigned long cpu6502_state_size(void);
void cpu6502_save_state(const Cpu6502* cpu, uint8_t* out);
void cpu6502_load_state(Cpu6502* cpu, const uint8_t* in);

#ifdef __cplusplus
}
#endif
