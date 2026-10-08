// chldrace — PostgreSQL's postmaster shape (docs/ARM64-PLAN.md §11 item 0): a parent with a
// SIGCHLD handler sleeps in sigsuspend while a forked child runs alone on another core and
// exits; the core goes idle on the dead child's space, the parent is woken by SIGCHLD and may
// move onto that core. On a c6i.xlarge the postmaster then returned into rip 0/1. Each round
// checks the parent's own stack (a canary array) and that the handler ran; the verdict line
// is "chldrace: ALL OK <rounds>".
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile sig_atomic_t got;
static int use_susp;   // "susp": the parent sleeps in sigsuspend with SIGCHLD blocked outside it
static void on_chld(int s) { (void)s; got++; }

static int round_once(int r) {
    volatile unsigned long canary[64];
    for (int i = 0; i < 64; i++) canary[i] = 0x5a5a0000ul + (unsigned long)r * 64 + i;
    sigset_t block, old;
    sigemptyset(&block); sigaddset(&block, SIGCHLD);
    sigprocmask(SIG_BLOCK, &block, &old);
    int before = got;
    pid_t pid = fork();
    if (pid < 0) { printf("chldrace: fork failed at round %d\n", r); return 0; }
    if (pid == 0) {
        // the child: its own dispositions (as PostgreSQL's startup resets SIGCHLD), a short
        // spin so that it is alone on its core, then exit
        signal(SIGCHLD, (r & 1) ? SIG_IGN : SIG_DFL);
        volatile unsigned long x = 0;
        for (unsigned long i = 0; i < 20000ul + (unsigned long)(r % 7) * 5000ul; i++) x += i;
        _exit(0);
    }
    if (use_susp) {
        // the textbook shape: SIGCHLD blocked, sigsuspend with it admitted, until the handler ran
        while (got == before) sigsuspend(&old);
    }
    sigprocmask(SIG_SETMASK, &old, NULL);
    int st = 0;
    // (otherwise) sleep in waitpid; the SIGCHLD handler runs on the way out (SA_RESTART)
    if (waitpid(pid, &st, 0) != pid) { printf("chldrace: waitpid failed at round %d\n", r); return 0; }
    for (int i = 0; i < 64; i++)
        if (canary[i] != 0x5a5a0000ul + (unsigned long)r * 64 + i) {
            printf("chldrace: THE PARENT'S STACK changed at round %d word %d\n", r, i); return 0;
        }
    return 1;
}

int main(int argc, char **argv) {
    int rounds = argc > 1 ? atoi(argv[1]) : 2000;
    use_susp = argc > 2 && strcmp(argv[2], "susp") == 0;
    struct sigaction sa; memset(&sa, 0, sizeof sa);
    sa.sa_handler = on_chld; sa.sa_flags = SA_RESTART;
    sigaction(SIGCHLD, &sa, NULL);
    for (int r = 0; r < rounds; r++) {
        if (!round_once(r)) { printf("chldrace: FAILED\n"); return 1; }
        if (r % 500 == 499) printf("chldrace: %d rounds\n", r + 1);
    }
    printf("chldrace: ALL OK %d%s (the handler ran %d times)\n", rounds, use_susp ? " susp" : "", (int)got);
    return 0;
}
