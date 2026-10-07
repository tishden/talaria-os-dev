# The build of Talaria's test user binaries from the sources (RELEASE-1.0 §4.6).
# The toolchains are docker images (services/docker/build-images.sh):
#   talaria-musl         — musl-gcc/as/ld (C and the assembler)
#   talaria-alpine-build — g++ (static C++ under musl)
# make all — build everything; make disks — rebuild disk.img (mkfs.py).
# The third-party artefacts are NOT built here (see README.md):
#   dyn/ld-musl-x86_64.so.1 — a patched musl (tsync/build-musl.sh),
#   dyn/libz.so.1 (Alpine), glibc/* (Ubuntu 22.04) — the dyn*/glibc tests
#   are SKIPped by the regression when they are absent.
MUSL := docker run --rm -v $(CURDIR):/w -w /w talaria-musl
CXXI := docker run --rm -v $(CURDIR):/w -w /w talaria-alpine-build
LDF  := -static -no-pie -Wl,-Ttext-segment=0x10000000

# C (musl-gcc), the binary's name = the name of the .c
CBINS := connect_test efault epsrv execmt fastapp fastflood forksrv fputest fsbig fsinfo fsread fsyncconc futexlat \
         forktls idleshoot mapshared mbench memhog memrace memtest migregs mixwait mtsrv netshoot pthreads schedapi schedbal schedpin schedspread \
         piperace rqwait shmrace sigtest smp2 smp4 srv stealbal sysbench tcp_bench tickcmd udp_ping userver wvsock
# C with a source name that does not match
mhello_SRC  := mh.c
mprintf_SRC := mp.c
# C++ (alpine g++)
CPPBINS := cppdtor cpptest cpptest2 cpptest3
# The assembler (as + ld -T user.ld): *.elf and the bare binaries uaftest/sfilter
ASMELF  := hello.elf devnull.elf forktest.elf
ASMBIN  := uaftest sfilter
# libtsync
TSYNC   := tsync/tsynctest tsync/syncbench tsync/fiberbench tsync/tsynctest.dyn

all: $(CBINS) sigtest-hi sigtest-lo mhello mprintf launcher profdeep crashme $(CPPBINS) $(ASMELF) $(ASMBIN) $(TSYNC) dyn/hello

$(CBINS): %: %.c tal_arch.h
	$(MUSL) sh -c 'musl-gcc -O2 $(LDF) -o $@ $< && strip $@'

# sigtest linked at 1 GiB — inside RAM's former identity on x86 (GB1): it loads and works only when
# a process space has none of it (`talaria.dmap=strict`, docs/ARM64-PLAN.md §10.16 stage 4b).
sigtest-hi: sigtest.c tal_arch.h
	$(MUSL) sh -c 'musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x40000000 -o $@ $< && strip $@'

# sigtest linked at 2 MB — where the KERNEL'S IMAGE is loaded on x86: it runs only when the image
# lives at its high alias and a strict process space keeps none of its identity (stage 7.4).
sigtest-lo: sigtest.c tal_arch.h
	$(MUSL) sh -c 'musl-gcc -O2 -static -no-pie -Wl,-Ttext-segment=0x200000 -o $@ $< && strip $@'

# AS A SEPARATE RULE: the gate of the stack unwinding requires frame pointers
# (otherwise there is no chain of frames at all) and does NOT tolerate strip (otherwise
# addr2line stays silent). Both differences are exactly what the guide asks the user for
# when profiling their own workload.
profdeep: profdeep.c
	$(MUSL) musl-gcc -O2 -fno-omit-frame-pointer $(LDF) -o $@ $<

# crashme — also WITHOUT strip and with frame pointers: it is used to check that a
# workload's core dump opens in gdb with the NAMES of the functions, while a stripped
# binary has no names by construction, and the gate would be checking only the format.
crashme: crashme.c
	$(MUSL) musl-gcc -O2 -g -fno-omit-frame-pointer $(LDF) -o $@ $<

mhello: mh.c
	$(MUSL) sh -c 'musl-gcc -O2 $(LDF) -o $@ $< && strip $@'
mprintf: mp.c
	$(MUSL) sh -c 'musl-gcc -O2 $(LDF) -o $@ $< && strip $@'
# freestanding (its own _start, raw syscalls)
launcher: launcher.c
	$(MUSL) musl-gcc -O2 -static -no-pie -nostartfiles -Wl,-Ttext-segment=0x10000000 -o $@ $<

$(CPPBINS): %: %.cpp
	$(CXXI) sh -c 'g++ -O2 -pthread $(LDF) -o $@ $< && strip $@'

%.o: %.s
	$(MUSL) as -o $@ $<
$(ASMELF): %.elf: %.o user.ld
	$(MUSL) ld -T user.ld -o $@ $<
$(ASMBIN): %: %.o user.ld
	$(MUSL) ld -T user.ld -o $@ $<

# libtsync: the benches are linked with tsync.c (+fiber.c for fiberbench)
tsync/tsynctest: tsync/tsynctest.c tsync/tsync.c tsync/tsync.h
	$(MUSL) sh -c 'musl-gcc -O2 $(LDF) -o $@ tsync/tsynctest.c tsync/tsync.c && strip $@'
