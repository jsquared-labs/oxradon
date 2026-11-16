//! Replay `radon mi` outputs and tables recorded by `scripts/update_golden_mi.py`.
//!
//! Outputs are compared byte for byte, except syntax-error messages (`... (<unknown>, line N)`),
//! which CPython and Ruff word differently (a COMPATIBILITY.md row); raw's own errors are exact.

mod common;

use common::{golden, strings};
use oxradon::hal::PyNum;
use oxradon::mi;
use serde_json::Value;

const SYNTAX_ERROR: &str = "<syntax error>";

fn manifest() -> Value {
    serde_json::from_str(&golden("mi-manifest.json")).expect("manifest")
}

/// Whether `message` is a syntax error from the parser: `... (<unknown>, line N)`.
fn is_syntax_error(message: &str) -> bool {
    const MARKER: &str = "(<unknown>, line ";
    message
        .strip_suffix(')')
        .and_then(|rest| rest.rfind(MARKER).map(|i| &rest[i + MARKER.len()..]))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Mask syntax-error messages, in `ERROR:` lines and in JSON `"error"` values. JSON is masked
/// in the text, not parsed, so that the comparison stays byte for byte (key order included).
fn mask(text: &str) -> String {
    if text.starts_with('{') {
        let mut out = String::new();
        let mut rest = text;
        while let Some(i) = rest.find("{\"error\": \"") {
            let start = i + 11;
            out.push_str(&rest[..start]);
            let body = &rest[start..];
            let mut end = 0;
            let mut escaped = false;
            for (j, c) in body.char_indices() {
                match (escaped, c) {
                    (true, _) => escaped = false,
                    (false, '\\') => escaped = true,
                    (false, '"') => {
                        end = j;
                        break;
                    }
                    _ => {}
                }
            }
            let message: String =
                serde_json::from_str(&format!("\"{}\"", &body[..end])).expect("JSON string");
            out.push_str(if is_syntax_error(&message) {
                SYNTAX_ERROR
            } else {
                &body[..end]
            });
            rest = &body[end..];
        }
        out.push_str(rest);
        return out;
    }
    // `    ERROR: msg`, colored or not.
    text.split_inclusive('\n')
        .map(|line| {
            let plain = strip_ansi(line);
            match plain.strip_prefix("    ERROR: ") {
                Some(message) if is_syntax_error(message.trim_end_matches('\n')) => {
                    let keep = line.len() - message.len();
                    format!(
                        "{}{SYNTAX_ERROR}\n",
                        &line[..line.find(": ").map_or(keep, |i| i + 2)]
                    )
                }
                _ => line.to_owned(),
            }
        })
        .collect()
}

/// Remove `ESC[...m` sequences.
fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find('\x1b') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        rest = rest.find('m').map_or("", |m| &rest[m + 1..]);
    }
    out.push_str(rest);
    out
}

