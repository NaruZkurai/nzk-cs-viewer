//! viewer — MCP client for the v3 native viewer's `--control` unix socket.
//!
//! The MCP server (`csgraph --mcp`, stdio) runs the engine in-process and has no
//! HTTP client; this module is the missing **input** half: it discovers a live
//! viewer's control socket from the instance registry and passes one command to
//! it. The app side owns the listener; this side is a client only.
//!
//! # Protocol v1 (contract with `native-csviewer --control`)
//!
//! Transport: `AF_UNIX` stream (`SOCK_STREAM`), one connection per command.
//! Framing: newline-delimited JSON, exactly one request line then one reply line.
//!
//! Request (client -> viewer):
//! ```json
//! {"op":"click","args":{"selector":"#modeFind"}}
//! {"op":"ping"}
//! ```
//! `op` is required; `args` is optional and defaults to `{}`. An `id` field is
//! accepted by the server but not required for v1 correlation: this client
//! writes one request per connection, then half-closes nothing and reads the
//! single reply line.
//!
//! Reply (viewer -> client), the shape MCP must be able to report honestly:
//! ```json
//! {"ok":true,"op":"click","result":{"pov":{...}}}
//! {"ok":false,"op":"click","error":{"code":"unknown_selector","message":"..."}}
//! ```
//! `ok` must be a boolean. `error` may be a string or an object; it is passed
//! through verbatim (never reinterpreted as success). If the socket cannot be
//! found/connected the MCP tool returns `{"ok":false,"error":"no_control_channel"}`
//! and sets `isError`, never a fabricated success.
//!
//! # Discovery
//!
//! The viewer adds a `"control": "<socket path>"` field to its instance record
//! (`$XDG_RUNTIME_DIR/csviewer/instances/<pid>.json` or `/tmp/csviewer/instances`,
//! the existing [`crate::registry`] locations, e.g.
//! `/tmp/csviewer/instances/control/<pid>.sock`). This client reads those
//! records with [`crate::registry::read_records`], honours an explicit `path` or
//! `instance` selector, and refuses to guess when several live control sockets
//! exist without a selector (`ambiguous_control_channel`).
//!
//! Ops are the plan's set (`notes/v3-control-channel-plan.md` §2): `ping`,
//! `state`, `click` `{selector}|{x,y,canvas}`, `type` `{selector?,text}`,
//! `key` `{key,mods}`, `drag`, `scroll`, `mode`, `pane`, `tab`, `window`,
//! `select_node`, `open_source`, `find`, `edit`, `preview`, `reorg`, `pov`.
//! `viewer_control` is the generic passthrough for all of them.
//!
//! Client I/O uses `set_read_timeout`/`set_write_timeout`; a reply that does not
//! arrive in time is reported as `control_timeout`, not blocked on forever.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

/// Default per-command timeout for MCP calls (2 s).
pub const DEFAULT_TIMEOUT_MS: u64 = 2000;

/// The instance-record key that carries the control socket path (v1 contract).
pub const CONTROL_KEY: &str = "control";

/// Why a control command could not be delivered. Every variant maps to an
/// honest machine-readable `error` code in the MCP result.
#[derive(Debug)]
pub enum ViewerError {
    /// No viewer record with a `control` field, or the socket path is gone.
    NoChannel(String),
    /// More than one control socket and no selector: refuse rather than guess.
    Ambiguous(Vec<String>),
    /// The socket exists but the connect/read/write failed.
    Io(String),
    /// Connected, wrote the request, but no reply line arrived in time.
    Timeout,
    /// A reply arrived but was not a JSON object with a boolean `ok`.
    Protocol(String),
}

impl ViewerError {
    pub fn code(&self) -> &'static str {
        match self {
            ViewerError::NoChannel(_) => "no_control_channel",
            ViewerError::Ambiguous(_) => "ambiguous_control_channel",
            ViewerError::Io(_) => "control_io_error",
            ViewerError::Timeout => "control_timeout",
            ViewerError::Protocol(_) => "control_protocol_error",
        }
    }

    fn detail(&self) -> Value {
        match self {
            ViewerError::NoChannel(m) => json!(m),
            ViewerError::Ambiguous(candidates) => json!({"candidates": candidates}),
            ViewerError::Io(m) => json!(m),
            ViewerError::Protocol(m) => json!(m),
            ViewerError::Timeout => json!("no reply within the timeout"),
        }
    }
}

/// A discovered control endpoint: the socket plus the record that named it.
#[derive(Debug, Clone)]
pub struct Target {
    pub socket: PathBuf,
    pub pid: Option<u32>,
    pub record: PathBuf,
}