tsync/syncbench: tsync/syncbench.c tsync/tsync.c tsync/tsync.h
	$(MUSL) sh -c 'musl-gcc -O2 $(LDF) -o $@ tsync/syncbench.c tsync/tsync.c && strip $@'
tsync/fiberbench: tsync/fiberbench.c tsync/fiber.c tsync/fiber.h tsync/tsync.c
	$(MUSL) sh -c 'musl-gcc -O2 $(LDF) -o $@ tsync/fiberbench.c tsync/fiber.c tsync/tsync.c && strip $@'

# the dynamic ones (PIE; the PT_INTERP will point at /lib/ld-musl in the disk image)
dyn/hello: dyn/hello.c
	$(MUSL) musl-gcc -O2 -fPIE -pie -rdynamic -o $@ $<
tsync/tsynctest.dyn: tsync/tsynctest.c tsync/tsync.c
	$(MUSL) musl-gcc -O2 -fPIE -pie -o $@ tsync/tsynctest.c tsync/tsync.c

# ── AArch64 (docs/ARM64-PLAN.md §10.1, stage 3.4): the same sources, the same flags, into
# aarch64/, with the cross musl of the talaria-musl-aarch64 image (services/docker/
# Dockerfile.musl-aarch64). MUSLA64= names another toolchain runner: `make aarch64 MUSLA64=`
# runs a musl-gcc from the host's PATH (a cross musl built by hand, as on the dev box).
MUSLA64 ?= docker run --rm -v $(CURDIR):/w -w /w talaria-musl-aarch64
# The link address IS the load address (the loader keeps a static binary's identity: vaddr ==
# phys, `user.rs`), so it must be RAM on the machine: QEMU `virt` and Graviton have RAM from
# 0x4000_0000, and 0x1000_0000 — x86's choice — is a PCIe window there, which reads as zeros
# (the first arm64 `hello` faulted on `udf #0` at its own entry). 0x5000_0000 is 256 MiB in.
LDFA64 := -static -no-pie -Wl,-Ttext-segment=0x50000000
# Every C source builds for arm64 except sysbench (it times with `rdtsc` in its own
# assembler); fastapp/fastflood take the machine's call and spin hint from tal_arch.h.
# The `.s` files (but hello) and libtsync's fibers are not ported yet.
A64CBINS := $(filter-out sysbench,$(CBINS))
A64 := $(addprefix aarch64/,$(A64CBINS) mhello mprintf hello.elf crashme profdeep launcher idregs)
# idregs: arm64 only (the EL0 reads of the ID registers, HWCAP_CPUID)
# The C++ set needs a g++ whose libstdc++ was built against musl for aarch64: the
# `aarch64-linux-musl-cross` toolchain (musl.cc; unpack it and put its bin/ on the PATH, or
# name the compiler: `make aarch64 CXXA64=/path/aarch64-linux-musl-g++`). Without it the
# four binaries are not built and the gate skips them by name.
CXXA64 ?= aarch64-linux-musl-g++
A64CPP := $(if $(shell command -v $(CXXA64) 2>/dev/null),$(addprefix aarch64/,$(CPPBINS)))
aarch64: $(A64) $(A64CPP)
$(addprefix aarch64/,$(CPPBINS)): aarch64/%: %.cpp
	@mkdir -p aarch64
	$(CXXA64) -O2 -pthread $(LDFA64) -o $@ $< && $(dir $(shell command -v $(CXXA64)))aarch64-linux-musl-strip $@
aarch64/crashme: crashme.c
	@mkdir -p aarch64
	$(MUSLA64) musl-gcc -O2 -g -fno-omit-frame-pointer $(LDFA64) -o $@ $<
aarch64/profdeep: profdeep.c
	@mkdir -p aarch64
	$(MUSLA64) musl-gcc -O2 -fno-omit-frame-pointer $(LDFA64) -o $@ $<
aarch64/launcher: launcher.c
	@mkdir -p aarch64
	$(MUSLA64) musl-gcc -O2 -static -no-pie -nostartfiles $(LDFA64) -o $@ $<
aarch64/%: %.c tal_arch.h
	@mkdir -p aarch64
	$(MUSLA64) sh -c 'musl-gcc -O2 $(LDFA64) -o $@ $< && aarch64-linux-gnu-strip $@'
aarch64/mhello: mh.c
	@mkdir -p aarch64
	$(MUSLA64) sh -c 'musl-gcc -O2 $(LDFA64) -o $@ $< && aarch64-linux-gnu-strip $@'
aarch64/mprintf: mp.c
	@mkdir -p aarch64
	$(MUSLA64) sh -c 'musl-gcc -O2 $(LDFA64) -o $@ $< && aarch64-linux-gnu-strip $@'
aarch64/hello.elf: hello-aarch64.s user-aarch64.ld
	@mkdir -p aarch64
	$(MUSLA64) sh -c 'aarch64-linux-gnu-as -o aarch64/hello.o $< && aarch64-linux-gnu-ld -T user-aarch64.ld -o $@ aarch64/hello.o && rm -f aarch64/hello.o'

disks: mhello epsrv memhog
	cd .. && python3 mkfs.py

clean:
	rm -f $(CBINS) mhello mprintf $(CPPBINS) $(ASMELF) $(ASMBIN) $(TSYNC) \
	      *.o dyn/hello tsync/tsynctest.dyn

.PHONY: all disks clean
