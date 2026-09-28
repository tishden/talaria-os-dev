// FIBERBENCH (PLAN-SYNC stage 4): a parity with coroutines on one core — a bare switch,
// a condvar handoff, an SPSC through a fiber mutex+cv; plus the kernel's directed switch
// (swtchto) between the BSP's threads.
// The build: musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -o fiberbench fiberbench.c fiber.c tsync.c
#define _GNU_SOURCE
#include "fiber.h"
#include "tsync.h"
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <time.h>
#include <unistd.h>
#include <sys/syscall.h>

static inline uint64_t rdtsc(void) { unsigned lo, hi; __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi)); return (uint64_t)hi << 32 | lo; }
static uint64_t now_ns(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec; }
static double tsc_ns;
static void calibrate(void) { uint64_t n0 = now_ns(), t0 = rdtsc(); for (volatile int i = 0; i < 30000000; i++) ; tsc_ns = (double)(now_ns() - n0) / (double)(rdtsc() - t0); }

static char stk1[65536], stk2[65536];
static int fails;
#define CHECK(cond, name) do { if (cond) printf("  ok  %s\n", name); else { printf("  FAIL %s\n", name); fails++; } } while (0)

// ── 1. the bare switch (a yield ping-pong) — this is what a "coroutine" is ───
#define SW_N 2000000ull
static volatile uint64_t sw_count;
static void f_yielder(void *a) { (void)a; for (uint64_t i = 0; i < SW_N; i++) { sw_count++; tfib_yield(); } }

// ── 2. condvar handoff ping-pong ────────────────────────────────────────────
#define CV_N 500000ull
static tfib_cond c_ab = TFIB_COND_INIT, c_ba = TFIB_COND_INIT;
static volatile uint64_t cv_turn, cv_rounds;
static void f_pong(void *a) {
    (void)a;
    for (uint64_t i = 1; i <= CV_N; i++) {
        while (cv_turn != 2 * i - 1) tfib_cond_wait(&c_ab);
        cv_turn = 2 * i;
        cv_rounds++;
        tfib_cond_signal_switch(&c_ba);
    }
}

// ── 3. an SPSC through a fiber mutex+cv (the standard semantics, one core) ──
#define Q_N 500000ull
#define QCAP 16
static struct { uint64_t buf[QCAP]; uint64_t h, t; } fq;
static tfib_mutex qm = TFIB_MUTEX_INIT;
static tfib_cond q_ne = TFIB_COND_INIT, q_nf = TFIB_COND_INIT;
static volatile uint64_t q_bad, q_got;
static void f_consumer(void *a) {
    (void)a;
    for (uint64_t i = 0; i < Q_N; i++) {
        tfib_mutex_lock(&qm);
        while (fq.h == fq.t) { tfib_mutex_unlock(&qm); tfib_cond_wait(&q_ne); tfib_mutex_lock(&qm); }
        uint64_t v = fq.buf[fq.h++ & (QCAP - 1)];
        tfib_mutex_unlock(&qm);
        tfib_cond_signal(&q_nf);
        if (v != i) q_bad++;
        q_got++;
    }
}

// ── 4. the kernel's swtchto between two threads of the BSP ──────────────────
#define KSW_N 100000
static volatile int kturn; static volatile long tid_b;
static long swtchto(long tid) { return syscall(4600, tid); }
static void *kthr(void *a) {
    (void)a;
    tid_b = syscall(SYS_gettid);
    long tid_a = 1;
    for (int i = 1; i <= KSW_N; i++) {
        while (kturn != 2 * i - 1) swtchto(tid_a);
        kturn = 2 * i;
        swtchto(tid_a);
    }
    return 0;
}

int main(void) {
    calibrate();
    tfib_init();

    tsync_fiber *f = tfib_spawn(f_yielder, 0, stk1, sizeof stk1);
    uint64_t t0 = rdtsc();
    for (uint64_t i = 0; i < SW_N; i++) tfib_yield();
    tfib_join(f);
    uint64_t per_sw = (uint64_t)((rdtsc() - t0) * tsc_ns) / (2 * SW_N);
    printf("FIBER the switch (a yield ping-pong): %llu ns/switch (%llu switches)\n", (unsigned long long)per_sw, (unsigned long long)(2 * SW_N));
    CHECK(sw_count == SW_N, "the yield ping-pong: the counter agrees");

    f = tfib_spawn(f_pong, 0, stk1, sizeof stk1);
    t0 = rdtsc();
    for (uint64_t i = 1; i <= CV_N; i++) {
        cv_turn = 2 * i - 1;
        tfib_cond_signal_switch(&c_ab);
        while (cv_turn != 2 * i) tfib_cond_wait(&c_ba);
    }
    tfib_join(f);
    uint64_t per_rtt = (uint64_t)((rdtsc() - t0) * tsc_ns) / CV_N;
    printf("FIBER the condvar handoff RTT: %llu ns (wait+signal_switch, 2 switches)\n", (unsigned long long)per_rtt);
    CHECK(cv_rounds == CV_N, "the condvar ping-pong: the rounds agree");

    f = tfib_spawn(f_consumer, 0, stk2, sizeof stk2);
    t0 = rdtsc();
    for (uint64_t i = 0; i < Q_N; i++) {
        tfib_mutex_lock(&qm);
        while (fq.t - fq.h >= QCAP) { tfib_mutex_unlock(&qm); tfib_cond_wait(&q_nf); tfib_mutex_lock(&qm); }
        fq.buf[fq.t++ & (QCAP - 1)] = i;
        tfib_mutex_unlock(&qm);
        tfib_cond_signal_switch(&q_ne);
    }
    tfib_join(f);
    uint64_t per_el = (uint64_t)((rdtsc() - t0) * tsc_ns) / Q_N;
    printf("FIBER SPSC (mutex+cv, one core): %llu ns/element\n", (unsigned long long)per_el);
    CHECK(q_bad == 0 && q_got == Q_N, "the fiber SPSC: 500k values in order");

    // the kernel's directed switch
    pthread_t t;
    pthread_create(&t, 0, kthr, 0);
    while (!tid_b) ;
    t0 = rdtsc();
    for (int i = 1; i <= KSW_N; i++) {
        kturn = 2 * i - 1;
        while (kturn != 2 * i) swtchto(tid_b);
    }
    uint64_t krtt = (uint64_t)((rdtsc() - t0) * tsc_ns) / KSW_N;
    pthread_join(t, 0);
    printf("KERNEL swtchto RTT (2 threads of the BSP): %llu ns\n", (unsigned long long)krtt);
    CHECK(kturn == 2 * KSW_N, "the swtchto ping-pong: the rounds agree");

    printf(fails ? "FIBER: %d FAIL\n" : "FIBER: ALL OK\n", fails);
    return fails != 0;
}
