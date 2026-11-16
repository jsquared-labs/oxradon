//! `mi` command-line behaviours checked against radon 6.0.1 on CPython 3.14. Each expected
//! value was observed from radon itself (scratch files `ok.py`, a copy of
//! `tests/fixtures/mi/rank_b.py`; `bad.py`, `x =`; `denied.py`, unreadable).

use std::path::Path;

mod common;

use common::{command_in, output, scratch};

const OK: &str = "ok.py - B\n";
const OK_JSON: &str = "\"ok.py\": {\"mi\": 13.821487415762208, \"rank\": \"B\"}";
const DENIED: &str = "ERROR: [Errno 13] Permission denied: 'denied.py'\n";

/// A scratch project with `ok.py`, `bad.py` and an unreadable `denied.py`. `None` (with a note on
/// stderr) when the unreadable file can be read anyway (running as root).
fn project(name: &str) -> Option<std::path::PathBuf> {
    let dir = scratch(name);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mi/rank_b.py");
    std::fs::copy(fixture, dir.join("ok.py")).expect("copy");
    std::fs::write(dir.join("bad.py"), "x =\n").expect("write");
    let denied = dir.join("denied.py");
    std::fs::write(&denied, "x = 1\n").expect("write");
    common::make_unreadable(&denied).then_some(dir)
}

fn run_in(dir: &Path, args: &[&str]) -> (String, i32) {
    let out = output(command_in(dir).args(args), b"");
    let stdout = String::from_utf8(out.stdout).expect("UTF-8");
    (stdout, out.status.code().expect("exit status"))
}

/// Stdout with the syntax error's wording (Ruff's, a COMPATIBILITY.md row) replaced.
fn masked(stdout: &str) -> String {
    stdout
        .lines()
        .map(|l| {
            if l.starts_with("    ERROR: ") && l.ends_with("(<unknown>, line 1)") {
                "    ERROR: <syntax error>\n".to_owned()
            } else {
                format!("{l}\n")
            }
        })
        .collect()
}

/// `--sort` drains every input before sorting: an error entry raises `KeyError('mi')`, so radon
/// prints `ERROR: 'mi'` alone and exits 0; an unreadable input raises first, so only its error
/// is printed. `-j` ignores `--sort`, and `-O` leaves the file empty.
#[test]
fn sort_prints_only_radon_crash() {
    let Some(dir) = project("mi-sort") else {
        return;
    };
    assert_eq!(
        run_in(&dir, &["mi", "ok.py", "bad.py", "--sort"]),
        ("ERROR: 'mi'\n".into(), 0)
    );
    assert_eq!(
        run_in(
            &dir,
            &["mi", "--sort", "-n", "A", "-x", "A", "ok.py", "bad.py"]
        )
        .0,
        "ERROR: 'mi'\n"
    );
    assert_eq!(
        run_in(&dir, &["mi", "ok.py", "bad.py", "denied.py", "--sort"]),
        (DENIED.into(), 0)
    );
    assert_eq!(
        run_in(
            &dir,
            &["mi", "ok.py", "bad.py", "denied.py", "--sort", "-j"]
        ),
        (DENIED.into(), 0)
    );
    let (stdout, status) = run_in(
        &dir,
        &[
            "mi",
            "-O",
            "out.txt",
            "ok.py",
            "bad.py",
            "denied.py",
            "--sort",
        ],
    );
    assert_eq!((stdout.as_str(), status), (DENIED, 0));
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).expect("out.txt"),
        ""
    );
    let (stdout, _) = run_in(&dir, &["mi", "-O", "out.txt", "--sort", "ok.py", "bad.py"]);
    assert_eq!(stdout, "ERROR: 'mi'\n");
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).expect("out.txt"),
        ""
    );
    let (stdout, _) = run_in(&dir, &["mi", "ok.py", "bad.py", "--sort", "-j"]);
    assert!(
        stdout.starts_with(&format!("{{{OK_JSON}, \"bad.py\": {{\"error\": ")),
        "{stdout}"
    );
    // Repeated names are sorted like any other entry (stable).
    assert_eq!(
        run_in(&dir, &["mi", "ok.py", "ok.py", "-s", "--sort"]).0,
        "ok.py - B (13.82)\nok.py - B (13.82)\n"
    );
}

