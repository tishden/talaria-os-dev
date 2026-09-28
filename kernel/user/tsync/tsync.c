// libtsync — the implementation (see tsync.h). It is portable: on Talaria the futex tier
// sleeps in the kernel with an mwait on the word itself (a write wakes it without an
// IPI/a syscall at the waker's side), on Linux it is an ordinary futex.
#define _GNU_SOURCE
#include "tsync.h"
#include <stdatomic.h>
#include <time.h>
#include <unistd.h>
#include <sys/syscall.h>
#include <stdlib.h>

#define FUTEX_WAIT 0
#define FUTEX_WAKE 1
#define TIER0_SPINS 300      // a short pause spin: it catches nanosecond races
#define TIER1_SLICE_NS 50000 // the quantum of umwait/mwaitx before a recheck

static int g_tier1; // 0 none, 1 waitpkg (umonitor/umwait), 2 AMD (monitorx/mwaitx)
// The tuning of the tier 1 (PLAN-V1.1 §2.1). It is configurable from the environment, so
// that the variants can be tried on a live stand without a rebuild:
//   TSYNC_UMWAIT_STATE=1 — C0.1 (a fast exit, a little more energy),
//                     =0 — C0.2 (deeper, the exit is longer — it used to be the default);
//   TSYNC_SLICE_NS=<ns>  — the quantum of the waiting before a recheck of the word.
#ifndef TSYNC_UMWAIT_DEFAULT
#define TSYNC_UMWAIT_DEFAULT 1        // C0.1: the latency matters more than the energy
#endif
#ifndef TSYNC_SLICE_DEFAULT
#define TSYNC_SLICE_DEFAULT 50000
#endif
static uint32_t g_umwait_state = TSYNC_UMWAIT_DEFAULT;
static uint64_t g_slice_ns = TSYNC_SLICE_DEFAULT;
static int g_kmode = -1; // Talaria's kernel: the mode of waiting (>=1 — an mwait on the word)
static int g_inited;
static void tier1_wait(volatile uint32_t *a, uint32_t v);
static void tier1_calibrate(void);
static uint32_t g_tier1_cost;  // the measured cost of a cycle of the tier 1 (ns) if it is switched off
static long futex(volatile uint32_t *a, int op, uint32_t val, const struct timespec *to);

static inline uint64_t rdtsc(void) {
    unsigned lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return (uint64_t)hi << 32 | lo;
}
static inline void cpuid2(uint32_t leaf, uint32_t sub, uint32_t *c, uint32_t *d) {
    uint32_t a, b;
    __asm__ volatile("cpuid" : "=a"(a), "=b"(b), "=c"(*c), "=d"(*d) : "a"(leaf), "c"(sub));
}

void tsync_init(void) {
    uint32_t c, d;
    { const char *e = getenv("TSYNC_UMWAIT_STATE"); if (e) g_umwait_state = (uint32_t)atoi(e); }
    { const char *e = getenv("TSYNC_SLICE_NS"); if (e) g_slice_ns = (uint64_t)atoll(e); }
    cpuid2(7, 0, &c, &d);
#ifdef TSYNC_DISABLE_TIER1
    g_tier1 = 0;                      // the diagnostics of §2.1: the tier 1 is off, we sleep in the kernel
    (void)c; (void)d;
#else
    if (c & (1u << 5)) { g_tier1 = 1; }
    else {
        cpuid2(0x80000001u, 0, &c, &d);
        if (c & (1u << 29)) g_tier1 = 2;
    }
#endif
    long m = syscall(4601); // Talaria: tsync_kmode; Linux: -1 (ENOSYS)
    g_kmode = (m >= 0 && m <= 3) ? (int)m : -1;
    g_inited = 1;
    if (g_tier1) tier1_calibrate();
}

