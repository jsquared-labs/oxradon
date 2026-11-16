//! radon's configuration files: defaults for its options from a `[radon]`
//! section (`radon/cli/__init__.py`, `FileConfig`).
//!
//! radon builds one `configparser.ConfigParser()` (CPython 3.14 defaults:
//! strict, `BasicInterpolation`, `=`/`:` delimiters, `#`/`;` full-line
//! comments) from, in order, later values winning:
//!
//! 1. `$RADONCFG`, then `./radon.cfg`, if they exist (`read_file`): any open,
//!    decode or parse failure is fatal;
//! 2. `./pyproject.toml`'s `[tool]` table (`read_dict`): a TOML syntax error is
//!    fatal, any other problem (missing, unreadable, not UTF-8, no `tool`)
//!    means no TOML config;
//! 3. `./setup.cfg`, then `~/.radon.cfg` (`read`): files that cannot be opened
//!    are skipped; decode and parse failures are fatal.
//!
//! radon does this while importing `radon.cli`, outside `main`'s error
//! handling, and evaluates every command's keys there; so any failure ends
//! the run with a traceback and exit status 1, before arguments are parsed.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::pyfmt::{
    os_string_from_bytes, py_isspace, py_lower, py_lstrip, py_repr, py_rstrip, py_strip,
};
use crate::source::os_error;
use crate::tomllib;

/// The section radon reads.
const SECTION: &str = "radon";
/// `configparser.DEFAULTSECT`.
const DEFAULT_SECTION: &str = "DEFAULT";
/// `configparser.MAX_INTERPOLATION_DEPTH`.
const MAX_INTERPOLATION_DEPTH: usize = 10;
/// Nesting past which radon's `str()` of a TOML value exhausts CPython's
/// native stack (`RecursionError: Stack overflow ... while getting the repr`).
/// An approximation, not a port: the real boundary depends on how CPython was
/// built and, for some builds, on `RLIMIT_STACK`. Measured on macOS arm64: a
/// standalone CPython 3.14.10 works at 69,680 nested tables and fails at
/// 69,760 with the limit at 16 MiB (34,720 and 34,800 at 8 MiB), and
/// Homebrew's CPython 3.14.8 works at 69,000 and fails at 70,000 with the
/// limit at either. A documented divergence.
const MAX_RENDER_DEPTH: usize = 69_700;

/// The defaults radon takes from configuration for the commands oxradon
/// implements: `None`, or `false`, where nothing is configured.
#[derive(Debug, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one bool per radon option
pub(crate) struct Config {
    pub(crate) min: Option<String>,
    pub(crate) max: Option<String>,
    pub(crate) order: Option<String>,
    pub(crate) exclude: Option<String>,
    pub(crate) ignore: Option<String>,
    pub(crate) output_file: Option<String>,
    pub(crate) show_complexity: bool,
    pub(crate) average: bool,
    pub(crate) total_average: bool,
    pub(crate) no_assert: bool,
    pub(crate) show_closures: bool,
    /// `hal`'s `functions`.
    pub(crate) functions: bool,
    /// `mi`'s `mi_min`.
    pub(crate) mi_min: Option<String>,
    /// `mi`'s `mi_max`.
    pub(crate) mi_max: Option<String>,
    /// `mi`'s `multi`: `None` when absent, which `mi` reads as radon's
    /// default, `True`.
    pub(crate) multi: Option<bool>,
    /// `mi`'s `show_mi`.
    pub(crate) show_mi: bool,
}

/// Read every configuration source and evaluate radon's keys.
///
/// This runs on the caller's stack. `tomllib` parses within a bounded depth,
/// and a deeply nested TOML value is rendered and dropped without recursion,
/// so a main thread's stack is ample (see `tomllib::loads`).
///
/// # Errors
/// The failure that makes radon exit with status 1, roughly as Python words it.
pub(crate) fn load() -> Result<Config, String> {
    let mut parser = Parser::default();
    if let Some(path) = std::env::var_os("RADONCFG")
        && Path::new(&path).exists()
    {
        parser.read_file(Path::new(&path))?;
    }
    if Path::new("radon.cfg").exists() {
        parser.read_file(Path::new("radon.cfg"))?;
    }
    if let Some(tool) = pyproject_tool()? {
        parser.read_dict(&tool)?;
    }
    for path in [PathBuf::from("setup.cfg"), home_config()] {
        parser.read_if_openable(&path)?;
    }
    parser.config()
}

