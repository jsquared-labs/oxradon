# Compatibility in detail

The [README](README.md#compatibility) has the short version. This file has the full promise,
every check behind the "0 differences" figures, and every known difference.

## What oxradon promises

oxradon aims to print what **radon 6.0.1 prints when it runs on CPython 3.14.10**.

For the flags oxradon supports, the terminal output is identical byte for byte. So is the JSON,
with one exception: the order of the keys inside `cc`'s objects. radon takes that order from a
Python `set`, so it changes from one radon run to the next. (`hal`'s, `raw`'s and `mi`'s JSON
are byte-identical.)

That includes radon's quirks: `assert` adds 1 but nothing inside it is counted, decorators are
never visited, and a function's `endline` comes from its last statement only.

The exact Python release matters. CPython 3.14.10 changed `tokenize` so that a bracket which
mismatches an f-string field's own `{` is an error. As a result, radon's `raw` on 3.14.8 or
older counts a file such as `tests/fixtures/raw/field_bracket.py`, and radon on 3.14.10 reports
it as an error. oxradon follows 3.14.10.

`raw` never parses your code. radon groups lines by re-running CPython's `tokenize` on them, so
oxradon ports CPython 3.14's tokenizer. Python 3.12 and 3.13 have the same tokenizer design but
are not tested. radon on 3.11 or older gives different `raw` numbers: its tokenizer read an
f-string as one `STRING`, so an f-string "docstring" counted as a comment line, where newer
versions count it as code.

oxradon reports a file as an error exactly when `ast.parse` on CPython 3.14 rejects it. radon
inherits that from the Python running it, so radon on an older Python accepts different code.
The structure of errors matches radon as well: which files are listed, where an error appears,
and which errors end the run. For example, a file that cannot be opened stops radon's whole run,
with one `ERROR:` line on stdout and exit status 0. The wording of error messages can differ,
and the first row of the table further down explains how.

Two parts of radon's output depend on the machine it runs on, and oxradon follows radon on the
same machine. `mi` calls the C library's `sin` and `log`, whose last bit differs between
systems, so the last digit of a score can differ between them. Of 4,007 recorded `mi_compute`
results (recorded on an Apple silicon Mac), 34 differ in that digit on Linux with glibc, 37 on
Linux with musl (Alpine) and 78 on an Intel Mac. Also, a directory's files are
listed in the order the system returns them, which differs between file systems.

Configuration is read the way radon reads it: `setup.cfg`, `radon.cfg`, `~/.radon.cfg` and the
file named by `RADONCFG` with `configparser`'s syntax and interpolation, and `pyproject.toml`
with a port of CPython's `tomllib`.

As in radon, `setup.cfg`, `radon.cfg` and `pyproject.toml` are read from the directory you run
oxradon in, and they can set `output_file`. The report then replaces that file's contents,
wherever it is. So don't run oxradon from inside a project you don't trust; run it from
elsewhere and name the project's path instead (`oxradon cc path/to/project`).

Jupyter notebooks are not analyzed. `--include-ipynb` and `--ipynb-cells` are accepted and
notebooks are skipped, which is what radon itself does when `nbformat` is not installed.

Like radon, oxradon says nothing about a path that does not exist: it is skipped, and the exit
status is still 0.

## How it was checked

Every figure below comes from a script in `scripts/`, named at the end of its item, so you can
run it yourself. [CONTRIBUTING.md](CONTRIBUTING.md) explains the setup. The corpus is the CPython
3.14.10 standard library plus Django, SymPy, SQLAlchemy, Pydantic, Rich, Black, FastAPI and
httpx: 5,698 files.

- `cc`, on the 4,828 files that have at least one block (1,688 from the standard library, 3,140
  from the other packages). Compared byte for byte across 7 flag combinations: 0 differences
  (`scripts/diff_cc.py`).
- `hal`, on 5,697 of the 5,698 files, including files without blocks. radon hits Python's
  recursion limit on one SymPy file, so that file is skipped. JSON and terminal output, with and
  without `-f`, compared byte for byte: 0 differences (`scripts/diff_hal.py`).
- `raw`, on all 5,698 files, in terminal output and JSON, with and without `-s`, error messages
  included: 0 differences, byte for byte (`scripts/diff_raw.py`). On top of that, 60,000
  generated files aimed at the tokenizer's corner cases (`scripts/fuzz_raw.py`). `cargo test`
  replays 4,044 buffers through CPython's tokenizer, along with CPython's decoding errors, line
  splitting and `{:.0%}` rounding (`scripts/update_golden_raw.py`).
- `mi`, on 5,697 files (4,962 rank A, 220 B, 510 C, 5 errors; the same SymPy file is skipped).
  7 flag combinations, plus `-s --sort` on the files without errors: 0 differences, byte for
  byte apart from the wording of syntax errors (`scripts/diff_mi.py`). `cargo test` replays, bit
  for bit, 4,007 recorded `mi_compute` results, `mi_parameters` on 2,000 modules (the fixtures
  and generated ones, with multi-line strings counted and not counted), and Python's `{:.2f}` on
  5,012 values (`scripts/update_golden_mi.py`).
- String concatenations: 12,000 generated f-/t-string concatenations, including escapes that
  decode to nothing. They decide `endline` in subtle ways: 0 differences
  (`scripts/fuzz_strings.py`).
- Which code is accepted: 3,179 snippets, 1,981 of them invalid. 3,150 come from CPython's own
  syntax tests, 16 sit at the edges of its tokenizer's fixed limits (200 nested brackets, 100
  levels of indentation, 150 nested f-strings), and 13 are places where CPython's lexer and
  Ruff's read the text differently. oxradon accepts and rejects each one exactly as `ast.parse`
  does, except the `<>` case below (`scripts/check_acceptance.py`).
