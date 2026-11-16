//! Parsing that accepts exactly what CPython 3.14's `ast.parse(str)` accepts.
//!
//! radon reports a file as an error whenever `ast.parse` raises, so the set of
//! accepted programs is part of its output. Ruff's parser differs at the edges:
//! - it accepts syntax from newer Python versions, which it reports separately
//!   as "unsupported syntax" for a target version;
//! - it skips a leading BOM and tolerates NUL bytes;
//! - it does not run the checks CPython makes while building the AST: the
//!   `__future__` features of a module's leading imports, and how deeply
//!   f-/t-string fields nest through format specs;
//! - its lexer reads two things differently from CPython's: `{{` in a format
//!   spec after a nested field, which CPython reads as text and Ruff as the
//!   start of another field, and tabs in indentation, which CPython measures
//!   with tab stops of 8 and 1 and Ruff with 2 and 1. On the files that could
//!   have either, a port of CPython's lexer runs first ([`rawtok::prepass`]).
//!
//! The wording of errors may differ from CPython's, but an error CPython's
//! lexer reports is worded by that port, in every file: where Ruff rejects a
//! file the port has not read, the port reads it then.
//!
//! `from __future__ import barry_as_FLUFL` (which enables `<>`) is not
//! supported: Ruff's grammar has no `<>`.

use ruff_python_ast::helpers::body_without_leading_docstring;
use ruff_python_ast::token::{TokenKind, Tokens};
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{
    Expr, InterpolatedStringElement, InterpolatedStringElements, ModModule, PythonVersion, Stmt,
};
use ruff_python_parser::{LexicalErrorType, Mode, ParseErrorType, ParseOptions, Parsed};
use ruff_text_size::{Ranged, TextSize};

use crate::error::Error;
use crate::limits::{MAX_EXPR_NESTING, MAX_FSTRING_LEVEL, MAX_INDENT, MAX_LEVEL, MAX_STR_DIGITS};
use crate::pyfmt::identifier_error;
use crate::rawtok;
use crate::source::{Source, syntax_error};

/// The Python version whose `ast.parse` oxradon matches.
const PYTHON_VERSION: PythonVersion = PythonVersion::PY314;

/// `__future__.all_feature_names` in CPython 3.14.
const FUTURE_FEATURES: [&str; 10] = [
    "nested_scopes",
    "generators",
    "division",
    "absolute_import",
    "with_statement",
    "print_function",
    "unicode_literals",
    "barry_as_FLUFL",
    "generator_stop",
    "annotations",
];

/// Parse `text` like CPython 3.14's `ast.parse`.
///
/// # Errors
/// The syntax error, laid out as CPython's `msg (<unknown>, line N)`. The
/// message and line are CPython's for a leading BOM, a NUL, an error its lexer
/// reports ([`rawtok::prepass`], which adds one refusal of oxradon's own) and
/// the ported checks; any other error has Ruff's.
pub(crate) fn parse(text: &str, src: &Source<'_>) -> Result<Parsed<ModModule>, Error> {
    if text.starts_with('\u{feff}') {
        return Err(syntax_error("invalid non-printable character U+FEFF", 1));
    }
    if text.contains('\0') {
        return Err(Error::new("source code string cannot contain null bytes"));
    }
    let lexer_error = |error: rawtok::LexError| syntax_error(&error.message, error.line);
    // Where Ruff's lexer could read the text differently from CPython's,
    // CPython's runs first: its error is the file's error, and otherwise the
    // bytes the two read differently are respelled for Ruff, in a copy of the
    // same length. Ruff never sees such a text before that lexer has read it
    // to the end. Every other text goes to Ruff as it is.
    let lexed = rawtok::needs_prepass(text);
    let repaired = if lexed {
        let repairs = rawtok::prepass(text).map_err(lexer_error)?;
        (!repairs.is_empty()).then(|| repairs.apply(text))
    } else {
        None
    };
    let (message, line, ruffs_unindent) =
        match parse_and_check(repaired.as_deref().unwrap_or(text), text, src) {
            Ok(parsed) => {
                // CPython's lexer must accept whatever is accepted here. On the
                // texts it read above it already has; a debug build runs it on
                // the others too.
                debug_assert!(
                    lexed || rawtok::prepass(text).is_ok(),
                    "accepted, but CPython's lexer fails: {:?}",
                    rawtok::prepass(text).err()
                );
                return Ok(parsed);
            }
            Err(error) => error,
        };
    // The text is an error. If CPython's lexer rejects it too, the error to
    // report is usually that lexer's, in CPython's words. (It has already
    // accepted a text it read above.) The exception approximates CPython's
    // (see `LexError::yields_before`): some lexer errors give way to a parser
    // error on an earlier line, for which Ruff's first error stands in. Ruff's own unindent error cannot: it is
    // the lexer's error, which the two place differently (after continuation
    // lines CPython has it on the line the indentation ends on, Ruff on the
    // line it starts on). Ruff's other lexical errors still stand in: CPython
    // reads `$` or `?` as a token and its parser reports it.
    if !lexed
        && let Err(lexical) = rawtok::prepass(text)
        && (ruffs_unindent || lexical.yields_before.is_none_or(|first| line >= first))
    {
        return Err(lexer_error(lexical));
    }
    Err(syntax_error(message, line))
}

