// Closed-loop UDP ping: busy-poll send/recv, RTT percentiles.
// usage: udp_ping <ip> <port> <count> <warmup> <cpu> [payload=64]
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static long long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

static int cmp_ll(const void *a, const void *b) {
    long long x = *(const long long *)a, y = *(const long long *)b;
    return (x > y) - (x < y);
}

int main(int argc, char **argv) {
    if (argc < 6) {
        fprintf(stderr, "usage: %s ip port count warmup cpu [payload]\n", argv[0]);
        return 1;
    }
    const char *ip = argv[1];
    int port = atoi(argv[2]);
    long count = atol(argv[3]);
    long warmup = atol(argv[4]);
    int cpu = atoi(argv[5]);
    int payload = argc > 6 ? atoi(argv[6]) : 64;

    if (cpu >= 0) {
        cpu_set_t set;
        CPU_ZERO(&set);
        CPU_SET(cpu, &set);
        if (sched_setaffinity(0, sizeof(set), &set) != 0)
            perror("sched_setaffinity");
        struct sched_param sp = {.sched_priority = 50};
        sched_setscheduler(0, SCHED_FIFO, &sp); // best effort
    }

    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in dst = {0};
    dst.sin_family = AF_INET;
    dst.sin_port = htons(port);
    inet_pton(AF_INET, ip, &dst.sin_addr);
    if (connect(fd, (struct sockaddr *)&dst, sizeof(dst)) != 0) {
        perror("connect");
        return 1;
    }
    fcntl(fd, F_SETFL, O_NONBLOCK);

    char buf[2048];
    memset(buf, 0xA5, sizeof(buf));
    long long *rtt = calloc(count, sizeof(long long));
    long long deadline_ns = 200 * 1000000LL; // 200ms per-op timeout
    long lost = 0;

    for (long i = -warmup; i < count; i++) {
        *(long *)buf = i;
        long long t0 = now_ns();
        if (send(fd, buf, payload, 0) != payload) {
            perror("send");
            return 1;
        }
        long long t1;
        for (;;) {
            ssize_t r = recv(fd, buf, sizeof(buf), 0);
            t1 = now_ns();
            if (r > 0) break;
            if (t1 - t0 > deadline_ns) { t1 = -1; break; }
        }
        if (t1 < 0) {
            lost++;
            if (i >= 0) rtt[i] = -1;
            continue;
        }
        if (i >= 0) rtt[i] = t1 - t0;
    }

    // compact out losses
    long n = 0;
    for (long i = 0; i < count; i++)
        if (rtt[i] > 0) rtt[n++] = rtt[i];
    if (n == 0) {
        printf("all lost\n");
        return 1;
    }
    qsort(rtt, n, sizeof(long long), cmp_ll);
    long long sum = 0;
    for (long i = 0; i < n; i++) sum += rtt[i];
    #define PCT(p) rtt[(long)((p) * (n - 1))]
    printf("n=%ld lost=%ld min=%lld p50=%lld p90=%lld p99=%lld p999=%lld max=%lld mean=%lld (ns)\n",
           n, lost, rtt[0], PCT(0.50), PCT(0.90), PCT(0.99), PCT(0.999), rtt[n - 1], sum / n);
    return 0;
}