/// `FileConfig.toml_config`: the `[tool]` value of `./pyproject.toml`, or
/// `None` where radon ignores the file.
fn pyproject_tool() -> Result<Option<tomllib::Value>, String> {
    let Ok(bytes) = fs::read("pyproject.toml") else {
        return Ok(None);
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return Ok(None); // `UnicodeDecodeError` is not a `TOMLDecodeError`
    };
    let table = match tomllib::loads(&text) {
        Ok(table) => table,
        Err(tomllib::LoadError::Decode(message)) => {
            return Err(format!("tomllib.TOMLDecodeError: {message}"));
        }
        Err(tomllib::LoadError::Discard(_)) => return Ok(None),
    };
    Ok(table
        .0
        .into_iter()
        .find(|(key, _)| key == "tool")
        .map(|(_, value)| value))
}

/// `os.path.expanduser('~/.radon.cfg')`.
fn home_config() -> PathBuf {
    let home =
        std::env::var_os("HOME").or_else(|| std::env::home_dir().map(PathBuf::into_os_string));
    let Some(home) = home else {
        return PathBuf::from("~/.radon.cfg");
    };
    let mut bytes = home.into_encoded_bytes();
    while bytes.last() == Some(&b'/') {
        bytes.pop();
    }
    bytes.extend_from_slice(b"/.radon.cfg");
    PathBuf::from(os_string_from_bytes(bytes))
}

/// An option's value while a file is read (its lines) and after (joined).
#[derive(Debug, Clone)]
enum Value {
    Lines(Vec<String>),
    Text(String),
}

/// Options in insertion order, like a Python dict.
#[derive(Debug, Default)]
struct Options(Vec<(String, Value)>);

impl Options {
    fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.0.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// `options[key] = value`: a new key goes last, an old one keeps its place.
    fn set(&mut self, key: String, value: Value) {
        match self.get_mut(&key) {
            Some(slot) => *slot = value,
            None => self.0.push((key, value)),
        }
    }

    fn text(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Value::Text(text) => Some(text),
            Value::Lines(_) => None,
        }
    }
}

/// Which options a line goes into.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Current {
    Defaults,
    Section(usize),
}

/// `configparser.ConfigParser()` with its default settings.
#[derive(Default)]
struct Parser {
    sections: Vec<(String, Options)>,
    defaults: Options,
}

impl Parser {
    fn section_index(&self, name: &str) -> Option<usize> {
        self.sections.iter().position(|(n, _)| n == name)
    }

    fn options_mut(&mut self, current: Current) -> &mut Options {
        match current {
            Current::Defaults => &mut self.defaults,
            Current::Section(i) => &mut self.sections[i].1,
        }
    }

    /// The value lines of the option being read, if any (an option needs a
    /// non-empty name to take continuation or blank lines).
    fn open_lines(
        &mut self,
        current: Option<Current>,
        option: Option<&str>,
    ) -> Option<&mut Vec<String>> {
        let (current, name) = (current?, option.filter(|n| !n.is_empty())?);
        match self.options_mut(current).get_mut(name)? {
            Value::Lines(lines) => Some(lines),
            Value::Text(_) => None,
        }
    }

    /// `read_file(open(path))`: every failure is fatal.
    fn read_file(&mut self, path: &Path) -> Result<(), String> {
        let bytes = fs::read(path).map_err(|e| os_error(&e, path))?;
        let text = decode(bytes)?;
        self.read_text(&text, path)
    }

    /// `read([path])`: a file that cannot be opened or read is skipped.
    fn read_if_openable(&mut self, path: &Path) -> Result<(), String> {
        let Ok(bytes) = fs::read(path) else {
            return Ok(());
        };
        let text = decode(bytes)?;
        self.read_text(&text, path)
    }

