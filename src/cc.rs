//! Cyclomatic complexity, ported from radon's `ComplexityVisitor`.
//!
//! Results match radon 6.0.1 exactly, quirks included:
//! - `assert` adds 1 (unless `no_assert`) and its sub-expressions are never
//!   visited;
//! - decorators, default arguments and annotations of `def`/`class` are never
//!   visited;
//! - a function's `endline` comes only from the *last* statement of its body;
//! - `except*` (`TryStar`) adds nothing;
//! - classes defined inside functions are dropped.

use std::cmp::Reverse;
use std::fmt;

use ruff_python_ast::token::{TokenKind, Tokens};
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{
    self as ast, Alias, Comprehension, Decorator, ElifElseClause, ExceptHandler, Expr,
    InterpolatedStringElement, InterpolatedStringElements, Keyword, Parameter, Pattern, Stmt,
    StringLiteral, TypeParam,
};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::error::Error;
use crate::source::{Source, to_u32};

/// A function or method.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Function {
    /// The name after `def`. Listed as a block of its own
    /// ([`Options::show_closures`]), a closure is named with its parent:
    /// `outer.inner`.
    pub name: String,
    /// The line of the `def` keyword (of `async`, for `async def`), counted
    /// from 1. Decorators come before it and are not counted.
    pub lineno: u32,
    /// The column of the `def` keyword (of `async`, for `async def`), in
    /// UTF-8 bytes from 0, as CPython's `col_offset`.
    pub col_offset: u32,
    /// The function's last line as radon computes it, which is not always
    /// where it ends: the line on which the last piece of its last statement
    /// starts. A statement that goes on past that line (a multi-line string,
    /// say) does not extend it, and when the last statement is an `assert`,
    /// a `def` or a `class`, the result is [`Function::lineno`].
    pub endline: u32,
    /// The enclosing class for methods; `None` for functions and closures.
    pub classname: Option<String>,
    /// The functions defined inside this one, but not inside one of those.
    pub closures: Vec<Function>,
    /// The cyclomatic complexity: one, plus one per decision point in the
    /// body. Closures are scored on their own and add nothing here.
    pub complexity: u32,
}

impl Function {
    /// Whether the function is defined directly in a class body.
    pub fn is_method(&self) -> bool {
        self.classname.is_some()
    }

    /// The grade for [`Function::complexity`].
    pub fn rank(&self) -> Rank {
        Rank::of(f64::from(self.complexity))
    }
}

/// A class with its methods.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Class {
    /// The name after `class`. Listed as a block of its own
    /// ([`Options::show_closures`]), an inner class is named with its parent:
    /// `Outer.Inner`.
    pub name: String,
    /// The line of the `class` keyword, counted from 1.
    pub lineno: u32,
    /// The column of the `class` keyword, in UTF-8 bytes from 0.
    pub col_offset: u32,
    /// The class's last line as radon computes it, which is not always where
    /// it ends: the highest line on which a piece of its body starts, or on
    /// which one of its methods ends ([`Function::endline`]). `assert`
    /// statements and inner classes are not looked into.
    pub endline: u32,
    /// The functions defined in the class body, but not inside another
    /// function or class.
    pub methods: Vec<Function>,
    /// The classes defined in the class body, but not inside a function or
    /// another class.
    pub inner_classes: Vec<Class>,
    /// One plus the decision points of the whole class body.
    real_complexity: u32,
}

impl Class {
    /// radon's class score. Without methods: one, plus the decision points of
    /// the class body. With methods: that, plus the methods' complexities,
    /// divided by the number of methods (rounding down), plus one when there
    /// is more than one method.
    pub fn complexity(&self) -> u32 {
        match to_u32(self.methods.len()) {
            0 => self.real_complexity,
            n => self.real_complexity / n + u32::from(n > 1),
        }
    }

    /// The grade for [`Class::complexity`].
    pub fn rank(&self) -> Rank {
        Rank::of(f64::from(self.complexity()))
    }
}

/// A scored block, as listed by `radon cc`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    /// A function or a method. With [`Options::show_closures`], also a
    /// closure or an inner class's method.
    Function(Function),
    /// A class. With [`Options::show_closures`], also an inner class.
    Class(Class),
}

impl Block {
    /// The block's name, without its class: [`Function::name`] or
    /// [`Class::name`].
    pub fn name(&self) -> &str {
        match self {
            Self::Function(f) => &f.name,
            Self::Class(c) => &c.name,
        }
    }

