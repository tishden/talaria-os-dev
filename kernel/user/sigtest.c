/* sigtest.c — the regression of Talaria's process model (talaria.test=sigtest, from an ext2 disk as a process):
   the fd tables/dup2, pipe (EOF, nonblock, poll/select), the cwd and relative paths on ext2,
   readdir/unlink/rmdir, the signals (a handler, a mask, kill/SIGCHLD, the default SIGTERM),
   fork: MAP_SHARED is shared, MAP_PRIVATE is COW, SysV shm, the wait4 statuses. musl static. */
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <signal.h>
#include <poll.h>
#include <dirent.h>
#include <sys/stat.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <sys/select.h>
#include <sys/shm.h>
#include <sys/sysinfo.h>
#include <sys/eventfd.h>
#include <sys/timerfd.h>
#include <sys/epoll.h>
#include <sys/signalfd.h>
#include <pthread.h>
#include <time.h>

static int fails;
#define CHECK(c, name) do { if (c) printf("sigtest: ok %s\n", name); else { printf("sigtest: FAIL %s (errno=%d)\n", name, errno); fails++; } } while (0)
static volatile sig_atomic_t got_usr1, got_chld;
static void h_usr1(int s) { (void)s; got_usr1 = 1; }
static void h_chld(int s) { (void)s; got_chld = 1; }

