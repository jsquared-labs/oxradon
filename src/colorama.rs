//! What colorama does to radon's stdout off Windows.
//!
//! radon calls `colorama.init(strip=not color_enabled())`. With color on,
//! stdout is left alone. With color off, every `write` goes through
//! `AnsiToWin32.write_and_convert`, which removes ANSI sequences, including
//! any inside file names or error messages, and flushes. radon makes one
//! write per output line, so [`strip`] is applied to whole lines. colorama
//! also resets the terminal at exit ([`exit_reset`]).

use std::borrow::Cow;
use std::io::{self, Write};
use std::sync::LazyLock;

use regex::Regex;

/// `AnsiToWin32.ANSI_CSI_RE`. Like Python's `re` on `str`, `\d` is any
/// Unicode decimal digit (the `regex` crate's default).
static CSI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x01?\x1b\[(?:\d|;)*[a-zA-Z]\x02?").expect("valid pattern"));

/// `AnsiToWin32.ANSI_OSC_RE`. The body may span lines.
static OSC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x01?\x1b\][^\x07]*\x07\x02?").expect("valid pattern"));

/// colorama's `atexit` hook: it wraps the original stdout afresh, and that
/// wrapper writes `ESC[0m` whenever it takes stdout for a terminal, whatever
/// `COLOR` says (unstripped). It also takes stdout for one whenever
/// `PYCHARM_HOSTED` is set, to any value. CPython runs it on every exit but a
/// signal; oxradon does not run it on Ctrl-C either (a documented divergence).
///
/// # Errors
/// The failed write, which CPython reports as a failed final flush.
pub(crate) fn exit_reset(stdout_is_terminal: bool) -> io::Result<()> {
    if stdout_is_terminal || std::env::var_os("PYCHARM_HOSTED").is_some() {
        let mut stdout = io::stdout().lock();
        stdout.write_all(b"\x1b[0m")?;
        stdout.flush()?;
    }
    Ok(())
}

/// What one `write(text)` puts on a stripping colorama stdout.
///
/// One quirk is deliberate: see [`convert_osc`]. colorama also calls
/// `winterm.set_title` for an OSC with one `;` whose first parameter is in
/// `'02'`, and `winterm` is `None` off Windows, so radon crashes there; that
/// sequence is stripped like any other (a documented divergence).
pub(crate) fn strip(text: &str) -> Cow<'_, str> {
    if !text.contains('\x1b') {
        return Cow::Borrowed(text);
    }
    Cow::Owned(CSI.replace_all(&convert_osc(text), "").into_owned())
}

/// `AnsiToWin32.convert_osc`: it finds every match in the original text, but
/// removes each one with `text = text[:start] + text[end:]` on the text
/// already shortened by earlier removals. So after the first, the removed
/// range is shifted (and clipped like a Python slice). Spans are code points.
fn convert_osc(text: &str) -> Cow<'_, str> {
    let code_points = |byte: usize| text[..byte].chars().count();
    let spans: Vec<(usize, usize)> = OSC
        .find_iter(text)
        .map(|m| (code_points(m.start()), code_points(m.end())))
        .collect();
    if spans.is_empty() {
        return Cow::Borrowed(text);
    }
    let mut chars: Vec<char> = text.chars().collect();
    for (start, end) in spans {
        let len = chars.len();
        chars.drain(start.min(len)..end.min(len));
    }
    Cow::Owned(chars.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::strip;

    /// Each output was recorded from colorama itself:
    /// `AnsiToWin32(io.StringIO(), convert=False, strip=True).write(text)`.
    #[test]
    fn strip_matches_colorama() {
        let cases = [
            // The second OSC is removed at the first one's stale span, which
            // then takes `.py\n` instead; the second OSC survives.
            ("\x1b]a\x07\x1b]b\x07.py\n", "\x1b]b\x07"),
            // Stale spans count code points, not bytes.
            (
                "\u{e9}\u{20ac}\x1b]a\x07\x1b]bb\x07xyz\n",
                "\u{e9}\u{20ac}\x1b]bb",
            ),
            // An OSC body may span lines.
            ("a\x1b]x\ny\x07b\n", "ab\n"),
            // `\d` is any Unicode decimal digit.
            ("x\x1b[\u{661}m.py\n", "x.py\n"),
            // The optional `\x01` and `\x02` go with the sequence.
            ("y\x01\x1b[1;2m\x02z.py\n", "yz.py\n"),
            // Empty parameters.
            ("w\x1b[;m.py\n", "w.py\n"),
            ("v\x1b[m.py\n", "v.py\n"),
            ("\x1b[1m\x1b[31mERROR\x1b[0m: x\n", "ERROR: x\n"),
            ("\x1b[1;2;3H\x1b]x\x07z", "z"),
            // An unfinished sequence stays.
            ("\x1b[12", "\x1b[12"),
            ("plain.py\n", "plain.py\n"),
        ];
        for (text, want) in cases {
            assert_eq!(strip(text), want, "{text:?}");
        }
    }

    #[test]
    fn text_without_escapes_is_borrowed() {
        assert!(matches!(strip("a.py\n"), std::borrow::Cow::Borrowed(_)));
    }
}
