//! Python-compatible text: `repr()`, `str.format()`, `str.strip()` and the
//! case mappings, `json.dumps` strings, and the `str` Python sees for a path
//! (and the path for its bytes).

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::LazyLock;

use regex::Regex;

/// Python's `repr()` of a float: the shortest digits that round-trip, and
/// among equally short candidates the closest, ties going to the even digit
/// (`_Py_dg_dtoa` mode 0); fixed notation when the decimal point falls within
/// `-4 < decpt <= 16`, otherwise `1e+16` / `1.5e-05` style.
///
/// Rust's shortest formatting finds the same digits except at an exact tie,
/// where it can round the other way (`1000000000000000.25` is `.3` in Rust and
/// `.2` in Python), so ties are detected from the exact decimal expansion.
pub(crate) fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x.is_infinite() {
        return format!("{sign}inf");
    }
    if x == 0.0 {
        return format!("{sign}0.0");
    }
    let (digits, exponent) = shortest_digits(x.abs());
    format!("{sign}{}", python_layout(&digits, exponent))
}

/// The significant digits and the exponent of the first digit of `x` (> 0),
/// as Python's `repr` chooses them.
fn shortest_digits(x: f64) -> (String, i32) {
    let (digits, exponent) = scientific(&format!("{x:e}"));
    let n = digits.len();
    // A tie needs the exact value to be those n digits and a final 5, so `x`
    // correctly rounded to n + 2 digits then ends in "50" (with the same
    // exponent). That is checked first: it rules out nearly every value without
    // the exact expansion, which is slow to produce. Precision n + 1 is n + 2
    // significant digits.
    let (near, near_exponent) = scientific(&format!("{x:.*e}", n + 1));
    if near_exponent != exponent || !near.ends_with("50") {
        return (digits, exponent);
    }
    break_tie(x, digits, exponent)
}

/// Rust's shortest `digits` for `x`, or the rival Python picks when the exact
/// value is a tie between two of them: the even one, if it also round-trips.
/// The tie is found in the exact decimal expansion, which an f64 always has in
/// fewer than 1100 digits.
fn break_tie(x: f64, digits: String, exponent: i32) -> (String, i32) {
    let n = digits.len();
    let (exact, exact_exponent) = scientific(&format!("{x:.1100e}"));
    let exact = exact.trim_end_matches('0');
    let tie = exact_exponent == exponent && exact.len() == n + 1 && exact.ends_with('5');
    if !tie {
        return (digits, exponent);
    }
    let lower = exact[..n].to_owned();
    let other = if digits == lower {
        match increment(&lower) {
            Some(upper) => upper,
            None => return (digits, exponent), // 99..9 + 1 is longer: not a rival
        }
    } else {
        lower
    };
    let even = |d: &str| {
        d.bytes()
            .last()
            .is_some_and(|b| (b - b'0').is_multiple_of(2))
    };
    let round_trips =
        |d: &str| format!("{}.{}e{exponent}", &d[..1], &d[1..]).parse::<f64>() == Ok(x);
    if !even(&digits) && even(&other) && round_trips(&other) {
        (other, exponent)
    } else {
        (digits, exponent)
    }
}

/// Split Rust's `{:e}` output (`1.25e3`) into digits (`125`) and exponent.
fn scientific(text: &str) -> (String, i32) {
    let (mantissa, exponent) = text.split_once('e').expect("scientific notation");
    let digits = mantissa.replace('.', "");
    (digits, exponent.parse().expect("an exponent"))
}

/// Add one unit in the last place, or `None` if that adds a digit.
fn increment(digits: &str) -> Option<String> {
    let mut bytes = digits.as_bytes().to_vec();
    for b in bytes.iter_mut().rev() {
        if *b == b'9' {
            *b = b'0';
        } else {
            *b += 1;
            return String::from_utf8(bytes).ok();
        }
    }
    None
}

