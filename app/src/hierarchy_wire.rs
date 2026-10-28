//! The wire half of the hierarchy subsystem: view state, and the JSON the DOM
//! layer deserializes.
//!
//! Mirrors the state and render-naming half of
//! `rust-only-csviewer/frontend/static/hierarchy.js`:
//!
//! * `state` (`hierarchy.js:56-64`) — `mode` (`call`|`type`), `nodeId`, `word`,
//!   `dir` (`both`|`forward`|`backward`), `depth` (1..4, default 2), `collapse`,
//!   `generation`;
//! * `show()` (`hierarchy.js:457-473`) titles the panel and clears `collapse`;
//! * `renderCall`/`renderType` (529-546, 597-...) turn the loaded data into the
//!   section list the DOM layer paints.
//!
//! # The absent-key rule
//!
//! [`HierarchyView::facts`] returns `Option<Value>`. When the panel is closed it
//! returns **`None`**, and the caller must not insert the key at all — a JSON
//! `null` would be a *different* document from the one `index.html` ships, and
//! the default paint has to stay byte-identical. `serde_json`'s `json!` turns a
//! `None` into `null`, so the key is inserted conditionally by the caller rather
//! than built inline. This is the same contract `find_wire.rs` documents.
//!
//! # Relationship to the core
//!
//! All graph reasoning lives in [`crate::hierarchy`]. This module only holds
//! *view* state and projects already-computed trees into JSON; it performs no
//! traversal of its own, so the two never disagree about what a hierarchy is.

use serde_json::{json, Value};

// The core. Declared here with `#[path]` rather than in `main.rs`, which is
// owned by the windows/threading task and deliberately frozen: every module
// added since has been declared this way (see `app.rs:38-60` for nzk_ui,
// nzk_ui_client, buildinfo, source_highlight, source_pane). `hierarchy_wire.rs`
// is reached from `app.rs`, so declaring the core here keeps `main.rs`
// untouched.
#[path = "hierarchy.rs"]
pub mod core_impl;

pub use core_impl::{
    bases_label, chain_base_children, clamp_depth, direction_label, edge_label, expand_dirs,
    flatten_tree, hop_count, hop_label, is_type_kind, is_valid_dir, label_of, norm_chain,
    relations_from_groups, same_member_kind, truncated_label, Hop, Relations, CHAIN_MAX,
    MAX_DERIVED_DEPTH, TYPE_CHAIN_DEPTH,
};

/// Which hierarchy the panel is showing (`hierarchy.js:57`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Call,
    Type,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Call => "call",
            Mode::Type => "type",
        }
    }

    /// `state.mode = mode` where `mode` is `'call'` or `'type'`
    /// (`hierarchy.js:461`); anything else is not a mode.
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "call" => Some(Mode::Call),
            "type" => Some(Mode::Type),
            _ => None,
        }
    }
}

/// One rendered section of the panel body — the Rust shape of the elements
/// `renderCall` appends (`hierarchy.js:535-543`).
#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    /// The uppercase section label (`section()` at `hierarchy.js:523-525`).
    pub label: String,
    /// The hint after a `·`, or `""` for none — e.g. `"3 hops"`, `"2 bases"`.
    pub hint: String,
    pub tree: Option<Hop>,
    /// The reference prints its own "truncated at N hops" row.
    pub truncated: bool,
}

/// The panel's view state.
#[derive(Clone, Debug, Default)]
pub struct HierarchyView {
    /// `false` = the panel is closed and the document must be left alone.
    open: bool,
    mode: Mode,
    /// The resolved symbol id, or `""` before resolution (`hierarchy.js:58`).
    node_id: String,
    /// The word under the cursor (`hierarchy.js:59`).
    word: String,
    /// `both` | `forward` | `backward`.
    dir: String,
    depth: u32,
    /// Which rows the user collapsed, keyed by the row's path
    /// (`hierarchy.js:487,503`).
    collapse: Vec<String>,
    /// Bumped on every load so a stale async result can be discarded
    /// (`hierarchy.js:549,562`).
    generation: u64,
    /// The sub-label shown next to the title (`hierarchy.js:466-467`).
    subtitle: String,
    sections: Vec<Section>,
    /// The body's "nothing here" line, or `""`.
    empty: String,
}

