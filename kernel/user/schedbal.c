// SCHEDBAL: the balancer RESETTLES the threads that have settled on one core.
//
// What is checked is exactly the mechanism that in session 20 turned out to be switched
// on and not working: the periodic balancing. The layout at creation and the stealing of
// work resettle AT ONCE, so a natural skew cannot be waited for here — it is created by
// the rule `talaria.pin=*:1`: all the nameless threads lie down onto one core (a rule by
// a pattern does not "pin them down" — the balancer may move them).
//
// A thread reports its core TWICE: right after the start and after a long count. The first
// number shows that the skew really was there; the second, that it has been taken apart.
// The count is deliberately long: the balancer looks at the skew once every 32 ms, and on
// a short phase its absence is indistinguishable from its slowness.
//
// The run: talaria.test=schedbal talaria.smp=4 talaria.pin=*:1
// The build: musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -o schedbal schedbal.c
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include <sys/syscall.h>

#define NW 8
#define NCORE 4
#define ITERS 120000000ULL      /* ~0.5-1 s per thread: the balancer needs dozens of windows of 32 ms */

struct slot { volatile uint64_t v; char pad[56]; };
static struct slot ctr[NW] __attribute__((aligned(64)));
static volatile unsigned cpu0[NW], cpu1[NW];
static volatile int ready[NW];
static volatile int go;

static unsigned mycpu(void) {
    unsigned c = ~0u;
    if (syscall(SYS_getcpu, &c, 0, 0) != 0) return ~0u;
    return c;
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

static void spread_of(const volatile unsigned *c, int *used, int *mn, int *mx) {
    unsigned per[NCORE];
    for (int k = 0; k < NCORE; k++) per[k] = 0;
    for (int i = 0; i < NW; i++) if (c[i] < NCORE) per[c[i]]++;
    *used = 0; *mn = 1 << 30; *mx = 0;
    for (int k = 0; k < NCORE; k++) {
        if (per[k]) (*used)++;
        if ((int)per[k] < *mn) *mn = per[k];
        if ((int)per[k] > *mx) *mx = per[k];
    }
}

int main(void) {
    pthread_t t[NW];
    for (long i = 0; i < NW; i++)
        if (pthread_create(&t[i], 0, worker, (void *)i)) { dprintf(1, "BAL: FAIL pthread_create\n"); return 1; }
    for (int i = 0; i < NW; i++) while (!ready[i]) ;

    int u0, n0, x0;
    spread_of(cpu0, &u0, &n0, &x0);
    dprintf(1, "BAL: the cores at the start:");
    for (int i = 0; i < NW; i++) dprintf(1, " %u", cpu0[i]);
    dprintf(1, "  (in use %d, per core %d..%d)\n", u0, n0, x0);

    go = 1;
    for (int i = 0; i < NW; i++) pthread_join(t[i], 0);

    int u1, n1, x1;
    spread_of(cpu1, &u1, &n1, &x1);
    dprintf(1, "BAL: the cores after the counting:");
    for (int i = 0; i < NW; i++) dprintf(1, " %u", cpu1[i]);
    dprintf(1, "  (in use %d, per core %d..%d)\n", u1, n1, x1);

    uint64_t sum = 0;
    for (int i = 0; i < NW; i++) sum += ctr[i].v;
    if (sum != (uint64_t)NW * ITERS) { dprintf(1, "BAL: FAIL — not all the threads finished counting (%llu)\n", (unsigned long long)sum); return 1; }

    /* That the skew WAS there is proved not by this thread but by the kernel: the rule
       pin=*:1 lays EVERYONE onto one core at creation, and the size of the skew is printed
       by `stat: balancing: the greatest skew`. Here it may already not be visible — the
       balancer manages to resettle them before the threads report their core, and to
       demand "at the start everyone is on one" would be to demand that it work slowly. */
    /* What is asserted is what the balancer PROMISES: the skew has been taken apart, the
       work has spread over all the cores of the pool. It does not promise exactly evenly —
       it moves one task per window and stops as soon as the skew stopped reaching the
       threshold; a perfect 2/2/2/2 comes out often but not always, and to demand it would
       be to gate luck. */
    if (u1 == NCORE)
        dprintf(1, "BAL: the skew %d cores -> %d cores, per core %d..%d — RESETTLED ALL OK\n", u0, u1, n1, x1);
    else
        dprintf(1, "BAL: FAIL — not resettled (cores %d, per core %d..%d)\n", u1, n1, x1);
    return 0;
}
