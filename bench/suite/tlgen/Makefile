# tlgen: ONE static musl binary for both systems.
#
# Talaria and Linux get a byte-identical tlgen: the measuring instrument must not
# be a variable of the comparison. Linking at 0x10000000 is Talaria's user region
# (USER-GUIDE §6.1); on Linux such an ELF runs as an ordinary ET_EXEC.
#
#   make          — build tlgen (docker talaria-musl)
#   make host     — additionally build tlgen.host (gcc/glibc) for debugging
#   make a64      — tlgen.a64 for arm64 (both sides of the Graviton stand) with the
#                   aarch64-linux-musl-cross toolchain (musl.cc; CCA64= names it). Linked at
#                   0x50000000: x86's 0x10000000 is a PCIe window on `virt` and on Graviton,
#                   and Talaria keeps a static binary at its link address (services/build-aarch64.sh)
#   make clean
SRC  := tlgen.c proto_resp.c proto_http.c proto_pg.c proto_mysql.c proto_mongo.c
MUSL := docker run --rm -v $(CURDIR):/w -w /w talaria-musl
CFL  := -O2 -Wall -Wextra -Wno-unused-parameter
LDF  := -static -no-pie -Wl,-Ttext-segment=0x10000000

CCA64 ?= $(shell command -v aarch64-linux-musl-gcc 2>/dev/null || echo $(HOME)/.cache/talaria/aarch64-linux-musl-cross/bin/aarch64-linux-musl-gcc)

all: tlgen

tlgen: $(SRC) tlgen.h
	$(MUSL) musl-gcc $(CFL) $(LDF) -o $@ $(SRC) -lm -lpthread

a64: tlgen.a64
tlgen.a64: $(SRC) tlgen.h
	$(CCA64) $(CFL) -static -no-pie -Wl,-Ttext-segment=0x50000000 -o $@ $(SRC) -lm -lpthread

# A debug build on the host: the same sources, an ordinary gcc. Useful when
# gdb/asan are needed; measurements use the musl binary.
host: tlgen.host
tlgen.host: $(SRC) tlgen.h
	gcc $(CFL) -g -o $@ $(SRC) -lm -lpthread

asan: $(SRC) tlgen.h
	gcc $(CFL) -g -fsanitize=address,undefined -o tlgen.asan $(SRC) -lm -lpthread

clean:
	rm -f tlgen tlgen.a64 tlgen.host tlgen.asan

.PHONY: all a64 host asan clean
