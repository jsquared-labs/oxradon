//! radon-compatible output for `cc`: the terminal listing and `--json`.

use std::fmt;
use std::io::{self, Write};

use crate::cc::{Block, Class, Function, Rank};
use crate::pyfmt::{float_repr, write_json_str};
use crate::report::{
    FileReport, Outcome, RankFilter, Sink, WriteFailure, write_file_error, write_joined,
    write_json_files, write_name,
};

/// Terminal-listing switches.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TerminalOptions {
    /// `-s`: print the score next to the rank.
    pub(crate) show_complexity: bool,
    /// `-a`: print the average of the listed blocks.
    pub(crate) average: bool,
    /// `--total-average`: print the average of all blocks, listed or not.
    pub(crate) total_average: bool,
}

/// `-s`: the score after the rank, or nothing.
struct Score(Option<u32>);

impl fmt::Display for Score {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(score) => write!(f, " ({score})"),
            None => Ok(()),
        }
    }
}

/// Write radon's terminal listing to `out`, the report's stream. When an
/// input ended the run (`aborted`), radon never reaches the averages.
///
/// radon's `log_result` calls `log_error` without the stream, so per-file
/// `ERROR` lines go to `stdout` even when the report goes to an `-O` file.
///
/// # Errors
/// The first failed write; nothing is written after it.
pub(crate) fn write_terminal<R: Write, S: Write>(
    out: &mut Sink<R>,
    stdout: &mut Sink<S>,
    reports: &[FileReport<Vec<Block>>],
    filter: &RankFilter,
    options: TerminalOptions,
    aborted: bool,
) -> Result<(), WriteFailure> {
    let p = out.palette();
    let mut total = 0.0;
    let mut analyzed = 0u32;
    for report in reports {
        let blocks = match &report.outcome {
            Outcome::Analyzed(blocks) => blocks,
            Outcome::Failed(message) | Outcome::Aborted(message) => {
                write_name(out, &report.name, "")?;
                write_file_error(out, stdout, message)?;
                continue;
            }
        };
        let mut header_written = false;
        for block in blocks {
            let rank = block.rank();
            let listed = filter.contains(rank);
            if listed || options.total_average {
                total += f64::from(block.complexity());
                analyzed += 1;
            }
            if !listed {
                continue;
            }
            if !header_written {
                write_name(out, &report.name, "")?;
                header_written = true;
            }
            let letter = block.letter();
            let score = Score(options.show_complexity.then(|| block.complexity()));
            out.line(format_args!(
                "    {}{}{letter} {}{}:{} {} - {}{rank}{score}{}\n",
                p.bright(),
                p.letter(letter),
                p.reset(),
                block.lineno(),
                block.col_offset(),
                block.fullname(),
                p.rank(rank),
                p.reset(),
            ))?;
        }
    }
    if (options.average || options.total_average) && analyzed > 0 && !aborted {
        let average = total / f64::from(analyzed);
        let rank = Rank::of(average);
        // radon logs the count with a leading newline, as one write.
        out.line(format_args!(
            "\n{analyzed} blocks (classes, functions, methods) analyzed.\n"
        ))?;
        out.line(format_args!(
            "Average complexity: {}{rank} ({}){}\n",
            p.rank(rank),
            float_repr(average),
            p.reset()
        ))?;
    }
    Ok(())
}

/// Write radon's `--json` output, followed by a newline. Output is streamed
/// straight from the blocks; key order within objects is fixed (radon's
/// varies run to run, as it comes from a Python `set`).
///
/// # Errors
/// Propagates write errors.
pub(crate) fn write_json(
    out: &mut impl Write,
    reports: &[FileReport<Vec<Block>>],
    filter: &RankFilter,
) -> io::Result<()> {
    // A file appears if it failed or has at least one block in range.
    let listed_files = reports.iter().filter(|report| match &report.outcome {
        Outcome::Failed(_) | Outcome::Aborted(_) => true,
        Outcome::Analyzed(blocks) => blocks.iter().any(|b| filter.contains(b.rank())),
    });
    write_json_files(out, listed_files, |out, blocks| {
        let listed_blocks = blocks.iter().filter(|b| filter.contains(b.rank()));
        write_list(out, listed_blocks, |out, block| match block {
            Block::Function(f) => write_function(out, f),
            Block::Class(c) => write_class(out, c),
        })
    })
}

fn write_function(out: &mut impl Write, f: &Function) -> io::Result<()> {
    let kind = if f.is_method() { "method" } else { "function" };
    write_common_fields(
        out,
        kind,
        f.rank(),
        &f.name,
        [f.lineno, f.col_offset, f.endline],
    )?;
    if let Some(class) = &f.classname {
        out.write_all(b", \"classname\": ")?;
        write_json_str(out, class)?;
    }
    write!(out, ", \"complexity\": {}, \"closures\": ", f.complexity)?;
    write_list(out, &f.closures, write_function)?;
    out.write_all(b"}")
}

fn write_class(out: &mut impl Write, c: &Class) -> io::Result<()> {
    write_common_fields(
        out,
        "class",
        c.rank(),
        &c.name,
        [c.lineno, c.col_offset, c.endline],
    )?;
    write!(out, ", \"complexity\": {}, \"methods\": ", c.complexity())?;
    write_list(out, &c.methods, write_function)?;
    out.write_all(b"}")
}

/// Open a block object with the fields functions and classes share.
fn write_common_fields(
    out: &mut impl Write,
    kind: &str,
    rank: Rank,
    name: &str,
    [lineno, col_offset, endline]: [u32; 3],
) -> io::Result<()> {
    write!(
        out,
        "{{\"type\": \"{kind}\", \"rank\": \"{rank}\", \"name\": "
    )?;
    write_json_str(out, name)?;
    write!(
        out,
        ", \"lineno\": {lineno}, \"col_offset\": {col_offset}, \"endline\": {endline}"
    )
}

fn write_list<W: Write, T>(
    out: &mut W,
    items: impl IntoIterator<Item = T>,
    write_item: impl FnMut(&mut W, T) -> io::Result<()>,
) -> io::Result<()> {
    out.write_all(b"[")?;
    write_joined(out, items, write_item)?;
    out.write_all(b"]")
}
