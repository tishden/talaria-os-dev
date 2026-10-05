#!/bin/sh
# The guest tree for PostgreSQL 16 (shared by Talaria and Linux).
# Authentication is trust: the suite's generator deliberately cannot do md5/SCRAM
# (see proto_pg.c), and the subject of the measurement is the query round trip, not SASL parsing.
set -eu
OUT=${1:?directory}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../../.." && pwd)
SRC="$REPO/services/postgresql/build"
. "$HERE/../../lib/arch.sh"
[ -d "$SRC/pgdata" ] || { echo "no $SRC/pgdata (initdb)"; exit 1; }

mkdir -p "$OUT/opt/pg/bin" "$OUT/opt/pg/share" "$OUT/opt/pg/lib/postgresql" "$OUT/tmp" "$OUT/etc"
if [ "$ARCH" = arm64 ]; then
    # Alpine's aarch64 PostgreSQL 16 (dynamic) with its closure, as the arm64 gates run it
    # (services/build-aarch64-db.sh). The binary goes to x86's path so that the manifest,
    # the spec and their measured settings stay ONE file for both machines; its share
    # directory is found at the compiled-in /usr/share/postgresql16 (PostgreSQL falls back
    # to the absolute path when the binary is not under its compiled bindir). The data
    # directory is x86's initdb — the same bytes on both little-endian machines.
    . "$REPO/services/apkroot.sh"
    apkroot "$OUT.apk" v3.20 postgresql16 >&2 || exit 1
    [ -f "$OUT.apk/usr/libexec/postgresql16/postgres" ] || { echo "no postgres in the unpacked tree"; exit 1; }
    cp -a "$OUT.apk/." "$OUT/"; rm -rf "$OUT.apk"
    rm -rf "$OUT/usr/share/doc" "$OUT/usr/share/man" "$OUT/usr/include" "$OUT/usr/share/postgresql16/extension" \
        "$OUT/usr/share/postgresql16/tsearch_data" "$OUT"/usr/lib/postgresql16/llvmjit*
    mv "$OUT/usr/libexec/postgresql16/postgres" "$OUT/opt/pg/bin/postgres"
else
    [ -x "$SRC/pg-static/opt/pg/bin/postgres" ] || { echo "no static postgres — run services/postgresql/build.sh"; exit 1; }
    cp "$SRC/pg-static/opt/pg/bin/postgres" "$OUT/opt/pg/bin/"
    cp -r "$SRC/pg-static/opt/pg/share/postgresql" "$OUT/opt/pg/share/"
    rm -rf "$OUT/opt/pg/share/postgresql/extension" "$OUT/opt/pg/share/postgresql/tsearch_data"
fi
cp -r "$SRC/pgdata" "$OUT/pgdata"
chmod 700 "$OUT/pgdata"
rm -f "$OUT/pgdata/postmaster.pid"
printf 'root:x:0:0:root:/root:/bin/sh\npostgres:x:26:26:postgres:/:/bin/sh\n' > "$OUT/etc/passwd"

# Measurement settings. fsync is switched off IDENTICALLY on both sides: the test cases
# are reads and protocol round trips, and with fsync on they would measure the medium,
# which has its own barrier discipline under a hypervisor.
cat >> "$OUT/pgdata/postgresql.conf" <<'C'
# --- the bench/suite set ---
listen_addresses = '*'
fsync = off
synchronous_commit = off
full_page_writes = off
logging_collector = off
# Startup messages are NOT silenced: they are printed once and do not disturb the
# measurement, while without them a silent server exit cannot be told from a successful
# start (exactly that cost an evening). Only per-statement logging disturbs the
# measurement — log_statement turns it off.
log_min_messages = warning
log_min_error_statement = error
log_statement = 'none'
log_connections = off
log_disconnections = off
max_connections = 200
shared_buffers = 256MB
C
echo "the postgresql tree is ready: $OUT"
