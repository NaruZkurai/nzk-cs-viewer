//! The flagship view: a native relation-graph canvas faithfully ported from the
//! original frontend's canvas pass.
//!
//! The geometry, palette and layout live in [`crate::graphviz`] (no `egui`
//! types); this module only turns that geometry into `egui` shapes and handles
//! pan/zoom/pick/select. Keeping the two apart is what lets the drawing fidelity
//! survive a change of rendering stack.

use std::collections::{HashMap, HashSet};

use egui::epaint::{EllipseShape, QuadraticBezierShape};
use egui::{
    Align2, Color32, FontId, Id, Order, Painter, PointerButton, Pos2, Rect, Sense, Stroke,
    StrokeKind, Vec2,
};

use crate::graphviz::{
    self, Camera, ChildPlacement, ChildSpec, ChipMetrics, EdgeDraw, HalfSize, LayoutInput,
    LayoutSettings, Palette, Rgba, SeedContinuity, UiTheme,
};
use crate::actions::{SliderEdit, SliderKind, SliderPhase};
use crate::model::Graph;

/// Which nodes the canvas shows.
///
/// The original's default view is namespace-first (`graph.js loadNamespaces`):
/// only namespaces are on the canvas until one is expanded. `Full` approximates
/// its "Full graph" action by showing the whole model, which is how the A/B
/// evidence exercises edges, curves and pills.
///
/// Selected at startup from `NATIVE_CSVIEWER_VIEW` (`full` => `Full`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewMode {
    Namespaces,
    Full,
}

#[derive(Clone, Debug)]
struct NodeVis {
    /// Index into `Graph::nodes`.
    index: usize,
    /// Position in the visible order (edge slots address this).
    slot: usize,
    /// World position.
    pos: (f64, f64),
    /// World chip size.
    size: (f32, f32),
    fixed: bool,
    /// True until a layout run has actually placed this node.
    ///
    /// This is what makes "continue from the positions on screen" decidable, and
    /// it is a flag rather than a coordinate sentinel because the sentinel is
    /// wrong: a one-node graph legitimately solves to exactly `(0.0, 0.0)`, so
    /// "is at the origin" cannot be read as "has no position" — which is exactly
    /// how a first layout of a single namespace would be mistaken for an
    /// expansion. `rebuild_view` mints every node unplaced; `relayout` clears
    /// the flag as it writes each position.
    unplaced: bool,
}

/// One live left-button drag of a node chip (`interaction.js:73-84`).
///
/// The offset is in **world** units: it is the vector from the pointer to the
/// chip centre at the press, so the chip keeps the exact grab point while the
/// pointer moves rather than snapping its centre under the cursor.
#[derive(Clone, Copy, Debug)]
struct NodeDrag {
    index: usize,
    off_x: f64,
    off_y: f64,
}

/// The interactive graph canvas: namespace-first view, FA2 layout, camera,
/// picking and selection.
pub struct GraphView {
    pub graph: Graph,
    pub theme: UiTheme,
    pub palette: Palette,
    pub settings: LayoutSettings,
    pub view_mode: ViewMode,
    pub selected: Option<usize>,
    pub hovered: Option<usize>,
    pub show_edges: bool,
    pub show_labels: bool,
    /// True while a shell modal (the folder picker, Shortcuts, Edit, Preview or
    /// Reexport) covers the viewport.
    ///
    /// The CSS modal is a real full-viewport backdrop painted by `nzk_ui`;
    /// the native canvas beneath it is still an egui widget, so without this the
    /// canvas would receive the press as well — a click on a picker row would
    /// also select/clear a graph node, and the selection-change handler in
    /// `App::frame` would then close the modal (`NzkUiState::surface = None`).
    /// The same click would fall through the `#browseBtn` backdrop. The host
    /// sets this from the painted geometry each frame, so the canvas stops
    /// taking input exactly while a backdrop is on screen and not a frame
    /// sooner.
    pub input_suspended: bool,
    /// `state.filters.groupChildren` (`core.js:270`, default `true`).
    ///
    /// Off ⇒ `contains` edges are dropped from the drawn edge list
    /// (`graph.js:812`). The reference exposes this one flag through **two
    /// mutually-inverse checkboxes**:
    ///
    /// * `#groupChildren` shows the flag directly
    ///   (`shell.js:603`: `state.filters.groupChildren = groupChildrenEl.checked`);
    /// * `#hideChildren` shows its inverse
    ///   (`shell.js:646`: `state.filters.groupChildren = !hideChildrenEl.checked`),
    ///   and each change handler re-mirrors the other
    ///   (`shell.js:604`, `:647`; `graph.js:688-689`; `sync.js:166`).
    ///
    /// So the field is stored **once** and the inversion lives at the two UI
    /// edges — see [`GraphView::set_group_children`], which is the only writer
    /// and keeps both boxes consistent by construction.
    ///
    /// Deliberately a plain `bool` on the view rather than a separate filter
    /// struct: `Graph` already holds every edge, so this is a *view* filter like
    /// [`Self::show_edges`], never a re-index.
    pub group_children: bool,
    /// `state.filters.groupEdges` (`core.js:271`), the flag behind `#groupEdges`
    /// — the *edges* counterpart of [`Self::group_children`], and a genuinely
    /// distinct control (`#groupEdges` vs `#groupChildren`).
    ///
    /// Where `group_children` gates the `contains` edges, this one gates
    /// everything else: `graph.js:814` —
    /// `if (e.kind !== 'contains' && !f.groupEdges) return false;`. With the
    /// flag **off** the view keeps only containment edges and drops every
    /// Calls/Uses/Inherits/... edge; with it **on** (the shipped default) the
    /// `contains`/non-`contains` split above is decided purely by
    /// `group_children`.
    ///
    /// `core.js:271` ships it `true` and `index.html:255` ships
    /// `#groupEdges checked hidden` — a hidden box that defaults to on, which is
    /// why the reset path (`filters.js:412`, `:418`) restores both the flag and
    /// the box together.
    ///
    /// Like `group_children` this is a plain `bool` on the view rather than a
    /// filter struct: `Graph` already holds every edge, so it is a *view* filter
    /// that only chooses which drawn edges survive, never a re-index.
    pub group_edges: bool,
    /// `state.filters.hideIsolated` (`core.js` default `false`,
    /// `#hideIsolated`): drop every node that has no **drawn** edge.
    ///
    /// `graph.js:814-820` filters this *after* the drawn edge list exists:
    /// `used` collects both endpoints of every surviving edge, `nodes` keeps only
    /// those, then `edges` is re-filtered against the smaller set. A view filter
    /// like the two grouping flags — `Graph` keeps every node — written only by
    /// [`Self::set_filters`].
    pub hide_isolated: bool,
    /// `state.filters.hideEdgeLabels` (`#hideEdgeLabels`): hide only the edge
    /// word pills (Calling/Uses/…), never the lines or arrows. Read by the
    /// canvas paint's `label_all` gate.
    pub vt_mode: bool,
    /// "Only related nodes": the visible set is the selected node plus every
    /// node it shares an edge with, in both directions.
    ///
    /// Anchored on `self.selected`, so a selection made anywhere (graph, drawer
    /// relation row, find row, chain hop) moves the focus. `focus_selected`
    /// remembers the selection the current visible set was built from, which is
    /// what lets [`Self::sync_focus_selection`] rebuild only on a real change.
    /// The relation definition matches [`Self::all_neighbours`] (the set the
    /// drawer's `Expand` uses), so the canvas and the drawer agree about what
    /// counts as related. Unlike the old one-node filter, turning this off
    /// restores every node the ambient filters allow.
    pub focus_only: bool,
    /// "Hide parents": the selected node's containment ancestors are not drawn.
    ///
    /// The ancestor chain is the same `contains` walk the drawer's `Parent`
    /// bucket is built from ([`Self::parent_of`]), so a node the drawer calls a
    /// parent is exactly a node this filter hides.
    pub focus_parents: bool,
    /// The selection the visible set was last filtered against. Only meaningful
    /// while one of the focus flags is on.
    focus_selected: Option<usize>,
    /// Node kinds whose sidebar chip is OFF (`filters.js`). Empty = every kind on,
    /// which is the shipped render. Written only by [`Self::set_filters`].
    hidden_kinds: std::collections::BTreeSet<String>,
    /// Edge kinds whose sidebar chip is OFF. Empty = every edge kind drawn.
    hidden_edges: std::collections::BTreeSet<String>,
    /// Project ids whose sidebar chip is OFF (`filters.js`
    /// `state.filters.projects`). The project model itself (`Graph::projects`)
    /// is the mapping owner; this set only gates it. Empty = every project on.
    filter_projects: std::collections::BTreeSet<String>,
    /// `#nsSelect` value; empty = "All namespaces".
    filter_namespace: String,
    /// `#nsDeep` — include sub-namespaces (`index.html` ships it checked).
    filter_ns_deep: bool,
    /// Node ids whose `contains` children the drawer's `Hide children` scope has
    /// hidden (`drawer.js` `state.hiddenChildren`).
    ///
    /// v2's `±` removed a relation bucket from an incrementally built map; this
    /// fork already holds every node/edge, so the faithful analogue is a *view*
    /// predicate exactly like the sidebar filters: the children leave the
    /// canvas (and their drawn edges leave with them) while `Graph` keeps them,
    /// so `Show children` restores the same nodes with no re-index. The set is
    /// the ONE drawer owner's data (`DrawerUi::hidden_children`) mirrored here
    /// by [`Self::sync_hidden_children`]; this field is never a second owner.
    hidden_children: std::collections::BTreeSet<String>,
    /// Set true when "Fit" ran (consumed by the screenshot path).
    pub fit_pending: bool,

    /// Nodes the user double-clicked open, by `Graph::nodes` index.
    ///
    /// This is the navigation model `actions.rs` records as missing. It is a
    /// *view* filter, never a re-index: `Graph` already holds every node and
    /// edge, so expanding is O(neighbours) bookkeeping over data that is
    /// already resident (`graph.js expandNode` had to fetch `/api/node`
    /// because its model was lazy; ours is not).
    ///
    /// Only meaningful in [`ViewMode::Namespaces`]; [`ViewMode::Full`] shows
    /// the whole model by definition, so this set is simply not consulted.
    expanded: HashSet<usize>,
    /// Expanded nodes whose **every** relation bucket was pulled, by
    /// `Graph::nodes` index.
    ///
    /// v2 has two expansion gestures: a double-click pulls containment plus
    /// calls/inherits (`graph.js:549-590`), while the node drawer's `Expand`
    /// button pulls every bucket (`expandNode(id, {all:true})`). This set is
    /// what tells the two apart. Always a subset of [`Self::expanded`]; keyed
    /// by graph index for the same reason.
    expanded_all: HashSet<usize>,
    /// Maximum neighbour hops revealed for each expanded node.
    expansion_depth: u32,
    /// Nodes the user pinned, by `Graph::nodes` index.
    ///
    /// `NodeVis.fixed` was painted (a dot on the chip) but never set by anything,
    /// so the pin could not appear. This is the real backing store, and it drives
    /// both halves of what "pinned" means: the dot is drawn, and the node is
    /// exempt from re-settle movement.
    ///
    /// Keyed by graph index like `expanded`, so a pin survives a collapse/
    /// re-expand cycle and a change of view mode — the pin is a property of the
    /// *node*, not of the layout run that happened to place it.
    pinned: HashSet<usize>,
    /// Selection history, newest last: the real backing store for Back.
    ///
    /// `graph.js selectNode` pushes the *previous* selection here (capped at 80)
    /// and `navigateBack` pops it. Kept as `Graph::nodes` indices, and pruned of
    /// anything no longer visible so Back can only ever jump somewhere real.
    history: Vec<usize>,
    /// Selection history, oldest last: the real backing store for Forward.
    ///
    /// The mirror of `history`: `navigate_back` records the node it leaves here
    /// so `navigate_forward` can retrace it, and any genuinely new selection
    /// clears it so Forward can never resurrect a branch the user abandoned.
    /// Kept as `Graph::nodes` indices and pruned exactly like `history`.
    forward: Vec<usize>,
    vis: Vec<NodeVis>,
    /// Every node id's last laid-out position and measured box, alive or not.
    ///
    /// This is the fork's stand-in for `graph.js`'s process-lifetime `state.pos`
    /// (`graph.js:269`): a node that leaves the canvas (a collapse, a filter, or
    /// "Only related nodes") and comes back is restored where the user last saw it,
    /// not dropped on the origin. Keyed by the node's stable id rather than a
    /// `Graph::nodes` index, so a prune that remaps indices does not silently
    /// attach a position to the wrong node.
    positions: HashMap<String, (f64, f64, (f32, f32))>,
    /// `Graph::nodes` index -> visible slot.
    slot_of: HashMap<usize, usize>,
    /// Visible edges: `(slot_from, slot_to, edge_index)`.
    edges: Vec<(usize, usize, usize)>,
    /// Parallel-edge fan for each drawn edge: `(pair_index, pair_count)`.
    ///
    /// `paint_canvas` needs this on every frame, and it used to rebuild the
    /// grouping with a fresh `HashMap` plus two more maps per frame — the same
    /// answer for the same edge set. It is derived in [`Self::rebuild_view`], the
    /// only place `edges` changes, so the paint pass is a lookup.
    edge_fan: Vec<(f32, f32)>,
    /// Screen-space position of every `vis` slot, recomputed once per paint pass.
    ///
    /// The edge loop and the node loop both need it, and every edge used to call
    /// `world_to_screen` twice; this is the one shared conversion per slot.
    screen_pos: Vec<(f32, f32)>,
    cam: Camera,
    metrics: ChipMetrics,
    layout_dirty: bool,
    /// The node set the last completed layout run settled, by `Graph::nodes`
    /// index.
    ///
    /// This is what lets a run tell "the set grew" from "the set lost nodes"
    /// without a caller-set flag: growth means continue, loss means re-seed.
    /// Empty until the first run finishes.
    settled_keys: HashSet<usize>,
    /// One-shot override of the next run's seeding branch ([`Self::reseed_layout`]).
    ///
    /// `None` (the normal state) means "decide from the data": continue when the
    /// visible set still holds what was on screen, re-seed when it does not. Kept
    /// as an explicit override rather than a boolean flag that `rebuild_view`
    /// sets, so a caller's intent cannot leak into an unrelated later run.
    seed_override: Option<SeedContinuity>,
    canvas_w: f32,
    canvas_h: f32,
    /// Nodes selected by the last completed marquee, by `Graph::nodes` index.
    ///
    /// Distinct from `selected: Option<usize>` on purpose: `selected` is the
    /// single *focused* node that Back/Up/Enter act on and that the source pane
    /// follows, while this is the *set* a marquee grabbed. Collapsing the two
    /// would either make a marquee destroy the focus/history model, or force
    /// `selected` to become a set and break every existing caller.
    marquee_sel: HashSet<usize>,
    /// Live marquee drag, in canvas-relative screen points: `(anchor, cursor)`.
    ///
    /// `None` when no marquee is in progress. Only Some *while the button is
    /// down*, so a stale rectangle can never outlive its gesture.
    marquee: Option<(Pos2, Pos2)>,
    /// The node chip being dragged, if any (`interaction.js:73-84`).
    ///
    /// A drag that starts on a chip moves that chip and pins it in place; it
    /// must not pan the camera (a pan is the empty-canvas / middle-button
    /// gesture). Keeping a distinct mode is what lets the two drags coexist on
    /// one pointer.
    node_drag: Option<NodeDrag>,
    /// The right-click card pinned open for one node (`interaction.js:291-301`).
    ///
    /// `pos` is canvas-relative, matching the marquee coordinate space, so the
    /// card stays under the user's pointer when the camera moves. `None` when
    /// no card is pinned; a left click on the canvas or Esc closes it.
    pinned_card: Option<(usize, Pos2)>,
    /// The node whose `{ } View code` card action was pressed.
    ///
    /// The canvas cannot open the source pane (it owns no `Index` and no
    /// `SourceState`), so the request is queued here and the host completes it
    /// after the paint pass through the same `Effect::source_open` path the
    /// drawer's button uses.
    node_code_request: Option<String>,
    /// The arrange/spacing model with the reference's module-level `gap`
    /// (`arrange.js:41`). One per view, so a second `Shift+=` widens from the
    /// already-widened spacing instead of restarting at 16.
    arrangement: crate::chords::arrange::Arrangement,
}

/// The camera that puts the world point `world` at the centre of the canvas,
/// keeping `zoom`. Pure so the centring rule is assertable without a window:
/// `Camera::world_to_screen` is `(world - cam) * zoom + size/2`, so selecting
/// `cam = world` makes the screen result exactly `size/2` at any zoom.
fn centred_camera(zoom: f64, world: (f64, f64)) -> (f64, f64, f64) {
    (zoom, world.0, world.1)
}

fn col(c: Rgba) -> Color32 {
    Color32::from_rgba_unmultiplied(c.0, c.1, c.2, c.3)
}

