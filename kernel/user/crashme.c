#define _GNU_SOURCE
#include <stdio.h>
#include <pthread.h>
#include <unistd.h>
#include <sched.h>
#include <sys/wait.h>

/* crashme mt — the crash of ONE thread of a multithreaded workload. It is checked that
 * the whole process crashes (as on Linux) rather than one thread: until session 44 in the
 * cell mode the supervisor announced a halt while the sisters of the crashed thread went
 * on living on the other cores, and there was no core dump. The main thread prints "alive"
 * only three seconds after the sister's crash — time to write the dump, with a reserve. */
static void *sleeper(void *a) { (void)a; for (;;) sleep(1); return 0; }
/* crashme live — a long-living multithreaded workload for a restart by the supervisor
 * (the watchdog): one thread spins in ring 3 without syscalls, one sleeps on a condvar,
 * one is in nanosleep. The restart must take all three off wherever they stand, and
 * bring a new instance up on the same cores. */
static volatile unsigned long spins;
static void *spinner(void *a) { (void)a; for (;;) spins++; return 0; }
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static void *waiter(void *a) { (void)a; pthread_mutex_lock(&mu); for (;;) pthread_cond_wait(&cv, &mu); return 0; }
static void *boom(void *a) {
    (void)a;
    usleep(200000);
    volatile int *p = 0;
    *p = 42;                 /* a #PF in ring 3 — in a non-working thread */
    return 0;
}

int main(int argc, char **argv) {
    /* crashme peek — a read of the physical memory through the kernel's identity window.
     * Until session 44 the window 32 MB-1 GB was marked U/S in a process's AS, and the
     * workload read whatever it wanted: the page tables, the kernel stacks, other
     * processes. The correct outcome is a SIGSEGV on the very first read. */
    if (argc > 1 && argv[1][0] == 'p') {
        /* 128 MB into the RAM: GB0 on x86; on arm64's `virt` the RAM starts at 1 GB (the
         * same address there is the interrupt controller's window — closed too, but not
         * the memory this gate is about). */
#if defined(__aarch64__)
        volatile unsigned long *p = (volatile unsigned long *)0x48000000;
#else
        volatile unsigned long *p = (volatile unsigned long *)0x8000000;
#endif
        printf("crashme: reading the identity window at %p\n", (void *)p);
        fflush(stdout);
        unsigned long v = *p;
        printf("crashme: READ %#lx — the window is OPEN\n", v);
        fflush(stdout);
        return 0;
    }
    /* crashme forkap — the children that live and die on an AP. After a child exits the
     * AP idles with the CR3 of its address space, while the BSP frees that space: the
     * tables went out from under a core that stands on them. Forty children over three
     * APs; each one takes itself onto its own core. */
    if (argc > 1 && argv[1][0] == 'f') {
        int bad = 0;
        for (int i = 0; i < 40; i++) {
            pid_t pid = fork();
            if (pid == 0) {
                cpu_set_t set; CPU_ZERO(&set); CPU_SET(1 + i % 3, &set);
                sched_setaffinity(0, sizeof set, &set);
                volatile unsigned long x = 0;
                for (unsigned long k = 0; k < 2000000; k++) x += k;
                _exit(x == 0 ? 1 : 0);
            }
            int st = 0;
            if (pid < 0 || waitpid(pid, &st, 0) != pid || !WIFEXITED(st) || WEXITSTATUS(st) != 0) bad++;
        }
        printf("crashme: forkap %s (children 40, failures %d)\n", bad ? "FAIL" : "ALL OK", bad);
        fflush(stdout);
        return 0;
    }
    if (argc > 1 && argv[1][0] == 'l') {
        pthread_t a, b, c;
        pthread_create(&a, 0, spinner, 0);
        pthread_create(&b, 0, waiter, 0);
        pthread_create(&c, 0, sleeper, 0);
        printf("crashme: a live instance — the threads spin, wait and sleep\n");
        fflush(stdout);
        for (;;) sleep(1);
    }
    if (argc > 1 && argv[1][0] == 'm') {
        printf("crashme: multithreaded — three threads, one crashes\n");
        fflush(stdout);
        pthread_t a, b, c;
        pthread_create(&a, 0, sleeper, 0);
        pthread_create(&b, 0, sleeper, 0);
        pthread_create(&c, 0, boom, 0);
        for (int i = 0; ; i++) {
            usleep(100000);
            if (i >= 32) { printf("crashme: the main thread is alive after the sister's crash\n"); fflush(stdout); }
        }
    }
    printf("crashme: I work in ring 3, now I will dereference NULL and crash\n");
    fflush(stdout);
    volatile int* p = 0;
    *p = 42;                 // a #PF in ring 3
    printf("(this must not be printed)\n");
    return 0;
}
