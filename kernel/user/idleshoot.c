// The experiment: does an IDLING core stay silent. One thread removes mappings, one
// sleeps in nanosleep, and two more cores out of four are busy with nothing — that is,
// they stand in sti;hlt. It is exactly this arrangement that gives "did not acknowledge"
// on the stand (a task whose last syscall is 35 nanosleep and 202 futex).
#define _GNU_SOURCE
#include <sys/mman.h>
#include <pthread.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>
#define AREA (2u << 20)
static volatile int stop_now = 0;
static volatile unsigned long unmaps = 0;
static void *churn(void *a) {
    (void)a; unsigned long n = 0;
    while (!stop_now) {
        char *p = mmap(NULL, AREA, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) continue;
        for (unsigned o = 0; o < AREA; o += 4096) p[o] = 1;
        munmap(p, AREA); n++;
    }
    __sync_fetch_and_add(&unmaps, n);
    return NULL;
}
static void *sleeper(void *a) {
    (void)a;
    while (!stop_now) { struct timespec t = {0, 2000000}; nanosleep(&t, NULL); } // 2 ms
    return NULL;
}
int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    pthread_t c, s;
    pthread_create(&c, NULL, churn, NULL);
    pthread_create(&s, NULL, sleeper, NULL);
    struct timespec t = {5, 0}; nanosleep(&t, NULL);
    stop_now = 1;
    pthread_join(c, NULL); pthread_join(s, NULL);
    printf("idleshoot: the removals of mappings %lu\n", unmaps);
    printf("idleshoot: ALL OK\n");
    return 0;
}
