"""Compare oxradon's port of CPython's lexer with CPython's own, on generated text.

Usage: python scripts/check_lexer.py RADON OXRADON [COUNT] [SEED] [--record FILE]

Run it with the oracle's Python (the one radon is installed in): it calls that
interpreter's lexer directly, in the mode `ast.parse` uses
(`_tokenize.TokenizerIter(..., extra_tokens=False)`).

oxradon runs CPython's lexer, ported, before Ruff parses a file that contains
`{{` or has a tab or form feed in a line's leading blanks; on any other file it
runs it after Ruff has reported an error. Either way, if that lexer fails, the
file's error is the lexer's, with CPython's message and line. Each generated
text is checked twice: as written, and with a first line `# {{`, a comment
that makes the lexer run first. For every file:

- radon analyzes it: oxradon must give the same result, for `cc` and for `hal`;
- radon reports an error: oxradon must report one, for `cc` and for `hal`;
- radon's error is the one CPython's lexer reports: oxradon's must be the same,
  message and line, for `cc` and for `hal`. (Where CPython's parser fails first and its error stands,
  oxradon has the lexer's error or Ruff's, and the wording is not compared.)

The one accepted difference is the COMPATIBILITY.md row on a tab before a backslash
continuation inside indentation. The generator knows the first line of each
text that has that construct in an indentation CPython uses. oxradon's refusal
is excused only as that error on that line, from both commands, and only where
CPython's lexer reaches that line.

`--record FILE` also writes the texts with CPython's lexer verdict, for the
replay test in `src/rawtok.rs`.
"""

import io
import itertools
import json
import os
import random
import sys
import tempfile
import token as tokens
import warnings
from pathlib import Path

import _tokenize

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import is_error, run, usage  # noqa: E402

FORCE = "# {{\n"
POLICY = "inconsistent use of tabs and spaces in indentation"
EOF_IN_BRACKETS = "unexpected EOF in multi-line statement"
OPENING = {tokens.LPAR: "(", tokens.LSQB: "[", tokens.LBRACE: "{"}
CLOSING = {tokens.RPAR, tokens.RSQB, tokens.RBRACE}

OPENERS = ["if a:", "for i in b:", "while c:", "def g():", "with d:"]
PLAIN = ["x = 1", "pass", "y = x or z"]
BLANKS = [ws for n in range(9) for ws in map("".join, itertools.product(" \t", repeat=n))]
CONTINUED = ["\\\n", "\\\n    ", "    \\\n", "    \\\n    ", "  \\\n  ", "\\\n\\\n  ", "\t\\\n", "\t  \\\n  ",
             "  \t\\\n\t", "\\\n\t", "       \t\\\n", " \x0c\t\\\n  ", "\x0c", "\x0c  ", "\t\x0c\t"]
NUMBERS = ["0", "1", "12", "0x1f", "0o17", "0b101", "1.5", "1e5", "1j", "1_000", "012", "00", "0_0", "1__0",
           "0x", "0o8", "0b2", "0o", "0b", "1e", "1e+", "1_", "0x_", "1.e2", ".5", "0.0j", "09.5", "0e0"]
SUFFIXES = ["", " ", "abc", "if y else 2", "and y", "or y", "else", "in y", "is y", "not in y", "for", "_", "l",
            "e", "j", "x", "é", ".real", "(", "]"]
BRACKETS = ["(", ")", "[", "]", "{", "}", "a", ",", "1", ":", "\n", "f'{", "}'", "'"]
NAMES = ["a", "é", "aé", "_é", "é1", "a\u00b7", "\u00b7a", "©", "a©", "\u0301x", "x\u0301", "a\u00a0b", "€",
         "a€", "\u1885", "a\u1885", "\u2118", "\u212e", "x\u0660", "\u0660x", "\U0001f600", "a\u200b", "\ufeffa",
         "名前", "a\u0e33", "\u309b", "a\u309b"]
PREFIXES = ["", "b", "r", "u", "f", "t", "rb", "br", "rf", "fr", "rt", "tr", "ub", "ur", "uf", "bf", "ft", "F", "Rb"]
BODIES = ["", "a", "a\\", "a\\'", "a\\\nb", "{x}", "{x", "x}", "{{", "}}", "a\nb", "\\N{BULLET}", "{x!r}", "{x=}"]
SPEC_PIECES = ["", "x", ">10", "{b}", "{b}{c}", "{b:{c}}", "{{", "}}", "{", "}", "{{x}}", "{{x or y}}", "!r", "=",
               "\\", "\\N{BULLET}", "\\{", " ", ":", "{b!r}", "{b=}", "\n", "{{{c}}}", "(", "[1"]
BAD_PREFIXES = ["0b2\n", "x = 012\n", "def f():\n\tif a:\n   return 1\n", "x = )\n", "x = (]\n", "x = 1abc\n",
                "x = 'abc\n", "if a:\n\tx = 1\n        y = 2\n", "x = a©\n", "x = 1 \\ 2\n"]
