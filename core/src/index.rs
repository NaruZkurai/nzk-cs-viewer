//! index — port of the `Index` class in `tools/csgraph/server.py`.
//!
//! `Index` owns one built graph: the normalized node/edge records, the display
//! path -> source text map, the per-file declaration ranges (for code lookup),
//! the conditional-compilation symbol set, and the on-disk cache. Everything
//! `server.py` answers a `/api/*` query from lives here; the HTTP shape itself
//! is `api.rs`.
//!
//! Fidelity notes, all of them observable in `tools/oracle.py`'s dump:
//!
//! * node records are rebuilt field-by-field exactly like `Index.build`'s
//!   comprehension, including the `literal`/`external` booleans and the
//!   `head`/`tail`/`startOff`/`endOff`/`parts` source-viewer payload;
//! * `degree` is re-derived as the number of *distinct* neighbours (the graph
//!   builder counts edges), and `inDeg`/`outDeg` count edges;
//! * edge ids are renumbered `e0..eN` in graph order;
//! * node insertion order is preserved (a `Vec` + id index, not a map), because
//!   every stable sort in the query methods can fall back on it;
//! * `parse_ms` and the cache hit/miss counters are timing state, never part of
//!   the canonical document.
//!
//! SHA-256 is implemented here rather than pulled in as a dependency: the
//! crate list is fixed and none of `serde`/`regex`/`flate2`/`walkdir` is a
//! digest, yet `oracle.py`'s `_doc_digest` hashes the document lines with
//! `hashlib.sha256`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use walkdir::WalkDir;

use crate::cache::{Cache, CACHE_DIR};
use crate::config::{
    define_sources, discover_projects, project_file_map, resolve_defines, Config, ProjectModel,
};
use crate::doc::{self, DocCache};
use crate::graph::build_graph;
use crate::lex::{active_line_flags, mask_noncode, strip_bom};
use crate::model::to_chars;
use crate::parser::{discover_files, display_path};

/// The Python cache db name; kept only so it can be excluded from discovery
/// (the v2 cache is a directory, [`CACHE_DIR`]).
pub const DB_NAME: &str = ".csgraph.db";
/// `server.py`'s `DB_VERSION`; part of the graph-cache signature. Bumped to 4
/// when the canonical file-form rule landed (gap 5): a payload written under
/// the old, repo-spelling-dependent form must not be served.
pub const DB_VERSION: u32 = 4;

/// Discovery guard: the most files a single `build` may walk before refusing.
///
/// Deliberately far above any real corpus (the default `_sealed_src` is 52
/// files) but far below the hundreds of thousands of entries a stray
/// `target/` or vendored SDK contributes. Override for tests with
/// `CSGRAPH_MAX_WALK_FILES`.
pub const DEFAULT_MAX_WALK_FILES: usize = 200_000;

/// Discovery guard: the wall-clock ceiling for the pre-flight walk, in seconds.
/// The owner's rule is no operation over 30 s, so this leaves room for the
/// cheap post-guard discovery and parse. Override with `CSGRAPH_MAX_WALK_SECS`.
pub const DEFAULT_MAX_WALK_SECS: f64 = 15.0;

/// Read the discovery ceilings, honouring the test overrides.
fn walk_limits() -> (usize, f64) {
    let files = std::env::var("CSGRAPH_MAX_WALK_FILES")
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_MAX_WALK_FILES);
    let secs = std::env::var("CSGRAPH_MAX_WALK_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|n| *n > 0.0)
        .unwrap_or(DEFAULT_MAX_WALK_SECS);
    (files, secs)
}

/// Pre-flight bound on [`discover_files`]' recursive walk.
///
/// Applies the *same* excludes as discovery and returns `Err` as soon as the
/// walk visits more than `max_files` files or runs longer than `max_secs`.
/// The error names the ceiling and the point the walk reached, so the caller
/// can narrow `roots` or add an `exclude` instead of waiting.
fn bounded_walk_check(
    repo: &Path,
    roots: &[String],
    excludes: &[String],
    max_files: usize,
    max_secs: f64,
) -> Result<(), String> {
    let start = Instant::now();
    let mut files = 0usize;
    let mut entries = 0usize;
    // `parser::excluded` is called once per directory and compiles a regex per
    // pattern, which is fine for a real discovery pass but makes this pre-flight
    // walk slower than the walk it guards. Split the patterns: a plain name with
    // no glob metachar and no `/` is matched by component equality, which is
    // exactly what `excluded`'s `parts.contains(ex)` arm does. Only the
    // genuinely patterned excludes fall back to the shared matcher.
    let mut literal: HashSet<&str> = HashSet::new();
    let mut patterned: Vec<String> = Vec::new();
    for ex in excludes {
        if ex.is_empty() {
            continue;
        }
        if !ex.contains('/') && !ex.contains(['*', '?', '[']) {
            literal.insert(ex.as_str());
        } else {
            patterned.push(ex.clone());
        }
    }
    for root in roots {
        if root.is_empty() {
            continue;
        }
        let p = if Path::new(root).is_absolute() {
            PathBuf::from(root)
        } else {
            repo.join(root)
        };
        // A root that names a file contributes at most one file; there is no
        // tree to bound.
        if p.is_file() {
            files += 1;
            if files > max_files {
                return Err(bounds_error(files, start.elapsed().as_secs_f64(), max_files, max_secs));
            }
            continue;
        }
        let walk = WalkDir::new(&p).follow_links(true).into_iter().filter_entry(|e| {
            if e.depth() == 0 || !e.file_type().is_dir() {
                return true;
            }
            let rel = e
                .path()
                .strip_prefix(&p)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let mut parts: HashSet<String> = HashSet::new();
            for part in rel.split('/') {
                if !part.is_empty() {
                    parts.insert(part.to_string());
                }
            }
            if parts.iter().any(|part| literal.contains(part.as_str())) {
                return false;
            }
            !crate::parser::excluded(&rel, &parts, &patterned)
        });
        for entry in walk {
            let Ok(entry) = entry else { continue };
            entries += 1;
            if entry.file_type().is_file() {
                files += 1;
                if files > max_files {
                    return Err(bounds_error(
                        files,
                        start.elapsed().as_secs_f64(),
                        max_files,
                        max_secs,
                    ));
                }
            }
            // One clock read per 256 entries visited. Counting entries, not
            // files, is what bounds a directory-heavy tree that contributes few
            // files: the walk still cannot run past the deadline there.
            if entries % 256 == 0 && start.elapsed().as_secs_f64() > max_secs {
                return Err(bounds_error(
                    files,
                    start.elapsed().as_secs_f64(),
                    max_files,
                    max_secs,
                ));
            }
        }
    }
    if std::env::var_os("CSGRAPH_BUILD_TIMING").is_some() {
        eprintln!("[timing] guard walked {files} files");
    }
    if start.elapsed().as_secs_f64() > max_secs {
        return Err(bounds_error(files, start.elapsed().as_secs_f64(), max_files, max_secs));
    }
    Ok(())
}

fn bounds_error(files: usize, secs: f64, max_files: usize, max_secs: f64) -> String {
    format!(
        "directory discovery aborted: walked {files} files in {secs:.1}s \
         (limits {max_files} files / {max_secs:.0}s); narrow `roots` or add an `exclude`"
    )
}

/// Most references listed for an external symbol before the document is cut.
pub const EXTERNAL_REF_LIMIT: usize = 400;

/// Most declarations `/api/decls` returns in one response.
pub const DECLS_LIMIT: usize = 5000;

/// How many synthesized documents an index memoizes before evicting the oldest.
/// Each one can be megabytes, so this is a memory ceiling, not just a cache size.
pub const DOC_CACHE_MAX: usize = 16;

/// Files per parallel parse chunk in [`Index::parse_files`]. Small enough that a
/// Stop request is honoured within a chunk, large enough that the scoped worker
/// pool is not rebuilt per file.
const PARSE_CHUNK: usize = 64;

/// True when a node is an unresolved stub — a symbol referenced but never
/// declared inside the indexed roots.
pub fn is_external_node(v: &Value) -> bool {
    v.get("kind").and_then(|k| k.as_str()) == Some("external")
}

/// Python `int(x)` for a JSON value, with `default` on failure.
pub fn py_int(v: Option<&Value>, default: i64) -> i64 {
    let Some(v) = v else { return default };
    match v {
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i
            } else if let Some(f) = n.as_f64() {
                f as i64
            } else {
                default
            }
        }
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(default),
        Value::Bool(b) => *b as i64,
        _ => default,
    }
}

/// Python truthiness (`bool(x)`), used for `n.get('literal')` and friends.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `seq[:limit]` — negative limits count from the end.
fn py_slice_len(len: usize, limit: i64) -> usize {
    if limit >= 0 {
        (limit as usize).min(len)
    } else {
        let l = len as i64 + limit;
        if l < 0 {
            0
        } else {
            l as usize
        }
    }
}

/// `os.path.normpath` for the component forms that occur here (`.`/`..`).
pub fn normpath(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// The canonical file identity used by every file-keyed map in the index
/// (gap 5, `node-key vs file-key path forms`).
///
/// The rule is exactly what Python's `Index.display` settles — **repo-relative
/// when the path is inside the repo, normalized absolute otherwise** — applied
/// to whichever form a caller (or a stale parse cache) hands us:
///
/// * absolute inside `repo` -> `display_path`, i.e. repo-relative;
/// * absolute outside `repo` -> normalized absolute (as `display_path` keeps it);
/// * already relative -> unchanged apart from forward-slash normalisation.
///
/// `node.file`, `file_texts` keys and the `nodes_by_file`/`node_ranges` keys all
/// use this one form, so a lookup by either spelling resolves.
pub fn canon_file_with(file: &str, repo: &Path) -> String {
    if file.is_empty() {
        return String::new();
    }
    let p = Path::new(file);
    if p.is_absolute() {
        display_path(p, repo)
    } else {
        file.replace('\\', "/")
    }
}

/// Apply [`canon_file_with`] to a node's `file` and every `parts[].file`.
/// Idempotent on a consistent corpus; this is what settles the gap-5 form.
fn canonicalize_node_files(n: &mut Value, repo: &Path) {
    if let Some(f) = n.get("file").and_then(|v| v.as_str()).map(str::to_string) {
        let canon = canon_file_with(&f, repo);
        if canon != f {
            n["file"] = json!(canon);
        }
    }
    if let Some(parts) = n.get_mut("parts").and_then(Value::as_array_mut) {
        for p in parts.iter_mut() {
            if let Some(f) = p.get("file").and_then(|v| v.as_str()).map(str::to_string) {
                let canon = canon_file_with(&f, repo);
                if canon != f {
                    p["file"] = json!(canon);
                }
            }
        }
    }
}

/// `os.path.abspath` — normalise against the current directory, no symlinks.
pub fn abspath(p: &Path) -> PathBuf {
    if p.is_absolute() {
        normpath(p)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        normpath(&cwd.join(p))
    }
}

fn mtime_ns(meta: &fs::Metadata) -> i128 {
    match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        },
        Err(_) => 0,
    }
}

fn mtime_secs(meta: &fs::Metadata) -> f64 {
    mtime_ns(meta) as f64 / 1e9
}

/// One indexed file's ASCII-lowercased view, used by [`Index::text_hits`]'s
/// case-insensitive fast path.
#[derive(Debug)]
struct LcFile {
    /// `to_ascii_lowercase()` of the source text: same byte length, non-ASCII
    /// bytes untouched, so it splits by `\n` in lockstep with the original.
    lower: String,
    /// Whether the original text is entirely ASCII, so `lower.contains(ql)` is a
    /// sound file-level prefilter for an ASCII `ql`.
    ascii: bool,
}

/// ASCII word byte, matching the regex crate's `\b` on ASCII input.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// One cached parse result. Mirrors the `(size, mtime, blob)` row of the
/// Python `file_cache` table, with the pickle replaced by JSON.
#[derive(Serialize, Deserialize)]
struct ParseEntry {
    size: u64,
    mtime: f64,
    parse: Value,
}

/// The whole graph, as persisted by the graph cache.
#[derive(Serialize, Deserialize)]
struct GraphPayload {
    nodes: Vec<Value>,
    edges: Vec<Value>,
    counts: Value,
    file_texts: HashMap<String, String>,
    node_ranges: HashMap<String, Vec<(i64, i64, String)>>,
    warnings: Vec<String>,
    file_count: usize,
}

/// Mutable state `build` must put back if it is cancelled mid-flight, so the
/// live index keeps answering exactly as it did before the cancelled attempt.
///
/// Only the fields `build` writes *before* it installs a payload are captured:
/// the graph itself (`nodes`/`edges`/...) is never touched until `install`, so
/// a cancelled build cannot partially overwrite the previous index.
struct BuildSnapshot {
    cfg: Config,
    defines: Option<HashSet<String>>,
    defines_known: bool,
    defines_sig: String,
    player: bool,
    roots: Vec<String>,
    project_model: ProjectModel,
    project_of_file: std::collections::BTreeMap<String, Vec<String>>,
    define_projects: std::collections::BTreeMap<String, Vec<String>>,
    cache_path: String,
    cache_hits: usize,
    cache_misses: usize,
    from_cache: bool,
}

/// `class Index`.
/// Pre-lowercased node strings, in `nodes` order.
///
/// `/api/search` and `/api/find` compare case-insensitively against `full`,
/// `name`, `signature` and `id`. Doing that per query re-cased the whole graph
/// on every call (three `to_lowercase` allocations per node, plus one per id in
/// `find`). The work is graph-shaped, not query-shaped, so it is done once in
/// [`Index::install`] instead — O(N) once rather than O(N) per query.
#[derive(Debug, Default)]
struct NodeLc {
    id: String,
    full: String,
    name: String,
    label: String,
    signature: String,
}

pub struct Index {
    pub repo: PathBuf,
    repo_str: String,
    /// Normalized nodes in graph insertion order.
    nodes: Vec<Value>,
    /// Lowercased view of each node, index-aligned with `nodes`.
    node_lc: Vec<NodeLc>,
    node_ix: HashMap<String, usize>,
    /// id -> node object, built once per install: `doc::prepare` wants a map and
    /// rebuilding (and deep-cloning) it per document synthesis was a peak cost.
    nodes_by_id: Map<String, Value>,
    edges: Vec<Value>,
    /// node id -> indices into `edges`, in the order `Index.build` appends them.
    adj: HashMap<String, Vec<usize>>,
    file_texts: HashMap<String, String>,
    /// Display file paths in sorted order, built once at [`Index::install`].
    ///
    /// `text_hits` scanned the corpus in sorted file order, and re-collected and
    /// re-sorted `file_texts.keys()` on every call. The order is graph-shaped,
    /// not query-shaped, so it is computed once. Private, never serialized.
    files_sorted: Vec<String>,
    /// ASCII-lowercased text for every indexed file, plus whether the original
    /// file is entirely ASCII (built once at [`Index::install`]).
    ///
    /// `text_hits` compiles `(?i)\b…\b`; for an ASCII query in an ASCII *line*
    /// the regex is exactly a case-insensitive substring search, so the per-line
    /// scan can use `str::find` on this copy instead of running a regex engine on
    /// every line. A `contains` miss on `lower` proves no ASCII line matches, so a
    /// fully-ASCII file is skipped and a mixed file only regex-checks its
    /// non-ASCII lines. Non-ASCII queries use the regex path throughout. Memory:
    /// one extra copy of the source text (≈ the corpus text size, not the graph),
    /// private and never serialized.
    file_texts_lc: HashMap<String, LcFile>,
    /// display path -> [(line, endLine, node id)] sorted by (line, -span).
    node_ranges: HashMap<String, Vec<(i64, i64, String)>>,
    /// file -> every node id declared in it (types, namespaces and members).
    /// `/api/decls?file=` uses this; `node_ranges` only holds members.
    nodes_by_file: HashMap<String, Vec<String>>,
    counts: Value,
    pub defines: Option<HashSet<String>>,
    pub defines_known: bool,
    defines_sig: String,
    pub player: bool,
    roots: Vec<String>,
    file_count: usize,
    parse_ms: i64,
    pub indexed: bool,
    warnings: Vec<String>,
    cache: Option<Cache>,
    cache_path: String,
    cache_hits: usize,
    cache_misses: usize,
    from_cache: bool,
    /// node id -> synthesized document (or `None` when impossible).
    ///
    /// Cached as `Arc` so a cache hit is a pointer bump, not a deep clone of a
    /// document that can be tens of thousands of lines (the peak-load cost).
    /// Bounded to [`DOC_CACHE_MAX`] documents, oldest first: one whole-corpus
    /// document is megabytes of JSON, so at peak the cache must not grow without
    /// limit just because a user clicked through nodes.
    doc_cache: HashMap<String, Option<Arc<Value>>>,
    /// Insertion order for eviction (`doc_cache` has none).
    doc_cache_order: std::collections::VecDeque<String>,
    /// File models shared by every document synthesis in this index.
    doc_models: DocCache,
    /// The `.sln`/`.csproj` model discovered at the last `build`. Additive: it
    /// answers `/api/projects` and never changes an existing payload.
    project_model: ProjectModel,
    /// display file path -> owning project ids (from the model).
    project_of_file: std::collections::BTreeMap<String, Vec<String>>,
    /// symbol -> project ids that define it.
    define_projects: std::collections::BTreeMap<String, Vec<String>>,
    cfg: Config,
    /// Set by the host's Stop route from another thread while `build` runs.
    /// Checked with a relaxed load inside the parse/read loops: an O(1) flag
    /// test, never a scan of the graph.
    cancel: Arc<AtomicBool>,
}

/// Pick one live viewer instance for a selector, or refuse with candidates.
///
/// Returns `(active, ambiguous, selectedBy, note)`. Exactly one live instance
/// auto-selects; zero is empty; several are ambiguous and `active` is `None`.
fn select_instance<'a>(
    live: &[&'a crate::registry::Instance],
    selector: Option<&str>,
) -> (Option<&'a crate::registry::Instance>, bool, &'static str, &'static str) {
    match selector {
        Some(sel) if !sel.trim().is_empty() => {
            let matched: Vec<&&'a crate::registry::Instance> =
                live.iter().filter(|i| i.matches(sel)).collect();
            match matched.len() {
                1 => (Some(*matched[0]), false, "instance", ""),
                0 => (
                    None,
                    false,
                    "instance",
                    "no live viewer instance matches the selector",
                ),
                _ => (
                    None,
                    true,
                    "instance",
                    "the selector matches several live viewers; refine it to one pid",
                ),
            }
        }
        _ => match live.len() {
            1 => (Some(live[0]), false, "auto", ""),
            0 => (None, false, "auto", "no live viewer instance was found"),
            _ => (
                None,
                true,
                "auto",
                "several live viewers are running; pass instance=<pid|repo|root> to pick one",
            ),
        },
    }
}

