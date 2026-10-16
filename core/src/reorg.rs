//! reorg — port of `tools/csgraph/cs_reorg.py`.
//!
//! The reorganisation engine: MERGE (every declaration the graph knows about,
//! written as one compilable C# file) and SPLIT (the same declarations, grouped
//! into several files that compile to the same thing), plus the structural
//! `verify()` that proves nothing moved or changed.
//!
//! Like the Python original this never re-implements text extraction: it drives
//! [`crate::doc::DocBuilder`] once per selected unit and stitches the fragments
//! into files. Nodes and file texts are [`serde_json::Value`] maps, exactly the
//! shapes `server.py` hands the original, so the engine is callable from any
//! graph builder and from `csgraph dump-reorg`.
//!
//! Fidelity notes:
//!
//! * every offset and slice is a *character* offset, matching Python `str`;
//! * the `_HEAD_RE` / `_PARTIAL_RE` / `_HEAD_ANY_RE` patterns and `_slug` are
//!   recreated verbatim;
//! * `_norm` collapses whitespace with the port's `py_is_space` predicate so the
//!   C1 separators Python's `\s` accepts are handled the same way;
//! * ZIP bytes are not required to match Python's `zipfile` (nothing compares
//!   them) — only the entry names, order and archive validity;
//! * malformed input yields `Err` / a `{ok: false}` report instead of a panic,
//!   mirroring the `Result`-style failure paths `server.py` catches per call.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::sync::OnceLock;

use serde_json::{json, Map, Value};

use crate::doc::{DocBuilder, DocCache, TYPE_KINDS};
use crate::graph::build_graph;
use crate::hash::sha1_hex;
use crate::lex::mask_noncode;
use crate::model::{
    chars_to_string, lstrip_ws, py_is_space, strip_ws, to_chars, Re,
};
use crate::parser::{
    discover_files, display_path, finalize_members, parse_source, read_python_text, Parser,
    Source,
};

// ---------------------------------------------------------------------------
// module constants (cs_reorg.py)
// ---------------------------------------------------------------------------
/// `MEMBER_KINDS`.
pub const MEMBER_KINDS: [&str; 6] = [
    "method",
    "constructor",
    "operator",
    "property",
    "field",
    "event",
];
/// `DATA_KINDS` — order-sensitive members that are never scattered.
pub const DATA_KINDS: [&str; 3] = ["field", "property", "event"];
/// `NON_UNIT_KINDS` — never movable.
pub const NON_UNIT_KINDS: [&str; 3] = ["namespace", "string", "external"];
/// `DEFAULT_MAX_LINES`.
pub const DEFAULT_MAX_LINES: usize = 400000;

fn is_type_kind(k: &str) -> bool {
    TYPE_KINDS.contains(&k)
}
fn is_member_kind(k: &str) -> bool {
    MEMBER_KINDS.contains(&k)
}
fn is_data_kind(k: &str) -> bool {
    DATA_KINDS.contains(&k)
}
fn is_unit_kind(k: &str) -> bool {
    is_type_kind(k) || is_member_kind(k)
}
fn is_non_unit_kind(k: &str) -> bool {
    NON_UNIT_KINDS.contains(&k)
}

// ---------------------------------------------------------------------------
// regexes
// ---------------------------------------------------------------------------
const HEAD_PATTERN: &str = concat!(
    r"^(?P<indent>[ \t]*)",
    r"(?P<mods>(?:(?:public|private|protected|internal|static|sealed|abstract",
    r"|unsafe|new|readonly|ref|file)\s+)*)",
    r"(?P<kw>class|struct|interface|record)\b"
);

fn head_re() -> &'static Re {
    static R: OnceLock<Re> = OnceLock::new();
    R.get_or_init(|| Re::new(HEAD_PATTERN))
}

fn head_any_re() -> &'static Re {
    static R: OnceLock<Re> = OnceLock::new();
    R.get_or_init(|| Re::new(HEAD_PATTERN.trim_start_matches('^')))
}

fn partial_re() -> &'static Re {
    static R: OnceLock<Re> = OnceLock::new();
    R.get_or_init(|| Re::new(r"\bpartial\b"))
}

fn name_re() -> &'static Re {
    static R: OnceLock<Re> = OnceLock::new();
    R.get_or_init(|| Re::new(r"[A-Za-z_]\w*"))
}

fn directive_re() -> &'static Re {
    static R: OnceLock<Re> = OnceLock::new();
    R.get_or_init(|| Re::new(r"#\s*(if|elif|else|endif)\b"))
}

// ---------------------------------------------------------------------------
// value helpers — Python `or` / `int()` / truthiness
// ---------------------------------------------------------------------------
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n
            .as_i64()
            .map(|i| i != 0)
            .or_else(|| n.as_u64().map(|u| u != 0))
            .or_else(|| n.as_f64().map(|f| f != 0.0))
            .unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn truthy_get(v: &Value, key: &str) -> bool {
    v.get(key).map(truthy).unwrap_or(false)
}

/// `v.get(key) or ''`.
fn s_or_empty(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}

/// `int(value)` for the JSON shapes the parser emits; `None` where Python
/// would raise (the caller treats that as "unusable offset").
fn to_int(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else if let Some(u) = n.as_u64() {
                i64::try_from(u).ok()
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() {
                    Some(f.trunc() as i64)
                } else {
                    None
                }
            } else {
                None
            }
        }
        Value::String(s) => parse_int_string(s),
        _ => None,
    }
}

fn parse_int_string(s: &str) -> Option<i64> {
    let cs = to_chars(s);
    let t = strip_ws(&cs);
    if t.is_empty() {
        return None;
    }
    let mut i = 0usize;
    let mut neg = false;
    if t[0] == '+' || t[0] == '-' {
        neg = t[0] == '-';
        i = 1;
    }
    let mut digits = String::new();
    let mut any = false;
    while i < t.len() {
        let c = t[i];
        if let Some(d) = c.to_digit(10) {
            digits.push(std::char::from_digit(d, 10).unwrap());
            any = true;
        } else {
            return None;
        }
        i += 1;
    }
    if !any {
        return None;
    }
    let mag = i64::from_str_radix(&digits, 10).ok()?;
    Some(if neg { -mag } else { mag })
}

/// `int(value or default)`.
fn int_or(v: Option<&Value>, default: i64) -> Option<i64> {
    match v {
        Some(x) if truthy(x) => to_int(Some(x)),
        _ => Some(default),
    }
}

/// `_slug(text, limit=110)`.
fn slug(text: &str, limit: usize) -> String {
    let mut out = String::new();
    let mut in_run = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('_');
            in_run = true;
        }
    }
    let trimmed = out.trim_matches('_');
    let s = if trimmed.is_empty() { "node" } else { trimmed };
    s.chars().take(limit).collect()
}

/// `text.lstrip().startswith('[')`.
fn lstrip_starts_bracket(text: &str) -> bool {
    lstrip_ws(&to_chars(text)).first() == Some(&'[')
}

/// `s[a:b]` with Python-style negative indices and clamping.
fn py_slice(s: &[char], a: i64, b: i64) -> Vec<char> {
    let n = s.len() as i64;
    let mut a = if a < 0 { (n + a).max(0) } else { a.min(n) };
    let mut b = if b < 0 { (n + b).max(0) } else { b.min(n) };
    if b < a {
        b = a;
    }
    a = a.min(b);
    s[a as usize..b as usize].to_vec()
}

