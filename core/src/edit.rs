//! edit — the MCP-only refactor/query tools.
//!
//! These MCP-only tools are deliberately **not** part of the `/api/*` surface:
//! they bypass `mcp::resolve_call` / `dispatch_call` (and therefore `api.rs`)
//! and run directly against one [`Index`]. That keeps the frozen v1 route table
//! intact while giving an MCP client a small refactoring engine:
//!
//! | tool | kind |
//! |---|---|
//! | [`fuzzy_search`] | read — deterministic ranked fuzzy match |
//! | [`resolve_chain`] | read — resolve a symbol, then `Index::chain` |
//! | [`code_block`] | read — one compilable block from doc synthesis |
//! | [`rename_symbol`] | **write** (apply) — conservative token rename |
//! | [`qualify_symbols`] | **write** (apply) — fully qualify + drop usings |
//! | [`expand_namespace`] | **write** (apply) — normalise namespace headers |
//! | [`write_file`] | **write** (apply) — atomic create/overwrite |
//! | [`copy_path`] | **write** (apply) — repo-confined file/tree copy, all-or-nothing |
//! | [`apply_edits`] | **write** (apply) — exact-occurrence edits, all-or-nothing |
//! | [`select_decls`] | read — the `nzkcs -f like … mtype ?= …` selection half |
//! | [`move_decls`] | **write** (apply) — select -> move/re-namespace -> re-reference |
//! | [`grep`] | read — regex/text search over the indexed corpus, with counts |
//!
//! Every write tool defaults to a dry run: `apply` must be passed explicitly,
//! every path is re-checked to be inside the indexed repo, and writes go through
//! a sibling temp file plus `rename` so a crash never truncates a source file.
//! All ordering is deterministic (file, then line).
//!
//! `copy_path`'s durable journal is untrusted on-disk data. Recovery only acts
//! on entries whose recorded shape matches this engine's own artifacts (the
//! `tool`/`version`/`nonce`/`repo` markers plus the exact
//! `<target>.csgraph-bak-<nonce>` backup name), and for a target the copy
//! *created* it re-hashes the file before removing it. A journal crafted by
//! someone who can already write inside `<repo>/.csgraph-copy/` is still inside
//! the repo's own trust domain: such a writer can delete or replace in-repo
//! files directly, so recovery deliberately stops at refusal instead of
//! claiming to be a privilege boundary against them.
//!
//! No new crate dependencies: `serde_json` and the `regex`-free hand scanner
//! below.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::guard::WriteScope;
use crate::index::{sha256_hex, Index};
use crate::model;

/// The MCP-only tool names this module owns.
pub const TOOLS: [&str; 14] = [
    "fuzzy_search",
    "resolve_chain",
    "code_block",
    "rename_symbol",
    "qualify_symbols",
    "expand_namespace",
    "write_file",
    "apply_edits",
    "select_decls",
    "move_decls",
    "grep",
    "reformat",
    "functionize",
    "copy_path",
];

/// True for a tool name handled here instead of routed through `/api/*`.
pub fn is_edit_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

/// Tool names handled by [`invoke_aux`]: MCP-only, but deliberately **not** in
/// [`TOOLS`], so the app's `/api/edit` panel (which enumerates `TOOLS`) never
/// offers a non-edit operation such as the validity gate, the differential
/// compile, the dedupe report, the count summary or the workspace scope.
pub const AUX_TOOLS: [&str; 5] = [
    "parse_check",
    "compile_diff",
    "find_duplicates",
    "counts",
    "workspace",
];

/// True for an MCP-only tool that is not an edit tool.
pub fn is_aux_tool(name: &str) -> bool {
    AUX_TOOLS.contains(&name)
}

/// The standard MCP text envelope for one tool body: `isError` is set when the
/// body reports `"ok": false`, so an agent can tell a refusal (ambiguous rename,
/// illegal identifier, `skipped` validity gate, nothing indexed) from a payload
/// exactly like the routed tools.
fn envelope(body: Value) -> Value {
    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let text = serde_json::to_string(&body).unwrap_or_else(|_| "{\"ok\":false}".to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": !ok,
    })
}

/// Run one edit tool and wrap its body in the standard MCP text envelope.
///
/// `isError` is set when the body reports `"ok": false`, so an agent can tell a
/// refusal (ambiguous rename, illegal identifier, nothing indexed) from a
/// payload exactly like the routed tools.
pub fn invoke(index: &mut Index, name: &str, args: &Value) -> Value {
    let body = match name {
        "fuzzy_search" => fuzzy_search(index, args),
        "resolve_chain" => resolve_chain(index, args),
        "code_block" => code_block(index, args),
        "rename_symbol" => rename_symbol(index, args),
        "qualify_symbols" => qualify_symbols(index, args),
        "expand_namespace" => expand_namespace(index, args),
        "write_file" => write_file(index, args),
        "apply_edits" => apply_edits(index, args),
        "select_decls" => select_decls(index, args),
        "move_decls" => move_decls(index, args),
        "grep" => grep(index, args),
        "reformat" => crate::reshape::reformat(index, args),
        "functionize" => crate::reshape::functionize(index, args),
        "copy_path" => copy_path(index, args),
        other => json!({"ok": false, "error": format!("unknown tool: {other}")}),
    };
    envelope(body)
}

// ---------------------------------------------------------------------------
// argument helpers
// ---------------------------------------------------------------------------

fn required_str(args: &Value, key: &str) -> Result<String, String> {
    match args.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(format!("argument {key:?} must be a non-empty string")),
        Some(_) => Err(format!("argument {key:?} must be a string")),
        None => Err(format!("argument {key:?} is required")),
    }
}

pub(crate) fn sget<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

pub(crate) fn iget(args: &Value, key: &str, default: i64) -> i64 {
    args.get(key).and_then(Value::as_i64).unwrap_or(default)
}

pub(crate) fn bget(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// The explicit, loud opt-in that widens a write past the configured indexed
/// root(s). Denial is the default; a caller must spell this out per call.
/// See [`crate::guard::WriteScope::check_optin`].
pub(crate) fn allow_outside_root(args: &Value) -> bool {
    bget(args, "allow_outside_root", false) || bget(args, "allowOutsideRoot", false)
}

/// A `files`/`scope` argument: an array of strings or a comma-separated string.
pub(crate) fn string_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn not_indexed() -> Value {
    json!({"ok": false, "error": "nothing indexed"})
}

pub(crate) fn node_row(n: &Value) -> Value {
    json!({
        "id": n.get("id").cloned().unwrap_or_else(|| json!("")),
        "full": n.get("full").cloned().unwrap_or_else(|| json!("")),
        "kind": n.get("kind").cloned().unwrap_or_else(|| json!("")),
        "label": n.get("label").cloned().unwrap_or_else(|| json!("")),
        "name": n.get("name").cloned().unwrap_or_else(|| json!("")),
        "file": n.get("file").cloned().unwrap_or_else(|| json!("")),
        "line": n.get("line").cloned().unwrap_or_else(|| json!(0)),
        "endLine": n.get("endLine").cloned().unwrap_or_else(|| json!(0)),
    })
}

pub(crate) fn str_of<'a>(n: &'a Value, key: &str) -> &'a str {
    n.get(key).and_then(Value::as_str).unwrap_or("")
}

// ---------------------------------------------------------------------------
// 1. fuzzy_search
// ---------------------------------------------------------------------------

/// Ordinal tier: 0 exact full, 1 case-insensitive full, 2 exact simple,
/// 3 prefix, 4 subsequence.
///
/// `full_lc`/`label_lc`/`name_lc` are the pre-folded forms from
/// [`Index::node_lc`] and `ql` is the pre-folded query, so ranking a whole
/// corpus performs no case folding at all.
#[allow(clippy::too_many_arguments)]
fn match_tier(
    full: &str,
    label: &str,
    name: &str,
    full_lc: &str,
    label_lc: &str,
    name_lc: &str,
    q: &str,
    ql: &str,
) -> Option<(u8, &'static str)> {
    if full == q {
        return Some((0, "full"));
    }
    if full.eq_ignore_ascii_case(q) {
        return Some((1, "full-ci"));
    }
    let simple = full.rsplit('.').next().unwrap_or(full);
    if name == q || label == q || simple == q {
        return Some((2, "simple"));
    }
    if full_lc.starts_with(ql) || label_lc.starts_with(ql) || name_lc.starts_with(ql) {
        return Some((3, "prefix"));
    }
    if !ql.is_empty() && is_subsequence(full_lc, ql) {
        return Some((4, "subsequence"));
    }
    None
}

fn score_of(tier: u8) -> i64 {
    // Higher is better; the field is ordinal (it mirrors `match`).
    match tier {
        0 => 100,
        1 => 90,
        2 => 80,
        3 => 70,
        _ => 60,
    }
}

/// True when `needle` is a subsequence of `hay` (not necessarily contiguous).
///
/// C# identifiers and namespaces are ASCII, so the common case runs over bytes
/// — no `char` decoding, and `is_ascii` is word-at-a-time in std. The Unicode
/// path is kept for the rest, and the two agree: for ASCII text, byte equality
/// and `char` equality are the same relation.
fn is_subsequence(hay: &str, needle: &str) -> bool {
    if hay.is_ascii() && needle.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        let mut i = 0usize;
        for &c in n {
            while i < h.len() && h[i] != c {
                i += 1;
            }
            if i == h.len() {
                return false;
            }
            i += 1;
        }
        return true;
    }
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

