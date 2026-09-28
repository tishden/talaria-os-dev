/* mbench.c — the primitives the gap with Linux adds up from (talaria.test=mbench).
 *
 * ClickHouse, on the same queries and the same binary, runs two to three times slower
 * under Talaria than on Linux of the same hardware. One could go on enumerating
 * hypotheses (the instruction set, large pages, the scheduler), or one can measure the
 * PRIMITIVES the hot loop of the aggregation consists of and see which one exactly sags.
 * The binary is static (musl), so it runs both under Talaria and on Linux — the same
 * machine instructions on the same hardware are compared.
 *
 * What is measured and what it tells apart:
 *   alu    — a chain of dependent additions in the registers. The memory is not touched
 *            at all: the number speaks about the FREQUENCY and about whether anybody is taking cycles away.
 *   l1/l2  — a sum over an array that fits into the cache. The mapping of pages hardly
 *            takes part here: the working set is small, the TLB covers it.
 *   ram    — a sum over an array of 64 MB: the memory's bandwidth + the TLB.
 *   chase  — a walk over random links in 64 MB: THE PRICE OF A TLB MISS in its pure form.
 *            Under a hypervisor a miss costs double (a nested walk of the EPT), and if the
 *            gap is here then it is in the mapping of memory rather than in the OS's kernel.
 *   syscall— an empty syscall: the price of entering the kernel (for the completeness of the picture).
 *
 * musl static, -Wl,-Ttext-segment=0x10000000.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <fcntl.h>
#include <sys/syscall.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <time.h>
#include <sys/mman.h>

static double now_s(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (double)t.tv_sec + (double)t.tv_nsec / 1e9;
}

/* A chain of DEPENDENT additions: the next step will not start until the previous one is
 * ready, so the time = the number of steps * the latency of an addition / the frequency. */
static double bench_alu(unsigned long iters) {
    volatile unsigned long sink = 0;
    unsigned long a = 1;
    double t0 = now_s();
    for (unsigned long i = 0; i < iters; i++) { a = a * 6364136223846793005UL + 1442695040888963407UL; }
    double dt = now_s() - t0;
    sink = a; (void)sink;
    return dt;
}

/* A sum over an array: the hot loop of the aggregation without everything else. */
static double bench_sum(unsigned long *p, size_t n, int passes, unsigned long *out) {
    unsigned long s = 0;
    double t0 = now_s();
    for (int k = 0; k < passes; k++) for (size_t i = 0; i < n; i++) s += p[i];
    double dt = now_s() - t0;
    *out = s;
    return dt;
}

/* A walk over links: every step depends on the previous one, so what is measured is the
 * LATENCY of one miss (the cache + the TLB) rather than the bandwidth. */
static volatile size_t chase_sink;
static double bench_chase(size_t *p, size_t n, unsigned long steps) {
    (void)n;
    size_t idx = 0;
    double t0 = now_s();
    for (unsigned long i = 0; i < steps; i++) idx = p[idx];
    double dt = now_s() - t0;
    chase_sink = idx;                    /* otherwise the compiler throws the whole loop out */
    return dt;
}

static void shuffle_ring(size_t *p, size_t n) {
    /* A ring of all the indices in a random order: every step is a new cache line and,
     * at such a size, almost always a new page. */
    for (size_t i = 0; i < n; i++) p[i] = i;
    unsigned long r = 12345;
    for (size_t i = n - 1; i > 0; i--) {
        r = r * 6364136223846793005UL + 1442695040888963407UL;
        size_t j = (size_t)((r >> 33) % (i + 1));
        size_t t = p[i]; p[i] = p[j]; p[j] = t;
    }
    /* turn the permutation into a ring of links */
    size_t *ring = malloc(n * sizeof(size_t));
    for (size_t i = 0; i < n; i++) ring[p[i]] = p[(i + 1) % n];
    memcpy(p, ring, n * sizeof(size_t));
    free(ring);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);

    /* 1) the ALU — the frequency and the cycles */
    unsigned long it = 100000000UL;
    double dt = bench_alu(it);
    printf("RESULT mbench alu %.3f ns/step (%.2f billion steps/s)\n", dt * 1e9 / it, it / dt / 1e9);

    /* 2) a sum over an array: 32 KB (L1), 256 KB (L2), 8 MB (L3), 64 MB (the memory) */
    static const size_t sizes[] = { 32u << 10, 256u << 10, 8u << 20, 64u << 20 };
    static const char *names[] = { "l1-32k", "l2-256k", "l3-8m", "ram-64m" };
    for (int s = 0; s < 4; s++) {
        size_t bytes = sizes[s], n = bytes / sizeof(unsigned long);
        unsigned long *p = mmap(0, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { printf("RESULT mbench %s FAILED mmap\n", names[s]); continue; }
        for (size_t i = 0; i < n; i++) p[i] = i;              /* the warm-up: the pages have already been given out */
        int passes = (int)(1024u * 1024u * 256u / bytes);      /* ~256 MB of traffic per measurement */
        if (passes < 1) passes = 1;
        unsigned long out;
        double d = bench_sum(p, n, passes, &out);
        double gbs = (double)bytes * passes / d / 1e9;
        printf("RESULT mbench %s %.2f GB/s (%.2f ms, sum=%lu)\n", names[s], gbs, d * 1e3, out & 0xffff);
        munmap(p, bytes);
    }

    /* 3) a walk over links in 64 MB — the price of a TLB miss */
    {
        size_t bytes = 64u << 20, n = bytes / sizeof(size_t);
        size_t *p = mmap(0, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) printf("RESULT mbench chase FAILED mmap\n");
        else {
            shuffle_ring(p, n);
            unsigned long steps = 1000000UL;
            double d = bench_chase(p, n, steps);
            printf("RESULT mbench chase-64m %.1f ns/step\n", d * 1e9 / steps);
            munmap(p, bytes);
        }
    }

    /* 4) an empty syscall */
    {
        unsigned long k = 200000;
        double t0 = now_s();
        for (unsigned long i = 0; i < k; i++) (void)getppid();
        double d = now_s() - t0;
        printf("RESULT mbench syscall %.0f ns\n", d * 1e9 / k);
    }

    /* 5) syscalls with a pointer argument: what the kernel pays to validate it
       (uaccess.rs on Talaria) on top of the call itself */
    {
        unsigned long k = 200000;
        struct timespec ts;
        double t0 = now_s();
        for (unsigned long i = 0; i < k; i++) (void)syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &ts);
        double d = now_s() - t0;
        printf("RESULT mbench syscall-ptr16 %.0f ns\n", d * 1e9 / k);
        int z = open("/dev/zero", O_RDONLY);
        char buf[4096];
        t0 = now_s();
        for (unsigned long i = 0; i < k; i++) (void)read(z, buf, 64);
        d = now_s() - t0;
        printf("RESULT mbench read64 %.0f ns\n", d * 1e9 / k);
        close(z);
    }

    printf("mbench: ALL OK\n");
    return 0;
}