/// One line-delimited-JSON request/reply client for an `AF_UNIX` control socket.
pub struct ControlClient {
    writer: std::os::unix::net::UnixStream,
    reader: BufReader<std::os::unix::net::UnixStream>,
}

impl ControlClient {
    /// Connect to `socket`, applying `timeout` to write and read.
    pub fn connect(socket: &Path, timeout: Duration) -> Result<Self, ViewerError> {
        let stream = std::os::unix::net::UnixStream::connect(socket)
            .map_err(|e| ViewerError::NoChannel(format!("connect {}: {e}", socket.display())))?;
        stream.set_read_timeout(Some(timeout)).map_err(io_err)?;
        stream.set_write_timeout(Some(timeout)).map_err(io_err)?;
        let reader_src = stream.try_clone().map_err(io_err)?;
        Ok(ControlClient { writer: stream, reader: BufReader::new(reader_src) })
    }

    /// Send one request and return the parsed reply object (verbatim).
    pub fn call(&mut self, op: &str, args: &Value) -> Result<Value, ViewerError> {
        let req = json!({"op": op, "args": args});
        let mut line = serde_json::to_string(&req)
            .map_err(|e| ViewerError::Protocol(format!("encode request: {e}")))?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).map_err(io_err)?;
        self.writer.flush().map_err(io_err)?;

        let mut reply = String::new();
        self.reader.read_line(&mut reply).map_err(|e| match e.kind() {
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => ViewerError::Timeout,
            _ => ViewerError::Io(format!("read reply: {e}")),
        })?;
        if reply.trim().is_empty() {
            return Err(ViewerError::Protocol("empty reply line".to_string()));
        }
        let value: Value = serde_json::from_str(reply.trim())
            .map_err(|e| ViewerError::Protocol(format!("reply is not JSON: {e}")))?;
        match value.get("ok").and_then(Value::as_bool) {
            Some(_) => Ok(value),
            None => Err(ViewerError::Protocol(
                "reply has no boolean `ok` field".to_string(),
            )),
        }
    }
}

fn io_err(e: std::io::Error) -> ViewerError {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => ViewerError::Timeout,
        _ => ViewerError::Io(e.to_string()),
    }
}

/// Discover the control endpoint to talk to.
///
/// Precedence: an explicit `path` (the `path`/`socket` argument) wins; else an
/// `instance` selector (pid or path substring, matching the record fields);
/// else the only record that has a `control` field. Zero candidates is
/// [`ViewerError::NoChannel`]; several without a selector is
/// [`ViewerError::Ambiguous`] with the candidate paths.
pub fn discover(
    records: &[PathBuf],
    explicit: Option<&Path>,
    selector: Option<&str>,
) -> Result<Target, ViewerError> {
    if let Some(p) = explicit {
        let alive = p.exists();
        let record = records
            .iter()
            .find(|r| record_control_value(r).map(|c| c == p).unwrap_or(false))
            .cloned()
            .unwrap_or_else(|| PathBuf::new());
        if !alive {
            return Err(ViewerError::NoChannel(format!(
                "control socket {} does not exist",
                p.display()
            )));
        }
        return Ok(Target { socket: p.to_path_buf(), pid: record_pid(&record), record });
    }

    let mut found: Vec<Target> = Vec::new();
    for r in records {
        let Some(control) = record_control_value(r) else {
            continue;
        };
        if !control.exists() {
            continue;
        }
        let pid = record_pid(r);
        if let Some(sel) = selector.map(str::trim).filter(|s| !s.is_empty()) {
            let pid_matches = pid.map(|p| p.to_string() == sel).unwrap_or(false);
            // A numeric selector is an exact pid; only non-numeric selectors
            // fall back to the record-text substring (matching `Instance::matches`).
            let matches = if sel.parse::<u32>().is_ok() {
                pid_matches
            } else {
                pid_matches || std::fs::read_to_string(r).unwrap_or_default().contains(sel)
            };
            if !matches {
                continue;
            }
        }
        found.push(Target { socket: control, pid, record: r.clone() });
    }

    match found.len() {
        0 => Err(ViewerError::NoChannel(
            "no instance record carries a control socket (viewer not running with --control)"
                .to_string(),
        )),
        1 => Ok(found.remove(0)),
        _ => {
            let mut paths: Vec<String> = found
                .iter()
                .map(|t| t.socket.to_string_lossy().into_owned())
                .collect();
            paths.sort();
            Err(ViewerError::Ambiguous(paths))
        }
    }
}

/// Registry records for the default instance directories.
pub fn instance_records() -> Vec<PathBuf> {
    crate::registry::read_records(&crate::registry::instance_dirs())
        .into_iter()
        .map(|(path, _)| path)
        .collect()
}

