//! Python's `fnmatch.fnmatch` on POSIX, as a precompiled set of patterns.
//!
//! The translation is a port of CPython's `fnmatch.translate`, so bracket
//! expressions, `!` negation and range edge cases behave exactly as in radon.
//! Characters are emitted as `\x{..}` escapes, which sidesteps the differences
//! between Python's and Rust's regex escaping rules.

use regex::{RegexSet, RegexSetBuilder};

/// The compiled-program limit. `regex`'s default (10 MiB) rejects patterns
/// Python's `fnmatch` handles, such as 50,000 `?`; patterns come from the
/// user's own command line and config files, so allow far more. Larger still
/// remains a documented divergence. The limit caps memory; it is not reserved.
const SIZE_LIMIT: usize = 1 << 30;

/// Patterns compiled once and matched against many names.
pub(crate) struct PatternSet(RegexSet);

impl PatternSet {
    /// Compile shell-style patterns (`*`, `?`, `[seq]`, `[!seq]`).
    ///
    /// # Errors
    /// Fails only if a pattern exceeds [`SIZE_LIMIT`] once compiled.
    pub(crate) fn new<'p>(
        patterns: impl IntoIterator<Item = &'p str>,
    ) -> Result<Self, regex::Error> {
        RegexSetBuilder::new(patterns.into_iter().map(translate))
            .size_limit(SIZE_LIMIT)
            .build()
            .map(Self)
    }

    /// Whether `name` matches any pattern in the set (`fnmatch.fnmatch`).
    pub(crate) fn is_match(&self, name: &str) -> bool {
        self.0.is_match(name)
    }
}

#[derive(Clone, Copy)]
enum Item {
    Char(char),
    Range(char, char),
}

/// Translate a shell pattern into an anchored regex, following CPython.
fn translate(pattern: &str) -> String {
    let p: Vec<char> = pattern.chars().collect();
    let mut re = String::from(r"(?s)\A(?:");
    let mut i = 0;
    while i < p.len() {
        let c = p[i];
        i += 1;
        match c {
            '*' => {
                re.push_str(".*");
                while p.get(i) == Some(&'*') {
                    i += 1;
                }
            }
            '?' => re.push('.'),
            '[' => match bracket(&p, i) {
                Some((class, next)) => {
                    re.push_str(&class);
                    i = next;
                }
                None => re.push_str(r"\["),
            },
            c => push_char(&mut re, c),
        }
    }
    re.push_str(r")\z");
    re
}

/// Translate the bracket expression whose body starts at `start` (just past
/// `[`). Returns the regex and the index after `]`, or `None` if unclosed.
fn bracket(p: &[char], start: usize) -> Option<(String, usize)> {
    let mut j = start;
    if p.get(j) == Some(&'!') {
        j += 1;
    }
    if p.get(j) == Some(&']') {
        j += 1;
    }
    while j < p.len() && p[j] != ']' {
        j += 1;
    }
    if j >= p.len() {
        return None;
    }

    let mut items = items(&chunks(p, start, j));
    let negate = matches!(items.first(), Some(Item::Char('!')));
    if negate {
        items.remove(0);
    }
    let re = match (items.is_empty(), negate) {
        // `[!]`-like: a negated empty set matches any character.
        (true, true) => ".".to_owned(),
        // An empty set (all ranges were reversed) never matches.
        (true, false) => r"[^\x00-\x{10FFFF}]".to_owned(),
        _ => {
            let mut class = String::from(if negate { "[^" } else { "[" });
            for item in items {
                match item {
                    Item::Char(c) => push_char(&mut class, c),
                    Item::Range(lo, hi) => {
                        push_char(&mut class, lo);
                        class.push('-');
                        push_char(&mut class, hi);
                    }
                }
            }
            class.push(']');
            class
        }
    };
    Some((re, j + 1))
}

