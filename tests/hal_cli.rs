//! `hal` command-line behaviours checked against radon 6.0.1 on CPython 3.14.
//! Each expected value was observed from radon itself, except where a test
//! says it checks a documented difference.

use std::path::{Path, PathBuf};

mod common;

use common::{command_in, output, scratch};

/// The report radon prints for `x = 1 + 2` and for `return a and b`.
const FIELDS: [&str; 12] = [
    "h1: 1",
    "h2: 2",
    "N1: 1",
    "N2: 2",
    "vocabulary: 3",
    "length: 3",
    "calculated_length: 2.0",
    "volume: 4.754887502163469",
    "difficulty: 0.5",
    "effort: 2.3774437510817346",
    "time: 0.1320802083934297",
    "bugs: 0.0015849625007211565",
];

const JSON_REPORT: &str = "{\"h1\": 1, \"h2\": 2, \"N1\": 1, \"N2\": 2, \"vocabulary\": 3, \
    \"length\": 3, \"calculated_length\": 2.0, \"volume\": 4.754887502163469, \"difficulty\": 0.5, \
    \"effort\": 2.3774437510817346, \"time\": 0.1320802083934297, \"bugs\": 0.0015849625007211565}";

fn fields(indent: &str) -> String {
    FIELDS.iter().fold(String::new(), |mut out, f| {
        out.push_str(indent);
        out.push_str(f);
        out.push('\n');
        out
    })
}

/// `a.py` (no function), `b.py` (a syntax error), `c.py` (one function).
fn project(name: &str) -> PathBuf {
    let dir = scratch(name);
    for (file, code) in [
        ("a.py", "x = 1 + 2\n"),
        ("b.py", "def (\n"),
        ("c.py", "def f():\n    return a and b\n"),
    ] {
        std::fs::write(dir.join(file), code).expect("write fixture");
    }
    dir
}

/// Run `oxradon` in `dir` ([`common::command_in`]), requiring success;
/// returns stdout.
fn run_in(dir: &Path, args: &[&str], stdin: &[u8]) -> String {
    let out = output(command_in(dir).args(args), stdin);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("output is UTF-8")
}

/// oxradon's message for `b.py`, from its own JSON (CPython words it
/// differently, a documented difference).
fn b_error(dir: &Path) -> String {
    let json: serde_json::Value =
        serde_json::from_str(&run_in(dir, &["hal", "-j", "b.py"], b"")).expect("JSON");
    json["b.py"]["error"].as_str().expect("an error").to_owned()
}

#[test]
fn json_lists_each_file_once_in_order() {
    let dir = project("hal_json_order");
    let error = serde_json::to_string(&b_error(&dir)).expect("a JSON string");
    // radon: a repeated name keeps its first position; `-` among other paths
    // is not read; the error is per file.
    let out = run_in(
        &dir,
        &["hal", "-j", "a.py", "b.py", "a.py", "-", "c.py", "-"],
        b"y = -x\n",
    );
    let expected = format!(
        "{{\"a.py\": {{\"total\": {JSON_REPORT}, \"functions\": {{}}}}, \
         \"b.py\": {{\"error\": {error}}}, \
         \"c.py\": {{\"total\": {JSON_REPORT}, \"functions\": {{\"f\": {JSON_REPORT}}}}}}}\n"
    );
    assert_eq!(out, expected);
}

#[test]
fn a_syntax_error_is_reported_per_file() {
    // A documented difference: radon prints `b.py:` and then
    // `ERROR: 'dict' object has no attribute 'total'`, analyzing nothing after.
    let dir = project("hal_per_file_error");
    let error = b_error(&dir);
    let whole = run_in(&dir, &["hal", "a.py", "b.py", "c.py"], b"");
    let expected = format!(
        "a.py:\n{}b.py:\n    ERROR: {error}\nc.py:\n{}",
        fields("    "),
        fields("    ")
    );
    assert_eq!(whole, expected);
    let by_function = run_in(&dir, &["hal", "-f", "a.py", "b.py", "c.py"], b"");
    let expected = format!(
        "a.py:\nb.py:\n    ERROR: {error}\nc.py:\n    f:\n{}",
        fields("        ")
    );
    assert_eq!(by_function, expected);
}

#[test]
fn per_file_errors_go_to_stdout_under_output_file() {
    // As in `cc`: the report goes to the file, `ERROR` lines to stdout.
    let dir = project("hal_output_file");
    let error = b_error(&dir);
    let out = run_in(&dir, &["hal", "-O", "out.txt", "a.py", "b.py"], b"");
    assert_eq!(out, format!("    ERROR: {error}\n"));
    let file = std::fs::read_to_string(dir.join("out.txt")).expect("the -O file");
    assert_eq!(file, format!("a.py:\n{}b.py:\n", fields("    ")));
    // radon: JSON errors stay in the JSON.
    let out = run_in(&dir, &["hal", "-j", "-O", "out.txt", "b.py"], b"");
    assert_eq!(out, "");
    let file = std::fs::read_to_string(dir.join("out.txt")).expect("the -O file");
    assert!(file.starts_with("{\"b.py\": {\"error\": "), "{file}");
}