fn fuzzy_search(index: &Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let q = match required_str(args, "q") {
        Ok(q) => q.trim().to_string(),
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let kind = sget(args, "kind");
    let limit = iget(args, "limit", 30).clamp(0, 1000) as usize;
    if q.is_empty() {
        return json!({"ok": true, "query": q, "results": []});
    }
    let ql = q.to_lowercase();
    // (tier, -degree, full, id, match-label, node index) — the row JSON is
    // built only for the survivors, so a query that matches hundreds of nodes
    // but asks for 30 does not serialise hundreds of rows.
    let mut scored: Vec<(u8, i64, String, String, &'static str, usize)> = Vec::new();
    for (i, n) in index.nodes().iter().enumerate() {
        let k = str_of(n, "kind");
        if let Some(want) = kind {
            if k != want {
                continue;
            }
        }
        let Some((_id_lc, full_lc, name_lc, label_lc)) = index.node_lc(i) else {
            continue;
        };
        let full = str_of(n, "full");
        let label = str_of(n, "label");
        let name = str_of(n, "name");
        let Some((tier, m)) = match_tier(full, label, name, full_lc, label_lc, name_lc, &q, &ql)
        else {
            continue;
        };
        let degree = n.get("degree").and_then(Value::as_i64).unwrap_or(0);
        scored.push((
            tier,
            -degree,
            full.to_string(),
            str_of(n, "id").to_string(),
            m,
            i,
        ));
    }
    // The sort key is (tier, -degree, full, id); the trailing fields are
    // carried, never compared, so the ordering is exactly as before.
    scored.sort_by(|a, b| (a.0, a.1, &a.2, &a.3).cmp(&(b.0, b.1, &b.2, &b.3)));
    let total = scored.len();
    let nodes = index.nodes();
    let results: Vec<Value> = scored
        .iter()
        .take(limit)
        .map(|x| {
            let mut row = node_row(&nodes[x.5]);
            if let Some(o) = row.as_object_mut() {
                o.insert("score".to_string(), json!(score_of(x.0)));
                o.insert("match".to_string(), json!(x.4));
            }
            row
        })
        .collect();
    json!({"ok": true, "query": q, "total": total, "results": results})
}

// ---------------------------------------------------------------------------
// 1b. select_decls
// ---------------------------------------------------------------------------

/// Case-insensitive lowercased patterns from `key`: an array of strings, or one
/// comma/whitespace separated string. Empty when the key is absent or empty.
fn patterns(args: &Value, key: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    match args.get(key) {
        Some(Value::Array(items)) => {
            for it in items {
                if let Some(s) = it.as_str() {
                    let s = s.trim();
                    if !s.is_empty() {
                        out.push(s.to_lowercase());
                    }
                }
            }
        }
        Some(Value::String(s)) => {
            for part in s.split([',', ' ']).filter(|p| !p.trim().is_empty()) {
                out.push(part.trim().to_lowercase());
            }
        }
        _ => {}
    }
    out
}

fn is_decl_kind(kind: &str) -> bool {
    TYPE_KINDS.contains(&kind) || MEMBER_KINDS.contains(&kind)
}

/// First line of the comment / attribute / conditional block immediately above
/// `line` (1-based), so a moved declaration carries its documentation with it.
pub(crate) fn leading_block_start(lines: &[&str], line: i64) -> i64 {
    let mut start = line.max(1);
    let mut i = line - 2;
    while i >= 0 {
        let t = lines[i as usize].trim();
        let block = t.is_empty()
            || t.starts_with("//")
            || t.starts_with("/*")
            || t.starts_with('*')
            || t.starts_with('[')
            || t.starts_with("#if")
            || t.starts_with("#else")
            || t.starts_with("#elif")
            || t.starts_with("#endif");
        if !block {
            break;
        }
        start = i + 1;
        i -= 1;
    }
    start
}

/// `select_decls {like?, mtype?, kind?, limit?}` — the selection half of the
/// owner's `nzkcs -f like [sps, pcs, socket, plug] mtype ?= [void, …]` move
/// command.
///
/// `like` is a case-insensitive **substring** match over a declaration's `name`
/// or `full` — deliberately not the subsequence match `fuzzy_search` performs,
/// which returns string literals for `socket` and unrelated methods for `plug`.
/// `mtype` filters on the node's `ret` (`void`, `System.String`, a generic `T`).
/// Read-only: it answers "what would move" and nothing else.
fn select_decls(index: &Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let like = patterns(args, "like");
    let mtype = patterns(args, "mtype");
    let kinds = patterns(args, "kind");
    let limit = iget(args, "limit", 200).clamp(0, 5000) as usize;
    let want_text = args.get("text").and_then(Value::as_bool).unwrap_or(false);
    if like.is_empty() && mtype.is_empty() {
        return json!({"ok": false,
                      "error": "select_decls needs at least one of `like` or `mtype`"});
    }
    // Split each source file at most once, and only when the caller asked for
    // the text, so a 200-row selection does not re-split megabytes per row.
    let mut lines_by_file: std::collections::HashMap<String, Vec<&str>> =
        std::collections::HashMap::new();
    let mut rows: Vec<Value> = Vec::new();
    for (i, n) in index.nodes().iter().enumerate() {
        let kind = str_of(n, "kind");
        if !is_decl_kind(kind) {
            continue;
        }
        if !kinds.is_empty() && !kinds.iter().any(|k| k == kind) {
            continue;
        }
        let Some((_id, full_lc, name_lc, _label)) = index.node_lc(i) else {
            continue;
        };
        if !like.is_empty() && !like.iter().any(|p| name_lc.contains(p) || full_lc.contains(p)) {
            continue;
        }
        let ret = str_of(n, "ret");
        if !mtype.is_empty() && !mtype.iter().any(|t| t == &ret.to_lowercase()) {
            continue;
        }
        let file = str_of(n, "file");
        let line = n.get("line").and_then(Value::as_i64).unwrap_or(0);
        let end_line = n.get("endLine").and_then(Value::as_i64).unwrap_or(0);
        let mut row = json!({
            "id": str_of(n, "id"),
            "full": str_of(n, "full"),
            "name": str_of(n, "name"),
            "kind": kind,
            "ret": ret,
            "file": file,
            "line": line,
            "endLine": end_line,
            "degree": n.get("degree").and_then(Value::as_i64).unwrap_or(0),
        });
        if want_text {
            if let Some(key) = canonical_file(index, file) {
                let lines = lines_by_file.entry(key.clone()).or_insert_with(|| {
                    index.file_text_of(&key).unwrap_or("").split('\n').collect()
                });
                let start = leading_block_start(lines, line);
                let last = if end_line > 0 { end_line } else { line };
                if start >= 1 && last >= start && (last as usize) <= lines.len() {
                    let text = lines[(start - 1) as usize..last as usize].join("\n");
                    if let Some(o) = row.as_object_mut() {
                        o.insert("blockStart".into(), json!(start));
                        o.insert("text".into(), json!(text));
                    }
                }
            }
        }
        rows.push(row);
    }
    rows.sort_by(|a, b| {
        let fa = a["file"].as_str().unwrap_or("");
        let fb = b["file"].as_str().unwrap_or("");
        let la = a["line"].as_i64().unwrap_or(0);
        let lb = b["line"].as_i64().unwrap_or(0);
        let na = a["full"].as_str().unwrap_or("");
        let nb = b["full"].as_str().unwrap_or("");
        (fa, la, na).cmp(&(fb, lb, nb))
    });
    let total = rows.len();
    let truncated = total > limit;
    rows.truncate(limit);
    json!({"ok": true, "like": like, "mtype": mtype, "kind": kinds,
           "total": total, "truncated": truncated, "results": rows})
}

// ---------------------------------------------------------------------------
// 2. resolve_chain
// ---------------------------------------------------------------------------

/// Resolve a node id, exact full/label/name, or a fuzzy query to exactly one
/// node. Returns the candidates when it is not unique.
fn resolve_to_one(index: &Index, q: &str) -> Result<(String, Value), Vec<Value>> {
    let q = q.trim();
    let mut exact: Vec<(String, Value)> = Vec::new();
    for n in index.nodes() {
        let id = str_of(n, "id");
        let full = str_of(n, "full");
        let name = str_of(n, "name");
        let label = str_of(n, "label");
        if id == q || full == q || name == q || label == q {
            exact.push((id.to_string(), node_row(n)));
        }
    }
    exact.dedup_by(|a, b| a.0 == b.0);
    if exact.len() == 1 {
        return Ok(exact.pop().unwrap());
    }
    if exact.len() > 1 {
        return Err(exact.into_iter().take(20).map(|x| x.1).collect());
    }
    // No exact hit: fall back to the ranked fuzzy search, but only accept a
    // single answer (or a single exact-full answer).
    let res = fuzzy_search(
        index,
        &json!({"q": q, "limit": 20}),
    );
    let results: Vec<Value> = res
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if results.len() == 1 {
        let r = results[0].clone();
        let id = r.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        return Ok((id, r));
    }
    let strong: Vec<Value> = results
        .iter()
        .filter(|r| matches!(r.get("match").and_then(Value::as_str), Some("full") | Some("full-ci")))
        .cloned()
        .collect();
    if strong.len() == 1 {
        let r = strong[0].clone();
        let id = r.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        return Ok((id, r));
    }
    Err(results)
}

fn resolve_chain(index: &Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let sym = match required_str(args, "symbol") {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let depth = iget(args, "depth", 3).clamp(1, 6);
    let max = iget(args, "max", 300).max(1);
    let dir_raw = sget(args, "dir").unwrap_or("forward");
    let dir = match dir_raw {
        "forward" => "forward",
        "reverse" => "backward",
        "backward" => "backward",
        "both" => "both",
        other => {
            return json!({"ok": false, "error": format!("invalid dir {other:?} (forward|reverse|both)")})
        }
    };
    let (id, resolved) = match resolve_to_one(index, &sym) {
        Ok(x) => x,
        Err(cands) => {
            return json!({
                "ok": false,
                "error": format!("{sym:?} does not resolve to exactly one node ({} candidates)", cands.len()),
                "candidates": cands,
            })
        }
    };
    let mut chain = index
        .chain(&id, depth, dir, max)
        .unwrap_or_else(|| json!({"ok": false, "error": "no such node"}));
    if chain.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        if let Some(o) = chain.as_object_mut() {
            o.insert("resolved".to_string(), resolved);
        }
    }
    chain
}

// ---------------------------------------------------------------------------
// 3. code_block
// ---------------------------------------------------------------------------

fn part_marker(node: &str, file: &str, line: i64, end: i64) -> String {
    let end = if end != 0 { end } else { line };
    format!("// \u{2500}\u{2500} {node} \u{b7} {file}:{line}-{end} \u{2500}\u{2500}")
}

fn code_block(index: &mut Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let mode = sget(args, "mode").unwrap_or("doc");
    if !matches!(mode, "doc" | "effective" | "raw") {
        return json!({"ok": false, "error": format!("invalid mode {mode:?} (doc|effective|raw)")});
    }
    let limit = iget(args, "limit", 8).clamp(1, 64) as usize;
    let path_only = bget(args, "path_only", false);

    let mut requested: Vec<String> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();
    if let Some(node) = sget(args, "node") {
        match index.resolve_node_id(node) {
            Some(id) => requested.push(id),
            None => unresolved.push(node.to_string()),
        }
    } else if let Some(arr) = args.get("nodes").and_then(Value::as_array) {
        for v in arr {
            if let Some(raw) = v.as_str() {
                match index.resolve_node_id(raw) {
                    Some(id) => requested.push(id),
                    None => unresolved.push(raw.to_string()),
                }
            }
        }
    } else if let Some(q) = sget(args, "q") {
        let r = fuzzy_search(index, &json!({"q": q, "limit": limit}));
        for row in r.get("results").and_then(Value::as_array).cloned().unwrap_or_default() {
            if let Some(id) = row.get("id").and_then(Value::as_str) {
                requested.push(id.to_string());
            }
        }
    } else {
        return json!({"ok": false, "error": "one of node, nodes or q is required"});
    }

    // Deterministic selection order, de-duplicated, capped by `limit`.
    let mut ids: Vec<String> = Vec::new();
    for id in requested {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids.truncate(limit);
    if ids.is_empty() {
        return json!({"ok": false, "error": "no node resolved", "unresolved": unresolved});
    }

    // Snapshot the node metadata first so the mutable `source_shared` calls below
    // do not hold a borrow of the index.
    let mut meta: Vec<(String, String, i64, i64)> = Vec::new();
    for id in &ids {
        let n = index.node_value(id);
        let file = n.map(|n| str_of(n, "file").to_string()).unwrap_or_default();
        let line = n.and_then(|n| n.get("line")).and_then(Value::as_i64).unwrap_or(0);
        let end = n.and_then(|n| n.get("endLine")).and_then(Value::as_i64).unwrap_or(0);
        meta.push((id.clone(), file, line, end));
    }

    let mut blocks: Vec<Vec<String>> = Vec::new();
    let mut used: Vec<String> = Vec::new();
    let mut parts: Vec<Value> = Vec::new();
    let mut files: BTreeSet<String> = BTreeSet::new();

    for (id, nfile, nline, nend) in &meta {
        let Some(payload) = index.source_shared(None, 1, 0, Some(id), None, mode) else {
            unresolved.push(id.clone());
            continue;
        };
        let mut block: Vec<String> = Vec::new();
        let mut has_marker = false;
        let mut block_parts: Vec<Value> = Vec::new();
        if let Some(lines) = payload.get("lines").and_then(Value::as_array) {
            for l in lines {
                if l.get("ctx").and_then(Value::as_str) == Some("part") {
                    has_marker = true;
                }
                if let Some(file) = l.get("file").and_then(Value::as_str) {
                    if !file.is_empty() {
                        files.insert(file.to_string());
                    }
                }
                block.push(l.get("text").and_then(Value::as_str).unwrap_or("").to_string());
            }
        }
        if let Some(ps) = payload.get("parts").and_then(Value::as_array) {
            for p in ps {
                let f = p.get("file").and_then(Value::as_str).unwrap_or("");
                let a = p.get("line").and_then(Value::as_i64).unwrap_or(0);
                let b = p.get("endLine").and_then(Value::as_i64).unwrap_or(a);
                block_parts.push(json!({"node": id, "file": f, "line": a, "endLine": b}));
                if !f.is_empty() {
                    files.insert(f.to_string());
                }
            }
        }
        if !has_marker {
            let (f, a, b) = match block_parts.first() {
                Some(p) => (
                    p.get("file").and_then(Value::as_str).unwrap_or(nfile),
                    p.get("line").and_then(Value::as_i64).unwrap_or(*nline),
                    p.get("endLine").and_then(Value::as_i64).unwrap_or(*nend),
                ),
                None => (nfile.as_str(), *nline, *nend),
            };
            if !f.is_empty() {
                block.insert(0, part_marker(id, f, a, b));
            }
        }
        if block.is_empty() {
            unresolved.push(id.clone());
            continue;
        }
        parts.extend(block_parts);
        used.push(id.clone());
        blocks.push(block);
    }

    if blocks.is_empty() {
        return json!({"ok": false, "error": "no code produced", "unresolved": unresolved});
    }

    let mut text = String::new();
    if !path_only {
        for (i, block) in blocks.iter().enumerate() {
            if i > 0 {
                text.push('\n');
            }
            text.push_str(&block.join("\n"));
            text.push('\n');
        }
    }
    let line_count = if path_only { 0 } else { text.lines().count() as i64 };
    json!({
        "ok": true,
        "nodes": used,
        "text": text,
        "lines": line_count,
        "parts": parts,
        "files": files.into_iter().collect::<Vec<String>>(),
        "mode": mode,
        "pathOnly": path_only,
        "unresolved": unresolved,
    })
}

// ---------------------------------------------------------------------------
// identifier scanning
// ---------------------------------------------------------------------------

pub(crate) const TYPE_KINDS: [&str; 5] = ["class", "struct", "interface", "enum", "record"];
pub(crate) const MEMBER_KINDS: [&str; 6] = [
    "method", "property", "field", "event", "operator", "constructor",
];
pub(crate) const DECL_KEYWORDS: [&str; 7] = [
    "class", "struct", "interface", "enum", "record", "namespace", "delegate",
];
const USAGE_KINDS: [&str; 5] = ["calls", "constructs", "uses", "assigns", "inherits"];

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_char(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

pub(crate) fn valid_cs_identifier(s: &str) -> bool {
    let mut it = s.chars();
    match it.next() {
        Some(c) if is_ident_start(c) => {}
        _ => return false,
    }
    if !it.all(is_ident_char) {
        return false;
    }
    !model::is_keyword(s)
}

pub(crate) fn valid_qualified_name(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    s.split('.').all(|seg| {
        let mut it = seg.chars();
        match it.next() {
            Some(c) if is_ident_start(c) => {}
            _ => return false,
        }
        it.all(is_ident_char)
    })
}

/// One frame of the hand scanner. `Code` carries whether it is inside an
/// interpolated-string hole and the brace depth of that hole, so `{ … }`
/// expressions inside `$"…"` are scanned as code (matching the spec's rule that
/// bare identifiers are rewritten, while literals and comments are not).
#[derive(Clone, Copy, PartialEq)]
enum ScanFrame {
    Code { hole: bool, depth: i32 },
    BlockComment,
    LineComment,
    Str { verbatim: bool, interp: bool },
    Char,
}

/// Replace identifiers on one line, leaving string/char literals and comments
/// untouched while still scanning interpolated-string holes as code.
/// `*in_block` carries `/* … */` state across lines. `f` is called with
/// `(token, preceded_by_dot, followed_by_dot, previous_identifier)` and returns
/// the replacement when the token should be rewritten.
pub(crate) fn transform_line<F>(line: &str, in_block: &mut bool, f: &mut F) -> String
where
    F: FnMut(&str, bool, bool, Option<&str>) -> Option<String>,
{
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0usize;
    let mut prev_ident: Option<String> = None;
    let mut stack: Vec<ScanFrame> = vec![ScanFrame::Code {
        hole: false,
        depth: 0,
    }];
    if *in_block {
        // Push the carried construct *on top of* the base `Code` frame instead
        // of replacing it: closing the block comment on this line pops back to
        // `Code`, so the stack can never become empty.
        stack.push(ScanFrame::BlockComment);
        *in_block = false;
    }
    while i < chars.len() {
        let c = chars[i];
        match stack.last().copied().unwrap_or(ScanFrame::Code {
            hole: false,
            depth: 0,
        }) {
            ScanFrame::LineComment => {
                out.push(c);
                i += 1;
            }
            ScanFrame::BlockComment => {
                out.push(c);
                if c == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                    out.push('/');
                    i += 2;
                    stack.pop();
                    continue;
                }
                i += 1;
            }
            ScanFrame::Char => {
                out.push(c);
                if c == '\\' {
                    if i + 1 < chars.len() {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                } else if c == '\'' {
                    stack.pop();
                }
                i += 1;
            }
            ScanFrame::Str { verbatim, interp } => {
                if !verbatim && c == '\\' {
                    out.push(c);
                    if i + 1 < chars.len() {
                        out.push(chars[i + 1]);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    continue;
                }
                if c == '"' {
                    if verbatim && i + 1 < chars.len() && chars[i + 1] == '"' {
                        out.push_str("\"\"");
                        i += 2;
                        continue;
                    }
                    out.push(c);
                    i += 1;
                    stack.pop();
                    continue;
                }
                if interp && c == '{' {
                    if i + 1 < chars.len() && chars[i + 1] == '{' {
                        out.push_str("{{");
                        i += 2;
                        continue;
                    }
                    out.push('{');
                    i += 1;
                    stack.push(ScanFrame::Code { hole: true, depth: 0 });
                    continue;
                }
                if interp && c == '}' {
                    if i + 1 < chars.len() && chars[i + 1] == '}' {
                        out.push_str("}}");
                        i += 2;
                        continue;
                    }
                    out.push('}');
                    i += 1;
                    continue;
                }
                out.push(c);
                i += 1;
            }
            ScanFrame::Code { hole, depth } => {
                if c == '/' && i + 1 < chars.len() {
                    if chars[i + 1] == '/' {
                        out.push_str("//");
                        i += 2;
                        stack.push(ScanFrame::LineComment);
                        continue;
                    }
                    if chars[i + 1] == '*' {
                        out.push_str("/*");
                        i += 2;
                        stack.push(ScanFrame::BlockComment);
                        continue;
                    }
                }
                if c == '"' {
                    let mut rev = out.chars().rev();
                    let prev = rev.next();
                    let prev2 = rev.next();
                    let (verbatim, interp) = match (prev, prev2) {
                        (Some('$'), Some('@')) => (true, true),
                        (Some('$'), _) => (false, true),
                        (Some('@'), Some('$')) => (true, true),
                        (Some('@'), _) => (true, false),
                        _ => (false, false),
                    };
                    out.push(c);
                    i += 1;
                    stack.push(ScanFrame::Str { verbatim, interp });
                    continue;
                }
                if c == '\'' {
                    out.push(c);
                    i += 1;
                    stack.push(ScanFrame::Char);
                    continue;
                }
                if hole {
                    if c == '{' {
                        if let Some(ScanFrame::Code { hole: true, depth: d }) = stack.last_mut() {
                            *d += 1;
                        }
                        out.push(c);
                        i += 1;
                        continue;
                    }
                    if c == '}' {
                        if depth == 0 {
                            out.push(c);
                            i += 1;
                            stack.pop();
                            continue;
                        }
                        if let Some(ScanFrame::Code { hole: true, depth: d }) = stack.last_mut() {
                            *d -= 1;
                        }
                        out.push(c);
                        i += 1;
                        continue;
                    }
                }
                if is_ident_start(c) {
                    let start = i;
                    while i < chars.len() && is_ident_char(chars[i]) {
                        i += 1;
                    }
                    let tok: String = chars[start..i].iter().collect();
                    let after_dot = out.chars().rev().find(|ch| !ch.is_whitespace()) == Some('.');
                    let followed_dot = chars[i..]
                        .iter()
                        .find(|ch| !ch.is_whitespace())
                        .map(|ch| *ch == '.')
                        .unwrap_or(false);
                    let rep = f(&tok, after_dot, followed_dot, prev_ident.as_deref());
                    out.push_str(rep.as_deref().unwrap_or(&tok));
                    prev_ident = Some(tok);
                    continue;
                }
                if !c.is_whitespace() {
                    prev_ident = None;
                }
                out.push(c);
                i += 1;
            }
        }
    }
    *in_block = matches!(stack.last(), Some(ScanFrame::BlockComment));
    out
}

/// For each line, whether that line *begins* inside a `/* … */` block comment.
pub(crate) fn block_comment_starts(text: &str) -> Vec<bool> {
    let mut flags = Vec::new();
    let mut in_block = false;
    for line in text.split('\n') {
        flags.push(in_block);
        // A cheap delimiter count is enough: strings are rare inside block
        // comments and C# has no nesting.
        let mut rest = line;
        loop {
            if in_block {
                match rest.find("*/") {
                    Some(i) => {
                        in_block = false;
                        rest = &rest[i + 2..];
                    }
                    None => break,
                }
            } else {
                match rest.find("/*") {
                    Some(i) => {
                        in_block = true;
                        rest = &rest[i + 2..];
                    }
                    None => break,
                }
            }
        }
    }
    flags
}

/// True when `line` contains `name` as a bare identifier (not a member access
/// and not inside a string/comment). Used to detect a stale graph whose line
/// numbers no longer point at the declaration it recorded.
fn line_mentions_ident(line: &str, in_block: bool, name: &str) -> bool {
    let mut ib = in_block;
    let mut hit = false;
    let _ = transform_line(line, &mut ib, &mut |tok, dot, _follow, _prev| {
        if !dot && tok == name {
            hit = true;
        }
        None
    });
    hit
}

/// One code token from [`code_tokens`]: identifiers carry `after_dot` (member
/// access), punctuation is its own token.
#[derive(Clone)]
pub(crate) struct Tok {
    pub(crate) text: String,
    pub(crate) ident: bool,
    pub(crate) after_dot: bool,
}

/// Tokenise one line of code, skipping strings/chars/comments (interpolated
/// string holes are code, matching [`transform_line`]). Punctuation is emitted
/// one character at a time, so `<`/`>`/`(`/`)`/`,`/`;`/`=`/`.` are visible to
/// the declaration scan.
pub(crate) fn code_tokens(line: &str, in_block: &mut bool) -> Vec<Tok> {
    let chars: Vec<char> = line.chars().collect();
    let mut out: Vec<Tok> = Vec::new();
    let mut i = 0usize;
    let mut stack: Vec<ScanFrame> = vec![ScanFrame::Code {
        hole: false,
        depth: 0,
    }];
    if *in_block {
        // Same invariant as [`transform_line`]: the carried block comment sits
        // on top of the base `Code` frame, so closing it on this line pops back
        // to `Code` and the stack can never become empty.
        stack.push(ScanFrame::BlockComment);
        *in_block = false;
    }
    while i < chars.len() {
        let c = chars[i];
        match stack.last().copied().unwrap_or(ScanFrame::Code {
            hole: false,
            depth: 0,
        }) {
            ScanFrame::LineComment => i += 1,
            ScanFrame::BlockComment => {
                if c == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                    i += 2;
                    stack.pop();
                } else {
                    i += 1;
                }
            }
            ScanFrame::Char => {
                if c == '\\' {
                    i += 2;
                } else {
                    if c == '\'' {
                        stack.pop();
                    }
                    i += 1;
                }
            }
            ScanFrame::Str { verbatim, interp } => {
                if !verbatim && c == '\\' {
                    i += 2;
                    continue;
                }
                if c == '"' {
                    if verbatim && i + 1 < chars.len() && chars[i + 1] == '"' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    stack.pop();
                    continue;
                }
                if interp && c == '{' {
                    if i + 1 < chars.len() && chars[i + 1] == '{' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    stack.push(ScanFrame::Code { hole: true, depth: 0 });
                    continue;
                }
                if interp && c == '}' {
                    if i + 1 < chars.len() && chars[i + 1] == '}' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    continue;
                }
                i += 1;
            }
            ScanFrame::Code { hole, depth } => {
                if c == '/' && i + 1 < chars.len() {
                    if chars[i + 1] == '/' {
                        i += 2;
                        stack.push(ScanFrame::LineComment);
                        continue;
                    }
                    if chars[i + 1] == '*' {
                        i += 2;
                        stack.push(ScanFrame::BlockComment);
                        continue;
                    }
                }
                if c == '"' {
                    let mut rev = out.iter().rev();
                    let prev = rev.next();
                    let prev2 = rev.next();
                    let (verbatim, interp) = match (
                        prev.map(|t| t.text.as_str()),
                        prev2.map(|t| t.text.as_str()),
                    ) {
                        (Some("$"), Some("@")) => (true, true),
                        (Some("$"), _) => (false, true),
                        (Some("@"), Some("$")) => (true, true),
                        (Some("@"), _) => (true, false),
                        _ => (false, false),
                    };
                    i += 1;
                    stack.push(ScanFrame::Str { verbatim, interp });
                    continue;
                }
                if c == '\'' {
                    i += 1;
                    stack.push(ScanFrame::Char);
                    continue;
                }
                if hole {
                    if c == '{' {
                        if let Some(ScanFrame::Code { hole: true, depth: d }) = stack.last_mut() {
                            *d += 1;
                        }
                        i += 1;
                        continue;
                    }
                    if c == '}' {
                        if depth == 0 {
                            i += 1;
                            stack.pop();
                            continue;
                        }
                        if let Some(ScanFrame::Code { hole: true, depth: d }) = stack.last_mut() {
                            *d -= 1;
                        }
                        i += 1;
                        continue;
                    }
                }
                if is_ident_start(c) {
                    let start = i;
                    while i < chars.len() && is_ident_char(chars[i]) {
                        i += 1;
                    }
                    let text: String = chars[start..i].iter().collect();
                    let after_dot = out
                        .last()
                        .map(|t: &Tok| !t.ident && t.text == ".")
                        .unwrap_or(false);
                    out.push(Tok {
                        text,
                        ident: true,
                        after_dot,
                    });
                    continue;
                }
                if !c.is_whitespace() {
                    out.push(Tok {
                        text: c.to_string(),
                        ident: false,
                        after_dot: false,
                    });
                }
                i += 1;
            }
        }
    }
    *in_block = matches!(stack.last(), Some(ScanFrame::BlockComment));
    out
}

/// Keywords that must never be read as the start of a type expression. The
/// predefined types (`string`, `int`, …) are *not* here — they are valid type
/// starts.
fn is_stop_keyword(s: &str) -> bool {
    model::is_keyword(s)
        || matches!(
            s,
            "var" | "return" | "if" | "else" | "for" | "foreach" | "while" | "switch" | "case"
                | "new" | "throw" | "using" | "lock" | "catch" | "try" | "do" | "in" | "is"
                | "as" | "out" | "ref" | "params" | "this" | "base" | "true" | "false" | "null"
                | "void" | "await" | "yield" | "get" | "set" | "add" | "remove" | "where"
                | "select" | "from" | "let" | "when" | "default" | "checked" | "unchecked"
                | "fixed" | "unsafe" | "stackalloc"
        )
}

/// True when `s` can start a declared type expression: a predefined type or a
/// plain (non-keyword) identifier.
fn is_type_start(s: &str) -> bool {
    matches!(
        s,
        "bool" | "byte" | "sbyte" | "char" | "decimal" | "double" | "float" | "int" | "uint"
            | "long" | "ulong" | "short" | "ushort" | "string" | "object" | "dynamic"
    ) || !is_stop_keyword(s)
}

/// The identifiers **declared inside** a moved slice. The dependency qualifier
/// must never rewrite any of these: the declaration's own name, generic type
/// parameters, parameter names, locals (`var`/`for`/`foreach`/`using`/`catch`,
/// typed declarations and lambda parameters), and any name introduced right
/// after a declaration keyword (`class B`, `namespace N`, …).
pub(crate) fn declared_names(tokens: &[Tok], own: &str) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    let is_punct = |t: &Tok, s: &str| !t.ident && t.text == s;

    // `class B`, `struct T`, `namespace N`, … the introduced name is declared.
    for i in 0..tokens.len() {
        if tokens[i].ident && DECL_KEYWORDS.contains(&tokens[i].text.as_str()) {
            if let Some(n) = tokens.get(i + 1) {
                if n.ident && !n.after_dot {
                    out.insert(n.text.clone());
                }
            }
        }
    }

    // Generic parameters of the declaration itself: `Eq<T>` / `class Foo<T>`.
    if let Some(pi) = tokens
        .iter()
        .position(|t| t.ident && t.text == own && !t.after_dot)
    {
        let mut j = pi + 1;
        if j < tokens.len() && is_punct(&tokens[j], "<") {
            let mut depth = 0i32;
            while j < tokens.len() {
                let t = &tokens[j];
                if is_punct(t, "<") {
                    depth += 1;
                } else if is_punct(t, ">") {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                } else if t.ident && depth >= 1 && !t.after_dot {
                    out.insert(t.text.clone());
                }
                j += 1;
            }
        }
    }

    for i in 0..tokens.len() {
        // `var x` declares x.
        if tokens[i].ident && tokens[i].text == "var" {
            if let Some(n) = tokens.get(i + 1) {
                if n.ident && !n.after_dot {
                    out.insert(n.text.clone());
                }
            }
        }
        // `x => …` lambda parameters.
        if tokens[i].ident
            && !tokens[i].after_dot
            && !is_stop_keyword(&tokens[i].text)
            && i + 2 < tokens.len()
            && is_punct(&tokens[i + 1], "=")
            && is_punct(&tokens[i + 2], ">")
        {
            out.insert(tokens[i].text.clone());
        }
    }

    // `<type-expression> <name> <terminator>` — parameters, locals, foreach and
    // catch variables. The type expression is a dotted, optionally generic name.
    let mut i = 0usize;
    while i < tokens.len() {
        let t = &tokens[i];
        if !(t.ident && !t.after_dot && is_type_start(&t.text)) {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        loop {
            if j < tokens.len()
                && is_punct(&tokens[j], ".")
                && tokens.get(j + 1).map(|x| x.ident).unwrap_or(false)
            {
                j += 2;
                continue;
            }
            if j < tokens.len() && is_punct(&tokens[j], "<") {
                let mut depth = 0i32;
                let mut k = j;
                while k < tokens.len() {
                    if is_punct(&tokens[k], "<") {
                        depth += 1;
                    } else if is_punct(&tokens[k], ">") {
                        depth -= 1;
                        if depth == 0 {
                            k += 1;
                            break;
                        }
                    }
                    k += 1;
                }
                if depth == 0 {
                    j = k;
                    continue;
                }
            }
            break;
        }
        if j < tokens.len()
            && tokens[j].ident
            && !tokens[j].after_dot
            && !model::is_keyword(&tokens[j].text)
        {
            let term = tokens.get(j + 1);
            let ok = match term {
                Some(x) if x.ident => x.text == "in",
                Some(x) => matches!(x.text.as_str(), "=" | "," | ";" | ")" | "[" | "]"),
                None => true,
            };
            if ok {
                out.insert(tokens[j].text.clone());
            }
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// path + write helpers
// ---------------------------------------------------------------------------
/// Resolve a display path to an absolute path inside the indexed repo, or
/// `None` when it escapes the repo (or does not exist). This is the `apply`
/// guard: an outside path is refused, never written.
fn repo_abs(index: &Index, file: &str) -> Option<PathBuf> {
    let root = index.repo_path();
    let root_c = root.canonicalize().ok()?;
    let candidate = root.join(file);
    let canon = candidate.canonicalize().ok()?;
    if canon.starts_with(&root_c) {
        Some(canon)
    } else {
        None
    }
}

/// The bytes currently on disk for an indexed file (falling back to the
/// in-memory build when the read fails).
pub(crate) fn disk_text(index: &Index, file: &str) -> Option<String> {
    if let Some(abs) = repo_abs(index, file) {
        if let Ok(t) = fs::read_to_string(&abs) {
            return Some(t);
        }
    }
    index.file_text_of(file).map(str::to_string)
}

fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let tmp = dir.join(format!(".{name}.csgraph-tmp-{}", std::process::id()));
    if let Err(e) = fs::write(&tmp, text) {
        // A failed write must not leave the staging temp behind.
        let _ = fs::remove_file(&tmp);
        return Err(format!("write {}: {e}", tmp.display()));
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(format!("rename {}: {e}", path.display()))
        }
    }
}

/// One planned line operation: `after: None` deletes the line.
struct Op {
    before: Option<String>,
    after: Option<String>,
}

type LineOps = BTreeMap<String, BTreeMap<i64, Op>>;

/// Apply line operations to every affected file, reading the current bytes from
/// disk. When `verify_before` is set a line whose on-disk text no longer equals
/// the planned `before` is left untouched and reported as stale.
fn apply_ops(
    index: &Index,
    ops: &LineOps,
    verify_before: bool,
    allow_outside_root: bool,
) -> Result<(Vec<String>, Vec<Value>), String> {
    let scope = WriteScope::for_index(index);
    let mut written: Vec<String> = Vec::new();
    let mut stale: Vec<Value> = Vec::new();
    for (file, file_ops) in ops {
        let abs = repo_abs(index, file)
            .ok_or_else(|| format!("refusing to write outside the indexed repo: {file}"))?;
        scope.check_optin(&abs, allow_outside_root)?;
        let text = fs::read_to_string(&abs).map_err(|e| format!("read {}: {e}", abs.display()))?;
        let src: Vec<String> = text.split('\n').map(str::to_string).collect();
        let mut out: Vec<String> = Vec::with_capacity(src.len());
        for (idx, line) in src.iter().enumerate() {
            let ln = idx as i64 + 1;
            match file_ops.get(&ln) {
                Some(op) => {
                    if verify_before {
                        if let Some(b) = &op.before {
                            if b != line {
                                stale.push(json!({"file": file, "line": ln, "reason": "line changed since the plan"}));
                                out.push(line.clone());
                                continue;
                            }
                        }
                    }
                    match &op.after {
                        Some(a) => out.push(a.clone()),
                        None => {}
                    }
                }
                None => out.push(line.clone()),
            }
        }
        write_atomic(&abs, &out.join("\n"))?;
        written.push(file.clone());
    }
    Ok((written, stale))
}

fn in_scope(scopes: &[String], file: &str) -> bool {
    if scopes.is_empty() {
        return true;
    }
    scopes.iter().any(|s| {
        let s = s.trim_end_matches('/');
        file == s || file.starts_with(&format!("{s}/"))
    })
}

/// Map an absolute file/dir argument back onto a display path, if it is indexed.
pub(crate) fn canonical_file(index: &Index, raw: &str) -> Option<String> {
    if index.file_text_of(raw).is_some() {
        return Some(raw.to_string());
    }
    let p = Path::new(raw);
    if p.is_absolute() {
        let root = index.repo_path();
        if let Ok(rel) = p.strip_prefix(root) {
            let rel = rel.to_string_lossy().replace('\\', "/");
            if index.file_text_of(&rel).is_some() {
                return Some(rel);
            }
        }
    }
    None
}

/// Normalise a `scope` entry to a repo-relative prefix when it is a path inside
/// the repo; otherwise keep it verbatim.
fn normalise_scope(index: &Index, raw: &str) -> String {
    let p = Path::new(raw);
    if p.is_absolute() {
        let root = index.repo_path();
        // A directory prefix may not canonicalise as a file; strip textually.
        if let Ok(rel) = p.strip_prefix(root) {
            return rel.to_string_lossy().replace('\\', "/");
        }
    }
    raw.trim_end_matches('/').to_string()
}

// ---------------------------------------------------------------------------
// 4. rename_symbol
// ---------------------------------------------------------------------------

/// Declaration candidates for `q`, in priority order: exact id, exact full,
/// exact simple name, then the ranked fuzzy search. Returns a single id when
/// the answer is unique.
pub(crate) fn resolve_declaration(index: &Index, q: &str) -> (Option<String>, Vec<Value>) {
    let q = q.trim();
    if index.node_value(q).is_some() && index.is_declaration_id(q) {
        return (Some(q.to_string()), Vec::new());
    }
    let pick = |mut rows: Vec<Value>| -> (Option<String>, Vec<Value>) {
        rows.dedup_by(|a, b| a.get("id") == b.get("id"));
        if rows.len() == 1 {
            let id = rows[0]
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (Some(id), Vec::new())
        } else {
            (None, rows.into_iter().take(20).collect())
        }
    };
    let by = |key: &str| -> Vec<Value> {
        index
            .nodes()
            .iter()
            .filter(|n| {
                index.is_declaration_id(str_of(n, "id")) && str_of(n, key) == q
            })
            .map(node_row)
            .collect()
    };
    for key in ["full", "name", "label"] {
        let rows = by(key);
        if !rows.is_empty() {
            return pick(rows);
        }
    }
    // Fuzzy fallback, declarations only.
    let res = fuzzy_search(index, &json!({"q": q, "limit": 25}));
    let rows: Vec<Value> = res
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            r.get("id")
                .and_then(Value::as_str)
                .map(|id| index.is_declaration_id(id))
                .unwrap_or(false)
        })
        .collect();
    pick(rows)
}

pub(crate) fn decl_slices(node: &Value) -> Vec<(String, i64, i64)> {
    let mut out: Vec<(String, i64, i64)> = Vec::new();
    if let Some(parts) = node.get("parts").and_then(Value::as_array) {
        for p in parts {
            let f = p.get("file").and_then(Value::as_str).unwrap_or("");
            if f.is_empty() {
                continue;
            }
            let a = p.get("line").and_then(Value::as_i64).unwrap_or(0).max(1);
            let b = p.get("endLine").and_then(Value::as_i64).unwrap_or(a).max(a);
            out.push((f.to_string(), a, b));
        }
    }
    if out.is_empty() {
        let f = str_of(node, "file");
        if !f.is_empty() {
            let a = node.get("line").and_then(Value::as_i64).unwrap_or(1).max(1);
            let b = node.get("endLine").and_then(Value::as_i64).unwrap_or(a).max(a);
            out.push((f.to_string(), a, b));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn rename_symbol(index: &mut Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let from = match required_str(args, "from") {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let to = match required_str(args, "to") {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    if !valid_cs_identifier(&to) {
        return json!({"ok": false, "error": format!("{to:?} is not a legal C# identifier")});
    }
    let apply = bget(args, "apply", false);
    let include_declarations = bget(args, "include_declarations", true);
    let scopes: Vec<String> = string_list(args, "scope")
        .iter()
        .map(|s| normalise_scope(index, s))
        .collect();

    let (unique, candidates) = resolve_declaration(index, &from);
    let Some(id) = unique else {
        return json!({
            "ok": false,
            "error": format!("{from:?} does not resolve to one declaration"),
            "ambiguity": candidates,
        });
    };
    let node = match index.node_value(&id).cloned() {
        Some(n) => n,
        None => return json!({"ok": false, "error": format!("no such node: {id}")}),
    };
    let kind = str_of(&node, "kind");
    if matches!(kind, "namespace" | "external" | "string") {
        return json!({
            "ok": false,
            "error": format!("{id} is a {kind} node; only code declarations can be renamed"),
        });
    }
    let name = str_of(&node, "name").to_string();
    if name.is_empty() {
        return json!({"ok": false, "error": format!("{id} has no simple name to rename")});
    }

    // (file, line) -> declaration line (declaration) / usage line.
    let mut sites: BTreeMap<(String, i64), bool> = BTreeMap::new();
    if include_declarations {
        for (file, a, b) in decl_slices(&node) {
            for line in a..=b {
                sites.insert((file.clone(), line), true);
            }
        }
    }
    // Incoming edges only: an edge into this node records sites of *this*
    // symbol in the referencing declaration. Skip structural `contains`.
    for e in index.edges_of(&id) {
        let target = e.get("target").and_then(Value::as_str).unwrap_or("");
        let source = e.get("source").and_then(Value::as_str).unwrap_or("");
        if target != id || source == id {
            continue;
        }
        let kind = e.get("kind").and_then(Value::as_str).unwrap_or("");
        if !USAGE_KINDS.contains(&kind) {
            continue;
        }
        let Some(src) = index.node_value(source) else {
            continue;
        };
        let file = str_of(src, "file").to_string();
        if file.is_empty() {
            continue;
        }
        let lines: Vec<i64> = e
            .get("lines")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        if lines.is_empty() {
            let l = src
                .get("line")
                .and_then(Value::as_i64)
                .unwrap_or(1)
                .max(1);
            sites.insert((file, l), false);
        } else {
            for l in lines {
                sites.insert((file.clone(), l), false);
            }
        }
    }

    // Plan per file, computing before/after from the bytes on disk.
    let mut ops: LineOps = BTreeMap::new();
    let mut files_planned: Vec<String> = Vec::new();
    let mut skipped_outside: Vec<String> = Vec::new();
    let mut truncated = false;
    let cap: usize = 20_000;
    let mut count = 0usize;
    let mut cache: BTreeMap<String, (Vec<String>, Vec<bool>)> = BTreeMap::new();
    for ((file, line), is_decl) in &sites {
        if !in_scope(&scopes, file) {
            continue;
        }
        if count >= cap {
            truncated = true;
            break;
        }
        if !cache.contains_key(file) {
            let Some(text) = disk_text(index, file) else {
                skipped_outside.push(file.clone());
                continue;
            };
            let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
            let flags = block_comment_starts(&text);
            cache.insert(file.clone(), (lines, flags));
        }
        let (lines, flags) = cache.get(file).unwrap();
        if *line < 1 || (*line as usize) > lines.len() {
            continue;
        }
        let orig = &lines[*line as usize - 1];
        let mut in_block = flags.get(*line as usize - 1).copied().unwrap_or(false);
        let new_text = transform_line(orig, &mut in_block, &mut |tok, _dot, _follow, _prev| {
            if tok == name {
                Some(to.clone())
            } else {
                None
            }
        });
        if new_text == *orig {
            continue;
        }
        if !files_planned.contains(file) {
            files_planned.push(file.clone());
        }
        ops.entry(file.clone())
            .or_default()
            .insert(
                *line,
                Op {
                    before: Some(orig.clone()),
                    after: Some(new_text),
                },
            );
        count += 1;
        let _ = is_decl;
    }

    let mut edits: Vec<Value> = Vec::new();
    for (file, file_ops) in &ops {
        for (line, op) in file_ops {
            edits.push(json!({
                "file": file,
                "line": line,
                "before": op.before.clone().unwrap_or_default(),
                "after": op.after.clone().unwrap_or_default(),
            }));
        }
    }
    files_planned.sort();
    edits.sort_by(|a, b| {
        let fa = a.get("file").and_then(Value::as_str).unwrap_or("");
        let fb = b.get("file").and_then(Value::as_str).unwrap_or("");
        let la = a.get("line").and_then(Value::as_i64).unwrap_or(0);
        let lb = b.get("line").and_then(Value::as_i64).unwrap_or(0);
        (fa, la).cmp(&(fb, lb))
    });

    let mut applied = false;
    let mut written: Vec<String> = Vec::new();
    let mut stale: Vec<Value> = Vec::new();
    let mut write_error: Option<String> = None;
    if apply && !ops.is_empty() {
        match apply_ops(index, &ops, true, allow_outside_root(args)) {
            Ok((w, s)) => {
                applied = true;
                written = w;
                stale = s;
            }
            Err(e) => write_error = Some(e),
        }
    } else if apply {
        applied = true;
    }

    let mut body = json!({
        "ok": write_error.is_none(),
        "from_node": node_row(&node),
        "to": to,
        "ambiguity": [],
        "files_planned": files_planned,
        "edits": edits,
        "applied": applied,
        "written": written,
        "truncated": truncated,
        "skipped": skipped_outside,
    });
    if !stale.is_empty() {
        body["stale"] = json!(stale);
    }
    if let Some(e) = write_error {
        body["error"] = json!(e);
    }
    body
}

// ---------------------------------------------------------------------------
// 5. qualify_symbols
// ---------------------------------------------------------------------------

pub(crate) fn using_directive(line: &str) -> bool {
    let t = line.trim();
    let t = t.strip_prefix("global ").map(str::trim_start).unwrap_or(t);
    if !t.starts_with("using ") && !t.starts_with("using\t") {
        return false;
    }
    if t.starts_with("using (") || t.starts_with("using(") {
        return false;
    }
    t.ends_with(';')
}

/// simple name -> unique full name for every indexed declaration that can be
/// named without a receiver: types (class/struct/interface/enum/record/delegate)
/// and **static** members (method/property/field/event/operator). An instance
/// member cannot be fully qualified from a bare identifier, so it is excluded
/// rather than rewritten into invalid C#.
pub(crate) fn qualify_map(index: &Index) -> (BTreeMap<String, String>, Vec<Value>) {
    let mut by_simple: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for n in index.nodes() {
        let kind = str_of(n, "kind");
        let is_type = TYPE_KINDS.contains(&kind) || kind == "delegate";
        let is_static_member = MEMBER_KINDS.contains(&kind)
            && n.get("static").and_then(Value::as_bool).unwrap_or(false);
        if !is_type && !is_static_member {
            continue;
        }
        let id = str_of(n, "id");
        if !index.is_declaration_id(id) {
            continue;
        }
        let name = str_of(n, "name");
        let full = str_of(n, "full");
        if name.is_empty()
            || full.is_empty()
            || name == full
            || !valid_cs_identifier(name)
            || !valid_qualified_name(full)
        {
            continue;
        }
        by_simple
            .entry(name.to_string())
            .or_default()
            .insert(full.to_string());
    }
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    let mut ambiguous: Vec<Value> = Vec::new();
    for (name, fulls) in by_simple {
        if fulls.len() == 1 {
            map.insert(name, fulls.into_iter().next().unwrap());
        } else {
            ambiguous.push(json!({
                "name": name,
                "candidates": fulls.into_iter().collect::<Vec<String>>(),
            }));
        }
    }
    ambiguous.truncate(200);
    (map, ambiguous)
}

fn qualify_symbols(index: &mut Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let remove_usings = bget(args, "remove_usings", true);
    let apply = bget(args, "apply", false);
    let max_edits = iget(args, "max_edits", 2000).clamp(0, 100_000) as usize;
    let files: Vec<String> = {
        let listed = string_list(args, "files");
        if listed.is_empty() {
            index.file_paths()
        } else {
            listed
                .iter()
                .filter_map(|f| canonical_file(index, f))
                .collect()
        }
    };
    let (names, skipped_ambiguous) = qualify_map(index);

    let mut ops: LineOps = BTreeMap::new();
    let mut edits: Vec<Value> = Vec::new();
    let mut remove_usings_rows: Vec<Value> = Vec::new();
    let mut files_planned: Vec<String> = Vec::new();
    let mut truncated = false;
    let mut budget = max_edits;

    for file in &files {
        let Some(text) = disk_text(index, file) else {
            continue;
        };
        let src: Vec<String> = text.split('\n').map(str::to_string).collect();
        let flags = block_comment_starts(&text);
        // Names declared in this file, keyed by the declaration's start line:
        // the declared identifier must never be qualified in its own header.
        let mut protected: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
        for id in index.node_ids_of_file(file) {
            if !index.is_declaration_id(&id) {
                continue;
            }
            if let Some(n) = index.node_value(&id) {
                let name = str_of(n, "name");
                if name.is_empty() {
                    continue;
                }
                let ln = n.get("line").and_then(Value::as_i64).unwrap_or(0).max(1);
                protected.entry(ln).or_default().insert(name.to_string());
            }
        }
        for (idx, raw) in src.iter().enumerate() {
            let line = idx as i64 + 1;
            if remove_usings && using_directive(raw) {
                if budget == 0 {
                    truncated = true;
                    break;
                }
                budget -= 1;
                if !files_planned.contains(file) {
                    files_planned.push(file.clone());
                }
                remove_usings_rows.push(json!({"file": file, "line": line, "text": raw}));
                edits.push(json!({"file": file, "line": line, "before": raw, "after": ""}));
                ops.entry(file.clone()).or_default().insert(
                    line,
                    Op {
                        before: Some(raw.clone()),
                        after: None,
                    },
                );
                continue;
            }
            let prot = protected.get(&line);
            let mut in_block = flags.get(idx).copied().unwrap_or(false);
            let new_text =
                transform_line(raw, &mut in_block, &mut |tok, after_dot, followed_dot, prev| {
                    // Already (partly) qualified, or the declared name itself.
                    if after_dot || followed_dot {
                        return None;
                    }
                    if let Some(ps) = prot {
                        if ps.contains(tok) {
                            return None;
                        }
                    }
                    if prev.map(|p| DECL_KEYWORDS.contains(&p)).unwrap_or(false) {
                        return None;
                    }
                    names.get(tok).cloned()
                });
            if new_text == *raw {
                continue;
            }
            if budget == 0 {
                truncated = true;
                break;
            }
            budget -= 1;
            if !files_planned.contains(file) {
                files_planned.push(file.clone());
            }
            edits.push(json!({"file": file, "line": line, "before": raw, "after": new_text}));
            ops.entry(file.clone()).or_default().insert(
                line,
                Op {
                    before: Some(raw.clone()),
                    after: Some(new_text),
                },
            );
        }
    }

    files_planned.sort();
    edits.sort_by(|a, b| {
        let fa = a.get("file").and_then(Value::as_str).unwrap_or("");
        let fb = b.get("file").and_then(Value::as_str).unwrap_or("");
        let la = a.get("line").and_then(Value::as_i64).unwrap_or(0);
        let lb = b.get("line").and_then(Value::as_i64).unwrap_or(0);
        (fa, la).cmp(&(fb, lb))
    });
    remove_usings_rows.sort_by(|a, b| {
        let fa = a.get("file").and_then(Value::as_str).unwrap_or("");
        let fb = b.get("file").and_then(Value::as_str).unwrap_or("");
        let la = a.get("line").and_then(Value::as_i64).unwrap_or(0);
        let lb = b.get("line").and_then(Value::as_i64).unwrap_or(0);
        (fa, la).cmp(&(fb, lb))
    });

    let mut applied = false;
    let mut written: Vec<String> = Vec::new();
    let mut stale: Vec<Value> = Vec::new();
    let mut write_error: Option<String> = None;
    if apply && !ops.is_empty() {
        match apply_ops(index, &ops, true, allow_outside_root(args)) {
            Ok((w, s)) => {
                applied = true;
                written = w;
                stale = s;
            }
            Err(e) => write_error = Some(e),
        }
    } else if apply {
        applied = true;
    }

    let mut body = json!({
        "ok": write_error.is_none(),
        "files_planned": files_planned,
        "remove_usings": remove_usings_rows,
        "edits": edits,
        "skipped_ambiguous": skipped_ambiguous,
        "applied": applied,
        "written": written,
        "truncated": truncated,
    });
    if !stale.is_empty() {
        body["stale"] = json!(stale);
    }
    if let Some(e) = write_error {
        body["error"] = json!(e);
    }
    body
}

// ---------------------------------------------------------------------------
// 6. expand_namespace
// ---------------------------------------------------------------------------

/// The namespace a `namespace X {` / `namespace X;` line declares, if any.
fn namespace_header(line: &str) -> Option<(String, usize, usize)> {
    let t = line.trim_start();
    let rest = t.strip_prefix("namespace ")?;
    let offset = line.len() - t.len() + "namespace ".len();
    let mut end = 0usize;
    for (i, c) in rest.char_indices() {
        if c.is_whitespace() || c == '{' || c == ';' {
            end = i;
            break;
        }
        end = i + c.len_utf8();
    }
    if end == 0 {
        return None;
    }
    let name = &rest[..end];
    if !valid_qualified_name(name) {
        return None;
    }
    Some((name.to_string(), offset, offset + end))
}

fn expand_namespace(index: &mut Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let apply = bget(args, "apply", false);

    // With a destination this is a thin delegate onto the existing reorg write
    // path, grouped by namespace. That path already owns its own destination
    // and overwrite semantics; the central scope is applied here too, because
    // this route carries no `Index` inside `Reorg::write_tree`.
    if let Some(dest) = sget(args, "dest") {
        if let Err(e) = WriteScope::for_index(index)
            .check_optin(Path::new(dest), allow_outside_root(args))
        {
            return json!({
                "ok": false,
                "delegated": true,
                "dest": dest,
                "applied": false,
                "error": e,
            });
        }
        let spec = json!({"group": "namespace"});
        let result = index.reorg(Some(&spec), "write", Some(dest), true);
        let ok = result.get("ok").and_then(Value::as_bool).unwrap_or(false);
        return json!({
            "ok": ok,
            "delegated": true,
            "dest": dest,
            "result": result,
            "applied": true,
        });
    }

    let namespace = sget(args, "namespace").map(str::to_string);
    let listed = string_list(args, "files");
    let files: Vec<String> = if listed.is_empty() {
        index.file_paths()
    } else {
        listed
            .iter()
            .filter_map(|f| canonical_file(index, f))
            .collect()
    };

    let mut ops: LineOps = BTreeMap::new();
    let mut planned: Vec<Value> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    for file in &files {
        // Which namespace does the graph record for this file's declarations?
        let mut recorded: BTreeSet<String> = BTreeSet::new();
        for id in index.node_ids_of_file(file) {
            if !index.is_declaration_id(&id) {
                continue;
            }
            if let Some(n) = index.node_value(&id) {
                let ns = str_of(n, "ns");
                if !ns.is_empty() {
                    recorded.insert(ns.to_string());
                }
            }
        }
        if recorded.len() != 1 {
            skipped.push(json!({
                "file": file,
                "reason": if recorded.is_empty() { "no namespaced declarations" }
                          else { "declarations span multiple namespaces" },
            }));
            continue;
        }
        let want = recorded.into_iter().next().unwrap();
        if let Some(filter) = &namespace {
            if &want != filter {
                skipped.push(json!({"file": file,
                    "reason": format!("graph namespace {want:?} is not the requested {filter:?}")}));
                continue;
            }
        }
        let Some(text) = disk_text(index, file) else {
            skipped.push(json!({"file": file, "reason": "unreadable"}));
            continue;
        };
        let mut headers: Vec<(i64, String, String, String)> = Vec::new();
        for (idx, raw) in text.split('\n').enumerate() {
            if let Some((declared, _, _)) = namespace_header(raw) {
                headers.push((idx as i64 + 1, raw.to_string(), declared, String::new()));
            }
        }
        if headers.len() != 1 {
            skipped.push(json!({"file": file,
                "reason": format!("expected exactly one namespace header, found {}", headers.len())}));
            continue;
        }
        let (line, before, declared, _) = headers.remove(0);
        if declared == want {
            continue;
        }
        let after = before.replacen(&declared, &want, 1);
        planned.push(json!({
            "file": file,
            "line": line,
            "declared": declared,
            "namespace": want,
            "before": before,
            "after": after,
        }));
        ops.entry(file.clone()).or_default().insert(
            line,
            Op {
                before: Some(before),
                after: Some(after),
            },
        );
    }

    planned.sort_by(|a, b| {
        let fa = a.get("file").and_then(Value::as_str).unwrap_or("");
        let fb = b.get("file").and_then(Value::as_str).unwrap_or("");
        let la = a.get("line").and_then(Value::as_i64).unwrap_or(0);
        let lb = b.get("line").and_then(Value::as_i64).unwrap_or(0);
        (fa, la).cmp(&(fb, lb))
    });

    let mut applied = false;
    let mut written: Vec<String> = Vec::new();
    let mut stale: Vec<Value> = Vec::new();
    let mut write_error: Option<String> = None;
    if apply && !ops.is_empty() {
        match apply_ops(index, &ops, true, allow_outside_root(args)) {
            Ok((w, s)) => {
                applied = true;
                written = w;
                stale = s;
            }
            Err(e) => write_error = Some(e),
        }
    } else if apply {
        applied = true;
    }

    let files_planned: Vec<String> = planned
        .iter()
        .filter_map(|p| p.get("file").and_then(Value::as_str).map(str::to_string))
        .collect();
    let mut body = json!({
        "ok": write_error.is_none(),
        "delegated": false,
        "files_planned": files_planned,
        "planned": planned,
        "skipped": skipped,
        "applied": applied,
        "written": written,
    });
    if !stale.is_empty() {
        body["stale"] = json!(stale);
    }
    if let Some(e) = write_error {
        body["error"] = json!(e);
    }
    body
}

// ---------------------------------------------------------------------------
// 7. write_file
// ---------------------------------------------------------------------------

/// Resolve a caller-supplied path to an absolute path confined to the indexed
/// repo. The parent directory must exist and canonicalise inside the root, and
/// an existing symlink at the target is refused outright (no symlink escape).
///
/// The central [`WriteScope`] guard runs first: a destination outside the
/// indexed root(s) is refused unless `allow_outside_root`, and a destination
/// inside an engine/Unity-managed directory is always refused.
fn resolve_write_target(
    index: &Index,
    raw: &str,
    allow_outside_root: bool,
) -> Result<PathBuf, String> {
    if raw.trim().is_empty() {
        return Err("path must be a non-empty string".to_string());
    }
    let root = index.repo_path();
    let root_c = root
        .canonicalize()
        .map_err(|e| format!("repo root {}: {e}", root.display()))?;
    let p = Path::new(raw);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    // Scope first, so a managed directory or an out-of-root destination is
    // named precisely even when its parent does not exist yet.
    WriteScope::for_index(index).check_optin(&abs, allow_outside_root)?;
    if let Ok(md) = fs::symlink_metadata(&abs) {
        if md.file_type().is_symlink() {
            return Err(format!("refusing to write through a symlink: {raw}"));
        }
    }
    let parent = abs
        .parent()
        .ok_or_else(|| format!("path has no parent directory: {raw}"))?;
    let parent_c = parent
        .canonicalize()
        .map_err(|e| format!("parent directory of {raw}: {e}"))?;
    if !parent_c.starts_with(&root_c) {
        return Err(format!("refusing to write outside the indexed repo: {raw}"));
    }
    let name = abs
        .file_name()
        .ok_or_else(|| format!("path has no file name: {raw}"))?;
    Ok(parent_c.join(name))
}

/// Temp file + fsync + atomic rename. Never deletes a caller file.
pub(crate) fn write_atomic_sync(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(
        ".{name}.csgraph-tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    let staged = (|| -> Result<(), String> {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
        Ok(())
    })();
    if let Err(e) = staged {
        // A failed create/write/fsync must not leave the staging temp behind.
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("rename {}: {e}", path.display()));
    }
    // Best effort: make the rename durable too.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

fn sha_of(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

fn write_file(index: &mut Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let raw_path = match required_str(args, "path") {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let content = match args.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(_) => return json!({"ok": false, "error": "argument \"content\" must be a string"}),
        None => return json!({"ok": false, "error": "argument \"content\" is required"}),
    };
    let create = bget(args, "create", false);
    let apply = bget(args, "apply", false);
    let expected = sget(args, "expected_sha256").map(|s| s.trim().to_lowercase());

    let target = match resolve_write_target(index, &raw_path, allow_outside_root(args)) {
        Ok(t) => t,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let display = index.display(&target);
    let exists = target.is_file();
    let old_sha256 = if exists {
        match fs::read(&target) {
            Ok(b) => Some(sha_of(&b)),
            Err(e) => return json!({"ok": false, "error": format!("read {}: {e}", target.display())}),
        }
    } else {
        None
    };
    if !exists && !create {
        return json!({
            "ok": false,
            "error": "file does not exist; pass create=true to create it",
            "path": display,
            "create": create,
            "old_sha256": Value::Null,
        });
    }
    if let Some(want) = &expected {
        match &old_sha256 {
            Some(have) if have == want => {}
            Some(have) => {
                return json!({
                    "ok": false,
                    "error": "expected_sha256 does not match the current file",
                    "path": display,
                    "old_sha256": have,
                    "expected_sha256": want,
                })
            }
            None => {
                return json!({
                    "ok": false,
                    "error": "expected_sha256 given but the file does not exist",
                    "path": display,
                    "old_sha256": Value::Null,
                })
            }
        }
    }

    let new_sha256 = sha_of(content.as_bytes());
    let mut written: Vec<String> = Vec::new();
    let mut write_error: Option<String> = None;
    if apply {
        if let Err(e) = write_atomic_sync(&target, content.as_bytes()) {
            write_error = Some(e);
        } else {
            written.push(display.clone());
        }
    }
    let mut body = json!({
        "ok": write_error.is_none(),
        "path": display,
        "create": create,
        "bytes": content.len(),
        "old_sha256": old_sha256,
        "new_sha256": new_sha256,
        "applied": apply && write_error.is_none(),
        "written": written,
    });
    if let Some(e) = write_error {
        body["error"] = json!(e);
    }
    body
}

// ---------------------------------------------------------------------------
// 7b. copy_path
// ---------------------------------------------------------------------------

/// `O_NOFOLLOW` without a libc dependency. Opening a path that resolves to a
/// symlink with this flag fails with `ELOOP`, so a path swapped for a symlink
/// between the lstat walk and the read cannot redirect the copy. Only defined
/// for the unix targets this crate builds on; elsewhere the flag is unused.
#[cfg(any(target_os = "linux", target_os = "android"))]
const O_NOFOLLOW: i32 = 0o400000;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
const O_NOFOLLOW: i32 = 0x0100;

/// `(dev, ino)` of `path` from `symlink_metadata` (never following a link), or
/// `None` on a platform without that notion or when the path cannot be lstat-ed.
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Refuse a hardlinked *regular file* source or target: a copy would silently
/// not preserve the link (the other name would keep the old inode), and an
/// overwrite would either break the link or look like an ordinary replace.
/// `symlink_metadata` never follows a symlink, and directory `nlink` (subdir
/// count) is deliberately not checked. Unix-only: other platforms cannot even
/// observe the link count here.
fn refuse_copy_hardlink(path: &Path, display: &str, role: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let md = fs::symlink_metadata(path).map_err(|e| format!("{role} {display}: {e}"))?;
        if md.file_type().is_file() && md.nlink() > 1 {
            return Err(format!(
                "refusing to {role} a hardlinked file (nlink={}): {display}",
                md.nlink()
            ));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, display);
    }
    Ok(())
}

/// Read one planned source, re-validating identity at open time.
///
/// On unix the file is opened with `O_NOFOLLOW` — so a path swapped to a
/// symlink after the lstat walk fails with `ELOOP` instead of being followed —
/// then `fstat`-ed and required to match the `(dev, ino)` recorded during
/// planning, to still be a regular file, and to still have `nlink == 1`. Any
/// mismatch is refused rather than copied. Elsewhere this is the plain read.
fn read_copy_source_verified(f: &CopyFile) -> Result<Vec<u8>, String> {
    #[cfg(unix)]
    {
        use std::io::Read as _;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let mut opts = fs::OpenOptions::new();
        opts.read(true).custom_flags(O_NOFOLLOW);
        let mut file = opts
            .open(&f.from_abs)
            .map_err(|e| format!("read {}: {e}", f.from_display))?;
        let md = file
            .metadata()
            .map_err(|e| format!("stat {}: {e}", f.from_display))?;
        if !md.file_type().is_file() {
            return Err(format!(
                "refusing to copy a non-regular source: {}",
                f.from_display
            ));
        }
        if let Some(want) = f.from_id {
            if (md.dev(), md.ino()) != want {
                return Err(format!(
                    "copy source changed between planning and reading \
                     (dev/ino mismatch): {}",
                    f.from_display
                ));
            }
        }
        if md.nlink() > 1 {
            return Err(format!(
                "refusing to copy a hardlinked file (nlink={}): {}",
                md.nlink(),
                f.from_display
            ));
        }
        let mut bytes = Vec::with_capacity(f.bytes as usize);
        file.read_to_end(&mut bytes)
            .map_err(|e| format!("read {}: {e}", f.from_display))?;
        Ok(bytes)
    }
    #[cfg(not(unix))]
    {
        fs::read(&f.from_abs).map_err(|e| format!("read {}: {e}", f.from_display))
    }
}

/// Re-validate an existing destination immediately before it is replaced:
/// `O_NOFOLLOW` open, `fstat`, then the same `(dev, ino)`/regular-file/nlink
/// checks as the source. A target swapped after planning is refused instead of
/// being renamed away, which narrows the plan-to-rename TOCTOU window.
fn validate_copy_target(f: &CopyFile) -> Result<(), String> {
    if !f.exists {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let mut opts = fs::OpenOptions::new();
        opts.read(true).custom_flags(O_NOFOLLOW);
        let file = opts
            .open(&f.to_abs)
            .map_err(|e| format!("open {}: {e}", f.to_display))?;
        let md = file
            .metadata()
            .map_err(|e| format!("stat {}: {e}", f.to_display))?;
        if !md.file_type().is_file() {
            return Err(format!(
                "refusing to overwrite a non-regular destination: {}",
                f.to_display
            ));
        }
        if let Some(want) = f.to_id {
            if (md.dev(), md.ino()) != want {
                return Err(format!(
                    "copy destination changed between planning and writing \
                     (dev/ino mismatch): {}",
                    f.to_display
                ));
            }
        }
        if md.nlink() > 1 {
            return Err(format!(
                "refusing to overwrite a hardlinked file (nlink={}): {}",
                md.nlink(),
                f.to_display
            ));
        }
    }
    Ok(())
}

/// Resolve a copy source. It must exist, must not itself be a symlink, and must
/// canonicalise inside the indexed repo. Returns `(absolute, display, is_dir)`.
fn resolve_copy_source(index: &Index, raw: &str) -> Result<(PathBuf, String, bool), String> {
    if raw.trim().is_empty() {
        return Err("source path must be a non-empty string".to_string());
    }
    let root = index.repo_path();
    let root_c = root
        .canonicalize()
        .map_err(|e| format!("repo root {}: {e}", root.display()))?;
    let p = Path::new(raw);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        lexical_normalize(&root.join(p))
    };
    let md = fs::symlink_metadata(&abs).map_err(|e| format!("source {raw}: {e}"))?;
    if md.file_type().is_symlink() {
        return Err(format!("refusing to copy a symlink source: {raw}"));
    }
    let canon = abs
        .canonicalize()
        .map_err(|e| format!("source {raw}: {e}"))?;
    if !canon.starts_with(&root_c) {
        return Err(format!("refusing to read outside the indexed repo: {raw}"));
    }
    let is_dir = canon.is_dir();
    if !is_dir && !canon.is_file() {
        return Err(format!(
            "source is not a regular file or directory: {raw}"
        ));
    }
    let display = index.display(&canon);
    if !is_dir {
        refuse_copy_hardlink(&canon, &display, "copy")?;
    }
    Ok((canon, display, is_dir))
}

/// Refuse when `path` or any existing ancestor below the repo root is a
/// symlink, so a directory symlink inside an existing destination cannot
/// redirect the copy outside the repo.
fn refuse_copy_symlink_path(index: &Index, path: &Path, raw: &str) -> Result<(), String> {
    let root = index.repo_path();
    let root_c = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let inside = |p: &Path| p.starts_with(root) || p.starts_with(&root_c);
    let mut cur = path.to_path_buf();
    while inside(&cur) && cur.as_path() != root && cur.as_path() != root_c.as_path() {
        if fs::symlink_metadata(&cur)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(format!("refusing to write through a symlink: {raw}"));
        }
        match cur.parent() {
            Some(par) if inside(par) => cur = par.to_path_buf(),
            _ => break,
        }
    }
    Ok(())
}

/// Resolve a copy destination that may not exist yet. Refuses `..` escapes,
/// absolute paths outside the repo, and any symlink on the destination or in its
/// existing parent chain. Returns `(absolute, display)`.
///
/// The central [`WriteScope`] guard additionally confines the destination to
/// the indexed root(s) and refuses engine/Unity-managed directories.
fn resolve_copy_dest(
    index: &Index,
    raw: &str,
    allow_outside_root: bool,
) -> Result<(PathBuf, String), String> {
    if raw.trim().is_empty() {
        return Err("destination path must be a non-empty string".to_string());
    }
    let root = index.repo_path();
    let root_c = root
        .canonicalize()
        .map_err(|e| format!("repo root {}: {e}", root.display()))?;
    let inside = |p: &Path| p.starts_with(root) || p.starts_with(&root_c);
    let p = Path::new(raw);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    let abs = lexical_normalize(&joined);
    if !inside(&abs) {
        return Err(format!("refusing to write outside the indexed repo: {raw}"));
    }
    WriteScope::for_index(index).check_optin(&abs, allow_outside_root)?;
    refuse_copy_symlink_path(index, &abs, raw)?;
    let display = index.display(&abs);
    Ok((abs, display))
}

/// One file a copy will write. `bytes` comes from the source's metadata and
/// `sha256` is filled in only after the honest `max_files`/`max_bytes` guard
/// has passed (so an oversized request never reads the corpus).
struct CopyFile {
    from_abs: PathBuf,
    from_display: String,
    to_abs: PathBuf,
    to_display: String,
    bytes: u64,
    sha256: String,
    exists: bool,
    /// `(dev, ino)` of the source at plan time; `None` off unix. Re-checked on
    /// the opened fd before the bytes are read.
    from_id: Option<(u64, u64)>,
    /// `(dev, ino)` of an existing destination at plan time; `None` off unix or
    /// when the destination does not exist. Re-checked on the opened fd before
    /// the old target is renamed away.
    to_id: Option<(u64, u64)>,
}

/// Add `dir` and each of its missing ancestors (still inside the repo) to
/// `out`. Existing directories and the repo root itself are never listed.
fn seed_copy_dir(dir: &Path, root: &Path, root_c: &Path, out: &mut BTreeSet<PathBuf>) {
    let inside = |p: &Path| p.starts_with(root) || p.starts_with(root_c);
    let mut cur = dir.to_path_buf();
    while inside(&cur) && cur.as_path() != root && cur.as_path() != root_c && !cur.exists() {
        out.insert(cur.clone());
        match cur.parent() {
            Some(par) if inside(par) => cur = par.to_path_buf(),
            _ => break,
        }
    }
}

/// Collect every file/dir/skipped-symlink under the directory `src`, mapping it
/// under `dst`. Sorted `read_dir` keeps the plan deterministic.
fn collect_copy_dir(
    index: &Index,
    src: &Path,
    dst: &Path,
    src_root: &Path,
    dir_seeds: &mut BTreeSet<PathBuf>,
    files: &mut Vec<CopyFile>,
    skipped: &mut Vec<Value>,
    total: &mut u64,
) -> Result<(), String> {
    let mut entries: Vec<PathBuf> = fs::read_dir(src)
        .map_err(|e| format!("read {}: {e}", src.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for entry in entries {
        let md = match fs::symlink_metadata(&entry) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.file_type().is_symlink() {
            skipped.push(json!({"path": index.display(&entry), "reason": "symlink"}));
            continue;
        }
        let rel = match entry.strip_prefix(src_root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        let target = dst.join(&rel);
        if md.is_dir() {
            dir_seeds.insert(target.clone());
            collect_copy_dir(index, &entry, dst, src_root, dir_seeds, files, skipped, total)?;
        } else if md.is_file() {
            refuse_copy_hardlink(&entry, &index.display(&entry), "copy")?;
            let bytes = md.len();
            *total += bytes;
            files.push(CopyFile {
                from_abs: entry.clone(),
                from_display: index.display(&entry),
                to_display: index.display(&target),
                to_abs: target,
                bytes,
                sha256: String::new(),
                exists: false,
                from_id: file_identity(&entry),
                to_id: None,
            });
        } else {
            skipped.push(json!({"path": index.display(&entry), "reason": "not a regular file"}));
        }
    }
    Ok(())
}

/// Read each planned source file and fill in its sha256. Runs only after the
/// size guards, so a refused request never hashes the corpus.
fn fill_copy_hashes(files: &mut [CopyFile]) -> Result<(), String> {
    for f in files.iter_mut() {
        let bytes = read_copy_source_verified(f)?;
        f.sha256 = sha256_hex(&bytes);
    }
    Ok(())
}

// -- durable copy journal / recovery ----------------------------------------

/// `copy_path`'s durable journal lives in `<repo>/<COPY_JOURNAL_DIR>/<nonce>.json`.
/// `Config::default().excludes` prunes it so the journal is never indexed.
const COPY_JOURNAL_DIR: &str = ".csgraph-copy";
const COPY_JOURNAL_VERSION: u32 = 1;

fn copy_journal_root(index: &Index) -> PathBuf {
    index.repo_path().join(COPY_JOURNAL_DIR)
}

/// A nonce that is unique within this process and across processes: pid, a
/// per-process counter, and the nanosecond clock.
fn copy_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

/// `<target>.csgraph-bak-<nonce>` — a sibling of the target, so the rename that
/// creates it is atomic on one filesystem.
fn copy_backup_path(target: &Path, nonce: &str) -> PathBuf {
    let mut s = target.as_os_str().to_os_string();
    s.push(format!(".csgraph-bak-{nonce}"));
    PathBuf::from(s)
}

/// True only for a *real* directory: `symlink_metadata` is used so a symlinked
/// `.csgraph-copy` is never treated as the journal root (and never followed).
fn journal_root_is_real_dir(dir: &Path) -> bool {
    fs::symlink_metadata(dir)
        .map(|m| m.file_type().is_dir())
        .unwrap_or(false)
}

/// Every `<repo>/<COPY_JOURNAL_DIR>/*.json`, sorted; a missing root is empty.
///
/// The root is lstat-ed (a symlinked journal dir yields nothing) and each entry
/// is lstat-ed too, so a `.csgraph-copy/evil.json -> outside.json` symlink is
/// never read or acted on.
fn list_copy_journals(root: &Path) -> Vec<PathBuf> {
    if !journal_root_is_real_dir(root) {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = fs::read_dir(root)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    fs::symlink_metadata(p)
                        .map(|m| m.file_type().is_file())
                        .unwrap_or(false)
                        && p.extension().and_then(|s| s.to_str()) == Some("json")
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn fsync_dir_best_effort(dir: &Path) {
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Write the journal atomically; `write_atomic_sync` fsyncs the file and its
/// directory, and this re-fsyncs the journal directory after the rename.
fn write_copy_journal(path: &Path, body: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(body).map_err(|e| format!("journal json: {e}"))?;
    write_atomic_sync(path, &bytes)?;
    if let Some(dir) = path.parent() {
        fsync_dir_best_effort(dir);
    }
    Ok(())
}

/// Refuse when `path` or any existing ancestor below the repo root is a symlink.
/// The journal paths are untrusted data, so this mirrors `refuse_copy_symlink_path`
/// but works from a bare root (the target may not exist yet, so only the
/// existing prefix is walked).
fn refuse_journal_symlink_path(root: &Path, root_c: &Path, path: &Path, raw: &str) -> Result<(), String> {
    let inside = |p: &Path| p.starts_with(root) || p.starts_with(root_c);
    let mut cur = path.to_path_buf();
    while inside(&cur) && cur.as_path() != root && cur.as_path() != root_c {
        if fs::symlink_metadata(&cur)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(format!(
                "refusing to act on a journal path through a symlink: {raw}"
            ));
        }
        match cur.parent() {
            Some(par) if inside(par) => cur = par.to_path_buf(),
            _ => break,
        }
    }
    Ok(())
}

/// Refuse a path whose deepest existing ancestor canonicalises outside the repo.
/// Belt-and-suspenders next to the symlink walk: even if a link were swapped in
/// after the lstat, a canonicalised parent outside `root_c` is rejected.
fn refuse_journal_parent_escape(root_c: &Path, path: &Path) -> Result<(), String> {
    let mut cur = path.parent().unwrap_or(path).to_path_buf();
    loop {
        if let Ok(pc) = cur.canonicalize() {
            if pc.starts_with(root_c) {
                return Ok(());
            }
            return Err(format!(
                "refusing a journal path whose parent escapes the repo: {}",
                path.display()
            ));
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => return Ok(()),
        }
    }
}

/// Resolve a journal-recorded repo-relative path, refusing any escape: a journal
/// is untrusted data on disk and must never be able to reach outside the repo.
/// Both the lexical prefix and every existing symlink component are checked, and
/// the deepest existing ancestor must canonicalise inside the repo.
fn resolve_journal_path(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if rel.trim().is_empty() {
        return Err("journal entry has an empty path".to_string());
    }
    let root_c = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let joined = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        root.join(rel)
    };
    let abs = lexical_normalize(&joined);
    if !(abs.starts_with(root) || abs.starts_with(&root_c)) {
        return Err(format!("journal path escapes the repo: {rel}"));
    }
    refuse_journal_symlink_path(root, &root_c, &abs, rel)?;
    refuse_journal_parent_escape(&root_c, &abs)?;
    Ok(abs)
}

/// Create/validate the real `<repo>/.csgraph-copy` directory before writing a
/// journal. A symlinked journal root (or a non-directory) is refused, and the
/// canonicalised root must sit inside the canonicalised repo, so the journal can
/// never land outside the repo.
fn ensure_copy_journal_root(index: &Index) -> Result<PathBuf, String> {
    let root = index.repo_path().to_path_buf();
    let root_c = root
        .canonicalize()
        .map_err(|e| format!("repo root {}: {e}", root.display()))?;
    let journal_root = root.join(COPY_JOURNAL_DIR);
    refuse_journal_symlink_path(&root, &root_c, &journal_root, COPY_JOURNAL_DIR)?;
    if let Ok(md) = fs::symlink_metadata(&journal_root) {
        if !md.file_type().is_dir() {
            return Err(format!(
                "copy journal path is not a directory: {}",
                journal_root.display()
            ));
        }
    }
    fs::create_dir_all(&journal_root)
        .map_err(|e| format!("create {}: {e}", journal_root.display()))?;
    let journal_c = journal_root
        .canonicalize()
        .map_err(|e| format!("journal root {}: {e}", journal_root.display()))?;
    if !journal_c.starts_with(&root_c) {
        return Err(format!(
            "refusing a copy journal outside the repo: {}",
            journal_root.display()
        ));
    }
    Ok(journal_root)
}

/// Rename a backup back over its target. `rename` replaces atomically on Unix;
/// on Windows the target (if any) is removed first.
fn restore_copy_backup(backup: &Path, target: &Path) -> Result<(), String> {
    match fs::rename(backup, target) {
        Ok(()) => Ok(()),
        Err(first) => {
            if target.exists() {
                fs::remove_file(target)
                    .map_err(|e| format!("remove {}: {e}", target.display()))?;
                fs::rename(backup, target)
                    .map_err(|e| format!("restore {}: {e} (first: {first})", target.display()))
            } else {
                Err(format!("restore {}: {first}", target.display()))
            }
        }
    }
}

#[derive(Default)]
struct CopyRecovery {
    restored: Vec<String>,
    removed: Vec<String>,
    removed_dirs: Vec<String>,
    errors: Vec<String>,
    /// Read-only decision rows — one per journal entry (and per committed
    /// backup). Present for dry-run *and* apply, so a caller always sees what
    /// was planned or done.
    plan: Vec<Value>,
}

impl CopyRecovery {
    fn ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// The canonical repo id every engine-written journal carries in its `repo`
/// field. Recovery refuses a journal whose `repo` does not match, so a stray or
/// hand-copied journal from another tree is never acted on.
fn journal_repo_id(index: &Index) -> String {
    let root = index.repo_path();
    root.canonicalize()
        .unwrap_or_else(|_| root.to_path_buf())
        .display()
        .to_string()
}

/// The only backup name recovery will ever act on: the sibling of `target`
/// that this engine's own apply creates (`<target>.csgraph-bak-<nonce>`, see
/// [`copy_backup_path`]). Any other `backup` value is a forged journal.
fn engine_backup_name(target_rel: &str, nonce: &str) -> String {
    format!("{target_rel}.csgraph-bak-{nonce}")
}

/// Validate that a `.csgraph-copy/*.json` file really is one of this engine's
/// journals before anything is renamed or deleted. The journal is untrusted
/// data on disk: a hand-crafted file must fail here (foreign/corrupt) rather
/// than drive recovery. Returns its `entries` array on success.
///
/// Beyond the engine marker/version/nonce/repo shape, every `existed:true`
/// entry's `backup` must be exactly [`engine_backup_name`] for its target and
/// the journal nonce — the engine's own sibling artifact. A crafted
/// `backup:"unrelated.cs"` (which would otherwise be moved over the target) is
/// refused here, before any mutation.
fn validate_copy_journal(index: &Index, v: &Value) -> Result<Vec<Value>, String> {
    if v.get("tool").and_then(Value::as_str) != Some("copy_path") {
        return Err("journal is not engine-written (missing tool:\"copy_path\")".to_string());
    }
    if v.get("version").and_then(Value::as_u64) != Some(COPY_JOURNAL_VERSION as u64) {
        return Err("journal version does not match this engine".to_string());
    }
    if v.get("nonce")
        .and_then(Value::as_str)
        .map(str::is_empty)
        .unwrap_or(true)
    {
        return Err("journal has no nonce".to_string());
    }
    match v.get("repo").and_then(Value::as_str) {
        Some(r) if r == journal_repo_id(index) => {}
        Some(_) => return Err("journal belongs to a different repository".to_string()),
        None => return Err("journal has no repo field".to_string()),
    }
    match v.get("status").and_then(Value::as_str) {
        Some("pending") | Some("committed") | None => {}
        Some(other) => return Err(format!("journal has unknown status {other:?}")),
    }
    let entries = v
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "journal has no entries array".to_string())?;
    let nonce = v.get("nonce").and_then(Value::as_str).unwrap_or("");
    for e in &entries {
        if e.get("existed").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let target = e
            .get("target")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "journal entry has no target".to_string())?;
        let backup = e
            .get("backup")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("journal entry {target} has no backup"))?;
        let want = engine_backup_name(target, nonce);
        if backup != want {
            return Err(format!(
                "journal entry {target} names a foreign backup {backup:?} \
                 (expected {want:?})"
            ));
        }
    }
    Ok(entries)
}

/// Restore one journal, branching on its `status`.
///
/// `"pending"` (or absent, for an older journal) is a rollback: put every
/// backup back over its target, delete files that the copy created, drop
/// created dirs deepest-first when empty, then delete the journal.
///
/// `"committed"` means every target already landed: the copy is final, so this
/// is cleanup-only — delete any backup the interrupted cleanup left behind,
/// delete the journal, and never touch a target. Created dirs are deliberately
/// kept too: they are committed artifacts, and an intentionally recreated empty
/// dir must survive.
///
/// The journal must pass [`validate_copy_journal`] first; a hand-crafted
/// journal is refused with `errors` and *nothing* is deleted. For an
/// `existed:false` entry the on-disk bytes must still match the recorded
/// post-write `sha256After` before the file is removed, and for an
/// `existed:true` entry the backup must exist (and be a regular file) before it
/// is renamed back.
///
/// `apply == false` is a dry run: the same decisions are computed and reported
/// (and any corruption/refusal is still an `error`), but no path is renamed,
/// removed or written. Both branches are idempotent.
fn recover_one_copy_journal(index: &Index, journal: &Path, apply: bool) -> CopyRecovery {
    let root = index.repo_path().to_path_buf();
    let mut out = CopyRecovery::default();
    let display = index.display(journal);
    let parsed = fs::read(journal)
        .map_err(|e| format!("read {}: {e}", journal.display()))
        .and_then(|b| {
            serde_json::from_slice::<Value>(&b)
                .map_err(|e| format!("parse {}: {e}", journal.display()))
        });
    let v = match parsed {
        Ok(v) => v,
        Err(e) => {
            out.errors.push(e);
            out.plan.push(json!({
                "journal": display, "action": "refuse", "reason": "unreadable journal",
            }));
            return out;
        }
    };
    let entries = match validate_copy_journal(index, &v) {
        Ok(entries) => entries,
        Err(e) => {
            out.errors.push(format!("{display}: {e}"));
            out.plan.push(json!({
                "journal": display, "action": "refuse", "reason": e,
            }));
            return out;
        }
    };
    let nonce = v.get("nonce").and_then(Value::as_str).unwrap_or("").to_string();
    if v.get("status").and_then(Value::as_str) == Some("committed") {
        // The apply passed its commit point: every target already holds the
        // new bytes, so never restore or remove a target and never remove a
        // created dir. Just finish the interrupted backup/journal cleanup.
        for e in &entries {
            let backup_rel = e.get("backup").and_then(Value::as_str).unwrap_or("");
            if backup_rel.is_empty() {
                continue;
            }
            let backup = match resolve_journal_path(&root, backup_rel) {
                Ok(p) => p,
                Err(err) => {
                    out.errors.push(err);
                    continue;
                }
            };
            if !fs::symlink_metadata(&backup)
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                continue; // already deleted by the interrupted cleanup
            }
            if apply {
                match fs::remove_file(&backup) {
                    Ok(()) => {
                        fsync_dir_best_effort(backup.parent().unwrap_or(&root));
                        out.removed.push(backup_rel.to_string());
                    }
                    Err(err) => out
                        .errors
                        .push(format!("remove backup {}: {err}", backup.display())),
                }
            } else {
                out.removed.push(backup_rel.to_string());
            }
            out.plan.push(json!({
                "journal": display, "action": "removeBackup", "path": backup_rel,
            }));
        }
        if apply && out.ok() {
            match fs::remove_file(journal) {
                Ok(()) => fsync_dir_best_effort(journal.parent().unwrap_or(&root)),
                Err(e) => out
                    .errors
                    .push(format!("remove journal {}: {e}", journal.display())),
            }
        }
        return out;
    }
    for e in &entries {
        let target_rel = match e
            .get("target")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(s) => s,
            None => {
                out.errors
                    .push(format!("{display}: journal entry has no target"));
                continue;
            }
        };
        let target = match resolve_journal_path(&root, target_rel) {
            Ok(p) => p,
            Err(err) => {
                out.plan.push(json!({
                    "journal": display, "action": "refuse", "path": target_rel,
                    "reason": err,
                }));
                out.errors.push(err);
                continue;
            }
        };
        // Recovery is a write path too: a journal entry that names a
        // managed directory (or a target outside the indexed root) is refused
        // rather than restored into it.
        if let Err(err) = WriteScope::for_index(index).check(&target) {
            out.plan.push(json!({
                "journal": display, "action": "refuse", "path": target_rel,
                "reason": err,
            }));
            out.errors.push(format!(
                "{display}: journal entry {target_rel} is outside the write scope"
            ));
            continue;
        }
        let existed = match e.get("existed").and_then(Value::as_bool) {
            Some(b) => b,
            None => {
                out.errors.push(format!(
                    "{display}: journal entry {target_rel} has no existed flag"
                ));
                continue;
            }
        };
        if existed {
            let backup_rel = match e
                .get("backup")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                Some(s) => s,
                None => {
                    out.errors
                        .push(format!("{display}: entry {target_rel} has no backup"));
                    continue;
                }
            };
            let expected_backup = engine_backup_name(target_rel, &nonce);
            if backup_rel != expected_backup {
                // Second, mutation-site check of the same invariant: only the
                // engine's own sibling backup may ever be renamed over a target.
                out.errors.push(format!(
                    "{display}: entry {target_rel} names a foreign backup \
                     {backup_rel:?}; not moved"
                ));
                out.plan.push(json!({
                    "journal": display, "action": "refuse", "path": target_rel,
                    "reason": "foreign backup name",
                }));
                continue;
            }
            let backup = match resolve_journal_path(&root, backup_rel) {
                Ok(p) => p,
                Err(err) => {
                    out.errors.push(err);
                    continue;
                }
            };
            if !fs::symlink_metadata(&backup)
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                out.errors
                    .push(format!("{display}: backup for {target_rel} is missing"));
                out.plan.push(json!({
                    "journal": display, "action": "refuse", "path": target_rel,
                    "reason": "backup missing",
                }));
                continue;
            }
            if apply {
                match restore_copy_backup(&backup, &target) {
                    Ok(()) => {
                        fsync_dir_best_effort(target.parent().unwrap_or(&root));
                        if let Some(want) = e.get("sha256Before").and_then(Value::as_str) {
                            match fs::read(&target) {
                                Ok(b) if sha_of(&b) == want => {}
                                Ok(_) => out
                                    .errors
                                    .push(format!(
                                        "restored {target_rel} does not match sha256Before"
                                    )),
                                Err(err) => out.errors.push(format!("verify {target_rel}: {err}")),
                            }
                        }
                        out.restored.push(target_rel.to_string());
                    }
                    Err(err) => {
                        out.errors.push(err);
                        continue;
                    }
                }
            } else {
                out.restored.push(target_rel.to_string());
            }
            out.plan.push(json!({
                "journal": display, "action": "restore", "path": target_rel,
                "backup": backup_rel,
            }));
        } else {
            let want = match e
                .get("sha256After")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                Some(s) => s.to_string(),
                None => {
                    out.errors.push(format!(
                        "{display}: entry {target_rel} records no post-write sha256"
                    ));
                    out.plan.push(json!({
                        "journal": display, "action": "refuse", "path": target_rel,
                        "reason": "no post-write sha256",
                    }));
                    continue;
                }
            };
            match fs::symlink_metadata(&target) {
                Ok(md) if md.file_type().is_file() => match fs::read(&target) {
                    Ok(b) if sha_of(&b) == want => {
                        if apply {
                            match fs::remove_file(&target) {
                                Ok(()) => {
                                    fsync_dir_best_effort(target.parent().unwrap_or(&root));
                                    out.removed.push(target_rel.to_string());
                                }
                                Err(err) => out
                                    .errors
                                    .push(format!("remove {}: {err}", target.display())),
                            }
                        } else {
                            out.removed.push(target_rel.to_string());
                        }
                        out.plan.push(json!({
                            "journal": display, "action": "remove", "path": target_rel,
                        }));
                    }
                    Ok(_) => {
                        out.errors.push(format!(
                            "{display}: {target_rel} does not match the recorded \
                             post-write sha256; not removed"
                        ));
                        out.plan.push(json!({
                            "journal": display, "action": "refuse", "path": target_rel,
                            "reason": "sha256 mismatch",
                        }));
                    }
                    Err(err) => out.errors.push(format!("verify {target_rel}: {err}")),
                },
                Ok(_) => {
                    out.errors.push(format!(
                        "{display}: refusing to remove a non-regular target: {target_rel}"
                    ));
                    out.plan.push(json!({
                        "journal": display, "action": "refuse", "path": target_rel,
                        "reason": "not a regular file",
                    }));
                }
                Err(_) => out.plan.push(json!({
                    "journal": display, "action": "skip", "path": target_rel,
                    "reason": "already absent",
                })),
            }
        }
    }
    // Created dirs, deepest first, and only while empty — and only when the
    // journal is otherwise clean, so a refused recover never deletes anything.
    if out.ok() {
        let mut dirs: Vec<(usize, String)> = v
            .get("createdDirs")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(|s| (Path::new(s).components().count(), s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        for (_, rel) in dirs {
            if let Ok(abs) = resolve_journal_path(&root, &rel) {
                if !abs.is_dir() {
                    continue;
                }
                let empty = fs::read_dir(&abs)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(false);
                if !empty {
                    continue;
                }
                if !apply {
                    out.removed_dirs.push(rel.clone());
                } else if fs::remove_dir(&abs).is_ok() {
                    out.removed_dirs.push(rel.clone());
                } else {
                    continue;
                }
                out.plan.push(json!({
                    "journal": display, "action": "removeDir", "path": rel,
                }));
            }
        }
    }
    if apply && out.ok() {
        match fs::remove_file(journal) {
            Ok(()) => fsync_dir_best_effort(journal.parent().unwrap_or(&root)),
            Err(e) => out
                .errors
                .push(format!("remove journal {}: {e}", journal.display())),
        }
    }
    out
}

/// `action:"recover"` — process every pending journal, then drop the journal
/// directory when the last one is gone.
///
/// Like every other write in this family, recovery is a dry run unless
/// `apply:true` is passed: without it the response carries the same counts plus
/// a per-entry `plan`, and nothing on disk changes. A journal that is not
/// engine-shaped, or whose `existed:true` backup is not
/// `<target>.csgraph-bak-<nonce>`, or whose `existed:false` target no longer
/// matches the recorded post-write hash, is an `error`/`incomplete:true` and is
/// left in place.
///
/// Trust boundary: the journal lives in the repo and is attacker-writable by
/// anyone already able to write inside `<repo>/.csgraph-copy/` — the same
/// domain that can touch the target files directly. These checks keep recovery
/// from *amplifying* a crafted journal into an unrelated move/delete; they are
/// not a credential or privilege boundary.
fn copy_recover(index: &Index, apply: bool) -> Value {
    let root = index.repo_path().to_path_buf();
    let journal_root = root.join(COPY_JOURNAL_DIR);
    let journals = list_copy_journals(&journal_root);
    let mut total = CopyRecovery::default();
    for j in &journals {
        let r = recover_one_copy_journal(index, j, apply);
        total.restored.extend(r.restored);
        total.removed.extend(r.removed);
        total.removed_dirs.extend(r.removed_dirs);
        total.errors.extend(r.errors);
        total.plan.extend(r.plan);
    }
    if apply && total.ok() && journal_root_is_real_dir(&journal_root) {
        let _ = fs::remove_dir(&journal_root);
    }
    if apply {
        fsync_dir_best_effort(&root);
    }
    let incomplete = !total.ok();
    let mut body = json!({
        "ok": !incomplete,
        "tool": "copy_path",
        "action": "recover",
        "apply": apply,
        "applied": apply && !incomplete,
        "dryRun": !apply,
        "journals": journals.len(),
        "restored": total.restored.len(),
        "removed": total.removed.len(),
        "removedDirs": total.removed_dirs.len(),
        "incomplete": incomplete,
        "plan": total.plan,
    });
    if !total.errors.is_empty() {
        body["errors"] = json!(total.errors);
    }
    body
}

/// `CSGRAPH_COPY_FAULT` — test/probe-only crash injection. One of
/// `create-dirs`, `after-rename:<N>`, `before-journal-delete`,
/// `after-commit:<N>` (fires after the fsynced `status:"committed"` marker and
/// after the Nth backup has been deleted).
///
/// Unit tests override it with a thread-local (below) because the environment is
/// process-global and libtest runs test functions on parallel threads; the
/// real, cross-process path is the env var, proven **in real server processes**
/// by `tools/probe_copy_recover.py`.
fn copy_fault() -> Option<String> {
    #[cfg(test)]
    {
        if let Some(f) = TEST_COPY_FAULT.with(|c| c.borrow().clone()) {
            return Some(f);
        }
    }
    match std::env::var("CSGRAPH_COPY_FAULT") {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

#[cfg(test)]
thread_local! {
    static TEST_COPY_FAULT: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// The fault answer: never rolls back, so the crash state stays on disk for a
/// *second* process to restore through `action:"recover"`.
fn copy_fault_body(fault: &str, from: &str, to: &str, kind: &str) -> Value {
    json!({
        "ok": false,
        "tool": "copy_path",
        "from": from,
        "to": to,
        "kind": kind,
        "apply": true,
        "applied": false,
        "fault": fault,
        "error": format!("fault injection {fault}: left the crash state on disk for action:\"recover\""),
    })
}

/// `copy_path{from?/src?/source?, to?/dest?, overwrite?, apply?, max_files?,
/// max_bytes?, action?:"copy"|"recover", recover?}` — a repo-confined file or
/// directory-tree copy, dry-run by default. `apply` is durable and recoverable:
/// a fsynced journal plus a backup-by-rename per overwritten target; a crash
/// leaves that state for a later `action:"recover"` (across process boundaries).
fn copy_path(index: &mut Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let action = sget(args, "action")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("copy")
        .to_string();
    if action == "recover" {
        // Recovery follows the same dry-run convention as every other write in
        // this family: `apply` must be passed explicitly before anything on
        // disk is renamed or deleted.
        return copy_recover(index, bget(args, "apply", false));
    }
    if action != "copy" {
        return json!({
            "ok": false,
            "tool": "copy_path",
            "error": format!("unknown action {action:?}; expected \"copy\" or \"recover\""),
        });
    }
    let recover_first = bget(args, "recover", false);
    let journal_root = copy_journal_root(index);
    let pending = list_copy_journals(&journal_root);
    let mut recovery: Option<Value> = None;
    // A previous crashed apply must be recovered before a new one starts: the
    // plan below reads the *current* on-disk state, which recovery may change.
    if bget(args, "apply", false) && !pending.is_empty() {
        if !recover_first {
            return json!({
                "ok": false,
                "tool": "copy_path",
                "error": format!(
                    "{} unrecovered copy journal(s) exist; pass recover=true to recover them first",
                    pending.len()
                ),
                "unrecovered": pending.iter().map(|p| index.display(p)).collect::<Vec<_>>(),
                "apply": true,
                "applied": false,
            });
        }
        let journals = pending.len();
        let mut total = CopyRecovery::default();
        for j in &pending {
            let r = recover_one_copy_journal(index, j, true);
            total.restored.extend(r.restored);
            total.removed.extend(r.removed);
            total.removed_dirs.extend(r.removed_dirs);
            total.errors.extend(r.errors);
        }
        if journal_root_is_real_dir(&journal_root) {
            let _ = fs::remove_dir(&journal_root);
        }
        let incomplete = !total.ok();
        let mut rb = json!({
            "journals": journals,
            "restored": total.restored.len(),
            "removed": total.removed.len(),
            "removedDirs": total.removed_dirs.len(),
            "incomplete": incomplete,
        });
        if !total.errors.is_empty() {
            rb["errors"] = json!(total.errors);
        }
        if incomplete {
            rb["ok"] = json!(false);
            return json!({
                "ok": false,
                "tool": "copy_path",
                "error": "recover before copy was incomplete",
                "recovery": rb,
            });
        }
        recovery = Some(rb);
    }
    let from_raw = sget(args, "from")
        .or_else(|| sget(args, "src"))
        .or_else(|| sget(args, "source"));
    let from_raw = match from_raw.filter(|s| !s.trim().is_empty()) {
        Some(s) => s.to_string(),
        None => {
            return json!({"ok": false, "tool": "copy_path",
                "error": "argument \"from\" is required (aliases: src, source)"})
        }
    };
    let to_raw = sget(args, "to").or_else(|| sget(args, "dest"));
    let to_raw = match to_raw.filter(|s| !s.trim().is_empty()) {
        Some(s) => s.to_string(),
        None => {
            return json!({"ok": false, "tool": "copy_path",
                "error": "argument \"to\" is required (alias: dest)"})
        }
    };
    let overwrite = bget(args, "overwrite", false);
    let apply = bget(args, "apply", false);
    let max_files = iget(args, "max_files", 20_000).max(0) as u64;
    let max_bytes = iget(args, "max_bytes", 536_870_912).max(0) as u64;

    let (from_abs, from_display, is_dir) = match resolve_copy_source(index, &from_raw) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "tool": "copy_path", "error": e}),
    };
    let (to_abs, to_display) = match resolve_copy_dest(index, &to_raw, allow_outside_root(args)) {
        Ok(v) => v,
        Err(e) => return json!({"ok": false, "tool": "copy_path", "error": e}),
    };
    if to_abs == from_abs || to_abs.starts_with(&from_abs) {
        return json!({
            "ok": false,
            "tool": "copy_path",
            "error": "destination must not be equal to or inside the source",
            "from": from_display,
            "to": to_display,
        });
    }
    let kind = if is_dir { "dir" } else { "file" };
    // Destination type must agree with the source.
    if let Ok(md) = fs::symlink_metadata(&to_abs) {
        if md.file_type().is_symlink() {
            return json!({"ok": false, "tool": "copy_path",
                "error": format!("refusing to write through a symlink: {to_display}")});
        }
        if is_dir && !md.is_dir() {
            return json!({"ok": false, "tool": "copy_path",
                "error": "destination type mismatch: source is a directory but the destination is a file",
                "from": from_display, "to": to_display});
        }
        if !is_dir && md.is_dir() {
            return json!({"ok": false, "tool": "copy_path",
                "error": "destination type mismatch: source is a file but the destination is a directory",
                "from": from_display, "to": to_display});
        }
    }

    let root = index.repo_path().to_path_buf();
    let root_c = root
        .canonicalize()
        .unwrap_or_else(|_| root.clone());
    let mut dir_seeds: BTreeSet<PathBuf> = BTreeSet::new();
    let mut files: Vec<CopyFile> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut total_bytes: u64 = 0;
    if is_dir {
        dir_seeds.insert(to_abs.clone());
        if let Err(e) = collect_copy_dir(
            index,
            &from_abs,
            &to_abs,
            &from_abs,
            &mut dir_seeds,
            &mut files,
            &mut skipped,
            &mut total_bytes,
        ) {
            return json!({"ok": false, "tool": "copy_path", "error": e});
        }
    } else {
        if let Some(par) = to_abs.parent() {
            dir_seeds.insert(par.to_path_buf());
        }
        let bytes = fs::symlink_metadata(&from_abs)
            .map(|m| m.len())
            .unwrap_or(0);
        total_bytes = bytes;
        files.push(CopyFile {
            from_abs: from_abs.clone(),
            from_display: from_display.clone(),
            to_display: to_display.clone(),
            to_abs: to_abs.clone(),
            bytes,
            sha256: String::new(),
            exists: false,
            from_id: file_identity(&from_abs),
            to_id: None,
        });
    }
    files.sort_by(|a, b| a.to_display.cmp(&b.to_display));
    skipped.sort_by(|a, b| {
        str_of(a, "path")
            .cmp(str_of(b, "path"))
    });

    // Honest guards: refuse above the limits before reading/hashing anything.
    if files.len() as u64 > max_files {
        return json!({
            "ok": false,
            "tool": "copy_path",
            "error": format!("refusing to copy {} files (max_files={max_files})", files.len()),
            "from": from_display, "to": to_display, "count": files.len(), "max_files": max_files,
        });
    }
    if total_bytes > max_bytes {
        return json!({
            "ok": false,
            "tool": "copy_path",
            "error": format!("refusing to copy {total_bytes} bytes (max_bytes={max_bytes})"),
            "from": from_display, "to": to_display, "totalBytes": total_bytes, "max_bytes": max_bytes,
        });
    }
    if let Err(e) = fill_copy_hashes(&mut files) {
        return json!({"ok": false, "tool": "copy_path", "error": e});
    }

    // Destination collisions. Dry run and apply both refuse identically here.
    let mut collisions: Vec<String> = Vec::new();
    for f in files.iter_mut() {
        // Every per-file destination of a tree copy must pass the central write
        // scope too, so a managed directory *inside* the copied tree is refused.
        if let Err(e) = WriteScope::for_index(index).check_optin(&f.to_abs, allow_outside_root(args)) {
            return json!({"ok": false, "tool": "copy_path", "error": e});
        }
        if let Err(e) = refuse_copy_symlink_path(index, &f.to_abs, &f.to_display) {
            return json!({"ok": false, "tool": "copy_path", "error": e});
        }
        let md = fs::symlink_metadata(&f.to_abs);
        // A directory sitting where a file must land is a plan-time type
        // mismatch, refused before anything is staged (not a rename failure).
        if md.as_ref().map(|m| m.is_dir()).unwrap_or(false) {
            return json!({
                "ok": false,
                "tool": "copy_path",
                "error": format!(
                    "destination type mismatch: a directory already exists at {}",
                    f.to_display
                ),
                "from": from_display, "to": to_display, "path": f.to_display,
            });
        }
        f.exists = md.as_ref().map(|m| m.is_file()).unwrap_or(false);
        f.to_id = if f.exists {
            file_identity(&f.to_abs)
        } else {
            None
        };
        if f.exists {
            if let Err(e) = refuse_copy_hardlink(&f.to_abs, &f.to_display, "overwrite") {
                return json!({
                    "ok": false,
                    "tool": "copy_path",
                    "error": e,
                    "from": from_display, "to": to_display, "path": f.to_display,
                });
            }
            collisions.push(f.to_display.clone());
        }
    }
    if !overwrite && !collisions.is_empty() {
        collisions.sort();
        collisions.dedup();
        return json!({
            "ok": false,
            "tool": "copy_path",
            "error": "destination file(s) already exist; pass overwrite=true to replace them",
            "from": from_display,
            "to": to_display,
            "apply": apply,
            "overwrite": overwrite,
            "collisions": collisions,
            "applied": false,
            "written": [],
        });
    }

    // Only the directories that do not exist yet need creating.
    let mut dirs_abs: BTreeSet<PathBuf> = BTreeSet::new();
    for seed in &dir_seeds {
        seed_copy_dir(seed, &root, &root_c, &mut dirs_abs);
    }
    for d in &dirs_abs {
        if let Err(e) = WriteScope::for_index(index).check_optin(d, allow_outside_root(args)) {
            return json!({"ok": false, "tool": "copy_path", "error": e});
        }
    }
    let dirs: Vec<String> = dirs_abs.iter().map(|d| index.display(d)).collect();

    let created_planned = files.iter().filter(|f| !f.exists).count() as i64;
    let overwritten_planned = files.iter().filter(|f| f.exists).count() as i64;
    let file_rows: Vec<Value> = files
        .iter()
        .map(|f| {
            json!({
                "from": f.from_display,
                "to": f.to_display,
                "bytes": f.bytes,
                "sha256": f.sha256,
                "action": if f.exists { "overwrite" } else { "create" },
            })
        })
        .collect();

    let mut written: Vec<String> = Vec::new();
    let mut rolled_back: Vec<String> = Vec::new();
    let mut write_error: Option<String> = None;
    if apply {
        let nonce = copy_nonce();
        let journal_path = journal_root.join(format!("{nonce}.json"));
        // Every entry is fully described (target, backup, existed, sha256Before,
        // sha256After) before the journal is written, i.e. before the first
        // mutation. `sha256After` is the post-write hash recovery verifies
        // before it ever deletes a file the copy created.
        let mut backups: Vec<Option<PathBuf>> = Vec::with_capacity(files.len());
        let mut entries: Vec<Value> = Vec::with_capacity(files.len());
        for f in &files {
            if f.exists {
                let backup = copy_backup_path(&f.to_abs, &nonce);
                match fs::read(&f.to_abs) {
                    Ok(before) => {
                        entries.push(json!({
                            "target": f.to_display,
                            "backup": index.display(&backup),
                            "existed": true,
                            "sha256Before": sha_of(&before),
                            "sha256After": f.sha256,
                        }));
                        backups.push(Some(backup));
                    }
                    Err(e) => {
                        return json!({
                            "ok": false,
                            "tool": "copy_path",
                            "error": format!("read {}: {e}", f.to_display),
                        })
                    }
                }
            } else {
                entries.push(json!({
                    "target": f.to_display,
                    "backup": Value::Null,
                    "existed": false,
                    "sha256Before": Value::Null,
                    "sha256After": f.sha256,
                }));
                backups.push(None);
            }
        }
        let journal_body = json!({
            "tool": "copy_path",
            "version": COPY_JOURNAL_VERSION,
            "nonce": nonce,
            "repo": journal_repo_id(index),
            "status": "pending",
            "from": from_display,
            "to": to_display,
            "createdDirs": dirs,
            "entries": entries,
        });
        if let Err(e) = ensure_copy_journal_root(index) {
            return json!({"ok": false, "tool": "copy_path", "error": e});
        }
        fsync_dir_best_effort(index.repo_path());
        if let Err(e) = write_copy_journal(&journal_path, &journal_body) {
            if journal_root_is_real_dir(&journal_root) {
                let _ = fs::remove_dir(&journal_root);
            }
            return json!({"ok": false, "tool": "copy_path", "error": e});
        }

        // Apply order: create dirs -> per file { rename target to its backup
        // (fsync) -> write_atomic_sync the new bytes }.
        for d in &dirs_abs {
            if let Err(e) = fs::create_dir_all(d) {
                write_error = Some(format!("create {}: {e}", d.display()));
                break;
            }
        }
        if write_error.is_none() {
            if let Some(fault) = copy_fault() {
                if fault == "create-dirs" {
                    return copy_fault_body(&fault, &from_display, &to_display, kind);
                }
            }
        }
        if write_error.is_none() {
            let mut renamed: u64 = 0;
            for (i, f) in files.iter().enumerate() {
                // Read the source before any mutation for this entry, opening
                // it with O_NOFOLLOW and re-checking its dev/ino, so a path
                // swapped after planning is refused.
                let bytes = match read_copy_source_verified(f) {
                    Ok(b) => b,
                    Err(e) => {
                        write_error = Some(e);
                        break;
                    }
                };
                // And re-check an existing destination immediately before the
                // rename that would displace it.
                if let Err(e) = validate_copy_target(f) {
                    write_error = Some(e);
                    break;
                }
                if let Some(backup) = &backups[i] {
                    if let Err(e) = fs::rename(&f.to_abs, backup) {
                        write_error = Some(format!("rename {}: {e}", f.to_display));
                        break;
                    }
                    fsync_dir_best_effort(f.to_abs.parent().unwrap_or(&root));
                    renamed += 1;
                    if let Some(fault) = copy_fault() {
                        if fault == format!("after-rename:{renamed}") {
                            return copy_fault_body(&fault, &from_display, &to_display, kind);
                        }
                    }
                }
                if let Err(e) = write_atomic_sync(&f.to_abs, &bytes) {
                    write_error = Some(e);
                    break;
                }
                written.push(f.to_display.clone());
            }
        }
        if write_error.is_none() {
            if let Some(fault) = copy_fault() {
                if fault == "before-journal-delete" {
                    return copy_fault_body(&fault, &from_display, &to_display, kind);
                }
            }
            // Commit point: atomically fsync `status:"committed"` BEFORE the
            // first backup is deleted, so a crash during cleanup can never make
            // recovery mix restored and committed targets.
            let mut committed_body = journal_body.clone();
            committed_body["status"] = json!("committed");
            if let Err(e) = write_copy_journal(&journal_path, &committed_body) {
                write_error = Some(format!("commit marker {}: {e}", journal_path.display()));
            }
        }
        if let Some(e) = write_error {
            // Immediate rollback through the same journal-driven path a later
            // `action:"recover"` would use, then drop the journal. The journal
            // is still `pending` (a failed commit-marker write leaves the
            // fsynced pending file untouched).
            let rec = recover_one_copy_journal(index, &journal_path, true);
            rolled_back.extend(rec.restored.iter().cloned());
            rolled_back.extend(rec.removed.iter().cloned());
            rolled_back.extend(rec.removed_dirs.iter().cloned());
            if journal_root_is_real_dir(&journal_root) {
                let _ = fs::remove_dir(&journal_root);
            }
            write_error = Some(if rec.errors.is_empty() {
                e
            } else {
                format!("{e}; rollback incomplete: {}", rec.errors.join("; "))
            });
        } else {
            // Committed: delete every backup, the journal and the empty dir.
            let mut cleanup_errors: Vec<String> = Vec::new();
            let fault = copy_fault();
            let mut removed_backups: u64 = 0;
            for b in backups.iter().flatten() {
                if let Err(e) = fs::remove_file(b) {
                    cleanup_errors.push(format!("remove backup {}: {e}", b.display()));
                    continue;
                }
                removed_backups += 1;
                if let Some(f) = &fault {
                    if f == &format!("after-commit:{removed_backups}") {
                        return copy_fault_body(f, &from_display, &to_display, kind);
                    }
                }
            }
            if let Err(e) = fs::remove_file(&journal_path) {
                cleanup_errors.push(format!("remove journal {}: {e}", journal_path.display()));
            }
            fsync_dir_best_effort(&journal_root);
            let _ = fs::remove_dir(&journal_root);
            fsync_dir_best_effort(index.repo_path());
            if !cleanup_errors.is_empty() {
                write_error = Some(cleanup_errors.join("; "));
            }
        }
    }

    let applied = apply && write_error.is_none();
    let mut body = json!({
        "ok": write_error.is_none(),
        "tool": "copy_path",
        "from": from_display,
        "to": to_display,
        "kind": kind,
        "apply": apply,
        "applied": applied,
        "overwrite": overwrite,
        "dirs": dirs,
        "files": file_rows,
        "created": created_planned,
        "overwritten": overwritten_planned,
        "totalBytes": total_bytes,
        "skipped": skipped,
        "written": written,
    });
    if !rolled_back.is_empty() {
        body["rolled_back"] = json!(rolled_back);
    }
    if let Some(r) = recovery {
        body["recovery"] = r;
    }
    let unrecovered = list_copy_journals(&journal_root);
    if !unrecovered.is_empty() {
        body["unrecovered"] =
            json!(unrecovered.iter().map(|p| index.display(p)).collect::<Vec<_>>());
    }
    if let Some(e) = write_error {
        body["error"] = json!(e);
    }
    if applied {
        body["note"] = json!("call the `index` tool to re-sync the graph");
    }
    body
}

// ---------------------------------------------------------------------------
// 8. apply_edits
// ---------------------------------------------------------------------------

struct EditSpec {
    idx: usize,
    file: String,
    old: String,
    new: String,
    count: usize,
}

fn apply_edits(index: &mut Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let apply = bget(args, "apply", false);
    let raw_edits = match args.get("edits") {
        Some(Value::Array(a)) if !a.is_empty() => a.clone(),
        Some(Value::Array(_)) => {
            return json!({"ok": false, "error": "edits must be a non-empty array"})
        }
        _ => return json!({"ok": false, "error": "argument \"edits\" is required (array)"}),
    };

    let mut specs: Vec<EditSpec> = Vec::new();
    for (i, e) in raw_edits.iter().enumerate() {
        let file = match e.get("file").and_then(Value::as_str) {
            Some(f) if !f.is_empty() => f.to_string(),
            _ => return json!({"ok": false, "error": format!("edits[{i}].file is required")}),
        };
        let old = match e.get("old").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                return json!({"ok": false,
                    "error": format!("edits[{i}].old is required and must be non-empty")})
            }
        };
        let new = match e.get("new").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return json!({"ok": false, "error": format!("edits[{i}].new is required")}),
        };
        let count = e.get("count").and_then(Value::as_i64).unwrap_or(1);
        if count < 1 {
            return json!({"ok": false, "error": format!("edits[{i}].count must be >= 1")});
        }
        specs.push(EditSpec {
            idx: i,
            file,
            old,
            new,
            count: count as usize,
        });
    }
    // Deterministic: by file, then by the caller's order within the file.
    specs.sort_by(|a, b| a.file.cmp(&b.file).then(a.idx.cmp(&b.idx)));

    // Validate + build the new text for every file before writing anything.
    let mut order: Vec<String> = Vec::new();
    let mut working: BTreeMap<String, String> = BTreeMap::new();
    let mut originals: BTreeMap<String, String> = BTreeMap::new();
    let mut paths: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut reports: Vec<Value> = Vec::new();
    let scope = WriteScope::for_index(index);
    let optin = allow_outside_root(args);
    for spec in &specs {
        if !working.contains_key(&spec.file) {
            let target = match repo_abs(index, &spec.file) {
                Some(p) => p,
                None => {
                    return json!({"ok": false,
                        "error": format!("refusing to edit outside the indexed repo: {}", spec.file)})
                }
            };
            if let Err(e) = scope.check_optin(&target, optin) {
                return json!({"ok": false, "error": e});
            }
            let text = match fs::read_to_string(&target) {
                Ok(t) => t,
                Err(e) => {
                    return json!({"ok": false, "error": format!("read {}: {e}", spec.file)})
                }
            };
            paths.insert(spec.file.clone(), target);
            originals.insert(spec.file.clone(), text.clone());
            working.insert(spec.file.clone(), text);
            order.push(spec.file.clone());
        }
        let cur = working.get(&spec.file).unwrap().clone();
        let matched = cur.matches(&spec.old).count();
        if matched != spec.count {
            return json!({
                "ok": false,
                "error": format!(
                    "{}: expected {} match(es) of {:?}, found {}",
                    spec.file, spec.count, spec.old, matched
                ),
                "path": spec.file,
                "matched": matched,
                "expected": spec.count,
                "applied": false,
            });
        }
        let next = cur.replace(&spec.old, &spec.new);
        let old_sha = sha_of(originals[&spec.file].as_bytes());
        let new_sha = sha_of(next.as_bytes());
        working.insert(spec.file.clone(), next);
        reports.push(json!({
            "file": spec.file,
            "matched": matched,
            "replaced": matched,
            "old_sha256": old_sha,
            "new_sha256": new_sha,
        }));
    }

    let mut written: Vec<String> = Vec::new();
    let mut write_error: Option<String> = None;
    let mut rolled_back: Vec<String> = Vec::new();
    if apply {
        // Stage every file first, then rename them all. `originals` holds the
        // pre-edit bytes of each file, so a failure after the first rename is
        // rolled back instead of leaving a half-applied batch behind.
        let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut renamed: Vec<PathBuf> = Vec::new();
        for file in &order {
            let target = &paths[file];
            let dir = target.parent().unwrap_or_else(|| Path::new("."));
            let name = target
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file");
            let tmp = dir.join(format!(
                ".{name}.csgraph-tmp-{}-{}",
                std::process::id(),
                staged.len()
            ));
            match fs::write(&tmp, working[file].as_bytes()) {
                Ok(()) => {
                    if let Ok(f) = fs::File::open(&tmp) {
                        let _ = f.sync_all();
                    }
                    staged.push((tmp, target.clone()));
                }
                Err(e) => {
                    // The failing temp never enters `staged`, so remove it here
                    // as well as cleaning up whatever was staged before it.
                    let _ = fs::remove_file(&tmp);
                    write_error = Some(format!("stage {}: {e}", tmp.display()));
                    break;
                }
            }
        }
        if write_error.is_none() {
            for (tmp, target) in &staged {
                if let Err(e) = fs::rename(tmp, target) {
                    write_error = Some(format!("rename {}: {e}", target.display()));
                    break;
                }
                renamed.push(target.clone());
            }
        }
        // Clean up any temp file that was staged but never renamed.
        for (tmp, target) in &staged {
            if !renamed.contains(target) {
                let _ = fs::remove_file(tmp);
            }
        }
        // Roll back the files already renamed by restoring the original bytes.
        if write_error.is_some() && !renamed.is_empty() {
            let mut restore_failed = false;
            for target in &renamed {
                let file = index.display(target);
                if let Some(orig) = originals.get(&file) {
                    if write_atomic_sync(target, orig.as_bytes()).is_err() {
                        restore_failed = true;
                    }
                }
                rolled_back.push(file);
            }
            if restore_failed {
                write_error = Some(format!(
                    "{}; rollback of {} file(s) was incomplete",
                    write_error.unwrap_or_default(),
                    renamed.len()
                ));
            }
        }
        if write_error.is_none() {
            written = order.iter().map(|f| f.clone()).collect();
        }
    }

    let mut body = json!({
        "ok": write_error.is_none(),
        "edits": reports,
        "files_planned": order,
        "applied": apply && write_error.is_none(),
        "written": written,
    });
    if !rolled_back.is_empty() {
        body["rolled_back"] = json!(rolled_back);
    }
    if let Some(e) = write_error {
        body["error"] = json!(e);
    }
    body
}

// ---------------------------------------------------------------------------
// 9. move_decls (the write half of `nzkcs`)
// ---------------------------------------------------------------------------

/// Collapse `.` / `..` lexically, without touching the filesystem: a move
/// destination directory may not exist yet, so `canonicalize` is unavailable.
fn lexical_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve a move destination that may not exist yet. The target must land
/// inside the indexed repo and its nearest existing ancestor must canonicalise
/// inside the root, so a symlinked directory cannot escape. Returns
/// `(absolute path, display path, already exists)`.
///
/// The central [`WriteScope`] guard additionally confines the destination to
/// the indexed root(s) and refuses engine/Unity-managed directories; only an
/// explicit `allow_outside_root` widens the root.
pub(crate) fn resolve_move_target(
    index: &Index,
    raw: &str,
    allow_outside_root: bool,
) -> Result<(PathBuf, String, bool), String> {
    if raw.trim().is_empty() {
        return Err("destination path must be a non-empty string".to_string());
    }
    let root = index.repo_path();
    let root_c = root
        .canonicalize()
        .map_err(|e| format!("repo root {}: {e}", root.display()))?;
    let p = Path::new(raw);
    let joined = if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
    let abs = lexical_normalize(&joined);
    if !abs.starts_with(&root_c) && !abs.starts_with(root) {
        return Err(format!("refusing to write outside the indexed repo: {raw}"));
    }
    WriteScope::for_index(index).check_optin(&abs, allow_outside_root)?;
    let mut anc = abs.clone();
    while !anc.exists() {
        match anc.parent() {
            Some(par) if par != anc.as_path() => anc = par.to_path_buf(),
            _ => break,
        }
    }
    if let Ok(c) = anc.canonicalize() {
        if !c.starts_with(&root_c) {
            return Err(format!("refusing to write outside the indexed repo: {raw}"));
        }
    }
    if let Ok(md) = fs::symlink_metadata(&abs) {
        if md.file_type().is_symlink() {
            return Err(format!("refusing to write through a symlink: {raw}"));
        }
    }
    let existed = abs.is_file();
    let display = index.display(&abs);
    Ok((abs, display, existed))
}

/// `newfile` (a path, optionally bare) plus an optional `dir`/`dest` prefix.
pub(crate) fn move_target_path(args: &Value) -> Option<String> {
    let nf = sget(args, "newfile")?;
    let dir = sget(args, "dir").or_else(|| sget(args, "dest"));
    match dir {
        Some(dir) if !nf.contains('/') => {
            Some(format!("{}/{}", dir.trim_end_matches('/'), nf))
        }
        _ => Some(nf.to_string()),
    }
}

/// True when more than one declaration shares this simple name, so a bare
/// occurrence in source cannot be attributed to one declaration textually.
fn name_is_ambiguous(index: &Index, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let mut seen = 0;
    for node in index.nodes() {
        let kind = str_of(node, "kind");
        if matches!(kind, "string" | "external" | "namespace") {
            continue;
        }
        if str_of(node, "name") == name && index.is_declaration_id(str_of(node, "id")) {
            seen += 1;
            if seen > 1 {
                return true;
            }
        }
    }
    false
}

/// Parse the rename-into target `<Namespace>.<Class>.<member>` (the namespace
/// may be empty or itself dotted). Returns `(namespace, class, member)`.
fn parse_new_home(to: &str) -> Result<(String, String, String), String> {
    if !valid_qualified_name(to) {
        return Err(format!(
            "{to:?} is not a legal C# qualified name (<Namespace>.<Class>.<member>)"
        ));
    }
    let segs: Vec<&str> = to.split('.').collect();
    if segs.len() < 2 {
        return Err(format!(
            "{to:?} must be <Namespace>.<Class>.<member> (at least <Class>.<member>)"
        ));
    }
    let member = segs[segs.len() - 1].to_string();
    let class = segs[segs.len() - 2].to_string();
    let ns = segs[..segs.len() - 2].join(".");
    if model::is_keyword(&member) || model::is_keyword(&class) {
        return Err(format!("{to:?} uses a C# keyword as a class or member name"));
    }
    Ok((ns, class, member))
}

/// Infer a destination type name from a qualified `newfile` stem:
/// `NZK.VF.cs` -> `VF`. A bare `moved.cs` yields `None` (refuse, do not guess).
pub(crate) fn infer_class_from_path(raw: &str) -> Option<String> {
    let file = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    if !file.ends_with(".cs") {
        return None;
    }
    let stem = &file[..file.len() - 3];
    if !stem.contains('.') {
        return None;
    }
    let last = stem.rsplit('.').next().unwrap_or("");
    if valid_cs_identifier(last) && !model::is_keyword(last) {
        Some(last.to_string())
    } else {
        None
    }
}

/// Simple name -> unique full name for declarations that can be reached through
/// their owner type: types, and `static`/`const` members. This is the map a
/// *moved* block needs to keep resolving after it leaves its original type.
/// Unlike [`qualify_map`] it also accepts `const` fields, which the parser does
/// not mark `static`.
fn dependency_map(
    index: &Index,
) -> (BTreeMap<String, String>, BTreeMap<String, Vec<String>>) {
    let mut by_simple: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for n in index.nodes() {
        let kind = str_of(n, "kind");
        let is_type = TYPE_KINDS.contains(&kind) || kind == "delegate";
        let sig = n.get("signature").and_then(Value::as_str).unwrap_or("");
        let is_member = MEMBER_KINDS.contains(&kind)
            && (n.get("static").and_then(Value::as_bool).unwrap_or(false)
                || sig.contains("static")
                || sig.contains("const"));
        if !is_type && !is_member {
            continue;
        }
        let id = str_of(n, "id");
        if !index.is_declaration_id(id) {
            continue;
        }
        let name = str_of(n, "name");
        let full = str_of(n, "full");
        if name.is_empty()
            || full.is_empty()
            || name == full
            || !valid_cs_identifier(name)
            || !valid_qualified_name(full)
        {
            continue;
        }
        by_simple.entry(name.to_string()).or_default().insert(full.to_string());
    }
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    let mut ambiguous: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, fulls) in by_simple {
        if fulls.len() == 1 {
            map.insert(name, fulls.into_iter().next().unwrap());
        } else {
            ambiguous.insert(name, fulls.into_iter().collect());
        }
    }
    (map, ambiguous)
}

/// One selected declaration, its comment-aware slice and its new home.
struct Moved {
    id: String,
    file: String,
    block_start: i64,
    end_line: i64,
    name: String,
    old_ns: String,
    full: String,
    kind: String,
    /// `name(param,param,…)` — the C# duplicate-signature key (CS0111) used to
    /// refuse a destination type that would hold two copies of the same member.
    sig: String,
    /// The slice exactly as it moves (extract form) or after the member rename
    /// (rename-into form), including the leading comment/attribute/`#if` block.
    insert_lines: Vec<String>,
    new_full: String,
}

/// The selector half is `select_decls` verbatim; `from` is an extra convenience
/// that resolves one declaration exactly (the `= (A => B)` grammar).
/// Resolve `move_decls`' `from` to exactly one declaration, **exact-only**:
/// node id, exact `full`, or exact `label`; then a unique exact simple `name`.
/// There is deliberately no fuzzy/substring fallback — after a symbol has moved,
/// a stale `from` must refuse with near-matches instead of silently moving a
/// different symbol (e.g. `…FolderFor` matching `…PrefabFolderFor`).
pub(crate) fn resolve_from_declaration(index: &Index, q: &str) -> Result<Value, Value> {
    let q = q.trim();
    let is_decl = |n: &Value| index.is_declaration_id(str_of(n, "id"));
    let mut exact: Vec<Value> = index
        .nodes()
        .iter()
        .filter(|n| {
            is_decl(n)
                && (str_of(n, "id") == q || str_of(n, "full") == q || str_of(n, "label") == q)
        })
        .cloned()
        .collect();
    exact.dedup_by(|a, b| a["id"] == b["id"]);
    if exact.len() == 1 {
        return Ok(exact.pop().unwrap());
    }
    if exact.len() > 1 {
        return Err(json!({
            "ok": false,
            "error": format!("{q:?} matches {} declarations", exact.len()),
            "ambiguity": exact.iter().map(node_row).collect::<Vec<_>>(),
        }));
    }
    let mut by_name: Vec<Value> = index
        .nodes()
        .iter()
        .filter(|n| is_decl(n) && str_of(n, "name") == q)
        .cloned()
        .collect();
    by_name.dedup_by(|a, b| a["id"] == b["id"]);
    if by_name.len() == 1 {
        return Ok(by_name.pop().unwrap());
    }
    if by_name.len() > 1 {
        return Err(json!({
            "ok": false,
            "error": format!("{q:?} matches {} declarations by simple name", by_name.len()),
            "ambiguity": by_name.iter().map(node_row).collect::<Vec<_>>(),
        }));
    }
    // The requested declaration is not there any more (or never was). Report the
    // near matches for a human to pick from; do not move anything.
    let res = fuzzy_search(index, &json!({"q": q, "limit": 20}));
    let near: Vec<Value> = res
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            r.get("id")
                .and_then(Value::as_str)
                .map(|id| index.is_declaration_id(id))
                .unwrap_or(false)
        })
        .collect();
    Err(json!({
        "ok": false,
        "error": format!("{q:?} does not name a declaration (it may have already moved)"),
        "near": near,
    }))
}

