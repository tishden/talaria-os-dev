#!/bin/sh
# The guest tree for MariaDB 10.11 (shared by Talaria and Linux).
#
# --skip-grant-tables: after mariadb-install-db root goes through unix_socket, while
# the test case goes over TCP. Permission checking is not the subject of the measurement and
# is switched off IDENTICALLY on both sides — as is everything else in this tree.
set -eu
OUT=${1:?directory}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../../.." && pwd)
SRC="$REPO/services/mariadb/build"
[ -x "$SRC/b/sql/mariadbd" ] || { echo "no mariadbd — run services/mariadb/build.sh"; exit 1; }
[ -d "$SRC/datadir" ] || { echo "no $SRC/datadir (mariadb-install-db)"; exit 1; }

mkdir -p "$OUT/opt/mariadb/bin" "$OUT/opt/mariadb/share" "$OUT/tmp" "$OUT/etc" "$OUT/run"
cp "$SRC/b/sql/mariadbd" "$OUT/opt/mariadb/bin/mariadbd"
strip "$OUT/opt/mariadb/bin/mariadbd" 2>/dev/null || true
cp -r "$SRC/b/sql/share/english" "$OUT/opt/mariadb/share/english"
cp -r "$SRC/b/sql/share/charsets" "$OUT/opt/mariadb/share/charsets" 2>/dev/null \
  || cp -r "$SRC/mariadb-10.11.9/sql/share/charsets" "$OUT/opt/mariadb/share/charsets"
cp -r "$SRC/datadir" "$OUT/data"
rm -f "$OUT/data"/*.pid
printf 'root:x:0:0:root:/root:/bin/sh\nmysql:x:27:27:mysql:/:/bin/false\n' > "$OUT/etc/passwd"
printf 'root:x:0:\nmysql:x:27:\n' > "$OUT/etc/group"

cat > "$OUT/etc/my.cnf" <<'C'
[mysqld]
basedir=/opt/mariadb
datadir=/data
tmpdir=/tmp
lc-messages-dir=/opt/mariadb/share
socket=
pid-file=/run/mariadbd.pid
bind-address=0.0.0.0
port=3306
skip-name-resolve
skip-grant-tables
# We do NOT set `user=`: MariaDB accepts that knob only from root, while Talaria
# gives the workload a non-zero uid — the server refused to start ("One can only
# use the --user switch if running as root") and died right after. On the Linux side
# the unprivileged uid is set by init from the spec, so both sides run the
# server the same way.
innodb-use-native-aio=0
innodb-flush-method=fsync
innodb-flush-log-at-trx-commit=2
innodb-buffer-pool-size=128M
innodb-log-file-size=32M
thread-handling=one-thread-per-connection
max-connections=64
C
echo "the mysql tree is ready: $OUT"
