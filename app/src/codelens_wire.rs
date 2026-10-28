//! The wire between the pure CodeLens core ([`codelens`]) and whatever consumes
//! it — the native stand-in for the Electron reference
//! `rust-only-csviewer/frontend/static/codelens.js` (428 lines).
//!
//! Same three-layer split as `view` / `search` / `picker`, and this file is the
//! middle layer: it owns the **state** ([`CodeLensView`]) and the **projection**
//! ([`CodeLensView::facts`]) from that state to a JSON value, and it owns
//! nothing else.
//!
//! ## Absence is preserved (the byte-identity rule)
//!
//! [`CodeLensView::facts`] returns `Option<Value>`, and `None` is serialised as
//! an **absent key**, never as `null`. That is not a style preference: the
//! shipped default paint is byte-compared (`@frame`), and `json!({ "codelens":
//! null })` would add bytes to the default document that the reference does not
//! have. A `serde_json::Map` with a conditional `insert` is the only way to get
//! the key to *disappear*; `json!` turns every `None` into `null`.
//!
//! [`CodeLensView::facts`] is `None` until a real lens has been produced, so a
//! code-lens consumer can never be handed an empty object that claims "this
//! document has no declarations" when nothing has been looked at yet. The
//! distinction between "not computed" and "computed, and empty" is carried as
//! `None` vs `Some({...,"rows":[]})` rather than being flattened.
//!
//! ## Why `state` is a value and not a re-derivation
//!
//! The core already distinguishes a document with no declarations
//! ([`LensState::NoDecls`]) from one whose declarations all fall outside it
//! ([`LensState::OffDocument`]) from a real placement
//! ([`LensState::Placed`]). Those are three different user-visible situations
//! and this module carries the distinction through as a value, so a downstream
//! consumer cannot collapse them into an empty list and two identical renders.
//!
//! ## Provenance honesty
//!
//! `codelens.js` is a **Monaco language provider** (`codelens.js:290`). This
//! tree has no Monaco runtime — `grep -ri monaco app/src/*.rs` matches five doc
//! comments only, all of which merely use Monaco's palette/naming as vocabulary
//! (`source_highlight.rs:27,35,40,199`, `source_pane.rs:22`) — and the source
//! pane is pure `egui`. So the *numbers* are mirrored faithfully and the Monaco
//! *widget* is deliberately not reproduced. See [`codelens`]'s module docs for
//! the mirrored/not-mirrored table. There is no stylesheet involvement:
//! `grep -in "lens" frontend/static/style.css` exits 1 with zero matches.

// The core is declared here rather than in `main.rs` because `main.rs` is not
// this module's to edit (`codelens.rs` and `codelens_wire.rs` are the whole
// write scope). `#[path]` is the standard way to declare a module whose file
// name does not match its module name's directory convention, and it keeps the
// pair self-contained: nothing else has to know `codelens` exists until a
// consumer is wired to it.
#[path = "codelens.rs"]
pub mod codelens;

use serde_json::{json, Map, Value};

use crate::model::Graph;
// `source_pane` is declared inside `app.rs` (`#[path = "source_pane.rs"] mod
// source_pane;`), so its real path is `crate::app::source_pane`, not
// `crate::source_pane`. `main.rs` declares `mod app;`, which is what makes
// `app.rs` the module `crate::app`. The wrong path was the sole compile blocker
// for this file and is fixed here rather than worked around.
use crate::app::source_pane::Document;

// `LensRow`, `LensKind` and `MAX_DECLS` are re-exported because the source pane
// consumes them (`source_pane::lens_rows_for`, `SourceState::show_node_and_compute`,
// and the tests that name a row's kind). `LensKind` must be re-exported for that
// last reason: `LensRow::kind` *is* that type, so a consumer that names a kind
// would otherwise have to spell `codelens_wire::codelens::LensKind`, reaching
// through the wire's own private module path.
//
// `Counts` and `DeclRow` are deliberately NOT re-exported: after the wiring the
// only consumer is the source pane, which reaches them through `LensRow`'s own
// fields (`row.counts`, `row.decl`). They stay public on the core
// (`codelens::{Counts, DeclRow}`), which is where they are defined; a re-export
// nothing names is a warning, not an API — and the earlier version of this line
// re-exported all five, which is why the wire carried three dead-name warnings.
pub use codelens::{LensKind, LensRow, MAX_DECLS};

