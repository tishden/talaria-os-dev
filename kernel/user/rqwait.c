// RQWAIT: the metric "READY BUT NOT RUNNING" (session 26) is judged where the answer is
// known in advance.
//
// Three computing threads without a single syscall in the loop. If all three are seated
// on ONE core (`talaria.pin=*:1 talaria.nobalance=1 talaria.nosteal=1`), then at any
// moment one runs while two are READY and wait for the processor: the total wait must be
// about TWO walls of the counting (the share of the wall >= 100 %). The same three
// threads on four cores must hardly wait at all (the share < 25 % — that is what the
// resettling by the stealing and the balance at the start costs).
//
// The metric is counted by the KERNEL (sched::RQ_*), printed by `metrics::dump` as the
// line "stat: ready->cpu", and the share of the wall by the gate's report in
// selftest/concur.rs (`RQWAIT: ... the share of the waiting is N%`). The program only
// creates a load with a known answer and reports the wall of the counting, so that both numbers are side by side.
//
// The waking (`stat: woken->ran`) does not see such a wait: a thread whose core was taken
// away by the quantum was woken by nobody — it did not fall asleep either.
// It is exactly this blind spot that the metric closes.
//
// The run: talaria.test=rqwait talaria.pin=*:1 talaria.nobalance=1 talaria.nosteal=1 (-smp 4)
//          talaria.test=rqwait (-smp 4) — the control: the share is small
// The build: make -C kernel/user rqwait
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <time.h>
#include <unistd.h>

#define NW 3
#define ITERS 100000000ULL      /* ~0.4 s per thread: hundreds of quanta of 500 us */

struct slot { volatile uint64_t v; char pad[56]; };
static struct slot ctr[NW] __attribute__((aligned(64)));
static volatile int ready[NW];
static volatile int go;

static uint64_t now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static void *worker(void *arg) {
    long i = (long)arg;
    ready[i] = 1;
    while (!go) ;
    for (uint64_t k = 0; k < ITERS; k++) ctr[i].v++;
    return 0;
}

int main(void) {
    pthread_t t[NW];
    for (long i = 0; i < NW; i++)
        if (pthread_create(&t[i], 0, worker, (void *)i)) { dprintf(1, "RQWAIT: FAIL pthread_create\n"); return 1; }
    for (int i = 0; i < NW; i++) while (!ready[i]) ;
    uint64_t t0 = now_ms();
    go = 1;
    for (int i = 0; i < NW; i++) pthread_join(t[i], 0);
    uint64_t wall = now_ms() - t0;
    uint64_t sum = 0;
    for (int i = 0; i < NW; i++) sum += ctr[i].v;
    if (sum != (uint64_t)NW * ITERS) { dprintf(1, "RQWAIT: FAIL — not all the threads finished counting (%llu)\n", (unsigned long long)sum); return 1; }
    dprintf(1, "RQWAIT: the %d threads finished counting, the wall of the counting is %llu ms — ALL OK\n", NW, (unsigned long long)wall);
    return 0;
}
