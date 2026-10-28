//! Form-control transport: the state machines behind a *real* select, checkbox
//! and text-input interaction in this static-rasteriser host.
//!
//! The round-1 D2 residual was that [`ui.reorg`](crate::reorg_wire) and
//! `ui.preview` were paint-only: the shell draws the controls from host state,
//! but nothing turned a press or a keystroke into a new value. This module is
//! that missing half, and it is deliberately display-free: no egui, no DOM, no
//! engine. The host owns one [`FormState`]; `actions.rs` mutates it from a click
//! or a key, `nzk_ui.rs` publishes [`FormState::facts`] as `ui.form`, and
//! `nzk_ui` paints the value and the open menu.
//!
//! ## Interaction contract
//!
//! * **Select** — a press *opens* the menu ([`SelectMenu`]); `Up`/`Down` move the
//!   active row (wrapping, like a real `<select>`), `Enter` commits it,
//!   `Escape` or a click-away closes without changing the value.
//! * **Text** — a press focuses the box and starts a [`TextEdit`]; printable
//!   characters append at the caret, `Backspace` deletes, `Enter` commits and
//!   blurs, `Escape` blurs (the reference's `blur()` path keeps the typed text).
//! * **Checkbox** — a press toggles the boolean immediately.
//!
//! Every change is also mirrored into the owning request model
//! ([`FormState::sync_reorg`]) so the values the engine is called with are the
//! values the user sees — never a second copy that can drift.
//!
//! ## Why a click-away guard exists
//!
//! While a menu is open a press on any other control would otherwise route to
//! that control; the reference's `select` closes first. [`FormState::is_active`]
//! is what `actions.rs` tests to close-and-swallow that press, exactly like a
//! native popup.

use serde_json::{json, Value};

use crate::reorg_wire::{Gran, ReorgRequest, Scope};
use crate::blocks_peek::{ReorgGroup, ReorgMode};

pub const DEPTH_MIN: u32 = 0;
pub const DEPTH_MAX: u32 = 9_999_999;
pub const DEPTH_DEFAULT: u32 = 1;

/// The kind of control a field id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Select,
    Text,
    Check,
}

/// One form control the host can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// The DOM id — the same string `CONTROLS`/`resolve_selector` resolves.
    pub id: &'static str,
    pub kind: Kind,
}

/// The controls wired in this pass: the reorg dialog's form (`reorg.js`), the
/// preview dialog's target (`preview.js`), and the folder picker's path input
/// (`shell.js:53` `#modalPathInput`).
pub const FIELDS: &[Field] = &[
    Field { id: "#reorgMode", kind: Kind::Select },
    Field { id: "#reorgScope", kind: Kind::Select },
    Field { id: "#reorgGran", kind: Kind::Select },
    Field { id: "#reorgGroup", kind: Kind::Select },
    Field { id: "#reorgSize", kind: Kind::Text },
    Field { id: "#reorgDest", kind: Kind::Text },
    Field { id: "#reorgHeader", kind: Kind::Check },
    Field { id: "#previewSelector", kind: Kind::Text },
    Field { id: "#previewTo", kind: Kind::Text },
    Field { id: "#previewOp", kind: Kind::Select },
    Field { id: "#nsSelect", kind: Kind::Select },
    Field { id: "#editTool", kind: Kind::Select },
    Field { id: "#uiTheme", kind: Kind::Select },
    Field { id: "#chainDepth", kind: Kind::Select },
    Field { id: "#chainDir", kind: Kind::Select },
    Field { id: "#sourceTheme", kind: Kind::Select },
    Field { id: "#depthSelect", kind: Kind::Text },
    Field { id: "#rootInput", kind: Kind::Text },
    Field { id: "#pubSearch", kind: Kind::Text },
    Field { id: "#strSearch", kind: Kind::Text },
    // The v3-only Declared-types section's fuzzy box. It is created at runtime
    // by `nzk_ui::dom::apply_type_defs`, so it has no `index.html` counterpart;
    // `Field` is what lets the form model focus it and read its in-flight text.
    Field { id: "#typeSearch", kind: Kind::Text },
    Field { id: "#modalPathInput", kind: Kind::Text },
];

/// The field a DOM id names, if it is one of [`FIELDS`].
pub fn field(id: &str) -> Option<Field> {
    FIELDS.iter().copied().find(|f| f.id == id)
}

/// One `<option>`: `(value, label)` — the shipped markup's own pairs
/// (`nzk_ui/src/skeleton/modals_b.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionDef {
    pub value: &'static str,
    pub label: &'static str,
}

/// The static options of a select, from the shipped skeleton.
pub fn options(id: &str) -> &'static [OptionDef] {
    match id {
        "#reorgMode" => &[
            OptionDef { value: "single", label: "One merged file" },
            OptionDef { value: "split", label: "Split into chunks" },
        ],
        "#reorgScope" => &[
            OptionDef { value: "selection", label: "The graph selection" },
            OptionDef { value: "all", label: "The whole corpus" },
        ],
        "#reorgGran" => &[
            OptionDef { value: "leaf", label: "One declaration per chunk" },
            OptionDef { value: "top", label: "Whole top-level types" },
        ],
        "#reorgGroup" => &[
            OptionDef { value: "size", label: "By size budget" },
            OptionDef { value: "namespace", label: "By namespace" },
            OptionDef { value: "owner", label: "By owning type" },
            OptionDef { value: "single", label: "Everything in one file" },
        ],
        "#uiTheme" => &[
            OptionDef { value: "nzk-dark+", label: "nzk-dark+ · VS Code Dark+" },
            OptionDef { value: "dark-v1", label: "dark v1" },
        ],
        "#chainDepth" => &[
            OptionDef { value: "1", label: "1" },
            OptionDef { value: "2", label: "2" },
            OptionDef { value: "3", label: "3" },
            OptionDef { value: "4", label: "4" },
            OptionDef { value: "5", label: "5" },
            OptionDef { value: "6", label: "6" },
        ],
        "#chainDir" => &[
            OptionDef { value: "forward", label: "forward" },
            OptionDef { value: "backward", label: "backward" },
            OptionDef { value: "both", label: "both" },
        ],
        "#sourceTheme" => &[
            OptionDef { value: "darkplus", label: "Dark+" },
            OptionDef { value: "darkmodern", label: "Dark Modern" },
            OptionDef { value: "monokai", label: "Monokai" },
            OptionDef { value: "solarized", label: "Solarized Dark" },
        ],
        // `#previewOp` is filled from the engine's selector list
        // (`preview.js`), so its options are dynamic — see
        // [`FormState::set_preview_ops`]. `#editTool` is the engine's tool list
        // (`edit.js:196-201`), so it is dynamic too — see
        // [`FormState::set_edit_tools`].
        _ => &[],
    }
}

