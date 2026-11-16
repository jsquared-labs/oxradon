//! Raw line metrics, ported from radon's `radon/raw.py` (`analyze`).
//!
//! radon strips every line, then regroups lines by re-tokenizing a growing
//! buffer of them with CPython's `tokenize` until it no longer fails, and
//! counts each group. That loop is ported with its quirks:
//! - "lines" are `str.splitlines()` lines, so a form feed or U+2028 inside a
//!   string splits it;
//! - a lone string on one line counts as a single-line comment, and a lone
//!   multi-line string as `multi` lines, but an f-string or t-string never
//!   does;
//! - logical lines come from splitting on `;` and comparing the last `:`'s
//!   position with the piece's length.
//!
//! The tokenizer resumes a growing buffer instead of starting over, so tokens
//! already finished are never lexed again; only a token still open at the end
//! (a long string, say) is rescanned as lines are added.

use std::ops::AddAssign;

use crate::error::Error;
use crate::pyfmt::py_strip;
use crate::rawtok::{Step, Tok, Tokenizer};

/// radon's `Module` namedtuple: a file's raw metrics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Module {
    /// Lines of code: `sloc + blank + multi + single_comments`.
    pub loc: u64,
    /// Logical lines of code.
    pub lloc: u64,
    /// Source lines of code.
    pub sloc: u64,
    /// `#` comments (a line with code and a comment counts here and in `sloc`).
    pub comments: u64,
    /// Non-empty lines of multi-line strings standing alone, taken as
    /// docstrings (their empty lines count in `blank`). Never an f-string or
    /// t-string.
    pub multi: u64,
    /// Empty (or whitespace-only) lines.
    pub blank: u64,
    /// Lines holding only a comment, or only a one-line string (never an
    /// f-string or t-string).
    pub single_comments: u64,
}

/// Adds every counter: radon's `-s` totals.
impl AddAssign for Module {
    fn add_assign(&mut self, other: Self) {
        self.loc += other.loc;
        self.lloc += other.lloc;
        self.sloc += other.sloc;
        self.comments += other.comments;
        self.multi += other.multi;
        self.blank += other.blank;
        self.single_comments += other.single_comments;
    }
}

/// Analyze Python source: radon's `raw.analyze`.
///
/// No syntax check is made; only CPython's tokenizer decides how lines group.
///
/// ```
/// let m = oxradon::raw::analyze("# setup\nx = 1  # one\n\ny = 2\n")?;
///
/// assert_eq!(m.loc, 4);
/// assert_eq!(m.sloc, 2);
/// assert_eq!(m.blank, 1);
/// // Both comments count; only the first has a line to itself.
/// assert_eq!((m.comments, m.single_comments), (2, 1));
/// # Ok::<(), oxradon::Error>(())
/// ```
///
/// # Errors
/// radon's `SyntaxError at line: N` when a group never tokenizes, N being the
/// line it starts on.
pub fn analyze(text: &str) -> Result<Module, Error> {
    let lines: Vec<&str> = splitlines(text).map(py_strip).collect();
    let mut module = Module::default();
    let mut i = 0;
    while i < lines.len() {
        let mut end = i + 1;
        let mut buffer = String::from(lines[i]);
        let mut tokenizer = Tokenizer::default();
        // radon retokenizes the growing buffer from scratch; `resume` gives the
        // same answers without starting over, and says when no longer buffer
        // can succeed (radon then runs out of lines, still failing, and reports
        // the group's first line).
        loop {
            match tokenizer.resume(&buffer) {
                Step::Done => break,
                Step::Failed { at_eof: true } if end < lines.len() => {
                    buffer.push('\n');
                    buffer.push_str(lines[end]);
                    end += 1;
                }
                Step::Failed { .. } => return Err(group_error(i)),
            }
        }
        module.add_group(&tokenizer.into_tokens(), &lines[i..end]);
        i = end;
    }
    module.loc = module.sloc + module.blank + module.multi + module.single_comments;
    Ok(module)
}

