"""Regenerate tests/golden/mi-* from the real radon and from CPython itself.

Usage, from the repository root, with the Python that radon is installed in:
    python -I scripts/update_golden_mi.py path/to/radon

Records:
- mi-manifest.json and mi-*.txt/json: `radon mi` over the fixtures, per flag set, and a few
  configuration cases (one scratch project each);
- mi-compute.json: `radon.metrics.mi_compute` on generated inputs (results by `repr`);
- mi-format.json: Python's `' ({0:.2f})'.format(x)` for floats given by their `repr`;
- mi-params.json: `radon.metrics.mi_parameters` and `mi_visit` on generated valid modules and on
  the fixtures, with multi-line strings counted and not.

The Rust tests replay these, so `cargo test` needs neither Python nor radon.

The recordings are made on macOS. `mi_compute` calls the C library's `sin` and `log`, whose last
bit differs between systems, so radon's own results differ there too. On Linux (glibc), run

    python -I scripts/update_golden_mi.py --linux-overlay [DIR]

to write mi-compute.linux.json and mi-params.linux.json into DIR (default: tests/golden): the
entries of the two tables, by index, whose recorded result differs on this system. The inputs
are read from the recorded tables, not generated again. The tests use the overlay on Linux.
On musl (Alpine) and on an Intel Mac, the same command's files are renamed *.musl.json and
*.macos-x86_64.json; .github/workflows/record-libm.yml does that, on radon run there.
"""

import json
import math
import os
import random
import subprocess
import sys
from pathlib import Path

from radon.metrics import mi_compute, mi_parameters, mi_visit

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import RADON_ENV as ENV, record_config, run, usage  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
GOLDEN = ROOT / "tests/golden"
FIXTURES = sorted(
    str(p.relative_to(ROOT))
    for d in ["", "hal", "raw", "mi"]
    for p in (ROOT / "tests/fixtures" / d).glob("*.py")
) + ["tests/fixtures/long_shebang"]
MI_FIXTURES = sorted(str(p.relative_to(ROOT)) for p in (ROOT / "tests/fixtures/mi").glob("*.py"))
# The mi fixtures radon reports an error for.
FAILING = {"ff_in_string.py", "ls_parse_error.py", "ls_raw_error.py", "raw_error{{y}}.py"}
ANALYZED = [f for f in MI_FIXTURES if Path(f).name not in FAILING]

CASES = {
    "mi-text": ([], FIXTURES),
    "mi-text-show": (["-s"], FIXTURES),
    "mi-json": (["-j"], FIXTURES),
    "mi-text-no-multi": (["-m", "-s"], FIXTURES),
    "mi-json-no-multi": (["-j", "-m"], FIXTURES),
    "mi-text-range": (["-n", "B", "-x", "C", "-s"], FIXTURES),
    "mi-text-range-lower": (["-n", "b", "-x", "b", "-s"], MI_FIXTURES),
    "mi-json-range": (["-j", "-n", "C"], MI_FIXTURES),
    "mi-text-sort": (["-s", "--sort"], ANALYZED),
    # A listed error: radon's sort raises KeyError('mi') and prints only that.
    "mi-text-sort-error": (["-s", "--sort"], MI_FIXTURES),
    # An error outside the range is still listed, so the sort still crashes.
    "mi-text-sort-error-range": (["--sort", "-n", "C"], MI_FIXTURES),
    # JSON ignores --sort.
    "mi-json-sort": (["-j", "--sort"], MI_FIXTURES),
    # A walked directory, and repeated names (JSON: first position, last value).
    "mi-text-walk": (["-s"], ["tests/fixtures/mi"]),
    "mi-json-repeated": (["-j"], ["tests/fixtures/mi/rank_b.py", "tests/fixtures/mi/rank_c.py",
                                  "tests/fixtures/mi/rank_b.py"]),
    "mi-json-notebook-flags": (["-j", "--include-ipynb", "--ipynb-cells"],
                               ["tests/fixtures/raw/notebook.ipynb", "tests/fixtures/mi/rank_b.py"]),
    # A repeated flag is the flag once (mando's store_false / store_true).
    "mi-text-repeated-flags": (["-m", "-m", "-s", "-s"], MI_FIXTURES),
    # Rank bounds compared as strings, after Python's `upper` (`ß` is `SS`).
    "mi-text-min-empty": (["-n", "", "-s"], MI_FIXTURES),
    "mi-text-max-empty": (["-x", ""], MI_FIXTURES),
    "mi-text-min-sharp-s": (["-n", "\u00df"], MI_FIXTURES),
    "mi-text-max-sharp-s": (["-x", "\u00df"], MI_FIXTURES),
    "mi-json-empty": (["-j", "-n", "D"], ANALYZED),
    "mi-text-empty": (["-n", "D"], ANALYZED),
    # MI's colors: A green, B yellow, C red (cc's are different).
    "mi-text-color": (["-s"], MI_FIXTURES, {"COLOR": "yes"}),
    "mi-text-sort-color": (["-s", "--sort"], ANALYZED, {"COLOR": "yes"}),
}

