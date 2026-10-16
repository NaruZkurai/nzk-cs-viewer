//! CS Viewer v2 — the Rust host.
//!
//! One Tauri process, an embedded webview per window, and NO Python and NO HTTP
//! server in the graph path: the frontend's `/api/*` fetches are handed to this
//! process over Tauri IPC and answered by `csgraph-core`, the Rust port of the
//! csgraph engine. The only Python that ships with v2 is the CS splitter.
//!
//! Isolation is the point of v2:
//!
//!   * every document can be opened as its own [`WebviewWindow`] — its own
//!     webview, its own JavaScript main thread, its own Monaco worker, so a
//!     pathological document cannot stall another window;
//!   * every `/api/*` call runs on a worker thread off the UI thread, so the
//!     window that asked stays responsive while a 36k-line document is
//!     synthesized;
//!   * the engine is shared behind a lock, so simultaneous windows see one
//!     consistent graph instead of racing to re-index it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{Emitter, Manager, State, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

use csgraph_core::{api, compile_check, config, dedupe, edit, parser};

/// stderr logging that never tears the app down. `eprintln!` *panics* when the
/// write fails (e.g. a closed stderr pipe), and a panic inside `setup` exits the
/// process — i.e. a window that opens and instantly closes for no visible reason.
macro_rules! note {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

// ---------------------------------------------------------------------------
// NVIDIA / WebKitGTK rendering quirk
// ---------------------------------------------------------------------------
//
// On the proprietary NVIDIA driver, WebKitGTK's DMA-BUF renderer enables
// Wayland explicit sync but commits without an acquire point. Compositors that
// enforce the rule (KWin/Plasma) kill the connection, which looks like the
// window appearing and exiting about a second later, with WebKitWebProcess
// dying in libnvidia-eglcore. Upstream: tauri-apps/tauri#9394, WebKitGTK
// bug #280210.
//
// Disabling explicit sync and the DMA-BUF renderer keeps the window alive. An
// explicit choice in the environment always wins, as do --no-gpu-quirk and
// CSVIEWER_NO_GPU_QUIRK=1.
const GPU_QUIRK_VARS: [&str; 4] = [
    "WEBKIT_DISABLE_DMABUF_RENDERER",
    "__NV_DISABLE_EXPLICIT_SYNC",
    "WEBKIT_DISABLE_COMPOSITING_MODE",
    "WEBKIT_DMABUF_RENDERER_FORCE_SHM",
];

/// True when a DRM card is bound to the proprietary `nvidia` kernel driver.
fn nvidia_driver_loaded() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("card") || name.contains('-') {
            return false;
        }
        std::fs::read_link(e.path().join("device/driver"))
            .ok()
            .and_then(|t| t.file_name().map(|n| n == "nvidia"))
            .unwrap_or(false)
    })
}

/// Whether GDK will pick the Wayland backend.
fn gdk_uses_wayland() -> bool {
    if let Ok(b) = std::env::var("GDK_BACKEND") {
        if let Some(first) = b
            .split(',')
            .map(str::trim)
            .find(|s| *s == "x11" || *s == "wayland")
        {
            return first == "wayland";
        }
    }
    match std::env::var("XDG_SESSION_TYPE").ok().as_deref() {
        Some("wayland") => true,
        Some("x11") => false,
        _ => std::env::var_os("WAYLAND_DISPLAY").is_some(),
    }
}

/// `set_var` is unsafe in edition 2024; this runs from `main` before any thread
/// exists and nothing else is reading the environment concurrently.
fn set_quirk_env(key: &str, value: &str) {
    unsafe { std::env::set_var(key, value) };
}

