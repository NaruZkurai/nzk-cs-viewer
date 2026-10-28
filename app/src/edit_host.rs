//! The edit modal's **host state** — the piece `edit.js` spreads across
//! module-level vars, the `#editModal` open flag and the seven DOM nodes the
//! modal owns.
//!
//! This module owns exactly three things and nothing else:
//!
//! 1. **Open/closed** — whether `#editModal` is being shown.
//! 2. **The working view** — one [`EditToolView`], exposed by reference so the
//!    app can drive it without this module knowing about transports. There is
//!    **no HTTP client here** and nothing in this file performs I/O; the request
//!    bodies live in [`crate::edit_wire`] and the sending is another owner's
//!    job.
//! 3. **The facts document** — a `serde_json` object keyed by the modal's real
//!    element ids (`#editTool`, `#editArgs`, `#editOut`, `#editNote`, `#editDry`,
//!    `#editApply`, `#editClose`), or `None` while closed.
//!
//! ## `None` is "absent", never "empty"
//!
//! `facts()` returns `None` while the modal is closed so the host document
//! carries **no** `edit` key at all, the same convention
//! [`crate::codelens_wire`] and [`crate::hierarchy_wire`] use for their own
//! surfaces. While open, a datum the modal does not have is **omitted** from the
//! object rather than filled with a placeholder: no selection yet means there is
//! no `#editTool` key, no concluded run means there is no `#editOut` key, an
//! empty note means there is no `#editNote` key. The one thing this module must
//! never do is invent a value the view does not hold.
//!
//! ## The Apply invariant is inherited, not re-decided
//!
//! [`EditHost::on_key`] delegates to [`EditToolView::keydown`], which routes
//! through [`crate::edit_wire::KeyIntent`] — a type with **no** apply variant.
//! This module adds no run path of its own: the single Apply producer remains
//! [`EditToolView::press_apply`], reached only by the Lead driving
//! [`EditHost::view_mut`]. The `#editApply` key in `facts()` is *descriptive*
//! (which gesture the button is wired to, and whether it is disabled while busy);
//! it cannot produce a request.

use serde_json::{json, Map, Value};

use crate::edit_args::KeyEvent;
use crate::edit_wire::{EditToolView, KeyIntent};

/// The edit modal as the host sees it: open flag plus the working view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditHost {
    /// Whether `#editModal` is shown. The view survives a close, so reopening
    /// shows what the user left behind rather than a blank modal.
    open: bool,
    /// The modal's own state — args box, tool list, selection, busy, output,
    /// note.
    view: EditToolView,
}

impl Default for EditHost {
    /// A closed modal with a fresh view — the state before `#editBtn` is pressed.
    fn default() -> EditHost {
        EditHost::new()
    }
}

impl EditHost {
    /// A closed modal with the shipped view (`{}` in the box, nothing fetched).
    pub fn new() -> EditHost {
        EditHost { open: false, view: EditToolView::new() }
    }

    /// Show the modal — `#editBtn`. Opens only; the fetch that follows in
    /// `edit.js:232-245` is another owner's call (`load_ops` via `view_mut`).
    pub fn open(&mut self) {
        self.open = true;
    }

    /// Hide the modal — `#editClose`. The view is deliberately **not** reset: the
    /// reference hides the element and keeps its state, so reopening is cheap and
    /// non-destructive.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// Whether the modal is shown.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The working view, for reading state the host does not model.
    pub fn view(&self) -> &EditToolView {
        &self.view
    }

    /// The working view, for the app to drive (load the tool list, start a run,
    /// record a response). Apply goes through `press_apply` here and nowhere
    /// else.
    pub fn view_mut(&mut self) -> &mut EditToolView {
        &mut self.view
    }

    /// A keydown in `#editArgs`, as the host should act on it.
    ///
    /// Delegates to [`EditToolView::keydown`], which decides via
    /// [`crate::edit_args::decide_key`] and can yield only
    /// [`KeyIntent::DryRun`] or [`KeyIntent::Ignored`]. The dry [`RunRequest`]
    /// that `keydown` builds is intentionally *not* returned: this type speaks
    /// intent, and the transport owner takes the request from the view.
    pub fn on_key(&mut self, ev: &KeyEvent) -> KeyIntent {
        let (intent, _note, _request) = self.view.keydown(*ev);
        intent
    }

