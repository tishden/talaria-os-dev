#!/bin/sh
# A local run of one workload on both stands: the Talaria VM and the Linux VM under
# IDENTICAL QEMU/KVM discipline, with the load generator on the host.
#
#   run-local.sh <workload> [talaria|linux|both] [case label]
#   env: SMP= MEM= DUR= WARM= CONNS= RATES= REPEAT= OUTDIR=
#
# The local numbers are FOR DEBUGGING. The generator and the guest share the physical cores of one
# machine, the network goes through the host's tap, the guest lives under a hypervisor: it is good
# for "the workload came up, the protocol parsed, the report was assembled", not for
# publication. Numbers for publication are taken on ec2/metal (infra/).
#
# The order of work is two passes:
#   1) on each stand the CEILING is measured (a closed loop);
#   2) both sides walk THE SAME ladder of absolute rates, taken from
#      THE SMALLER of the ceilings.
# Otherwise each side would be measured at its own load fractions, and there would be nothing
# to compare: the runs would not share a single rate.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
cd "$REPO"
. "$HERE/lib/vm.sh"

WL=${1:?workload (a directory in workloads/)}
SIDES=${2:-both}
ONLY=${3:-}
WD="$HERE/workloads/$WL"
[ -d "$WD" ] || { echo "no workload $WL"; exit 1; }
# The workload passport sets DEFAULTS, not orders: what is named in the environment
# wins. Otherwise `SMP=1 run-local.sh postgresql` silently runs with two cores,
# because that is what meta.env says — and half a day goes into "why does it not match".
_E_SMP=${SMP:-}; _E_MEM=${MEM:-}; _E_DUR=${DUR:-}; _E_WARM=${WARM:-}
_E_CONNS=${CONNS:-}; _E_REPEAT=${REPEAT:-}; _E_BOOT=${BOOT_WAIT:-}
. "$WD/meta.env"

SMP=${_E_SMP:-${SMP:-1}};      MEM=${_E_MEM:-${MEM:-512}}
DUR=${_E_DUR:-${DUR:-10}};     WARM=${_E_WARM:-${WARM:-3}}
CONNS=${_E_CONNS:-${CONNS:-4}}; REPEAT=${_E_REPEAT:-${REPEAT:-1}}
BOOT_WAIT=${_E_BOOT:-${BOOT_WAIT:-60}}
STAMP=$(date +%Y%m%d-%H%M%S)
OUTDIR=${OUTDIR:-$HERE/results/$WL-$STAMP}
mkdir -p "$OUTDIR"
TLGEN="$HERE/tlgen/tlgen"
[ -x "$TLGEN" ] || { echo "no tlgen — run make -C bench/suite/tlgen"; exit 1; }
# THE GENERATOR GOES ON ITS OWN CORES (see the header of lib/vm.sh about VM_CPUSET/GEN_CPUSET).
# A wrapper rather than a taskset in every call: `"$TLGEN"` is called both here and from
# the workload passport's WARMUP_CMD, and a missed place would mean that the warm-up
# runs on the wrong cores while the measurement runs on the right ones. `exec` keeps the process name, so
# the cleanup of previous generators (`vm_kill_stale_gen`) still finds them.
if [ -n "${GEN_CPUSET:-}" ]; then
    printf '#!/bin/sh\nexec taskset -c %s %s "$@"\n' "$GEN_CPUSET" "$TLGEN" > "$OUTDIR/tlgen-pinned"
    chmod +x "$OUTDIR/tlgen-pinned"
    TLGEN="$OUTDIR/tlgen-pinned"
    echo "== the generator on cores $GEN_CPUSET, the guest on ${VM_CPUSET:-all}"
fi

case "$SIDES" in
    talaria) STANDS="talaria" ;;
    linux)   STANDS="linux" ;;
    *)       STANDS="talaria linux" ;;
esac

echo "== workload $WL, stands: $STANDS, results: $OUTDIR"

# Workloads with an EXTERNAL harness (KIND=external) are measured not by the generator but by their own
# runner: for ClickHouse the test case is query time, for the actors it is message delivery
# inside a process, for task it is its own ends with their own timestamps.
# Forcing them into a client-server ladder of rates would be a stretch; they
# bring the result itself to the common format (lib/mkresult.py).
if [ "${KIND:-}" = external ]; then
    for stand in $STANDS; do
        echo "-- stand $stand"
        "$WD/run.sh" "$stand" "$OUTDIR" "$ONLY" || echo "   the $stand run did not work"
    done
    python3 "$HERE/lib/report.py" "$OUTDIR" || true
    echo "== done: $OUTDIR"
    exit 0