/// Parse with Ruff and run the ported checks. Ruff reads `source`: `text`
/// itself, or its copy with the bytes Ruff would misread respelled. The
/// checks read `text`. Positions are the same in both.
///
/// # Errors
/// The first error's message and line, and whether it is the unindent error of Ruff's lexer.
fn parse_and_check(
    source: &str,
    text: &str,
    src: &Source<'_>,
) -> Result<Parsed<ModModule>, (String, u32, bool)> {
    let options = ParseOptions::from(Mode::Module).with_target_version(PYTHON_VERSION);
    let parsed = ruff_python_parser::parse_unchecked(source, options);
    if let Some(e) = parsed.errors().first() {
        let ruffs_unindent = matches!(
            e.error,
            ParseErrorType::Lexical(LexicalErrorType::IndentationError)
        );
        return Err((
            e.error.to_string(),
            src.line(e.location.start()),
            ruffs_unindent,
        ));
    }
    if let Some(e) = parsed.unsupported_syntax_errors().first() {
        return Err((e.to_string(), src.line(e.range.start()), false));
    }
    let parsed = parsed
        .try_into_module()
        .expect("module mode yields a module");
    let tokens = parsed.tokens();
    let scan = TokenScan::new(text, tokens);
    // The scan may only ever err toward running the exact checks.
    debug_assert!(scan.suspect || check_tokens(text, tokens).is_ok());
    debug_assert!(scan.has_templates || FieldDepthCheck::run(&parsed).is_ok());
    let token_checks = if scan.suspect {
        check_tokens(text, tokens)
    } else {
        Ok(())
    };
    token_checks
        .and_then(|()| check_future_imports(parsed.syntax()))
        .and_then(|()| {
            if scan.has_templates {
                FieldDepthCheck::run(&parsed)
            } else {
                Ok(())
            }
        })
        .map_err(|(message, at)| (message, src.line(at), false))?;
    Ok(parsed)
}

/// What one pass over the tokens rules out, so that the exact checks run only
/// on the rare file that could fail them. Nearly every file is nowhere near a
/// limit, and [`check_tokens`] branches on each token's kind, which is slow
/// over millions of tokens; this pass keeps the same counts with a table.
///
/// It may be wrong in one direction only: `suspect` must be true whenever
/// [`check_tokens`] would fail (a debug build asserts it on every file).
struct TokenScan {
    /// Some token could fail [`check_tokens`]: nesting passed a limit (or
    /// closed more than it opened), a token is long enough to be an integer
    /// literal with too many digits, or the text is not ASCII, so a name
    /// could hold a character CPython does not know.
    suspect: bool,
    /// There is an f-string or a t-string, so a field could nest too deep.
    has_templates: bool,
}