/// Python's `repr` layout of `0.DIGITS * 10**(exponent + 1)`.
fn python_layout(digits: &str, exponent: i32) -> String {
    let n = i32::try_from(digits.len()).expect("few digits");
    let decpt = exponent + 1;
    if -4 < decpt && decpt <= 16 {
        if decpt <= 0 {
            let zeros = "0".repeat(usize::try_from(-decpt).unwrap_or(0));
            format!("0.{zeros}{digits}")
        } else if decpt >= n {
            let zeros = "0".repeat(usize::try_from(decpt - n).unwrap_or(0));
            format!("{digits}{zeros}.0")
        } else {
            let (int, frac) = digits.split_at(usize::try_from(decpt).unwrap_or(0));
            format!("{int}.{frac}")
        }
    } else {
        let (first, rest) = digits.split_at(1);
        let point = if rest.is_empty() {
            String::new()
        } else {
            format!(".{rest}")
        };
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{first}{point}e{sign}{:02}", exponent.unsigned_abs())
    }
}

/// The Unicode 16.0.0 character classes Python's case mappings depend on.
/// The `regex` crate's tables are that version, as is CPython 3.14's
/// database; Rust's own case tables are newer, so they map characters that
/// CPython still treats as unassigned. Keep these aligned on upgrades.
static UNASSIGNED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\p{Cn}").expect("valid pattern"));
static CASED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{Cased}").expect("valid pattern"));
static CASE_IGNORABLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\p{Case_Ignorable}").expect("valid pattern"));

fn has(class: &Regex, c: char) -> bool {
    class.is_match(c.encode_utf8(&mut [0; 4]))
}

/// Whether `c` is unassigned in Unicode 16.0, the version CPython 3.14 knows.
pub(crate) fn is_unassigned(c: char) -> bool {
    !c.is_ascii() && has(&UNASSIGNED, c)
}

/// One character's case mapping as CPython 3.14 has it: Rust's mapping when
/// the character and everything it maps to are in Unicode 16.0; otherwise
/// none (a newer version gave it a partner, e.g. U+A7D3 → U+A7D2).
fn map_case<I: Iterator<Item = char>>(out: &mut String, c: char, mapping: impl Fn(char) -> I) {
    let current = |c: char| !is_unassigned(c);
    if current(c) && mapping(c).all(current) {
        out.extend(mapping(c));
    } else {
        out.push(c);
    }
}

/// Python's `str.upper()`: full case mappings (`ß` → `SS`), Unicode 16.0.
pub(crate) fn py_upper(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        map_case(&mut out, c, char::to_uppercase);
    }
    out
}

/// Python's `str.lower()`: full case mappings, Unicode 16.0, and CPython's
/// `handle_capital_sigma` rule for `Σ`.
pub(crate) fn py_lower(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '\u{3a3}' {
            out.push(if final_sigma(&chars, i) {
                '\u{3c2}'
            } else {
                '\u{3c3}'
            });
        } else {
            map_case(&mut out, c, char::to_lowercase);
        }
    }
    out
}

/// `Σ` at `i` is final when a cased letter precedes it and none follows,
/// skipping case-ignorable characters both ways.
fn final_sigma(chars: &[char], i: usize) -> bool {
    let before = chars[..i].iter().rev().find(|&&c| !has(&CASE_IGNORABLE, c));
    if !before.is_some_and(|&c| has(&CASED, c)) {
        return false;
    }
    let after = chars[i + 1..].iter().find(|&&c| !has(&CASE_IGNORABLE, c));
    !after.is_some_and(|&c| has(&CASED, c))
}

/// Python's `str.isspace()` for one character, which is also what `\s`
/// matches in a `str` pattern.
pub(crate) fn py_isspace(c: char) -> bool {
    matches!(
        c,
        '\t'..='\r'
            | '\x1c'..='\x1f'
            | ' '
            | '\u{85}'
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

/// `str.strip()` with no argument.
pub(crate) fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

/// `str.lstrip()` with no argument.
pub(crate) fn py_lstrip(s: &str) -> &str {
    s.trim_start_matches(py_isspace)
}

/// `str.rstrip()` with no argument.
pub(crate) fn py_rstrip(s: &str) -> &str {
    s.trim_end_matches(py_isspace)
}

/// Python's `text.format()` with no arguments: `{{` and `}}` become single
/// braces. Returns `None` where Python raises (a replacement field, or a
/// lone `}`).
pub(crate) fn format_without_args(text: &str) -> Option<Cow<'_, str>> {
    if !text.contains(['{', '}']) {
        return Some(Cow::Borrowed(text));
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '{' | '}' if chars.next() == Some(c) => out.push(c),
            '{' | '}' => return None,
            _ => out.push(c),
        }
    }
    Some(Cow::Owned(out))
}