impl Index {
    /// `Index(repo)`.
    pub fn new(repo: PathBuf) -> Self {
        let repo = abspath(&repo);
        let repo_str = repo.to_string_lossy().into_owned();
        let mut ix = Index {
            repo,
            repo_str,
            nodes: Vec::new(),
            node_lc: Vec::new(),
            node_ix: HashMap::new(),
            nodes_by_id: Map::new(),
            edges: Vec::new(),
            adj: HashMap::new(),
            file_texts: HashMap::new(),
            files_sorted: Vec::new(),
            file_texts_lc: HashMap::new(),
            node_ranges: HashMap::new(),
            nodes_by_file: HashMap::new(),
            counts: json!({}),
            defines: None,
            defines_known: false,
            defines_sig: String::new(),
            player: false,
            roots: Vec::new(),
            file_count: 0,
            parse_ms: 0,
            indexed: false,
            warnings: Vec::new(),
            cache: None,
            cache_path: String::new(),
            cache_hits: 0,
            cache_misses: 0,
            from_cache: false,
            doc_cache: HashMap::new(),
            doc_cache_order: std::collections::VecDeque::new(),
            doc_models: DocCache::new(),
            project_model: ProjectModel::default(),
            project_of_file: std::collections::BTreeMap::new(),
            define_projects: std::collections::BTreeMap::new(),
            cfg: Config::default(),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        ix.set_defines(None, false);
        ix
    }

    /// The config the last `build` was asked for (used by `POST /api/index`).
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Share the host's stop flag with this index.
    ///
    /// The host owns one flag per engine and clones it into the index so a Stop
    /// request can flip it without taking the engine lock an in-flight build is
    /// holding.
    pub fn set_cancel_flag(&mut self, flag: Arc<AtomicBool>) {
        self.cancel = flag;
    }

    /// The shared stop flag, for a host that wants to signal without locking.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// Ask an in-flight `build` to stop. Safe from any thread: a single atomic
    /// store, no scan and no graph access.
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// True once a Stop has been requested and no later build has cleared it.
    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// The per-iteration check used inside the build loops. Relaxed is enough:
    /// the flag only ever gates a full unwind to `finish_cancelled`.
    #[inline]
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Capture the mutable bookkeeping `build` writes before it installs a
    /// payload, so a cancelled build can leave the previous index untouched.
    fn snapshot_for_cancel(&self) -> BuildSnapshot {
        BuildSnapshot {
            cfg: self.cfg.clone(),
            defines: self.defines.clone(),
            defines_known: self.defines_known,
            defines_sig: self.defines_sig.clone(),
            player: self.player,
            roots: self.roots.clone(),
            project_model: self.project_model.clone(),
            project_of_file: self.project_of_file.clone(),
            define_projects: self.define_projects.clone(),
            cache_path: self.cache_path.clone(),
            cache_hits: self.cache_hits,
            cache_misses: self.cache_misses,
            from_cache: self.from_cache,
        }
    }

    /// Put a cancelled build's partial bookkeeping back. The graph fields were
    /// never touched, so the previous index stays fully queryable.
    fn restore_for_cancel(&mut self, s: BuildSnapshot) {
        self.cfg = s.cfg;
        self.defines = s.defines;
        self.defines_known = s.defines_known;
        self.defines_sig = s.defines_sig;
        self.player = s.player;
        self.roots = s.roots;
        self.project_model = s.project_model;
        self.project_of_file = s.project_of_file;
        self.define_projects = s.define_projects;
        self.cache_path = s.cache_path;
        self.cache_hits = s.cache_hits;
        self.cache_misses = s.cache_misses;
        self.from_cache = s.from_cache;
    }

    /// End a cancelled `build`: restore whatever the previous index had and
    /// return its state flagged `cancelled: true`, never a partial result.
    fn finish_cancelled(&mut self, snapshot: Option<BuildSnapshot>) -> Value {
        if let Some(s) = snapshot {
            self.restore_for_cancel(s);
        }
        let mut state = self.state();
        if let Some(obj) = state.as_object_mut() {
            obj.insert("cancelled".to_string(), json!(true));
        }
        state
    }

    /// End a `build` refused by the discovery guard.
    ///
    /// Mirrors [`Self::finish_cancelled`]: the previous graph (if any) is
    /// restored, so a refused rebuild never installs a partial index, and the
    /// returned state carries `ok:false` plus the guard's `error` instead of
    /// pretending the index succeeded.
    fn finish_bounds(
        &mut self,
        snapshot: Option<BuildSnapshot>,
        msg: String,
        max_files: usize,
        max_secs: f64,
    ) -> Value {
        if let Some(s) = snapshot {
            self.restore_for_cancel(s);
        }
        let mut state = self.state();
        if let Some(obj) = state.as_object_mut() {
            obj.insert("ok".to_string(), json!(false));
            obj.insert("aborted".to_string(), json!("discovery-bounds"));
            obj.insert("error".to_string(), json!(msg));
            obj.insert(
                "bounds".to_string(),
                json!({
                    "maxFiles": max_files,
                    "maxSecs": max_secs,
                }),
            );
        }
        state
    }

    /// The roots the last `build` was asked for.
    pub fn roots(&self) -> &[String] {
        &self.roots
    }

    /// Install a freshly discovered `.sln`/`.csproj` model.
    fn set_projects(&mut self, model: ProjectModel) {
        self.project_of_file = project_file_map(&model).into_iter().collect();
        self.define_projects = define_sources(&model).into_iter().collect();
        self.project_model = model;
    }

    /// The project that owns `file` (repo-relative display path), if known.
    pub fn projects_for_file(&self, file: &str) -> Vec<String> {
        self.project_of_file.get(file).cloned().unwrap_or_default()
    }

    /// `file: message (project: Name, ...)` — still a parser warning, never a
    /// compiler diagnostic. The file prefix mirrors the engine's own warning
    /// shape; the project suffix is added only when a `.csproj` claims the file.
    fn attribute_warning(&self, file: &str, message: &str) -> String {
        let msg = message.trim();
        let base = if file.is_empty() || msg.starts_with(&format!("{file}:")) {
            msg.to_string()
        } else {
            format!("{file}: {msg}")
        };
        let ids = self.projects_for_file(file);
        if ids.is_empty() {
            return base;
        }
        let names: Vec<String> = ids
            .iter()
            .map(|id| {
                self.project_model
                    .projects
                    .iter()
                    .find(|p| &p.id == id)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| id.clone())
            })
            .collect();
        format!("{base} (project: {})", names.join(", "))
    }

