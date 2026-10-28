//! `--control <sock>` — the v3 Unix-socket control channel (transport half).
//!
//! This is the missing mechanism behind the owner requirement *"MCP must be able
//! to emulate any human task on CSViewer"* (`notes/v3-control-channel-plan.md`,
//! §2a: Unix domain socket + line-delimited JSON, opt-in, inject at the ACTION
//! layer). This module owns only the transport; the *semantics* live in the host
//! (`main.rs` headless harness / the future winit drain), which is what keeps a
//! socket thread from ever touching `GraphView` or the nzk_ui's thread-local
//! geometry.
//!
//! Protocol (v1): one JSON object per line, request/response correlated by `id`.
//!
//! ```text
//! {"id":7,"op":"click","selector":"#fitBtn"}
//! {"id":7,"ok":true,"op":"click","result":{...},"status":"files 18 ..."}
//! {"id":8,"ok":false,"op":"click","error":{"code":"unknown_selector",...}}
//! ```
//!
//! Threading: `ping` and `state` are answered by the connection thread from a
//! shared [`Snapshot`] (never blocked on a frame). Every other op is pushed
//! through an `mpsc` channel to the main thread, which is the only thread allowed
//! to run actions; its ack travels back on a per-request channel. If the main
//! thread never drains (windowed winit loop, not wired yet), the connection
//! answers `unsupported_in_windowed` after a short timeout rather than hanging —
//! an honest seam, not a fabricated success.
//!
//! Security: opt-in only (`--control` off by default), socket mode `0600`, stale
//! socket path removed before bind. No TCP, no listener otherwise.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

/// How long a main-thread op may wait for its drain before the transport reports
/// the seam honestly. Headless drains immediately; windowed has no drain yet.
const MAIN_THREAD_TIMEOUT: Duration = Duration::from_secs(5);

/// Live state any connection may read without waking the main thread.
///
/// The host refreshes it after each render pass ([`Server::set_snapshot`]), so a
/// `state` request can never observe a half-applied action.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// The exact status-line text (v2 wording: `files N · parse Nms · nodes N · edges N`).
    pub status: String,
    /// Engine raw counts (`graph.state.counts`), not the drawn-only subtotal.
    pub counts: Value,
    /// Selected node id, if the canvas has a selection.
    pub selected: Option<String>,
    /// `[x, y, w, h]` of the `#graph` box from the last painted frame.
    pub canvas: Option<[f32; 4]>,
    /// True when the process is the headless `--screenshot` harness.
    pub headless: bool,
}

/// A request that needs the main thread plus the one-shot ack channel back to
/// the connection thread.
pub struct Request {
    pub value: Value,
    pub reply: SyncSender<Value>,
}

/// The control endpoint: a bound Unix socket plus the main-thread request queue.
pub struct Server {
    path: PathBuf,
    rx: Receiver<Request>,
    snap: Arc<Mutex<Snapshot>>,
}

