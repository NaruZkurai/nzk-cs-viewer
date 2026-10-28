//! W-A2: the wire between the quick-open core and the DOM.
//!
//! [`crate::quickopen`] is a complete, tested model. This module is the
//! connection, and it owns exactly three things:
//!
//! 1. **State**: [`QuickOpenView`] — whether the palette is open, which mode it
//!    is in, the query, the composed rows and the selection. It is the only
//!    place the app remembers what the user asked for.
//! 2. **Projection**: [`QuickOpenView::facts`] turns that state plus a real
//!    [`Graph`] into the *exact* JSON shape `nzk_ui::dom::QuickOpenFacts`
//!    deserializes.
//! 3. **Honesty**: absence is preserved. `facts()` returns `Option<Value>`, and
//!    `None` — the palette has never been opened, or is closed — makes the
//!    caller **omit the key entirely**. That is what keeps the default paint
//!    byte-identical: `serde_json::json!` serialises `None` as `null`, so a
//!    `"quickopen": null` would be a *different document* from an absent key and
//!    would take the "present but null" path in the DOM layer. The caller must
//!    use [`insert_facts`] (or `Map::insert` conditionally) rather than
//!    `json!({ "quickopen": view.facts(..) })`.
//!
//! ## Why the `state` field is carried and not re-derived
//!
//! Like [`crate::find_wire::FindView`], the honest state is serialised as a
//! *value* so a test can assert the distinction survived the trip. The palette's
//! states that all look like "no rows" if flattened — `closed`, `noquery`,
//! `nohits` — each carry different copy and a different note-bar visibility, so
//! collapsing them would make a rendering bug indistinguishable from a correct
//! empty list.
//!
//! ## Module registration
//!
//! `app/src/app.rs` (the module list) is owned by the Lead, and `main.rs` lists
//! its own modules, so neither can be touched here. This file therefore declares
//! the core itself with a `#[path]` include, exactly as `app.rs:40-41` declares
//! `nzk_ui`:
//!
//! ```ignore
//! #[path = "quickopen.rs"]
//! mod quickopen;
//! ```
//!
//! That makes the pair self-registering: whoever adds `mod quickopen_wire;` (one
//! line) gets the core with it, and `cargo test --bin native-csviewer` compiles
//! and runs both test modules in the meantime.

#[path = "quickopen.rs"]
mod quickopen;

use quickopen::{
    self as qo, Mode, NavEntry, Row, Seed, Selection, Source,
};

/// The activation decision, re-exported so a host can name it without reaching
/// into the private core module (`quickopen` is declared with `#[path]` here and
/// is not visible as `crate::quickopen`).
pub use quickopen::Open;

use serde_json::{json, Map, Value};

use crate::model::Graph;

/// The palette's remembered state.
///
/// Deliberately *not* a cached `Vec<Row>` only: `open`/`closed` and `mode` are
/// independent of the rows (`a closed palette still remembers its mode`), and
/// `seed_query` is remembered separately from the live input because
/// quickopen.js:311 uses `seedQuery` — not the input value — to choose the
/// empty-list copy.
#[derive(Clone, Debug)]
pub struct QuickOpenView {
    open: bool,
    mode: Mode,
    /// The live input text.
    query: String,
    /// `seedQuery` (quickopen.js:36): the query the current list was produced
    /// for. Distinct from `query` in the reference — `query` is what the input
    /// holds, `seedQuery` is what the last render was based on — and the
    /// distinction is observable in the empty-list copy.
    seed_query: String,
    /// The composed, ranked rows for `seed_query` in `mode`.
    rows: Vec<Row>,
    selection: Selection,
    /// The note sentence for the current list, or `None` to hide the bar.
    note: Option<String>,
    /// `CsNav` history, the seed for an empty palette.
    history: Vec<NavEntry>,
}

impl Default for QuickOpenView {
    fn default() -> Self {
        QuickOpenView {
            open: false,
            mode: Mode::Files,
            query: String::new(),
            seed_query: String::new(),
            rows: Vec::new(),
            selection: Selection::default(),
            note: None,
            history: Vec::new(),
        }
    }
}

impl QuickOpenView {
    /// A closed palette in files mode: the honest "never used" state.
    pub fn new() -> QuickOpenView {
        QuickOpenView::default()
    }

    /// Is the palette showing? `isOpen()` (quickopen.js:449).
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// `mode()` (quickopen.js:524).
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The live input text.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// `rows()` (quickopen.js:522) — a clone, as the reference's `rows.slice()`.
    pub fn rows(&self) -> Vec<Row> {
        self.rows.clone()
    }

    /// The selected index, or `None` for the `sel = -1` sentinel.
    pub fn selected(&self) -> Option<usize> {
        self.selection.index()
    }

    /// Replace the remembered navigation history (the `CsNav` seed).
    pub fn set_history(&mut self, history: Vec<NavEntry>) {
        self.history = history;
    }

    /// Type into the input — the `input` event (quickopen.js:172-175), which
    /// records `seedQuery` and schedules a search. The debounce timer itself is
    /// the host's; this only records the text.
    pub fn set_query(&mut self, query: &str) {
        self.query.clear();
        self.query.push_str(query);
    }

