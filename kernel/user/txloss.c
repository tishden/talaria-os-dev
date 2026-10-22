// txloss — what one lost segment costs this stack's TCP (docs/ARM64-PLAN.md §11 item 2a). The
// kernel boots with `talaria.txdrop=N` (every N-th transmitted frame is dropped on the floor);
// one connection to the harness's echo server (10.0.0.1:5555) carries 60 request/response rounds,
// and the time of every round is measured. A dropped request is recovered by the retransmission
// timer alone — a closed loop has no second segment in flight to earn a duplicate ACK — so the
// slow rounds ARE the stack's RTO floor: smoltcp's was the RFC's 1 s (the Talaria generator
// stalled exactly 1.000 s once per rung on a c7g.xlarge), now Linux's 200 ms. Verdict "(ok)":
// at least one round over 100 ms (a drop did happen) and none over 600 ms.
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

static long ms_since(const struct timespec *a) {
    struct timespec b; clock_gettime(CLOCK_MONOTONIC, &b);
    return (b.tv_sec - a->tv_sec) * 1000 + (b.tv_nsec - a->tv_nsec) / 1000000;
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    struct sockaddr_in a; memset(&a, 0, sizeof a);
    a.sin_family = AF_INET; a.sin_port = htons(5555); inet_pton(AF_INET, "10.0.0.1", &a.sin_addr);
    int s = -1;
    for (int attempt = 0; attempt < 5 && s < 0; attempt++) {     // the SYN itself may be the dropped frame
        s = socket(AF_INET, SOCK_STREAM, 0);
        if (connect(s, (struct sockaddr *)&a, sizeof a) < 0) { printf("txloss: connect errno=%d (attempt %d)\n", errno, attempt + 1); close(s); s = -1; }
    }
    if (s < 0) { printf("txloss: no connection (BAD)\n"); return 1; }
    int one = 1; setsockopt(s, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
    const int rounds = 60;
    long mx = 0, slow = 0, over = 0, bad = 0;
    char msg[32], out[64];
    for (int i = 0; i < rounds; i++) {
        int n = snprintf(msg, sizeof msg, "PING-%03d", i);
        struct timespec t0; clock_gettime(CLOCK_MONOTONIC, &t0);
        if (send(s, msg, n, 0) != n) { bad++; break; }
        int got = 0;
        while (got < n) {
            int r = recv(s, out + got, sizeof out - 1 - got, 0);
            if (r <= 0) { bad++; break; }
            got += r;
        }
        long ms = ms_since(&t0);
        if (got != n || memcmp(out, msg, n) != 0) bad++;
        if (ms > mx) mx = ms;
        if (ms > 100) { slow++; printf("txloss: round %d took %ld ms (a retransmission)\n", i, ms); }
        if (ms > 600) over++;
    }
    close(s);
    int ok = bad == 0 && slow >= 1 && over == 0;
    printf("txloss: %d rounds, max %ld ms, slow (>100 ms) %ld, over 600 ms %ld, bad %ld (%s)\n", rounds, mx, slow, over, bad, ok ? "ok" : "BAD");
    printf("txloss: %s\n", ok ? "ALL OK" : "FAILURES");
    return ok ? 0 : 1;
}
