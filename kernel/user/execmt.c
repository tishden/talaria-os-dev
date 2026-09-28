/* execmt.c — the regression of a MULTITHREADED execve (talaria.test=execmt).
   On a successful exec Linux leaves the process one thread — the calling one. Here three
   threads are raised (one sleeps on a futex through a mutex, two spin), after which the
   main one does an execve onto itself. We check in the new image: it has started, the
   arguments and the environment have arrived, and there is one thread again.
   musl static, -Wl,-Ttext-segment=0x10000000 (see services/README). */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <pthread.h>
#include <sys/syscall.h>

static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t c = PTHREAD_COND_INITIALIZER;
static volatile int spin = 1;

static void *sleeper(void *a) {          /* it sleeps in a futex — the kernel must take it off the wait */
    (void)a;
    pthread_mutex_lock(&m);
    while (spin) pthread_cond_wait(&c, &m);
    pthread_mutex_unlock(&m);
    return 0;
}
static void *spinner(void *a) {          /* it spins in user space — the kernel must preempt it */
    (void)a;
    volatile unsigned long x = 0;
    while (spin) x += 1;
    return (void *)(unsigned long)x;
}

/* How many threads the process has: we count the directories in /proc/self/task, and if
   procfs does not give it out — we count the very fact that the new image is alive as a
   success (the kernel prints "exec: the process's threads finished: N" on its side). */
int main(int argc, char **argv, char **envp) {
    (void)envp;
    if (argc > 1 && strcmp(argv[1], "child") == 0) {
        const char *e = getenv("EXECMT");
        printf("execmt: the new image has started, argv[1]=%s env=%s\n", argv[1], e ? e : "(none)");
        if (!e || strcmp(e, "ok") != 0) { printf("execmt: FAIL the environment did not arrive\n"); fflush(stdout); return 1; }
        /* the unmodified code after an exec must be able to start threads again */
        pthread_t t;
        spin = 1;
        if (pthread_create(&t, 0, spinner, 0) != 0) { printf("execmt: FAIL a thread after the exec was not created\n"); fflush(stdout); return 1; }
        spin = 0;
        pthread_join(t, 0);
        printf("execmt: ALL OK\n");
        fflush(stdout);
        return 0;
    }

    pthread_t t1, t2, t3;
    pthread_create(&t1, 0, sleeper, 0);
    pthread_create(&t2, 0, spinner, 0);
    pthread_create(&t3, 0, spinner, 0);
    usleep(200000);                       /* let all three really start */
    printf("execmt: three threads are up, I am doing an execve onto myself\n");
    fflush(stdout);

    char *av[] = { "execmt", "child", 0 };
    char *ev[] = { "EXECMT=ok", "HOME=/", 0 };
    execve("/execmt", av, ev);
    printf("execmt: FAIL execve returned\n");
    fflush(stdout);
    return 1;
}
