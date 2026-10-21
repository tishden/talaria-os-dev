// gatecost — what a syscall's pointer check costs by the LENGTH the process asked for, not by the
// bytes the kernel wrote (docs/ARM64-PLAN.md §11 item 2a). `gatecost [iters] [limit]`: an empty
// non-blocking pipe is read <iters> times with buffers of 0 B, 4 KiB, 64 KiB, 1 MiB (touched) and
// 1 MiB never written (lazy pages); every read answers EAGAIN and copies nothing, so the whole
// difference between the lengths is the kernel's check of the buffer before the call. Then the
// same reads at the END of a file (/cell.manifest, or argv[0]; they answer 0): a file may fill the
// whole buffer, so its check is not capped — that is the walk's own price. tlgen's recv asks for
// 1 MiB (TL_RBUF) on every poll of a socket: a check walking the tables page by page made the
// EMPTY receive a 4-us syscall on a Graviton (the generator's whole ceiling). Prints the ns per
// call, then with the limits "gatecost: OK" or "gatecost: SLOW" by the ratios 1 MiB / 4 KiB:
// `gatecost [iters] [pipe-limit] [file-limit]`. The same binary runs on Linux (a copy with
// faults: the length costs nothing there).
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

static unsigned long long now_ns(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return (unsigned long long)t.tv_sec * 1000000000ull + (unsigned long long)t.tv_nsec;
}
/* ns per read(fd, buf, len); every call must answer `want` (-1 = EAGAIN) and copy nothing */
static unsigned long long cost(int fd, void *buf, size_t len, long iters, long *bad, ssize_t want) {
    unsigned long long t0 = now_ns();
    for (long i = 0; i < iters; i++) {
        ssize_t k = read(fd, buf, len);
        if (want < 0 ? !(k < 0 && errno == EAGAIN) : k != want) (*bad)++;
    }
    return (now_ns() - t0) / (unsigned long long)iters;
}

int main(int argc, char **argv) {
    long iters = argc > 1 ? atol(argv[1]) : 200000, lim = argc > 2 ? atol(argv[2]) : 0, flim = argc > 3 ? atol(argv[3]) : 0, bad = 0;
    int fds[2];
    if (pipe2(fds, O_NONBLOCK) < 0) { printf("gatecost: pipe2: %s\n", strerror(errno)); return 1; }
    const size_t M = 1u << 20;
    char *b4k = malloc(4096), *b64k = malloc(64 * 1024), *b1m = malloc(M);
    char *lazy = mmap(0, M, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (!b4k || !b64k || !b1m || lazy == MAP_FAILED) { printf("gatecost: no memory\n"); return 1; }
    memset(b4k, 1, 4096); memset(b64k, 1, 64 * 1024); memset(b1m, 1, M);   /* present pages */
    cost(fds[0], b4k, 4096, iters / 10, &bad, -1);                          /* warm up */
    unsigned long long c0 = cost(fds[0], b4k, 0, iters, &bad, 0);
    unsigned long long c4k = cost(fds[0], b4k, 4096, iters, &bad, -1);
    unsigned long long c64k = cost(fds[0], b64k, 64 * 1024, iters, &bad, -1);
    unsigned long long c1m = cost(fds[0], b1m, M, iters, &bad, -1);
    unsigned long long c1ml = cost(fds[0], lazy, M, iters, &bad, -1);
    printf("gatecost: read on an empty non-blocking pipe, ns per call: 0 B %llu, 4 KiB %llu, 64 KiB %llu, 1 MiB %llu, 1 MiB untouched %llu (x %ld, wrong answers %ld)\n",
           c0, c4k, c64k, c1m, c1ml, iters, bad);
    int ff = open("/cell.manifest", O_RDONLY);
    if (ff < 0) ff = open(argv[0], O_RDONLY);
    unsigned long long f4k = 0, f1m = 0, f1ml = 0;
    if (ff >= 0 && lseek(ff, 0, SEEK_END) >= 0) {
        cost(ff, b4k, 4096, iters / 10, &bad, 0);
        f4k = cost(ff, b4k, 4096, iters, &bad, 0);
        f1m = cost(ff, b1m, M, iters, &bad, 0);
        f1ml = cost(ff, lazy, M, iters, &bad, 0);
        printf("gatecost: read at a file's end (the check is not capped: the walk itself), ns per call: 4 KiB %llu, 1 MiB %llu, 1 MiB untouched %llu (wrong answers %ld)\n",
               f4k, f1m, f1ml, bad);
    } else printf("gatecost: no file to read at its end (%s)\n", strerror(errno));
    if (bad) { printf("gatecost: WRONG ANSWERS %ld\n", bad); return 1; }
    if (lim > 0) {
        unsigned long long worst = c1m > c1ml ? c1m : c1ml, fworst = f1m > f1ml ? f1m : f1ml;
        double r = c4k ? (double)worst / (double)c4k : 0, fr = f4k ? (double)fworst / (double)f4k : 0;
        int ok = r <= (double)lim && (flim <= 0 || ff < 0 || fr <= (double)flim);
        printf("gatecost: %s (a 1 MiB read costs %.1fx a 4 KiB one on the pipe, the limit %ldx; %.1fx at a file's end, the limit %ldx%s)\n",
               ok ? "OK" : "SLOW", r, lim, fr, flim, ok ? "" : ": the check walks the buffer page by page");
    }
    return 0;
}
