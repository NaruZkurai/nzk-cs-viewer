//! api — port of the `Handler` routing in `tools/csgraph/server.py`.
//!
//! `server.py` answers `/api/*` over `http.server`; the Tauri host answers the
//! same routes over IPC. This module is the routing half only: it parses the
//! query, calls into [`Index`], and returns `(status, json)` exactly where the
//! Python returns an HTTP status. The static route is kept for parity even
//! though the v2 host serves the bundled frontend itself.
//!
//! The Python's `_query` returns `urllib.parse.parse_qs`'s dict of name ->
//! list of decoded values. The host hands us a [`Value`]; both a JSON object
//! and a raw query string are accepted, and every value is percent-decoded here
//! because the frontend may pass it either way.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::ser::{Serialize, Serializer};
use serde_json::{json, Map, Value};

use crate::config::{load_settings, save_settings, UI_THEMES};
use crate::index::{normpath, Index};

/// `POST /api/index/cancel` — Stop for an in-flight `/api/index`.
///
/// The host intercepts this path *before* taking the engine lock (an in-flight
/// build holds that lock for its whole duration), so the host flips the shared
/// flag itself; this constant keeps both sides on one spelling, and the route
/// below still works for callers that hold the index directly.
pub const CANCEL_PATH: &str = "/api/index/cancel";

/// A response body that is either owned or a shared cached value.
///
/// Document synthesis is memoized as an `Arc`; the fast paths below hand that
/// `Arc` straight to the serializer, so a cache hit at peak costs a pointer
/// copy instead of a deep clone of a document that can be tens of thousands of
/// lines. Every other route still uses an owned [`Value`].
pub enum Payload {
    Value(Value),
    Shared(Arc<Value>),
}

impl Serialize for Payload {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Payload::Value(v) => v.serialize(s),
            Payload::Shared(a) => a.serialize(s),
        }
    }
}

impl From<Value> for Payload {
    fn from(v: Value) -> Self {
        Payload::Value(v)
    }
}

/// Route one request, returning the body as a [`Payload`] so shared cached
/// values are never cloned. This is what the Tauri host calls.
pub fn handle_payload(
    index: &mut Index,
    method: &str,
    path: &str,
    query: &Value,
    body: Option<Value>,
) -> (u16, Payload) {
    if method.eq_ignore_ascii_case("GET") {
        if let Some(hit) = shared_route(index, path, query) {
            return hit;
        }
    }
    let (status, value) = handle(index, method, path, query, body);
    (status, Payload::Value(value))
}

/// The routes that can answer from a shared cached payload.
fn shared_route(index: &mut Index, path: &str, query: &Value) -> Option<(u16, Payload)> {
    match path {
        "/api/doc" => {
            let node_id = qfirst(query, "node").unwrap_or_default();
            Some(match index.doc_source(&node_id) {
                None => err_p(404, "no synthesized document for node"),
                Some(s) => (200, Payload::Shared(s)),
            })
        }
        "/api/source" => {
            let to = qint(query, "to", 0);
            let node = qfirst(query, "node");
            let src = index.source_shared(
                qfirst(query, "file").as_deref(),
                qint(query, "line", 1),
                qint(query, "context", 30),
                node.as_deref(),
                if to != 0 { Some(to) } else { None },
                &qfirst(query, "mode").unwrap_or_else(|| "doc".to_string()),
            );
            Some(match src {
                None if node.is_some() => {
                    err_p(404, "no source: symbol is external or not declared")
                }
                None => err_p(404, "file not indexed"),
                Some(s) => (200, Payload::Shared(s)),
            })
        }
        _ => None,
    }
}

fn err_p(code: u16, msg: impl Into<String>) -> (u16, Payload) {
    (
        code,
        Payload::Value(json!({"ok": false, "error": msg.into()})),
    )
}

/// `Handler` — route one request.
///
/// Returns the HTTP status Python would send and the JSON body (or the file
/// text for the static route, as a JSON string).
pub fn handle(
    index: &mut Index,
    method: &str,
    path: &str,
    query: &Value,
    body: Option<Value>,
) -> (u16, Value) {
    if method.eq_ignore_ascii_case("POST") {
        do_post(index, path, query, body)
    } else {
        do_get(index, path, query)
    }
}

fn err(code: u16, msg: impl Into<String>) -> (u16, Value) {
    (code, json!({"ok": false, "error": msg.into()}))
}