/// Python's text for a path, as near as a Rust string allows: each byte that
/// is not valid UTF-8 becomes one U+FFFD (Python's `surrogateescape` makes it
/// one lone surrogate, which `--exclude` patterns and output then see).
pub(crate) fn path_text(path: &OsStr) -> Cow<'_, str> {
    if let Some(text) = path.to_str() {
        return Cow::Borrowed(text);
    }
    let mut out = String::new();
    for chunk in path.as_encoded_bytes().utf8_chunks() {
        out.push_str(chunk.valid());
        out.extend(std::iter::repeat_n('\u{fffd}', chunk.invalid().len()));
    }
    Cow::Owned(out)
}

/// A path from its bytes: the inverse of what [`path_text`] reads.
#[cfg(unix)]
pub(crate) fn os_string_from_bytes(bytes: Vec<u8>) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes)
}

/// Non-Unix platforms are not supported; keep paths usable but lossy.
#[cfg(not(unix))]
pub(crate) fn os_string_from_bytes(bytes: Vec<u8>) -> OsString {
    String::from_utf8_lossy(&bytes).into_owned().into()
}

/// `str(UnicodeEncodeError)` from encoding `value`, decoded with
/// `surrogateescape` as `os.environ` decodes it, back to strict UTF-8, or
/// `None` when it is valid UTF-8 and encodes cleanly.
///
/// Each invalid byte is one lone surrogate; the error covers the first run of
/// consecutive surrogates, with positions counted in code points.
pub(crate) fn utf8_encode_error(value: &OsStr) -> Option<String> {
    let mut position = 0;
    // (start, first byte, length) of the first run of surrogates.
    let mut run: Option<(usize, u8, usize)> = None;
    for chunk in value.as_encoded_bytes().utf8_chunks() {
        if !chunk.valid().is_empty() {
            if run.is_some() {
                break;
            }
            position += chunk.valid().chars().count();
        }
        if let Some(&first) = chunk.invalid().first() {
            let (_, _, len) = run.get_or_insert((position, first, 0));
            *len += chunk.invalid().len();
            position += chunk.invalid().len();
        }
    }
    let (start, first, len) = run?;
    Some(if len == 1 {
        format!(
            "'utf-8' codec can't encode character '\\udc{first:02x}' in position {start}: \
             surrogates not allowed"
        )
    } else {
        let end = start + len - 1;
        format!(
            "'utf-8' codec can't encode characters in position {start}-{end}: \
             surrogates not allowed"
        )
    })
}

/// Python's `repr()` of a string or path (a path as the `str` it decodes to
/// with `surrogateescape`): Python's quoting, escapes, and printable test
/// ([`is_printable`]).
pub(crate) fn py_repr(text: impl AsRef<OsStr>) -> String {
    let bytes = text.as_ref().as_encoded_bytes();
    let quote = if bytes.contains(&b'\'') && !bytes.contains(&b'"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push(quote);
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                c if c == quote => {
                    out.push('\\');
                    out.push(c);
                }
                ' '..='~' => out.push(c),
                c if !c.is_ascii() && is_printable(c) => out.push(c),
                c => {
                    let n = u32::from(c);
                    let _ = match n {
                        0..=0xff => write!(out, "\\x{n:02x}"),
                        0x100..=0xffff => write!(out, "\\u{n:04x}"),
                        _ => write!(out, "\\U{n:08x}"),
                    };
                }
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(out, "\\udc{byte:02x}");
        }
    }
    out.push(quote);
    out
}

/// Python's `str.isprintable()` for one character: false for the Unicode
/// categories Other (C*) and Separator (Z*), except the ASCII space. The
/// `regex` crate's tables and CPython 3.14 both use Unicode 16.0.0, so this is
/// exact (keep the two aligned when either is upgraded).
fn is_printable(c: char) -> bool {
    static NOT_PRINTABLE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[\p{C}\p{Z}]").expect("valid pattern"));
    c == ' ' || !NOT_PRINTABLE.is_match(c.encode_utf8(&mut [0; 4]))
}