/// Split a bracket body at the hyphens that form ranges, as CPython does:
/// the chunks are joined back with range hyphens between them.
fn chunks(p: &[char], start: usize, end: usize) -> Vec<Vec<char>> {
    let body = &p[start..end];
    if !body.contains(&'-') {
        return vec![body.to_vec()];
    }
    let mut chunks: Vec<Vec<char>> = Vec::new();
    let mut i = start;
    let mut k = if p[start] == '!' {
        start + 2
    } else {
        start + 1
    };
    while let Some(h) = (k..end).find(|&x| p[x] == '-') {
        chunks.push(p[i..h].to_vec());
        i = h + 1;
        k = h + 3;
    }
    match &p[i..end] {
        [] => chunks.last_mut().expect("a hyphen was found").push('-'),
        tail => chunks.push(tail.to_vec()),
    }
    // Remove empty (reversed) ranges by merging their neighbours.
    for k in (1..chunks.len()).rev() {
        if let (Some(&lo), Some(&hi)) = (chunks[k - 1].last(), chunks[k].first())
            && lo > hi
        {
            let next = chunks.remove(k);
            chunks[k - 1].pop();
            chunks[k - 1].extend_from_slice(&next[1..]);
        }
    }
    chunks
}

/// Turn chunks into class items: a range spans the last char of one chunk and
/// the first char of the next; every other char is literal.
fn items(chunks: &[Vec<char>]) -> Vec<Item> {
    let mut items = Vec::new();
    for (idx, chunk) in chunks.iter().enumerate() {
        let mut rest = chunk.as_slice();
        if idx > 0
            && let Some((&hi, tail)) = rest.split_first()
        {
            match items.pop() {
                Some(Item::Char(lo)) => items.push(Item::Range(lo, hi)),
                other => {
                    items.extend(other);
                    items.push(Item::Char('-'));
                    items.push(Item::Char(hi));
                }
            }
            rest = tail;
        }
        items.extend(rest.iter().copied().map(Item::Char));
    }
    items
}

fn push_char(re: &mut String, c: char) {
    use std::fmt::Write;
    let _ = write!(re, r"\x{{{:X}}}", u32::from(c));
}

#[cfg(test)]
mod tests {
    use super::PatternSet;

    fn fnmatch(name: &str, pattern: &str) -> bool {
        PatternSet::new([pattern]).unwrap().is_match(name)
    }

    #[test]
    fn wildcards() {
        assert!(fnmatch(".git", ".*"));
        assert!(!fnmatch("git", ".*"));
        assert!(fnmatch("a/b/test_x.py", "*test_*"), "`*` crosses `/`");
        assert!(fnmatch("abc", "a?c"));
        assert!(fnmatch("a\nc", "a?c"), "`?` matches newlines");
        assert!(!fnmatch("abcd", "a?c"), "patterns are anchored");
    }

    #[test]
    fn brackets() {
        assert!(fnmatch("x1.py", "x[0-9].py"));
        assert!(!fnmatch("xa.py", "x[0-9].py"));
        assert!(fnmatch("xa.py", "x[!0-9].py"));
        assert!(fnmatch("]", "[]]"), "leading `]` is literal");
        assert!(fnmatch("-", "[a-]"), "trailing `-` is literal");
        assert!(fnmatch("[x", "[x"), "unclosed `[` is literal");
        assert!(fnmatch("^", "[^]"), "`^` is literal, not negation");
        assert!(fnmatch("\\", "[\\]"), "backslash is literal");
    }

    /// Cases recorded from Python's `fnmatch.fnmatchcase` by
    /// `scripts/update_golden.py`.
    #[test]
    fn matches_python() {
        let cases: Vec<(String, String, bool)> =
            serde_json::from_str(include_str!("../tests/golden/fnmatch.json")).unwrap();
        assert!(cases.len() > 1000);
        let failures: Vec<String> = cases
            .iter()
            .filter(|(pattern, name, expected)| fnmatch(name, pattern) != *expected)
            .map(|(pattern, name, expected)| format!("{name:?} vs {pattern:?}: python={expected}"))
            .collect();
        assert!(
            failures.is_empty(),
            "{} of {} differ:\n{}",
            failures.len(),
            cases.len(),
            failures.join("\n")
        );
    }

    #[test]
    fn reversed_ranges_follow_cpython() {
        assert!(!fnmatch("a", "[z-a]"), "an empty set never matches");
        assert!(!fnmatch("z", "[z-a]"));
        assert!(
            fnmatch("q", "[!z-a]"),
            "a negated empty set matches anything"
        );
        assert!(fnmatch("e", "[c-ad-f]"), "c-a is dropped, d-f survives");
        assert!(!fnmatch("b", "[c-ad-f]"));
    }
}
