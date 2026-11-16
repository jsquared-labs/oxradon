"""Random Python-ish source for testing `raw` (used by fuzz_raw.py and update_golden_raw.py).

The fragments aim at the paths radon's `raw` reaches through CPython's tokenizer: strings of every
prefix and quote, f-/t-string replacement fields and format specs, unterminated literals, numbers
with invalid digits, continuation backslashes, brackets, `:` and `;`, comments, and the characters
that `str.splitlines()` and `str.strip()` treat specially.
"""

PREFIXES = ["", "", "", "r", "b", "rb", "Br", "u", "U", "f", "f", "rf", "Fr", "t", "rt", "Tr",
            "uf", "bf", "ft", "ub", "ur", "bt"]
QUOTES = ['"', "'", '"""', "'''"]
NAMES = ["x", "y", "foo", "_", "é", "€", "a1", "if", "else", "lambda", "def", "pass", "class",
         "\ufeff", "x\xa0y"]
NUMBERS = ["0", "1", "42", "1_000", "0x1f", "0x", "0x_1", "1_", "1__0", "0b101", "0b2", "0o7",
           "0o9", "0o", "0777", "00", "1e5", "1e", "1e+", "1e-3", "1j", "1.5j", ".5", "1.", "1.e5",
           "1if", "0x1for", "1_e2", "3.14_15"]
OPS = [":", ":", ":", ";", ";", ":=", "->", "...", "<>", "!=", "!", "=", "==", "+", "**=", ">>=",
       ".", ",", "@", "$", "?", "`", "~", "->"]
BRACKETS = ["(", ")", "[", "]", "{", "}"]
ODD = ["\x00", "\x01", "\x1f", "\x7f", "\x0c", "\x0b", "\x1c", "\x1d", "\x85", "\u2028", "\u2029",
       "\xa0", "\u3000", "\t", "\x1e"]
LINE_BREAKS = ["\n"] * 20 + ["\r\n", "\r", "\x0c", "\x0b", "\x1c", "\x85", "\u2028", "\u2029"]
STRING_BODY = ["a", " ", "x y", "\\", "\\n", "\\'", '\\"', "'", '"', "{", "}", "{{", "}}", "#",
               "\\N{DASH}", "\\N", ":", ";", "\n", "\\\n", "\x0c", "é"]
FIELD_BODY = ["x", "x!r", "x!s:>10", "x:>{w}", "x=", "x = ", "x:", "x:=5", "a;b", "lambda: 1",
              "x # c\n", "x:{y}", "x:{y:{z}}", "f'{x}'", "'a'", "{x}", "x:\n", "x\n:>3", "",
              "x!", "x:%Y-%m", "(x)", "[1:2]", "x}", "x:{"]


def string_literal(rng):
    prefix = rng.choice(PREFIXES)
    quote = rng.choice(QUOTES)
    parts = []
    for _ in range(rng.randrange(4)):
        is_ft = "f" in prefix.lower() or "t" in prefix.lower()
        if is_ft and rng.random() < 0.5:
            parts.append("{" + rng.choice(FIELD_BODY) + rng.choice(["}", "}", "}", ""]))
            continue
        parts.append(rng.choice(STRING_BODY))
    close = quote if rng.random() < 0.93 else rng.choice(["", quote[0], "\n"])
    return prefix + quote + "".join(parts) + close


def fragment(rng):
    r = rng.random()
    if r < 0.22:
        return string_literal(rng)
    if r < 0.40:
        return rng.choice(NAMES)
    if r < 0.50:
        return rng.choice(NUMBERS)
    if r < 0.65:
        return rng.choice(OPS)
    if r < 0.70:
        return rng.choice(["()", "[x]", "{a: b}", "(x,\n y)", "[1:2]", "f(x)[0]", "{}"])
    if r < 0.76:
        return rng.choice(BRACKETS)
    if r < 0.82:
        return "#" + rng.choice(["", " c", " a: b; c", " 'x", " \\"])
    if r < 0.85:
        return "\\" + rng.choice(["", "", "", " ", "x"])
    if r < 0.97:
        return rng.choice(["if x:", "else:", "def f():", "class C: pass", "try: 1/0",
                           "x = 1; y = 2", "for i in x: pass", "while 1: break; x"])
    return rng.choice(ODD)


