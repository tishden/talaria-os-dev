/* fsyncconc.c — the flush of a big dirty file DOES NOT HOLD the FS lock
 * (talaria.test=fsyncconc).
 *
 * What is caught. On EC2 an insert into MongoDB stalled for 0.8-1.4 s once a minute, and
 * locally with a disk throttled down to a gp3 (125 MB/s, 3000 IOPS) the same thing: the
 * FS lock was held for 7.9 s out of 16. Three paths held it through a trip to the disk:
 *   1) an fsync of a big file wrote ALL the dirty blocks under the lock;
 *   2) the eviction of a dirty data block — a synchronous write of 4 KB under the lock
 *      (426 us each on the throttled disk, thousands over a run);
 *   3) the eviction of dirty METAdata — a commit of the journal with three barriers
 *      under the lock.
 * While one thread waits for the disk under the lock, ANY operation of the FS stands for
 * everyone else — even a read of a block from the cache. From the outside it is "the database has stalled", inside it is a lock.
 *
 * How it is checked. Three threads on one volume:
 *   the writer      — writes a file twice as large as the block cache (evictions are
 *                     inevitable) and does an fsync, then re-reads it and compares every byte;
 *   the overwriter  — overwrites ITS OWN file round after round (a block given out to the
 *                     disk without the lock becomes dirty again while the write is in
 *                     flight) and at the end compares the last round;
 *   the neighbour   — does cheap FS operations all this time (a pread from the cache, an
 *                     fstat) and measures EVERY one: the largest latency and the number of
 *                     "stalls" longer than 20 ms.
 * Before the fix the neighbour stood for as long as the writer waited for the disk
 * (hundreds of ms on the throttled disk); after it, its worst operation is shorter than a
 * portion of the flush. The data is compared in full: a flush without the lock has no
 * right either to tear a block or to lose a repeated write.
 * musl static, -Wl,-Ttext-segment=0x10000000.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <pthread.h>
#include <time.h>
#include <sys/stat.h>
#include <errno.h>

#define BLK 4096
#define BIG_MB 64          /* twice the cache with 512 MB of memory (the cache = the memory/16) */
#define OVR_BLK 1024       /* the overwrite file: 4 MB */
#define OVR_ROUNDS 6
#define STALL_MS 20

static volatile int writer_done, overwriter_done;
static int fails;
static void chk(int ok, const char *what) {
    if (ok) printf("fsyncconc: ok %s\n", what);
    else { printf("fsyncconc: FAIL %s\n", what); fails++; }
}
static double now_ms(void) {
    struct timespec ts; clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e3 + ts.tv_nsec / 1e6;
}
static void fill(unsigned char *b, unsigned blk, unsigned seed) {
    for (int i = 0; i < BLK; i++) b[i] = (unsigned char)(blk * 31u + seed * 7u + i);
}
static int cmp(const unsigned char *got, unsigned blk, unsigned seed) {
    for (int i = 0; i < BLK; i++)
        if (got[i] != (unsigned char)(blk * 31u + seed * 7u + i)) return i;
    return -1;
}
static int verify(const char *path, unsigned nblk, unsigned (*seed_of)(unsigned)) {
    static __thread unsigned char buf[BLK];
    int fd = open(path, O_RDONLY);
    if (fd < 0) { printf("fsyncconc: %s did not open for reading\n", path); return 1; }
    for (unsigned b = 0; b < nblk; b++) {
        if (pread(fd, buf, BLK, (off_t)b * BLK) != BLK) { printf("fsyncconc: a short read of %s the block %u\n", path, b); close(fd); return 1; }
        int off = cmp(buf, b, seed_of(b));
        if (off >= 0) {
            printf("fsyncconc: A DIFFERENCE in %s the block %u the offset %d: read %02x, expected %02x\n",
                   path, b, off, buf[off], (unsigned char)(b * 31u + seed_of(b) * 7u + off));
            close(fd); return 1;
        }
    }
    close(fd); return 0;
}

