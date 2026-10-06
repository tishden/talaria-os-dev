#!/bin/sh
# The direct map's meter (docs/ARM64-PLAN.md §10.16, stage 1): how many integer<->pointer
# casts in the kernel still say nothing about WHAT the address is. Every such cast must go
# through a helper that names its kind (`mem::kptr`/`kpa_of` for RAM by its physical address,
# `mem::vptr`/`vaddr` for an address that is already virtual, `uaccess::uptr` for ring 3,
# `mem::devptr` for MMIO), because the direct map makes those kinds differ. The compiler
# counts: a scratch copy of the kernel gets `implicit_provenance_casts` (both directions) as a
# warning, and the meter prints the sites per file for both machines.
# The count reached 0 (2026-10-06) and the lints are denied in main.rs since: a new bare cast
# is a build error. The meter stays for a branch that turns the deny off to land in pieces.
#   kernel/census-dmap.sh            # the summary
#   kernel/census-dmap.sh -v         # plus every site (file:line), x86 then arm64
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
W=${TMPDIR:-/tmp}/talaria-dmap-$$; mkdir -p "$W"
trap 'rm -rf "$W"' EXIT
(cd "$REPO" && tar --exclude=kernel/target -cf - kernel) | tar -xf - -C "$W"
python3 - "$W/kernel/src/main.rs" <<'PY'
import re, sys
p = sys.argv[1]; s = open(p).read()
s = re.sub(r'#!\[(deny|warn)\((implicit|lossy)_provenance_casts\)\]\n', '', s)
feat = '' if '#![feature(strict_provenance_lints)]' in s else '#![feature(strict_provenance_lints)]\n'
s = s.replace('#![no_std]\n', '#![no_std]\n' + feat + '#![warn(implicit_provenance_casts)]\n', 1)
open(p, 'w').write(s)
PY
cd "$W/kernel" || exit 1
TOTAL=0
for t in x86_64 aarch64; do
  CARGO_TARGET_DIR="${TAL_DMAP_TARGET:-$REPO/kernel/target/census-dmap}" \
    cargo -Z json-target-spec check --release --target $t-talaria.json 2> "$W/$t.log" >/dev/null
  ERR=$(grep -c '^error' "$W/$t.log")
  # the sites: the line after the warning names the file (only the kernel's own src/)
  grep -A1 -E 'implicitly (relies on exposed|exposes pointer) provenance' "$W/$t.log" \
    | grep -oE -- '--> src/[a-z0-9_/]+\.rs:[0-9]+' | sed 's/^--> //' | sort -u > "$W/$t.sites"
  N=$(wc -l < "$W/$t.sites")
  TOTAL=$((TOTAL + N))
  echo "$t: $N bare casts ($ERR errors)"
  cut -d: -f1 "$W/$t.sites" | sort | uniq -c | sort -rn | head -30 | sed 's/^/   /'
  [ "${1:-}" = -v ] && sed 's/^/     /' "$W/$t.sites"
done
for t in x86_64 aarch64; do cp "$W/$t.log" "$REPO/kernel/target/dmap-$t.log" 2>/dev/null; done
echo "dmap census: $TOTAL"
exit 0
