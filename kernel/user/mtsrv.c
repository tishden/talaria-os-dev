// mtsrv — a server with a THREAD PER CONNECTION (the MariaDB model) rather than with one epoll.
//
// Why a separate workload. The rule "one pass of the stack per machine is enough"
// (`talaria.netsolo=1`) parks every waiter except the polling one, and a delivery wakes
// them. In a single-threaded server the waiter is THE ONLY one and it is the polling one
// too — such a server does not touch this branch at all, which is why neither epsrv nor
// srv shows the defect. It is shown by exactly this model: several threads, each blocked
// in `read` on ITS OWN socket, and the requests go over persistent connections one after
// another.
//
// What is printed. Every thread measures the interval "the answer has been sent -> the
// next request has been read", that is, the turnaround the waking of the waiter enters.
// If a parked one is woken by a delivery, that is tens to hundreds of microseconds; if it
// is woken by the backstop of the parking (5 ms), the median sits down on half of the
// backstop, and that is visible as a number rather than as a guess.
#include <sys/socket.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <pthread.h>

#define MAXS 4096

static long long now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}

static int cmp_ll(const void *a, const void *b) {
    long long x = *(const long long *)a, y = *(const long long *)b;
    return (x > y) - (x < y);
}

static pthread_mutex_t PRT = PTHREAD_MUTEX_INITIALIZER;
static int CONNS = 0;
// HOW MANY CONNECTIONS WE WAIT FOR BEFORE EXITING. The exit is needed not by the server
// but by the gate: the kernel's counters (`stat:`) are printed at the workload's finish,
// and without them "the median is large" and "the median was made by the parking's backstop" are indistinguishable.
#define WANT_CONNS 4
// The answer is a valid HTTP/1.1 with keep-alive: the load is driven by THE SAME
// instrument as the whole set (`bench/suite/tlgen --proto http`) rather than by a
// home-made client. A saturating generator is mandatory here: the defect lives at the
// CEILING, where the waiter really does have time to fall asleep between the requests.
static const char RESP[] =
    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n"
    "Connection: keep-alive\r\nContent-Length: 3\r\n\r\nOK\n";
#define RESPLEN (sizeof(RESP) - 1)

static void *serve(void *arg) {
    int c = (int)(long)arg;
    int one = 1;
    setsockopt(c, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
    // THE SOCKET IS NON-BLOCKING — as in MariaDB. This is not a detail of style: a
    // blocking `read` declares the thread the owner by itself ("it is I who will wait"),
    // and on it the defect of `poll` is not visible at all. On a non-blocking one only
    // `poll` can wait, and the owner stayed THE ACCEPTING thread.
    fcntl(c, F_SETFL, fcntl(c, F_GETFL, 0) | O_NONBLOCK);
    long long *s = malloc(MAXS * sizeof(long long));
    int n = 0;
    if (!s) { close(c); return 0; }
    char buf[512];
    long long t_sent = 0;
    struct pollfd pf = { c, POLLIN, 0 };
    for (;;) {
        // WE WAIT THROUGH `poll` RATHER THAN SIMPLY READ. That is what MariaDB's vio does,
        // and it is on this path that the kernel did NOT declare the thread the socket's
        // owner (`epoll_wait` did, `poll` did not): the delivery rang the accepting
        // thread, while the sleeping one waited for the backstop of 5 ms.
        if (poll(&pf, 1, 5000) <= 0) break;
        int r = read(c, buf, sizeof(buf));
        if (r <= 0) break;
        if (t_sent && n < MAXS) s[n++] = now_us() - t_sent;   // the turnaround "the answer -> the next request"
        write(c, RESP, RESPLEN);
        t_sent = now_us();
    }
    close(c);
    if (n > 0) {
        qsort(s, n, sizeof(s[0]), cmp_ll);
        pthread_mutex_lock(&PRT);
        printf("mtsrv: conn n=%d p50=%lld p99=%lld max=%lld us\n",
               n, s[n / 2], s[(n * 99) / 100], s[n - 1]);
        fflush(stdout);
        CONNS++;
        int done = CONNS;
        pthread_mutex_unlock(&PRT);
        if (done >= WANT_CONNS) { printf("mtsrv: DONE (%d connections)\n", done); fflush(stdout); _exit(0); }
    }
    return 0;
}

int main(void) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in a = {0};
    a.sin_family = AF_INET; a.sin_port = htons(80); a.sin_addr.s_addr = 0;
    bind(s, (struct sockaddr *)&a, sizeof(a));
    listen(s, 16);
    printf("mtsrv: a thread per connection, the port 80\n");
    fflush(stdout);
    for (;;) {
        int c = accept(s, 0, 0);
        if (c < 0) continue;
        pthread_t th;
        if (pthread_create(&th, 0, serve, (void *)(long)c) != 0) { printf("mtsrv: pthread_create REFUSED\n"); fflush(stdout); close(c); continue; }
        pthread_detach(th);
    }
}
