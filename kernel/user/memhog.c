// memhog: it eats 64MB of anonymous mmap and crashes — a probe of the accounting of
// memory at supervise's restarts (talaria.test=restartmem): without freeing the regions
// every restart would lose 64MB; with the fix the drift between the restarts is ~0.
// The build: musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -o memhog memhog.c
#include <stdio.h>
#include <sys/mman.h>

int main(void) {
    const size_t sz = 64u << 20;
    char *p = mmap(0, sz, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) { printf("memhog: the mmap was refused\n"); return 1; }
    for (size_t i = 0; i < sz; i += 4096) p[i] = (char)i;
    printf("memhog: 64MB have been taken and touched, I am crashing\n");
    fflush(stdout);
    *(volatile int *)0 = 42; // a #PF -> supervise restarts
    return 0;
}
