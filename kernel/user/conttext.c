// conttext — the Contiguous hint on a read-only segment (`talaria.cont=1`, arm64;
// docs/ARM64-PLAN.md §12 item 3). The loader folds every whole 64 KB of the text into one
// TLB entry; a write that changes one page of a group must unfold the WHOLE group first,
// or the TLB may keep translating the changed page through the 64 KB entry.
//   1. bigfn (256 KB: `add x0, x0, #1` 65534 times — it returns how many it ran) runs and its
//      bytes sum as linked;
//   2. a child of fork runs it too (the child's table carries the groups whole);
//   3. two threads on two cores run bigfn without a pause while the main thread makes one
//      page in the middle writable and writes a `ret` at its start (a COW copy: the page's
//      frame changes under a folded group): every thread must come to return the shortened
//      count — a stale 64 KB translation keeps it on the old frame and the full count — and
//      after the page is restored, the full count again. 50 rounds.
// Verdict: "conttext: ALL OK" or "conttext: FAIL <what>".
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

// 64 Ki instructions, 64 KB-aligned so the groups are whole: x0 = 0, 65534 increments, `ret`.
__asm__(".text\n.balign 65536\n.global bigfn\n.type bigfn,%function\nbigfn:\n"
        "mov x0, #0\n.rept 65534\nadd x0, x0, #1\n.endr\nret\n.size bigfn,.-bigfn\n.balign 65536\n");
extern long bigfn(void);
#define BIG (65536 * 4)
#define FULL 65534L
// the page 64 KB + 4 KB in (the second group's second page): a `ret` at its first word
#define AT ((65536 + 4096) / 4)
#define SHORT ((long)AT - 1)
static const uint32_t ADD = 0x91000400, RET = 0xd65f03c0;

static uint64_t sum(void) {
    const uint32_t *p = (const uint32_t *)(uintptr_t)bigfn;
    uint64_t s = 0;
    for (int i = 0; i < BIG / 4; i++) s = s * 31 + p[i];
    return s;
}
static void pin(int cpu) { cpu_set_t s; CPU_ZERO(&s); CPU_SET(cpu, &s); sched_setaffinity(0, sizeof s, &s); sched_yield(); }
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec * 1e-9; }

static volatile int quit;
static volatile long last[2];
static void *runner(void *a) {
    int me = (int)(intptr_t)a;
    pin(me);
    // `isb` between calls: another core's write of code is architecturally seen by this core
    // only after a context synchronization (the writer's `ic ivau` reached every icache)
    while (!quit) { last[me] = bigfn(); __asm__ volatile("isb" ::: "memory"); }
    return 0;
}

// Wait until both runners return `want` (2 s at most).
static int both(long want) {
    last[0] = last[1] = -1;
    double t0 = now();
    while (now() - t0 < 2.0) if (last[0] == want && last[1] == want) return 1;
    return 0;
}
// On a failure: what the page holds as data, and what a call returns on every core.
static void diag(uint32_t *pg) {
    printf("conttext: diag: the word reads %#x; a call returns", pg[0]);
    for (int c = 0; c < 4; c++) { pin(c); long r = bigfn(); printf(" core%d=%ld", c, r); }
    sched_yield(); printf(" again=%ld\n", bigfn());
}
static void put(uint32_t *pg, uint32_t w) {
    mprotect(pg, 4096, PROT_READ | PROT_WRITE | PROT_EXEC);
    pg[0] = w;
    __builtin___clear_cache((char *)pg, (char *)pg + 4);
    mprotect(pg, 4096, PROT_READ | PROT_EXEC);
}

int main(void) {
    uint64_t s0 = sum();
    if (bigfn() != FULL) { printf("conttext: FAIL bigfn returned %ld, not %ld\n", bigfn(), FULL); return 1; }
    pid_t c = fork();
    if (c == 0) _exit(bigfn() == FULL && sum() == s0 ? 0 : 3);
    int st = 0; waitpid(c, &st, 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) { printf("conttext: FAIL the child of fork (status %#x)\n", st); return 1; }
    pthread_t th[2];
    for (int i = 0; i < 2; i++) pthread_create(&th[i], 0, runner, (void *)(intptr_t)i);
    uint32_t *pg = (uint32_t *)((uintptr_t)bigfn + AT * 4);
    for (int round = 0; round < 50; round++) {
        put(pg, RET);
        if (bigfn() != SHORT) { printf("conttext: FAIL round %d: this thread returns %ld after the `ret` (%ld wanted)\n", round, bigfn(), SHORT); diag(pg); return 1; }
        if (!both(SHORT)) { printf("conttext: FAIL round %d: a runner keeps returning %ld / %ld after the `ret` (a stale translation)\n", round, last[0], last[1]); quit = 1; diag(pg); return 1; }
        put(pg, ADD);
        if (bigfn() != FULL) { printf("conttext: FAIL round %d: this thread returns %ld after the restore\n", round, bigfn()); diag(pg); return 1; }
        if (!both(FULL)) { printf("conttext: FAIL round %d: a runner keeps returning %ld / %ld after the restore\n", round, last[0], last[1]); quit = 1; diag(pg); return 1; }
    }
    quit = 1;
    for (int i = 0; i < 2; i++) pthread_join(th[i], 0);
    if (sum() != s0) { printf("conttext: FAIL the text's sum changed\n"); return 1; }
    printf("conttext: ALL OK 50 rounds\n");
    return 0;
}