    /// The `/api/projects` payload.
    ///
    /// Additive and self-contained: `files=1` lists each project's source set,
    /// `file=<display path>` answers the per-node lookup, and `defineSources`
    /// names the project(s) behind every conditional-compilation symbol. Before
    /// the first `build` the model is discovered on demand from the config
    /// roots (bounded), so the endpoint works on a cold engine too.
    pub fn projects_payload(&self, with_files: bool, file: Option<&str>) -> Value {
        let fallback: ProjectModel;
        let model = if self.project_model.projects.is_empty()
            && self.project_model.solutions.is_empty()
        {
            fallback = discover_projects(&self.repo, &self.cfg.roots, 3);
            &fallback
        } else {
            &self.project_model
        };
        let name_of = |id: &str| -> String {
            model
                .projects
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| id.to_string())
        };
        if let Some(f) = file {
            let ids = project_file_map(model)
                .get(f)
                .cloned()
                .unwrap_or_default();
            let names: Vec<String> = ids.iter().map(|id| name_of(id)).collect();
            return json!({
                "ok": true,
                "file": f,
                "projects": ids,
                "names": names,
                "count": model.projects.len(),
            });
        }
        let mut payload = if with_files { model.full() } else { model.compact() };
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("ok".to_string(), json!(true));
            obj.insert("count".to_string(), json!(model.projects.len()));
            obj.insert(
                "defineSources".to_string(),
                json!(self
                    .define_projects
                    .iter()
                    .map(|(sym, ids)| json!({"symbol": sym, "projects": ids}))
                    .collect::<Vec<Value>>()),
            );
        }
        payload
    }

    /// Every node, in graph insertion order (`/api/export.json`).
    pub fn nodes(&self) -> &[Value] {
        &self.nodes
    }

    /// Lowercased `(id, full, name, label)` for node `i`, index-aligned with
    /// [`Index::nodes`].
    ///
    /// Callers that rank many nodes against one query (the MCP `fuzzy_search`
    /// tool) read this instead of calling `to_lowercase` per node per query, so
    /// case folding stays O(N) once per install rather than O(N) per request.
    pub fn node_lc(&self, i: usize) -> Option<(&str, &str, &str, &str)> {
        self.node_lc.get(i).map(|k| {
            (
                k.id.as_str(),
                k.full.as_str(),
                k.name.as_str(),
                k.label.as_str(),
            )
        })
    }

    /// Every edge, in graph insertion order (`/api/export.json`).
    pub fn edges(&self) -> &[Value] {
        &self.edges
    }

    /// `set_defines(defines=None, player=False)`.
    pub fn set_defines(&mut self, defines: Option<HashSet<String>>, player: bool) {
        self.player = player;
        self.defines = defines;
        self.defines_known = self.defines.is_some();
        let text = match &self.defines {
            Some(d) => {
                let mut v: Vec<&String> = d.iter().collect();
                v.sort();
                v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n")
            }
            None => "<all-branches>".to_string(),
        };
        self.defines_sig = sha256_hex(text.as_bytes()).chars().take(16).collect();
    }

    // -- node access --------------------------------------------------------
    fn node(&self, id: &str) -> Option<&Value> {
        self.node_ix.get(id).map(|&i| &self.nodes[i])
    }

    /// `node id -> node`, the shape `cs_doc.prepare` consumes.
    fn nodes_map(&self) -> &Map<String, Value> {
        &self.nodes_by_id
    }

    /// `lite(nid)` — the full node minus the source-viewer payload.
    pub fn lite(&self, nid: &str) -> Option<Value> {
        let n = self.node(nid)?;
        let mut obj = n.as_object()?.clone();
        for k in ["params", "signature", "head", "tail", "parts"] {
            obj.remove(k);
        }
        Some(Value::Object(obj))
    }

    // -- additive accessors for the MCP-only edit tools ---------------------
    //
    // These expose the graph bookkeeping that `crate::edit` needs without
    // duplicating it. They are pure reads: no existing payload, ordering or
    // clone-differential answer changes.

    /// Additive: the raw node object for `id`, if any.
    pub fn node_value(&self, id: &str) -> Option<&Value> {
        self.node(id)
    }

    /// Additive: whether `id` names a declaration (not a literal/external stub).
    pub fn is_declaration_id(&self, id: &str) -> bool {
        self.node(id).map(Index::is_declaration).unwrap_or(false)
    }

    /// Additive: the node ids the graph attributes to `file`, in build order.
    /// `file` may be given in either spelling (see [`Index::canon_file`]).
    pub fn node_ids_of_file(&self, file: &str) -> Vec<String> {
        self.nodes_by_file
            .get(&self.canon_file(file))
            .cloned()
            .unwrap_or_default()
    }

    /// Additive: the raw text of an indexed `file` (display path). `file` may be
    /// given in either spelling.
    pub fn file_text_of(&self, file: &str) -> Option<&str> {
        let key = self.canon_file(file);
        self.file_texts.get(&key).map(String::as_str)
    }

    /// Additive: every indexed display file path, sorted, so a scan is
    /// deterministic without depending on `HashMap` iteration order.
    pub fn file_paths(&self) -> Vec<String> {
        let mut v: Vec<String> = self.file_texts.keys().cloned().collect();
        v.sort();
        v
    }

    /// Additive: every edge touching `id`, in build order.
    pub fn edges_of(&self, id: &str) -> Vec<&Value> {
        self.adj_ix(id).iter().map(|&i| self.edge_obj(i)).collect()
    }

    /// Additive: the `(line, endLine, node id)` ranges the graph recorded for a
    /// display file, exactly as stored. `file` may be given in either spelling.
    pub fn node_ranges_of_file(&self, file: &str) -> Vec<(i64, i64, String)> {
        self.node_ranges
            .get(&self.canon_file(file))
            .cloned()
            .unwrap_or_default()
    }

    /// Additive: resolve an id, full name or label to a real node id.
    pub fn resolve_node_id(&self, raw: &str) -> Option<String> {
        self.resolve_source_id(raw)
    }

    /// Additive: the repo root this index was built for.
    pub fn repo_path(&self) -> &Path {
        &self.repo
    }

    // -- building -----------------------------------------------------------
    /// `display(path)` — repo-relative when inside the repo.
    pub fn display(&self, path: &Path) -> String {
        display_path(path, &self.repo)
    }

    /// Canonical form of a caller-supplied file path — the key every
    /// file-keyed map is built with. See [`canon_file_with`].
    pub fn canon_file(&self, file: &str) -> String {
        canon_file_with(file, &self.repo)
    }

    /// `cache_dir(roots=None)` — the first root that exists, else the repo.
    pub fn cache_dir(&self, roots: Option<&[String]>) -> PathBuf {
        let owned;
        let roots = match roots {
            Some(r) => r,
            None => {
                owned = self.roots.clone();
                &owned
            }
        };
        for r in roots {
            if r.is_empty() {
                continue;
            }
            let p = if Path::new(r).is_absolute() {
                PathBuf::from(r)
            } else {
                self.repo.join(r)
            };
            let p = normpath(&p);
            if p.is_dir() {
                return p;
            }
            if p.is_file() {
                return p.parent().map(Path::to_path_buf).unwrap_or(p);
            }
        }
        self.repo.clone()
    }

    /// `_signature(files, roots, excludes)`.
    fn signature(&self, files: &[String], roots: &[String], excludes: &[String]) -> String {
        let mut sorted_ex: Vec<&String> = excludes.iter().collect();
        sorted_ex.sort();
        let mut s = format!(
            "v{}|repo={}|{}|{}|defines={}|",
            DB_VERSION,
            normpath(&self.repo).to_string_lossy(),
            roots.join(","),
            sorted_ex
                .iter()
                .map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(","),
            self.defines_sig
        );
        for f in files {
            let Ok(st) = fs::metadata(f) else { continue };
            s.push_str(&format!("{}\0{}\0{}\n", f, st.len(), mtime_ns(&st)));
        }
        format!("v{}:{}", DB_VERSION, sha256_hex(s.as_bytes()))
    }

    /// `build(roots, excludes, cfg=None)` — discover, parse, graph, install.
    ///
    /// Cancellable: the host can flip [`Index::request_cancel`] from another
    /// thread while this runs. Every check is a single relaxed atomic load in a
    /// loop that already exists; a cancelled build restores the pre-build
    /// bookkeeping and returns the previous state with `cancelled: true`, never
    /// a partial install.
    pub fn build(&mut self, roots: &[String], excludes: &[String], cfg: &Config) -> Value {
        let t0 = Instant::now();
        let timing = std::env::var_os("CSGRAPH_BUILD_TIMING").is_some();
        // A new build starts un-cancelled; Stop is meaningful only while one is
        // actually running.
        self.cancel.store(false, Ordering::SeqCst);
        // Only an indexed graph needs preserving; a cold index has nothing to
        // restore (and cloning its empty bookkeeping would be pointless).
        let snapshot = if self.indexed {
            Some(self.snapshot_for_cancel())
        } else {
            None
        };
        self.cfg = cfg.clone();
        // Project model first: it is the attribution lookup `/api/projects` and
        // the UI filter read, and it must be present on a cache hit too.
        self.set_projects(discover_projects(&self.repo, roots, 3));
        if timing {
            eprintln!("[timing] projects {}ms", t0.elapsed().as_millis());
        }
        let mut excludes: Vec<String> = excludes.to_vec();
        let (syms, known) = resolve_defines(&self.repo, cfg, Some(roots));
        self.set_defines(if known { Some(syms) } else { None }, cfg.player);
        if timing {
            eprintln!("[timing] defines {}ms", t0.elapsed().as_millis());
        }
        if self.cancelled() {
            return self.finish_cancelled(snapshot);
        }
        // Never let a cache (or its journal) become an input to the index.
        for name in [
            DB_NAME.to_string(),
            format!("{DB_NAME}-wal"),
            format!("{DB_NAME}-shm"),
            CACHE_DIR.to_string(),
        ] {
            if !excludes.contains(&name) {
                excludes.push(name);
            }
        }
        // Hard bound on the recursive walk below. `discover_files` has no
        // ceiling of its own: a root that reaches a build tree or a vendored
        // SDK spends minutes there, which is over the owner's 30 s limit. The
        // pre-flight walk applies the same excludes and stops at a file-count
        // or wall-clock ceiling, so a pathological root is refused with a
        // clear error instead of hanging. A normal corpus (the default
        // `_sealed_src` is 52 files) is nowhere near the limits.
        let (max_files, max_secs) = walk_limits();
        if let Err(msg) = bounded_walk_check(&self.repo, roots, &excludes, max_files, max_secs) {
            return self.finish_bounds(snapshot, msg, max_files, max_secs);
        }
        if timing {
            eprintln!("[timing] guard {}ms", t0.elapsed().as_millis());
        }
        let files = discover_files(&self.repo, roots, &excludes);
        if timing {
            eprintln!("[timing] discover {}ms files={}", t0.elapsed().as_millis(), files.len());
        }
        let cache = Cache::open(&self.cache_dir(Some(roots)));
        self.cache_path = cache.dir().to_string_lossy().into_owned();
        let sig = self.signature(&files, roots, &excludes);
        if self.cancelled() {
            return self.finish_cancelled(snapshot);
        }

        if let Some(payload) = self.load_graph_cache(&cache, &sig) {
            // Exact same file set -> no parsing, no graph build, instant start.
            self.from_cache = true;
            let ms = t0.elapsed().as_millis() as i64;
            self.install(payload, roots, ms, true);
            return self.state();
        }

        let (parse, warnings) = match self.parse_files(&cache, &files) {
            Some(v) => v,
            None => return self.finish_cancelled(snapshot),
        };
        if timing {
            eprintln!("[timing] parse {}ms", t0.elapsed().as_millis());
        }
        if self.cancelled() {
            return self.finish_cancelled(snapshot);
        }
        let t_graph = Instant::now();
        let gb = build_graph(&parse);
        if timing {
            eprintln!("[timing] graph {}ms", t_graph.elapsed().as_millis());
        }
        let t_mat = Instant::now();
        if self.cancelled() {
            return self.finish_cancelled(snapshot);
        }
        let parse_ms = t0.elapsed().as_millis() as i64;

        // ---- nodes (field set and defaults exactly as Index.build) --------
        let mut nodes: Vec<Value> = Vec::with_capacity(gb.nodes.len());
        let mut node_ix: HashMap<String, usize> = HashMap::new();
        for (i, gnode) in gb.nodes.iter().enumerate() {
            // One atomic load per 1024 nodes: cancellable, and not a rescan.
            if i % 1024 == 0 && self.cancelled() {
                return self.finish_cancelled(snapshot);
            }
            let mut n = normalize_node(gnode);
            // The file form is settled in `install`
            // (`canonicalize_node_files`), which covers both a fresh parse and a
            // graph-cache payload; `n` is only `mut` for the degree fields below.
            let id = n
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            node_ix.insert(id, nodes.len());
            nodes.push(Value::Object(n));
        }

        // ---- edges, renumbered e0..eN ------------------------------------
        let mut edges: Vec<Value> = Vec::with_capacity(gb.edges.len());
        for (i, e) in gb.edges.iter().enumerate() {
            let mut e = e.clone();
            if let Some(obj) = e.as_object_mut() {
                obj.insert("id".to_string(), json!(format!("e{i}")));
            }
            edges.push(e);
        }

        // ---- degrees ------------------------------------------------------
        let mut indeg: HashMap<String, i64> = HashMap::new();
        let mut outdeg: HashMap<String, i64> = HashMap::new();
        let mut neigh: HashMap<String, HashSet<String>> = HashMap::new();
        for e in &edges {
            let s = e.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let t = e.get("target").and_then(|v| v.as_str()).unwrap_or("");
            *outdeg.entry(s.to_string()).or_insert(0) += 1;
            *indeg.entry(t.to_string()).or_insert(0) += 1;
            neigh.entry(s.to_string()).or_default().insert(t.to_string());
            neigh.entry(t.to_string()).or_default().insert(s.to_string());
        }
        for n in nodes.iter_mut() {
            let id = n
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let obj = n.as_object_mut().unwrap();
            obj.insert(
                "inDeg".to_string(),
                json!(indeg.get(&id).copied().unwrap_or(0)),
            );
            obj.insert(
                "outDeg".to_string(),
                json!(outdeg.get(&id).copied().unwrap_or(0)),
            );
            obj.insert(
                "degree".to_string(),
                json!(neigh.get(&id).map(|s| s.len()).unwrap_or(0) as i64),
            );
        }

        // ---- adjacent edges (insertion order matters for stable sorts) ----
        let mut adj: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in edges.iter().enumerate() {
            let s = e.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let t = e.get("target").and_then(|v| v.as_str()).unwrap_or("");
            adj.entry(s.to_string()).or_default().push(i);
            adj.entry(t.to_string()).or_default().push(i);
        }

        // ---- facet counts -------------------------------------------------
        let kinds_of = |what: &str| -> i64 {
            nodes
                .iter()
                .filter(|n| {
                    let k = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                    match what {
                        "methods" => matches!(k, "method" | "constructor" | "operator"),
                        "types" => {
                            matches!(k, "class" | "struct" | "interface" | "enum" | "record")
                        }
                        "fields" => matches!(k, "field" | "property" | "event"),
                        "strings" => k == "string",
                        "externals" => k == "external",
                        "namespaces" => k == "namespace",
                        _ => false,
                    }
                })
                .count() as i64
        };
        let (n_methods, n_types, n_fields, n_strings, n_ext, n_ns) = (
            kinds_of("methods"),
            kinds_of("types"),
            kinds_of("fields"),
            kinds_of("strings"),
            kinds_of("externals"),
            kinds_of("namespaces"),
        );
        let counts = json!({
            "nodes": nodes.len(),
            "edges": edges.len(),
            "methods": n_methods,
            "types": n_types,
            "fields": n_fields,
            "strings": n_strings,
            "externals": n_ext,
            "namespaces": n_ns,
        });

        // ---- original source text + declaration ranges --------------------
        let mut texts: HashMap<String, String> = HashMap::new();
        let mut warnings = warnings;
        for f in &files {
            // Reading hundreds of thousands of lines is I/O the Stop must beat.
            if self.cancelled() {
                return self.finish_cancelled(snapshot);
            }
            let rel = self.display(Path::new(f));
            match fs::read(f) {
                Ok(bytes) => {
                    texts.insert(rel, python_text(&bytes));
                }
                Err(exc) => warnings.push(self.attribute_warning(&rel, &exc.to_string())),
            }
        }
        let mut ranges: HashMap<String, Vec<(i64, i64, String)>> = HashMap::new();
        for n in &nodes {
            let file = n.get("file").and_then(|v| v.as_str()).unwrap_or("");
            let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if file.is_empty()
                || !matches!(
                    kind,
                    "method" | "constructor" | "operator" | "property" | "field" | "event"
                )
            {
                continue;
            }
            let line = n.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
            let end_line = n.get("endLine").and_then(|v| v.as_i64()).unwrap_or(0);
            let id = n
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            ranges
                .entry(file.to_string())
                .or_default()
                .push((line, if end_line != 0 { end_line } else { line }, id));
        }
        for rs in ranges.values_mut() {
            // (line, -(endLine - line)) — the widest span first on a tie.
            rs.sort_by(|a, b| (a.0, -(a.1 - a.0)).cmp(&(b.0, -(b.1 - b.0))));
        }

        if timing {
            eprintln!(
                "[timing] materialize {}ms nodes={} edges={}",
                t_mat.elapsed().as_millis(),
                nodes.len(),
                edges.len()
            );
        }
        let payload = GraphPayload {
            nodes,
            edges,
            counts,
            file_texts: texts,
            node_ranges: ranges,
            warnings: warnings.iter().take(200).cloned().collect(),
            file_count: files.len(),
        };
        if timing {
            eprintln!("[timing] payload {}ms", t_mat.elapsed().as_millis());
        }
        let t_store = Instant::now();
        self.store_graph_cache(&cache, &sig, &payload);
        self.from_cache = false;
        self.install(payload, roots, parse_ms, false);
        if timing {
            eprintln!("[timing] install {}ms", t_store.elapsed().as_millis());
        }
        let t_state = Instant::now();
        let state = self.state();
        if timing {
            eprintln!("[timing] state {}ms", t_state.elapsed().as_millis());
        }
        state
    }

    /// `_parse_files(db, files)` — reuse unchanged files, parse the rest.
    ///
    /// `None` means the Stop flag was seen between files: the caller abandons
    /// the build without installing anything.
    /// Parse every discovered file, in three phases so the expensive part is
    /// parallel without changing any observable output:
    ///
    /// 1. **serial plan** — stat + cache lookup per file (the `Cache` is not
    ///    `Sync`, and its writes are the only shared mutable state), with the
    ///    per-file `cancelled()` check exactly where the serial loop had it;
    /// 2. **parallel parse** — misses are parsed by
    ///    [`crate::parser::parse_files_parallel`] in bounded chunks, so a Stop
    ///    request is still honoured between chunks;
    /// 3. **serial merge** — cache writes, project-attributed warnings and the
    ///    output vector are rebuilt in original file order, so the payload is
    ///    identical to the serial loop's.
    fn parse_files(&mut self, cache: &Cache, files: &[String]) -> Option<(Vec<Value>, Vec<String>)> {
        self.cache_hits = 0;
        self.cache_misses = 0;
        let defines = self.defines.clone();
        let repo = self.repo.clone();

        enum Slot {
            Hit(Value),
            Miss { key: String, size: u64, mtime: f64 },
            Skipped,
        }

        // ---- phase 1: serial plan (the Cache is not Sync) ------------------
        let mut slots: Vec<Slot> = Vec::with_capacity(files.len());
        for f in files {
            if self.cancelled() {
                return None;
            }
            let Ok(st) = fs::metadata(f) else {
                slots.push(Slot::Skipped);
                continue;
            };
            let size = st.len();
            let mtime = mtime_secs(&st);
            let key = format!(
                "{}|{}",
                normpath(Path::new(f)).to_string_lossy(),
                self.defines_sig
            );
            let mut hit: Option<Value> = None;
            if let Some(blob) = cache.get("parse", &key) {
                if let Ok(ent) = serde_json::from_slice::<ParseEntry>(&blob) {
                    if ent.size == size && (ent.mtime - mtime).abs() < 1e-6 {
                        hit = Some(ent.parse);
                        self.cache_hits += 1;
                    }
                }
            }
            slots.push(match hit {
                Some(pr) => Slot::Hit(pr),
                None => Slot::Miss { key, size, mtime },
            });
        }

        // ---- phase 2: parallel parse of the misses -------------------------
        let mut parsed: Vec<Option<Value>> = vec![None; files.len()];
        for (i, s) in slots.iter().enumerate() {
            if let Slot::Hit(pr) = s {
                parsed[i] = Some(pr.clone());
            }
        }
        let miss_ix: Vec<usize> = slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| matches!(s, Slot::Miss { .. }).then_some(i))
            .collect();
        let cancel = self.cancel.clone();
        let mut parsed_misses: Vec<(usize, Value)> = Vec::with_capacity(miss_ix.len());
        for chunk in miss_ix.chunks(PARSE_CHUNK) {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let chunk_files: Vec<String> = chunk.iter().map(|&i| files[i].clone()).collect();
            let out = crate::parser::parse_files_parallel(&chunk_files, &repo, defines.as_ref());
            for (&i, pr) in chunk.iter().zip(out.into_iter()) {
                parsed_misses.push((i, pr));
            }
            self.cache_misses += chunk.len();
        }

        // ---- phase 3: serial merge, original order -------------------------
        for (i, pr) in parsed_misses {
            if let Slot::Miss { key, size, mtime } = &slots[i] {
                let ent = ParseEntry { size: *size, mtime: *mtime, parse: pr.clone() };
                if let Ok(bytes) = serde_json::to_vec(&ent) {
                    let _ = cache.put("parse", key, &bytes);
                }
            }
            parsed[i] = Some(pr);
        }
        let mut parse: Vec<Value> = Vec::with_capacity(files.len());
        let mut warnings: Vec<String> = Vec::new();
        for (i, f) in files.iter().enumerate() {
            let Some(pr) = parsed[i].take() else {
                // `stat` failed: the serial loop warned instead of parsing.
                warnings.push(format!("{f}: stat failed"));
                continue;
            };
            let display = self.display(Path::new(f));
            if let Some(ws) = pr.get("warnings").and_then(|v| v.as_array()) {
                for w in ws {
                    // Parser warnings are attributed to their file and, when a
                    // .csproj claims that file, to the project. Still parser
                    // warnings — there is no Roslyn here.
                    warnings.push(self.attribute_warning(&display, w.as_str().unwrap_or("")));
                }
            }
            parse.push(pr);
        }
        Some((parse, warnings))
    }

    fn load_graph_cache(&self, cache: &Cache, sig: &str) -> Option<GraphPayload> {
        let blob = cache.get("graph", sig)?;
        serde_json::from_slice(&blob).ok()
    }

    fn store_graph_cache(&self, cache: &Cache, sig: &str, payload: &GraphPayload) {
        if let Ok(bytes) = serde_json::to_vec(payload) {
            let _ = cache.put("graph", sig, &bytes);
        }
    }

    /// `_install(payload, roots, parse_ms, from_cache)`.
    fn install(
        &mut self,
        payload: GraphPayload,
        roots: &[String],
        parse_ms: i64,
        from_cache: bool,
    ) {
        let GraphPayload {
            mut nodes,
            edges,
            counts,
            file_texts,
            node_ranges,
            warnings,
            file_count,
        } = payload;
        // Settle the path form at install, so it holds for a fresh parse AND for
        // a graph-cache payload. `file_texts` keys are always built with the
        // current `display`; a parse/graph cached under a different `--repo`
        // spelling could carry the other form (gap 5).
        for n in nodes.iter_mut() {
            canonicalize_node_files(n, &self.repo);
        }
        self.node_ix.clear();
        for (i, n) in nodes.iter().enumerate() {
            if let Some(id) = n.get("id").and_then(|v| v.as_str()) {
                self.node_ix.insert(id.to_string(), i);
            }
        }
        self.adj.clear();
        for (i, e) in edges.iter().enumerate() {
            let s = e.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let t = e.get("target").and_then(|v| v.as_str()).unwrap_or("");
            self.adj.entry(s.to_string()).or_default().push(i);
            self.adj.entry(t.to_string()).or_default().push(i);
        }
        self.nodes = nodes;
        self.node_lc = self
            .nodes
            .iter()
            .map(|n| {
                let lc = |key: &str| {
                    n.get(key)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_lowercase()
                };
                NodeLc {
                    id: lc("id"),
                    full: lc("full"),
                    name: lc("name"),
                    label: lc("label"),
                    signature: lc("signature"),
                }
            })
            .collect();
        self.nodes_by_id = self
            .nodes
            .iter()
            .filter_map(|n| {
                n.get("id")
                    .and_then(|v| v.as_str())
                    .map(|id| (id.to_string(), n.clone()))
            })
            .collect();
        self.edges = edges;
        self.nodes_by_file = HashMap::new();
        for n in &self.nodes {
            let f = n.get("file").and_then(|v| v.as_str()).unwrap_or("");
            if f.is_empty() {
                continue;
            }
            if let Some(id) = n.get("id").and_then(|v| v.as_str()) {
                // Keyed by the canonical form: a query in either spelling
                // resolves, and `decls {file: <relative>}` matches on a
                // multi-segment root (gap 5).
                let key = canon_file_with(f, &self.repo);
                self.nodes_by_file
                    .entry(key)
                    .or_default()
                    .push(id.to_string());
            }
        }
        self.counts = counts;
        self.roots = roots.to_vec();
        self.file_count = file_count;
        self.parse_ms = parse_ms;
        self.warnings = warnings.into_iter().take(50).collect();
        self.file_texts = file_texts;
        // Re-key any cached ranges to the same canonical form (a payload
        // written before the rule settled can hold the other spelling).
        let mut rekeyed: HashMap<String, Vec<(i64, i64, String)>> = HashMap::new();
        for (k, v) in node_ranges {
            rekeyed
                .entry(canon_file_with(&k, &self.repo))
                .or_default()
                .extend(v);
        }
        self.node_ranges = rekeyed;
        // Graph-shaped work done once per install instead of once per query:
        // the sorted file order `text_hits` scans, and an ASCII-lowercased copy
        // of every file (plus its all-ASCII flag) for the case-insensitive fast
        // path. Both are derived from `file_texts` and discarded on the next
        // install.
        self.files_sorted = self.file_texts.keys().cloned().collect();
        self.files_sorted.sort();
        self.file_texts_lc.clear();
        self.file_texts_lc.reserve(self.file_texts.len());
        for (f, text) in &self.file_texts {
            self.file_texts_lc.insert(
                f.clone(),
                LcFile {
                    lower: text.to_ascii_lowercase(),
                    ascii: text.is_ascii(),
                },
            );
        }
        self.doc_cache.clear();
        self.doc_cache_order.clear();
        self.doc_models = DocCache::new();
        self.indexed = true;
        self.from_cache = from_cache;
    }

    // -- viewer state -------------------------------------------------------
    fn current_cache(&mut self) -> &Cache {
        if self.cache.is_none() {
            let c = Cache::open(&self.cache_dir(None));
            self.cache_path = c.dir().to_string_lossy().into_owned();
            self.cache = Some(c);
        }
        self.cache.as_ref().unwrap()
    }

    /// `viewer_state()`.
    pub fn viewer_state(&self) -> Value {
        let owned;
        let cache = match &self.cache {
            Some(c) => c,
            None => {
                owned = Cache::open(&self.cache_dir(None));
                &owned
            }
        };
        cache.viewer_state().unwrap_or_else(|| json!({}))
    }

    /// `save_viewer_state(obj)`.
    pub fn save_viewer_state(&mut self, obj: &Value) -> bool {
        let cache = self.current_cache();
        cache.save_viewer_state(obj).is_ok()
    }

    // -- live viewer POV (selection + focus) --------------------------------

    /// Candidate `viewer.json` paths this engine can read itself: every
    /// configured/current root's cache dir plus the repo. Used when instance
    /// discovery yields no state path (an argument-less GUI launch) and to read
    /// a stale snapshot honestly when no viewer is running.
    fn viewer_state_paths(&self) -> Vec<PathBuf> {
        let mut roots: Vec<String> = Vec::new();
        roots.extend(self.roots.iter().cloned());
        roots.extend(self.cfg.roots.iter().cloned());
        let mut dirs: Vec<PathBuf> = Vec::new();
        for r in &roots {
            let d = self.cache_dir(Some(std::slice::from_ref(r)));
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
        let d = self.cache_dir(None);
        if !dirs.contains(&d) {
            dirs.push(d);
        }
        if !dirs.contains(&self.repo) {
            dirs.push(self.repo.clone());
        }
        dirs.iter().map(|d| crate::registry::state_path_for(d)).collect()
    }

    /// `GET /api/viewer_instances` — enumerate every discovered cs viewer
    /// instance (live *and* stale, each labelled) and auto-select one.
    ///
    /// Auto-selection is deliberately conservative: exactly one live instance
    /// selects itself; zero live instances is an honest empty result; several
    /// live instances are reported as `ambiguous` with the candidate list and
    /// `active: null`, never a guess.
    pub fn viewer_instances(&self, selector: Option<&str>) -> Value {
        let (instances, discovery) = crate::registry::discover(&self.repo_str);
        let live: Vec<&crate::registry::Instance> =
            instances.iter().filter(|i| i.alive).collect();
        let (active, ambiguous, matched_by, note) = select_instance(&live, selector);
        json!({
            "ok": true,
            "repo": self.repo_str,
            "discovery": discovery,
            "instances": instances.iter().map(|i| i.to_json()).collect::<Vec<_>>(),
            "liveCount": live.len(),
            "active": active.map(|i| i.to_json()),
            "ambiguous": ambiguous,
            "selectedBy": matched_by,
            "staleAfterMs": crate::registry::STALE_AFTER_MS,
            "note": note,
        })
    }

    /// `GET /api/viewer_pov` — the running viewer's POV: Monaco text selection,
    /// graph box (marquee) selection, and the loose IDE context, plus honest
    /// freshness metadata.
    ///
    /// Cross-process: the viewer publishes `pov` inside its `viewer.json`
    /// ([`Index::save_viewer_state`]); this reads the file of the selected
    /// instance (or, when there is no live instance, the freshest candidate file
    /// in this engine's own scope) and never fabricates a field.
    pub fn viewer_pov(&self, selector: Option<&str>) -> Value {
        let now = crate::registry::now_ms();
        let (instances, discovery) = crate::registry::discover(&self.repo_str);
        let live: Vec<&crate::registry::Instance> =
            instances.iter().filter(|i| i.alive).collect();
        let (active, ambiguous, matched_by, note) = select_instance(&live, selector);

        // An ambiguous/empty selection must not silently read somebody else's
        // file: when a selector was given or auto-selection is ambiguous, stop
        // with `pov: null` and the candidates.
        let refused = ambiguous || (selector.map(|s| !s.trim().is_empty()).unwrap_or(false) && active.is_none());

        let mut chosen: Option<(String, Value, i64, Option<bool>, &'static str)> = None;
        if !refused {
            if let Some(inst) = active {
                if !inst.state_path.is_empty() {
                    if let Some((v, m)) =
                        crate::registry::read_state_file(Path::new(&inst.state_path))
                    {
                        chosen = Some((inst.state_path.clone(), v, m, Some(true), "instance"));
                    }
                }
            }
            if chosen.is_none() {
                // No live instance at all: a stale local snapshot is still
                // useful, and is labelled stale below. If a live instance exists
                // but knows no state path, the same local read is the only
                // honest option (publisherAlive stays unknown).
                let publisher_alive = if live.is_empty() { Some(false) } else { None };
                let mut best: Option<(String, Value, i64)> = None;
                for p in self.viewer_state_paths() {
                    if let Some((v, m)) = crate::registry::read_state_file(&p) {
                        if best.as_ref().map(|(_, _, bm)| m > *bm).unwrap_or(true) {
                            best = Some((p.to_string_lossy().into_owned(), v, m));
                        }
                    }
                }
                if let Some((p, v, m)) = best {
                    chosen = Some((p, v, m, publisher_alive, "local"));
                }
            }
        }

        let (state_path, pov, tabs, selected, at, mtime, live_flag, stale, publisher_alive, source) =
            match chosen {
                Some((path, value, mtime, publisher_alive, source)) => {
                    let pov = value.get("pov").cloned().filter(|v| !v.is_null());
                    let tabs = value.get("tabs").cloned().filter(|v| !v.is_null());
                    let selected = value.get("selected").cloned().filter(|v| !v.is_null());
                    let at = pov
                        .as_ref()
                        .and_then(|p| p.get("at"))
                        .and_then(Value::as_i64);
                    let age = at.map(|t| (now - t).max(0)).unwrap_or_else(|| (now - mtime).max(0));
                    let is_live = age <= crate::registry::STALE_AFTER_MS;
                    (
                        Some(path),
                        pov,
                        tabs,
                        selected,
                        at,
                        Some(mtime),
                        is_live,
                        !is_live,
                        publisher_alive,
                        Some(source),
                    )
                }
                None => (None, None, None, None, None, None, false, true, None, None),
            };

        let note = if refused {
            note
        } else if pov.is_none() {
            "no published POV in the viewer state file (the viewer may predate this feature, or nothing is selected yet)"
        } else {
            note
        };

        json!({
            "ok": true,
            "repo": self.repo_str,
            "discovery": discovery,
            "instances": instances.iter().map(|i| i.to_json()).collect::<Vec<_>>(),
            "liveCount": live.len(),
            "active": active.map(|i| i.to_json()),
            "ambiguous": ambiguous,
            "selectedBy": matched_by,
            "statePath": state_path,
            "source": source,
            "mtimeMs": mtime,
            "at": at,
            "ageMs": at
                .map(|t| (now - t).max(0))
                .or_else(|| mtime.map(|m| (now - m).max(0))),
            "staleAfterMs": crate::registry::STALE_AFTER_MS,
            "live": live_flag,
            "stale": stale,
            "publisherAlive": publisher_alive,
            "pov": pov,
            "tabs": tabs,
            "selected": selected,
            "note": note,
        })
    }

    // -- reads --------------------------------------------------------------
    /// `state()`.
    pub fn state(&self) -> Value {
        let first = self.roots.first().cloned().unwrap_or_default();
        let mut root = first;
        if !root.is_empty() && !Path::new(&root).is_absolute() {
            root = normpath(&self.repo.join(&root))
                .to_string_lossy()
                .into_owned();
        }
        if root.is_empty() {
            root = self.repo_str.clone();
        }
        let mut symbols: Vec<String> = self
            .defines
            .as_ref()
            .map(|d| d.iter().cloned().collect())
            .unwrap_or_default();
        symbols.sort();
        json!({
            "ok": true,
            "indexed": self.indexed,
            "root": root,
            "repo": self.repo_str,
            "roots": self.roots,
            "fileCount": self.file_count,
            "parseMs": self.parse_ms,
            "counts": self.counts,
            "warnings": self.warnings,
            "cache": {
                "db": self.cache_path,
                "fromCache": self.from_cache,
                "hits": self.cache_hits,
                "misses": self.cache_misses,
            },
            // Peak-load visibility: how much document synthesis is being reused.
            "docCache": self.doc_cache_stats(),
            "defines": {
                "known": self.defines_known,
                "player": self.player,
                "count": self.defines.as_ref().map(|d| d.len()).unwrap_or(0),
                "symbols": symbols,
            },
            "viewer": self.viewer_state(),
        })
    }

    /// `search(q, limit=30)`.
    pub fn search(&self, q: &str, limit: i64) -> Vec<Value> {
        let ql = q.to_lowercase();
        if ql.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(i64, i64, String)> = Vec::new();
        debug_assert_eq!(self.nodes.len(), self.node_lc.len());
        for (n, k) in self.nodes.iter().zip(self.node_lc.iter()) {
            if k.full.contains(&ql) || k.signature.contains(&ql) {
                let rank = if k.name == ql {
                    -1
                } else if k.name.starts_with(&ql) {
                    0
                } else if k.name.contains(&ql) {
                    1
                } else {
                    2
                };
                let degree = n.get("degree").and_then(|v| v.as_i64()).unwrap_or(0);
                let nid = n.get("id").and_then(|v| v.as_str()).unwrap_or("");
                scored.push((rank, -degree, nid.to_string()));
            }
        }
        scored.sort();
        let keep = py_slice_len(scored.len(), limit);
        scored.truncate(keep);
        scored
            .into_iter()
            .filter_map(|(_, _, nid)| self.lite(&nid))
            .collect()
    }

    fn adj_ix(&self, nid: &str) -> &[usize] {
        self.adj.get(nid).map(Vec::as_slice).unwrap_or(&[])
    }

    fn edge_obj(&self, i: usize) -> &Value {
        &self.edges[i]
    }

    fn degree_of(&self, nid: &str) -> i64 {
        self.node(nid)
            .and_then(|n| n.get("degree"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    }

    /// `subgraph(focus=None, depth=1, max_nodes=250)`.
    pub fn subgraph(&self, focus: Option<&str>, depth: i64, max_nodes: i64) -> Value {
        if !self.indexed {
            return json!({"ok": true, "focus": focus, "truncated": false,
                          "nodes": [], "edges": []});
        }
        let focus = match focus {
            Some(f) if !f.is_empty() && self.node_ix.contains_key(f) => Some(f.to_string()),
            _ => None,
        };
        let mut keep_set: HashSet<String> = HashSet::new();
        let mut ordered: Vec<String> = Vec::new();
        let mut truncated = false;
        let out_focus;
        if let Some(f) = &focus {
            out_focus = Some(f.clone());
            let mut seen: HashSet<String> = HashSet::new();
            seen.insert(f.clone());
            let mut frontier: Vec<String> = vec![f.clone()];
            let mut chosen: Vec<String> = Vec::new();
            let mut chosen_set: HashSet<String> = HashSet::new();
            let rounds = depth.clamp(0, 9_999_999);
            for _ in 0..rounds {
                let mut nxt: Vec<String> = Vec::new();
                for nid in &frontier {
                    for &ei in self.adj_ix(nid) {
                        let e = self.edge_obj(ei);
                        let s = e.get("source").and_then(|v| v.as_str()).unwrap_or("");
                        let t = e.get("target").and_then(|v| v.as_str()).unwrap_or("");
                        let other = if s == nid { t } else { s };
                        if !seen.contains(other) {
                            seen.insert(other.to_string());
                            nxt.push(other.to_string());
                        }
                    }
                }
                for n in &nxt {
                    if chosen_set.insert(n.clone()) {
                        chosen.push(n.clone());
                    }
                }
                frontier = nxt;
                if seen.len() as i64 >= max_nodes {
                    break;
                }
            }
            // Python builds a set; keep a deterministic node order instead.
            keep_set.insert(f.clone());
            for c in &chosen {
                keep_set.insert(c.clone());
            }
            if keep_set.len() as i64 > max_nodes {
                let mut sorted: Vec<String> = keep_set.iter().cloned().collect();
                sorted.sort_by_key(|x| std::cmp::Reverse(self.degree_of(x)));
                let take = py_slice_len(sorted.len(), max_nodes);
                sorted.truncate(take);
                keep_set = sorted.into_iter().collect();
                keep_set.insert(f.clone());
                truncated = true;
            }
            for n in &self.nodes {
                if let Some(id) = n.get("id").and_then(|v| v.as_str()) {
                    if keep_set.contains(id) {
                        ordered.push(id.to_string());
                    }
                }
            }
        } else {
            out_focus = None;
            let mut order: Vec<(i64, String)> = self
                .nodes
                .iter()
                .map(|n| {
                    (
                        n.get("degree").and_then(|v| v.as_i64()).unwrap_or(0),
                        n.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                    )
                })
                .collect();
            order.sort_by_key(|(d, _)| std::cmp::Reverse(*d));
            truncated = order.len() as i64 > max_nodes;
            let take = py_slice_len(order.len(), max_nodes);
            order.truncate(take);
            for (_, id) in &order {
                keep_set.insert(id.clone());
            }
            ordered = order.into_iter().map(|(_, id)| id).collect();
        }
        let edges: Vec<Value> = self
            .edges
            .iter()
            .filter(|e| {
                let s = e.get("source").and_then(|v| v.as_str()).unwrap_or("");
                let t = e.get("target").and_then(|v| v.as_str()).unwrap_or("");
                keep_set.contains(s) && keep_set.contains(t)
            })
            .cloned()
            .collect();
        json!({
            "ok": true,
            "focus": out_focus,
            "truncated": truncated,
            "nodes": ordered.iter().filter_map(|x| self.lite(x)).collect::<Vec<Value>>(),
            "edges": edges,
        })
    }

    /// `RELATIONS`.
    const RELATIONS: [(&'static str, &'static str); 12] = [
        ("calls:out", "Calling"),
        ("calls:in", "Called by"),
        ("uses:out", "Uses"),
        ("uses:in", "Used by"),
        ("assigns:out", "Assigns"),
        ("assigns:in", "Assigned by"),
        ("constructs:out", "Constructs"),
        ("constructs:in", "Constructed by"),
        ("inherits:out", "Inherits"),
        ("inherits:in", "Inherited by"),
        ("contains:out", "Children"),
        ("contains:in", "Parent"),
    ];

    const RELATION_ORDER: [&'static str; 11] = [
        "calls:out",
        "calls:in",
        "uses:out",
        "uses:in",
        "assigns:out",
        "constructs:out",
        "constructs:in",
        "inherits:out",
        "inherits:in",
        "contains:out",
        "contains:in",
    ];

    fn relation_label(key: &str) -> String {
        for (k, label) in Self::RELATIONS {
            if k == key {
                return label.to_string();
            }
        }
        let (kind, _, dir) = split3(key);
        format!("{kind} {dir}")
    }

    /// `node_relations(nid, limit=300)`.
    pub fn node_relations(&self, nid: &str, limit: usize) -> Vec<Value> {
        struct Group {
            key: String,
            kind: String,
            dir: String,
            label: String,
            count: i64,
            nodes: Vec<Value>,
            truncated: bool,
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut where_is: HashMap<String, usize> = HashMap::new();
        for &ei in self.adj_ix(nid) {
            let e = self.edge_obj(ei);
            let s = e.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let t = e.get("target").and_then(|v| v.as_str()).unwrap_or("");
            let (other, direction) = if s == nid {
                (t, "out")
            } else if t == nid {
                (s, "in")
            } else {
                continue;
            };
            if self.node(other).is_none() {
                continue;
            }
            let kind = e
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let gkey = format!("{kind}:{direction}");
            let gi = match where_is.get(&gkey) {
                Some(&gi) => gi,
                None => {
                    let label = Self::relation_label(&gkey);
                    groups.push(Group {
                        key: gkey.clone(),
                        kind: kind.clone(),
                        dir: direction.to_string(),
                        label,
                        count: 0,
                        nodes: Vec::new(),
                        truncated: false,
                    });
                    where_is.insert(gkey.clone(), groups.len() - 1);
                    groups.len() - 1
                }
            };
            let g = &mut groups[gi];
            g.count += 1;
            if g.nodes.len() < limit {
                g.nodes.push(self.lite(other).unwrap());
            } else {
                g.truncated = true;
            }
        }
        let idx_of = |key: &str| -> i64 {
            Self::RELATION_ORDER
                .iter()
                .position(|k| *k == key)
                .map(|p| p as i64)
                .unwrap_or(99)
        };
        groups.sort_by(|a, b| (idx_of(&a.key), -a.count).cmp(&(idx_of(&b.key), -b.count)));
        for g in groups.iter_mut() {
            g.nodes.sort_by(|a, b| {
                let da = a.get("degree").and_then(|v| v.as_i64()).unwrap_or(0);
                let db = b.get("degree").and_then(|v| v.as_i64()).unwrap_or(0);
                let fa = a.get("full").and_then(|v| v.as_str()).unwrap_or("");
                let fb = b.get("full").and_then(|v| v.as_str()).unwrap_or("");
                (-da, fa).cmp(&(-db, fb))
            });
        }
        groups
            .into_iter()
            .map(|g| {
                json!({
                    "key": g.key, "kind": g.kind, "dir": g.dir, "label": g.label,
                    "count": g.count, "nodes": g.nodes, "truncated": g.truncated,
                })
            })
            .collect()
    }

    /// `relation_page(nid, key, offset=0, limit=200)`.
    pub fn relation_page(&self, nid: &str, key: &str, offset: i64, limit: i64) -> Option<Value> {
        let (kind, _, direction) = split3(key);
        if kind.is_empty()
            || (direction != "in" && direction != "out")
            || !self.node_ix.contains_key(nid)
        {
            return None;
        }
        let mut items: Vec<Value> = Vec::new();
        for &ei in self.adj_ix(nid) {
            let e = self.edge_obj(ei);
            if e.get("kind").and_then(|v| v.as_str()).unwrap_or("") != kind {
                continue;
            }
            let s = e.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let t = e.get("target").and_then(|v| v.as_str()).unwrap_or("");
            let other = if direction == "out" {
                if s != nid {
                    continue;
                }
                t
            } else {
                if t != nid {
                    continue;
                }
                s
            };
            if self.node(other).is_some() {
                items.push(self.lite(other).unwrap());
            }
        }
        items.sort_by(|a, b| {
            let da = a.get("degree").and_then(|v| v.as_i64()).unwrap_or(0);
            let db = b.get("degree").and_then(|v| v.as_i64()).unwrap_or(0);
            let fa = a.get("full").and_then(|v| v.as_str()).unwrap_or("");
            let fb = b.get("full").and_then(|v| v.as_str()).unwrap_or("");
            (-da, fa).cmp(&(-db, fb))
        });
        let offset = offset.max(0);
        let limit = limit.clamp(1, 2000);
        let total = items.len() as i64;
        let start = offset.min(total) as usize;
        let end = (offset + limit).min(total) as usize;
        Some(json!({
            "ok": true, "key": key, "total": total, "offset": offset,
            "nodes": items[start..end].to_vec(),
        }))
    }

    /// `node_detail(nid)`.
    pub fn node_detail(&self, nid: &str) -> Option<Value> {
        let n = self.node(nid)?;
        let mut callers: Vec<Value> = Vec::new();
        let mut callees: Vec<Value> = Vec::new();
        let mut uses: Vec<Value> = Vec::new();
        let mut used_by: Vec<Value> = Vec::new();
        for &ei in self.adj_ix(nid) {
            let e = self.edge_obj(ei);
            let s = e.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let t = e.get("target").and_then(|v| v.as_str()).unwrap_or("");
            let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            let count = e.get("count").cloned().unwrap_or_else(|| json!(0));
            let lines = e.get("lines").cloned().unwrap_or_else(|| json!([]));
            if s == nid {
                let item = json!({"node": self.lite(t), "kind": kind,
                                  "count": count, "lines": lines});
                if matches!(kind, "uses" | "assigns") {
                    uses.push(item);
                } else {
                    callees.push(item);
                }
            } else if t == nid {
                let item = json!({"node": self.lite(s), "kind": kind,
                                  "count": count, "lines": lines});
                if matches!(kind, "uses" | "assigns") {
                    used_by.push(item);
                } else {
                    callers.push(item);
                }
            }
        }
        let sort_key = |v: &mut Vec<Value>| {
            v.sort_by(|a, b| {
                let ca = a.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
                let cb = b.get("count").and_then(|v| v.as_i64()).unwrap_or(0);
                let la = a
                    .get("node")
                    .and_then(|n| n.get("label"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let lb = b
                    .get("node")
                    .and_then(|n| n.get("label"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                (-ca, la).cmp(&(-cb, lb))
            });
        };
        sort_key(&mut callers);
        sort_key(&mut callees);
        sort_key(&mut uses);
        sort_key(&mut used_by);
        Some(json!({
            "ok": true, "node": n,
            "callers": callers, "callees": callees,
            "uses": uses, "usedBy": used_by,
            "relations": self.node_relations(nid, 300),
        }))
    }

    // -- code lookup --------------------------------------------------------
    /// `enclosing(file, line)` — smallest member range containing the line.
    /// `file` is canonicalised first, so `text_hits`' display key and a node's
    /// absolute `file` both resolve to the same ranges (gap 5).
    pub fn enclosing(&self, file: &str, line: i64) -> Option<String> {
        let key = self.canon_file(file);
        let mut best: Option<String> = None;
        let mut best_span: Option<i64> = None;
        if let Some(rs) = self.node_ranges.get(&key) {
            for (a, b, nid) in rs {
                if *a <= line && line <= *b {
                    let span = b - a;
                    if best_span.is_none() || span < best_span.unwrap() {
                        best = Some(nid.clone());
                        best_span = Some(span);
                    }
                }
            }
        }
        best
    }

    /// Whether `q` is the bare-word form: first char ASCII alphabetic or `_`,
    /// the rest alphanumeric (Unicode-aware, matching Python `\w`) or `_`. A
    /// bare query gets `\b` boundaries in [`Index::hit_regex`].
    fn is_bare(q: &str) -> bool {
        let mut chars = q.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                // Python `\w` is Unicode-aware after the ASCII first char.
                chars.all(|c| c.is_alphanumeric() || c == '_')
            }
            _ => false,
        }
    }

    fn hit_regex(q: &str) -> regex::Regex {
        let pat = if Self::is_bare(q) {
            format!(r"(?i)\b{}\b", regex::escape(q))
        } else {
            format!("(?i){}", regex::escape(q))
        };
        regex::Regex::new(&pat).unwrap_or_else(|_| regex::Regex::new("$^").unwrap())
    }

    /// Leftmost byte offset where `ql` occurs in `hay` with word boundaries on
    /// both sides.
    ///
    /// For ASCII `hay`/`ql` this is exactly the position set of the
    /// `(?i)\bql\b` regex: case folding preserves which bytes are word bytes, so
    /// a boundary can be tested on the lowercased copy. Both arguments must be
    /// ASCII (the caller checks); non-ASCII keeps the regex path.
    fn find_bare_ci(hay: &str, ql: &str) -> Option<usize> {
        let hb = hay.as_bytes();
        let mut from = 0usize;
        while from <= hb.len() {
            let Some(rel) = hay[from..].find(ql) else { return None };
            let start = from + rel;
            let end = start + ql.len();
            let before = start == 0 || !is_word_byte(hb[start - 1]);
            let after = end == hb.len() || !is_word_byte(hb[end]);
            if before && after {
                return Some(start);
            }
            from = start + 1;
        }
        None
    }

    /// `text_hits(q, limit=400)`.
    ///
    /// Two things happen once per install instead of once per call: the sorted
    /// file order ([`Index::files_sorted`]) and an ASCII-lowercased copy of every
    /// file ([`Index::file_texts_lc`]). For an ASCII query in an ASCII line the
    /// `(?i)` literal regex is exactly a case-insensitive substring search, so the
    /// per-line scan uses `str::find` and never touches the regex engine; when the
    /// lowercased text has no occurrence at all, the whole file is skipped (ASCII)
    /// or only its non-ASCII lines are regex-checked (mixed). A non-ASCII query
    /// keeps the original regex path. Payloads are byte-identical either way.
    pub fn text_hits(&self, q: &str, limit: i64) -> (Vec<Value>, bool) {
        let q = q.trim();
        if q.is_empty() {
            return (Vec::new(), false);
        }
        let fast_q: Option<(bool, String)> = if q.is_ascii() {
            Some((Self::is_bare(q), q.to_ascii_lowercase()))
        } else {
            None
        };
        // Compiled lazily: the fast path never needs it, the first non-ASCII
        // file (or the first non-ASCII query) does.
        let mut pat: Option<regex::Regex> = None;
        let mut out: Vec<Value> = Vec::new();
        for f in &self.files_sorted {
            let text = &self.file_texts[f];
            let lc = fast_q
                .as_ref()
                .and_then(|_| self.file_texts_lc.get(f));
            if let (Some((bare, ql)), Some(lc)) = (fast_q.as_ref(), lc) {
                let (bare, ql) = (*bare, ql.as_str());
                // If the ASCII-lowercased text has no occurrence of the literal,
                // no ASCII line can match: a fully-ASCII file is done, and a
                // mixed file can still match only through Unicode case folding on
                // a non-ASCII line, which then runs the exact regex. This keeps
                // the prefilter sound (the ASCII lines are provably clear) while
                // skipping the line walk for rare and absent terms.
                if !lc.lower.contains(ql) {
                    if lc.ascii {
                        continue;
                    }
                    for (i, line) in text.split('\n').enumerate() {
                        if line.is_ascii() {
                            continue;
                        }
                        let i = i as i64 + 1;
                        let Some(m) = pat.get_or_insert_with(|| Self::hit_regex(q)).find(line) else {
                            continue;
                        };
                        let col = line[..m.start()].chars().count() as i64 + 1;
                        let nid = self.enclosing(f, i);
                        out.push(json!({
                            "file": f, "line": i, "col": col,
                            "text": line.trim().chars().take(400).collect::<String>(),
                            "node": nid.as_deref().and_then(|x| self.lite(x)),
                        }));
                        if out.len() as i64 >= limit {
                            return (out, true);
                        }
                    }
                    continue;
                }
                let mut orig = text.split('\n');
                let mut lower = lc.lower.split('\n');
                let mut i = 1i64;
                while let (Some(line), Some(ll)) = (orig.next(), lower.next()) {
                    let ascii = line.is_ascii();
                    let start = if ascii {
                        if bare {
                            Self::find_bare_ci(ll, ql)
                        } else {
                            ll.find(ql)
                        }
                    } else {
                        pat.get_or_insert_with(|| Self::hit_regex(q))
                            .find(line)
                            .map(|m| m.start())
                    };
                    if let Some(start) = start {
                        let col = if ascii {
                            start as i64 + 1
                        } else {
                            line[..start].chars().count() as i64 + 1
                        };
                        let nid = self.enclosing(f, i);
                        out.push(json!({
                            "file": f, "line": i, "col": col,
                            "text": line.trim().chars().take(400).collect::<String>(),
                            "node": nid.as_deref().and_then(|x| self.lite(x)),
                        }));
                        if out.len() as i64 >= limit {
                            return (out, true);
                        }
                    }
                    i += 1;
                }
            } else {
                let pat = pat.get_or_insert_with(|| Self::hit_regex(q));
                for (i, line) in text.split('\n').enumerate() {
                    let i = i as i64 + 1;
                    let Some(m) = pat.find(line) else { continue };
                    let col = line[..m.start()].chars().count() as i64 + 1;
                    let nid = self.enclosing(f, i);
                    out.push(json!({
                        "file": f, "line": i, "col": col,
                        "text": line.trim().chars().take(400).collect::<String>(),
                        "node": nid.as_deref().and_then(|x| self.lite(x)),
                    }));
                    if out.len() as i64 >= limit {
                        return (out, true);
                    }
                }
            }
        }
        (out, false)
    }

    /// `find(q, limit=400)`.
    pub fn find(&self, q: &str, limit: i64) -> Value {
        let q = q.trim();
        if q.is_empty() {
            return json!({"ok": true, "query": q, "definitions": [],
                          "implementations": [], "usages": [], "truncated": false});
        }
        let ql = q.to_lowercase();
        let bare = q.split('(').next().unwrap_or("").trim();
        let simple = bare.rsplit('.').next().unwrap_or("").trim();
        let sl = simple.to_lowercase();
        let has_dot = bare.contains('.');
        let member_kinds = [
            "method",
            "constructor",
            "operator",
            "property",
            "field",
            "event",
        ];
        let type_kinds = ["class", "struct", "interface", "enum", "record"];

        let mut defs: Vec<Value> = Vec::new();
        let mut impls: Vec<Value> = Vec::new();
        debug_assert_eq!(self.nodes.len(), self.node_lc.len());
        let dot_suffix = format!(".{ql}");
        for (n, k) in self.nodes.iter().zip(self.node_lc.iter()) {
            let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if k.full == ql
                || k.id == ql
                || (has_dot && k.full.ends_with(&dot_suffix))
                || (!has_dot && k.name == sl)
            {
                defs.push(self.find_row(n, "definition"));
            } else if k.name == sl
                && (member_kinds.contains(&kind) || type_kinds.contains(&kind))
            {
                impls.push(self.find_row(n, "implementation"));
            }
        }
        fn rank(rows: &mut Vec<Value>) {
            rows.sort_by(|a, b| {
                let ka = a.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                let kb = b.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                let ma = !matches!(ka, "method" | "constructor" | "operator");
                let mb = !matches!(kb, "method" | "constructor" | "operator");
                let da = a.get("degree").and_then(|v| v.as_i64()).unwrap_or(0);
                let db = b.get("degree").and_then(|v| v.as_i64()).unwrap_or(0);
                let fa = a.get("full").and_then(|v| v.as_str()).unwrap_or("");
                let fb = b.get("full").and_then(|v| v.as_str()).unwrap_or("");
                (ma, -da, fa).cmp(&(mb, -db, fb))
            });
            rows.truncate(rows.len().min(80));
        }
        rank(&mut defs);
        rank(&mut impls);

        let mut hits: Vec<Value> = Vec::new();
        let mut seen: HashSet<(String, i64)> = HashSet::new();
        let truncated;
        if has_dot {
            let (exact, t1) = self.text_hits(q, limit);
            let mut trunc = t1;
            for mut h in exact {
                if let Some(o) = h.as_object_mut() {
                    o.insert("match".to_string(), json!("exact"));
                }
                if let (Some(f), Some(l)) = (
                    h.get("file").and_then(|v| v.as_str()).map(str::to_string),
                    h.get("line").and_then(|v| v.as_i64()),
                ) {
                    seen.insert((f, l));
                }
                hits.push(h);
            }
            if (hits.len() as i64) < limit {
                let (more, t2) = self.text_hits(simple, limit - hits.len() as i64);
                trunc = trunc || t2;
                for mut h in more {
                    let key = (
                        h.get("file")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        h.get("line").and_then(|v| v.as_i64()).unwrap_or(0),
                    );
                    if seen.contains(&key) {
                        continue;
                    }
                    if let Some(o) = h.as_object_mut() {
                        o.insert("match".to_string(), json!("simple"));
                    }
                    hits.push(h);
                }
            }
            truncated = trunc;
        } else {
            let (mut h, t) = self.text_hits(q, limit);
            truncated = t;
            for h in h.iter_mut() {
                if let Some(o) = h.as_object_mut() {
                    o.insert("match".to_string(), json!("exact"));
                }
            }
            hits = h;
        }
        for h in hits.iter_mut() {
            if let Some(o) = h.as_object_mut() {
                o.insert("kind".to_string(), json!("usage"));
            }
        }
        json!({
            "ok": true, "query": q, "simple": simple,
            "definitions": defs,
            "implementations": impls,
            "usages": hits,
            "truncated": truncated,
        })
    }

    fn find_row(&self, n: &Value, kind: &str) -> Value {
        let id = n.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let mut row = self.lite(id).unwrap_or_else(|| json!({}));
        if let Some(o) = row.as_object_mut() {
            o.insert(
                "kind".to_string(),
                n.get("kind").cloned().unwrap_or_else(|| json!("")),
            );
            o.insert("match".to_string(), json!(kind));
        }
        row
    }

    fn file_flags(&self, file: &str) -> Option<Vec<bool>> {
        let text = self.file_texts.get(file)?;
        if !self.defines_known {
            return None;
        }
        Some(active_line_flags(
            &mask_noncode(&strip_bom(&to_chars(text))),
            self.defines.as_ref(),
        ))
    }

    // -- doc synthesis ------------------------------------------------------
    /// `doc_source(node_id)` — one synthesized document for a node.
    ///
    /// Synthesized once per node and then served from [`Index::doc_cache`] as a
    /// shared `Arc`: at peak, opening the same partial type again costs a
    /// pointer copy, not a re-synthesis and not a deep clone of a document that
    /// can be tens of thousands of lines. The file models the synthesis reads
    /// come from [`Index::doc_models`], which lives as long as the index, so a
    /// file is lexed once for every document that contains it.
    pub fn doc_source(&mut self, node_id: &str) -> Option<Arc<Value>> {
        if !self.indexed || node_id.is_empty() {
            return None;
        }
        // An external stub has no declaration site and no file, so building its
        // own doc yields nothing and the raw fallback yields "file not indexed".
        // Resolve it to the declared node it stands for first: the graph carries
        // both `NZK` (external, from a bare reference) and `#ns:NZK` (the
        // namespace), and "view code" on either should show the same document.
        let node_id = self.doc_alias(node_id);
        if !self.node_ix.contains_key(&node_id) {
            return None;
        }
        if let Some(cached) = self.doc_cache.get(&node_id) {
            return cached.clone();
        }
        let nodes = self.nodes_map();
        let result = doc::prepare(
            &nodes,
            &self.file_texts,
            &node_id,
            self.defines.as_ref(),
            self.defines_known,
            &self.doc_models,
            20000,
        )
        .ok()
        .flatten()
        .map(Arc::new);
        self.doc_cache.insert(node_id.clone(), result.clone());
        self.doc_cache_order.push_back(node_id.clone());
        while self.doc_cache_order.len() > DOC_CACHE_MAX {
            if let Some(old) = self.doc_cache_order.pop_front() {
                if old != node_id {
                    self.doc_cache.remove(&old);
                }
            }
        }
        result
    }

    /// `doc_cache_stats()` — for `/api/state` and the peak-load regression test.
    pub fn doc_cache_stats(&self) -> Value {
        let (hits, misses) = self.doc_models.stats();
        json!({
            "documents": self.doc_cache.len(),
            "files": self.doc_models.len(),
            "modelHits": hits,
            "modelMisses": misses,
        })
    }

    /// `/api/decls` — the declarations a document or a file contains.
    ///
    /// `node=<id>`: the node plus every declaration nested inside it through
    /// `contains` edges — exactly the set a synthesized document of that node
    /// emits, which is what document symbols, folding, sticky scroll,
    /// breadcrumbs and CodeLens all need.
    ///
    /// `file=<path>`: every declaration whose range lies in that file, for a raw
    /// file window.
    ///
    /// Rows are `lite` node objects (id/name/label/full/kind/file/line/endLine/
    /// access/signature/degree/…) so the client can map them onto its Monaco
    /// model by `(file, line)` and colour them by `kind`.
    /// True for nodes the outline/CodeLens/decl surface should ignore: string
    /// literals (they are nodes with a file/line but are not declarations) and
    /// unresolved external stubs.
    fn is_declaration(node: &Value) -> bool {
        let kind = node.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if matches!(kind, "string" | "external") {
            return false;
        }
        !node
            .get("literal")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub fn decls(&self, node: Option<&str>, file: Option<&str>) -> Value {
        if !self.indexed {
            return json!({"ok": true, "node": null, "file": null, "decls": [], "count": 0,
                          "truncated": false});
        }
        let mut rows: Vec<Value> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

        if let Some(f) = file.filter(|f| !f.is_empty()) {
            let key = self.canon_file(f);
            if let Some(ids) = self.nodes_by_file.get(&key) {
                for id in ids {
                    if !seen.insert(id.clone()) {
                        continue;
                    }
                    if !self.node(id).map(Index::is_declaration).unwrap_or(false) {
                        continue;
                    }
                    if let Some(row) = self.lite(id) {
                        rows.push(row);
                    }
                }
            }
            rows.sort_by(|a, b| {
                let la = a.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
                let lb = b.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
                let ea = a.get("endLine").and_then(|v| v.as_i64()).unwrap_or(0);
                let eb = b.get("endLine").and_then(|v| v.as_i64()).unwrap_or(0);
                (la, -(ea - la)).cmp(&(lb, -(eb - lb)))
            });
            let truncated = rows.len() >= DECLS_LIMIT;
            if truncated {
                rows.truncate(DECLS_LIMIT);
            }
            return json!({"ok": true, "node": null, "file": f, "decls": rows,
                          "count": rows.len(), "truncated": truncated});
        }

        let Some(root) = node.filter(|n| !n.is_empty()).map(|n| self.resolve_source_id(n)).flatten()
        else {
            return json!({"ok": true, "node": null, "file": null, "decls": [], "count": 0,
                          "truncated": false});
        };

        // The node itself, then a breadth-first walk over `contains` edges.
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        seen.insert(root.clone());
        queue.push_back(root.clone());
        while let Some(cur) = queue.pop_front() {
            if self.node(&cur).map(Index::is_declaration).unwrap_or(false) {
                if let Some(row) = self.lite(&cur) {
                    rows.push(row);
                }
            }
            if rows.len() >= DECLS_LIMIT {
                break;
            }
            for &ei in self.adj_ix(&cur) {
                let e = self.edge_obj(ei);
                if e.get("kind").and_then(|v| v.as_str()) != Some("contains") {
                    continue;
                }
                let s = e.get("source").and_then(|v| v.as_str()).unwrap_or("");
                let t = e.get("target").and_then(|v| v.as_str()).unwrap_or("");
                // Descend only: `contains` also edges child -> parent, and the
                // declarations a document contains are its own subtree, not the
                // namespace it lives in or the type that owns it.
                if s != cur {
                    continue;
                }
                let other = t;
                if other.is_empty() || !seen.insert(other.to_string()) {
                    continue;
                }
                queue.push_back(other.to_string());
            }
        }
        // Documents read top-to-bottom, so declarations must be ordered the way
        // they appear: by file, then by line.
        rows.sort_by(|a, b| {
            let fa = a.get("file").and_then(|v| v.as_str()).unwrap_or("");
            let fb = b.get("file").and_then(|v| v.as_str()).unwrap_or("");
            let la = a.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
            let lb = b.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
            (fa, la, a.get("id").and_then(|v| v.as_str()).unwrap_or(""))
                .cmp(&(fb, lb, b.get("id").and_then(|v| v.as_str()).unwrap_or("")))
        });
        let truncated = rows.len() >= DECLS_LIMIT;
        json!({"ok": true, "node": root, "file": null, "decls": rows,
               "count": rows.len(), "truncated": truncated})
    }

    /// The id whose document represents `node_id`: itself for a declared node,
    /// else a declared node with the same `full` name (external `NZK` -> namespace
    /// `#ns:NZK`), or failing that a declared node whose id *is* what was passed.
    fn doc_alias(&self, node_id: &str) -> String {
        fn is_external(v: &Value) -> bool {
            v.get("kind").and_then(|k| k.as_str()) == Some("external")
        }
        let find_declared = |name: &str| -> Option<String> {
            self.nodes
                .iter()
                .find(|m| {
                    !is_external(m)
                        && (m.get("id").and_then(|v| v.as_str()) == Some(name)
                            || m.get("full").and_then(|v| v.as_str()) == Some(name)
                            || m.get("label").and_then(|v| v.as_str()) == Some(name))
                })
                .and_then(|m| m.get("id").and_then(|v| v.as_str()).map(str::to_string))
        };

        let Some(n) = self.node(node_id) else {
            // Not a known id: the caller may have passed a `full`/`label`.
            return find_declared(node_id).unwrap_or_else(|| node_id.to_string());
        };
        if !is_external(n) {
            return node_id.to_string();
        }
        let full = n
            .get("full")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| n.get("label").and_then(|v| v.as_str()));
        match full {
            Some(full) => find_declared(full).unwrap_or_else(|| node_id.to_string()),
            None => node_id.to_string(),
        }
    }

    /// The graph id a source request is really asking about.
    ///
    /// The frontend sometimes hands over a `full` name or a label instead of an
    /// id (and older pop-out links handed over `file:line`). Resolve those to a
    /// real node id when possible so a valid declaration never 404s, and return
    /// `None` when the string names nothing in this graph.
    fn resolve_source_id(&self, raw: &str) -> Option<String> {
        if self.node(raw).is_some() {
            return Some(raw.to_string());
        }
        let alias = self.doc_alias(raw);
        if self.node(&alias).is_some() {
            return Some(alias);
        }
        None
    }

    /// Split a `path:line` target into a file that is actually indexed.
    ///
    /// Accepts the exact display path, or a suffix of it (`MeshesMerge.cs.nzk:12`
    /// for `_sealed_src/MeshesMerge.cs.nzk:12`), and only when the trailing
    /// segment parses as a line number.
    fn split_file_line(&self, raw: &str) -> Option<(String, i64)> {
        let (path, num) = raw.rsplit_once(':')?;
        if path.is_empty() {
            return None;
        }
        let line: i64 = num.trim().parse().ok()?;
        if self.file_texts.contains_key(path) {
            return Some((path.to_string(), line));
        }
        // A Windows drive letter (`C:\...`) never reaches `rsplit_once` twice, so
        // a single suffix match is enough for the relative/absolute variants the
        // UI can produce.
        self.file_texts
            .keys()
            .find(|k| k.ends_with(path) || path.ends_with(k.as_str()))
            .map(|k| (k.clone(), line))
    }

    /// A synthesized document for a symbol that has no declaration site here —
    /// it is declared outside the indexed roots. Rather than answering "not
    /// declared" and leaving the pane empty, list the corpus' references to it,
    /// with the real source lines, so a leaf node always has something to read.
    fn external_usage_doc(&self, id: &str) -> Value {
        let node = self
            .node(id)
            .cloned()
            .unwrap_or_else(|| json!({ "id": id, "full": id, "kind": "external" }));

        // (file, line, other-label, other-id, edge-kind)
        let mut refs: Vec<(String, i64, String, String, String)> = Vec::new();
        for &ei in self.adj.get(id).map(Vec::as_slice).unwrap_or(&[]) {
            let Some(e) = self.edges.get(ei) else { continue };
            let src = e.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let tgt = e.get("target").and_then(|v| v.as_str()).unwrap_or("");
            let other_id = if src == id { tgt } else { src };
            let Some(other) = self.node(other_id) else { continue };
            if is_external_node(other) {
                continue;
            }
            let file = other
                .get("file")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if file.is_empty() {
                continue;
            }
            let label = other
                .get("label")
                .and_then(|v| v.as_str())
                .or_else(|| other.get("full").and_then(|v| v.as_str()))
                .unwrap_or(other_id)
                .to_string();
            let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let lines: Vec<i64> = e
                .get("lines")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(Value::as_i64).collect())
                .unwrap_or_default();
            let fallback = other.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
            if lines.is_empty() {
                refs.push((file, fallback, label, other_id.to_string(), kind));
            } else {
                for l in lines {
                    refs.push((file.clone(), l, label.clone(), other_id.to_string(), kind.clone()));
                }
            }
        }
        refs.sort_by(|a, b| (a.0.as_str(), a.1, a.3.as_str()).cmp(&(b.0.as_str(), b.1, b.3.as_str())));
        refs.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.3 == b.3);

        let total_refs = refs.len();
        let truncated = total_refs > EXTERNAL_REF_LIMIT;
        if truncated {
            refs.truncate(EXTERNAL_REF_LIMIT);
        }
        let files: std::collections::BTreeSet<String> =
            refs.iter().map(|r| r.0.clone()).collect();

        let texts: Vec<(&str, Vec<&str>)> = files
            .iter()
            .filter_map(|f| {
                self.file_texts
                    .get(f)
                    .map(|t| (f.as_str(), t.split('\n').collect::<Vec<&str>>()))
            })
            .collect();
        let text_for = |file: &str| -> Option<&Vec<&str>> {
            texts.iter().find(|(f, _)| *f == file).map(|(_, v)| v)
        };

        let mut lines: Vec<Value> = Vec::new();
        lines.push(json!({
            "n": 0, "ctx": "part",
            "text": format!("/* external symbol · {id} */")
        }));
        lines.push(json!({
            "n": 0, "ctx": "part",
            "text": "/* No declaration site inside the indexed roots — this symbol is \
                     declared outside the corpus (a Unity/VRC/BCL type, or a file that \
                     is not indexed). Its references below are what the index can show. */"
        }));
        if total_refs == 0 {
            lines.push(json!({
                "n": 0, "ctx": "part",
                "text": "/* Nothing in the indexed corpus references it either. */"
            }));
        } else {
            lines.push(json!({
                "n": 0, "ctx": "part",
                "text": format!(
                    "/* {total_refs} reference{} across {} file{} — to see the declaration, add its folder to the index (Browse → Index). */",
                    if total_refs == 1 { "" } else { "s" },
                    files.len(),
                    if files.len() == 1 { "" } else { "s" })
            }));
        }

        let mut n = 0i64;
        let mut last = String::new();
        for (file, lno, label, other_id, kind) in &refs {
            let key = format!("{other_id}@{file}");
            if key != last {
                last = key;
                lines.push(json!({
                    "n": 0, "ctx": "part",
                    "file": file,
                    "text": format!("// ── {label} · {file}:{lno} ({kind})")
                }));
            }
            let text = text_for(file)
                .and_then(|v| v.get((*lno).max(1) as usize - 1).copied())
                .unwrap_or("");
            n += 1;
            lines.push(json!({
                "n": n, "ctx": "body", "file": file, "text": text,
                "orig": lno,
                "src": { "file": file, "line": lno }
            }));
        }
        if truncated {
            lines.push(json!({
                "n": 0, "ctx": "part",
                "text": format!("/* … {} more references not shown */", total_refs - EXTERNAL_REF_LIMIT)
            }));
        }

        json!({
            "ok": true,
            "file": "",
            "path": "",
            "doc": true,
            "mode": "doc",
            "node": node,
            "total": n,
            "start": if n > 0 { 1 } else { 0 },
            "end": n,
            "line": if n > 0 { 1 } else { 0 },
            "definesKnown": false,
            "parts": [],
            "sites": 0,
            "sections": 0,
            "external": true,
            "lines": lines,
        })
    }

    // -- source viewer ------------------------------------------------------
    /// `source(file, line, context, node, to, mode)`.
    /// `source(...)` — the owned-value form, for the CLI and the clone tests.
    pub fn source(
        &mut self,
        file: Option<&str>,
        line: i64,
        context: i64,
        node: Option<&str>,
        to: Option<i64>,
        mode: &str,
    ) -> Option<Value> {
        self.source_shared(file, line, context, node, to, mode)
            .map(|a| (*a).clone())
    }

    /// The shared form of [`Index::source`]: a synthesized document is returned
    /// as the cached `Arc` itself, so serving it costs no deep clone.
    pub fn source_shared(
        &mut self,
        file: Option<&str>,
        line: i64,
        context: i64,
        node: Option<&str>,
        to: Option<i64>,
        mode: &str,
    ) -> Option<Arc<Value>> {
        let mut mode = mode.to_string();
        let mut file = file.map(str::to_string);
        let mut line = line;
        let mut to = to;

        // Resolve the node id first: an id, a full name or a label all land on a
        // real node; a `file:line` target (the pop-out button on a usage site)
        // names a file window instead.
        let node_id = node.and_then(|raw| self.resolve_source_id(raw));
        if node_id.is_none() {
            if let Some(raw) = node {
                if let Some((f, l)) = self.split_file_line(raw) {
                    file = Some(f);
                    line = l;
                    to = None;
                }
            }
        }
        let n = node_id.as_deref().and_then(|id| self.node(id)).cloned();
        if let Some(ref n) = n {
            let nid = n
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if mode == "doc" {
                if let Some(d) = self.doc_source(&nid) {
                    return Some(d);
                }
                // No declaration site: an external symbol still gets a document
                // built from the places that reference it.
                if is_external_node(n) {
                    return Some(Arc::new(self.external_usage_doc(&nid)));
                }
                mode = "raw".to_string();
            } else if is_external_node(n) {
                return Some(Arc::new(self.external_usage_doc(&nid)));
            }
        }
        let mut main = file;
        if let Some(ref n) = n {
            main = n.get("file").and_then(|v| v.as_str()).map(str::to_string);
            line = n.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
            let end = n.get("endLine").and_then(|v| v.as_i64()).unwrap_or(0);
            to = Some(if end != 0 { end } else { line });
        }
        // A node with no file of its own (a namespace, or an external stub that
        // resolution could not tie to a declaration) has no raw view; answer
        // with its synthesized document rather than a 404.
        let Some(main) = main else {
            if let Some(id) = node_id.as_deref() {
                if let Some(d) = self.doc_source(id) {
                    return Some(d);
                }
                return Some(Arc::new(self.external_usage_doc(id)));
            }
            return None;
        };
        if !self.file_texts.contains_key(&main) {
            return None;
        }
        let flags = if mode != "raw" {
            self.file_flags(&main)
        } else {
            None
        };
        if flags.is_none() && mode != "raw" {
            mode = "raw".to_string();
        }
        let raw: Vec<String> = self
            .file_texts
            .get(&main)
            .map(|t| t.split('\n').map(str::to_string).collect())
            .unwrap_or_default();
        let total = raw.len() as i64;
        let line = line.max(1).min(total.max(1));

        let mut out: Vec<Value> = Vec::new();
        if n.is_some() && mode == "effective" {
            let n = n.as_ref().unwrap();
            let parts: Vec<Value> = match n.get("parts").and_then(|v| v.as_array()) {
                Some(p) if !p.is_empty() => p.clone(),
                _ => vec![json!({
                    "file": n.get("file").cloned().unwrap_or_else(|| json!(main)),
                    "line": n.get("line").cloned().unwrap_or_else(|| json!(line)),
                    "endLine": n.get("endLine").cloned().unwrap_or_else(|| json!(line)),
                    "head": n.get("head").cloned().unwrap_or_else(|| json!([])),
                    "tail": n.get("tail").cloned().unwrap_or_else(|| json!([])),
                })],
            };
            let many = parts.len() > 1;
            for p in &parts {
                let pf = p
                    .get("file")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&main)
                    .to_string();
                let pl: Option<Vec<String>> = self
                    .file_texts
                    .get(&pf)
                    .map(|t| t.split('\n').map(str::to_string).collect());
                let Some(pl) = pl else { continue };
                let pflags = if self.defines_known {
                    let joined = pl.join("\n");
                    Some(active_line_flags(
                        &mask_noncode(&strip_bom(&to_chars(&joined))),
                        self.defines.as_ref(),
                    ))
                } else {
                    None
                };
                let a = p.get("line").and_then(|v| v.as_i64()).unwrap_or(1).max(1);
                let b = pl.len().min(
                    p.get("endLine")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(a)
                        .max(a) as usize,
                ) as i64;
                if many {
                    out.push(json!({"n": 0, "ctx": "part", "file": pf,
                                    "text": format!("// ── {pf}:{a}-{b} ──")}));
                }
                let mut body: Vec<Value> = Vec::new();
                for i in a..=b {
                    if let Some(ref fl) = pflags {
                        if !fl.get((i - 1) as usize).copied().unwrap_or(false) {
                            continue;
                        }
                    }
                    let mut o = json!({"n": i, "ctx": "body", "text": pl[(i - 1) as usize]});
                    if pf != main {
                        o.as_object_mut()
                            .unwrap()
                            .insert("file".to_string(), json!(pf));
                    }
                    body.push(o);
                }
                let first = body
                    .first()
                    .and_then(|o| o.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if let Some(head) = p.get("head").and_then(|v| v.as_array()) {
                    for h in head {
                        let hs = h.as_str().unwrap_or("");
                        if !first.is_empty() && first.starts_with(hs.trim()) {
                            continue;
                        }
                        out.push(json!({"n": 0, "ctx": "head", "file": pf, "text": hs}));
                    }
                }
                out.extend(body);
                if let Some(tail) = p.get("tail").and_then(|v| v.as_array()) {
                    for t in tail {
                        out.push(json!({"n": 0, "ctx": "tail", "file": pf,
                                        "text": t.as_str().unwrap_or("")}));
                    }
                }
            }
        } else {
            let (start, end) = if let Some(to) = to {
                let start = line.max(1);
                (start, total.min(to.max(start)))
            } else {
                let half = (context / 2).max(1);
                (line.saturating_sub(half).max(1), total.min(line + half))
            };
            for i in start..=end {
                if flags.as_ref().map(|f| f[(i - 1) as usize]).unwrap_or(true) {
                    out.push(json!({"n": i, "ctx": "body", "text": raw[(i - 1) as usize]}));
                }
            }
            if out.is_empty() {
                for i in start..=end {
                    out.push(json!({"n": i, "ctx": "body", "text": raw[(i - 1) as usize]}));
                }
            }
        }

        let shown: Vec<i64> = out
            .iter()
            .filter_map(|o| o.get("n").and_then(|v| v.as_i64()))
            .filter(|n| *n != 0)
            .collect();
        let parts_meta: Vec<Value> = match n
            .as_ref()
            .and_then(|n| n.get("parts"))
            .and_then(|v| v.as_array())
        {
            Some(ps) => ps
                .iter()
                .map(|p| {
                    json!({
                        "file": p.get("file"),
                        "line": p.get("line"),
                        "endLine": p.get("endLine"),
                    })
                })
                .collect(),
            None => Vec::new(),
        };
        Some(Arc::new(json!({
            "ok": true, "file": main, "mode": mode,
            "start": shown.iter().min().copied().unwrap_or(line),
            "end": shown.iter().max().copied().unwrap_or(line),
            "total": total, "line": line,
            "definesKnown": self.defines_known,
            "parts": parts_meta,
            "lines": out,
        })))
    }

    // -- reorganisation ------------------------------------------------------
    /// `Index.reorg(spec, action, dest, overwrite)` (server.py).
    ///
    /// Merge / split the indexed corpus back into files, prove the result, and
    /// hand it over. The emitting is in-memory; the only action that touches
    /// the filesystem is `write`, and only at the destination the caller named.
    ///
    /// `action=compile` goes further: it compiles the original corpus and the
    /// emitted tree with the same Roslyn toolchain and compares diagnostics
    /// (see [`crate::compile_check`]). Like v1, it reports `skipped` — never a
    /// false pass — when no Unity/`.NET` toolchain can be found.
    pub fn reorg(
        &mut self,
        spec: Option<&Value>,
        action: &str,
        dest: Option<&str>,
        overwrite: bool,
    ) -> Value {
        if !self.indexed {
            return json!({"ok": false, "error": "nothing indexed"});
        }
        let spec_v: Value = match spec {
            Some(v @ Value::Object(_)) => v.clone(),
            _ => json!({}),
        };

        let nodes_map: Map<String, Value> = self
            .nodes
            .iter()
            .filter_map(|n| {
                n.get("id")
                    .and_then(Value::as_str)
                    .map(|id| (id.to_string(), n.clone()))
            })
            .collect();

        let engine = crate::reorg::Reorg::new(
            &nodes_map,
            &self.file_texts,
            self.defines.as_ref(),
            self.defines_known,
            None,
        );

        let plan = match engine.plan(Some(&spec_v)) {
            Ok(p) => p,
            Err(e) => return json!({"ok": false, "error": e}),
        };
        if action == "plan" {
            return json!({"ok": true, "plan": plan});
        }

        // `header` defaults to true; any explicit falsy value turns it off.
        let header = match spec_v.get("header") {
            Some(v) => truthy(Some(v)),
            None => true,
        };
        let files = engine.emit(&plan, header);
        let report = engine.verify(&files, None, Some(&plan));
        let report_ok = report.get("ok").and_then(Value::as_bool).unwrap_or(false);

        match action {
            "verify" => json!({
                "ok": report_ok,
                "plan": plan,
                "report": report,
                "previews": crate::reorg::previews(&files, 40),
            }),
            "zip" => {
                let root = spec_v.get("zipRoot").and_then(Value::as_str).unwrap_or("");
                let bytes = engine.zip_bytes(&files, root);
                // JSON-only transports (Tauri IPC) cannot carry a binary body,
                // so the archive travels base64-encoded; the viewer rebuilds
                // the Blob from `data`.
                json!({
                    "ok": report_ok,
                    "plan": plan,
                    "report": report,
                    "data": b64_encode(&bytes),
                    "name": crate::reorg::zip_name(&spec_v),
                })
            }
            "write" => {
                let wr = engine.write_tree(&files, dest.unwrap_or(""), overwrite);
                let wok = wr.get("ok").and_then(Value::as_bool).unwrap_or(false);
                json!({
                    "ok": wok && report_ok,
                    "plan": plan,
                    "report": report,
                    "write": wr,
                })
            }
            "compile" => {
                // Compile the ORIGINAL corpus and the emitted tree the same
                // way and compare diagnostics; see `crate::compile_check`.
                // Mirrors server.py:
                //   res = CC.differential(self.file_texts, files,
                //                         defines=tuple(spec.get('defines') or
                //                                       CC.DEFAULT_DEFINES))
                //   res['ok'] = bool(res.get('ok')) and report['ok']
                //   res['plan'] = plan; res['report'] = report
                let defines: Vec<String> = match spec_v.get("defines") {
                    Some(Value::Array(a)) if !a.is_empty() => a
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect(),
                    Some(Value::String(s)) if !s.is_empty() => {
                        s.chars().map(|c| c.to_string()).collect()
                    }
                    _ => crate::compile_check::DEFAULT_DEFINES
                        .iter()
                        .map(|s| (*s).to_string())
                        .collect(),
                };
                // `differential_ex` is the same code path `compile-diff` uses,
                // so the body carries the SAME toolchain provenance: the
                // `toolchain` string and the `references` inventory are added
                // additively and every pre-existing key is byte-compatible with
                // `differential` (which is `differential_ex(..).0`).
                let inv = crate::compile_check::RefInventory::default();
                let (mut res, toolchain) = crate::compile_check::differential_ex(
                    &self.file_texts,
                    &files,
                    None,
                    &defines,
                    &[],
                    900,
                    None,
                );
                let diff_ok = res.get("ok").and_then(Value::as_bool).unwrap_or(false);
                if let Value::Object(ref mut m) = res {
                    m.insert("ok".to_string(), json!(diff_ok && report_ok));
                    m.insert("plan".to_string(), plan.clone());
                    m.insert("report".to_string(), report.clone());
                    m.insert("toolchain".to_string(), json!(toolchain));
                    m.insert(
                        "references".to_string(),
                        crate::compile_check::references_report(None, &inv),
                    );
                }
                res
            }
            other => json!({"ok": false, "error": format!("unknown action {other:?}")}),
        }
    }

    // -- facets, definitions, resolve chain ---------------------------------
    /// `children_count(nid)`.
    pub fn children_count(&self, nid: &str) -> i64 {
        self.adj_ix(nid)
            .iter()
            .filter(|&&ei| {
                let e = self.edge_obj(ei);
                e.get("kind").and_then(|v| v.as_str()) == Some("contains")
                    && e.get("source").and_then(|v| v.as_str()) == Some(nid)
            })
            .count() as i64
    }

    /// `kinds()`.
    pub fn kinds(&self) -> Value {
        let mut counts: Vec<(String, i64)> = Vec::new();
        for n in &self.nodes {
            let k = n
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match counts.iter_mut().find(|(key, _)| *key == k) {
                Some((_, c)) => *c += 1,
                None => counts.push((k, 1)),
            }
        }
        let mut namespaces: Vec<Value> = Vec::new();
        let mut types: Vec<Value> = Vec::new();
        let mut public: Vec<Value> = Vec::new();
        for n in &self.nodes {
            let nid = n.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let k = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if k == "namespace" {
                namespaces.push(json!({
                    "id": nid,
                    "full": n.get("full"),
                    "label": n.get("label"),
                    "count": self.children_count(nid),
                }));
            } else if matches!(k, "class" | "struct" | "interface" | "enum" | "record") {
                let owner = n.get("owner").and_then(|v| v.as_str()).unwrap_or("");
                if owner.is_empty() {
                    if let Some(l) = self.lite(nid) {
                        types.push(l);
                    }
                }
            }
            if n.get("access").and_then(|v| v.as_str()) == Some("public")
                && !matches!(k, "string" | "external" | "namespace")
            {
                if let Some(l) = self.lite(nid) {
                    public.push(l);
                }
            }
        }
        namespaces.sort_by(|a, b| {
            a.get("full")
                .and_then(|v| v.as_str())
                .cmp(&b.get("full").and_then(|v| v.as_str()))
        });
        fn by_degree(v: &mut Vec<Value>) {
            v.sort_by(|a, b| {
                let da = a.get("degree").and_then(|x| x.as_i64()).unwrap_or(0);
                let db = b.get("degree").and_then(|x| x.as_i64()).unwrap_or(0);
                let fa = a.get("full").and_then(|x| x.as_str()).unwrap_or("");
                let fb = b.get("full").and_then(|x| x.as_str()).unwrap_or("");
                (-da, fa).cmp(&(-db, fb))
            });
        }
        by_degree(&mut types);
        by_degree(&mut public);
        types.truncate(400);
        public.truncate(800);
        counts.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
        json!({
            "ok": true,
            "kinds": counts.into_iter().map(|(k, c)| json!({"kind": k, "count": c}))
                     .collect::<Vec<Value>>(),
            "namespaces": namespaces,
            "types": types,
            "public": public,
            "counts": self.counts,
        })
    }

    /// `definitions(scope='public', limit=400)`.
    pub fn definitions(&self, scope: &str, limit: i64) -> Value {
        let want_type = scope == "types";
        let mut out: Vec<Value> = Vec::new();
        for n in &self.nodes {
            let nid = n.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let k = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if matches!(k, "string" | "external" | "namespace") {
                continue;
            }
            if want_type {
                if !matches!(k, "class" | "struct" | "interface" | "enum" | "record") {
                    continue;
                }
            } else if n.get("access").and_then(|v| v.as_str()) != Some("public") {
                continue;
            }
            if let Some(l) = self.lite(nid) {
                out.push(l);
            }
        }
        out.sort_by(|a, b| {
            let da = a.get("degree").and_then(|x| x.as_i64()).unwrap_or(0);
            let db = b.get("degree").and_then(|x| x.as_i64()).unwrap_or(0);
            let fa = a.get("full").and_then(|x| x.as_str()).unwrap_or("");
            let fb = b.get("full").and_then(|x| x.as_str()).unwrap_or("");
            (-da, fa).cmp(&(-db, fb))
        });
        let total = out.len();
        out.truncate(py_slice_len(total, limit));
        json!({"ok": true, "scope": scope, "total": total, "definitions": out})
    }

    /// `items(node_id)` of `chain` — the ranked outgoing/incoming neighbours.
    fn chain_items(
        &self,
        node_id: &str,
        direction: &str,
    ) -> Vec<(String, String, Option<i64>)> {
        let mut out: Vec<(String, String, Option<i64>)> = Vec::new();
        for &ei in self.adj_ix(node_id) {
            let e = self.edge_obj(ei);
            let s = e.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let t = e.get("target").and_then(|v| v.as_str()).unwrap_or("");
            let fwd = s == node_id;
            let bwd = t == node_id;
            if direction == "forward" && !fwd {
                continue;
            }
            if direction == "backward" && !bwd {
                continue;
            }
            let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if kind == "contains" && bwd {
                continue; // don't climb into the declaring type
            }
            let other = if fwd { t } else { s };
            let line = e
                .get("lines")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_i64());
            out.push((other.to_string(), kind.to_string(), line));
        }
        let rank = |k: &str| -> i64 {
            match k {
                "calls" => 0,
                "constructs" => 1,
                "assigns" => 2,
                "uses" => 3,
                "inherits" => 4,
                "contains" => 5,
                _ => 9,
            }
        };
        out.sort_by(|a, b| {
            (rank(&a.1), -self.degree_of(&a.0)).cmp(&(rank(&b.1), -self.degree_of(&b.0)))
        });
        out
    }

    /// `chain(nid, depth=3, direction='forward', max_nodes=300)`.
    pub fn chain(&self, nid: &str, depth: i64, direction: &str, max_nodes: i64) -> Option<Value> {
        if !self.node_ix.contains_key(nid) {
            return None;
        }
        let depth = depth.clamp(1, 6);
        let counter = std::cell::Cell::new(0i64);
        let truncated = std::cell::Cell::new(false);

        fn walk(
            this: &Index,
            node_id: &str,
            tree: &mut Vec<Value>,
            d: i64,
            depth: i64,
            direction: &str,
            path: &HashSet<String>,
            counter: &std::cell::Cell<i64>,
            truncated: &std::cell::Cell<bool>,
            max_nodes: i64,
        ) {
            if d >= depth {
                return;
            }
            for (other, kind, line) in this.chain_items(node_id, direction) {
                if counter.get() >= max_nodes {
                    truncated.set(true);
                    return;
                }
                counter.set(counter.get() + 1);
                let cycle = path.contains(&other);
                let mut children: Vec<Value> = Vec::new();
                if !cycle {
                    let mut new_path = path.clone();
                    new_path.insert(other.clone());
                    walk(
                        this,
                        &other,
                        &mut children,
                        d + 1,
                        depth,
                        direction,
                        &new_path,
                        counter,
                        truncated,
                        max_nodes,
                    );
                }
                tree.push(json!({
                    "node": this.lite(&other), "edge": kind, "line": line,
                    "cycle": cycle, "children": children,
                }));
            }
        }

        let mut root_children: Vec<Value> = Vec::new();
        let mut path: HashSet<String> = HashSet::new();
        path.insert(nid.to_string());
        walk(
            self,
            nid,
            &mut root_children,
            0,
            depth,
            direction,
            &path,
            &counter,
            &truncated,
            max_nodes,
        );
        let root = json!({
            "node": self.lite(nid), "edge": null, "line": null,
            "cycle": false, "children": root_children,
        });

        fn flatten(t: &Value, d: i64, flat: &mut Vec<Value>) {
            if let Some(cs) = t.get("children").and_then(|v| v.as_array()) {
                for c in cs {
                    flat.push(json!({
                        "depth": d + 1, "node": c.get("node"),
                        "edge": c.get("edge"), "line": c.get("line"),
                        "cycle": c.get("cycle"),
                    }));
                    flatten(c, d + 1, flat);
                }
            }
        }
        let mut flat: Vec<Value> = Vec::new();
        flatten(&root, 0, &mut flat);
        Some(json!({
            "ok": true, "root": self.lite(nid), "tree": root, "flat": flat,
            "direction": direction, "depth": depth, "truncated": truncated.get(),
        }))
    }

    // -- oracle dump --------------------------------------------------------
    /// The canonical document `tools/oracle.py` emits, from this Index.
    ///
    /// This is the clone contract: exactly the structure `tools/diff_clone.py`
    /// compares field by field.
    pub fn oracle_dump(&mut self, roots: &[String]) -> Value {
        let repo = self.repo_str.clone();
        let basename = Path::new(&repo)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();

        // state (minus timing/cache/viewer storage), paths relativised.
        // `viewer` is deliberately dropped on both sides: v1 stores the UI's
        // theme/camera/positions in the corpus' SQLite file and v2 in its own
        // store, so comparing it would be comparing storage, not behaviour.
        // `docCache` is v2-only runtime cache state (it counts whichever
        // documents this process happens to have synthesized so far), so it is
        // dropped for the same reason `parseMs`/`cache` are: it is not an
        // answer, and leaving it in would make the canonical dump depend on
        // what was asked before the dump.
        let mut state = self.state();
        if let Some(o) = state.as_object_mut() {
            o.remove("parseMs");
            o.remove("fromCache");
            o.remove("cache");
            o.remove("docCache");
            o.remove("viewer");
        }
        let state = doc::rel_value(&state, &repo);

        let mut ids: Vec<String> = self
            .nodes
            .iter()
            .filter_map(|n| n.get("id").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        ids.sort();
        let nodes: Vec<Value> = ids
            .iter()
            .filter_map(|id| self.node(id))
            .map(|n| doc::rel_value(n, &repo))
            .collect();

        let mut edges: Vec<Value> = self.edges.clone();
        edges.sort_by(|a, b| {
            let key = |e: &Value| {
                (
                    e.get("source")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    e.get("target")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    e.get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                )
            };
            key(a).cmp(&key(b))
        });
        let edges: Vec<Value> = edges.iter().map(|e| doc::rel_value(e, &repo)).collect();

        // per-node detail + relation buckets
        let mut detail = Map::new();
        for id in &ids {
            let kind = self
                .node(id)
                .and_then(|n| n.get("kind"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if kind == "string" || kind == "external" {
                continue;
            }
            if let Some(d) = self.node_detail(id) {
                detail.insert(id.clone(), doc::rel_value(&d, &repo));
            }
        }

        // find / refs (the queries oracle.py exercises)
        let find_queries = [
            "NZK.Core.MA.NormaliseName",
            "NormaliseName",
            "SetDirty",
            "errorcode text",
            "BaseMeshName",
        ];
        let mut find = Map::new();
        for q in find_queries {
            let r = self.find(q, 400);
            find.insert(q.to_string(), doc::rel_value(&r, &repo));
        }

        // kinds / definitions
        let kinds = doc::rel_value(&self.kinds(), &repo);
        let mut definitions = Map::new();
        definitions.insert(
            "public".to_string(),
            doc::rel_value(&self.definitions("public", 400), &repo),
        );
        definitions.insert(
            "types".to_string(),
            doc::rel_value(&self.definitions("types", 400), &repo),
        );

        // chain (depth 3, all three directions)
        let doc_nodes = [
            "#ns:NZK",
            "NZK.Core.MA",
            "NZK.Core.MA.NormaliseName",
            "NZK.AD",
        ];
        let mut chain = Map::new();
        for nid in doc_nodes {
            if !self.node_ix.contains_key(nid) {
                continue;
            }
            for direction in ["forward", "backward", "both"] {
                let key = format!("{nid}|{direction}");
                if let Some(c) = self.chain(nid, 3, direction, 300) {
                    chain.insert(key, doc::rel_value(&c, &repo));
                }
            }
        }

        // synthesized documents (digest: hash + head/tail)
        let mut docs = Map::new();
        for nid in doc_nodes {
            if !self.node_ix.contains_key(nid) {
                continue;
            }
            let payload = self.doc_source(nid).map(|a| (*a).clone());
            let digest = doc_digest(payload);
            docs.insert(nid.to_string(), doc::rel_value(&digest, &repo));
        }

        // Reorganisation: the same five specs `tools/oracle.py` uses, each run
        // through plan -> emit -> verify. The key is Python's
        // `json.dumps(spec, sort_keys=True)`, and wall-clock timing is stripped
        // — the two engines cannot and must not agree on how long they took.
        //
        // The specs are independent and `verify`'s `before` pass is identical
        // for all of them, so the corpus is parsed once (the graph `verify`
        // would otherwise rebuild per spec) and the five specs run as parallel
        // units. Five serial re-parses were 80 s of an 86 s dump.
        let nodes_map: Map<String, Value> = self
            .nodes
            .iter()
            .filter_map(|n| {
                n.get("id")
                    .and_then(Value::as_str)
                    .map(|id| (id.to_string(), n.clone()))
            })
            .collect();
        let before = crate::reorg::Reorg::graph_of(&self.file_texts);
        let specs = [
            json!({"group": "single"}),
            json!({"group": "size", "size": 4000}),
            json!({"granularity": "leaf", "group": "size", "size": 3000}),
            json!({"group": "namespace"}),
            json!({"group": "owner"}),
        ];
        let prepared = {
            let nodes_ref = &nodes_map;
            let texts_ref = &self.file_texts;
            let defines_ref = self.defines.as_ref();
            let before_ref = &before;
            let known = self.defines_known;
            std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(specs.len());
                for spec in &specs {
                    let spec = spec;
                    handles.push(scope.spawn(move || {
                        (
                            python_spec_key(spec),
                            reorg_verify_prepared(
                                texts_ref, defines_ref, known, nodes_ref, spec, before_ref,
                            ),
                        )
                    }));
                }
                let mut out = Vec::with_capacity(handles.len());
                for h in handles {
                    match h.join() {
                        Ok(v) => out.push(v),
                        Err(_) => out.push((
                            String::new(),
                            json!({"ok": false, "error": "reorg worker panicked"}),
                        )),
                    }
                }
                out
            })
        };
        let mut reorg = Map::new();
        for (key, mut payload) in prepared {
            strip_ms(&mut payload);
            reorg.insert(key, doc::rel_value(&payload, &repo));
        }

        json!({
            "repo": basename,
            "roots": roots,
            "state": state,
            "nodes": nodes,
            "edges": edges,
            "nodeDetail": detail,
            "find": find,
            "kinds": kinds,
            "definitions": definitions,
            "chain": chain,
            "docs": docs,
            "reorg": reorg,
        })
    }
}

/// plan + emit + verify one reorg spec with the `before` graph supplied by the
/// caller.
///
/// `Index::reorg` builds that graph itself for the single HTTP call it answers;
/// `Index::oracle_dump` runs the same five specs as parallel units and shares
/// one corpus graph across them, because the `before` re-parse is identical for
/// every spec (see `Reorg::verify_with_before`).
fn reorg_verify_prepared(
    file_texts: &HashMap<String, String>,
    defines: Option<&HashSet<String>>,
    defines_known: bool,
    nodes_map: &Map<String, Value>,
    spec: &Value,
    before: &crate::graph::GraphBuilder,
) -> Value {
    let engine = crate::reorg::Reorg::new(nodes_map, file_texts, defines, defines_known, None);
    let plan = match engine.plan(Some(spec)) {
        Ok(p) => p,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let header = match spec.get("header") {
        Some(v) => truthy(Some(v)),
        None => true,
    };
    let files = engine.emit(&plan, header);
    let report = engine.verify_with_before(before, &files, Some(&plan), std::time::Duration::ZERO);
    let report_ok = report.get("ok").and_then(Value::as_bool).unwrap_or(false);
    json!({
        "ok": report_ok,
        "plan": plan,
        "report": report,
        "previews": crate::reorg::previews(&files, 40),
    })
}

/// Remove every `ms` key, recursively — the canonical dump carries no timing.
pub fn strip_ms(v: &mut Value) {
    match v {
        Value::Object(o) => {
            o.remove("ms");
            for x in o.values_mut() {
                strip_ms(x);
            }
        }
        Value::Array(a) => {
            for x in a.iter_mut() {
                strip_ms(x);
            }
        }
        _ => {}
    }
}

/// `json.dumps(spec, sort_keys=True)` — the key `tools/oracle.py` uses for the
/// reorg section of the canonical dump.
///
/// Python's default separators are `", "` and `": "`, keys are sorted, and
/// `ensure_ascii` is on. Specs only ever carry strings, integers and booleans,
/// but the escaping is handled properly so a name with a quote or a non-ASCII
/// character still produces Python's exact bytes.
pub fn python_spec_key(spec: &Value) -> String {
    fn escape(s: &str, out: &mut String) {
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c if (c as u32) < 0x7f => out.push(c),
                c => {
                    let cp = c as u32;
                    if cp > 0xffff {
                        let v = cp - 0x10000;
                        out.push_str(&format!("\\u{:04x}\\u{:04x}",
                                              0xd800 + (v >> 10), 0xdc00 + (v & 0x3ff)));
                    } else {
                        out.push_str(&format!("\\u{cp:04x}"));
                    }
                }
            }
        }
    }
    fn fmt(v: &Value, out: &mut String) {
        match v {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&n.to_string()),
            Value::String(s) => {
                out.push('"');
                escape(s, out);
                out.push('"');
            }
            Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    fmt(x, out);
                }
                out.push(']');
            }
            Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push('"');
                    escape(k, out);
                    out.push_str("\": ");
                    fmt(o.get(*k).unwrap(), out);
                }
                out.push('}');
            }
        }
    }
    let mut s = String::new();
    fmt(spec, &mut s);
    s
}

