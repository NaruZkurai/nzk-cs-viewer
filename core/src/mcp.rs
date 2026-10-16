//! mcp — the Model Context Protocol server (`csgraph --mcp`).
//!
//! An agent harness spawns `csgraph --mcp --repo <path>` and talks JSON-RPC 2.0
//! over stdio: one JSON message per line, stdout carries protocol traffic only,
//! everything else goes to stderr.
//!
//! Design, in one paragraph. `run` binds one [`RepoSlot`] per `--repo` (index 0
//! is the primary/default); each slot keeps its own fixed pool of `threads`
//! worker indices over its own `Mutex<Index>` replicas. The primary is
//! pre-indexed at startup when a `--root` was given; every other repo starts
//! cold and builds on its first call. Every tool call may carry a `repo`
//! argument (1-based index, path, or unique basename; absent = primary): the
//! server resolves it against the bound set, **strips it from the argument
//! map**, and dispatches against that repo's slots only — so a call with no
//! `repo` argument behaves exactly as before. Worker threads pull requests off
//! an unbounded job channel, so the stdio read loop never blocks and protocol
//! methods (`initialize`, `ping`, `tools/list`) stay responsive while a long
//! `tools/call` is in flight. Reads only ever need one slot, so two independent
//! reads run on two slots at once. The one route family that changes index
//! state (`/api/index`, `/api/index/cancel`) is broadcast under that repo's
//! write gate to every one of its slots, so every replica answers from the same
//! built graph afterwards, and other repos are untouched. The non-index writes
//! (`/api/settings`, `/api/viewer_state`, `/api/reorg`) either write a file
//! shared by all slots or are pure, so they run once.
//!
//! Every tool call is routed through the same [`crate::api::handle_payload`] /
//! [`crate::api::handle`] the HTTP host uses, so the MCP surface is the whole
//! `/api/*` surface — reads and writes — with no second implementation.
//!
//! No new crate dependencies: the framing is `serde_json` and the pool is
//! `crossbeam-channel` (already a dependency of this crate).

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use serde_json::{json, Value};

use crate::api::{self, Payload};
use crate::config::Config;
use crate::index::Index;

/// Server name reported in `initialize`'s `serverInfo`.
pub const SERVER_NAME: &str = "csgraph";

/// Protocol revisions this server understands, newest first.
pub const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// The revision answered when the client asks for one we do not know.
pub const LATEST_PROTOCOL: &str = "2025-06-18";

/// How long after stdin EOF the server waits for in-flight calls before leaving.
const DRAIN_TIMEOUT_S: u64 = 60;

/// Options for [`run`], mirroring the `csgraph --mcp` flags.
#[derive(Debug, Clone)]
pub struct McpOptions {
    /// Bound repositories, in order. Index 0 is the **primary** — the repo a
    /// tool call selects when it carries no `repo` argument. `--repo` is
    /// repeatable and at least one occurrence is required.
    pub repos: Vec<PathBuf>,
    /// Roots to pre-index at startup. Empty means "start cold".
    pub roots: Vec<String>,
    /// Excludes for the startup build; empty means [`Config::default`]'s list.
    pub excludes: Vec<String>,
    /// Worker slots. `0` means `min(4, available cores)`.
    pub threads: usize,
}

impl McpOptions {
    fn resolved_threads(&self) -> usize {
        if self.threads > 0 {
            return self.threads;
        }
        thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .clamp(2, 4)
    }

    fn resolved_excludes(&self) -> Vec<String> {
        if self.excludes.is_empty() {
            Config::default().excludes
        } else {
            self.excludes.clone()
        }
    }
}

/// The immutable `(method, path)` a tool maps to, plus its arguments.
struct Call {
    method: String,
    path: String,
    query: Value,
    body: Option<Value>,
}

impl Call {
    /// Routes that install a new graph must be applied to every worker slot.
    ///
    /// `/api/index/cancel` is deliberately **not** here: it must be answered
    /// out-of-band (see [`run_tool`]) because a rebuild holds the write gate for
    /// its whole run — a cancel that queued behind that gate could never stop
    /// the build it was meant to stop.
    fn broadcasts(&self) -> bool {
        self.path == "/api/index"
    }
}

// ---------------------------------------------------------------------------
// framing
// ---------------------------------------------------------------------------

/// What the read loop does with a decoded message.
pub enum Handling {
    /// A notification: no response is sent.
    Ignore,
    /// Respond immediately (protocol methods).
    Reply(Value),
    /// Run on a worker slot (all `tools/call`s).
    Dispatch(Value),
}

fn ok_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

/// Negotiate the protocol revision: echo the client's when supported, else our
/// latest.
pub fn negotiate_protocol(requested: Option<&str>) -> String {
    match requested {
        Some(p) if SUPPORTED_PROTOCOLS.contains(&p) => p.to_string(),
        _ => LATEST_PROTOCOL.to_string(),
    }
}

/// The `initialize` result.
pub fn initialize_result(protocol: Option<&str>, threads: usize) -> Value {
    json!({
        "protocolVersion": negotiate_protocol(protocol),
        "capabilities": {"tools": {}},
        "serverInfo": {
            "name": SERVER_NAME,
            "version": crate::VERSION,
            "threads": threads,
        },
        "instructions": concat!(
            "csgraph exposes the whole /api/* surface of the running engine. ",
            "Use `api_call` for any route; the typed tools map 1:1 onto the ",
            "common routes. Every tool result's text carries a boolean `ok`; ",
            "routed results additionally carry {\"status\": <http code>, ",
            "\"body\": <route body>}. isError is set when status >= 400 for a ",
            "routed tool and when `ok` is false for an MCP-only tool. ",
            "This server may be bound to several repos (`--repo` is repeatable; ",
            "index 0 is the primary/default). Every tool accepts an optional ",
            "`repo` argument — a 1-based index (\"2\" or 2), a path, or a unique ",
            "basename; omit it for the primary. The `repo` key is consumed by ",
            "the server, never forwarded, and an unknown/ambiguous value is an ",
            "error listing every bound repo. `index` rebuilds the selected ",
            "repo's graph only, `index_cancel` cancels the selected repo's ",
            "build only, and `workspace` lists every bound repo with its index, ",
            "roots, loaded flag and slot count. `reorg` mutates a destination ",
            "tree, `settings`/`viewer_state` write state."
        ),
    })
}