fn record_control_value(record: &Path) -> Option<PathBuf> {
    let raw = std::fs::read(record).ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    let control = value.get(CONTROL_KEY)?.as_str()?.trim();
    if control.is_empty() {
        return None;
    }
    Some(PathBuf::from(control))
}

fn record_pid(record: &Path) -> Option<u32> {
    let raw = std::fs::read(record).ok()?;
    let value: Value = serde_json::from_slice(&raw).ok()?;
    value.get("pid")?.as_u64().map(|n| n as u32)
}

/// Run one control op against the discovered (or explicit) socket.
pub fn call(op: &str, args: &Value, explicit: Option<&Path>, selector: Option<&str>) -> Result<Value, ViewerError> {
    let target = discover(&instance_records(), explicit, selector)?;
    let mut client = ControlClient::connect(&target.socket, Duration::from_millis(DEFAULT_TIMEOUT_MS))?;
    client.call(op, args)
}

/// The MCP tool result envelope: one text block carrying the JSON body; the MCP
/// `isError` flag is set whenever the body's `ok` is false. The body is never
/// rewritten: a viewer refusal is surfaced exactly as the viewer wrote it.
pub fn envelope(body: Value) -> Value {
    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let text = serde_json::to_string(&body)
        .unwrap_or_else(|_| r#"{"ok":false,"error":"control_protocol_error"}"#.to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": !ok,
    })
}

/// Error-shaped tool result with an honest machine-readable code.
pub fn error_body(err: &ViewerError) -> Value {
    let mut body = json!({"ok": false, "error": err.code(), "detail": err.detail()});
    if let ViewerError::NoChannel(_) = err {
        if let Some(o) = body.as_object_mut() {
            o.insert(
                "hint".to_string(),
                json!("launch a viewer with --control and a `control` field in its instance record"),
            );
        }
    }
    body
}

/// Names owned by this module (the viewer-input tools, not the read-only
/// `viewer_state`/`viewer_instances`/`viewer_pov` routes).
pub const TOOLS: &[&str] = &["viewer_control", "viewer_click", "viewer_observe"];

/// True when `name` is one of the viewer-control tools.
pub fn is_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

