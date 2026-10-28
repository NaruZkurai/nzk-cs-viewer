//! Engine access for the fork.
//!
//! This module is the *only* place that talks to `csgraph_core`. It reuses the
//! engine's public API (`Index`, `Config`) exactly as the Tauri host does, but
//! read-only: nothing under `rust-only-csviewer/core/` is edited by this fork.

use std::path::{Path, PathBuf};
use std::time::Instant;

use csgraph_core::config::{self, Config};
use csgraph_core::index::Index;

/// FNV-1a hasher for the id -> index map. `std`'s default SipHash is a poor fit
/// for short keys looked up ~20k times; FNV is a few lines and needs no crate.
#[derive(Default)]
pub struct FnvHasher(u64);

impl std::hash::Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut h = if self.0 == 0 { 0xcbf2_9ce4_8422_2325 } else { self.0 };
        for &b in bytes {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 = h;
    }
}

type FnvBuild = std::hash::BuildHasherDefault<FnvHasher>;

fn profile_enabled() -> bool {
    std::env::var_os("CSVIEWER_PROFILE_MODEL").is_some()
}

/// One graph node, reduced to the fields the view needs.
///
/// Kept as owned `String`s (not `serde_json::Value`) so the renderer never
/// touches JSON on the hot path.
#[derive(Clone, Debug, Default)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub full: String,
    pub kind: String,
    pub ns: String,
    /// The engine's `parent` link: for a **namespace** it is the direct parent
    /// namespace's id (`#ns:a.b` -> `#ns:a`), and for a member/type it is the
    /// declaring type (or the namespace when there is none).
    ///
    /// Carried because the namespace resolve-chain walks this link upward, and
    /// the engine already puts it on every node; the fork used to drop it.
    pub parent: String,
    pub owner: String,
    pub file: String,
    pub line: i64,
    pub end_line: i64,
    pub degree: i64,
    /// The declaration's **declared type** — the engine's `ret` field.
    ///
    /// A method's return type, a field's/property's type, an event's delegate
    /// type, a class's base? No: the parser leaves it empty for types and
    /// namespaces, because a `class` has no declared *type* of its own. That is
    /// exactly the distinction the sidebar's declared-type browser relies on:
    /// a declaration with a non-empty `ret` is a member whose type can be
    /// filtered by (`string`, `bool`, `int`, `System.String`, …), and one with
    /// an empty `ret` is not.
    ///
    /// Carried from `convert` because the engine already ships it on every node
    /// object (`/api/definitions` and `/api/node` return the same field); the
    /// fork used to drop it, so the only way to ask "which declarations are
    /// declared `string`?" was a re-index or an engine call.
    pub ret: String,
    pub external: bool,
}

/// One graph edge, as indices into [`Graph::nodes`].
#[derive(Clone, Debug)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub kind: String,
    pub count: i64,
}

/// One parsed `.sln`/`.csproj`, in the shape nzk_ui's `dom::ProjectRow`
/// declares (see `nzk_ui/src/dom.rs`).
///
/// This is the engine's real project model, not a fixture: `id`/`name`/`path`/
/// `targets`/`defines` come from `csgraph_core::config::Project`, and
/// `file_count` counts that project's *source set*. The count is carried
/// separately because the engine's compact JSON also spells it `fileCount`,
/// while the renderer's field is `file_count` — a passthrough of the engine
/// object would silently leave the renderer's count at its default.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub file_count: usize,
    pub path: String,
    pub targets: Vec<String>,
    pub defines: Vec<String>,
}

impl ProjectRow {
    /// The engine object serialised into nzk_ui's `ProjectRow` key names.
    ///
    /// Hand-written rather than derived: the app crate does not depend on
    /// `serde` directly (only `serde_json`), and an explicit mapping is the
    /// point of this type — the engine's `fileCount` must become the
    /// renderer's `file_count`, and a derived pass-through on the engine object
    /// would key it `fileCount` and leave the renderer's field at 0.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "file_count": self.file_count,
            "path": self.path,
            "targets": self.targets,
            "defines": self.defines,
        })
    }
}

/// One real parser warning, in the shape nzk_ui's `dom::WarningRow`
/// declares.
///
/// The engine's installed warning list is a `Vec<String>` with positions
/// already stripped (index.rs takes up to 50 of them from
/// `GraphBuilder::warnings`, which the parser fills with bare strings). So
/// `file`/`line` are filled from the warning line itself when it embeds them
/// (`path(line,col): kind CSxxxx: msg`, the shape `compile_check` parses) and
/// are otherwise left empty — never invented. `raw` always carries the
/// verbatim engine string, which is what makes a row with no position usable
/// rather than misleading.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WarningRow {
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub text: String,
    pub raw: String,
}

impl WarningRow {
    /// Serialised into nzk_ui's `WarningRow` key names.
    ///
    /// Hand-written for the same reason as [`ProjectRow::to_json`]: the app
    /// crate has no direct `serde` dependency. `file`/`line`/`col` are emitted
    /// exactly as parsed — an empty file with line 0 is what tells the renderer
    /// (dom.rs:983) to show text with no location link.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "file": self.file,
            "line": self.line,
            "col": self.col,
            "text": self.text,
            "raw": self.raw,
        })
    }
}