    /// Append one typed character.
    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
    }

    /// Backspace.
    pub fn pop_char(&mut self) {
        self.query.pop();
    }

    /// Run the search for the current input and record the rows — `runSearch`
    /// (quickopen.js:262-280) without the debounce.
    ///
    /// The reference short-circuits on an empty query (`if (!api || !q) {
    /// setRows([]) … }`, quickopen.js:266) *after* trimming (265). Trimming
    /// matters: a query of spaces is the empty query, so it clears the list
    /// rather than searching for spaces.
    pub fn run_search(&mut self, graph: &Graph) {
        let q = self.query.trim();
        if q.is_empty() {
            self.seed_query = self.query.clone();
            self.rows.clear();
            self.selection = Selection::set_rows(0);
            // `seedQuery ? 'No matches.' : 'Type to search…'` — the empty text
            // is chosen from the *untrimmed* seed query (quickopen.js:311). It is
            // projected by `facts_value`, which always carries the key.
            self.note = None;
            return;
        }
        self.seed_query = self.query.clone();
        let want_symbols = self.mode.want_symbols();
        let composed = qo::query_graph(graph, q, self.mode);
        // `if (!list.length) setNote('No files or symbols match “q”.'); else
        // clearNote();` (quickopen.js:271-272).
        self.note = if composed.is_empty() {
            Some(qo::no_match_note(q))
        } else {
            None
        };
        self.rows = composed;
        self.selection = Selection::set_rows(self.rows.len());
    }

    /// Run a search for an explicit query, returning the rows — the `query(q,
    /// m)` test hook (quickopen.js:452-459).
    pub fn query_now(&mut self, q: &str, mode: Option<Mode>, graph: &Graph) -> Vec<Row> {
        if let Some(m) = mode {
            self.mode = m;
        }
        self.set_query(q);
        self.run_search(graph);
        self.rows()
    }

    /// `open(m, initialQuery)` — quickopen.js:427-439.
    ///
    /// `seed()`'s three-way branch is honoured: a non-empty query searches, an
    /// empty query shows recents, and an empty query with no recents shows the
    /// "Type to search" empty text with **no** note (quickopen.js:420).
    pub fn open(&mut self, mode: Mode, initial_query: &str, graph: &Graph) -> Vec<Row> {
        self.mode = mode;
        self.open = true;
        self.set_query(initial_query);
        match qo::seed(initial_query, &self.history) {
            Seed::Query(q) => {
                self.seed_query = q.clone();
                // `runSearch(seedQuery, mode === 'symbols')` (quickopen.js:415).
                let want_symbols = self.mode.want_symbols();
                let composed = qo::query_graph(graph, &q, self.mode);
                self.note = if composed.is_empty() {
                    Some(qo::no_match_note(&q))
                } else {
                    None
                };
                let _ = want_symbols;
                self.rows = composed;
            }
            Seed::Recents(rows) => {
                self.seed_query = String::new();
                self.note = qo::seed("", &self.history).note().map(|s| s.to_string());
                self.rows = rows;
            }
            Seed::EmptyRecents => {
                self.seed_query = String::new();
                self.note = None;
                self.rows.clear();
            }
        }
        self.selection = Selection::set_rows(self.rows.len());
        self.rows()
    }

    /// `close()` — quickopen.js:441-447. The rows are kept, as the reference
    /// keeps them (it only clears `shown` and drops the in-flight response), so
    /// reopening restores what the user saw.
    pub fn close(&mut self) {
        self.open = false;
    }

    /// `move(delta)` (quickopen.js:355-366) over the current rows.
    pub fn move_by(&mut self, delta: i64) {
        self.selection = self.selection.moved(delta, self.rows.len());
    }

    /// `activate(i)` (quickopen.js:384-389): the row Enter opens, closing the
    /// palette first as the reference does.
    pub fn activate(&mut self, i: usize) -> Option<qo::Open> {
        let row = self.rows.get(i)?;
        let decision = qo::open_row(row);
        self.open = false;
        Some(decision)
    }

    /// `activate(sel)` — the Enter key's own form, which opens nothing when the
    /// sentinel selection is in force.
    pub fn activate_selected(&mut self) -> Option<qo::Open> {
        let i = self.selection.enter()?;
        self.activate(i)
    }

    /// The note text, or `None` when the bar must stay hidden — `setNote`
    /// (quickopen.js:291-297: an empty text also adds the `hidden` class).
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref().filter(|s| !s.is_empty())
    }

    /// The honest state name, computed from the model — never from emptiness.
    ///
    /// * `closed` — `shown` is false (quickopen.js:449). The DOM must be left
    ///   **untouched**, which is why [`QuickOpenView::facts`] returns `None`.
    /// * `noquery` — open with nothing typed: the recents/`Type to search` state.
    /// * `nohits` — a real query that matched nothing.
    /// * `hits` — rows are showing.
    pub fn state(&self) -> &'static str {
        if !self.open {
            return "closed";
        }
        if self.rows.is_empty() && self.seed_query.trim().is_empty() {
            return "noquery";
        }
        if self.rows.is_empty() {
            return "nohits";
        }
        "hits"
    }

    /// The rows as the wire shape the DOM consumes.
    ///
    /// Every field is read off [`Row`], whose methods carry the reference's own
    /// fallback chains and formatting rules — this function is a transport and
    /// never invents copy.
    fn rows_value(&self) -> Vec<Value> {
        self.rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                json!({
                    // `row.setAttribute('data-i', String(i))` — the click target
                    // (quickopen.js:322).
                    "index": i,
                    // `kindOf(r)` (quickopen.js:299-303).
                    "kind": r.kind_of(),
                    // `name.textContent = r.label || r.name || r.id || r.file`
                    // (quickopen.js:331).
                    "label": r.display_label(),
                    // The extra `.cs-qo-full` span, present only when `full`
                    // differs from both label and name (quickopen.js:332-337).
                    "full": r.full,
                    "show_full": !r.full.is_empty()
                        && r.full != r.label
                        && r.full != r.name,
                    // `.cs-qo-loc` (quickopen.js:341).
                    "loc": r.loc(),
                    // `row.title` (quickopen.js:343-344).
                    "title": r.title(),
                    // `role="option"` / `aria-selected` (quickopen.js:321-323).
                    "selected": self.selection.is_selected(i),
                    // The node id an activation opens (quickopen.js:372).
                    "id": r.id,
                    "file": r.file,
                    "line": r.line,
                    "source": r.source.tag(),
                })
            })
            .collect()
    }

    /// The `ui.quickopen` payload, or `None` when the document must be left
    /// alone.
    ///
    /// `None` **only** when the palette is closed, which is the shipped state:
    /// the caller omits the key and the document is byte-identical to
    /// `index.html`. When it is open, the payload always carries the mode, the
    /// query, the rows and the honest state — including the honest empty states,
    /// whose copy ("Type to search files and symbols." / "No matches.") is real
    /// text that must reach the DOM.
    ///
    /// This is the function the Lead wires; see [`insert_facts`] for the
    /// omitted-key requirement.
    pub fn facts(&self, _graph: &Graph) -> Option<Value> {
        if !self.open {
            return None;
        }
        Some(self.facts_value())
    }

    /// The payload unconditionally, for callers that already know it is open.
    pub fn facts_value(&self) -> Value {
        // `MODE_LABEL` / `MODE_PLACEHOLDER` (quickopen.js:39-43, 430-431).
        let items = self.rows_value();
        let mut v = Map::new();
        v.insert("mode".to_string(), json!(self.mode_str()));
        v.insert("mode_label".to_string(), json!(self.mode.label()));
        v.insert("placeholder".to_string(), json!(self.mode.placeholder()));
        v.insert("query".to_string(), json!(self.query));
        v.insert("seed_query".to_string(), json!(self.seed_query));
        v.insert("state".to_string(), json!(self.state()));
        v.insert(
            "selected".to_string(),
            match self.selection.index() {
                Some(i) => json!(i),
                // `sel = -1` (quickopen.js:289) is the sentinel; JSON has a
                // real null, so the sentinel is carried as one rather than as a
                // magic -1 the renderer would have to know about.
                None => Value::Null,
            },
        );
        v.insert("rows".to_string(), json!(items));
        // The empty-list text (quickopen.js:311-313). Unconditional: the key set
        // of `ui.quickopen` is a contract the DOM struct deserializes by name, so
        // an `Option` that silently drops the key would make the field's presence
        // depend on whether the list happened to be empty. The copy itself still
        // branches on `seedQuery`, and the renderer only *uses* it when there are
        // no rows.
        v.insert(
            "empty_text".to_string(),
            json!(qo::empty_text(&self.seed_query)),
        );
        // The note. `hidden` is a separate boolean rather than "note is empty",
        // so the honesty is a value everywhere else in the pipeline too.
        v.insert(
            "note".to_string(),
            json!(self.note().unwrap_or("")),
        );
        v.insert(
            "note_hidden".to_string(),
            json!(self.note().is_none()),
        );
        // The header hint and the overlay's accessible name (quickopen.js:142,
        // 121) travel with the payload so the DOM layer holds no copy of them.
        v.insert("hint".to_string(), json!(qo::HINT));
        v.insert("aria_label".to_string(), json!(qo::ARIA_LABEL));
        Value::Object(v)
    }

    /// `'symbols'` / `'files'` — the reference's string form of the mode.
    pub fn mode_str(&self) -> &'static str {
        match self.mode {
            Mode::Symbols => "symbols",
            Mode::Files => "files",
        }
    }
}

