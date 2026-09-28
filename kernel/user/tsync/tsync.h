// libtsync (PLAN-SYNC stage 1): cheap synchronisation primitives without a busy spin.
// The tiers of the waiting: 0 — a short pause spin; 1 — umonitor/umwait (Intel WAITPKG)
// or monitorx/mwaitx (AMD) right in ring 3; 2 — a futex syscall: Talaria's kernel sleeps
// with an mwait ON THIS VERY user word, so the waker's write wakes the sleeper without an
// IPI and without a syscall at the waker's side (on Linux the same code = an ordinary futex).
// All the wait functions wait exactly for a CHANGE of the 32-bit word (the WaitOnAddress style).
#ifndef TSYNC_H
#define TSYNC_H
#include <stdint.h>
#include <stdatomic.h>

void tsync_init(void); // the CPUID detection (waitpkg/mwaitx); it may be left uncalled — the detection is lazy

// Wait while *addr == expected (the tiers 0->1->2). It returns after the word has changed
// (or spuriously — the caller rechecks the predicate).
void tsync_wait(volatile uint32_t *addr, uint32_t expected);
// The same with a timeout (ns); 0 = woken/the word has changed, -1 = a timeout.
int tsync_wait_ns(volatile uint32_t *addr, uint32_t expected, uint64_t timeout_ns);
// Wake up to n sleepers on addr. THE CALLER has already changed the word! It skips the
// syscall if nobody has declared a sleep (the waiters counter is transparent for the futex tier).
void tsync_wake(volatile uint32_t *addr, int n);

// ── the mutex: a fast path of 1 CAS, contended — a futex parking (3-state) ───
typedef struct { _Atomic uint32_t state; } tsync_mutex; // 0 free / 1 locked / 2 contended
#define TSYNC_MUTEX_INIT {0}
void tsync_mutex_lock(tsync_mutex *m);
int  tsync_mutex_trylock(tsync_mutex *m); // 0 = taken
void tsync_mutex_unlock(tsync_mutex *m);

// ── the MCS mutex: a queue of the waiters, each one sleeps on ITS OWN node (a pinpoint
//    wake-up, FIFO, no thundering herd; the node lives on the waiter's stack) ──
typedef struct tsync_mcs_node {
    _Atomic(struct tsync_mcs_node *) next;
    _Atomic uint32_t locked;
} tsync_mcs_node;
typedef struct { _Atomic(tsync_mcs_node *) tail; } tsync_mcs;
#define TSYNC_MCS_INIT {0}
void tsync_mcs_lock(tsync_mcs *m, tsync_mcs_node *me);
void tsync_mcs_unlock(tsync_mcs *m, tsync_mcs_node *me);

// ── the condvar: a seq counter of the generations (the lost wakeup is closed: the seq is
//    read under the mutex, the wait waits for a change of the seq) ──────────
typedef struct { _Atomic uint32_t seq; _Atomic uint32_t waiters; } tsync_cond;
#define TSYNC_COND_INIT {0, 0}
void tsync_cond_wait(tsync_cond *c, tsync_mutex *m);
int  tsync_cond_timedwait_ns(tsync_cond *c, tsync_mutex *m, uint64_t ns); // 0/-1 a timeout
void tsync_cond_signal(tsync_cond *c);
void tsync_cond_broadcast(tsync_cond *c);