fn selected_nodes(index: &Index, args: &Value) -> Result<Vec<Value>, Value> {
    if let Some(from) = sget(args, "from") {
        return resolve_from_declaration(index, from).map(|n| vec![n]);
    }
    let mut sargs = args.clone();
    if let Some(o) = sargs.as_object_mut() {
        o.insert("text".to_string(), json!(true));
    }
    let sel = select_decls(index, &sargs);
    if !sel.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return Err(sel);
    }
    let rows = sel.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for r in rows {
        if let Some(id) = r.get("id").and_then(Value::as_str) {
            if let Some(n) = index.node_value(id) {
                out.push(n.clone());
            }
        }
    }
    Ok(out)
}

/// A file move_decls will write: its final bytes, plus the original bytes for a
/// rollback if a later rename in the same batch fails.
pub(crate) struct MoveFile {
    pub(crate) display: String,
    pub(crate) abs: PathBuf,
    pub(crate) existed: bool,
    pub(crate) content: String,
    pub(crate) original: Option<String>,
}

/// Stage every file (temp + fsync), then rename them all; roll back the files
/// already renamed when a rename fails. New destination directories are created
/// only inside the repo (validated by [`resolve_move_target`]).
pub(crate) fn commit_move_files(writes: &[MoveFile]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    for w in writes {
        let dir = w.abs.parent().unwrap_or_else(|| Path::new("."));
        if !dir.exists() {
            if let Err(e) = fs::create_dir_all(dir) {
                for (t, _) in &staged {
                    let _ = fs::remove_file(t);
                }
                return Err(format!("create {}: {e}", dir.display()));
            }
        }
        let name = w.abs.file_name().and_then(|s| s.to_str()).unwrap_or("file");
        let tmp = dir.join(format!(
            ".{name}.csgraph-move-{}-{}",
            std::process::id(),
            staged.len()
        ));
        if let Err(e) = fs::write(&tmp, w.content.as_bytes()) {
            // Never leave a staging temp behind when the write fails.
            let _ = fs::remove_file(&tmp);
            for (t, _) in &staged {
                let _ = fs::remove_file(t);
            }
            return Err(format!("write {}: {e}", tmp.display()));
        }
        if let Ok(f) = fs::File::open(&tmp) {
            let _ = f.sync_all();
        }
        staged.push((tmp, w.abs.clone()));
    }
    let mut renamed: Vec<usize> = Vec::new();
    for i in 0..staged.len() {
        if let Err(e) = fs::rename(&staged[i].0, &staged[i].1) {
            for (t, _) in &staged {
                let _ = fs::remove_file(t);
            }
            for &j in &renamed {
                match &writes[j].original {
                    Some(orig) => {
                        let _ = write_atomic_sync(&writes[j].abs, orig.as_bytes());
                    }
                    None => {
                        let _ = fs::remove_file(&writes[j].abs);
                    }
                }
            }
            return Err(format!("rename {}: {e}", staged[i].1.display()));
        }
        renamed.push(i);
    }
    Ok((
        writes.iter().map(|w| w.display.clone()).collect(),
        writes.iter().filter(|w| !w.existed).map(|w| w.display.clone()).collect(),
    ))
}

