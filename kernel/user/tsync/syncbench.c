// SYNCBENCH (PLAN-SYNC): the target benches — an SPSC through the mutex+condvar semantics
// (libtsync, a sleep on the word of the index) AGAINST a spinning ring buffer; the latency
// of push->pop with an idle queue (the full path of the wake-up) and the throughput.
// Plus the microbenches: the wait/wake RTT, an uncontended mutex, the condvar RTT.
// The build: musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -o syncbench syncbench.c tsync.c
#define _GNU_SOURCE
#include "tsync.h"
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdatomic.h>
#include <stdint.h>
#include <time.h>
#include <unistd.h>
#include <sched.h>
#include <sys/syscall.h>

static inline uint64_t rdtsc(void) { unsigned lo, hi; __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi)); return (uint64_t)hi << 32 | lo; }
static void to_ap(void) { cpu_set_t s; CPU_ZERO(&s); CPU_SET(1, &s); syscall(SYS_sched_setaffinity, 0, sizeof(s), &s); }
static uint64_t now_ns(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec; }
static double tsc_ns;
static void calibrate(void) { uint64_t n0 = now_ns(), t0 = rdtsc(); for (volatile int i = 0; i < 30000000; i++) ; tsc_ns = (double)(now_ns() - n0) / (double)(rdtsc() - t0); }

#define LATN 20000
static uint64_t lat[LATN];
static int cmp64(const void *a, const void *b) { uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b; return x < y ? -1 : x > y; }
static void report(const char *name, int n) {
    qsort(lat, n, 8, cmp64);
    printf("SYNCBENCH %s: p50=%llu ns p99=%llu ns min=%llu ns (N=%d)\n", name,
           (unsigned long long)(lat[n / 2] * tsc_ns), (unsigned long long)(lat[n * 99 / 100] * tsc_ns),
           (unsigned long long)(lat[0] * tsc_ns), n);
    fflush(stdout);
}

// ── the spinning SPSC (the baseline: a busy poll, a full Rigtorp — cached indices) ──
#define SCAP 1024
static struct {
    _Atomic uint64_t tail; char p0[56];
    _Atomic uint64_t head; char p1[56];
    uint64_t cached_head; char p2[56]; // the producer's local
    uint64_t cached_tail; char p3[56]; // the consumer's local
    uint64_t buf[SCAP];
} sq;
static void spin_push(uint64_t v) {
    uint64_t t = atomic_load_explicit(&sq.tail, memory_order_relaxed);
    if (t - sq.cached_head >= SCAP) {
        while ((sq.cached_head = atomic_load_explicit(&sq.head, memory_order_acquire)), t - sq.cached_head >= SCAP)
            __asm__ volatile("pause");
    }
    sq.buf[t & (SCAP - 1)] = v;
    atomic_store_explicit(&sq.tail, t + 1, memory_order_release);
}
static int spin_pop(uint64_t *v) {
    uint64_t h = atomic_load_explicit(&sq.head, memory_order_relaxed);
    if (h == sq.cached_tail) {
        sq.cached_tail = atomic_load_explicit(&sq.tail, memory_order_acquire);
        if (h == sq.cached_tail) return -1;
    }
    *v = sq.buf[h & (SCAP - 1)];
    atomic_store_explicit(&sq.head, h + 1, memory_order_release);
    return 0;
}

// ── the consumers ────────────────────────────────────────────────────────────
#define TPUT_N 2000000ull
static volatile int mode; // 1 spin-throughput, 2 spin-latency, 3 ts-throughput, 4 ts-latency
static volatile uint64_t consumed;
static tsync_spsc tq;
static uint64_t tqbuf[SCAP];
static _Atomic uint64_t pop_ts[LATN]; // the rdtsc at the pop (the latency phase)

static void *consumer(void *arg) {
    (void)arg; to_ap();
    for (;;) {
        int m = mode;
        if (m == 1) { uint64_t v; uint64_t n = 0; while (n < TPUT_N) { if (!spin_pop(&v)) n++; else __asm__ volatile("pause"); } consumed = n; }
        else if (m == 2) { uint64_t v; for (int i = 0; i < LATN; i++) { while (spin_pop(&v)) __asm__ volatile("pause"); atomic_store(&pop_ts[v], rdtsc()); } consumed = LATN; }
        else if (m == 3) { uint64_t n = 0; while (n < TPUT_N) { tsync_spsc_pop(&tq); n++; } consumed = n; }
        else if (m == 4) { for (int i = 0; i < LATN; i++) { uint64_t v = tsync_spsc_pop(&tq); atomic_store(&pop_ts[v], rdtsc()); } consumed = LATN; }
        else if (m == 6) { uint64_t v; uint64_t n = 0; while (n < TPUT_N) { if (!tsync_spsc_try_pop(&tq, &v)) n++; else __asm__ volatile("pause"); } consumed = n; }
        else if (m == 5) return 0;
        mode = 0;
        while (mode == 0) ; // busy: the next phase starts quickly
    }
}