/// Execute one MCP viewer-control tool call. `args` are the tool arguments after
/// the server-only `repo` key has been stripped.
///
/// * `viewer_control` — `{op, args?, path?, instance?}` generic passthrough.
/// * `viewer_click` — `{selector, path?, instance?}` -> `{op:"click",args:{selector}}`.
/// * `viewer_observe` — `{path?, instance?, full?}` -> `{op:"state",args:{...}}`,
///   the channel's status/POV snapshot.
pub fn invoke_tool(name: &str, args: &Value) -> Value {
    let explicit = args
        .get("path")
        .or_else(|| args.get("socket"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(Path::new);
    let selector = args.get("instance").and_then(Value::as_str);
    let (op, op_args) = match name {
        "viewer_control" => {
            let Some(op) = args.get("op").and_then(Value::as_str).filter(|s| !s.trim().is_empty())
            else {
                return envelope(json!({
                    "ok": false,
                    "error": "missing_argument",
                    "detail": "viewer_control requires a non-empty `op`",
                }));
            };
            let op_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
            if !op_args.is_object() {
                return envelope(json!({
                    "ok": false,
                    "error": "missing_argument",
                    "detail": "`args` must be an object",
                }));
            }
            (op.to_string(), op_args)
        }
        "viewer_click" => {
            let Some(sel) = args
                .get("selector")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
            else {
                return envelope(json!({
                    "ok": false,
                    "error": "missing_argument",
                    "detail": "viewer_click requires a non-empty `selector` (canvas clicks go through viewer_control {op:\"click\", args:{x,y,canvas:true}})",
                }));
            };
            ("click".to_string(), json!({"selector": sel}))
        }
        "viewer_observe" => {
            let mut a = json!({});
            if let Some(full) = args.get("full") {
                a = json!({"full": full});
            }
            ("state".to_string(), a)
        }
        other => {
            return envelope(json!({
                "ok": false,
                "error": "unknown_tool",
                "detail": format!("not a viewer-control tool: {other}"),
            }))
        }
    };
    match call(&op, &op_args, explicit, selector) {
        Ok(reply) => envelope(reply),
        Err(err) => envelope(error_body(&err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("csgraph-viewer-test-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A fake viewer: accept one connection, read one request line, then either
    /// reply with `reply` or (when `None`) stay silent past the client timeout.
    fn fake_server(dir: &Path, reply: Option<Value>) -> PathBuf {
        let sock = dir.join("control.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut req = String::new();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let _ = reader.read_line(&mut req);
            match reply {
                Some(v) => {
                    let mut line = serde_json::to_string(&v).unwrap();
                    line.push('\n');
                    let _ = stream.write_all(line.as_bytes());
                    let _ = stream.flush();
                }
                None => std::thread::sleep(Duration::from_secs(5)),
            }
        });
        sock
    }

    fn write_record(dir: &Path, name: &str, record: Value) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, serde_json::to_vec(&record).unwrap()).unwrap();
        p
    }

    #[test]
    fn round_trip_ping_against_a_fake_unix_socket() {
        let dir = unique_dir("roundtrip");
        let sock = fake_server(&dir, Some(json!({"ok": true, "op": "ping", "result": {"pong": 1}})));
        let mut client = ControlClient::connect(&sock, Duration::from_millis(500)).unwrap();
        let reply = client.call("ping", &json!({})).unwrap();
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["op"], "ping");
        assert_eq!(reply["result"]["pong"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn viewer_control_tool_round_trips_a_click_and_passes_the_ack_through() {
        let dir = unique_dir("click");
        let sock = fake_server(
            &dir,
            Some(json!({"ok": true, "op": "click", "result": {"pov": {"context": {"selected": "N1"}}}})),
        );
        let out = invoke_tool(
            "viewer_control",
            &json!({"op": "click", "args": {"selector": "#modeFind"}, "path": sock}),
        );
        assert_eq!(out["isError"], false);
        let body: Value = serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["op"], "click");
        assert_eq!(body["result"]["pov"]["context"]["selected"], "N1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_silent_viewer_is_reported_as_control_timeout() {
        let dir = unique_dir("timeout");
        let sock = fake_server(&dir, None);
        let mut client = ControlClient::connect(&sock, Duration::from_millis(250)).unwrap();
        let err = client.call("state", &json!({})).unwrap_err();
        assert_eq!(err.code(), "control_timeout", "{err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_socket_is_an_honest_no_control_channel_error() {
        let dir = unique_dir("nosocket");
        let missing = dir.join("does-not-exist.sock");
        let err = call("ping", &json!({}), Some(&missing), None).unwrap_err();
        assert_eq!(err.code(), "no_control_channel");
        // And discovery over an empty registry must not invent a channel.
        let err = discover(&[], None, None).unwrap_err();
        assert_eq!(err.code(), "no_control_channel");
        let out = invoke_tool("viewer_control", &json!({"op": "ping"}));
        assert_eq!(out["isError"], true);
        let body: Value = serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["ok"], false);
        assert_eq!(body["error"], "no_control_channel");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_prefers_the_registry_control_field_and_refuses_ambiguity() {
        let dir = unique_dir("discovery");
        // Two sockets live in their own dirs; the RECORDS both live in `dir`.
        let dir_a = unique_dir("discovery-a");
        let dir_b = unique_dir("discovery-b");
        let sock_a = fake_server(&dir_a, Some(json!({"ok": true})));
        let sock_b = fake_server(&dir_b, Some(json!({"ok": true})));
        let ra = write_record(&dir, "a.json", json!({"pid": 11, "control": sock_a}));
        let rb = write_record(&dir, "b.json", json!({"pid": 22, "control": sock_b}));

        // No selector + two live control sockets -> refuse rather than guess.
        let err = discover(&[ra.clone(), rb.clone()], None, None).unwrap_err();
        assert_eq!(err.code(), "ambiguous_control_channel");

        // Selector picks exactly one.
        let t = discover(&[ra.clone(), rb.clone()], None, Some("22")).unwrap();
        assert_eq!(t.socket, sock_b);
        assert_eq!(t.pid, Some(22));
        assert_eq!(t.record, rb);

        // Unrelated selector is an honest miss.
        let err = discover(&[ra.clone(), rb.clone()], None, Some("999")).unwrap_err();
        assert_eq!(err.code(), "no_control_channel");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_reply_is_a_protocol_error_not_success() {
        let dir = unique_dir("malformed");
        let sock = fake_server(&dir, Some(json!({"nope": true})));
        let mut client = ControlClient::connect(&sock, Duration::from_millis(500)).unwrap();
        let err = client.call("ping", &json!({})).unwrap_err();
        assert_eq!(err.code(), "control_protocol_error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn viewer_click_requires_a_selector_and_sends_op_click() {
        let out = invoke_tool("viewer_click", &json!({}));
        assert_eq!(out["isError"], true);
        let body: Value = serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["error"], "missing_argument");
        // viewer_observe maps to op "state" (no channel -> honest error, but the
        // tool must be recognised, not reported as unknown).
        let out = invoke_tool("viewer_observe", &json!({}));
        assert_eq!(out["isError"], true);
        let body: Value = serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_ne!(body["error"], "unknown_tool");
    }
}
