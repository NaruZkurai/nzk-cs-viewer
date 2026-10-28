//! The change-preview panel (`#previewModal`) — v2's `preview.js` in process.
//!
//! v2's panel (`frontend/static/preview.js`) asks one read-only question: *which
//! signatures and call sites would a rename or a retype of one declaration
//! change?* It talks to `GET`/`POST /api/preview`, whose POST half is
//! `csgraph_core::preview::invoke` (v2 `app/src/main.rs:336`) — the app-facing
//! twin of the MCP-only `preview_change` tool. That core function *is* the
//! implementation; it forces `apply:false` and always reports `writes: []`, so
//! it cannot write.
//!
//! This fork owns the engine [`Index`] itself, so the request is routed straight
//! into the same core function: no HTTP client, no async runtime, no second
//! planner. What is ported here is the *presentation* half
//! (`preview.js:64-137`) — as **plain text**, because the shell writes
//! `#previewOut.textContent`, not `innerHTML`. The text is derived only from
//! the engine's own JSON; when the engine refuses, the refusal is what is shown
//! (never a fabricated plan).
//!
//! ## What v2 does on open, and what this module does
//!
//! `open()` (`preview.js:176-187`) loads the op list (`GET /api/preview`),
//! defaults `#previewSelector` to `state.selected`, and **does not** plan
//! anything: only the Run button POSTs. The panel's own `#previewTo` input is
//! where a target is typed. This fork does not own that panel input yet, so the
//! one Host-owned request form the app already has is `#editArgs`
//! (`crate::edit_args`); when it holds a preview-shaped request
//! (`{"to": ...}`) the open runs the REAL engine plan, otherwise the open
//! publishes the same read-only listing the reference's GET twin returns. See
//! [`build`].

use csgraph_core::index::Index;
use serde_json::{json, Value};

/// The ops `csgraph_core::preview::invoke` accepts.
///
/// Transcribed from the engine's own acceptance test
/// (`core/src/preview.rs:319` — `op != "rename" && op != "retype"`); the v2
/// app-facing route publishes the same two names (`app/src/main.rs:312`).
pub const OPS: [&str; 2] = ["rename", "retype"];

/// The note the reference's GET twin publishes verbatim
/// (`app/src/main.rs:316`).
pub const READ_ONLY_NOTE: &str = "Read-only plan: no file is modified and there is no apply path.";

/// `preview.js` renders at most this many edit rows (`edits.slice(0, 60)`).
const EDITS_CAP: usize = 60;
/// `sites.slice(0, 60)`.
const SITES_CAP: usize = 60;
/// `decls.slice(0, 40)`.
const DECLS_CAP: usize = 40;
/// `amb.slice(0, 12)`.
const CANDIDATES_CAP: usize = 12;

/// One rendered panel state: what `#previewOut` says, what `#previewNote` says,
/// and which note class the reference would use (`setNote(text, kind)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewReport {
    /// `#previewOut`'s `textContent` — plain text, written exactly as built.
    pub text: String,
    /// `#previewNote`'s `textContent`.
    pub note: String,
    /// `"ok"` or `"bad"` — the second argument to `setNote`
    /// (`preview.js:39`, class `modal-note ok|bad`).
    pub note_kind: &'static str,
}

impl PreviewReport {
    /// The `ui.preview` payload (`{#previewOut, #previewNote}` content).
    pub fn facts(&self) -> Value {
        json!({
            "text": self.text,
            "note": self.note,
            "note_kind": self.note_kind,
        })
    }
}