/// What each kind of token adds to the tokenizer's three stacks: open
/// brackets, indentation levels, and f-/t-string modes.
const STACK_DELTAS: [[i8; 3]; 256] = {
    let mut table = [[0; 3]; 256];
    table[TokenKind::Lpar as usize][0] = 1;
    table[TokenKind::Lsqb as usize][0] = 1;
    table[TokenKind::Lbrace as usize][0] = 1;
    table[TokenKind::Rpar as usize][0] = -1;
    table[TokenKind::Rsqb as usize][0] = -1;
    table[TokenKind::Rbrace as usize][0] = -1;
    table[TokenKind::Indent as usize][1] = 1;
    table[TokenKind::Dedent as usize][1] = -1;
    table[TokenKind::FStringStart as usize][2] = 1;
    table[TokenKind::TStringStart as usize][2] = 1;
    table[TokenKind::FStringEnd as usize][2] = -1;
    table[TokenKind::TStringEnd as usize][2] = -1;
    table
};

/// The highest count each stack may reach without failing [`check_tokens`],
/// which compares before it counts: an opening bracket fails at `MAX_LEVEL`,
/// an indent at `MAX_INDENT - 1`, and an f-/t-string start at
/// `MAX_FSTRING_LEVEL` modes (the regular mode included). Keep the two in
/// step.
// The limits are small constants, so the casts are exact.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
const STACK_LIMITS: [i32; 3] = [
    MAX_LEVEL as i32,
    MAX_INDENT as i32 - 1,
    MAX_FSTRING_LEVEL as i32,
];

impl TokenScan {
    fn new(text: &str, tokens: &Tokens) -> Self {
        // As `check_tokens` starts them: no bracket, no indent, one mode.
        let mut counts = [0_i32, 0, 1];
        // Negative once any count has passed its limit or gone below zero:
        // each term's sign bit is set in exactly those cases, and OR keeps it.
        // (Only unbalanced tokens go below zero, where `check_tokens`
        // saturates instead; it then decides.)
        let mut out_of_range = 0_i32;
        // OR never clears a bit, so this is non-zero once any f-/t-string
        // started or ended.
        let mut templates = 0_i8;
        let mut long_token = false;
        for token in tokens {
            let deltas = STACK_DELTAS[token.kind() as usize];
            for ((count, delta), limit) in counts.iter_mut().zip(deltas).zip(STACK_LIMITS) {
                *count += i32::from(delta);
                out_of_range |= (limit - *count) | *count;
            }
            templates |= deltas[2];
            // An integer literal with too many digits is at least this long.
            long_token |= token.range().len().to_usize() > MAX_STR_DIGITS;
        }
        Self {
            suspect: out_of_range < 0 || long_token || !text.is_ascii(),
            has_templates: templates != 0,
        }
    }
}

/// The checks CPython makes token by token that Ruff's lexer does not. They
/// run in token order, so among these the first failure in the file is the
/// one reported (a file that also fails a later check in [`parse`] is an
/// error either way; only the message can differ):
/// - the tokenizer's fixed-size stacks (`Parser/lexer/lexer.c`): brackets
///   nested past [`MAX_LEVEL`], indentation past [`MAX_INDENT`] levels, and
///   f-/t-strings nested past [`MAX_FSTRING_LEVEL`];
/// - identifiers ([`check_identifier`]) and decimal integer literals
///   ([`check_int_digits`]).
///
/// [`TokenScan`] decides whether this runs at all, so a check added here
/// needs its counterpart there.
fn check_tokens(text: &str, tokens: &Tokens) -> Result<(), (String, TextSize)> {
    let mut level = 0; // `tok->level`: open brackets, f-string fields included
    let mut indent = 0; // `tok->indent`
    let mut modes = 1; // the mode stack: regular mode, plus each open f-/t-string
    // Only a name with a non-ASCII character needs checking, and most files
    // have none.
    let ascii = text.is_ascii();
    for token in tokens {
        let too_deep = |what: &str| Err((format!("too many {what}"), token.start()));
        match token.kind() {
            TokenKind::Lpar | TokenKind::Lsqb | TokenKind::Lbrace => {
                if level >= MAX_LEVEL {
                    return too_deep("nested parentheses");
                }
                level += 1;
            }
            TokenKind::Rpar | TokenKind::Rsqb | TokenKind::Rbrace => {
                level = level.saturating_sub(1);
            }
            TokenKind::Indent => {
                if indent + 1 >= MAX_INDENT {
                    return too_deep("levels of indentation");
                }
                indent += 1;
            }
            TokenKind::Dedent => indent = indent.saturating_sub(1),
            TokenKind::FStringStart | TokenKind::TStringStart => {
                if modes >= MAX_FSTRING_LEVEL {
                    return too_deep("nested f-strings or t-strings");
                }
                modes += 1;
            }
            TokenKind::FStringEnd | TokenKind::TStringEnd => modes = modes.saturating_sub(1),
            TokenKind::Identifier if !ascii => {
                check_identifier(&text[token.range()], token.start())?;
            }
            TokenKind::Int => check_int_digits(&text[token.range()], token.start())?,
            _ => {}
        }
    }
    Ok(())
}

