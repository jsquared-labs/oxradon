"""Regenerate tests/golden/raw-* from the real radon and from CPython itself.

Usage, from the repository root, with the Python that radon is installed in:
    python -I scripts/update_golden_raw.py path/to/radon

Records:
- raw-manifest.json and raw-*.txt/json: `radon raw` over every fixture, per flag set;
- raw-tokens.json: CPython's tokenizer on radon-style buffers (stripped lines joined by "\\n"),
  reduced to the token classes radon's `raw` looks at, or null where tokenizing fails;
- raw-lines.json: `str.splitlines()` then `str.strip()` on generated text;
- raw-percent.json: Python's `'{:.0%}'.format(x)` for floats given by their `repr`;
- decode-errors.json: `open(path, encoding=...).read()`'s error message (or null) for generated
  bytes, per codec oxradon supports (raw compares these messages exactly).

The Rust tests replay these, so `cargo test` needs neither Python nor radon.
"""

import io
import json
import os
import random
import sys
import tempfile
import tokenize
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _rawgen  # noqa: E402
from _common import record_config, run, usage  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
GOLDEN = ROOT / "tests/golden"
# cc's fixtures and the raw-only ones in tests/fixtures/raw.
FIXTURES = sorted(
    str(p.relative_to(ROOT))
    for p in [*(ROOT / "tests/fixtures").glob("*.py"), *(ROOT / "tests/fixtures/raw").glob("*.py")]
) + ["tests/fixtures/long_shebang"]

CASES = {
    "raw-text": ([], FIXTURES),
    "raw-text-summary": (["-s"], FIXTURES),
    "raw-json": (["-j"], FIXTURES),
    "raw-json-summary": (["-j", "-s"], FIXTURES),
    # A walked directory: its shebang `.ipynb` is found, opened and skipped.
    "raw-text-walk": ([], ["tests/fixtures/raw"]),
    # The notebook flags change nothing without `nbformat`.
    "raw-json-notebook-flags": (["-j", "--include-ipynb", "--ipynb-cells"],
                                ["tests/fixtures/raw/notebook.ipynb", "tests/fixtures/raw/percent.py"]),
    # Only failures: the totals fall back to radon's `or 1` guards.
    "raw-text-summary-failures": (["-s"], ["tests/fixtures/raw/unterminated.py",
                                           "tests/fixtures/nul_byte.py"]),
}


def token_classes(buffer):
    """The token stream as radon's `raw` sees it (see src/rawtok.rs), or None on TokenError."""
    try:
        tokens = list(tokenize.generate_tokens(io.StringIO(buffer).readline))
    except tokenize.TokenError:
        return None
    out = []
    for t in tokens:
        if t.type == tokenize.COMMENT:
            out.append("#")
        elif t.type in (tokenize.NL, tokenize.NEWLINE):
            out.append("n")
        elif t.type == tokenize.ENDMARKER:
            out.append("$")
        elif t.type == tokenize.STRING:
            out.append("S" if t.start[0] != t.end[0] else "s")
        elif (t.type, t.string) == (tokenize.OP, ":"):
            out.append(":")
        elif (t.type, t.string) == (tokenize.OP, ";"):
            out.append(";")
        else:
            out.append(".")
    return "".join(out)