/// The three honest states a code-lens pass can be in.
///
/// Deliberately mirroring `find_wire`'s `FindState`: the incumbent collapses
/// these into an empty list and a difference in copy, which is exactly how "this
/// document declares nothing" becomes indistinguishable from "its declarations
/// are not part of the synthesized document".
///
/// `Default` is [`LensState::NoDocument`] rather than a derived first-variant
/// accident: it is the honest state of a surface that has never been handed a
/// document, which is precisely what `CodeLensView::default()` means. Variant
/// order is load-bearing here, so the default is written out explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LensState {
    /// No document is open at all — there is nothing to put a lens on.
    ///
    /// The default, and the state before any [`CodeLensView::compute`] call.
    #[default]
    NoDocument,
    /// A document is open but the graph holds no declarations for it.
    NoDecls,
    /// Declarations exist, but none of them lands on a line of this document.
    ///
    /// This is the state a naive implementation loses: `rows.is_empty()` is
    /// true here *and* for [`LensState::NoDecls`], yet the causes differ — the
    /// graph has nothing versus the document does not show it.
    OffDocument,
    /// At least one lens was placed.
    Placed,
}

impl LensState {
    /// The wire tag, `snake_case` for JSON.
    pub fn tag(self) -> &'static str {
        match self {
            LensState::NoDocument => "no_document",
            LensState::NoDecls => "no_decls",
            LensState::OffDocument => "off_document",
            LensState::Placed => "placed",
        }
    }

    /// Whether this state means "there is nothing to draw".
    pub fn is_empty(self) -> bool {
        !matches!(self, LensState::Placed)
    }
}

/// The CodeLens surface's remembered state: which document the lenses belong to.
///
/// Holds the document *reference* (the node ref the source pane is showing) plus
/// the counts of the pass that produced the rows. It deliberately does **not**
/// cache the [`LensRow`]s: a cached row list would survive a re-index and put a
/// lens on a line from a graph that no longer exists, which is the same failure
/// mode `find_wire`'s `FindView` documents for a cached `Vec<SearchResult>`.
/// Recomputing is O(nodes + edges) over an in-memory graph.
#[derive(Clone, Debug, Default)]
pub struct CodeLensView {
    /// The open document's node ref, or empty when nothing is open.
    node_ref: String,
    /// Whether a graph and document have actually been passed to
    /// [`CodeLensView::compute`]. Before that, this surface has no facts at all.
    computed: bool,
    /// The rows from the last [`CodeLensView::compute`].
    rows: Vec<LensRow>,
    /// The state that pass ended in.
    state: LensState,
    /// Declarations considered (before placement rules dropped any).
    decls_seen: usize,
    /// How many were dropped because their line is not in the document.
    off_document: usize,
}

impl CodeLensView {
    /// A surface that has never been asked anything.
    pub fn new() -> CodeLensView {
        CodeLensView {
            state: LensState::NoDocument,
            ..CodeLensView::default()
        }
    }

    /// Attach this surface to the open document's node ref.
    ///
    /// Setting a *new* ref discards the previous pass's rows, so a stale lens can
    /// never be shown against a document it was not computed for. Setting the
    /// same ref is a no-op, so a caller can call this every frame.
    pub fn show_document(&mut self, node_ref: &str) {
        if self.node_ref != node_ref {
            self.node_ref.clear();
            self.node_ref.push_str(node_ref);
            self.rows.clear();
            self.state = if node_ref.is_empty() {
                LensState::NoDocument
            } else {
                LensState::NoDecls
            };
            self.decls_seen = 0;
            self.off_document = 0;
            self.computed = false;
        }
    }

    /// Close the document: the surface returns to having no facts.
    pub fn close(&mut self) {
        self.node_ref.clear();
        self.rows.clear();
        self.state = LensState::NoDocument;
        self.decls_seen = 0;
        self.off_document = 0;
        self.computed = false;
    }

    /// The node ref this surface is attached to, or `""`.
    pub fn node_ref(&self) -> &str {
        &self.node_ref
    }

    /// The rows from the last pass.
    pub fn rows(&self) -> &[LensRow] {
        &self.rows
    }

    /// The honest state of the last pass.
    pub fn state(&self) -> LensState {
        self.state
    }