- Where the two lexers can disagree: 100,000 generated files. They mix tabs, spaces, form feeds
  and backslash continuations in indentation; put doubled braces in f-string format specs; and
  break numbers, brackets, names and strings. Each also appears after an earlier error, and with
  Unix, Windows and old Mac line endings. `cc` and `hal` accept and reject each as radon does and
  give radon's results, except the tab-before-continuation case below. Where CPython's lexer
  rejects a file, oxradon's error has that lexer's message and line (see "Error message wording"
  below); in 9 of 10 such files it is exactly the error radon prints (`scripts/check_lexer.py`).
- Names, character by character: every non-ASCII character CPython 3.14 accepts in a name
  (141,194 at the start, 144,459 after it), every assigned one it rejects, and samples of the
  unassigned and private-use ones: 0 differences (`scripts/check_identifiers.py`).
- `cargo test`, which needs no Python. It replays recordings made from the real tools:
  - radon's output over quirk-focused fixtures;
  - 678 first-line `#!` checks from radon's own `_is_python_file`, around 8 KiB read boundaries;
  - 8,117 `RADONFILESENCODING` spellings as `open()` resolves them;
  - 31,425 `fnmatch` cases;
  - 1,910 verdicts of CPython's own lexer, in the mode `ast.parse` uses.

  It also covers the error paths that end radon's run.

## Intentional differences

This is everything oxradon knowingly does differently. The first table has the differences you
may meet in ordinary use:

| Situation | radon | oxradon |
|---|---|---|
| Error message wording | CPython's | CPython's own for an error its lexer reports (an unterminated string, a malformed number, unbalanced brackets, inconsistent indentation), from a port of that lexer. Ruff's for an error only the parser finds, which can name a different line. Where a file has both kinds, oxradon reports the lexer's; CPython sometimes reports the parser's. A broken configuration file ends the run with status 1 like radon, but with a one-line message instead of a traceback |
| `hal` (terminal output) on a file it cannot read or parse | prints the file's name, then stops the whole run with `ERROR: 'dict' object has no attribute 'total'` (`'functions'` with `-f`) | prints the file's name and its `ERROR:` line, as `cc` does, and goes on to the next file |
| `-O file` | writes ANSI color codes into the file | colors files only with `COLOR=yes` |
| `RADONFILESENCODING` | any Python codec | utf-8, utf-8-sig, latin-1, ascii and cp1252, with Python's name rules and aliases, and `locale` (assumed UTF-8). Other codecs are reported as unknown |
| Paths after an option (`hal a.py -f b.py`) | `unrecognized arguments`, exit 2 | accepted: every path is analyzed |
| An option value that looks like a negative number (`--exclude -1`, `-e -.5`) | taken as the value | a usage error, exit 2 (`--exclude=-1` works in both) |
| `--help` and `--version` | argparse's help text, from radon's docstrings; `6.0.1` | oxradon's own help text (the same options, worded and laid out differently); `oxradon 0.1.0` |

### Edge cases

These need unusual input or an unusual environment:

| Situation | radon | oxradon |
|---|---|---|
| Very deeply nested code (e.g. SymPy's `resolvent_lookup.py`) | a per-file error once it passes Python's limits: `maximum recursion depth exceeded` (`cc`, `hal` and `mi` hit it at different depths), or CPython's parser or compiler giving up on still deeper code | analyzes it. Its own limit is far higher but not infinite: an expression nested beyond roughly 100,000 levels exhausts oxradon's stack, and the run aborts with no report (exit status 134) |
| File name with a lone `{` or `}`, or with a set-window-title escape (`ESC]0;…BEL`) | crashes while printing it (`str.format`, or colorama calling a Windows-only title setter). `mi` passes a successfully analyzed file's name to `str.format` as an argument, so only its error entries crash on a brace | prints it (with color off, the escape is stripped like any other) |
| The `-O` file is also read: it is stdout itself (`-O /dev/stdout`), or also an input (named, walked as a `.py`, or redirected into stdin) | what is read, and how report and `ERROR` lines interleave, depends on when CPython flushes its 128 KiB write buffer | the report is written after analysis, so such an input reads as empty; matches radon for reports under the buffer size |
| Output fails part-way (the reader of a pipe exits, a disk fills) and a later input is `-` | stops before reading stdin if its stdout write was already flushed; otherwise reads it, depending on CPython's buffering | reads every input, then writes. So it can **wait on stdin after the downstream reader has gone**. The exit status (120 for stdout, `ERROR:` line and 0 for an `-O` file) is the same |
| A FIFO (or another file that blocks when opened) that comes after a file which cannot be opened | stops at the file it cannot open and never reaches the FIFO | opens its inputs in parallel before reporting anything, so it waits on the FIFO until something writes to it |
| `from __future__ import barry_as_FLUFL` | `<>` is accepted and `!=` rejected after the import | the import is accepted, but `<>` stays a syntax error and `!=` stays valid |
| Character names in `\N{...}` escapes | resolved with CPython 3.14's Unicode 16.0 tables, so a name added later (`\N{LANDSLIDE}`) is a syntax error and algorithmic names (`\N{TANGUT IDEOGRAPH-17000}`) are valid | resolved with Ruff's tables: newer names are accepted and some algorithmic names are rejected, so such a file can be an error in only one of the two |
| A tab before a backslash continuation inside a line's indentation (the indentation is split over two lines) | CPython takes the first backslash's column, counted with 8-column tab stops, as the line's indentation, so some such files are valid | `cc`, `hal` and `mi` report the file as an error (`inconsistent use of tabs and spaces in indentation`) whenever that column differs from the count of characters before the backslash: Ruff's parser, which oxradon builds on, cannot be given such an indentation |
| CPython's own settings (`PYTHONIOENCODING`, `PYTHONUNBUFFERED`, `PYTHONUTF8`, a non-UTF-8 locale, …) | change stdin's encoding, stdout's encoding and buffering, the encoding config files are read with, and so which exit status a failed write gives | not read: oxradon behaves like radon under CPython's defaults (stdin and config files are always UTF-8) |
| Enormous `--exclude`/`--ignore` patterns (hundreds of kilobytes, from the command line or a config file) | matched by `fnmatch`, limited only by memory | patterns are compiled to a regex set capped at 1 GiB; past that, the run ends with status 1 |
| Deeply nested values in `pyproject.toml` | parsing gives up at CPython's recursion limit, which depends on how radon was started (the `radon` script and `python -m radon` differ), and rendering a deeply nested value exhausts CPython's native stack at a depth that depends on how CPython was built and, for some builds, on the stack limit. Measured on macOS arm64: a standalone CPython 3.14.10 works at 69,680 levels and fails at 69,760 with a 16 MiB stack limit, and works at 34,720 and fails at 34,800 with 8 MiB; Homebrew's CPython 3.14.8 works at 69,000 and fails at 70,000 with either | matches the `radon` console script's parsing limit exactly; treats a rendered value nested more than 69,700 levels deep as radon's crash, on every machine: an approximation, not a port of CPython's stack check |
| CPython's warnings (a `SyntaxWarning` for an invalid escape such as `"\d"`, or for `\{` in an f-string) | printed on stderr by CPython while it parses or tokenizes, sometimes more than once per file | not printed. stdout and the exit status are the same |
| Ctrl-C | CPython runs colorama's exit hook, which writes `ESC[0m` to a terminal | dies without it (one invisible byte; matching CPython's handling of an inherited ignored SIGINT would need unsafe code) |
| stdout closed (`>&-`) | exits 1 (with `COLOR` unset or `auto`, even before parsing arguments) | Rust's runtime reopens fd 1 on `/dev/null` before `main`, so it behaves as with `>/dev/null` and exits 0 |
| stdout inherited read-only (`1< file`) | every write fails, so it exits 120 | Rust's standard output treats a write to such a descriptor as done, so nothing is printed and it exits 0 |
| Non-UTF-8 file names | shown via `surrogateescape`; printing them may fail, depending on stdout's encoding and error policy | one U+FFFD per invalid byte, which `--exclude`/`--ignore` patterns also see (JSON uses Python's `\udcXX`) |
| An option value that is not valid UTF-8 (`-e $'\xff'`) | taken as the value (decoded with `surrogateescape`) | a usage error, exit 2 |
