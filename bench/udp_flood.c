// udp_flood — a background UDP stream to a given address (a load test as a
// diagnostic). The point of the measurement: if udp_ping's p50 DROPS sharply under
// background traffic, the device was holding completions on its own timer and the
// stream "pushes" it along; if it does not change, the delay is in the path itself,
// and it must be looked for elsewhere than in moderation. This halves the hypotheses cheaper than any other probe.
//
//   udp_flood <ip> <port> <seconds> [packets/s, 0 = unlimited] [bytes]
//
// Prints how many packets actually left — without that "there was background load" is unprovable.
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <time.h>
#include <arpa/inet.h>
#include <sys/socket.h>

static long long now_ns(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

int main(int argc, char **argv)
{
    if (argc < 4) {
        fprintf(stderr, "usage: %s ip port seconds [pps, 0=max] [payload]\n", argv[0]);
        return 2;
    }
    const char *ip = argv[1];
    int port = atoi(argv[2]);
    double seconds = atof(argv[3]);
    long pps = argc > 4 ? atol(argv[4]) : 0;
    int payload = argc > 5 ? atoi(argv[5]) : 64;
    if (payload < 1) payload = 1;
    if (payload > 1400) payload = 1400;

    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) { perror("socket"); return 1; }
    struct sockaddr_in sa;
    memset(&sa, 0, sizeof(sa));
    sa.sin_family = AF_INET;
    sa.sin_port = htons(port);
    if (inet_pton(AF_INET, ip, &sa.sin_addr) != 1) { fprintf(stderr, "bad address %s\n", ip); return 2; }
    if (connect(fd, (struct sockaddr *)&sa, sizeof(sa)) < 0) { perror("connect"); return 1; }

    char *buf = calloc(1, payload);
    long long t0 = now_ns(), deadline = t0 + (long long)(seconds * 1e9);
    // The interval between packets at the given pps; with pps=0 send without pauses.
    long long step = pps > 0 ? 1000000000LL / pps : 0;
    long long sent = 0, dropped = 0, next = t0;
    while (now_ns() < deadline) {
        if (step) {
            next += step;
            long long d = next - now_ns();
            if (d > 0) {
                struct timespec ts = { d / 1000000000LL, d % 1000000000LL };
                nanosleep(&ts, NULL);
            }
        }
        if (send(fd, buf, payload, MSG_DONTWAIT) < 0) dropped++;
        else sent++;
    }
    long long el = now_ns() - t0;
    printf("udp_flood: sent %lld packets of %d B in %.2f s -> %lld packets/s (dropped locally %lld)\n",
           sent, payload, el / 1e9, el ? sent * 1000000000LL / el : 0, dropped);
    return 0;
}