/// Unsorted, earlier files are listed before an unreadable input's error; with `-O` the listing
/// goes to the file and per-file `ERROR` lines to stdout.
#[test]
fn abort_and_output_file_split() {
    let Some(dir) = project("mi-abort") else {
        return;
    };
    let (stdout, status) = run_in(&dir, &["mi", "ok.py", "bad.py", "denied.py"]);
    assert_eq!(status, 0);
    assert_eq!(
        masked(&stdout),
        format!("{OK}bad.py\n    ERROR: <syntax error>\n{DENIED}")
    );
    let (stdout, _) = run_in(&dir, &["mi", "-O", "out.txt", "ok.py", "bad.py"]);
    assert_eq!(masked(&stdout), "    ERROR: <syntax error>\n");
    let file = std::fs::read_to_string(dir.join("out.txt")).expect("out.txt");
    assert_eq!(file, format!("{OK}bad.py\n"));
    // An error outside the rank range is still listed.
    let (stdout, _) = run_in(&dir, &["mi", "-n", "A", "-x", "A", "ok.py", "bad.py"]);
    assert_eq!(masked(&stdout), "bad.py\n    ERROR: <syntax error>\n");
}

/// radon's argparse takes an unambiguous prefix of a long option and rejects an ambiguous one
/// with exit 2: `--m` (`--min`, `--max`, `--multi`), `--s` (`--show`, `--sort`), `--i`
/// (`--ignore`, `--include-ipynb`, `--ipynb-cells`). Checked against radon for these invocations.
#[test]
fn long_option_prefixes_follow_argparse() {
    let Some(dir) = project("mi-prefixes") else {
        return;
    };
    let same = |a: &[&str], b: &[&str]| assert_eq!(run_in(&dir, a), run_in(&dir, b), "{a:?}");
    same(&["mi", "--mu", "-s", "ok.py"], &["mi", "-m", "-s", "ok.py"]);
    same(&["mi", "--sh", "ok.py"], &["mi", "-s", "ok.py"]);
    same(
        &["mi", "--so", "ok.py", "bad.py"],
        &["mi", "--sort", "ok.py", "bad.py"],
    );
    same(&["mi", "--mi", "B", "ok.py"], &["mi", "-n", "B", "ok.py"]);
    same(&["mi", "--ma", "A", "ok.py"], &["mi", "-x", "A", "ok.py"]);
    same(&["mi", "--js", "ok.py"], &["mi", "-j", "ok.py"]);
    same(&["mi", "--in", "ok.py"], &["mi", "ok.py"]);
    same(&["mi", "--ip", "ok.py"], &["mi", "ok.py"]);
    same(&["mi", "--ig", "x", "ok.py"], &["mi", "ok.py"]);
    same(&["mi", "--ex", "x", "ok.py"], &["mi", "ok.py"]);
    for args in [
        ["mi", "--m", "ok.py"].as_slice(),
        &["mi", "--s", "ok.py"],
        &["mi", "--i", "ok.py"],
        &["mi", "--multix", "ok.py"],
    ] {
        let (stdout, status) = run_in(&dir, args);
        assert_eq!((stdout.as_str(), status), ("", 2), "{args:?}");
    }
}

/// A repeated flag is the flag once (mando's `store_false`/`store_true`), not a toggle.
#[test]
fn repeated_flags_are_not_toggles() {
    let dir = scratch("mi-repeated");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mi/docstrings.py");
    std::fs::copy(fixture, dir.join("doc.py")).expect("copy");
    assert_eq!(
        run_in(&dir, &["mi", "-s", "doc.py"]).0,
        "doc.py - A (88.10)\n"
    );
    assert_eq!(
        run_in(&dir, &["mi", "-s", "-m", "-m", "doc.py"]).0,
        "doc.py - A (98.08)\n"
    );
    assert_eq!(
        run_in(&dir, &["mi", "-s", "-s", "doc.py"]).0,
        "doc.py - A (88.10)\n"
    );
}

/// With color off, radon builds each line with colorama's codes and colorama strips it; its OSC
/// removal uses offsets that go stale after the first removal, so the rank codes change what
/// survives. Recorded from radon (`COLOR=no`, the file holds `x = 1 + 2`).
#[test]
fn color_off_strips_lines_built_with_codes() {
    let dir = scratch("mi-osc");
    let name = "\x1b]longlong\x07\x1b]b\x07.py";
    std::fs::write(dir.join(name), "x = 1 + 2\n").expect("write");
    assert_eq!(run_in(&dir, &["mi", name]).0, "\x1b]b\x07.py - \x1bA\n");
    assert_eq!(
        run_in(&dir, &["mi", "-s", name]).0,
        "\x1b]b\x07.py - \x1bA (95.12)\n"
    );
}
