// Closed-loop TCP ping: busy-poll send/recv over a persistent TCP connection,
// RTT percentiles per request. The peer is epsrv (an epoll TCP server with a
// fixed reply to every request, request boundary \r\n\r\n) or any server that
// answers each request with a constant number of bytes: the reply length is taken
// from the first exchange (warmup >= 1 is mandatory) and then expected in full.
// usage: tcp_ping <ip> <port> <count> <warmup> <cpu> [payload=64]
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
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
    if (payload < 8) payload = 8;
    if (warmup < 1) warmup = 1; // the first exchange calibrates the reply length

    if (cpu >= 0) {
        cpu_set_t set;
        CPU_ZERO(&set);
        CPU_SET(cpu, &set);
        if (sched_setaffinity(0, sizeof(set), &set) != 0)
            perror("sched_setaffinity");
        struct sched_param sp = {.sched_priority = 50};
        sched_setscheduler(0, SCHED_FIFO, &sp); // best effort
    }

    int fd = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
    struct sockaddr_in dst = {0};
    dst.sin_family = AF_INET;
    dst.sin_port = htons(port);
    inet_pton(AF_INET, ip, &dst.sin_addr);
    if (connect(fd, (struct sockaddr *)&dst, sizeof(dst)) != 0) {
        perror("connect");
        return 1;
    }
    fcntl(fd, F_SETFL, O_NONBLOCK);

    char req[2048], buf[4096];
    memset(req, 0xA5, sizeof(req));
    memcpy(req + payload - 4, "\r\n\r\n", 4); // request boundary for epsrv
    long long *rtt = calloc(count, sizeof(long long));
    long long deadline_ns = 500 * 1000000LL; // 500ms per-op
    long lost = 0;
    int resp_len = 0; // 0 = take it from the first exchange (a quiet pause = end of the reply)

    for (long i = -warmup; i < count; i++) {
        *(long *)req = i;
        long long t0 = now_ns();
        int off = 0;
        while (off < payload) {
            int w = send(fd, req + off, payload - off, 0);
            if (w > 0) off += w;
            else if (errno != EAGAIN && errno != EWOULDBLOCK) { perror("send"); return 1; }
        }
        long long t1 = -1, tlast = 0;
        int got = 0;
        for (;;) {
            ssize_t r = recv(fd, buf, sizeof(buf), 0);
            long long now = now_ns();
            if (r > 0) { got += r; tlast = now; t1 = now; }
            else if (r == 0) { fprintf(stderr, "peer closed\n"); return 1; }
            if (resp_len > 0) {
                if (got >= resp_len) break;
            } else if (got > 0 && now - tlast > 2000000) {
                break; // calibration: 2 ms of silence after the first reply = the end
            }
            if (now - t0 > deadline_ns) { t1 = -1; break; }
        }
        if (resp_len == 0 && got > 0) resp_len = got;
        if (t1 < 0) {
            lost++;
            if (i >= 0) rtt[i] = -1;
            continue;
        }
        if (i >= 0) rtt[i] = t1 - t0;
    }

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
    printf("n=%ld lost=%ld resp=%dB min=%lld p50=%lld p90=%lld p99=%lld p999=%lld max=%lld mean=%lld (ns)\n",
           n, lost, resp_len, rtt[0], PCT(0.50), PCT(0.90), PCT(0.99), PCT(0.999), rtt[n - 1], sum / n);
    return 0;
}
