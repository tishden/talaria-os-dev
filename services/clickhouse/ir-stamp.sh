#!/bin/sh
# A stamp of the Linux guest's initramfs contents: what in it may DIVERGE from the
# Talaria image. That is the config, the users and the QUERY SET from the image
# (they are baked into the initramfs) plus the ClickHouse binary itself and init.
# If the stamp does not match the one stored next to a ready initramfs, KEEP_IR=1
# must rebuild it — otherwise the Linux guest runs the old q.sql under a new test case name.
#   ir-stamp.sh <disk-ch.img> <clickhouse binary>   -> 16 hex digits
set -eu
img=$1; bin=$2
{
  for f in config.xml users.xml q.sql; do
    printf '== %s\n' "$f"; debugfs -R "cat /$f" "$img" 2>/dev/null || true
  done
  printf '== bin %s\n' "$(stat -c '%s %Y' "$bin")"
  printf '== init %s\n' "$(sha256sum "$(dirname "$0")/linux-init.c" | cut -c1-16)"
} | sha256sum | cut -c1-16
