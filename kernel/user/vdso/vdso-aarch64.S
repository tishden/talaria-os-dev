/* vdso-aarch64.S — Talaria's vDSO for arm64: the clock in user space, without a
 * syscall, and the signal-return trampoline (docs/ARM64-PLAN.md §4.4, §4.9).
 *
 * The same image layout and the same data page as the x86 vDSO (vdso.S) — the kernel
 * writes the parameters by the same offsets, `vdso.rs` does not know which one it maps:
 *   the page 0: the ELF headers + the code (R+X for the user)
 *   the page 1: vdso_data — the clock's parameters, written by THE KERNEL (R for the user)
 *
 * The data (a seqlock, as on Linux):
 *   +0  seq      u32   odd = the kernel is editing, it must not be read
 *   +8  tsc_base u64   a reading of the counter (CNTVCT_EL0 here)
 *   +16 ns_base  u64   the monotonic nanoseconds at that reading
 *   +24 mult     u64   1e9 * 2^32 / counter_Hz  (ns = (dt * mult) >> 32)
 *   +32 wall_off u64   realtime = monotonic + wall_off
 *
 * What differs from x86, and why:
 *   * the counter is `cntvct_el0` behind an `isb` — without the barrier the read may
 *     be satisfied early, and the kernel lets EL0 read it (CNTKCTL_EL1.EL0VCTEN);
 *   * the symbols are `__kernel_*`, versioned LINUX_2.6.39 — that is what musl and
 *     glibc look for on arm64;
 *   * `__kernel_rt_sigreturn` EXISTS: arm64 has no SA_RESTORER, the kernel points
 *     `lr` at this trampoline when it builds a signal frame, and the two instructions
 *     here are what libc's unwinder recognises as a signal frame;
 *   * the seqlock's re-read is ordered by `dmb ishld` (x86 needed nothing: TSO).
 */
	.text
	.balign 16

/* The common counting: x0 = the monotonic nanoseconds, x11 = wall_off.
   It clobbers x9..x16. */
.macro READ_NS
	adrp	x9, vdso_data
	add	x9, x9, :lo12:vdso_data
9:	ldr	w10, [x9]			/* seq */
	tbnz	w10, #0, 9b
	dmb	ishld
	ldp	x12, x13, [x9, #8]		/* tsc_base, ns_base */
	ldp	x14, x11, [x9, #24]		/* mult, wall_off */
	isb
	mrs	x15, cntvct_el0
	sub	x15, x15, x12			/* dt */
	mul	x16, x15, x14			/* the low 64 bits of dt * mult */
	umulh	x15, x15, x14			/* the high 64 bits */
	lsr	x16, x16, #32
	orr	x0, x16, x15, lsl #32		/* (dt * mult) >> 32 */
	add	x0, x0, x13			/* + ns_base */
	dmb	ishld
	ldr	w12, [x9]			/* has the kernel not edited the data under us? */
	cmp	w12, w10
	b.ne	9b
.endm

/* int __kernel_clock_gettime(clockid_t clk, struct timespec *ts) */
	.globl	__kernel_clock_gettime
	.type	__kernel_clock_gettime, %function
__kernel_clock_gettime:
	cmp	w0, #1				/* CLOCK_MONOTONIC */
	b.eq	1f
	cmp	w0, #4				/* CLOCK_MONOTONIC_RAW */
	b.eq	1f
	cmp	w0, #6				/* CLOCK_MONOTONIC_COARSE */
	b.eq	1f
	cmp	w0, #0				/* CLOCK_REALTIME */
	b.eq	2f
	cmp	w0, #5				/* CLOCK_REALTIME_COARSE */
	b.eq	2f
	mov	x8, #113			/* the other clocks — by an ordinary syscall */
	svc	#0
	ret
1:	READ_NS
	b	3f
2:	READ_NS
	add	x0, x0, x11			/* realtime = monotonic + the offset */
3:	mov	x9, #0xca00			/* 1 000 000 000 = 0x3b9aca00 */
	movk	x9, #0x3b9a, lsl #16
	udiv	x10, x0, x9			/* the seconds */
	msub	x12, x10, x9, x0		/* the nanoseconds */
	stp	x10, x12, [x1]
	mov	x0, #0
	ret
	.size	__kernel_clock_gettime, .-__kernel_clock_gettime

/* int __kernel_gettimeofday(struct timeval *tv, void *tz) */
	.globl	__kernel_gettimeofday
	.type	__kernel_gettimeofday, %function
__kernel_gettimeofday:
	cbz	x0, 5f
	mov	x1, x0
	READ_NS
	add	x0, x0, x11
	mov	x9, #0xca00
	movk	x9, #0x3b9a, lsl #16
	udiv	x10, x0, x9			/* tv_sec */
	msub	x12, x10, x9, x0		/* the nanoseconds */
	mov	x9, #1000
	udiv	x12, x12, x9			/* tv_usec */
	stp	x10, x12, [x1]
5:	mov	x0, #0
	ret
	.size	__kernel_gettimeofday, .-__kernel_gettimeofday

/* int __kernel_clock_getres(clockid_t clk, struct timespec *res) */
	.globl	__kernel_clock_getres
	.type	__kernel_clock_getres, %function
__kernel_clock_getres:
	cmp	w0, #6
	b.hi	7f
	cbz	x1, 6f
	mov	x9, #1				/* one nanosecond: the counter is converted, not truncated */
	stp	xzr, x9, [x1]
6:	mov	x0, #0
	ret
7:	mov	x8, #114
	svc	#0
	ret
	.size	__kernel_clock_getres, .-__kernel_clock_getres

/* The signal-return trampoline: `lr` of a signal handler points here. The nop before
   it is what the unwinders expect (the frame's return address is the second word). */
	.balign 16
	nop
	.globl	__kernel_rt_sigreturn
	.type	__kernel_rt_sigreturn, %function
__kernel_rt_sigreturn:
	mov	x8, #139			/* rt_sigreturn */
	svc	#0
	.size	__kernel_rt_sigreturn, .-__kernel_rt_sigreturn

/* The data page: exactly after the code, it is written by the kernel. */
	.section .vdso_data, "a", %progbits
	.balign 4096
	.globl	vdso_data
vdso_data:
	.space	4096
	.section .note.GNU-stack, "", %progbits
