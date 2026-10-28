//! W7: the missing wire between the two search/picker cores and the DOM.
//!
//! [`crate::search`] and [`crate::picker`] are complete, tested, and were
//! **unconnected**: nothing in the app called them, so `#findPanel` and `#modal`
//! had no way to receive real content and the shipped shells stayed empty no
//! matter what the user typed. This module is the connection, and it owns
//! exactly three things:
//!
//! 1. **State**: [`FindView`] (the query) and [`PickerView`] (the listing), the
//!    only places the app remembers what the user asked for.
//! 2. **Projection**: [`FindView::facts`] and [`PickerView::facts`] turn that
//!    state plus a real [`Graph`] / a real directory into the *exact* JSON shape
//!    nzk_ui's [`nzk_ui::dom::FindFacts`] / `PickerFacts` deserialize.
//! 3. **Honesty**: every field is copied from what the core returned. The two
//!    cores already draw the distinctions that matter — `NoQuery` vs `NoHits`,
//!    `NotFound` vs `Empty` — and this module's whole job is to carry those
//!    through *as values* rather than flattening them into `""` and `[]`.
//!
//! ## Why the `state` field is serialised and not re-derived
//!
//! `FindState`/`PickerState` are carried explicitly so a downstream consumer
//! (and the nzk_ui tests) can assert the distinction survived the trip. If the
//! app side only emitted `meta`/`notice`/`dirs`, a rendering bug could collapse
//! "typed nothing" into "nothing matched" and the JSON would look identical.
//! [`FindView::facts`] derives the state from the core's own enum, never from
//! the emptiness of a string.
//!
//! ## Absence is preserved
//!
//! Both `facts()` return `Option<Value>`: `None` means "this surface has never
//! been asked anything", which the shell renders by leaving its nodes exactly as
//! `index.html` shipped them. That is what keeps the default paint byte-identical
//! — a fresh app sends `find: null, picker: null`, not a pair of empty objects.
//! [`PickerView::facts`] is `None` until a listing actually exists, so the modal
//! can never claim "Empty folder." before anything was read.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::model::Graph;
use crate::picker::{self, Listing, Note, Picker};
use crate::search::{self, Outcome};

/// The Find panel's remembered state: just the query.
///
/// Deliberately *not* a cached [`Outcome`]: `Outcome` borrows the graph
/// (`Outcome<'a>`), so holding one would tie this struct's lifetime to a graph
/// the app replaces on every re-index. Storing the query and re-running
/// `search_default` at projection time is O(query) and cannot serve a stale hit
/// list from a graph that no longer exists — which is the failure mode a cached
/// `Vec<SearchResult>` would have after an index rebuild.
/// The engine's Find limit, matching the reference's `FIND_LIMIT`
/// (`search.js:144-156` sends `limit: FIND_LIMIT`; the core's `/api/find`
/// route defaults to the same 400, `core/src/api.rs`).
pub const FIND_LIMIT: i64 = 400;

#[derive(Clone, Debug, Default)]
pub struct FindView {
    query: String,
    /// The engine's grouped answer (`definitions`/`implementations`/`usages`)
    /// for [`FindView::refreshed_for`], already projected to the DOM shape by
    /// [`groups_value`]. `None` = "not computed", never "computed and empty":
    /// a query that genuinely found nothing produces `Some({"groups":[]})`.
    groups: Option<Value>,
    /// The query `groups` was computed for; empty means "not computed". This is
    /// what makes a per-frame [`FindView::refresh`] cheap (no O(corpus) rescan)
    /// while still never serving groups for a query the user has changed.
    refreshed_for: String,
}

impl FindView {
    /// A view with no query typed yet: the honest [`Outcome::NoQuery`] state.
    pub fn new() -> FindView {
        FindView::default()
    }

    /// Set the query text (e.g. from `#findQuery`'s `input` event).
    ///
    /// No trimming or normalisation happens here: [`search::search_default`]
    /// owns what counts as "no query", so this module cannot disagree with the
    /// core about what the user asked for.
    pub fn set_query(&mut self, query: &str) {
        self.query.clear();
        self.query.push_str(query);
    }

    /// Append one typed character — the `input` event's real shape.
    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
    }

    /// Remove the last character (backspace), a no-op when already empty.
    pub fn pop_char(&mut self) {
        self.query.pop();
    }

    /// Clear the query back to [`Outcome::NoQuery`].
    pub fn clear(&mut self) {
        self.query.clear();
    }

    /// The query text as typed.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The engine-backed v2 result groups for the current query, if computed.
    ///
    /// `Some` is the `groups` payload [`groups_value`] built from
    /// [`engine_find`]; `None` means the engine has not been asked (or the query
    /// is empty, where v2 renders its "type a symbol" copy instead of groups).
    pub fn groups(&self) -> Option<&Value> {
        self.groups.as_ref()
    }

    /// Ask the engine for this query's grouped results and cache them.
    ///
    /// Called by the host on every frame (it is a no-op unless the query
    /// changed), exactly like `FindView` re-runs `search_default` at projection
    /// time. The classification, ranking, limit and `truncated` flag are the
    /// core's (`Index::find`), never re-derived here. An empty query clears the
    /// groups so the panel falls back to the core's `noquery` copy.
    pub fn refresh(&mut self, index: &mut csgraph_core::index::Index, limit: i64) {
        if self.query.trim().is_empty() {
            self.groups = None;
            self.refreshed_for.clear();
            return;
        }
        if self.refreshed_for == self.query {
            return;
        }
        let payload = engine_find(index, &self.query, limit);
        self.groups = Some(groups_value(&payload));
        self.refreshed_for.clear();
        self.refreshed_for.push_str(&self.query);
    }

    /// Run the real search core over `graph` and return its [`Outcome`].
    ///
    /// This is the only call site of [`search::search_default`] in the app, and
    /// it is deliberately a thin pass-through: the limit, the ranking and the
    /// three honest states all belong to the core.
    pub fn outcome<'g>(&self, graph: &'g Graph) -> Outcome<'g> {
        search::search_default(graph, &self.query)
    }

    /// The `ui.find` payload, or `None` when this surface was never used.
    ///
    /// `None` only when there is no query *and* the caller has not asked for the
    /// panel to be shown: a caller that opens the panel with an empty query
    /// wants the `NoQuery` copy ("Type a symbol name to search…"), which is a
    /// real state with real text, so it must reach the DOM. Pass
    /// `open = true` from the code that opens `#findPanel`.
    pub fn facts(&self, graph: &Graph, open: bool) -> Option<Value> {
        if !open && self.query.is_empty() {
            return None;
        }
        Some(self.facts_value(graph))
    }

    /// The `ui.find` payload unconditionally, for callers that already decided
    /// the panel is open.
    pub fn facts_value(&self, graph: &Graph) -> Value {
        let outcome = self.outcome(graph);

        // The state is read off the core's own enum — never inferred from an
        // empty `rows` — so `NoQuery` and `NoHits` stay distinct on the wire.
        let state = match outcome {
            Outcome::NoQuery => "noquery",
            Outcome::Hits(_) => "hits",
            Outcome::NoHits => "nohits",
        };

        let rows: Vec<Value> = outcome
            .results()
            .iter()
            .map(|hit| {
                json!({
                    // `SearchResult::title()` is the `full || label || id`
                    // fallback chain; the core guarantees it is never empty.
                    "title": hit.title(),
                    "subtitle": hit.subtitle(),
                    // `MatchKind::tag()` (`exact`/`prefix`/`substring`/`subseq`)
                    // goes to `data-match` verbatim.
                    "tag": hit.kind.tag(),
                    "kind": hit.node.kind,
                    // `Node::id` is the click target; carried unparsed.
                    "node_id": hit.node.id,
                })
            })
            .collect();

        json!({
            "query": self.query,
            // `Outcome::meta()` is `""` for NoQuery on purpose (the bars shows
            // nothing rather than a "0 hits" that would look like a finished
            // search); `Outcome::notice()` is `""` for Hits, which is exactly
            // what makes nzk_ui hide `#findNotice`.
            "meta": outcome.meta(),
            "notice": outcome.notice(),
            "rows": rows,
            "state": state,
        })
    }
}

/// What a key means to the open suggestion popup — the reference's input
/// `keydown` branches (shell.js:508-534).
///
/// A pure decision type (like `quickopen_wire::KeyAction`): the host supplies
/// the key's reference name and performs the state change, so the binding table
/// is assertable without a window. `Move(delta)` is `moveSuggestion`
/// (search.js:64); `Activate` is `commitSearch` (search.js:76).
///
/// Only keys the popup actually claims are returned; `Escape` is deliberately
/// absent because the reference handles it as a global chord
/// (shell.js:388-396), and every other key falls through to the producers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestKey {
    /// `ArrowUp`/`ArrowDown` — `moveSuggestion`'s delta.
    Move(isize),
    /// `Enter` — `commitSearch`.
    Activate,
}

/// The popup's `keydown` table (shell.js:508-534): `ArrowDown`/`ArrowUp` move
/// the active row, `Enter` commits it. `None` for every other key, so the host
/// cannot consume a key the reference leaves to the producers.
pub fn route_key(name: &str) -> Option<SuggestKey> {
    match name {
        "ArrowDown" => Some(SuggestKey::Move(1)),
        "ArrowUp" => Some(SuggestKey::Move(-1)),
        "Enter" => Some(SuggestKey::Activate),
        _ => None,
    }
}

