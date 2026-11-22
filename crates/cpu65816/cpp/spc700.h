// Sony SPC700 (SNES audio CPU) interpreter core.
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef uint8_t (*spc_read_fn)(void* ctx, uint16_t addr);
typedef void (*spc_write_fn)(void* ctx, uint16_t addr, uint8_t value);
typedef void (*spc_idle_fn)(void* ctx);

typedef struct Spc700 Spc700;

typedef struct SpcRegs {
    uint8_t a, x, y, sp, psw;
    uint16_t pc;
    uint8_t stopped;
} SpcRegs;

Spc700* spc_new(void* ctx, spc_read_fn read, spc_write_fn write, spc_idle_fn idle);
void spc_free(Spc700* cpu);
void spc_step(Spc700* cpu);
void spc_get_regs(const Spc700* cpu, SpcRegs* out);
void spc_set_regs(Spc700* cpu, const SpcRegs* in);

// Save and restore every register and internal flag (not the host's callbacks), for save
// states and rewind. `state_size` bytes are written by save and read by load.
unsigned long spc_state_size(void);
void spc_save_state(const Spc700* cpu, uint8_t* out);
void spc_load_state(Spc700* cpu, const uint8_t* in);

#ifdef __cplusplus
}
#endif
