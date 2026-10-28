//! The Reorganise & export dialog's wire: request model, in-process engine
//! call, and the report formatting the shell paints.
//!
//! The reference is `frontend/static/reorg.js` (v2). Its `spec()` builds the
//! `POST /api/reorg` body, `render()` turns the engine's answer into the
//! `.reorg-row` rows of `#reorgReport`, and `setBusy`/`setNote` own
//! `#reorgNote`. This fork has no HTTP and no async runtime, so the route
//! becomes the engine call it always was:
//! [`csgraph_core::index::Index::reorg`] (`core/src/index.rs:3181`, the
//! server.py `Index.reorg` port; `core/src/api.rs:539` is the HTTP wrapper).
//! Its signature is copied from the engine, never invented:
//!
//! ```text
//! pub fn reorg(&mut self,
//!     spec: Option<&Value>, action: &str,
//!     dest: Option<&str>, overwrite: bool) -> Value
//! ```
//!
//! The same four actions the reference exposes are reachable: `plan`,
//! `verify`, `zip`, `write`; `compile` additionally runs the differential
//! Roslyn check. `#reorgPlan` is not in the shipped `index.html` markup (the
//! reference *creates* it at runtime, `reorg.js:326-331`), and this skeleton is
//! transcribed from that markup, so the four shipped buttons are the four
//! actions wired here; `plan` remains an engine action this module can invoke
//! but no control currently issues — stated as a residual, not faked.
//!
//! ## Honesty rules
//!
//! * The note and report are built **only** from the engine's returned
//!   [`Value`]; no caller can claim a run happened.
//! * `write` with an empty destination never reaches the engine (the reference
//!   refuses too, `reorg.js:205`: `if (!dest) { toast(...); return; }`), so an
//!   accidental write into the repo root is impossible.
//! * `zip` cannot download in a headless binary (there is no browser `Blob`
//!   and no HTTP). The base64 payload the engine returns is decoded and either
//!   written to the explicit destination folder when one was named, or kept in
//!   [`ReorgView::archive`] and the note says exactly that. A file is never
//!   claimed to have been "downloaded" when nothing left this process.

use serde_json::{json, Value};

use crate::blocks_peek::{ReorgGroup, ReorgMode};
use csgraph_core::index::Index;

// ------------------------------------------------------------------ question

/// Where the output comes from: the whole corpus, or the graph selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `#reorgScope` value `"selection"` (`reorg.js:26`).
    Selection,
    /// `#reorgScope` value `"all"`.
    All,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Selection => "selection",
            Scope::All => "all",
        }
    }
}

/// `#reorgGran` — the chunk unit (`index.html:461-465`, `reorg.js:17`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gran {
    Leaf,
    Top,
}

impl Gran {
    pub fn as_str(self) -> &'static str {
        match self {
            Gran::Leaf => "leaf",
            Gran::Top => "top",
        }
    }
}

/// One engine action (`reorg.js:126-200`: `plan`/`verify`/`zip`/`write` plus
/// `compile`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Plan,
    Verify,
    Zip,
    Write,
    Compile,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Plan => "plan",
            Action::Verify => "verify",
            Action::Zip => "zip",
            Action::Write => "write",
            Action::Compile => "compile",
        }
    }

    /// The control id that issues this action, if the shipped markup has one.
    /// `Plan` has none: it is created at runtime by `reorg.js:326-331` and the
    /// skeleton is transcribed from the shipped document, so it is not faked
    /// in here.
    pub fn from_control(selector: &str) -> Option<Action> {
        match selector {
            "#reorgVerify" => Some(Action::Verify),
            "#reorgZip" => Some(Action::Zip),
            "#reorgWrite" => Some(Action::Write),
            "#reorgCompile" => Some(Action::Compile),
            _ => None,
        }
    }
}

/// The dialog's form state — the Rust side of `reorg.js`'s `els.mode.value`,
/// `els.scope.value`, `els.gran.value`, `els.group.value`, `els.size.value`,
/// `els.header.checked` and `els.dest.value`.
///
/// Defaults are the **shipped markup's** defaults (`index.html` /
/// `nzk_ui/src/skeleton/modals_b.rs`): the first option of each select
/// (`single`, `selection`, `leaf`, `size`), `value="3000"` on `#reorgSize`, and
/// `#reorgHeader checked`. That is what the reference's `spec()` would read
/// with an untouched form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReorgRequest {
    pub mode: ReorgMode,
    pub scope: Scope,
    pub gran: Gran,
    pub group: ReorgGroup,
    /// `#reorgSize`, already clamped to the input's `min="50"` the way
    /// `reorg.js:20` does (`Math.max(50, Number(...) || 3000)`).
    pub size: u32,
    /// `#reorgDest`'s typed text. Empty means "no folder named".
    pub dest: String,
    /// `#reorgHeader.checked` — the banner comment in every emitted file.
    pub header: bool,
}

impl Default for ReorgRequest {
    fn default() -> Self {
        Self {
            mode: ReorgMode::Single,
            scope: Scope::Selection,
            gran: Gran::Leaf,
            group: ReorgGroup::Size,
            size: 3000,
            dest: String::new(),
            header: true,
        }
    }
}