/// The reorg dialog's editable values. Mirrors
/// [`ReorgRequest`] one-for-one; [`FormState::sync_reorg`] is the only writer
/// that pushes them across.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReorgForm {
    pub mode: &'static str,
    pub scope: &'static str,
    pub gran: &'static str,
    pub group: &'static str,
    pub size: String,
    pub dest: String,
    pub header: bool,
}

impl Default for ReorgForm {
    fn default() -> Self {
        Self {
            mode: "single",
            scope: "selection",
            gran: "leaf",
            group: "size",
            size: "3000".to_string(),
            dest: String::new(),
            header: true,
        }
    }
}

/// An open `<select>` menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectMenu {
    /// The select's DOM id.
    pub id: &'static str,
    /// `(value, label)` rows, in document order.
    pub options: Vec<(String, String)>,
    /// The highlighted row.
    pub active: usize,
}

impl SelectMenu {
    fn open(id: &'static str, options: Vec<(String, String)>, current: &str) -> Self {
        let active = options
            .iter()
            .position(|(v, _)| v == current)
            .unwrap_or(0)
            .min(options.len().saturating_sub(1));
        Self { id, options, active }
    }

    /// Move the highlight, wrapping like a native `<select>`.
    pub fn move_by(&mut self, delta: i32) {
        let n = self.options.len();
        if n == 0 {
            return;
        }
        let next = (self.active as i32 + delta).rem_euclid(n as i32) as usize;
        self.active = next;
    }

    /// The highlighted row's `value`.
    pub fn value(&self) -> Option<&str> {
        self.options.get(self.active).map(|(v, _)| v.as_str())
    }
}

/// A focused text box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    /// The input's DOM id.
    pub id: &'static str,
    /// The characters, so caret movement never splits a UTF-8 sequence.
    pub chars: Vec<char>,
    /// Caret index in `chars`.
    pub cursor: usize,
    pub selected_all: bool,
}

impl TextEdit {
    fn new(id: &'static str, current: &str) -> Self {
        let chars: Vec<char> = current.chars().collect();
        let cursor = chars.len();
        Self { id, chars, cursor, selected_all: false }
    }

    pub fn value(&self) -> String {
        self.chars.iter().collect()
    }

    fn insert(&mut self, c: char) {
        if self.selected_all {
            self.chars.clear();
            self.cursor = 0;
            self.selected_all = false;
        }
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) -> bool {
        if self.selected_all {
            self.chars.clear();
            self.cursor = 0;
            self.selected_all = false;
            return true;
        }
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        self.chars.remove(self.cursor);
        true
    }
}

/// One key, normalised away from winit so the state machine is testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Backspace,
    Enter,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    SelectAll,
}

/// What a key did to the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Nothing was focused: the caller must let the canvas see the key.
    Ignored,
    /// Focused and consumed, but no value changed.
    Consumed,
    /// Focused and consumed; a value/report changed, so repaint.
    Changed,
}

/// The host's whole form interaction state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormState {
    pub reorg: ReorgForm,
    /// `#previewSelector`'s text — the declaration the preview panel plans
    /// against (`preview.js:176-187`: `open()` prefills it from the selection,
    /// `run()` reads it back). Empty = "use the graph selection", the fallback
    /// [`crate::preview_wire::build`] applies.
    pub preview_selector: String,
    pub preview_to: String,
    pub preview_op: String,
    /// `#modalPathInput`'s text (the picker owns the listing; this owns typing).
    pub modal_path: String,
    /// The dynamic `#previewOp` options supplied by the engine selector.
    preview_ops: Vec<(String, String)>,
    /// `#nsSelect`'s committed value; empty = "All namespaces". This is the
    /// form-side copy `actions.rs` mirrors into the filter model (the view
    /// predicate's consumer) after a change.
    pub namespace: String,
    /// `#depthSelect`'s committed numeric value (`shell.js` clamps 0..=9999999).
    pub depth: u32,
    /// `#rootInput`'s committed text — the folder the next Index press indexes.
    /// Seeded from the live repo when the field is focused.
    pub root: String,
    /// `#editTool`'s selected operation and the engine's current tool list.
    pub edit_tool: String,
    edit_tools: Vec<(String, String)>,
    /// `#uiTheme`'s selected value (`nzk-dark+` / `dark-v1`).
    pub ui_theme: String,
    /// `#sourceTheme`'s selected value (`darkplus` / `darkmodern` / `monokai` /
    /// `solarized`).
    pub source_theme: String,
    /// `#pubSearch` / `#strSearch`'s committed text (filters.js:520-530).
    pub pub_query: String,
    pub str_query: String,
    /// `#typeSearch`'s committed text (`#typeSection`, v3-only). The host state
    /// also carries it, and `actions.rs` mirrors this copy into that one on each
    /// edit — the form owns the in-flight text, the host owns the filter.
    pub type_query: String,
    /// `#chainDepth` / `#chainDir`.
    pub chain_depth: u32,
    pub chain_dir: String,
    /// The dynamic `#nsSelect` rows, built by the host from the graph's real
    /// namespaces — never a hardcoded list.
    ns_options: Vec<(String, String)>,
    /// Set when Enter committed `#previewTo`: the `preview.js:188-199` run path
    /// must fire, and `actions.rs` (which owns the dispatch) is told by
    /// [`FormState::take_preview_run`]. One-shot, so a run cannot repeat on the
    /// next unrelated key.
    preview_run_requested: bool,
    /// A committed `#rootInput` waiting for the host to re-index that folder.
    /// One-shot: [`FormState::take_root_commit`] clears it. Enter in the root box
    /// is the reference's `doIndex()` (`shell.js:494-496`); typing alone must
    /// not start a build.
    root_commit: Option<String>,
    /// A committed `#modalPathInput` waiting for the picker to browse it.
    /// One-shot: Enter in the modal's path box is `browseTo(value)`
    /// (`shell.js:752-753`).
    modal_go_requested: bool,
    /// The focused control's DOM id, or `None`.
    pub focus: Option<&'static str>,
    pub menu: Option<SelectMenu>,
    pub edit: Option<TextEdit>,
}

