/* init for the Linux guest: the same VM, the same ClickHouse binary, the same query set.
 *
 * Why. Talaria is measured IN A VM, while Linux used to be measured ON HARDWARE — and
 * that is no detail: a TLB miss under a hypervisor drags a walk of the nested tables
 * behind it, and our own `mbench` shows 84 ns against 67 for it. A system can only be
 * compared with a system under equal conditions, so Linux goes into the same
 * QEMU/KVM with the same number of cores and the same memory.
 *
 * The root is an initramfs holding exactly the tree mkdisk.sh puts into the
 * Talaria image. The arguments come from /proc/cmdline after `args=`.
 *
 * THE DATABASE FILES ARE ON A REAL DISK, not in tmpfs. `/var/lib/clickhouse` used
 * to be a tmpfs, and that quietly broke exactly the four test cases MergeTree was
 * added to the suite for: Talaria wrote the table to a virtual disk with real
 * device barriers, and Linux into RAM, where fsync does nothing. The comparison
 * became "disk against memory" instead of "system against system", in both
 * directions at once: our DDL looked like a failure (94 ms vs 1.5) and our
 * insert like a bigger win than it was.
 * Now both sides use virtio-blk with ext4 (the virtio_blk/ext4 modules go into the
 * initramfs, see run-linux-vm.sh).
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <unistd.h>
#include <dirent.h>
#include <fcntl.h>
#include <sys/mount.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/reboot.h>

/* Loading modules: run-linux-vm.sh puts them into the root, their names numbered in
 * dependency order. Several passes, so as not to parse the graph here. */
static void insmod_all(void) {
    for (int pass = 0; pass < 3; pass++) {
        struct dirent **list;
        int n = scandir("/", &list, NULL, alphasort);
        if (n < 0) return;
        for (int i = 0; i < n; i++) {
            const char *name = list[i]->d_name;
            size_t l = strlen(name);
            if (l > 3 && strcmp(name + l - 3, ".ko") == 0) {
                char p[512];
                snprintf(p, sizeof p, "/%s", name);
                int fd = open(p, O_RDONLY);
                if (fd >= 0) { syscall(SYS_finit_module, fd, "", 0); close(fd); }
            }
            free(list[i]);
        }
        free(list);
    }
}

int main(void) {
    mount("proc", "/proc", "proc", 0, NULL);
    mount("devtmpfs", "/dev", "devtmpfs", 0, NULL); // ClickHouse wants /dev/urandom
    mount("sysfs", "/sys", "sysfs", 0, NULL);
    mount("tmpfs", "/tmp", "tmpfs", 0, "size=1024m");
    insmod_all();
    if (mount("/dev/vda", "/var/lib/clickhouse", "ext4", 0, NULL) != 0) {
        perror("[guest] mount /dev/vda -> /var/lib/clickhouse");
        fprintf(stderr, "[guest] WARNING: the database files go to tmpfs — "
                        "the MergeTree comparison will be UNFAIR (memory against disk)\n");
        mount("tmpfs", "/var/lib/clickhouse", "tmpfs", 0, "size=1024m");
    } else {
        printf("[guest] /var/lib/clickhouse: ext4 on /dev/vda (a real disk, as on Talaria)\n");
    }

    static char cl[4096];
    FILE *f = fopen("/proc/cmdline", "r");
    if (f) { if (!fgets(cl, sizeof cl, f)) cl[0] = 0; fclose(f); }
    cl[strcspn(cl, "\n")] = 0;
    char *args = strstr(cl, "args=");
    args = args ? args + 5 : NULL;

    char *argv[64]; int ac = 0;
    argv[ac++] = (char *)"clickhouse";
    if (args) {
        char *s = args;
        while (*s && ac < 63) {
            while (*s == ' ') s++;
            if (!*s) break;
            argv[ac++] = s;
            while (*s && *s != ' ') s++;
            if (*s) *s++ = 0;
        }
    }
    argv[ac] = NULL;
    char *envp[] = { (char *)"HOME=/", (char *)"PATH=/", (char *)"TZ=UTC", NULL };

    pid_t p = fork();
    if (p == 0) { execve("/clickhouse", argv, envp); _exit(127); }
    int st = 0; waitpid(p, &st, 0);
    printf("[guest] clickhouse exited, code %d\n", WEXITSTATUS(st));
    fflush(stdout);
    sync();
    reboot(RB_POWER_OFF);
    for (;;) pause();
}
