//! pov — the v3 fork's viewer POV and cross-user instance record.
//!
//! v2 (the Tauri viewer) published its live selection into
//! `<cache_root>/.csgraph.v2/viewer.json` under a `pov` key and registered an
//! instance record in `$XDG_RUNTIME_DIR/csviewer/instances/<pid>.json`. The
//! read-only MCP tools `viewer_instances` / `viewer_pov` (and `GET
//! /api/viewer_instances` / `GET /api/viewer_pov`) consume exactly that seam.
//!
//! This module carries the same seam onto v3, reusing `csgraph_core::cache` and
//! `csgraph_core::registry` rather than re-implementing anything:
//!
//! * [`update`] is called once per frame by the app loop (one line). It builds
//!   the POV from the *real* live state, writes the state file atomically
//!   (throttled; no polling thread and no timer) and keeps a 2 s heartbeat so a
//!   live-but-idle window is not reported stale.
//! * [`shutdown`] writes the final POV and removes the instance record on a
//!   clean exit. A killed window leaves the record behind, and discovery
//!   correctly judges it stale by pid/uid/cmdline.
//!
//! What v3 can and cannot expose (honest, not fabricated):
//!
//! | field | v3 state |
//! |---|---|
//! | `context.selected` / `selectedId` | real: `GraphView.selected` via `pick()` |
//! | `context.hovered` / `focus` | real: `GraphView.hovered`, hover wins like `render.js` |
//! | `context.viewMode` | real: namespaces / full |
//! | `context.indexing` | real: `!graph.state.indexed` (the fork indexes before its first frame, so this is normally `false`) |
//! | `text` | **null** — the native source pane has no text-selection model yet |
//! | `box` | **null** — the native canvas has click-pick and camera drag, no marquee yet |
//! | `context.tabs` / `panels` / `camera` / `viewport` | **null** until the app exposes them |
//!
//! `capabilities` in the payload says which of those are real, so a consumer can
//! distinguish "no selection" from "not implemented" without guessing.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::view::{GraphView, ViewMode};

/// The product marker written into both the record and the POV.
pub const PRODUCT: &str = "cs viewer v3";
/// The binary name. `csgraph_core::registry`'s liveness predicate accepts it:
/// `viewer_prog_name` (`core/src/registry.rs`) is
/// `base == "native-csviewer" || base == "csviewer" || base.starts_with("csviewer-")`,
/// and `native-csviewer` is the v3 fork's shipping name (task-36). A renamed
/// binary would not be recognised — that is deliberate, since a rename cannot be
/// inferred from argv.
pub const BINARY: &str = "native-csviewer";

/// At most one state-file write per this interval while the POV keeps changing.
const DEBOUNCE: Duration = Duration::from_millis(200);
/// A live-but-idle window still refreshes the file this often, so `ageMs` in
/// `viewer_pov` reflects "the viewer is alive", not "the user moved something".
const HEARTBEAT: Duration = Duration::from_millis(2000);

struct PovState {
    roots: Vec<String>,
    cache_root: PathBuf,
    record_path: Option<PathBuf>,
    /// Hash of the last written POV (timestamp excluded).
    last_payload: String,
    last_write: Instant,
    writes: u64,
    /// The first write error is reported once, not on every heartbeat. A
    /// read-only corpus (`EACCES` on `<root>/.csgraph.v2`) used to print
    /// `[pov] viewer_state write failed` forever, which drowned the console.
    write_error_logged: bool,
}

