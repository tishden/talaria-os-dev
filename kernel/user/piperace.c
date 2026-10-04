/* piperace: A PIPE AND AN EVENTFD WITH THEIR ENDS ON TWO CORES AT ONCE.
 *
 * What it is for. The kernel's pipe was one count both ends read, changed and wrote
 * back with plain stores, and an eventfd's counter was the same: on two cores at
 * once one end's update is lost — bytes vanish from a pipe or are read twice, an
 * eventfd increment disappears — and on arm64 a reader could also see the count
 * before the bytes behind it. Nothing crashes; the stream is simply wrong.
 *
 * So the check is the stream itself, end to end, with the ends on different cores:
 *   1. a pipe: a writer thread puts PIPE_BYTES of a known sequence in chunks of
 *      varying size, a reader thread takes them in chunks of other sizes and checks
 *      EVERY byte against the sequence, and the total;
 *   2. an eventfd: EV_WRITERS threads each add 1 EV_PER times while a reader takes
 *      the counter; the sum taken must be exactly EV_WRITERS * EV_PER.
 * The threads meet at a spin barrier before each phase, so that they run together. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/eventfd.h>
#include <time.h>
#include <unistd.h>
#include "tal_arch.h"

#define PIPE_BYTES (4u << 20)
#define EV_WRITERS 3
#define EV_PER 100000

static volatile int go;
static int pfd[2];
static int efd;
static volatile int fails;

static unsigned char seqb(uint32_t i) { return (unsigned char)(i * 2654435761u >> 13); }

static double now(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec * 1e-9;
}

static void *pwriter(void *a) {
    (void)a;
    while (!go) ;
    unsigned char buf[3001];
    uint32_t off = 0, k = 1;
    while (off < PIPE_BYTES) {
        uint32_t n = 1 + (k * 7919u) % sizeof buf; k++;
        if (n > PIPE_BYTES - off) n = PIPE_BYTES - off;
        for (uint32_t i = 0; i < n; i++) buf[i] = seqb(off + i);
        uint32_t done = 0;
        while (done < n) {
            ssize_t w = write(pfd[1], buf + done, n - done);
            if (w <= 0) { printf("piperace: FAIL the pipe write returned %zd at %u\n", w, off + done); fails++; return 0; }
            done += w;
        }
        off += n;
    }
    close(pfd[1]);
    return 0;
}

static void *ewriter(void *a) {
    (void)a;
    while (go < 2) ;
    uint64_t one = 1;
    for (int i = 0; i < EV_PER; i++)
        if (write(efd, &one, 8) != 8) { printf("piperace: FAIL an eventfd write\n"); fails++; return 0; }
    return 0;
}

int main(void) {
    printf("piperace: a pipe (%u bytes) and an eventfd (%d writers x %d) across cores\n", PIPE_BYTES, EV_WRITERS, EV_PER);
    if (pipe(pfd) != 0) { printf("piperace: FAIL pipe()\n"); return 1; }
    pthread_t w;
    pthread_create(&w, 0, pwriter, 0);
    double t0 = now();
    go = 1;
    unsigned char buf[1777];
    uint32_t off = 0, bad = 0, k = 1;
    for (;;) {
        size_t want = 1 + (k * 104729u) % sizeof buf; k++;
        ssize_t r = read(pfd[0], buf, want);
        if (r < 0) { printf("piperace: FAIL the pipe read returned %zd (errno %d)\n", r, errno); fails++; break; }
        if (r == 0) break;
        for (ssize_t i = 0; i < r; i++) if (buf[i] != seqb(off + i) && bad++ < 3)
            printf("piperace: FAIL byte %u is %#x, expected %#x\n", off + (uint32_t)i, buf[i], seqb(off + i));
        off += r;
        if (now() - t0 > 60) { printf("piperace: FAIL the pipe stalled at %u bytes\n", off); fails++; break; }
    }
    pthread_join(w, 0);
    close(pfd[0]);
    if (bad || off != PIPE_BYTES) { printf("piperace: FAIL the pipe: %u bytes of %u, %u wrong\n", off, PIPE_BYTES, bad); fails++; }
    else printf("piperace: the pipe carried %u bytes in order across cores (%.2f s)\n", off, now() - t0);

    efd = eventfd(0, EFD_NONBLOCK);
    if (efd < 0) { printf("piperace: FAIL eventfd()\n"); return 1; }
    pthread_t ew[EV_WRITERS];
    for (int i = 0; i < EV_WRITERS; i++) pthread_create(&ew[i], 0, ewriter, 0);
    uint64_t sum = 0, v;
    t0 = now();
    go = 2;
    while (sum < (uint64_t)EV_WRITERS * EV_PER && now() - t0 < 60) {
        if (read(efd, &v, 8) == 8) sum += v;
    }
    for (int i = 0; i < EV_WRITERS; i++) pthread_join(ew[i], 0);
    if (read(efd, &v, 8) == 8) sum += v;
    if (sum != (uint64_t)EV_WRITERS * EV_PER) { printf("piperace: FAIL the eventfd sum %llu, expected %d\n", (unsigned long long)sum, EV_WRITERS * EV_PER); fails++; }
    else printf("piperace: the eventfd summed %llu increments from %d threads exactly\n", (unsigned long long)sum, EV_WRITERS);
    printf(fails ? "piperace: FAILURES %d\n" : "piperace: ALL OK\n", fails);
    return fails != 0;
}