    /// `Class.method` for methods, otherwise the name.
    pub fn fullname(&self) -> impl fmt::Display + '_ {
        struct FullName<'b>(Option<&'b str>, &'b str);
        impl fmt::Display for FullName<'_> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                if let Some(class) = self.0 {
                    write!(f, "{class}.")?;
                }
                f.write_str(self.1)
            }
        }
        let class = match self {
            Self::Function(f) => f.classname.as_deref(),
            Self::Class(_) => None,
        };
        FullName(class, self.name())
    }

    /// The line the block starts on, counted from 1.
    pub fn lineno(&self) -> u32 {
        match self {
            Self::Function(f) => f.lineno,
            Self::Class(c) => c.lineno,
        }
    }

    /// The column the block starts at, in UTF-8 bytes from 0.
    pub fn col_offset(&self) -> u32 {
        match self {
            Self::Function(f) => f.col_offset,
            Self::Class(c) => c.col_offset,
        }
    }

    /// The block's score: a function's complexity, or a class's
    /// ([`Class::complexity`]).
    pub fn complexity(&self) -> u32 {
        match self {
            Self::Function(f) => f.complexity,
            Self::Class(c) => c.complexity(),
        }
    }

    /// The grade for [`Block::complexity`].
    pub fn rank(&self) -> Rank {
        match self {
            Self::Function(f) => f.rank(),
            Self::Class(c) => c.rank(),
        }
    }

    /// `F` function, `M` method, `C` class.
    pub fn letter(&self) -> char {
        match self {
            Self::Function(f) if f.is_method() => 'M',
            Self::Function(_) => 'F',
            Self::Class(_) => 'C',
        }
    }
}

/// A complexity grade from A (simplest) to F.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rank {
    /// 1 to 5: simple.
    A,
    /// 6 to 10.
    B,
    /// 11 to 20.
    C,
    /// 21 to 30.
    D,
    /// 31 to 40.
    E,
    /// Above 40: very complex.
    F,
}

impl Rank {
    /// `radon.complexity.cc_rank`: A for 1-5, B for 6-10, C for 11-20, D for
    /// 21-30, E for 31-40 and F above 40.
    pub fn of(score: f64) -> Self {
        let tens = (score / 10.0).ceil();
        let base = if tens == 0.0 { 1.0 } else { tens };
        let index = base - if score <= 5.0 { 1.0 } else { 0.0 };
        // Comparisons rather than a cast: `index` is a whole number, and
        // anything past the ends (or NaN) takes the nearest grade.
        if index >= 5.0 {
            Self::F
        } else if index >= 4.0 {
            Self::E
        } else if index >= 3.0 {
            Self::D
        } else if index >= 2.0 {
            Self::C
        } else if index >= 1.0 {
            Self::B
        } else {
            Self::A
        }
    }

    /// The grade's letter.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
            Self::D => "D",
            Self::E => "E",
            Self::F => "F",
        }
    }
}

impl fmt::Display for Rank {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How `radon cc` orders blocks within a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Order {
    /// Highest complexity first.
    #[default]
    Score,
    /// By line number.
    Lines,
    /// By name.
    Alpha,
}

impl Order {
    /// Parse radon's `-o` value; like radon, anything unknown means `SCORE`.
    /// Case-folding is Python's full `str.upper()` (`ſ` becomes `S`).
    pub(crate) fn from_radon(name: &str) -> Self {
        match crate::pyfmt::py_upper(name).as_str() {
            "LINES" => Self::Lines,
            "ALPHA" => Self::Alpha,
            _ => Self::Score,
        }
    }
}

/// How [`analyze`] counts and lists blocks: radon's `--no-assert`,
/// `--show-closures` and `--order`. The default is radon's.
///
/// ```
/// use oxradon::cc::{Options, Order};
///
/// let by_line = Options { order: Order::Lines, ..Options::default() };
/// assert!(!by_line.no_assert);
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Do not count `assert` statements.
    pub no_assert: bool,
    /// List closures and inner classes as blocks of their own.
    pub show_closures: bool,
    /// The order of the blocks.
    pub order: Order,
}

