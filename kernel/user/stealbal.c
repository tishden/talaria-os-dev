// STEALBAL: the stealing SEPARATES two computing threads that have settled on one core —
// with a live pool of SLEEPING threads alongside.
//
// The gate closes two defects of the selection found on ClickHouse (session 25), and both
// are visible only together with the sleeping ones:
//
//  1. the balance of the stealing was counted by the number of ASSIGNED tasks. In a thread
//     pool most of them sleep, and a core with twelve sleeping ones looked more loaded
//     than a core with two computing ones — the stealing retreated "for the sake of the
//     balance" that was not there. Hence there are DELIBERATELY more sleeping ones here than cores;
//  2. the cooling threshold ("the victim must have stood without a core for a whole slice")
//     rejected exactly those for whose sake the stealing exists: two ready threads on one
//     core replace each other EVERY slice and therefore never cool down.
//
// The skew is created by the rule `talaria.pin=*:1` (all the nameless threads onto one
// core; a rule by a pattern does not pin them down, they may be moved). The periodic
// balancing is switched off by `talaria.nobalance=1`: otherwise it is unclear who exactly
// did the resettling — and the gate is about the STEALING.
//
// TWO facts are asserted rather than one: (a) the computing threads have moved apart onto
// different cores and (b) both have finished counting. "They moved apart" alone is not
// enough — a thread that did not reach the end is also "on another core".
//
// The run: talaria.test=stealbal talaria.smp=4 talaria.pin=*:1 talaria.nobalance=1
// The build: make -C kernel/user stealbal
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include <sys/syscall.h>

#define FUTEX_WAIT 0    /* without linux/futex.h: it is not there in the gate's musl environment */

#define NW 2                    /* the computing ones — one per "spare" core */
#define NS 12                   /* the sleeping ones: more of them are assigned than there are cores */
#define NCORE 4
#define ITERS 200000000ULL      /* ~1 s per thread: the stealing needs dozens of slices */

struct slot { volatile uint64_t v; char pad[56]; };
static struct slot ctr[NW] __attribute__((aligned(64)));
static volatile unsigned cpu0[NW], cpu1[NW];
static volatile int ready[NW];
static volatile int sleepers;
static volatile int go;
static int futex_word;          /* a zero word: a FUTEX_WAIT on it will not return */

static unsigned mycpu(void) {
    unsigned c = ~0u;
    if (syscall(SYS_getcpu, &c, 0, 0) != 0) return ~0u;
    return c;
}

static void *sleeper(void *arg) {
    (void)arg;
    __sync_fetch_and_add(&sleepers, 1);
    /* It sleeps for ever: the word equals the expected value, nobody is going to wake it.
       The task is alive and ASSIGNED to a core — exactly what confused the old count. */
    for (;;) syscall(SYS_futex, &futex_word, FUTEX_WAIT, 0, 0, 0, 0);
    return 0;
}

static void *worker(void *arg) {
    long i = (long)arg;
    cpu0[i] = mycpu();
    ready[i] = 1;
    while (!go) ;
    for (uint64_t k = 0; k < ITERS; k++) ctr[i].v++;
    cpu1[i] = mycpu();
    return 0;
}

int main(void) {
    pthread_t t[NW], s[NS];
    for (long i = 0; i < NS; i++)
        if (pthread_create(&s[i], 0, sleeper, (void *)i)) { dprintf(1, "STEAL: FAIL pthread_create (a sleeping one)\n"); return 1; }
    while (sleepers < NS) ;
    for (long i = 0; i < NW; i++)
        if (pthread_create(&t[i], 0, worker, (void *)i)) { dprintf(1, "STEAL: FAIL pthread_create (a computing one)\n"); return 1; }
    for (int i = 0; i < NW; i++) while (!ready[i]) ;

    dprintf(1, "STEAL: sleeping %d, computing %d; the cores at the start:", NS, NW);
    for (int i = 0; i < NW; i++) dprintf(1, " %u", cpu0[i]);
    dprintf(1, "\n");

    go = 1;
    for (int i = 0; i < NW; i++) pthread_join(t[i], 0);

    dprintf(1, "STEAL: the cores after the counting:");
    for (int i = 0; i < NW; i++) dprintf(1, " %u", cpu1[i]);
    dprintf(1, "\n");

    uint64_t sum = 0;
    for (int i = 0; i < NW; i++) sum += ctr[i].v;
    if (sum != (uint64_t)NW * ITERS) { dprintf(1, "STEAL: FAIL — not all the threads finished counting (%llu)\n", (unsigned long long)sum); return 1; }

    int used = 0;
    unsigned per[NCORE] = {0};
    for (int i = 0; i < NW; i++) if (cpu1[i] < NCORE) per[cpu1[i]]++;
    for (int k = 0; k < NCORE; k++) if (per[k]) used++;
    if (used == NW)
        dprintf(1, "STEAL: the computing ones are on %d different cores, both finished counting — SEPARATED ALL OK\n", used);
    else
        dprintf(1, "STEAL: FAIL — the computing ones stayed on %d core(s)\n", used);
    return 0;
}
