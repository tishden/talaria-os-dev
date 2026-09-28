#!/bin/sh
# Builds a patched musl 1.2.5: tier 1 in libc's futex paths
# (umonitor/umwait or monitorx/mwaitx where the CPU has them; without them the
# ordinary futex, which on Talaria already sleeps in the kernel with mwait). The
# result goes into user/dyn/ as ld-musl-x86_64.so.1, so every DYNAMIC binary in
# the image picks it up automatically through PT_INTERP, with no rebuild.
set -e
D=$(cd "$(dirname "$0")" && pwd)
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
echo "== downloading musl-1.2.5 =="
docker run --rm -v "$W":/s talaria-musl bash -c \
  'curl -sL --retry 5 --max-time 120 -o /s/musl.tgz https://musl.libc.org/releases/musl-1.2.5.tar.gz'
tar xzf "$W/musl.tgz" -C "$W"
echo "== applying the talaria patch =="
patch -p1 -d "$W/musl-1.2.5" < "$D/musl-1.2.5-talaria.patch"
echo "== building (docker talaria-musl) =="
docker run --rm -v "$W":/s talaria-musl bash -c \
  'cd /s/musl-1.2.5 && ./configure --prefix=/s/out >/dev/null && make -j"$(nproc)" >/dev/null 2>&1 && cp lib/libc.so /s/libc-talaria.so'
cp "$W/libc-talaria.so" "$D/../dyn/ld-musl-x86_64.so.1"
echo "== done: user/dyn/ld-musl-x86_64.so.1 updated =="