// The self-calibration of the tier 1 (PLAN-V1.1 §2.1). The cost of one arm-and-exit of
// umonitor/umwait must be hundreds of ticks; if the platform makes it a microsecond one
// (Sapphire Rapids under KVM: a umwait in ring 3 is intercepted by the hypervisor — the
// measurement on an m7i.metal-24xl gave 119 s against 11 s for the tier 2 on one and the
// same test), the tier 1 is harmful: it does not save, it adds latency.
// Then we switch it off and sleep in the kernel at once (the tier 2 — an mwait on the
// word, whose wake-up by a store goes without a syscall at the waker's side).
static void tier1_calibrate(void) {
    // We check the MAIN property of the tier 1: can the platform really SLEEP on a
    // monitor. We arm the monitor, nobody writes into the line, we ask for a sleep until a
    // distant deadline — a healthy platform will come back at the deadline. Sapphire
    // Rapids under KVM returns control immediately (the umwait does not sleep), and the
    // loop of the waiting turns into a busy loop that takes the processor away from the
    // partner: the same tsync test took 119 s against 11 s on the tier 2 (the measurement on an m7i.metal-24xl, §2.1).
    volatile uint32_t probe = 0;
    uint64_t slept = 0;
    const int N = 5;
    for (int i = 0; i < N; i++) {
        uint64_t want = 100000;                    // the ticks (~30-50 us)
        uint64_t t0 = rdtsc(), dl = t0 + want;
        if (g_tier1 == 1) {
            __asm__ volatile("umonitor %0" :: "r"(&probe));
            __asm__ volatile("umwait %0" :: "r"(g_umwait_state),
                             "a"((uint32_t)dl), "d"((uint32_t)(dl >> 32)) : "cc");
        } else {
            __asm__ volatile("monitorx" :: "a"(&probe), "c"(0), "d"(0));
            __asm__ volatile("mwaitx" :: "a"(0xF0u), "c"(0x2u), "b"((uint32_t)want));
        }
        uint64_t el = rdtsc() - t0;
        if (el * 3 >= want) slept++;               // we slept at least a third of the quantum
    }
    uint64_t per = slept;
    if (per * 2 < (uint64_t)N) {                   // it slept in less than half of the probes
        g_tier1_cost = g_tier1;                    // remember which tier 1 we rejected
        g_tier1 = 0;                               // a fallback onto the tier 2 (a sleep in the kernel)
    }
}
int tsync_kmode(void) { if (!g_inited) tsync_init(); return g_kmode; }
/// A non-zero value — the tier 1 (1 waitpkg / 2 mwaitx) has been rejected by the
/// self-calibration: the platform does not sleep on a monitor, the waiting goes straight into the kernel (the tier 2).
uint32_t tsync_tier1_disabled(void) { if (!g_inited) tsync_init(); return g_tier1_cost; }

// A kind1 upark: wait while (i32)(*addr - target) < 0. The tier 0 -> the tier 1 (ring 3,
// a monitor on the word — the state stays 0, the waker needs nothing) -> the kernel
// (the state is written by the kernel: 1 mwait / 2 parked).
long tsync_upark_until(volatile uint32_t *addr, uint32_t target, volatile uint32_t *state) {
    if (!g_inited) tsync_init();
    // An adaptive pre-spin (poll-then-sleep): under a real pressure of data the partner
    // moves the index on within microseconds — we catch up without a syscall and without
    // the sawtooth "one sleeps, the other works"; when it is idle we go to sleep quickly.
    for (int i = 0; i < 1200; i++) {
        if ((int32_t)(*addr - target) >= 0) return 0;
        __asm__ volatile("pause");
    }
    if (g_tier1) {
        while ((int32_t)(*addr - target) < 0) tier1_wait(addr, *addr);
        return 0;
    }
    return syscall(4602, addr, target, 0, 1, state);
}

static long futex(volatile uint32_t *a, int op, uint32_t val, const struct timespec *to) {
    return syscall(SYS_futex, a, op, val, to, 0, 0);
}

// A kind2 upark (the door): sleep on the word door while (i32)(*idx - target) < 0.
// pair = {&idx, target} for a recheck of the predicate BY THE KERNEL (after a membarrier —
// the anti-deadlock "the final store of the index was not visible"). The pre-spin is by the index.
static long tsync_upark_door(volatile uint32_t *door, volatile uint32_t *idx,
                             uint32_t target, uint64_t *pair, volatile uint32_t *state) {
    for (int i = 0; i < 1200; i++) {
        if ((int32_t)(*idx - target) >= 0) return 0;
        __asm__ volatile("pause");
    }
    if (g_tier1) { // the tier 1: a monitor on the index (ring 3, without the kernel)
        while ((int32_t)(*idx - target) < 0) tier1_wait(idx, *idx);
        return 0;
    }
    uint32_t d = *door;
    if ((int32_t)(*idx - target) >= 0) return 0;
    return syscall(4602, door, d, 0, 2, state, pair);
}