// ── an SPSC queue of u64 (the capacity is a power of two), PLAN-SYNC §5 v2.
//    The fast path is lock-free (Rigtorp: cached indices, the head/tail on different
//    lines). Empty/full — a sleep ON THE WORD OF THE INDEX: a store of the index itself
//    wakes an mwait sleeper. On Talaria (a kernel with an mwait) it is the kfast protocol:
//    a upark with the predicate "while the index < target" (the re-arming of the monitor
//    IN THE KERNEL — a hysteresis without syscalls) and a state word from the kernel
//    (1 = mwait, a store wakes it, the waker does NOT need a wake syscall; 2 = parked —
//    a futex wake is needed, the race is closed by the kernel's membarrier). The
//    producer's fast path: the write + a release store + one L1-hit load, WITHOUT a fence
//    and WITHOUT a syscall. Otherwise (Linux) it is the portable sleep-flag protocol. ──
// The layout by OWNERSHIP (no foreign stores into the partner's hot lines):
// the producer's line: the tail (the producer writes often) + c_state (the kernel writes
//   it from the consumer RARELY) + p_target (the producer writes it in the slow path);
// the consumer's line: the head (the consumer writes often) + p_state (rarely);
// the door — a separate line (a sleeping producer mwaits here: the pop stores of the head
//   do not ping its monitor, the consumer rings once at the threshold);
// the sides' local lines: the caches of the indices, rang, door_pair (read rarely).
typedef struct {
    _Atomic uint64_t tail;            // the producer's line (the low 32 bits = the consumer's wait word)
    _Atomic uint32_t c_state;         // the consumer's sleep: kfast — THE KERNEL writes it (0/1/2), portable — a flag
    _Atomic uint32_t p_target;        // the head index the producer sleeps until (the hysteresis)
    char _p0[48];
    _Atomic uint64_t head;            // the consumer's line (the low 32 bits = the producer's wait word)
    _Atomic uint32_t p_state;         // the producer's sleep
    _Atomic uint32_t c_target;        // the tail index the consumer sleeps until (it is stable while it sleeps)
    char _p1[48];
    _Atomic uint32_t p_door;          // the producer's "door" — a separate line
    char _pd[60];
    _Atomic uint32_t c_door;          // the consumer's "door" — a separate line (it sleeps here rather than on the tail)
    char _cd[60];
    uint64_t cached_tail;             // consumer-local
    uint64_t rang_target;             // consumer-local: the target of the producer's door already rung for (~0 = none)
    uint64_t c_parks, c_slow, c_slow_cyc; // consumer-local: the parkings / the entries into the slow path / the ticks in it
    uint64_t cdoor_pair[2];           // {&tail, target} for the consumer's kind2 upark (the kernel reads it)
    char _p2[8];
    uint64_t cached_head;             // producer-local
    uint64_t door_pair[2];            // {&head, target} for a kind2 upark (the kernel reads it at the parking)
    uint64_t p_parks, p_slow, p_slow_cyc; // producer-local: the parkings / the entries into the slow path / the ticks in it
    uint64_t prod_rang_target;        // producer-local: the target of the consumer's door already rung for
    char _p3[8];
    uint64_t cap_mask;
    uint64_t *buf;
    uint32_t batch;                   // the producer's hysteresis: sleep until batch free slots
    uint32_t kfast;                   // Talaria's kernel with an mwait: the state/upark protocol
} tsync_spsc;
int  tsync_spsc_init(tsync_spsc *q, uint64_t cap_pow2, uint64_t *buf);
void tsync_spsc_set_batch(tsync_spsc *q, uint32_t batch); // 1..cap (default cap/4)

// The slow paths (tsync.c): the waiting for room/data and the waking of the sleepers.
void tsync_spsc_push_wait(tsync_spsc *q, uint64_t t);   // wait for a free slot
void tsync_spsc_pop_wait(tsync_spsc *q, uint64_t h);    // wait for data
void tsync_spsc_wake_consumer_slow(tsync_spsc *q, uint32_t new_tail);
void tsync_spsc_wake_producer_slow(tsync_spsc *q, uint32_t new_head);
void tsync_spsc_wake_consumer_portable(tsync_spsc *q);
void tsync_spsc_wake_producer_portable(tsync_spsc *q);