    /// `RawConfigParser._read`: parse, then join multi-line values.
    fn read_text(&mut self, text: &str, path: &Path) -> Result<(), String> {
        let result = self.read_inner(text, path);
        self.join_multiline_values();
        result
    }

    /// `RawConfigParser._read_inner` and its `_handle_*` helpers.
    fn read_inner(&mut self, text: &str, path: &Path) -> Result<(), String> {
        let source = py_repr(path);
        let mut current: Option<Current> = None;
        let mut section_name = String::new();
        let mut option: Option<String> = None;
        let mut indent_level = 0;
        let mut sections_added: HashSet<String> = HashSet::new();
        let mut options_added: HashSet<(String, String)> = HashSet::new();
        let mut errors: Vec<usize> = Vec::new();

        for (index, line) in universal_lines(text).enumerate() {
            let lineno = index + 1;
            let trimmed = py_strip(&line);
            // `_CommentSpec.strip`: `#` or `;` starting the trimmed line.
            let clean = if trimmed.starts_with(['#', ';']) {
                ""
            } else {
                trimmed
            };
            let has_comments = trimmed != clean;
            if clean.is_empty() {
                if !has_comments && let Some(lines) = self.open_lines(current, option.as_deref()) {
                    lines.push(String::new());
                }
                continue;
            }
            let cur_indent = line.chars().position(|c| !py_isspace(c)).unwrap_or(0);

            // A continuation line?
            if cur_indent > indent_level
                && let Some(lines) = self.open_lines(current, option.as_deref())
            {
                lines.push(clean.to_owned());
                continue;
            }

            indent_level = cur_indent;
            if let Some(header) = section_header(clean) {
                header.clone_into(&mut section_name);
                if let Some(i) = self.section_index(header) {
                    if sections_added.contains(header) {
                        return Err(format!(
                            "configparser.DuplicateSectionError: While reading from {} \
                             [line {lineno:2}]: section {} already exists",
                            source,
                            py_repr(header)
                        ));
                    }
                    current = Some(Current::Section(i));
                    sections_added.insert(section_name.clone());
                } else if header == DEFAULT_SECTION {
                    current = Some(Current::Defaults);
                } else {
                    self.sections
                        .push((section_name.clone(), Options::default()));
                    current = Some(Current::Section(self.sections.len() - 1));
                    sections_added.insert(section_name.clone());
                }
                option = None;
                continue;
            }
            let Some(target) = current else {
                return Err(format!(
                    "configparser.MissingSectionHeaderError: File contains no section headers.\n\
                     file: {}, line: {lineno}\n{}",
                    source,
                    py_repr(&line)
                ));
            };
            let Some((raw_name, value)) = option_line(clean) else {
                errors.push(lineno);
                continue; // the previous option stays current
            };
            if raw_name.is_empty() {
                errors.push(lineno);
            }
            let key = py_lower(py_rstrip(raw_name));
            if !options_added.insert((section_name.clone(), key.clone())) {
                return Err(format!(
                    "configparser.DuplicateOptionError: While reading from {} [line {lineno:2}]: \
                     option {} in section {} already exists",
                    source,
                    py_repr(&key),
                    py_repr(&section_name)
                ));
            }
            let value = py_strip(value).to_owned();
            self.options_mut(target)
                .set(key.clone(), Value::Lines(vec![value]));
            option = Some(key);
        }
        if let Some(lineno) = errors.first() {
            return Err(format!(
                "configparser.ParsingError: Source contains parsing errors: {source} \
                 [line {lineno:2}]"
            ));
        }
        Ok(())
    }

    /// `_join_multiline_values`: lines joined with `\n`, then `rstrip`.
    fn join_multiline_values(&mut self) {
        let all =
            std::iter::once(&mut self.defaults).chain(self.sections.iter_mut().map(|(_, o)| o));
        for options in all {
            for (_, value) in &mut options.0 {
                if let Value::Lines(lines) = value {
                    *value = Value::Text(py_rstrip(&lines.join("\n")).to_owned());
                }
            }
        }
    }

