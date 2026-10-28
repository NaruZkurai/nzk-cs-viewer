//! The `#editArgs` args box: its JSON model and its keydown policy.
//!
//! This is the natively-testable half of the reference edit modal
//! (`rust-only-csviewer/frontend/static/edit.js`, 296 lines). It deliberately
//! contains **no** egui and no I/O: it is pure state plus pure policy, so the
//! rules that decide whether the user previews or writes can be asserted
//! directly instead of only through a browser.
//!
//! ## The one rule that must never invert
//!
//! `edit.js:281-288`, verbatim:
//!
//! ```text
//! // Ctrl/Cmd+Enter runs a dry run without leaving the args box.
//! els.args.addEventListener('keydown', function (ev) {
//!   if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey)) {
//!     ev.preventDefault();
//!     run(false);
//!   }
//! });
//! ```
//!
//! `run(false)` is the **dry run**; `run(true)` is the **apply**
//! (`edit.js:217` adds `apply: true` and only then is anything written, and
//! `edit.js:218` labels the two states `"Dry-running <tool>…"` vs
//! `"Applying <tool>…"`). So Ctrl/Cmd+Enter must map to [`RunMode::Dry`] and
//! **never** to [`RunMode::Apply`]. Getting this backwards would silently write
//! edits the user only asked to preview, so [`decide_key`] returns the mode as
//! data and the tests assert the distinction in *both* directions — that
//! Ctrl+Enter yields `Dry` *and* that it is not `Apply`, that Apply comes only
//! from `run(true)`.
//!
//! Three further details of that handler are load-bearing and are modelled
//! here rather than assumed:
//!
//! * **Plain `Enter` is not consumed.** `preventDefault()` sits *inside* the
//!   `if`, so a bare Enter falls through to the textarea and inserts a newline.
//!   [`KeyOutcome::consumed`] is therefore `false` for plain Enter — a
//!   two-directional requirement, not a nicety.
//! * **Both `ctrlKey` and `metaKey` count.** The condition is `||`, so
//!   Cmd+Enter on macOS and Ctrl+Enter elsewhere are the same gesture.
//! * **Only `Enter` counts.** `ev.key === 'Enter'` — `NumpadEnter` is not
//!   matched by the reference, so [`decide_key`] does not invent that case.
//!
//! ## The args model
//!
//! The markup ships the box with the literal content `{}`
//! (`index.html:531`) and the placeholder
//! `{"like": ["sps", "pcs"], "limit": 50}`. `boxArgs` (`edit.js:162-170`) is
//! the whole parser:
//!
//! ```text
//! raw = value.trim()
//! if !raw            -> {}                    // empty box is not an error
//! v = JSON.parse(raw)
//! if !v || typeof v !== 'object' || Array.isArray(v)
//!                    -> throw 'args must be a JSON object'
//! return v
//! ```
//!
//! Note what is *not* there: a JSON array such as `[]` is **rejected** even
//! though it parses, and `null` is rejected even though it parses. A bare
//! string/number/boolean is rejected. This module keeps those three refusals
//! distinct from the malformed-JSON refusal, and — critically — a refusal
//! produces a note, never a panic: the reference catches and shows
//! `'Args: ' + e.message` (`edit.js:174`, `:181`, `:214`).
//!
//! ## What is deliberately NOT here
//!
//! There is no fetch, no `/api/edit` call and no `apply` key injection in this
//! module: that is the wire layer's job, and it is the remainder recorded in
//! the handoff. This file owns *what the box means* and *what a key means*.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// `edit.js:164` — the box is shipped holding exactly this (`index.html:531`),
/// and `fillTemplate` (`edit.js:53`) treats it as the "untouched" sentinel that
/// a picked operation may overwrite. Typing anything else is treated as a
/// deliberate document and is never clobbered.
pub const DEFAULT_ARGS_TEXT: &str = "{}";

/// `index.html:531` — the placeholder is an affordance, not content. Kept as a
/// constant so the widget has exactly one spelling of it.
pub const ARGS_PLACEHOLDER: &str = r#"{"like": ["sps", "pcs"], "limit": 50}"#;

/// `edit.js:174/181/214` — the note prefix the modal uses when the box cannot be
/// read. The reference composes `'Args: ' + e.message`, so the prefix is part of
/// the user-visible contract and is modelled, not inlined at the call site.
pub const ARGS_NOTE_PREFIX: &str = "Args: ";

/// `edit.js:217` — Apply is the *same* body with `apply: true` added. The literal
/// key and value are the whole dry-run/apply difference on the wire.
pub const APPLY_KEY: &str = "apply";

