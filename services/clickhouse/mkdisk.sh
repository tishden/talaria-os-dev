#!/bin/sh
# The ClickHouse disk for Talaria: the official binary (clickhouse-common-static) +
# the glibc runtime (Ubuntu 22.04), because "static" in the package name refers to
# ClickHouse's dependencies, while the binary itself is a DYNAMIC ELF under ld-linux.
#
#   /clickhouse              the binary (~700 MB, ET_EXEC, segments 2 MB..526 MB)
#   /lib64, /lib/x86_64-linux-gnu   ld-linux + libc/libm/libdl/librt/libpthread
#   /q.sql                   the queries (manifest arguments cannot contain spaces)
#   /cell.manifest           what to run
# mkdisk.sh [size]   env: CH_ARGS="local --queries-file /q.sql" CH_QUERY="SELECT 1"
set -eu
cd "$(dirname "$0")"
BIN=$(ls build/clickhouse-common-static-*/usr/bin/clickhouse | head -1)
[ -f "$BIN" ] || { echo "no ClickHouse binary in build/ (unpack the tgz)"; exit 1; }
ROOT=$(mktemp -d)
mkdir -p "$ROOT/lib64" "$ROOT/lib/x86_64-linux-gnu" "$ROOT/etc" "$ROOT/tmp" "$ROOT/proc" "$ROOT/sys" \
         "$ROOT/var/lib/clickhouse" "$ROOT/var/log/clickhouse-server"
cp "$BIN" "$ROOT/clickhouse"
# a small dynamic binary of the same runtime — a control run (CH_BINARY=/echo)
docker run --rm ubuntu:22.04 cat /bin/echo > "$ROOT/echo"; chmod +x "$ROOT/echo"
# the glibc runtime comes from the Ubuntu 22.04 image (glibc 2.35)
docker run --rm ubuntu:22.04 tar -C / -cf - \
  lib64/ld-linux-x86-64.so.2 lib/x86_64-linux-gnu/ld-linux-x86-64.so.2 \
  lib/x86_64-linux-gnu/libc.so.6 lib/x86_64-linux-gnu/libm.so.6 \
  lib/x86_64-linux-gnu/libdl.so.2 lib/x86_64-linux-gnu/librt.so.1 \
  lib/x86_64-linux-gnu/libpthread.so.0 lib/x86_64-linux-gnu/libgcc_s.so.1 \
  | tar -C "$ROOT" -xf -

printf 'root:x:0:0:root:/:/bin/sh\nclickhouse:x:1000:1000:clickhouse:/var/lib/clickhouse:/bin/sh\n' > "$ROOT/etc/passwd"
printf 'root:x:0:\nclickhouse:x:1000:\n' > "$ROOT/etc/group"
printf '127.0.0.1 localhost talaria\n' > "$ROOT/etc/hosts"
printf 'hosts: files dns\n' > "$ROOT/etc/nsswitch.conf"
# Queries: by default a small set proving that not just "start-up" works but the
# engine: aggregation over 10 M rows, a MergeTree table, inserting and reading with
# a condition. Overridden by CH_QUERY (one line) or CH_QUERIES_FILE.
if [ -n "${CH_QUERIES_FILE:-}" ]; then cp "$CH_QUERIES_FILE" "$ROOT/q.sql"
elif [ -n "${CH_QUERY:-}" ]; then printf '%s\n' "$CH_QUERY" > "$ROOT/q.sql"
# MergeTree in the set is a real table on disk, not only counting in memory:
# insert, reading by parts and merging. NO comments in the query file:
# `--queries-file` parses it itself and trips over comments (the rest of the file
# is silently not executed, and the exit code stays zero).
else cat > "$ROOT/q.sql" <<'SQL'
SELECT version() AS v, 1 + 1 AS two;
SELECT count() AS n, sum(number) AS s, round(avg(number), 2) AS a FROM numbers(10000000);
SELECT number % 7 AS k, count() AS c FROM numbers(2000000) GROUP BY k ORDER BY k;
SELECT sum(x * x) FROM (SELECT toUInt64(number) AS x FROM numbers(5000000));
CREATE TABLE IF NOT EXISTS t (id UInt64, k UInt32, s String) ENGINE = MergeTree ORDER BY id;
TRUNCATE TABLE t;
INSERT INTO t SELECT number, number % 100, concat('s', toString(number % 1000)) FROM numbers(200000);
SELECT count(), sum(id), uniqExact(k) FROM t;
OPTIMIZE TABLE t FINAL;
SELECT count() FROM t WHERE id % 7 = 0;
SQL
fi