// The tier 1: arm the monitor -> recheck -> fall asleep until a write into the line.
// The arm/check race is closed by the order (a write between them raises the armed flag).
static void tier1_wait(volatile uint32_t *a, uint32_t v) {
    if (g_tier1 == 1) {
        __asm__ volatile("umonitor %0" :: "r"(a));
        if (*a != v) return;
        uint64_t dl = rdtsc() + g_slice_ns * 3; // ~a quantum (roughly, the ticks ~ ns)
        // The bit 0 of the operand chooses the state: 1 = C0.1 (a fast wake-up),
        // 0 = C0.2 (deeper, but the exit is longer — on an SPR that gave "sleeping" bursts).
        __asm__ volatile("umwait %0" :: "r"(g_umwait_state), "a"((uint32_t)dl), "d"((uint32_t)(dl >> 32)) : "cc");
    } else {
        __asm__ volatile("monitorx" :: "a"(a), "c"(0), "d"(0));
        if (*a != v) return;
        __asm__ volatile("mwaitx" :: "a"(0xF0u), "c"(0x2u), "b"((uint32_t)(g_slice_ns * 3))); // C1, with the EBX timeout
    }
}

void tsync_wait(volatile uint32_t *addr, uint32_t expected) {
    if (!g_inited) tsync_init();
    for (int i = 0; i < TIER0_SPINS; i++) {         // the tier 0
        if (*addr != expected) return;
        __asm__ volatile("pause");
    }
    while (*addr == expected) {
        if (g_tier1) tier1_wait(addr, expected);    // the tier 1 (ring 3, without a syscall)
        else futex(addr, FUTEX_WAIT, expected, 0);  // the tier 2 (Talaria: an mwait in the kernel)
    }
}

int tsync_wait_ns(volatile uint32_t *addr, uint32_t expected, uint64_t timeout_ns) {
    if (!g_inited) tsync_init();
    struct timespec t0, now;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    uint64_t dl = (uint64_t)t0.tv_sec * 1000000000ull + t0.tv_nsec + timeout_ns;
    for (int i = 0; i < TIER0_SPINS; i++) {
        if (*addr != expected) return 0;
        __asm__ volatile("pause");
    }
    while (*addr == expected) {
        clock_gettime(CLOCK_MONOTONIC, &now);
        uint64_t n = (uint64_t)now.tv_sec * 1000000000ull + now.tv_nsec;
        if (n >= dl) return -1;
        if (g_tier1) { tier1_wait(addr, expected); continue; }
        uint64_t left = dl - n;
        struct timespec to = { .tv_sec = left / 1000000000ull, .tv_nsec = left % 1000000000ull };
        futex(addr, FUTEX_WAIT, expected, &to);
    }
    return 0;
}

void tsync_wake(volatile uint32_t *addr, int n) {
    // The caller has already changed the word: the mwait/umwait sleepers have woken from
    // the write itself; the futex syscall finishes off the ones sleeping in hlt/on the BSP.
    atomic_thread_fence(memory_order_seq_cst);
    futex(addr, FUTEX_WAKE, (uint32_t)n, 0);
}

// ── mutex (Drepper 3-state) ──────────────────────────────────────────────────
void tsync_mutex_lock(tsync_mutex *m) {
    uint32_t z = 0;
    if (atomic_compare_exchange_strong_explicit(&m->state, &z, 1, memory_order_acquire, memory_order_relaxed)) return;
    uint32_t c = z;
    if (c != 2) c = atomic_exchange_explicit(&m->state, 2, memory_order_acquire);
    while (c) {
        tsync_wait((volatile uint32_t *)&m->state, 2);
        c = atomic_exchange_explicit(&m->state, 2, memory_order_acquire);
    }
}
int tsync_mutex_trylock(tsync_mutex *m) {
    uint32_t z = 0;
    return atomic_compare_exchange_strong_explicit(&m->state, &z, 1, memory_order_acquire, memory_order_relaxed) ? 0 : -1;
}
void tsync_mutex_unlock(tsync_mutex *m) {
    if (atomic_exchange_explicit(&m->state, 0, memory_order_release) == 2)
        tsync_wake((volatile uint32_t *)&m->state, 1);
}

