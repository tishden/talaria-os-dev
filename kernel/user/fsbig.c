/* fsbig.c — a file LARGER than the block cache: the write, the re-read, the comparison
 * (talaria.test=fsbig).
 *
 * Why a separate gate is needed. All the previous checks of the FS worked with files
 * that fit into the block cache (8 MB) entirely — that is, they checked the path where
 * the eviction of dirty pages almost never happens. And exactly where it happens all
 * the time the file system DOES BREAK: ClickHouse on a table of 10 million rows (some
 * fifty megabytes) could not, in about half of the runs, read what it had written
 * itself — "Cannot open file s.cmrk2", "Cannot read all data. Bytes read: 404,
 * expected 416", "Can't adjust last granule". From the outside it looks like a bug of
 * the database rather than a lost write, which is why it lived long: e2fsck after such
 * a run is clean (the structure is intact), it is the CONTENT that is spoiled.
 *
 * What is checked, in increasing order:
 *   1. One file of 32 MB: every 4 KB block is signed with its own number and seed,
 *      written sequentially, then read anew and compared in full.
 *   2. The same file read OUT OF ORDER (pread in a random order of the blocks):
 *      a sequential read may accidentally coincide with the order of the eviction.
 *   3. Four threads write into a file of 8 MB each at the same time and then each one
 *      compares its own: a mix of writing, eviction and reading on four cores — that is
 *      exactly the mode in which it used to break.
 *
 * The first byte that did not match is printed with its address: "the file is corrupted"
 * and "the file is corrupted RIGHT HERE" are different messages, and the second saves an evening.
 * musl static, -Wl,-Ttext-segment=0x10000000.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <pthread.h>
#include <sys/stat.h>
#include <errno.h>

#define BLK 4096
static int fails;
static void chk(int ok, const char *what) {
    if (ok) printf("fsbig: ok %s\n", what);
    else { printf("fsbig: FAIL %s\n", what); fails++; }
}

/* A block is signed so that ANY substitution is visible: the block's number, the seed and
 * the position inside the block. A coincidence by chance is excluded. */
static void fill(unsigned char *b, unsigned blk, unsigned seed) {
    for (int i = 0; i < BLK; i++) b[i] = (unsigned char)(blk * 31u + seed * 7u + i);
}

/* Returns -1 if everything matched, otherwise the offset of the first difference inside the block. */
static int cmp(const unsigned char *got, unsigned blk, unsigned seed) {
    for (int i = 0; i < BLK; i++)
        if (got[i] != (unsigned char)(blk * 31u + seed * 7u + i)) return i;
    return -1;
}

/* Write a file of nblk blocks, then read it and compare. rnd — read out of
 * order. Returns 0 if everything matched. */
static int one_file(const char *path, unsigned nblk, unsigned seed, int rnd) {
    static __thread unsigned char buf[BLK];
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { printf("fsbig: %s did not open\n", path); return 1; }
    for (unsigned b = 0; b < nblk; b++) {
        fill(buf, b, seed);
        if (write(fd, buf, BLK) != BLK) { printf("fsbig: a short write of the block %u\n", b); close(fd); return 1; }
    }
    if (fsync(fd) != 0) { printf("fsbig: fsync failed\n"); close(fd); return 1; }
    /* We re-read through a NEW descriptor: that way the read path does not reuse the
     * state left over from the write. */
    close(fd);
    fd = open(path, O_RDONLY);
    if (fd < 0) { printf("fsbig: %s did not open for reading\n", path); return 1; }
    int bad = 0;
    for (unsigned k = 0; k < nblk; k++) {
        /* The "out of order" order is deterministic (a step coprime with nblk), so
         * that a failure reproduces rather than comes on a whim. */
        unsigned b = rnd ? (unsigned)(((unsigned long long)k * 4099ull) % nblk) : k;
        ssize_t n = pread(fd, buf, BLK, (off_t)b * BLK);
        if (n != BLK) { printf("fsbig: a short read of the block %u: %ld instead of %d\n", b, (long)n, BLK); bad = 1; break; }
        int off = cmp(buf, b, seed);
        if (off >= 0) {
            printf("fsbig: A DIFFERENCE in %s the block %u the offset %d: read %02x, expected %02x\n",
                   path, b, off, buf[off], (unsigned char)(b * 31u + seed * 7u + off));
            bad = 1; break;
        }
    }
    close(fd);
    /* We clean up after ourselves at once: the peak occupancy of the volume is one file,
     * not the sum of them all. The gate needs a volume LARGER than the cache. */
    unlink(path);
    return bad;
}

/* ── Phase 4: the life cycle of a "part" the way MergeTree spins it ──────────
 * A directory of a dozen files of different sizes is written under a temporary name,
 * synced, renamed into the permanent one — and then deleted WHILE IT IS STILL BEING
 * READ. It is exactly this mix that was in no gate: we did check large files (the
 * phases 1-3), but not together with the rename of a directory and a deletion from
 * under an open descriptor. In ClickHouse on a table of 10 million rows an insert is cut
 * into several parts, they are merged in the background and the old ones are deleted —
 * and about half of the runs ended with "Cannot open file", "Cannot read all data" or a
 * broken granule. POSIX requires an open file to survive the removal of its name and to
 * give out exactly what was in it. */