/// The graph-mode `#suggestions` popup state: the query plus the active row.
///
/// This is the native transcription of `search.js`'s `state.searchResults` +
/// `state.searchIndex` pair (search.js:40-84), but it deliberately carries
/// **only the query** — never a cached result list. Exactly like [`FindView`],
/// the hit rows are re-derived from the graph at projection time by the single
/// [`search::search_default`] call in [`SuggestionView::outcome`], so a re-index
/// can never leave a stale popup naming nodes the graph no longer has.
///
/// `active` is the reference's `state.searchIndex`, with `None` standing in for
/// its sentinel `-1` ("no selection"). Slice 7B routes `ArrowUp`/`ArrowDown`/
/// `Enter` into it through [`route_key`]/[`SuggestionView::move_active`]/
/// [`SuggestionView::commit_index`]; a click on a `.sug` row now routes through
/// the shell's `@hit` `data-id` payload into [`SuggestionView::commit_by_id`]
/// (`nzk_ui::suggestion_row_click` -> hosted by `app.rs`), so both input paths
/// share one commit.
#[derive(Clone, Debug, Default)]
pub struct SuggestionView {
    query: String,
    /// `state.searchIndex`: `None` = -1 = no highlighted row.
    active: Option<usize>,
}

impl SuggestionView {
    /// A view with nothing typed: the popup's closed state (no facts).
    pub fn new() -> SuggestionView {
        SuggestionView::default()
    }

    /// Replace the query text and reset the highlight.
    ///
    /// The reset is the reference's `state.searchIndex = -1` on every new result
    /// set (search.js:46-49, also on an empty query at :52): a highlight must
    /// never survive across queries. Storing the query untrimmed matches
    /// [`FindView`] — the core owns what counts as "no query".
    pub fn set_query(&mut self, query: &str) {
        self.query.clear();
        self.query.push_str(query);
        self.active = None;
    }

    /// Append one typed character — the `input` event's real shape.
    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.active = None;
    }

    /// Remove the last character (backspace), a no-op when already empty.
    pub fn pop_char(&mut self) {
        self.query.pop();
        self.active = None;
    }

    /// Hide the popup: clear the query and the highlight together.
    pub fn clear(&mut self) {
        self.query.clear();
        self.active = None;
    }

    /// The query text as typed.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The highlighted row index (`None` = no selection).
    pub fn active(&self) -> Option<usize> {
        self.active
    }

    /// Set the highlight directly, e.g. from a future click-to-focus route.
    pub fn set_active(&mut self, active: Option<usize>) {
        self.active = active;
    }

    /// Move the highlight by `delta` over a result list of `rows` entries.
    ///
    /// This is `moveSuggestion`'s wrap rule verbatim (search.js:64-72):
    /// `-1`/`None` is "no selection"; moving down from `-1` goes to `0`, moving
    /// down past the last row returns to `-1`, moving up from `-1` goes to the
    /// last row, and moving up from `0` returns to `-1`. An empty list never
    /// changes the highlight (the reference returns immediately).
    pub fn move_active(&mut self, delta: isize, rows: usize) {
        if rows == 0 {
            return;
        }
        let n = rows as isize;
        let idx = match self.active {
            None => -1,
            Some(k) => k as isize,
        };
        let next = idx + delta;
        let next = if next < -1 {
            n - 1
        } else if next >= n {
            -1
        } else {
            next
        };
        self.active = if next < 0 { None } else { Some(next as usize) };
    }

    /// Is the popup open (has something to search for)?
    ///
    /// The reference's popup is open while the input is focused with a non-empty
    /// trimmed query (`onSearchInput`, search.js:56-62: an empty query calls
    /// `hideSuggestions()`), and it publishes the empty "no matches" row for a
    /// query with no hits. Focus and mode belong to the nzk_ui
    /// (`NzkUiState::search_focused` / `surface`) — this answers only the query
    /// half, so the app's key router and the facts producer cannot disagree.
    pub fn is_open(&self) -> bool {
        !self.query.trim().is_empty()
    }

    /// Hide the popup **without losing the typed text** (`hideSuggestions`,
    /// search.js:11-20): the input's `value` is untouched, only
    /// `state.searchIndex = -1` is reset. This is the honest Escape/blur/commit
    /// semantics — `clear()` is reserved for "the query itself went away".
    pub fn hide(&mut self) {
        self.active = None;
    }

    /// The `graph.nodes` index `Enter` would select, or `None` when there is
    /// nothing to commit.
    ///
    /// `commitSearch` (search.js:76-84) commits `searchIndex >= 0 ? searchIndex
    /// : 0`, then centers that node by **id**. This returns the index for the
    /// same row, resolving the id against THIS graph: an id the graph does not
    /// contain (a facts paint that outlived a re-index) yields `None`, never a
    /// neighbouring node. The caller must then only hide the popup — never
    /// panic and never select something else.
    pub fn commit_index(&self, graph: &Graph) -> Option<usize> {
        let outcome = self.outcome(graph);
        let rows = outcome.results();
        let row = rows.get(self.active.unwrap_or(0))?;
        let id = &row.node.id;
        graph.nodes.iter().position(|n| &n.id == id)
    }

    /// The `graph.nodes` index a **click** on the row with `id` must select, or
    /// `None` when this graph no longer contains that id.
    ///
    /// This is the click half of `commitSearch`. The reference's mousedown
    /// handler reads the row's `data-id` and calls `centerNode(id)` directly
    /// (`shell.js:527-536`), so the CLICKED id — never the highlight — is what
    /// commits. Resolving it against THIS graph makes a row that a re-index
    /// removed a no-op: it can never select a neighbouring node.
    pub fn commit_by_id(&self, graph: &Graph, id: &str) -> Option<usize> {
        graph.nodes.iter().position(|n| n.id == id)
    }

    /// Run the real search core over `graph` and return its [`Outcome`].
    ///
    /// The second (and only other) call site of [`search::search_default`]: the
    /// graph-mode popup and the Find panel must never disagree about ranking,
    /// the limit or the honest three-state split, so both go through the same
    /// core and neither re-implements it.
    pub fn outcome<'g>(&self, graph: &'g Graph) -> Outcome<'g> {
        search::search_default(graph, &self.query)
    }

    /// The `ui.suggestions` payload, or `None` when the popup is closed.
    ///
    /// `None` when the trimmed query is empty — the shipped document then keeps
    /// `class="suggestions hidden"` with no children, which is the byte-identical
    /// default paint. A whitespace-only query is "closed" for the same reason the
    /// core answers [`Outcome::NoQuery`] for it: there is nothing to search for.
    pub fn facts(&self, graph: &Graph) -> Option<Value> {
        if self.query.trim().is_empty() {
            return None;
        }
        Some(self.facts_value(graph))
    }

    /// The `ui.suggestions` payload unconditionally, for callers that already
    /// decided the popup is open.
    pub fn facts_value(&self, graph: &Graph) -> Value {
        let outcome = self.outcome(graph);
        let rows: Vec<Value> = outcome
            .results()
            .iter()
            .map(|hit| {
                // search.js:34: `label || name || id`. The native `model::Node`
                // has no `name`, so the honest fallback is label -> id. This is
                // deliberately NOT `SearchResult::title()`, whose chain starts at
                // `full` (search.rs:143-155) — the popup's third span carries
                // `full` separately, so using `title()` would duplicate it.
                let label = if !hit.node.label.is_empty() {
                    &hit.node.label
                } else {
                    &hit.node.id
                };
                json!({
                    "id": hit.node.id,
                    "kind": hit.node.kind,
                    "label": label,
                    // search.js:35 uses `n.ns || ''`; `Node::ns` is that field.
                    "full": hit.node.ns,
                })
            })
            .collect();
        // Never publish a highlight that names a row this projection did not
        // produce (the core's result set can shrink when the graph changes).
        let active = self.active.filter(|i| *i < rows.len());
        // `search.js:23`: the core's own `NoHits` state is what draws the
        // "no matches" row. An empty `rows` alone is NOT that signal — a caller
        // must key off `empty`, or a closed popup would look like a miss.
        let empty = match outcome {
            Outcome::NoHits => Some("no matches"),
            Outcome::NoQuery | Outcome::Hits(_) => None,
        };
        json!({
            "rows": rows,
            "active": active,
            "empty": empty,
        })
    }
}

/// The folder picker's remembered state: the live listing plus its `Picker`.
///
/// [`Picker`] already owns the current path and the repo root, and
/// [`Picker::current`] hands back the listing by reference, so this struct is a
/// thin wrapper rather than a second copy of the navigation state. The listing is
/// materialised (cloned) only when `facts()` is called.
#[derive(Clone, Debug)]
pub struct PickerView {
    picker: Picker,
    /// The last listing actually produced by a navigation call.
    ///
    /// `None` until the first `browse`: a `Picker::new` deliberately does not
    /// read the filesystem (see `picker.rs`), so claiming a listing before one
    /// was requested would make the modal assert something no `read_dir` backed.
    listing: Option<Listing>,
    /// The text in `#modalPathInput`, when it differs from the listing path.
    input: Option<String>,
}

impl PickerView {
    /// A picker rooted at `repo_root`, showing nothing yet.
    ///
    /// `start` is only stored (as the input text); the first listing comes from
    /// an explicit [`PickerView::go`]/[`PickerView::open_dir`]. That keeps
    /// `Picker::new`'s "no filesystem read" contract intact.
    pub fn new(repo_root: &Path, start: &str) -> PickerView {
        PickerView {
            picker: Picker::new(repo_root, start),
            listing: None,
            input: if start.is_empty() {
                None
            } else {
                Some(start.to_string())
            },
        }
    }

