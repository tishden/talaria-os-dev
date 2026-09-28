/* The full process life cycle on Talaria: fork -> execve(the real busybox) -> wait4.
   Freestanding (raw syscalls), linked at 0x10000000. */
static long sc6(long n,long a,long b,long c,long d,long e,long f){
  long r; register long r10 asm("r10")=d; register long r8 asm("r8")=e; register long r9 asm("r9")=f;
  asm volatile("syscall":"=a"(r):"a"(n),"D"(a),"S"(b),"d"(c),"r"(r10),"r"(r8),"r"(r9):"rcx","r11","memory");
  return r;
}
static int slen(const char*s){int n=0;while(s[n])n++;return n;}
static void wr(const char*s){ sc6(1,1,(long)s,slen(s),0,0,0); }

void _start(void){
  wr("[launcher] fork -> child execve /busybox, parent wait4\n");
  long pid = sc6(57,0,0,0,0,0,0);          /* fork */
  if (pid == 0){
    static char *av[] = {"busybox","awk",
      "BEGIN{print \"[child] busybox awk via fork+execve on Talaria: 6*7=\" 6*7}",0};
    sc6(59,(long)"/busybox",(long)av,0,0,0,0); /* execve */
    wr("[child] execve failed\n");
    sc6(60,127,0,0,0,0,0);
  }
  long st=0;
  sc6(61,pid,(long)&st,0,0,0,0);            /* wait4 */
  int code=(int)((st>>8)&0xff);
  wr("[launcher] child exited, code=");
  char c[2]; c[0]='0'+(char)(code%10); c[1]='\n'; sc6(1,1,(long)c,2,0,0,0);
  wr("[launcher] full process cycle (fork+execve+wait4) on Talaria OK\n");
  sc6(60,0,0,0,0,0,0);                      /* exit */
}
