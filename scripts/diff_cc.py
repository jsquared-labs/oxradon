"""Differential test: compare `radon cc` against `oxradon cc` on real code.

Usage: python scripts/diff_cc.py RADON OXRADON PATH [PATH ...]

Compares JSON output semantically (radon's key order is set-ordered, so it
varies run to run), then the terminal output byte for byte across flag combos.
Only error messages are normalized; line endings and exit codes are checked.
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import ERROR_PREFIX, is_error, run, usage  # noqa: E402

JSON_FLAGS = [[], ["--show-closures"], ["--no-assert"]]
TEXT_FLAGS = [
    ["-s", "-a"],
    ["-s", "--total-average", "-n", "C"],
    ["-s", "-o", "LINES", "--show-closures"],
    ["-o", "ALPHA"],
]


def normalize(data):
    """Drop error messages (CPython and Ruff word them differently)."""
    return {name: {"error": True} if is_error(value) else value for name, value in data.items()}


def radon_crashes(radon, paths):
    """Files where radon itself hits Python's recursion limit. oxradon analyzes
    these correctly, so they are an intentional difference, not a parity bug."""
    data = json.loads(run([radon, "cc", "-j", *paths]))
    return {
        name
        for name, value in data.items()
        if is_error(value) and "maximum recursion depth" in value["error"]
    }


def describe_diff(a, b):
    """Explain how one file's JSON entries differ."""
    if not (isinstance(a, list) and isinstance(b, list)):
        return [f"radon={str(a)[:120]} rs={str(b)[:120]}"]
    lines = []
    for x, y in zip(a, b):
        if x != y:
            for key in sorted(k for k in set(x) | set(y) if x.get(k) != y.get(k)):
                lines.append(f"{x.get('name')}: {key} radon={x.get(key)!r} rs={y.get(key)!r}")
            break
    if len(a) != len(b):
        lines.append(f"block count radon={len(a)} rs={len(b)}")
    return lines


def diff_json(radon, rs, paths, flags, skip):
    a = normalize(json.loads(run([radon, "cc", "-j", *flags, *paths])))
    b = normalize(json.loads(run([rs, "cc", "-j", *flags, *paths])))
    files = sorted((set(a) | set(b)) - skip)
    bad = [f for f in files if a.get(f) != b.get(f)]
    print(f"json {' '.join(flags) or '(default)'}: {len(files)} files, {len(bad)} differ")
    for f in bad[:10]:
        print(f"  {f}")
        if f not in a or f not in b:
            print(f"    only in {'radon' if f in a else 'oxradon'}")
        else:
            for line in describe_diff(a[f], b[f]):
                print(f"    {line}")
    return len(bad)


def sections(text, skip):
    """Split terminal output into (file sections in order, footer lines). Lines
    keep their endings, so a missing final newline or CRLF counts as a difference."""
    current = ("", [])
    out, footer = [current], []
    lines = text.splitlines(keepends=True)
    for i, line in enumerate(lines):
        if line == "\n" and i + 1 < len(lines) and "blocks (classes" in lines[i + 1]:
            footer = lines[i:]
            break
        if line.startswith("    "):
            current[1].append(ERROR_PREFIX + "\n" if line.startswith(ERROR_PREFIX) else line)
        else:
            current = (line, [])
            out.append(current)
    return [s for s in out if s[0].rstrip("\r\n") not in skip and (s[0] or s[1])], footer


def diff_text(radon, rs, paths, flags, skip):
    a, fa = sections(run([radon, "cc", *flags, *paths]), skip)
    b, fb = sections(run([rs, "cc", *flags, *paths]), skip)
    bad = [(x, y) for x, y in zip(a, b) if x != y]
    extra = abs(len(a) - len(b))
    # Skipped files change the averages, so footers are only comparable without them.
    footer_bad = int(not skip and fa != fb)
    lines = sum(1 + len(s[1]) for s in a)
    footer = "skipped" if skip else ("DIFFERS" if footer_bad else "same")
    print(f"text {' '.join(flags)}: {len(a)} files / {lines} lines, {len(bad)} differ, "
          f"count delta {extra}, footer {footer}")
    for x, y in bad[:5]:
        print(f"  {x[0]}\n    radon: {x[1][:3]}\n    rs:    {y[1][:3]}")
    if footer_bad:
        print(f"  footer radon={fa} rs={fb}")
    return len(bad) + extra + footer_bad


def main():
    if len(sys.argv) < 4:
        usage(__doc__)
    radon, rs, *paths = sys.argv[1:]
    skip = radon_crashes(radon, paths)
    if skip:
        print(f"skipping {len(skip)} file(s) where radon hits Python's recursion limit:")
        for f in sorted(skip):
            print(f"  {f}")
    failures = sum(diff_json(radon, rs, paths, flags, skip) for flags in JSON_FLAGS)
    failures += sum(diff_text(radon, rs, paths, flags, skip) for flags in TEXT_FLAGS)
    print("PASS" if not failures else f"FAIL ({failures} differences)")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
