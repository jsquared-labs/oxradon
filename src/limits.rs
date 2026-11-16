//! Limits built into CPython 3.14 that decide what radon reports. Each is
//! defined once here, for the modules that port the code enforcing it.

/// `MAXLEVEL` (`Parser/lexer/state.h`): the tokenizer's bracket stack. A
/// bracket opened at this depth is "too many nested parentheses".
pub(crate) const MAX_LEVEL: u32 = 200;

/// `MAXINDENT`: the tokenizer's indentation stack. The indent that would
/// fill its last slot is "too many levels of indentation".
pub(crate) const MAX_INDENT: usize = 100;

/// `MAXFSTRINGLEVEL`: the tokenizer's mode stack, which holds the regular
/// mode and one entry per open f-/t-string.
pub(crate) const MAX_FSTRING_LEVEL: usize = 150;

/// `MAX_EXPR_NESTING` (`Parser/lexer/lexer.c`): how deep replacement fields
/// may nest through format specs within one f-/t-string. A C `int`, as the
/// lexer compares it.
pub(crate) const MAX_EXPR_NESTING: i32 = 3;

/// The default `sys.get_int_max_str_digits()`: `int()` refuses a decimal
/// literal with more digits, in source code and in `tomllib` alike.
pub(crate) const MAX_STR_DIGITS: usize = 4300;
