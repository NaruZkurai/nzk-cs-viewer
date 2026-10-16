//! doc — port of `tools/csgraph/cs_doc.py`.
//!
//! cs_doc.py answers "given a graph node (namespace / type / member), what
//! would the whole thing look like if it were written as a single C# file?"
//! It reassembles one document out of every declaration site the graph knows
//! about, wrapping each site in the namespace / enclosing-type headers that
//! are still open at its first line and appending the matching closing braces,
//! while keeping every `#if` / `#elif` / `#else` / `#endif` region as its own
//! labelled section.
//!
//! Deliberate divergence from the frozen v1: declaration sites of one type that
//! share a wrapper **and** a conditional context are emitted under a single
//! `partial` header (see [`DocBuilder::emit_merged_group`]), with each fragment
//! still marked by its `// ── label · file:a-b ──` separator. v1 repeats the
//! header per site, which for a type with dozens of `partial` sites is noise.
//!
//! The Python module works on plain dicts (`node['parts']`, `part['line']`, …)
//! and so does this port: nodes are [`serde_json::Value`] objects and the file
//! texts are a `display path -> source` map, exactly the shapes `server.py`
//! feeds the original. That keeps the port callable from any graph builder and
//! lets `csgraph dump-doc` accept a graph snapshot.
//!
//! Fidelity notes:
//!
//! * every offset is a *character* offset, matching Python's `str` indexing;
//! * the Python regexes `RECORD_RE` / `TYPE_RE` are recreated here because the
//!   in-flight `parser` module does not export them yet;
//! * `write_doc`'s fingerprint is `sha1(full + '|' + str(len(text)))[:10]`, the
//!   exact rule the frozen `cs_doc.py` uses (its module docstring says 12, the
//!   code says 10 — the code wins);
//! * bad input yields [`DocError`], mirroring `server.py` catching per node.

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;

use serde_json::{json, Map, Value};

use crate::graph::build_graph;
use crate::hash::sha1_hex;
use crate::lex::{active_line_flags, mask_noncode, strip_bom};
use crate::model::{
    chars_to_string, find_sub_from, rstrip_ws, split_on, strip_ws, to_chars, Re,
};
use crate::parser::{discover_files, display_path, parse_source, read_python_text};

// ---------------------------------------------------------------------------
// module constants
// ---------------------------------------------------------------------------
/// `TYPE_KINDS`.
pub const TYPE_KINDS: [&str; 5] = ["class", "struct", "interface", "enum", "record"];
/// `WRAPPER_KINDS = ('namespace',) + TYPE_KINDS`.
pub const WRAPPER_KINDS: [&str; 6] =
    ["namespace", "class", "struct", "interface", "enum", "record"];

fn is_wrapper(kind: &str) -> bool {
    WRAPPER_KINDS.contains(&kind)
}

/// Identity of the scopes wrapping a declaration, used to group partial sites.
/// Two sites with the same wrapper names (e.g. both directly in namespace
/// `NZK`) can share one emitted header.
fn wrapper_key(fm: &FileModel, own: usize) -> String {
    let sc = &fm.scopes[own];
    let mut wraps: Vec<usize> = (0..fm.scopes.len())
        .filter(|&i| {
            i != own
                && is_wrapper(&fm.scopes[i].kind)
                && fm.scopes[i].open_off < sc.open_off
                && fm.scopes[i].close_off >= sc.close_off
        })
        .collect();
    wraps.sort_by_key(|&i| fm.scopes[i].open_off);
    let names: Vec<&str> = wraps.iter().map(|&i| fm.scopes[i].name.as_str()).collect();
    names.join("\u{1f}")
}

/// A site's line range clamped to the file, exactly as `emit_site` clamps it.
fn site_line_range(fm: &FileModel, s: &Site) -> Option<(usize, usize)> {
    let nlines = fm.lines.len() as i64;
    if nlines == 0 {
        return None;
    }
    let av = if s.a != 0 { s.a } else { 1 };
    let a = av.min(nlines).max(1) as usize;
    let bv = if s.b != 0 { s.b } else { a as i64 };
    let b = bv.min(nlines).max(a as i64) as usize;
    Some((a, b))
}

/// The `#if` / `#elif` / `#else` text that will be emitted *before* a
/// declaration's header. Two sites may share one emitted header only when this
/// is identical: `#if UNITY_EDITOR` around one fragment and nothing around
/// another must not silently become the same unconditional declaration.
fn frame_context_key(fm: &FileModel, own: usize) -> String {
    let start_line = fm.scopes[own].header_start_line;
    let mut ids: Vec<usize> = fm.frames_at(start_line as i64).to_vec();
    ids.sort_by_key(|&f| fm.frames[f].start_line);
    let mut parts: Vec<String> = Vec::new();
    for fid in ids {
        let fr = &fm.frames[fid];
        for (dl, dtext) in &fr.branches {
            if *dl < start_line {
                parts.push(chars_to_string(rstrip_ws(&to_chars(dtext))));
            }
        }
    }
    parts.join("\u{1f}")
}

fn directive_re() -> &'static Re {
    static R: OnceLock<Re> = OnceLock::new();
    R.get_or_init(|| Re::new(r"(?i)^\s*#\s*(if|elif|else|endif)\b(.*)$"))
}

fn ns_head_re() -> &'static Re {
    static R: OnceLock<Re> = OnceLock::new();
    R.get_or_init(|| Re::new(r"\bnamespace\s+([A-Za-z_][\w.]*)\s*$"))
}

fn record_re() -> &'static Re {
    static R: OnceLock<Re> = OnceLock::new();
    R.get_or_init(|| Re::new(r"\brecord\s+(class|struct)\s+([A-Za-z_]\w*)"))
}

fn type_re() -> &'static Re {
    static R: OnceLock<Re> = OnceLock::new();
    R.get_or_init(|| Re::new(r"\b(class|struct|interface|enum|record)\s+([A-Za-z_]\w*)"))
}

/// `<tmp>/csgraph-docs`, Python's `DOC_DIR`.
pub fn doc_dir() -> PathBuf {
    std::env::temp_dir().join("csgraph-docs")
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------
/// A conversion the Python original would raise on (`int(garbage)` and the
/// like). `server.py` swallows these per node, so callers may simply ignore it.
#[derive(Debug, Clone)]
pub struct DocError(pub String);

impl DocError {
    fn new(msg: impl Into<String>) -> DocError {
        DocError(msg.into())
    }
}

impl std::fmt::Display for DocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DocError {}

// ---------------------------------------------------------------------------
// value helpers — the Python `or` / `int()` / `dict.get` semantics
// ---------------------------------------------------------------------------
fn str_of<'x>(v: &'x Value, key: &str) -> &'x str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

/// `v.get(key) or ''` — the value when it is a non-empty string, else `''`.
fn first_str<'x>(v: &'x Value, key: &str) -> Option<&'x str> {
    v.get(key).and_then(|x| x.as_str()).filter(|s| !s.is_empty())
}

/// Python truthiness of a JSON value (used for `x or default`).
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

/// `int(value)` for the JSON shapes the parser emits.
fn py_int(v: Option<&Value>) -> Result<i64, DocError> {
    let Some(v) = v else {
        return Err(DocError::new("int() argument must not be None"));
    };
    match v {
        Value::Null => Err(DocError::new("int() argument must not be None")),
        Value::Bool(b) => Ok(if *b { 1 } else { 0 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i)
            } else if let Some(u) = n.as_u64() {
                i64::try_from(u).map_err(|_| DocError::new("int() out of range"))
            } else if let Some(f) = n.as_f64() {
                if f.is_finite() {
                    Ok(f.trunc() as i64)
                } else {
                    Err(DocError::new("cannot convert float to int"))
                }
            } else {
                Err(DocError::new("cannot convert number to int"))
            }
        }
        Value::String(s) => parse_py_int_string(s),
        other => Err(DocError::new(format!("int() of {:?}", other))),
    }
}

