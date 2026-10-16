//! model — the shared substrate of the cs_parser port.
//!
//! Python indexes `str` by *character*, slices by character and hands
//! character offsets to `re`. Rust indexes `str` by *byte*. The corpus is not
//! pure ASCII (box-drawing characters sit in the comment banners), so using
//! byte offsets would silently shift every position after the first non-ASCII
//! character. Everything in this port therefore works on `&[char]` / `Vec<char>`
//! and reaches the `regex` crate through [`Re`], which converts every match
//! back to character offsets.
//!
//! This module also holds the Python-compatible character predicates and the
//! keyword tables copied from `cs_parser.py`.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

// ---------------------------------------------------------------------------
// keyword tables (cs_parser.py)
// ---------------------------------------------------------------------------
/// `KEYWORDS` — module-level frozenset, verbatim.
pub const KEYWORDS_RAW: &str = "\
abstract as base bool break byte case catch char checked class const continue \
decimal default delegate do double else enum event explicit extern false finally \
fixed float for foreach goto if implicit in int interface internal is lock long \
namespace new null object operator out override params private protected public \
readonly ref return sbyte sealed short sizeof stackalloc static string struct \
switch this throw true try typeof uint ulong unchecked unsafe ushort using \
virtual void volatile while add alias and ascending async await by descending \
dynamic equals file from get global group init into join let managed nameof \
nint not notnull nuint on or orderby partial record remove required scoped \
select set unmanaged value var when where with yield";

/// `OPEN_KEYWORDS` — kept for parity even though the parser never reads it.
pub const OPEN_KEYWORDS_RAW: &str = "\
if for while switch catch lock using fixed checked unchecked sizeof typeof \
nameof default new return throw await yield do else try finally";

/// `ACCESS` — order matters: the first word present in a signature wins.
pub const ACCESS: [&str; 4] = ["public", "private", "protected", "internal"];

/// `MODIFIERS` — module-level frozenset, verbatim.
pub const MODIFIERS_RAW: &str = "\
public private protected internal static virtual override abstract sealed async \
partial extern unsafe new readonly const event volatile required file ref out \
in params this scoped fixed";

/// `UNITY_DEFINES` — used by server.py's `resolve_defines`, kept here for parity.
pub const UNITY_DEFINES_RAW: &str = "\
UNITY_EDITOR UNITY_EDITOR_64 UNITY_EDITOR_LINUX UNITY_EDITOR_WIN \
DEBUG TRACE UNITY_ASSERTIONS UNITY_INCLUDE_TESTS \
UNITY_5_3_OR_NEWER UNITY_5_4_OR_NEWER UNITY_5_5_OR_NEWER UNITY_5_6_OR_NEWER \
UNITY_2017_1_OR_NEWER UNITY_2017_2_OR_NEWER UNITY_2017_3_OR_NEWER \
UNITY_2017_4_OR_NEWER UNITY_2018_1_OR_NEWER UNITY_2018_2_OR_NEWER \
UNITY_2018_3_OR_NEWER UNITY_2018_4_OR_NEWER UNITY_2019_1_OR_NEWER \
UNITY_2019_2_OR_NEWER UNITY_2019_3_OR_NEWER UNITY_2019_4_OR_NEWER \
UNITY_2020_1_OR_NEWER UNITY_2020_2_OR_NEWER UNITY_2020_3_OR_NEWER \
UNITY_2021_1_OR_NEWER UNITY_2021_2_OR_NEWER UNITY_2021_3_OR_NEWER \
UNITY_2022_1_OR_NEWER UNITY_2022_2_OR_NEWER UNITY_2022_3_OR_NEWER \
CSHARP_7_OR_LATER CSHARP_7_3_OR_NEWER NET_4_6 SYSTEM_DRAWING";

fn words(raw: &'static str) -> Vec<&'static str> {
    // The tables are `'static`; splitting on whitespace mirrors `str.split()`.
    raw.split_whitespace().collect()
}

static KEYWORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();
static MODIFIERS: OnceLock<HashSet<&'static str>> = OnceLock::new();

fn keyword_set() -> &'static HashSet<&'static str> {
    KEYWORDS.get_or_init(|| words(KEYWORDS_RAW).into_iter().collect())
}

fn modifier_set() -> &'static HashSet<&'static str> {
    MODIFIERS.get_or_init(|| words(MODIFIERS_RAW).into_iter().collect())
}

/// `name in KEYWORDS`.
pub fn is_keyword(name: &str) -> bool {
    keyword_set().contains(name)
}

/// `name in MODIFIERS`.
pub fn is_modifier(name: &str) -> bool {
    modifier_set().contains(name)
}

/// The full `KEYWORDS` set (used by graph.py for the same membership test).
pub fn keywords() -> &'static HashSet<&'static str> {
    keyword_set()
}

/// The full `MODIFIERS` set.
pub fn modifiers() -> &'static HashSet<&'static str> {
    modifier_set()
}

/// The `UNITY_DEFINES` set.
pub fn unity_defines() -> HashSet<&'static str> {
    words(UNITY_DEFINES_RAW).into_iter().collect()
}