REPAIRABLE = ["x = f'{a:{b}{{x if y else z}}}'\n", "x = f'{a:{b}{{" + "(" * 300 + "1" + ")" * 300 + "}}}'\n",
              "if a:\n\tx = f'{a:{b}{{y or z}}}'\n\ty = 2\n"]


def scan(ws):
    """CPython's two indentation columns for the blanks `ws`, and whether a
    backslash fixed them at a point where they differ."""
    col = alt = 0
    fixed = None
    i = 0
    while i < len(ws):
        ch = ws[i]
        if ch == " ":
            col, alt = col + 1, alt + 1
        elif ch == "\t":
            col, alt = (col // 8 + 1) * 8, alt + 1
        elif ch == "\x0c":
            col = alt = 0
        elif ch == "\\":
            if fixed is None and col:
                fixed = (col, col != alt)
            i += 1  # the newline
        i += 1
    if fixed:
        return fixed[0], fixed[0], fixed[1]
    return col, alt, False


BY_COLUMNS = {}
for _ws in BLANKS:
    BY_COLUMNS.setdefault(scan(_ws)[:2], []).append(_ws)


def indentation(rng):
    """A function whose lines are indented with random blanks, levels often
    respelled with the same two CPython columns. Also the first line, if any,
    where oxradon refuses the indentation (the COMPATIBILITY.md row)."""
    lines, stack, refused, last_line = ["def f():"], [""], None, 1
    for _ in range(rng.randint(2, 7)):
        roll = rng.random()
        if lines[-1].endswith(":") or roll < 0.3:
            deeper = [ws for ws in rng.sample(BLANKS, 40)
                      if scan(ws)[0] > scan(stack[-1])[0] and scan(ws)[1] > scan(stack[-1])[1]]
            ws = rng.choice(deeper) if deeper and rng.random() < 0.85 else rng.choice(BLANKS)
            stack.append(ws)
        elif roll < 0.8:
            del stack[rng.randint(1, len(stack)):]
            ws = rng.choice(BY_COLUMNS.get(scan(stack[-1])[:2], [stack[-1]]))
        else:
            ws = rng.choice(BLANKS)
        if rng.random() < 0.15:
            ws = rng.choice(CONTINUED) if rng.random() < 0.5 else ws + rng.choice(CONTINUED)
        # The line the statement itself is on, after any continuation lines.
        last_line += 1 + ws.count("\n")
        kind = rng.random()
        if kind < 0.08:
            body = "# comment"  # the indentation is not used
        elif kind < 0.12:
            body = ""
        else:
            body = rng.choice(OPENERS) if rng.random() < 0.4 else rng.choice(PLAIN)
            if refused is None and scan(ws)[2]:
                refused = last_line
        lines.append(ws + body)
    if lines[-1].endswith(":"):
        lines.append(stack[-1] + "         pass")
    if rng.random() < 0.1:  # the same blanks inside brackets: not used either
        lines.append("z = (\n" + rng.choice(CONTINUED) + "1)")
    return "\n".join(lines) + "\n", refused


def numbers(rng):
    return f"x = {rng.choice(NUMBERS)}{rng.choice(SUFFIXES)}\n", None


def brackets(rng):
    return "x = " + "".join(rng.choice(BRACKETS) for _ in range(rng.randint(1, 8))) + "\n", None


def names(rng):
    return f"{rng.choice(NAMES)} = {rng.choice(NAMES)}.{rng.choice(NAMES)}\n", None


def strings(rng):
    quote = rng.choice(["'", '"', "'''", '"""'])
    end = rng.choice([quote, quote, quote, "", quote[0]])
    text = f"x = {rng.choice(PREFIXES)}{quote}{rng.choice(BODIES)}{rng.choice(BODIES)}{end}"
    return text + rng.choice(["\n", "\ny = 1\n", " + 'a'\n"]), None


def format_specs(rng):
    prefix = rng.choice(["f", "f", "t", "rf", "F"])
    quote = rng.choice(["'", "'", '"', "'''"])
    spec = "".join(rng.choice(SPEC_PIECES) for _ in range(rng.randint(0, 5)))
    inner = f"{{a:{spec}}}" if rng.random() < 0.9 else f"{{a{spec}"
    return f"def f():\n    return {prefix}{quote}{inner}{quote} + (p or q)\n", None


def after_an_error(rng):
    return rng.choice(BAD_PREFIXES) + rng.choice(REPAIRABLE), None


GENERATORS = [indentation, indentation, indentation, numbers, brackets, names, strings, format_specs,
              format_specs, after_an_error]


def lexer_verdict(text):
    """None if CPython's parser-mode lexer accepts `text`, else the message and line
    `ast.parse` gives that error. They are the lexer's own, except at EOF inside brackets,
    where the parser names the innermost open bracket and its line."""
    brackets = []
    try:
        for kind, _, start, _, _ in _tokenize.TokenizerIter(io.StringIO(text).readline, extra_tokens=False):
            if kind in OPENING:
                brackets.append((OPENING[kind], start[0]))
            elif kind in CLOSING and brackets:
                brackets.pop()
    except SyntaxError as error:
        if error.msg == EOF_IN_BRACKETS and brackets:
            bracket, line = brackets[-1]
            return f"'{bracket}' was never closed", line
        return error.msg, error.lineno
    return None


def normalized(text):
    """Line endings as `ast.parse` and radon's file reading see them."""
    return text.replace("\r\n", "\n").replace("\r", "\n")


def message(verdict):
    return f"{verdict[0]} (<unknown>, line {verdict[1]})"


def compare(verdict, refusal, results):
    """The problems in one file, and whether the COMPATIBILITY.md row excuses it.

    `verdict` is CPython's lexer verdict on the file, `refusal` the generated line and
    error where oxradon may refuse the indentation (or None), and `results` maps each
    command to radon's and oxradon's result."""
    if refusal is not None:
        line, error = refusal
        # The row, only as that error on that line from both commands, and only where
        # CPython's lexer gets that far: an earlier lexer error is the file's error.
        if results["cc"][1] == error and (verdict is None or verdict[1] >= line):
            if results["hal"][1] == error:
                return [], True
            return [("hal, where cc refuses the indentation", error, results["hal"][1])], False
    problems = []
    for command, (want, got) in results.items():
        if not is_error(want):
            if want != got:
                problems.append((f"{command}: radon analyzes it", want, got))
        elif not is_error(got):
            problems.append((f"{command}: radon rejects it", want, got))
        elif verdict and want["error"] == message(verdict) and got != want:
            # Where CPython reports the lexer's error, so must oxradon, to the letter.
            # Where CPython's parser failed first and its error stands, oxradon has the
            # lexer's or Ruff's, and Ruff's wording is not compared.
            problems.append((f"{command}: the lexer's error", want, got))
    return problems, False


def record(cases, path):
    """The fixture for `rawtok`'s replay test: text, then CPython's lexer verdict. Texts
    oxradon refuses on its own account are left out: that is not CPython's verdict."""
    rows, seen = [], set()
    for text, refused in cases:
        text = normalized(text)
        if refused is not None or text in seen:
            continue
        seen.add(text)
        verdict = lexer_verdict(text)
        rows.append([text, list(verdict) if verdict else None])
    Path(path).write_text(json.dumps(rows, ensure_ascii=True, indent=0) + "\n", encoding="utf-8")
    print(f"recorded {len(rows)} texts in {path}")


def main():
    args = [a for a in sys.argv[1:] if a != "--record"]
    recording = sys.argv[sys.argv.index("--record") + 1] if "--record" in sys.argv else None
    if recording:
        args.remove(recording)
    if len(args) < 2:
        usage(__doc__)
    radon, rs = os.path.abspath(args[0]), os.path.abspath(args[1])
    count = int(args[2]) if len(args) > 2 else 4000
    rng = random.Random(int(args[3]) if len(args) > 3 else 5)
    warnings.simplefilter("ignore")
    cases = [rng.choice(GENERATORS)(rng) for _ in range(count)]
    if recording:
        record(cases, recording)
    with tempfile.TemporaryDirectory() as tmp:
        files = {}
        for i, (text, refused) in enumerate(cases):
            newline = rng.choice(["\n", "\n", "\r\n", "\r"])
            for label, body, shift in (("plain", text, 0), ("forced", FORCE + text, 1)):
                name = f"{label}_{i:05}.py"
                Path(tmp, name).write_bytes(body.replace("\n", newline).encode("utf-8"))
                refusal = None if refused is None else (refused + shift, {"error": message((POLICY, refused + shift))})
                files[name] = (body, refusal)
        a, b = (json.loads(run([exe, "cc", "-j", "."], cwd=tmp)) for exe in (radon, rs))
        hal_a, hal_b = (json.loads(run([exe, "hal", "-j", "."], cwd=tmp)) for exe in (radon, rs))
        problems, excused, lexical, exact = [], 0, 0, 0
        for name, (body, refusal) in files.items():
            want, got = a.get(name), b.get(name)
            verdict = lexer_verdict(normalized(body))
            found, was_excused = compare(verdict, refusal, {"cc": (want, got), "hal": (hal_a.get(name), hal_b.get(name))})
            problems += [(name, why, w, g) for why, w, g in found]
            excused += was_excused
            if verdict and not was_excused and is_error(want) and is_error(got):
                lexical += 1
                exact += got["error"] == want["error"]
        valid = sum(not is_error(v) for v in a.values())
        print(f"{len(files)} files from {count} texts: radon analyzes {valid}; {lexical} fail in CPython's lexer "
              f"({exact} of them with radon's exact error); {excused} excused by the COMPATIBILITY.md row; "
              f"{len(problems)} problems")
        for kind in sorted({why for _, why, _, _ in problems}):
            print(f"  {sum(why == kind for _, why, _, _ in problems):6} {kind}")
        for name, why, want, got in problems[:12]:
            print(f"--- {name} ({why})\n    text:    {files[name][0]!r}\n    radon:   {want}\n    oxradon: {got}")
        sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
