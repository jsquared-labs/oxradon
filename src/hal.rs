//! Halstead metrics (`radon hal`): radon's `HalsteadVisitor` and
//! `halstead_visitor_report`, over Ruff's AST.
//!
//! Operators are counted at `BinOp`, `UnaryOp`, `BoolOp`, `AugAssign` and
//! `Compare` only, by AST class. Operands are the direct children of those
//! nodes, and the distinct ones form a Python `set` of `(context, value)`
//! pairs, compared with Python's equality (`1 == 1.0 == True`, a name equals
//! a string with the same text, and other nodes compare by identity).

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;

use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{
    self as ast, BoolOp, CmpOp, Expr, Number, Operator, Stmt, StringFlags, StringLiteralValue,
    UnaryOp,
};
use ruff_text_size::{Ranged, TextRange};

use crate::error::Error;
use crate::limits::MAX_STR_DIGITS;
use crate::pyfmt::float_repr;
use crate::source::Source;

/// The report for a file: the whole module, and each function radon lists.
#[derive(Clone, Debug, PartialEq)]
pub struct Halstead {
    /// The whole module, functions included.
    pub total: Report,
    /// Every `def` reached without passing through another `def` (methods
    /// included), in source order, by name. Nested functions are counted in
    /// their enclosing function instead.
    pub functions: Vec<(String, Report)>,
}

/// A number as radon prints it. Python keeps some metrics as the `int` 0 in
/// degenerate cases (a module with no operands has volume `0`, not `0.0`),
/// and radon's output shows the difference, so the type is kept here.
/// [`PyNum::as_f64`] gives the value when the distinction does not matter,
/// and `Display` prints it as Python would.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PyNum {
    /// A Python `int`.
    Int(u64),
    /// A Python `float`.
    Float(f64),
}

impl PyNum {
    /// The value as a float, as Python converts an `int`.
    #[allow(clippy::cast_precision_loss)] // Python's int-to-float conversion
    pub fn as_f64(self) -> f64 {
        match self {
            Self::Int(i) => i as f64,
            Self::Float(f) => f,
        }
    }

    /// Python's `*`: two ints stay an int, anything else is a float.
    fn mul(self, other: Self) -> Self {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => Self::Int(a * b),
            _ => Self::Float(self.as_f64() * other.as_f64()),
        }
    }
}

impl fmt::Display for PyNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(i) => write!(f, "{i}"),
            Self::Float(x) => f.write_str(&float_repr(*x)),
        }
    }
}

/// radon's `HalsteadReport`.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// Distinct operators.
    pub h1: u64,
    /// Distinct operands.
    pub h2: u64,
    /// Total operators.
    pub n1: u64,
    /// Total operands.
    pub n2: u64,
    /// `h1 * log2(h1) + h2 * log2(h2)`, or the int 0 unless both `h1` and
    /// `h2` are non-zero.
    pub calculated_length: PyNum,
    /// `(N1 + N2) * log2(h1 + h2)`, or the int 0 for an empty vocabulary.
    pub volume: PyNum,
    /// `(h1 * N2) / (2 * h2)`, or the int 0 without operands.
    pub difficulty: PyNum,
    /// `difficulty * volume`.
    pub effort: PyNum,
    /// `effort / 18`: Halstead's estimate of seconds to write the code.
    pub time: f64,
    /// `volume / 3000`: Halstead's estimate of delivered bugs.
    pub bugs: f64,
}

impl Report {
    /// `halstead_visitor_report`, branch for branch, so each value keeps the
    /// Python type it has there.
    #[allow(clippy::cast_precision_loss)] // Python's int-to-float conversion
    fn new(h1: u64, h2: u64, n1: u64, n2: u64) -> Self {
        let h = h1 + h2;
        let n = n1 + n2;
        let calculated_length = if h1 != 0 && h2 != 0 {
            PyNum::Float(h1 as f64 * log2(h1) + h2 as f64 * log2(h2))
        } else {
            PyNum::Int(0)
        };
        let volume = if h == 0 {
            PyNum::Int(0)
        } else {
            PyNum::Float(n as f64 * log2(h))
        };
        let difficulty = if h2 == 0 {
            PyNum::Int(0)
        } else {
            PyNum::Float((h1 * n2) as f64 / (2 * h2) as f64)
        };
        let effort = difficulty.mul(volume);
        Self {
            h1,
            h2,
            n1,
            n2,
            calculated_length,
            volume,
            difficulty,
            effort,
            time: effort.as_f64() / 18.0,
            bugs: volume.as_f64() / 3000.0,
        }
    }