/// Standard base64 (RFC 4648, with padding).
/// Only needed because the Tauri IPC channel is JSON-only: a `zip` action
/// carries the archive as text and the viewer rebuilds the Blob from it. Kept
/// local so the engine needs no extra dependency.
pub fn b64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// `_doc_digest(payload)` — keep the payload, shrink `lines` to a hash + sample.
pub fn doc_digest(payload: Option<Value>) -> Value {
    let out = match payload {
        Some(Value::Object(o)) => o,
        Some(other) => return other,
        None => return Value::Null,
    };
    let mut out = out;
    let lines = out.remove("lines");
    if let Some(Value::Array(lines)) = lines {
        let mut blob = String::new();
        for (i, l) in lines.iter().enumerate() {
            if i > 0 {
                blob.push('\n');
            }
            blob.push_str(&py_str(l.get("n").unwrap_or(&Value::Null)));
            blob.push('\u{1f}');
            blob.push_str(&py_str(l.get("ctx").unwrap_or(&Value::Null)));
            blob.push('\u{1f}');
            blob.push_str(&py_str_opt(l.get("sec")));
            blob.push('\u{1f}');
            blob.push_str(&py_str_opt(l.get("text")));
        }
        out.insert("lineCount".to_string(), json!(lines.len()));
        out.insert("linesSha256".to_string(), json!(sha256_hex(blob.as_bytes())));
        out.insert(
            "linesHead".to_string(),
            Value::Array(lines.iter().take(40).cloned().collect()),
        );
        let skip = lines.len().saturating_sub(40);
        out.insert(
            "linesTail".to_string(),
            Value::Array(lines.iter().skip(skip).cloned().collect()),
        );
    }
    Value::Object(out)
}

