// SMP2 (PLAN-SYNC stage 0): the migration of a thread onto an AP through
// sched_setaffinity and a proof of real parallelism: two CPU-bound loops (the BSP + an AP)
// must go at the same time (a speedup of ~2x against the time-sharing of one core).
// The build: musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -o smp2 smp2.c
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

static volatile uint64_t ready, go, done;
/* the counters — each in ITS OWN cache line: otherwise the both-phase measures false
   sharing (one line for wctr/mctr2 -> an RFO ping-pong; on SMT siblings it is cheap, on
   separate CCXs of an EPYC it is x3-4 — it was caught as a speedup of 0.46 on AMD) */
static volatile uint64_t wctr  __attribute__((aligned(64)));
static volatile uint64_t mctr  __attribute__((aligned(64)));
static volatile uint64_t mctr2 __attribute__((aligned(64)));
static volatile long aff_ret = -1000, tid_after = -1;
/* A10: the core the thread ENDED UP on (getcpu already from the AP). Without this check
   the test deceived itself: if the migration did not succeed, the worker stayed on the BSP
   and spun its loop in competition with main — then t1 (the single pass) doubled, and the
   both-phase equalled it, and the speedup = 2*t1/both still gave ~2.0. It is exactly like
   that that "ALL OK" was printed on a topology with sparse APIC ids where there was no
   parallelism at all. */
static volatile unsigned cpu_after = ~0u;

static void spin_work(volatile uint64_t *c) { for (uint64_t i = 0; i < ITERS; i++) (*c)++; }

static void *worker(void *arg) {
    (void)arg;
    cpu_set_t s;
    CPU_ZERO(&s);
    CPU_SET(1, &s);
    aff_ret = syscall(SYS_sched_setaffinity, 0, sizeof(s), &s); // the migration onto an AP
    tid_after = syscall(SYS_gettid);                            // a syscall already from the AP
    {   unsigned c = ~0u;
        if (syscall(SYS_getcpu, &c, 0, 0) == 0) cpu_after = c;  // WHICH core we are on now
    }
    ready = 1;
    while (!go) ;
    spin_work(&wctr);
    done = 1;
    return 0;
}

int main(void) {
    pthread_t t;
    if (pthread_create(&t, 0, worker, 0)) { dprintf(1, "SMP2: FAIL pthread_create\n"); return 1; }
    while (!ready) ;
    if (aff_ret != 0) dprintf(1, "SMP2: setaffinity ret=%ld (did the migration fail?)\n", aff_ret);

    // T1: the single pass on the BSP (the worker spins waiting for go on the AP)
    uint64_t t0 = rdtsc();
    spin_work(&mctr);
    uint64_t t1 = rdtsc() - t0;

    // The parallel pass: both cores work at the same time
    go = 1;
    t0 = rdtsc();
    spin_work(&mctr2);
    while (!done) ;
    uint64_t tboth = rdtsc() - t0;

    pthread_join(t, 0); // a futex join of a thread that died on an AP
    double speedup = 2.0 * (double)t1 / (double)tboth;
    dprintf(1, "SMP2: t1=%llu both=%llu speedup=%.2f wctr=%llu tid=%ld cpu=%d\n",
            (unsigned long long)t1, (unsigned long long)tboth, speedup,
            (unsigned long long)wctr, tid_after, (int)cpu_after);
    if (aff_ret == 0 && cpu_after == 1 && wctr == ITERS && mctr == ITERS && mctr2 == ITERS && speedup > 1.5)
        dprintf(1, "SMP2: the parallel speedup %.2f (the thread on an AP, the syscalls from the AP) ALL OK\n", speedup);
    else
        dprintf(1, "SMP2: FAIL (aff=%ld cpu=%d speedup=%.2f)\n", aff_ret, (int)cpu_after, speedup);
    return 0;
}
