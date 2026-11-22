// Memory operators for C++ on WebAssembly: weak so several cores can each include this.
#pragma once
#include <stddef.h>
#ifdef __cplusplus
extern "C" {
void* malloc(size_t);
void free(void*);
}
__attribute__((weak)) void* operator new(size_t n) { return malloc(n); }
__attribute__((weak)) void* operator new[](size_t n) { return malloc(n); }
__attribute__((weak)) void operator delete(void* p) noexcept { free(p); }
__attribute__((weak)) void operator delete[](void* p) noexcept { free(p); }
__attribute__((weak)) void operator delete(void* p, size_t) noexcept { free(p); }
__attribute__((weak)) void operator delete[](void* p, size_t) noexcept { free(p); }
#endif