impl HierarchyView {
    pub fn new() -> Self {
        Self {
            dir: "both".to_string(),
            depth: 2,
            ..Default::default()
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn dir(&self) -> &str {
        &self.dir
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn word(&self) -> &str {
        &self.word
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn subtitle(&self) -> &str {
        &self.subtitle
    }

    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    pub fn empty(&self) -> &str {
        &self.empty
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn collapse(&self) -> &[String] {
        &self.collapse
    }

    /// `show(mode, titleText, subText)` (`hierarchy.js:457-473`).
    ///
    /// Opens the panel, sets the mode and subtitle, and **clears `collapse`** —
    /// the reference resets the collapse map on every show, so a reopened panel
    /// is fully expanded. The mode is not validated here: `show` is called with
    /// a literal in the reference.
    pub fn show(&mut self, mode: Mode, word: &str, subtitle: &str) {
        self.open = true;
        self.mode = mode;
        self.word = word.to_string();
        self.subtitle = subtitle.to_string();
        self.collapse.clear();
        self.sections.clear();
        self.empty.clear();
    }

    /// `hide()` (`hierarchy.js:453-455`) — the panel is `display: none`, which
    /// for this layer means the document is left as it ships.
    pub fn hide(&mut self) {
        self.open = false;
    }

    /// The `dirSelect` change handler (`hierarchy.js:428-431`). Returns whether
    /// the selection was accepted; an invalid direction leaves state untouched,
    /// which is what lets a caller reload conditionally.
    pub fn set_dir(&mut self, dir: &str) -> bool {
        if !is_valid_dir(dir) {
            return false;
        }
        self.dir = dir.to_string();
        true
    }

    /// The `depthSelect` change handler (`hierarchy.js:438-441`):
    /// `parseInt(value, 10) || 2`, then the reference leaves it unbounded while
    /// the select only offers 1..4 — [`clamp_depth`] applies the offered range.
    pub fn set_depth(&mut self, depth: u32) {
        self.depth = clamp_depth(if depth == 0 { 2 } else { depth });
    }

    /// Begin a load: `var gen = ++state.generation` (`hierarchy.js:549`).
    pub fn begin_load(&mut self, node_id: &str) -> u64 {
        self.generation += 1;
        self.node_id = node_id.to_string();
        self.sections.clear();
        self.empty.clear();
        self.generation
    }

    /// Discard a stale result — `if (gen !== state.generation) return null`
    /// (`hierarchy.js:562`).
    pub fn is_current(&self, generation: u64) -> bool {
        generation == self.generation
    }

    /// Accept a loaded call tree, if it is still current.
    pub fn set_call_sections(&mut self, generation: u64, sections: Vec<Section>) {
        if !self.is_current(generation) {
            return;
        }
        let any = sections.iter().any(|s| s.tree.is_some());
        self.empty = if any {
            String::new()
        } else {
            core_impl::empty::NOTHING_LINKED.to_string()
        };
        self.sections = sections;
    }

    /// Accept a loaded type tree, if it is still current.
    pub fn set_type_sections(&mut self, generation: u64, sections: Vec<Section>) {
        if !self.is_current(generation) {
            return;
        }
        self.empty.clear();
        self.sections = sections;
    }

    /// The body's placeholder line (`hierarchy.js:533`).
    pub fn set_no_hierarchy(&mut self) {
        self.sections.clear();
        self.empty = core_impl::empty::NO_HIERARCHY.to_string();
    }

    /// `dblclick` on a row toggles its collapse (`hierarchy.js:501-505`).
    pub fn toggle_collapse(&mut self, path: &str) {
        if let Some(i) = self.collapse.iter().position(|p| p == path) {
            self.collapse.remove(i);
        } else {
            self.collapse.push(path.to_string());
        }
    }

    pub fn is_collapsed(&self, path: &str) -> bool {
        self.collapse.iter().any(|p| p == path)
    }

    /// The JSON the DOM layer reads, or `None` when the panel is closed.
    ///
    /// **`None` means the key must be absent, not null.** See the module docs.
    pub fn facts(&self) -> Option<Value> {
        if !self.open {
            return None;
        }
        let sections: Vec<Value> = self
            .sections
            .iter()
            .map(|s| {
                json!({
                    "label": s.label,
                    "hint": s.hint,
                    "truncated": s.truncated,
                    // Absent tree = nothing to show for this direction; the
                    // DOM layer distinguishes that from an empty tree.
                    "tree": s.tree.as_ref().map(hop_to_json),
                })
            })
            .collect();
        Some(json!({
            "mode": self.mode.as_str(),
            "word": self.word,
            "nodeId": self.node_id,
            "subtitle": self.subtitle,
            "dir": self.dir,
            "depth": self.depth,
            "sections": sections,
            "empty": self.empty,
            "collapse": self.collapse,
        }))
    }

    /// The panel facts in **`nzk_ui::dom::HierarchyFacts` shape**, or `None`
    /// when the panel is closed.
    ///
    /// [`Self::facts`] emits the *reference's* runtime shapes (`word`, nested
    /// `tree`, no option lists). The DOM layer does not read those: it
    /// deserializes `HierarchyFacts`, whose fields are `title`/`sections:
    /// [{caption, rows:[HierRow]}]`/`dir_options`/`mode_options`. Feeding the
    /// old shape to it silently yields a default-valued struct — an empty,
    /// untitled panel with no controls — because every field is
    /// `#[serde(default)]`. This method is the adapter that makes the two
    /// agree; `facts` stays for the reference-shaped consumers/tests.
    ///
    /// The flattening mirrors `rowEl` exactly:
    ///
    /// * `collapsed` is `self.is_collapsed(path) && depth > 0` — the depth
    ///   guard is the reference's own (`rowEl`), so a root row never collapses;
    /// * the caret follows from `has_children`/`collapsed` in the renderer, so
    ///   only the two booleans are sent;
    /// * `kind` is the row's *edge* family, which is what `hop-wrap {kind}`
    ///   uses; `node_kind` is the node's own kind;
    /// * `label` applies `labelOf` (label, else name, else id, else `?`);
    /// * children are flattened in order, each at its own `depth`, and are
    ///   **omitted entirely when the row is collapsed** — the same `if (kids.length
    ///   && !collapsed)` guard, so a collapsed row's descendants never reach the
    ///   DOM.
    ///
    /// `caption` is `label` plus the reference's own `" · "` join with the hint
    /// (`section()` at `hierarchy.js:523-525` appends the hint itself, so the
    /// join belongs here, not in the renderer).
    ///
    /// A section whose tree is `None` is **skipped**, matching `renderCall`'s
    /// `if (!sec.tree || !sec.tree.root) return;`.
    /// `(rows, truncated)` for the walk the panel is rendering — the
    /// `(d.flat || []).length` and `d.truncated` pair `filters.js:555` reads for
    /// `#chainCount`.
    ///
    /// Read off the **live sections, not a stored copy**: it loops
    /// `self.sections` exactly as [`Self::dom_facts`] does and flattens through
    /// the same `flatten_tree`, so the head counter and the panel body can never
    /// describe two different walks. `None` while no section carries a tree
    /// (nothing has been walked), which is what makes the counter's key
    /// *absent* rather than a `0` nobody measured.
    ///
    /// The root is **included** and a walk that found nothing reports `0` — not
    /// "absent" — because the reference counts `flat` itself (`filters.js:555`),
    /// where a resolved node is always the first row. The body's `count = rows - 1`
    /// hint is a different number from the same array.
    pub fn chain_flat_rows(&self) -> Option<(usize, bool)> {
        self.sections
            .iter()
            .find_map(|s| s.tree.as_ref().map(|root| (flatten_tree(root).len(), s.truncated)))
    }

    pub fn dom_facts(&self) -> Option<Value> {
        if !self.open {
            return None;
        }

        let mut sections: Vec<Value> = Vec::new();
        for s in &self.sections {
            let Some(root) = s.tree.as_ref() else { continue };
            let mut rows: Vec<Value> = Vec::new();
            // The root row's path is the empty base, so a child's key reads
            // `/0:<id>` — exactly the reference's `path + '/' + i + ':' + id`
            // from its own root call (`rowEl(sec.tree.root, 0, sec.dir)`), and
            // exactly what `toggle_collapse` is given by the click handler.
            flatten_rows(root, 0, "", self, &mut rows);
            // `count = (sec.tree.rows || []).length - 1`, then the plural form.
            let count = rows.len().saturating_sub(1);
            let hint = if s.hint.is_empty() {
                format!("{} hop{}", count, if count == 1 { "" } else { "s" })
            } else {
                s.hint.clone()
            };
            let caption = format!("{} \u{00b7} {}", s.label, hint);
            if s.truncated {
                let n = self.depth as usize;
                rows.push(json!({
                    "kind": "",
                    "depth": 0,
                    "has_children": false,
                    "collapsed": false,
                    "edge": "",
                    "label": truncated_label(n),
                    "node_kind": "",
                    "cycle": "",
                }));
            }
            sections.push(json!({ "caption": caption, "rows": rows }));
        }

        // `renderCall` appends "Nothing linked in this direction." when it
        // rendered no rows at all; the view's own `empty` wins when set.
        let empty = if !self.empty.is_empty() {
            self.empty.clone()
        } else if self.mode == Mode::Call && sections.iter().all(|s| {
            s.get("rows").and_then(|r| r.as_array()).map(|r| r.is_empty()).unwrap_or(true)
        }) {
            core_impl::empty::NOTHING_LINKED.to_string()
        } else {
            String::new()
        };

        Some(json!({
            "mode": self.mode.as_str(),
            "dir": self.dir,
            "depth": self.depth,
            "title": self.title(),
            "subtitle": self.subtitle,
            "dir_options": dir_options(&self.dir),
            "mode_options": depth_options(self.depth),
            "sections": sections,
            "empty": empty,
        }))
    }

    /// Every `.hop` row's collapse path, in paint order — the exact keys
    /// [`Self::toggle_collapse`] expects for the rows [`Self::dom_facts`] emits.
    ///
    /// This is the click route's half of the panel. `flatten_rows` decides which
    /// paths become rows; this walks the *same* recursion with the *same* `""`
    /// base and the same `/i:id` child keys, so the Nth path here is the Nth
    /// `.hop` on screen by construction. A row that is collapsed contributes no
    /// descendants in either — which is what makes the Nth-band hit test in
    /// `nzk_ui_hierarchy_ui` land on the row the user actually sees.
    pub fn row_paths(&self) -> Vec<String> {
        let mut out = Vec::new();
        for s in &self.sections {
            let Some(root) = s.tree.as_ref() else { continue };
            collect_row_paths(root, 0, "", self, &mut out);
            if s.truncated {
                // `dom_facts` appends the "truncated at N hops" row, which is
                // not collapsible but IS a `.hop`, so it must occupy a band.
                out.push(String::new());
            }
        }
        out
    }

    /// `.cs-hier-title` text (`show()` at `hierarchy.js:457-473`).
    pub fn title(&self) -> String {
        match self.mode {
            Mode::Call => "Call hierarchy".to_string(),
            Mode::Type => "Type hierarchy".to_string(),
        }
    }
}

/// The reference's `dirSelect` options, in order, with `selected` set
/// (`hierarchy.js:469`).
fn dir_options(dir: &str) -> Vec<Value> {
    [("both", "Both"), ("forward", "Outgoing"), ("backward", "Incoming")]
        .iter()
        .map(|(v, l)| {
            json!({ "value": v, "label": l, "selected": *v == dir })
        })
        .collect()
}

/// The reference's `depthSelect` options: `1..=4`, labelled `"depth N"`, with
/// the *string* value compared against `String(state.depth)`
/// (`hierarchy.js:435-441`).
fn depth_options(depth: u32) -> Vec<Value> {
    (1..=4u32)
        .map(|d| {
            json!({
                "value": d.to_string(),
                "label": format!("depth {}", d),
                "selected": d == depth,
            })
        })
        .collect()
}

/// Flatten one [`Hop`] subtree into `HierRow` JSON, in `rowEl`'s order.
///
/// `path` accumulates the same `/i:id` key the reference uses for its collapse
/// map, so a row's `collapsed` flag is looked up with the identical key.
fn flatten_rows(
    h: &Hop,
    depth: u32,
    path: &str,
    view: &HierarchyView,
    out: &mut Vec<Value>,
) {
    let collapsed = view.is_collapsed(path) && depth > 0;
    let node_kind = if h.kind.is_empty() { String::new() } else { h.kind.clone() };
    let label = if !h.label.is_empty() {
        h.label.clone()
    } else if !h.name.is_empty() {
        h.name.clone()
    } else if !h.id.is_empty() {
        h.id.clone()
    } else {
        "?".to_string()
    };
    out.push(json!({
        "kind": h.edge.clone().unwrap_or_default(),
        "depth": depth,
        "has_children": !h.children.is_empty(),
        "collapsed": collapsed,
        "edge": h.edge.clone().unwrap_or_default(),
        "label": label,
        "node_kind": node_kind,
        "cycle": if h.cycle { "cycle" } else { "" },
    }));
    if !h.children.is_empty() && !collapsed {
        for (i, kid) in h.children.iter().enumerate() {
            let child_path = format!("{}/{}:{}", path, i, kid.id);
            flatten_rows(kid, depth + 1, &child_path, view, out);
        }
    }
}

/// The path half of [`flatten_rows`]: the same recursion, emitting only the keys.
///
/// Kept adjacent to `flatten_rows` on purpose — the two must agree on the base
/// (`""`), the child key (`/i:id`) and the collapsed-prunes-descendants rule, or
/// a click would toggle a different row than the one under the pointer.
fn collect_row_paths(
    h: &Hop,
    depth: u32,
    path: &str,
    view: &HierarchyView,
    out: &mut Vec<String>,
) {
    out.push(path.to_string());
    let collapsed = view.is_collapsed(path) && depth > 0;
    if !h.children.is_empty() && !collapsed {
        for (i, kid) in h.children.iter().enumerate() {
            let child_path = format!("{}/{}:{}", path, i, kid.id);
            collect_row_paths(kid, depth + 1, &child_path, view, out);
        }
    }
}

/// Project a [`Hop`] to the exact object shape `rowEl` reads
/// (`hierarchy.js:480-515`): `id/label/kind/file/line/edge/cycle/children`,
/// plus the `full` used for the row's `title` tooltip.
pub fn hop_to_json(h: &Hop) -> Value {
    json!({
        "id": h.id,
        "name": h.name,
        "label": h.label,
        "full": h.full,
        "kind": h.kind,
        "file": h.file,
        "line": h.line,
        "edge": h.edge,
        "cycle": h.cycle,
        "depth": h.depth,
        "children": h.children.iter().map(hop_to_json).collect::<Vec<_>>(),
    })
}

/// Build the call-tree sections for one direction (`loadCall`,
/// `hierarchy.js:556-559`): label from [`direction_label`], tree normalized at
/// depth 0, and the truncated flag carried through from the engine.
pub fn call_section(dir: &str, tree: Option<Value>, truncated: bool) -> Section {
    Section {
        label: direction_label(dir).to_string(),
        hint: String::new(),
        tree: norm_chain(tree.as_ref(), 0),
        truncated,
    }
}

/// Fill in each section's `hint` from its own row count — the step
/// `renderCall` performs at `hierarchy.js:538-539`, where the count is
/// `rows.length - 1` and the hint is `"N hops"`/`"1 hop"`.
pub fn with_hop_hints(mut sections: Vec<Section>) -> Vec<Section> {
    for s in &mut sections {
        if let Some(root) = &s.tree {
            let count = hop_count(flatten_tree(root).len());
            s.hint = hop_label(count);
        } else {
            s.hint = String::new();
        }
    }
    sections
}

/// Walk one direction from a node and normalise the result into a section
/// tree, exactly as `treeFor`/`loadCall` do (`hierarchy.js:548-567`).
///
/// This is the **producer's** traversal, kept here rather than in
/// [`crate::hierarchy`] for the same reason `norm_chain` is: the core reasons
/// about *shapes* (`Hop`, `Relations`), while this module is the half that
/// knows where the app's graph actually lives. The walk is the reference's own
/// `/api/chain` call reduced to the in-memory [`crate::model::Graph`]: the app
/// has no chain route of its own, so the edelist *is* the chain source.
///
/// Direction semantics match the engine's route (and `expand_dirs`):
///
/// * `forward`  — follow `from -> to` edges out of the node ("Outgoing calls");
/// * `backward` — follow `to -> from` edges into it ("Incoming callers").
///
/// `depth` is the reference's limit (`state.depth`, 1..4) and is clamped by
/// [`clamp_depth`] so a caller cannot ask for an unbounded walk. A node already
/// on the current path is marked `cycle` and **not** descended into — the
/// reference's own guard against a relation loop, which is what stops a
/// self-recursive method from producing an infinite tree.
pub fn walk_call_section(
    graph: &crate::model::Graph,
    root_id: &str,
    dir: &str,
    depth: u32,
) -> Section {
    let depth = clamp_depth(depth);
    let mut cut = false;
    let tree = walk_hop(graph, root_id, dir, 0, depth, &mut Vec::new(), &mut cut);
    Section {
        label: direction_label(dir).to_string(),
        hint: String::new(),
        tree,
        // `truncated` is the reference's "there was more to walk than the depth
        // allowed" (`hierarchy.js:540`, the `"truncated at N hops"` row). It is
        // latched during the traversal, because only the walk can know whether an
        // edge was actually dropped — inferring it from the root's depth is wrong
        // in both directions: a leaf at the limit is not truncated, and an empty
        // result is not either.
        truncated: cut,
    }
}

/// One hop of [`walk_call_section`]'s traversal.
///
/// `seen` is the path from the root to here (ids), so a back-edge is marked
/// rather than followed. `cut` is latched `true` the moment an edge is dropped
/// because the depth limit was reached — that is the only place that fact is
/// known, and it is what `Section::truncated` reports.
fn walk_hop(
    graph: &crate::model::Graph,
    id: &str,
    dir: &str,
    level: u32,
    max: u32,
    seen: &mut Vec<String>,
    cut: &mut bool,
) -> Option<Hop> {
    let node = graph.nodes.iter().find(|n| n.id == id)?;
    let hop = |n: &crate::model::Node, edge: Option<&str>, cycle: bool, children: Vec<Hop>| Hop {
        id: n.id.clone(),
        name: n.label.clone(),
        label: label_of_node(n),
        full: n.full.clone(),
        kind: n.kind.clone(),
        file: n.file.clone(),
        line: n.line,
        edge: edge.map(|e| e.to_string()),
        cycle,
        depth: level,
        children,
    };

    let mut children = Vec::new();
    if level < max {
        seen.push(id.to_string());
        // Deterministic order: edges as stored, each resolved once. The engine
        // sorts its own edge list, so this is stable across runs.
        for e in &graph.edges {
            let (next, edge_kind) = if dir == "backward" {
                if graph.nodes.get(e.to).map(|n| n.id.as_str()) != Some(id) {
                    continue;
                }
                (graph.nodes.get(e.from), e.kind.as_str())
            } else {
                if graph.nodes.get(e.from).map(|n| n.id.as_str()) != Some(id) {
                    continue;
                }
                (graph.nodes.get(e.to), e.kind.as_str())
            };
            let Some(next) = next else { continue };
            if next.id == id {
                continue;
            }
            let cycle = seen.iter().any(|s| s == &next.id);
            let sub = if cycle {
                // A cycle is reported, never entered (`hierarchy.js:487-499`).
                Vec::new()
            } else {
                walk_hop(graph, &next.id, dir, level + 1, max, seen, cut)
                    .map(|h| h.children)
                    .unwrap_or_default()
            };
            children.push(hop(next, Some(edge_kind), cycle, sub));
        }
        seen.pop();
    } else if graph.edges.iter().any(|e| {
        // At the limit with an edge still to follow: the walk WAS cut short, and
        // this is the only place that is knowable. `dir` picks the end the edge
        // leaves from, exactly as the loop above does.
        let (own, other) = if dir == "backward" { (e.to, e.from) } else { (e.from, e.to) };
        graph.nodes.get(own).map(|n| n.id.as_str()) == Some(id)
            && graph.nodes.get(other).map(|n| n.id.as_str()) != Some(id)
    }) {
        *cut = true;
    }
    Some(hop(node, None, false, children))
}

/// `labelOf` for the graph's own [`crate::model::Node`]: label, else the
/// node's display name, else the id — the same fallback chain the reference's
/// `labelOf(thing)` uses for a node object (`hierarchy.js:120-126`), so a
/// nameless node still gets a row rather than blank text.
pub fn label_of_node(n: &crate::model::Node) -> String {
    if !n.label.is_empty() {
        n.label.clone()
    } else if !n.full.is_empty() {
        n.full.clone()
    } else {
        n.id.clone()
    }
}

/// The type panel's sections (`renderType`, `hierarchy.js:597-...`): a "Type"
/// root row, then the base chain filtered to `inherits` hops, falling back to
/// the raw `inherits:out` ids, and the derived types when present.
pub fn type_sections(node: Option<&Value>, rel: &Relations, chain_root: Option<&Hop>) -> Vec<Section> {
    let mut out = Vec::new();

    let root = node.map(|n| Hop {
        id: n.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        name: n.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        label: label_of(n),
        full: n.get("full").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        kind: n.get("kind").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        file: n.get("file").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        line: n.get("line").and_then(|v| v.as_i64()).unwrap_or(0),
        edge: None,
        cycle: false,
        depth: 0,
        children: Vec::new(),
    });

    out.push(Section {
        label: "Type".to_string(),
        hint: String::new(),
        tree: root,
        truncated: false,
    });

    let chain_kids = chain_root.map(chain_base_children).unwrap_or_default();
    let base_section = if !chain_kids.is_empty() {
        Section {
            label: "Base chain".to_string(),
            hint: bases_label(chain_kids.len()),
            // Re-root so the DOM layer paints the bases under the section.
            tree: chain_root.cloned(),
            truncated: false,
        }
    } else if !rel.bases.is_empty() {
        Section {
            label: "Base chain".to_string(),
            hint: bases_label(rel.bases.len()),
            tree: Some(Hop {
                id: String::new(),
                name: String::new(),
                label: String::new(),
                full: String::new(),
                kind: String::new(),
                file: String::new(),
                line: 0,
                edge: None,
                cycle: false,
                depth: 0,
                children: rel
                    .bases
                    .iter()
                    .map(|id| Hop {
                        id: id.clone(),
                        label: id.clone(),
                        edge: Some("inherits".to_string()),
                        ..Hop::default_hop()
                    })
                    .collect(),
            }),
            truncated: false,
        }
    } else {
        Section {
            label: "Base chain".to_string(),
            hint: String::new(),
            tree: None,
            truncated: false,
        }
    };
    out.push(base_section);

    if !rel.derived.is_empty() {
        out.push(Section {
            label: "Derived types".to_string(),
            hint: format!("{} types", rel.derived.len()),
            tree: Some(Hop {
                id: String::new(),
                name: String::new(),
                label: String::new(),
                full: String::new(),
                kind: String::new(),
                file: String::new(),
                line: 0,
                edge: None,
                cycle: false,
                depth: 0,
                children: rel
                    .derived
                    .iter()
                    .map(|id| Hop {
                        id: id.clone(),
                        label: id.clone(),
                        edge: Some("inherits".to_string()),
                        ..Hop::default_hop()
                    })
                    .collect(),
            }),
            truncated: false,
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(id: &str, children: Vec<Hop>) -> Hop {
        Hop {
            id: id.to_string(),
            label: id.to_string(),
            children,
            ..Hop::default_hop()
        }
    }

    #[test]
    fn closed_panel_yields_no_facts_at_all() {
        // THE central contract: closed must be `None` (key absent), never a
        // JSON null, or the default document changes.
        let v = HierarchyView::new();
        assert!(!v.is_open());
        assert!(v.facts().is_none());
    }

    #[test]
    fn closed_panel_yields_no_dom_facts_either() {
        // The DOM producer keeps the same absent-key rule, so a closed panel
        // can never perturb the shipped document (and its frame token).
        let v = HierarchyView::new();
        assert!(v.dom_facts().is_none());
    }

    /// The root cause this producer exists for: `facts()` speaks the
    /// *reference's* shape (`word`, nested `tree`, no option lists) while the
    /// DOM reads `HierarchyFacts` (`title`, flat `sections[{caption,rows}]`,
    /// option lists). This asserts the DOM-facing names are all present, so a
    /// silent `serde(default)` fallback can never masquerade as success.
    #[test]
    fn dom_facts_uses_the_dom_field_names_not_the_reference_ones() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Foo", "N.Foo");
        let f = v.dom_facts().expect("open panel produces DOM facts");

        // dom::HierarchyFacts.title — NOT the reference's "word".
        assert_eq!(f["title"], "Call hierarchy");
        assert!(f.get("word").is_none(), "`word` is not a DOM field");
        assert_eq!(f["mode"], "call");
        assert_eq!(f["dir"], "both");
        assert_eq!(f["depth"], 2);
        assert!(f["subtitle"].is_string());
        assert!(f["sections"].is_array());
        assert!(f["dir_options"].is_array());
        assert!(f["mode_options"].is_array());
    }

    #[test]
    fn type_mode_titles_itself_type_hierarchy() {
        let mut v = HierarchyView::new();
        v.show(Mode::Type, "Bar", "N.Bar");
        assert_eq!(v.dom_facts().unwrap()["title"], "Type hierarchy");
    }

    /// `hierarchy.js:469` — the direction select's three options, in order,
    /// with exactly the current direction marked selected.
    #[test]
    fn dir_options_are_both_outgoing_incoming_and_mark_the_selection() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Foo", "N.Foo");
        let f = v.dom_facts().unwrap();
        let opts = f["dir_options"].as_array().unwrap();
        let got: Vec<(&str, &str, bool)> = opts
            .iter()
            .map(|o| {
                (
                    o["value"].as_str().unwrap(),
                    o["label"].as_str().unwrap(),
                    o["selected"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("both", "Both", true),
                ("forward", "Outgoing", false),
                ("backward", "Incoming", false),
            ]
        );

        v.set_dir("backward");
        let f = v.dom_facts().unwrap();
        let opts = f["dir_options"].as_array().unwrap();
        assert!(!opts[0]["selected"].as_bool().unwrap());
        assert!(opts[2]["selected"].as_bool().unwrap());
    }

    /// The bug this fixes: the DOM's *second* select is the depth select, and
    /// it reads `mode_options`. So `mode_options` must carry 1..=4 labelled
    /// `"depth N"` — not modes — and exactly one must be selected.
    #[test]
    fn the_second_select_carries_depth_1_to_4_not_modes() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Foo", "N.Foo");
        let f = v.dom_facts().unwrap();
        let opts = f["mode_options"].as_array().unwrap();
        let got: Vec<(&str, &str, bool)> = opts
            .iter()
            .map(|o| {
                (
                    o["value"].as_str().unwrap(),
                    o["label"].as_str().unwrap(),
                    o["selected"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("1", "depth 1", false),
                ("2", "depth 2", true),
                ("3", "depth 3", false),
                ("4", "depth 4", false),
            ],
            "select#2 must offer depths 1..4 with the state's depth selected"
        );

        v.set_depth(4);
        let f = v.dom_facts().unwrap();
        let opts = f["mode_options"].as_array().unwrap();
        assert!(opts[3]["selected"].as_bool().unwrap());
        assert_eq!(opts[3]["value"], "4");
    }

    /// `rowEl`'s flattening: depth, `has_children`, edge/label/node_kind/cycle
    /// spans, and children omitted under a collapsed row.
    #[test]
    fn rows_flatten_with_depth_children_and_span_fields() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Root", "N.Root");
        let g = v.begin_load("root");
        let mut root = hop("Root", vec![hop("Kid", vec![])]);
        root.kind = "method".to_string();
        root.edge = None;
        root.children[0].edge = Some("calls".to_string());
        root.children[0].kind = "method".to_string();
        root.children[0].cycle = true;
        v.set_call_sections(
            g,
            vec![Section {
                label: "Outgoing calls".to_string(),
                hint: String::new(),
                tree: Some(root),
                truncated: false,
            }],
        );

        let f = v.dom_facts().unwrap();
        let sec = &f["sections"][0];
        // caption = label + the reference's own " · " join with the hint.
        assert_eq!(sec["caption"], "Outgoing calls \u{00b7} 1 hop");
        let rows = sec["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "root plus one child");

        assert_eq!(rows[0]["depth"], 0);
        assert_eq!(rows[0]["label"], "Root");
        assert_eq!(rows[0]["node_kind"], "method");
        assert!(rows[0]["has_children"].as_bool().unwrap());
        assert!(!rows[0]["collapsed"].as_bool().unwrap());
        assert_eq!(rows[0]["edge"], "", "the root has no incoming edge");

        assert_eq!(rows[1]["depth"], 1);
        assert_eq!(rows[1]["label"], "Kid");
        assert_eq!(rows[1]["edge"], "calls", ".hop-edge text");
        assert_eq!(rows[1]["kind"], "calls", "hop-wrap kind is the edge family");
        assert_eq!(rows[1]["cycle"], "cycle", ".hop-cycle chip text");
        assert!(!rows[1]["has_children"].as_bool().unwrap());
    }

    /// A collapsed row hides its descendants entirely (`if (kids.length &&
    /// !collapsed)`), and the caret's collapsed flag rides along.
    #[test]
    fn a_collapsed_row_drops_its_descendants_from_the_dom_rows() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Root", "N.Root");
        let g = v.begin_load("root");
        let root = hop("Root", vec![hop("Kid", vec![hop("Grandkid", vec![])])]);
        v.set_call_sections(
            g,
            vec![Section {
                label: "Outgoing calls".to_string(),
                hint: String::new(),
                tree: Some(root),
                truncated: false,
            }],
        );
        assert_eq!(v.dom_facts().unwrap()["sections"][0]["rows"].as_array().unwrap().len(), 3);

        // The child's own path key is "/0:Kid"; the root is "/".
        v.toggle_collapse("/0:Kid");
        let f = v.dom_facts().unwrap();
        let rows = f["sections"][0]["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "the grandchild is gone from the flat rows");
        assert!(rows[1]["collapsed"].as_bool().unwrap(), "caret reads collapsed");
        assert!(rows[1]["has_children"].as_bool().unwrap());
    }

    /// The reference never collapses the *root* (`depth > 0` guard), even if a
    /// stale path key says so.
    #[test]
    fn the_root_row_never_reports_itself_collapsed() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Root", "N.Root");
        let g = v.begin_load("root");
        v.set_call_sections(
            g,
            vec![Section {
                label: "Outgoing calls".to_string(),
                hint: String::new(),
                tree: Some(hop("Root", vec![hop("Kid", vec![])])),
                truncated: false,
            }],
        );
        v.toggle_collapse("/");
        let f = v.dom_facts().unwrap();
        let rows = f["sections"][0]["rows"].as_array().unwrap();
        assert!(!rows[0]["collapsed"].as_bool().unwrap());
        assert_eq!(rows.len(), 2, "root stays expanded");
    }

    /// `labelOf`: label, else name, else id, else `?` (`hierarchy.js:90-93`).
    #[test]
    fn a_row_with_no_label_falls_back_to_name_then_id_then_question_mark() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Root", "N.Root");
        let g = v.begin_load("root");
        let mut root = hop("", vec![]);
        root.id = "root-id".to_string();
        root.name = "RootName".to_string();
        root.label.clear();
        let mut bare = hop("", vec![]);
        bare.id.clear();
        bare.name.clear();
        root.children.push(bare);
        v.set_call_sections(
            g,
            vec![Section {
                label: "Outgoing calls".to_string(),
                hint: String::new(),
                tree: Some(root),
                truncated: false,
            }],
        );
        let f = v.dom_facts().unwrap();
        let rows = f["sections"][0]["rows"].as_array().unwrap();
        assert_eq!(rows[0]["label"], "RootName", "name is the fallback");
        assert_eq!(rows[1]["label"], "?", "bare node lands on `?`");
    }

    /// A section with no tree is skipped, like `renderCall`'s early return.
    #[test]
    fn a_section_without_a_tree_is_omitted_entirely() {
        let mut v = HierarchyView::new();
        v.show(Mode::Type, "Bar", "N.Bar");
        let g = v.begin_load("bar");
        v.set_type_sections(
            g,
            vec![
                Section {
                    label: "Base types".to_string(),
                    hint: String::new(),
                    tree: None,
                    truncated: false,
                },
                Section {
                    label: "Derived types".to_string(),
                    hint: String::new(),
                    tree: Some(hop("Derived", vec![])),
                    truncated: false,
                },
            ],
        );
        let f = v.dom_facts().unwrap();
        let secs = f["sections"].as_array().unwrap();
        assert_eq!(secs.len(), 1, "the treeless section is dropped");
        assert!(secs[0]["caption"].as_str().unwrap().starts_with("Derived types"));
    }

    /// Both hops singular and plural, then the truncation row.
    #[test]
    fn captions_pluralise_hops_and_a_truncated_section_appends_its_row() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Root", "N.Root");
        v.set_depth(3);
        let g = v.begin_load("root");
        v.set_call_sections(
            g,
            vec![Section {
                label: "Outgoing calls".to_string(),
                hint: String::new(),
                tree: Some(hop("Root", vec![hop("A", vec![]), hop("B", vec![])])),
                truncated: true,
            }],
        );
        let f = v.dom_facts().unwrap();
        let sec = &f["sections"][0];
        assert_eq!(sec["caption"], "Outgoing calls \u{00b7} 2 hops");
        let rows = sec["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 4, "root + 2 children + the truncation row");
        assert_eq!(rows[3]["label"], "truncated at 3 hops");
    }

    /// A call hierarchy with nothing linked reports the reference's own copy
    /// rather than an empty body.
    #[test]
    fn an_empty_call_hierarchy_reports_nothing_linked() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Root", "N.Root");
        let g = v.begin_load("root");
        v.set_call_sections(g, vec![]);
        let f = v.dom_facts().unwrap();
        assert_eq!(f["empty"], "Nothing linked in this direction.");
    }

    /// The generation guard still governs the DOM-facing producer: a stale
    /// async result must never reach the panel.
    #[test]
    fn a_stale_generation_cannot_publish_dom_facts() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Root", "N.Root");
        let stale = v.begin_load("root");
        let fresh = v.begin_load("other");
        assert_ne!(stale, fresh);

        v.set_call_sections(
            stale,
            vec![Section {
                label: "Stale".to_string(),
                hint: String::new(),
                tree: Some(hop("StaleRow", vec![])),
                truncated: false,
            }],
        );
        let f = v.dom_facts().unwrap();
        assert!(
            f["sections"].as_array().unwrap().is_empty(),
            "the stale section must not be published"
        );

        v.set_call_sections(
            fresh,
            vec![Section {
                label: "Fresh".to_string(),
                hint: String::new(),
                tree: Some(hop("FreshRow", vec![])),
                truncated: false,
            }],
        );
        let f = v.dom_facts().unwrap();
        let secs = f["sections"].as_array().unwrap();
        assert_eq!(secs.len(), 1);
        assert!(secs[0]["caption"].as_str().unwrap().starts_with("Fresh"));
    }

    /// `hide()` returns the key to absent for the DOM producer too.
    #[test]
    fn hiding_the_panel_removes_the_dom_facts() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Foo", "N.Foo");
        assert!(v.dom_facts().is_some());
        v.hide();
        assert!(v.dom_facts().is_none());
    }

    /// The producer must round-trip through the *actual* condition the DOM
    /// applies: every row carries the seven fields `HierRow` deserializes, so
    /// no field silently defaults.
    #[test]
    fn every_emitted_row_carries_all_seven_hierrow_fields() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Root", "N.Root");
        let g = v.begin_load("root");
        let mut kid = hop("Kid", vec![]);
        kid.edge = Some("calls".to_string());
        kid.kind = "method".to_string();
        v.set_call_sections(
            g,
            vec![Section {
                label: "Outgoing calls".to_string(),
                hint: String::new(),
                tree: Some(hop("Root", vec![kid])),
                truncated: true,
            }],
        );
        let f = v.dom_facts().unwrap();
        for row in f["sections"][0]["rows"].as_array().unwrap() {
            for field in [
                "kind",
                "depth",
                "has_children",
                "collapsed",
                "edge",
                "label",
                "node_kind",
                "cycle",
            ] {
                assert!(
                    row.get(field).is_some(),
                    "row is missing `{field}`: {row}"
                );
            }
        }
    }

    #[test]
    fn closed_panel_still_absent_after_being_opened_then_hidden() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Foo", "N.Foo");
        assert!(v.facts().is_some());
        v.hide();
        assert!(v.facts().is_none(), "hide() must return to the absent key");
    }

    #[test]
    fn facts_is_an_object_not_null_when_open() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "Foo", "N.Foo");
        let f = v.facts().expect("open panel has facts");
        assert!(f.is_object(), "must be an object, got {f}");
        assert_eq!(f["mode"], "call");
        assert_eq!(f["word"], "Foo");
        assert_eq!(f["dir"], "both");
        assert_eq!(f["depth"], 2);
        assert!(f["sections"].is_array());
    }

