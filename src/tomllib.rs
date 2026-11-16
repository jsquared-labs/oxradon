//! A port of CPython 3.14's `tomllib` (`Lib/tomllib/_parser.py` and
//! `Lib/tomllib/_re.py`), with `parse_float=float`, plus Python's `str()` and
//! `repr()` of the values it returns.
//!
//! radon reads `pyproject.toml` with `tomllib.load` and `str()`s every value,
//! so this must accept exactly the documents tomllib accepts and render the
//! same text. The parser mirrors tomllib function for function, by code point,
//! after the same `"\r\n"` → `"\n"` replacement. Values keep Python's types:
//! integers are unbounded (`int(text, 0)`), floats are correctly rounded from
//! the text `float()` gets, and date/times are validated as `datetime`'s
//! constructors validate them.
//!
//! # Exceptions
//!
//! `TOMLDecodeError` is [`LoadError::Decode`]; radon crashes on it. Every
//! other exception tomllib can raise is [`LoadError::Discard`], because radon
//! then silently drops the whole TOML config:
//! - `ValueError` from `int()` on a decimal literal of more than 4300 digits
//!   (CPython's default `sys.get_int_max_str_digits()`);
//! - `RecursionError` from a key of more than `MAX_KEY_PARTS` (1000) parts;
//! - `RecursionError` from Python's frame limit, modelled below.
//!
//! # The frame model
//!
//! CPython's recursion limit (1000) counts Python frames, and tomllib recurses
//! on nested arrays and inline tables. Each Rust function here that ports a
//! Python-level function (tomllib's functions and methods, the `__init__`s it
//! runs, `_re`'s helpers, and the key generator in `key_value_rule`) runs
//! inside [`Parser::framed`], which counts one frame and fails with
//! `RecursionError` past [`FRAME_BUDGET`]. Python details that move the count:
//! - constructing a `TOMLDecodeError` runs its Python `__init__`, so a decode
//!   error raised at the very limit becomes a `RecursionError`;
//! - `cached_tz` is behind `functools.lru_cache`: its frame exists only on a
//!   cache miss (the cache lives for the process; radon parses once, so a fresh
//!   cache per [`loads`] call matches it);
//! - a class call (`NestedDict()`, `Flags()`) counts one frame, for its
//!   `__init__`. A specialized call site (`CALL_ALLOC_AND_ENTER_INIT`) also
//!   pushes a shim frame, but never at the limit (measured: CPython falls back
//!   to the generic call there), so the shim is not modelled.
//!
//! [`FRAME_BUDGET`] is calibrated for the `radon` **console script**: with
//! `loads` as frame 1, the frames below it (the script, `radon.main`, the
//! import machinery importing `radon.cli`, `FileConfig`, `toml_config`,
//! `tomllib.load`) leave 988. It reproduces radon 6.0.1 on CPython 3.14.8 for
//! a value under `[tool.other]` (max kept / first discarded): nested arrays
//! 491/492, nested inline tables 327/328, `[{a=…}]` pairs 196/197, arrays
//! inside one inline table 490/491. `python -m radon` starts at a different
//! depth, so its limits differ slightly.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt::Write as _;

use crate::limits::MAX_STR_DIGITS;
use crate::pyfmt;

/// `sys.getrecursionlimit()` when tomllib is imported (CPython's default).
const MAX_KEY_PARTS: usize = 1000;

/// Python frames available to `loads`, itself included, under the `radon`
/// console script (see the module docs).
const FRAME_BUDGET: usize = 988;

/// Integers of at least this many bits have more than [`MAX_STR_DIGITS`]
/// decimal digits: `2**14285 > 10**4300`.
const TOO_MANY_DIGITS_BITS: usize = 14286;

/// A TOML value as tomllib returns it.
///
/// A value can be nested hundreds of thousands of levels deep, so nothing
/// that handles one may recurse per level: converting, rendering and dropping
/// use work lists. `Debug` and `PartialEq` would recurse, so they exist only
/// for the tests.
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(crate) enum Value {
    String(String),
    Integer(Int),
    Float(f64),
    Boolean(bool),
    DateTime(DateTime),
    Date(Date),
    Time(Time),
    Array(Vec<Value>),
    Table(Table),
}

/// A TOML table: keys in document order, like a Python dict.
#[derive(Default)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(crate) struct Table(pub(crate) Vec<(String, Value)>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoadError {
    /// tomllib.TOMLDecodeError: radon crashes (exit 1).
    Decode(String),
    /// Any other exception from `tomllib.loads` (`ValueError` from `int()`'s
    /// 4300-digit limit, `RecursionError`): radon silently discards the whole
    /// TOML config.
    Discard(String),
}

/// A Python `int` of any size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Int(IntRepr);

#[derive(Clone, Debug, PartialEq, Eq)]
enum IntRepr {
    /// A decimal literal: at most [`MAX_STR_DIGITS`] digits, no leading
    /// zeros, and never a negative zero.
    Decimal { negative: bool, digits: String },
    /// A hexadecimal, octal or binary literal (never negative): 32-bit limbs,
    /// least significant first, without high zero limbs.
    Binary(Vec<u32>),
}

/// `datetime.date`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Date {
    pub(crate) year: u16,
    pub(crate) month: u8,
    pub(crate) day: u8,
}

/// `datetime.time` (always naive in TOML).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Time {
    pub(crate) hour: u8,
    pub(crate) minute: u8,
    pub(crate) second: u8,
    pub(crate) microsecond: u32,
}

/// `datetime.datetime`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DateTime {
    pub(crate) date: Date,
    pub(crate) time: Time,
    /// Minutes east of UTC; `Some(0)` is `datetime.timezone.utc` (`Z`,
    /// `+00:00` and `-00:00` alike), `None` a local date-time.
    pub(crate) offset_minutes: Option<i16>,
}

/// `tomllib.loads(text)` (the caller decodes the bytes as strict UTF-8 first).
pub(crate) fn loads(text: &str) -> Result<Table, LoadError> {
    parse_document(text, FRAME_BUDGET)
}

/// [`loads`] with a given frame budget.
///
/// Parsing recurses only within that budget, so the stack it needs is
/// bounded, and configuration is read on the caller's stack (the main
/// thread's, from the command line). How much that is depends on the build
/// and the machine. Measured on arm64 macOS only, the deepest documents the
/// budget accepts were read with a 512 KiB stack by a release build, and
/// needed about 4 MiB in a debug build, whose frames are larger. A
/// release-build test holds the need under 1 MiB. A main thread usually has
/// 8 MiB on macOS and Linux, but that is a setting, not a guarantee.
///
/// What TOML does allow is values nested hundreds of thousands of levels deep
/// (inline tables of 1000-part dotted keys reach about 330,000), which is why
/// converting, rendering and dropping a value never recurse per level.
fn parse_document(text: &str, budget: usize) -> Result<Table, LoadError> {
    // The spec allows converting "\r\n" to "\n", even in string literals.
    let src: Vec<char> = text.replace("\r\n", "\n").chars().collect();
    let mut parser = Parser {
        src: &src,
        depth: 0,
        budget,
        tz_cache: HashSet::new(),
    };
    match parser.loads() {
        Ok(dict) => Ok(dict.into_table()),
        Err(Exc::Decode(msg)) => Err(LoadError::Decode(msg)),
        Err(Exc::Recursion(msg) | Exc::Value(msg)) => Err(LoadError::Discard(msg)),
        Err(Exc::Index) => Err(LoadError::Discard("list index out of range".into())),
        // tomllib catches every KeyError it raises.
        Err(Exc::Key) => Err(LoadError::Discard("KeyError".into())),
    }
}

impl Value {
    /// Python `str(value)`; Err(message) where CPython raises (an int with more
    /// than 4300 decimal digits).
    pub(crate) fn py_str(&self) -> Result<String, String> {
        match self {
            Value::String(s) => Ok(s.clone()),
            Value::DateTime(dt) => Ok(dt.py_str()),
            Value::Date(d) => Ok(d.py_str()),
            Value::Time(t) => Ok(t.py_str()),
            _ => self.py_repr(),
        }
    }

    /// Python `repr(value)`, same error rule (nested anywhere).
    pub(crate) fn py_repr(&self) -> Result<String, String> {
        let mut out = String::new();
        write_pending(vec![Piece::Value(self)], &mut out)?;
        Ok(out)
    }

    /// Move the values directly inside this one to `out`.
    fn take_children(&mut self, out: &mut Vec<Value>) {
        match self {
            Value::Array(items) => out.append(items),
            Value::Table(table) => out.extend(table.0.drain(..).map(|(_, value)| value)),
            // Listed, so that a new variant has to say whether it nests.
            Value::String(_)
            | Value::Integer(_)
            | Value::Float(_)
            | Value::Boolean(_)
            | Value::DateTime(_)
            | Value::Date(_)
            | Value::Time(_) => {}
        }
    }
}

/// Frees a value's children from a work list, not by recursing into them: a
/// value can be nested far deeper than the stack has room for.
impl Drop for Value {
    fn drop(&mut self) {
        let mut pending = Vec::new();
        self.take_children(&mut pending);
        while let Some(mut value) = pending.pop() {
            value.take_children(&mut pending);
        }
    }
}

impl Table {
    /// Used by the tests, which compare whole tables with tomllib's.
    #[cfg(test)]
    /// Python `repr()` of the dict: `{'k': v, ...}`.
    pub(crate) fn py_repr(&self) -> Result<String, String> {
        let mut out = String::new();
        let mut pending = Vec::new();
        open_table(self, &mut pending, &mut out);
        write_pending(pending, &mut out)?;
        Ok(out)
    }
}

