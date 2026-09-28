#!/bin/sh
# The reference: THE SAME ClickHouse binary on Linux (Ubuntu 22.04 in docker), as many
# cores, the same query file and the same config — otherwise there is nothing to compare.
#   run-linux.sh [query file]     env: SMP=N (cores, default 2)
set -eu
cd "$(dirname "$0")"
BIN=$(ls build/clickhouse-common-static-*/usr/bin/clickhouse | head -1)
SMP=${SMP:-2}
Q=${1:-build/q-linux.sql}
if [ ! -f "$Q" ]; then
  mkdir -p build
  cat > build/q-linux.sql <<'SQL'
SELECT version() AS v, 1 + 1 AS two;
SELECT count() AS n, sum(number) AS s, round(avg(number), 2) AS a FROM numbers(10000000);
SELECT number % 7 AS k, count() AS c FROM numbers(2000000) GROUP BY k ORDER BY k;
SELECT sum(x * x) FROM (SELECT toUInt64(number) AS x FROM numbers(5000000));
SQL
  Q=build/q-linux.sql
fi
docker run --rm --cpuset-cpus="0-$((SMP - 1))" -v "$PWD":/w -w /w ubuntu:22.04 \
  "/w/$BIN" local --queries-file "/w/$Q" --send_logs_level=debug 2>&1 | grep -aE "Read [0-9]+ rows|^[0-9]" | tail -12