/// Does the payload's `rows` array have no entries? (Read from the built map so
/// the "empty" decision is made once, on the value that ships.)
fn items_is_empty(v: &Map<String, Value>) -> bool {
    v.get("rows")
        .and_then(|r| r.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true)
}

/// Insert a facts payload into a JSON object **only when it is `Some`**.
///
/// This is the whole omitted-key contract in one function: `json!({ "quickopen":
/// view.facts(g) })` would emit `"quickopen": null`, and `serde_json::Value`
/// deserialises that into `Option<Value>::None` only if the field is *absent* —
/// `null` is a present value. Keeping the insert conditional means the shipped
/// document has no `quickopen` key at all.
///
/// Usage from the app's `ui` builder:
///
/// ```ignore
/// let mut ui = Map::new();
/// crate::quickopen_wire::insert_facts(&mut ui, "quickopen", view.facts(graph));
/// ```
pub fn insert_facts(map: &mut Map<String, Value>, key: &str, facts: Option<Value>) {
    if let Some(v) = facts {
        map.insert(key.to_string(), v);
    }
}

/// Build a `ui` object containing only the surfaces that are actually present.
///
/// A convenience for the app side and for tests: every argument that is `None`
/// contributes no key. `None`-only input yields an empty object, never a set of
/// nulls.
pub fn ui_object(pairs: &[(&str, Option<Value>)]) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        insert_facts(&mut m, k, v.clone());
    }
    Value::Object(m)
}

/// The default `ui` value the shipped document sends — every surface absent.
pub fn closed_ui() -> Value {
    ui_object(&[("quickopen", None)])
}

/// Convert a graph node to a palette row — the public form of
/// `Row::from_search_node`, for a host that builds candidates itself.
pub fn row_from_node(n: &crate::model::Node) -> Option<Row> {
    Row::from_search_node(n)
}

/// A `/api/find`-shaped payload's rows, for a host with real find results.
pub fn find_rows(
    definitions: Vec<Row>,
    implementations: Vec<Row>,
    usages: Vec<Row>,
) -> qo::FindRows {
    qo::FindRows {
        definitions,
        implementations,
        usages,
    }
}

/// The source tag for a row, so a host can label rows it built itself.
pub fn source_tag(s: Source) -> &'static str {
    s.tag()
}

// ----------------------------------------------------------- host key routing

/// What one key event asked the palette to do — the host-facing form of
/// [`qo::Key`].
///
/// The host translates a real `winit::Key` into the reference's
/// `(key, ctrl, meta, shift, alt)` tuple, calls [`route_key`], and acts on the
/// answer. Keeping the *decision* here means the whole keymap — including the
/// `!shift && !alt` rule on the two open chords — lives in one place and is
/// testable without a window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyAction {
    /// Open the palette in files mode (`Ctrl/Cmd+P`).
    OpenFiles,
    /// Open the palette in symbols mode (`Ctrl/Cmd+T`).
    OpenSymbols,
    /// Close it (`Escape`).
    Close,
    /// Move the selection by this many rows.
    Move(i64),
    /// Run the search for the current input.
    Search,
    /// Activate the selected row (`Enter`).
    Activate,
    /// Append a character to the query.
    Type(char),
    /// Delete the last character of the query.
    Backspace,
}

/// Translate a real key event into the palette action it means — **or `None` to
/// let the key through untouched**.
///
/// This is the single decision point the app's key path uses, and it is
/// deliberately the *reference's* order (quickopen.js:463-491):
///
/// 1. The two global chords are tested first and work whether or not the
///    palette is up — and only with `ctrl`/`meta` and **without** shift or alt,
///    so `Ctrl+Shift+P` falls through rather than being swallowed.
/// 2. Every other key is ignored while the palette is down (`if (!shown)
///    return`), which is what stops a closed palette from eating Escape and the
///    arrow keys.
/// 3. While it is up, the navigation keys are the reference's own set, and a
///    printable character with no modifier becomes `Type`. That last case is the
///    one the reference gets from the `<input>` element's own `input` event
///    (quickopen.js:172-175) rather than from `keydown` — a real DOM gives it for
///    free, a native host has to spell it out. Modifier combos are **not**
///    typed, so `Ctrl+C` is not a `c` in the query.
pub fn route_key(
    key: &str,
    ctrl: bool,
    meta: bool,
    shift: bool,
    alt: bool,
    shown: bool,
) -> Option<KeyAction> {
    match qo::resolve_key(key, ctrl, meta, shift, alt, shown)? {
        qo::Key::OpenFiles => Some(KeyAction::OpenFiles),
        qo::Key::OpenSymbols => Some(KeyAction::OpenSymbols),
        qo::Key::Escape => Some(KeyAction::Close),
        qo::Key::Up => Some(KeyAction::Move(qo::step_for(qo::Key::Up))),
        qo::Key::Down => Some(KeyAction::Move(qo::step_for(qo::Key::Down))),
        qo::Key::PageUp => Some(KeyAction::Move(qo::step_for(qo::Key::PageUp))),
        qo::Key::PageDown => Some(KeyAction::Move(qo::step_for(qo::Key::PageDown))),
        // `Tab` is navigation, not focus traversal (quickopen.js:490).
        qo::Key::Tab { shift } => Some(KeyAction::Move(qo::step_for(qo::Key::Tab { shift }))),
        qo::Key::Enter => Some(KeyAction::Activate),
    }
}

