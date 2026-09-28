# hello-talaria: build with the docker toolchains (see README.md).
# make            — both binaries + disk.img (default workload: hello)
# make run        — build and run in QEMU (needs /dev/kvm)
# make APP=hellopp run  — the same with the C++ example
MUSL_IMG  ?= talaria-musl
CXX_IMG   ?= talaria-alpine-build
APP       ?= hello
LDFLAGS    = -static -no-pie -Wl,-Ttext-segment=0x10000000
PACK       = ../../kernel/tools/talaria-pack

all: hello hellopp disk.img

hello: hello.c
	docker run --rm -v $(CURDIR):/w -w /w $(MUSL_IMG) musl-gcc -O2 $(LDFLAGS) -o $@ $<

hellopp: hellopp.cpp
	docker run --rm -v $(CURDIR):/w -w /w $(CXX_IMG) g++ -O2 -pthread $(LDFLAGS) -o $@ $<

# always rebuilt (cheap): the content depends on APP=, which make cannot see
disk.img: $(APP) greeting.txt $(PACK) FORCE
	python3 $(PACK) -o $@ --app $(APP) --args="from-manifest" --restart 1 greeting.txt

FORCE:

run: disk.img
	./run.sh

clean:
	rm -f hello hellopp disk.img

.PHONY: all run clean FORCE