// ── MCS: everyone waits on their own node; a wake-up syscall only if the waiter has
//    really fallen asleep (locked: 1 = waits/spins, 2 = sleeps in the kernel) ──
void tsync_mcs_lock(tsync_mcs *m, tsync_mcs_node *me) {
    atomic_store_explicit(&me->next, 0, memory_order_relaxed);
    atomic_store_explicit(&me->locked, 1, memory_order_relaxed);
    tsync_mcs_node *prev = atomic_exchange_explicit(&m->tail, me, memory_order_acq_rel);
    if (!prev) return;
    atomic_store_explicit(&prev->next, me, memory_order_release);
    for (int i = 0; i < TIER0_SPINS; i++) {
        if (atomic_load_explicit(&me->locked, memory_order_acquire) == 0) return;
        __asm__ volatile("pause");
    }
    uint32_t one = 1;
    while (atomic_load_explicit(&me->locked, memory_order_acquire) != 0) {
        if (atomic_compare_exchange_strong_explicit(&me->locked, &one, 2, memory_order_acquire, memory_order_relaxed))
            tsync_wait((volatile uint32_t *)&me->locked, 2);
        one = 1;
    }
}
void tsync_mcs_unlock(tsync_mcs *m, tsync_mcs_node *me) {
    tsync_mcs_node *nx = atomic_load_explicit(&me->next, memory_order_acquire);
    if (!nx) {
        tsync_mcs_node *exp = me;
        if (atomic_compare_exchange_strong_explicit(&m->tail, &exp, 0, memory_order_release, memory_order_relaxed)) return;
        while (!(nx = atomic_load_explicit(&me->next, memory_order_acquire))) __asm__ volatile("pause");
    }
    if (atomic_exchange_explicit(&nx->locked, 0, memory_order_release) == 2)
        tsync_wake((volatile uint32_t *)&nx->locked, 1);
}

// ── the condvar (the seq protocol: a lost wakeup is excluded even without a syscall at
//    the signaller's side when nobody is waiting) ──────────────────────────
void tsync_cond_wait(tsync_cond *c, tsync_mutex *m) {
    uint32_t s = atomic_load_explicit(&c->seq, memory_order_relaxed);
    atomic_fetch_add_explicit(&c->waiters, 1, memory_order_seq_cst);
    tsync_mutex_unlock(m);
    tsync_wait((volatile uint32_t *)&c->seq, s);
    atomic_fetch_sub_explicit(&c->waiters, 1, memory_order_seq_cst);
    tsync_mutex_lock(m);
}
int tsync_cond_timedwait_ns(tsync_cond *c, tsync_mutex *m, uint64_t ns) {
    uint32_t s = atomic_load_explicit(&c->seq, memory_order_relaxed);
    atomic_fetch_add_explicit(&c->waiters, 1, memory_order_seq_cst);
    tsync_mutex_unlock(m);
    int r = tsync_wait_ns((volatile uint32_t *)&c->seq, s, ns);
    atomic_fetch_sub_explicit(&c->waiters, 1, memory_order_seq_cst);
    tsync_mutex_lock(m);
    return r;
}
void tsync_cond_signal(tsync_cond *c) {
    atomic_fetch_add_explicit(&c->seq, 1, memory_order_seq_cst); // the increment itself wakes the mwait sleepers
    if (atomic_load_explicit(&c->waiters, memory_order_seq_cst))
        tsync_wake((volatile uint32_t *)&c->seq, 1);
}
void tsync_cond_broadcast(tsync_cond *c) {
    atomic_fetch_add_explicit(&c->seq, 1, memory_order_seq_cst);
    if (atomic_load_explicit(&c->waiters, memory_order_seq_cst))
        tsync_wake((volatile uint32_t *)&c->seq, 1u << 30);
}