/// What is still to be written of a `repr()`, in a work list.
enum Piece<'a> {
    Value(&'a Value),
    /// A dict key, with its colon.
    Key(&'a str),
    Text(&'static str),
}

/// Start a dict's `repr()`: its opening brace now, the rest on the work list.
fn open_table<'a>(table: &'a Table, pending: &mut Vec<Piece<'a>>, out: &mut String) {
    out.push('{');
    pending.push(Piece::Text("}"));
    for (i, (key, value)) in table.0.iter().enumerate().rev() {
        pending.push(Piece::Value(value));
        pending.push(Piece::Key(key));
        if i > 0 {
            pending.push(Piece::Text(", "));
        }
    }
}

/// Python's `repr()`, written from a work list and not by recursion, so that
/// nesting costs no stack. The list is a stack: what is pushed last is
/// written first. The pieces come out in the order the text reads, so the
/// first integer too long to print is the one that fails, as in Python.
fn write_pending(mut pending: Vec<Piece<'_>>, out: &mut String) -> Result<(), String> {
    while let Some(piece) = pending.pop() {
        match piece {
            Piece::Text(text) => out.push_str(text),
            Piece::Key(key) => {
                out.push_str(&pyfmt::py_repr(OsStr::new(key)));
                out.push_str(": ");
            }
            Piece::Value(Value::Array(items)) => {
                out.push('[');
                pending.push(Piece::Text("]"));
                for (i, item) in items.iter().enumerate().rev() {
                    pending.push(Piece::Value(item));
                    if i > 0 {
                        pending.push(Piece::Text(", "));
                    }
                }
            }
            Piece::Value(Value::Table(table)) => open_table(table, &mut pending, out),
            Piece::Value(Value::String(s)) => out.push_str(&pyfmt::py_repr(OsStr::new(s))),
            Piece::Value(Value::Integer(i)) => out.push_str(&i.py_str()?),
            Piece::Value(Value::Float(x)) => out.push_str(&pyfmt::float_repr(*x)),
            Piece::Value(Value::Boolean(b)) => out.push_str(if *b { "True" } else { "False" }),
            Piece::Value(Value::DateTime(dt)) => dt.write_repr(out),
            Piece::Value(Value::Date(d)) => d.write_repr(out),
            Piece::Value(Value::Time(t)) => t.write_repr(out),
        }
    }
    Ok(())
}

impl Int {
    /// `int(text, 0)` for a decimal literal matched by `RE_NUMBER`.
    fn from_decimal_literal(text: &[char]) -> Result<Self, Exc> {
        let (negative, body) = match text.split_first() {
            Some(('-', rest)) => (true, rest),
            Some(('+', rest)) => (false, rest),
            _ => (false, text),
        };
        let digits: String = body.iter().filter(|&&c| c != '_').collect();
        if digits.len() > MAX_STR_DIGITS {
            return Err(Exc::Value(format!(
                "Exceeds the limit ({MAX_STR_DIGITS} digits) for integer string conversion: \
                 value has {} digits; use sys.set_int_max_str_digits() to increase the limit",
                digits.len()
            )));
        }
        let negative = negative && digits != "0";
        Ok(Int(IntRepr::Decimal { negative, digits }))
    }

    /// `int(text, 0)` for a `0x`/`0o`/`0b` literal: no digit limit.
    fn from_prefixed_literal(text: &[char]) -> Self {
        let bits: usize = match text.get(1) {
            Some('x') => 4,
            Some('o') => 3,
            _ => 1,
        };
        let mut limbs = Vec::new();
        let mut acc: u64 = 0;
        let mut filled = 0;
        for &c in text[2..].iter().rev() {
            let Some(digit) = c.to_digit(16) else {
                continue; // '_'
            };
            acc |= u64::from(digit) << filled;
            filled += bits;
            if filled >= 32 {
                limbs.push(low_u32(acc));
                acc >>= 32;
                filled -= 32;
            }
        }
        if acc != 0 {
            limbs.push(low_u32(acc));
        }
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
        Int(IntRepr::Binary(limbs))
    }

    /// Python `str()`: decimal, or Err past the 4300-digit limit.
    fn py_str(&self) -> Result<String, String> {
        let too_long = || {
            format!(
                "Exceeds the limit ({MAX_STR_DIGITS} digits) for integer string conversion; \
                 use sys.set_int_max_str_digits() to increase the limit"
            )
        };
        match &self.0 {
            IntRepr::Decimal { negative, digits } => Ok(if *negative {
                format!("-{digits}")
            } else {
                digits.clone()
            }),
            IntRepr::Binary(limbs) => {
                let Some(&top) = limbs.last() else {
                    return Ok("0".into());
                };
                let bit_length = 32 * (limbs.len() - 1) + (32 - top.leading_zeros() as usize);
                if bit_length >= TOO_MANY_DIGITS_BITS {
                    return Err(too_long());
                }
                let digits = limbs_to_decimal(limbs.clone());
                if digits.len() > MAX_STR_DIGITS {
                    return Err(too_long());
                }
                Ok(digits)
            }
        }
    }
}

fn low_u32(x: u64) -> u32 {
    u32::try_from(x & 0xffff_ffff).unwrap_or_default()
}

/// Decimal digits of a nonzero little-endian limb vector.
fn limbs_to_decimal(mut limbs: Vec<u32>) -> String {
    const CHUNK: u64 = 1_000_000_000;
    let mut chunks = Vec::new();
    while !limbs.is_empty() {
        let mut rem: u64 = 0;
        for limb in limbs.iter_mut().rev() {
            let cur = (rem << 32) | u64::from(*limb);
            *limb = low_u32(cur / CHUNK);
            rem = cur % CHUNK;
        }
        chunks.push(rem);
        while limbs.last() == Some(&0) {
            limbs.pop();
        }
    }
    let mut out = String::new();
    for (i, chunk) in chunks.iter().rev().enumerate() {
        if i == 0 {
            let _ = write!(out, "{chunk}");
        } else {
            let _ = write!(out, "{chunk:09}");
        }
    }
    out
}

impl Date {
    /// `date.__str__`: `YYYY-MM-DD`.
    fn py_str(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    fn write_repr(self, out: &mut String) {
        let _ = write!(
            out,
            "datetime.date({}, {}, {})",
            self.year, self.month, self.day
        );
    }
}

impl Time {
    /// `time.__str__`: `HH:MM:SS`, plus `.ffffff` when there are microseconds.
    fn py_str(self) -> String {
        let mut out = format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second);
        if self.microsecond != 0 {
            let _ = write!(out, ".{:06}", self.microsecond);
        }
        out
    }

    /// The arguments of `time.__repr__`: seconds only when seconds or
    /// microseconds are nonzero, microseconds only when nonzero.
    fn write_repr_args(self, out: &mut String) {
        let _ = write!(out, "{}, {}", self.hour, self.minute);
        if self.microsecond != 0 {
            let _ = write!(out, ", {}, {}", self.second, self.microsecond);
        } else if self.second != 0 {
            let _ = write!(out, ", {}", self.second);
        }
    }

    fn write_repr(self, out: &mut String) {
        out.push_str("datetime.time(");
        self.write_repr_args(out);
        out.push(')');
    }
}

impl DateTime {
    /// `datetime.__str__`: `isoformat(sep=' ')`.
    fn py_str(self) -> String {
        let mut out = format!("{} {}", self.date.py_str(), self.time.py_str());
        if let Some(offset) = self.offset_minutes {
            let sign = if offset < 0 { '-' } else { '+' };
            let minutes = offset.unsigned_abs();
            let _ = write!(out, "{sign}{:02}:{:02}", minutes / 60, minutes % 60);
        }
        out
    }

    fn write_repr(self, out: &mut String) {
        let Date { year, month, day } = self.date;
        let _ = write!(out, "datetime.datetime({year}, {month}, {day}, ");
        self.time.write_repr_args(out);
        match self.offset_minutes {
            None => {}
            Some(0) => out.push_str(", tzinfo=datetime.timezone.utc"),
            Some(offset) => {
                // timedelta normalizes to 0 <= seconds < 86400.
                let total = i32::from(offset) * 60;
                let (days, seconds) = (total.div_euclid(86_400), total.rem_euclid(86_400));
                out.push_str(", tzinfo=datetime.timezone(datetime.timedelta(");
                if days != 0 {
                    let _ = write!(out, "days={days}");
                    if seconds != 0 {
                        out.push_str(", ");
                    }
                }
                if seconds != 0 {
                    let _ = write!(out, "seconds={seconds}");
                }
                out.push_str("))");
            }
        }
        out.push(')');
    }
}

// --- The parser's data: Python dicts and lists while parsing. ---

/// A value while parsing. Arrays and tables stay mutable, as Python's lists
/// and dicts do, until [`Dict::into_table`].
#[cfg_attr(test, derive(Debug))]
enum Node {
    Leaf(Value),
    List(Vec<Node>),
    Dict(Dict),
}

/// A list or dict being converted by [`Node::into_value`]: the children
/// still to convert, and the ones done.
enum Converting {
    List {
        rest: std::vec::IntoIter<Node>,
        done: Vec<Value>,
    },
    Dict {
        rest: std::vec::IntoIter<(String, Node)>,
        done: Vec<(String, Value)>,
        /// The key of the child being converted.
        key: Option<String>,
    },
}

impl Converting {
    fn next_child(&mut self) -> Option<Node> {
        match self {
            Self::List { rest, .. } => rest.next(),
            Self::Dict { rest, key, .. } => {
                let (name, node) = rest.next()?;
                *key = Some(name);
                Some(node)
            }
        }
    }

    fn push(&mut self, value: Value) {
        match self {
            Self::List { done, .. } => done.push(value),
            Self::Dict { done, key, .. } => {
                // `next_child` set the key before this child was converted.
                debug_assert!(key.is_some());
                if let Some(key) = key.take() {
                    done.push((key, value));
                }
            }
        }
    }

    fn finish(self) -> Value {
        match self {
            Self::List { done, .. } => Value::Array(done),
            Self::Dict { done, .. } => Value::Table(Table(done)),
        }
    }
}

impl Node {
    /// The finished value. A work list stands in for recursion: `open` holds
    /// the containers on the way down to the node being converted.
    fn into_value(self) -> Value {
        let mut open: Vec<Converting> = Vec::new();
        let mut node = self;
        loop {
            // Down: open containers until a leaf, or an empty container,
            // gives a value.
            let mut value = loop {
                let mut container = match &mut node {
                    Node::Leaf(value) => break std::mem::replace(value, Value::Boolean(false)),
                    Node::List(items) => Converting::List {
                        done: Vec::with_capacity(items.len()),
                        rest: std::mem::take(items).into_iter(),
                    },
                    Node::Dict(dict) => Converting::Dict {
                        done: Vec::with_capacity(dict.entries.len()),
                        rest: std::mem::take(&mut dict.entries).into_iter(),
                        key: None,
                    },
                };
                match container.next_child() {
                    Some(child) => {
                        open.push(container);
                        node = child;
                    }
                    None => break container.finish(),
                }
            };
            // Up: hand the value to its container; a container with no child
            // left becomes a value for its own container.
            loop {
                let Some(mut container) = open.pop() else {
                    return value;
                };
                container.push(value);
                match container.next_child() {
                    Some(child) => {
                        open.push(container);
                        node = child;
                        break;
                    }
                    None => value = container.finish(),
                }
            }
        }
    }

    /// Move the nodes directly inside this one to `out`.
    fn take_children(&mut self, out: &mut Vec<Node>) {
        match self {
            Node::List(items) => out.append(items),
            Node::Dict(dict) => out.extend(dict.entries.drain(..).map(|(_, node)| node)),
            Node::Leaf(_) => {}
        }
    }

    /// `isinstance(value, (dict, list))`.
    fn is_container(&self) -> bool {
        matches!(self, Node::List(_) | Node::Dict(_))
    }
}

/// As for [`Value`]: freed from a work list, since a parse that fails part-way
/// can leave a tree as deep as any finished one.
impl Drop for Node {
    fn drop(&mut self) {
        let mut pending = Vec::new();
        self.take_children(&mut pending);
        while let Some(mut node) = pending.pop() {
            node.take_children(&mut pending);
        }
    }
}

/// An insertion-ordered dict.
#[derive(Default)]
#[cfg_attr(test, derive(Debug))]
struct Dict {
    entries: Vec<(String, Node)>,
    index: HashMap<String, usize>,
}

impl Dict {
    fn contains(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut Node> {
        let i = *self.index.get(key)?;
        Some(&mut self.entries[i].1)
    }

    /// `self[key] = value` for a key not present.
    fn insert_new(&mut self, key: String, value: Node) -> usize {
        let i = self.entries.len();
        self.index.insert(key.clone(), i);
        self.entries.push((key, value));
        i
    }

    fn into_table(self) -> Table {
        Table(
            self.entries
                .into_iter()
                .map(|(key, node)| (key, node.into_value()))
                .collect(),
        )
    }
}

/// `Flags.FROZEN`: an immutable namespace (inline array or inline table).
const FROZEN: u8 = 1;
/// `Flags.EXPLICIT_NEST`: a nest that has been explicitly created and can no
/// longer be opened using the `[table]` syntax.
const EXPLICIT_NEST: u8 = 2;

/// One entry of `Flags._flags`: `{"flags", "recursive_flags", "nested"}`.
#[derive(Default)]
#[cfg_attr(test, derive(Debug))]
struct FlagEntry {
    flags: u8,
    recursive_flags: u8,
    nested: HashMap<String, FlagEntry>,
}

/// Flags nest once per key part, so as deep as the values: freed from a work
/// list too.
impl Drop for FlagEntry {
    fn drop(&mut self) {
        // Destructured, so that a new field has to say whether it nests.
        let Self {
            flags: _,
            recursive_flags: _,
            nested,
        } = self;
        let mut pending: Vec<FlagEntry> = nested.drain().map(|(_, entry)| entry).collect();
        while let Some(mut entry) = pending.pop() {
            pending.extend(entry.nested.drain().map(|(_, entry)| entry));
        }
    }
}

/// `class Flags`.
#[derive(Default)]
#[cfg_attr(test, derive(Debug))]
struct Flags {
    flags: HashMap<String, FlagEntry>,
    pending: HashSet<(Vec<String>, u8)>,
}

/// `class Output`.
#[derive(Default)]
#[cfg_attr(test, derive(Debug))]
struct Output {
    data: Dict,
    flags: Flags,
}

/// The exceptions tomllib can raise or catch.
#[derive(Debug)]
enum Exc {
    /// `TOMLDecodeError`, with its formatted message.
    Decode(String),
    /// `RecursionError`.
    Recursion(String),
    /// `ValueError`: `int()`'s digit limit, or a `datetime` constructor
    /// (caught by `parse_value`).
    Value(String),
    /// `KeyError` from `NestedDict` (always caught).
    Key,
    /// `IndexError` from `cont[-1]` on an empty list in `get_or_create_nest`.
    Index,
}

fn recursion_error() -> Exc {
    Exc::Recursion("maximum recursion depth exceeded".into())
}

// --- Character classes. ---

fn is_ascii_ctrl(c: char) -> bool {
    c < '\x20' || c == '\x7f'
}

/// `ILLEGAL_BASIC_STR_CHARS`, also `ILLEGAL_LITERAL_STR_CHARS` and
/// `ILLEGAL_COMMENT_CHARS`.
fn illegal_basic_str(c: char) -> bool {
    is_ascii_ctrl(c) && c != '\t'
}

/// `ILLEGAL_MULTILINE_BASIC_STR_CHARS`, also
/// `ILLEGAL_MULTILINE_LITERAL_STR_CHARS`.
fn illegal_multiline_basic_str(c: char) -> bool {
    is_ascii_ctrl(c) && c != '\t' && c != '\n'
}

fn toml_ws(c: char) -> bool {
    c == ' ' || c == '\t'
}

fn toml_ws_and_newline(c: char) -> bool {
    toml_ws(c) || c == '\n'
}

fn bare_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

fn key_initial_char(c: char) -> bool {
    bare_key_char(c) || c == '"' || c == '\''
}

/// Python's `repr()` of a key tuple, for error messages.
fn key_repr(key: &[String]) -> String {
    let parts: Vec<String> = key.iter().map(|k| pyfmt::py_repr(OsStr::new(k))).collect();
    if parts.len() == 1 {
        format!("({},)", parts[0])
    } else {
        format!("({})", parts.join(", "))
    }
}

fn str_repr(s: &str) -> String {
    pyfmt::py_repr(OsStr::new(s))
}

/// The parsing state of one `loads` call, plus the frame model.
struct Parser<'s> {
    src: &'s [char],
    /// Python frames on the stack, `loads` being 1.
    depth: usize,
    budget: usize,
    /// `cached_tz`'s `lru_cache` keys: (sign, hours, minutes).
    tz_cache: HashSet<(char, u8, u8)>,
}

impl Parser<'_> {
    /// Runs `body` as one Python frame.
    fn framed<T>(&mut self, body: impl FnOnce(&mut Self) -> Result<T, Exc>) -> Result<T, Exc> {
        if self.depth >= self.budget {
            return Err(recursion_error());
        }
        self.depth += 1;
        let result = body(self);
        self.depth -= 1;
        result
    }

    /// A call of a plain class: its Python `__init__` is one frame.
    fn instantiate(&mut self) -> Result<(), Exc> {
        self.framed(|_| Ok(()))
    }

    /// `TOMLDecodeError(msg, src, pos)`: its `__init__` is a Python frame too.
    fn decode_error(&mut self, msg: &str, pos: usize) -> Exc {
        if self.depth >= self.budget {
            return recursion_error();
        }
        let doc = self.src;
        let before = &doc[..pos.min(doc.len())];
        let lineno = before.iter().filter(|&&c| c == '\n').count() + 1;
        let colno = match before.iter().rposition(|&c| c == '\n') {
            Some(newline) if lineno > 1 => pos - newline,
            _ => pos + 1,
        };
        let coord = if pos >= doc.len() {
            "end of document".to_owned()
        } else {
            format!("line {lineno}, column {colno}")
        };
        Exc::Decode(format!("{msg} (at {coord})"))
    }

    fn char_at(&self, pos: usize) -> Option<char> {
        self.src.get(pos).copied()
    }

    /// `src.startswith(prefix, pos)`.
    fn starts_with(&self, pos: usize, prefix: &str) -> bool {
        (pos..)
            .zip(prefix.chars())
            .all(|(i, c)| self.char_at(i) == Some(c))
    }

    fn slice(&self, start: usize, end: usize) -> String {
        self.src[start..end].iter().collect()
    }

    fn loads(&mut self) -> Result<Dict, Exc> {
        self.framed(|p| {
            let mut pos = 0;
            // out = Output(): Output.__init__ runs NestedDict() and Flags().
            p.framed(|p| {
                p.instantiate()?;
                p.instantiate()
            })?;
            let mut out = Output::default();
            let mut header: Vec<String> = Vec::new();
            p.framed(|_| Ok(()))?; // make_safe_parse_float(float)

            // Parse one statement at a time (typically one line).
            loop {
                pos = p.skip_chars(pos, toml_ws)?;
                let Some(c) = p.char_at(pos) else { break };
                if c == '\n' {
                    pos += 1;
                    continue;
                }
                if key_initial_char(c) {
                    pos = p.key_value_rule(pos, &mut out, &header)?;
                    pos = p.skip_chars(pos, toml_ws)?;
                } else if c == '[' {
                    let second = p.char_at(pos + 1);
                    p.finalize_pending(&mut out.flags)?;
                    (pos, header) = if second == Some('[') {
                        p.create_list_rule(pos, &mut out)?
                    } else {
                        p.create_dict_rule(pos, &mut out)?
                    };
                    pos = p.skip_chars(pos, toml_ws)?;
                } else if c != '#' {
                    return Err(p.decode_error("Invalid statement", pos));
                }

                pos = p.skip_comment(pos)?;

                let Some(c) = p.char_at(pos) else { break };
                if c != '\n' {
                    return Err(p.decode_error(
                        "Expected newline or end of document after a statement",
                        pos,
                    ));
                }
                pos += 1;
            }
            Ok(out.data)
        })
    }

    // --- Flags methods. ---

    fn add_pending(&mut self, flags: &mut Flags, key: Vec<String>, flag: u8) -> Result<(), Exc> {
        self.framed(|_| {
            flags.pending.insert((key, flag));
            Ok(())
        })
    }

    fn finalize_pending(&mut self, flags: &mut Flags) -> Result<(), Exc> {
        self.framed(|p| {
            for (key, flag) in std::mem::take(&mut flags.pending) {
                p.flags_set(flags, &key, flag, false)?;
            }
            Ok(())
        })
    }

    fn unset_all(&mut self, flags: &mut Flags, key: &[String]) -> Result<(), Exc> {
        self.framed(|_| {
            let Some((stem, parent)) = key.split_last() else {
                return Err(Exc::Index);
            };
            let mut cont = &mut flags.flags;
            for k in parent {
                match cont.get_mut(k) {
                    Some(entry) => cont = &mut entry.nested,
                    None => return Ok(()),
                }
            }
            cont.remove(stem);
            Ok(())
        })
    }

    fn flags_set(
        &mut self,
        flags: &mut Flags,
        key: &[String],
        flag: u8,
        recursive: bool,
    ) -> Result<(), Exc> {
        self.framed(|_| {
            let Some((stem, parent)) = key.split_last() else {
                return Err(Exc::Index);
            };
            let mut cont = &mut flags.flags;
            for k in parent {
                cont = &mut cont.entry(k.clone()).or_default().nested;
            }
            let entry = cont.entry(stem.clone()).or_default();
            if recursive {
                entry.recursive_flags |= flag;
            } else {
                entry.flags |= flag;
            }
            Ok(())
        })
    }

    fn flags_is(&mut self, flags: &Flags, key: &[String], flag: u8) -> Result<bool, Exc> {
        self.framed(|_| {
            let Some((stem, parent)) = key.split_last() else {
                return Ok(false); // document root has no flags
            };
            let mut cont = &flags.flags;
            for k in parent {
                let Some(inner) = cont.get(k) else {
                    return Ok(false);
                };
                if inner.recursive_flags & flag != 0 {
                    return Ok(true);
                }
                cont = &inner.nested;
            }
            Ok(cont
                .get(stem)
                .is_some_and(|entry| (entry.flags | entry.recursive_flags) & flag != 0))
        })
    }

    // --- NestedDict methods. ---

    fn get_or_create_nest<'d>(
        &mut self,
        dict: &'d mut Dict,
        key: &[String],
        access_lists: bool,
    ) -> Result<&'d mut Dict, Exc> {
        self.framed(move |_| {
            let mut cont = dict;
            for k in key {
                let i = match cont.index.get(k.as_str()) {
                    Some(&i) => i,
                    None => cont.insert_new(k.clone(), Node::Dict(Dict::default())),
                };
                let mut node = &mut cont.entries[i].1;
                if access_lists {
                    node = match node {
                        Node::List(items) => items.last_mut().ok_or(Exc::Index)?,
                        other => other,
                    };
                }
                match node {
                    Node::Dict(inner) => cont = inner,
                    _ => return Err(Exc::Key), // There is no nest behind this key
                }
            }
            Ok(cont)
        })
    }

    fn append_nest_to_list(&mut self, dict: &mut Dict, key: &[String]) -> Result<(), Exc> {
        self.framed(|p| {
            let Some((last_key, parent)) = key.split_last() else {
                return Err(Exc::Index);
            };
            let cont = p.get_or_create_nest(dict, parent, true)?;
            match cont.get_mut(last_key) {
                Some(Node::List(items)) => items.push(Node::Dict(Dict::default())),
                // An object other than list found behind this key
                Some(_) => return Err(Exc::Key),
                None => {
                    cont.insert_new(
                        last_key.clone(),
                        Node::List(vec![Node::Dict(Dict::default())]),
                    );
                }
            }
            Ok(())
        })
    }

    // --- Module-level functions of _parser.py. ---

    fn skip_chars(&mut self, pos: usize, chars: fn(char) -> bool) -> Result<usize, Exc> {
        self.framed(|p| {
            let mut pos = pos;
            while p.char_at(pos).is_some_and(chars) {
                pos += 1;
            }
            Ok(pos)
        })
    }

    fn skip_until(
        &mut self,
        pos: usize,
        expect: &str,
        error_on: fn(char) -> bool,
        error_on_eof: bool,
    ) -> Result<usize, Exc> {
        self.framed(|p| {
            let len = p.src.len();
            let expect_len = expect.chars().count();
            let found = (pos..=len.saturating_sub(expect_len)).find(|&i| p.starts_with(i, expect));
            let new_pos = if let Some(i) = found {
                i
            } else if error_on_eof {
                return Err(p.decode_error(&format!("Expected {}", str_repr(expect)), len));
            } else {
                len
            };
            if let Some(bad) = (pos..new_pos).find(|&i| error_on(p.src[i])) {
                let msg = format!(
                    "Found invalid character {}",
                    str_repr(&p.src[bad].to_string())
                );
                return Err(p.decode_error(&msg, bad));
            }
            Ok(new_pos)
        })
    }

    fn skip_comment(&mut self, pos: usize) -> Result<usize, Exc> {
        self.framed(|p| {
            if p.char_at(pos) == Some('#') {
                return p.skip_until(pos + 1, "\n", illegal_basic_str, false);
            }
            Ok(pos)
        })
    }

    fn skip_comments_and_array_ws(&mut self, pos: usize) -> Result<usize, Exc> {
        self.framed(|p| {
            let mut pos = pos;
            loop {
                let pos_before_skip = pos;
                pos = p.skip_chars(pos, toml_ws_and_newline)?;
                pos = p.skip_comment(pos)?;
                if pos == pos_before_skip {
                    return Ok(pos);
                }
            }
        })
    }

    fn create_dict_rule(
        &mut self,
        pos: usize,
        out: &mut Output,
    ) -> Result<(usize, Vec<String>), Exc> {
        self.framed(|p| {
            let pos = pos + 1; // Skip "["
            let pos = p.skip_chars(pos, toml_ws)?;
            let (pos, key) = p.parse_key(pos)?;

            if p.flags_is(&out.flags, &key, EXPLICIT_NEST)?
                || p.flags_is(&out.flags, &key, FROZEN)?
            {
                return Err(
                    p.decode_error(&format!("Cannot declare {} twice", key_repr(&key)), pos)
                );
            }
            p.flags_set(&mut out.flags, &key, EXPLICIT_NEST, false)?;
            match p.get_or_create_nest(&mut out.data, &key, true) {
                Ok(_) => {}
                Err(Exc::Key) => return Err(p.decode_error("Cannot overwrite a value", pos)),
                Err(e) => return Err(e),
            }

            if !p.starts_with(pos, "]") {
                return Err(p.decode_error("Expected ']' at the end of a table declaration", pos));
            }
            Ok((pos + 1, key))
        })
    }

    fn create_list_rule(
        &mut self,
        pos: usize,
        out: &mut Output,
    ) -> Result<(usize, Vec<String>), Exc> {
        self.framed(|p| {
            let pos = pos + 2; // Skip "[["
            let pos = p.skip_chars(pos, toml_ws)?;
            let (pos, key) = p.parse_key(pos)?;

            if p.flags_is(&out.flags, &key, FROZEN)? {
                let msg = format!("Cannot mutate immutable namespace {}", key_repr(&key));
                return Err(p.decode_error(&msg, pos));
            }
            // Free the namespace now that it points to another empty list
            // item...
            p.unset_all(&mut out.flags, &key)?;
            // ...but this key precisely is still prohibited from table
            // declaration
            p.flags_set(&mut out.flags, &key, EXPLICIT_NEST, false)?;
            match p.append_nest_to_list(&mut out.data, &key) {
                Ok(()) => {}
                Err(Exc::Key) => return Err(p.decode_error("Cannot overwrite a value", pos)),
                Err(e) => return Err(e),
            }

            if !p.starts_with(pos, "]]") {
                return Err(p.decode_error("Expected ']]' at the end of an array declaration", pos));
            }
            Ok((pos + 2, key))
        })
    }

    fn key_value_rule(
        &mut self,
        pos: usize,
        out: &mut Output,
        header: &[String],
    ) -> Result<usize, Exc> {
        self.framed(|p| {
            let (pos, key, value) = p.parse_key_value_pair(pos)?;
            let (key_stem, key_parent) = key.split_last().ok_or(Exc::Index)?;
            let abs_key_parent = [header, key_parent].concat();

            // `relative_path_cont_keys` is a generator expression: creating it
            // and each `next()` run its frame.
            p.framed(|_| Ok(()))?;
            for i in 1..key.len() {
                p.framed(|_| Ok(()))?;
                let cont_key = [header, &key[..i]].concat();
                // Check that dotted key syntax does not redefine an existing
                // table
                if p.flags_is(&out.flags, &cont_key, EXPLICIT_NEST)? {
                    let msg = format!("Cannot redefine namespace {}", key_repr(&cont_key));
                    return Err(p.decode_error(&msg, pos));
                }
                // Containers in the relative path can't be opened with the
                // table syntax or dotted key/value syntax in following table
                // sections.
                p.add_pending(&mut out.flags, cont_key, EXPLICIT_NEST)?;
            }
            p.framed(|_| Ok(()))?; // the generator's final next()

            if p.flags_is(&out.flags, &abs_key_parent, FROZEN)? {
                let msg = format!(
                    "Cannot mutate immutable namespace {}",
                    key_repr(&abs_key_parent)
                );
                return Err(p.decode_error(&msg, pos));
            }

            let nest = match p.get_or_create_nest(&mut out.data, &abs_key_parent, true) {
                Ok(nest) => nest,
                Err(Exc::Key) => return Err(p.decode_error("Cannot overwrite a value", pos)),
                Err(e) => return Err(e),
            };
            if nest.contains(key_stem) {
                return Err(p.decode_error("Cannot overwrite a value", pos));
            }
            // Mark inline table and array namespaces recursively immutable
            if value.is_container() {
                p.flags_set(&mut out.flags, &[header, &key].concat(), FROZEN, true)?;
            }
            nest.insert_new(key_stem.clone(), value);
            Ok(pos)
        })
    }

    fn parse_key_value_pair(&mut self, pos: usize) -> Result<(usize, Vec<String>, Node), Exc> {
        self.framed(|p| {
            let (pos, key) = p.parse_key(pos)?;
            if p.char_at(pos) != Some('=') {
                return Err(p.decode_error("Expected '=' after a key in a key/value pair", pos));
            }
            let pos = p.skip_chars(pos + 1, toml_ws)?;
            let (pos, value) = p.parse_value(pos)?;
            Ok((pos, key, value))
        })
    }

    fn parse_key(&mut self, pos: usize) -> Result<(usize, Vec<String>), Exc> {
        self.framed(|p| {
            let (pos, key_part) = p.parse_key_part(pos)?;
            let mut key = vec![key_part];
            let mut pos = p.skip_chars(pos, toml_ws)?;
            loop {
                if p.char_at(pos) != Some('.') {
                    return Ok((pos, key));
                }
                pos = p.skip_chars(pos + 1, toml_ws)?;
                let (next, key_part) = p.parse_key_part(pos)?;
                key.push(key_part);
                if key.len() > MAX_KEY_PARTS {
                    return Err(Exc::Recursion(format!(
                        "TOML key has more than the allowed {MAX_KEY_PARTS} parts"
                    )));
                }
                pos = p.skip_chars(next, toml_ws)?;
            }
        })
    }

    fn parse_key_part(&mut self, pos: usize) -> Result<(usize, String), Exc> {
        self.framed(|p| match p.char_at(pos) {
            Some(c) if bare_key_char(c) => {
                let end = p.skip_chars(pos, bare_key_char)?;
                Ok((end, p.slice(pos, end)))
            }
            Some('\'') => p.parse_literal_str(pos),
            Some('"') => p.parse_one_line_basic_str(pos),
            _ => Err(p.decode_error("Invalid initial character for a key part", pos)),
        })
    }

    fn parse_one_line_basic_str(&mut self, pos: usize) -> Result<(usize, String), Exc> {
        self.framed(|p| p.parse_basic_str(pos + 1, false))
    }

    fn parse_array(&mut self, pos: usize) -> Result<(usize, Node), Exc> {
        self.framed(|p| {
            let mut array = Vec::new();
            let mut pos = p.skip_comments_and_array_ws(pos + 1)?;
            if p.starts_with(pos, "]") {
                return Ok((pos + 1, Node::List(array)));
            }
            loop {
                let (next, value) = p.parse_value(pos)?;
                array.push(value);
                pos = p.skip_comments_and_array_ws(next)?;

                match p.char_at(pos) {
                    Some(']') => return Ok((pos + 1, Node::List(array))),
                    Some(',') => {}
                    _ => return Err(p.decode_error("Unclosed array", pos)),
                }
                pos = p.skip_comments_and_array_ws(pos + 1)?;
                if p.starts_with(pos, "]") {
                    return Ok((pos + 1, Node::List(array)));
                }
            }
        })
    }

    fn parse_inline_table(&mut self, pos: usize) -> Result<(usize, Node), Exc> {
        self.framed(|p| {
            p.instantiate()?; // NestedDict()
            let mut nested_dict = Dict::default();
            p.instantiate()?; // Flags()
            let mut flags = Flags::default();

            let mut pos = p.skip_chars(pos + 1, toml_ws)?;
            if p.starts_with(pos, "}") {
                return Ok((pos + 1, Node::Dict(nested_dict)));
            }
            loop {
                let (value_end, key, value) = p.parse_key_value_pair(pos)?;
                let (key_stem, key_parent) = key.split_last().ok_or(Exc::Index)?;
                if p.flags_is(&flags, &key, FROZEN)? {
                    let msg = format!("Cannot mutate immutable namespace {}", key_repr(&key));
                    return Err(p.decode_error(&msg, value_end));
                }
                let nest = match p.get_or_create_nest(&mut nested_dict, key_parent, false) {
                    Ok(nest) => nest,
                    Err(Exc::Key) => {
                        return Err(p.decode_error("Cannot overwrite a value", value_end));
                    }
                    Err(e) => return Err(e),
                };
                if nest.contains(key_stem) {
                    let msg = format!("Duplicate inline table key {}", str_repr(key_stem));
                    return Err(p.decode_error(&msg, value_end));
                }
                let is_container = value.is_container();
                nest.insert_new(key_stem.clone(), value);
                pos = p.skip_chars(value_end, toml_ws)?;
                match p.char_at(pos) {
                    Some('}') => return Ok((pos + 1, Node::Dict(nested_dict))),
                    Some(',') => {}
                    _ => return Err(p.decode_error("Unclosed inline table", pos)),
                }
                if is_container {
                    p.flags_set(&mut flags, &key, FROZEN, true)?;
                }
                pos = p.skip_chars(pos + 1, toml_ws)?;
            }
        })
    }

    /// Returns the new position and the escaped character (`None` for a
    /// line-ending backslash).
    fn parse_basic_str_escape(
        &mut self,
        pos: usize,
        multiline: bool,
    ) -> Result<(usize, Option<char>), Exc> {
        self.framed(|p| {
            let escape_id = p.slice(pos, (pos + 2).min(p.src.len()));
            let mut pos = pos + 2;
            if multiline && matches!(escape_id.as_str(), "\\ " | "\\\t" | "\\\n") {
                // Skip whitespace until next non-whitespace character or end of
                // the doc. Error if non-whitespace is found before newline.
                if escape_id != "\\\n" {
                    pos = p.skip_chars(pos, toml_ws)?;
                    match p.char_at(pos) {
                        None => return Ok((pos, None)),
                        Some('\n') => {}
                        Some(_) => return Err(p.decode_error("Unescaped '\\' in a string", pos)),
                    }
                    pos += 1;
                }
                pos = p.skip_chars(pos, toml_ws_and_newline)?;
                return Ok((pos, None));
            }
            let replacement = match escape_id.as_str() {
                "\\u" => return p.parse_hex_char(pos, 4).map(|(pos, c)| (pos, Some(c))),
                "\\U" => return p.parse_hex_char(pos, 8).map(|(pos, c)| (pos, Some(c))),
                "\\b" => '\u{8}',
                "\\t" => '\t',
                "\\n" => '\n',
                "\\f" => '\u{c}',
                "\\r" => '\r',
                "\\\"" => '"',
                "\\\\" => '\\',
                _ => return Err(p.decode_error("Unescaped '\\' in a string", pos)),
            };
            Ok((pos, Some(replacement)))
        })
    }

    fn parse_basic_str_escape_multiline(
        &mut self,
        pos: usize,
    ) -> Result<(usize, Option<char>), Exc> {
        self.framed(|p| p.parse_basic_str_escape(pos, true))
    }

    fn parse_hex_char(&mut self, pos: usize, hex_len: usize) -> Result<(usize, char), Exc> {
        self.framed(|p| {
            let hex_str = &p.src[pos.min(p.src.len())..(pos + hex_len).min(p.src.len())];
            if hex_str.len() != hex_len || !hex_str.iter().all(char::is_ascii_hexdigit) {
                return Err(p.decode_error("Invalid hex value", pos));
            }
            let pos = pos + hex_len;
            let hex_int = hex_str.iter().fold(0u32, |acc, c| {
                (acc << 4) | c.to_digit(16).unwrap_or_default()
            });
            if !p.is_unicode_scalar_value(hex_int)? {
                return Err(p.decode_error("Escaped character is not a Unicode scalar value", pos));
            }
            let c = char::from_u32(hex_int).ok_or(Exc::Index)?;
            Ok((pos, c))
        })
    }

    fn parse_literal_str(&mut self, pos: usize) -> Result<(usize, String), Exc> {
        self.framed(|p| {
            let start_pos = pos + 1; // Skip starting apostrophe
            let pos = p.skip_until(start_pos, "'", illegal_basic_str, true)?;
            Ok((pos + 1, p.slice(start_pos, pos))) // Skip ending apostrophe
        })
    }

    fn parse_multiline_str(&mut self, pos: usize, literal: bool) -> Result<(usize, String), Exc> {
        self.framed(|p| {
            let mut pos = pos + 3;
            if p.starts_with(pos, "\n") {
                pos += 1;
            }

            let (delim, mut result);
            if literal {
                delim = '\'';
                let end_pos = p.skip_until(pos, "'''", illegal_multiline_basic_str, true)?;
                result = p.slice(pos, end_pos);
                pos = end_pos + 3;
            } else {
                delim = '"';
                (pos, result) = p.parse_basic_str(pos, true)?;
            }

            // Add at maximum two extra apostrophes/quotes if the end sequence
            // is 4 or 5 chars long instead of just 3.
            for _ in 0..2 {
                if p.char_at(pos) != Some(delim) {
                    break;
                }
                pos += 1;
                result.push(delim);
            }
            Ok((pos, result))
        })
    }

    fn parse_basic_str(&mut self, pos: usize, multiline: bool) -> Result<(usize, String), Exc> {
        self.framed(|p| {
            let error_on = if multiline {
                illegal_multiline_basic_str
            } else {
                illegal_basic_str
            };
            let mut result = String::new();
            let mut pos = pos;
            let mut start_pos = pos;
            loop {
                let Some(c) = p.char_at(pos) else {
                    return Err(p.decode_error("Unterminated string", pos));
                };
                if c == '"' {
                    if !multiline {
                        result.extend(&p.src[start_pos..pos]);
                        return Ok((pos + 1, result));
                    }
                    if p.starts_with(pos, "\"\"\"") {
                        result.extend(&p.src[start_pos..pos]);
                        return Ok((pos + 3, result));
                    }
                    pos += 1;
                    continue;
                }
                if c == '\\' {
                    result.extend(&p.src[start_pos..pos]);
                    let (next, parsed_escape) = if multiline {
                        p.parse_basic_str_escape_multiline(pos)?
                    } else {
                        p.parse_basic_str_escape(pos, false)?
                    };
                    result.extend(parsed_escape);
                    pos = next;
                    start_pos = pos;
                    continue;
                }
                if error_on(c) {
                    let msg = format!("Illegal character {}", str_repr(&c.to_string()));
                    return Err(p.decode_error(&msg, pos));
                }
                pos += 1;
            }
        })
    }

    fn parse_value(&mut self, pos: usize) -> Result<(usize, Node), Exc> {
        self.framed(|p| {
            let leaf = |pos, value| Ok((pos, Node::Leaf(value)));
            let string = |r: Result<(usize, String), Exc>| {
                r.map(|(pos, s)| (pos, Node::Leaf(Value::String(s))))
            };
            match p.char_at(pos) {
                // Basic strings
                Some('"') if p.starts_with(pos, "\"\"\"") => {
                    return string(p.parse_multiline_str(pos, false));
                }
                Some('"') => return string(p.parse_one_line_basic_str(pos)),
                // Literal strings
                Some('\'') if p.starts_with(pos, "'''") => {
                    return string(p.parse_multiline_str(pos, true));
                }
                Some('\'') => return string(p.parse_literal_str(pos)),
                // Booleans
                Some('t') if p.starts_with(pos, "true") => {
                    return leaf(pos + 4, Value::Boolean(true));
                }
                Some('f') if p.starts_with(pos, "false") => {
                    return leaf(pos + 5, Value::Boolean(false));
                }
                // Arrays
                Some('[') => return p.parse_array(pos),
                // Inline tables
                Some('{') => return p.parse_inline_table(pos),
                _ => {}
            }

            // Dates and times
            if let Some(m) = match_datetime(p.src, pos) {
                return match p.match_to_datetime(&m) {
                    Ok(value) => leaf(m.end, value),
                    Err(Exc::Value(_)) => Err(p.decode_error("Invalid date or datetime", pos)),
                    Err(e) => Err(e),
                };
            }
            if let Some(m) = match_time(p.src, pos) {
                let value = p.match_to_localtime(&m)?;
                return leaf(m.end, value);
            }

            // Integers and "normal" floats. The regex will greedily match any
            // type starting with a decimal char, so needs to be located after
            // handling of dates and times.
            if let Some(m) = match_number(p.src, pos) {
                let value = p.match_to_number(&m)?;
                return leaf(m.end, value);
            }

            // Special floats
            let first_three = p.slice(pos.min(p.src.len()), (pos + 3).min(p.src.len()));
            if first_three == "inf" || first_three == "nan" {
                return leaf(pos + 3, Value::Float(special_float(&first_three)));
            }
            let first_four = p.slice(pos.min(p.src.len()), (pos + 4).min(p.src.len()));
            if matches!(first_four.as_str(), "-inf" | "+inf" | "-nan" | "+nan") {
                return leaf(pos + 4, Value::Float(special_float(&first_four)));
            }

            Err(p.decode_error("Invalid value", pos))
        })
    }

    fn is_unicode_scalar_value(&mut self, codepoint: u32) -> Result<bool, Exc> {
        self.framed(|_| Ok(codepoint <= 55295 || (57344..=1_114_111).contains(&codepoint)))
    }

    // --- _re.py. ---

    fn match_to_datetime(&mut self, m: &DateTimeMatch) -> Result<Value, Exc> {
        self.framed(|p| {
            let Some(time) = m.time else {
                return Ok(Value::Date(new_date(m.year, m.month, m.day)?));
            };
            let offset_minutes = match m.offset {
                Some(Offset::Numeric { sign, hour, minute }) => {
                    if p.tz_cache.insert((sign, hour, minute)) {
                        p.framed(|_| Ok(()))?; // cached_tz, on a cache miss
                    }
                    let minutes = i16::from(hour) * 60 + i16::from(minute);
                    Some(if sign == '-' { -minutes } else { minutes })
                }
                Some(Offset::Zulu) => Some(0),
                None => None, // local date-time
            };
            Ok(Value::DateTime(DateTime {
                date: new_date(m.year, m.month, m.day)?,
                time: time.time,
                offset_minutes,
            }))
        })
    }

    fn match_to_localtime(&mut self, m: &TimeMatch) -> Result<Value, Exc> {
        self.framed(|_| Ok(Value::Time(m.time)))
    }

    fn match_to_number(&mut self, m: &NumberMatch) -> Result<Value, Exc> {
        self.framed(|p| {
            let text = &p.src[m.start..m.end];
            if m.float {
                let digits: String = text.iter().filter(|&&c| c != '_').collect();
                return digits.parse().map(Value::Float).map_err(|_| {
                    Exc::Value(format!("could not convert string to float: {digits:?}"))
                });
            }
            if m.prefixed {
                return Ok(Value::Integer(Int::from_prefixed_literal(text)));
            }
            Int::from_decimal_literal(text).map(Value::Integer)
        })
    }
}