impl Default for FormState {
    fn default() -> Self {
        Self {
            reorg: ReorgForm::default(),
            preview_selector: String::new(),
            preview_to: String::new(),
            preview_op: String::new(),
            modal_path: String::new(),
            preview_ops: Vec::new(),
            namespace: String::new(),
            depth: DEPTH_DEFAULT,
            root: String::new(),
            edit_tool: String::new(),
            edit_tools: Vec::new(),
            ui_theme: "nzk-dark+".to_string(),
            source_theme: "darkplus".to_string(),
            pub_query: String::new(),
            str_query: String::new(),
            type_query: String::new(),
            chain_depth: 3,
            chain_dir: "forward".to_string(),
            ns_options: Vec::new(),
            preview_run_requested: false,
            root_commit: None,
            modal_go_requested: false,
            focus: None,
            menu: None,
            edit: None,
        }
    }
}

impl FormState {
    /// Supply `#previewOp`'s rows (the engine's selector list). Kept separate
    /// because they are CONTENT, not the shipped markup's static options.
    pub fn set_preview_ops(&mut self, ops: Vec<(String, String)>) {
        self.preview_ops = ops;
    }

    /// Supply `#nsSelect`'s rows from the graph's namespaces (host-mirrored
    /// content, not the shipped static options).
    pub fn set_ns_options(&mut self, ops: Vec<(String, String)>) {
        self.ns_options = ops;
    }

    /// Is a menu open, a box focused, or an edit in flight? `actions.rs` uses
    /// this to close-and-swallow the next non-form press (a click-away).
    pub fn is_active(&self) -> bool {
        self.focus.is_some() || self.menu.is_some() || self.edit.is_some()
    }

    /// The current value of a field, as the menus/edits read it.
    pub fn current(&self, id: &'static str) -> String {
        match id {
            "#reorgMode" => self.reorg.mode.to_string(),
            "#reorgScope" => self.reorg.scope.to_string(),
            "#reorgGran" => self.reorg.gran.to_string(),
            "#reorgGroup" => self.reorg.group.to_string(),
            "#reorgSize" => self.reorg.size.clone(),
            "#reorgDest" => self.reorg.dest.clone(),
            "#previewSelector" => self.preview_selector.clone(),
            "#previewTo" => self.preview_to.clone(),
            "#previewOp" => self.preview_op.clone(),
            "#nsSelect" => self.namespace.clone(),
            "#depthSelect" => self.depth.to_string(),
            "#rootInput" => self.root.clone(),
            "#pubSearch" => self.pub_query.clone(),
            "#strSearch" => self.str_query.clone(),
            "#typeSearch" => self.type_query.clone(),
            "#editTool" => self.edit_tool.clone(),
            "#uiTheme" => self.ui_theme.clone(),
            "#sourceTheme" => self.source_theme.clone(),
            "#chainDepth" => self.chain_depth.to_string(),
            "#chainDir" => self.chain_dir.clone(),
            "#modalPathInput" => self.modal_path.clone(),
            _ => String::new(),
        }
    }

    /// Supply `#editTool`'s rows from the engine's tool list (`edit.js:196-201`).
    pub fn set_edit_tools(&mut self, tools: &[String]) {
        self.edit_tools = tools.iter().map(|t| (t.clone(), t.clone())).collect();
    }

    /// The choice an open menu's row index names, without committing it. Used by
    /// callers that must route the value through a non-generic owner (the Edit
    /// dialog's tool selection is owned by `EditToolView`, not this struct).
    pub fn menu_choice(&self, index: usize) -> Option<(&'static str, String)> {
        let menu = self.menu.as_ref()?;
        let (value, _) = menu.options.get(index)?;
        Some((menu.id, value.clone()))
    }

    /// Seed `#rootInput` from the live repo WITHOUT treating it as a commit
    /// (focusing the box must not re-index anything by itself).
    pub fn seed_root(&mut self, value: &str) {
        self.root = value.to_string();
    }

    /// The value visible in a text field, including its uncommitted edit buffer.
    pub fn value(&self, id: &'static str) -> String {
        self.edit
            .as_ref()
            .filter(|edit| edit.id == id)
            .map(TextEdit::value)
            .unwrap_or_else(|| self.current(id))
    }

    /// A press on a form control: open a select's menu, focus a text box, or
    /// toggle a checkbox. Returns `true` when the control is one this module
    /// owns (so the press must not fall through to the canvas).
    pub fn click(&mut self, id: &'static str) -> bool {
        let Some(field) = field(id) else {
            return false;
        };
        self.close();
        self.focus = Some(id);
        match field.kind {
            Kind::Select => {
                let options = if id == "#previewOp" {
                    self.preview_ops.clone()
                } else if id == "#nsSelect" {
                    self.ns_options.clone()
                } else if id == "#editTool" {
                    self.edit_tools.clone()
                } else {
                    options(id)
                        .iter()
                        .map(|o| (o.value.to_string(), o.label.to_string()))
                        .collect()
                };
                let current = self.current(id);
                self.menu = Some(SelectMenu::open(id, options, &current));
            }
            Kind::Text => {
                let current = self.current(id);
                self.edit = Some(TextEdit::new(id, &current));
            }
            Kind::Check => {
                // The only checkbox wired in this pass: `#reorgHeader`.
                self.reorg.header = !self.reorg.header;
                self.focus = None;
            }
        }
        true
    }