/// Route one decoded JSON-RPC message. Pure: no I/O, no threads, so the
/// framing is unit-testable without a client.
pub fn handle_message(msg: &Value, threads: usize) -> Handling {
    let is_request = msg.get("id").is_some();
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = match msg.get("method").and_then(Value::as_str) {
        Some(m) => m,
        None => {
            return Handling::Reply(err_response(id, -32600, "invalid request: method missing"));
        }
    };
    // A notification carries no id and never gets a response.
    if !is_request {
        return Handling::Ignore;
    }
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    match method {
        "initialize" => {
            let protocol = params.get("protocolVersion").and_then(Value::as_str);
            Handling::Reply(ok_response(id, initialize_result(protocol, threads)))
        }
        "ping" => Handling::Reply(ok_response(id, json!({}))),
        "tools/list" => Handling::Reply(ok_response(id, json!({"tools": tools()}))),
        "tools/call" => Handling::Dispatch(msg.clone()),
        "notifications/initialized" | "notifications/cancelled" => Handling::Ignore,
        other => Handling::Reply(err_response(
            id,
            -32601,
            format!("method not found: {other}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// tools
// ---------------------------------------------------------------------------

/// One tool descriptor. Every schema carries a `repo` selector: the argument is
/// consumed by the server (it never reaches the route call), and omitting it
/// selects the primary `--repo`.
fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    let mut props = props;
    if let Some(obj) = props.as_object_mut() {
        obj.insert(
            "repo".to_string(),
            json!({
                "type": "string",
                "description": "Repository selector for this call: a 1-based index (\"2\" or 2), \
                                an absolute or cwd-relative path, or a unique repo basename \
                                (case-insensitive). Omitted = the primary `--repo`. An unknown or \
                                ambiguous selector is an error listing every bound repo. Consumed \
                                by the server; it is stripped before the call is dispatched."
            }),
        );
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": props,
            "required": required,
            "additionalProperties": true,
        },
    })
}

/// The tool catalogue.
///
/// Each description names the `/api/*` route it executes, which is how the
/// probe (and the unit test) proves the tool list covers every route the app
/// can execute. `api_call` is the generic escape hatch for all of them.
pub fn tools() -> Vec<Value> {
    vec![
        tool(
            "api_call",
            "Generic escape hatch: call ANY route the app can execute, reads and writes. \
             Maps to method + path, routed through api::handle/handle_payload. GET reaches every \
             GET route; POST reaches every POST route (including /api/index, /api/index/cancel, \
             /api/settings, /api/viewer_state, /api/reorg).",
            json!({
                "method": {"type": "string", "enum": ["GET", "POST"], "default": "GET",
                           "description": "HTTP verb the route is registered under."},
                "path": {"type": "string", "description": "Route path, e.g. /api/state or /api/reorg."},
                "query": {"type": ["object", "string"], "description": "Query parameters as an object ({\"node\": \"...\"}) or a raw query string."},
                "body": {"type": "object", "description": "JSON body for POST routes."}
            }),
            &["path"],
        ),
        tool("state", "GET /api/state — engine state: indexed, root, fileCount, counts, cache.",
             json!({}), &[]),
        tool("index", "POST /api/index — (re)build the graph of the SELECTED repo (`repo` argument, \
             absent = primary); body may carry {roots:[...]}. Broadcast to every worker slot of \
             that repo only, so all its replicas serve the new graph while other bound repos are \
             untouched.",
             json!({"roots": {"type": ["array", "string"], "description": "Roots relative to the selected repo; defaults to the configured roots."}}),
             &[]),
        tool("index_cancel", "POST /api/index/cancel — ask the in-flight build to unwind.",
             json!({}), &[]),
        tool("browse", "GET /api/browse — list a directory in the repo (dirs + .cs files).",
             json!({"path": {"type": "string", "description": "Relative or absolute directory; empty = repo root."}}), &[]),
        tool("search", "GET /api/search — search nodes by name/kind.",
             json!({"q": {"type": "string"}, "limit": {"type": "integer", "default": 30}}), &["q"]),
        tool("node", "GET /api/node — full detail for one node id.",
             json!({"id": {"type": "string"}}), &["id"]),
        tool("relation", "GET /api/relation — one paginated relation list of a node.",
             json!({"node": {"type": "string"}, "key": {"type": "string"},
                    "offset": {"type": "integer", "default": 0}, "limit": {"type": "integer", "default": 200}}),
             &["node", "key"]),
        tool("find", "GET /api/find — fuzzy/symbol find.",
             json!({"q": {"type": "string"}, "limit": {"type": "integer", "default": 400}}), &["q"]),
        tool("refs", "GET /api/refs — text usages of a symbol.",
             json!({"q": {"type": "string"}, "limit": {"type": "integer", "default": 400}}), &["q"]),
        tool("source", "GET /api/source — source slice for a file+line, a node, or a range.",
             json!({"file": {"type": "string"}, "line": {"type": "integer", "default": 1},
                    "context": {"type": "integer", "default": 30}, "node": {"type": "string"},
                    "to": {"type": "integer"}, "mode": {"type": "string", "default": "doc"}}), &[]),
        tool("doc", "GET /api/doc — synthesized document for a node.",
             json!({"node": {"type": "string"}}), &["node"]),
        tool("decls", "GET /api/decls — declarations by node or file.",
             json!({"node": {"type": "string"}, "file": {"type": "string"}}), &[]),
        tool("chain", "GET /api/chain — call/inheritance chain around a node.",
             json!({"id": {"type": "string"}, "depth": {"type": "integer", "default": 3},
                    "dir": {"type": "string", "default": "forward"}, "max": {"type": "integer", "default": 300}}),
             &["id"]),
        tool("kinds", "GET /api/kinds — node-kind histogram.",
             json!({}), &[]),
        tool("definitions", "GET /api/definitions — public/private definition list.",
             json!({"scope": {"type": "string", "default": "public"}, "limit": {"type": "integer", "default": 400}}), &[]),
        tool("subgraph", "GET /api/subgraph — focused subgraph around a node.",
             json!({"id": {"type": "string"}, "depth": {"type": "integer", "default": 1},
                    "max": {"type": "integer", "default": 250}}), &[]),
        tool("export", "GET /api/export.json — the whole graph as one JSON document.",
             json!({}), &[]),
        tool("projects", "GET /api/projects — the .sln/.csproj model (write route: none).",
             json!({"files": {"type": "string", "description": "1/true to list each project's files."},
                    "file": {"type": "string", "description": "Per-file project lookup."}}), &[]),
        tool("viewer_state", "GET /api/viewer_state — read the saved viewer state; with a `body` \
             argument POST it to mutate the state file (write).",
             json!({"body": {"type": "object", "description": "When present, POST this object instead of GET."}}), &[]),
        tool("settings", "GET /api/settings — read UI settings; with a `body` argument POST it to \
             mutate the settings file (write).",
             json!({"body": {"type": "object", "description": "When present, POST this object instead of GET."}}), &[]),
        tool("reorg", "POST /api/reorg — plan/verify/zip/write a reorganisation \
             ({action, spec, dest, overwrite}); `write` mutates the destination tree (write).",
             json!({"action": {"type": "string", "enum": ["plan", "verify", "zip", "write"], "default": "plan"},
                    "spec": {"type": "object", "description": "Reorg spec, e.g. {\"group\": \"single\"}."},
                    "dest": {"type": "string", "description": "Absolute destination for action=write."},
                    "overwrite": {"type": "boolean", "default": true}}), &[]),
        // -- MCP-only refactor/query family (crate::edit; bypasses /api/*) ---
        tool("fuzzy_search", "MCP-only deterministic ranked fuzzy match over node full/label/name. \
             Tiers: exact full > case-insensitive full > exact simple > prefix > subsequence; ties by \
             degree desc then full asc. Read-only.",
             json!({"q": {"type": "string"}, "kind": {"type": "string", "description": "Optional node-kind filter."},
                    "limit": {"type": "integer", "default": 30}}), &["q"]),
        tool("resolve_chain", "MCP-only: resolve a symbol (node id or fuzzy name) to exactly ONE node, \
             else error with candidates; then return the chain payload for that node plus `resolved`. \
             `dir` is forward|reverse|both. Read-only.",
             json!({"symbol": {"type": "string"}, "depth": {"type": "integer", "default": 3},
                    "dir": {"type": "string", "enum": ["forward", "reverse", "both"], "default": "forward"},
                    "max": {"type": "integer", "default": 300}}), &["symbol"]),
        tool("code_block", "MCP-only: build one compilable code block from the doc synthesis \
             (Index::doc_source / source_shared), keeping the `// ── node · file:line-endLine ──` part \
             markers. Select by `node`, `nodes[]` or `q`. `mode` is doc|effective|raw; `path_only` \
             returns just the file list. Read-only.",
             json!({"node": {"type": "string"}, "nodes": {"type": "array", "items": {"type": "string"}},
                    "q": {"type": "string"}, "limit": {"type": "integer", "default": 8},
                    "mode": {"type": "string", "enum": ["doc", "effective", "raw"], "default": "doc"},
                    "path_only": {"type": "boolean", "default": false}}), &[]),
        tool("rename_symbol", "MCP-only: resolve `from` fuzzy to ONE declaration (refuse ambiguity), \
             validate `to` as a C# identifier, and plan conservative line-level identifier-token \
             replacements over declaration slices plus graph-attributed usage sites. Dry-run by \
             default; `apply` writes via temp file + rename, inside indexed roots only (write).",
             json!({"from": {"type": "string"}, "to": {"type": "string"},
                    "apply": {"type": "boolean", "default": false},
                    "scope": {"type": ["string", "array"], "description": "File or directory prefix (or list)."},
                    "include_declarations": {"type": "boolean", "default": true}}), &["from", "to"]),
        tool("qualify_symbols", "MCP-only fully-qualify pass: list `using`/`using static`/alias \
             directives for removal, build simple-name -> unique full name from indexed declarations, \
             and rewrite bare identifiers (not preceded by `.`) to the full name; ambiguous names are \
             reported untouched. Dry-run by default; `apply` writes files (write).",
             json!({"files": {"type": ["string", "array"]}, "remove_usings": {"type": "boolean", "default": true},
                    "apply": {"type": "boolean", "default": false},
                    "max_edits": {"type": "integer", "default": 2000}}), &[]),
        tool("expand_namespace", "MCP-only: normalise targeted declarations to their graph-recorded full \
             namespace. With `dest`, delegate to reorg action=write spec {group:\"namespace\"} and return \
             its report. Dry-run by default; `apply` writes files (write).",
             json!({"files": {"type": ["string", "array"]}, "namespace": {"type": "string"},
                    "apply": {"type": "boolean", "default": false},
                    "dest": {"type": "string", "description": "When present, delegate a namespace-grouped write here."}}), &[]),
        tool("write_file", "MCP-only: atomically create/overwrite one source file inside the indexed repo. \
             Dry-run by default (writes nothing); `apply` stages a same-directory temp file, fsyncs and \
             renames. `create=true` is required to create a new file; `expected_sha256` refuses a stale \
             write. Paths outside the repo and symlink targets are refused (write).",
             json!({"path": {"type": "string"}, "content": {"type": "string"},
                    "expected_sha256": {"type": "string"},
                    "create": {"type": "boolean", "default": false},
                    "apply": {"type": "boolean", "default": false}}), &["path", "content"]),
        tool("apply_edits", "MCP-only: replace the Nth exact occurrence of `old` with `new` per file, \
             across one or more files. Refuses when the match count differs (no silent no-ops) and is \
             all-or-nothing: every edit is validated before anything is staged, then all temp files are \
             renamed. Dry-run by default; `apply` writes (write).",
             json!({"edits": {"type": "array", "items": {"type": "object",
                       "properties": {"file": {"type": "string"}, "old": {"type": "string"},
                                      "new": {"type": "string"}, "count": {"type": "integer", "default": 1}},
                       "required": ["file", "old", "new"]}},
                    "apply": {"type": "boolean", "default": false}}), &["edits"]),
        tool("select_decls", "MCP-only: the selection half of the `nzkcs -f like ... mtype ?= ...` \
             move command. `like` is a case-insensitive SUBSTRING match over a declaration's name \
             or full name (not the subsequence match `fuzzy_search` does, which returns string \
             literals for `socket` and unrelated methods for `plug`); `mtype` filters on the \
             member's return type (`void`, `System.String`, `T`). Read-only: reports what would \
             move, sorted by file then line.",
             json!({"like": {"type": ["array", "string"],
                       "description": "Substrings to match against name/full, e.g. [\"sps\",\"pcs\",\"socket\",\"plug\"]."},
                    "mtype": {"type": ["array", "string"],
                       "description": "Return types to keep, e.g. [\"void\",\"System.String\",\"T\"]."},
                    "kind": {"type": ["array", "string"], "description": "Restrict to these node kinds."},
                    "limit": {"type": "integer", "default": 200}}), &[]),
        tool("move_decls", "MCP-only: the write half of the `nzkcs` move command, composing \
             select_decls -> a validated plan -> the atomic write path. Selector identical to \
             select_decls (`like`/`mtype`/`kind`/`limit`), or `from`, which is exact-only (node id, \
             exact full/label, then a unique exact simple name) and refuses with `near` matches when \
             the symbol is gone — it never falls back to a substring match. \
             Extract form: `newfile` (+ optional `prefix`/`namespace`, `dir`, `class`, `modifiers`) \
             moves each selected declaration with its leading comment/attribute/#if block into that \
             file, re-namespaced. Members are wrapped in `namespace <NS>{  <modifiers> class \
             <Class>{` (`modifiers` defaults to `public static partial`); the class comes from \
             `class` or a qualified `newfile` stem, else the call is refused rather than emitting a \
             namespace-level member. When the destination already exists it is loaded and written in \
             the same all-or-nothing batch as the origin delete. Rename-into form: `to` = \
             \"<Namespace>.<Class>.<member>\" reparents one declaration, creating the class/namespace \
             if absent. The moved block's dependencies (siblings left behind) are re-qualified to \
             their unique full name; if one cannot be resolved the call is REFUSED with the list in \
             `unresolved` unless `allow_unresolved:true`, which emits with loud `warnings`. A batch \
             that would create two members with the same name+parameter list is refused with \
             `duplicates` (CS0111). A stale slice no longer declaring its own name is refused. \
             Identifiers DECLARED inside the moved slice (the declaration and type names, generic \
             parameters, parameter names, `var`/for/foreach/using/catch/typed locals and lambda \
             parameters) are never re-qualified. \
             Deletions and call-site re-references are validated before anything is staged; the dry \
             run exposes the destination text as `destination.content`. On `apply`, the candidate \
             bytes of every file the commit would change are parsed by the real Roslyn driver: any \
             parse diagnostic REFUSES the whole batch (nothing written) unless `allow_invalid:true` \
             overrides it with a loud warning. `validated` reports `\"roslyn\"` when the gate ran and \
             passed, or `\"skipped:<reason>\"` when no toolchain is present (never a false pass). \
             Dry-run by default; `apply` writes (write). After an apply, call `index` to re-sync the \
             graph.",
             json!({"like": {"type": ["array", "string"],
                       "description": "Substrings to match against name/full, e.g. [\"sps\",\"socket\"]."},
                    "mtype": {"type": ["array", "string"], "description": "Return types to keep."},
                    "kind": {"type": ["array", "string"], "description": "Restrict to these node kinds."},
                    "limit": {"type": "integer", "default": 200},
                    "from": {"type": "string", "description": "Resolve exactly one declaration (exact id/full/label or a unique simple name)."},
                    "to": {"type": "string", "description": "Fully-qualified new home \"<Namespace>.<Class>.<member>\"."},
                    "newfile": {"type": "string", "description": "Destination file for the extract form."},
                    "dir": {"type": "string", "description": "Optional destination directory prefix for a bare `newfile`."},
                    "class": {"type": "string", "description": "Destination type for extracted members (inferred from a qualified newfile stem when absent)."},
                    "modifiers": {"type": "string", "description": "Class modifiers for the extract wrapper (default `public static partial`)."},
                    "prefix": {"type": "string", "description": "New namespace for the extract form (alias: `namespace`)."},
                    "namespace": {"type": "string", "description": "Alias for `prefix`."},
                    "allow_unresolved": {"type": "boolean", "default": false, "description": "Emit even when a moved-block dependency cannot be uniquely resolved; adds a loud `warnings` entry."},
                    "allow_invalid": {"type": "boolean", "default": false, "description": "Write even when the Roslyn parse gate reports diagnostics in the candidate content; adds a loud warning."},
                    "apply": {"type": "boolean", "default": false}}), &[]),
        tool("grep", "MCP-only regex/text search over the INDEXED corpus (never walks the disk): \
             returns `file:line:col:text` rows plus per-file and total counts, so a shell `grep -n` \
             one-liner becomes one MCP call. `pattern` is a Rust/RE2-style regex (linear time, no \
             backtracking); `literal:true` escapes it, `case` defaults to true (ignore case) and \
             `case:false` is case-sensitive. `glob` (shell-style, `**` crosses directories), \
             `path`/`paths` (repo-relative file or directory) scope the scan; `limit` caps rows \
             (files when `count_only`), `max_per_file` caps rows per file while keeping the true \
             count, and `context:N` adds before/after lines. Deterministic file/line order. \
             Read-only.",
             json!({"pattern": {"type": "string", "description": "Regex (or literal when `literal:true`)."},
                    "glob": {"type": ["string", "array"], "description": "Shell-style file glob(s), e.g. \"*.cs\" or \"sub/**\"."},
                    "path": {"type": "string", "description": "Repo-relative file or directory scope."},
                    "paths": {"type": ["string", "array"], "description": "Several repo-relative scopes."},
                    "limit": {"type": "integer", "default": 200},
                    "max_per_file": {"type": "integer", "default": 50},
                    "case": {"type": "boolean", "default": true, "description": "true (default) ignores case; false is case-sensitive."},
                    "literal": {"type": "boolean", "default": false},
                    "count_only": {"type": "boolean", "default": false},
                    "context": {"type": "integer", "default": 0}}), &["pattern"]),
        tool("reformat", "MCP-only: normalize one file (or one declaration) to the toolkit dialect \
             documented in the repo README: `/* */` comments only (`//` and `///` converted with the \
             text preserved verbatim; a body that would contain `*/` REFUSES), no `using` directives, \
             fully-qualified type names from the graph (ambiguous names are reported, never \
             guessed), the `namespace NS{  public static partial class X{` opener with 2-space \
             indentation, and trivial one-expression bodies collapsed onto the signature line. \
             Dry-run by default: the plan carries `before`/`after`, `hunks` and per-pass counts and \
             NEVER writes. `apply` writes atomically through the same temp+fsync+rename path \
             `move_decls` uses, gated by the real Roslyn parse driver: `validated:\"roslyn\"` when \
             the gate passed, `\"skipped:<reason>\"` + `unverified:true` when no toolchain is \
             present, and `require_validated:true` refuses an unverified write. `require_shape:true` \
             refuses when the namespace/class shape cannot be normalized.",
             json!({"file": {"type": "string", "description": "Indexed file to reformat."},
                    "decl": {"type": "string", "description": "Optional single declaration (node id, exact full/label/name) to reformat in place."},
                    "require_shape": {"type": "boolean", "default": false, "description": "Refuse unless the namespace/class shape was normalized."},
                    "require_validated": {"type": "boolean", "default": false, "description": "Refuse an apply when the validity gate was skipped."},
                    "allow_invalid": {"type": "boolean", "default": false},
                    "apply": {"type": "boolean", "default": false}}), &["file"]),
        tool("functionize", "MCP-only: take a dedupe cluster (a `find_duplicates` cluster object or \
             `members`/`ids` of node ids / {file,line} references), prove the members' bodies are \
             token-identical and their parameter lists and return types match, extract the common \
             body into one new shared static helper (`newfile` + `member`, with `class` or a \
             qualified newfile stem, optional `namespace`/`prefix`, `modifiers` default \
             `public static`), and rewrite every duplicate site to call it. Refuses rather than \
             guesses: differing bodies, differing signatures, a free identifier that cannot be \
             uniquely resolved (`allow_unresolved:true` overrides with loud warnings), a generic \
             member. Dry-run by default; `apply` is the same all-or-nothing atomic commit \
             `move_decls` uses, Roslyn-gated (`validated`, `unverified`, `require_validated`).",
             json!({"cluster": {"type": ["object", "array"], "description": "A find_duplicates exact cluster, or an array of member references."},
                    "members": {"type": ["array", "string"], "description": "Node ids, or {file,line} objects."},
                    "ids": {"type": ["array", "string"], "description": "Alias for `members`."},
                    "newfile": {"type": "string", "description": "Destination file for the new helper."},
                    "dir": {"type": "string", "description": "Optional destination directory prefix for a bare `newfile`."},
                    "class": {"type": "string", "description": "Helper class (inferred from a qualified newfile stem when absent)."},
                    "member": {"type": "string", "description": "Helper member name (alias: `name`)."},
                    "name": {"type": "string", "description": "Alias for `member`."},
                    "namespace": {"type": "string", "description": "Helper namespace (default: the members' common namespace)."},
                    "prefix": {"type": "string", "description": "Alias for `namespace`."},
                    "modifiers": {"type": "string", "description": "Helper member modifiers (default `public static`)."},
                    "allow_unresolved": {"type": "boolean", "default": false},
                    "require_validated": {"type": "boolean", "default": false, "description": "Refuse an apply when the validity gate was skipped."},
                    "allow_invalid": {"type": "boolean", "default": false},
                    "apply": {"type": "boolean", "default": false}}), &[]),
        tool("copy_path", "MCP-only: copy one file or a directory tree to a new path inside the \
             indexed repo (no `/api/*` route). `action:\"copy\"` (default) plans or applies a copy; \
             `action:\"recover\"` scans the durable journal (`<repo>/.csgraph-copy/`) and restores a \
             crashed apply from a previous process (idempotent): `{ok, action:\"recover\", journals, \
             restored, removed, removedDirs, incomplete}`. An apply writes an fsynced journal before \
             its first mutation; `apply:true` refuses while an unrecovered journal exists unless \
             `recover:true` recovers it first. Dry-run by default: a plan lists the directories \
             and files it would copy, their byte sizes and sha256s, and NEVER writes or creates \
             directories. `apply` is all-or-nothing like `apply_edits` (staged temp files, then \
             renames; on error it rolls back and reports `rolled_back`). Refuses symlinked \
             sources/destinations, `..` escapes, paths outside the repo, and `to` equal to or \
             inside `from`. Symlinks inside a source directory are skipped and reported. \
             `overwrite:false` (default) refuses if any destination file already exists, returning \
             `collisions`; `overwrite:true` replaces colliding files and keeps unrelated \
             destination files. After an apply, call `index` to re-sync the graph. \
             Aliases: `src`/`source` for `from`, `dest` for `to`.",
             json!({"action": {"type": "string", "enum": ["copy", "recover"], "default": "copy",
                               "description": "`copy` (default) plans/applies a copy; `recover` restores an unrecovered crash journal left by a previous process."},
                    "recover": {"type": "boolean", "default": false,
                               "description": "With action:\"copy\" and apply:true, recover any existing journal first, then proceed."},
                    "from": {"type": "string", "description": "Source, repo-relative or absolute, inside the repo. Aliases: `src`, `source`."},
                    "to": {"type": "string", "description": "Destination, repo-relative or absolute, inside the repo. Alias: `dest`."},
                    "overwrite": {"type": "boolean", "default": false, "description": "Allow replacing existing destination files."},
                    "apply": {"type": "boolean", "default": false, "description": "false = plan only, true = write."},
                    "max_files": {"type": "integer", "default": 20000, "description": "Refuse (honest) above this many files."},
                    "max_bytes": {"type": "integer", "default": 536870912, "description": "Refuse (honest) above this many total source bytes."}}),
             &["from", "to"]),
        tool("counts", "MCP-only first-class counts in one call (no field is ever added to the \
             frozen `find`/`refs` payloads). With no `q`/`node` it summarises the whole index: \
             `{files, nodes, declarations, byKind, edges, byEdgeKind}`. With `q` (node id, exact \
             full/label/simple name, then a declaration resolve) it returns exactly one symbol's \
             attributed graph counts: `{symbol:{id,full,label,name,kind,file,line,degree}, \
             definitions, matches, usages, incomingTotal, outgoingTotal, incoming:{kind:count}, \
             outgoing:{kind:count}, callers, usageLines}`. An ambiguous `q` is refused with the \
             candidate rows. Not an edit tool (no `/api/edit` entry), read-only. \
             `roots`/`paths`/`exclude`/`file` scope every count to a subset of the corpus \
             (alias `paths` for `roots`).",
             json!({"q": {"type": "string", "description": "Symbol: node id, exact full/label/name, or a unique declaration name."},
                    "node": {"type": "string", "description": "Alias for `q`."},
                    "roots": {"type": ["string", "array"], "description": "Repo-relative directory prefixes to scope every count to (alias: `paths`)."},
                    "paths": {"type": ["string", "array"], "description": "Alias for `roots`."},
                    "exclude": {"type": ["string", "array"], "description": "Glob(s) or repo-relative prefixes to subtract from the scope."},
                    "file": {"type": "string", "description": "Scope to one repo-relative file (optional sugar)."}}), &[]),
        tool("preview_change", "MCP-only READ-ONLY change preview (M3 slice 2): before any write, show \
             which signatures and call sites a rename or retype of ONE declaration would change. \
             Selector `node`/`id`/`symbol`/`from` resolves exactly one declaration (an unknown or \
             ambiguous selector is REFUSED, the ambiguous case with `ambiguity` candidates). `op` is \
             `\"rename\"` (`to` = a legal C# identifier, planned by the real `rename_symbol` dry \
             run) or `\"retype\"` (`to` = a legal C# type name; the declaration's declared type is \
             replaced). Returns the `currentSignature`, the `proposedSignature`, \
             `affectedDeclarations` (incoming callers/uses with file+line), every `callSites` row \
             with line numbers, the exact `edits` (before/after) an apply would make, `counts`, and \
             `truncated`. Never writes: `readOnly:true`, `applied:false`, `writes:[]`; there is no \
             apply path. Deterministic order (file, line).",
             json!({"node": {"type": "string", "description": "Declaration selector: node id, exact full/label/name, or unique simple name."},
                    "id": {"type": "string", "description": "Alias for `node`."},
                    "symbol": {"type": "string", "description": "Alias for `node`."},
                    "from": {"type": "string", "description": "Alias for `node` (rename-planner spelling)."},
                    "op": {"type": "string", "enum": ["rename", "retype"], "description": "The change to preview."},
                    "to": {"type": "string", "description": "New name (rename) or new declared type (retype)."},
                    "include_declarations": {"type": "boolean", "default": true, "description": "Include the declaration's own lines in the rename edit plan."}}),
             &["op", "to"]),
        tool("workspace", "MCP-only: report the exact scope this server holds. One process may be \
             bound to SEVERAL repos (`--repo` is repeatable; index 0 is the primary/default); every \
             tool takes an optional `repo` argument (1-based index, path, or unique basename; absent \
             = primary). `--root` is repeatable and global, and `index {roots:[...], repo:...}` \
             re-indexes the selected repo over different roots. Returns `{ok, repo` (=primary)`, \
             roots, indexed, files, nodes, servers, selected, defaultRepo, repos:[{index, repo, \
             roots, loaded, slots, default}], scoping:{multiRoot, reScope, repoSelector}, \
             limitation}`. Not an edit tool (no `/api/edit` entry), read-only.",
             json!({}), &[]),
        tool("parse_check", "MCP-only validity gate: runs the local Roslyn parse check \
             (`core/src/compile_check.rs`, the same call as `csgraph parse-check`) over the indexed \
             corpus, or the named subset, and returns its report verbatim: `{ok, toolchain, files, \
             count, diagnostics:[{severity,id,file,line,col,message}], skipped}`. `files`/`paths` \
             scope the selection (repo-relative) and `exclude` subtracts globs or prefixes; \
             `timeout` is seconds (1..30, default 25). `ok:false` with a concrete non-null `skipped` \
             reason when no Roslyn toolchain/driver is available — never a false pass. Use it before \
             `apply` so an edit cannot land invalid C#. Not an edit tool (no `/api/edit` entry), \
             read-only.",
             json!({"files": {"type": ["string", "array"], "description": "Repo-relative file(s) to check; default = the whole indexed corpus."},
                    "paths": {"type": ["string", "array"], "description": "Repo-relative file or directory scopes."},
                    "exclude": {"type": ["string", "array"], "description": "Glob(s) or repo-relative prefixes to exclude."},
                    "timeout": {"type": "integer", "default": 25, "description": "Seconds, 1..30."}}), &[]),
        tool("compile_diff", "MCP-only differential Roslyn compile — the in-process half of \
             `csgraph compile-diff`, so a workspace can be compiled and checked with NO Unity \
             process, NO Editor focus and NO Unity licence. Compiles the selected corpus \
             (candidate) and the same corpus minus every `baseline_exclude` match (baseline) \
             against one reference set, then reports only the diagnostics the candidate ADDS: \
             `{ok, skipped, toolchain, references, baselineDiagnostics, candidateDiagnostics, \
             newDiagnostics, worseDiagnostics, structuralDiagnostics, resolvedDiagnostics}`. \
             `files`/`paths`/`exclude` scope the candidate; `baseline_exclude` is the glob/prefix \
             list whose files are dropped for the baseline (e.g. the files a pass just added). \
             `data` names the Unity `Editor/Data` directory whose reference assemblies are used \
             (default: `CSVIEWER_UNITY_DATA`, then auto-discovery); `refs` adds extra reference \
             directories or DLLs; `define` adds defines on top of UNITY_EDITOR and \
             UNITY_2022_3_OR_NEWER; `timeout` is seconds (1..90, default 25). `references` names \
             exactly which assemblies were loaded, so a caller can prove a vendor assembly was \
             NOT available. `skipped` (with `ok:false`) means no compiler/toolchain exists — \
             never a false pass. Not an edit tool (no `/api/edit` entry), read-only.",
             json!({"files": {"type": ["string", "array"], "description": "Repo-relative file(s) to compile; default = the whole indexed corpus."},
                    "paths": {"type": ["string", "array"], "description": "Repo-relative file or directory scopes."},
                    "exclude": {"type": ["string", "array"], "description": "Glob(s) or repo-relative prefixes to drop from the candidate."},
                    "baseline_exclude": {"type": ["string", "array"], "description": "Glob(s) or repo-relative prefixes dropped from the baseline only (the files under test)."},
                    "data": {"type": "string", "description": "Unity Editor/Data directory; default CSVIEWER_UNITY_DATA then auto-discovery."},
                    "refs": {"type": ["string", "array"], "description": "Extra reference directories (every non-hidden *.dll below) or DLL paths."},
                    "define": {"type": ["string", "array"], "description": "Extra preprocessor defines."},
                    "timeout": {"type": "integer", "default": 25, "description": "Seconds, 1..90."}}), &[]),
        tool("find_duplicates", "MCP-only: the owner's priority-(2) duplicate-code report, calling \
             `core/src/dedupe.rs` directly (the exact function `csgraph dedupe` prints) and returning \
             that JSON verbatim: `{ok, repo, roots, files, methodBodies, units, minLines, threshold, \
             exact, near, blocks, stats:{exactClusters,nearClusters,blockGroups,pairComparisons}, \
             truncated}`. Exact duplicates are token-normalised method/constructor bodies; `near` \
             clusters are Jaccard-similar above `threshold`; `blocks` are recurring runs of \
             `>= min_lines` non-trivial lines. Deterministic order and capped; `truncated` is \
             surfaced honestly (never a false empty report). `min_lines`/`threshold` map to the CLI \
             flags, `limit` caps clusters/blocks, and `roots`/`paths`/`exclude` scope the scan \
             (defaults to the indexed roots). Not an edit tool (no `/api/edit` entry), read-only.",
             json!({"min_lines": {"type": "integer", "description": "Minimum run length for a repeated block (default 4)."},
                    "threshold": {"type": "number", "description": "Near-duplicate Jaccard threshold 0..=1 (default 0.80)."},
                    "limit": {"type": "integer", "description": "Cap on returned exact clusters and block groups."},
                    "roots": {"type": ["string", "array"], "description": "Directories to scan, relative to the repo (default: the indexed roots)."},
                    "paths": {"type": ["string", "array"], "description": "Alias for `roots`."},
                    "exclude": {"type": ["string", "array"], "description": "Path components/patterns to exclude."},
                    "max_clusters": {"type": "integer"}, "max_members": {"type": "integer"},
                    "max_blocks": {"type": "integer"}}), &[]),
    ]
}

/// Resolve `(name, arguments)` to the route call. This is the single mapping
/// between the MCP surface and `/api/*`.
fn resolve_call(name: &str, args: &Value) -> Result<Call, String> {
    let empty = json!({});
    let a = if args.is_object() { args } else { &empty };
    let get = |name: &str| -> Call {
        Call {
            method: "GET".to_string(),
            path: name.to_string(),
            query: a.clone(),
            body: None,
        }
    };
    let require = |key: &str| -> Result<(), String> {
        match a.get(key) {
            Some(Value::String(s)) if !s.is_empty() => Ok(()),
            Some(Value::Number(_)) | Some(Value::Bool(_)) => Ok(()),
            Some(_) => Err(format!("argument {key:?} must be a non-empty string")),
            None => Err(format!("argument {key:?} is required")),
        }
    };
    let call = match name {
        "api_call" => {
            require("path")?;
            let method = a
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_uppercase();
            Call {
                method,
                path: a["path"].as_str().unwrap_or("").to_string(),
                query: a.get("query").cloned().unwrap_or_else(|| json!({})),
                body: a.get("body").cloned(),
            }
        }
        "index" => Call { method: "POST".into(), path: "/api/index".into(), query: json!({}), body: Some(a.clone()) },
        "index_cancel" => Call { method: "POST".into(), path: api::CANCEL_PATH.into(), query: json!({}), body: Some(json!({})) },
        "viewer_state" | "settings" => {
            let body = a.get("body").cloned();
            match body {
                Some(b) if !b.is_null() => Call {
                    method: "POST".into(),
                    path: format!("/api/{name}"),
                    query: json!({}),
                    body: Some(b),
                },
                _ => {
                    let mut q = a.clone();
                    if let Some(o) = q.as_object_mut() {
                        o.remove("body");
                    }
                    Call { method: "GET".into(), path: format!("/api/{name}"), query: q, body: None }
                }
            }
        }
        "reorg" => Call { method: "POST".into(), path: "/api/reorg".into(), query: json!({}), body: Some(a.clone()) },
        "browse" => get("/api/browse"),
        "search" => { require("q")?; get("/api/search") }
        "node" => { require("id")?; get("/api/node") }
        "relation" => { require("node")?; require("key")?; get("/api/relation") }
        "find" => { require("q")?; get("/api/find") }
        "refs" => { require("q")?; get("/api/refs") }
        "source" => get("/api/source"),
        "doc" => { require("node")?; get("/api/doc") }
        "decls" => get("/api/decls"),
        "chain" => { require("id")?; get("/api/chain") }
        "kinds" => get("/api/kinds"),
        "definitions" => get("/api/definitions"),
        "subgraph" => get("/api/subgraph"),
        "export" => get("/api/export.json"),
        "projects" => get("/api/projects"),
        "state" => get("/api/state"),
        other => return Err(format!("unknown tool: {other}")),
    };
    Ok(call)
}

/// The envelope the tool result carries: the HTTP status the route would send,
/// the unified success flag (`ok` = `status < 400`), and its body. Serialized in
/// one pass so a shared (memoized) document is streamed straight out instead of
/// being deep-cloned into a second `Value`.
#[derive(serde::Serialize)]
struct Envelope<T: serde::Serialize> {
    status: u16,
    ok: bool,
    body: T,
}

/// The MCP tool result for one `(status, body)`. The text is
/// `{"status": <http status>, "ok": <status < 400>, "body": <route body>}` — the
/// unified `ok` signal every tool result carries; `isError` is unchanged and is
/// set for a status >= 400 so an agent can tell a refusal from a payload.
fn envelope_result<T: serde::Serialize>(status: u16, body: T) -> Value {
    let text = serde_json::to_string(&Envelope { status, ok: status < 400, body })
        .unwrap_or_else(|_| format!("{{\"status\":{status},\"ok\":{},\"body\":null}}", status < 400));
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": status >= 400,
    })
}