/// Analyze Python source: radon's `cc_visit`, then the CLI's closure
/// expansion and sorting (radon's `CCHarvester.gobble`).
///
/// Code is accepted exactly when CPython 3.14's `ast.parse` accepts it,
/// except `from __future__ import barry_as_FLUFL` (which enables `<>`).
///
/// Parsing and visiting recurse once per nesting level, so pathologically
/// deep input needs a large stack; the CLI runs this on 64 MiB threads.
///
/// ```
/// use oxradon::cc::{self, Block, Options};
///
/// let source = "\
/// class Cart:
///     def total(self, items):
///         return sum(i.price for i in items if i.in_stock)
/// ";
/// let blocks = cc::analyze(source, &Options::default())?;
///
/// // radon lists a class and each of its methods, highest score first.
/// let listed: Vec<_> = blocks
///     .iter()
///     .map(|b| (b.fullname().to_string(), b.complexity()))
///     .collect();
/// assert_eq!(listed, [("Cart".to_owned(), 4), ("Cart.total".to_owned(), 3)]);
/// assert!(matches!(blocks[0], Block::Class(_)));
/// # Ok::<(), oxradon::Error>(())
/// ```
///
/// # Errors
/// The syntax error of code that CPython 3.14 does not accept.
pub fn analyze(text: &str, options: &Options) -> Result<Vec<Block>, Error> {
    let src = Source::new(text);
    let parsed = crate::syntax::parse(text, &src)?;
    let mut visitor = ComplexityVisitor::new(&src, parsed.tokens(), options.no_assert, None);
    for stmt in &parsed.syntax().body {
        visitor.visit_stmt(stmt);
    }
    let mut blocks = visitor.into_blocks();
    if options.show_closures {
        blocks = add_inner_blocks(blocks);
    }
    sort(&mut blocks, options.order);
    Ok(blocks)
}

/// A module's total complexity, as `radon mi` uses it:
/// `ComplexityVisitor.from_ast(ast.parse(text)).total_complexity`. That is 1,
/// plus the module-level decision points, plus each top-level function's and
/// class's complexity less one (`functions_complexity`, `classes_complexity`).
/// `assert` counts, as `no_assert` is off there.
///
/// Parsing and visiting recurse once per nesting level, so pathologically
/// deep input needs a large stack; the CLI runs this on 64 MiB threads.
///
/// # Errors
/// As [`analyze`].
pub(crate) fn total_complexity(text: &str) -> Result<u64, Error> {
    let src = Source::new(text);
    let parsed = crate::syntax::parse(text, &src)?;
    let mut visitor = ComplexityVisitor::new(&src, parsed.tokens(), false, None);
    for stmt in &parsed.syntax().body {
        visitor.visit_stmt(stmt);
    }
    // radon's module visitor starts at 1 (`off=True`); this one starts at 0.
    let functions: u64 = (visitor.functions.iter())
        .map(|f| u64::from(f.complexity) - 1)
        .sum();
    let classes: u64 = (visitor.classes.iter())
        .map(|c| u64::from(c.real_complexity) - 1)
        .sum();
    Ok(1 + u64::from(visitor.complexity) + functions + classes)
}

/// Stable sort, like Python's `sorted`.
fn sort(blocks: &mut [Block], order: Order) {
    match order {
        Order::Score => blocks.sort_by_key(|b| Reverse(b.complexity())),
        Order::Lines => blocks.sort_by_key(Block::lineno),
        Order::Alpha => blocks.sort_by(|a, b| a.name().cmp(b.name())),
    }
}

/// `radon.complexity.add_inner_blocks`: list closures and inner classes as
/// top-level blocks, prefixing names with their parent. The traversal order
/// (a stack) is radon's, which matters for ties when sorting.
fn add_inner_blocks(blocks: Vec<Block>) -> Vec<Block> {
    let mut out = Vec::with_capacity(blocks.len());
    let mut stack = blocks;
    while let Some(block) = stack.pop() {
        match &block {
            Block::Function(f) => {
                for closure in &f.closures {
                    stack.push(Block::Function(Function {
                        name: format!("{}.{}", f.name, closure.name),
                        ..closure.clone()
                    }));
                }
            }
            Block::Class(c) => {
                for inner in &c.inner_classes {
                    let name = format!("{}.{}", c.name, inner.name);
                    let methods: Vec<Block> = (inner.methods.iter())
                        .map(|m| {
                            let classname = Some(name.clone());
                            Block::Function(Function {
                                classname,
                                ..m.clone()
                            })
                        })
                        .collect();
                    stack.push(Block::Class(Class {
                        name,
                        ..inner.clone()
                    }));
                    stack.extend(methods);
                }
            }
        }
        out.push(block);
    }
    out
}