    /// Commit menu row `index` — the MOUSE path (`#csFormMenuOption<i>` hit in
    /// the shell chain), sharing [`FormState::set_value`] with the Enter path so
    /// a click and a keystroke cannot commit differently. `false` when no menu is
    /// open or the index is out of range (a stale row id after a repaint).
    pub fn commit_index(&mut self, index: usize) -> bool {
        let Some(menu) = self.menu.as_ref() else {
            return false;
        };
        let Some(value) = menu.options.get(index).map(|(v, _)| v.clone()) else {
            return false;
        };
        let id = menu.id;
        self.menu = None;
        self.focus = None;
        self.set_value(id, &value);
        true
    }

    /// Close any menu/edit/focus without changing a value (Escape, click-away).
    pub fn close(&mut self) {
        self.focus = None;
        self.menu = None;
        self.edit = None;
    }

    /// Commit the focused control's in-flight value, as a browser's `change`
    /// event fires before a click on a button elsewhere (`preview.js`'s Run
    /// button reads `els.to.value`, which includes text typed since focus).
    ///
    /// Returns `true` when there was something to commit. A closed form or an
    /// open menu has nothing to commit (the menu's active row is not a typed
    /// value), so this is a no-op there.
    pub fn commit_focused(&mut self) -> bool {
        let (id, value) = match self.edit.take() {
            Some(edit) => (edit.id, edit.value()),
            None => {
                let had = self.menu.is_some() || self.focus.is_some();
                self.close();
                return had;
            }
        };
        self.focus = None;
        self.menu = None;
        self.set_value(id, &value);
        true
    }

    /// Take the one-shot `#previewTo` Enter request. `true` means "run the
    /// preview now"; the flag is cleared so a second call cannot re-run it.
    pub fn take_preview_run(&mut self) -> bool {
        std::mem::take(&mut self.preview_run_requested)
    }

    /// Take the one-shot `#rootInput` Enter request: the folder the host must
    /// re-index, or `None` when Enter did not commit a non-empty path.
    pub fn take_root_commit(&mut self) -> Option<String> {
        self.root_commit.take()
    }

    /// Take the one-shot `#modalPathInput` Enter request: `true` means "browse
    /// the committed text now".
    pub fn take_modal_go(&mut self) -> bool {
        std::mem::take(&mut self.modal_go_requested)
    }

    /// Route one key. The caller must only offer keys while
    /// [`FormState::is_active`] — [`KeyOutcome::Ignored`] is the escape hatch for
    /// a focus that vanished between the key and the call.
    pub fn key(&mut self, key: Key) -> KeyOutcome {
        if !self.is_active() {
            return KeyOutcome::Ignored;
        }
        if key == Key::Escape {
            self.close();
            return KeyOutcome::Changed;
        }
        if let Some(menu) = self.menu.as_mut() {
            match key {
                Key::Up => {
                    menu.move_by(-1);
                    return KeyOutcome::Changed;
                }
                Key::Down => {
                    menu.move_by(1);
                    return KeyOutcome::Changed;
                }
                Key::Enter => {
                    let id = menu.id;
                    let value = menu.value().map(str::to_string);
                    self.menu = None;
                    self.focus = None;
                    if let Some(value) = value {
                        self.set_value(id, &value);
                        return KeyOutcome::Changed;
                    }
                    return KeyOutcome::Consumed;
                }
                _ => return KeyOutcome::Consumed,
            }
        }
        if let Some(edit) = self.edit.as_mut() {
            match key {
                Key::Char(c) if !c.is_control() => {
                    edit.insert(c);
                    return KeyOutcome::Changed;
                }
                Key::Backspace => {
                    return if edit.backspace() {
                        KeyOutcome::Changed
                    } else {
                        KeyOutcome::Consumed
                    };
                }
                Key::Delete => {
                    if edit.selected_all {
                        edit.chars.clear();
                        edit.cursor = 0;
                        edit.selected_all = false;
                        return KeyOutcome::Changed;
                    }
                    if edit.cursor < edit.chars.len() {
                        edit.chars.remove(edit.cursor);
                        return KeyOutcome::Changed;
                    }
                    return KeyOutcome::Consumed;
                }
                Key::Left => {
                    edit.selected_all = false;
                    edit.cursor = edit.cursor.saturating_sub(1);
                    return KeyOutcome::Consumed;
                }
                Key::Right => {
                    edit.selected_all = false;
                    edit.cursor = (edit.cursor + 1).min(edit.chars.len());
                    return KeyOutcome::Consumed;
                }
                Key::Home => {
                    edit.selected_all = false;
                    edit.cursor = 0;
                    return KeyOutcome::Consumed;
                }
                Key::End => {
                    edit.selected_all = false;
                    edit.cursor = edit.chars.len();
                    return KeyOutcome::Consumed;
                }
                Key::SelectAll => {
                    edit.selected_all = true;
                    return KeyOutcome::Consumed;
                }
                Key::Enter => {
                    // Commit the buffer into the value and blur, the reference's
                    // `input.addEventListener('change'/'blur')` path.
                    let id = edit.id;
                    let value = edit.value();
                    self.edit = None;
                    self.focus = None;
                    self.set_value(id, &value);
                    // Enter in `#previewTo` runs the plan (`preview.js`'s Run
                    // gesture; the reference wires the button, this host also
                    // accepts Enter in the target box). The decision — engine
                    // request or the reference's "enter a target" note — is
                    // `actions.rs`'s, not this pure state machine's.
                    if id == "#previewTo" {
                        self.preview_run_requested = true;
                    }
                    // Enter is the reference's activation gesture for the two
                    // path boxes: `doIndex()` in `#rootInput` (`shell.js:494-496`)
                    // and `browseTo(value)` in `#modalPathInput`
                    // (`shell.js:752-753`). A blur commit is NOT that gesture, so
                    // these flags are set here and not in `commit_focused`.
                    if id == "#rootInput" {
                        let trimmed = value.trim();
                        if !trimmed.is_empty() {
                            self.root_commit = Some(trimmed.to_string());
                        }
                    }
                    if id == "#modalPathInput" {
                        self.modal_go_requested = true;
                    }
                    return KeyOutcome::Changed;
                }
                _ => return KeyOutcome::Consumed,
            }
        }
        KeyOutcome::Consumed
    }