/// Python `%s` for a JSON value (`None` -> `"None"`).
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Python `x or ''` followed by `%s`.
fn py_str_opt(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) | Some(Value::Bool(false)) => String::new(),
        Some(Value::String(s)) if s.is_empty() => String::new(),
        Some(other) => py_str(other),
    }
}

/// `key.partition(':')` — `(before, sep, after)`.
fn split3(key: &str) -> (&str, &str, &str) {
    match key.find(':') {
        Some(i) => (&key[..i], ":", &key[i + 1..]),
        None => (key, "", ""),
    }
}

/// `Index.build`'s node comprehension, field for field.
fn normalize_node(gnode: &Value) -> Map<String, Value> {
    let get = |k: &str| gnode.get(k);
    let id = get("id").cloned().unwrap_or_else(|| json!(""));
    let full = match get("full") {
        Some(v) if !v.is_null() => v.clone(),
        _ => id.clone(),
    };
    let mut n = Map::new();
    n.insert("id".to_string(), id);
    n.insert(
        "name".to_string(),
        get("name").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!("")),
    );
    n.insert(
        "label".to_string(),
        get("label").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!("")),
    );
    n.insert("full".to_string(), full);
    n.insert(
        "kind".to_string(),
        get("kind").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!("")),
    );
    n.insert("ns".to_string(), str_or_empty(get("ns")));
    n.insert("owner".to_string(), str_or_empty(get("owner")));
    n.insert("ownerKind".to_string(), str_or_empty(get("ownerKind")));
    n.insert("parent".to_string(), str_or_empty(get("parent")));
    n.insert("file".to_string(), str_or_empty(get("file")));
    n.insert("line".to_string(), int_or_zero(get("line")));
    n.insert("endLine".to_string(), int_or_zero(get("endLine")));
    n.insert("signature".to_string(), str_or_empty(get("signature")));
    n.insert("ret".to_string(), str_or_empty(get("ret")));
    n.insert(
        "params".to_string(),
        get("params")
            .filter(|v| v.is_array())
            .cloned()
            .unwrap_or_else(|| json!([])),
    );
    n.insert("access".to_string(), str_or_empty(get("access")));
    n.insert(
        "static".to_string(),
        get("static")
            .and_then(|v| v.as_bool())
            .map(Value::Bool)
            .unwrap_or_else(|| json!(false)),
    );
    n.insert("literal".to_string(), json!(truthy(get("literal"))));
    n.insert("external".to_string(), json!(truthy(get("external"))));
    n.insert("degree".to_string(), int_or_zero(get("degree")));
    n.insert("head".to_string(), list_or_empty(get("head")));
    n.insert("tail".to_string(), list_or_empty(get("tail")));
    n.insert(
        "startOff".to_string(),
        get("startOff").cloned().unwrap_or(Value::Null),
    );
    n.insert(
        "endOff".to_string(),
        get("endOff").cloned().unwrap_or(Value::Null),
    );
    n.insert(
        "parts".to_string(),
        get("_parts")
            .filter(|v| v.is_array())
            .cloned()
            .unwrap_or_else(|| json!([])),
    );
    n
}