fn special_float(text: &str) -> f64 {
    let magnitude = if text.ends_with("inf") {
        f64::INFINITY
    } else {
        f64::NAN
    };
    if text.starts_with('-') {
        -magnitude
    } else {
        magnitude
    }
}

/// `datetime.date(year, month, day)`'s validation.
fn new_date(year: u16, month: u8, day: u8) -> Result<Date, Exc> {
    if year == 0 {
        return Err(Exc::Value(format!("year {year} is out of range")));
    }
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day > days_in_month {
        return Err(Exc::Value("day is out of range for month".into()));
    }
    Ok(Date { year, month, day })
}

// --- Hand-written equivalents of _re.py's regexes (`[0-9]` is ASCII). ---

fn ascii_digit(src: &[char], pos: usize) -> Option<u8> {
    let c = *src.get(pos)?;
    c.is_ascii_digit()
        .then(|| u8::try_from(c.to_digit(10).unwrap_or_default()).unwrap_or_default())
}

/// Two digits, the first in `first` and the number at most `max`.
fn two_digits(
    src: &[char],
    pos: usize,
    first: std::ops::RangeInclusive<u8>,
    max: u8,
) -> Option<u8> {
    let tens = ascii_digit(src, pos)?;
    let ones = ascii_digit(src, pos + 1)?;
    let value = tens * 10 + ones;
    (first.contains(&tens) && value <= max).then_some(value)
}