fn parse_py_int_string(s: &str) -> Result<i64, DocError> {
    let cs = to_chars(s);
    let t = strip_ws(&cs);
    if t.is_empty() {
        return Err(DocError::new("invalid literal for int()"));
    }
    let mut i = 0usize;
    let mut neg = false;
    if t[0] == '+' || t[0] == '-' {
        neg = t[0] == '-';
        i = 1;
    }
    let mut digits = String::new();
    let mut last_digit = false;
    let mut any = false;
    while i < t.len() {
        let c = t[i];
        if c == '_' {
            if !last_digit {
                return Err(DocError::new("invalid literal for int()"));
            }
            last_digit = false;
        } else if let Some(d) = c.to_digit(10) {
            digits.push(std::char::from_digit(d, 10).unwrap());
            last_digit = true;
            any = true;
        } else {
            return Err(DocError::new("invalid literal for int()"));
        }
        i += 1;
    }
    if !any || !last_digit {
        return Err(DocError::new("invalid literal for int()"));
    }
    let mag = i64::from_str_radix(&digits, 10)
        .map_err(|_| DocError::new("int() out of range"))?;
    Ok(if neg { -mag } else { mag })
}

/// `int(value or default)`.
fn py_int_or(v: Option<&Value>, default: i64) -> Result<i64, DocError> {
    match v {
        Some(x) if truthy(x) => py_int(Some(x)),
        _ => Ok(default),
    }
}