/// CPython's `verify_identifier`: a name must be an identifier by the
/// Unicode 16.0 tables. Ruff's tables are newer, so it also lexes names with
/// characters assigned since, which CPython 3.14 still sees as unassigned
/// (and so as not printable, which decides the message).
fn check_identifier(name: &str, at: TextSize) -> Result<(), (String, TextSize)> {
    // Most names in a file with a non-ASCII character are still ASCII, and
    // those Ruff has checked.
    if name.is_ascii() {
        return Ok(());
    }
    identifier_error(name).map_or(Ok(()), |message| Err((message, at)))
}

/// CPython converts each decimal integer literal while parsing, and the
/// conversion refuses more than [`MAX_STR_DIGITS`] digits (underscores
/// aside). Hexadecimal, octal and binary literals, and literals of zeros only,
/// are exempt; float and imaginary literals have no limit.
fn check_int_digits(literal: &str, at: TextSize) -> Result<(), (String, TextSize)> {
    let radix_prefix = literal.len() > 1
        && literal.starts_with('0')
        && matches!(
            literal.as_bytes()[1],
            b'x' | b'X' | b'o' | b'O' | b'b' | b'B'
        );
    if literal.len() <= MAX_STR_DIGITS || radix_prefix {
        return Ok(());
    }
    let digits = literal.bytes().filter(u8::is_ascii_digit).count();
    if digits > MAX_STR_DIGITS && literal.bytes().any(|b| matches!(b, b'1'..=b'9')) {
        let message = format!(
            "Exceeds the limit ({MAX_STR_DIGITS} digits) for integer string conversion: \
             value has {digits} digits; use sys.set_int_max_str_digits() to increase the \
             limit - Consider hexadecimal for huge integer literals to avoid decimal \
             conversion limits."
        );
        return Err((message, at));
    }
    Ok(())
}

/// CPython's `future_parse`: after an optional docstring, every leading
/// `from __future__ import` must name a known feature. Imports after the first
/// other statement are not checked by `ast.parse`.
fn check_future_imports(module: &ModModule) -> Result<(), (String, TextSize)> {
    // A docstring is a plain `str` literal (`_PyAST_GetDocString`), the test
    // Ruff's helper also uses.
    for stmt in body_without_leading_docstring(&module.body) {
        let Stmt::ImportFrom(import) = stmt else {
            break;
        };
        if import.level != 0 || import.module.as_deref() != Some("__future__") {
            break;
        }
        for alias in &import.names {
            let name = alias.name.as_str();
            if name == "braces" {
                return Err(("not a chance".to_owned(), alias.start()));
            }
            if !FUTURE_FEATURES.contains(&name) {
                return Err((
                    format!("future feature {name} is not defined"),
                    alias.start(),
                ));
            }
        }
    }
    Ok(())
}

