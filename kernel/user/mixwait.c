/* mixwait: A WAITER WHOSE EPOLL SET IS NOT ONLY SOCKETS, PARKED, MUST BE WOKEN BY THE EVENT.
 *
 * What it is for. With `talaria.netmix=<ns>` a network waiter whose set holds a pipe, an
 * eventfd or a signalfd parks (one waiter stays the stack's poller and spins); nothing on
 * the network moves its word, so the producers of those readinesses kick it (`mix_kick`:
 * a pipe's write, an eventfd's write, a signal made pending). A kick that is missing costs
 * the park's ceiling — the gate boots with a ceiling of 2 s, so a missing kick reads as a
 * wake of ~2000 ms, not as a hang.
 *
 * Two waiter threads, each with its own epoll set {a pipe's read end, an eventfd, a
 * signalfd for SIGUSR1}, both inside epoll_wait at once (one of them is the poller, the
 * other parks); the main thread, 30 ms after both are in, fires one source of one waiter
 * and the waiter stamps the time it returned. ROUNDS rounds over the three sources and the
 * two waiters; the verdict is the worst wake. */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/signalfd.h>
#include <time.h>
#include <unistd.h>

#define ROUNDS 6
#define LIMIT_MS 200.0   /* far below the 2 s ceiling, far above TCG's scheduling noise */

static double now(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec * 1e-6;
}

struct waiter {
    pthread_t th;
    int ep, pfd[2], efd, sfd;
    volatile int inside;     /* the round number the waiter is waiting in */
    volatile int done;       /* the round number it returned from */
    volatile double t_ret;   /* when it returned */
    volatile int got;        /* the readiness it saw: 1 pipe, 2 eventfd, 3 signal */
};
static struct waiter w[2];
static volatile int round_no;

static void *waiter_main(void *a) {
    struct waiter *me = a;
    sigset_t m; sigemptyset(&m); sigaddset(&m, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &m, 0);
    me->sfd = signalfd(-1, &m, SFD_NONBLOCK);
    struct epoll_event ev = {0};
    ev.events = EPOLLIN; ev.data.u32 = 1; epoll_ctl(me->ep, EPOLL_CTL_ADD, me->pfd[0], &ev);
    ev.data.u32 = 2; epoll_ctl(me->ep, EPOLL_CTL_ADD, me->efd, &ev);
    ev.data.u32 = 3; epoll_ctl(me->ep, EPOLL_CTL_ADD, me->sfd, &ev);
    for (int r = 1; r <= 3 * ROUNDS; r++) {
        while (round_no < r) ;
        me->inside = r;
        struct epoll_event out[4];
        int n = epoll_wait(me->ep, out, 4, 5000);
        me->t_ret = now();
        me->got = n > 0 ? (int)out[0].data.u32 : -n;
        /* consume what fired */
        char b[64]; uint64_t v; struct signalfd_siginfo si;
        if (me->got == 1) read(me->pfd[0], b, sizeof b);
        if (me->got == 2) read(me->efd, &v, 8);
        if (me->got == 3) read(me->sfd, &si, sizeof si);
        me->done = r;
    }
    return 0;
}

int main(void) {
    sigset_t m; sigemptyset(&m); sigaddset(&m, SIGUSR1);
    pthread_sigmask(SIG_BLOCK, &m, 0);   /* the threads inherit it: SIGUSR1 is the signalfd's */
    for (int i = 0; i < 2; i++) {
        w[i].ep = epoll_create1(0); pipe(w[i].pfd); w[i].efd = eventfd(0, EFD_NONBLOCK);
        pthread_create(&w[i].th, 0, waiter_main, &w[i]);
    }
    double worst = 0; int bad = 0;
    const char *src[] = {"", "pipe", "eventfd", "signal"};
    for (int r = 1; r <= 3 * ROUNDS; r++) {
        round_no = r;
        while (w[0].inside < r || w[1].inside < r) ;
        struct timespec ts = {0, 30 * 1000000}; nanosleep(&ts, 0);  /* both well inside the wait */
        int k = 1 + (r - 1) % 3;
        for (int i = 0; i < 2; i++) {
            double t0 = now();
            if (k == 1) write(w[i].pfd[1], "x", 1);
            if (k == 2) { uint64_t one = 1; write(w[i].efd, &one, 8); }
            if (k == 3) pthread_kill(w[i].th, SIGUSR1);
            while (w[i].done < r) ;
            double d = w[i].t_ret - t0;
            if (d > worst) worst = d;
            if (w[i].got != k) { bad++; printf("mixwait: round %d waiter %d: fired %s, saw %d\n", r, i, src[k], w[i].got); }
            if (d > LIMIT_MS) { bad++; printf("mixwait: round %d waiter %d: the %s woke it after %.1f ms\n", r, i, src[k], d); }
        }
    }
    for (int i = 0; i < 2; i++) pthread_join(w[i].th, 0);
    printf("mixwait: %d wakes by a pipe, an eventfd and a signal, the worst %.1f ms (the limit %.0f)\n", 6 * ROUNDS, worst, LIMIT_MS);
    puts(bad ? "mixwait: FAILURES" : "mixwait: ALL OK");
    return 0;
}