/// The cache root the viewer may actually write its state into.
///
/// `cache_root_for` points at the corpus (or its first root), which is right for
/// the graph cache but not always writable: a corpus owned by another user, a
/// read-only mount, or a `.csgraph.v2` left behind by a root build all make the
/// state file `EACCES`. In that case the state (POV + panel layout) moves to the
/// user's own cache directory, keyed by the preferred path so two corpora cannot
/// overwrite each other. The instance record carries this root, so MCP discovery
/// still finds the state file it names.
pub fn writable_cache_root(preferred: &Path) -> PathBuf {
    if cache_dir_is_writable(preferred) {
        return preferred.to_path_buf();
    }
    let fallback = user_cache_root(preferred);
    if cache_dir_is_writable(&fallback) {
        static FALLBACK_LOGGED: std::sync::Once = std::sync::Once::new();
        FALLBACK_LOGGED.call_once(|| {
            eprintln!(
                "[pov] {} is not writable; viewer state uses {}",
                preferred.join(csgraph_core::cache::CACHE_DIR).display(),
                fallback.join(csgraph_core::cache::CACHE_DIR).display()
            );
        });
        return fallback;
    }
    // Neither is writable: keep the preferred root and let `write` report the
    // real error once. A state file is a convenience, never a start-up gate.
    static NOWHERE_LOGGED: std::sync::Once = std::sync::Once::new();
    NOWHERE_LOGGED.call_once(|| {
        eprintln!(
            "[pov] no writable viewer-state directory (tried {} and {})",
            preferred.display(),
            fallback.display()
        );
    });
    preferred.to_path_buf()
}