/// `_off(value)` — an offset, or `None` when it is missing / not usable.
fn off_of(v: Option<&Value>) -> Option<usize> {
    match py_int(v) {
        Ok(n) if n >= 0 => usize::try_from(n).ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// _slug / _line_at
// ---------------------------------------------------------------------------
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

/// `_line_at(starts, off)` — `bisect.bisect_right`.
fn line_at_starts(starts: &[usize], off: usize) -> usize {
    starts.partition_point(|&s| s <= off)
}

// ---------------------------------------------------------------------------
// FileModel
// ---------------------------------------------------------------------------
/// One brace scope, exactly the dict `_scan_scopes` builds.
#[derive(Debug, Clone)]
pub struct Scope {
    pub kind: String,
    pub name: String,
    pub open_off: usize,
    pub close_off: usize,
    pub open_line: usize,
    pub close_line: usize,
    pub header_start_off: usize,
    pub header_start_line: usize,
    pub depth: usize,
}

/// One conditional frame. The Python original keeps *references* to these
/// dicts in `line_frames`, so an `#endif` later updates the frame every
/// snapshot sees; here frames live in an arena and the snapshots hold indices.
#[derive(Debug, Clone)]
pub struct Frame {
    pub start_line: usize,
    pub end_line: usize,
    pub branches: Vec<(usize, String)>,
    pub depth: usize,
}

/// `class FileModel` — line/scope/conditional model of a single source file.
pub struct FileModel {
    pub text: Vec<char>,
    pub lines: Vec<Vec<char>>,
    _starts: Vec<usize>,
    pub clean: Vec<char>,
    _flags: Vec<bool>,
    pub defines_known: bool,
    pub scan: Vec<char>,
    pub scopes: Vec<Scope>,
    frames: Vec<Frame>,
    line_frames: Vec<Vec<usize>>,
}

impl FileModel {
    /// `FileModel(text, defines=None)`.
    pub fn new(text: &str, defines: Option<&HashSet<String>>) -> FileModel {
        let tb = strip_bom(&to_chars(text));
        let lines = split_on(&tb, '\n');
        let mut starts = vec![0usize];
        let mut i = 0usize;
        while let Some(p) = find_sub_from(&tb, &['\n'], i) {
            starts.push(p + 1);
            i = p + 1;
        }
        let clean = mask_noncode(&tb);
        let flags = active_line_flags(&clean, defines);
        let mut fm = FileModel {
            text: tb,
            lines,
            _starts: starts,
            clean,
            _flags: flags,
            defines_known: defines.is_some(),
            scan: Vec::new(),
            scopes: Vec::new(),
            frames: Vec::new(),
            line_frames: Vec::new(),
        };
        fm.scan = fm.clean.clone();
        fm.scan_scopes();
        fm.scan_frames();
        fm
    }

    /// `line_at(off)`.
    pub fn line_at(&self, off: usize) -> usize {
        line_at_starts(&self._starts, off)
    }

    /// `line_text(line)`.
    pub fn line_text(&self, line: i64) -> Vec<char> {
        let line = if line != 0 { line } else { 1 };
        let idx = line.max(1).min(self.lines.len() as i64) as usize;
        self.lines[idx - 1].clone()
    }

    /// `flags(line)`.
    pub fn flags(&self, line: i64) -> bool {
        let i = line - 1;
        if i >= 0 && (i as usize) < self._flags.len() {
            self._flags[i as usize]
        } else {
            true
        }
    }

    /// `frames_at(line)`.
    pub fn frames_at(&self, line: i64) -> &[usize] {
        let i = line - 1;
        if i >= 0 && (i as usize) < self.line_frames.len() {
            &self.line_frames[i as usize]
        } else {
            &[]
        }
    }

    /// `_classify(header)`.
    fn classify(header: &[char]) -> (String, String) {
        let h = strip_ws(header);
        if h.is_empty() {
            return ("block".to_string(), String::new());
        }
        if let Some(m) = ns_head_re().find(h) {
            return (
                "namespace".to_string(),
                m.group(1).unwrap_or("").to_string(),
            );
        }
        if let Some(m) = record_re().find(h) {
            return ("record".to_string(), m.group(2).unwrap_or("").to_string());
        }
        if let Some(m) = type_re().find(h) {
            return (
                m.group(1).unwrap_or("").to_string(),
                m.group(2).unwrap_or("").to_string(),
            );
        }
        ("block".to_string(), String::new())
    }

    /// `_scan_scopes(clean)`.
    fn scan_scopes(&mut self) {
        let clean = std::mem::take(&mut self.scan);
        let n = clean.len();
        let mut stack: Vec<usize> = Vec::new();
        let mut seg_start: Vec<usize> = vec![0];
        let mut i = 0usize;
        while i < n {
            let c = clean[i];
            if c == '{' {
                let start = *seg_start.last().unwrap();
                let header = &clean[start..i];
                let (kind, name) = Self::classify(header);
                let mut hs = start;
                loop {
                    while hs < i && matches!(clean[hs], ' ' | '\t' | '\r' | '\n') {
                        hs += 1;
                    }
                    if hs < i && clean[hs] == '#' {
                        match find_sub_from(&clean, &['\n'], hs) {
                            Some(nl) if nl < i => hs = nl + 1,
                            _ => hs = i,
                        }
                        continue;
                    }
                    break;
                }
                let open_line = self.line_at(i);
                let sc = Scope {
                    kind,
                    name,
                    open_off: i,
                    close_off: n,
                    open_line,
                    close_line: self.lines.len(),
                    header_start_off: hs,
                    header_start_line: self.line_at(hs),
                    depth: stack.len(),
                };
                self.scopes.push(sc);
                stack.push(self.scopes.len() - 1);
                seg_start.push(i + 1);
                i += 1;
                continue;
            }
            if c == '}' {
                if let Some(sidx) = stack.pop() {
                    seg_start.pop();
                    self.scopes[sidx].close_off = i;
                    self.scopes[sidx].close_line = self.line_at(i);
                }
                *seg_start.last_mut().unwrap() = i + 1;
                i += 1;
                continue;
            }
            if c == ';' {
                *seg_start.last_mut().unwrap() = i + 1;
            }
            i += 1;
        }
        self.scan = clean;
    }

    /// `_scan_frames()`.
    fn scan_frames(&mut self) {
        let mut stack: Vec<usize> = Vec::new();
        for idx in 1..=self.lines.len() {
            let ln = self.lines[idx - 1].clone();
            if let Some(m) = directive_re().match_at(&ln, 0) {
                let word = m.group(1).unwrap_or("").to_lowercase();
                if word == "if" {
                    let fr = Frame {
                        start_line: idx,
                        end_line: idx,
                        branches: vec![(idx, chars_to_string(&ln))],
                        depth: stack.len(),
                    };
                    self.frames.push(fr);
                    stack.push(self.frames.len() - 1);
                } else if word == "elif" || word == "else" {
                    if let Some(&fid) = stack.last() {
                        self.frames[fid].branches.push((idx, chars_to_string(&ln)));
                    }
                } else if word == "endif" {
                    if let Some(fid) = stack.pop() {
                        self.frames[fid].end_line = idx;
                    }
                }
            }
            // the frame stack AFTER this line is what wraps the next line
            self.line_frames.push(stack.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// DocBuilder
// ---------------------------------------------------------------------------
/// Shared model cache, the analogue of `server.py`'s `doc_cache`.
///
/// One instance lives on [`crate::index::Index`] for the whole session, so a
/// file is lexed/masked once no matter how many documents are synthesized from
/// it. `Arc` (not `Rc`) so the owning `Index` stays `Send` for the Tauri worker
/// threads; the counters make "parse once, reuse" observable and testable.
#[derive(Default)]
pub struct DocCache {
    inner: RefCell<HashMap<String, Arc<FileModel>>>,
    hits: std::cell::Cell<usize>,
    misses: std::cell::Cell<usize>,
}

impl DocCache {
    pub fn new() -> DocCache {
        DocCache::default()
    }

    /// `(modelCacheHits, modelCacheMisses)` since the index was built.
    pub fn stats(&self) -> (usize, usize) {
        (self.hits.get(), self.misses.get())
    }

    /// Number of distinct files currently lexed into the cache.
    pub fn len(&self) -> usize {
        self.inner.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `[(file, firstLine, lastLine, startOff, endOff)]`.
#[derive(Debug, Clone)]
struct Site {
    file: String,
    a: i64,
    b: i64,
    so: Option<usize>,
    eo: Option<usize>,
}

/// `class DocBuilder` — turns graph nodes + file texts into one document.
pub struct DocBuilder<'a> {
    nodes: &'a Map<String, Value>,
    file_texts: &'a HashMap<String, String>,
    defines: Option<&'a HashSet<String>>,
    defines_known: bool,
    cache: &'a DocCache,
}

impl<'a> DocBuilder<'a> {
    /// `DocBuilder(nodes, file_texts, defines=None, defines_known=False,
    /// cache=None)`.
    pub fn new(
        nodes: &'a Map<String, Value>,
        file_texts: &'a HashMap<String, String>,
        defines: Option<&'a HashSet<String>>,
        defines_known: bool,
        cache: &'a DocCache,
    ) -> DocBuilder<'a> {
        DocBuilder {
            nodes,
            file_texts,
            defines,
            defines_known,
            cache,
        }
    }

    /// `model(file)`.
    pub fn model(&self, file: &str) -> Option<Arc<FileModel>> {
        if let Some(fm) = self.cache.inner.borrow().get(file) {
            self.cache.hits.set(self.cache.hits.get() + 1);
            return Some(fm.clone());
        }
        let text = self.file_texts.get(file)?;
        let fm = Arc::new(FileModel::new(text, self.defines));
        self.cache.misses.set(self.cache.misses.get() + 1);
        self.cache
            .inner
            .borrow_mut()
            .insert(file.to_string(), fm.clone());
        Some(fm)
    }

    /// `_type_kids(ns_node)`.
    fn type_kids(&self, ns_node: &Value) -> Vec<&'a Value> {
        let target = first_str(ns_node, "ns")
            .or_else(|| first_str(ns_node, "full"))
            .unwrap_or("");
        let dot = format!("{target}.");
        let mut out: Vec<&'a Value> = Vec::new();
        for n in self.nodes.values() {
            let kind = str_of(n, "kind");
            if !TYPE_KINDS.contains(&kind) || truthy_get(n, "owner") {
                continue;
            }
            let ns = first_str(n, "ns").unwrap_or("");
            if ns == target || (!target.is_empty() && ns.starts_with(&dot)) {
                out.push(n);
            }
        }
        out.sort_by(|a, b| {
            let ka = (
                first_str(a, "ns").unwrap_or("").to_string(),
                first_str(a, "full").unwrap_or("").to_string(),
            );
            let kb = (
                first_str(b, "ns").unwrap_or("").to_string(),
                first_str(b, "full").unwrap_or("").to_string(),
            );
            ka.cmp(&kb)
        });
        out
    }

    /// `_sites(node)`.
    fn sites(&self, node: &Value) -> Result<Vec<Site>, DocError> {
        let mut sites: Vec<Site> = Vec::new();
        let parts = node
            .get("parts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for p in &parts {
            let f = first_str(p, "file")
                .or_else(|| first_str(node, "file"))
                .unwrap_or("");
            let a = py_int_or(p.get("line"), 0)?;
            let b = py_int_or(p.get("endLine"), a)?;
            if !f.is_empty() && a != 0 {
                sites.push(Site {
                    file: f.to_string(),
                    a,
                    b,
                    so: off_of(p.get("startOff")),
                    eo: off_of(p.get("endOff")),
                });
            }
        }
        if sites.is_empty() {
            if let Some(f) = first_str(node, "file") {
                if truthy_get(node, "line") {
                    let line_v = node.get("line");
                    let a = py_int(line_v)?;
                    let b = if truthy_get(node, "endLine") {
                        py_int(node.get("endLine"))?
                    } else {
                        a
                    };
                    sites.push(Site {
                        file: f.to_string(),
                        a,
                        b,
                        so: off_of(node.get("startOff")),
                        eo: off_of(node.get("endOff")),
                    });
                }
            }
        }
        sites.sort_by(|x, y| {
            (
                x.file.as_str(),
                x.a,
                x.so.map(|v| v as i64).unwrap_or(-1),
            )
                .cmp(&(
                    y.file.as_str(),
                    y.a,
                    y.so.map(|v| v as i64).unwrap_or(-1),
                ))
        });
        Ok(sites)
    }

    /// The `parts[]` payload entry for one declaration site.
    fn parts_entry(&self, root: &Value, s: &Site) -> Value {
        let node = root
            .get("full")
            .filter(|v| truthy(v))
            .or_else(|| root.get("id").filter(|v| truthy(v)))
            .cloned()
            .unwrap_or(Value::Null);
        json!({
            "file": s.file,
            "line": s.a,
            "endLine": s.b,
            "startOff": s.so,
            "endOff": s.eo,
            "node": node,
        })
    }

    /// `build(node_id, max_lines=20000)`. `max_lines` is unused upstream too.
    pub fn build(&self, node_id: &str, _max_lines: usize) -> Result<Option<Value>, DocError> {
        let Some(node) = self.nodes.get(node_id) else {
            return Ok(None);
        };
        let kind = str_of(node, "kind");
        if kind == "external" {
            return Ok(None);
        }

        let roots: Vec<&Value> = if kind == "namespace" {
            self.type_kids(node)
        } else {
            vec![node]
        };

        let mut out: Vec<Value> = Vec::new();
        let mut sec_count = 0usize;
        let mut parts: Vec<Value> = Vec::new();
        for root in roots {
            let sites = self.sites(root)?;
            if sites.is_empty() {
                continue;
            }
            let merge_kind = TYPE_KINDS.contains(&str_of(root, "kind"));

            // Partition the sites. A type site whose *header* is not inside a
            // conditional can share one emitted header with its siblings; a site
            // inside `#if … #endif` keeps its own section, because there the
            // condition is exactly the context the reader wants to see.
            let mut groups: Vec<(String, Vec<(Arc<FileModel>, usize, usize)>)> = Vec::new();
            let mut jobs: Vec<(usize, i64)> = Vec::new(); // (site index, group index | -1)
            for (si, s) in sites.iter().enumerate() {
                if merge_kind {
                    if let Some(fm) = self.model(&s.file) {
                        if let Some((a, b)) = site_line_range(&fm, s) {
                            if let Some(own) = self.own_scope(&fm, root, a, b) {
                                let sc = &fm.scopes[own];
                                // Every conditional open at the header must wrap
                                // the whole declaration; one that closes inside
                                // the body cannot be hoisted above a merged one.
                                let wraps_whole = fm
                                    .frames_at(sc.header_start_line as i64)
                                    .iter()
                                    .all(|&fid| fm.frames[fid].end_line > sc.close_line);
                                if wraps_whole {
                                    let key = format!(
                                        "{}\u{1e}{}",
                                        wrapper_key(&fm, own),
                                        frame_context_key(&fm, own)
                                    );
                                    let gi = match groups.iter().position(|(k, _)| *k == key) {
                                        Some(gi) => gi,
                                        None => {
                                            groups.push((key, Vec::new()));
                                            groups.len() - 1
                                        }
                                    };
                                    groups[gi].1.push((fm, own, si));
                                    jobs.push((si, gi as i64));
                                    continue;
                                }
                            }
                        }
                    }
                }
                jobs.push((si, -1));
            }
            jobs.sort_by_key(|(si, _)| *si);

            for (si, gi) in jobs {
                let s = &sites[si];
                if gi < 0 {
                    let Some(fm) = self.model(&s.file) else {
                        continue;
                    };
                    parts.push(self.parts_entry(root, s));
                    out.push(self.part_marker(root, &s.file, s.a, s.b));
                    sec_count += 1;
                    self.emit_site(&fm, &s.file, s.a, s.b, root, &mut out, s.so, s.eo)?;
                } else {
                    let group = &groups[gi as usize].1;
                    // One emission per group, at its first site.
                    if group.first().map(|t| t.2) != Some(si) {
                        continue;
                    }
                    self.emit_merged_group(root, group, &sites, &mut out, &mut parts, &mut sec_count);
                }
            }
        }
        if out.is_empty() {
            return Ok(None);
        }

        let kind_v = node.get("kind").cloned().unwrap_or(Value::Null);
        let file_v = node
            .get("file")
            .filter(|v| truthy(v))
            .cloned()
            .unwrap_or_else(|| json!(""));
        let line_v = node
            .get("line")
            .filter(|v| truthy(v))
            .cloned()
            .unwrap_or_else(|| json!(0));
        let node_payload = json!({
            "id": node.get("id").cloned().unwrap_or(Value::Null),
            "full": node.get("full").cloned().unwrap_or(Value::Null),
            "label": node.get("label").cloned().unwrap_or(Value::Null),
            "kind": kind_v,
            "file": file_v,
            "line": line_v,
        });

        Ok(Some(json!({
            "lines": out,
            "parts": parts,
            "sections": sec_count,
            "definesKnown": self.defines_known,
            "node": node_payload,
        })))
    }

    /// `_part_marker(root, file, a, b)`.
    fn part_marker(&self, root: &Value, file: &str, a: i64, b: i64) -> Value {
        let label = first_str(root, "full")
            .or_else(|| first_str(root, "id"))
            .unwrap_or("");
        json!({
            "n": 0,
            "ctx": "part",
            "text": format!(
                "// \u{2500}\u{2500} {} \u{00b7} {}:{}-{} \u{2500}\u{2500}",
                label, file, a, b
            ),
            "file": file,
            "orig": a,
            "node": root.get("id").cloned().unwrap_or(Value::Null),
        })
    }

    /// `_own_scope(fm, node, a, b)`.
    fn own_scope(&self, fm: &FileModel, node: &Value, a: usize, b: usize) -> Option<usize> {
        let kind = str_of(node, "kind");
        let want_kind: String;
        let want_name: Option<String>;
        if kind == "namespace" {
            let full = first_str(node, "full").unwrap_or("");
            let name = first_str(node, "name").unwrap_or_else(|| {
                full.rsplit('.').next().unwrap_or("")
            });
            want_kind = "namespace".to_string();
            want_name = Some(name.to_string());
        } else if TYPE_KINDS.contains(&kind) {
            want_kind = kind.to_string();
            want_name = node.get("name").and_then(|v| v.as_str()).map(str::to_string);
        } else {
            return None;
        }
        let mut found = None;
        for (i, sc) in fm.scopes.iter().enumerate() {
            if sc.kind == want_kind
                && Some(sc.name.as_str()) == want_name.as_deref()
                && a <= sc.open_line
                && sc.open_line <= b
            {
                found = Some(i);
            }
        }
        found
    }

    /// `_emit_site(fm, file, a, b, node, out, start_off=None, end_off=None)`.
    fn emit_site(
        &self,
        fm: &FileModel,
        file: &str,
        a0: i64,
        b0: i64,
        node: &Value,
        out: &mut Vec<Value>,
        start_off: Option<usize>,
        end_off: Option<usize>,
    ) -> Result<(), DocError> {
        let nlines = fm.lines.len() as i64;
        let av = if a0 != 0 { a0 } else { 1 };
        let a = av.min(nlines).max(1) as usize;
        let bv = if b0 != 0 { b0 } else { a as i64 };
        let b = bv.min(nlines).max(a as i64) as usize;

        let own = self.own_scope(fm, node, a, b);
        let len_text = fm.text.len();
        let body_start: usize;
        let mut body_end: usize;

        if let Some(oi) = own {
            // A namespace/type site IS its own scope: exact range, header in body.
            body_start = fm.scopes[oi].header_start_off;
            body_end = fm.scopes[oi].close_off + 1;
        } else if let (Some(so), Some(eo)) = (start_off, end_off) {
            if so < eo && eo <= len_text {
                body_start = so;
                body_end = eo;
            } else {
                let (s, e) = Self::line_granular(fm, a, b, own, len_text);
                body_start = s;
                body_end = e;
            }
        } else {
            let (s, e) = Self::line_granular(fm, a, b, own, len_text);
            body_start = s;
            body_end = e;
        }
        body_end = body_end.min(len_text);
        if body_end <= body_start {
            return Ok(());
        }

        let wraps: Vec<usize> = (0..fm.scopes.len())
            .filter(|&i| is_wrapper(&fm.scopes[i].kind) && Some(i) != own)
            .collect();
        // Any wrapper whose close still lies inside an over-spanned member range
        // must be trimmed too. Exact namespace/type bodies are never trimmed.
        if own.is_none() {
            let ends: Vec<usize> = wraps
                .iter()
                .filter(|&&i| body_start < fm.scopes[i].close_off && fm.scopes[i].close_off < body_end)
                .map(|&i| fm.scopes[i].close_off)
                .collect();
            if let Some(m) = ends.iter().min() {
                body_end = *m;
            }
        }
        if body_end <= body_start {
            return Ok(());
        }
        let start_line = fm.line_at(body_start);
        let end_line = fm.line_at(body_end - 1);

        // 1. wrappers that contain the whole body.
        let mut scopes: Vec<usize> = (0..fm.scopes.len())
            .filter(|&i| {
                Some(i) != own
                    && is_wrapper(&fm.scopes[i].kind)
                    && fm.scopes[i].open_off < body_start
                    && fm.scopes[i].close_off >= body_end
            })
            .collect();
        scopes.sort_by_key(|&i| fm.scopes[i].open_off);
        // (is_frame, offset, index); Python compares `'frame' < 'scope'`.
        let mut opens: Vec<(bool, usize, usize)> = scopes
            .iter()
            .map(|&i| (false, fm.scopes[i].open_off, i))
            .collect();
        for &fid in fm.frames_at(start_line as i64) {
            if fm.frames[fid].start_line < start_line {
                opens.push((true, fm._starts[fm.frames[fid].start_line - 1], fid));
            }
        }
        // Python sorts by offset alone (`opens.sort(key=lambda t: t[1])`);
        // `sort_by_key` is stable, so scopes-before-frames survives ties.
        opens.sort_by_key(|&(_, off, _)| off);

        let mark = out.len();
        for &(is_frame, _off, idx) in &opens {
            if !is_frame {
                Self::emit_head(fm, file, idx, out);
            } else {
                let branches = fm.frames[idx].branches.clone();
                for (dl, dtext) in branches {
                    if dl < start_line {
                        out.push(Self::cond_rec(file, dl, &to_chars(&dtext), "open"));
                    }
                }
            }
        }
        for rec in out[mark..].iter_mut() {
            rec["wrap"] = json!("open");
        }

        // 2. the body itself, every conditional kept as its own section.
        Self::emit_range(fm, file, body_start, body_end, out);

        // 3. close exactly what step 1 opened, in reverse (LIFO).
        let mut closers: Vec<(bool, usize, usize)> = Vec::new();
        for &(is_frame, off, idx) in &opens {
            if is_frame {
                if fm.frames[idx].end_line > end_line {
                    closers.push((true, off, idx));
                }
            } else if fm.scopes[idx].close_off >= body_end {
                closers.push((false, off, idx));
            }
        }
        let mark2 = out.len();
        for &(is_frame, _off, idx) in closers.iter().rev() {
            if is_frame {
                let el = fm.frames[idx].end_line;
                let txt = fm.line_text(el as i64);
                out.push(Self::cond_rec(file, el, &txt, "close"));
            } else {
                let cl = fm.scopes[idx].close_off;
                let ol = fm.line_at(cl);
                out.push(json!({
                    "n": 0,
                    "ctx": "tail",
                    "text": "}",
                    "file": file,
                    "orig": ol,
                    "active": fm.flags(ol as i64),
                }));
            }
        }
        for rec in out[mark2..].iter_mut() {
            rec["wrap"] = json!("close");
        }
        Ok(())
    }

    /// Emit `[body_start, body_end)` of `fm` as `body` / `cond` records.
    fn emit_range(fm: &FileModel, file: &str, body_start: usize, body_end: usize, out: &mut Vec<Value>) {
        if body_end <= body_start || body_start >= fm.text.len() {
            return;
        }
        let start_line = fm.line_at(body_start);
        let end_line = fm.line_at(body_end - 1);
        let first_col = body_start - fm._starts[start_line - 1];
        let last_col = body_end - fm._starts[end_line - 1];
        for l in start_line..=end_line {
            let raw = &fm.lines[l - 1];
            let raw_len = raw.len();
            let text: Vec<char> = if l == start_line && l == end_line {
                slice_chars(raw, first_col, last_col)
            } else if l == start_line {
                slice_chars(raw, first_col, raw_len)
            } else if l == end_line {
                slice_chars(raw, 0, last_col)
            } else {
                raw.clone()
            };
            if let Some(m) = directive_re().match_at(raw, 0) {
                let word = m.group(1).unwrap_or("").to_lowercase();
                let sec = if word == "if" {
                    "open"
                } else if word == "endif" {
                    "close"
                } else {
                    "mid"
                };
                out.push(Self::cond_rec(file, l, &text, sec));
            } else {
                out.push(json!({
                    "n": 0,
                    "ctx": "body",
                    "text": chars_to_string(&text),
                    "file": file,
                    "orig": l,
                    "active": fm.flags(l as i64),
                }));
            }
        }
    }

    /// Merge several `partial` declaration sites of one type into a single
    /// declaration.
    ///
    /// The wrapper (namespace / enclosing types) and the type header are emitted
    /// once, each site's *inner* body follows under its own `// ── label ·
    /// file:a-b ──` marker, and the braces are closed once. That is the point:
    /// reading `NZK.B` or `NZK.S` must not repeat `class B {` dozens of times.
    ///
    /// The conditional context is part of the group key (see
    /// [`frame_context_key`]), so `#if UNITY_EDITOR` active over every fragment
    /// is emitted once around the merged declaration, while a fragment under a
    /// *different* condition keeps its own labelled section. The header keeps
    /// `partial`, so the merged document would still compile if the fragments
    /// turn out not to be mergeable.
    fn emit_merged_group(
        &self,
        root: &Value,
        group: &[(Arc<FileModel>, usize, usize)],
        sites: &[Site],
        out: &mut Vec<Value>,
        parts: &mut Vec<Value>,
        sec_count: &mut usize,
    ) {
        let (fm0, own0) = (group[0].0.clone(), group[0].1);
        let site0 = &sites[group[0].2];
        let sc0 = &fm0.scopes[own0];
        let start_line = sc0.header_start_line;
        let end_line = sc0.close_line;

        // Wrappers that contain the declaration.
        let mut scopes: Vec<usize> = (0..fm0.scopes.len())
            .filter(|&i| {
                i != own0
                    && is_wrapper(&fm0.scopes[i].kind)
                    && fm0.scopes[i].open_off < sc0.open_off
                    && fm0.scopes[i].close_off >= sc0.close_off
            })
            .collect();
        scopes.sort_by_key(|&i| fm0.scopes[i].open_off);

        // (is_frame, offset, index); stable sort keeps wrappers before frames on
        // a tie, which is the order `emit_site` uses.
        let mut opens: Vec<(bool, usize, usize)> = scopes
            .iter()
            .map(|&i| (false, fm0.scopes[i].open_off, i))
            .collect();
        for &fid in fm0.frames_at(start_line as i64) {
            if fm0.frames[fid].start_line < start_line {
                opens.push((true, fm0._starts[fm0.frames[fid].start_line - 1], fid));
            }
        }
        opens.sort_by_key(|&(_, off, _)| off);

        let mark = out.len();
        for &(is_frame, _off, idx) in &opens {
            if !is_frame {
                Self::emit_head(&fm0, &site0.file, idx, out);
            } else {
                for (dl, dtext) in fm0.frames[idx].branches.clone() {
                    if dl < start_line {
                        out.push(Self::cond_rec(&site0.file, dl, &to_chars(&dtext), "open"));
                    }
                }
            }
        }
        // The single type header, still `partial`.
        let head_mark = out.len();
        Self::emit_head(&fm0, &site0.file, own0, out);
        for rec in out[head_mark..].iter_mut() {
            if let Some(t) = rec.get("text").and_then(|v| v.as_str()) {
                let fixed = crate::reorg::ensure_partial(t);
                if fixed != t {
                    rec["text"] = json!(fixed);
                }
            }
        }
        for rec in out[mark..].iter_mut() {
            rec["wrap"] = json!("open");
        }

        for (fm, own, si) in group {
            let s = &sites[*si];
            out.push(self.part_marker(root, &s.file, s.a, s.b));
            *sec_count += 1;
            parts.push(self.parts_entry(root, s));
            let sc = &fm.scopes[*own];
            Self::emit_range(fm, &s.file, sc.open_off + 1, sc.close_off, out);
        }

        let close_mark = out.len();
        out.push(json!({
            "n": 0,
            "ctx": "tail",
            "text": "}",
            "file": site0.file,
            "orig": sc0.close_line,
            "active": fm0.flags(sc0.close_line as i64),
        }));
        // Close exactly what `opens` opened, innermost first (LIFO).
        let mut closers: Vec<(bool, usize, usize)> = Vec::new();
        for &(is_frame, off, idx) in &opens {
            if is_frame {
                if fm0.frames[idx].end_line > end_line {
                    closers.push((true, off, idx));
                }
            } else if fm0.scopes[idx].close_off >= sc0.close_off {
                closers.push((false, off, idx));
            }
        }
        for &(is_frame, _off, idx) in closers.iter().rev() {
            if is_frame {
                let el = fm0.frames[idx].end_line;
                let txt = fm0.line_text(el as i64);
                out.push(Self::cond_rec(&site0.file, el, &txt, "close"));
            } else {
                let sc = &fm0.scopes[idx];
                out.push(json!({
                    "n": 0,
                    "ctx": "tail",
                    "text": "}",
                    "file": site0.file,
                    "orig": sc.close_line,
                    "active": fm0.flags(sc.close_line as i64),
                }));
            }
        }
        for rec in out[close_mark..].iter_mut() {
            rec["wrap"] = json!("close");
        }
    }

    /// The line-granular fallback of `_emit_site`.
    fn line_granular(
        fm: &FileModel,
        a: usize,
        b: usize,
        own: Option<usize>,
        len_text: usize,
    ) -> (usize, usize) {
        let mut body_start = fm._starts[a - 1];
        let mut body_end = if b < fm._starts.len() {
            fm._starts[b].wrapping_sub(1)
        } else {
            len_text
        };
        body_end = body_end.min(len_text);
        let inside: Option<usize> = (0..fm.scopes.len())
            .filter(|&i| {
                is_wrapper(&fm.scopes[i].kind)
                    && Some(i) != own
                    && body_start <= fm.scopes[i].open_off
                    && fm.scopes[i].open_off < body_end
            })
            .max_by_key(|&i| fm.scopes[i].open_off);
        if let Some(inner) = inside {
            body_start = fm.scopes[inner].open_off + 1;
            body_end = body_end.min(fm.scopes[inner].close_off);
        }
        (body_start, body_end)
    }

    /// `_cond_rec(file, line, text, sec)`.
    fn cond_rec(file: &str, line: usize, text: &[char], sec: &str) -> Value {
        json!({
            "n": 0,
            "ctx": "cond",
            "text": chars_to_string(rstrip_ws(text)),
            "file": file,
            "orig": line,
            "sec": sec,
            "section": chars_to_string(strip_ws(text)),
        })
    }

    /// `_emit_head(fm, file, sc, out)`.
    fn emit_head(fm: &FileModel, file: &str, sc_idx: usize, out: &mut Vec<Value>) {
        let sc = &fm.scopes[sc_idx];
        let hs = sc.header_start_off;
        let oe = sc.open_off + 1;
        if hs > oe || oe > fm.text.len() {
            return;
        }
        let head: Vec<char> = fm.text[hs..oe].to_vec();
        if head.is_empty() {
            return;
        }
        let full = Self::scope_full(fm, sc_idx);
        let base_line = sc.header_start_line;
        for (i, part) in split_on(&head, '\n').iter().enumerate() {
            let line = base_line + i;
            out.push(json!({
                "n": 0,
                "ctx": "head",
                "text": chars_to_string(part),
                "file": file,
                "orig": line,
                "full": full,
                "active": fm.flags(line as i64),
            }));
        }
    }

    /// `_scope_full(fm, sc)`.
    fn scope_full(fm: &FileModel, sc_idx: usize) -> String {
        let sc = &fm.scopes[sc_idx];
        let mut parts: Vec<(usize, String)> = Vec::new();
        for (i, other) in fm.scopes.iter().enumerate() {
            if i == sc_idx {
                continue;
            }
            if is_wrapper(&other.kind)
                && other.open_off < sc.open_off
                && sc.open_off < other.close_off
            {
                parts.push((other.open_off, other.name.clone()));
            }
        }
        parts.sort();
        let mut names: Vec<String> = parts.into_iter().map(|p| p.1).collect();
        names.push(sc.name.clone());
        names.join(".")
    }
}

/// `s[a:b]` with Python-like clamping (never panics).
fn slice_chars(s: &[char], a: usize, b: usize) -> Vec<char> {
    let a = a.min(s.len());
    let b = b.max(a).min(s.len());
    s[a..b].to_vec()
}

// ---------------------------------------------------------------------------
// /tmp materialization
// ---------------------------------------------------------------------------
/// `doc_text(lines)`.
pub fn doc_text(lines: &[Value]) -> String {
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(l.get("text").and_then(|v| v.as_str()).unwrap_or(""));
    }
    out
}

/// `write_doc(node, text, directory=None)`.
pub fn write_doc(node: &Value, text: &str, directory: Option<&Path>) -> String {
    let dir = directory
        .map(PathBuf::from)
        .unwrap_or_else(doc_dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return String::new();
    }
    let full = first_str(node, "full")
        .or_else(|| first_str(node, "id"))
        .unwrap_or("node");
    let digest = sha1_hex(format!("{}|{}", full, text.chars().count()).as_bytes());
    let h = &digest[..digest.len().min(10)];
    let path = dir.join(format!("{}.{}.cs", slug(full, 110), h));
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    let mut payload = String::from(text);
    if !text.ends_with('\n') {
        payload.push('\n');
    }
    if std::fs::write(&tmp, payload.as_bytes()).is_err() {
        return String::new();
    }
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return String::new();
    }
    path.to_string_lossy().into_owned()
}

/// `prepare(nodes, file_texts, node_id, defines=None, defines_known=False,
/// cache=None, max_lines=20000)`.
pub fn prepare(
    nodes: &Map<String, Value>,
    file_texts: &HashMap<String, String>,
    node_id: &str,
    defines: Option<&HashSet<String>>,
    defines_known: bool,
    cache: &DocCache,
    max_lines: usize,
) -> Result<Option<Value>, DocError> {
    let builder = DocBuilder::new(nodes, file_texts, defines, defines_known, cache);
    let Some(mut doc) = builder.build(node_id, max_lines)? else {
        return Ok(None);
    };

    // number the real lines of the generated document 1..N, keeping chrome at 0
    let total = {
        let lines = doc
            .get_mut("lines")
            .and_then(|v| v.as_array_mut())
            .ok_or_else(|| DocError::new("built doc has no lines"))?;
        let mut n = 0i64;
        for ln in lines.iter_mut() {
            let ctx = ln.get("ctx").and_then(|v| v.as_str()).unwrap_or("");
            if ctx == "part" {
                ln["n"] = json!(0);
                continue;
            }
            n += 1;
            ln["n"] = json!(n);
            let orig = ln.get("orig").cloned().unwrap_or(Value::Null);
            if truthy(&orig) && ln.get("src").is_none() {
                let f = ln
                    .get("file")
                    .filter(|v| truthy(v))
                    .cloned()
                    .unwrap_or_else(|| json!(""));
                ln["src"] = json!({"file": f, "line": orig});
            }
        }
        n
    };

    let lines_val = doc
        .get_mut("lines")
        .map(|v| v.take())
        .unwrap_or_else(|| json!([]));
    let text = {
        let arr = lines_val.as_array().map(Vec::as_slice).unwrap_or(&[]);
        doc_text(arr)
    };
    let (start, end) = {
        let arr = lines_val.as_array().map(Vec::as_slice).unwrap_or(&[]);
        let starts: Vec<i64> = arr
            .iter()
            .filter(|l| {
                let ctx = l.get("ctx").and_then(|v| v.as_str()).unwrap_or("");
                ctx != "part" && ctx != "cond"
            })
            .map(|l| l.get("n").and_then(|v| v.as_i64()).unwrap_or(0))
            .collect();
        if starts.is_empty() {
            (1i64, 1i64)
        } else {
            (
                starts.iter().copied().min().unwrap_or(1),
                starts.iter().copied().max().unwrap_or(1),
            )
        }
    };
    let conds = lines_val
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter(|l| l.get("ctx").and_then(|v| v.as_str()) == Some("cond"))
                .count()
        })
        .unwrap_or(0);

    let node = doc.get("node").cloned().unwrap_or(Value::Null);
    let path = write_doc(&node, &text, None);
    let parts = doc.get("parts").cloned().unwrap_or_else(|| json!([]));
    let parts_len = parts.as_array().map(|a| a.len()).unwrap_or(0);
    let file_field = if path.is_empty() {
        first_str(&node, "full")
            .map(str::to_string)
            .unwrap_or_else(|| node_id.to_string())
    } else {
        path.clone()
    };

    let mut payload = Map::new();
    payload.insert("ok".to_string(), json!(true));
    payload.insert("file".to_string(), json!(file_field));
    payload.insert("doc".to_string(), json!(true));
    payload.insert("mode".to_string(), json!("doc"));
    payload.insert("node".to_string(), node);
    payload.insert("path".to_string(), json!(path));
    payload.insert("total".to_string(), json!(total));
    payload.insert("start".to_string(), json!(start));
    payload.insert("end".to_string(), json!(end));
    payload.insert("line".to_string(), json!(start));
    payload.insert(
        "definesKnown".to_string(),
        doc.get("definesKnown").cloned().unwrap_or(json!(false)),
    );
    payload.insert("parts".to_string(), parts);
    payload.insert("sites".to_string(), json!(parts_len));
    payload.insert("sections".to_string(), json!(conds));
    payload.insert("lines".to_string(), lines_val);
    Ok(Some(Value::Object(payload)))
}

// ---------------------------------------------------------------------------
// path relativisation (mirrors today's oracle.py `_rel`/`_norm`)
// ---------------------------------------------------------------------------
/// `_rel(value, repo)` for one string.
pub fn rel_path(value: &str, repo: &str) -> String {
    if value.is_empty() || !value.starts_with(repo) {
        return value.to_string();
    }
    let rel = value[repo.len()..].trim_start_matches('/');
    if rel.is_empty() {
        ".".to_string()
    } else {
        rel.to_string()
    }
}

/// `_norm(obj, repo)` — rewrite every absolute repo path to a relative one.
pub fn rel_value(v: &Value, repo: &str) -> Value {
    match v {
        Value::String(s) => Value::String(rel_path(s, repo)),
        Value::Array(a) => Value::Array(a.iter().map(|x| rel_value(x, repo)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), rel_value(x, repo)))
                .collect(),
        ),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// `csgraph dump-doc`
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

/// Build the `nodes` (id -> dict, Python's `index.nodes` shape) and
/// `file_texts` maps straight from the corpus, exactly the way `dump-parser`
/// walks it.
fn build_corpus_graph(
    repo: &str,
    roots: &[String],
    excludes: &[String],
    defines: Option<&HashSet<String>>,
) -> (Map<String, Value>, HashMap<String, String>) {
    let repo_path = Path::new(repo);
    let files = discover_files(repo_path, roots, excludes);
    let mut parses: Vec<Value> = Vec::new();
    let mut file_texts: HashMap<String, String> = HashMap::new();
    for f in &files {
        let p = Path::new(f);
        let disp = display_path(p, repo_path);
        parses.push(parse_source(p, &disp, defines));
        let text = read_python_text(p)
            .map(|cs| chars_to_string(&cs))
            .unwrap_or_default();
        file_texts.insert(disp, text);
    }
    let gb = build_graph(&parses);
    let mut nodes: Map<String, Value> = Map::new();
    for n in &gb.nodes {
        let Some(id) = n.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let mut obj = n.as_object().cloned().unwrap_or_default();
        // The graph keeps the declaration-site list private (`_parts`);
        // `server.py` promotes it to the public `parts` doc reads.
        let parts = obj
            .get("_parts")
            .cloned()
            .unwrap_or_else(|| json!([]));
        obj.insert("parts".to_string(), parts);
        nodes.insert(id.to_string(), Value::Object(obj));
    }
    (nodes, file_texts)
}

/// Read a graph snapshot `{nodes, file_texts, defines, definesKnown,
/// nodeIds}` — the shape `tools/oracle_doc.py --graph-out` writes. This mode
/// isolates the document synthesis from the parser/graph build.
fn load_snapshot(
    text: &str,
) -> Result<(Map<String, Value>, HashMap<String, String>, Option<HashSet<String>>, bool, Vec<String>), String> {
    let graph: Value =
        serde_json::from_str(text).map_err(|e| format!("graph json: {e}"))?;
    let nodes: Map<String, Value> = graph
        .get("nodes")
        .and_then(|v| v.as_object())
        .cloned()
        .ok_or_else(|| "graph snapshot has no `nodes` object".to_string())?;
    let file_texts: HashMap<String, String> = graph
        .get("file_texts")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let defines: Option<HashSet<String>> = graph
        .get("defines")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        });
    let defines_known = graph
        .get("definesKnown")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let node_ids: Vec<String> = graph
        .get("nodeIds")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok((nodes, file_texts, defines, defines_known, node_ids))
}

