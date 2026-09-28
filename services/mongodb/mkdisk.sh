#!/bin/sh
# MongoDB 4.0 disk (Alpine 3.9, the musl-dynamic mongod + shared objects) for Talaria: the root from build/root
# (/usr/bin/mongod, /lib/ld-musl-x86_64.so.1, /usr/lib/*.so), /data/db, /tmp, /cell.manifest. mkdisk.sh [size]
set -eu
cd "$(dirname "$0")"
ROOT=$(mktemp -d)
cp -r build/root/. "$ROOT/"
# PLAN-SYNC stage 3 (picked up automatically, layer 2): the patched ld-musl 1.2.5 (the
# umwait tier in libc's futex paths; on this stand it falls back to the futex, which
# sleeps in mwait in the Talaria kernel). Turn off: TAL_MUSL=0 ./mkdisk.sh
if [ "${TAL_MUSL:-1}" = 1 ] && [ -f ../../kernel/user/dyn/ld-musl-x86_64.so.1 ]; then
  cp ../../kernel/user/dyn/ld-musl-x86_64.so.1 "$ROOT/lib/ld-musl-x86_64.so.1"
  echo "mkdisk: ld-musl replaced with the patched one (PLAN-SYNC; TAL_MUSL=0 to turn off)"
fi
mkdir -p "$ROOT/data/db" "$ROOT/tmp" "$ROOT/etc" "$ROOT/proc" "$ROOT/sys"
printf 'root:x:0:0:root:/:/bin/sh\nmongodb:x:1000:1000:mongodb:/data:/bin/sh\n' > "$ROOT/etc/passwd"
printf 'root:x:0:\nmongodb:x:1000:\n' > "$ROOT/etc/group"
printf 'hosts: files dns\n' > "$ROOT/etc/nsswitch.conf"
printf '127.0.0.1 localhost talaria\n' > "$ROOT/etc/hosts"
cat > "$ROOT/cell.manifest" <<M
# MongoDB 4.0.5 (Alpine musl, dynamic ELF) on Talaria
binary=/usr/bin/mongod
args=mongod --dbpath /data/db --bind_ip_all --port 27017 --wiredTigerCacheSizeGB 0.25 --setParameter diagnosticDataCollectionEnabled=false --setParameter disableLogicalSessionCacheRefresh=true --nounixsocket
env=HOME=/
env=LD_LIBRARY_PATH=/usr/lib:/lib
env=PATH=/usr/bin:/bin
M
../../kernel/mkext2.sh "$ROOT" disk-mongo.img ${1:-512M}
rm -rf "$ROOT"
ls -la disk-mongo.img; e2fsck -fn disk-mongo.img >/dev/null 2>&1 && echo "disk-mongo.img: ext2 ok"
