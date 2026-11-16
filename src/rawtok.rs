//! CPython 3.14's lexer (`Parser/lexer/lexer.c`), ported statement by
//! statement, in the two modes CPython runs it in.
//!
//! In `tokenize` mode (`extra_tokens=True`) it is the tokenizer radon's `raw`
//! uses: `tokenize.generate_tokens` on a buffer of stripped lines. There the
//! port is reduced to what radon reads: whether tokenizing fails, and the
//! token stream as [`Tok`] classes, in CPython's order and number. radon
//! discards the error message, so none is built. Bracket-mismatch checks and
//! identifier and number-suffix validation are off in that mode, in CPython
//! too, and indentation is not tracked: every line is stripped, so its column
//! is always 0.
//!
//! In *strict* mode it is the lexer as `ast.parse` runs it (`extra_tokens`
//! off): those checks are on, indentation is tracked, and a failure carries
//! CPython's message and line. [`prepass`] runs it over whole files before
//! Ruff parses them.
//!
//! Left out of both modes, since neither answer depends on them: f-string
//! debug text, and the choice between NL and NEWLINE (and with it the INDENT
//! and DEDENT tokens).

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;

use crate::limits::{MAX_EXPR_NESTING, MAX_FSTRING_LEVEL, MAX_INDENT, MAX_LEVEL};
use crate::pyfmt::identifier_error;

/// The token classes radon's `raw` distinguishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tok {
    /// `COMMENT`.
    Comment,
    /// `NL` or `NEWLINE`.
    Newline,
    /// `ENDMARKER`, always the last token.
    EndMarker,
    /// `STRING`; `multiline` when it spans more than one line (its start and
    /// end rows differ).
    String { multiline: bool },
    /// `OP` with the string `:` (not `:=`).
    Colon,
    /// `OP` with the string `;`.
    Semi,
    /// Every other token: names, numbers, other operators, f-/t-string parts
    /// (empty ones too).
    Other,
}

/// Tokenizing failed: `tokenize` raised `TokenError` (any lexer error).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Failed;

type Lexed<T> = Result<T, Failed>;

/// `EOF` from `tok_nextc`.
const EOF: i32 = -1;
/// `tok_nextc` fails once a line's column reaches `INT_MAX`.
const MAX_COLUMN: usize = i32::MAX as usize;
/// CPython's `TabError`: the two indentation columns disagree.
const TAB_ERROR: &str = "inconsistent use of tabs and spaces in indentation";

/// Tokenize `buffer` as
/// `tokenize.generate_tokens(io.StringIO(buffer).readline)` does.
///
/// # Errors
/// [`Failed`] where CPython raises `TokenError`.
#[cfg(test)]
pub(crate) fn tokenize(buffer: &str) -> Lexed<Vec<Tok>> {
    let mut tokenizer = Tokenizer::default();
    match tokenizer.resume(buffer) {
        Step::Done => Ok(tokenizer.tokens),
        Step::Failed { .. } => Err(Failed),
    }
}

/// What [`prepass`] found for Ruff's benefit: the bytes that CPython and
/// Ruff's lexer read differently, each to be overwritten with spaces in the
/// copy of the text that Ruff parses.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Repairs {
    /// `{{` in a format spec after a nested field: an escaped brace to
    /// CPython, the start of another field to Ruff.
    pub(crate) braces: Vec<usize>,
    /// Tabs in indentation that counts. CPython measures them with tab stops
    /// of 8 and of 1, Ruff with 2 and 1; as single spaces Ruff reads CPython's
    /// second column, which decides the same blocks as its first.
    pub(crate) tabs: Vec<usize>,
}

impl Repairs {
    pub(crate) fn is_empty(&self) -> bool {
        self.braces.is_empty() && self.tabs.is_empty()
    }

    /// `text` with every recorded byte replaced by a space: the same length
    /// and the same line breaks.
    pub(crate) fn apply(&self, text: &str) -> String {
        let mut bytes = text.as_bytes().to_vec();
        for &at in &self.braces {
            debug_assert_eq!(&bytes[at..at + 2], b"{{");
            bytes[at] = b' ';
            bytes[at + 1] = b' ';
        }
        for &at in &self.tabs {
            debug_assert_eq!(bytes[at], b'\t');
            bytes[at] = b' ';
        }
        String::from_utf8(bytes).expect("ASCII bytes replaced ASCII bytes")
    }
}

/// Whether `text` could hold something [`prepass`] exists for: `{{`, or a
/// tab or form feed among the blanks that start a physical line. Without
/// `{{` there is no brace to repair. Without a tab or form feed in any line's
/// leading blanks, every indentation is a count of spaces to both lexers.
pub(crate) fn needs_prepass(text: &str) -> bool {
    let bytes = text.as_bytes();
    if text.contains("{{") {
        return true;
    }
    if !bytes.contains(&b'\t') && !bytes.contains(&0x0c) {
        return false;
    }
    bytes.split(|&b| b == b'\n' || b == b'\r').any(|line| {
        line.iter()
            .take_while(|&&b| b == b' ' || b == b'\t' || b == 0x0c)
            .any(|&b| b != b' ')
    })
}

/// Lex the whole of `text` as CPython 3.14's `ast.parse` does, before Ruff
/// sees it.
///
/// # Errors
/// CPython's lexer rejects the text (so `ast.parse` raises, whatever the
/// parser would have said): its message and line. Or the text has the one
/// indentation oxradon refuses ([`LexError::policy`]).
pub(crate) fn prepass(text: &str) -> Result<Repairs, LexError> {
    // `ast.parse` sees a lone CR as a line break. CRLF the lexer steps over.
    // Most texts have no CR at all, which one fast search settles.
    let mut bytes = Cow::Borrowed(text.as_bytes());
    let lone_cr = |bytes: &[u8], i: usize| bytes[i] == b'\r' && bytes.get(i + 1) != Some(&b'\n');
    if bytes.contains(&b'\r') && (0..bytes.len()).any(|i| lone_cr(&bytes, i)) {
        let owned = bytes.to_mut();
        for i in 0..owned.len() {
            if lone_cr(owned, i) {
                owned[i] = b'\n';
            }
        }
    }
    let mut lexer = Lexer::new(&bytes, State::default(), Some(Strict::new()));
    // Only a text that long can hold a line that long.
    let too_long = bytes.len() >= MAX_COLUMN
        && bytes
            .split(|&b| b == b'\n')
            .any(|line| line.len() >= MAX_COLUMN);
    let outcome = if too_long {
        // `tok_nextc` stops when a line's column reaches `INT_MAX`.
        lexer.fail_at(
            0,
            format_args!("Parser column offset overflow - source line is too big"),
        )
    } else {
        loop {
            match lexer.next_token() {
                Ok(Tok::EndMarker) => break Ok(()),
                Ok(_) => {}
                Err(Failed) => break Err(Failed),
            }
        }
    };
    let strict = lexer.strict.take().expect("a strict lexer");
    match outcome {
        Ok(()) => Ok(strict.repairs),
        // Every failure in strict mode records its error.
        Err(Failed) => Err(strict.error.expect("a failure with its error")),
    }
}

/// How [`Tokenizer::resume`] ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The buffer tokenizes: [`Tokenizer::into_tokens`] has its tokens.
    Done,
    /// It doesn't (CPython's `TokenError`). Only when the lexer read to the end
    /// of the buffer (`at_eof`) can a longer buffer (this one, `\n`, more)
    /// tokenize: any other failure happens on bytes every longer buffer shares,
    /// so it repeats.
    Failed { at_eof: bool },
}