/// CPython 3.14's `verify_identifier` on a name that has a non-ASCII
/// character: the error for the first character that may not stand where it
/// does, if there is one.
///
/// The rule is `_PyUnicode_ScanIdentifier`'s: `_` or `XID_Start` first, then
/// `XID_Continue`, by Unicode 16.0. `unicode-ident`'s tables are newer (they are
/// the ones Ruff's lexer uses), so characters assigned since are taken out;
/// `scripts/check_identifiers.py` checks the result against CPython.
pub(crate) fn identifier_error(name: &str) -> Option<String> {
    let (_, bad) = name.chars().enumerate().find(|&(i, c)| {
        let known = if i == 0 {
            c == '_' || unicode_ident::is_xid_start(c)
        } else {
            unicode_ident::is_xid_continue(c)
        };
        !known || is_unassigned(c)
    })?;
    let code = u32::from(bad);
    Some(if is_printable(bad) {
        format!("invalid character '{bad}' (U+{code:04X})")
    } else {
        format!("invalid non-printable character U+{code:04X}")
    })
}

/// Write `s` as a JSON string literal, escaped like Python's `json.dumps`
/// with `ensure_ascii=True`.
pub(crate) fn write_json_str(out: &mut impl Write, s: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    write_json_chars(out, s)?;
    out.write_all(b"\"")
}

/// Write a path as a JSON string literal the way Python would after decoding
/// it with `surrogateescape`: each byte that is not valid UTF-8 becomes the
/// lone surrogate `\udcXX`.
pub(crate) fn write_json_os_str(out: &mut impl Write, s: &OsStr) -> io::Result<()> {
    out.write_all(b"\"")?;
    for chunk in s.as_encoded_bytes().utf8_chunks() {
        write_json_chars(out, chunk.valid())?;
        for byte in chunk.invalid() {
            write!(out, "\\udc{byte:02x}")?;
        }
    }
    out.write_all(b"\"")
}

/// The body of a JSON string literal, with Python's escaping.
fn write_json_chars(out: &mut impl Write, s: &str) -> io::Result<()> {
    let mut plain_from = 0;
    for (i, c) in s.char_indices() {
        if matches!(c, ' '..='~') && c != '"' && c != '\\' {
            continue;
        }
        out.write_all(&s.as_bytes()[plain_from..i])?;
        match c {
            '"' => out.write_all(b"\\\"")?,
            '\\' => out.write_all(b"\\\\")?,
            '\n' => out.write_all(b"\\n")?,
            '\r' => out.write_all(b"\\r")?,
            '\t' => out.write_all(b"\\t")?,
            '\x08' => out.write_all(b"\\b")?,
            '\x0c' => out.write_all(b"\\f")?,
            _ => {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    write!(out, "\\u{unit:04x}")?;
                }
            }
        }
        plain_from = i + c.len_utf8();
    }
    out.write_all(&s.as_bytes()[plain_from..])
}

#[cfg(test)]
mod tests {
    use super::{float_repr, format_without_args, py_isspace, py_repr, write_json_str};

    /// `str.isspace()` is wider than ASCII whitespace, and narrower than
    /// "invisible": a zero-width space is not one.
    #[test]
    fn whitespace_is_pythons() {
        for c in ['\x1c', '\x1f', '\u{85}', '\u{2028}', '\u{3000}'] {
            assert!(py_isspace(c), "{c:?}");
        }
        assert!(!py_isspace('\u{200b}'));
    }

    /// Characters that gained a case partner after Unicode 16.0: CPython 3.14
    /// maps them to themselves. (A sweep of every code point against
    /// CPython's `lower()`/`upper()` matched.)
    #[test]
    fn case_mappings_stop_at_unicode_16() {
        for c in [
            '\u{277}', '\u{27c}', '\u{a7d3}', '\u{a7d5}', '\u{ab4b}', '\u{ab4c}', '\u{a7ce}',
        ] {
            assert_eq!(super::py_upper(&c.to_string()), c.to_string(), "{c:?}");
            assert_eq!(super::py_lower(&c.to_string()), c.to_string(), "{c:?}");
        }
        assert_eq!(super::py_upper("stra\u{df}e"), "STRASSE");
        assert_eq!(super::py_lower("\u{130}"), "i\u{307}");
    }

