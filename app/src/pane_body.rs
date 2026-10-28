//! The real body model of a **detached pane window** (task-6).
//!
//! `pane_ui` (app.rs) used to paint `"detached <kind> pane — real OS window"`
//! for every pane kind except Source. That was a placeholder, not content. This
//! module is the replacement's pure half: given the same live state the main
//! window reads, it decides what a detached pane's body actually *is*.
//!
//! There are four pane kinds (`crate::windows::Pane`), and in this fork the
//! main window's bodies for them are split across two layers:
//!
//! * **Source** — a real egui pane (`source_pane::ui`) painted into `#sourceBody`
//!   in the main window. Its body needs the mutable `SourceState`, so
//!   [`PaneBody::Source`] only marks the kind; `pane_ui` keeps hosting it.
//! * **Node** — the main window's `#nodeDrawer` content is a nzk_ui DOM
//!   skeleton; the live Rust model behind it is the selected `GraphView` node,
//!   which is exactly what the non-nzk_ui fallback side panel reads. The
//!   detached window paints that same node model.
//! * **Find** — the main window's `#findResults` is filled by
//!   `find_wire::FindView::facts_value`, which runs `search::search_default` and
//!   keeps the core's three honest states. [`find_body`] is that same producer,
//!   projected into owned row data for egui.
//! * **Blocks** — this build has **no** block-canvas model: the main window's
//!   `#blocksPanel` ships with an empty `#blocksStage`/`#blocksCards` and
//!   `scratch.js` was never ported. The honest body is therefore the panel's own
//!   static head plus an empty-stage message — never fabricated rows.
//!
//! **Display-free by construction**: nothing here mentions egui, a window, or a
//! GPU, so every kind's mapping is unit-tested headlessly. `pane_ui` is then a
//! dumb renderer of the returned model; egui window painting itself is not
//! headless-testable, and the tests below deliberately stop at the model.

use crate::find_wire::FindView;
use crate::model::Node;
use crate::search::{Outcome, SearchResult};
use crate::view::GraphView;
use crate::windows::Pane;

/// The real body a detached pane window should paint.
pub enum PaneBody<'g> {
    /// The Source pane: a live [`crate::app::source_pane::SourceState`] surface.
    /// The mutable document is hosted by `pane_ui`; this variant only names the
    /// kind so [`pane_body`] is total over [`Pane::ALL`].
    Source,
    /// The Node drawer's body: the selected node's real properties.
    Node(NodeBody<'g>),
    /// The Find panel's body: the search core's real outcome.
    Find(FindBody),
    /// The Blocks panel's body: static panel head + honest empty stage.
    Blocks(BlocksBody),
}

/// The Node drawer's real content.
#[derive(Clone, Copy, Debug)]
pub struct NodeBody<'g> {
    /// The selected node in the graph the main window is showing, if any.
    pub node: Option<&'g Node>,
    /// Shown when nothing is selected — the main window's own state then (its
    /// drawer has no detail to show, so it stays closed).
    pub empty: &'static str,
}

/// The Find panel's real content, owned so it can outlive the `Outcome` borrow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindBody {
    /// The query as typed (`FindView::query`).
    pub query: String,
    /// `noquery` / `hits` / `nohits` — the core's own state tags, the same
    /// strings `find_wire` puts on the wire.
    pub state: &'static str,
    /// `Outcome::meta()` — `""` for NoQuery by the core's own choice.
    pub meta: String,
    /// `Outcome::notice()` — the state-specific empty-state sentence.
    pub notice: String,
    /// Real result rows, in the core's own order. Never synthesised.
    pub rows: Vec<FindRow>,
}

/// One Find result, mirroring the row `find_wire::facts_value` publishes to
/// `#findResults` (`search.js`'s `.frow`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindRow {
    pub node_id: String,
    /// `SearchResult::title()` (`full || label || id`).
    pub title: String,
    /// `SearchResult::subtitle()` — empty means the reference omits the span.
    pub subtitle: String,
    pub kind: String,
    /// `MatchKind::tag()` — `exact`/`prefix`/`substring`/`subseq`.
    pub tag: &'static str,
}

/// The Blocks panel's body. This build has no block model (see module docs), so
/// the model is the shipped panel's static head and no rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlocksBody {
    /// `#blocksTitle` text as shipped.
    pub title: &'static str,
    /// `#blocksMeta` is empty in the shipped skeleton.
    pub meta: &'static str,
    /// The shipped head hint.
    pub hint: &'static str,
    /// The honest empty stage — no card is fabricated.
    pub empty: &'static str,
}

