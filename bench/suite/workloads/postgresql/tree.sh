#!/bin/sh
# The guest tree for PostgreSQL 16 (shared by Talaria and Linux).
# Authentication is trust: the suite's generator deliberately cannot do md5/SCRAM
# (see proto_pg.c), and the subject of the measurement is the query round trip, not SASL parsing.
set -eu
OUT=${1:?directory}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../../.." && pwd)
SRC="$REPO/services/postgresql/build"
[ -x "$SRC/pg-static/opt/pg/bin/postgres" ] || { echo "no static postgres — run services/postgresql/build.sh"; exit 1; }
[ -d "$SRC/pgdata" ] || { echo "no $SRC/pgdata (initdb)"; exit 1; }

mkdir -p "$OUT/opt/pg/bin" "$OUT/opt/pg/share" "$OUT/opt/pg/lib/postgresql" "$OUT/tmp" "$OUT/etc"
cp "$SRC/pg-static/opt/pg/bin/postgres" "$OUT/opt/pg/bin/"
cp -r "$SRC/pg-static/opt/pg/share/postgresql" "$OUT/opt/pg/share/"
rm -rf "$OUT/opt/pg/share/postgresql/extension" "$OUT/opt/pg/share/postgresql/tsearch_data"
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
