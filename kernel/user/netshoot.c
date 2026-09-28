// A THREAD WAITING FOR THE NETWORK MUST ACKNOWLEDGE A TLB FLUSH TO ITS NEIGHBOURS.
//
// The path of waiting for the network goes with the interrupts disabled, so the call of
// the TLB-flush vector does not reach the waiting one: the initiator of the `munmap`
// waits for the acknowledgement for 10 ms and GIVES UP, leaving a stale TLB entry at the
// neighbour — that is, the kernel goes on walking over a frame that the allocator has
// already given away. In the stand's console this is `shootdown: core N did not
// acknowledge within 10 ms (the last syscall 281/202)`: 957 times over a run of mongodb and not once for mysql, postgresql and redis.
//
// Here is the same arrangement in its pure form: one thread stands in accept() (nobody
// connects), three neighbours drive mmap/touch/munmap. On a kernel without the fix the
// lines "did not acknowledge" appear in batches; with the fix there is not a single one,
// and that is what the gate `netshoot` in test.sh checks.
#define _GNU_SOURCE
#include <sys/socket.h>
#include <sys/mman.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <time.h>
#include <poll.h>

#define CHURN_THREADS 3
#define AREA (2u << 20)          /* 2 MB — 512 pages to be removed */

static volatile int stop_now = 0;
static volatile unsigned long unmaps = 0;

static void *churn(void *arg) {
    (void)arg;
    unsigned long n = 0;
    while (!stop_now) {
        char *p = mmap(NULL, AREA, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { struct timespec t = {0, 1000000}; nanosleep(&t, NULL); continue; }
        for (unsigned o = 0; o < AREA; o += 4096) p[o] = 1;   /* the mappings are REALLY brought up */
        munmap(p, AREA);                                      /* -> a TLB flush to the neighbours */
        n++;
    }
    __sync_fetch_and_add(&unmaps, n);
    return NULL;
}

/* After six seconds stop the neighbours: by that moment the main thread has already
 * stood its time waiting for the network. */
static void *stopper(void *arg) {
    (void)arg;
    struct timespec t = {4, 0};
    nanosleep(&t, NULL);
    stop_now = 1;
    return NULL;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int port = 7411;
    int s = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in a; memset(&a, 0, sizeof(a));
    a.sin_family = AF_INET; a.sin_port = htons((unsigned short)port); a.sin_addr.s_addr = 0;
    if (bind(s, (struct sockaddr *)&a, sizeof(a)) < 0) { printf("netshoot: bind FAILED\n"); return 1; }
    if (listen(s, 8) < 0) { printf("netshoot: listen FAILED\n"); return 1; }

    pthread_t th[CHURN_THREADS], sp;
    for (int i = 0; i < CHURN_THREADS; i++) pthread_create(&th[i], NULL, churn, NULL);
    pthread_create(&sp, NULL, stopper, NULL);

    /* THE MAIN THREAD STANDS HERE — waiting for a network that will not come.
     * `poll` with a ceiling rather than `accept` without one: the gate must FINISH by
     * itself, while their path of waiting is common (`netsys::wait_frame_w`) — and it is
     * exactly that which is checked. */
    struct pollfd p = { .fd = s, .events = POLLIN };
    int pr = poll(&p, 1, 4000);

    stop_now = 1;
    for (int i = 0; i < CHURN_THREADS; i++) pthread_join(th[i], NULL);
    pthread_join(sp, NULL);
    close(s);
    printf("netshoot: the removals of mappings %lu, the wait for the network returned %d\n", unmaps, pr);
    printf("netshoot: %s\n", (unmaps > 100 && pr == 0) ? "ALL OK" : "FAILED");
    return 0;
}