/// radon's loop exactly as written: a fresh tokenization of each buffer. The
/// oracle for [`analyze`]'s resumable tokenizing.
#[cfg(test)]
fn analyze_literal(text: &str) -> Result<Module, Error> {
    let lines: Vec<&str> = splitlines(text).map(py_strip).collect();
    let mut module = Module::default();
    let mut i = 0;
    while i < lines.len() {
        let mut end = i + 1;
        let mut buffer = String::from(lines[i]);
        let tokens = loop {
            if let Ok(tokens) = crate::rawtok::tokenize(&buffer) {
                break tokens;
            }
            let Some(next) = lines.get(end) else {
                return Err(group_error(i));
            };
            buffer.push('\n');
            buffer.push_str(next);
            end += 1;
        };
        module.add_group(&tokens, &lines[i..end]);
        i = end;
    }
    module.loc = module.sloc + module.blank + module.multi + module.single_comments;
    Ok(module)
}

/// radon's error for a group of lines that never tokenizes, by the index of
/// the group's first line.
fn group_error(first_line: usize) -> Error {
    Error::new(format!("SyntaxError at line: {}", first_line + 1))
}

impl Module {
    /// One group's tokens and lines.
    fn add_group(&mut self, tokens: &[Tok], lines: &[&str]) {
        self.comments += tally(tokens.iter().filter(|t| **t == Tok::Comment));
        let non_empty = tally(lines.iter().filter(|l| !l.is_empty()));
        let empty = tally(lines.iter().filter(|l| l.is_empty()));
        // `is_single_token`: one token, then only newlines and the end marker.
        let rest_is_trivial = tokens[1..]
            .iter()
            .all(|t| matches!(t, Tok::Newline | Tok::EndMarker));
        match tokens[0] {
            Tok::Comment | Tok::String { multiline: false } if rest_is_trivial => {
                self.single_comments += 1;
            }
            Tok::String { multiline: true } if rest_is_trivial => {
                self.multi += non_empty;
                self.blank += empty;
            }
            _ => {
                self.sloc += non_empty;
                self.blank += empty;
            }
        }
        self.lloc += logical(tokens);
    }
}

fn tally<T>(items: impl Iterator<Item = T>) -> u64 {
    items.map(|_| 1).sum()
}

/// radon's `_logical`: the logical lines in one group.
fn logical(tokens: &[Tok]) -> u64 {
    tokens
        .split(|t| *t == Tok::Semi)
        .map(|piece| {
            let kept: Vec<Tok> = (piece.iter().copied())
                .filter(|t| !matches!(t, Tok::Comment | Tok::Newline))
                .collect();
            match kept.iter().rposition(|t| *t == Tok::Colon) {
                // `2 - (token_pos == len(processed) - 2)`, in signed
                // arithmetic.
                Some(colon) => 2 - u64::from(colon + 2 == kept.len()),
                None if kept.iter().all(|t| *t == Tok::EndMarker) => 0,
                None => 1,
            }
        })
        .sum()
}

/// `str.splitlines()`: no empty last line, `\r\n` is one break.
fn splitlines(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let Some((at, c)) = rest.char_indices().find(|&(_, c)| is_line_break(c)) else {
            return Some(std::mem::take(&mut rest));
        };
        let line = &rest[..at];
        let mut after = at + c.len_utf8();
        if c == '\r' && rest[after..].starts_with('\n') {
            after += 1;
        }
        rest = &rest[after..];
        Some(line)
    })
}