/// The original applies `globalAlpha` before filling an opaque colour; this is
/// that blend, with the alpha kept straight.
fn col_a(c: Rgba, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(c.0, c.1, c.2, (a.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// `render.js clamp(Math.sqrt(zoom), 0.7, 2.2)` (unused here; kept for parity).
#[allow(dead_code)]
fn width_scale(zoom: f32) -> f32 {
    zoom.sqrt().clamp(0.7, 2.2)
}

/// Screen-space margin used when culling shapes against the canvas.
///
/// Covers a chip's maximum half-extent plus the bow of a fanned quadratic edge,
/// so a shape that is culled can never have a pixel inside the clip rect.
const EDGE_CULL_MARGIN: f32 = 64.0;

/// Edges actually on screen above which per-edge ornament (arrowheads, relation
/// pills) is dropped for everything except the focused/touched edges.
///
/// Well above a normal view (the vrcfury "full graph" shows ~28k), and never
/// reached by the small reference corpora the fidelity gates use.
const DENSE_EDGE_LIMIT: usize = 6000;

/// Chips actually on screen above which un-focused labels are dropped.
const DENSE_NODE_LIMIT: usize = 3000;

/// Rendered font size (px) at which a dense canvas keeps its labels.
const DENSE_LABEL_MIN_FONT: f32 = 9.0;

/// Startup override for VT mode (`NATIVE_CSVIEWER_VT_MODE=1|on|true`).
///
/// The checkbox is the user's switch; this exists so a headless
/// `--screenshot` (and a review run) can render the mode without a pointer.
/// It is read once at startup and then behaves exactly like a checked box.
pub fn vt_mode_from_env() -> bool {
    matches!(
        std::env::var("NATIVE_CSVIEWER_VT_MODE").as_deref(),
        Ok("1") | Ok("on") | Ok("true") | Ok("ON") | Ok("TRUE")
    )
}

impl GraphView {
    pub fn new(graph: Graph) -> Self {
        let view_mode = match std::env::var("NATIVE_CSVIEWER_VIEW").as_deref() {
            Ok("full") | Ok("Full") | Ok("all") => ViewMode::Full,
            _ => ViewMode::Namespaces,
        };
        let mut v = GraphView {
            graph,
            theme: UiTheme::NzkDarkPlus,
            palette: Palette::of(UiTheme::NzkDarkPlus),
            settings: LayoutSettings::default(),
            view_mode,
            selected: None,
            hovered: None,
            show_edges: true,
            // `core.js:270` — `groupChildren: true` is the shipped default, and
            // it is what keeps the default render byte-identical: at `true` no
            // `contains` edge is dropped, so the drawn edge set is exactly what
            // it was before this flag existed.
            group_children: true,
            // `core.js:271` ships `groupEdges: true`; `index.html:255` ships
            // `#groupEdges checked`, so the document and the model agree.
            group_edges: true,
            // `core.js` ships `hideIsolated: false` and `hideEdgeLabels: false`
            // — the shipped render draws every node and every pill.
            hide_isolated: false,
            focus_only: false,
            focus_parents: false,
            focus_selected: None,
            vt_mode: vt_mode_from_env(),
            // The shipped filter model: every chip ON, all namespaces.
            hidden_kinds: std::collections::BTreeSet::new(),
            hidden_edges: std::collections::BTreeSet::new(),
            filter_projects: std::collections::BTreeSet::new(),
            filter_namespace: String::new(),
            filter_ns_deep: true,
            hidden_children: std::collections::BTreeSet::new(),
            show_labels: true,
            input_suspended: false,
            fit_pending: true,
            expanded: HashSet::new(),
            expanded_all: HashSet::new(),
            expansion_depth: crate::form_wire::DEPTH_DEFAULT,
            pinned: HashSet::new(),
            history: Vec::new(),
            forward: Vec::new(),
            vis: Vec::new(),
            positions: HashMap::new(),
            slot_of: HashMap::new(),
            edges: Vec::new(),
            edge_fan: Vec::new(),
            screen_pos: Vec::new(),
            cam: Camera::default(),
            metrics: ChipMetrics::new(graphviz::NODE_SIZE_DEFAULT),
            layout_dirty: true,
            settled_keys: HashSet::new(),
            seed_override: None,
            canvas_w: 1.0,
            canvas_h: 1.0,
            marquee_sel: HashSet::new(),
            marquee: None,
            node_drag: None,
            pinned_card: None,
            node_code_request: None,
            arrangement: crate::chords::arrange::Arrangement::new(),
        };
        v.rebuild_view();
        v
    }

    /// Copy every placed `vis` slot's geometry into the id-keyed store.
    ///
    /// Must run while `vis` and `graph` still describe the same node list: the
    /// store is read by [`Self::rebuild_view`] to restore returning nodes.
    fn remember_visible_positions(&mut self) {
        for v in &self.vis {
            if !v.unplaced {
                let id = self.graph.nodes[v.index].id.clone();
                self.positions.insert(id, (v.pos.0, v.pos.1, v.size));
            }
        }
    }

    /// Adopt a newly built graph (a completed re-index) without mistaking old
    /// positions for new nodes' coordinates.
    ///
    /// The old `vis` indices address the OLD node list, so they are recorded into
    /// the id-keyed store and then discarded. Positions follow a node id that
    /// survives the rebuild; a node id the store has never seen is unplaced, and
    /// the next layout seeds it. Every index-valued model (expansion, pins,
    /// selection, history, marquee) is dropped for the same reason — an old index
    /// no longer names the node it did, and inventing a mapping would be worse
    /// than the honest reset.
    pub fn install_graph(&mut self, graph: crate::model::Graph) {
        self.remember_visible_positions();
        self.graph = graph;
        self.vis.clear();
        self.slot_of.clear();
        self.edges.clear();
        self.settled_keys.clear();
        self.selected = None;
        self.hovered = None;
        self.history.clear();
        self.forward.clear();
        self.marquee_sel.clear();
        self.expanded.clear();
        self.expanded_all.clear();
        self.pinned.clear();
        self.rebuild_view();
    }

    /// Recompute the visible subset (node order + visible edges). The order is
    /// load-bearing: `graph.js loadNamespaces` sorts namespaces by `full` before
    /// `seedNewNodes` runs, and the seed depends on that order.
    ///
    /// # Positions survive the rebuild
    ///
    /// A node that was already placed keeps its position here, and one that this
    /// rebuild adds is marked [`NodeVis::unplaced`]. That is what makes an
    /// expansion incremental in the live app: `expand_node` calls this, and the
    /// very next `relayout` sees "the set grew, the placed nodes still hold their
    /// coordinates" and continues, instead of being handed a canvas of origins
    /// and correctly concluding it must start over.
    ///
    /// A collapse keeps the collapsed nodes' positions in [`Self::positions`],
    /// keyed by id, so re-expanding them restores the arrangement instead of
    /// dumping them on the origin. The re-seed decision still treats the *set*
    /// losing nodes as the trigger ([`Self::relayout`]); this store only decides
    /// where a returning node lands.
    ///
    /// In [`ViewMode::Namespaces`] the visible set is the namespace roots plus the
    /// children of every expanded node — `graph.js loadNamespaces` installs the
    /// namespaces and *no* edges, and `expandNode` is what pulls each child in.
    /// Before this existed the namespaces branch returned the roots and nothing
    /// could ever add to them, which is why the shipped canvas was permanently
    /// one node.
    pub fn rebuild_view(&mut self) {
        let mut indices: Vec<usize> = match self.view_mode {
            ViewMode::Namespaces => {
                let ns: Vec<usize> = (0..self.graph.nodes.len())
                    .filter(|&i| self.graph.nodes[i].kind == "namespace")
                    .collect();
                if ns.is_empty() {
                    // No namespace nodes: the original falls back to the
                    // top-level types (`loadNamespaces`). Nothing is ever
                    // contained by them here, so expansion matches nothing and
                    // the fallback list is returned as-is.
                    (0..self.graph.nodes.len()).collect()
                } else {
                    self.visible_in_namespace_mode(&ns)
                }
            }
            ViewMode::Full => (0..self.graph.nodes.len()).collect(),
        };
        // Sorting is what makes the layout seed deterministic, and `expanded`
        // holds indices, so membership is stable under it.
        indices.sort_by(|&a, &b| {
            let na = &self.graph.nodes[a];
            let nb = &self.graph.nodes[b];
            (&na.kind, &na.full, &na.label).cmp(&(&nb.kind, &nb.full, &nb.label))
        });

        // The sidebar filter model (`filters.js`): a node whose kind chip is OFF
        // or that sits outside the selected namespace is not drawn. This is a
        // *view* predicate, never a re-index — `Graph` still holds every node.
        //
        // The drawer's `Hide children` scope joins it here: the hidden node's
        // `contains` children are removed from the visible set, so their drawn
        // edges drop out through the `slot_of` check below (an edge can never
        // point at a node that is not on the canvas). Unlike the sidebar chips
        // this is per-parent scope, not a global kind predicate, which is why it
        // is a node-id set rather than a field on `FilterState`.
        let hidden_by_scope = self.hidden_child_indices();
        indices.retain(|&i| {
            !hidden_by_scope.contains(&i) && self.filter_passes(&self.graph.nodes[i])
        });

        // ---- focus filters ("Only related nodes" / "Hide parents") -----------
        //
        // Both are anchored on the live selection and applied here, before the
        // visible set is minted, so their edges drop out through the `slot_of`
        // check exactly like a hidden kind. The edge gate below additionally
        // keeps only edges incident to the focused node, so a relation between
        // two of its neighbours is not mistaken for a relation to it.
        self.focus_selected = self.selected;
        let focus = self.selected.filter(|&i| i < self.graph.nodes.len());
        if self.focus_only {
            // `clear` + `extend` rather than `retain`: the focus is an explicit
            // request to see the node's relations even when the ambient view
            // mode (namespace-only) or a kind predicate would not have put them
            // on the canvas. `all_neighbours` is the same relation definition
            // the drawer's `Expand` uses, so its buckets and this set agree.
            // It is still a view predicate — the graph keeps everything.
            if let Some(sel) = focus {
                indices.clear();
                indices.extend(self.all_neighbours(sel));
            }
        } else if self.focus_parents {
            if let Some(sel) = focus {
                let ancestors = self.containment_ancestors(sel);
                indices.retain(|i| !ancestors.contains(i));
            }
        }

        // `#hideIsolated` (`graph.js:814-820`): drop every node with no DRAWN
        // edge. This has to run before `vis` is minted, because the drawn edge
        // set is defined by which endpoints have slots; filtering `vis`
        // afterwards would leave the edges pointing at slots that no longer
        // exist. The candidate edges are computed here with exactly the gates
        // the draw loop below applies (grouping flags + hidden edge kinds +
        // non-self edges), restricted to the nodes that survived the predicates
        // above — so the two passes cannot disagree about what is "isolated".
        //
        // In the default namespace view with nothing expanded there are no drawn
        // edges at all, so the reference's own `computeView` would drop everything
        // too; that is the honest reading of the flag, not a special case.
        if self.hide_isolated
            && !self.focus_only
            && (self.view_mode != ViewMode::Namespaces || !self.expanded.is_empty())
        {
            let visible: HashSet<usize> = indices.iter().copied().collect();
            let mut used: HashSet<usize> = HashSet::new();
            for e in &self.graph.edges {
                if e.from == e.to
                    || !visible.contains(&e.from)
                    || !visible.contains(&e.to)
                    || (e.kind == "contains" && !self.group_children)
                    || self.hidden_edges.contains(&e.kind)
                    || (e.kind != "contains" && !self.group_edges)
                {
                    continue;
                }
                used.insert(e.from);
                used.insert(e.to);
            }
            indices.retain(|i| used.contains(i));
        }

        // `graph.js` keeps one position per node id for the life of the process
        // (`state.pos`), so a rebuild of the node list never costs a placed node
        // its coordinates. Record the visible nodes' current geometry here, then
        // mint every slot from that same id-keyed store — not only from the
        // previous `vis`, so a node that was filtered away and is now returning
        // comes back to its own place instead of to the origin.
        self.remember_visible_positions();
        let known = std::mem::take(&mut self.positions);

        self.vis = indices
            .iter()
            .enumerate()
            .map(|(slot, &index)| match known.get(&self.graph.nodes[index].id) {
                Some(&(x, y, size)) => NodeVis {
                    index,
                    slot,
                    pos: (x, y),
                    size,
                    fixed: false,
                    unplaced: false,
                },
                None => NodeVis {
                    index,
                    slot,
                    pos: (0.0, 0.0),
                    size: (60.0, 13.0),
                    fixed: false,
                    unplaced: true,
                },
            })
            .collect();
        self.positions = known;
        // `slot_of` is rebuilt from the same `indices` that minted `vis`, so a
        // slot can never outlive the node it addresses. Every consumer below
        // (`focus_slot`, the edge loop, `pick`) reads `self.vis[slot]`, which is
        // why the two must be derived together rather than patched.
        self.slot_of.clear();
        for v in &self.vis {
            self.slot_of.insert(v.index, v.slot);
        }
        // Re-apply the pins to the freshly minted `NodeVis`es. `vis` is rebuilt
        // from scratch here, so a pin would otherwise be lost — including the
        // benign-looking case of pinning a node that is not currently visible and
        // then expanding to reveal it.
        self.sync_fixed_flags();

        // History holds indices, and an index can stop being visible when the
        // node that pulled it in is collapsed. Drop those now, so Back can only
        // ever return to something that is genuinely on the canvas.
        self.history.retain(|i| self.slot_of.contains_key(i));
        // The same pruning for Forward: a collapsed node must not stay a
        // reachable Forward destination.
        self.forward.retain(|i| self.slot_of.contains_key(i));

        self.edges = Vec::new();
        // The default namespace view installs only the namespace nodes and NO
        // edges — `graph.js loadNamespaces` has no edge pass at all, so the
        // original's untouched canvas has no lines. Edges appear only where
        // expansion put both endpoints on screen. `Full` keeps its whole-model
        // edge pass, which is the A/B evidence path. "Only related nodes" is an
        // explicit edge pass too: it overrides the ambient set, so its relations
        // must be drawn even before any expansion.
        if self.view_mode != ViewMode::Namespaces
            || !self.expanded.is_empty()
            || self.focus_only
        {
            for (ei, e) in self.graph.edges.iter().enumerate() {
                if let (Some(&a), Some(&b)) = (self.slot_of.get(&e.from), self.slot_of.get(&e.to))
                {
                    if a != b {
                        // "Only related nodes" draws the focused node's own
                        // relations, not a relation between two of its
                        // neighbours: keep only edges with the anchor as an
                        // endpoint. The node set above already bounds the rest.
                        if self.focus_only
                            && self
                                .focus_selected
                                .is_some_and(|sel| e.from != sel && e.to != sel)
                        {
                            continue;
                        }
                        // `#hideChildren` / `#groupChildren`: `graph.js:812`
                        // drops `contains` from the *visible edge list* when the
                        // flag is off —
                        //     `if (e.kind === 'contains' && !f.groupChildren) return false;`
                        // and that filter sits in the block that builds the edges
                        // it then draws. This `self.edges` rebuild is the
                        // faithful analogue: it is the drawn-edge candidate list.
                        //
                        // It is deliberately NOT [`Self::neighbours`] (:377).
                        // That function answers "what does a double-click pull
                        // onto the canvas", and the reference keeps pulling
                        // containment children in `expandNode` regardless of the
                        // flag — it only stops *drawing* the containment lines.
                        // Gating `neighbours` instead would make the checkbox
                        // silently disable expansion, which the reference does
                        // not do and which the comments at :313-328 call out as
                        // the way this gesture is supposed to work.
                        if !self.group_children && e.kind == "contains" {
                            continue;
                        }
                        // `graph.js:814` — the complementary gate. Exactly one
                        // of these two can fire for a given edge, because they
                        // test `kind == "contains"` and `kind != "contains"`;
                        // `contains` edges answer to `group_children`, every
                        // other kind answers to `group_edges`.
                        // The sidebar filter model's edge half (`filters.js`):
                        // an edge kind whose chip is OFF is not drawn, on top of
                        // the two grouping gates. A predicate here (not only a
                        // chip class) is what makes the chip real.
                        if self.hidden_edges.contains(&e.kind) {
                            continue;
                        }
                        if !self.group_edges && e.kind != "contains" {
                            continue;
                        }
                        self.edges.push((a, b, ei));
                    }
                }
            }
        }
        // The parallel-relation fan, derived ONCE per edge-set change. This is
        // exactly what `paint_canvas` used to recompute every frame with three
        // `HashMap`s; same grouping, same deterministic member order (the edge
        // order), so the drawn strands cannot move.
        self.edge_fan.clear();
        self.edge_fan.resize(self.edges.len(), (0.0, 1.0));
        let mut groups: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
        for (k, &(a, b, _)) in self.edges.iter().enumerate() {
            groups.entry(if a < b { (a, b) } else { (b, a) }).or_default().push(k);
        }
        for members in groups.values() {
            let count = members.len() as f32;
            for (i, &k) in members.iter().enumerate() {
                self.edge_fan[k] = (i as f32, count);
            }
        }
        self.layout_dirty = true;
    }

    /// Namespaces plus everything the expanded nodes pulled in.
    ///
    /// Children are `contains` in the out direction — the engine stores a
    /// containment edge as `source` = container, `target` = contained (verified
    /// against live data: `NZK.E.C` contains `NZK.E.C.e`). This mirrors
    /// `graph.js expandNode`, where the `contains:out` relation is always wanted.
    ///
    /// The pull is **not only containment**. `expandNode` also adds
    /// `inherits:out` for a type-like node and `calls:out` for a member, and the
    /// nodes on the far end of those come with it — that is the "calling edges"
    /// half of the canvas notice. Computing the visible set from `contains`
    /// alone made that half a no-op: the edge was in `self.edges`' candidate set
    /// but one of its endpoints was never on the canvas, so a method's call to
    /// another type's method never drew. The set therefore comes from
    /// [`Self::expansion_neighbours`], the same function that defines what each
    /// gesture pulls, so the two can never disagree. That function also carries
    /// the drawer's `Expand` distinction: a node opened with every bucket uses
    /// [`Self::all_neighbours`] instead of the narrower double-click pull.
    ///
    /// Only one hop is walked even when an expanded node's child is itself
    /// expanded. That is the original's behaviour too: `expandNode` is called
    /// per double-click, so a chain grows one level per gesture.
    fn visible_in_namespace_mode(&self, roots: &[usize]) -> Vec<usize> {
        if self.expanded.is_empty() {
            return roots.to_vec();
        }
        let mut out: Vec<usize> = roots.to_vec();
        let mut seen: HashSet<usize> = roots.iter().copied().collect();
        for &e in &self.expanded {
            let mut frontier = vec![e];
            let mut visited = HashSet::from([e]);
            // The expanded node itself is part of the pull (depth 1 = the node
            // and one hop). Depth 0 is the reference's "just the focused node",
            // so it does not add the expanded node either.
            if self.expansion_depth > 0 && seen.insert(e) {
                out.push(e);
            }
            for _ in 0..self.expansion_depth.min(self.graph.nodes.len() as u32) {
                let mut next = Vec::new();
                for node in frontier {
                    for neighbour in self.expansion_neighbours(node) {
                        if visited.insert(neighbour) {
                            if seen.insert(neighbour) {
                                out.push(neighbour);
                            }
                            next.push(neighbour);
                        }
                    }
                }
                frontier = next;
                if frontier.is_empty() { break; }
            }
        }
        out
    }

    /// Set the expansion depth for existing and future expanded nodes.
    pub fn set_expansion_depth(&mut self, depth: u32) -> bool {
        let depth = depth.clamp(crate::form_wire::DEPTH_MIN, crate::form_wire::DEPTH_MAX);
        if self.expansion_depth == depth {
            return false;
        }
        self.expansion_depth = depth;
        self.rebuild_view();
        true
    }

    /// The edges double-clicking pulls in, beyond containment.
    ///
    /// The original's notice says "children and calling edges": `expandNode`
    /// always adds `contains:out`, then adds `inherits:out` for a type-like node
    /// and `calls:out` for a member. The nodes on the far end of those are pulled
    /// onto the canvas by [`Self::neighbours`] so those edges have somewhere to
    /// land.
    fn pull_kinds_for(&self, index: usize) -> &'static [&'static str] {
        const TYPE_LIKE: [&str; 6] =
            ["namespace", "class", "struct", "interface", "enum", "record"];
        if TYPE_LIKE.contains(&self.graph.nodes[index].kind.as_str()) {
            &["contains", "inherits"]
        } else {
            &["contains", "calls"]
        }
    }

    /// Everything one double-click of `index` brings onto the canvas, itself
    /// included: the containment children, plus the call/inherit neighbours the
    /// pulled-in edges address.
    fn neighbours(&self, index: usize) -> Vec<usize> {
        if index >= self.graph.nodes.len() {
            return Vec::new();
        }
        let kinds = self.pull_kinds_for(index);
        let mut out: Vec<usize> = vec![index];
        let mut seen: HashSet<usize> = HashSet::from([index]);
        for e in &self.graph.edges {
            if !kinds.contains(&e.kind.as_str()) {
                continue;
            }
            let other = if e.from == index {
                e.to
            } else if e.to == index {
                // Containment is directed, so a `contains` edge pointing *at* us
                // is our parent, not our child; only the undirected kinds pull
                // backwards.
                if e.kind == "contains" {
                    continue;
                }
                e.from
            } else {
                continue;
            };
            if seen.insert(other) {
                out.push(other);
            }
        }
        out
    }

    /// Everything the drawer's `Expand` brings onto the canvas: every edge
    /// touching `index`, in both directions, itself included.
    ///
    /// v2's `expandNode(id, {all:true})` (`graph.js:561-590`) walks *every*
    /// relation bucket `/api/node` reports, not just containment and calls. The
    /// resident graph already holds those edges, so the faithful pull is "all
    /// incident edges"; `contains` stays directed, while every other kind is
    /// symmetric (an incoming `calls` is a caller and must come too).
    fn all_neighbours(&self, index: usize) -> Vec<usize> {
        if index >= self.graph.nodes.len() {
            return Vec::new();
        }
        let mut out: Vec<usize> = vec![index];
        let mut seen: HashSet<usize> = HashSet::from([index]);
        for e in &self.graph.edges {
            let other = if e.from == index {
                e.to
            } else if e.to == index {
                e.from
            } else {
                continue;
            };
            if seen.insert(other) {
                out.push(other);
            }
        }
        out
    }

    /// The pull of one expanded node: every bucket when the drawer's `Expand`
    /// opened it, otherwise the double-click's narrower containment +
    /// calls/inherits set.
    fn expansion_neighbours(&self, index: usize) -> Vec<usize> {
        if self.expanded_all.contains(&index) {
            self.all_neighbours(index)
        } else {
            self.neighbours(index)
        }
    }

    /// Expand a node: its children and calling edges come onto the canvas.
    ///
    /// Returns the nodes that were newly added (empty when it was already
    /// expanded), which is what the tests assert on. Never panics on an
    /// out-of-range or childless node: a leaf simply expands to itself.
    pub fn expand_node(&mut self, index: usize) -> Vec<usize> {
        self.expand_with(index, false)
    }

    /// The node drawer's `Expand`: pull **every** relation bucket of `index`
    /// onto the canvas, as v2's `expandNode(id, {all:true})` does.
    ///
    /// A node already expanded by double-click is upgraded in place instead of
    /// being treated as a no-op: the gesture can only ever add nodes, never
    /// remove what the narrower pull placed. Returns the newly visible nodes.
    pub fn expand_node_all(&mut self, index: usize) -> Vec<usize> {
        self.expand_with(index, true)
    }

    fn expand_with(&mut self, index: usize, all: bool) -> Vec<usize> {
        if index >= self.graph.nodes.len() {
            return Vec::new();
        }
        let was_expanded = self.expanded.contains(&index);
        let was_all = self.expanded_all.contains(&index);
        if was_expanded && (!all || was_all) {
            return Vec::new();
        }
        self.expanded.insert(index);
        if all {
            self.expanded_all.insert(index);
        }
        let before: HashSet<usize> = self.slot_of.keys().copied().collect();
        self.rebuild_view();
        let added: Vec<usize> =
            self.vis.iter().map(|v| v.index).filter(|i| !before.contains(i)).collect();
        if !added.is_empty() {
            self.fit_pending = true;
        }
        added
    }

    /// Collapse a node: its pull is undone, and so is every pull beneath it.
    ///
    /// Pruning *descendants* is load-bearing, not tidiness. An expansion is only
    /// meaningful while the thing it expanded is on the canvas: if collapsing a
    /// namespace left its classes expanded, then re-expanding the namespace would
    /// silently resurrect those classes' members — a set that accumulated across
    /// a collapse/re-expand cycle instead of returning to where it started. It
    /// would also leave `expanded` naming nodes nothing can reach, so the next
    /// unrelated expansion could pull them back for no visible reason.
    ///
    /// Descendants are the containment subtree under `index`; walk it through
    /// `contains` edges before the set is touched.
    pub fn collapse_node(&mut self, index: usize) {
        if !self.expanded.remove(&index) {
            return;
        }
        self.expanded_all.remove(&index);
        for d in self.descendants(index) {
            self.expanded.remove(&d);
            self.expanded_all.remove(&d);
        }
        self.rebuild_view();
    }

    /// Every node reachable downward from `index` through `contains` edges.
    ///
    /// Iterative with a `seen` guard: a malformed or cyclic index must not hang
    /// the UI thread, which a recursive walk without a visited set would.
    fn descendants(&self, index: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut seen: HashSet<usize> = HashSet::from([index]);
        let mut stack = vec![index];
        while let Some(cur) = stack.pop() {
            for e in &self.graph.edges {
                if e.kind == "contains" && e.from == cur && seen.insert(e.to) {
                    out.push(e.to);
                    stack.push(e.to);
                }
            }
        }
        out
    }

    /// Double-click: expand a collapsed node, collapse an expanded one.
    ///
    /// The toggle is the fork's addition — the original only ever expands by
    /// double-click. Both gestures are still honest: the set it toggles is the
    /// set that decides what is drawn.
    pub fn toggle_node(&mut self, index: usize) {
        if self.expanded.contains(&index) {
            self.collapse_node(index);
        } else {
            self.expand_node(index);
        }
    }

    /// Is this node currently expanded open? (Read by the tests and by
    /// `actions.rs`'s disabled-state reasoning.)
    pub fn is_expanded(&self, index: usize) -> bool {
        self.expanded.contains(&index)
    }

    /// The current expanded set, for hosts that mirror it.
    pub fn expanded_set(&self) -> HashSet<usize> {
        self.expanded.clone()
    }

    // -----------------------------------------------------------------------
    // pinning
    // -----------------------------------------------------------------------

    /// Pin or unpin `index`. Returns the new state; `false` for an out-of-range
    /// index (nothing is pinned, and no state is invented for a node that does
    /// not exist).
    ///
    /// `shell.js:467` is the behaviour this reproduces: a re-settle moves the
    /// free nodes and leaves "pinned nodes stay pinned". So a pin has to mean
    /// two things at once — the chip shows its dot, and `relayout` passes the
    /// node to FA2 as `fixed`. Setting only the dot would be the same defect the
    /// brief describes (`NodeVis.fixed` painted but never set), one layer up.
    pub fn set_pinned(&mut self, index: usize, pinned: bool) -> bool {
        if index >= self.graph.nodes.len() {
            return false;
        }
        if pinned {
            self.pinned.insert(index);
        } else {
            self.pinned.remove(&index);
        }
        self.sync_fixed_flags();
        pinned
    }

    /// Toggle the pin on `index`. Returns the new state.
    pub fn toggle_pinned(&mut self, index: usize) -> bool {
        self.set_pinned(index, !self.pinned.contains(&index))
    }

    pub fn is_pinned(&self, index: usize) -> bool {
        self.pinned.contains(&index)
    }

    pub fn pinned_set(&self) -> HashSet<usize> {
        self.pinned.clone()
    }

    /// Push the pinned set onto `NodeVis.fixed`, which is what `paint_chip`
    /// reads for the dot and what `relayout` reads for the FA2 `fixed` vector.
    ///
    /// Derived from `pinned` rather than stored independently, so the paint flag
    /// and the layout flag can never disagree.
    fn sync_fixed_flags(&mut self) {
        for v in self.vis.iter_mut() {
            v.fixed = self.pinned.contains(&v.index);
        }
    }

    // -----------------------------------------------------------------------
    // box-selection (marquee)
    // -----------------------------------------------------------------------

    /// The node set the last completed marquee selected.
    pub fn marquee_selection(&self) -> &HashSet<usize> {
        &self.marquee_sel
    }

    /// Whether a marquee drag is currently in progress.
    pub fn marquee_active(&self) -> bool {
        self.marquee.is_some()
    }

    /// The live marquee rectangle in canvas-relative points, when one is open.
    pub fn marquee_rect(&self) -> Option<Rect> {
        self.marquee.map(|(a, b)| Rect::from_two_pos(a, b))
    }

    /// Every visible node whose chip intersects `rect` (canvas-relative points).
    ///
    /// Intersection rather than containment, so dragging across a cluster picks
    /// up the chips it touches instead of only the ones fully enclosed — which
    /// is what a marquee is for. Iterates `vis` (not a screen-space index) so the
    /// result is always a set of real, currently-visible nodes.
    fn nodes_in_rect(&self, rect: Rect) -> HashSet<usize> {
        let mut out = HashSet::new();
        for v in &self.vis {
            let (cx, cy) = self.world_to_screen(v.pos);
            let chip = Rect::from_center_size(
                Pos2::new(cx, cy),
                Vec2::new(v.size.0 * self.cam.zoom as f32, v.size.1 * self.cam.zoom as f32),
            );
            if rect.intersects(chip) {
                out.insert(v.index);
            }
        }
        out
    }

    /// Finish a marquee: commit the captured set as `marquee_sel`.
    ///
    /// `selected` (the single focus that Back/Up/Enter and the source pane use)
    /// is set to one representative of the set, not one entry per node — the
    /// history stack records *selection*, so pushing every captured node would
    /// turn one drag into N Back steps. That is the interaction the Lead warned
    /// about, and this is where it is avoided.
    fn finish_marquee(&mut self, rect: Rect) {
        let hit = self.nodes_in_rect(rect);
        self.marquee = None;
        self.marquee_sel = hit;
        if !self.marquee_sel.is_empty() {
            // Deterministic representative: lowest graph index, so the focus a
            // drag leaves behind does not depend on HashMap iteration order.
            if let Some(&min) = self.marquee_sel.iter().min() {
                self.select(Some(min));
            }
        }
    }

    // -----------------------------------------------------------------------
    // arranging / spacing (arrange.js:210-312)
    // -----------------------------------------------------------------------

    /// The marquee-selected node ids, lowest `Graph::nodes` index first.
    ///
    /// `arrange.js:48-53` `ids()` also falls back to the drawer's primary
    /// selection when nothing is box-selected; that fallback belongs to the
    /// app layer, so this is purely the marquee set. The sort is what keeps
    /// `HashSet` iteration order out of every result below.
    fn arrange_ids(&self) -> Vec<String> {
        let mut idx: Vec<usize> = self.marquee_sel.iter().copied().collect();
        idx.sort_unstable();
        idx.into_iter()
            .filter_map(|i| self.graph.nodes.get(i))
            .map(|n| n.id.clone())
            .collect()
    }

    /// The `file`/`line`/`label` declaration key of `arrange.js:98-102`
    /// (`declRank`), with an id the graph does not hold ranking last.
    fn decl_rank(&self, id: &str) -> (u8, String, i64, String) {
        match self.graph.nodes.iter().find(|n| n.id == id) {
            Some(n) => (0, n.file.clone(), n.line, n.label.clone()),
            None => (1, String::new(), 0, id.to_string()),
        }
    }

    /// `buildStepIndex()` (`arrange.js:114-155`): node id -> depth-first rank
    /// in the `contains` hierarchy (first container wins, siblings by line then
    /// declaration rank, depth 64 as the cycle guard).
    fn step_index(&self) -> HashMap<String, usize> {
        let n = self.graph.nodes.len();
        let mut parent_of: HashMap<usize, usize> = HashMap::new();
        let mut kids: HashMap<usize, Vec<(usize, i64)>> = HashMap::new();
        for e in &self.graph.edges {
            if e.kind != "contains" || e.from == e.to {
                continue;
            }
            if e.from >= n || e.to >= n {
                continue;
            }
            if parent_of.contains_key(&e.to) {
                continue;
            }
            parent_of.insert(e.to, e.from);
            kids.entry(e.from).or_default().push((e.to, self.graph.nodes[e.to].line));
        }
        for list in kids.values_mut() {
            list.sort_by(|a, b| {
                a.1.cmp(&b.1).then_with(|| {
                    self.decl_rank(&self.graph.nodes[a.0].id)
                        .cmp(&self.decl_rank(&self.graph.nodes[b.0].id))
                })
            });
        }
        fn walk(
            id: usize,
            depth: usize,
            kids: &HashMap<usize, Vec<(usize, i64)>>,
            visited: &mut HashSet<usize>,
            counter: &mut usize,
            order: &mut HashMap<usize, usize>,
        ) {
            if visited.contains(&id) || depth > 64 {
                return;
            }
            visited.insert(id);
            order.insert(id, *counter);
            *counter += 1;
            if let Some(list) = kids.get(&id) {
                for (k, _) in list {
                    walk(*k, depth + 1, kids, visited, counter, order);
                }
            }
        }
        let rank_of = |i: usize| self.decl_rank(&self.graph.nodes[i].id);
        let mut roots: Vec<usize> = (0..n).filter(|i| !parent_of.contains_key(i)).collect();
        roots.sort_by(|&a, &b| rank_of(a).cmp(&rank_of(b)));
        let mut visited = HashSet::new();
        let mut counter = 0usize;
        let mut order: HashMap<usize, usize> = HashMap::new();
        for r in roots {
            walk(r, 0, &kids, &mut visited, &mut counter, &mut order);
        }
        let mut rest: Vec<usize> = (0..n).filter(|i| !visited.contains(i)).collect();
        rest.sort_by(|&a, &b| rank_of(a).cmp(&rank_of(b)));
        for r in rest {
            walk(r, 0, &kids, &mut visited, &mut counter, &mut order);
        }
        order
            .into_iter()
            .map(|(i, r)| (self.graph.nodes[i].id.clone(), r))
            .collect()
    }

    /// `sortByStep(list)` (`arrange.js:157-165`): `ids` in depth-first step
    /// order, ids missing from the step index last.
    fn sort_ids_by_step(&self, ids: &[String]) -> Vec<String> {
        let step = self.step_index();
        let mut list: Vec<&String> = ids.iter().collect();
        list.sort_by(|a, b| {
            let oa = step.get(a.as_str()).copied().unwrap_or(usize::MAX);
            let ob = step.get(b.as_str()).copied().unwrap_or(usize::MAX);
            oa.cmp(&ob)
                .then_with(|| self.decl_rank(a).cmp(&self.decl_rank(b)))
        });
        list.into_iter().cloned().collect()
    }

    /// How many nodes the canvas is actually drawing (`vis.len()`).
    ///
    /// This is v2's `viewNodes` (`shell.js updateCanvasNotice`): the count after
    /// filters, collapse/expand and drawer scope — never the graph total. The
    /// nzk_ui toast renders this number, so it is a public read of the same
    /// vector the painter iterates.
    pub fn visible_count(&self) -> usize {
        self.vis.len()
    }

    /// Every visible node's settled world position, in slot order.
    ///
    /// A read-only window on the same vector the painter iterates, for callers
    /// that must assert a layout really moved the arrangement (the Re-settle /
    /// Full-graph consequence tests). Never a second owner of the positions.
    pub fn visible_positions(&self) -> Vec<(f64, f64)> {
        self.vis.iter().map(|v| v.pos).collect()
    }

    /// Whether every visible node holds a settled position (no `unplaced` slot).
    ///
    /// `rebuild_view` mints new slots unplaced; the next `relayout` places them.
    /// Reported so a caller can assert "the layout actually placed the set"
    /// instead of reading a counter.
    pub fn visible_all_placed(&self) -> bool {
        self.vis.iter().all(|v| !v.unplaced)
    }

    /// The visible slot, world position and chip size for `id`.
    fn slot_pos_size(&self, id: &str) -> Option<(usize, (f64, f64), (f64, f64))> {
        self.vis
            .iter()
            .enumerate()
            .find(|(_, v)| self.graph.nodes[v.index].id == id)
            .map(|(slot, v)| {
                (
                    slot,
                    v.pos,
                    (f64::from(v.size.0), f64::from(v.size.1)),
                )
            })
    }

    /// `orderAlong(list, axis)` (`arrange.js:255-271`) as the rank map the
    /// frozen `order_key` closure needs: rank 0 is the id sitting first along
    /// the axis. Ties keep the incoming order, `d` uses `x + y`.
    fn along_axis_ranks(&self, ids: &[String], axis: crate::chords::Axis) -> HashMap<String, usize> {
        let mut keyed: Vec<(f64, f64, &String)> = ids
            .iter()
            .map(|id| {
                let (x, y) = self.slot_pos_size(id).map(|(_, p, _)| p).unwrap_or((0.0, 0.0));
                let (k1, k2) = match axis {
                    crate::chords::Axis::Horizontal => (x, y),
                    crate::chords::Axis::Diagonal => (x + y, 0.0),
                    crate::chords::Axis::Vertical => (y, x),
                };
                (k1, k2, id)
            })
            .collect();
        keyed.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        });
        keyed
            .into_iter()
            .enumerate()
            .map(|(rank, (_, _, id))| (id.clone(), rank))
            .collect()
    }

    /// `detectAxis(list)` (`arrange.js:187-203`): edge alignment, not bounding
    /// box — a column has zero spread in the chips' left edges, a row in their
    /// tops. `None` when both spreads are under 0.5 (`:196`).
    fn detect_axis(&self, ids: &[String]) -> Option<crate::chords::Axis> {
        let mut min_left = f64::INFINITY;
        let mut max_left = f64::NEG_INFINITY;
        let mut min_top = f64::INFINITY;
        let mut max_top = f64::NEG_INFINITY;
        let mut any = false;
        for id in ids {
            if let Some((_, p, sz)) = self.slot_pos_size(id) {
                any = true;
                let left = p.0 - sz.0 / 2.0;
                let top = p.1 - sz.1 / 2.0;
                min_left = min_left.min(left);
                max_left = max_left.max(left);
                min_top = min_top.min(top);
                max_top = max_top.max(top);
            }
        }
        if !any {
            return None;
        }
        let dx = max_left - min_left;
        let dy = max_top - min_top;
        if dx < 0.5 && dy < 0.5 {
            return None;
        }
        if dx < 0.5 {
            return Some(crate::chords::Axis::Vertical);
        }
        if dy < 0.5 {
            return Some(crate::chords::Axis::Horizontal);
        }
        let ratio = dx / dy;
        if ratio > 2.2 {
            Some(crate::chords::Axis::Horizontal)
        } else if ratio < 0.45 {
            Some(crate::chords::Axis::Vertical)
        } else {
            Some(crate::chords::Axis::Diagonal)
        }
    }

    /// `place(order, axis, gap)` (`arrange.js:210-250`): write each id's new
    /// world position into `vis`, anchored at the selection's current top-left,
    /// with the left edges flush on a column and the top edges flush on a row.
    fn place_order(&mut self, order: &[String], axis: crate::chords::Axis, gap: f64) {
        if order.is_empty() {
            return;
        }
        let mut slots: Vec<usize> = Vec::with_capacity(order.len());
        let mut sizes: Vec<(f64, f64)> = Vec::with_capacity(order.len());
        let mut left = f64::INFINITY;
        let mut top = f64::INFINITY;
        for id in order {
            if let Some((slot, p, sz)) = self.slot_pos_size(id) {
                slots.push(slot);
                sizes.push(sz);
                left = left.min(p.0 - sz.0 / 2.0);
                top = top.min(p.1 - sz.1 / 2.0);
            }
        }
        if slots.is_empty() {
            return;
        }
        let max_side = sizes.iter().fold(0.0_f64, |m, &(w, h)| m.max(w).max(h));
        let diag_step = (gap + max_side) * std::f64::consts::SQRT_2 / 2.0;
        let mut run = 0.0_f64;
        let mut placed: Vec<(f64, f64)> = Vec::with_capacity(slots.len());
        for (i, &(w, h)) in sizes.iter().enumerate() {
            let (x, y) = match axis {
                crate::chords::Axis::Horizontal => {
                    if i > 0 {
                        run += (sizes[i - 1].0 + w) / 2.0 + gap;
                    }
                    (left + w / 2.0 + run, top + h / 2.0)
                }
                crate::chords::Axis::Diagonal => (
                    left + w / 2.0 + i as f64 * diag_step,
                    top + h / 2.0 + i as f64 * diag_step,
                ),
                crate::chords::Axis::Vertical => {
                    if i > 0 {
                        run += (sizes[i - 1].1 + h) / 2.0 + gap;
                    }
                    (left + w / 2.0, top + h / 2.0 + run)
                }
            };
            placed.push((x, y));
        }
        for (k, &slot) in slots.iter().enumerate() {
            let v = &mut self.vis[slot];
            v.pos = placed[k];
            // `p.fixed = true` (`arrange.js:235`): arranged nodes stay put.
            v.fixed = true;
            // `p.seeded = true` (`arrange.js:236`): the layout has placed it.
            v.unplaced = false;
        }
    }

    /// `Ctrl/Cmd+Shift+V | H | D` (`arrange.js:289-299`): lay the marquee
    /// selection out along `axis` in step order, flipping it when the selection
    /// already runs in step order along that axis.
    ///
    /// `None` is the reference's toast-and-return (`:291`), which the app layer
    /// turns into the toast; no position is touched in that case.
    pub fn arrange_selection(&mut self, axis: crate::chords::Axis) -> Option<Vec<String>> {
        let ids = self.arrange_ids();
        if ids.len() < 2 {
            return None;
        }
        let step = self.sort_ids_by_step(&ids);
        let ranks = self.along_axis_ranks(&ids, axis);
        let key = |id: &str| ranks.get(id).copied();
        let gap = self.arrangement.gap();
        let order = self.arrangement.arrange(&step, axis, &key)?;
        self.place_order(&order, axis, gap);
        Some(order)
    }

    /// `Shift+= / Shift+-` (`arrange.js:302-312`): widen (`dir > 0`) or tighten
    /// the spacing along the axis the selection already follows, then re-place
    /// it. `gap` persists on the view, so a second press widens from the wider
    /// value.
    ///
    /// `None` is the reference's toast-and-return: fewer than two selected
    /// (`:304`), or no detectable axis (`:305-306`), in which case `gap` is
    /// left untouched.
    pub fn space_selection(&mut self, dir: i32) -> Option<Vec<String>> {
        let ids = self.arrange_ids();
        if ids.len() < 2 {
            return None;
        }
        let axis = match self.detect_axis(&ids) {
            Some(a) => a,
            // `arrange.js:305-306` returns without touching `gap`; a keyless
            // ordering makes the model layer perform exactly that guard.
            None => return self.arrangement.spacing(&ids, dir, &|_| None),
        };
        let ranks = self.along_axis_ranks(&ids, axis);
        let key = |id: &str| ranks.get(id).copied();
        let order = self.arrangement.spacing(&ids, dir, &key)?;
        let gap = self.arrangement.gap();
        self.place_order(&order, axis, gap);
        Some(order)
    }

    /// Select a node, recording the previous selection for Back.
    ///
    /// `graph.js selectNode`: push the outgoing selection (unless it is the same
    /// node), cap the stack at 80. Selection itself is `GraphView::selected`, as
    /// before; this only adds the history the Back control needs.
    pub fn select(&mut self, index: Option<usize>) {
        if self.selected != index {
            if let Some(prev) = self.selected {
                if self.history.len() >= 80 {
                    self.history.remove(0);
                }
                self.history.push(prev);
            }
            // A fresh selection is a new branch: the Forward stack belongs to
            // the branch just abandoned, so it must not be reachable any more.
            self.forward.clear();
        }
        self.selected = index;
    }

    /// The single writer for [`Self::theme`] / [`Self::palette`] —
    /// `#uiTheme`'s whole-viewer palette (`core.js UI_THEMES`,
    /// `applyUiTheme` in `source.js:199-205`). Returns whether it changed.
    ///
    /// `nzk-dark+` is the shipped default; an unknown value is not a theme and
    /// leaves the current one alone (the same refuse-don't-guess rule the
    /// sidebar selects use).
    pub fn set_theme(&mut self, name: &str) -> bool {
        let next = match name {
            "nzk-dark+" | "nzk-darkplus" | "NzkDarkPlus" => UiTheme::NzkDarkPlus,
            "dark-v1" | "DarkV1" => UiTheme::DarkV1,
            _ => return false,
        };
        if self.theme == next {
            return false;
        }
        self.theme = next;
        self.palette = Palette::of(next);
        true
    }

    /// Set `state.filters.groupChildren` — the **single writer** for the flag.
    ///
    /// Both reference checkboxes route through here so they cannot disagree:
    /// `#groupChildren` shows the flag, `#hideChildren` shows its inverse, and
    /// `shell.js:604`/`:647` (plus `graph.js:688-689`) re-mirror the other box on
    /// every change. Keeping one setter means the inversion is applied in exactly
    /// one place rather than at each call site, where a missed `!` would leave
    /// the two boxes simultaneously "correct" and contradictory.
    ///
    /// Returns whether the value actually changed, so a caller can skip a
    /// repaint — and so a test can distinguish "clicked the already-set state"
    /// from "changed the filter".
    ///
    /// Rebuilds the visible edge set, because the flag gates the drawn
    /// `contains` edges ([`Self::rebuild_view`] is the same rebuild every other
    /// view filter goes through).
    pub fn set_group_children(&mut self, on: bool) -> bool {
        if self.group_children == on {
            return false;
        }
        self.group_children = on;
        self.rebuild_view();
        true
    }

    /// The single writer for [`Self::group_edges`] — `#groupEdges`
    /// (`shell.js:609-612`: `state.filters.groupEdges = groupEdgesEl.checked`).
    ///
    /// The exact shape of [`Self::set_group_children`], deliberately: one
    /// boolean field, one setter, a `true` return only on a real transition so a
    /// repaint can distinguish "clicked the already-set state" from "changed the
    /// filter", and a [`Self::rebuild_view`] because the flag gates which drawn
    /// edges survive (`graph.js:814`).
    ///
    /// Unlike `group_children` there is no second, inverse checkbox mirroring
    /// this flag — `#groupEdges` is the only box that shows it, so there is no
    /// inversion to keep consistent here.
    pub fn set_group_edges(&mut self, on: bool) -> bool {
        if self.group_edges == on {
            return false;
        }
        self.group_edges = on;
        self.rebuild_view();
        true
    }

    /// The render predicate for ONE node: drawn iff its kind's chip is ON and it
    /// belongs to the selected namespace (or a sub-namespace when `#nsDeep` is
    /// checked). An empty model passes everything — the shipped render.
    pub fn filter_passes(&self, node: &crate::model::Node) -> bool {
        if self.hidden_kinds.contains(&node.kind) {
            return false;
        }
        // Project chips (`filters.js` `state.filters.projects`): a node owned by
        // a hidden project is hidden. Ownership comes from the graph's real
        // project model (`Graph::projects`), never from a second mapping copy.
        if !self.filter_projects.is_empty()
            && self.graph.projects.iter().any(|p| {
                self.filter_projects.contains(&p.id) && Self::project_owns_node(p, &node.file)
            })
        {
            return false;
        }
        if self.filter_namespace.is_empty() {
            return true;
        }
        node.ns == self.filter_namespace
            || (self.filter_ns_deep
                && node
                    .ns
                    .strip_prefix(&self.filter_namespace)
                    .map(|rest| rest.starts_with('.'))
                    .unwrap_or(false))
    }

    /// The single writer for the sidebar filter model (`filters.js`).
    ///
    /// Same contract as [`Self::set_group_children`]: one owner, `true` only on a
    /// real transition so the host can skip the rebuild, and a
    /// [`Self::rebuild_view`] because the model gates which nodes are on the
    /// canvas and which edge kinds are drawn. A chip that changed a class but not
    /// this predicate would be a dead control, so the two are always updated
    /// together (the host pushes the model here on every dispatched action).
    pub fn set_filters(&mut self, filters: &crate::actions::FilterState) -> bool {
        if self.hidden_kinds == filters.hidden_kinds
            && self.hidden_edges == filters.hidden_edges
            && self.filter_projects == filters.hidden_projects
            && self.filter_namespace == filters.namespace
            && self.filter_ns_deep == filters.ns_deep
            && self.hide_isolated == filters.hide_isolated
            && self.vt_mode == filters.vt_mode
            && self.focus_only == filters.focus_only
            && self.focus_parents == filters.focus_parents
        {
            return false;
        }
        self.hidden_kinds = filters.hidden_kinds.clone();
        self.hidden_edges = filters.hidden_edges.clone();
        self.filter_projects = filters.hidden_projects.clone();
        self.filter_namespace = filters.namespace.clone();
        self.filter_ns_deep = filters.ns_deep;
        self.hide_isolated = filters.hide_isolated;
        self.vt_mode = filters.vt_mode;
        self.focus_only = filters.focus_only;
        self.focus_parents = filters.focus_parents;
        self.rebuild_view();
        true
    }

    /// Rebuild when the selection moved while a focus filter is on.
    ///
    /// The sidebar/drawer focus flags anchor on the *selected* node, and a
    /// selection can change through paths that never touch `set_filters` (a
    /// graph click, a relation row, a find row). Comparing the anchor here is
    /// what makes "Only related nodes" follow the selection instead of freezing on
    /// whatever happened to be selected when the box was ticked. Returns `true`
    /// only on a real transition, so the per-frame caller pays nothing while the
    /// selection is steady.
    pub fn sync_focus_selection(&mut self) -> bool {
        if !(self.focus_only || self.focus_parents) {
            return false;
        }
        if self.focus_selected == self.selected {
            return false;
        }
        self.rebuild_view();
        true
    }

    /// Mirror the drawer's `Hide children` scope into the canvas predicate.
    ///
    /// `DrawerUi::hidden_children` is the ONE owner (v2 `state.hiddenChildren`);
    /// this is the view's copy. Returns `true` only on a real transition — the
    /// same contract as [`Self::set_filters`] — so the host can call it on every
    /// frame and pay a rebuild only when the scope actually moved.
    pub fn sync_hidden_children(
        &mut self,
        ids: &std::collections::BTreeSet<String>,
    ) -> bool {
        if self.hidden_children == *ids {
            return false;
        }
        self.hidden_children = ids.clone();
        self.rebuild_view();
        true
    }

    /// The graph indices hidden because a `Hide children` parent owns them.
    ///
    /// The real data models containment as a `contains` edge, which is what the
    /// drawer itself lists rows from, so that edge is the primary relation.
    /// `Node::owner` is a fallback for corpora whose owner strings are populated
    /// without a containment edge; including it keeps the canvas and the drawer
    /// in agreement even on a partial graph rather than silently disagreeing.
    fn hidden_child_indices(&self) -> HashSet<usize> {
        if self.hidden_children.is_empty() {
            return HashSet::new();
        }
        let mut out = HashSet::new();
        for e in &self.graph.edges {
            if e.kind != "contains" {
                continue;
            }
            let Some(parent) = self.graph.nodes.get(e.from) else {
                continue;
            };
            if self.hidden_children.contains(&parent.id) {
                out.insert(e.to);
            }
        }
        for (i, n) in self.graph.nodes.iter().enumerate() {
            if !n.owner.is_empty() && self.hidden_children.contains(&n.owner) {
                out.insert(i);
            }
        }
        out
    }

    /// `#pruneHidden`: delete the nodes the filters (and the drawer scope) hide.
    ///
    /// v2 `pruneHidden` removed the hidden nodes from its incrementally built
    /// map, so the canvas kept only what was on screen and a later filter change
    /// could not bring them back without a reload. This is the same model
    /// transition over the resident graph: keep the passing nodes, drop every
    /// edge that touched a dropped node, and remap every index-valued model
    /// (`selected`, history/forward, expanded, pinned, the marquee set and the
    /// settled-key continuity set) through node ids so no index can dangle.
    ///
    /// The filter model itself is NOT reset — the chips still say what is
    /// hidden, exactly like v2. Returns the number of nodes removed; `0` means
    /// nothing was hidden, which is an honest no-op rather than a fake delete.
    pub fn prune_hidden(&mut self) -> usize {
        let hidden_by_scope = self.hidden_child_indices();
        // The focus filters are deliberately NOT part of "Remove hidden": they
        // are a transient way to read one node, not a statement that the rest
        // of the model should be deleted. Applying them here would let a single
        // press of `Remove hidden` erase every node but the selection.
        let keep: Vec<usize> = (0..self.graph.nodes.len())
            .filter(|&i| {
                !hidden_by_scope.contains(&i) && self.filter_passes(&self.graph.nodes[i])
            })
            .collect();
        let removed = self.graph.nodes.len().saturating_sub(keep.len());
        if removed == 0 {
            return 0;
        }
        // Preserve every index-valued model by id BEFORE the indices move.
        // Positions go to the id-keyed store for the same reason; recording them
        // after the remap would attach them to the wrong node.
        self.remember_visible_positions();
        let id_at = |i: usize| self.graph.nodes.get(i).map(|n| n.id.clone());
        let selected_id = self.selected.and_then(id_at);
        let hovered_id = self.hovered.and_then(id_at);
        let ids_of = |set: &HashSet<usize>| -> Vec<String> {
            set.iter().filter_map(|&i| id_at(i)).collect()
        };
        let expanded_ids = ids_of(&self.expanded);
        let expanded_all_ids = ids_of(&self.expanded_all);
        let pinned_ids = ids_of(&self.pinned);
        let marquee_ids = ids_of(&self.marquee_sel);
        let settled_ids = ids_of(&self.settled_keys);
        let history_ids: Vec<String> = self.history.iter().filter_map(|&i| id_at(i)).collect();
        let forward_ids: Vec<String> = self.forward.iter().filter_map(|&i| id_at(i)).collect();

        let mut remap = vec![usize::MAX; self.graph.nodes.len()];
        for (new_i, &old_i) in keep.iter().enumerate() {
            remap[old_i] = new_i;
        }
        let mut edges = Vec::with_capacity(self.graph.edges.len());
        for e in &self.graph.edges {
            let (from, to) = (remap[e.from], remap[e.to]);
            if from != usize::MAX && to != usize::MAX {
                edges.push(crate::model::Edge {
                    from,
                    to,
                    kind: e.kind.clone(),
                    count: e.count,
                });
            }
        }
        let nodes: Vec<crate::model::Node> =
            keep.iter().map(|&i| self.graph.nodes[i].clone()).collect();
        self.graph.nodes = nodes;
        self.graph.edges = edges;

        fn pos_of(graph: &crate::model::Graph, id: &str) -> Option<usize> {
            graph.nodes.iter().position(|n| n.id == id)
        }
        self.expanded = expanded_ids.iter().filter_map(|id| pos_of(&self.graph, id)).collect();
        self.expanded_all =
            expanded_all_ids.iter().filter_map(|id| pos_of(&self.graph, id)).collect();
        self.pinned = pinned_ids.iter().filter_map(|id| pos_of(&self.graph, id)).collect();
        self.marquee_sel = marquee_ids.iter().filter_map(|id| pos_of(&self.graph, id)).collect();
        self.settled_keys = settled_ids.iter().filter_map(|id| pos_of(&self.graph, id)).collect();
        self.history = history_ids.iter().filter_map(|id| pos_of(&self.graph, id)).collect();
        self.forward = forward_ids.iter().filter_map(|id| pos_of(&self.graph, id)).collect();
        self.selected = selected_id.as_deref().and_then(|id| pos_of(&self.graph, id));
        self.hovered = hovered_id.as_deref().and_then(|id| pos_of(&self.graph, id));

        // Node indices changed, so the freshly minted `vis` must not re-use the
        // old slot list. Positions are safe: the store is keyed by node id, so
        // every survivor keeps where the user last saw it.
        self.vis.clear();
        self.slot_of.clear();
        self.edges.clear();
        self.rebuild_view();
        removed
    }

    /// Does `file` belong to the project? v2 maps a node to projects through the
/// engine's exact source-file set (`filters.js:52-56` projectIdsFor). This fork
/// carries the project `path` + `targets` instead, so the match is a path
/// prefix (project directory) — an honest approximation, named in PARITY.
fn project_owns_node(project: &crate::model::ProjectRow, file: &str) -> bool {
    if file.is_empty() {
        return false;
    }
    let dir = project.path.trim_end_matches(['/', '\\']);
    if !dir.is_empty() && (file == dir || file.starts_with(&format!("{dir}/"))) {
        return true;
    }
    project
        .targets
        .iter()
        .any(|t| !t.is_empty() && (file.starts_with(t) || file.contains(t.as_str())))
}

/// Back: return to the previously selected node and re-expand it.
    ///
    /// `interaction.js navigateBack`. The entries that are no longer visible are
    /// skipped rather than remembered as dead state.
    ///
    /// The node this call leaves is recorded on the Forward stack, so Forward
    /// can retrace the step exactly.
    ///
    /// Returns the node gone back to, or `None` when there was nothing to go
    /// back to — it does not invent a destination, it does not touch the
    /// selection when the history is empty, and it does not push a Forward
    /// destination it never actually left.
    pub fn navigate_back(&mut self) -> Option<usize> {
        let id = loop {
            let cand = self.history.pop()?;
            if self.slot_of.contains_key(&cand) {
                break cand;
            }
        };
        if let Some(prev) = self.selected {
            if self.forward.len() >= 80 {
                self.forward.remove(0);
            }
            self.forward.push(prev);
        }
        self.expand_node(id);
        self.selected = Some(id);
        self.fit_pending = true;
        Some(id)
    }

    /// Forward: retrace a step taken by [`Self::navigate_back`].
    ///
    /// The mirror of [`Self::navigate_back`], and the pane-stack `forward()`
    /// behaviour of the reference: pop the Forward stack, re-expand and select
    /// that node, and record the node being left on the Back stack so Back and
    /// Forward can be alternated. Invisible entries are skipped, exactly as
    /// Back skips them.
    ///
    /// Returns the node gone forward to, or `None` when there is nothing to go
    /// forward to (a new selection clears the stack, so Forward never
    /// resurrects a stale branch).
    pub fn navigate_forward(&mut self) -> Option<usize> {
        let id = loop {
            let cand = self.forward.pop()?;
            if self.slot_of.contains_key(&cand) {
                break cand;
            }
        };
        if let Some(prev) = self.selected {
            if self.history.len() >= 80 {
                self.history.remove(0);
            }
            self.history.push(prev);
        }
        self.expand_node(id);
        self.selected = Some(id);
        self.fit_pending = true;
        Some(id)
    }

    /// Up: walk to the containment parent (namespace / declaring type).
    ///
    /// `interaction.js navigateUp`. The parent is the `contains` edge pointing
    /// *at* the selected node. This is real index data, not a depth counter, so
    /// it can be honest about having nothing to say: a namespace, or a node whose
    /// parent was dropped from this view, returns `None` rather than a guess.
    pub fn navigate_up(&mut self) -> Option<usize> {
        let cur = self.selected?;
        let parent = self
            .graph
            .edges
            .iter()
            .find(|e| e.to == cur && e.kind == "contains")
            .map(|e| e.from)?;
        self.expand_node(parent);
        self.selected = Some(parent);
        self.fit_pending = true;
        Some(parent)
    }

    /// The containment parent of a node, whether or not it is currently visible.
    pub fn parent_of(&self, index: usize) -> Option<usize> {
        self.graph
            .edges
            .iter()
            .find(|e| e.to == index && e.kind == "contains")
            .map(|e| e.from)
    }

    /// Every containment ancestor of `index`, nearest first, bounded so a
    /// malformed/cyclic corpus cannot hang the view.
    ///
    /// Uses the same directed `contains` edge as [`Self::parent_of`], so
    /// "Hide parents" removes exactly the chain the drawer's `Parent` bucket and
    /// the breadcrumb walk report — never an incoming `calls`/`uses` neighbour.
    fn containment_ancestors(&self, index: usize) -> HashSet<usize> {
        let mut out = HashSet::new();
        let mut cur = index;
        for _ in 0..64 {
            let Some(parent) = self.parent_of(cur) else { break };
            if !out.insert(parent) {
                break; // cycle: already visited
            }
            cur = parent;
        }
        out
    }

    /// Whether a layout run would be a guaranteed no-op.
    ///
    /// `rebuild_view` marks the layout dirty for *every* view change — but an
    /// edge-kind chip, a label toggle or `#hideRelations` only changes which
    /// edges are drawn. The node set is then still exactly the one the last run
    /// settled, every node is placed, and the glyph metrics are unchanged. A run
    /// would re-measure every label and then hand an all-anchored set to a solver
    /// that cannot move one of them.
    ///
    /// False also when a glyph metric moved (`node_size`/`text_size`), because
    /// the run owns updating every chip's box even when no position changes.
    fn layout_is_noop(&self) -> bool {
        self.seed_override.is_none()
            && self.vis.iter().all(|v| !v.unplaced)
            && self.settled_keys.len() == self.vis.len()
            && self.settled_keys.iter().all(|k| self.slot_of.contains_key(k))
            && self.metrics.dot == self.settings.node_size
            && self.metrics.font_size == self.settings.text_size
    }

    /// Re-lay-out, measuring labels with `ctx` so chip sizes match what will
    /// actually be painted. Port of `layout.js startLayout` (seed -> FA2 ->
    /// Noverlap) plus `graph.js seedExpansion`/`seedFan` for the nodes that are
    /// new to this run.
    ///
    /// # The continue-vs-re-seed rule (the point of this method)
    ///
    /// * **A run CONTINUES from the positions on screen whenever the visible set
    ///   did not lose nodes** — the node set merely grew (a double-click
    ///   expansion, `Enter`, a list/Back/Up expansion), or did not change at all
    ///   (a pin toggle, a `fit`, a relayout after a settings change). Every node
    ///   that already had a position keeps it and is anchored ([`anchored`]), so
    ///   only the nodes that are new to the canvas move. This is `graph.js
    ///   expandNode`'s path: it places the new ids and leaves the placed ones
    ///   alone (`graph.js:580`, `seedExpansion`/`seedFan` + the quick layout).
    /// * **A run RE-SEEDS from [`graphviz::seed_positions`] when the visible set
    ///   lost nodes** (a collapse, or a rebuild that replaced the graph) **or
    ///   when there is nothing placed to continue from** (the first layout).
    ///   `rebuild_view` resets every position when it mints the `vis` vector, so
    ///   "the set lost nodes" and "every position is zero" are the same signal
    ///   from here — and it is the signal the reference's own gesture map
    ///   implies: collapsing a node is the one gesture after which the visible
    ///   arrangement genuinely no longer exists.
    ///
    /// The reference settles the same two cases the same way, with one deviation
    /// stated here rather than left implicit: its layout never re-seeds on its
    /// own (`layout.js:280` is behind `reinitialize`, and every call site passes
    /// `false`), and its continue branch hands FA2 every node except the pinned
    /// ones. `R` — "re-settle (pinned nodes stay pinned)", `shell.js:467` —
    /// therefore re-runs the solver and visibly moves the free nodes. This
    /// port's continue branch holds every inherited node still so an expansion
    /// cannot scatter the arrangement ([`anchored`]); there is no FA2 pass to run,
    /// so the implementation applies the child seed and then a Noverlap-only
    /// safety phase (a restored node can land under a chip that moved while it was
    /// hidden). If even one arriving node has no placed root to be seeded beside,
    /// the "continue" premise is false and the whole set takes the re-seed branch
    /// instead — that is what keeps a filter being turned back off from piling
    /// every returning chip on the origin.
    ///
    /// Over an unchanged set nothing is new and nothing
    /// is free, so a plain continue `R` is a guaranteed no-op ("Re-settle does
    /// nothing"). `R` therefore sets `reseed_layout` (`actions.rs`), which is
    /// the fork's honest equivalent of "re-run the layout from scratch";
    /// [`Self::reseed_layout`] remains the hook for that branch.
    ///
    /// [`anchored`]: graphviz::LayoutInput::anchored
    pub fn relayout(&mut self, ctx: &egui::Context) {
        self.layout_dirty = false;
        if self.vis.is_empty() {
            return;
        }
        // EARLY EXIT: nothing this run could change, and no chip box it must
        // update. See `layout_is_noop`.
        if self.layout_is_noop() {
            return;
        }
        // Retune rather than rebuild: an unchanged font/dot keeps the measured
        // boxes, so an expansion only measures the labels it actually added.
        self.metrics.retune(self.settings.node_size, self.settings.text_size);
        let mut measure = |text: &str| -> f32 {
            ctx.fonts_mut(|f| {
                f.layout_no_wrap(
                    text.to_owned(),
                    crate::typography::canvas_mono(self.settings.text_size),
                    Color32::WHITE,
                )
                .size()
                .x
            })
        };

        let n = self.vis.len();
        let mut sizes: Vec<(f32, f32)> = Vec::with_capacity(n);
        let mut labels: Vec<String> = Vec::with_capacity(n);
        for v in &self.vis {
            let nd = &self.graph.nodes[v.index];
            let label = if nd.kind == "namespace" && !nd.full.is_empty() {
                nd.full.clone()
            } else {
                nd.label.clone()
            };
            sizes.push(self.metrics.size(&label, &mut measure));
            labels.push(label);
        }

        // --- the seed: continue, or re-seed? -----------------------------
        //
        // Two facts decide it, and both come from the data rather than from a
        // flag a caller has to remember to set:
        //
        // * `placed` — where everything is. `rebuild_view` carries a placed
        //   node's position across, so a slot that is still `unplaced` is a node
        //   this run has never laid out, i.e. one the gesture just added.
        // * `lost` — whether the visible set dropped a node the last run
        //   finished with. `settled_keys` is that set.
        //
        // CONTINUE when nothing was lost (the set grew, or is unchanged);
        // RE-SEED when something was lost (a collapse, or a rebuild that swapped
        // the graph). Growth must never re-seed: expanding a lone namespace into
        // its two classes leaves one node in three placed, and a
        // "how much survived" ratio would call that a fresh start and
        // re-scramble the canvas — the defect this method exists to remove.
        let placed: Vec<Option<(f64, f64)>> = self
            .vis
            .iter()
            .map(|v| if v.unplaced { None } else { Some(v.pos) })
            .collect();
        let keys_now: HashSet<usize> = self.vis.iter().map(|v| v.index).collect();
        let lost = self.settled_keys.iter().any(|k| !keys_now.contains(k));
        let continuity = if self.settled_keys.is_empty() {
            // First layout of this view: nothing has been settled yet.
            graphviz::seed_continuity(&placed)
        } else if lost {
            SeedContinuity::Reseed
        } else {
            SeedContinuity::Continue
        };
        let mut seeding = self.seed_override.take().unwrap_or(continuity);
        let mut seed = match seeding {
            SeedContinuity::Continue => graphviz::seed_incremental(&placed, n),
            SeedContinuity::Reseed => graphviz::seed_positions(n),
        };

        // Child placement (`graph.js seedExpansion`/`seedFan`): hand the nodes
        // that are new to this canvas a position beside the node that revealed
        // them, instead of leaving them at the origin for the force pass to
        // resolve. Done before the run starts, so the solver has a sensible
        // starting point and does not have to push them out of a pile.
        let mut child_seed = match seeding {
            SeedContinuity::Continue => {
                self.seed_new_children(&placed, &sizes, &seed, self.settings.spacing)
            }
            SeedContinuity::Reseed => None,
        };

        // A CONTINUE run assumes every arriving node can be hung off a placed
        // root. That assumption is false whenever the set comes back from outside
        // the canvas rather than by an expansion: turning "Only related nodes" (or a
        // kind/namespace chip) off restores hundreds of nodes that have no placed
        // neighbour, and `seed_new_children` can only place nodes beside a placed
        // root. Leaving those slots at (0, 0) is the pile where every chip is
        // drawn on top of every other. If even one new node has no placed parent,
        // the arrangement this run was going to continue from no longer exists —
        // so take the honest re-seed branch over the whole set instead.
        if seeding == SeedContinuity::Continue {
            let mut placed_by_seed = vec![false; n];
            for &(slot, _, _) in child_seed.as_deref().unwrap_or(&[]) {
                placed_by_seed[slot] = true;
            }
            let every_new_node_placed =
                (0..n).all(|s| placed[s].is_some() || placed_by_seed[s]);
            if !every_new_node_placed {
                seeding = SeedContinuity::Reseed;
                seed = graphviz::seed_positions(n);
                child_seed = None;
            }
        }

        for (slot, p, _kind) in child_seed.unwrap_or_default() {
            seed[slot] = p;
        }

        // A CONTINUE run has nothing for FA2 to do: the child seed is the whole
        // placement, and forcing every inherited node would only scramble the
        // arrangement. It still runs the cheap Noverlap phase, because a node
        // restored from the position store can come back under a chip that moved
        // while it was hidden. Over a clear layout that phase reports zero
        // displacements on its first pass and costs one grid build; over a
        // collision it separates exactly the pair, so no chip is ever left fully
        // covering another node's box (the pile the user sees when a filter is
        // turned back off).
        let out = if seeding == SeedContinuity::Continue {
            let fixed: Vec<bool> =
                self.vis.iter().map(|v| self.pinned.contains(&v.index)).collect();
            let hw: Vec<f64> = sizes.iter().map(|s| s.0 as f64 / 2.0).collect();
            let hh: Vec<f64> = sizes.iter().map(|s| s.1 as f64 / 2.0).collect();
            let ids: Vec<String> =
                self.vis.iter().map(|v| self.graph.nodes[v.index].id.clone()).collect();
            // Hold exactly the nodes the last run settled. They are the ones the
            // user is already looking at, and the continue rule promises they do
            // not move. Restored/arriving nodes are NOT in `settled_keys`, so
            // Noverlap may shift them off a chip they would otherwise cover.
            let anchored: Vec<bool> =
                self.vis.iter().map(|v| self.settled_keys.contains(&v.index)).collect();
            graphviz::noverlap_only(
                &fixed,
                &anchored,
                &hw,
                &hh,
                &ids,
                &seed,
                self.settings,
            )
        } else {
            // Unique undirected pairs (`graph.js buildModel` -> `state.pairs`).
            let mut pairs: Vec<(usize, usize)> = Vec::new();
            let mut seen: HashSet<(usize, usize)> = HashSet::new();
            let mut degree = vec![0usize; n];
            for &(a, b, _) in &self.edges {
                let key = if a < b { (a, b) } else { (b, a) };
                if seen.insert(key) {
                    pairs.push(key);
                }
                degree[a] += 1;
                degree[b] += 1;
            }
            let mass: Vec<f64> = degree.iter().map(|&d| d as f64 + 1.0).collect();
            // Pinned nodes are handed to FA2 as `fixed`, which is what makes the
            // pin *mean* something: `shell.js:467` re-settles the layout and the
            // pinned nodes stay where they were. Derived from `pinned` here (not
            // read off `NodeVis.fixed`) so the layout vector cannot drift from
            // the painted flag — one source of truth, consulted by both.
            let fixed: Vec<bool> =
                self.vis.iter().map(|v| self.pinned.contains(&v.index)).collect();
            let hw: Vec<f64> = sizes.iter().map(|s| s.0 as f64 / 2.0).collect();
            let hh: Vec<f64> = sizes.iter().map(|s| s.1 as f64 / 2.0).collect();
            let ids: Vec<String> =
                self.vis.iter().map(|v| self.graph.nodes[v.index].id.clone()).collect();
            // A re-seed preserves nothing: `anchored` is empty (the explicit
            // `&[]`, not a vector of `false`, per `LayoutInput::anchored`), so
            // only a pin holds a node still.
            let input = LayoutInput {
                pairs: &pairs,
                mass: &mass,
                fixed: &fixed,
                hw: &hw,
                hh: &hh,
                ids: &ids,
                anchored: &[],
            };
            graphviz::run_layout(&input, &seed, self.settings)
        };

        for (v, &(x, y)) in self.vis.iter_mut().zip(out.iter()) {
            // A pinned node keeps the position it was pinned at; FA2 returns the
            // pinned coordinate unchanged, but writing it explicitly means a
            // future solver change cannot quietly move a pinned node.
            if self.pinned.contains(&v.index) {
                // A pinned node is placed too, just not by this run — it already
                // had a position when the pin was set.
                v.unplaced = false;
                continue;
            }
            v.pos = (x, y);
            v.unplaced = false;
        }
        // This run has now settled exactly this set; the next one compares
        // against it to tell growth from loss.
        self.settled_keys = self.vis.iter().map(|v| v.index).collect();
        self.sync_fixed_flags();
        for (v, &s) in self.vis.iter_mut().zip(sizes.iter()) {
            v.size = s;
        }
        if self.fit_pending {
            self.fit();
            self.fit_pending = false;
        }
        if std::env::var_os("NATIVE_CSVIEWER_DEBUG_LAYOUT").is_some() {
            for v in &self.vis {
                eprintln!(
                    "[native-csviewer] layout {} size=({:.3},{:.3}) pos=({:.6},{:.6})",
                    self.graph.nodes[v.index].id, v.size.0, v.size.1, v.pos.0, v.pos.1
                );
            }
            eprintln!(
                "[native-csviewer] camera x={:.6} y={:.6} zoom={:.6} canvas={}x{}",
                self.cam.x, self.cam.y, self.cam.zoom, self.canvas_w, self.canvas_h
            );
        }
    }

    /// Place the nodes that are new to this canvas beside the node that revealed
    /// them. Port of `graph.js seedExpansion` (spline) then `seedFan` (ring).
    ///
    /// `placed` is the inherited position of every slot (`None` for a slot this
    /// run has never laid out); `sizes` are the measured chip sizes; `seed` is the
    /// position vector the run will start from. Only the slots that [`Self::relayout`]
    /// found unplaced are candidates, so a root is always an inherited node —
    /// which is exactly the reference's precondition (`graph.js:417`).
    ///
    /// Returns `(slot, position, which branch placed it)` per newly placed node,
    /// or `None` when nothing was new (a plain re-settle, a settings change, a
    /// pin toggle). Deterministic: `hash32` over the node id, never an RNG.
    fn seed_new_children(
        &self,
        placed: &[Option<(f64, f64)>],
        sizes: &[(f32, f32)],
        seed: &[(f64, f64)],
        spacing: f64,
    ) -> Option<Vec<(usize, (f64, f64), ChildPlacement)>> {
        let kids: Vec<usize> =
            (0..self.vis.len()).filter(|&s| placed[s].is_none()).collect();
        if kids.is_empty() {
            return None;
        }
        // Every "is this new node near its parent?" question is asked against the
        // seed as it stands, so a node placed by an earlier root in this same loop
        // can anchor the next root's children — the behaviour `graph.js` gets
        // because it writes each placement straight into `state.pos`.
        let mut work: Vec<(f64, f64)> = seed.to_vec();
        // Adjacency in slot space, computed once: slot -> the slots it is joined
        // to by a pulled relation. `GraphView::neighbours` is the same function
        // that decides what a double-click reveals, so a new node can only ever
        // be seeded under a node that genuinely brought it in.
        //
        // Built with ONE pass over the edges, not `neighbours(slot)` once per
        // visible slot: the latter is O(V × E) and made an expansion on a large
        // canvas scan every edge thousands of times. The relation semantics are
        // the ones `neighbours` implements — `contains` is directed outwards,
        // every other pulled kind is symmetric, and the set includes the node
        // itself.
        let n_slots = self.vis.len();
        let mut slot_nbrs: Vec<Vec<usize>> = (0..n_slots).map(|s| vec![s]).collect();
        for e in &self.graph.edges {
            let from_slot = self.slot_of.get(&e.from).copied();
            let to_slot = self.slot_of.get(&e.to).copied();
            if let (Some(fs), Some(ts)) = (from_slot, to_slot) {
                if self.pull_kinds_for(e.from).contains(&e.kind.as_str()) {
                    slot_nbrs[fs].push(ts);
                }
                if e.kind != "contains"
                    && self.pull_kinds_for(e.to).contains(&e.kind.as_str())
                {
                    slot_nbrs[ts].push(fs);
                }
            }
        }
        // `neighbours` deduped with a set; parallel edges can otherwise list the
        // same child twice and bias the spline fit. First occurrence wins, so
        // the order still follows the edge list.
        for list in &mut slot_nbrs {
            if list.len() > 1 {
                let mut seen: HashSet<usize> = HashSet::with_capacity(list.len());
                list.retain(|&s| seen.insert(s));
            }
        }
        let mut is_kid = vec![false; n_slots];
        for &k in &kids {
            is_kid[k] = true;
        }

        // Group the new slots by the placed node that revealed them, walking the
        // *roots* rather than the children. `GraphView::neighbours` is
        // directional — an expansion pulls `contains:out` — so a new class lists
        // its own children but not the namespace that brought it in. Reading the
        // relation from the parent's side is the only way to find the root, and
        // it is also exactly what `graph.js` does: `expandNode` calls
        // `seedExpansion(id, newNodeIds)` with the id it just expanded.
        //
        // Slots are visited in ascending order and each root's children keep
        // their slot order, so the grouping is deterministic.
        let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
        let mut grouped: HashSet<usize> = HashSet::new();
        for root in 0..self.vis.len() {
            if placed[root].is_none() || !slot_nbrs[root].iter().any(|&c| is_kid[c]) {
                continue;
            }
            let children: Vec<usize> =
                slot_nbrs[root].iter().copied().filter(|&c| is_kid[c]).collect();
            if children.is_empty() {
                continue;
            }
            for &c in &children {
                grouped.insert(c);
            }
            groups.push((root, children));
        }
        // New nodes no placed root lists — the far end of a `calls` edge whose
        // near end arrived in the same gesture, say. They keep the disc position
        // the seed already gave them and are deliberately left un-anchored: they
        // are exactly the nodes the force pass should be allowed to place.
        let unparented: Vec<usize> =
            kids.iter().copied().filter(|k| !grouped.contains(k)).collect();

        let mut out: Vec<(usize, (f64, f64), ChildPlacement)> = Vec::new();
        for (root, children) in groups {
            let root_pos = work[root];
            let spec_of = |s: usize| ChildSpec {
                half: HalfSize {
                    hw: sizes[s].0 as f64 / 2.0,
                    hh: sizes[s].1 as f64 / 2.0,
                },
                hash: (graphviz::hash32(&self.graph.nodes[self.vis[s].index].id) % 100) as f64
                    / 100.0,
            };
            let specs: Vec<ChildSpec> = children.iter().map(|&s| spec_of(s)).collect();
            let points: Vec<Option<(f64, f64)>> = work.iter().map(|p| Some(*p)).collect();
            let Some(placement) = graphviz::seed_children(
                root_pos,
                spec_of(root).half,
                &points,
                &slot_nbrs[root],
                graphviz::graph_center(&points),
                &specs,
                spacing,
            ) else {
                continue;
            };
            let kind = placement.kind.unwrap_or(ChildPlacement::Fan);
            for (&slot, &p) in children.iter().zip(placement.positions.iter()) {
                work[slot] = p;
                out.push((slot, p, kind));
            }
        }

        // `unparented` is read only to keep the reason above explicit at the call
        // site rather than implicit in an omission.
        let _ = unparented;
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// The branch [`Self::relayout`] will take on its next run, without running
    /// it. Exposed so a caller can report *why* a layout is about to happen
    /// instead of inferring it from the positions afterwards.
    ///
    /// A reported [`SeedContinuity::Continue`] is the *intended* branch; the run
    /// itself can still turn it into a re-seed when child placement finds an
    /// arriving node with no placed root. That decision needs the measured chip
    /// boxes, which this side-effect-free probe does not compute.
    pub fn seed_branch(&self) -> SeedContinuity {
        if let Some(o) = self.seed_override {
            return o;
        }
        let placed: Vec<Option<(f64, f64)>> = self
            .vis
            .iter()
            .map(|v| if v.unplaced { None } else { Some(v.pos) })
            .collect();
        let keys_now: HashSet<usize> = self.vis.iter().map(|v| v.index).collect();
        if self.settled_keys.is_empty() {
            graphviz::seed_continuity(&placed)
        } else if self.settled_keys.iter().any(|k| !keys_now.contains(k)) {
            SeedContinuity::Reseed
        } else {
            SeedContinuity::Continue
        }
    }

    /// Force the next [`Self::relayout`] to lay the visible set out from the
    /// golden-angle disc rather than continuing from the positions on screen.
    ///
    /// The non-destructive branch is the default because that is what the
    /// reference does on every `startLayout` call it makes (`layout.js:280` is
    /// behind `reinitialize`, and all of them pass `false`). This is the hook for
    /// the gestures that genuinely invalidate an arrangement — a rebuild that
    /// swapped the graph, or a host that wants "start over" — so that the choice
    /// is stated by the caller rather than guessed from a counter.
    pub fn reseed_layout(&mut self) {
        self.seed_override = Some(SeedContinuity::Reseed);
    }

    /// Apply one Layout ▸ slider edit to the live settings.
    ///
    /// This is the *consumed* half of the slider path: the value the pointer
    /// produced (`nzk_ui::route_drag`) lands here, and the renderer reads it back
    /// out of `self.settings` on the next paint — `spacing` in the layout seed and
    /// the FA2 pass, `curvature` in every edge's control point, `node_size` in the
    /// dot radius and chip pad, `text_size` in the chip label's font and the
    /// measured box. Nothing here is a dead control: each of those four reads was
    /// verified in this file before the binding was added.
    ///
    /// # Timing (`shell.js:818-869`, the semantics this reproduces)
    ///
    /// * **Live** (`input`, pointer still down) writes `settings` only for the
    ///   controls whose *paint* is cheap: curvature, node size and text size. It
    ///   never re-runs the layout — dragging must not start an FA2 pass per pixel.
    /// * **Release** (`change`) is the commit: it marks the layout dirty so the
    ///   next frame re-settles, which is what makes spacing, iterations and node
    ///   size take effect on the arrangement.
    ///
    /// The two halves are expressed on the *edit* ([`SliderEdit::writes_settings`]
    /// and [`SliderEdit::resettles`]) rather than decided by a caller, so a test
    /// can assert the distinction directly instead of inferring it from a frame.
    ///
    /// Returns `true` when the edit changed something, so a host can skip a
    /// repaint on a no-op drag.
    pub fn apply_slider(&mut self, edit: SliderEdit) -> bool {
        let value = edit.value();
        let before = self.settings;
        if edit.writes_settings() {
            match edit.kind {
                SliderKind::Spacing => self.settings.spacing = value,
                SliderKind::Iterations => self.settings.iterations = value.round() as i32,
                SliderKind::Curve => self.settings.curvature = value,
                SliderKind::NodeSize => self.settings.node_size = value as f32,
                SliderKind::TextSize => self.settings.text_size = value as f32,
            }
        }
        if edit.resettles() {
            self.layout_dirty = true;
        }
        // A text-size change invalidates every cached box even for a live edit
        // that does not re-run the layout — the cache is keyed on the font, so
        // the next `size()` would miss anyway, but clearing here also drops the
        // now-unreachable old-font entries instead of letting them sit until the
        // 5000-entry sweep.
        if edit.kind == SliderKind::TextSize && self.settings.text_size != before.text_size {
            self.metrics = ChipMetrics::with_font(self.settings.node_size, self.settings.text_size);
        }
        self.settings != before || edit.resettles()
    }

    /// The current settings, for a caller that paints the readouts.
    pub fn settings(&self) -> LayoutSettings {
        self.settings
    }

    /// `layout.js fitView` / `computeFit`.
    pub fn fit(&mut self) {
        let pts: Vec<(f64, f64)> = self.vis.iter().map(|v| v.pos).collect();
        if let Some(c) = Camera::compute_fit(self.canvas_w, self.canvas_h, &pts) {
            self.cam = c;
        }
    }

    /// The camera as `(zoom, x, y)`, for host-driven actions (task-26).
    ///
    /// Display-free: no egui type is involved, so an action module can hold and
    /// compare this without a display.
    pub fn camera(&self) -> (f64, f64, f64) {
        (self.cam.zoom, self.cam.x, self.cam.y)
    }

    /// Set the camera from a host action. The zoom is clamped to the same
    /// bounds the interactive path uses, so a `#zoomInBtn` hit and a scroll
    /// wheel can never exceed different limits.
    pub fn set_camera(&mut self, zoom: f64, x: f64, y: f64) {
        self.cam.zoom = zoom.clamp(graphviz::MIN_ZOOM, graphviz::MAX_ZOOM);
        self.cam.x = x;
        self.cam.y = y;
    }

    /// Centre the camera on the visible node with `Graph::nodes` index
    /// `index`, keeping the current zoom. Returns `false` (and moves nothing)
    /// when that node has no visible slot, which is the honest answer for a
    /// node the current view mode filtered out.
    ///
    /// This is `centerNode`'s "fly the camera to it" half (`interaction.js:422`):
    /// `Camera::world_to_screen` puts the world point `(cam.x, cam.y)` at the
    /// canvas centre, so assigning the node's world position there centres it.
    /// The single decision is the pure [`centred_camera`] helper, which the test
    /// pins against the real projection.
    pub fn center_on_node(&mut self, index: usize) -> bool {
        let Some(v) = self.vis.iter().find(|v| v.index == index) else {
            return false;
        };
        let (z, x, y) = centred_camera(self.cam.zoom, v.pos);
        self.set_camera(z, x, y);
        true
    }

    /// Topmost chip under a canvas-relative screen point.
    ///
    /// "Topmost" is the **paint order**, not the slot order. `paint_canvas`
    /// sorts chips so a selected node draws last (and a focused/hovered one
    /// before it), so two chips that overlap look like one node with the
    /// selected chip on top. Picking from `vis.iter().rev()` ignored that sort:
    /// clicking the visibly top chip could select the one painted underneath —
    /// the "it selected a node I did not click" report, most obvious when a
    /// filtered view leaves two chips stacked so they look like a single node.
    /// The rank below is the same `(!dim, selected)` key the painter sorts by,
    /// with slot order as the deterministic tie-break.
    ///
    /// The focus neighbourhood is only built when a point really hits MORE than
    /// one chip: a hover over a normal canvas has zero or one candidate, and
    /// paying for an edge scan on every pointer frame would make hovering the
    /// 9k-node corpus needlessly expensive.
    pub fn pick(&self, screen: Pos2) -> Option<usize> {
        let mut hits: Vec<usize> = Vec::new();
        for (slot, v) in self.vis.iter().enumerate() {
            let (cx, cy) = self.world_to_screen(v.pos);
            let r = Rect::from_center_size(
                Pos2::new(cx, cy),
                Vec2::new(v.size.0 * self.cam.zoom as f32, v.size.1 * self.cam.zoom as f32),
            )
            .expand(2.0);
            if r.contains(screen) {
                hits.push(slot);
            }
        }
        match hits.len() {
            0 => return None,
            1 => return Some(self.vis[hits[0]].index),
            _ => {}
        }

        let focus = self
            .hovered
            .or(self.selected)
            .filter(|i| self.slot_of.contains_key(i));
        let focus_slot = focus.and_then(|i| self.slot_of.get(&i).copied());
        let mut focus_set: HashSet<usize> = HashSet::new();
        if let Some(fs) = focus_slot {
            for &(a, b, _) in &self.edges {
                if a == fs {
                    focus_set.insert(b);
                }
                if b == fs {
                    focus_set.insert(a);
                }
            }
        }
        let selected = self.selected;
        let is_selected = |index: usize| {
            Some(index) == selected || self.marquee_sel.contains(&index)
        };
        let mut best: Option<((u8, u8), usize)> = None;
        for slot in hits {
            let v = &self.vis[slot];
            let dim = match focus {
                Some(f) => {
                    v.index != f && !focus_set.contains(&v.slot) && !is_selected(v.index)
                }
                None => false,
            };
            // `paint_canvas` sorts by exactly this key and draws in ascending
            // order; the later slot wins a tie, so a higher `(key, slot)` is
            // the chip a user sees on top. `!dim` is deliberate: dim chips are
            // painted FIRST (the selected chip must stay on top), so "not dim"
            // ranks higher in the paint order.
            let key = ((!dim) as u8, is_selected(v.index) as u8);
            if best.map_or(true, |(bk, bslot)| key > bk || (key == bk && slot > bslot)) {
                best = Some((key, slot));
            }
        }
        best.map(|(_, slot)| self.vis[slot].index)
    }

    /// Absolute canvas position of a world point.
    fn world_to_screen(&self, p: (f64, f64)) -> (f32, f32) {
        self.cam.world_to_screen(self.canvas_w, self.canvas_h, p.0, p.1)
    }

    /// The whole canvas: interaction (pan / zoom / pick / select) + painting.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        if std::env::var_os("NATIVE_CSVIEWER_DEBUG_RECT").is_some() {
            eprintln!(
                "[native-csviewer] canvas rect min=({:.1},{:.1}) size={:.1}x{:.1}",
                rect.min.x,
                rect.min.y,
                rect.width(),
                rect.height()
            );
        }
        self.canvas_w = rect.width().max(1.0);
        self.canvas_h = rect.height().max(1.0);
        // `Sense::click_and_drag()` is `CLICK | FOCUSABLE | DRAG` and therefore
        // already senses double-clicks; egui 0.37 has no `Sense::double_click()`
        // constructor at all. Only the *query* was missing, which is exactly why
        // the canvas could advertise "double-click a node to expand" and never
        // see the gesture. Drag/pan and single-click selection are unaffected.
        // A shell modal owns the pointer: hover still paints, but the canvas
        // takes no click/drag/scroll so a picker press cannot also select a node
        // behind the backdrop.
        let sense = if self.input_suspended {
            Sense::hover()
        } else {
            Sense::click_and_drag()
        };
        let response = ui.allocate_rect(rect, sense);
        let painter = ui.painter_at(rect);

        if self.layout_dirty {
            let ctx = ui.ctx().clone();
            let t = std::time::Instant::now();
            self.relayout(&ctx);
            crate::render::frame_mark("layout.total", t);
        }
        // `fit_pending` is a CAMERA request, not a layout one. It used to be
        // consumed only inside `relayout`, so `#fitBtn` (which sets the flag and
        // marks no layout dirty) never applied: the press was swallowed and the
        // canvas only re-fitted when some unrelated action happened to relayout.
        // Apply it here, after any relayout above has settled the positions and
        // after `canvas_w/h` are known, so the next painted frame uses the new
        // camera.
        if self.fit_pending {
            self.fit();
            self.fit_pending = false;
        }

        // `painter_at` keeps absolute screen coordinates; `Camera` is centred on
        // the canvas, so shift by the canvas origin.
        let origin = rect.min;

        self.paint_background(&painter, rect);

        // --- input ---------------------------------------------------------
        //
        // GESTURE PRECEDENCE (marquee vs pan vs click vs double-click).
        //
        // All four gestures share this one `Sense::click_and_drag()` response, so
        // the order below *is* the disambiguation rule. egui sets more than one
        // of these flags for a single physical gesture, which is exactly how two
        // features silently break each other. The rule, in priority order:
        //
        // 1. **Marquee** — a drag that *started on empty canvas* (no chip under
        //    the press point) is a marquee, and is consumed: it does not pan.
        //    A drag that started on a chip is a pan, so dragging a node still
        //    scrolls the canvas and never leaves a rectangle behind.
        // 2. **Pan** — the remaining drags (`dragged()`), unchanged.
        // 3. **Double-click** — checked before single click, because egui reports
        //    `clicked()` on the first click of a double-click too; taking the
        //    single-click branch first would `select()` and then *also* toggle
        //    expansion. Double-click on empty canvas is ignored (it cannot
        //    collapse anything by accident).
        // 4. **Single click** — the fallback, so a click that ends a micro-drag
        //    still selects exactly once.
        //
        // `drag_stopped()` (not `dragged()`) is what *commits* a marquee: it
        // fires once, on release, so the captured set is computed from the final
        // rectangle and `marquee` is cleared even when the release lands outside
        // the canvas.
        //
        // SELECTION IS PRESS-TIME. The reference selects on `pointerdown`
        // (`interaction.js:33-84`): a node is selected, Alt-pinned and its card
        // dismissed the moment the button goes down, not on the later
        // `pointerup` / DOM `click`. egui's `response.clicked()` is a *release*
        // edge (`PointerEvent::Released { click: Some(_) }`), so using it made
        // every pick wait for the mouse-up. The press branch below is the primary
        // path; the release branch is kept only for the empty-canvas click that
        // clears the selection, which the reference deliberately does on
        // `pointerup` (`interaction.js:122`).
        // Canvas-relative pointer position, in the same `Pos2` space `pick`
        // expects (`paint_canvas` converts with `origin + world_to_screen`, and
        // `pick` is called below with `p.x - origin.x`; this is that same shift).
        let rel_pos = response.interact_pointer_pos().map(|p| Pos2::new(p.x - origin.x, p.y - origin.y));
        let alt = ui.input(|i| i.modifiers.alt);
        // Press-time picking. `contains_pointer` (not `hovered`) is the right
        // gate: it is true on the press frame, and false when the nzk_ui chrome
        // or another layer covers the canvas, so a chrome press cannot select a
        // node underneath it.
        let primary_pressed = !self.input_suspended
            && response.contains_pointer()
            && ui.input(|i| i.pointer.button_pressed(PointerButton::Primary));
        if primary_pressed {
            if let Some(p) = ui.input(|i| i.pointer.interact_pos()) {
                // `interaction.js:38` — any left press on the canvas dismisses
                // the pinned card. A press on the card itself is intercepted by
                // its own `egui::Area`, so its controls stay usable.
                self.pinned_card = None;
                let hit = self.pick(Pos2::new(p.x - origin.x, p.y - origin.y));
                if alt {
                    // `interaction.js:48-56`: Alt+press pins/unpins the node
                    // under the pointer and selects it, without starting a drag.
                    if let Some(index) = hit {
                        self.select(Some(index));
                        self.toggle_pinned(index);
                    }
                } else if let Some(index) = hit {
                    // `select` records the outgoing selection for Back. It is
                    // idempotent, so the drag-started branch below re-selecting
                    // the same chip adds no history entry.
                    self.select(Some(index));
                }
            }
        }
        // A **primary** drag that starts on a chip moves that chip and pins it
        // (`interaction.js:73-84`); a primary drag over empty canvas is the
        // marquee; a middle/other drag is a pan (handled below). Checking the
        // button here is essential: without it `drag_started()` would turn every
        // node drag into a camera pan, which is the one gesture the reference
        // says *pins* a node.
        if response.drag_started_by(PointerButton::Primary) {
            // The gesture is resolved from the **press origin**, not the current
            // pointer: egui only calls a drag "started" after the cursor has
            // moved past its click slop, and by then the pointer has usually
            // left a narrow chip. Using the current position would make a drag
            // that began on a node look like a drag that began on empty canvas
            // (and would anchor a marquee at the wrong corner).
            let press_rel = ui
                .input(|i| i.pointer.press_origin())
                .map(|p| Pos2::new(p.x - origin.x, p.y - origin.y));
            if let Some(p) = press_rel.or(rel_pos) {
                if let Some(hit) = self.pick(p) {
                    self.select(Some(hit));
                    // Dragging pins the node (`interaction.js:82`), so the chip
                    // stays where the user drops it across the next re-settle.
                    self.set_pinned(hit, true);
                    if let Some(v) = self.vis.iter().find(|v| v.index == hit) {
                        let w = self.cam.screen_to_world(
                            self.canvas_w,
                            self.canvas_h,
                            p.x,
                            p.y,
                        );
                        self.node_drag = Some(NodeDrag {
                            index: hit,
                            off_x: v.pos.0 - w.0,
                            off_y: v.pos.1 - w.1,
                        });
                    }
                    // Moving the node dismisses a pinned card rather than
                    // leaving it floating over a chip it no longer describes.
                    self.pinned_card = None;
                } else {
                    // A marquee only opens on a press over empty canvas.
                    self.marquee = Some((p, rel_pos.unwrap_or(p)));
                }
            }
        }
        let marquee_open = self.marquee.is_some();
        if marquee_open {
            if let Some(p) = rel_pos {
                if let Some((anchor, _)) = self.marquee {
                    self.marquee = Some((anchor, p));
                }
            }
        }
        // Commit on release. `drag_stopped()` also fires for a pan, so gate on
        // the marquee actually being open — a pan must not resurrect one.
        if response.drag_stopped() && marquee_open {
            if let Some(rect) = self.marquee_rect() {
                self.finish_marquee(rect);
            }
        }
        // Move the dragged chip in world space. The world offset captured at the
        // press keeps the exact grab point under the pointer; `dragged_by` keeps
        // a middle-button pan from also moving the node.
        if response.dragged_by(PointerButton::Primary) {
            if let Some(drag) = self.node_drag {
                if let Some(p) = rel_pos {
                    let w = self.cam.screen_to_world(self.canvas_w, self.canvas_h, p.x, p.y);
                    if let Some(v) = self.vis.iter_mut().find(|v| v.index == drag.index) {
                        v.pos = (w.0 + drag.off_x, w.1 + drag.off_y);
                        v.unplaced = false;
                    }
                }
            }
        }
        if response.drag_stopped() {
            self.node_drag = None;
        }
        if response.dragged() && !marquee_open && self.node_drag.is_none() {
            let d = response.drag_delta();
            self.cam.x -= d.x as f64 / self.cam.zoom;
            self.cam.y -= d.y as f64 / self.cam.zoom;
        }
        if response.hovered() && !self.input_suspended {
            let scroll = ui.input(|i| i.smooth_scroll_delta);
            if scroll.y.abs() > 0.01 {
                if let Some(p) = response.hover_pos() {
                    let rel = p - origin;
                    let before = self.cam.screen_to_world(self.canvas_w, self.canvas_h, rel.x, rel.y);
                    self.cam.zoom = (self.cam.zoom * (scroll.y as f64 * 0.0025).exp())
                        .clamp(graphviz::MIN_ZOOM, graphviz::MAX_ZOOM);
                    let after = self.cam.screen_to_world(self.canvas_w, self.canvas_h, rel.x, rel.y);
                    self.cam.x += before.0 - after.0;
                    self.cam.y += before.1 - after.1;
                }
            }
        }
        // A click that *ended a marquee* must not also re-select: the release
        // reports `clicked()` when the drag was short enough for egui to still
        // call it a click, and `finish_marquee` has already chosen the focus.
        // `marquee_open` was captured before the commit, so this reads the
        // gesture's own state rather than the (now cleared) field.
        //
        // Node selection already happened on the press above; this release
        // branch exists only for the empty-canvas click, whose reference clears
        // the selection on `pointerup` when the press did not become a drag
        // (`interaction.js:122`). Re-selecting the hit here would undo the whole
        // point of press-time picking (and would toggle an Alt pin twice).
        if response.clicked() && !marquee_open {
            // Clear from the PRESS origin, not the release point. Selection is
            // press-time, and a focus rebuild can move chips between the two
            // edges of a quick click; asking about the release point then made
            // a click that had selected a node look like an empty-canvas click
            // and silently cleared it. The gesture's own press origin is the
            // only honest place to ask "did this click begin on empty canvas?".
            let press_rel = ui
                .input(|i| i.pointer.press_origin())
                .map(|p| Pos2::new(p.x - origin.x, p.y - origin.y));
            if let Some(p) = press_rel {
                if self.pick(p).is_none() {
                    self.select(None);
                }
            }
        }
        // Right-click a node to pin its action card open (`interaction.js:291-301`).
        // The card is rendered by [`Self::paint_pinned_card`] from a separate
        // `egui::Area`, so its buttons are real controls rather than pixels.
        // A press edge, like the primary pick: the reference's `contextmenu`
        // opens the card as soon as the right button goes down.
        if !self.input_suspended
            && response.contains_pointer()
            && ui.input(|i| i.pointer.button_pressed(PointerButton::Secondary))
        {
            self.pinned_card = ui
                .input(|i| i.pointer.interact_pos())
                .map(|p| Pos2::new(p.x - origin.x, p.y - origin.y))
                .and_then(|p| self.pick(p).map(|index| (index, p)));
            if let Some((index, _)) = self.pinned_card {
                self.select(Some(index));
            }
        }
        // The gesture the notice advertises. `interaction.js onDblClick` selects
        // the hit and expands it; a double-click on empty canvas is ignored, so
        // this cannot collapse anything by accident.
        //
        // Guarded on `!marquee_open` for the same reason as `clicked()`: egui
        // reports `double_clicked()` on the second click of a rapid double-click,
        // and a marquee must never be the thing that toggles expansion.
        if response.double_clicked() && !marquee_open {
            // Same press-time rule as the selection above: the hit is resolved
            // from where the click STARTED, not where the button came up. A
            // fast click that drifts, or a chip that moved with a focus
            // rebuild, must still toggle the node the press picked rather than
            // whichever node happens to be under the release point.
            let press_rel = ui
                .input(|i| i.pointer.press_origin())
                .map(|p| Pos2::new(p.x - origin.x, p.y - origin.y));
            if let Some(p) = press_rel {
                if let Some(hit) = self.pick(p) {
                    self.select(Some(hit));
                    self.toggle_node(hit);
                }
            }
        }
        // A pointer over the drawer/modal is not hovering the canvas behind
        // it: that overlay is painted pixels, not an egui widget, so egui still
        // reports the canvas as hovered. Picking through it made the canvas
        // highlight (and show relation pills for) a node hidden under the
        // drawer instead of the selected one. Input is suspended exactly while
        // an opaque overlay owns the pointer, so hover must be too.
        self.hovered = if self.input_suspended {
            None
        } else {
            response
                .hover_pos()
                .and_then(|p| self.pick(Pos2::new(p.x - origin.x, p.y - origin.y)))
        };
        if !self.input_suspended && ui.input(|i| i.key_pressed(egui::Key::F)) {
            self.fit();
        }
        if !self.input_suspended && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.selected = None;
            self.marquee_sel.clear();
            // `shell.js:398` — Esc closes a pinned card before clearing any
            // further state; the two happen together here because the canvas
            // owns both in this frame.
            self.pinned_card = None;
        }
        // P pins/unpins the selection. Bound here rather than in `actions.rs`
        // because a pin is pure view state: it changes what is drawn and what
        // re-settle may move, and `GraphView` is where both of those live. A pin
        // needs a node, so it no-ops when nothing is selected instead of
        // inventing a target.
        if !self.input_suspended && ui.input(|i| i.key_pressed(egui::Key::P)) {
            if let Some(sel) = self.selected {
                self.toggle_pinned(sel);
            }
        }
        // Keyboard parity with the original: Backspace is Back (shell.js:445),
        // Up walks to the containment parent.
        if ui.input(|i| i.key_pressed(egui::Key::Backspace)) {
            self.navigate_back();
        }
        if ui.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
            self.navigate_up();
        }
        // Enter / Space expand the selection, mirroring the double-click.
        if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            if let Some(sel) = self.selected {
                self.toggle_node(sel);
            }
        }

        if !self.show_edges {
            // still draw nodes below
        }
        let t_paint = std::time::Instant::now();
        self.paint_canvas(&painter, origin);
        crate::render::frame_mark("paint.canvas", t_paint);
        self.paint_pinned_card(ui, origin, rect);
        let _ = response;
    }

    /// Paint the right-click card pinned to one node, and route its buttons.
    ///
    /// The reference's card is the DOM `#tooltip` with real buttons
    /// (`interaction.js:161-208`); this is the native equivalent, kept in an
    /// `egui::Area` above the canvas so a click on a button is a widget click,
    /// not a canvas press. Its actions are the ones the reference exposes:
    /// pin/unpin the node, expand/collapse it, and open its declaration.
    fn paint_pinned_card(&mut self, ui: &mut egui::Ui, origin: Pos2, canvas: Rect) {
        let Some((index, rel)) = self.pinned_card else { return };
        let Some(node) = self.graph.nodes.get(index) else {
            self.pinned_card = None;
            return;
        };
        let node_id = node.id.clone();
        let title = if node.label.is_empty() { &node.full } else { &node.label };
        let loc = if node.file.is_empty() {
            String::new()
        } else if node.line > 0 {
            format!("{}:{}", node.file, node.line)
        } else {
            node.file.clone()
        };
        let pinned = self.pinned.contains(&index);
        let expanded = self.expanded.contains(&index);
        let mut close = false;
        let mut toggle_pin = None;
        let mut toggle_expand = None;
        let mut open_code = false;
        // Keep the card inside the canvas even when the user right-clicks near
        // an edge. The card's own size is not known before layout, so clamp the
        // anchor with a generous margin and let egui keep the frame on-screen.
        let anchor = Pos2::new(
            origin.x + rel.x.clamp(8.0, (canvas.width() - 260.0).max(8.0)),
            origin.y + rel.y.clamp(8.0, (canvas.height() - 150.0).max(8.0)),
        );
        egui::Area::new(Id::new("ncv_node_card"))
            .order(Order::Foreground)
            .fixed_pos(anchor)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style())
                    .fill(Color32::from_rgb(0x25, 0x25, 0x26))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(0x3c, 0x3c, 0x3c)))
                    .show(ui, |ui| {
                        ui.set_max_width(280.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(title)
                                    .monospace()
                                    .strong()
                                    .color(Color32::from_rgb(0xd4, 0xd4, 0xd4)),
                            );
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.small_button("✕").clicked() {
                                    close = true;
                                }
                            });
                        });
                        // The degree line is `kind · N edges`; spell out what N
                        // counts when the pointer rests on it. The same
                        // explanation the drawer's `deg N` chip carries, in the
                        // native card (which has no DOM `title` to borrow).
                        ui.label(
                            egui::RichText::new(format!("{} · {} edges", node.kind, node.degree))
                                .monospace()
                                .size(11.0)
                                .color(Color32::from_rgb(0x9d, 0x9d, 0x9d)),
                        )
                        .on_hover_text(format!(
                            "Degree — the number of distinct graph neighbours \
                             (incoming + outgoing edges) for this declaration. This one has {}.",
                            node.degree
                        ));
                        if !loc.is_empty() {
                            ui.label(
                                egui::RichText::new(loc)
                                    .monospace()
                                    .size(11.0)
                                    .color(Color32::from_rgb(0x9d, 0x9d, 0x9d)),
                            );
                        }
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.small_button(if pinned { "Unpin node" } else { "Pin node" }).clicked() {
                                toggle_pin = Some(index);
                            }
                            if ui
                                .small_button(if expanded { "Collapse" } else { "Expand" })
                                .clicked()
                            {
                                toggle_expand = Some(index);
                            }
                            if ui.small_button("{ } View code").clicked() {
                                open_code = true;
                            }
                        });
                    });
            });
        // The actions are applied AFTER the closure so no `&mut self` borrow is
        // held by the area's UI. Pressing the card never reaches the canvas's
        // click branch above, so it cannot also move the selection.
        if close {
            self.pinned_card = None;
        }
        if let Some(index) = toggle_pin {
            self.set_pinned(index, !self.pinned.contains(&index));
        }
        if let Some(index) = toggle_expand {
            // v2's card action is the same all-buckets pull as the drawer's
            // `Expand` (`interaction.js:283-287`: `expandNode(id, {all:true})`),
            // not the narrower canvas double-click.
            if self.expanded.contains(&index) {
                self.collapse_node(index);
            } else {
                self.expand_node_all(index);
            }
        }
        if open_code {
            self.node_code_request = Some(node_id);
        }
    }

    /// The node whose card action asked to open its declaration, exactly once.
    pub fn take_node_code_request(&mut self) -> Option<String> {
        self.node_code_request.take()
    }

    fn paint_background(&self, painter: &Painter, rect: Rect) {
        let pal = self.palette;
        painter.rect_filled(rect, 0.0, col(pal.canvas));
        // The original's radial gradient is exact — and therefore invisible —
        // for nzk-dark+, where glow == canvas.
        if pal.canvas_glow != pal.canvas {
            let c = Pos2::new(rect.center().x, rect.min.y + rect.height() * 0.38);
            let steps = 18;
            for i in (0..steps).rev() {
                let t = i as f32 / steps as f32;
                let mix = |a: u8, b: u8| -> u8 { (a as f32 + (b as f32 - a as f32) * t) as u8 };
                let color = Rgba::rgb(
                    mix(pal.canvas.0, pal.canvas_glow.0),
                    mix(pal.canvas.1, pal.canvas_glow.1),
                    mix(pal.canvas.2, pal.canvas_glow.2),
                );
                painter.add(EllipseShape::filled(
                    c,
                    Vec2::new((1200.0 * t).max(0.5), (720.0 * t).max(0.5)),
                    col(color),
                ));
            }
        }
    }

    /// Whether edge `k` (between slots `a` and `b`) can contribute any pixel
    /// inside `cull`.
    ///
    /// The bbox is the straight segment expanded by the cull margin AND by the
    /// quadratic's bow: a fanned parallel pair can put its control point far
    /// outside the endpoint box, and culling on the endpoints alone would drop a
    /// curve that visibly crosses the canvas.
    fn edge_in_view(&self, k: usize, a: usize, b: usize, cull: Rect) -> bool {
        let (ax, ay) = self.screen_pos[a];
        let (bx, by) = self.screen_pos[b];
        let mut pad = EDGE_CULL_MARGIN;
        let (pi, pc) = self.edge_fan.get(k).copied().unwrap_or((0.0, 1.0));
        if pc > 1.0 && self.settings.curvature > 0.0 {
            let curve = self.settings.curvature as f32 * (pi - (pc - 1.0) / 2.0);
            if curve != 0.0 {
                let len = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
                pad += curve.abs() * len;
            }
        }
        Rect::from_two_pos(Pos2::new(ax, ay), Pos2::new(bx, by))
            .expand(pad)
            .intersects(cull)
    }

    fn paint_canvas(&mut self, painter: &Painter, origin: Pos2) {
        let (w, h) = (self.canvas_w, self.canvas_h);
        let pal = self.palette;
        let zoom = self.cam.zoom as f32;

        // --- grid ----------------------------------------------------------
        // Pre-composited, because egui would blend the 5% alpha in linear space
        // and land on (32,32,34) where the reference has (34,36,39).
        let grid_stroke = Stroke::new(1.0, col(graphviz::grid_over_canvas(&pal)));
        for (x1, y1, x2, y2) in graphviz::grid_lines(&self.cam, w, h) {
            painter.line_segment(
                [origin + Vec2::new(x1, y1), origin + Vec2::new(x2, y2)],
                grid_stroke,
            );
        }

        // --- focus (hover wins over selection, like `render.js draw()`) -----
        let focus = self
            .hovered
            .or(self.selected)
            .filter(|i| self.slot_of.contains_key(i));
        let focus_slot = focus.and_then(|i| self.slot_of.get(&i).copied());
        let mut focus_set: HashSet<usize> = HashSet::new();
        if let Some(fs) = focus_slot {
            for &(a, b, _) in &self.edges {
                if a == fs {
                    focus_set.insert(b);
                }
                if b == fs {
                    focus_set.insert(a);
                }
            }
        }

        // --- shared screen geometry, once per frame -------------------------
        //
        // Both loops need world -> screen, and the edge loop needs it for both
        // endpoints; converting once per slot is what keeps the per-frame cost
        // O(n + visible edges) instead of O(E) conversions.
        let (cw, ch, cam) = (self.canvas_w, self.canvas_h, self.cam);
        self.screen_pos.clear();
        self.screen_pos.extend(
            self.vis
                .iter()
                .map(|v| cam.world_to_screen(cw, ch, v.pos.0, v.pos.1)),
        );
        let clip = Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h));
        // A shape wholly outside the clip rect cannot contribute a pixel: egui
        // clips it away after tessellation. Culling it before shape creation is
        // therefore pixel-identical and is what makes a zoomed-in view of a huge
        // graph cost the same as the small view it actually shows.
        let cull = clip.expand(EDGE_CULL_MARGIN);
        // Relation labels are laid out before the nodes paint but after the
        // labels are measured, so the chip boxes are the first thing they must
        // keep clear: a word box / relation node half-hidden under an opaque
        // chip is unreadable clutter even when it overlaps no other label.
        //
        // `vt_mode` (the repurposed `#hideEdgeLabels` switch) is no longer a
        // "hide" gate: it selects HOW the relation is drawn — a word box beside
        // a curved arrow (off) or a relation node on a direct line (on). Word
        // boxes keep the reference's zoom gate; relation nodes are structural,
        // so they paint at any zoom (still thinned on a dense canvas).
        let label_all = zoom >= graphviz::EDGE_LABEL_ZOOM;
        let labels_possible = self.vt_mode || label_all;
        let mut chip_rects: Vec<graphviz::LabelRect> = Vec::new();
        let mut on_screen = 0usize;
        for (i, v) in self.vis.iter().enumerate() {
            let (sx, sy) = self.screen_pos[i];
            let chip = Rect::from_center_size(
                Pos2::new(sx, sy),
                Vec2::new(v.size.0 * zoom, v.size.1 * zoom),
            );
            if cull.intersects(chip) {
                on_screen += 1;
                // A label exists for focused edges even below the "all labels"
                // zoom (and for every edge in VT mode), so the no-overlap seed
                // must be collected whenever one can be painted.
                if labels_possible {
                    chip_rects.push(graphviz::LabelRect::from_center(
                        origin.x + sx,
                        origin.y + sy,
                        chip.width() / 2.0,
                        chip.height() / 2.0,
                    ));
                }
            }
        }

        // --- edges ----------------------------------------------------------
        // `graph.js buildModel`: parallel relations between one pair fan out
        // into separate strands, each with its own curve offset. The fan is
        // derived in `rebuild_view`; the paint pass is a lookup, not three maps.
        let show_edges = self.show_edges;

        // Count what is actually on screen: density is a property of this frame,
        // not of the resident edge list, so zooming into a crowded graph keeps
        // full fidelity once only a few hundred edges remain visible.
        let visible_edges = if show_edges {
            self.edges
                .iter()
                .enumerate()
                .filter(|(k, e)| self.edge_in_view(*k, e.0, e.1, cull))
                .count()
        } else {
            0
        };
        let dense = visible_edges > DENSE_EDGE_LIMIT;

        // Relation pills are collected while the edges stroke, then placed in a
        // second pass. Painting one at its edge's midpoint is what makes two
        // crossing edges (or two relations fanned between the same pair) stack
        // their word boxes; the collected set lets `place_pills` slide each one
        // to the nearest free slot on its own path and drop it only when the
        // whole path is occupied.
        let mut pills: Vec<PendingPill> = Vec::new();

        if show_edges {
            for (k, &(a, b, ei)) in self.edges.iter().enumerate() {
                let (ax, ay) = self.screen_pos[a];
                let (bx, by) = self.screen_pos[b];
                if !self.edge_in_view(k, a, b, cull) {
                    continue;
                }
                let pa = origin + Vec2::new(ax, ay);
                let pb = origin + Vec2::new(bx, by);
                let touches = focus_slot.map(|fs| a == fs || b == fs);
                let (pi, pc) = self.edge_fan.get(k).copied().unwrap_or((0.0, 1.0));
                let kind = &self.graph.edges[ei].kind;
                let mut d: EdgeDraw = graphviz::edge_draw(
                    (pa.x, pa.y),
                    self.vis[a].size.0 * zoom,
                    (pb.x, pb.y),
                    self.vis[b].size.0 * zoom,
                    kind,
                    pi,
                    pc,
                    zoom,
                    self.settings.curvature,
                    if focus_slot.is_some() { touches } else { None },
                );
                if self.vt_mode {
                    vt_straighten(&mut d);
                }
                let color = col_a(d.color, d.alpha);
                let stroke = Stroke::new(d.width.max(0.5), color);
                let p1 = Pos2::new(d.x1, d.y1);
                let p2 = Pos2::new(d.x2, d.y2);
                if d.curved {
                    painter.add(QuadraticBezierShape::from_points_stroke(
                        [p1, Pos2::new(d.mx, d.my), p2],
                        false,
                        Color32::TRANSPARENT,
                        stroke,
                    ));
                } else if let Some([dl, gl]) = d.dash {
                    painter.extend(egui::Shape::dashed_line(&[p1, p2], stroke, dl, gl));
                } else {
                    painter.line_segment([p1, p2], stroke);
                }
                // At density a 7.5 px arrowhead per edge is invisible clutter
                // that costs one polygon and several tessellation segments each.
                // Focused/touched edges keep it, so emphasis still reads.
                if let Some(tri) = d.arrow {
                    if !dense || touches == Some(true) {
                        painter.add(egui::Shape::convex_polygon(
                            tri.iter().map(|&(x, y)| Pos2::new(x, y)).collect(),
                            color,
                            Stroke::NONE,
                        ));
                    }
                }
                // Same density reasoning for the relation label: at density it
                // overlaps into an unreadable band and each one lays out text.
                if edge_label_visible(
                    self.vt_mode,
                    label_all,
                    touches == Some(true),
                    dense,
                ) && d.pill.is_some()
                {
                    let label = graphviz::edge_label(kind);
                    if !label.is_empty() {
                        pills.push(PendingPill {
                            p1: (d.x1, d.y1),
                            mid: (d.mx, d.my),
                            p2: (d.x2, d.y2),
                            curved: d.curved,
                            text: label,
                            strong: touches == Some(true),
                            color: d.color,
                        });
                    }
                }
            }
        }

        if !pills.is_empty() {
            paint_pills(painter, &mut pills, &chip_rects, self.vt_mode);
        }

        // --- nodes: dim first, selected last (`render.js` sorts before draw) --
        //
        // The sort only exists to order three classes (dim, normal, selected).
        // With no focus and nothing selected every key is `(0, 0)`, and a stable
        // sort leaves the identity order untouched — so skip it, which is the
        // common case (a still canvas with no hover).
        let selected = self.selected;
        // A marquee-captured node draws with the selected border too, so the set
        // a drag grabbed is visible after release. `marquee_sel` is a set, so this
        // is a membership test rather than a second `Option`.
        let is_selected = |index: usize| {
            Some(index) == selected || self.marquee_sel.contains(&index)
        };
        let dim_of = |v: &NodeVis| -> bool {
            match focus {
                Some(f) => v.index != f && !focus_set.contains(&v.slot) && !is_selected(v.index),
                None => false,
            }
        };
        let needs_order =
            focus.is_some() || selected.is_some() || !self.marquee_sel.is_empty();
        let mut order: Vec<usize> = if needs_order {
            (0..self.vis.len()).collect()
        } else {
            Vec::new()
        };
        if needs_order {
            order.sort_by_key(|&i| {
                let v = &self.vis[i];
                // `!dim`: dimmed chips are the background and must be painted
                // first. Sorting on `dim` directly (1 = dim) painted them LAST,
                // so a dim node could cover the selected chip whenever their
                // boxes overlapped. A click then picked the chip visibly on top
                // (the dim one) instead of the selected node.
                ((!dim_of(v)) as u8, is_selected(v.index) as u8)
            });
        }
        let order_len = if needs_order { order.len() } else { self.vis.len() };
        // When thousands of chips are on screen at a legible font, their labels
        // overlap into mush and each one costs a text layout. Keep labels for the
        // focused/selected chips and for sizes where type is actually readable.
        let dense_nodes = on_screen > DENSE_NODE_LIMIT;
        let chip_fs = self.settings.text_size * zoom;
        let dense_labels_ok = !dense_nodes || chip_fs >= DENSE_LABEL_MIN_FONT;

        for k in 0..order_len {
            let i = if needs_order { order[k] } else { k };
            let v = &self.vis[i];
            let (sx, sy) = self.screen_pos[i];
            let chip = Rect::from_center_size(
                Pos2::new(sx, sy),
                Vec2::new(v.size.0 * zoom, v.size.1 * zoom),
            );
            if !cull.intersects(chip) {
                continue;
            }
            let nd = &self.graph.nodes[v.index];
            let (sx, sy) = (origin.x + sx, origin.y + sy);
            let label = if nd.kind == "namespace" && !nd.full.is_empty() {
                nd.full.as_str()
            } else {
                nd.label.as_str()
            };
            let sel = is_selected(v.index);
            let label_ok = dense_labels_ok || sel || Some(v.index) == focus;
            self.paint_chip(
                painter,
                sx,
                sy,
                v.size,
                zoom,
                label,
                &nd.kind,
                dim_of(v),
                sel,
                v.fixed,
                label_ok,
            );
        }

        // --- the amber canvas notice (`core.js updateCanvasNotice`) ----------
        // This string is now literally true: double-click *does* expand children
        // and calling edges, and the count is the real `vis.len()`. The suffix
        // names which relation a double-click will pull for the current
        // selection, because that genuinely differs by kind (`graph.js
        // expandNode`): a type-like node pulls its base types, a member pulls its
        // call targets. Saying "calling edges" unconditionally would overstate
        // what a double-click on a namespace does — the same defect class as the
        // original string, which described a gesture that did not exist.
        let hint = match self.selected {
            None => "double-click a node to expand its children".to_string(),
            Some(i) if i < self.graph.nodes.len() => {
                if self.expanded.contains(&i) {
                    "double-click to collapse it again".to_string()
                } else if self.pull_kinds_for(i).contains(&"inherits") {
                    "double-click to expand its children and base types".to_string()
                } else {
                    "double-click to expand its children and calling edges".to_string()
                }
            }
            Some(_) => "double-click a node to expand its children".to_string(),
        };
        let text = format!("{} nodes in view · {}", self.vis.len(), hint);
        paint_notice(painter, origin, w, &text, &pal);

        // --- marquee rectangle ------------------------------------------------
        // Drawn last so it sits above the chips it is capturing. Only the live
        // drag is drawn: a committed selection shows as the chips' own selected
        // border rather than a persistent rectangle, which is what the original
        // does and avoids a stale box claiming a selection that has changed.
        if let Some(rel) = self.marquee_rect() {
            let r = rel.translate(origin.to_vec2());
            painter.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(0x86, 0xaa, 0xf9, 28));
            painter.rect_stroke(
                r,
                0.0,
                Stroke::new(1.0, col(pal.chip_sel_border)),
                StrokeKind::Inside,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint_chip(
        &self,
        painter: &Painter,
        sx: f32,
        sy: f32,
        size: (f32, f32),
        zoom: f32,
        label: &str,
        kind: &str,
        dim: bool,
        selected: bool,
        fixed: bool,
        label_ok: bool,
    ) {
        let pal = self.palette;
        let w = size.0 * zoom;
        let h = size.1 * zoom;
        let x = sx - w / 2.0;
        let y = sy - h / 2.0;
        let dot = self.settings.node_size * zoom;
        let r = (dot / 2.0).max(1.6);
        let fs = self.settings.text_size * zoom;
        let dot_col = col(if dim { pal.chip_dim_text } else { graphviz::kind_color(self.theme, kind) });

        // When the text is no longer legible the chip degenerates to its glyph.
        if fs < 6.0 || !self.show_labels {
            painter.circle_filled(Pos2::new(sx, sy), r, dot_col);
            return;
        }

        let rect = Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h));
        let radius = (h / 2.0).min(2.0);
        painter.rect_filled(rect, radius, col(if dim { pal.chip_dim_bg } else { pal.chip_bg }));
        painter.rect_stroke(
            rect,
            radius,
            Stroke::new(
                if selected { 1.6 } else { 1.0 },
                col(if selected { pal.chip_sel_border } else { pal.chip_border }),
            ),
            StrokeKind::Inside,
        );

        if fixed {
            let pr = (h * 0.3).clamp(2.0, 3.5);
            let c = Pos2::new(x + w - pr - 2.0, y + pr + 2.0);
            painter.circle_filled(c, pr, col(pal.pin));
            painter.circle_stroke(
                c,
                pr,
                Stroke::new(1.0, Color32::from_rgba_unmultiplied(0, 0, 0, 140)),
            );
        }

        painter.circle_filled(Pos2::new(x + graphviz::CHIP_PAD * zoom + r, sy), r, dot_col);
        // Dense canvases keep the chip box and glyph but drop the unreadable
        // text: this is the single most expensive per-chip shape (a Parley
        // layout), and at this density the labels overlap into noise anyway.
        if !label_ok {
            return;
        }
        let tx = x + (graphviz::CHIP_PAD + self.settings.node_size + graphviz::CHIP_GAP) * zoom;
        // The box was measured from `chip_text` (`graphviz::ChipMetrics::size`),
        // which truncates above `CHIP_MAX_CHARS`. Painting the raw label drew the
        // overflow outside its own box — a long namespace name then ran into the
        // neighbour's name. Use the measured string here too, and clip as a hard
        // guarantee for the sub-pixel case (fractional font sizes rasterise a
        // hair wider than the world-unit measurement scaled by the camera).
        let trimmed;
        let shown = if label.chars().count() > graphviz::CHIP_MAX_CHARS {
            trimmed = graphviz::chip_text(label);
            trimmed.as_str()
        } else {
            label
        };
        // Vertical ink calibration (rounds 25-26): the reference canvas paints
        // the chip label 2 px higher than egui's `LEFT_CENTER` box centring does.
        // Round 25 measured the two label inks at (537..731, 490..506) and
        // (427..780, 543..565) and took a 3 px up-shift (-2.5, whole-frame mae
        // 1.7166 -> 1.4901). Round 26 re-measured at sub-pixel resolution: the
        // first chip's ink still sat 1 px low while the second was aligned, so
        // the glyph raster phase, not a uniform offset, was off. A 0.5 px
        // down-shift (-2.0) moves the first label onto the ref phase and leaves
        // the second alone: canvas mae 0.2912 -> 0.2385, whole 1.3649 -> 1.3262.
        // The plateau is -2.25..-1.75; -1.5 and -2.5 are both ~1 px off.
        painter.with_clip_rect(rect).text(
            Pos2::new(tx, sy - 2.0),
            Align2::LEFT_CENTER,
            shown,
            crate::typography::canvas_mono(fs),
            col(if dim { pal.chip_dim_text } else { pal.chip_text }),
        );
    }
}

