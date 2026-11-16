"""Differential test: compare `radon hal` against `oxradon hal` on real code.

Usage: python scripts/diff_hal.py RADON OXRADON PATH [PATH ...]

- JSON (`-j`, and `-j -f`, which radon prints identically): every file's entry,
  in order, with numbers compared as written (`0` is not `0.0`) and error
  messages dropped (CPython and Ruff word them differently); and the raw
  bytes, on every file radon analyzes.
- Terminal (default and `-f`): byte for byte, on every file radon analyzes.
  radon's terminal mode stops at the first file it cannot parse (a
  documented difference: oxradon reports it and continues), so those files
  are compared separately: oxradon must print `name:` and an `ERROR:` line.

Files where radon hits Python's recursion limit are skipped: oxradon analyzes
them (an intentional difference).
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import ERROR_PREFIX, chunks, is_error, run, usage  # noqa: E402


def load(text):
    """Parse `hal -j` output keeping key order and the exact number tokens."""
    return json.loads(
        text,
        object_pairs_hook=list,
        parse_int=lambda s: ("int", s),
        parse_float=lambda s: ("float", s),
    )


def masked(data):
    """Drop error messages: CPython and Ruff word them differently."""
    return [(name, {"error": True} if is_error(dict(value)) else value) for name, value in data]


def entries(radon_or_rs, flags, paths):
    return masked(load(run([radon_or_rs, "hal", "-j", *flags, *paths])))


def diff_json(radon, rs, paths, flags, skip, radon_entries=None):
    a = [e for e in radon_entries or entries(radon, flags, paths) if e[0] not in skip]
    b = [e for e in entries(rs, flags, paths) if e[0] not in skip]
    bad = [(x, y) for x, y in zip(a, b) if x != y]
    extra = abs(len(a) - len(b))
    print(f"json {' '.join(flags) or '(default)'}: {len(a)} files, {len(bad)} differ, "
          f"count delta {extra}")
    for x, y in bad[:10]:
        print(f"  {x[0]}" if x[0] == y[0] else f"  order: radon {x[0]}, rs {y[0]}")
        print(f"    radon: {str(x[1])[:300]}\n    rs:    {str(y[1])[:300]}")
    return len(bad) + extra


def diff_json_bytes(radon, rs, good):
    """Raw `-j` output, byte for byte (spacing, escaping, number spelling), on
    the files radon analyzes."""
    bad = [c[0] for c in chunks(good) if run([radon, "hal", "-j", *c]) != run([rs, "hal", "-j", *c])]
    print(f"json bytes: {len(good)} files, {len(bad)} chunk(s) differ")
    for first in bad[:5]:
        print(f"  chunk starting at {first}")
    return len(bad)


def diff_text(radon, rs, good, failed, flags):
    """Byte-exact on the files radon analyzes; the per-file error shape on the
    files it cannot parse."""
    bad = 0
    for chunk in chunks(good):
        a = run([radon, "hal", *flags, *chunk])
        b = run([rs, "hal", *flags, *chunk])
        if a != b:
            bad += 1
            al, bl = a.splitlines(keepends=True), b.splitlines(keepends=True)
            first = next((i for i, (x, y) in enumerate(zip(al, bl)) if x != y), min(len(al), len(bl)))
            header = next((al[i] for i in range(first, -1, -1) if i < len(al) and not al[i].startswith(" ")), "?")
            print(f"  near {header.strip()} line {first}:\n    radon: {al[first:first + 2]}\n"
                  f"    rs:    {bl[first:first + 2]}")
    for chunk in chunks(failed):
        b = run([rs, "hal", *flags, *chunk]).splitlines(keepends=True)
        expected = [line for name in chunk for line in (f"{name}:\n", ERROR_PREFIX)]
        actual = [line if not line.startswith(ERROR_PREFIX) else ERROR_PREFIX for line in b]
        if actual != expected:
            bad += 1
            print(f"  per-file errors differ in a chunk starting at {chunk[0]}")
    print(f"text {' '.join(flags) or '(default)'}: {len(good)} files byte-exact, "
          f"{len(failed)} per-file errors: {bad} chunk(s) differ")
    return bad


def main():
    if len(sys.argv) < 4:
        usage(__doc__)
    radon, rs, *paths = sys.argv[1:]
    raw = load(run([radon, "hal", "-j", *paths]))
    listing = masked(raw)
    skip = {name for name, value in raw
            if is_error(dict(value)) and "maximum recursion depth" in dict(value)["error"]}
    if skip:
        print(f"skipping {len(skip)} file(s) where radon hits Python's recursion limit:")
        for f in sorted(skip):
            print(f"  {f}")
    failed = [name for name, value in listing if value == {"error": True} and name not in skip]
    good = [name for name, value in listing if value != {"error": True}]
    print(f"{len(listing)} files: {len(good)} analyzed, {len(failed)} errors, {len(skip)} skipped")
    failures = diff_json(radon, rs, paths, [], skip, listing)
    failures += diff_json(radon, rs, paths, ["-f"], skip)
    failures += diff_json_bytes(radon, rs, good)
    failures += sum(diff_text(radon, rs, good, failed, flags) for flags in ([], ["-f"]))
    print("PASS" if not failures else f"FAIL ({failures} differences)")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