    /// The fields in radon's order, under radon's names.
    pub(crate) fn fields(&self) -> [(&'static str, PyNum); 12] {
        [
            ("h1", PyNum::Int(self.h1)),
            ("h2", PyNum::Int(self.h2)),
            ("N1", PyNum::Int(self.n1)),
            ("N2", PyNum::Int(self.n2)),
            ("vocabulary", PyNum::Int(self.h1 + self.h2)),
            ("length", PyNum::Int(self.n1 + self.n2)),
            ("calculated_length", self.calculated_length),
            ("volume", self.volume),
            ("difficulty", self.difficulty),
            ("effort", self.effort),
            ("time", PyNum::Float(self.time)),
            ("bugs", PyNum::Float(self.bugs)),
        ]
    }
}

/// `math.log(x, 2)` on an int: CPython divides two natural logarithms (the
/// same libm `log`), which is not always `log2(x)` to the last bit.
#[allow(clippy::cast_precision_loss)] // Python's int-to-float conversion
fn log2(x: u64) -> f64 {
    (x as f64).ln() / 2f64.ln()
}

/// Compute radon's Halstead metrics for a module.
///
/// Code is accepted exactly as [`crate::cc::analyze`] accepts it: radon
/// parses it with the same `ast.parse`.
///
/// Parsing and visiting recurse once per nesting level, so pathologically
/// deep input needs a large stack; the CLI runs this on 64 MiB threads.
///
/// ```
/// let h = oxradon::hal::analyze("def area(w, h):\n    return w * h + 1\n")?;
///
/// // Two operators (`*`, `+`), each used once.
/// assert_eq!((h.total.h1, h.total.n1), (2, 2));
/// // The module's totals, and the same counts for its one function.
/// assert_eq!(h.functions[0].0, "area");
/// assert_eq!(h.functions[0].1, h.total);
/// # Ok::<(), oxradon::Error>(())
/// ```
///
/// # Errors
/// The syntax error of code that CPython 3.14 does not accept.
pub fn analyze(text: &str) -> Result<Halstead, Error> {
    let text = universal_newlines(text);
    let text = text.as_ref();
    let src = Source::new(text);
    let parsed = crate::syntax::parse(text, &src)?;
    let mut visitor = HalsteadVisitor::new(text, None);
    for stmt in &parsed.syntax().body {
        visitor.visit_stmt(stmt);
    }
    Ok(Halstead {
        total: visitor.counts.report(),
        functions: (visitor.functions.into_iter())
            .map(|(name, report)| (name.to_owned(), report))
            .collect(),
    })
}

/// CPython's tokenizer translates every CRLF and lone CR to LF before
/// parsing (radon's files are also read in text mode; stdin keeps its CRs,
/// and they are translated there). That matters inside string literals, whose
/// values Ruff copies from the source. (Positions don't matter here; `cc`,
/// which reports them, parses the text as it is.)
fn universal_newlines(text: &str) -> Cow<'_, str> {
    if text.contains('\r') {
        Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(text)
    }
}

/// An operator, identified as radon identifies it: by its AST class name.
/// The class names of the four kinds never overlap, and `AugAssign` shares
/// `BinOp`'s classes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum OpKey {
    Bin(Operator),
    Unary(UnaryOp),
    Bool(BoolOp),
    Cmp(CmpOp),
}

/// An operand's value, compared as Python compares it inside a `set`.
///
/// - `Name.id`, `Attribute.attr` and `str` constants are all `str`.
/// - Numbers compare by value across `bool`, `int`, `float` and `complex`, so
///   every integral value is an integer here, in a canonical form: [`Key::Int`]
///   when it fits in a `u64`, otherwise [`Key::BigInt`]. Literals are never
///   negative or NaN.
/// - Any other node is compared by identity: every occurrence is distinct.
#[derive(Debug, PartialEq, Eq, Hash)]
enum Key<'a> {
    Str(Cow<'a, str>),
    /// A `str` with lone surrogates, as code points: it equals no name and no
    /// surrogate-free string.
    Surrogates(Box<[u32]>),
    Bytes(Box<[u8]>),
    Int(u64),
    /// An integer above `u64::MAX`, as base-2³² limbs (least significant
    /// first, no high zero limbs).
    BigInt(Box<[u32]>),
    /// The bits of a float that is not integral (or is infinite).
    Float(u64),
    /// The bits of an imaginary literal's non-zero imaginary part.
    Imag(u64),
    None,
    Ellipsis,
    /// The node's address: unique among the nodes of one AST.
    Node(usize),
}

