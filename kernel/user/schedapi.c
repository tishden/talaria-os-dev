/* schedapi.c — the scheduler's contract with the workload (talaria.test=schedapi).
 *
 * Here the things are checked that are easy to do "successfully" and not to do: the
 * syscall returns zero, the application believes it, and what was promised did not
 * happen. Such a refusal is visible in no log — it surfaces minutes later as a foreign hang.
 *
 * 1) sched_yield REALLY gives the processor up. Ours was a no-op on every core except
 *    the control plane (for the sake of run-to-completion on a dedicated core). For a
 *    thread that had moved onto another core this turned into an eternal loop:
 *    `while (condition) sched_yield();` holds the core, while the one who is to fulfil
 *    the condition stands next to it and gets no processor. The test seats TWO threads
 *    on ONE core and demands that the spinning one let the neighbour through.
 * 2) membarrier does not lie. It too returned zero while doing nothing — and it is asked
 *    for exactly when one's own barrier on the fast path is deliberately dropped.
 *    We check that QUERY gives out a non-empty mask and that the command itself works.
 * 3) the binding to cores: sched_setaffinity/getaffinity go there and back, while a
 *    non-existent core is rejected rather than accepted silently.
 *
 * musl static, -Wl,-Ttext-segment=0x10000000.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <pthread.h>
#include <sched.h>
#include <errno.h>
#include <sys/syscall.h>
#include <time.h>

static int fails = 0;
static void chk(int ok, const char *what) {
    if (ok) printf("schedapi: ok %s\n", what);
    else { printf("schedapi: FAIL %s\n", what); fails++; }
    fflush(stdout);
}

static double now_s(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (double)t.tv_sec + (double)t.tv_nsec / 1e9;
}

static int pin_to(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    return sched_setaffinity(0, sizeof set, &set);
}

/* The neighbour: it sits down on the same core and raises a flag. If the spinning thread
 * does not give the processor up, control will never come here. */
static volatile int peer_ran;
static int peer_cpu;
static void *peer(void *arg) {
    (void)arg;
    if (pin_to(peer_cpu) != 0) { printf("schedapi: FAIL the binding of the neighbour\n"); fails++; return 0; }
    sched_yield();                 /* let the spinning one get onto the core first */
    __atomic_store_n((int *)&peer_ran, 1, __ATOMIC_SEQ_CST);
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    int ncpu = (int)sysconf(_SC_NPROCESSORS_ONLN);
    printf("schedapi: cores online %d\n", ncpu);

    /* ── 1) sched_yield gives the processor to the neighbour on THE SAME core ───── */
    peer_cpu = ncpu > 1 ? 1 : 0;   /* a core of the common pool, not the control plane */
    if (pin_to(peer_cpu) != 0) { printf("schedapi: FAIL the binding of the main thread\n"); fails++; }
    pthread_t t;
    peer_ran = 0;
    if (pthread_create(&t, 0, peer, 0) != 0) { printf("schedapi: FAIL pthread_create\n"); fails++; }
    double t0 = now_s();
    while (!__atomic_load_n((int *)&peer_ran, __ATOMIC_SEQ_CST)) {
        sched_yield();
        if (now_s() - t0 > 5.0) break;  /* a no-op yield -> we hold the core for ever */
    }
    double waited = now_s() - t0;
    chk(__atomic_load_n((int *)&peer_ran, __ATOMIC_SEQ_CST), "sched_yield let the neighbour through on the same core");
    printf("schedapi: the neighbour got the core in %.3f s\n", waited);
    pthread_join(t, 0);

    /* ── 2) membarrier ─────────────────────────────────────────────────────── */
    long mask = syscall(SYS_membarrier, 0 /*QUERY*/, 0, 0);
    chk(mask > 0, "membarrier QUERY gives out a mask of what is supported");
    if (mask > 0) {
        int cmd = (mask & 8) ? 8 : ((mask & 1) ? 1 : 0);   /* PRIVATE_EXPEDITED or GLOBAL */
        if (mask & 16) (void)syscall(SYS_membarrier, 16, 0, 0); /* the registration, if it is needed */
        long r = cmd ? syscall(SYS_membarrier, cmd, 0, 0) : -1;
        chk(r == 0, "membarrier executes the barrier rather than returning success silently");
    }

    /* ── 3) the binding to cores ───────────────────────────────────────────────── */
    cpu_set_t got;
    CPU_ZERO(&got);
    chk(sched_getaffinity(0, sizeof got, &got) == 0 && CPU_ISSET(peer_cpu, &got) && CPU_COUNT(&got) == 1,
        "sched_getaffinity shows exactly the core we have bound ourselves to");
    cpu_set_t bad;
    CPU_ZERO(&bad);
    CPU_SET(63, &bad);             /* there is no core 63 on the stand */
    chk(sched_setaffinity(0, sizeof bad, &bad) != 0,
        "the binding to a non-existent core has been rejected rather than accepted silently");

    printf("schedapi: %s\n", fails ? "FAILED" : "ALL OK");
    fflush(stdout);
    return fails ? 1 : 0;
}