/// The MCP tool result for one routed call, through the API the HTTP host uses.
fn dispatch_call(index: &mut Index, call: &Call) -> Value {
    let (status, payload) = if call.method.eq_ignore_ascii_case("GET") {
        api::handle_payload(index, "GET", &call.path, &call.query, None)
    } else {
        let (s, v) = api::handle(index, &call.method, &call.path, &call.query, call.body.clone());
        (s, Payload::Value(v))
    };
    envelope_result(status, payload)
}

/// True when a routed result carries the engine's `"cancelled": true` state,
/// i.e. the build unwound and did **not** install a graph.
fn result_is_cancelled(result: &Value) -> bool {
    result["content"][0]["text"]
        .as_str()
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .map(|env| env["body"]["cancelled"] == Value::Bool(true))
        .unwrap_or(false)
}

fn tool_error(message: impl Into<String>) -> Value {
    json!({
        "content": [{"type": "text", "text": message.into()}],
        "isError": true,
    })
}

/// Execute one tool against one index. Used by tests and by the single-slot
/// path of a worker.
///
/// The MCP-only refactor/query family ([`crate::edit`]) is dispatched here,
/// before the `/api/*` routing: those tools deliberately bypass
/// [`resolve_call`] / [`dispatch_call`], so the frozen route table is untouched.
pub fn invoke_on_index(index: &mut Index, name: &str, args: &Value) -> Value {
    let args = strip_repo(args);
    if name == "workspace" {
        let views = vec![RepoView {
            repo: index.repo_path().to_string_lossy().into_owned(),
            roots: effective_roots(index),
            loaded: index.indexed,
            slots: 1,
            default: true,
        }];
        return aux_result(workspace_body(&views, 0, index));
    }
    if name == "preview_change" {
        // M3 slice 2: read-only, always dry. It is neither an edit tool nor a
        // route tool, so it is dispatched here, before the `/api/*` fallback.
        return aux_result(crate::preview::invoke(index, &args));
    }
    if crate::edit::is_edit_tool(name) {
        return crate::edit::invoke(index, name, &args);
    }
    if crate::edit::is_aux_tool(name) {
        return crate::edit::invoke_aux(index, name, &args);
    }
    match resolve_call(name, &args) {
        Ok(call) => dispatch_call(index, &call),
        Err(e) => tool_error(e),
    }
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

/// One bound repository plus its worker-slot pool.
///
/// Each repo owns `threads` independent `Mutex<Index>` replicas and its own
/// free-slot channel and write gate, so a broadcast rebuild of one repo can
/// never hold up a read of another. `loaded` starts true only for the primary
/// pre-indexed at startup; every other repo is built lazily under `write_gate`
/// on its first routed call.
struct RepoSlot {
    /// The path exactly as passed to `Index::new` (as given on `--repo`).
    path: PathBuf,
    /// Canonicalised path (symlinks resolved when possible) for `repo` matching.
    canonical: PathBuf,
    /// Absolute, lexically-normalised path (mirrors `Index::new`'s `abspath`).
    absolute: PathBuf,
    slots: Vec<Mutex<Index>>,
    free_tx: Sender<usize>,
    free_rx: Receiver<usize>,
    /// Serialises whole-pool broadcasts (`/api/index`) and the lazy first build.
    write_gate: Mutex<()>,
    /// One stop flag for this repo, cloned into every slot with
    /// [`Index::set_cancel_flag`]. A cancel stores it without touching the
    /// write gate or a slot, so it reaches an in-flight build immediately and
    /// every replica of this repo at once — and no other repo.
    cancel: Arc<AtomicBool>,
    /// True once a build has run for this repo (successful or refused).
    loaded: AtomicBool,
}

impl RepoSlot {
    fn is_loaded(&self) -> bool {
        self.loaded.load(Ordering::SeqCst)
    }

    /// The first replica, used for cheap read-only metadata (`workspace`,
    /// cancel flag) without taking the free-slot channel.
    fn primary_index(&self) -> std::sync::MutexGuard<'_, Index> {
        lock_index(&self.slots[0])
    }

    fn free_slot(&self) -> Result<usize, Value> {
        self.free_rx
            .recv()
            .map_err(|_| tool_error("server is shutting down"))
    }
}