#define PARTF 10
static int part_cycle(int id, int rounds) {
    static __thread unsigned char buf[BLK];
    char tmp[64], fin[64], f[128];
    int bad = 0;
    for (int r = 0; r < rounds && !bad; r++) {
        snprintf(tmp, sizeof tmp, "/tmp_%d_%d", id, r);
        snprintf(fin, sizeof fin, "/part_%d_%d", id, r);
        if (mkdir(tmp, 0755) != 0) { printf("fsbig: mkdir %s failed: errno=%d\n", tmp, errno); return 1; }
        for (int i = 0; i < PARTF; i++) {
            /* The spread of the sizes is like that of a real part: the marks are
             * kilobytes, the columns are megabytes. */
            unsigned nb = (i == 0) ? 1 : (unsigned)(1u << (i % 7));
            snprintf(f, sizeof f, "%s/f%d.bin", tmp, i);
            int fd = open(f, O_RDWR | O_CREAT | O_TRUNC, 0644);
            if (fd < 0) { printf("fsbig: %s was not created: errno=%d\n", f, errno); return 1; }
            for (unsigned b = 0; b < nb; b++) {
                fill(buf, b, (unsigned)(id * 100 + i));
                if (write(fd, buf, BLK) != BLK) { printf("fsbig: a short write of %s\n", f); close(fd); return 1; }
            }
            fsync(fd); close(fd);
        }
        if (rename(tmp, fin) != 0) { printf("fsbig: rename %s -> %s failed\n", tmp, fin); return 1; }

        /* Open ALL the files of the part, remove the names from under the open
         * descriptors and only then read: the content must stay the same. */
        int fds[PARTF];
        for (int i = 0; i < PARTF; i++) {
            snprintf(f, sizeof f, "%s/f%d.bin", fin, i);
            fds[i] = open(f, O_RDONLY);
            if (fds[i] < 0) { printf("fsbig: %s did not open after the rename\n", f); bad = 1; }
        }
        for (int i = 0; i < PARTF; i++) { snprintf(f, sizeof f, "%s/f%d.bin", fin, i); unlink(f); }
        for (int i = 0; i < PARTF && !bad; i++) {
            if (fds[i] < 0) continue;
            unsigned nb = (i == 0) ? 1 : (unsigned)(1u << (i % 7));
            for (unsigned b = 0; b < nb; b++) {
                ssize_t n = pread(fds[i], buf, BLK, (off_t)b * BLK);
                if (n != BLK) { printf("fsbig: the deleted-but-open f%d: a read of the block %u gave %ld\n", i, b, (long)n); bad = 1; break; }
                int off = cmp(buf, b, (unsigned)(id * 100 + i));
                if (off >= 0) { printf("fsbig: the deleted-but-open f%d the block %u: %02x instead of %02x\n", i, b, buf[off], (unsigned char)(b * 31u + (unsigned)(id*100+i) * 7u + off)); bad = 1; break; }
            }
        }
        for (int i = 0; i < PARTF; i++) if (fds[i] >= 0) close(fds[i]);
        rmdir(fin);
    }
    return bad;
}
static void *part_worker(void *p) { int *r = p; *r = part_cycle(*r, 6); return 0; }

struct arg { const char *path; unsigned nblk, seed; int bad; };
static void *worker(void *p) {
    struct arg *a = p;
    a->bad = one_file(a->path, a->nblk, a->seed, 1);
    return 0;
}

/* ── THE RACE FOR THE PATH BUFFER ─────────────────────────────────────────────
 *
 * The parsing of a path string happens OUTSIDE the FS lock, while what is given out is
 * a SLICE of a kernel buffer that lives until the end of the syscall. While a syscall
 * cannot be interrupted the buffer may be kept per core; as soon as a VOLUNTARY yield
 * appeared inside it (a thread waiting for the FS lock gives the processor to a
 * neighbour), the slice survives a task switch — and the neighbour on the same core
 * writes its own path into it.
 *
 * THIS CANNOT BE CAUGHT BY THE ABSENCE OF A FILE: a substituted path most often points
 * at SOMEONE ELSE'S EXISTING file, and stat honestly returns success. That is why every
 * thread has its own files with a UNIQUE SIZE (1000*id + k): any substitution is visible
 * at once and exactly, rather than "sometimes and as an ENOENT".
 *
 * There are twice as many threads as cores — otherwise there is nobody to yield to and
 * there is no race. An fsync goes alongside, so the FS lock is held long and waiters yield. */
