# Contributing to oxradon

Thanks for wanting to help. oxradon is a port of radon 6.0.1 running on CPython 3.14.10, and the
aim is output that matches radon's byte for byte. Most questions about a change come back to
that aim.

## What oxradon promises

- radon's own output is the definition of correct. When oxradon and radon disagree, oxradon is
  wrong.
- The only exceptions are the rows of the "Intentional differences" tables in COMPATIBILITY.md
  (the README carries a short version). A change that needs a new row should say so up front.
- radon's quirks are kept on purpose, because people's CI thresholds were tuned against them:
  `assert` subtrees are not visited, decorators are skipped, `endline` comes from the last
  statement, `except*` adds 0, classes inside functions are dropped. Please don't "fix" them.

## How changes are made

- Port the mechanism. When behavior is unclear, read the radon or CPython source and port its
  logic. Rules inferred from example outputs have repeatedly turned out wrong here.
- Unicode versions matter. CPython 3.14 and the `regex` crate use Unicode 16.0, and Rust's
  standard library is newer. For Python's case rules use `pyfmt::py_lower` and `py_upper`, never
  `char::to_lowercase` or `to_uppercase`.
- TOML here is TOML 1.0. `src/tomllib.rs` ports CPython's `tomllib`. The `toml` crate implements
  TOML 1.1 and differs on integers and dates, so it can't stand in for it.
- Every subcommand declares all of radon's long options, even unsupported ones (hidden, and
  failing with exit status 2). Prefix matching (`--fun` for `--functions`) depends on the declared
  set, as it does in radon's argparse.
- No `unsafe` (it is forbidden in `Cargo.toml`), and no new dependency without discussion.
- `cargo fmt` formats the code. Comments wrap at 80 columns, which it does not enforce.
- Every public item has a doc comment (`missing_docs` warns otherwise). A comment says why the
  code is the way it is, and names the radon or CPython function it ports.

## Checks that need only Rust

```bash
cargo test --no-fail-fast
cargo clippy --all-targets
cargo fmt --check
cargo doc --no-deps
```

`cargo test` replays recordings made from radon and CPython (`tests/golden/`), so it needs no
Python. Use `--no-fail-fast`: without it, one failing test binary hides the rest. A few tests
measure things that only hold for optimized code (how much stack reading a deeply nested
`pyproject.toml` needs); `cargo test --release --no-fail-fast` runs those too.

## Checks against radon

These compare oxradon with the real radon, byte for byte. They need an oracle and a corpus.

### The oracle

