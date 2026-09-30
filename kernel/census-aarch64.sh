#!/bin/sh
# The arm64 port's progress meter (docs/ARM64-PLAN.md §10): copy the kernel to a scratch
# directory, drop EVERY `#[cfg(target_arch = "x86_64")]` gate in front of a `mod X;` line of
# main.rs, and count what `cargo check` for aarch64 says — the errors per file and per missing
# name. The count is what phase 3 drives to zero; the real tree keeps its gates until a
# module's whole dependency closure compiles, so the arm64 build in CI stays green meanwhile.
#   kernel/census-aarch64.sh            # the summary
#   kernel/census-aarch64.sh -v         # plus the full check log's path
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
W=${TMPDIR:-/tmp}/talaria-census-$$; mkdir -p "$W"
trap 'rm -rf "$W"' EXIT
(cd "$REPO" && tar --exclude=kernel/target -cf - kernel) | tar -xf - -C "$W"
python3 - "$W/kernel/src/main.rs" <<'PY'
import re, sys
p = sys.argv[1]; s = open(p).read()
s = re.sub(r'#\[cfg\(target_arch = "x86_64"\)\]\n(mod [a-z0-9_]+;)', r'\1', s)
# the stand-ins of `arch/aarch64/pending.rs` step aside: the real modules are what is counted
s = re.sub(r'#\[cfg\(target_arch = "aarch64"\)\]\nuse arch::pending::\{[^}]*\};\n', '', s)
open(p, 'w').write(s)
PY
cd "$W/kernel" || exit 1
# TAL_CENSUS_BUILD=1: a full build instead of a check — the linker's word (undefined symbols)
# once the type errors are gone.
CMD=check; [ "${TAL_CENSUS_BUILD:-0}" = 1 ] && CMD=build
CARGO_TARGET_DIR="${TAL_CENSUS_TARGET:-$REPO/kernel/target/census}" cargo -Z json-target-spec $CMD --release --target aarch64-talaria.json 2> "$W/check.log" >/dev/null
cp "$W/check.log" "$REPO/kernel/target/census-check.log" 2>/dev/null
GATES=$(grep -c 'cfg(target_arch = "x86_64")' "$REPO/kernel/src/main.rs")
ERR=$(grep -c '^error' "$W/check.log")
echo "gates in main.rs: $GATES; errors with every mod ungated: $ERR ($CMD)"
grep -oE 'undefined symbol: [^ ]+|undefined reference to [^ ]+' "$W/check.log" | sort | uniq -c | sort -rn | head -20 | sed 's/^/   /' 
echo "-- by file:"
grep -A1 '^error' "$W/check.log" | grep -oE 'src/[a-z0-9_/]+\.rs' | sort | uniq -c | sort -rn | head -25 | sed 's/^/   /'
echo "-- by missing name:"
grep -oE 'could not find `[a-z_0-9]+` in (`crate`|`arch`|the crate root|`[a-z_]+`)|cannot find (function|value|macro) `[a-z_0-9]+`( in module `[a-z:_0-9]+`)?|unresolved import `[a-z:_0-9]+`' "$W/check.log" | sort | uniq -c | sort -rn | head -40 | sed 's/^/   /'
[ "${1:-}" = -v ] && echo "the full log: kernel/target/census-check.log"
exit 0
