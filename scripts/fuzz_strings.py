"""Generate random f-/t-string concatenations and compare radon with oxradon.

Usage: python scripts/fuzz_strings.py RADON OXRADON [COUNT] [SEED]

Each generated function returns one implicitly concatenated string spread
over several lines. That stresses how CPython merges literal pieces and
positions format-spec nodes, which decides the reported `endline`.
"""

import json
import os
import random
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import is_error, run, usage  # noqa: E402

PLAIN = [
    '"abc"', '""', "'x y'", '"""a\nb"""', '""""""',
    # Escapes that decode to nothing, and raw counterparts where they do not.
    '"\\\n"', 'r"\\\n"', '"\\\n\\\n"', '"a\\\nb"',
]
F_PIECES = [
    'f"{x}"', 'f""', 'f"lit"', 'f"a{x}b"', 'f"{x}{y}"', 'f"{x:>10}"', 'f"{x:}"', 'f"{x!r:}"',
    'f"{x=}"', 'f"{x=:}"', 'f"{x:{y}}"', 'f"{x:{y}>3}"', 'f"{x!s}"', 'f"{{lit}}"',
    'f"""{x\n:}"""', 'f"""a\n{x}\nb"""', 'f"""{x:\n}"""', 'f"""{\nx\n}"""', 'f"""\n"""',
    # Literal elements that decode to nothing (backslash-newline), and raw counterparts.
    'f"\\\n"', 'f"\\\n{x}"', 'f"{x}\\\n"', 'rf"\\\n{x}"', 'f"{x:\\\n}"', 'f"{x:{y}\\\n}"',
    'f"""{x\n=}"""', 'f"""{\nx=}"""', 'f"{{\\\n}}"', 'f"}}{x}{{"',
    # A doubled brace after a nested field in a format spec is text to CPython (Ruff's lexer
    # starts a field there; see `rawtok::prepass`).
    'f"{x:{y}{{z or w}}}"', 'f"{x:{y}{{z}"', 'f"""{x:{y}\n{{z or w}}}\n"""', 'f"{x:{y}{{{z}}"',
]
T_PIECES = [p.replace('f"', 't"', 1) for p in F_PIECES]


def expression(rng):
    family = rng.choice(["f", "t"])
    pool = T_PIECES if family == "t" else F_PIECES + PLAIN
    pieces = [rng.choice(pool) for _ in range(rng.randint(1, 5))]
    if family == "f" and not any(p.startswith("f") for p in pieces):
        pieces.append(rng.choice(F_PIECES))
    return pieces[0] + "".join(rng.choice([" ", "\n            "]) + p for p in pieces[1:])


def key(value):
    """What the comparison looks at for one file's JSON entry."""
    if is_error(value):
        return "error"
    return [(blk["name"], blk["endline"], blk["complexity"]) for blk in value]


def main():
    if len(sys.argv) < 3:
        usage(__doc__)
    radon, rs = sys.argv[1], sys.argv[2]
    count = int(sys.argv[3]) if len(sys.argv) > 3 else 3000
    rng = random.Random(int(sys.argv[4]) if len(sys.argv) > 4 else 7)
    with tempfile.TemporaryDirectory() as tmp:
        files = []
        for i in range(count):
            path = Path(tmp, f"case_{i:05}.py")
            path.write_text(f"def f():\n    return ({expression(rng)})\n", encoding="utf-8")
            files.append(str(path))
        a, b = (json.loads(run([exe, "cc", "-j", tmp])) for exe in (radon, rs))
        differ = [f for f in files if key(a.get(f, [])) != key(b.get(f, []))]
        errors = sum(is_error(v) for v in a.values())
        print(f"{count} generated functions ({errors} rejected by CPython): {len(differ)} differ")
        for f in differ[:8]:
            print(f"--- {os.path.basename(f)}: radon={key(a.get(f, []))} rs={key(b.get(f, []))}")
            print(Path(f).read_text(encoding="utf-8"))
        sys.exit(1 if differ else 0)


if __name__ == "__main__":
    main()
