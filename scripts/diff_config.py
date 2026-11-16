"""Differential test of radon's configuration files: `radon cc` (and `mi`) vs `oxradon`.

Usage: python scripts/diff_config.py RADON [OXRADON] [--only NAME]

Each case builds a scratch project (and a scratch $HOME), runs its command there
(`cc src` unless the case gives one; the `mi` cases do), and compares stdout,
the exit status, and any `-O` file. stderr is not compared:
radon's config errors are Python tracebacks. With RADON alone, it prints
radon's results, for recording expected values.
"""

import json
import os
import platform
import resource
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _common import RADON_ENV, usage  # noqa: E402

# Functions of rank A (1), B (6) and C (11).
SOURCE = (
    "def a():\n    pass\n\n"
    "def b(x):\n" + "".join(f"    if x == {i}: return {i}\n" for i in range(5)) + "    return 0\n\n"
    "def c(x):\n" + "".join(f"    if x == {i}: return {i}\n" for i in range(10)) + "    return 0\n"
)

RC = "radon.cfg"
# A pyproject.toml whose [tool.radon] sets cc_min = C, followed by another table.
PYPROJECT = b'[tool.radon]\ncc_min="C"\n[tool.other]\n'


def ini(body):
    return ("[radon]\n" + body).encode()


# (name, {path: bytes or None for a directory}, extra env, args)
# Paths starting with `~/` go in the scratch $HOME. Args default to `cc src`.
CASES = [
    ("none", {}, {}, None),
    ("radon_cfg_min", {RC: ini("cc_min = B\n")}, {}, None),
    ("radoncfg_env", {"custom.cfg": ini("cc_min = C\n")}, {"RADONCFG": "custom.cfg"}, None),
    ("radon_cfg_beats_env", {"custom.cfg": ini("cc_min = C\n"), RC: ini("cc_min = B\n")}, {"RADONCFG": "custom.cfg"}, None),
    ("pyproject_beats_radon_cfg", {RC: ini("cc_min = C\n"), "pyproject.toml": b'[tool.radon]\ncc_min = "B"\n'}, {}, None),
    ("setup_cfg_beats_pyproject", {"pyproject.toml": b'[tool.radon]\ncc_min = "B"\n', "setup.cfg": ini("cc_min = C\n")}, {}, None),
    ("home_beats_setup_cfg", {"setup.cfg": ini("cc_min = B\n"), "~/.radon.cfg": ini("cc_min = C\n")}, {}, None),
    ("merge_across_files", {RC: ini("cc_min = B\n"), "setup.cfg": ini("show_complexity = yes\n")}, {}, None),
    ("bool_true", {RC: ini("show_complexity = true\n")}, {}, None),
    ("bool_true_flag_turns_off", {RC: ini("show_complexity = true\n")}, {}, ["cc", "-s", "src"]),
    ("bool_true_flag_twice", {RC: ini("show_complexity = on\n")}, {}, ["cc", "-s", "-s", "src"]),
    ("bool_spellings", {RC: ini("show_complexity = YES\naverage = 1\ntotal_average = off\n")}, {}, None),
    ("bool_invalid", {RC: ini("show_complexity = maybe\n")}, {}, None),
    ("bool_invalid_other_command", {RC: ini("multi = maybe\n")}, {}, None),
    ("bool_invalid_functions", {RC: ini("functions = nope\n")}, {}, None),
    ("bool_invalid_ipynb", {RC: ini("include_ipynb = sure\n")}, {}, None),
    ("ipynb_valid_ignored", {RC: ini("include_ipynb = true\nipynb_cells = true\n")}, {}, None),
    ("invalid_config_help", {RC: ini("show_complexity = maybe\n")}, {}, ["cc", "--help"]),
    ("inline_comment_kept", {RC: ini("cc_min = B ; comment\n")}, {}, None),
    ("header_trailing_text", {RC: b"[radon] ignored\ncc_min = B\n"}, {}, None),
    ("header_greedy", {RC: b"[radon]x]\ncc_min = B\n"}, {}, None),
    ("section_case", {RC: b"[RADON]\ncc_min = B\n"}, {}, None),
    ("key_case", {RC: ini("CC_Min = B\n")}, {}, None),
    ("colon_delimiter", {RC: ini("cc_min: B\n")}, {}, None),
    ("lowercase_value", {RC: ini("cc_min = b\norder = lines\n")}, {}, None),
    ("empty_value", {RC: ini("cc_min =\n")}, {}, None),
    ("default_section", {RC: b"[DEFAULT]\ncc_min = C\n[radon]\n"}, {}, None),
    ("default_without_radon", {RC: b"[DEFAULT]\ncc_min = C\n"}, {}, None),
    ("interpolation", {RC: ini("level = C\ncc_min = %(level)s\n")}, {}, None),
    ("interpolation_escape", {RC: ini("exclude = *100%%*\n")}, {}, None),
    ("interpolation_bad_read_key", {RC: ini("cc_min = 50%\n")}, {}, None),
    ("interpolation_bad_unread_key", {RC: ini("other = 50%\n")}, {}, None),
    ("interpolation_missing", {RC: ini("cc_min = %(nope)s\n")}, {}, None),
    ("multiline_value", {RC: ini("exclude =\n  src/b*,\n  src/a*\n")}, {}, None),
    ("comment_lines", {RC: b"# top\n[radon]\n; note\ncc_min = B\n  # indented\n"}, {}, None),
    ("bom", {RC: b"\xef\xbb\xbf" + ini("cc_min = B\n")}, {}, None),
    ("crlf", {RC: ini("cc_min = B\n").replace(b"\n", b"\r\n")}, {}, None),
    ("non_utf8_radon_cfg", {RC: ini("cc_min = B\n") + b"# \xff\n"}, {}, None),
    ("non_utf8_setup_cfg", {"setup.cfg": ini("cc_min = B\n") + b"# \xff\n"}, {}, None),
    ("missing_section_header", {RC: b"cc_min = B\n"}, {}, None),
    ("parse_error", {RC: ini("not an option\n")}, {}, None),
    ("duplicate_option", {RC: ini("cc_min = B\ncc_min = C\n")}, {}, None),
    ("duplicate_section", {RC: b"[radon]\ncc_min = B\n[radon]\ncc_max = C\n"}, {}, None),
    ("radoncfg_empty", {}, {"RADONCFG": ""}, None),
    ("radoncfg_missing", {}, {"RADONCFG": "nope.cfg"}, None),
    ("radoncfg_directory", {"adir": None}, {"RADONCFG": "adir"}, None),
    ("radon_cfg_directory", {RC: None}, {}, None),
    ("setup_cfg_directory", {"setup.cfg": None}, {}, None),
    ("exclude_ignore", {RC: ini("exclude = src/b*\nignore = sub\n"), "src/sub/x.py": SOURCE.encode()}, {}, None),
    ("order_from_config", {RC: ini("order = ALPHA\n")}, {}, None),
    ("output_file", {RC: ini("output_file = out.txt\n")}, {}, ["cc", "-j", "src"]),
    ("output_file_empty", {RC: ini("output_file =\n")}, {}, None),
    ("flag_beats_config", {RC: ini("cc_min = C\n")}, {}, ["cc", "-n", "A", "src"]),
    ("pyproject_bool", {"pyproject.toml": b"[tool.radon]\nshow_complexity = true\n"}, {}, None),
    ("pyproject_int", {"pyproject.toml": b"[tool.radon]\ncc_min = 1\n"}, {}, None),
    ("pyproject_list", {"pyproject.toml": b'[tool.radon]\nexclude = ["src/a*", "src/b*"]\n'}, {}, None),
    ("pyproject_other_tool_percent", {"pyproject.toml": b'[tool.pytest.ini_options]\nlog_date_format = "%Y-%m-%d"\n'}, {}, None),
    ("pyproject_other_tool_ok", {"pyproject.toml": b'[tool.black]\nline-length = 88\n[tool.radon]\ncc_min = "B"\n'}, {}, None),
    ("pyproject_tool_scalar", {"pyproject.toml": b"[tool]\nfoo = 1\n"}, {}, None),
    ("pyproject_tool_not_table", {"pyproject.toml": b"tool = 1\n"}, {}, None),
    ("pyproject_no_tool", {"pyproject.toml": b'[project]\nname = "x"\n'}, {}, None),
    ("pyproject_invalid_toml", {"pyproject.toml": b"[tool.radon\n"}, {}, None),
    ("pyproject_non_utf8", {"pyproject.toml": b'[tool.radon]\ncc_min = "B"\n# \xff\n'}, {}, None),
    ("pyproject_default_section", {"pyproject.toml": b'[tool.DEFAULT]\ncc_min = "C"\n[tool.radon]\n'}, {}, None),
    ("pyproject_directory", {"pyproject.toml": None}, {}, None),
    # TOML values: validity, integer and float edge cases, rendering, recursion limits.
    ("toml_big_int_kept", {RC: ini("cc_min = B\n"), "pyproject.toml": PYPROJECT + b"n=9223372036854775808\n"}, {}, None),
    ("toml_year_zero", {"pyproject.toml": PYPROJECT + b"x=0000-01-01\n"}, {}, None),
    ("toml_leap_second", {"pyproject.toml": PYPROJECT + b"x=23:59:60\n"}, {}, None),
    ("toml_feb_29_non_leap", {"pyproject.toml": PYPROJECT + b"x=2023-02-29\n"}, {}, None),
    ("toml_v1_1_escape", {"pyproject.toml": PYPROJECT + b'x="\\e"\n'}, {}, None),
    ("toml_unassigned_repr", {"pyproject.toml": b'[tool.radon]\nexclude=["%(\\u0378)s"]\n"\\u0378"="ok"\n'}, {}, None),
    ("toml_empty_section_name", {"pyproject.toml": b'[tool.""]\ncc_min="C"\n[tool.radon]\n'}, {}, None),
    ("default_repeated_header", {RC: b"[DEFAULT]\ncc_min=B\n[DEFAULT]\nshow_complexity=yes\n[radon]\n"}, {}, None),
    ("toml_decimal_4300", {RC: ini("cc_min = B\n"), "pyproject.toml": PYPROJECT + b"n=" + b"9" * 4300 + b"\n"}, {}, None),
    ("toml_decimal_4301", {RC: ini("cc_min = B\n"), "pyproject.toml": PYPROJECT + b"n=" + b"9" * 4301 + b"\n"}, {}, None),
    ("toml_large_hex", {RC: ini("cc_min = B\n"), "pyproject.toml": PYPROJECT + b"n=0x" + b"f" * 3600 + b"\n"}, {}, None),
    ("toml_key_1000_parts", {RC: ini("cc_min = B\n"), "pyproject.toml": PYPROJECT + b".".join([b"k"] * 1000) + b"=1\n"}, {}, None),
    ("toml_key_1001_parts", {RC: ini("cc_min = B\n"), "pyproject.toml": PYPROJECT + b".".join([b"k"] * 1001) + b"=1\n"}, {}, None),
    ("toml_arrays_491", {RC: ini("cc_min = B\n"), "pyproject.toml": PYPROJECT + b"n=" + b"[" * 491 + b"1" + b"]" * 491 + b"\n"}, {}, None),
    ("toml_arrays_492", {RC: ini("cc_min = B\n"), "pyproject.toml": PYPROJECT + b"n=" + b"[" * 492 + b"1" + b"]" * 492 + b"\n"}, {}, None),
    ("toml_tables_327", {RC: ini("cc_min = B\n"), "pyproject.toml": PYPROJECT + b"n=" + b"{a=" * 327 + b"1" + b"}" * 327 + b"\n"}, {}, None),
    ("toml_tables_328", {RC: ini("cc_min = B\n"), "pyproject.toml": PYPROJECT + b"n=" + b"{a=" * 328 + b"1" + b"}" * 328 + b"\n"}, {}, None),
    ("toml_float_path", {"pyproject.toml": b"[tool.radon]\noutput_file=1000000000000000.25\n", "1000000000000000.2": None}, {}, None),
    # Unicode case mapping, deep values, NUL in output_file.
    ("unicode_case_after_16", {RC: "[radon]\n\ua7ce=B\n\ua7cf=C\n".encode()}, {}, None),
    ("unicode_case_upper_after_16", {RC: "[radon]\ncc_min=\ua7d3\n".encode()}, {}, None),
    ("final_sigma_key", {RC: "[radon]\ncc_min = B\nlevel\u03a3 = %(LEVEL\u03a3)s\n".encode()}, {}, None),
    # Below where radon's rendering of a nested value overflows a 16 MiB stack (see DEEP_OVER
    # below for the other side).
    ("toml_deep_under_tool_65", {"pyproject.toml": PYPROJECT + b"x=" + (b"{" + b".".join([b"k"] * 1000) + b"=") * 65 + b"1" + b"}" * 65 + b"\n"}, {}, None),
    ("output_file_nul_ini", {RC: b"[radon]\noutput_file=out\x00.txt\n"}, {}, None),
    ("output_file_nul_toml", {"pyproject.toml": b'[tool.radon]\noutput_file="out\\u0000.txt"\n'}, {}, None),
    ("huge_exclude_pattern", {RC: b"[radon]\nexclude=" + b"?" * 50000 + b"\n"}, {}, None),
    ("huge_ignore_pattern", {RC: b"[radon]\nignore=" + b"?" * 50000 + b"\n"}, {}, None),
    ("toml_deep_top_level_327", {"pyproject.toml": b"x=" + (b"{" + b".".join([b"k"] * 1000) + b"=") * 327 + b"1" + b"}" * 327 + b"\n" + b'[tool.radon]\ncc_min="C"\n'}, {}, None),
]