/// radon's `ComplexityVisitor`, over Ruff's AST.
///
/// Ruff's AST differs from CPython's in a few places this visitor maps back:
/// `elif` is a clause rather than a nested `If`, string pieces are grouped
/// differently (see [`LiteralRun`]), and format specs have no node of their
/// own.
struct ComplexityVisitor<'a> {
    src: &'a Source<'a>,
    tokens: &'a Tokens,
    no_assert: bool,
    /// Set while visiting a class body: functions found here are its methods.
    classname: Option<&'a str>,
    complexity: u32,
    functions: Vec<Function>,
    classes: Vec<Class>,
    /// Start of the last-starting node seen. radon tracks the largest
    /// `node.lineno`; lines grow with offsets, so the offset is converted to a
    /// line only when needed.
    max_offset: Option<TextSize>,
}

impl<'a> ComplexityVisitor<'a> {
    fn new(
        src: &'a Source<'a>,
        tokens: &'a Tokens,
        no_assert: bool,
        classname: Option<&'a str>,
    ) -> Self {
        Self {
            src,
            tokens,
            no_assert,
            classname,
            complexity: 0,
            functions: Vec::new(),
            classes: Vec::new(),
            max_offset: None,
        }
    }

    /// radon's `blocks`: functions, then each class followed by its methods.
    fn into_blocks(self) -> Vec<Block> {
        let mut blocks: Vec<Block> = self.functions.into_iter().map(Block::Function).collect();
        for class in self.classes {
            let methods = class.methods.clone();
            blocks.push(Block::Class(class));
            blocks.extend(methods.into_iter().map(Block::Function));
        }
        blocks
    }

    /// `generic_visit` records the position of every node that has one.
    fn mark(&mut self, node: &impl Ranged) {
        self.mark_offset(node.start());
    }

    fn mark_offset(&mut self, offset: TextSize) {
        self.max_offset = self.max_offset.max(Some(offset));
    }

    fn line(&self, offset: Option<TextSize>) -> u32 {
        offset.map_or(0, |o| self.src.line(o))
    }

    /// CPython places a decorated `def`/`class` at its keyword (`def`,
    /// `async` or `class`), not at the first decorator: the first token after
    /// the last decorator that is not a newline or comment.
    fn position(&self, range: TextRange, decorators: &[Decorator]) -> (u32, u32) {
        let keyword = decorators.last().and_then(|last| {
            (self.tokens.after(last.end()).iter())
                .find(|t| !t.kind().is_trivia() && t.kind() != TokenKind::Newline)
                .map(Ranged::start)
        });
        let start = keyword.unwrap_or(range.start());
        (self.src.line(start), self.src.col(start))
    }

    fn visit_function_def(&mut self, node: &'a ast::StmtFunctionDef) {
        let mut closures = Vec::new();
        let mut complexity = 1;
        let mut last_offset = None;
        for stmt in &node.body {
            let mut child = ComplexityVisitor::new(self.src, self.tokens, self.no_assert, None);
            child.visit_stmt(stmt);
            complexity += child.complexity;
            last_offset = child.max_offset;
            closures.extend(child.functions);
        }
        let (lineno, col_offset) = self.position(node.range, &node.decorator_list);
        self.functions.push(Function {
            name: node.name.to_string(),
            lineno,
            col_offset,
            endline: lineno.max(self.line(last_offset)),
            classname: self.classname.map(str::to_owned),
            closures,
            complexity,
        });
    }

    fn visit_class_def(&mut self, node: &'a ast::StmtClassDef) {
        let name = node.name.as_str();
        let mut methods = Vec::new();
        let mut inner_classes = Vec::new();
        let mut real_complexity = 1;
        let mut max_offset = None;
        for stmt in &node.body {
            let mut child =
                ComplexityVisitor::new(self.src, self.tokens, self.no_assert, Some(name));
            child.visit_stmt(stmt);
            real_complexity +=
                child.complexity + child.functions.iter().map(|f| f.complexity).sum::<u32>();
            max_offset = max_offset.max(child.max_offset);
            methods.extend(child.functions);
            inner_classes.extend(child.classes);
        }
        let (lineno, col_offset) = self.position(node.range, &node.decorator_list);
        let methods_end = methods.iter().map(|m| m.endline).max().unwrap_or(0);
        self.classes.push(Class {
            name: name.to_owned(),
            lineno,
            col_offset,
            endline: lineno.max(self.line(max_offset)).max(methods_end),
            methods,
            inner_classes,
            real_complexity,
        });
    }

