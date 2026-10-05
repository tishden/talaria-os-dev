# Sourced: apkroot DIR RELEASE PKG... — the Alpine aarch64 packages with their closure unpacked
# into DIR by an x86 container's `apk --arch aarch64 --root --no-scripts` (nothing is executed),
# the symbolic links materialised (the kernel's ext2 does not follow them), the apk database
# dropped. The caller sets DL, the download cache (kernel/target/a64-services/dl by convention).
# Used by services/build-aarch64-db.sh and by bench/suite's arm64 workload trees.
apkroot() {
  dir=$1; rel=$2; shift 2
  : "${DL:?apkroot: DL (the download cache) is not set}"
  case $dir in /*) ;; *) dir=$PWD/$dir ;; esac   # docker -v takes absolute paths only
  rm -rf "$dir.raw" "$dir"; mkdir -p "$dir.raw" "$dir" "$DL/apk-$rel"
  docker run --rm -v "$dir.raw":/r -v "$DL":/dl alpine:3.20 sh -c "
    apk --arch aarch64 -X https://dl-cdn.alpinelinux.org/alpine/$rel/main -X https://dl-cdn.alpinelinux.org/alpine/$rel/community \
      --allow-untrusted --root /r --initdb --no-scripts --cache-dir /dl/apk-$rel add $* >/r/apk.log 2>&1; rc=\$?
    rm -rf /r/var/cache/apk /r/lib/apk /r/etc/apk; chown -R $(id -u):$(id -g) /r /dl/apk-$rel; exit \$rc" \
    || { echo "apkroot: apk failed for $* (a CDN hiccup is the usual cause — run it again):"; tail -3 "$dir.raw/apk.log"; return 1; }
  # -h: a link's target is copied under the link's name; a link to a directory, the tree
  (cd "$dir.raw" && tar -chf - --exclude=./dev --exclude=./proc --exclude=./sys . 2>/dev/null) | (cd "$dir" && tar -xf -)
  rm -rf "$dir.raw"
  rm -f "$dir/apk.log"
  [ -f "$dir/lib/ld-musl-aarch64.so.1" ]
}