# Past that boundary where it has been measured: radon on macOS arm64 survives 69,680 levels and
# fails at 69,760 with a 16 MiB stack (oxradon's fixed limit is 69,700). Elsewhere radon's
# boundary is not known (a standalone CPython on Linux arm64 still works at 80,000), so there
# this case is reported as skipped, not compared.
DEEP_OVER = ("toml_deep_over_72", {"pyproject.toml": PYPROJECT + b"x=" + (b"{" + b".".join([b"k"] * 1000) + b"=") * 72 + b"1" + b"}" * 72 + b"\n"}, {}, None)
DEEP_OVER_MEASURED = sys.platform == "darwin" and platform.machine() == "arm64"


# `mi`: a module where counting multi-line strings as comments changes the index.
DOC = (Path(__file__).resolve().parent.parent / "tests/fixtures/mi/docstrings.py").read_bytes()
MI_CASES = [
    ("mi_none", {"src/doc.py": DOC}, {}, ["mi", "-s", "src"]),
    ("mi_multi_absent_flag", {"src/doc.py": DOC}, {}, ["mi", "-s", "-m", "src"]),
    ("mi_multi_false", {"src/doc.py": DOC, RC: ini("multi = false\n")}, {}, ["mi", "-s", "src"]),
    ("mi_multi_false_flag", {"src/doc.py": DOC, RC: ini("multi = false\n")}, {}, ["mi", "-s", "-m", "src"]),
    ("mi_multi_true_flag", {"src/doc.py": DOC, "setup.cfg": ini("multi = on\n")}, {}, ["mi", "-s", "-m", "src"]),
    ("mi_multi_toml_false", {"src/doc.py": DOC, "pyproject.toml": b"[tool.radon]\nmulti = false\n"}, {}, ["mi", "-s", "src"]),
    ("mi_multi_invalid", {RC: ini("multi = maybe\n")}, {}, ["mi", "src"]),
    ("mi_show_mi", {"src/doc.py": DOC, RC: ini("show_mi = yes\n")}, {}, ["mi", "src"]),
    ("mi_show_mi_flag", {"src/doc.py": DOC, RC: ini("show_mi = yes\n")}, {}, ["mi", "-s", "src"]),
    ("mi_show_mi_invalid", {RC: ini("show_mi = 2\n")}, {}, ["mi", "src"]),
    ("mi_min_max", {"src/doc.py": DOC, RC: ini("mi_min = b\nmi_max = c\n")}, {}, ["mi", "-s", "src"]),
    ("mi_min_flag_wins", {"src/doc.py": DOC, RC: ini("mi_min = C\n")}, {}, ["mi", "-n", "A", "src"]),
    ("mi_min_interpolated", {"src/doc.py": DOC, RC: ini("lvl = C\nmi_min = %(lvl)s\n")}, {}, ["mi", "src"]),
    ("mi_cc_keys_ignored", {"src/doc.py": DOC, RC: ini("cc_min = C\nshow_complexity = yes\n")}, {}, ["mi", "src"]),
    ("mi_exclude", {"src/doc.py": DOC, RC: ini("exclude = src/a.py\n")}, {}, ["mi", "src"]),
    ("mi_output_file", {"src/doc.py": DOC, RC: ini("output_file = out.txt\n")}, {}, ["mi", "-j", "src"]),
    ("mi_sort", {"src/doc.py": DOC}, {}, ["mi", "-s", "--sort", "src"]),
]
CASES += MI_CASES
if DEEP_OVER_MEASURED:
    CASES.append(DEEP_OVER)


