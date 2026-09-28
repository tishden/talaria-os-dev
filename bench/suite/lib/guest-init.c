/* The init for the Linux guest of the bench/suite set.
 *
 * One program for all workloads: what to bring up is described by the /spec file inside
 * the initramfs rather than hard-coded. That way the Linux side of the comparison is built from the
 * same trees as the Talaria images and differs only in packaging.
 *
 * The /spec format is lines of key=value; the order matters only for arg/env:
 *   mount=<device>:<mount point>:<fs>  mount it (may appear several times)
 *   ip=A.B.C.D/P                      the eth0 address
 *   gw=A.B.C.D                        the default gateway
 *   tune=lowlat                       low-latency settings (see tune_lowlat)
 *   exec=/path                        what to run
 *   arg=...                           the next argument (the first is usually argv[0])
 *   env=K=V                           an environment variable
 *   chown=<path>:<uid>:<gid>          change the owner recursively (before the start)
 *   uid=N / gid=N                     as whom to run the workload
 *   halt=1                            power the machine off when the workload exits
 *
 * About uid. PostgreSQL refuses to work as root ("must be started under an
 * unprivileged user ID"), while the initramfs is built by an ordinary user, so
 * init fixes the tree's owner itself: it is root and does so before starting the workload.
 * Talaria has no such check, but the workload does not change with the uid — what matters
 * is that both sides run THE SAME binary with the same arguments.
 *
 * The workload is started as a child so that its exit code is visible in the console;
 * init stays pid 1 and reaps orphans.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dirent.h>
#include <fcntl.h>
#include <net/if.h>
#include <net/route.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <signal.h>
#include <grp.h>
#include <unistd.h>
#include <errno.h>
#include <linux/perf_event.h>
#include <netinet/in.h>
#include <poll.h>

static void insmod_all(void) {
    for (int pass = 0; pass < 3; pass++) {
        struct dirent **list;
        int n = scandir("/", &list, NULL, alphasort);
        if (n < 0) return;
        for (int i = 0; i < n; i++) {
            const char *nm = list[i]->d_name;
            size_t l = strlen(nm);
            if (l > 3 && !strcmp(nm + l - 3, ".ko")) {
                char p[512];
                snprintf(p, sizeof p, "/%s", nm);
                int fd = open(p, O_RDONLY);
                if (fd >= 0) { syscall(SYS_finit_module, fd, "", 0); close(fd); }
            }
            free(list[i]);
        }
        free(list);
    }
}

/* A recursive change of owner: the tree in the initramfs belongs to whoever
 * built it, while the workload needs its own uid. */
static void chown_rec(const char *path, int uid, int gid) {
    if (chown(path, (uid_t)uid, (gid_t)gid) != 0) return;
    DIR *d = opendir(path);
    if (!d) return;
    struct dirent *e;
    while ((e = readdir(d))) {
        if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
        char sub[1024];
        snprintf(sub, sizeof sub, "%s/%s", path, e->d_name);
        struct stat st;
        if (lstat(sub, &st) == 0 && S_ISDIR(st.st_mode)) chown_rec(sub, uid, gid);
        else lchown(sub, (uid_t)uid, (gid_t)gid);
    }
    closedir(d);
}

static void wr(const char *path, const char *val) {
    int fd = open(path, O_WRONLY);
    if (fd < 0) return;
    if (write(fd, val, strlen(val)) < 0) { /* not every knob exists in every kernel */ }
    close(fd);
}

/* The guest's low-latency settings. All of this is the same as what people do
 * by hand on a production host; the list is deliberately short, because every
 * line must be explainable in the report. */
static void tune_lowlat(void) {
    wr("/proc/sys/kernel/sched_rt_runtime_us", "-1");        /* do not throttle RT threads */
    wr("/proc/sys/net/ipv4/tcp_low_latency", "1");           /* absent on newer kernels — silently skipped */
    wr("/proc/sys/net/core/busy_poll", "50");
    wr("/proc/sys/net/core/busy_read", "50");
    wr("/proc/sys/net/ipv4/tcp_fastopen", "3");
    wr("/proc/sys/vm/swappiness", "0");
    wr("/proc/sys/kernel/numa_balancing", "0");
    wr("/sys/kernel/mm/transparent_hugepage/enabled", "madvise");
    wr("/proc/sys/net/core/somaxconn", "4096");
    wr("/proc/sys/net/ipv4/tcp_syncookies", "0");
}