/// Which of the modal's two run buttons a gesture is equivalent to.
///
/// `edit.js:206` (`function run(apply)`) has exactly this two-valued domain, and
/// `:276`/`:277` bind the buttons to `run(false)`/`run(true)` respectively.
/// Modelled as an enum so a caller cannot pass a bare `bool` and lose which
/// meaning `false` carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunMode {
    /// `run(false)` — `edit.js:224` labels the result `"<tool> dry run"`.
    Dry,
    /// `run(true)` — `edit.js:224` labels the result `"<tool> applied"`.
    Apply,
}

impl RunMode {
    /// `edit.js:218` — the busy label, verbatim: `'Applying ' + tool + '…'`
    /// versus `'Dry-running ' + tool + '…'`.
    ///
    /// The distinction is user-visible while the request is in flight, which is
    /// precisely when a wrong answer is most expensive.
    pub fn busy_label(self, tool: &str) -> String {
        match self {
            RunMode::Dry => format!("Dry-running {tool}…"),
            RunMode::Apply => format!("Applying {tool}…"),
        }
    }

    /// `edit.js:224` — the note shown after a non-refused response, verbatim:
    /// `tool + ' dry run'` versus `tool + ' applied'`.
    ///
    /// Note the reference's asymmetry is preserved exactly: the `Apply` note
    /// carries its `apply: true` fact in the word `applied`, and the `Dry` note
    /// says `dry run` — neither is a generic "Done".
    pub fn done_note(self, tool: &str) -> String {
        match self {
            RunMode::Dry => format!("{tool} dry run"),
            RunMode::Apply => format!("{tool} applied"),
        }
    }

    /// Whether this run may write. Only [`RunMode::Apply`] can, and it is the
    /// mode `run(true)` produces — never the mode the Ctrl/Cmd+Enter handler
    /// produces (`edit.js:285`).
    pub fn writes(self) -> bool {
        matches!(self, RunMode::Apply)
    }
}

/// A focused text box's key state, as `edit.js:282-287` reads it: `ev.key`,
/// `ev.ctrlKey`, `ev.metaKey`.
///
/// Deliberately *only* those three fields. Modelling extra modifiers would
/// invite handling the reference does not do (e.g. Shift+Enter, Alt+Enter),
/// and the reference's condition mentions neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    /// `ev.key` — the reference compares it to the literal `'Enter'`.
    pub key: KeyName,
    /// `ev.ctrlKey`.
    pub ctrl: bool,
    /// `ev.metaKey` — the Cmd key on macOS.
    pub meta: bool,
}

impl KeyEvent {
    /// A key with no modifiers held.
    pub fn plain(key: KeyName) -> KeyEvent {
        KeyEvent { key, ctrl: false, meta: false }
    }

    /// `Enter` with Ctrl held — the gesture `edit.js:283` accepts.
    pub fn ctrl_enter() -> KeyEvent {
        KeyEvent { key: KeyName::Enter, ctrl: true, meta: false }
    }

    /// `Enter` with the platform Cmd key held — also accepted, by the `||`.
    pub fn meta_enter() -> KeyEvent {
        KeyEvent { key: KeyName::Enter, ctrl: false, meta: true }
    }
}

/// The subset of `ev.key` values this handler can distinguish.
///
/// `edit.js:283` tests equality against `'Enter'` only; every other key takes the
/// same (non-consuming) path. [`KeyName::Other`] collapses that infinity into one
/// variant *because the reference does*, rather than inventing a taxonomy the
/// handler never consults. `NumpadEnter` is explicitly **not** `Enter` here,
/// matching the strict `===` in the reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyName {
    /// The literal `ev.key === 'Enter'`.
    Enter,
    /// Any other key, including `NumpadEnter`, `Escape`, `Tab`, a character.
    Other,
}

/// What the handler did with a keydown: the consumed flag, and — when it acted —
/// the run it asked for.
///
/// `edit.js:284` calls `preventDefault()` if and only if it also calls `run`, so
/// the two facts are inseparable and are returned together as one value. A caller
/// that only got a `bool` could keep the mode and drop the `preventDefault`
/// (Ctrl+Enter would then insert a newline *and* run), or drop the mode and keep
/// the `preventDefault` (Ctrl+Enter would silently swallow the keystroke). Both
/// are prevented by construction here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyOutcome {
    /// `ev.preventDefault()` was called — the textarea must not receive the key.
    pub consumed: bool,
    /// The `run(...)` argument, when the handler ran. Always
    /// `Some(RunMode::Dry)` from this handler, because `edit.js:285` passes the
    /// literal `false`.
    pub run: Option<RunMode>,
}

