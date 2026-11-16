//! Replay outputs recorded from the real radon (see `scripts/update_golden.py`).
//! Codec and `fnmatch` recordings are replayed by unit tests in `src/`.

mod common;

use common::{golden, strings};
use serde_json::Value;

/// Error messages come from different parsers, so they are replaced by this
/// marker on both sides (as in `scripts/update_golden.py`).
const ERROR: &str = "<error>";
const ERROR_PREFIX: &str = "    ERROR: ";

fn run_cc(flags: &[String], fixtures: &[String]) -> String {
    let args: Vec<&str> = ["cc"]
        .into_iter()
        .chain(flags.iter().chain(fixtures).map(String::as_str))
        .collect();
    common::run(&args, b"", &[])
}

fn normalize_json(text: &str) -> Value {
    let mut value: Value = serde_json::from_str(text).expect("valid JSON");
    for entry in value
        .as_object_mut()
        .expect("top-level object")
        .values_mut()
    {
        if let Some(error) = entry.get_mut("error") {
            *error = Value::String(ERROR.into());
        }
    }
    value
}

/// Byte-exact apart from error messages: line endings are kept as they are.
fn normalize_text(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            if line.starts_with(ERROR_PREFIX) {
                format!("{ERROR_PREFIX}{ERROR}\n")
            } else {
                line.to_owned()
            }
        })
        .collect()
}

fn manifest() -> Value {
    serde_json::from_str(&golden("manifest.json")).expect("valid manifest")
}

/// `hal` recordings have no errors, so they are compared byte for byte.
#[test]
fn hal_matches_radon() {
    let manifest = manifest();
    let fixtures = strings(&manifest["hal_fixtures"]);
    let cases = manifest["hal_cases"].as_object().expect("hal cases");
    assert!(!cases.is_empty());
    for (name, case) in cases {
        let flags = strings(&case["flags"]);
        let args: Vec<&str> = ["hal"]
            .into_iter()
            .chain(flags.iter().chain(&fixtures).map(String::as_str))
            .collect();
        let expected = golden(case["file"].as_str().expect("file name"));
        assert_eq!(
            common::run(&args, b"", &[]),
            expected,
            "case `{name}` ({flags:?})"
        );
    }
}

#[test]
fn cc_matches_radon() {
    let manifest = manifest();
    let fixtures = strings(&manifest["fixtures"]);
    let cases = manifest["cases"].as_object().expect("cases");
    assert!(!cases.is_empty());
    for (name, case) in cases {
        let flags = strings(&case["flags"]);
        let expected = golden(case["file"].as_str().expect("file name"));
        let actual = run_cc(&flags, &fixtures);
        if case["format"] == "json" {
            assert_eq!(
                normalize_json(&actual),
                normalize_json(&expected),
                "case `{name}` ({flags:?})"
            );
        } else {
            assert_eq!(
                normalize_text(&actual),
                expected,
                "case `{name}` ({flags:?})"
            );
        }
    }
}
