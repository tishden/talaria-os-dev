/* vdso.S — Talaria's vDSO: the clock in user space, without a syscall.
 *
 * Why: a syscall on this machine costs 95 ns, and `clock_gettime` 172 ns, and the
 * runtimes pull it all the time (the JVM's System.nanoTime — on every message in a
 * queue). Linux gives a process a page of code that reads the TSC and converts it into
 * nanoseconds itself; here it is the same.
 *
 * The layout of the image (the kernel knows it too, vdso.rs):
 *   the page 0: the ELF headers + the code (R+X for the user)
 *   the page 1: vdso_data — the clock's parameters, written by THE KERNEL (R for the user)
 *
 * The data (a seqlock, as on Linux):
 *   +0  seq      u32   odd = the kernel is editing, it must not be read
 *   +8  tsc_base u64   a reading of the TSC
 *   +16 ns_base  u64   the monotonic nanoseconds at that reading
 *   +24 mult     u64   1e9 * 2^32 / TSC_Hz  (ns = (dt * mult) >> 32)
 *   +32 wall_off u64   realtime = monotonic + wall_off
 */
	.text
	.balign 16

/* The common counting: r8 = tsc_base, r9 = ns_base, r10 = mult, r11 = wall_off.
   It returns the nanoseconds in rax. It clobbers rax, rcx, rdx, r8-r11, ebx (saved). */
.macro READ_NS
9:	movl	vdso_data(%rip), %ebx		/* seq */
	testb	$1, %bl
	jnz	9b
	movq	vdso_data+8(%rip), %r8
	movq	vdso_data+16(%rip), %r9
	movq	vdso_data+24(%rip), %r10
	movq	vdso_data+32(%rip), %r11
	rdtsc
	shlq	$32, %rdx
	orq	%rdx, %rax			/* rax = tsc */
	subq	%r8, %rax			/* dt */
	mulq	%r10				/* rdx:rax = dt * mult */
	shrdq	$32, %rdx, %rax			/* rax = (dt*mult) >> 32 = the ns from the base */
	addq	%r9, %rax
	cmpl	vdso_data(%rip), %ebx		/* has the kernel not edited the data under us? */
	jne	9b
.endm

/* long __vdso_clock_gettime(clockid_t clk, struct timespec *ts) */
	.globl	__vdso_clock_gettime
	.type	__vdso_clock_gettime, @function
__vdso_clock_gettime:
	pushq	%rbx
	movl	%edi, %ecx
	cmpl	$1, %ecx			/* CLOCK_MONOTONIC */
	je	1f
	cmpl	$4, %ecx			/* CLOCK_MONOTONIC_RAW */
	je	1f
	cmpl	$6, %ecx			/* CLOCK_MONOTONIC_COARSE */
	je	1f
	cmpl	$0, %ecx			/* CLOCK_REALTIME */
	je	2f
	cmpl	$5, %ecx			/* CLOCK_REALTIME_COARSE */
	je	2f
	popq	%rbx				/* the other clocks — by an ordinary syscall */
	movl	$228, %eax
	syscall
	ret
1:	READ_NS
	jmp	3f
2:	READ_NS
	addq	%r11, %rax			/* realtime = monotonic + the offset */
3:	movl	$1000000000, %ecx
	xorl	%edx, %edx
	divq	%rcx				/* rax = the seconds, rdx = the nanoseconds */
	movq	%rax, (%rsi)
	movq	%rdx, 8(%rsi)
	xorl	%eax, %eax
	popq	%rbx
	ret
	.size	__vdso_clock_gettime, .-__vdso_clock_gettime

/* long __vdso_gettimeofday(struct timeval *tv, void *tz) */
	.globl	__vdso_gettimeofday
	.type	__vdso_gettimeofday, @function
__vdso_gettimeofday:
	pushq	%rbx
	testq	%rdi, %rdi
	jz	5f
	READ_NS
	addq	%r11, %rax
	movl	$1000000000, %ecx
	xorl	%edx, %edx
	divq	%rcx
	movq	%rax, (%rdi)			/* tv_sec */
	movq	%rdx, %rax
	xorl	%edx, %edx
	movl	$1000, %ecx
	divq	%rcx				/* the nanoseconds -> the microseconds */
	movq	%rax, 8(%rdi)			/* tv_usec */
5:	xorl	%eax, %eax
	popq	%rbx
	ret
	.size	__vdso_gettimeofday, .-__vdso_gettimeofday

/* long __vdso_time(time_t *t) */
	.globl	__vdso_time
	.type	__vdso_time, @function
__vdso_time:
	pushq	%rbx
	movq	%rdi, %rsi
	READ_NS
	addq	%r11, %rax
	movl	$1000000000, %ecx
	xorl	%edx, %edx
	divq	%rcx				/* rax = the seconds */
	testq	%rsi, %rsi
	jz	6f
	movq	%rax, (%rsi)
6:	popq	%rbx
	ret
	.size	__vdso_time, .-__vdso_time

/* long __vdso_getcpu(unsigned *cpu, unsigned *node, void *cache) — by a syscall */
	.globl	__vdso_getcpu
	.type	__vdso_getcpu, @function
__vdso_getcpu:
	movl	$309, %eax
	syscall
	ret
	.size	__vdso_getcpu, .-__vdso_getcpu

/* The data page: exactly after the code, it is written by the kernel. */
	.section .vdso_data, "a", @progbits
	.balign 4096
	.globl	vdso_data
vdso_data:
	.space	4096