// ── SPSC v2 ──────────────────────────────────────────────────────────────────
int tsync_spsc_init(tsync_spsc *q, uint64_t cap_pow2, uint64_t *buf) {
    if (!cap_pow2 || (cap_pow2 & (cap_pow2 - 1))) return -1;
    if (!g_inited) tsync_init();
    atomic_store(&q->tail, 0); atomic_store(&q->head, 0);
    atomic_store(&q->c_state, 0); atomic_store(&q->p_state, 0);
    atomic_store(&q->p_target, 0); atomic_store(&q->c_target, 0);
    atomic_store(&q->p_door, 0); atomic_store(&q->c_door, 0);
    q->door_pair[0] = (uint64_t)(uintptr_t)&q->head; q->door_pair[1] = 0;
    q->cdoor_pair[0] = (uint64_t)(uintptr_t)&q->tail; q->cdoor_pair[1] = 0;
    q->cached_tail = 0; q->cached_head = 0;
    q->rang_target = ~0ull; q->prod_rang_target = ~0ull;
    q->p_parks = q->p_slow = q->c_parks = q->c_slow = 0; q->p_slow_cyc = q->c_slow_cyc = 0;
    q->cap_mask = cap_pow2 - 1; q->buf = buf;
    q->batch = cap_pow2 / 4 ? cap_pow2 / 4 : 1;
    q->kfast = (g_kmode >= 1) && !g_tier1; // Talaria's kernel with an mwait; with a user wait the tier 1 closes the races itself
    return 0;
}
void tsync_spsc_set_batch(tsync_spsc *q, uint32_t batch) {
    uint64_t cap = q->cap_mask + 1;
    q->batch = batch < 1 ? 1 : (batch > cap ? (uint32_t)cap : batch);
}

// ── the slow paths of the SPSC (the fast path is static inline in tsync.h) ───

// The waking of a sleeping CONSUMER (it is called from the inline one when c_state != 0).
// The consumer sleeps on ITS OWN door (the tail line is never monitored); the ringing
// happens once for its target (deadlock-free: a repeated parking = a new target).
void tsync_spsc_wake_consumer_slow(tsync_spsc *q, uint32_t new_tail) {
    uint32_t s = atomic_load_explicit(&q->c_state, memory_order_relaxed);
    if (s == 0) return;
    uint32_t tg = atomic_load_explicit(&q->c_target, memory_order_relaxed);
    if ((int32_t)(new_tail - tg) >= 0 && q->prod_rang_target != (uint64_t)tg) {
        q->prod_rang_target = (uint64_t)tg;
        atomic_fetch_add_explicit(&q->c_door, 1, memory_order_seq_cst);
        if (s == 2) tsync_wake((volatile uint32_t *)&q->c_door, 1);
    }
}
void tsync_spsc_wake_consumer_portable(tsync_spsc *q) {
    atomic_thread_fence(memory_order_seq_cst); // a StoreLoad for the sleep flag
    if (atomic_load_explicit(&q->c_state, memory_order_relaxed)) tsync_wake((volatile uint32_t *)&q->tail, 1);
}

// The waking of a sleeping PRODUCER (it is called from the inline one when p_state != 0).
void tsync_spsc_wake_producer_slow(tsync_spsc *q, uint32_t new_head) {
    uint32_t s = atomic_load_explicit(&q->p_state, memory_order_relaxed);
    if (s == 0) return;
    uint32_t tg = atomic_load_explicit(&q->p_target, memory_order_relaxed);
    if ((int32_t)(new_head - tg) >= 0 && q->rang_target != (uint64_t)tg) {
        q->rang_target = (uint64_t)tg;
        atomic_fetch_add_explicit(&q->p_door, 1, memory_order_seq_cst);
        if (s == 2) tsync_wake((volatile uint32_t *)&q->p_door, 1);
    }
}
void tsync_spsc_wake_producer_portable(tsync_spsc *q) {
    atomic_thread_fence(memory_order_seq_cst);
    if (atomic_load_explicit(&q->p_state, memory_order_relaxed)) tsync_wake((volatile uint32_t *)&q->head, 1);
}

// The waiting for a free slot (a full queue): a hysteresis through the door (kind2).
void tsync_spsc_push_wait(tsync_spsc *q, uint64_t t) {
    q->cached_head = atomic_load_explicit(&q->head, memory_order_acquire);
    if (t - q->cached_head <= q->cap_mask) return;
    q->p_slow++;
    uint64_t slow0 = rdtsc();
    while (t - q->cached_head > q->cap_mask) {
        if (q->kfast) {
            uint32_t target = (uint32_t)(t - q->cap_mask - 1 + q->batch);
            atomic_store_explicit(&q->p_target, target, memory_order_release);
            q->door_pair[1] = target;
            q->p_parks++;
            tsync_upark_door((volatile uint32_t *)&q->p_door,
                             (volatile uint32_t *)&q->head, target,
                             q->door_pair, (volatile uint32_t *)&q->p_state);
        } else {
            atomic_store_explicit(&q->p_state, 1, memory_order_seq_cst);
            uint64_t h2 = atomic_load_explicit(&q->head, memory_order_acquire);
            if (t - h2 > q->cap_mask)
                tsync_wait((volatile uint32_t *)&q->head, (uint32_t)h2);
            atomic_store_explicit(&q->p_state, 0, memory_order_relaxed);
        }
        q->cached_head = atomic_load_explicit(&q->head, memory_order_acquire);
    }
    q->p_slow_cyc += rdtsc() - slow0;
}