CURATED_BUFFERS = [
    "", "x", "#", "# c", "'''a", "'''a\nb'''", "x = (", "x = (\n1)", "x = \\", "x = \\\n1",
    "x = \\\n", "\\", "\\\nx", "if x: pass", "if x:", "if x:  # c", "x = 1; y = 2", "x;;y", "x;",
    "a[1:]; b", "lambda: 1; y", "f'{x:}'", "f'{x:=5}'", "f'{{{x}'", "f'{x!r:>{w}}b'",
    "f'\\N{DASH}x'", "t'{x}'", "rf'{x}\\{'", "f'\\{x}'", "f'''{x:\n}'''", "f'{x:\n}'",
    "f'''{\nx # c\n}'''", "f'{a;b}'", "f'{'", "f'}'", "f'{x}}'", "f'{{'", "f'{x:{y:{z}}}'",
    "f'{x:{y:{z:{w}}}}'", "0b2", "0o9", "0x", "0x_1", "1_", "1__0", "1e", "1e+", "1if", "0777",
    "$", "?", "`", "a\x01b", "\x00", "\x7f", "\x1f", "é = 1", "\ufeff# c", "<>", ":=", "->",
    "'abc", "'abc\\\ndef'", "b'x'", "ub''", "bf''", "ft''", "ur''", "Rb''", "'''a'''", "\"\"\"\"\"\"",
    "'''a''''", "f'''a''''", "f'''{x}\n'''", "(" * 199 + ")" * 199, "(" * 200, "(" * 201,
    "f'" * 149, "f'{x" + "}'" * 1, "u'x'",
    # The nesting limits: 149 and 150 nested f-strings, 200 and 201 nested brackets, closed.
    "f'{" * 149 + "x" + "}'" * 149, "f'{" * 150 + "x" + "}'" * 150,
    "(" * 200 + ")" * 200, "(" * 201 + ")" * 201, "f''", "t''''''", "'a' 'b'", "('a')", "'a'  # c",
]


# A closing bracket in an f-string field (CPython 3.14.10 made one that mismatches the field's
# own `{` an error for `tokenize`; other mismatches, and closers with nothing open, still pass).
BRACKET_BUFFERS = [
    "f'{a)'", "f'{a]'", "f'{a)(}'", "f'{a](}'", "f'{a)[}'", "t'{a)(}'", "t'{a]'", "rf'{a)(}'",
    "f'{a)(b}'", "f'{a)(:x}'", "f'{a:{b)(}}'", "f'{a:{b](}}'", "f'{a!r)(}'", "f'{a=)(}'",
    "f'''{a)\n(}'''", "f'''{\na)\n(}'''", "t'''{a]\n\n[}'''", "x = f'{a)(}' ; y",
    # Mismatches that do not close the field's own `{`: allowed.
    "f'{(a]}'", "f'{[a)}'", "f'{(a}'", "f'{[a}'", "f'{ {a) }'", "f'{(a)](}'", "f'{a:{(b]}}'",
    "(x]", "[x)", "{x)", "(x}", "([x)]", "f'{a}' )", "(f'{a}']",
    # Closers with nothing open, and the field's `{` closed properly after them.
    ")", "]", "}", "x)", "x )\ny", "f'{a}')", ") f'{a)'", "]]f'{a](}'", "f'{a}' f'{b)'",
    # Nested f-strings: the inner field's `{` and the outer one's.
    "f'{f\"{a)\"}'", "f'{f\"{a}\")}'", "f'{f\"{(a]}\"}'", "f'{x:{f\"{a)\"}}}'",
]


def record_tokens(rng):
    buffers = list(CURATED_BUFFERS)
    seen = set(buffers)
    while len(buffers) < 4000:
        for b in _rawgen.buffers(rng, _rawgen.source(rng)):
            if b not in seen:
                seen.add(b)
                buffers.append(b)
    # After the generated buffers, so that adding one here changes none of those.
    buffers += [b for b in BRACKET_BUFFERS if b not in seen]
    return [[b, token_classes(b)] for b in buffers]


def record_lines(rng):
    texts = ["", "\n", "a\r\nb\n\rc\r", "x\n", " a \x1f\x1c b\x85c\u2028d\u2029e\x0bf\x0cg ",
             "\xa0\u3000x\u200b\ufeff"]
    texts += [_rawgen.source(rng) for _ in range(500)]
    return [[t, [ln.strip() for ln in t.splitlines()]] for t in texts]


def record_percent(rng):
    values = [0.0, 0.125, 0.375, 0.005, 0.015, 0.025, 1 / 3, 2 / 3, 1.0, 5.0, 0.995, 0.985,
              0.0049999999999999, 0.5, 1.5, 2.5, 100.0, 1e-9, 0.07, 0.29, 0.57]
    for _ in range(3000):
        den = rng.randrange(1, 2000)
        values.append(rng.randrange(0, 3 * den) / den)
    return [[repr(v), "{:.0%}".format(v)] for v in values]