/// Finds an f-/t-string field nested deeper than [`MAX_EXPR_NESTING`] through
/// format specs (`f"{a:{b:{c:{d}}}}"`), counting each f-/t-string on its own.
#[derive(Default)]
struct FieldDepthCheck {
    error: Option<(String, TextSize)>,
}

impl FieldDepthCheck {
    /// Walk the tree. Only a file with f-/t-strings needs it
    /// ([`TokenScan::has_templates`]).
    fn run(parsed: &Parsed<ModModule>) -> Result<(), (String, TextSize)> {
        let mut check = Self::default();
        check.visit_body(&parsed.syntax().body);
        check.error.map_or(Ok(()), Err)
    }

    fn check(&mut self, kind: &str, elements: &InterpolatedStringElements, depth: i32) {
        for element in elements {
            let InterpolatedStringElement::Interpolation(field) = element else {
                continue;
            };
            if depth > MAX_EXPR_NESTING {
                let message = format!("{kind}: expressions nested too deeply");
                self.error.get_or_insert((message, field.start()));
                return;
            }
            if let Some(spec) = &field.format_spec {
                self.check(kind, &spec.elements, depth + 1);
            }
        }
    }
}

impl<'a> Visitor<'a> for FieldDepthCheck {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if self.error.is_some() {
            return;
        }
        match expr {
            Expr::FString(node) => {
                for f in node.value.f_strings() {
                    self.check("f-string", &f.elements, 1);
                }
            }
            Expr::TString(node) => {
                for t in &node.value {
                    self.check("t-string", &t.elements, 1);
                }
            }
            _ => {}
        }
        visitor::walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, parse};
    use crate::source::Source;

    fn accepts(text: &str) -> bool {
        parse(text, &Source::new(text)).is_ok()
    }

    /// `{{` in a format spec after a nested field is text to CPython, and
    /// Ruff parses it as CPython reads it.
    #[test]
    fn format_spec_braces_follow_cpython() {
        assert!(accepts("x = f'{a:{b}{{x if y else z}}}'\n"));
        assert!(accepts("x = f'{a:{b}{{x}'\n"));
        assert!(accepts("x = t'{a:{b}{{x}'\n"));
        assert!(!accepts("x = f'{a:{b}{{x}}{c or d}}'\n"));
        // What follows the braces is text, however much it looks like code:
        // 198 open brackets, and a letter CPython 3.14 does not know.
        let brackets = format!("x = f'{{a:{{b}}{{{{{}}}'\n", "(".repeat(198));
        assert!(accepts(&brackets));
        assert!(accepts("x = f'{a:{b}{{\u{1e6c0}}'\n"));
    }

    /// Text CPython reads as literal never reaches Ruff's parser as code, and
    /// every repair is found in one pass.
    #[test]
    fn literal_text_costs_no_stack_and_no_reparse() {
        let nested = format!("{}1{}", "(".repeat(200_000), ")".repeat(200_000));
        let text = format!("def f():\n    return f'{{a:{{b}}{{{{{nested}}}}}}}'\n");
        assert!(accepts(&text));
        // After an error on an earlier line, Ruff is not called at all.
        assert!(!accepts(&format!("0b2\n{text}")));
        assert!(accepts(&"x = f'{a:{b}{{}}}'\n".repeat(100_000)));
    }

    /// An error CPython's lexer reports is worded as CPython words it, in any
    /// file; an error only the parser finds keeps Ruff's wording.
    #[test]
    fn lexical_errors_have_cpythons_wording() {
        let message = |text: &str| match parse(text, &Source::new(text)) {
            Ok(_) => String::from("accepted"),
            Err(error) => error.message().to_owned(),
        };
        assert_eq!(
            message("x = 0b2\n"),
            "invalid digit '2' in binary literal (<unknown>, line 1)"
        );
        assert_eq!(
            message("x = 1\ny = 'abc\n"),
            "unterminated string literal (detected at line 2) (<unknown>, line 2)"
        );
        assert_eq!(
            message("x = (1,\ny = 2\n"),
            "'(' was never closed (<unknown>, line 1)"
        );
        // The same error where the lexer runs first.
        assert_eq!(
            message("# {{\nx = 0b2\n"),
            "invalid digit '2' in binary literal (<unknown>, line 2)"
        );
        // Only the parser objects: Ruff's wording (CPython says "invalid
        // syntax"), whether or not the lexer ran first.
        assert_eq!(
            message("x = = 1\n"),
            "Expected an expression (<unknown>, line 1)"
        );
        assert_eq!(
            message("# {{\nx = = 1\n"),
            "Expected an expression (<unknown>, line 2)"
        );
    }

    /// Where the parser fails on an earlier line than the lexer, CPython
    /// reports the lexer's error if the lexer raised it, and the parser's if
    /// the lexer only returned a status (or was inside an f-string).
    #[test]
    fn a_parser_error_before_a_lexer_error_follows_cpython() {
        let line = |text: &str| {
            let error = parse(text, &Source::new(text)).expect_err("an error");
            let message = error.message();
            message[message.rfind("line ").expect("a line") + 5..message.len() - 1].to_owned()
        };
        // Raised: the lexer's error wins, on its own line.
        assert_eq!(line("1 = x\ny = 0b2\n"), "2");
        assert_eq!(line("x = = 1\ny = 'abc\n"), "2");
        assert_eq!(line("x = = 1\ny = )\n"), "2");
        // A status, or inside an f-string: the parser's error stays.
        assert_eq!(line("x = = 1\nif a:\n        y = 1\n    z = 2\n"), "1");
        assert_eq!(line("x = = 1\ny = 1 \\ 2\n"), "1");
        assert_eq!(line("x = = 1\ny = (1,\n"), "1");
        assert_eq!(line("x = = 1\ny = f'}'\n"), "1");
        // With no parser error before it, a status is the lexer's error.
        assert_eq!(line("x = (1,\ny = = 2\n"), "1");
        assert_eq!(line("if a:\n        y = 1\n    z = 2\nx = = 1\n"), "3");
        // Ruff's own lexer reports the same unindent on the line the continuation starts on
        // (4); CPython reports it where the line ends (6), and that is not a parser error.
        let continued = "def f():\n\\\n    y = x or z\n\\\n\\\n  with d:\n  \\\n  y = x or z\n";
        assert_eq!(line(continued), "6");
        // Ruff's lexer rejects `$` and `?`; CPython's parser does, on that line.
        assert_eq!(line("a = $\nb = 1\nif x:\n        y\n    z\n"), "1");
        assert_eq!(line("a = ?\nif x:\n        y\n    z\n"), "1");
    }

    /// CPython measures a tab in indentation with tab stops of 8 and of 1.
    #[test]
    fn tab_indentation_follows_cpython() {
        assert!(accepts("if a:\n\tx = 1\n\ty = 2\n"));
        // One level, spelled two ways.
        assert!(accepts("def f():\n    \t\tpass\n \t   \tpass\n"));
        assert!(!accepts("def f():\n\tif a:\n   return 1\n"));
        assert!(!accepts("if a:\n\tx = 1\n        y = 2\n"));
        assert!(!accepts("def f():\n\t\x0c\tdef g():\n    x = 1\n"));
        // The COMPATIBILITY.md row.
        assert!(!accepts("def f():\n \\\n        def g():\n\t\\\nx = 1\n"));
    }

    #[test]
    fn newer_syntax_is_rejected() {
        assert!(!accepts("def f(): return [*x for x in y]\n"));
        assert!(!accepts("lazy import x\n"));
    }

    #[test]
    fn bom_and_nul_are_rejected() {
        assert!(!accepts("\u{feff}x = 1\n"));
        assert!(!accepts("s = 'a\0b'\n"));
    }

    #[test]
    fn future_features_follow_cpython() {
        assert!(accepts("from __future__ import annotations\n"));
        assert!(!accepts("from __future__ import braces\n"));
        assert!(!accepts("from __future__ import annotations, nope\n"));
        assert!(!accepts("from __future__ import *\n"));
        assert!(!accepts("\"\"\"doc\"\"\"\nfrom __future__ import nope\n"));
        // Only the leading block is checked by `ast.parse`.
        assert!(accepts("x = 1\nfrom __future__ import nope\n"));
        assert!(accepts("b'doc'\nfrom __future__ import nope\n"));
        assert!(accepts("def f():\n    from __future__ import nope\n"));
        assert!(accepts("from .__future__ import nope\n"));
    }

    #[test]
    fn decimal_int_digits_are_limited() {
        let assign = |digits: &str| format!("x = {digits}\n");
        assert!(accepts(&assign(&"1".repeat(4300))));
        assert!(!accepts(&assign(&"1".repeat(4301))));
        // Underscores don't count, but the digits around them do.
        assert!(!accepts(&assign(&vec!["1"; 4301].join("_"))));
        assert!(accepts(&assign(&"0".repeat(4301))));
        assert!(accepts(&assign(&format!("0x{}", "f".repeat(4301)))));
        assert!(accepts(&assign(&format!("{}j", "1".repeat(4301)))));
        // Never visited by `cc` or `hal`, but parsed all the same.
        assert!(!accepts(&format!("def f(a={}): pass\n", "1".repeat(4301))));
    }

    /// One past each of the tokenizer's stack sizes, as CPython 3.14 rejects
    /// them (`scripts/check_acceptance.py` checks the same boundaries against
    /// it).
    #[test]
    fn tokenizer_limits_follow_cpython() {
        let brackets = |n: usize| format!("x = {}1{}\n", "(".repeat(n), ")".repeat(n));
        assert!(accepts(&brackets(200)));
        assert!(!accepts(&brackets(201)));
        // Every kind of bracket counts, and so does an f-string field's brace.
        let mixed = |n: usize| format!("x = {}{}\n", "[{".repeat(n), "}]".repeat(n));
        assert!(accepts(&mixed(100)));
        assert!(!accepts(&format!(
            "x = [{}{}]\n",
            "[{".repeat(100),
            "}]".repeat(100)
        )));
        let in_field = |n: usize| format!("f'{{{}1{}}}'\n", "(".repeat(n), ")".repeat(n));
        assert!(accepts(&in_field(199)));
        assert!(!accepts(&in_field(200)));

        let blocks = |n: usize| {
            let mut code = String::new();
            for depth in 0..n {
                code.push_str(&" ".repeat(depth));
                code.push_str("if x:\n");
            }
            code.push_str(&" ".repeat(n));
            code.push_str("pass\n");
            code
        };
        assert!(accepts(&blocks(99)));
        assert!(!accepts(&blocks(100)));
        // A dedent frees a level.
        assert!(accepts(&format!("{}{}", blocks(99), blocks(99))));

        let strings = |n: usize| format!("x = {}1{}\n", "f\"{".repeat(n), "}\"".repeat(n));
        assert!(accepts(&strings(149)));
        assert!(!accepts(&strings(150)));
    }

    /// The one thing the scan must never do is clear a token stream that the
    /// exact checks reject. `parse` asserts it on every file it accepts; this
    /// also covers what Ruff rejects first, where brackets close more than
    /// they open and the two ways of counting part.
    #[test]
    fn the_scan_never_clears_what_the_exact_checks_reject() {
        use super::{TokenScan, check_tokens};
        use ruff_python_parser::{Mode, ParseOptions, parse_unchecked};

        let brackets = |n: usize| format!("x = {}1{}\n", "(".repeat(n), ")".repeat(n));
        let strings = |n: usize| format!("x = {}1{}\n", "f\"{".repeat(n), "}\"".repeat(n));
        let blocks = |n: usize| {
            let mut code = String::new();
            for depth in 0..=n {
                code.push_str(&" ".repeat(depth));
                code.push_str(if depth < n { "if x:\n" } else { "pass\n" });
            }
            code
        };
        let mut sources = vec![
            ")))\nx = (\n".to_owned(),
            "]\n".to_owned(),
            format!(")))){}", brackets(201)),
            format!("}}\"{}", strings(150)),
            format!("x = {}\n", "1".repeat(4301)),
            format!("x = '{}'\n", "a".repeat(5000)),
            format!(
                "x{} = 1\n",
                char::from_u32(0x1_e6c0).expect("a scalar value")
            ),
            "caf\u{e9} = 1\n".to_owned(),
        ];
        for n in [199, 200, 201, 202] {
            sources.push(brackets(n));
            sources.push(format!("f'{{{}}}'\n", brackets(n).trim()));
        }
        for n in [98, 99, 100, 101] {
            sources.push(blocks(n));
            sources.push(format!("{}{}", blocks(n), blocks(99)));
        }
        for n in [148, 149, 150, 151] {
            sources.push(strings(n));
        }
        let mut rejected = 0;
        for text in &sources {
            let parsed = parse_unchecked(text, ParseOptions::from(Mode::Module));
            let scan = TokenScan::new(text, parsed.tokens());
            let exact = check_tokens(text, parsed.tokens());
            assert!(scan.suspect || exact.is_ok(), "{text:.60}");
            rejected += usize::from(exact.is_err());
        }
        // The list really does hold streams the exact checks reject.
        assert!(rejected >= 8, "{rejected}");
        // And ordinary code skips them.
        let plain = "def f(a, b):\n    return [a, {b: f'{a}'}]\n";
        let parsed = parse_unchecked(plain, ParseOptions::from(Mode::Module));
        let scan = TokenScan::new(plain, parsed.tokens());
        assert!(!scan.suspect && scan.has_templates);
    }

    #[test]
    fn tokenizer_limit_messages() {
        let message = |text: &str| {
            parse(text, &Source::new(text))
                .err()
                .map(Error::into_message)
        };
        let deep = format!("x = 1\ny = {}1{}\n", "(".repeat(201), ")".repeat(201));
        let expected = "too many nested parentheses (<unknown>, line 2)";
        assert_eq!(message(&deep).as_deref(), Some(expected));

        // The line of the statement indented too far, as radon reports it.
        let mut blocks = String::new();
        for depth in 0..100 {
            blocks.push_str(&" ".repeat(depth));
            blocks.push_str("if x:\n");
        }
        blocks.push_str(&" ".repeat(100));
        blocks.push_str("pass\n");
        let expected = "too many levels of indentation (<unknown>, line 101)";
        assert_eq!(message(&blocks).as_deref(), Some(expected));

        let strings = format!(
            "x = 1\n\ny = {}1{}\n",
            "f\"{".repeat(150),
            "}\"".repeat(150)
        );
        let expected = "too many nested f-strings or t-strings (<unknown>, line 3)";
        assert_eq!(message(&strings).as_deref(), Some(expected));
    }

    /// U+1E6C0 was assigned in Unicode 17.0: CPython 3.14 (Unicode 16.0)
    /// rejects it in a name, though not in a string or a comment.
    #[test]
    fn identifiers_use_unicode_16() {
        let message = |text: &str| {
            parse(text, &Source::new(text))
                .err()
                .map(Error::into_message)
        };
        let new_letter = char::from_u32(0x1_e6c0).expect("a scalar value");
        let expected = "invalid non-printable character U+1E6C0 (<unknown>, line 2)";
        let named = format!("y = 1\nx{new_letter} = 1\n");
        assert_eq!(message(&named).as_deref(), Some(expected));
        assert!(accepts(&format!("x = '{new_letter}'  # {new_letter}\n")));
        assert!(accepts("caf\u{e9} = 1\n"));
    }

    #[test]
    fn field_depth_follows_cpython() {
        assert!(accepts("f'{x:{y:{z}}}'\n"));
        assert!(!accepts("f'{x:{y:{z:{w}}}}'\n"));
        assert!(!accepts("t'{x:{y:{z:{w}}}}'\n"));
        // Each f-string counts on its own.
        assert!(accepts("f'{x:{f\"{y:{z:{w}}}\"}}'\n"));
        assert!(!accepts("f'{f\"{x:{y:{z:{w}}}}\"}'\n"));
    }
}