    /// Open without reading the directory. The host uses this on the UI thread,
    /// then calls `go(start)` on its picker worker and adopts the completed view.
    pub fn pending(repo_root: &Path, start: &str) -> PickerView {
        let picker = Picker::pending(repo_root, start);
        let listing = Some(picker.current().clone());
        PickerView {
            picker,
            listing,
            input: if start.is_empty() { None } else { Some(start.to_string()) },
        }
    }

    /// Keep the current path visible but mark its next listing as pending.
    pub fn set_loading(&mut self) {
        let listing = self.picker.current();
        self.listing = Some(crate::picker::Listing {
            path: listing.path.clone(),
            parent: listing.parent.clone(),
            dirs: Vec::new(),
            files_skipped: 0,
            note: crate::picker::Note::Loading,
        });
    }

    /// Rooted at `repo_root` with no starting text.
    pub fn rooted_at(repo_root: &Path) -> PickerView {
        PickerView::new(repo_root, "")
    }

    /// The listing for a path, stored as the current one — the direct
    /// `renderPicker(d)` step, for a host that already resolved the path.
    ///
    /// `shell.js:29 browseTo(path)` -> `renderPicker` involves no typed text, so
    /// [`PickerView::go`] (which records what the user typed) is the wrong entry
    /// point for a `data-nav` row click or a `#modalRoot`/`#modalHome` press.
    /// This is the one that matches: the path is listed and `#modalPathInput` is
    /// synced to the path that was actually shown.
    pub fn show(&mut self, path: &Path) -> &Listing {
        let listing = self.picker.open_dir(path);
        self.sync_input();
        self.listing = Some(listing);
        self.current()
    }

    /// The folder a `#modalSelect` press would commit, or `None` when the
    /// listing is not selectable.
    ///
    /// An alias of [`PickerView::chosen_path`], named for the reference handler
    /// (`chooseCurrentFolder`, `shell.js:99-106`) so the `#modalSelect` arm reads
    /// the same at the call site as it does in the JS. The `None` is the whole
    /// gate: a missing or unreadable path must never be committed as the root.
    pub fn choose_current_folder(&self) -> Option<PathBuf> {
        self.chosen_path()
    }

    /// Resolve `input` (a typed path, possibly relative, possibly `~`) and list
    /// it — the `#modalGo` handler. Returns the real listing.
    pub fn go(&mut self, input: &str) -> &Listing {
        // `renderPicker` sets `modalPathInputEl.value = d.path` after every real
        // listing (`shell.js:53`), so the box shows the RESOLVED absolute path,
        // not the raw typed text ("sub" becomes "/repo/sub"). Using the listed
        // path also means an empty start (`browseTo('')`) paints the repo root
        // rather than an empty bar.
        let listing = self.picker.go(input);
        self.listing = Some(listing);
        self.sync_input();
        self.current()
    }

    /// Descend into `path` — the `data-nav` click handler (`shell.js:58`).
    pub fn open_dir(&mut self, path: &Path) -> &Listing {
        let listing = self.picker.open_dir(path);
        self.sync_input();
        self.listing = Some(listing);
        self.current()
    }

    /// Go to the parent directory — `#modalUp`.
    pub fn up(&mut self) -> &Listing {
        let listing = self.picker.up();
        self.sync_input();
        self.listing = Some(listing);
        self.current()
    }

    /// Go to the home directory — `#modalHome`.
    pub fn home(&mut self) -> &Listing {
        let listing = self.picker.home();
        self.sync_input();
        self.listing = Some(listing);
        self.current()
    }

    /// Go to the filesystem root — `#modalRoot`.
    pub fn root(&mut self) -> &Listing {
        let listing = self.picker.root();
        self.sync_input();
        self.listing = Some(listing);
        self.current()
    }

    /// Go to the repo root — `#modalRepo`.
    pub fn repo(&mut self) -> &Listing {
        let listing = self.picker.repo();
        self.sync_input();
        self.listing = Some(listing);
        self.current()
    }

    /// Keep `#modalPathInput` in step with navigation that was not typed.
    ///
    /// A `go` keeps exactly what the user typed (the reference does too); every
    /// other move replaces it with the path that was actually resolved, so the
    /// input can never disagree with the folder on screen.
    fn sync_input(&mut self) {
        self.input = Some(self.current().path.to_string_lossy().into_owned());
    }

    /// The listing produced so far, or `None` if nothing was ever listed.
    pub fn listing(&self) -> Option<&Listing> {
        self.listing.as_ref()
    }

    /// The listing the `Picker` currently holds.
    ///
    /// Falls back to `Picker::current`, which is always a real `Listing`, so this
    /// is total even before the first explicit navigation.
    pub fn current(&self) -> &Listing {
        self.picker.current()
    }

    /// The folder that would be indexed — `#modalSelect`'s payload.
    pub fn chosen(&self) -> Option<&Path> {
        self.picker.chosen()
    }

    /// [`PickerView::chosen`] as an owned path.
    pub fn chosen_path(&self) -> Option<PathBuf> {
        self.picker.chosen_path()
    }

    /// The repo root this picker was created against.
    pub fn repo_root(&self) -> &Path {
        self.picker.repo_root()
    }

    /// The `ui.picker` payload, or `None` before any listing exists.
    ///
    /// The `None` is load-bearing: nzk_ui's `apply_picker` treats a listed
    /// state with no directories as "Empty folder.", and that message must be
    /// unreachable until a real `read_dir` said so.
    pub fn facts(&self) -> Option<Value> {
        let listing = self.listing.as_ref()?;
        Some(Self::facts_of(listing, self.input.as_deref()))
    }

    /// The `ui.picker` payload for `listing`, as nzk_ui's `PickerFacts`.
    ///
    /// Exposed separately so a caller that already holds a `Listing` (a test, or
    /// a future host that lists without a `PickerView`) still gets the one
    /// canonical projection rather than re-implementing it.
    pub fn facts_of(listing: &Listing, input: Option<&str>) -> Value {
        let state = match listing.note {
            crate::picker::Note::Loading => "loading",
            Note::Indexable => "indexable",
            Note::NoCs => "nocs",
            Note::Empty => "empty",
            Note::NotFound => "notfound",
            Note::Unreadable => "unreadable",
        };

        let dirs: Vec<Value> = listing
            .dirs
            .iter()
            .map(|dir| {
                json!({
                    "name": dir.name,
                    "path": dir.path.to_string_lossy(),
                    "symlink": dir.symlink,
                })
            })
            .collect();

        json!({
            "path": listing.path.to_string_lossy(),
            // `shell.js:53` puts the resolved path in the input except while the
            // user is mid-edit; an absent `input` therefore means "same as path".
            "path_input": input.unwrap_or_else(|| listing.path.to_str().unwrap_or("")),
            // `Note::text()` is `None` for nothing — every variant has text — but
            // an empty note must still serialise as `""` so it clears the slot.
            "note": listing.note.text().unwrap_or(""),
            "note_class": listing.note.class(),
            // A missing or unreadable path is never selectable, so `#modalSelect`
            // cannot invite the user to index something that is not there.
            "selectable": listing.selectable(),
            // `parent.is_none()` disables `#modalUp` (`shell.js:54`) — at the
            // filesystem root and after a failed listing alike.
            "can_go_up": listing.parent.is_some(),
            "dirs": dirs,
            // The count is a `usize` in the core and an `Option` on the wire, so
            // a caller can tell "nothing was skipped" from "no count taken".
            "files_skipped": listing.files_skipped,
            "state": state,
        })
    }
}

/// Project a `Listing` straight to facts — the free-function form of
/// [`PickerView::facts_of`] for callers with no view.
pub fn picker_facts(listing: &Listing) -> Value {
    PickerView::facts_of(listing, None)
}

/// The `ui.find` payload for a raw query — the free-function form of
/// [`FindView::facts_value`] for callers with no view.
pub fn find_facts(graph: &Graph, query: &str) -> Value {
    let mut view = FindView::new();
    view.set_query(query);
    view.facts_value(graph)
}

