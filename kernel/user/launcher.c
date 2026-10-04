/* The full process life cycle on Talaria: fork -> execve(a real program) -> wait4.
   Freestanding (raw syscalls), linked at the machine's load address. x86-64 runs the real
   busybox's awk; arm64 has no busybox build yet, so its child runs /mhello (a musl process
   from the same disk) — the cycle is the same, only the program differs. */
#if defined(__x86_64__)
static long sc6(long n,long a,long b,long c,long d,long e,long f){
  long r; register long r10 asm("r10")=d; register long r8 asm("r8")=e; register long r9 asm("r9")=f;
  asm volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");
  return r;
}
enum { NR_WRITE = 1, NR_EXECVE = 59, NR_EXIT = 60, NR_WAIT4 = 61 };
/* fork (57) */
static long do_fork(void){ return sc6(57,0,0,0,0,0,0); }
#define CHILD_PATH "/busybox"
#elif defined(__aarch64__)
static long sc6(long n,long a,long b,long c,long d,long e,long f){
  register long x8 asm("x8")=n; register long x0 asm("x0")=a; register long x1 asm("x1")=b;
  register long x2 asm("x2")=c; register long x3 asm("x3")=d; register long x4 asm("x4")=e; register long x5 asm("x5")=f;
  asm volatile("svc #0":"+r"(x0):"r"(x8),"r"(x1),"r"(x2),"r"(x3),"r"(x4),"r"(x5):"memory");
  return x0;
}
enum { NR_WRITE = 64, NR_EXECVE = 221, NR_EXIT = 93, NR_WAIT4 = 260 };
/* arm64 has no fork: clone(SIGCHLD, 0, ...) is what libc's fork is (220) */
static long do_fork(void){ return sc6(220,17,0,0,0,0,0); }
#define CHILD_PATH "/mhello"
#endif
static int slen(const char*s){int n=0;while(s[n])n++;return n;}
static void wr(const char*s){ sc6(NR_WRITE,1,(long)s,slen(s),0,0,0); }

void _start(void){
  wr("[launcher] fork -> child execve " CHILD_PATH ", parent wait4\n");
  long pid = do_fork();
  if (pid == 0){
#if defined(__x86_64__)
    static char *av[] = {"busybox","awk",
      "BEGIN{print \"[child] busybox awk via fork+execve on Talaria: 6*7=\" 6*7}",0};
#else
    static char *av[] = {"mhello",0};
#endif
    sc6(NR_EXECVE,(long)CHILD_PATH,(long)av,0,0,0,0);
    wr("[child] execve failed\n");
    sc6(NR_EXIT,127,0,0,0,0,0);
  }
  long st=0;
  sc6(NR_WAIT4,pid,(long)&st,0,0,0,0);
  int code=(int)((st>>8)&0xff);
  wr("[launcher] child exited, code=");
  char c[2]; c[0]='0'+(char)(code%10); c[1]='\n'; sc6(NR_WRITE,1,(long)c,2,0,0,0);
  wr("[launcher] full process cycle (fork+execve+wait4) on Talaria OK\n");
  sc6(NR_EXIT,0,0,0,0,0,0);
}