/// Counts and distinct sets, as `HalsteadVisitor` keeps them.
#[derive(Default)]
struct Counts<'a> {
    operators: HashSet<OpKey>,
    operands: HashSet<(Option<&'a str>, Key<'a>)>,
    n1: u64,
    n2: u64,
}

impl<'a> Counts<'a> {
    /// Add a child visitor's counts and sets (`visit_FunctionDef`'s updates).
    fn absorb(&mut self, other: Counts<'a>) {
        self.n1 += other.n1;
        self.n2 += other.n2;
        self.operators.extend(other.operators);
        self.operands.extend(other.operands);
    }

    fn report(&self) -> Report {
        Report::new(
            self.operators.len() as u64,
            self.operands.len() as u64,
            self.n1,
            self.n2,
        )
    }
}

/// radon's `HalsteadVisitor`, over Ruff's AST. Ruff's default walk visits the
/// same child expressions as CPython's `generic_visit`; only functions differ.
struct HalsteadVisitor<'a> {
    text: &'a str,
    /// The enclosing function's name (`None` at module level).
    context: Option<&'a str>,
    counts: Counts<'a>,
    /// `function_visitors`: the functions this visitor met directly.
    functions: Vec<(&'a str, Report)>,
}

impl<'a> HalsteadVisitor<'a> {
    fn new(text: &'a str, context: Option<&'a str>) -> Self {
        Self {
            text,
            context,
            counts: Counts::default(),
            functions: Vec::new(),
        }
    }

    /// One operator node: its operator, and its operand children.
    fn count(&mut self, op: OpKey, operands: impl IntoIterator<Item = &'a Expr>) {
        self.counts.n1 += 1;
        self.counts.operators.insert(op);
        self.add_operands(operands);
    }

    fn add_operands(&mut self, operands: impl IntoIterator<Item = &'a Expr>) {
        for operand in operands {
            self.counts.n2 += 1;
            let key = operand_key(self.text, operand);
            self.counts.operands.insert((self.context, key));
        }
    }

    /// `visit_FunctionDef` (and `visit_AsyncFunctionDef`): only the body is
    /// visited, under the function's name. Its counts go to this visitor and
    /// to a report of its own; functions nested in it are counted, not listed.
    fn visit_function_def(&mut self, node: &'a ast::StmtFunctionDef) {
        let name = node.name.as_str();
        let mut child = HalsteadVisitor::new(self.text, Some(name));
        for stmt in &node.body {
            child.visit_stmt(stmt);
        }
        self.functions.push((name, child.counts.report()));
        self.counts.absorb(child.counts);
    }
}

impl<'a> Visitor<'a> for HalsteadVisitor<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(node) => return self.visit_function_def(node),
            Stmt::AugAssign(node) => self.count(OpKey::Bin(node.op), [&*node.target, &*node.value]),
            _ => {}
        }
        visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::BinOp(node) => self.count(OpKey::Bin(node.op), [&*node.left, &*node.right]),
            Expr::UnaryOp(node) => self.count(OpKey::Unary(node.op), [&*node.operand]),
            Expr::BoolOp(node) => self.count(OpKey::Bool(node.op), &node.values),
            Expr::Compare(node) => {
                self.counts.n1 += node.ops.len() as u64;
                (self.counts.operators).extend(node.ops.iter().copied().map(OpKey::Cmp));
                // Ruff keeps `left` and the comparators together.
                self.add_operands(&node.operands);
            }
            _ => {}
        }
        visitor::walk_expr(self, expr);
    }
}

/// The value radon puts in its operand set: `getattr(operand, attr,
/// operand)` with `attr` from `HalsteadVisitor.types`.
fn operand_key<'a>(text: &str, expr: &'a Expr) -> Key<'a> {
    match expr {
        Expr::Name(node) => Key::Str(Cow::Borrowed(node.id.as_str())),
        Expr::Attribute(node) => Key::Str(Cow::Borrowed(node.attr.as_str())),
        Expr::StringLiteral(node) => string_key(text, &node.value),
        Expr::BytesLiteral(node) => Key::Bytes(node.value.bytes().collect()),
        Expr::NumberLiteral(node) => number_key(&node.value),
        Expr::BooleanLiteral(node) => Key::Int(u64::from(node.value)),
        Expr::NoneLiteral(_) => Key::None,
        Expr::EllipsisLiteral(_) => Key::Ellipsis,
        _ => Key::Node(std::ptr::from_ref(expr).addr()),
    }
}