def build(root, files):
    home = root / "home"
    proj = root / "proj"
    (proj / "src").mkdir(parents=True)
    home.mkdir()
    for name in ("a.py", "b.py"):
        (proj / "src" / name).write_text(SOURCE)
    for rel, data in files.items():
        path = home / rel[2:] if rel.startswith("~/") else proj / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        if data is None:
            path.mkdir()
        else:
            path.write_bytes(data)
    return proj, home


def normalize(stdout, command="cc"):
    """radon's `cc` JSON key order varies; compare those documents semantically.
    Other commands' JSON is byte-exact."""
    if command != "cc":
        return stdout
    try:
        return json.dumps(json.loads(stdout), sort_keys=True).encode()
    except ValueError:
        return stdout


# Both tools run with `RLIMIT_STACK` set to this (in KiB). How deep a TOML value radon can
# render before CPython's native stack overflows depends on the build and, for some builds, on
# that limit. Measured on macOS arm64: a standalone CPython 3.14.10 works at 34,720 levels and
# fails at 34,800 with the limit at 8 MiB, and works at 69,680 and fails at 69,760 with it at
# 16 MiB; Homebrew's CPython 3.14.8 works at 69,000 and fails at 70,000 with the limit at
# either (why it does not follow the limit was not looked into). oxradon's fixed limit is
# 69,700, so both tools get 16 MiB here; without that the `toml_deep_*` cases would depend on
# the shell that started the script.
STACK_KB = 16384


