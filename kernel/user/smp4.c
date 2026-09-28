// SMP4 (the actor bench, the preparation): 4 cores — three threads migrate onto AP1..AP3
// (sched_setaffinity), the CPU-bound loops go IN PARALLEL with the BSP (a speedup > 2.5x
// against a single pass x4), sched_getaffinity gives out the mask of the online cores.
// The build: musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -o smp4 smp4.c
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <unistd.h>
#include <sched.h>
#include <sys/syscall.h>

static inline uint64_t rdtsc(void) {
    unsigned lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return (uint64_t)hi << 32 | lo;
}

#define ITERS 100000000ULL
#define NW 3

static volatile uint64_t ready[NW], go, done[NW];
/* a line per counter — see the comment in smp2.c (false sharing on an EPYC) */
struct actr { volatile uint64_t v; char pad[56]; };
static struct actr wctr_l[NW] __attribute__((aligned(64)));
#define wctr_at(i) (wctr_l[i].v)
static volatile uint64_t mctr  __attribute__((aligned(64)));
static volatile uint64_t mctr2 __attribute__((aligned(64)));
static volatile long aff_ret[NW];
/* A10: the core the thread ENDED UP on (see the comment in smp2.c: the speedup metric
   alone is not enough — with a failed migration it gives "parallelism" too). */
static volatile unsigned cpu_after[NW];

static void spin_work(volatile uint64_t *c) { for (uint64_t i = 0; i < ITERS; i++) (*c)++; }

static void *worker(void *arg) {
    long idx = (long)arg;
    cpu_set_t s;
    CPU_ZERO(&s);
    CPU_SET((int)(idx + 1), &s); // AP1..AP3
    aff_ret[idx] = syscall(SYS_sched_setaffinity, 0, sizeof(s), &s);
    {   unsigned c = ~0u;
        if (syscall(SYS_getcpu, &c, 0, 0) == 0) cpu_after[idx] = c;
        else cpu_after[idx] = ~0u;
    }
    ready[idx] = 1;
    while (!go) ;
    spin_work(&wctr_at(idx));
    done[idx] = 1;
    return 0;
}

int main(void) {
    // the mask of the online cores: we expect 4 cores (0b1111)
    unsigned long mask = 0;
    long gr = syscall(SYS_sched_getaffinity, 0, sizeof(mask), &mask);
    int ncpu = __builtin_popcountl(mask);
    dprintf(1, "SMP4: getaffinity ret=%ld mask=%#lx ncpu=%d\n", gr, mask, ncpu);

    pthread_t t[NW];
    for (long i = 0; i < NW; i++)
        if (pthread_create(&t[i], 0, worker, (void *)i)) { dprintf(1, "SMP4: FAIL pthread_create\n"); return 1; }
    for (int i = 0; i < NW; i++) while (!ready[i]) ;
    for (int i = 0; i < NW; i++)
        if (aff_ret[i] != 0) dprintf(1, "SMP4: setaffinity[%d] ret=%ld (did the migration fail?)\n", i, aff_ret[i]);

    // T1: the single pass on the BSP (the workers spin waiting for go on the APs)
    uint64_t t0 = rdtsc();
    spin_work(&mctr);
    uint64_t t1 = rdtsc() - t0;

    // The parallel pass: 4 cores at the same time
    go = 1;
    t0 = rdtsc();
    spin_work(&mctr2);
    for (int i = 0; i < NW; i++) while (!done[i]) ;
    uint64_t tboth = rdtsc() - t0;

    for (int i = 0; i < NW; i++) pthread_join(t[i], 0); // a futex join of the threads that died on the APs
    double speedup = 4.0 * (double)t1 / (double)tboth;
    uint64_t wsum = wctr_at(0) + wctr_at(1) + wctr_at(2);
    dprintf(1, "SMP4: t1=%llu both=%llu speedup=%.2f wsum=%llu\n",
            (unsigned long long)t1, (unsigned long long)tboth, speedup, (unsigned long long)wsum);
    int aff_ok = aff_ret[0] == 0 && aff_ret[1] == 0 && aff_ret[2] == 0
              && cpu_after[0] == 1 && cpu_after[1] == 2 && cpu_after[2] == 3;
    dprintf(1, "SMP4: the threads are on the cores %u/%u/%u\n", cpu_after[0], cpu_after[1], cpu_after[2]);
    if (aff_ok && ncpu == 4 && wsum == 3 * ITERS && mctr == ITERS && mctr2 == ITERS && speedup > 2.5)
        dprintf(1, "SMP4: the parallel speedup %.2f on 4 cores (3 threads on AP1..AP3) ALL OK\n", speedup);
    else
        dprintf(1, "SMP4: FAIL (aff=%d ncpu=%d speedup=%.2f)\n", aff_ok, ncpu, speedup);
    return 0;
}