fn number_key(number: &Number) -> Key<'static> {
    match number {
        Number::Int(int) => match int.as_u64() {
            Some(value) => Key::Int(value),
            // Ruff keeps the token of an integer above `u64::MAX`.
            None => Key::BigInt(big_int_limbs(&int.to_string())),
        },
        Number::Float(value) => float_key(*value),
        // An imaginary literal: its real part is always 0.
        Number::Complex { imag, .. } if *imag == 0.0 => Key::Int(0),
        Number::Complex { imag, .. } => Key::Imag(imag.to_bits()),
    }
}

/// A float literal: integral values compare equal to ints.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // checked range
fn float_key(value: f64) -> Key<'static> {
    if !value.is_finite() || value.fract() != 0.0 {
        return Key::Float(value.to_bits());
    }
    if value < 18_446_744_073_709_551_616.0 {
        // Includes -0.0.
        return Key::Int(value as u64);
    }
    // value = mantissa * 2^exponent, with exponent >= 12 here.
    let bits = value.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as usize - 1075;
    let mantissa = u128::from((bits & ((1 << 52) - 1)) | (1 << 52)) << (exponent % 32);
    let mut limbs = vec![0; exponent / 32];
    limbs.extend([
        mantissa as u32,
        (mantissa >> 32) as u32,
        (mantissa >> 64) as u32,
    ]);
    Key::BigInt(normalized(limbs))
}

/// An integer token (`0x…`, `0o…`, `0b…` or decimal, maybe with `_`) as
/// base-2³² limbs, least significant first. Power-of-two radixes are packed
/// bit by bit (linear time, as they have no length limit); decimal literals
/// take quadratic time, bounded because `syntax::parse` rejects any longer
/// than [`MAX_STR_DIGITS`].
#[allow(clippy::cast_possible_truncation)] // carries fit in a limb
fn big_int_limbs(token: &str) -> Box<[u32]> {
    let lower = token.to_ascii_lowercase();
    let (radix, digits) = match lower.get(..2) {
        Some("0x") => (16, &lower[2..]),
        Some("0o") => (8, &lower[2..]),
        Some("0b") => (2, &lower[2..]),
        _ => (10, lower.as_str()),
    };
    let digits = digits.chars().filter_map(|c| c.to_digit(radix));
    let mut limbs: Vec<u32> = Vec::new();
    if radix == 10 {
        debug_assert!(lower.bytes().filter(u8::is_ascii_digit).count() <= MAX_STR_DIGITS);
        for digit in digits {
            let mut carry = u64::from(digit);
            for limb in &mut limbs {
                let value = u64::from(*limb) * 10 + carry;
                *limb = value as u32;
                carry = value >> 32;
            }
            if carry > 0 {
                limbs.push(carry as u32);
            }
        }
    } else {
        let width = radix.trailing_zeros() as usize;
        let digits: Vec<u32> = digits.collect();
        let mut bit = 0;
        for digit in digits.into_iter().rev() {
            let (index, shift) = (bit / 32, bit % 32);
            if limbs.len() < index + 2 {
                limbs.resize(index + 2, 0);
            }
            let wide = u64::from(digit) << shift;
            limbs[index] |= wide as u32;
            limbs[index + 1] |= (wide >> 32) as u32;
            bit += width;
        }
    }
    normalized(limbs)
}

/// Limbs without high zero limbs, so equal values have equal keys.
fn normalized(mut limbs: Vec<u32>) -> Box<[u32]> {
    while limbs.last() == Some(&0) {
        limbs.pop();
    }
    limbs.into()
}

