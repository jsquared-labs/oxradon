//! Which files radon analyzes: a port of `radon.cli.tools.iter_filenames`.
//!
//! Output order matches radon's `os.walk`: a directory's files in `readdir`
//! order, then each subdirectory in turn. Subdirectories are walked in
//! parallel and their results concatenated in that order.
//!
//! Paths stay as OS strings for every filesystem operation, so names that are
//! not valid UTF-8 are opened as they are. Python sees such names through
//! `surrogateescape`; [`path_text`] is the closest Rust string, used for
//! pattern matching and display.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirEntry, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::fnmatch::PatternSet;
use crate::pyfmt::{os_string_from_bytes, path_text};
use crate::source::{STDIN, is_stdin};

/// radon's `--exclude` and `--ignore` filters.
pub(crate) struct Filters {
    /// Matched against each file's normalized path.
    exclude: PatternSet,
    /// Matched against directory names; hidden directories are always ignored.
    ignore: PatternSet,
}

impl Filters {
    /// Build filters from radon's comma-separated option values.
    ///
    /// # Errors
    /// Fails only if a pattern exceeds the regex engine's size limits.
    pub(crate) fn new(exclude: Option<&str>, ignore: Option<&str>) -> Result<Self, regex::Error> {
        let exclude = exclude.filter(|e| !e.is_empty()).map(|e| e.split(','));
        let ignore = ignore.filter(|i| !i.is_empty()).map(|i| i.split(','));
        Ok(Self {
            exclude: PatternSet::new(exclude.into_iter().flatten())?,
            ignore: PatternSet::new([".*"].into_iter().chain(ignore.into_iter().flatten()))?,
        })
    }

    fn excludes(&self, path: &OsStr) -> bool {
        self.exclude.is_match(&path_text(path))
    }

    fn ignores(&self, dir_name: &OsStr) -> bool {
        self.ignore.is_match(&path_text(dir_name))
    }
}

/// A file found by the walk. Files without a `.py` suffix count only if
/// their first line is a Python shebang, which is checked later, in parallel.
enum Candidate {
    Python(PathBuf),
    MaybeScript(PathBuf),
}

impl Candidate {
    fn confirm(self) -> Option<PathBuf> {
        match self {
            Self::Python(path) => Some(path),
            Self::MaybeScript(path) => has_python_shebang(&path).then_some(path),
        }
    }
}

/// The names radon's `iter_filenames` yields for `paths`, in its order.
///
/// Names, not files: radon later reads any name that is exactly `-` from
/// stdin ([`is_stdin`]). Besides the all-`-` case, that happens for an
/// explicit `-` when a file `-` exists, and for a walked `-` whose path
/// normalizes to `-` (a walk of `.`); `./-` or `sub/-` stay files.
pub(crate) fn python_files(paths: &[OsString], filters: &Filters) -> Vec<PathBuf> {
    if !paths.is_empty() && paths.iter().all(|p| p == STDIN) {
        return vec![PathBuf::from(STDIN)];
    }
    let mut candidates = Vec::new();
    for path in paths {
        let as_path = Path::new(path);
        if as_path.is_file() && is_python_file(as_path) && !filters.excludes(path) {
            candidates.push(Candidate::Python(as_path.to_path_buf()));
        } else {
            candidates.extend(walk(as_path, filters));
        }
    }
    candidates
        .into_par_iter()
        .filter_map(Candidate::confirm)
        .collect()
}

/// `os.walk(top)` (without following symlinks) with radon's filters applied.
/// `DirEntry::path` joins exactly like `os.path.join`.
fn walk(top: &Path, filters: &Filters) -> Vec<Candidate> {
    let Ok(entries) = fs::read_dir(top) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if is_dir(&entry) {
            // os.walk lists symlinked directories but does not descend into
            // them.
            let is_link = entry.file_type().is_ok_and(|t| t.is_symlink());
            if !is_link && !filters.ignores(&name) {
                subdirs.push(entry.path());
            }
            continue;
        }
        if name.as_encoded_bytes().starts_with(b".") {
            continue;
        }
        let path = normpath(entry.path().as_os_str());
        if filters.excludes(&path) {
            continue;
        }
        let path = PathBuf::from(path);
        // `_is_python_file` sees the normalized name, so a walked `-` needs no
        // shebang.
        found.push(if has_py_suffix(path.as_os_str()) || is_stdin(&path) {
            Candidate::Python(path)
        } else {
            Candidate::MaybeScript(path)
        });
    }
    let nested: Vec<Vec<Candidate>> = subdirs.par_iter().map(|dir| walk(dir, filters)).collect();
    found.extend(nested.into_iter().flatten());
    found
}