#[test]
fn stdin_among_other_paths_is_not_read() {
    let dir = project("hal_stdin");
    let out = run_in(&dir, &["hal", "a.py", "-", "c.py", "-"], b"y = -x\n");
    assert_eq!(
        out,
        format!("a.py:\n{}c.py:\n{}", fields("    "), fields("    "))
    );
}

#[cfg(unix)]
#[test]
fn a_file_that_cannot_be_opened_aborts_the_run() {
    let dir = project("hal_unreadable");
    let locked = dir.join("locked.py");
    std::fs::write(&locked, "x = 1\n").expect("write fixture");
    if !common::make_unreadable(&locked) {
        return;
    }
    let abort = "ERROR: [Errno 13] Permission denied: 'locked.py'\n";
    // radon: earlier files are printed, then the error, exit 0.
    let out = run_in(&dir, &["hal", "a.py", "locked.py", "c.py"], b"");
    assert_eq!(out, format!("a.py:\n{}{abort}", fields("    ")));
    // radon: in JSON mode, only the error.
    let out = run_in(&dir, &["hal", "-j", "a.py", "locked.py", "c.py"], b"");
    assert_eq!(out, abort);
}

#[test]
fn configured_functions_makes_the_flag_turn_it_off() {
    let dir = project("hal_config_functions");
    std::fs::write(dir.join("radon.cfg"), "[radon]\nfunctions = true\n").expect("write config");
    let by_function = format!("c.py:\n    f:\n{}", fields("        "));
    let whole = format!("c.py:\n{}", fields("    "));
    assert_eq!(run_in(&dir, &["hal", "c.py"], b""), by_function);
    assert_eq!(run_in(&dir, &["hal", "-f", "c.py"], b""), whole);
    assert_eq!(run_in(&dir, &["hal", "-f", "-f", "c.py"], b""), whole);
}

#[test]
fn notebook_and_repeated_flags_are_accepted() {
    // radon (without nbformat): the notebook flags change nothing; `-j -f` is
    // the same JSON as `-j`.
    let dir = project("hal_flags");
    let plain = run_in(&dir, &["hal", "-j", "c.py"], b"");
    let flags = [
        "hal",
        "-j",
        "-j",
        "--include-ipynb",
        "--ipynb-cells",
        "-f",
        "-f",
        "c.py",
    ];
    assert_eq!(run_in(&dir, &flags, b""), plain);
}

#[test]
fn long_option_prefixes_follow_argparse() {
    // radon (argparse `allow_abbrev`): `--fun` is `--functions`, `--out=` is
    // `--output-file=`; `--i` could be `--ignore` or `--include-ipynb`, a usage
    // error (exit 2).
    let dir = project("hal_prefixes");
    let by_function = run_in(&dir, &["hal", "-f", "c.py"], b"");
    assert_eq!(run_in(&dir, &["hal", "--fun", "c.py"], b""), by_function);
    assert_eq!(
        run_in(&dir, &["hal", "--out=out.txt", "--fun", "c.py"], b""),
        ""
    );
    let file = std::fs::read_to_string(dir.join("out.txt")).expect("the -O file");
    assert_eq!(file, by_function);
    let ambiguous = output(command_in(&dir).args(["hal", "--i", "x", "c.py"]), b"");
    assert_eq!(ambiguous.status.code(), Some(2));
    assert_eq!(ambiguous.stdout, [] as [u8; 0]);
    // The same holds for `cc`: `--show-c` matches two options, `--show-com` one.
    let ambiguous = output(command_in(&dir).args(["cc", "--show-c", "c.py"]), b"");
    assert_eq!(ambiguous.status.code(), Some(2));
    let out = run_in(&dir, &["cc", "--show-com", "c.py"], b"");
    assert_eq!(out, "c.py\n    F 1:0 f - A (2)\n");
    // radon's `cc --i` could be `--ignore`, `--include-ipynb` or `--ipynb-cells`.
    let ambiguous = output(command_in(&dir).args(["cc", "--i", "x", "c.py"]), b"");
    assert_eq!(ambiguous.status.code(), Some(2));
    // radon (without nbformat): `cc`'s notebook flags change nothing.
    let plain = run_in(&dir, &["cc", "-j", "c.py"], b"");
    let flags = ["cc", "-j", "--include-ipynb", "--ipynb-cells", "c.py"];
    assert_eq!(run_in(&dir, &flags, b""), plain);
}
