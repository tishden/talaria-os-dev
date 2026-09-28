// Minimal initramfs /init for the Linux-guest baseline:
// load every /NN_*.ko (numbered in dependency order, multi-pass),
// configure eth0 10.0.0.2/24, busy-poll UDP echo :7777.
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dirent.h>
#include <fcntl.h>
#include <net/if.h>
#include <net/route.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static int insmod(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        perror(path);
        return -1;
    }
    int rc = syscall(SYS_finit_module, fd, "", 0);
    close(fd);
    return rc;
}

static void insmod_all(void) {
    for (int pass = 0; pass < 3; pass++) {
        struct dirent **list;
        int n = scandir("/", &list, NULL, alphasort);
        for (int i = 0; i < n; i++) {
            const char *name = list[i]->d_name;
            size_t l = strlen(name);
            if (l > 3 && strcmp(name + l - 3, ".ko") == 0) {
                char p[512];
                snprintf(p, sizeof(p), "/%s", name);
                insmod(p); // EEXIST/missing-dep errors resolve across passes
            }
        }
    }
}

static void ifconfig(void) {
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    struct ifreq ifr = {0};
    strcpy(ifr.ifr_name, "eth0");

    struct sockaddr_in *sa = (struct sockaddr_in *)&ifr.ifr_addr;
    sa->sin_family = AF_INET;
    inet_pton(AF_INET, "10.0.0.2", &sa->sin_addr);
    if (ioctl(fd, SIOCSIFADDR, &ifr) != 0) perror("SIOCSIFADDR");

    inet_pton(AF_INET, "255.255.255.0", &sa->sin_addr);
    if (ioctl(fd, SIOCSIFNETMASK, &ifr) != 0) perror("SIOCSIFNETMASK");

    if (ioctl(fd, SIOCGIFFLAGS, &ifr) != 0) perror("SIOCGIFFLAGS");
    ifr.ifr_flags |= IFF_UP | IFF_RUNNING;
    if (ioctl(fd, SIOCSIFFLAGS, &ifr) != 0) perror("SIOCSIFFLAGS");
    close(fd);
}

int main(void) {
    mount("proc", "/proc", "proc", 0, NULL);
    insmod_all();
    // Give the NIC a moment to register.
    for (int i = 0; i < 50; i++) {
        int fd = socket(AF_INET, SOCK_DGRAM, 0);
        struct ifreq ifr = {0};
        strcpy(ifr.ifr_name, "eth0");
        int ok = ioctl(fd, SIOCGIFFLAGS, &ifr) == 0;
        close(fd);
        if (ok) break;
        usleep(100000);
    }
    ifconfig();
    printf("linux-guest: eth0 up, udp echo :7777\n");

    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    struct sockaddr_in addr = {0};
    addr.sin_family = AF_INET;
    addr.sin_port = htons(7777);
    addr.sin_addr.s_addr = INADDR_ANY;
    bind(fd, (struct sockaddr *)&addr, sizeof(addr));
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