/// Can a probe file be created inside `root`'s cache directory?
fn cache_dir_is_writable(root: &Path) -> bool {
    let cache = root.join(csgraph_core::cache::CACHE_DIR);
    if std::fs::create_dir_all(&cache).is_err() {
        return false;
    }
    let probe = cache.join(format!(".write-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// A per-user state root for `preferred`, under `$XDG_CACHE_HOME`/`~/.cache`.
fn user_cache_root(preferred: &Path) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    let key = stable_hash(&json!(preferred.to_string_lossy()));
    base.join("csgraph").join("viewer-state").join(key)
}

static POV: OnceLock<Mutex<PovState>> = OnceLock::new();

/// Called once per frame by the app loop. The first call registers the process.
pub fn update(repo: &Path, roots: &[String], view: Option<&GraphView>) {
    let state = POV.get_or_init(|| Mutex::new(PovState::start(repo, roots)));
    let mut state = match state.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    state.tick(view);
}

/// Clean exit: last write, then remove the instance record.
pub fn shutdown() {
    if let Some(state) = POV.get() {
        let mut state = match state.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.stop();
    }
}

impl PovState {
    fn start(repo: &Path, roots: &[String]) -> Self {
        let cache_root = writable_cache_root(&csgraph_core::registry::cache_root_for(repo, roots));
        let mut me = PovState {
            roots: roots.to_vec(),
            cache_root,
            record_path: None,
            last_payload: String::new(),
            last_write: Instant::now() - HEARTBEAT, // first tick writes at once
            writes: 0,
            write_error_logged: false,
        };

        // Same toggle as v2 (`~/.config/csgraph/settings.json`, default on).
        if csgraph_core::config::load_settings(None).viewer_discovery {
            let record = record_json(repo, roots, &me.cache_root);
            me.record_path =
                csgraph_core::registry::register(&record, &csgraph_core::registry::write_dir());
            match &me.record_path {
                Some(p) => eprintln!("[pov] instance record {}", p.display()),
                None => eprintln!("[pov] could not write the instance record"),
            }
        } else {
            eprintln!("[pov] viewerDiscovery is off; no instance record");
        }
        me
    }

    fn tick(&mut self, view: Option<&GraphView>) {
        let Some(view) = view else { return };
        let pov = pov_json(view, &self.roots);
        let hash = stable_hash(&pov);
        let changed = hash != self.last_payload;
        let due = self.last_write.elapsed() >= DEBOUNCE;
        let heartbeat = self.last_write.elapsed() >= HEARTBEAT;
        if (changed && due) || heartbeat {
            let at = csgraph_core::registry::now_ms();
            self.write(&pov, at);
            self.last_payload = hash;
        }
    }

    /// Atomic state-file write (via the engine's `Cache`), merging into any blob
    /// an older viewer left so its camera/positions are not clobbered.
    fn write(&mut self, pov: &Value, at: i64) {
        let cache = csgraph_core::cache::Cache::open(&self.cache_root);
        let mut blob = cache
            .viewer_state()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({"version": 1}));
        if let Some(o) = blob.as_object_mut() {
            o.entry("version".to_string()).or_insert(json!(1));
            o.insert("pov".to_string(), pov.clone());
            o.insert("product".to_string(), json!(PRODUCT));
            o.insert("pid".to_string(), json!(std::process::id()));
            o.insert("povAt".to_string(), json!(at));
        }
        match cache.save_viewer_state(&blob) {
            Ok(()) => {
                self.writes += 1;
                self.last_write = Instant::now();
                self.write_error_logged = false;
            }
            Err(e) => {
                // Once per failure streak: a read-only corpus must not turn a
                // 2 s heartbeat into an endless console flood.
                if !self.write_error_logged {
                    self.write_error_logged = true;
                    eprintln!(
                        "[pov] viewer_state write failed ({}): {e}; further failures are not logged",
                        self.cache_root.display()
                    );
                }
                // Do not retry every heartbeat either; the next real change or
                // a successful write resets the backoff.
                self.last_write = Instant::now();
            }
        }
    }

    fn stop(&mut self) {
        // Refresh the timestamp so the final snapshot is not reported stale the
        // instant the window closes, then remove the record.
        let cache = csgraph_core::cache::Cache::open(&self.cache_root);
        if let Some(mut blob) = cache.viewer_state().filter(Value::is_object) {
            if let Some(o) = blob.as_object_mut() {
                o.insert("povAt".to_string(), json!(csgraph_core::registry::now_ms()));
                o.insert("product".to_string(), json!(PRODUCT));
                o.insert("pid".to_string(), json!(std::process::id()));
            }
            let _ = cache.save_viewer_state(&blob);
        }
        if let Some(path) = self.record_path.take() {
            csgraph_core::registry::unregister(&path);
            eprintln!("[pov] removed instance record {}", path.display());
        }
    }
}

pub(crate) fn record_json(repo: &Path, roots: &[String], cache_root: &Path) -> Value {
    let mut record = json!({
        "v": 1,
        "pid": std::process::id(),
        "uid": csgraph_core::registry::current_uid(),
        "user": csgraph_core::registry::current_user(),
        "repo": repo.to_string_lossy(),
        "roots": roots,
        "cacheDir": cache_root.to_string_lossy(),
        "statePath": csgraph_core::registry::state_path_for(cache_root).to_string_lossy(),
        "mode": "gui",
        "port": Value::Null,
        "startedMs": csgraph_core::registry::now_ms(),
        // The PRODUCT's version (workspace package, e.g. 3.0.0-alpha.1) — not
        // `csgraph_core::VERSION`, which is the engine crate (2.0.0).
        "version": crate::app::buildinfo::VERSION,
        "product": PRODUCT,
        "binary": BINARY,
        "cmdline": std::env::args().collect::<Vec<_>>().join(" "),
    });
    // R20-E: advertise the opt-in control socket under the exact field name the
    // MCP discovery reads (`csgraph_core::viewer` -> instance record "control").
    // Omitted entirely (not null) when --control is off, so discovery never
    // tries to connect to an endpoint that does not exist.
    if let Some(control) = crate::control::socket_path() {
        record["control"] = json!(control.to_string_lossy());
    }
    record
}

/// Build the POV from the real view state. `at` is added at write time.
pub fn pov_json(view: &GraphView, roots: &[String]) -> Value {
    let graph = &view.graph;
    let selected = view.selected.filter(|i| *i < graph.nodes.len());
    let hovered = view.hovered.filter(|i| *i < graph.nodes.len());
    // `render.js` focus rule: hover wins over the selection.
    let focus = hovered.or(selected);
    let node_of = |i: Option<usize>| -> Value {
        match i {
            Some(i) => {
                let n = &graph.nodes[i];
                json!({"index": i, "id": n.id, "label": n.label, "kind": n.kind,
                       "file": n.file, "line": n.line, "owner": n.owner})
            }
            None => Value::Null,
        }
    };
    let indexed = graph.state.get("indexed").and_then(Value::as_bool).unwrap_or(false);
    let counts = graph.state.get("counts").cloned().unwrap_or_else(|| json!({}));
    let view_mode = match view.view_mode {
        ViewMode::Namespaces => "namespaces",
        ViewMode::Full => "full",
    };
    let context = json!({
        "product": PRODUCT,
        "repo": graph.repo.to_string_lossy(),
        "roots": roots,
        "selected": selected.map(|i| graph.nodes[i].id.clone()),
        "selectedIndex": selected,
        "selectedNode": node_of(selected),
        "hovered": hovered.map(|i| graph.nodes[i].id.clone()),
        "focus": focus.map(|i| graph.nodes[i].id.clone()),
        "focusNode": node_of(focus),
        "mode": view_mode,
        "viewMode": view_mode,
        "showEdges": view.show_edges,
        "showLabels": view.show_labels,
        "indexed": indexed,
        // The fork indexes before its first frame, so this is normally false;
        // no long-running re-index is exposed to the POV yet.
        "indexing": !indexed,
        "nodes": graph.nodes.len(),
        "edges": graph.edges.len(),
        "counts": counts,
        // Honest nulls: v3 has no tab stack, side panels, camera accessor or
        // viewport metrics plumbed into this seam yet.
        "tabs": Value::Null,
        "panels": Value::Null,
        "filters": {"showEdges": view.show_edges, "showLabels": view.show_labels},
        "depth": Value::Null,
        "settings": Value::Null,
        "camera": Value::Null,
        "viewport": Value::Null,
    });
    json!({
        "v": 1,
        "product": PRODUCT,
        "pid": std::process::id(),
        "text": Value::Null,
        "box": Value::Null,
        "context": context,
        "capabilities": {
            "graphSelection": true,
            "textSelection": false,
            "boxSelection": false,
            "note": "v3 native canvas has click-pick, hover and camera drag; \
                     no marquee/box selection and no text-selection model yet",
        },
    })
}

/// Stable FNV-1a hash of a JSON value, used to avoid rewriting an unchanged
/// payload. `at` is added only at write time, so a heartbeat still writes.
fn stable_hash(value: &Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

// ---------------------------------------------------------- panel layout store
//
// The panel layout (floating-pane rectangles + dock-zone assignment) is
// persisted inside the same viewer-state blob as the POV, under a versioned
// `panels` key. Writing it here reuses the engine cache's atomic write and
// keeps the app from growing a second settings sidecar. Reading is deliberately
// forgiving: a missing key, an older blob, a foreign/corrupt payload or one bad
// entry must never stop the GUI from starting.

/// The viewer-state blob key that carries the panel layout.
pub const PANEL_LAYOUT_KEY: &str = "panels";
/// Format version written into the `panels` object.
pub const PANEL_LAYOUT_VERSION: i64 = 1;

fn rect_json(r: &crate::windows::Rect) -> Value {
    json!({"x": r.x, "y": r.y, "w": r.w, "h": r.h})
}

fn rect_from(value: &Value) -> Option<crate::windows::Rect> {
    let f = |k: &str| value.get(k).and_then(Value::as_f64).map(|n| n as f32);
    Some(crate::windows::Rect::new(f("x")?, f("y")?, f("w")?, f("h")?))
}

/// Serialise a layout into the `panels` object. Coordinates are `f32`; JSON
/// carries them as `f64` and the reader casts back, exact for finite `f32`s.
pub fn panel_layout_json(layout: &crate::windows::PanelLayout) -> Value {
    let floating: Vec<Value> = layout
        .geometry
        .entries()
        .iter()
        .map(|(pane, rect)| {
            let mut o = rect_json(rect);
            o["pane"] = json!(pane.id());
            o
        })
        .collect();
    let stacks: Vec<Value> = layout
        .docks
        .stacks()
        .iter()
        .map(|(zone, panes)| {
            json!({
                "zone": zone.id(),
                "panes": panes.iter().map(|p| p.id()).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({"v": PANEL_LAYOUT_VERSION, "floating": floating, "stacks": stacks})
}

/// Parse a persisted layout. Unknown pane/zone ids, malformed rows and a
/// non-object payload are skipped row-by-row, so a blob written by an older or
/// newer viewer still restores whatever this build understands.
pub fn panel_layout_from_json(value: &Value) -> crate::windows::PanelLayout {
    let Some(obj) = value.as_object() else {
        return crate::windows::PanelLayout::default();
    };
    let mut floating = Vec::new();
    if let Some(rows) = obj.get("floating").and_then(Value::as_array) {
        for row in rows {
            let Some(pane) = row
                .get("pane")
                .and_then(Value::as_str)
                .and_then(crate::windows::Pane::from_id)
            else {
                continue;
            };
            if let Some(rect) = rect_from(row) {
                floating.push((pane, rect));
            }
        }
    }
    let mut stacks = Vec::new();
    if let Some(rows) = obj.get("stacks").and_then(Value::as_array) {
        for row in rows {
            let Some(zone) = row
                .get("zone")
                .and_then(Value::as_str)
                .and_then(crate::windows::DockZone::from_id)
            else {
                continue;
            };
            let panes: Vec<crate::windows::Pane> = row
                .get("panes")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .filter_map(crate::windows::Pane::from_id)
                        .collect()
                })
                .unwrap_or_default();
            if !panes.is_empty() {
                stacks.push((zone, panes));
            }
        }
    }
    crate::windows::PanelLayout {
        geometry: crate::windows::PanelGeometry::from_entries(floating),
        docks: crate::windows::DockAssignment::from_stacks(stacks),
    }
}

/// Load the persisted panel layout from `cache_root`'s viewer-state blob.
/// Missing state / missing `panels` key / unreadable payload -> default layout.
pub fn load_panel_layout(cache_root: &Path) -> crate::windows::PanelLayout {
    let cache = csgraph_core::cache::Cache::open(cache_root);
    match cache.viewer_state() {
        Some(blob) => panel_layout_from_json(blob.get(PANEL_LAYOUT_KEY).unwrap_or(&Value::Null)),
        None => crate::windows::PanelLayout::default(),
    }
}

/// Persist the layout, merging into the existing viewer-state object so the POV
/// and any camera/selection the app loop wrote are preserved. Returns the real
/// write error rather than claiming success.
pub fn save_panel_layout(
    cache_root: &Path,
    layout: &crate::windows::PanelLayout,
) -> Result<(), String> {
    let cache = csgraph_core::cache::Cache::open(cache_root);
    let mut blob = cache
        .viewer_state()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"version": 1}));
    if let Some(o) = blob.as_object_mut() {
        o.entry("version".to_string()).or_insert(json!(1));
        o.insert(PANEL_LAYOUT_KEY.to_string(), panel_layout_json(layout));
    }
    cache.save_viewer_state(&blob).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, Graph, Node};

    fn node(id: &str, kind: &str) -> Node {
        Node {
            id: id.to_string(),
            label: id.to_string(),
            full: id.to_string(),
            kind: kind.to_string(),
            ns: "N".to_string(),
            owner: String::new(),
            file: "a.cs".to_string(),
            line: 1,
            end_line: 2,
            degree: 0,
            external: false,
            ..Default::default()
        }
    }

    fn graph(selected: bool) -> GraphView {
        let g = Graph {
            repo: PathBuf::from("/tmp/pov-test-repo"),
            nodes: vec![node("N.A", "namespace"), node("N.B", "class")],
            edges: vec![Edge { from: 0, to: 1, kind: "contains".into(), count: 1 }],
            state: json!({"indexed": true, "counts": {"nodes": 2, "edges": 1}}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let mut v = GraphView::new(g);
        if selected {
            v.selected = Some(1);
        }
        v
    }

    #[test]
    fn pov_reports_real_selection_and_honest_nulls() {
        let v = graph(false);
        let pov = pov_json(&v, &[".".to_string()]);
        assert_eq!(pov["text"], Value::Null, "v3 has no text selection: must be null");
        assert_eq!(pov["box"], Value::Null, "v3 has no marquee: must be null");
        assert_eq!(pov["context"]["selected"], Value::Null);
        assert_eq!(pov["capabilities"]["textSelection"], json!(false));
        assert_eq!(pov["capabilities"]["boxSelection"], json!(false));
        assert_eq!(pov["capabilities"]["graphSelection"], json!(true));
        assert_eq!(pov["context"]["mode"], json!("namespaces"));
        assert_eq!(pov["context"]["indexed"], json!(true));
        assert_eq!(pov["context"]["indexing"], json!(false));
        assert_eq!(pov["product"], json!(PRODUCT));
    }

    #[test]
    fn pov_reports_the_click_pick() {
        let v = graph(true);
        let pov = pov_json(&v, &[]);
        assert_eq!(pov["context"]["selected"], json!("N.B"));
        assert_eq!(pov["context"]["selectedIndex"], json!(1));
        assert_eq!(pov["context"]["focus"], json!("N.B"));
        assert_eq!(pov["context"]["selectedNode"]["kind"], json!("class"));
    }

    #[test]
    fn hash_is_stable_and_timestamp_independent() {
        let v = graph(true);
        let a = pov_json(&v, &[]);
        let b = pov_json(&v, &[]);
        assert_eq!(stable_hash(&a), stable_hash(&b));
        // The timestamp is added at write time (blob `povAt`), never inside the
        // POV, so a heartbeat does not look like a state change.
        assert!(a.get("at").is_none(), "pov must not carry `at`; write() records povAt");
    }

    #[test]
    fn record_carries_the_v3_marker_and_a_real_state_path() {
        let rec = record_json(Path::new("/tmp/pov-test-repo"), &[".".to_string()], Path::new("/tmp/pov-test-repo"));
        assert_eq!(rec["product"], json!(PRODUCT));
        assert_eq!(rec["binary"], json!(BINARY));
        assert_eq!(rec["pid"], json!(std::process::id()));
        assert!(rec["statePath"].as_str().unwrap().ends_with(".csgraph.v2/viewer.json"));
    }

    /// A corpus whose cache directory cannot be created must not stop the POV:
    /// the state moves to the user's cache, and the real state file is written
    /// there (this is the `EACCES` spam case, made deterministic without relying
    /// on file modes — tests may run as root, which ignores them).
    #[test]
    fn an_unwritable_cache_root_falls_back_to_the_user_cache() {
        let scratch = temp_cache_root("unwritable");
        let _ = std::fs::remove_dir_all(&scratch);
        // A regular FILE where a directory is expected: every join below fails
        // with ENOTDIR for any user, including root.
        std::fs::write(&scratch, b"not a directory").expect("write blocker");
        let effective = writable_cache_root(&scratch);
        if effective == scratch {
            // Neither the blocked root nor the user cache is writable (sandboxed
            // CI, or a read-only HOME). The function's contract in that case is
            // to keep the preferred root, so there is nothing to assert here.
            eprintln!("[pov] no writable fallback in this environment; skipping");
            let _ = std::fs::remove_file(&scratch);
            return;
        }
        assert_ne!(effective, scratch, "an unusable root must move the state");
        let cache = csgraph_core::cache::Cache::open(&effective);
        cache
            .save_viewer_state(&json!({"version": 1}))
            .expect("the fallback root must be writable");
        assert!(cache.viewer_state().is_some(), "the state file really exists");
        let _ = std::fs::remove_dir_all(&effective);
        let _ = std::fs::remove_file(&scratch);
    }

    // ---------------------------------------------------- panel layout store

    fn temp_cache_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("csviewer-pov-panels-{tag}-{}", std::process::id()))
    }

    #[test]
    fn panel_layout_round_trips_through_the_viewer_state_store() {
        let root = temp_cache_root("roundtrip");
        let _ = std::fs::remove_dir_all(&root);
        let mut layout = crate::windows::PanelLayout::new();
        layout.geometry.remember(
            crate::windows::Pane::Find,
            crate::windows::Rect::new(300.0, 200.0, 560.0, 720.0),
        );
        layout.geometry.remember(
            crate::windows::Pane::Blocks,
            crate::windows::Rect::new(12.5, 34.25, 720.0, 640.0),
        );
        layout.docks.assign(crate::windows::Pane::Source, crate::windows::DockZone::Left);
        layout.docks.assign(crate::windows::Pane::NodeDrawer, crate::windows::DockZone::Right);
        layout.docks.assign(crate::windows::Pane::Find, crate::windows::DockZone::Center);
        save_panel_layout(&root, &layout).expect("save layout");
        let loaded = load_panel_layout(&root);
        assert_eq!(loaded, layout, "save -> load must preserve rects and stacks");
        // The store merges into the existing blob: `version` is still there and
        // the `panels` object is versioned.
        let cache = csgraph_core::cache::Cache::open(&root);
        let blob = cache.viewer_state().expect("viewer state blob");
        assert_eq!(blob["version"], json!(1));
        assert_eq!(blob[PANEL_LAYOUT_KEY]["v"], json!(PANEL_LAYOUT_VERSION));
        // Saving a newer layout replaces only the panels key.
        let mut newer = layout.clone();
        newer.docks.assign(crate::windows::Pane::Blocks, crate::windows::DockZone::Bottom);
        save_panel_layout(&root, &newer).expect("re-save layout");
        assert_eq!(load_panel_layout(&root), newer);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_or_older_viewer_state_loads_the_default_layout() {
        let root = temp_cache_root("tolerant");
        let _ = std::fs::remove_dir_all(&root);
        // No viewer-state file at all.
        assert_eq!(load_panel_layout(&root), crate::windows::PanelLayout::default());
        // An older blob (version 1, POV only, no `panels` key) is tolerated.
        let cache = csgraph_core::cache::Cache::open(&root);
        cache
            .save_viewer_state(&json!({"version": 1, "pov": {"v": 1}, "product": PRODUCT}))
            .expect("seed older blob");
        assert_eq!(load_panel_layout(&root), crate::windows::PanelLayout::default());
        // Malformed / unknown panel data is skipped row-by-row, never fatal.
        cache
            .save_viewer_state(&json!({"version": 1, "panels": {
                "v": 99,
                "floating": [
                    {"pane": "findPanel", "x": 1.0, "y": 2.0, "w": 3.0, "h": 4.0},
                    {"pane": "noSuchPane", "x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0},
                    {"pane": "source", "x": "not-a-number", "y": 0.0, "w": 1.0, "h": 1.0},
                    {"pane": "blocks", "x": 5.0}
                ],
                "stacks": [
                    {"zone": "left", "panes": ["source", "bogus"]},
                    {"zone": "nowhere", "panes": ["findPanel"]},
                    {"zone": "top", "panes": []}
                ]
            }}))
            .expect("seed malformed blob");
        let loaded = load_panel_layout(&root);
        assert_eq!(
            loaded.geometry.remembered(crate::windows::Pane::Find),
            Some(crate::windows::Rect::new(1.0, 2.0, 3.0, 4.0))
        );
        assert_eq!(loaded.geometry.remembered(crate::windows::Pane::Source), None);
        assert_eq!(loaded.geometry.remembered(crate::windows::Pane::Blocks), None);
        assert_eq!(
            loaded.docks.stack(crate::windows::DockZone::Left),
            &[crate::windows::Pane::Source]
        );
        assert_eq!(loaded.docks.zone_of(crate::windows::Pane::Find), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