    /// Visit the pieces of an f-string or t-string, as CPython's AST sees them
    /// (see [`LiteralRun`]).
    fn visit_string_parts(&mut self, parts: impl IntoIterator<Item = StringPart<'a>>) {
        let mut run = LiteralRun::default();
        for part in parts {
            match part {
                StringPart::Literal(s) => run.push(s.start(), !s.value.is_empty()),
                StringPart::Elements(elements) => {
                    for element in elements {
                        match element {
                            // Decodes to nothing (e.g. a backslash-newline):
                            // CPython drops it before concatenating, so it
                            // cannot start a run.
                            InterpolatedStringElement::Literal(l) if l.value.is_empty() => {}
                            InterpolatedStringElement::Literal(l) => run.push(l.start(), true),
                            InterpolatedStringElement::Interpolation(_) => {
                                self.close_run(&mut run);
                                self.visit_interpolated_string_element(element);
                            }
                        }
                    }
                }
            }
        }
        self.close_run(&mut run);
    }

    fn close_run(&mut self, run: &mut LiteralRun) {
        if let Some(start) = std::mem::take(run).node_start() {
            self.mark_offset(start);
        }
    }
}

/// Consecutive literal pieces of an implicitly concatenated f-/t-string
/// (`'a' f'b{x}' 'c'`), following CPython's `_build_concatenated_str`: the run
/// folds into one `Constant` positioned at its first piece, and no node at all
/// when its text is empty. Plain string parts always join the run, even empty
/// ones (`""`); literal elements of an f-/t-string that decode to nothing were
/// already dropped by `_get_resized_exprs` and never do.
#[derive(Default)]
struct LiteralRun {
    start: Option<TextSize>,
    has_content: bool,
}

impl LiteralRun {
    fn push(&mut self, start: TextSize, has_content: bool) {
        self.start.get_or_insert(start);
        self.has_content |= has_content;
    }

    /// Where CPython's merged `Constant` starts, if it creates one.
    fn node_start(self) -> Option<TextSize> {
        self.start.filter(|_| self.has_content)
    }
}

/// A piece of an f-string or t-string: a plain literal, or the elements of
/// one `f"..."`/`t"..."` part.
enum StringPart<'a> {
    Literal(&'a StringLiteral),
    Elements(&'a InterpolatedStringElements),
}

impl<'a> Visitor<'a> for ComplexityVisitor<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(node) => return self.visit_function_def(node),
            Stmt::ClassDef(node) => return self.visit_class_def(node),
            Stmt::Assert(_) => {
                self.complexity += u32::from(!self.no_assert);
                return;
            }
            Stmt::If(_) => self.complexity += 1,
            Stmt::For(node) => self.complexity += 1 + u32::from(!node.orelse.is_empty()),
            Stmt::While(node) => self.complexity += 1 + u32::from(!node.orelse.is_empty()),
            Stmt::Try(node) if !node.is_star => {
                self.complexity += to_u32(node.handlers.len()) + u32::from(!node.orelse.is_empty());
            }
            Stmt::Match(node) => {
                // A bare name or `_` pattern is the "else" branch.
                let has_default = node.cases.iter().any(|case| {
                    matches!(
                        &case.pattern,
                        Pattern::MatchAs(ast::PatternMatchAs { pattern: None, .. })
                    )
                });
                self.complexity += to_u32(node.cases.len()).saturating_sub(u32::from(has_default));
            }
            _ => {}
        }
        self.mark(stmt);
        visitor::walk_stmt(self, stmt);
    }

    /// An `elif` is a nested `If` node in CPython's AST.
    fn visit_elif_else_clause(&mut self, clause: &'a ElifElseClause) {
        if clause.test.is_some() {
            self.mark(clause);
            self.complexity += 1;
        }
        visitor::walk_elif_else_clause(self, clause);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        self.mark(expr);
        match expr {
            Expr::BoolOp(node) => self.complexity += to_u32(node.values.len()) - 1,
            Expr::If(_) => self.complexity += 1,
            Expr::FString(node) => {
                return self.visit_string_parts(node.value.iter().map(|part| match part {
                    ast::FStringPartRef::Literal(s) => StringPart::Literal(s),
                    ast::FStringPartRef::FString(f) => StringPart::Elements(&f.elements),
                }));
            }
            Expr::TString(node) => {
                return self.visit_string_parts(
                    node.value.iter().map(|t| StringPart::Elements(&t.elements)),
                );
            }
            _ => {}
        }
        visitor::walk_expr(self, expr);
    }

    fn visit_comprehension(&mut self, node: &'a Comprehension) {
        self.complexity += to_u32(node.ifs.len()) + 1;
        visitor::walk_comprehension(self, node);
    }

    fn visit_except_handler(&mut self, node: &'a ExceptHandler) {
        self.mark(node);
        visitor::walk_except_handler(self, node);
    }

    fn visit_parameter(&mut self, node: &'a Parameter) {
        self.mark(node);
        visitor::walk_parameter(self, node);
    }

    fn visit_keyword(&mut self, node: &'a Keyword) {
        self.mark(node);
        visitor::walk_keyword(self, node);
    }

    fn visit_alias(&mut self, node: &'a Alias) {
        self.mark(node);
        visitor::walk_alias(self, node);
    }

    fn visit_pattern(&mut self, node: &'a Pattern) {
        self.mark(node);
        visitor::walk_pattern(self, node);
    }

    fn visit_type_param(&mut self, node: &'a TypeParam) {
        self.mark(node);
        visitor::walk_type_param(self, node);
    }

    fn visit_interpolated_string_element(&mut self, node: &'a InterpolatedStringElement) {
        // A literal that decodes to nothing is no node in CPython (here: inside
        // format specs, which Ruff's walk visits element by element).
        if let InterpolatedStringElement::Literal(l) = node
            && l.value.is_empty()
        {
            return;
        }
        self.mark(node);
        // CPython gives a format spec its own `JoinedStr` node, even an empty
        // one (`f"{x:}"`); Ruff's walk only visits the spec's elements.
        if let InterpolatedStringElement::Interpolation(field) = node
            && let Some(spec) = &field.format_spec
        {
            self.mark(spec.as_ref());
        }
        visitor::walk_interpolated_string_element(self, node);
    }
}

