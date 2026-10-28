//! parser — `Source`, `Parser`, reference/string extraction and file discovery.
//!
//! This is the middle of `tools/csgraph/cs_parser.py`: from `simple_match_brace`
//! down to `discover_files`. Records are built as `serde_json::Value` objects
//! so the key set matches the Python dicts exactly; `serde_json`'s map is a
//! `BTreeMap`, so serialisation is sorted and key order carries no meaning.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{json, Map, Value};

use crate::lex::{is_literal_start, mask_inactive, mask_noncode, mem_name, scan_lit, strip_bom};
use crate::model::{
    chars_to_string, collapse_double_spaces, find_sub, is_keyword, is_modifier, join_str,
    py_is_space, rstrip_chars, rstrip_ws, split_on, strip_ws, Prepared, Re,
};

// ---------------------------------------------------------------------------
// compiled patterns (cs_parser.py's module-level re.compile calls)
// ---------------------------------------------------------------------------
macro_rules! re_static {
    ($name:ident, $pat:expr) => {
        pub fn $name() -> &'static Re {
            static R: OnceLock<Re> = OnceLock::new();
            R.get_or_init(|| Re::new($pat))
        }
    };
}

re_static!(type_re, r"\b(class|struct|interface|enum|record)\s+([A-Za-z_]\w*)");
re_static!(record_re, r"\brecord\s+(class|struct)\s+([A-Za-z_]\w*)");
re_static!(ident_re, r"[A-Za-z_]\w*");
re_static!(ident_full_re, r"^[A-Za-z_]\w*$");
re_static!(ret_token_re, r"[A-Za-z_][\w.]*(?:<[^;{}]*>)?(?:\[\])*");
re_static!(token_re, r"[A-Za-z_]\w*|=>|\?\.|->|\?\?|[^\s]");
re_static!(
    var_decl_base_re,
    r"\b([A-Za-z_]\w*(?:<[^;{}()=]*>)?(?:\[\])*)\s+([a-z_]\w*)\s*"
);
re_static!(new_var_re, r"\bvar\s+([a-z_]\w*)\s*=\s*new\s+([A-Za-z_]\w*)");
re_static!(foreach_re, r"\bforeach\s*\(\s*([A-Za-z_]\w*)\s+([a-z_]\w*)\s+in\b");
re_static!(namespace_re, r"\bnamespace\s+([A-Za-z_][\w.]*)\s*([{;])");
re_static!(namespace_lead_re, r"\bnamespace\s+");
re_static!(where_re, r"\bwhere\b");
re_static!(implicit_operator_re, r"\b(?:implicit|explicit)\s+operator\b");
re_static!(operator_name_re, r"operator\s+([^\s(]+)");
re_static!(operator_re, r"\boperator\b");
re_static!(operator_name2_re, r"\boperator\s*([^\s(]+)");
re_static!(event_re, r"\bevent\b");
re_static!(this_bracket_re, r"\bthis\s*\[");
re_static!(static_re, r"\bstatic\b");
re_static!(qualified_re, r"[A-Za-z_][\w.]*");