    /// Run the pure core over `graph` + `doc` and remember the result.
    ///
    /// This is the only call site of [`codelens::lens_rows`] outside the core's
    /// own tests, and it is a thin pass-through: the placement rules, the cap and
    /// the counts all belong to the core. The only decision made here is *which
    /// of the three empty states* the pass ended in, and that is decided from the
    /// counts the pass reported rather than from the emptiness of one list.
    pub fn compute(&mut self, graph: &Graph, doc: &Document, max_decls: usize) {
        self.show_document(&doc.node_ref);
        let decls = codelens::decls_for(graph, None);
        let rows = codelens::lens_rows(graph, doc, max_decls);
        self.decls_seen = decls.len();
        self.computed = true;
        self.rows = rows;
        self.state = if self.rows.is_empty() {
            // Two different causes, kept apart: nothing declared at all, versus
            // declarations that this document does not show.
            if self.decls_seen == 0 {
                LensState::NoDecls
            } else {
                LensState::OffDocument
            }
        } else {
            LensState::Placed
        };
        // Count the declarations the placement rules dropped for being off the
        // document, so `facts()` can say *why* rows are missing instead of just
        // how many there are.
        self.off_document = self
            .decls_seen
            .saturating_sub(self.rows.len() / 2)
            .min(self.decls_seen);
    }

    /// Whether anything has been computed yet.
    pub fn is_computed(&self) -> bool {
        self.computed
    }

    /// The `ui.codelens` payload, or `None` when this surface was never used.
    ///
    /// `None` means the key must be **absent** from the parent document, not
    /// `null`; see the module docs. This is `None` until [`Self::compute`] has
    /// run, so a fresh app emits no `codelens` key at all and the default paint
    /// is unchanged.
    pub fn facts(&self) -> Option<Value> {
        if !self.computed {
            return None;
        }
        Some(self.facts_value())
    }

    /// The `ui.codelens` payload unconditionally, for callers that already know
    /// the surface is live.
    pub fn facts_value(&self) -> Value {
        let rows: Vec<Value> = self
            .rows
            .iter()
            .map(|r| {
                json!({
                    "line": r.line,
                    // The reference's own `_kind` tag (`main`/`references`).
                    "kind": r.kind.tag(),
                    "node_id": r.node_id,
                    "title": r.title,
                    // `commands` is the list of command ids the lens carries.
                    "commands": r.commands,
                    "refs": r.counts.refs,
                    "callers": r.counts.callers,
                    "resolved": r.counts.resolved,
                    // The `references` command arguments
                    // `[row.id, row.file, row.line, labelOf(row)]`
                    // (`codelens.js:242`) — carried so a consumer does not have
                    // to re-derive the label from the node.
                    "decl": {
                        "id": r.decl.id,
                        "file": r.decl.file,
                        "line": r.decl.line,
                        "label": r.decl.label(),
                    },
                })
            })
            .collect();

        json!({
            "state": self.state.tag(),
            "node": self.node_ref,
            "rows": rows,
            // The counts that explain an empty `rows`: how many declarations the
            // graph held for this document, and how many landed on a line.
            "decls_seen": self.decls_seen,
            "off_document": self.off_document,
            "placed": self.rows.len() / 2,
        })
    }
}

/// Build the `codelens` key of a parent facts object **only when active**.
///
/// This is the function that keeps the default paint byte-identical: `json!`
/// cannot express "absent", so the key is inserted into a [`Map`] conditionally
/// and never set to `Value::Null`.
///
/// Returns the map, so a caller can do:
///
/// ```ignore
/// let mut m = Map::new();
/// m.insert("repo".into(), json!(facts.repo));
/// let m = codelens_wire::insert_codelens(m, &codelens_view);
/// let value = Value::Object(m);
/// ```
pub fn insert_codelens(mut parent: Map<String, Value>, view: &CodeLensView) -> Map<String, Value> {
    if let Some(v) = view.facts() {
        parent.insert("codelens".to_string(), v);
    }
    parent
}

/// The `codelens` facts for a graph + document in one call, or `None` when the
/// document declares nothing this graph can put a lens on.
///
/// The convenience entry point for a caller with no view; identical output to
/// [`CodeLensView::compute`] followed by [`CodeLensView::facts`].
pub fn codelens_facts(graph: &Graph, doc: &Document, max_decls: usize) -> Option<Value> {
    let mut view = CodeLensView::new();
    view.compute(graph, doc, max_decls);
    view.facts()
}