// ---------------------------------------------------------------------------
// Python character predicates
// ---------------------------------------------------------------------------
/// `ch.isspace()` — Python's set, which includes the four C1 separators
/// (U+001C..U+001F) that the Unicode `White_Space` property omits.
pub fn py_is_space(c: char) -> bool {
    matches!(c,
        '\u{09}'..='\u{0d}'
        | '\u{1c}'..='\u{1f}'
        | '\u{20}'
        | '\u{85}'
        | '\u{a0}'
        | '\u{1680}'
        | '\u{2000}'..='\u{200a}'
        | '\u{2028}'
        | '\u{2029}'
        | '\u{202f}'
        | '\u{205f}'
        | '\u{3000}')
}

/// `ch.isalnum()` — the predicate the lexer uses to collect identifiers.
pub fn py_is_alnum(c: char) -> bool {
    c.is_alphanumeric()
}

/// `[A-Za-z_]` then `\w*` — the identifier shape Python's `re` accepts.
pub fn py_is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// A `\w` character as the parser's inputs use it.
pub fn py_is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ---------------------------------------------------------------------------
// character-slice helpers
// ---------------------------------------------------------------------------
/// `''.join(chars)`.
pub fn chars_to_string(cs: &[char]) -> String {
    cs.iter().collect()
}

/// `list(s)`.
pub fn to_chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

