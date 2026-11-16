//! radon-compatible output for `hal`: the terminal listing and `--json`.

use std::io::{self, Write};

use crate::hal::{Halstead, Report};
use crate::pyfmt::write_json_str;
use crate::report::{
    FileReport, Outcome, Sink, WriteFailure, dict_entries, write_file_error, write_joined,
    write_json_files, write_name,
};

/// Write radon's `hal` terminal listing to `out`, the report's stream.
///
/// A file that could not be read or parsed gets a per-file `ERROR` line under
/// its name, as in `cc`. radon itself stops the whole run there (its
/// formatter reads the error's `total`), a documented difference.
///
/// # Errors
/// The first failed write; nothing is written after it.
pub(crate) fn write_terminal<R: Write, S: Write>(
    out: &mut Sink<R>,
    stdout: &mut Sink<S>,
    reports: &[FileReport<Halstead>],
    by_function: bool,
) -> Result<(), WriteFailure> {
    for report in reports {
        // radon logs `"{}:".format(name)`, which `log` formats once more.
        write_name(out, &report.name, ":")?;
        match &report.outcome {
            Outcome::Analyzed(halstead) if by_function => {
                for (name, function) in &halstead.functions {
                    out.line(format_args!("    {name}:\n"))?;
                    write_fields(out, function, "        ")?;
                }
            }
            Outcome::Analyzed(halstead) => write_fields(out, &halstead.total, "    ")?,
            Outcome::Failed(message) | Outcome::Aborted(message) => {
                write_file_error(out, stdout, message)?;
            }
        }
    }
    Ok(())
}

/// `hal_report_to_terminal`: one `key: value` line per field.
fn write_fields<W: Write>(
    out: &mut Sink<W>,
    report: &Report,
    indent: &str,
) -> Result<(), WriteFailure> {
    for (key, value) in report.fields() {
        out.line(format_args!("{indent}{key}: {value}\n"))?;
    }
    Ok(())
}

/// Write radon's `hal --json` output, followed by a newline: every file, with
/// its `total` and its `functions` (whether or not `-f` was given).
///
/// # Errors
/// Propagates write errors.
pub(crate) fn write_json(out: &mut impl Write, reports: &[FileReport<Halstead>]) -> io::Result<()> {
    write_json_files(out, reports, |out, halstead| {
        out.write_all(b"{\"total\": ")?;
        write_report(out, &halstead.total)?;
        out.write_all(b", \"functions\": {")?;
        let functions = dict_entries(halstead.functions.iter().map(|(n, r)| (n.as_str(), r)));
        write_joined(out, functions, |out, (name, function)| {
            write_json_str(out, name)?;
            out.write_all(b": ")?;
            write_report(out, function)
        })?;
        out.write_all(b"}}")
    })
}

/// `HalsteadReport._asdict()`, as `json.dumps` writes it.
fn write_report(out: &mut impl Write, report: &Report) -> io::Result<()> {
    out.write_all(b"{")?;
    write_joined(out, report.fields(), |out, (key, value)| {
        write!(out, "\"{key}\": {value}")
    })?;
    out.write_all(b"}")
}
