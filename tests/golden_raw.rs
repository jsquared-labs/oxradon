//! Replay `radon raw` outputs recorded by `scripts/update_golden_raw.py`. raw's output order is
//! fixed and its error messages come from radon and CPython, so the comparison is byte for byte.

mod common;

use common::{golden, strings};
use serde_json::Value;

#[test]
fn raw_matches_radon() {
    let manifest: Value = serde_json::from_str(&golden("raw-manifest.json")).expect("manifest");
    let cases = manifest["cases"].as_object().expect("cases");
    assert!(!cases.is_empty());
    for (name, case) in cases {
        let flags = strings(&case["flags"]);
        let files = strings(&case["files"]);
        let args: Vec<&str> = ["raw"]
            .into_iter()
            .chain(flags.iter().chain(&files).map(String::as_str))
            .collect();
        let expected = golden(case["file"].as_str().expect("file name"));
        let actual = common::run(&args, b"", &[]);
        if common::walks(&files) {
            assert_eq!(
                common::walk_order_free(&actual),
                common::walk_order_free(&expected),
                "case `{name}` ({flags:?})"
            );
        } else {
            assert_eq!(actual, expected, "case `{name}` ({flags:?})");
        }
    }
}

/// `-O`: the listing goes to the file, per-file `ERROR` lines to stdout.
#[test]
fn output_file_takes_the_listing_and_stdout_the_errors() {
    let dir = common::scratch("raw-output-file");
    let out = dir.join("out.txt");
    let stdout = common::run(
        &[
            "raw",
            "-O",
            out.to_str().expect("UTF-8 path"),
            "tests/fixtures/raw/percent.py",
            "tests/fixtures/raw/unterminated.py",
        ],
        b"",
        &[],
    );
    assert_eq!(stdout, "    ERROR: SyntaxError at line: 3\n");
    let file = std::fs::read_to_string(&out).expect("output file");
    assert!(
        file.starts_with("tests/fixtures/raw/percent.py\n    LOC: 8\n"),
        "{file}"
    );
    assert!(
        file.ends_with("tests/fixtures/raw/unterminated.py\n"),
        "{file}"
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

/// stdin (`-`) is read as UTF-8 and analyzed like a file.
#[test]
fn stdin_is_analyzed() {
    let out = common::run(&["raw", "-j", "-"], b"# c\nx = 1\n", &[]);
    assert_eq!(
        out,
        "{\"-\": {\"loc\": 2, \"lloc\": 1, \"sloc\": 1, \"comments\": 1, \"multi\": 0, \
         \"blank\": 0, \"single_comments\": 1}}\n"
    );
}

/// `raw` takes `exclude`, `ignore` and `output_file` from radon's configuration files, recorded
/// from radon in scratch projects (run from the project, with `HOME` there too).
#[test]
fn raw_reads_configuration_like_radon() {
    let manifest: Value = serde_json::from_str(&golden("raw-manifest.json")).expect("manifest");
    common::replay_config(&manifest["config"], "raw-config", str::to_owned);
}

/// radon's argparse takes an unambiguous prefix of a long option (`--js`, `--out=`) and rejects
/// an ambiguous or unknown one with exit 2 (`--i`: `--ignore`, `--include-ipynb` or
/// `--ipynb-cells`). Checked against radon for these invocations.
#[test]
fn long_option_prefixes_follow_argparse() {
    let file = "tests/fixtures/raw/percent.py";
    let json = common::run(&["raw", "--json", file], b"", &[]);
    assert_eq!(common::run(&["raw", "--js", file], b"", &[]), json);
    let summary = common::run(&["raw", "--summary", file], b"", &[]);
    assert_eq!(common::run(&["raw", "--sum", file], b"", &[]), summary);
    for args in [
        ["raw", "--i", "x", file].as_slice(),
        &["raw", "--x", file],
        &["raw", "--jsonx", file],
    ] {
        let out = common::oxradon(args, b"", &[]);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
    assert_eq!(
        common::run(&["raw", "--ig", "x", file], b"", &[]),
        common::run(&["raw", file], b"", &[])
    );
}
