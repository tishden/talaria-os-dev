// The Linux guest's /init for futexlat: run THE SAME binary that is embedded in the
// Talaria kernel and power the machine off. Without this wrapper futexlat would be init
// itself, and its exit would be a panic "attempted to kill init": QEMU would have to be
// killed by timeout, and every run would take the whole timeout.
#include <stdio.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    mount("proc", "/proc", "proc", 0, NULL);
    pid_t pid = fork();
    if (pid == 0) { execl("/futexlat", "futexlat", (char *)0); perror("execv"); _exit(127); }
    int st = 0; waitpid(pid, &st, 0);
    printf("linux-guest: futexlat exited status=%d\n", WEXITSTATUS(st)); fflush(stdout);
    sync();
    reboot(RB_POWER_OFF);
    for (;;) pause();
}