# Configuration read by `mi`, one scratch project each, run from the project with HOME there.
# A module where counting multi-line strings as comments changes the index.
DOC = (ROOT / "tests/fixtures/mi/docstrings.py").read_text()
CONFIG_CASES = {
    "multi-absent": {"a.py": DOC, "__args__": "mi -s ."},
    "multi-absent-flag": {"a.py": DOC, "__args__": "mi -s -m ."},
    "multi-false": {"setup.cfg": "[radon]\nmulti = false\n", "a.py": DOC, "__args__": "mi -s ."},
    "multi-false-flag": {"setup.cfg": "[radon]\nmulti = false\n", "a.py": DOC,
                         "__args__": "mi -s -m ."},
    "multi-true-flag": {"radon.cfg": "[radon]\nmulti = yes\n", "a.py": DOC,
                        "__args__": "mi -s -m ."},
    "show-mi": {"radon.cfg": "[radon]\nshow_mi = on\n", "a.py": DOC, "__args__": "mi ."},
    "show-mi-flag": {"radon.cfg": "[radon]\nshow_mi = on\n", "a.py": DOC, "__args__": "mi -s ."},
    "mi-min-max": {"pyproject.toml": '[tool.radon]\nmi_min = "b"\nmi_max = "c"\n',
                   "a.py": DOC, "b.py": (ROOT / "tests/fixtures/mi/rank_b.py").read_text(),
                   "__args__": "mi -s ."},
    "mi-min-flag-wins": {"setup.cfg": "[radon]\nmi_min = C\n", "a.py": DOC,
                         "__args__": "mi -n A ."},
    "cc-min-not-read": {"setup.cfg": "[radon]\ncc_min = C\n", "a.py": DOC, "__args__": "mi ."},
    "output-file": {"setup.cfg": "[radon]\noutput_file = report.txt\nexclude = b*.py\n",
                    "a.py": DOC, "b.py": "x = 1\n", "__args__": "mi -s .",
                    "__read__": "report.txt"},
}


def record_stdin(radon):
    """`mi -j -` (and `-m`) on fixtures with CRLF and lone-CR line endings: radon reads stdin
    without translating newlines."""
    cases = []
    for fixture in ["docstrings.py", "module_level.py", "rank_b.py"]:
        text = (ROOT / "tests/fixtures/mi" / fixture).read_text()
        for ending in ["\r\n", "\r"]:
            data = text.replace("\n", ending).encode()
            for flags in [[], ["-m"]]:
                proc = subprocess.run([radon, "mi", "-j", *flags, "-"], check=True, input=data,
                                      capture_output=True, env=ENV, cwd=ROOT)
                cases.append({"stdin": data.decode(), "flags": flags, "stdout": proc.stdout.decode()})
    return cases


def num(x):
    """A Python number as [type, repr]: `0` and `0.0` differ."""
    return ["int" if isinstance(x, int) else "float", repr(x)]