    /// `RawConfigParser.read_dict(pyproject['tool'])`.
    fn read_dict(&mut self, tool: &tomllib::Value) -> Result<(), String> {
        let tomllib::Value::Table(sections) = tool else {
            return Err(no_items(tool));
        };
        let mut added: HashSet<(String, String)> = HashSet::new();
        for (section, keys) in &sections.0 {
            let tomllib::Value::Table(keys) = keys else {
                return Err(no_items(keys));
            };
            // `add_section` raises for `DEFAULT` and for a known section;
            // `read_dict` ignores both (a repeat within this dict cannot
            // occur). `set()` then writes to the defaults for `DEFAULT` or a
            // falsy name.
            let target = if section.is_empty() || section == DEFAULT_SECTION {
                if section.is_empty() && self.section_index(section).is_none() {
                    self.sections.push((section.clone(), Options::default()));
                }
                Current::Defaults
            } else if let Some(i) = self.section_index(section) {
                Current::Section(i)
            } else {
                self.sections.push((section.clone(), Options::default()));
                Current::Section(self.sections.len() - 1)
            };
            for (key, value) in &keys.0 {
                let key = py_lower(key);
                let value = render(value)?;
                if !added.insert((section.clone(), key.clone())) {
                    return Err(format!(
                        "configparser.DuplicateOptionError: While reading from '<dict>': \
                         option {} in section {} already exists",
                        py_repr(&key),
                        py_repr(section)
                    ));
                }
                if !value.is_empty() {
                    check_interpolation_syntax(&value)?;
                }
                self.options_mut(target).set(key, Value::Text(value));
            }
        }
        Ok(())
    }

    /// `has_option('radon', key)`: needs a `radon` section, then looks in it
    /// and in the defaults.
    fn has_option(&self, key: &str) -> bool {
        self.section_index(SECTION).is_some_and(|i| {
            self.sections[i].1.get(key).is_some() || self.defaults.get(key).is_some()
        })
    }

    /// The raw value of `key` in the `radon` section, falling back to defaults.
    fn raw(&self, key: &str) -> Option<&str> {
        let i = self.section_index(SECTION)?;
        self.sections[i]
            .1
            .text(key)
            .or_else(|| self.defaults.text(key))
    }

