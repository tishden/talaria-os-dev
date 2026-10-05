# Sourced: which machine the stand is for. ARCH=x86_64 (the default) or ARCH=arm64 (a
# Graviton stand: both sides on c7g, Talaria from the aarch64 UEFI stub, Linux on AL2023
# arm64 — docs/ARM64-PLAN.md phase 9). One switch for the whole EC2 path: the workload
# trees (tree.sh), the generator binary, the AMI, terraform and the Linux tuning.
#
# What it sets: ARCH (normalised, exported), TLGEN_BIN (the generator for that machine),
# A64 (the arm64 service binaries' directory, services/build-aarch64*.sh), DL (their
# download cache, for services/apkroot.sh).
_arch_repo=$(cd "$(dirname "$0")" && git rev-parse --show-toplevel 2>/dev/null || echo "${REPO:-.}")
case "${ARCH:-x86_64}" in
  arm64|aarch64) ARCH=arm64 ;;
  x86_64|amd64)  ARCH=x86_64 ;;
  *) echo "ARCH: x86_64 or arm64, not $ARCH" >&2; exit 1 ;;
esac
export ARCH
A64=${A64:-$_arch_repo/kernel/target/a64-services}
DL=${DL:-$A64/dl}
if [ "$ARCH" = arm64 ]; then
  TLGEN_BIN=$_arch_repo/bench/suite/tlgen/tlgen.a64
  TL_KERNEL_DEFAULT=$_arch_repo/kernel/target/aarch64-talaria/release/talaria
  TL_TYPE_DEFAULT=c7g.xlarge
else
  TLGEN_BIN=$_arch_repo/bench/suite/tlgen/tlgen
  TL_KERNEL_DEFAULT=$_arch_repo/kernel/target/x86_64-talaria/release/talaria
  TL_TYPE_DEFAULT=c6i.xlarge
fi
# a64_need FILE HOW: an arm64 service binary is there, or the command that builds it is said.
a64_need() { [ -f "$1" ] || { echo "no $1 — run $2"; exit 1; }; }
