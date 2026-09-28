# tlgen: ONE static musl binary for both systems.
#
# Talaria and Linux get a byte-identical tlgen: the measuring instrument must not
# be a variable of the comparison. Linking at 0x10000000 is Talaria's user region
# (USER-GUIDE §6.1); on Linux such an ELF runs as an ordinary ET_EXEC.
#
#   make          — build tlgen (docker talaria-musl)
#   make host     — additionally build tlgen.host (gcc/glibc) for debugging
#   make clean
SRC  := tlgen.c proto_resp.c proto_http.c proto_pg.c proto_mysql.c proto_mongo.c
MUSL := docker run --rm -v $(CURDIR):/w -w /w talaria-musl
CFL  := -O2 -Wall -Wextra -Wno-unused-parameter
LDF  := -static -no-pie -Wl,-Ttext-segment=0x10000000

all: tlgen

tlgen: $(SRC) tlgen.h
	$(MUSL) musl-gcc $(CFL) $(LDF) -o $@ $(SRC) -lm -lpthread

# A debug build on the host: the same sources, an ordinary gcc. Useful when
# gdb/asan are needed; measurements use the musl binary.
host: tlgen.host
tlgen.host: $(SRC) tlgen.h
	gcc $(CFL) -g -o $@ $(SRC) -lm -lpthread

asan: $(SRC) tlgen.h
	gcc $(CFL) -g -fsanitize=address,undefined -o tlgen.asan $(SRC) -lm -lpthread

clean:
	rm -f tlgen tlgen.host tlgen.asan

.PHONY: all host asan clean
