// Intel 8086/80186 real-mode interpreter core.
//
// The core owns 1 MiB of guest RAM (A20 disabled: addresses wrap at 1 MiB).
// The host supplies port I/O callbacks and a "BIOS trap" callback: the
// otherwise-unused opcode sequence 0F FF nn calls trap(ctx, cpu, nn), which is
// how the host implements BIOS services natively (high-level emulation).
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct X86 X86;

typedef uint16_t (*x86_in_fn)(void* ctx, uint16_t port, int word);
typedef void (*x86_out_fn)(void* ctx, uint16_t port, uint16_t value, int word);
typedef void (*x86_trap_fn)(void* ctx, X86* cpu, uint8_t n);

typedef struct X86Regs {
    uint16_t ax, cx, dx, bx, sp, bp, si, di;
    uint16_t es, cs, ss, ds;
    uint16_t ip, flags;
} X86Regs;

#define X86_MEM_SIZE 0x100000u

X86* x86_new(void* ctx, x86_in_fn in, x86_out_fn out, x86_trap_fn trap);
void x86_free(X86* cpu);
uint8_t* x86_mem(X86* cpu);
void x86_get_regs(const X86* cpu, X86Regs* out);
void x86_set_regs(X86* cpu, const X86Regs* in);
// Queues a hardware interrupt vector; delivered when IF=1 (wakes HLT).
void x86_irq(X86* cpu, uint8_t vector);
int x86_halted(const X86* cpu);
// Executes up to `max` instructions. Returns the number executed; stops early
// when the CPU halts with nothing deliverable, or when x86_stop() is called.
uint32_t x86_run(X86* cpu, uint32_t max);
// Ask x86_run to return after the current instruction (callable from callbacks).
void x86_stop(X86* cpu);

#ifdef __cplusplus
}
#endif