impl Server {
    /// Bind `path` and start the accept thread. Opt-in: never called unless
    /// `--control` was given.
    pub fn start(path: &Path, snap: Snapshot) -> Result<Server, String> {
        if path.exists() {
            // A stale socket from a killed run: remove it so bind() is not a
            // permanent failure. Same path we were explicitly asked to own.
            let _ = std::fs::remove_file(path);
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let listener = UnixListener::bind(path)
            .map_err(|e| format!("--control bind {}: {e}", path.display()))?;
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        let (tx, rx) = sync_channel::<Request>(64);
        let snap = Arc::new(Mutex::new(snap));
        let shared = snap.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let tx = tx.clone();
                let snap = shared.clone();
                // One thread per connection: a client may hold a line open.
                std::thread::spawn(move || serve_connection(stream, &tx, &snap));
            }
        });
        Ok(Server { path: path.to_path_buf(), rx, snap })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Replace the shared snapshot (host calls this on the main thread after a
    /// render pass / after an action).
    pub fn set_snapshot(&self, f: impl FnOnce(&mut Snapshot)) {
        if let Ok(mut s) = self.snap.lock() {
            f(&mut s);
        }
    }

    /// Main-thread drain: wait up to `timeout` for one request that needs the
    /// main thread and return it. The caller must finish with [`Server::reply`].
    pub fn recv(&self, timeout: Duration) -> Option<Request> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// Non-blocking variant used by the windowed per-frame drain: never waits,
    /// returns `None` the moment the queue is empty.
    pub fn try_recv(&self) -> Option<Request> {
        self.rx.try_recv().ok()
    }

    /// The mode this endpoint was started in (headless `--screenshot` or GUI).
    fn snapshot_headless(&self) -> bool {
        self.snap.lock().map(|s| s.headless).unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Process-wide endpoint (one `--control` per process)
//
// Storing the bound `Server` here is what lets BOTH loops serve it — the
// headless `--screenshot` harness and the windowed winit frame — and what lets
// `pov::record_json` advertise `"control": "<path>"` to the MCP discovery
// (`csgraph_core::viewer` reads exactly that instance-record field) without
// threading a parameter through the app.
// ---------------------------------------------------------------------------

static ACTIVE: std::sync::OnceLock<Mutex<Server>> = std::sync::OnceLock::new();

/// Bind + install the opt-in control endpoint, and log the path.
pub fn start(path: &Path, view: &crate::view::GraphView, headless: bool) -> Result<(), String> {
    let server = Server::start(path, snapshot_from_view(view, headless))?;
    eprintln!(
        "[native-csviewer] control socket: {} (headless={headless})",
        server.path().display()
    );
    install(server);
    Ok(())
}

/// Install an already-bound server (`start` does this; tests use it directly).
pub fn install(server: Server) {
    let _ = ACTIVE.set(Mutex::new(server));
}

pub fn is_active() -> bool {
    ACTIVE.get().is_some()
}

/// The advertised socket path. `None` when `--control` was not given, so the
/// instance record omits the field instead of advertising a dead endpoint.
pub fn socket_path() -> Option<PathBuf> {
    ACTIVE
        .get()
        .and_then(|m| m.lock().ok().map(|s| s.path().to_path_buf()))
}

/// Snapshot from the live view. Main thread only: the canvas rect is the nzk_ui
/// thread-local geometry from the last painted frame.
pub fn snapshot_from_view(view: &crate::view::GraphView, headless: bool) -> Snapshot {
    use crate::app::nzk_ui;
    let counts = view
        .graph
        .state
        .get("counts")
        .cloned()
        .unwrap_or_else(|| json!({}));
    Snapshot {
        status: nzk_ui::status_text(&view.graph),
        counts,
        selected: view
            .selected
            .and_then(|i| view.graph.nodes.get(i))
            .map(|n| n.id.clone()),
        canvas: nzk_ui::canvas_rect().map(|r| [r.min.x, r.min.y, r.width(), r.height()]),
        headless,
    }
}

/// Refresh the shared snapshot from the live view (main thread).
pub fn refresh_snapshot(view: &crate::view::GraphView) {
    let Some(m) = ACTIVE.get() else { return };
    let Ok(server) = m.lock() else { return };
    let headless = server.snapshot_headless();
    let fresh = snapshot_from_view(view, headless);
    server.set_snapshot(move |s| *s = fresh);
}

/// Windowed per-frame drain: NON-BLOCKING (`try_recv`), so the winit frame never
/// stalls on a control request; every queued main-thread op runs on this thread
/// with the real `GraphView`/nzk_ui geometry.
///
/// Returns true when at least one request ran. The caller must schedule a
/// follow-up frame in that case: a view-only click (`Fit`, `Full graph`,
/// `Re-settle`) sets flags/`layout_dirty` that are consumed by the NEXT
/// `ui()` pass, and an event-driven window would otherwise sit on the change
/// until an unrelated input arrived. A drained `quit` is acknowledged but does
/// not stop the windowed loop (the window owns its exit policy).
pub fn drain_pending(view: &mut crate::view::GraphView) -> bool {
    let Some(m) = ACTIVE.get() else { return false };
    let Ok(server) = m.lock() else { return false };
    let mut handled = false;
    while let Some(req) = server.try_recv() {
        handled = true;
        let _ = dispatch(view, req, true);
    }
    drop(server);
    refresh_snapshot(view);
    handled
}

/// Execute one main-thread request. Returns true when the client asked to quit
/// AND this loop must stop (headless); windowed `quit` only acknowledges.
pub fn dispatch(view: &mut crate::view::GraphView, req: Request, windowed: bool) -> bool {
    let id = req.value.get("id").cloned().unwrap_or(Value::Null);
    let op = req.value.get("op").and_then(Value::as_str).unwrap_or("").to_string();
    let ack = match op.as_str() {
        "quit" => {
            let _ = req.reply.send(ack_ok(&id, "quit", json!({"bye": true})));
            return !windowed;
        }
        "click" => click(view, &id, &req.value, windowed),
        "select_node" => select_node(view, &id, &req.value),
        "state" => ack_ok(
            &id,
            "state",
            snapshot_json(&snapshot_from_view(view, !windowed)),
        ),
        other => ack_err(
            &id,
            other,
            "unknown_op",
            "supported ops: ping, state, click, select_node, quit",
        ),
    };
    let _ = req.reply.send(ack);
    false
}

/// `{"op":"select_node","id"|"label"|"index": ...}` — select ONE node in the
/// live view and report what was selected.
///
/// This is a VIEW-ONLY control op: it needs nothing but `&mut GraphView`, which
/// is all `dispatch` receives, so it works in the headless harness as well as
/// the windowed app (the 14 ops that need `NzkUiState`/`egui::Context` still
/// do not). Resolution order: exact node `id`, then exact `label`, then 0-based
/// `index` into `GraphView::graph.nodes`; `selector` accepts either id or label.
/// Ambiguity is an error (`ambiguous_selector` with the candidate count), not a
/// silent first-match, so a caller can never believe it selected a different
/// node than the one it named.
fn select_node(view: &mut crate::view::GraphView, id: &Value, req: &Value) -> Value {
    let nodes = &view.graph.nodes;
    let by_index = req.get("index").and_then(Value::as_u64).map(|i| i as usize);
    let by_id = req.get("id").and_then(Value::as_str);
    let by_label = req.get("label").and_then(Value::as_str);
    let by_sel = req.get("selector").and_then(Value::as_str);

    let index: Option<usize> = if let Some(i) = by_index {
        if i < nodes.len() { Some(i) } else { None }
    } else if let Some(want) = by_id.or(by_sel) {
        let hits: Vec<usize> = nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.id == want)
            .map(|(i, _)| i)
            .collect();
        match hits.len() {
            1 => Some(hits[0]),
            0 => None,
            n => {
                return ack_err(
                    id,
                    "select_node",
                    "ambiguous_selector",
                    format!("node id {want:?} matched {n} nodes"),
                )
            }
        }
    } else if let Some(want) = by_label {
        let hits: Vec<usize> = nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.label == want)
            .map(|(i, _)| i)
            .collect();
        match hits.len() {
            1 => Some(hits[0]),
            0 => None,
            n => {
                return ack_err(
                    id,
                    "select_node",
                    "ambiguous_selector",
                    format!("label {want:?} matched {n} nodes; use id or index"),
                )
            }
        }
    } else {
        None
    };

    match index {
        Some(i) => {
            view.select(Some(i));
            let n = &view.graph.nodes[i];
            ack_ok(
                id,
                "select_node",
                json!({"selected": i, "id": n.id, "label": n.label, "kind": n.kind}),
            )
        }
        None => ack_err(
            id,
            "select_node",
            "missing_selector",
            "select_node needs id, label, or index of an existing node",
        ),
    }
}

