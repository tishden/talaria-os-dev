// cowstale — a COW copy must not leave another core translating to the original
// (docs/ARM64-PLAN.md §11 item 0). Staged with pinning, every round:
//   1. on core 1 the parent reads page X (core 1 caches X -> the original frame, read-only:
//      a live child shares it);
//   2. the parent moves to core 0 — core 1 idles, lazily keeping the parent's CR3;
//   3. on core 0 the parent writes X: a COW copy, X -> a new frame;
//   4. the child exits (the original is nobody's and is freed); the parent maps fresh pages and
//      fills them with a marker, so the freed frame is reused;
//   5. back on core 1 ("the same CR3, no reload") the parent reads X: its own value, or — with a
//      stale translation left on core 1 — the marker / garbage.
// Verdict: "cowstale: ALL OK <rounds>" or "cowstale: STALE".
#define _GNU_SOURCE
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>
#include <pthread.h>
#include <time.h>

static void pin(int cpu) {
    cpu_set_t s; CPU_ZERO(&s); CPU_SET(cpu, &s);
    sched_setaffinity(0, sizeof s, &s);
    sched_yield();
}

// THE SIBLING-THREAD CASE ("threads"): T2 spins on core 1 reading X (its TLB holds X -> the
// original, read-only); T1 on core 0 writes X after a fork (a COW copy). Without a shootdown T2
// keeps reading the original and never sees T1's write.
static volatile unsigned long *tx;
static volatile unsigned long expect;
static volatile int phase;                               // 0 idle, 1 check `expect`, -1 quit
static void *spinner(void *a) {
    (void)a; pin(1);
    struct timespec t0, t;
    for (;;) {
        while (__atomic_load_n(&phase, __ATOMIC_SEQ_CST) == 0) { (void)tx[0]; } // keep the translation hot
        if (phase < 0) return 0;
        unsigned long want = expect;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        while (tx[0] != want) {
            clock_gettime(CLOCK_MONOTONIC, &t);
            if ((t.tv_sec - t0.tv_sec) * 1000000000L + (t.tv_nsec - t0.tv_nsec) > 500000000L) {
                printf("cowstale: STALE (threads): the sibling on the other core still reads %#lx, the write was %#lx\n", tx[0], want);
                exit(1);
            }
        }
        __atomic_store_n(&phase, 0, __ATOMIC_SEQ_CST);
    }
}
static int threads_mode(int rounds) {
    pin(0);
    tx = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    tx[0] = 1;
    pthread_t th; pthread_create(&th, 0, spinner, 0);
    for (int r = 0; r < rounds; r++) {
        int go[2]; if (pipe(go)) return 1;
        pid_t c = fork();                                // X is COW-shared again
        if (c == 0) { char b; read(go[0], &b, 1); _exit(0); }
        struct timespec d = {0, 2000000}; nanosleep(&d, 0); // the sibling re-reads X (read-only now)
        unsigned long v = 0x3333000000000000ul + r;
        tx[0] = v;                                        // COW on core 0: X -> a copy
        expect = v;
        __atomic_store_n(&phase, 1, __ATOMIC_SEQ_CST);
        while (__atomic_load_n(&phase, __ATOMIC_SEQ_CST) != 0) {}
        write(go[1], "x", 1); waitpid(c, 0, 0); close(go[0]); close(go[1]);
    }
    __atomic_store_n(&phase, -1, __ATOMIC_SEQ_CST); pthread_join(th, 0);
    printf("cowstale: ALL OK %d threads\n", rounds);
    return 0;
}

int main(int argc, char **argv) {
    int rounds = argc > 1 ? atoi(argv[1]) : 300;
    if (argc > 2 && strcmp(argv[2], "threads") == 0) return threads_mode(rounds);
    const size_t FILL = 64 * 4096;
    for (int r = 0; r < rounds; r++) {
        volatile unsigned long *x = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        x[0] = 0x1111000000000000ul + r;                 // a private page, then shared by the fork
        int go[2]; if (pipe(go)) return 1;
        pid_t c = fork();
        if (c == 0) { char b; read(go[0], &b, 1); _exit(0); }   // holds the page until told
        pin(1);
        int c1 = sched_getcpu();
        unsigned long seen1 = x[0];                      // core 1 caches X -> the original (read-only)
        pin(0);
        int c0 = sched_getcpu();
        x[0] = 0x2222000000000000ul + r;                 // COW on core 0: X -> a copy
        write(go[1], "x", 1); waitpid(c, 0, 0);          // the original's last owner leaves
        char *fill = mmap(0, FILL, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        for (size_t o = 0; o < FILL; o += 8) *(unsigned long *)(fill + o) = 0xDEAD0000DEAD0000ul;
        pin(1);
        int c2 = sched_getcpu();
        unsigned long seen2 = x[0];                      // must be the parent's own write
        if (seen1 != 0x1111000000000000ul + r || seen2 != 0x2222000000000000ul + r) {
            printf("cowstale: STALE at round %d: before the copy %#lx (core %d), the copy on core %d, after it %#lx (core %d; want %#lx)\n",
                   r, seen1, c1, c0, seen2, c2, 0x2222000000000000ul + r);
            return 1;
        }
        munmap(fill, FILL); munmap((void *)x, 4096); close(go[0]); close(go[1]);
    }
    printf("cowstale: ALL OK %d\n", rounds);
    return 0;
}