struct Shared {
    repos: Vec<RepoSlot>,
    cfg: Config,
    /// stdout is protocol-only; a lock keeps response lines from interleaving.
    out: Mutex<io::Stdout>,
}

fn lock_index<'a>(slot: &'a Mutex<Index>) -> std::sync::MutexGuard<'a, Index> {
    slot.lock().unwrap_or_else(|e| e.into_inner())
}

fn write_line(out: &Mutex<io::Stdout>, value: &Value) {
    let line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let mut guard = out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = guard.write_all(line.as_bytes());
    let _ = guard.write_all(b"\n");
    let _ = guard.flush();
}

/// The MCP-only envelope: `isError` is set when the body reports `ok:false`.
fn aux_result(body: Value) -> Value {
    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let text = serde_json::to_string(&body).unwrap_or_else(|_| "{\"ok\":false}".to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": !ok,
    })
}

// ---------------------------------------------------------------------------
// repo resolution
// ---------------------------------------------------------------------------

/// Absolute, lexically-normalised form of `p` (the same normalisation
/// `Index::new` applies), so a selector can be compared without touching disk.
fn absolute(p: &std::path::Path) -> PathBuf {
    crate::index::abspath(p)
}

/// Canonical form when the path exists, else the lexical absolute form.
fn canonical_repo(p: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| absolute(p))
}