// ── MPSC (a Vyukov bounded one, the actor model: many senders -> one actor) ──
int tsync_mpsc_init(tsync_mpsc *q, uint64_t cap_pow2, tsync_mpsc_slot *buf) {
    if (!cap_pow2 || (cap_pow2 & (cap_pow2 - 1))) return -1;
    if (!g_inited) tsync_init();
    atomic_store(&q->tail, 0);
    atomic_store(&q->c_state, 0);
    atomic_store(&q->c_waitpos, 0);
    atomic_store(&q->p_sleepers, 0);
    q->head = 0; q->c_parks = q->c_slow = 0;
    q->cap_mask = cap_pow2 - 1; q->buf = buf;
    for (uint64_t i = 0; i < cap_pow2; i++) { atomic_store(&buf[i].seq, (uint32_t)i); buf[i].val = 0; }
    q->kfast = (g_kmode >= 1) && !g_tier1;
    return 0;
}

// The producer: a fetch_add of the position; the publication of the slot with a store of
// seq = t+1 (on Talaria this store itself wakes an mwait-sleeping consumer; state==2 — a
// futex wake is sent only by the producer of the position c_waitpos, the race is closed by the kernel's membarrier).
void tsync_mpsc_push(tsync_mpsc *q, uint64_t v) {
    uint64_t t = atomic_fetch_add_explicit(&q->tail, 1, memory_order_relaxed);
    tsync_mpsc_slot *s = &q->buf[t & q->cap_mask];
    uint32_t sq = atomic_load_explicit(&s->seq, memory_order_acquire);
    if (__builtin_expect((int32_t)(sq - (uint32_t)t) < 0, 0)) {
        // A full queue: we sleep on the seq word of OUR slot until the consumer frees it.
        // The timeout of 1 ms is an insurance against the StoreLoad race on Linux (a fence
        // in the fast path of the pop is not needed; on Talaria the consumer's store wakes the kernel mwait itself).
        atomic_fetch_add_explicit(&q->p_sleepers, 1, memory_order_seq_cst);
        while ((int32_t)((sq = atomic_load_explicit(&s->seq, memory_order_acquire)) - (uint32_t)t) < 0)
            tsync_wait_ns((volatile uint32_t *)&s->seq, sq, 1000000ull);
        atomic_fetch_sub_explicit(&q->p_sleepers, 1, memory_order_seq_cst);
    }
    s->val = v;
    atomic_store_explicit(&s->seq, (uint32_t)t + 1, memory_order_release); // the publication = a wake
    // The fence is in the kfast branch too: it closes the StoreLoad "the store of the seq
    // / the read of c_state" ON THE PRODUCER'S SIDE (~20 ticks), so the consumer parks
    // with kind1|4 — WITHOUT the kernel's membarrier (7.5 us of IPI ack per parking; on a
    // crowded BSP that was also an IPI storm over the neighbouring APs' mwait).
    atomic_thread_fence(memory_order_seq_cst);
    if (__builtin_expect(q->kfast != 0, 1)) {
        if (__builtin_expect(atomic_load_explicit(&q->c_state, memory_order_relaxed) == 2, 0)
            && atomic_load_explicit(&q->c_waitpos, memory_order_relaxed) == t)
            tsync_wake((volatile uint32_t *)&s->seq, 1);
    } else {
        if (atomic_load_explicit(&q->c_state, memory_order_relaxed))
            tsync_wake((volatile uint32_t *)&s->seq, 1);
    }
}

