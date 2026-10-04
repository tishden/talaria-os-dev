#!/bin/bash
# The large services on arm64 (docs/ARM64-PLAN.md phase 7): PostgreSQL, MariaDB, MongoDB, the
# JVM and ClickHouse — the distributions' aarch64 builds from `services/build-aarch64-db.sh`,
# each from its ext2 volume in `talaria.mode=proc`, each judged by a RESULT (a row read back, a
# sum, a verdict line), not by "it started". The arm64 counterpart of validate.sh B10-B12. It
# runs INSIDE the stand image (talaria-qemu-aarch64) with a tap, as test-aarch64-net.sh;
# `kernel/test-aarch64.sh` starts it when the images are there and adds its totals. By hand:
#   docker run --rm --cap-add NET_ADMIN --device /dev/net/tun -v "$PWD:/w" -w /w \
#     talaria-qemu-aarch64 bash kernel/test-aarch64-db.sh [pg mdb mongo jvm ch]
# The environment: TAL_IMG (the kernel image), TAL_ACCEL (the accelerator flags), TAL_KVM=1
# (the stand: the budgets are a KVM machine's), TAL_SVCDIR (the images; kernel/target/a64-services).
# The clients are Python (services/*/…probe.py): no x86 binary in the gate, so the stand's
# arm64 host runs it unchanged. Under TCG a database's start is minutes, not seconds — the
# budgets below are TCG's; the KVM stand's are a tenth.
set -u
IMG=${TAL_IMG:-kernel/target/aarch64-talaria/release/talaria.img}
ACCEL=${TAL_ACCEL:--cpu max}
KVM=${TAL_KVM:-0}
SV=${TAL_SVCDIR:-kernel/target/a64-services}
WHAT=${*:-pg mdb mongo jvm ch}
PASS=0; FAIL=0
O=/tmp/db.log
ip tuntap add tap0 mode tap 2>/dev/null
ip addr add 10.0.0.1/24 dev tap0 2>/dev/null; ip link set tap0 up 2>/dev/null
ip link show tap0 >/dev/null 2>&1 || { echo "  FAIL  db: no tap0 (--cap-add NET_ADMIN --device /dev/net/tun)"; echo "DB TOTAL: PASS=0 FAIL=1"; exit 1; }
# The boot's extra knobs. Under TCG every PostgreSQL process waits in epoll on a set with a
# signalfd and SPINS there (the product's default — a waiter is a dedicated core's): six
# spinners left a backend 26 % of an emulated core and a `select 1` took up to 147 s. With
# `talaria.netmix=` the non-polling ones park (docs/ARM64-PLAN.md §10.13); the stand keeps the
# default, as x86's validate.sh does. TAL_DBAPP= overrides either.
if [ "$KVM" = 1 ]; then DBAPP=${TAL_DBAPP-}; else DBAPP=${TAL_DBAPP-talaria.netmix=1000000}; fi
# A budget in seconds: TCG's, or a fifth of it on the stand.
budget() { if [ "$KVM" = 1 ]; then echo $(( $1 / 5 + 20 )); else echo "$1"; fi; }