/// radon's growing buffers, tokenized without starting over.
///
/// radon tokenizes `B`, then `B + "\n" + line`, and so on, from scratch each
/// time. The longer buffer has the same bytes as `B` up to and including the
/// newline that ends `B` (implicit there, real here), so every token the lexer
/// finished on `B` comes out the same. Only the token that read past the end of
/// `B` can change: it is lexed again from where it started, with the lexer's
/// state as it was then.
#[derive(Default)]
pub(crate) struct Tokenizer {
    state: State,
    tokens: Vec<Tok>,
    /// Bytes of the buffer already checked for NUL and line length.
    checked: usize,
}

impl Tokenizer {
    /// Tokenize `buffer`: the first time, any text; then the previous buffer
    /// plus `\n` and more.
    pub(crate) fn resume(&mut self, buffer: &str) -> Step {
        let bytes = buffer.as_bytes();
        let new = &bytes[self.checked.min(bytes.len())..];
        // `tok_nextc` rejects a line holding NUL when it reads it, and success
        // reads every line. Every byte is read at least once on success, so a
        // line with more than `INT_MAX` bytes (counting its newline) always
        // reaches the column limit. Both repeat for every longer buffer.
        if new.contains(&0)
            || new
                .split(|&b| b == b'\n')
                .any(|line| line.len() >= MAX_COLUMN)
        {
            return Step::Failed { at_eof: false };
        }
        self.checked = bytes.len();
        let mut lexer = Lexer::new(bytes, std::mem::take(&mut self.state), None);
        loop {
            let before = lexer.state.snapshot();
            let Ok(token) = lexer.next_token() else {
                let at_eof = lexer.at_eof;
                self.state = lexer.state;
                if at_eof && !self.state.restore(before) {
                    // The failed token changed the mode stack itself, which no
                    // failing token does: start over to stay exact all the
                    // same.
                    self.state = State::default();
                    self.tokens.clear();
                }
                return Step::Failed { at_eof };
            };
            self.tokens.push(token);
            if token == Tok::EndMarker {
                self.state = lexer.state;
                return Step::Done;
            }
        }
    }

    pub(crate) fn into_tokens(self) -> Vec<Tok> {
        self.tokens
    }
}

/// The lexer's state between tokens (the `tok_state` fields that matter here).
struct State {
    cur: usize,
    /// Open brackets (`tok->level`).
    level: u32,
    /// `parenstack`: the `level` open brackets, with where each one is (its line is
    /// `parenlinenostack`).
    brackets: Vec<(u8, usize)>,
    /// At the beginning of a line (`tok->atbol`).
    atbol: bool,
    modes: Vec<Mode>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            cur: 0,
            level: 0,
            brackets: Vec::new(),
            atbol: true,
            modes: vec![Mode::regular()],
        }
    }
}

/// [`State`] before a token: within one token only the top mode changes,
/// besides the stack growing or shrinking by one in tokens that succeed, and
/// at most one bracket opens or closes.
#[derive(Clone, Copy)]
struct Snapshot {
    cur: usize,
    level: u32,
    /// The innermost open bracket, which a closing bracket in the token pops.
    bracket: Option<(u8, usize)>,
    atbol: bool,
    depth: usize,
    top: Mode,
}

impl State {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            cur: self.cur,
            level: self.level,
            bracket: self.brackets.last().copied(),
            atbol: self.atbol,
            depth: self.modes.len(),
            top: self.modes[self.modes.len() - 1],
        }
    }

    /// Go back to `snapshot`; false if the mode stack's depth changed since.
    fn restore(&mut self, snapshot: Snapshot) -> bool {
        if self.modes.len() != snapshot.depth {
            return false;
        }
        self.cur = snapshot.cur;
        // Undo the token's one push or pop: `brackets` always has `level` entries.
        self.brackets.truncate(snapshot.level as usize);
        if self.level < snapshot.level
            && let Some(bracket) = snapshot.bracket
        {
            self.brackets.push(bracket);
        }
        self.level = snapshot.level;
        self.atbol = snapshot.atbol;
        let last = self.modes.len() - 1;
        self.modes[last] = snapshot.top;
        true
    }
}

/// `tokenizer_mode`'s kind.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Regular,
    FString,
}

/// One entry of the lexer's mode stack (`tokenizer_mode`), with the fields that
/// matter here.
#[derive(Clone, Copy)]
struct Mode {
    kind: Kind,
    quote: i32,
    quote_size: usize,
    raw: bool,
    /// `curly_bracket_depth`: brackets open inside this f-string.
    depth: i32,
    /// `curly_bracket_expr_start_depth`: -1 outside a replacement field.
    expr_start: i32,
    in_format_spec: bool,
    /// Where the string starts (its line is `first_line`), for messages.
    start: usize,
    /// `string_kind == TSTRING`, for messages.
    template: bool,
}

impl Mode {
    /// The bottom entry (`tok_mode_stack[0]`).
    fn regular() -> Self {
        Self {
            kind: Kind::Regular,
            quote: 0,
            quote_size: 0,
            raw: false,
            depth: 0,
            expr_start: 0,
            in_format_spec: false,
            start: 0,
            template: false,
        }
    }

    /// `INSIDE_FSTRING_EXPR`.
    fn in_expr(&self) -> bool {
        self.expr_start >= 0
    }

    /// `TOK_GET_STRING_PREFIX`.
    fn prefix(&self) -> char {
        if self.template { 't' } else { 'f' }
    }
}

/// Why strict lexing stopped: CPython's syntax error as `ast.parse` words it,
/// or oxradon's own refusal of the one indentation it cannot hand to Ruff.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LexError {
    pub(crate) message: String,
    pub(crate) line: u32,
    /// Not CPython's verdict: a tab before a backslash continuation fixed a
    /// used indentation at a column that only 8-column tab stops give (a
    /// COMPATIBILITY.md row).
    pub(crate) policy: bool,
    /// The line before which a parser error takes precedence over this one,
    /// if any does. When CPython's parser fails, CPython lexes the rest of the
    /// text for errors, and one found there replaces the parser's only if the
    /// lexer raised it as an exception outside any f-string: a status code
    /// (EOF, a continuation, indentation) does not, and inside an f-string
    /// the parser's error is kept
    /// (`_PyPegen_tokenize_full_source_to_check_for_errors`). The line is
    /// where what the lexer failed in begins: the error's own, or that of the
    /// outermost bracket or f-string still open.
    ///
    /// shortcut: an approximation, not a port. CPython decides by how far its
    /// parser got, keeps the parser's error for any lexer error inside an
    /// f-string, and for a status code compares the innermost open bracket's
    /// line. Here Ruff's first error line stands in for the parser's progress,
    /// so a file with both kinds of error can name a different one than radon
    /// (`f"""{x!z` / `0b2` / `}"""`). Revisit with the error-line fix in
    /// `syntax::parse`.
    pub(crate) yields_before: Option<u32>,
}

/// How the lexer reports an error.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Signal {
    /// `_PyTokenizer_syntaxerror`: a Python exception, raised at once.
    Raised,
    /// A status code in `tok->done`, worded later by the parser.
    Status,
    /// oxradon's own refusal ([`LexError::policy`]).
    Refusal,
}

/// What strict mode keeps beside the token stream.
struct Strict {
    /// `indstack` and `altindstack`: each open indentation's column with tab
    /// stops of 8, and of 1. The bottom entry is column 0 and stays.
    indents: Vec<(usize, usize)>,
    repairs: Repairs,
    /// Tabs of the indentation run being read, until it is known to count.
    run_tabs: Vec<usize>,
    error: Option<LexError>,
}

impl Strict {
    fn new() -> Self {
        Strict {
            indents: vec![(0, 0)],
            repairs: Repairs::default(),
            run_tabs: Vec::new(),
            error: None,
        }
    }

    fn top(&self) -> (usize, usize) {
        *self.indents.last().expect("the bottom entry stays")
    }

