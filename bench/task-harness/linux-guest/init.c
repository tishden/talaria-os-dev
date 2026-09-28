// The Linux guest's /init for the challenge harness: the same stand as Talaria's (two VMs,
// the harness's ivshmem rings, tap+bridge), but with the "middle" running on the Linux kernel.
// Cmdline: ip=A.B.C.D  app=/sender|/receiver  args=<the rest of the line = argv[1..]>
// ivshmem: /sys/bus/pci/devices/*/{vendor,device}=1af4:1110 -> resource2; /tmp/shm/<name> -> symlinks,
// HARNESS_SHM_DIR=/tmp/shm (an override in shm_segment.h/attach.h), so the ring lives in the host's memory.
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dirent.h>
#include <fcntl.h>
#include <net/if.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
static int insmod(const char *path) { int fd = open(path, O_RDONLY); if (fd < 0) return -1; int rc = syscall(SYS_finit_module, fd, "", 0); close(fd); return rc; }
static void insmod_all(void) {
    for (int pass = 0; pass < 3; pass++) {
        struct dirent **list; int n = scandir("/", &list, NULL, alphasort);
        for (int i = 0; i < n; i++) { const char *nm = list[i]->d_name; size_t l = strlen(nm);
            if (l > 3 && strcmp(nm + l - 3, ".ko") == 0) { char p[512]; snprintf(p, sizeof p, "/%s", nm); insmod(p); } }
    }
}
static void ifconfig(const char *ip) {
    int fd = socket(AF_INET, SOCK_DGRAM, 0); struct ifreq ifr = {0}; strcpy(ifr.ifr_name, "eth0");
    struct sockaddr_in *sa = (struct sockaddr_in *)&ifr.ifr_addr; sa->sin_family = AF_INET; inet_pton(AF_INET, ip, &sa->sin_addr);
    if (ioctl(fd, SIOCSIFADDR, &ifr) != 0) perror("SIOCSIFADDR");
    inet_pton(AF_INET, "255.255.255.0", &sa->sin_addr); ioctl(fd, SIOCSIFNETMASK, &ifr);
    ioctl(fd, SIOCGIFFLAGS, &ifr); ifr.ifr_flags |= IFF_UP | IFF_RUNNING; if (ioctl(fd, SIOCSIFFLAGS, &ifr) != 0) perror("SIOCSIFFLAGS");
    close(fd);
}
static char *cmdline_val(char *cl, const char *key) { // key=value (value up to a space); for "args=" — to the end of the line
    char k[64]; snprintf(k, sizeof k, "%s=", key); char *p = strstr(cl, k); if (!p) return NULL; p += strlen(k);
    if (strcmp(key, "args") == 0) return p;
    char *e = strchr(p, ' '); if (e) *e = 0; return p;
}
static void find_ivshmem(char *out, size_t n) { // the resource2 path of device 1af4:1110
    out[0] = 0; DIR *d = opendir("/sys/bus/pci/devices"); if (!d) return; struct dirent *e;
    while ((e = readdir(d))) { if (e->d_name[0] == '.') continue; char p[512]; char v[16] = {0}, dv[16] = {0};
        snprintf(p, sizeof p, "/sys/bus/pci/devices/%s/vendor", e->d_name); FILE *f = fopen(p, "r"); if (f) { fgets(v, sizeof v, f); fclose(f); }
        snprintf(p, sizeof p, "/sys/bus/pci/devices/%s/device", e->d_name); f = fopen(p, "r"); if (f) { fgets(dv, sizeof dv, f); fclose(f); }
        if (strncmp(v, "0x1af4", 6) == 0 && strncmp(dv, "0x1110", 6) == 0) { snprintf(out, n, "/sys/bus/pci/devices/%s/resource2", e->d_name); break; } }
    closedir(d);
}
int main(void) {
    mount("proc", "/proc", "proc", 0, NULL); mkdir("/sys", 0755); mount("sysfs", "/sys", "sysfs", 0, NULL);
    mkdir("/tmp", 0777); mkdir("/tmp/shm", 0777); mkdir("/dev", 0755); mknod("/dev/null", S_IFCHR | 0666, makedev(1, 3));
    insmod_all();
    static char cl[4096]; FILE *f = fopen("/proc/cmdline", "r"); if (f) { fgets(cl, sizeof cl, f); fclose(f); } cl[strcspn(cl, "\n")] = 0;
    char *args = cmdline_val(cl, "args"); char *app = cmdline_val(cl, "app"); char *ip = cmdline_val(cl, "ip");
    for (int i = 0; i < 50; i++) { int fd = socket(AF_INET, SOCK_DGRAM, 0); struct ifreq ifr = {0}; strcpy(ifr.ifr_name, "eth0");
        int ok = ioctl(fd, SIOCGIFFLAGS, &ifr) == 0; close(fd); if (ok) break; usleep(100000); }
    ifconfig(ip ? ip : "10.0.0.2");
    char res[512]; find_ivshmem(res, sizeof res);
    if (res[0]) { symlink(res, "/tmp/shm/fanout_ring"); symlink(res, "/tmp/shm/fanout_rx"); setenv("HARNESS_SHM_DIR", "/tmp/shm", 1); }
    printf("linux-guest: eth0 %s up, ivshmem=%s, app=%s\n", ip ? ip : "?", res[0] ? res : "none", app ? app : "?"); fflush(stdout);
    if (!app) { for (;;) pause(); }
    char *argv[64]; int ac = 0; argv[ac++] = app;
    if (args) { char *s = args; while (*s && ac < 63) { while (*s == ' ') s++; if (!*s) break; argv[ac++] = s; while (*s && *s != ' ') s++; if (*s) *s++ = 0; } }
    argv[ac] = NULL;
    pid_t pid = fork();
    if (pid == 0) { execv(app, argv); perror("execv"); _exit(127); }
    int st = 0; waitpid(pid, &st, 0);
    printf("linux-guest: app exited status=%d\n", WEXITSTATUS(st)); fflush(stdout);
    for (;;) pause();
}