    /// Write one value into the right model. This is the ONLY value writer, so
    /// a menu/edit cannot update a display copy and forget the request model.
    pub fn set_value(&mut self, id: &'static str, value: &str) {
        match id {
            "#reorgMode" => self.reorg.mode = leak(value, &["single", "split"], "single"),
            "#reorgScope" => self.reorg.scope = leak(value, &["selection", "all"], "selection"),
            "#reorgGran" => self.reorg.gran = leak(value, &["leaf", "top"], "leaf"),
            "#reorgGroup" => {
                self.reorg.group = leak(value, &["size", "namespace", "owner", "single"], "size")
            }
            "#reorgSize" | "#reorgDest" | "#depthSelect" => {
                if id == "#reorgSize" {
                    self.reorg.size = value.to_string();
                } else if id == "#reorgDest" {
                    self.reorg.dest = value.to_string();
                } else {
                    self.depth = value
                        .trim()
                        .parse::<u32>()
                        .unwrap_or(self.depth)
                        .clamp(DEPTH_MIN, DEPTH_MAX);
                }
            }
            "#previewSelector" => self.preview_selector = value.to_string(),
            "#previewTo" => self.preview_to = value.to_string(),
            "#previewOp" => self.preview_op = value.to_string(),
            "#nsSelect" => self.namespace = value.to_string(),
            "#rootInput" => {
                self.root = value.to_string();
            }
            "#editTool" => self.edit_tool = value.to_string(),
            "#pubSearch" => self.pub_query = value.to_string(),
            "#strSearch" => self.str_query = value.to_string(),
            "#typeSearch" => self.type_query = value.to_string(),
            "#uiTheme" => self.ui_theme = value.to_string(),
            "#sourceTheme" => self.source_theme = value.to_string(),
            "#chainDepth" => {
                self.chain_depth = value.trim().parse::<u32>().unwrap_or(self.chain_depth).clamp(1, 6)
            }
            "#chainDir" => self.chain_dir = value.to_string(),
            "#modalPathInput" => self.modal_path = value.to_string(),
            _ => {}
        }
    }

    /// Push the reorg form values into the request model the engine is called
    /// with — the facts round-trip requirement: change a control, and the next
    /// `Index::reorg` uses the new value.
    pub fn sync_reorg(&self, req: &mut ReorgRequest) {
        req.mode = if self.reorg.mode == "split" {
            ReorgMode::Split
        } else {
            ReorgMode::Single
        };
        req.scope = if self.reorg.scope == "all" {
            Scope::All
        } else {
            Scope::Selection
        };
        req.gran = if self.reorg.gran == "top" {
            Gran::Top
        } else {
            Gran::Leaf
        };
        req.group = match self.reorg.group {
            "namespace" => ReorgGroup::Namespace,
            "owner" => ReorgGroup::Owner,
            "single" => ReorgGroup::Single,
            _ => ReorgGroup::Size,
        };
        // `reorg.js:20` clamps the size to the input's `min="50"` and falls back
        // to 3000 on an unparsable value.
        req.size = self
            .reorg
            .size
            .trim()
            .parse::<u32>()
            .map(|n| n.max(50))
            .unwrap_or(3000);
        req.dest = self.reorg.dest.clone();
        req.header = self.reorg.header;
    }

    /// Prefill `#previewSelector` from the graph selection, the way the
    /// reference's `open()` does at `preview.js:183-185`
    /// (`els.sel.value = state.selected || ''`). A selector typed by the user is
    /// replaced on every open, exactly like the reference; the text the user can
    /// see afterwards is the text the next plan asks about.
    pub fn prefill_preview_selector(&mut self, selected: Option<&str>) {
        self.preview_selector = selected.unwrap_or("").to_string();
    }

    /// The facts for a selector that is committed but no longer focused: the
    /// shell has to paint the prefilled/typed declaration even though no menu is
    /// open. `None` while there is nothing to paint, so the shipped document
    /// stays byte-identical.
    fn selector_facts(&self) -> Option<Value> {
        (!self.preview_selector.is_empty())
            .then(|| json!({"id": "#previewSelector", "value": self.preview_selector}))
    }

    /// The `ui.form` facts: focused control, open menu, and the text box's
    /// in-flight value. `None` when nothing is focused, so the default paint is
    /// byte-identical.
    pub fn facts(&self) -> Option<Value> {
        // `#modalPathInput`'s committed text is published even when the box is
        // no longer focused: the picker's own `ui.picker` facts paint that input
        // too, and they are applied EARLIER in `apply_ui_state` (dom.rs) — this
        // later write is what lets a typed path survive a same-pass picker
        // repaint. An empty default still yields `None`, so the shipped document
        // is byte-identical.
        if !self.is_active()
            && self.modal_path.is_empty()
            && self.preview_selector.is_empty()
            && self.depth == DEPTH_DEFAULT
        {
            return None;
        }
        let menu = self.menu.as_ref().map(|m| {
            json!({
                "id": m.id,
                "active": m.active,
                "options": m.options.iter().map(|(v, l)| json!({
                    "value": v,
                    "label": l,
                    "selected": v == m.value().unwrap_or(""),
                })).collect::<Vec<_>>(),
            })
        });
        let input = self.edit.as_ref().map(|e| {
            // `caret` is published only for a live edit buffer: an unfocused
            // box whose committed value is still painted (preview selector,
            // picker path) must not grow a caret at offset 0.
            json!({"id": e.id, "value": e.value(), "caret": e.cursor})
        }).or_else(|| self.selector_facts()).or_else(|| {
            (!self.modal_path.is_empty()).then(|| {
                json!({"id": "#modalPathInput", "value": self.modal_path})
            })
        }).or_else(|| {
            (self.depth != DEPTH_DEFAULT).then(|| {
                json!({"id": "#depthSelect", "value": self.depth.to_string()})
            })
        });
        Some(json!({
            "focused": self.focus,
            "menu": menu,
            "input": input,
        }))
    }
}

