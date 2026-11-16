//! What the commands' reports share: one input's outcome, radon's two output
//! streams and their colors, its error lines, the `--min`/`--max` filter of
//! the ranked commands, and the frame of `--json` output. Each command's own
//! listing is in `<command>_report`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Write as _};
use std::hash::Hash;
use std::io::{self, Write};

use crate::cc::Rank;
use crate::colorama;
use crate::mi;
use crate::pyfmt::{format_without_args, path_text, py_upper, write_json_os_str, write_json_str};

/// One input, under the name radon reports it by.
pub(crate) struct FileReport<T> {
    pub(crate) name: OsString,
    pub(crate) outcome: Outcome<T>,
}

/// One input's analysis: a command's result, or why there is none.
pub(crate) enum Outcome<T> {
    Analyzed(T),
    /// Reading, decoding or parsing failed: radon reports it for this file.
    Failed(String),
    /// Opening failed: radon's whole run stops here (see [`split_at_abort`]).
    Aborted(String),
}

/// radon opens each file outside its per-file error handling, so the first
/// file it cannot open ends the run: earlier files are reported as usual and
/// the error is printed after them (and nothing else). Returns the reports
/// before that file, and its error.
pub(crate) fn split_at_abort<T>(reports: &[FileReport<T>]) -> (&[FileReport<T>], Option<&str>) {
    for (i, report) in reports.iter().enumerate() {
        if let Outcome::Aborted(message) = &report.outcome {
            return (&reports[..i], Some(message));
        }
    }
    (reports, None)
}

/// The two streams radon writes to. A failed write is handled differently
/// for each: on stdout it ends the run with CPython's exit status; on the
/// `-O` file it becomes radon's top-level `ERROR` line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    File,
}

/// A write that failed, and the stream it was for.
#[derive(Debug)]
pub(crate) struct WriteFailure {
    pub(crate) stream: Stream,
    pub(crate) error: io::Error,
}

/// One of radon's output streams, written a line at a time, with the colors
/// it gets.
///
/// Each [`Sink::line`] is one radon `log()` call, so one `write`: indent,
/// formatted message and newline together. On a stripping colorama stdout
/// (color off) each line goes through [`colorama::strip`] whole, as it does
/// in radon.
pub(crate) struct Sink<W> {
    out: W,
    stream: Stream,
    palette: &'static Palette,
    strip: bool,
    line: String,
}

impl<W: Write> Sink<W> {
    /// radon's stdout. radon's color constants are always colorama's codes;
    /// with color off, colorama strips each write afterwards. So lines are
    /// built with the codes and stripped, as radon's are: colorama's OSC
    /// removal uses stale offsets (see [`colorama::strip`]), which the codes
    /// shift.
    pub(crate) fn stdout(out: W, color: bool) -> Self {
        Self::new(out, Stream::Stdout, true, !color)
    }

    /// The `-O` file, which colorama never touches.
    pub(crate) fn file(out: W, color: bool) -> Self {
        Self::new(out, Stream::File, color, false)
    }

    fn new(out: W, stream: Stream, color: bool, strip: bool) -> Self {
        Self {
            out,
            stream,
            palette: Palette::new(color),
            strip,
            line: String::new(),
        }
    }

    /// The styles this sink's lines get.
    pub(crate) fn palette(&self) -> &'static Palette {
        self.palette
    }

    fn fail(&self, error: io::Error) -> WriteFailure {
        WriteFailure {
            stream: self.stream,
            error,
        }
    }

    /// Write one line; `args` includes its newline.
    ///
    /// # Errors
    /// The write error, tagged with this sink's stream.
    pub(crate) fn line(&mut self, args: fmt::Arguments<'_>) -> Result<(), WriteFailure> {
        self.line.clear();
        let _ = self.line.write_fmt(args); // writing to a String cannot fail
        let text = if self.strip {
            colorama::strip(&self.line)
        } else {
            Cow::Borrowed(self.line.as_str())
        };
        let mut result = self.out.write_all(text.as_bytes());
        // colorama flushes every write. A line buffer flushes at each
        // newline already, so only a line whose newline was stripped needs
        // it, or it would sit in the buffer and its failure go unseen.
        if self.strip && !text.ends_with('\n') {
            result = result.and_then(|()| self.out.flush());
        }
        result.map_err(|e| self.fail(e))
    }

    /// Write output that never contains an escape (JSON escapes control
    /// characters), bypassing [`colorama::strip`].
    ///
    /// # Errors
    /// The write error, tagged with this sink's stream.
    pub(crate) fn raw(
        &mut self,
        write: impl FnOnce(&mut W) -> io::Result<()>,
    ) -> Result<(), WriteFailure> {
        write(&mut self.out).map_err(|e| self.fail(e))
    }

    /// # Errors
    /// The flush error, tagged with this sink's stream.
    pub(crate) fn flush(&mut self) -> Result<(), WriteFailure> {
        self.out.flush().map_err(|e| self.fail(e))
    }
}