    /// A line indented to `col` and `altcol` (`lexer.c`, "if (!blankline &&
    /// tok->level == 0)"): the two columns must agree on whether it continues
    /// the open block, opens one, or closes some.
    ///
    /// # Errors
    /// CPython's message.
    fn indent(&mut self, col: usize, altcol: usize) -> Result<(), &'static str> {
        let (top, alt_top) = self.top();
        let consistent = match col.cmp(&top) {
            Ordering::Equal => altcol == alt_top,
            // An indent: always one level.
            Ordering::Greater => {
                if self.indents.len() >= MAX_INDENT {
                    return Err("too many levels of indentation");
                }
                self.indents.push((col, altcol));
                altcol > alt_top
            }
            // A dedent: any number of levels, onto one of them.
            Ordering::Less => {
                while self.indents.len() > 1 && col < self.top().0 {
                    self.indents.pop();
                }
                let (top, alt_top) = self.top();
                if col != top {
                    return Err("unindent does not match any outer indentation level");
                }
                altcol == alt_top
            }
        };
        if !consistent {
            return Err(TAB_ERROR);
        }
        // The indentation counts, so Ruff must read its tabs as CPython does.
        self.repairs.tabs.append(&mut self.run_tabs);
        Ok(())
    }
}

struct Lexer<'a> {
    bytes: &'a [u8],
    /// `bytes.len()`, plus one for the implicit newline.
    len: usize,
    state: State,
    /// `tok_nextc` returned EOF at least once.
    at_eof: bool,
    /// The furthest `cur` has been before a [`Lexer::backup`]: with `cur`, how
    /// far the text has been read, which is what `tok->lineno` counts.
    high: usize,
    /// Parser mode (`tok_extra_tokens` off), with what it collects.
    strict: Option<Strict>,
}

fn is_digit(c: i32) -> bool {
    (i32::from(b'0')..=i32::from(b'9')).contains(&c)
}

fn is_xdigit(c: i32) -> bool {
    is_digit(c) || (i32::from(b'a')..=i32::from(b'f')).contains(&(c | 0x20))
}

/// `is_potential_identifier_start`: ASCII letters, `_`, and every byte of a
/// non-ASCII character.
fn is_identifier_start(c: i32) -> bool {
    c >= 0x80 || c == i32::from(b'_') || (i32::from(b'a')..=i32::from(b'z')).contains(&(c | 0x20))
}

/// `is_potential_identifier_char`.
fn is_identifier_char(c: i32) -> bool {
    is_identifier_start(c) || is_digit(c)
}

fn is(c: i32, byte: u8) -> bool {
    c == i32::from(byte)
}

/// `c`, which is a byte (not [`EOF`]), as a `u8`.
fn byte(c: i32) -> u8 {
    u8::try_from(c).unwrap_or(0)
}

/// `_PyToken_TwoChars` matched (`<>` excluded: `barry_as_FLUFL` is off).
fn is_two_char_op(c1: u8, c2: u8) -> bool {
    match c1 {
        b'*' | b'/' | b'<' | b'>' => c2 == c1 || c2 == b'=', // ** // << >> and with =
        b'-' => c2 == b'=' || c2 == b'>',
        b'!' | b'%' | b'&' | b'+' | b':' | b'=' | b'@' | b'^' | b'|' => c2 == b'=',
        _ => false,
    }
}

/// `_PyToken_ThreeChars` matched (`...` is lexed before operators, so never
/// here).
fn is_three_char_op(c1: u8, c2: u8, c3: u8) -> bool {
    matches!(
        (c1, c2, c3),
        (b'*', b'*', b'=') | (b'/', b'/', b'=') | (b'<', b'<', b'=') | (b'>', b'>', b'=')
    )
}

impl<'a> Lexer<'a> {
    fn new(bytes: &'a [u8], state: State, strict: Option<Strict>) -> Self {
        // A last line without `\n` gets an implicit one, from the readline
        // tokenizer and from the string tokenizer alike.
        let implicit_newline = bytes.last().is_some_and(|&b| b != b'\n');
        Lexer {
            bytes,
            len: bytes.len() + usize::from(implicit_newline),
            state,
            at_eof: false,
            high: 0,
            strict,
        }
    }

    /// `tok_nextc`.
    fn nextc(&mut self) -> i32 {
        if self.state.cur < self.len {
            let c = self.bytes.get(self.state.cur).copied().unwrap_or(b'\n');
            self.state.cur += 1;
            i32::from(c)
        } else {
            self.at_eof = true;
            EOF
        }
    }

    /// `tok_backup`: EOF is not backed up.
    fn backup(&mut self, c: i32) {
        if c != EOF {
            self.high = self.high.max(self.state.cur);
            self.state.cur -= 1;
        }
    }

    /// The line of the byte at `offset`, counting from 1.
    fn line_of(&self, offset: usize) -> usize {
        let end = offset.min(self.bytes.len());
        self.bytes[..end].split(|&b| b == b'\n').count()
    }

    /// `tok->lineno`: the line of the last byte read (backed up or not).
    fn lineno(&self) -> usize {
        self.line_of(self.high.max(self.state.cur).saturating_sub(1))
    }

    /// Fail, keeping the error in strict mode: `message` on the line of the
    /// byte at `offset`.
    #[cold]
    fn report<T>(&mut self, offset: usize, message: fmt::Arguments, signal: Signal) -> Lexed<T> {
        // `raw` fails often (every buffer that ends inside a statement) and
        // reads no message: it must not pay for one.
        if self.strict.is_none() {
            return Err(Failed);
        }
        let line = |offset: usize| u32::try_from(self.line_of(offset)).unwrap_or(u32::MAX);
        let fstring = self.state.modes.get(1).map(|outermost| outermost.start);
        let bracket = (self.strict.as_ref())
            .and_then(|_| self.state.brackets.first())
            .map(|&(_, at)| at);
        let yields = signal == Signal::Status || (signal == Signal::Raised && fstring.is_some());
        // Where what the lexer failed in begins: the outermost bracket or
        // f-string still open, if that is before the error.
        let begins = [Some(offset), fstring, bracket].into_iter().flatten().min();
        let yields_before = begins.filter(|_| yields).map(line);
        let error = LexError {
            message: message.to_string(),
            line: line(offset),
            policy: signal == Signal::Refusal,
            yields_before,
        };
        if let Some(strict) = &mut self.strict {
            strict.error = Some(error);
        }
        Err(Failed)
    }

    /// The byte whose line is `tok->lineno`: the last one read.
    fn here(&self) -> usize {
        self.high.max(self.state.cur).saturating_sub(1)
    }

    /// Fail with CPython's `message`, on the line of the byte at `offset`.
    fn fail_at<T>(&mut self, offset: usize, message: fmt::Arguments) -> Lexed<T> {
        self.report(offset, message, Signal::Raised)
    }

    /// Fail with CPython's `message` on the current line.
    fn fail<T>(&mut self, message: fmt::Arguments) -> Lexed<T> {
        self.report(self.here(), message, Signal::Raised)
    }

    /// Fail with a status code, which the parser words as `message`, on the
    /// current line.
    fn fail_with_status<T>(&mut self, message: &str) -> Lexed<T> {
        self.report(self.here(), format_args!("{message}"), Signal::Status)
    }

    /// `E_EOF`: the input ended inside a statement.
    fn fail_at_eof<T>(&mut self) -> Lexed<T> {
        match (self.strict.as_ref()).and_then(|_| self.state.brackets.last().copied()) {
            Some((bracket, at)) => self.report(
                at,
                format_args!("'{}' was never closed", char::from(bracket)),
                Signal::Status,
            ),
            None => self.fail_with_status("unexpected EOF while parsing"),
        }
    }

    fn top(&mut self) -> &mut Mode {
        let last = self.state.modes.len() - 1;
        &mut self.state.modes[last]
    }