radon 6.0.1 on CPython 3.14.10, in its own virtual environment. The exact patch release matters,
because 3.14.10 changed `tokenize`. Use a Python that stays at that version (a package manager
can upgrade one in place without telling you); [uv](https://docs.astral.sh/uv/) installs one:

```bash
uv python install 3.14.10
uv venv --python 3.14.10 oracle
uv pip install --python oracle/bin/python radon==6.0.1
```

Below, `RADON` is the absolute path of `oracle/bin/radon`, `PYTHON` is `oracle/bin/python`, and
`OXRADON` is `target/release/oxradon` (build it with `cargo build --release`). `RADON` must be an
absolute path: the scripts run both tools with a minimal `PATH`.

### The corpus

The README's numbers come from two trees:

- `STDLIB`: the `Lib` directory of the CPython source at the tag `v3.14.10` (1,873 `.py`
  files). An installed Python's standard library also works, but its contents depend on the
  build: some ship without CPython's tests. `scripts/check_acceptance.py` reads those tests
  from the interpreter's own standard library, or from `CPYTHON_LIB` when that names a `Lib`
  directory, and stops if they are missing;
- `THIRD_PARTY`: the `site-packages` of a second virtual environment holding the packages pinned
  in `scripts/corpus-requirements.txt`:

  ```bash
  uv venv --python 3.14.10 corpus
  uv pip install --python corpus/bin/python -r scripts/corpus-requirements.txt
  ```

Any Python tree works for a quick check. Build the pinned one with
[uv](https://docs.astral.sh/uv/), as shown, and you get the exact files the README's numbers were
measured on: the same 3,824, with the same contents. Use uv and not `python -m venv` for this,
because `venv` also installs pip into the environment, and pip's own Python files would then be
measured along with everything else.

### The scripts

Run them from the repository root. Each prints what it compared and exits non-zero on a
difference.

| Script | What it checks |
|---|---|
| `PYTHON scripts/diff_cc.py RADON OXRADON PATH...` | `cc`, 7 flag sets. With one path it compares the average footers too |
| `PYTHON scripts/diff_hal.py RADON OXRADON PATH...` | `hal`, JSON and terminal, with and without `-f` |
| `PYTHON -I scripts/diff_raw.py RADON OXRADON PATH...` | `raw`, JSON and terminal, with and without `-s` |
| `PYTHON -I scripts/diff_mi.py RADON OXRADON PATH...` | `mi`, 7 flag sets plus `-s --sort` |
| `PYTHON -I scripts/diff_config.py RADON OXRADON` | configuration files: one scratch project per case |
| `PYTHON -I scripts/fuzz_raw.py RADON OXRADON 20000 SEED` | `raw` on generated tokenizer corner cases |
| `PYTHON scripts/fuzz_strings.py RADON OXRADON` | f-/t-string concatenations, which decide `endline` |
| `PYTHON -I scripts/check_acceptance.py OXRADON` | accepts exactly what `ast.parse` accepts (CPython 3.14 is the oracle; radon is not needed) |
| `PYTHON -I scripts/check_identifiers.py OXRADON` | accepts a character in a name exactly when CPython 3.14 does (radon is not needed) |
| `PYTHON -I scripts/check_lexer.py RADON OXRADON` | the port of CPython's lexer that runs before Ruff (`rawtok::prepass`) against CPython's own: tab indentation, format specs, numbers, brackets, names and strings, valid and not (`--record` rewrites `tests/golden/strict-lexer.json`) |
| `PYTHON scripts/bench.py RADON OXRADON --command cc PATH` | speed, best of several runs |

`scripts/chart.py` draws the README's chart from `docs/bench.json`. oxradon's bars are drawn 3
pixels wide so they show at all; to scale they would be thinner than a pixel.

radon is slow on large trees (`mi` on the third-party corpus takes about three minutes).
`diff_raw.py` and `diff_mi.py` reuse radon's outputs from the directory named by `RADON_CACHE`;
delete it after changing radon or the corpus.

### Re-recording the test data

Only needed when a change adds or alters a recorded case. Run with the oracle's Python:

```bash
PYTHON scripts/update_golden.py RADON        # cc and hal outputs, fnmatch, encodings, shebangs
PYTHON -I scripts/update_golden_raw.py RADON # raw's recordings
PYTHON -I scripts/update_golden_mi.py RADON  # mi's recordings
```

The recordings are made on macOS. Two of `mi`'s tables hold floats whose last bit differs on
Linux, as radon's own results do, so after re-recording them run this on Linux (glibc) as well;
it writes the entries that differ there to `tests/golden/mi-*.linux.json`:

```bash
PYTHON -I scripts/update_golden_mi.py --linux-overlay
```

musl and Intel Macs round differently again, so their wheels need `tests/golden/mi-*.musl.json`
and `mi-*.macos-x86_64.json` too. Those come from radon itself, run under Alpine (on x86-64 and
ARM) and on an Intel Mac: start `.github/workflows/record-libm.yml` by hand and commit the
overlay files it uploads, after checking that both Alpine architectures recorded the same thing.

If a re-recording changes files you did not mean to change, find out why before you commit it.

## Before a pull request

1. All four Rust checks pass.
2. The differential scripts for the commands you touched report 0 differences on both trees
   (all of them, for a change to shared code such as discovery, reading or output).
3. `scripts/bench.py` shows no slowdown for the commands you touched.

## Releasing

Releases are made by `.github/workflows/release.yml`, from a tag `vX.Y.Z` that matches
`Cargo.toml`'s version; the version is set nowhere else. The workflow tests the tagged commit,
builds six wheels (Linux x86-64 and ARM, glibc and musl; macOS ARM and Intel) and the crate,
installs each wheel and runs the whole test suite against the installed binary
(`scripts/smoke_wheel.sh`), and then publishes those exact files: crates.io first, then PyPI,
each only after a maintainer approves it in the run. The job summary lists every file and its
SHA-256 before approval. Its rules and checks are in `scripts/release.py`, with tests in
`scripts/test_release.py` (`python3 -m unittest discover -s scripts -p test_release.py`).

If an upload fails, re-run the failed jobs of the same run. Don't start a new run for a version
that has started: it would build new files, and the registries would hold files that weren't
tested together. A version that has reached a registry can't be uploaded again, even after a
yank or delete, so a broken release is fixed with a new version.

## Things that look wrong but are not

- Rust reopens closed file descriptors 0-2 on `/dev/null` before `main`, so `>&-` cannot be told
  apart from `>/dev/null` without `unsafe` code. It is a documented difference.
- radon's documentation says it reads `tox.ini`. It doesn't, so oxradon doesn't either.
- `syntax::parse` sometimes hands Ruff a copy of the file with a few bytes replaced by spaces.
  Ruff's lexer reads two things differently from CPython's: `{{` in an f-string's format spec
  after a nested field (text to CPython, a field to Ruff), and tabs in indentation (tab stops
  of 8 and 1 in CPython, 2 and 1 in Ruff). A port of CPython's lexer (`rawtok::prepass`) runs
  first on the files that could have either, rejects what CPython rejects, and marks those
  bytes. The copy has the same length, so every position still refers to the original text.
  On any other file the same lexer runs only after Ruff has reported an error, to word the
  error as CPython does when it is one CPython's lexer reports.
- A path that does not exist is skipped silently, with exit status 0. radon does the same.
