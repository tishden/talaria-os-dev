// FUTEXLAT (PLAN-SYNC stage 2): the latency of a futex wake-up on an unmodified musl —
// the kernel sleeps with an mwait on a user word (a write wakes it without an IPI), the
// wake = a directed switch (same-core) / store+kick (cross-core). A ping-pong RTT, p50/p99.
// The build: musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -o futexlat futexlat.c
#define _GNU_SOURCE
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>
#include <sched.h>
#include <sys/syscall.h>

#define FUTEX_WAIT 0
#define FUTEX_WAKE 1
#define N 2000

static inline uint64_t rdtsc(void) {
    unsigned lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return (uint64_t)hi << 32 | lo;
}
static long fwait(volatile int *a, int v) { return syscall(SYS_futex, a, FUTEX_WAIT, v, 0, 0, 0); }
static long fwake(volatile int *a, int n) { return syscall(SYS_futex, a, FUTEX_WAKE, n, 0, 0, 0); }

static volatile int w1, w2, go_cross;
static uint64_t lat[N];
static double tsc_ns; // ns per tick

static uint64_t now_ns(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec; }

static void calibrate(void) {
    uint64_t n0 = now_ns(), t0 = rdtsc();
    for (volatile int i = 0; i < 20000000; i++) ;
    tsc_ns = (double)(now_ns() - n0) / (double)(rdtsc() - t0);
}

static int cmp64(const void *a, const void *b) {
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return x < y ? -1 : x > y;
}
static void report(const char *name) {
    qsort(lat, N, 8, cmp64);
    printf("FUTEXLAT %s: RTT p50=%llu ns p99=%llu ns min=%llu ns (N=%d)\n", name,
           (unsigned long long)(lat[N / 2] * tsc_ns), (unsigned long long)(lat[N * 99 / 100] * tsc_ns),
           (unsigned long long)(lat[0] * tsc_ns), N);
    fflush(stdout);
}

// ── raw futex ping-pong ──────────────────────────────────────────────────────
static void *pong_futex(void *arg) {
    if (arg) { cpu_set_t s; CPU_ZERO(&s); CPU_SET(1, &s); syscall(SYS_sched_setaffinity, 0, sizeof(s), &s); }
    for (int i = 1; i <= N; i++) {
        while (w1 < i) fwait(&w1, i - 1);
        w2 = i;
        fwake(&w2, 1);
    }
    return 0;
}
static void ping_futex(const char *name) {
    for (int i = 1; i <= N; i++) {
        uint64_t t0 = rdtsc();
        w1 = i;
        fwake(&w1, 1);
        while (w2 < i) fwait(&w2, i - 1);
        lat[i - 1] = rdtsc() - t0;
    }
    report(name);
}

// ── pthread mutex+condvar ping-pong ──────────────────────────────────────────
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int turn; // 0: nobody's, odd: B's turn, even: A's turn

static void *pong_cond(void *arg) {
    if (arg) { cpu_set_t s; CPU_ZERO(&s); CPU_SET(1, &s); syscall(SYS_sched_setaffinity, 0, sizeof(s), &s); }
    for (int i = 1; i <= N; i++) {
        pthread_mutex_lock(&mu);
        while (turn != 2 * i - 1) pthread_cond_wait(&cv, &mu);
        turn = 2 * i;
        pthread_cond_signal(&cv);
        pthread_mutex_unlock(&mu);
    }
    return 0;
}
static void ping_cond(const char *name) {
    for (int i = 1; i <= N; i++) {
        uint64_t t0 = rdtsc();
        pthread_mutex_lock(&mu);
        turn = 2 * i - 1;
        pthread_cond_signal(&cv);
        while (turn != 2 * i) pthread_cond_wait(&cv, &mu);
        pthread_mutex_unlock(&mu);
        lat[i - 1] = rdtsc() - t0;
    }
    report(name);
}

int main(void) {
    calibrate();
    pthread_t t;

    w1 = w2 = 0;
    pthread_create(&t, 0, pong_futex, (void *)0);
    ping_futex("futex same-core");
    pthread_join(t, 0);

    turn = 0;
    pthread_create(&t, 0, pong_cond, (void *)0);
    ping_cond("cond same-core");
    pthread_join(t, 0);

    w1 = w2 = 0;
    pthread_create(&t, 0, pong_futex, (void *)1);
    ping_futex("futex cross-core");
    pthread_join(t, 0);

    turn = 0;
    pthread_create(&t, 0, pong_cond, (void *)1);
    ping_cond("cond cross-core");
    pthread_join(t, 0);

    printf("FUTEXLAT: ALL OK\n");
    return 0;
}