    /// `get('radon', key)` with `BasicInterpolation`.
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        if !self.has_option(key) {
            return Ok(None);
        }
        let raw = self.raw(key).unwrap_or_default();
        let mut out = String::new();
        self.interpolate(key, &mut out, raw, 1)?;
        Ok(Some(out))
    }

    /// `BasicInterpolation._interpolate_some`.
    fn interpolate(
        &self,
        option: &str,
        out: &mut String,
        rest: &str,
        depth: usize,
    ) -> Result<(), String> {
        if depth > MAX_INTERPOLATION_DEPTH {
            return Err(format!(
                "configparser.InterpolationDepthError: Recursion limit exceeded in value \
                 substitution: option {} in section 'radon' contains an interpolation key \
                 which cannot be substituted in {MAX_INTERPOLATION_DEPTH} steps.",
                py_repr(option)
            ));
        }
        let mut rest = rest;
        while !rest.is_empty() {
            let Some(p) = rest.find('%') else {
                out.push_str(rest);
                return Ok(());
            };
            out.push_str(&rest[..p]);
            rest = &rest[p..];
            match rest[1..].chars().next() {
                Some('%') => {
                    out.push('%');
                    rest = &rest[2..];
                }
                Some('(') => {
                    let Some(m) = KEY_REFERENCE
                        .captures(rest)
                        .filter(|m| m.get(0).is_some_and(|m| m.start() == 0))
                    else {
                        return Err(format!(
                            "configparser.InterpolationSyntaxError: bad interpolation variable \
                             reference {}",
                            py_repr(rest)
                        ));
                    };
                    let var = py_lower(&m[1]);
                    rest = &rest[m[0].len()..];
                    let Some(value) = self.raw(&var) else {
                        return Err(format!(
                            "configparser.InterpolationMissingOptionError: Bad value \
                             substitution: option {} in section 'radon' contains an \
                             interpolation key {} which is not a valid option name.",
                            py_repr(option),
                            py_repr(&var)
                        ));
                    };
                    if value.contains('%') {
                        self.interpolate(option, out, value, depth + 1)?;
                    } else {
                        out.push_str(value);
                    }
                }
                _ => {
                    return Err(format!(
                        "configparser.InterpolationSyntaxError: '%' must be followed by '%' \
                         or '(', found: {}",
                        py_repr(rest)
                    ));
                }
            }
        }
        Ok(())
    }

    /// `getboolean('radon', key)`, or `false` when the key is absent.
    fn get_bool(&self, key: &str) -> Result<bool, String> {
        Ok(self.get_bool_option(key)?.unwrap_or(false))
    }

    /// `getboolean('radon', key)`, or `None` when the key is absent.
    fn get_bool_option(&self, key: &str) -> Result<Option<bool>, String> {
        let Some(value) = self.get(key)? else {
            return Ok(None);
        };
        match py_lower(&value).as_str() {
            "1" | "yes" | "true" | "on" => Ok(Some(true)),
            "0" | "no" | "false" | "off" => Ok(Some(false)),
            _ => Err(format!("ValueError: Not a boolean: {value}")),
        }
    }

    /// Evaluate every key radon reads at import (all four commands), in
    /// radon's order, keeping those oxradon uses.
    fn config(&self) -> Result<Config, String> {
        let mut config = Config {
            min: self.get("cc_min")?,
            max: self.get("cc_max")?,
            show_complexity: self.get_bool("show_complexity")?,
            average: self.get_bool("average")?,
            exclude: self.get("exclude")?,
            ignore: self.get("ignore")?,
            order: self.get("order")?,
            no_assert: self.get_bool("no_assert")?,
            show_closures: self.get_bool("show_closures")?,
            total_average: self.get_bool("total_average")?,
            output_file: self.get("output_file")?,
            ..Config::default()
        };
        // `cc`'s ipynb options (also `raw`'s), then `mi`'s keys, then `hal`'s:
        // radon's definition order, which decides the error a broken
        // configuration reports first.
        for key in ["include_ipynb", "ipynb_cells"] {
            self.get_bool(key)?;
        }
        config.mi_min = self.get("mi_min")?;
        config.mi_max = self.get("mi_max")?;
        config.multi = self.get_bool_option("multi")?;
        config.show_mi = self.get_bool("show_mi")?;
        config.functions = self.get_bool("functions")?;
        Ok(config)
    }
}

/// `%(name)s`, `BasicInterpolation._KEYCRE`.
static KEY_REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"%\(([^)]+)\)s").expect("valid pattern"));

/// `BasicInterpolation.before_set`.
fn check_interpolation_syntax(value: &str) -> Result<(), String> {
    let without_escapes = value.replace("%%", "");
    let rest = KEY_REFERENCE.replace_all(&without_escapes, "");
    match rest.find('%') {
        Some(position) => Err(format!(
            "ValueError: invalid interpolation syntax in {} at position {}",
            py_repr(value),
            rest[..position].chars().count()
        )),
        None => Ok(()),
    }
}

/// `str(value)` of a TOML value, failing as CPython does: past a fixed depth
/// that stands in for its native-stack overflow ([`MAX_RENDER_DEPTH`]), or on
/// an integer too long to print.
fn render(value: &tomllib::Value) -> Result<String, String> {
    if depth(value) > MAX_RENDER_DEPTH {
        return Err("RecursionError: Stack overflow while getting the repr of an object".into());
    }
    value
        .py_str()
        .map_err(|message| format!("ValueError: {message}"))
}

/// The nesting depth of a TOML value (1 for a scalar), without recursion.
fn depth(value: &tomllib::Value) -> usize {
    let mut deepest = 0;
    let mut stack = vec![(value, 1)];
    while let Some((value, level)) = stack.pop() {
        deepest = deepest.max(level);
        match value {
            tomllib::Value::Array(items) => stack.extend(items.iter().map(|v| (v, level + 1))),
            tomllib::Value::Table(table) => {
                stack.extend(table.0.iter().map(|(_, v)| (v, level + 1)));
            }
            _ => {}
        }
    }
    deepest
}