fn access_re(word: &str) -> &'static Re {
    macro_rules! per_word {
        ($($w:literal => $f:ident),*) => {
            match word {
                $($w => { fn $f() -> &'static Re { static R: OnceLock<Re> = OnceLock::new(); R.get_or_init(|| Re::new(concat!(r"\b", $w, r"\b"))) } $f() })*
                _ => unreachable!("unknown access modifier"),
            }
        };
    }
    per_word!("public" => r_public, "private" => r_private,
              "protected" => r_protected, "internal" => r_internal)
}

// ---------------------------------------------------------------------------
// structural scanning
// ---------------------------------------------------------------------------
/// `simple_match_brace(s, o, b)` — `s` must already be masked.
pub fn simple_match_brace(s: &[char], o: usize, b: usize) -> usize {
    let mut i = o + 1;
    let mut d: i64 = 1;
    while i < b {
        let c = s[i];
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
    b
}

/// `skip_trivia(s, i, b)`.
pub fn skip_trivia(s: &[char], mut i: usize, b: usize) -> usize {
    while i < b {
        let c = s[i];
        if c == ' ' || c == '\t' || c == '\r' || c == '\n' {
            i += 1;
            continue;
        }
        if c == ';' {
            i += 1;
            continue;
        }
        if c == '#' {
            let j = find_sub(s, &['\n'], i, b);
            i = match j {
                Some(j) => j,
                None => b,
            };
            continue;
        }
        if c == '[' {
            i = crate::lex::match_bracket(s, i, b) + 1;
            continue;
        }
        break;
    }
    i
}

/// `_top_level_index(s, a, b, want)` — first char in `want` at depth 0.
pub fn top_level_index(s: &[char], a: usize, b: usize, want: &[char]) -> isize {
    let mut i = a;
    let mut paren: i64 = 0;
    while i < b {
        let c = s[i];
        if paren == 0 && want.contains(&c) {
            return i as isize;
        }
        if c == '(' {
            paren += 1;
        } else if c == ')' {
            paren = (paren - 1).max(0);
        } else if c == '[' {
            i = crate::lex::match_bracket(s, i, b);
        }
        i += 1;
    }
    -1
}

/// `_split_top(s, sep=',')`.
pub fn split_top(s: &[char], sep: char) -> Vec<Vec<char>> {
    let mut out: Vec<Vec<char>> = Vec::new();
    let mut depth: i64 = 0;
    let mut cur: Vec<char> = Vec::new();
    for &ch in s {
        if ch == '(' || ch == '[' || ch == '{' || ch == '<' {
            depth += 1;
        } else if ch == ')' || ch == ']' || ch == '}' || ch == '>' {
            depth = (depth - 1).max(0);
        }
        if ch == sep && depth == 0 {
            out.push(std::mem::take(&mut cur));
            continue;
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `_strip_default(p)`.
pub fn strip_default(p: &[char]) -> Vec<char> {
    let mut depth: i64 = 0;
    for (i, &ch) in p.iter().enumerate() {
        if ch == '(' || ch == '[' || ch == '{' || ch == '<' {
            depth += 1;
        } else if ch == ')' || ch == ']' || ch == '}' || ch == '>' {
            depth = (depth - 1).max(0);
        } else if ch == '=' && depth == 0 {
            return p[..i].to_vec();
        }
    }
    p.to_vec()
}

/// `parse_param(p)` — one parameter, or `None` when there is no name.
pub fn parse_param(p: &[char]) -> Option<Value> {
    let stripped = strip_default(p);
    let stripped = strip_ws(&stripped);
    if stripped.is_empty() {
        return None;
    }
    let collapsed = collapse_double_spaces(stripped);
    let toks: Vec<Vec<char>> = split_on(&collapsed, ' ')
        .into_iter()
        .filter(|t| !t.is_empty())
        .collect();
    if toks.is_empty() {
        return None;
    }
    let last = toks.last().unwrap();
    let mut name = chars_to_string(last);
    while name.ends_with(']') {
        name.pop();
    }
    if !ident_full_re().is_match_str(&name) {
        let m = ident_re().find(last);
        match m {
            Some(m) => name = m.text().to_string(),
            None => return None,
        }
    }
    let mut rest: Vec<String> = Vec::new();
    for t in &toks[..toks.len() - 1] {
        let ts = chars_to_string(t);
        if !is_modifier(&ts) && ts != ":" {
            rest.push(ts);
        }
    }
    let mut type_ = join_str(&rest, " ").trim().to_string();
    if type_.is_empty() || is_modifier(&type_) {
        type_ = String::new();
    }
    Some(json!({"name": name, "type": type_}))
}

/// `parse_params(text)`.
pub fn parse_params(text: &[char]) -> Vec<Value> {
    let inner = strip_ws(text);
    let mut params = Vec::new();
    for part in split_top(inner, ',') {
        if let Some(pp) = parse_param(&part) {
            params.push(pp);
        }
    }
    params
}

/// `_name_before_paren(before)`.
pub fn name_before_paren(before: &[char]) -> Option<String> {
    // Compiled once: this runs per member declaration, and recompiling the
    // automata each time was a measurable share of a large-file parse.
    fn trailing_ident_re() -> &'static Re {
        static R: OnceLock<Re> = OnceLock::new();
        R.get_or_init(|| Re::new(r"([A-Za-z_]\w*)$"))
    }
    let mut b = rstrip_ws(before).to_vec();
    if b.last() == Some(&'>') {
        let mut d: i64 = 0;
        let mut cut: Option<usize> = None;
        for i in (0..b.len()).rev() {
            if b[i] == '>' {
                d += 1;
            } else if b[i] == '<' {
                d -= 1;
                if d == 0 {
                    cut = Some(i);
                    break;
                }
            }
        }
        if let Some(i) = cut {
            b.truncate(i);
        }
        b = rstrip_ws(&b).to_vec();
    }
    let m = trailing_ident_re().find(&b);
    m.map(|m| m.text().to_string())
}

/// `_sig_key(params)`.
pub fn sig_key(params: &[Value]) -> String {
    let parts: Vec<String> = params
        .iter()
        .map(|p| match p.get("type").and_then(|t| t.as_str()) {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => "?".to_string(),
        })
        .collect();
    join_str(&parts, ", ")
}

/// `_join_id(owner, name)`.
pub fn join_id(owner: &str, name: &str) -> String {
    if owner.is_empty() {
        name.to_string()
    } else {
        format!("{owner}.{name}")
    }
}

// ---------------------------------------------------------------------------
// source file view
// ---------------------------------------------------------------------------
/// One enclosing scope, as `_scope` threads it through `Source::context`.
#[derive(Clone)]
pub struct Scope {
    pub kind: String,
    pub name: String,
    pub open: Option<usize>,
    pub close: Option<usize>,
}

/// `class Source`.
pub struct Source {
    pub path: String,
    pub display: String,
    pub text: Vec<char>,
    pub _starts: Vec<usize>,
    pub pp_flags: Vec<bool>,
    pub masked: Vec<char>,
}

impl Source {
    /// `Source.__init__` — `text` is a decoded `str` (BOM included).
    pub fn new(
        path: String,
        display: String,
        text: Vec<char>,
        defines: Option<&HashSet<String>>,
    ) -> Source {
        let text = if text.first() == Some(&'\u{feff}') {
            strip_bom(&text)
        } else {
            text
        };
        let mut starts = vec![0usize];
        let mut i = 0usize;
        while let Some(j) = find_sub(&text, &['\n'], i, text.len()) {
            starts.push(j + 1);
            i = j + 1;
        }
        let clean = mask_noncode(&text);
        let pp_flags = crate::lex::active_line_flags(&clean, defines);
        let masked = mask_inactive(&clean, defines);
        Source {
            path,
            display,
            text,
            _starts: starts,
            pp_flags,
            masked,
        }
    }

    /// `Source.line_at(off)` — `bisect.bisect_right` over the line starts.
    pub fn line_at(&self, off: usize) -> usize {
        self._starts.partition_point(|&s| s <= off)
    }

    /// `Source.line_start(line)`.
    pub fn line_start(&self, line: usize) -> usize {
        let i = line.max(1).min(self._starts.len());
        self._starts[i - 1]
    }

    /// `Source.line_text(line)`.
    pub fn line_text(&self, line: usize) -> Vec<char> {
        let a = self.line_start(line);
        let b = if line < self._starts.len() {
            self._starts[line] - 1
        } else {
            self.text.len()
        };
        self.text[a..b].to_vec()
    }

    /// `Source.context(scopes)` — `(head, tail)` wrap lines.
    pub fn context(&self, scopes: &[Scope]) -> (Vec<String>, Vec<String>) {
        let mut head: Vec<String> = Vec::new();
        let mut seen: Option<usize> = None;
        for sc in scopes {
            let Some(open_off) = sc.open else { continue };
            let line = self.line_at(open_off + 1);
            let seg = self.line_text(line);
            let col = open_off - self.line_start(line);
            let cut = (col + 1).min(seg.len());
            let seg = chars_to_string(rstrip_ws(&seg[..cut]));
            if Some(line) == seen {
                *head.last_mut().unwrap() = seg;
            } else {
                head.push(seg);
                seen = Some(line);
            }
        }
        let mut tail: Vec<String> = Vec::new();
        let mut seen_line: Option<usize> = None;
        for sc in scopes.iter().rev() {
            let Some(close_off) = sc.close else { continue };
            let line = self.line_at(close_off + 1);
            let col = close_off - self.line_start(line);
            let seg = self.line_text(line);
            let seg = chars_to_string(rstrip_ws(&seg[col.min(seg.len())..]));
            if Some(line) == seen_line {
                if seg.len() > tail.last().unwrap().len() {
                    *tail.last_mut().unwrap() = seg;
                }
            } else {
                tail.push(seg);
                seen_line = Some(line);
            }
        }
        (
            head.into_iter().filter(|h| !h.trim().is_empty()).collect(),
            tail.into_iter().filter(|t| !t.trim().is_empty()).collect(),
        )
    }
}

// ---------------------------------------------------------------------------
// the parser
// ---------------------------------------------------------------------------
/// `class Parser`.
pub struct Parser<'a> {
    pub src: &'a Source,
    pub s: &'a [char],
    /// The whole file, converted once for `Re` probes. Without this every
    /// namespace/declaration probe re-copied the file and rebuilt its
    /// byte→char table, which is quadratic on a large source file.
    pub prep: Prepared,
    pub types: Vec<Value>,
    pub members: Vec<Value>,
    pub inherits: Vec<(String, String)>,
    pub warnings: Vec<String>,
    pub cur_tname: String,
}

impl<'a> Parser<'a> {
    pub fn new(source: &'a Source) -> Parser<'a> {
        Parser {
            src: source,
            s: &source.masked,
            prep: Prepared::new(&source.masked),
            types: Vec::new(),
            members: Vec::new(),
            inherits: Vec::new(),
            warnings: Vec::new(),
            cur_tname: String::new(),
        }
    }

    // -- entry point --------------------------------------------------------
    /// `Parser.parse`.
    pub fn parse(&mut self) {
        let n = self.s.len();
        let matches = namespace_re().all(self.s);
        // Only the *outermost* namespace of a nest is started here. `scope`
        // walks into a nested `namespace` block itself and prefixes its name, so
        // an inner block must not also be started from here with a bare name:
        // `namespace A { namespace B { … } }` must yield `A.B`, not a second
        // top-level `B`.
        let mut covered_until = 0usize;
        for m in matches {
            if m.start < covered_until {
                continue;
            }
            let g1 = m.group(1).unwrap_or("").to_string();
            let g2 = m.group(2).unwrap_or("").to_string();
            if g2 == "{" {
                let close = simple_match_brace(self.s, m.end - 1, n);
                covered_until = close + 1;
                let ns_scope = vec![Scope {
                    kind: "namespace".to_string(),
                    name: g1.clone(),
                    open: Some(m.end - 1),
                    close: Some(close),
                }];
                self.scope(m.end, close, &g1, &[], &ns_scope);
            } else {
                let nxt = namespace_lead_re().find_from_p(&self.prep, m.end);
                let stop = match nxt {
                    Some(x) => m.end + x.start,
                    None => n,
                };
                covered_until = stop;
                let ns_scope = vec![Scope {
                    kind: "namespace".to_string(),
                    name: g1.clone(),
                    open: None,
                    close: None,
                }];
                self.scope(m.end, stop, &g1, &[], &ns_scope);
            }
        }
        if self.types.is_empty() && self.members.is_empty() {
            self.scope(0, n, "", &[], &[]);
        }
    }

    // -- declaration scanner ------------------------------------------------
    /// `_scan_decl(i, b)` -> `(head_end, body_open, body_close, semi)`.
    pub fn scan_decl(
        &self,
        i: usize,
        b: usize,
    ) -> Option<(usize, Option<usize>, Option<usize>, Option<usize>)> {
        let s = self.s;
        let mut p = i;
        let mut paren: i64 = 0;
        let mut on_assign = false;
        while p < b {
            let c = s[p];
            if c == '(' {
                paren += 1;
                p += 1;
                continue;
            }
            if c == ')' {
                paren = (paren - 1).max(0);
                p += 1;
                continue;
            }
            if c == '[' {
                p = crate::lex::match_bracket(s, p, b) + 1;
                continue;
            }
            if c == '{' {
                if paren == 0 && !on_assign {
                    let close = simple_match_brace(s, p, b);
                    return Some((p, Some(p), Some(close), None));
                }
                p = simple_match_brace(s, p, b) + 1;
                continue;
            }
            if c == '}' {
                return None;
            }
            if c == ';' && paren == 0 {
                return Some((p, None, None, Some(p)));
            }
            if c == '=' && paren == 0 {
                let nxt = if p + 1 < b { s[p + 1] } else { '\0' };
                if nxt == '>' {
                    on_assign = true;
                    p += 2;
                    continue;
                }
                if nxt == '=' || nxt == '<' {
                    p += 2;
                    continue;
                }
                let prev = if p > 0 { s[p - 1] } else { '\0' };
                if prev == '=' || prev == '<' || prev == '>' || prev == '!'
                    || prev == '+' || prev == '-' || prev == '*' || prev == '/'
                    || prev == '%' || prev == '&' || prev == '|' || prev == '^'
                {
                    p += 1;
                    continue;
                }
                on_assign = true;
                p += 1;
                continue;
            }
            p += 1;
        }
        None
    }

    // -- scope walk ---------------------------------------------------------
    /// `_scope(a, b, ns, stack, scopes=None)`.
    pub fn scope(&mut self, a: usize, b: usize, ns: &str, stack: &[String], scopes: &[Scope]) {
        let mut scopes: Vec<Scope> = scopes.to_vec();
        let mut i = a;
        let mut guard = 0usize;
        while i < b {
            guard += 1;
            if guard > 200_000 {
                self.warnings.push("scope guard tripped".to_string());
                break;
            }
            i = skip_trivia(self.s, i, b);
            if i >= b {
                break;
            }
            let start = i;
            // A namespace nested inside this scope keeps its parent's name:
            // `namespace A { namespace B { … } }` declares the namespace `A.B`
            // and everything in it, not a top-level `B`. Without this the inner
            // block's declarations were emitted under the bare name and the
            // outer namespace produced no node at all.
            if let Some(nm) = namespace_re().match_at_p(&self.prep, i) {
                let name = nm.group(1).unwrap_or("").to_string();
                let sep = nm.group(2).unwrap_or("").to_string();
                let inner = if ns.is_empty() {
                    name.clone()
                } else {
                    format!("{ns}.{name}")
                };
                if sep == "{" {
                    let open = nm.end - 1;
                    let close = simple_match_brace(self.s, open, b);
                    scopes.push(Scope {
                        kind: "namespace".to_string(),
                        name: name.clone(),
                        open: Some(open),
                        close: Some(close),
                    });
                    self.scope(open + 1, close, &inner, stack, &scopes);
                    scopes.pop();
                    i = close + 1;
                } else {
                    scopes.push(Scope {
                        kind: "namespace".to_string(),
                        name: name.clone(),
                        open: None,
                        close: None,
                    });
                    self.scope(nm.end, b, &inner, stack, &scopes);
                    scopes.pop();
                    return;
                }
                continue;
            }
            let sc = self.scan_decl(i, b);
            let Some(sc) = sc else { break };
            let (head_end, body_open, body_close, semi) = sc;
            let head: Vec<char> = self.s[start..head_end].to_vec();
            let tinfo = self.classify_type(&head);
            if let Some((tkind, tname)) = tinfo {
                self.add_type(&tkind, &tname, &head, ns, stack, &sc, start, &scopes);
                if body_open.is_some() && tkind != "enum" {
                    let anchor = Scope {
                        kind: tkind.clone(),
                        name: tname.clone(),
                        open: body_open,
                        close: body_close,
                    };
                    let mut st2 = stack.to_vec();
                    st2.push(tname.clone());
                    scopes.push(anchor);
                    self.scope(
                        body_open.unwrap() + 1,
                        body_close.unwrap(),
                        ns,
                        &st2,
                        &scopes,
                    );
                    scopes.pop();
                }
            } else {
                self.add_member(&head, &sc, ns, stack, start, &scopes);
            }
            i = match body_open {
                Some(_) => body_close.unwrap() + 1,
                None => semi.unwrap() + 1,
            };
        }
    }

    // -- classification -----------------------------------------------------
    /// `_classify_type(head)`.
    pub fn classify_type(&self, head: &[char]) -> Option<(String, String)> {
        if let Some(m) = record_re().find(head) {
            return Some(("record".to_string(), m.group(2).unwrap_or("").to_string()));
        }
        if let Some(m) = type_re().find(head) {
            return Some((
                m.group(1).unwrap_or("").to_string(),
                m.group(2).unwrap_or("").to_string(),
            ));
        }
        None
    }

    /// `_add_type(kind, name, head, ns, stack, sc, start, scopes=None)`.
    #[allow(clippy::too_many_arguments)]
    fn add_type(
        &mut self,
        kind: &str,
        name: &str,
        head: &[char],
        ns: &str,
        stack: &[String],
        sc: &(usize, Option<usize>, Option<usize>, Option<usize>),
        start: usize,
        scopes: &[Scope],
    ) {
        let mut parts: Vec<String> = Vec::new();
        if !ns.is_empty() {
            parts.push(ns.to_string());
        }
        parts.extend(stack.iter().cloned());
        parts.push(name.to_string());
        let full = join_str(&parts, ".");
        let bases = self.base_list(head, name);
        let (head_lines, tail_lines) = self.src.context(scopes);
        let owner = if stack.is_empty() {
            String::new()
        } else {
            let mut o: Vec<String> = Vec::new();
            if !ns.is_empty() {
                o.push(ns.to_string());
            }
            o.extend(stack.iter().cloned());
            join_str(&o, ".")
        };
        let end_off = match sc.2 {
            Some(v) => v,
            None => match sc.3 {
                Some(v) => v,
                None => start,
            },
        };
        let bases_json: Vec<Value> = bases.iter().map(|b| Value::String(b.clone())).collect();
        self.types.push(json!({
            "id": full,
            "name": name,
            "full": full,
            "ns": ns,
            "kind": kind,
            "owner": owner,
            "file": self.src.display,
            "line": self.src.line_at(start),
            "endLine": self.src.line_at(end_off),
            "bases": bases_json,
            "head": head_lines,
            "tail": tail_lines,
        }));
        for bse in bases {
            self.inherits.push((full.clone(), bse));
        }
    }

    /// `_base_list(head, tname)`.
    fn base_list(&self, head: &[char], _tname: &str) -> Vec<String> {
        let m = match type_re().find(head) {
            Some(m) => Some(m),
            None => record_re().find(head),
        };
        let Some(m) = m else { return Vec::new() };
        let mut tail: &[char] = &head[m.end..];
        if let Some(w) = where_re().find(tail) {
            tail = &tail[..w.start];
        }
        let ci = top_level_index(tail, 0, tail.len(), &[':']);
        if ci < 0 {
            return Vec::new();
        }
        tail = &tail[ci as usize + 1..];
        let mut out = Vec::new();
        for part in split_top(tail, ',') {
            let part = strip_ws(&part);
            if part.is_empty() {
                continue;
            }
            if let Some(mm) = ident_re().match_at(part, 0) {
                let g = mm.text().to_string();
                if !is_keyword(&g) {
                    out.push(g);
                }
            }
        }
        out
    }

    /// `_member_class(head, sc)` -> `(kind, name, is_event)`.
    fn member_class(
        &self,
        head: &[char],
        sc: &(usize, Option<usize>, Option<usize>, Option<usize>),
    ) -> (String, String, bool) {
        if implicit_operator_re().is_match(head) {
            let mm = operator_name_re().find(head);
            let name = match mm {
                Some(m) => m.group(1).unwrap_or("").to_string(),
                None => "?".to_string(),
            };
            return ("operator".to_string(), format!("operator {name}"), false);
        }
        if operator_re().is_match(head) {
            let mm = operator_name2_re().find(head);
            let suffix = match mm {
                Some(m) => m.group(1).unwrap_or("").to_string(),
                None => String::new(),
            };
            return ("operator".to_string(), format!("operator{suffix}"), false);
        }
        let pi = top_level_index(head, 0, head.len(), &['(']);
        let first_eq = top_level_index(head, 0, head.len(), &['=']);
        let is_event = event_re().is_match(head);
        let is_arrow_prop = pi < 0
            && first_eq >= 0
            && head[first_eq as usize + 1..(first_eq as usize + 2).min(head.len())] == ['>'];
        if pi >= 0 && (first_eq < 0 || pi < first_eq) {
            let mut nm = name_before_paren(&head[..pi as usize]).unwrap_or_else(|| "member".into());
            if nm.is_empty() {
                nm = "member".to_string();
            }
            if crate::model::lstrip_ws(head).first() == Some(&'~') {
                return ("destructor".to_string(), nm, is_event);
            }
            if nm == self.cur_tname {
                return ("constructor".to_string(), nm, is_event);
            }
            let kind = if is_event { "event" } else { "method" };
            return (kind.to_string(), nm, is_event);
        }
        if is_arrow_prop {
            let kind = if is_event { "event" } else { "property" };
            return (kind.to_string(), mem_name(head), is_event);
        }
        let mut nm = mem_name(head);
        if nm.is_empty() || nm == "member" {
            nm = mem_name(rstrip_chars(head, &[';']));
        }
        if sc.1.is_some() {
            if this_bracket_re().is_match(head) {
                return ("property".to_string(), "this[]".to_string(), is_event);
            }
            let kind = if is_event { "event" } else { "property" };
            return (kind.to_string(), nm, is_event);
        }
        if this_bracket_re().is_match(head) {
            return ("property".to_string(), "this[]".to_string(), is_event);
        }
        let kind = if is_event { "event" } else { "field" };
        (kind.to_string(), nm, is_event)
    }

    /// `_add_member(head, sc, ns, stack, start, scopes=None)`.
    #[allow(clippy::too_many_arguments)]
    fn add_member(
        &mut self,
        head: &[char],
        sc: &(usize, Option<usize>, Option<usize>, Option<usize>),
        ns: &str,
        stack: &[String],
        start: usize,
        scopes: &[Scope],
    ) {
        let hs = strip_ws(head);
        if hs.is_empty() || hs[0] == '#' {
            return;
        }
        let hss = chars_to_string(hs);
        if hss.starts_with("using ") || hss.starts_with("extern ") || hss.starts_with("namespace") {
            return;
        }
        let tname = stack.last().cloned().unwrap_or_default();
        self.cur_tname = tname;
        let (kind, name, is_event) = self.member_class(head, sc);

        let pi = top_level_index(head, 0, head.len(), &['(']);
        let mut params: Vec<Value> = Vec::new();
        let mut ret = String::new();
        if pi >= 0
            && (kind == "method" || kind == "constructor" || kind == "destructor" || kind == "operator")
        {
            let pi = pi as usize;
            let close = match_paren(head, pi);
            let inner: &[char] = if close > pi { &head[pi + 1..close] } else { &[] };
            params = parse_params(inner);
            let before = &head[..pi];
            ret = self.ret_of(before, &name, &kind);
        }

        let head_clean = {
            let trimmed = rstrip_chars(head, &[';', ' ']);
            let collapsed = collapse_ws(trimmed);
            chars_to_string(strip_ws(&collapsed))
        };
        let mut owner_parts: Vec<String> = Vec::new();
        if !ns.is_empty() {
            owner_parts.push(ns.to_string());
        }
        owner_parts.extend(stack.iter().cloned());
        let owner_full = join_str(&owner_parts, ".");

        if (kind == "field" || kind == "property" || kind == "event")
            && !this_bracket_re().is_match(head)
        {
            let mut names = self.declarator_names(head);
            if names.is_empty() {
                names.push(name.clone());
            }
            for nm in names {
                let mid = join_id(&owner_full, &nm);
                let rec = self.member_rec(
                    &mid, &nm, &kind, &head_clean, sc, ns, stack, start, &[], "", is_event, scopes,
                );
                self.members.push(rec);
            }
            return;
        }
        let mid = if kind == "method" || kind == "constructor" || kind == "operator" {
            format!("{}({})", join_id(&owner_full, &name), sig_key(&params))
        } else {
            join_id(&owner_full, &name)
        };
        let rec = self.member_rec(
            &mid, &name, &kind, &head_clean, sc, ns, stack, start, &params, &ret, is_event, scopes,
        );
        self.members.push(rec);
    }

    /// `_declarator_names(head)`.
    fn declarator_names(&self, head: &[char]) -> Vec<String> {
        let h: Vec<char> = strip_default(head);
        let h = rstrip_chars(&h, &[';']);
        let h = strip_ws(h);
        let parts = split_top(h, ',');
        let mut names = Vec::new();
        for part in parts {
            let toks: Vec<String> = ident_re()
                .findall1(&part)
                .into_iter()
                .filter(|t| !is_modifier(t) && !is_keyword(t))
                .collect();
            if let Some(last) = toks.last() {
                names.push(last.clone());
            }
        }
        names
    }

    /// `_ret_of(before, name, kind)`.
    fn ret_of(&self, before: &[char], name: &str, kind: &str) -> String {
        if kind == "constructor" || kind == "destructor" {
            return String::new();
        }
        let mut toks: Vec<String> = ret_token_re()
            .findall1(before)
            .into_iter()
            .filter(|t| !is_modifier(t))
            .collect();
        if let Some(last) = toks.last() {
            if last == name || last.starts_with(&format!("{name}<")) {
                toks.pop();
            }
        }
        join_str(&toks, " ")
    }

    /// `_member_rec(...)`.
    #[allow(clippy::too_many_arguments)]
    fn member_rec(
        &self,
        mid: &str,
        name: &str,
        kind: &str,
        sig: &str,
        sc: &(usize, Option<usize>, Option<usize>, Option<usize>),
        ns: &str,
        stack: &[String],
        start: usize,
        params: &[Value],
        ret: &str,
        is_event: bool,
        scopes: &[Scope],
    ) -> Value {
        let body_open = sc.1;
        let body_close = sc.2;
        let semi = sc.3;
        let mut bstart: Option<usize> = None;
        let mut body_end: Option<usize> = None;
        if let Some(bo) = body_open {
            bstart = Some(bo + 1);
            body_end = body_close;
        } else if let Some(semi) = semi {
            if let Some(arrow) = find_sub(self.s, &['=', '>'], start, semi) {
                bstart = Some(arrow + 2);
                body_end = Some(semi);
            }
        }
        let mut access = "private".to_string();
        for a in crate::model::ACCESS {
            if access_re(a).is_match_str(sig) {
                access = a.to_string();
                break;
            }
        }
        let (head_lines, tail_lines) = self.src.context(scopes);
        let end_off = match body_end {
            Some(v) => v,
            None => match semi {
                Some(v) => v,
                None => start,
            },
        };
        let dend = match body_close {
            Some(v) => v + 1,
            None => match semi {
                Some(v) => v + 1,
                None => start,
            },
        };
        let mut owner_parts: Vec<String> = Vec::new();
        if !ns.is_empty() {
            owner_parts.push(ns.to_string());
        }
        owner_parts.extend(stack.iter().cloned());
        json!({
            "id": mid,
            "name": name,
            "kind": kind,
            "ns": ns,
            "owner": join_str(&owner_parts, "."),
            "ownerKind": "",
            "file": self.src.display,
            "line": self.src.line_at(start),
            "endLine": self.src.line_at(end_off),
            "signature": sig,
            "ret": ret,
            "params": params,
            "access": access,
            "static": static_re().is_match_str(sig),
            "event": is_event,
            "head": head_lines,
            "tail": tail_lines,
            "_bstart": bstart,
            "_bend": body_end,
            "_dstart": start,
            "_dend": dend,
        })
    }
}

/// `re.sub(r'\s+', ' ', s)`.
fn collapse_ws(s: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(s.len());
    let mut prev_ws = false;
    for &c in s {
        if py_is_space(c) {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out
}

/// `_match_paren(s, o)`.
pub fn match_paren(s: &[char], o: usize) -> usize {
    let mut i = o + 1;
    let mut d: i64 = 1;
    let n = s.len();
    while i < n {
        let c = s[i];
        if c == '(' {
            d += 1;
        } else if c == ')' {
            d -= 1;
            if d == 0 {
                return i;
            }
        }
        i += 1;
    }
    n
}

// ---------------------------------------------------------------------------
// reference extraction
// ---------------------------------------------------------------------------
type Tok = (String, usize);

/// `_chain_before(toks, i)`.
pub fn chain_before(toks: &[Tok], i: usize) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut j = i as isize - 1;
    while j >= 1 && (toks[j as usize].0 == "." || toks[j as usize].0 == "?.") {
        let prev = &toks[(j - 1) as usize].0;
        if ident_full_re().is_match_str(prev) {
            parts.insert(0, prev.clone());
            j -= 2;
        } else if prev == ")" || prev == "]" {
            return None;
        } else {
            break;
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(join_str(&parts, "."))
    }
}

/// `_chain_after(toks, i)` -> `(chain, next_index)`.
pub fn chain_after(toks: &[Tok], i: usize) -> (Vec<String>, usize) {
    if i >= toks.len() || !ident_full_re().is_match_str(&toks[i].0) {
        return (Vec::new(), i);
    }
    let mut chain = vec![toks[i].0.clone()];
    let mut k = i + 1;
    while k + 1 < toks.len() && toks[k].0 == "." && ident_full_re().is_match_str(&toks[k + 1].0) {
        chain.push(toks[k + 1].0.clone());
        k += 2;
    }
    (chain, k)
}

/// `extract_refs(body, line_at, base_off)`.
pub fn extract_refs(
    body: &[char],
    source: &Source,
    base_off: usize,
) -> Vec<Value> {
    let mut refs: Vec<Value> = Vec::new();
    let toks: Vec<Tok> = token_re()
        .all(body)
        .into_iter()
        .map(|m| (m.text().to_string(), m.start))
        .collect();
    let n = toks.len();
    let mut i = 0usize;
    while i < n {
        let (t, p) = toks[i].clone();
        if t == "new" {
            let (chain, k) = chain_after(&toks, i + 1);
            if !chain.is_empty() && !is_keyword(&chain[0]) {
                refs.push(json!({
                    "kind": "construct",
                    "name": chain.last().unwrap(),
                    "qual": opt_or_null(&join_str(&chain[..chain.len() - 1], ".")),
                    "dotted": join_str(&chain, "."),
                    "pos": p,
                }));
                i = k;
                continue;
            }
            i += 1;
            continue;
        }
        if t == "typeof" || t == "nameof" || t == "sizeof" || t == "default" {
            if i + 1 < n && toks[i + 1].0 == "(" {
                let (chain, _) = chain_after(&toks, i + 2);
                if !chain.is_empty() {
                    refs.push(json!({
                        "kind": "use",
                        "name": chain.last().unwrap(),
                        "qual": opt_or_null(&join_str(&chain[..chain.len() - 1], ".")),
                        "dotted": join_str(&chain, "."),
                        "pos": p + t.chars().count(),
                    }));
                }
            }
            i += 1;
            continue;
        }
        if t == "is" || t == "as" {
            let (chain, _) = chain_after(&toks, i + 1);
            if !chain.is_empty() && chain[0].chars().next().map(|c| c.is_uppercase()).unwrap_or(false)
            {
                refs.push(json!({
                    "kind": "use",
                    "name": chain.last().unwrap(),
                    "qual": opt_or_null(&join_str(&chain[..chain.len() - 1], ".")),
                    "dotted": join_str(&chain, "."),
                    "pos": p,
                }));
            }
            i += 1;
            continue;
        }
        if ident_full_re().is_match_str(&t) && !is_keyword(&t) {
            let qual = chain_before(&toks, i);
            let mut is_call = false;
            if i + 1 < n && toks[i + 1].0 == "(" {
                is_call = true;
            } else if i + 1 < n && toks[i + 1].0 == "<" {
                let mut d: i64 = 0;
                let mut k = i + 1;
                while k < n {
                    let tk = &toks[k].0;
                    if tk == "<" {
                        d += 1;
                    } else if tk == ">" {
                        d -= 1;
                        if d == 0 {
                            break;
                        }
                    } else if tk == ";" || tk == "{" || tk == "}" {
                        break;
                    }
                    k += 1;
                }
                if k + 1 < n && toks[k].0 == ">" && toks[k + 1].0 == "(" {
                    is_call = true;
                }
            }
            let dotted = match &qual {
                Some(q) => format!("{q}.{t}"),
                None => t.clone(),
            };
            if is_call {
                refs.push(json!({
                    "kind": "call", "name": t, "qual": qual, "dotted": dotted, "pos": p,
                }));
            } else if qual.is_some() {
                refs.push(json!({
                    "kind": "use", "name": t, "qual": qual, "dotted": dotted, "pos": p,
                }));
            }
        }
        i += 1;
    }
    for r in refs.iter_mut() {
        let pos = r["pos"].as_u64().unwrap() as usize;
        let line = source.line_at(base_off + pos);
        r["line"] = json!(line);
    }
    refs
}

/// Python's `x or None` for strings: the empty string becomes `null`.
fn opt_or_null(s: &str) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        Value::String(s.to_string())
    }
}

// ---------------------------------------------------------------------------
// string literals
// ---------------------------------------------------------------------------
/// `decode_string(lit)`.
pub fn decode_string(lit: &[char]) -> String {
    let mut k = 0usize;
    while k < lit.len() && (lit[k] == '@' || lit[k] == '$') {
        k += 1;
    }
    let verbatim = lit[..k].contains(&'@');
    let mut s: &[char] = &lit[k..];
    if s.len() >= 2 && s[0] == '"' && s[s.len() - 1] == '"' {
        s = &s[1..s.len() - 1];
    }
    if verbatim {
        // `s.replace('""', '"')`
        let mut out = String::new();
        let mut i = 0usize;
        while i < s.len() {
            if s[i] == '"' && i + 1 < s.len() && s[i + 1] == '"' {
                out.push('"');
                i += 2;
            } else {
                out.push(s[i]);
                i += 1;
            }
        }
        return out;
    }
    let mut out = String::new();
    let mut i = 0usize;
    while i < s.len() {
        let c = s[i];
        if c == '\\' && i + 1 < s.len() {
            let nxt = s[i + 1];
            out.push(match nxt {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '0' => '\0',
                '\\' => '\\',
                '"' => '"',
                '\'' => '\'',
                other => other,
            });
            i += 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `extract_strings(text, start, end, line_at)`.
pub fn extract_strings(
    text: &[char],
    start: Option<usize>,
    end: Option<usize>,
    source: &Source,
) -> Vec<Value> {
    let mut out = Vec::new();
    let (Some(start), Some(end)) = (start, end) else {
        return out;
    };
    if end <= start {
        return out;
    }
    let mut i = start;
    while i < end {
        let c = text[i];
        if c == '/' && i + 1 < end && text[i + 1] == '/' {
            let j = find_sub(text, &['\n'], i, end);
            i = match j {
                Some(j) => j,
                None => end,
            };
            continue;
        }
        if c == '/' && i + 1 < end && text[i + 1] == '*' {
            let j = find_sub(text, &['*', '/'], i + 2, end);
            i = match j {
                Some(j) => j + 2,
                None => end,
            };
            continue;
        }
        if c == '\'' {
            i = scan_lit(text, i, end);
            continue;
        }
        if c == '"' || ((c == '@' || c == '$') && is_literal_start(text, i, end)) {
            let j = scan_lit(text, i, end);
            let value = decode_string(&text[i..j]);
            let mut k = i as isize - 1;
            while k > start as isize && py_is_space(text[k as usize]) {
                k -= 1;
            }
            let mut assigned = false;
            if k > start as isize && text[k as usize] == '=' {
                let prev = text[(k - 1) as usize];
                if !matches!(prev, '=' | '!' | '<' | '>' | '+' | '-' | '*' | '/' | '%' | '&' | '|' | '^')
                {
                    assigned = true;
                }
            }
            out.push(json!({
                "value": value,
                "pos": i,
                "line": source.line_at(i),
                "assigned": assigned,
            }));
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

/// `collect_var_types(body, params)`.
pub fn collect_var_types(body: &[char], params: &[Value]) -> Value {
    let mut vt: Map<String, Value> = Map::new();
    for p in params {
        let ty = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let nm = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if !ty.is_empty() && !nm.is_empty() {
            vt.insert(nm.to_string(), Value::String(ty.to_string()));
        }
    }
    for m in var_decl_base_re().all(body) {
        // `(?=[=;,)])` by hand: the character after the trailing `\s*` run.
        if m.end >= body.len() || !matches!(body[m.end], '=' | ';' | ',' | ')') {
            continue;
        }
        let ty = m.group(1).unwrap_or("").to_string();
        let nm = m.group(2).unwrap_or("").to_string();
        if is_keyword(&ty) || ty == "var" {
            continue;
        }
        vt.entry(nm).or_insert(Value::String(ty));
    }
    for m in new_var_re().all(body) {
        vt.insert(
            m.group(1).unwrap_or("").to_string(),
            Value::String(m.group(2).unwrap_or("").to_string()),
        );
    }
    for m in foreach_re().all(body) {
        vt.entry(m.group(2).unwrap_or("").to_string())
            .or_insert(Value::String(m.group(1).unwrap_or("").to_string()));
    }
    Value::Object(vt)
}

// ---------------------------------------------------------------------------
// parsing a whole file
// ---------------------------------------------------------------------------
/// `finalize_members(parser, src)`.
pub fn finalize_members(parser: &Parser, src: &Source) -> Vec<Value> {
    let mut out = Vec::new();
    for m in &parser.members {
        let bstart = opt_usize(m, "_bstart");
        let bend = opt_usize(m, "_bend");
        let dstart = opt_usize(m, "_dstart");
        let dend = opt_usize(m, "_dend");
        let mut refs: Vec<Value> = Vec::new();
        let mut vartypes = Value::Object(Map::new());
        if let (Some(bs), Some(be)) = (bstart, bend) {
            if be > bs {
                let body = &src.masked[bs..be];
                refs = extract_refs(body, src, bs);
                vartypes = collect_var_types(body, m["params"].as_array().unwrap());
            }
        }
        let s_start = bstart.or(dstart);
        let s_end = bend.or(dend);
        let strings = extract_strings(&src.text, s_start, s_end, src);
        let mut mm = m.as_object().unwrap().clone();
        mm.insert("refs".to_string(), Value::Array(refs));
        mm.insert("vartypes".to_string(), vartypes);
        mm.insert("strings".to_string(), Value::Array(strings));
        mm.insert("startOff".to_string(), json!(dstart));
        mm.insert("endOff".to_string(), json!(dend));
        mm.insert("_bstart".to_string(), Value::Null);
        mm.insert("_bend".to_string(), Value::Null);
        mm.remove("_dstart");
        mm.remove("_dend");
        out.push(Value::Object(mm));
    }
    out
}

fn opt_usize(v: &Value, key: &str) -> Option<usize> {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_u64().map(|x| x as usize),
        _ => None,
    }
}

/// Decode a file exactly like `open(path, encoding='utf-8', errors='replace')`.
/// Parse every file across the available cores, returning results in `files`
/// order.
///
/// `parse_source` only reads, and the per-file result does not depend on any
/// other file (cross-file links are resolved afterwards by `build_graph`), so
/// the fan-out is safe. Files are chunked contiguously and each chunk keeps its
/// index, so the output order is identical to the sequential loop.
pub fn parse_files_parallel(
    files: &[String],
    repo: &Path,
    defines: Option<&HashSet<String>>,
) -> Vec<Value> {
    let n = files.len();
    if n == 0 {
        return Vec::new();
    }
    let workers = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1)
        .min(n)
        .max(1);
    if workers <= 1 {
        return files
            .iter()
            .map(|f| {
                let p = Path::new(f);
                let display = display_path(p, repo);
                parse_source(p, &display, defines)
            })
            .collect();
    }
    let chunk = n.div_ceil(workers);
    let mut out: Vec<(usize, Value)> = Vec::with_capacity(n);
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for w in 0..workers {
            let start = w * chunk;
            if start >= n {
                break;
            }
            let end = (start + chunk).min(n);
            handles.push(scope.spawn(move || {
                let mut local: Vec<(usize, Value)> = Vec::with_capacity(end - start);
                for (i, f) in files.iter().enumerate().take(end).skip(start) {
                    let p = Path::new(f);
                    let display = display_path(p, repo);
                    local.push((i, parse_source(p, &display, defines)));
                }
                local
            }));
        }
        for h in handles {
            if let Ok(local) = h.join() {
                out.extend(local);
            }
        }
    });
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, v)| v).collect()
}

/// `_read_text(path)` — the Python source reader (`errors="replace"`).
pub fn read_python_text(path: &Path) -> std::io::Result<Vec<char>> {
    let bytes = std::fs::read(path)?;
    let lossy = String::from_utf8_lossy(&bytes);
    // universal newlines: \r\n and \r both become \n
    let mut out: Vec<char> = Vec::with_capacity(lossy.len());
    let mut it = lossy.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\r' {
            if it.peek() == Some(&'\n') {
                it.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

/// `parse_source(path, display, defines=None)`.
pub fn parse_source(path: &Path, display: &str, defines: Option<&HashSet<String>>) -> Value {
    let text = read_python_text(path).unwrap_or_default();
    let probe = std::env::var_os("REORG_TIME").is_some();
    let t0 = std::time::Instant::now();
    let src = Source::new(path.to_string_lossy().to_string(), display.to_string(), text, defines);
    let d_src = t0.elapsed();
    let t1 = std::time::Instant::now();
    let mut p = Parser::new(&src);
    p.parse();
    let d_parse = t1.elapsed();
    let t2 = std::time::Instant::now();
    let members = finalize_members(&p, &src);
    let d_fin = t2.elapsed();
    if probe {
        eprintln!(
            "[parse] {display}: src {:?}, parse {:?}, finalize {:?} ({} chars)",
            d_src,
            d_parse,
            d_fin,
            src.masked.len()
        );
    }
    let inherits: Vec<Value> = p
        .inherits
        .iter()
        .map(|(a, b)| json!([a, b]))
        .collect();
    json!({
        "file": display,
        "types": p.types,
        "members": members,
        "inherits": inherits,
        "warnings": p.warnings,
    })
}

// ---------------------------------------------------------------------------
// discovery
// ---------------------------------------------------------------------------
/// `_excluded(rel, parts, excludes)`.
///
/// **Hot path.** `parts` already contains every path component of `rel`, so a
/// glob-free exclude (`target`, `cache`, …) is fully decided by
/// `parts.contains(ex)`: `fnmatch(rel, ex)`, `fnmatch(rel, "*/{ex}")` and
/// `fnmatch(rel, "{ex}/*")` can only match when some component equals `ex`.
/// Skipping the `fnmatch` calls for plain names matters because `fnmatch`
/// builds a fresh matcher via `fnmatch_translate` on **every** call, and this
/// function runs once per directory with up to three calls per exclude — the
/// measured 65 % discovery hotspot.
pub fn excluded(rel: &str, parts: &HashSet<String>, excludes: &[String]) -> bool {
    for ex in excludes {
        if ex.is_empty() {
            continue;
        }
        if parts.contains(ex) {
            return true;
        }
        if !has_glob(ex) {
            continue;
        }
        if fnmatch(rel, ex) || fnmatch(rel, &format!("*/{ex}")) {
            return true;
        }
        if fnmatch(rel, &format!("{ex}/*")) {
            return true;
        }
    }
    false
}

/// `true` when `pat` can match more than one literal name.
fn has_glob(pat: &str) -> bool {
    pat.bytes().any(|b| matches!(b, b'*' | b'?' | b'['))
}

/// `is_cs(name)`.
pub fn is_cs(name: &str) -> bool {
    name.ends_with(".cs") || name.ends_with(".cs.nzk")
}

/// `fnmatch.fnmatch(name, pat)` on POSIX (`normcase` is the identity).
pub fn fnmatch(name: &str, pat: &str) -> bool {
    let re = fnmatch_translate(pat);
    match re {
        Some(r) => r.is_match_str(name),
        None => false,
    }
}

fn fnmatch_translate(pat: &str) -> Option<Re> {
    let mut out = String::from("(?s:^");
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0usize;
    let n = chars.len();
    while i < n {
        let c = chars[i];
        i += 1;
        if c == '*' {
            out.push_str(".*");
        } else if c == '?' {
            out.push('.');
        } else if c == '[' {
            let mut j = i;
            if j < n && (chars[j] == '!' || chars[j] == '^') {
                j += 1;
            }
            if j < n && chars[j] == ']' {
                j += 1;
            }
            while j < n && chars[j] != ']' {
                j += 1;
            }
            if j >= n {
                out.push_str("\\[");
            } else {
                let mut stuff: String = chars[i..j].iter().collect();
                i = j + 1;
                if stuff.starts_with('!') {
                    stuff.replace_range(0..1, "^");
                }
                stuff = stuff.replace('\\', "\\\\");
                out.push('[');
                out.push_str(&stuff);
                out.push(']');
            }
        } else {
            out.push_str(&regex_escape_char(c));
        }
    }
    out.push_str(")$");
    Re::new_checked(&out)
}

fn regex_escape_char(c: char) -> String {
    if ".^$*+?()[]{}|\\".contains(c) {
        format!("\\{c}")
    } else {
        c.to_string()
    }
}

/// `discover_files(base, roots, excludes=())`.
///
/// Symlinks are followed — a project routinely links a shared tree in — but
/// the real directory behind a path is walked at most once. A link back up the
/// tree, or two links onto the same folder, therefore contributes each file
/// exactly once instead of looping or duplicating it, and when a file is
/// reachable both directly and through a link the direct path is the one kept.
pub fn discover_files(base: &Path, roots: &[String], excludes: &[String]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // Directory identity is `(st_dev, st_ino)` from the lstat we already make
    // for the entry: no `canonicalize` per directory, and still a full cycle
    // guard for symlinks, bind mounts and overlayfs duplicates alike.
    let mut visited: HashSet<(u64, u64)> = HashSet::new();
    for root in roots {
        let p = if Path::new(root).is_absolute() {
            PathBuf::from(root)
        } else {
            base.join(root)
        };
        let p = normalize_path(&p);
        let Ok(md) = std::fs::metadata(&p) else { continue };
        if md.is_file() {
            let s = p.to_string_lossy().to_string();
            if is_cs(&s) && seen.insert(s.clone()) {
                files.push(s);
            }
            continue;
        }
        if !md.is_dir() {
            continue;
        }
        let ident = ident_of(&md);
        walk_collect(&p, "", excludes, &mut files, &mut seen, &mut visited, ident);
    }
    files.sort();
    files
}

/// `(st_dev, st_ino)` — one directory's identity, without `canonicalize`.
fn ident_of(md: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (md.dev(), md.ino())
}

fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        use std::path::Component::*;
        match comp {
            CurDir => {}
            ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `true` when the path itself is a link rather than the thing it names.
#[allow(dead_code)]
fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Walk one directory tree.
///
/// `ident` is the directory's `(st_dev, st_ino)` identity; it is inserted into
/// `visited` before the directory is read, so a symlink cycle, a second link
/// onto the same tree, or the same directory surfaced twice by a bind mount /
/// overlay filesystem is walked exactly once. This replaces a `canonicalize`
/// per directory (several syscalls) with one integer-pair insert.
///
/// Per entry this makes **one** `symlink_metadata` call instead of `is_dir()`
/// plus `is_link()` (two stats); a symlinked entry's target is stat'ed once,
/// for both its kind and its identity.
fn walk_collect(
    dir: &Path,
    rel: &str,
    excludes: &[String],
    files: &mut Vec<String>,
    seen: &mut HashSet<String>,
    visited: &mut HashSet<(u64, u64)>,
    ident: (u64, u64),
) {
    // Identity, not path: inserted before the directory is read, so a cycle or
    // a second name for the same directory stops here.
    if !visited.insert(ident) {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    // Real subfolders are walked before linked ones, so the untouched tree
    // claims its identity slot and keeps its own paths in the index.
    let mut dirs: Vec<(String, PathBuf, (u64, u64))> = Vec::new();
    let mut linked_dirs: Vec<(String, PathBuf, (u64, u64))> = Vec::new();
    for path in entries {
        let name = match path.file_name() {
            Some(n) => n.to_string_lossy().to_string(),
            None => continue,
        };
        // One lstat per entry: `file_type()` from the same call tells us
        // file/dir/link, instead of `is_dir()` + `is_link()` (two stats).
        let Ok(md) = std::fs::symlink_metadata(&path) else { continue };
        let ft = md.file_type();
        if ft.is_symlink() {
            match std::fs::metadata(&path) {
                Ok(target) if target.is_dir() => {
                    linked_dirs.push((name, path, ident_of(&target)))
                }
                Ok(target) if target.is_file() && is_cs(&name) => {
                    let s = path.to_string_lossy().to_string();
                    if seen.insert(s.clone()) {
                        files.push(s);
                    }
                }
                _ => {}
            }
        } else if ft.is_dir() {
            let id = ident_of(&md);
            dirs.push((name, path, id));
        } else if is_cs(&name) {
            let s = path.to_string_lossy().to_string();
            if seen.insert(s.clone()) {
                files.push(s);
            }
        }
    }
    for (name, path, id) in dirs.into_iter().chain(linked_dirs) {
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let mut parts: HashSet<String> = HashSet::new();
        for part in child_rel.split('/') {
            if !part.is_empty() {
                parts.insert(part.to_string());
            }
        }
        if excluded(&child_rel, &parts, excludes) {
            continue;
        }
        walk_collect(&path, &child_rel, excludes, files, seen, visited, id);
    }
}

/// `server.py::Index.display` — repo-relative when inside the repo.
pub fn display_path(path: &Path, repo: &Path) -> String {
    let p = normalize_path(path);
    let r = normalize_path(repo);
    match p.strip_prefix(&r) {
        Ok(rest) => {
            let s = rest.to_string_lossy().replace('\\', "/");
            if s.is_empty() {
                ".".to_string()
            } else {
                s
            }
        }
        Err(_) => p.to_string_lossy().replace('\\', "/"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::to_chars;

    /// The skipped-`fnmatch` fast path must not change which paths are excluded.
    ///
    /// `parts` is, by contract, the component set **of `rel`** — both real
    /// callers (`walk_collect`, `bounded_walk_check`) build it by splitting the
    /// same path they pass. The first version of this test passed a component
    /// set taken from a different path (`a/target` while querying `a/src`),
    /// which made `parts.contains("target")` fire; that input is not one the
    /// function supports, and the test was wrong, not the code.
    #[test]
    fn excluded_plain_names_and_globs_agree() {
        fn parts_of(rel: &str) -> HashSet<String> {
            rel.split('/')
                .filter(|p| !p.is_empty())
                .map(|s| s.to_string())
                .collect()
        }
        let ex = vec!["target".to_string(), "*.gen".to_string(), "obj".to_string()];
        // plain name: the component set decides it
        assert!(excluded("a/target", &parts_of("a/target"), &ex));
        assert!(excluded("a/target/x", &parts_of("a/target/x"), &ex));
        assert!(!excluded("a/src", &parts_of("a/src"), &ex));
        assert!(!excluded("a/objx", &parts_of("a/objx"), &ex));
        // glob: still matched by fnmatch even when no component equals it
        assert!(excluded("a/foo.gen", &parts_of("a/foo.gen"), &ex));
        assert!(excluded("foo.gen", &parts_of("foo.gen"), &ex));
    }

    /// A symlink cycle and a second name for the same directory must each be
    /// walked once: the identity guard is `(st_dev, st_ino)`, not the path.
    #[test]
    fn discover_files_visits_a_symlink_cycle_once() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("csgraph-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("a/b")).unwrap();
        std::fs::write(base.join("a/One.cs"), "class One {}").unwrap();
        std::fs::write(base.join("a/b/Two.cs"), "class Two {}").unwrap();
        symlink(base.join("a"), base.join("a/loop")).unwrap();
        symlink(base.join("a/b"), base.join("a/b/sibling")).unwrap();

        let got = discover_files(&base, &[".".to_string()], &[]);
        let names: Vec<String> = got
            .iter()
            .map(|p| p.rsplit('/').next().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["One.cs".to_string(), "Two.cs".to_string()]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_param_strips_modifiers_and_brackets() {
        // Python ground truth for these exact inputs:
        //   {'name': 'args', 'type': 'object[]'}
        //   {'name': 'xs',   'type': 'int[]'}
        //   {'name': 'x',    'type': 'int'}
        // The array suffix is part of the type, so it is kept.
        let p = parse_param(&to_chars("params object[] args")).unwrap();
        assert_eq!(p["name"], "args");
        assert_eq!(p["type"], "object[]");
        let p = parse_param(&to_chars("this int[] xs")).unwrap();
        assert_eq!(p["name"], "xs");
        assert_eq!(p["type"], "int[]");
        let p = parse_param(&to_chars("int x = 3")).unwrap();
        assert_eq!(p["name"], "x");
        assert_eq!(p["type"], "int");
    }

    #[test]
    fn decode_string_handles_verbatim_and_escapes() {
        assert_eq!(decode_string(&to_chars(r#""a\\nb""#)), "a\\nb");
        assert_eq!(decode_string(&to_chars(r#""a\nb""#)), "a\nb");
        // Python: decode_string('@""a""b"') == '"a"b' — the leading quote is
        // part of the result for this (deliberately odd) input.
        assert_eq!(decode_string(&to_chars(r#"@""a""b""#)), "\"a\"b");
    }

    #[test]
    fn fnmatch_translates_basics() {
        assert!(fnmatch("obj", "obj"));
        // Python fnmatch('a/obj', 'obj') is False: without a wildcard it is an
        // exact match, and '/' is an ordinary character to fnmatch.
        assert!(!fnmatch("a/obj", "obj"));
        assert!(fnmatch("obj/x.cs", "obj/*"));
        assert!(fnmatch("x.g.cs", "*.g.cs"));
        assert!(!fnmatch("objx", "obj"));
    }

    fn parse_types(text: &str) -> Vec<Value> {
        let src = Source::new(
            "mem.cs".to_string(),
            "mem.cs".to_string(),
            to_chars(text),
            None,
        );
        let mut p = Parser::new(&src);
        p.parse();
        p.types
    }

    fn parsed_type_ids(text: &str) -> Vec<String> {
        parse_types(text)
            .iter()
            .map(|t| t["id"].as_str().unwrap_or("").to_string())
            .collect()
    }

    /// `namespace A { namespace B { … } }` declares the namespace `A.B`, and
    /// the outer `A` must still become a namespace in the graph. Before this
    /// was handled, the top-level scan started the inner block a second time
    /// with the bare name `B`, so `A` produced no namespace node at all and a
    /// corpus with several namespaces could collapse to a single one.
    #[test]
    fn nested_namespace_keeps_parent_name() {
        let types = parse_types(
            "namespace Company\n{\n    namespace Product\n    {\n        class A { }\n    }\n}\n",
        );
        assert_eq!(types.len(), 1);
        assert_eq!(types[0]["id"], "Company.Product.A");
        assert_eq!(types[0]["ns"], "Company.Product");
    }

    /// A namespace declared in a sibling block stays top-level, and a
    /// file-scoped namespace is not swallowed by the block before it.
    #[test]
    fn sibling_and_filescoped_namespaces() {
        assert_eq!(
            parsed_type_ids(
                "namespace One { class A { } }\nnamespace Two { class B { } }\n"
            ),
            vec!["One.A".to_string(), "Two.B".to_string()]
        );
        assert_eq!(
            parsed_type_ids("namespace Solo;\nclass C { }\n"),
            vec!["Solo.C".to_string()]
        );
    }

    /// A dotted file-scoped namespace keeps the full path on its own:
    /// `namespace O.P;` then a class yields `O.P.C`.
    #[test]
    fn dotted_filescoped_namespace() {
        assert_eq!(
            parsed_type_ids("namespace O.P;\nclass C { }\n"),
            vec!["O.P.C".to_string()]
        );
    }


    /// Files reached only through a symlinked folder are part of the project
    /// and must be discovered — but a file that is reachable twice must not be
    /// indexed twice. The untouched tree wins over the link that points at it.
    #[cfg(unix)]
    #[test]
    fn discover_files_follows_symlinks_without_duplicating() {
        let dir = std::env::temp_dir().join(format!("csgraph-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pkg")).unwrap();
        std::fs::write(dir.join("pkg/Deep.cs"), "class Deep {}\n").unwrap();
        std::fs::write(dir.join("Top.cs"), "class Top {}\n").unwrap();
        std::os::unix::fs::symlink(dir.join("pkg"), dir.join("aliased")).unwrap();

        let roots = vec![".".to_string()];
        let files = discover_files(&dir, &roots, &[]);
        let prefix = format!("{}/", dir.to_string_lossy());
        let rel: Vec<String> = files.iter().map(|f| f.replace(&prefix, "")).collect();

        assert!(
            rel.iter().any(|f| f == "pkg/Deep.cs"),
            "real file missing: {rel:?}"
        );
        assert!(
            rel.iter().any(|f| f == "Top.cs"),
            "root file missing: {rel:?}"
        );
        assert_eq!(
            rel.iter().filter(|f| f.ends_with("Deep.cs")).count(),
            1,
            "the same file was indexed once per path: {rel:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A link pointing back up its own tree must not be walked forever.
    #[cfg(unix)]
    #[test]
    fn discover_files_survives_symlink_loop() {
        let dir = std::env::temp_dir().join(format!("csgraph-loop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pkg/inner")).unwrap();
        std::fs::write(dir.join("pkg/inner/Deep.cs"), "class Deep {}\n").unwrap();
        std::os::unix::fs::symlink(dir.join("pkg/inner"), dir.join("pkg/inner/loop")).unwrap();

        let roots = vec![".".to_string()];
        let files = discover_files(&dir, &roots, &[]);
        let prefix = format!("{}/", dir.to_string_lossy());
        let rel: Vec<String> = files.iter().map(|f| f.replace(&prefix, "")).collect();

        assert_eq!(rel, vec!["pkg/inner/Deep.cs".to_string()], "loop changed the file set: {rel:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