    #[test]
    fn show_clears_collapse_like_the_reference() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "A", "");
        v.toggle_collapse("forward/0:x");
        assert!(v.is_collapsed("forward/0:x"));
        v.show(Mode::Call, "B", "");
        assert!(
            !v.is_collapsed("forward/0:x"),
            "show() resets the collapse map (hierarchy.js:471)"
        );
    }

    #[test]
    fn toggle_collapse_round_trips() {
        let mut v = HierarchyView::new();
        assert!(!v.is_collapsed("p"));
        v.toggle_collapse("p");
        assert!(v.is_collapsed("p"));
        v.toggle_collapse("p");
        assert!(!v.is_collapsed("p"));
    }

    #[test]
    fn default_dir_and_depth_match_state() {
        let v = HierarchyView::new();
        assert_eq!(v.dir(), "both", "hierarchy.js:60");
        assert_eq!(v.depth(), 2, "hierarchy.js:61");
    }

    #[test]
    fn set_dir_rejects_invalid_and_keeps_previous() {
        let mut v = HierarchyView::new();
        assert!(v.set_dir("forward"));
        assert_eq!(v.dir(), "forward");
        assert!(!v.set_dir("sideways"), "invalid rejected");
        assert_eq!(v.dir(), "forward", "state untouched on rejection");
    }

    #[test]
    fn set_depth_clamps_and_zero_falls_back_to_2() {
        let mut v = HierarchyView::new();
        v.set_depth(3);
        assert_eq!(v.depth(), 3);
        v.set_depth(9);
        assert_eq!(v.depth(), 4);
        v.set_depth(0);
        assert_eq!(v.depth(), 2, "parseInt(...)||2");
    }

    #[test]
    fn generation_discards_a_stale_load() {
        let mut v = HierarchyView::new();
        let g1 = v.begin_load("first");
        let g2 = v.begin_load("second");
        assert_ne!(g1, g2);
        assert!(!v.is_current(g1), "the first load is now stale");
        assert!(v.is_current(g2));

        // A stale result must not overwrite the live one.
        v.set_call_sections(g2, vec![call_section("forward", Some(serde_json::json!({
            "node": {"id": "live"}
        })), false)]);
        v.set_call_sections(g1, vec![call_section("forward", Some(serde_json::json!({
            "node": {"id": "stale"}
        })), false)]);
        assert_eq!(v.sections().len(), 1);
        assert_eq!(v.sections()[0].tree.as_ref().unwrap().id, "live");
        assert_eq!(v.node_id(), "second");
    }

    #[test]
    fn no_linked_direction_sets_the_reference_empty_string() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "A", "");
        let g = v.begin_load("a");
        // A direction that produced no tree at all.
        v.set_call_sections(g, vec![call_section("forward", None, false)]);
        assert_eq!(v.empty(), "Nothing linked in this direction.");
    }

    #[test]
    fn a_present_tree_clears_the_empty_line() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "A", "");
        let g = v.begin_load("a");
        v.set_call_sections(
            g,
            with_hop_hints(vec![call_section(
                "forward",
                Some(serde_json::json!({"node": {"id": "a"}, "children": [{"node": {"id": "b"}}]})),
                false,
            )]),
        );
        assert_eq!(v.empty(), "");
        assert_eq!(v.sections()[0].hint, "1 hop", "one child = one hop");
    }

    #[test]
    fn hop_hints_count_rows_minus_root() {
        let secs = with_hop_hints(vec![call_section(
            "forward",
            Some(serde_json::json!({
                "node": {"id": "r"},
                "children": [
                    {"node": {"id": "a"}, "children": [{"node": {"id": "a1"}}]},
                    {"node": {"id": "b"}}
                ]
            })),
            false,
        )]);
        assert_eq!(secs[0].hint, "3 hops");
        assert_eq!(secs[0].label, "Outgoing calls");
    }

    #[test]
    fn backward_section_uses_the_incoming_label() {
        let s = call_section("backward", None, false);
        assert_eq!(s.label, "Incoming callers");
    }

    #[test]
    fn truncated_flag_survives_into_facts() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "A", "");
        let g = v.begin_load("a");
        v.set_call_sections(
            g,
            vec![call_section(
                "forward",
                Some(serde_json::json!({"node": {"id": "a"}})),
                true,
            )],
        );
        let f = v.facts().unwrap();
        assert_eq!(f["sections"][0]["truncated"], true);
    }

    #[test]
    fn facts_carries_edge_cycle_and_line_for_rows() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "A", "");
        let g = v.begin_load("a");
        v.set_call_sections(
            g,
            vec![call_section(
                "forward",
                Some(serde_json::json!({
                    "node": {"id": "r", "kind": "method", "file": "r.cs", "line": 3, "full": "N.r"},
                    "children": [
                        {"node": {"id": "k", "kind": "method", "line": 9},
                         "edge": "calls", "cycle": true}
                    ]
                })),
                false,
            )],
        );
        let f = v.facts().unwrap();
        let kid = &f["sections"][0]["tree"]["children"][0];
        assert_eq!(kid["id"], "k");
        assert_eq!(kid["edge"], "calls");
        assert_eq!(kid["cycle"], true);
        assert_eq!(kid["line"], 9);
        assert_eq!(f["sections"][0]["tree"]["full"], "N.r", "row tooltip source");
    }

    #[test]
    fn hop_to_json_round_trips_nested_children() {
        let h = hop("r", vec![hop("a", vec![hop("a1", vec![])])]);
        let j = hop_to_json(&h);
        assert_eq!(j["id"], "r");
        assert_eq!(j["children"][0]["id"], "a");
        assert_eq!(j["children"][0]["children"][0]["id"], "a1");
        assert_eq!(j["children"][0]["children"][0]["children"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn set_no_hierarchy_uses_the_reference_string() {
        let mut v = HierarchyView::new();
        v.show(Mode::Call, "A", "");
        v.set_no_hierarchy();
        assert_eq!(v.empty(), "No hierarchy loaded.");
        assert!(v.sections().is_empty());
    }

    #[test]
    fn mode_parse_is_exact() {
        assert_eq!(Mode::parse("call"), Some(Mode::Call));
        assert_eq!(Mode::parse("type"), Some(Mode::Type));
        assert_eq!(Mode::parse("Call"), None, "case sensitive");
        assert_eq!(Mode::parse(""), None);
    }

    #[test]
    fn type_sections_root_then_base_chain_then_derived() {
        let node = serde_json::json!({"id": "T", "kind": "class", "full": "N.T", "line": 1});
        let rel = Relations {
            bases: vec!["B".into()],
            derived: vec!["D1".into(), "D2".into()],
        };
        let secs = type_sections(Some(&node), &rel, None);
        let labels: Vec<&str> = secs.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, vec!["Type", "Base chain", "Derived types"]);
        assert_eq!(secs[0].tree.as_ref().unwrap().id, "T");
        assert_eq!(secs[1].hint, "1 bases", "reference does not singularise");
        assert_eq!(secs[2].hint, "2 types");
        // the base row carries the inherits edge
        let kid = &secs[1].tree.as_ref().unwrap().children[0];
        assert_eq!(kid.id, "B");
        assert_eq!(kid.edge.as_deref(), Some("inherits"));
    }

    #[test]
    fn type_sections_without_bases_has_no_hint() {
        let node = serde_json::json!({"id": "T", "kind": "class"});
        let secs = type_sections(Some(&node), &Relations::default(), None);
        assert_eq!(secs[1].label, "Base chain");
        assert_eq!(secs[1].hint, "");
        assert!(secs[1].tree.is_none(), "no base chain row at all");
        assert_eq!(secs.len(), 2, "no derived section when there are none");
    }

    #[test]
    fn type_sections_prefers_chain_inherits_children_over_raw_bases() {
        // chainKids non-empty wins over rel.bases (hierarchy.js:613-622).
        let node = serde_json::json!({"id": "T", "kind": "class"});
        let rel = Relations {
            bases: vec!["RAW".into()],
            derived: vec![],
        };
        let chain_root = hop(
            "T",
            vec![Hop {
                id: "CHAIN".into(),
                label: "CHAIN".into(),
                edge: Some("inherits".into()),
                ..Hop::default_hop()
            }],
        );
        let secs = type_sections(Some(&node), &rel, Some(&chain_root));
        assert_eq!(secs[1].hint, "1 bases");
        let ids: Vec<&str> = secs[1]
            .tree
            .as_ref()
            .unwrap()
            .children
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, vec!["CHAIN"], "the chain row is used, not RAW");
    }

    #[test]
    fn type_sections_tolerate_a_missing_node() {
        let secs = type_sections(None, &Relations::default(), None);
        assert_eq!(secs[0].label, "Type");
        assert!(secs[0].tree.is_none(), "no root row without a node");
    }
}