static int ifup(const char *dev, const char *cidr, const char *gw) {
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    if (s < 0) return -1;
    struct ifreq ifr;
    memset(&ifr, 0, sizeof ifr);
    snprintf(ifr.ifr_name, IFNAMSIZ, "%s", dev);

    char buf[64]; snprintf(buf, sizeof buf, "%s", cidr);
    char *slash = strchr(buf, '/');
    int plen = slash ? atoi(slash + 1) : 24;
    if (slash) *slash = 0;

    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    inet_pton(AF_INET, buf, &a.sin_addr);
    memcpy(&ifr.ifr_addr, &a, sizeof a);
    if (ioctl(s, SIOCSIFADDR, &ifr) < 0) { close(s); return -1; }

    a.sin_addr.s_addr = plen >= 32 ? 0xFFFFFFFFu : htonl(~((1u << (32 - plen)) - 1));
    memcpy(&ifr.ifr_netmask, &a, sizeof a);
    ioctl(s, SIOCSIFNETMASK, &ifr);

    ifr.ifr_flags = IFF_UP | IFF_RUNNING;
    ioctl(s, SIOCSIFFLAGS, &ifr);

    /* loopback is needed too: the server may talk to itself */
    struct ifreq lo;
    memset(&lo, 0, sizeof lo);
    snprintf(lo.ifr_name, IFNAMSIZ, "lo");
    lo.ifr_flags = IFF_UP | IFF_RUNNING;
    ioctl(s, SIOCSIFFLAGS, &lo);

    if (gw && *gw) {
        struct rtentry rt;
        memset(&rt, 0, sizeof rt);
        struct sockaddr_in *d = (struct sockaddr_in *)&rt.rt_dst;
        struct sockaddr_in *g = (struct sockaddr_in *)&rt.rt_gateway;
        struct sockaddr_in *m = (struct sockaddr_in *)&rt.rt_genmask;
        d->sin_family = g->sin_family = m->sin_family = AF_INET;
        inet_pton(AF_INET, gw, &g->sin_addr);
        rt.rt_flags = RTF_UP | RTF_GATEWAY;
        rt.rt_dev = (char *)dev;
        ioctl(s, SIOCADDRT, &rt);
    }
    close(s);
    return 0;
}

/* ─────────────────────────── workload counters ───────────────────────────
 * init is the workload's parent, so it is init that takes its counters: it knows the pid,
 * can open perf events on the task and hand a snapshot outside over TCP.
 * No separate binary in the initramfs is needed for that, and, most importantly, the snapshot is taken
 * FROM OUTSIDE the workload and changes nothing in it.
 *
 * The STATD_PORT port: the string `snap` -> a text snapshot, `quit` -> close.
 */
#define STATD_PORT 9999

static const struct { const char *name; unsigned type; unsigned long long cfg; } PEVENTS[] = {
    { "cycles",          PERF_TYPE_HARDWARE, PERF_COUNT_HW_CPU_CYCLES },
    { "instructions",    PERF_TYPE_HARDWARE, PERF_COUNT_HW_INSTRUCTIONS },
    { "cache_misses",    PERF_TYPE_HARDWARE, PERF_COUNT_HW_CACHE_MISSES },
    { "branch_misses",   PERF_TYPE_HARDWARE, PERF_COUNT_HW_BRANCH_MISSES },
    { "context_switches",PERF_TYPE_SOFTWARE, PERF_COUNT_SW_CONTEXT_SWITCHES },
    { "cpu_migrations",  PERF_TYPE_SOFTWARE, PERF_COUNT_SW_CPU_MIGRATIONS },
    { "page_faults",     PERF_TYPE_SOFTWARE, PERF_COUNT_SW_PAGE_FAULTS },
};
#define NPEV ((int)(sizeof PEVENTS / sizeof PEVENTS[0]))
static int pev_fd[NPEV];