#define PR_FILES 8
struct prarg { int id; int bad; int checks; };
static int pr_prepare(int id) {
    char d[64], f[128];
    snprintf(d, sizeof d, "/pr_t%d", id);
    mkdir(d, 0755);
    for (int k = 0; k < PR_FILES; k++) {
        snprintf(f, sizeof f, "%s/granule_marks_%02d.cmrk2", d, k);
        int fd = open(f, O_CREAT | O_WRONLY | O_TRUNC, 0644);
        if (fd < 0) return 1;
        size_t want = (size_t)(1000 * id + k);
        static char z[9000];
        if (write(fd, z, want) != (ssize_t)want) { close(fd); return 1; }
        close(fd);
    }
    return 0;
}
static void *pr_worker(void *p) {
    struct prarg *a = p;
    char f[128], scratch[64];
    snprintf(scratch, sizeof scratch, "/pr_t%d/scratch.tmp", a->id);
    static char buf[65536];
    for (int it = 0; it < 60; it++) {
        for (int k = 0; k < PR_FILES; k++) {
            struct stat st;
            snprintf(f, sizeof f, "/pr_t%d/granule_marks_%02d.cmrk2", a->id, k);
            a->checks++;
            if (stat(f, &st) != 0) {
                if (a->bad++ == 0) printf("fsbig: the path: stat(%s) -> the error %d\n", f, errno);
                continue;
            }
            if (st.st_size != (off_t)(1000 * a->id + k)) {
                if (a->bad++ == 0)
                    printf("fsbig: the path: stat(%s) gave out SOMEONE ELSE'S file (the size %lld, we expected %d)\n",
                           f, (long long)st.st_size, 1000 * a->id + k);
            }
        }
        /* we hold the FS lock for a long time — so that the waiters have something to yield */
        int fd = open(scratch, O_CREAT | O_WRONLY | O_TRUNC, 0644);
        if (fd >= 0) { write(fd, buf, sizeof buf); fsync(fd); close(fd); }
    }
    return 0;
}
static int path_race(void) {
    enum { NTH = 8 };
    pthread_t t[NTH];
    struct prarg a[NTH];
    for (int i = 0; i < NTH; i++) {
        a[i].id = i + 1; a[i].bad = 0; a[i].checks = 0;
        if (pr_prepare(a[i].id)) { printf("fsbig: the path: the tree of the thread %d could not be prepared\n", i + 1); return 1; }
    }
    for (int i = 1; i < NTH; i++) pthread_create(&t[i], 0, pr_worker, &a[i]);
    pr_worker(&a[0]);
    for (int i = 1; i < NTH; i++) pthread_join(t[i], 0);
    int bad = 0, checks = 0;
    for (int i = 0; i < NTH; i++) { bad += a[i].bad; checks += a[i].checks; }
    printf("fsbig: the path: checks %d, substitutions %d\n", checks, bad);
    return bad;
}

int main(int argc, char **argv) {
    /* The race of the parts is put behind an argument, because it reproduces a
     * CLASS of defect (a shared path buffer under a concurrent FS) rather than a
     * single case, and the reproduction costs forty seconds instead of a
     * five-minute run of ClickHouse.
     *   talaria.test=fsbig      — what must work;
     *   talaria.test=fsbigrace  — the same plus the MergeTree part life cycle.
     * Both are in the regression. */
    int race = (argc > 1 && strcmp(argv[1], "race") == 0);
    /* 32 MB is four times larger than the block cache (8 MB): the eviction of dirty
     * pages goes on the whole run rather than now and then. */
    chk(one_file("/big1.dat", 8192, 1, 0) == 0, "a file of 32 MB: written and read sequentially without differences");
    chk(one_file("/big2.dat", 8192, 2, 1) == 0, "a file of 32 MB: an out-of-order read without differences");

    pthread_t t[3];
    struct arg a[4] = {
        { "/big_a.dat", 2048, 11, 0 }, { "/big_b.dat", 2048, 22, 0 },
        { "/big_c.dat", 2048, 33, 0 }, { "/big_d.dat", 2048, 44, 0 },
    };
    for (int i = 0; i < 3; i++) pthread_create(&t[i], 0, worker, &a[i]);
    worker(&a[3]);
    for (int i = 0; i < 3; i++) pthread_join(t[i], 0);
    chk(!a[0].bad && !a[1].bad && !a[2].bad && !a[3].bad,
        "four threads: 8 MB each, a simultaneous write/eviction/read");

    chk(part_cycle(0, 6) == 0, "the parts of MergeTree: a directory of files, a rename, a read from under deleted names");
    chk(path_race() == 0, "a syscall's path belongs to ITS OWN task: eight threads on four cores, a stat of their own files under an fsync load");
    if (race) {
        pthread_t pt[3]; int id[4] = {1, 2, 3, 4};
        for (int i = 0; i < 3; i++) pthread_create(&pt[i], 0, part_worker, &id[i]);
        part_worker(&id[3]);
        for (int i = 0; i < 3; i++) pthread_join(pt[i], 0);
        chk(!id[0] && !id[1] && !id[2] && !id[3], "four threads spin the parts at the same time (a write, a rename, a deletion from under a read)");
    }

    printf(fails ? "FSBIG: %d FAIL\n" : "FSBIG: ALL OK\n", fails);
    return fails != 0;
}
