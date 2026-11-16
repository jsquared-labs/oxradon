"""Check that oxradon accepts a character in a name exactly when CPython does.

Usage: python scripts/check_identifiers.py OXRADON

CPython (this script's interpreter, which must be 3.14) is the oracle, and radon is not
needed. Ruff's identifier tables follow a newer Unicode version than CPython 3.14's 16.0, so
oxradon checks names itself; this is the check that it draws the line where CPython does.

Every non-ASCII code point is tried as the first character of a name and as a later one:
- the ones CPython accepts go into one file per position, which oxradon must accept (if it
  rejects one, its error names the line, which is dropped and the file tried again);
- the ones CPython rejects each need a file of their own. All assigned characters outside the
  private-use areas are tried, and samples of the unassigned and private-use ones.
"""

import ast
import json
import os
import random
import re
import sys
import tempfile
import unicodedata
import warnings

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import is_error, run, usage  # noqa: E402

# A name with the character first, and one with it second.
POSITIONS = {"first": "{c}_ = 1\n", "later": "a{c} = 1\n"}
# How many of the characters CPython rejects are tried, where there are too many to try all.
UNASSIGNED_SAMPLE = 3000
PRIVATE_USE_SAMPLE = 500


def accepted(source):
    """Whether `ast.parse` accepts the source, as radon would find out."""
    try:
        ast.parse(source)
    except Exception:
        return False
    return True


def rejected_lines(rs, path, lines):
    """Write `lines` to `path` and return the ones oxradon rejects, one retry per rejection."""
    rejected = []
    while len(rejected) < 200:
        with open(path, "w", encoding="utf-8", newline="") as fh:
            fh.write("".join(text for _, text in lines))
        entry = json.loads(run([rs, "cc", "-j", path])).get(path)
        if not is_error(entry):
            break
        named = re.search(r"line (\d+)\)$", entry["error"])
        if not named:
            sys.exit(f"an error that names no line: {entry['error']}")
        rejected.append((lines.pop(int(named.group(1)) - 1)[0], entry["error"]))
    return rejected


def describe(code_point):
    char = chr(code_point)
    return f"U+{code_point:04X} {unicodedata.category(char)} {unicodedata.name(char, '?')}"


def main():
    if len(sys.argv) != 2:
        usage(__doc__)
    rs = sys.argv[1]
    warnings.simplefilter("ignore")
    random.seed(1)
    points = [cp for cp in range(0x80, 0x110000) if not 0xD800 <= cp <= 0xDFFF]
    differences = 0
    for position, template in POSITIONS.items():
        ok = [cp for cp in points if accepted(template.format(c=chr(cp)))]
        bad = sorted(set(points) - set(ok))
        category = {cp: unicodedata.category(chr(cp)) for cp in bad}
        assigned = [cp for cp in bad if category[cp] not in ("Cn", "Co")]
        unassigned = [cp for cp in bad if category[cp] == "Cn"]
        private = [cp for cp in bad if category[cp] == "Co"]
        tried = (assigned + random.sample(unassigned, UNASSIGNED_SAMPLE)
                 + random.sample(private, PRIVATE_USE_SAMPLE))
        with tempfile.TemporaryDirectory() as tmp:
            lines = [(cp, template.format(c=chr(cp))) for cp in ok]
            too_strict = rejected_lines(rs, os.path.join(tmp, "accepted.py"), lines)
            directory = os.path.join(tmp, "rejected")
            os.mkdir(directory)
            names = {}
            for cp in tried:
                name = os.path.join(directory, f"u{cp:06x}.py")
                with open(name, "w", encoding="utf-8", newline="") as fh:
                    fh.write(template.format(c=chr(cp)))
                names[name] = cp
            result = json.loads(run([rs, "cc", "-j", directory]))
        too_lax = [cp for name, cp in names.items() if not is_error(result.get(name))]
        print(f"{position} character: CPython accepts {len(ok)} and rejects {len(bad)} "
              f"({len(assigned)} assigned, {len(unassigned)} unassigned, "
              f"{len(private)} private use)")
        print(f"  accepted by CPython, rejected by oxradon: {len(too_strict)}")
        for cp, error in too_strict[:15]:
            print(f"    {describe(cp)}: {error}")
        print(f"  rejected by CPython, accepted by oxradon: {len(too_lax)} of {len(tried)} tried")
        for cp in too_lax[:15]:
            print(f"    {describe(cp)}")
        differences += len(too_strict) + len(too_lax)
    print(f"{differences} differ")
    sys.exit(1 if differences else 0)


if __name__ == "__main__":
    main()