// ---------------------------------------------------------------------------
// query parsing (`parse_qs` + the `_int`/`_list_arg` helpers)
// ---------------------------------------------------------------------------

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `urllib.parse.unquote_plus` — `%XX` and `+` to space, UTF-8 with replace.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `parse_qs` for a raw query string (`a=1&b=2&a=3`).
fn parse_raw_query(q: &str) -> Map<String, Value> {
    let mut map: Map<String, Value> = Map::new();
    for part in q.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.find('=') {
            Some(i) => (&part[..i], &part[i + 1..]),
            None => (part, ""),
        };
        let key = percent_decode(k);
        let val = percent_decode(v);
        match map.get_mut(&key) {
            Some(Value::Array(a)) => a.push(Value::String(val)),
            _ => {
                map.insert(key, Value::Array(vec![Value::String(val)]));
            }
        }
    }
    map
}

/// The query as `name -> [value, ...]`, whichever shape the host sent.
fn query_map(query: &Value) -> Map<String, Value> {
    match query {
        Value::Object(m) => {
            let mut out: Map<String, Value> = Map::new();
            for (k, v) in m {
                let vals: Vec<Value> = match v {
                    Value::String(s) => vec![Value::String(percent_decode(s))],
                    Value::Array(a) => a
                        .iter()
                        .map(|x| match x {
                            Value::String(s) => Value::String(percent_decode(s)),
                            other => Value::String(other.to_string()),
                        })
                        .collect(),
                    Value::Null => Vec::new(),
                    other => vec![Value::String(other.to_string())],
                };
                out.insert(k.clone(), Value::Array(vals));
            }
            out
        }
        Value::String(s) => parse_raw_query(s),
        _ => Map::new(),
    }
}