/// Collapse duplicate repos (same canonical path) into one entry, first wins,
/// preserving the order of `--repo` occurrences. Index 0 stays the primary.
fn dedupe_repo_paths(repos: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(repos.len());
    let mut seen: Vec<PathBuf> = Vec::new();
    for p in repos {
        let canon = canonical_repo(p);
        if seen.iter().any(|c| *c == canon) {
            continue;
        }
        seen.push(canon);
        out.push(p.clone());
    }
    out
}

/// A human listing of the bound repos, each with its 1-based index.
fn repos_listing(repos: &[RepoSlot]) -> String {
    repos
        .iter()
        .enumerate()
        .map(|(i, r)| {
            format!(
                "{}: {}{}",
                i + 1,
                r.path.display(),
                if i == 0 { " (default)" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The error for an unknown/ambiguous `repo` selector. It names every bound
/// repo with its 1-based index so the caller can retry.
fn repo_error(repos: &[RepoSlot], selector: &Value) -> String {
    format!("unknown repo {selector}; bound repos: {}", repos_listing(repos))
}

/// Resolve a `repo` argument to a bound repo index.
///
/// Order: an exact canonical path (absolute, or relative to the process cwd),
/// then a 1-based index (`"2"` or `2`), then a unique case-insensitive basename.
/// Unknown or ambiguous is an error.
fn resolve_repo_selector(repos: &[RepoSlot], selector: &Value) -> Result<usize, String> {
    let n = repos.len();
    let as_index = |i: u64| -> Option<usize> {
        if i >= 1 && (i as usize) <= n {
            Some(i as usize - 1)
        } else {
            None
        }
    };
    match selector {
        Value::Number(num) => num
            .as_u64()
            .and_then(as_index)
            .ok_or_else(|| repo_error(repos, selector)),
        Value::String(s) => {
            let text = s.trim();
            // 1. exact canonical path, or a path relative to the cwd.
            if !text.is_empty() {
                let given = PathBuf::from(text);
                let joined = if given.is_absolute() {
                    given
                } else {
                    std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .join(&given)
                };
                let canon = canonical_repo(&joined);
                let lex = absolute(&joined);
                if let Some(i) = repos
                    .iter()
                    .position(|r| r.canonical == canon || r.absolute == lex)
                {
                    return Ok(i);
                }
            }
            // 2. a 1-based index spelled as a string.
            if let Ok(i) = text.parse::<u64>() {
                if let Some(i) = as_index(i) {
                    return Ok(i);
                }
            }
            // 3. a unique basename, case-insensitive.
            let want = text.trim_end_matches(['/', '\\']).to_lowercase();
            if !want.is_empty() {
                let hits: Vec<usize> = repos
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| {
                        r.path
                            .file_name()
                            .map(|f| f.to_string_lossy().to_lowercase() == want)
                            .unwrap_or(false)
                    })
                    .map(|(i, _)| i)
                    .collect();
                if hits.len() == 1 {
                    return Ok(hits[0]);
                }
                if hits.len() > 1 {
                    let list = hits
                        .iter()
                        .map(|i| format!("{}: {}", i + 1, repos[*i].path.display()))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(format!(
                        "ambiguous repo {selector:?}: matches {list}; use a path or a 1-based index"
                    ));
                }
            }
            Err(repo_error(repos, selector))
        }
        _ => Err(format!(
            "argument \"repo\" must be a string or integer; bound repos: {}",
            repos_listing(repos)
        )),
    }
}

/// Drop the server-only `repo` key from an argument map. The result is what
/// reaches `resolve_call` / `dispatch_call` / [`crate::edit`], so the key can
/// never leak into a routed or edit payload.
fn strip_repo(args: &Value) -> Value {
    match args.as_object() {
        Some(obj) => {
            let mut clone = obj.clone();
            clone.remove("repo");
            Value::Object(clone)
        }
        None => json!({}),
    }
}

/// Strip the server-only `repo` key from an argument map and resolve it.
fn resolve_repo_argument(shared: &Shared, args: &Value) -> Result<(usize, Value), String> {
    let stripped = strip_repo(args);
    match args.get("repo").filter(|v| !v.is_null()) {
        None => Ok((0, stripped)),
        Some(sel) => Ok((resolve_repo_selector(&shared.repos, sel)?, stripped)),
    }
}

/// Build the selected repo (all its slots) once, under its write gate.
///
/// A cold primary with no `--root` is left alone: that is the unchanged
/// "start cold, call the `index` tool" contract. Every secondary repo — and
/// any repo whose `--root` was given — builds on its first routed call,
/// exactly like an explicit `index`: when no global `--root` was given the
/// build falls back to [`Config::default`]'s roots, the same roots the `index`
/// route would use on a cold primary.
fn ensure_loaded(shared: &Shared, idx: usize) {
    let repo = &shared.repos[idx];
    if repo.is_loaded() {
        return;
    }
    if idx == 0 && shared.cfg.roots.is_empty() {
        return;
    }
    let mut cfg = shared.cfg.clone();
    if cfg.roots.is_empty() {
        cfg.roots = Config::default().roots;
    }
    let _gate = repo.write_gate.lock().unwrap_or_else(|e| e.into_inner());
    if repo.is_loaded() {
        return;
    }
    for slot in &repo.slots {
        let mut index = lock_index(slot);
        if !index.indexed {
            index.build(&cfg.roots, &cfg.excludes, &cfg);
        }
    }
    repo.loaded.store(true, Ordering::SeqCst);
}

/// The roots a repo's first replica reports: its configured roots, or `["."]`
/// for a repo that has never been built.
fn effective_roots(index: &Index) -> Vec<String> {
    if index.roots().is_empty() {
        vec![".".to_string()]
    } else {
        index.roots().to_vec()
    }
}

/// A read-only snapshot of one bound repo for the `workspace` tool.
struct RepoView {
    repo: String,
    roots: Vec<String>,
    loaded: bool,
    slots: usize,
    default: bool,
}

/// The `workspace` payload. `repo` is always the primary (index 0); the
/// `selected`/`roots`/`indexed`/`files`/`nodes` fields describe the repo this
/// call selected. `repos` reports every bound repo with its 1-based index.
fn workspace_body(views: &[RepoView], selected: usize, index: &Index) -> Value {
    let n = views.len();
    let fallback = RepoView {
        repo: index.repo_path().to_string_lossy().into_owned(),
        roots: effective_roots(index),
        loaded: index.indexed,
        slots: 1,
        default: true,
    };
    let repos_json: Vec<Value> = views
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json!({
                "index": i + 1,
                "repo": v.repo.clone(),
                "roots": v.roots.clone(),
                "loaded": v.loaded,
                "slots": v.slots,
                "default": v.default,
            })
        })
        .collect();
    let primary = views.first().unwrap_or(&fallback);
    let selected_view = views.get(selected).unwrap_or(primary);
    let roots = effective_roots(index);
    json!({
        "ok": true,
        "tool": "workspace",
        "repo": primary.repo.clone(),
        "roots": roots,
        "indexed": index.indexed,
        "files": index.file_paths().len(),
        "nodes": index.nodes().len(),
        "servers": format!("one process, {} bound {}", n, if n == 1 { "repo" } else { "repos" }),
        "selected": selected_view.repo.clone(),
        "defaultRepo": primary.repo.clone(),
        "repos": repos_json,
        "scoping": {
            "multiRoot": "the roots of one repo, set at spawn with repeatable `--root` (global: every bound repo uses them)",
            "reScope": "call `index {roots:[\"...\", \"...\"], repo:...}` to re-index the selected repo over different roots",
            "repoSelector": "every tool takes `repo`: a 1-based index (\"2\" or 2), a path, or a unique basename; absent = the primary repo. `index`/`index_cancel` act on the selected repo only.",
        },
        "limitation": format!(
            "{} {} bound to one process; `--root`/`--exclude` are global and every tool selects \
             a repo with the optional `repo` argument (absent = primary). `index` rebuilds only \
             the selected repo.",
            n,
            if n == 1 { "repo is" } else { "repos are" }
        ),
    })
}

/// Run one tool call against the repo selected by its `repo` argument (absent =
/// primary), using one free slot of that repo (or all of them when the route
/// installs a new graph). Never panics on a poisoned lock.
fn run_tool(shared: &Shared, name: &str, args: &Value) -> Value {
    let (repo_idx, args) = match resolve_repo_argument(shared, args) {
        Ok(v) => v,
        Err(e) => return tool_error(e),
    };
    let repo = &shared.repos[repo_idx];

    // `workspace` reports the whole bound set; it is handled here so it can see
    // every repo instead of one `Index`.
    if name == "workspace" {
        let views: Vec<RepoView> = shared
            .repos
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let index = r.primary_index();
                RepoView {
                    repo: r.path.to_string_lossy().into_owned(),
                    roots: effective_roots(&index),
                    loaded: r.is_loaded(),
                    slots: r.slots.len(),
                    default: i == 0,
                }
            })
            .collect();
        let index = repo.primary_index();
        return aux_result(workspace_body(&views, repo_idx, &index));
    }

    // MCP-only tools are not routes: take one slot of the selected repo.
    if crate::edit::is_edit_tool(name)
        || crate::edit::is_aux_tool(name)
        || name == "preview_change"
    {
        ensure_loaded(shared, repo_idx);
        let slot = match repo.free_slot() {
            Ok(s) => s,
            Err(e) => return e,
        };
        let result = {
            let mut index = lock_index(&repo.slots[slot]);
            if name == "preview_change" {
                aux_result(crate::preview::invoke(&mut index, &args))
            } else if crate::edit::is_edit_tool(name) {
                crate::edit::invoke(&mut index, name, &args)
            } else {
                crate::edit::invoke_aux(&index, name, &args)
            }
        };
        let _ = repo.free_tx.send(slot);
        return result;
    }

    let call = match resolve_call(name, &args) {
        Ok(c) => c,
        Err(e) => return tool_error(e),
    };
    // Stop is answered out-of-band, exactly like the Tauri host intercepts the
    // cancel path before it takes the engine lock: store this repo's flag and
    // reply. No write gate (an in-flight rebuild holds it for its whole run),
    // no slot (a broadcast rebuild holds every slot) — otherwise this call
    // would queue behind the very build it is meant to interrupt. Other repos
    // are untouched.
    if call.path == api::CANCEL_PATH && call.method.eq_ignore_ascii_case("POST") {
        repo.cancel.store(true, Ordering::SeqCst);
        return envelope_result(200, json!({"ok": true, "cancelled": true}));
    }
    if call.broadcasts() {
        // A cold repo must first learn the global roots/excludes: an explicit
        // `index` on a secondary would otherwise fall back to
        // `Config::default()`'s roots. This is a no-op for an already-hot repo
        // and for a cold primary with no `--root` (the unchanged "start cold"
        // contract, where the route itself uses the default roots).
        ensure_loaded(shared, repo_idx);
        let _gate = repo.write_gate.lock().unwrap_or_else(|e| e.into_inner());
        // Wait until every slot of THIS repo is idle, then apply the same call
        // to each so a rebuilt graph is visible from every one of its workers.
        let mut ids: Vec<usize> = Vec::with_capacity(repo.slots.len());
        for _ in 0..repo.slots.len() {
            match repo.free_rx.recv() {
                Ok(id) => ids.push(id),
                Err(_) => break,
            }
        }
        let mut first: Option<Value> = None;
        for &id in &ids {
            let mut index = lock_index(&repo.slots[id]);
            let result = dispatch_call(&mut index, &call);
            // A rebuild that unwound did not install a graph; a cancel that
            // landed too late (flag set, build finished) means this broadcast
            // should stop here too. Either way the remaining slots keep their
            // previous graph. `Index::build` clears the flag at its start, so
            // the next `index` call begins clean.
            let stop = result_is_cancelled(&result) || repo.cancel.load(Ordering::SeqCst);
            if first.is_none() {
                first = Some(result);
            }
            if stop {
                break;
            }
        }
        for &id in &ids {
            let _ = repo.free_tx.send(id);
        }
        // A build ran (successfully, refused, or cancelled): don't rebuild it
        // again on the next read.
        repo.loaded.store(true, Ordering::SeqCst);
        return first.unwrap_or_else(|| tool_error("no worker slots"));
    }
    // Ordinary routed read/write: build the selected repo once if it is cold.
    ensure_loaded(shared, repo_idx);
    let slot = match repo.free_slot() {
        Ok(s) => s,
        Err(e) => return e,
    };
    let result = {
        let mut index = lock_index(&repo.slots[slot]);
        dispatch_call(&mut index, &call)
    };
    let _ = repo.free_tx.send(slot);
    result
}

