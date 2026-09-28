/* tickcmd.c — a console command FROM A TICK on the BSP does not disturb the network's
 * syscalls on the other cores (talaria.test=tickcmd).
 *
 * What is caught. The console's flag "I am executing from a tick" was ONE PER MACHINE:
 * while the BSP printed the answer to `stat` from the tick's handler (hundreds of
 * milliseconds with IF=0), all the other cores considered themselves "in a tick", and the
 * network stack answered their syscalls with EAGAIN. `close()` cannot have such an answer
 * by contract — the workload does not repeat it, and the socket stays for ever (mongod: a
 * CLOSE-WAIT with an unread receive exactly at the moment of the counters' snapshot; the
 * slots ran out and the server stopped accepting).
 *
 * How it is checked. In this mode the kernel executes, once every few milliseconds, a
 * console command with a long printout from the BSP's tick. Three threads (the scheduler
 * spreads them over the APs) spin socket()+close() for two seconds and count the
 * refusals: there must be ZERO of them. A thread on the BSP cannot get a refusal by
 * construction (the tick interrupts it rather than running alongside), so only the APs are counted.
 * musl static, -Wl,-Ttext-segment=0x10000000.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

#define NTH 3
static long calls[NTH], refused[NTH], sock_fail[NTH];

static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec / 1e9; }

static void *worker(void *arg) {
    int k = (int)(long)arg;
    cpu_set_t cs; CPU_ZERO(&cs); CPU_SET(1 + k, &cs);
    sched_setaffinity(0, sizeof cs, &cs);
    double t0 = now();
    while (now() - t0 < 2.0) {
        int s = socket(AF_INET, SOCK_DGRAM, 0);
        calls[k]++;
        if (s < 0) { if (errno == EAGAIN) refused[k]++; else sock_fail[k]++; continue; }
        if (close(s) < 0) { if (errno == EAGAIN) refused[k]++; else sock_fail[k]++; }
    }
    return NULL;
}

int main(void) {
    pthread_t th[NTH];
    for (int k = 0; k < NTH; k++) pthread_create(&th[k], NULL, worker, (void *)(long)k);
    for (int k = 0; k < NTH; k++) pthread_join(th[k], NULL);
    long c = 0, r = 0, f = 0;
    for (int k = 0; k < NTH; k++) { c += calls[k]; r += refused[k]; f += sock_fail[k]; }
    printf("tickcmd: socket+close on the APs: calls %ld, EAGAIN refusals %ld, other errors %ld\n", c, r, f);
    if (c < 1000) { printf("tickcmd: FAILED — the threads hardly ran (%ld calls)\n", c); return 1; }
    if (r != 0 || f != 0) { printf("tickcmd: FAILED — a network syscall on a foreign core was refused because of the BSP's tick\n"); return 1; }
    printf("tickcmd: ALL OK\n");
    return 0;
}
