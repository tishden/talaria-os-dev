// pinmove — sched_setaffinity moves the calling thread wherever it asks, FROM ANY core (ARM64-PLAN
// §11 item 5). The thread walks 0 -> 1 -> 2 -> 1 -> 0 -> 2 -> 0 (the cores the machine has), and after
// every move sched_getcpu() must name the core asked for; a short spin shows the thread RUNS there.
// Before, a move was honoured only from the boot core (a PoC): a thread on an AP that asked for
// another core silently stayed where it was. Prints "pinmove: ALL OK" or "pinmove: FAILURES n".
#define _GNU_SOURCE
#include <sched.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

static int cpu_now(void) { unsigned c = 0, n = 0; syscall(SYS_getcpu, &c, &n, 0); return (int)c; }

int main(void) {
    cpu_set_t all; CPU_ZERO(&all);
    sched_getaffinity(0, sizeof all, &all);
    int ncpu = CPU_COUNT(&all);
    static const int walk[] = {1, 2, 1, 0, 2, 0};
    int bad = 0, moves = 0;
    volatile unsigned long spin = 0;
    for (unsigned i = 0; i < sizeof walk / sizeof walk[0]; i++) {
        int want = walk[i];
        if (want >= ncpu) { printf("pinmove: the core %d is not there (%d online) — skipped\n", want, ncpu); continue; }
        int from = cpu_now();
        cpu_set_t s; CPU_ZERO(&s); CPU_SET(want, &s);
        int r = sched_setaffinity(0, sizeof s, &s);
        for (int k = 0; k < 200000; k++) spin++;          // run a little where we landed
        int got = cpu_now();
        int ok = r == 0 && got == want;
        printf("pinmove: %d -> %d (asked %d) r=%d %s\n", from, got, want, r, ok ? "ok" : "BAD");
        if (!ok) bad++; else moves++;
    }
    if (bad) printf("pinmove: FAILURES %d\n", bad);
    else printf("pinmove: ALL OK (%d moves, %d cores online)\n", moves, ncpu);
    return bad ? 1 : 0;
}