/// Run the Roslyn parse gate over the candidate bytes of every file a move is
/// about to write. The bytes are parked under the OS temp dir (never the
/// corpus) as `.cs` files, `compile_check::parse_check_with` parses them, and
/// each diagnostic's temp `file` is remapped to the real display path.
///
/// `toolchain_override` is the `_dotnet` argument (tests use it to force the
/// `skipped` path); production passes `None`.
fn validate_candidates(
    writes: &[MoveFile],
    toolchain_override: Option<&str>,
) -> Result<Value, String> {
    let candidates: Vec<(String, String)> = writes
        .iter()
        .map(|w| (w.display.clone(), w.content.clone()))
        .collect();
    parse_gate_candidates(&candidates, toolchain_override)
}

/// Run the Roslyn parse gate over candidate contents given as
/// `(display path, candidate bytes)`. The bytes are parked under the OS temp
/// directory (never the corpus), parsed by the real driver, and each
/// diagnostic's temp `file` is remapped to the real display path.
///
/// Shared by `move_decls` and the `reshape` tools so every write path runs
/// exactly one gate. `toolchain_override` is the `_dotnet` argument (tests use
/// it to force the `skipped` path); production passes `None`.
pub(crate) fn parse_gate_candidates(
    candidates: &[(String, String)],
    toolchain_override: Option<&str>,
) -> Result<Value, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("csgraph-move-check-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let mut paths: Vec<String> = Vec::new();
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for (i, (display, content)) in candidates.iter().enumerate() {
        let p = dir.join(format!("candidate{i}.cs"));
        fs::write(&p, content.as_bytes()).map_err(|e| format!("write {}: {e}", p.display()))?;
        let ps = p.to_string_lossy().into_owned();
        map.insert(ps.clone(), display.clone());
        paths.push(ps);
    }
    let report = crate::compile_check::parse_check_with(&paths, toolchain_override, 25);
    let _ = fs::remove_dir_all(&dir);
    let mut report = report;
    if let Some(arr) = report.get_mut("diagnostics").and_then(Value::as_array_mut) {
        for d in arr.iter_mut() {
            if let Some(f) = d.get("file").and_then(Value::as_str) {
                if let Some(real) = map.get(f) {
                    d["file"] = json!(real);
                }
            }
        }
    }
    Ok(report)
}