/// The detached Find window's **own** editable input state
/// (`panewin.js:70-98 addFindInput`): v2 gives the pane a real `<input
/// class="pane-find-input">` whose value is seeded from and kept in step with
/// the shared `state.find.query`, and typing calls the same `runFind`. So the
/// widget/focus is pane-local, the query is the shared [`FindView`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FindInput {
    /// The pane-local text buffer (seeded from `FindView::query`).
    pub text: String,
    /// The pane-local widget has keyboard focus.
    pub focused: bool,
}

/// One routed keydown from the Pane-role window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindKey {
    /// A printable character, appended to the buffer.
    Char(char),
    /// Backspace: drop the last character.
    Backspace,
    /// Enter: run the current query (`runFind(q)`).
    Enter,
    /// Escape: clear the query back to the NoQuery state.
    Escape,
    /// Every other key: no find effect.
    Other,
}

/// What a routed key does to the find model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindAction {
    /// The buffer changed: `FindView::set_query(text)`.
    Query,
    /// Enter pressed: the query is submitted (`runFind`).
    Run,
    /// Escape pressed: clear.
    Clear,
    /// Nothing to do.
    None,
}

impl FindInput {
    /// Seed the pane-local buffer from the shared query (`panewin.js:93`).
    pub fn new(query: &str) -> FindInput {
        FindInput { text: query.to_string(), focused: false }
    }

    /// `name` is the key name as the host reports it (`"Backspace"`, `"Enter"`,
    /// `"Escape"`); a one-character name is a printable char.
    pub fn route_key(&mut self, name: &str) -> FindAction {
        match name {
            "Backspace" => {
                self.text.pop();
                FindAction::Query
            }
            "Enter" => FindAction::Run,
            "Escape" => {
                self.text.clear();
                FindAction::Clear
            }
            other if other.chars().count() == 1 => {
                if let Some(c) = other.chars().next() {
                    if !c.is_control() {
                        self.text.push(c);
                        return FindAction::Query;
                    }
                }
                FindAction::None
            }
            _ => FindAction::None,
        }
    }

    /// The query text to give the shared `FindView` after a change.
    pub fn query(&self) -> &str {
        &self.text
    }
}

/// Text shown by a detached Node window when the graph has no selection.
pub const NODE_EMPTY: &str = "no node selected in the main window";

/// Text shown by a detached Blocks window: the panel's stage has no producer.
pub const BLOCKS_EMPTY: &str =
    "no block cards — this build has no Blocks canvas model (the main window's stage is empty too)";

/// Decide the real body of `pane` from the live state the main window shows.
///
/// Pure: `view` supplies the selected node, `find` is the same `FindView` the
/// main window's `#findPanel` is projected from. `Pane::Source` is a marker
/// because the live `SourceState` is mutable and stays in `pane_ui`.
pub fn pane_body<'g>(pane: Pane, view: &'g GraphView, find: &FindView) -> PaneBody<'g> {
    match pane {
        Pane::Source => PaneBody::Source,
        Pane::NodeDrawer => PaneBody::Node(NodeBody {
            node: selected_node(view),
            empty: NODE_EMPTY,
        }),
        Pane::Find => PaneBody::Find(find_body(view, find)),
        Pane::Blocks => PaneBody::Blocks(BlocksBody {
            title: "Blocks",
            meta: "",
            hint: "click a block to navigate",
            empty: BLOCKS_EMPTY,
        }),
    }
}

/// The node the main window is showing, if the selection is still in range.
fn selected_node(view: &GraphView) -> Option<&Node> {
    view.selected.and_then(|i| view.graph.nodes.get(i))
}

/// The Find panel's real body: the same `search_default` outcome the main
/// window publishes (`find_wire::FindView::facts_value`).
fn find_body(view: &GraphView, find: &FindView) -> FindBody {
    let outcome = find.outcome(&view.graph);
    // Borrow the outcome's rows rather than moving its `Vec` out: `meta()` and
    // `notice()` still need it, and they are the core's own state copy.
    let (state, rows): (&'static str, Vec<FindRow>) = match &outcome {
        Outcome::NoQuery => ("noquery", Vec::new()),
        Outcome::NoHits => ("nohits", Vec::new()),
        Outcome::Hits(hits) => ("hits", hits.iter().map(find_row).collect()),
    };
    FindBody {
        query: find.query().to_string(),
        state,
        meta: outcome.meta(),
        notice: outcome.notice(),
        rows,
    }
}