# Config: by default ClickHouse creates pools of HUNDREDS of threads (IO, background
# schedules, metrics). The kernel has 192 tasks for the whole machine, and the pool
# ceiling hit it before the query was reached. Here the pools are cut to the
# minimum — a limitation of the stand, not of the query.
#
# BUT not below 32 in the global pool. With 16, `CREATE TABLE ... ENGINE = MergeTree`
# hung forever on "Loading data parts": the part loader hands its work to the GLOBAL
# pool, which is already taken by the background executors — the task never starts,
# and all pool threads wait for it on one word. This was long taken for our bug; in
# fact the same config with the same binary hangs on Linux in exactly the same way —
# checked by a direct run. The pools can be cut, but not into a deadlock.
cat > "$ROOT/config.xml" <<'CFG'
<clickhouse>
    <logger><level>warning</level><console>true</console></logger>
    <path>/var/lib/clickhouse/</path>
    <tmp_path>/tmp/</tmp_path>
    <user_directories><users_xml><path>/users.xml</path></users_xml></user_directories>
    <max_thread_pool_size>64</max_thread_pool_size>
    <max_thread_pool_free_size>2</max_thread_pool_free_size>
    <max_io_thread_pool_size>4</max_io_thread_pool_size>
    <max_io_thread_pool_free_size>1</max_io_thread_pool_free_size>
    <background_pool_size>16</background_pool_size>
    <background_schedule_pool_size>2</background_schedule_pool_size>
    <background_common_pool_size>2</background_common_pool_size>
    <background_fetches_pool_size>1</background_fetches_pool_size>
    <background_move_pool_size>1</background_move_pool_size>
    <background_distributed_schedule_pool_size>1</background_distributed_schedule_pool_size>
    <background_message_broker_schedule_pool_size>1</background_message_broker_schedule_pool_size>
    <tables_loader_foreground_pool_size>8</tables_loader_foreground_pool_size>
    <tables_loader_background_pool_size>4</tables_loader_background_pool_size>
    <asynchronous_metrics_update_period_s>3600</asynchronous_metrics_update_period_s>
    <asynchronous_heavy_metrics_update_period_s>3600</asynchronous_heavy_metrics_update_period_s>
    <mark_cache_size>134217728</mark_cache_size>
    <uncompressed_cache_size>0</uncompressed_cache_size>
    <mlock_executable>false</mlock_executable>
    <merge_tree>
        <number_of_free_entries_in_pool_to_execute_mutation>2</number_of_free_entries_in_pool_to_execute_mutation>
        <number_of_free_entries_in_pool_to_lower_max_size_of_merge>2</number_of_free_entries_in_pool_to_lower_max_size_of_merge>
        <number_of_free_entries_in_pool_to_execute_optimize_entire_partition>2</number_of_free_entries_in_pool_to_execute_optimize_entire_partition>
    </merge_tree>
</clickhouse>
CFG
cat > "$ROOT/users.xml" <<'USR'
<clickhouse>
    <profiles><default>
        <max_threads>4</max_threads>
        <max_memory_usage>1000000000</max_memory_usage>
    </default></profiles>
    <users><default>
        <password></password>
        <profile>default</profile>
        <quota>default</quota>
        <access_management>1</access_management>
    </default></users>
    <quotas><default></default></quotas>
</clickhouse>
USR

cat > "$ROOT/cell.manifest" <<M
# ClickHouse on Talaria (the high-base kernel: build-kernel.sh)
binary=${CH_BINARY:-/clickhouse}
args=${CH_ARGV0:-clickhouse} ${CH_ARGS:-local --config-file=/config.xml --queries-file /q.sql}
env=HOME=/
env=PATH=/
env=TZ=UTC
# THE WORKLOAD ASKS FOR LARGE PAGES ITSELF. The hot spot of an analytics engine is a
# hash table of a few megabytes with random access, which does not fit into the TLB
# with 4 KB pages (and under a hypervisor a miss also walks the EPT). Measured:
# uniqExact 253 -> 197 ms, GROUP BY 100 k 235 -> 197 (Linux 188 and 236).
# This is NOT on by default for the machine — there it costs 1.5 µs of p50 on
# redis. It is declared here, not on the stand's command line: the command line
# belongs to the operator, the manifest travels with the workload.
huge=${CH_HUGE:-2}
# THE STEAL PUSH is a property of the workload too. The query executors fall asleep
# and wake up hundreds of times and drift into a 2+1+1+0 layout, and an empty core
# does not come to steal (its only task sleeps and does not enter the scheduler).
# On a tick the donor itself gives a neighbour the task waiting longer than 100 µs.
# Measured LIKE on a Memory table, max_threads=4: 0.083-0.165 -> 0.082-0.085 s per
# query (Linux ~0.084). NOT on by default for the machine: it separates a latency
# pair (actors pingpong so-simple) across cores, p50 3.1 -> 9.2 µs.
stealpush=${CH_STEALPUSH:-on}
${CH_PLAN:-}
# LD_LIBRARY_PATH is deliberately NOT set here: ClickHouse considers such variables
# harmful (checkHarmfulEnvironmentVariables) and RE-EXECUTES itself via execve after
# dropping them. The libraries are on the paths built into ld-linux anyway.
M
../../kernel/mkext2.sh "$ROOT" disk-ch.img ${1:-1500M}
rm -rf "$ROOT"
ls -la disk-ch.img
e2fsck -fn disk-ch.img >/dev/null 2>&1 && echo "disk-ch.img: ext2 ok"
