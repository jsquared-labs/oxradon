"""Differential test: compare `radon raw` against `oxradon raw` on real code.

Usage: python -I scripts/diff_raw.py RADON OXRADON PATH [PATH ...]

raw's output has a fixed order (JSON fields come from a namedtuple) and its error messages come
from radon and CPython themselves, so everything is compared byte for byte, with no normalization:
the terminal listing with and without `-s`, and the JSON with and without `-s`, plus exit status.

radon takes minutes on the corpus; with RADON_CACHE=DIR its outputs are kept there, keyed by the
command, and reused (delete the directory after changing radon or the corpus).
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import run_radon, run_status, usage  # noqa: E402

FLAG_SETS = [[], ["-s"], ["-j"], ["-j", "-s"]]


def file_sections(text):
    """Terminal output as (header line, body lines) per file, endings kept."""
    out = []
    for line in text.splitlines(keepends=True):
        if line.startswith("    ") and out:
            out[-1][1].append(line)
        else:
            out.append((line, []))
    return out


def explain(flags, a, b):
    if "-j" in flags:
        try:
            ja, jb = json.loads(a), json.loads(b)
        except ValueError as e:
            return [f"invalid JSON: {e}"]
        names = [n for n in dict.fromkeys([*ja, *jb]) if ja.get(n) != jb.get(n)]
        lines = [f"{n}\n    radon={ja.get(n)}\n    rs=   {jb.get(n)}" for n in names[:10]]
        if not names:
            lines.append("same values, different bytes (key order or formatting)")
        return [f"{len(names)} files differ", *lines]
    sa, sb = file_sections(a), file_sections(b)
    bad = [(x, y) for x, y in zip(sa, sb) if x != y]
    lines = [f"{len(bad)} sections differ, section count radon={len(sa)} rs={len(sb)}"]
    for x, y in bad[:5]:
        lines.append(f"{x[0].rstrip()}\n    radon: {x[1]}\n    rs:    {y[1]}")
    return lines


def main():
    if len(sys.argv) < 4:
        usage(__doc__)
    radon, rs, *paths = sys.argv[1:]
    failures = 0
    for flags in FLAG_SETS:
        status_a, a = run_radon([radon, "raw", *flags, *paths])
        status_b, b = run_status([rs, "raw", *flags, *paths])
        label = " ".join(flags) or "(default)"
        # Analyzed files in the listing (the `** Total **` block has a `LOC:` line too).
        listed = a.count("\n    LOC: ") - ("-s" in flags)
        files = len(json.loads(a)) if "-j" in flags and status_a == 0 else listed
        same = a == b and status_a == status_b
        print(f"raw {label}: {files} files, {len(a)} bytes, "
              f"{'same' if same else 'DIFFERENT'} (status radon={status_a} rs={status_b})")
        if not same:
            failures += 1
            for line in explain(flags, a, b):
                print(f"  {line}")
    print("PASS" if not failures else f"FAIL ({failures} flag sets differ)")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
