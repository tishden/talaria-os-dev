// udp_owd — one-way delays, following the method of bench/ena-latency §2.
// Needs an echo that STAMPS the first 8 bytes of the reply with its own clock
// (Talaria: talaria.kecho=<port> talaria.kts=1; both sides' clocks are disciplined by
// AWS Time Sync/kvmclock, so comparing CLOCK_REALTIME is meaningful).
//   T1 — the request leaves (our clock), T2 — the echo's stamp (its clock), T3 — arrival (ours)
//   A->B = T2-T1, B->A = T3-T2, sanity: (A->B)+(B->A) == RTT
// usage: udp_owd <ip> <port> <count> <warmup> <cpu> [payload=64]
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static long long now_ns(void) {
    struct timespec ts; clock_gettime(CLOCK_REALTIME, &ts);
    return ts.tv_sec * 1000000000LL + ts.tv_nsec;
}
static int cmp(const void *a, const void *b) {
    long long x = *(const long long *)a, y = *(const long long *)b;
    return (x > y) - (x < y);
}
int main(int argc, char **argv) {
    if (argc < 6) { fprintf(stderr, "usage: %s ip port count warmup cpu [payload]\n", argv[0]); return 1; }
    const char *ip = argv[1]; int port = atoi(argv[2]);
    int count = atoi(argv[3]), warm = atoi(argv[4]), cpu = atoi(argv[5]);
    int plen = argc > 6 ? atoi(argv[6]) : 64;
    if (plen < 8) plen = 8;
    if (cpu >= 0) { cpu_set_t s; CPU_ZERO(&s); CPU_SET(cpu, &s); sched_setaffinity(0, sizeof s, &s); }
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in a = {0}; a.sin_family = AF_INET; a.sin_port = htons(port);
    inet_pton(AF_INET, ip, &a.sin_addr);
    struct timeval tv = {1, 0}; setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    char *buf = malloc(plen);
    long long *ab = malloc(sizeof(long long) * count), *ba = malloc(sizeof(long long) * count);
    long long *rtt = malloc(sizeof(long long) * count);
    int n = 0, lost = 0;
    for (int i = 0; i < warm + count; i++) {
        memset(buf, 0x5a, plen);
        long long t1 = now_ns();
        if (sendto(fd, buf, plen, 0, (struct sockaddr *)&a, sizeof a) < 0) { lost++; continue; }
        int r = recv(fd, buf, plen, 0);
        long long t3 = now_ns();
        if (r < 8) { lost++; continue; }
        if (i < warm) continue;
        long long t2; memcpy(&t2, buf, 8);
        ab[n] = t2 - t1; ba[n] = t3 - t2; rtt[n] = t3 - t1; n++;
    }
    if (!n) { printf("udp_owd: no replies (lost=%d)\n", lost); return 1; }
    qsort(ab, n, sizeof *ab, cmp); qsort(ba, n, sizeof *ba, cmp); qsort(rtt, n, sizeof *rtt, cmp);
    /* The sides' clocks are offset (with AWS Time Sync that is milliseconds): the classic
       NTP correction — offset = (A->B - B->A)/2, subtracted from one side and
       added to the other; the sum (and RTT) does not change. */
    long long off = (ab[n/2] - ba[n/2]) / 2;
    printf("n=%d lost=%d  raw: A->B p50=%lld B->A p50=%lld (clock offset %lld ns)\n",
           n, lost, ab[n/2], ba[n/2], off);
    printf("one-way (corrected): A->B p50=%lld p99=%lld  B->A p50=%lld p99=%lld  RTT p50=%lld p99=%lld (ns)\n",
           ab[n/2] - off, ab[n*99/100] - off, ba[n/2] + off, ba[n*99/100] + off, rtt[n/2], rtt[n*99/100]);
    printf("sanity: (A->B)+(B->A)=%lld vs RTT p50=%lld\n", ab[n/2] + ba[n/2], rtt[n/2]);
    return 0;
}
