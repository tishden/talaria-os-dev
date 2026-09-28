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
         forktls idleshoot mapshared mbench memhog memrace memtest mtsrv netshoot pthreads schedapi schedbal schedpin schedspread \
         rqwait shmrace sigtest smp2 smp4 srv stealbal sysbench tcp_bench tickcmd udp_ping userver wvsock
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

all: $(CBINS) mhello mprintf launcher profdeep crashme $(CPPBINS) $(ASMELF) $(ASMBIN) $(TSYNC) dyn/hello

$(CBINS): %: %.c
	$(MUSL) sh -c 'musl-gcc -O2 $(LDF) -o $@ $< && strip $@'

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

disks: mhello epsrv memhog
	cd .. && python3 mkfs.py

clean:
	rm -f $(CBINS) mhello mprintf $(CPPBINS) $(ASMELF) $(ASMBIN) $(TSYNC) \
	      *.o dyn/hello tsync/tsynctest.dyn

.PHONY: all disks clean
