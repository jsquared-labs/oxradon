//! Reading Python files the way radon does, and mapping byte offsets to
//! CPython-style positions.

use std::fmt::Display;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use ruff_source_file::LineIndex;
use ruff_text_size::TextSize;

use crate::error::Error;
use crate::pyfmt::{py_repr, utf8_encode_error};

/// The codec radon opens files with: `RADONFILESENCODING`, else UTF-8.
///
/// On Python 3, radon calls `open(path, encoding=...)` directly, so PEP 263
/// coding cookies are ignored and a UTF-8 BOM is kept in the text (where
/// `ast.parse` rejects it). Python supports many more codecs than oxradon
/// implements; the others are treated as unknown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    Utf8,
    Utf8Sig,
    Latin1,
    Ascii,
    Cp1252,
    /// A name `codecs.lookup` does not know (or oxradon does not implement).
    Unknown(String),
    /// A non-UTF-8 value: `io.open` fails converting it, with this message.
    Unencodable(String),
}

/// Python's `encodings.aliases` entries for the codecs above.
const ALIASES: &[(&str, &[&str])] = &[
    (
        "utf_8",
        &["cp65001", "u8", "utf", "utf8", "utf8_ucs2", "utf8_ucs4"],
    ),
    ("utf_8_sig", &[]),
    (
        "latin_1",
        &[
            "8859",
            "cp819",
            "csisolatin1",
            "ibm819",
            "iso8859",
            "iso8859_1",
            "iso_8859_1",
            "iso_8859_1_1987",
            "iso_ir_100",
            "l1",
            "latin",
            "latin1",
        ],
    ),
    (
        "ascii",
        &[
            "646",
            "ansi_x3.4_1968",
            "ansi_x3.4_1986",
            "ansi_x3_4_1968",
            "cp367",
            "csascii",
            "ibm367",
            "iso646_us",
            "iso_646.irv_1991",
            "iso_ir_6",
            "us",
            "us_ascii",
        ],
    ),
    ("cp1252", &["1252", "windows_1252"]),
];

/// cp1252 bytes 0x80-0x9F; `None` marks the five bytes Python leaves undefined.
const CP1252_HIGH: [Option<char>; 32] = [
    Some('\u{20ac}'),
    None,
    Some('\u{201a}'),
    Some('\u{0192}'),
    Some('\u{201e}'),
    Some('\u{2026}'),
    Some('\u{2020}'),
    Some('\u{2021}'),
    Some('\u{02c6}'),
    Some('\u{2030}'),
    Some('\u{0160}'),
    Some('\u{2039}'),
    Some('\u{0152}'),
    None,
    Some('\u{017d}'),
    None,
    None,
    Some('\u{2018}'),
    Some('\u{2019}'),
    Some('\u{201c}'),
    Some('\u{201d}'),
    Some('\u{2022}'),
    Some('\u{2013}'),
    Some('\u{2014}'),
    Some('\u{02dc}'),
    Some('\u{2122}'),
    Some('\u{0161}'),
    Some('\u{203a}'),
    Some('\u{0153}'),
    None,
    Some('\u{017e}'),
    Some('\u{0178}'),
];

impl Encoding {
    /// Read `RADONFILESENCODING` once, as raw bytes: Python decodes it with
    /// `surrogateescape`, and `io.open` then fails to encode the surrogates.
    pub(crate) fn from_env() -> Self {
        let Some(value) = std::env::var_os("RADONFILESENCODING") else {
            return Self::Utf8;
        };
        match value.to_str() {
            Some(name) => Self::from_name(name),
            // Not UTF-8, so there is an invalid byte and always a message.
            None => Self::Unencodable(utf8_encode_error(&value).unwrap_or_default()),
        }
    }

