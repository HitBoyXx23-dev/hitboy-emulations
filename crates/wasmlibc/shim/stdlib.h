#pragma once
#include <stddef.h>
#ifdef __cplusplus
extern "C" {
#endif
void* malloc(size_t);
void* calloc(size_t, size_t);
void* realloc(void*, size_t);
void free(void*);
void abort(void) __attribute__((noreturn));
#ifdef __cplusplus
}
#endif
#include "wasm_new.h"
