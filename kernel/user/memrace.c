/* memrace.c — the regression of CONCURRENT work with memory (talaria.test=memrace).
 *
 * The kernel has got lazy pages, cluster materialisation, large pages, MADV_DONTNEED and
 * a TLB flush between the cores. A mistake in any of these paths looks not like a crash
 * on the spot but like foreign bytes in foreign memory after minutes of work — that is,
 * exactly like a "random" crash of a big runtime. So we press on them directly: several
 * threads at the same time map, touch, protect, hand back and remove memory, comparing
 * EVERY byte.
 *
 * What is checked:
 *   1) a fresh mmap comes as ZEROES (otherwise the workload gets someone else's data);
 *   2) what has been written reads back (the materialisation neither loses nor substitutes);
 *   3) mprotect(RO) really forbids a write, and back again allows it;
 *   4) MADV_DONTNEED zeroes (the Linux semantics for anonymous memory);
 *   5) the neighbouring threads neither see nor spoil each other's areas;
 *   6) a shared area: every thread writes its own piece, everyone reads everything.
 * musl static, -Wl,-Ttext-segment=0x10000000.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <signal.h>
#include <setjmp.h>
#include <pthread.h>
#include <sys/mman.h>
#include <ucontext.h>

#define THREADS 4
#define ROUNDS  40
#define BIG     (2u << 20)          /* 2 MB — the size at which the kernel takes a large page */

static volatile int fails = 0;
static void fail(const char *what, unsigned long a, unsigned long b) {
    printf("memrace: FAIL %s (%lu != %lu)\n", what, a, b);
    fflush(stdout);
    __atomic_fetch_add((int *)&fails, 1, __ATOMIC_SEQ_CST);
}

static unsigned char byte_of(unsigned long seed, unsigned long i) {
    return (unsigned char)((seed * 1103515245u + i * 12345u) >> 7);
}

/* The check of the ban on writing: we catch the SIGSEGV and go on.
 *
 * An UNEXPECTED signal used to simply exit with the code 97, and from the outside that
 * looked like "the test died silently": neither the address, nor the thread, nor what
 * exactly it had expected. "A spare signal came at a foreign address" cannot be told
 * apart from "the EXPECTED one came, but the handler read a foreign TLS" by the exit
 * code, while the cures are opposite. We print everything we know — straight with
 * write(), without stdio: stdio must not be called in a signal handler. */
static __thread sigjmp_buf jb;
static __thread volatile int expect_fault;
static __thread unsigned long tls_probe;   /* the address of this variable = the thread's TLS fingerprint */

static void puthex(char *p, unsigned long v) {
    for (int i = 15; i >= 0; i--) { p[i] = "0123456789abcdef"[v & 15]; v >>= 4; }
}
static void segv(int s, siginfo_t *si, void *uc) {
    (void)s; (void)uc;
    if (expect_fault) siglongjmp(jb, 1);
    /* "addr" — where the access went, "tls" — whose TLS the handler sees, "exp" — what
       lies in it. A match of tls with the expected one and exp=0 means the signal is a
       SPARE one; a mismatch of tls means the handler landed in a foreign thread. */
    /* The mark goes FIRST and into the fd 1: if the handler crashes while parsing si/uc
       (a nested signal), at least the fact of the entry will stay in the log. */
    ssize_t _ = write(1, "memrace: AN UNEXPECTED SIGSEGV\n", 31); (void)_;
    char b[] = "memrace:   addr=0x0000000000000000 rip=0x0000000000000000 tls=0x0000000000000000 exp=0\n";
    puthex(b + 18, (unsigned long)(si ? si->si_addr : 0));
    puthex(b + 41, (unsigned long)(uc ? ((ucontext_t *)uc)->uc_mcontext.gregs[REG_RIP] : 0));
    puthex(b + 64, (unsigned long)&tls_probe);
    b[85] = (char)('0' + (expect_fault & 1));
    _ = write(1, b, sizeof b - 1); (void)_;
    _exit(97);
}