/// The character a key event types into the palette, or `None` when the event is
/// not a printable, unmodified insertion.
///
/// Separate from [`route_key`] because the reference gets this from the
/// `<input>`'s own `input` event, not from its `keydown` handler: a native host
/// has to make the same call itself, and it is *not* the same set of keys.
/// `Backspace` is here rather than in [`route_key`] for the same reason — the
/// browser delivers it as an editing action on the focused field.
pub fn edit_key(key: &str, ctrl: bool, meta: bool, alt: bool, shown: bool) -> Option<KeyAction> {
    if !shown || ctrl || meta || alt {
        return None;
    }
    if key == "Backspace" {
        return Some(KeyAction::Backspace);
    }
    // Exactly one `char` and it is not a control character: `"a"` types, but
    // `"F2"`, `"Shift"` and a multi-character name do not.
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => Some(KeyAction::Type(c)),
        _ => None,
    }
}

/// Apply a [`KeyAction`] to a view, returning the row an activation opened (if
/// any). The host performs the open; the model decides.
///
/// `graph` is needed because typing re-runs the search — the reference's
/// debounced `runSearch` (quickopen.js:283-286), applied synchronously here
/// because this host has no timer and the core is fast enough for a keystroke.
pub fn apply_key(view: &mut QuickOpenView, action: KeyAction, graph: &Graph) -> Option<qo::Open> {
    match action {
        KeyAction::OpenFiles => {
            view.open(Mode::Files, "", graph);
            None
        }
        KeyAction::OpenSymbols => {
            view.open(Mode::Symbols, "", graph);
            None
        }
        KeyAction::Close => {
            view.close();
            None
        }
        KeyAction::Move(delta) => {
            view.move_by(delta);
            None
        }
        KeyAction::Search => {
            view.run_search(graph);
            None
        }
        KeyAction::Activate => view.activate_selected(),
        KeyAction::Type(c) => {
            view.push_char(c);
            view.run_search(graph);
            None
        }
        KeyAction::Backspace => {
            view.pop_char();
            view.run_search(graph);
            None
        }
    }
}

/// Build the `ui` object the shell consumes, with the palette's payload inserted
/// **only when it is showing**.
///
/// This is the one-liner the app's facts builder calls, and it is the whole
/// reason [`insert_facts`] exists: a closed palette contributes no key, so the
/// document stays byte-identical to `index.html`.
pub fn ui_with(view: &QuickOpenView, graph: &Graph) -> Value {
    ui_object(&[("quickopen", view.facts(graph))])
}

