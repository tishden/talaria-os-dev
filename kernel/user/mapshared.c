/* mapshared — a file mmap(MAP_SHARED) on Talaria's ext2 (PLAN-V1.1 §8.5).
   It checks what the mapping is made through the kernel's block cache for:
     1. a write through the mapping is visible to a read() of THE SAME file (the coherence),
     2. msync takes it down to the disk,
     3. after a munmap/a repeated open the content is in place,
     4. a read of the mapping sees what has been written through write().
   It prints mapshared: ALL OK. musl static. */
#include <stdio.h>
#include <string.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/mman.h>

#define N 8192
static int fails;
#define CHECK(c, m) do { if (c) printf("mapshared: ok   %s\n", m); \
                         else { printf("mapshared: FAIL %s\n", m); fails++; } } while (0)

int main(void) {
    int fd = open("/mstest.dat", O_RDWR | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { printf("mapshared: open FAILED\n"); return 1; }
    if (ftruncate(fd, N) != 0) printf("mapshared: ftruncate did not go through (we continue)\n");

    char *m = mmap(0, N, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(m != MAP_FAILED, "mmap(MAP_SHARED, fd) has been accepted");
    if (m == MAP_FAILED) { printf("mapshared: FAILED\n"); return 1; }

    for (int i = 0; i < N; i++) m[i] = (char)(i * 31 + 7);
    CHECK(m[0] == 7 && (unsigned char)m[N - 1] == (unsigned char)((N - 1) * 31 + 7), "a write through the mapping");

    /* the coherence: a read() of the same file must see what has been written through the memory */
    char buf[256];
    CHECK(pread(fd, buf, sizeof buf, 0) == (long)sizeof buf, "a pread after a write into the mapping");
    int coh = 1;
    for (int i = 0; i < (int)sizeof buf; i++) if (buf[i] != (char)(i * 31 + 7)) { coh = 0; break; }
    CHECK(coh, "read() sees the bytes written through the mapping");

    CHECK(msync(m, N, MS_SYNC) == 0, "msync(MS_SYNC)");

    /* the other direction: a write() into the file is visible in the mapping */
    char probe[16];
    memset(probe, 'Z', sizeof probe);
    CHECK(pwrite(fd, probe, sizeof probe, 4096) == (long)sizeof probe, "a pwrite into the middle of the file");
    CHECK(m[4096] == 'Z' && m[4096 + 15] == 'Z', "the mapping sees the bytes written through write()");

    CHECK(munmap(m, N) == 0, "munmap");
    close(fd);

    /* the content has survived the removal of the mapping */
    fd = open("/mstest.dat", O_RDONLY);
    CHECK(fd >= 0, "a repeated open");
    CHECK(pread(fd, buf, sizeof buf, 0) == (long)sizeof buf, "a read after the munmap");
    int keep = 1;
    for (int i = 0; i < (int)sizeof buf; i++) if (buf[i] != (char)(i * 31 + 7)) { keep = 0; break; }
    CHECK(keep, "the mapping's data has been kept in the file");
    close(fd);

    /* §3.3: a file SCATTERED over the disk. The blocks are allocated in runs (the window
       of the preallocation), and close() gives the remainder of the window back — so the
       alternation "write into A, close, write into B, open A again" lays A down in pieces
       with holes. Such a mapping runs into the layout of the block cache: while the kernel
       could map only a contiguous window of slots, an ENODEV was returned here. */
    const int FN = 8;                       /* 8 pieces of 4 blocks */
    for (int r = 0; r < FN; r++) {
        int a = open("/frag_a.dat", O_RDWR | O_CREAT | (r ? O_APPEND : O_TRUNC), 0644);
        int b = open("/frag_b.dat", O_RDWR | O_CREAT | (r ? O_APPEND : O_TRUNC), 0644);
        if (a < 0 || b < 0) { printf("mapshared: FAIL to open the frag files\n"); fails++; break; }
        char blk[4096];
        for (int k = 0; k < 4; k++) {
            memset(blk, 'A' + r, sizeof blk);
            if (write(a, blk, sizeof blk) != sizeof blk) { fails++; }
            memset(blk, 'b', sizeof blk);
            if (write(b, blk, sizeof blk) != sizeof blk) { fails++; }
        }
        close(a); close(b);                 /* close gives the remainder of the allocation window back */
    }
    int fa = open("/frag_a.dat", O_RDWR);
    CHECK(fa >= 0, "frag: the scattered file has been opened");
    long fsz = (long)FN * 4 * 4096;
    char *fm = mmap(0, fsz, PROT_READ | PROT_WRITE, MAP_SHARED, fa, 0);
    CHECK(fm != MAP_FAILED, "frag: mmap(MAP_SHARED) of a scattered file (there used to be an ENODEV)");
    if (fm != MAP_FAILED) {
        int okr = 1;
        for (int r = 0; r < FN; r++) if (fm[(long)r * 4 * 4096] != 'A' + r) { okr = 0; break; }
        CHECK(okr, "frag: the mapping sees the file's pieces in the right order");
        fm[0] = 'X'; fm[fsz - 1] = 'Y';
        CHECK(msync(fm, fsz, MS_SYNC) == 0, "frag: msync");
        CHECK(munmap(fm, fsz) == 0, "frag: munmap");
        char t[2];
        CHECK(pread(fa, t, 1, 0) == 1 && t[0] == 'X', "frag: an edit through the mapping reached the file");
        CHECK(pread(fa, t, 1, fsz - 1) == 1 && t[0] == 'Y', "frag: an edit of the last page reached the file");
    }
    if (fa >= 0) close(fa);

    printf("mapshared: %s\n", fails ? "FAILED" : "ALL OK");
    return fails ? 1 : 0;
}