impl ReorgRequest {
    /// `reorg.js:15-28` `spec()` as a JSON object.
    ///
    /// Two deliberate deviations, both honest:
    /// * `group` collapses to `"single"` in one-file mode (`reorg.js:18`) —
    ///   the same rule, not a second one.
    /// * `units` is set from `selection` only when the scope is `selection`
    ///   **and** at least one node is selected (`reorg.js:22-25`). No ids is
    ///   the reference's "the selection is empty" state, in which the
    ///   `units` key is absent; the engine then plans the whole corpus.
    pub fn spec(&self, selection: &[String]) -> Value {
        let mut spec = json!({
            "header": self.header,
            "granularity": self.gran.as_str(),
            "group": if self.mode == ReorgMode::Single {
                ReorgGroup::Single.as_str()
            } else {
                self.group.as_str()
            },
        });
        if spec["group"] == "size" {
            spec["size"] = json!(self.size.max(50));
        }
        if self.scope == Scope::Selection && !selection.is_empty() {
            spec["units"] = json!(selection);
        }
        spec
    }

    /// The destination this request would hand the engine for a `write`, or
    /// `None` when the box is empty (never `Some("")`: that would make the
    /// engine pick its own default).
    pub fn write_dest(&self) -> Option<&str> {
        let d = self.dest.trim();
        if d.is_empty() { None } else { Some(d) }
    }
}

// ------------------------------------------------------------------- format

/// One `div.reorg-row` of `#reorgReport` (`reorg.js:46-52,57-120`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRow {
    /// `.reorg-k`
    pub k: String,
    /// `.reorg-v`
    pub v: String,
    /// The reference's optional class: `good`, `bad` or `warn`.
    pub class: String,
}

impl ReportRow {
    fn new(k: impl Into<String>, v: impl Into<String>, class: &str) -> Self {
        Self {
            k: k.into(),
            v: v.into(),
            class: class.to_string(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"k": self.k, "v": self.v, "class": self.class})
    }
}