    /// `INSIDE_FSTRING`.
    fn inside_fstring(&self) -> bool {
        self.state.modes.len() > 1
    }

    /// `tok_get`.
    fn next_token(&mut self) -> Lexed<Tok> {
        if self.top().kind == Kind::Regular {
            self.normal_mode()
        } else {
            self.fstring_mode()
        }
    }

    /// `tok_continuation_line`: a backslash must end its line, and the next
    /// line must exist.
    fn continuation_line(&mut self) -> Lexed<()> {
        let mut c = self.nextc();
        if is(c, b'\r') {
            c = self.nextc();
        }
        if !is(c, b'\n') {
            return self.fail_with_status("unexpected character after line continuation character");
        }
        let c = self.nextc();
        if c == EOF {
            return self.fail_at_eof();
        }
        self.backup(c);
        Ok(())
    }

    /// "Get indentation level": the blanks that start a line.
    ///
    /// `tokenize` mode reads them and moves on: `raw`'s lines are stripped, so
    /// their column is always 0. Strict mode keeps CPython's two columns, with
    /// tab stops of 8 (`col`) and of 1 (`altcol`), and its checks that the two
    /// agree about every line the indentation of which counts.
    fn indentation(&mut self) -> Lexed<()> {
        let (mut col, mut altcol) = (0_usize, 0_usize);
        // `cont_line_col`, and whether the two columns differed where it was
        // set.
        let mut continued: Option<(usize, bool)> = None;
        if let Some(strict) = &mut self.strict {
            strict.run_tabs.clear();
        }
        let next = loop {
            let c = self.nextc();
            if is(c, b' ') {
                col += 1;
                altcol += 1;
            } else if is(c, b'\t') {
                col = (col / 8 + 1) * 8;
                altcol += 1;
                let at = self.state.cur - 1;
                if let Some(strict) = &mut self.strict {
                    strict.run_tabs.push(at);
                }
            } else if is(c, 0x0c) {
                col = 0;
                altcol = 0;
            } else if is(c, b'\\') {
                // Indentation cannot be split over lines: the first backslash
                // after some blanks fixes it.
                if continued.is_none() && col != 0 {
                    continued = Some((col, col != altcol));
                }
                self.continuation_line()?;
            } else {
                self.backup(c);
                break c;
            }
        };
        // A line of blanks or a comment has no indentation, and neither has a
        // line inside brackets.
        let blank = is(next, b'#') || is(next, b'\n') || is(next, b'\r');
        if self.strict.is_none() || blank || self.state.level > 0 {
            return Ok(());
        }
        if let Some((column, differed)) = continued {
            if differed {
                // oxradon's own refusal: Ruff cannot be handed this column in
                // the same bytes.
                return self.report(self.here(), format_args!("{TAB_ERROR}"), Signal::Refusal);
            }
            col = column;
            altcol = column;
        }
        let checked = match &mut self.strict {
            Some(strict) => strict.indent(col, altcol),
            None => Ok(()),
        };
        match checked {
            Ok(()) => Ok(()),
            Err(message) => self.fail_with_status(message),
        }
    }

    /// `tok_get_normal_mode`.
    fn normal_mode(&mut self) -> Lexed<Tok> {
        if self.state.atbol {
            self.state.atbol = false;
            self.indentation()?;
        }
        // `again:`, after a continuation line.
        loop {
            let mut c = self.nextc();
            while is(c, b' ') || is(c, b'\t') || is(c, 0x0c) {
                c = self.nextc();
            }
            let start = self.state.cur.saturating_sub(1);

            if is(c, b'#') {
                while c != EOF && !is(c, b'\n') && !is(c, b'\r') {
                    c = self.nextc();
                }
                self.backup(c);
                return Ok(Tok::Comment);
            }
            if c == EOF {
                // EOF is an error inside brackets; open f-string modes don't
                // matter.
                return if self.state.level > 0 {
                    self.fail_at_eof()
                } else {
                    Ok(Tok::EndMarker)
                };
            }
            if is_identifier_start(c) {
                return self.name_or_string(c, start);
            }
            if is(c, b'\r') {
                c = self.nextc();
            }
            if is(c, b'\n') {
                self.state.atbol = true;
                return Ok(Tok::Newline);
            }
            if is(c, b'.') {
                let c = self.nextc();
                if is_digit(c) {
                    return self.fraction(c);
                }
                if is(c, b'.') {
                    let c = self.nextc();
                    if is(c, b'.') {
                        return Ok(Tok::Other); // ELLIPSIS
                    }
                    self.backup(c);
                    self.backup(i32::from(b'.'));
                } else {
                    self.backup(c);
                }
                return Ok(Tok::Other); // DOT
            }
            if is_digit(c) {
                return self.number(c);
            }
            if is(c, b'\'') || is(c, b'"') {
                return self.string(c, start);
            }
            if is(c, b'\\') {
                self.continuation_line()?;
                continue;
            }
            return self.operator(c);
        }
    }

    /// From `/* Punctuation character */` on: f-string format-spec colons,
    /// operators, brackets.
    fn operator(&mut self, c: i32) -> Lexed<Tok> {
        let inside = self.inside_fstring();
        let is_punctuation = is(c, b':') || is(c, b'}') || is(c, b'!') || is(c, b'{');
        if is_punctuation && inside && self.top().in_expr() {
            let mode = self.top();
            let cursor = mode.depth - i32::from(!is(c, b'{'));
            if is(c, b':') && cursor == mode.expr_start {
                mode.kind = Kind::FString;
                mode.in_format_spec = true;
                return Ok(Tok::Colon);
            }
        }

        let c2 = self.nextc();
        if c2 != EOF && is_two_char_op(byte(c), byte(c2)) {
            let c3 = self.nextc();
            if c3 == EOF || !is_three_char_op(byte(c), byte(c2), byte(c3)) {
                self.backup(c3);
            }
            return Ok(Tok::Other);
        }
        self.backup(c2);

        let closer = char::from(byte(c));
        match byte(c) {
            b'(' | b'[' | b'{' => {
                if self.state.level >= MAX_LEVEL {
                    return self.fail(format_args!("too many nested parentheses"));
                }
                let at = self.state.cur - 1;
                self.state.brackets.push((byte(c), at));
                self.state.level += 1;
                if inside {
                    self.top().depth += 1;
                }
            }
            b')' | b']' | b'}' => {
                if inside && self.top().depth == 0 && is(c, b'}') {
                    let prefix = self.top().prefix();
                    return self.fail(format_args!("{prefix}-string: single '}}' is not allowed"));
                }
                self.close_bracket(byte(c), inside)?;
                if inside {
                    let mode = self.top();
                    mode.depth -= 1;
                    if mode.depth < 0 {
                        let prefix = mode.prefix();
                        return self.fail(format_args!("{prefix}-string: unmatched '{closer}'"));
                    }
                    if is(c, b'}') && mode.depth == mode.expr_start {
                        mode.expr_start -= 1;
                        mode.kind = Kind::FString;
                        mode.in_format_spec = false;
                    }
                }
            }
            _ => {}
        }

        // `Py_UNICODE_ISPRINTABLE` on an ASCII byte (bytes >= 0x80 start
        // identifiers).
        if !(0x20..0x7f).contains(&c) {
            return self.fail(format_args!("invalid non-printable character U+{c:04X}"));
        }
        Ok(match byte(c) {
            b':' => Tok::Colon,
            b';' => Tok::Semi,
            _ => Tok::Other,
        })
    }

