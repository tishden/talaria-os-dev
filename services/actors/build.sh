#!/bin/sh
# Build actor-bench: SObjectizer-5.8.6 (static) + the benchmark, a static musl binary
# for Talaria (linked at 0x10000000) — the same binary runs in the Linux guest.
# Docker talaria-alpine-build (services/docker/Dockerfile.alpine-build).
set -e
D=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$D/../.." && pwd)
mkdir -p "$D/build"

SO_VER=5.8.6
if [ ! -d "$D/build/sobjectizer-$SO_VER" ]; then
  echo "== downloading SObjectizer $SO_VER =="
  curl -sL -o "$D/build/so.tar.gz" "https://github.com/Stiffstream/sobjectizer/archive/refs/tags/v$SO_VER.tar.gz"
  tar xzf "$D/build/so.tar.gz" -C "$D/build"
  rm "$D/build/so.tar.gz"
fi

echo "== building (docker talaria-alpine-build) =="
docker run --rm -i -v "$REPO":/w talaria-alpine-build sh -e <<INNER
cd /w/services/actors/build
# 1. the static so_5 library
if [ ! -f so-build/lib/libso_s.$SO_VER.a ]; then
  mkdir -p so-build && cd so-build
  cmake -DCMAKE_BUILD_TYPE=Release -DSOBJECTIZER_BUILD_SHARED=OFF -DSOBJECTIZER_INSTALL=OFF \
        ../sobjectizer-$SO_VER/dev >/dev/null
  make -j\$(nproc) >/dev/null
  cd ..
fi
# 2. libtsync (C11) + the shim
gcc -O2 -c -o tsync.o /w/kernel/user/tsync/tsync.c
gcc -O2 -c -I/w/kernel/user/tsync -o tsync_shim.o /w/services/actors/bench/tsync_shim.c
# 3. the benchmark: static musl, linked at 0x10000000 (the Talaria user region)
g++ -O2 -std=c++17 -DSO_5_STATIC_LIB \
    -I sobjectizer-$SO_VER/dev -I /w/services/actors/bench \
    -static -no-pie -Wl,-Ttext-segment=0x10000000 \
    -o actor-bench /w/services/actors/bench/actor_bench.cpp \
    tsync.o tsync_shim.o so-build/lib/libso_s.$SO_VER.a -lpthread
file actor-bench
INNER
echo "== done: $D/build/actor-bench =="