/// Is `path` an existing directory? A small helper for callers deciding whether
/// to open the picker at all, kept here so the answer is spelled once.
pub fn is_dir(path: &Path) -> bool {
    picker::browse_to(path).selectable()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Node;

    fn node(id: &str, label: &str, full: &str, kind: &str, file: &str, line: i64) -> Node {
        Node {
            id: id.to_string(),
            label: label.to_string(),
            full: full.to_string(),
            kind: kind.to_string(),
            file: file.to_string(),
            line,
            ..Node::default()
        }
    }

    /// `Path` -> `String` for comparing against a `serde_json::Value`, which has
    /// no `PartialEq<Cow<str>>`.
    fn path_str(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    /// A graph with names chosen to exercise all four `MatchKind`s for the query
    /// `hit`: exact, prefix, substring, subsequence.
    fn fixture_graph() -> Graph {
        Graph {
            repo: PathBuf::from("/tmp/find_wire_fixture"),
            nodes: vec![
                node("n:1", "hit", "App.Hit", "method", "app/src/hit.rs", 12),
                node("n:2", "hitsAll", "App.HitsAll", "method", "app/src/hits.rs", 7),
                node("n:3", "unhit", "App.Unhit", "field", "app/src/unhit.rs", 3),
                node("n:4", "h_i_t", "App.HIT", "class", "app/src/hit2.rs", 0),
                node("n:5", "other", "App.Other", "method", "app/src/other.rs", 99),
            ],
            edges: Vec::new(),
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        }
    }

    // ---- FindView: the three honest states -------------------------------

    /// The distinction this whole wire exists to preserve.
    #[test]
    fn empty_query_is_noquery_and_has_copy() {
        let view = FindView::new();
        let facts = view.facts_value(&fixture_graph());

        assert_eq!(facts["state"], "noquery");
        assert_eq!(facts["meta"], "");
        assert_eq!(facts["rows"].as_array().unwrap().len(), 0);
        // A NoQuery panel is not blank: it carries the core's own instruction.
        let notice = facts["notice"].as_str().unwrap();
        assert!(
            notice.starts_with("Type a symbol name to search."),
            "unexpected NoQuery notice: {notice:?}"
        );
    }

    #[test]
    fn unmatched_query_is_nohits_not_noquery() {
        let mut view = FindView::new();
        view.set_query("zzzz-no-such-symbol");
        let facts = view.facts_value(&fixture_graph());

        assert_eq!(facts["state"], "nohits");
        assert_eq!(facts["meta"], "no hits");
        assert_eq!(facts["notice"], "No symbol matched this query.");
        assert_eq!(facts["rows"].as_array().unwrap().len(), 0);

        // The two empty-result states must not be identical documents.
        let noquery = FindView::new().facts_value(&fixture_graph());
        assert_ne!(noquery["state"], facts["state"]);
        assert_ne!(noquery["notice"], facts["notice"]);
    }

    #[test]
    fn real_query_returns_real_rows_with_core_values() {
        let mut view = FindView::new();
        view.set_query("hit");
        let facts = view.facts_value(&fixture_graph());

        assert_eq!(facts["state"], "hits");
        let rows = facts["rows"].as_array().unwrap();
        assert!(!rows.is_empty(), "a real query must return real rows");
        assert_eq!(facts["meta"], format!("{} hits", rows.len()));
        // Hits carry no notice, which is what hides `#findNotice`.
        assert_eq!(facts["notice"], "");

        // Every row id must be a node the graph really has, and the title must
        // be the core's `full || label || id` chain.
        let ids: Vec<&str> = rows.iter().map(|r| r["node_id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"n:1"));
        let first = rows
            .iter()
            .find(|r| r["node_id"] == "n:1")
            .expect("the exact match must be present");
        assert_eq!(first["title"], "App.Hit");
        assert_eq!(first["kind"], "method");
        assert_eq!(first["tag"], "exact");
        // The subtitle is the core's `label · file:line` shape.
        let sub = first["subtitle"].as_str().unwrap();
        assert!(sub.contains("app/src/hit.rs:12"), "subtitle was {sub:?}");

        // Rows must be the core's order: exact before prefix before the rest.
        let exact = rows.iter().position(|r| r["tag"] == "exact");
        let prefix = rows.iter().position(|r| r["tag"] == "prefix");
        if let (Some(e), Some(p)) = (exact, prefix) {
            assert!(e < p, "exact rank must precede prefix");
        }
    }

    /// `SearchResult::title()` is the `full || label || id` chain, and the search
    /// core does **not** search `id` (`search::fields_of` covers
    /// label/full/kind/ns/owner/file). A node the core CAN find but whose `full`
    /// is blank must therefore still get a non-empty title from `label`, so no
    /// row can render an empty `.fname`.
    #[test]
    fn title_falls_back_to_label_so_no_row_is_blank() {
        let g = Graph {
            repo: PathBuf::from("/tmp/find_wire_fixture"),
            nodes: vec![node("n:1", "onlyLabel", "", "method", "", 0)],
            edges: Vec::new(),
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let mut view = FindView::new();
        view.set_query("onlylabel");
        let facts = view.facts_value(&g);

        let rows = facts["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "a blank-`full` node must still be findable");
        // The title comes from the label, never an empty string.
        assert_eq!(rows[0]["title"], "onlyLabel");
        assert_eq!(rows[0]["node_id"], "n:1");
    }

    /// The last resort of the chain: a findable node with BOTH `full` and
    /// `label` blank is titled by its `id`. It is reachable through a searchable
    /// field (`kind` here), which is how a node like that gets found at all —
    /// note that `search::fields_of` does **not** include `id`, so a node with
    /// every searched field blank is genuinely unfindable and is not a case the
    /// row renderer can ever be handed.
    #[test]
    fn title_falls_back_to_id_when_full_and_label_are_blank() {
        let g = Graph {
            repo: PathBuf::from("/tmp/find_wire_fixture"),
            nodes: vec![node("n:lonely", "", "", "uniquekind", "", 0)],
            edges: Vec::new(),
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let mut view = FindView::new();
        view.set_query("uniquekind");
        let facts = view.facts_value(&g);

        let rows = facts["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        // Title falls all the way through to the id — never empty.
        assert_eq!(rows[0]["title"], "n:lonely");
    }

    /// `facts()` with no query and the panel unopened is `None` — the absence
    /// that keeps the default paint byte-identical.
    #[test]
    fn unopened_empty_find_has_no_facts() {
        let view = FindView::new();
        assert!(view.facts(&fixture_graph(), false).is_none());
        // Opening the panel with an empty query is a real state, so it does.
        assert!(view.facts(&fixture_graph(), true).is_some());
    }

    #[test]
    fn typing_accumulates_like_a_real_input_event() {
        let mut view = FindView::new();
        for c in "hit".chars() {
            view.push_char(c);
        }
        assert_eq!(view.query(), "hit");
        view.pop_char();
        assert_eq!(view.query(), "hi");
        view.clear();
        assert_eq!(view.query(), "");
        view.set_query("replaced");
        assert_eq!(view.query(), "replaced");
    }

    // ---- PickerView: real TempDir, real listings --------------------------

    fn tmpdir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "find_wire_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn never_listed_picker_has_no_facts() {
        let root = tmpdir("unlisted");
        let view = PickerView::rooted_at(&root);
        // No `read_dir` has run, so the modal must not claim "Empty folder.".
        assert!(view.facts().is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pending_picker_exposes_loading_without_listing_the_directory() {
        let root = tmpdir("pending");
        let view = PickerView::pending(&root, "");
        let facts = view.facts().expect("pending state paints immediately");
        assert_eq!(facts["state"], "loading");
        assert_eq!(facts["selectable"], false);
        assert_eq!(facts["dirs"].as_array().map(Vec::len), Some(0));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn listing_a_real_directory_produces_real_rows() {
        let root = tmpdir("real");
        std::fs::create_dir_all(root.join("alpha")).unwrap();
        std::fs::create_dir_all(root.join("beta")).unwrap();
        std::fs::write(root.join("skip.cs"), "// x").unwrap();
        std::fs::create_dir_all(root.join("empty_sub")).unwrap();

        let mut view = PickerView::rooted_at(&root);
        let listing = view.go(&root.to_string_lossy()).clone();
        let facts = view.facts().expect("a real listing must produce facts");

        // No `.cs` file exists below this synthetic temp dir, so the core's
        // honest note is `NoCs` — not `Indexable`. That is the core's call, and
        // this test asserts we carry its word through rather than upgrading it.
        assert_eq!(facts["state"], "nocs");
        assert_eq!(facts["note"], "no .cs files in this subtree");
        assert_eq!(facts["note_class"], "modal-note warn");
        // A readable folder with no `.cs` files is still a legitimate choice.
        assert!(facts["selectable"].as_bool().unwrap());
        assert_eq!(facts["path"], path_str(&listing.path));

        let dirs = facts["dirs"].as_array().unwrap();
        let names: Vec<&str> = dirs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        // Only directories, in the core's sort order, and no files.
        assert_eq!(names, vec!["alpha", "beta", "empty_sub"]);
        for d in dirs {
            // `data-nav` must be the absolute path the row navigates to.
            let p = d["path"].as_str().unwrap();
            assert!(Path::new(p).is_absolute(), "data-nav was not absolute: {p}");
            assert!(Path::new(p).is_dir(), "data-nav was not a directory: {p}");
            assert_eq!(d["symlink"], false);
        }
        // The one non-directory entry is reported as a real count, not hidden.
        assert_eq!(facts["files_skipped"], 1);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Opening the picker with an empty start (`openPicker(startPath || '')`)
    /// lists the repo root and must paint THAT path in `#modalPathInput`; an
    /// empty `input` used to serialise `path_input: ""`, i.e. a blank path bar
    /// over a real listing, and clicking it then focused an empty box.
    #[test]
    fn an_empty_start_paints_the_listed_path_not_an_empty_bar() {
        let root = tmpdir("empty-start");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let mut view = PickerView::new(&root, "");
        let listing = view.go("").clone();
        let facts = view.facts().expect("a real listing");
        assert_eq!(facts["path"], path_str(&listing.path));
        assert_eq!(facts["path_input"], facts["path"], "the bar shows the listing");
        assert!(!facts["path_input"].as_str().unwrap().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_empty_folder_is_empty_but_still_selectable() {
        let root = tmpdir("emptydir");
        let mut view = PickerView::rooted_at(&root);
        view.go(&root.to_string_lossy());
        let facts = view.facts().unwrap();

        assert_eq!(facts["state"], "empty");
        assert_eq!(facts["note"], "folder is empty");
        assert_eq!(facts["note_class"], "modal-note warn");
        assert_eq!(facts["dirs"].as_array().unwrap().len(), 0);
        // An existing, readable folder is a legitimate choice even with no .cs.
        assert!(
            facts["selectable"].as_bool().unwrap(),
            "an empty but readable folder must remain selectable"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The bug this module exists to prevent: a missing path must not be able to
    /// look like an empty folder.
    #[test]
    fn missing_path_is_notfound_and_not_selectable() {
        let root = tmpdir("missing");
        let gone = root.join("definitely-not-here");
        let mut view = PickerView::rooted_at(&root);
        let listing = view.go(&gone.to_string_lossy()).clone();
        let facts = view.facts().unwrap();

        assert_eq!(listing.note, Note::NotFound);
        assert_eq!(facts["state"], "notfound");
        assert_eq!(facts["note"], "path does not exist");
        assert_eq!(facts["note_class"], "modal-note warn");
        assert_eq!(facts["dirs"].as_array().unwrap().len(), 0);
        assert!(!facts["selectable"].as_bool().unwrap());
        // `state` is the value nzk_ui gates the "Empty folder." message on.
        assert_ne!(facts["state"], "empty");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn nav_moves_update_path_and_can_go_up() {
        let root = tmpdir("nav");
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(sub.join("inner")).unwrap();

        let mut view = PickerView::rooted_at(&root);
        view.go(&root.to_string_lossy());
        let at_root = view.facts().unwrap();
        assert_eq!(at_root["path"], path_str(&root));

        // Descend via `data-nav`, exactly as the row click would.
        let nav = at_root["dirs"][0]["path"].as_str().unwrap().to_string();
        view.open_dir(Path::new(&nav));
        let inside = view.facts().unwrap();
        assert_eq!(inside["path"], path_str(&sub));
        assert!(inside["can_go_up"].as_bool().unwrap());
        // The input follows a non-typed move, so it cannot disagree.
        assert_eq!(inside["path_input"], path_str(&sub));
        assert_eq!(inside["dirs"][0]["name"], "inner");

        // Up returns to the real parent.
        view.up();
        assert_eq!(view.facts().unwrap()["path"], path_str(&root));

        // `repo()` goes to the root the picker was created against.
        view.repo();
        assert_eq!(view.facts().unwrap()["path"], path_str(&root));
        std::fs::remove_dir_all(&root).ok();
    }

    /// The filesystem root genuinely has no parent, so `#modalUp` must disable.
    #[test]
    fn filesystem_root_disables_up() {
        let root = picker::root_dir().expect("a filesystem root must exist");
        let mut view = PickerView::rooted_at(&root);
        let listing = view.root().clone();
        assert_eq!(listing.path, root);
        let facts = view.facts().unwrap();
        assert_eq!(facts["path"], path_str(&root));
        assert!(
            !facts["can_go_up"].as_bool().unwrap(),
            "the filesystem root must disable #modalUp"
        );
    }

    #[test]
    fn chosen_path_is_the_resolved_directory() {
        let root = tmpdir("chosen");
        let mut view = PickerView::rooted_at(&root);
        view.go(&root.to_string_lossy());
        let chosen = view.chosen().expect("a listed folder must be chosen");
        assert_eq!(
            picker::normalize(chosen),
            picker::normalize(&root),
            "the chosen folder must be the one that was listed"
        );
        assert_eq!(view.repo_root(), root.as_path());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn free_function_forms_agree_with_the_views() {
        let mut view = FindView::new();
        view.set_query("hit");
        assert_eq!(
            find_facts(&fixture_graph(), "hit"),
            view.facts_value(&fixture_graph())
        );

        let root = tmpdir("agree");
        let mut pv = PickerView::rooted_at(&root);
        let listing = pv.go(&root.to_string_lossy()).clone();
        assert_eq!(picker_facts(&listing), pv.facts().unwrap());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn is_dir_agrees_with_the_filesystem() {
        let root = tmpdir("isdir");
        assert!(is_dir(&root));
        assert!(!is_dir(&root.join("nope")));
        std::fs::remove_dir_all(&root).ok();
    }

    // ---- Wire-shape conformance ------------------------------------------

    /// The serialised payloads must be exactly the keys nzk_ui declares, so a
    /// renamed field is a test failure rather than a silently-defaulted `null`
    /// that would render as an empty panel.
    #[test]
    fn find_payload_has_exactly_the_shell_keys() {
        let mut view = FindView::new();
        view.set_query("hit");
        let facts = view.facts_value(&fixture_graph());
        let mut keys: Vec<&str> = facts.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["meta", "notice", "query", "rows", "state"],
            "FindFacts field names drifted from nzk_ui"
        );

        let row = &facts["rows"][0];
        let mut rkeys: Vec<&str> = row.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        rkeys.sort_unstable();
        assert_eq!(rkeys, vec!["kind", "node_id", "subtitle", "tag", "title"]);
    }

    #[test]
    fn picker_payload_has_exactly_the_shell_keys() {
        let root = tmpdir("keys");
        std::fs::create_dir_all(root.join("one")).unwrap();
        let mut view = PickerView::rooted_at(&root);
        view.go(&root.to_string_lossy());
        let facts = view.facts().unwrap();

        let mut keys: Vec<&str> = facts.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "can_go_up",
                "dirs",
                "files_skipped",
                "note",
                "note_class",
                "path",
                "path_input",
                "selectable",
                "state",
            ],
            "PickerFacts field names drifted from nzk_ui"
        );

        let row = &facts["dirs"][0];
        let mut rkeys: Vec<&str> = row.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        rkeys.sort_unstable();
        assert_eq!(rkeys, vec!["name", "path", "symlink"]);
        std::fs::remove_dir_all(&root).ok();
    }

    /// Every state name emitted here must be one nzk_ui's enum accepts, or
    /// serde would fail the whole payload and the panel would render as shipped.
    ///
    /// nzk_ui is a **separate cargo workspace** with no dependency edge to
    /// this binary (the app reaches it over a JSON pipe), so `nzk_ui::dom`
    /// cannot be imported and `serde`'s derive is not an app dependency. The
    /// contract is therefore pinned against nzk_ui's **source of truth**:
    /// `nzk_ui/src/dom.rs` is read at test time and each emitted state name is
    /// checked to be a real variant name, lowercased the way its
    /// `rename_all = "lowercase"` requires. That is strictly stronger than a
    /// local mirror, because it fails if the shell renames a variant.
    fn dom_rs_source() -> Option<String> {
        // CARGO_MANIFEST_DIR is `<repo>/app`. nzk_ui is its own module repo in
        // the CsViewerGitRepo layout; the sibling path is the legacy layout.
        let base = Path::new(env!("CARGO_MANIFEST_DIR"));
        for rel in [
            "../NZKRepoDeps/CSV Module (nzk-cs-rs_nzk_ui)/src/dom.rs",
            "../nzk_ui/src/dom.rs",
        ] {
            let path = base.join(rel);
            if let Ok(s) = std::fs::read_to_string(&path) {
                return Some(s);
            }
        }
        None
    }

    /// Assert `state_name` (e.g. "noquery") is a variant of `enum_name` in
    /// nzk_ui's `dom.rs` under a lowercase rename.
    fn assert_shell_state_variant(src: &str, enum_name: &str, state_name: &str) {
        let start = src
            .find(&format!("pub enum {enum_name}"))
            .unwrap_or_else(|| panic!("nzk_ui has no `pub enum {enum_name}`"));
        let body = &src[start..];
        let end = body.find("\n}").expect("unterminated enum");
        let body = &body[..end];
        // `#[serde(rename_all = "lowercase")]` sits ABOVE the `pub enum`, so the
        // attribute must be looked for in the lines preceding the declaration,
        // not inside the variant body. `src[..start]` ends with the attribute
        // line (plus the doc comment that precedes it).
        let head = &src[..start];
        let attr_line = head
            .lines()
            .rev()
            .find(|l| {
                let t = l.trim();
                !t.is_empty() && !t.starts_with("///")
            })
            .unwrap_or("");
        assert!(
            attr_line.contains("rename_all = \"lowercase\""),
            "nzk_ui's {enum_name} is not lowercase-renamed (attribute line was \
             {attr_line:?}), so {state_name:?} would not parse"
        );
        // Variants are CamelCase in Rust and lowercased by serde, so compare
        // case-insensitively rather than uppercasing (`NoQuery` -> `noquery`,
        // but `NOQUERY` != `NoQuery`).
        let found = body.lines().map(str::trim).any(|l| {
            let name = l
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap_or("");
            !name.is_empty() && name.eq_ignore_ascii_case(state_name)
        });
        assert!(
            found,
            "nzk_ui's {enum_name} has no variant that lowercases to {state_name:?}"
        );
    }

    #[test]
    fn state_names_match_css_shell_dom_rs() {
        let Some(src) = dom_rs_source() else {
            eprintln!("nzk_ui module source absent; skipping");
            return;
        };
        for s in ["noquery", "hits", "nohits"] {
            assert_shell_state_variant(&src, "FindState", s);
        }
        for s in ["unlisted", "indexable", "nocs", "empty", "notfound", "unreadable"] {
            assert_shell_state_variant(&src, "PickerState", s);
        }
        // The check is not vacuous: a name the shell lacks must fail it.
        let missing = std::panic::catch_unwind(|| {
            assert_shell_state_variant(&src, "FindState", "bogus")
        });
        assert!(missing.is_err(), "the variant check must reject a bogus name");
    }

    /// The keys this module emits must be the very keys nzk_ui's structs
    /// declare, so a renamed field is a failure rather than a silently-defaulted
    /// `null` that would render as an empty panel.
    #[test]
    fn payload_keys_exist_in_css_shell_structs() {
        let Some(src) = dom_rs_source() else {
            eprintln!("nzk_ui module source absent; skipping");
            return;
        };

        let struct_body = |name: &str| -> String {
            let start = src
                .find(&format!("pub struct {name}"))
                .unwrap_or_else(|| panic!("nzk_ui has no `pub struct {name}`"));
            let body = &src[start..];
            let end = body.find("\n}").expect("unterminated struct");
            body[..end].to_string()
        };

        let check = |struct_name: &str, keys: &[&str]| {
            let body = struct_body(struct_name);
            for k in keys {
                assert!(
                    body.contains(&format!("pub {k}:")),
                    "nzk_ui's {struct_name} has no field `{k}`"
                );
            }
        };

        check("FindFacts", &["query", "meta", "notice", "rows", "state"]);
        check("FindRow", &["title", "subtitle", "tag", "kind", "node_id"]);
        check(
            "PickerFacts",
            &[
                "path",
                "path_input",
                "note",
                "note_class",
                "selectable",
                "can_go_up",
                "dirs",
                "files_skipped",
                "state",
            ],
        );
        check("PickerRow", &["name", "path", "symlink"]);
    }

    /// End-to-end on this side of the wire: a real graph and a real directory,
    /// both projected, then serialised to the JSON **text** that actually goes
    /// down the pipe and parsed back — the same round trip the shell performs.
    #[test]
    fn real_payloads_survive_json_text_round_trip() {
        let mut fv = FindView::new();
        fv.set_query("hit");
        let text = serde_json::to_string(&fv.facts_value(&fixture_graph())).unwrap();
        let back: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back["state"], "hits");
        assert_eq!(back["query"], "hit");
        let rows = back["rows"].as_array().unwrap();
        assert!(!rows.is_empty());
        assert_eq!(back["meta"], format!("{} hits", rows.len()));
        assert!(rows.iter().all(|r| !r["title"].as_str().unwrap().is_empty()));

        // NoQuery and NoHits stay distinct across the text round trip.
        let nq: Value =
            serde_json::from_str(&serde_json::to_string(&FindView::new().facts_value(&fixture_graph())).unwrap())
                .unwrap();
        assert_eq!(nq["state"], "noquery");
        let mut nv = FindView::new();
        nv.set_query("zzzz-no-such-symbol");
        let nh: Value = serde_json::from_str(
            &serde_json::to_string(&nv.facts_value(&fixture_graph())).unwrap(),
        )
        .unwrap();
        assert_eq!(nh["state"], "nohits");
        assert_ne!(nq["state"], nh["state"]);

        let root = tmpdir("e2e");
        std::fs::create_dir_all(root.join("kid")).unwrap();
        let mut pv = PickerView::rooted_at(&root);
        pv.go(&root.to_string_lossy());
        let ptext = serde_json::to_string(&pv.facts().unwrap()).unwrap();
        let p: Value = serde_json::from_str(&ptext).unwrap();
        assert_eq!(p["state"], "nocs");
        assert_eq!(p["selectable"], true);
        assert_eq!(p["dirs"].as_array().unwrap().len(), 1);
        assert_eq!(p["files_skipped"], 0);

        // A failure lands on the distinct variant through the same round trip.
        let gone = root.join("nope");
        pv.go(&gone.to_string_lossy());
        let bad: Value =
            serde_json::from_str(&serde_json::to_string(&pv.facts().unwrap()).unwrap()).unwrap();
        assert_eq!(bad["state"], "notfound");
        assert_eq!(bad["selectable"], false);
        assert_ne!(bad["state"], "empty");
        std::fs::remove_dir_all(&root).ok();
    }
}

#[cfg(test)]
mod path_check {
    /// Proves the paths the Lead's nzk_ui.rs/actions.rs hooks will use actually
    /// resolve from OUTSIDE this module, so the integration compiles.
    #[test]
    fn crate_root_paths_resolve_from_other_modules() {
        // actions.rs will write `crate::find_wire::FindView`.
        let v = crate::find_wire::FindView::new();
        assert_eq!(v.query(), "");
        // nzk_ui.rs will write `crate::find_wire::PickerView` and `.facts()`.
        let p = crate::find_wire::PickerView::rooted_at(std::path::Path::new("/tmp"));
        assert!(p.facts().is_none(), "an unlisted picker has no facts");
        // The free fns too.
        let _f: fn(&crate::model::Graph, &str) -> serde_json::Value = crate::find_wire::find_facts;
    }
}

#[cfg(test)]
mod suggestions_tests {
    use super::*;
    use crate::model::Node;

    fn node(id: &str, label: &str, full: &str, kind: &str, ns: &str) -> Node {
        Node {
            id: id.to_string(),
            label: label.to_string(),
            full: full.to_string(),
            kind: kind.to_string(),
            ns: ns.to_string(),
            ..Node::default()
        }
    }

    /// A graph with three `hit` matches across different kinds/namespaces.
    fn fixture() -> Graph {
        Graph {
            repo: PathBuf::from("/tmp/suggestion_fixture"),
            nodes: vec![
                node("n:1", "hit", "App.Hit", "method", "App"),
                node("n:2", "hitsAll", "App.HitsAll", "field", "App"),
                node("n:3", "h_i_t", "Other.HIT", "class", "Other"),
                node("n:4", "other", "App.Other", "method", "App"),
            ],
            edges: Vec::new(),
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        }
    }

    /// ADVERSARIAL: an empty (or whitespace-only) query must produce **no facts
    /// at all**, because `ui.suggestions` absent is what leaves the shipped
    /// `class="suggestions hidden"` box byte-identical (and keeps `@frame`
    /// pinned). Clearing the graph-mode query is therefore exactly how the
    /// producer hides the popup — there is no `"open": false` flag to forget.
    #[test]
    fn suggestion_view_no_query_has_no_facts() {
        let view = SuggestionView::new();
        assert!(view.facts(&fixture()).is_none());
        let mut ws = SuggestionView::new();
        ws.set_query("   ");
        assert!(
            ws.facts(&fixture()).is_none(),
            "whitespace is NoQuery (the core trims), so the key must stay absent"
        );
        // The honest NoQuery state is still reachable from the view itself.
        assert!(matches!(ws.outcome(&fixture()), Outcome::NoQuery));
    }

    /// ADVERSARIAL: `NoHits` must publish the reference's own message, not a
    /// bare empty `rows` array. If the producer routed the state through
    /// `rows.is_empty()` this test would still pass on `rows`, which is why the
    /// `empty` field is asserted as the discriminator (`search.js:23-27`).
    #[test]
    fn suggestion_view_no_hits_has_empty_message_not_empty_rows() {
        let mut view = SuggestionView::new();
        view.set_query("zzzz-no-such-symbol");
        let facts = view.facts(&fixture()).expect("a real query publishes facts");
        assert_eq!(facts["rows"].as_array().unwrap().len(), 0);
        assert_eq!(facts["empty"], serde_json::json!("no matches"));
        assert_eq!(facts["active"], serde_json::Value::Null);
    }

    /// The frozen row shape from `renderSuggestions` (search.js:29-37):
    /// exactly `id`/`kind`/`label`/`full`, core order, <= DEFAULT_LIMIT.
    /// Removing a field or adding one would break the renderer seam.
    #[test]
    fn suggestion_view_hits_row_shape_is_the_reference_shape() {
        let mut view = SuggestionView::new();
        view.set_query("hit");
        let facts = view.facts(&fixture()).expect("hits publish facts");
        let rows = facts["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 3, "the fixture's three 'hit' matches");
        assert!(
            rows.len() <= search::DEFAULT_LIMIT,
            "the popup uses the core's 30-row default limit (search.js:42 limit=30)"
        );
        for row in rows {
            let obj = row.as_object().expect("a row object");
            let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                vec!["full", "id", "kind", "label"],
                "the row shape is frozen; the paint reads exactly these four"
            );
            for k in ["id", "kind", "label", "full"] {
                assert!(row[k].is_string(), "row.{k} must be a string");
            }
        }
        assert_eq!(facts["empty"], serde_json::Value::Null);
        // Core order is preserved, never re-sorted: exact match first.
        assert_eq!(rows[0]["id"], serde_json::json!("n:1"));
    }

    /// `moveSuggestion`'s wrap rule (search.js:64-72), transcribed exactly:
    /// `-1`/None is a sentinel that down -> 0, up -> last, down past last -> -1,
    /// up from 0 -> -1. A stale highlight must be cleared by a new query.
    #[test]
    fn suggestion_view_move_active_wraps_like_the_reference() {
        let mut view = SuggestionView::new();
        assert_eq!(view.active(), None, "no selection before any move");
        view.move_active(1, 3);
        assert_eq!(view.active(), Some(0), "down from -1 selects the first row");
        view.move_active(1, 3);
        assert_eq!(view.active(), Some(1));
        view.move_active(1, 3);
        assert_eq!(view.active(), Some(2));
        view.move_active(1, 3);
        assert_eq!(view.active(), None, "down past the last row returns to -1");
        view.move_active(-1, 3);
        assert_eq!(view.active(), Some(2), "up from -1 selects the last row");
        view.move_active(-1, 3);
        assert_eq!(view.active(), Some(1));
        view.set_active(Some(0));
        view.move_active(-1, 3);
        assert_eq!(view.active(), None, "up from 0 returns to -1");
        // An empty result list never changes the highlight.
        view.set_active(Some(1));
        view.move_active(1, 0);
        assert_eq!(view.active(), Some(1));
        // Any new query resets the selection (search.js:47 `searchIndex = -1`).
        view.set_query("other");
        assert_eq!(view.active(), None);
        view.push_char('s');
        assert_eq!(view.active(), None);
        view.pop_char();
        assert_eq!(view.active(), None);
        view.clear();
        assert_eq!(view.active(), None);
    }

    /// ADVERSARIAL: the producer must never name a row it did not publish. A
    /// highlight left over from a larger result set is clamped away rather than
    /// serialised, or the paint would mark no row while the facts claimed one.
    #[test]
    fn suggestion_view_active_is_never_published_out_of_range() {
        let mut view = SuggestionView::new();
        view.set_query("hit");
        view.set_active(Some(99));
        let facts = view.facts(&fixture()).unwrap();
        assert_eq!(
            facts["active"],
            serde_json::Value::Null,
            "an out-of-range index is not published"
        );
        view.set_active(Some(1));
        let facts = view.facts(&fixture()).unwrap();
        assert_eq!(facts["active"], serde_json::json!(1));
    }

    /// ADVERSARIAL: the label chain is label -> id (search.js:34), not
    /// `SearchResult::title()`'s full -> label -> id. A node with a full name but
    /// no label must publish its **id** as the label (and the full name in
    /// `full`), proving the two chains are distinct.
    #[test]
    fn suggestion_label_falls_back_to_id_never_to_full() {
        let graph = Graph {
            repo: PathBuf::from("/tmp/suggestion_label_fixture"),
            nodes: vec![node("n:no-label", "", "App.FullOnly", "method", "App")],
            edges: Vec::new(),
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let mut view = SuggestionView::new();
        view.set_query("FullOnly");
        let facts = view.facts(&graph).expect("full-name hit publishes facts");
        let row = &facts["rows"][0];
        assert_eq!(row["label"], serde_json::json!("n:no-label"));
        assert_eq!(row["full"], serde_json::json!("App"));
        // `title()` would have returned the full name here; that is the bug this
        // assertion exists to catch.
        assert_ne!(row["label"], serde_json::json!("App.FullOnly"));
    }

    /// The popup's `keydown` table (shell.js:508-534) claims exactly three keys
    /// and nothing else. A route that also ate `Escape`/letters would break the
    /// producers; one that missed `Enter` would leave the commit unreachable.
    #[test]
    fn suggestion_key_router_claims_only_the_reference_keys() {
        use super::{route_key, SuggestKey};
        assert_eq!(route_key("ArrowDown"), Some(SuggestKey::Move(1)));
        assert_eq!(route_key("ArrowUp"), Some(SuggestKey::Move(-1)));
        assert_eq!(route_key("Enter"), Some(SuggestKey::Activate));
        for other in ["Escape", "Backspace", "Tab", "PageDown", "PageUp", "a", ""] {
            assert_eq!(route_key(other), None, "{other} must fall through");
        }
    }

    /// `hideSuggestions` (search.js:11-20) leaves `#searchInput.value` alone and
    /// only resets `state.searchIndex = -1` — the 7A gap 3 fix. The query must
    /// survive (so `ui.search_query` keeps painting the box) while the popup
    /// closes by absence and the highlight is gone.
    #[test]
    fn suggestion_hide_keeps_the_typed_query_and_resets_the_highlight() {
        let graph = fixture();
        let mut view = SuggestionView::new();
        view.set_query("hit");
        view.set_active(Some(1));
        assert!(view.is_open());
        assert!(view.facts(&graph).is_some());

        view.hide();
        assert_eq!(view.query(), "hit", "hide must NOT clear the typed text");
        assert!(view.is_open(), "the query is still there, so typing/focus reopens");
        assert_eq!(view.active(), None, "hideSuggestions resets searchIndex to -1");
        let facts = view.facts(&graph).expect("the query still publishes rows");
        assert_eq!(facts["active"], serde_json::Value::Null);
        assert_eq!(
            view.outcome(&graph).results().len(),
            3,
            "the hidden popup still describes the same query"
        );

        // `clear()` is the other contract: the query itself is gone.
        view.clear();
        assert!(!view.is_open());
        assert!(view.facts(&graph).is_none(), "a cleared query closes by absence");
    }

    /// `commitSearch` (search.js:76-84) commits `searchIndex >= 0 ? it : 0`; the
    /// resolved value is the `graph.nodes` index of that row's node id, never a
    /// list position guessed from the facts order alone.
    #[test]
    fn suggestion_commit_index_selects_the_highlighted_row_or_row_zero() {
        let graph = fixture();
        let mut view = SuggestionView::new();
        view.set_query("zzzz-no-such-symbol");
        assert_eq!(
            view.commit_index(&graph),
            None,
            "no rows to commit -> the caller must only hide"
        );

        view.set_query("hit");
        assert_eq!(view.active(), None);
        assert_eq!(
            view.commit_index(&graph),
            Some(0),
            "Enter with no selection commits row 0 (n:1 is nodes[0])"
        );
        view.set_active(Some(1));
        assert_eq!(
            view.commit_index(&graph),
            Some(1),
            "the highlight is committed, not row 0"
        );
        // A highlight past the (current) result set is not committed.
        view.set_active(Some(99));
        assert_eq!(view.commit_index(&graph), None);
    }

    /// The commit is resolved by node **id** against the current graph, never by a
    /// remembered list position: after a re-index the same query resolves to the
    /// new graph's own row, and a query with no row at all is a `None` the host
    /// turns into hide-only (no selection, no guess).
    #[test]
    fn suggestion_commit_index_resolves_against_the_current_graph() {
        let mut view = SuggestionView::new();
        view.set_query("hit");
        view.set_active(Some(0));
        assert_eq!(view.commit_index(&fixture()), Some(0));

        let reindexed = Graph {
            repo: PathBuf::from("/tmp/suggestion_reindexed"),
            nodes: vec![node("n:99", "hit", "New.Hit", "method", "New")],
            edges: Vec::new(),
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        assert_eq!(
            view.commit_index(&reindexed),
            Some(0),
            "the row is resolved in the CURRENT graph (n:99), not a cached position"
        );
        assert_eq!(reindexed.nodes[0].id, "n:99");

        let no_match = Graph {
            repo: PathBuf::from("/tmp/suggestion_reindexed"),
            nodes: vec![node("n:99", "other", "New.Other", "method", "New")],
            edges: Vec::new(),
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        assert_eq!(
            view.commit_index(&no_match),
            None,
            "nothing to resolve -> hide-only, never a guessed node"
        );
    }

    /// The click path commits the id the row's `data-id` carries, resolved in
    /// the current graph. A row whose node disappeared on a re-index is a
    /// no-op (`None`), never a neighbouring node — the same stale-id rule as
    /// `commit_index`, but keyed by the CLICKED id rather than the highlight.
    #[test]
    fn suggestion_commit_by_id_resolves_the_clicked_row_only() {
        let graph = fixture();
        let view = SuggestionView::new();
        // The id the popup's `data-id` names commits, regardless of highlight.
        let id = graph.nodes[2].id.clone();
        let index = view
            .commit_by_id(&graph, &id)
            .expect("a node the graph really has must resolve");
        assert_eq!(graph.nodes[index].id, id);
        // A stale id (the row outlived a re-index) resolves to nothing.
        assert_eq!(view.commit_by_id(&graph, "n:gone"), None);
        // An empty id is never a wildcard.
        assert_eq!(view.commit_by_id(&graph, ""), None);
    }
}

// ---------------------------------------------------------------------------
// engine-backed Find providers (v2 search.js:144-351)
//
// Round-17 parity gap (notes/round17-parity-audit.md §6): the Find panel had
// free-text hits only. v2's `runFind` calls `GET /api/find` and renders three
// grouped providers — definitions, implementations and usages — then
// `activateFindRow` opens a node (defs/impls) or a file:line (usages)
// (`search.js:144-172`, `181-250`, `305-351`, `394-406`).
//
// The engine already implements that exact route: `Index::find` (`core/src/
// index.rs:2379`) returns `{ok, query, simple, definitions, implementations,
// usages, truncated}` with `find_row` rows (`index.rs:2498`) and usage rows
// carrying `{file, line, col, text, node, match}`. So these providers are
// **pass-throughs plus a display mapping** — nothing here invents a row, a
// count or a target. The mapping only renames the engine's own fields into the
// `.frow` shape `buildDefRow`/`buildUsageRow` paint, and carries the click
// target each row already has (`node.id`, or `file`/`line` for a usage).
// ---------------------------------------------------------------------------

/// One display row of a Find group — `buildDefRow`/`buildUsageRow`'s fields
/// (`search.js:181-250`) with the engine values behind them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindRow {
    /// `definition`, `implementation` or `usage` — the engine's own tag.
    pub kind: String,
    /// The headline: `full || label || id` (`search.js:187`).
    pub title: String,
    /// The sub-line: `label · file:line` for defs/impls, `in <owner>` shape is
    /// kept as the engine's own `node` label for usages.
    pub subtitle: String,
    /// Click target for a symbol row (`openFindNode`, `search.js:393-398`).
    pub node_id: String,
    /// Click target for a usage row (`openUsage`, `search.js:400-406`).
    pub file: String,
    pub line: i64,
    pub col: i64,
    /// The usage's own source line, verbatim from the engine.
    pub text: String,
    /// `exact` or `simple` (`applyUsage`, `search.js:400-406`).
    pub match_kind: String,
}

/// One rendered Find group. Only non-empty groups are produced, matching
/// `renderFind`'s `if (defs.length) frag.appendChild(…)` (`search.js:349-351`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindGroup {
    /// `definitions` / `implementations` / `usages` (`search.js:254`).
    pub kind: &'static str,
    /// The group head text (`search.js:257-262`).
    pub heading: &'static str,
    pub rows: Vec<FindRow>,
}

/// The three groups in v2's order (`search.js:349-351`).
pub const FIND_GROUP_ORDER: [(&str, &str); 3] = [
    ("definitions", "Definitions"),
    ("implementations", "Implementations"),
    ("usages", "Usages"),
];

/// The engine's own `GET /api/find` answer (`Index::find`).
///
/// A one-line pass-through on purpose: the classification (definition vs
/// implementation vs usage), the ranking, the limit and `truncated` all belong
/// to the core (`core/src/index.rs:2379-2505`). This wrapper exists only so the
/// app has an in-process seam to call instead of an HTTP route.
pub fn engine_find(index: &mut csgraph_core::index::Index, query: &str, limit: i64) -> Value {
    index.find(query, limit)
}

/// **The find-references entry point for a source-language provider (task-16).**
///
/// This is the engine's own `GET /api/refs` (`core/src/api.rs` `/api/refs`):
/// `Index::text_hits` returns every literal occurrence of `query` with
/// `{file, line, col, text, node}`, plus the honest `truncated` flag. Same
/// payload the core's route serves — no re-ranking, no fabrication.
pub fn references(index: &mut csgraph_core::index::Index, query: &str, limit: i64) -> Value {
    let (usages, truncated) = index.text_hits(query, limit);
    json!({"ok": true, "usages": usages, "truncated": truncated})
}

fn row_str(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

fn row_int(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Map the engine's `/api/find` payload to v2's three display groups.
///
/// Every field is copied from the engine row; the only decisions are the ones
/// `search.js` itself makes:
/// * the headline is `full || label || id` (`labelOf`'s fallback chain);
/// * a def/impl row's sub-line joins `label` (when it differs from the
///   headline) with `file:line` using `  ·  ` (`search.js:199-205`);
/// * a usage row keeps the engine's `text`, `match`, `file:line` and the
///   enclosing node's label (`search.js:214-250`);
/// * empty groups are dropped (`search.js:349-351`).
pub fn groups_of(payload: &Value) -> Vec<FindGroup> {
    let mut out: Vec<FindGroup> = Vec::new();
    for (kind, heading) in FIND_GROUP_ORDER {
        let Some(items) = payload.get(kind).and_then(Value::as_array) else {
            continue;
        };
        let mut rows: Vec<FindRow> = Vec::with_capacity(items.len());
        for item in items {
            if kind == "usages" {
                let node_label = item
                    .get("node")
                    .and_then(|n| n.get("label").or_else(|| n.get("name")))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                rows.push(FindRow {
                    kind: row_str(item, "kind"),
                    title: format!(
                        "{}:{}",
                        row_str(item, "file"),
                        row_int(item, "line")
                    ),
                    subtitle: if node_label.is_empty() {
                        String::new()
                    } else {
                        format!("in {node_label}")
                    },
                    node_id: item
                        .get("node")
                        .and_then(|n| n.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    file: row_str(item, "file"),
                    line: row_int(item, "line"),
                    col: row_int(item, "col"),
                    text: row_str(item, "text"),
                    match_kind: row_str(item, "match"),
                });
            } else {
                let id = row_str(item, "id");
                let full = row_str(item, "full");
                let label = row_str(item, "label");
                let title = if !full.is_empty() {
                    full.clone()
                } else if !label.is_empty() {
                    label.clone()
                } else {
                    id.clone()
                };
                let mut bits: Vec<String> = Vec::new();
                if !label.is_empty() && label != title {
                    bits.push(label.clone());
                }
                let file = row_str(item, "file");
                let line = row_int(item, "line");
                if !file.is_empty() {
                    bits.push(if line > 0 {
                        format!("{file}:{line}")
                    } else {
                        file.clone()
                    });
                }
                rows.push(FindRow {
                    kind: row_str(item, "kind"),
                    title,
                    subtitle: bits.join("  \u{b7}  "),
                    node_id: id,
                    file,
                    line,
                    col: 0,
                    text: String::new(),
                    match_kind: row_str(item, "match"),
                });
            }
        }
        if rows.is_empty() {
            continue;
        }
        out.push(FindGroup {
            kind,
            heading,
            rows,
        });
    }
    out
}

/// The `groups` value the DOM layer consumes: one object per non-empty group
/// with its heading, its engine count and its rows.
pub fn groups_value(payload: &Value) -> Value {
    let groups: Vec<Value> = groups_of(payload)
        .into_iter()
        .map(|g| {
            let rows: Vec<Value> = g
                .rows
                .iter()
                .map(|r| {
                    json!({
                        "kind": r.kind,
                        "title": r.title,
                        "subtitle": r.subtitle,
                        "node_id": r.node_id,
                        "file": r.file,
                        "line": r.line,
                        "col": r.col,
                        "text": r.text,
                        "match": r.match_kind,
                    })
                })
                .collect();
            json!({
                "kind": g.kind,
                "heading": g.heading,
                "count": g.rows.len(),
                "rows": rows,
            })
        })
        .collect();
    json!({
        "groups": groups,
        "total": groups_of(payload).iter().map(|g| g.rows.len()).sum::<usize>(),
        "truncated": payload.get("truncated").and_then(Value::as_bool).unwrap_or(false),
        "simple": row_str(payload, "simple"),
    })
}

#[cfg(test)]
mod find_modes_tests {
    use super::*;

    /// An `/api/find` payload in the exact shape `Index::find` emits
    /// (`core/src/index.rs:2379-2505`, `find_row` at `:2498`): defs/impls are
    /// `lite` node rows (`id/label/full/kind/file/line/degree`) plus `match`;
    /// usages are `{file,line,col,text,node,match,kind:"usage"}`.
    fn engine_payload() -> Value {
        json!({
            "ok": true,
            "query": "Build",
            "simple": "Build",
            "definitions": [
                {"id": "T:Build", "label": "Build", "full": "NZK.Build",
                 "kind": "method", "file": "a.cs", "line": 10, "match": "definition"},
                {"id": "T:Builder", "label": "Builder", "full": "NZK.Builder",
                 "kind": "class", "file": "", "line": 0, "match": "definition"}
            ],
            "implementations": [
                {"id": "T:IBuild", "label": "Build", "full": "NZK.IBuild",
                 "kind": "interface", "file": "b.cs", "line": 3, "match": "implementation"}
            ],
            "usages": [
                {"file": "a.cs", "line": 42, "col": 7,
                 "text": "var x = Build();",
                 "node": {"id": "T:Ctx", "label": "Ctx", "kind": "method"},
                 "match": "exact", "kind": "usage"}
            ],
            "truncated": false
        })
    }

    /// The three v2 groups come out in `search.js:349-351` order, with the
    /// engine's own counts and the real click targets.
    #[test]
    fn groups_follow_the_reference_order_and_carry_click_targets() {
        let groups = groups_of(&engine_payload());
        let kinds: Vec<&str> = groups.iter().map(|g| g.kind).collect();
        assert_eq!(kinds, vec!["definitions", "implementations", "usages"]);
        assert_eq!(groups[0].heading, "Definitions");
        assert_eq!(groups[0].rows.len(), 2);
        assert_eq!(groups[0].rows[0].title, "NZK.Build", "full wins the label chain");
        assert_eq!(groups[0].rows[0].subtitle, "Build  \u{b7}  a.cs:10");
        assert_eq!(groups[0].rows[0].node_id, "T:Build", "the node click target");
        assert_eq!(groups[0].rows[0].kind, "method", "the engine's node kind");
        // A row with no file:line still shows its short label when that label
        // differs from the headline — `buildDefRow`'s `bits` is label then
        // `file:line` (`search.js:199-205`); with no file, label only.
        assert_eq!(groups[0].rows[1].title, "NZK.Builder");
        assert_eq!(groups[0].rows[1].subtitle, "Builder");
        assert_eq!(groups[0].rows[1].file, "", "the engine gave no file for this row");

        let usage = &groups[2].rows[0];
        assert_eq!(usage.title, "a.cs:42", "the file:line headline");
        assert_eq!(usage.text, "var x = Build();", "the engine's source line");
        assert_eq!(usage.match_kind, "exact");
        assert_eq!(usage.file, "a.cs");
        assert_eq!(usage.line, 42);
        assert_eq!(usage.col, 7);
        assert_eq!(usage.node_id, "T:Ctx", "the enclosing node");
        assert_eq!(usage.subtitle, "in Ctx");
    }

    /// Empty groups are dropped (`search.js:349-351`); an engine "nothing
    /// found" answer therefore renders no group heads at all.
    #[test]
    fn empty_groups_are_dropped() {
        let payload = json!({"ok": true, "query": "zzz", "simple": "zzz",
                             "definitions": [], "implementations": [],
                             "usages": [], "truncated": false});
        assert!(groups_of(&payload).is_empty());
        let v = groups_value(&payload);
        assert_eq!(v["groups"].as_array().unwrap().len(), 0);
        assert_eq!(v["total"], 0);
        assert_eq!(v["truncated"], false);
    }

    /// The DOM-facing projection carries `count`, `heading`, and per-row
    /// `file`/`line`/`col`/`text`/`node_id` — the fields the renderer needs to
    /// build `.fgroup-head`/`.fcount`/`.frow` and the click attributes.
    #[test]
    fn groups_value_carries_counts_and_row_targets() {
        let v = groups_value(&engine_payload());
        let groups = v["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0]["heading"], "Definitions");
        assert_eq!(groups[0]["count"], 2);
        assert_eq!(groups[2]["kind"], "usages");
        assert_eq!(groups[2]["rows"][0]["file"], "a.cs");
        assert_eq!(groups[2]["rows"][0]["line"], 42);
        assert_eq!(groups[2]["rows"][0]["node_id"], "T:Ctx");
        assert_eq!(groups[0]["rows"][1]["node_id"], "T:Builder");
        assert_eq!(v["total"], 4, "2 defs + 1 impl + 1 usage");
        assert_eq!(v["simple"], "Build", "the engine's simple term survives");
    }
}
