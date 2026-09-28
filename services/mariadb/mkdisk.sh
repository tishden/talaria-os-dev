#!/bin/sh
# MariaDB disk for Talaria (ext2 RW): /opt/mariadb/bin/mariadbd (static musl), share/english/errmsg.sys,
# /data (mariadb-install-db), /tmp, /etc/my.cnf, /cell.manifest. mkdisk.sh [size]
set -eu
cd "$(dirname "$0")"
ROOT=$(mktemp -d)
mkdir -p "$ROOT/opt/mariadb/bin" "$ROOT/opt/mariadb/share" "$ROOT/tmp" "$ROOT/etc" "$ROOT/run"
cp build/b/sql/mariadbd "$ROOT/opt/mariadb/bin/mariadbd"
strip "$ROOT/opt/mariadb/bin/mariadbd" 2>/dev/null || true
cp -r build/b/sql/share/english "$ROOT/opt/mariadb/share/english"
cp -r build/b/sql/share/charsets "$ROOT/opt/mariadb/share/charsets" 2>/dev/null || cp -r build/mariadb-10.11.9/sql/share/charsets "$ROOT/opt/mariadb/share/charsets"
cp -r build/datadir "$ROOT/data"
cat > "$ROOT/etc/my.cnf" <<M
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
innodb-use-native-aio=0
innodb-flush-method=fsync
innodb-buffer-pool-size=32M
innodb-log-file-size=8M
thread-handling=one-thread-per-connection
max-connections=20
M
cat > "$ROOT/cell.manifest" <<M
# MariaDB 10.11 on Talaria (process mode: threads, ext2 RW)
binary=/opt/mariadb/bin/mariadbd
args=mariadbd --defaults-file=/etc/my.cnf
env=HOME=/
env=PATH=/opt/mariadb/bin
M
../../kernel/mkext2.sh "$ROOT" disk-mdb.img ${1:-512M}
rm -rf "$ROOT"
ls -la disk-mdb.img; e2fsck -fn disk-mdb.img >/dev/null 2>&1 && echo "disk-mdb.img: ext2 ok"
