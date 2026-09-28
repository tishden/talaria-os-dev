#!/bin/sh
# Export the PUBLIC Talaria OS tree: a clean copy of HEAD without `private/`
# and without build artefacts, then hard checks (tools/export-check.py) and a
# secret scan. Every check fails the export; nothing is left for "manual
# review". The public repository starts from fresh history:
#   tools/export-public.sh <outdir> && cd <outdir> && git init && git add . && git commit
set -eu
OUT=${1:?usage: tools/export-public.sh <outdir>}
REPO=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$OUT"
[ -z "$(ls -A "$OUT")" ] || { echo "export: $OUT is not empty"; exit 1; }
git -C "$REPO" archive HEAD | tar -x -C "$OUT"

echo "== private overlay"
rm -rf "$OUT/private"

echo "== build artefacts (the public tree builds them from source)"
# Test binaries of kernel/user and anything else that is an ELF: `make -C
# kernel/user all` rebuilds the former, the rest has its own build script.
find "$OUT" -type f | while read -r f; do
  case "$(file -b "$f")" in ELF*) rm -f "$f";; esac
done
find "$OUT" -type f -name '*.img' -delete

echo "== files a fresh 'git add .' would silently drop"
# The public repository starts from `git init && git add .`, which obeys the
# .gitignore that travels with the tree. A file that the export copied but
# .gitignore matches would vanish from the release without a word — that is how
# `bench/suite/infra/ansible/files/statd.sh` (source next to build products,
# force-added in the private tree) nearly did not travel.
# Asked of the EXPORTED .gitignore (not the private one) and with --no-index,
# because check-ignore stays silent about a path the private index tracks —
# and being force-added there is exactly what hides the problem.
TMPGIT=$(mktemp -d)
git init -q "$TMPGIT"
DROPPED=$( (cd "$OUT" && find . -path ./.git -prune -o -type f -print) \
  | sed 's|^\./||' \
  | git --git-dir="$TMPGIT/.git" --work-tree="$OUT" -C "$OUT" \
        check-ignore --no-index --stdin 2>/dev/null || true )
rm -rf "$TMPGIT"
if [ -n "$DROPPED" ]; then
  echo "$DROPPED" | sed 's/^/  ignored by .gitignore: /'
  echo "export: these files are in the tree but a fresh commit would drop them"
  exit 1
fi

echo "== checks"
python3 "$REPO/tools/export-check.py" "$OUT"

echo "== secret scan (gitleaks)"
if command -v gitleaks >/dev/null 2>&1; then
  gitleaks detect --no-git --redact -s "$OUT"
else
  docker run --rm -v "$OUT":/src:ro zricethezav/gitleaks:latest detect --no-git --redact -s /src
fi

echo "OK: public tree in $OUT ($(du -sh "$OUT" | cut -f1))"
echo "next: make -C $OUT/kernel/user all && (cd $OUT/kernel && python3 mkfs.py && cargo -Z json-target-spec build --release)"
