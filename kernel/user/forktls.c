// AFTER A fork THE CHILD'S TLS MUST BE INTACT.
//
// On the stand on the 8th of September a PostgreSQL backend crashed inside musl's
// `fork()`: `__errno_location()` returned 0x131, that is, `__pthread_self()` (a read of
// `%fs:0`) gave out garbage — the child's TLS was broken. The diagnosis cost the
// disassembly of a foreign binary; here the same invariant is checked head-on and on our own code.
//
// The address space is deliberately LARGE (a COW clone copies the tables, and the
// postmaster has many of them: in the stand's console "COW copies: 7258"), and the forks
// go one after another in hundreds — on the stand the crash happened after 119
// connections, that is, the event is rare.
#define _GNU_SOURCE
#include <sys/mman.h>
#include <sys/wait.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <errno.h>

static __thread volatile unsigned long canary;

static unsigned long tp(void) {              /* the TLS self-pointer: [fs:0] */
    unsigned long v;
    __asm__ volatile ("mov %%fs:0, %0" : "=r"(v));
    return v;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int n = 100, mb = 1500;
    if (argc > 1) n = atoi(argv[1]);
    if (argc > 2) mb = atoi(argv[2]);
    if (n <= 0) n = 400;
    if (mb < 0) mb = 0;

    /* An address space LIKE THE POSTMASTER'S: not one piece but many different mappings
     * with different rights. One big piece is carried over by a COW clone with a single
     * record of the directory, and the path on which PostgreSQL crashes (7720 COW copies)
     * is not gone through that way. */
    int nmaps = (argc > 3) ? atoi(argv[3]) : 200;
    if (nmaps < 0) nmaps = 0;
    size_t area = (size_t)mb << 20;
    if (area) {
        char *p = mmap(NULL, area, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { printf("forktls: mmap %d MB REFUSED\n", mb); return 1; }
        for (size_t o = 0; o < area; o += 4096) p[o] = (char)o;   /* the pages are REALLY brought up */
    }
    for (int k = 0; k < nmaps; k++) {
        size_t len = (size_t)((k % 7) + 1) * 4096;
        char *q = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (q == MAP_FAILED) { nmaps = k; break; }
        for (size_t o = 0; o < len; o += 4096) q[o] = (char)k;
        if (k % 3 == 0) mprotect(q, 4096, PROT_READ);            /* and different rights */
    }

    unsigned long parent_tp = tp();
    canary = 0xC0FFEE;
    printf("forktls: the address space is %d MB + %d mappings, the forks %d, the parent's TLS %#lx\n", mb, nmaps, n, parent_tp);

    int bad = 0, ok = 0;
    for (int i = 0; i < n; i++) {
        pid_t c = fork();
        if (c == 0) {
            /* The FIRST thing the child does is to compare the TLS. It is exactly here that
             * the backend crashed: musl goes into `errno` through `%fs` right after the clone. */
            unsigned long t = tp();
            int e = (t != parent_tp) || (canary != 0xC0FFEE);
            if (!e) { errno = 0; e = (errno != 0); }   /* and through musl itself */
            _exit(e ? 2 : 0);
        }
        if (c < 0) { printf("forktls: fork REFUSED at %d (%s)\n", i, strerror(errno)); break; }
        /* THE PARENT IS CHECKED TOO. On the stand the PostgreSQL postmaster crashed not in
         * the child but on the `retq` at the end of `_Fork` — with a non-canonical return
         * address, that is, with ITS OWN stack spoilt. */
        volatile unsigned long stack_canary = 0xDEADBEEFCAFEUL;
        int st = 0;
        waitpid(c, &st, 0);
        if (stack_canary != 0xDEADBEEFCAFEUL || tp() != parent_tp) {
            printf("forktls: THE PARENT is spoilt at the fork %d (the canary %#lx, the TLS %#lx)\n",
                   i, stack_canary, tp());
            bad++;
            break;
        }
        if (WIFEXITED(st) && WEXITSTATUS(st) == 0) ok++;
        else { bad++; if (bad <= 3) printf("forktls: the child %d DID NOT MATCH (the code %d)\n", i, WEXITSTATUS(st)); }
    }
    printf("forktls: matched %d, did not match %d\n", ok, bad);
    printf("forktls: %s\n", (bad == 0 && ok == n) ? "ALL OK" : "FAILED");
    return 0;
}
