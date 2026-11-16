//! radon-compatible output for `mi`: the terminal listing
//! (`MIHarvester.to_terminal`, with `--sort`) and `--json`.

use std::cmp::Ordering;
use std::io::{self, Write};

use crate::mi::Mi;
use crate::pyfmt::{float_repr, path_text};
use crate::report::{
    FileReport, Outcome, RankFilter, Sink, WriteFailure, write_abort, write_file_error,
    write_json_files, write_name,
};

/// `MIHarvester.filtered_results`: errors always, results whose rank is within
/// `--min`/`--max`.
fn filtered<'a>(
    reports: &'a [FileReport<Mi>],
    filter: &'a RankFilter,
) -> impl Iterator<Item = &'a FileReport<Mi>> {
    reports.iter().filter(|r| match &r.outcome {
        Outcome::Analyzed(m) => filter.contains_str(m.rank.as_str()),
        Outcome::Failed(_) | Outcome::Aborted(_) => true,
    })
}

/// Write radon's `mi` terminal listing to `out`, the report's stream. Per-file
/// `ERROR` lines, and `--sort`'s crash, go to `stdout`.
///
/// With `sort`, radon sorts by `mi` with `sorted(..., key=lambda el:
/// el[1]['mi'])`, so a listed error entry raises `KeyError('mi')` before
/// anything is printed: radon's `main` then prints `ERROR: 'mi'` alone (and
/// exits 0).
///
/// # Errors
/// The first failed write; nothing is written after it.
pub(crate) fn write_terminal<R: Write, S: Write>(
    out: &mut Sink<R>,
    stdout: &mut Sink<S>,
    reports: &[FileReport<Mi>],
    filter: &RankFilter,
    show: bool,
    sort: bool,
    aborted: bool,
) -> Result<(), WriteFailure> {
    // With `--sort` radon reads every input before it prints anything, so an
    // input that ended the run leaves only that input's error.
    if sort && aborted {
        return Ok(());
    }
    let mut listed: Vec<&FileReport<Mi>> = filtered(reports, filter).collect();
    if sort {
        let score = |r: &FileReport<Mi>| match &r.outcome {
            Outcome::Analyzed(m) => Some(m.mi),
            Outcome::Failed(_) | Outcome::Aborted(_) => None,
        };
        if listed.iter().any(|r| score(r).is_none()) {
            return write_abort(stdout, "'mi'");
        }
        // A stable sort with Python's `<` (scores are never NaN).
        listed.sort_by(|a, b| score(a).partial_cmp(&score(b)).unwrap_or(Ordering::Equal));
    }
    let palette = out.palette();
    for report in listed {
        match &report.outcome {
            Outcome::Analyzed(m) => {
                // The name is a `str.format` argument here, so it is printed as
                // it is.
                let name = path_text(&report.name);
                let suffix = if show { shown(m.mi) } else { String::new() };
                out.line(format_args!(
                    "{name} - {}{}{suffix}{}\n",
                    palette.mi_rank(m.rank),
                    m.rank.as_str(),
                    palette.reset()
                ))?;
            }
            Outcome::Failed(message) | Outcome::Aborted(message) => {
                write_name(out, &report.name, "")?;
                write_file_error(out, stdout, message)?;
            }
        }
    }
    Ok(())
}

/// `-s`: `' ({0:.2f})'.format(mi)`, correctly rounded with ties to even, as
/// Rust formats too.
fn shown(mi: f64) -> String {
    format!(" ({mi:.2})")
}

/// Write radon's `mi --json` output, followed by a newline. `--sort` does not
/// apply to it.
///
/// # Errors
/// Propagates write errors.
pub(crate) fn write_json(
    out: &mut impl Write,
    reports: &[FileReport<Mi>],
    filter: &RankFilter,
) -> io::Result<()> {
    // `dict(self.filtered_results)`: filtered first, then repeated names
    // merged.
    write_json_files(out, filtered(reports, filter), |out, m| {
        write!(
            out,
            "{{\"mi\": {}, \"rank\": \"{}\"}}",
            float_repr(m.mi),
            m.rank.as_str()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Python's `' ({0:.2f})'.format(x)`, recorded by
    /// `scripts/update_golden_mi.py`.
    #[test]
    fn shown_matches_python() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/mi-format.json");
        let text = std::fs::read_to_string(path).expect("mi-format.json");
        let cases: Vec<(String, String)> = serde_json::from_str(&text).expect("JSON");
        assert!(cases.len() > 1000);
        for (x, expected) in &cases {
            let value: f64 = x.parse().expect("a float repr");
            assert_eq!(&shown(value), expected, "{x}");
        }
    }
}