DECODE_PIECES = [b"a", b"\n", b"\r\n", b"\xc3\xa9", b"\xe2\x82\xac", b"\xf0\x9f\x98\x80", b"\xe2\x82",
                 b"\xe2", b"\xf0\x9f", b"\xff", b"\x80", b"\xc0\xaf", b"\xed\xa0\x80", b"\xe0\x80",
                 b"\xf4\x90\x80\x80", b"\xef\xbb\xbf", b"\x81", b"\x9d", b"\x8d", b"\x93"]
CODECS = ["utf-8", "utf-8-sig", "ascii", "cp1252", "latin-1"]


def decode_message(tmp, codec, data):
    """`open(path, encoding=codec).read()`'s error message for `data`, or None."""
    tmp.write_bytes(data)
    try:
        with open(tmp, encoding=codec) as f:
            f.read()
    except UnicodeDecodeError as e:
        return str(e)
    return None


def record_decode(rng, tmp):
    # Whole files that are only part of a BOM read as empty under utf-8-sig.
    inputs = [(codec, data) for data in [b"", b"\xef", b"\xef\xbb", b"\xef\xbb\xbf", b"\xef\xbbx",
                                         b"\xef\xbf"] for codec in CODECS]
    for _ in range(1500):
        data = b"".join(rng.choice(DECODE_PIECES) for _ in range(rng.randrange(1, 7)))
        inputs.append((rng.choice(CODECS), data))
    return [[codec, data.hex(), decode_message(tmp, codec, data)] for codec, data in inputs]


# Configuration read by `raw` (exclude, ignore, output_file), one scratch project each, run from
# the project with HOME pointing at it (no ~/.radon.cfg).
CONFIG_CASES = {
    "exclude-ignore": {
        "radon.cfg": "[radon]\nexclude = */b*.py\nignore = skip\n",
        "a.py": "# a\nx = 1\n", "b1.py": "y = 2\n", "skip/c.py": "z = 3\n",
        "sub/d.py": '"""d"""\n',
    },
    "setup-cfg-flag-wins": {
        "setup.cfg": "[radon]\nexclude = *.py\n",
        "a.py": "x = 1; y = 2\n",
        "__args__": "raw -s -e nothing .",
    },
    "pyproject-output-file": {
        "pyproject.toml": '[tool.radon]\noutput_file = "report.txt"\n',
        "a.py": "if x: pass\n",
        "__read__": "report.txt",
    },
}


def main():
    if len(sys.argv) != 2:
        usage(__doc__)
    radon = sys.argv[1]
    manifest = {"cases": {}}
    for name, (flags, files) in CASES.items():
        is_json = "-j" in flags
        out_name = f"{name}.{'json' if is_json else 'txt'}"
        (GOLDEN / out_name).write_bytes(run([radon, "raw", *flags, *files], cwd=ROOT).encode("utf-8", "surrogateescape"))
        manifest["cases"][name] = {"flags": flags, "files": files, "file": out_name}
    manifest["config"] = record_config(radon, CONFIG_CASES, "raw .")
    (GOLDEN / "raw-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    rng = random.Random(20261113)
    for name, data in [("raw-tokens.json", record_tokens(rng)), ("raw-lines.json", record_lines(rng)),
                       ("raw-percent.json", record_percent(rng))]:
        (GOLDEN / name).write_text(json.dumps(data, ensure_ascii=True, indent=0) + "\n")
    with tempfile.TemporaryDirectory() as tmp:
        decode = record_decode(rng, Path(tmp) / "input.py")
    (GOLDEN / "decode-errors.json").write_text(json.dumps(decode, indent=0) + "\n")
    print(f"recorded {len(CASES)} raw cases and the tokenizer, line and percent tables")


if __name__ == "__main__":
    main()