fn worker(shared: Arc<Shared>, jobs: Receiver<Value>, done: Sender<()>) {
    while let Ok(req) = jobs.recv() {
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let params = req.get("params").cloned().unwrap_or(json!({}));
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let result = run_tool(&shared, &name, &args);
        write_line(&shared.out, &ok_response(id, result));
    }
    let _ = done.send(());
}

/// Build one index, reusing the on-disk graph cache when the signature matches.
/// The shared stop flag is wired in before the first build.
#[cfg(test)]
fn build_slot(
    repo: &PathBuf,
    roots: &[String],
    excludes: &[String],
    cfg: &Config,
    cancel: &Arc<AtomicBool>,
) -> Index {
    let mut index = Index::new(repo.clone());
    index.set_cancel_flag(cancel.clone());
    if !roots.is_empty() {
        index.build(roots, excludes, cfg);
    }
    index
}

/// One repo's slot pool: `threads` replicas of its `Index`, pre-built from the
/// startup cache when `pre_index` (the primary with a `--root`).
fn make_repo_slot(
    path: PathBuf,
    threads: usize,
    cancel: Arc<AtomicBool>,
    cfg: &Config,
    pre_index: bool,
) -> RepoSlot {
    let mut slots: Vec<Mutex<Index>> = Vec::with_capacity(threads);
    for _ in 0..threads {
        let mut index = Index::new(path.clone());
        index.set_cancel_flag(cancel.clone());
        if pre_index {
            index.build(&cfg.roots, &cfg.excludes, cfg);
        }
        slots.push(Mutex::new(index));
    }
    let (free_tx, free_rx) = crossbeam_channel::bounded::<usize>(threads);
    for id in 0..threads {
        let _ = free_tx.send(id);
    }
    RepoSlot {
        canonical: canonical_repo(&path),
        absolute: absolute(&path),
        path,
        slots,
        free_tx,
        free_rx,
        write_gate: Mutex::new(()),
        cancel,
        loaded: AtomicBool::new(pre_index),
    }
}

