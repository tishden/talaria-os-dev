// Busy-poll UDP echo server (Linux baseline for talaria).
// usage: udp_echo <port> <cpu>
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s port cpu\n", argv[0]);
        return 1;
    }
    int port = atoi(argv[1]);
    int cpu = atoi(argv[2]);

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
    struct sockaddr_in addr = {0};
    addr.sin_family = AF_INET;
    addr.sin_port = htons(port);
    addr.sin_addr.s_addr = INADDR_ANY;
    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        perror("bind");
        return 1;
    }
    fcntl(fd, F_SETFL, O_NONBLOCK);

    char buf[2048];
    struct sockaddr_in src;
    socklen_t sl;
    for (;;) {
        sl = sizeof(src);
        ssize_t r = recvfrom(fd, buf, sizeof(buf), 0, (struct sockaddr *)&src, &sl);
        if (r > 0)
            sendto(fd, buf, r, 0, (struct sockaddr *)&src, sl);
    }
}