/// `csgraph dump-doc` — synthesize one document per requested node and print
/// `{definesKnown, nodeIds, docs}` as JSON.
///
/// Corpus mode (the acceptance run):
///
/// ```text
/// csgraph dump-doc --repo DIR [--root REL]... [--exclude PAT]...
///                  [--define SYM]... [--node ID]...
/// ```
///
/// With `--define` the conditional symbols are known (and passed to the
/// parser); without them the parser keeps every branch, exactly like the
/// sealed-corpus oracle.
///
/// Snapshot mode (`--graph FILE`, or `-` for stdin) accepts the same
/// `nodes` / `file_texts` / `defines` / `definesKnown` / `nodeIds` document
/// that `tools/oracle_doc.py --graph-out` writes, so `doc.rs` can be checked
/// independently of the in-flight parser port.
pub fn cli_dump_doc(args: &[String]) -> Result<(), String> {
    let repo = arg_flag(args, "--repo").unwrap_or_default();
    let node_args = arg_all(args, "--node");
    let define_args = arg_all(args, "--define");

    let (nodes, file_texts, defines, defines_known, node_ids) =
        if let Some(graph_path) = arg_flag(args, "--graph") {
            let text = if graph_path == "-" {
                let mut s = String::new();
                std::io::stdin()
                    .read_to_string(&mut s)
                    .map_err(|e| format!("stdin: {e}"))?;
                s
            } else {
                std::fs::read_to_string(&graph_path)
                    .map_err(|e| format!("{graph_path}: {e}"))?
            };
            let (n, ft, d, dk, ids) = load_snapshot(&text)?;
            (n, ft, d, dk, if node_args.is_empty() { ids } else { node_args })
        } else {
            if repo.is_empty() {
                return Err("--repo is required".to_string());
            }
            let mut roots = arg_all(args, "--root");
            if roots.is_empty() {
                roots.push("_sealed_src".to_string());
            }
            let excludes = arg_all(args, "--exclude");
            let dset: HashSet<String> = define_args.iter().cloned().collect();
            let dopt = if dset.is_empty() { None } else { Some(&dset) };
            let (n, ft) = build_corpus_graph(&repo, &roots, &excludes, dopt);
            let dk = dopt.is_some();
            (n, ft, dopt.cloned(), dk, node_args)
        };

    let mut node_ids = node_ids;
    if node_ids.is_empty() {
        node_ids = nodes.keys().cloned().collect();
        node_ids.sort();
    }

    let cache = DocCache::new();
    let mut docs = Map::new();
    for nid in &node_ids {
        let payload = prepare(
            &nodes,
            &file_texts,
            nid,
            defines.as_ref(),
            defines_known,
            &cache,
            20000,
        )
        .ok()
        .flatten();
        let payload = match payload {
            Some(p) if !repo.is_empty() => rel_value(&p, &repo),
            other => other.unwrap_or(Value::Null),
        };
        docs.insert(nid.clone(), payload);
    }

    let defines_json = match &defines {
        Some(d) => {
            let mut v: Vec<String> = d.iter().cloned().collect();
            v.sort();
            Value::Array(v.into_iter().map(Value::String).collect())
        }
        None => Value::Null,
    };
    let repo_name = repo.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    let out = json!({
        "repo": repo_name,
        "defines": defines_json,
        "definesKnown": defines_known,
        "nodeIds": node_ids,
        "docs": docs,
    });
    let stdout = serde_json::to_string(&out).map_err(|e| e.to_string())?;
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
    fn file_model_scopes_and_frames() {
        let src = "namespace A {\nclass B {\n#if X\nint f;\n#endif\n}\n}\n";
        let fm = FileModel::new(src, None);
        assert_eq!(fm.lines.len(), 8);
        assert_eq!(fm.scopes.len(), 2);
        assert_eq!(fm.scopes[0].kind, "namespace");
        assert_eq!(fm.scopes[0].name, "A");
        assert_eq!(fm.scopes[1].kind, "class");
        assert_eq!(fm.scopes[1].name, "B");
        // line_frames[3] is the stack after line 4 (`int f;`), i.e. inside #if.
        assert_eq!(fm.frames_at(4).len(), 1);
        assert!(fm.frames_at(1).is_empty());
    }

    #[test]
    fn int_parsing() {
        assert_eq!(py_int(Some(&json!(3))).unwrap(), 3);
        assert_eq!(py_int(Some(&json!(" 12 "))).unwrap(), 12);
        assert_eq!(py_int(Some(&json!(-4))).unwrap(), -4);
        assert!(py_int(Some(&json!("x"))).is_err());
        assert_eq!(off_of(Some(&json!(-1))), None);
        assert_eq!(off_of(Some(&json!(5))), Some(5));
    }

    fn build_doc(files: &[(&str, &str)], parts: Value) -> Value {
        let mut nodes: Map<String, Value> = Map::new();
        nodes.insert(
            "NZK.B".to_string(),
            json!({
                "id": "NZK.B", "full": "NZK.B", "label": "B",
                "kind": "class", "name": "B", "ns": "NZK", "parts": parts,
            }),
        );
        let mut ft: HashMap<String, String> = HashMap::new();
        for (name, text) in files {
            ft.insert((*name).to_string(), (*text).to_string());
        }
        let cache = DocCache::new();
        prepare(&nodes, &ft, "NZK.B", None, false, &cache, 20000)
            .unwrap()
            .unwrap()
    }

    fn ctx_count(doc: &Value, ctx: &str) -> usize {
        doc["lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l.get("ctx").and_then(|v| v.as_str()) == Some(ctx))
            .count()
    }

    fn body_text(doc: &Value) -> String {
        doc["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.get("text").and_then(|v| v.as_str()).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn partial_sites_share_one_header() {
        let a = "namespace NZK {\npublic static partial class B {\n    public static int A1() { return 1; }\n}\n}\n";
        let b = "namespace NZK {\npublic static partial class B {\n    public static int A2() { return 2; }\n}\n}\n";
        let parts = json!([
            {"file": "a.cs", "line": 1, "endLine": 5},
            {"file": "b.cs", "line": 1, "endLine": 5},
        ]);
        let doc = build_doc(&[("a.cs", a), ("b.cs", b)], parts);
        assert_eq!(ctx_count(&doc, "head"), 2, "one namespace + one type header");
        assert_eq!(ctx_count(&doc, "tail"), 2);
        assert_eq!(ctx_count(&doc, "part"), 2, "both fragments are marked");
        let text = body_text(&doc);
        assert!(text.contains("A1") && text.contains("A2"), "both bodies kept");
        assert_eq!(
            text.matches("class B").count(),
            1,
            "the declaration is emitted once, not per fragment"
        );
    }

    #[test]
    fn same_conditional_context_merges_once() {
        let a = "#if UNITY_EDITOR\nnamespace NZK {\npublic static partial class B {\n    public static int A1() { return 1; }\n}\n}\n#endif\n";
        let b = "#if UNITY_EDITOR\nnamespace NZK {\npublic static partial class B {\n    public static int A2() { return 2; }\n}\n}\n#endif\n";
        let parts = json!([
            {"file": "a.cs", "line": 1, "endLine": 8},
            {"file": "b.cs", "line": 1, "endLine": 8},
        ]);
        let doc = build_doc(&[("a.cs", a), ("b.cs", b)], parts);
        assert_eq!(ctx_count(&doc, "head"), 2);
        let text = body_text(&doc);
        assert_eq!(text.matches("class B").count(), 1);
        assert_eq!(text.matches("#if UNITY_EDITOR").count(), 1, "condition emitted once");
        assert_eq!(text.matches("#endif").count(), 1);
    }

    #[test]
    fn different_conditionals_stay_separate() {
        // One fragment unconditional, one under `#if X`: they cannot share a
        // header, because the condition would silently change meaning.
        let a = "namespace NZK {\npublic static partial class B {\n    public static int A1() { return 1; }\n}\n}\n";
        let b = "#if X\nnamespace NZK {\npublic static partial class B {\n    public static int A2() { return 2; }\n}\n}\n#endif\n";
        let parts = json!([
            {"file": "a.cs", "line": 1, "endLine": 5},
            {"file": "b.cs", "line": 1, "endLine": 8},
        ]);
        let doc = build_doc(&[("a.cs", a), ("b.cs", b)], parts);
        let text = body_text(&doc);
        assert_eq!(text.matches("class B").count(), 2, "each context keeps its header");
        assert!(text.contains("#if X") && text.contains("#endif"));
    }
}
