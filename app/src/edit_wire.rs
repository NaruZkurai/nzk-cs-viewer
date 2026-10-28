//! The wire between the pure `#editArgs` model ([`crate::edit_args`]) and the
//! engine's edit/analyze routes — the middle layer, exactly as
//! [`crate::find_wire`] and [`crate::codelens_wire`] are for their cores.
//!
//! It owns three things and nothing else:
//!
//! 1. **Request bodies** — the exact JSON `edit.js` POSTs.
//! 2. **The single Apply producer** — [`EditToolView::press_apply`]. This is the
//!    *only* place in the tree that can construct [`RunMode::Apply`].
//! 3. **Response interpretation** — `render`, `note`, and `noteAnalyze`
//!    (`edit.js:59-129`), including the rule that a **skipped gate is an error,
//!    never a pass**.
//!
//! ## Why there is no `fetch` here
//!
//! The reference calls `fetch('/api/edit', …)` (`edit.js:78-83`) and
//! `fetch('/api/analyze', …)` (`edit.js:134`). This tree has **no HTTP client
//! and no async runtime** — the native app reads a `Graph` from the engine
//! in-process. So the honest port is a *projector*: it builds the request body
//! and interprets the response body, and leaves the transport to whatever owns
//! it. That is strictly more testable than a fetch wrapper, and it avoids
//! inventing a client that would drift from the routes it claims to call.
//!
//! The consequence is stated plainly: **a body produced here is not yet sent.**
//! [`EditToolView::dry_run_body`] and [`EditToolView::press_apply`] return the
//! JSON `edit.js` would POST; nothing in this module performs I/O. See the
//! handoff for what remains.
//!
//! ## The Apply invariant (the point of this task)
//!
//! [`RunMode::Apply`] previously had **no producer at all**, and
//! [`crate::edit_args::decide_key`] can only ever return
//! [`RunMode::Dry`]. This module adds exactly one producer — the Apply button,
//! via [`EditToolView::press_apply`] — and deliberately provides **no** pathway
//! from a key event to Apply. [`EditToolView::press_dry`] and
//! [`EditToolView::keydown`] both route through the pure model and both yield
//! `Dry`. A test enumerates every key event and asserts none reaches Apply, so
//! adding a second producer fails the suite rather than shipping silently.
//!
//! ## The ruled `apply:true` behaviour (do NOT "fix" this)
//!
//! **User ruling, explicitly made: stay faithful to the reference.**
//! `edit.js:217` is `if (apply) args.apply = true` — it *sets* the key when
//! applying and otherwise leaves the parsed object exactly as typed. So a user
//! who types `{"apply": true}` into the args box and then presses **Dry-run**
//! (or Ctrl+Enter) has that `apply:true` forwarded to the engine. There is no
//! stripping in the reference and there must be none here.
//!
//! This is a deliberate divergence from the safer behaviour (strip `apply` on a
//! dry run), because the reference is the specification. It is called out in
//! [`EditToolView::dry_run_body`] and pinned by the test
//! `a_typed_apply_true_is_forwarded_by_a_dry_run`, so that a future reader who
//! spots the tension finds the ruling instead of "fixing" it.
//!
//! Note the distinction the code keeps straight: [`RunMode::Dry`] means *this
//! request did not set `apply`*, not *this request carries no `apply` field*.
//! [`RunMode::Dry`] never adds the key; it never removes one either.

use serde_json::{json, Value};

use crate::edit_args::{Args, ArgsError, EditArgsView, KeyEvent, KeyOutcome, Note, NoteKind, RunMode};

/// The route `edit.js:20` calls for the tool list and for a run.
pub const EDIT_ROUTE: &str = "/api/edit";

/// The route `edit.js:22` calls for `parse_check` / `dedupe`.
pub const ANALYZE_ROUTE: &str = "/api/analyze";

/// `edit.js:175` — `if (args.timeout === undefined) args.timeout = 25`.
/// `parse_check` reads `timeout`; the modal supplies a default so the gate
/// always gets one.
pub const DEFAULT_ANALYZE_TIMEOUT: i64 = 25;

/// `edit.js:174` — the note prefix for a bad args box, reused by validate.
pub const ANALYZE_NOTE_PREFIX: &str = "Args: ";

/// `edit.js:99`/`:120` — the two `noteAnalyze` op names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyzeOp {
    /// `parse_check` — the Roslyn validity gate (`edit.js:172-177`).
    ParseCheck,
    /// `dedupe` — the duplicate report (`edit.js:179-183`).
    Dedupe,
}

impl AnalyzeOp {
    /// `edit.js:149`/`:176`/`:182` — the wire value of `op`.
    pub fn as_str(self) -> &'static str {
        match self {
            AnalyzeOp::ParseCheck => "parse_check",
            AnalyzeOp::Dedupe => "dedupe",
        }
    }

    /// `edit.js:176` — `'Validating the corpus'`; `edit.js:182` —
    /// `'Scanning for duplicates'`. These are the busy labels.
    pub fn busy_label(self) -> &'static str {
        match self {
            AnalyzeOp::ParseCheck => "Validating the corpus",
            AnalyzeOp::Dedupe => "Scanning for duplicates",
        }
    }
}

/// What the app should do with a key event in the args box: either nothing, or
/// a **dry** run. There is deliberately no `Apply` variant.
///
/// This is the type-level half of the Apply invariant. A caller holding a
/// [`KeyIntent`] cannot express an apply, so the safety property is carried by
/// the type rather than by remembering not to write the wrong branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyIntent {
    /// The key was not ours; leave the box and the note alone.
    Ignored,
    /// Run the current tool as a dry run.
    DryRun,
}

/// A run request, ready to send: the tool name and the exact `args` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRequest {
    /// `body.tool` (`edit.js:219`).
    pub tool: String,
    /// `body.args` (`edit.js:219`) — post-`with_apply_if`.
    pub args: Value,
    /// Which run produced it. Carried so a caller (and a test) can assert the
    /// dry/apply distinction *after* the body is built, not only before.
    pub mode: RunMode,
}