#[derive(Clone, Copy)]
struct TimeMatch {
    end: usize,
    time: Time,
}

#[derive(Clone, Copy)]
enum Offset {
    Zulu,
    Numeric { sign: char, hour: u8, minute: u8 },
}

struct DateTimeMatch {
    end: usize,
    year: u16,
    month: u8,
    day: u8,
    time: Option<TimeMatch>,
    offset: Option<Offset>,
}

struct NumberMatch {
    start: usize,
    end: usize,
    /// The `floatpart` group matched something.
    float: bool,
    /// A `0x`/`0o`/`0b` literal.
    prefixed: bool,
}

/// `_TIME_RE_STR` (`RE_LOCALTIME.match(src, pos)`):
/// `([01][0-9]|2[0-3]):([0-5][0-9]):([0-5][0-9])(?:\.([0-9]{1,6})[0-9]*)?`.
fn match_time(src: &[char], pos: usize) -> Option<TimeMatch> {
    let hour = two_digits(src, pos, 0..=2, 23)?;
    if src.get(pos + 2) != Some(&':') {
        return None;
    }
    let minute = two_digits(src, pos + 3, 0..=5, 59)?;
    if src.get(pos + 5) != Some(&':') {
        return None;
    }
    let second = two_digits(src, pos + 6, 0..=5, 59)?;
    let mut end = pos + 8;
    let mut microsecond = 0;
    if src.get(end) == Some(&'.') && ascii_digit(src, end + 1).is_some() {
        end += 1;
        // micros_str.ljust(6, "0"), then the remaining digits are dropped.
        let mut scale = 100_000;
        while let Some(d) = ascii_digit(src, end) {
            if scale > 0 {
                microsecond += u32::from(d) * scale;
                scale /= 10;
            }
            end += 1;
        }
    }
    Some(TimeMatch {
        end,
        time: Time {
            hour,
            minute,
            second,
            microsecond,
        },
    })
}

