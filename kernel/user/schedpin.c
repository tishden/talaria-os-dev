// SCHEDPIN: the pinning by the THREAD'S NAME from the manifest/the command line.
//
// A thread names itself (`prctl(PR_SET_NAME)`), the kernel looks at the rules and moves it
// — onto a specific core or onto a DEDICATED one whose number it picks itself.
//
// What is measured is the SETTLED state rather than the instantaneous one. The moment of
// the renaming is not yet the answer: the core given away as a dedicated one could at
// that instant be busy with a foreign thread that was executing and therefore did not
// move. The resettling is finished by the balancer, and the right question is "where did
// everyone end up once everything had settled". The check is DIRECT (getcpu): a metric
// can be plausible with the thing it checks completely absent (the pitfall of session 10).
//
// The run: talaria.test=schedpin talaria.smp=4 talaria.pin=md_*:dedicated,bg*:shared
// The build: musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x10000000 -o schedpin schedpin.c
#define _GNU_SOURCE
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <sys/syscall.h>

#define NMD 2   /* the threads that require a dedicated core */
#define NBG 4   /* the background threads of the common pool */
#define NW (NMD + NBG)
#define SETTLE_NS 3000000000ULL   /* how much we give the layout to settle */

static const char *names[NW] = { "md_feed", "md_book", "bg_log", "bg_gc", "bg_io", "bg_stat" };
static volatile unsigned cpu_before[NW], cpu_after[NW], cpu_now[NW];
static volatile int ready[NW], quit;
static char got_name[NW][20];

static unsigned mycpu(void) {
    unsigned c = ~0u;
    if (syscall(SYS_getcpu, &c, 0, 0) != 0) return ~0u;
    return c;
}
static uint64_t mono_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + ts.tv_nsec;
}

static void *worker(void *arg) {
    long i = (long)arg;
    cpu_before[i] = mycpu();
    /* the thread's name is the entry into the rules of the layout; after it the thread may MOVE */
    syscall(SYS_prctl, 15 /*PR_SET_NAME*/, (unsigned long)names[i], 0, 0, 0);
    cpu_after[i] = mycpu();
    syscall(SYS_prctl, 16 /*PR_GET_NAME*/, (unsigned long)got_name[i], 0, 0, 0);
    cpu_now[i] = cpu_after[i];
    ready[i] = 1;
    /* we keep the core busy and report all the time where we are now */
    while (!quit) cpu_now[i] = mycpu();
    return 0;
}

int main(void) {
    pthread_t t[NW];
    for (long i = 0; i < NW; i++)
        if (pthread_create(&t[i], 0, worker, (void *)i)) { dprintf(1, "PIN: FAIL pthread_create\n"); return 1; }
    for (int i = 0; i < NW; i++) while (!ready[i]) ;

    for (int i = 0; i < NW; i++)
        dprintf(1, "PIN: the thread %s: the core %u -> %u (the name from the OS kernel: %s)\n",
                names[i], cpu_before[i], cpu_after[i], got_name[i]);

    /* let the layout settle: the balancer works by the scheduler's ticks */
    uint64_t t0 = mono_ns();
    while (mono_ns() - t0 < SETTLE_NS) ;

    unsigned now[NW];
    for (int i = 0; i < NW; i++) now[i] = cpu_now[i];
    quit = 1;
    for (int i = 0; i < NW; i++) pthread_join(t[i], 0);

    unsigned per[64];
    for (int c = 0; c < 64; c++) per[c] = 0;
    int sane = 1;
    for (int i = 0; i < NW; i++) { if (now[i] >= 64) sane = 0; else per[now[i]]++; }

    /* md_* — each one alone on its own core, and the cores are different */
    int md_ok = sane && now[0] != now[1];
    for (int i = 0; i < NMD && md_ok; i++) if (per[now[i]] != 1) md_ok = 0;
    /* the background ones must not sit on the cores given away to md_* */
    int bg_ok = sane;
    for (int i = NMD; i < NW && bg_ok; i++)
        for (int j = 0; j < NMD; j++) if (now[i] == now[j]) bg_ok = 0;
    /* the names reached the OS kernel and came back */
    int name_ok = 1;
    for (int i = 0; i < NW; i++) if (strcmp(got_name[i], names[i]) != 0) name_ok = 0;

    dprintf(1, "PIN: it has settled: the dedicated cores %u,%u; the background ones on", now[0], now[1]);
    for (int i = NMD; i < NW; i++) dprintf(1, " %u", now[i]);
    dprintf(1, "\n");
    if (md_ok && bg_ok && name_ok)
        dprintf(1, "PIN: the name -> the rule -> a dedicated core ALL OK\n");
    else
        dprintf(1, "PIN: FAIL (dedicated=%d background=%d names=%d)\n", md_ok, bg_ok, name_ok);
    return 0;
}
