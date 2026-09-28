// The workload for the gate of the stack unwinding (talaria.test=profdeep).
//
// A profiler must not merely name the hot address but show WHO called it. This cannot be
// checked on a real workload: there the chain of calls depends on what the compiler
// decided to inline. Here it is given explicitly — five levels, each one noinline — and
// the gate compares the DEPTH and the REPEATABILITY of the stack rather than the specific
// addresses (every build has its own).
//
// It is built WITH FRAME POINTERS (-fno-omit-frame-pointer) and WITHOUT strip: without the
// first there is nothing to unwind, without the second there is nothing for addr2line to resolve.
#include <stdio.h>
#include <stdint.h>

static volatile uint64_t sink;

__attribute__((noinline)) static uint64_t burn(uint64_t n) {
    uint64_t a = 0x9E3779B97F4A7C15ull;
    for (uint64_t i = 0; i < n; i++) a = a * 6364136223846793005ull + i;
    sink = a;
    return a;
}
__attribute__((noinline)) static uint64_t lvl4(uint64_t n) { return burn(n) + 4; }
__attribute__((noinline)) static uint64_t lvl3(uint64_t n) { return lvl4(n) + 3; }
__attribute__((noinline)) static uint64_t lvl2(uint64_t n) { return lvl3(n) + 2; }
__attribute__((noinline)) static uint64_t lvl1(uint64_t n) { return lvl2(n) + 1; }

int main(int argc, char **argv) {
    (void)argv;
    // We count long enough for the tick to take dozens of samples, but
    // short enough for the gate to fit into the set's window.
    uint64_t rounds = (argc > 1) ? 40 : 20;
    uint64_t acc = 0;
    printf("profdeep: the chain main->lvl1->lvl2->lvl3->lvl4->burn, %llu rounds\n",
           (unsigned long long)rounds);
    for (uint64_t r = 0; r < rounds; r++) acc += lvl1(2000000);
    printf("profdeep: done, acc=%llu\n", (unsigned long long)acc);
    return 0;
}
