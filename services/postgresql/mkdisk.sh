#!/bin/sh
# PostgreSQL disk for Talaria (ext2 RW): /opt/pg (static postgres + share), /pgdata (initdb), /cell.manifest.
set -eu
cd "$(dirname "$0")"
ROOT=$(mktemp -d)
mkdir -p "$ROOT/opt/pg/bin" "$ROOT/opt/pg/share" "$ROOT/opt/pg/lib/postgresql" "$ROOT/tmp"
cp build/pg-static/opt/pg/bin/postgres "$ROOT/opt/pg/bin/"
cp -r build/pg-static/opt/pg/share/postgresql "$ROOT/opt/pg/share/"
rm -rf "$ROOT/opt/pg/share/postgresql/extension" "$ROOT/opt/pg/share/postgresql/tsearch_data"  # not needed by a server without extensions
cp -r build/pgdata "$ROOT/pgdata"; chmod 700 "$ROOT/pgdata"; rm -f "$ROOT/pgdata/postmaster.pid"
cat > "$ROOT/cell.manifest" <<M
# PostgreSQL 16 postmaster on Talaria (process mode: fork/signals/ext2)
binary=/opt/pg/bin/postgres
args=postgres -D /pgdata
env=PGDATA=/pgdata
env=HOME=/
env=PATH=/opt/pg/bin
M
../../kernel/mkext2.sh "$ROOT" disk-pg.img ${1:-256M}
rm -rf "$ROOT"
ls -la disk-pg.img; e2fsck -fn disk-pg.img >/dev/null 2>&1 && echo "disk-pg.img: ext2 ok"
