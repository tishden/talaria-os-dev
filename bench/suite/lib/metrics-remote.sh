#!/bin/sh
# A snapshot of the counters of the stand's remote server.
#   metrics-remote.sh <stand> <server address> <file> [label]
# Talaria — the kernel console over TCP (filed/ctl are not needed: the counters come from the
# kernel's statd port), Linux — statd.sh on :9999.
#
# A CAVEAT THAT USED TO BE MISSING HERE: on EC2 the Talaria path is NOT free.
# The counters come from serial, and the kernel can print there only periodically
# (`talaria.stat=`), and on Nitro every serial byte is an exit to the hypervisor with
# interrupts disabled. So by default printing is OFF
# (terraform, `talaria_stat_ms=0`), and a protocol run will have no Talaria
# counters. If they are needed for analysis — turn them on explicitly and understand that the
# tail of such a run is spoiled by its own diagnostics.
set -eu
STAND=${1:?stand}; SRV=${2:?address}; OUT=${3:?file}; TAG=${4:-}

# THE TALARIA SIDE: COUNTERS FROM SERIAL, NOT FROM TCP.
#
# Locally they are taken by the kernel console through QEMU's chardev socket, but on EC2
# there is no console at all: the only channel is serial, and it can be reached only with
# `aws ec2 get-console-output`. The kernel prints a `stat:` block there periodically
# (`talaria.stat=` in user-data, see terraform). Without this, a run on EC2
# would show WHAT but would not let one explain WHY, and the whole method of analysis
# rests on the counters.
if [ "$STAND" = talaria ]; then
    ID=${TALARIA_ID:-}
    {
        echo "### snapshot $STAND $TAG $(date -Is)"
        if [ -z "$ID" ]; then
            echo "the instance id is unknown (stand.env without talaria_server_id) — there will be no counters"
        elif ! command -v aws >/dev/null; then
            echo "no aws cli — the instance console is unavailable"
        else
            # The console's tail: the `stat:` block is printed every few seconds,
            # so the last lines are the fresh snapshot.
            # `--latest` IS MANDATORY: without it EC2 returns the console snapshot taken
            # at boot — the whole of ec2ab-mysql-20260906-131019 kept the tail of the
            # BOOT here (net: close of the first connections, timestamp 10:22),
            # identical "before" and "after" at every point. The gate is selfcheck.
            aws ec2 get-console-output --instance-id "$ID" --latest --output text 2>&1 \
                | tail -200 || echo "the instance console is unavailable"
        fi
        echo "### end of snapshot"
    } >> "$OUT"
    exit 0
fi
{
    echo "### snapshot $STAND $TAG $(date -Is)"
    # STATD_PORT is only for selfcheck (the same client against statd on its own port)
    python3 - "$SRV" "${STATD_PORT:-9999}" <<'PY' 2>&1 || echo "the snapshot is unavailable"
import socket, sys
# 30 s: statd computes the whole snapshot before the first byte (~7 s with the perfall sets).
s = socket.create_connection((sys.argv[1], int(sys.argv[2])), timeout=30)
s.settimeout(30)
buf = b""
while True:
    try:
        c = s.recv(65536)
    except socket.timeout:
        break
    if not c:
        break
    buf += c
sys.stdout.write(buf.decode("utf-8", "replace"))
PY
    echo "### end of snapshot"
} >> "$OUT"
