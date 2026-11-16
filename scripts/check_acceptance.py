"""Compare which Python sources oxradon rejects with what CPython rejects.

Usage: python scripts/check_acceptance.py OXRADON

radon reports an error exactly when `ast.parse` raises, so CPython (this
script's interpreter) is the oracle and radon itself is not needed. The
snippets are harvested from CPython's own syntax tests: every short string
constant in the files below, their doctest examples, and the whole-file cases
(`badsyntax_*.py`, `test_future_stmt/*.py`). Generated sources at the edges of
the tokenizer's fixed limits are added, since those tests don't reach them,
and a few where CPython's lexer and Ruff's read the text differently.
"""

import ast
import doctest
import glob
import json
import os
import platform
import sys
import sysconfig
import tempfile
import warnings
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import is_error, run, usage  # noqa: E402

TEST_FILES = [
    "test_syntax.py", "test_grammar.py", "test_fstring.py", "test_patma.py",
    "test_type_params.py", "test_exceptions.py", "test_positional_only_arg.py",
    "test_unpack_ex.py", "test_genexps.py", "test_tstring.py", "test_string_literals.py",
    "test_named_expressions.py", "test_type_aliases.py", "test_except_star.py",
    "test_keywordonlyarg.py", "test_global.py", "test_scope.py", "test_eof.py",
]


# Files read whole. `badsyntax_*.py` matches nothing in CPython 3.14.10 and is kept for the
# releases that have such files; the future-statement files are listed, so that a tree without
# them is an error.
FUTURE_FILES = [
    "badsyntax_future.py", "import_nested_scope_twice.py", "nested_scope.py", "test_future.py",
    "test_future_flags.py", "test_future_multiple_features.py",
    "test_future_multiple_imports.py", "test_future_single_import.py", "__init__.py",
]


def source_version(lib):
    """`PY_VERSION` of the CPython source tree whose `Lib` directory is `lib`, or None."""
    try:
        text = (Path(lib).parent / "Include" / "patchlevel.h").read_text(encoding="utf-8")
    except OSError:
        return None
    for line in text.splitlines():
        parts = line.split()
        if parts[:2] == ["#define", "PY_VERSION"]:
            return parts[2].strip('"')
    return None


def harvest():
    # CPython's test files: `CPYTHON_LIB/test` (the `Lib` directory of a CPython source tree of
    # this interpreter's version) or the interpreter's own. Some builds ship no tests, which
    # must not pass as a smaller check.
    lib = os.environ.get("CPYTHON_LIB")
    if lib:
        # The tests must be this interpreter's: another release's would be a different check.
        version = source_version(lib)
        if version != platform.python_version():
            sys.exit(f"CPYTHON_LIB={lib} is CPython {version or '(no Include/patchlevel.h)'}, "
                     f"but this interpreter is {platform.python_version()}")
    else:
        lib = sysconfig.get_path("stdlib")
    test_dir = os.path.join(lib, "test")
    expected = TEST_FILES + [os.path.join("test_future_stmt", name) for name in FUTURE_FILES]
    missing = [name for name in expected if not os.path.exists(os.path.join(test_dir, name))]
    if missing:
        sys.exit(f"{test_dir} lacks {len(missing)} of CPython's test files ({missing[0]}, ...): "
                 "set CPYTHON_LIB to the Lib directory of a CPython source tree")
    parser = doctest.DocTestParser()
    snippets = set()
    for name in TEST_FILES:
        path = os.path.join(test_dir, name)
        tree = ast.parse(Path(path).read_text(encoding="utf-8"))
        for node in ast.walk(tree):
            if isinstance(node, ast.Constant) and isinstance(node.value, str):
                text = node.value
                if ">>> " in text:
                    snippets.update(ex.source for ex in parser.get_examples(text))
                elif 0 < len(text) <= 400:
                    snippets.add(text)
    # The required files are read by name: a listing that fails or comes back short must not
    # shrink the check. The optional pattern is escaped, so the tree's own path cannot change it.
    for name in FUTURE_FILES:
        path = os.path.join(test_dir, "test_future_stmt", name)
        snippets.add(Path(path).read_text(encoding="utf-8"))
    for path in glob.glob(os.path.join(glob.escape(test_dir), "badsyntax_*.py")):
        snippets.add(Path(path).read_text(encoding="utf-8"))
    return sorted(s for s in snippets if s.isprintable() or "\n" in s or "\t" in s)