// The consumer: FIFO by the positions; a sleep on the seq word of the current slot (a
// kind1 upark: "while (i32)(seq - (h+1)) < 0", the re-arming of the monitor in the kernel).
uint64_t tsync_mpsc_pop(tsync_mpsc *q) {
    uint64_t h = q->head;
    tsync_mpsc_slot *s = &q->buf[h & q->cap_mask];
    uint32_t want = (uint32_t)h + 1;
    uint32_t sq = atomic_load_explicit(&s->seq, memory_order_acquire);
    if (__builtin_expect((int32_t)(sq - want) < 0, 0)) {
        q->c_slow++;
        // An adaptive pre-spin (poll-then-sleep): under a stream the next slot is
        // published within hundreds of ns — we catch it without a flag/a syscall
        // (otherwise every element = an mfence+a flag at the consumer and a wake SYSCALL at the producer).
        for (int i = 0; i < 1200; i++) {
            if ((int32_t)(atomic_load_explicit(&s->seq, memory_order_acquire) - want) >= 0)
                goto got;
            __asm__ volatile("pause");
        }
        if (q->kfast) {
            atomic_store_explicit(&q->c_waitpos, h, memory_order_release);
            q->c_parks++;
            while ((int32_t)(atomic_load_explicit(&s->seq, memory_order_acquire) - want) < 0) {
                // the pre-spin as in tsync_upark_until, but kind = 1|4: the producer
                // fences by itself — the kernel's membarrier at the parking is not needed
                int spun = 0;
                for (int i = 0; i < 1200; i++) {
                    if ((int32_t)(atomic_load_explicit(&s->seq, memory_order_acquire) - want) >= 0) { spun = 1; break; }
                    __asm__ volatile("pause");
                }
                if (!spun)
                    syscall(4602, &s->seq, want, 0, 1 | 4, &q->c_state);
            }
        } else {
            while ((int32_t)((sq = atomic_load_explicit(&s->seq, memory_order_acquire)) - want) < 0) {
                atomic_store_explicit(&q->c_state, 1, memory_order_seq_cst);
                uint32_t s2 = atomic_load_explicit(&s->seq, memory_order_acquire); // a recheck after the flag
                if ((int32_t)(s2 - want) < 0) tsync_wait((volatile uint32_t *)&s->seq, s2);
                atomic_store_explicit(&q->c_state, 0, memory_order_relaxed);
            }
        }
    }
got:;
    uint64_t v = s->val;
    atomic_store_explicit(&s->seq, (uint32_t)(h + q->cap_mask + 1), memory_order_release); // the slot is free
    q->head = h + 1;
    if (__builtin_expect(atomic_load_explicit(&q->p_sleepers, memory_order_relaxed) != 0, 0))
        tsync_wake((volatile uint32_t *)&s->seq, 1u << 30);
    return v;
}

int tsync_mpsc_try_pop(tsync_mpsc *q, uint64_t *v) {
    uint64_t h = q->head;
    tsync_mpsc_slot *s = &q->buf[h & q->cap_mask];
    uint32_t sq = atomic_load_explicit(&s->seq, memory_order_acquire);
    if ((int32_t)(sq - ((uint32_t)h + 1)) < 0) return -1;
    *v = s->val;
    atomic_store_explicit(&s->seq, (uint32_t)(h + q->cap_mask + 1), memory_order_release);
    q->head = h + 1;
    if (__builtin_expect(atomic_load_explicit(&q->p_sleepers, memory_order_relaxed) != 0, 0))
        tsync_wake((volatile uint32_t *)&s->seq, 1u << 30);
    return 0;
}

// The waiting for data (an empty queue): a sleep on the consumer's door (kind2).
void tsync_spsc_pop_wait(tsync_spsc *q, uint64_t h) {
    q->cached_tail = atomic_load_explicit(&q->tail, memory_order_acquire);
    if (h != q->cached_tail) return;
    q->c_slow++;
    uint64_t slow0 = rdtsc();
    while (h == q->cached_tail) {
        if (q->kfast) {
            uint32_t target = (uint32_t)(h + 1);
            atomic_store_explicit(&q->c_target, target, memory_order_release);
            q->cdoor_pair[1] = target;
            q->c_parks++;
            tsync_upark_door((volatile uint32_t *)&q->c_door,
                             (volatile uint32_t *)&q->tail, target,
                             q->cdoor_pair, (volatile uint32_t *)&q->c_state);
        } else {
            atomic_store_explicit(&q->c_state, 1, memory_order_seq_cst);
            uint64_t t2 = atomic_load_explicit(&q->tail, memory_order_acquire);
            if (h == t2)
                tsync_wait((volatile uint32_t *)&q->tail, (uint32_t)t2);
            atomic_store_explicit(&q->c_state, 0, memory_order_relaxed);
        }
        q->cached_tail = atomic_load_explicit(&q->tail, memory_order_acquire);
    }
    q->c_slow_cyc += rdtsc() - slow0;
}