/// `{"op":"click","selector":"#fitBtn"}` — routed through the REAL nzk_ui path:
/// painted `box_rect` -> center hit-test -> `NzkUiAction::from_selector`, then a
/// real consequence for the actions this slice applies.
fn click(
    view: &mut crate::view::GraphView,
    id: &Value,
    req: &Value,
    windowed: bool,
) -> Value {
    use crate::actions::NzkUiAction;
    use crate::app::nzk_ui;
    let selector = req
        .get("selector")
        .and_then(Value::as_str)
        .or_else(|| {
            req.get("args")
                .and_then(|a| a.get("selector"))
                .and_then(Value::as_str)
        })
        .map(str::to_string);
    let Some(selector) = selector else {
        return ack_err(
            id,
            "click",
            "missing_selector",
            "expected {\"op\":\"click\",\"selector\":\"#fitBtn\"}",
        );
    };
    // Painted-selector lookup: refuses an id the nzk_ui did not actually paint,
    // so the control channel cannot click imaginary controls.
    let Some(rect) = nzk_ui::box_rect(&selector) else {
        return ack_err(
            id,
            "click",
            "unknown_selector",
            format!("{selector} is not in the last painted nzk_ui geometry"),
        );
    };
    let center = rect.center();
    let (cx, cy) = (center.x, center.y);
    // The same box-containment hit-test a pointer press uses; None on drift.
    let Some(hit) = nzk_ui::hit_selector(cx, cy) else {
        return ack_err(
            id,
            "click",
            "no_hit",
            format!("no painted control contains the center of {selector}"),
        );
    };
    let action = NzkUiAction::from_selector(hit);
    let mut consequence = Value::Null;
    if action == NzkUiAction::Fit {
        // A real consequence in BOTH modes: the next frame applies the fit.
        view.fit_pending = true;
        consequence = json!({"fit_pending": true});
    } else if action == NzkUiAction::FullGraph {
        // `FullGraph` and `Resettle` are VIEW-ONLY consequences (the sidebar
        // counters they also bump are not observable in `state`), so the
        // control channel can perform the real thing rather than refuse: set
        // the mode, drop the old arrangement and let the next frame's `ui()`
        // run the layout, exactly as the host dispatch does.
        view.view_mode = crate::view::ViewMode::Full;
        view.reseed_layout();
        view.rebuild_view();
        view.fit_pending = true;
        consequence = json!({
            "view_mode": "full",
            "visible": view.visible_count(),
            "reseed": true,
            "fit_pending": true,
        });
    } else if action == NzkUiAction::Resettle {
        view.reseed_layout();
        view.rebuild_view();
        view.fit_pending = true;
        consequence = json!({
            "visible": view.visible_count(),
            "reseed": true,
            "fit_pending": true,
        });
    } else {
        return ack_err(
            id,
            "click",
            if windowed { "unsupported_action" } else { "unsupported_in_headless" },
            format!(
                "{action:?} needs the full App dispatch (nzk_ui hit -> dispatch_action); \
                 this slice implements Fit, Full graph and Re-settle (selector {hit} \
                 resolved through the real path)"
            ),
        );
    }
    ack_ok(
        id,
        "click",
        json!({
            "selector": selector,
            "hit": hit,
            "action": format!("{action:?}"),
            "box": {"x": rect.min.x, "y": rect.min.y, "w": rect.width(), "h": rect.height()},
            "point": [cx, cy],
            "consequence": consequence,
            "mode": if windowed { "windowed" } else { "headless" },
            "status": nzk_ui::status_text(&view.graph),
        }),
    )
}