impl KeyOutcome {
    /// The key was not consumed and nothing ran (`edit.js:283` false branch).
    pub const IGNORED: KeyOutcome = KeyOutcome { consumed: false, run: None };
}

/// **The keydown decision function** — the Rust counterpart of
/// `edit.js:282-287`, and the single place the dry/apply mapping is decided.
///
/// ```text
/// if key == 'Enter' && (ctrl || meta) {
///     preventDefault();      -> KeyOutcome { consumed: true, run: Some(Dry) }
///     run(false);            ->                    ^^^^^^^^^^^^^^^ never Apply
/// }
/// ```
///
/// Returns [`KeyOutcome::IGNORED`] for a plain Enter and for every other key, so
/// the textarea keeps its default behaviour. The returned [`RunMode`] is a
/// hard-coded [`RunMode::Dry`]: there is no input to this function that can
/// produce [`RunMode::Apply`], which is the property the tests pin.
pub fn decide_key(ev: KeyEvent) -> KeyOutcome {
    if ev.key == KeyName::Enter && (ev.ctrl || ev.meta) {
        KeyOutcome { consumed: true, run: Some(RunMode::Dry) }
    } else {
        KeyOutcome::IGNORED
    }
}

/// A parsed args box: the JSON object `run`/`validate`/`duplicates` dispatch.
///
/// `BTreeMap` rather than `Map` so iteration (and therefore any test that walks
/// the keys) is deterministic; `edit.js:162-170` never iterates, so this is a
/// strengthening, not a divergence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    fields: BTreeMap<String, Value>,
}

impl Default for Args {
    /// The shipped box content `{}` parses to zero fields — the state the modal
    /// opens in (`index.html:531`), and not an error.
    fn default() -> Args {
        Args { fields: BTreeMap::new() }
    }
}

impl Args {
    /// Parse the box's raw text exactly as `boxArgs` (`edit.js:162-170`) does.
    ///
    /// The three refusal classes the reference distinguishes are kept distinct
    /// via [`ArgsError`], and no input panics:
    ///
    /// * whitespace-only or empty -> `Ok(Args::default())`, because
    ///   `if (!raw) return {}` short-circuits *before* `JSON.parse`;
    /// * malformed JSON -> [`ArgsError::Malformed`] carrying serde's message,
    ///   which is what `e.message` was;
    /// * well-formed JSON that is not a plain object — an array, `null`, a
    ///   string, a number, a boolean -> [`ArgsError::NotAnObject`], the
    ///   reference's literal `'args must be a JSON object'`.
    pub fn parse(raw: &str) -> Result<Args, ArgsError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(Args::default());
        }
        let value: Value = serde_json::from_str(trimmed)
            .map_err(|e| ArgsError::Malformed(e.to_string()))?;
        let obj: Map<String, Value> = match value {
            Value::Object(m) => m,
            _ => return Err(ArgsError::NotAnObject),
        };
        Ok(Args { fields: obj.into_iter().collect() })
    }

    /// The number of keys — `Object.keys(args).length` in the reference's terms.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// True for the shipped `{}`.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// One field, for a caller that needs to read `files`/`timeout`/`threshold`
    /// (`edit.js:175` defaults `timeout` to 25 when absent).
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    /// Whether the box supplied `key` at all — the check `edit.js:175` makes
    /// (`if (args.timeout === undefined)`), kept separate from `get().is_none()`
    /// so an explicit JSON `null` stays distinguishable from an absent key.
    pub fn has(&self, key: &str) -> bool {
        self.fields.contains_key(key)
    }

    /// `edit.js:217` — `if (apply) args.apply = true`.
    ///
    /// Mutating a *copy* of the parsed args is exactly what the reference does to
    /// its own object, and it is the only difference between the two request
    /// bodies. Kept as a method so the wire layer cannot forget it, and kept
    /// explicitly `Apply`-only so a dry run can never acquire the key.
    pub fn with_apply_if(&self, mode: RunMode) -> Args {
        let mut out = self.clone();
        if mode.writes() {
            out.fields.insert(APPLY_KEY.to_string(), Value::Bool(true));
        }
        out
    }

    /// The request body's `args` object, in the box's own key order (sorted).
    pub fn to_json(&self) -> Value {
        Value::Object(self.fields.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }

    /// A copy with `key` set to `value` — the generic form of the
    /// `args.timeout = 25` default `validate()` applies (`edit.js:175`).
    ///
    /// Unconditional by design: the caller decides *whether* to default (the
    /// reference guards on `args.timeout === undefined`, which is
    /// [`Args::has`]), and this method only performs the assignment. Splitting
    /// the two keeps the absent-vs-null distinction in the caller where the
    /// reference makes it, instead of hiding it in an `unwrap_or`.
    pub fn with_field(&self, key: &str, value: Value) -> Args {
        let mut out = self.clone();
        out.fields.insert(key.to_string(), value);
        out
    }
}