fi

# THE STAND'S NOISE FLOOR — before any comparison. If the host's own jitter is
# larger than the effect under study, a number at a high percentile speaks about the host, and
# the report must say so rather than pass noise off as a result.
if [ "${FLOOR:-1}" = 1 ]; then
    "$HERE/lib/noisefloor.sh" "$OUTDIR/floor.json" 2>&1 | tail -1 | sed 's/^/-- floor: /'
fi

vm_tap_up >/dev/null
TREE=$(mktemp -d)
trap 'vm_down; rm -rf "$TREE"' EXIT INT TERM
"$WD/tree.sh" "$TREE" >/dev/null
echo "-- the tree is built"

IMG="$HERE/results/disk-$WL.img"
IR="$HERE/results/ir-$WL.gz"

vm_start() {   # $1 stand
    log="$HERE/results/serial-$1-$WL.log"
    if [ "$1" = talaria ]; then
        cp "$WD/manifest" "$TREE/cell.manifest"
        "$REPO/kernel/mkext2.sh" "$TREE" "$IMG" "${DISK_MB:-64}M" >/dev/null
        rm -f "$TREE/cell.manifest"
        vm_up_talaria "${IMG#$REPO/}" "${TAL_MODE:-proc}" "$SMP" "$MEM" "${log#$REPO/}" "${TALARIA_EXTRA:-}"
    else
        sed -e "s|@GUEST_IP@|$GUEST_IP|g" -e "s|@HOST_IP@|$HOST_IP|g" "$WD/spec" > "$TREE/.spec"
        # THE DATA DIRECTORY GOES ON THE SAME KIND OF DISK, not into memory (see vm_up_linux).
        # The `mount=` line comes FIRST: guest-init executes the spec in order,
        # and the passport's `chown=` must work on an ALREADY mounted volume.
        LDISK=; LMODS="${GUEST_MODULES:-virtio_net virtio_pci virtio_mmio}"
        if [ -n "${DATADIR:-}" ]; then
            # The volume IS BUILT FROM THE SAME DIRECTORY that goes into the Talaria image
            # (`mkfs.ext4 -d`): otherwise an empty volume would cover the already created database, and
            # the server would not come up at all.
            LDISK="$HERE/results/data-linux-$WL.img"
            rm -f "$LDISK"
            truncate -s "${DISK_MB:-64}M" "$LDISK"
            if mkfs.ext4 -q -F -d "$TREE$DATADIR" "$LDISK" >/dev/null 2>&1; then
                sed -i "1i mount=/dev/vda:$DATADIR:ext4" "$TREE/.spec"
                LMODS="$LMODS virtio_blk ext4"
            else
                echo "   WARNING: the volume for $DATADIR was not built — Linux will write INTO MEMORY (a comparison of media!)"
                LDISK=
            fi
        fi
        "$HERE/lib/mk-linux-guest.sh" "$TREE" "$TREE/.spec" "$IR" $LMODS >/dev/null
        rm -f "$TREE/.spec"
        vm_up_linux "${IR#$REPO/}" "$SMP" "$MEM" "${log#$REPO/}" "${LINUX_EXTRA:-}" "${LDISK:+${LDISK#$REPO/}}"
    fi
    if ! vm_wait_port "$GUEST_IP" "$PORT" "${BOOT_WAIT:-60}"; then
        echo "   guest $1 did not come up, the tail of the console:"; tail -15 "$log" 2>/dev/null | sed 's/^/     /'
        vm_down; return 1
    fi
    # Warming the workload up (filling the database, warming the caches) — before any measurements.
    export TLGEN GUEST_IP PORT PROTO
    [ -z "${WARMUP_CMD:-}" ] || sh -c "$WARMUP_CMD" || echo "   the warm-up did not work"
    return 0
}