    /// Recorded from CPython 3.14 `str.lower()`.
    #[test]
    fn lower_applies_pythons_final_sigma_rule() {
        let cases = [
            ("\u{3a3}\u{391}\u{3a3}", "\u{3c3}\u{3b1}\u{3c2}"),
            ("\u{391}\u{3a3}", "\u{3b1}\u{3c2}"),
            ("\u{3a3}", "\u{3c3}"),
            ("\u{391}\u{3a3}.", "\u{3b1}\u{3c2}."),
            ("\u{391}\u{301}\u{3a3}", "\u{3b1}\u{301}\u{3c2}"),
            ("\u{391}\u{3a3}\u{391}", "\u{3b1}\u{3c3}\u{3b1}"),
            ("a\u{345}\u{3a3}", "a\u{345}\u{3c2}"),
        ];
        for (text, want) in cases {
            assert_eq!(super::py_lower(text), want, "{text:?}");
        }
        // Assigned only after Unicode 16.0: CPython 3.14 leaves them alone.
        assert_eq!(super::py_lower("\u{a7ce}"), "\u{a7ce}");
    }

    /// Exact halfway ties, where Rust's shortest digits round the other way.
    /// Expected values from CPython 3.14 `repr`. A sweep of 21,805 floats
    /// (random bit patterns and quarter values at every scale) also matched.
    #[test]
    fn float_repr_breaks_ties_like_python() {
        let cases = [
            // Exact sums: x.25 at these magnitudes is representable.
            (1e15 + 0.25, "1000000000000000.2"),
            (562_949_953_421_312.0 + 0.25, "562949953421312.2"),
            (-(1e15 + 0.25), "-1000000000000000.2"),
            (1e22, "1e+22"),
            (5e-324, "5e-324"),
            (9_007_199_254_740_992.0, "9007199254740992.0"),
        ];
        for (x, want) in cases {
            assert_eq!(float_repr(x), want, "{x:e}");
        }
    }

    /// [`super::shortest_digits`] without its quick check: the exact expansion
    /// every time.
    fn shortest_digits_exact(x: f64) -> (String, i32) {
        let (digits, exponent) = super::scientific(&format!("{x:e}"));
        super::break_tie(x, digits, exponent)
    }

    /// The quick check never changes the answer: random bit patterns, values
    /// that are exact short decimals (where ties live), and their neighbours.
    #[test]
    fn quick_tie_check_matches_the_exact_expansion() {
        const SAMPLES: usize = 20_000;
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut values = Vec::new();
        for _ in 0..SAMPLES {
            let x = f64::from_bits(next());
            if x.is_finite() && x != 0.0 {
                values.push(x.abs());
            }
        }
        for _ in 0..SAMPLES {
            // m / 2^k (m below 2^32, k below 12), times 10^-20..10^19: values
            // with short exact expansions, where ties live.
            let m = f64::from(u32::try_from(next() % 1_000_000_007).expect("below 2^32"));
            let k = u32::try_from(next() % 12).expect("small");
            let p = (next() % 40) as i32 - 20;
            let x = m / f64::from(2_u32.pow(k)) * 10f64.powi(p);
            if x.is_finite() && x != 0.0 {
                values.extend([
                    x,
                    f64::from_bits(x.to_bits() + 1),
                    f64::from_bits(x.to_bits() - 1),
                ]);
            }
        }
        values.extend([
            1e15 + 0.25,
            562_949_953_421_312.0 + 0.25,
            1e22,
            5e-324,
            9_007_199_254_740_992.0,
        ]);
        for x in values {
            assert_eq!(super::shortest_digits(x), shortest_digits_exact(x), "{x:e}");
        }
    }

    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (3.0, "3.0"),
            (2.0 / 3.0 + 2.0, "2.6666666666666665"),
            (0.1, "0.1"),
            (-0.5, "-0.5"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (0.0001, "0.0001"),
            (1.5e-5, "1.5e-05"),
            (123_456_789.25, "123456789.25"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.797_693_134_862_315_7e308, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
            (f64::INFINITY, "inf"),
            (f64::NAN, "nan"),
        ];
        for (x, want) in cases {
            assert_eq!(float_repr(x), want);
        }
    }

    #[test]
    fn json_strings_match_python() {
        let mut out = Vec::new();
        write_json_str(&mut out, "caf\u{e9} \"q\"\\\n\u{1f600}\x01").unwrap();
        // `u` escapes are spelled with `{bs}` so the expected text stays ASCII.
        let want = format!(
            r#""caf{bs}u00e9 \"q\"\\\n{bs}ud83d{bs}ude00{bs}u0001""#,
            bs = '\\'
        );
        assert_eq!(String::from_utf8(out).unwrap(), want);
    }