#[cfg(test)]
mod tests {
    use super::{Options, Rank, analyze};

    #[test]
    fn ranks_match_radon() {
        let cases = [
            (0.0, "A"),
            (1.0, "A"),
            (5.0, "A"),
            (5.5, "B"),
            (6.0, "B"),
            (10.0, "B"),
            (11.0, "C"),
            (20.0, "C"),
            (21.0, "D"),
            (30.0, "D"),
            (31.0, "E"),
            (40.0, "E"),
            (41.0, "F"),
            (100.0, "F"),
        ];
        for (score, want) in cases {
            assert_eq!(Rank::of(score).as_str(), want, "score {score}");
        }
    }

    fn scores(src: &str) -> Vec<(String, u32)> {
        analyze(src, &Options::default())
            .unwrap()
            .iter()
            .map(|b| (b.fullname().to_string(), b.complexity()))
            .collect()
    }

    #[test]
    fn decision_points() {
        let src = "\
def f(a, b):
    if a and b or a:
        pass
    elif b:
        pass
    for x in a:
        pass
    else:
        pass
    return [y for y in b if y if not y]
";
        // 1 + if + 2 boolop + elif + for + for-else + comprehension + 2 ifs
        assert_eq!(scores(src), [("f".to_owned(), 10)]);
    }

    #[test]
    fn match_default_case_is_free() {
        let src = "def f(x):\n    match x:\n        case 1: pass\n        case 2: pass\n        case _: pass\n";
        assert_eq!(scores(src), [("f".to_owned(), 3)]);
    }

    #[test]
    fn try_star_adds_nothing() {
        let src = "def f():\n    try:\n        pass\n    except* ValueError:\n        pass\n";
        assert_eq!(scores(src), [("f".to_owned(), 1)]);
    }

    #[test]
    fn decorated_def_is_placed_at_keyword() {
        let blocks = analyze(
            "@a\n# note\n@b(\n  1)\nasync def f(): pass\n",
            &Options::default(),
        )
        .unwrap();
        assert_eq!((blocks[0].lineno(), blocks[0].col_offset()), (5, 0));
    }

    #[test]
    fn class_score_averages_methods() {
        let src = "\
class C:
    def a(self):
        if x: pass
    def b(self):
        pass
";
        // real = 1 + 2 + 1 = 4; 4 / 2 methods = 2, plus 1 for having several.
        assert_eq!(scores(src)[0], ("C".to_owned(), 3));
    }
}