def record_compute(rng):
    cases = [(0, 1, 5, 0), (0.0, 1, 5, 0.0), (10.0, 1, 0, 0), (1e-300, 0, 1, 0.0),
             (1.0, 0, 1, 0.0), (1e6, 500, 20000, 0.0), (1e9, 10**6, 10**7, 100.0)]
    for _ in range(4000):
        volume = rng.choice([0, 0.0]) if rng.random() < 0.02 else math.exp(rng.uniform(-2, 16))
        complexity = rng.randrange(1, rng.choice([5, 50, 500, 5000]))
        lloc = rng.randrange(0, rng.choice([3, 30, 300, 3000]))
        sloc = rng.randrange(1, 4000)
        comments = rng.choice([0, 0.0]) if rng.random() < 0.1 else rng.randrange(0, 2 * sloc) / sloc * 100
        cases.append((volume, complexity, lloc, comments))
    return [[num(v), c, s, num(k), repr(mi_compute(v, c, s, k))] for v, c, s, k in cases]


def record_format(rng):
    values = [0.0, 100.0, 0.125, 0.375, 2.675, 1.005, 63.125, 99.995, 0.005, 19.0, 9.0, 44.101081126220535]
    values += [rng.uniform(0, 100) for _ in range(3000)]
    values += [rng.randrange(0, 10001) / 100 for _ in range(1000)]
    values += [rng.randrange(0, 80001) / 800 for _ in range(1000)]
    return [[repr(v), " ({0:.2f})".format(v)] for v in values]


def expr(rng, depth=0):
    atoms = ["a", "b", "1", "2.5", "'s'", "None", "True", "x.y", "f(a)", "[1, 2]", "{'k': a}"]
    if depth > 2 or rng.random() < 0.4:
        return rng.choice(atoms)
    form = rng.randrange(8)
    left, right = expr(rng, depth + 1), expr(rng, depth + 1)
    return [
        f"{left} + {right}", f"{left} and {right}", f"{left} or {right} or a",
        f"({left} if {right} else b)", f"[q for q in {left} if q if {right}]",
        f"(lambda q: q * {left})", f"not {left}", f"{left} < {right} <= 3",
    ][form]


def block(rng, indent, depth):
    pad = "    " * indent
    lines = []
    for _ in range(rng.randrange(1, 4)):
        kind = rng.randrange(14 if depth < 3 else 5)
        if kind == 0:
            lines.append(f"{pad}x = {expr(rng)}")
        elif kind == 1:
            lines.append(f"{pad}# a comment")
            lines.append(f"{pad}y = {expr(rng)}  # inline")
        elif kind == 2:
            lines.append(f'{pad}"""a string\n{pad}over lines"""')
        elif kind == 3:
            lines.append(f"{pad}assert {expr(rng)}")
        elif kind == 4:
            lines.append(f"{pad}pass")
        elif kind == 5:
            lines.append(f"{pad}if {expr(rng)}:")
            lines += block(rng, indent + 1, depth + 1)
            if rng.random() < 0.5:
                lines.append(f"{pad}elif {expr(rng)}:")
                lines += block(rng, indent + 1, depth + 1)
            if rng.random() < 0.5:
                lines.append(f"{pad}else:")
                lines += block(rng, indent + 1, depth + 1)
        elif kind == 6:
            lines.append(f"{pad}for i in {expr(rng)}:")
            lines += block(rng, indent + 1, depth + 1)
            if rng.random() < 0.3:
                lines.append(f"{pad}else:")
                lines += block(rng, indent + 1, depth + 1)
        elif kind == 7:
            lines.append(f"{pad}while {expr(rng)}:")
            lines += block(rng, indent + 1, depth + 1)
        elif kind == 8:
            lines.append(f"{pad}try:")
            lines += block(rng, indent + 1, depth + 1)
            for _ in range(rng.randrange(1, 3)):
                lines.append(f"{pad}except{rng.choice(['', ' ValueError', '* TypeError'])}:")
                lines += block(rng, indent + 1, depth + 1)
            if rng.random() < 0.3:
                lines.append(f"{pad}else:")
                lines += block(rng, indent + 1, depth + 1)
        elif kind in (9, 10):
            deco = f"{pad}@decorator({expr(rng)})\n" if rng.random() < 0.3 else ""
            lines.append(f"{deco}{pad}{rng.choice(['def', 'async def'])} fn{rng.randrange(9)}(a, b={expr(rng)}):")
            lines += block(rng, indent + 1, depth + 1)
        elif kind == 11:
            lines.append(f"{pad}class K{rng.randrange(9)}:")
            lines += block(rng, indent + 1, depth + 1)
        elif kind == 12:
            lines.append(f"{pad}match {expr(rng)}:")
            for case in rng.sample(["[a]", "{'k': b}", "1 | 2", "_", "q", "str()"], rng.randrange(1, 4)):
                lines.append(f"{pad}    case {case}:")
                lines += block(rng, indent + 2, depth + 1)
        else:
            lines.append(f"{pad}with {expr(rng)} as w:")
            lines += block(rng, indent + 1, depth + 1)
    return lines