fn str_or_empty(v: Option<&Value>) -> Value {
    match v {
        Some(Value::String(s)) => Value::String(s.clone()),
        _ => json!(""),
    }
}

fn int_or_zero(v: Option<&Value>) -> Value {
    match v {
        Some(Value::Number(n)) => Value::Number(n.clone()),
        _ => json!(0),
    }
}

fn list_or_empty(v: Option<&Value>) -> Value {
    match v {
        Some(Value::Array(a)) => Value::Array(a.clone()),
        _ => json!([]),
    }
}

/// Python `open(path, encoding='utf-8', errors='replace')` text mode: the BOM
/// is kept and universal newlines translate CRLF / CR to LF.
pub fn python_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    if s.contains('\r') {
        s.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        s.into_owned()
    }
}

// ---------------------------------------------------------------------------
// SHA-256 (no dependency available; `oracle.py` hashes with hashlib)
// ---------------------------------------------------------------------------
const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let long = "a".repeat(1_000_000);
        assert_eq!(
            sha256_hex(long.as_bytes()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn python_slice_and_int_semantics() {
        assert_eq!(py_slice_len(10, 3), 3);
        assert_eq!(py_slice_len(10, -1), 9);
        assert_eq!(py_slice_len(10, -20), 0);
        assert_eq!(py_int(Some(&json!(" 30 ")), 0), 30);
        assert_eq!(py_int(Some(&json!("x")), 7), 7);
        assert_eq!(py_int(None, 5), 5);
        assert_eq!(py_int(Some(&json!(-3)), 0), -3);
    }

    #[test]
    fn source_doc_resolves_namespace_and_type_nodes() {
        let dir = std::env::temp_dir().join(format!("csg-doc-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.cs"),
            "namespace NZK {\npublic static partial class B {\n  public static int A() { return 1; }\n}\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("b.cs"),
            "namespace NZK {\npublic static partial class B {\n  public static int C() { return 2; }\n}\n}\n",
        )
        .unwrap();

        let mut ix = Index::new(dir.clone());
        let cfg = Config::default();
        ix.build(&[".".to_string()], &[], &cfg);

        // The namespace node is `#ns:NZK`; `/api/source?node=#ns:NZK&mode=doc`
        // must synthesize it rather than 404.
        let ns = ix.source(None, 1, 30, Some("#ns:NZK"), None, "doc");
        assert!(ns.is_some(), "namespace doc must synthesize");
        assert_eq!(ns.unwrap()["mode"], "doc");

        let ty = ix.source(None, 1, 30, Some("NZK.B"), None, "doc");
        assert!(ty.is_some(), "type doc must synthesize");
        assert_eq!(ty.unwrap()["parts"].as_array().unwrap().len(), 2);

        // The graph also carries an external stub `NZK` (from a bare reference).
        // "View code" on it must resolve to the namespace node, not 404.
        let idx = ix.nodes.len();
        ix.nodes.push(json!({
            "id": "NZK", "full": "NZK", "label": "NZK", "kind": "external",
            "file": "", "line": 0,
        }));
        ix.node_ix.insert("NZK".to_string(), idx);
        assert_eq!(ix.doc_alias("NZK"), "#ns:NZK");
        let ext = ix.source(None, 1, 30, Some("NZK"), None, "doc");
        assert!(ext.is_some(), "external NZK must resolve to the namespace doc");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `full`/label is not an id, but the source pane must still find the node
    /// (the frontend hands over whatever the graph row carries).
    #[test]
    fn source_resolves_full_names_and_labels() {
        let dir = std::env::temp_dir().join(format!("csg-src-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.cs"),
            "namespace NZK {\npublic static class A {\n  public static int M() { return 1; }\n}\n}\n",
        )
        .unwrap();
        let mut ix = Index::new(dir.clone());
        ix.build(&[".".to_string()], &[], &Config::default());

        // The method's id carries the `()`; its `full` does not.
        let by_full = ix.source(None, 1, 30, Some("NZK.A.M"), None, "doc");
        assert!(by_full.is_some(), "full name must resolve to the method");
        assert_eq!(by_full.unwrap()["mode"], "doc");

        let by_label = ix.source(None, 1, 30, Some("A.M"), None, "doc");
        assert!(by_label.is_some(), "label must resolve to the method");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An external symbol has no declaration site, but a real file references it.
    /// "View code" must produce the usage document, not a 404.
    #[test]
    fn external_symbol_gets_usage_document() {
        let dir = std::env::temp_dir().join(format!("csg-src-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.cs"),
            "namespace NZK {\npublic static class A {\n  public static void M() { UnityEngine.GameObject go = null; }\n}\n}\n",
        )
        .unwrap();
        let mut ix = Index::new(dir.clone());
        ix.build(&[".".to_string()], &[], &Config::default());

        // The parser records the unresolved root of the qualified reference.
        let ext_id = ix
            .nodes()
            .iter()
            .find(|n| is_external_node(n))
            .and_then(|n| n.get("id").and_then(|v| v.as_str()))
            .expect("the corpus references an external symbol")
            .to_string();

        let ext = ix.source(None, 1, 30, Some(&ext_id), None, "doc");
        let ext = ext.expect("external symbol must synthesize a usage document");
        assert_eq!(ext["external"], json!(true));
        assert_eq!(ext["mode"], json!("doc"));
        let lines = ext["lines"].as_array().unwrap();
        assert!(lines.len() > 3, "usage document must list references");
        let joined: String = lines
            .iter()
            .map(|l| l.get("text").and_then(|v| v.as_str()).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("external symbol"), "header present");
        assert!(joined.contains(&ext_id), "references the symbol");
        assert!(joined.contains("a.cs"), "names the referencing file");

        // `raw` on an external is the same document, never a 404.
        let raw = ix.source(None, 1, 30, Some(&ext_id), None, "raw");
        assert!(raw.is_some(), "raw on an external must not 404");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pop-out button can hand over `file:line` as the "node"; that must open
    /// the file window instead of being treated as an unknown symbol.
    #[test]
    fn source_accepts_file_line_targets() {
        let dir = std::env::temp_dir().join(format!("csg-src-fileline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.cs"),
            "namespace NZK {\npublic static class A {\n  public static int M() { return 1; }\n}\n}\n",
        )
        .unwrap();
        let mut ix = Index::new(dir.clone());
        ix.build(&[".".to_string()], &[], &Config::default());

        let target = format!("a.cs:{}", 3);
        let hit = ix.source(None, 1, 30, Some(&target), None, "raw");
        let hit = hit.expect("`file:line` must open the indexed file");
        assert_eq!(hit["mode"], json!("raw"));
        let lines = hit["lines"].as_array().unwrap();
        assert!(!lines.is_empty(), "file window has lines");

        // A suffix of the display path works too.
        let suffix = format!("a.cs:{}", 3);
        assert!(ix.source(None, 1, 30, Some(&suffix), None, "raw").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn doc_digest_hashes_and_samples() {
        let lines: Vec<Value> = (1..=100)
            .map(|n| json!({"n": n, "ctx": "body", "text": format!("line {n}")}))
            .collect();
        let payload = json!({"ok": true, "lines": lines});
        let d = doc_digest(Some(payload));
        assert_eq!(d["lineCount"], 100);
        assert_eq!(d["linesHead"].as_array().unwrap().len(), 40);
        assert_eq!(d["linesTail"].as_array().unwrap().len(), 40);
        assert_eq!(d["linesHead"][0]["text"], "line 1");
        assert_eq!(d["linesTail"][39]["text"], "line 100");
        assert!(d.get("lines").is_none());
        assert_eq!(
            d["linesSha256"],
            "7394f1cc4c9684bdf4a5a3fc9f3a22870a46e619af08eee8f5b68aee26382351".to_string()
        );
    }

    /// Peak-load regression: a second document that reads the same file must not
    /// re-lex it, and a repeated request must not re-synthesize or deep-clone.
    ///
    /// Before the fix `doc_source` passed `&DocCache::new()`, so every document
    /// build re-parsed every file it touched — "showing a partial doc needs to
    /// load the full doc" all over again on each open.
    #[test]
    fn doc_cache_is_shared_across_documents_and_hits_are_shared() {
        let dir = std::env::temp_dir().join(format!("csg-doccache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Two types in ONE file: the second document must reuse the first's model.
        std::fs::write(
            dir.join("a.cs"),
            "namespace NZK {\npublic static class A {\n  public static int M() { return B.N(); }\n}\npublic static class B {\n  public static int N() { return 1; }\n}\n}\n",
        )
        .unwrap();
        let mut ix = Index::new(dir.clone());
        ix.build(&[".".to_string()], &[], &Config::default());

        let a = ix.doc_source("NZK.A").expect("A has a document");
        let (_, misses_after_a) = ix.doc_models.stats();
        assert!(misses_after_a >= 1, "the file was lexed for A");
        let files_after_a = ix.doc_models.len();

        let b = ix.doc_source("NZK.B").expect("B has a document");
        let (_, misses_after_b) = ix.doc_models.stats();
        assert_eq!(
            misses_after_b, misses_after_a,
            "B must reuse A's already-lexed file model"
        );
        assert_eq!(ix.doc_models.len(), files_after_a);

        // The second request for A is the memoized Arc: same allocation, no clone.
        let a2 = ix.doc_source("NZK.A").expect("A is cached");
        assert!(Arc::ptr_eq(&a, &a2), "a cache hit must return the same Arc");

        let stats = ix.doc_cache_stats();
        assert_eq!(stats["documents"], json!(2));
        assert!(stats["modelHits"].as_u64().unwrap() >= 1);
        // Sanity: the two documents really differ.
        let text = |v: &Value| {
            v["lines"]
                .as_array()
                .unwrap()
                .iter()
                .map(|l| l["text"].as_str().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_ne!(text(&a), text(&b));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/api/decls` must return the declaration tree a document contains, in
    /// document order, with the ranges the outline/CodeLens/folding need.
    #[test]
    fn decls_lists_nested_declarations_in_order() {
        let dir = std::env::temp_dir().join(format!("csg-decls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.cs"),
            "namespace NZK {\npublic static class A {\n  public int F;\n  public static int M() { return 1; }\n}\n}\n",
        )
        .unwrap();
        let mut ix = Index::new(dir.clone());
        ix.build(&[".".to_string()], &[], &Config::default());

        let d = ix.decls(Some("NZK.A"), None);
        assert_eq!(d["ok"], json!(true));
        let rows = d["decls"].as_array().unwrap();
        let ids: Vec<&str> = rows
            .iter()
            .map(|r| r["id"].as_str().unwrap_or(""))
            .collect();
        assert!(ids.contains(&"NZK.A"), "the root is included: {ids:?}");
        assert!(ids.iter().any(|i| i.contains(".M")), "method: {ids:?}");
        assert!(ids.iter().any(|i| i.ends_with(".F")), "field: {ids:?}");
        // Document order: the field is declared before the method.
        let fi = ids.iter().position(|i| i.ends_with(".F")).unwrap();
        let mi = ids.iter().position(|i| i.contains(".M")).unwrap();
        assert!(fi < mi, "declarations are ordered by line: {ids:?}");
        for r in rows {
            assert!(r["line"].as_i64().unwrap_or(0) > 0, "line is set");
            assert!(r["endLine"].as_i64().unwrap_or(0) > 0, "endLine is set");
        }

        // `file=` returns every declaration in one file.
        let by_file = ix.decls(None, Some("a.cs"));
        assert!(by_file["decls"].as_array().unwrap().len() >= 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Parser warnings still say they are parser warnings; when a `.csproj`
    /// claims the file, they also name the project that owns it.
    #[test]
    fn parser_warnings_are_attributed_to_their_project() {
        let dir = std::env::temp_dir().join(format!("csgraph-warnattr-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("App")).unwrap();
        fs::write(
            dir.join("App/App.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\">\n<PropertyGroup>\
             <AssemblyName>DemoApp</AssemblyName></PropertyGroup>\n</Project>\n",
        )
        .unwrap();
        fs::write(dir.join("App/Program.cs"), "namespace N { class Program {} }\n").unwrap();

        let mut ix = Index::new(dir.clone());
        ix.set_projects(crate::config::discover_projects(&dir, &[], 1));
        assert_eq!(
            ix.attribute_warning("App/Program.cs", "scope guard tripped"),
            "App/Program.cs: scope guard tripped (project: DemoApp)"
        );
        // No project claims this file: the warning stays unattributed.
        assert_eq!(
            ix.attribute_warning("Other.cs", "scope guard tripped"),
            "Other.cs: scope guard tripped"
        );
        assert_eq!(
            ix.projects_for_file("App/Program.cs"),
            vec!["App/App.csproj".to_string()]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_excludes_prune_build_and_vendor_trees() {
        let cfg = Config::default();
        // The default corpus is `_sealed_src`; these additions only matter for
        // a wider root, but they must be present for `root = "."` to stay
        // bounded.
        for name in [
            "cache", "target", ".rustup", ".cargo", "vendor", "__pycache__", ".venv",
        ] {
            assert!(
                cfg.excludes.iter().any(|e| e == name),
                "default excludes must contain {name:?}: {:?}",
                cfg.excludes
            );
        }
        // Existing defaults are preserved bit-for-bit.
        for name in [
            ".git", "obj", "bin", "tmp", "node_modules", "_bak", "_bak_nzk_consolidate",
            "_bak_v6_originals", "_superseded", "testing",
        ] {
            assert!(
                cfg.excludes.iter().any(|e| e == name),
                "default excludes lost {name:?}"
            );
        }
    }

    #[test]
    fn bounded_walk_check_refuses_over_the_file_ceiling() {
        let dir = std::env::temp_dir().join(format!("csg-walk-cap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        for i in 0..8 {
            fs::write(dir.join("sub").join(format!("f{i}.cs")), "// x\n").unwrap();
        }
        let roots = vec![".".to_string()];
        // A cap the walk exceeds: refused with a message naming the limit.
        let err = bounded_walk_check(&dir, &roots, &[], 4, 30.0).unwrap_err();
        assert!(err.contains("aborted") && err.contains("4 files"), "{err}");
        // A cap it does not exceed: the same tree passes.
        assert!(bounded_walk_check(&dir, &roots, &[], 64, 30.0).is_ok());
        // The excludes prune the tree the same way discovery does.
        assert!(bounded_walk_check(&dir, &roots, &["sub".to_string()], 4, 30.0).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    /// The pre-change implementation, kept as the differential oracle for the
    /// `text_hits` fast path: compile `hit_regex`, sort the files, scan every
    /// line of every file, smallest-span `enclosing`. It must answer every query
    /// exactly as the optimized path does.
    fn text_hits_reference(ix: &Index, q: &str, limit: i64) -> (Vec<Value>, bool) {
        let q = q.trim();
        if q.is_empty() {
            return (Vec::new(), false);
        }
        let pat = Index::hit_regex(q);
        let mut out: Vec<Value> = Vec::new();
        let mut files: Vec<&String> = ix.file_texts.keys().collect();
        files.sort();
        for f in files {
            let text = &ix.file_texts[f];
            for (i, line) in text.split('\n').enumerate() {
                let i = i as i64 + 1;
                let Some(m) = pat.find(line) else { continue };
                let col = line[..m.start()].chars().count() as i64 + 1;
                let nid = ix.enclosing(f, i);
                out.push(json!({
                    "file": f, "line": i, "col": col,
                    "text": line.trim().chars().take(400).collect::<String>(),
                    "node": nid.as_deref().and_then(|x| ix.lite(x)),
                }));
                if out.len() as i64 >= limit {
                    return (out, true);
                }
            }
        }
        (out, false)
    }

    /// The ASCII fast path must be byte-identical to the regex it replaces, in
    /// particular on the cases that make a naive `contains` prefilter unsound:
    /// Unicode case folding (`\u{017f}` long s -> `s`, `\u{212a}` kelvin -> `k`)
    /// on a non-ASCII line inside a file with no ASCII occurrence at all.
    #[test]
    fn text_hits_fast_path_matches_regex_reference() {
        let dir = std::env::temp_dir().join(format!("csg-hits-fast-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.cs"),
            "class A {\n  int Soket = 1;  // Socket\n  string s = \"MiXeD\";\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("b.cs"),
            "class B {\n  void socket_plug() {}\n  void Plug() {}\n  // MiXeD Socket\n}\n",
        )
        .unwrap();
        // No ASCII `s` or `k` anywhere: the only match for `s` / `k` is the
        // non-ASCII char that folds to it, so the file-level prefilter must not
        // skip this file.
        std::fs::write(
            dir.join("c.cs"),
            "class C {\n  // \u{017f} and \u{212a} alone\n}\n",
        )
        .unwrap();

        let mut ix = Index::new(dir.clone());
        let cfg = Config::default();
        ix.build(&[".".to_string()], &[], &cfg);

        let queries = [
            "socket", "SOCKET", "sOcKeT", "Socket", "plug", "Plug", "s", "k", "S", "K",
            "MiXeD", "mixed", "public static", "NZK.Core.NaN", "zzz_absent", "  ",
            "\u{017f}", "\u{212a}",
        ];
        for q in queries {
            let got = ix.text_hits(q, 400);
            let want = text_hits_reference(&ix, q, 400);
            assert_eq!(
                serde_json::to_string(&got.0).unwrap(),
                serde_json::to_string(&want.0).unwrap(),
                "payload differs for query {q:?}"
            );
            assert_eq!(got.1, want.1, "truncated flag differs for query {q:?}");
            // The fold cases only exist on non-ASCII lines; make sure the fast
            // path is not skipping them outright.
            if q == "s" || q == "k" {
                assert!(
                    got.0.iter().any(|h| h["file"] == "c.cs"),
                    "query {q:?} must still find c.cs via Unicode case folding"
                );
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- gap 5: node-key vs file-key path forms ---------------------------

    #[test]
    fn canon_file_settles_the_repo_relative_rule() {
        let repo = Path::new("/repo/root");
        // Inside the repo -> repo-relative, the canonical clone form.
        assert_eq!(canon_file_with("/repo/root/a/b.cs", repo), "a/b.cs");
        assert_eq!(canon_file_with("/repo/root", repo), ".");
        assert_eq!(canon_file_with("/repo/root/../root/a.cs", repo), "a.cs");
        // Outside the repo -> normalized absolute, unchanged.
        assert_eq!(canon_file_with("/other/x.cs", repo), "/other/x.cs");
        // Already relative -> unchanged apart from slash normalisation.
        assert_eq!(canon_file_with("a/b.cs", repo), "a/b.cs");
        assert_eq!(canon_file_with("a\\b.cs", repo), "a/b.cs");
        assert_eq!(canon_file_with("", repo), "");
        // Backslashes in an absolute path normalise like `display_path`.
        assert_eq!(canon_file_with("/repo/root/a\\b.cs", repo), "a/b.cs");
    }

    /// The Nemesis-style repro: an index built while `--repo` did not lexically
    /// contain the files (display -> absolute) primes the parse cache; a second
    /// build with a multi-segment relative root reuses that parse. Before the
    /// canon rule, node `file`s stayed absolute while `file_texts` keys were
    /// relative, so `decls {file: <relative>}` returned 0 and `find`/`refs`
    /// usage rows lost their `node`. Pin that both spellings now resolve.
    #[test]
    fn multi_segment_root_paths_are_consistent_after_a_repo_respelling() {
        let dir = std::env::temp_dir().join(format!("csg-gap5-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub/dir")).unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        std::fs::write(
            dir.join("sub/dir/a.cs"),
            "namespace N {\n public class C {\n  public int Helper() { return 2; }\n  public int Use() { return Helper(); }\n }\n}\n",
        )
        .unwrap();
        let cfg = Config::default();

        // Build A: the repo spelling does not contain the file lexically, so
        // `display` keeps the absolute form and the parse cache stores it.
        let root_abs = dir.join("sub/dir").to_string_lossy().into_owned();
        let mut a = Index::new(dir.join("other"));
        a.build(&[root_abs], &[], &cfg);
        assert!(
            a.nodes.iter().any(|n| n["file"]
                .as_str()
                .map(|f| f.starts_with('/'))
                .unwrap_or(false)),
            "build A should have produced absolute node paths: {:?}",
            a.nodes.iter().map(|n| n["file"].clone()).collect::<Vec<_>>()
        );

        // Build B: the multi-segment relative root on the real repo. The parse
        // cache from A is reused, and the installed graph must still be
        // canonical.
        let mut b = Index::new(dir.clone());
        b.build(&["sub/dir".to_string()], &[], &cfg);
        let files: Vec<String> = b
            .nodes
            .iter()
            .filter_map(|n| n.get("file").and_then(|v| v.as_str()).map(str::to_string))
            .filter(|f| !f.is_empty())
            .collect();
        assert!(
            files.iter().any(|f| f == "sub/dir/a.cs"),
            "node files must be repo-relative after the respelling: {files:?}"
        );
        for f in &files {
            assert!(
                b.file_texts.contains_key(f),
                "every node file must have a matching text key: {f:?}"
            );
        }

        // Both spellings resolve for decls; the relative one used to return 0.
        let rel = b.decls(None, Some("sub/dir/a.cs"));
        assert!(
            rel["count"].as_i64().unwrap_or(0) > 0,
            "relative decls must be non-zero: {rel}"
        );
        let abs = dir.join("sub/dir/a.cs").to_string_lossy().into_owned();
        let abs_decls = b.decls(None, Some(&abs));
        assert!(
            abs_decls["count"].as_i64().unwrap_or(0) > 0,
            "absolute decls must still work: {abs_decls}"
        );

        // `find` usage rows keep their enclosing node and the display key.
        let found = b.find("Helper", 400);
        let usages = found["usages"].as_array().cloned().unwrap_or_default();
        assert!(!usages.is_empty(), "find must produce usages: {found}");
        assert!(
            usages.iter().all(|u| !u["node"].is_null()),
            "usage rows lost their node: {found}"
        );
        assert_eq!(usages[0]["file"], json!("sub/dir/a.cs"));
        assert!(
            b.enclosing("sub/dir/a.cs", usages[0]["line"].as_i64().unwrap_or(0)).is_some(),
            "enclosing must resolve the display key"
        );
        assert!(
            b.enclosing(&abs, usages[0]["line"].as_i64().unwrap_or(0)).is_some(),
            "enclosing must resolve the absolute spelling too"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// The auto-selection rule for live viewer instances: one selects itself,
    /// zero is empty, several are ambiguous (never a guess).
    #[test]
    fn instance_selection_is_conservative_when_ambiguous() {
        fn inst(pid: u32, user: &str) -> crate::registry::Instance {
            crate::registry::Instance {
                pid,
                uid: Some(0),
                user: user.to_string(),
                repo: "/repo".to_string(),
                roots: vec!["r".to_string()],
                cache_dir: "/repo/r".to_string(),
                state_path: "/repo/r/.csgraph.v2/viewer.json".to_string(),
                mode: "serve".to_string(),
                port: None,
                started_ms: 0,
                alive: true,
                stale: false,
                cmdline: String::new(),
                record: String::new(),
                source: "registry",
            }
        }
        let a = inst(1, "alice");
        let b = inst(2, "bob");

        let one = vec![&a];
        let (active, ambiguous, by, _) = select_instance(&one, None);
        assert_eq!(active.map(|i| i.pid), Some(1));
        assert!(!ambiguous);
        assert_eq!(by, "auto");

        let none: Vec<&crate::registry::Instance> = Vec::new();
        let (active, ambiguous, _, note) = select_instance(&none, None);
        assert!(active.is_none());
        assert!(!ambiguous);
        assert!(note.contains("no live viewer"));

        let two = vec![&a, &b];
        let (active, ambiguous, _, note) = select_instance(&two, None);
        assert!(active.is_none());
        assert!(ambiguous);
        assert!(note.contains("instance="));

        let (active, ambiguous, by, _) = select_instance(&two, Some("bob"));
        assert_eq!(active.map(|i| i.pid), Some(2));
        assert!(!ambiguous);
        assert_eq!(by, "instance");

        let (active, ambiguous, _, _) = select_instance(&two, Some("nobody"));
        assert!(active.is_none());
        assert!(!ambiguous);
    }
}