    /// The checks on a closing bracket. For the parser one must be open, and
    /// it must be the matching one. `tokenize` mode allows unmatched and
    /// mismatched closers, except (since CPython 3.14.10) one that closes a
    /// field's own `{`.
    fn close_bracket(&mut self, closer: u8, inside: bool) -> Lexed<()> {
        let strict = self.strict.is_some();
        let closer_char = char::from(closer);
        let Some((opener, at)) = self.state.brackets.pop() else {
            if strict {
                return self.fail(format_args!("unmatched '{closer_char}'"));
            }
            return Ok(());
        };
        // With the pop, as in CPython, so that `brackets` has `level`
        // entries on the error paths below too.
        self.state.level -= 1;
        if matches!((opener, closer), (b'(', b')') | (b'[', b']') | (b'{', b'}')) {
            return Ok(());
        }
        // A field's own `{` closed by something else is the f-string's
        // error, not a mismatch.
        if inside && opener == b'{' {
            let mode = *self.top();
            if mode.depth - 1 == mode.expr_start {
                let prefix = mode.prefix();
                return self.fail(format_args!("{prefix}-string: unmatched '{closer_char}'"));
            }
        }
        if !strict {
            return Ok(());
        }
        let opener_char = char::from(opener);
        let opened_on = self.line_of(at);
        let elsewhere = if opened_on == self.lineno() {
            String::new()
        } else {
            format!(" on line {opened_on}")
        };
        self.fail(format_args!(
            "closing parenthesis '{closer_char}' does not match opening parenthesis \
             '{opener_char}'{elsewhere}"
        ))
    }

    /// `verify_identifier`, for the name that starts at `start` and has a
    /// non-ASCII character.
    fn verify_identifier(&mut self, start: usize) -> Lexed<()> {
        let end = self.state.cur.min(self.bytes.len());
        let name = std::str::from_utf8(&self.bytes[start..end])
            .expect("a name is whole characters of a str");
        match identifier_error(name) {
            None => Ok(()),
            Some(message) => self.fail(format_args!("{message}")),
        }
    }

    /// An identifier, or a string with a prefix (`b`, `r`, `u`, `f`, `t` in any
    /// case and order, at most one of each).
    fn name_or_string(&mut self, first: i32, start: usize) -> Lexed<Tok> {
        let (mut saw_b, mut saw_r, mut saw_u, mut saw_f, mut saw_t) =
            (false, false, false, false, false);
        let mut c = first;
        loop {
            match byte(c).to_ascii_lowercase() {
                b'b' if !saw_b => saw_b = true,
                b'u' if !saw_u => saw_u = true,
                b'r' if !saw_r => saw_r = true,
                b'f' if !saw_f => saw_f = true,
                b't' if !saw_t => saw_t = true,
                _ => break,
            }
            c = self.nextc();
            if is(c, b'"') || is(c, b'\'') {
                // `maybe_raise_syntax_error_for_string_prefixes`, in its order.
                let incompatible = [
                    (saw_u && saw_b, "u", "b"),
                    (saw_u && saw_r, "u", "r"),
                    (saw_u && saw_f, "u", "f"),
                    (saw_u && saw_t, "u", "t"),
                    (saw_b && saw_f, "b", "f"),
                    (saw_b && saw_t, "b", "t"),
                    (saw_f && saw_t, "f", "t"),
                ];
                if let Some((_, first, second)) = incompatible.iter().find(|pair| pair.0) {
                    return self.fail(format_args!(
                        "'{first}' and '{second}' prefixes are incompatible"
                    ));
                }
                if saw_f || saw_t {
                    return self.fstring_start(c, start);
                }
                return self.string(c, start);
            }
        }
        while is_identifier_char(c) {
            c = self.nextc();
        }
        self.backup(c);
        // A name is never validated in `tokenize` mode, and an ASCII one
        // needs no validating.
        let end = self.state.cur.min(self.bytes.len());
        if self.strict.is_some() && !self.bytes[start..end].is_ascii() {
            self.verify_identifier(start)?;
        }
        Ok(Tok::Other) // NAME
    }

    /// `f_string_quote:`, after an `f`/`t` prefix: push a mode for the string.
    fn fstring_start(&mut self, quote: i32, start: usize) -> Lexed<Tok> {
        let mut quote_size = 1;
        let after_quote = self.nextc();
        if after_quote == quote {
            let after_after_quote = self.nextc();
            if after_after_quote == quote {
                quote_size = 3;
            } else {
                self.backup(after_after_quote);
                self.backup(after_quote);
            }
        }
        if after_quote != quote {
            self.backup(after_quote);
        }
        if self.state.modes.len() >= MAX_FSTRING_LEVEL {
            return self.fail(format_args!("too many nested f-strings or t-strings"));
        }
        let prefix = |i: usize| self.bytes.get(start + i).map(u8::to_ascii_lowercase);
        let (raw, template) = match prefix(0) {
            Some(b'r') => (true, prefix(1) == Some(b't')),
            first => (prefix(1) == Some(b'r'), first == Some(b't')),
        };
        self.state.modes.push(Mode {
            kind: Kind::FString,
            quote,
            quote_size,
            raw,
            depth: 0,
            expr_start: -1,
            in_format_spec: false,
            start,
            template,
        });
        Ok(Tok::Other) // FSTRING_START or TSTRING_START
    }

    /// `letter_quote:`: a string literal that starts at `start`, the opening
    /// quote already read.
    fn string(&mut self, quote: i32, start: usize) -> Lexed<Tok> {
        let mut quote_size = 1;
        let mut end_quote_size = 0;
        let mut c = self.nextc();
        if c == quote {
            c = self.nextc();
            if c == quote {
                quote_size = 3;
            } else {
                end_quote_size = 1; // empty string
            }
        }
        if c != quote {
            self.backup(c);
        }
        let mut multiline = false;
        while end_quote_size != quote_size {
            let mut c = self.nextc();
            if c == EOF || (quote_size == 1 && is(c, b'\n')) {
                return self.unterminated_string(quote, quote_size, start);
            }
            if c == quote {
                end_quote_size += 1;
            } else {
                end_quote_size = 0;
                if is(c, b'\\') {
                    c = self.nextc(); // the escaped byte
                    if is(c, b'\r') {
                        c = self.nextc();
                    }
                }
            }
            multiline |= is(c, b'\n');
        }
        Ok(Tok::String { multiline })
    }

    /// The error for a string that starts at `start` and never ends: reported
    /// on its first line, naming the line where the lexer gave up.
    #[cold]
    fn unterminated_string<T>(&mut self, quote: i32, quote_size: usize, start: usize) -> Lexed<T> {
        // `raw` meets this on every buffer that ends inside a string.
        if self.strict.is_none() {
            return Err(Failed);
        }
        let detected = self.lineno();
        // `has_escaped_quote`, found here and not in the loop every string
        // of every file goes through: a backslash escapes the next byte.
        let end = self.state.cur.min(self.bytes.len());
        let mut body = self.bytes[start..end].iter();
        let mut has_escaped_quote = false;
        while let Some(&b) = body.next() {
            if b == b'\\' {
                has_escaped_quote |= body.next().is_some_and(|&next| i32::from(next) == quote);
            }
        }
        // Inside an f-string that uses the same quotes, the string is really
        // the f-string's end and a `}` is missing.
        let mode = *self.top();
        if self.inside_fstring() && mode.quote == quote && mode.quote_size == quote_size {
            let prefix = mode.prefix();
            return self.fail_at(start, format_args!("{prefix}-string: expecting '}}'"));
        }
        let (triple, hint) = match (quote_size, has_escaped_quote) {
            (3, _) => ("triple-quoted ", ""),
            (_, true) => ("", "; perhaps you escaped the end quote?"),
            (_, false) => ("", ""),
        };
        self.fail_at(
            start,
            format_args!("unterminated {triple}string literal (detected at line {detected}){hint}"),
        )
    }