/// `fmt_str`: the reference's `esc(String(x))` without the HTML escaping — this
/// renderer writes text nodes, so `&`/`<`/`>` are literal and must stay so.
fn s(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn n(v: Option<&Value>) -> i64 {
    v.and_then(Value::as_i64).unwrap_or(0)
}

fn arr_len(v: Option<&Value>) -> usize {
    v.and_then(Value::as_array).map(|a| a.len()).unwrap_or(0)
}

/// The joined first few entries of a string array, or the empty string.
fn join_first(v: Option<&Value>, max: usize) -> String {
    let Some(items) = v.and_then(Value::as_array) else {
        return String::new();
    };
    items
        .iter()
        .take(max)
        .map(|x| s(Some(x)))
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render the engine answer into report rows (`reorg.js:57-120` `render`).
///
/// The plan block, the verification report, the write result and the compile
/// diagnostics are independent: a `verify` answer carries plan+report, a
/// `write` adds `write`, a `compile` adds the differential keys. Each block is
/// emitted only when the engine actually returned it, so a partial answer
/// cannot display a filled-in row it never earned.
pub fn report_rows(data: &Value) -> Vec<ReportRow> {
    let mut rows = Vec::new();
    if let Some(plan) = data.get("plan").filter(|p| p.is_object()) {
        rows.push(ReportRow::new("files", s(plan.get("fileCount")), ""));
        rows.push(ReportRow::new("chunks", s(plan.get("unitCount")), ""));
        rows.push(ReportRow::new("lines", s(plan.get("lines")), ""));
        rows.push(ReportRow::new("grouping", s(plan.get("group")), ""));
        if let Some(files) = plan.get("files").and_then(Value::as_array) {
            let names: Vec<String> = files
                .iter()
                .take(12)
                .map(|f| {
                    let path = s(f.get("path"));
                    let lines = s(f.get("lines"));
                    format!("{path} {lines}L")
                })
                .collect();
            if !names.is_empty() {
                rows.push(ReportRow::new("", names.join("  "), ""));
            }
        }
    }
    if let Some(rep) = data.get("report").filter(|r| r.is_object()) {
        let bad = arr_len(rep.get("missing")) > 0
            || arr_len(rep.get("added")) > 0
            || arr_len(rep.get("changed")) > 0;
        let before_decls = n(rep.get("before").and_then(|b| b.get("declarations")));
        let after_decls = n(rep.get("after").and_then(|b| b.get("declarations")));
        rows.push(ReportRow::new(
            "declarations",
            format!("{after_decls} / {before_decls}"),
            if bad { "bad" } else { "good" },
        ));
        let text = rep.get("text");
        let checked = n(text.and_then(|t| t.get("checked")));
        let missing = arr_len(text.and_then(|t| t.get("missing")));
        let text_ok = text
            .and_then(|t| t.get("ok"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        rows.push(ReportRow::new(
            "source text",
            format!("{} / {checked} spans kept", checked - missing as i64),
            if text_ok { "good" } else { "bad" },
        ));
        let unbalanced = rep.get("unbalanced").and_then(Value::as_array);
        rows.push(ReportRow::new(
            "balanced",
            if arr_len(rep.get("unbalanced")) > 0 {
                join_first(rep.get("unbalanced"), 3)
            } else {
                "yes".to_string()
            },
            if unbalanced.map(|u| !u.is_empty()).unwrap_or(false) {
                "bad"
            } else {
                "good"
            },
        ));
        if s(rep.get("scope")) == "corpus" {
            let equal = rep.get("equal").and_then(Value::as_bool).unwrap_or(false);
            let before_rel = n(rep.get("before").and_then(|b| b.get("relations")));
            let after_rel = n(rep.get("after").and_then(|a| a.get("relations")));
            rows.push(ReportRow::new(
                "relations",
                format!("{after_rel} / {before_rel}"),
                if equal { "good" } else { "warn" },
            ));
            if !equal {
                rows.push(ReportRow::new(
                    "edges drifted",
                    format!(
                        "{} lost, {} new",
                        arr_len(rep.get("relationsMissing")),
                        arr_len(rep.get("relationsAdded"))
                    ),
                    "warn",
                ));
            }
        }
        for (key, label, class) in [
            ("missing", "missing", "bad"),
            ("added", "invented", "bad"),
            ("changed", "changed", "bad"),
        ] {
            if arr_len(rep.get(key)) > 0 {
                rows.push(ReportRow::new(label, join_first(rep.get(key), 5), class));
            }
        }
    }
    if let Some(write) = data.get("write").filter(|w| w.is_object()) {
        let ok = write.get("ok").and_then(Value::as_bool).unwrap_or(false);
        rows.push(ReportRow::new(
            "written to",
            s(write.get("dest")),
            if ok { "good" } else { "bad" },
        ));
        if arr_len(write.get("errors")) > 0 {
            rows.push(ReportRow::new(
                "write errors",
                join_first(write.get("errors"), 3),
                "bad",
            ));
        }
    }
    match data.get("skipped") {
        Some(Value::String(skip)) if !skip.is_empty() => {
            rows.push(ReportRow::new("compile", skip.clone(), "warn"));
        }
        _ => {
            if data.get("newDiagnostics").is_some() {
                let diags = new_diagnostic_lines(data);
                rows.push(ReportRow::new(
                    "new diagnostics",
                    diags.len().to_string(),
                    if diags.is_empty() { "good" } else { "bad" },
                ));
                for line in diags.iter().take(4) {
                    rows.push(ReportRow::new("", line.clone(), "bad"));
                }
                if let (Some(cand), Some(base)) = (
                    data.get("candidate").and_then(|c| c.get("errors")),
                    data.get("baseline").and_then(|b| b.get("errors")),
                ) {
                    rows.push(ReportRow::new(
                        "errors",
                        format!("{cand} / {base} (was)"),
                        if diags.is_empty() { "good" } else { "bad" },
                    ));
                }
            }
        }
    }
    rows
}

/// Format `data.newDiagnostics` as `"CODE message"` lines (`reorg.js:107`:
/// `esc(d.code + ' ' + d.message)`).
pub fn new_diagnostic_lines(data: &Value) -> Vec<String> {
    let Some(items) = data.get("newDiagnostics").and_then(Value::as_array) else {
        return Vec::new();
    };
    items
        .iter()
        .map(|d| {
            let code = s(d.get("code"));
            let message = s(d.get("message"));
            if code.is_empty() {
                message
            } else {
                format!("{code} {message}")
            }
        })
        .collect()
}

// ---------------------------------------------------------------------- zip

/// Standard base64 decode (RFC 4648). The engine encodes the archive with
/// [`csgraph_core::index::b64_encode`] because a JSON transport cannot carry a
/// binary body (`index.rs:3243-3252`); this is the inverse, kept local so the
/// fork does not need a base64 crate.
pub fn b64_decode(text: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let bytes = text.trim().as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|c| **c == b'=').count();
        let a = val(chunk[0])?;
        let b = val(chunk[1])?;
        let c = if pad >= 2 { 0 } else { val(chunk[2])? };
        let d = if pad >= 1 { 0 } else { val(chunk[3])? };
        let n = (a << 18) | (b << 12) | (c << 6) | d;
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// The archive the engine packaged, held in memory because this host has no
/// browser download. `bytes` is the decoded payload, so a future caller can
/// write it without re-inventing the decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    pub name: String,
    pub bytes: Vec<u8>,
}

// ------------------------------------------------------------------ the view

/// One requested engine run, taken by the host after the nzk_ui pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub action: Action,
    pub request: ReorgRequest,
}

/// The export dialog's state on the host side: the form, the last engine
/// answer, and the note.
///
/// Like [`crate::edit_wire`], this never performs I/O itself: [`ReorgView`]
/// records the request and the answer; [`invoke`] is the one function that
/// calls the engine. `app.rs` owns that seam.
#[derive(Debug, Clone, Default)]
pub struct ReorgView {
    pub request: ReorgRequest,
    /// The raw engine answer, kept for the report and the facts.
    pub last: Option<Value>,
    /// `#reorgNote` text (`reorg.js:37-40`).
    pub note: String,
    /// `#reorgNote`'s extra class: `""`, `"bad"`, `"warn"` or `"good"` — the
    /// reference's `kind` argument, spelled in this tree's vocabulary.
    pub note_kind: String,
    /// The packaged `.zip`, if the last `zip` run produced one.
    pub archive: Option<Archive>,
    pending: Option<Pending>,
    busy: bool,
}

impl ReorgView {
    /// Record a press (`reorg.js:122-125` `setBusy(true, ...)`): the request is
    /// handed to the host, which runs it against the `Index`.
    pub fn begin(&mut self, action: Action) {
        self.pending = Some(Pending {
            action,
            request: self.request.clone(),
        });
        self.busy = true;
        self.note = "Working…".to_string();
        self.note_kind.clear();
    }

    /// Take the request once. The host calls this after the nzk_ui pass; a
    /// second call is `None`, so one press is one engine run.
    pub fn take_pending(&mut self) -> Option<Pending> {
        self.pending.take()
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// Record the engine's answer and derive the note (`reorg.js:124-134`,
    /// `:155-200`). The note is a function of the answer only; nothing here can
    /// claim a run the engine did not perform.
    pub fn finish(&mut self, pending: &Pending, data: Value) {
        self.busy = false;
        self.last = None;
        self.archive = None;
        let _ = &pending.action;
        // Clear the note kind before deriving a fresh one.
        self.note_kind.clear();
        if data.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let plan_files = n(data.get("plan").and_then(|p| p.get("fileCount")));
            let plan_chunks = n(data.get("plan").and_then(|p| p.get("unitCount")));
            match pending.action {
                Action::Plan => {
                    self.note = format!("Plan: {plan_files} file(s), {plan_chunks} chunk(s)");
                    self.note_kind = "good".to_string();
                }
                Action::Verify => {
                    self.note =
                        format!("Verified: {plan_files} file(s), no declaration lost");
                    self.note_kind = "good".to_string();
                }
                Action::Zip => self.finish_zip(&data),
                Action::Write => {
                    let w = data.get("write");
                    let written = arr_len(w.and_then(|w| w.get("written")));
                    let dest = s(w.and_then(|w| w.get("dest")));
                    self.note = format!("Wrote {written} file(s) to {dest}");
                    self.note_kind = "good".to_string();
                }
                Action::Compile => match data.get("skipped").and_then(Value::as_str) {
                    Some(skip) if !skip.is_empty() => {
                        self.note = format!("No toolchain found — compile check skipped ({skip})");
                        self.note_kind = "warn".to_string();
                    }
                    _ => {
                        let diags = new_diagnostic_lines(&data);
                        self.note = if diags.is_empty() {
                            "Compiles the same: no new diagnostics".to_string()
                        } else {
                            format!("Compile check found {} new diagnostic(s)", diags.len())
                        };
                        self.note_kind = if diags.is_empty() {
                            "good".to_string()
                        } else {
                            "bad".to_string()
                        };
                    }
                },
            }
        } else {
            // A `verify`/`write`/`compile` that did not pass still carries the
            // report; the note names the engine's own error when it gave one.
            let err = s(data.get("error"));
            self.note = match pending.action {
                Action::Verify => {
                    if err.is_empty() {
                        "Verification found a problem — see the report".to_string()
                    } else {
                        format!("Verification failed: {err}")
                    }
                }
                Action::Zip => {
                    if err.is_empty() {
                        "the output did not verify — not packaged".to_string()
                    } else {
                        format!("Packaging failed: {err}")
                    }
                }
                Action::Write => {
                    let first_error = join_first(
                        data.get("write").and_then(|w| w.get("errors")),
                        1,
                    );
                    if !first_error.is_empty() {
                        format!("write failed: {first_error}")
                    } else if !err.is_empty() {
                        format!("write failed: {err}")
                    } else {
                        "write failed".to_string()
                    }
                }
                Action::Compile => {
                    if err.is_empty() {
                        "compile check found a difference — see the report".to_string()
                    } else {
                        format!("compile check failed: {err}")
                    }
                }
                Action::Plan => {
                    if err.is_empty() {
                        "plan failed — see the report".to_string()
                    } else {
                        format!("plan failed: {err}")
                    }
                }
            };
            self.note_kind = "bad".to_string();
        }
        self.last = Some(data);
    }

    /// `zip`'s note: decode the engine's base64 payload. When the request named
    /// a destination, write the real archive bytes there — to that exact file
    /// when it ends in `.zip`, otherwise into that folder under the engine's
    /// name. With no destination the archive stays in memory and the note says
    /// so; this host has no HTTP/browser download path and never claims one.
    fn finish_zip(&mut self, data: &Value) {
        let name = {
            let n = s(data.get("name"));
            if n.is_empty() {
                "reorg.zip".to_string()
            } else {
                n
            }
        };
        let b64 = s(data.get("data"));
        let Some(bytes) = b64_decode(&b64) else {
            self.note = "the engine's archive payload did not decode — nothing written".to_string();
            self.note_kind = "bad".to_string();
            return;
        };
        let len = bytes.len();
        match self.request.write_dest() {
            Some(dest) => {
                // `dest` naming the archive file is written verbatim; a folder
                // destination gets the engine's archive name joined onto it.
                let path = if dest.to_ascii_lowercase().ends_with(".zip") {
                    std::path::PathBuf::from(dest)
                } else {
                    std::path::Path::new(dest).join(&name)
                };
                let result = match path.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => {
                        std::fs::create_dir_all(parent)
                    }
                    _ => Ok(()),
                }
                .and_then(|_| std::fs::write(&path, &bytes));
                match result {
                    Ok(()) => {
                        self.note = format!("Wrote {name} ({len} bytes) to {}", path.display());
                        self.note_kind = "good".to_string();
                    }
                    Err(e) => {
                        // Never fake success: the note names the real failure.
                        self.note = format!("could not write {}: {e}", path.display());
                        self.note_kind = "bad".to_string();
                    }
                }
            }
            None => {
                // No HTTP, no Blob, no download: the honest answer is that the
                // archive exists in this process only.
                self.note = format!(
                    "Packaged {name} ({len} bytes) and kept it in memory — this native build has no browser download"
                );
                self.note_kind = "warn".to_string();
            }
        }
        self.archive = Some(Archive { name, bytes });
    }

    /// The `ui.reorg` facts, or `None` when the dialog has never been opened
    /// and no run happened — `None` is what keeps the shipped document (and the
    /// `@frame` token) byte-identical.
    ///
    /// `open` is the dialog's own open state from `modals_open`
    /// (`HostSurface::Export`). When it is open, every form value is published
    /// so the shell can paint the controls from the state it actually owns;
    /// when it is closed, only a completed run's report/note is published (the
    /// dialog is hidden so the values cannot be read anyway).
    pub fn facts(&self, open: bool) -> Option<Value> {
        if !open && self.last.is_none() && self.note.is_empty() {
            return None;
        }
        let rows: Vec<Value> = self
            .last
            .as_ref()
            .map(|d| report_rows(d).iter().map(ReportRow::to_json).collect())
            .unwrap_or_default();
        Some(json!({
            "mode": self.request.mode.as_str(),
            "scope": self.request.scope.as_str(),
            "gran": self.request.gran.as_str(),
            "group": self.request.group.as_str(),
            "size": self.request.size,
            "header": self.request.header,
            "dest": self.request.dest,
            "has_report": self.last.is_some(),
            "rows": rows,
            "note": self.note,
            "note_kind": self.note_kind,
        }))
    }
}

/// A one-shot decode for the write path (the bytes were already validated, but
/// re-decoding avoids holding two copies of the accounting between the two
/// branches of [`ReorgView::finish_zip`]).
fn b64_decode_owned(text: &str) -> Vec<u8> {
    b64_decode(text).unwrap_or_default()
}

// ------------------------------------------------------------------- engine

/// Run one pending request against the engine, in process.
///
/// This is the fork's `/api/reorg` route: identical arguments to
/// [`Index::reorg`] — `overwrite` is `true`, the reference's absent-means-true
/// default (`api.rs:555-565`). The `write` action is guarded here (empty dest)
/// before the engine is called at all.
pub fn invoke(index: &mut Index, pending: &Pending, selection: &[String]) -> Value {
    let spec = pending.request.spec(selection);
    match pending.action {
        Action::Write => match pending.request.write_dest() {
            Some(dest) => index.reorg(Some(&spec), "write", Some(dest), true),
            None => json!({"ok": false, "error": "enter an absolute folder to write to"}),
        },
        action => index.reorg(Some(&spec), action.as_str(), None, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine's own encoder, used to prove the decoder is its inverse
    /// rather than a fixture this module invented.
    fn encode(bytes: &[u8]) -> String {
        csgraph_core::index::b64_encode(bytes)
    }

    // ---------------------------------------------------------- request/spec

    #[test]
    fn default_request_is_the_shipped_markup_state() {
        let r = ReorgRequest::default();
        assert_eq!(r.mode, ReorgMode::Single);
        assert_eq!(r.scope, Scope::Selection);
        assert_eq!(r.gran, Gran::Leaf);
        assert_eq!(r.group, ReorgGroup::Size);
        assert_eq!(r.size, 3000);
        assert!(r.header);
        assert_eq!(r.dest, "");
    }

    #[test]
    fn spec_matches_the_reference_shape() {
        let r = ReorgRequest::default();
        assert_eq!(
            r.spec(&[]),
            json!({"header": true, "granularity": "leaf", "group": "single"})
        );
        let split = ReorgRequest {
            mode: ReorgMode::Split,
            ..ReorgRequest::default()
        };
        assert_eq!(
            split.spec(&[]),
            json!({"header": true, "granularity": "leaf", "group": "size", "size": 3000})
        );
    }

    #[test]
    fn spec_sends_units_only_for_a_non_empty_selection_scope() {
        let ids = vec!["a".to_string(), "b".to_string()];
        let selection = ReorgRequest::default();
        assert_eq!(selection.spec(&ids)["units"], json!(["a", "b"]));
        assert_eq!(selection.spec(&[])["units"], Value::Null);
        let all = ReorgRequest {
            scope: Scope::All,
            ..ReorgRequest::default()
        };
        assert_eq!(all.spec(&ids)["units"], Value::Null);
    }

    #[test]
    fn size_is_clamped_to_the_input_minimum() {
        let r = ReorgRequest {
            mode: ReorgMode::Split,
            size: 1,
            ..ReorgRequest::default()
        };
        assert_eq!(r.spec(&[])["size"], json!(50));
    }

    #[test]
    fn empty_destination_is_none_never_the_empty_string() {
        assert_eq!(ReorgRequest::default().write_dest(), None);
        let r = ReorgRequest {
            dest: "   ".to_string(),
            ..ReorgRequest::default()
        };
        assert_eq!(r.write_dest(), None);
        let r = ReorgRequest {
            dest: " /tmp/out ".to_string(),
            ..ReorgRequest::default()
        };
        assert_eq!(r.write_dest(), Some("/tmp/out"));
    }

    #[test]
    fn only_the_four_shipped_controls_issue_actions() {
        assert_eq!(Action::from_control("#reorgVerify"), Some(Action::Verify));
        assert_eq!(Action::from_control("#reorgZip"), Some(Action::Zip));
        assert_eq!(Action::from_control("#reorgWrite"), Some(Action::Write));
        assert_eq!(Action::from_control("#reorgCompile"), Some(Action::Compile));
        // `#reorgPlan` is created by the reference script, not shipped here.
        assert_eq!(Action::from_control("#reorgPlan"), None);
        assert_eq!(Action::from_control("#reorgClose"), None);
        assert_eq!(Action::Verify.as_str(), "verify");
        assert_eq!(Action::Zip.as_str(), "zip");
        assert_eq!(Action::Write.as_str(), "write");
        assert_eq!(Action::Compile.as_str(), "compile");
        assert_eq!(Action::Plan.as_str(), "plan");
    }

    // ------------------------------------------------------------- formatting

    fn verify_answer() -> Value {
        json!({
            "ok": true,
            "plan": {"fileCount": 3, "unitCount": 2, "lines": 120, "group": "size",
                     "files": [{"path": "a.cs", "lines": 60}, {"path": "b.cs", "lines": 60}]},
            "report": {
                "before": {"declarations": 9, "relations": 4},
                "after": {"declarations": 9, "relations": 4},
                "text": {"checked": 9, "missing": [], "ok": true},
                "unbalanced": [],
                "missing": [], "added": [], "changed": [],
                "equal": true,
                "scope": "corpus",
            }
        })
    }

    #[test]
    fn report_rows_are_the_reference_rows_for_a_clean_verify() {
        let rows = report_rows(&verify_answer());
        let pairs: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|r| (r.k.as_str(), r.v.as_str(), r.class.as_str()))
            .collect();
        assert_eq!(pairs[0], ("files", "3", ""));
        assert_eq!(pairs[1], ("chunks", "2", ""));
        assert_eq!(pairs[2], ("lines", "120", ""));
        assert_eq!(pairs[3], ("grouping", "size", ""));
        assert_eq!(pairs[4].0, ""); // the file preview line
        assert_eq!(pairs[5], ("declarations", "9 / 9", "good"));
        assert_eq!(pairs[6], ("source text", "9 / 9 spans kept", "good"));
        assert_eq!(pairs[7], ("balanced", "yes", "good"));
        assert_eq!(pairs[8], ("relations", "4 / 4", "good"));
        // A clean answer has no bad rows at all.
        assert!(rows.iter().all(|r| r.class != "bad"));
    }

    #[test]
    fn a_lost_declaration_and_text_span_are_bad_rows() {
        let mut ans = verify_answer();
        ans["report"]["after"]["declarations"] = json!(8);
        ans["report"]["missing"] = json!(["a.cs"]);
        ans["report"]["text"]["missing"] = json!(["a.cs"]);
        ans["report"]["text"]["ok"] = json!(false);
        ans["report"]["unbalanced"] = json!(["b.cs"]);
        let rows = report_rows(&ans);
        let find = |k: &str| rows.iter().find(|r| r.k == k).unwrap().clone();
        assert_eq!(find("declarations"), ReportRow::new("declarations", "8 / 9", "bad"));
        assert_eq!(find("source text"), ReportRow::new("source text", "8 / 9 spans kept", "bad"));
        assert_eq!(find("balanced"), ReportRow::new("balanced", "b.cs", "bad"));
    }

    #[test]
    fn missing_added_and_changed_names_are_listed_and_marked_bad() {
        let mut ans = verify_answer();
        ans["report"]["missing"] = json!(["a.cs", "b.cs"]);
        ans["report"]["added"] = json!(["x.cs"]);
        ans["report"]["changed"] = json!(["y.cs"]);
        let rows = report_rows(&ans);
        let missing = rows.iter().find(|r| r.k == "missing").unwrap();
        assert_eq!(missing.v, "a.cs, b.cs");
        assert_eq!(missing.class, "bad");
        assert_eq!(rows.iter().find(|r| r.k == "invented").unwrap().v, "x.cs");
        assert_eq!(rows.iter().find(|r| r.k == "changed").unwrap().v, "y.cs");
    }

    #[test]
    fn relations_drift_adds_the_warn_row_only_when_unequal() {
        let mut ans = verify_answer();
        ans["report"]["equal"] = json!(false);
        ans["report"]["relationsMissing"] = json!(["a->b"]);
        ans["report"]["relationsAdded"] = json!([]);
        let rows = report_rows(&ans);
        let drift = rows.iter().find(|r| r.k == "edges drifted").unwrap();
        assert_eq!(drift.v, "1 lost, 0 new");
        assert_eq!(drift.class, "warn");
        // The equal answer must NOT carry that row.
        assert!(report_rows(&verify_answer()).iter().all(|r| r.k != "edges drifted"));
    }

    #[test]
    fn a_write_answer_shows_the_destination_and_errors() {
        let ans = json!({
            "ok": false,
            "write": {"ok": false, "dest": "/tmp/out", "errors": ["denied", "again"]}
        });
        let rows = report_rows(&ans);
        assert_eq!(rows[0].k, "written to");
        assert_eq!(rows[0].v, "/tmp/out");
        assert_eq!(rows[0].class, "bad");
        assert_eq!(rows[1].k, "write errors");
        assert_eq!(rows[1].v, "denied, again");
    }

    #[test]
    fn compile_rows_cover_skipped_and_the_diagnostic_list() {
        let skipped = json!({"ok": true, "skipped": "no Unity editor found"});
        let rows = report_rows(&skipped);
        assert_eq!(
            rows.iter().find(|r| r.k == "compile").unwrap(),
            &ReportRow::new("compile", "no Unity editor found", "warn")
        );

        let found = json!({
            "ok": false,
            "newDiagnostics": [
                {"code": "CS0234", "message": "missing namespace"},
                {"code": "CS0103", "message": "name not found"},
            ],
            "candidate": {"errors": 2}, "baseline": {"errors": 0},
        });
        assert_eq!(
            new_diagnostic_lines(&found),
            vec!["CS0234 missing namespace", "CS0103 name not found"]
        );
        let rows = report_rows(&found);
        assert_eq!(
            rows.iter().find(|r| r.k == "new diagnostics").unwrap().v,
            "2"
        );
        assert_eq!(
            rows.iter().find(|r| r.k == "errors").unwrap().v,
            "2 / 0 (was)"
        );
        // A clean compile has zero diagnostics and a good count row.
        let clean = json!({"ok": true, "newDiagnostics": [],
                           "candidate": {"errors": 0}, "baseline": {"errors": 0}});
        assert!(new_diagnostic_lines(&clean).is_empty());
        let rows = report_rows(&clean);
        assert_eq!(rows.iter().find(|r| r.k == "new diagnostics").unwrap().class, "good");
        assert_eq!(rows.iter().find(|r| r.k == "errors").unwrap().v, "0 / 0 (was)");
    }

    #[test]
    fn a_message_only_diagnostic_is_not_prefixed_with_a_blank_code() {
        assert_eq!(
            new_diagnostic_lines(&json!({"newDiagnostics": [{"message": "boom"}]})),
            vec!["boom"]
        );
    }

    // -------------------------------------------------------------- base64

    #[test]
    fn base64_decode_is_the_engine_encoders_inverse() {
        for bytes in [
            b"".as_slice(),
            b"f".as_slice(),
            b"fo".as_slice(),
            b"foo".as_slice(),
            b"foob".as_slice(),
            b"fooba".as_slice(),
            b"foobar".as_slice(),
            &[0u8, 255, 128, 1, 2, 3],
        ] {
            let encoded = encode(bytes);
            assert_eq!(b64_decode(&encoded).as_deref(), Some(bytes), "{encoded}");
        }
    }

    #[test]
    fn base64_decode_refuses_malformed_text_instead_of_guessing() {
        assert_eq!(b64_decode("abc"), None); // not a multiple of 4
        assert_eq!(b64_decode("!!!!"), None);
        assert_eq!(b64_decode("AB=C"), None); // '=' before the pad positions
    }

    // ---------------------------------------------------------------- engine

    /// The real engine call: an unindexed `Index` answers with the engine's own
    /// refusal, so [`invoke`] is proven to reach `Index::reorg` rather than a
    /// hand-built stub.
    #[test]
    fn invoke_reaches_the_engine_and_returns_its_refusal() {
        let mut index = Index::new(std::env::temp_dir());
        let pending = Pending {
            action: Action::Verify,
            request: ReorgRequest::default(),
        };
        let data = invoke(&mut index, &pending, &[]);
        assert_eq!(data, json!({"ok": false, "error": "nothing indexed"}));
    }

    /// The empty-destination guard must fire **before** the engine call: the
    /// engine would otherwise be handed the repo root as a destination.
    #[test]
    fn a_write_without_a_destination_never_calls_the_engine() {
        let mut index = Index::new(std::env::temp_dir());
        let pending = Pending {
            action: Action::Write,
            request: ReorgRequest::default(),
        };
        let data = invoke(&mut index, &pending, &[]);
        assert_eq!(
            data,
            json!({"ok": false, "error": "enter an absolute folder to write to"})
        );
    }

    // ------------------------------------------------------------------ view

    fn pending(action: Action) -> Pending {
        Pending {
            action,
            request: ReorgRequest::default(),
        }
    }

    #[test]
    fn a_fresh_view_publishes_nothing_while_closed() {
        let view = ReorgView::default();
        assert_eq!(view.facts(false), None);
        // Open, it publishes the shipped defaults so the select values can be
        // painted from host state.
        let facts = view.facts(true).expect("open dialog publishes its form");
        assert_eq!(facts["mode"], "single");
        assert_eq!(facts["scope"], "selection");
        assert_eq!(facts["gran"], "leaf");
        assert_eq!(facts["group"], "size");
        assert_eq!(facts["size"], 3000);
        assert_eq!(facts["header"], true);
        assert_eq!(facts["rows"], json!([]));
        assert_eq!(facts["note"], "");
    }

    #[test]
    fn begin_hands_exactly_one_request_to_the_host() {
        let mut view = ReorgView::default();
        view.begin(Action::Verify);
        assert!(view.is_busy());
        assert_eq!(view.note, "Working…");
        let p = view.take_pending().expect("one pending");
        assert_eq!(p.action, Action::Verify);
        assert_eq!(p.request, ReorgRequest::default());
        assert_eq!(view.take_pending(), None);
    }

    #[test]
    fn a_successful_verify_sets_the_reference_note_and_report_rows() {
        let mut view = ReorgView::default();
        let p = pending(Action::Verify);
        view.finish(&p, verify_answer());
        assert!(!view.is_busy());
        assert_eq!(view.note, "Verified: 3 file(s), no declaration lost");
        assert_eq!(view.note_kind, "good");
        let facts = view.facts(false).expect("a finished run publishes facts");
        assert_eq!(facts["rows"].as_array().unwrap()[0]["k"], "files");
        assert_eq!(facts["note"], view.note);
    }

    #[test]
    fn a_failed_verify_names_the_engine_error_and_stays_bad() {
        let mut view = ReorgView::default();
        view.finish(&pending(Action::Verify), json!({"ok": false, "error": "nothing indexed"}));
        assert_eq!(view.note, "Verification failed: nothing indexed");
        assert_eq!(view.note_kind, "bad");
    }

    #[test]
    fn a_skipped_compile_is_a_warning_not_a_pass() {
        let mut view = ReorgView::default();
        view.finish(
            &pending(Action::Compile),
            json!({"ok": true, "skipped": "no toolchain"}),
        );
        assert!(view.note.starts_with("No toolchain found"));
        assert_eq!(view.note_kind, "warn");
    }

    #[test]
    fn a_write_names_the_written_count_and_destination() {
        let mut view = ReorgView::default();
        view.finish(
            &pending(Action::Write),
            json!({"ok": true, "write": {"ok": true, "dest": "/tmp/out",
                                          "written": ["a.cs", "b.cs"]}}),
        );
        assert_eq!(view.note, "Wrote 2 file(s) to /tmp/out");
        assert_eq!(view.note_kind, "good");
    }

    #[test]
    fn a_headless_zip_is_kept_in_memory_and_says_so() {
        let bytes = b"PK-not-really-a-zip";
        let b64 = encode(bytes);
        let mut view = ReorgView::default();
        view.finish(
            &pending(Action::Zip),
            json!({"ok": true, "name": "corpus.zip", "data": b64}),
        );
        assert_eq!(
            view.note,
            format!(
                "Packaged corpus.zip ({} bytes) and kept it in memory — this native build has no browser download",
                bytes.len()
            )
        );
        assert_eq!(view.note_kind, "warn");
        assert_eq!(
            view.archive,
            Some(Archive {
                name: "corpus.zip".to_string(),
                bytes: bytes.to_vec()
            })
        );
        // The archive bytes never leak into the JSON facts.
        let facts = view.facts(false).unwrap();
        assert_eq!(facts["rows"], json!([]));
    }

    #[test]
    fn an_undecodable_archive_payload_is_refused_not_written() {
        let mut view = ReorgView::default();
        view.finish(
            &pending(Action::Zip),
            json!({"ok": true, "name": "x.zip", "data": "***"}),
        );
        assert_eq!(view.note_kind, "bad");
        assert_eq!(view.archive, None);
        assert!(view.note.contains("did not decode"));
    }

    // -------------------------------------------------- zip written to disk

    fn temp_dir_for(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("csviewer-reorg-zip-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// A **valid empty ZIP** (the EOCD record, `PK\x05\x06` + 18 zero bytes),
    /// so the test proves real archive bytes reach the disk.
    fn empty_zip_bytes() -> Vec<u8> {
        let mut zip = vec![0x50, 0x4b, 0x05, 0x06];
        zip.extend_from_slice(&[0u8; 18]);
        zip
    }

    #[test]
    fn a_named_zip_file_dest_receives_the_real_archive_bytes() {
        let dir = temp_dir_for("file-dest");
        let dest = dir.join("out.zip");
        let zip = empty_zip_bytes();
        let mut view = ReorgView::default();
        // `finish_zip` reads the view's own request (the live form state); the
        // pending clone is the same value in the real host path.
        view.request.dest = dest.to_string_lossy().to_string();
        view.finish(
            &pending(Action::Zip),
            json!({"ok": true, "name": "corpus.zip", "data": encode(&zip)}),
        );
        let on_disk = std::fs::read(&dest).expect("archive written to the named .zip dest");
        assert_eq!(on_disk, zip, "the bytes on disk are exactly the engine payload");
        assert!(on_disk.starts_with(b"PK"), "a real zip signature reached the disk");
        assert_eq!(view.note_kind, "good");
        assert!(view.note.contains(&format!("{} bytes", zip.len())), "{}", view.note);
        assert!(view.note.contains(dest.to_string_lossy().as_ref()), "{}", view.note);
        // The archive is still handed to the caller as well.
        assert_eq!(view.archive.expect("in-memory archive").bytes, zip);
    }

    #[test]
    fn a_folder_dest_receives_the_archive_under_its_engine_name() {
        let dir = temp_dir_for("folder-dest");
        let folder = dir.join("out");
        let zip = empty_zip_bytes();
        let mut view = ReorgView::default();
        view.request.dest = folder.to_string_lossy().to_string();
        view.finish(
            &pending(Action::Zip),
            json!({"ok": true, "name": "corpus.zip", "data": encode(&zip)}),
        );
        let written = folder.join("corpus.zip");
        assert_eq!(std::fs::read(&written).expect("archive written into folder"), zip);
        assert_eq!(view.note_kind, "good");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_zip_write_is_reported_honestly_never_faked() {
        let dir = temp_dir_for("fail-dest");
        // A regular file where the parent folder would have to be: create_dir_all
        // fails, so the write fails. The note must say so and stay "bad".
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("blocker file");
        let zip = empty_zip_bytes();
        let mut view = ReorgView::default();
        view.request.dest = blocker.join("out.zip").to_string_lossy().to_string();
        view.finish(
            &pending(Action::Zip),
            json!({"ok": true, "name": "corpus.zip", "data": encode(&zip)}),
        );
        assert_eq!(view.note_kind, "bad", "a failed write must never be 'good'");
        assert!(view.note.contains("could not write"), "{}", view.note);
        // The decoded archive is still available in memory — honest about both.
        assert_eq!(view.archive.expect("in-memory archive").bytes, zip);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zip_without_a_dest_never_touches_the_disk() {
        let zip = empty_zip_bytes();
        let mut view = ReorgView::default();
        view.finish(
            &pending(Action::Zip),
            json!({"ok": true, "name": "corpus.zip", "data": encode(&zip)}),
        );
        assert_eq!(view.note_kind, "warn");
        assert!(!view.note.contains("Wrote"), "{}", view.note);
        assert_eq!(view.archive.expect("in-memory archive").bytes, zip);
    }
}