/// `RE_DATETIME.match(src, pos)`.
fn match_datetime(src: &[char], pos: usize) -> Option<DateTimeMatch> {
    let mut year: u16 = 0;
    for i in 0..4 {
        year = year * 10 + u16::from(ascii_digit(src, pos + i)?);
    }
    if src.get(pos + 4) != Some(&'-') {
        return None;
    }
    let month = two_digits(src, pos + 5, 0..=1, 12).filter(|&m| m >= 1)?;
    if src.get(pos + 7) != Some(&'-') {
        return None;
    }
    let day = two_digits(src, pos + 8, 0..=3, 31).filter(|&d| d >= 1)?;
    let mut m = DateTimeMatch {
        end: pos + 10,
        year,
        month,
        day,
        time: None,
        offset: None,
    };
    if !matches!(src.get(m.end), Some('T' | 't' | ' ')) {
        return Some(m);
    }
    let Some(time) = match_time(src, m.end + 1) else {
        return Some(m);
    };
    m.time = Some(time);
    m.end = time.end;
    match src.get(m.end) {
        Some('Z' | 'z') => {
            m.offset = Some(Offset::Zulu);
            m.end += 1;
        }
        Some(&sign @ ('+' | '-')) => {
            let hour = two_digits(src, m.end + 1, 0..=2, 23);
            let colon = src.get(m.end + 3) == Some(&':');
            let minute = two_digits(src, m.end + 4, 0..=5, 59);
            if let (Some(hour), true, Some(minute)) = (hour, colon, minute) {
                m.offset = Some(Offset::Numeric { sign, hour, minute });
                m.end += 6;
            }
        }
        _ => {}
    }
    Some(m)
}

/// `(?:_?[0-9])*` and its hex/octal/binary variants: the end of the run.
fn digits_with_underscores(src: &[char], mut pos: usize, digit: fn(char) -> bool) -> usize {
    loop {
        match src.get(pos) {
            Some(&c) if digit(c) => pos += 1,
            Some('_') if src.get(pos + 1).is_some_and(|&c| digit(c)) => pos += 2,
            _ => return pos,
        }
    }
}

