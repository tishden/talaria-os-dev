/* shmrace: TWO THREADS CREATE SHARED MEMORY AT THE SAME INSTANT.
 *
 * What it is for. `shm_open(O_CREAT)` and `shmget(IPC_CREAT)` are compound
 * operations: find the object by name or key, and if there is none, take a free
 * row of the kernel's table. Done without a lock, two threads that both miss the
 * lookup both take a row — and the process gets TWO objects where POSIX promises
 * one: each half of a workload then maps its own memory and they never see each
 * other. Nothing crashes; the halves simply do not communicate.
 *
 * So the check is not "shm works" but "shm works WHEN TWO THREADS RACE":
 *   1. the same NAME from N threads  -> one object: every thread writes its own
 *      slot through its own descriptor and must see ALL the slots;
 *   2. distinct names from N threads -> distinct objects: a row taken twice would
 *      show up as somebody else's pattern in my own object;
 *   3. the same SysV KEY from N threads -> one id and one segment (the same
 *      slot check).
 * The threads meet at a spin barrier before every phase: without it they arrive
 * one after another and the race never happens. The kernel counts how many times
 * a creator actually had to wait (`stat: shm: …`), and the gate reads that too —
 * a race that did not happen proves nothing. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/shm.h>
#include <sys/stat.h>
#include <unistd.h>
#include "tal_arch.h"

#define NT 4
#define SZ 4096

static volatile int barrier_cnt, barrier_gen;
static pthread_mutex_t bm = PTHREAD_MUTEX_INITIALIZER;

/* A barrier on a mutex plus a spin: pthread_barrier is not in every musl build,
   and the point here is only that the threads leave together. */
static void meet(void)
{
    pthread_mutex_lock(&bm);
    int gen = barrier_gen;
    if (++barrier_cnt == NT) { barrier_cnt = 0; barrier_gen++; }
    pthread_mutex_unlock(&bm);
    while (barrier_gen == gen) { tal_relax(); }
}

static int fails;
static void fail(const char *what, long a, long b)
{
    __sync_fetch_and_add(&fails, 1);
    printf("shmrace: FAIL %s (%ld vs %ld)\n", what, a, b);
}

struct slot { volatile unsigned mark[NT]; };

static void *worker(void *arg)
{
    long id = (long)arg;

    /* 1. ONE name from every thread */
    meet();
    int fd = shm_open("/race-one", O_CREAT | O_RDWR, 0600);
    if (fd < 0) { fail("shm_open(same)", fd, 0); return 0; }
    struct slot *p = mmap(0, SZ, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (p == MAP_FAILED) { fail("mmap(same)", -1, 0); return 0; }
    p->mark[id] = 0xA0 + id;
    meet();
    for (int i = 0; i < NT; i++)
        if (p->mark[i] != (unsigned)(0xA0 + i)) fail("one name -> one object", p->mark[i], 0xA0 + i);

    /* 2. distinct names: my object must hold only MY pattern */
    meet();
    char name[32];
    snprintf(name, sizeof name, "/race-%ld", id);
    int fd2 = shm_open(name, O_CREAT | O_RDWR, 0600);
    if (fd2 < 0) { fail("shm_open(own)", fd2, 0); return 0; }
    struct slot *q = mmap(0, SZ, PROT_READ | PROT_WRITE, MAP_SHARED, fd2, 0);
    if (q == MAP_FAILED) { fail("mmap(own)", -1, 0); return 0; }
    for (int i = 0; i < NT; i++) q->mark[i] = 0;
    q->mark[id] = 0xB0 + id;
    meet();
    for (int i = 0; i < NT; i++) {
        unsigned want = (i == id) ? (unsigned)(0xB0 + id) : 0u;
        if (q->mark[i] != want) fail("distinct names -> distinct objects", q->mark[i], want);
    }

    /* 3. one SysV key from every thread */
    meet();
    int sid = shmget(0x7a1a, SZ, IPC_CREAT | 0600);
    if (sid < 0) { fail("shmget(same key)", sid, 0); return 0; }
    struct slot *s = shmat(sid, 0, 0);
    if (s == (void *)-1) { fail("shmat", -1, 0); return 0; }
    s->mark[id] = 0xC0 + id;
    meet();
    for (int i = 0; i < NT; i++)
        if (s->mark[i] != (unsigned)(0xC0 + i)) fail("one key -> one segment", s->mark[i], 0xC0 + i);
    return 0;
}

int main(void)
{
    pthread_t t[NT];
    for (long i = 0; i < NT; i++)
        if (pthread_create(&t[i], 0, worker, (void *)i) != 0) { printf("shmrace: pthread_create failed\n"); return 1; }
    for (int i = 0; i < NT; i++) pthread_join(t[i], 0);
    if (fails == 0) printf("shmrace: ALL OK (%d threads, one name, own names, one SysV key)\n", NT);
    else printf("shmrace: FAILURES %d\n", fails);
    return fails != 0;
}
