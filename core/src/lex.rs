//! lex — the low-level half of `tools/csgraph/cs_parser.py`.
//!
//! Ports, in order: `skip_ws_c`, `scan_lit`, `match_block`, `match_bracket`,
//! `_scan_member_name`, `mem_name`, `_blank`, `_is_literal_start`,
//! `mask_noncode`, `_pp_tokens`, `eval_pp_expr`, `active_line_flags`,
//! `mask_inactive` and `strip_bom`.
//!
//! Every offset here is a *character* offset, exactly like Python's: see
//! `model.rs` for why that matters.

use std::collections::HashSet;
use std::sync::OnceLock;

use crate::model::{
    chars_to_string, find_sub, find_sub_from, lstrip_ws, py_is_alnum, py_is_space, split_on,
    split_ws, strip_ws, to_chars, Re,
};

// ---------------------------------------------------------------------------
// lexer primitives — DUPLICATED from source/convert.py in the Python original
// ---------------------------------------------------------------------------
/// `skip_ws_c(s, i, end)`.
pub fn skip_ws_c(s: &[char], mut i: usize, end: usize) -> usize {
    while i < end {
        let c = s[i];
        if c == '/' && i + 1 < end && s[i + 1] == '/' {
            match find_sub_from(s, &['\n'], i) {
                Some(j) => i = j,
                None => i = end,
            }
        } else if c == '/' && i + 1 < end && s[i + 1] == '*' {
            match find_sub(s, &['*', '/'], i + 2, end) {
                Some(j) => i = j + 2,
                None => i = end,
            }
        } else if c == ' ' || c == '\t' || c == '\r' || c == '\n' {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// `scan_lit(s, i, end)` — one char literal or one (possibly verbatim) string.
pub fn scan_lit(s: &[char], i: usize, end: usize) -> usize {
    if s[i] == '\'' {
        let mut i = i + 1;
        while i < end {
            if s[i] == '\\' {
                i += 2;
            } else if s[i] == '\'' {
                i += 1;
                break;
            } else {
                i += 1;
            }
        }
        return i;
    }
    let mut j = i;
    let mut verb = false;
    while j < end && (s[j] == '@' || s[j] == '$') {
        if s[j] == '@' {
            verb = true;
        }
        j += 1;
    }
    if j >= end || s[j] != '"' {
        return i + 1;
    }
    let mut i = j + 1;
    while i < end {
        let e = s[i];
        if verb {
            if e == '"' {
                if i + 1 < end && s[i + 1] == '"' {
                    i += 2;
                    continue;
                }
                i += 1;
                break;
            }
        } else {
            if e == '\\' {
                i += 2;
                continue;
            }
            if e == '"' {
                i += 1;
                break;
            }
        }
        i += 1;
    }
    i
}

/// `match_block(s, o, end)` — index of the `}` matching the `{` at `o`.
pub fn match_block(s: &[char], o: usize, end: usize) -> usize {
    let mut i = o + 1;
    let mut d: i64 = 1;
    while i < end {
        let c = s[i];
        if c == '"' || c == '\'' {
            i = scan_lit(s, i, end);
            continue;
        }
        if c == '/' && i + 1 < end && s[i + 1] == '/' {
            match find_sub_from(s, &['\n'], i) {
                Some(j) => i = j,
                None => i = end,
            }
            continue;
        }
        if c == '/' && i + 1 < end && s[i + 1] == '*' {
            match find_sub(s, &['*', '/'], i + 2, end) {
                Some(j) => i = j + 2,
                None => i = end,
            }
            continue;
        }
        if c == '{' {
            d += 1;
        } else if c == '}' {
            d -= 1;
            if d == 0 {
                return i;
            }
        }
        i += 1;
    }
    end
}

/// `match_bracket(s, o, end)` — index of the `]` matching the `[` at `o`.
pub fn match_bracket(s: &[char], o: usize, end: usize) -> usize {
    let mut i = o + 1;
    let mut d: i64 = 1;
    while i < end {
        let c = s[i];
        if c == '"' || c == '\'' {
            i = scan_lit(s, i, end);
            continue;
        }
        if c == '/' && i + 1 < end && s[i + 1] == '/' {
            match find_sub_from(s, &['\n'], i) {
                Some(j) => i = j,
                None => i = end,
            }
            continue;
        }
        if c == '/' && i + 1 < end && s[i + 1] == '*' {
            match find_sub(s, &['*', '/'], i + 2, end) {
                Some(j) => i = j + 2,
                None => i = end,
            }
            continue;
        }
        if c == '[' {
            d += 1;
        } else if c == ']' {
            d -= 1;
            if d == 0 {
                return i;
            }
        }
        i += 1;
    }
    end
}

/// `_scan_member_name(head)`.
pub fn scan_member_name(head: &[char]) -> Option<String> {
    let end = head.len();
    let mut i = 0usize;
    let mut pending: Option<String> = None;
    let mut cand: Option<String> = None;
    let mut depth: i64 = 0;
    while i < end {
        let c = head[i];
        if py_is_alnum(c) || c == '_' {
            let mut j = i;
            while j < end && (py_is_alnum(head[j]) || head[j] == '_') {
                j += 1;
            }
            pending = Some(chars_to_string(&head[i..j]));
            i = j;
            continue;
        }
        if py_is_space(c) {
            i += 1;
            continue;
        }
        if c == '<' {
            let mut d: i64 = 1;
            i += 1;
            while i < end && d != 0 {
                if head[i] == '<' {
                    d += 1;
                } else if head[i] == '>' {
                    d -= 1;
                }
                i += 1;
            }
            continue;
        }
        if c == '(' {
            if depth == 0 && pending.is_some() {
                cand = pending.clone();
            }
            depth += 1;
            pending = None;
            i += 1;
            continue;
        }
        if c == ')' {
            depth -= 1;
            i += 1;
            continue;
        }
        pending = None;
        i += 1;
    }
    cand
}

/// `mem_name(head)`.
pub fn mem_name(head: &[char]) -> String {
    // Compiled once: `mem_name` runs for every member of every type, and
    // building the regex automata each time dominated parsing on a large file.
    fn arrow_re() -> &'static Re {
        static R: OnceLock<Re> = OnceLock::new();
        R.get_or_init(|| Re::new("([A-Za-z_]\\w*)\\s*=>"))
    }
    fn eq_base_re() -> &'static Re {
        static R: OnceLock<Re> = OnceLock::new();
        R.get_or_init(|| Re::new("([A-Za-z_]\\w*)\\s*="))
    }
    fn assign_re() -> &'static Re {
        static R: OnceLock<Re> = OnceLock::new();
        R.get_or_init(|| Re::new("([A-Za-z_]\\w*)\\s*=\\s*(?:\\{|[A-Za-z_])"))
    }
    fn words_re() -> &'static Re {
        static R: OnceLock<Re> = OnceLock::new();
        R.get_or_init(|| Re::new("\\b([A-Za-z_]\\w*)\\b"))
    }

    let before: &[char] = match find_sub_from(head, &['=', '>'], 0) {
        Some(p) => &head[..p],
        None => head,
    };
    if let Some(nm) = scan_member_name(before) {
        if !nm.is_empty() {
            return nm;
        }
    }
    if find_sub_from(head, &['=', '>'], 0).is_some() {
        let arrow = arrow_re();
        let ms = arrow.all(head);
        if let Some(m) = ms.last() {
            if let Some(g) = m.group(1) {
                return g.to_string();
            }
        }
    }
    // `re.findall(r'(?<=[\s;,(])([A-Za-z_]\w*)\s*=(?!=|>|<|~)', head)`.
    //
    // The vendored `regex` crate has no lookaround, so both assertions are
    // applied by hand. The scan reproduces Python's start-position walk: take
    // the leftmost `name\s*=` from the current position; if either assertion
    // fails, retry one character later (not after the match — Python only
    // advances past a match it accepted). A match rejected this way can never
    // hide a later valid one here, because the lookbehind class contains no
    // identifier character and the lookahead sits after the `=`.
    let eq_base = eq_base_re();
    let mut eq: Vec<String> = Vec::new();
    let mut pos = 0usize;
    while pos <= head.len() {
        let Some(m) = eq_base.find_from(head, pos) else {
            break;
        };
        let ok_before = m.start > 0 && {
            let prev = head[m.start - 1];
            py_is_space(prev) || prev == ';' || prev == ',' || prev == '('
        };
        let ok_after = m.end >= head.len() || !matches!(head[m.end], '=' | '>' | '<' | '~');
        if ok_before && ok_after {
            if let Some(g) = m.group(1) {
                eq.push(g.to_string());
            }
            pos = m.end;
        } else {
            pos = m.start + 1;
        }
    }
    if let Some(last) = eq.last() {
        return last.clone();
    }
    let assign = assign_re();
    if let Some(m) = assign.find(head) {
        if let Some(g) = m.group(1) {
            return g.to_string();
        }
    }
    let words = words_re();
    let mm = words.findall1(head);
    if let Some(last) = mm.last() {
        return last.clone();
    }
    "member".to_string()
}

