"""Regenerate the cc and hal recordings in tests/golden, and the CPython-only tables.

Usage, from the repository root, with the Python that radon is installed in:
    python scripts/update_golden.py path/to/radon

Records:
- manifest.json, cc-*.txt/json and hal-*.txt/json: `radon cc` over the fixtures and `radon hal`
  over the hal fixtures, per flag set (error messages replaced for cc, hal is byte-exact);
- fnmatch.json: Python's `fnmatch.fnmatchcase` on generated patterns and names;
- encodings.json: how `open(path, encoding=name)` resolves generated codec spellings, and
  cp1252's high bytes;
- shebang.json: radon's own `_is_python_file` on generated files around the 8 KiB read
  boundaries.

The raw and mi recordings have their own scripts (update_golden_raw.py, update_golden_mi.py).
The Rust tests replay these recordings against oxradon, so `cargo test`
needs neither Python nor radon.
"""

import codecs
import encodings.aliases
import fnmatch
import json
import os
import random
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import ERROR_PREFIX, is_error, run, usage  # noqa: E402

from radon.cli.tools import _is_python_file  # noqa: E402  (the oracle for `#!` checks)

ROOT = Path(__file__).resolve().parent.parent
# Every .py fixture, plus an extension-less script found through its shebang.
FIXTURES = sorted(str(p.relative_to(ROOT)) for p in (ROOT / "tests/fixtures").glob("*.py"))
FIXTURES.append("tests/fixtures/long_shebang")
GOLDEN = ROOT / "tests/golden"

CASES = {
    "json": ["-j"],
    "json-closures": ["-j", "--show-closures"],
    "json-no-assert": ["-j", "--no-assert"],
    "json-min-b": ["-j", "-n", "b"],
    "text": [],
    "text-scores-average": ["-s", "-a"],
    "text-total-average-range": ["-s", "--total-average", "-n", "B", "-x", "C"],
    "text-lines-closures": ["-s", "-o", "LINES", "--show-closures"],
    "text-alpha": ["-o", "alpha"],
}

# `hal` fixtures parse without errors, so these recordings are byte-exact
# (radon's `hal` key order is deterministic).
HAL_FIXTURES = sorted(str(p.relative_to(ROOT)) for p in (ROOT / "tests/fixtures/hal").glob("*.py"))
HAL_CASES = {
    "json": ["-j"],
    "text": [],
    "text-functions": ["-f"],
}

# Error messages are replaced by this marker (tests/golden.rs uses the same).
ERROR = "<error>"