def tokenizer_limits():
    """Sources at each edge of the limits CPython's tokenizer has and Ruff's lexer lacks:
    200 nested brackets, 100 levels of indentation, 150 nested f-strings, and names
    restricted to the interpreter's own Unicode version."""
    def brackets(n, opening="(", closing=")"):
        return f"x = {opening * n}1{closing * n}\n"

    def blocks(n):
        return "".join(" " * i + "if x:\n" for i in range(n)) + " " * n + "pass\n"

    def strings(n):
        return "x = " + 'f"{' * n + "1" + '}"' * n + "\n"

    newer = chr(0x1E6C0)  # assigned in Unicode 17.0, after CPython 3.14's 16.0
    accented = "caf" + chr(0xE9)
    cases = [brackets(n) for n in (200, 201)]
    cases += [brackets(n, "[", "]") for n in (200, 201)]
    cases += [f"x = {'[{' * 100}{'}]' * 100}\n", f"x = [{'[{' * 100}{'}]' * 100}]\n"]
    cases += [f"f'{{{'(' * n}1{')' * n}}}'\n" for n in (199, 200)]
    cases += [blocks(n) for n in (99, 100)]
    cases += [blocks(99) * 2]
    cases += [strings(n) for n in (149, 150)]
    cases += [f"x{newer} = 1\n", f"x = '{newer}'  # {newer}\n", f"{accented} = 1\n"]
    return cases


def lexer_edges():
    """Sources CPython's lexer and Ruff's read differently, which oxradon lexes with a port of
    CPython's before Ruff parses them (scripts/check_lexer.py generates many more): a doubled
    brace in a format spec after a nested field, and indentation with tabs."""
    spec = "x = f'{a:{b}{{"
    newer = chr(0x1E6C0)
    return [
        spec + "x if y else z}}}'\n", spec + "x}'\n", spec + "x}}{c or d}}'\n",
        spec + "(" * 198 + "}'\n", spec + newer + "}'\n", "0b2\n" + spec + "x}'\n",
        "def f():\n    \t\tpass\n \t   \tpass\n", "def f():\n\tif a:\n   return 1\n",
        "if a:\n\tx = 1\n        y = 2\n", "def f():\n\t\x0c\tdef g():\n    x = 1\n",
        "def f():\n    return (\n\t\\\n        1)\n", "def f():\n\t\\\n# comment\n    return 1\n",
        "if a:\n\tx = " + spec[4:] + "y or z}}}'\n\ty = 2\n",
    ]


def accepted(source):
    """Whether `ast.parse` accepts the source. Any exception counts as a
    rejection (SyntaxError, ValueError, RecursionError...): radon reports it."""
    try:
        ast.parse(source)
    except Exception:
        return False
    return True


# The one accepted difference: `<>` after `from __future__ import barry_as_FLUFL` (a
# COMPATIBILITY.md row). Exactly these snippets, each with (CPython accepts, oxradon accepts);
# they are shown and do not fail the check. Anything else that differs does, whatever it
# mentions.
DOCUMENTED = {
    "from __future__ import barry_as_FLUFL\n1<>2": (True, False),
    "from __future__ import barry_as_FLUFL\na != b": (False, True),
}


def main():
    if len(sys.argv) < 2:
        usage(__doc__)
    warnings.simplefilter("ignore")  # ast.parse warns about the snippets' escapes
    rs = sys.argv[1]
    snippets = harvest() + tokenizer_limits() + lexer_edges()
    with tempfile.TemporaryDirectory() as tmp:
        paths = {}
        for i, src in enumerate(snippets):
            path = os.path.join(tmp, f"s{i:05}.py")
            try:
                with open(path, "w", encoding="utf-8", newline="") as fh:
                    fh.write(src)
            except UnicodeEncodeError:
                continue
            paths[path] = src
        result = json.loads(run([rs, "cc", "-j", tmp]))
    python_ok = {path: accepted(src) for path, src in paths.items()}
    differ = []
    for path, src in paths.items():
        rs_ok = not is_error(result.get(path))
        if rs_ok != python_ok[path]:
            differ.append((src, python_ok[path], rs_ok))
    rejected = sum(not ok for ok in python_ok.values())
    documented = [d for d in differ if DOCUMENTED.get(d[0]) == d[1:]]
    unexpected = [d for d in differ if DOCUMENTED.get(d[0]) != d[1:]]
    print(f"CPython {platform.python_version()}, tests from {os.environ.get('CPYTHON_LIB') or 'its own library'}")
    print(f"{len(paths)} snippets ({rejected} rejected by CPython): {len(differ)} differ, "
          f"{len(documented)} of them the documented `<>` case")
    for src, py_ok, rs_ok in (unexpected + documented)[:25]:
        print(f"--- CPython {'accepts' if py_ok else 'rejects'}, oxradon {'accepts' if rs_ok else 'rejects'}:")
        print(src if len(src) < 300 else src[:300] + " ...")
    sys.exit(1 if unexpected else 0)


if __name__ == "__main__":
    main()
