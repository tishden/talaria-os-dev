// SCHEDSPREAD: the unpinned threads move apart over the cores and ALL get the processor.
//
// What is checked is exactly what was not in the kernel before V1.9: there is more than
// one thread on a core of the common pool, and they spin in turn. If there is no
// preemption on a secondary core, the test will not "show a bad number" — it WILL NOT
// FINISH: the second thread of every core will never reach the finish. That is why it has
// a timeout of its own: silence must be distinguishable from a hang (the pitfall of session 10).
//
// The run: talaria.test=schedspread talaria.smp=4 talaria.sched=spread
// The build: musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -o schedspread schedspread.c
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include <sys/syscall.h>

#define NW 8            /* the threads */
#define NCORE 4         /* the cores in the pool (the BSP + 3 APs) */
#define ITERS 3000000ULL

struct slot { volatile uint64_t v; char pad[56]; };   /* a line per thread */
static struct slot ctr[NW] __attribute__((aligned(64)));
static volatile unsigned cpu_of[NW];
static volatile int ready[NW];
static volatile int done[NW];
static volatile int go;

static unsigned mycpu(void) {
    unsigned c = ~0u;
    if (syscall(SYS_getcpu, &c, 0, 0) != 0) return ~0u;
    return c;
}

static void *worker(void *arg) {
    long i = (long)arg;
    cpu_of[i] = mycpu();          /* the core CHOSEN by the OS kernel at the clone */
    ready[i] = 1;
    while (!go) ;
    for (uint64_t k = 0; k < ITERS; k++) ctr[i].v++;
    done[i] = 1;
    return 0;
}

int main(void) {
    pthread_t t[NW];
    for (long i = 0; i < NW; i++)
        if (pthread_create(&t[i], 0, worker, (void *)i)) { dprintf(1, "SPREAD: FAIL pthread_create\n"); return 1; }
    /* let the threads report their core before the counting starts: they have already been laid out by the OS kernel */
    for (int i = 0; i < NW; i++) while (!ready[i]) ;

    unsigned per[NCORE + 8];
    for (unsigned c = 0; c < NCORE + 8; c++) per[c] = 0;
    for (int i = 0; i < NW; i++) if (cpu_of[i] < NCORE + 8) per[cpu_of[i]]++;
    dprintf(1, "SPREAD: the threads' cores:");
    for (int i = 0; i < NW; i++) dprintf(1, " %u", cpu_of[i]);
    dprintf(1, "\n");
    dprintf(1, "SPREAD: threads per core: %u %u %u %u\n", per[0], per[1], per[2], per[3]);

    go = 1;
    /* we wait for the finish of ALL: if there is no preemption on a core of the pool, the second thread will not get there */
    int alldone = 0;
    for (long guard = 0; guard < 400000000L && !alldone; guard++) {
        alldone = 1;
        for (int i = 0; i < NW; i++) if (!done[i]) { alldone = 0; break; }
    }
    if (!alldone) {
        dprintf(1, "SPREAD: FAIL — not all the threads reached the end (there is no preemption on a core of the pool):");
        for (int i = 0; i < NW; i++) dprintf(1, " %d", done[i]);
        dprintf(1, "\n");
        return 1;
    }
    uint64_t sum = 0;
    for (int i = 0; i < NW; i++) sum += ctr[i].v;
    /* As a separate line: "all the threads got the processor" — a statement in itself,
       independent of how exactly they were laid out over the cores. */
    dprintf(1, "SPREAD: all the %d threads reached the end, the sum is %llu\n", NW, (unsigned long long)sum);

    int used = 0, minp = 1 << 30, maxp = 0;
    for (int c = 0; c < NCORE; c++) { if (per[c]) used++; if ((int)per[c] < minp) minp = per[c]; if ((int)per[c] > maxp) maxp = per[c]; }
    int spread_ok = (used == NCORE) && (maxp - minp <= 1);
    if (spread_ok && sum == (uint64_t)NW * ITERS)
        dprintf(1, "SPREAD: %d threads on %d cores, %d..%d per core, all reached the end ALL OK\n", NW, NCORE, minp, maxp);
    else
        dprintf(1, "SPREAD: FAIL (cores in use %d, per core %d..%d, the sum is %llu)\n",
                used, minp, maxp, (unsigned long long)sum);
    return 0;
}