// ---------------------------------------------------------------------------
// normalise_nodes / ensure_partial / can_split
// ---------------------------------------------------------------------------
/// `normalise_nodes(nodes)`.
pub fn normalise_nodes(nodes: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (nid, n) in nodes {
        let mut d = n.as_object().cloned().unwrap_or_default();
        let mut parts_val = d.get("parts").cloned().unwrap_or(Value::Null);
        if parts_val.is_null() {
            parts_val = d.get("_parts").cloned().unwrap_or(Value::Null);
        }
        let arr = match parts_val {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        let parts: Vec<Value> = arr
            .into_iter()
            .map(|p| Value::Object(p.as_object().cloned().unwrap_or_default()))
            .collect();
        d.insert("parts".to_string(), Value::Array(parts));
        out.insert(nid.clone(), Value::Object(d));
    }
    out
}

/// `ensure_partial(line)`.
pub fn ensure_partial(line: &str) -> String {
    let cs = to_chars(line);
    let Some(m) = head_re().match_at(&cs, 0) else {
        return line.to_string();
    };
    let indent = m.group(1).unwrap_or("");
    let mods = m.group(2).unwrap_or("");
    if partial_re().is_match(&to_chars(mods)) {
        return line.to_string();
    }
    let mods_end =
        m.start + indent.chars().count() + mods.chars().count();
    let rest = chars_to_string(&cs[mods_end.min(cs.len())..]);
    format!("{indent}{mods}partial {rest}")
}

/// `can_split(kind)`.
pub fn can_split(kind: &str) -> bool {
    is_type_kind(kind) && kind != "enum"
}

// ---------------------------------------------------------------------------
// grouping helper
// ---------------------------------------------------------------------------
#[derive(Clone)]
struct Group {
    opens: Vec<Value>,
    closes: Vec<Value>,
    bodies: Vec<(String, Vec<Value>)>,
}

// ---------------------------------------------------------------------------
// the engine
// ---------------------------------------------------------------------------
/// `class Reorg` — merge / split / verify a corpus held as (nodes, file_texts).
pub struct Reorg {
    nodes: Map<String, Value>,
    file_texts: HashMap<String, String>,
    defines: Option<HashSet<String>>,
    defines_known: bool,
    max_lines: usize,
    doc_cache: DocCache,
    frag_cache: RefCell<HashMap<String, String>>,
    rec_cache: RefCell<HashMap<String, Vec<Value>>>,
    site_cache: RefCell<HashMap<String, Vec<Vec<Value>>>>,
    child_index: RefCell<Option<HashMap<String, HashSet<String>>>>,
    /// full type name -> simple name, for the partial rewriter.
    type_names: HashMap<String, String>,
}

impl Reorg {
    /// `Reorg.__init__`.
    pub fn new(
        nodes: &Map<String, Value>,
        file_texts: &HashMap<String, String>,
        defines: Option<&HashSet<String>>,
        defines_known: bool,
        max_lines: Option<usize>,
    ) -> Reorg {
        let nodes = normalise_nodes(nodes);
        let mut type_names = HashMap::new();
        for n in nodes.values() {
            let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if is_type_kind(kind) {
                let key = n
                    .get("full")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .or_else(|| n.get("id").and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .to_string();
                let name = n
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or("")
                    .to_string();
                type_names.insert(key, name);
            }
        }
        Reorg {
            nodes,
            file_texts: file_texts.clone(),
            defines: defines.cloned(),
            defines_known,
            max_lines: max_lines.unwrap_or(DEFAULT_MAX_LINES),
            doc_cache: DocCache::new(),
            frag_cache: RefCell::new(HashMap::new()),
            rec_cache: RefCell::new(HashMap::new()),
            site_cache: RefCell::new(HashMap::new()),
            child_index: RefCell::new(None),
            type_names,
        }
    }

    fn kind_of(&self, uid: &str) -> String {
        self.nodes
            .get(uid)
            .and_then(|n| n.get("kind"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    // -- units --------------------------------------------------------------
    /// `unit_ids()`.
    pub fn unit_ids(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .nodes
            .iter()
            .filter(|(_, n)| {
                is_unit_kind(n.get("kind").and_then(|v| v.as_str()).unwrap_or(""))
            })
            .map(|(nid, _)| nid.clone())
            .collect();
        out.sort();
        out
    }

    /// `owner_chain(uid)` — enclosing type ids, outermost first.
    pub fn owner_chain(&self, uid: &str) -> Vec<String> {
        let mut chain: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut cur = self.nodes.get(uid);
        while let Some(n) = cur {
            let owner = n.get("owner").and_then(|v| v.as_str()).unwrap_or("");
            if owner.is_empty() || seen.contains(owner) || !self.nodes.contains_key(owner) {
                break;
            }
            seen.insert(owner.to_string());
            chain.push(owner.to_string());
            cur = self.nodes.get(owner);
        }
        chain.reverse();
        chain
    }

    /// `top_owner(uid)`.
    pub fn top_owner(&self, uid: &str) -> String {
        let chain = self.owner_chain(uid);
        chain.first().cloned().unwrap_or_else(|| uid.to_string())
    }

    /// `has_sites(uid)`.
    pub fn has_sites(&self, uid: &str) -> bool {
        let Some(n) = self.nodes.get(uid) else {
            return false;
        };
        if n.get("parts")
            .and_then(|v| v.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
        {
            return true;
        }
        let file = n.get("file").and_then(|v| v.as_str()).unwrap_or("");
        let line = n.get("line");
        !file.is_empty() && line.map(truthy).unwrap_or(false)
    }

    /// `_children()` — owner id -> set of movable children, barrier-aware.
    fn has_unit_child(&self, uid: &str) -> bool {
        {
            let borrowed = self.child_index.borrow();
            if let Some(idx) = borrowed.as_ref() {
                return idx.get(uid).map(|s| !s.is_empty()).unwrap_or(false);
            }
        }
        let mut idx: HashMap<String, HashSet<String>> = HashMap::new();
        for u in self.unit_ids() {
            if !self.has_sites(&u) {
                continue;
            }
            let b = self.barrier(&u);
            let chain = self.owner_chain(&b);
            let parent = chain.last().cloned().unwrap_or_default();
            idx.entry(parent).or_default().insert(b);
        }
        let res = idx.get(uid).map(|s| !s.is_empty()).unwrap_or(false);
        *self.child_index.borrow_mut() = Some(idx);
        res
    }

    /// `_barrier(uid)`.
    fn barrier(&self, uid: &str) -> String {
        for t in self.owner_chain(uid) {
            if !can_split(&self.kind_of(&t)) {
                return t;
            }
        }
        uid.to_string()
    }

    /// `select(ids, granularity)`.
    pub fn select(&self, ids: Option<Vec<String>>, granularity: &str) -> Vec<String> {
        let mut chosen: HashSet<String> = HashSet::new();
        match ids {
            Some(list) if !list.is_empty() => {
                for i in list {
                    if let Some(n) = self.nodes.get(&i) {
                        let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                        if is_unit_kind(kind) && self.has_sites(&i) {
                            chosen.insert(self.barrier(&i));
                        }
                    }
                }
            }
            _ => {
                if granularity == "leaf" {
                    for i in self.unit_ids() {
                        if self.has_sites(&i) && !self.has_unit_child(&i) {
                            chosen.insert(self.barrier(&i));
                        }
                    }
                } else {
                    for i in self.unit_ids() {
                        if self.owner_chain(&i).is_empty() && self.has_sites(&i) {
                            chosen.insert(i);
                        }
                    }
                }
            }
        }
        let mut sorted: Vec<String> = chosen.iter().cloned().collect();
        sorted.sort();
        let mut keep: Vec<String> = Vec::new();
        let mut seen_span: HashSet<(String, i64, i64)> = HashSet::new();
        for uid in sorted {
            if self.owner_chain(&uid).iter().any(|a| chosen.contains(a)) {
                continue;
            }
            if let Some(span) = self.span_key(&uid) {
                if seen_span.contains(&span) {
                    continue;
                }
                seen_span.insert(span);
            }
            keep.push(uid);
        }
        keep
    }

    /// `_span_key(uid)`.
    pub fn span_key(&self, uid: &str) -> Option<(String, i64, i64)> {
        let n = self.nodes.get(uid)?;
        let sites = n.get("parts").and_then(|v| v.as_array());
        let mut so = n.get("startOff");
        let mut eo = n.get("endOff");
        let mut f = n.get("file").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if let Some(sites) = sites {
            if let Some(p) = sites.first() {
                if p.get("startOff").is_some() {
                    so = p.get("startOff");
                }
                if p.get("endOff").is_some() {
                    eo = p.get("endOff");
                }
                if f.is_empty() {
                    f = p.get("file").and_then(|v| v.as_str()).unwrap_or("").to_string();
                }
            }
        }
        if f.is_empty() {
            return None;
        }
        let so = to_int(so)?;
        let eo = to_int(eo)?;
        Some((f, so, eo))
    }

    // -- fragments ----------------------------------------------------------
    /// `site_records(uid)`.
    pub fn site_records(&self, uid: &str) -> Vec<Vec<Value>> {
        {
            let cache = self.site_cache.borrow();
            if let Some(v) = cache.get(uid) {
                return v.clone();
            }
        }
        let doc = {
            let builder = DocBuilder::new(
                &self.nodes,
                &self.file_texts,
                self.defines.as_ref(),
                self.defines_known,
                &self.doc_cache,
            );
            builder.build(uid, self.max_lines).ok().flatten()
        };
        let mut sites: Vec<Vec<Value>> = Vec::new();
        let mut cur: Vec<Value> = Vec::new();
        if let Some(lines) = doc
            .as_ref()
            .and_then(|d| d.get("lines"))
            .and_then(|v| v.as_array())
        {
            for r in lines {
                if r.get("ctx").and_then(|v| v.as_str()) == Some("part") {
                    if !cur.is_empty() {
                        sites.push(std::mem::take(&mut cur));
                    }
                    continue;
                }
                cur.push(r.clone());
            }
        }
        if !cur.is_empty() {
            sites.push(cur);
        }
        self.site_cache
            .borrow_mut()
            .insert(uid.to_string(), sites.clone());
        sites
    }

    /// `fragment_records(uid)`.
    pub fn fragment_records(&self, uid: &str) -> Vec<Value> {
        {
            let cache = self.rec_cache.borrow();
            if let Some(v) = cache.get(uid) {
                return v.clone();
            }
        }
        let mut recs: Vec<Value> = Vec::new();
        for s in self.site_records(uid) {
            recs.extend(s);
        }
        self.rec_cache
            .borrow_mut()
            .insert(uid.to_string(), recs.clone());
        recs
    }

    /// `fragment(uid)`.
    pub fn fragment(&self, uid: &str) -> String {
        {
            let cache = self.frag_cache.borrow();
            if let Some(v) = cache.get(uid) {
                return v.clone();
            }
        }
        let text = text_of(&self.fragment_records(uid));
        self.frag_cache
            .borrow_mut()
            .insert(uid.to_string(), text.clone());
        text
    }

    /// `_split_records(recs)`.
    fn split_records(recs: &[Value]) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
        let mut i = 0usize;
        while i < recs.len()
            && recs[i].get("wrap").and_then(|v| v.as_str()) == Some("open")
        {
            i += 1;
        }
        let mut j = recs.len();
        while j > i && recs[j - 1].get("wrap").and_then(|v| v.as_str()) == Some("close") {
            j -= 1;
        }
        (
            recs[..i].to_vec(),
            recs[i..j].to_vec(),
            recs[j..].to_vec(),
        )
    }

    /// `_groups(units)`.
    ///
    /// v2's merged document emits a type header once and then one part per
    /// declaration site, so `_split_records` sees a *header site*
    /// (`opens = [namespace, class X]`, `closes = []`) followed by *body-only
    /// sites* (`opens = []`). Two different types can produce body-only sites
    /// with the same `(opens = [], closes = [}...])` key, and keying those
    /// together glued one type's members under another type's header. A
    /// body-only site is meaningless without the header site it was merged
    /// with, so such sites are scoped to their own unit; header sites keep the
    /// v1 cross-unit scaffold sharing.
    fn groups(&self, units: &[String]) -> Vec<Group> {
        let mut order: Vec<Group> = Vec::new();
        let mut index: HashMap<
            (
                Vec<(Option<String>, String)>,
                Vec<(Option<String>, String)>,
                Option<String>,
            ),
            usize,
        > = HashMap::new();
        for uid in units {
            for site in self.site_records(uid) {
                if site.is_empty() {
                    continue;
                }
                let (opens, body, closes) = Self::split_records(&site);
                let owner = if opens.is_empty() {
                    Some(uid.clone())
                } else {
                    None
                };
                let key = (ctx_text_key(&opens), ctx_text_key(&closes), owner);
                let ix = match index.get(&key) {
                    Some(&ix) => ix,
                    None => {
                        let ix = order.len();
                        order.push(Group {
                            opens: Vec::new(),
                            closes: Vec::new(),
                            bodies: Vec::new(),
                        });
                        index.insert(key, ix);
                        ix
                    }
                };
                order[ix].opens = opens;
                order[ix].closes = closes;
                order[ix].bodies.push((uid.clone(), body));
            }
        }
        order
    }

    /// `_banner(plan, f)`.
    fn banner(&self, plan: &Value, _f: &Value) -> String {
        format!(
            "// <auto-generated by csviewer reorg>\n\
             // mode={} group={} units={} files={}\n\
             // source: the graph under this viewer; edits belong in the source files\n",
            plan.get("mode").and_then(|v| v.as_str()).unwrap_or(""),
            plan.get("group").and_then(|v| v.as_str()).unwrap_or(""),
            plan.get("unitCount").and_then(|v| v.as_i64()).unwrap_or(0),
            plan.get("fileCount").and_then(|v| v.as_i64()).unwrap_or(0),
        )
    }

    /// `emit(plan, header=True)` — `{path: text}` in plan order.
    pub fn emit(&self, plan: &Value, header: bool) -> Vec<(String, String)> {
        let empty: Vec<Value> = Vec::new();
        let files = plan
            .get("files")
            .and_then(|v| v.as_array())
            .unwrap_or(&empty);
        let mut laid: Vec<(&Value, Vec<Group>)> = Vec::new();
        for f in files {
            let units: Vec<String> = f
                .get("units")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let groups = self.groups(&units);
            if !groups.is_empty() {
                laid.push((f, groups));
            }
        }

        // A type opened by more than one emitted group must say `partial`.
        let mut opens: HashMap<String, usize> = HashMap::new();
        for (_f, groups) in &laid {
            for g in groups {
                let mut tids: HashSet<String> = HashSet::new();
                for r in &g.opens {
                    if let Some(full) = r.get("full").and_then(|v| v.as_str()) {
                        if !full.is_empty() {
                            tids.insert(full.to_string());
                        }
                    }
                }
                for t in tids {
                    *opens.entry(t).or_insert(0) += 1;
                }
            }
        }
        let reopened: HashSet<String> = opens.keys().cloned().collect();
        let must_ids: HashSet<String> = opens
            .iter()
            .filter(|&(_, &n)| n > 1)
            .map(|(t, _)| t.clone())
            .collect();
        let must_partial = self.partial_names(&must_ids);

        let mut out: Vec<(String, String)> = Vec::new();
        let mut attrs_done: HashSet<String> = HashSet::new();
        for (f, groups) in &laid {
            let mut chunks: Vec<String> = Vec::new();
            for g in groups {
                let head = self.prune_attrs(&g.opens, &reopened, &mut attrs_done);
                let body = g
                    .bodies
                    .iter()
                    .map(|(_uid, b)| text_of(b))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let closed = text_of(&g.closes);
                let head_text = text_of(&head);
                let parts: Vec<String> = [head_text, body, closed]
                    .into_iter()
                    .filter(|x| !x.is_empty())
                    .collect();
                chunks.push(parts.join("\n"));
            }
            let mut blob = chunks
                .iter()
                .map(|c| c.trim_end_matches('\n'))
                .collect::<Vec<_>>()
                .join("\n\n")
                + "\n";
            if !must_partial.is_empty() {
                blob = self.patch_partial(&blob, &must_partial);
            }
            if header {
                blob = format!("{}{}", self.banner(plan, f), blob);
            }
            let path = f
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            out.push((path, blob));
        }
        out
    }

    /// `_prune_attrs(opens, reopened, seen)`.
    fn prune_attrs(
        &self,
        opens: &[Value],
        reopened: &HashSet<String>,
        seen: &mut HashSet<String>,
    ) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::new();
        let n = opens.len();
        let mut i = 0usize;
        while i < n {
            let rec = &opens[i];
            let full = rec.get("full").and_then(|v| v.as_str()).unwrap_or("");
            let text = rec.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let ctx = rec.get("ctx").and_then(|v| v.as_str()).unwrap_or("");
            if !full.is_empty()
                && reopened.contains(full)
                && ctx == "head"
                && lstrip_starts_bracket(text)
            {
                if seen.contains(full) {
                    while i < n {
                        let t = opens[i].get("text").and_then(|v| v.as_str()).unwrap_or("");
                        let tcs = to_chars(t);
                        if let Some(m) = head_any_re().find(&tcs) {
                            let mut trimmed =
                                opens[i].as_object().cloned().unwrap_or_default();
                            let sliced = chars_to_string(&tcs[m.start.min(tcs.len())..]);
                            trimmed.insert("text".to_string(), json!(sliced));
                            out.push(Value::Object(trimmed));
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
                seen.insert(full.to_string());
            }
            out.push(rec.clone());
            i += 1;
        }
        out
    }

    /// `_partial_names(type_ids)`.
    fn partial_names(&self, type_ids: &HashSet<String>) -> HashSet<String> {
        let mut out = HashSet::new();
        for t in type_ids {
            if let Some(name) = self.type_names.get(t) {
                if !name.is_empty() {
                    out.insert(name.clone());
                }
            }
        }
        out
    }

    /// `fragment_lines(uid)`.
    pub fn fragment_lines(&self, uid: &str) -> i64 {
        self.fragment(uid).matches('\n').count() as i64 + 1
    }

    fn unit_label(&self, uid: &str) -> String {
        self.nodes
            .get(uid)
            .map(|n| s_or_empty(n, "full"))
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.nodes
                    .get(uid)
                    .map(|n| s_or_empty(n, "id"))
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| uid.to_string())
    }

    fn unit_ns(&self, uid: &str) -> String {
        self.nodes
            .get(uid)
            .map(|n| n.get("ns").and_then(|v| v.as_str()).unwrap_or("").to_string())
            .unwrap_or_default()
    }

    // -- grouping -----------------------------------------------------------
    /// `group(units, spec)`.
    pub fn group(
        &self,
        units: &[String],
        spec: &Value,
    ) -> Result<Vec<(String, Vec<String>)>, String> {
        let mode = spec
            .get("group")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("single");
        match mode {
            "single" => {
                let name = spec
                    .get("name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| self.corpus_name(Some(units)));
                Ok(vec![(slug(&name, 110), units.to_vec())])
            }
            "namespace" => {
                let mut buckets: Vec<(String, Vec<String>)> = Vec::new();
                let mut index: HashMap<String, usize> = HashMap::new();
                for uid in units {
                    let ns = {
                        let v = self.unit_ns(uid);
                        if v.is_empty() { "_global".to_string() } else { v }
                    };
                    let ix = match index.get(&ns) {
                        Some(&ix) => ix,
                        None => {
                            let ix = buckets.len();
                            buckets.push((slug(&ns, 110), Vec::new()));
                            index.insert(ns.clone(), ix);
                            ix
                        }
                    };
                    buckets[ix].1.push(uid.clone());
                }
                Ok(buckets)
            }
            "owner" => {
                let mut buckets: Vec<(String, Vec<String>)> = Vec::new();
                let mut index: HashMap<String, usize> = HashMap::new();
                for uid in units {
                    let owner = self.top_owner(uid);
                    let label = if owner != *uid {
                        self.unit_label(&owner)
                    } else {
                        "_top".to_string()
                    };
                    let ix = match index.get(&label) {
                        Some(&ix) => ix,
                        None => {
                            let ix = buckets.len();
                            buckets.push((slug(&label, 110), Vec::new()));
                            index.insert(label.clone(), ix);
                            ix
                        }
                    };
                    buckets[ix].1.push(uid.clone());
                }
                Ok(buckets)
            }
            "size" => {
                let budget = int_or(spec.get("size"), 4000)
                    .ok_or_else(|| "invalid size".to_string())?
                    .max(1);
                let mut out: Vec<Vec<String>> = Vec::new();
                let mut cur: Vec<String> = Vec::new();
                let mut used: i64 = 0;
                for uid in units {
                    let n = self.fragment_lines(uid);
                    if !cur.is_empty() && used + n > budget {
                        out.push(std::mem::take(&mut cur));
                        used = 0;
                    }
                    cur.push(uid.clone());
                    used += n;
                }
                if !cur.is_empty() {
                    out.push(cur);
                }
                let width = std::cmp::max(2, out.len().to_string().len());
                Ok(out
                    .into_iter()
                    .enumerate()
                    .map(|(i, b)| (format!("part_{:0width$}", i + 1, width = width), b))
                    .collect())
            }
            "explicit" => {
                let empty: Vec<Value> = Vec::new();
                let groups = spec
                    .get("groups")
                    .and_then(|v| v.as_array())
                    .unwrap_or(&empty);
                let mut out: Vec<(String, Vec<String>)> = Vec::new();
                let mut assigned: HashSet<String> = HashSet::new();
                for (i, g) in groups.iter().enumerate() {
                    let mut ids: Vec<String> = Vec::new();
                    if let Some(items) = g.as_array() {
                        for uid in items {
                            let uid = uid.as_str().unwrap_or("");
                            if self.nodes.contains_key(uid) && !assigned.contains(uid) {
                                ids.push(uid.to_string());
                                assigned.insert(uid.to_string());
                            }
                        }
                    }
                    if !ids.is_empty() {
                        out.push((format!("group_{:02}", i + 1), ids));
                    }
                }
                let rest: Vec<String> = units
                    .iter()
                    .filter(|u| !assigned.contains(*u))
                    .cloned()
                    .collect();
                if !rest.is_empty() {
                    out.push(("rest".to_string(), rest));
                }
                Ok(out)
            }
            other => Err(format!("unknown group mode: {other:?}")),
        }
    }

    /// `pin_data_members(units, buckets)`.
    pub fn pin_data_members(
        &self,
        units: &[String],
        mut buckets: Vec<(String, Vec<String>)>,
    ) -> Vec<(String, Vec<String>)> {
        let mut by_owner: Vec<(String, Vec<String>)> = Vec::new();
        for uid in units {
            let Some(n) = self.nodes.get(uid) else {
                continue;
            };
            let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if is_data_kind(kind) {
                let owner = n
                    .get("owner")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                match by_owner.iter_mut().find(|(o, _)| *o == owner) {
                    Some(e) => e.1.push(uid.clone()),
                    None => by_owner.push((owner, vec![uid.clone()])),
                }
            }
        }
        for (_owner, ids) in &by_owner {
            if ids.len() < 2 {
                continue;
            }
            let mut home: Option<usize> = None;
            for (bi, (_name, bucket)) in buckets.iter().enumerate() {
                if ids.iter().any(|i| bucket.contains(i)) {
                    home = Some(bi);
                    break;
                }
            }
            let Some(home) = home else { continue };
            for (bi, (_name, bucket)) in buckets.iter_mut().enumerate() {
                if bi == home {
                    continue;
                }
                bucket.retain(|u| !ids.contains(u));
            }
            let home_bucket = &mut buckets[home].1;
            for i in ids {
                if !home_bucket.contains(i) {
                    home_bucket.push(i.clone());
                }
            }
        }
        let order: HashMap<&String, usize> =
            units.iter().enumerate().map(|(k, u)| (u, k)).collect();
        for (_name, bucket) in buckets.iter_mut() {
            bucket.sort_by_key(|u| *order.get(u).unwrap_or(&0));
        }
        buckets
    }

    /// `_corpus_name(units)`.
    fn corpus_name(&self, units: Option<&[String]>) -> String {
        let sel: Vec<String> = match units {
            Some(u) if !u.is_empty() => u.to_vec(),
            _ => self.select(None, "top"),
        };
        let mut names: HashSet<String> = HashSet::new();
        for u in &sel {
            let ns = self.unit_ns(u);
            if !ns.is_empty() {
                names.insert(ns);
            }
        }
        let mut names: Vec<String> = names.into_iter().collect();
        names.sort();
        if names.len() == 1 {
            names[0].clone()
        } else {
            "merged".to_string()
        }
    }

    // -- plan / emit --------------------------------------------------------
    /// `plan(spec=None)`.
    pub fn plan(&self, spec: Option<&Value>) -> Result<Value, String> {
        let spec_obj: Map<String, Value> = match spec {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        let spec_v = Value::Object(spec_obj.clone());

        let ids = truthy_array(&spec_obj, "units")
            .or_else(|| truthy_array(&spec_obj, "ids"))
            .or_else(|| truthy_array(&spec_obj, "peel"));
        let granularity = spec_obj
            .get("granularity")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("top");
        let mut units = self.select(ids, granularity);
        if spec_obj.get("include_only").map(truthy).unwrap_or(false) {
            if let Some(arr) = spec_obj.get("include_only").and_then(|v| v.as_array()) {
                let set: HashSet<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect();
                units.retain(|u| set.contains(u));
            }
        }

        let buckets = self.group(&units, &spec_v)?;
        let buckets = self.pin_data_members(&units, buckets);

        let mut files: Vec<Value> = Vec::new();
        let mut seen: HashMap<String, i64> = HashMap::new();
        for (stem, bucket) in buckets {
            if bucket.is_empty() {
                continue;
            }
            let base;
            if let Some(c) = seen.get_mut(&stem) {
                *c += 1;
                base = format!("{}_{}", stem, *c);
            } else {
                seen.insert(stem.clone(), 0);
                base = stem.clone();
            }
            let lines: i64 = bucket.iter().map(|u| self.fragment_lines(u)).sum();
            files.push(json!({
                "path": format!("{}.cs", base),
                "units": bucket,
                "lines": lines,
            }));
        }
        let total: i64 = files
            .iter()
            .map(|f| f.get("lines").and_then(|v| v.as_i64()).unwrap_or(0))
            .sum();
        let defines_val = match &self.defines {
            Some(d) if !d.is_empty() => {
                let mut v: Vec<String> = d.iter().cloned().collect();
                v.sort();
                json!(v)
            }
            _ => json!([]),
        };
        Ok(json!({
            "mode": "split",
            "group": spec_obj.get("group").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or("single"),
            "unitCount": units.len(),
            "fileCount": files.len(),
            "lines": total,
            "maxLines": self.max_lines,
            "definesKnown": self.defines_known,
            "defines": defines_val,
            "files": files,
            "spec": spec_v,
        }))
    }

    /// `_patch_partial(text, type_names)`.
    fn patch_partial(&self, text: &str, type_names: &HashSet<String>) -> String {
        if type_names.is_empty() {
            return text.to_string();
        }
        let mut out: Vec<String> = Vec::new();
        for line in text.split('\n') {
            let mut line = line.to_string();
            let cs = to_chars(&line);
            if let Some(m) = head_re().match_at(&cs, 0) {
                let kw = m.group(3).unwrap_or("");
                if ["class", "struct", "interface", "record"].contains(&kw) {
                    let rest_slice: Vec<char> = cs[m.end.min(cs.len())..].to_vec();
                    let rest = lstrip_ws(&rest_slice);
                    if let Some(nm) = name_re().match_at(rest, 0) {
                        if type_names.contains(nm.group(0).unwrap_or("")) {
                            line = ensure_partial(&line);
                        }
                    }
                }
            }
            out.push(line);
        }
        out.join("\n")
    }

    /// `merge_text(spec=None)`.
    pub fn merge_text(&self, spec: Option<&Value>) -> Result<String, String> {
        let mut merged = match spec {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        merged.insert("group".to_string(), json!("single"));
        let plan = self.plan(Some(&Value::Object(merged)))?;
        let header = spec
            .and_then(|s| s.get("header"))
            .map(truthy)
            .unwrap_or(true);
        let files = self.emit(&plan, header);
        Ok(files.first().map(|(_, t)| t.clone()).unwrap_or_default())
    }

    // -- verification -------------------------------------------------------
    /// `expected_ids(units)`.
    pub fn expected_ids(&self, units: &[String]) -> HashSet<String> {
        let mut sel: HashSet<String> = units.iter().cloned().collect();
        let mut by_span: HashMap<(String, i64, i64), HashSet<String>> = HashMap::new();
        for uid in self.nodes.keys() {
            if let Some(key) = self.span_key(uid) {
                by_span.entry(key).or_default().insert(uid.clone());
            }
        }
        for uid in units {
            if let Some(key) = self.span_key(uid) {
                if let Some(sib) = by_span.get(&key) {
                    sel.extend(sib.iter().cloned());
                }
            }
        }
        let mut out: HashSet<String> = HashSet::new();
        for (uid, n) in &self.nodes {
            let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if is_non_unit_kind(kind) {
                continue;
            }
            if sel.contains(uid) || self.owner_chain(uid).iter().any(|a| sel.contains(a)) {
                out.insert(uid.clone());
            }
        }
        for uid in &sel {
            for a in self.owner_chain(uid) {
                let kind = self.kind_of(&a);
                if is_type_kind(&kind) {
                    out.insert(a);
                }
            }
        }
        out
    }

    /// `verify(emitted, original_texts=None, plan=None)`.
    pub fn verify(
        &self,
        emitted: &[(String, String)],
        original_texts: Option<&HashMap<String, String>>,
        plan: Option<&Value>,
    ) -> Value {
        let original = original_texts.unwrap_or(&self.file_texts);
        let t0 = std::time::Instant::now();
        let before = Self::graph(original);
        self.verify_with_before(&before, emitted, plan, t0.elapsed())
    }

    /// `verify` with the `before` graph already built by the caller.
    ///
    /// The `before` pass is a full re-parse of the corpus and is identical for
    /// every spec because `file_texts` is fixed, so a caller that runs several
    /// specs parses once and hands the graph in here. It must be the graph of
    /// the same text `verify` would have parsed — `Reorg::graph(&file_texts)`,
    /// which is exactly what `corpus_graph` builds while it is indexing — and
    /// `d_before` is how long that caller spent building it, kept for `--time`.
    pub fn verify_with_before(
        &self,
        before: &crate::graph::GraphBuilder,
        emitted: &[(String, String)],
        plan: Option<&Value>,
        d_before: std::time::Duration,
    ) -> Value {
        let tprobe = std::env::var_os("REORG_TIME").is_some();
        let emitted_map: HashMap<String, String> = emitted.iter().cloned().collect();
        let t1 = std::time::Instant::now();
        let after = Self::graph(&emitted_map);
        let d_after = t1.elapsed();
        if tprobe {
            eprintln!("[verify] graph before {:?}, after {:?}", d_before, d_after);
        }

        let before_all = decl_map(&before);
        let after_decl = decl_map(&after);
        let units: Option<Vec<String>> = plan.map(|p| {
            p.get("files")
                .and_then(|v| v.as_array())
                .map(|files| {
                    files
                        .iter()
                        .flat_map(|f| {
                            f.get("units")
                                .and_then(|v| v.as_array())
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(str::to_string))
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default()
                        })
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default()
        });

        let scope;
        let before_decl: HashMap<String, String>;
        if let Some(units) = &units {
            scope = "subset";
            let expected = self.expected_ids(units);
            before_decl = before_all
                .iter()
                .filter(|(i, _)| expected.contains(*i))
                .map(|(i, v)| (i.clone(), v.clone()))
                .collect();
        } else {
            scope = "corpus";
            before_decl = before_all.clone();
        }

        let mut missing: Vec<String> = before_decl
            .keys()
            .filter(|i| !after_decl.contains_key(*i))
            .cloned()
            .collect();
        missing.sort();
        let mut added: Vec<String> = after_decl
            .keys()
            .filter(|i| !before_decl.contains_key(*i))
            .cloned()
            .collect();
        added.sort();
        let mut changed: Vec<String> = before_decl
            .iter()
            .filter(|(i, v)| after_decl.get(*i).map(|x| x != *v).unwrap_or(false))
            .map(|(i, _)| i.clone())
            .collect();
        changed.sort();

        let rel_missing: Vec<(String, String, String)>;
        let rel_added: Vec<(String, String, String)>;
        let before_rel;
        let after_rel;
        if scope == "subset" {
            rel_missing = Vec::new();
            rel_added = Vec::new();
            before_rel = HashSet::new();
            after_rel = HashSet::new();
        } else {
            before_rel = rel_set(&before);
            after_rel = rel_set(&after);
            let mut m: Vec<(String, String, String)> =
                before_rel.difference(&after_rel).cloned().collect();
            m.sort();
            rel_missing = m;
            let mut a: Vec<(String, String, String)> =
                after_rel.difference(&before_rel).cloned().collect();
            a.sort();
            rel_added = a;
        }

        let mut ordered: Vec<&(String, String)> = emitted.iter().collect();
        ordered.sort_by(|a, b| a.0.cmp(&b.0));
        let mut balance_obj = Map::new();
        let mut unbalanced: Vec<String> = Vec::new();
        for (p, t) in &ordered {
            let b = balance(t);
            if b != Value::Bool(false) {
                unbalanced.push((*p).clone());
            }
            balance_obj.insert((*p).clone(), b);
        }
        unbalanced.sort();

        let keep: HashSet<String> = before_decl.keys().cloned().collect();
        let t2 = std::time::Instant::now();
        let text = self.audit_text(emitted, Some(&keep));
        if tprobe {
            eprintln!("[verify] audit_text {:?}", t2.elapsed());
        }

        let text_ok = text.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
        let ok = missing.is_empty()
            && added.is_empty()
            && changed.is_empty()
            && unbalanced.is_empty()
            && text_ok;
        let equal = ok && rel_missing.is_empty() && rel_added.is_empty();

        json!({
            "ok": ok,
            "equal": equal,
            "scope": scope,
            "before": {"declarations": before_decl.len(), "relations": before_rel.len()},
            "after": {"declarations": after_decl.len(), "relations": after_rel.len()},
            "missing": missing.iter().take(200).cloned().collect::<Vec<_>>(),
            "added": added.iter().take(200).cloned().collect::<Vec<_>>(),
            "changed": changed.iter().take(200).cloned().collect::<Vec<_>>(),
            "relationsMissing": rel_missing.iter().take(200).map(|(a,b,c)| json!([a,b,c])).collect::<Vec<_>>(),
            "relationsAdded": rel_added.iter().take(200).map(|(a,b,c)| json!([a,b,c])).collect::<Vec<_>>(),
            "unbalanced": unbalanced.iter().take(50).cloned().collect::<Vec<_>>(),
            "balance": Value::Object(balance_obj),
            "text": text,
            "files": emitted.len(),
            "ms": 0,
        })
    }

    /// `audit_text(emitted, keep=None)`.
    pub fn audit_text(
        &self,
        emitted: &[(String, String)],
        keep: Option<&HashSet<String>>,
    ) -> Value {
        let mut ordered: Vec<&(String, String)> = emitted.iter().collect();
        ordered.sort_by(|a, b| a.0.cmp(&b.0));
        let joined = ordered
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let blob = norm_text(&joined);

        let mut missing: Vec<String> = Vec::new();
        let mut checked: i64 = 0;
        // `to_chars` allocates a 4-byte-per-char copy of a whole file. Doing that
        // for every member node made this audit quadratic (files x nodes); the
        // file set is fixed for the call, so convert each file once.
        let mut file_chars: HashMap<String, Vec<char>> = HashMap::new();
        let mut ids: Vec<&String> = self.nodes.keys().collect();
        ids.sort();
        for uid in ids {
            if let Some(keep) = keep {
                if !keep.contains(uid) {
                    continue;
                }
            }
            let n = &self.nodes[uid];
            let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if !is_member_kind(kind) {
                continue;
            }
            let Some((f, so, eo)) = self.span_key(uid) else {
                continue;
            };
            let Some(src) = self.file_texts.get(&f) else {
                continue;
            };
            let chars = file_chars
                .entry(f.clone())
                .or_insert_with(|| to_chars(src));
            let piece = norm_text(&chars_to_string(&py_slice(chars, so, eo)));
            if piece.is_empty() {
                continue;
            }
            checked += 1;
            if !blob.contains(&piece) {
                missing.push(uid.clone());
            }
        }
        json!({
            "ok": missing.is_empty(),
            "checked": checked,
            "missing": missing.iter().take(100).cloned().collect::<Vec<_>>(),
        })
    }

    /// The `before` graph `verify` builds for `texts`, exposed so a caller that
    /// verifies several specs can parse the corpus once and pass it to
    /// [`Reorg::verify_with_before`].
    pub fn graph_of(texts: &HashMap<String, String>) -> crate::graph::GraphBuilder {
        Self::graph(texts)
    }

    /// `_graph(texts)`.
    fn graph(texts: &HashMap<String, String>) -> crate::graph::GraphBuilder {
        let mut keys: Vec<&String> = texts.keys().collect();
        keys.sort();
        let mut parses: Vec<Value> = Vec::new();
        for path in keys {
            let text = &texts[path];
            if text.is_empty() {
                continue;
            }
            parses.push(Self::parse_text(path, text));
        }
        build_graph(&parses)
    }

    /// `_parse_text(path, text, defines=None)`.
    fn parse_text(path: &str, text: &str) -> Value {
        let src = Source::new(
            path.to_string(),
            path.to_string(),
            to_chars(text),
            None,
        );
        let mut p = Parser::new(&src);
        p.parse();
        let members = finalize_members(&p, &src);
        let inherits: Vec<Value> = p
            .inherits
            .iter()
            .map(|(a, b)| json!([a, b]))
            .collect();
        json!({
            "file": path,
            "types": p.types,
            "members": members,
            "inherits": inherits,
            "warnings": p.warnings,
        })
    }

    // -- export -------------------------------------------------------------
    /// `zip_bytes(emitted, root='')`.
    pub fn zip_bytes(&self, emitted: &[(String, String)], root: &str) -> Vec<u8> {
        let prefix = root.trim_matches('/');
        let mut sorted: Vec<&(String, String)> = emitted.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out: Vec<u8> = Vec::new();
        let mut central: Vec<u8> = Vec::new();
        let mut entries: u16 = 0;
        for (path, text) in sorted {
            let arc = if prefix.is_empty() {
                path.clone()
            } else {
                format!("{prefix}/{path}")
            };
            let data = text.as_bytes();
            let crc = {
                let mut c = flate2::Crc::new();
                c.update(data);
                c.sum()
            };
            let comp = deflate(data);
            let offset = out.len() as u32;
            let nb = arc.as_bytes();
            // local file header
            out.extend_from_slice(&0x04034b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0x0800u16.to_le_bytes());
            out.extend_from_slice(&8u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(nb);
            out.extend_from_slice(&comp);
            // central directory header
            central.extend_from_slice(&0x02014b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0x0800u16.to_le_bytes());
            central.extend_from_slice(&8u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&(comp.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(nb);
            entries += 1;
        }
        let cd_offset = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        // end of central directory
        out.extend_from_slice(&0x06054b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&entries.to_le_bytes());
        out.extend_from_slice(&entries.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// `write_tree(emitted, dest, overwrite=True)`.
    pub fn write_tree(
        &self,
        emitted: &[(String, String)],
        dest: &str,
        overwrite: bool,
    ) -> Value {
        if dest.is_empty() || !dest.starts_with('/') {
            return json!({"ok": false, "error": "destination must be an absolute path"});
        }
        let dest = normpath(dest);
        let home = std::env::var("HOME").unwrap_or_default();
        if dest == "/" || dest == home {
            return json!({
                "ok": false,
                "error": format!("refusing to write the whole tree into '{dest}'"),
            });
        }
        if std::fs::create_dir_all(&dest).is_err() {
            return json!({"ok": false, "error": format!("could not create {dest}")});
        }
        let mut written: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut sorted: Vec<&(String, String)> = emitted.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let dest_sep = format!("{dest}/");
        for (path, text) in sorted {
            let target = format!("{dest}/{path}");
            let real = normpath(&target);
            if !real.starts_with(&dest_sep) {
                errors.push(format!("{path}: escapes destination"));
                continue;
            }
            let exists = Path::new(&real).exists();
            if exists && !overwrite {
                skipped.push(path.clone());
                continue;
            }
            let tmp = format!("{real}.csviewer-tmp");
            let parent = Path::new(&real)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
            let result = (|| -> std::io::Result<()> {
                std::fs::create_dir_all(&parent)?;
                let mut fh = std::fs::File::create(&tmp)?;
                fh.write_all(text.as_bytes())?;
                drop(fh);
                std::fs::rename(&tmp, &real)
            })();
            match result {
                Ok(()) => written.push(path.clone()),
                Err(exc) => {
                    errors.push(format!("{path}: {exc}"));
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        }
        json!({
            "ok": errors.is_empty(),
            "dest": dest,
            "written": written,
            "skipped": skipped,
            "errors": errors,
        })
    }
}

// ---------------------------------------------------------------------------
// graph diffing helpers
// ---------------------------------------------------------------------------
fn decl_map(g: &crate::graph::GraphBuilder) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for n in &g.nodes {
        let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if is_non_unit_kind(kind) {
            continue;
        }
        let id = n.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let sig = n
            .get("signature")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        out.insert(id, format!("{kind}|{sig}"));
    }
    out
}

fn rel_set(g: &crate::graph::GraphBuilder) -> HashSet<(String, String, String)> {
    let mut out = HashSet::new();
    for e in &g.edges {
        let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if kind == "contains" {
            continue;
        }
        out.insert((
            e.get("source").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            kind.to_string(),
            e.get("target").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        ));
    }
    out
}

/// `_norm(text)`.
fn norm_text(text: &str) -> String {
    let chars = to_chars(text);
    let mut out = String::new();
    let mut in_ws = false;
    for c in chars {
        if py_is_space(c) {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out.trim_matches(' ').to_string()
}

/// `_balance(text)` — `false` when balanced, else a short reason string.
fn balance(text: &str) -> Value {
    let cs = to_chars(text);
    let clean = mask_noncode(&cs);
    for (open, close, label) in [('{', '}', "brace"), ('(', ')', "paren"), ('[', ']', "bracket")]
    {
        let mut depth: i64 = 0;
        for &c in &clean {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth < 0 {
                    return Value::String(format!("unbalanced {label}"));
                }
            }
        }
        if depth != 0 {
            return Value::String(format!("unbalanced {label}"));
        }
    }
    let mut depth: i64 = 0;
    for line in text.split('\n') {
        let s = line.trim();
        if !s.starts_with('#') {
            continue;
        }
        let scs = to_chars(s);
        let Some(m) = directive_re().match_at(&scs, 0) else {
            continue;
        };
        let word = m.group(1).unwrap_or("");
        if word == "if" {
            depth += 1;
        } else if word == "endif" {
            depth -= 1;
            if depth < 0 {
                return Value::String("unbalanced #endif".to_string());
            }
        }
    }
    if depth != 0 {
        return Value::String("unbalanced #if".to_string());
    }
    Value::Bool(false)
}

fn text_of(recs: &[Value]) -> String {
    recs.iter()
        .map(|r| r.get("text").and_then(|v| v.as_str()).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

fn ctx_text_key(recs: &[Value]) -> Vec<(Option<String>, String)> {
    recs.iter()
        .map(|r| {
            (
                r.get("ctx").and_then(|v| v.as_str()).map(str::to_string),
                r.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            )
        })
        .collect()
}

fn truthy_array(spec: &Map<String, Value>, key: &str) -> Option<Vec<String>> {
    let arr = spec.get(key)?.as_array()?;
    if arr.is_empty() {
        return None;
    }
    Some(
        arr.iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
    )
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = enc.write_all(data);
    enc.finish().unwrap_or_default()
}

/// A minimal `os.path.normpath` good enough for the export guard.
fn normpath(p: &str) -> String {
    let abs = p.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if let Some(last) = parts.last() {
                    if *last != ".." {
                        parts.pop();
                        continue;
                    }
                }
                if !abs {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    let mut out = if abs { String::from("/") } else { String::new() };
    out.push_str(&parts.join("/"));
    if out.is_empty() {
        ".".to_string()
    } else {
        out
    }
}

// ---------------------------------------------------------------------------
// convenience for a server / CLI caller
// ---------------------------------------------------------------------------
/// `build(nodes, file_texts, spec=None, defines=None, defines_known=False)`.
pub fn build(
    nodes: &Map<String, Value>,
    file_texts: &HashMap<String, String>,
    spec: Option<&Value>,
    defines: Option<&HashSet<String>>,
    defines_known: bool,
) -> Result<(Value, Vec<(String, String)>, Value), String> {
    let engine = Reorg::new(nodes, file_texts, defines, defines_known, None);
    let plan = engine.plan(spec)?;
    let header = spec
        .and_then(|s| s.get("header"))
        .map(truthy)
        .unwrap_or(true);
    let files = engine.emit(&plan, header);
    let report = engine.verify(&files, None, Some(&plan));
    Ok((plan, files, report))
}

/// `_zip_name(spec)` (server.py).
pub fn zip_name(spec: &Value) -> String {
    let mode = spec
        .get("group")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("single");
    let base = spec
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if mode == "single" {
                "merged".to_string()
            } else {
                "reorg".to_string()
            }
        });
    // `re.sub(r'[^A-Za-z0-9._-]+', '_', str(base)).strip('_') or 'reorg'` —
    // unlike `_slug`, an empty result falls back to `reorg`, not `node`.
    let mut scrubbed = String::new();
    let mut in_run = false;
    for c in base.chars() {
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
            scrubbed.push(c);
            in_run = false;
        } else if !in_run {
            scrubbed.push('_');
            in_run = true;
        }
    }
    let safe = scrubbed.trim_matches('_');
    let safe = if safe.is_empty() { "reorg" } else { safe };
    format!("{safe}.zip")
}

/// `_previews(files, lines=40)` (server.py).
pub fn previews(emitted: &[(String, String)], lines: usize) -> Value {
    let mut sorted: Vec<&(String, String)> = emitted.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out: Vec<Value> = Vec::new();
    for (path, text) in sorted {
        let body: Vec<&str> = text.split('\n').collect();
        let head = body.iter().take(lines).cloned().collect::<Vec<_>>().join("\n");
        out.push(json!({
            "path": path,
            "lines": body.len(),
            "bytes": text.chars().count(),
            "head": head,
        }));
    }
    Value::Array(out)
}

// ---------------------------------------------------------------------------
// `csgraph dump-reorg`
// ---------------------------------------------------------------------------
fn arg_flag(args: &[String], name: &str) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            return it.next().cloned();
        }
    }
    None
}

fn arg_all(args: &[String], name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            if let Some(v) = it.next() {
                out.push(v.clone());
            }
        }
    }
    out
}

/// Build `(nodes, file_texts)` from a corpus exactly the way `dump-doc` does.
pub fn corpus_graph(
    repo: &str,
    roots: &[String],
    excludes: &[String],
) -> (Map<String, Value>, HashMap<String, String>) {
    let (nodes, file_texts, _) = corpus_graph_with_builder(repo, roots, excludes);
    (nodes, file_texts)
}

/// As [`corpus_graph`], but also returns the [`crate::graph::GraphBuilder`] it
/// built along the way.
///
/// That builder is exactly `Reorg::graph(&file_texts)`, so a caller that needs
/// both the node map and the `verify` `before` graph pays for the corpus parse
/// once instead of once per spec.
pub fn corpus_graph_with_builder(
    repo: &str,
    roots: &[String],
    excludes: &[String],
) -> (
    Map<String, Value>,
    HashMap<String, String>,
    crate::graph::GraphBuilder,
) {
    let repo_path = Path::new(repo);
    let files = discover_files(repo_path, roots, excludes);
    let n = files.len();

    // Parsing a file is pure (`parse_source` only reads), and the graph build
    // below is order-independent once the parses are back in file order, so the
    // whole per-file pass is fanned out across the available cores. This is the
    // step every differential pays for, and it is what made `dump-reorg` spend
    // half a minute of CPU before it had even planned a spec.
    let workers = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1)
        .min(n.max(1))
        .max(1);

    let mut parsed: Vec<(usize, Value, String, String)> = Vec::with_capacity(n);
    if workers <= 1 {
        for (i, f) in files.iter().enumerate() {
            let p = Path::new(f);
            let disp = display_path(p, repo_path);
            let parse = parse_source(p, &disp, None);
            let text = read_python_text(p)
                .map(|cs| chars_to_string(&cs))
                .unwrap_or_default();
            parsed.push((i, parse, disp, text));
        }
    } else {
        let chunk = n.div_ceil(workers);
        std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for w in 0..workers {
                let start = w * chunk;
                if start >= n {
                    break;
                }
                let end = (start + chunk).min(n);
                let files = &files;
                handles.push(scope.spawn(move || {
                    let mut local: Vec<(usize, Value, String, String)> =
                        Vec::with_capacity(end - start);
                    for (i, f) in files.iter().enumerate().take(end).skip(start) {
                        let p = Path::new(f);
                        let disp = display_path(p, repo_path);
                        let parse = parse_source(p, &disp, None);
                        let text = read_python_text(p)
                            .map(|cs| chars_to_string(&cs))
                            .unwrap_or_default();
                        local.push((i, parse, disp, text));
                    }
                    local
                }));
            }
            for h in handles {
                if let Ok(local) = h.join() {
                    parsed.extend(local);
                }
            }
        });
        parsed.sort_by_key(|(i, _, _, _)| *i);
    }

    let mut parses: Vec<Value> = Vec::with_capacity(n);
    let mut file_texts: HashMap<String, String> = HashMap::new();
    for (_, parse, disp, text) in parsed {
        parses.push(parse);
        file_texts.insert(disp, text);
    }
    let gb = build_graph(&parses);
    let mut nodes: Map<String, Value> = Map::new();
    for n in &gb.nodes {
        if let Some(id) = n.get("id").and_then(|v| v.as_str()) {
            nodes.insert(id.to_string(), n.clone());
        }
    }
    (nodes, file_texts, gb)
}

/// `spec_key(spec)` — a stable label both the oracle and this CLI compute.
pub fn spec_key(spec: &Value) -> String {
    let Some(obj) = spec.as_object() else {
        return String::new();
    };
    let mut keys: Vec<&String> = obj.keys().collect();
    keys.sort();
    keys.iter()
        .map(|k| {
            let v = obj.get(*k).unwrap();
            let repr = match v {
                Value::String(s) => s.clone(),
                Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
                Value::Null => "None".to_string(),
                Value::Number(n) => n.to_string(),
                other => other.to_string(),
            };
            format!("{k}={repr}")
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// The specs `tools/oracle.py` uses for the reorg surface.
pub fn default_specs() -> Vec<Value> {
    vec![
        json!({"group": "single"}),
        json!({"group": "size", "size": 4000}),
        json!({"granularity": "leaf", "group": "size", "size": 3000}),
        json!({"group": "namespace"}),
        json!({"group": "owner"}),
    ]
}

/// `csgraph dump-reorg` — plan + emit + verify every default spec and print
/// `{specs: {key: {plan, report, emitted}}}` as canonical JSON.
///
/// The corpus graph is built once and its `GraphBuilder` becomes the `before`
/// pass of every `verify`; the specs then run as parallel units (see
/// [`Reorg::verify_with_before`]).
pub fn cli_dump_reorg(args: &[String], repo_override: Option<&str>) -> Result<(), String> {
    let repo = match repo_override {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => arg_flag(args, "--repo").unwrap_or_default(),
    };
    if repo.is_empty() {
        return Err("--repo is required".to_string());
    }
    let mut roots = arg_all(args, "--root");
    if roots.is_empty() {
        roots.push("_sealed_src".to_string());
    }
    let excludes = arg_all(args, "--exclude");
    // `--spec KEY` (repeatable) runs only those specs, so a caller can keep each
    // invocation inside a wall-clock budget; `--time` reports where the time
    // went on stderr.
    let wanted = arg_all(args, "--spec");
    let timing = args.iter().any(|a| a == "--time");

    let t_graph = std::time::Instant::now();
    let (nodes, file_texts, before_graph) = corpus_graph_with_builder(&repo, &roots, &excludes);
    let d_graph = t_graph.elapsed();
    if timing {
        eprintln!(
            "[reorg] graph: {} node(s), {} file(s), {:?}",
            nodes.len(),
            file_texts.len(),
            d_graph
        );
    }

    let selected: Vec<Value> = default_specs()
        .into_iter()
        .filter(|spec| {
            let key = spec_key(spec);
            wanted.is_empty() || wanted.iter().any(|w| w == &key)
        })
        .collect();

    struct SpecOut {
        key: String,
        entry: Map<String, Value>,
        d_plan: std::time::Duration,
        d_emit: std::time::Duration,
        n_files: usize,
        d_verify: std::time::Duration,
    }

    // The specs are independent and `before_graph` is identical for all of
    // them (it is the corpus graph the indexing pass just built), so each worker
    // gets its own engine and the specs run as parallel units: the wall time is
    // the heaviest spec, not their sum. Results are joined in `default_specs`
    // order, so the JSON stays byte-identical to a serial run.
    let results = std::thread::scope(|scope| -> Result<Vec<SpecOut>, String> {
        let nodes_ref: &Map<String, Value> = &nodes;
        let file_texts_ref: &HashMap<String, String> = &file_texts;
        let before_ref: &crate::graph::GraphBuilder = &before_graph;
        let mut handles = Vec::with_capacity(selected.len());
        for spec in &selected {
            let spec = spec;
            handles.push(scope.spawn(move || -> Result<SpecOut, String> {
                let engine = Reorg::new(nodes_ref, file_texts_ref, None, false, None);
                let t_plan = std::time::Instant::now();
                let plan = engine.plan(Some(spec))?;
                let d_plan = t_plan.elapsed();
                let t_emit = std::time::Instant::now();
                let files = engine.emit(&plan, true);
                let d_emit = t_emit.elapsed();
                let n_files = files.len();
                let t_verify = std::time::Instant::now();
                let mut report =
                    engine.verify_with_before(before_ref, &files, Some(&plan), d_graph);
                let d_verify = t_verify.elapsed();
                if let Some(o) = report.as_object_mut() {
                    o.remove("ms");
                }
                let key = spec_key(spec);
                let mut entry = Map::new();
                entry.insert("plan".to_string(), plan);
                entry.insert("report".to_string(), report);
                if key == "group=single" {
                    let mut sorted: Vec<&(String, String)> = files.iter().collect();
                    sorted.sort_by(|a, b| a.0.cmp(&b.0));
                    let list: Vec<Value> = sorted
                        .into_iter()
                        .map(|(path, text)| {
                            json!({
                                "path": path,
                                "sha1": sha1_hex(text.as_bytes()),
                                "lines": text.matches('\n').count() + 1,
                                "chars": text.chars().count(),
                                "text": text,
                            })
                        })
                        .collect();
                    entry.insert("emitted".to_string(), Value::Array(list));
                } else {
                    entry.insert("emitted".to_string(), Value::Null);
                }
                Ok(SpecOut {
                    key,
                    entry,
                    d_plan,
                    d_emit,
                    n_files,
                    d_verify,
                })
            }));
        }
        let mut out = Vec::with_capacity(handles.len());
        for h in handles {
            match h.join() {
                Ok(Ok(v)) => out.push(v),
                Ok(Err(e)) => return Err(e),
                Err(_) => return Err("dump-reorg: a spec worker panicked".to_string()),
            }
        }
        Ok(out)
    })?;

    let mut specs_out = Map::new();
    for r in results {
        if timing {
            eprintln!(
                "[reorg] {}: plan {:?}, emit {:?} ({} file(s)), verify {:?}",
                r.key, r.d_plan, r.d_emit, r.n_files, r.d_verify
            );
        }
        specs_out.insert(r.key, Value::Object(r.entry));
    }
    let doc = json!({"specs": Value::Object(specs_out)});
    let doc = crate::doc::rel_value(&doc, &repo);
    let stdout = serde_json::to_string(&doc).map_err(|e| e.to_string())?;
    println!("{stdout}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_python() {
        assert_eq!(slug("NZK.Core.MA", 110), "NZK.Core.MA");
        assert_eq!(slug("a b/c", 110), "a_b_c");
        assert_eq!(slug("___", 110), "node");
        assert_eq!(slug("", 110), "node");
    }

    #[test]
    fn ensure_partial_inserts_before_keyword() {
        assert_eq!(
            ensure_partial("public sealed class Foo {"),
            "public sealed partial class Foo {"
        );
        assert_eq!(
            ensure_partial("    class Bar"),
            "    partial class Bar"
        );
        assert_eq!(ensure_partial("public partial class X"), "public partial class X");
        assert_eq!(ensure_partial("enum E {"), "enum E {");
    }

    #[test]
    fn norm_collapses_whitespace() {
        assert_eq!(norm_text("  a \n b\t"), "a b");
        assert_eq!(norm_text("\u{1c}"), "");
    }

    #[test]
    fn balance_detects_imbalance() {
        assert_eq!(balance("class A { }"), Value::Bool(false));
        assert_ne!(balance("class A {"), Value::Bool(false));
        assert_ne!(balance("#if X\n#endif\n#endif"), Value::Bool(false));
    }

    #[test]
    fn spec_key_is_stable() {
        assert_eq!(spec_key(&json!({"group": "single"})), "group=single");
        assert_eq!(
            spec_key(&json!({"group": "size", "size": 4000})),
            "group=size|size=4000"
        );
        assert_eq!(
            spec_key(&json!({"granularity": "leaf", "group": "size", "size": 3000})),
            "granularity=leaf|group=size|size=3000"
        );
    }

    #[test]
    fn zip_name_falls_back_to_reorg() {
        assert_eq!(zip_name(&json!({"group": "single"})), "merged.zip");
        assert_eq!(zip_name(&json!({"group": "size"})), "reorg.zip");
        assert_eq!(zip_name(&json!({"group": "size", "name": "My Tree"})), "My_Tree.zip");
        assert_eq!(zip_name(&json!({"group": "size", "name": "!!!"})), "reorg.zip");
    }

    #[test]
    fn zip_bytes_container_is_well_formed() {
        let nodes = Map::new();
        let texts = HashMap::new();
        let engine = Reorg::new(&nodes, &texts, None, false, None);
        let emitted = vec![
            ("a.cs".to_string(), "class A {}\n".to_string()),
            ("sub/b.cs".to_string(), "class B {}\n".to_string()),
        ];
        let bytes = engine.zip_bytes(&emitted, "root");
        // local file header, then (at the end) the EOCD.
        assert_eq!(&bytes[0..4], &[0x50, 0x4b, 0x03, 0x04]);
        let n = bytes.len();
        assert_eq!(&bytes[n - 22..n - 18], &[0x50, 0x4b, 0x05, 0x06]);
        assert_eq!(u16::from_le_bytes([bytes[n - 12], bytes[n - 11]]), 2);
        let path = std::env::temp_dir().join("csgraph_reorg_zip_test.zip");
        std::fs::write(&path, &bytes).unwrap();
    }

    /// Pin the `groups()` key fix (task-2): a file with >= 2 owner units whose
    /// v2 merged documents each end in a *body-only* site (`opens = []`,
    /// `closes = [}...]`). Real-corpus trigger:
    /// `_sealed_src/Shorthand.cs.nzk:75`
    /// (`}}namespace NZK{  public static partial class B{`). Before the fix the
    /// two body-only sites from different units shared the `(opens, closes)`
    /// key, so unit B's members were emitted under unit A's still-open header
    /// (`NZK.A.B1()` instead of `NZK.B.B1()`) and left the file brace-unbalanced.
    /// Acceptance command:
    /// `python3 tools/diff_reorg.py --repo /2tb/@/nzk/git/vrcCS --root _sealed_src
    /// --spec 'group=owner'` -> `OK: 0 differences`.
    #[test]
    fn owner_groups_do_not_merge_body_only_sites_across_units() {
        let dir = std::env::temp_dir().join(format!(
            "csgraph-reorg-groups-{}",
            std::process::id()
        ));
        let src = dir.join("src");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&src).unwrap();
        // Two types, each declared as two partial sites, with the trigger line
        // `}}namespace NZK{` closing one scope and opening the next on one line.
        std::fs::write(
            src.join("one.cs"),
            r#"namespace NZK{
public static partial class A{
  public static void A1(){}
}}namespace NZK{
public static partial class A{
  public static void A2(){}
}}namespace NZK{
public static partial class B{
  public static void B1(){}
}}namespace NZK{
public static partial class B{
  public static void B2(){}
}}
"#,
        )
        .unwrap();

        let roots = vec!["src".to_string()];
        let (nodes, texts) = corpus_graph(dir.to_str().unwrap(), &roots, &[]);
        let engine = Reorg::new(&nodes, &texts, None, false, None);
        let spec = json!({"group": "owner"});
        let plan = engine.plan(Some(&spec)).unwrap();
        assert_eq!(
            plan["unitCount"].as_i64(),
            Some(2),
            "fixture must have two owner units: {plan}"
        );

        let emitted = engine.emit(&plan, true);
        let report = engine.verify(&emitted, None, Some(&plan));
        assert_eq!(report["ok"].as_bool(), Some(true), "report: {report}");
        assert!(
            report["added"].as_array().unwrap().is_empty(),
            "added declarations: {report}"
        );
        assert!(
            report["missing"].as_array().unwrap().is_empty(),
            "missing declarations: {report}"
        );
        assert!(
            report["unbalanced"].as_array().unwrap().is_empty(),
            "unbalanced files: {report}"
        );

        // A body-only site of the second unit must stay under its own class.
        let emitted_map: HashMap<String, String> = emitted.iter().cloned().collect();
        let after = Reorg::graph(&emitted_map);
        let decls = decl_map(&after);
        for want in ["NZK.B.B1()", "NZK.B.B2()"] {
            assert!(decls.contains_key(want), "{want} missing from {decls:?}");
        }
        for bad in ["NZK.A.B1()", "NZK.A.B2()"] {
            assert!(!decls.contains_key(bad), "{bad} was attributed to class A");
        }

        // Determinism: a second plan/emit is byte-identical.
        let plan2 = engine.plan(Some(&spec)).unwrap();
        let emitted2 = engine.emit(&plan2, true);
        assert_eq!(emitted, emitted2, "emit is not deterministic");

        // The old 39-file corpus has a single owner unit, so the header site is
        // the only grouping site and no cross-unit collision is possible. A
        // one-unit fixture must keep verifying clean.
        let one = dir.join("one_unit/src");
        std::fs::create_dir_all(&one).unwrap();
        std::fs::write(
            one.join("solo.cs"),
            r#"namespace NZK{
public static partial class Solo{
  public static void S1(){}
}}namespace NZK{
public static partial class Solo{
  public static void S2(){}
}}
"#,
        )
        .unwrap();
        let one_root = dir.join("one_unit");
        let (nodes1, texts1) = corpus_graph(one_root.to_str().unwrap(), &roots, &[]);
        let engine1 = Reorg::new(&nodes1, &texts1, None, false, None);
        let plan1 = engine1.plan(Some(&spec)).unwrap();
        assert_eq!(plan1["unitCount"].as_i64(), Some(1), "plan: {plan1}");
        let report1 = engine1.verify(&engine1.emit(&plan1, true), None, Some(&plan1));
        assert_eq!(report1["ok"].as_bool(), Some(true), "report: {report1}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
