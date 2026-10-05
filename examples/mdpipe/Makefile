# mdpipe: a teaching multithreaded workload (see docs/HANDS-ON-GUIDE.md).
#   make              the binary + disk.img (ext2 with a journal)
#   make check        Talaria ABI compatibility check (talaria-check)
#   make run          run in QEMU/KVM: ./qrun.sh "talaria.mode=cell talaria.smp=$(SMP)"
#   make crash        the disk-crash.img image: a thread crashes at tick 150000
#   make prof         the disk-prof.img image: the compute part is heavier (for profiling)
#   make a64          the same for arm64 (AWS Graviton, QEMU virt): mdpipe-a64 + disk-a64.img, built
#                     with the aarch64-linux-musl-cross toolchain (musl.cc; CXXA64= names it)
CXX_IMG ?= talaria-alpine-build
CXXA64  ?= $(shell command -v aarch64-linux-musl-g++ 2>/dev/null || echo $(HOME)/.cache/talaria/aarch64-linux-musl-cross/bin/aarch64-linux-musl-g++)
SMP     ?= 4
PACK     = python3 ../../kernel/tools/talaria-pack
ARGS    ?= --secs 15 --rate 20000 --workers 2 --out /mdpipe.log
# -g and frame pointers are for stacks in the profile, the dump and the debugger;
# -Ttext-segment — the workload lives above 256 MB (USER-GUIDE §6.1).
CXXFLAGS = -O2 -g -std=c++17 -fno-omit-frame-pointer -pthread
LDFLAGS  = -static -no-pie -Wl,-Ttext-segment=0x10000000

all: mdpipe disk.img

mdpipe: mdpipe.cpp
	docker run --rm -v $(CURDIR):/w -w /w $(CXX_IMG) g++ $(CXXFLAGS) $(LDFLAGS) -o $@ $<

disk.img: mdpipe FORCE
	$(PACK) -o $@ --fs ext2 --journal --size 128M --app mdpipe --args="$(ARGS)" --restart 0

crash: mdpipe
	$(PACK) -o disk-crash.img --fs ext2 --journal --size 128M --app mdpipe \
	  --args="$(ARGS) --crash-at 150000" --restart 0

prof: mdpipe
	$(PACK) -o disk-prof.img --fs ext2 --journal --size 128M --app mdpipe \
	  --args="--secs 20 --rate 20000 --workers 2 --rounds 8000 --out /mdpipe.log" --restart 0

a64: mdpipe-a64 disk-a64.img

mdpipe-a64: mdpipe.cpp
	$(CXXA64) $(CXXFLAGS) $(LDFLAGS) -o $@ $<

disk-a64.img: mdpipe-a64 FORCE
	$(PACK) -o $@ --fs ext2 --journal --size 128M --app mdpipe-a64 --args="$(ARGS)" --restart 0

check: mdpipe
	python3 ../../kernel/tools/talaria-check ./mdpipe --args="--secs 3 --rate 5000" --secs 25

run: disk.img
	./qrun.sh "talaria.mode=cell talaria.smp=$(SMP)" 60 $(SMP)

clean:
	rm -rf mdpipe mdpipe-a64 *.img *.log out

FORCE:
.PHONY: all a64 crash prof check run clean FORCE
