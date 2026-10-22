// wakesleep — a woken thread behind a SLEEPING sibling: the core's current task is asleep in its
// own futex wait (not spinning, as in wakedef), and the woken one must get the core at once
// (docs/ARM64-PLAN.md §11 item 2b, the "occupant asleep" class: on a c7g.large the core slept in
// four 62-us pieces over a ready neighbour until the sleeper's own wake rotated — 296 us; 7 ms
// on 2026-10-09). Two threads pin themselves to core 1: "ws_occ" sleeps on its own word (woken
// rarely, every 50 wakes of the other), "ws_wait" is the one measured; the main thread on core
// 0 wakes ws_wait N times at a random phase and measures how long it took to run.
// Usage: wakesleep [n] [p99 limit, us] [max limit, us]. Verdict: "wakesleep: OK" or "wakesleep: SLOW".
#define _GNU_SOURCE
#include <errno.h>
#define FUTEX_WAIT 0
#define FUTEX_WAKE 1
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static volatile int stop;
static volatile int go;                 // the measured waiter's word: 1 = run, 2 = exit
static volatile int occ;                // the occupant's word
static volatile unsigned long long t_run;
static volatile int ack;
static volatile int occ_cpu = -1, wait_cpu = -1;

static unsigned long long now_ns(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return (unsigned long long)t.tv_sec * 1000000000ull + (unsigned long long)t.tv_nsec;
}
static void pin(int cpu) {
    cpu_set_t s; CPU_ZERO(&s); CPU_SET(cpu, &s);
    sched_setaffinity(0, sizeof s, &s);
    sched_yield();
}
static void *occupant(void *a) {
    (void)a; prctl(PR_SET_NAME, "ws_occ", 0, 0, 0); pin(1);
    occ_cpu = sched_getcpu();
    for (;;) {
        while (occ == 0) syscall(SYS_futex, &occ, FUTEX_WAIT, 0, 0, 0, 0);
        if (occ == 2) return 0;
        occ = 0;
    }
}
static void *waiter(void *a) {
    (void)a; prctl(PR_SET_NAME, "ws_wait", 0, 0, 0); pin(1);
    wait_cpu = sched_getcpu();
    for (;;) {
        while (go == 0) syscall(SYS_futex, &go, FUTEX_WAIT, 0, 0, 0, 0);
        if (go == 2) return 0;
        t_run = now_ns();
        go = 0;
        ack = 1;
    }
}
static int cmp(const void *a, const void *b) { unsigned long long x = *(const unsigned long long *)a, y = *(const unsigned long long *)b; return x < y ? -1 : x > y; }

int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 1000;
    unsigned long long lim99 = argc > 2 ? strtoull(argv[2], 0, 10) : 250, limmax = argc > 3 ? strtoull(argv[3], 0, 10) : 1000;
    prctl(PR_SET_NAME, "ws_main", 0, 0, 0); pin(0);
    pthread_t to, tw;
    pthread_create(&to, 0, occupant, 0);
    pthread_create(&tw, 0, waiter, 0);
    struct timespec warm = {0, 20 * 1000 * 1000}; nanosleep(&warm, 0);
    if (occ_cpu < 0 || wait_cpu < 0 || occ_cpu != wait_cpu || occ_cpu == sched_getcpu()) {
        printf("wakesleep: SKIP (the occupant is on core %d, the waiter on core %d, the main thread on core %d: the test needs the two on one core and the main thread elsewhere)\n",
            occ_cpu, wait_cpu, sched_getcpu());
        stop = 1; go = 2; occ = 2; syscall(SYS_futex, &go, FUTEX_WAKE, 1, 0, 0, 0); syscall(SYS_futex, &occ, FUTEX_WAKE, 1, 0, 0, 0);
        return 0;
    }
    unsigned long long *lat = malloc(sizeof *lat * n), seed = 12345;
    int lost = 0;
    for (int i = 0; i < n; i++) {
        seed = seed * 6364136223846793005ull + 1442695040888963407ull;
        struct timespec gap = {0, (long)(50000 + (seed >> 33) % 900000)};   // 50..950 us: a random phase
        nanosleep(&gap, 0);
        if (i % 50 == 49) { occ = 1; syscall(SYS_futex, &occ, FUTEX_WAKE, 1, 0, 0, 0); }   // the occupant wakes rarely and sleeps again
        ack = 0;
        unsigned long long t0 = now_ns();
        go = 1;
        syscall(SYS_futex, &go, FUTEX_WAKE, 1, 0, 0, 0);
        while (!ack) { if (now_ns() - t0 > 100000000ull) break; }
        if (!ack) { lost++; lat[i] = 100000000ull; continue; }
        lat[i] = t_run - t0;
    }
    stop = 1; go = 2; occ = 2; syscall(SYS_futex, &go, FUTEX_WAKE, 1, 0, 0, 0); syscall(SYS_futex, &occ, FUTEX_WAKE, 1, 0, 0, 0);
    pthread_join(to, 0); pthread_join(tw, 0);
    qsort(lat, n, sizeof *lat, cmp);
    unsigned long long p50 = lat[n / 2] / 1000, p90 = lat[n * 9 / 10] / 1000, p99 = lat[n * 99 / 100] / 1000, mx = lat[n - 1] / 1000;
    int b[6] = {0}; // <50 <100 <200 <300 <500 500+
    for (int i = 0; i < n; i++) { unsigned long long us = lat[i] / 1000; b[us < 50 ? 0 : us < 100 ? 1 : us < 200 ? 2 : us < 300 ? 3 : us < 500 ? 4 : 5]++; }
    printf("wakesleep: a wakeup behind a sleeping sibling (core %d), us: p50 %llu p90 %llu p99 %llu max %llu (x %d, lost %d); <50:%d <100:%d <200:%d <300:%d <500:%d 500+:%d\n",
        occ_cpu, p50, p90, p99, mx, n, lost, b[0], b[1], b[2], b[3], b[4], b[5]);
    int ok = p99 <= lim99 && mx <= limmax && lost == 0;
    printf("wakesleep: %s (p99 %llu against the limit %llu, max %llu against %llu)\n", ok ? "OK" : "SLOW", p99, lim99, mx, limmax);
    return ok ? 0 : 1;
}
