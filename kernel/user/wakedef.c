// wakedef — a woken thread behind a RUNNING sibling must wait no longer than the preemption
// threshold (docs/ARM64-PLAN.md §11 item 2b, `talaria.wakepreempt=`, default 150 us). A thread
// "wd_hog" spins; "wd_wait" sleeps on a futex on the same core; the main thread on core 0 wakes
// it N times at a random phase of the hog's slice and measures how long the waiter took to run.
// Both pin themselves to core 1 (a cell on two cores: the kernel spreads the first new thread
// onto core 1 and the second onto core 0, whence it moves); the test checks they share a core
// and says SKIP otherwise. In `talaria.test=` mode the APs are the legacy dedicated ones without
// a tick, so the gates boot it as a cell from a manifest. Before the fix a wakeup that found the hog younger than the threshold
// only kicked the core, and the waiter waited for the END OF THE SLICE (up to 500 us); now the
// core's tick is pulled to the moment the hog reaches the threshold.
// Usage: wakedef [n] [p99 limit, us] [max limit, us]. Verdict: "wakedef: OK" or "wakedef: SLOW".
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
static volatile int go;                 // the futex word: 1 = run
static volatile unsigned long long t_run;
static volatile int ack;
static volatile int hog_cpu = -1, wait_cpu = -1;

static unsigned long long now_ns(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return (unsigned long long)t.tv_sec * 1000000000ull + (unsigned long long)t.tv_nsec;
}
static void pin(int cpu) {
    cpu_set_t s; CPU_ZERO(&s); CPU_SET(cpu, &s);
    sched_setaffinity(0, sizeof s, &s);
    sched_yield();
}
static void *hog(void *a) {
    (void)a; prctl(PR_SET_NAME, "wd_hog", 0, 0, 0); pin(1);
    hog_cpu = sched_getcpu();
    while (!stop) { }
    return 0;
}
static void *waiter(void *a) {
    (void)a; prctl(PR_SET_NAME, "wd_wait", 0, 0, 0); pin(1);
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
    unsigned long long lim99 = argc > 2 ? strtoull(argv[2], 0, 10) : 250, limmax = argc > 3 ? strtoull(argv[3], 0, 10) : 450;
    prctl(PR_SET_NAME, "wd_main", 0, 0, 0); pin(0);
    pthread_t th, tw;
    pthread_create(&th, 0, hog, 0);
    pthread_create(&tw, 0, waiter, 0);
    struct timespec warm = {0, 20 * 1000 * 1000}; nanosleep(&warm, 0);
    if (hog_cpu < 0 || wait_cpu < 0 || hog_cpu != wait_cpu || hog_cpu == sched_getcpu()) {
        printf("wakedef: SKIP (the hog is on core %d, the waiter on core %d, the main thread on core %d: the test needs the two on one core and the main thread elsewhere)\n",
            hog_cpu, wait_cpu, sched_getcpu());
        stop = 1; go = 2; syscall(SYS_futex, &go, FUTEX_WAKE, 1, 0, 0, 0);
        return 0;
    }
    unsigned long long *lat = malloc(sizeof *lat * n), seed = 12345;
    int lost = 0;
    for (int i = 0; i < n; i++) {
        seed = seed * 6364136223846793005ull + 1442695040888963407ull;
        struct timespec gap = {0, (long)(50000 + (seed >> 33) % 900000)};   // 50..950 us: a random phase of the slice
        nanosleep(&gap, 0);
        ack = 0;
        unsigned long long t0 = now_ns();
        go = 1;
        syscall(SYS_futex, &go, FUTEX_WAKE, 1, 0, 0, 0);
        while (!ack) { if (now_ns() - t0 > 100000000ull) break; }
        if (!ack) { lost++; lat[i] = 100000000ull; continue; }
        lat[i] = t_run - t0;
    }
    stop = 1; go = 2; syscall(SYS_futex, &go, FUTEX_WAKE, 1, 0, 0, 0);
    pthread_join(th, 0); pthread_join(tw, 0);
    qsort(lat, n, sizeof *lat, cmp);
    unsigned long long p50 = lat[n / 2] / 1000, p90 = lat[n * 9 / 10] / 1000, p99 = lat[n * 99 / 100] / 1000, mx = lat[n - 1] / 1000;
    int b[6] = {0}; // <50 <100 <200 <300 <500 500+
    for (int i = 0; i < n; i++) { unsigned long long us = lat[i] / 1000; b[us < 50 ? 0 : us < 100 ? 1 : us < 200 ? 2 : us < 300 ? 3 : us < 500 ? 4 : 5]++; }
    printf("wakedef: a wakeup behind a spinning sibling (core %d), us: p50 %llu p90 %llu p99 %llu max %llu (x %d, lost %d); <50:%d <100:%d <200:%d <300:%d <500:%d 500+:%d\n",
        hog_cpu, p50, p90, p99, mx, n, lost, b[0], b[1], b[2], b[3], b[4], b[5]);
    int ok = p99 <= lim99 && mx <= limmax && lost == 0;
    printf("wakedef: %s (p99 %llu against the limit %llu, max %llu against %llu)\n", ok ? "OK" : "SLOW", p99, lim99, mx, limmax);
    return ok ? 0 : 1;
}