/// radon's top-level error line (`radon.main` calling `log_error`). It goes
/// to stdout even when the report goes to an `-O` file.
///
/// # Errors
/// The failed write.
pub(crate) fn write_abort<W: Write>(
    stdout: &mut Sink<W>,
    message: &str,
) -> Result<(), WriteFailure> {
    write_error(stdout, "", message)
}

/// radon's `log_error`: `{BRIGHT}{RED}ERROR{RESET}: {message}`, indented.
pub(crate) fn write_error<W: Write>(
    out: &mut Sink<W>,
    indent: &str,
    message: &str,
) -> Result<(), WriteFailure> {
    let p = out.palette;
    let message = terminal_text(message);
    out.line(format_args!(
        "{indent}{}{}ERROR{}: {message}\n",
        p.bright, p.red, p.reset
    ))
}

/// A per-file `ERROR` line, indented under the file's name. radon's
/// `log_result` calls `log_error` without the stream, so it goes to `stdout`
/// even when the report goes to an `-O` file.
///
/// # Errors
/// The failed write.
pub(crate) fn write_file_error<R: Write, S: Write>(
    out: &mut Sink<R>,
    stdout: &mut Sink<S>,
    message: &str,
) -> Result<(), WriteFailure> {
    match out.stream {
        Stream::Stdout => write_error(out, "    ", message),
        Stream::File => write_error(stdout, "    ", message),
    }
}

/// A file's name on a line of its own. radon logs the name as the format
/// string ([`terminal_text`]); `suffix` is `hal`'s colon, which radon adds
/// before formatting and which changes nothing about the braces.
///
/// # Errors
/// The failed write.
pub(crate) fn write_name<W: Write>(
    out: &mut Sink<W>,
    name: &OsStr,
    suffix: &str,
) -> Result<(), WriteFailure> {
    let name = path_text(name);
    out.line(format_args!("{}{suffix}\n", terminal_text(&name)))
}

/// Every command's `--json` output: one object keyed by file name, followed
/// by a newline. radon builds a `dict` from the files it lists, so a repeated
/// name keeps its first position and takes its last entry. A file that could
/// not be analyzed is `{"error": message}`; `write_result` writes the value
/// of one that was.
///
/// # Errors
/// Propagates write errors.
pub(crate) fn write_json_files<'a, W: Write, T: 'a>(
    out: &mut W,
    listed: impl IntoIterator<Item = &'a FileReport<T>>,
    mut write_result: impl FnMut(&mut W, &T) -> io::Result<()>,
) -> io::Result<()> {
    out.write_all(b"{")?;
    let files = dict_entries(listed.into_iter().map(|r| (r.name.as_os_str(), r)));
    write_joined(out, files, |out, (name, report)| {
        write_json_os_str(out, name)?;
        out.write_all(b": ")?;
        match &report.outcome {
            Outcome::Analyzed(result) => write_result(out, result),
            Outcome::Failed(message) | Outcome::Aborted(message) => {
                out.write_all(b"{\"error\": ")?;
                write_json_str(out, message)?;
                out.write_all(b"}")
            }
        }
    })?;
    out.write_all(b"}\n")
}

/// Write `items` with `", "` between them, as `json.dumps` separates the
/// members of a list or an object.
///
/// # Errors
/// Propagates write errors.
pub(crate) fn write_joined<W: Write, T>(
    out: &mut W,
    items: impl IntoIterator<Item = T>,
    mut write_item: impl FnMut(&mut W, T) -> io::Result<()>,
) -> io::Result<()> {
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.write_all(b", ")?;
        }
        write_item(out, item)?;
    }
    Ok(())
}

/// radon's dicts keyed by name: a repeated name keeps its first position and
/// takes its last value. Returns the entries in that order.
pub(crate) fn dict_entries<K: Hash + Eq + Copy, V: Copy>(
    entries: impl IntoIterator<Item = (K, V)>,
) -> Vec<(K, V)> {
    let entries: Vec<(K, V)> = entries.into_iter().collect();
    let mut latest: HashMap<K, V> = entries.iter().copied().collect();
    (entries.into_iter())
        .filter_map(|(key, _)| latest.remove(&key).map(|value| (key, value)))
        .collect()
}

