// sleeplat — how long does nanosleep(N us) really sleep, and how often does the sleeper run
// (docs/ARM64-PLAN.md §11 item 2b). `sleeplat <us> <count> [hog|-] [p50-limit-us]`: <count>
// sleeps of <us> microseconds, the overshoot's percentiles; with "hog" a thread spins pinned to the
// sleeper's core (the sleeper's core has other work: the parking path). Prints
// "sleeplat: N us x C: overshoot p50=.. p99=.. max=.. us", then with a limit "sleeplat: OK" or
// "sleeplat: OVERSLEPT" (a sleep served by the tick oversleeps to it: ~2 ms).
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static volatile int stop;
static unsigned long long now_ns(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return (unsigned long long)t.tv_sec * 1000000000ull + (unsigned long long)t.tv_nsec;
}
static void pin(int cpu) { cpu_set_t s; CPU_ZERO(&s); CPU_SET(cpu, &s); sched_setaffinity(0, sizeof s, &s); sched_yield(); }
static void *hog(void *a) { (void)a; pin(0); while (!stop) { } return 0; }
static int cmp(const void *a, const void *b) { unsigned x = *(const unsigned *)a, y = *(const unsigned *)b; return x < y ? -1 : x > y; }

int main(int argc, char **argv) {
    long us = argc > 1 ? atol(argv[1]) : 50, n = argc > 2 ? atol(argv[2]) : 20000;
    long lim = argc > 4 ? atol(argv[4]) : 0;
    int withhog = argc > 3 && !strcmp(argv[3], "hog");
    pin(0);
    pthread_t th;
    if (withhog) pthread_create(&th, 0, hog, 0);
    unsigned *ov = malloc(sizeof(unsigned) * (size_t)n);
    for (long i = 0; i < n; i++) {
        struct timespec r = { us / 1000000, (us % 1000000) * 1000 };
        unsigned long long t0 = now_ns();
        nanosleep(&r, 0);
        unsigned long long d = now_ns() - t0;
        ov[i] = d > (unsigned long long)us * 1000 ? (unsigned)((d - (unsigned long long)us * 1000) / 1000) : 0;
    }
    stop = 1;
    if (withhog) pthread_join(th, 0);
    qsort(ov, (size_t)n, sizeof *ov, cmp);
    printf("sleeplat: %ld us x %ld%s: overshoot p50=%u p99=%u max=%u us\n",
           us, n, withhog ? " (a hog on the core)" : "", ov[n / 2], ov[n * 99 / 100], ov[n - 1]);
    if (lim) printf("sleeplat: %s (p50 %u us, the limit %ld)\n", ov[n / 2] < lim ? "OK" : "OVERSLEPT", ov[n / 2], lim);
    return 0;
}