/// `AttributeError` from calling `.items()` on a non-table.
fn no_items(value: &tomllib::Value) -> String {
    let type_name = match value {
        tomllib::Value::String(_) => "str",
        tomllib::Value::Integer(_) => "int",
        tomllib::Value::Float(_) => "float",
        tomllib::Value::Boolean(_) => "bool",
        tomllib::Value::Array(_) => "list",
        tomllib::Value::Table(_) => "dict",
        tomllib::Value::DateTime(_) => "datetime.datetime",
        tomllib::Value::Date(_) => "datetime.date",
        tomllib::Value::Time(_) => "datetime.time",
    };
    format!("AttributeError: '{type_name}' object has no attribute 'items'")
}

/// Text read in text mode: strict UTF-8 (a BOM stays, and then breaks the
/// first section header).
fn decode(bytes: Vec<u8>) -> Result<String, String> {
    crate::source::Encoding::Utf8
        .decode(bytes)
        .map_err(|message| format!("UnicodeDecodeError: {message}"))
}

/// Lines as a text-mode file yields them: `\r\n` and `\r` become `\n`, and
/// each line keeps its `\n`.
fn universal_lines(text: &str) -> impl Iterator<Item = String> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    text.split_inclusive('\n')
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .into_iter()
}

/// `SECTCRE.match(clean)`: `\[(?P<header>.+)\]` from the start, greedy to the
/// last `]` that leaves a non-empty name.
fn section_header(clean: &str) -> Option<&str> {
    let inner = clean.strip_prefix('[')?;
    let end = inner.rfind(']')?;
    (end > 0).then(|| &inner[..end])
}