/// radon's `--min`/`--max`: the ranks to show. Compared as strings, as radon
/// does.
pub(crate) struct RankFilter {
    min: String,
    max: String,
}

impl RankFilter {
    pub(crate) fn new(min: &str, max: &str) -> Self {
        Self {
            min: py_upper(min),
            max: py_upper(max),
        }
    }

    pub(crate) fn contains(&self, rank: Rank) -> bool {
        self.contains_str(rank.as_str())
    }

    /// `min <= rank <= max`, as Python compares strings (by code point).
    pub(crate) fn contains_str(&self, rank: &str) -> bool {
        (self.min.as_str()..=self.max.as_str()).contains(&rank)
    }
}

/// ANSI styles used by radon (via colorama), or none.
pub(crate) struct Palette {
    bright: &'static str,
    reset: &'static str,
    green: &'static str,
    yellow: &'static str,
    red: &'static str,
    magenta: &'static str,
    cyan: &'static str,
    white: &'static str,
}

impl Palette {
    pub(crate) const PLAIN: Self = Self {
        bright: "",
        reset: "",
        green: "",
        yellow: "",
        red: "",
        magenta: "",
        cyan: "",
        white: "",
    };

    pub(crate) const ANSI: Self = Self {
        bright: "\x1b[1m",
        reset: "\x1b[0m",
        green: "\x1b[32m",
        yellow: "\x1b[33m",
        red: "\x1b[31m",
        magenta: "\x1b[35m",
        cyan: "\x1b[36m",
        white: "\x1b[37m",
    };

    /// The styles for a stream that gets color or not.
    pub(crate) fn new(color: bool) -> &'static Self {
        if color { &Self::ANSI } else { &Self::PLAIN }
    }

    /// `RANKS_COLORS`: A and B green, C and D yellow, E and F red.
    pub(crate) fn rank(&self, rank: Rank) -> &'static str {
        match rank {
            Rank::A | Rank::B => self.green,
            Rank::C | Rank::D => self.yellow,
            Rank::E | Rank::F => self.red,
        }
    }

    /// `MI_RANKS`: A green, B yellow, C red.
    pub(crate) fn mi_rank(&self, rank: mi::Rank) -> &'static str {
        match rank {
            mi::Rank::A => self.green,
            mi::Rank::B => self.yellow,
            mi::Rank::C => self.red,
        }
    }

    /// colorama's `Style.BRIGHT`.
    pub(crate) fn bright(&self) -> &'static str {
        self.bright
    }

    /// colorama's `Style.RESET_ALL`.
    pub(crate) fn reset(&self) -> &'static str {
        self.reset
    }

    /// `LETTERS_COLORS`: functions magenta, classes cyan, methods white.
    pub(crate) fn letter(&self, letter: char) -> &'static str {
        match letter {
            'F' => self.magenta,
            'C' => self.cyan,
            _ => self.white,
        }
    }
}

/// radon's color rule for a stream, from the environment.
pub(crate) fn color_from_env(is_terminal: bool) -> bool {
    color_enabled(std::env::var_os("COLOR").as_deref(), is_terminal)
}

/// `radon.cli.colors.color_enabled`: `COLOR=yes` forces color, and `auto` (or
/// unset) colors only a terminal. Anything else, including a value that is not
/// UTF-8 (which Python sees with surrogates), means no color.
fn color_enabled(value: Option<&OsStr>, is_terminal: bool) -> bool {
    match value.map(OsStr::as_encoded_bytes) {
        Some(b"yes") => true,
        None | Some(b"auto") => is_terminal,
        Some(_) => false,
    }
}

/// radon prints file names and error messages through `str.format()` with no
/// arguments, so `{{` and `}}` collapse. Text that would make radon crash
/// there (a lone brace) is printed as is.
pub(crate) fn terminal_text(text: &str) -> Cow<'_, str> {
    format_without_args(text).unwrap_or(Cow::Borrowed(text))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::color_enabled;

    #[test]
    fn color_follows_radon_rule() {
        let yes = Some(OsStr::new("yes"));
        let auto = Some(OsStr::new("auto"));
        for terminal in [false, true] {
            assert!(color_enabled(yes, terminal));
            assert_eq!(color_enabled(auto, terminal), terminal);
            assert_eq!(color_enabled(None, terminal), terminal);
            for other in ["", "no", "Yes", "AUTO"] {
                assert!(!color_enabled(Some(OsStr::new(other)), terminal), "{other}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_color_means_no_color() {
        use std::os::unix::ffi::OsStrExt;
        // Python sees `'\udcff'`, which is neither `yes` nor `auto`.
        assert!(!color_enabled(Some(OsStr::from_bytes(b"\xff")), true));
    }
}
