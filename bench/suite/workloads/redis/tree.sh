#!/bin/sh
# The guest tree for Redis 7.2 (shared by Talaria and Linux).
set -eu
OUT=${1:?directory}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../../.." && pwd)
[ -f "$REPO/services/redis/redis-server" ] || { echo "no services/redis/redis-server — see services/redis/README.md"; exit 1; }
mkdir -p "$OUT/etc"
cp "$REPO/services/redis/redis-server" "$OUT/redis-server"
# The suite's config, not the one in services/: logs and persistence are off —
# otherwise the speed of writing to the log and to a snapshot is measured, not the work with memory.
cat > "$OUT/redis.conf" <<'C'
port 6379
bind 0.0.0.0
protected-mode no
save ""
appendonly no
daemonize no
logfile ""
loglevel warning
maxmemory 256mb
maxmemory-policy allkeys-lru
C
printf 'root:x:0:0:root:/root:/bin/sh\n' > "$OUT/etc/passwd"
echo "the redis tree is ready: $OUT"
