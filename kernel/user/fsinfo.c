/* fsinfo.c — what the OS tells about the file system (talaria.test=fsinfo).
 *
 * An application asks about the free space not out of curiosity: a database reserves the
 * room for a part of a table BEFORE writing and, not having got it, honestly refuses to
 * work. While `statfs` zeroed the buffer and returned success, this looked like "the file
 * system is there, the free blocks are zero", and ClickHouse crashed on
 * `CREATE TABLE ... ENGINE = MergeTree` with "Cannot reserve 4.56 MiB".
 * A silent zero is worse than a refusal: a refusal is visible at once, a zero half an
 * hour later and in somebody else's code.
 *
 * What is checked: the block size is sane, the blocks and the inodes are not zero, the
 * free ones are not more than the total, `fstatfs` on an open file says the same as
 * `statfs` on a path, and the declared size of the volume looks true (tens of megabytes).
 * musl static, -Wl,-Ttext-segment=0x10000000.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/vfs.h>

static int fails = 0;
static void chk(int ok, const char *what) {
    if (ok) printf("fsinfo: ok %s\n", what);
    else { printf("fsinfo: FAIL %s\n", what); fails++; }
    fflush(stdout);
}

int main(void) {
    struct statfs a, b;
    memset(&a, 0, sizeof a);
    memset(&b, 0, sizeof b);

    chk(statfs("/", &a) == 0, "statfs(\"/\") has worked");
    printf("fsinfo: the block %ld, the blocks %llu, free %llu, the inodes %llu, free inodes %llu\n",
           (long)a.f_bsize, (unsigned long long)a.f_blocks, (unsigned long long)a.f_bfree,
           (unsigned long long)a.f_files, (unsigned long long)a.f_ffree);
    fflush(stdout);

    chk(a.f_bsize >= 512 && a.f_bsize <= 65536, "the block size is sane");
    chk(a.f_blocks > 0, "the blocks are not zero");
    chk(a.f_bfree > 0 && a.f_bfree <= a.f_blocks, "the free blocks are neither zero nor more than the total");
    chk(a.f_files > 0, "the inodes are not zero");
    chk(a.f_ffree > 0 && a.f_ffree <= a.f_files, "the free inodes are neither zero nor more than the total");
    /* the volume in the test is 64 MB; we check the order of magnitude, not the exact number */
    unsigned long long bytes = (unsigned long long)a.f_bsize * a.f_blocks;
    chk(bytes >= 4ull * 1024 * 1024, "the declared size of the volume is megabytes rather than bytes");

    int fd = open("/hello.txt", O_RDONLY);
    if (fd < 0) { printf("fsinfo: FAIL /hello.txt did not open\n"); fails++; }
    else {
        chk(fstatfs(fd, &b) == 0, "fstatfs on an open file has worked");
        chk(b.f_bsize == a.f_bsize && b.f_blocks == a.f_blocks,
            "fstatfs says the same as statfs on a path");
        close(fd);
    }

    printf("fsinfo: %s\n", fails ? "FAILED" : "ALL OK");
    fflush(stdout);
    return fails ? 1 : 0;
}
