//! The error the analysis functions return.

use std::fmt;

/// Why a source could not be analyzed.
///
/// It carries the message the command line prints for that source:
///
/// - for code CPython 3.14 does not accept, a syntax error laid out as
///   CPython's `ast.parse` lays it out, `Expected an expression (<unknown>,
///   line 3)`. The wording is Ruff's, so it differs from radon's, and it can
///   name a different line;
/// - from [`raw`](crate::raw), which only tokenizes, radon's own `SyntaxError
///   at line: 7`.
///
/// [`Display`] prints the message and nothing else.
///
/// [`Display`]: fmt::Display
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The message, without a prefix.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The message alone, for the reports.
    pub(crate) fn into_message(self) -> String {
        self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}
