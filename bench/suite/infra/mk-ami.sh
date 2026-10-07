#!/bin/sh
# Build a Talaria AMI for the stand: the workload's data image + the load generator +
# TWO manifests (server and generator) in one image.
#
#   mk-ami.sh <workload> [--local-only]
#   ARCH=arm64 mk-ami.sh <workload>   — a Graviton image: the arm64 tree (tree.sh), tlgen.a64,
#                                       the aarch64 UEFI stub with the kernel inside
#
# One image for both roles — because on a "Talaria against Talaria" stand both
# machines must boot from ONE image: otherwise two builds are compared rather than
# two roles. The role is chosen when the instance is launched: user-data passes
# `talaria.manifest=/cell.manifest` (the server) or `/loadgen.manifest`
# (the generator, brought up as a job receiver on :9100).
#
# Prints the line `AMI_ID=ami-...` — that is what goes into terraform.
set -eu
WL=${1:?workload (a directory in bench/suite/workloads)}
LOCAL_ONLY=${2:-}
HERE=$(cd "$(dirname "$0")" && pwd)
SUITE=$(cd "$HERE/.." && pwd)
REPO=$(cd "$SUITE/../.." && pwd)
WD="$SUITE/workloads/$WL"
[ -d "$WD" ] || { echo "no workload $WL"; exit 1; }
. "$WD/meta.env"
. "$SUITE/lib/arch.sh"
[ -x "$TLGEN_BIN" ] || { echo "no $TLGEN_BIN — run make -C bench/suite/tlgen$([ "$ARCH" = arm64 ] && echo ' a64')"; exit 1; }

OUT=${OUT:-$HOME/.talaria-lab/bench-$WL$([ "$ARCH" = arm64 ] && echo -a64).img}
mkdir -p "$(dirname "$OUT")"
TREE=$(mktemp -d)
trap 'rm -rf "$TREE"' EXIT

echo "== the workload tree $WL"
# A TREE THAT FAILED TO BUILD MUST STOP THE IMAGE. Through a pipe into `sed` its exit status
# was sed's: a worktree without `services/mariadb/build` gave an AMI with no server binary, and
# the stand spent an hour on a phase A whose every point was "connection did not come up"
# (the console said "proc: the binary was not found on the disk").
TLOG=$(mktemp)
if ! "$WD/tree.sh" "$TREE" > "$TLOG" 2>&1; then
    sed 's/^/   /' "$TLOG"; rm -f "$TLOG"
    echo "REFUSAL: the workload tree of $WL did not build"; exit 1
fi
sed 's/^/   /' "$TLOG"; rm -f "$TLOG"

cp "$TLGEN_BIN" "$TREE/tlgen"
cp "$WD/manifest" "$TREE/cell.manifest"
cat > "$TREE/loadgen.manifest" <<'M'
# The load generator's role: a job receiver on :9100.
# A job is a line of tlgen arguments over TCP, the answer is a JSON report.
binary=/tlgen
args=tlgen --serve 9100
env=HOME=/
M

# Spare room: the server needs somewhere to write (WAL, InnoDB, WiredTiger), while the data
# image on EC2 becomes the root volume and does not grow by itself.
SZ=$(( ${DISK_MB:-64} * 2 ))
[ "$SZ" -lt 128 ] && SZ=128
echo "== ext2 image ${SZ}M -> $OUT"
"$REPO/kernel/mkext2.sh" "$TREE" "$OUT" "${SZ}M" > "$OUT.log" 2>&1 || { sed 's/^/   /' "$OUT.log"; echo "REFUSAL: the ext2 image did not build"; exit 1; }
sed 's/^/   /' "$OUT.log"

if [ "$LOCAL_ONLY" = "--local-only" ]; then
    echo "== --local-only: not building an AMI, the data image is ready: $OUT"
    exit 0
fi

# WHICH KERNEL GOES INTO THE AMI is printed here and stays in mk-ami-<wl>.log:
# the suite `ec2all-20260905-133810` turned out to have been taken on THREE different kernels, and
# there was nothing to tie a point to a commit with. `KERNEL=` substitutes another kernel
# (an A/B on EC2); by default it is the current build.
KERN=${KERNEL:-$TL_KERNEL_DEFAULT}
[ -f "$KERN" ] || { echo "no kernel $KERN — build it first"; exit 1; }
if [ "$ARCH" = arm64 ]; then
    # On arm64 the kernel travels INSIDE the UEFI stub (tools/mk-ami.sh): KERNEL= cannot
    # substitute it, and a stub older than the kernel is an AMI with the previous kernel.
    [ -z "${KERNEL:-}" ] || { echo "REFUSAL: KERNEL= is x86-only (arm64's kernel is inside the stub)"; exit 1; }
    STUB=$REPO/kernel/uefi-stub/target/aarch64-unknown-uefi/release/talaria-uefi-stub.efi
    [ -f "$STUB" ] && [ ! "$STUB" -ot "$KERN" ] || { echo "REFUSAL: the aarch64 stub is missing or older than the kernel — sh kernel/uefi-stub/build.sh aarch64"; exit 1; }
fi
echo "== kernel: $KERN"
KSHA=$(sha256sum "$KERN" | cut -c1-16)
KTREE=$(git -C "$REPO" describe --always --dirty 2>/dev/null || echo '?')
echo "   sha256 $KSHA, built $(date -r "$KERN" +%Y-%m-%d\ %H:%M:%S), tree $KTREE"
# A DIRTY TREE DOES NOT GO INTO A SCOREBOARD. One scoreboard (`ec2all-20260911-213108`)
# was taken with the kernel `...-487-gaed6200-dirty`, while the A/B of the same day used two OTHER
# binaries; the conclusion "the same build moved by 20 %" rested on a false premise,
# and it could be checked only in mk-ami.log, where nobody looked.
# Hence two rules: the kernel stamp goes into stand.env (and from there into every results
# directory and into the summary's header), and a dirty tree requires an explicit ALLOW_DIRTY=1.
case $KTREE in *-dirty) [ "${ALLOW_DIRTY:-0}" = 1 ] || { echo "REFUSAL: the tree is dirty ($KTREE) — commit or set ALLOW_DIRTY=1"; exit 1; } ;; esac
printf 'STAND_KERNEL_SHA=%s\nSTAND_KERNEL_TREE=%s\nSTAND_KERNEL_BUILT=%s\n' "$KSHA" "$KTREE" "$(date -r "$KERN" +%Y-%m-%dT%H:%M:%S)" > "${STAND_DIR:-$HOME/.talaria-lab/stand}/kernel.stamp" 2>/dev/null || true
export KERN

echo "== AMI (image -> volume -> snapshot -> register-image)"
"$REPO/infra/aws/talaria-deploy.sh" --arch "$ARCH" \
    --data "$OUT" --mode proc --smp "${SMP:-4}" --net dhcp --userdata \
    --name "tlbench-$WL-$(date +%Y%m%d-%H%M%S)" --ami-only