#[test]
fn mi_matches_radon() {
    let manifest = manifest();
    let cases = manifest["cases"].as_object().expect("cases");
    assert!(cases.len() > 20);
    for (name, case) in cases {
        let flags = strings(&case["flags"]);
        let files = strings(&case["files"]);
        let env: Vec<(String, String)> = (case["env"].as_object().expect("env").iter())
            .map(|(k, v)| (k.clone(), v.as_str().expect("value").to_owned()))
            .collect();
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let args: Vec<&str> = ["mi"]
            .into_iter()
            .chain(flags.iter().chain(&files).map(String::as_str))
            .collect();
        let expected = mask(&golden(case["file"].as_str().expect("file name")));
        let actual = mask(&common::run(&args, b"", &env));
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

/// `mi` takes `mi_min`, `mi_max`, `multi`, `show_mi`, `exclude` and `output_file` from radon's
/// configuration files, recorded from radon in scratch projects (run from the project, with
/// `HOME` there too).
#[test]
fn mi_reads_configuration_like_radon() {
    // radon leaks color codes into `-O` files; oxradon colors them only with `COLOR=yes` (a
    // README row), so the recorded file is compared without them.
    common::replay_config(&manifest()["config"], "mi-config", strip_ansi);
}

/// stdin keeps its CRs in radon (no newline translation): fixtures with CRLF and lone-CR line
/// endings, recorded from `radon mi -j [-m] -`.
#[test]
fn stdin_with_carriage_returns_matches_radon() {
    let manifest = manifest();
    let cases = manifest["stdin"].as_array().expect("stdin cases");
    assert_eq!(cases.len(), 12);
    for case in cases {
        let flags = strings(&case["flags"]);
        let args: Vec<&str> = ["mi", "-j"]
            .into_iter()
            .chain(flags.iter().map(String::as_str))
            .chain(["-"])
            .collect();
        let stdin = case["stdin"].as_str().expect("stdin");
        assert_eq!(
            common::run(&args, stdin.as_bytes(), &[]),
            case["stdout"].as_str().expect("stdout"),
            "{flags:?} {stdin:?}"
        );
    }
}

/// A recorded Python number: `["int", "0"]` or `["float", repr]`.
fn number(v: &Value) -> PyNum {
    let kind = v[0].as_str().expect("type");
    let text = v[1].as_str().expect("repr");
    match kind {
        "int" => PyNum::Int(text.parse().expect("int")),
        _ => PyNum::Float(text.parse().expect("float")),
    }
}

fn float(v: &Value) -> f64 {
    v.as_str().expect("repr").parse().expect("float")
}

fn same(a: PyNum, b: PyNum) -> bool {
    match (a, b) {
        (PyNum::Int(x), PyNum::Int(y)) => x == y,
        (PyNum::Float(x), PyNum::Float(y)) => x.to_bits() == y.to_bits(),
        _ => false,
    }
}

/// `radon.metrics.mi_compute` on generated inputs, compared by bits. On Linux, the results that
/// differ there come from the overlay recorded there (glibc or musl).
#[test]
fn compute_matches_radon_bit_for_bit() {
    let cases: Vec<Value> = serde_json::from_str(&golden("mi-compute.json")).expect("JSON");
    assert!(cases.len() > 1000);
    let overlay = common::libm_overlay("mi-compute");
    for (i, case) in cases.iter().enumerate() {
        let volume = number(&case[0]);
        let complexity = case[1].as_u64().expect("complexity");
        let lloc = case[2].as_u64().expect("lloc");
        let comments = number(&case[3]).as_f64();
        let expected = float(overlay.get(&i.to_string()).unwrap_or(&case[4]));
        let actual = mi::compute(volume, complexity, lloc, comments);
        assert_eq!(actual.to_bits(), expected.to_bits(), "{case}: {actual:?}");
    }
}

/// `radon.metrics.mi_parameters` and `mi_visit` on generated valid modules and the fixtures,
/// with multi-line strings counted and not. `null` is radon's error. On Linux, the results that
/// differ there come from the overlay recorded there (glibc or musl).
#[test]
fn parameters_match_radon_bit_for_bit() {
    let cases: Vec<Value> = serde_json::from_str(&golden("mi-params.json")).expect("JSON");
    assert!(cases.len() > 1000);
    let overlay = common::libm_overlay("mi-params");
    for (i, case) in cases.iter().enumerate() {
        let text = case[0].as_str().expect("text");
        let multi = case[1].as_bool().expect("multi");
        let expected = overlay.get(&i.to_string()).unwrap_or(&case[2]);
        let options = mi::Options { count_multi: multi };
        let actual = mi::parameters(text, &options);
        if expected.is_null() {
            assert!(actual.is_err(), "{text:?}: {actual:?}");
            continue;
        }
        let p = actual.unwrap_or_else(|e| panic!("{text:?}: {e}"));
        assert!(
            same(p.volume, number(&expected["volume"])),
            "{text:?}: {p:?} {expected}"
        );
        assert_eq!(
            p.complexity,
            expected["complexity"].as_u64().expect("int"),
            "{text:?}"
        );
        assert_eq!(p.lloc, expected["lloc"].as_u64().expect("int"), "{text:?}");
        let comments = number(&expected["comments"]).as_f64();
        assert_eq!(p.comments.to_bits(), comments.to_bits(), "{text:?}: {p:?}");
        let options = mi::Options { count_multi: multi };
        let mi = mi::analyze(text, &options).expect("analyzed").mi;
        assert_eq!(mi.to_bits(), float(&expected["mi"]).to_bits(), "{text:?}");
    }
}
