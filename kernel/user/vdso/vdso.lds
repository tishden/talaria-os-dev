/* The versions of the symbols: glibc takes only VERSIONED symbols from a vDSO
   (musl takes any). The name of the version is the same as on Linux. */
LINUX_2.6 {
global:
	__vdso_clock_gettime;
	__vdso_gettimeofday;
	__vdso_time;
	__vdso_getcpu;
	clock_gettime;
	gettimeofday;
	time;
	getcpu;
local: *;
};