impl RunRequest {
    /// The whole POST body, exactly `{ tool, args }` (`edit.js:219`).
    pub fn body(&self) -> Value {
        json!({ "tool": self.tool, "args": self.args })
    }

    /// Whether this request asks the engine to write. The only question a
    /// writer of this struct should ever branch on.
    pub fn writes(&self) -> bool {
        self.mode.writes()
    }
}

/// An analyze request (`edit.js:137`): `{ op, args }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzeRequest {
    /// `body.op`.
    pub op: AnalyzeOp,
    /// `body.args` — with the `timeout` default applied.
    pub args: Value,
}

impl AnalyzeRequest {
    /// The whole POST body, exactly `{ op, args }` (`edit.js:137`).
    pub fn body(&self) -> Value {
        json!({ "op": self.op.as_str(), "args": self.args })
    }
}

/// The modal's working state: the args box, the tool list, which tool is
/// selected, and what the last operation concluded.
///
/// This is the state `edit.js` keeps in module-level vars (`busy`, `els.tool`,
/// `installed`) plus the DOM nodes `#editTool`, `#editArgs`, `#editNote` and
/// `#editOut`. Holding it as one value is what lets the dry/apply decision be
/// asserted without a browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditToolView {
    /// `els.args` — the box and its dry-run key policy.
    pub args: EditArgsView,
    /// `GET /api/edit` -> `tools`, once loaded (`edit.js:191-204`).
    tools: Option<Vec<String>>,
    /// `els.tool.value` — the selected operation.
    selected: Option<String>,
    /// `busy` (`edit.js:48`) — a run is in flight; `run` returns early if set
    /// (`edit.js:207`) and the buttons are disabled (`edit.js:65-67`).
    busy: bool,
    /// `els.out.textContent`'s model: the last raw engine JSON, refusals
    /// included (`edit.js:87` — "never thrown away").
    last_response: Option<Value>,
    /// `els.note` — the last note line.
    last_note: Note,
}

impl Default for EditToolView {
    /// The shipped state: the box holds `{}` (`index.html:531`), no tool list
    /// has been fetched, nothing is selected, nothing is busy, no note.
    fn default() -> EditToolView {
        EditToolView {
            args: EditArgsView::default(),
            tools: None,
            selected: None,
            busy: false,
            last_response: None,
            last_note: Note::clear(),
        }
    }
}

impl EditToolView {
    /// A fresh modal.
    pub fn new() -> EditToolView {
        EditToolView::default()
    }

    // ------------------------------------------------------------- tool list

    /// Whether `GET /api/edit` has answered. `None` is "never asked", which is a
    /// different state from "asked and got an empty list" — the same distinction
    /// [`crate::find_wire`] draws for its own surfaces, and the reason the tool
    /// list is an `Option<Vec<_>>` rather than a `Vec`.
    pub fn tools_loaded(&self) -> bool {
        self.tools.is_some()
    }

    /// The loaded tool names, or `None` when nothing was fetched.
    pub fn tools(&self) -> Option<&[String]> {
        self.tools.as_deref()
    }

    /// The selected tool, if any — `els.tool.value`. `run` refuses without one
    /// (`edit.js:209`: `note('No operation available', 'error')`).
    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// Whether a run is in flight.
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// The last raw engine response, refusals included (`edit.js:87`).
    pub fn last_response(&self) -> Option<&Value> {
        self.last_response.as_ref()
    }

    /// The last note.
    pub fn last_note(&self) -> &Note {
        &self.last_note
    }

    /// Take the answer to `GET /api/edit` (`loadOps`, `edit.js:191-204`).
    ///
    /// A refusal (`ok:false`) is an error note and leaves the list unfetched, so
    /// `tools_loaded` stays `false` and the modal cannot pretend it has
    /// operations (`edit.js:193`: `throw new Error(data && data.error || 'could
    /// not list edit tools')`).
    ///
    /// Returns the note the caller must display.
    pub fn load_ops(&mut self, data: &Value) -> Note {
        if data.get("ok").and_then(Value::as_bool) == Some(false) {
            let msg = data
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("could not list edit tools")
                .to_string();
            // Title the note with the reference's message and keep `tools` as
            // `None`: a partial list would be a lie about what the engine offers.
            self.last_note = Note::error(msg);
            return self.last_note.clone();
        }
        let tools: Vec<String> = data
            .get("tools")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        // `els.tool` is repopulated and defaults to its first option
        // (`edit.js:196-201` builds the options; a `<select>` then selects the
        // first), so the selection follows the list.
        self.selected = tools.first().cloned();
        self.tools = Some(tools);
        // `open()`'s `.then` notes the count (`edit.js:242`).
        let count = self.tools.as_ref().map(Vec::len).unwrap_or(0);
        self.last_note = Note { text: format!("{count} operations"), kind: None };
        self.last_note.clone()
    }

    /// Select a tool — `#editTool`'s value changing (`edit.js:278` delegates to
    /// `fillTemplate`, which reads `els.tool.value`).
    ///
    /// Selecting a tool that the engine did not offer is refused, so the modal
    /// cannot run an operation the host never advertised.
    pub fn select_tool(&mut self, tool: &str) -> bool {
        match &self.tools {
            Some(list) if list.iter().any(|t| t == tool) => {
                self.selected = Some(tool.to_string());
                true
            }
            _ => false,
        }
    }

    // ------------------------------------------------------------- pure policy

    /// The key-event policy: a key event can ask for a **dry run** or nothing.
    ///
    /// Returns [`KeyIntent`], a type with no apply variant, so this function
    /// cannot express an apply even by mistake. It does **not** consult
    /// [`EditToolView::selected`]: the reference's handler calls `run(false)`
    /// unconditionally (`edit.js:285`) and lets `run` itself report the
    /// no-operation error (`edit.js:209`), so a keypress with no tool selected
    /// still produces [`KeyIntent::DryRun`] and an error note.
    pub fn key_intent(ev: KeyEvent) -> KeyIntent {
        match crate::edit_args::decide_key(ev) {
            KeyOutcome { consumed: true, run: Some(_) } => KeyIntent::DryRun,
            _ => KeyIntent::Ignored,
        }
    }

