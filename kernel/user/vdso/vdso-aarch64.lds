/* The versions of the symbols: glibc takes only VERSIONED symbols from a vDSO
   (musl takes any). The name of the version is the arm64 kernel's. */
LINUX_2.6.39 {
global:
	__kernel_clock_gettime;
	__kernel_gettimeofday;
	__kernel_clock_getres;
	__kernel_rt_sigreturn;
local: *;
};