static void pev_open(pid_t pid) {
    for (int i = 0; i < NPEV; i++) {
        struct perf_event_attr a;
        memset(&a, 0, sizeof a);
        a.size = sizeof a;
        a.type = PEVENTS[i].type;
        a.config = PEVENTS[i].cfg;
        a.disabled = 0;
        a.inherit = 1;                 /* the workload's threads are counted too */
        a.exclude_kernel = 0;
        a.exclude_hv = 1;
        pev_fd[i] = (int)syscall(__NR_perf_event_open, &a, pid, -1, -1, 0);
    }
}

static void cat_to(FILE *out, const char *path, const char *label) {
    FILE *f = fopen(path, "r");
    if (!f) return;
    char b[512];
    while (fgets(b, sizeof b, f)) fprintf(out, "%s %s", label, b);
    fclose(f);
}

static void snapshot(FILE *out, pid_t pid) {
    fprintf(out, "# a counter snapshot of the Linux guest\n");
    for (int i = 0; i < NPEV; i++) {
        unsigned long long v = 0;
        if (pev_fd[i] >= 0 && read(pev_fd[i], &v, sizeof v) == sizeof v)
            fprintf(out, "perf %s %llu\n", PEVENTS[i].name, v);
        else
            fprintf(out, "perf %s -\n", PEVENTS[i].name);
    }
    char p[128];
    snprintf(p, sizeof p, "/proc/%d/status", (int)pid);  cat_to(out, p, "status");
    snprintf(p, sizeof p, "/proc/%d/stat", (int)pid);    cat_to(out, p, "stat");
    snprintf(p, sizeof p, "/proc/%d/io", (int)pid);      cat_to(out, p, "io");
    cat_to(out, "/proc/stat", "sys");
    cat_to(out, "/proc/interrupts", "irq");
    cat_to(out, "/proc/net/snmp", "snmp");
    cat_to(out, "/proc/net/netstat", "netstat");
    cat_to(out, "/proc/loadavg", "load");
    fprintf(out, "# end\n");
}