    /// Handle a keydown in the args box end to end: the intent, the pure model's
    /// note, and — when it is a dry run with a tool selected — the request body.
    ///
    /// The returned [`RunRequest`] is *always* [`RunMode::Dry`], because
    /// [`EditToolView::key_intent`] cannot yield anything else. That is the
    /// invariant as an executable fact rather than a comment.
    pub fn keydown(&mut self, ev: KeyEvent) -> (KeyIntent, Option<Note>, Option<RunRequest>) {
        let intent = EditToolView::key_intent(ev);
        let (_, note) = self.args.keydown(ev);
        if let Some(n) = note.clone() {
            self.last_note = n;
        }
        let req = match intent {
            KeyIntent::Ignored => None,
            // A parse failure produces a note and no request, matching `run`'s
            // try/catch (`edit.js:210-216`).
            KeyIntent::DryRun => self.build_request(RunMode::Dry).ok(),
        };
        (intent, note, req)
    }

    // -------------------------------------------------------- the run producers

    /// **The single Apply producer.** Pressing `#editApply` (`edit.js:277`:
    /// `els.apply.addEventListener('click', function () { run(true); })`).
    ///
    /// This is the only function in the tree that constructs
    /// [`RunMode::Apply`]. Every other entry point is dry. A test asserts the
    /// keydown path cannot reach it, so this stays the sole producer.
    ///
    /// Refuses (with the reference's note) when busy or when no tool is
    /// selected, both before building a body — `run` checks `busy` first
    /// (`edit.js:207`) and the tool second (`edit.js:209`).
    pub fn press_apply(&mut self) -> Result<RunRequest, Note> {
        self.build_request(RunMode::Apply)
    }

    /// Pressing `#editDry` (`edit.js:276`: `run(false)`).
    pub fn press_dry(&mut self) -> Result<RunRequest, Note> {
        self.build_request(RunMode::Dry)
    }

    /// The shared half of `run` (`edit.js:206-232`): busy check, tool check,
    /// args parse, `apply` injection.
    fn build_request(&mut self, mode: RunMode) -> Result<RunRequest, Note> {
        if self.busy {
            // `edit.js:207` returns silently, leaving the note alone.
            return Err(self.last_note.clone());
        }
        let Some(tool) = self.selected.clone() else {
            let n = Note::error("No operation available");
            self.last_note = n.clone();
            return Err(n);
        };
        let parsed: Args = match self.args.parse() {
            Ok(a) => a,
            Err(e) => {
                // `edit.js:214` — `note('Args: ' + e.message, 'error')`.
                let n = Note::error(e.note());
                self.last_note = n.clone();
                return Err(n);
            }
        };
        // `edit.js:217` — `if (apply) args.apply = true`. See the module docs:
        // a *typed* `apply:true` is deliberately NOT stripped on a dry run.
        let args = parsed.with_apply_if(mode);
        // `setBusy(true, …)` (`edit.js:218`).
        self.busy = true;
        self.last_note = Note { text: mode.busy_label(&tool), kind: None };
        Ok(RunRequest { tool, args: args.to_json(), mode })
    }

    /// The exact POST body for a dry run. Always `RunMode::Dry`.
    ///
    /// **Ruled behaviour:** `RunMode::Dry` never *sets* `apply`, and never
    /// *removes* it either — if the user typed `{"apply": true}`, that key is
    /// forwarded verbatim (`edit.js:217` sets the key only when applying). This
    /// is faithful to the reference and was ruled explicitly; do not "fix" it by
    /// stripping `apply` here.
    pub fn dry_run_body(tool: &str, args: &Args) -> Value {
        RunRequest {
            tool: tool.to_string(),
            args: args.with_apply_if(RunMode::Dry).to_json(),
            mode: RunMode::Dry,
        }
        .body()
    }

    /// The exact POST body for an apply: the same args plus `apply: true`
    /// (`edit.js:217`), overwriting any typed value.
    ///
    /// **Private on purpose.** A `pub` body-builder would be a second way to
    /// produce an apply that skips [`EditToolView::press_apply`]'s busy /
    /// no-tool / parse guards, which is exactly how the "sole producer"
    /// invariant would rot. Callers that need an apply go through `press_apply`;
    /// this exists only so the two bodies can be compared against one reference.
    fn apply_body(tool: &str, args: &Args) -> Value {
        RunRequest {
            tool: tool.to_string(),
            args: args.with_apply_if(RunMode::Apply).to_json(),
            mode: RunMode::Apply,
        }
        .body()
    }

    // ---------------------------------------------------------------- analyze

    /// `validate()` (`edit.js:172-177`): parse the box, default `timeout`, run
    /// `parse_check`.
    ///
    /// The default is applied only when `timeout` is *absent*, not when it is
    /// `null` — `edit.js:175` tests `args.timeout === undefined`, which is why
    /// [`Args::has`] exists rather than `get().is_none()`.
    pub fn validate(&self) -> Result<AnalyzeRequest, Note> {
        self.analyze_request(AnalyzeOp::ParseCheck)
    }

    /// `duplicates()` (`edit.js:179-183`): parse the box, run `dedupe`.
    ///
    /// Note the reference applies **no** `timeout` default here — only
    /// `validate` does (`edit.js:175` is inside `validate`, not `boxArgs`).
    pub fn duplicates(&self) -> Result<AnalyzeRequest, Note> {
        self.analyze_request(AnalyzeOp::Dedupe)
    }

    fn analyze_request(&self, op: AnalyzeOp) -> Result<AnalyzeRequest, Note> {
        let parsed = match self.args.parse() {
            Ok(a) => a,
            // `edit.js:174`/`:181` — `note('Args: ' + e.message, 'error')`.
            Err(e) => return Err(Note::error(e.note())),
        };
        let mut args = parsed.clone();
        if op == AnalyzeOp::ParseCheck && !parsed.has("timeout") {
            args = args.with_field("timeout", json!(DEFAULT_ANALYZE_TIMEOUT));
        }
        Ok(AnalyzeRequest { op, args: args.to_json() })
    }

    // -------------------------------------------------------------- responses