/// `s.find(needle, start, end)` for a character slice.
pub fn find_sub(hay: &[char], needle: &[char], start: usize, end: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(start.min(end));
    }
    let end = end.min(hay.len());
    if start >= end || needle.len() > end - start {
        return None;
    }
    let last = end - needle.len();
    let mut i = start;
    while i <= last {
        if hay[i..i + needle.len()] == *needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// `s.find(needle, start)` (to the end of the slice).
pub fn find_sub_from(hay: &[char], needle: &[char], start: usize) -> Option<usize> {
    find_sub(hay, needle, start, hay.len())
}

/// `s.split(sep)` for a single-character separator, keeping empty fields.
pub fn split_on(hay: &[char], sep: char) -> Vec<Vec<char>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for &c in hay {
        if c == sep {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

/// `s.split()` (runs of whitespace, no empty fields).
pub fn split_ws(hay: &[char]) -> Vec<Vec<char>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    for &c in hay {
        if py_is_space(c) {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn is_in(c: char, set: &[char]) -> bool {
    set.contains(&c)
}

/// `s.strip()`.
pub fn strip_ws(hay: &[char]) -> &[char] {
    let mut a = 0;
    let mut b = hay.len();
    while a < b && py_is_space(hay[a]) {
        a += 1;
    }
    while b > a && py_is_space(hay[b - 1]) {
        b -= 1;
    }
    &hay[a..b]
}

/// `s.lstrip()`.
pub fn lstrip_ws(hay: &[char]) -> &[char] {
    let mut a = 0;
    while a < hay.len() && py_is_space(hay[a]) {
        a += 1;
    }
    &hay[a..]
}

/// `s.rstrip()`.
pub fn rstrip_ws(hay: &[char]) -> &[char] {
    let mut b = hay.len();
    while b > 0 && py_is_space(hay[b - 1]) {
        b -= 1;
    }
    &hay[..b]
}

/// `s.strip(chars)`.
pub fn strip_chars<'a>(hay: &'a [char], set: &[char]) -> &'a [char] {
    let mut a = 0;
    let mut b = hay.len();
    while a < b && is_in(hay[a], set) {
        a += 1;
    }
    while b > a && is_in(hay[b - 1], set) {
        b -= 1;
    }
    &hay[a..b]
}

/// `s.rstrip(chars)`.
pub fn rstrip_chars<'a>(hay: &'a [char], set: &[char]) -> &'a [char] {
    let mut b = hay.len();
    while b > 0 && is_in(hay[b - 1], set) {
        b -= 1;
    }
    &hay[..b]
}

/// `s.replace('  ', ' ')` — Python replaces non-overlapping pairs left to right.
pub fn collapse_double_spaces(hay: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if hay[i] == ' ' && i + 1 < hay.len() && hay[i + 1] == ' ' {
            out.push(' ');
            i += 2;
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// `sep.join(parts)` where each part is a `String`.
pub fn join_str<S: AsRef<str>>(parts: &[S], sep: &str) -> String {
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        out.push_str(p.as_ref());
    }
    out
}

/// `'>'.join` over `Vec<char>` slices — used by the graph ids.
pub fn join_chars(parts: &[Vec<char>], sep: &str) -> String {
    let owned: Vec<String> = parts.iter().map(|p| chars_to_string(p)).collect();
    join_str(&owned, sep)
}

// ---------------------------------------------------------------------------
// regex over character slices
// ---------------------------------------------------------------------------
/// One match, in character offsets, with every capture group as text.
#[derive(Debug, Clone)]
pub struct Match {
    pub start: usize,
    pub end: usize,
    pub groups: Vec<Option<String>>,
}

impl Match {
    pub fn text(&self) -> &str {
        self.groups[0].as_deref().unwrap_or("")
    }
    pub fn group(&self, i: usize) -> Option<&str> {
        self.groups.get(i).and_then(|g| g.as_deref())
    }
}

/// A `re`-like wrapper that always returns character offsets.
pub struct Re {
    re: Regex,
}

/// A haystack converted once for many [`Re`] probes.
///
/// `Regex` works on `&str` while the parser works on `&[char]`, so every probe
/// used to rebuild the string and its byte→char table. Preparing once turns a
/// per-declaration cost from O(file) into O(1), which is the difference between
/// a quadratic scan and a linear one on a large repository.
pub struct Prepared {
    st: String,
    b2c: Vec<usize>,
    chars: usize,
}

impl Prepared {
    pub fn new(hay: &[char]) -> Prepared {
        let (st, b2c) = Re::prepare(hay);
        let chars = hay.len();
        Prepared { st, b2c, chars }
    }

    pub fn chars(&self) -> usize {
        self.chars
    }
}

impl Re {
    pub fn new(pattern: &str) -> Re {
        Re {
            re: Regex::new(pattern).unwrap_or_else(|e| {
                panic!("invalid ported regex {pattern:?}: {e}")
            }),
        }
    }

    /// Like [`Re::new`], but returns `None` instead of panicking — used by the
    /// `fnmatch` translation, which escapes arbitrary literal patterns.
    pub fn new_checked(pattern: &str) -> Option<Re> {
        Regex::new(pattern).ok().map(|re| Re { re })
    }

    fn prepare(hay: &[char]) -> (String, Vec<usize>) {
        let mut st = String::with_capacity(hay.len());
        for &c in hay {
            st.push(c);
        }
        let mut b2c = vec![0usize; st.len() + 1];
        let mut ci = 0usize;
        for (b, _) in st.char_indices() {
            b2c[b] = ci;
            ci += 1;
        }
        b2c[st.len()] = ci;
        // continuation bytes only matter defensively; regex never reports them
        for b in 1..st.len() {
            if b2c[b] == 0 && (st.as_bytes()[b] & 0xC0) == 0x80 {
                b2c[b] = b2c[b - 1];
            }
        }
        (st, b2c)
    }

    fn build_caps(b2c: &[usize], caps: &regex::Captures<'_>) -> Match {
        let mut groups = Vec::with_capacity(caps.len());
        for i in 0..caps.len() {
            groups.push(caps.get(i).map(|g| g.as_str().to_string()));
        }
        let whole = caps.get(0).expect("group 0 always present");
        Match {
            start: b2c[whole.start()],
            end: b2c[whole.end()],
            groups,
        }
    }

    /// `re.search` — leftmost match anywhere.
    pub fn find(&self, hay: &[char]) -> Option<Match> {
        let (st, b2c) = Self::prepare(hay);
        let caps = self.re.captures(&st)?;
        Some(Self::build_caps(&b2c, &caps))
    }

    /// `re.search` over an already prepared haystack.
    ///
    /// [`Re::prepare`] copies the whole haystack and builds a byte→char map, so
    /// calling `find`/`match_at` on the same slice in a loop is quadratic in the
    /// file size. A parser scanning one file must prepare it once
    /// ([`Prepared::new`]) and use these methods for every probe.
    pub fn find_p(&self, hay: &Prepared) -> Option<Match> {
        let caps = self.re.captures(&hay.st)?;
        Some(Self::build_caps(&hay.b2c, &caps))
    }

    /// `re.search` but only at or after character offset `from`.
    pub fn find_from(&self, hay: &[char], from: usize) -> Option<Match> {
        if from > hay.len() {
            return None;
        }
        let (st, b2c) = Self::prepare(hay);
        let caps = self.re.captures_at(&st, b2c[from])?;
        Some(Self::build_caps(&b2c, &caps))
    }

    /// [`Re::find_from`] over an already prepared haystack.
    pub fn find_from_p(&self, hay: &Prepared, from: usize) -> Option<Match> {
        if from > hay.chars {
            return None;
        }
        let caps = self.re.captures_at(&hay.st, hay.b2c[from])?;
        Some(Self::build_caps(&hay.b2c, &caps))
    }

    /// `re.match(p, hay, from)` — the match must begin exactly at `from`.
    pub fn match_at(&self, hay: &[char], from: usize) -> Option<Match> {
        if from > hay.len() {
            return None;
        }
        let (st, b2c) = Self::prepare(hay);
        let caps = self.re.captures_at(&st, b2c[from])?;
        let whole = caps.get(0).expect("group 0 always present");
        if whole.start() != b2c[from] {
            return None;
        }
        Some(Self::build_caps(&b2c, &caps))
    }

    /// [`Re::match_at`] over an already prepared haystack.
    pub fn match_at_p(&self, hay: &Prepared, from: usize) -> Option<Match> {
        if from > hay.chars {
            return None;
        }
        let caps = self.re.captures_at(&hay.st, hay.b2c[from])?;
        let whole = caps.get(0).expect("group 0 always present");
        if whole.start() != hay.b2c[from] {
            return None;
        }
        Some(Self::build_caps(&hay.b2c, &caps))
    }

    /// `re.finditer` — every non-overlapping match, left to right.
    pub fn all(&self, hay: &[char]) -> Vec<Match> {
        let (st, b2c) = Self::prepare(hay);
        self.re
            .captures_iter(&st)
            .map(|caps| Self::build_caps(&b2c, &caps))
            .collect()
    }

    /// `re.finditer` starting at character offset `from`.
    pub fn all_from(&self, hay: &[char], from: usize) -> Vec<Match> {
        if from > hay.len() {
            return Vec::new();
        }
        let (st, b2c) = Self::prepare(hay);
        self.re
            .captures_iter(&st)
            .filter(|c| c.get(0).map(|m| m.start() >= b2c[from]).unwrap_or(false))
            .map(|caps| Self::build_caps(&b2c, &caps))
            .collect()
    }

    /// `re.findall` for the shapes the parser uses: with no capture group it
    /// yields the whole match (Python semantics); with one group it yields
    /// that group. (No ported pattern has two or more groups under `findall`.)
    pub fn findall1(&self, hay: &[char]) -> Vec<String> {
        let ngroups = self.re.captures_len() - 1;
        let idx = if ngroups == 0 { 0 } else { 1 };
        self.all(hay)
            .into_iter()
            .filter_map(|m| m.groups.get(idx).cloned().flatten())
            .collect()
    }

    /// `bool(re.search(...))`.
    pub fn is_match(&self, hay: &[char]) -> bool {
        let (st, _) = Self::prepare(hay);
        self.re.is_match(&st)
    }

    /// `bool(re.search(...))` on a plain string.
    pub fn is_match_str(&self, hay: &str) -> bool {
        self.re.is_match(hay)
    }

    /// `re.search(...).group(1)` on a plain string.
    pub fn search_group1(&self, hay: &str) -> Option<String> {
        self.re
            .captures(hay)
            .and_then(|c| c.get(1).map(|g| g.as_str().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_offsets_survive_non_ascii() {
        let hay = to_chars("a─b");
        let re = Re::new("[a-z]");
        let ms = re.all(&hay);
        assert_eq!(ms.len(), 2);
        assert_eq!(ms[0].start, 0);
        assert_eq!(ms[1].start, 2);
    }

    #[test]
    fn match_at_is_anchored() {
        let re = Re::new(r"\s*(?:(\d+)|([A-Za-z_]\w*))");
        assert!(re.match_at(&to_chars("  12x"), 0).is_some());
        assert!(re.match_at(&to_chars("  12x"), 1).is_some()); // `\s*` eats the space
        assert!(re.match_at(&to_chars("!12"), 0).is_none());
        let digits = Re::new(r"(\d+)");
        assert!(digits.match_at(&to_chars("a12"), 0).is_none()); // leftmost is at 1
        assert!(digits.match_at(&to_chars("a12"), 1).is_some());
    }

    #[test]
    fn python_space_includes_c1_separators() {
        assert!(py_is_space('\u{1c}'));
        assert!(py_is_space('\n'));
        assert!(!py_is_space('\u{200b}'));
    }
}