static void h_term(int s) { (void)s; write(1, "sleeper: got SIGTERM, exiting cleanly\n", 39); _exit(0); }
static volatile sig_atomic_t waiter_got; static int waiter_masked = -1;
static void *sigwaiter(void *a) { (void)a; sigset_t cur; pthread_sigmask(SIG_BLOCK, NULL, &cur); waiter_masked = sigismember(&cur, SIGUSR2); /* has the mask been inherited */
  sigset_t w; sigemptyset(&w); sigaddset(&w, SIGUSR2); int sig = 0; sigwait(&w, &sig); waiter_got = sig; return 0; }

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  if (argc > 1 && !strcmp(argv[1], "sleep")) { /* the mode for talaria.stop: we wait for a SIGTERM and exit cleanly */
    struct sigaction st; memset(&st, 0, sizeof st); st.sa_handler = h_term; sigaction(SIGTERM, &st, 0);
    printf("sleeper: pid=%d waiting for SIGTERM\n", getpid()); for (;;) pause(); }
  if (argc > 1 && !strcmp(argv[1], "ignore")) { /* the mode for the gate of the power button: the SIGTERM is ignored, the kernel powers the machine off by the deadline */
    signal(SIGTERM, SIG_IGN); printf("sleeper: pid=%d ignoring SIGTERM\n", getpid()); for (;;) pause(); }
  char b[16], rb[16], cwd[128]; int status = 0;
  /* 1. pipe */
  int p[2]; CHECK(pipe(p) == 0, "pipe");
  CHECK(write(p[1], "abc", 3) == 3 && read(p[0], b, 8) == 3 && !memcmp(b, "abc", 3), "pipe write/read");
  struct pollfd pf = { p[0], POLLIN, 0 };
  CHECK(poll(&pf, 1, 0) == 0, "poll empty pipe -> 0");
  write(p[1], "x", 1); CHECK(poll(&pf, 1, 0) == 1 && (pf.revents & POLLIN), "poll pipe readable");
  read(p[0], b, 8);
  fcntl(p[0], F_SETFL, O_NONBLOCK); CHECK(read(p[0], b, 8) == -1 && errno == EAGAIN, "pipe nonblock EAGAIN");
  /* 2. files + dup2 */
  mkdir("/t", 0755);
  int fd = open("/t/dup.txt", O_CREAT | O_RDWR | O_TRUNC, 0644); CHECK(fd >= 0, "open O_CREAT");
  CHECK(dup2(fd, 50) == 50, "dup2 -> 50"); CHECK(write(50, "hello", 5) == 5, "write via dup");
  close(fd); CHECK(pread(50, rb, 16, 0) == 5 && !memcmp(rb, "hello", 5), "pread via dup after close orig");
  close(50);
  /* 3. cwd / relative paths */
  CHECK(getcwd(cwd, sizeof cwd) && !strcmp(cwd, "/"), "getcwd /");
  CHECK(chdir("/t") == 0 && getcwd(cwd, sizeof cwd) && !strcmp(cwd, "/t"), "chdir /t");
  struct stat st; CHECK(stat("dup.txt", &st) == 0 && st.st_size == 5, "stat relative");
  CHECK(mkdir("sub2", 0755) == 0 && chdir("sub2") == 0 && getcwd(cwd, sizeof cwd) && !strcmp(cwd, "/t/sub2"), "mkdir+chdir relative");
  CHECK(chdir("..") == 0 && getcwd(cwd, sizeof cwd) && !strcmp(cwd, "/t"), "chdir ..");
  CHECK(rmdir("sub2") == 0, "rmdir");
  DIR *d = opendir("/t"); int n = 0; struct dirent *de;
  while (d && (de = readdir(d))) { if (!strcmp(de->d_name, "dup.txt")) n++; }
  if (d) closedir(d);
  CHECK(n == 1, "readdir finds dup.txt");
  CHECK(unlink("dup.txt") == 0 && stat("dup.txt", &st) != 0, "unlink");
  chdir("/");
  /* 4. signals */
  struct sigaction sa; memset(&sa, 0, sizeof sa); sa.sa_handler = h_usr1; sigemptyset(&sa.sa_mask); sa.sa_flags = SA_RESTART; sigaction(SIGUSR1, &sa, 0);
  sa.sa_handler = h_chld; sigaction(SIGCHLD, &sa, 0);
  kill(getpid(), SIGUSR1); CHECK(got_usr1 == 1, "kill self SIGUSR1 -> handler");
  got_usr1 = 0; sigset_t ss; sigemptyset(&ss); sigaddset(&ss, SIGUSR1);
  sigprocmask(SIG_BLOCK, &ss, 0); kill(getpid(), SIGUSR1); CHECK(got_usr1 == 0, "blocked signal stays pending");
  sigprocmask(SIG_UNBLOCK, &ss, 0); CHECK(got_usr1 == 1, "unblock -> delivered");
  /* 5. shared / private / sysv */
  int *shared = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0); *shared = 1;
  int *priv = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0); *priv = 1;
  /* a MAP_SHARED|MAP_FIXED: shared after a fork too (it used to take the private branch and turn COW) */
  int *fx = mmap((void *)0x6f0000000000ul, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
  if (fx != MAP_FAILED) *fx = 1;
  printf("sigtest: MAP_SHARED at %p, MAP_SHARED|MAP_FIXED at %p\n", (void *)shared, (void *)fx);
  /* a private MAP_FIXED below 4 GiB: refused while the kernel's identity is there, granted under
     talaria.dmap=strict where the process space has none (RAM in the direct map, devices in their
     window) — the gate reads which; either way it must stay private after the fork */
  int *lo = mmap((void *)0xa0000000ul, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
  if (lo != MAP_FAILED) { lo[0] = 1; lo[1024] = 2; }
  printf("sigtest: a low MAP_FIXED at 0xa0000000: %s\n", lo == MAP_FAILED ? "refused" : (lo == (int *)0xa0000000ul ? "granted" : "MOVED"));
  int shmid = shmget(IPC_PRIVATE, 4096, IPC_CREAT | 0600);
  int *sv = shmid >= 0 ? (int *)shmat(shmid, 0, 0) : (int *)-1; if (sv != (int *)-1) *sv = 1;
  CHECK(shmid >= 0 && sv != (int *)-1, "shmget/shmat");
  /* a POSIX shm object: shm_open + ftruncate + mmap(MAP_SHARED, fd) */
  int sfd = shm_open("/sigtest.shm", O_CREAT | O_RDWR, 0600);
  int *ps = MAP_FAILED;
  if (sfd >= 0 && ftruncate(sfd, 4096) == 0) ps = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, sfd, 0);
  if (ps != MAP_FAILED) *ps = 1;
  printf("sigtest: SysV shm at %p, POSIX shm at %p\n", (void *)sv, (void *)ps);
  /* 6. fork */
  int p2[2]; pipe(p2); got_chld = 0; got_usr1 = 0;
  pid_t me = getpid();
  pid_t pid = fork();
  if (pid == 0) {
    *shared = 42; *priv = 42; if (lo != MAP_FAILED) lo[0] = 46; if (sv != (int *)-1) *sv = 43; if (fx != MAP_FAILED) *fx = 44; if (ps != MAP_FAILED) *ps = 45;
    printf("sigtest: child pid=%d ppid=%d (parent %d) %s\n", getpid(), getppid(), me, getppid() == me ? "ok" : "MISMATCH");
    write(p2[1], "ready", 5);
    while (!got_usr1) pause();
    close(p2[1]);
    _exit(7);
  }
  CHECK(pid > 0 && pid != me, "fork");
  CHECK(read(p2[0], rb, 5) == 5, "child wrote to pipe");
  CHECK(*shared == 42, "MAP_SHARED visible to parent");
  CHECK(fx != MAP_FAILED && *fx == 44, "MAP_SHARED|MAP_FIXED visible to parent");
  CHECK(ps != MAP_FAILED && *ps == 45, "POSIX shm visible to parent");
  CHECK(*priv == 1, "MAP_PRIVATE isolated (COW)");
  CHECK(lo == MAP_FAILED || (lo == (int *)0xa0000000ul && lo[0] == 1 && lo[1024] == 2), "a low MAP_FIXED isolated (COW)");
  CHECK(lo == MAP_FAILED || munmap(lo, 8192) == 0, "a low MAP_FIXED unmapped");
  CHECK(sv == (int *)-1 || *sv == 43, "sysv shm visible to parent");
  CHECK(kill(pid, SIGUSR1) == 0, "kill child SIGUSR1");
  pid_t w = waitpid(pid, &status, 0);
  CHECK(w == pid && WIFEXITED(status) && WEXITSTATUS(status) == 7, "waitpid status 7");
  CHECK(got_chld == 1, "SIGCHLD delivered to parent");
  close(p2[1]); CHECK(read(p2[0], rb, 8) == 0, "pipe EOF after all writers closed");
  /* 7. select timeout */
  fd_set rs; FD_ZERO(&rs); FD_SET(p[0], &rs); struct timeval tv = { 0, 10000 };
  CHECK(select(p[0] + 1, &rs, 0, 0, &tv) == 0, "select timeout");
  /* 8. default action */
  pid = fork(); if (pid == 0) { while (1) pause(); }
  kill(pid, SIGTERM); w = waitpid(pid, &status, 0);
  CHECK(w == pid && WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM, "SIGTERM default kills child");
  /* 9. fsync + the file grows through O_APPEND */
  unlink("/t/app.log"); fd = open("/t/app.log", O_CREAT | O_WRONLY | O_APPEND, 0644); write(fd, "l1\n", 3); write(fd, "l2\n", 3); fsync(fd); close(fd);
  CHECK(stat("/t/app.log", &st) == 0 && st.st_size == 6, "O_APPEND + fsync");
  /* 10. the processes' memory is freed at exit: 40 children of 1 MB each, the free RAM before/after */
  { struct sysinfo si0, si1; sysinfo(&si0);
    for (int k = 0; k < 40; k++) { pid = fork(); if (pid == 0) { char *m = malloc(1 << 20); memset(m, k, 1 << 20); _exit(m[100] == k ? 0 : 1); } waitpid(pid, &status, 0); }
    usleep(200000); sysinfo(&si1);
    long delta = (long)si0.freeram - (long)si1.freeram;
    printf("sigtest: free RAM before=%lu after=%lu delta=%ld KB\n", si0.freeram >> 10, si1.freeram >> 10, delta >> 10);
    CHECK(delta < (4L << 20), "process memory freed at exit (40 forks x 1MB, leak < 4MB)"); }
  /* 11. a signal to the process is taken by the sigwait thread (the mask has been inherited at the clone), as in mongod */
  { sigset_t bs; sigemptyset(&bs); sigaddset(&bs, SIGUSR2); pthread_sigmask(SIG_BLOCK, &bs, 0);
    pthread_t th; pthread_create(&th, 0, sigwaiter, 0); usleep(50000);
    CHECK(waiter_masked == 1, "thread inherits creator sigmask");
    kill(getpid(), SIGUSR2); pthread_join(th, 0);
    CHECK(waiter_got == SIGUSR2, "process-directed signal consumed by sigwait thread"); }
  /* 12. signalfd */
  { sigset_t ss1; sigemptyset(&ss1); sigaddset(&ss1, SIGUSR1); sigprocmask(SIG_BLOCK, &ss1, 0);
    int sfd = signalfd(-1, &ss1, SFD_NONBLOCK); struct signalfd_siginfo si; 
    struct pollfd spf = { sfd, POLLIN, 0 }; CHECK(sfd >= 0 && poll(&spf, 1, 0) == 0, "signalfd created, not readable");
    kill(getpid(), SIGUSR1); CHECK(poll(&spf, 1, 0) == 1 && read(sfd, &si, sizeof si) == sizeof si && si.ssi_signo == SIGUSR1, "signalfd readable after kill, ssi_signo");
    close(sfd); sigprocmask(SIG_UNBLOCK, &ss1, 0); }
  /* 13. eventfd / timerfd */
  { int efd = eventfd(0, EFD_NONBLOCK); uint64_t v = 0; struct pollfd epf = { efd, POLLIN, 0 };
    CHECK(efd >= 0 && poll(&epf, 1, 0) == 0 && read(efd, &v, 8) == -1 && errno == EAGAIN, "eventfd empty: EAGAIN, not readable");
    v = 3; write(efd, &v, 8); v = 2; write(efd, &v, 8); v = 0;
    CHECK(poll(&epf, 1, 0) == 1 && read(efd, &v, 8) == 8 && v == 5, "eventfd counter 3+2=5 readable"); close(efd);
    int tfd = timerfd_create(CLOCK_MONOTONIC, 0); struct itimerspec its = {{0,0},{0,50000000}}; timerfd_settime(tfd, 0, &its, 0);
    struct timespec t0, t1; clock_gettime(CLOCK_MONOTONIC, &t0); uint64_t exp = 0; int r = read(tfd, &exp, 8); clock_gettime(CLOCK_MONOTONIC, &t1);
    long ms = (t1.tv_sec - t0.tv_sec) * 1000 + (t1.tv_nsec - t0.tv_nsec) / 1000000;
    CHECK(r == 8 && exp == 1 && ms >= 45 && ms < 1000, "timerfd 50ms fires once (blocking read)"); close(tfd);
  /* EPOLLET on an eventfd: the EDGE is given out once until there is a new write (asio does not read its own interrupter,
     and by the level it spun epoll_wait into a storm). By the level it is given out every time. */
    int ep = epoll_create1(0); int e2 = eventfd(0, EFD_NONBLOCK); struct epoll_event ee = { EPOLLIN | EPOLLET, { .u64 = 7 } }, out[4];
    CHECK(ep >= 0 && epoll_ctl(ep, EPOLL_CTL_ADD, e2, &ee) == 0 && epoll_wait(ep, out, 4, 0) == 0, "eventfd ET: nothing before write");
    v = 1; write(e2, &v, 8);
    CHECK(epoll_wait(ep, out, 4, 0) == 1 && out[0].data.u64 == 7 && epoll_wait(ep, out, 4, 0) == 0, "eventfd ET: edge reported ONCE, not re-reported unread");
    write(e2, &v, 8);
    CHECK(epoll_wait(ep, out, 4, 0) == 1 && epoll_wait(ep, out, 4, 0) == 0, "eventfd ET: new write = new edge");
    ee.events = EPOLLIN; epoll_ctl(ep, EPOLL_CTL_MOD, e2, &ee);
    CHECK(epoll_wait(ep, out, 4, 0) == 1 && epoll_wait(ep, out, 4, 0) == 1, "eventfd LT: reported every time while unread");
    read(e2, &v, 8); CHECK(v == 2 && epoll_wait(ep, out, 4, 0) == 0, "eventfd LT: drained -> quiet");
    int t2 = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK); ee.events = EPOLLIN | EPOLLET; ee.data.u64 = 9; epoll_ctl(ep, EPOLL_CTL_ADD, t2, &ee);
    struct itimerspec its2 = {{0,0},{0,20000000}}; timerfd_settime(t2, 0, &its2, 0); usleep(40000);
    CHECK(epoll_wait(ep, out, 4, 0) == 1 && out[0].data.u64 == 9 && epoll_wait(ep, out, 4, 0) == 0, "timerfd ET: expiry reported once");
    timerfd_settime(t2, 0, &its2, 0); usleep(40000);
    CHECK(epoll_wait(ep, out, 4, 0) == 1 && read(t2, &exp, 8) == 8 && epoll_wait(ep, out, 4, 0) == 0, "timerfd ET: re-armed = new edge, read -> quiet");
    close(t2); close(e2); close(ep); }
  /* 14. the procfs/sysfs minimum */
  { char pb[512]; int pfd = open("/proc/meminfo", O_RDONLY); int n = pfd >= 0 ? read(pfd, pb, sizeof pb - 1) : -1; if (n > 0) pb[n] = 0; close(pfd);
    CHECK(n > 0 && strstr(pb, "MemTotal:"), "/proc/meminfo readable");
    pfd = open("/proc/self/status", O_RDONLY); n = pfd >= 0 ? read(pfd, pb, sizeof pb - 1) : -1; if (n > 0) pb[n] = 0; close(pfd);
    CHECK(n > 0 && strstr(pb, "Pid:\t"), "/proc/self/status readable");
    CHECK(stat("/sys/kernel/mm/transparent_hugepage/enabled", &st) == 0 && stat("/sys/block", &st) == 0 && S_ISDIR(st.st_mode), "sysfs thp + /sys/block dir"); }
  /* 15. mprotect RO -> fork -> RW: the isolation (COW for RO pages too) */
  { char *pg = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0); pg[0] = 1; mprotect(pg, 4096, PROT_READ);
    int p3[2]; pipe(p3); pid = fork();
    if (pid == 0) { mprotect(pg, 4096, PROT_READ | PROT_WRITE); pg[0] = 42; char c = pg[0]; write(p3[1], &c, 1); _exit(0); }
    char cc = 0; read(p3[0], &cc, 1); mprotect(pg, 4096, PROT_READ | PROT_WRITE); pg[0] = 7; waitpid(pid, &status, 0);
    CHECK(cc == 42 && pg[0] == 7, "mprotect(RO) page stays private across fork (child 42 / parent 7)"); }
  /* 16. openat(dirfd)/fstatat relative to a directory, a file mmap MAP_PRIVATE */
  { int dfd = open("/t", O_RDONLY | O_DIRECTORY); int xf = openat(dfd, "x.txt", O_CREAT | O_RDWR, 0644); write(xf, "0123456789", 10); close(xf);
    CHECK(dfd >= 0 && fstatat(dfd, "x.txt", &st, 0) == 0 && st.st_size == 10, "openat/fstatat relative to dirfd");
    xf = openat(dfd, "x.txt", O_RDONLY); char *m = mmap(0, 4096, PROT_READ, MAP_PRIVATE, xf, 0);
    CHECK(m != MAP_FAILED && !memcmp(m, "0123456789", 10), "file mmap MAP_PRIVATE content"); munmap(m, 4096); close(xf); unlinkat(dfd, "x.txt", 0); close(dfd); }
  printf(fails ? "sigtest: FAILED (%d)\n" : "sigtest: ALL OK\n", fails);
  return 0;
}
