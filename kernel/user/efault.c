/* efault.c — regression for syscall pointer validation (talaria.test=efault).
 *
 * A process hands the kernel pointers it does not own: into the kernel image,
 * into the kernel's frame area (page tables, kernel stacks), unmapped,
 * non-canonical, NULL, a buffer that runs off the end of a mapping, a read-only
 * page as a destination. Every such call must fail with EFAULT and leave the
 * kernel intact; the same calls with valid buffers must still work (including
 * a demand-zero page the process has not touched yet).
 *
 * Raw syscall() everywhere: libc wrappers may copy through their own buffers
 * and would hide what the kernel does with the pointer. musl static. */
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <time.h>

static int fails;

static void expect_efault(long r, const char *what) {
    if (r == -1 && errno == EFAULT) printf("efault: ok %s -> EFAULT\n", what);
    else { printf("efault: FAIL %s -> r=%ld errno=%d\n", what, r, errno); fails++; }
}

static void expect_ok(long r, long want, const char *what) {
    if (r == want) printf("efault: ok %s -> %ld\n", what, r);
    else { printf("efault: FAIL %s -> r=%ld errno=%d (want %ld)\n", what, r, errno, want); fails++; }
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0); /* keep printf and raw write() in order */
    /* Kernel image (linked at 2 MiB) and the frame allocator area above it:
       identity-mapped in the process address space, but without U/S. */
    void *kimg = (void *)0x201000;
    void *kframes = (void *)0x3600000;
    void *unmapped = (void *)0x700000000000ull;
    void *noncanon = (void *)0xffff800000001000ull;
    char buf[64];
    struct timespec ts;

    int z = open("/dev/zero", O_RDONLY);
    expect_ok(z >= 0, 1, "open /dev/zero");

    /* The kernel would WRITE here. */
    expect_efault(syscall(SYS_getrandom, kimg, 16, 0), "getrandom(kernel image)");
    expect_efault(syscall(SYS_getrandom, kframes, 16, 0), "getrandom(kernel frames)");
    expect_efault(syscall(SYS_read, z, kframes, 64), "read(/dev/zero, kernel frames)");
    expect_efault(syscall(SYS_read, z, unmapped, 64), "read(/dev/zero, unmapped)");
    expect_efault(syscall(SYS_read, z, noncanon, 64), "read(/dev/zero, non-canonical)");
    expect_efault(syscall(SYS_read, z, NULL, 64), "read(/dev/zero, NULL)");
    expect_efault(syscall(SYS_fstat, 1, kframes), "fstat(1, kernel frames)");
    expect_efault(syscall(SYS_clock_gettime, CLOCK_MONOTONIC, kimg), "clock_gettime(kernel image)");
    expect_efault(syscall(SYS_uname, kframes), "uname(kernel frames)");
    expect_efault(syscall(SYS_pipe, kframes), "pipe(kernel frames)");

    /* The kernel would READ here (and hand the data back or out). */
    expect_efault(syscall(SYS_write, 1, kimg, 32), "write(stdout, kernel image)");
    struct iovec iov = { kimg, 32 };
    expect_efault(syscall(SYS_writev, 1, &iov, 1), "writev(stdout, iov -> kernel image)");
    expect_efault(syscall(SYS_writev, 1, kframes, 1), "writev(stdout, iov array in kernel)");
    expect_efault(syscall(SYS_open, kimg, O_RDONLY), "open(path in kernel image)");
    expect_efault(syscall(SYS_nanosleep, kframes, NULL), "nanosleep(kernel frames)");
    expect_efault(syscall(SYS_futex, kframes, 0 /* FUTEX_WAIT */, 0, NULL, NULL, 0), "futex(kernel frames)");

    /* A buffer that runs off the end of a mapping into an unmapped page. */
    char *two = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    munmap(two + 4096, 4096);
    expect_efault(syscall(SYS_read, z, two + 4096 - 16, 64), "read(/dev/zero, straddles unmapped page)");
    memset(two + 4096 - 4, 'a', 4); /* no NUL before the unmapped page */
    expect_efault(syscall(SYS_open, two + 4096 - 4, O_RDONLY), "open(path runs into unmapped page)");
    memcpy(two + 4096 - 4, "/x", 3);
    expect_ok(syscall(SYS_open, two + 4096 - 4, O_RDONLY) == -1 && errno == ENOENT, 1, "open(short path at page end) reaches the fs");

    /* Destination is read-only. */
    char *ro = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    expect_efault(syscall(SYS_read, z, ro, 16), "read(/dev/zero, read-only page)");

    /* Valid buffers keep working. */
    expect_ok(syscall(SYS_getrandom, buf, sizeof buf, 0), sizeof buf, "getrandom(stack)");
    expect_ok(syscall(SYS_read, z, buf, sizeof buf), sizeof buf, "read(/dev/zero, stack)");
    expect_ok(syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &ts), 0, "clock_gettime(stack)");
    char *fresh = mmap(NULL, 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    expect_ok(syscall(SYS_read, z, fresh + 12345, 100000), 100000, "read(/dev/zero, untouched demand-zero pages)");
    expect_ok(syscall(SYS_write, 1, "efault: write(stdout, rodata) ok\n", 33), 33, "write(stdout, rodata)");

    if (fails == 0) printf("efault: ALL OK\n");
    else printf("efault: %d FAILED\n", fails);
    return fails != 0;
}