/// Headless serve loop: blocking `recv` between render passes, on the main
/// thread that painted the frame (so `box_rect`/`hit_selector` are valid).
/// Exits on `quit` or `idle` silence. Returns false if `--control` is inactive.
pub fn serve_headless(view: &mut crate::view::GraphView, idle: Duration) -> bool {
    let Some(m) = ACTIVE.get() else { return false };
    let Ok(server) = m.lock() else { return false };
    let path = server.path().display().to_string();
    drop(server);
    eprintln!("[control] headless: send one JSON object per line to {path} (op \"quit\" exits)");
    // Refresh BEFORE serving: the first `state` must carry the painted canvas.
    refresh_snapshot(view);
    loop {
        let req = {
            let Some(m) = ACTIVE.get() else { break };
            let Ok(server) = m.lock() else { break };
            server.recv(idle)
        };
        let Some(req) = req else {
            eprintln!("[control] no request for {}s; exiting", idle.as_secs());
            break;
        };
        let quit = dispatch(view, req, false);
        refresh_snapshot(view);
        if quit {
            break;
        }
    }
    true
}

/// Frame a successful ack. `id` echoes the request so a client can pipeline.
pub fn ack_ok(id: &Value, op: &str, result: Value) -> Value {
    json!({ "id": id, "ok": true, "op": op, "result": result })
}

/// Frame a failed ack with a stable machine-readable `code`.
pub fn ack_err(id: &Value, op: &str, code: &str, message: impl Into<String>) -> Value {
    json!({
        "id": id,
        "ok": false,
        "op": op,
        "error": { "code": code, "message": message.into() },
    })
}

/// JSON view of the shared snapshot (`state` op / `status` echo).
pub fn snapshot_json(s: &Snapshot) -> Value {
    json!({
        "status": s.status,
        "counts": s.counts,
        "selected": s.selected,
        "canvas": s.canvas,
        "headless": s.headless,
    })
}