# bootdb SMP MEM APPEND IMAGE: the guest in the background on a COPY of IMAGE (a run writes
# its volume), a NIC on tap0, the console into $O; sets $Q.
bootdb() {
  rm -f $O; cp "/w/$SV/$4" /tmp/run.img
  qemu-system-aarch64 -M virt,gic-version=3,its=on $ACCEL -smp "$1" -m "$2" -display none -serial file:$O \
    -kernel "/w/$IMG" -append "console=ttyAMA0 talaria.mode=proc $DBAPP $3 talaria.smp=$1" \
    -netdev tap,id=n0,ifname=tap0,script=no,downscript=no \
    -device virtio-net-device,netdev=n0,mac=52:54:00:12:34:56 \
    -drive id=d0,file=/tmp/run.img,format=raw,if=none -device virtio-blk-device,drive=d0 >/tmp/q.log 2>&1 &
  Q=$!
}
stopq() { kill $Q 2>/dev/null; wait $Q 2>/dev/null; }
alive() { kill -0 $Q 2>/dev/null; }
ok()   { echo "  PASS  $1"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL  $1"; FAIL=$((FAIL+1)); [ -n "${2:-}" ] && printf '%s\n' "$2" | tail -4 | cut -c1-160 | sed 's/^/      | /'
         grep -aE '\[trap\]|PANIC|an unknown number|exit_group' $O 2>/dev/null | tail -3 | cut -c1-160 | sed 's/^/      /'
         # its own directory, owned as the tree is: the container runs as root, and a root-owned
         # a64-fail/ stopped the main gate from clearing and keeping its own consoles
         mkdir -p /w/kernel/target/a64-fail-db && cp $O "/w/kernel/target/a64-fail-db/db-$((PASS+FAIL)).log" 2>/dev/null
         chown -R --reference=/w/kernel /w/kernel/target/a64-fail-db 2>/dev/null; }
refute() { if grep -aqE "$1" $O 2>/dev/null; then bad "$2 (found /$1/)"; else ok "$2"; fi; }
BAD='\[trap\]|PANIC|an unknown number'
# until SECS CMD...: repeat CMD every 2 s until it succeeds, QEMU dies or SECS pass; the
# seconds it took in $T.
until_ok() {
  local s=$1 t0=$SECONDS; shift
  while [ $((SECONDS - t0)) -lt "$s" ]; do
    "$@" >/dev/null 2>&1 && { T=$((SECONDS - t0)); return 0; }
    alive || break; sleep 2
  done; T=$((SECONDS - t0)); return 1
}
have() { [ -f "/w/$SV/$1" ] || { echo "  SKIP  $2: there is no $SV/$1 (services/build-aarch64-db.sh)"; return 1; }; }

echo "== the large services on arm64 (the distributions' aarch64 builds, mode=proc, ext2, virtio-net) =="
for w in $WHAT; do case $w in
pg) have ext2-pg.img postgresql || continue
  export PGPROBE_TIMEOUT=$(budget 240)
  PGP="python3 /w/services/postgresql/pgprobe.py 10.0.0.2 5432"
  bootdb 4 1024 "" ext2-pg.img
  if until_ok "$(budget 400)" $PGP "select 1"; then
    ok "postgresql: the postmaster answers the protocol after $T s (Alpine's aarch64 postgres 16, the x86 initdb's data)"
    R=$($PGP "select version(); create table if not exists t(id serial primary key, v text); insert into t(v) values ('talaria'),('bare metal'); select id, v from t order by id; select count(*) from pg_class;" 2>&1)
    if echo "$R" | grep -q 'aarch64' && echo "$R" | grep -q '|bare metal' && ! echo "$R" | grep -q ERROR; then
      ok "postgresql: a forked backend creates a table, inserts and reads back ($(echo "$R" | grep -o 'PostgreSQL [0-9.]* on aarch64[^,]*'))"
    else bad "postgresql: the query set" "$R"; fi
    # A second connection, a second backend: the postmaster forks per connection
    R2=$($PGP "select v from t where id = 2" 2>&1)
    echo "$R2" | grep -q '^bare metal$' && ok "postgresql: a second backend reads the row the first one committed" || bad "postgresql: the second backend" "$R2"
  else bad "postgresql: no answer within $(budget 400) s"; fi
  refute "$BAD" "postgresql: no trap, no unknown syscall"
  stopq ;;