def line(rng):
    pad = lambda: rng.choice(["", "", "", " ", "    ", "\t", " \x0c"])  # noqa: E731
    seps = ["", " ", " ", " "]
    body = "".join(fragment(rng) + rng.choice(seps) for _ in range(rng.randrange(7)))
    return pad() + body + pad()


CLEAN_PREFIXES = ["", "", "", "r", "b", "rb", "u", "f", "f", "rf", "t", "rt"]
CLEAN_FIELDS = ["x", "x!r", "x!s:>10", "x:>{w}", "x=", "x:=5", "a;b", "lambda: 1", "x:{y}",
                "(x)", "[1:2]", "x:%Y-%m", "f'{y}'"]
CLEAN_BODY = ["a", " ", "x y", "\\n", "\\'", '\\"', "{{", "}}", "#", "\\N{DASH}", ":", ";",
              "\xe9", "\x0c", "\n"]


def clean_string(rng):
    """A string literal that always tokenizes on its own."""
    prefix = rng.choice(CLEAN_PREFIXES)
    quote = rng.choice(QUOTES)
    is_ft = "f" in prefix or "t" in prefix
    parts = []
    for _ in range(rng.randrange(4)):
        if is_ft and rng.random() < 0.5:
            parts.append("{" + rng.choice(CLEAN_FIELDS) + "}")
            continue
        piece = rng.choice(CLEAN_BODY)
        if len(quote) == 1 and piece in ("\x0c", "\n"):
            piece = "a"  # these split or end a single-quoted line
        if quote[0] == "'" and is_ft and piece == "\\'":
            piece = "a"
        parts.append(piece)
    return prefix + quote + "".join(parts) + quote


def clean_fragment(rng):
    """A fragment that never makes tokenizing fail on its own."""
    r = rng.random()
    if r < 0.25:
        return clean_string(rng)
    if r < 0.45:
        return rng.choice(NAMES[:12])
    if r < 0.55:
        return rng.choice(["0", "1", "42", "1_000", "0x1f", "1e5", "1j", ".5", "0777", "1if"])
    if r < 0.70:
        return rng.choice(OPS[:16])
    if r < 0.80:
        return rng.choice(["()", "[x]", "{a: b}", "(x,\n y)", "[1:2]", "f(x)[0]", "{}"])
    if r < 0.88:
        return "#" + rng.choice(["", " c", " a: b; c", " 'x"])
    return rng.choice(["if x:", "else:", "def f():", "class C: pass", "try: 1/0",
                       "x = 1; y = 2", "for i in x: pass", "while 1: break; x"])


def clean_source(rng):
    """Text that radon analyzes (no file-level error) far more often than [`source`]'s."""
    out = []
    for _ in range(rng.randrange(1, 13)):
        pad = rng.choice(["", "", "    ", "\t"])
        if rng.random() < 0.1:  # a docstring-like string alone on its lines
            q = rng.choice(['"""', "'''"])
            inner = "\n".join(rng.choice(["", "  text", "x: y; z", "# not a comment"])
                               for _ in range(rng.randrange(3)))
            out.append(f"{pad}{rng.choice(['', 'r', 'f', 'b'])}{q}{inner}{q}\n")
            continue
        body = " ".join(clean_fragment(rng) for _ in range(rng.randrange(5)))
        out.append(pad + body + rng.choice(["", "", " "]) + rng.choice(["\n"] * 8 + ["\r\n", "\x0c"]))
    return "".join(out)


def source(rng):
    """A whole file's text: random lines joined by random line breaks."""
    if rng.random() < 0.6:
        return clean_source(rng)
    lines = [line(rng) for _ in range(rng.randrange(1, 13))]
    text = "".join(ln + rng.choice(LINE_BREAKS) for ln in lines)
    if rng.random() < 0.3:
        text = text.rstrip("\n")
    return text


def buffers(rng, text):
    """radon's tokenizer inputs for one source: every run of consecutive stripped lines, joined
    by "\\n", that starts at a line (capped, so long files don't explode)."""
    stripped = [ln.strip() for ln in text.splitlines()]
    out = []
    for i in range(len(stripped)):
        for j in range(i + 1, min(len(stripped), i + 4) + 1):
            out.append("\n".join(stripped[i:j]))
    rng.shuffle(out)
    return out[:6]