/* ── the writer ───────────────────────────────────────────────────────── */
static double t_write_ms, t_fsync_ms;
static unsigned seed_big(unsigned b) { (void)b; return 11; }
static void *writer(void *arg) {
    static unsigned char buf[BLK];
    unsigned nblk = BIG_MB * 256u;
    int fd = open("/big.dat", O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { printf("fsyncconc: /big.dat was not created: errno=%d\n", errno); fails++; writer_done = 1; return arg; }
    double t0 = now_ms();
    for (unsigned b = 0; b < nblk; b++) {
        fill(buf, b, 11);
        if (write(fd, buf, BLK) != BLK) { printf("fsyncconc: a short write of the block %u\n", b); fails++; break; }
    }
    double t1 = now_ms();
    if (fsync(fd) != 0) { printf("fsyncconc: fsync failed\n"); fails++; }
    double t2 = now_ms();
    close(fd);
    t_write_ms = t1 - t0; t_fsync_ms = t2 - t1;
    writer_done = 1;
    chk(verify("/big.dat", nblk, seed_big) == 0, "the big file after the fsync reads back as what was written");
    unlink("/big.dat");
    return arg;
}

/* ── the overwriter ───────────────────────────────────────────────────── */
static unsigned seed_ovr(unsigned b) { (void)b; return 100 + OVR_ROUNDS - 1; }
static void *overwriter(void *arg) {
    static unsigned char buf[BLK];
    int fd = open("/ovr.dat", O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { printf("fsyncconc: /ovr.dat was not created\n"); fails++; overwriter_done = 1; return arg; }
    for (int r = 0; r < OVR_ROUNDS; r++)
        for (unsigned b = 0; b < OVR_BLK; b++) {
            /* An out-of-order order: next to the block that is in flight right now. */
            unsigned bb = (unsigned)(((unsigned long long)b * 613ull) % OVR_BLK);
            fill(buf, bb, 100 + r);
            if (pwrite(fd, buf, BLK, (off_t)bb * BLK) != BLK) { printf("fsyncconc: a short overwrite\n"); fails++; r = OVR_ROUNDS; break; }
        }
    fsync(fd); close(fd);
    overwriter_done = 1;
    chk(verify("/ovr.dat", OVR_BLK, seed_ovr) == 0, "a file overwritten round after round keeps the LAST round");
    unlink("/ovr.dat");
    return arg;
}

/* ── the neighbour ────────────────────────────────────────────────────── */
static double nb_max_ms; static unsigned long nb_ops, nb_stalls;
static void *neighbour(void *arg) {
    static unsigned char buf[BLK];
    int fd = open("/small.dat", O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { printf("fsyncconc: /small.dat was not created\n"); fails++; return arg; }
    fill(buf, 0, 5); write(fd, buf, BLK); fsync(fd);
    struct stat st;
    while (!(writer_done && overwriter_done)) {
        double t0 = now_ms();
        if (pread(fd, buf, BLK, 0) != BLK || fstat(fd, &st) != 0) { printf("fsyncconc: the neighbour: an operation failed\n"); fails++; break; }
        double d = now_ms() - t0;
        nb_ops++;
        if (d > nb_max_ms) nb_max_ms = d;
        if (d > STALL_MS) nb_stalls++;
    }
    close(fd); unlink("/small.dat");
    return arg;
}

int main(void) {
    pthread_t tw, to, tn;
    pthread_create(&tn, 0, neighbour, 0);
    pthread_create(&tw, 0, writer, 0);
    pthread_create(&to, 0, overwriter, 0);
    pthread_join(tw, 0); pthread_join(to, 0); pthread_join(tn, 0);
    printf("fsyncconc: the writer: %d MB write %.0f ms, fsync %.0f ms\n", BIG_MB, t_write_ms, t_fsync_ms);
    printf("fsyncconc: the neighbour: operations %lu, the worst %.1f ms, stalls longer than %d ms: %lu\n",
           nb_ops, nb_max_ms, STALL_MS, nb_stalls);
    /* The neighbour must live: not a single stall longer than 20 ms over the whole time
     * while the writer drives twice the cache onto the disk. Without the fix the stalls
     * are dozens (each one as long as a wait for the disk under the lock). */
    chk(nb_ops > 100, "the neighbour managed to work");
    chk(nb_stalls == 0, "the neighbour has not a single stall longer than 20 ms");
    printf("fsyncconc: %s\n", fails ? "FAILED" : "ALL OK");
    return fails != 0;
}
