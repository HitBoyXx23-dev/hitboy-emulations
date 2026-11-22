// Sharp SM83 (Game Boy / Game Boy Color CPU) interpreter core.
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef uint8_t (*sm83_read_fn)(void* ctx, uint16_t addr);
typedef void (*sm83_write_fn)(void* ctx, uint16_t addr, uint8_t value);

typedef struct Sm83 Sm83;

typedef struct Sm83Regs {
    uint8_t a, f, b, c, d, e, h, l;
    uint16_t sp, pc;
    uint8_t ime, halted;
} Sm83Regs;

Sm83* sm83_new(void* ctx, sm83_read_fn read, sm83_write_fn write);
void sm83_free(Sm83* cpu);
// Executes one instruction (or services an interrupt). Returns T-cycles used.
// Interrupt state is read from the bus (IE at FFFF, IF at FF0F).
uint32_t sm83_step(Sm83* cpu);
void sm83_get_regs(const Sm83* cpu, Sm83Regs* out);
void sm83_set_regs(Sm83* cpu, const Sm83Regs* in);

// Save and restore every register and internal flag (not the host's callbacks), for save
// states and rewind. `state_size` bytes are written by save and read by load.
unsigned long sm83_state_size(void);
void sm83_save_state(const Sm83* cpu, uint8_t* out);
void sm83_load_state(Sm83* cpu, const uint8_t* in);

#ifdef __cplusplus
}
#endif
