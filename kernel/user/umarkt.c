// umarkt — the slow-interval instrument must name who held the cores (docs/ARM64-PLAN.md §11
// item 2b, kernel/src/umark.rs). A known truth: a thread named "um_hog" spins in ring 3 pinned to
// core 1 for the whole run, while the main thread on core 0 sleeps 2 ms and names that sleep to
// the kernel as a slow interval — N times. Core 1 is then held by um_hog all the time (~50 % of
// the core-time of two cores) and core 0 by the main thread asleep; the `stat:` block at the end
// says so (the gate reads it). Also: an empty or inverted interval is refused with EINVAL.
// Verdict: "umarkt: ALL OK <n>" or "umarkt: FAIL ...".
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/prctl.h>
#include <time.h>
#include <unistd.h>

#define PR_TALARIA_SLOW 0x54414c01

static volatile int stop;
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
    (void)a;
    prctl(PR_SET_NAME, "um_hog", 0, 0, 0);
    pin(1);
    while (!stop) { }
    return 0;
}

int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 50;
    prctl(PR_SET_NAME, "um_main", 0, 0, 0);
    pin(0);
    pthread_t th;
    pthread_create(&th, 0, hog, 0);
    struct timespec warm = {0, 20 * 1000 * 1000};
    nanosleep(&warm, 0);                                  // the hog is on core 1 by now
    unsigned long long t = now_ns();
    if (prctl(PR_TALARIA_SLOW, t, t, 0, 0) != -1 || errno != EINVAL) { printf("umarkt: FAIL an empty interval was taken\n"); return 1; }
    if (prctl(PR_TALARIA_SLOW, t, t - 1000, 0, 0) != -1 || errno != EINVAL) { printf("umarkt: FAIL an inverted interval was taken\n"); return 1; }
    for (int i = 0; i < n; i++) {
        struct timespec ms2 = {0, 2 * 1000 * 1000};
        unsigned long long t0 = now_ns();
        nanosleep(&ms2, 0);
        unsigned long long t1 = now_ns();
        if (prctl(PR_TALARIA_SLOW, t0, t1, 0, 0) != 0) { printf("umarkt: FAIL mark %d refused (errno %d)\n", i, errno); return 1; }
    }
    stop = 1;
    pthread_join(th, 0);
    printf("umarkt: ALL OK %d\n", n);
    return 0;
}