def module(rng):
    return "\n".join(block(rng, 0, 0)) + rng.choice(["\n", "\n", ""])


def params(text, multi):
    try:
        volume, complexity, lloc, comments = mi_parameters(text, multi)
        return {"volume": num(volume), "complexity": complexity, "lloc": lloc,
                "comments": num(comments), "mi": repr(mi_visit(text, multi))}
    except Exception:  # noqa: BLE001 - radon's harvester reports any exception as the file's error
        return None


def record_params(rng):
    texts = []
    for fixture in FIXTURES:
        try:
            with open(ROOT / fixture) as f:  # as radon reads files (universal newlines)
                texts.append(f.read())
        except (UnicodeDecodeError, OSError):
            pass
    while len(texts) < 2000:
        text = module(rng)
        try:
            compile(text, "<gen>", "exec", flags=0x400, dont_inherit=True)  # PyCF_ONLY_AST
        except SyntaxError:
            continue
        texts.append(text)
    return [[text, multi, params(text, multi)] for text in texts for multi in (True, False)]


def unnum(pair):
    """The Python number recorded by `num`."""
    kind, text = pair
    return int(text) if kind == "int" else float(text)


def record_overlay(out_dir):
    """Write the entries of mi-compute.json and mi-params.json that differ on this system."""
    compute = json.loads((GOLDEN / "mi-compute.json").read_text())
    differing = {}
    for i, (volume, complexity, lloc, comments, recorded) in enumerate(compute):
        here = repr(mi_compute(unnum(volume), complexity, lloc, unnum(comments)))
        if here != recorded:
            differing[str(i)] = here
    (out_dir / "mi-compute.linux.json").write_text(json.dumps(differing, indent=0) + "\n")
    print(f"mi-compute: {len(differing)} of {len(compute)} entries differ")
    table = json.loads((GOLDEN / "mi-params.json").read_text())
    differing = {}
    for i, (text, multi, recorded) in enumerate(table):
        here = params(text, multi)
        if here != recorded:
            differing[str(i)] = here
    (out_dir / "mi-params.linux.json").write_text(json.dumps(differing, indent=0) + "\n")
    print(f"mi-params: {len(differing)} of {len(table)} entries differ")


def main():
    if len(sys.argv) in (2, 3) and sys.argv[1] == "--linux-overlay":
        record_overlay(Path(sys.argv[2]) if len(sys.argv) == 3 else GOLDEN)
        return
    if len(sys.argv) != 2:
        usage(__doc__)
    radon = sys.argv[1]
    manifest = {"cases": {}}
    for name, (flags, files, *env) in CASES.items():
        env = env[0] if env else {}
        is_json = "-j" in flags
        out_name = f"{name}.{'json' if is_json else 'txt'}"
        out = run([radon, "mi", *flags, *files], {**ENV, **env}, cwd=ROOT)
        (GOLDEN / out_name).write_bytes(out.encode("utf-8", "surrogateescape"))
        manifest["cases"][name] = {"flags": flags, "files": files, "env": env, "file": out_name}
    manifest["config"] = record_config(radon, CONFIG_CASES)
    manifest["stdin"] = record_stdin(radon)
    (GOLDEN / "mi-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    rng = random.Random(20261113)
    for name, data in [("mi-compute.json", record_compute(rng)), ("mi-format.json", record_format(rng)),
                       ("mi-params.json", record_params(rng))]:
        (GOLDEN / name).write_text(json.dumps(data, ensure_ascii=True, indent=0) + "\n")
    print(f"recorded {len(CASES)} mi cases, {len(CONFIG_CASES)} config cases and the tables")


if __name__ == "__main__":
    main()