/// Like `DirEntry.is_dir()` in Python: follows symlinks, but only pays for a
/// `stat` when the entry is one.
fn is_dir(entry: &DirEntry) -> bool {
    match entry.file_type() {
        Ok(t) if t.is_symlink() => fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()),
        Ok(t) => t.is_dir(),
        Err(_) => false,
    }
}

/// `radon.cli.tools._is_python_file`.
fn is_python_file(path: &Path) -> bool {
    is_stdin(path) || has_py_suffix(path.as_os_str()) || has_python_shebang(path)
}

/// radon checks `filename.endswith('.py')`: case-sensitive, so `X.PY` is not
/// Python.
fn has_py_suffix(path: &OsStr) -> bool {
    path.as_encoded_bytes().ends_with(b".py")
}

/// radon's `#!` check: `open(filename).readline()`, then
/// `line.startswith('#!') and 'python' in line`; any exception means no.
fn has_python_shebang(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    first_line_is_python_shebang(file).unwrap_or(false)
}

/// The size of each read `TextIOWrapper.readline` makes (`read_chunk(0)`).
const CHUNK: usize = 8192;

/// CPython's `TextIOWrapper.readline` over an `IncrementalNewlineDecoder`
/// wrapping the UTF-8 decoder, reduced to what the `#!` check needs.
///
/// - The file is read in 8 KiB chunks, and each chunk is decoded *whole*: an
///   invalid byte anywhere in it raises, even after the newline.
/// - An incomplete UTF-8 sequence at the end of a chunk waits for the next one;
///   at end of file (`final=True`) it raises.
/// - A `\r` at the very end of the decoded text is held back until more text
///   (or end of file) shows whether it starts `\r\n`; `\r`, `\n` and `\r\n` all
///   end the line.
///
/// Returns `None` where Python raises.
fn first_line_is_python_shebang(mut file: impl Read) -> Option<bool> {
    let mut chunk = vec![0; CHUNK];
    let mut held = Vec::new(); // an incomplete UTF-8 sequence carried over
    let mut held_cr = false; // the line ended with a `\r` that is held back
    let mut line = LineScan::default();
    loop {
        let n = read_full(&mut file, &mut chunk).ok()?;
        if n == 0 {
            // The final decode raises on a carried-over partial sequence.
            return held.is_empty().then(|| line.is_python_shebang());
        }
        if line.head.is_empty() && !chunk[..n].starts_with(b"#!") {
            return Some(false); // the line cannot start with `#!`
        }
        let mut data = std::mem::take(&mut held);
        data.extend_from_slice(&chunk[..n]);
        let decoded = match std::str::from_utf8(&data) {
            Ok(_) => data.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => return None,
        };
        held = data.split_off(decoded);
        let text = data;
        if held_cr {
            if text.is_empty() {
                continue; // still waiting for text after the `\r`
            }
            return Some(line.is_python_shebang());
        }
        match text.iter().position(|&b| b == b'\n' || b == b'\r') {
            Some(end) => {
                line.push(&text[..end]);
                if text[end] == b'\r' && end + 1 == text.len() {
                    held_cr = true;
                } else {
                    return Some(line.is_python_shebang());
                }
            }
            None => line.push(&text),
        }
    }
}

