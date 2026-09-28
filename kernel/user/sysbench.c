/* A microbench: the cost of a syscall (ring3->ring0->ring3) on Talaria.
   It measures rdtsc around N nop syscalls and around an empty loop (to subtract the overhead). */
#include <stdio.h>
#include <stdint.h>
#include <time.h>
#include <sys/syscall.h>
#include <unistd.h>

static inline uint64_t rdtsc(void){ uint32_t lo,hi; __asm__ volatile("rdtsc":"=a"(lo),"=d"(hi)); return ((uint64_t)hi<<32)|lo; }
static inline long nop_sys(void){ long r; __asm__ volatile("syscall":"=a"(r):"a"(1001UL):"rcx","r11","memory"); return r; }
static inline uint64_t tsc_hz(void){ uint64_t r; __asm__ volatile("syscall":"=a"(r):"a"(1002UL):"rcx","r11","memory"); return r; }

int main(void){
    volatile long acc = 0;
    const uint64_t N = 2000000;
    for (uint64_t i=0;i<10000;i++) acc += nop_sys();      // the warm-up
    uint64_t t0 = rdtsc();
    for (uint64_t i=0;i<N;i++) acc += nop_sys();
    uint64_t t1 = rdtsc();
    for (volatile uint64_t i=0;i<N;i++) acc += (long)i;   // the empty reference loop
    uint64_t t2 = rdtsc();

    /* The clock: libc takes clock_gettime from the vDSO if the kernel gave it out (AT_SYSINFO_EHDR).
       We measure both through libc and FORCIBLY by a syscall — the difference is the vDSO's gain. */
    struct timespec ts;
    const uint64_t M = 200000;
    for (uint64_t i = 0; i < 10000; i++) clock_gettime(CLOCK_MONOTONIC, &ts);
    uint64_t c0 = rdtsc();
    for (uint64_t i = 0; i < M; i++) { clock_gettime(CLOCK_MONOTONIC, &ts); acc += ts.tv_nsec; }
    uint64_t c1 = rdtsc();
    for (uint64_t i = 0; i < M; i++) { syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &ts); acc += ts.tv_nsec; }
    uint64_t c2 = rdtsc();

    /* Do the values agree: the vDSO and the syscall must give one and the same time (< 1 ms) */
    struct timespec a, b;
    clock_gettime(CLOCK_MONOTONIC, &a);
    syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &b);
    long long da = (long long)(b.tv_sec - a.tv_sec) * 1000000000LL + (b.tv_nsec - a.tv_nsec);
    if (da < 0) da = -da;

    uint64_t hz = tsc_hz();
    uint64_t sys_cyc = (t1 - t0) / N;
    uint64_t loop_cyc = (t2 - t1) / N;
    uint64_t net_cyc = sys_cyc > loop_cyc ? sys_cyc - loop_cyc : 0;
    uint64_t sys_ns = sys_cyc * 1000000000ULL / hz;
    uint64_t net_ns = net_cyc * 1000000000ULL / hz;
    {
        uint64_t vdso_ns = (c1 - c0) / M * 1000000000ULL / hz;
        uint64_t sysc_ns = (c2 - c1) / M * 1000000000ULL / hz;
        printf("CLOCKBENCH: clock_gettime through libc = %llu ns, through a syscall = %llu ns, the difference = %lld ns\n",
            (unsigned long long)vdso_ns, (unsigned long long)sysc_ns, (long long)da);
        printf("CLOCKBENCH: %s\n", (vdso_ns * 2 < sysc_ns && da < 1000000) ? "the vDSO WORKS" : "the vDSO is not used");
    }
    printf("SYSBENCH: the syscall(nop) = %llu cycles (%llu ns); net = %llu cycles (%llu ns); the reference loop = %llu cycles; tsc=%llu Hz\n",
        (unsigned long long)sys_cyc, (unsigned long long)sys_ns,
        (unsigned long long)net_cyc, (unsigned long long)net_ns,
        (unsigned long long)loop_cyc, (unsigned long long)hz);
    return 0;
}