    /// The codec `open(path, encoding=name)` would use.
    ///
    /// `TextIOWrapper` maps exactly `locale` (case-sensitive) to the locale
    /// encoding, assumed to be UTF-8. Anything else goes through
    /// `codecs.lookup`: [`normalize_encoding`], then
    /// `encodings.search_function`, which tries the alias table (also with `.`
    /// replaced by `_`) and then the name itself as a module, skipping
    /// candidates that are empty or contain `.`.
    pub(crate) fn from_name(name: &str) -> Self {
        if name == "locale" {
            return Self::Utf8;
        }
        let normalized = normalize_encoding(name);
        let alias = |key: &str| {
            ALIASES
                .iter()
                .find(|(_, aliases)| aliases.contains(&key))
                .map(|(codec, _)| *codec)
        };
        let aliased = alias(&normalized).or_else(|| alias(&normalized.replace('.', "_")));
        let module = aliased
            .into_iter()
            .chain([normalized.as_str()])
            .find(|m| !m.is_empty() && !m.contains('.') && Self::module(m).is_some());
        module
            .and_then(Self::module)
            .unwrap_or_else(|| Self::Unknown(name.to_owned()))
    }

    /// The `encodings.<module>` oxradon implements.
    fn module(name: &str) -> Option<Self> {
        Some(match name {
            "utf_8" => Self::Utf8,
            "utf_8_sig" => Self::Utf8Sig,
            "latin_1" => Self::Latin1,
            "ascii" => Self::Ascii,
            "cp1252" => Self::Cp1252,
            _ => return None,
        })
    }

    /// Decode file contents into text, with Python-style error messages.
    ///
    /// # Errors
    /// Returns the message radon would report for undecodable input.
    pub(crate) fn decode(&self, mut bytes: Vec<u8>) -> Result<String, String> {
        match self {
            Self::Utf8 => utf8(bytes),
            Self::Utf8Sig => {
                // `utf_8_sig.IncrementalDecoder` holds back fewer than three
                // bytes that could start a BOM, even when final, and
                // `TextIOWrapper.read()` never checks for leftovers: such a
                // file reads as empty.
                if bytes.len() < 3 && b"\xef\xbb\xbf".starts_with(&bytes) {
                    return Ok(String::new());
                }
                if bytes.starts_with(b"\xef\xbb\xbf") {
                    bytes.drain(..3);
                }
                utf8(bytes)
            }
            Self::Latin1 => Ok(bytes.iter().map(|&b| char::from(b)).collect()),
            Self::Ascii => match bytes.iter().position(|b| !b.is_ascii()) {
                Some(pos) => Err(format!(
                    "'ascii' codec can't decode byte {:#04x} in position {pos}: \
                     ordinal not in range(128)",
                    bytes[pos]
                )),
                None => utf8(bytes),
            },
            Self::Cp1252 => bytes
                .iter()
                .enumerate()
                .map(|(pos, &b)| match b {
                    0x80..=0x9f => CP1252_HIGH[usize::from(b - 0x80)].ok_or_else(|| {
                        format!(
                            "'charmap' codec can't decode byte {b:#04x} in position {pos}: \
                             character maps to <undefined>"
                        )
                    }),
                    _ => Ok(char::from(b)),
                })
                .collect(),
            Self::Unknown(name) => Err(format!("unknown encoding: {name}")),
            Self::Unencodable(message) => Err(message.clone()),
        }
    }
}

/// CPython's `_Py_normalize_encoding`, which `codecs.lookup` applies to the
/// name's UTF-8 bytes: ASCII letters, digits and `.` are kept (lower-cased);
/// every other byte, including each byte of a non-ASCII character, separates
/// them, and a run of separators becomes one `_` between kept bytes.
/// (`encodings.normalize_encoding`, applied next, leaves this result as is.)
fn normalize_encoding(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut separated = false;
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b == b'.' {
            if separated && !out.is_empty() {
                out.push('_');
            }
            out.push(char::from(b.to_ascii_lowercase()));
            separated = false;
        } else {
            separated = true;
        }
    }
    out
}

/// Decode UTF-8 with `UnicodeDecodeError`'s message for the first bad sequence.
/// Its span follows CPython's `unicode_decode_utf8`: an invalid start byte is
/// one byte; an invalid continuation covers the valid prefix before it (Rust's
/// `error_len`, both being Unicode's maximal subpart); a sequence cut short
/// runs to the end of the input.
fn utf8(bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|e| {
        let err = e.utf8_error();
        let start = err.valid_up_to();
        let byte = e.as_bytes()[start];
        let (end, reason) = match err.error_len() {
            None => (e.as_bytes().len(), "unexpected end of data"),
            Some(_) if (0x80..0xc2).contains(&byte) || byte > 0xf4 => {
                (start + 1, "invalid start byte")
            }
            Some(len) => (start + len, "invalid continuation byte"),
        };
        decode_error("utf-8", e.as_bytes(), start, end, reason)
    })
}