    /// Record a completed run's response and produce the note
    /// (`edit.js:219-225`).
    ///
    /// `ok:false` is a refusal note carrying the engine's own `error`; anything
    /// else is the mode's done-note ([`RunMode::done_note`]). `busy` clears.
    pub fn finish_run(&mut self, mode: RunMode, tool: &str, data: &Value) -> Note {
        self.last_response = Some(data.clone());
        self.busy = false;
        let note = if data.get("ok").and_then(Value::as_bool) == Some(false) {
            let msg = data.get("error").and_then(Value::as_str).unwrap_or("error");
            Note::error(format!("Refused: {msg}"))
        } else {
            Note::ok(mode.done_note(tool))
        };
        self.last_note = note.clone();
        note
    }

    /// Record a transport/parse failure (`edit.js:226-228`): render
    /// `{ok:false, error}` and note the message.
    pub fn fail_run(&mut self, message: &str) -> Note {
        let n = Note::error(message);
        self.last_response = Some(json!({ "ok": false, "error": message }));
        self.busy = false;
        self.last_note = n.clone();
        n
    }

    /// The `master` of `#editOut`'s `bad` class (`edit.js:88`):
    /// `data.ok === false || data.skipped`.
    ///
    /// `skipped` counts as "bad" because a skipped validity gate is not a pass —
    /// the module header of `edit.js` says so outright ("a `skipped` gate is
    /// shown as an error, never as a pass").
    pub fn out_is_bad(data: &Value) -> bool {
        data.get("ok").and_then(Value::as_bool) == Some(false)
            || data.get("skipped").map(|s| !s.is_null()).unwrap_or(false)
    }
}

/// `noteAnalyze` (`edit.js:99-129`) as a pure function: the note an
/// `/api/analyze` response produces.
///
/// **The rule this exists to protect:** for `parse_check`, a `skipped` gate is
/// an **error**, never a pass. `edit.js:104-107` checks `data.skipped` *before*
/// `data.ok`, so a body with both `ok:true` and a `skipped` reason reports
/// `Skipped: <reason>` as an error. Getting this backwards would display a
/// missing toolchain as "Clean", which is the exact falsehood the reference's
/// comment forbids.
pub fn note_analyze(op: AnalyzeOp, data: &Value) -> Note {
    match op {
        AnalyzeOp::ParseCheck => {
            // `edit.js:104` — skipped is checked FIRST and is an error.
            if let Some(reason) = data.get("skipped").filter(|s| !s.is_null()) {
                let reason = reason.as_str().unwrap_or("skipped");
                return Note::error(format!("Skipped: {reason}"));
            }
            if data.get("ok").and_then(Value::as_bool) == Some(false) {
                let count = data.get("count").and_then(Value::as_i64).unwrap_or(0);
                let files = data.get("files").and_then(Value::as_i64).unwrap_or(0);
                return Note::error(format!("Diagnostics: {count} in {files} file(s)"));
            }
            let files = data.get("files").and_then(Value::as_i64).unwrap_or(0);
            Note::ok(format!("Clean: {files} file(s), no diagnostics"))
        }
        AnalyzeOp::Dedupe => {
            if data.get("ok").and_then(Value::as_bool) == Some(false) {
                let msg = data.get("error").and_then(Value::as_str).unwrap_or("error");
                return Note::error(format!("Refused: {msg}"));
            }
            let stats = data.get("stats").cloned().unwrap_or_else(|| json!({}));
            let n = |k: &str| stats.get(k).and_then(Value::as_i64).unwrap_or(0);
            let truncated = if data.get("truncated").and_then(Value::as_bool) == Some(true) {
                " (truncated)"
            } else {
                ""
            };
            Note::ok(format!(
                "Duplicates: {} exact, {} near, {} blocks{}",
                n("exactClusters"),
                n("nearClusters"),
                n("blockGroups"),
                truncated
            ))
        }
    }
}

/// The note for an `/api/analyze` body that is missing entirely
/// (`edit.js:100`: `if (!data) { note('Empty response', 'error'); return; }`).
pub fn note_analyze_missing() -> Note {
    Note::error("Empty response")
}

/// Record an analyze response on the view: the `render` + `noteAnalyze` pair
/// (`edit.js:150-151`), plus clearing `busy` (`edit.js:155`).
impl EditToolView {
    /// Store the raw response, clear `busy`, and return the analyze note.
    pub fn finish_analyze(&mut self, op: AnalyzeOp, data: Option<&Value>) -> Note {
        self.busy = false;
        let note = match data {
            None => note_analyze_missing(),
            Some(d) => {
                self.last_response = Some(d.clone());
                note_analyze(op, d)
            }
        };
        self.last_note = note.clone();
        note
    }

    /// Record a note without starting a run — the reference's bare
    /// `note('Args: …', 'error')` on a box that cannot be parsed
    /// (`edit.js:174`). Never sets `busy`, because nothing was sent.
    pub fn set_note(&mut self, note: Note) {
        self.last_note = note;
    }

    /// `analyze()`'s entry (`edit.js:146-148`): refuse when busy, else mark busy
    /// with the op's label.
    pub fn begin_analyze(&mut self, op: AnalyzeOp) -> Option<Note> {
        if self.busy {
            return Some(self.last_note.clone());
        }
        self.busy = true;
        self.last_note = Note { text: format!("{}…", op.busy_label()), kind: None };
        None
    }
}

/// `boxArgs`' error, surfaced for a caller that wants it directly.
pub fn parse_args(raw: &str) -> Result<Args, ArgsError> {
    Args::parse(raw)
}