// ---------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, Graph, Node};
    use std::path::PathBuf;

    fn node(id: &str, label: &str, full: &str, file: &str, line: i64) -> Node {
        Node {
            id: id.to_string(),
            label: label.to_string(),
            full: full.to_string(),
            kind: "method".to_string(),
            ns: "N".to_string(),
            owner: "N.C".to_string(),
            file: file.to_string(),
            line,
            end_line: line,
            degree: 1,
            external: false,
            ..Default::default()
        }
    }

    /// A two-node graph: a symbol named `find` in `other.rs`, and a symbol that
    /// merely lives in `find.rs`.
    fn graph() -> Graph {
        Graph {
            repo: PathBuf::from("/tmp/fid-corpus"),
            nodes: vec![
                node("n1", "find", "N.find", "src/other.rs", 10),
                node("n2", "helper", "N.helper", "src/find.rs", 20),
                node("n3", "widget", "N.widget", "src/widget.rs", 30),
            ],
            edges: Vec::<Edge>::new(),
            state: serde_json::json!({}),
            skipped: Vec::new(),
            projects: Vec::new(),
        }
    }

    fn nav(node_id: &str, file: &str, line: i64, label: &str, mode: &str) -> NavEntry {
        NavEntry {
            node_id: node_id.to_string(),
            file: file.to_string(),
            label: label.to_string(),
            line,
            mode: mode.to_string(),
        }
    }

    // ------------------------------------------------------- absence contract

    /// THE CENTRAL HONESTY TEST. A fresh, never-opened palette yields `facts() ==
    /// None`, and the `ui` object built from it contains **no** `quickopen` key
    /// at all — not a null.
    #[test]
    fn closed_palette_omits_the_key_entirely_never_null() {
        let v = QuickOpenView::new();
        assert!(v.facts(&graph()).is_none(), "closed => no payload");

        let ui = ui_object(&[("quickopen", v.facts(&graph()))]);
        let obj = ui.as_object().expect("ui is an object");
        assert!(!obj.contains_key("quickopen"), "the key must be ABSENT");
        assert!(obj.is_empty(), "a closed palette contributes no keys");

        // Prove the failure mode this guards against: the naive json! spelling
        // DOES emit null, which is a present value, not an absent one.
        let naive = json!({ "quickopen": v.facts(&graph()) });
        assert!(
            naive.as_object().unwrap().contains_key("quickopen"),
            "json! spells None as a present null — this is exactly why insert is conditional"
        );
        assert!(naive["quickopen"].is_null());
        // And it is genuinely a different document from the omitted form.
        assert_ne!(naive, ui);
    }

    /// The shipped default really is `{}` — every surface absent.
    #[test]
    fn closed_ui_is_an_empty_object() {
        let ui = closed_ui();
        assert_eq!(ui, json!({}));
        assert_eq!(ui.to_string(), "{}");
    }

    /// After closing, the key disappears again — a reopen/close cycle returns the
    /// document to the shipped shape.
    #[test]
    fn closing_removes_the_key_again() {
        let g = graph();
        let mut v = QuickOpenView::new();
        assert!(v.facts(&g).is_none());
        v.open(Mode::Files, "find", &g);
        assert!(v.facts(&g).is_some(), "open => payload present");
        assert_eq!(v.state(), "hits");
        v.close();
        assert!(v.facts(&g).is_none(), "closed again => absent again");
        assert_eq!(v.state(), "closed");
    }

    /// `insert_facts` inserts on `Some` and leaves the map untouched on `None`.
    #[test]
    fn insert_facts_is_conditional() {
        let mut m = Map::new();
        insert_facts(&mut m, "quickopen", None);
        assert!(m.is_empty());
        insert_facts(&mut m, "quickopen", Some(json!({"state":"hits"})));
        assert_eq!(m.len(), 1);
        assert_eq!(m["quickopen"]["state"], "hits");
    }

    // ---------------------------------------------------------------- states

    /// The three open states are distinguishable values on the wire.
    #[test]
    fn states_are_distinct_values_not_emptiness() {
        let g = graph();
        let mut v = QuickOpenView::new();
        assert_eq!(v.state(), "closed");

        // Open with recents available: `noquery` with real rows.
        v.set_history(vec![nav("n1", "src/other.rs", 10, "find", "doc")]);
        v.open(Mode::Files, "", &g);
        assert_eq!(v.state(), "hits", "recents are rows");
        let f = v.facts_value();
        assert_eq!(f["state"], "hits");
        assert!(f["note_hidden"] == json!(false), "recents note shows");
        assert_eq!(f["note"], "Recent documents. Type to search files and symbols.");

        // Open with NO history: `noquery`, still no note (quickopen.js:420).
        let mut v2 = QuickOpenView::new();
        v2.open(Mode::Files, "", &g);
        assert_eq!(v2.state(), "noquery");
        let f2 = v2.facts_value();
        assert_eq!(f2["state"], "noquery");
        assert_eq!(f2["note_hidden"], json!(true), "the empty seed hides the note");
        assert_eq!(f2["empty_text"], "Type to search files and symbols.");
        assert_eq!(f2["rows"], json!([]));

        // A real query that matches nothing: `nohits` WITH a note.
        let mut v3 = QuickOpenView::new();
        v3.open(Mode::Files, "zzzzz", &g);
        assert_eq!(v3.state(), "nohits");
        let f3 = v3.facts_value();
        assert_eq!(f3["state"], "nohits");
        assert_eq!(f3["note_hidden"], json!(false));
        assert_eq!(f3["empty_text"], "No matches.");
        assert_eq!(f3["note"], "No files or symbols match \u{201c}zzzzz\u{201d}.");
    }

    /// A whitespace-only query trims to the empty query (quickopen.js:265) — it
    /// does not search for spaces.
    #[test]
    fn whitespace_query_is_treated_as_empty() {
        let g = graph();
        let mut v = QuickOpenView::new();
        let rows = v.open(Mode::Files, "   ", &g);
        assert!(rows.is_empty());
        assert_eq!(v.state(), "noquery");
    }

    // ---------------------------------------------------------------- payload

    /// The payload carries the reference's own labels, placeholders and hints.
    #[test]
    fn payload_carries_mode_labels_placeholder_and_hint() {
        let g = graph();
        let mut v = QuickOpenView::new();
        v.open(Mode::Symbols, "find", &g);
        let f = v.facts_value();
        assert_eq!(f["mode"], "symbols");
        assert_eq!(f["mode_label"], "Symbols");
        assert_eq!(f["placeholder"], "Type a symbol name\u{2026}");
        assert_eq!(f["query"], "find");
        assert_eq!(f["hint"], "\u{2191}\u{2193} \u{00b7} Enter \u{00b7} Esc");
        assert_eq!(f["aria_label"], "Quick open");

        let mut v2 = QuickOpenView::new();
        v2.open(Mode::Files, "find", &g);
        let f2 = v2.facts_value();
        assert_eq!(f2["mode"], "files");
        assert_eq!(f2["mode_label"], "Files & symbols");
        assert_eq!(f2["placeholder"], "Type a file or symbol name\u{2026}");
    }

    /// Row 0 is selected on a fresh result set, and the `selected` flags are
    /// exactly one true at that index (quickopen.js:289).
    #[test]
    fn exactly_one_row_is_selected_and_selection_starts_at_zero() {
        let g = graph();
        let mut v = QuickOpenView::new();
        v.open(Mode::Files, "find", &g);
        let f = v.facts_value();
        assert_eq!(f["selected"], json!(0));
        let rows = f["rows"].as_array().unwrap();
        assert!(!rows.is_empty());
        let sel: Vec<bool> = rows
            .iter()
            .map(|r| r["selected"].as_bool().unwrap())
            .collect();
        assert_eq!(sel.iter().filter(|b| **b).count(), 1, "exactly one");
        assert!(sel[0], "index 0");

        // Moving updates the flags and never produces two selected rows.
        v.move_by(1);
        let f = v.facts_value();
        assert_eq!(f["selected"], json!(1));
        let rows = f["rows"].as_array().unwrap();
        let sel: Vec<bool> = rows
            .iter()
            .map(|r| r["selected"].as_bool().unwrap())
            .collect();
        assert_eq!(sel.iter().filter(|b| **b).count(), 1);
        assert!(sel[1]);
    }

    /// Row indices are the reference's `data-i` values, in order.
    #[test]
    fn row_indices_are_sequential_from_zero() {
        let g = graph();
        let mut v = QuickOpenView::new();
        v.open(Mode::Files, "find", &g);
        let f = v.facts_value();
        let rows = f["rows"].as_array().unwrap();
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r["index"], json!(i), "data-i");
        }
    }

    /// The row's rendered fields match the reference's rules, including the
    /// `show_full` span condition (quickopen.js:332).
    #[test]
    fn row_fields_match_the_render_rules() {
        let g = graph();
        let mut v = QuickOpenView::new();
        v.open(Mode::Files, "find", &g);
        let f = v.facts_value();
        let rows = f["rows"].as_array().unwrap();
        let top = &rows[0];
        assert_eq!(top["label"], "find", "label || name || id || file");
        assert_eq!(top["loc"], "src/other.rs:10");
        assert_eq!(top["title"], "N.find  \u{2014}  src/other.rs:10");
        assert_eq!(top["id"], "n1");
        assert_eq!(top["file"], "src/other.rs");
        assert_eq!(top["line"], json!(10));
        assert_eq!(top["source"], "search");
        assert_eq!(top["kind"], "method");
        assert_eq!(top["show_full"], json!(true), "full differs from label");
    }

    /// The name-hit-before-file-owner rule survives the trip to JSON: the symbol
    /// named `find` is row 0, the one that merely lives in `find.rs` is behind it.
    #[test]
    fn ranking_survives_the_projection() {
        let g = graph();
        let mut v = QuickOpenView::new();
        v.open(Mode::Files, "find", &g);
        let f = v.facts_value();
        let rows = f["rows"].as_array().unwrap();
        assert_eq!(rows[0]["id"], "n1", "the symbol NAMED find wins");
        let ids: Vec<&str> = rows.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"n2"), "the file-owner row is kept, just lower");
    }

    /// `activate` returns the host's decision and closes the palette first
    /// (quickopen.js:387).
    #[test]
    fn activate_closes_first_and_reports_the_decision() {
        let g = graph();
        let mut v = QuickOpenView::new();
        v.open(Mode::Files, "find", &g);
        let decision = v.activate_selected().expect("a row was selected");
        assert!(!v.is_open(), "activate closes the palette");
        match decision {
            qo::Open::Node { id } => assert_eq!(id, "n1"),
            other => panic!("expected a node open, got {other:?}"),
        }
        assert!(v.facts(&g).is_none(), "closed => absent again");
    }

    /// Activating a row out of range opens nothing rather than panicking.
    #[test]
    fn activate_out_of_range_is_none() {
        let g = graph();
        let mut v = QuickOpenView::new();
        v.open(Mode::Files, "find", &g);
        assert_eq!(v.activate(999), None);
    }

    /// With no selection (empty list), Enter opens nothing.
    #[test]
    fn enter_with_an_empty_list_opens_nothing() {
        let g = graph();
        let mut v = QuickOpenView::new();
        v.open(Mode::Files, "zzzzz", &g);
        assert_eq!(v.selected(), None, "the -1 sentinel as a real null");
        let f = v.facts_value();
        assert_eq!(f["selected"], Value::Null);
        assert!(v.activate_selected().is_none());
    }

    /// The reference keeps its rows across a close (only `shown` changes), so a
    /// reopen restores the list.
    #[test]
    fn close_keeps_rows_for_a_reopen() {
        let g = graph();
        let mut v = QuickOpenView::new();
        let before = v.open(Mode::Files, "find", &g);
        v.close();
        assert!(!v.is_open());
        assert_eq!(v.rows().len(), before.len(), "rows survive the close");
        let after = v.open(Mode::Files, "find", &g);
        assert_eq!(after.len(), before.len());
    }

    /// Typing then backspacing to empty returns to the `noquery` state.
    #[test]
    fn query_editing_round_trips_to_noquery() {
        let g = graph();
        let mut v = QuickOpenView::new();
        v.open(Mode::Files, "", &g);
        assert_eq!(v.state(), "noquery");
        v.set_query("find");
        v.run_search(&g);
        assert_eq!(v.state(), "hits");
        v.pop_char();
        v.pop_char();
        v.pop_char();
        v.pop_char();
        assert_eq!(v.query(), "");
        v.run_search(&g);
        assert_eq!(v.state(), "noquery");
    }

    /// The mode is remembered independently of the rows.
    #[test]
    fn mode_is_independent_state() {
        let g = graph();
        let mut v = QuickOpenView::new();
        v.open(Mode::Symbols, "find", &g);
        assert_eq!(v.mode(), Mode::Symbols);
        v.close();
        assert_eq!(v.mode(), Mode::Symbols, "closing does not reset the mode");
        v.open(Mode::Files, "find", &g);
        assert_eq!(v.mode(), Mode::Files);
    }

    /// `query_now` sets the mode when given, mirroring the `query(q, m)` hook
    /// (quickopen.js:452-459).
    #[test]
    fn query_now_honours_an_optional_mode() {
        let g = graph();
        let mut v = QuickOpenView::new();
        v.query_now("find", Some(Mode::Symbols), &g);
        assert_eq!(v.mode(), Mode::Symbols);
        v.query_now("find", None, &g);
        assert_eq!(v.mode(), Mode::Symbols, "no mode argument leaves it alone");
    }

    // ------------------------------------------------------------- shape lock

    /// Lock the wire shape: the exact key set, so a rename cannot pass silently
    /// and the DOM struct the Lead writes is checkable against this list.
    #[test]
    fn wire_shape_is_exactly_the_documented_keys() {
        let g = graph();
        let mut v = QuickOpenView::new();
        v.open(Mode::Files, "find", &g);
        let f = v.facts_value();
        let obj = f.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "aria_label",
                "empty_text",
                "hint",
                "mode",
                "mode_label",
                "note",
                "note_hidden",
                "placeholder",
                "query",
                "rows",
                "seed_query",
                "selected",
                "state",
            ],
            "the ui.quickopen key set is a contract"
        );
        // A row's keys are a contract too.
        let row = &obj["rows"].as_array().unwrap()[0];
        let mut rkeys: Vec<&str> = row.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        rkeys.sort_unstable();
        assert_eq!(
            rkeys,
            vec![
                "file", "full", "id", "index", "kind", "line", "loc", "selected", "show_full",
                "source", "title", "label"
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>(),
            "the row key set is a contract"
        );
    }

    /// The payload serialises to JSON without loss (it round-trips).
    #[test]
    fn payload_round_trips_through_json() {
        let g = graph();
        let mut v = QuickOpenView::new();
        v.open(Mode::Files, "find", &g);
        let f = v.facts_value();
        let s = serde_json::to_string(&f).unwrap();
        let back: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back, f);
    }
    // ------------------------------------------------------ host key routing

    /// THE CHORD THE WHOLE TASK IS ABOUT: `Ctrl+P` opens the palette in files
    /// mode and `Ctrl+T` in symbols mode, and both work while it is CLOSED —
    /// which is the only state in which a user ever presses them.
    ///
    /// The reference requires `!shiftKey && !altKey` for both
    /// (quickopen.js:465-476), so `Ctrl+Shift+P` is asserted to fall through
    /// rather than being swallowed: that is a real key that other bindings may
    /// want, and swallowing it would be a bug the JS does not have.
    #[test]
    fn ctrl_p_and_ctrl_t_open_the_palette_in_the_right_mode() {
        let g = graph();

        for (key, mode, label) in [
            ("p", Mode::Files, "Files & symbols"),
            ("P", Mode::Files, "Files & symbols"),
            ("t", Mode::Symbols, "Symbols"),
            ("T", Mode::Symbols, "Symbols"),
        ] {
            let mut view = QuickOpenView::new();
            assert!(!view.is_open(), "starts closed");
            let action = route_key(key, true, false, false, false, false)
                .unwrap_or_else(|| panic!("Ctrl+{key} must be a palette chord"));
            apply_key(&mut view, action, &g);
            assert!(view.is_open(), "Ctrl+{key} must open the palette");
            assert_eq!(view.mode(), mode, "Ctrl+{key} picks {mode:?}");
            assert_eq!(view.facts_value()["mode_label"], json!(label));
        }

        // Cmd on macOS (`metaKey`) is the same accelerator.
        let mut view = QuickOpenView::new();
        let action = route_key("p", false, true, false, false, false).expect("Cmd+P opens");
        apply_key(&mut view, action, &g);
        assert!(view.is_open());

        // ... but shift/alt are NOT part of the chord.
        let mut view = QuickOpenView::new();
        assert_eq!(route_key("p", true, false, true, false, false), None, "Ctrl+Shift+P");
        assert_eq!(route_key("p", true, false, false, true, false), None, "Ctrl+Alt+P");
        assert_eq!(route_key("t", true, false, true, false, false), None, "Ctrl+Shift+T");
        assert!(!view.is_open());
    }

    /// While the palette is DOWN every non-chord key falls through: a closed
    /// palette must not eat Escape or the arrows (quickopen.js:477).
    #[test]
    fn a_closed_palette_swallows_nothing_but_its_own_chords() {
        for key in [
            "Escape", "ArrowDown", "ArrowUp", "PageDown", "PageUp", "Enter", "Tab",
        ] {
            assert_eq!(
                route_key(key, false, false, false, false, false),
                None,
                "{key} must pass through a closed palette"
            );
        }
        // And an unmodified letter is never a navigation action.
        assert_eq!(route_key("a", false, false, false, false, false), None);
        assert_eq!(route_key("a", false, false, false, false, true), None);
    }

    /// While it is UP, the reference's navigation set is routed and step sizes
    /// are the reference's own: 1 for the arrows and Tab, 10 for PageUp/Down
    /// (quickopen.js:483-490).
    #[test]
    fn an_open_palette_routes_navigation_with_the_reference_step_sizes() {
        assert_eq!(route_key("ArrowDown", false, false, false, false, true), Some(KeyAction::Move(1)));
        assert_eq!(route_key("ArrowUp", false, false, false, false, true), Some(KeyAction::Move(-1)));
        assert_eq!(route_key("PageDown", false, false, false, false, true), Some(KeyAction::Move(10)));
        assert_eq!(route_key("PageUp", false, false, false, false, true), Some(KeyAction::Move(-10)));
        assert_eq!(route_key("Tab", false, false, false, false, true), Some(KeyAction::Move(1)));
        assert_eq!(route_key("Tab", false, false, true, false, true), Some(KeyAction::Move(-1)));
        assert_eq!(route_key("Escape", false, false, false, false, true), Some(KeyAction::Close));
        assert_eq!(route_key("Enter", false, false, false, false, true), Some(KeyAction::Activate));
    }

    /// `Escape` closes the palette and the key disappears again — the
    /// reopen/close cycle returns the document to the shipped shape.
    #[test]
    fn escape_closes_and_the_key_goes_away_again() {
        let g = graph();
        let mut view = QuickOpenView::new();
        apply_key(&mut view, route_key("p", true, false, false, false, false).unwrap(), &g);
        apply_key(&mut view, KeyAction::Type('f'), &g);
        assert!(view.facts(&g).is_some(), "open => key present");
        let rows_before = view.rows().len();
        assert!(rows_before > 0, "a real query produced rows");

        apply_key(&mut view, KeyAction::Close, &g);
        assert!(!view.is_open());
        assert!(view.facts(&g).is_none(), "closed => key absent again");
        // The rows survive, as the reference keeps them (quickopen.js:441-447).
        assert_eq!(view.rows().len(), rows_before, "rows survive the close");
    }

    /// Editing: typing flows into the query and re-searches, Backspace removes
    /// one character, and neither happens while the palette is closed or with a
    /// command modifier held.
    #[test]
    fn typing_and_backspace_edit_the_query_only_when_open() {
        let g = graph();
        let mut view = QuickOpenView::new();

        // Closed: nothing types.
        assert_eq!(edit_key("f", false, false, false, false), None);
        // Open: characters type.
        assert_eq!(edit_key("f", false, false, false, true), Some(KeyAction::Type('f')));
        assert_eq!(edit_key("Backspace", false, false, false, true), Some(KeyAction::Backspace));
        // A named key is NOT a character; a chord is not typing either.
        assert_eq!(edit_key("F2", false, false, false, true), None);
        assert_eq!(edit_key("ArrowDown", false, false, false, true), None);
        assert_eq!(edit_key("c", true, false, false, true), None, "Ctrl+C is not a 'c'");

        apply_key(&mut view, KeyAction::OpenFiles, &g);
        for c in "find".chars() {
            apply_key(&mut view, KeyAction::Type(c), &g);
        }
        assert_eq!(view.query(), "find");
        // The search really ran: the symbol named `find` is the top row.
        let top = view.rows_value();
        assert_eq!(top[0]["id"], json!("n1"), "typing re-runs the search");
        assert_eq!(view.state(), "hits");

        apply_key(&mut view, KeyAction::Backspace, &g);
        assert_eq!(view.query(), "fin");
        for _ in 0..3 {
            apply_key(&mut view, KeyAction::Backspace, &g);
        }
        assert_eq!(view.query(), "");
        assert_eq!(view.state(), "noquery", "backspacing to empty returns to noquery");
    }

    /// Moving the selection is the reference's `move(delta)`: it wraps in both
    /// directions over the current rows (quickopen.js:355-366).
    #[test]
    fn move_wraps_around_the_row_list() {
        let g = graph();
        let mut view = QuickOpenView::new();
        apply_key(&mut view, KeyAction::OpenFiles, &g);
        apply_key(&mut view, KeyAction::Type('f'), &g);
        let n = view.rows().len();
        assert!(n >= 2, "need at least two rows to test wrapping, got {n}");

        assert_eq!(view.selected(), Some(0));
        apply_key(&mut view, KeyAction::Move(1), &g);
        assert_eq!(view.selected(), Some(1));
        // Down past the end wraps to the top.
        for _ in 0..n {
            apply_key(&mut view, KeyAction::Move(1), &g);
        }
        assert_eq!(view.selected(), Some(1), "wrapped back to the same slot");
        // Up past the top wraps to the end.
        apply_key(&mut view, KeyAction::Move(-1), &g);
        assert_eq!(view.selected(), Some(0));
        apply_key(&mut view, KeyAction::Move(-1), &g);
        assert_eq!(view.selected(), Some(n - 1));
    }

    /// Enter activates the SELECTED row and reports the decision, closing the
    /// palette first (quickopen.js:384-389).
    #[test]
    fn enter_activates_the_selected_row() {
        let g = graph();
        let mut view = QuickOpenView::new();
        apply_key(&mut view, KeyAction::OpenFiles, &g);
        apply_key(&mut view, KeyAction::Type('f'), &g);
        let first_id = view.rows_value()[0]["id"].as_str().unwrap().to_string();

        let opened = apply_key(&mut view, KeyAction::Activate, &g).expect("a row is selected");
        assert!(!view.is_open(), "activation closes the palette");
        match opened {
            Open::Node { id } => assert_eq!(id, first_id),
            other => panic!("expected a node open, got {other:?}"),
        }
    }

    /// With no rows (`nohits`), Enter opens NOTHING rather than panicking or
    /// activating row 0 of an empty list.
    #[test]
    fn enter_with_no_rows_opens_nothing() {
        let g = graph();
        let mut view = QuickOpenView::new();
        apply_key(&mut view, KeyAction::OpenFiles, &g);
        for c in "zzzzz".chars() {
            apply_key(&mut view, KeyAction::Type(c), &g);
        }
        assert_eq!(view.state(), "nohits");
        assert_eq!(view.selected(), None);
        assert_eq!(apply_key(&mut view, KeyAction::Activate, &g), None);
        // The note is the core's own honest sentence, and it is not hidden.
        let f = view.facts_value();
        assert_eq!(f["note_hidden"], json!(false));
        assert_eq!(f["empty_text"], "No matches.");
    }

    /// THE `@frame` CONTRACT, at the wire layer: a closed palette contributes
    /// **no** `quickopen` key, so the shipped document is unchanged, and opening
    /// then closing returns it to exactly that state.
    #[test]
    fn a_closed_palette_adds_no_key_to_the_ui_object() {
        let g = graph();
        let mut view = QuickOpenView::new();

        let shut = ui_with(&view, &g);
        assert_eq!(shut, json!({}), "closed => the ui object is literally empty");
        assert_eq!(shut.to_string(), "{}");

        apply_key(&mut view, route_key("p", true, false, false, false, false).unwrap(), &g);
        apply_key(&mut view, KeyAction::Type('f'), &g);
        let up = ui_with(&view, &g);
        assert!(up.as_object().unwrap().contains_key("quickopen"));
        // The 13 keys are all there, and `closed` is not among the states.
        assert_eq!(up["quickopen"]["state"], "hits");
        assert_eq!(up["quickopen"].as_object().unwrap().len(), 13);

        apply_key(&mut view, KeyAction::Close, &g);
        assert_eq!(
            ui_with(&view, &g),
            json!({}),
            "closing must return the document to the shipped shape"
        );
    }

    /// The window's modifier state reaches the chord through the host's own
    /// translation, so `Ctrl` and `Cmd` are both accelerators and the unmodified
    /// canvas letters stay unmodified — the guard `has_command_modifier` is the
    /// app-side half of this contract.
    #[test]
    fn only_a_command_modifier_is_an_accelerator() {
        // Ctrl (Linux/Windows) and Cmd (macOS).
        assert!(route_key("p", true, false, false, false, false).is_some());
        assert!(route_key("p", false, true, false, false, false).is_some());
        // Neither on its own without a letter.
        assert_eq!(route_key("i", false, false, false, false, false), None);
        // Shift alone is not an accelerator, so `Shift+P` is not the chord.
        assert_eq!(route_key("p", false, false, true, false, false), None);
    }

    /// `Open::File` carries the id too, and `perform_quickopen` needs it; the
    /// line the core applied is `line || 1` (quickopen.js:376), preserved here.
    #[test]
    fn file_rows_carry_the_line_and_the_id_through_to_open() {
        let mut r = Row::default();
        r.id = "n7".to_string();
        r.file = "src/x.rs".to_string();
        r.line = 0;
        assert_eq!(
            qo::open_row(&r),
            Open::Node { id: "n7".to_string() },
            "an id wins even with a file present (quickopen.js:372)"
        );

        // File-only row: the line becomes 1, not 0.
        let mut r2 = Row::default();
        r2.file = "src/x.rs".to_string();
        r2.line = 0;
        match qo::open_row(&r2) {
            Open::File { file, line, id } => {
                assert_eq!(file, "src/x.rs");
                assert_eq!(line, 1, "`r.line || 1`");
                assert_eq!(id, "");
            }
            other => panic!("expected a file open, got {other:?}"),
        }

        // Neither: the reference's "editor is not ready" (quickopen.js:379).
        assert_eq!(qo::open_row(&Row::default()), Open::NotReady);
    }

    /// Every [`KeyAction`] is reachable from a real key event — a decision the
    /// router can never produce would be dead code behind a live-looking enum.
    #[test]
    fn every_key_action_is_reachable_from_a_real_event() {
        let mut seen: Vec<KeyAction> = Vec::new();
        let down = ["Escape", "ArrowDown", "ArrowUp", "PageDown", "PageUp", "Enter"];
        for k in down {
            if let Some(a) = route_key(k, false, false, false, false, true) {
                seen.push(a);
            }
        }
        seen.push(route_key("Tab", false, false, true, false, true).unwrap());
        // The two chords come from the keydown router while CLOSED.
        seen.push(route_key("p", true, false, false, false, false).unwrap());
        seen.push(route_key("t", true, false, false, false, false).unwrap());
        // Typing and Backspace come from the editing router while OPEN.
        for k in ["f", "Backspace"] {
            if let Some(a) = edit_key(k, false, false, false, true) {
                seen.push(a);
            }
        }
        for want in [
            KeyAction::OpenFiles,
            KeyAction::OpenSymbols,
            KeyAction::Close,
            KeyAction::Move(1),
            KeyAction::Move(-1),
            KeyAction::Activate,
            KeyAction::Type('f'),
            KeyAction::Backspace,
        ] {
            assert!(seen.contains(&want), "no real event produces {want:?}");
        }
    }

}