/// Whether a relation label may be painted at all.
///
/// `vt_mode` selects the presentation, not whether the relation is visible:
///
/// * **word box** (`vt_mode == false`) — the reference's pill beside a curved
///   arrow. `label_all` (readable zoom) or a touched edge enables it; on a dense
///   canvas only the touched edge keeps its pill so the words do not merge into
///   an unreadable band.
/// * **relation node** (`vt_mode == true`) — a compact node on a direct line.
///   That node IS the relation, so it paints at every zoom; only the dense
///   canvas thinning applies (touched edges always keep it).
///
/// Split out of the paint loop so the truth table is testable without a GPU.
fn edge_label_visible(vt_mode: bool, label_all: bool, touched: bool, dense: bool) -> bool {
    if vt_mode {
        !dense || touched
    } else {
        (label_all || touched) && (!dense || touched)
    }
}

/// Make an edge a straight chord and put its relation node on the chord's
/// midpoint (VT mode).
///
/// `edge_draw` bends a parallel fan with a quadratic control point; VT mode's
/// "direct line" means the stroke is a straight segment, so the control point
/// collapses onto the midpoint and `curved` is cleared. The fan's parallel
/// offset is already baked into the endpoints, so parallel relations stay
/// separate strands rather than collapsing onto one line.
fn vt_straighten(d: &mut EdgeDraw) {
    let mx = (d.x1 + d.x2) / 2.0;
    let my = (d.y1 + d.y2) / 2.0;
    d.mx = mx;
    d.my = my;
    d.curved = false;
    d.pill = Some((mx, my));
}