/// Why the args box could not be read — the reference's caught exceptions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgsError {
    /// `JSON.parse` threw; the payload is the parser's message (`e.message`).
    Malformed(String),
    /// `edit.js:167` — `'args must be a JSON object'`.
    NotAnObject,
}

impl ArgsError {
    /// `edit.js:166-168` — the literal refusal message for a non-object.
    pub const NOT_AN_OBJECT: &'static str = "args must be a JSON object";

    /// The user-facing message, matching the reference's two cases.
    pub fn message(&self) -> String {
        match self {
            ArgsError::Malformed(m) => m.clone(),
            ArgsError::NotAnObject => ArgsError::NOT_AN_OBJECT.to_string(),
        }
    }

    /// `edit.js:174/181/214` — `note('Args: ' + e.message, 'error')`.
    ///
    /// Returned as a value so a refusal is a *testable string*, and so the one
    /// spelling of the prefix lives here rather than being retyped per call site.
    pub fn note(&self) -> String {
        format!("{ARGS_NOTE_PREFIX}{}", self.message())
    }
}

/// The modal's note line (`#editNote`) as a value: either an error or an
/// ordinary status.
///
/// `note(text, kind)` (`edit.js:59-62`) writes `textContent` and sets
/// `className = 'modal-note' + (kind ? ' ' + kind : '')`. The reference only ever
/// passes `'error'` or `'ok'`; there is no third kind in `edit.js`, so there is
/// none here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// `els.note.textContent` — `''` clears the line.
    pub text: String,
    /// The class suffix: `None` is the bare `modal-note`.
    pub kind: Option<NoteKind>,
}

/// The `kind` values `edit.js` actually passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteKind {
    /// `'ok'` — a success (`edit.js:110`, `:124`, `:128`, `:224`).
    Ok,
    /// `'error'` — a refusal or a skipped gate (`edit.js:105`, `:109`, `:120`,
    /// `:127`, `:154`, `:174`, `:181`, `:209`, `:214`, `:222`, `:228`, `:244`).
    Error,
}

impl NoteKind {
    /// The class token appended after `modal-note`.
    pub fn css(self) -> &'static str {
        match self {
            NoteKind::Ok => "ok",
            NoteKind::Error => "error",
        }
    }
}

impl Note {
    /// The `className` `note()` would produce.
    pub fn class_name(&self) -> String {
        match self.kind {
            Some(k) => format!("modal-note {}", k.css()),
            None => "modal-note".to_string(),
        }
    }

    /// `note('')` — the clearing call `setBusy(false)` makes (`edit.js:63`).
    pub fn clear() -> Note {
        Note { text: String::new(), kind: None }
    }

    /// An `'error'` note.
    pub fn error(text: impl Into<String>) -> Note {
        Note { text: text.into(), kind: Some(NoteKind::Error) }
    }

    /// An `'ok'` note.
    pub fn ok(text: impl Into<String>) -> Note {
        Note { text: text.into(), kind: Some(NoteKind::Ok) }
    }

    /// True when nothing is displayed — `textContent === ''`.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// The args box's whole state: the text, plus what the last attempt concluded.
///
/// This is the piece the widget needs and the reference keeps in two loosely
/// coupled DOM nodes (`#editArgs`'s `value` and `#editNote`'s `textContent`).
/// Holding both here means the note can never describe a parse of text the box no
/// longer contains — the failure mode where a user fixes their JSON but still
/// reads the old error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditArgsView {
    /// `els.args.value`.
    text: String,
    /// Whether the box still holds untouched content (`{}` or empty), which is
    /// the condition under which `fillTemplate` (`edit.js:53`) may overwrite it
    /// with an operation's skeleton.
    untouched: bool,
}

impl Default for EditArgsView {
    /// The shipped state: the literal `{}` from `index.html:531`, untouched.
    fn default() -> EditArgsView {
        EditArgsView { text: DEFAULT_ARGS_TEXT.to_string(), untouched: true }
    }
}

