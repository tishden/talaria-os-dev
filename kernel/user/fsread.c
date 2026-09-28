/* fsread.c — the speed of READING A FILE, with the same binary on Talaria and on Linux.
 *
 * Why separately from fsbig (that one checks CORRECTNESS). ClickHouse loses exactly on
 * the queries that read a table from the disk, while the profile of the syscalls shows
 * `read` at 26 ms per call. There are several hypotheses about where this comes from (the
 * walk over the indirect blocks, the FS lock, the number of trips to the device), and
 * they all differ in the numbers: a sequential read in big pieces, the same in small
 * ones, an out-of-order read. One static binary for both systems — otherwise what would
 * be compared would be not the file systems but the ways of measuring them.
 *
 *   fsread <file> [KB per call]      musl static, -Ttext-segment=0x10000000
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static double now_s(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (double)t.tv_sec + (double)t.tv_nsec / 1e9;
}

/* A sequential read of the whole file in pieces of chunk bytes. -> MB/s */
static double seq(const char *path, size_t chunk, const char *tag) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) { printf("fsread: there is no %s\n", path); return 0; }
    char *buf = malloc(chunk);
    if (!buf) { printf("fsread: out of memory\n"); close(fd); return 0; }
    unsigned long long total = 0, sum = 0;
    double t0 = now_s();
    for (;;) {
        ssize_t n = read(fd, buf, chunk);
        if (n <= 0) break;
        total += (unsigned long long)n;
        sum += (unsigned char)buf[0] + (unsigned char)buf[n - 1]; /* so that it is not thrown out */
    }
    double dt = now_s() - t0;
    double mbs = dt > 0 ? total / dt / 1e6 : 0.0;
    printf("RESULT fsread %s %.1f MB/s (%llu MB in %.3f s, the piece %zu KB, sum=%llu)\n",
           tag, mbs, total >> 20, dt, chunk >> 10, sum);
    free(buf);
    close(fd);
    return mbs;
}

/* An OUT-OF-ORDER read: a pread at random offsets — the price of one trip. */
static void rnd(const char *path, size_t chunk, long iters) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return;
    off_t sz = lseek(fd, 0, SEEK_END);
    if (sz <= (off_t)chunk) { close(fd); return; }
    char *buf = malloc(chunk);
    if (!buf) { close(fd); return; }
    unsigned long long sum = 0;
    unsigned long long r = 88172645463325252ULL;
    double t0 = now_s();
    for (long i = 0; i < iters; i++) {
        r ^= r << 13; r ^= r >> 7; r ^= r << 17;
        off_t off = (off_t)(r % (unsigned long long)(sz - (off_t)chunk));
        if (pread(fd, buf, chunk, off) > 0) sum += (unsigned char)buf[0];
    }
    double dt = now_s() - t0;
    printf("RESULT fsread out-of-order %.1f us/trip (%ld trips of %zu KB, sum=%llu)\n",
           dt * 1e6 / (double)iters, iters, chunk >> 10, sum);
    free(buf);
    close(fd);
}

/* A file of the needed size: if it is not there (or it is small) — create it. The gate
 * must work on any volume rather than only on one prepared in advance. */
static int ensure(const char *path, unsigned long long want) {
    int fd = open(path, O_RDWR | O_CREAT, 0644);
    if (fd < 0) return -1;
    off_t have = lseek(fd, 0, SEEK_END);
    if ((unsigned long long)have >= want) { close(fd); return 0; }
    char *z = malloc(1 << 20);
    if (!z) { close(fd); return -1; }
    memset(z, 0xA5, 1 << 20);
    lseek(fd, 0, SEEK_SET);
    for (unsigned long long w = 0; w < want; w += (1 << 20)) {
        if (write(fd, z, 1 << 20) != (1 << 20)) { free(z); close(fd); return -1; }
    }
    free(z);
    fsync(fd);
    close(fd);
    return 0;
}

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : "/big.dat";
    size_t kb = argc > 2 ? (size_t)atol(argv[2]) : 1024;
    unsigned long long mb = argc > 3 ? (unsigned long long)atol(argv[3]) : 0;
    if (mb && ensure(path, mb << 20) != 0) { printf("fsread: cannot create %s\n", path); return 1; }
    double big = seq(path, kb * 1024, "sequentially");
    double small = seq(path, 4096, "in 4K");
    rnd(path, 4096, 20000);
    /* THE ONLY assertion of the gate: a sequential read in small pieces must not be
     * several times slower than in big ones. If it has sagged, it means the read-ahead
     * window is again counted by the size of the request rather than by the behaviour, and
     * every block again costs a separate trip to the device. */
    int ok = big > 0 && small >= big / 2;
    printf("fsread: the read-ahead by behaviour is %s: in 4K = %.0f%% of the big pieces\n",
           ok ? "ALIVE" : "BROKEN", big > 0 ? small * 100 / big : 0.0);
    if (!ok) { printf("fsread: FAIL\n"); return 1; }
    printf("fsread: ALL OK\n");
    return 0;
}