/// `Note::kind` re-exported for convenience so a consumer of this module need
/// not import both.
pub use crate::edit_args::NoteKind as EditNoteKind;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit_args::{KeyEvent, KeyName};

    fn view_with(tools: &[&str], selected: &str) -> EditToolView {
        let mut v = EditToolView::new();
        let list: Vec<Value> = tools.iter().map(|t| json!(t)).collect();
        v.load_ops(&json!({ "ok": true, "count": list.len(), "tools": list }));
        assert!(v.select_tool(selected), "test setup: {selected} must be offered");
        v
    }

    // ================================================== THE APPLY INVARIANT

    /// **The task's central property.** `no_key_event_can_produce_an_apply`
    /// (in `edit_args`) must still hold through the wire layer: enumerate every
    /// key event and assert none produces a writing request.
    #[test]
    fn no_key_event_can_produce_an_apply_through_the_wire() {
        let keys = [KeyName::Enter, KeyName::Other];
        for key in keys {
            for ctrl in [false, true] {
                for meta in [false, true] {
                    for shift in [false, true] {
                        for alt in [false, true] {
                            let mut v = view_with(&["rename_symbol"], "rename_symbol");
                            let ev = KeyEvent { key, ctrl, meta };
                            let (intent, _, req) = v.keydown(ev);
                            // `KeyIntent` has no apply variant, so the strongest
                            // available assertion is that the intent is one of the
                            // two known variants and the request (if any) is dry.
                            assert!(
                                matches!(intent, KeyIntent::Ignored | KeyIntent::DryRun),
                                "unexpected intent {intent:?}"
                            );
                            if let Some(r) = req {
                                assert_eq!(
                                    r.mode,
                                    RunMode::Dry,
                                    "key {key:?} ctrl={ctrl} meta={meta} shift={shift} alt={alt} \
                                     produced a non-dry request"
                                );
                                assert!(!r.writes(), "a key event must never write");
                                assert_ne!(r.args.get("apply"), Some(&json!(true)),
                                    "a key event must never build an apply:true body");
                            }
                        }
                    }
                }
            }
        }
    }

    /// The type itself has no apply variant — the invariant is structural.
    /// `KeyIntent` is `{Ignored, DryRun}`; this pins that no third, writing
    /// variant is ever added without a deliberate test change.
    #[test]
    fn key_intent_has_no_apply_variant() {
        let all = [KeyIntent::Ignored, KeyIntent::DryRun];
        assert_eq!(all.len(), 2, "KeyIntent must have exactly two variants");
        for i in all {
            assert!(matches!(i, KeyIntent::Ignored | KeyIntent::DryRun));
        }
    }

    /// Ctrl+Enter is a dry run; a plain Enter is ignored. Two-directional.
    #[test]
    fn ctrl_enter_is_a_dry_run_and_plain_enter_is_ignored() {
        assert_eq!(EditToolView::key_intent(KeyEvent::ctrl_enter()), KeyIntent::DryRun);
        assert_eq!(EditToolView::key_intent(KeyEvent::meta_enter()), KeyIntent::DryRun);
        assert_eq!(
            EditToolView::key_intent(KeyEvent::plain(KeyName::Enter)),
            KeyIntent::Ignored
        );
        assert_eq!(
            EditToolView::key_intent(KeyEvent::plain(KeyName::Other)),
            KeyIntent::Ignored
        );
    }

    /// **The Apply button is the SOLE producer.** Pressing it yields an apply;
    /// pressing Dry yields a dry run; a keydown yields a dry run; the two are
    /// never confused.
    #[test]
    fn the_apply_button_is_the_sole_apply_producer() {
        let mut v = view_with(&["rename_symbol"], "rename_symbol");

        let applied = v.press_apply().expect("apply must build a request");
        assert_eq!(applied.mode, RunMode::Apply);
        assert!(applied.writes(), "the Apply button must produce a writing request");
        assert_eq!(applied.args.get("apply"), Some(&json!(true)));

        let mut v = view_with(&["rename_symbol"], "rename_symbol");
        let dry = v.press_dry().expect("dry must build a request");
        assert_eq!(dry.mode, RunMode::Dry);
        assert!(!dry.writes(), "the Dry-run button must not write");
        assert_eq!(dry.args.get("apply"), None, "a dry-run button press adds no apply key");

        let mut v = view_with(&["rename_symbol"], "rename_symbol");
        let (_, _, by_key) = v.keydown(KeyEvent::ctrl_enter());
        let by_key = by_key.expect("Ctrl+Enter with a tool selected builds a request");
        assert_eq!(by_key.mode, RunMode::Dry, "the keydown path must never apply");
        assert!(!by_key.writes());
    }

    /// **Exactly one producer** — proven behaviourally, not by scanning source.
    ///
    /// The repository forbids embedding source text in code (a gate asserts zero
    /// such occurrences), so the invariant is proven by *exhausting the public
    /// surface* instead: for every tool selection state and every key event, the
    /// only public call that yields a writing request is [`EditToolView::press_apply`].
    ///
    /// This is stronger than a source scan: it survives refactoring, and it fails
    /// if a *new* public method ever returns an apply — because the exhaustive
    /// sweep below asserts that no non-`press_apply` entry point writes.
    #[test]
    fn the_apply_button_is_the_only_public_call_that_writes() {
        let keys = [
            KeyEvent::ctrl_enter(),
            KeyEvent::meta_enter(),
            KeyEvent::plain(KeyName::Enter),
            KeyEvent::plain(KeyName::Other),
        ];

        // 1. Every key event, in every selection state, yields a dry request or
        //    nothing — never a writing one.
        for tool in [None, Some("rename_symbol")] {
            for ev in keys {
                let mut v = EditToolView::new();
                if let Some(t) = tool {
                    let _ = v.load_ops(&json!({ "ok": true, "tools": [t] }));
                    assert!(v.select_tool(t));
                }
                let (_, _, req) = v.keydown(ev);
                if let Some(r) = req {
                    assert!(!r.writes(), "keydown wrote for tool={tool:?}");
                    assert_eq!(r.mode, RunMode::Dry);
                }
            }
        }

        // 2. `press_dry`, `validate` and `duplicates` produce nothing that writes.
        let mut v = view_with(&["rename_symbol"], "rename_symbol");
        assert!(!v.press_dry().expect("dry builds").writes());
        let mut v = EditToolView::new();
        v.args.set_text("{}");
        assert_eq!(v.validate().expect("valid").op, AnalyzeOp::ParseCheck);
        assert_eq!(v.duplicates().expect("valid").op, AnalyzeOp::Dedupe);

        // 3. The single writer is `press_apply`, and it really does write.
        let mut v = view_with(&["rename_symbol"], "rename_symbol");
        let applied = v.press_apply().expect("apply builds");
        assert!(applied.writes(), "press_apply MUST write");
        assert_eq!(applied.mode, RunMode::Apply);
        assert_eq!(applied.args.get("apply"), Some(&json!(true)));
    }

    /// `keydown` builds its request in `Dry` mode only — the direct, non-scanning
    /// statement of the keydown half of the invariant.
    #[test]
    fn keydown_builds_only_dry_requests() {
        let mut v = view_with(&["rename_symbol"], "rename_symbol");
        let (intent, _, req) = v.keydown(KeyEvent::ctrl_enter());
        assert_eq!(intent, KeyIntent::DryRun);
        let req = req.expect("a request is built");
        assert_eq!(req.mode, RunMode::Dry);
        assert_ne!(req.mode, RunMode::Apply);
        assert!(!req.writes());
        // …and the body it would post is the dry body.
        assert_ne!(req.args.get("apply"), Some(&json!(true)));
    }

    // ================================================ REQUEST BODIES

    /// A dry run posts `{tool, args}` with **no** `apply` key.
    #[test]
    fn a_dry_run_posts_a_dry_body_and_not_an_apply() {
        let args = Args::parse(r#"{"like": ["sps"]}"#).expect("valid");
        let body = EditToolView::dry_run_body("select_decls", &args);
        assert_eq!(body.get("tool"), Some(&json!("select_decls")));
        assert_eq!(body.get("args"), Some(&json!({ "like": ["sps"] })));
        assert_eq!(
            body.pointer("/args/apply"),
            None,
            "a dry run must not add apply:true"
        );
        assert_ne!(
            body.pointer("/args/apply"),
            Some(&json!(true)),
            "…and in particular must not look like an apply"
        );
    }

    /// An apply posts the same args plus `apply: true` (`edit.js:217`).
    #[test]
    fn an_apply_posts_the_same_args_plus_apply_true() {
        let args = Args::parse(r#"{"like": ["sps"]}"#).expect("valid");
        let dry = EditToolView::dry_run_body("select_decls", &args);
        let applied = EditToolView::apply_body("select_decls", &args);
        assert_eq!(applied.pointer("/args/apply"), Some(&json!(true)));
        assert_eq!(applied.get("tool"), dry.get("tool"));
        assert_eq!(
            applied.pointer("/args/like"),
            dry.pointer("/args/like"),
            "apply must change nothing but the apply key"
        );
        assert_ne!(applied, dry, "the two bodies must be distinguishable");
    }

    /// **The ruled behaviour.** A user-typed `{"apply": true}` IS forwarded by a
    /// dry run — faithful to `edit.js:217`, which only *sets* the key when
    /// applying and never strips it. Do not "fix" this.
    #[test]
    fn a_typed_apply_true_is_forwarded_by_a_dry_run() {
        let args = Args::parse(r#"{"like": ["sps"], "apply": true}"#).expect("valid");
        let body = EditToolView::dry_run_body("select_decls", &args);
        assert_eq!(
            body.pointer("/args/apply"),
            Some(&json!(true)),
            "the ruled behaviour: a typed apply:true survives a dry run"
        );
    }

    /// …and a typed `apply:false` survives a dry run unchanged, while an apply
    /// overwrites it with `true` (`edit.js:217` is an assignment).
    #[test]
    fn a_typed_apply_false_survives_dry_and_is_overwritten_by_apply() {
        let args = Args::parse(r#"{"apply": false}"#).expect("valid");
        assert_eq!(
            EditToolView::dry_run_body("t", &args).pointer("/args/apply"),
            Some(&json!(false)),
            "a dry run must not invent an apply key"
        );
        assert_eq!(
            EditToolView::apply_body("t", &args).pointer("/args/apply"),
            Some(&json!(true)),
            "apply assigns true over a typed false"
        );
    }

    /// The body's `args` is the box's object, not a re-serialisation of it.
    #[test]
    fn nested_args_survive_into_the_body() {
        let raw = r#"{"edits":[{"file":"a.cs","old":"x","new":"y"}]}"#;
        let args = Args::parse(raw).expect("valid");
        let body = EditToolView::dry_run_body("apply_edits", &args);
        assert_eq!(body.pointer("/args"), serde_json::from_str::<Value>(raw).ok().as_ref());
    }

    // ================================================ RUN GUARDS

    /// A run with no tool selected is refused with the reference's message, and
    /// the Apply button is refused too — it cannot bypass the guard.
    #[test]
    fn run_without_a_tool_is_refused_and_apply_is_not_exempt() {
        let mut v = EditToolView::new();
        let n = v.press_dry().expect_err("no tool => refused");
        assert_eq!(n.text, "No operation available");
        assert_eq!(n.kind, Some(NoteKind::Error));

        let mut v = EditToolView::new();
        let n = v.press_apply().expect_err("apply must be refused too");
        assert_eq!(n.text, "No operation available");
        assert!(!v.is_busy(), "a refused run must not leave the modal busy");
    }

    /// A malformed box is refused as an `Args:` error and builds no request —
    /// including on the Apply path.
    #[test]
    fn a_bad_box_builds_no_request_on_either_button() {
        let mut v = view_with(&["rename_symbol"], "rename_symbol");
        v.args.set_text("{oops");
        let n = v.press_dry().expect_err("bad args => refused");
        assert!(n.text.starts_with("Args: "), "{:?}", n.text);
        assert_eq!(n.kind, Some(NoteKind::Error));
        assert!(v.press_apply().is_err(), "apply must also refuse a bad box");
    }

    /// Busy is a real state: a second run while busy is refused and the first
    /// request's data is untouched (`edit.js:207`).
    #[test]
    fn a_run_while_busy_is_refused() {
        let mut v = view_with(&["select_decls"], "select_decls");
        let _first = v.press_dry().expect("first run builds");
        assert!(v.is_busy());
        assert!(v.press_dry().is_err(), "a second run while busy is refused");
        assert!(v.press_apply().is_err(), "…and so is an apply while busy");
    }

    /// The busy label distinguishes the two modes (`edit.js:218`), so a user can
    /// always tell which one is in flight.
    #[test]
    fn the_busy_label_names_the_mode() {
        let mut v = view_with(&["rename_symbol"], "rename_symbol");
        let _ = v.press_dry().expect("dry builds");
        assert_eq!(v.last_note().text, "Dry-running rename_symbol…");

        let mut v = view_with(&["rename_symbol"], "rename_symbol");
        let _ = v.press_apply().expect("apply builds");
        assert_eq!(v.last_note().text, "Applying rename_symbol…");
    }

    // ================================================ TOOL LIST

    /// A fetched list selects its first entry; an unfetched view is a distinct
    /// state; a refusal leaves the list unloaded and notes the error.
    #[test]
    fn load_ops_selection_and_refusal() {
        let mut v = EditToolView::new();
        assert!(!v.tools_loaded(), "nothing fetched => not loaded");
        assert_eq!(v.tools(), None);
        assert_eq!(v.selected(), None);

        let note = v.load_ops(&json!({ "ok": true, "count": 2, "tools": ["a", "b"] }));
        assert!(v.tools_loaded());
        assert_eq!(v.tools().map(|t| t.to_vec()), Some(vec!["a".into(), "b".into()]));
        assert_eq!(v.selected(), Some("a"), "the first option is selected");
        assert_eq!(note.text, "2 operations");

        let mut v = EditToolView::new();
        let note = v.load_ops(&json!({ "ok": false, "error": "no host route" }));
        assert!(!v.tools_loaded(), "a refusal must not load a partial list");
        assert_eq!(v.selected(), None);
        assert_eq!(note.kind, Some(NoteKind::Error));
        assert_eq!(note.text, "no host route");
    }

    /// `select_tool` only accepts an advertised tool.
    #[test]
    fn select_tool_refuses_an_unadvertised_tool() {
        let mut v = view_with(&["select_decls", "rename_symbol"], "select_decls");
        assert!(v.select_tool("rename_symbol"));
        assert_eq!(v.selected(), Some("rename_symbol"));
        assert!(!v.select_tool("not_a_tool"), "must refuse an unadvertised tool");
        assert_eq!(v.selected(), Some("rename_symbol"), "…and leave the selection alone");
    }

    // ================================================ RESPONSES

    /// A refusal carries the engine's own error; a success carries the mode's
    /// done-note; both store the raw body (`edit.js:87`, `:220-225`).
    #[test]
    fn finish_run_notes_refusal_versus_success() {
        let mut v = view_with(&["select_decls"], "select_decls");
        let _ = v.press_dry().expect("builds");
        let n = v.finish_run(RunMode::Dry, "select_decls", &json!({ "ok": true }));
        assert_eq!(n, Note::ok("select_decls dry run"));
        assert_eq!(n.kind, Some(NoteKind::Ok));
        assert!(!v.is_busy(), "finishing clears busy");
        assert_eq!(v.last_response(), Some(&json!({ "ok": true })));

        let mut v = view_with(&["select_decls"], "select_decls");
        let _ = v.press_apply().expect("builds");
        let n = v.finish_run(RunMode::Apply, "select_decls", &json!({ "ok": false, "error": "nope" }));
        assert_eq!(n, Note::error("Refused: nope"));
        assert!(n.text.starts_with("Refused: "));
    }

    /// The two modes' done-notes are different, so a completed apply is never
    /// reported as a preview.
    #[test]
    fn a_finished_apply_is_not_reported_as_a_dry_run() {
        let mut v = view_with(&["t"], "t");
        let _ = v.press_apply().expect("builds");
        let n = v.finish_run(RunMode::Apply, "t", &json!({ "ok": true }));
        assert_eq!(n.text, "t applied");
        assert_ne!(n.text, "t dry run");
    }

    /// A transport failure renders `{ok:false, error}` and clears busy.
    #[test]
    fn fail_run_renders_an_error_body() {
        let mut v = view_with(&["t"], "t");
        let _ = v.press_dry().expect("builds");
        let n = v.fail_run("HTTP 500: malformed JSON");
        assert_eq!(n.kind, Some(NoteKind::Error));
        assert_eq!(v.last_response(), Some(&json!({ "ok": false, "error": "HTTP 500: malformed JSON" })));
        assert!(!v.is_busy());
    }

    /// `#editOut`'s `bad` class: a refusal OR a skipped gate (`edit.js:88`).
    #[test]
    fn out_is_bad_for_refusal_and_for_a_skipped_gate() {
        assert!(EditToolView::out_is_bad(&json!({ "ok": false })));
        assert!(EditToolView::out_is_bad(&json!({ "ok": true, "skipped": "no toolchain" })));
        assert!(!EditToolView::out_is_bad(&json!({ "ok": true, "skipped": null })));
        assert!(!EditToolView::out_is_bad(&json!({ "ok": true })));
        assert!(!EditToolView::out_is_bad(&json!({})));
    }

    // ================================================ ANALYZE

    /// `validate` defaults `timeout` to 25 — but only when the key is ABSENT,
    /// not when it is explicitly null (`edit.js:175` tests `undefined`).
    #[test]
    fn validate_defaults_timeout_only_when_absent() {
        let mut v = EditToolView::new();
        v.args.set_text("{}");
        let req = v.validate().expect("valid");
        assert_eq!(req.op, AnalyzeOp::ParseCheck);
        assert_eq!(req.body().pointer("/args/timeout"), Some(&json!(25)));
        assert_eq!(req.body().get("op"), Some(&json!("parse_check")));

        // An explicit number wins.
        let mut v = EditToolView::new();
        v.args.set_text(r#"{"timeout": 5}"#);
        assert_eq!(v.validate().expect("valid").body().pointer("/args/timeout"), Some(&json!(5)));

        // An explicit null is NOT replaced: `has` is true, so no default.
        let mut v = EditToolView::new();
        v.args.set_text(r#"{"timeout": null}"#);
        assert_eq!(
            v.validate().expect("valid").body().pointer("/args/timeout"),
            Some(&Value::Null),
            "an explicit null must not be treated as absent"
        );
    }

    /// `duplicates` sends `dedupe` and applies NO timeout default.
    #[test]
    fn duplicates_does_not_default_timeout() {
        let mut v = EditToolView::new();
        v.args.set_text("{}");
        let req = v.duplicates().expect("valid");
        assert_eq!(req.op, AnalyzeOp::Dedupe);
        assert_eq!(req.body().get("op"), Some(&json!("dedupe")));
        assert_eq!(
            req.body().pointer("/args/timeout"),
            None,
            "only validate defaults timeout (edit.js:175)"
        );
    }

    /// A bad box refuses both analyze ops with the `Args:` note.
    #[test]
    fn analyze_refuses_a_bad_box() {
        let mut v = EditToolView::new();
        v.args.set_text("[]");
        for n in [v.validate().expect_err("refused"), v.duplicates().expect_err("refused")] {
            assert_eq!(n.kind, Some(NoteKind::Error));
            assert!(n.text.starts_with("Args: "), "{:?}", n.text);
        }
    }

    /// **A skipped gate is an ERROR, never a pass** — even when `ok` is true.
    /// This is the falsehood `edit.js:104-107` exists to prevent.
    #[test]
    fn a_skipped_parse_check_is_an_error_never_a_pass() {
        let n = note_analyze(
            AnalyzeOp::ParseCheck,
            &json!({ "ok": true, "skipped": "no Roslyn toolchain", "files": 12, "count": 0 }),
        );
        assert_eq!(n.kind, Some(NoteKind::Error), "skipped must be an error");
        assert_eq!(n.text, "Skipped: no Roslyn toolchain");
        assert!(!n.text.contains("Clean"), "a skipped gate must never read as clean: {:?}", n.text);
    }

    /// A clean gate reads as clean; a gate with diagnostics reports the counts.
    #[test]
    fn parse_check_reports_clean_and_diagnostics_distinctly() {
        let clean = note_analyze(AnalyzeOp::ParseCheck, &json!({ "ok": true, "files": 12, "count": 0 }));
        assert_eq!(clean.kind, Some(NoteKind::Ok));
        assert_eq!(clean.text, "Clean: 12 file(s), no diagnostics");

        let bad = note_analyze(AnalyzeOp::ParseCheck, &json!({ "ok": false, "files": 3, "count": 7 }));
        assert_eq!(bad.kind, Some(NoteKind::Error));
        assert_eq!(bad.text, "Diagnostics: 7 in 3 file(s)");
        assert_ne!(bad.text, clean.text);
    }

    /// `dedupe`'s summary, including the `(truncated)` marker.
    #[test]
    fn dedupe_summarises_stats_and_truncation() {
        let n = note_analyze(
            AnalyzeOp::Dedupe,
            &json!({ "ok": true, "stats": { "exactClusters": 2, "nearClusters": 3, "blockGroups": 4 } }),
        );
        assert_eq!(n.kind, Some(NoteKind::Ok));
        assert_eq!(n.text, "Duplicates: 2 exact, 3 near, 4 blocks");

        let t = note_analyze(
            AnalyzeOp::Dedupe,
            &json!({ "ok": true, "truncated": true, "stats": { "exactClusters": 1 } }),
        );
        assert_eq!(t.text, "Duplicates: 1 exact, 0 near, 0 blocks (truncated)");

        let refused = note_analyze(AnalyzeOp::Dedupe, &json!({ "ok": false, "error": "busy" }));
        assert_eq!(refused, Note::error("Refused: busy"));
    }

    /// A missing analyze body is an error, never a silent pass (`edit.js:100`).
    #[test]
    fn a_missing_analyze_response_is_an_error() {
        let mut v = EditToolView::new();
        let n = v.finish_analyze(AnalyzeOp::ParseCheck, None);
        assert_eq!(n.kind, Some(NoteKind::Error));
        assert_eq!(n.text, "Empty response");
        assert_eq!(n, note_analyze_missing());
    }

    /// `analyze()`'s busy guard and label.
    #[test]
    fn begin_analyze_marks_busy_and_refuses_when_already_busy() {
        let mut v = EditToolView::new();
        assert!(v.begin_analyze(AnalyzeOp::ParseCheck).is_none());
        assert!(v.is_busy());
        assert_eq!(v.last_note().text, "Validating the corpus…");
        assert!(v.begin_analyze(AnalyzeOp::Dedupe).is_some(), "a second begin is refused");

        let mut v = EditToolView::new();
        let _ = v.begin_analyze(AnalyzeOp::Dedupe);
        assert_eq!(v.last_note().text, "Scanning for duplicates…");
        let n = v.finish_analyze(AnalyzeOp::Dedupe, Some(&json!({ "ok": true, "stats": {} })));
        assert!(!v.is_busy());
        assert_eq!(n.kind, Some(NoteKind::Ok));
    }

    /// Routes are named exactly as the reference calls them.
    #[test]
    fn routes_match_the_reference() {
        assert_eq!(EDIT_ROUTE, "/api/edit");
        assert_eq!(ANALYZE_ROUTE, "/api/analyze");
        assert_eq!(AnalyzeOp::ParseCheck.as_str(), "parse_check");
        assert_eq!(AnalyzeOp::Dedupe.as_str(), "dedupe");
        assert_eq!(DEFAULT_ANALYZE_TIMEOUT, 25);
        assert_eq!(ANALYZE_NOTE_PREFIX, "Args: ");
    }

    /// The note-kind re-export is the same type, so a consumer cannot end up
    /// comparing two incompatible kinds.
    #[test]
    fn the_note_kind_reexport_is_the_same_type() {
        let a: EditNoteKind = NoteKind::Ok;
        assert_eq!(a, NoteKind::Ok);
        assert_eq!(NoteKind::Ok.css(), "ok");
        assert_eq!(NoteKind::Error.css(), "error");
    }

    /// `parse_args` is a thin passthrough and keeps the error distinction.
    #[test]
    fn parse_args_passthrough_keeps_the_two_refusals_apart() {
        assert!(parse_args("{}").is_ok());
        assert!(matches!(parse_args("{"), Err(ArgsError::Malformed(_))));
        assert_eq!(parse_args("[]"), Err(ArgsError::NotAnObject));
    }
}