    /// A number starting with the digit `c`.
    fn number(&mut self, c: i32) -> Lexed<Tok> {
        if !is(c, b'0') {
            let c = self.decimal_tail()?;
            return self.after_integer(c);
        }
        let mut c = self.nextc();
        let radix = byte(c).to_ascii_lowercase();
        if radix == b'x' {
            c = self.nextc();
            loop {
                if is(c, b'_') {
                    c = self.nextc();
                }
                if !is_xdigit(c) {
                    self.backup(c);
                    return self.fail(format_args!("invalid hexadecimal literal"));
                }
                loop {
                    c = self.nextc();
                    if !is_xdigit(c) {
                        break;
                    }
                }
                if !is(c, b'_') {
                    break;
                }
            }
            self.verify_end_of_number(c, "hexadecimal")?;
        } else if radix == b'o' || radix == b'b' {
            let (top, kind) = if radix == b'o' {
                (b'8', "octal")
            } else {
                (b'2', "binary")
            };
            let in_radix = |c: i32| (i32::from(b'0')..i32::from(top)).contains(&c);
            let digit = |c: i32| char::from(byte(c));
            c = self.nextc();
            loop {
                if is(c, b'_') {
                    c = self.nextc();
                }
                if !in_radix(c) {
                    if is_digit(c) {
                        return self.fail(format_args!(
                            "invalid digit '{}' in {kind} literal",
                            digit(c)
                        ));
                    }
                    self.backup(c);
                    return self.fail(format_args!("invalid {kind} literal"));
                }
                loop {
                    c = self.nextc();
                    if !in_radix(c) {
                        break;
                    }
                }
                if !is(c, b'_') {
                    break;
                }
            }
            if is_digit(c) {
                return self.fail(format_args!(
                    "invalid digit '{}' in {kind} literal",
                    digit(c)
                ));
            }
            self.verify_end_of_number(c, kind)?;
        } else {
            // Zeros, then perhaps more digits.
            loop {
                if is(c, b'_') {
                    c = self.nextc();
                    if !is_digit(c) {
                        self.backup(c);
                        return self.fail(format_args!("invalid decimal literal"));
                    }
                }
                if !is(c, b'0') {
                    break;
                }
                c = self.nextc();
            }
            let nonzero = is_digit(c);
            if nonzero {
                c = self.decimal_tail()?;
            }
            // Old-style octal is allowed in `tokenize` mode.
            let goes_on = matches!(byte(c), b'.' | b'e' | b'E' | b'j' | b'J') && c != EOF;
            if nonzero && !goes_on && self.strict.is_some() {
                self.backup(c);
                return self.fail(format_args!(
                    "leading zeros in decimal integer literals are not permitted; \
                     use an 0o prefix for octal integers"
                ));
            }
            return self.after_integer(c);
        }
        self.backup(c);
        Ok(Tok::Other)
    }

    /// `lookahead`: whether `rest` and then no identifier character follow.
    /// Nothing is consumed.
    fn lookahead(&mut self, rest: &[u8]) -> bool {
        let mut matched = 0;
        let result = loop {
            let c = self.nextc();
            if matched == rest.len() {
                self.backup(c);
                break !is_identifier_char(c);
            }
            if !is(c, rest[matched]) {
                self.backup(c);
                break false;
            }
            matched += 1;
        };
        for &read in rest[..matched].iter().rev() {
            self.backup(i32::from(read));
        }
        result
    }

    /// `verify_end_of_number`, `c` being the byte after the number: in strict
    /// mode a name may not follow a number directly, except one of the
    /// keywords CPython only warns about.
    fn verify_end_of_number(&mut self, c: i32, kind: &str) -> Lexed<()> {
        if self.strict.is_none() || c == EOF {
            return Ok(());
        }
        let keyword = match byte(c) {
            b'a' => self.lookahead(b"nd"),
            b'e' => self.lookahead(b"lse"),
            b'f' => self.lookahead(b"or"),
            b'i' => {
                let next = self.nextc();
                self.backup(next);
                is(next, b'f') || is(next, b'n') || is(next, b's')
            }
            b'o' => self.lookahead(b"r"),
            b'n' => self.lookahead(b"ot"),
            _ => false,
        };
        if !keyword && c < 0x80 && is_identifier_char(c) {
            self.backup(c);
            return self.fail(format_args!("invalid {kind} literal"));
        }
        Ok(())
    }

    /// `tok_decimal_tail`, the first digit already read: digits with single `_`
    /// separators. Returns the byte after them.
    fn decimal_tail(&mut self) -> Lexed<i32> {
        loop {
            let mut c;
            loop {
                c = self.nextc();
                if !is_digit(c) {
                    break;
                }
            }
            if !is(c, b'_') {
                return Ok(c);
            }
            c = self.nextc();
            if !is_digit(c) {
                self.backup(c);
                return self.fail(format_args!("invalid decimal literal"));
            }
        }
    }

    /// After a decimal integer part: `c` is the byte following it.
    fn after_integer(&mut self, c: i32) -> Lexed<Tok> {
        if is(c, b'.') {
            let c = self.nextc();
            return self.fraction(c);
        }
        self.exponent(c)
    }

    /// `fraction:`, after the `.`: `c` is the byte following it.
    fn fraction(&mut self, mut c: i32) -> Lexed<Tok> {
        if is_digit(c) {
            c = self.decimal_tail()?;
        }
        self.exponent(c)
    }

    /// The optional exponent and `j` suffix; `c` is the next byte.
    fn exponent(&mut self, mut c: i32) -> Lexed<Tok> {
        if is(c, b'e') || is(c, b'E') {
            let e = c;
            c = self.nextc();
            if is(c, b'+') || is(c, b'-') {
                c = self.nextc();
                if !is_digit(c) {
                    self.backup(c);
                    return self.fail(format_args!("invalid decimal literal"));
                }
            } else if !is_digit(c) {
                // Not an exponent: the number ends before the `e`.
                self.backup(c);
                self.verify_end_of_number(e, "decimal")?;
                self.backup(e);
                return Ok(Tok::Other);
            }
            c = self.decimal_tail()?;
        }
        if is(c, b'j') || is(c, b'J') {
            c = self.nextc();
            self.verify_end_of_number(c, "imaginary")?;
        } else {
            self.verify_end_of_number(c, "decimal")?;
        }
        self.backup(c);
        Ok(Tok::Other)
    }

    /// `tok_get_fstring_mode`: inside an f-/t-string, outside a replacement
    /// field.
    fn fstring_mode(&mut self) -> Lexed<Tok> {
        let start_char = self.nextc();
        if is(start_char, b'{') {
            let peek = self.nextc();
            self.backup(peek);
            self.backup(start_char);
            if !is(peek, b'{') {
                self.start_field()?;
                return self.normal_mode();
            }
        } else {
            self.backup(start_char);
        }

        let Mode {
            quote, quote_size, ..
        } = *self.top();
        // The end of the string? Quotes matched before a mismatch stay
        // consumed, as in CPython.
        for _ in 0..quote_size {
            let c = self.nextc();
            if c != quote {
                self.backup(c);
                return self.fstring_middle();
            }
        }
        self.state.modes.pop();
        Ok(Tok::Other) // FSTRING_END or TSTRING_END
    }

    /// A `{` that starts a replacement field: back to regular mode, one
    /// level deeper.
    fn start_field(&mut self) -> Lexed<()> {
        let mode = self.top();
        mode.expr_start += 1;
        if mode.expr_start >= MAX_EXPR_NESTING {
            let prefix = mode.prefix();
            return self.fail(format_args!(
                "{prefix}-string: expressions nested too deeply"
            ));
        }
        mode.kind = Kind::Regular;
        Ok(())
    }