/// `query.get(name)` — the list of decoded values.
fn qvals(query: &Value, name: &str) -> Vec<String> {
    let m = query_map(query);
    match m.get(name) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// `(query.get(name) or [''])[0]`.
fn qfirst(query: &Value, name: &str) -> Option<String> {
    qvals(query, name).into_iter().next()
}

/// `_int(query.get(name), default)`.
fn qint(query: &Value, name: &str, default: i64) -> i64 {
    match qfirst(query, name) {
        Some(s) => s.trim().parse::<i64>().unwrap_or(default),
        None => default,
    }
}

/// `_list_arg(v)` — comma-split, stripped, `None` when empty.
fn list_arg(vals: &[String]) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for item in vals {
        for p in item.split(',') {
            let p = p.trim();
            if !p.is_empty() {
                out.push(p.to_string());
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// `_list_arg` over a JSON body value (`{"roots": ["a,b"]}`).
fn list_arg_value(v: Option<&Value>) -> Option<Vec<String>> {
    match v {
        Some(Value::Array(a)) => {
            let vals: Vec<String> = a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
            list_arg(&vals)
        }
        Some(Value::String(s)) => list_arg(std::slice::from_ref(s)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// GET
// ---------------------------------------------------------------------------

fn do_get(index: &mut Index, path: &str, query: &Value) -> (u16, Value) {
    match path {
        "/api/state" => (200, index.state()),
        "/api/index" => {
            let roots = list_arg(&qvals(query, "roots"));
            let final_roots = match roots {
                Some(r) => r,
                None => index.config().roots.clone(),
            };
            do_index(index, final_roots)
        }
        "/api/browse" => browse(index, &qfirst(query, "path").unwrap_or_default()),
        "/api/search" => (
            200,
            json!({
                "ok": true,
                "results": index.search(
                    &qfirst(query, "q").unwrap_or_default(),
                    qint(query, "limit", 30),
                ),
            }),
        ),
        "/api/node" => node(index, &qfirst(query, "id").unwrap_or_default()),
        "/api/relation" => {
            let page = index.relation_page(
                &qfirst(query, "node").unwrap_or_default(),
                &qfirst(query, "key").unwrap_or_default(),
                qint(query, "offset", 0),
                qint(query, "limit", 200),
            );
            match page {
                None => err(400, "node/key are required"),
                Some(p) => (200, p),
            }
        }
        "/api/find" => (
            200,
            index.find(
                &qfirst(query, "q").unwrap_or_default(),
                qint(query, "limit", 400),
            ),
        ),
        "/api/refs" => {
            let (hits, trunc) = index.text_hits(
                &qfirst(query, "q").unwrap_or_default(),
                qint(query, "limit", 400),
            );
            (200, json!({"ok": true, "usages": hits, "truncated": trunc}))
        }
        "/api/source" => {
            let to = qint(query, "to", 0);
            let node = qfirst(query, "node");
            let src = index.source(
                qfirst(query, "file").as_deref(),
                qint(query, "line", 1),
                qint(query, "context", 30),
                node.as_deref(),
                if to != 0 { Some(to) } else { None },
                &qfirst(query, "mode").unwrap_or_else(|| "doc".to_string()),
            );
            match src {
                // A node with no declaration site (an unresolved external symbol,
                // say) is not a missing file; say so instead of "file not indexed".
                None if node.is_some() => err(404, "no source: symbol is external or not declared"),
                None => err(404, "file not indexed"),
                Some(s) => (200, s),
            }
        }
        "/api/doc" => {
            let node_id = qfirst(query, "node").unwrap_or_default();
            let src = index.doc_source(&node_id);
            match src {
                None => err(404, "no synthesized document for node"),
                // Owned form for the legacy/CLI path; `handle_payload` serves
                // the same document without this clone.
                Some(s) => (200, (*s).clone()),
            }
        }
        "/api/decls" => (
            200,
            index.decls(
                qfirst(query, "node").as_deref(),
                qfirst(query, "file").as_deref(),
            ),
        ),
        "/api/chain" => {
            let ch = index.chain(
                &qfirst(query, "id").unwrap_or_default(),
                qint(query, "depth", 3),
                &qfirst(query, "dir").unwrap_or_else(|| "forward".to_string()),
                qint(query, "max", 300),
            );
            match ch {
                None => err(404, "no such node"),
                Some(c) => (200, c),
            }
        }
        "/api/kinds" => (200, index.kinds()),
        "/api/definitions" => (
            200,
            index.definitions(
                &qfirst(query, "scope").unwrap_or_else(|| "public".to_string()),
                qint(query, "limit", 400),
            ),
        ),
        "/api/subgraph" => {
            if !index.indexed {
                return (
                    200,
                    json!({"ok": true, "focus": null, "truncated": false,
                           "nodes": [], "edges": []}),
                );
            }
            let focus = qfirst(query, "id");
            (
                200,
                index.subgraph(
                    focus.as_deref(),
                    qint(query, "depth", 1),
                    qint(query, "max", 250),
                ),
            )
        }
        "/api/export.json" => (
            200,
            json!({
                "ok": true,
                "nodes": index.nodes().to_vec(),
                "edges": index.edges().to_vec(),
                "roots": index.roots().to_vec(),
            }),
        ),
        "/api/viewer_state" => (200, json!({"ok": true, "state": index.viewer_state()})),
        // Read-only: which cs viewer instances are live (cross-user registry +
        // bounded /proc fallback), and the selected instance's POV (Monaco text
        // selection, graph box selection, loose IDE context) with freshness.
        "/api/viewer_instances" => {
            let selector = qfirst(query, "instance");
            (200, index.viewer_instances(selector.as_deref()))
        }
        "/api/viewer_pov" => {
            let selector = qfirst(query, "instance");
            (200, index.viewer_pov(selector.as_deref()))
        }
        "/api/settings" => (
            200,
            json!({
                "ok": true,
                "settings": load_settings(None),
                "uiThemes": UI_THEMES,
            }),
        ),
        // Additive: the .sln/.csproj model. `files=1` lists each project's
        // source set; `file=<display path>` answers the per-node lookup.
        "/api/projects" => {
            let with_files = matches!(
                qfirst(query, "files").as_deref(),
                Some("1") | Some("true") | Some("yes")
            );
            let file = qfirst(query, "file").filter(|f| !f.is_empty());
            (200, index.projects_payload(with_files, file.as_deref()))
        }
        p if p.starts_with("/api/") => err(404, format!("unknown endpoint {p}")),
        p => static_route(p),
    }
}

// ---------------------------------------------------------------------------
// POST
// ---------------------------------------------------------------------------

fn do_post(index: &mut Index, path: &str, _query: &Value, body: Option<Value>) -> (u16, Value) {
    if !matches!(
        path,
        "/api/index" | CANCEL_PATH | "/api/viewer_state" | "/api/settings" | "/api/reorg"
    ) {
        return err(404, format!("unknown endpoint {path}"));
    }
    // No body at all means `{}` in Python; any other non-object is a 400.
    let body = match body {
        None => json!({}),
        Some(Value::Object(o)) => Value::Object(o),
        Some(_) => return err(400, "body must be a JSON object"),
    };
    match path {
        CANCEL_PATH => {
            // Only a flag store: the build checks it on its next loop turn and
            // unwinds without touching the installed graph.
            index.request_cancel();
            (200, json!({"ok": true, "cancelled": true}))
        }
        "/api/reorg" => reorg(index, &body),
        "/api/settings" => match save_settings(&body, None) {
            None => err(500, "could not write the settings file"),
            Some(saved) => (
                200,
                json!({"ok": true, "settings": saved, "uiThemes": UI_THEMES}),
            ),
        },
        "/api/viewer_state" => {
            let ok = index.save_viewer_state(&body);
            (200, json!({"ok": ok}))
        }
        "/api/index" => {
            let roots = list_arg_value(body.get("roots")).or_else(|| {
                if index.roots().is_empty() {
                    None
                } else {
                    Some(index.roots().to_vec())
                }
            });
            let final_roots = match roots {
                Some(r) => r,
                None => index.config().roots.clone(),
            };
            do_index(index, final_roots)
        }
        _ => err(404, format!("unknown endpoint {path}")),
    }
}

/// `_do_index(roots)`.
fn do_index(index: &mut Index, roots: Vec<String>) -> (u16, Value) {
    let roots = if roots.is_empty() {
        index.config().roots.clone()
    } else {
        roots
    };
    let cfg = index.config().clone();
    let state = index.build(&roots, &cfg.excludes, &cfg);
    (200, state)
}

/// `_reorg(body)`.
///
/// Merge / split the indexed corpus and hand it back. `verify` returns the
/// identity proof, `zip` returns the archive base64-encoded (the Tauri IPC
/// channel is JSON-only, so a binary body cannot cross it), and `write` writes
/// the tree under the caller's absolute destination.
///
/// `compile` additionally runs the differential Roslyn check
/// ([`crate::compile_check`]): the original corpus and the emitted tree are
/// compiled the same way and their diagnostics compared. It degrades to
/// `skipped` when no Unity/`.NET` toolchain is present, never to a false pass.
fn reorg(index: &mut Index, body: &Value) -> (u16, Value) {
    let action = body
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("plan");
    let dest = body.get("dest").and_then(|v| v.as_str());
    // Python: `body.get('overwrite', True)` — absent means true, otherwise the
    // value's own truthiness decides.
    let overwrite = match body.get("overwrite") {
        None => true,
        Some(Value::Bool(b)) => *b,
        Some(Value::Null) => false,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    };
    let res = index.reorg(body.get("spec"), action, dest, overwrite);
    let ok = res.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if action == "zip" {
        // v1 sends the archive with `Content-Disposition`; when the tree did not
        // verify it refuses with 409 rather than shipping a broken package.
        if !ok {
            return err(409, "the emitted tree did not verify; refusing to package it");
        }
        return (200, res);
    }
    (if ok { 200 } else { 400 }, res)
}

// ---------------------------------------------------------------------------
// endpoint impls
// ---------------------------------------------------------------------------

/// `_node(nid)`.
fn node(index: &Index, nid: &str) -> (u16, Value) {
    if nid.is_empty() {
        return err(400, "id is required");
    }
    match index.node_detail(nid) {
        None => err(404, format!("no such node: {nid}")),
        Some(d) => (200, d),
    }
}

/// `_resolve_path(p)` — absolute paths (and `~`) are honoured.
fn resolve_path(index: &Index, p: &str) -> PathBuf {
    let p = p.replace('\\', "/");
    let p = p.trim();
    if p.is_empty() || p == "." || p == "./" {
        return index.repo.clone();
    }
    let p = if let Some(rest) = p.strip_prefix('~') {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{home}{rest}")
    } else {
        p.to_string()
    };
    if Path::new(&p).is_absolute() {
        normpath(Path::new(&p))
    } else {
        normpath(&index.repo.join(&p))
    }
}

/// `_dir_has_cs(path)` — bounded "does this subtree hold C#?" probe.
///
/// Symlinked folders are followed, so a probe through a link answers for the
/// tree it points at; each real directory is visited at most once, so a link
/// back up the tree cannot turn the probe into a loop.
fn dir_has_cs(path: &Path) -> Option<bool> {
    const BUDGET_S: f64 = 0.10;
    const MAX_FILES: usize = 600;
    const MAX_DEPTH: usize = 3;

    fn walk(
        dir: &Path,
        depth: usize,
        t0: &Instant,
        seen: &mut usize,
        visited: &mut HashSet<PathBuf>,
    ) -> Option<bool> {
        if t0.elapsed().as_secs_f64() > BUDGET_S {
            return None;
        }
        let key = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if !visited.insert(key) {
            return Some(false);
        }
        let rd = fs::read_dir(dir).ok()?;
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for ent in rd.flatten() {
            // `fs::metadata`, not `ent.file_type()`: it follows a link, so a
            // linked folder is descended into like a real one.
            let Ok(meta) = fs::metadata(ent.path()) else { continue };
            let name = ent.file_name().to_string_lossy().to_string();
            if meta.is_dir() {
                subdirs.push(ent.path());
            } else if name.ends_with(".cs") || name.ends_with(".cs.nzk") {
                return Some(true);
            } else {
                *seen += 1;
                if *seen > MAX_FILES {
                    return None;
                }
            }
        }
        if depth >= MAX_DEPTH {
            return Some(false);
        }
        for d in subdirs {
            match walk(&d, depth + 1, t0, seen, visited) {
                Some(true) => return Some(true),
                None => return None,
                Some(false) => {}
            }
        }
        Some(false)
    }
    let t0 = Instant::now();
    let mut seen = 0usize;
    walk(path, 0, &t0, &mut seen, &mut HashSet::new())
}

/// `_browse(rel)`.
///
/// Symlinks are resolved: a linked folder is listed as a folder (so it can be
/// opened and picked for indexing) and a linked `.cs`/`.cs.nzk` as a file. The
/// entry carries `symlink` so the picker can show that the path is a link.
fn browse(index: &Index, rel: &str) -> (u16, Value) {
    let target = resolve_path(index, rel);
    if !target.is_dir() {
        return err(404, format!("not a directory: {rel}"));
    }
    let mut entries: Vec<(String, PathBuf)> = Vec::new();
    match fs::read_dir(&target) {
        Ok(rd) => {
            for ent in rd.flatten() {
                entries.push((ent.file_name().to_string_lossy().to_string(), ent.path()));
            }
        }
        Err(exc) => return err(500, exc.to_string()),
    }
    entries.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then(a.0.cmp(&b.0)));
    let deadline = Instant::now();
    let mut dirs: Vec<Value> = Vec::new();
    let mut files: Vec<Value> = Vec::new();
    for (name, p) in &entries {
        if name.starts_with('.') {
            continue;
        }
        // Follow the link (`fs::metadata`), so a linked folder/file is usable;
        // a broken link stat()s to an error and is skipped.
        let Ok(meta) = fs::metadata(p) else { continue };
        let is_link = fs::symlink_metadata(p)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if meta.is_dir() {
            let mut count = 0i64;
            if let Ok(rd) = fs::read_dir(p) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().to_string();
                    if n.ends_with(".cs") || n.ends_with(".cs.nzk") {
                        count += 1;
                    }
                }
            }
            let has = if deadline.elapsed().as_secs_f64() > 2.5 {
                true // unknown -> keep it selectable
            } else {
                match dir_has_cs(p) {
                    None => true,
                    Some(v) => v,
                }
            };
            dirs.push(json!({
                "name": name,
                "path": p.to_string_lossy().replace('\\', "/"),
                "hasCs": has,
                "csCount": count,
                "symlink": is_link,
            }));
        } else if meta.is_file() && (name.ends_with(".cs") || name.ends_with(".cs.nzk")) {
            files.push(json!({
                "name": name,
                "path": p.to_string_lossy().replace('\\', "/"),
                "symlink": is_link,
            }));
        }
    }
    let target_str = target.to_string_lossy().replace('\\', "/");
    let parent = target_str
        .trim_end_matches('/')
        .rsplit_once('/')
        .map(|(head, _)| if head.is_empty() { "/" } else { head })
        .unwrap_or("/")
        .to_string();
    files.truncate(200);
    (
        200,
        json!({
            "ok": true,
            "path": target_str,
            "parent": parent,
            "dirs": dirs,
            "files": files,
        }),
    )
}

// ---------------------------------------------------------------------------
// static route (kept for parity; the v2 host serves the frontend itself)
// ---------------------------------------------------------------------------

/// `STATIC` — the v2 frontend root (`frontend/` next to the crate).
fn static_dir() -> PathBuf {
    if let Ok(p) = std::env::var("CSVIEWER_STATIC") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../frontend")
}

/// The value the host would send for a static file, as text.
fn static_body(fp: &Path) -> Value {
    match fs::read(fp) {
        Ok(bytes) => Value::String(String::from_utf8_lossy(&bytes).into_owned()),
        Err(exc) => Value::String(exc.to_string()),
    }
}

/// `_static(path)`.
fn static_route(path: &str) -> (u16, Value) {
    let path = if path.is_empty() || path == "/" {
        "/index.html"
    } else {
        path
    };
    let rel = normpath(Path::new(path))
        .to_string_lossy()
        .replace('\\', "/");
    let rel = rel.trim_start_matches('/').to_string();
    if rel.starts_with("..") {
        return err(403, "forbidden");
    }
    let fp = static_dir().join(&rel);
    if !fp.is_file() {
        return err(404, format!("not found: {path}"));
    }
    (200, static_body(&fp))
}

// ---------------------------------------------------------------------------
// reorg helpers (`server.py`'s module-level helpers, ported verbatim)
// ---------------------------------------------------------------------------

/// `_zip_name(spec)` — download name for a reorganisation archive.
#[allow(dead_code)]
pub fn zip_name(spec: &Value) -> String {
    let mode = spec
        .get("group")
        .and_then(|v| v.as_str())
        .unwrap_or("single");
    let base = spec
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if mode == "single" {
                "merged".to_string()
            } else {
                "reorg".to_string()
            }
        });
    let re = regex::Regex::new(r"[^A-Za-z0-9._-]+").unwrap();
    let safe = re.replace_all(&base, "_");
    let safe = safe.trim_matches('_');
    let safe = if safe.is_empty() { "reorg" } else { safe };
    format!("{safe}.zip")
}

/// `_previews(files, lines=40)` — first lines of each emitted file.
#[allow(dead_code)]
pub fn previews(files: &Map<String, Value>, lines: usize) -> Vec<Value> {
    let mut paths: Vec<&String> = files.keys().collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let text = files[path].as_str().unwrap_or("");
            let body: Vec<&str> = text.split('\n').collect();
            json!({
                "path": path,
                "lines": body.len(),
                "bytes": text.chars().count(),
                "head": body[..body.len().min(lines)].join("\n"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_matches_unquote_plus() {
        assert_eq!(
            percent_decode("NZK.Core.MA.NormaliseName"),
            "NZK.Core.MA.NormaliseName"
        );
        assert_eq!(percent_decode("a%20b+c%2Fd"), "a b c/d");
        assert_eq!(percent_decode("%zz%2"), "%zz%2");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
    }

    /// The Stop route flips the shared flag and answers `cancelled`; a later
    /// build starts clean, so Stop only ever means "the build in flight".
    #[test]
    fn cancel_route_flips_the_stop_flag() {
        let dir = std::env::temp_dir().join(format!("csgraph-api-cancel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.cs"), "namespace N { public class A {} }\n").unwrap();

        let mut ix = Index::new(dir.clone());
        assert!(!ix.cancel_requested(), "a fresh index is not cancelled");

        let (code, body) = handle(&mut ix, "POST", CANCEL_PATH, &json!({}), Some(json!({})));
        assert_eq!(code, 200);
        assert_eq!(body["ok"], true);
        assert_eq!(body["cancelled"], true);
        assert!(ix.cancel_requested(), "the Stop route must set the shared flag");

        // A subsequent build is a fresh start and installs its graph.
        let cfg = crate::config::Config::default();
        let state = ix.build(&[".".to_string()], &[], &cfg);
        assert_ne!(state["cancelled"], true, "a new build must not inherit the flag");
        assert_eq!(state["indexed"], true);
        assert!(!ix.cancel_requested());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_arg_splits_and_strips() {
        let v: Vec<String> = vec!["a, b ,,c".to_string()];
        assert_eq!(
            list_arg(&v),
            Some(vec!["a".to_string(), "b".to_string(), "c".to_string()])
        );
        assert_eq!(list_arg(&[]), None);
        assert_eq!(list_arg(&["  ".to_string()]), None);
    }

    #[test]
    fn routes_without_an_index() {
        let dir = std::env::temp_dir().join(format!("csgraph-api-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let mut ix = Index::new(dir.clone());

        let (code, body) = handle(&mut ix, "GET", "/api/state", &json!({}), None);
        assert_eq!(code, 200);
        assert_eq!(body["ok"], true);
        assert_eq!(body["indexed"], false);

        let (code, body) = handle(&mut ix, "GET", "/api/node", &json!({}), None);
        assert_eq!(code, 400);
        assert_eq!(body["error"], "id is required");

        let (code, body) = handle(
            &mut ix,
            "GET",
            "/api/relation",
            &json!({"node": ["x"], "key": ["calls:out"]}),
            None,
        );
        assert_eq!(code, 400);
        assert_eq!(body["error"], "node/key are required");

        let (code, body) = handle(&mut ix, "GET", "/api/unknown", &json!({}), None);
        assert_eq!(code, 404);
        assert!(body["error"].as_str().unwrap().starts_with("unknown endpoint"));

        let (code, _) = handle(&mut ix, "GET", "/api/subgraph", &json!({}), None);
        assert_eq!(code, 200);

        let (code, _body) = handle(&mut ix, "POST", "/api/nope", &json!({}), None);
        assert_eq!(code, 404);

        let _ = fs::remove_dir_all(&dir);
    }

    /// `/api/projects` is additive: a new route over a cold index, and after a
    /// build it reports the project and answers the per-file lookup.
    #[test]
    fn projects_route_reports_the_model_and_file_lookup() {
        let dir = std::env::temp_dir().join(format!("csgraph-api-proj-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("App")).unwrap();
        fs::write(
            dir.join("App/App.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\">\n<PropertyGroup>\n\
             <AssemblyName>App</AssemblyName>\n<TargetFramework>net472</TargetFramework>\n\
             <DefineConstants>TRACE;APP_ONLY</DefineConstants>\n</PropertyGroup>\n</Project>\n",
        )
        .unwrap();
        fs::write(dir.join("App/Program.cs"), "namespace N { public class Program {} }\n").unwrap();

        let mut ix = Index::new(dir.clone());
        // cold: no build yet, still a valid (empty-ish) additive answer
        let (code, body) = handle(&mut ix, "GET", "/api/projects", &json!({}), None);
        assert_eq!(code, 200);
        assert_eq!(body["ok"], true);
        assert!(body["projects"].is_array());

        let cfg = crate::config::Config::default();
        ix.build(&[".".to_string()], &[], &cfg);
        assert!(ix.indexed);

        let (code, body) = handle(&mut ix, "GET", "/api/projects", &json!({}), None);
        assert_eq!(code, 200, "projects failed: {body}");
        assert_eq!(body["ok"], true);
        let p = body["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "App")
            .expect("App project missing");
        assert_eq!(p["path"], "App/App.csproj");
        assert_eq!(p["targets"][0], "net472");
        assert_eq!(p["fileCount"], 1);
        let src = body["defineSources"].as_array().unwrap();
        assert!(src.iter().any(|s| s["symbol"] == "APP_ONLY"), "{body}");

        let (code, body) = handle(
            &mut ix,
            "GET",
            "/api/projects",
            &json!({"files": ["1"]}),
            None,
        );
        assert_eq!(code, 200);
        let p = body["projects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "App")
            .unwrap();
        assert_eq!(p["files"][0], "App/Program.cs");

        let (code, body) = handle(
            &mut ix,
            "GET",
            "/api/projects",
            &json!({"file": ["App/Program.cs"]}),
            None,
        );
        assert_eq!(code, 200);
        assert_eq!(body["ok"], true);
        assert_eq!(body["names"][0], "App");
        assert_eq!(body["projects"][0], "App/App.csproj");

        let (code, body) = handle(
            &mut ix,
            "GET",
            "/api/projects",
            &json!({"file": ["nowhere/Nope.cs"]}),
            None,
        );
        assert_eq!(code, 200);
        assert_eq!(body["projects"].as_array().unwrap().len(), 0);

        let _ = fs::remove_dir_all(&dir);
    }

    /// A project that links a shared folder in must be browsable: the picker
    /// has to list the linked folder as a folder (so it can be walked into and
    /// chosen for indexing) instead of dropping it as "neither file nor dir".
    #[cfg(unix)]
    #[test]
    fn browse_follows_symlinked_dirs_and_files() {
        let dir = std::env::temp_dir().join(format!("csgraph-api-link-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("shared")).unwrap();
        fs::write(dir.join("shared/Deep.cs"), "namespace N { class Deep {} }\n").unwrap();
        std::os::unix::fs::symlink(dir.join("shared"), dir.join("linkdir")).unwrap();
        std::os::unix::fs::symlink(dir.join("shared/Deep.cs"), dir.join("alias.cs")).unwrap();

        let mut ix = Index::new(dir.clone());
        let (code, body) = handle(&mut ix, "GET", "/api/browse", &json!({"path": [""]}), None);
        assert_eq!(code, 200, "browse failed: {body}");

        let dirs = body["dirs"].as_array().unwrap();
        let link = dirs
            .iter()
            .find(|d| d["name"] == "linkdir")
            .expect("symlinked folder missing from the picker");
        assert_eq!(link["hasCs"], true);
        assert_eq!(link["symlink"], true);
        assert_eq!(link["csCount"], 1);

        let files = body["files"].as_array().unwrap();
        let alias = files
            .iter()
            .find(|f| f["name"] == "alias.cs")
            .expect("symlinked .cs file missing from the picker");
        assert_eq!(alias["symlink"], true);

        // A linked tree is selectable, exactly like a real one.
        let mut ix = Index::new(dir.clone());
        let (code, body) = handle(
            &mut ix,
            "GET",
            "/api/browse",
            &json!({"path": [dir.join("linkdir").to_string_lossy().to_string()]}),
            None,
        );
        assert_eq!(code, 200, "browse of linked folder failed: {body}");
        assert_eq!(body["ok"], true);

        let _ = fs::remove_dir_all(&dir);
    }

    /// The reorg route is the one place the API reaches into `crate::reorg`;
    /// this exercises plan -> verify -> zip -> write for real, on a tiny corpus.
    #[test]
    fn reorg_route_plans_verifies_zips_and_writes() {
        let dir = std::env::temp_dir().join(format!("csgraph-api-reorg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("a.cs"),
            "namespace N {\n public partial class C {\n  public int A() { return 1; }\n }\n}\n",
        )
        .unwrap();

        let mut ix = Index::new(dir.clone());
        let cfg = crate::config::Config::default();
        ix.build(&[".".to_string()], &[], &cfg);
        assert!(ix.indexed);

        // plan
        let (code, body) = handle(
            &mut ix,
            "POST",
            "/api/reorg",
            &json!({}),
            Some(json!({"action": "plan", "spec": {"group": "single"}})),
        );
        assert_eq!(code, 200, "plan failed: {body}");
        assert_eq!(body["ok"], true);
        assert!(body["plan"].is_object());

        // verify — the identity proof
        let (code, body) = handle(
            &mut ix,
            "POST",
            "/api/reorg",
            &json!({}),
            Some(json!({"action": "verify", "spec": {"group": "single"}})),
        );
        assert_eq!(code, 200, "verify failed: {body}");
        assert_eq!(body["ok"], true, "verify not ok: {body}");
        assert_eq!(body["report"]["ok"], true);
        assert!(body["previews"].is_array());

        // zip — base64 over the JSON-only IPC channel
        let (code, body) = handle(
            &mut ix,
            "POST",
            "/api/reorg",
            &json!({}),
            Some(json!({"action": "zip", "spec": {"group": "single"}})),
        );
        assert_eq!(code, 200, "zip failed: {body}");
        let data = body["data"].as_str().expect("zip data missing");
        assert!(!data.is_empty());
        assert!(data.starts_with("UEs")); // base64 of "PK\x03": a real ZIP container
        assert!(body["name"].as_str().unwrap().ends_with(".zip"));

        // write — only ever under the destination the caller named
        let out = dir.join("out");
        let (code, body) = handle(
            &mut ix,
            "POST",
            "/api/reorg",
            &json!({}),
            Some(json!({"action": "write", "dest": out.to_string_lossy(),
                        "spec": {"group": "single"}})),
        );
        assert_eq!(code, 200, "write failed: {body}");
        assert_eq!(body["ok"], true, "write not ok: {body}");
        assert!(out.join("N.single.cs").exists() || out.is_dir());

        // a relative destination is refused, exactly like v1
        let (code, body) = handle(
            &mut ix,
            "POST",
            "/api/reorg",
            &json!({}),
            Some(json!({"action": "write", "dest": "relative/dir",
                        "spec": {"group": "single"}})),
        );
        assert_eq!(code, 400);
        assert_eq!(body["ok"], false);

        let _ = fs::remove_dir_all(&dir);
    }
}