// the latency phase: a push once every ~20 us — the consumer has time to fall asleep; we measure push->pop
static void run_latency(int m, const char *name, void (*push)(uint64_t)) {
    for (int i = 0; i < LATN; i++) atomic_store(&pop_ts[i], 0);
    consumed = 0; mode = m;
    uint64_t gap = (uint64_t)(20000.0 / tsc_ns); // 20 us in ticks
    for (uint64_t i = 0; i < LATN; i++) {
        uint64_t t0 = rdtsc();
        push(i);
        lat[i] = t0; // the time of the push; the latency = pop_ts - t0
        while (rdtsc() - t0 < gap) __asm__ volatile("pause");
    }
    while (consumed != LATN) ;
    for (int i = 0; i < LATN; i++) lat[i] = atomic_load(&pop_ts[i]) - lat[i];
    report(name, LATN);
}
static void ts_push(uint64_t v) { tsync_spsc_push(&tq, v); }
static uint64_t run_tput_once(int m, int spin) {
    consumed = 0; mode = m;
    uint64_t t0 = now_ns();
    for (uint64_t i = 0; i < TPUT_N; i++) { if (spin) spin_push(i); else tsync_spsc_push(&tq, i); }
    while (consumed != TPUT_N) ;
    return now_ns() - t0;
}
// 5 repeats, the best one (the min dt): the stand 4C/8T is noisy — we take the cleanest run
static void run_tput(int m, const char *name, int spin) {
    uint64_t best = ~0ull;
    for (int r = 0; r < 5; r++) {
        uint64_t dt = run_tput_once(m, spin);
        if (dt < best) best = dt;
        while (mode) ;
        if (r < 4) { /* the consumer is already waiting for the next phase */ }
    }
    printf("SYNCBENCH %s: %llu Mops (best of 5, %llu ms for %llu)\n", name,
           (unsigned long long)(TPUT_N * 1000 / best), (unsigned long long)(best / 1000000), (unsigned long long)TPUT_N);
    fflush(stdout);
}

// ── the wait/wake RTT (tsync, cross-core) ────────────────────────────────────
static volatile uint32_t ww1, ww2;
#define WWN 2000
static void *ww_pong(void *arg) {
    (void)arg; to_ap();
    for (uint32_t i = 1; i <= WWN; i++) {
        while (ww1 < i) tsync_wait(&ww1, i - 1);
        ww2 = i;
        tsync_wake(&ww2, 1);
    }
    return 0;
}

int main(void) {
    calibrate();
    tsync_init();
    pthread_t t;

    // wait/wake RTT
    pthread_create(&t, 0, ww_pong, 0);
    for (uint32_t i = 1; i <= WWN; i++) {
        uint64_t t0 = rdtsc();
        ww1 = i;
        tsync_wake(&ww1, 1);
        while (ww2 < i) tsync_wait(&ww2, i - 1);
        lat[i - 1] = rdtsc() - t0;
    }
    pthread_join(t, 0);
    report("wait/wake RTT cross-core", WWN);

    // uncontended mutex lock/unlock
    {
        tsync_mutex m = TSYNC_MUTEX_INIT;
        uint64_t t0 = rdtsc();
        for (int i = 0; i < 1000000; i++) { tsync_mutex_lock(&m); tsync_mutex_unlock(&m); }
        uint64_t c = (rdtsc() - t0) / 1000000;
        pthread_mutex_t pm = PTHREAD_MUTEX_INITIALIZER;
        t0 = rdtsc();
        for (int i = 0; i < 1000000; i++) { pthread_mutex_lock(&pm); pthread_mutex_unlock(&pm); }
        uint64_t cp = (rdtsc() - t0) / 1000000;
        printf("SYNCBENCH mutex uncontended: tsync=%llu ticks/lock+unlock, pthread=%llu\n",
               (unsigned long long)c, (unsigned long long)cp);
    }

    // SPSC
    tsync_spsc_init(&tq, SCAP, tqbuf);
    tsync_spsc_set_batch(&tq, 1); // the pre-spin closes the hot path; the sleep is only when it is idle
    pthread_create(&t, 0, consumer, 0);
    run_tput(1, "spin-SPSC throughput cross-core", 1);
    run_latency(2, "the spin-SPSC push->pop latency (an empty queue)", spin_push);
    while (mode) ; run_tput(3, "the tsync-SPSC (a cv sleep) throughput cross-core", 0);
    while (mode) ; run_tput(6, "the tsync-SPSC (a busy consumer, try_pop) throughput", 0);
    printf("SYNCBENCH the tsync-SPSC counters: p_slow=%llu p_parks=%llu p_slow_ms=%llu | c_slow=%llu c_parks=%llu c_slow_ms=%llu\n",
           (unsigned long long)tq.p_slow, (unsigned long long)tq.p_parks, (unsigned long long)(tq.p_slow_cyc * tsc_ns / 1e6),
           (unsigned long long)tq.c_slow, (unsigned long long)tq.c_parks, (unsigned long long)(tq.c_slow_cyc * tsc_ns / 1e6));
    fflush(stdout);
    run_latency(4, "the tsync-SPSC (a cv sleep) push->pop latency (a sleep and a wake-up)", ts_push);
    while (mode) ; mode = 5;
    pthread_join(t, 0);

    printf("SYNCBENCH: ALL OK\n");
    return 0;
}