# --------------------------------------------------------------- pass 1
CAPFILE="$OUTDIR/.caps"
: > "$CAPFILE"
if [ -z "${RATES:-}" ]; then
    for stand in $STANDS; do
        vm_start "$stand" || continue
        old_ifs=$IFS; IFS='|'
        for c in $CASES; do
            IFS=$old_ifs
            label=${c%%:*}; args=${c#*:}
            if [ -z "$ONLY" ] || [ "$ONLY" = "$label" ]; then
                # THE CEILING IS TAKEN AS A MEDIAN OF SEVERAL MEASUREMENTS, not as a single one.
                # A single four-second probe right after the guest comes up is
                # a measurement of the STATE (the block cache, the checkpointer, autovacuum), not of
                # the ceiling: `postgresql pk` gave 15937 / 12406 / 10174 / 11694 /
                # 11452 over different runs, a spread of 56 %. The whole ladder is built from the ceiling
                # (and the SMALLER of the two stands is taken), so one
                # unlucky probe shifted EVERY point of the workload — and produced
                # losses that do not exist in the system.
                probes=
                n=1
                while [ "$n" -le "${PROBE_N:-3}" ]; do
                    eval "\"$TLGEN\" --proto $PROTO --host $GUEST_IP --port $PORT $args \
                        --conns $CONNS --rate 0 --duration ${PROBE_DUR:-4} --warmup 1 --spin \
                        --out '$OUTDIR/.probe.json'" >/dev/null 2>&1 || true
                    v=$(python3 -c "
import json
try: print(int(json.load(open('$OUTDIR/.probe.json'))['throughput_rps']))
except Exception: print(0)")
                    probes="$probes $v"
                    n=$((n + 1))
                done
                cap=$(python3 -c "
import sys
v = sorted(int(x) for x in '$probes'.split())
print(v[len(v)//2] if v else 0)")
                echo "$label $stand $cap" >> "$CAPFILE"
                spread=$(python3 -c "
v = sorted(int(x) for x in '$probes'.split())
print('%d %%' % (0 if not v or v[-1] == 0 else (v[-1]-v[0])*100//max(v[-1],1)))")
                echo "   ceiling $stand/$label: $cap req/s (probes:$probes, spread $spread)"
            fi
            IFS='|'
        done
        IFS=$old_ifs
        vm_down
    done
fi

# --------------------------------------------------------------- pass 2
ladder_for() {   # $1 — the case label
    if [ -n "${RATES:-}" ]; then echo "$RATES"; return; fi
    python3 - "$CAPFILE" "$1" <<'PY'
import sys
caps = []
for line in open(sys.argv[1]):
    p = line.split()
    if len(p) == 3 and p[0] == sys.argv[2] and int(p[2]) > 0:
        caps.append(int(p[2]))
if not caps:
    print("")
else:
    # The ladder comes from the SMALLER ceiling: at a rate above it the weaker side
    # is certainly overloaded, and its percentiles would measure the generator's queue.
    cap = min(caps)
    print(" ".join(str(int(cap * f)) for f in (0.1, 0.25, 0.5, 0.75, 0.9)))
PY
}

for stand in $STANDS; do
    echo "-- stand $stand"
    vm_start "$stand" || continue
    old_ifs=$IFS; IFS='|'
    for c in $CASES; do
        IFS=$old_ifs
        label=${c%%:*}; args=${c#*:}
        if [ -z "$ONLY" ] || [ "$ONLY" = "$label" ]; then
            ladder=$(ladder_for "$label")
            if [ -z "$ladder" ]; then
                echo "   $label: the ladder is empty (the ceiling was not measured) — skipping"
            else
                echo "   $label: ladder $ladder"
                for rate in $ladder; do
                    r=1
                    while [ "$r" -le "$REPEAT" ]; do
                        m="$OUTDIR/metrics-$stand-$label-r$rate-$r.txt"
                        : > "$m"
                        "$HERE/lib/metrics.sh" "$stand" "$m" "before" 2>/dev/null || true
                        eval "\"$TLGEN\" --proto $PROTO --host $GUEST_IP --port $PORT $args \
                            --conns $CONNS --rate $rate --duration $DUR --warmup $WARM --spin \
                            --name '$WL/$label' --stand $stand \
                            --tags 'workload=$WL,case=$label,smp=$SMP,mem=$MEM,repeat=$r' \
                            --out '$OUTDIR/$stand-$label-r$rate-$r.json'" 2>&1 | tail -1 | sed "s/^/     $rate: /"
                        "$HERE/lib/metrics.sh" "$stand" "$m" "after" 2>/dev/null || true
                        r=$((r + 1))
                    done
                done
            fi
        fi
        IFS='|'
    done
    IFS=$old_ifs
    cp "$HERE/results/serial-$stand-$WL.log" "$OUTDIR/serial-$stand.log" 2>/dev/null || true
    vm_down
done

rm -f "$OUTDIR/.probe.json"
python3 "$HERE/lib/report.py" "$OUTDIR" || true
echo "== done: $OUTDIR"