/// `&'static str` from a select value: the static option table is the owner, so
/// a value that is not one of the shipped options falls back to the shipped
/// default rather than inventing a string.
fn leak(value: &str, allowed: &[&'static str], default: &'static str) -> &'static str {
    allowed
        .iter()
        .copied()
        .find(|candidate| *candidate == value)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_wired_field_id_is_unique_and_typed() {
        for (i, f) in FIELDS.iter().enumerate() {
            assert!(FIELDS.iter().skip(i + 1).all(|g| g.id != f.id), "dup {}", f.id);
        }
        assert_eq!(field("#reorgMode").map(|f| f.kind), Some(Kind::Select));
        assert_eq!(field("#reorgHeader").map(|f| f.kind), Some(Kind::Check));
        assert_eq!(field("#reorgSize").map(|f| f.kind), Some(Kind::Text));
        assert_eq!(field("#previewTo").map(|f| f.kind), Some(Kind::Text));
        assert_eq!(field("#modalPathInput").map(|f| f.kind), Some(Kind::Text));
        assert_eq!(field("#filtersHead"), None);
    }

    #[test]
    fn a_fresh_form_is_absent_while_idle() {
        let form = FormState::default();
        assert!(!form.is_active());
        assert_eq!(form.facts(), None);
        assert_eq!(form.reorg, ReorgForm::default());
        assert_eq!(form.reorg.size, "3000");
        assert!(form.reorg.header);
    }

    #[test]
    fn clicking_a_select_opens_the_shipped_options_at_the_current_value() {
        let mut form = FormState::default();
        assert!(form.click("#reorgMode"));
        let menu = form.menu.as_ref().expect("menu open");
        assert_eq!(menu.id, "#reorgMode");
        assert_eq!(menu.active, 0, "current 'single' is row 0");
        assert_eq!(menu.options.len(), 2);
        // Opening does not change the value.
        assert_eq!(form.reorg.mode, "single");
    }

    #[test]
    fn the_menu_wraps_and_enter_commits_the_highlighted_value() {
        let mut form = FormState::default();
        form.click("#reorgMode");
        assert_eq!(form.key(Key::Up), KeyOutcome::Changed, "wrap backwards");
        assert_eq!(form.menu.as_ref().unwrap().active, 1);
        assert_eq!(form.key(Key::Enter), KeyOutcome::Changed);
        assert_eq!(form.reorg.mode, "split");
        assert!(form.menu.is_none(), "commit closes the menu");
        assert!(!form.is_active());
    }

    #[test]
    fn escape_closes_a_menu_without_changing_the_value() {
        let mut form = FormState::default();
        form.click("#reorgGroup");
        form.key(Key::Down);
        assert_eq!(form.key(Key::Escape), KeyOutcome::Changed);
        assert_eq!(form.reorg.group, "size", "Esc is a cancel, never a commit");
        assert!(!form.is_active());
    }

    #[test]
    fn every_field_of_the_reorg_request_is_editable_end_to_end() {
        let mut form = FormState::default();
        // mode -> split
        form.click("#reorgMode");
        form.key(Key::Down);
        form.key(Key::Enter);
        // scope -> all
        form.click("#reorgScope");
        form.key(Key::Down);
        form.key(Key::Enter);
        // gran -> top
        form.click("#reorgGran");
        form.key(Key::Down);
        form.key(Key::Enter);
        // group -> owner
        form.click("#reorgGroup");
        form.key(Key::Down);
        form.key(Key::Down);
        form.key(Key::Enter);
        // header -> off (click toggles immediately)
        form.click("#reorgHeader");
        // size / dest via typing. `#reorgSize` ships "3000", so the caret
        // starts after it; clear it first, exactly as a user would.
        form.click("#reorgSize");
        for _ in 0..4 {
            form.key(Key::Backspace);
        }
        for c in "1200".chars() {
            form.key(Key::Char(c));
        }
        form.key(Key::Enter);
        form.click("#reorgDest");
        for c in "/tmp/out".chars() {
            form.key(Key::Char(c));
        }
        form.key(Key::Enter);
        assert_eq!(form.reorg.mode, "split");
        assert_eq!(form.reorg.scope, "all");
        assert_eq!(form.reorg.gran, "top");
        assert_eq!(form.reorg.group, "owner");
        assert!(!form.reorg.header);
        assert_eq!(form.reorg.size, "1200");
        assert_eq!(form.reorg.dest, "/tmp/out");
        // ... and the request model the engine is called with follows.
        let mut req = ReorgRequest::default();
        form.sync_reorg(&mut req);
        assert_eq!(req.mode.as_str(), "split");
        assert_eq!(req.scope.as_str(), "all");
        assert_eq!(req.gran.as_str(), "top");
        assert_eq!(req.group.as_str(), "owner");
        assert_eq!(req.size, 1200);
        assert_eq!(req.dest, "/tmp/out");
        assert!(!req.header);
    }

    #[test]
    fn text_editing_starts_at_the_caret_end_and_backspace_removes_one_char() {
        let mut form = FormState::default();
        form.click("#reorgSize");
        let edit = form.edit.as_ref().unwrap();
        assert_eq!(edit.value(), "3000");
        assert_eq!(edit.cursor, 4, "caret starts after the existing text");
        for c in "5".chars() {
            form.key(Key::Char(c));
        }
        assert_eq!(form.edit.as_ref().unwrap().value(), "30005");
        form.key(Key::Backspace);
        form.key(Key::Backspace);
        assert_eq!(form.edit.as_ref().unwrap().value(), "300");
        assert_eq!(form.key(Key::Enter), KeyOutcome::Changed);
        assert_eq!(form.reorg.size, "300");
    }

    #[test]
    fn escape_on_a_text_box_keeps_the_typing_but_does_not_commit_it() {
        let mut form = FormState::default();
        form.click("#reorgDest");
        for c in "abc".chars() {
            form.key(Key::Char(c));
        }
        form.key(Key::Escape);
        assert_eq!(form.reorg.dest, "", "Esc never commits a text box");
        assert!(!form.is_active());
    }

    #[test]
    fn an_unset_id_is_not_a_form_control_and_changes_nothing() {
        let mut form = FormState::default();
        assert!(!form.click("#filtersHead"));
        assert!(!form.is_active());
        assert_eq!(form.key(Key::Char('a')), KeyOutcome::Ignored);
    }

    #[test]
    fn keys_are_ignored_when_nothing_is_focused() {
        let mut form = FormState::default();
        assert_eq!(form.key(Key::Down), KeyOutcome::Ignored);
        assert_eq!(form.facts(), None);
    }

    #[test]
    fn the_size_is_clamped_and_unparsable_text_falls_back_like_reorg_js() {
        let mut form = FormState::default();
        form.set_value("#reorgSize", "7");
        let mut req = ReorgRequest::default();
        form.sync_reorg(&mut req);
        assert_eq!(req.size, 50, "Math.max(50, …)");
        form.set_value("#reorgSize", "abc");
        form.sync_reorg(&mut req);
        assert_eq!(req.size, 3000, "Number('abc') -> the 3000 fallback");
    }

    #[test]
    fn a_menu_row_click_commits_the_same_value_as_enter() {
        let mut form = FormState::default();
        form.click("#reorgGran");
        // Row 1 is "top" (`#reorgGran`'s second shipped option).
        assert!(form.commit_index(1));
        assert_eq!(form.reorg.gran, "top");
        assert!(!form.is_active());
        // A stale index (or a commit with no menu open) is refused, never
        // guessed at.
        assert!(!form.commit_index(0));
        assert_eq!(form.reorg.gran, "top");
    }

    #[test]
    fn a_committed_picker_path_stays_published_after_blur() {
        let mut form = FormState::default();
        assert_eq!(form.facts(), None, "an empty path is not a fact");
        form.click("#modalPathInput");
        for c in "/repo".chars() {
            form.key(Key::Char(c));
        }
        form.key(Key::Enter);
        assert!(!form.is_active());
        let facts = form.facts().expect("a committed path survives the blur");
        assert_eq!(facts["input"]["id"], "#modalPathInput");
        assert_eq!(facts["input"]["value"], "/repo");
        assert_eq!(
            facts["input"]["caret"],
            Value::Null,
            "a committed value has no live caret, so nzk_ui must not paint one"
        );
    }

    #[test]
    fn the_preview_selector_is_host_owned_like_the_other_preview_inputs() {
        let mut form = FormState::default();
        assert_eq!(field("#previewSelector").map(|f| f.kind), Some(Kind::Text));
        // `open()` prefill, `preview.js:183-185`.
        form.prefill_preview_selector(Some("n:7"));
        assert_eq!(form.preview_selector, "n:7");
        let facts = form.facts().expect("a prefilled selector is painted");
        assert_eq!(facts["input"]["id"], "#previewSelector");
        assert_eq!(facts["input"]["value"], "n:7");
        assert_eq!(facts["focused"], Value::Null);
        // A press focuses the box and starts an edit at the caret end, exactly
        // like `#previewTo`; typing lands in the same one slot the facts paint.
        assert!(form.click("#previewSelector"));
        assert_eq!(form.edit.as_ref().unwrap().value(), "n:7");
        for _ in 0..3 {
            form.key(Key::Backspace);
        }
        for c in "n:42".chars() {
            form.key(Key::Char(c));
        }
        let facts = form.facts().unwrap();
        assert_eq!(facts["focused"], "#previewSelector");
        assert_eq!(facts["input"]["value"], "n:42");
        assert_eq!(
            facts["input"]["caret"], 4,
            "a live edit publishes the caret the input must scroll to"
        );
        form.key(Key::Enter);
        assert_eq!(form.preview_selector, "n:42", "Enter commits the typed selector");
        assert!(!form.is_active());
        // Escape is a cancel for the typing, the existing text-box contract.
        form.click("#previewSelector");
        form.key(Key::Char('x'));
        assert_eq!(form.key(Key::Escape), KeyOutcome::Changed);
        assert_eq!(form.preview_selector, "n:42");
        assert!(!form.is_active());
    }

    #[test]
    fn an_empty_preview_selector_publishes_nothing_and_never_leaks_into_other_fields() {
        let mut form = FormState::default();
        form.prefill_preview_selector(None);
        assert_eq!(form.preview_selector, "");
        assert_eq!(form.facts(), None, "the shipped document stays byte-identical");
        // The selector is its own slot: typing it does not write #previewTo.
        form.click("#previewSelector");
        for c in "n:1".chars() {
            form.key(Key::Char(c));
        }
        form.key(Key::Enter);
        assert_eq!(form.preview_selector, "n:1");
        assert_eq!(form.preview_to, "");
        assert_eq!(form.modal_path, "");
    }

    #[test]
    fn ns_select_uses_the_graph_namespaces_and_commits_the_value() {
        let mut form = FormState::default();
        assert_eq!(field("#nsSelect").map(|f| f.kind), Some(Kind::Select));
        // The rows are CONTENT supplied by the host (the graph's namespaces),
        // with the reference's count on the "All namespaces" row.
        form.set_ns_options(vec![
            (String::new(), "All namespaces (2)".to_string()),
            ("NZK.A".to_string(), "NZK.A".to_string()),
            ("NZK.B".to_string(), "NZK.B".to_string()),
        ]);
        form.set_value("#nsSelect", "NZK.A");
        form.click("#nsSelect");
        let menu = form.menu.as_ref().expect("menu open");
        assert_eq!(menu.id, "#nsSelect");
        assert_eq!(menu.options.len(), 3, "all namespaces + the graph's two");
        assert_eq!(menu.options[0].1, "All namespaces (2)");
        assert_eq!(menu.active, 1, "the current filter value is highlighted");
        // Down + Enter commits the next namespace into the form's one value.
        form.key(Key::Down);
        form.key(Key::Enter);
        assert_eq!(form.namespace, "NZK.B");
        assert_eq!(form.current("#nsSelect"), "NZK.B");
        // The committed value is published for the nzk_ui as a normal field.
        assert_eq!(form.facts().is_some(), false, "a committed select is not a paint fact");
    }

    #[test]
    fn a_click_away_closes_the_menu() {
        let mut form = FormState::default();
        form.click("#reorgMode");
        assert!(form.is_active());
        form.close();
        assert!(!form.is_active());
        assert_eq!(form.facts(), None);
    }

    #[test]
    fn preview_op_uses_the_engine_supplied_options() {
        let mut form = FormState::default();
        form.set_preview_ops(vec![
            ("rename".into(), "Rename".into()),
            ("move".into(), "Move".into()),
        ]);
        form.set_value("#previewOp", "move");
        form.click("#previewOp");
        let menu = form.menu.as_ref().unwrap();
        assert_eq!(menu.options.len(), 2);
        assert_eq!(menu.active, 1, "current value is pre-highlighted");
        form.key(Key::Up);
        form.key(Key::Enter);
        assert_eq!(form.preview_op, "rename");
    }

    #[test]
    fn the_facts_describe_the_open_menu_and_the_typed_value() {
        let mut form = FormState::default();
        form.click("#reorgMode");
        form.key(Key::Down);
        let facts = form.facts().expect("menu open publishes facts");
        assert_eq!(facts["focused"], "#reorgMode");
        assert_eq!(facts["menu"]["id"], "#reorgMode");
        assert_eq!(facts["menu"]["active"], 1);
        assert_eq!(facts["menu"]["options"][1]["selected"], true);
        assert_eq!(facts["input"], Value::Null);
        form.key(Key::Escape);
        form.click("#reorgDest");
        form.key(Key::Char('x'));
        let facts = form.facts().unwrap();
        assert_eq!(facts["menu"], Value::Null);
        assert_eq!(facts["input"]["id"], "#reorgDest");
        assert_eq!(facts["input"]["value"], "x");
    }

    #[test]
    fn a_checkbox_click_toggles_immediately_and_blurs() {
        let mut form = FormState::default();
        assert!(form.click("#reorgHeader"));
        assert!(!form.reorg.header);
        assert!(!form.is_active(), "a checkbox has no menu to keep open");
        assert!(form.click("#reorgHeader"));
        assert!(form.reorg.header);
    }

    #[test]
    fn the_modal_path_typing_is_kept_in_its_own_slot() {
        let mut form = FormState::default();
        form.click("#modalPathInput");
        for c in "/repo".chars() {
            form.key(Key::Char(c));
        }
        assert_eq!(form.value("#modalPathInput"), "/repo");
        form.key(Key::Enter);
        assert_eq!(form.modal_path, "/repo");
        assert_eq!(form.reorg.dest, "", "the picker path never leaks to reorg");
    }

    #[test]
    fn depth_input_commits_a_bounded_value_and_keeps_it_painted() {
        let mut form = FormState::default();
        assert!(form.click("#depthSelect"));
        form.key(Key::Backspace);
        for c in "99999999".chars() {
            form.key(Key::Char(c));
        }
        assert_eq!(form.value("#depthSelect"), "99999999");
        assert_eq!(form.key(Key::Enter), KeyOutcome::Changed);
        assert_eq!(form.depth, DEPTH_MAX);
        let facts = form.facts().unwrap();
        assert_eq!(facts["input"]["id"], "#depthSelect");
        assert_eq!(facts["input"]["value"], "9999999");
    }

    #[test]
    fn text_fields_support_cursor_movement_and_replace_all() {
        let mut form = FormState::default();
        form.set_value("#modalPathInput", "/repo");
        form.click("#modalPathInput");
        form.key(Key::Home);
        form.key(Key::Right);
        form.key(Key::Char('x'));
        assert_eq!(form.value("#modalPathInput"), "/xrepo");
        form.key(Key::SelectAll);
        form.key(Key::Char('n'));
        assert_eq!(form.value("#modalPathInput"), "n");
    }

    /// Enter in `#previewTo` is the preview run gesture, one-shot; the target
    /// box alone carries it, and Run commits typing first.
    #[test]
    fn enter_in_the_preview_target_requests_a_run_once() {
        let mut form = FormState::default();
        form.click("#previewTo");
        for c in "Plus".chars() {
            form.key(Key::Char(c));
        }
        assert!(!form.take_preview_run(), "typing is not a run");
        assert_eq!(form.key(Key::Enter), KeyOutcome::Changed);
        assert!(form.take_preview_run(), "Enter in #previewTo asks for a run");
        assert!(!form.take_preview_run(), "the request is one-shot");
        assert_eq!(form.preview_to, "Plus");

        // Enter in any other box never runs the preview.
        form.click("#previewSelector");
        form.key(Key::Enter);
        assert!(!form.take_preview_run());
        assert_eq!(form.preview_selector, "");
        // Run while the target box is focused commits the typing (browser blur).
        form.click("#previewTo");
        for c in "Renamed".chars() {
            form.key(Key::Char(c));
        }
        assert!(form.commit_focused());
        assert_eq!(form.preview_to, "PlusRenamed");
        assert!(!form.is_active());
        assert!(!form.take_preview_run(), "a commit alone is not an Enter");
    }
}