    /// The error for an f-/t-string that never ends: reported on its first
    /// line, naming the line where the lexer gave up.
    #[cold]
    fn unterminated_fstring<T>(&mut self, mode: Mode) -> Lexed<T> {
        if self.strict.is_none() {
            return Err(Failed);
        }
        let (prefix, detected) = (mode.prefix(), self.lineno());
        let triple = if mode.quote_size == 3 {
            "triple-quoted "
        } else {
            ""
        };
        self.fail_at(
            mode.start,
            format_args!(
                "unterminated {triple}{prefix}-string literal (detected at line {detected})"
            ),
        )
    }

    /// `f_string_middle:`: literal text up to a replacement field or the
    /// closing quotes.
    fn fstring_middle(&mut self) -> Lexed<Tok> {
        let Mode {
            quote,
            quote_size,
            raw,
            ..
        } = *self.top();
        let mut end_quote_size = 0;
        let mut unicode_escape = false;
        while end_quote_size != quote_size {
            let c = self.nextc();
            let mode = *self.top();
            let in_format_spec = mode.in_format_spec && mode.in_expr();
            if c == EOF || (quote_size == 1 && is(c, b'\n')) {
                // A newline in a single-quoted format spec, or unterminated.
                if in_format_spec && is(c, b'\n') {
                    let prefix = mode.prefix();
                    return self.fail(format_args!(
                        "{prefix}-string: newlines are not allowed in format specifiers \
                         for single quoted {prefix}-strings"
                    ));
                }
                return self.unterminated_fstring(mode);
            }
            if c == quote {
                end_quote_size += 1;
                continue;
            }
            end_quote_size = 0;
            if is(c, b'{') {
                let peek = self.nextc();
                if !is(peek, b'{') || in_format_spec {
                    self.backup(peek);
                    self.backup(c);
                    self.start_field()?;
                    self.top().in_format_spec = false;
                } else if mode.in_expr() {
                    // An escaped brace in a format spec, after a nested field:
                    // Ruff starts another field here (see `prepass`).
                    let at = self.state.cur - 2;
                    if let Some(strict) = &mut self.strict {
                        strict.repairs.braces.push(at);
                    }
                }
                return Ok(Tok::Other); // FSTRING_MIDDLE, possibly empty
            }
            if is(c, b'}') {
                if unicode_escape {
                    return Ok(Tok::Other);
                }
                let peek = self.nextc();
                if !(is(peek, b'}') && !in_format_spec && mode.depth == 0) {
                    self.backup(peek);
                    self.backup(c);
                    let mode = self.top();
                    mode.kind = Kind::Regular;
                    mode.in_format_spec = false;
                }
                return Ok(Tok::Other);
            }
            if is(c, b'\\') {
                let mut peek = self.nextc();
                if is(peek, b'\r') {
                    peek = self.nextc();
                }
                if is(peek, b'{') || is(peek, b'}') {
                    // A non-raw string also gets a SyntaxWarning on stderr.
                    // oxradon prints none of CPython's warnings (a COMPATIBILITY.md row).
                    self.backup(peek);
                    continue;
                }
                if !raw && is(peek, b'N') {
                    let next = self.nextc();
                    if is(next, b'{') {
                        unicode_escape = true;
                    } else {
                        self.backup(next);
                    }
                }
            }
        }
        // The closing quotes: emit the middle now, and the end on the next
        // call.
        for _ in 0..quote_size {
            self.backup(quote);
        }
        Ok(Tok::Other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classes as `scripts/update_golden_raw.py` spells them.
    fn spell(tokens: &[Tok]) -> String {
        tokens
            .iter()
            .map(|t| match t {
                Tok::Comment => '#',
                Tok::Newline => 'n',
                Tok::EndMarker => '$',
                Tok::String { multiline: true } => 'S',
                Tok::String { multiline: false } => 's',
                Tok::Colon => ':',
                Tok::Semi => ';',
                Tok::Other => '.',
            })
            .collect()
    }

    fn spelled(buffer: &str) -> Option<String> {
        tokenize(buffer).ok().map(|t| spell(&t))
    }

    #[test]
    fn small_cases() {
        assert_eq!(spelled(""), Some("$".into()));
        assert_eq!(spelled("x"), Some(".n$".into()));
        assert_eq!(spelled("# c"), Some("#n$".into()));
        assert_eq!(spelled("x = ("), None);
        assert_eq!(spelled("'''a\nb'''"), Some("Sn$".into()));
        assert_eq!(spelled("f'{x:}'"), Some("...:...n$".into()));
        // A field's own `{` closed by another bracket is an error since CPython 3.14.10;
        // other mismatches still pass in `tokenize` mode.
        assert_eq!(spelled("f\"{x)"), None);
        assert_eq!(spelled("f\"{(x]}\""), Some(".......n$".into()));
        assert_eq!(spelled("(x]"), Some("...n$".into()));
        assert_eq!(spelled("a\u{1}b"), None);
        assert_eq!(spelled("x\0"), None);
    }

    /// CPython's tokenizer on radon-style buffers, recorded by
    /// `scripts/update_golden_raw.py`.
    #[test]
    fn matches_cpython_recordings() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/raw-tokens.json");
        let text = std::fs::read_to_string(path).expect("raw-tokens.json");
        let cases: Vec<(String, Option<String>)> = serde_json::from_str(&text).expect("JSON");
        let mut bad = Vec::new();
        for (buffer, expected) in &cases {
            let got = spelled(buffer);
            if &got != expected {
                bad.push(format!("{buffer:?}: expected {expected:?}, got {got:?}"));
            }
        }
        assert!(
            bad.is_empty(),
            "{} of {} differ:\n{}",
            bad.len(),
            cases.len(),
            bad[..bad.len().min(30)].join("\n")
        );
    }

    /// The error of a text CPython's lexer rejects: message and line.
    fn error(text: &str) -> (String, u32) {
        let error = prepass(text).expect_err("CPython's lexer rejects it");
        assert!(!error.policy, "{text:?} is refused by oxradon, not CPython");
        (error.message, error.line)
    }

    fn repairs(text: &str) -> Repairs {
        prepass(text).expect("CPython's lexer accepts it")
    }

    /// Where the tabs that start the given lines (counted from 1) are.
    fn leading_tabs(text: &str, lines: &[usize]) -> Vec<usize> {
        let mut at = 0;
        let mut tabs = Vec::new();
        for (index, line) in text.split_inclusive('\n').enumerate() {
            if lines.contains(&(index + 1)) {
                let blanks = line.len() - line.trim_start_matches([' ', '\t']).len();
                tabs.extend(
                    (0..blanks)
                        .filter(|&i| line.as_bytes()[i] == b'\t')
                        .map(|i| at + i),
                );
            }
            at += line.len();
        }
        tabs
    }

    /// CPython's lexer in the mode `ast.parse` uses, recorded by
    /// `scripts/check_lexer.py --record`: for each text, nothing if it lexes,
    /// or the error's message and line.
    #[test]
    fn strict_mode_matches_cpython_recordings() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/strict-lexer.json"
        );
        let text = std::fs::read_to_string(path).expect("strict-lexer.json");
        let cases: Vec<(String, Option<(String, u32)>)> =
            serde_json::from_str(&text).expect("JSON");
        assert!(cases.len() > 1000 && cases.iter().any(|case| case.1.is_none()));
        let mut bad = Vec::new();
        for (text, expected) in &cases {
            let got = prepass(text).err().map(|e| (e.message, e.line));
            if got != *expected {
                bad.push(format!("{text:?}: expected {expected:?}, got {got:?}"));
            }
        }
        assert!(
            bad.is_empty(),
            "{} of {} differ:\n{}",
            bad.len(),
            cases.len(),
            bad[..bad.len().min(30)].join("\n")
        );
    }

    #[test]
    fn an_escaped_brace_after_a_nested_field_is_recorded() {
        // CPython reads `{{` as text here; Ruff starts a field.
        assert_eq!(repairs("f'{a:{b}{{x if y else z}}}'\n").braces, [8]);
        assert_eq!(repairs("f'{a:{b}{{x}'\n").braces, [8]);
        assert_eq!(repairs("f'{a:{b}{{{c}}'\n").braces, [8]);
        assert_eq!(repairs("f'{a:{b}{{x{{y}'\n").braces, [8, 11]);
        assert_eq!(repairs("t'{a:{b}{{x}'\n").braces, [8]);
        // Two strings, and a string nested in a field.
        let text = "x = f'{a:{b}{{x}' + f\"{c:{d}{{y}\"\n";
        let found: Vec<usize> = text.match_indices("{{").map(|(at, _)| at).collect();
        assert_eq!(repairs(text).braces, found);
        let text = "x = f'{a:{f\"{b:{c}{{z}\"}{{x}'\n";
        let found: Vec<usize> = text.match_indices("{{").map(|(at, _)| at).collect();
        assert_eq!(repairs(text).braces, found);
    }

    #[test]
    fn other_doubled_braces_are_not_recorded() {
        for text in [
            "f'{{a}}'\n",         // outside any field: an escape to both lexers
            "f'{a:{{b}}}'\n",     // at the start of a spec: a field to both
            "f'{a:{b:{{c}}}}'\n", // the same, in a nested field's own spec
            "f'{a:{b}{ {x}}}'\n", // not doubled
            "x = '{{}}'.format(a)  # {{\n",
            "x = r'{{' + b'{{'\n",
            "def f():\n    '''{a:{b}{{'''\n    return 1 \\\n        + 2\n",
        ] {
            assert_eq!(repairs(text), Repairs::default(), "{text:?}");
        }
    }

    #[test]
    fn repairs_are_at_the_same_place_for_every_line_ending() {
        let text = "if a:\n\tx = f'{a:{b}{{x}'\n\ty = 2\n";
        for ending in ["\n", "\r\n", "\r"] {
            let text = text.replace('\n', ending);
            let found = repairs(&text);
            assert_eq!(
                found.braces,
                [text.find("{{").expect("a brace")],
                "{ending:?}"
            );
            let tabs: Vec<usize> = text.match_indices('\t').map(|(at, _)| at).collect();
            assert_eq!(found.tabs, tabs, "{ending:?}");
            let repaired = found.apply(&text);
            assert_eq!(repaired.len(), text.len());
            assert!(!repaired.contains("{{") && !repaired.contains('\t'));
        }
    }

    #[test]
    fn tabs_are_recorded_only_where_the_indentation_counts() {
        // A comment line, a blank line and a line inside brackets have none.
        let text = "if a:\n\tx = 1\n\t# c\n\t\n\ty = (\n\t1)\n";
        assert_eq!(repairs(text).tabs, leading_tabs(text, &[2, 5]));
        // CPython reads both lines at column 16 (and 6 with tab stops of 1).
        let text = "def f():\n    \t\tpass\n \t   \tpass\n";
        assert_eq!(repairs(text).tabs, leading_tabs(text, &[2, 3]));
        // A tab in a string or after code is not indentation.
        assert_eq!(
            repairs("x = '\t'\t# c\n'''\n\ta\n'''\n"),
            Repairs::default()
        );
    }

    #[test]
    fn indentation_errors_are_cpythons() {
        let tabs = "inconsistent use of tabs and spaces in indentation";
        let dedent = "unindent does not match any outer indentation level";
        assert_eq!(
            error("def f():\n\tif a:\n   return 1\n"),
            (dedent.into(), 3)
        );
        assert_eq!(error("if a:\n\tx = 1\n        y = 2\n"), (tabs.into(), 3));
        assert_eq!(error("if a:\n  x = 1\n\ty = 2\n"), (tabs.into(), 3));
        assert_eq!(
            error("def f():\n\t\x0c\tdef g():\n    x = 1\n"),
            (dedent.into(), 3)
        );
        let mut deep = String::from("if a:\n");
        for depth in 1..=100 {
            deep.push_str(&" ".repeat(depth));
            deep.push_str("if a:\n");
        }
        assert_eq!(error(&deep), ("too many levels of indentation".into(), 101));
    }

    #[test]
    fn a_tab_before_a_continuation_is_refused_only_in_used_indentation() {
        // The COMPATIBILITY.md row: CPython's indentation here is column 8, which
        // one tab cannot say to Ruff.
        let refused =
            prepass("def f():\n \\\n        def g():\n\t\\\nx = 1\n").expect_err("refused");
        assert!(refused.policy);
        // Where the two columns agree, the line is like any other.
        let text = "if a:\n       \t\\\n x = 1\n        y = 2\n";
        assert_eq!(repairs(text).tabs, [text.find('\t').expect("a tab")]);
        // CPython computes the same columns here and ignores them: inside
        // brackets, and before a comment or a blank line.
        for text in [
            "def f():\n    return (\n\t\\\n        1)\n",
            "def f():\n\t\\\n# comment\n    return 1\n",
            "def f():\n\t\\\n\n    return 1\n",
            "x = f'''{\n\t\\\n a}'''\n",
        ] {
            assert_eq!(repairs(text), Repairs::default(), "{text:?}");
        }
    }

    #[test]
    fn strict_errors_are_cpythons() {
        let cases: &[(&str, &str, u32)] = &[
            (
                "x = 012\n",
                "leading zeros in decimal integer literals are not permitted; use an 0o prefix for octal integers",
                1,
            ),
            ("x = 1abc\n", "invalid decimal literal", 1),
            ("x = 0x1g\n", "invalid hexadecimal literal", 1),
            ("\n0b2\n", "invalid digit '2' in binary literal", 2),
            ("x = 1jx\n", "invalid imaginary literal", 1),
            ("x = )\n", "unmatched ')'", 1),
            (
                "x = (]\n",
                "closing parenthesis ']' does not match opening parenthesis '('",
                1,
            ),
            (
                "x = (\n]\n",
                "closing parenthesis ']' does not match opening parenthesis '(' on line 1",
                2,
            ),
            ("x = f\"{)(}\"\n", "f-string: unmatched ')'", 1),
            ("x = (1,\ny = 2\n", "'(' was never closed", 1),
            ("x = 1 \\\n", "unexpected EOF while parsing", 1),
            (
                "x = '''abc\ndef\n",
                "unterminated triple-quoted string literal (detected at line 2)",
                1,
            ),
            (
                "x = 'abc\\'\n",
                "unterminated string literal (detected at line 1); perhaps you escaped the end quote?",
                1,
            ),
            (
                "x = f'''abc\n{d}\n",
                "unterminated triple-quoted f-string literal (detected at line 2)",
                1,
            ),
            (
                "x = t'abc\n",
                "unterminated t-string literal (detected at line 1)",
                1,
            ),
            (
                "f'{a:{b}{{x}}{c or d}}'\n",
                "f-string: single '}' is not allowed",
                1,
            ),
        ];
        for (text, message, line) in cases {
            assert_eq!(error(text), (message.to_string(), *line), "{text:?}");
        }
        // Only a warning in CPython: a keyword may follow a number.
        assert_eq!(repairs("x = 1if y else 2\n"), Repairs::default());
    }

    #[test]
    fn the_prepass_runs_where_the_lexers_could_differ() {
        for text in [
            "x = '{{'\n",
            "if a:\n\tx\n",
            "if a:\n \x0c x\n",
            "a\r\tb",
            "a \\\n\tb\n",
        ] {
            assert!(needs_prepass(text), "{text:?}");
        }
        for text in [
            "x = 1\n",
            "x = 'a\tb'\n",
            "x = {1: {2}}\n",
            "x = 1\t# c\n",
            "x\x0c\n",
        ] {
            assert!(!needs_prepass(text), "{text:?}");
        }
    }
}
