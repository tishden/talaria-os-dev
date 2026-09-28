// /init of the Linux guest for actor-bench: the same QEMU/KVM and -smp 4 as Talaria,
// but the kernel is the host's vmlinuz. Cmdline: args=<actor-bench arguments to the end>.
// No network, no modules: the benchmark is pure compute (threads + futex). After it —
// poweroff, so that QEMU exits by itself.
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    mount("proc", "/proc", "proc", 0, NULL);
    static char cl[4096];
    FILE *f = fopen("/proc/cmdline", "r");
    if (f) { if (!fgets(cl, sizeof cl, f)) cl[0] = 0; fclose(f); }
    cl[strcspn(cl, "\n")] = 0;
    char *args = strstr(cl, "args=");
    args = args ? args + 5 : NULL; // to the end of the line
    char *argv[64]; int ac = 0;
    argv[ac++] = (char *)"/actor-bench";
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
    printf("linux-guest: starting actor-bench (%d arguments)\n", ac - 1); fflush(stdout);
    pid_t pid = fork();
    if (pid == 0) { execv(argv[0], argv); perror("execv"); _exit(127); }
    int st = 0; waitpid(pid, &st, 0);
    printf("linux-guest: bench exited status=%d\n", WEXITSTATUS(st)); fflush(stdout);
    sync();
    reboot(RB_POWER_OFF);
    for (;;) pause();
}