/// `move_decls` — the write half of the owner's `nzkcs` command, composing
/// `select_decls` -> a validated plan -> the atomic write path. Dry-run by
/// default; `apply:true` writes, then the caller re-runs `index`.
///
/// * select form: `like`/`mtype`/`kind`/`limit` (identical to `select_decls`),
///   or `from` for exactly one declaration;
/// * extract form: `newfile` (+ optional `prefix`/`namespace`, `dir`) moves each
///   selected slice — including its leading comment/attribute/`#if` block — into
///   one file, re-namespaced under `prefix`/`namespace`;
/// * rename-into form: `to = "<Namespace>.<Class>.<member>"` moves one
///   declaration into that class, creating the class and namespace if absent,
///   renaming the member, and re-referencing call sites under the new full name.
///
/// Deletions, insertions and call-site rewrites are all validated before a
/// single byte is written, so the batch is all-or-nothing. A call site whose
/// simple name is declared more than once is reported in `ambiguous` and left
/// untouched, never guessed.
fn move_decls(index: &Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let apply = bget(args, "apply", false);
    let allow_invalid = bget(args, "allow_invalid", false);
    // One scope for the whole call; the move destination and every file the
    // batch rewrites must pass it before anything is staged.
    let scope = WriteScope::for_index(index);
    let optin = allow_outside_root(args);
    let to = sget(args, "to").map(str::to_string);
    let dest_raw = move_target_path(args);
    if to.is_none() && dest_raw.is_none() {
        return json!({
            "ok": false,
            "error": "move_decls needs a target: `to` = \"<Ns>.<Class>.<member>\" \
                      or `newfile` (+ optional `prefix`/`namespace`, `dir`)"
        });
    }
    let rename_into = to.is_some();

    let nodes = match selected_nodes(index, args) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if nodes.is_empty() {
        return json!({"ok": false, "error": "move_decls selected no declarations"});
    }
    if rename_into && nodes.len() != 1 {
        return json!({
            "ok": false,
            "error": format!("rename-into (`to`) moves exactly one declaration; {} selected", nodes.len())
        });
    }

    let new_ns = sget(args, "prefix")
        .or_else(|| sget(args, "namespace"))
        .unwrap_or("")
        .trim()
        .to_string();
    if !new_ns.is_empty() && !valid_qualified_name(&new_ns) {
        return json!({"ok": false, "error": format!("{new_ns:?} is not a legal namespace")});
    }
    let (ns, class, member) = if rename_into {
        match parse_new_home(to.as_deref().unwrap_or("")) {
            Ok(x) => x,
            Err(e) => return json!({"ok": false, "error": e}),
        }
    } else {
        (String::new(), String::new(), String::new())
    };

    // ---- gather slices (with leading blocks) --------------------------------
    let mut lines_by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut flags_by_file: BTreeMap<String, Vec<bool>> = BTreeMap::new();
    let mut moved: Vec<Moved> = Vec::new();
    for node in &nodes {
        let node_file = str_of(node, "file").to_string();
        if node_file.is_empty() {
            return json!({"ok": false, "error": "a selected declaration has no file"});
        }
        let fallback_line = node.get("line").and_then(Value::as_i64).unwrap_or(0).max(1);
        let fallback_end = node
            .get("endLine")
            .and_then(Value::as_i64)
            .unwrap_or(fallback_line)
            .max(fallback_line);
        // A node merged from several files (a partial class declared in more
        // than one place) aggregates `endLine` across all its parts; take the
        // part that belongs to the node's primary file, never the aggregate.
        let parts = decl_slices(node);
        let (file, line, end_line) = parts
            .iter()
            .find(|(f, _, _)| *f == node_file)
            .cloned()
            .or_else(|| parts.first().cloned())
            .unwrap_or((node_file.clone(), fallback_line, fallback_end));
        if !lines_by_file.contains_key(&file) {
            let Some(text) = disk_text(index, &file) else {
                return json!({"ok": false, "error": format!("cannot read {file}")});
            };
            flags_by_file.insert(file.clone(), block_comment_starts(&text));
            lines_by_file.insert(file.clone(), text.split('\n').map(str::to_string).collect());
        }
        let lines = &lines_by_file[&file];
        let strs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let block_start = leading_block_start(&strs, line);
        if end_line as usize > lines.len() {
            return json!({"ok": false, "error": format!("{file}:{line} slice runs past the file")});
        }
        let name = str_of(node, "name").to_string();
        if name.is_empty() {
            return json!({"ok": false, "error": format!("{} has no simple name", str_of(node, "id"))});
        }
        let slice: Vec<String> = lines[(block_start - 1) as usize..end_line as usize].to_vec();
        // The graph may be older than the file (an `apply` without a following
        // `index` shifts line numbers). Never move a slice whose declaration no
        // longer mentions its own name: that would silently move a sibling.
        let mentions = slice.iter().enumerate().any(|(k, l)| {
            let ib = flags_by_file[&file][(block_start as usize - 1) + k];
            line_mentions_ident(l, ib, &name)
        });
        if !mentions {
            return json!({
                "ok": false,
                "error": format!(
                    "{file}:{block_start}-{end_line} no longer declares `{name}`; re-run `index` before moving"),
            });
        }
        let params: Vec<String> = node
            .get("params")
            .and_then(Value::as_array)
            .map(|a| a.iter().map(|p| p.as_str().unwrap_or("").to_string()).collect())
            .unwrap_or_default();
        let kind = str_of(node, "kind").to_string();
        let sig = if MEMBER_KINDS.contains(&kind.as_str()) {
            // The node id carries the real parameter list (`Full(string, int)`);
            // the `params` array is not always populated.
            let id = str_of(node, "id");
            let list = match (id.find('('), id.rfind(')')) {
                (Some(a), Some(b)) if b > a => id[a + 1..b].to_string(),
                _ => params.join(","),
            };
            format!("{name}({list})")
        } else {
            name.clone()
        };
        moved.push(Moved {
            id: str_of(node, "id").to_string(),
            file,
            block_start,
            end_line,
            name,
            old_ns: str_of(node, "ns").to_string(),
            full: str_of(node, "full").to_string(),
            kind,
            sig,
            insert_lines: slice,
            new_full: String::new(),
        });
    }

    // A member cannot live at namespace level. Resolve the destination type for
    // the extract form: explicit `class`, else inferred from a qualified
    // `newfile` stem, else refuse.
    let has_member = moved.iter().any(|m| MEMBER_KINDS.contains(&m.kind.as_str()));
    let has_type = moved.iter().any(|m| TYPE_KINDS.contains(&m.kind.as_str()));
    let class_name = if !rename_into && has_member {
        if let Some(c) = sget(args, "class") {
            if !valid_cs_identifier(c) || model::is_keyword(c) {
                return json!({"ok": false, "error": format!("{c:?} is not a legal C# class name")});
            }
            c.to_string()
        } else {
            match dest_raw.as_deref().and_then(infer_class_from_path) {
                Some(c) => c,
                None => {
                    return json!({
                        "ok": false,
                        "error": "member extraction needs a destination type: pass `class`, \
                                  or a qualified `newfile` stem (e.g. newfile NZK.VF.cs -> class VF)"
                    })
                }
            }
        }
    } else if rename_into {
        class.clone()
    } else {
        String::new()
    };
    if !rename_into && !class_name.is_empty() && !valid_cs_identifier(&class_name) {
        return json!({"ok": false, "error": format!("{class_name:?} is not a legal C# class name")});
    }

    // Names that must stay bare: the moved declarations themselves, the
    // destination type, anything already declared in that type, and — critically
    // — every identifier DECLARED inside the moved slice (parameters, locals,
    // generic parameters, the declaration/type names). Qualifying one of those
    // produced invalid C# such as `Has(string NZK.S.P.a, string b)`.
    let mut skip_names: BTreeSet<String> = moved.iter().map(|m| m.name.clone()).collect();
    if !class_name.is_empty() {
        skip_names.insert(class_name.clone());
    }
    if rename_into {
        let class_full = if ns.is_empty() { class.clone() } else { format!("{ns}.{class}") };
        for n in index.nodes() {
            if str_of(n, "owner") == class_full || str_of(n, "parent") == class_full {
                skip_names.insert(str_of(n, "name").to_string());
            }
        }
    }
    for m in &moved {
        let mut ib = flags_by_file[&m.file][(m.block_start - 1) as usize];
        let mut toks: Vec<Tok> = Vec::new();
        for l in &m.insert_lines {
            toks.extend(code_tokens(l, &mut ib));
        }
        skip_names.extend(declared_names(&toks, &m.name));
    }

    // Re-reference the moved block's own dependencies (siblings left behind in
    // the origin type) to their unique fully-qualified name, so the destination
    // still resolves. Ambiguous names are reported, never guessed.
    let (dep_map, dep_amb) = dependency_map(index);
    let mut unresolved: Vec<Value> = Vec::new();
    for m in moved.iter_mut() {
        let orig_lines = std::mem::take(&mut m.insert_lines);
        let mut in_block = flags_by_file[&m.file][(m.block_start - 1) as usize];
        let mut out: Vec<String> = Vec::with_capacity(orig_lines.len());
        for (i, l) in orig_lines.iter().enumerate() {
            let mut hits: Vec<Value> = Vec::new();
            let rename_new = member.clone();
            let m_name = m.name.clone();
            let m_file = m.file.clone();
            let m_start = m.block_start;
            let after = transform_line(l, &mut in_block, &mut |tok, dot, _follow, _prev| {
                if dot {
                    return None;
                }
                if rename_into && tok == m_name {
                    return Some(rename_new.clone());
                }
                if skip_names.contains(tok) {
                    return None;
                }
                if let Some(full) = dep_map.get(tok) {
                    return Some(full.clone());
                }
                if let Some(cands) = dep_amb.get(tok) {
                    hits.push(json!({
                        "name": tok,
                        "file": m_file,
                        "line": m_start + i as i64,
                        "candidates": cands,
                    }));
                }
                None
            });
            unresolved.extend(hits);
            out.push(after);
        }
        m.insert_lines = out;
        m.new_full = if rename_into {
            to.clone().unwrap_or_default()
        } else if MEMBER_KINDS.contains(&m.kind.as_str()) {
            if new_ns.is_empty() {
                format!("{class_name}.{}", m.name)
            } else {
                format!("{new_ns}.{class_name}.{}", m.name)
            }
        } else if new_ns.is_empty() {
            m.name.clone()
        } else {
            format!("{new_ns}.{}", m.name)
        };
    }

    // Dependency honesty: a moved block that still references an ambiguous
    // sibling would emit a file that does not resolve. Refuse by default;
    // `allow_unresolved:true` is the explicit escape hatch.
    unresolved.dedup_by(|a, b| a["name"] == b["name"]);
    let allow_unresolved = bget(args, "allow_unresolved", false);
    if !unresolved.is_empty() && !allow_unresolved {
        let names: Vec<String> = unresolved
            .iter()
            .map(|u| u["name"].as_str().unwrap_or("").to_string())
            .collect();
        return json!({
            "ok": false,
            "error": format!(
                "{} moved-block dependency name(s) cannot be uniquely resolved ({}); \
                 qualify them, or pass `allow_unresolved:true` to emit with warnings",
                unresolved.len(),
                names.join(", ")
            ),
            "unresolved": unresolved,
            "hint": "`allow_unresolved:true` emits anyway and records a loud `warnings` entry",
        });
    }

    // Two members with the same name + parameter list would be CS0111.
    if has_member {
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        let mut duplicates: Vec<Value> = Vec::new();
        for m in moved.iter().filter(|m| MEMBER_KINDS.contains(&m.kind.as_str())) {
            match seen.get(&m.sig) {
                Some(first) => {
                    duplicates.push(json!({"signature": m.sig, "first": first, "second": m.id}))
                }
                None => {
                    seen.insert(m.sig.clone(), m.id.clone());
                }
            }
        }
        if !duplicates.is_empty() {
            return json!({
                "ok": false,
                "error": "destination type would contain duplicate member signatures (CS0111)",
                "duplicates": duplicates,
            });
        }
    }

    if !rename_into && new_ns.is_empty() {
        let first = moved[0].old_ns.clone();
        if moved.iter().any(|m| m.old_ns != first) {
            return json!({
                "ok": false,
                "error": "selection spans several namespaces; pass `prefix`/`namespace` for the new home"
            });
        }
    }

    // ---- plan: deletes, call-site rewrites ----------------------------------
    let mut deletes: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
    let mut rewrites: BTreeMap<String, BTreeMap<i64, (String, String)>> = BTreeMap::new();
    let mut inserts: BTreeMap<String, Vec<(i64, &'static str, String)>> = BTreeMap::new();
    let mut new_files: BTreeMap<String, String> = BTreeMap::new();
    let mut ambiguous: Vec<Value> = Vec::new();
    let mut edits: Vec<Value> = Vec::new();

    for m in &moved {
        for l in m.block_start..=m.end_line {
            deletes.entry(m.file.clone()).or_default().insert(l);
        }
        edits.push(json!({
            "kind": "delete",
            "file": m.file,
            "from": m.block_start,
            "to": m.end_line,
            "lines": m.end_line - m.block_start + 1,
            "text": lines_by_file[&m.file][(m.block_start - 1) as usize],
        }));

        if name_is_ambiguous(index, &m.name) {
            ambiguous.push(json!({
                "id": m.id,
                "name": m.name,
                "reason": "the simple name is declared more than once; call sites left untouched",
            }));
            continue;
        }
        // Incoming usage edges attribute each site to this exact declaration.
        let mut site_lines: BTreeSet<(String, i64)> = BTreeSet::new();
        for e in index.edges_of(&m.id) {
            if e.get("target").and_then(Value::as_str) != Some(m.id.as_str()) {
                continue;
            }
            let source = e.get("source").and_then(Value::as_str).unwrap_or("");
            if source == m.id {
                continue;
            }
            let kind = e.get("kind").and_then(Value::as_str).unwrap_or("");
            if !USAGE_KINDS.contains(&kind) {
                continue;
            }
            let Some(src) = index.node_value(source) else {
                continue;
            };
            let sfile = str_of(src, "file").to_string();
            if sfile.is_empty() {
                continue;
            }
            let lines: Vec<i64> = e
                .get("lines")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            if lines.is_empty() {
                let l = src.get("line").and_then(Value::as_i64).unwrap_or(1).max(1);
                site_lines.insert((sfile, l));
            } else {
                for l in lines {
                    site_lines.insert((sfile.clone(), l));
                }
            }
        }
        for (sfile, sl) in site_lines {
            if deletes
                .get(&sfile)
                .map(|d| d.contains(&sl))
                .unwrap_or(false)
            {
                continue; // the usage line is itself moving
            }
            if !lines_by_file.contains_key(&sfile) {
                let Some(text) = disk_text(index, &sfile) else {
                    continue;
                };
                flags_by_file.insert(sfile.clone(), block_comment_starts(&text));
                lines_by_file.insert(sfile.clone(), text.split('\n').map(str::to_string).collect());
            }
            let lines = &lines_by_file[&sfile];
            if sl < 1 || sl as usize > lines.len() {
                continue;
            }
            let orig = lines[sl as usize - 1].clone();
            let mut in_block = flags_by_file[&sfile][sl as usize - 1];
            let after = transform_line(&orig, &mut in_block, &mut |tok, dot, _follow, _prev| {
                if tok == m.name && !dot {
                    Some(m.new_full.clone())
                } else {
                    None
                }
            });
            if after == orig {
                continue;
            }
            rewrites
                .entry(sfile.clone())
                .or_default()
                .insert(sl, (orig.clone(), after.clone()));
            edits.push(json!({
                "kind": "rewrite",
                "file": sfile,
                "line": sl,
                "before": orig,
                "after": after,
            }));
        }
    }

    // ---- plan: destination --------------------------------------------------
    let mut target_json = json!({});
    let dest_display: Option<String>;
    if rename_into {
        let m = &moved[0];
        let class_full = if ns.is_empty() {
            class.clone()
        } else {
            format!("{ns}.{class}")
        };
        let class_node = index.nodes().iter().find(|n| {
            str_of(n, "full") == class_full && TYPE_KINDS.contains(&str_of(n, "kind"))
        });
        let wrapper = {
            let body = m.insert_lines.join("\n");
            if ns.is_empty() {
                format!("public class {class} {{\n{body}\n}}\n")
            } else {
                format!("namespace {ns} {{\n public class {class} {{\n{body}\n }}\n}}\n")
            }
        };
        let (_dabs, ddisp, dexisted);
        let anchor: Option<i64>;
        if let Some(cn) = class_node {
            let cfile = str_of(cn, "file").to_string();
            if let Some(nf) = &dest_raw {
                let (_, ndisp, _) = match resolve_move_target(index, nf, optin) {
                    Ok(x) => x,
                    Err(e) => return json!({"ok": false, "error": e}),
                };
                if ndisp != cfile {
                    return json!({
                        "ok": false,
                        "error": format!("class {class_full} already exists in {cfile}, not {ndisp}")
                    });
                }
            }
            _dabs = match repo_abs(index, &cfile) {
                Some(p) => p,
                None => return json!({"ok": false, "error": format!("class {class_full} is outside the repo")}),
            };
            if let Err(e) = scope.check_optin(&_dabs, optin) {
                return json!({"ok": false, "error": e});
            }
            ddisp = cfile.clone();
            dexisted = true;
            anchor = cn.get("endLine").and_then(Value::as_i64).filter(|l| *l > 0);
        } else if let Some(nf) = &dest_raw {
            match resolve_move_target(index, nf, optin) {
                Ok(x) => {
                    _dabs = x.0;
                    ddisp = x.1;
                    dexisted = x.2;
                }
                Err(e) => return json!({"ok": false, "error": e}),
            }
            anchor = None;
        } else {
            _dabs = match repo_abs(index, &m.file) {
                Some(p) => p,
                None => return json!({"ok": false, "error": format!("{} is outside the repo", m.file)}),
            };
            if let Err(e) = scope.check_optin(&_dabs, optin) {
                return json!({"ok": false, "error": e});
            }
            ddisp = m.file.clone();
            dexisted = true;
            anchor = None;
        }
        target_json = json!({
            "to": m.new_full, "namespace": ns, "class": class, "member": member,
            "file": ddisp, "created_file": !dexisted, "existing_class": class_node.is_some(),
        });
        dest_display = Some(ddisp.clone());
        if !dexisted {
            let bytes = wrapper.len();
            new_files.insert(ddisp.clone(), wrapper.clone());
            edits.push(json!({"kind": "create", "file": ddisp, "bytes": bytes, "content": wrapper}));
        } else if class_node.is_some() {
            let text = m.insert_lines.join("\n");
            let anchor = anchor.unwrap_or(0);
            inserts
                .entry(ddisp.clone())
                .or_default()
                .push((anchor, "before", text.clone()));
            edits.push(json!({"kind": "insert", "file": ddisp, "line": anchor, "position": "before",
                              "text": text.clone(), "content": text}));
        } else {
            // Insert into the existing namespace header if there is one.
            let ns_end = if ns.is_empty() {
                None
            } else {
                index
                    .nodes()
                    .iter()
                    .find(|n| {
                        str_of(n, "kind") == "namespace"
                            && str_of(n, "full") == ns
                            && str_of(n, "file") == ddisp
                    })
                    .and_then(|n| n.get("endLine").and_then(Value::as_i64))
                    .filter(|l| *l > 0)
            };
            match ns_end {
                Some(line) => {
                    inserts
                        .entry(ddisp.clone())
                        .or_default()
                        .push((line, "before", wrapper.clone()));
                    edits.push(json!({"kind": "insert", "file": ddisp, "line": line, "position": "before",
                                      "text": wrapper.clone(), "content": wrapper}));
                }
                None => {
                    inserts
                        .entry(ddisp.clone())
                        .or_default()
                        .push((0, "end", wrapper.clone()));
                    edits.push(json!({"kind": "insert", "file": ddisp, "line": 0, "position": "end",
                                      "text": wrapper.clone(), "content": wrapper}));
                }
            }
        }
    } else {
        let raw = dest_raw.clone().unwrap_or_default();
        let (_dabs, ddisp, dexisted) = match resolve_move_target(index, &raw, optin) {
            Ok(x) => x,
            Err(e) => return json!({"ok": false, "error": e}),
        };
        let mods = args
            .get("modifiers")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("public static partial")
            .to_string();
        let head = if mods.is_empty() {
            format!("class {class_name}{{")
        } else {
            format!("{mods} class {class_name}{{")
        };
        let push_body = |block: &mut String, lines: &[String]| {
            block.push_str(&lines.join("\n"));
            block.push('\n');
        };
        let mut block = String::new();
        let pure_members = has_member && !has_type;
        if pure_members {
            // Toolkit dialect opener, exactly `namespace NS{  public static
            // partial class X{` on one line, closed by `}}`.
            if new_ns.is_empty() {
                block.push_str(&head);
                block.push('\n');
            } else {
                block.push_str(&format!("namespace {new_ns}{{  {head}\n"));
            }
            for m in &moved {
                push_body(&mut block, &m.insert_lines);
            }
            if new_ns.is_empty() {
                block.push_str("}\n");
            } else {
                block.push_str("}}\n");
            }
        } else {
            if !new_ns.is_empty() {
                block.push_str(&format!("namespace {new_ns}{{\n"));
            }
            for m in moved.iter().filter(|m| TYPE_KINDS.contains(&m.kind.as_str())) {
                push_body(&mut block, &m.insert_lines);
            }
            if has_member {
                let ind = if new_ns.is_empty() { "" } else { "  " };
                block.push_str(&format!("{ind}{head}\n"));
                for m in moved
                    .iter()
                    .filter(|m| MEMBER_KINDS.contains(&m.kind.as_str()))
                {
                    push_body(&mut block, &m.insert_lines);
                }
                block.push_str(&format!("{ind}}}\n"));
            }
            if !new_ns.is_empty() {
                block.push_str("}\n");
            }
        }
        target_json = json!({
            "namespace": new_ns, "newfile": ddisp, "created_file": !dexisted,
            "class": class_name, "modifiers": mods,
        });
        dest_display = Some(ddisp.clone());
        if !dexisted {
            new_files.insert(ddisp.clone(), block.clone());
            edits.push(json!({"kind": "create", "file": ddisp, "bytes": block.len(), "content": block}));
        } else {
            inserts
                .entry(ddisp.clone())
                .or_default()
                .push((0, "end", block.clone()));
            edits.push(json!({"kind": "insert", "file": ddisp, "line": 0, "position": "end",
                              "text": block.clone(), "content": block}));
        }
        let _ = _dabs;
    }

    // ---- validate + assemble every file before staging ----------------------
    // An insert may target a file that already exists on disk (a destination
    // used by an earlier move). It must be loaded so it is assembled and written
    // in the same all-or-nothing commit — otherwise the origin delete commits
    // while the destination insert is silently dropped (data loss). Refuse when
    // it cannot be read, before anything is staged.
    let insert_targets: Vec<String> = inserts.keys().cloned().collect();
    for f in insert_targets {
        if new_files.contains_key(&f) || lines_by_file.contains_key(&f) {
            continue;
        }
        match disk_text(index, &f) {
            Some(text) => {
                flags_by_file.insert(f.clone(), block_comment_starts(&text));
                lines_by_file.insert(f.clone(), text.split('\n').map(str::to_string).collect());
            }
            None => {
                return json!({
                    "ok": false,
                    "error": format!("cannot read existing destination {f}; refusing to move"),
                })
            }
        }
    }
    let mut writes: Vec<MoveFile> = Vec::new();
    for (file, lines) in &lines_by_file {
        let dels = deletes.get(file);
        let rw = rewrites.get(file);
        let ins = inserts.get(file);
        if dels.is_none() && rw.is_none() && ins.is_none() {
            continue;
        }
        if let Some(rw) = rw {
            for (ln, (before, _)) in rw {
                if *ln < 1 || *ln as usize > lines.len() {
                    return json!({"ok": false, "error": format!("{file}:{ln} is out of range")});
                }
                if lines[*ln as usize - 1] != *before {
                    return json!({"ok": false, "error": format!("{file}:{ln} changed on disk since selection")});
                }
            }
        }
        if let Some(dels) = dels {
            for ln in dels {
                if *ln < 1 || *ln as usize > lines.len() {
                    return json!({"ok": false, "error": format!("{file}:{ln} is out of range")});
                }
            }
        }
        let mut new_lines: Vec<String> = Vec::new();
        let mut idx_map: BTreeMap<i64, usize> = BTreeMap::new();
        for (i, l) in lines.iter().enumerate() {
            let ln = i as i64 + 1;
            if dels.map(|d| d.contains(&ln)).unwrap_or(false) {
                continue;
            }
            idx_map.insert(ln, new_lines.len());
            match rw.and_then(|r| r.get(&ln)) {
                Some((_, after)) => new_lines.push(after.clone()),
                None => new_lines.push(l.clone()),
            }
        }
        if let Some(ins) = ins {
            let mut ins = ins.clone();
            ins.sort_by(|a, b| b.0.cmp(&a.0));
            for (anchor, pos, text) in ins {
                if pos == "before" {
                    let at = idx_map.get(&anchor).copied().unwrap_or(new_lines.len());
                    for (k, part) in text.split('\n').enumerate() {
                        new_lines.insert(at + k, part.to_string());
                    }
                } else {
                    for part in text.split('\n') {
                        new_lines.push(part.to_string());
                    }
                }
            }
        }
        let abs = match repo_abs(index, file) {
            Some(p) => p,
            None => {
                return json!({"ok": false, "error": format!("refusing to write outside the indexed repo: {file}")})
            }
        };
        if let Err(e) = scope.check_optin(&abs, optin) {
            return json!({"ok": false, "error": e});
        }
        writes.push(MoveFile {
            display: file.clone(),
            abs,
            existed: true,
            content: new_lines.join("\n"),
            original: Some(lines.join("\n")),
        });
    }
    for (display, content) in &new_files {
        let (abs, _, _) = match resolve_move_target(index, display, optin) {
            Ok(x) => x,
            Err(e) => return json!({"ok": false, "error": e}),
        };
        writes.push(MoveFile {
            display: display.clone(),
            abs,
            existed: false,
            content: content.clone(),
            original: None,
        });
    }
    writes.sort_by(|a, b| a.display.cmp(&b.display));

    let selected: Vec<Value> = moved
        .iter()
        .map(|m| {
            json!({
                "id": m.id, "full": m.full, "name": m.name, "kind": m.kind,
                "file": m.file, "blockStart": m.block_start, "endLine": m.end_line,
                "newFull": m.new_full,
            })
        })
        .collect();
    let files: Vec<String> = writes.iter().map(|w| w.display.clone()).collect();
    // ---- Roslyn parse gate over the candidate content -----------------------
    // The point of the M0 validity work: an apply must never land syntactically
    // invalid C#. Candidate bytes are parked in the OS temp dir (never the
    // corpus), parsed by the real Roslyn driver, and diagnostics are mapped back
    // to the real display files. A missing toolchain is reported as
    // `validated:"skipped:<reason>"` + `unverified:true`, never as a pass.
    let require_validated = bget(args, "require_validated", false);
    let mut validated = "not-run".to_string();
    let mut unverified = false;
    let mut invalid: Vec<Value> = Vec::new();
    if apply && !writes.is_empty() {
        match validate_candidates(&writes, sget(args, "_dotnet")) {
            Ok(report) => {
                let policy = crate::compile_check::validity_policy(&report, require_validated);
                validated = policy.validated.clone();
                unverified = policy.unverified;
                if let Some(refusal) = policy.refusal {
                    return refusal;
                }
                invalid = report["diagnostics"].as_array().cloned().unwrap_or_default();
                if !invalid.is_empty() && !allow_invalid {
                    return json!({
                        "ok": false,
                        "validated": validated,
                        "unverified": unverified,
                        "error": format!(
                            "the commit would leave {} Roslyn parse diagnostic(s); nothing written \
                             (pass `allow_invalid:true` to override)",
                            invalid.len()
                        ),
                        "diagnostics": invalid,
                        "files": files,
                    });
                }
            }
            Err(e) => {
                return json!({"ok": false, "error": format!("candidate validation failed: {e}")})
            }
        }
    }
    let mut warnings: Vec<String> = unresolved
        .iter()
        .map(|u| {
            format!(
                "{}.{}: `{}` is ambiguous ({} candidates); left unqualified",
                u["file"].as_str().unwrap_or(""),
                u["line"].as_i64().unwrap_or(0),
                u["name"].as_str().unwrap_or(""),
                u["candidates"].as_array().map(|a| a.len()).unwrap_or(0),
            )
        })
        .collect();
    if !invalid.is_empty() {
        warnings.push(format!(
            "allow_invalid: {} Roslyn parse diagnostic(s) ignored and written",
            invalid.len()
        ));
    }
    let mut body = json!({
        "ok": true,
        "tool": "move_decls",
        "mode": if rename_into { "rename-into" } else { "extract" },
        "target": target_json,
        "selected": selected,
        "files": files,
        "edits": edits,
        "ambiguous": ambiguous,
        "unresolved": unresolved,
        "warnings": warnings,
        "validated": validated,
        "unverified": unverified,
        "applied": false,
        "written": [],
        "created": [],
        "reindex_required": true,
        "note": "after `apply`, call `index` to re-sync the graph with the files on disk",
    });
    if !invalid.is_empty() {
        body["diagnostics"] = json!(invalid);
    }
    if let Some(d) = &dest_display {
        let content = writes
            .iter()
            .find(|w| &w.display == d)
            .map(|w| w.content.clone())
            .unwrap_or_default();
        body["destination"] = json!({
            "file": d,
            "created": new_files.contains_key(d),
            "content": content,
        });
    }
    if apply {
        match commit_move_files(&writes) {
            Ok((written, created)) => {
                body["applied"] = json!(true);
                body["written"] = json!(written);
                body["created"] = json!(created);
            }
            Err(e) => {
                body["ok"] = json!(false);
                body["error"] = json!(e);
            }
        }
    }
    body
}

// ---------------------------------------------------------------------------
// 10. grep
// ---------------------------------------------------------------------------

/// A `glob`/`paths` argument with its case preserved: an array of strings or a
/// comma-separated string.
pub(crate) fn raw_string_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Translate a shell-style glob to an anchored regex. `**` crosses `/`, `*`
/// stays within one path segment, `?` is one non-`/` character. Everything else
/// is escaped, so a glob cannot smuggle in a regex.
fn glob_to_regex(pat: &str) -> String {
    let chars: Vec<char> = pat.chars().collect();
    let mut out = String::from("^");
    let mut i = 0usize;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    out.push_str(".*");
                    i += 2;
                    // `**/x` also matches a path that starts at `x`.
                    if i < chars.len() && chars[i] == '/' {
                        i += 1;
                    }
                } else {
                    out.push_str("[^/]*");
                    i += 1;
                }
            }
            '?' => {
                out.push_str("[^/]");
                i += 1;
            }
            c => {
                out.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }
    out.push('$');
    out
}

/// Compiled globs as `(regex, matches_full_path)`: a pattern containing `/`
/// matches the whole display path, otherwise it matches the basename.
fn compile_globs(args: &Value) -> Result<Vec<(regex::Regex, bool)>, String> {
    let pats = raw_string_list(args, "glob");
    if pats.len() > 32 {
        return Err("too many glob patterns (max 32)".to_string());
    }
    let mut out = Vec::new();
    for p in &pats {
        let full = p.contains('/');
        let re = regex::Regex::new(&glob_to_regex(p))
            .map_err(|e| format!("bad glob {p:?}: {e}"))?;
        out.push((re, full));
    }
    Ok(out)
}

fn scopes_match(scopes: &[String], file: &str) -> bool {
    if scopes.is_empty() {
        return true;
    }
    scopes.iter().any(|s| {
        let s = s.trim_end_matches('/');
        file == s || file.starts_with(&format!("{s}/"))
    })
}

/// `grep{pattern, glob?, path?/paths?, limit?, max_per_file?, case?, literal?,
/// count_only?, context?}` — a first-class regex/text search over the indexed
/// corpus, so a shell `grep -n` one-liner is expressible as one MCP call.
///
/// The pattern is a Rust/RE2-style regex (linear time, no backtracking), so
/// there is no catastrophic-backtracking hazard; `literal:true` escapes it.
/// `case` defaults to `true` (meaning *ignore* case); `case:false` is
/// case-sensitive. Files and lines are ordered deterministically, and the scan
/// never walks the disk: it reads only [`Index::file_paths`] /
/// [`Index::file_text_of`].
fn grep(index: &Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let pattern = match required_str(args, "pattern") {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    if pattern.len() > 4096 {
        return json!({"ok": false, "error": "pattern too long (max 4096 bytes)"});
    }
    let literal = bget(args, "literal", false);
    let case_insensitive = bget(args, "case", true);
    let count_only = bget(args, "count_only", false);
    let limit = iget(args, "limit", 200).clamp(1, 20_000) as usize;
    let max_per_file = iget(args, "max_per_file", 50).clamp(1, 5_000) as usize;
    let context = iget(args, "context", 0).clamp(0, 10) as usize;

    let pat = if literal { regex::escape(&pattern) } else { pattern.clone() };
    let re = match regex::RegexBuilder::new(&pat)
        .case_insensitive(case_insensitive)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
    {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": format!("invalid pattern: {e}")}),
    };
    let globs = match compile_globs(args) {
        Ok(g) => g,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let mut scopes: Vec<String> = string_list(args, "paths")
        .iter()
        .map(|s| normalise_scope(index, s))
        .collect();
    if let Some(p) = sget(args, "path") {
        scopes.push(normalise_scope(index, p));
    }
    scopes.sort();
    scopes.dedup();

    let mut files_out: Vec<Value> = Vec::new();
    let mut total_matches: i64 = 0;
    let mut rows_emitted = 0usize;
    let mut truncated = false;
    let all_paths = index.file_paths();
    for file in all_paths {
        if !scopes_match(&scopes, &file) {
            continue;
        }
        if !globs.is_empty() {
            let base = file.rsplit('/').next().unwrap_or(&file);
            let hits = globs
                .iter()
                .any(|(re, full)| re.is_match(if *full { &file } else { base }));
            if !hits {
                continue;
            }
        }
        let Some(text) = index.file_text_of(&file) else {
            continue;
        };
        let lines: Vec<&str> = text.split('\n').collect();
        let mut file_count: i64 = 0;
        let mut rows: Vec<Value> = Vec::new();
        for (i, raw_line) in lines.iter().enumerate() {
            let line = raw_line.trim_end_matches('\r');
            let mut line_hits = 0usize;
            for m in re.find_iter(line) {
                file_count += 1;
                line_hits += 1;
                if count_only || rows.len() >= max_per_file {
                    if !count_only {
                        truncated = true;
                    }
                    continue;
                }
                let col = line[..m.start()].chars().count() + 1;
                let mut row = json!({"line": i as i64 + 1, "col": col, "text": line});
                if context > 0 {
                    let a = i.saturating_sub(context);
                    let b = (i + context + 1).min(lines.len());
                    row["before"] = json!(lines[a..i]
                        .iter()
                        .map(|s| s.trim_end_matches('\r'))
                        .collect::<Vec<_>>());
                    row["after"] = json!(lines[i + 1..b]
                        .iter()
                        .map(|s| s.trim_end_matches('\r'))
                        .collect::<Vec<_>>());
                }
                rows.push(row);
            }
            let _ = line_hits;
        }
        if file_count == 0 {
            continue;
        }
        total_matches += file_count;
        if count_only && files_out.len() >= limit {
            truncated = true;
            break;
        }
        files_out.push(json!({"file": file, "count": file_count, "matches": rows}));
        rows_emitted += rows.len();
        if !count_only && rows_emitted >= limit {
            truncated = true;
            break;
        }
    }

    json!({
        "ok": true,
        "pattern": pattern,
        "literal": literal,
        "case": case_insensitive,
        "count_only": count_only,
        "total_matches": total_matches,
        "file_count": files_out.len(),
        "files": files_out,
        "truncated": truncated,
    })
}

// ---------------------------------------------------------------------------
// 11. parse_check (MCP surface for the Roslyn validity gate)
// ---------------------------------------------------------------------------

/// One `exclude` entry: a shell glob (contains `*`/`?`) matched against the
/// display path, or a repo-relative file/directory prefix.
fn exclude_hits(pat: &str, file: &str) -> bool {
    if pat.contains('*') || pat.contains('?') {
        let full = pat.contains('/');
        let base = file.rsplit('/').next().unwrap_or(file);
        return regex::Regex::new(&glob_to_regex(pat))
            .map(|re| re.is_match(if full { file } else { base }))
            .unwrap_or(false);
    }
    let p = pat.trim_end_matches('/');
    file == p || file.starts_with(&format!("{p}/"))
}

/// `parse_check{files?, paths?, exclude?, timeout?}` — run the local
/// [`crate::compile_check`] Roslyn gate over the indexed corpus (or the named
/// subset) and return that report verbatim: `{ok, toolchain, files,
/// diagnostics, count, skipped}`.
///
/// This is the **same** call the `csgraph parse-check` CLI makes — no
/// re-implementation. `ok:false` with a concrete `skipped` reason when no
/// Roslyn toolchain/driver is available, never a false pass. The file list is
/// taken from [`Index::file_paths`] (the indexed corpus), so it never walks the
/// disk; `files`/`paths` scope it and `exclude` (globs or prefixes) subtracts.
fn parse_check_tool(index: &Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let timeout = iget(args, "timeout", 25).clamp(1, 30) as u64;
    let files_scopes: Vec<String> = string_list(args, "files")
        .iter()
        .map(|f| normalise_scope(index, f))
        .collect();
    let paths_scopes: Vec<String> = string_list(args, "paths")
        .iter()
        .map(|p| normalise_scope(index, p))
        .collect();
    let excludes = raw_string_list(args, "exclude");
    let root = index.repo_path();
    let mut selected: Vec<String> = Vec::new();
    for file in index.file_paths() {
        if !files_scopes.is_empty() {
            let base = file.rsplit('/').next().unwrap_or(&file);
            let hit = scopes_match(&files_scopes, &file)
                || files_scopes.iter().any(|f| !f.contains('/') && f == base);
            if !hit {
                continue;
            }
        }
        if !paths_scopes.is_empty() && !scopes_match(&paths_scopes, &file) {
            continue;
        }
        if excludes.iter().any(|e| exclude_hits(e, &file)) {
            continue;
        }
        let abs = root.join(&file);
        if abs.is_file() {
            selected.push(abs.to_string_lossy().into_owned());
        }
    }
    selected.sort();
    selected.dedup();
    crate::compile_check::parse_check(&selected, timeout)
}

/// `compile_diff{files?, paths?, exclude?, baseline_exclude?, data?, refs?,
/// define?, timeout?}` — the differential Roslyn compile, MCP-native.
///
/// This is the in-process half of `csgraph compile-diff`: it compiles the
/// selected corpus (candidate) and the same corpus minus every file matching
/// `baseline_exclude` (baseline) against the SAME reference set, and reports
/// only the diagnostics the candidate ADDS. The reference set is the local
/// Unity install's `Editor/Data` assemblies when `data` (or
/// `CSVIEWER_UNITY_DATA`, or auto-discovery) names one; otherwise the SDK's own
/// pack. Nothing here launches the Unity Editor, takes a licence, or touches
/// window focus: it drives the same offline Roslyn compiler `parse_check` uses,
/// so a workspace can be validated on a box where Unity is closed, unlicensed or
/// absent. `toolchain: null` + `skipped` means no compiler exists — never a pass.
fn compile_diff_tool(index: &Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let timeout = iget(args, "timeout", 25).clamp(1, 90) as u64;
    let files_scopes: Vec<String> = string_list(args, "files")
        .iter()
        .map(|f| normalise_scope(index, f))
        .collect();
    let paths_scopes: Vec<String> = string_list(args, "paths")
        .iter()
        .map(|p| normalise_scope(index, p))
        .collect();
    let excludes = raw_string_list(args, "exclude");
    let baseline_excludes = raw_string_list(args, "baseline_exclude");
    let root = index.repo_path();
    let mut selected: Vec<(String, String)> = Vec::new();
    for file in index.file_paths() {
        if !files_scopes.is_empty() {
            let base = file.rsplit('/').next().unwrap_or(&file);
            let hit = scopes_match(&files_scopes, &file)
                || files_scopes.iter().any(|f| !f.contains('/') && f == base);
            if !hit {
                continue;
            }
        }
        if !paths_scopes.is_empty() && !scopes_match(&paths_scopes, &file) {
            continue;
        }
        if excludes.iter().any(|e| exclude_hits(e, &file)) {
            continue;
        }
        let abs = root.join(&file);
        if !abs.is_file() {
            continue;
        }
        match fs::read(&abs) {
            Ok(bytes) => selected.push((
                file.clone(),
                String::from_utf8_lossy(&bytes).into_owned(),
            )),
            // A source the engine indexed but cannot read must not silently
            // shrink the compared tree: refuse rather than report a clean pass.
            Err(e) => {
                return json!({
                    "ok": false,
                    "error": format!("cannot read {file}: {e}"),
                })
            }
        }
    }
    selected.sort_by(|a, b| a.0.cmp(&b.0));
    if selected.is_empty() {
        return json!({"ok": false, "error": "no C# files match the selection"});
    }
    let mut original: HashMap<String, String> = HashMap::new();
    for (rel, text) in &selected {
        if baseline_excludes.iter().any(|e| exclude_hits(e, rel)) {
            continue;
        }
        original.insert(format!("0/{rel}"), text.clone());
    }
    if original.is_empty() {
        // `differential` needs a baseline that compiles into something; the same
        // stub the `sps_gate.sh` wrapper uses.
        original.insert("0/GateStub.cs".to_string(), "class GateStub {}\n".to_string());
    }
    let mut emitted: Vec<(String, String)> = Vec::new();
    for (rel, text) in &selected {
        emitted.push((format!("1/{rel}"), text.clone()));
    }
    let mut defines: Vec<String> = crate::compile_check::DEFAULT_DEFINES
        .iter()
        .map(|d| (*d).to_string())
        .collect();
    defines.extend(raw_string_list(args, "define"));
    let refs = raw_string_list(args, "refs");
    let data = match args.get("data").and_then(Value::as_str) {
        Some(d) if !d.is_empty() => Some(d.to_string()),
        _ => crate::compile_check::find_unity_data(),
    };
    crate::compile_check::compile_diff_report(
        &original,
        &emitted,
        data.as_deref(),
        &defines,
        &refs,
        timeout,
    )
}

/// `find_duplicates{min_lines?, threshold?, limit?, roots?/paths?, exclude?,
/// max_clusters?, max_members?, max_blocks?}` — the owner's priority-(2)
/// duplicate-code report, reachable without a shell.
///
/// It calls [`crate::dedupe::dedupe`] directly (the exact function the
/// `csgraph dedupe` CLI prints) and returns that JSON verbatim, so the MCP body
/// is byte-identical to the CLI for the same inputs. `min_lines`/`threshold`
/// map to the CLI flags; `limit` caps the returned clusters (and blocks) via
/// `DedupeOptions::max_clusters`/`max_blocks`, with `max_*` as explicit
/// overrides. The search roots default to the roots the index was built with.
fn find_duplicates(index: &Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let mut opts = crate::dedupe::DedupeOptions::default();
    if let Some(n) = args.get("min_lines").and_then(Value::as_i64) {
        if n < 1 {
            return json!({"ok": false, "error": "min_lines must be at least 1"});
        }
        opts.min_lines = n as usize;
    }
    if let Some(t) = args.get("threshold").and_then(Value::as_f64) {
        if t.is_nan() || !(0.0..=1.0).contains(&t) {
            return json!({"ok": false, "error": "threshold must be within 0..=1"});
        }
        opts.threshold = t;
    }
    if let Some(limit) = args.get("limit").and_then(Value::as_i64) {
        if limit < 1 {
            return json!({"ok": false, "error": "limit must be at least 1"});
        }
        opts.max_clusters = limit as usize;
        opts.max_blocks = limit as usize;
    }
    if let Some(v) = args.get("max_clusters").and_then(Value::as_i64) {
        opts.max_clusters = v.max(1) as usize;
    }
    if let Some(v) = args.get("max_members").and_then(Value::as_i64) {
        opts.max_members = v.max(1) as usize;
    }
    if let Some(v) = args.get("max_blocks").and_then(Value::as_i64) {
        opts.max_blocks = v.max(1) as usize;
    }
    let mut roots = raw_string_list(args, "roots");
    if roots.is_empty() {
        roots = string_list(args, "paths");
    }
    if roots.is_empty() {
        roots = index.roots().to_vec();
    }
    if roots.is_empty() {
        roots.push(".".to_string());
    }
    let excludes = raw_string_list(args, "exclude");
    // Same function the CLI calls: identical body, deterministic ordering,
    // `stats`/`truncated` surfaced as-is (never a false empty result).
    crate::dedupe::dedupe(index.repo_path(), &roots, &excludes, &opts)
}

/// Group `edges` by `kind`, counting rows. Deterministic (`BTreeMap`).
fn edge_kind_counts<'a, I: Iterator<Item = &'a Value>>(edges: I) -> BTreeMap<String, i64> {
    let mut out: BTreeMap<String, i64> = BTreeMap::new();
    for e in edges {
        let k = str_of(e, "kind").to_string();
        *out.entry(k).or_insert(0) += 1;
    }
    out
}

/// The display file of a node id, when the node exists and names a file.
fn node_file_of<'a>(index: &'a Index, id: &str) -> Option<&'a str> {
    index
        .node_value(id)
        .map(|n| str_of(n, "file"))
        .filter(|f| !f.is_empty())
}

