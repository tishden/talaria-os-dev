// The regression of Talaria's outgoing network: DNS (getaddrinfo through musl's resolver -> UDP :53),
// a blocking connect, a non-blocking connect + poll + SO_ERROR, send/recv, the wall clock.
#include <sys/socket.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
#include <netdb.h>
#include <unistd.h>
#include <string.h>
#include <stdio.h>
#include <fcntl.h>
#include <poll.h>
#include <errno.h>
#include <time.h>
#include <stdlib.h>

static int echo_once(struct sockaddr_in *a, const char *msg, char *out, int outl, int nonblock) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    if (nonblock) {
        fcntl(s, F_SETFL, fcntl(s, F_GETFL, 0) | O_NONBLOCK);
        int r = connect(s, (struct sockaddr*)a, sizeof(*a));
        if (r < 0 && errno != EINPROGRESS) { printf("connect: nb connect errno=%d\n", errno); return -1; }
        struct pollfd p = { .fd = s, .events = POLLOUT };
        r = poll(&p, 1, 5000);
        int err = 0; socklen_t el = sizeof(err);
        getsockopt(s, SOL_SOCKET, SO_ERROR, &err, &el);
        if (r <= 0 || err) { printf("connect: nb poll r=%d err=%d\n", r, err); return -1; }
        fcntl(s, F_SETFL, fcntl(s, F_GETFL, 0) & ~O_NONBLOCK);
    } else if (connect(s, (struct sockaddr*)a, sizeof(*a)) < 0) { printf("connect: errno=%d\n", errno); return -1; }
    int one = 1; setsockopt(s, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
    send(s, msg, strlen(msg), 0);
    int n = recv(s, out, outl - 1, 0);
    if (n > 0) out[n] = 0; else out[0] = 0;
    close(s);
    return n;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    struct timespec ts; clock_gettime(CLOCK_REALTIME, &ts);
    struct tm tm; gmtime_r(&ts.tv_sec, &tm);
    printf("wall: %04d-%02d-%02dT%02d:%02d:%02dZ (%s)\n", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday,
           tm.tm_hour, tm.tm_min, tm.tm_sec, tm.tm_year + 1900 >= 2025 ? "ok" : "BAD");
    // DNS: the name talaria.test -> an A record (a mini-DNS on the harness's side)
    struct addrinfo hints = { .ai_family = AF_INET, .ai_socktype = SOCK_STREAM }, *res = NULL;
    struct timespec d0, d1; clock_gettime(CLOCK_MONOTONIC, &d0);
    int r = getaddrinfo("talaria.test", "5555", &hints, &res);
    clock_gettime(CLOCK_MONOTONIC, &d1);
    printf("dns-took: %ld ms\n", (d1.tv_sec - d0.tv_sec) * 1000 + (d1.tv_nsec - d0.tv_nsec) / 1000000);
    char ip[32] = "-";
    struct sockaddr_in a; memset(&a, 0, sizeof(a));
    if (r == 0 && res) {
        a = *(struct sockaddr_in*)res->ai_addr;
        inet_ntop(AF_INET, &a.sin_addr, ip, sizeof(ip));
        printf("dns: talaria.test -> %s\n", ip);
        freeaddrinfo(res);
    } else {
        printf("dns: FAIL r=%d (%s)\n", r, gai_strerror(r));
        a.sin_family = AF_INET; a.sin_port = htons(5555); inet_pton(AF_INET, "10.0.0.1", &a.sin_addr);
    }
    char out[64];
    int n1 = echo_once(&a, "PING-B", out, sizeof(out), 0);
    printf("echo-blocking: %d %s\n", n1, out);
    int n2 = echo_once(&a, "PING-N", out, sizeof(out), 1);
    printf("echo-nonblock: %d %s\n", n2, out);
    // a closed port: connect must return ECONNREFUSED
    struct sockaddr_in b = a; b.sin_port = htons(5556);
    int s = socket(AF_INET, SOCK_STREAM, 0);
    int rr = connect(s, (struct sockaddr*)&b, sizeof(b));
    printf("refused: r=%d errno=%d (%s)\n", rr, errno, (rr < 0 && errno == ECONNREFUSED) ? "ok" : "unexpected");
    close(s);
    // A REFUSAL SEEN BY A NON-BLOCKING SOCKET. A deferred error must reach all
    // three of its consumers in ONE AND THE SAME form: SO_ERROR as a POSITIVE
    // errno (that is how libc puts it into errno), read and send as a NEGATIVE
    // one (that is how the syscalls return it). While the sign was stored the
    // other way round, SO_ERROR gave out -111 and musl printed "No error
    // information" instead of "Connection refused" (on the stand mongod's
    // refusal looked like that: the cause of the refusal had been wiped by the
    // instrument), and read returned +111 — "111 bytes have been read" from a socket that was refused a connection.
    {
        int ns = socket(AF_INET, SOCK_STREAM, 0);
        fcntl(ns, F_SETFL, fcntl(ns, F_GETFL, 0) | O_NONBLOCK);
        int r0 = connect(ns, (struct sockaddr*)&b, sizeof(b));
        int e0 = errno;
        struct pollfd np = { .fd = ns, .events = POLLOUT };
        int pr = poll(&np, 1, 5000);
        int soerr = -1; socklen_t sl = sizeof(soerr);
        getsockopt(ns, SOL_SOCKET, SO_ERROR, &soerr, &sl);
        close(ns);
        // THE SECOND socket — for read: SO_ERROR TAKES the error away
        // (read-and-clear), and both cannot be asked about the same error on one socket.
        int ms2 = socket(AF_INET, SOCK_STREAM, 0);
        fcntl(ms2, F_SETFL, fcntl(ms2, F_GETFL, 0) | O_NONBLOCK);
        connect(ms2, (struct sockaddr*)&b, sizeof(b));
        struct pollfd np2 = { .fd = ms2, .events = POLLOUT };
        poll(&np2, 1, 5000);
        char rb[8]; errno = 0;
        int rn = (int)recv(ms2, rb, sizeof(rb), 0);
        int re = errno;
        printf("refused-nb: connect=%d/%d poll=%d SO_ERROR=%d recv=%d/%d (%s)\n",
               r0, e0, pr, soerr, rn, re,
               (r0 < 0 && e0 == EINPROGRESS && pr > 0 && soerr == ECONNREFUSED
                && rn < 0 && re == ECONNREFUSED) ? "ok" : "BAD");
        close(ms2);
    }
    // close() with a DEAD peer: the host gives out "GO" and hides the address 10.0.0.9,
    // so our FIN stays unanswered. Earlier close() itself waited for the untangling for
    // up to 200 ms, spinning under the stack's lock of the whole machine (on the stand —
    // a debt of the load generator of exactly 200 ms for every timeout); now it returns
    // at once, while the slot waits for a FIN/RST in a pass of the stack. The criterion:
    // close < 20 ms and THE NEXT connection (the echo on 10.0.0.1) answers right after it.
    {
        struct sockaddr_in c; memset(&c, 0, sizeof(c));
        c.sin_family = AF_INET; c.sin_port = htons(5557); inet_pton(AF_INET, "10.0.0.9", &c.sin_addr);
        int cs = socket(AF_INET, SOCK_STREAM, 0);
        if (connect(cs, (struct sockaddr*)&c, sizeof(c)) == 0) {
            char go[8]; int ng = recv(cs, go, sizeof(go), 0);
            struct timespec w = { 0, 100000000 }; nanosleep(&w, NULL); // the host has time to hide the address
            struct timespec c0, c1, c2; clock_gettime(CLOCK_MONOTONIC, &c0);
            close(cs);
            clock_gettime(CLOCK_MONOTONIC, &c1);
            int n3 = echo_once(&a, "PING-A", out, sizeof(out), 0);
            clock_gettime(CLOCK_MONOTONIC, &c2);
            long cms = (c1.tv_sec - c0.tv_sec) * 1000 + (c1.tv_nsec - c0.tv_nsec) / 1000000;
            long ems = (c2.tv_sec - c1.tv_sec) * 1000 + (c2.tv_nsec - c1.tv_nsec) / 1000000;
            printf("close-dead: go=%d close %ld ms, the echo after it %d in %ld ms (%s)\n", ng, cms, n3, ems,
                   (ng == 2 && cms < 20 && n3 == 6 && ems < 50) ? "ok" : "BAD");
        } else printf("close-dead: connect errno=%d (BAD)\n", errno);
    }
    // close() with UNSENT data to a live but slow peer (10.0.0.1:5558: a tiny receive
    // window, it reads after 0.6 s — the Internet with an RTT of 0.25 s and a slow start
    // looks the same). The first version of the orphan (§26.5) kept the deadline of
    // 200 ms BY THE CLOCK and sent an RST by it — on EC2 the generator's report was lost
    // in the middle of a read. The deadline must be counted from the last progress. How
    // much arrived is reported by the peer itself over a second connection
    // (10.0.0.1:5559) — when it has read it all. We wait for its answer with a BLOCKING
    // recv rather than with a sleep: a pass of the stack is done by whoever waits for the
    // network (on EC2 the generator stands in accept() after close()), a sleeping thread
    // does not drive the stack — and the orphan does not progress with it.
    {
        struct sockaddr_in c = a; c.sin_port = htons(5558);
        int cs = socket(AF_INET, SOCK_STREAM, 0);
        static char big[48 * 1024];
        memset(big, 'x', sizeof(big));
        long sent = -1;
        if (connect(cs, (struct sockaddr*)&c, sizeof(c)) == 0) {
            sent = 0;
            while (sent < (long)sizeof(big)) {
                long w = send(cs, big + sent, sizeof(big) - sent, 0);
                if (w <= 0) break;
                sent += w;
            }
            close(cs);
        }
        struct sockaddr_in q = a; q.sin_port = htons(5559);
        int qs = socket(AF_INET, SOCK_STREAM, 0);
        long got = -1;
        if (connect(qs, (struct sockaddr*)&q, sizeof(q)) == 0) {
            char buf[32]; int n = recv(qs, buf, sizeof(buf) - 1, 0);
            if (n > 0) { buf[n] = 0; got = atol(buf); }
        }
        close(qs);
        printf("close-slow: sent %ld, arrived %ld (%s)\n", sent, got,
               (sent == (long)sizeof(big) && got == sent) ? "ok" : "BAD");
    }
    // a sleep of 20 ms (nanosleep) — we measure it
    struct timespec t0, t1; clock_gettime(CLOCK_MONOTONIC, &t0);
    struct timespec d = { 0, 20000000 }; nanosleep(&d, NULL);
    clock_gettime(CLOCK_MONOTONIC, &t1);
    long ms = (t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_nsec - t0.tv_nsec) / 1000000;
    printf("sleep: %ld ms (%s)\n", ms, (ms >= 19 && ms < 200) ? "ok" : "BAD");
    printf("connect-test: %s\n", (n1 > 0 && n2 > 0 && strcmp(ip, "-") != 0) ? "ALL OK" : "FAILED");
    return 0;
}
