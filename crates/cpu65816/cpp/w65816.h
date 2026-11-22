// WDC 65C816 (SNES main CPU, as in the Ricoh 5A22) interpreter core.
// The host supplies memory callbacks and an "idle" callback for internal
// cycles, so it can count master clocks per region like real hardware.
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef uint8_t (*w65_read_fn)(void* ctx, uint32_t addr);
typedef void (*w65_write_fn)(void* ctx, uint32_t addr, uint8_t value);
typedef void (*w65_idle_fn)(void* ctx);

typedef struct W65816 W65816;

typedef struct W65Regs {
    uint16_t a, x, y, s, d, pc;
    uint8_t db, pb, p, e;
    uint8_t waiting, stopped;
} W65Regs;

W65816* w65_new(void* ctx, w65_read_fn read, w65_write_fn write, w65_idle_fn idle);
void w65_free(W65816* cpu);
void w65_reset(W65816* cpu);
// Executes one instruction or services a pending interrupt.
void w65_step(W65816* cpu);
void w65_nmi(W65816* cpu);
void w65_set_irq(W65816* cpu, int asserted);
void w65_get_regs(const W65816* cpu, W65Regs* out);
void w65_set_regs(W65816* cpu, const W65Regs* in);

// Save and restore every register and internal flag (not the host's callbacks), for save
// states and rewind. `state_size` bytes are written by save and read by load.
unsigned long w65_state_size(void);
void w65_save_state(const W65816* cpu, uint8_t* out);
void w65_load_state(W65816* cpu, const uint8_t* in);

#ifdef __cplusplus
}
#endif