def stack_limit():
    """The stack limit to run the tools with. Stops the script when it cannot be set: a tool
    that never started must not count as a matching result."""
    want = STACK_KB * 1024
    _, hard = resource.getrlimit(resource.RLIMIT_STACK)
    if hard != resource.RLIM_INFINITY and hard < want:
        sys.exit(f"cannot give the tools a {STACK_KB} KiB stack: the hard limit is "
                 f"{hard // 1024} KiB (`ulimit -H -s`)")
    return want, hard


def set_stack():
    """In the child, before the tool starts."""
    resource.setrlimit(resource.RLIMIT_STACK, STACK)


def run_case(tool, case):
    name, files, env_extra, args = case
    root = Path(tempfile.mkdtemp(prefix=f"cfg-{name}-"))
    try:
        proj, home = build(root, files)
        env = {**RADON_ENV, "HOME": str(home), **env_extra}
        argv = args or ["cc", "src"]
        # A tool that cannot be started raises here, and ends the script.
        p = subprocess.run([tool, *argv], cwd=proj, env=env, preexec_fn=set_stack,
                           capture_output=True, stdin=subprocess.DEVNULL)
        out = proj / "out.txt"
        written = normalize(out.read_bytes(), argv[0]) if out.exists() else None
        return p.returncode, normalize(p.stdout, argv[0]), written, p.stderr.decode(errors="replace").strip().splitlines()[-1:]
    finally:
        shutil.rmtree(root)