fn find_row(hit: &SearchResult<'_>) -> FindRow {
    FindRow {
        node_id: hit.node.id.clone(),
        title: hit.title().to_string(),
        subtitle: hit.subtitle(),
        kind: hit.node.kind.clone(),
        tag: hit.kind.tag(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, Graph};
    use crate::find_wire::FindView;
    use crate::view::GraphView;
    use std::path::PathBuf;

    fn view() -> GraphView {
        let graph = Graph {
            repo: PathBuf::from("/tmp/does-not-exist"),
            nodes: vec![
                Node {
                    id: "A".into(),
                    label: "Widget".into(),
                    full: "Demo.Widget".into(),
                    kind: "class".into(),
                    ns: "Demo".into(),
                    file: "src/widget.cs".into(),
                    line: 3,
                    end_line: 9,
                    degree: 2,
                    ..Default::default()
                },
                Node {
                    id: "B".into(),
                    label: "NormaliseName".into(),
                    full: "Demo.Widget.NormaliseName".into(),
                    kind: "method".into(),
                    ns: "Demo".into(),
                    owner: "Widget".into(),
                    file: "src/widget.cs".into(),
                    line: 5,
                    degree: 1,
                    ..Default::default()
                },
            ],
            edges: vec![Edge { from: 0, to: 1, kind: "contains".into(), count: 1 }],
            state: serde_json::json!({"roots": ["."], "indexed": true, "fileCount": 1}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        GraphView::new(graph)
    }

    /// Every pane kind in `Pane::ALL` must map to a real body, not a placeholder:
    /// Source is the live surface; the other three are their own models.
    #[test]
    fn every_pane_kind_has_a_real_body_and_never_a_placeholder() {
        let v = view();
        let find = FindView::new();
        let mut seen = Vec::new();
        for p in Pane::ALL {
            match pane_body(p, &v, &find) {
                PaneBody::Source => seen.push("source"),
                PaneBody::Node(_) => seen.push("node"),
                PaneBody::Find(_) => seen.push("find"),
                PaneBody::Blocks(_) => seen.push("blocks"),
            }
            assert_eq!(pane_body(p, &v, &find).kind_tag(), p.id());
        }
        assert_eq!(seen, vec!["source", "node", "find", "blocks"]);
    }

    #[test]
    fn node_drawer_carries_the_selected_node_and_is_honestly_empty_without_one() {
        let mut v = view();
        assert!(v.selected.is_none(), "fresh view has no selection");
        match pane_body(Pane::NodeDrawer, &v, &FindView::new()) {
            PaneBody::Node(b) => {
                assert!(b.node.is_none());
                assert_eq!(b.empty, NODE_EMPTY);
            }
            _ => panic!("nodeDrawer mapped to the wrong body"),
        }

        v.selected = Some(1);
        match pane_body(Pane::NodeDrawer, &v, &FindView::new()) {
            PaneBody::Node(b) => {
                let n = b.node.expect("selection reaches the detached drawer");
                // The real node, not a copy or a placeholder row.
                assert_eq!(n.id, "B");
                assert_eq!(n.full, "Demo.Widget.NormaliseName");
                assert_eq!(n.kind, "method");
                assert_eq!(n.file, "src/widget.cs");
                assert_eq!(n.line, 5);
                assert_eq!(n.degree, 1);
            }
            _ => panic!("nodeDrawer mapped to the wrong body"),
        }
    }

    #[test]
    fn node_drawer_treats_a_stale_selection_index_as_empty_not_a_panic() {
        let mut v = view();
        v.selected = Some(99);
        match pane_body(Pane::NodeDrawer, &v, &FindView::new()) {
            PaneBody::Node(b) => {
                assert!(b.node.is_none());
                assert_eq!(b.empty, NODE_EMPTY);
            }
            _ => panic!("nodeDrawer mapped to the wrong body"),
        }
    }

    #[test]
    fn detached_find_input_types_routes_backspace_enter_and_escape() {
        // Seeded from the shared query (`panewin.js:93`), then pane-local edits.
        let mut input = FindInput::new("Wid");
        assert_eq!(input.query(), "Wid");
        assert!(!input.focused);
        input.focused = true;
        assert_eq!(input.route_key("g"), FindAction::Query);
        assert_eq!(input.route_key("e"), FindAction::Query);
        assert_eq!(input.route_key("t"), FindAction::Query);
        assert_eq!(input.query(), "Widget");
        assert_eq!(input.route_key("Backspace"), FindAction::Query);
        assert_eq!(input.query(), "Widge");
        // Enter submits exactly the typed text; it does not mutate it.
        assert_eq!(input.route_key("Enter"), FindAction::Run);
        assert_eq!(input.query(), "Widge");
        // Escape clears back to the core's NoQuery state.
        assert_eq!(input.route_key("Escape"), FindAction::Clear);
        assert_eq!(input.query(), "");
        // Non-printable / unknown names are inert, never swallowed as text.
        assert_eq!(input.route_key("F5"), FindAction::None);
        assert_eq!(input.route_key("\u{7f}"), FindAction::None);
    }

    #[test]
    fn detached_find_typing_drives_the_same_shared_find_model() {
        let v = view();
        let mut find = FindView::new();
        let mut input = FindInput::new(find.query());
        for name in ["W", "i", "d", "g", "e", "t"] {
            input.route_key(name);
        }
        find.set_query(input.query());
        // The detached pane's typing reaches the main window's model: the same
        // `FindView` now reports the typed query, so `#findPanel` stays in step.
        assert_eq!(find.query(), "Widget");
        assert_eq!(pane_body(Pane::Find, &v, &find).kind_tag(), "findPanel");
    }

    #[test]
    fn find_body_is_the_real_search_core_and_keeps_its_three_states() {
        let v = view();
        let mut find = FindView::new();

        // No query: the core's NoQuery state, whose notice is the typing prompt.
        let no_query = find_body(&v, &find);
        assert_eq!(no_query.state, "noquery");
        assert!(no_query.rows.is_empty());
        assert_eq!(no_query.meta, "");
        assert!(!no_query.notice.is_empty(), "NoQuery carries the typing prompt");

        // A query that hits: real rows in the core's order.
        find.set_query("Normalise");
        let hits = find_body(&v, &find);
        assert_eq!(hits.state, "hits");
        assert_eq!(hits.rows.len(), 1);
        assert_eq!(hits.rows[0].node_id, "B");
        assert_eq!(hits.rows[0].title, "Demo.Widget.NormaliseName");
        assert_eq!(hits.rows[0].kind, "method");
        assert_eq!(hits.rows[0].tag, "prefix");
        assert_eq!(hits.notice, "");

        // A query that does not: NoHits, whose notice differs from NoQuery.
        find.set_query("zzzznotasymbol");
        let no_hits = find_body(&v, &find);
        assert_eq!(no_hits.state, "nohits");
        assert!(no_hits.rows.is_empty());
        assert!(!no_hits.notice.is_empty());
        assert_ne!(no_hits.notice, no_query.notice);
    }

    #[test]
    fn blocks_body_is_an_honest_empty_stage_with_the_shipped_head() {
        let v = view();
        match pane_body(Pane::Blocks, &v, &FindView::new()) {
            PaneBody::Blocks(b) => {
                assert_eq!(b.title, "Blocks");
                assert_eq!(b.meta, "");
                assert_eq!(b.hint, "click a block to navigate");
                assert!(b.empty.contains("no block cards"));
            }
            _ => panic!("blocks mapped to the wrong body"),
        }
        // Selecting a node does not fabricate block rows — this build has no
        // block model, so the body stays the same honest empty stage.
        let mut v = view();
        v.selected = Some(0);
        match pane_body(Pane::Blocks, &v, &FindView::new()) {
            PaneBody::Blocks(b) => assert!(b.empty.contains("no block cards")),
            _ => panic!("blocks mapped to the wrong body"),
        }
    }
}

#[cfg(test)]
impl PaneBody<'_> {
    /// Test-only tag used to prove the mapping is total and stable.
    fn kind_tag(&self) -> &'static str {
        match self {
            PaneBody::Source => "source",
            PaneBody::Node(_) => "nodeDrawer",
            PaneBody::Find(_) => "findPanel",
            PaneBody::Blocks(_) => "blocks",
        }
    }
}