/// Serve MCP over stdio until EOF. Blocks until the transport closes.
///
/// On EOF the job channel is dropped and the workers drain; the process then
/// returns. SIGTERM terminates the process through the default handler.
pub fn run(opts: McpOptions) -> Result<(), String> {
    let threads = opts.resolved_threads();
    let excludes = opts.resolved_excludes();
    if opts.repos.is_empty() {
        return Err("at least one --repo is required".to_string());
    }
    // The excludes live in the config as well as the startup build: a later
    // `index` call rebuilds with `index.config().excludes`, so both must be the
    // same list or the two builds compute different cache signatures and the
    // graph cache never matches. The same `cfg` is shared by every repo, so
    // `--root`/`--exclude` are global and apply to every bound repo.
    let cfg = Config {
        roots: opts.roots.clone(),
        excludes: excludes.clone(),
        defines: Vec::new(),
        player: false,
        ..Default::default()
    };

    // `--repo` is repeatable. Duplicate canonical paths collapse into one entry
    // (first wins); no error.
    let repo_paths = dedupe_repo_paths(&opts.repos);

    // Only the primary is pre-indexed when a `--root` was given (unchanged
    // behaviour; its replicas load the cache the first build wrote). Every
    // other repo starts cold and builds on its first call. One stop flag per
    // repo, cloned into each of that repo's slots.
    let t0 = std::time::Instant::now();
    let mut repos: Vec<RepoSlot> = Vec::with_capacity(repo_paths.len());
    for (i, path) in repo_paths.iter().enumerate() {
        let cancel = Arc::new(AtomicBool::new(false));
        let pre_index = i == 0 && !opts.roots.is_empty();
        repos.push(make_repo_slot(path.clone(), threads, cancel, &cfg, pre_index));
    }
    if !opts.roots.is_empty() {
        eprintln!(
            "csgraph mcp: indexed repo={} roots={:?} file_count={} in {:.2}s ({} workers)",
            repos[0].path.display(),
            opts.roots,
            repos[0].primary_index().state()["fileCount"],
            t0.elapsed().as_secs_f64(),
            threads
        );
    } else {
        eprintln!(
            "csgraph mcp: cold start repo={} (no --root; call the `index` tool to build) ({} workers)",
            repos[0].path.display(),
            threads
        );
    }
    if repos.len() > 1 {
        eprintln!(
            "csgraph mcp: {} repos bound; {} secondary repo(s) cold until their first call",
            repos.len(),
            repos.len() - 1
        );
    }

    let (job_tx, job_rx) = crossbeam_channel::unbounded::<Value>();
    let (done_tx, done_rx) = crossbeam_channel::unbounded::<()>();

    let shared = Arc::new(Shared {
        repos,
        cfg,
        out: Mutex::new(io::stdout()),
    });

    let mut handles = Vec::with_capacity(threads);
    for _ in 0..threads {
        let shared = shared.clone();
        let jobs = job_rx.clone();
        let done = done_tx.clone();
        handles.push(thread::Builder::new()
            .name("csgraph-mcp".to_string())
            .spawn(move || worker(shared, jobs, done))
            .map_err(|e| format!("could not start a worker thread: {e}"))?);
    }
    drop(done_tx);

    // ---- read loop: never blocks on a worker ------------------------------
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                write_line(&shared.out, &err_response(Value::Null, -32700, format!("parse error: {e}")));
                continue;
            }
        };
        if !msg.is_object() {
            write_line(&shared.out, &err_response(Value::Null, -32600, "invalid request"));
            continue;
        }
        match handle_message(&msg, threads) {
            Handling::Ignore => {}
            Handling::Reply(resp) => write_line(&shared.out, &resp),
            Handling::Dispatch(req) => {
                let _ = job_tx.send(req);
            }
        }
    }

    // ---- graceful drain ----------------------------------------------------
    drop(job_tx);
    let mut finished = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(DRAIN_TIMEOUT_S);
    while finished < threads {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match done_rx.recv_timeout(left) {
            Ok(()) => finished += 1,
            Err(_) => break,
        }
    }
    eprintln!("csgraph mcp: stdin closed; {finished}/{threads} workers drained");
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::fs;

    fn fixture(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("csgraph-mcp-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("a.cs"),
            "namespace N {\n public partial class C {\n  public int A() { return B(); }\n  public int B() { return 2; }\n }\n}\n",
        )
        .unwrap();
        dir
    }

    fn indexed(tag: &str) -> (PathBuf, Index) {
        let dir = fixture(tag);
        let mut ix = Index::new(dir.clone());
        let cfg = Config::default();
        ix.build(&[".".to_string()], &[], &cfg);
        assert!(ix.indexed);
        (dir, ix)
    }

    #[test]
    fn initialize_negotiates_the_requested_protocol() {
        for p in SUPPORTED_PROTOCOLS {
            assert_eq!(negotiate_protocol(Some(p)), *p);
        }
        assert_eq!(negotiate_protocol(Some("1999-01-01")), LATEST_PROTOCOL);
        assert_eq!(negotiate_protocol(None), LATEST_PROTOCOL);

        let init = handle_message(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "t"}}}),
            3,
        );
        let resp = match init {
            Handling::Reply(r) => r,
            _ => panic!("initialize must reply"),
        };
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(resp["result"]["capabilities"], json!({"tools": {}}));
        assert_eq!(resp["result"]["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(resp["result"]["serverInfo"]["threads"], 3);
    }

    #[test]
    fn tools_list_has_one_entry_per_tool_and_covers_every_route() {
        let listed = tools();
        let names: BTreeSet<&str> = listed.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for want in [
            "api_call", "state", "index", "index_cancel", "browse", "search", "node", "relation",
            "find", "refs", "source", "doc", "decls", "chain", "kinds", "definitions", "subgraph",
            "export", "projects", "viewer_state", "settings", "reorg", "preview_change",
        ] {
            assert!(names.contains(want), "tool {want} missing from tools/list");
        }

        // Every route literal in api.rs (outside its test module) must be named
        // by at least one tool description. `include_str!` keeps this honest
        // when a route is added.
        let src = include_str!("api.rs");
        let cutoff = src.find("#[cfg(test)]").unwrap_or(src.len());
        let declared = &src[..cutoff];
        let mut routes: BTreeSet<String> = BTreeSet::new();
        let re = regex::Regex::new(r#""(/api/[A-Za-z0-9_./-]*)""#).unwrap();
        for cap in re.captures_iter(declared) {
            routes.insert(cap[1].to_string());
        }
        assert!(routes.len() >= 20, "expected many routes, got {routes:?}");
        let described: String = listed
            .iter()
            .map(|t| t["description"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        for r in &routes {
            assert!(described.contains(r.as_str()), "route {r} is not in any tool description");
        }
        // And the negative: nothing invents a route api.rs does not have.
        assert!(!described.contains("/api/does-not-exist"));
    }

    #[test]
    fn edit_tools_are_listed_and_dispatched_without_a_route() {
        let listed = tools();
        assert_eq!(
            listed.len(),
            42,
            "22 routed tools + 14 MCP-only edit tools + 5 aux tools + 1 read-only preview tool"
        );
        let names: BTreeSet<&str> = listed.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for want in [
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
        ] {
            assert!(names.contains(want), "tool {want} missing from tools/list");
            // The MCP-only family must bypass the /api/* routing entirely.
            assert!(crate::edit::is_edit_tool(want));
            assert!(resolve_call(want, &json!({})).is_err());
        }
        // The aux (non-edit) MCP-only tools are listed and dispatched without a
        // route, but are deliberately NOT in edit::TOOLS.
        for want in ["parse_check", "compile_diff", "find_duplicates", "counts", "workspace"] {
            assert!(names.contains(want), "tool {want} missing from tools/list");
            assert!(crate::edit::is_aux_tool(want));
            assert!(!crate::edit::is_edit_tool(want));
            assert!(resolve_call(want, &json!({})).is_err());
        }

        // `copy_path` carries the full D1 argument schema, aliases named.
        let cp = listed.iter().find(|t| t["name"] == "copy_path").unwrap();
        let props = &cp["inputSchema"]["properties"];
        for arg in [
            "from", "to", "overwrite", "apply", "max_files", "max_bytes", "action", "recover",
        ] {
            assert!(props.get(arg).is_some(), "copy_path schema missing arg {arg}");
        }
        assert_eq!(cp["inputSchema"]["required"], json!(["from", "to"]));
        let cp_desc = cp["description"].as_str().unwrap();
        for alias in ["src", "source", "dest"] {
            assert!(cp_desc.contains(alias), "copy_path description must name alias {alias}");
        }

        // `counts` carries the D4 scope args registered here.
        let counts = listed.iter().find(|t| t["name"] == "counts").unwrap();
        let cprops = &counts["inputSchema"]["properties"];
        for arg in ["roots", "paths", "exclude", "file"] {
            assert!(cprops.get(arg).is_some(), "counts schema missing scope arg {arg}");
        }

        let (dir, mut ix) = indexed("edit-dispatch");
        let r = invoke_on_index(&mut ix, "fuzzy_search", &json!({"q": "N.C.A"}));
        assert_eq!(r["isError"], false, "{r}");
        let env: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(env["ok"], true);
        assert_eq!(env["results"][0]["id"], "N.C.A()");
        // The first-class counts tool answers both scopes without a route.
        let r = invoke_on_index(&mut ix, "counts", &json!({}));
        let env: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(env["ok"], true, "{env}");
        assert_eq!(env["scope"], "corpus", "{env}");
        assert_eq!(env["nodes"].as_i64().unwrap() > 0, true, "{env}");
        let r = invoke_on_index(&mut ix, "counts", &json!({"q": "N.C.A"}));
        let env: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(env["ok"], true, "{env}");
        assert_eq!(env["scope"], "symbol", "{env}");
        assert_eq!(env["symbol"]["id"], "N.C.A()", "{env}");
        // `workspace` names the bound repo/roots and the bound set honestly.
        let r = invoke_on_index(&mut ix, "workspace", &json!({}));
        let env: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(env["ok"], true, "{env}");
        assert_eq!(env["indexed"], true, "{env}");
        assert_eq!(env["repos"].as_array().unwrap().len(), 1, "{env}");
        assert_eq!(env["repos"][0]["default"], true, "{env}");
        assert_eq!(env["repos"][0]["index"], 1, "{env}");
        assert_eq!(env["repo"], env["repos"][0]["repo"], "{env}");
        assert!(
            env["servers"].as_str().unwrap().contains("1 bound repo"),
            "{env}"
        );
        assert!(
            env["limitation"].as_str().unwrap().contains("optional `repo` argument"),
            "{env}"
        );
        assert!(
            env["scoping"]["repoSelector"].as_str().unwrap().contains("repo"),
            "{env}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// M3 slice 2: `preview_change` is listed, has no route, and answers a
    /// read-only plan without ever writing the file it read.
    #[test]
    fn preview_change_is_listed_and_dispatched_without_a_route() {
        let listed = tools();
        let names: BTreeSet<&str> = listed.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains("preview_change"));
        assert!(resolve_call("preview_change", &json!({})).is_err());
        assert!(!crate::edit::is_edit_tool("preview_change"));
        assert!(!crate::edit::is_aux_tool("preview_change"));
        // The selector aliases and the op/to contract are in the schema.
        let pv = listed.iter().find(|t| t["name"] == "preview_change").unwrap();
        let props = &pv["inputSchema"]["properties"];
        for arg in ["node", "id", "symbol", "from", "op", "to", "include_declarations"] {
            assert!(props.get(arg).is_some(), "preview_change schema missing arg {arg}");
        }

        let (dir, mut ix) = indexed("preview-change");
        let before = fs::read_to_string(dir.join("a.cs")).unwrap();

        // rename: B -> C. The planner edits the declaration and its call site.
        let r = invoke_on_index(
            &mut ix,
            "preview_change",
            &json!({"node": "N.C.B()", "op": "rename", "to": "C"}),
        );
        assert_eq!(r["isError"], false, "{r}");
        let env: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(env["ok"], true, "{env}");
        assert_eq!(env["readOnly"], true, "{env}");
        assert_eq!(env["applied"], false, "{env}");
        assert_eq!(env["writes"], json!([]), "{env}");
        assert!(env["currentSignature"].as_str().unwrap().contains('B'), "{env}");
        assert!(env["proposedSignature"].as_str().unwrap().contains('C'), "{env}");
        assert!(env["counts"]["edits"].as_i64().unwrap() >= 2, "{env}");
        assert!(env["counts"]["callSites"].as_i64().unwrap() >= 1, "{env}");
        assert!(env["callSites"][0]["line"].as_i64().unwrap() >= 1, "{env}");

        // retype: int -> long, replacing the declared type on the line.
        let r = invoke_on_index(
            &mut ix,
            "preview_change",
            &json!({"symbol": "N.C.B", "op": "retype", "to": "long"}),
        );
        let env: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(env["ok"], true, "{env}");
        assert_eq!(env["currentType"], "int", "{env}");
        assert!(env["proposedSignature"].as_str().unwrap().contains("long"), "{env}");
        assert!(env["counts"]["edits"].as_i64().unwrap() >= 1, "{env}");

        // Refusals are isError with an explicit reason.
        let r = invoke_on_index(
            &mut ix,
            "preview_change",
            &json!({"node": "N.C.NoSuchThing", "op": "rename", "to": "X"}),
        );
        assert_eq!(r["isError"], true);
        let env: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(env["ok"], false, "{env}");
        assert!(env["error"].as_str().unwrap().contains("unknown symbol"), "{env}");
        let r = invoke_on_index(
            &mut ix,
            "preview_change",
            &json!({"node": "N.C.B()", "op": "rename", "to": "not a name"}),
        );
        assert_eq!(r["isError"], true);
        let r = invoke_on_index(
            &mut ix,
            "preview_change",
            &json!({"node": "N.C.B()", "op": "teleport", "to": "X"}),
        );
        assert_eq!(r["isError"], true);

        // The read path wrote nothing.
        let after = fs::read_to_string(dir.join("a.cs")).unwrap();
        assert_eq!(before, after, "preview_change must never write");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn framing_handles_ping_notifications_and_bad_methods() {
        let ping = handle_message(&json!({"jsonrpc": "2.0", "id": "p", "method": "ping"}), 2);
        match ping {
            Handling::Reply(r) => {
                assert_eq!(r["id"], "p");
                assert_eq!(r["result"], json!({}));
            }
            _ => panic!("ping must reply"),
        }

        assert!(matches!(
            handle_message(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), 2),
            Handling::Ignore
        ));

        let bad = handle_message(&json!({"jsonrpc": "2.0", "id": 7, "method": "resources/list"}), 2);
        match bad {
            Handling::Reply(r) => assert_eq!(r["error"]["code"], -32601),
            _ => panic!("unknown method must error"),
        }

        let no_method = handle_message(&json!({"jsonrpc": "2.0", "id": 9}), 2);
        match no_method {
            Handling::Reply(r) => assert_eq!(r["error"]["code"], -32600),
            _ => panic!("invalid request must error"),
        }
    }

    #[test]
    fn tools_call_is_dispatched_to_a_worker() {
        let call = handle_message(
            &json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                    "params": {"name": "state", "arguments": {}}}),
            2,
        );
        match call {
            Handling::Dispatch(req) => assert_eq!(req["params"]["name"], "state"),
            _ => panic!("tools/call must dispatch"),
        }
    }

    #[test]
    fn unknown_tool_and_bad_arguments_are_tool_errors() {
        let (dir, mut ix) = indexed("unknown-tool");

        let r = invoke_on_index(&mut ix, "does_not_exist", &json!({}));
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("unknown tool"));

        let r = invoke_on_index(&mut ix, "node", &json!({}));
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("required"));

        let r = invoke_on_index(&mut ix, "api_call", &json!({"method": "GET"}));
        assert_eq!(r["isError"], true);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_call_matches_the_http_route() {
        let (dir, mut ix) = indexed("read");

        let direct = api::handle(&mut ix, "GET", "/api/state", &json!({}), None);
        let via_tool = invoke_on_index(&mut ix, "state", &json!({}));
        let text = via_tool["content"][0]["text"].as_str().unwrap();
        let envelope: Value = serde_json::from_str(text).unwrap();
        assert_eq!(envelope["status"], 200);
        assert_eq!(envelope["ok"], true, "routed success must carry ok:true");
        assert_eq!(envelope["body"], direct.1, "tool body must equal the route body");
        assert_eq!(envelope["body"]["indexed"], true);

        // the generic tool reaches the same place
        let generic = invoke_on_index(&mut ix, "api_call", &json!({"method": "GET", "path": "/api/state"}));
        assert_eq!(generic["isError"], false);

        // an unknown route is the app's own 404
        let nope = invoke_on_index(&mut ix, "api_call", &json!({"method": "GET", "path": "/api/does-not-exist"}));
        assert_eq!(nope["isError"], true);
        let env: Value = serde_json::from_str(nope["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(env["status"], 404);
        assert_eq!(env["ok"], false, "routed failure must carry ok:false");
        assert!(env["body"]["error"].as_str().unwrap().contains("unknown endpoint"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn routed_result_carries_status_ok_and_body() {
        let (dir, mut ix) = indexed("envelope-ok");
        for (name, args, want_status, want_ok) in [
            ("state", json!({}), 200u16, true),
            ("api_call", json!({"method": "GET", "path": "/api/does-not-exist"}), 404, false),
        ] {
            let r = invoke_on_index(&mut ix, name, &args);
            // `isError` semantics are unchanged: exactly `status >= 400`.
            assert_eq!(r["isError"], !want_ok, "{name}: isError must stay status >= 400");
            let text = r["content"][0]["text"].as_str().unwrap();
            let env: Value = serde_json::from_str(text).unwrap();
            let obj = env.as_object().unwrap();
            assert_eq!(obj.len(), 3, "routed envelope must be exactly status+ok+body: {env}");
            for key in ["status", "ok", "body"] {
                assert!(obj.contains_key(key), "routed envelope missing {key}: {env}");
            }
            assert_eq!(env["status"], want_status, "{env}");
            assert_eq!(env["ok"], want_ok, "{env}");
        }
        // The unified rule is stated in `initialize`'s instructions.
        let instructions = initialize_result(None, 1);
        let text = instructions["instructions"].as_str().unwrap();
        assert!(text.contains("`ok`"), "instructions must state the unified ok rule: {text}");
        assert!(text.contains("status"), "instructions must still name status: {text}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_call_actually_mutates() {
        let (dir, mut ix) = indexed("write");

        // viewer_state: POST mutates the state file, GET reads it back.
        let w = invoke_on_index(&mut ix, "viewer_state", &json!({"body": {"probe": 123}}));
        assert_eq!(w["isError"], false, "{w}");
        let env: Value = serde_json::from_str(w["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(env["body"]["ok"], true);

        let r = invoke_on_index(&mut ix, "viewer_state", &json!({}));
        let env: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(env["body"]["state"]["probe"], 123);

        // reorg is a real write route: `write` emits a tree under dest.
        let out = dir.join("out");
        let w = invoke_on_index(
            &mut ix,
            "reorg",
            &json!({"action": "write", "dest": out.to_string_lossy(), "spec": {"group": "single"}}),
        );
        assert_eq!(w["isError"], false, "{w}");
        assert!(out.is_dir(), "reorg write did not create the destination");

        // index rebuild is exposed too
        let idx = invoke_on_index(&mut ix, "index", &json!({"roots": ["."]}));
        assert_eq!(idx["isError"], false, "{idx}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn indexed_only_when_a_root_was_asked_for() {
        // `build_slot` with no roots stays cold; `run` relies on this for the
        // "no --root starts cold" contract.
        let dir = fixture("cold");
        let cancel = Arc::new(AtomicBool::new(false));
        let cold = build_slot(&dir, &[], &[], &Config::default(), &cancel);
        assert!(!cold.indexed);
        let hot = build_slot(&dir, &[".".to_string()], &[], &Config::default(), &cancel);
        assert!(hot.indexed);
        let _ = fs::remove_dir_all(&dir);
    }

    /// The Stop over MCP: `index_cancel` must not be a broadcast (it would wait
    /// behind the rebuild's write gate and slot pool), the one flag must reach
    /// every replica, and the engine's `cancelled` state must be detectable in
    /// the result so the broadcast loop can stop early.
    #[test]
    fn index_cancel_is_out_of_band_and_shared() {
        let cancel = resolve_call("index_cancel", &json!({})).expect("cancel tool resolves");
        assert_eq!(cancel.path, api::CANCEL_PATH);
        assert_eq!(cancel.method, "POST");
        assert!(
            !cancel.broadcasts(),
            "cancel must be answered out-of-band, not behind the rebuild's write gate"
        );

        let build = resolve_call("index", &json!({})).expect("index tool resolves");
        assert!(build.broadcasts(), "a successful rebuild must still reach every slot");

        // One flag, wired into every replica: a cancel reaches them all.
        let dir = fixture("cancel-shared");
        let flag = Arc::new(AtomicBool::new(false));
        let a = build_slot(&dir, &[], &[], &Config::default(), &flag);
        let b = build_slot(&dir, &[], &[], &Config::default(), &flag);
        assert!(!a.cancel_requested() && !b.cancel_requested());
        flag.store(true, Ordering::SeqCst);
        assert!(a.cancel_requested(), "replica A did not see the shared cancel");
        assert!(b.cancel_requested(), "replica B did not see the shared cancel");

        // The routed cancel response is exactly the shape the engine returns,
        // and `result_is_cancelled` recognises an unwound build.
        let stop = envelope_result(200, json!({"ok": true, "cancelled": true}));
        assert_eq!(stop["isError"], false);
        assert!(result_is_cancelled(&stop));
        assert!(!result_is_cancelled(&envelope_result(200, json!({"ok": true, "indexed": true}))));

        let _ = fs::remove_dir_all(&dir);
    }

    // -- multi-repo (R1) ---------------------------------------------------

    /// A `Shared` over fresh, never-built slots: primary + secondaries cold.
    fn test_shared(paths: &[PathBuf]) -> Shared {
        let cfg = Config::default();
        let mut repos = Vec::new();
        for p in paths {
            let cancel = Arc::new(AtomicBool::new(false));
            repos.push(make_repo_slot(p.clone(), 1, cancel, &cfg, false));
        }
        Shared { repos, cfg, out: Mutex::new(io::stdout()) }
    }

    /// A `Shared` configured with `--root .` where the primary is pre-indexed.
    fn test_shared_hot(paths: &[PathBuf]) -> Shared {
        let cfg = Config {
            roots: vec![".".to_string()],
            ..Config::default()
        };
        let mut repos = Vec::new();
        for (i, p) in paths.iter().enumerate() {
            let cancel = Arc::new(AtomicBool::new(false));
            repos.push(make_repo_slot(p.clone(), 1, cancel, &cfg, i == 0));
        }
        Shared { repos, cfg, out: Mutex::new(io::stdout()) }
    }

    fn env_of(result: &Value) -> Value {
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn every_tool_schema_carries_a_repo_property() {
        let listed = tools();
        assert_eq!(listed.len(), 42, "tool count must stay 42");
        for t in &listed {
            let name = t["name"].as_str().unwrap();
            let props = &t["inputSchema"]["properties"];
            assert_eq!(
                props["repo"]["type"], "string",
                "{name} schema must carry a `repo` property: {t}"
            );
            assert_eq!(
                t["inputSchema"]["additionalProperties"], true,
                "{name} additionalProperties must stay true"
            );
        }
    }

    #[test]
    fn repo_argument_resolves_path_index_and_basename() {
        let a = fixture("repo-a");
        let b = fixture("repo-b");
        let shared = test_shared(&[a.clone(), b.clone()]);

        // Absent => primary, and the key is stripped before anything sees it.
        let (i, stripped) = resolve_repo_argument(&shared, &json!({"q": "x"})).unwrap();
        assert_eq!(i, 0);
        assert_eq!(stripped, json!({"q": "x"}));
        let (i, stripped) =
            resolve_repo_argument(&shared, &json!({"repo": "2", "q": "x"})).unwrap();
        assert_eq!(i, 1);
        assert_eq!(stripped, json!({"q": "x"}));
        assert!(stripped.get("repo").is_none());
        // The stripped map is exactly what resolve_call sees.
        let call = resolve_call("search", &stripped).unwrap();
        assert_eq!(call.query, json!({"q": "x"}));
        let call = resolve_call("api_call", &strip_repo(&json!({"repo": 1, "path": "/api/state"})))
            .unwrap();
        assert_eq!(call.query, json!({}));

        assert_eq!(resolve_repo_selector(&shared.repos, &json!(1)).unwrap(), 0);
        assert_eq!(resolve_repo_selector(&shared.repos, &json!(2)).unwrap(), 1);
        assert_eq!(resolve_repo_selector(&shared.repos, &json!("2")).unwrap(), 1);
        assert_eq!(
            resolve_repo_selector(&shared.repos, &json!(a.to_string_lossy())).unwrap(),
            0
        );
        assert_eq!(
            resolve_repo_selector(&shared.repos, &json!(b.to_string_lossy())).unwrap(),
            1
        );
        let upper = a.file_name().unwrap().to_string_lossy().to_uppercase();
        assert_eq!(resolve_repo_selector(&shared.repos, &json!(upper)).unwrap(), 0);

        // Unknown index and unknown name both list every bound repo.
        for sel in [json!(3), json!(0), json!("nope")] {
            let err = resolve_repo_selector(&shared.repos, &sel).unwrap_err();
            assert!(err.contains("1:") && err.contains("2:"), "{err}");
            assert!(err.contains("(default)"), "{err}");
        }
        let err = resolve_repo_selector(&shared.repos, &json!({"x": 1})).unwrap_err();
        assert!(err.contains("bound repos"), "{err}");

        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn ambiguous_repo_basename_is_refused() {
        let root = std::env::temp_dir().join(format!("csgraph-mcp-amb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let a = root.join("x").join("src");
        let b = root.join("y").join("src");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let shared = test_shared(&[a.clone(), b.clone()]);
        let err = resolve_repo_selector(&shared.repos, &json!("src")).unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");
        assert!(err.contains("1:") && err.contains("2:"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn secondary_repo_builds_on_its_first_call() {
        let a = fixture("lazy-a");
        let b = fixture("lazy-b");
        fs::write(
            b.join("b.cs"),
            "namespace M {\n public partial class D {\n  public int C() { return 1; }\n }\n}\n",
        )
        .unwrap();
        let shared = test_shared_hot(&[a.clone(), b.clone()]);
        assert!(shared.repos[0].is_loaded(), "primary must be pre-indexed");
        assert!(!shared.repos[1].is_loaded(), "secondary must start cold");

        let state = env_of(&run_tool(&shared, "state", &json!({"repo": 2})));
        assert_eq!(state["body"]["indexed"], true, "{state}");
        assert_eq!(state["body"]["fileCount"], 2, "{state}");
        assert!(shared.repos[1].is_loaded(), "the first call must build repo 2");

        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn index_repo_scopes_the_broadcast_to_the_selected_repo() {
        let a = fixture("scope-a");
        let b = fixture("scope-b");
        fs::write(
            b.join("b.cs"),
            "namespace M {\n public partial class D {\n  public int C() { return 1; }\n }\n}\n",
        )
        .unwrap();
        let shared = test_shared_hot(&[a.clone(), b.clone()]);
        assert!(shared.repos[0].is_loaded());
        assert!(!shared.repos[1].is_loaded());
        let repo1_before = env_of(&run_tool(&shared, "state", &json!({"repo": 1})));

        // `index {repo:"2"}` builds repo 2 only; repo 1 is untouched.
        let r = run_tool(&shared, "index", &json!({"repo": "2"}));
        assert_eq!(r["isError"], false, "{r}");
        assert!(shared.repos[1].is_loaded());
        let repo2 = env_of(&run_tool(&shared, "state", &json!({"repo": "2"})));
        assert_eq!(repo2["body"]["fileCount"], 2, "{repo2}");
        let repo1_after = env_of(&run_tool(&shared, "state", &json!({"repo": 1})));
        assert_eq!(
            repo1_after["body"], repo1_before["body"],
            "index on repo 2 must not change repo 1"
        );
        assert_eq!(repo1_after["body"]["fileCount"], 1, "{repo1_after}");

        // `index_cancel` is out-of-band and flips only the selected repo's flag.
        assert!(!shared.repos[0].cancel.load(Ordering::SeqCst));
        let c = run_tool(&shared, "index_cancel", &json!({"repo": 2}));
        assert_eq!(c["isError"], false, "{c}");
        assert!(shared.repos[1].cancel.load(Ordering::SeqCst));
        assert!(!shared.repos[0].cancel.load(Ordering::SeqCst));

        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn workspace_lists_every_bound_repo() {
        let a = fixture("ws-a");
        let b = fixture("ws-b");
        let shared = test_shared_hot(&[a.clone(), b.clone()]);
        let env = env_of(&run_tool(&shared, "workspace", &json!({})));
        assert_eq!(env["ok"], true, "{env}");
        assert_eq!(env["repo"], a.to_string_lossy().to_string(), "{env}");
        assert_eq!(env["selected"], a.to_string_lossy().to_string(), "{env}");
        let repos = env["repos"].as_array().unwrap();
        assert_eq!(repos.len(), 2, "{env}");
        assert_eq!(repos[0]["index"], 1, "{env}");
        assert_eq!(repos[0]["default"], true, "{env}");
        assert_eq!(repos[0]["slots"], 1, "{env}");
        assert_eq!(repos[1]["index"], 2, "{env}");
        assert_eq!(repos[1]["default"], false, "{env}");
        assert_eq!(repos[1]["repo"], b.to_string_lossy().to_string(), "{env}");
        assert!(
            env["servers"].as_str().unwrap().contains("2 bound repos"),
            "{env}"
        );
        assert!(
            !env["limitation"].as_str().unwrap().contains("one repo per process"),
            "{env}"
        );
        assert!(
            env["scoping"]["repoSelector"].as_str().unwrap().contains("repo"),
            "{env}"
        );
        assert!(
            env["scoping"]["reScope"].as_str().unwrap().contains("index {roots"),
            "{env}"
        );

        // Selecting repo 2 keeps `repo` = primary but reports the selection.
        let env2 = env_of(&run_tool(&shared, "workspace", &json!({"repo": "2"})));
        assert_eq!(env2["selected"], b.to_string_lossy().to_string(), "{env2}");
        assert_eq!(env2["repo"], a.to_string_lossy().to_string(), "{env2}");

        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }

    #[test]
    fn secondary_without_root_uses_the_default_roots() {
        let root = std::env::temp_dir().join(format!("csgraph-mcp-noroot-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let make = |name: &str| {
            let repo = root.join(name);
            fs::create_dir_all(repo.join("_sealed_src")).unwrap();
            fs::write(
                repo.join("_sealed_src").join("a.cs"),
                "namespace N { public class C { } }\n",
            )
            .unwrap();
            repo
        };
        let a = make("a");
        let b = make("b");
        // No global `--root`: a secondary still builds, like an explicit
        // `index`, using `Config::default()`'s roots.
        let shared = test_shared(&[a.clone(), b.clone()]);
        assert!(!shared.repos[1].is_loaded());
        let s = env_of(&run_tool(&shared, "state", &json!({"repo": 2})));
        assert_eq!(s["body"]["indexed"], true, "{s}");
        assert_eq!(s["body"]["fileCount"], 1, "{s}");
        assert_eq!(s["body"]["roots"], json!(["_sealed_src"]), "{s}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_repo_paths_collapse_first_wins() {
        let a = fixture("dup-repo-a");
        let b = fixture("dup-repo-b");
        // A duplicate path (and a path with a trailing `/`, i.e. the same
        // canonical dir) collapses; order is preserved and the first wins.
        let deduped = dedupe_repo_paths(&[
            a.clone(),
            b.clone(),
            a.clone(),
            a.join("."),
        ]);
        assert_eq!(deduped.len(), 2, "{deduped:?}");
        assert_eq!(deduped[0], a);
        assert_eq!(deduped[1], b);
        let _ = fs::remove_dir_all(&a);
        let _ = fs::remove_dir_all(&b);
    }
}
