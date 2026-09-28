#!/bin/sh
# PLAN-SYNC этап 3: сборка патченного musl 1.2.5 (ярус 1 в futex-путях libc —
# umonitor/umwait|monitorx/mwaitx при наличии; без них — futex, который на
# Talaria уже спит mwait'ом в ядре). Результат кладётся в user/dyn/ как
# ld-musl-x86_64.so.1 — все ДИНАМИЧЕСКИЕ бинари образа подхватывают его
# автоматически через PT_INTERP, без пересборки (автоподхват, слой 2).
set -e
D=$(cd "$(dirname "$0")" && pwd)
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
echo "== качаю musl-1.2.5 =="
docker run --rm -v "$W":/s talaria-musl bash -c \
  'curl -sL --retry 5 --max-time 120 -o /s/musl.tgz https://musl.libc.org/releases/musl-1.2.5.tar.gz'
tar xzf "$W/musl.tgz" -C "$W"
echo "== применяю talaria-патч =="
patch -p1 -d "$W/musl-1.2.5" < "$D/musl-1.2.5-talaria.patch"
echo "== собираю (docker talaria-musl) =="
docker run --rm -v "$W":/s talaria-musl bash -c \
  'cd /s/musl-1.2.5 && ./configure --prefix=/s/out >/dev/null && make -j"$(nproc)" >/dev/null 2>&1 && cp lib/libc.so /s/libc-talaria.so'
cp "$W/libc-talaria.so" "$D/../dyn/ld-musl-x86_64.so.1"
echo "== готово: user/dyn/ld-musl-x86_64.so.1 обновлён =="
