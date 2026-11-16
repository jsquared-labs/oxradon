"""Differential test: compare `radon mi` against `oxradon mi` on real code.

Usage: python -I scripts/diff_mi.py RADON OXRADON PATH [PATH ...]

For each flag set, stdout and the exit status are compared byte for byte, except syntax-error
messages (`... (<unknown>, line N)`), which CPython and Ruff word differently (a COMPATIBILITY.md row): those
are masked on both sides. raw's own `SyntaxError at line: N` and decode errors are compared as
they are.

Files are passed explicitly, in chunks (to stay under ARG_MAX), in the order radon walks PATHs.
Files where radon hits Python's recursion limit are skipped: oxradon analyzes them (a COMPATIBILITY.md row).
`--sort` with any error prints only `ERROR: 'mi'` (radon's crash, copied), so `-s --sort` also
runs on the files radon analyzes without error.

radon takes minutes on the corpus; with RADON_CACHE=DIR its outputs are kept there, keyed by the
command, and reused (delete the directory after changing radon or the corpus).
"""

import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import ERROR_PREFIX, chunks, run_radon, run_status, usage  # noqa: E402

FLAG_SETS = [[], ["-s"], ["-j"], ["-m", "-s"], ["-s", "--sort"], ["-n", "B", "-x", "C", "-s"],
             ["-j", "-m"]]
SYNTAX = re.compile(r"\(<unknown>, line \d+\)$")
JSON_ERROR = re.compile(r'"error": "((?:[^"\\]|\\.)*)"')


def mask(text):
    """Mask syntax-error messages; return the text and how many were masked."""
    count = 0

    def json_sub(m):
        nonlocal count
        if SYNTAX.search(json.loads(f'"{m.group(1)}"')):
            count += 1
            return '"error": "<syntax error>"'
        return m.group(0)

    if text.startswith("{"):
        return JSON_ERROR.sub(json_sub, text), count
    lines = []
    for line in text.splitlines(keepends=True):
        if line.startswith(ERROR_PREFIX) and SYNTAX.search(line.rstrip("\n")):
            count += 1
            line = f"{ERROR_PREFIX}<syntax error>\n"
        lines.append(line)
    return "".join(lines), count


def compare(radon, rs, flags, names, label):
    """Compare every chunk; return the number of chunks that differ."""
    bad = masked = worded = 0
    for chunk in chunks(names):
        status_a, a = run_radon([radon, "mi", *flags, *chunk])
        status_b, b = run_status([rs, "mi", *flags, *chunk])
        (ma, na), (mb, _) = mask(a), mask(b)
        masked += na
        worded += a != b
        if ma != mb or status_a != status_b:
            bad += 1
            al, bl = ma.splitlines(keepends=True), mb.splitlines(keepends=True)
            first = next((i for i, (x, y) in enumerate(zip(al, bl)) if x != y),
                         min(len(al), len(bl)))
            print(f"  chunk at {chunk[0]}: status radon={status_a} rs={status_b}, "
                  f"line {first}:\n    radon: {al[first:first + 2]!r:.300}\n"
                  f"    rs:    {bl[first:first + 2]!r:.300}")
    print(f"mi {label}: {len(names)} files, {masked} syntax errors masked, "
          f"{worded} chunk(s) differ only in their wording, {bad} chunk(s) differ")
    return bad


def main():
    if len(sys.argv) < 4:
        usage(__doc__)
    radon, rs, *paths = sys.argv[1:]
    status, listing = run_radon([radon, "mi", "-j", *paths])
    if status != 0:
        sys.exit(f"radon mi -j exited {status}")
    entries = json.loads(listing, object_pairs_hook=list)
    skip = {name for name, value in entries
            if dict(value).get("error", "").startswith("maximum recursion depth")}
    names = [name for name, _ in entries if name not in skip]
    good = [name for name, value in entries if "error" not in dict(value)]
    if skip:
        print(f"skipping {len(skip)} file(s) where radon hits Python's recursion limit:")
        for name in sorted(skip):
            print(f"  {name}")
    print(f"{len(names)} files: {len(good)} analyzed, {len(names) - len(good)} errors")
    failures = 0
    for flags in FLAG_SETS:
        failures += compare(radon, rs, flags, names, " ".join(flags) or "(default)")
    failures += compare(radon, rs, ["-s", "--sort"], good, "-s --sort (analyzed files only)")
    print("PASS" if not failures else f"FAIL ({failures} chunks differ)")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