def main():
    args = sys.argv[1:]
    only = None
    if "--only" in args:
        i = args.index("--only")
        only = args[i + 1]
        del args[i:i + 2]
    if not args:
        usage(__doc__)
    radon, *rest = (os.path.abspath(a) for a in args)
    rs = rest[0] if rest else None
    global STACK
    STACK = stack_limit()
    if only == DEEP_OVER[0] and not DEEP_OVER_MEASURED:
        sys.exit(f"{only} is compared on macOS arm64 only: radon's boundary is not measured here")
    selected = [case for case in CASES if only in (None, case[0])]
    if not selected:
        sys.exit(f"no case named {only!r}")
    failures = 0
    for case in selected:
        a = run_case(radon, case)
        if rs is None:
            print(f"{case[0]}: exit={a[0]} stdout={a[1][:160]!r} file={a[2]!r} stderr={a[3]}")
            continue
        b = run_case(rs, case)
        if a[:3] != b[:3]:
            failures += 1
            print(f"DIFF {case[0]}:\n  radon:    exit={a[0]} stdout={a[1][:200]!r} file={a[2]!r}\n"
                  f"  oxradon: exit={b[0]} stdout={b[1][:200]!r} file={b[2]!r}")
    if rs is not None:
        skipped = "" if DEEP_OVER_MEASURED or only else f" ({DEEP_OVER[0]} skipped: not macOS arm64)"
        print(f"{len(selected)} cases, {failures} differ{skipped}")
        print("PASS" if failures == 0 else "FAIL")
        sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