/// One row of the `#pubList` box — a public definition, as the **engine**
/// returned it (`Index::definitions("public", N)`, `core/src/index.rs:3403`).
///
/// # Why these four fields, and no more
///
/// `renderListRows` (`filters.js:448-475`) builds each row from exactly: the
/// button's `title` = `n.full || n.id`, a `.dot` coloured by `n.kind`, a
/// `.l-label` = `n.label || n.name || n.full || n.id`, and a `.l-kind` =
/// `n.kind`. So these are the fields the renderer actually *reads*; the
/// fallback chains live in [`PubRow::from_lite`] rather than in the renderer, so
/// a row with a missing label can never render empty.
///
/// `id` is carried because clicking a row calls `openListNode(n)`
/// (`filters.js:477-487`), which needs the node id to select it in the graph —
/// the row is not just a label.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PubRow {
    /// `n.id` — what `openListNode` selects, and the last `title` fallback.
    pub id: String,
    /// `n.full` — the button `title`.
    pub full: String,
    /// `n.label || n.name || n.full || n.id` — the visible `.l-label` text.
    pub label: String,
    /// `n.kind` — both the `.l-kind` chip text and the `.dot` colour key.
    pub kind: String,
}

impl PubRow {
    /// Build a row from one `Index::definitions` entry (`Index::lite`,
    /// `core/src/index.rs:872`).
    ///
    /// The fallback chain is applied **here**, at the boundary, exactly as
    /// `renderListRows` applies `n.label || n.name || n.full || n.id`. Folding
    /// it in at construction means the renderer receives ready-to-paint text and
    /// an empty `.l-label` is impossible for any node that has an id — which is
    /// the one thing every indexed node has.
    ///
    /// Returns `None` only when the entry has no usable identity at all (no id
    /// and no full name). A row that cannot name anything is dropped rather than
    /// rendered as a blank line: the reference would push a button with an empty
    /// `title` and empty text, which is a control that lies about being
    /// clickable. `None` is the honest answer.
    pub fn from_lite(v: &serde_json::Value) -> Option<PubRow> {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let id = s("id");
        let full = s("full");
        if id.is_empty() && full.is_empty() {
            return None;
        }
        let label = {
            let l = s("label");
            if !l.is_empty() {
                l
            } else {
                let n = s("name");
                if !n.is_empty() {
                    n
                } else if !full.is_empty() {
                    full.clone()
                } else {
                    id.clone()
                }
            }
        };
        Some(PubRow { id, full, label, kind: s("kind") })
    }

    /// Serialised into nzk_ui's `PubRow` key names.
    ///
    /// Hand-written for the same reason as [`WarningRow::to_json`]: the app
    /// crate has no direct `serde` dependency.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "full": self.full,
            "label": self.label,
            "kind": self.kind,
        })
    }
}

/// Split one engine warning string into the renderer's `WarningRow` fields.
///
/// Mirrors the incumbent's `problems.js parseWarning`, which recognises exactly
/// two location-bearing shapes:
///
/// * `src/Foo.cs(12,5): warning CS0168: …` — MSBuild-style, tried first;
/// * `src/Foo.cs:12:5: …` and `src/Foo.cs:12: …` — repo-relative + line.
///
/// Anything else (the parser's own prose warnings, e.g. "scope guard tripped")
/// keeps `file`/`line`/`col` empty and survives through `raw`, exactly as
/// `problems.js` keeps it verbatim with no location. nzk_ui's renderer
/// already branches on `!file.is_empty() && line > 0` (dom.rs:983), so an
/// empty file with line 0 renders as text with no dead link — which is why
/// leaving them unfilled is honest rather than lossy.
pub fn warning_row_from(engine_line: &str) -> WarningRow {
    let s = engine_line.trim();
    let mut row = WarningRow { file: String::new(), line: 0, col: 0, text: s.to_string(), raw: s.to_string() };

    // Shape 1: `path(line,col): rest`
    if let Some(open) = s.find('(') {
        if let Some(close) = s[open..].find(')').map(|i| i + open) {
            let pos = &s[open + 1..close];
            let mut it = pos.split(',');
            if let (Some(l), Some(c)) = (it.next(), it.next()) {
                if let (Ok(line), Ok(col)) =
                    (l.trim().parse::<usize>(), c.trim().parse::<usize>())
                {
                    row.file = s[..open].trim().to_string();
                    row.line = line;
                    row.col = col;
                    row.text = s[close + 1..].trim_start_matches(':').trim().to_string();
                    if row.text.is_empty() {
                        row.text = s.to_string();
                    }
                    return row;
                }
            }
        }
    }

    // Shape 2: `path:line` or `path:line:col`, then an optional `rest`.
    // Split on the first colon; the caller's file part must look like a path so
    // that a prose warning containing a colon ("note: thing") is not mistaken
    // for a location.
    if let Some((file, rest)) = s.split_once(':') {
        let file = file.trim();
        let looks_like_path = !file.is_empty()
            && !file.contains(' ')
            && (file.contains('.') || file.contains('/') || file.contains('\\'));
        if looks_like_path {
            let mut it = rest.splitn(2, ':');
            if let Some(line) = it.next().and_then(|v| v.trim().parse::<usize>().ok()) {
                let tail = it.next().unwrap_or("").trim();
                // A third numeric field is the column; otherwise it is prose.
                let (col, text) = match tail.split_once(':') {
                    Some((c, t)) if c.trim().parse::<usize>().is_ok() => {
                        (c.trim().parse::<usize>().unwrap_or(0), t.trim().to_string())
                    }
                    _ => match tail.parse::<usize>() {
                        Ok(c) if !tail.is_empty() => (c, String::new()),
                        _ => (0, tail.to_string()),
                    },
                };
                row.file = file.to_string();
                row.line = line;
                row.col = col;
                row.text = if text.is_empty() { s.to_string() } else { text };
                return row;
            }
        }
    }

    row
}