/// A relation pill waiting for a free slot on its own edge.
///
/// Collected during the edge pass and painted afterwards, because placement needs
/// every pill's box before it can decide whether a slot is free. The curve is the
/// geometry that was actually stroked (`edge_draw`'s trimmed endpoints plus the
/// fan's control point), so a slid pill remains on its own relation.
struct PendingPill {
    p1: (f32, f32),
    mid: (f32, f32),
    p2: (f32, f32),
    curved: bool,
    text: &'static str,
    strong: bool,
    /// The edge's own colour — the relation node's dot in VT mode.
    color: graphviz::Rgba,
}

/// Candidate positions along an edge, as fractions of its drawn path.
///
/// `0.5` is the reference anchor (`core.js`'s midpoint formula), so a pill whose
/// midpoint is clear does not move at all. The rest step toward the endpoints in
/// a fixed order: a disputed slot is resolved by the nearest free slot on the
/// *same* edge instead of by drawing both boxes on top of each other.
const PILL_SLIDE: [f32; 11] = [0.5, 0.44, 0.56, 0.38, 0.62, 0.32, 0.68, 0.26, 0.74, 0.2, 0.8];

/// Place and paint the collected relation pills without overlaps.
///
/// Greedy and deterministic: focused/touched edges claim their slots first (a
/// stable sort, so the rest keep edge order), then each pill tries the slide
/// positions in [`PILL_SLIDE`], then a small perpendicular step off the path, and
/// is omitted when every position collides. Omission is deliberate — drawing a
/// stacked box anyway is exactly the overlap this pass exists to remove.
///
/// `chips` are the painted node boxes: they seed the occupied set, so a word box
/// never disappears under an opaque chip name. If the whole edge is covered by
/// nodes, the pill is dropped (the edge line and arrow still name the relation).
///
/// The galleys are laid out once per distinct relation word (there are six) and
/// reused, so the deconfliction costs one lookup per pill, not an extra text
/// layout.
fn paint_pills(
    painter: &Painter,
    pills: &mut [PendingPill],
    chips: &[graphviz::LabelRect],
    vt_mode: bool,
) {
    pills.sort_by_key(|p| !p.strong);
    let font = FontId::monospace(if vt_mode {
        graphviz::VT_REL_FONT
    } else {
        graphviz::EDGE_PILL_FONT
    });
    let mut galleys: HashMap<&'static str, std::sync::Arc<egui::Galley>> = HashMap::new();
    let mut placed = graphviz::LabelPlacer::new();
    let gap = graphviz::EDGE_PILL_GAP;
    // Chip boxes are occupied from the start. Seeding never fails for a settled
    // layout (the Noverlap pass keeps chips apart), and a label is never drawn
    // under one when a free slot exists somewhere on its edge.
    for chip in chips {
        placed.try_insert(chip.expanded(gap));
    }

    for pill in pills.iter() {
        let galley = galleys
            .entry(pill.text)
            .or_insert_with(|| {
                painter.layout_no_wrap(pill.text.to_owned(), font.clone(), Color32::PLACEHOLDER)
            })
            .clone();
        let tw = galley.size().x;
        let (hw, hh) = if vt_mode {
            // The relation node is dot + gap + text inside the horizontal pad.
            (
                (tw + graphviz::VT_REL_DOT + graphviz::VT_REL_GAP
                    + graphviz::VT_REL_PAD_X * 2.0)
                    / 2.0,
                graphviz::VT_REL_H / 2.0,
            )
        } else {
            (tw / 2.0 + graphviz::EDGE_PILL_PAD_X, graphviz::EDGE_PILL_H / 2.0)
        };

        let mut chosen: Option<(f32, f32)> = None;
        for &t in PILL_SLIDE.iter() {
            let c = graphviz::edge_point_at(pill.p1, pill.mid, pill.p2, pill.curved, t);
            let rect = graphviz::LabelRect::from_center(c.0, c.1, hw, hh).expanded(gap);
            if placed.try_insert(rect) {
                chosen = Some(c);
                break;
            }
        }
        if chosen.is_none() && !vt_mode {
            // Word boxes may step off the path: a dense fan still yields
            // readable boxes instead of dropping all but the first.
            let (dx, dy) = (pill.p2.0 - pill.p1.0, pill.p2.1 - pill.p1.1);
            let len = (dx * dx + dy * dy).sqrt().max(1.0);
            let (nx, ny) = (-dy / len, dx / len);
            let off = hh * 2.0 + gap * 2.0;
            'search: for &t in PILL_SLIDE.iter().take(5) {
                let c = graphviz::edge_point_at(pill.p1, pill.mid, pill.p2, pill.curved, t);
                for side in [-1.0f32, 1.0] {
                    let p = (c.0 + nx * off * side, c.1 + ny * off * side);
                    let rect = graphviz::LabelRect::from_center(p.0, p.1, hw, hh).expanded(gap);
                    if placed.try_insert(rect) {
                        chosen = Some(p);
                        break 'search;
                    }
                }
            }
        }
        if chosen.is_none() && vt_mode {
            // A relation node must stay ON its direct line (it is the relation,
            // not an annotation). If every slot collides, keep the midpoint and
            // let the nodes overlap rather than silently dropping the relation.
            chosen = Some(graphviz::edge_point_at(
                pill.p1, pill.mid, pill.p2, pill.curved, 0.5,
            ));
        }

        if let Some(c) = chosen {
            let c = Pos2::new(c.0, c.1);
            if vt_mode {
                paint_relation_node(painter, c, galley, tw, pill.color, pill.strong);
            } else {
                paint_pill(painter, c, galley, tw, pill.strong);
            }
        }
    }
}

