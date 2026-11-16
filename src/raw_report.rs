//! radon-compatible output for `raw`: the terminal listing
//! (`RawHarvester.to_terminal`), the `-s` totals, and `--json`.

use std::io::{self, Write};

use crate::raw::Module;
use crate::report::{
    FileReport, Outcome, Sink, WriteFailure, write_file_error, write_json_files, write_name,
};

/// The seven counters in radon's terminal order (`RawHarvester.headers`).
fn fields(m: &Module) -> [(&'static str, u64); 7] {
    [
        ("LOC", m.loc),
        ("LLOC", m.lloc),
        ("SLOC", m.sloc),
        ("Comments", m.comments),
        ("Single comments", m.single_comments),
        ("Multi", m.multi),
        ("Blank", m.blank),
    ]
}

/// Write radon's `raw` terminal listing to `out`, the report's stream, and with
/// `summary` the totals after it.
///
/// # Errors
/// The first failed write; nothing is written after it.
pub(crate) fn write_terminal<R: Write, S: Write>(
    out: &mut Sink<R>,
    stdout: &mut Sink<S>,
    reports: &[FileReport<Module>],
    summary: bool,
    aborted: bool,
) -> Result<(), WriteFailure> {
    let mut total = Module::default();
    for report in reports {
        write_name(out, &report.name, "")?;
        match &report.outcome {
            Outcome::Analyzed(m) => {
                for (header, value) in fields(m) {
                    out.line(format_args!("    {header}: {value}\n"))?;
                }
                write_ratios(out, m.comments, m.multi, m.loc, m.sloc)?;
                total += *m;
            }
            Outcome::Failed(message) | Outcome::Aborted(message) => {
                write_file_error(out, stdout, message)?;
            }
        }
    }
    if summary && !aborted {
        // radon's `sum_metrics` has no key until a file is analyzed, and
        // `_get('LOC', 1) or 1` makes an absent or zero denominator 1, as
        // `percent` does for zero.
        out.line(format_args!("** Total **\n"))?;
        for (header, value) in fields(&total) {
            out.line(format_args!("    {header}: {value}\n"))?;
        }
        write_ratios(out, total.comments, total.multi, total.loc, total.sloc)?;
    }
    Ok(())
}

/// `- Comment Stats` and its three `{:.0%}` ratios, each denominator with
/// radon's `or 1`.
fn write_ratios<W: Write>(
    out: &mut Sink<W>,
    comments: u64,
    multi: u64,
    loc: u64,
    sloc: u64,
) -> Result<(), WriteFailure> {
    out.line(format_args!("    - Comment Stats\n"))?;
    let ratios = [
        ("(C % L)", comments, loc),
        ("(C % S)", comments, sloc),
        ("(C + M % L)", comments + multi, loc),
    ];
    for (label, n, d) in ratios {
        out.line(format_args!("        {label}: {}\n", percent(n, d)))?;
    }
    Ok(())
}

/// Python's `'{:.0%}'.format(n / (float(d) or 1))`: the quotient times 100 in
/// binary64, then fixed-point with no decimals, correctly rounded with ties to
/// even (as Rust formats too).
#[allow(clippy::cast_precision_loss)] // radon's counters are far below 2**53
fn percent(n: u64, d: u64) -> String {
    let d = if d == 0 { 1.0 } else { d as f64 };
    format_percent(n as f64 / d)
}

fn format_percent(x: f64) -> String {
    format!("{:.0}%", x * 100.0)
}

/// Write radon's `raw --json` output, followed by a newline. `-s` adds nothing
/// to it.
///
/// # Errors
/// Propagates write errors.
pub(crate) fn write_json(out: &mut impl Write, reports: &[FileReport<Module>]) -> io::Result<()> {
    write_json_files(out, reports, |out, m| {
        write!(
            out,
            "{{\"loc\": {}, \"lloc\": {}, \"sloc\": {}, \"comments\": {}, \"multi\": {}, \
             \"blank\": {}, \"single_comments\": {}}}",
            m.loc, m.lloc, m.sloc, m.comments, m.multi, m.blank, m.single_comments
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Python's `'{:.0%}'.format(x)`, recorded by
    /// `scripts/update_golden_raw.py` (`x` by `repr`).
    #[test]
    fn percent_matches_python() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/raw-percent.json");
        let text = std::fs::read_to_string(path).expect("raw-percent.json");
        let cases: Vec<(String, String)> = serde_json::from_str(&text).expect("JSON");
        for (x, expected) in &cases {
            let value: f64 = x.parse().expect("a float repr");
            assert_eq!(&format_percent(value), expected, "{x}");
        }
    }
}