/// Parser warnings that carry a real position, from the Roslyn parse gate.
///
/// This is the only engine source that reports genuine `file`/`line`/`col` for
/// every warning (`compile_check::parse_full`), but it is a *subprocess* that
/// needs a Roslyn toolchain, so it is strictly opt-in: it runs only when
/// `CSVIEWER_ROSLYN_WARNINGS` is set, and it degrades to an empty list when no
/// The file list `parse_check` should gate: explicit `args.files` (array or
/// comma string) resolved against the repo, else discovery over the same roots
/// and excludes the graph used. This is the in-process twin of the reference
/// host's `analyze_files` (`rust-only-csviewer/app/src/main.rs:418-446`), so the
/// Edit dialog's Validate button gates the same set the CLI/MCP route does.
pub fn analyze_files(repo: &Path, roots: &[String], args: &serde_json::Value) -> Vec<String> {
    let explicit: Vec<String> = match args.get("files") {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Some(serde_json::Value::String(s)) => s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    if explicit.is_empty() {
        let roots: Vec<String> = if roots.is_empty() {
            vec!["_sealed_src".to_string()]
        } else {
            roots.to_vec()
        };
        let cfg = load_config(repo);
        csgraph_core::parser::discover_files(repo, &roots, &cfg.excludes)
    } else {
        explicit
            .iter()
            .map(|f| {
                if Path::new(f).is_absolute() {
                    f.clone()
                } else {
                    repo.join(f).to_string_lossy().into_owned()
                }
            })
            .collect()
    }
}

/// toolchain exists rather than blocking or fabricating. The default warning
/// path stays the engine's already-installed list
/// ([`warning_rows_from_state`]), which costs nothing per frame.
pub fn rosllyn_warning_rows(files: &[String], timeout_secs: u64) -> Vec<WarningRow> {
    if std::env::var_os("CSVIEWER_ROSLYN_WARNINGS").is_none() {
        return Vec::new();
    }
    let report = csgraph_core::compile_check::parse_check(files, timeout_secs);
    // `skipped` is a concrete non-null reason when no toolchain is present;
    // never treat that as "no warnings". An absent key is treated the same way
    // as a non-null one: the report is not a clean run, so it contributes
    // nothing rather than being read as an empty (clean) diagnostic list.
    let clean_run = report.get("skipped").map(|s| s.is_null()).unwrap_or(false);
    if !clean_run {
        return Vec::new();
    }
    report
        .get("diagnostics")
        .and_then(|v| v.as_array())
        .map(|ds| {
            ds.iter()
                .map(|d| WarningRow {
                    file: str_at(d, "file").to_string(),
                    line: int_at(d, "line").max(0) as usize,
                    col: int_at(d, "col").max(0) as usize,
                    text: {
                        let msg = str_at(d, "message");
                        let code = str_at(d, "code");
                        if code.is_empty() { msg.to_string() } else { format!("{code}: {msg}") }
                    },
                    raw: {
                        let msg = str_at(d, "message");
                        let code = str_at(d, "code");
                        let kind = str_at(d, "kind");
                        format!(
                            "{}({},{}): {kind} {code}: {msg}",
                            str_at(d, "file"),
                            int_at(d, "line"),
                            int_at(d, "col")
                        )
                    },
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Real parser warnings, with their positions where the engine recorded them.
/// The source is the engine's own installed warning list (`state.warnings`),
/// which is what `/api/state` reports. An absent or non-array value is an
/// empty list — a corpus with no warnings genuinely has none, and that empty
/// list is what keeps the renderer's "No parser warnings" row honest.
pub fn warning_rows_from_state(state: &serde_json::Value) -> Vec<WarningRow> {
    state
        .get("warnings")
        .and_then(|v| v.as_array())
        .map(|ws| {
            ws.iter()
                .filter_map(|w| w.as_str())
                .map(warning_row_from)
                .collect()
        })
        .unwrap_or_default()
}

/// Read the engine's project list out of an `/api/projects`-shaped payload.
///
/// Accepts the payload the engine actually produces (`{"projects": [...]}`)
/// and the bare array form, so a caller that already unwrapped it still works.
pub fn project_rows_from_payload(payload: &serde_json::Value) -> Vec<ProjectRow> {
    let arr = payload
        .get("projects")
        .and_then(|v| v.as_array())
        .or_else(|| payload.as_array());
    let Some(arr) = arr else {
        return Vec::new();
    };
    arr.iter()
        .map(|p| {
            let strings = |k: &str| -> Vec<String> {
                p.get(k)
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|s| s.as_str()).map(str::to_string).collect())
                    .unwrap_or_default()
            };
            ProjectRow {
                id: str_at(p, "id").to_string(),
                name: str_at(p, "name").to_string(),
                // The engine spells it `fileCount`; the renderer's field is
                // `file_count`. Both spellings are read so neither side's
                // rename can silently zero the count.
                file_count: p
                    .get("fileCount")
                    .or_else(|| p.get("file_count"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize,
                path: str_at(p, "path").to_string(),
                targets: strings("targets"),
                defines: strings("defines"),
            }
        })
        .collect()
}

/// The loaded relation graph plus the engine's own summary payload.
pub struct Graph {
    pub repo: PathBuf,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub state: serde_json::Value,
    /// Node kinds that were dropped from the view (external stubs, literals),
    /// with their counts. Reported in the side panel, never silently hidden.
    pub skipped: Vec<(String, usize)>,
    /// The engine's real `.sln`/`.csproj` model for this corpus.
    ///
    /// `state` alone cannot carry this: `/api/state` has no project list, only
    /// `/api/projects` does. Reading `state["projects"]` therefore always
    /// yielded an empty list, which is why the Filters section could only ever
    /// show its "no .sln/.csproj found" chip. The list is captured here from
    /// the live `Index` at load time instead.
    pub projects: Vec<ProjectRow>,
}

/// Convenience: the node for an id, if the view kept it.
#[allow(dead_code)]
pub fn find<'a>(graph: &'a Graph, id: &str) -> Option<&'a Node> {
    graph.nodes.iter().find(|n| n.id == id)
}

/// Borrowing field readers. `str_of`/`int_of` used to allocate a `String` for
/// every field just to copy it again; these hand the engine's own `&str` out so
/// the only allocations left are the ones [`Node`]/[`Edge`] actually own.
fn str_at<'a>(v: &'a serde_json::Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn int_at(v: &serde_json::Value, k: &str) -> i64 {
    v.get(k).and_then(|x| x.as_i64()).unwrap_or(0)
}

/// One-shot in-process attribution of the conversion cost, enabled by
/// `CSVIEWER_PROFILE_MODEL=1`. It runs *before* the real conversion and does not
/// change it; every candidate strategy is measured on the same data so the fix
/// is chosen from numbers rather than guesses.
fn micro_profile(raw_nodes: &[serde_json::Value], raw_edges: &[serde_json::Value]) {
    use std::hint::black_box;

    const STR_KEYS: [&str; 7] = ["id", "label", "name", "kind", "ns", "owner", "file"];
    const INT_KEYS: [&str; 3] = ["line", "endLine", "degree"];

    let t = Instant::now();
    let mut acc = 0usize;
    for v in raw_nodes {
        for k in STR_KEYS {
            if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
                acc += s.len();
            }
        }
        for k in INT_KEYS {
            acc += v.get(k).and_then(|x| x.as_i64()).unwrap_or(0).unsigned_abs() as usize;
        }
    }
    let scan = t.elapsed();
    black_box(acc);

    let t = Instant::now();
    let mut owned: Vec<String> = Vec::with_capacity(raw_nodes.len() * 6);
    for v in raw_nodes {
        for k in ["id", "label", "kind", "ns", "owner", "file"] {
            owned.push(v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string());
        }
    }
    let alloc = t.elapsed();
    black_box(&owned);

    let t = Instant::now();
    let mut map_owned: std::collections::HashMap<String, usize> =
        std::collections::HashMap::with_capacity(raw_nodes.len());
    for (i, v) in raw_nodes.iter().enumerate() {
        map_owned.insert(v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(), i);
    }
    let map_string = t.elapsed();
    black_box(&map_owned);

    let t = Instant::now();
    let mut map_ref: std::collections::HashMap<&str, usize, FnvBuild> =
        std::collections::HashMap::with_capacity_and_hasher(raw_nodes.len(), FnvBuild::default());
    for (i, v) in raw_nodes.iter().enumerate() {
        map_ref.insert(v.get("id").and_then(|x| x.as_str()).unwrap_or(""), i);
    }
    let map_borrow = t.elapsed();
    black_box(&map_ref);

    let t = Instant::now();
    let mut hits = 0usize;
    for e in raw_edges {
        let s = e.get("source").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let d = e.get("target").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if map_owned.contains_key(&s) && map_owned.contains_key(&d) {
            hits += 1;
        }
    }
    let edge_string = t.elapsed();
    black_box(hits);

    let t = Instant::now();
    let mut hits2 = 0usize;
    for e in raw_edges {
        let s = e.get("source").and_then(|x| x.as_str()).unwrap_or("");
        let d = e.get("target").and_then(|x| x.as_str()).unwrap_or("");
        if map_ref.contains_key(s) && map_ref.contains_key(d) {
            hits2 += 1;
        }
    }
    let edge_borrow = t.elapsed();
    black_box(hits2);

    let t = Instant::now();
    let mut kinds: Vec<String> = Vec::with_capacity(raw_edges.len());
    for e in raw_edges {
        kinds.push(e.get("kind").and_then(|x| x.as_str()).unwrap_or("").to_string());
    }
    let edge_kind = t.elapsed();
    black_box(&kinds);

    eprintln!(
        "[model-profile] micro nodes={} edges={} (one pass each)",
        raw_nodes.len(),
        raw_edges.len()
    );
    eprintln!("[model-profile]   value_scan          {scan:?}  (all 10 fields, no alloc)");
    eprintln!("[model-profile]   node_string_alloc   {alloc:?}  (6 Strings/node)");
    eprintln!("[model-profile]   id_map_string       {map_string:?}  (String keys + clone, SipHash)");
    eprintln!("[model-profile]   id_map_borrowed     {map_borrow:?}  (&str keys, FNV)");
    eprintln!("[model-profile]   edge_lookup_string  {edge_string:?}  (2 String allocs/edge)");
    eprintln!("[model-profile]   edge_lookup_borrow  {edge_borrow:?}  (0 allocs/edge)");
    eprintln!("[model-profile]   edge_kind_alloc     {edge_kind:?}  (1 String/edge)");
}

/// Resolve the repository to index: `--repo` wins, else `CSVIEWER_REPO`, else
/// the current directory (the engine's own `resolve_repo` policy, minus the
/// auto-detection walk which is a CLI concern).
pub fn resolve_repo(cli: Option<&Path>) -> PathBuf {
    if let Some(p) = cli {
        return p.to_path_buf();
    }
    if let Ok(env) = std::env::var("CSVIEWER_REPO") {
        if !env.is_empty() {
            return PathBuf::from(env);
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Load the engine config for `repo`, honouring a `csgraph.config.json` in the
/// same two places the Tauri host looks.
pub fn load_config(repo: &Path) -> Config {
    for c in [
        repo.join("tools/csgraph/csgraph.config.json"),
        repo.join("csgraph.config.json"),
    ] {
        if c.is_file() {
            return config::load_config(&c);
        }
    }
    Config::default()
}

/// Roots to index: explicit roots, else config roots that exist, else the repo.
pub fn resolve_roots(repo: &Path, cli: &[String], cfg: &Config) -> Vec<String> {
    if !cli.is_empty() {
        return cli.to_vec();
    }
    let from_cfg: Vec<String> = cfg
        .roots
        .iter()
        .filter(|r| repo.join(r).exists())
        .cloned()
        .collect();
    if !from_cfg.is_empty() {
        return from_cfg;
    }
    vec![repo.to_string_lossy().into_owned()]
}

/// Build the index, convert it into the view's graph, **and keep the index**.
///
/// The source pane needs the live `Index` (`Index::doc_source`) for as long as
/// the app runs, so integration must not drop it. `load` keeps its old shape for
/// callers that only want the graph.
pub fn load_with_index(repo: &Path, roots: &[String]) -> Result<(Graph, Index), String> {
    load_with_index_impl(repo, roots, None).map(|(graph, index, _)| (graph, index))
}

/// [`load_with_index`] with the engine's Stop flag shared in.
///
/// The index build is the one long-running engine call in the app. `#stopIndexBtn`
/// and Esc-while-indexing flip this flag from the paint thread; the engine
/// checks it between files and at each phase boundary. A cancelled build
/// restores its snapshot and returns the **previous** index state with
/// `cancelled: true` (see `csgraph_core::index::Index::build`), which is why the
/// result carries the cancelled bit: the host keeps its current graph instead of
/// adopting a restored copy.
pub fn load_with_index_cancellable(
    repo: &Path,
    roots: &[String],
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(Graph, Index, bool), String> {
    load_with_index_impl(repo, roots, Some(cancel))
}

fn load_with_index_impl(
    repo: &Path,
    roots: &[String],
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> Result<(Graph, Index, bool), String> {
    let repo = csgraph_core::index::abspath(repo);
    let cfg = load_config(&repo);
    let mut index = Index::new(repo.clone());
    if let Some(flag) = cancel {
        index.set_cancel_flag(flag);
    }
    let profiling = profile_enabled();
    let t_build = Instant::now();
    let state = index.build(roots, &cfg.excludes, &cfg);
    let t_build = t_build.elapsed();
    let cancelled = state
        .get("cancelled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if profiling {
        micro_profile(index.nodes(), index.edges());
    }

    if profiling {
        eprintln!("[model-profile]   engine_build={t_build:?}");
    }
    let conv = convert(index.nodes(), index.edges());
    // The engine's real project model. `/api/state` carries no project list, so
    // this is the only place the view can learn the corpus's `.sln`/`.csproj`
    // set; it is read from the live index rather than guessed from file names.
    let projects = project_rows_from_payload(&index.projects_payload(false, None));

    Ok((
        Graph {
            repo,
            nodes: conv.nodes,
            edges: conv.edges,
            state,
            skipped: conv.skipped,
            projects,
        },
        index,
        cancelled,
    ))
}

/// The app-side view of the engine's JSON: kept nodes, mapped edges, dropped
/// kinds.
pub struct Conversion {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub skipped: Vec<(String, usize)>,
}

/// Convert the engine's `Value` nodes/edges into the view's model.
///
/// This is the task-25 hotspot. The semantics the old implementation had, and
/// that this one keeps — pinned by the tests below:
///
/// * `external` / `literal` nodes are dropped and counted by kind, sorted;
/// * an edge is dropped when either endpoint was dropped, or when it is a
///   self-loop after compaction;
/// * a duplicated id keeps the last occurrence.
///
/// The costs removed, each measured first (`CSVIEWER_PROFILE_MODEL=1`): ids are
/// borrowed from the engine's own strings for the map key (no clone), edge
/// endpoints are looked up without allocating two `String`s per edge, dropped
/// nodes are never materialised, and the map uses FNV rather than SipHash.
///
/// `pub` so the task-25 A/B harness can time the old and new algorithms on the
/// same engine data in one process.
pub fn convert(raw_nodes: &[serde_json::Value], raw_edges: &[serde_json::Value]) -> Conversion {
    let profiling = profile_enabled();
    let mut nodes: Vec<Node> = Vec::with_capacity(raw_nodes.len());
    // id -> kept slot. Keys borrow the engine's `Value` strings.
    let mut ix_of: std::collections::HashMap<&str, usize, FnvBuild> =
        std::collections::HashMap::with_capacity_and_hasher(raw_nodes.len(), FnvBuild::default());
    let mut skipped_kinds: Vec<(&str, usize)> = Vec::new();

    let t_nodes = Instant::now();
    for v in raw_nodes {
        let kind = str_at(v, "kind");
        let external =
            v.get("external").and_then(|b| b.as_bool()).unwrap_or(false) || kind == "external";
        if external || kind == "literal" {
            match skipped_kinds.iter_mut().find(|(k, _)| *k == kind) {
                Some((_, c)) => *c += 1,
                None => skipped_kinds.push((kind, 1)),
            }
            continue;
        }
        let id = str_at(v, "id");
        let mut label = str_at(v, "label");
        if label.is_empty() {
            label = str_at(v, "name");
        }
        let slot = nodes.len();
        ix_of.insert(id, slot);
        nodes.push(Node {
            id: id.to_string(),
            label: label.to_string(),
            full: str_at(v, "full").to_string(),
            kind: kind.to_string(),
            ns: str_at(v, "ns").to_string(),
            parent: str_at(v, "parent").to_string(),
            owner: str_at(v, "owner").to_string(),
            file: str_at(v, "file").to_string(),
            line: int_at(v, "line"),
            end_line: int_at(v, "endLine"),
            degree: int_at(v, "degree"),
            ret: str_at(v, "ret").to_string(),
            external,
        });
    }
    let t_nodes = t_nodes.elapsed();

    let t_edges = Instant::now();
    let mut edges = Vec::with_capacity(raw_edges.len());
    for e in raw_edges {
        // `ix_of` already stores compacted slots, so the endpoints are used
        // directly: the old code remapped a *raw* index a second time, which is
        // what made a rewrite silently lose an edge.
        let (s, t) = (str_at(e, "source"), str_at(e, "target"));
        let (Some(&si), Some(&ti)) = (ix_of.get(s), ix_of.get(t)) else {
            continue;
        };
        if si == ti {
            continue;
        }
        edges.push(Edge {
            from: si,
            to: ti,
            kind: str_at(e, "kind").to_string(),
            count: int_at(e, "count"),
        });
    }
    let t_edges = t_edges.elapsed();

    // `BTreeMap` used to give kind-sorted output; keep that determinism.
    skipped_kinds.sort_by(|a, b| a.0.cmp(b.0));

    if profiling {
        eprintln!(
            "[model-profile] conversion raw_nodes={} kept={} skipped={} raw_edges={} kept_edges={}",
            raw_nodes.len(),
            nodes.len(),
            raw_nodes.len() - nodes.len(),
            raw_edges.len(),
            edges.len()
        );
        eprintln!(
            "[model-profile]   nodes_loop={t_nodes:?} edges_loop={t_edges:?} conversion={:?}",
            t_nodes + t_edges
        );
    }

    Conversion {
        nodes,
        edges,
        skipped: skipped_kinds.into_iter().map(|(k, c)| (k.to_string(), c)).collect(),
    }
}

/// Graph-only convenience over [`load_with_index`].
pub fn load(repo: &Path, roots: &[String]) -> Result<Graph, String> {
    load_with_index(repo, roots).map(|(g, _index)| g)
}

// -- the persisted "last project" pointer ---------------------------------
//
// Lives here rather than in `main.rs` because the WINDOW binary is not the only
// writer: `App::commit_repo` (in `app.rs`, compiled into both the binary and the
// library) changes the corpus mid-run, and a choice that is not persisted would
// be forgotten on the next launch — which then reopens the previous project and
// looks like the target folder was never indexed.

/// Persisted "last project" pointer file name.
const LAST_PROJECT_FILE: &str = "last-project.json";

/// Where a no-arg launch looks for the pointer. Two places, both writable and
/// both outside any single repo: the csgraph config dir when it is usable
/// (honours `$CSGRAPH_SETTINGS` / `$XDG_CONFIG_HOME`), the boot-durable state
/// dir, then the engine's registry dir (`$XDG_RUNTIME_DIR/csviewer`, else
/// `$TMPDIR/csviewer`). A read-only `$HOME` makes the config dir unwritable, so
/// a later location is where the write normally lands.
fn last_project_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let settings = csgraph_core::config::settings_path();
    if let Some(dir) = settings.parent() {
        out.push(dir.join(LAST_PROJECT_FILE));
    }
    // A boot-durable per-user state dir, before the registry dir: the registry
    // lives under `$XDG_RUNTIME_DIR` (per-boot), and the pointer must survive a
    // reboot for "double-click the AppImage tomorrow" to reopen the project.
    let state = std::env::var("XDG_STATE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".local/state")));
    if let Some(dir) = state {
        out.push(dir.join("csgraph").join(LAST_PROJECT_FILE));
    }
    out.push(csgraph_core::registry::write_dir().join(LAST_PROJECT_FILE));
    out
}

/// Read the saved repo+roots. A saved repo that no longer exists is reported and
/// ignored (honest fallback), never silently indexed.
pub fn load_last_project() -> Option<(PathBuf, Vec<String>)> {
    for path in last_project_paths() {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            eprintln!("[native-csviewer] ignoring malformed {}", path.display());
            continue;
        };
        let Some(repo) = v.get("repo").and_then(|r| r.as_str()) else { continue };
        let repo = PathBuf::from(repo);
        let roots = v
            .get("roots")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        if repo.is_dir() {
            return Some((repo, roots));
        }
        eprintln!(
            "[native-csviewer] saved project {} no longer exists (from {}); falling back to cwd",
            repo.display(),
            path.display()
        );
    }
    None
}

/// Persist the repo+roots actually in use. Best effort: a read-only location
/// must never stop the viewer from starting or from switching corpora.
pub fn save_last_project(repo: &Path, roots: &[String]) {
    let body = serde_json::json!({
        "version": 1,
        "repo": repo.to_string_lossy(),
        "roots": roots,
    });
    let text = body.to_string();
    for path in last_project_paths() {
        if let Some(dir) = path.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                continue;
            }
        }
        match std::fs::write(&path, &text) {
            Ok(()) => {
                eprintln!("[native-csviewer] saved last project to {}", path.display());
                return;
            }
            Err(e) => eprintln!(
                "[native-csviewer] cannot write {} ({e}); trying next location",
                path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, kind: &str) -> serde_json::Value {
        json!({
            "id": id, "label": id, "kind": kind, "ns": "N", "owner": "",
            "file": "f.cs", "line": 1, "endLine": 2, "degree": 3
        })
    }

    fn edge(s: &str, t: &str, kind: &str) -> serde_json::Value {
        json!({"source": s, "target": t, "kind": kind, "count": 1})
    }

    fn ids(c: &Conversion) -> Vec<&str> {
        c.nodes.iter().map(|n| n.id.as_str()).collect()
    }

    #[test]
    fn convert_keeps_nodes_drops_externals_and_maps_edges() {
        let nodes = vec![
            node("A", "namespace"),
            node("B", "class"),
            json!({"id": "X", "kind": "external"}),
            json!({"id": "L", "kind": "literal"}),
        ];
        let edges = vec![
            edge("A", "B", "contains"),
            edge("A", "X", "calls"), // dropped endpoint
            edge("B", "B", "calls"), // self-loop
        ];
        let c = convert(&nodes, &edges);
        assert_eq!(ids(&c), ["A", "B"]);
        assert_eq!(c.edges.len(), 1);
        assert_eq!(
            (c.edges[0].from, c.edges[0].to, c.edges[0].kind.as_str()),
            (0, 1, "contains")
        );
        assert_eq!(c.skipped, vec![("external".to_string(), 1), ("literal".to_string(), 1)]);
    }

    /// Regression guard for the rewrite that first lost an edge (7979 -> 7978 on
    /// the bench corpus): endpoints are compacted *slots*, never re-remapped.
    #[test]
    fn convert_endpoints_are_compacted_slots() {
        let nodes = vec![
            json!({"id": "X", "kind": "external"}),
            node("A", "class"),
            json!({"id": "Y", "kind": "external"}),
            node("B", "class"),
        ];
        let edges = vec![edge("A", "B", "uses")];
        let c = convert(&nodes, &edges);
        assert_eq!(ids(&c), ["A", "B"]);
        assert_eq!((c.edges[0].from, c.edges[0].to), (0, 1));
    }

    #[test]
    fn convert_label_falls_back_to_name_and_reads_ints() {
        let nodes = vec![json!({
            "id": "A", "kind": "class", "name": "ByName",
            "line": 7, "endLine": 9, "degree": 4, "external": false
        })];
        let c = convert(&nodes, &[]);
        assert_eq!(c.nodes[0].label, "ByName");
        assert_eq!((c.nodes[0].line, c.nodes[0].end_line, c.nodes[0].degree), (7, 9, 4));
    }

    #[test]
    fn convert_duplicate_id_keeps_the_last_occurrence() {
        let nodes = vec![node("D", "class"), node("D", "class"), node("E", "class")];
        let edges = vec![edge("D", "E", "uses")];
        let c = convert(&nodes, &edges);
        // D resolved to its last slot (1), E to slot 2.
        assert_eq!((c.edges[0].from, c.edges[0].to), (1, 2));
    }

    #[test]
    fn convert_counts_external_flag_as_well_as_kind() {
        let nodes = vec![
            json!({"id": "A", "kind": "class", "external": true}),
            node("B", "class"),
        ];
        let c = convert(&nodes, &[]);
        assert_eq!(ids(&c), ["B"]);
        assert_eq!(c.skipped, vec![("class".to_string(), 1)]);
    }

    #[test]
    fn fnv_hasher_is_deterministic_and_distinguishes_keys() {
        use std::hash::{BuildHasher, Hash, Hasher};
        let b = FnvBuild::default();
        let h = |s: &str| {
            let mut hh = b.build_hasher();
            s.hash(&mut hh);
            hh.finish()
        };
        assert_eq!(h("abc"), h("abc"));
        assert_ne!(h("abc"), h("abd"));
        assert_ne!(h(""), h("a"));
    }

    // -- D3 live state: real projects / warnings ---------------------------

    /// The engine's `fileCount` must land on the renderer's `file_count`.
    ///
    /// This is the whole reason the rows are mapped rather than passed through:
    /// nzk_ui's `ProjectRow` declares `file_count`, the engine's JSON keys it
    /// `fileCount`, and a naive passthrough leaves the renderer at 0.
    #[test]
    fn project_rows_map_engine_filecount_to_the_renderers_file_count() {
        let payload = json!({"projects": [{
            "id": "App/App.csproj", "name": "App", "path": "App/App.csproj",
            "targets": ["net472"], "defines": ["TRACE", "APP"],
            "fileCount": 7, "sdkStyle": true
        }]});
        let rows = project_rows_from_payload(&payload);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "App/App.csproj");
        assert_eq!(rows[0].name, "App");
        assert_eq!(rows[0].file_count, 7, "fileCount must become file_count");
        assert_eq!(rows[0].targets, vec!["net472".to_string()]);
        assert_eq!(rows[0].defines, vec!["TRACE".to_string(), "APP".to_string()]);
        // Serialised with the renderer's key names, not the engine's.
        let j = rows[0].to_json();
        assert_eq!(j["file_count"], 7);
        assert!(j.get("fileCount").is_none(), "engine spelling must not leak");
        assert!(j.get("sdkStyle").is_none(), "engine-only field must not leak");
    }

    /// Both spellings are read, so neither side's rename can zero the count.
    #[test]
    fn project_rows_accept_either_count_spelling() {
        let rows = project_rows_from_payload(
            &json!([{"id": "L/L.csproj", "file_count": 3}]),
        );
        assert_eq!(rows[0].file_count, 3, "bare-array form + snake_case key");
    }

    /// A corpus with no project model yields no rows — which is what keeps the
    /// renderer's "no .sln/.csproj found" chip honest rather than decorative.
    #[test]
    fn project_rows_are_empty_when_the_engine_reports_none() {
        assert!(project_rows_from_payload(&json!({"projects": []})).is_empty());
        assert!(project_rows_from_payload(&json!({})).is_empty());
    }

    /// `problems.js parseWarning` shape 1: `path(line,col): rest`.
    #[test]
    fn warning_row_parses_msbuild_style_positions() {
        let w = warning_row_from("src/Foo.cs(12,5): warning CS0168: unused variable 'x'");
        assert_eq!(w.file, "src/Foo.cs");
        assert_eq!((w.line, w.col), (12, 5));
        assert_eq!(w.text, "warning CS0168: unused variable 'x'");
        assert_eq!(w.raw, "src/Foo.cs(12,5): warning CS0168: unused variable 'x'");
    }

    /// `problems.js parseWarning` shape 2: `path:line` and `path:line:col`.
    #[test]
    fn warning_row_parses_repo_relative_positions() {
        let w = warning_row_from("src/Foo.cs:12: parser: something odd");
        assert_eq!(w.file, "src/Foo.cs");
        assert_eq!((w.line, w.col), (12, 0));
        assert_eq!(w.text, "parser: something odd");

        let w2 = warning_row_from("src/Foo.cs:12:5: parser: something odd");
        assert_eq!(w2.file, "src/Foo.cs");
        assert_eq!((w2.line, w2.col), (12, 5));
        assert_eq!(w2.text, "parser: something odd");
    }

    /// The position-free case must stay position-free.
    ///
    /// nzk_ui's renderer branches on `!file.is_empty() && line > 0`
    /// (dom.rs:983), so an empty file with line 0 is *the* encoding for "show
    /// this as text with no location link". Filling in a fake file or line 1
    /// here would manufacture a clickable link to a place that does not exist.
    #[test]
    fn warning_row_keeps_bare_prose_position_free() {
        let w = warning_row_from("scope guard tripped");
        assert_eq!(w.file, "");
        assert_eq!((w.line, w.col), (0, 0));
        assert_eq!(w.text, "scope guard tripped");
        assert_eq!(w.raw, "scope guard tripped", "verbatim text is preserved");

        // A prose message containing a colon is not a location.
        let p = warning_row_from("note: this is not a path");
        assert_eq!(p.file, "");
        assert_eq!(p.line, 0);
    }

    /// The row count comes from the engine's own list, not a literal.
    #[test]
    fn warning_rows_track_the_engines_real_list() {
        let none = warning_rows_from_state(&json!({"warnings": []}));
        assert!(none.is_empty(), "a clean corpus has no rows");

        let state = json!({"warnings": [
            "src/Foo.cs(3,1): warning CS0168: unused",
            "scope guard tripped",
        ]});
        let rows = warning_rows_from_state(&state);
        assert_eq!(rows.len(), 2, "count is the engine's, not a constant");
        assert_eq!(rows[0].file, "src/Foo.cs");
        assert_eq!(rows[1].file, "", "the prose warning stays position-free");

        // Absent key = no warnings recorded; never a panic or a fabrication.
        assert!(warning_rows_from_state(&json!({})).is_empty());
    }

    /// The Roslyn path is opt-in and must not fabricate anything when off.
    #[test]
    fn rosllyn_rows_are_opt_in_and_never_invented() {
        // Env is not set in the test process, so this must short-circuit to
        // empty without spawning a compiler.
        assert!(rosllyn_warning_rows(&[], 1).is_empty());
    }
}