/// The optional file scope of a `counts` request: repo-relative `roots`/`paths`
/// prefixes (plus the `file` sugar) and `exclude` globs or prefixes.
struct CountsScope {
    roots: Vec<String>,
    excludes: Vec<String>,
}

impl CountsScope {
    /// True when any scoping argument was supplied, so the response carries the
    /// scoped keys (`filter`, `scoped`, `byRoot`).
    fn requested(&self) -> bool {
        !self.roots.is_empty() || !self.excludes.is_empty()
    }

    /// True when `file` (a display path) is inside the requested roots and not
    /// subtracted by any exclude pattern. An empty scope accepts everything.
    fn file_ok(&self, file: &str) -> bool {
        scopes_match(&self.roots, file) && !self.excludes.iter().any(|e| exclude_hits(e, file))
    }

    /// Scope test for a node's file: the node must live in a real (non-empty)
    /// display file that passes the scope. A node with no file cannot be
    /// attributed to any file scope.
    fn node_ok(&self, file: &str) -> bool {
        !file.is_empty() && self.file_ok(file)
    }

    /// True when `file` belongs to one specific requested root.
    fn root_ok(&self, file: &str, root: &str) -> bool {
        let one = [root.to_string()];
        !file.is_empty()
            && scopes_match(&one, file)
            && !self.excludes.iter().any(|e| exclude_hits(e, file))
    }

    /// True when the node named by edge `e`'s `key` endpoint is inside the scope.
    fn endpoint_ok(&self, index: &Index, e: &Value, key: &str) -> bool {
        match node_file_of(index, str_of(e, key)) {
            Some(f) => self.file_ok(f),
            None => false,
        }
    }

    /// True when either endpoint of `e` is inside the scope.
    fn edge_ok(&self, index: &Index, e: &Value) -> bool {
        self.endpoint_ok(index, e, "source") || self.endpoint_ok(index, e, "target")
    }
}