mdb) have ext2-mdb.img mariadb || continue
  export MYSQLPROBE_TIMEOUT=$(budget 240)
  MYP="python3 /w/services/mariadb/mysqlprobe.py 10.0.0.2 3306"
  bootdb 4 1024 "" ext2-mdb.img
  if until_ok "$(budget 500)" $MYP "select 1"; then
    ok "mariadb: the server answers the MySQL protocol after $T s (Alpine's aarch64 mariadbd 10.11, the x86 install-db's data)"
    R=$($MYP "select version(); create database if not exists talaria; use talaria; create table if not exists t(id int auto_increment primary key, v varchar(32)) engine=innodb; insert into t(v) values ('talaria'),('bare metal'); select id, v from t order by id" 2>&1)
    if echo "$R" | grep -q 'bare metal' && echo "$R" | grep -q 'MariaDB' && ! echo "$R" | grep -q ' ERR '; then
      ok "mariadb: an InnoDB table created, written and read back by a connection's thread ($(echo "$R" | grep -o '10\.11\.[0-9]*-MariaDB' | head -1))"
    else bad "mariadb: the query set" "$R"; fi
  else bad "mariadb: no answer within $(budget 500) s"; fi
  refute "$BAD" "mariadb: no trap, no unknown syscall"
  stopq ;;
mongo) have ext2-mongo.img mongodb || continue
  export MONGOPROBE_TIMEOUT=$(budget 240)
  MGP="python3 /w/services/mongodb/mongoprobe.py 10.0.0.2 27017"
  bootdb 4 1024 "" ext2-mongo.img
  if until_ok "$(budget 600)" $MGP ping; then
    ok "mongodb: mongod answers OP_MSG after $T s (Alpine 3.9's aarch64 mongod 4.0.5: ld-musl and 18 shared objects, WiredTiger)"
    R=$($MGP demo 2>&1)
    echo "$R" | grep -q 'mongoprobe: ALL OK' && ok "mongodb: insert, find, count through the wire protocol ($(echo "$R" | grep -o 'version=[0-9.]*'))" \
      || bad "mongodb: the demo" "$R"
  else bad "mongodb: no answer within $(budget 600) s"; fi
  refute "$BAD" "mongodb: no trap, no unknown syscall"
  stopq ;;
jvm) # TCG runs the short set (the start, the interpreter and C1/C2, a park between threads);
  # the stand runs validate.sh B11's whole one and judges its numbers
  J=ext2-jvm-short.img; [ "$KVM" = 1 ] && J=ext2-jvm.img
  have $J jvm || continue
  bootdb 4 2048 "" $J
  t0=$SECONDS; while [ $((SECONDS - t0)) -lt "$(budget 900)" ] && alive && ! grep -aq 'BENCH DONE' $O 2>/dev/null; do sleep 3; done
  if grep -aq 'java 21.* / OpenJDK 64-Bit Server VM / os Linux.*aarch64' $O && grep -aq 'BENCH DONE' $O && ! grep -aq 'ERROR in test' $O; then
    ok "jvm: Temurin 21 aarch64 runs its set to the end in $((SECONDS - t0)) s ($(grep -ao 'RESULT pingpong p50 [0-9.]* [a-z]*' $O | head -1))"
  else bad "jvm: the set (BENCH DONE, no ERROR in a test)" "$(grep -a 'RESULT\|ERROR\|java ' $O)"; fi
  if [ "$KVM" = 1 ]; then
    SPD=$(sed -n 's/.*RESULT scale speedup4 \([0-9.]*\).*/\1/p' $O | tail -1); PP=$(sed -n 's/.*RESULT pingpong p50 \([0-9.]*\).*/\1/p' $O | tail -1)
    if [ -n "$SPD" ] && [ -n "$PP" ] && awk "BEGIN{exit !($SPD > 2.2 && $PP < 10)}"; then ok "jvm: speedup4=$SPD (>2.2), park/unpark p50=$PP us (<10) — validate.sh B11's thresholds"
    else bad "jvm: speedup4='$SPD' park p50='$PP' (B11: >2.2, <10)"; fi
  fi
  refute "$BAD" "jvm: no trap (HotSpot's SIGSEGVs are delivered, not traps), no unknown syscall"
  stopq ;;
*) echo "  SKIP  '$w' (not yet)";;
esac; done
echo "DB TOTAL: PASS=$PASS FAIL=$FAIL"