fn apply_gpu_quirk() -> Option<&'static str> {
    if std::env::var_os("CSVIEWER_NO_GPU_QUIRK").is_some() {
        return None;
    }
    if GPU_QUIRK_VARS.iter().any(|v| std::env::var_os(v).is_some()) {
        return None;
    }
    if !nvidia_driver_loaded() {
        return None;
    }
    if gdk_uses_wayland() {
        set_quirk_env("__NV_DISABLE_EXPLICIT_SYNC", "1");
        set_quirk_env("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        Some("__NV_DISABLE_EXPLICIT_SYNC=1 WEBKIT_DISABLE_DMABUF_RENDERER=1 (NVIDIA/Wayland)")
    } else {
        set_quirk_env("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        Some("WEBKIT_DISABLE_DMABUF_RENDERER=1 (NVIDIA/X11)")
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

/// Owns the one graph the whole app shares.
///
/// `Mutex` is deliberate: every request is short (a lookup into an in-memory
/// graph) except `index` and `reorg`, and serialising those is exactly what we
/// want — two windows must not build the graph twice. The UI thread never
/// takes this lock; `api` runs the whole call inside `spawn_blocking`.
struct Engine {
    roots: Vec<String>,
    cfg: config::Config,
    index_on_start: bool,
    /// The Stop flag shared with the index. `/api/index/cancel` flips it
    /// *without* taking `inner`: an in-flight build holds that lock for its
    /// whole run, so the cancel request must never wait on it.
    cancel: Arc<AtomicBool>,
    inner: Mutex<EngineState>,
}

struct EngineState {
    index: csgraph_core::index::Index,
    /// Set once the graph is built.
    built: bool,
}

/// `/api/edit` — the app-facing twin of the MCP-only edit tools.
///
/// The MCP tools dispatch in `mcp::resolve_call` before the route layer, so an
/// app click had no way to reach them. `Engine::handle` is the one funnel both
/// the Tauri IPC command and `--serve` share, so the intercept lives there and
/// both host paths get the same command surface an agent has over MCP.
const EDIT_PATH: &str = "/api/edit";

/// `/api/analyze` — the app-facing twin of the agent/CLI analysis actions:
/// the Roslyn `parse_check` validity gate and the `dedupe` duplicate report.
///
/// Like the edit tools these live outside the frozen `/api/*` contract, so the
/// intercept is here rather than in `api.rs`. Both host paths (Tauri IPC and
/// `--serve`) funnel through `Engine::handle` and therefore get them.
const ANALYZE_PATH: &str = "/api/analyze";

/// `/api/preview` — the app-facing twin of the MCP-only `preview_change` tool
/// (M3 slice 2).
///
/// A GET returns the op list; a POST body (or `{args:{...}}`) is handed to
/// [`csgraph_core::preview::invoke`], which is read-only by construction
/// (`apply` is never set, `writes:[]`). The intercept lives in `Engine::handle`
/// so the Tauri IPC path and `--serve` share it, like `/api/edit`.
const PREVIEW_PATH: &str = "/api/preview";

impl Engine {
    fn new(repo: PathBuf, roots: Vec<String>, cfg: config::Config, index_on_start: bool) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut index = csgraph_core::index::Index::new(repo);
        index.set_cancel_flag(cancel.clone());
        Engine {
            roots,
            cfg,
            index_on_start,
            cancel,
            inner: Mutex::new(EngineState { index, built: false }),
        }
    }

    /// Build the graph once, on demand, off the UI thread.
    fn ensure_indexed(state: &mut EngineState, roots: &[String], cfg: &config::Config) {
        if state.built {
            return;
        }
        state.index.build(roots, &cfg.excludes, cfg);
        state.built = true;
    }

    fn handle(&self, method: &str, path: &str, query: &Value, body: Option<Value>) -> Result<Reply, String> {
        // Stop before the lock. An in-flight `/api/index` holds `inner` until
        // its build returns, so a cancel that waited for the lock could only
        // ever fire after the build it was meant to stop. Flipping the shared
        // atomic here is the whole cancellation.
        if method.eq_ignore_ascii_case("POST") && path == api::CANCEL_PATH {
            self.cancel.store(true, Ordering::SeqCst);
            return Ok(Reply {
                status: 200,
                body: api::Payload::Value(json!({"ok": true, "cancelled": true})),
            });
        }
        let mut state = self.inner.lock().map_err(|_| "engine lock poisoned".to_string())?;

        // `--no-index-on-start` mirrors the Python server: start empty and wait
        // for an explicit POST /api/index. Everything else indexes lazily so
        // the window can open before the first parse finishes.
        let wants_index = path == "/api/index";
        if !state.built && (self.index_on_start || wants_index) {
            Self::ensure_indexed(&mut state, &self.roots, &self.cfg);
        }

        // User/agent parity: the MCP-only edit tools reachable from the app.
        // `query` is not consulted here — the body carries everything — and the
        // response is the tool's own JSON body, not the MCP envelope.
        if path == EDIT_PATH {
            return Ok(Self::handle_edit(&mut state, method, body));
        }
        if path == ANALYZE_PATH {
            return Ok(Self::handle_analyze(
                &mut state,
                method,
                body,
                &self.roots,
                &self.cfg.excludes,
            ));
        }
        if path == PREVIEW_PATH {
            return Ok(Self::handle_preview(&mut state, method, body));
        }

        let (status, payload) = api::handle_payload(&mut state.index, method, path, query, body);
        // The tauri-shim reads {status, body}; keep that shape. `Payload` lets
        // a memoized document serialize straight from its cache `Arc`.
        Ok(Reply { status, body: payload })
    }

    /// Answer `GET /api/edit` (the op list) or `POST /api/edit` (one tool).
    ///
    /// The op list comes from [`edit::TOOLS`] so the panel never hardcodes the
    /// names. A POST body is `{"tool": <name>, "args": {...}}`; the tool's
    /// `{"ok": ...}` body is returned unchanged, dry run or apply.
    fn handle_edit(state: &mut EngineState, method: &str, body: Option<Value>) -> Reply {
        let listing = || {
            let tools: Vec<Value> = edit::TOOLS.iter().map(|t| json!(*t)).collect();
            json!({"ok": true, "count": edit::TOOLS.len(), "tools": tools})
        };

        if method.eq_ignore_ascii_case("GET") {
            return Reply { status: 200, body: api::Payload::Value(listing()) };
        }
        if !method.eq_ignore_ascii_case("POST") {
            return Reply {
                status: 200,
                body: api::Payload::Value(json!({
                    "ok": false,
                    "error": format!("edit accepts GET or POST, not {method}"),
                })),
            };
        }

        let body = body.unwrap_or(Value::Null);
        let tool = body.get("tool").and_then(Value::as_str).unwrap_or("").trim().to_string();
        // `tool: "list"` is the same enumeration over POST, for callers that
        // only have a POST helper.
        if tool.is_empty() || tool == "list" || tool == "ops" {
            return Reply { status: 200, body: api::Payload::Value(listing()) };
        }
        if !edit::is_edit_tool(&tool) {
            let mut out = listing();
            out["ok"] = json!(false);
            out["error"] = json!(format!("unknown edit tool: {tool}"));
            return Reply { status: 200, body: api::Payload::Value(out) };
        }

        let args = body.get("args").cloned().unwrap_or_else(|| json!({}));
        // `invoke` wraps the body in the MCP `{content:[{text}], isError}`
        // envelope; unwrap and re-parse so HTTP/IPC see the tool JSON itself.
        let envelope = edit::invoke(&mut state.index, &tool, &args);
        let out = envelope
            .get("content")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .and_then(|t| serde_json::from_str::<Value>(t).ok())
            .unwrap_or_else(|| {
                json!({"ok": false, "error": format!("edit tool {tool} returned a malformed envelope")})
            });
        Reply { status: 200, body: api::Payload::Value(out) }
    }

    /// Answer `GET /api/preview` (the op list) or `POST /api/preview` (one
    /// read-only change preview).
    ///
    /// The op list is the tool's own contract, so the panel never hardcodes it.
    /// A POST body is either the tool arguments directly or `{"args": {...}}`.
    /// The reply is the tool's `{"ok": ...}` body unchanged — never an MCP
    /// envelope — and it can never write.
    fn handle_preview(state: &mut EngineState, method: &str, body: Option<Value>) -> Reply {
        let listing = || {
            json!({
                "ok": true,
                "tool": "preview_change",
                "readOnly": true,
                "ops": ["rename", "retype"],
                "count": 2,
                "note": "Read-only plan: no file is modified and there is no apply path.",
            })
        };
        if method.eq_ignore_ascii_case("GET") {
            return Reply { status: 200, body: api::Payload::Value(listing()) };
        }
        if !method.eq_ignore_ascii_case("POST") {
            return Reply {
                status: 200,
                body: api::Payload::Value(json!({
                    "ok": false,
                    "error": format!("preview accepts GET or POST, not {method}"),
                })),
            };
        }
        let body = body.unwrap_or(Value::Null);
        let args = body
            .get("args")
            .cloned()
            .unwrap_or_else(|| if body.is_object() { body.clone() } else { json!({}) });
        Reply {
            status: 200,
            body: api::Payload::Value(csgraph_core::preview::invoke(&mut state.index, &args)),
        }
    }

    /// Answer `GET /api/analyze` (the op list) or `POST /api/analyze` (one op).
    ///
    /// Built on [`Engine::handle_analyze_with`]; the real Roslyn gate is the
    /// only thing the test seam substitutes.
    fn handle_analyze(
        state: &mut EngineState,
        method: &str,
        body: Option<Value>,
        roots: &[String],
        excludes: &[String],
    ) -> Reply {
        Self::handle_analyze_with(state, method, body, roots, excludes, compile_check::parse_check)
    }

    /// The body of `/api/analyze`, with the parse gate injected so the host
    /// route's clean / diagnostics / skipped handling is testable without a
    /// Roslyn toolchain (a real gate is `compile_check::parse_check`).
    ///
    /// Bodies mirror the CLI/MCP counterparts byte-for-byte:
    /// `parse_check` -> `{ok, toolchain, files, diagnostics, count, skipped}`
    /// and `dedupe` -> `{ok, repo, roots, files, methodBodies, units, minLines,
    /// threshold, exact, near, blocks, stats, truncated}`.
    fn handle_analyze_with(
        state: &mut EngineState,
        method: &str,
        body: Option<Value>,
        roots: &[String],
        excludes: &[String],
        parse: fn(&[String], u64) -> Value,
    ) -> Reply {
        let listing = || json!({"ok": true, "count": 2, "ops": ["parse_check", "dedupe"]});

        if method.eq_ignore_ascii_case("GET") {
            return Reply { status: 200, body: api::Payload::Value(listing()) };
        }
        if !method.eq_ignore_ascii_case("POST") {
            return Reply {
                status: 200,
                body: api::Payload::Value(json!({
                    "ok": false,
                    "error": format!("analyze accepts GET or POST, not {method}"),
                })),
            };
        }

        let body = body.unwrap_or(Value::Null);
        let op = body.get("op").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if op.is_empty() || op == "list" || op == "ops" {
            return Reply { status: 200, body: api::Payload::Value(listing()) };
        }
        let args = body.get("args").cloned().unwrap_or_else(|| json!({}));

        match op.as_str() {
            "parse_check" => {
                // Same default/clamp as `csgraph parse-check` (25 s, 1..=30).
                let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(25).clamp(1, 30);
                let files = analyze_files(state, &args, roots, excludes);
                Reply { status: 200, body: api::Payload::Value(parse(&files, timeout)) }
            }
            "dedupe" => {
                let opts = dedupe_options(&args);
                let repo = state.index.repo_path().to_path_buf();
                let out = dedupe::dedupe(&repo, roots, excludes, &opts);
                Reply { status: 200, body: api::Payload::Value(out) }
            }
            other => {
                let mut out = listing();
                out["ok"] = json!(false);
                out["error"] = json!(format!("unknown analyze op: {other}"));
                Reply { status: 200, body: api::Payload::Value(out) }
            }
        }
    }
}

/// The file list `parse_check` should gate: explicit `args.files` (array or
/// comma string) resolved against the repo, else discovery over the same roots
/// and excludes the graph used. Mirrors `csgraph parse-check`.
fn analyze_files(state: &EngineState, args: &Value, roots: &[String], excludes: &[String]) -> Vec<String> {
    let explicit: Vec<String> = match args.get("files") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        Some(Value::String(s)) => s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
        _ => Vec::new(),
    };
    let repo = state.index.repo_path();
    if explicit.is_empty() {
        let roots: Vec<String> = if roots.is_empty() {
            vec!["_sealed_src".to_string()]
        } else {
            roots.to_vec()
        };
        parser::discover_files(repo, &roots, excludes)
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

/// `DedupeOptions` from an `args` object, defaulting every field.
fn dedupe_options(args: &Value) -> dedupe::DedupeOptions {
    let mut opts = dedupe::DedupeOptions::default();
    if let Some(v) = args.get("min_lines").and_then(Value::as_u64) {
        opts.min_lines = v as usize;
    }
    if let Some(v) = args.get("threshold").and_then(Value::as_f64) {
        opts.threshold = v;
    }
    if let Some(v) = args.get("max_clusters").and_then(Value::as_u64) {
        opts.max_clusters = v as usize;
    }
    if let Some(v) = args.get("max_members").and_then(Value::as_u64) {
        opts.max_members = v as usize;
    }
    if let Some(v) = args.get("max_blocks").and_then(Value::as_u64) {
        opts.max_blocks = v as usize;
    }
    opts
}

/// The `{status, body}` envelope the tauri-shim expects.
#[derive(Serialize)]
pub struct Reply {
    pub status: u16,
    pub body: api::Payload,
}

// ---------------------------------------------------------------------------
// Paths / options
// ---------------------------------------------------------------------------

struct Options {
    repo: Option<PathBuf>,
    roots: Vec<String>,
    selftest: bool,
    no_gpu_quirk: bool,
    no_index_on_start: bool,
    /// `--serve`: run the same engine over a tiny HTTP server instead of a
    /// window, so the frontend can be driven headlessly (tools/headless_check.mjs
    /// and the feature probes). No Tauri, no display server.
    serve: bool,
    port: u16,
    /// `--frontend DIR`: the frontend tree `--serve` should hand out.
    frontend: Option<PathBuf>,
    /// `--open-doc NODE`: open that document in its own window at startup.
    open_docs: Vec<String>,
    /// `--open-pane PANE`: detach that pane into its own window at startup.
    open_panes: Vec<String>,
}

fn parse_args() -> Options {
    let mut opts = Options {
        repo: None,
        roots: Vec::new(),
        selftest: false,
        no_gpu_quirk: false,
        no_index_on_start: false,
        serve: false,
        port: 8799,
        frontend: None,
        open_docs: Vec::new(),
        open_panes: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--repo" => opts.repo = args.next().map(PathBuf::from),
            "--root" => {
                if let Some(r) = args.next() {
                    opts.roots.push(r);
                }
            }
            "--selftest" => opts.selftest = true,
            "--serve" => opts.serve = true,
            "--port" => {
                if let Some(p) = args.next() {
                    opts.port = p.parse().unwrap_or(8799);
                }
            }
            "--frontend" => opts.frontend = args.next().map(PathBuf::from),
            "--no-gpu-quirk" => opts.no_gpu_quirk = true,
            "--no-index-on-start" => opts.no_index_on_start = true,
            "--open-doc" => {
                if let Some(n) = args.next() {
                    opts.open_docs.push(n);
                }
            }
            "--open-pane" => {
                if let Some(p) = args.next() {
                    opts.open_panes.push(p);
                }
            }
            "-h" | "--help" => {
                println!(
                    "CS Viewer v2 (Rust engine)\n\n\
                     Usage: csviewer [--repo PATH] [--root PATH]... [--selftest] [--no-gpu-quirk]\n\
                     \x20                [--no-index-on-start] [--open-doc NODE] [--open-pane PANE]\n\
                     \x20                [--serve [--port N] [--frontend DIR]]\n\n\
                     \x20 --repo PATH        repository/folder to index (default: auto-detected)\n\
                     \x20 --root PATH        index this folder or file (repeatable)\n\
                     \x20 --selftest         start the engine, print /api/state, exit (no window)\n\
                     \x20 --serve            serve the engine + frontend over HTTP (no window);\n\
                     \x20                   used by tools/headless_check.mjs and the probes\n\
                     \x20 --port N           port for --serve (default 8799)\n\
                     \x20 --frontend DIR     frontend directory for --serve\n\
                     \x20 --no-index-on-start  start empty; wait for POST /api/index\n\
                     \x20 --no-gpu-quirk     skip the NVIDIA/Wayland WebKitGTK workaround\n\
                     \x20 --open-doc NODE    also open NODE in its own document window (repeatable)\n\
                     \x20 --open-pane PANE   also detach PANE into its own window (repeatable)\n\
                     \x20                   (side | source | nodeDrawer | findPanel)\n\
                     \x20 -h, --help         show this help"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
    opts
}

fn abs(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| {
        std::env::current_dir().map(|c| c.join(p)).unwrap_or_else(|_| p.to_path_buf())
    })
}

/// Resolve the repository to index.
///
/// Returns `(repo, found)`. `found` is true when the caller named the repo
/// (`--repo` / `CSVIEWER_REPO`) or a repo marker (`.git`, `tools/csgraph`) was
/// found walking up from the working directory. A plain file-manager launch
/// from `$HOME` or `/` finds no marker, so it returns the working directory
/// with `found == false` — and [`resolve_roots`] then refuses to auto-index it,
/// because walking an arbitrary directory can mean scanning the whole home
/// directory or `/`, which looks exactly like the app hanging on "Connecting
/// to server…".
fn resolve_repo(opts: &Options) -> (PathBuf, bool) {
    if let Some(r) = &opts.repo {
        return (abs(r), true);
    }
    if let Ok(r) = std::env::var("CSVIEWER_REPO") {
        if !r.is_empty() {
            return (abs(Path::new(&r)), true);
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut d = cwd.clone();
    loop {
        if d.join("tools/csgraph").is_dir() || d.join(".git").is_dir() {
            return (d, true);
        }
        match d.parent() {
            Some(p) => d = p.to_path_buf(),
            None => break,
        }
    }
    (cwd, false)
}

/// Roots to index: CLI > CSVIEWER_ROOTS > the repo itself.
/// Indexer policy: the v1 config when a checkout is present (v2 reads it, never
/// writes it), else the AppImage's own copy under `$CSVIEWER_HOME`, else the
/// built-in defaults.
fn load_cfg(repo: &Path) -> config::Config {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("CSVIEWER_HOME") {
        if !home.is_empty() {
            candidates.push(Path::new(&home).join("csgraph.config.json"));
        }
    }
    candidates.push(repo.join("tools/csgraph/csgraph.config.json"));
    for path in candidates {
        if path.is_file() {
            return config::load_config(&path);
        }
    }
    config::Config::default()
}

/// Roots to index: CLI > CSVIEWER_ROOTS > config roots that exist > the repo.
/// Returns `(roots, index_on_start)`.
///
/// When no root was named, no config root exists and `repo_found` is false, the
/// app starts empty instead of scanning the working directory. That is the
/// difference between a file-manager launch opening instantly and one that
/// silently tries to index `$HOME` (or `/`) while the canvas shows "Connecting
/// to server…". The user can still index any folder from the UI.
fn resolve_roots(repo: &Path, opts: &Options, cfg: &config::Config, repo_found: bool) -> (Vec<String>, bool) {
    if opts.no_index_on_start {
        return (opts.roots.clone(), false);
    }
    if !opts.roots.is_empty() {
        return (opts.roots.clone(), true);
    }
    if let Ok(v) = std::env::var("CSVIEWER_ROOTS") {
        let roots: Vec<String> = v
            .split(|c: char| c == ':' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !roots.is_empty() {
            return (roots, true);
        }
    }
    let existing: Vec<String> = cfg
        .roots
        .iter()
        .filter(|r| repo.join(r).exists())
        .cloned()
        .collect();
    if !existing.is_empty() {
        return (existing, true);
    }
    if !repo_found {
        return (Vec::new(), false);
    }
    (vec![repo.to_string_lossy().to_string()], true)
}

// ---------------------------------------------------------------------------
// IPC commands
// ---------------------------------------------------------------------------

/// Every `/api/*` call from every window funnels through here.
///
/// The engine work happens inside `spawn_blocking`, so the window that issued
/// the request never blocks its own UI thread and the async runtime is never
/// stalled by a multi-second document synthesis.
#[tauri::command]
async fn api(
    engine: State<'_, Arc<Engine>>,
    method: String,
    path: String,
    query: Value,
    body: Option<Value>,
) -> Result<Reply, String> {
    let engine = engine.inner().clone();
    tauri::async_runtime::spawn_blocking(move || engine.handle(&method, &path, &query, body))
        .await
        .map_err(|e| e.to_string())?
}

static WINDOW_SEQ: AtomicU64 = AtomicU64::new(1);

/// Every pane that can be detached into its own operating-system window.
///
/// The ids are the ones `panels.js` knows: `side` (filters / public definitions
/// / strings / resolve chain), `source` (the Monaco document), `nodeDrawer` and
/// `findPanel`. A detached pane is a real [`WebviewWindow`], not a `<div>` in
/// `#floatLayer`, so it renders outside the main window and can be moved
/// anywhere on the desktop — the Unity editor behaviour.
const PANES: [&str; 4] = ["side", "source", "nodeDrawer", "findPanel"];

/// Detached panes that are open: pane id -> window label.
///
/// A pane has at most one window. Asking for an already-detached pane focuses
/// its window instead of stacking a second copy of the same panel.
#[derive(Default)]
struct OpenPanes(Mutex<HashMap<String, String>>);

fn pane_label(pane: &str) -> String {
    format!("pane-{pane}")
}

fn pane_title(pane: &str) -> &'static str {
    match pane {
        "side" => "Sidebar",
        "source" => "Source",
        "nodeDrawer" => "Node",
        "findPanel" => "Find",
        _ => "Pane",
    }
}

/// `(inner_size, min_inner_size)` for a detached pane.
fn pane_size(pane: &str) -> ((f64, f64), (f64, f64)) {
    match pane {
        "source" => ((1180.0, 860.0), (520.0, 320.0)),
        "side" => ((430.0, 900.0), (300.0, 360.0)),
        "nodeDrawer" => ((440.0, 620.0), (320.0, 240.0)),
        "findPanel" => ((560.0, 720.0), (340.0, 280.0)),
        _ => ((720.0, 640.0), (320.0, 260.0)),
    }
}

/// Where a detached window should open: beside the main window when its monitor
/// has room, cascaded when it does not, otherwise centred (the caller's
/// fallback).
///
/// A pane window that opened on top of the main window would look exactly like
/// the in-page floating pane this replaces — the point of detaching is that it
/// lives *outside* the main window.
///
/// `outer_position()`/`outer_size()` are the authority, but on Wayland a window
/// that has not been mapped yet can refuse them; the monitor is then used
/// directly so a pane never silently falls back to "centred over the main
/// window".
fn beside_main(app: &tauri::AppHandle, w: f64, h: f64, cascade: u64) -> Option<(f64, f64)> {
    let main = app.get_webview_window("main");
    let step = (cascade % 6) as f64 * 32.0;

    let monitor = main
        .as_ref()
        .and_then(|m| m.current_monitor().ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten());

    let scale = monitor
        .as_ref()
        .map(|m| m.scale_factor())
        .or_else(|| main.as_ref().and_then(|m| m.scale_factor().ok()))
        .unwrap_or(1.0);

    // Monitor rectangle (logical pixels).
    let mon = monitor.map(|m| {
        let p = m.position();
        let s = m.size();
        (
            p.x as f64 / scale,
            p.y as f64 / scale,
            s.width as f64 / scale,
            s.height as f64 / scale,
        )
    });

    // Main-window rectangle (logical pixels), when the window manager tells us.
    let main_rect = main.as_ref().and_then(|m| {
        let pos = m.outer_position().ok()?;
        let size = m.outer_size().ok()?;
        Some((
            pos.x as f64 / scale,
            pos.y as f64 / scale,
            size.width as f64 / scale,
            size.height as f64 / scale,
        ))
    });

    let (mx, my, mw, mh) = match (main_rect, mon) {
        (Some(rect), _) => rect,
        // No main-window geometry: place along the monitor instead of guessing.
        (None, Some((px, py, _, sh))) => (px, py, 0.0, sh * 0.6),
        (None, None) => return None,
    };

    // Preferred: a column just to the right of the main window.
    let mut x = mx + mw + 10.0 + step;
    let mut y = my + 28.0 + step;

    if let Some((px, py, sw, sh)) = mon {
        if x + w > px + sw - 8.0 {
            // No room to the right: cascade from the main window's top-left, or
            // below it when the monitor is tall enough.
            x = (mx + 48.0 + step).min(px + sw - w - 8.0).max(px + 8.0);
            let below = my + mh + 10.0 + step;
            y = if below + h <= py + sh - 48.0 {
                below
            } else {
                (my + 48.0 + step).min((py + sh - h - 48.0).max(py + 8.0))
            };
        }
        x = x.max(px + 8.0);
        y = y.max(py + 8.0);
    }
    Some((x, y))
}

/// Everything needed to bring up one secondary window.
struct Spawn {
    label: String,
    query: String,
    init: String,
    title: String,
    size: (f64, f64),
    min: (f64, f64),
    /// `Some(pane id)` for a detached pane, `None` for a document window.
    pane: Option<String>,
    beside_main: bool,
}

/// Build, show, focus and instrument one secondary window.
///
/// Notes that matter on Linux/WebKitGTK:
///
///   * no `.incognito(true)` — wry maps that to `WebContext::new_ephemeral()`,
///     and a second ephemeral context on WebKitGTK does not get its own frame,
///     so the window stayed blank until the main window happened to repaint.
///     Sharing the default `WebContext` is the tested multi-window path;
///   * the window is shown, un-minimised and focused explicitly, because a
///     secondary webview is only composited once its window is mapped;
///   * `.position()` instead of `.center()` puts it beside the main window
///     rather than on top of it.
fn spawn_window(app: &tauri::AppHandle, spec: Spawn) -> Result<WebviewWindow, String> {
    let mut builder = WebviewWindowBuilder::new(app, &spec.label, WebviewUrl::App(spec.query.into()))
        .title(spec.title)
        .inner_size(spec.size.0, spec.size.1)
        .min_inner_size(spec.min.0, spec.min.1)
        .resizable(true)
        .initialization_script(spec.init)
        .visible(true)
        .focused(true);

    let cascade = WINDOW_SEQ.fetch_add(1, Ordering::Relaxed);
    let placed = if spec.beside_main {
        beside_main(app, spec.size.0, spec.size.1, cascade)
    } else {
        None
    };
    builder = match placed {
        Some((x, y)) => {
            note!("[csviewer] {} opened at {x:.0},{y:.0}", spec.label);
            builder.position(x, y)
        }
        None => {
            note!("[csviewer] {} centred (no main-window geometry)", spec.label);
            builder.center()
        }
    };

    let window = builder
        .build()
        .map_err(|e| format!("could not open the window: {e}"))?;

    // Belt and braces: show, un-minimise and focus the new window so its webview
    // receives a first paint even on a busy compositor.
    let _ = window.show();
    let _ = window.unminimize();
    let _ = window.set_focus();

    attach_close_signal(app, &window, spec.pane);
    Ok(window)
}

/// Tell every other window when a secondary window goes away, and forget a
/// detached pane so it can be opened again.
///
/// `Destroyed` (not `CloseRequested`) is the right hook: a close request can be
/// vetoed, and the main window must redock the pane exactly when its window is
/// really gone.
fn attach_close_signal(app: &tauri::AppHandle, window: &WebviewWindow, pane: Option<String>) {
    let app = app.clone();
    let label = window.label().to_string();
    window.on_window_event(move |event| {
        if !matches!(event, tauri::WindowEvent::Destroyed) {
            return;
        }
        if let Some(pane) = pane.as_deref() {
            if let Some(reg) = app.try_state::<OpenPanes>() {
                if let Ok(mut open) = reg.0.lock() {
                    open.remove(pane);
                }
            }
        }
        let kind = if pane.is_some() { "pane" } else { "doc" };
        let _ = app.emit(
            "cs:win-closed",
            json!({ "label": label, "kind": kind, "pane": pane }),
        );
    });
}

/// Open `node` (or a `file:line` usage site) in its own native window: its own
/// webview, its own JS main thread, its own Monaco worker.
///
/// The target is delivered through an `initialization_script` rather than only
/// the URL. A Tauri `WebviewUrl::App` path is run through `Url::join`, which is
/// free to normalise or drop a query string; when that happened the window came
/// up with no target and rendered nothing. The injected `window.__CS_DOC__` is
/// set before any page script runs, so `windows.js` always knows what to show.
/// The query is kept as a fallback for the plain-web build.
#[tauri::command]
async fn open_doc_window(
    app: tauri::AppHandle,
    node: Option<String>,
    file: Option<String>,
    line: Option<i64>,
    mode: Option<String>,
) -> Result<String, String> {
    spawn_doc(&app, node, file, line, mode)
}

/// Synchronous core of [`open_doc_window`], so `setup` can use it too.
fn spawn_doc(
    app: &tauri::AppHandle,
    node: Option<String>,
    file: Option<String>,
    line: Option<i64>,
    mode: Option<String>,
) -> Result<String, String> {
    let n = WINDOW_SEQ.fetch_add(1, Ordering::Relaxed);
    let label = format!("doc-{n}");

    let node = node.filter(|s| !s.is_empty());
    let file = file.filter(|s| !s.is_empty());
    let mode = match mode.as_deref() {
        Some("raw") | Some("effective") | Some("doc") => mode.as_deref().unwrap_or("doc"),
        _ => "doc",
    };

    let title = node
        .as_deref()
        .or(file.as_deref())
        .unwrap_or("document")
        .to_string();
    let short: String = title.chars().take(80).collect();

    // Query string: a human-readable URL for the web build and for debugging.
    let mut q = String::from("index.html?win=doc");
    if let Some(node) = node.as_ref() {
        q.push_str("&node=");
        q.push_str(&urlencode(node));
    }
    if let Some(file) = file.as_ref() {
        q.push_str("&file=");
        q.push_str(&urlencode(file));
    }
    if let Some(line) = line.filter(|l| *l > 0) {
        q.push_str("&line=");
        q.push_str(&line.to_string());
    }
    q.push_str("&mode=");
    q.push_str(mode);

    // The authoritative target, injected before page scripts run.
    let doc = json!({
        "node": node,
        "file": file,
        "line": line.unwrap_or(0),
        "mode": mode,
        "win": "doc",
    });
    let init = format!("window.__CS_DOC__ = {doc};");

    spawn_window(
        app,
        Spawn {
            label: label.clone(),
            query: q,
            init,
            title: format!("CS Viewer — {short}"),
            size: (1180.0, 860.0),
            min: (640.0, 400.0),
            pane: None,
            beside_main: true,
        },
    )?;
    Ok(label)
}

/// Detach `pane` into its own native window, or focus the one that already
/// exists. `state` is the pane's current view state (focus node, source
/// target, find query, filters, theme); it is handed over through
/// `window.__CS_WIN__` so the window paints the same thing the main window
/// showed, and later updates arrive over the `cs:sync` event.
#[tauri::command]
async fn open_pane_window(
    app: tauri::AppHandle,
    pane: String,
    state: Option<Value>,
) -> Result<String, String> {
    spawn_pane(&app, pane, state)
}

fn spawn_pane(app: &tauri::AppHandle, pane: String, state: Option<Value>) -> Result<String, String> {
    if !PANES.contains(&pane.as_str()) {
        return Err(format!("unknown pane '{pane}'"));
    }
    let label = pane_label(&pane);
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.show();
        let _ = existing.unminimize();
        let _ = existing.set_focus();
        return Ok(label);
    }

    let init = format!(
        "window.__CS_WIN__ = {};",
        json!({ "kind": "pane", "pane": pane, "state": state, "win": "pane" })
    );
    let query = format!("index.html?win=pane&pane={}", urlencode(&pane));
    let (size, min) = pane_size(&pane);

    spawn_window(
        app,
        Spawn {
            label: label.clone(),
            query,
            init,
            title: format!("CS Viewer — {}", pane_title(&pane)),
            size,
            min,
            pane: Some(pane.clone()),
            beside_main: true,
        },
    )?;

    if let Some(reg) = app.try_state::<OpenPanes>() {
        if let Ok(mut open) = reg.0.lock() {
            open.insert(pane.clone(), label.clone());
        }
    }
    let _ = app.emit("cs:pane-open", json!({ "pane": pane, "label": label }));
    Ok(label)
}

/// Redock a detached pane by closing its window. The window's `Destroyed`
/// handler sends `cs:win-closed`, which is what actually restores the pane in
/// the main window — so this is safe to call for a window that is already gone.
#[tauri::command]
async fn close_pane_window(app: tauri::AppHandle, pane: String) -> Result<(), String> {
    let label = pane_label(&pane);
    if let Some(window) = app.get_webview_window(&label) {
        let _ = window.close();
    }
    Ok(())
}

/// Minimal percent-encoding for a query value (unreserved characters kept).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------

/// Headless check of the whole engine path: no window, no display server.
fn selftest(opts: &Options) -> Result<(), String> {
    let repo = resolve_repo(opts);
    let cfg = load_cfg(&repo.0);
    let (roots, index_on_start) = resolve_roots(&repo.0, opts, &cfg, repo.1);
    note!("[selftest] repo={} roots={:?} index_on_start={}", repo.0.display(), roots, index_on_start);
    let engine = Engine::new(repo.0, roots, cfg, index_on_start);
    let reply = engine.handle("GET", "/api/state", &json!({}), None)?;
    // `Reply` is a Serialize struct now (the body may be a shared cached
    // document); the selftest inspects it as a Value.
    let reply = serde_json::to_value(&reply).map_err(|e| e.to_string())?;
    println!("{}", serde_json::to_string_pretty(&reply).unwrap_or_else(|_| reply.to_string()));
    let ok = reply.get("body").and_then(|b| b.get("ok")).and_then(Value::as_bool).unwrap_or(false);
    let status = reply.get("status").and_then(Value::as_u64).unwrap_or(0);
    if ok && status == 200 { Ok(()) } else { Err(format!("unexpected reply: {reply}")) }
}

// ---------------------------------------------------------------------------
// Dev HTTP server (`--serve`)
// ---------------------------------------------------------------------------
//
// The shipped app has no HTTP server: every /api/* fetch goes over Tauri IPC.
// That makes headless verification of the frontend impossible, so `--serve`
// runs the SAME `Engine` behind a minimal HTTP server and hands out
// `frontend/` for everything else. The frontend needs no change — it already
// falls through to real `fetch` when `window.__TAURI__` is absent.
//
// This is a development/verification surface, not the product: one thread per
// connection, no keep-alive, no TLS, bound to 127.0.0.1.

fn serve(opts: &Options) -> Result<(), String> {
    let (repo, repo_found) = resolve_repo(opts);
    let cfg = load_cfg(&repo);
    let (roots, index_on_start) = resolve_roots(&repo, opts, &cfg, repo_found);
    let frontend = find_frontend(opts.frontend.as_deref())?;
    note!(
        "[serve] repo={} roots={:?} frontend={}",
        repo.display(),
        roots,
        frontend.display()
    );
    let engine = Arc::new(Engine::new(repo, roots, cfg, index_on_start));
    let listener = std::net::TcpListener::bind(("127.0.0.1", opts.port))
        .map_err(|e| format!("bind 127.0.0.1:{}: {e}", opts.port))?;
    note!(
        "[serve] listening on http://127.0.0.1:{}/ (Ctrl-C to stop)",
        opts.port
    );
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let engine = engine.clone();
        let frontend = frontend.clone();
        std::thread::spawn(move || {
            if let Err(e) = serve_conn(stream, &engine, &frontend) {
                note!("[serve] connection error: {e}");
            }
        });
    }
    Ok(())
}

/// The frontend tree: `--frontend`, else `../../rust-only-csviewer/frontend`
/// relative to the executable, else a few common relative locations.
fn find_frontend(explicit: Option<&Path>) -> Result<PathBuf, String> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = explicit {
        candidates.push(p.to_path_buf());
    }
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or(Path::new(".")).to_path_buf();
        candidates.push(dir.join("../rust-only-csviewer/frontend"));
        candidates.push(dir.join("../../rust-only-csviewer/frontend"));
    }
    candidates.push(PathBuf::from("../frontend"));
    candidates.push(PathBuf::from("frontend"));
    for c in &candidates {
        if c.join("index.html").is_file() {
            return Ok(std::fs::canonicalize(c).unwrap_or_else(|_| c.clone()));
        }
    }
    Err(format!(
        "frontend not found (looked in {})",
        candidates
            .iter()
            .map(|c| c.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn serve_conn(
    stream: std::net::TcpStream,
    engine: &Engine,
    frontend: &Path,
) -> std::io::Result<()> {
    use std::io::{BufRead, BufReader, Read};

    stream.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();

    let mut length = 0usize;
    let mut content_type = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        let lower = trimmed.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        }
        if let Some(v) = lower.strip_prefix("content-type:") {
            content_type = v.trim().to_string();
        }
    }
    let mut body_bytes = vec![0u8; length.min(64 * 1024 * 1024)];
    if !body_bytes.is_empty() {
        reader.read_exact(&mut body_bytes)?;
    }

    let (path, raw_query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };

    // /api/* -> the engine.
    if path.starts_with("/api/") {
        let query = query_object(&raw_query);
        let body: Option<Value> = if content_type.contains("json") && !body_bytes.is_empty() {
            serde_json::from_slice(&body_bytes).ok()
        } else {
            None
        };
        let reply = engine.handle(&method, &path, &query, body);
        let (status, payload) = match reply {
            Ok(r) => {
                let text = serde_json::to_vec(&r.body).unwrap_or_else(|_| b"{}".to_vec());
                return respond(stream, r.status, "application/json; charset=utf-8", &text);
            }
            Err(e) => (500u16, e),
        };
        let text = serde_json::to_vec(&serde_json::json!({"ok": false, "error": payload}))
            .unwrap_or_else(|_| b"{}".to_vec());
        return respond(stream, status, "application/json; charset=utf-8", &text);
    }

    // Everything else -> the frontend tree.
    let rel = percent_decode_path(&path);
    let rel = rel.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let joined = frontend.join(rel);
    let safe = std::fs::canonicalize(&joined).ok();
    match safe {
        Some(p) if p.starts_with(frontend) && p.is_file() => {
            let bytes = std::fs::read(&p)?;
            respond(stream, 200, mime_of(&p), &bytes)
        }
        _ => respond(stream, 404, "text/plain; charset=utf-8", b"not found"),
    }
}

fn respond(
    mut stream: std::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn query_object(raw: &str) -> Value {
    let mut map = serde_json::Map::new();
    for part in raw.split('&') {
        if part.is_empty() {
            continue;
        }
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k, v),
            None => (part, ""),
        };
        // Leave values percent-encoded: `api::query_map` decodes them once.
        if !k.is_empty() {
            map.insert(k.to_string(), Value::String(v.to_string()));
        }
    }
    Value::Object(map)
}

fn percent_decode_path(path: &str) -> String {
    api::percent_decode(path)
}

fn mime_of(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "ico" => "image/x-icon",
        "txt" | "map" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------------
// Open in the real IDE
// ---------------------------------------------------------------------------

/// `open_external(file, line)` — the honest bridge out of the viewer.
///
/// `CSVIEWER_EDITOR` overrides the editor. If it contains `{file}`/`{line}` the
/// whole string is treated as a command line with those placeholders; otherwise
/// the file is appended. With no override the first editor found on `PATH`
/// wins: `code -g file:line`, then `codium`/`code-insiders`, then
/// `rider`/`idea --line N file`.
#[tauri::command]
fn open_external(file: String, line: Option<i64>) -> Result<String, String> {
    let file = file.trim().to_string();
    if file.is_empty() {
        return Err("no file to open".into());
    }
    let line = line.unwrap_or(1).max(1);
    let (program, args) = editor_invocation(&file, line);
    std::process::Command::new(&program)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| format!("{program} {}", args.join(" ")))
        .map_err(|e| format!("could not launch {program}: {e}"))
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|p| p.join(program).is_file()))
        .unwrap_or(false)
}

/// The environment/PATH-bound editor resolution. This is the only piece that
/// touches process state, so it stays a thin wrapper over
/// [`editor_invocation_spec`], which is pure and therefore unit-testable.
fn editor_invocation(file: &str, line: i64) -> (String, Vec<String>) {
    let spec = std::env::var("CSVIEWER_EDITOR").ok();
    editor_invocation_spec(file, line, spec.as_deref(), which)
}

/// Pure editor resolution: given an optional `CSVIEWER_EDITOR` spec and a
/// `have(program)` PATH probe, return the program and its argv.
///
/// * `spec` with `{file}`/`{line}` is a command line whose placeholders are
///   substituted; without placeholders the file is appended as the last arg.
/// * a missing/blank `spec` falls back to PATH: `code -g file:line`, then
///   `codium`/`code-insiders`, then `rider`/`idea --line N file`, and finally
///   the documented `code -g file:line` default even when nothing is found.
/// * `line` is clamped to `>= 1` so a bad caller never yields `file:0`.
fn editor_invocation_spec(
    file: &str,
    line: i64,
    spec: Option<&str>,
    have: impl Fn(&str) -> bool,
) -> (String, Vec<String>) {
    let line = line.max(1);
    if let Some(spec) = spec {
        let spec = spec.trim();
        if !spec.is_empty() {
            let mut parts: Vec<String> = spec.split_whitespace().map(str::to_string).collect();
            let program = parts.remove(0);
            if parts
                .iter()
                .any(|a| a.contains("{file}") || a.contains("{line}"))
            {
                let args = parts
                    .iter()
                    .map(|a| a.replace("{file}", file).replace("{line}", &line.to_string()))
                    .collect();
                return (program, args);
            }
            let mut args = parts;
            args.push(file.to_string());
            return (program, args);
        }
    }
    for cand in ["code", "codium", "code-insiders"] {
        if have(cand) {
            return (cand.to_string(), vec!["-g".to_string(), format!("{file}:{line}")]);
        }
    }
    for cand in ["rider", "idea"] {
        if have(cand) {
            return (
                cand.to_string(),
                vec!["--line".to_string(), line.to_string(), file.to_string()],
            );
        }
    }
    ("code".to_string(), vec!["-g".to_string(), format!("{file}:{line}")])
}

fn main() {
    let opts = parse_args();
    let (repo, repo_found) = resolve_repo(&opts);

    if opts.serve {
        match serve(&opts) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                note!("serve failed: {e}");
                std::process::exit(1);
            }
        }
    }

    if opts.selftest {
        match selftest(&opts) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                note!("selftest failed: {e}");
                std::process::exit(1);
            }
        }
    }

    // Must run before Tauri initialises GTK/WebKit.
    if !opts.no_gpu_quirk {
        if let Some(applied) = apply_gpu_quirk() {
            note!("[csviewer] NVIDIA/Wayland quirk applied: {applied}");
        }
    }

    let cfg = load_cfg(&repo);
    let (roots, index_on_start) = resolve_roots(&repo, &opts, &cfg, repo_found);
    note!("[csviewer] repo={} roots={:?}", repo.display(), roots);
    let startup_windows = (opts.open_docs.clone(), opts.open_panes.clone());

    tauri::Builder::default()
        .setup(move |app| {
            app.manage(Arc::new(Engine::new(
                repo.clone(),
                roots.clone(),
                cfg.clone(),
                index_on_start,
            )));
            app.manage(OpenPanes::default());
            // `--open-doc` / `--open-pane`: the same secondary windows the UI
            // opens, requested on the command line. Useful on its own (start with
            // a document already popped out) and the only way to drive a window
            // on a Wayland session from a script, where synthetic input does not
            // reach the app.
            //
            // Off the setup thread on purpose: the window declared in
            // tauri.conf.json does not exist yet here, and `beside_main` needs it
            // to place a pane *outside* the main window rather than on top of it.
            if !startup_windows.0.is_empty() || !startup_windows.1.is_empty() {
                let handle = app.handle().clone();
                let (docs, panes) = startup_windows.clone();
                std::thread::spawn(move || {
                    for _ in 0..60 {
                        if handle.get_webview_window("main").is_some() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    for node in docs {
                        if let Err(e) = spawn_doc(&handle, Some(node), None, None, None) {
                            note!("[csviewer] --open-doc failed: {e}");
                        }
                    }
                    for pane in panes {
                        if let Err(e) = spawn_pane(&handle, pane, None) {
                            note!("[csviewer] --open-pane failed: {e}");
                        }
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            api,
            open_doc_window,
            open_pane_window,
            close_pane_window,
            open_external
        ])
        // Closing the main window closes the viewer, even when a document or a
        // detached pane still has a window open: those are views of this app, and
        // leaving them running with no main window is a dead end.
        .on_window_event(|window, event| {
            if window.label() == "main" && matches!(event, tauri::WindowEvent::Destroyed) {
                window.app_handle().exit(0);
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running CS Viewer v2");
}

#[cfg(test)]
mod tests {
    use super::editor_invocation_spec;

    /// Build the exact `Vec<String>` argv the resolver must return.
    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    /// PATH probe that reports exactly `names` as present.
    fn only(names: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |p: &str| names.contains(&p)
    }

    /// PATH probe that reports nothing.
    fn none() -> impl Fn(&str) -> bool {
        |_: &str| false
    }

    #[test]
    fn code_on_path_uses_goto() {
        let (program, args) = editor_invocation_spec("/w/a.cs", 7, None, only(&["code"]));
        assert_eq!(program, "code");
        assert_eq!(args, v(&["-g", "/w/a.cs:7"]));
    }

    #[test]
    fn line_is_clamped_to_at_least_one() {
        let (_, zero) = editor_invocation_spec("/w/a.cs", 0, None, only(&["code"]));
        assert_eq!(zero, v(&["-g", "/w/a.cs:1"]));
        let (_, negative) = editor_invocation_spec("/w/a.cs", -5, None, only(&["code"]));
        assert_eq!(negative, v(&["-g", "/w/a.cs:1"]));
        let (_, placeholdered) =
            editor_invocation_spec("/w/a.cs", 0, Some("myedit {file}:{line}"), none());
        assert_eq!(placeholdered, v(&["/w/a.cs:1"]));
    }

    #[test]
    fn vscode_forks_fall_back_in_order() {
        // `code` wins even when every candidate is present.
        let (program, args) =
            editor_invocation_spec("/w/a.cs", 3, None, only(&["codium", "code-insiders", "code"]));
        assert_eq!(program, "code");
        assert_eq!(args, v(&["-g", "/w/a.cs:3"]));

        let (program, args) = editor_invocation_spec("/w/a.cs", 3, None, only(&["codium"]));
        assert_eq!(program, "codium");
        assert_eq!(args, v(&["-g", "/w/a.cs:3"]));

        let (program, args) = editor_invocation_spec("/w/a.cs", 3, None, only(&["code-insiders"]));
        assert_eq!(program, "code-insiders");
        assert_eq!(args, v(&["-g", "/w/a.cs:3"]));
    }

    #[test]
    fn rider_and_idea_use_line_flag() {
        let (program, args) =
            editor_invocation_spec("/w/a.cs", 9, None, only(&["idea", "rider"]));
        assert_eq!(program, "rider");
        assert_eq!(args, v(&["--line", "9", "/w/a.cs"]));

        let (program, args) = editor_invocation_spec("/w/a.cs", 9, None, only(&["idea"]));
        assert_eq!(program, "idea");
        assert_eq!(args, v(&["--line", "9", "/w/a.cs"]));
    }

    #[test]
    fn nothing_on_path_falls_back_to_documented_code_goto() {
        let (program, args) = editor_invocation_spec("/w/a.cs", 12, None, none());
        assert_eq!(program, "code");
        assert_eq!(args, v(&["-g", "/w/a.cs:12"]));
    }

    #[test]
    fn editor_spec_placeholders_are_substituted_program_first() {
        let (program, args) =
            editor_invocation_spec("/w/a.cs", 42, Some("myedit {file}:{line}"), none());
        assert_eq!(program, "myedit");
        assert_eq!(args, v(&["/w/a.cs:42"]));

        let (program, args) = editor_invocation_spec(
            "/w/a.cs",
            42,
            Some("myedit --goto {file} --line {line}"),
            none(),
        );
        assert_eq!(program, "myedit");
        assert_eq!(args, v(&["--goto", "/w/a.cs", "--line", "42"]));

        // Only one of the two placeholders is also fine.
        let (_, file_only) = editor_invocation_spec("/w/a.cs", 42, Some("myedit {file}"), none());
        assert_eq!(file_only, v(&["/w/a.cs"]));
        let (_, line_only) = editor_invocation_spec("/w/a.cs", 42, Some("myedit +{line}"), none());
        assert_eq!(line_only, v(&["+42"]));
    }

    #[test]
    fn editor_spec_without_placeholders_appends_file_last() {
        let (program, args) =
            editor_invocation_spec("/w/a.cs", 5, Some("myedit --reuse-window"), none());
        assert_eq!(program, "myedit");
        assert_eq!(args, v(&["--reuse-window", "/w/a.cs"]));

        // A bare program gets exactly one argument.
        let (program, args) = editor_invocation_spec("/w/a.cs", 5, Some("myedit"), none());
        assert_eq!(program, "myedit");
        assert_eq!(args, v(&["/w/a.cs"]));
    }

    #[test]
    fn blank_editor_spec_falls_through_to_path() {
        for spec in ["", "   ", "\t \n"] {
            let (program, args) = editor_invocation_spec("/w/a.cs", 5, Some(spec), only(&["code"]));
            assert_eq!(program, "code", "spec {spec:?} must fall through");
            assert_eq!(args, v(&["-g", "/w/a.cs:5"]));
        }
    }

    #[test]
    fn path_with_spaces_stays_one_argv_entry() {
        let file = "/w/dir with spaces/my file.cs";

        let (program, args) =
            editor_invocation_spec(file, 8, Some("myedit --goto {file} --line {line}"), none());
        assert_eq!(program, "myedit");
        assert_eq!(args, v(&["--goto", file, "--line", "8"]));
        assert_eq!(args.len(), 4, "spaces must not split into extra argv entries");
        assert_eq!(args[1], file);

        let (_, appended) = editor_invocation_spec(file, 8, Some("myedit"), none());
        assert_eq!(appended, v(&[file]));

        let (_, goto) = editor_invocation_spec(file, 8, None, only(&["code"]));
        assert_eq!(goto, v(&["-g", "/w/dir with spaces/my file.cs:8"]));
    }

    /// Real launch: a generated fake editor script records its own argv, so the
    /// proof is an actual spawned process, not a stubbed Tauri invoke.
    ///
    /// This is the only test that touches `CSVIEWER_EDITOR`; the guard keeps the
    /// mutation serialized even if more env-touching tests are added later.
    #[test]
    fn real_launch_records_editor_argv() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::{Duration, Instant};

        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().expect("env lock");

        let dir = std::env::temp_dir().join(format!(
            "csviewer-editor-launch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let record = dir.join("argv.txt");
        let script = dir.join("fake_editor.sh");
        // Bake the record path into the script so the child needs no extra env.
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                record.display()
            ),
        )
        .expect("write fake editor");
        let mut perms = std::fs::metadata(&script).expect("stat script").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod fake editor");

        // The file deliberately contains a space: it must stay one argv entry.
        let file = dir.join("my file.cs");
        let spec = format!("{} {{file}} {{line}}", script.display());

        // SAFETY: `ENV_LOCK` above serializes this single env-mutating test, and
        // the pure resolver tests never read the environment.
        unsafe { std::env::set_var("CSVIEWER_EDITOR", &spec) };

        let launched = super::open_external(file.display().to_string(), Some(42));
        let launched = match launched {
            Ok(line) => line,
            Err(e) => {
                unsafe { std::env::remove_var("CSVIEWER_EDITOR") };
                panic!("open_external failed to launch the fake editor: {e}");
            }
        };

        let deadline = Instant::now() + Duration::from_secs(10);
        let recorded = loop {
            if let Ok(text) = std::fs::read_to_string(&record) {
                if text.lines().count() >= 2 {
                    break text;
                }
            }
            if Instant::now() > deadline {
                unsafe { std::env::remove_var("CSVIEWER_EDITOR") };
                panic!("fake editor never recorded argv (open_external said: {launched})");
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        unsafe { std::env::remove_var("CSVIEWER_EDITOR") };

        let argv: Vec<String> = recorded.lines().map(str::to_string).collect();
        let expected = v(&[file.to_str().expect("utf-8 path"), "42"]);
        assert_eq!(
            argv, expected,
            "recorded argv must be exactly [<path with spaces>, \"42\"]"
        );
        assert_eq!(argv[0].matches(' ').count(), 1);
        assert_eq!(argv.len(), 2, "spaces in the file must not split argv");
        assert!(launched.starts_with(script.to_str().expect("utf-8 script")));

        // Evidence: `cargo test -p csviewer real_launch -- --nocapture`.
        eprintln!("real-launch recorded argv: {argv:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The `/api/edit` host intercept, exercised at the same `Engine::handle_edit`
/// entry both the Tauri IPC command and `--serve` call — no window, no HTTP.
#[cfg(test)]
mod edit_route_tests {
    use super::*;

    fn state() -> EngineState {
        EngineState { index: csgraph_core::index::Index::new(PathBuf::from(".")), built: false }
    }

    fn value(reply: Reply) -> Value {
        match reply.body {
            api::Payload::Value(v) => v,
            _ => panic!("the edit route must return a Value payload"),
        }
    }

    #[test]
    fn get_edit_lists_every_tool_once() {
        let mut st = state();
        let body = value(Engine::handle_edit(&mut st, "GET", None));
        assert_eq!(body["ok"], json!(true));
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), edit::TOOLS.len());
        for name in edit::TOOLS {
            assert!(
                tools.iter().any(|t| t.as_str() == Some(name)),
                "op list is missing {name}"
            );
        }
        assert_eq!(body["count"], json!(edit::TOOLS.len()));
    }

    #[test]
    fn post_edit_list_matches_get() {
        let mut st = state();
        let get = value(Engine::handle_edit(&mut st, "GET", None));
        let post = value(Engine::handle_edit(&mut st, "POST", Some(json!({"tool": "list"}))));
        assert_eq!(get, post);
    }

    #[test]
    fn post_edit_unknown_tool_is_a_body_not_a_transport_error() {
        let mut st = state();
        let reply = Engine::handle_edit(&mut st, "POST", Some(json!({"tool": "nope", "args": {}})));
        assert_eq!(reply.status, 200);
        let body = value(reply);
        assert_eq!(body["ok"], json!(false));
        assert!(body["error"].as_str().unwrap().contains("nope"));
        assert_eq!(body["tools"].as_array().unwrap().len(), edit::TOOLS.len());
    }
}

/// The `/api/preview` host intercept (M3 slice 2): the app-facing twin of the
/// MCP-only `preview_change` tool, exercised at `Engine::handle_preview`.
#[cfg(test)]
mod preview_route_tests {
    use super::*;

    fn state() -> (PathBuf, EngineState) {
        let dir = std::env::temp_dir().join(format!("csviewer-preview-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.cs"),
            "namespace N {\n public partial class C {\n  public int A() { return B(); }\n  public int B() { return 2; }\n }\n}\n",
        )
        .unwrap();
        let mut index = csgraph_core::index::Index::new(dir.clone());
        let cfg = config::Config::default();
        index.build(&[".".to_string()], &[], &cfg);
        (dir, EngineState { index, built: true })
    }

    fn value(reply: Reply) -> Value {
        match reply.body {
            api::Payload::Value(v) => v,
            _ => panic!("the preview route must return a Value payload"),
        }
    }

    #[test]
    fn get_preview_lists_the_read_only_ops() {
        let (_dir, mut st) = state();
        let body = value(Engine::handle_preview(&mut st, "GET", None));
        assert_eq!(body["ok"], json!(true));
        assert_eq!(body["readOnly"], json!(true));
        assert_eq!(body["ops"], json!(["rename", "retype"]));
        assert_eq!(body["tool"], json!("preview_change"));
    }

    #[test]
    fn post_preview_plans_without_writing() {
        let (dir, mut st) = state();
        let before = std::fs::read_to_string(dir.join("a.cs")).unwrap();
        let body = value(Engine::handle_preview(
            &mut st,
            "POST",
            Some(json!({"node": "N.C.B()", "op": "rename", "to": "C"})),
        ));
        assert_eq!(body["ok"], json!(true), "{body}");
        assert_eq!(body["readOnly"], json!(true));
        assert_eq!(body["applied"], json!(false));
        assert_eq!(body["writes"], json!([]));
        assert!(body["proposedSignature"].as_str().unwrap().contains('C'), "{body}");
        assert!(body["counts"]["edits"].as_i64().unwrap() >= 1, "{body}");
        // `{args: ...}` is unwrapped like the edit route.
        let wrapped = value(Engine::handle_preview(
            &mut st,
            "POST",
            Some(json!({"args": {"symbol": "N.C.A", "op": "retype", "to": "long"}})),
        ));
        assert_eq!(wrapped["ok"], json!(true), "{wrapped}");
        assert_eq!(wrapped["currentType"], json!("int"), "{wrapped}");
        // A refusal is a 200 body, never a transport error.
        let refused = Engine::handle_preview(
            &mut st,
            "POST",
            Some(json!({"node": "N.C.Missing", "op": "rename", "to": "X"})),
        );
        assert_eq!(refused.status, 200);
        let refused = value(refused);
        assert_eq!(refused["ok"], json!(false));
        assert!(refused["error"].as_str().unwrap().contains("unknown symbol"));
        assert_eq!(std::fs::read_to_string(dir.join("a.cs")).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The `/api/analyze` host intercept. The parse gate is injected (`fn` pointer)
/// so the clean / diagnostics / skipped shapes are provable without a Roslyn
/// toolchain; `dedupe` runs for real against the report shape.
#[cfg(test)]
mod analyze_route_tests {
    use super::*;

    fn state() -> EngineState {
        EngineState { index: csgraph_core::index::Index::new(PathBuf::from(".")), built: false }
    }

    fn value(reply: Reply) -> Value {
        match reply.body {
            api::Payload::Value(v) => v,
            _ => panic!("the analyze route must return a Value payload"),
        }
    }

    fn clean(_files: &[String], _timeout: u64) -> Value {
        json!({"ok": true, "toolchain": "dotnet-roslyn fake 1.0", "files": 3,
               "diagnostics": [], "count": 0, "skipped": Value::Null})
    }

    fn diagnostics(_files: &[String], _timeout: u64) -> Value {
        json!({"ok": false, "toolchain": "dotnet-roslyn fake 1.0", "files": 3,
               "diagnostics": [{"severity": "error", "id": "CS0103", "file": "a.cs",
                                "line": 1, "col": 1, "message": "nope"}],
               "count": 1, "skipped": Value::Null})
    }

    fn skipped(_files: &[String], _timeout: u64) -> Value {
        json!({"ok": false, "toolchain": Value::Null, "files": 0,
               "diagnostics": [], "count": 0, "skipped": "no toolchain"})
    }

    fn post(parse: fn(&[String], u64) -> Value, op: &str) -> Reply {
        let mut st = state();
        Engine::handle_analyze_with(
            &mut st,
            "POST",
            Some(json!({"op": op, "args": {}})),
            &["_sealed_src".to_string()],
            &[],
            parse,
        )
    }

    #[test]
    fn get_analyze_lists_both_ops() {
        let mut st = state();
        let body = value(Engine::handle_analyze_with(
            &mut st, "GET", None, &[], &[], compile_check::parse_check,
        ));
        assert_eq!(body["ok"], json!(true));
        let ops = body["ops"].as_array().expect("ops array");
        assert!(ops.iter().any(|o| o == "parse_check"));
        assert!(ops.iter().any(|o| o == "dedupe"));
    }

    #[test]
    fn clean_gate_is_the_only_ok_true_shape() {
        let body = value(post(clean, "parse_check"));
        assert_eq!(body["ok"], json!(true));
        assert!(body["skipped"].is_null());
        assert_eq!(body["count"], json!(0));
        assert!(body["diagnostics"].as_array().unwrap().is_empty());
    }

    #[test]
    fn diagnostics_are_ok_false_without_a_skip_reason() {
        let body = value(post(diagnostics, "parse_check"));
        assert_eq!(body["ok"], json!(false));
        assert!(body["skipped"].is_null(), "diagnostics must not masquerade as a skip");
        assert_eq!(body["count"], json!(body["diagnostics"].as_array().unwrap().len()));
        assert!(body["count"].as_u64().unwrap() > 0);
    }

    #[test]
    fn skipped_gate_is_ok_false_with_a_reason_and_no_diagnostics() {
        let body = value(post(skipped, "parse_check"));
        assert_eq!(body["ok"], json!(false));
        assert!(body["skipped"].is_string(), "a skip must carry its honest reason");
        assert_eq!(body["count"], json!(0));
        assert!(body["diagnostics"].as_array().unwrap().is_empty());
    }

    #[test]
    fn real_gate_never_reports_ok_true_with_a_skip_reason() {
        let mut st = state();
        // An empty explicit list falls back to discovery, which finds nothing
        // under this crate; the real gate must answer skipped, not clean.
        let body = value(Engine::handle_analyze_with(
            &mut st,
            "POST",
            Some(json!({"op": "parse_check", "args": {"files": []}})),
            &[],
            &[],
            compile_check::parse_check,
        ));
        assert!(body["skipped"].is_null() == (body["ok"] == json!(true)));
    }

    #[test]
    fn unknown_op_is_a_body_not_a_transport_error() {
        let reply = post(clean, "nope");
        assert_eq!(reply.status, 200);
        let body = value(reply);
        assert_eq!(body["ok"], json!(false));
        assert!(body["error"].as_str().unwrap().contains("nope"));
        assert_eq!(body["ops"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn dedupe_route_returns_the_cli_report_shape() {
        let mut st = state();
        let body = value(Engine::handle_analyze_with(
            &mut st,
            "POST",
            Some(json!({"op": "dedupe", "args": {"min_lines": 4}})),
            &[],
            &[],
            compile_check::parse_check,
        ));
        assert_eq!(body["ok"], json!(true));
        for key in ["exact", "near", "blocks", "stats", "truncated", "files", "units"] {
            assert!(body.get(key).is_some(), "dedupe report is missing {key}");
        }
        assert!(body["exact"].is_array() && body["near"].is_array() && body["blocks"].is_array());
        assert!(body["truncated"].is_boolean());
    }
}