// The fast path is static inline (a call boundary in a hot loop costs ~4x of the
// throughput: the values do not live in the registers, the store forwarding is torn).
static inline void tsync_spsc_push(tsync_spsc *q, uint64_t v) {
    uint64_t t = atomic_load_explicit(&q->tail, memory_order_relaxed);
    if (__builtin_expect(t - q->cached_head > q->cap_mask, 0)) tsync_spsc_push_wait(q, t);
    q->buf[t & q->cap_mask] = v;
    atomic_store_explicit(&q->tail, t + 1, memory_order_release); // a store = a notify (the kernel mwait/the door)
    if (__builtin_expect(q->kfast != 0, 1)) {
        uint32_t cs = atomic_load_explicit(&q->c_state, memory_order_relaxed);
        if (__builtin_expect(cs != 0, 0)) tsync_spsc_wake_consumer_slow(q, (uint32_t)(t + 1));
    } else tsync_spsc_wake_consumer_portable(q);
}
static inline uint64_t tsync_spsc_pop(tsync_spsc *q) {
    uint64_t h = atomic_load_explicit(&q->head, memory_order_relaxed);
    if (__builtin_expect(h == q->cached_tail, 0)) tsync_spsc_pop_wait(q, h);
    uint64_t v = q->buf[h & q->cap_mask];
    atomic_store_explicit(&q->head, h + 1, memory_order_release);
    if (__builtin_expect(q->kfast != 0, 1)) {
        uint32_t ps = atomic_load_explicit(&q->p_state, memory_order_relaxed);
        if (__builtin_expect(ps != 0, 0)) tsync_spsc_wake_producer_slow(q, (uint32_t)(h + 1));
    } else tsync_spsc_wake_producer_portable(q);
    return v;
}
static inline int tsync_spsc_try_push(tsync_spsc *q, uint64_t v) {
    uint64_t t = atomic_load_explicit(&q->tail, memory_order_relaxed);
    if (t - q->cached_head > q->cap_mask) {
        q->cached_head = atomic_load_explicit(&q->head, memory_order_acquire);
        if (t - q->cached_head > q->cap_mask) return -1;
    }
    q->buf[t & q->cap_mask] = v;
    atomic_store_explicit(&q->tail, t + 1, memory_order_release);
    if (__builtin_expect(q->kfast != 0, 1)) {
        uint32_t cs = atomic_load_explicit(&q->c_state, memory_order_relaxed);
        if (__builtin_expect(cs != 0, 0)) tsync_spsc_wake_consumer_slow(q, (uint32_t)(t + 1));
    } else tsync_spsc_wake_consumer_portable(q);
    return 0;
}
static inline int tsync_spsc_try_pop(tsync_spsc *q, uint64_t *v) {
    uint64_t h = atomic_load_explicit(&q->head, memory_order_relaxed);
    if (h == q->cached_tail) {
        q->cached_tail = atomic_load_explicit(&q->tail, memory_order_acquire);
        if (h == q->cached_tail) return -1;
    }
    *v = q->buf[h & q->cap_mask];
    atomic_store_explicit(&q->head, h + 1, memory_order_release);
    if (__builtin_expect(q->kfast != 0, 1)) {
        uint32_t ps = atomic_load_explicit(&q->p_state, memory_order_relaxed);
        if (__builtin_expect(ps != 0, 0)) tsync_spsc_wake_producer_slow(q, (uint32_t)(h + 1));
    } else tsync_spsc_wake_producer_portable(q);
    return 0;
}

// ── an MPSC queue of u64 (many producers -> one consumer) — the actor model:
//    an actor has many senders. A Vyukov bounded one: a producer takes a position with a
//    fetch_add and publishes the slot with a store into the per-slot seq. The consumer
//    sleeps ON THE SEQ WORD of its slot: a kind1 upark "while (i32)(seq - (h+1)) < 0" —
//    the publication of a slot (an ordinary store by the producer) wakes an mwait sleeper
//    without a syscall on either side; state==2 (parked/hlt) — a futex wake is sent ONLY
//    by the producer of that same position (c_waitpos). On Linux — the portable sleep-flag protocol + a fence.
typedef struct { _Atomic uint32_t seq; uint32_t _pad; uint64_t val; } tsync_mpsc_slot;
typedef struct {
    _Atomic uint64_t tail;        // the producers' line (a fetch_add)
    char _p0[56];
    _Atomic uint32_t c_state;     // the consumer's sleep: kfast — THE KERNEL writes it (0/1/2), portable — a flag
    _Atomic uint64_t c_waitpos;   // the position on whose slot the consumer parks
    _Atomic uint32_t p_sleepers;  // the producers sleeping on a full queue (the gate of the consumer's wake)
    char _p1[44];
    uint64_t head;                // consumer-local (the consumer only)
    uint64_t c_parks, c_slow;     // consumer-local: the statistics of the slow paths
    char _p2[40];
    uint64_t cap_mask;
    tsync_mpsc_slot *buf;
    uint32_t kfast;
} tsync_mpsc;
// buf — an array of cap_pow2 slots (it is initialised here). 0 = ok.
int  tsync_mpsc_init(tsync_mpsc *q, uint64_t cap_pow2, tsync_mpsc_slot *buf);
void tsync_mpsc_push(tsync_mpsc *q, uint64_t v);   // it blocks when it is full
uint64_t tsync_mpsc_pop(tsync_mpsc *q);            // it sleeps when it is empty
int  tsync_mpsc_try_pop(tsync_mpsc *q, uint64_t *v); // -1 = empty

// Talaria's kernel mode of waiting (0 hlt / 1 mwait / 2 mwaitx / 3 waitpkg; -1 = not Talaria).
int tsync_kmode(void);
// Wait in the kernel while (int32)(*addr - target) < 0 (a kind1 upark); the state is written by the kernel.
long tsync_upark_until(volatile uint32_t *addr, uint32_t target, volatile uint32_t *state);

#endif
