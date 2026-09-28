// TSYNCTEST (PLAN-SYNC stage 1): the correctness of libtsync — a mutex/MCS stress on two
// cores, a condvar ping-pong (the order, the absence of lost wake-ups), a timedwait (the
// BSP's tick and an AP's TSC deadline), SPSC (the order, full/empty, both cores).
// The build: musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -o tsynctest tsynctest.c tsync.c
#define _GNU_SOURCE
#include "tsync.h"
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sched.h>
#include <sys/syscall.h>

static void to_ap(void) { cpu_set_t s; CPU_ZERO(&s); CPU_SET(1, &s); syscall(SYS_sched_setaffinity, 0, sizeof(s), &s); }
static uint64_t now_ns(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec; }

static int fails;
#define CHECK(cond, name) do { if (cond) printf("  ok  %s\n", name); else { printf("  FAIL %s\n", name); fails++; } } while (0)

// 1-2: the mutex / MCS stress
#define STRESS 200000
static tsync_mutex mu = TSYNC_MUTEX_INIT;
static tsync_mcs mcs = TSYNC_MCS_INIT;
static volatile uint64_t g1, g2;
static void *w_mutex(void *a) { if (a) to_ap(); for (int i = 0; i < STRESS; i++) { tsync_mutex_lock(&mu); g1++; tsync_mutex_unlock(&mu); } return 0; }
static void *w_mcs(void *a) { if (a) to_ap(); tsync_mcs_node n; for (int i = 0; i < STRESS; i++) { tsync_mcs_lock(&mcs, &n); g2++; tsync_mcs_unlock(&mcs, &n); } return 0; }

// 3: a condvar ping-pong with a strict order
#define ROUNDS 20000
static tsync_mutex pm = TSYNC_MUTEX_INIT;
static tsync_cond pc = TSYNC_COND_INIT;
static int turn, order_bad;
static void *w_pong(void *a) {
    if (a) to_ap();
    for (int i = 1; i <= ROUNDS; i++) {
        tsync_mutex_lock(&pm);
        while (turn != 2 * i - 1) tsync_cond_wait(&pc, &pm);
        if (turn != 2 * i - 1) order_bad++;
        turn = 2 * i;
        tsync_cond_signal(&pc);
        tsync_mutex_unlock(&pm);
    }
    return 0;
}

// 4-5: SPSC
static tsync_spsc q;
static uint64_t qbuf[64]; // a small capacity: we drive both the full and the empty one
static volatile uint64_t spsc_bad, spsc_got;
#define NELEM 300000
static void *w_consume(void *a) {
    if (a) to_ap();
    for (uint64_t i = 0; i < NELEM; i++) {
        uint64_t v = tsync_spsc_pop(&q);
        if (v != i) spsc_bad++;
        spsc_got++;
    }
    return 0;
}

// 6: broadcast — two waiters (a BSP thread and an AP thread) both wake up
static tsync_mutex bm = TSYNC_MUTEX_INIT;
static tsync_cond bc = TSYNC_COND_INIT;
static volatile int b_go, b_woken;
static void *w_bcast(void *a) {
    if (a) to_ap();
    tsync_mutex_lock(&bm);
    while (!b_go) tsync_cond_wait(&bc, &bm);
    __sync_fetch_and_add(&b_woken, 1);
    tsync_mutex_unlock(&bm);
    return 0;
}

// 7: the portable SPSC protocol (kfast=0 — the Linux branch: a sleep flag + a fence + a futex)
static void *w_consume_portable(void *a) {
    if (a) to_ap();
    for (uint64_t i = 0; i < NELEM / 3; i++) {
        uint64_t v = tsync_spsc_pop(&q);
        if (v != i) spsc_bad++;
        spsc_got++;
    }
    return 0;
}

// 8: a timedwait on an AP (a TSC deadline)
static volatile uint32_t never;
static volatile long ap_to_ret = -100; static volatile uint64_t ap_to_ns;
static void *w_timed(void *a) {
    (void)a; to_ap();
    uint64_t t0 = now_ns();
    ap_to_ret = tsync_wait_ns(&never, 0, 50 * 1000 * 1000ull);
    ap_to_ns = now_ns() - t0;
    return 0;
}

// 11-12: MPSC (Vyukov, the actor model) — 2 producers -> 1 consumer; the order within
// each producer is strict, the sum agrees; cross-core and portable.
static tsync_mpsc mq;
static tsync_mpsc_slot mqbuf[64];
#define MELEM 50000ull
static volatile uint64_t mpsc_bad, mpsc_got;
static void *w_mproduce(void *a) {
    uint64_t id = (uint64_t)a;
    for (uint64_t i = 0; i < MELEM; i++) tsync_mpsc_push(&mq, id << 32 | i);
    return 0;
}
static void *w_mconsume(void *a) {
    if (a) to_ap();
    uint64_t next[2] = {0, 0};
    for (uint64_t i = 0; i < 2 * MELEM; i++) {
        uint64_t v = tsync_mpsc_pop(&mq);
        uint64_t id = v >> 32;
        if (id > 1 || (v & 0xffffffffull) != next[id]++) mpsc_bad++;
        mpsc_got++;
    }
    return 0;
}