/// A string field of an engine JSON object, or `""`.
fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// An integer field rendered as text (`null` -> `0`).
fn int_of(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Render one engine answer (`preview::invoke`'s body) as plain text.
///
/// Pure: no index, no I/O. The refusal branch is `preview.js:68-87`, the plan
/// branch `preview.js:88-136`, field for field and in the same order.
pub fn render(data: &Value) -> PreviewReport {
    if data.get("ok").and_then(Value::as_bool) != Some(true) {
        let why = {
            let e = str_of(data, "error");
            if e.is_empty() { "preview refused" } else { e }
        };
        let mut text = format!("refused: {why}");
        if let Some(amb) = data.get("ambiguity").and_then(Value::as_array) {
            for c in amb.iter().take(CANDIDATES_CAP) {
                let kind = str_of(c, "kind");
                let mut name = str_of(c, "full").to_string();
                if name.is_empty() {
                    name = str_of(c, "id").to_string();
                }
                text.push_str(&format!(
                    "\ncandidate {kind} {name} ({}:{})",
                    str_of(c, "file"),
                    int_of(c, "line")
                ));
            }
        }
        return PreviewReport { text, note: why.to_string(), note_kind: "bad" };
    }

    let op = str_of(data, "op");
    let to = str_of(data, "to");
    let decl = data.get("declaration").cloned().unwrap_or_else(|| json!({}));
    let counts = data.get("counts").cloned().unwrap_or_else(|| json!({}));
    let c_edits = int_of(&counts, "edits");
    let c_files = int_of(&counts, "files");
    let c_sites = int_of(&counts, "callSites");
    let c_decls = int_of(&counts, "declarations");
    let c_changed = int_of(&counts, "changedLines");

    let mut out = String::new();
    out.push_str("Read-only plan — nothing is written");
    if data.get("truncated").and_then(Value::as_bool) == Some(true) {
        out.push_str(" (truncated)");
    }
    out.push('\n');
    out.push_str(&format!("op: {op} → {to}\n"));
    let mut decl_name = str_of(&decl, "full").to_string();
    if decl_name.is_empty() {
        decl_name = str_of(&decl, "id").to_string();
    }
    out.push_str(&format!(
        "declaration: {decl_name} ({}:{})\n",
        str_of(&decl, "file"),
        int_of(&decl, "line")
    ));
    let current_type = str_of(data, "currentType");
    if !current_type.is_empty() {
        out.push_str(&format!("declared type: {current_type}\n"));
    }
    out.push_str("Signature\n");
    out.push_str(&format!(
        "  {}\n  {}\n",
        str_of(data, "currentSignature"),
        str_of(data, "proposedSignature")
    ));
    out.push_str("Counts\n");
    out.push_str(&format!("  edits: {c_edits} line(s) in {c_files} file(s)\n"));
    out.push_str(&format!(
        "  call sites: {c_sites}\n  affected declarations: {c_decls}\n  changed lines: {c_changed}\n"
    ));

    if let Some(edits) = data.get("edits").and_then(Value::as_array) {
        if !edits.is_empty() {
            out.push_str(&format!("Edits ({})\n", edits.len()));
            for e in edits.iter().take(EDITS_CAP) {
                out.push_str(&format!("  {}:{}\n", str_of(e, "file"), int_of(e, "line")));
                out.push_str(&format!("  - {}\n", str_of(e, "before")));
                out.push_str(&format!("  + {}\n", str_of(e, "after")));
            }
            if edits.len() > EDITS_CAP {
                out.push_str(&format!("  …{} more\n", edits.len() - EDITS_CAP));
            }
        }
    }

    let sites = data
        .get("callSites")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    out.push_str(&format!("Call sites ({})\n", sites.len()));
    if sites.is_empty() {
        out.push_str("  No incoming call site is attributed to this declaration.\n");
    } else {
        for s in sites.iter().take(SITES_CAP) {
            out.push_str(&format!(
                "  {}:{} {} [{}]{}\n",
                str_of(s, "file"),
                int_of(s, "line"),
                str_of(s, "node"),
                str_of(s, "edge"),
                if s.get("changed").and_then(Value::as_bool) == Some(true) { " edited" } else { "" }
            ));
        }
        if sites.len() > SITES_CAP {
            out.push_str(&format!("  …{} more\n", sites.len() - SITES_CAP));
        }
    }

    if let Some(decls) = data.get("affectedDeclarations").and_then(Value::as_array) {
        if !decls.is_empty() {
            out.push_str(&format!("Affected declarations ({})\n", decls.len()));
            for d in decls.iter().take(DECLS_CAP) {
                let mut name = str_of(d, "full").to_string();
                if name.is_empty() {
                    name = str_of(d, "id").to_string();
                }
                out.push_str(&format!(
                    "  {}:{} {name} [{}]\n",
                    str_of(d, "file"),
                    int_of(d, "line"),
                    str_of(d, "edge")
                ));
            }
        }
    }

    let note = format!(
        "Planned {c_edits} edit(s), {c_sites} call site(s) — wrote nothing"
    );
    PreviewReport { text: out, note, note_kind: "ok" }
}

/// The read-only listing the reference's `GET /api/preview` returns
/// (`app/src/main.rs:306-318`), rendered for the panel that is now open.
///
/// `#previewOut` is otherwise empty on open in the reference (`open()` never
/// touches it), but a native panel with a blank body is a dead control — this
/// publishes the real contract (the op list + read-only note) and, when the
/// graph has a selection, the selected declaration the reference would prefill
/// into `#previewSelector`.
pub fn listing(index: &Index, selected: Option<&str>) -> PreviewReport {
    let _ = index;
    let mut text = String::new();
    text.push_str("Read-only plan — nothing is written\n");
    text.push_str(&format!("ops: {}\n", OPS.join(", ")));
    match selected {
        Some(id) if !id.is_empty() => {
            text.push_str(&format!("declaration: {id}\n"));
        }
        _ => text.push_str("declaration: none selected\n"),
    }
    text.push_str("Enter a new name/type (`to`) to plan a change.");
    PreviewReport { text, note: READ_ONLY_NOTE.to_string(), note_kind: "ok" }
}

/// Extract a preview request from the Host-owned `#editArgs` box, if it holds
/// one.
///
/// The `#previewTo` input is not app-owned yet, so the pending request is read
/// from the box the app does own. A request needs a non-empty `to`; the selector
/// is the first of `node`/`id`/`symbol`/`from` in the box, then the graph
/// selection. `op` defaults to `rename` (the first op in the reference's list),
/// and an unknown op is left to the engine — it refuses loudly, which is the
/// honest answer.
pub fn request_from_edit_args(text: &str, selected: Option<&str>) -> Option<Value> {
    if text.trim().is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(text).ok()?;
    if !value.is_object() {
        return None;
    }
    let to = str_of(&value, "to").trim();
    if to.is_empty() {
        return None;
    }
    let mut node = "";
    for key in ["node", "id", "symbol", "from"] {
        let candidate = str_of(&value, key).trim();
        if !candidate.is_empty() {
            node = candidate;
            break;
        }
    }
    if node.is_empty() {
        node = selected.map(str::trim).unwrap_or("");
    }
    if node.is_empty() {
        return None;
    }
    let op = {
        let candidate = str_of(&value, "op").trim();
        if candidate.is_empty() { "rename" } else { candidate }
    };
    Some(json!({ "node": node, "op": op, "to": to }))
}

/// The note `preview.js`'s `run()` writes when no target is typed
/// (`preview.js:191`: `setNote('Enter the new name/type (`to`)', 'bad')`).
/// Transcribed, not invented: an empty `#previewTo` never reaches the engine.
pub const MISSING_TARGET_NOTE: &str = "Enter the new name/type (`to`)";

/// The panel state for a run with an empty `#previewTo` — the reference's own
/// note, before any request is made.
pub fn missing_target() -> PreviewReport {
    PreviewReport {
        text: MISSING_TARGET_NOTE.to_string(),
        note: MISSING_TARGET_NOTE.to_string(),
        note_kind: "bad",
    }
}

/// A preview request built from the HOST-OWNED form values (`#previewTo`,
/// `#previewOp`, `#previewSelector`) — the `run()` path (`preview.js:188-199`).
///
/// This is the task-14 facts round-trip for the preview dialog: a target the
/// user typed into `#previewTo` is what the engine is asked to plan against. It
/// reuses the same request shape as [`request_from_edit_args`]: a non-empty `to`
/// is required, an empty `op` defaults to `rename` (the first op in the
/// reference's list), and `target` is the already-resolved declaration from
/// [`resolve_target`] (typed `#previewSelector`, else the graph selection).
///
/// `target: None` still produces a request (the reference posts `{op,to}` and
/// lets the engine refuse), so the answer shown is the engine's, not one this
/// module made up.
pub fn request_from_form(to: &str, op: &str, target: Option<&str>) -> Option<Value> {
    let to = to.trim();
    if to.is_empty() {
        return None;
    }
    let op = {
        let candidate = op.trim();
        if candidate.is_empty() { "rename" } else { candidate }
    };
    let mut args = json!({ "op": op, "to": to });
    if let Some(node) = target.map(str::trim).filter(|s| !s.is_empty()) {
        args["node"] = json!(node);
    }
    Some(args)
}

/// The `#previewRun` button / Enter-in-`#previewTo` path (`preview.js:188-199`):
/// build the request from the Host-owned form values and hand it to the REAL
/// engine (`preview::invoke`, through [`plan`]).
///
/// Returns `None` when `#previewTo` is empty — the caller then shows
/// [`missing_target`] instead of calling the engine, exactly like the reference
/// (`if (!a.to) { setNote(..., 'bad'); return; }`). A `Some(report)` is always
/// the engine's own answer (a plan or its refusal), never synthesized here.
pub fn run(
    index: &mut Index,
    selected: Option<&str>,
    selector: &str,
    to: &str,
    op: &str,
) -> Option<PreviewReport> {
    let target = resolve_target(selected, selector);
    let args = request_from_form(to, op, target)?;
    Some(plan(index, &args))
}

/// Run one read-only preview through the engine and render it.
///
/// This is the POST half of the v2 route, in process: the args go straight into
/// [`csgraph_core::preview::invoke`], the same function the reference's backend
/// called, and the answer is rendered by [`render`] — never invented.
pub fn plan(index: &mut Index, args: &Value) -> PreviewReport {
    render(&csgraph_core::preview::invoke(index, args))
}

/// Which declaration a plan is asked about: the Host-owned `#previewSelector`
/// text when the user typed one, else the current graph selection.
///
/// `preview.js:183-187` prefills `els.sel.value` from `state.selected` on open
/// and `run()` sends `els.sel.value` — so an **empty** selector is the
/// "nothing typed" case and falls back to the selection. Trimmed, because a
/// stray space in a text box is not a declaration id.
pub fn resolve_target<'a>(selected: Option<&'a str>, typed: &'a str) -> Option<&'a str> {
    let typed = typed.trim();
    if typed.is_empty() {
        selected
    } else {
        Some(typed)
    }
}

/// Build the panel content for an open of `#previewModal`.
///
/// * A preview-shaped `#editArgs` request -> the REAL engine plan
///   ([`plan`]).
/// * Otherwise -> the read-only op listing ([`listing`]), exactly the contract
///   the reference's GET twin publishes before anything is typed.
///
/// Either way the engine is consulted for anything plan-shaped, and the panel
/// never shows a plan the engine did not produce. `selector` is the Host-owned
/// `#previewSelector` text (see [`resolve_target`]): a typed declaration drives
/// the request, and an empty one falls back to the graph selection.
pub fn build(
    index: &mut Index,
    selected: Option<&str>,
    selector: &str,
    edit_args: &str,
) -> PreviewReport {
    let target = resolve_target(selected, selector);
    if let Some(args) = request_from_edit_args(edit_args, target) {
        return plan(index, &args);
    }
    listing(index, target)
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    /// The selector fallback rule (`preview.js:183-187`): a typed declaration
    /// drives the plan; an empty box is the current selection.
    #[test]
    fn a_typed_selector_overrides_the_selection_and_empty_falls_back() {
        assert_eq!(resolve_target(Some("n:1"), "n:9"), Some("n:9"));
        assert_eq!(resolve_target(Some("n:1"), "  n:9  "), Some("n:9"), "trimmed");
        assert_eq!(resolve_target(Some("n:1"), ""), Some("n:1"), "empty => selection");
        assert_eq!(resolve_target(Some("n:1"), "   "), Some("n:1"));
        assert_eq!(resolve_target(None, "n:9"), Some("n:9"));
        assert_eq!(resolve_target(None, ""), None);
    }

    /// The typed selector is not merely displayed: it is the `node` the engine
    /// request carries, through the same extraction the real plan uses.
    #[test]
    fn the_typed_selector_reaches_the_engine_request() {
        let typed = resolve_target(Some("n:1"), "n:9");
        let args = request_from_edit_args(r#"{"to": "Plus"}"#, typed).expect("request");
        assert_eq!(args["node"], "n:9");
        assert_eq!(args["to"], "Plus");
        let fallback = resolve_target(Some("n:1"), "");
        let args = request_from_edit_args(r#"{"to": "Plus"}"#, fallback).expect("request");
        assert_eq!(args["node"], "n:1", "an empty selector falls back to the selection");
        // A typed selector with no `to` is still not a request: nothing is
        // invented just because a declaration is named.
        assert!(request_from_edit_args(r#"{}"#, resolve_target(Some("n:1"), "n:9")).is_none());
    }

    /// One full plan in the engine's own shape (`core/src/preview.rs:485-540`).
    fn plan_body() -> Value {
        json!({
            "ok": true,
            "tool": "preview_change",
            "readOnly": true,
            "dryRun": true,
            "applied": false,
            "writes": [],
            "op": "rename",
            "to": "Plus",
            "declaration": {
                "id": "n:1",
                "full": "App.Math.Add",
                "label": "Add",
                "name": "Add",
                "kind": "method",
                "file": "src/App.cs",
                "line": 12,
                "endLine": 20
            },
            "currentSignature": "public static int Add(int a, int b)",
            "proposedSignature": "public static int Plus(int a, int b)",
            "counts": {"edits": 2, "files": 2, "callSites": 1, "declarations": 1, "changedLines": 2},
            "truncated": false,
            "edits": [
                {"file": "src/App.cs", "line": 12, "before": "public static int Add(int a, int b)", "after": "public static int Plus(int a, int b)", "kind": "declaration-or-usage"},
                {"file": "src/Use.cs", "line": 7, "before": "var x = Add(1, 2);", "after": "var x = Plus(1, 2);", "kind": "declaration-or-usage"}
            ],
            "callSites": [
                {"file": "src/Use.cs", "line": 7, "node": "n:2", "edge": "calls", "changed": true}
            ],
            "affectedDeclarations": [
                {"id": "n:2", "full": "App.Run", "kind": "method", "file": "src/Use.cs", "line": 7, "edge": "calls", "reason": "references the renamed symbol"}
            ]
        })
    }

    #[test]
    fn renders_a_plan_in_the_reference_order() {
        let report = render(&plan_body());
        let expected = concat!(
            "Read-only plan — nothing is written\n",
            "op: rename → Plus\n",
            "declaration: App.Math.Add (src/App.cs:12)\n",
            "Signature\n",
            "  public static int Add(int a, int b)\n",
            "  public static int Plus(int a, int b)\n",
            "Counts\n",
            "  edits: 2 line(s) in 2 file(s)\n",
            "  call sites: 1\n",
            "  affected declarations: 1\n",
            "  changed lines: 2\n",
            "Edits (2)\n",
            "  src/App.cs:12\n",
            "  - public static int Add(int a, int b)\n",
            "  + public static int Plus(int a, int b)\n",
            "  src/Use.cs:7\n",
            "  - var x = Add(1, 2);\n",
            "  + var x = Plus(1, 2);\n",
            "Call sites (1)\n",
            "  src/Use.cs:7 n:2 [calls] edited\n",
            "Affected declarations (1)\n",
            "  src/Use.cs:7 App.Run [calls]\n",
        );
        assert_eq!(report.text, expected);
        assert_eq!(report.note, "Planned 2 edit(s), 1 call site(s) — wrote nothing");
        assert_eq!(report.note_kind, "ok");
    }

    #[test]
    fn renders_retype_with_the_declared_type_line() {
        let mut body = plan_body();
        body["op"] = json!("retype");
        body["to"] = json!("long");
        body["currentType"] = json!("int");
        let report = render(&body);
        assert!(report.text.contains("op: retype → long\n"), "{}", report.text);
        assert!(report.text.contains("declared type: int\n"), "{}", report.text);
    }

    #[test]
    fn renders_a_refusal_and_its_candidates_never_a_plan() {
        let body = json!({
            "ok": false,
            "tool": "preview_change",
            "error": "\"Dup\" does not resolve to one declaration",
            "ambiguity": [
                {"id": "n:1", "kind": "method", "full": "A.Dup", "file": "src/A.cs", "line": 3},
                {"id": "n:2", "kind": "field", "full": "", "file": "src/B.cs", "line": 9}
            ]
        });
        let report = render(&body);
        assert_eq!(report.text, concat!(
            "refused: \"Dup\" does not resolve to one declaration\n",
            "candidate method A.Dup (src/A.cs:3)\n",
            "candidate field n:2 (src/B.cs:9)",
        ));
        assert_eq!(report.note, "\"Dup\" does not resolve to one declaration");
        assert_eq!(report.note_kind, "bad");
    }

    #[test]
    fn a_refusal_without_an_error_message_is_still_a_refusal() {
        let report = render(&json!({"ok": false}));
        assert_eq!(report.text, "refused: preview refused");
        assert_eq!(report.note_kind, "bad");
    }

    #[test]
    fn empty_call_sites_say_so_instead_of_pretending() {
        let mut body = plan_body();
        body["callSites"] = json!([]);
        body["affectedDeclarations"] = json!([]);
        body["counts"]["callSites"] = json!(0);
        let report = render(&body);
        assert!(report.text.ends_with(
            "Call sites (0)\n  No incoming call site is attributed to this declaration.\n"
        ));
        assert!(report.note.contains("0 call site(s)"));
        // `affectedDeclarations` is optional and absent here -> no stale section.
        assert!(!report.text.contains("Affected declarations"));
    }

    #[test]
    fn the_caps_and_the_truncation_marker_come_from_the_engine_body() {
        let edits: Vec<Value> = (0..=EDITS_CAP)
            .map(|i| json!({"file": "src/F.cs", "line": i, "before": "a", "after": "b"}))
            .collect();
        let sites: Vec<Value> = (0..=SITES_CAP)
            .map(|i| json!({"file": "src/S.cs", "line": i, "node": "n", "edge": "calls"}))
            .collect();
        let mut body = plan_body();
        body["edits"] = json!(edits);
        body["callSites"] = json!(sites);
        body["truncated"] = json!(true);
        let report = render(&body);
        assert!(report.text.contains("Read-only plan — nothing is written (truncated)\n"));
        assert!(report.text.contains("  …1 more\n"), "edits cap must truncate");
        assert_eq!(report.text.matches("  …1 more\n").count(), 2, "sites cap must truncate too");
    }

    #[test]
    fn the_listing_is_the_reference_get_twin() {
        let index = Index::new(std::env::temp_dir());
        let report = listing(&index, Some("n:1"));
        assert_eq!(report.text, concat!(
            "Read-only plan — nothing is written\n",
            "ops: rename, retype\n",
            "declaration: n:1\n",
            "Enter a new name/type (`to`) to plan a change.",
        ));
        assert_eq!(report.note, READ_ONLY_NOTE);
        assert_eq!(report.note_kind, "ok");
        let none = listing(&index, None);
        assert!(none.text.contains("declaration: none selected\n"));
    }

    #[test]
    fn the_ops_are_the_core_functions_two_accepted_ops() {
        // The value `csgraph_core::preview::invoke` checks against
        // (`core/src/preview.rs:319`).
        assert_eq!(OPS, ["rename", "retype"]);
    }

    #[test]
    fn request_from_form_uses_the_host_owned_preview_inputs() {
        // No target typed -> no request (the caller shows the reference note).
        assert_eq!(request_from_form("", "retype", Some("n:1")), None);
        assert_eq!(request_from_form("   ", "retype", Some("n:1")), None);
        // A target but no declaration to plan against still produces the
        // request the reference posts; the ENGINE refuses it, not this module.
        assert_eq!(
            request_from_form("Plus", "retype", None),
            Some(json!({"op": "retype", "to": "Plus"}))
        );
        // The typed target and the selected op are exactly what is asked.
        assert_eq!(
            request_from_form(" Plus ", "retype", Some("n:1")),
            Some(json!({"node": "n:1", "op": "retype", "to": "Plus"}))
        );
        // An op the user has not chosen yet defaults to the reference's first.
        assert_eq!(
            request_from_form("Plus", "", Some("n:1")),
            Some(json!({"node": "n:1", "op": "rename", "to": "Plus"}))
        );
    }

    /// The `#previewRun` path reaches the REAL engine: on an unindexed index the
    /// engine's own refusal is the text, so a run can never show a fake plan.
    #[test]
    fn run_hands_the_typed_form_values_to_the_engine() {
        let mut index = Index::new(std::env::temp_dir());
        // Empty #previewTo -> no engine call; the caller shows the v2 note.
        assert!(run(&mut index, Some("n:1"), "n:1", "   ", "rename").is_none());
        assert_eq!(missing_target().note_kind, "bad");
        assert_eq!(missing_target().note, MISSING_TARGET_NOTE);
        // A typed target + typed selector -> the engine's answer, verbatim.
        let report = run(&mut index, Some("n:1"), "n:9", "Plus", "rename")
            .expect("a request was built");
        assert_eq!(report.note_kind, "bad");
        assert_eq!(report.note, "index a folder first");
        assert_eq!(report.text, "refused: index a folder first");
        // The typed selector overrides the selection in the request: the engine
        // request shape is asserted directly (an unindexed corpus refuses both).
        let args = request_from_form("Plus", "rename", resolve_target(Some("n:1"), "n:9")).unwrap();
        assert_eq!(args["node"], "n:9");
        let fallback =
            request_from_form("Plus", "rename", resolve_target(Some("n:1"), "")).unwrap();
        assert_eq!(fallback["node"], "n:1");
    }

    #[test]
    fn request_from_edit_args_needs_a_target_and_a_selector() {
        assert_eq!(request_from_edit_args("", Some("n:1")), None);
        assert_eq!(request_from_edit_args("   ", Some("n:1")), None);
        assert_eq!(request_from_edit_args("{", Some("n:1")), None);
        assert_eq!(request_from_edit_args("[]", Some("n:1")), None);
        // No `to` and no target -> nothing to plan.
        assert_eq!(request_from_edit_args("{}", None), None);
        // No `to` even with a selection -> still nothing to plan.
        assert_eq!(request_from_edit_args("{}", Some("n:1")), None);
        // A selector but no target cannot be planned either.
        assert_eq!(request_from_edit_args("{\"node\":\"n:1\"}", Some("n:1")), None);
    }

    #[test]
    fn request_from_edit_args_prefers_the_box_selector_and_defaults_the_op() {
        let req = request_from_edit_args("{\"node\":\"n:9\",\"to\":\"Plus\"}", Some("n:1"))
            .expect("a request");
        assert_eq!(req, json!({"node": "n:9", "op": "rename", "to": "Plus"}));
        // `from` is accepted too — it is the same selector vocabulary the core
        // resolve uses (`core/src/preview.rs:315`).
        let req = request_from_edit_args("{\"from\":\"App.Add\",\"to\":\"Plus\"}", None)
            .expect("a request");
        assert_eq!(req["node"], json!("App.Add"));
    }

    #[test]
    fn request_from_edit_args_falls_back_to_the_graph_selection() {
        let req = request_from_edit_args("{\"to\":\"long\",\"op\":\"retype\"}", Some("n:1"))
            .expect("a request");
        assert_eq!(req, json!({"node": "n:1", "op": "retype", "to": "long"}));
    }

    #[test]
    fn build_is_the_listing_without_a_request_and_a_real_engine_answer_with_one() {
        let mut index = Index::new(std::env::temp_dir());
        // A fresh index is not indexed yet; the listing needs no engine. An
        // empty selector is the "nothing typed" case: the graph selection is
        // what the listing names.
        let listing_only = build(&mut index, Some("n:1"), "", "");
        assert_eq!(listing_only.note_kind, "ok");
        assert!(listing_only.text.contains("ops: rename, retype"));
        assert!(listing_only.text.contains("declaration: n:1"));
        // A typed selector wins over the selection, again without an engine call.
        let typed = build(&mut index, Some("n:1"), "n:9", "");
        assert!(typed.text.contains("declaration: n:9"));
        assert!(!typed.text.contains("declaration: n:1"));
        // A request reaches the REAL engine: on an unindexed index the engine's
        // own refusal is what comes back (not a plan we made up).
        let refused = build(&mut index, Some("n:1"), "n:1", "{\"to\":\"Plus\"}");
        assert_eq!(refused.note_kind, "bad");
        assert_eq!(refused.note, "index a folder first");
        assert_eq!(refused.text, "refused: index a folder first");
    }

    #[test]
    fn facts_carry_every_field_the_shell_writes() {
        let report = render(&plan_body());
        let facts = report.facts();
        assert_eq!(facts["text"], json!(report.text));
        assert_eq!(facts["note"], json!(report.note));
        assert_eq!(facts["note_kind"], json!("ok"));
    }
}