def radon_cc(radon_bin, flags, is_json):
    out = run([radon_bin, "cc", *flags, *FIXTURES], cwd=ROOT)  # from the root: names are relative
    if is_json:
        data = json.loads(out)
        for value in data.values():
            if is_error(value):
                value["error"] = ERROR
        return json.dumps(data, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
    return "".join(
        f"{ERROR_PREFIX}{ERROR}\n" if line.startswith(ERROR_PREFIX) else line
        for line in out.splitlines(keepends=True)
    )


def fnmatch_cases():
    rng = random.Random(20261112)
    patterns = ["*", "*.py", ".*", "test_*", "[!_]*", "[a-c]?", "[]]", "[!]]", "[a-]", "[z-a]",
                "[!z-a]", "[c-ad-f]", "[\\]", "[^a]", "[", "a[", "*[!.]*", "**/x", "?"]
    alphabet = "ab-!]^[*?.\\/"
    patterns += ["".join(rng.choice(alphabet) for _ in range(rng.randint(1, 7))) for _ in range(400)]
    names = ["", "a", "b", "-", "!", "]", "^", "[", ".", "\\", "x.py", ".git", "test_a", "a/b/x", "\n"]
    names += ["".join(rng.choice("ab-!]^[.\\/") for _ in range(rng.randint(1, 4))) for _ in range(60)]
    return [[p, n, fnmatch.fnmatchcase(n, p)] for p in patterns for n in names]


def encoding_cases(probe):
    """What `open(path, encoding=name)` resolves each spelling to (as radon
    uses it for RADONFILESENCODING), and cp1252's high bytes."""
    bases = set(encodings.aliases.aliases) | set(encodings.aliases.aliases.values())
    spellings = {"locale", "Locale", "LOCALE", " locale", "locale ", "", " ", "-", ".",
                 "utf.8", "utf..8", ".utf8", "utf8.", "utf8sig", "windows1252", "cp-1252"}
    for name in bases:
        spellings |= {name, name.upper(), name.replace("_", "-"), name.replace("_", " "),
                      name.replace("_", "."), name.replace("_", "__"), f" {name} ", f"-{name}-"}
        for odd in ("\u212a", "\u0130", "\u00e9", "\u00a0"):
            middle = len(name) // 2
            spellings |= {odd + name, name + odd, name[:middle] + odd + name[middle:]}
    rng = random.Random(20261113)
    alphabet = "utf8-_. \u00e9\u212a\u0130latinscp1252"
    spellings |= {"".join(rng.choice(alphabet) for _ in range(rng.randint(1, 9))) for _ in range(400)}

    lookups = []
    for name in sorted(spellings):
        try:
            with open(probe, encoding=name) as fh:
                lookups.append([name, codecs.lookup(fh.encoding).name])
        except (LookupError, ValueError):
            lookups.append([name, None])
    cp1252 = []
    for byte in range(0x80, 0x100):
        try:
            cp1252.append(bytes([byte]).decode("cp1252"))
        except UnicodeDecodeError:
            cp1252.append(None)
    return {"lookups": lookups, "cp1252": cp1252}


def shebang_cases(tmp):
    """radon's own `_is_python_file` on real files around 8 KiB read boundaries.

    A file is a list of parts: a string is hex bytes, a number is a run of
    `x` bytes, so large cases stay small in JSON.
    """
    cases = []

    def add(label, *parts):
        data = b"".join(b"x" * p if isinstance(p, int) else p for p in parts)
        path = os.path.join(tmp, "script")
        Path(path).write_bytes(data)
        encoded = [p if isinstance(p, int) else p.hex() for p in parts]
        cases.append({"label": label, "parts": encoded, "radon": _is_python_file(path)})

    for raw in [b"", b"#", b"#!", b"#!python", b"#!python\r", b"#!python\n", b"#!python\r\n",
                b"#!python\r\xc3", b"#!python\n\xc3", b"#!python\r\n\xc3", b"#!python\r\r\xc3",
                b"#!python\n\xff", b"#!py\nthon", b"#\n!python", b"\xef\xbb\xbf#!python\n",
                b"#!/usr/bin/env python3\n", b"#!/bin/sh\npython\n", b"#!python\xe2\x82"]:
        add(f"literal {raw!r}", raw)

    terminators = [b"", b"\n", b"\r", b"\r\n"]
    suffixes = [b"", b"\xc3", b"\xff", b"abc", "\u20ac".encode()]
    for offset in [8189, 8190, 8191, 8192, 8193, 8194, 8200, 16383, 16384, 16385, 16386]:
        for term in terminators:
            for suffix in suffixes:
                tail = term + suffix
                name = f"term {term!r} at {offset}, then {suffix!r}"
                # `python` right after `#!`, so only decoding decides.
                add(f"{name}: python first", b"#!python", offset - 8, tail)
                # `python` after the line ends: never a Python shebang.
                add(f"{name}: python after", b"#!", offset - 2, tail + b"python")
                # `python` straddling the first 8 KiB boundary.
                if offset >= 8195:
                    fill = offset - 8195
                    add(f"{name}: python split", b"#!", 8187, b"python", fill, tail)
                # A multibyte character straddling the boundary before the line ends.
                if offset >= 8194:
                    fill = offset - 8194
                    add(f"{name}: euro split", b"#!python", 8183, "\u20ac".encode(), fill, tail)
    return cases


def main():
    if len(sys.argv) > 2:
        usage(__doc__)
    radon_bin = sys.argv[1] if len(sys.argv) > 1 else "radon"
    GOLDEN.mkdir(exist_ok=True)
    manifest = {"fixtures": FIXTURES, "cases": {}}
    for name, flags in CASES.items():
        is_json = "-j" in flags
        file = f"cc-{name}.{'json' if is_json else 'txt'}"
        (GOLDEN / file).write_bytes(radon_cc(radon_bin, flags, is_json).encode("utf-8"))
        manifest["cases"][name] = {"flags": flags, "format": "json" if is_json else "text", "file": file}
    manifest["hal_fixtures"] = HAL_FIXTURES
    manifest["hal_cases"] = {}
    for name, flags in HAL_CASES.items():
        file = f"hal-{name}.{'json' if '-j' in flags else 'txt'}"
        out = run([radon_bin, "hal", *flags, *HAL_FIXTURES], cwd=ROOT)
        (GOLDEN / file).write_bytes(out.encode("utf-8"))
        manifest["hal_cases"][name] = {"flags": flags, "file": file}
    (GOLDEN / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (GOLDEN / "fnmatch.json").write_text(json.dumps(fnmatch_cases()) + "\n")
    with tempfile.TemporaryDirectory() as tmp:
        probe = Path(tmp) / "probe.py"
        probe.write_text("x = 1\n")
        (GOLDEN / "encodings.json").write_text(json.dumps(encoding_cases(probe), indent=1) + "\n")
        shebang = shebang_cases(tmp)
    (GOLDEN / "shebang.json").write_text(json.dumps(shebang, indent=1) + "\n")
    print(f"wrote {len(CASES)} cc cases, {len(HAL_CASES)} hal cases, fnmatch, encodings and {len(shebang)} shebang cases "
          f"to {GOLDEN.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
