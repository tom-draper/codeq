// Linux/glibc LD_PRELOAD probe. Counts successful intercepted allocation calls,
// including reallocations, across the entire CLI process. This is not peak RSS.
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>

extern void *__libc_malloc(size_t);
extern void *__libc_calloc(size_t, size_t);
extern void *__libc_realloc(void *, size_t);

static _Atomic unsigned long long allocations;

void *malloc(size_t size) {
    void *result = __libc_malloc(size);
    if (result) atomic_fetch_add_explicit(&allocations, 1, memory_order_relaxed);
    return result;
}

void *calloc(size_t count, size_t size) {
    void *result = __libc_calloc(count, size);
    if (result) atomic_fetch_add_explicit(&allocations, 1, memory_order_relaxed);
    return result;
}

void *realloc(void *ptr, size_t size) {
    void *result = __libc_realloc(ptr, size);
    if (result) atomic_fetch_add_explicit(&allocations, 1, memory_order_relaxed);
    return result;
}

__attribute__((destructor)) static void report(void) {
    unsigned long long count = atomic_load_explicit(&allocations, memory_order_relaxed);
    fprintf(stderr, "# allocation_probe: %llu\n", count);
}
