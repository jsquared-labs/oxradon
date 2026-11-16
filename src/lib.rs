//! A fast, drop-in Rust port of [radon](https://github.com/rubik/radon), the
//! Python code metrics tool.
//!
//! The results are those of radon 6.0.1 running on CPython 3.14, quirks
//! included, so thresholds and dashboards built on radon keep their meaning.
//! Most people use the `oxradon` command; this library is the analysis behind
//! it, one module per radon command:
//!
//! - [`cc`]: cyclomatic complexity (`radon cc`);
//! - [`hal`]: Halstead metrics (`radon hal`);
//! - [`raw`]: line counts (`radon raw`);
//! - [`mi`]: the Maintainability Index (`radon mi`).
//!
//! Each has an `analyze` function that takes Python source as text (`cc` and
//! `mi` also take their options):
//!
//! ```
//! use oxradon::{cc, raw};
//!
//! let source = "def pick(x):\n    return x or 1\n";
//!
//! let blocks = cc::analyze(source, &cc::Options::default())?;
//! assert_eq!(blocks[0].name(), "pick");
//! assert_eq!(blocks[0].complexity(), 2);
//! assert_eq!(blocks[0].rank(), cc::Rank::A);
//!
//! let lines = raw::analyze(source)?;
//! assert_eq!((lines.loc, lines.lloc), (2, 2));
//! # Ok::<(), oxradon::Error>(())
//! ```
//!
//! # What "the same as radon" covers
//!
//! `cc`, `hal` and `mi` accept source exactly when CPython 3.14's `ast.parse`
//! accepts it (COMPATIBILITY.md lists the few known exceptions); `raw` only
//! tokenizes, as radon's does. The numbers are radon's, down to the last
//! digit of each float. An [`Error`] holds the message for a source that
//! could not be analyzed; for syntax errors the wording is Ruff's, not
//! CPython's.
//!
//! # Deeply nested code
//!
//! Parsing and analysis recurse once per nesting level of the code. Ordinary
//! code needs very little stack, but a pathological input can exhaust it,
//! which aborts the process: an expression nested many thousands of levels
//! deep without brackets (brackets stop at 200 levels, as in CPython), such
//! as a very long chain of operators. The command line runs the analysis on
//! threads with 64 MiB stacks, which is enough for roughly 100,000 levels; do
//! the same when the input is not trusted.

#![warn(missing_docs)]

pub mod cc;
mod cc_report;
#[doc(hidden)]
pub mod cli;
mod colorama;
mod config;
mod discover;
mod error;
mod fnmatch;
pub mod hal;
mod hal_report;
mod limits;
pub mod mi;
mod mi_report;
mod pyfmt;
pub mod raw;
mod raw_report;
mod rawtok;
mod report;
mod source;
mod syntax;
mod tomllib;

pub use error::Error;