/// The line boundaries of `str.splitlines()` (CPython 3.14, Unicode 16.0).
fn is_line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\x0b'
            | '\x0c'
            | '\x1c'
            | '\x1d'
            | '\x1e'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;

    fn metrics(text: &str) -> [u64; 7] {
        let m = analyze(text).expect("analyzes");
        [
            m.loc,
            m.lloc,
            m.sloc,
            m.comments,
            m.multi,
            m.blank,
            m.single_comments,
        ]
    }

    #[test]
    fn counts_follow_radon() {
        // loc, lloc, sloc, comments, multi, blank, single_comments
        assert_eq!(metrics(""), [0; 7]);
        assert_eq!(metrics("\n"), [1, 0, 0, 0, 0, 1, 0]);
        assert_eq!(metrics("# c\nx = 1  # d\n"), [2, 1, 1, 2, 0, 0, 1]);
        assert_eq!(metrics("'''a\n\nb'''\n"), [3, 1, 0, 0, 2, 1, 0]);
        assert_eq!(
            metrics("if x: pass\nif y:\n    pass\n"),
            [3, 4, 3, 0, 0, 0, 0]
        );
        assert_eq!(metrics("x = 1; y = 2\nx;\n"), [2, 3, 2, 0, 0, 0, 0]);
        assert_eq!(metrics("f\"\"\"{x:\n#hi\n}\"\"\""), [3, 2, 3, 0, 0, 0, 0]);
        assert_eq!(
            analyze("a = 1\n'''x\n"),
            Err(Error::new("SyntaxError at line: 2"))
        );
    }

    #[test]
    fn logical_pieces() {
        use Tok::{Colon, EndMarker, Newline, Other, Semi};
        // `if x: pass; y`: the first piece has exactly one token after its
        // colon.
        let tokens = [Other, Other, Colon, Other, Semi, Other, Newline, EndMarker];
        assert_eq!(logical(&tokens), 2);
        // A piece of just `:` (len - 2 = -1) counts 2.
        assert_eq!(logical(&[Colon, Semi, EndMarker]), 2);
        assert_eq!(logical(&[Other, Semi, Semi, Other, Newline, EndMarker]), 2);
    }

    /// The resumable tokenizing gives the literal loop's answer on every
    /// recorded source and tokenizer buffer, and on every fixture.
    #[test]
    fn resuming_matches_the_literal_loop() {
        let root = env!("CARGO_MANIFEST_DIR");
        let read = |name: &str| {
            std::fs::read_to_string(format!("{root}/tests/golden/{name}")).expect(name)
        };
        let lines: Vec<(String, Vec<String>)> =
            serde_json::from_str(&read("raw-lines.json")).expect("JSON");
        let tokens: Vec<(String, Option<String>)> =
            serde_json::from_str(&read("raw-tokens.json")).expect("JSON");
        let mut texts: Vec<String> = lines.into_iter().map(|(t, _)| t).collect();
        texts.extend(tokens.into_iter().map(|(t, _)| t));
        for dir in ["tests/fixtures", "tests/fixtures/raw"] {
            for entry in std::fs::read_dir(format!("{root}/{dir}")).expect("fixtures") {
                if let Ok(text) = std::fs::read_to_string(entry.expect("entry").path()) {
                    texts.push(text);
                }
            }
        }
        // Long groups: a big bracketed literal with multi-line strings,
        // unclosed at the end too.
        let mut big = String::new();
        for i in 0..300 {
            let _ = writeln!(big, "    'k{i}': \"\"\"line\n\n  # text {i}\n\"\"\",");
        }
        texts.push(format!("x = {{\n{big}}}\ny = 1\n"));
        texts.push(format!("x = {{\n{big}"));
        for text in &texts {
            assert_eq!(analyze(text), analyze_literal(text), "{text:?}");
        }
    }

    /// `str.splitlines()` then `str.strip()`, recorded from CPython by
    /// `scripts/update_golden_raw.py`.
    #[test]
    fn lines_match_cpython_recordings() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/raw-lines.json");
        let text = std::fs::read_to_string(path).expect("raw-lines.json");
        let cases: Vec<(String, Vec<String>)> = serde_json::from_str(&text).expect("JSON");
        for (text, expected) in &cases {
            let got: Vec<&str> = splitlines(text).map(py_strip).collect();
            assert_eq!(&got, expected, "{text:?}");
        }
    }
}
