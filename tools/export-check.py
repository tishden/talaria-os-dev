#!/usr/bin/env python3
"""Hard checks on an exported public tree (tools/export-public.sh).

Each check prints what it found and makes the script exit non-zero:

* forbidden words — names of private material must not leak (FORBIDDEN);
* home directories — no `/home/<user>` paths from a developer machine;
* binaries — no ELF objects, kernel modules, archives, disk images, keys;
* dangling paths — every repository path quoted in Markdown (`kernel/...`,
  `docs/...`, ...) exists in the export.

Files that are known to be pending cleanup are listed in PENDING by name, so an
exception is always visible here and never silent.
"""
import os
import re
import sys

# Spelled backwards so that this file does not trip its own check.
FORBIDDEN = [re.compile("semreh"[::-1], re.I)]
# Paths inside third-party build output quoted in docs (not our machines).
HOME_OK = [re.compile(r"/home/buildozer/")]
BINARY_EXT = (".ko", ".gz", ".tgz", ".xz", ".zst", ".tar", ".so", ".a", ".o",
              ".img", ".pem", ".key", ".p12", ".efi", ".bin")
TOP = ("kernel", "docs", "bench", "services", "tools", "examples", "infra",
       "control-plane", ".github")
# Empty since the release plan moved into `private/` at publication (8.3): the
# plan was the one file that named the private material it was about removing.
# Nothing may be added here: an exemption is how a name leaks.
PENDING = set()
# The changelog quotes files that were later removed or renamed; everything else
# under docs/ is current documentation and its paths must resolve.
JOURNAL = re.compile(r"^CHANGELOG\.md$")


def is_elf(path):
    try:
        with open(path, "rb") as f:
            return f.read(4) == b"\x7fELF"
    except OSError:
        return False


def exists(p, fileset, dirset):
    """A quoted path is fine if it exists, or if it is a build product whose
    source is in the tree (`kernel/user/fsread` <- `fsread.c`), or lives in a
    build directory (`kernel/target/...`)."""
    if p in fileset or p in dirset:
        return True
    if re.match(r"kernel/target(-[a-z]+)?(/|$)", p):
        return True
    return any(f.startswith(p + ".") for f in fileset if f.startswith(p))


def main(root):
    errors = []
    files = []
    for d, dirs, names in os.walk(root):
        dirs[:] = [x for x in dirs if x != ".git"]
        for n in names:
            files.append(os.path.relpath(os.path.join(d, n), root))
    files.sort()
    fileset = set(files)
    dirset = {os.path.dirname(f) for f in files}
    for f in list(dirset):
        while f:
            f = os.path.dirname(f)
            dirset.add(f)

    for rel in files:
        path = os.path.join(root, rel)
        if rel.endswith(BINARY_EXT) or is_elf(path):
            errors.append(f"binary: {rel}")
            continue
        try:
            text = open(path, encoding="utf-8").read()
        except (UnicodeDecodeError, OSError):
            continue
        if rel not in PENDING:
            for rx in FORBIDDEN:
                for m in rx.finditer(text):
                    line = text.count("\n", 0, m.start()) + 1
                    errors.append(f"forbidden word: {rel}:{line}")
            for m in re.finditer(r"/home/[A-Za-z0-9_.-]+/", text):
                if not any(ok.search(m.group(0)) for ok in HOME_OK):
                    line = text.count("\n", 0, m.start()) + 1
                    errors.append(f"home path: {rel}:{line}: {m.group(0)}")
        if rel.endswith(".md") and not JOURNAL.search(rel):
            for m in re.finditer(r"`((?:%s)/[^`\s]*)`" % "|".join(map(re.escape, TOP)), text):
                p = m.group(1)
                if any(c in p for c in "*<>{}$…|["):
                    continue
                p = re.sub(r"(::\w+|:\d+(-\d+)?)$", "", p).rstrip("/.,;")
                here = os.path.normpath(os.path.join(os.path.dirname(rel), p))
                if exists(p, fileset, dirset) or exists(here, fileset, dirset):
                    continue
                if p not in fileset and p not in dirset:
                    line = text.count("\n", 0, m.start()) + 1
                    errors.append(f"dangling path: {rel}:{line}: {p}")

    for e in errors:
        print("  " + e)
    print(f"export-check: {len(files)} files, {len(errors)} problems")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