static void statd(pid_t pid) {
    int s = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(s, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET; a.sin_port = htons(STATD_PORT); a.sin_addr.s_addr = 0;
    if (bind(s, (struct sockaddr *)&a, sizeof a) < 0 || listen(s, 4) < 0) { close(s); return; }
    for (;;) {
        int c = accept(s, NULL, NULL);
        if (c < 0) { if (errno == EINTR) continue; break; }
        FILE *f = fdopen(c, "w");
        if (f) { snapshot(f, pid); fflush(f); fclose(f); } else close(c);
    }
    close(s);
}

int main(void) {
    mkdir("/proc", 0755); mkdir("/sys", 0755); mkdir("/dev", 0755); mkdir("/tmp", 01777);
    mount("proc", "/proc", "proc", 0, NULL);
    mount("sysfs", "/sys", "sysfs", 0, NULL);
    mount("devtmpfs", "/dev", "devtmpfs", 0, NULL);
    mount("tmpfs", "/tmp", "tmpfs", 0, "size=512m");
    mkdir("/dev/shm", 01777);
    mount("tmpfs", "/dev/shm", "tmpfs", 0, "size=512m");
    insmod_all();

    char *argv[64]; int ac = 0;
    char *envp[32]; int ec = 0;
    char exec_path[256] = "";
    int halt = 0;
    int run_uid = -1, run_gid = -1;

    FILE *f = fopen("/spec", "r");
    if (!f) { fprintf(stderr, "[guest] no /spec\n"); reboot(RB_POWER_OFF); }
    char line[1024];
    while (fgets(line, sizeof line, f)) {
        line[strcspn(line, "\n")] = 0;
        if (!line[0] || line[0] == '#') continue;
        char *v = strchr(line, '=');
        if (!v) continue;
        *v++ = 0;
        if (!strcmp(line, "mount")) {
            char *dev = v, *pt = strchr(v, ':');
            if (!pt) continue;
            *pt++ = 0;
            char *fs = strchr(pt, ':');
            if (fs) *fs++ = 0; else fs = (char *)"ext4";
            mkdir(pt, 0755);
            if (mount(dev, pt, fs, 0, NULL) != 0)
                fprintf(stderr, "[guest] mount %s -> %s (%s) FAILED\n", dev, pt, fs);
            else
                printf("[guest] %s -> %s (%s)\n", dev, pt, fs);
        } else if (!strcmp(line, "ip")) {
            static char ipbuf[64]; snprintf(ipbuf, sizeof ipbuf, "%s", v);
            setenv("GUEST_IP", ipbuf, 1);
        } else if (!strcmp(line, "gw")) {
            setenv("GUEST_GW", v, 1);
        } else if (!strcmp(line, "tune")) {
            if (!strcmp(v, "lowlat")) tune_lowlat();
        } else if (!strcmp(line, "exec")) {
            snprintf(exec_path, sizeof exec_path, "%s", v);
        } else if (!strcmp(line, "arg")) {
            if (ac < 63) argv[ac++] = strdup(v);
        } else if (!strcmp(line, "env")) {
            if (ec < 31) envp[ec++] = strdup(v);
        } else if (!strcmp(line, "uid")) {
            run_uid = atoi(v);
        } else if (!strcmp(line, "gid")) {
            run_gid = atoi(v);
        } else if (!strcmp(line, "chown")) {
            char *pt = v, *u = strchr(v, ':');
            if (!u) continue;
            *u++ = 0;
            char *g = strchr(u, ':');
            if (g) *g++ = 0;
            char cmd[512];
            snprintf(cmd, sizeof cmd, "%s", pt);
            chown_rec(cmd, atoi(u), g ? atoi(g) : atoi(u));
        } else if (!strcmp(line, "halt")) {
            halt = atoi(v);
        }
    }
    fclose(f);
    argv[ac] = NULL; envp[ec] = NULL;

    const char *ip = getenv("GUEST_IP");
    if (ip && ifup("eth0", ip, getenv("GUEST_GW")) == 0)
        printf("[guest] eth0 %s gw %s\n", ip, getenv("GUEST_GW") ? getenv("GUEST_GW") : "-");
    else if (ip)
        fprintf(stderr, "[guest] eth0 DID NOT COME UP (%s)\n", ip);

    if (!exec_path[0]) { fprintf(stderr, "[guest] no exec= in /spec\n"); reboot(RB_POWER_OFF); }
    printf("[guest] starting %s\n", exec_path);
    fflush(stdout);

    pid_t p = fork();
    if (p == 0) {
        if (run_gid >= 0) {
            if (setgroups(0, NULL) != 0) perror("[guest] setgroups");
            if (setgid((gid_t)run_gid) != 0) perror("[guest] setgid");
        }
        if (run_uid >= 0 && setuid((uid_t)run_uid) != 0) perror("[guest] setuid");
        /* Diagnostics for exactly the case that did happen: execve refused,
         * and without the euid and the file's permissions the reason cannot be told from "no such binary". */
        /* If the workload does not start, the reason must be named at once: permissions on
         * THE PATH ITSELF, not "no such binary". That is how the cause above was found — the guest's
         * root arrived with mode 700 from mktemp, and the dropped uid could not
         * get through "/" at all. */
        struct stat es;
        if (stat(exec_path, &es) != 0) {
            struct stat rs;
            fprintf(stderr, "[guest] stat %s: %s (I am %u:%u", exec_path, strerror(errno),
                    geteuid(), getegid());
            if (stat("/", &rs) == 0)
                fprintf(stderr, ", root %o owner %u:%u", (unsigned)(rs.st_mode & 07777),
                        rs.st_uid, rs.st_gid);
            fprintf(stderr, ")\n");
        }
        execve(exec_path, argv, envp); perror("[guest] execve"); _exit(127);
    }
    pev_open(p);
    pid_t sd = fork();
    if (sd == 0) { statd(p); _exit(0); }
    int st = 0;
    while (waitpid(p, &st, 0) < 0 && errno == EINTR) { }
    printf("[guest] %s exited, code %d\n", exec_path, WEXITSTATUS(st));
    kill(sd, SIGTERM);
    fflush(stdout);
    sync();
    if (halt) reboot(RB_POWER_OFF);
    for (;;) pause();
}