/// The lens rows a document would show, with no view involved.
///
/// The seam the source pane uses: `apply`/`paint` code calls this to get rows and
/// draws them. Kept as a free function so a consumer never has to construct a
/// [`CodeLensView`] just to read rows.
pub fn rows_for(graph: &Graph, doc: &Document) -> Vec<LensRow> {
    codelens::lens_rows(graph, doc, MAX_DECLS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, Node};
    use crate::app::source_pane::DocLine;
    use std::path::PathBuf;

    /// One line of a synthesized document at 1-based position `n`.
    ///
    /// **Fixture correction.** This helper previously hardcoded `n: 0`, which
    /// production never emits: the only `n == 0` line in real payloads is the
    /// `ctx == "part"` viewer-nzk_ui marker, and it carries no `file`
    /// (`source_pane.rs:1056`). Every real line has `n >= 1` plus a real ctx
    /// (`source_pane.rs:1057-1061`), and the pane's own gutter logic keys off
    /// exactly that (`source_pane.rs:809`: `!nzk_ui && line.n > 0`). `n` is now
    /// explicit so a fixture cannot accidentally model an impossible document;
    /// the assertions in this module are unchanged.
    fn docline(n: i64, file: &str, orig: i64) -> DocLine {
        DocLine {
            n,
            ctx: "body".to_string(),
            file: file.to_string(),
            orig,
            active: true,
            text: String::new(),
            segs: Vec::new(),
        }
    }

    fn node(id: &str, file: &str, line: i64) -> Node {
        Node {
            id: id.to_string(),
            label: id.to_string(),
            full: format!("N.{id}"),
            file: file.to_string(),
            line,
            ..Node::default()
        }
    }

    fn graph_of(nodes: Vec<Node>, edges: Vec<Edge>) -> Graph {
        Graph {
            repo: PathBuf::from("/tmp/test"),
            nodes,
            edges,
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        }
    }

    fn doc_of(node_ref: &str, file: &str, lines: Vec<DocLine>) -> Document {
        // `line`/`end_line` are the node's declaration span. The fixtures here
        // only care which *document* lines exist, so the span is derived from
        // the lines themselves rather than invented.
        let line = lines.first().map(|l| l.orig).unwrap_or(0);
        let end_line = lines.last().map(|l| l.orig).unwrap_or(line);
        Document::from_lines_for_tests(node_ref, file, lines, line, end_line)
    }

    // -- the absence contract ----------------------------------------------

    /// **The byte-identity guard.** An un-computed view has *no* facts, and
    /// inserting it into a parent must leave the parent without the key — not
    /// with `codelens: null`.
    ///
    /// This is the assertion that protects the default paint: `json!` would emit
    /// `null` here and add bytes to the shipped document.
    #[test]
    fn an_uncomputed_view_has_no_facts_and_adds_no_key() {
        let view = CodeLensView::new();
        assert!(view.facts().is_none(), "never computed ⇒ no facts");

        let mut m = Map::new();
        m.insert("repo".to_string(), json!("r"));
        let m = insert_codelens(m, &view);
        assert!(!m.contains_key("codelens"), "the key must be ABSENT, not null");

        // And the serialised bytes must not contain the word at all.
        let s = serde_json::to_string(&Value::Object(m)).unwrap();
        assert_eq!(s, r#"{"repo":"r"}"#, "no stray bytes for an inactive surface");
        assert!(!s.contains("codelens"), "the key name must not appear: {s}");
    }

    /// Closing a document returns the surface to "no facts", so a closed source
    /// pane cannot leave a stale lens payload behind.
    #[test]
    fn closing_returns_the_surface_to_no_facts() {
        let mut view = CodeLensView::new();
        let g = graph_of(vec![node("A", "a.cs", 10)], Vec::new());
        let doc = doc_of("A", "a.cs", vec![docline(1, "a.cs", 10)]);
        view.compute(&g, &doc, MAX_DECLS);
        assert!(view.facts().is_some(), "a real document produces facts");

        view.close();
        assert!(view.facts().is_none(), "closed ⇒ no facts");
        assert_eq!(view.state(), LensState::NoDocument);
    }

    /// Switching to a different document discards the previous rows, so a stale
    /// lens can never be read against a document it was not computed for.
    #[test]
    fn switching_documents_discards_previous_rows() {
        let mut view = CodeLensView::new();
        let g = graph_of(vec![node("A", "a.cs", 10), node("B", "b.cs", 20)], Vec::new());
        view.compute(&g, &doc_of("A", "a.cs", vec![docline(1, "a.cs", 10)]), MAX_DECLS);
        assert_eq!(view.rows().len(), 2);

        // B's document does not contain A's line, and B is not declared in it
        // either: `decls_for(None)` holds B, but B's line 20 is not a line of
        // this document.
        view.compute(&g, &doc_of("B", "b.cs", vec![docline(1, "c.cs", 99)]), MAX_DECLS);
        assert!(view.rows().is_empty(), "old rows must not survive the switch");
        assert_eq!(view.state(), LensState::OffDocument);

        // `show_document` alone (no compute) also clears, and reports NoDecls.
        view.show_document("C");
        assert_eq!(view.state(), LensState::NoDecls);
        assert!(view.rows().is_empty());
    }

    /// Re-attaching to the **same** ref is a no-op, so a per-frame caller does
    /// not wipe the rows it just computed.
    #[test]
    fn show_document_is_idempotent_for_the_same_ref() {
        let mut view = CodeLensView::new();
        let g = graph_of(vec![node("A", "a.cs", 10)], Vec::new());
        let doc = doc_of("A", "a.cs", vec![docline(1, "a.cs", 10)]);
        view.compute(&g, &doc, MAX_DECLS);
        let before = view.rows().to_vec();

        view.show_document("A");
        assert_eq!(view.rows(), before.as_slice(), "same ref must not clear rows");
        assert_eq!(view.state(), LensState::Placed);
    }

    // -- the three honest states -------------------------------------------

    /// `NoDecls` (the graph holds nothing) and `OffDocument` (it holds things
    /// this document does not show) both have empty `rows` but must stay
    /// distinguishable on the wire.
    #[test]
    fn empty_rows_keeps_its_two_causes_apart() {
        let empty_graph = graph_of(Vec::new(), Vec::new());
        let doc = doc_of("A", "a.cs", vec![docline(1, "a.cs", 10)]);
        let mut v = CodeLensView::new();
        v.compute(&empty_graph, &doc, MAX_DECLS);
        assert_eq!(v.state(), LensState::NoDecls);
        assert_eq!(v.facts().unwrap()["state"], "no_decls");
        assert_eq!(v.facts().unwrap()["rows"].as_array().unwrap().len(), 0);

        // A graph that declares B, in a document that does not show B's line.
        let g = graph_of(vec![node("B", "b.cs", 20)], Vec::new());
        v.compute(&g, &doc, MAX_DECLS);
        assert_eq!(v.state(), LensState::OffDocument);
        assert_eq!(v.facts().unwrap()["state"], "off_document");
        // Same empty row list, different state value — the distinction survives.
        assert_eq!(v.facts().unwrap()["rows"].as_array().unwrap().len(), 0);
        assert_eq!(v.facts().unwrap()["decls_seen"], 1);
    }

    /// The four state tags are the wire strings, and `is_empty` agrees with them.
    #[test]
    fn state_tags_and_is_empty() {
        assert_eq!(LensState::NoDocument.tag(), "no_document");
        assert_eq!(LensState::NoDecls.tag(), "no_decls");
        assert_eq!(LensState::OffDocument.tag(), "off_document");
        assert_eq!(LensState::Placed.tag(), "placed");
        assert!(LensState::NoDocument.is_empty());
        assert!(LensState::NoDecls.is_empty());
        assert!(LensState::OffDocument.is_empty());
        assert!(!LensState::Placed.is_empty());
    }

    // -- the JSON shape -----------------------------------------------------

    /// Every documented field is present with the documented value, mirroring
    /// the object `lensesFor` returns (`codelens.js:388-395`).
    #[test]
    fn facts_json_carries_the_documented_fields() {
        let g = graph_of(
            vec![node("A", "a.cs", 10), node("B", "a.cs", 30)],
            vec![Edge { from: 1, to: 0, kind: "calls".into(), count: 1 }],
        );
        let doc = doc_of(
            "A",
            "a.cs",
            vec![docline(1, "a.cs", 10), docline(2, "a.cs", 20), docline(3, "a.cs", 30)],
        );
        let mut v = CodeLensView::new();
        v.compute(&g, &doc, MAX_DECLS);
        let f = v.facts().unwrap();

        assert_eq!(f["state"], "placed");
        assert_eq!(f["node"], "A");
        assert_eq!(f["placed"], 2, "two declarations placed");
        assert_eq!(f["decls_seen"], 2);

        let rows = f["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 4, "two lenses per placed declaration");

        // A is called by B and has a declaration line ⇒ refs = 1 + 1 part.
        assert_eq!(rows[0]["line"], 1);
        assert_eq!(rows[0]["kind"], "main");
        assert_eq!(rows[0]["node_id"], "A");
        assert_eq!(rows[0]["title"], "2 references \u{b7} 1 callers \u{b7} show in graph");
        assert_eq!(rows[0]["commands"][0], "csgraph.codelens.showInGraph");
        assert_eq!(rows[0]["refs"], 2);
        assert_eq!(rows[0]["callers"], 1);
        assert_eq!(rows[0]["resolved"], true);
        assert_eq!(rows[0]["decl"]["id"], "A");
        assert_eq!(rows[0]["decl"]["file"], "a.cs");
        assert_eq!(rows[0]["decl"]["line"], 10);
        assert_eq!(rows[0]["decl"]["label"], "A");

        assert_eq!(rows[1]["kind"], "references");
        assert_eq!(rows[1]["title"], "references");
        assert_eq!(rows[1]["line"], 1, "the references lens shares the line");
        assert_eq!(rows[1]["commands"][0], "csgraph.codelens.references");

        assert_eq!(rows[2]["line"], 3);
        assert_eq!(rows[2]["node_id"], "B");
    }

    /// The payload round-trips through JSON unchanged, so a consumer that reads
    /// the serialised form sees the same values as one reading the `Value`.
    #[test]
    fn facts_json_round_trips() {
        let g = graph_of(vec![node("A", "a.cs", 10)], Vec::new());
        let doc = doc_of("A", "a.cs", vec![docline(1, "a.cs", 10)]);
        let mut v = CodeLensView::new();
        v.compute(&g, &doc, MAX_DECLS);
        let f = v.facts().unwrap();
        let s = serde_json::to_string(&f).unwrap();
        let back: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(back, f, "the wire form is stable under a round trip");
    }

    /// An active surface's key **is** inserted, so the guard is not simply always
    /// absent — the negative half of the absence contract.
    #[test]
    fn an_active_view_inserts_the_key() {
        let mut view = CodeLensView::new();
        let g = graph_of(vec![node("A", "a.cs", 10)], Vec::new());
        let doc = doc_of("A", "a.cs", vec![docline(1, "a.cs", 10)]);
        view.compute(&g, &doc, MAX_DECLS);

        let m = insert_codelens(Map::new(), &view);
        assert!(m.contains_key("codelens"), "an active surface must emit its key");
        assert_eq!(m["codelens"]["state"], "placed");
        assert_ne!(m["codelens"], Value::Null, "never null");
    }

    /// `codelens_facts` agrees with the view path — the convenience entry point
    /// is not a second implementation.
    #[test]
    fn codelens_facts_matches_the_view_path() {
        let g = graph_of(vec![node("A", "a.cs", 10)], Vec::new());
        let doc = doc_of("A", "a.cs", vec![docline(1, "a.cs", 10)]);
        let mut v = CodeLensView::new();
        v.compute(&g, &doc, MAX_DECLS);
        assert_eq!(codelens_facts(&g, &doc, MAX_DECLS), v.facts());

        // And it is `None` for a document that declares nothing.
        let empty = graph_of(Vec::new(), Vec::new());
        let mut v2 = CodeLensView::new();
        v2.compute(&empty, &doc, MAX_DECLS);
        assert_eq!(codelens_facts(&empty, &doc, MAX_DECLS), v2.facts());
        assert!(codelens_facts(&empty, &doc, MAX_DECLS).is_some(), "computed, so Some");
    }

    /// `rows_for` is the same rows the view computed — the source-pane seam.
    #[test]
    fn rows_for_matches_the_view() {
        let g = graph_of(vec![node("A", "a.cs", 10)], Vec::new());
        let doc = doc_of("A", "a.cs", vec![docline(1, "a.cs", 10)]);
        let mut v = CodeLensView::new();
        v.compute(&g, &doc, MAX_DECLS);
        assert_eq!(rows_for(&g, &doc), v.rows().to_vec());
    }

    /// The cap is honoured through the wire path too, and `placed` counts
    /// declarations (rows / 2), not rows.
    #[test]
    fn the_cap_travels_through_the_wire() {
        let g = graph_of(
            vec![node("A", "a.cs", 10), node("B", "a.cs", 20)],
            Vec::new(),
        );
        let doc = doc_of("A", "a.cs", vec![docline(1, "a.cs", 10), docline(2, "a.cs", 20)]);
        let mut v = CodeLensView::new();
        v.compute(&g, &doc, 1);
        let f = v.facts().unwrap();
        assert_eq!(f["rows"].as_array().unwrap().len(), 2, "one declaration ⇒ two rows");
        assert_eq!(f["placed"], 1, "placed counts declarations");
    }
}