/// Fill `buf` from `reader` like `BufferedReader.read1` does for a regular
/// file: a short count only at end of file.
fn read_full(reader: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// Tracks the first line without keeping it: its first two bytes, and
/// whether `python` occurs in it (also across chunk boundaries).
#[derive(Default)]
struct LineScan {
    head: Vec<u8>,
    tail: Vec<u8>,
    found_python: bool,
}

impl LineScan {
    fn push(&mut self, bytes: &[u8]) {
        let missing = 2usize.saturating_sub(self.head.len());
        self.head.extend(bytes.iter().take(missing));
        let mut window = std::mem::take(&mut self.tail);
        window.extend_from_slice(bytes);
        self.found_python |= window.windows(6).any(|w| w == b"python");
        let keep = window.len().min(5);
        self.tail = window.split_off(window.len() - keep);
    }

    fn is_python_shebang(&self) -> bool {
        self.head == b"#!" && self.found_python
    }
}

/// `posixpath.normpath`, on the path's bytes.
fn normpath(path: &OsStr) -> OsString {
    let bytes = path.as_encoded_bytes();
    if bytes.is_empty() {
        return ".".into();
    }
    // POSIX keeps exactly two leading slashes but collapses three or more.
    let root: &[u8] = if bytes.starts_with(b"//") && !bytes.starts_with(b"///") {
        b"//"
    } else if bytes.starts_with(b"/") {
        b"/"
    } else {
        b""
    };
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in bytes.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." if parts.last().is_some_and(|&p| p != b"..") => {
                parts.pop();
            }
            // `..` at the root goes nowhere; relative paths keep it.
            b".." if !root.is_empty() => {}
            _ => parts.push(part),
        }
    }
    let mut out = root.to_vec();
    out.extend(parts.join(&b'/'));
    if out.is_empty() {
        return ".".into();
    }
    os_string_from_bytes(out)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use serde_json::Value;

    use super::{first_line_is_python_shebang, normpath};

    fn norm(path: &str) -> String {
        normpath(OsStr::new(path)).into_string().unwrap()
    }

    #[test]
    fn normpath_matches_posixpath() {
        assert_eq!(norm("./a/b.py"), "a/b.py");
        assert_eq!(norm("src/"), "src");
        assert_eq!(norm("a//b/../c"), "a/c");
        assert_eq!(norm("../x"), "../x");
        assert_eq!(norm("../../x"), "../../x");
        assert_eq!(norm("a/../../x"), "../x");
        assert_eq!(norm("/../x"), "/x");
        assert_eq!(norm("//x"), "//x");
        assert_eq!(norm("///x"), "/x");
        assert_eq!(norm(""), ".");
        assert_eq!(norm("a/.."), ".");
    }

    fn shebang(bytes: &[u8]) -> bool {
        first_line_is_python_shebang(bytes).unwrap_or(false)
    }

    #[test]
    fn shebang_basics() {
        assert!(shebang(b"#!/usr/bin/env python3\nx = 1\n"));
        assert!(!shebang(b"#!/bin/sh\npython\n"));
        assert!(shebang(b"#!python"), "no newline at all");
        assert!(
            !shebang(b"#!python\r\xc3"),
            "held CR, then a truncated sequence at EOF"
        );
        assert!(
            shebang(b"#!python\n\xc3"),
            "the line ends before the truncated sequence"
        );
        assert!(
            !shebang(b"#!python\n\xff"),
            "invalid bytes in the same chunk raise"
        );
    }

    /// Byte strings built from the cases `scripts/update_golden.py` recorded by
    /// running radon's own `_is_python_file` on real files.
    #[test]
    fn shebang_matches_radon() {
        let golden: Value =
            serde_json::from_str(include_str!("../tests/golden/shebang.json")).unwrap();
        let cases = golden.as_array().unwrap();
        assert!(cases.len() > 100);
        // A string part is hex bytes; a number part is a run of `x` bytes.
        let bytes_of = |parts: &Value| -> Vec<u8> {
            let mut bytes = Vec::new();
            for part in parts.as_array().unwrap() {
                if let Some(hex) = part.as_str() {
                    let pairs = (0..hex.len()).step_by(2);
                    bytes.extend(pairs.map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()));
                } else {
                    let run = usize::try_from(part.as_u64().unwrap()).unwrap();
                    bytes.extend(std::iter::repeat_n(b'x', run));
                }
            }
            bytes
        };
        let mut failures = Vec::new();
        for case in cases {
            let bytes = bytes_of(&case["parts"]);
            let expected = case["radon"].as_bool().unwrap();
            if shebang(&bytes) != expected {
                failures.push(format!("{}: radon={expected}", case["label"]));
            }
        }
        assert!(
            failures.is_empty(),
            "{} differ:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}