/// `str(UnicodeDecodeError)`: one byte is named, a longer span is given as a
/// range.
fn decode_error(codec: &str, bytes: &[u8], start: usize, end: usize, reason: &str) -> String {
    if end == start + 1 {
        format!(
            "'{codec}' codec can't decode byte {:#04x} in position {start}: {reason}",
            bytes[start]
        )
    } else {
        format!(
            "'{codec}' codec can't decode bytes in position {start}-{}: {reason}",
            end - 1
        )
    }
}

/// An input opened the way radon's `_open` opens it.
pub(crate) enum Opened<'e> {
    /// radon reads `sys.stdin` (UTF-8, BOM kept), so `RADONFILESENCODING`
    /// applies to files only.
    Stdin,
    File(File, &'e Encoding),
}

/// The name that radon's `_open` reads from stdin.
pub(crate) const STDIN: &str = "-";

/// radon's `_open` rule: the name `-`, however it was found, means stdin.
pub(crate) fn is_stdin(name: &Path) -> bool {
    name.as_os_str() == STDIN
}

/// Open an input like radon's `_open`.
///
/// # Errors
/// radon opens files outside its per-file error handling, so a failure here
/// aborts its whole run. The message is `str()` of the Python exception, in
/// the order `io.open` raises them: the encoding argument's conversion (a
/// non-UTF-8 `RADONFILESENCODING`), then the OS open (`[Errno N] <reason>:
/// '<path>'`), then the codec lookup (`unknown encoding: <name>`).
pub(crate) fn open<'e>(name: &Path, encoding: &'e Encoding) -> Result<Opened<'e>, String> {
    if is_stdin(name) {
        return Ok(Opened::Stdin);
    }
    if let Encoding::Unencodable(message) = encoding {
        return Err(message.clone());
    }
    let file = File::open(name).map_err(|e| os_error(&e, name))?;
    if let Encoding::Unknown(codec) = encoding {
        return Err(format!("unknown encoding: {codec}"));
    }
    Ok(Opened::File(file, encoding))
}

impl Opened<'_> {
    /// Read and decode the whole input. Failures here are per-file in radon.
    ///
    /// # Errors
    /// Returns an I/O or decoding message.
    pub(crate) fn read_text(self) -> Result<String, String> {
        let mut bytes = Vec::new();
        match self {
            Self::Stdin => {
                io::stdin()
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                Encoding::Utf8.decode(bytes)
            }
            Self::File(mut file, encoding) => {
                file.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                encoding.decode(bytes)
            }
        }
    }
}

/// `str(OSError)` as Python words it: `[Errno 13] Permission denied: '/x.py'`.
/// A path containing NUL fails before any system call, as Python's `open`
/// raises `ValueError` for it (only a configured `output_file` can hold one).
pub(crate) fn os_error(error: &io::Error, path: &Path) -> String {
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        return "embedded null byte".into();
    }
    match error.raw_os_error() {
        Some(_) => format!("{}: {}", errno_error(error), py_repr(path.as_os_str())),
        None => error.to_string(),
    }
}

/// `str(OSError)` for an error with no file name, such as a failed write:
/// `[Errno 27] File too large`.
pub(crate) fn errno_error(error: &io::Error) -> String {
    let Some(errno) = error.raw_os_error() else {
        return error.to_string();
    };
    let reason = io::Error::from_raw_os_error(errno).to_string();
    let reason = reason
        .strip_suffix(&format!(" (os error {errno})"))
        .unwrap_or(&reason);
    format!("[Errno {errno}] {reason}")
}

/// A syntax error laid out like `str(SyntaxError)` from `ast.parse`.
pub(crate) fn syntax_error(message: impl Display, line: u32) -> Error {
    Error::new(format!("{message} (<unknown>, line {line})"))
}