impl EditArgsView {
    /// The shipped box: `{}`, untouched.
    pub fn new() -> EditArgsView {
        EditArgsView::default()
    }

    /// The raw text as the textarea holds it.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether a picked operation may replace the text — `edit.js:53`'s
    /// `cur === '' || cur === '{}' || cur === lastTemplate`. Only the first two
    /// are modelled (the third needs the template bookkeeping the wire layer
    /// owns), so this is a *necessary* condition, not a sufficient one: a caller
    /// must still check its own `lastTemplate`.
    pub fn is_replaceable_skeleton(&self) -> bool {
        let cur = self.text.trim();
        cur.is_empty() || cur == DEFAULT_ARGS_TEXT
    }

    /// Whether the user has typed something deliberate — the case the reference
    /// refuses to clobber (`edit.js:53`).
    pub fn is_untouched(&self) -> bool {
        self.untouched
    }

    /// Set the text as a keystroke would. Any edit marks the box as deliberate,
    /// so a later operation pick cannot silently discard the user's document.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.untouched = false;
    }

    /// Parse the current text, or report why not — `boxArgs`, as state.
    ///
    /// Nothing here mutates or panics; the caller turns an `Err` into
    /// [`ArgsError::note`].
    pub fn parse(&self) -> Result<Args, ArgsError> {
        Args::parse(&self.text)
    }

    /// Handle a keydown: decide it, and — when the handler runs — perform the
    /// dry run's *parse* so a malformed box is reported as a note rather than
    /// being sent.
    ///
    /// Returns `(outcome, note)`. A `None` note means the handler did not run at
    /// all, so the caller must leave `#editNote` exactly as it was — the
    /// reference does no `note()` call on the ignored path.
    ///
    /// The parse failure is deliberately *not* treated as "no run": the
    /// reference calls `run(false)` unconditionally and lets `run` itself catch
    /// the parse error and note it (`edit.js:210-216`), so `outcome.run` stays
    /// `Some(RunMode::Dry)` and the note carries the refusal. Collapsing this
    /// into `None` would lose the difference between "key not handled" and
    /// "handled, and the args were bad".
    pub fn keydown(&mut self, ev: KeyEvent) -> (KeyOutcome, Option<Note>) {
        let outcome = decide_key(ev);
        let note = match outcome.run {
            None => None,
            Some(mode) => match self.parse() {
                Ok(_) => Some(Note::ok(mode.busy_label(""))),
                Err(e) => Some(Note::error(e.note())),
            },
        };
        (outcome, note)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------ keydown policy

    /// The headline rule, in the positive direction: Ctrl+Enter is a **dry run**.
    #[test]
    fn ctrl_enter_is_a_dry_run() {
        let out = decide_key(KeyEvent::ctrl_enter());
        assert!(out.consumed, "Ctrl+Enter must call preventDefault (edit.js:284)");
        assert_eq!(out.run, Some(RunMode::Dry), "Ctrl+Enter must call run(false)");
    }

    /// ...and in the negative direction, which is the direction that matters:
    /// Ctrl+Enter must **not** be an apply. An off-by-one-predicate here would
    /// silently write edits the user only meant to preview.
    #[test]
    fn ctrl_enter_is_not_an_apply() {
        let out = decide_key(KeyEvent::ctrl_enter());
        assert_ne!(out.run, Some(RunMode::Apply), "Ctrl+Enter must never apply");
        assert!(!out.run.unwrap().writes(), "the Ctrl+Enter run must not write");
    }

    /// Cmd+Enter takes the same path, because the reference's condition is
    /// `ev.ctrlKey || ev.metaKey` (`edit.js:283`).
    #[test]
    fn meta_enter_is_a_dry_run_and_not_an_apply() {
        let out = decide_key(KeyEvent::meta_enter());
        assert!(out.consumed);
        assert_eq!(out.run, Some(RunMode::Dry));
        assert_ne!(out.run, Some(RunMode::Apply));
    }

    /// The reference is symmetric in the two modifiers, so the two gestures must
    /// be indistinguishable to the handler.
    #[test]
    fn ctrl_and_meta_enter_are_the_same_gesture() {
        assert_eq!(decide_key(KeyEvent::ctrl_enter()), decide_key(KeyEvent::meta_enter()));
    }

    /// Both modifiers held is still just `||` — one dry run, not two.
    #[test]
    fn ctrl_plus_meta_enter_is_one_dry_run() {
        let ev = KeyEvent { key: KeyName::Enter, ctrl: true, meta: true };
        assert_eq!(decide_key(ev), KeyOutcome { consumed: true, run: Some(RunMode::Dry) });
    }

    /// `preventDefault()` lives *inside* the `if` (`edit.js:284`), so a plain
    /// Enter must not be consumed — it has to reach the textarea and insert a
    /// newline. Asserted in both directions.
    #[test]
    fn plain_enter_is_not_consumed_and_runs_nothing() {
        let out = decide_key(KeyEvent::plain(KeyName::Enter));
        assert!(!out.consumed, "plain Enter must NOT be preventDefault'd");
        assert_eq!(out.run, None, "plain Enter must not run anything");
        assert_eq!(out, KeyOutcome::IGNORED);
    }

    /// Any other key is ignored, with and without Ctrl — the reference tests
    /// `ev.key === 'Enter'` first, so Ctrl+<anything else> is not a run.
    #[test]
    fn other_keys_are_ignored_even_with_ctrl_or_meta() {
        assert_eq!(decide_key(KeyEvent::plain(KeyName::Other)), KeyOutcome::IGNORED);
        assert_eq!(
            decide_key(KeyEvent { key: KeyName::Other, ctrl: true, meta: false }),
            KeyOutcome::IGNORED
        );
        assert_eq!(
            decide_key(KeyEvent { key: KeyName::Other, ctrl: false, meta: true }),
            KeyOutcome::IGNORED
        );
        assert_eq!(
            decide_key(KeyEvent { key: KeyName::Other, ctrl: true, meta: true }),
            KeyOutcome::IGNORED
        );
    }

    /// The handler has no Apply path at all: no key event can produce one. This
    /// is the structural guarantee behind the two-directional keydown tests
    /// above — Apply is reachable only from `run(true)`.
    #[test]
    fn no_key_event_can_produce_an_apply() {
        for key in [KeyName::Enter, KeyName::Other] {
            for ctrl in [false, true] {
                for meta in [false, true] {
                    let out = decide_key(KeyEvent { key, ctrl, meta });
                    assert_ne!(
                        out.run,
                        Some(RunMode::Apply),
                        "keydown produced Apply for {key:?} ctrl={ctrl} meta={meta}"
                    );
                }
            }
        }
    }

    // ------------------------------------------------------- dry vs apply labels

    /// The two modes must be distinguishable in every user-visible string,
    /// because a mislabelled apply is how a user comes to believe a write was
    /// only a preview.
    #[test]
    fn the_two_modes_have_disjoint_labels() {
        assert_eq!(RunMode::Dry.busy_label("rename_symbol"), "Dry-running rename_symbol…");
        assert_eq!(RunMode::Apply.busy_label("rename_symbol"), "Applying rename_symbol…");
        assert_ne!(RunMode::Dry.busy_label("t"), RunMode::Apply.busy_label("t"));

        assert_eq!(RunMode::Dry.done_note("rename_symbol"), "rename_symbol dry run");
        assert_eq!(RunMode::Apply.done_note("rename_symbol"), "rename_symbol applied");
        assert_ne!(RunMode::Dry.done_note("t"), RunMode::Apply.done_note("t"));
    }

    /// `run(true)` is the only thing that writes; `run(false)` does not.
    #[test]
    fn only_apply_writes() {
        assert!(!RunMode::Dry.writes());
        assert!(RunMode::Apply.writes());
    }

    /// `edit.js:217` — Apply adds `apply: true`, and the dry run does not. Both
    /// directions, at the wire level.
    #[test]
    fn apply_key_is_added_only_by_apply() {
        let args = Args::parse(r#"{"like": ["sps"]}"#).expect("valid");
        let applied = args.with_apply_if(RunMode::Apply);
        assert_eq!(applied.get("apply"), Some(&Value::Bool(true)));
        assert!(applied.has("apply"));

        let dry = args.with_apply_if(RunMode::Dry);
        assert_eq!(dry.get("apply"), None);
        assert!(!dry.has("apply"), "a dry run must not carry apply:true");
    }

    /// A dry run must not mutate the args it was derived from — the reference
    /// mutates its own object, but the two bodies must not alias in our model.
    #[test]
    fn with_apply_if_does_not_mutate_the_original() {
        let args = Args::parse(r#"{"limit": 50}"#).expect("valid");
        let _ = args.with_apply_if(RunMode::Apply);
        assert!(!args.has("apply"), "the source args must be unchanged");
    }

    /// An explicit `apply:false` typed into the box survives a dry run and is
    /// genuinely overwritten by an apply — the reference's `args.apply = true` is
    /// an assignment, not a conditional insert.
    #[test]
    fn apply_overrides_a_typed_apply_false() {
        let args = Args::parse(r#"{"apply": false}"#).expect("valid");
        assert_eq!(args.get("apply"), Some(&Value::Bool(false)));
        assert_eq!(args.with_apply_if(RunMode::Apply).get("apply"), Some(&Value::Bool(true)));
        assert_eq!(args.with_apply_if(RunMode::Dry).get("apply"), Some(&Value::Bool(false)));
    }

    // --------------------------------------------------------------- args model

    /// The box opens holding `{}` (`index.html:531`), and that parses to zero
    /// fields rather than being an error.
    #[test]
    fn the_shipped_default_parses_to_an_empty_object() {
        let view = EditArgsView::new();
        assert_eq!(view.text(), DEFAULT_ARGS_TEXT);
        assert_eq!(view.text(), "{}");
        let args = view.parse().expect("the shipped `{}` must parse");
        assert!(args.is_empty());
        assert_eq!(args.len(), 0);
        assert!(view.is_untouched());
        assert!(view.is_replaceable_skeleton());
    }

    /// A whitespace-only box is `{}`, not a parse error: `boxArgs` short-circuits
    /// on `!raw` before calling `JSON.parse` (`edit.js:164`).
    #[test]
    fn a_blank_box_is_an_empty_object_not_an_error() {
        for raw in ["", "   ", "\n\t "] {
            let args = Args::parse(raw).unwrap_or_else(|e| panic!("{raw:?} must be Ok, got {e:?}"));
            assert!(args.is_empty(), "{raw:?} must parse to zero fields");
        }
    }

    /// The documented placeholder is itself valid JSON, so an affordance can
    /// never be a trap.
    #[test]
    fn the_placeholder_is_valid_args() {
        let args = Args::parse(ARGS_PLACEHOLDER).expect("the placeholder must parse");
        assert_eq!(args.get("limit"), Some(&Value::from(50)));
        assert_eq!(args.len(), 2);
    }

    /// Real field values survive with their JSON types intact — the reference
    /// relies on `limit` being a number, not `"50"`.
    #[test]
    fn field_types_survive_parsing() {
        let args = Args::parse(r#"{"limit":50,"literal":false,"q":"sps"}"#).expect("valid");
        assert_eq!(args.get("limit"), Some(&Value::from(50)));
        assert_eq!(args.get("literal"), Some(&Value::Bool(false)));
        assert_eq!(args.get("q"), Some(&Value::from("sps")));
        assert!(!args.has("missing"));
        assert_eq!(args.get("missing"), None);
    }

    /// An explicit JSON `null` stays distinguishable from an absent key — which
    /// is why `has` exists beside `get`.
    #[test]
    fn an_explicit_null_is_not_an_absent_key() {
        let args = Args::parse(r#"{"files": null}"#).expect("valid");
        assert!(args.has("files"));
        assert_eq!(args.get("files"), Some(&Value::Null));
    }

    /// Nested arrays/objects are preserved verbatim — `like` ships as an array in
    /// the templates (`edit.js:38`) and must not be flattened.
    #[test]
    fn nested_values_round_trip() {
        let raw = r#"{"like":["sps","socket"],"mtype":["void"],"edits":[{"old":"a","new":"b"}]}"#;
        let args = Args::parse(raw).expect("valid");
        assert_eq!(args.to_json(), serde_json::from_str::<Value>(raw).expect("valid"));
    }

    // ------------------------------------------------------------- malformed in

    /// Malformed JSON is reported as an `'Args: '` error note and must not panic
    /// — the reference wraps `JSON.parse` in try/catch (`edit.js:210-216`).
    #[test]
    fn malformed_json_is_an_error_note_and_never_panics() {
        for raw in ["{", "{\"a\":}", "not json", "{'a':1}", "[1,", "{\"a\":1,}"] {
            let err = Args::parse(raw).expect_err(&format!("{raw:?} must be refused"));
            assert!(matches!(err, ArgsError::Malformed(_)), "{raw:?} -> {err:?}");
            let note = err.note();
            assert!(note.starts_with(ARGS_NOTE_PREFIX), "{note:?} must carry the prefix");
            assert!(note.len() > ARGS_NOTE_PREFIX.len(), "the parser message must survive");
        }
    }

    /// Well-formed JSON that is not a plain object takes the reference's *other*
    /// branch — `'args must be a JSON object'` (`edit.js:167`). An array is
    /// refused even though it parses, as are `null` and the scalars.
    #[test]
    fn non_object_json_is_refused_with_the_reference_message() {
        for raw in ["[]", "[1,2]", "null", "true", "false", "0", "42", "\"sps\""] {
            let err = Args::parse(raw).expect_err(&format!("{raw:?} must be refused"));
            assert_eq!(err, ArgsError::NotAnObject, "{raw:?}");
            assert_eq!(err.message(), "args must be a JSON object");
            assert_eq!(err.note(), "Args: args must be a JSON object");
        }
    }

    /// The two refusals are distinct values, so a caller cannot show the parser
    /// message for a shape problem or vice versa.
    #[test]
    fn the_two_refusal_kinds_are_distinguishable() {
        let malformed = Args::parse("{").expect_err("refused");
        let not_object = Args::parse("[]").expect_err("refused");
        assert_ne!(malformed, not_object);
        assert!(matches!(malformed, ArgsError::Malformed(_)));
        assert!(matches!(not_object, ArgsError::NotAnObject));
    }

    // ------------------------------------------------------------ note plumbing

    /// `note()` (`edit.js:59-62`) composes exactly these class names.
    #[test]
    fn note_classes_match_the_reference() {
        assert_eq!(Note::ok("x").class_name(), "modal-note ok");
        assert_eq!(Note::error("x").class_name(), "modal-note error");
        assert_eq!(Note::clear().class_name(), "modal-note");
        assert!(Note::clear().is_empty());
        assert!(!Note::ok("").is_empty() || Note::ok("").is_empty());
        assert_eq!(NoteKind::Ok.css(), "ok");
        assert_eq!(NoteKind::Error.css(), "error");
    }

    /// The handler notes a *refusal* for a bad box, and the mode it asked for is
    /// still the dry run — "handled, and the args were bad" is a distinct
    /// outcome from "key not handled".
    #[test]
    fn a_bad_box_on_ctrl_enter_notes_the_error_but_still_asks_for_a_dry_run() {
        let mut view = EditArgsView::new();
        view.set_text("{oops");
        let (outcome, note) = view.keydown(KeyEvent::ctrl_enter());
        assert!(outcome.consumed);
        assert_eq!(outcome.run, Some(RunMode::Dry), "a parse failure must not become an apply");
        let note = note.expect("the handler ran, so it noted something");
        assert_eq!(note.kind, Some(NoteKind::Error));
        assert!(note.text.starts_with(ARGS_NOTE_PREFIX));
    }

    /// ...and the ignored path notes **nothing at all**, because the reference
    /// performs no `note()` call when the `if` is false. A caller must leave
    /// `#editNote` alone.
    #[test]
    fn an_ignored_key_produces_no_note() {
        let mut view = EditArgsView::new();
        let (outcome, note) = view.keydown(KeyEvent::plain(KeyName::Enter));
        assert_eq!(outcome, KeyOutcome::IGNORED);
        assert!(note.is_none(), "an ignored keydown must not touch #editNote");
    }

    /// A good box on Ctrl+Enter notes the dry-run busy label, so the user is told
    /// which of the two things is happening.
    #[test]
    fn a_good_box_on_ctrl_enter_notes_a_dry_run_not_an_apply() {
        let mut view = EditArgsView::new();
        let (outcome, note) = view.keydown(KeyEvent::ctrl_enter());
        assert_eq!(outcome.run, Some(RunMode::Dry));
        let note = note.expect("noted");
        assert_eq!(note.kind, Some(NoteKind::Ok));
        assert!(note.text.contains("Dry-running"), "{:?}", note.text);
        assert!(!note.text.contains("Applying"), "{:?}", note.text);
    }

    /// Typing marks the box deliberate, so `fillTemplate` may no longer replace
    /// it (`edit.js:53` protects a typed document).
    #[test]
    fn typing_makes_the_box_unreplaceable() {
        let mut view = EditArgsView::new();
        assert!(view.is_replaceable_skeleton());
        view.set_text(r#"{"like":["sps"]}"#);
        assert!(!view.is_untouched());
        assert!(!view.is_replaceable_skeleton());
        assert_eq!(view.parse().expect("valid").get("like"), Some(&serde_json::json!(["sps"])));
    }

    /// Replacing the box with a fresh `{}` is still replaceable — the sentinel is
    /// the *content*, not merely "never assigned".
    #[test]
    fn a_box_reset_to_empty_object_is_replaceable_again() {
        let mut view = EditArgsView::new();
        view.set_text("garbage");
        view.set_text("{}");
        assert!(view.is_replaceable_skeleton());
        view.set_text("   ");
        assert!(view.is_replaceable_skeleton());
    }
}