/// A `str` constant. Ruff decodes surrogate escapes (`\uD800`) to U+FFFD,
/// which Python keeps as lone surrogates; so a value containing U+FFFD is
/// re-derived from the source to tell them apart.
fn string_key<'a>(text: &str, value: &'a StringLiteralValue) -> Key<'a> {
    let joined = value.to_str();
    if !joined.contains(char::REPLACEMENT_CHARACTER) {
        return Key::Str(Cow::Borrowed(joined));
    }
    let mut code_points = Vec::with_capacity(joined.len());
    for part in value {
        let raw = part.flags.prefix().is_raw();
        let content = TextRange::new(
            part.start() + part.flags.opener_len(),
            part.end() - part.flags.closer_len(),
        );
        let start = code_points.len();
        if raw || !recover_surrogates(&text[content], &part.value, &mut code_points) {
            code_points.truncate(start);
            code_points.extend(part.value.chars().map(u32::from));
        }
    }
    if code_points.iter().any(|c| (0xD800..=0xDFFF).contains(c)) {
        Key::Surrogates(code_points.into())
    } else {
        Key::Str(Cow::Borrowed(joined))
    }
}

/// Walk a non-raw literal's source and Ruff's decoded value in step, writing
/// the value's code points with surrogate escapes restored. Returns `false` if
/// the two fall out of step (then the decoded value is used as it is).
fn recover_surrogates(source: &str, decoded: &str, out: &mut Vec<u32>) -> bool {
    let mut decoded = decoded.chars();
    let mut source = source.chars();
    let take = |decoded: &mut std::str::Chars<'_>, out: &mut Vec<u32>| {
        decoded.next().map(|c| out.push(u32::from(c)))
    };
    while let Some(c) = source.next() {
        if c != '\\' {
            if take(&mut decoded, out).is_none() {
                return false;
            }
            continue;
        }
        let Some(escape) = source.next() else {
            return false;
        };
        let produced = match escape {
            // A backslash-newline continues the line and decodes to nothing.
            '\n' => continue,
            'u' | 'U' => {
                let len = if escape == 'u' { 4 } else { 8 };
                let hex: String = source.by_ref().take(len).collect();
                let Ok(code) = u32::from_str_radix(&hex, 16) else {
                    return false;
                };
                if (0xD800..=0xDFFF).contains(&code) {
                    // Ruff's U+FFFD stands for this surrogate.
                    out.push(code);
                    decoded.next().map(|_| ())
                } else {
                    take(&mut decoded, out)
                }
            }
            'x' => {
                source.by_ref().take(2).for_each(drop);
                take(&mut decoded, out)
            }
            '0'..='7' => {
                // Up to three octal digits in all.
                let rest = source.as_str();
                let more = rest.chars().take(2).take_while(|d| d.is_digit(8)).count();
                source = rest[more..].chars();
                take(&mut decoded, out)
            }
            'N' => {
                let rest = source.as_str();
                let Some(end) = rest.find('}') else {
                    return false;
                };
                source = rest[end + 1..].chars();
                take(&mut decoded, out)
            }
            '\\' | '\'' | '"' | 'a' | 'b' | 'f' | 'n' | 'r' | 't' | 'v' => take(&mut decoded, out),
            // An unknown escape keeps its backslash.
            _ => take(&mut decoded, out).and_then(|()| take(&mut decoded, out)),
        };
        if produced.is_none() {
            return false;
        }
    }
    decoded.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total(code: &str) -> Report {
        analyze(code).expect("valid code").total
    }

    #[test]
    fn integral_floats_and_big_ints_are_canonical() {
        let two_64 = Key::BigInt([0, 0, 1].into());
        assert_eq!(float_key(18_446_744_073_709_551_616.0), two_64);
        assert_eq!(Key::BigInt(big_int_limbs("18446744073709551616")), two_64);
        assert_eq!(
            Key::BigInt(big_int_limbs("0x1_0000_0000_0000_0000")),
            two_64
        );
        assert_eq!(
            Key::BigInt(big_int_limbs("0o2000000000000000000000")),
            two_64
        );
        assert_eq!(
            Key::BigInt(big_int_limbs(&format!("0b1{}", "0".repeat(64)))),
            two_64
        );
        assert_eq!(
            float_key(1e20),
            Key::BigInt(big_int_limbs("100000000000000000000"))
        );
        // 2^1000 is exact as a float (1e300 is not 10^300).
        let two_1000 = big_int_limbs(&format!("0x1{}", "0".repeat(250)));
        assert_eq!(float_key(2f64.powi(1000)), Key::BigInt(two_1000));
        assert_eq!(float_key(-0.0), Key::Int(0));
        assert_eq!(float_key(2.5), Key::Float(2.5f64.to_bits()));
    }

    #[test]
    fn python_equality_merges_operands() {
        // `x` the name and `'x'` the string are the same `str`.
        assert_eq!(total("y = x + 'x'\n").h2, 1);
        // 1, 1.0, True are equal; 0j and 0 too; the three BinOps are distinct.
        assert_eq!(total("y = 1 + 1.0 + True + 0j*0\n").h2, 5);
        assert_eq!(total("y = 100000000000000000000 + 1e20\n").h2, 1);
        assert_eq!(total("y = b'a' + 'a'\n").h2, 2);
        assert_eq!(total("y = 'ab' + 'a' 'b'\n").h2, 1);
        assert_eq!(total("y = f'a' + f'a'\n").h2, 2);
    }

    #[test]
    fn surrogates_stay_distinct() {
        // Built at runtime: an editing hazard turns such escapes into
        // characters.
        let escape = |hex: &str| format!("{}u{hex}", '\\');
        let code = format!(
            "y = '{}' + '{}' + '{}'\n",
            escape("d800"),
            escape("dfff"),
            escape("fffd")
        );
        assert_eq!(total(&code).h2, 4);
        let raw = format!("y = r'{}' + '{}'\n", escape("d800"), escape("d800"));
        // The raw one is six plain characters.
        assert_eq!(total(&raw).h2, 2);
    }

    #[test]
    fn surrogate_recovery_follows_the_escape_scanner() {
        // Each pair is one Python `str` value, so h2 = 1 (the BinOp is not an
        // operand of anything), except where noted.
        let e = |rest: &str| format!("{}{rest}", '\\');
        let cases = [
            (
                format!("'{}{}'", e("777"), e("ud800")),
                format!("'{}{}'", e("u01ff"), e("ud800")),
                1,
            ),
            // An unknown escape keeps its backslash.
            (
                format!("'{}{}'", e("q"), e("ud800")),
                format!("'{}{}{}'", e(""), e("q"), e("ud800")),
                1,
            ),
            (
                format!("'{}{}'", e("N{REPLACEMENT CHARACTER}"), e("ud800")),
                format!("'{}{}'", e("ufffd"), e("ud800")),
                1,
            ),
            // Two surrogates are not the character they would pair into.
            (
                format!("'{}{}'", e("ud800"), e("udc00")),
                format!("'{}'", e("U00010000")),
                2,
            ),
            // A line continuation, and a non-ASCII character, before a
            // surrogate.
            (
                format!("'é{}\n{}'", e(""), e("ud800")),
                format!("'é{}'", e("ud800")),
                1,
            ),
            // A raw part and an escaped part, implicitly concatenated.
            (
                format!("r'x' '{}'", e("ud800")),
                format!("'x{}'", e("ud800")),
                1,
            ),
        ];
        for (a, b, h2) in cases {
            let code = format!("y = {a} + {b}\n");
            assert_eq!(total(&code).h2, h2, "{code}");
        }
    }

    #[test]
    fn node_identity_is_file_wide() {
        let h = analyze("def f():\n    [] + []\n    [] + []\n").expect("valid code");
        assert_eq!(h.total.h2, 4);
        assert_eq!(h.functions[0].1.h2, 4);
    }

    #[test]
    fn physical_newlines_in_literals_are_universal() {
        assert_eq!(total("y = '''a\rb''' + 'a\\nb'\n").h2, 1);
        assert_eq!(total("y = b'''a\r\nb''' + b'a\\nb'\n").h2, 1);
        // An escaped CR stays a CR.
        assert_eq!(total("y = 'a\\rb' + 'a\\nb'\n").h2, 2);
    }

    #[test]
    fn functions_report_body_only_and_roll_up() {
        let h = analyze(
            "@deco(1 + 2)\ndef f(a=1 + 2):\n    def g(b):\n        return b + 1\n    return a + g(a)\n",
        )
        .expect("valid code");
        assert_eq!(h.functions.len(), 1);
        assert_eq!(h.functions[0].0, "f");
        assert_eq!((h.total.h1, h.total.h2, h.total.n1), (1, 4, 2));
        assert_eq!(h.functions[0].1, h.total);
    }

    #[test]
    fn degenerate_report_keeps_python_ints() {
        let fields = total("pass\n").fields();
        assert_eq!(fields[6], ("calculated_length", PyNum::Int(0)));
        assert_eq!(fields[9], ("effort", PyNum::Int(0)));
        assert_eq!(fields[10], ("time", PyNum::Float(0.0)));
        assert_eq!(PyNum::Float(0.0).to_string(), "0.0");
    }
}