    #[test]
    fn str_format_without_args() {
        assert_eq!(format_without_args("a{{b}}.py").as_deref(), Some("a{b}.py"));
        assert!(matches!(
            format_without_args("plain"),
            Some(std::borrow::Cow::Borrowed(_))
        ));
        assert_eq!(format_without_args("plain").as_deref(), Some("plain"));
        assert_eq!(format_without_args("a{b}.py"), None, "KeyError in Python");
        assert_eq!(format_without_args("a{}.py"), None, "IndexError in Python");
        assert_eq!(format_without_args("a}.py"), None, "ValueError in Python");
    }

    #[cfg(unix)]
    #[test]
    fn json_paths_use_surrogate_escapes() {
        use std::os::unix::ffi::OsStrExt;
        let mut out = Vec::new();
        let path = std::ffi::OsStr::from_bytes(b"a\xff\xc3\xa9.py");
        super::write_json_os_str(&mut out, path).unwrap();
        // Spelled with `{bs}` so the expected text stays ASCII.
        let want = format!(r#""a{bs}udcff{bs}u00e9.py""#, bs = '\\');
        assert_eq!(String::from_utf8(out).unwrap(), want);
    }

    /// Expected values are Python 3.14's `repr()` of the same strings.
    #[test]
    fn repr_matches_python() {
        use std::ffi::OsStr;
        let bs = '\\';
        let cases = [
            ("a'b", "\"a'b\"".to_owned()),
            ("a\"b", "'a\"b'".to_owned()),
            ("a'\"b", format!("'a{bs}'\"b'")),
            ("a\\b", format!("'a{bs}{bs}b'")),
            ("a\rb\nc\td", format!("'a{bs}rb{bs}nc{bs}td'")),
            ("a\u{0}\u{7f}\u{80}b", format!("'a{bs}x00{bs}x7f{bs}x80b'")),
            ("a\u{200b}b", format!("'a{bs}u200bb'")),
            ("a\u{e0001}b", format!("'a{bs}U000e0001b'")),
            ("caf\u{e9}", "'caf\u{e9}'".to_owned()),
        ];
        for (input, want) in cases {
            assert_eq!(py_repr(OsStr::new(input)), want, "{input:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn repr_escapes_undecodable_bytes_as_surrogates() {
        use std::os::unix::ffi::OsStrExt;
        let got = py_repr(std::ffi::OsStr::from_bytes(b"a\xffb"));
        assert_eq!(got, format!("'a{}udcffb'", '\\'));
    }

    /// Cases recorded from radon 6.0.1 with `RADONFILESENCODING` set to these
    /// bytes.
    #[cfg(unix)]
    #[test]
    fn encode_errors_match_python() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        use super::utf8_encode_error;
        let single = |byte: &str, at: usize| {
            format!(
                "'utf-8' codec can't encode character '{}udc{byte}' in position {at}: \
                 surrogates not allowed",
                '\\'
            )
        };
        let run = |from: usize, to: usize| {
            format!(
                "'utf-8' codec can't encode characters in position {from}-{to}: \
                 surrogates not allowed"
            )
        };
        // `\u{e9}\u{1f4a9}` then FF FE: two code points before the run.
        let mut emoji = "\u{e9}\u{1f4a9}".as_bytes().to_vec();
        emoji.extend_from_slice(b"\xff\xfe");
        let cases: [(&[u8], String); 7] = [
            (b"\xff", single("ff", 0)),
            (b"\xff\xfe", run(0, 1)),
            (b"ut\xffx\xfe", single("ff", 2)),
            (b"a\xffb\xfe\xfdc", single("ff", 1)),
            (&emoji, run(2, 3)),
            (b"\xed\xa0\x80", run(0, 2)),
            (b"\xff\xc3\xa9\xfe\xfd", single("ff", 0)),
        ];
        for (bytes, want) in cases {
            let got = utf8_encode_error(OsStr::from_bytes(bytes));
            assert_eq!(got.as_deref(), Some(want.as_str()), "{bytes:x?}");
        }
        assert_eq!(utf8_encode_error(OsStr::new("utf-8")), None);
    }
}