/// A relation drawn as a small **node** on the line (VT mode).
///
/// Same visual language as a graph node — a rounded, bordered box with the
/// relation-coloured dot and the relation's own word (`Calls`, `Uses`,
/// `Children`, …) — so the edge reads `node -> relation node -> node` rather
/// than `node -> arrow` with a floating label. The stroke is painted first, so
/// the node covers the middle of the direct line.
fn paint_relation_node(
    painter: &Painter,
    c: Pos2,
    galley: std::sync::Arc<egui::Galley>,
    tw: f32,
    edge_color: graphviz::Rgba,
    strong: bool,
) {
    use graphviz::{
        VT_REL_BORDER, VT_REL_DOT, VT_REL_FILL, VT_REL_FILL_STRONG, VT_REL_GAP, VT_REL_H,
        VT_REL_PAD_X, VT_REL_RADIUS,
    };
    let w = tw + VT_REL_DOT + VT_REL_GAP + VT_REL_PAD_X * 2.0;
    let rect = Rect::from_center_size(c, Vec2::new(w, VT_REL_H));
    let fill = if strong { VT_REL_FILL_STRONG } else { VT_REL_FILL };
    painter.rect_filled(rect, VT_REL_RADIUS, col_a(fill, if strong { 0.98 } else { 0.94 }));
    painter.rect_stroke(
        rect,
        VT_REL_RADIUS,
        Stroke::new(1.0, col_a(VT_REL_BORDER, if strong { 1.0 } else { 0.85 })),
        StrokeKind::Inside,
    );
    // The dot carries the edge kind's own colour, exactly like a node's kind dot.
    let dot = Pos2::new(rect.left() + VT_REL_PAD_X + VT_REL_DOT / 2.0, c.y);
    painter.circle_filled(dot, VT_REL_DOT / 2.0, col_a(edge_color, 1.0));
    let g = galley.size();
    let text_x = rect.left() + VT_REL_PAD_X + VT_REL_DOT + VT_REL_GAP;
    painter.galley(
        Pos2::new(text_x, c.y - g.y / 2.0),
        galley,
        col_a(
            if strong { graphviz::EDGE_PILL_TEXT_STRONG } else { graphviz::EDGE_PILL_TEXT },
            if strong { 1.0 } else { 0.9 },
        ),
    );
}

/// Small pill naming the relation (`render.js drawEdgePill`).
fn paint_pill(
    painter: &Painter,
    c: Pos2,
    galley: std::sync::Arc<egui::Galley>,
    tw: f32,
    strong: bool,
) {
    let rect = Rect::from_center_size(
        c,
        Vec2::new(tw + graphviz::EDGE_PILL_PAD_X * 2.0, graphviz::EDGE_PILL_H),
    );
    let fill = if strong { graphviz::EDGE_PILL_FILL_STRONG } else { graphviz::EDGE_PILL_FILL };
    let text_col =
        if strong { graphviz::EDGE_PILL_TEXT_STRONG } else { graphviz::EDGE_PILL_TEXT };
    painter.rect_filled(
        rect,
        graphviz::EDGE_PILL_RADIUS,
        col_a(fill, if strong { 0.96 } else { 0.72 }),
    );
    painter.rect_stroke(
        rect,
        graphviz::EDGE_PILL_RADIUS,
        Stroke::new(
            1.0,
            col_a(graphviz::EDGE_PILL_BORDER, if strong { 1.0 } else { 0.8 }),
        ),
        StrokeKind::Inside,
    );
    // Center the galley in the box on BOTH axes. The old `c.y + 0.5` treated
    // the galley's top as the box's centre, so a 14px line box started 7px low
    // and the word spilled out under its own pill ("Children"/"Uses" looked
    // bottom-heavy and outside the rounded rect). The rect is centred on `c`,
    // so the text must be too.
    let g = galley.size();
    painter.galley(
        Pos2::new(c.x - g.x / 2.0, c.y - g.y / 2.0),
        galley,
        col_a(text_col, if strong { 1.0 } else { 0.8 }),
    );
}