int main(void) {
    tsync_init();
    pthread_t t;
    uint64_t tt0 = now_ns();
#define PH(name) dprintf(1, "phase %s +%llu ms\n", name, (unsigned long long)((now_ns() - tt0) / 1000000))
    PH("start");

    pthread_create(&t, 0, w_mutex, (void *)1);
    for (int i = 0; i < STRESS; i++) { tsync_mutex_lock(&mu); g1++; tsync_mutex_unlock(&mu); }
    pthread_join(t, 0);
    CHECK(g1 == 2 * STRESS, "mutex: 2 threads x 2 cores, the counter agrees"); PH("mutex-done");

    pthread_create(&t, 0, w_mcs, (void *)1);
    { tsync_mcs_node n; for (int i = 0; i < STRESS; i++) { tsync_mcs_lock(&mcs, &n); g2++; tsync_mcs_unlock(&mcs, &n); } }
    pthread_join(t, 0);
    CHECK(g2 == 2 * STRESS, "mcs: 2 threads x 2 cores, the counter agrees"); PH("mcs-done");

    pthread_create(&t, 0, w_pong, (void *)1);
    for (int i = 1; i <= ROUNDS; i++) {
        tsync_mutex_lock(&pm);
        turn = 2 * i - 1;
        tsync_cond_signal(&pc);
        while (turn != 2 * i) tsync_cond_wait(&pc, &pm);
        tsync_mutex_unlock(&pm);
    }
    pthread_join(t, 0);
    CHECK(order_bad == 0 && turn == 2 * ROUNDS, "condvar: a ping-pong of 20k rounds, the order is strict, the wakeups are not lost"); PH("cond-done");

    tsync_spsc_init(&q, 64, qbuf);
    pthread_create(&t, 0, w_consume, (void *)1); // the consumer on an AP
    for (uint64_t i = 0; i < NELEM; i++) tsync_spsc_push(&q, i);
    pthread_join(t, 0);
    CHECK(spsc_bad == 0 && spsc_got == NELEM, "spsc cross-core: 300k values in order (full/empty)");

    tsync_spsc_init(&q, 64, qbuf); spsc_got = 0;
    pthread_create(&t, 0, w_consume, (void *)0); // same-core
    for (uint64_t i = 0; i < NELEM; i++) tsync_spsc_push(&q, i);
    pthread_join(t, 0);
    CHECK(spsc_bad == 0 && spsc_got == NELEM, "spsc same-core: 300k values in order");

    tsync_spsc_init(&q, 2, qbuf); spsc_got = 0; // the capacity 2: every push/pop crosses the full/empty boundaries
    pthread_create(&t, 0, w_consume, (void *)1);
    for (uint64_t i = 0; i < NELEM; i++) tsync_spsc_push(&q, i);
    pthread_join(t, 0);
    CHECK(spsc_bad == 0 && spsc_got == NELEM, "spsc cap=2 cross-core: the hysteresis at the boundaries, the order is intact");

    { // broadcast: both waiting threads (the BSP and an AP) must wake from one signal
        pthread_t t2;
        pthread_create(&t, 0, w_bcast, (void *)0);
        pthread_create(&t2, 0, w_bcast, (void *)1);
        while (__atomic_load_n(&bc.waiters, __ATOMIC_SEQ_CST) != 2) sched_yield();
        tsync_mutex_lock(&bm);
        b_go = 1;
        tsync_cond_broadcast(&bc);
        tsync_mutex_unlock(&bm);
        pthread_join(t, 0); pthread_join(t2, 0);
        CHECK(b_woken == 2, "broadcast: both waiters (the BSP and the AP) woke from one signal");
    }

    tsync_spsc_init(&q, 64, qbuf); spsc_got = 0; spsc_bad = 0;
    q.kfast = 0; // we force the PORTABLE protocol (the Linux branch: a fence + a sleep flag + a futex)
    pthread_create(&t, 0, w_consume_portable, (void *)1);
    for (uint64_t i = 0; i < NELEM / 3; i++) tsync_spsc_push(&q, i);
    pthread_join(t, 0);
    CHECK(spsc_bad == 0 && spsc_got == NELEM / 3, "spsc the portable protocol (kfast=0, the Linux branch): 100k in order");

    { // MPSC: 2 producers (the BSP) -> a consumer on an AP; a small capacity drives the full one
        pthread_t t2;
        tsync_mpsc_init(&mq, 64, mqbuf);
        pthread_create(&t, 0, w_mconsume, (void *)1);
        pthread_create(&t2, 0, w_mproduce, (void *)1);
        w_mproduce((void *)0);
        pthread_join(t2, 0); pthread_join(t, 0);
        CHECK(mpsc_bad == 0 && mpsc_got == 2 * MELEM, "mpsc cross-core: 2 producers, 100k, the order in each is strict");

        tsync_mpsc_init(&mq, 64, mqbuf); mpsc_got = 0; mpsc_bad = 0;
        mq.kfast = 0; // the PORTABLE protocol (the Linux branch: a fence + a sleep flag + a futex)
        pthread_create(&t, 0, w_mconsume, (void *)0);
        pthread_create(&t2, 0, w_mproduce, (void *)1);
        w_mproduce((void *)0);
        pthread_join(t2, 0); pthread_join(t, 0);
        CHECK(mpsc_bad == 0 && mpsc_got == 2 * MELEM, "mpsc the portable protocol (kfast=0): 100k, the order is intact");
    }

    uint64_t t0 = now_ns();
    int r = tsync_wait_ns(&never, 0, 50 * 1000 * 1000ull);
    uint64_t dt = now_ns() - t0;
    CHECK(r == -1 && dt > 40ull * 1000 * 1000 && dt < 1000ull * 1000 * 1000, "timedwait BSP: the timeout of 50 ms has fired");

    pthread_create(&t, 0, w_timed, 0);
    pthread_join(t, 0);
    CHECK(ap_to_ret == -1 && ap_to_ns > 40ull * 1000 * 1000 && ap_to_ns < 1000ull * 1000 * 1000, "timedwait AP: the TSC-deadline timeout of 50 ms has fired");

    printf(fails ? "TSYNC: %d FAIL\n" : "TSYNC: ALL OK\n", fails);
    return fails != 0;
}
