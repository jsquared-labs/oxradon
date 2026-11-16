//! The Maintainability Index, ported from radon's `mi_visit`, `mi_parameters`,
//! `mi_compute` and `mi_rank` (`radon/metrics.py`).
//!
//! It combines three metrics oxradon already computes: the Halstead volume
//! ([`crate::hal`]), the module's total cyclomatic complexity ([`crate::cc`])
//! and the logical line and comment counts ([`crate::raw`]). radon's quirks are
//! kept: the logical line count (`lloc`) goes where the formula says SLOC, and
//! the comment percentage is taken over `sloc`.

use std::fmt;

use crate::error::Error;
use crate::hal::{self, PyNum};
use crate::{cc, raw};

/// A maintainability grade: A (above 19), B (above 9) or C.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rank {
    /// Above 19: easy to maintain.
    A,
    /// Above 9, up to 19.
    B,
    /// 9 or below: hard to maintain.
    C,
}

impl Rank {
    /// `mi_rank`: `chr(65 + (9 - score >= 0) + (19 - score >= 0))`.
    pub fn of(score: f64) -> Self {
        match u8::from(9.0 - score >= 0.0) + u8::from(19.0 - score >= 0.0) {
            0 => Self::A,
            1 => Self::B,
            _ => Self::C,
        }
    }

    /// The grade's letter.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
        }
    }
}

impl fmt::Display for Rank {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How comments are counted: radon's `--multi`. The default is radon's.
///
/// ```
/// use oxradon::mi::Options;
///
/// assert!(Options::default().count_multi);
/// let hash_comments_only = Options { count_multi: false };
/// # let _ = hash_comments_only;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Count multi-line strings (docstrings) as comments, as radon does
    /// unless given `-m`.
    pub count_multi: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { count_multi: true }
    }
}

/// One module's result, as radon's `MIHarvester.gobble` returns it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mi {
    /// The index, from 0 to 100.
    pub mi: f64,
    /// The grade for [`Mi::mi`].
    pub rank: Rank,
}

/// The four inputs of [`compute`], as `mi_parameters` returns them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Parameters {
    /// Halstead `total.volume`: the int `0`, or a float.
    pub volume: PyNum,
    /// `ComplexityVisitor.total_complexity`.
    pub complexity: u64,
    /// `raw.lloc`, which radon passes as the formula's SLOC.
    pub lloc: u64,
    /// Comment lines as a percentage of `raw.sloc` lines, or 0 when there
    /// are none: `raw.comments`, plus `raw.multi` when multi-line strings
    /// count.
    pub comments: f64,
}

/// `mi_parameters(code, count_multi)`: the inputs of the index.
///
/// radon parses the code first, then runs `raw.analyze`, so a syntax error
/// is reported before a `raw` error. The visitors that follow cannot fail.
///
/// Parsing and visiting recurse once per nesting level, so pathologically
/// deep input needs a large stack; the CLI runs this on 64 MiB threads.
///
/// # Errors
/// The syntax error, or `raw`'s `SyntaxError at line: N`.
#[allow(clippy::cast_precision_loss)] // Python's int-to-float conversion
pub fn parameters(text: &str, options: &Options) -> Result<Parameters, Error> {
    // `ast.parse(code)`, with the error text `cc` gives for this file.
    let complexity = cc::total_complexity(text)?;
    let counts = raw::analyze(text)?;
    let volume = hal::analyze(text)?.total.volume;
    let comment_lines = counts.comments + if options.count_multi { counts.multi } else { 0 };
    // radon's `else` value is the int 0, which `math.radians` treats as 0.0.
    let comments = if counts.sloc == 0 {
        0.0
    } else {
        comment_lines as f64 / counts.sloc as f64 * 100.0
    };
    Ok(Parameters {
        volume,
        complexity,
        lloc: counts.lloc,
        comments,
    })
}

/// `math.radians`'s factor: `Py_MATH_PI / 180.0`, rounded once.
const DEG_TO_RAD: f64 = std::f64::consts::PI / 180.0;

/// `mi_compute(halstead_volume, complexity, sloc, comments)`, operation for
/// operation, with radon's `lloc` as `sloc`. The libm calls (`log`, `sqrt`,
/// `sin`) are the ones CPython's `math` module makes.
#[must_use]
#[allow(clippy::cast_precision_loss)] // Python's int-to-float conversion
pub fn compute(volume: PyNum, complexity: u64, sloc: u64, comments: f64) -> f64 {
    let volume = volume.as_f64();
    if volume <= 0.0 || sloc == 0 {
        return 100.0;
    }
    let sloc_scale = (sloc as f64).ln();
    let volume_scale = volume.ln();
    let comments_scale = (2.46 * (comments * DEG_TO_RAD)).sqrt();
    // Left to right, as Python evaluates it (Rust never fuses these).
    let nn_mi = 171.0 - 5.2 * volume_scale - 0.23 * complexity as f64 - 16.2 * sloc_scale
        + 50.0 * comments_scale.sin();
    let scaled = nn_mi * 100.0 / 171.0;
    // `min(max(0.0, scaled), 100.0)`: Python's `max` keeps its first argument
    // unless a later one is greater, and `min` unless a later one is less.
    let floored = if scaled > 0.0 { scaled } else { 0.0 };
    if 100.0 < floored { 100.0 } else { floored }
}

/// Compute radon's Maintainability Index for a module: `mi_visit(code,
/// multi)` and `mi_rank`.
///
/// Parsing and visiting recurse once per nesting level, so pathologically
/// deep input needs a large stack; the CLI runs this on 64 MiB threads.
///
/// ```
/// use oxradon::mi::{self, Rank};
///
/// let source = "def double(x):\n    return x * 2\n";
/// let result = mi::analyze(source, &mi::Options::default())?;
/// assert_eq!(result.rank, Rank::A);
/// assert!(result.mi > 19.0 && result.mi <= 100.0);
/// # Ok::<(), oxradon::Error>(())
/// ```
///
/// # Errors
/// As [`parameters`].
pub fn analyze(text: &str, options: &Options) -> Result<Mi, Error> {
    let p = parameters(text, options)?;
    let mi = compute(p.volume, p.complexity, p.lloc, p.comments);
    Ok(Mi {
        mi,
        rank: Rank::of(mi),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_boundaries() {
        let next_up = |x: f64| f64::from_bits(x.to_bits() + 1);
        assert_eq!(Rank::of(0.0), Rank::C);
        assert_eq!(Rank::of(9.0), Rank::C);
        assert_eq!(Rank::of(next_up(9.0)), Rank::B);
        assert_eq!(Rank::of(19.0), Rank::B);
        assert_eq!(Rank::of(next_up(19.0)), Rank::A);
        assert_eq!(Rank::of(100.0), Rank::A);
    }

    #[test]
    fn empty_and_trivial_modules_score_100() {
        assert_eq!(compute(PyNum::Int(0), 1, 0, 0.0), 100.0);
        assert_eq!(compute(PyNum::Float(0.0), 1, 3, 0.0), 100.0);
        assert_eq!(compute(PyNum::Float(10.0), 1, 0, 0.0), 100.0);
        let mi = analyze("", &Options::default()).expect("empty module");
        assert_eq!((mi.mi, mi.rank), (100.0, Rank::A));
    }

    #[test]
    fn syntax_errors_come_before_raw_errors() {
        // Unparseable, and also a raw error (an unterminated string).
        let text = "x = (\n'";
        assert!(raw::analyze(text).is_err());
        let err = analyze(text, &Options::default()).expect_err("syntax error");
        assert_eq!(err, cc::analyze(text, &cc::Options::default()).unwrap_err());
    }
}