// ---------------------------------------------------------------------------
// masking
// ---------------------------------------------------------------------------
/// `_blank(seg)` — same length, newlines kept, everything else a space.
pub fn blank(seg: &[char]) -> Vec<char> {
    seg.iter()
        .map(|&ch| if ch == '\n' { '\n' } else { ' ' })
        .collect()
}

/// `_is_literal_start(s, i, n)`.
pub fn is_literal_start(s: &[char], i: usize, n: usize) -> bool {
    let c = s[i];
    if c == '\'' || c == '"' {
        return true;
    }
    let mut j = i;
    while j < n && (s[j] == '@' || s[j] == '$') {
        j += 1;
    }
    j > i && j < n && s[j] == '"'
}

/// `mask_noncode(src)` — blank comments + literals, preserving offsets.
pub fn mask_noncode(src: &[char]) -> Vec<char> {
    let mut out: Vec<char> = Vec::with_capacity(src.len());
    let mut i = 0usize;
    let n = src.len();
    while i < n {
        let c = src[i];
        if c == '/' && i + 1 < n && src[i + 1] == '/' {
            let j = find_sub_from(src, &['\n'], i).unwrap_or(n);
            out.extend(std::iter::repeat_n(' ', j - i));
            i = j;
            continue;
        }
        if c == '/' && i + 1 < n && src[i + 1] == '*' {
            let j = match find_sub(src, &['*', '/'], i + 2, n) {
                Some(v) => v + 2,
                None => n,
            };
            out.extend(blank(&src[i..j]));
            i = j;
            continue;
        }
        if c == '@' && i + 1 < n && src[i + 1] != '"' && src[i + 1] != '$' {
            out.push(c);
            i += 1;
            continue;
        }
        if is_literal_start(src, i, n) {
            let j = scan_lit(src, i, n);
            out.extend(blank(&src[i..j]));
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// preprocessor
// ---------------------------------------------------------------------------
/// A token pair like Python's `('num'|'id'|'op', text)`.
pub type PpTok = (&'static str, String);

/// `_pp_tokens(expr)` — hand-rolled equivalent of `_PP_TOKEN_RE.match` in a
/// loop. The pattern is
/// `\s*(?:(\d+)|([A-Za-z_]\w*)|(==|!=|&&|\|\||[!()]))`; unknown characters are
/// skipped one at a time.
pub fn pp_tokens(expr: &[char]) -> Vec<PpTok> {
    let mut toks = Vec::new();
    let mut i = 0usize;
    let n = expr.len();
    while i < n {
        // `\s*`
        while i < n && py_is_space(expr[i]) {
            i += 1;
        }
        if i >= n {
            break;
        }
        let c = expr[i];
        if c.is_ascii_digit() {
            let mut j = i;
            while j < n && expr[j].is_ascii_digit() {
                j += 1;
            }
            toks.push(("num", chars_to_string(&expr[i..j])));
            i = j;
            continue;
        }
        if c.is_ascii_alphabetic() || c == '_' {
            let mut j = i;
            while j < n && (expr[j].is_alphanumeric() || expr[j] == '_') {
                j += 1;
            }
            toks.push(("id", chars_to_string(&expr[i..j])));
            i = j;
            continue;
        }
        // `==`, `!=`, `&&`, `||` and the single-character operators.
        let two = |k: usize| -> String {
            if k + 1 < n {
                chars_to_string(&expr[k..k + 2])
            } else {
                String::new()
            }
        };
        let t = two(i);
        if t == "==" || t == "!=" || t == "&&" || t == "||" {
            toks.push(("op", t));
            i += 2;
            continue;
        }
        if c == '!' || c == '(' || c == ')' {
            toks.push(("op", c.to_string()));
            i += 1;
            continue;
        }
        // unknown character: skip it
        i += 1;
    }
    toks
}

struct PpEval<'a> {
    toks: Vec<PpTok>,
    pos: usize,
    defines: &'a HashSet<String>,
}

impl<'a> PpEval<'a> {
    fn peek(&self) -> Option<&PpTok> {
        self.toks.get(self.pos)
    }

    fn is_op(&self, v: &str) -> bool {
        matches!(self.peek(), Some((k, s)) if *k == "op" && s == v)
    }

    /// `primary()`.
    fn primary(&mut self) -> bool {
        let Some((kind, val)) = self.peek().map(|(k, v)| (*k, v.clone())) else {
            return false;
        };
        if kind == "op" && val == "(" {
            self.pos += 1;
            let v = self.or_expr();
            if self.is_op(")") {
                self.pos += 1;
            }
            return v;
        }
        if kind == "op" && val == "!" {
            self.pos += 1;
            return !self.primary();
        }
        if kind == "num" {
            self.pos += 1;
            return val.chars().any(|c| c != '0');
        }
        if kind == "id" {
            self.pos += 1;
            let low = val.to_lowercase();
            if low == "true" {
                return true;
            }
            if low == "false" {
                return false;
            }
            if low == "defined" {
                if self.is_op("(") {
                    self.pos += 1;
                }
                let k2 = self.peek().map(|t| t.0);
                if k2 != Some("id") {
                    return false;
                }
                let v2 = self.peek().unwrap().1.clone();
                self.pos += 1;
                if self.is_op(")") {
                    self.pos += 1;
                }
                return self.defines.contains(&v2);
            }
            return self.defines.contains(&val);
        }
        false
    }

    /// `eq_expr()`.
    fn eq_expr(&mut self) -> bool {
        let mut left = self.primary();
        loop {
            let op = match self.peek() {
                Some((k, v)) if *k == "op" && (v == "==" || v == "!=") => v.clone(),
                _ => break,
            };
            self.pos += 1;
            let right = self.primary();
            left = if op == "==" { left == right } else { left != right };
        }
        left
    }

    /// `and_expr()`.
    fn and_expr(&mut self) -> bool {
        let mut left = self.eq_expr();
        while matches!(self.peek(), Some((k, v)) if *k == "op" && v == "&&") {
            self.pos += 1;
            let right = self.eq_expr();
            left = left && right;
        }
        left
    }

    /// `or_expr()`.
    fn or_expr(&mut self) -> bool {
        let mut left = self.and_expr();
        while matches!(self.peek(), Some((k, v)) if *k == "op" && v == "||") {
            self.pos += 1;
            let right = self.and_expr();
            left = left || right;
        }
        left
    }
}

/// `eval_pp_expr(expr, defines=())` — `&&`, `||`, `!`, `==`, `!=`, `()`,
/// `defined(X)`.
pub fn eval_pp_expr(expr: &[char], defines: &HashSet<String>) -> bool {
    let mut ev = PpEval {
        toks: pp_tokens(expr),
        pos: 0,
        defines,
    };
    ev.or_expr()
}

/// One frame of `active_line_flags`'s conditional stack.
struct PpFrame {
    parent: bool,
    taken: bool,
    active: bool,
}

/// `active_line_flags(src, defines=None)`.
pub fn active_line_flags(src: &[char], defines: Option<&HashSet<String>>) -> Vec<bool> {
    let known = defines.is_some();
    let mut defs: HashSet<String> = defines.cloned().unwrap_or_default();
    let mut flags: Vec<bool> = Vec::new();
    let mut stack: Vec<PpFrame> = Vec::new();
    let mut active = true;
    let head_re = Re::new("#\\s*([A-Za-z_]\\w*)\\s*(.*)");

    for ln in split_on(src, '\n') {
        let stripped = lstrip_ws(&ln);
        if stripped.first() == Some(&'#') {
            let (word, rest) = match head_re.match_at(stripped, 0) {
                Some(m) => (
                    m.group(1).unwrap_or("").to_lowercase(),
                    strip_ws(&to_chars(m.group(2).unwrap_or(""))).to_vec(),
                ),
                None => (String::new(), Vec::new()),
            };
            let rest_s = chars_to_string(&rest);
            if known {
                if word == "if" {
                    let parent = active;
                    let val = parent && eval_pp_expr(&rest, &defs);
                    stack.push(PpFrame {
                        parent,
                        taken: val,
                        active: val,
                    });
                    active = val;
                } else if word == "elif" && !stack.is_empty() {
                    let fr = stack.last_mut().unwrap();
                    if fr.taken {
                        fr.active = false;
                    } else {
                        let val = fr.parent && eval_pp_expr(&rest, &defs);
                        fr.active = val;
                        fr.taken = fr.taken || val;
                    }
                    active = fr.active;
                } else if word == "else" && !stack.is_empty() {
                    let fr = stack.last_mut().unwrap();
                    let val = fr.parent && !fr.taken;
                    fr.active = val;
                    fr.taken = true;
                    active = fr.active;
                } else if word == "endif" && !stack.is_empty() {
                    stack.pop();
                    active = stack.last().map(|f| f.active).unwrap_or(true);
                } else if word == "define" && active && !rest.is_empty() {
                    let name = match rest.iter().position(|&c| c == ' ' || c == '\t' || c == '(') {
                        Some(p) => chars_to_string(&rest[..p]),
                        None => rest_s.clone(),
                    };
                    defs.insert(name);
                } else if word == "undef" && active && !rest.is_empty() {
                    if let Some(tok) = split_ws(&rest).first() {
                        defs.remove(&chars_to_string(tok));
                    }
                }
            }
            flags.push(false);
            continue;
        }
        flags.push(active);
    }
    flags
}

/// `mask_inactive(src, defines=None)`.
pub fn mask_inactive(src: &[char], defines: Option<&HashSet<String>>) -> Vec<char> {
    let flags = active_line_flags(src, defines);
    let lines = split_on(src, '\n');
    let mut out: Vec<char> = Vec::with_capacity(src.len());
    for (i, ln) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if i < flags.len() && flags[i] {
            out.extend_from_slice(ln);
        } else {
            out.extend(std::iter::repeat_n(' ', ln.len()));
        }
    }
    out
}

/// `strip_bom(text)` — replace a leading BOM with a space, shifting no offsets.
pub fn strip_bom(text: &[char]) -> Vec<char> {
    if text.first() == Some(&'\u{feff}') {
        let mut out = text.to_vec();
        out[0] = ' ';
        out
    } else {
        text.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_lit_handles_verbatim_and_escapes() {
        let s = to_chars(r#"@"" + "b\nc" + 'x'"#);
        let end = s.len();
        let i = scan_lit(&s, 0, end);
        assert_eq!(chars_to_string(&s[..i]), r#"@"""#);
        // Skip the whitespace *and the `+`*: scan_lit starts at the literal,
        // it does not hunt for one. Verified against the frozen Python, which
        // returns "+" if you hand it the operator position.
        let mut j = skip_ws_c(&s, i, end);
        if j < end && s[j] == '+' {
            j = skip_ws_c(&s, j + 1, end);
        }
        let k = scan_lit(&s, j, end);
        assert_eq!(chars_to_string(&s[j..k]), r#""b\nc""#);
    }

    #[test]
    fn mask_noncode_pads_comments_with_characters() {
        let src = to_chars("a /* ── */ b");
        let m = mask_noncode(&src);
        assert_eq!(m.len(), src.len());
        // `/* ── */` is 8 characters, so it becomes 8 spaces.
        assert_eq!(chars_to_string(&m), "a          b");
    }

    #[test]
    fn pp_eval_simple() {
        let d: HashSet<String> = ["A".to_string()].into_iter().collect();
        assert!(eval_pp_expr(&to_chars("A && !B"), &d));
        assert!(!eval_pp_expr(&to_chars("A && B"), &d));
        assert!(eval_pp_expr(&to_chars("defined(A) || B"), &d));
    }
}