    /// The modal's facts document, or `None` while closed.
    ///
    /// Keys are the modal's real ids; a datum the view does not have is omitted,
    /// never faked. `#editDry`/`#editApply`/`#editClose` report their bound
    /// gesture and disabled state, which (like `edit.js:65-67`) is exactly
    /// "busy disables the run buttons, Close stays live".
    pub fn facts(&self) -> Option<Value> {
        if !self.open {
            return None;
        }
        let mut out = Map::new();

        // `els.tool.value` — only once a tool list has supplied a selection.
        if let Some(tool) = self.view.selected() {
            out.insert("#editTool".to_string(), json!(tool));
        }

        // `els.tool`'s OPTIONS. `#editTool` above is only the *selected value*;
        // a `<select>` cannot be rebuilt from it, so the list that `load_ops`
        // holds (`edit.js:196-201` builds one `<option>` per engine tool) is
        // published as its own key. The name is an internal wire key rather than
        // a DOM id because the options are children: `nzk_ui::dom::apply_edit`
        // is what turns this into `<option value=...>` nodes, and it is the only
        // consumer. Without this key the select stayed empty and no operation
        // could ever be chosen — the modal would open on the first tool with no
        // way to pick another.
        if let Some(list) = self.view.tools() {
            out.insert("#editToolOptions".to_string(), json!(list));
        }

        // `els.args.value` — the box always holds its text, `{}` included.
        let args = self.view.args.text();
        if !args.is_empty() {
            out.insert("#editArgs".to_string(), json!(args));
        }

        // `els.out.textContent` — only after an operation concluded.
        if let Some(response) = self.view.last_response() {
            out.insert("#editOut".to_string(), response.clone());
        }

        // `els.note` — omitted while the line is empty, so "nothing to say" can
        // never be read as a note that says nothing.
        let note = self.view.last_note();
        if !note.text.is_empty() {
            let mut n = Map::new();
            n.insert("text".to_string(), json!(note.text));
            n.insert("class".to_string(), json!(note.class_name()));
            if let Some(kind) = note.kind {
                n.insert("kind".to_string(), json!(kind.css()));
            }
            out.insert("#editNote".to_string(), Value::Object(n));
        }

        let busy = self.view.is_busy();
        // `els.dry`/`els.apply` are disabled while busy (`edit.js:65-67`); the
        // `apply` field is the button's bound `run(...)` argument, not a request.
        out.insert("#editDry".to_string(), json!({ "apply": false, "disabled": busy }));
        out.insert("#editApply".to_string(), json!({ "apply": true, "disabled": busy }));
        // `#editClose` is never disabled — `edit.js` has no busy guard on it.
        out.insert("#editClose".to_string(), json!({ "disabled": false }));

        Some(Value::Object(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit_args::{KeyEvent, KeyName, RunMode};

    /// Load two tools and select the first, the state a real open reaches after
    /// `GET /api/edit` answers.
    fn host_with_tools() -> EditHost {
        let mut h = EditHost::new();
        h.open();
        h.view_mut().load_ops(&json!({ "ok": true, "tools": ["rename_symbol", "sps"] }));
        h
    }

    // ------------------------------------------------------------ open/closed

    #[test]
    fn a_closed_modal_has_no_facts_key() {
        let h = EditHost::new();
        assert!(!h.is_open(), "a fresh host is closed");
        assert_eq!(h.facts(), None, "closed means the key is absent, not null");
    }

    #[test]
    fn open_then_close_returns_none_and_keeps_the_view() {
        let mut h = host_with_tools();
        assert!(h.is_open());
        assert!(h.facts().is_some(), "an open modal has facts");
        h.close();
        assert!(!h.is_open());
        assert_eq!(h.facts(), None, "closing removes the key again");
        // Reopening shows the state the user left, not a blank modal.
        h.open();
        assert_eq!(
            h.facts().unwrap().get("#editTool").and_then(Value::as_str),
            Some("rename_symbol"),
            "the view survives close/open"
        );
    }

    // ----------------------------------------------------------------- facts

    #[test]
    fn fresh_open_facts_omit_absent_data_and_never_fake_it() {
        let mut h = EditHost::new();
        h.open();
        let f = h.facts().expect("open modal has facts");
        // The shipped box text is a real value, `index.html:531` — not a filler.
        assert_eq!(f.get("#editArgs").and_then(Value::as_str), Some("{}"));
        assert_eq!(f.get("#editDry").and_then(|v| v.get("disabled")).and_then(Value::as_bool), Some(false));
        assert_eq!(f.get("#editApply").and_then(|v| v.get("apply")).and_then(Value::as_bool), Some(true));
        assert_eq!(f.get("#editClose").and_then(|v| v.get("disabled")).and_then(Value::as_bool), Some(false));
        // No tool fetched, no run concluded, no note: the keys are ABSENT.
        assert!(!f.as_object().unwrap().contains_key("#editTool"), "no selection yet -> no #editTool");
        assert!(!f.as_object().unwrap().contains_key("#editOut"), "no response yet -> no #editOut");
        assert!(!f.as_object().unwrap().contains_key("#editNote"), "empty note -> no #editNote");
    }

    #[test]
    fn a_loaded_run_exposes_all_seven_real_ids() {
        let mut h = host_with_tools();
        h.view_mut().finish_run(RunMode::Dry, "rename_symbol", &json!({ "ok": true, "changed": 3 }));
        let f = h.facts().expect("open modal has facts");
        let o = f.as_object().unwrap();
        for id in ["#editTool", "#editArgs", "#editOut", "#editNote", "#editDry", "#editApply", "#editClose"] {
            assert!(o.contains_key(id), "open modal with data must carry {id}");
        }
        assert_eq!(o.get("#editTool").and_then(Value::as_str), Some("rename_symbol"));
        assert_eq!(o.get("#editOut"), Some(&json!({ "ok": true, "changed": 3 })), "the raw response verbatim");
        let note = o.get("#editNote").unwrap();
        assert_eq!(note.get("text").and_then(Value::as_str), Some("rename_symbol dry run"));
        assert_eq!(note.get("kind").and_then(Value::as_str), Some("ok"));
    }

    #[test]
    fn busy_disables_both_run_buttons_but_never_close() {
        let mut h = host_with_tools();
        assert!(h.view_mut().press_dry().is_ok(), "a dry run starts");
        assert!(h.view().is_busy());
        let f = h.facts().unwrap();
        assert_eq!(f["#editDry"]["disabled"], json!(true), "busy disables Dry (edit.js:65-67)");
        assert_eq!(f["#editApply"]["disabled"], json!(true), "busy disables Apply");
        assert_eq!(f["#editClose"]["disabled"], json!(false), "Close has no busy guard");
        h.view_mut().finish_run(RunMode::Dry, "rename_symbol", &json!({ "ok": true }));
        assert!(!h.view().is_busy());
        let f = h.facts().unwrap();
        assert_eq!(f["#editDry"]["disabled"], json!(false), "finishing re-enables Dry");
        assert_eq!(f["#editApply"]["disabled"], json!(false), "finishing re-enables Apply");
    }

    /// The select's OPTIONS, not just its value.
    ///
    /// `#editTool` alone is `els.tool.value`; the `<select>` cannot be rebuilt
    /// from it, so the host must publish the list too. Without this key
    /// `nzk_ui::dom::apply_edit` had nothing to build `<option>` children from
    /// and the picker stayed empty — the modal opened on the first operation with
    /// no way to choose another.
    #[test]
    fn the_open_modal_publishes_the_selects_options() {
        let h = host_with_tools();
        let f = h.facts().expect("open modal has facts");
        assert_eq!(
            f.get("#editToolOptions"),
            Some(&json!(["rename_symbol", "sps"])),
            "the options are the loaded list, in order"
        );
        assert_eq!(
            f.get("#editTool").and_then(Value::as_str),
            Some("rename_symbol"),
            "and the value is the selected one"
        );
    }

    /// No list fetched means no options key — absent, never an empty array that
    /// would read as "the engine offers nothing".
    #[test]
    fn an_unfetched_modal_omits_the_options_key() {
        let mut h = EditHost::new();
        h.open();
        let f = h.facts().expect("open modal has facts");
        assert!(
            !f.as_object().unwrap().contains_key("#editToolOptions"),
            "nothing fetched yet -> the key is absent"
        );
    }

    // ---------------------------------------------------------------- on_key

    #[test]
    fn on_key_ctrl_enter_is_a_dry_run_and_starts_one() {
        let mut h = host_with_tools();
        assert_eq!(h.on_key(&KeyEvent::ctrl_enter()), KeyIntent::DryRun);
        assert!(h.view().is_busy(), "the dry run is in flight");
        assert_eq!(h.view().last_note().text, "Dry-running rename_symbol…", "the Dry label, never Applying");
    }

    #[test]
    fn on_key_plain_enter_is_ignored_and_leaves_the_note_alone() {
        let mut h = host_with_tools();
        let before = h.facts().unwrap().get("#editNote").cloned();
        assert_eq!(before.as_ref().and_then(|n| n.get("text")).and_then(Value::as_str), Some("2 operations"));
        assert_eq!(h.on_key(&KeyEvent::plain(KeyName::Enter)), KeyIntent::Ignored);
        assert!(!h.view().is_busy(), "an ignored key starts no run");
        assert_eq!(h.facts().unwrap().get("#editNote").cloned(), before, "the ignored path does not touch the note");
    }

    /// The key path cannot reach Apply: `KeyIntent` has no apply variant, and the
    /// only request a key produces is a dry one. This is the host-level echo of
    /// `edit_wire`'s sole-producer invariant.
    #[test]
    fn no_key_event_can_reach_apply_through_the_host() {
        let events = [
            KeyEvent::plain(KeyName::Enter),
            KeyEvent::plain(KeyName::Other),
            KeyEvent::ctrl_enter(),
            KeyEvent::meta_enter(),
            KeyEvent { key: KeyName::Enter, ctrl: true, meta: true },
        ];
        for ev in events {
            let mut h = host_with_tools();
            let intent = h.on_key(&ev);
            assert!(
                matches!(intent, KeyIntent::Ignored | KeyIntent::DryRun),
                "on_key produced {intent:?}, which is not a permitted intent"
            );
            if intent == KeyIntent::DryRun {
                assert_eq!(h.view().last_note().text, "Dry-running rename_symbol…");
                assert!(h.view_mut().press_apply().is_err(), "Apply is refused while the dry run is in flight");
            }
        }
    }
}