/// Narrow a count or position to `u32`. Ruff's offsets are `u32`, so values
/// derived from a parsed file always fit; saturating is only a formality.
pub(crate) fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Source text with a line index, for CPython-style positions.
pub(crate) struct Source<'a> {
    text: &'a str,
    index: LineIndex,
}

impl<'a> Source<'a> {
    pub(crate) fn new(text: &'a str) -> Self {
        Self {
            text,
            index: LineIndex::from_source_text(text),
        }
    }

    /// 1-based line number, like CPython's `lineno`.
    pub(crate) fn line(&self, offset: TextSize) -> u32 {
        to_u32(self.index.line_index(offset).get())
    }

    /// Column in UTF-8 bytes, like CPython's `col_offset`.
    pub(crate) fn col(&self, offset: TextSize) -> u32 {
        let line = self.index.line_index(offset);
        (offset - self.index.line_start(line, self.text)).to_u32()
    }
}

#[cfg(test)]
mod tests {
    use super::{Encoding, Source};
    use ruff_text_size::TextSize;
    use serde_json::Value;

    #[test]
    fn utf8_sig_strips_the_bom_and_utf8_keeps_it() {
        let bytes = b"\xef\xbb\xbfx = 1\n".to_vec();
        assert_eq!(Encoding::Utf8Sig.decode(bytes.clone()).unwrap(), "x = 1\n");
        assert!(
            Encoding::Utf8
                .decode(bytes)
                .unwrap()
                .starts_with('\u{feff}')
        );
    }

    #[test]
    fn coding_cookies_are_ignored() {
        let latin1 = b"# -*- coding: latin-1 -*-\ns = '\xe9'\n".to_vec();
        let err = Encoding::Utf8.decode(latin1.clone()).unwrap_err();
        assert!(err.contains("invalid continuation"));
        assert!(Encoding::from_name("Latin_1").decode(latin1).is_ok());
    }

    /// Spellings and cp1252 bytes recorded from Python by
    /// `scripts/update_golden.py`.
    #[test]
    fn codecs_match_python() {
        let golden: Value =
            serde_json::from_str(include_str!("../tests/golden/encodings.json")).unwrap();
        let supported = ["utf-8", "utf-8-sig", "iso8859-1", "ascii", "cp1252"];
        for case in golden["lookups"].as_array().unwrap() {
            let (name, python) = (case[0].as_str().unwrap(), case[1].as_str());
            let ours = match Encoding::from_name(name) {
                Encoding::Utf8 => Some("utf-8"),
                Encoding::Utf8Sig => Some("utf-8-sig"),
                Encoding::Latin1 => Some("iso8859-1"),
                Encoding::Ascii => Some("ascii"),
                Encoding::Cp1252 => Some("cp1252"),
                Encoding::Unknown(_) | Encoding::Unencodable(_) => None,
            };
            let expected = python.filter(|p| supported.contains(p));
            assert_eq!(ours, expected, "lookup of {name:?}");
        }
        for (i, python) in golden["cp1252"].as_array().unwrap().iter().enumerate() {
            let byte = u8::try_from(i + 0x80).unwrap();
            let ours = Encoding::Cp1252.decode(vec![byte]).ok();
            assert_eq!(ours.as_deref(), python.as_str(), "cp1252 byte {byte:#04x}");
        }
    }

    /// `open(path, encoding=...).read()`'s messages, recorded by
    /// `scripts/update_golden_raw.py`.
    #[test]
    fn decode_errors_match_python() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/decode-errors.json"
        );
        let text = std::fs::read_to_string(path).expect("decode-errors.json");
        let cases: Vec<(String, String, Option<String>)> =
            serde_json::from_str(&text).expect("JSON");
        for (codec, hex, expected) in &cases {
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
                .collect();
            let got = Encoding::from_name(codec).decode(bytes).err();
            assert_eq!(&got, expected, "{codec} {hex}");
        }
    }

    #[test]
    fn columns_are_utf8_bytes() {
        // `\u{e9}` is two bytes in UTF-8.
        let src = Source::new("\u{e9} = 1\nab = 2\n");
        let b = TextSize::from(8); // `b` on line 2
        assert_eq!((src.line(b), src.col(b)), (2, 1));
        assert_eq!(src.col(TextSize::from(3)), 3);
    }
}