static void *worker(void *arg) {
    unsigned long id = (unsigned long)arg;
    for (int r = 0; r < ROUNDS; r++) {
        size_t len = (r % 2) ? BIG : (128u << 10);
        unsigned char *p = mmap(0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { fail("mmap", 0, 0); return 0; }

        for (size_t i = 0; i < len; i += 4093) if (p[i]) { fail("fresh memory is not zero", p[i], 0); break; }

        unsigned long seed = id * 7919 + r;
        for (size_t i = 0; i < len; i += 64) p[i] = byte_of(seed, i);
        for (size_t i = 0; i < len; i += 64) if (p[i] != byte_of(seed, i)) { fail("the write/read", p[i], byte_of(seed, i)); break; }

        if (mprotect(p, len, PROT_READ) != 0) fail("mprotect RO", 0, 0);
        for (size_t i = 0; i < len; i += 4096) if (p[i] != byte_of(seed, i)) { fail("the read under RO", p[i], byte_of(seed, i)); break; }
        expect_fault = 1;                       /* a write under RO must give a SIGSEGV */
        if (sigsetjmp(jb, 1) == 0) { p[0] = 0xFF; fail("a write under RO went through", 1, 0); }
        expect_fault = 0;
        if (mprotect(p, len, PROT_READ | PROT_WRITE) != 0) fail("mprotect RW", 0, 0);
        p[0] = byte_of(seed, 0);

        if (madvise(p, len, MADV_DONTNEED) == 0) {   /* it must give back zeroes */
            for (size_t i = 0; i < len; i += 4093) if (p[i]) { fail("MADV_DONTNEED did not zero", p[i], 0); break; }
        }
        for (size_t i = 0; i < len; i += 64) p[i] = byte_of(seed + 1, i);
        for (size_t i = 0; i < len; i += 64) if (p[i] != byte_of(seed + 1, i)) { fail("the write after DONTNEED", p[i], byte_of(seed + 1, i)); break; }
        munmap(p, len);
    }
    return 0;
}

/* The shared area: every thread writes its own piece, then everyone compares everything. */
static unsigned char *shared;
static pthread_barrier_t bar;
static void *sharer(void *arg) {
    unsigned long id = (unsigned long)arg;
    size_t part = BIG / THREADS;
    unsigned char *my = shared + id * part;
    for (size_t i = 0; i < part; i += 64) my[i] = byte_of(id + 100, i);
    pthread_barrier_wait(&bar);
    for (unsigned long t = 0; t < THREADS; t++) {
        unsigned char *o = shared + t * part;
        for (size_t i = 0; i < part; i += 4096)
            if (o[i] != byte_of(t + 100, i)) { fail("the shared area has been spoilt by a neighbour", o[i], byte_of(t + 100, i)); return 0; }
    }
    return 0;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = segv;
    sa.sa_flags = SA_SIGINFO;
    sigaction(SIGSEGV, &sa, 0);

    pthread_t t[THREADS];
    for (unsigned long i = 0; i < THREADS; i++) pthread_create(&t[i], 0, worker, (void *)i);
    for (int i = 0; i < THREADS; i++) pthread_join(t[i], 0);
    printf("memrace: ok the separate areas (%d threads x %d rounds)\n", THREADS, ROUNDS);
    fflush(stdout);

    shared = mmap(0, BIG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) { printf("memrace: FAILED (the mmap of the shared one)\n"); return 1; }
    pthread_barrier_init(&bar, 0, THREADS);
    for (unsigned long i = 0; i < THREADS; i++) pthread_create(&t[i], 0, sharer, (void *)i);
    for (int i = 0; i < THREADS; i++) pthread_join(t[i], 0);
    printf("memrace: ok the shared area\n");

    printf("memrace: %s\n", fails ? "FAILED" : "ALL OK");
    fflush(stdout);
    return fails ? 1 : 0;
}