/// `RE_NUMBER.match(src, pos)`.
fn match_number(src: &[char], pos: usize) -> Option<NumberMatch> {
    // 0x…, 0b…, 0o…
    if src.get(pos) == Some(&'0') {
        let digit: Option<fn(char) -> bool> = match src.get(pos + 1) {
            Some('x') => Some(|c| c.is_ascii_hexdigit()),
            Some('b') => Some(|c| c == '0' || c == '1'),
            Some('o') => Some(|c| ('0'..='7').contains(&c)),
            _ => None,
        };
        if let Some(digit) = digit
            && src.get(pos + 2).is_some_and(|&c| digit(c))
        {
            return Some(NumberMatch {
                start: pos,
                end: digits_with_underscores(src, pos + 3, digit),
                float: false,
                prefixed: true,
            });
        }
    }
    let is_digit = |c: char| c.is_ascii_digit();
    // [+-]?(?:0|[1-9](?:_?[0-9])*)
    let mut end = pos;
    if matches!(src.get(end), Some('+' | '-')) {
        end += 1;
    }
    match src.get(end) {
        Some('0') => end += 1,
        Some('1'..='9') => end = digits_with_underscores(src, end + 1, is_digit),
        _ => return None,
    }
    let float_start = end;
    // (?:\.[0-9](?:_?[0-9])*)?
    if src.get(end) == Some(&'.') && src.get(end + 1).is_some_and(|&c| is_digit(c)) {
        end = digits_with_underscores(src, end + 2, is_digit);
    }
    // (?:[eE][+-]?[0-9](?:_?[0-9])*)?
    if matches!(src.get(end), Some('e' | 'E')) {
        let mut exp = end + 1;
        if matches!(src.get(exp), Some('+' | '-')) {
            exp += 1;
        }
        if src.get(exp).is_some_and(|&c| is_digit(c)) {
            end = digits_with_underscores(src, exp + 1, is_digit);
        }
    }
    Some(NumberMatch {
        start: pos,
        end,
        float: end > float_start,
        prefixed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // BEGIN GENERATED CASES
    // Recorded from tomllib.loads + repr()/str() on CPython 3.14.8 by a scratch
    // generator.
    #[rustfmt::skip]
    const CASES: &[(&str, Expect)] = &[
        ("", Expect::Ok(Some("{}"), &[])),
        ("\n\n", Expect::Ok(Some("{}"), &[])),
        ("# only a comment\n", Expect::Ok(Some("{}"), &[])),
        ("a = 1", Expect::Ok(Some("{'a': 1}"), &[("a", Some("1"))])),
        ("a=1\nb=2\n", Expect::Ok(Some("{'a': 1, 'b': 2}"), &[("a", Some("1")), ("b", Some("2"))])),
        ("a = 'x' # c\n", Expect::Ok(Some("{'a': 'x'}"), &[("a", Some("x"))])),
        ("a = \"plain\"", Expect::Ok(Some("{'a': 'plain'}"), &[("a", Some("plain"))])),
        ("a = 'lit'", Expect::Ok(Some("{'a': 'lit'}"), &[("a", Some("lit"))])),
        ("a = \"\"", Expect::Ok(Some("{'a': ''}"), &[("a", Some(""))])),
        ("a = ''", Expect::Ok(Some("{'a': ''}"), &[("a", Some(""))])),
        ("a = \"\x5cb\x5ct\x5cn\x5cf\x5cr\x5c\"\x5c\x5c\"", Expect::Ok(Some("{'a': '\x5cx08\x5ct\x5cn\x5cx0c\x5cr\"\x5c\x5c'}"), &[("a", Some("\u{8}\t\n\u{c}\r\"\x5c"))])),
        ("a = \"\x5cu00e9\x5cu0041\"", Expect::Ok(Some("{'a': '\u{e9}A'}"), &[("a", Some("\u{e9}A"))])),
        ("a = \"\x5cU0001F600\"", Expect::Ok(Some("{'a': '\u{1f600}'}"), &[("a", Some("\u{1f600}"))])),
        ("a = \"\x5cuD800\"", Expect::Decode),
        ("a = \"\x5cU00110000\"", Expect::Decode),
        ("a = \"\x5cu12\"", Expect::Decode),
        ("a = \"\x5cu12G4\"", Expect::Decode),
        ("a = \"\x5cu00", Expect::Decode),
        ("a = \"\x5ce\"", Expect::Decode),
        ("a = \"\x5cx41\"", Expect::Decode),
        ("a = \"\x5c \"", Expect::Decode),
        ("a = \"\x5c\"", Expect::Decode),
        ("a = \"tab\there\"", Expect::Ok(Some("{'a': 'tab\x5cthere'}"), &[("a", Some("tab\there"))])),
        ("a = \"nl\nx\"", Expect::Decode),
        ("a = \"del\u{7f}\"", Expect::Decode),
        ("a = \"ctl\u{1}\"", Expect::Decode),
        ("a = 'ctl\u{1}'", Expect::Decode),
        ("a = 'tab\tok'", Expect::Ok(Some("{'a': 'tab\x5ctok'}"), &[("a", Some("tab\tok"))])),
        ("a = \"unterminated", Expect::Decode),
        ("a = 'unterminated", Expect::Decode),
        ("a = \"\u{e9} \u{2603} \u{1f600}\"", Expect::Ok(Some("{'a': '\u{e9} \u{2603} \u{1f600}'}"), &[("a", Some("\u{e9} \u{2603} \u{1f600}"))])),
        ("a = \"\u{2028}\u{85}\u{a0}\"", Expect::Ok(Some("{'a': '\x5cu2028\x5cx85\x5cxa0'}"), &[("a", Some("\u{2028}\u{85}\u{a0}"))])),
        ("a = \"\"\"\nline1\nline2\"\"\"", Expect::Ok(Some("{'a': 'line1\x5cnline2'}"), &[("a", Some("line1\nline2"))])),
        ("a = \"\"\"x\"\"\"\"", Expect::Ok(Some("{'a': 'x\"'}"), &[("a", Some("x\""))])),
        ("a = \"\"\"x\"\"\"\"\"", Expect::Ok(Some("{'a': 'x\"\"'}"), &[("a", Some("x\"\""))])),
        ("a = \"\"\"x\"\"\"\"\"\"", Expect::Decode),
        ("a = \"\"\"a\x5c\n   b\"\"\"", Expect::Ok(Some("{'a': 'ab'}"), &[("a", Some("ab"))])),
        ("a = \"\"\"a\x5c   \n\n  b\"\"\"", Expect::Ok(Some("{'a': 'ab'}"), &[("a", Some("ab"))])),
        ("a = \"\"\"a\x5c x\"\"\"", Expect::Decode),
        ("a = \"\"\"a\x5c\t\t\n b\"\"\"", Expect::Ok(Some("{'a': 'ab'}"), &[("a", Some("ab"))])),
        ("a = \"\"\"a\x5c  ", Expect::Decode),
        ("a = \"\"\"a\x5c\n", Expect::Decode),
        ("a = \"\"\"q\"q\"\"q\"\"\"", Expect::Ok(Some("{'a': 'q\"q\"\"q'}"), &[("a", Some("q\"q\"\"q"))])),
        ("a = \"\"\"tab\tnl\n\"\"\"", Expect::Ok(Some("{'a': 'tab\x5ctnl\x5cn'}"), &[("a", Some("tab\tnl\n"))])),
        ("a = \"\"\"ctl\u{1}\"\"\"", Expect::Decode),
        ("a = \"\"\"\x5cu0041\"\"\"", Expect::Ok(Some("{'a': 'A'}"), &[("a", Some("A"))])),
        ("a = \"\"\"\x5ce\"\"\"", Expect::Decode),
        ("a = \"\"\"\r\nx\r\ny\"\"\"", Expect::Ok(Some("{'a': 'x\x5cny'}"), &[("a", Some("x\ny"))])),
        ("a = \"\"\"\rx\"\"\"", Expect::Decode),
        ("a = '''\nraw \x5cn'''", Expect::Ok(Some("{'a': 'raw \x5c\x5cn'}"), &[("a", Some("raw \x5cn"))])),
        ("a = '''x''''", Expect::Ok(Some("{'a': \"x'\"}"), &[("a", Some("x'"))])),
        ("a = '''x'''''", Expect::Ok(Some("{'a': \"x''\"}"), &[("a", Some("x''"))])),
        ("a = '''x''''''", Expect::Decode),
        ("a = '''a\nb\u{1}'''", Expect::Decode),
        ("a = '''unterminated", Expect::Decode),
        ("a = '''\ttab'''", Expect::Ok(Some("{'a': '\x5cttab'}"), &[("a", Some("\ttab"))])),
        ("a = '''x\r\ny'''", Expect::Ok(Some("{'a': 'x\x5cny'}"), &[("a", Some("x\ny"))])),
        ("a = true", Expect::Ok(Some("{'a': True}"), &[("a", Some("True"))])),
        ("a = false", Expect::Ok(Some("{'a': False}"), &[("a", Some("False"))])),
        ("a = True", Expect::Decode),
        ("a = truex", Expect::Decode),
        ("a = tru", Expect::Decode),
        ("a = 0", Expect::Ok(Some("{'a': 0}"), &[("a", Some("0"))])),
        ("a = +0", Expect::Ok(Some("{'a': 0}"), &[("a", Some("0"))])),
        ("a = -0", Expect::Ok(Some("{'a': 0}"), &[("a", Some("0"))])),
        ("a = 1_000", Expect::Ok(Some("{'a': 1000}"), &[("a", Some("1000"))])),
        ("a = -17", Expect::Ok(Some("{'a': -17}"), &[("a", Some("-17"))])),
        ("a = +99", Expect::Ok(Some("{'a': 99}"), &[("a", Some("99"))])),
        ("a = 0xDEADBEEF", Expect::Ok(Some("{'a': 3735928559}"), &[("a", Some("3735928559"))])),
        ("a = 0xdead_beef", Expect::Ok(Some("{'a': 3735928559}"), &[("a", Some("3735928559"))])),
        ("a = 0o755", Expect::Ok(Some("{'a': 493}"), &[("a", Some("493"))])),
        ("a = 0b1101", Expect::Ok(Some("{'a': 13}"), &[("a", Some("13"))])),
        ("a = 0x00ff", Expect::Ok(Some("{'a': 255}"), &[("a", Some("255"))])),
        ("a = 0b0", Expect::Ok(Some("{'a': 0}"), &[("a", Some("0"))])),
        ("a = 0x0", Expect::Ok(Some("{'a': 0}"), &[("a", Some("0"))])),
        ("a = 01", Expect::Decode),
        ("a = 1__0", Expect::Decode),
        ("a = _1", Expect::Decode),
        ("a = 1_", Expect::Decode),
        ("a = 0x", Expect::Decode),
        ("a = 0X1F", Expect::Decode),
        ("a = +0x1", Expect::Decode),
        ("a = -0b1", Expect::Decode),
        ("a = 0x_1", Expect::Decode),
        ("a = 0xg", Expect::Decode),
        ("a = 9223372036854775808", Expect::Ok(Some("{'a': 9223372036854775808}"), &[("a", Some("9223372036854775808"))])),
        ("a = -9223372036854775809", Expect::Ok(Some("{'a': -9223372036854775809}"), &[("a", Some("-9223372036854775809"))])),
        ("a = 0xffffffffffffffffffffffffffffffff", Expect::Ok(Some("{'a': 340282366920938463463374607431768211455}"), &[("a", Some("340282366920938463463374607431768211455"))])),
        ("a = 0o7777777777777777777777777", Expect::Ok(Some("{'a': 37778931862957161709567}"), &[("a", Some("37778931862957161709567"))])),
        ("a = 123456789012345678901234567890", Expect::Ok(Some("{'a': 123456789012345678901234567890}"), &[("a", Some("123456789012345678901234567890"))])),
        ("a = 1.0", Expect::Ok(Some("{'a': 1.0}"), &[("a", Some("1.0"))])),
        ("a = -0.0", Expect::Ok(Some("{'a': -0.0}"), &[("a", Some("-0.0"))])),
        ("a = +0.0", Expect::Ok(Some("{'a': 0.0}"), &[("a", Some("0.0"))])),
        ("a = 3.14", Expect::Ok(Some("{'a': 3.14}"), &[("a", Some("3.14"))])),
        ("a = 1e10", Expect::Ok(Some("{'a': 10000000000.0}"), &[("a", Some("10000000000.0"))])),
        ("a = 1E-10", Expect::Ok(Some("{'a': 1e-10}"), &[("a", Some("1e-10"))])),
        ("a = 6.626e-34", Expect::Ok(Some("{'a': 6.626e-34}"), &[("a", Some("6.626e-34"))])),
        ("a = 1_000.5_5", Expect::Ok(Some("{'a': 1000.55}"), &[("a", Some("1000.55"))])),
        ("a = 5e+22", Expect::Ok(Some("{'a': 5e+22}"), &[("a", Some("5e+22"))])),
        ("a = 1e400", Expect::Ok(Some("{'a': inf}"), &[("a", Some("inf"))])),
        ("a = -1e400", Expect::Ok(Some("{'a': -inf}"), &[("a", Some("-inf"))])),
        ("a = 1e-400", Expect::Ok(Some("{'a': 0.0}"), &[("a", Some("0.0"))])),
        ("a = -1e-400", Expect::Ok(Some("{'a': -0.0}"), &[("a", Some("-0.0"))])),
        ("a = .5", Expect::Decode),
        ("a = 5.", Expect::Decode),
        ("a = 1.e5", Expect::Decode),
        ("a = 1e", Expect::Decode),
        ("a = 1e_5", Expect::Decode),
        ("a = 1.5_", Expect::Decode),
        ("a = inf", Expect::Ok(Some("{'a': inf}"), &[("a", Some("inf"))])),
        ("a = +inf", Expect::Ok(Some("{'a': inf}"), &[("a", Some("inf"))])),
        ("a = -inf", Expect::Ok(Some("{'a': -inf}"), &[("a", Some("-inf"))])),
        ("a = nan", Expect::Ok(Some("{'a': nan}"), &[("a", Some("nan"))])),
        ("a = +nan", Expect::Ok(Some("{'a': nan}"), &[("a", Some("nan"))])),
        ("a = -nan", Expect::Ok(Some("{'a': nan}"), &[("a", Some("nan"))])),
        ("a = infinity", Expect::Decode),
        ("a = Inf", Expect::Decode),
        ("a = NaN", Expect::Decode),
        ("a = 1000000000000000.25", Expect::Ok(Some("{'a': 1000000000000000.2}"), &[("a", Some("1000000000000000.2"))])),
        ("a = 0.1", Expect::Ok(Some("{'a': 0.1}"), &[("a", Some("0.1"))])),
        ("a = 1e16", Expect::Ok(Some("{'a': 1e+16}"), &[("a", Some("1e+16"))])),
        ("a = 1.5e-5", Expect::Ok(Some("{'a': 1.5e-05}"), &[("a", Some("1.5e-05"))])),
        ("a = 123456789.123456789", Expect::Ok(Some("{'a': 123456789.12345679}"), &[("a", Some("123456789.12345679"))])),
        ("a = 2.5e-324", Expect::Ok(Some("{'a': 5e-324}"), &[("a", Some("5e-324"))])),
        ("a = 4.9e-324", Expect::Ok(Some("{'a': 5e-324}"), &[("a", Some("5e-324"))])),
        ("a = 1.7976931348623157e308", Expect::Ok(Some("{'a': 1.7976931348623157e+308}"), &[("a", Some("1.7976931348623157e+308"))])),
        ("a = 1.7976931348623159e308", Expect::Ok(Some("{'a': inf}"), &[("a", Some("inf"))])),
        ("a = 0.30000000000000004", Expect::Ok(Some("{'a': 0.30000000000000004}"), &[("a", Some("0.30000000000000004"))])),
        ("a = 1e22", Expect::Ok(Some("{'a': 1e+22}"), &[("a", Some("1e+22"))])),
        ("a = 1e23", Expect::Ok(Some("{'a': 1e+23}"), &[("a", Some("1e+23"))])),
        ("a = 9007199254740993.0", Expect::Ok(Some("{'a': 9007199254740992.0}"), &[("a", Some("9007199254740992.0"))])),
        ("a = 00.5", Expect::Decode),
        ("a = -01.5", Expect::Decode),
        ("a = 1979-05-27", Expect::Ok(Some("{'a': datetime.date(1979, 5, 27)}"), &[("a", Some("1979-05-27"))])),
        ("a = 0000-01-01", Expect::Decode),
        ("a = 0001-01-01", Expect::Ok(Some("{'a': datetime.date(1, 1, 1)}"), &[("a", Some("0001-01-01"))])),
        ("a = 9999-12-31", Expect::Ok(Some("{'a': datetime.date(9999, 12, 31)}"), &[("a", Some("9999-12-31"))])),
        ("a = 2000-02-29", Expect::Ok(Some("{'a': datetime.date(2000, 2, 29)}"), &[("a", Some("2000-02-29"))])),
        ("a = 1900-02-29", Expect::Decode),
        ("a = 2024-02-29", Expect::Ok(Some("{'a': datetime.date(2024, 2, 29)}"), &[("a", Some("2024-02-29"))])),
        ("a = 2023-02-29", Expect::Decode),
        ("a = 2023-04-31", Expect::Decode),
        ("a = 2023-04-30", Expect::Ok(Some("{'a': datetime.date(2023, 4, 30)}"), &[("a", Some("2023-04-30"))])),
        ("a = 2023-13-01", Expect::Decode),
        ("a = 2023-00-10", Expect::Decode),
        ("a = 2023-01-00", Expect::Decode),
        ("a = 2023-01-32", Expect::Decode),
        ("a = 1979-05-27T07:32:00Z", Expect::Ok(Some("{'a': datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone.utc)}"), &[("a", Some("1979-05-27 07:32:00+00:00"))])),
        ("a = 1979-05-27t07:32:00z", Expect::Ok(Some("{'a': datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone.utc)}"), &[("a", Some("1979-05-27 07:32:00+00:00"))])),
        ("a = 1979-05-27 07:32:00", Expect::Ok(Some("{'a': datetime.datetime(1979, 5, 27, 7, 32)}"), &[("a", Some("1979-05-27 07:32:00"))])),
        ("a = 1979-05-27T07:32:00-07:30", Expect::Ok(Some("{'a': datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone(datetime.timedelta(days=-1, seconds=59400)))}"), &[("a", Some("1979-05-27 07:32:00-07:30"))])),
        ("a = 1979-05-27T07:32:00+00:00", Expect::Ok(Some("{'a': datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone.utc)}"), &[("a", Some("1979-05-27 07:32:00+00:00"))])),
        ("a = 1979-05-27T07:32:00-00:00", Expect::Ok(Some("{'a': datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone.utc)}"), &[("a", Some("1979-05-27 07:32:00+00:00"))])),
        ("a = 1979-05-27T07:32:00+23:59", Expect::Ok(Some("{'a': datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone(datetime.timedelta(seconds=86340)))}"), &[("a", Some("1979-05-27 07:32:00+23:59"))])),
        ("a = 1979-05-27T07:32:00-23:59", Expect::Ok(Some("{'a': datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone(datetime.timedelta(days=-1, seconds=60)))}"), &[("a", Some("1979-05-27 07:32:00-23:59"))])),
        ("a = 1979-05-27T07:32:00+24:00", Expect::Decode),
        ("a = 1979-05-27T07:32:00+0700", Expect::Decode),
        ("a = 1979-05-27T00:32:00.999999-07:00", Expect::Ok(Some("{'a': datetime.datetime(1979, 5, 27, 0, 32, 0, 999999, tzinfo=datetime.timezone(datetime.timedelta(days=-1, seconds=61200)))}"), &[("a", Some("1979-05-27 00:32:00.999999-07:00"))])),
        ("a = 1979-05-27T00:32:00.1234567", Expect::Ok(Some("{'a': datetime.datetime(1979, 5, 27, 0, 32, 0, 123456)}"), &[("a", Some("1979-05-27 00:32:00.123456"))])),
        ("a = 1979-05-27T00:32:00.5", Expect::Ok(Some("{'a': datetime.datetime(1979, 5, 27, 0, 32, 0, 500000)}"), &[("a", Some("1979-05-27 00:32:00.500000"))])),
        ("a = 1979-05-27T00:32:00.000000", Expect::Ok(Some("{'a': datetime.datetime(1979, 5, 27, 0, 32)}"), &[("a", Some("1979-05-27 00:32:00"))])),
        ("a = 1979-05-27T00:32:00.", Expect::Decode),
        ("a = 1979-05-27T07:32", Expect::Decode),
        ("a = 1979-05-27T23:59:60", Expect::Decode),
        ("a = 1979-05-27T24:00:00", Expect::Decode),
        ("a = 2023-02-29T00:00:00Z", Expect::Decode),
        ("a = 0000-01-01T00:00:00", Expect::Decode),
        ("a = 1979-05-27 ", Expect::Ok(Some("{'a': datetime.date(1979, 5, 27)}"), &[("a", Some("1979-05-27"))])),
        ("a = 1979-05-27x", Expect::Decode),
        ("a = 07:32:00", Expect::Ok(Some("{'a': datetime.time(7, 32)}"), &[("a", Some("07:32:00"))])),
        ("a = 00:00:00", Expect::Ok(Some("{'a': datetime.time(0, 0)}"), &[("a", Some("00:00:00"))])),
        ("a = 07:32:01", Expect::Ok(Some("{'a': datetime.time(7, 32, 1)}"), &[("a", Some("07:32:01"))])),
        ("a = 23:59:59.999999", Expect::Ok(Some("{'a': datetime.time(23, 59, 59, 999999)}"), &[("a", Some("23:59:59.999999"))])),
        ("a = 07:32:00.1234567", Expect::Ok(Some("{'a': datetime.time(7, 32, 0, 123456)}"), &[("a", Some("07:32:00.123456"))])),
        ("a = 07:32", Expect::Decode),
        ("a = 23:59:60", Expect::Decode),
        ("a = 24:00:00", Expect::Decode),
        ("a = 7:32:00", Expect::Decode),
        ("a = 07:32:00Z", Expect::Decode),
        ("a = 07:32:00.", Expect::Decode),
        ("a = [1979-05-27T07:32:00Z, 1979-05-27T07:32:00+05:30, 1979-05-27T07:32:00+05:30, 07:32:00.5, 1979-05-27]", Expect::Ok(Some("{'a': [datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone.utc), datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone(datetime.timedelta(seconds=19800))), datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone(datetime.timedelta(seconds=19800))), datetime.time(7, 32, 0, 500000), datetime.date(1979, 5, 27)]}"), &[("a", Some("[datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone.utc), datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone(datetime.timedelta(seconds=19800))), datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone(datetime.timedelta(seconds=19800))), datetime.time(7, 32, 0, 500000), datetime.date(1979, 5, 27)]"))])),
        ("a = []", Expect::Ok(Some("{'a': []}"), &[("a", Some("[]"))])),
        ("a = [ ]", Expect::Ok(Some("{'a': []}"), &[("a", Some("[]"))])),
        ("a = [1, 2, 3]", Expect::Ok(Some("{'a': [1, 2, 3]}"), &[("a", Some("[1, 2, 3]"))])),
        ("a = [1,2,]", Expect::Ok(Some("{'a': [1, 2]}"), &[("a", Some("[1, 2]"))])),
        ("a = [1 2]", Expect::Decode),
        ("a = [,]", Expect::Decode),
        ("a = [1,,2]", Expect::Decode),
        ("a = [\n 1, # c\n 2\n]", Expect::Ok(Some("{'a': [1, 2]}"), &[("a", Some("[1, 2]"))])),
        ("a = [[1, 2], ['a', \"b\"], [true], []]", Expect::Ok(Some("{'a': [[1, 2], ['a', 'b'], [True], []]}"), &[("a", Some("[[1, 2], ['a', 'b'], [True], []]"))])),
        ("a = [1, 'x', 1.5, {b = 1}]", Expect::Ok(Some("{'a': [1, 'x', 1.5, {'b': 1}]}"), &[("a", Some("[1, 'x', 1.5, {'b': 1}]"))])),
        ("a = [", Expect::Decode),
        ("a = [1", Expect::Decode),
        ("a = [1,", Expect::Decode),
        ("a = [ # c\u{1}\n]", Expect::Decode),
        ("a = [\n# c\n]", Expect::Ok(Some("{'a': []}"), &[("a", Some("[]"))])),
        ("a = [{}, {}]", Expect::Ok(Some("{'a': [{}, {}]}"), &[("a", Some("[{}, {}]"))])),
        ("a = {}", Expect::Ok(Some("{'a': {}}"), &[("a", Some("{}"))])),
        ("a = { }", Expect::Ok(Some("{'a': {}}"), &[("a", Some("{}"))])),
        ("a = {b = 1, c = 'x'}", Expect::Ok(Some("{'a': {'b': 1, 'c': 'x'}}"), &[("a", Some("{'b': 1, 'c': 'x'}"))])),
        ("a = {b.c = 1, b.d = 2}", Expect::Ok(Some("{'a': {'b': {'c': 1, 'd': 2}}}"), &[("a", Some("{'b': {'c': 1, 'd': 2}}"))])),
        ("a = {b = {c = {d = 1}}}", Expect::Ok(Some("{'a': {'b': {'c': {'d': 1}}}}"), &[("a", Some("{'b': {'c': {'d': 1}}}"))])),
        ("a = {b = 1,}", Expect::Decode),
        ("a = {b = 1\n}", Expect::Decode),
        ("a = {\nb = 1}", Expect::Decode),
        ("a = {b = 1, b = 2}", Expect::Decode),
        ("a = {b = {}, b.c = 1}", Expect::Decode),
        ("a = {b = 1, b.c = 2}", Expect::Decode),
        ("a = {b = [], b.c = 1}", Expect::Decode),
        ("a = {b.c = 1, b = 2}", Expect::Decode),
        ("a = {b.c = 1, b.c.d = 2}", Expect::Decode),
        ("a = {b = {c = 1}}\na.b.d = 2", Expect::Decode),
        ("a = {b = 1}\n[a]", Expect::Decode),
        ("a = {b = 1}\na.c = 2", Expect::Decode),
        ("a = {", Expect::Decode),
        ("a = {b", Expect::Decode),
        ("a = {b = ", Expect::Decode),
        ("a = {b = 1", Expect::Decode),
        ("a = {\"\" = 1}", Expect::Ok(Some("{'a': {'': 1}}"), &[("a", Some("{'': 1}"))])),
        ("a = {b = [{c = 1}], b.d = 1}", Expect::Decode),
        ("a = {b.c = [], b.c.d = 1}", Expect::Decode),
        ("a = {b = {c = 1}, b.d = 2}", Expect::Decode),
        ("\"\" = 1", Expect::Ok(Some("{'': 1}"), &[("", Some("1"))])),
        ("'' = 1", Expect::Ok(Some("{'': 1}"), &[("", Some("1"))])),
        ("\"a b\" = 1", Expect::Ok(Some("{'a b': 1}"), &[("a b", Some("1"))])),
        ("'a.b' = 1", Expect::Ok(Some("{'a.b': 1}"), &[("a.b", Some("1"))])),
        ("a . b . c = 1", Expect::Ok(Some("{'a': {'b': {'c': 1}}}"), &[("a", Some("{'b': {'c': 1}}"))])),
        ("a\t.\tb = 1", Expect::Ok(Some("{'a': {'b': 1}}"), &[("a", Some("{'b': 1}"))])),
        ("a.b = 1\na.c = 2", Expect::Ok(Some("{'a': {'b': 1, 'c': 2}}"), &[("a", Some("{'b': 1, 'c': 2}"))])),
        ("\"\u{e9}\" = 1", Expect::Ok(Some("{'\u{e9}': 1}"), &[("\u{e9}", Some("1"))])),
        ("\u{e9} = 1", Expect::Decode),
        ("a-b_C9 = 1", Expect::Ok(Some("{'a-b_C9': 1}"), &[("a-b_C9", Some("1"))])),
        ("1234 = 1", Expect::Ok(Some("{'1234': 1}"), &[("1234", Some("1"))])),
        ("a = 1 b = 2", Expect::Decode),
        ("a", Expect::Decode),
        ("a =", Expect::Decode),
        ("= 1", Expect::Decode),
        ("a.= 1", Expect::Decode),
        ("a .", Expect::Decode),
        ("a = 1 # c\u{7f}", Expect::Decode),
        ("a = 1 # c\ttab", Expect::Ok(Some("{'a': 1}"), &[("a", Some("1"))])),
        ("a = 1 # c\u{1}", Expect::Decode),
        ("a = 1\n# c\u{0}\n", Expect::Decode),
        ("a.b.c.d = 1\n[a.b]", Expect::Decode),
        ("a.b.c.d = 1\n[a.b.c.e]", Expect::Ok(Some("{'a': {'b': {'c': {'d': 1, 'e': {}}}}}"), &[("a", Some("{'b': {'c': {'d': 1, 'e': {}}}}"))])),
        ("a.b = 1\nA.b = 2", Expect::Ok(Some("{'a': {'b': 1}, 'A': {'b': 2}}"), &[("a", Some("{'b': 1}")), ("A", Some("{'b': 2}"))])),
        ("a = 1\n  b = 2", Expect::Ok(Some("{'a': 1, 'b': 2}"), &[("a", Some("1")), ("b", Some("2"))])),
        ("\ta = 1", Expect::Ok(Some("{'a': 1}"), &[("a", Some("1"))])),
        ("a = 1\r\n", Expect::Ok(Some("{'a': 1}"), &[("a", Some("1"))])),
        ("a = 1\rb = 2", Expect::Decode),
        ("a = 1\r\nb = 2\r\n", Expect::Ok(Some("{'a': 1, 'b': 2}"), &[("a", Some("1")), ("b", Some("2"))])),
        ("a = 'x'\r\nb = \"y\"\r\n", Expect::Ok(Some("{'a': 'x', 'b': 'y'}"), &[("a", Some("x")), ("b", Some("y"))])),
        ("a = 1 \r\n", Expect::Ok(Some("{'a': 1}"), &[("a", Some("1"))])),
        ("#\r\na = 1", Expect::Ok(Some("{'a': 1}"), &[("a", Some("1"))])),
        ("[a]\nb = 1", Expect::Ok(Some("{'a': {'b': 1}}"), &[("a", Some("{'b': 1}"))])),
        ("[a]\n[a]", Expect::Decode),
        ("[a]\nb = 1\n[a.c]\nd = 2", Expect::Ok(Some("{'a': {'b': 1, 'c': {'d': 2}}}"), &[("a", Some("{'b': 1, 'c': {'d': 2}}"))])),
        ("[a.c]\n[a]", Expect::Ok(Some("{'a': {'c': {}}}"), &[("a", Some("{'c': {}}"))])),
        ("[a.c]\n[a]\n[a]", Expect::Decode),
        ("[a]\n[a.c]\n[a]", Expect::Decode),
        ("a.b = 1\n[a]", Expect::Decode),
        ("[a]\nb.c = 1\n[a.b]", Expect::Decode),
        ("[a]\nb.c = 1\n[a.b.d]", Expect::Ok(Some("{'a': {'b': {'c': 1, 'd': {}}}}"), &[("a", Some("{'b': {'c': 1, 'd': {}}}"))])),
        ("[a]\nb = 1\n[a.b]", Expect::Decode),
        ("a = {}\n[a]", Expect::Decode),
        ("a = []\n[[a]]", Expect::Decode),
        ("[[a]]\n[a]", Expect::Decode),
        ("[a]\n[[a]]", Expect::Decode),
        ("a = [1]\na.b = 1", Expect::Decode),
        ("[[a]]\nb = {}\n[[a]]\nb = 1", Expect::Ok(Some("{'a': [{'b': {}}, {'b': 1}]}"), &[("a", Some("[{'b': {}}, {'b': 1}]"))])),
        ("[[a]]\nb = 1\n[[a]]\nb = 2", Expect::Ok(Some("{'a': [{'b': 1}, {'b': 2}]}"), &[("a", Some("[{'b': 1}, {'b': 2}]"))])),
        ("[[a]]\n[a.b]\nc = 1\n[[a]]\n[a.b]\nc = 2", Expect::Ok(Some("{'a': [{'b': {'c': 1}}, {'b': {'c': 2}}]}"), &[("a", Some("[{'b': {'c': 1}}, {'b': {'c': 2}}]"))])),
        ("[[a.b]]\nc = 1\n[a]\nd = 2", Expect::Ok(Some("{'a': {'b': [{'c': 1}], 'd': 2}}"), &[("a", Some("{'b': [{'c': 1}], 'd': 2}"))])),
        ("[[a]]\n[[a.b]]\nc = 1\n[[a.b]]\nc = 2\n[[a]]", Expect::Ok(Some("{'a': [{'b': [{'c': 1}, {'c': 2}]}, {}]}"), &[("a", Some("[{'b': [{'c': 1}, {'c': 2}]}, {}]"))])),
        ("[ a . b ]", Expect::Ok(Some("{'a': {'b': {}}}"), &[("a", Some("{'b': {}}"))])),
        ("[ [a]]", Expect::Decode),
        ("[[a] ]", Expect::Decode),
        ("[a", Expect::Decode),
        ("[[a]", Expect::Decode),
        ("[a]]", Expect::Decode),
        ("[a] b = 1", Expect::Decode),
        ("[a] # c", Expect::Ok(Some("{'a': {}}"), &[("a", Some("{}"))])),
        ("[]", Expect::Decode),
        ("[[]]", Expect::Decode),
        ("[a.]", Expect::Decode),
        ("[\"\"]\nb = 1", Expect::Ok(Some("{'': {'b': 1}}"), &[("", Some("{'b': 1}"))])),
        ("[a]\n[\"a\"]", Expect::Decode),
        ("[a]\nb = 1\n[a]\nc = 2", Expect::Decode),
        ("[a.b]\nc = 1\n[a]\nb.d = 2", Expect::Decode),
        ("[a.b]\nc = 1\n[a]\nb = 2", Expect::Decode),
        ("a.b.c = 1\na.b = 2", Expect::Decode),
        ("a.b.c = 1\na.b.d = 2\n[a.b]", Expect::Decode),
        ("[t]\na.b = 1\n[t.a]", Expect::Decode),
        ("[t]\na.b = 1\n[t.a.c]", Expect::Ok(Some("{'t': {'a': {'b': 1, 'c': {}}}}"), &[("t", Some("{'a': {'b': 1, 'c': {}}}"))])),
        ("[[t]]\na.b = 1\n[[t]]\na.b = 2", Expect::Ok(Some("{'t': [{'a': {'b': 1}}, {'a': {'b': 2}}]}"), &[("t", Some("[{'a': {'b': 1}}, {'a': {'b': 2}}]"))])),
        ("[[t]]\na.b = 1\n[t.a]", Expect::Decode),
        ("a = [{b = 1}]\n[a.c]", Expect::Decode),
        ("a = [{b = 1}]\n[[a]]", Expect::Decode),
        ("a = [{b = 1}]\n[[a.c]]", Expect::Decode),
        ("[tool.radon]\ncc_min = 'C'\n[tool.other]\nx = [1, 'a']\n", Expect::Ok(Some("{'tool': {'radon': {'cc_min': 'C'}, 'other': {'x': [1, 'a']}}}"), &[("tool", Some("{'radon': {'cc_min': 'C'}, 'other': {'x': [1, 'a']}}"))])),
        ("a = [{}]\na.b = 1", Expect::Decode),
        ("[[a]]\nx = [1]\n[[a]]\n[a.x]", Expect::Ok(Some("{'a': [{'x': [1]}, {'x': {}}]}"), &[("a", Some("[{'x': [1]}, {'x': {}}]"))])),
        ("p.x = []\n[[p]]", Expect::Decode),
        ("[[p]]\nx = []\n[[p]]\n[[p.x]]", Expect::Ok(Some("{'p': [{'x': []}, {'x': [{}]}]}"), &[("p", Some("[{'x': []}, {'x': [{}]}]"))])),
        ("$", Expect::Decode),
        ("a = 1 $", Expect::Decode),
        ("a = $", Expect::Decode),
        ("a = 1\n$", Expect::Decode),
        (" # c", Expect::Ok(Some("{}"), &[])),
        ("a=1#c", Expect::Ok(Some("{'a': 1}"), &[("a", Some("1"))])),
        ("a = \"x\" \"y\"", Expect::Decode),
        ("k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k.k = 1", Expect::Discard),
        ("a = [[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[", Expect::Discard),
        ("a = [[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[1]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]", Expect::Ok(Some("{'a': [[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[1]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]}"), &[("a", Some("[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[1]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]]"))])),
    ];
    // END GENERATED CASES

    enum Expect {
        /// `TOMLDecodeError`.
        Decode,
        /// Any other exception: radon drops the TOML config.
        Discard,
        /// `repr()` of the table (`None` where it raises), then each
        /// top-level key with its `str()`.
        Ok(
            Option<&'static str>,
            &'static [(&'static str, Option<&'static str>)],
        ),
    }

    #[test]
    fn oracle_cases() {
        on_a_main_sized_stack(check_oracle_cases);
    }

    fn check_oracle_cases() {
        let mut failures = Vec::new();
        for (doc, expect) in CASES {
            let got = loads(doc);
            let same = match (expect, &got) {
                (Expect::Decode, Err(LoadError::Decode(_)))
                | (Expect::Discard, Err(LoadError::Discard(_))) => true,
                (Expect::Ok(repr, strs), Ok(table)) => {
                    table.py_repr().ok().as_deref() == *repr
                        && table.0.len() == strs.len()
                        && table
                            .0
                            .iter()
                            .zip(strs.iter())
                            .all(|((key, value), (k, s))| {
                                key == k && value.py_str().ok().as_deref() == *s
                            })
                }
                _ => false,
            };
            if !same {
                let rendered = got.as_ref().map(Table::py_repr);
                failures.push(format!("{doc:?}: got {rendered:?}"));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} cases differ:\n{}",
            failures.len(),
            CASES.len(),
            failures.join("\n")
        );
    }

    /// Run `body` on a thread with the 8 MiB a main thread has. A test thread
    /// has 2 MiB, which a debug build's frames outgrow on the deepest
    /// documents the parser accepts.
    fn on_a_main_sized_stack(body: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(8 << 20)
            .spawn(body)
            .expect("a thread")
            .join()
            .expect("the test body");
    }

    fn is_discard(result: &Result<Table, LoadError>) -> bool {
        matches!(result, Err(LoadError::Discard(_)))
    }

    fn only_value(doc: &str) -> Value {
        let mut table = loads(doc).expect("valid document");
        assert_eq!(table.0.len(), 1);
        table.0.remove(0).1
    }

    #[test]
    fn decode_error_message_points_at_the_character() {
        let Err(LoadError::Decode(msg)) = loads("a = 1\nb = $") else {
            panic!("expected a decode error");
        };
        assert_eq!(msg, "Invalid value (at line 2, column 5)");
        let Err(LoadError::Decode(msg)) = loads("a = ") else {
            panic!("expected a decode error");
        };
        assert_eq!(msg, "Invalid value (at end of document)");
    }

    /// `int(text, 0)` refuses more than 4300 decimal digits; underscores and
    /// the sign do not count. (Oracle: 4300 kept, 4301 `ValueError`.)
    #[test]
    fn decimal_literals_stop_at_4300_digits() {
        let digits = "1".repeat(4300);
        assert_eq!(
            only_value(&format!("a = {digits}")).py_str(),
            Ok(digits.clone())
        );
        assert_eq!(
            only_value(&format!("a = -{digits}")).py_str(),
            Ok(format!("-{digits}"))
        );
        assert!(is_discard(&loads(&format!("a = {digits}1"))));
        assert!(is_discard(&loads(&format!("a = -{digits}1"))));
        let underscored = format!("1{}", "_1".repeat(4299));
        assert_eq!(
            only_value(&format!("a = {underscored}")).py_str(),
            Ok(digits)
        );
        assert!(is_discard(&loads(&format!("a = {underscored}_1"))));
        // The limit is on parsing the whole document, even in an unused table.
        assert!(is_discard(&loads(&format!(
            "[x]\na = [{}1]\n[tool.radon]\ncc_min = 'A'",
            "1".repeat(4300)
        ))));
    }

    /// Hexadecimal of `10**exp`, by a multiplication independent of the
    /// division the port renders with.
    fn hex_power_of_ten(exp: usize) -> String {
        let mut limbs: Vec<u32> = vec![1];
        for _ in 0..exp {
            let mut carry = 0u64;
            for limb in &mut limbs {
                let cur = u64::from(*limb) * 10 + carry;
                *limb = low_u32(cur);
                carry = cur >> 32;
            }
            if carry != 0 {
                limbs.push(low_u32(carry));
            }
        }
        let mut hex = format!("{:x}", limbs.last().unwrap());
        for limb in limbs.iter().rev().skip(1) {
            let _ = write!(hex, "{limb:08x}");
        }
        hex
    }

    /// Hex/octal/binary literals have no parse limit, but `str()` raises past
    /// 4300 decimal digits, nested or not.
    #[test]
    fn prefixed_literals_render_up_to_4300_digits() {
        let ten_4300 = hex_power_of_ten(4300);
        // 10**4300 - 1 has 4300 digits: subtract one from the hex text.
        let mut below = ten_4300.clone().into_bytes();
        for b in below.iter_mut().rev() {
            if *b == b'0' {
                *b = b'f';
            } else {
                *b = if *b == b'a' { b'9' } else { *b - 1 };
                break;
            }
        }
        let below = String::from_utf8(below).unwrap();
        assert_eq!(
            only_value(&format!("a = 0x{below}")).py_str(),
            Ok("9".repeat(4300))
        );
        let over = only_value(&format!("a = 0x{ten_4300}"));
        assert!(over.py_str().is_err());
        assert!(over.py_repr().is_err());
        let nested = loads(&format!("a = [1, {{b = 0x{ten_4300}}}]")).unwrap();
        assert!(nested.py_repr().is_err());
        assert!(nested.0[0].1.py_str().is_err());
        // 2**14284 has 4300 digits, 2**14285 has 4301 (oracle).
        let two_14284 = only_value(&format!("a = 0b1{}", "0".repeat(14284)))
            .py_str()
            .unwrap();
        assert_eq!(two_14284.len(), 4300);
        assert!(two_14284.starts_with("81744410132094422") && two_14284.ends_with("5823010816"));
        assert!(
            only_value(&format!("a = 0b1{}", "0".repeat(14285)))
                .py_str()
                .is_err()
        );
        // An enormous literal parses quickly and still refuses to render.
        let huge = only_value(&format!("a = 0x{}", "f".repeat(200_000)));
        assert!(huge.py_str().is_err());
    }

    /// `MAX_KEY_PARTS`: 1000 parts kept, 1001 `RecursionError` (oracle).
    #[test]
    fn keys_stop_at_1000_parts() {
        let key = |n: usize| vec!["a"; n].join(".");
        assert!(loads(&format!("{} = 1", key(1000))).is_ok());
        assert!(is_discard(&loads(&format!("{} = 1", key(1001)))));
        assert!(loads(&format!("[{}]", key(1000))).is_ok());
        assert!(is_discard(&loads(&format!("[{}]", key(1001)))));
        assert!(is_discard(&loads(&format!("x = {{{} = 1}}", key(1001)))));
    }

    /// Constructing `TOMLDecodeError` runs a Python `__init__`: at the frame
    /// limit the decode error becomes a `RecursionError`. For `a = "\q"`,
    /// CPython needs `sys.setrecursionlimit(9)` from a `-c` script (frame 1)
    /// for the decode error, and gives `RecursionError` at 8.
    #[test]
    fn decode_error_at_the_frame_limit_is_a_recursion_error() {
        let doc = format!("a = \"{}q\"", '\\');
        assert!(matches!(parse_document(&doc, 8), Err(LoadError::Decode(_))));
        assert!(is_discard(&parse_document(&doc, 7)));
    }

    fn under_tool_other(open: &str, leaf: &str, close: &str, n: usize) -> String {
        format!(
            "[tool.radon]\ncc_min=\"C\"\n[tool.other]\nn={}{leaf}{}\n",
            open.repeat(n),
            close.repeat(n)
        )
    }

    /// The `radon` console script's limits (max kept, measured on radon
    /// 6.0.1 / CPython 3.14.8): n kept, n + 1 discarded.
    #[test]
    fn frame_limit_matches_the_radon_console_script() {
        on_a_main_sized_stack(frame_limit_cases);
    }

    fn frame_limit_cases() {
        let ml = format!("\"\"\"a{}\n b\"\"\"", '\\');
        let esc = format!("\"{}u00e9\"", '\\');
        let cases: &[(&str, &str, &str, usize)] = &[
            ("[", "1", "]", 491),
            ("{a=", "1", "}", 327),
            ("[{a=", "1", "}]", 196),
            ("[", "\"s\"", "]", 491),
            ("[", "[]", "]", 490),
            ("[", "{}", "]", 491),
            ("[", "[ # c\n1]", "]", 490),
            ("[", "1979-05-27T07:32:00+01:00", "]", 491),
            ("[", &ml, "]", 489),
            ("[", &esc, "]", 489),
            ("{a=", "\"s\"", "}", 327),
            ("{a=", &ml, "}", 326),
            ("[{a=", &esc, "}]", 195),
        ];
        for &(open, leaf, close, n) in cases {
            assert!(
                loads(&under_tool_other(open, leaf, close, n)).is_ok(),
                "{open}{leaf} x{n}"
            );
            assert!(
                is_discard(&loads(&under_tool_other(open, leaf, close, n + 1))),
                "{open}{leaf} x{}",
                n + 1
            );
        }
        // Arrays inside one inline table: 490 / 491.
        let doc = |n: usize| {
            format!(
                "[tool.radon]\ncc_min=\"C\"\n[tool.other]\nn={{a={}1{}}}\n",
                "[".repeat(n),
                "]".repeat(n)
            )
        };
        assert!(loads(&doc(490)).is_ok());
        assert!(is_discard(&loads(&doc(491))));
        // Hostile nesting is refused without overflowing the stack.
        assert!(is_discard(&loads(&format!("a = {}", "[".repeat(100_000)))));
        assert!(is_discard(&loads(&format!(
            "a = {}",
            "{a=".repeat(100_000)
        ))));
    }

    /// The stack the parser needs is bounded by the frame budget, and a
    /// release build must keep it well inside a main thread's: the deepest
    /// documents the budget accepts, on a 1 MiB thread. (A debug build's
    /// frames are several times larger, so this runs under `--release` only.)
    #[cfg(not(debug_assertions))]
    #[test]
    fn the_deepest_documents_fit_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(|| {
                frame_limit_cases();
                let key = vec!["k"; 1000].join(".");
                let deep = format!(
                    "d = {}0{}",
                    format!("{{{key} = ").repeat(327),
                    "}".repeat(327)
                );
                assert!(loads(&deep).is_ok());
            })
            .expect("a thread")
            .join()
            .expect("the deepest documents");
    }

    /// TOML allows values nested far deeper than any stack: 327 inline tables
    /// of 1000-part keys make 327,000 levels. Converting, measuring,
    /// rendering and dropping such a value must not recurse once per level,
    /// which would need tens of megabytes here. (If one of them ever does,
    /// this test does not fail: the stack overflows and the whole test binary
    /// aborts.)
    #[test]
    fn deep_values_cost_no_stack() {
        on_a_main_sized_stack(|| {
            let key = vec!["k"; 1000].join(".");
            let nest = |levels: usize, leaf: &str, closed: usize| {
                let open = format!("{{{key} = ").repeat(levels);
                format!("deep = {open}{leaf}{}\n", "}".repeat(closed))
            };
            // Parsed, converted and dropped.
            let table = loads(&nest(327, "0", 327)).expect("a valid document");
            let (_, deep) = &table.0[0];
            // Rendered: `{'k': {'k': ... 0}}`, which is 6 characters a level
            // going in and one closing brace each coming out.
            let text = deep.py_repr().expect("no long integer");
            let (opening, closing) = text.split_once('0').expect("the leaf");
            assert_eq!(opening, "{'k': ".repeat(327_000));
            assert_eq!(closing, "}".repeat(327_000));
            drop(table);
            // A parse that fails after building the tree drops it too, with
            // the flags that nest as deep.
            let failed = loads(&nest(327, "0", 326));
            assert!(matches!(failed, Err(LoadError::Decode(_))));
            // One level more is past the frame budget, as in tomllib.
            assert!(is_discard(&loads(&nest(328, "0", 328))));
        });
    }

    #[test]
    fn values_render_like_python() {
        let v =
            only_value("a = [1979-05-27T07:32:00-07:30, 07:32:00.5, 0001-01-01, 1e400, -nan, 'x']");
        assert_eq!(
            v.py_str().unwrap(),
            "[datetime.datetime(1979, 5, 27, 7, 32, tzinfo=datetime.timezone(datetime.timedelta(days=-1, \
             seconds=59400))), datetime.time(7, 32, 0, 500000), datetime.date(1, 1, 1), inf, nan, 'x']"
        );
        assert_eq!(
            only_value("a = 1979-05-27T07:32:00.123456Z")
                .py_str()
                .unwrap(),
            "1979-05-27 07:32:00.123456+00:00"
        );
    }
}