/// `OPTCRE.match(clean)`: the name is everything before the first `=` or
/// `:` (less trailing space), the value everything after (less leading
/// space). `clean` has no leading space, so any text before a delimiter
/// matches the name pattern.
fn option_line(clean: &str) -> Option<(&str, &str)> {
    let at = clean.find(['=', ':'])?;
    Some((py_rstrip(&clean[..at]), py_lstrip(&clean[at + 1..])))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Parser, check_interpolation_syntax, option_line, section_header};

    fn parse(text: &str) -> Result<Parser, String> {
        let mut parser = Parser::default();
        parser.read_text(text, Path::new("radon.cfg"))?;
        Ok(parser)
    }

    fn get(text: &str, key: &str) -> Result<Option<String>, String> {
        parse(text)?.get(key)
    }

    #[test]
    fn headers_are_greedy_and_anchored_at_the_start_only() {
        assert_eq!(section_header("[radon]"), Some("radon"));
        assert_eq!(section_header("[radon] ignored"), Some("radon"));
        assert_eq!(section_header("[radon]x]"), Some("radon]x"));
        assert_eq!(section_header("[]"), None);
        assert_eq!(section_header("[]]"), Some("]"));
        assert_eq!(section_header(" [radon]"), None);
    }

    #[test]
    fn options_split_at_the_first_delimiter() {
        assert_eq!(option_line("a = b"), Some(("a", "b")));
        assert_eq!(option_line("a b: c = d"), Some(("a b", "c = d")));
        assert_eq!(option_line("= b"), Some(("", "b")));
        assert_eq!(option_line("no delimiter"), None);
    }

    #[test]
    fn values_continue_by_indent_and_keep_blank_lines() {
        let text = "[radon]\nexclude =\n  a,\n\n  b\n# c\nnext = 1\n";
        assert_eq!(get(text, "exclude").unwrap().as_deref(), Some("\na,\n\nb"));
        assert_eq!(get(text, "next").unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn comments_are_whole_lines_only() {
        assert_eq!(
            get("[radon]\ncc_min = B ; c\n", "cc_min")
                .unwrap()
                .as_deref(),
            Some("B ; c")
        );
        assert_eq!(
            get("[radon]\n  ; c\ncc_min = B\n", "cc_min")
                .unwrap()
                .as_deref(),
            Some("B")
        );
    }

    #[test]
    fn keys_fold_and_sections_do_not() {
        assert_eq!(
            get("[radon]\nCC_Min = B\n", "cc_min").unwrap().as_deref(),
            Some("B")
        );
        assert_eq!(get("[RADON]\ncc_min = B\n", "cc_min").unwrap(), None);
    }

    #[test]
    fn defaults_apply_only_with_a_radon_section() {
        assert_eq!(
            get("[DEFAULT]\ncc_min = C\n[radon]\n", "cc_min")
                .unwrap()
                .as_deref(),
            Some("C")
        );
        assert_eq!(get("[DEFAULT]\ncc_min = C\n", "cc_min").unwrap(), None);
        // A repeated `[DEFAULT]` header is fine; a repeated option is not.
        let ok = "[DEFAULT]\ncc_min=B\n[DEFAULT]\nshow_complexity=yes\n[radon]\n";
        assert!(parse(ok).unwrap().get_bool("show_complexity").unwrap());
        assert!(parse("[DEFAULT]\na=1\n[DEFAULT]\na=2\n").is_err());
    }

    #[test]
    fn strict_duplicates_and_parse_errors_are_fatal() {
        assert!(parse("[radon]\na = 1\na = 2\n").is_err());
        assert!(parse("[radon]\n[radon]\n").is_err());
        assert!(parse("[radon]\nnot an option\n").is_err());
        assert!(parse("cc_min = B\n").is_err());
        assert!(parse("\u{feff}[radon]\n").is_err());
    }

    #[test]
    fn sections_merge_across_files() {
        let mut parser = parse("[radon]\ncc_min = B\n").unwrap();
        parser
            .read_text("[radon]\ncc_max = C\n", Path::new("setup.cfg"))
            .unwrap();
        assert_eq!(parser.get("cc_min").unwrap().as_deref(), Some("B"));
        assert_eq!(parser.get("cc_max").unwrap().as_deref(), Some("C"));
    }

    #[test]
    fn interpolation_follows_basic_interpolation() {
        let text = "[radon]\nlevel = C\ncc_min = %(LEVEL)s\npct = 100%%\nbad = 50%\nmissing = %(x)s\n\
                    deep = %(deep)s\nempty = %()s\n";
        assert_eq!(get(text, "cc_min").unwrap().as_deref(), Some("C"));
        assert_eq!(get(text, "pct").unwrap().as_deref(), Some("100%"));
        for key in ["bad", "missing", "deep", "empty"] {
            assert!(get(text, key).is_err(), "{key}");
        }
    }

    #[test]
    fn keys_are_evaluated_in_radons_order() {
        // radon: `mi_min` (from `mi`) is read before `multi`, and `multi`
        // before `hal`'s `functions`; the first failure is the one reported.
        let error = |text: &str| parse(text).unwrap().config().unwrap_err();
        let both = "[radon]\nmulti = maybe\nmi_min = %(nope)s\n";
        assert!(!error(both).contains("Not a boolean"), "{}", error(both));
        let bools = "[radon]\nfunctions = second\nmulti = first\n";
        assert!(error(bools).ends_with("first"), "{}", error(bools));
    }

    #[test]
    fn booleans_take_configparser_spellings() {
        let parser = parse("[radon]\na = YES\nb = off\nc = maybe\n").unwrap();
        assert!(parser.get_bool("a").unwrap());
        assert!(!parser.get_bool("b").unwrap());
        assert!(parser.get_bool("c").is_err());
        assert!(!parser.get_bool("absent").unwrap());
    }

    #[test]
    fn before_set_rejects_stray_percent_signs() {
        assert!(check_interpolation_syntax("%Y-%m-%d").is_err());
        assert!(check_interpolation_syntax("%(name)s and 100%%").is_ok());
        assert!(check_interpolation_syntax("{'f': '%(a)s'}").is_ok());
    }
}