/// The amber canvas notice (`style.css .notice`, `core.js updateCanvasNotice`).
/// The fill is pre-composited over the canvas: `rgba(48,36,8,.92)` over
/// `#1e1e1e` is `(46,35,9)`, which is what the reference shows.
fn paint_notice(painter: &Painter, origin: Pos2, canvas_w: f32, text: &str, pal: &Palette) {
    let font = FontId::monospace(11.0);
    let wrap = (canvas_w - 140.0).clamp(80.0, 340.0);
    let galley = painter.layout(text.to_owned(), font, Color32::WHITE, wrap);
    let pad = Vec2::new(10.0, 6.0);
    let size = galley.size() + pad * 2.0;
    let rect = Rect::from_min_size(
        Pos2::new(origin.x + canvas_w - 10.0 - size.x, origin.y + 10.0),
        size,
    );
    painter.rect_filled(rect, 6.0, col(graphviz::composite_srgb(pal.canvas, Rgba::rgb(48, 36, 8), 0.92)));
    painter.rect_stroke(
        rect,
        6.0,
        Stroke::new(1.0, Color32::from_rgb(0x5a, 0x44, 0x13)),
        StrokeKind::Inside,
    );
    painter.galley(rect.min + pad, galley, Color32::from_rgb(0xcc, 0xa7, 0x00));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, Node};
    use std::path::PathBuf;

    fn node(id: &str, kind: &str) -> Node {
        Node {
            id: id.to_string(),
            label: id.to_string(),
            full: id.to_string(),
            kind: kind.to_string(),
            ns: "N".to_string(),
            owner: String::new(),
            file: "a.cs".to_string(),
            line: 1,
            end_line: 2,
            degree: 0,
            external: false,
            ..Default::default()
        }
    }

    fn edge(from: usize, to: usize, kind: &str) -> Edge {
        Edge { from, to, kind: kind.to_string(), count: 1 }
    }

    /// The shape the engine actually produces for this corpus: one namespace
    /// containing two classes, each containing one method, plus a `calls` edge
    /// from one method to the other. `counts` on the live index show `contains`
    /// as the dominant relation (2991) with `calls` at 3058, so both are real.
    ///
    /// 0 = NZK (namespace), 1 = NZK.A (class), 2 = NZK.B (class),
    /// 3 = NZK.A.m (method), 4 = NZK.B.k (method).
    fn graph() -> GraphView {
        GraphView::new(Graph {
            repo: PathBuf::from("/tmp/view-expansion-test"),
            nodes: vec![
                node("NZK", "namespace"),
                node("NZK.A", "class"),
                node("NZK.B", "class"),
                node("NZK.A.m", "method"),
                node("NZK.B.k", "method"),
            ],
            edges: vec![
                edge(0, 1, "contains"),
                edge(0, 2, "contains"),
                edge(1, 3, "contains"),
                edge(2, 4, "contains"),
                edge(3, 4, "calls"),
            ],
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        })
    }

    fn visible_ids(v: &GraphView) -> Vec<&str> {
        v.vis.iter().map(|n| v.graph.nodes[n.index].id.as_str()).collect()
    }

    fn visible_slots(v: &GraphView) -> HashSet<usize> {
        v.vis.iter().map(|n| n.slot).collect()
    }

    /// THE documented initial state. `graph.js loadNamespaces` installs only the
    /// namespaces, and view.rs:19-22 records that namespace-first is deliberate.
    /// If expansion altered this, it changed the default instead of adding to it.
    #[test]
    fn starts_namespace_only_before_any_expansion() {
        let v = graph();
        assert_eq!(v.vis.len(), 1, "the shipped initial view is one namespace");
        assert_eq!(visible_ids(&v), ["NZK"]);
        assert!(v.edges.is_empty(), "loadNamespaces installs no edges");
        assert_eq!(v.selected, None);
        assert!(!v.is_expanded(0));
    }

    /// Acceptance 2: expanding a namespace INCREASES the visible count and the
    /// expected children appear.
    #[test]
    fn expanding_a_namespace_reveals_its_children_and_edges() {
        let mut v = graph();
        let before = v.vis.len();
        assert_eq!(before, 1);

        let added = v.expand_node(0);

        assert_eq!(v.vis.len(), 3, "namespace + its two classes");
        assert!(v.vis.len() > before, "expansion must grow the visible set");
        assert_eq!(added.len(), 2, "exactly the two classes are new");
        assert!(visible_ids(&v).contains(&"NZK.A"));
        assert!(visible_ids(&v).contains(&"NZK.B"));
        assert!(!visible_ids(&v).contains(&"NZK.A.m"), "grandchildren need their own expansion");
        // Both `contains` edges now have both endpoints on screen.
        assert_eq!(v.edges.len(), 2);
        assert!(v.edges.iter().all(|&(a, b, _)| a != b), "no self-loops");
    }

    #[test]
    fn expansion_depth_controls_how_many_relationship_hops_are_visible() {
        let mut v = graph();
        v.expand_node(0);
        assert!(visible_ids(&v).contains(&"NZK.A"));
        assert!(!visible_ids(&v).contains(&"NZK.A.m"));

        assert!(v.set_expansion_depth(2));
        assert!(visible_ids(&v).contains(&"NZK.A.m"));
        assert!(visible_ids(&v).contains(&"NZK.B.k"));
        assert!(!v.set_expansion_depth(2), "unchanged depth does not rebuild");
        assert!(v.set_expansion_depth(0));
        assert_eq!(visible_ids(&v), ["NZK"]);
    }

    // ------------------------------------------------------------------
    // `#groupChildren` / `#hideChildren` (task-13)
    // ------------------------------------------------------------------

    /// Helper: the kinds of the drawn edges, in `self.edges` order.
    fn drawn_edge_kinds(v: &GraphView) -> Vec<&str> {
        v.edges.iter().map(|&(_, _, ei)| v.graph.edges[ei].kind.as_str()).collect()
    }

    /// **THE REGRESSION PIN.** `group_children == false` drops `contains` from
    /// the *drawn* edge list and from nothing else.
    ///
    /// This is the test the Lead asked for by name, and it is the one that
    /// matters most: the gate sits at `view.rs:348` inside the `self.edges`
    /// rebuild, and `self.edges` is consumed only by painters and hit-testing —
    /// never by [`GraphView::neighbours`] or [`GraphView::expand_node`]. So
    /// turning the filter on must hide the containment *lines* while leaving
    /// expansion completely intact.
    ///
    /// It is written to fail in both directions:
    ///
    /// * if someone moves the gate into `neighbours()`/`expand_node` (the
    ///   `view.rs:377` site the first spec wrongly named), the visible set and
    ///   `added` below shrink and this goes RED — that is the "regression
    ///   wearing a feature's clothes" case;
    /// * if someone makes the flag a no-op, `drawn_edge_kinds` still contains
    ///   `contains` and the first assertion goes RED.
    #[test]
    fn group_children_off_drops_only_drawn_contains_edges_never_expansion() {
        let mut v = graph();
        assert!(v.group_children, "core.js:270 default is true");

        // --- with the flag ON (the shipped default) ---------------------
        let added_on = v.expand_node(0); // NZK -> NZK.A, NZK.B
        assert_eq!(added_on.len(), 2);
        assert_eq!(v.vis.len(), 3);
        assert_eq!(
            drawn_edge_kinds(&v),
            vec!["contains", "contains"],
            "both containment edges are drawn at the default"
        );

        // --- turn the filter ON (i.e. groupChildren = false) ------------
        let mut v = graph();
        assert!(
            v.set_group_children(false),
            "a real change must report true"
        );
        assert!(!v.group_children);

        // Expansion is UNCHANGED: same additions, same visible set.
        let added_off = v.expand_node(0);
        assert_eq!(
            added_off, added_on,
            "expansion must pull exactly the same nodes with the flag off"
        );
        assert_eq!(v.vis.len(), 3, "the visible node set is untouched");
        // Membership, not order: `vis` is ordered by placement, and placement
        // legitimately differs when the drawn-edge set differs, so asserting a
        // literal sequence here would pin an unrelated detail. What matters is
        // that the same three nodes are on the canvas.
        {
            let mut ids = visible_ids(&v);
            ids.sort_unstable();
            assert_eq!(
                ids,
                vec!["NZK", "NZK.A", "NZK.B"],
                "the containment children are still revealed"
            );
        }
        assert!(v.is_expanded(0), "the node really is expanded");
        assert!(
            !visible_ids(&v).contains(&"NZK.A.m"),
            "grandchildren still need their own expansion"
        );

        // ...but the drawn containment lines are gone.
        assert!(
            v.edges.is_empty(),
            "every drawn edge was `contains`, so none survive: {:?}",
            drawn_edge_kinds(&v)
        );

        // --- and turning it back on restores them ----------------------
        assert!(v.set_group_children(true), "a real change must report true");
        assert_eq!(
            drawn_edge_kinds(&v),
            vec!["contains", "contains"],
            "re-enabling restores the drawn containment edges"
        );
        assert_eq!(v.vis.len(), 3, "and still does not disturb the node set");
    }

    /// A `calls` edge survives the filter — the gate is `contains`-specific, not
    /// a blanket edge drop.
    ///
    /// `graph.js:812` names the kind explicitly (`e.kind === 'contains'`), and a
    /// version that dropped every edge when the flag was off would still pass the
    /// test above on a corpus whose only drawn edges are `contains`. This pins
    /// the specificity.
    #[test]
    fn group_children_off_keeps_non_contains_edges() {
        let mut v = graph();
        v.set_group_children(false);

        // 0=NZK, 1=NZK.A, 2=NZK.B, 3=NZK.A.m, 4=NZK.B.k, with 3 --calls--> 4.
        // Expand the class (pulls its method) then the method (pulls the
        // `calls` far end), which is the path the reference's expandNode takes.
        v.expand_node(0);
        v.expand_node(1);
        v.expand_node(3);

        let kinds = drawn_edge_kinds(&v);
        assert!(
            kinds.contains(&"calls"),
            "the `calls` edge must still be drawn with the flag off: {kinds:?}"
        );
        assert!(
            !kinds.contains(&"contains"),
            "no `contains` edge may be drawn with the flag off: {kinds:?}"
        );
    }

    // ------------------------------------------------------------------
    // `#groupEdges` (task-24)
    // ------------------------------------------------------------------

    /// **THE `#groupEdges` PIN, BOTH DIRECTIONS.** `group_edges == false` is the
    /// exact complement of `group_children == false`: it drops every *non*
    /// `contains` edge and keeps the containment lines (`graph.js:814`).
    ///
    /// This is what makes the checkbox a real filter rather than a round-tripped
    /// boolean, and it is written to fail both ways:
    ///
    /// * if someone makes `set_group_edges` a no-op that only stores the flag,
    ///   the `calls` edge survives and the first assertion goes RED;
    /// * if someone wires it to `group_children` (aliasing the two distinct
    ///   controls — the specific bug the Lead named), turning it off would drop
    ///   `contains` instead of keeping it, and the second assertion goes RED.
    #[test]
    fn group_edges_off_drops_non_contains_edges_and_keeps_containment() {
        let mut v = graph();
        assert!(v.group_edges, "core.js:271 ships groupEdges: true");

        // Same expansion path as the `group_children` test: the class pulls its
        // method, the method pulls the `calls` far end, so the drawn set has
        // both `contains` and `calls` edges to distinguish.
        v.expand_node(0);
        v.expand_node(1);
        v.expand_node(3);

        let before = drawn_edge_kinds(&v);
        assert!(
            before.contains(&"calls") && before.contains(&"contains"),
            "fixture must draw both edge kinds for this test to mean anything: {before:?}"
        );
        // Owned copy: `before` borrows `v`, and `set_group_edges` needs `&mut v`.
        let before: Vec<String> = before.into_iter().map(str::to_string).collect();

        assert!(v.set_group_edges(false), "true ⇒ false is a real change");
        assert!(!v.group_edges);

        let after = drawn_edge_kinds(&v);
        assert!(
            !after.contains(&"calls"),
            "`calls` is not `contains`, so group_edges=false must drop it: {after:?}"
        );
        assert!(
            after.contains(&"contains"),
            "group_edges must NOT gate `contains` — that is group_children's job. \
             Seeing it dropped here means the two controls were aliased: {after:?}"
        );

        // The node set is a separate filter: it is what the expansions put on
        // screen, and an *edge* gate never changes it. Measured rather than
        // hard-coded, and compared against the same view with the flag on.
        let nodes_with_filter_off = v.vis.len();
        assert!(nodes_with_filter_off > 0);

        assert!(v.set_group_edges(true), "false ⇒ true is a real change");
        assert_eq!(
            drawn_edge_kinds(&v)
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>(),
            before,
            "re-enabling restores exactly the edges that were there"
        );
        assert_eq!(
            v.vis.len(),
            nodes_with_filter_off,
            "the drawn-edge gate must never change which nodes are on screen"
        );
    }

    /// The two flags gate disjoint halves of the edge set, so with BOTH off
    /// nothing is drawn and with both on the two tests above hold. Their
    /// independence is the point: neither is a synonym for the other.
    #[test]
    fn the_two_edge_gates_are_disjoint_and_independent() {
        let mut v = graph();
        v.expand_node(0);
        v.expand_node(1);
        v.expand_node(3);

        v.set_group_children(false);
        v.set_group_edges(false);
        assert!(
            drawn_edge_kinds(&v).is_empty(),
            "contains is gated by group_children and everything else by \
             group_edges, so both off must draw nothing: {:?}",
            drawn_edge_kinds(&v)
        );

        // Turning only one back on brings back only that half.
        v.set_group_children(true);
        let kinds = drawn_edge_kinds(&v);
        assert!(
            kinds.iter().all(|k| *k == "contains"),
            "with group_edges still off only containment may return: {kinds:?}"
        );
    }

    /// `set_group_edges` has the same single-writer contract as
    /// `set_group_children`: it reports `true` only on a real transition, so a
    /// repaint can tell "clicked the already-set state" from "changed the
    /// filter", and it is idempotent.
    #[test]
    fn set_group_edges_reports_change_and_is_idempotent() {
        let mut v = graph();
        assert!(v.group_edges, "default true");
        assert!(!v.set_group_edges(true), "no change ⇒ false");
        assert!(v.group_edges);
        assert!(v.set_group_edges(false), "true ⇒ false reports true");
        assert!(!v.group_edges);
        assert!(!v.set_group_edges(false), "already false ⇒ false");
        assert!(v.set_group_edges(true), "false ⇒ true reports true");
        assert!(!v.set_group_edges(true), "already true ⇒ false");
    }

    /// `group_edges` is a *drawn* filter, not an expansion filter — the same
    /// distinction the `group_children` work pinned. Turning it off must not
    /// stop a double-click from pulling neighbouring nodes on screen.
    #[test]
    fn group_edges_off_never_disables_expansion() {
        let mut v = graph();
        v.set_group_edges(false);
        let added = v.expand_node(0);
        assert!(
            !added.is_empty(),
            "expansion must still pull nodes with the edge filter off"
        );
        assert!(v.vis.len() > 1, "the class is on screen");
    }

    /// The single writer reports whether anything actually changed, and only
    /// rebuilds on a real change.
    ///
    /// The return value is what makes "clicked the already-current state"
    /// distinguishable from "changed the filter", which is what a caller needs to
    /// decide whether to repaint — and what lets the two-inverse-checkbox mirrors
    /// avoid a feedback loop.
    #[test]
    fn set_group_children_reports_change_and_is_idempotent() {
        let mut v = graph();
        assert!(v.group_children, "default true");

        // Setting it to the value it already has is a no-op.
        assert!(!v.set_group_children(true), "no change ⇒ false");
        assert!(v.group_children);

        // A real change reports true...
        assert!(v.set_group_children(false), "true ⇒ false reports true");
        assert!(!v.group_children);

        // ...and repeating it is again a no-op.
        assert!(!v.set_group_children(false), "already false ⇒ false");

        // Back, and idempotent again.
        assert!(v.set_group_children(true), "false ⇒ true reports true");
        assert!(!v.set_group_children(true), "already true ⇒ false");
    }

    /// The two-inverse-checkbox relationship, at the **model level**.
    ///
    /// The reference drives one flag from two boxes (`shell.js:603` and `:646`)
    /// and re-mirrors them on every change (`:604`, `:647`). The inversion is a
    /// pure function of the one flag, so it is asserted here as such:
    /// `#groupChildren.checked == group_children` and
    /// `#hideChildren.checked == !group_children`.
    ///
    /// **SCOPE NOTE, deliberately stated so this cannot be misread:** this test
    /// pins the *derivation*, not the *wiring*. Nothing in this crate yet calls
    /// `set_checked` with these two booleans — the DOM-side mirror call sites are
    /// a separate piece of work — so a green run here does **not** mean either
    /// checkbox is painted. What it does mean is that whoever writes those call
    /// sites has the single source of truth and the correct polarity pinned.
    #[test]
    fn the_two_filter_checkboxes_are_inverses_of_the_one_flag() {
        // The two derivations the DOM call sites must use.
        let group_checked = |v: &GraphView| v.group_children;
        let hide_checked = |v: &GraphView| !v.group_children;

        let mut v = graph();
        // Default: groupChildren = true ⇒ group box checked, hide box not.
        assert!(group_checked(&v));
        assert!(!hide_checked(&v));

        v.set_group_children(false);
        assert!(!group_checked(&v), "#groupChildren follows the flag");
        assert!(hide_checked(&v), "#hideChildren is the inverse");

        v.set_group_children(true);
        assert!(group_checked(&v));
        assert!(!hide_checked(&v));

        // The invariant, stated once: they are never both on and never both off.
        for on in [true, false] {
            let mut v = graph();
            v.set_group_children(on);
            assert_ne!(
                group_checked(&v),
                hide_checked(&v),
                "the two boxes must always disagree (groupChildren={on})"
            );
        }
    }

    /// A second expansion of a member pulls in the far end of its `calls` edge.
    ///
    /// This is the "and calling edges" half of the canvas notice, and it must be
    /// exercised on a node whose pull actually includes `calls`: `pull_kinds_for`
    /// mirrors `graph.js expandNode`, which asks for `contains:out` plus
    /// `inherits:out` on a *type-like* node and `calls:out` on a *member*. A
    /// class therefore brings its members (and its base types), not its call
    /// targets; the method does.
    #[test]
    fn expanding_a_member_reveals_its_calling_edge_far_end() {
        let mut v = graph();
        v.expand_node(0); // namespace: NZK.A, NZK.B
        v.expand_node(3); // the method NZK.A.m: pulls calls -> NZK.B.k
        assert!(
            visible_ids(&v).contains(&"NZK.B.k"),
            "the notice promises calling edges, so a call's far end must be pulled in"
        );
        let has_calls = v
            .edges
            .iter()
            .any(|&(_, _, ei)| v.graph.edges[ei].kind == "calls");
        assert!(has_calls, "the calls edge itself must be drawn, not just its node");
    }

    /// A class expansion brings its members; a memberless class adds nothing.
    #[test]
    fn expanding_a_class_reveals_its_members() {
        let mut v = graph();
        v.expand_node(0);
        let n = v.vis.len();
        v.expand_node(1); // NZK.A contains NZK.A.m
        assert!(v.vis.len() > n, "expanding a class reveals its member");
        assert!(visible_ids(&v).contains(&"NZK.A.m"));
        assert!(
            !visible_ids(&v).contains(&"NZK.B.k"),
            "a class pulls members and base types, never its call targets"
        );
    }

    /// THE drawer-Expand defect, stated as a test: v2's `Expand` is "Add every
    /// relation bucket", so pressing it must put new nodes on the canvas — not
    /// merely open the drawer's lists.
    ///
    /// The fixture adds an incoming `uses` edge that a double-click never pulls
    /// (a class expands to `contains` + `inherits` only). `expand_node_all` must
    /// follow it, and the newly visible node must be reported as added.
    #[test]
    fn drawer_expand_pulls_every_relation_bucket_onto_the_canvas() {
        let mut v = graph();
        // NZK.B.k uses NZK.A: an incoming `uses` bucket on the class.
        v.graph.edges.push(edge(4, 1, "uses"));
        v.expand_node(0); // namespaces -> NZK.A, NZK.B

        // The narrow double-click pull leaves the far end off the canvas.
        v.expand_node(1);
        assert!(visible_ids(&v).contains(&"NZK.A.m"));
        assert!(
            !visible_ids(&v).contains(&"NZK.B.k"),
            "the ordinary class pull must not follow `uses`"
        );

        let added = v.expand_node_all(1);
        assert!(
            visible_ids(&v).contains(&"NZK.B.k"),
            "drawer Expand must add every relation bucket's far end"
        );
        assert!(added.contains(&4), "the newly pulled node is reported as added");
        assert!(
            v.edges
                .iter()
                .any(|&(_, _, ei)| v.graph.edges[ei].kind == "uses"),
            "the bucket's edge must be drawn, not just its node"
        );
        assert!(
            v.expand_node_all(1).is_empty(),
            "a second press adds nothing: the all-buckets pull is idempotent"
        );
    }

    /// The all-buckets flag is part of the expansion state, so a collapse must
    /// clear it: re-expanding by double-click returns to the narrow pull rather
    /// than silently resurrecting every bucket.
    #[test]
    fn collapsing_clears_the_drawer_all_buckets_pull() {
        let mut v = graph();
        v.graph.edges.push(edge(4, 1, "uses"));
        v.expand_node(0);
        v.expand_node_all(1);
        assert!(visible_ids(&v).contains(&"NZK.B.k"));

        v.collapse_node(1);
        assert!(!visible_ids(&v).contains(&"NZK.A.m"));
        v.expand_node(1);
        assert!(visible_ids(&v).contains(&"NZK.A.m"));
        assert!(
            !visible_ids(&v).contains(&"NZK.B.k"),
            "a collapse must forget that the node was opened with every bucket"
        );
    }

    /// Acceptance 3: toggling back is safe — a childless node and an
    /// already-expanded node both round-trip without panicking.
    ///
    /// It also pins the pruning rule: collapsing the namespace must drop the
    /// class expansion underneath it, so re-expanding reproduces the ORIGINAL
    /// set (3) rather than an accumulated one.
    #[test]
    fn toggling_a_childless_and_an_expanded_node_back_is_safe() {
        let mut v = graph();

        // A method has no `contains` children: expanding it adds no new node of
        // its own, though it does pull its call target in.
        v.expand_node(0);
        v.expand_node(1);
        let with_class = v.vis.len();
        assert_eq!(with_class, 4, "namespace + 2 classes + NZK.A.m");
        v.expand_node(3);
        assert_eq!(v.vis.len(), 5, "the method's call target joins");

        // Toggle the leaf twice: on, off, on, off — never panics.
        v.toggle_node(3);
        v.toggle_node(3);
        assert!(!v.vis.is_empty());

        // Re-establish the two-level state, then collapse the namespace.
        v.expand_node(1);
        v.expand_node(3);
        assert!(v.is_expanded(0) && v.is_expanded(1) && v.is_expanded(3));

        v.toggle_node(0);
        assert!(!v.is_expanded(0));
        assert_eq!(v.vis.len(), 1, "collapsing the namespace restores namespace-only");
        assert_eq!(visible_ids(&v), ["NZK"]);
        assert!(!v.is_expanded(1), "the descendant's expansion must not survive");
        assert!(!v.is_expanded(3), "nor must the grandchild's");

        // And back on again: the ORIGINAL set, not an accumulated one.
        v.toggle_node(0);
        assert_eq!(v.vis.len(), 3, "re-expanding reproduces the original namespace view");
        assert_eq!(v.expanded_set().len(), 1, "only the namespace is expanded again");
    }

    /// Collapsing a *class* leaves its sibling and the namespace untouched: the
    /// pruning is scoped to the collapsed subtree, not the whole set.
    #[test]
    fn collapsing_one_branch_leaves_the_other_expansions_alone() {
        let mut v = graph();
        v.expand_node(0);
        v.expand_node(1); // NZK.A -> NZK.A.m
        v.expand_node(2); // NZK.B -> NZK.B.k
        assert_eq!(v.vis.len(), 5);

        v.collapse_node(1);
        assert!(!v.is_expanded(1));
        assert!(v.is_expanded(0), "the namespace stays expanded");
        assert!(v.is_expanded(2), "the sibling branch stays expanded");
        assert!(!visible_ids(&v).contains(&"NZK.A.m"));
        assert!(visible_ids(&v).contains(&"NZK.B.k"));
    }

    /// Expanding twice is idempotent: the second call adds nothing, so a
    /// double-fired gesture cannot duplicate slots.
    #[test]
    fn expanding_twice_adds_nothing_the_second_time() {
        let mut v = graph();
        assert_eq!(v.expand_node(0).len(), 2);
        assert!(v.expand_node(0).is_empty(), "already expanded: no new nodes");
        assert_eq!(v.vis.len(), 3);
    }

    // -----------------------------------------------------------------------
    // G4: the layout is incremental
    // -----------------------------------------------------------------------

    /// Seed the view the way the running app does: lay it out once, so every
    /// visible node holds a real position instead of `rebuild_view`'s origin.
    ///
    /// `relayout` measures text, so it needs an `egui::Context` with fonts
    /// loaded — `Context::default()` alone panics with "No fonts available until
    /// first call to Context::run()". The `run_ui` pass below is the same trick
    /// the pre-existing re-settle test uses, and it keeps these tests runnable
    /// without a display. Real font widths do not matter here: what is asserted
    /// is "kept" versus "moved".
    fn measured_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |_ui| {});
        out.textures_delta.clear();
        ctx
    }

    fn laid_out() -> GraphView {
        let mut v = graph();
        v.relayout(&measured_ctx());
        v
    }

    fn pos_of(v: &GraphView, id: &str) -> (f64, f64) {
        let slot = v
            .vis
            .iter()
            .find(|n| v.graph.nodes[n.index].id == id)
            .unwrap_or_else(|| panic!("{id} is not visible"));
        slot.pos
    }

    fn moved(a: (f64, f64), b: (f64, f64)) -> f64 {
        ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
    }

    /// One expansion as the app performs it: the gesture, then the layout the
    /// next frame runs over the enlarged set.
    fn expand_and_settle(v: &mut GraphView, index: usize) {
        v.expand_node(index);
        v.relayout(&measured_ctx());
    }

    /// Filter changes that only gate edges must not buy another solver pass:
    /// the node set, placements and glyph metrics are all unchanged.
    #[test]
    fn an_edge_only_filter_change_leaves_the_layout_a_noop() {
        let mut v = laid_out();
        assert!(v.layout_is_noop(), "a settled view has nothing left to do");
        v.hidden_edges.insert("calls".to_string());
        v.rebuild_view();
        assert!(
            v.layout_is_noop(),
            "hiding an edge kind cannot move a node or resize a chip"
        );
        // A node-kind filter really does change the set, so that one must run.
        // Full mode is needed for the method nodes to be on the canvas at all
        // (the shipped namespace view shows only the namespace until expanded).
        v.view_mode = ViewMode::Full;
        v.rebuild_view();
        v.relayout(&measured_ctx());
        assert!(v.layout_is_noop(), "the full set is settled now");
        v.hidden_kinds.insert("method".to_string());
        v.rebuild_view();
        assert!(!v.layout_is_noop(), "dropping nodes must re-settle the canvas");
    }

    /// Growing every chip box at once (the Layout ▸ Text size slider) is the
    /// case where the CONTINUE branch's "settled chips never move" rule used to
    /// leave two enlarged boxes stacked: no chip in the pair was free, so the
    /// safety Noverlap pass moved nothing and the names drew through each
    /// other. The settle pass must separate them.
    #[test]
    fn a_text_size_change_separates_chips_that_grew_into_each_other() {
        let mut v = graph();
        v.view_mode = ViewMode::Full;
        v.rebuild_view();
        v.settings.spacing = graphviz::SPACING_MIN;
        v.relayout(&measured_ctx());

        // Grow every measured box, then run the layout the slider release runs.
        v.settings.text_size = graphviz::TEXT_SIZE_MAX;
        v.relayout(&measured_ctx());

        for i in 0..v.vis.len() {
            for j in i + 1..v.vis.len() {
                let a = &v.vis[i];
                let b = &v.vis[j];
                let dx = (a.pos.0 - b.pos.0).abs();
                let dy = (a.pos.1 - b.pos.1).abs();
                let need_x = (a.size.0 + b.size.0) as f64 / 2.0;
                let need_y = (a.size.1 + b.size.1) as f64 / 2.0;
                assert!(
                    dx >= need_x - 0.01 || dy >= need_y - 0.01,
                    "chips {} and {} still overlap after the text grew: dx={dx} need_x={need_x} dy={dy} need_y={need_y}",
                    v.graph.nodes[a.index].id,
                    v.graph.nodes[b.index].id
                );
            }
        }
    }

    /// A glyph-metric change must still run even though no position moves, so
    /// the no-op predicate has to include the measured box inputs.
    #[test]
    fn changing_the_chip_font_is_never_a_layout_noop() {
        let mut v = laid_out();
        assert!(v.layout_is_noop());
        v.settings.text_size = 13.0;
        assert!(!v.layout_is_noop(), "a new font size changes every chip box");
    }

    /// The parallel-edge fan is derived on the rebuild that changes the edge
    /// set, not rebuilt per frame; both strands must keep the reference's
    /// index/count pair.
    #[test]
    fn parallel_edges_get_their_fan_from_the_rebuild() {
        let mut v = graph();
        v.graph.edges.push(edge(3, 4, "calls"));
        v.view_mode = ViewMode::Full;
        v.rebuild_view();
        let calls: Vec<usize> = v
            .edges
            .iter()
            .enumerate()
            .filter(|(_, e)| v.graph.edges[e.2].kind == "calls")
            .map(|(k, _)| k)
            .collect();
        assert_eq!(calls.len(), 2, "one strand per parallel relation");
        for (i, &k) in calls.iter().enumerate() {
            assert_eq!(v.edge_fan[k], (i as f32, 2.0), "strand {i} fan");
        }
    }

    /// The paint cull: an edge wholly outside the canvas is skipped, but a
    /// fanned strand whose bow crosses the canvas is NOT, even when both of its
    /// endpoints are outside.
    #[test]
    fn edge_culling_skips_only_shapes_that_cannot_touch_the_canvas() {
        let mut v = graph();
        v.view_mode = ViewMode::Full;
        v.canvas_w = 800.0;
        v.canvas_h = 600.0;
        v.rebuild_view();
        v.screen_pos = vec![(0.0, 0.0); v.vis.len()];
        v.edge_fan = vec![(0.0, 1.0); v.edges.len()];
        let cull = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))
            .expand(EDGE_CULL_MARGIN);
        let (a, b, _) = v.edges[0];
        // Both endpoints far above the canvas: a straight segment cannot reach
        // it, so the shape is culled.
        v.screen_pos[a] = (300.0, -300.0);
        v.screen_pos[b] = (500.0, -200.0);
        assert!(!v.edge_in_view(0, a, b, cull), "straight edge above the view");
        // The same endpoints with a large parallel-fan bow put the control
        // point inside the canvas; culling on endpoints alone would lose it.
        v.edge_fan[0] = (20.0, 21.0);
        v.settings.curvature = 0.2;
        assert!(
            v.edge_in_view(0, a, b, cull),
            "a bowed strand must survive the cull"
        );
    }

    /// A node that holds no position at all is not "at the origin": it is
    /// unplaced, and `rebuild_view` mints every node that way.
    #[test]
    fn a_node_with_no_position_is_unplaced_not_at_the_origin() {
        let v = graph();
        assert!(v.vis.iter().all(|n| n.pos == (0.0, 0.0)));
        // `seed_branch` reports what the next run would do, and it must say
        // "re-seed": there is nothing on this canvas to continue from.
        assert_eq!(v.seed_branch(), SeedContinuity::Reseed);
    }

    /// THE DEFECT, stated as a test: expanding a node must leave the nodes that
    /// were already on the canvas exactly where they were.
    ///
    /// Before this change `relayout` always seeded from
    /// `graphviz::seed_positions(n)`, so `n` growing from 1 to 3 moved *every*
    /// node — including the namespace the user had just double-clicked. This
    /// asserts the positions, not that some function was called, so it cannot
    /// pass against a layout that merely re-runs.
    #[test]
    fn expanding_keeps_the_preexisting_nodes_where_they_were() {
        let mut v = laid_out();
        let before = pos_of(&v, "NZK");
        assert!(v.vis.len() == 1);

        v.expand_node(0); // reveal NZK.A and NZK.B
        // The gesture ends with a layout: `expand_node` rebuilds the visible
        // set, and the next frame's `ui()` sees `layout_dirty` and relayouts.
        // These tests take that second step explicitly rather than pretending
        // the positions appear on their own.
        v.relayout(&measured_ctx());
        let after = pos_of(&v, "NZK");

        assert!(
            moved(before, after) < 1e-9,
            "the expanded node must not move: was {before:?}, now {after:?} \
             (a non-zero delta here is the re-seed defect)"
        );
        // ...and the nodes that DID arrive are somewhere real, not piled on the
        // root: the arrangement grew rather than only gaining slots.
        let a = pos_of(&v, "NZK.A");
        let b = pos_of(&v, "NZK.B");
        assert!(moved(a, before) > 1.0, "NZK.A must be placed away from the root: {a:?}");
        assert!(moved(b, before) > 1.0, "NZK.B must be placed away from the root: {b:?}");
        assert!(moved(a, b) > 1.0, "the two new chips must not coincide");
    }

    /// A second, deeper expansion keeps the first expansion's nodes still too:
    /// the rule is "everything already placed stays", not "the root stays".
    #[test]
    fn a_second_expansion_also_keeps_the_first_level_still() {
        let mut v = laid_out();
        expand_and_settle(&mut v, 0);
        let ns = pos_of(&v, "NZK");
        let a = pos_of(&v, "NZK.A");
        let b = pos_of(&v, "NZK.B");

        expand_and_settle(&mut v, 1); // NZK.A -> NZK.A.m

        assert!(moved(pos_of(&v, "NZK"), ns) < 1e-9, "the namespace drifted");
        assert!(moved(pos_of(&v, "NZK.A"), a) < 1e-9, "the expanded class drifted");
        assert!(moved(pos_of(&v, "NZK.B"), b) < 1e-9, "the untouched sibling drifted");
        let m = pos_of(&v, "NZK.A.m");
        assert!(moved(m, a) > 1.0, "the new member must be placed off its class: {m:?}");
    }

    /// The other half of the rule: an explicit re-settle re-seeds, so the
    /// arrangement genuinely changes. Without this, "keep everything still"
    /// would also be satisfied by a layout that never runs at all — and the
    /// two branches would be indistinguishable.
    #[test]
    fn an_explicit_resettle_does_change_the_arrangement() {
        let mut v = laid_out();
        expand_and_settle(&mut v, 0);
        let ns = pos_of(&v, "NZK");
        let a = pos_of(&v, "NZK.A");

        // `R` / `#resettleBtn`: `actions.rs` sets `rebuild_view` + `relayout`,
        // and `reseed_layout` is the hook that makes it a hard reset.
        v.reseed_layout();
        assert_eq!(v.seed_branch(), SeedContinuity::Reseed);
        v.relayout(&measured_ctx());

        assert!(
            moved(pos_of(&v, "NZK"), ns) > 1e-9,
            "a re-settle must lay the set out from the disc, not continue"
        );
        assert!(
            moved(pos_of(&v, "NZK.A"), a) > 1e-9,
            "a re-settle moves the free nodes too"
        );
        // The override is one-shot: the NEXT run is decided from the data again.
        assert_eq!(v.seed_branch(), SeedContinuity::Continue);
    }

    /// A plain relayout with no override, run over an unchanged node set,
    /// continues: nothing was added, so the hidden re-seed defect is impossible
    /// here. This is the incremental expand path (`graph.js expandNode`), NOT
    /// what `R` / `#resettleBtn` does — that action sets `seed_override` through
    /// [`GraphView::reseed_layout`], because otherwise it anchors every node and
    /// moves nothing (the "Re-settle does nothing" defect).
    #[test]
    fn a_relayout_over_an_unchanged_set_continues_and_moves_nothing() {
        let mut v = laid_out();
        expand_and_settle(&mut v, 0);
        let ns = pos_of(&v, "NZK");
        let a = pos_of(&v, "NZK.A");
        assert_eq!(v.seed_branch(), SeedContinuity::Continue);

        v.relayout(&measured_ctx());

        assert!(moved(pos_of(&v, "NZK"), ns) < 1e-9, "the root drifted on a no-op run");
        assert!(moved(pos_of(&v, "NZK.A"), a) < 1e-9, "a child drifted on a no-op run");
    }

    /// A pinned node never moves, whichever branch the layout takes — the
    /// promise `shell.js:467`'s comment makes ("pinned nodes stay pinned").
    #[test]
    fn a_pinned_node_survives_both_a_continue_and_a_reseed() {
        let mut v = laid_out();
        expand_and_settle(&mut v, 0);
        v.set_pinned(0, true); // pin NZK
        let ns = pos_of(&v, "NZK");
        assert_eq!(v.seed_branch(), SeedContinuity::Continue);
        v.relayout(&measured_ctx());
        assert!(moved(pos_of(&v, "NZK"), ns) < 1e-9, "pinned node moved on a continue run");

        v.reseed_layout();
        v.relayout(&measured_ctx());
        assert!(
            moved(pos_of(&v, "NZK"), ns) < 1e-9,
            "a pinned node must survive a re-seed; that is what the pin is for"
        );
    }

    /// Collapsing is the gesture after which the arrangement really is gone, so
    /// the next run re-seeds rather than continuing from a set that no longer
    /// contains what was on screen. `rebuild_view` clears every position, which
    /// is exactly how `relayout` learns this — no flag for a caller to forget.
    #[test]
    fn collapsing_reopens_the_door_for_a_reseed() {
        let mut v = laid_out();
        expand_and_settle(&mut v, 0);
        assert_eq!(v.seed_branch(), SeedContinuity::Continue);

        v.collapse_node(0);

        assert_eq!(v.vis.len(), 1, "the namespace is alone again");
        assert_eq!(
            v.seed_branch(),
            SeedContinuity::Reseed,
            "a collapse rebuilds the visible set from nothing, so the next run re-seeds"
        );
    }

    /// `seed_new_children` puts a newly revealed node beside the node that
    /// revealed it, and says which reference strategy did it — the spline when
    /// the root's placed neighbours already form a line, the fan otherwise.
    #[test]
    fn new_children_are_placed_beside_their_root_by_the_fan_or_the_spline() {
        let mut v = laid_out();
        v.expand_node(0); // NZK -> A, B

        let sizes: Vec<(f32, f32)> = v.vis.iter().map(|n| n.size).collect();
        let placed: Vec<Option<(f64, f64)>> = v.vis.iter().map(|n| Some(n.pos)).collect();
        let seed: Vec<(f64, f64)> = v.vis.iter().map(|n| n.pos).collect();

        // Add a brand-new node with no position, attached to slot 0 (the
        // namespace), exactly as an expansion presents it.
        let root = v.vis[0].pos;
        let child = graphviz::seed_children(
            root,
            HalfSize { hw: sizes[0].0 as f64 / 2.0, hh: sizes[0].1 as f64 / 2.0 },
            &placed,
            &[1, 2],
            graphviz::graph_center(&placed),
            &[ChildSpec { half: HalfSize { hw: 20.0, hh: 6.5 }, hash: 0.5 }],
            120.0,
        )
        .expect("a placed root with children must place them");

        assert_eq!(child.positions.len(), 1);
        let p = child.positions[0];
        assert!(moved(p, root) > 1.0, "the child must not land on the root: {p:?}");
        // Two neighbours on a line (A and B are opposite each other across the
        // root in this graph) snap to a 45° axis and get the spline; the fan is
        // the fallback when they do not.
        assert!(
            matches!(child.kind, Some(ChildPlacement::Spline) | Some(ChildPlacement::Fan)),
            "one of the two reference strategies must have placed it"
        );
        assert!(child.anchored, "a seeded child is held still by the run that follows");
        // The seed is only a starting point: it is still up to the caller's run.
        assert_eq!(seed.len(), placed.len());
    }

    /// The fan never drops a chip on top of another one in the same run, and it
    /// always clears the root's measured box — the `seedFan` contract that the
    /// old fixed 34..74 spiral ignored.
    #[test]
    fn the_fan_separates_siblings_and_clears_the_root() {
        let root = (0.0, 0.0);
        let root_half = HalfSize { hw: 5.0, hh: 5.0 };
        let big = ChildSpec { half: HalfSize { hw: 60.0, hh: 6.5 }, hash: 0.37 };
        let placement = graphviz::seed_children(
            root,
            root_half,
            &[],
            &[],
            (0.0, 0.0),
            &[big, big, big, big],
            120.0,
        )
        .expect("a placed root must fan its children");

        assert_eq!(placement.positions.len(), 4);
        for (i, p) in placement.positions.iter().enumerate() {
            // every chip clears a wide sibling's box, not just its centre
            assert!(graphviz::chip_support(root_half, p.0, p.1) > 0.0);
            let d = (p.0 * p.0 + p.1 * p.1).sqrt();
            assert!(d > graphviz::SPLINE_GAP, "chip {i} at {p:?} sits inside the root");
            for q in &placement.positions[i + 1..] {
                let sep = (p.0 - q.0).abs() >= 2.0 * big.half.hw + graphviz::SPLINE_GAP
                    || (p.1 - q.1).abs() >= 2.0 * big.half.hh + graphviz::SPLINE_GAP;
                assert!(sep, "chips {p:?} and {q:?} overlap");
            }
        }
    }

    /// The line detector is what decides spline vs fan, so its rejections matter:
    /// one neighbour is not a line, and a scatter is not a line.
    #[test]
    fn only_a_snapped_run_of_two_or_more_neighbours_is_a_line() {
        // one neighbour: not a line (graph.js `n < 2`)
        assert!(graphviz::neighbour_line(&[(100.0, 0.0)]).is_none());
        // two neighbours due east: a horizontal line, pointing away from the root
        let (ux, uy) = graphviz::neighbour_line(&[(100.0, 0.0), (200.0, 0.0)]).unwrap();
        assert!((ux - 1.0).abs() < 1e-9 && uy.abs() < 1e-9, "got ({ux},{uy})");
        // a diagonal snaps to 45°
        let (ux, uy) = graphviz::neighbour_line(&[(100.0, 100.0), (200.0, 200.0)]).unwrap();
        assert!((ux.abs() - uy.abs()).abs() < 1e-9, "got ({ux},{uy})");
        // a blob with no run is rejected
        assert!(graphviz::neighbour_line(&[(5.0, 5.0), (6.0, 5.0)]).is_none());
        // spread off the run is rejected (a 45° axis cannot explain these)
        assert!(graphviz::neighbour_line(&[(100.0, 0.0), (200.0, 90.0)]).is_none());
    }

    /// Acceptance 4: `slot_of` and `edges` describe the SAME visible set after
    /// expansion — no stale slot indices. This is the invariant the paint path
    /// depends on, because `edges` addresses `vis` by slot.
    #[test]
    fn slots_and_edges_stay_consistent_after_expansion() {
        let mut v = graph();
        for step in [0usize, 1, 3] {
            v.expand_node(step);

            // `slot_of` is a bijection onto 0..vis.len() named by `vis` itself.
            assert_eq!(v.slot_of.len(), v.vis.len(), "one slot per visible node");
            assert_eq!(visible_slots(&v), (0..v.vis.len()).collect::<HashSet<usize>>());
            for (slot, nv) in v.vis.iter().enumerate() {
                assert_eq!(nv.slot, slot, "NodeVis::slot must match its position");
                assert_eq!(v.slot_of.get(&nv.index), Some(&slot));
            }

            // Every visible edge addresses two live slots, and its recorded
            // `edge_index` still names the same relation in the real graph.
            for &(a, b, ei) in &v.edges {
                assert!(a < v.vis.len() && b < v.vis.len(), "edge slot in range");
                assert_ne!(a, b, "self-loops are dropped");
                assert_eq!(v.slot_of.get(&v.graph.edges[ei].from), Some(&a));
                assert_eq!(v.slot_of.get(&v.graph.edges[ei].to), Some(&b));
            }
        }
    }

    /// Collapsing prunes history entries that are no longer visible, so Back can
    /// never jump to a node that is not on the canvas.
    #[test]
    fn collapsing_prunes_history_of_nodes_that_left_the_canvas() {
        let mut v = graph();
        v.expand_node(0);
        v.expand_node(1); // brings in NZK.A.m
        v.select(Some(3)); // select the method: history gets nothing (was None)
        v.select(Some(1)); // now history records 3
        assert_eq!(v.history.last(), Some(&3));

        v.collapse_node(1); // NZK.A.m is no longer visible
        assert!(!v.slot_of.contains_key(&3));
        assert!(
            !v.history.contains(&3),
            "an invisible node must not stay in the back stack"
        );
    }

    /// Back returns to the previously selected node and re-expands it, which is
    /// `interaction.js navigateBack`.
    #[test]
    fn back_returns_to_the_previous_selection_and_re_expands() {
        let mut v = graph();
        v.expand_node(0);
        v.expand_node(1);
        v.select(Some(0));
        v.select(Some(1)); // history = [0]

        let back = v.navigate_back();
        assert_eq!(back, Some(0));
        assert_eq!(v.selected, Some(0));

        // With nothing left, Back is honestly empty rather than a made-up target.
        assert_eq!(v.navigate_back(), None);
        assert_eq!(v.selected, Some(0), "an empty history must not move the selection");

        // The failed Back above must not have pushed a Forward destination it
        // never actually left from: `forward` still holds only the node left by
        // the successful Back.
        assert_eq!(v.forward, vec![1]);
    }

    /// Back then Forward returns to the same node, and the two can be
    /// alternated: `nav.js:138 forward()`.
    #[test]
    fn back_then_forward_returns_to_the_same_node() {
        let mut v = graph();
        v.expand_node(0);
        v.expand_node(1);
        v.select(Some(0));
        v.select(Some(1)); // history = [0], forward = []

        assert_eq!(v.navigate_back(), Some(0));
        assert_eq!(v.selected, Some(0));
        assert_eq!(v.navigate_forward(), Some(1), "forward retraces the back step");
        assert_eq!(v.selected, Some(1));
        // Forward recorded 0 on the back stack, so Back works again.
        assert_eq!(v.navigate_back(), Some(0));
        assert_eq!(v.navigate_forward(), Some(1));
    }

    /// Forward with nothing behind it is honestly `None` and moves nothing.
    #[test]
    fn forward_on_an_empty_stack_is_none() {
        let mut v = graph();
        v.expand_node(0);
        v.select(Some(0));
        v.select(Some(1));

        assert_eq!(v.navigate_forward(), None);
        assert_eq!(v.selected, Some(1), "an empty forward stack must not move the selection");
    }

    /// A fresh selection invalidates the Forward stack, so Forward cannot
    /// resurrect the branch the user abandoned.
    #[test]
    fn a_new_selection_invalidates_the_forward_stack() {
        let mut v = graph();
        v.expand_node(0);
        v.expand_node(1);
        v.select(Some(0));
        v.select(Some(1));
        assert_eq!(v.navigate_back(), Some(0)); // forward = [1]

        v.select(Some(3)); // a genuinely new selection
        assert_eq!(
            v.navigate_forward(),
            None,
            "a new selection must clear the forward stack"
        );
        assert_eq!(v.selected, Some(3));
    }

    /// Up walks to the real containment parent; a namespace has none, and that is
    /// reported as `None` rather than guessed.
    #[test]
    fn up_walks_to_the_containment_parent_and_is_empty_at_the_root() {
        let mut v = graph();
        v.expand_node(0);
        v.expand_node(1);

        v.select(Some(3)); // the method NZK.A.m
        assert_eq!(v.parent_of(3), Some(1), "NZK.A contains NZK.A.m");
        assert_eq!(v.navigate_up(), Some(1), "Up goes to the declaring class");
        assert_eq!(v.selected, Some(1));
        assert_eq!(v.navigate_up(), Some(0), "then to the namespace");

        // The namespace is the root of containment: honest `None`, no invention.
        assert_eq!(v.parent_of(0), None);
        assert_eq!(v.navigate_up(), None);
        assert_eq!(v.selected, Some(0));
    }

    /// The advertised gesture must be reachable, and the advertised relations
    /// must be the ones actually pulled. This is the regression guard for the
    /// original defect: the UI printed a promise nothing implemented, so the
    /// promise is now derived from the same data the gesture uses.
    #[test]
    fn the_advertised_hint_matches_what_a_double_click_pulls() {
        let mut v = graph();

        // A member advertises calling edges, and pulling it really does add the
        // far end of a `calls` edge.
        assert_eq!(v.pull_kinds_for(3), &["contains", "calls"]);
        v.expand_node(0);
        v.expand_node(3);
        assert!(visible_ids(&v).contains(&"NZK.B.k"));

        // A type-like node advertises base types, and really pulls `inherits`.
        assert_eq!(v.pull_kinds_for(0), &["contains", "inherits"]);
        assert_eq!(v.pull_kinds_for(1), &["contains", "inherits"]);
    }

    /// Out-of-range indices are ignored rather than panicking — a click can
    /// arrive against a `vis` from an earlier frame.
    #[test]
    fn out_of_range_nodes_are_ignored_not_panicked() {
        let mut v = graph();
        let n = v.vis.len();
        assert!(v.expand_node(999).is_empty());
        v.collapse_node(999);
        v.toggle_node(999);
        assert_eq!(v.vis.len(), n);
    }

    /// `NATIVE_CSVIEWER_VIEW=full` is the A/B evidence path and must keep
    /// showing the whole model with its whole edge set — expansion is a
    /// namespace-mode feature and must not disturb it.
    #[test]
    fn full_mode_still_shows_the_whole_model_regardless_of_expansion() {
        let mut v = graph();
        v.view_mode = ViewMode::Full;
        v.rebuild_view();
        assert_eq!(v.vis.len(), 5, "Full shows every node");
        // All four `contains` edges plus the `calls` edge. No self-loops exist in
        // this fixture, so every edge qualifies — this is the whole model, which
        // is what makes Full the A/B evidence path for curves and pills.
        assert_eq!(v.edges.len(), 5, "and every edge with both endpoints present");

        // Expanding a node cannot change the Full view's node set.
        v.expand_node(0);
        assert_eq!(v.vis.len(), 5);
        assert_eq!(v.edges.len(), 5);
    }

    // ------------------------------------------------------------------
    // `#hideIsolated` / `#hideEdgeLabels` (task-1)
    // ------------------------------------------------------------------

    /// A Full-mode fixture with one node that has no edge at all.
    fn graph_with_isolated() -> GraphView {
        let mut v = GraphView::new(Graph {
            repo: PathBuf::from("/tmp/view-isolated-test"),
            nodes: vec![
                node("NZK.A", "class"),
                node("NZK.B", "class"),
                node("NZK.C", "method"), // no edge at all → isolated
            ],
            edges: vec![edge(0, 1, "contains")],
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        });
        v.view_mode = ViewMode::Full;
        v.rebuild_view();
        v
    }

    /// `#hideIsolated` drops exactly the nodes with no drawn edge, and turning it
    /// back off restores them. This is `graph.js:814-820`'s two-pass filter; a
    /// no-op flag would leave `NZK.C` visible in the first assertion.
    // ------------------------------------------------------------------
    // focus filters ("Only related nodes" / "Hide parents")
    // ------------------------------------------------------------------

    /// "Hide parents" removes exactly the containment ancestor chain of the
    /// selection — not `calls`/`uses` neighbours — and leaves the rest of the
    /// canvas alone.
    #[test]
    fn hide_parents_drops_the_containment_ancestors_of_the_selection() {
        let mut v = graph();
        v.view_mode = ViewMode::Full;
        v.selected = Some(3); // NZK.A.m, whose parents are NZK.A then NZK
        v.rebuild_view();
        assert_eq!(v.vis.len(), 5);

        let mut f = crate::actions::FilterState::default();
        f.focus_parents = true;
        assert!(v.set_filters(&f), "the flag must rebuild the view");
        assert_eq!(
            visible_ids(&v),
            ["NZK.B", "NZK.A.m", "NZK.B.k"],
            "the parent chain (NZK, NZK.A) is hidden"
        );
        // `NZK.A` -> `NZK.A.m` follows the hidden parent; the `calls` edge from
        // the still-visible method survives because its endpoints are visible.
        assert_eq!(v.edges.len(), 2, "only edges with visible endpoints survive");

        // Turning it off restores the same nodes without a re-index.
        f.focus_parents = false;
        assert!(v.set_filters(&f));
        assert_eq!(v.vis.len(), 5);
    }

    /// "Only related nodes" keeps the selected node and every node it shares an
    /// edge with, drops everything two hops away, and follows the selection
    /// instead of freezing on the node that was selected when it was pressed.
    #[test]
    fn only_relations_isolates_the_selection_and_follows_it() {
        let mut v = graph();
        v.view_mode = ViewMode::Full;
        v.selected = Some(3); // NZK.A.m: contains-in from NZK.A, calls to NZK.B.k
        v.rebuild_view();

        let mut f = crate::actions::FilterState::default();
        f.focus_only = true;
        assert!(v.set_filters(&f));
        assert_eq!(
            visible_ids(&v),
            ["NZK.A.m", "NZK.A", "NZK.B.k"],
            "the node and its direct relations survive; NZK and NZK.B do not"
        );
        assert_eq!(
            v.edges.len(),
            2,
            "only the two edges incident to the focused node survive"
        );
        assert!(
            !visible_ids(&v).contains(&"NZK.B"),
            "a node related only to a neighbour is still out"
        );

        // Advancing the selection is a real transition, applied by the per-frame
        // sync rather than by a filter press.
        v.selected = Some(4); // NZK.B.k: contains-in from NZK.B, calls from NZK.A.m
        assert!(v.sync_focus_selection(), "selection moved: rebuild");
        assert_eq!(visible_ids(&v), ["NZK.B.k", "NZK.B", "NZK.A.m"]);
        assert!(!v.sync_focus_selection(), "steady selection is a no-op");

        // Disabling the filter restores the whole model.
        let off = crate::actions::FilterState::default();
        assert!(v.set_filters(&off));
        assert_eq!(v.vis.len(), 5);
    }

    /// The focus is an explicit edge pass: in the shipped namespace view nothing
    /// is expanded, yet pressing "Only related nodes" must still draw the
    /// focused node's edges rather than leave its neighbours as floating chips.
    #[test]
    fn only_relations_draws_edges_before_any_expansion() {
        let mut v = graph(); // default Namespaces view, `expanded` empty
        v.selected = Some(3);
        v.rebuild_view();

        let mut f = crate::actions::FilterState::default();
        f.focus_only = true;
        assert!(v.set_filters(&f));
        assert_eq!(visible_ids(&v), ["NZK.A.m", "NZK.A", "NZK.B.k"]);
        assert_eq!(
            v.edges.len(),
            2,
            "the focused node's contains and calls relations are drawn"
        );
    }

    /// Turning "Only related nodes" back OFF restores the whole model to a laid-out
    /// canvas, not to a pile at the origin.
    ///
    /// This is the "only showing one thing" defect: while the filter was on,
    /// every other node left `vis`; turning it off grows the set without losing
    /// anything, so the continue branch runs. The returning nodes are restored
    /// from the id-keyed position store and then de-overlapped, so they come back
    /// where the user last saw them instead of all landing at (0, 0) — the pile
    /// where every chip is drawn on top of every other.
    #[test]
    fn turning_off_only_relations_lays_the_returning_set_out_again() {
        let mut v = laid_out();
        v.expand_node(0);
        v.relayout(&measured_ctx());
        v.selected = Some(1); // NZK.A

        let mut on = crate::actions::FilterState::default();
        on.focus_only = true;
        assert!(v.set_filters(&on));
        v.relayout(&measured_ctx());
        assert_eq!(
            v.vis.len(),
            3,
            "the filter really isolates the selection and its relations"
        );

        // The gesture the user makes: press "Show all nodes".
        let off = crate::actions::FilterState::default();
        assert!(v.set_filters(&off));
        v.relayout(&measured_ctx());

        assert!(v.vis.len() > 1, "the whole model is back");
        let mut seen: Vec<(f64, f64)> = Vec::new();
        for slot in &v.vis {
            let p = slot.pos;
            assert!(
                p != (0.0, 0.0),
                "{} came back at the origin — nodes are piled",
                v.graph.nodes[slot.index].id
            );
            assert!(
                !seen.iter().any(|q| moved(*q, p) < 1.0),
                "{} overlaps another returning node at {p:?}",
                v.graph.nodes[slot.index].id
            );
            seen.push(p);
        }
    }

    /// A continue run may not have a placed root for every new node (a deeper
    /// expansion, or a node the position store has never seen). It must re-seed
    /// rather than leave that node at the origin, overlapping every other chip.
    #[test]
    fn a_continue_run_with_an_unplaceable_new_node_reseeds_instead_of_piling() {
        let mut v = laid_out();
        // Depth 2 puts the methods on the canvas through their classes, which are
        // themselves new in this gesture: the methods have no PLACED root, so the
        // child seed cannot place them.
        assert!(v.set_expansion_depth(2));
        v.expand_node(0);
        v.relayout(&measured_ctx());

        assert!(v.vis.len() > 3, "the fixture really grew past one level");
        let mut seen: Vec<(f64, f64)> = Vec::new();
        for slot in &v.vis {
            let p = slot.pos;
            assert_ne!(
                p,
                (0.0, 0.0),
                "{} was left at the origin",
                v.graph.nodes[slot.index].id
            );
            assert!(
                !seen.iter().any(|q| moved(*q, p) < 1.0),
                "{} overlaps another node at {p:?}",
                v.graph.nodes[slot.index].id
            );
            seen.push(p);
        }
    }

    /// A re-index swaps the graph under the view. The old `vis` indices address
    /// the old node list, so adopting positions must go through the id-keyed
    /// store: a shortened graph must not panic, and a survivor must keep its
    /// coordinates.
    #[test]
    fn install_graph_carries_positions_by_id_across_a_different_node_list() {
        let mut v = laid_out();
        v.expand_node(0);
        v.relayout(&measured_ctx());
        let a = pos_of(&v, "NZK.A");
        assert!(v.vis.len() > 2, "the fixture has indices the new graph lacks");

        v.install_graph(Graph {
            repo: PathBuf::from("/tmp/view-install-graph-test"),
            nodes: vec![node("NZK.A", "class"), node("NZK.B", "class")],
            edges: vec![edge(0, 1, "uses")],
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        });

        assert_eq!(visible_ids(&v), ["NZK.A", "NZK.B"]);
        assert!(
            v.vis.iter().all(|n| n.index < v.graph.nodes.len()),
            "no slot may still address the old node list"
        );
        assert_eq!(pos_of(&v, "NZK.A"), a, "the surviving id keeps its position");
    }

    /// The two focus flags are alternatives in the actions layer, so the view
    /// never has to guess which one wins; `set_filters` still compares both.
    #[test]
    fn focus_flags_are_part_of_the_filter_transition() {
        let mut v = graph();
        let mut f = crate::actions::FilterState::default();
        assert!(!v.set_filters(&f));
        f.focus_only = true;
        assert!(v.set_filters(&f), "a new focus value is a real transition");
        assert!(!v.set_filters(&f), "repeating it is not");
        f.focus_only = false;
        assert!(v.set_filters(&f));
    }

    #[test]
    fn hide_isolated_drops_only_nodes_with_no_drawn_edge() {
        let mut v = graph_with_isolated();
        assert_eq!(v.vis.len(), 3, "every node is visible by default");
        assert!(visible_ids(&v).contains(&"NZK.C"));

        let mut f = crate::actions::FilterState::default();
        f.hide_isolated = true;
        assert!(v.set_filters(&f), "the flag must rebuild the view");
        assert_eq!(visible_ids(&v), ["NZK.A", "NZK.B"], "NZK.C is isolated");
        assert_eq!(v.edges.len(), 1, "the surviving edge keeps both endpoints");

        // Turning it off restores the isolated node with no other state sticking.
        let f = crate::actions::FilterState::default();
        assert!(v.set_filters(&f));
        assert_eq!(v.vis.len(), 3);
        assert!(visible_ids(&v).contains(&"NZK.C"));
    }

    /// `#hideEdgeLabels` is a paint-time pill gate, but its state must travel
    /// through the same `set_filters` comparison as the other filter fields —
    /// otherwise a press would change `NzkUiState` and never reach the view.
    #[test]
    fn vt_mode_is_part_of_the_filter_transition() {
        let mut v = graph_with_isolated();
        let mut f = crate::actions::FilterState::default();
        assert!(!v.vt_mode, "core.js ships it false");
        f.vt_mode = true;
        assert!(v.set_filters(&f), "a new filter value is a real transition");
        assert!(v.vt_mode);
        // Calling again with the same value is not a transition (no rebuild).
        assert!(!v.set_filters(&f));
        // Default restores it.
        assert!(v.set_filters(&crate::actions::FilterState::default()));
        assert!(!v.vt_mode);
    }

    /// The hide filter is absolute: not even the focused edge keeps its word
    /// box. Before this gate, `hide_edge_labels` only disabled `label_all`, so
    /// `touches` still forced a pill for the hovered/selected edge and the
    /// checkbox looked like it had done nothing.
    #[test]
    fn the_relation_label_gate_selects_the_presentation() {
        // VT mode: the relation node IS the relation, so it paints at every
        // zoom. The dense-canvas thinning still applies, and a touched edge is
        // always kept.
        assert!(edge_label_visible(true, false, false, false));
        assert!(edge_label_visible(true, true, false, false));
        assert!(!edge_label_visible(true, false, false, true));
        assert!(edge_label_visible(true, false, true, true));
        assert!(edge_label_visible(true, true, true, true));

        // Word-box mode: the reference's zoom gate plus focus/density rules.
        assert!(edge_label_visible(false, true, false, false));
        assert!(edge_label_visible(false, false, true, false));
        assert!(!edge_label_visible(false, false, false, false));
        assert!(!edge_label_visible(false, true, false, true));
        assert!(edge_label_visible(false, false, true, true));
    }

    /// VT mode's edge is a direct chord: the fan's control point collapses and
    /// the relation anchor moves to the midpoint of the straight segment.
    #[test]
    fn vt_mode_straightens_the_edge_and_centres_its_relation() {
        let mut d = graphviz::EdgeDraw {
            x1: 10.0,
            y1: 20.0,
            x2: 110.0,
            y2: 60.0,
            mx: 30.0,
            my: 90.0,
            curved: true,
            width: 1.0,
            dash: None,
            alpha: 1.0,
            color: graphviz::Rgba::rgb(1, 2, 3),
            arrow: None,
            pill: Some((30.0, 90.0)),
        };
        vt_straighten(&mut d);
        assert!(!d.curved, "the stroke must be a straight segment");
        assert_eq!((d.mx, d.my), (60.0, 40.0), "control point = chord midpoint");
        assert_eq!(d.pill, Some((60.0, 40.0)), "the relation sits on the chord");
    }

    /// End-to-end paint proof for the repurposed switch: with full-mode calls
    /// edges on screen, "Calling" is a real painted text shape in BOTH modes.
    /// VT mode does not hide the relation — it moves it from a floating word
    /// box onto the line as a relation node — so the word must survive. The
    /// helper's truth table above is the unit half; this is the half that would
    /// stay green if the paint loop forgot to call it.
    #[test]
    fn the_canvas_paints_the_relation_in_both_presentations() {
        fn collect(shape: &egui::Shape, out: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(t) => out.push(t.galley.text().to_string()),
                egui::Shape::Vec(v) => {
                    for s in v {
                        collect(s, out);
                    }
                }
                _ => {}
            }
        }
        fn painted_texts(ctx: &egui::Context, v: &mut GraphView) -> Vec<String> {
            let raw = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    Pos2::ZERO,
                    Vec2::new(800.0, 600.0),
                )),
                ..Default::default()
            };
            let mut out = ctx.run_ui(raw, |ui| v.ui(ui));
            out.textures_delta.clear();
            let mut texts = Vec::new();
            for clipped in &out.shapes {
                collect(&clipped.shape, &mut texts);
            }
            texts
        }

        let ctx = measured_ctx();
        let mut v = graph();
        v.view_mode = ViewMode::Full;
        v.rebuild_view();
        // One pass settles the layout and paints from the settled camera; the
        // second is the frame under test.
        let _ = painted_texts(&ctx, &mut v);
        let shown = painted_texts(&ctx, &mut v);
        assert!(
            shown.iter().any(|t| t == "Calling"),
            "precondition: the calls edge paints its relation word, got {shown:?}"
        );

        // VT mode: the same relation is still named, now by an on-line node.
        v.vt_mode = true;
        let vt = painted_texts(&ctx, &mut v);
        assert!(
            vt.iter().any(|t| t == "Calling" || t == "Children"),
            "VT mode must keep the relation as a node, got {vt:?}"
        );
    }

    /// A click selects the chip the user can SEE on top. `paint_canvas` sorts
    /// selected chips last; picking from slot order instead returned the chip
    /// painted underneath whenever two boxes overlapped (a filtered view can
    /// leave them visually stacked into what looks like one node).
    #[test]
    fn picking_an_overlap_returns_the_chip_painted_on_top() {
        let mut v = expanded_view();
        assert!(v.vis.len() >= 2, "need two chips for an overlap");
        // Stack every visible chip on one point, as a filtered/regrouped view
        // can, so every pick at that point is an overlap.
        let p = v.vis[0].pos;
        for n in &mut v.vis {
            n.pos = p;
            n.unplaced = false;
        }
        let (cx, cy) = v.world_to_screen(p);
        let at = Pos2::new(cx, cy);

        // Nothing selected: the last slot is painted last, so it wins.
        v.selected = None;
        v.hovered = None;
        let last = v.vis.last().unwrap().index;
        assert_eq!(v.pick(at), Some(last));

        // Select the FIRST slot. The painter now draws it last, on top; the
        // click must return the selected node, not the slot-order winner.
        let first = v.vis[0].index;
        v.select(Some(first));
        v.hovered = None;
        assert_eq!(
            v.pick(at),
            Some(first),
            "pick must follow paint order, not reverse slot order"
        );

        // Focusing a neighbour (not the selected chip) also ranks below the
        // selected chip, so moving the pointer cannot make the click target
        // disagree with the selected border.
        v.hovered = Some(v.vis[1].index);
        assert_eq!(v.pick(at), Some(first));
    }

    /// The real corpus, not a fixture: expansion against the live index.
    ///
    /// The synthetic graph above proves the arithmetic; this proves the engine's
    /// actual edge kinds and direction drive it correctly. `CSVIEWER_REPO`
    /// selects the corpus — the same variable `resolve_repo` reads — so the test
    /// exercises real data when one is pointed at and skips honestly otherwise.
    #[test]
    fn expansion_works_on_the_live_index_when_one_is_available() {
        if std::env::var("CSVIEWER_REPO").unwrap_or_default().is_empty() {
            eprintln!(
                "[view] CSVIEWER_REPO unset; skipping live-corpus test \
                 (point it at a checkout with C# sources to exercise this)"
            );
            return;
        }
        let repo = crate::model::resolve_repo(None);
        let cfg = crate::model::load_config(&repo);
        let roots = crate::model::resolve_roots(&repo, &[], &cfg);
        let Ok((graph, _)) = crate::model::load_with_index(&repo, &roots) else {
            eprintln!("[view] no index at {}; skipping live-corpus test", repo.display());
            return;
        };
        let namespaces = graph.nodes.iter().filter(|n| n.kind == "namespace").count();
        if namespaces == 0 {
            eprintln!("[view] live index at {} has no namespaces; skipping", repo.display());
            return;
        }

        let mut v = GraphView::new(graph);
        assert_eq!(v.vis.len(), namespaces, "namespace-first start: exactly the namespaces");
        let before = v.vis.len();

        // Expand the first namespace (sorted order, same as the visible order).
        let root = v.vis[0].index;
        let added = v.expand_node(root);
        assert!(!added.is_empty(), "a real namespace contains real children");
        assert!(v.vis.len() > before, "expansion must grow the live view");
        eprintln!(
            "[view] live expansion: {} -> {} nodes ({} added), {} edges",
            before,
            v.vis.len(),
            added.len(),
            v.edges.len()
        );

        // The same slot/edge invariant the fixture asserts, on real data.
        assert_eq!(v.slot_of.len(), v.vis.len());
        for &(a, b, ei) in &v.edges {
            assert!(a < v.vis.len() && b < v.vis.len());
            assert_eq!(v.slot_of.get(&v.graph.edges[ei].from), Some(&a));
            assert_eq!(v.slot_of.get(&v.graph.edges[ei].to), Some(&b));
        }

        // And collapsing returns exactly to the documented initial state.
        v.collapse_node(root);
        assert_eq!(v.vis.len(), namespaces, "collapse restores namespace-only");
        assert!(v.edges.is_empty(), "and the original view has no edges");
    }

    // -----------------------------------------------------------------------
    // (b) pinning
    // -----------------------------------------------------------------------

    /// The fixture ships namespace-only (exactly one visible node), so put the
    /// view into a multi-node state the way a user does: expand the namespace.
    /// Also gives the view a real canvas, so world->screen is meaningful.
    fn expanded_view() -> GraphView {
        let mut v = graph();
        v.rebuild_view();
        v.expand_node(0); // NZK -> NZK.A, NZK.B
        v.canvas_w = 800.0;
        v.canvas_h = 600.0;
        v.cam = Camera { zoom: 1.0, x: 0.0, y: 0.0 };
        v
    }

    /// A pin must do the two things it claims: set the painted flag, and exempt
    /// the node from re-settle movement.
    #[test]
    fn pinning_sets_the_painted_flag_and_survives_relayout() {
        let mut v = expanded_view();
        let target = v.vis[0].index;

        assert!(!v.is_pinned(target));
        assert!(!v.vis[0].fixed, "unpinned by default — the dot is not painted");

        assert!(v.toggle_pinned(target), "toggling on returns the new state");
        assert!(v.is_pinned(target));
        assert!(
            v.vis.iter().find(|n| n.index == target).unwrap().fixed,
            "NodeVis.fixed must follow the pinned set — this is the field that was \
             painted but never set"
        );

        // A re-settle is exactly what `shell.js:467` re-runs. The pinned node
        // must come out where it went in. `relayout` measures text, so it needs
        // a live `egui::Context` with fonts — hence the `run_ui` wrapper.
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |_ui| {});
        out.textures_delta.clear();

        let before = v.vis.iter().find(|n| n.index == target).unwrap().pos;
        v.layout_dirty = true;
        v.relayout(&ctx);
        let after = v.vis.iter().find(|n| n.index == target).unwrap().pos;
        assert_eq!(
            before, after,
            "a pinned node must not move when the layout re-settles"
        );
        assert!(
            v.vis.iter().find(|n| n.index == target).unwrap().fixed,
            "and it stays pinned through the relayout"
        );

        // The other nodes are still free: pinning one node must not freeze all.
        //
        // (`relayout` re-derives `fixed` from `pinned` via `sync_fixed_flags`,
        // so this also proves the unpinned ones were not swept up with it.)
        let free = v.vis.iter().filter(|n| !n.fixed).count();
        assert!(free > 0, "the rest of the graph is still free to move");
    }

    #[test]
    fn pinning_a_node_that_is_not_in_view_is_still_real() {
        let mut v = expanded_view();
        // Index 3 (`NZK.A.m`) is a member, not visible until its class expands.
        assert!(v.slot_of.get(&3).is_none(), "precondition: not currently visible");
        assert!(v.toggle_pinned(3), "pinning a hidden node is allowed");
        assert!(v.is_pinned(3));

        // It is a property of the node, so it comes back pinned when revealed.
        v.expand_node(1); // expand NZK.A -> pulls NZK.A.m
        let vis = v
            .vis
            .iter()
            .find(|n| n.index == 3)
            .expect("NZK.A.m is now visible");
        assert!(vis.fixed, "the pin applies as soon as the node is on canvas");
    }

    #[test]
    fn toggling_a_pin_off_clears_the_flag_and_out_of_range_is_refused() {
        let mut v = graph();
        v.rebuild_view();
        let target = v.vis[0].index;
        v.toggle_pinned(target);
        assert!(!v.toggle_pinned(target), "toggling off returns false");
        assert!(!v.is_pinned(target));
        assert!(!v.vis.iter().find(|n| n.index == target).unwrap().fixed);

        // Out of range: refused, and nothing is invented for a non-existent node.
        assert!(!v.set_pinned(9999, true));
        assert!(v.pinned_set().is_empty());
    }

    // -----------------------------------------------------------------------
    // (c) box-selection
    // -----------------------------------------------------------------------

    /// Layout the view so the chips occupy distinct positions.
    ///
    /// The fixture's initial `pos` is `(0,0)` for every node until `relayout`
    /// runs, so without this every chip is stacked at the origin and a small
    /// rectangle intersects all of them. `relayout` measures text and so needs a
    /// live `egui::Context` with fonts.
    fn laid_out_view() -> GraphView {
        let mut v = expanded_view();
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(1.0);
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |_ui| {});
        out.textures_delta.clear();
        v.layout_dirty = true;
        v.relayout(&ctx);
        // Positions are what matters; the camera stays at identity so
        // world->screen is a pure translate about the canvas centre.
        v
    }

    /// The marquee must capture exactly the chips it overlaps, and no others.
    #[test]
    fn marquee_selects_the_nodes_inside_it() {
        let v = laid_out_view();
        assert!(v.vis.len() >= 2, "need at least two visible nodes");
        // Precondition for the "one chip" case below: the layout gave distinct
        // positions, so a small rect cannot span two nodes.
        let distinct: HashSet<(i64, i64)> = v
            .vis
            .iter()
            .map(|n| (n.pos.0.to_bits() as i64, n.pos.1.to_bits() as i64))
            .collect();
        assert_eq!(distinct.len(), v.vis.len(), "the layout separated the chips");

        // A rect covering the whole canvas captures every visible node.
        let all = v.nodes_in_rect(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0)));
        assert_eq!(
            all.len(),
            v.vis.len(),
            "a full-canvas rect captures every visible node"
        );

        // A rect far away captures nothing — the selection is not "sticky".
        let none = v.nodes_in_rect(Rect::from_min_size(Pos2::new(9_000.0, 9_000.0), Vec2::splat(10.0)));
        assert!(none.is_empty(), "an empty region selects nothing");

        // A rect tightly around one chip captures exactly that node.
        let (cx, cy) = v.world_to_screen(v.vis[0].pos);
        let one = v.nodes_in_rect(Rect::from_center_size(Pos2::new(cx, cy), Vec2::splat(2.0)));
        assert_eq!(one.len(), 1, "one chip, one node");
        assert!(one.contains(&v.vis[0].index));
    }

    /// Committing a marquee sets the captured set and one focus — not N history
    /// entries. The Lead flagged this: a drag must not turn into N Back steps.
    #[test]
    fn finishing_a_marquee_sets_one_focus_not_one_history_entry_per_node() {
        let mut v = expanded_view();
        assert!(v.vis.len() >= 2);

        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        v.marquee = Some((rect.min, rect.max));
        v.finish_marquee(rect);

        assert!(v.marquee.is_none(), "the live rectangle is cleared on commit");
        assert_eq!(
            v.marquee_selection().len(),
            v.vis.len(),
            "the whole canvas was captured"
        );
        assert!(
            v.history.len() <= 1,
            "one drag must add at most one Back entry, got {}",
            v.history.len()
        );
        // The focus is the deterministic lowest captured index.
        let min = *v.marquee_selection().iter().min().unwrap();
        assert_eq!(v.selected, Some(min));
    }

    /// An empty marquee must not move the selection at all.
    #[test]
    fn an_empty_marquee_leaves_the_selection_alone() {
        let mut v = expanded_view();
        v.select(Some(v.vis[0].index));
        let before = v.selected;

        let empty = Rect::from_min_size(Pos2::new(9_000.0, 9_000.0), Vec2::splat(4.0));
        v.marquee = Some((empty.min, empty.max));
        v.finish_marquee(empty);

        assert!(v.marquee_selection().is_empty());
        assert_eq!(v.selected, before, "an empty marquee must not clear the focus");
    }

    // -----------------------------------------------------------------------
    // gesture disambiguation (marquee vs pan vs click vs double-click)
    // -----------------------------------------------------------------------

    /// The three-way interaction, tested as *state transitions* rather than by
    /// synthesising egui pointer events (which would only test egui).
    ///
    /// The invariant under test is the one in `ui()`'s GESTURE PRECEDENCE
    /// comment: a gesture is classified once, by where it *started*, and the
    /// marquee path consumes it.
    #[test]
    fn gesture_precedence_marquee_pan_click_and_double_click_do_not_overlap() {
        let mut v = expanded_view();
        let node = v.vis[0].index;
        let (cx, cy) = v.world_to_screen(v.vis[0].pos);
        let on_chip = Pos2::new(cx, cy);
        let empty = Pos2::new(cx + 500.0, cy + 400.0);

        // (1) A drag that STARTS ON A CHIP is a pan, not a marquee. This is what
        //     `started_on_chip` decides, and it is why dragging a node cannot
        //     leave a selection rectangle behind.
        assert!(
            v.pick(on_chip).is_some(),
            "precondition: this point is over a chip"
        );
        // (2) A drag that starts on empty canvas is a marquee.
        assert!(v.pick(empty).is_none(), "precondition: this point is empty canvas");

        // The marquee path itself must never touch expansion: run a full
        // capture over a node and prove `expanded` is untouched.
        let before_expanded = v.expanded_set();
        let rect = Rect::from_center_size(on_chip, Vec2::splat(6.0));
        v.marquee = Some((rect.min, rect.max));
        v.finish_marquee(rect);
        assert_eq!(
            v.expanded_set(),
            before_expanded,
            "a marquee drag must NOT toggle expansion"
        );
        assert!(v.marquee_selection().contains(&node), "but it did select the node");

        // (3) A double-click on the same node DOES toggle expansion — the
        //     gesture still works, it just is not what a drag does.
        v.toggle_node(node);
        assert_ne!(
            v.expanded_set(),
            before_expanded,
            "double-click still expands"
        );

        // (4) A marquee must never leave a zero-size rectangle behind: after the
        //     commit the live rect is gone, so a subsequent frame draws nothing.
        assert!(v.marquee.is_none(), "no stale marquee after release");
        assert!(v.marquee_rect().is_none(), "and no rect is reported either");
        assert!(!v.marquee_active());
    }

    /// A `drag_started()` on a chip must not open a marquee, and on empty canvas
    /// must. Tested against `started_on_chip`'s decision directly, since that is
    /// the single guard the whole precedence rule rests on.
    #[test]
    fn a_drag_starting_on_a_chip_is_a_pan_and_never_opens_a_marquee() {
        let v = expanded_view();
        let (cx, cy) = v.world_to_screen(v.vis[0].pos);
        let on_chip = Pos2::new(cx, cy);
        let empty = Pos2::new(cx + 500.0, cy + 400.0);

        // Replays `ui()`'s decision for both press points.
        let opens = |v: &GraphView, p: Pos2| v.pick(p).is_none();

        assert!(!opens(&v, on_chip), "press on a chip => pan, no marquee");
        assert!(opens(&v, empty), "press on empty canvas => marquee");

        // And the pan branch leaves the marquee field untouched.
        assert!(v.marquee.is_none(), "no marquee was ever opened");
    }

    // -----------------------------------------------------------------------
    // The Layout ▸ sliders reach the renderer (task-10)
    // -----------------------------------------------------------------------

    /// A slider edit changes `view.settings` — the state every renderer read
    /// consults. This is the "consumed" half: the route producing a value is not
    /// enough, the value has to land somewhere the paint path reads.
    #[test]
    fn applying_a_slider_edit_writes_the_setting_the_renderer_reads() {
        let mut v = laid_out();
        let before = v.settings();

        assert!(v.apply_slider(SliderEdit::new(SliderKind::Spacing, 200.0, SliderPhase::Release)));
        assert_eq!(v.settings().spacing, 200.0, "spacing is what `run_layout` is handed");
        assert_eq!(v.settings().iterations, before.iterations, "only spacing moved");

        assert!(v.apply_slider(SliderEdit::new(SliderKind::Curve, 0.2, SliderPhase::Release)));
        assert_eq!(v.settings().curvature, 0.2, "curvature is what `edge_draw` is handed");

        assert!(v.apply_slider(SliderEdit::new(SliderKind::NodeSize, 20.0, SliderPhase::Release)));
        assert_eq!(v.settings().node_size, 20.0, "node size is the dot radius");

        assert!(v.apply_slider(SliderEdit::new(SliderKind::Iterations, 1500.0, SliderPhase::Release)));
        assert_eq!(v.settings().iterations, 1500, "iterations is the FA2 step count");

        assert!(v.apply_slider(SliderEdit::new(SliderKind::TextSize, 18.0, SliderPhase::Release)));
        assert_eq!(v.settings().text_size, 18.0, "text size is the chip label font");
    }

    /// The value reaches the *measured* chip geometry, not just the settings
    /// struct. A 20px label is measurably taller than an 11px one, and the box
    /// the renderer will paint is the one the cache now returns.
    #[test]
    fn the_text_size_setting_reaches_the_measured_chip_box() {
        let mut small = ChipMetrics::with_font(8.0, 11.0);
        let mut big = ChipMetrics::with_font(8.0, 20.0);
        // A fixed text engine: width independent of the font, so the assertion
        // is about the *height* the font drives, not about font metrics.
        let mut measure = |_t: &str| 33.0;
        let (_w1, h1) = small.size("Alpha", &mut measure);
        let (_w2, h2) = big.size("Alpha", &mut measure);
        assert!(h2 > h1, "a larger font must make a taller box: {h1} vs {h2}");
        assert!((h1 - 13.0).abs() < 1e-4, "11px + 2*1px pad = 13, got {h1}");
        assert!((h2 - 22.0).abs() < 1e-4, "20px + 2*1px pad = 22, got {h2}");
    }

    /// **The stale-key hazard.** `ChipMetrics` used to key its cache on the label
    /// text alone, which was sound only while the font was a constant. Now that
    /// the font is a setting, a text-only key would serve boxes measured at the
    /// OLD size. One cache instance, two font sizes, same label: the two answers
    /// must differ.
    ///
    /// This is the mutation target: fold the font back out of the key and this
    /// test goes red, because the second lookup returns the first box.
    #[test]
    fn the_chip_cache_key_separates_two_font_sizes_for_one_label() {
        let mut at_11 = ChipMetrics::with_font(8.0, 11.0);
        let mut measure = |_t: &str| 33.0;
        let (_w, h11) = at_11.size("Alpha", &mut measure);

        // Same instance, different font. A text-only key would return `h11`.
        at_11.font_size = 20.0;
        let (_w, h20) = at_11.size("Alpha", &mut measure);
        assert_ne!(
            h11, h20,
            "the cache served a box measured at the old font - the key must carry it"
        );
        assert!((h20 - 22.0).abs() < 1e-4, "the new font must be used: {h20}");

        // ...and going back is still the original answer, so the key is a key and
        // not just a cache flush.
        at_11.font_size = 11.0;
        let (_w, again) = at_11.size("Alpha", &mut measure);
        assert_eq!(again, h11, "the 11px entry must still be there");
    }

    /// Live vs release: a live drag of iterations must NOT mark the layout dirty
    /// (that would start an FA2 pass per pixel); the release must. Spacing is the
    /// mirror image — the reference only commits it on `change`.
    #[test]
    fn live_edits_do_not_resettle_and_release_does() {
        let mut v = laid_out();

        // Iterations, live: no re-settle.
        v.layout_dirty = false;
        v.apply_slider(SliderEdit::new(SliderKind::Iterations, 1200.0, SliderPhase::Live));
        assert!(!v.layout_dirty, "dragging iterations must not re-run FA2 per pixel");
        assert_eq!(
            v.settings().iterations,
            graphviz::ITER_DEFAULT,
            "the live half of iterations only moves the readout"
        );

        // Iterations, release: commits AND re-settles.
        v.apply_slider(SliderEdit::new(SliderKind::Iterations, 1200.0, SliderPhase::Release));
        assert_eq!(v.settings().iterations, 1200);
        assert!(v.layout_dirty, "releasing iterations re-runs the layout");

        // Curve, live: writes immediately (the paint is cheap) but never resettles.
        v.layout_dirty = false;
        assert!(v.apply_slider(SliderEdit::new(SliderKind::Curve, 0.05, SliderPhase::Live)));
        assert_eq!(v.settings().curvature, 0.05, "curvature updates live");
        assert!(!v.layout_dirty, "curvature needs no layout pass");

        // Node size, live: writes, no re-settle. Release: re-settles.
        v.layout_dirty = false;
        assert!(v.apply_slider(SliderEdit::new(SliderKind::NodeSize, 14.0, SliderPhase::Live)));
        assert_eq!(v.settings().node_size, 14.0);
        assert!(!v.layout_dirty);
        v.apply_slider(SliderEdit::new(SliderKind::NodeSize, 14.0, SliderPhase::Release));
        assert!(v.layout_dirty, "the glyph feeds the FA2 radius, so release re-settles");
    }

    /// A no-op edit reports "nothing changed", so a host can skip a repaint — and
    /// it must not be confused by the release flag alone.
    #[test]
    fn a_slider_edit_that_changes_nothing_reports_so() {
        let mut v = laid_out();
        let s = v.settings();
        assert!(
            !v.apply_slider(SliderEdit::new(SliderKind::Curve, s.curvature, SliderPhase::Live)),
            "setting curvature to what it already is changed nothing"
        );
    }

    /// The default is a no-op by construction: `TEXT_SIZE_DEFAULT` is the font
    /// chip geometry already used, so a view that never touched the slider paints
    /// exactly what it painted before the control existed. This is the invariant
    /// that keeps `@frame` still.
    #[test]
    fn the_default_text_size_is_the_font_chip_geometry_already_used() {
        assert_eq!(graphviz::TEXT_SIZE_DEFAULT, graphviz::CHIP_FONT);
        assert_eq!(LayoutSettings::default().text_size, graphviz::CHIP_FONT);
        assert_eq!(LayoutSettings::default(), LayoutSettings {
            spacing: graphviz::SPACING_DEFAULT,
            iterations: graphviz::ITER_DEFAULT,
            curvature: graphviz::CURVE_DEFAULT,
            node_size: graphviz::NODE_SIZE_DEFAULT,
            text_size: graphviz::TEXT_SIZE_DEFAULT,
        });
        // A default-box measured at the default font is the pre-slider box.
        let mut m = ChipMetrics::new(8.0);
        let mut measure = |_t: &str| 33.0;
        assert_eq!(m.size("Alpha", &mut measure), ChipMetrics::with_font(8.0, 11.0).size("Alpha", &mut measure));
    }

    /// `arrange.js:289-299`: a three-node marquee arranged vertically comes out
    /// in depth-first step order top-to-bottom, left edges flush. The starting
    /// column is deliberately the *reverse* of step order, so the `inStepOrder`
    /// test (`:293`) is false and the press must place step order, not flip it.
    #[test]
    fn arrange_vertical_puts_the_selection_in_step_order_top_to_bottom() {
        let mut v = graph();
        v.expand_node(0);
        v.expand_node(1);
        let slots: Vec<(usize, (f32, f32))> = [("NZK", 300.0), ("NZK.A", 200.0), ("NZK.A.m", 100.0)]
            .iter()
            .map(|(id, y)| {
                let s = v
                    .vis
                    .iter()
                    .position(|n| v.graph.nodes[n.index].id == *id)
                    .unwrap_or_else(|| panic!("{id} is not visible"));
                v.vis[s].pos = (100.0, *y);
                (s, v.vis[s].size)
            })
            .collect();
        v.marquee_sel.clear();
        for &(s, _) in &slots {
            v.marquee_sel.insert(v.vis[s].index);
        }
        let order = v.arrange_selection(crate::chords::Axis::Vertical).unwrap();
        assert_eq!(order, vec!["NZK", "NZK.A", "NZK.A.m"], "step order is preserved");
        let ys: Vec<f64> = ["NZK", "NZK.A", "NZK.A.m"].iter().map(|id| pos_of(&v, id).1).collect();
        assert!(ys[0] < ys[1] && ys[1] < ys[2], "step order runs top-to-bottom: {ys:?}");
        let lefts: Vec<f64> = slots
            .iter()
            .map(|&(s, sz)| v.vis[s].pos.0 - f64::from(sz.0) / 2.0)
            .collect();
        assert!(
            (lefts[0] - lefts[1]).abs() < 1e-9 && (lefts[1] - lefts[2]).abs() < 1e-9,
            "a column has flush left edges: {lefts:?}"
        );
    }

    /// `arrange.js:41`/`:308`: `gap` lives on the view, so a second `Shift+=`
    /// widens from the first widened value instead of restarting at 16.
    #[test]
    fn a_second_spacing_press_widens_from_the_persisted_gap() {
        let mut v = graph();
        v.expand_node(0);
        v.expand_node(1);
        for (id, y) in [("NZK", 300.0), ("NZK.A", 200.0), ("NZK.A.m", 100.0)] {
            let s = v.vis.iter().position(|n| v.graph.nodes[n.index].id == id).unwrap();
            v.vis[s].pos = (100.0, y);
            v.marquee_sel.insert(v.vis[s].index);
        }
        assert_eq!(v.arrangement.gap(), 16.0);
        // Widen once: 16 * 1.25 + 0.5 = 20.5 (`arrange.js:308`).
        v.space_selection(1).unwrap();
        assert_eq!(v.arrangement.gap(), 20.5);
        let gap1 = pos_of(&v, "NZK.A").1 - pos_of(&v, "NZK.A.m").1;
        // Widen again: 20.5 * 1.25 + 0.5 = 26.125, a wider column than before.
        v.space_selection(1).unwrap();
        assert_eq!(v.arrangement.gap(), 26.125);
        let gap2 = pos_of(&v, "NZK.A").1 - pos_of(&v, "NZK.A.m").1;
        assert!(gap2 > gap1, "the second press widens: {gap1} -> {gap2}");
        assert!((gap2 - gap1 - (26.125 - 20.5)).abs() < 1e-9, "the cell pitch grows by the gap delta");
    }

    /// `arrange.js:290`/`:304`: fewer than two box-selected nodes toasts and
    /// returns without moving anything or touching `gap`.
    #[test]
    fn one_selected_node_returns_none_and_moves_nothing() {
        let mut v = graph();
        v.marquee_sel.insert(0);
        let before = v.vis[0].pos;
        assert_eq!(v.arrange_selection(crate::chords::Axis::Vertical), None);
        assert_eq!(v.space_selection(1), None);
        assert_eq!(v.vis[0].pos, before, "a refused command moves nothing");
        assert_eq!(v.arrangement.gap(), 16.0, "a refused spacing does not widen");
    }

    /// A suggestion commit centres the REAL camera on the committed node:
    /// the node's world point lands exactly at the canvas centre at the current
    /// zoom (`centerNode`'s fly-to, `interaction.js:422`), and an index the view
    /// mode filtered out is an honest no-op that moves nothing.
    #[test]
    fn centring_a_node_puts_it_at_the_canvas_centre_without_changing_zoom() {
        let mut v = graph();
        v.vis[0].pos = (123.0, -45.0);
        v.set_camera(1.5, 100.0, -50.0);

        assert!(v.center_on_node(0));
        let (zoom, x, y) = v.camera();
        assert_eq!(zoom, 1.5, "centring keeps the zoom");
        assert_eq!((x, y), (123.0, -45.0), "the camera's world point IS the node");

        // The projection really centres it: the node projects to (w/2, h/2).
        let (sx, sy) = v.cam.world_to_screen(800.0, 600.0, 123.0, -45.0);
        assert!((sx - 400.0).abs() < 1e-4, "x centred, got {sx}");
        assert!((sy - 300.0).abs() < 1e-4, "y centred, got {sy}");

        // An index that has no visible slot (the method is not expanded onto the
        // canvas) is a no-op, never a panic and never a guessed node.
        let before = v.camera();
        assert!(!v.center_on_node(4));
        assert_eq!(v.camera(), before, "an uncentrable node moves nothing");
    }

    /// The project chip model is a REAL predicate transition: `set_filters`
    /// reports the change, and a node owned by a hidden project's path is
    /// filtered out while an unowned one stays.
    #[test]
    fn the_project_filter_is_a_model_transition_and_matches_node_ownership() {
        let mut v = graph();
        let base = crate::actions::FilterState::default();
        assert!(!v.set_filters(&base), "the shipped model is already in place");
        let mut hidden = base.clone();
        hidden.hidden_projects.insert("p1".to_string());
        assert!(v.set_filters(&hidden), "a project toggle rebuilds the view");
        assert!(!v.set_filters(&hidden), "repeating the same model is not a transition");
        let project = crate::model::ProjectRow {
            id: "p1".to_string(),
            name: "P1".to_string(),
            file_count: 1,
            path: "/repo/A".to_string(),
            targets: Vec::new(),
            defines: Vec::new(),
        };
        assert!(GraphView::project_owns_node(&project, "/repo/A/X.cs"));
        assert!(!GraphView::project_owns_node(&project, "/repo/B/Y.cs"));
        assert!(!GraphView::project_owns_node(&project, ""));
    }

    /// The committed `#nsSelect` value is the ONE filter model input; the view
    /// predicate must consume it (and `#nsDeep` keeps sub-namespaces).
    #[test]
    fn a_committed_namespace_change_reaches_the_view_predicate() {
        let mut v = graph();
        // The fixture's node() helper puts every node in namespace "N"; give
        // this test the real ancestry the predicate is about.
        v.graph.nodes[1].ns = "NZK.A".to_string();
        v.graph.nodes[3].ns = "NZK.A.m".to_string();
        v.graph.nodes[2].ns = "NZK.B".to_string();
        v.graph.nodes[4].ns = "NZK.B.k".to_string();
        let node = |v: &GraphView, id: &str| {
            v.graph
                .nodes
                .iter()
                .find(|n| n.id == id)
                .cloned()
                .expect("fixture node")
        };
        // Empty model: every node passes (the shipped render).
        assert!(v.filter_passes(&node(&v, "NZK.B.k")));
        let mut filters = crate::actions::FilterState::default();
        filters.namespace = "NZK.A".to_string();
        assert!(v.set_filters(&filters), "a namespace change is a real transition");
        assert!(v.filter_passes(&node(&v, "NZK.A")));
        assert!(v.filter_passes(&node(&v, "NZK.A.m")), "ns_deep includes children");
        assert!(!v.filter_passes(&node(&v, "NZK.B.k")), "outside the namespace is hidden");
        // Back to "All namespaces" restores every node.
        assert!(v.set_filters(&crate::actions::FilterState::default()));
        assert!(v.filter_passes(&node(&v, "NZK.B.k")));
    }

    /// `drawer.js` `Hide children`: the parent's `contains` children leave the
    /// canvas and their drawn edges leave with them; `Show children` restores
    /// the same nodes because `Graph` was never re-indexed.
    #[test]
    fn hiding_children_removes_the_children_and_their_edges_from_the_canvas() {
        let mut v = graph();
        v.expand_node(0); // namespace NZK -> A, B
        assert!(visible_ids(&v).contains(&"NZK.A"));
        assert!(visible_ids(&v).contains(&"NZK.B"));
        assert!(!v.edges.is_empty(), "the containment edges are drawn");

        let mut scope = std::collections::BTreeSet::new();
        scope.insert("NZK".to_string());
        assert!(
            v.sync_hidden_children(&scope),
            "a scope change is a real transition"
        );
        assert!(!v.sync_hidden_children(&scope), "repeating the scope is not");
        assert!(!visible_ids(&v).contains(&"NZK.A"), "the child left the canvas");
        assert!(!visible_ids(&v).contains(&"NZK.B"), "the second child left too");
        assert!(visible_ids(&v).contains(&"NZK"), "the parent itself stays");
        assert!(v.edges.is_empty(), "no drawn edge can point at a hidden node");
        assert_eq!(v.graph.nodes.len(), 5, "the graph itself is untouched");

        scope.clear();
        assert!(v.sync_hidden_children(&scope), "Show children is a transition");
        assert!(visible_ids(&v).contains(&"NZK.A"));
        assert!(!v.edges.is_empty(), "the containment edges are drawn again");
        assert_eq!(v.graph.nodes.len(), 5);
    }

    /// `#pruneHidden` on the view: the children hidden by the drawer scope are
    /// deleted from the resident graph, every index-valued model is remapped by
    /// node id, and a second prune is an honest no-op.
    #[test]
    fn pruning_hidden_children_deletes_them_and_remaps_every_index_model() {
        let mut v = graph();
        v.expand_node(0); // NZK -> A, B
        v.expand_node(1); // NZK.A -> m
        v.selected = Some(3); // NZK.A.m
        v.pinned.insert(3);
        let mut scope = std::collections::BTreeSet::new();
        scope.insert("NZK".to_string());
        v.sync_hidden_children(&scope);

        let removed = v.prune_hidden();
        assert!(removed >= 2, "A and B were hidden and are gone, removed {removed}");
        assert!(v
            .graph
            .nodes
            .iter()
            .all(|n| n.id != "NZK.A" && n.id != "NZK.B"));
        // The selected method survived under a new index and still points at the
        // same id; a stale index would point at whatever happened to land there.
        assert_eq!(
            v.selected.map(|i| v.graph.nodes[i].id.as_str()),
            Some("NZK.A.m")
        );
        let selected = v.selected.expect("the selection survived");
        assert!(v.pinned.contains(&selected), "the pin followed its node");
        assert!(
            v.graph
                .edges
                .iter()
                .all(|e| e.from < v.graph.nodes.len() && e.to < v.graph.nodes.len()),
            "no edge can address a removed node"
        );
        assert_eq!(v.prune_hidden(), 0, "nothing hidden anymore = honest no-op");
    }

    // -----------------------------------------------------------------------
    // Pointer parity: drag pins, right-click card, Alt+click pin
    // -----------------------------------------------------------------------

    /// One egui frame of the real canvas. `events` are injected as raw input, so
    /// these tests exercise the same gesture code a human pointer drives.
    fn canvas_frame(
        ctx: &egui::Context,
        view: &mut GraphView,
        events: Vec<egui::Event>,
    ) {
        let raw = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
            events,
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    view.ui(ui);
                });
        });
        out.textures_delta.clear();
    }

    fn pointer_button(pos: Pos2, button: PointerButton, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    fn screen_of(view: &GraphView, index: usize) -> Pos2 {
        let node = view.vis.iter().find(|v| v.index == index).expect("visible node");
        let (x, y) = view.world_to_screen(node.pos);
        Pos2::new(x, y)
    }

    /// `interaction.js:73-84`: a left drag that starts on a chip moves the chip,
    /// pins it, and must NOT pan the camera. Before this test the gesture was a
    /// pan: the sidebar hint promised "drag pins a node" and nothing pinned it.
    #[test]
    fn dragging_a_node_pins_it_and_moves_it_instead_of_panning() {
        let ctx = measured_ctx();
        let mut v = laid_out();
        // One pass so the canvas dimensions and hit geometry are live.
        canvas_frame(&ctx, &mut v, Vec::new());
        let start = screen_of(&v, 0);
        let before_pos = v.vis.iter().find(|n| n.index == 0).unwrap().pos;
        let before_cam = v.camera();

        canvas_frame(&ctx, &mut v, vec![egui::Event::PointerMoved(start)]);
        canvas_frame(
            &ctx,
            &mut v,
            vec![pointer_button(start, PointerButton::Primary, true)],
        );
        canvas_frame(
            &ctx,
            &mut v,
            vec![egui::Event::PointerMoved(start + Vec2::new(30.0, 0.0))],
        );
        canvas_frame(
            &ctx,
            &mut v,
            vec![egui::Event::PointerMoved(start + Vec2::new(60.0, 20.0))],
        );
        canvas_frame(
            &ctx,
            &mut v,
            vec![pointer_button(start + Vec2::new(60.0, 20.0), PointerButton::Primary, false)],
        );

        assert!(v.is_pinned(0), "a node drag must set the pin");
        assert!(
            v.vis.iter().find(|n| n.index == 0).unwrap().pos != before_pos,
            "the dragged chip must actually move"
        );
        assert_eq!(
            v.camera(),
            before_cam,
            "a node drag must not pan the camera (that is the empty-canvas gesture)"
        );
        assert!(v.node_drag.is_none(), "the drag must end on release");
    }

    /// `interaction.js:291-301`: right-click selects the node and pins its card
    /// open; Esc closes it. The card is a real `egui::Area` in [`GraphView::ui`].
    #[test]
    fn right_click_pins_the_node_card_and_escape_closes_it() {
        let ctx = measured_ctx();
        let mut v = laid_out();
        canvas_frame(&ctx, &mut v, Vec::new());
        let at = screen_of(&v, 0);

        canvas_frame(&ctx, &mut v, vec![egui::Event::PointerMoved(at)]);
        canvas_frame(
            &ctx,
            &mut v,
            vec![pointer_button(at, PointerButton::Secondary, true)],
        );
        canvas_frame(
            &ctx,
            &mut v,
            vec![pointer_button(at, PointerButton::Secondary, false)],
        );
        assert_eq!(v.selected, Some(0), "right-click selects the node");
        assert_eq!(v.pinned_card.map(|(i, _)| i), Some(0), "the card is pinned");

        canvas_frame(
            &ctx,
            &mut v,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
        );
        assert!(v.pinned_card.is_none(), "Esc closes the pinned card");
    }

    /// `interaction.js:63-71`: Alt+click toggles the physics pin without
    /// starting a drag or opening the card.
    #[test]
    fn alt_click_toggles_the_pin() {
        let ctx = measured_ctx();
        let mut v = laid_out();
        canvas_frame(&ctx, &mut v, Vec::new());
        let at = screen_of(&v, 0);
        let alt = egui::Modifiers { alt: true, ..Default::default() };
        let click = |pressed| egui::Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: alt,
        };

        canvas_frame(&ctx, &mut v, vec![egui::Event::PointerMoved(at)]);
        // A real winit session reports the modifier state separately from the
        // pointer event; without this the test would leave `InputState.modifiers`
        // at its default and could not exercise the Alt branch at all.
        canvas_frame(&ctx, &mut v, vec![egui::Event::ModifiersChanged(alt)]);
        canvas_frame(&ctx, &mut v, vec![click(true)]);
        canvas_frame(&ctx, &mut v, vec![click(false)]);
        assert!(v.is_pinned(0), "Alt+click pins");
        assert!(v.pinned_card.is_none(), "Alt+click does not open the card");

        canvas_frame(&ctx, &mut v, vec![click(true)]);
        canvas_frame(&ctx, &mut v, vec![click(false)]);
        assert!(!v.is_pinned(0), "a second Alt+click unpins");
    }

    /// The reference selects on `pointerdown` (`interaction.js:33-84`), so the
    /// selection has to be live on the press frame — waiting for `response.
    /// clicked()` (egui's release edge) is the latency this guards against.
    #[test]
    fn clicking_a_node_selects_it_on_press_not_release() {
        let ctx = measured_ctx();
        let mut v = laid_out();
        canvas_frame(&ctx, &mut v, Vec::new());
        assert_eq!(v.selected, None, "precondition: nothing selected");

        let at = screen_of(&v, 0);
        canvas_frame(&ctx, &mut v, vec![egui::Event::PointerMoved(at)]);
        canvas_frame(
            &ctx,
            &mut v,
            vec![pointer_button(at, PointerButton::Primary, true)],
        );
        assert_eq!(
            v.selected,
            Some(0),
            "the press frame itself must select the node, before any release"
        );

        canvas_frame(
            &ctx,
            &mut v,
            vec![pointer_button(at, PointerButton::Primary, false)],
        );
        assert_eq!(v.selected, Some(0), "the release must keep the press selection");
    }

    /// A press and its release can arrive in ONE frame. That is exactly what
    /// the host's orphan-release recovery produces: the window manager
    /// swallowed the press of the click that focused the window, so `App` feeds
    /// egui a synthetic press immediately before the real release. The
    /// press-time pick must still see `button_pressed` for that batch and
    /// select — otherwise a first click on a node still does nothing and the
    /// "clicked again and it worked" report survives the fix.
    #[test]
    fn a_press_and_release_in_one_frame_still_selects_the_node() {
        let ctx = measured_ctx();
        let mut v = laid_out();
        canvas_frame(&ctx, &mut v, Vec::new());
        let at = screen_of(&v, 0);
        canvas_frame(&ctx, &mut v, vec![egui::Event::PointerMoved(at)]);
        canvas_frame(
            &ctx,
            &mut v,
            vec![
                pointer_button(at, PointerButton::Primary, true),
                pointer_button(at, PointerButton::Primary, false),
            ],
        );
        assert_eq!(
            v.selected,
            Some(0),
            "a synthetic press followed by the real release (one batch) must select"
        );
    }
}