fn serve_connection(
    stream: UnixStream,
    tx: &SyncSender<Request>,
    snap: &Arc<Mutex<Snapshot>>,
) {
    let Ok(mut out) = stream.try_clone() else { return };
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            let ack = ack_err(&Value::Null, "", "bad_json", "one JSON object per line expected");
            let _ = writeln!(out, "{ack}");
            continue;
        };
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        let op = value.get("op").and_then(Value::as_str).unwrap_or("").to_string();
        let ack = match op.as_str() {
            // Answered from the shared snapshot: this is the fast, frame-free
            // path MCP uses to poll state between human-task steps.
            "ping" => ack_ok(&id, "ping", json!({"pong": true})),
            "state" => {
                let s = snap.lock().map(|s| s.clone()).unwrap_or_default();
                ack_ok(&id, "state", snapshot_json(&s))
            }
            // Everything else mutates app/view state and must run on the main
            // thread. The connection thread blocks on the per-request channel.
            _ => {
                let (reply_tx, reply_rx) = sync_channel::<Value>(1);
                let mut ack = match tx.send(Request { value, reply: reply_tx }) {
                    Ok(()) => match reply_rx.recv_timeout(MAIN_THREAD_TIMEOUT) {
                        Ok(mut a) => {
                            a["id"] = id.clone();
                            a
                        }
                        Err(_) => ack_err(
                            &id,
                            &op,
                            "unsupported_in_windowed",
                            "no main-thread drain is wired for the windowed winit loop yet; use --screenshot (headless) for control ops",
                        ),
                    },
                    Err(_) => ack_err(&id, &op, "control_channel_closed", "main thread gone"),
                };
                ack["op"] = json!(op);
                ack
            }
        };
        if writeln!(out, "{ack}").is_err() {
            break;
        }
        if op == "quit" {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire shape acking clients parse: `id` echoed, `ok`, `op`, and a
    /// result/error body. Pinned because the MCP client is another process and
    /// a silent shape change would look like "control channel broken".
    #[test]
    fn ack_shapes_are_stable() {
        let ok = ack_ok(&json!(7), "ping", json!({"pong": true}));
        assert_eq!(ok["id"], json!(7));
        assert_eq!(ok["ok"], json!(true));
        assert_eq!(ok["op"], "ping");
        assert_eq!(ok["result"]["pong"], json!(true));
        let err = ack_err(&json!(8), "click", "unknown_selector", "nope");
        assert_eq!(err["ok"], json!(false));
        assert_eq!(err["error"]["code"], "unknown_selector");
    }

    #[test]
    fn snapshot_default_is_honest() {
        let s = Snapshot::default();
        let j = snapshot_json(&s);
        assert_eq!(j["selected"], Value::Null);
        assert_eq!(j["canvas"], Value::Null);
        assert_eq!(j["headless"], json!(false));
    }

    /// R20-E end-to-end proof of BOTH new guarantees, in one test so the
    /// process-wide endpoint is installed exactly once:
    ///
    /// * registry advertisement — the path `pov::record_json` publishes as the
    ///   instance record's `"control"` field (the key `csgraph_core::viewer`
    ///   discovery reads) resolves with no configured path once installed;
    /// * windowed drain — a main-thread request queued over the real socket is
    ///   dequeued and acked by `drain_pending`, the exact function `App::frame`
    ///   calls each pass (this sandbox has no usable display, so the windowed
    ///   run itself is proven by this + the live headless e2e).
    #[test]
    fn registry_advertises_and_windowed_drain_serves() {
        use crate::model::Graph;
        use crate::view::GraphView;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;

        let dir = std::env::temp_dir().join(format!("csviewer-ctl-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("advertised.sock");
        let server = Server::start(&path, Snapshot::default()).expect("bind test socket");
        assert!(socket_path().is_none(), "no advertisement before install");
        install(server);
        assert!(is_active());
        assert_eq!(socket_path().as_deref(), Some(path.as_path()));
        // The instance record MCP discovery reads must carry the same path under
        // exactly `"control"` (not null, not a different spelling).
        let record = crate::pov::record_json(
            std::path::Path::new("/tmp/does-not-exist"),
            &[],
            std::path::Path::new("/tmp/does-not-exist-cache"),
        );
        assert_eq!(record["control"], json!(path.to_string_lossy()));

        let mut view = GraphView::new(Graph {
            repo: std::path::PathBuf::from("/tmp/does-not-exist"),
            nodes: Vec::new(),
            edges: Vec::new(),
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        });
        let sock = path.clone();
        let client = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let mut stream = UnixStream::connect(&sock).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            stream
                .write_all(b"{\"id\":9,\"op\":\"click\",\"selector\":\"#fitBtn\"}\n")
                .expect("write");
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).expect("read ack");
            (start.elapsed(), line)
        });
        // Poll like the winit frame loop: the ack must land well inside 2 s, and
        // the drain must never block (try_recv only).
        for _ in 0..200 {
            let _ = drain_pending(&mut view);
            std::thread::sleep(Duration::from_millis(10));
        }
        let (elapsed, line) = client.join().expect("client thread");
        assert!(elapsed < Duration::from_secs(2), "windowed drain took {elapsed:?}");
        let ack: Value = serde_json::from_str(line.trim()).expect("ack json");
        assert_eq!(ack["id"], json!(9));
        assert_eq!(ack["op"], "click");
        // No painted geometry in a unit process: the request still reached the
        // MAIN thread and was refused honestly by the real selector path — not
        // parked behind the windowed timeout.
        assert_eq!(ack["ok"], json!(false));
        assert_eq!(ack["error"]["code"], "unknown_selector");
        let _ = std::fs::remove_file(&path);
    }
}