/// Parse the optional `roots`/`paths`/`exclude`/`file` scope of a `counts`
/// request. An unparseable exclude glob is refused (`ok:false`), never silently
/// ignored.
fn parse_counts_scope(index: &Index, args: &Value) -> Result<CountsScope, String> {
    let mut roots: Vec<String> = raw_string_list(args, "roots");
    if roots.is_empty() {
        roots = raw_string_list(args, "paths");
    }
    if let Some(f) = sget(args, "file") {
        let f = f.trim();
        if !f.is_empty() {
            roots.push(f.to_string());
        }
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let roots: Vec<String> = roots
        .iter()
        .map(|s| normalise_scope(index, s))
        .filter(|s| !s.is_empty() && seen.insert(s.clone()))
        .collect();
    let excludes = raw_string_list(args, "exclude");
    for p in &excludes {
        if p.contains('*') || p.contains('?') {
            regex::Regex::new(&glob_to_regex(p))
                .map_err(|e| format!("bad exclude glob {p:?}: {e}"))?;
        }
    }
    Ok(CountsScope { roots, excludes })
}

/// `counts {q?, node?, roots?/paths?, exclude?, file?}` — the owner's gap 4:
/// first-class counts in one MCP call without adding a single field to the
/// frozen `find`/`refs` `/api/*` payloads.
///
/// With no `q`/`node` it summarises the whole index (`files`, `nodes`,
/// `declarations`, `byKind`, `edges`, `byEdgeKind`); with a scope argument the
/// same keys are computed over the selected files only and `filter`/`scoped`/
/// `byRoot` are added. With a symbol it resolves exactly one node (id, exact
/// full/label/simple name, then `resolve_declaration`) and reports its
/// attributed graph counts, restricted to the scope when one is given.
/// Ambiguity is refused with the candidate rows.
fn counts_tool(index: &Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let scope = match parse_counts_scope(index, args) {
        Ok(s) => s,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let scoped = scope.requested();

    let q = sget(args, "q").or_else(|| sget(args, "node")).map(str::trim);
    let Some(q) = q.filter(|q| !q.is_empty()) else {
        let file_paths = index.file_paths();
        let files = if scoped {
            file_paths.iter().filter(|f| scope.file_ok(f)).count()
        } else {
            file_paths.len()
        };
        let mut by_kind: BTreeMap<String, i64> = BTreeMap::new();
        let mut declarations = 0i64;
        let mut nodes = 0i64;
        for n in index.nodes() {
            if scoped && !scope.node_ok(str_of(n, "file")) {
                continue;
            }
            nodes += 1;
            *by_kind.entry(str_of(n, "kind").to_string()).or_insert(0) += 1;
            if index.is_declaration_id(str_of(n, "id")) {
                declarations += 1;
            }
        }
        let mut edges = 0i64;
        let mut by_edge_kind: BTreeMap<String, i64> = BTreeMap::new();
        for e in index.edges() {
            if scoped && !scope.edge_ok(index, e) {
                continue;
            }
            edges += 1;
            *by_edge_kind
                .entry(str_of(e, "kind").to_string())
                .or_insert(0) += 1;
        }
        if !scoped {
            // No scope: byte-identical to the historical body (same keys).
            return json!({
                "ok": true,
                "tool": "counts",
                "scope": "corpus",
                "repo": index.repo_path().to_string_lossy(),
                "roots": index.roots(),
                "files": index.file_paths().len(),
                "nodes": index.nodes().len(),
                "declarations": declarations,
                "byKind": by_kind,
                "edges": index.edges().len(),
                "byEdgeKind": by_edge_kind,
            });
        }
        // Per-root split: one entry per requested root, counting independently
        // of the other roots (an edge counts when either endpoint is in it).
        let mut by_root: serde_json::Map<String, Value> = serde_json::Map::new();
        for r in &scope.roots {
            let mut rfiles = 0i64;
            let mut rnodes = 0i64;
            let mut redges = 0i64;
            let mut rdeclarations = 0i64;
            for f in &file_paths {
                if scope.root_ok(f, r) {
                    rfiles += 1;
                }
            }
            for n in index.nodes() {
                if scope.root_ok(str_of(n, "file"), r) {
                    rnodes += 1;
                    if index.is_declaration_id(str_of(n, "id")) {
                        rdeclarations += 1;
                    }
                }
            }
            for e in index.edges() {
                // Per-root: an edge counts for `r` when either endpoint's own
                // file is inside `r` — not merely inside some requested root.
                let src_here = node_file_of(index, str_of(e, "source"))
                    .map(|f| scope.root_ok(f, r))
                    .unwrap_or(false);
                let dst_here = node_file_of(index, str_of(e, "target"))
                    .map(|f| scope.root_ok(f, r))
                    .unwrap_or(false);
                if src_here || dst_here {
                    redges += 1;
                }
            }
            by_root.insert(
                r.clone(),
                json!({"files": rfiles, "nodes": rnodes, "edges": redges,
                       "declarations": rdeclarations}),
            );
        }
        let filter_roots = scope.roots.clone();
        let filter_excludes = scope.excludes.clone();
        return json!({
            "ok": true,
            "tool": "counts",
            "scope": "corpus",
            "repo": index.repo_path().to_string_lossy(),
            "roots": scope.roots,
            "files": files,
            "nodes": nodes,
            "declarations": declarations,
            "byKind": by_kind,
            "edges": edges,
            "byEdgeKind": by_edge_kind,
            "filter": {"roots": filter_roots, "exclude": filter_excludes},
            "scoped": true,
            "byRoot": by_root,
        });
    };

    // Exact id / full / label first (a namespace or a string-literal node is a
    // legitimate subject too), then the declaration resolver.
    let mut exact: Vec<&Value> = index
        .nodes()
        .iter()
        .filter(|n| {
            str_of(n, "id") == q || str_of(n, "full") == q || str_of(n, "label") == q
        })
        .collect();
    exact.dedup_by(|a, b| a["id"] == b["id"]);
    let node: Value = if exact.len() == 1 {
        exact[0].clone()
    } else if exact.len() > 1 {
        return json!({
            "ok": false,
            "error": format!("{q:?} matches {} nodes", exact.len()),
            "ambiguity": exact.iter().map(|n| node_row(n)).collect::<Vec<_>>(),
        });
    } else {
        let (id, candidates) = resolve_declaration(index, q);
        match id {
            Some(id) => match index.node_value(&id) {
                Some(n) => n.clone(),
                None => return json!({"ok": false, "error": format!("{id} is not indexed")}),
            },
            None => {
                return json!({
                    "ok": false,
                    "error": format!("{q:?} does not name exactly one node"),
                    "candidates": candidates,
                })
            }
        }
    };
    let id = str_of(&node, "id");
    let full = str_of(&node, "full");
    let label = str_of(&node, "label");

    // Definitions: every declaration node that shares this full name (and, when
    // a scope was requested, lives in a selected file).
    let definitions = index
        .nodes()
        .iter()
        .filter(|n| {
            !full.is_empty()
                && str_of(n, "full") == full
                && index.is_declaration_id(str_of(n, "id"))
                && (!scoped || scope.node_ok(str_of(n, "file")))
        })
        .count() as i64;
    let matches = index
        .nodes()
        .iter()
        .filter(|n| {
            ((!full.is_empty() && str_of(n, "full") == full)
                || (!label.is_empty() && str_of(n, "label") == label))
                && (!scoped || scope.node_ok(str_of(n, "file")))
        })
        .count() as i64;

    // Incoming/outgoing edges are kept only when the *other* endpoint (the
    // usage site for an incoming edge, the callee for an outgoing one) is in
    // scope; the subject's own file never decides the edge's scope.
    let mut incoming_edges: Vec<&Value> = Vec::new();
    let mut outgoing_edges: Vec<&Value> = Vec::new();
    for e in index.edges() {
        let src = str_of(e, "source");
        let dst = str_of(e, "target");
        if dst == id && (!scoped || scope.endpoint_ok(index, e, "source")) {
            incoming_edges.push(e);
        }
        if src == id && (!scoped || scope.endpoint_ok(index, e, "target")) {
            outgoing_edges.push(e);
        }
    }
    let incoming = edge_kind_counts(incoming_edges.iter().copied());
    let outgoing = edge_kind_counts(outgoing_edges.iter().copied());
    let usages: i64 = incoming
        .iter()
        .filter(|(k, _)| USAGE_KINDS.contains(&k.as_str()))
        .map(|(_, v)| *v)
        .sum();
    let mut callers: BTreeSet<String> = BTreeSet::new();
    let mut usage_lines = 0i64;
    for e in &incoming_edges {
        if str_of(e, "kind") == "calls" {
            let src = str_of(e, "source");
            if !src.is_empty() {
                callers.insert(src.to_string());
            }
        }
        if USAGE_KINDS.contains(&str_of(e, "kind")) {
            usage_lines += e
                .get("lines")
                .and_then(Value::as_array)
                .map(|a| a.len() as i64)
                .unwrap_or(0);
        }
    }
    let mut body = json!({
        "ok": true,
        "tool": "counts",
        "scope": "symbol",
        "symbol": {
            "id": id, "full": full, "label": label, "name": str_of(&node, "name"),
            "kind": str_of(&node, "kind"), "file": str_of(&node, "file"),
            "line": node.get("line").and_then(Value::as_i64).unwrap_or(0),
            "degree": node.get("degree").and_then(Value::as_i64).unwrap_or(0),
        },
        "definitions": definitions,
        "matches": matches,
        "usages": usages,
        "incomingTotal": incoming_edges.len(),
        "outgoingTotal": outgoing_edges.len(),
        "incoming": incoming,
        "outgoing": outgoing,
        "callers": callers.len(),
        "usageLines": usage_lines,
    });
    if scoped {
        body["filter"] = json!({"roots": scope.roots, "exclude": scope.excludes});
    }
    body
}

/// `workspace {}` — the single-repo fallback body for the `workspace` tool.
///
/// The MCP server intercepts `workspace` in `mcp.rs` and answers from its whole
/// bound set (repeatable `--repo DIR` plus a per-call `repo` selector). This
/// helper is reachable only through the raw [`invoke_aux`] surface, where it
/// reports the one repo the `Index` it was handed is bound to.
fn workspace_tool(index: &Index, _args: &Value) -> Value {
    let repo = index.repo_path().to_string_lossy().to_string();
    let roots: Vec<String> = if index.roots().is_empty() {
        vec![".".to_string()]
    } else {
        index.roots().to_vec()
    };
    json!({
        "ok": true,
        "tool": "workspace",
        "repo": repo,
        "roots": roots,
        "indexed": index.indexed,
        "files": index.file_paths().len(),
        "nodes": index.nodes().len(),
        "servers": "one Index per repo; the MCP server may bind several",
        "scoping": {
            "multiRoot": "the roots of one repo, set at spawn with repeatable `--root`",
            "reScope": "call `index {roots:[\"...\", \"...\"]}` to re-index this repo over different roots",
            "secondRepo": "bind several repos to one server with repeatable `--repo DIR`, \
                           then select per call with the `repo` argument (1-based index, path, \
                           or a unique basename)",
        },
        "limitation": "this raw aux helper answers from the single `Index` it is given; over MCP \
                       the `workspace` tool is served by the server, which reports every bound \
                       repo and the per-call `repo` selector.",
    })
}

/// Run one auxiliary (non-edit) MCP-only tool and wrap it in the standard
/// envelope. `parse_check` runs the real Roslyn gate; `find_duplicates` runs the
/// dedupe engine; `counts` is the first-class count summary; `workspace` reports
/// the bound repo/roots. All four are read-only.
pub fn invoke_aux(index: &Index, name: &str, args: &Value) -> Value {
    let body = match name {
        "parse_check" => parse_check_tool(index, args),
        "compile_diff" => compile_diff_tool(index, args),
        "find_duplicates" => find_duplicates(index, args),
        "counts" => counts_tool(index, args),
        "workspace" => workspace_tool(index, args),
        other => json!({"ok": false, "error": format!("unknown tool: {other}")}),
    };
    envelope(body)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;
    use std::path::PathBuf;

    fn indexed(tag: &str, files: &[(&str, &str)]) -> (PathBuf, Index) {
        let dir = std::env::temp_dir().join(format!("csgraph-edit-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (name, text) in files {
            let p = dir.join(name);
            if let Some(par) = p.parent() {
                fs::create_dir_all(par).unwrap();
            }
            fs::write(p, text).unwrap();
        }
        let mut ix = Index::new(dir.clone());
        ix.build(&[".".to_string()], &[], &Config::default());
        assert!(ix.indexed, "fixture must index");
        (dir, ix)
    }

    fn body(result: &Value) -> Value {
        let text = result["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    fn call(ix: &mut Index, name: &str, args: Value) -> Value {
        body(&invoke(ix, name, &args))
    }

    /// Dispatch an aux (non-edit) tool and unwrap its envelope.
    fn call_aux(ix: &Index, name: &str, args: Value) -> Value {
        let wrapped = invoke_aux(ix, name, &args);
        serde_json::from_str(wrapped["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    const SIMPLE: &str = "namespace N {\n public class C {\n  public int A() { return B(); }\n  public int B() { return 2; }\n }\n}\n";

    /// A Unity-shaped repo whose configured indexed root is
    /// `Assets/NZK toolkit v5`, plus the managed directories Unity itself owns.
    /// The engine must confine every write to that root and never touch
    /// `Library/` or `ProjectSettings/`.
    fn unity_indexed(tag: &str, files: &[(&str, &str)]) -> (PathBuf, Index) {
        let dir = std::env::temp_dir().join(format!(
            "csgraph-guard-edit-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        for d in ["Assets/NZK toolkit v5", "Assets/Other", "Library", "ProjectSettings"] {
            fs::create_dir_all(dir.join(d)).unwrap();
        }
        for (name, text) in files {
            let p = dir.join(name);
            if let Some(par) = p.parent() {
                fs::create_dir_all(par).unwrap();
            }
            fs::write(p, text).unwrap();
        }
        let mut ix = Index::new(dir.clone());
        ix.build(
            &["Assets/NZK toolkit v5".to_string()],
            &["Library".to_string(), "ProjectSettings".to_string()],
            &Config::default(),
        );
        assert!(ix.indexed, "fixture must index");
        (dir, ix)
    }

    #[test]
    fn write_file_refuses_managed_dirs_and_out_of_root_unless_opted_in() {
        let (dir, mut ix) = unity_indexed(
            "write-scope",
            &[("Assets/NZK toolkit v5/Test.cs", "namespace N { public class Test { } }\n")],
        );
        // Unity-managed directories inside the repo (and inside the root) are
        // refused, and nothing lands on disk.
        for rel in ["Library/foo.txt", "ProjectSettings/ProjectSettings.asset"] {
            let r = call(
                &mut ix,
                "write_file",
                json!({"path": rel, "content": "x", "create": true, "apply": true}),
            );
            assert_eq!(r["ok"], false, "{rel}: {r}");
            let err = r["error"].as_str().unwrap_or("");
            assert!(err.starts_with("write-scope refusal:"), "{rel}: {err}");
            assert!(err.contains(rel), "the refusal must name {rel}: {err}");
            assert!(!dir.join(rel).exists(), "a refused write must not land: {rel}");
        }
        // Inside the repo but outside the indexed root: refused by default…
        let r = call(
            &mut ix,
            "write_file",
            json!({"path": "Assets/Other/file.cs",
                   "content": "namespace N { public class D { } }\n",
                   "create": true, "apply": true}),
        );
        assert_eq!(r["ok"], false, "{r}");
        let err = r["error"].as_str().unwrap_or("");
        assert!(err.contains("outside the indexed root(s)"), "{err}");
        assert!(err.contains("allow_outside_root=true"), "{err}");
        assert!(!dir.join("Assets/Other/file.cs").exists());
        // …and allowed only with the explicit, loud opt-in.
        let r = call(
            &mut ix,
            "write_file",
            json!({"path": "Assets/Other/file.cs",
                   "content": "namespace N { public class D { } }\n",
                   "create": true, "apply": true, "allow_outside_root": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert!(dir.join("Assets/Other/file.cs").exists());
        // The opt-in never widens past a managed directory.
        let r = call(
            &mut ix,
            "write_file",
            json!({"path": "Library/foo.txt", "content": "x", "create": true,
                   "apply": true, "allow_outside_root": true}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert!(
            r["error"].as_str().unwrap_or("").contains("engine-managed directory"),
            "{r}"
        );
        // An ordinary write inside the root still works.
        let r = call(
            &mut ix,
            "write_file",
            json!({"path": "Assets/NZK toolkit v5/Test.cs",
                   "content": "namespace N { public class Test { public int V() { return 1; } } }\n",
                   "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert!(
            fs::read_to_string(dir.join("Assets/NZK toolkit v5/Test.cs"))
                .unwrap()
                .contains("V()"),
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_path_refuses_managed_and_out_of_root_destinations() {
        let (dir, mut ix) = unity_indexed(
            "copy-scope",
            &[("Assets/NZK toolkit v5/Src.cs", SIMPLE)],
        );
        for dest in ["Library/copied.cs", "ProjectSettings/copied.cs"] {
            let r = call(
                &mut ix,
                "copy_path",
                json!({"from": "Assets/NZK toolkit v5/Src.cs", "to": dest, "apply": true}),
            );
            assert_eq!(r["ok"], false, "{dest}: {r}");
            let err = r["error"].as_str().unwrap_or("");
            assert!(err.starts_with("write-scope refusal:"), "{dest}: {err}");
            assert!(!dir.join(dest).exists(), "a refused copy must not land: {dest}");
        }
        // A tree copy whose *destination* is fine but which carries a managed
        // directory inside it is refused per file.
        fs::create_dir_all(dir.join("Assets/NZK toolkit v5/Tree/Library")).unwrap();
        fs::write(dir.join("Assets/NZK toolkit v5/Tree/Library/inner.cs"), SIMPLE).unwrap();
        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "Assets/NZK toolkit v5/Tree", "to": "Assets/NZK toolkit v5/TreeCopy",
                   "apply": true}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert!(!dir.join("Assets/NZK toolkit v5/TreeCopy/Library/inner.cs").exists());
        // In-repo but out of root needs the opt-in.
        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "Assets/NZK toolkit v5/Src.cs", "to": "Assets/Other/Copy.cs",
                   "apply": true}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert!(
            r["error"].as_str().unwrap_or("").contains("outside the indexed root(s)"),
            "{r}"
        );
        // An ordinary in-root copy still works.
        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "Assets/NZK toolkit v5/Src.cs", "to": "Assets/NZK toolkit v5/Copy.cs",
                   "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert!(dir.join("Assets/NZK toolkit v5/Copy.cs").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_decls_refuses_managed_and_out_of_root_destinations() {
        let (dir, mut ix) =
            unity_indexed("move-scope", &[("Assets/NZK toolkit v5/Src.cs", MOVE_SRC)]);
        for nf in ["Library/C.cs", "Assets/Other/C.cs"] {
            let r = call(
                &mut ix,
                "move_decls",
                json!({"like": ["C"], "class": "VF", "newfile": nf, "apply": true}),
            );
            assert_eq!(r["ok"], false, "{nf}: {r}");
            let err = r["error"].as_str().unwrap_or("");
            assert!(err.starts_with("write-scope refusal:"), "{nf}: {err}");
            assert!(!dir.join(nf).exists(), "a refused move must not land: {nf}");
        }
        // An ordinary in-root move still works.
        let r = call(
            &mut ix,
            "move_decls",
            json!({"like": ["C"], "class": "VF", "newfile": "Assets/NZK toolkit v5/C.cs",
                   "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert!(dir.join("Assets/NZK toolkit v5/C.cs").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fuzzy_search_ranks_deterministically_and_filters_by_kind() {
        let (dir, mut ix) = indexed(
            "fuzzy",
            &[
                ("a.cs", SIMPLE),
                ("b.cs", "namespace N { public class Alpha { public void Zeta() {} } }\n"),
            ],
        );
        let r = call(&mut ix, "fuzzy_search", json!({"q": "N.Alpha"}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["results"][0]["id"], "N.Alpha");
        assert_eq!(r["results"][0]["match"], "full");
        assert_eq!(r["results"][0]["score"], 100);

        // simple-name hit is tier 2
        let r = call(&mut ix, "fuzzy_search", json!({"q": "B"}));
        let top = &r["results"][0];
        assert_eq!(top["id"], "N.C.B()");
        assert_eq!(top["match"], "simple");

        // kind filter
        let r = call(&mut ix, "fuzzy_search", json!({"q": "B", "kind": "method"}));
        assert!(r["results"].as_array().unwrap().iter().all(|x| x["kind"] == "method"));
        let r = call(&mut ix, "fuzzy_search", json!({"q": "B", "kind": "class"}));
        assert_eq!(r["results"].as_array().unwrap().len(), 0);

        // deterministic: two identical calls agree
        let a = call(&mut ix, "fuzzy_search", json!({"q": "C", "limit": 10}));
        let b = call(&mut ix, "fuzzy_search", json!({"q": "C", "limit": 10}));
        assert_eq!(a["results"], b["results"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_chain_resolves_and_reports_ambiguity() {
        let (dir, mut ix) = indexed("chain", &[("a.cs", SIMPLE)]);
        let r = call(&mut ix, "resolve_chain", json!({"symbol": "N.C.B()", "depth": 2}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["resolved"]["id"], "N.C.B()");
        assert!(r["flat"].is_array());
        assert_eq!(r["direction"], "forward");

        // fuzzy name resolves when unique
        let r = call(&mut ix, "resolve_chain", json!({"symbol": "Alpha"}));
        assert_eq!(r["ok"], false); // no Alpha in this fixture
        assert!(r["candidates"].is_array());

        // ambiguous name
        let (_dir2, mut ix2) = indexed(
            "chain-amb",
            &[("a.cs", "namespace N { class C { void Dup() {} } class D { void Dup() {} } }\n")],
        );
        let r = call(&mut ix2, "resolve_chain", json!({"symbol": "Dup"}));
        assert_eq!(r["ok"], false, "{r}");
        assert!(r["candidates"].as_array().unwrap().len() >= 2, "{r}");

        // bad direction
        let r = call(&mut ix, "resolve_chain", json!({"symbol": "N.C", "dir": "sideways"}));
        assert_eq!(r["ok"], false);

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&_dir2);
    }

    #[test]
    fn code_block_emits_part_markers_and_files() {
        let (dir, mut ix) = indexed("codeblock", &[("a.cs", SIMPLE), ("b.cs", "namespace N { public class Alpha { } }\n")]);
        let r = call(&mut ix, "code_block", json!({"nodes": ["N.C", "N.Alpha"]}));
        assert_eq!(r["ok"], true, "{r}");
        let text = r["text"].as_str().unwrap();
        assert!(text.contains("// \u{2500}\u{2500} N.C \u{b7} a.cs:"), "{text}");
        assert!(text.contains("public int B()"), "{text}");
        assert!(r["files"].as_array().unwrap().iter().any(|f| f == "a.cs"));
        assert!(r["parts"].as_array().unwrap().len() >= 2);
        assert!(r["lines"].as_i64().unwrap() > 0);

        // q-based selection
        let r = call(&mut ix, "code_block", json!({"q": "Alpha"}));
        assert!(r["text"].as_str().unwrap().contains("class Alpha"));

        // path_only suppresses the text
        let r = call(&mut ix, "code_block", json!({"node": "N.C", "path_only": true}));
        assert_eq!(r["pathOnly"], true);
        assert_eq!(r["text"], "");

        // missing selector
        let r = call(&mut ix, "code_block", json!({}));
        assert_eq!(r["ok"], false);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_symbol_dry_run_then_apply() {
        let (dir, mut ix) = indexed("rename", &[("a.cs", SIMPLE)]);
        let r = call(
            &mut ix,
            "rename_symbol",
            json!({"from": "N.C.B()", "to": "Beta"}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], false);
        assert_eq!(r["files_planned"], json!(["a.cs"]));
        let edits = r["edits"].as_array().unwrap();
        assert_eq!(edits.len(), 2, "{r}");
        assert!(edits.iter().any(|e| e["line"] == 3 && e["after"].as_str().unwrap().contains("Beta()")));
        assert!(edits.iter().any(|e| e["line"] == 4 && e["after"].as_str().unwrap().contains("Beta()")));

        // dry run left the file alone
        let raw = fs::read_to_string(dir.join("a.cs")).unwrap();
        assert!(raw.contains("int B()"));

        let r = call(
            &mut ix,
            "rename_symbol",
            json!({"from": "N.C.B()", "to": "Beta", "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], true);
        assert_eq!(r["written"], json!(["a.cs"]));
        let raw = fs::read_to_string(dir.join("a.cs")).unwrap();
        assert!(raw.contains("public int Beta()"), "{raw}");
        assert!(raw.contains("return Beta();"), "{raw}");

        // illegal identifier is refused
        let r = call(&mut ix, "rename_symbol", json!({"from": "N.C", "to": "1bad"}));
        assert_eq!(r["ok"], false);
        // keyword is refused
        let r = call(&mut ix, "rename_symbol", json!({"from": "N.C", "to": "class"}));
        assert_eq!(r["ok"], false);
        // unknown symbol is refused
        let r = call(&mut ix, "rename_symbol", json!({"from": "NoSuchThing", "to": "X"}));
        assert_eq!(r["ok"], false);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_symbol_refuses_ambiguity_and_respects_scope() {
        let (dir, mut ix) = indexed(
            "rename-amb",
            &[("a.cs", "namespace N { class C { void Dup() {} } class D { void Dup() {} } }\n")],
        );
        let r = call(&mut ix, "rename_symbol", json!({"from": "Dup", "to": "Dup2"}));
        assert_eq!(r["ok"], false, "{r}");
        assert!(r["ambiguity"].as_array().unwrap().len() >= 2, "{r}");

        // scope excludes the only edit site -> nothing planned
        let r = call(
            &mut ix,
            "rename_symbol",
            json!({"from": "N.C.Dup()", "to": "Dup2", "scope": "other.cs"}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["edits"].as_array().unwrap().len(), 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn qualify_symbols_removes_usings_and_reports_ambiguity() {
        let (dir, mut ix) = indexed(
            "qualify",
            &[
                ("a.cs", "namespace A { public class Widget { } }\n"),
                ("b.cs", "using A;\nnamespace B {\n public class User {\n  public Widget W;\n }\n}\n"),
            ],
        );
        let r = call(&mut ix, "qualify_symbols", json!({}));
        assert_eq!(r["ok"], true, "{r}");
        assert!(r["remove_usings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["file"] == "b.cs" && x["line"] == 1));
        assert!(r["edits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["file"] == "b.cs" && e["after"].as_str().unwrap().contains("A.Widget W")));
        assert_eq!(r["skipped_ambiguous"], json!([]));

        // apply drops the using and qualifies the reference
        let r = call(&mut ix, "qualify_symbols", json!({"apply": true}));
        assert_eq!(r["ok"], true, "{r}");
        let b = fs::read_to_string(dir.join("b.cs")).unwrap();
        assert!(!b.contains("using A;"), "{b}");
        assert!(b.contains("A.Widget W"), "{b}");
        let a = fs::read_to_string(dir.join("a.cs")).unwrap();
        assert!(a.contains("class Widget"), "{a}");

        // an ambiguous simple name is reported, never rewritten
        let (dir2, mut ix2) = indexed(
            "qualify-amb",
            &[
                ("a.cs", "namespace A { public class Widget { } }\n"),
                ("c.cs", "namespace C { public class Widget { } }\n"),
                ("b.cs", "namespace B { public class User { public Widget W; } }\n"),
            ],
        );
        let r = call(&mut ix2, "qualify_symbols", json!({}));
        assert!(r["skipped_ambiguous"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["name"] == "Widget" && x["candidates"].as_array().unwrap().len() == 2));
        assert!(r["edits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| !e["after"].as_str().unwrap().contains("A.Widget W")));

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&dir2);
    }

    #[test]
    fn qualify_symbols_qualifies_static_members_and_interpolated_holes() {
        let (dir, mut ix) = indexed(
            "qualify-static",
            &[
                (
                    "a.cs",
                    "namespace N { public class C { public static int Helper() { return 1; } } }\n",
                ),
                (
                    "b.cs",
                    "namespace M {\n public class U {\n  public string S() { return $\"v={Helper()} and {C.Helper()}\"; }\n }\n}\n",
                ),
            ],
        );
        let r = call(
            &mut ix,
            "qualify_symbols",
            json!({"files": ["b.cs"], "remove_usings": false}),
        );
        let joined = r["edits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["after"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // A unique static member is qualified even inside an interpolation hole.
        assert!(joined.contains("N.C.Helper()"), "{joined}");
        // An already-qualified chain (and its trailing name) is never touched.
        assert!(joined.contains("C.Helper()"), "{joined}");
        assert!(!joined.contains("N.C.C.Helper"), "{joined}");
        // The declared type name itself is protected.
        assert!(!joined.contains("M.U U"), "{joined}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_file_is_dry_run_then_creates_and_refuses() {
        let (dir, mut ix) = indexed("writefile", &[("a.cs", SIMPLE)]);
        let r = call(
            &mut ix,
            "write_file",
            json!({"path": "new.cs", "content": "namespace X {}\n", "create": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], false);
        assert_eq!(r["old_sha256"], Value::Null);
        assert!(r["bytes"].as_i64().unwrap() > 0);
        assert!(!dir.join("new.cs").exists(), "dry run must not create the file");

        // creating without create=true is refused
        let r = call(&mut ix, "write_file", json!({"path": "nope.cs", "content": "x"}));
        assert_eq!(r["ok"], false, "{r}");

        // apply really creates it
        let content = "namespace X { public class Y { } }\n";
        let r = call(
            &mut ix,
            "write_file",
            json!({"path": "new.cs", "content": content, "create": true, "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], true);
        assert_eq!(fs::read_to_string(dir.join("new.cs")).unwrap(), content);

        // expected_sha256 mismatch is refused
        let r = call(
            &mut ix,
            "write_file",
            json!({"path": "a.cs", "content": "x", "expected_sha256": "deadbeef"}),
        );
        assert_eq!(r["ok"], false, "{r}");

        // matching hash applies
        let cur = fs::read_to_string(dir.join("a.cs")).unwrap();
        let h = sha256_hex(cur.as_bytes());
        let r = call(
            &mut ix,
            "write_file",
            json!({"path": "a.cs", "content": "namespace N {}\n", "expected_sha256": h, "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(fs::read_to_string(dir.join("a.cs")).unwrap(), "namespace N {}\n");

        // a path outside the repo is refused
        let outside = std::env::temp_dir().join(format!("csgraph-outside-{}.cs", std::process::id()));
        let r = call(
            &mut ix,
            "write_file",
            json!({"path": outside.to_string_lossy(), "content": "x", "create": true, "apply": true}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert!(!outside.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_edits_replaces_exact_occurrences_and_is_all_or_nothing() {
        let (dir, mut ix) = indexed(
            "applyedits",
            &[
                ("a.cs", SIMPLE),
                ("b.cs", "namespace N { public class Alpha { } }\n"),
            ],
        );
        let r = call(
            &mut ix,
            "apply_edits",
            json!({"edits": [{"file": "b.cs", "old": "Alpha", "new": "Beta"}]}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], false, "dry run by default");
        assert_eq!(r["edits"][0]["matched"], 1);
        assert_eq!(r["edits"][0]["replaced"], 1);
        assert_eq!(r["edits"][0]["old_sha256"].as_str().unwrap().len(), 64);
        assert!(fs::read_to_string(dir.join("b.cs")).unwrap().contains("Alpha"));

        let r = call(
            &mut ix,
            "apply_edits",
            json!({"edits": [{"file": "b.cs", "old": "Alpha", "new": "Beta"}], "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], true);
        assert!(fs::read_to_string(dir.join("b.cs")).unwrap().contains("Beta"));

        // a wrong count refuses and writes nothing
        let before_a = fs::read_to_string(dir.join("a.cs")).unwrap();
        let r = call(
            &mut ix,
            "apply_edits",
            json!({"edits": [{"file": "a.cs", "old": "B()", "new": "Z()", "count": 5}], "apply": true}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert_eq!(fs::read_to_string(dir.join("a.cs")).unwrap(), before_a);

        // one zero-match edit aborts the whole batch before any write
        let r = call(
            &mut ix,
            "apply_edits",
            json!({"edits": [
                {"file": "a.cs", "old": "return B();", "new": "return Z();"},
                {"file": "b.cs", "old": "Nope", "new": "x"}], "apply": true}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert!(fs::read_to_string(dir.join("a.cs")).unwrap().contains("B()"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_namespace_normalises_stale_headers_and_delegates_to_reorg() {
        let (dir, mut ix) = indexed("expand", &[("a.cs", "namespace N { public class C { } }\n")]);
        // Make the source drift from the graph: the declaration now says Wrong.
        fs::write(dir.join("a.cs"), "namespace Wrong { public class C { } }\n").unwrap();
        let r = call(&mut ix, "expand_namespace", json!({}));
        assert_eq!(r["ok"], true, "{r}");
        let planned = r["planned"].as_array().unwrap();
        assert_eq!(planned.len(), 1, "{r}");
        assert_eq!(planned[0]["declared"], "Wrong");
        assert_eq!(planned[0]["namespace"], "N");

        let r = call(&mut ix, "expand_namespace", json!({"apply": true}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["written"], json!(["a.cs"]));
        let raw = fs::read_to_string(dir.join("a.cs")).unwrap();
        assert!(raw.starts_with("namespace N {"), "{raw}");

        // dest delegates to reorg action=write with group=namespace
        let dest = dir.join("out");
        let r = call(
            &mut ix,
            "expand_namespace",
            json!({"dest": dest.to_string_lossy()}),
        );
        assert_eq!(r["delegated"], true, "{r}");
        assert!(dest.is_dir(), "reorg write did not create the destination");

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- move_decls -------------------------------------------------------

    const MOVE_SRC: &str = "namespace N {\n public class C {\n  // doc\n  public int Helper() { return 2; }\n  public int Use() { return Helper(); }\n }\n}\n";

    #[test]
    fn move_decls_extract_is_a_dry_run_then_moves_deletes_and_references() {
        let (dir, ix) = indexed("move-extract", &[("a.cs", MOVE_SRC)]);
        let mut ix = ix;
        let r = call(
            &mut ix,
            "move_decls",
            json!({"like": ["Helper"], "mtype": ["int"], "newfile": "moved.cs",
                   "prefix": "NZK", "class": "VF"}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["mode"], "extract");
        assert_eq!(r["applied"], false);
        assert_eq!(r["reindex_required"], true);
        assert_eq!(r["files"], json!(["a.cs", "moved.cs"]));
        // slice includes the leading comment (blockStart 3), delete 3..4
        assert!(
            r["edits"].as_array().unwrap().iter().any(|e| e["kind"] == "delete"
                && e["file"] == "a.cs" && e["from"] == 3 && e["to"] == 4),
            "{r}"
        );
        // call site re-referenced under the new namespace + destination class
        assert!(
            r["edits"].as_array().unwrap().iter().any(|e| e["kind"] == "rewrite"
                && e["file"] == "a.cs" && e["line"] == 5
                && e["after"].as_str().unwrap().contains("NZK.VF.Helper()")),
            "{r}"
        );
        assert_eq!(r["selected"][0]["newFull"], "NZK.VF.Helper");
        // the destination text is inspectable before apply
        let content = r["destination"]["content"].as_str().unwrap();
        assert!(
            content.starts_with("namespace NZK{  public static partial class VF{"),
            "{content}"
        );
        assert!(content.contains("// doc"), "{content}");
        // dry run touched nothing
        assert_eq!(fs::read_to_string(dir.join("a.cs")).unwrap(), MOVE_SRC);
        assert!(!dir.join("moved.cs").exists(), "dry run must not create the file");

        // apply
        let r = call(
            &mut ix,
            "move_decls",
            json!({"like": ["Helper"], "mtype": ["int"], "newfile": "moved.cs",
                   "prefix": "NZK", "class": "VF", "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], true);
        assert_eq!(r["written"], json!(["a.cs", "moved.cs"]));
        assert_eq!(r["created"], json!(["moved.cs"]));
        let moved = fs::read_to_string(dir.join("moved.cs")).unwrap();
        assert!(moved.starts_with("namespace NZK{  public static partial class VF{"), "{moved}");
        assert!(moved.contains("// doc"), "leading comment must move: {moved}");
        assert!(moved.contains("public int Helper()"), "{moved}");
        assert!(moved.trim_end().ends_with("}}"), "{moved}");
        let src = fs::read_to_string(dir.join("a.cs")).unwrap();
        assert!(!src.contains("int Helper()"), "origin must be deleted: {src}");
        assert!(src.contains("NZK.VF.Helper()"), "call site must be re-referenced: {src}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_decls_extract_refuses_a_bare_member_without_a_class() {
        let (dir, mut ix) = indexed("move-refuse", &[("a.cs", MOVE_SRC)]);
        let r = call(
            &mut ix,
            "move_decls",
            json!({"like": ["Helper"], "newfile": "moved.cs", "prefix": "NZK"}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert!(r["error"].as_str().unwrap().contains("destination type"), "{r}");
        // a qualified newfile stem is enough to infer the class
        let r = call(
            &mut ix,
            "move_decls",
            json!({"like": ["Helper"], "newfile": "NZK.VF.cs", "prefix": "NZK"}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["target"]["class"], "VF");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_decls_rename_into_fully_qualified_home_creates_class_and_namespace() {
        let (dir, mut ix) = indexed(
            "move-home",
            &[(
                "a.cs",
                "namespace N {\n public class C {\n  public int B() { return 2; }\n  public int A() { return B(); }\n }\n}\n",
            )],
        );
        let args = json!({"from": "N.C.B()", "to": "NZK.B.compare", "newfile": "b.cs"});
        let r = call(&mut ix, "move_decls", args.clone());
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["mode"], "rename-into");
        assert_eq!(r["target"]["namespace"], "NZK");
        assert_eq!(r["target"]["class"], "B");
        assert_eq!(r["target"]["member"], "compare");
        assert_eq!(r["target"]["created_file"], true);
        assert_eq!(r["selected"][0]["newFull"], "NZK.B.compare");
        // dry run: nothing on disk
        assert!(!dir.join("b.cs").exists());
        assert!(fs::read_to_string(dir.join("a.cs")).unwrap().contains("int B()"));

        let mut args = args;
        args["apply"] = json!(true);
        let r = call(&mut ix, "move_decls", args);
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], true);
        let b = fs::read_to_string(dir.join("b.cs")).unwrap();
        assert!(b.contains("namespace NZK {"), "{b}");
        assert!(b.contains("public class B {"), "{b}");
        assert!(b.contains("public int compare()"), "{b}");
        assert!(!b.contains("public int B()"), "{b}");
        let a = fs::read_to_string(dir.join("a.cs")).unwrap();
        assert!(!a.contains("public int B()"), "origin must be deleted: {a}");
        assert!(a.contains("return NZK.B.compare();"), "call site must be re-referenced: {a}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_decls_reports_ambiguous_call_sites_untouched() {
        let (dir, mut ix) = indexed(
            "move-amb",
            &[(
                "a.cs",
                "namespace N {\n public class C {\n  public int Dup() { return 1; }\n  public int Use() { return Dup(); }\n }\n public class D {\n  public int Dup() { return 2; }\n }\n}\n",
            )],
        );
        let r = call(
            &mut ix,
            "move_decls",
            json!({"from": "N.C.Dup()", "to": "NZK.X.dup", "newfile": "x.cs"}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert!(
            r["ambiguous"].as_array().unwrap().iter().any(|a| a["name"] == "Dup"),
            "ambiguous simple name must be reported: {r}"
        );
        // never guessed: no rewrite of the call site
        assert!(
            r["edits"].as_array().unwrap().iter().all(|e| e["kind"] != "rewrite"),
            "{r}"
        );
        // apply still moves the declaration, but leaves the call alone
        let r = call(
            &mut ix,
            "move_decls",
            json!({"from": "N.C.Dup()", "to": "NZK.X.dup", "newfile": "x.cs", "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        let a = fs::read_to_string(dir.join("a.cs")).unwrap();
        assert!(a.contains("return Dup();"), "ambiguous call site must be untouched: {a}");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A hermetic stand-in for the real `_sealed_src/NZKNaNimateMeshFolder.cs.nzk`
    /// shape: nested `static partial` classes, a `const` field, static siblings
    /// and an already-qualified external chain. The same proof runs against the
    /// real file in `tools/probe_edit_mcp.py`, so the unit test never depends on
    /// the checkout's `_sealed_src` layout.
    const MOVE_NESTED: &str = "#if UNITY_EDITOR\n\
namespace NZK\n\
{\n\
public static partial class Core\n\
{\n\
public static class NZKNaNimateMeshFolder{\n\
    public const string MeshesFolderName=\"Meshes\";\n\
    /* doc comment for the moved member. */\n\
    public static string FolderFor(string modelAssetPath,string avatarName){return AvatarRoot(NZK.B.NoE(avatarName)?LeafNameOf(modelAssetPath):avatarName)+\"/\"+MeshesFolderName;}\n\
    /* Bare file name of an asset path, without its extension. */\n\
    static string LeafNameOf(string path){return NZK.S.P.Next(path);}\n\
    /* Generated avatar root for an already-sanitized avatar name. */\n\
    static string AvatarRoot(string avatarName){return \"Assets/!_NZK_Generated/\"+NZK.SS.An(avatarName);}\n\
  }\n\
}\n\
}\n\
#endif\n";

    #[test]
    fn move_decls_wraps_members_and_requalifies_nested_static_siblings() {
        let (dir, mut ix) = indexed("move-nested", &[("mesh.cs.nzk", MOVE_NESTED)]);
        let args = json!({
            "from": "NZK.Core.NZKNaNimateMeshFolder.FolderFor",
            "newfile": "NZK.VF.cs",
            "prefix": "NZK.VF.NS",
        });
        let r = call(&mut ix, "move_decls", args.clone());
        assert_eq!(r["ok"], true, "{r}");
        // the class is inferred from the qualified newfile stem
        assert_eq!(r["target"]["class"], "VF", "{r}");
        let content = r["destination"]["content"].as_str().unwrap();
        assert!(
            content.contains("namespace NZK.VF.NS{  public static partial class VF{"),
            "dialect opener missing:\n{content}"
        );
        assert!(content.trim_end().ends_with("}}"), "closer missing:\n{content}");
        // siblings left behind in the origin type are fully qualified
        assert!(
            content.contains("NZK.Core.NZKNaNimateMeshFolder.AvatarRoot("),
            "AvatarRoot not re-qualified:\n{content}"
        );
        assert!(
            content.contains("NZK.Core.NZKNaNimateMeshFolder.LeafNameOf("),
            "LeafNameOf not re-qualified:\n{content}"
        );
        assert!(
            content.contains("NZK.Core.NZKNaNimateMeshFolder.MeshesFolderName"),
            "const field not re-qualified:\n{content}"
        );
        // already-qualified chains and locals are untouched
        assert!(content.contains("NZK.B.NoE(avatarName)"), "{content}");
        assert!(!content.contains("NZK.Core.NZKNaNimateMeshFolder.NZK"), "{content}");
        // dry run writes nothing, but the create edit exposes the full text
        assert!(!dir.join("NZK.VF.cs").exists(), "dry run must not write");
        let create = r["edits"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["kind"] == "create")
            .unwrap();
        assert_eq!(create["content"].as_str().unwrap(), content);

        let mut args = args;
        args["apply"] = json!(true);
        let r = call(&mut ix, "move_decls", args);
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], true);
        let written = fs::read_to_string(dir.join("NZK.VF.cs")).unwrap();
        assert!(written.contains("public static partial class VF{"), "{written}");
        assert!(
            written.contains("NZK.Core.NZKNaNimateMeshFolder.AvatarRoot("),
            "{written}"
        );
        let origin = fs::read_to_string(dir.join("mesh.cs.nzk")).unwrap();
        assert!(
            !origin.contains("public static string FolderFor("),
            "origin must be deleted:\n{origin}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_decls_uses_the_per_file_part_for_a_merged_node() {
        // The parser merges same-named declarations declared in two files into
        // one node whose aggregate `endLine` spans both. The slice must come from
        // the node's primary file part, never the aggregate.
        let (dir, mut ix) = indexed(
            "move-part",
            &[
                (
                    "a.cs",
                    "namespace N {\n public partial class P {\n  public int Same() { return 1; }\n }\n}\n",
                ),
                (
                    "b.cs",
                    "namespace N {\n public partial class P {\n\n\n\n\n  public int Same() { return 2; }\n }\n}\n",
                ),
            ],
        );
        let r = call(
            &mut ix,
            "move_decls",
            json!({"like": ["Same"], "newfile": "NZK.VF.cs", "prefix": "NZK.VF.NS",
                   "class": "VF"}),
        );
        assert_eq!(r["ok"], true, "merged node must slice per file: {r}");
        assert_eq!(r["selected"][0]["file"], "a.cs", "{r}");
        assert_eq!(r["selected"][0]["endLine"], 3, "{r}");
        assert!(!dir.join("NZK.VF.cs").exists(), "dry run must not write");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_decls_refuses_a_missing_target_and_an_illegal_home() {
        let (dir, mut ix) = indexed("move-errors", &[("a.cs", MOVE_SRC)]);
        let r = call(&mut ix, "move_decls", json!({"like": ["Helper"]}));
        assert_eq!(r["ok"], false, "{r}");
        let r = call(
            &mut ix,
            "move_decls",
            json!({"like": ["Helper"], "to": "NZK.9bad.compare"}),
        );
        assert_eq!(r["ok"], false, "{r}");
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- grep -------------------------------------------------------------

    #[test]
    fn grep_searches_with_scope_counts_and_determinism() {
        let dir = std::env::temp_dir().join(format!("csgraph-grep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(
            dir.join("a.cs"),
            "namespace A { public class Alpha { public static int Hit() { return 1; } } }\n",
        )
        .unwrap();
        fs::write(dir.join("b.cs"), "namespace B { public class Beta { } }\n").unwrap();
        fs::write(
            dir.join("sub/c.cs"),
            "namespace C { public class Gamma { public static void Go() {} } }\n",
        )
        .unwrap();
        let mut ix = Index::new(dir.clone());
        ix.build(&[".".to_string()], &[], &Config::default());
        assert!(ix.indexed, "fixture must index");

        // default: case-insensitive regex, deterministic file/line ordering
        let r = call(&mut ix, "grep", json!({"pattern": "public static"}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["total_matches"], 2, "{r}");
        assert_eq!(r["file_count"], 2, "{r}");
        assert_eq!(r["files"][0]["file"], "a.cs");
        assert_eq!(r["files"][1]["file"], "sub/c.cs");
        assert_eq!(r["files"][0]["matches"][0]["line"], 1);
        assert!(r["files"][0]["matches"][0]["col"].as_i64().unwrap() >= 1);
        assert!(r["files"][0]["matches"][0]["text"].as_str().unwrap().contains("public static"));
        assert_eq!(r["truncated"], false);
        let a = call(&mut ix, "grep", json!({"pattern": "public static"}));
        assert_eq!(a, r, "grep must be deterministic");

        // regex alternation
        let r = call(&mut ix, "grep", json!({"pattern": "Alpha|Beta"}));
        assert_eq!(r["total_matches"], 2, "{r}");
        let got: Vec<String> = r["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["file"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(got, vec!["a.cs", "b.cs"]);

        // glob scope (basename) and directory path scope
        let r = call(&mut ix, "grep", json!({"pattern": "class", "glob": "*.cs"}));
        assert_eq!(r["file_count"], 3, "{r}");
        let r = call(&mut ix, "grep", json!({"pattern": "class", "glob": "sub/**"}));
        assert_eq!(r["file_count"], 1, "{r}");
        assert_eq!(r["files"][0]["file"], "sub/c.cs");
        let r = call(&mut ix, "grep", json!({"pattern": "class", "path": "sub"}));
        assert_eq!(r["file_count"], 1, "{r}");
        let r = call(&mut ix, "grep", json!({"pattern": "class", "paths": ["a.cs", "b.cs"]}));
        assert_eq!(r["file_count"], 2, "{r}");

        // literal + case-sensitive
        let r = call(
            &mut ix,
            "grep",
            json!({"pattern": "alpha", "literal": true, "case": false, "glob": "a.cs"}),
        );
        assert_eq!(r["total_matches"], 0, "{r}");
        let r = call(
            &mut ix,
            "grep",
            json!({"pattern": "Alpha", "literal": true, "case": false, "glob": "a.cs"}),
        );
        assert_eq!(r["total_matches"], 1, "{r}");

        // count_only returns counts with no rows
        let r = call(&mut ix, "grep", json!({"pattern": "public", "count_only": true}));
        assert_eq!(r["ok"], true, "{r}");
        assert!(r["total_matches"].as_i64().unwrap() >= 3, "{r}");
        assert!(
            r["files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["matches"].as_array().unwrap().is_empty()),
            "{r}"
        );

        // context rows
        let r = call(&mut ix, "grep", json!({"pattern": "Beta", "glob": "b.cs", "context": 1}));
        assert!(r["files"][0]["matches"][0]["before"].is_array(), "{r}");
        assert!(r["files"][0]["matches"][0]["after"].is_array(), "{r}");

        // max_per_file truncates rows but keeps the true count
        let r = call(&mut ix, "grep", json!({"pattern": "e", "glob": "a.cs", "max_per_file": 1}));
        assert_eq!(r["truncated"], true, "{r}");
        assert!(r["files"][0]["count"].as_i64().unwrap() > 1, "{r}");
        assert_eq!(r["files"][0]["matches"].as_array().unwrap().len(), 1, "{r}");

        // invalid pattern is refused, not panicked on
        let r = call(&mut ix, "grep", json!({"pattern": "("}));
        assert_eq!(r["ok"], false, "{r}");
        assert!(r["error"].as_str().unwrap().contains("invalid pattern"), "{r}");

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- move_decls: verifier regressions (task-14) ----------------------

    const MOVE_SEQ: &str = "namespace N {\n public class C {\n  public int First() { return 1; }\n  public int Second() { return 2; }\n }\n}\n";

    #[test]
    fn move_decls_second_move_into_existing_destination_writes_both() {
        let (dir, mut ix) = indexed("move-seq", &[("a.cs", MOVE_SEQ)]);
        let args = |from: &str, apply: bool| {
            json!({"from": from, "newfile": "NZK.VF.cs", "prefix": "NZK.VF.NS",
                   "class": "VF", "apply": apply})
        };
        // 1st move creates the destination.
        let r = call(&mut ix, "move_decls", args("N.C.First()", true));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["created"], json!(["NZK.VF.cs"]), "{r}");
        let dest1 = fs::read_to_string(dir.join("NZK.VF.cs")).unwrap();
        assert!(dest1.contains("public int First()"), "{dest1}");
        // Documented workflow: re-index after apply, so line numbers are current.
        ix.build(&[".".to_string()], &[], &Config::default());
        assert!(ix.indexed);

        // 2nd move, dry run: neither file changes.
        let src1 = fs::read_to_string(dir.join("a.cs")).unwrap();
        let r = call(&mut ix, "move_decls", args("N.C.Second()", false));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], false, "{r}");
        assert_eq!(r["files"], json!(["NZK.VF.cs", "a.cs"]), "{r}");
        assert_eq!(fs::read_to_string(dir.join("NZK.VF.cs")).unwrap(), dest1, "dry run wrote dest");
        assert_eq!(fs::read_to_string(dir.join("a.cs")).unwrap(), src1, "dry run wrote origin");

        // 2nd move, apply: origin delete AND destination append are one commit.
        let r = call(&mut ix, "move_decls", args("N.C.Second()", true));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], true, "{r}");
        assert_eq!(r["created"], json!([]), "{r}");
        assert_eq!(r["written"], json!(["NZK.VF.cs", "a.cs"]), "{r}");
        let dest2 = fs::read_to_string(dir.join("NZK.VF.cs")).unwrap();
        assert!(dest2.contains("public int First()"), "{dest2}");
        assert!(dest2.contains("public int Second()"), "destination append lost: {dest2}");
        let src2 = fs::read_to_string(dir.join("a.cs")).unwrap();
        assert!(!src2.contains("int First()"), "{src2}");
        assert!(!src2.contains("int Second()"), "{src2}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_decls_from_is_exact_and_refuses_after_the_symbol_moved() {
        let (dir, mut ix) = indexed("move-stale", &[("a.cs", MOVE_SEQ)]);
        let r = call(
            &mut ix,
            "move_decls",
            json!({"from": "N.C.First()", "newfile": "NZK.VF.cs", "prefix": "NZK.VF.NS",
                   "class": "VF", "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        // The documented caller re-runs `index` after apply; then the old name is
        // gone and must refuse rather than match Second's sibling by substring.
        ix.build(&[".".to_string()], &[], &Config::default());
        assert!(ix.indexed);
        let dest_before = fs::read_to_string(dir.join("NZK.VF.cs")).unwrap();
        let src_before = fs::read_to_string(dir.join("a.cs")).unwrap();
        let r = call(
            &mut ix,
            "move_decls",
            json!({"from": "N.C.First()", "newfile": "NZK.VF2.cs", "prefix": "NZK.VF.NS",
                   "class": "VF"}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert!(
            r["error"].as_str().unwrap().contains("does not name a declaration"),
            "{r}"
        );
        assert!(!dir.join("NZK.VF2.cs").exists());
        assert_eq!(fs::read_to_string(dir.join("NZK.VF.cs")).unwrap(), dest_before);
        assert_eq!(fs::read_to_string(dir.join("a.cs")).unwrap(), src_before);
        assert!(src_before.contains("public int Second()"), "{src_before}");

        // A stale exact `from` on a *stale* index must also not move a sibling:
        // the recorded slice no longer declares `First`, so it is refused.
        let (dir2, mut ix2) = indexed("move-stale2", &[("a.cs", MOVE_SEQ)]);
        let _ = call(
            &mut ix2,
            "move_decls",
            json!({"from": "N.C.First()", "newfile": "NZK.VF.cs", "prefix": "NZK.VF.NS",
                   "class": "VF", "apply": true}),
        );
        let r = call(
            &mut ix2,
            "move_decls",
            json!({"from": "N.C.First()", "newfile": "NZK.VF2.cs", "prefix": "NZK.VF.NS",
                   "class": "VF", "apply": true}),
        );
        assert_eq!(r["ok"], false, "stale slice must refuse: {r}");
        assert!(
            r["error"].as_str().unwrap().contains("re-run `index`"),
            "{r}"
        );
        let src2 = fs::read_to_string(dir2.join("a.cs")).unwrap();
        assert!(src2.contains("public int Second()"), "{src2}");
        assert!(!dir2.join("NZK.VF2.cs").exists());
        let _ = fs::remove_dir_all(&dir2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_decls_refuses_duplicate_destination_members() {
        let (dir, mut ix) = indexed(
            "move-dup",
            &[(
                "a.cs",
                "namespace N {\n public class C {\n  public static int FolderFor(string a) { return 1; }\n }\n public class D {\n  public static int FolderFor(string a) { return 2; }\n }\n}\n",
            )],
        );
        let r = call(
            &mut ix,
            "move_decls",
            json!({"like": ["FolderFor"], "newfile": "NZK.VF.cs", "prefix": "NZK.VF.NS",
                   "class": "VF"}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert!(
            r["error"].as_str().unwrap().contains("duplicate member signatures"),
            "{r}"
        );
        assert_eq!(r["duplicates"].as_array().unwrap().len(), 1, "{r}");
        assert!(!dir.join("NZK.VF.cs").exists());

        // Same name with different parameter lists are legal overloads: allowed.
        let (dir2, mut ix2) = indexed(
            "move-overload",
            &[(
                "a.cs",
                "namespace N {\n public class C {\n  public int M() { return 1; }\n  public int M(int x) { return x; }\n }\n}\n",
            )],
        );
        let r = call(
            &mut ix2,
            "move_decls",
            json!({"like": ["M"], "newfile": "NZK.VF.cs", "prefix": "NZK.VF.NS", "class": "VF"}),
        );
        assert_eq!(r["ok"], true, "overloads must not be refused: {r}");
        assert_eq!(r["selected"].as_array().unwrap().len(), 2, "{r}");
        let _ = fs::remove_dir_all(&dir2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_decls_refuses_unresolved_dependencies_unless_allowed() {
        let (dir, mut ix) = indexed(
            "move-unres",
            &[(
                "a.cs",
                "namespace N {\n public class C {\n  public static int Helper() { return 1; }\n  public int Mover() { return Helper(); }\n }\n public class E {\n  public static int Helper() { return 2; }\n }\n}\n",
            )],
        );
        let base = json!({"from": "N.C.Mover()", "newfile": "NZK.VF.cs", "prefix": "NZK.VF.NS",
                          "class": "VF", "apply": true});
        let r = call(&mut ix, "move_decls", base.clone());
        assert_eq!(r["ok"], false, "{r}");
        assert!(
            r["error"].as_str().unwrap().contains("cannot be uniquely resolved"),
            "{r}"
        );
        assert!(
            r["unresolved"]
                .as_array()
                .unwrap()
                .iter()
                .any(|u| u["name"] == "Helper"),
            "{r}"
        );
        assert!(!dir.join("NZK.VF.cs").exists(), "refusal must write nothing");

        // explicit escape hatch: emits with a loud warning
        let mut allowed = base;
        allowed["allow_unresolved"] = json!(true);
        let r = call(&mut ix, "move_decls", allowed);
        assert_eq!(r["ok"], true, "{r}");
        assert!(
            r["warnings"]
                .as_array()
                .unwrap()
                .iter()
                .any(|w| w.as_str().unwrap_or("").contains("Helper")),
            "{r}"
        );
        let dest = fs::read_to_string(dir.join("NZK.VF.cs")).unwrap();
        assert!(dest.contains("Helper()"), "{dest}");

        let _ = fs::remove_dir_all(&dir);
    }

    // ---- move_decls: declared identifiers + Roslyn gate (task-15) ---------

    const DECL_FIXTURE: &str = "namespace N {\n public class C {\n  public static int a(int projectRelative) { return projectRelative; }\n  public static int b() { return 1; }\n  public static int y() { return 2; }\n  public static int z() { return 3; }\n  public static int T() { return 4; }\n }\n public class D {\n  public static int Mover(int a) {\n   int b = a + 1;\n   int y = b;\n   var z = y;\n   return z;\n  }\n  public static int Generic<T>(T value) {\n   T copy = value;\n   int result = 0;\n   return result;\n  }\n }\n}\n";

    #[test]
    fn move_decls_never_qualifies_parameters_locals_or_generic_params() {
        let (dir, mut ix) = indexed("move-decl", &[("a.cs", DECL_FIXTURE)]);
        let r = call(
            &mut ix,
            "move_decls",
            json!({"from": "N.D.Mover(int)", "newfile": "NZK.VF.cs", "prefix": "NZK.VF.NS",
                   "class": "VF"}),
        );
        assert_eq!(r["ok"], true, "{r}");
        let content = r["destination"]["content"].as_str().unwrap();
        assert!(content.contains("Mover(int a)"), "parameter renamed:\n{content}");
        assert!(content.contains("int b = a + 1"), "local renamed:\n{content}");
        assert!(content.contains("var z = y"), "var local renamed:\n{content}");
        assert!(
            !content.contains("N.C.a") && !content.contains("N.C.b") && !content.contains("N.C.z"),
            "a declared identifier was qualified:\n{content}"
        );

        // Generic type parameters and their uses must stay bare too.
        let r = call(
            &mut ix,
            "move_decls",
            json!({"from": "N.D.Generic(T)", "newfile": "NZK.VG.cs", "prefix": "NZK.VF.NS",
                   "class": "VG"}),
        );
        assert_eq!(r["ok"], true, "{r}");
        let content = r["destination"]["content"].as_str().unwrap();
        assert!(content.contains("Generic<T>(T value)"), "generic param renamed:\n{content}");
        assert!(content.contains("T copy = value"), "generic local renamed:\n{content}");
        assert!(!content.contains("N.C.T"), "generic parameter qualified:\n{content}");

        let _ = fs::remove_dir_all(&dir);
    }

    const ONE_LINE_GENERIC: &str =
        "namespace N{  public static partial class B{public static bool Eq<T>(T a, T b){return System.Object.Equals(a, b);}}}\n";

    #[test]
    fn move_decls_does_not_qualify_a_type_declaration_name() {
        let (dir, mut ix) = indexed("move-typename", &[("a.cs", ONE_LINE_GENERIC)]);
        let r = call(
            &mut ix,
            "move_decls",
            json!({"from": "N.B.Eq(T, T)", "newfile": "NZK.X3.cs", "prefix": "NZK.X.NS"}),
        );
        // A dry run still reports the plan; the class name `B` must not become `N.B`.
        assert_eq!(r["ok"], true, "{r}");
        let content = r["destination"]["content"].as_str().unwrap();
        assert!(content.contains("class B{"), "type declaration name qualified:\n{content}");
        assert!(!content.contains("class N.B"), "type declaration name qualified:\n{content}");
        assert!(!dir.join("NZK.X3.cs").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_decls_roslyn_gate_refuses_invalid_candidates() {
        let toolchain = crate::compile_check::find_roslyn_toolchain(None).is_ok();
        let (dir, mut ix) = indexed("move-gate", &[("a.cs", ONE_LINE_GENERIC)]);
        let r = call(
            &mut ix,
            "move_decls",
            json!({"from": "N.B.Eq(T, T)", "newfile": "NZK.X3.cs", "prefix": "NZK.X.NS",
                   "apply": true}),
        );
        assert!(!dir.join("NZK.X3.cs").exists(), "nothing may be written on refusal");
        if toolchain {
            assert_eq!(r["ok"], false, "invalid candidate must be refused: {r}");
            assert_eq!(r["validated"], "roslyn", "{r}");
            assert!(!r["diagnostics"].as_array().unwrap().is_empty(), "{r}");
            // The origin is untouched too.
            assert!(fs::read_to_string(dir.join("a.cs")).unwrap().contains("class B{"));

            // Explicit override: writes anyway, with a loud warning.
            let r = call(
                &mut ix,
                "move_decls",
                json!({"from": "N.B.Eq(T, T)", "newfile": "NZK.X3.cs", "prefix": "NZK.X.NS",
                       "apply": true, "allow_invalid": true}),
            );
            assert_eq!(r["ok"], true, "{r}");
            assert_eq!(r["applied"], true, "{r}");
            assert_eq!(r["validated"], "roslyn", "{r}");
            assert!(
                r["warnings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|w| w.as_str().unwrap_or("").contains("allow_invalid")),
                "{r}"
            );
            assert!(dir.join("NZK.X3.cs").exists());
        } else {
            // No toolchain: proceed, but never claim validation.
            assert_eq!(r["ok"], true, "{r}");
            assert!(
                r["validated"].as_str().unwrap().starts_with("skipped:"),
                "{r}"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_decls_reports_skipped_validation_and_still_writes() {
        let (dir, mut ix) = indexed("move-skip", &[("a.cs", MOVE_SRC)]);
        let r = call(
            &mut ix,
            "move_decls",
            json!({"like": ["Helper"], "mtype": ["int"], "newfile": "NZK.VF.cs",
                   "prefix": "NZK.VF.NS", "class": "VF", "apply": true,
                   "_dotnet": "/nonexistent/csgraph-no-dotnet"}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], true, "{r}");
        assert!(
            r["validated"].as_str().unwrap().starts_with("skipped:"),
            "must not claim validation without a toolchain: {r}"
        );
        assert!(dir.join("NZK.VF.cs").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- parse_check (aux tool) ------------------------------------------

    // ---- find_duplicates (aux tool) --------------------------------------

    #[test]
    fn find_duplicates_reports_clusters_and_honours_knobs() {
        let body = "namespace NS {\n public class CLS {\n  public int Same(int x) {\n   int y = x + 1;\n   int z = y * 2;\n   int w = z - x;\n   return y + z + w;\n  }\n }\n}\n";
        let (dir, ix) = indexed(
            "finddup",
            &[
                ("a.cs", &body.replace("NS", "A").replace("CLS", "C")),
                ("b.cs", &body.replace("NS", "B").replace("CLS", "D")),
            ],
        );
        let r = call_aux(&ix, "find_duplicates", json!({}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["files"], 2, "{r}");
        assert!(
            r["stats"]["exactClusters"].as_i64().unwrap() >= 1,
            "identical bodies must be found: {r}"
        );
        assert!(!r["exact"].as_array().unwrap().is_empty(), "{r}");
        assert!(r["truncated"].is_boolean(), "{r}");
        // Deterministic: two runs agree.
        let again = call_aux(&ix, "find_duplicates", json!({}));
        assert_eq!(again["stats"], r["stats"], "dedupe must be deterministic");

        // `limit` caps the returned clusters.
        let limited = call_aux(&ix, "find_duplicates", json!({"limit": 1}));
        assert_eq!(limited["ok"], true, "{limited}");
        assert!(limited["exact"].as_array().unwrap().len() <= 1, "{limited}");

        // knobs are validated
        let bad = call_aux(&ix, "find_duplicates", json!({"min_lines": 0}));
        assert_eq!(bad["ok"], false, "{bad}");
        let bad = call_aux(&ix, "find_duplicates", json!({"threshold": 2.0}));
        assert_eq!(bad["ok"], false, "{bad}");

        // aux tool, not an edit tool; a cold index is refused
        assert!(is_aux_tool("find_duplicates"));
        assert!(!is_edit_tool("find_duplicates"));
        let cold = Index::new(std::env::temp_dir());
        let wrapped = invoke_aux(&cold, "find_duplicates", &json!({}));
        let body: Value =
            serde_json::from_str(wrapped["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["ok"], false, "{body}");
        assert_eq!(body["error"], "nothing indexed", "{body}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_check_is_aux_and_refuses_a_cold_index() {
        assert!(is_aux_tool("parse_check"));
        assert!(!is_edit_tool("parse_check"));
        // `exclude` accepts a glob or a repo-relative prefix.
        assert!(exclude_hits("*.nzk", "sub/a.cs.nzk"));
        assert!(exclude_hits("sub", "sub/x.cs"));
        assert!(!exclude_hits("sub", "other/x.cs"));

        // A cold index is answered without spawning the toolchain.
        let ix = Index::new(std::env::temp_dir());
        assert!(!ix.indexed);
        let wrapped = invoke_aux(&ix, "parse_check", &json!({}));
        let body: Value =
            serde_json::from_str(wrapped["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["ok"], false, "{body}");
        assert_eq!(body["error"], "nothing indexed", "{body}");
    }

    #[test]
    fn compile_diff_is_aux_and_compiles_the_candidate_against_a_baseline() {
        assert!(is_aux_tool("compile_diff"));
        assert!(!is_edit_tool("compile_diff"));

        // A cold index is answered without spawning the toolchain.
        let cold = Index::new(std::env::temp_dir());
        let wrapped = invoke_aux(&cold, "compile_diff", &json!({}));
        let body: Value =
            serde_json::from_str(wrapped["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["ok"], false, "{body}");
        assert_eq!(body["error"], "nothing indexed", "{body}");

        // `baseline_exclude` drops a file from the BASELINE only, so a compile
        // error that file introduces is reported as new. No Unity process, no
        // licence and no window focus are involved: this is offline Roslyn.
        let (dir, mut ix) = indexed(
            "compile-diff",
            &[
                (
                    "Clean.cs",
                    "namespace N { public class Clean { public int F() { return 1; } } }",
                ),
                (
                    "Added.cs",
                    "namespace N { public class Added { public int G() { return Missing(); } } }",
                ),
            ],
        );
        let r = call_aux(&ix, "compile_diff", json!({"baseline_exclude": ["Added.cs"]}));
        if r["skipped"].is_null() {
            assert_eq!(r["ok"], false, "a new CS0103 must fail the check: {r}");
            let new = r["newDiagnostics"].as_array().unwrap();
            assert!(
                new.iter().any(|d| d["code"] == "CS0103"),
                "the added file's error must be reported as NEW: {r}"
            );
            // The un-excluded file is in BOTH sides, so its diagnostics are not new.
            let clean = call_aux(&ix, "compile_diff", json!({"baseline_exclude": ["Added.cs"]}));
            assert_eq!(clean["newDiagnostics"], r["newDiagnostics"], "deterministic: {clean}");
        } else {
            // No Roslyn toolchain on this box: honest skip, never a pass.
            assert_eq!(r["ok"], false, "{r}");
            assert!(
                r["skipped"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
                "{r}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // -- D1 copy_path -------------------------------------------------------

    #[test]
    fn copy_path_dry_run_lists_the_file_and_writes_nothing() {
        let (dir, mut ix) = indexed("copy-dry", &[("src.cs", SIMPLE)]);
        let r = call(&mut ix, "copy_path", json!({"from": "src.cs", "to": "dst.cs"}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["tool"], "copy_path");
        assert_eq!(r["from"], "src.cs");
        assert_eq!(r["to"], "dst.cs");
        assert_eq!(r["kind"], "file");
        assert_eq!(r["apply"], false);
        assert_eq!(r["applied"], false);
        assert_eq!(r["overwrite"], false);
        assert_eq!(r["files"].as_array().unwrap().len(), 1, "{r}");
        let f = &r["files"][0];
        assert_eq!(f["from"], "src.cs");
        assert_eq!(f["to"], "dst.cs");
        assert_eq!(f["action"], "create");
        assert_eq!(f["bytes"].as_u64().unwrap(), SIMPLE.len() as u64);
        assert_eq!(f["sha256"], sha256_hex(SIMPLE.as_bytes()));
        assert_eq!(r["created"], 1);
        assert_eq!(r["overwritten"], 0);
        assert_eq!(r["totalBytes"].as_u64().unwrap(), SIMPLE.len() as u64);
        assert_eq!(r["written"], json!([]));
        assert_eq!(r["skipped"], json!([]));
        assert_eq!(r["dirs"], json!([]));
        assert!(!dir.join("dst.cs").exists(), "dry run must not write");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_path_apply_copies_a_file_with_matching_sha256() {
        let (dir, mut ix) = indexed("copy-apply", &[("src.cs", SIMPLE)]);
        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "src.cs", "to": "dst.cs", "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], true);
        assert_eq!(r["written"], json!(["dst.cs"]));
        assert_eq!(r["note"], "call the `index` tool to re-sync the graph");
        let copied = fs::read_to_string(dir.join("dst.cs")).unwrap();
        assert_eq!(copied, SIMPLE);
        assert_eq!(r["files"][0]["sha256"], sha256_hex(copied.as_bytes()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn copy_path_refuses_a_hardlinked_source() {
        let (dir, mut ix) = indexed("copy-hardlink-src", &[("src.cs", SIMPLE)]);
        fs::hard_link(dir.join("src.cs"), dir.join("alias.cs")).unwrap();
        // Either name of the pair is refused, and the refusal names the path.
        for from in ["src.cs", "alias.cs"] {
            let r = call(&mut ix, "copy_path", json!({"from": from, "to": "dst.cs"}));
            assert_eq!(r["ok"], false, "{r}");
            let err = r["error"].as_str().unwrap();
            assert!(err.contains("hardlink"), "{r}");
            assert!(err.contains(from), "{r}");
        }
        assert!(!dir.join("dst.cs").exists(), "a refused hardlink is never copied");
        // The pair itself is untouched.
        assert_eq!(fs::read_to_string(dir.join("alias.cs")).unwrap(), SIMPLE);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn copy_path_refuses_a_hardlinked_file_inside_a_tree_source() {
        let (dir, mut ix) = indexed("copy-hardlink-tree", &[("src/a.cs", SIMPLE)]);
        fs::hard_link(dir.join("src/a.cs"), dir.join("src/b.cs")).unwrap();
        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "src", "to": "dst", "apply": true}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert!(r["error"].as_str().unwrap().contains("hardlink"), "{r}");
        assert!(
            !dir.join("dst").exists(),
            "the tree is refused before any directory or file is created"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn copy_path_refuses_a_hardlinked_target() {
        use std::os::unix::fs::MetadataExt;
        let (dir, mut ix) = indexed("copy-hardlink-dst", &[("src.cs", SIMPLE)]);
        let other = "namespace N { public class Other { } }\n";
        fs::write(dir.join("kept.cs"), other).unwrap();
        fs::hard_link(dir.join("kept.cs"), dir.join("dst.cs")).unwrap();
        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "src.cs", "to": "dst.cs", "overwrite": true, "apply": true}),
        );
        assert_eq!(r["ok"], false, "{r}");
        let err = r["error"].as_str().unwrap();
        assert!(err.contains("hardlink"), "{r}");
        assert!(err.contains("dst.cs"), "{r}");
        // Both names still hold the original bytes and still share one inode.
        assert_eq!(fs::read_to_string(dir.join("dst.cs")).unwrap(), other);
        assert_eq!(fs::read_to_string(dir.join("kept.cs")).unwrap(), other);
        assert_eq!(fs::metadata(dir.join("dst.cs")).unwrap().nlink(), 2);
        // Nothing was staged: no journal, no backup, no temp.
        assert!(!dir.join(".csgraph-copy").exists(), "no journal may be written");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn copy_source_open_refuses_a_path_swapped_to_a_symlink() {
        let (dir, _ix) = indexed("copy-toctou", &[("src.cs", SIMPLE)]);
        let outside = dir.parent().unwrap().join(format!(
            "csgraph-edit-copy-toctou-outside-{}",
            std::process::id()
        ));
        let evil = "namespace Evil { public class X { } }\n";
        fs::write(&outside, evil).unwrap();
        // Identity is recorded while `src.cs` is the real regular file...
        let id = file_identity(&dir.join("src.cs"));
        // ...then the path is swapped for a symlink pointing outside the repo.
        fs::remove_file(dir.join("src.cs")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("src.cs")).unwrap();
        let f = CopyFile {
            from_abs: dir.join("src.cs"),
            from_display: "src.cs".to_string(),
            to_abs: dir.join("dst.cs"),
            to_display: "dst.cs".to_string(),
            bytes: SIMPLE.len() as u64,
            sha256: String::new(),
            exists: false,
            from_id: id,
            to_id: None,
        };
        let err = read_copy_source_verified(&f).unwrap_err();
        assert!(err.contains("src.cs"), "refusal must name the path: {err}");
        // The outside bytes were never read through the swapped link.
        assert_eq!(fs::read_to_string(&outside).unwrap(), evil);
        let _ = fs::remove_file(&outside);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_path_recursive_dir_recreates_empty_dirs() {
        let (dir, mut ix) = indexed("copy-dir", &[("src/a.cs", SIMPLE)]);
        fs::create_dir_all(dir.join("src/empty")).unwrap();
        let r = call(&mut ix, "copy_path", json!({"from": "src", "to": "dst"}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["kind"], "dir");
        let dirs = r["dirs"].as_array().unwrap();
        assert!(dirs.iter().any(|d| d == "dst"), "{r}");
        assert!(dirs.iter().any(|d| d == "dst/empty"), "{r}");
        assert!(r["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["to"] == "dst/a.cs"));
        assert!(!dir.join("dst").exists(), "dry run must not create directories");

        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "src", "to": "dst", "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert!(dir.join("dst/empty").is_dir(), "empty dir must be recreated");
        assert_eq!(fs::read_to_string(dir.join("dst/a.cs")).unwrap(), SIMPLE);
        assert_eq!(r["written"], json!(["dst/a.cs"]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_path_overwrite_false_refuses_collisions_without_writing() {
        let from_text = "namespace N { public class From { } }\n";
        let to_text = "namespace N { public class To { } }\n";
        let (dir, mut ix) = indexed("copy-collide", &[("src.cs", from_text), ("dst.cs", to_text)]);
        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "src.cs", "to": "dst.cs", "apply": true}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert_eq!(r["collisions"], json!(["dst.cs"]));
        assert_eq!(r["applied"], false);
        assert_eq!(r["written"], json!([]));
        assert_eq!(fs::read_to_string(dir.join("dst.cs")).unwrap(), to_text);

        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "src.cs", "to": "dst.cs", "overwrite": true, "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["created"], 0);
        assert_eq!(r["overwritten"], 1);
        assert_eq!(r["written"], json!(["dst.cs"]));
        assert_eq!(fs::read_to_string(dir.join("dst.cs")).unwrap(), from_text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_path_refuses_inside_destination_and_escape() {
        let (dir, mut ix) = indexed("copy-refuse", &[("src/a.cs", SIMPLE)]);
        let r = call(&mut ix, "copy_path", json!({"from": "src", "to": "src/sub"}));
        assert_eq!(r["ok"], false, "{r}");
        assert!(r["error"].as_str().unwrap().contains("inside"), "{r}");

        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "src/a.cs", "to": "../escaped.cs", "apply": true}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert!(!dir.parent().unwrap().join("escaped.cs").exists());

        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "src/a.cs", "to": "missing/x.cs"}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert!(r["dirs"].as_array().unwrap().iter().any(|d| d == "missing"));
        assert!(!dir.join("missing").exists(), "dry run must not create dirs");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_path_refuses_oversize_requests_honestly() {
        let (dir, mut ix) = indexed("copy-guard", &[("a.cs", SIMPLE), ("b.cs", SIMPLE)]);
        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "a.cs", "to": "c.cs", "max_files": 0}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert!(r["error"].as_str().unwrap().contains("max_files"), "{r}");
        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "a.cs", "to": "c.cs", "max_bytes": 1}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert!(r["error"].as_str().unwrap().contains("max_bytes"), "{r}");
        assert!(!dir.join("c.cs").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn copy_path_skips_source_symlinks_and_reports_them() {
        let (dir, mut ix) = indexed("copy-link", &[("src/real.cs", SIMPLE)]);
        std::os::unix::fs::symlink("real.cs", dir.join("src/link.cs")).unwrap();
        let r = call(&mut ix, "copy_path", json!({"from": "src", "to": "dst"}));
        assert_eq!(r["ok"], true, "{r}");
        assert!(
            r["files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["to"] != "dst/link.cs"),
            "{r}"
        );
        assert!(
            r["skipped"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["path"] == "src/link.cs" && s["reason"] == "symlink"),
            "{r}"
        );
        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "src", "to": "dst", "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert!(dir.join("dst/real.cs").is_file());
        assert!(!dir.join("dst/link.cs").exists());

        // A symlink as the source root itself is refused outright.
        std::os::unix::fs::symlink("src", dir.join("srclink")).unwrap();
        let r = call(&mut ix, "copy_path", json!({"from": "srclink", "to": "dst2"}));
        assert_eq!(r["ok"], false, "{r}");

        // A symlinked destination directory is refused too.
        std::os::unix::fs::symlink("src", dir.join("dlink")).unwrap();
        let r = call(&mut ix, "copy_path", json!({"from": "dst/real.cs", "to": "dlink/x.cs"}));
        assert_eq!(r["ok"], false, "{r}");
        assert!(!dir.join("src/x.cs").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    // -- R2 durable copy journal / recovery ---------------------------------

    /// Install `CSGRAPH_COPY_FAULT` for this test thread only and clear it on
    /// drop. A thread-local keeps parallel libtest threads from seeing another
    /// test's crash injection (the env var is process-global).
    struct FaultGuard;

    impl Drop for FaultGuard {
        fn drop(&mut self) {
            TEST_COPY_FAULT.with(|c| *c.borrow_mut() = None);
        }
    }

    fn fault_guard(fault: &str) -> FaultGuard {
        TEST_COPY_FAULT.with(|c| *c.borrow_mut() = Some(fault.to_string()));
        FaultGuard
    }

    /// Every path under `dir` whose name contains `needle` (recursive).
    fn paths_containing(dir: &Path, needle: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            if let Ok(rd) = fs::read_dir(&d) {
                for e in rd.filter_map(|e| e.ok()) {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p.clone());
                    }
                    if p.file_name()
                        .and_then(|s| s.to_str())
                        .map(|s| s.contains(needle))
                        .unwrap_or(false)
                    {
                        out.push(p);
                    }
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn copy_path_crash_after_first_rename_recovers_and_is_idempotent() {
        let (dir, mut ix) = indexed(
            "copy-fault-recover",
            &[
                ("src/aaa/new.cs", "// new aaa\n"),
                ("src/zzz.cs", "// new zzz\n"),
            ],
        );
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::write(dir.join("out/zzz.cs"), "// original zzz\n").unwrap();

        {
            let _fault = fault_guard("after-rename:1");
            let r = call(
                &mut ix,
                "copy_path",
                json!({"from": "src", "to": "out", "overwrite": true, "apply": true}),
            );
            assert_eq!(r["ok"], false, "{r}");
            assert_eq!(r["fault"], "after-rename:1", "{r}");
            assert_eq!(r["applied"], false, "{r}");
        }
        // Crash state stays on disk: the created file landed, the overwritten
        // target was renamed to its backup, the journal survives.
        assert!(dir.join("out/aaa/new.cs").is_file(), "created file must land");
        assert!(!dir.join("out/zzz.cs").exists(), "target is renamed away");
        assert_eq!(paths_containing(&dir, ".csgraph-bak-").len(), 1);
        assert_eq!(fs::read_dir(dir.join(".csgraph-copy")).unwrap().count(), 1);

        // Recovery is driven entirely by the journal; a second server process
        // performs exactly this call.
        let rec = call(&mut ix, "copy_path", json!({"action": "recover", "apply": true}));
        assert_eq!(rec["ok"], true, "{rec}");
        assert_eq!(rec["action"], "recover", "{rec}");
        assert_eq!(rec["journals"], 1, "{rec}");
        assert_eq!(rec["restored"], 1, "{rec}");
        assert_eq!(rec["removed"], 1, "{rec}");
        assert_eq!(rec["removedDirs"], 1, "{rec}");
        assert_eq!(rec["incomplete"], false, "{rec}");
        assert_eq!(
            fs::read_to_string(dir.join("out/zzz.cs")).unwrap(),
            "// original zzz\n"
        );
        assert!(!dir.join("out/aaa/new.cs").exists());
        assert!(!dir.join("out/aaa").exists());
        assert!(!dir.join(".csgraph-copy").exists());
        assert!(paths_containing(&dir, ".csgraph-bak-").is_empty());

        // Idempotent: a second recover is a clean no-op.
        let again = call(&mut ix, "copy_path", json!({"action": "recover", "apply": true}));
        assert_eq!(again["ok"], true, "{again}");
        assert_eq!(again["journals"], 0, "{again}");
        assert_eq!(again["restored"], 0, "{again}");
        assert_eq!(again["removed"], 0, "{again}");
        assert_eq!(again["removedDirs"], 0, "{again}");
        assert_eq!(again["incomplete"], false, "{again}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_path_before_journal_delete_fault_restores_original_bytes() {
        let (dir, mut ix) = indexed("copy-fault-bjd", &[("src.cs", "// new\n")]);
        fs::write(dir.join("dst.cs"), "// original\n").unwrap();
        {
            let _fault = fault_guard("before-journal-delete");
            let r = call(
                &mut ix,
                "copy_path",
                json!({"from": "src.cs", "to": "dst.cs", "overwrite": true, "apply": true}),
            );
            assert_eq!(r["ok"], false, "{r}");
            assert_eq!(r["fault"], "before-journal-delete", "{r}");
        }
        // All new bytes landed, but the backup and the journal are still there.
        assert_eq!(fs::read_to_string(dir.join("dst.cs")).unwrap(), "// new\n");
        assert_eq!(paths_containing(&dir, ".csgraph-bak-").len(), 1);
        assert!(dir.join(".csgraph-copy").is_dir());

        let rec = call(&mut ix, "copy_path", json!({"action": "recover", "apply": true}));
        assert_eq!(rec["ok"], true, "{rec}");
        assert_eq!(rec["journals"], 1, "{rec}");
        assert_eq!(rec["restored"], 1, "{rec}");
        assert_eq!(rec["incomplete"], false, "{rec}");
        assert_eq!(
            fs::read_to_string(dir.join("dst.cs")).unwrap(),
            "// original\n"
        );
        assert!(paths_containing(&dir, ".csgraph-bak-").is_empty());
        assert!(!dir.join(".csgraph-copy").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_path_after_commit_fault_keeps_the_committed_copy() {
        let (dir, mut ix) = indexed(
            "copy-fault-commit",
            &[("src/a.cs", "// new a\n"), ("src/zzz.cs", "// new z\n")],
        );
        fs::create_dir_all(dir.join("src/empty")).unwrap();
        fs::create_dir_all(dir.join("out")).unwrap();
        fs::write(dir.join("out/a.cs"), "// old a\n").unwrap();
        fs::write(dir.join("out/zzz.cs"), "// old z\n").unwrap();
        {
            // Fires after the fsynced commit marker and after 1 backup delete.
            let _fault = fault_guard("after-commit:1");
            let r = call(
                &mut ix,
                "copy_path",
                json!({"from": "src", "to": "out", "overwrite": true, "apply": true}),
            );
            assert_eq!(r["ok"], false, "{r}");
            assert_eq!(r["fault"], "after-commit:1", "{r}");
        }
        // Committed crash state: every target holds the new bytes, one backup is
        // already deleted, one is left, and the journal says "committed".
        assert_eq!(
            fs::read_to_string(dir.join("out/a.cs")).unwrap(),
            "// new a\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("out/zzz.cs")).unwrap(),
            "// new z\n"
        );
        assert_eq!(paths_containing(&dir, ".csgraph-bak-").len(), 1);
        let journal_path = fs::read_dir(dir.join(".csgraph-copy"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let journal = fs::read_to_string(&journal_path).unwrap();
        assert!(journal.contains("\"committed\""), "{journal}");

        let rec = call(&mut ix, "copy_path", json!({"action": "recover", "apply": true}));
        assert_eq!(rec["ok"], true, "{rec}");
        assert_eq!(rec["journals"], 1, "{rec}");
        assert_eq!(rec["restored"], 0, "{rec}");
        assert_eq!(rec["removed"], 1, "{rec}");
        assert_eq!(rec["incomplete"], false, "{rec}");
        // Cleanup-only: the committed bytes stay and the recreated empty dir
        // (a committed artifact) is not removed.
        assert_eq!(
            fs::read_to_string(dir.join("out/a.cs")).unwrap(),
            "// new a\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("out/zzz.cs")).unwrap(),
            "// new z\n"
        );
        assert!(dir.join("out/empty").is_dir(), "committed empty dir must survive");
        assert!(paths_containing(&dir, ".csgraph-bak-").is_empty());
        assert!(!dir.join(".csgraph-copy").exists());

        let again = call(&mut ix, "copy_path", json!({"action": "recover", "apply": true}));
        assert_eq!(again["ok"], true, "{again}");
        assert_eq!(again["journals"], 0, "{again}");
        assert_eq!(again["restored"], 0, "{again}");
        assert_eq!(again["incomplete"], false, "{again}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_path_recover_defaults_to_a_read_only_plan() {
        let (dir, mut ix) = indexed("copy-recover-dry", &[("src.cs", "// new\n")]);
        fs::write(dir.join("dst.cs"), "// original\n").unwrap();
        {
            let _fault = fault_guard("before-journal-delete");
            let r = call(
                &mut ix,
                "copy_path",
                json!({"from": "src.cs", "to": "dst.cs", "overwrite": true, "apply": true}),
            );
            assert_eq!(r["fault"], "before-journal-delete", "{r}");
        }
        assert_eq!(fs::read_to_string(dir.join("dst.cs")).unwrap(), "// new\n");

        // No `apply`: the plan is reported and nothing on disk changes.
        let plan = call(&mut ix, "copy_path", json!({"action": "recover"}));
        assert_eq!(plan["ok"], true, "{plan}");
        assert_eq!(plan["apply"], false, "{plan}");
        assert_eq!(plan["applied"], false, "{plan}");
        assert_eq!(plan["dryRun"], true, "{plan}");
        assert_eq!(plan["journals"], 1, "{plan}");
        assert_eq!(plan["restored"], 1, "{plan}");
        assert_eq!(plan["incomplete"], false, "{plan}");
        assert!(
            plan["plan"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["action"] == "restore"),
            "{plan}"
        );
        assert_eq!(
            fs::read_to_string(dir.join("dst.cs")).unwrap(),
            "// new\n",
            "a dry-run recover must not restore"
        );
        assert_eq!(
            paths_containing(&dir, ".csgraph-bak-").len(),
            1,
            "the backup must survive the dry run"
        );
        assert!(
            dir.join(".csgraph-copy").is_dir(),
            "the journal must survive the dry run"
        );

        // `apply:true` performs exactly the planned restore.
        let done = call(
            &mut ix,
            "copy_path",
            json!({"action": "recover", "apply": true}),
        );
        assert_eq!(done["ok"], true, "{done}");
        assert_eq!(done["applied"], true, "{done}");
        assert_eq!(
            fs::read_to_string(dir.join("dst.cs")).unwrap(),
            "// original\n"
        );
        assert!(paths_containing(&dir, ".csgraph-bak-").is_empty());
        assert!(!dir.join(".csgraph-copy").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_path_recover_refuses_crafted_and_hash_mismatched_journals() {
        let (dir, mut ix) = indexed(
            "copy-recover-crafted",
            &[("victim_a.cs", "// keep a\n"), ("victim_b.cs", "// keep b\n")],
        );
        let jroot = dir.join(".csgraph-copy");
        fs::create_dir_all(&jroot).unwrap();
        // (1) Not engine-shaped: no `tool`/`repo` and no post-write hash.
        let foreign = json!({
            "version": 1, "nonce": "evil", "status": "pending",
            "from": "x", "to": "y", "createdDirs": [],
            "entries": [{"target": "victim_a.cs", "backup": Value::Null,
                         "existed": false, "sha256Before": Value::Null}],
        });
        fs::write(
            jroot.join("foreign.json"),
            serde_json::to_vec(&foreign).unwrap(),
        )
        .unwrap();
        // (2) Engine-shaped, but the recorded post-write hash does not match the
        // bytes actually on disk.
        let mismatched = json!({
            "tool": "copy_path", "version": 1, "nonce": "mismatch",
            "repo": journal_repo_id(&ix), "status": "pending",
            "from": "x", "to": "y", "createdDirs": [],
            "entries": [{"target": "victim_b.cs", "backup": Value::Null,
                         "existed": false, "sha256Before": Value::Null,
                         "sha256After": sha256_hex(b"something else\n")}],
        });
        fs::write(
            jroot.join("mismatch.json"),
            serde_json::to_vec(&mismatched).unwrap(),
        )
        .unwrap();

        // Even a dry run refuses them (incomplete) and touches nothing.
        let dry = call(&mut ix, "copy_path", json!({"action": "recover"}));
        assert_eq!(dry["ok"], false, "{dry}");
        assert_eq!(dry["incomplete"], true, "{dry}");
        assert_eq!(
            fs::read_to_string(dir.join("victim_a.cs")).unwrap(),
            "// keep a\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("victim_b.cs")).unwrap(),
            "// keep b\n"
        );
        assert!(jroot.join("foreign.json").is_file());
        assert!(jroot.join("mismatch.json").is_file());

        // With apply:true they still delete nothing.
        let applied = call(
            &mut ix,
            "copy_path",
            json!({"action": "recover", "apply": true}),
        );
        assert_eq!(applied["ok"], false, "{applied}");
        assert_eq!(applied["incomplete"], true, "{applied}");
        assert_eq!(applied["restored"], 0, "{applied}");
        assert_eq!(applied["removed"], 0, "{applied}");
        assert_eq!(
            fs::read_to_string(dir.join("victim_a.cs")).unwrap(),
            "// keep a\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("victim_b.cs")).unwrap(),
            "// keep b\n"
        );
        assert!(
            jroot.join("foreign.json").is_file(),
            "a refused journal is kept: {applied}"
        );
        assert!(
            jroot.join("mismatch.json").is_file(),
            "a refused journal is kept: {applied}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_path_recover_refuses_a_forged_backup_name() {
        // Variants A/C: an engine-shaped `existed:true` entry whose `backup`
        // names an unrelated in-repo file. Nothing may be moved or deleted.
        let (dir, mut ix) = indexed(
            "copy-recover-forged-backup",
            &[("target.cs", "// target\n")],
        );
        fs::write(dir.join("keep.cs"), "// keep\n").unwrap();
        fs::write(dir.join("unrelated.cs"), "// unrelated\n").unwrap();
        let jroot = dir.join(".csgraph-copy");
        fs::create_dir_all(&jroot).unwrap();
        let base = json!({
            "tool": "copy_path", "version": 1,
            "repo": journal_repo_id(&ix), "status": "pending",
            "from": "x", "to": "y", "createdDirs": [],
        });
        for (tag, backup) in [("A", "keep.cs"), ("C", "unrelated.cs")] {
            let mut j = base.clone();
            j["nonce"] = json!(format!("forge{tag}"));
            j["entries"] = json!([{
                "target": "target.cs", "backup": backup, "existed": true,
                "sha256Before": sha256_hex(b"// keep\n"),
                "sha256After": sha256_hex(b"// target\n"),
            }]);
            fs::write(
                jroot.join(format!("forge{tag}.json")),
                serde_json::to_vec(&j).unwrap(),
            )
            .unwrap();
        }

        let r = call(
            &mut ix,
            "copy_path",
            json!({"action": "recover", "apply": true}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert_eq!(r["incomplete"], true, "{r}");
        assert_eq!(r["restored"], 0, "{r}");
        assert_eq!(r["removed"], 0, "{r}");
        assert_eq!(
            fs::read_to_string(dir.join("target.cs")).unwrap(),
            "// target\n"
        );
        assert_eq!(fs::read_to_string(dir.join("keep.cs")).unwrap(), "// keep\n");
        assert_eq!(
            fs::read_to_string(dir.join("unrelated.cs")).unwrap(),
            "// unrelated\n"
        );
        assert!(jroot.join("forgeA.json").is_file());
        assert!(jroot.join("forgeC.json").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn copy_path_recover_refuses_a_symlinked_journal_target() {
        let (dir, mut ix) = indexed("copy-crafted-link", &[("src.cs", "// x\n")]);
        let outside = std::env::temp_dir().join(format!(
            "csgraph-copy-out-{}-{}",
            std::process::id(),
            "link"
        ));
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&outside).unwrap();
        let victim = outside.join("victim.txt");
        fs::write(&victim, "PRECIOUS\n").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("link")).unwrap();
        let jroot = dir.join(".csgraph-copy");
        fs::create_dir_all(&jroot).unwrap();
        let crafted = json!({
            "tool": "copy_path", "version": 1, "nonce": "crafted",
            "repo": journal_repo_id(&ix),
            "status": "pending",
            "from": "src.cs", "to": "x", "createdDirs": [],
            "entries": [{"target": "link/victim.txt", "backup": Value::Null,
                         "existed": false, "sha256Before": Value::Null,
                         "sha256After": sha256_hex(b"PRECIOUS\n")}],
        });
        fs::write(
            jroot.join("crafted.json"),
            serde_json::to_vec(&crafted).unwrap(),
        )
        .unwrap();

        let rec = call(&mut ix, "copy_path", json!({"action": "recover", "apply": true}));
        assert_eq!(rec["ok"], false, "{rec}");
        assert_eq!(rec["incomplete"], true, "{rec}");
        assert!(victim.is_file(), "outside victim must survive: {rec}");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "PRECIOUS\n");
        assert!(
            jroot.join("crafted.json").is_file(),
            "a refused journal is left in place: {rec}"
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn copy_path_recover_ignores_a_symlinked_journal_file() {
        let (dir, mut ix) = indexed("copy-crafted-jfile", &[("src.cs", "// x\n")]);
        let outside = std::env::temp_dir().join(format!(
            "csgraph-copy-out-{}-{}",
            std::process::id(),
            "jfile"
        ));
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&outside).unwrap();
        let evil = outside.join("evil.json");
        let crafted = json!({
            "version": 1, "nonce": "evil", "status": "pending",
            "from": "x", "to": "y", "createdDirs": [],
            "entries": [{"target": "src.cs", "backup": Value::Null,
                         "existed": false, "sha256Before": Value::Null}],
        });
        fs::write(&evil, serde_json::to_vec(&crafted).unwrap()).unwrap();
        let jroot = dir.join(".csgraph-copy");
        fs::create_dir_all(&jroot).unwrap();
        std::os::unix::fs::symlink(&evil, jroot.join("evil.json")).unwrap();

        let rec = call(&mut ix, "copy_path", json!({"action": "recover", "apply": true}));
        assert_eq!(rec["ok"], true, "{rec}");
        assert_eq!(rec["journals"], 0, "{rec}");
        assert!(dir.join("src.cs").is_file(), "src.cs must survive: {rec}");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn copy_path_refuses_a_symlinked_journal_root() {
        let (dir, mut ix) = indexed("copy-crafted-root", &[("src.cs", "// new\n")]);
        fs::write(dir.join("dst.cs"), "// original\n").unwrap();
        let outside = std::env::temp_dir().join(format!(
            "csgraph-copy-out-{}-{}",
            std::process::id(),
            "jroot"
        ));
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join(".csgraph-copy")).unwrap();

        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "src.cs", "to": "dst.cs", "overwrite": true, "apply": true}),
        );
        assert_eq!(r["ok"], false, "{r}");
        assert!(r["error"].as_str().unwrap().contains("symlink"), "{r}");
        assert_eq!(
            fs::read_to_string(dir.join("dst.cs")).unwrap(),
            "// original\n"
        );
        assert!(
            fs::read_dir(&outside).unwrap().next().is_none(),
            "no journal may land outside the repo: {r}"
        );
        assert!(
            fs::symlink_metadata(dir.join(".csgraph-copy"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink itself is untouched"
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn copy_path_create_dirs_fault_is_recoverable() {
        let (dir, mut ix) = indexed("copy-fault-dirs", &[("src/deep/x.cs", "// x\n")]);
        {
            let _fault = fault_guard("create-dirs");
            let r = call(
                &mut ix,
                "copy_path",
                json!({"from": "src", "to": "out", "apply": true}),
            );
            assert_eq!(r["ok"], false, "{r}");
            assert_eq!(r["fault"], "create-dirs", "{r}");
        }
        assert!(dir.join("out/deep").is_dir(), "dirs land before the trip");
        assert!(!dir.join("out/deep/x.cs").exists());
        assert!(dir.join(".csgraph-copy").is_dir());

        let rec = call(&mut ix, "copy_path", json!({"action": "recover", "apply": true}));
        assert_eq!(rec["ok"], true, "{rec}");
        assert_eq!(rec["journals"], 1, "{rec}");
        assert_eq!(rec["restored"], 0, "{rec}");
        assert_eq!(rec["removed"], 0, "{rec}");
        assert_eq!(rec["removedDirs"], 2, "{rec}");
        assert!(!dir.join("out").exists());
        assert!(!dir.join(".csgraph-copy").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_path_success_leaves_no_copy_artifacts() {
        let (dir, mut ix) = indexed("copy-clean", &[("src.cs", "// new\n")]);
        fs::write(dir.join("dst.cs"), "// original\n").unwrap();
        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "src.cs", "to": "dst.cs", "overwrite": true, "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], true, "{r}");
        assert!(r.get("unrecovered").is_none(), "{r}");
        assert_eq!(fs::read_to_string(dir.join("dst.cs")).unwrap(), "// new\n");
        assert!(!dir.join(".csgraph-copy").exists());
        assert!(paths_containing(&dir, ".csgraph-bak-").is_empty());
        assert!(paths_containing(&dir, ".csgraph-tmp-").is_empty());
        let _ = fs::remove_dir_all(&dir);

        // A directory apply (several backups + created dirs) cleans up too.
        let (dir, mut ix) = indexed(
            "copy-clean-dir",
            &[("src/a/x.cs", "// a\n"), ("src/b.cs", "// b\n")],
        );
        fs::create_dir_all(dir.join("out/a")).unwrap();
        fs::write(dir.join("out/a/x.cs"), "// old a\n").unwrap();
        fs::write(dir.join("out/b.cs"), "// old b\n").unwrap();
        let r = call(
            &mut ix,
            "copy_path",
            json!({"from": "src", "to": "out", "overwrite": true, "apply": true}),
        );
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["applied"], true, "{r}");
        assert_eq!(r["overwritten"], 2, "{r}");
        assert_eq!(fs::read_to_string(dir.join("out/a/x.cs")).unwrap(), "// a\n");
        assert_eq!(fs::read_to_string(dir.join("out/b.cs")).unwrap(), "// b\n");
        assert!(!dir.join(".csgraph-copy").exists());
        assert!(paths_containing(&dir, ".csgraph-bak-").is_empty());
        assert!(paths_containing(&dir, ".csgraph-tmp-").is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_path_apply_refuses_a_pending_journal_unless_recover_true() {
        let (dir, mut ix) = indexed(
            "copy-pending",
            &[("src.cs", "// new src\n"), ("other.cs", "// other\n")],
        );
        fs::write(dir.join("dst.cs"), "// original\n").unwrap();
        {
            let _fault = fault_guard("after-rename:1");
            let r = call(
                &mut ix,
                "copy_path",
                json!({"from": "src.cs", "to": "dst.cs", "overwrite": true, "apply": true}),
            );
            assert_eq!(r["fault"], "after-rename:1", "{r}");
        }
        // A dry run still plans, but reports the unrecovered journal.
        let dry = call(
            &mut ix,
            "copy_path",
            json!({"from": "other.cs", "to": "other_out.cs"}),
        );
        assert_eq!(dry["ok"], true, "{dry}");
        assert_eq!(dry["unrecovered"].as_array().unwrap().len(), 1, "{dry}");

        // apply:true refuses until recover:true is passed.
        let refused = call(
            &mut ix,
            "copy_path",
            json!({"from": "other.cs", "to": "other_out.cs", "apply": true}),
        );
        assert_eq!(refused["ok"], false, "{refused}");
        assert!(
            refused["error"].as_str().unwrap().contains("unrecovered"),
            "{refused}"
        );
        assert_eq!(
            refused["unrecovered"].as_array().unwrap().len(),
            1,
            "{refused}"
        );
        assert!(!dir.join("other_out.cs").exists());

        // recover:true restores first, then the copy proceeds.
        let done = call(
            &mut ix,
            "copy_path",
            json!({"from": "other.cs", "to": "other_out.cs", "apply": true, "recover": true}),
        );
        assert_eq!(done["ok"], true, "{done}");
        assert_eq!(done["applied"], true, "{done}");
        assert_eq!(done["recovery"]["journals"], 1, "{done}");
        assert!(dir.join("other_out.cs").is_file());
        assert_eq!(
            fs::read_to_string(dir.join("dst.cs")).unwrap(),
            "// original\n"
        );
        assert!(!dir.join(".csgraph-copy").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    // -- D4 counts scoping --------------------------------------------------

    const TWO_ROOTS: &[(&str, &str)] = &[
        (
            "a/one.cs",
            "namespace A { public class One { public int M() { return 1; } } }\n",
        ),
        (
            "b/two.cs",
            "namespace B { public class Two { public int M() { return 2; } } }\n",
        ),
    ];

    fn sorted_keys(v: &Value) -> Vec<String> {
        let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    #[test]
    fn counts_scope_roots_exclude_byroot_and_file() {
        let (dir, ix) = indexed("counts-scope", TWO_ROOTS);
        let r = call_aux(&ix, "counts", json!({}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(r["scope"], "corpus");
        assert_eq!(r["roots"], json!(["."]));
        assert_eq!(r["files"], 2);
        assert_eq!(
            sorted_keys(&r),
            vec![
                "byEdgeKind",
                "byKind",
                "declarations",
                "edges",
                "files",
                "nodes",
                "ok",
                "repo",
                "roots",
                "scope",
                "tool"
            ]
        );
        let all_decls = r["declarations"].as_i64().unwrap();
        let all_nodes = r["nodes"].as_i64().unwrap();
        let all_edges = r["edges"].as_i64().unwrap();

        // roots:["a"] drops the b-only declaration.
        let s = call_aux(&ix, "counts", json!({"roots": ["a"]}));
        assert_eq!(s["ok"], true, "{s}");
        assert_eq!(s["scoped"], true);
        assert_eq!(s["files"], 1);
        assert_eq!(s["roots"], json!(["a"]));
        assert_eq!(s["filter"]["roots"], json!(["a"]));
        assert_eq!(s["filter"]["exclude"], json!([]));
        assert!(s["declarations"].as_i64().unwrap() < all_decls, "{r} vs {s}");
        assert!(s["nodes"].as_i64().unwrap() < all_nodes);
        assert!(s["edges"].as_i64().unwrap() <= all_edges);
        assert_eq!(
            sorted_keys(&s),
            vec![
                "byEdgeKind",
                "byKind",
                "byRoot",
                "declarations",
                "edges",
                "files",
                "filter",
                "nodes",
                "ok",
                "repo",
                "roots",
                "scope",
                "scoped",
                "tool"
            ]
        );

        // `paths` is an alias for `roots`, and `exclude` subtracts.
        let e = call_aux(&ix, "counts", json!({"paths": "a", "exclude": ["nope"]}));
        assert_eq!(e["files"], 1, "{e}");
        assert_eq!(e["roots"], json!(["a"]));
        assert_eq!(e["filter"]["exclude"], json!(["nope"]));
        let e = call_aux(&ix, "counts", json!({"exclude": ["b"]}));
        assert_eq!(e["files"], 1, "{e}");
        assert_eq!(e["declarations"], s["declarations"]);

        // byRoot splits the two requested roots.
        let br = call_aux(&ix, "counts", json!({"roots": ["a", "b"]}));
        assert_eq!(br["files"], 2, "{br}");
        assert_eq!(br["byRoot"]["a"]["files"], 1, "{br}");
        assert_eq!(br["byRoot"]["b"]["files"], 1, "{br}");
        assert_eq!(br["byRoot"]["a"]["declarations"], s["declarations"]);
        assert_eq!(br["byRoot"]["b"]["declarations"], s["declarations"]);
        // Per-root edges must be the single-root edges, not every edge touching
        // any requested root (regression: byRoot edges used the whole scope).
        let sb = call_aux(&ix, "counts", json!({"roots": ["b"]}));
        assert_eq!(br["byRoot"]["a"]["edges"], s["edges"], "{br} vs {s}");
        assert_eq!(br["byRoot"]["b"]["edges"], sb["edges"], "{br} vs {sb}");

        // `file` scopes to exactly one file.
        let f = call_aux(&ix, "counts", json!({"file": "a/one.cs"}));
        assert_eq!(f["ok"], true, "{f}");
        assert_eq!(f["files"], 1, "{f}");
        assert_eq!(f["roots"], json!(["a/one.cs"]));
        assert_eq!(f["declarations"], s["declarations"]);

        // Symbol counts are restricted too and carry `filter`; unscoped symbol
        // counts stay untouched.
        let sym = call_aux(&ix, "counts", json!({"q": "A.One", "roots": ["b"]}));
        assert_eq!(sym["ok"], true, "{sym}");
        assert_eq!(sym["scope"], "symbol");
        assert_eq!(sym["definitions"], 0, "{sym}");
        assert_eq!(sym["matches"], 0, "{sym}");
        assert_eq!(sym["filter"]["roots"], json!(["b"]));
        let sym_all = call_aux(&ix, "counts", json!({"q": "A.One"}));
        assert_eq!(sym_all["definitions"], 1, "{sym_all}");
        assert!(sym_all.get("filter").is_none(), "{sym_all}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn code_tokens_is_total_when_a_carried_block_comment_closes_mid_line() {
        // A line that begins inside a `/* … */` block comment and *closes* it
        // on the same line used to pop the only scanner frame, so the next
        // character hit `stack.last().copied().unwrap()` and panicked
        // (`*/ x` with `in_block = true`). Both scanners must stay total and
        // resume in code after the terminator.
        let mut in_block = true;
        let toks = code_tokens("*/ x", &mut in_block);
        assert!(!in_block, "the carried block comment must be closed");
        assert_eq!(toks.len(), 1, "one code token must survive the terminator");
        assert_eq!(toks[0].text, "x");
        assert!(toks[0].ident);

        // Same shape through `transform_line`.
        let mut in_block = true;
        let out = transform_line("*/ x", &mut in_block, &mut |tok, _dot, _f, _p| {
            if tok == "x" { Some("y".to_string()) } else { None }
        });
        assert!(!in_block);
        assert_eq!(out, "*/ y");

        // The terminator-only line and the real corpus shape (`*/}}`) stay
        // total too.
        let mut in_block = true;
        assert!(code_tokens("*/", &mut in_block).is_empty());
        assert!(!in_block);
        let mut in_block = true;
        let toks = code_tokens("*/}}", &mut in_block);
        assert!(!in_block);
        assert_eq!(
            toks.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["}", "}"]
        );
    }
}