#[cfg(test)]
mod word_box_padding_tests {
    use super::*;

    fn text_shape(shape: &egui::Shape, out: &mut Vec<(Pos2, egui::Vec2, String)>) {
        match shape {
            egui::Shape::Text(t) => out.push((t.pos, t.galley.size(), t.galley.text().to_string())),
            egui::Shape::Vec(v) => {
                for s in v {
                    text_shape(s, out);
                }
            }
            _ => {}
        }
    }

    /// The word box's text must be centred in its own pill on BOTH axes. The
    /// reported bug: `Children`/`Uses` text hung out of the bottom of the box
    /// because the galley's top was placed at the pill's centre (`c.y + 0.5`)
    /// instead of its top edge (`c.y - height/2`).
    #[test]
    fn pill_text_is_centred_in_the_pill() {
        let ctx = egui::Context::default();
        let c = Pos2::new(400.0, 300.0);
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let galley = ui.painter().layout_no_wrap(
                        "Children".to_owned(),
                        FontId::monospace(graphviz::EDGE_PILL_FONT),
                        Color32::PLACEHOLDER,
                    );
                    let tw = galley.size().x;
                    paint_pill(ui.painter(), c, galley, tw, false);
                });
            },
        );
        out.textures_delta.clear();
        let mut texts = Vec::new();
        for cs in &out.shapes {
            text_shape(&cs.shape, &mut texts);
        }
        let (pos, size, text) = texts
            .into_iter()
            .find(|(_, _, t)| t == "Children")
            .expect("the pill's label must be painted");
        let cx = pos.x + size.x / 2.0;
        let cy = pos.y + size.y / 2.0;
        assert!((cx - c.x).abs() < 0.51, "horizontal centre: {cx} vs {}", c.x);
        assert!((cy - c.y).abs() < 0.51, "vertical centre: {cy} vs {}", c.y);
    }

    /// A relation node centres its galley too, and puts the kind dot to its left
    /// inside the padded box.
    #[test]
    fn relation_node_text_is_centred_and_has_a_dot() {
        let ctx = egui::Context::default();
        let c = Pos2::new(300.0, 200.0);
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let galley = ui.painter().layout_no_wrap(
                        "Calling".to_owned(),
                        FontId::monospace(graphviz::VT_REL_FONT),
                        Color32::PLACEHOLDER,
                    );
                    let tw = galley.size().x;
                    paint_relation_node(
                        ui.painter(),
                        c,
                        galley,
                        tw,
                        graphviz::Rgba::rgb(0xc9, 0xd3, 0xe5),
                        false,
                    );
                });
            },
        );
        out.textures_delta.clear();
        let mut texts = Vec::new();
        for cs in &out.shapes {
            text_shape(&cs.shape, &mut texts);
        }
        let (pos, size, _) = texts
            .into_iter()
            .find(|(_, _, t)| t == "Calling")
            .expect("the relation node's label must be painted");
        assert!((pos.y + size.y / 2.0 - c.y).abs() < 0.51, "vertically centred");
        assert!(pos.x > c.x - size.x / 2.0, "text sits right of the dot");
    }
}
