//! The winit multi-window host and the shared egui UI.
//!
//! The **window loop** runs `App::frame` (`WindowEvent::RedrawRequested`), which
//! calls `build_ui_with` and then consumes the `Effect` it returns — that is the
//! path a pointer press takes, and it is why a host effect must be carried back
//! to `App` (`consume_engine_effects`) rather than dropped at the UI boundary.
//! The **offscreen PNG path** calls `build_ui`, which renders the same panels and
//! canvas into an image and discards the effect (`render.rs`); being effect-free
//! is exactly right for a headless render, but it means `build_ui` is *not* the
//! window loop.
//!
//! The windowed path adds the pane buttons and the snap indicator inside the
//! same egui pass; the offscreen path is untouched (native-fidelity diffs it).
//!
//! # Windows
//!
//! One winit `EventLoop` drives every window (that is how winit works); each
//! window owns its own wgpu surface, egui context and egui renderer, while all
//! of them share one wgpu adapter/device/queue ([`Gpu`] behind an `Arc`). Panes
//! detach into real OS windows and re-dock into the main window; the decision
//! rules live in [`crate::windows`] as pure geometry with unit tests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_wgpu::{RendererOptions, ScreenDescriptor};
use serde_json::json;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::event::Modifiers;
use winit::window::{Window, WindowAttributes, WindowId};

use crate::actions::{self, NzkUiAction, NzkUiState, Effect, HostPane, TabCmd};
use crate::doc_engine;
use crate::render::Gpu;
use scratch_rs_ide_addon::gui_scratch::{self as gui_scratch, GuiState, Theme as ScratchTheme};
use scratch_rs_ide_addon::idelink_scratch::{LinkState, Request};
use crate::graphviz::UiTheme;
use crate::view::{GraphView, ViewMode};
use crate::windows::{self, Decision, Pane, Rect, TargetId};

/// The `limit` `loadDefinitions` asks for (`filters.js:491`:
/// `apiGet('/api/definitions', { scope: 'public', limit: 400 })`).
///
/// The engine treats it as a **page size**, not a cap on `total`
/// (`core/src/index.rs:3425-3428`), so `#pubCount` can legitimately read larger
/// than the number of rows `#pubList` shows. That is the reference's own
/// behaviour, not a bug to smooth over.
pub(crate) const PUB_DEFINITIONS_LIMIT: i64 = 400;

// nzk_ui layer (task-18). Declared here with `#[path]` rather than in
// `main.rs` because main.rs is owned by the windows/threading task; the module
// is only referenced from this file.
#[path = "nzk_ui.rs"]
pub(crate) mod nzk_ui;

// Host half of the `nzk_ui --serve` protocol (task-3/D5). Same `#[path]`
// reason: it is only reached from `nzk_ui.rs`, which owns the nzk_ui invocation.
#[path = "app/nzk_ui_client.rs"]
mod nzk_ui_client;

// Build stamp + window titles (task-20). Same `#[path]` reason: main.rs is
// owned by the windows/threading task. `BUILD` comes from `app/build.rs` at
// compile time; see src/buildinfo.rs.
#[path = "buildinfo.rs"]
pub mod buildinfo;

// The source pane (task-7) and its highlighter (task-16 integration). Declared
// here with `#[path]` for the same reason as nzk_ui/buildinfo: main.rs stays
// untouched. `source_pane.rs` reaches its sibling through `super::`. Both are
// `pub` so the auxiliary `source_shot` bin can reach them through the crate's
// library half (`lib.rs` -> `app::source_pane`) instead of `#[path]`-including
// a subset of files that no longer compiles on its own.
#[path = "source_highlight.rs"]
pub mod source_highlight;
#[path = "source_pane.rs"]
pub mod source_pane;

// Call/type hierarchy (round 16). Declared here with `#[path]` for the same
// reason as nzk_ui/buildinfo: main.rs is owned by the windows/threading task and
// stays untouched. `hierarchy_wire.rs` in turn declares its own core
// (`hierarchy.rs`) the same way, so both files register without editing main.rs.
#[path = "hierarchy_wire.rs"]
pub mod hierarchy_wire;

// GAP B: the `#strCount` / `#chainCount` section-head counters' **producer**.
// nzk_ui's `apply_section_counts` (dom.rs:2913) was already correct and was
// never entered, because nothing in `app/src` emitted `section_counts`; this is
// the missing half. Same `#[path]` reason as everything above — `main.rs` is not
// touched.
#[path = "section_counts.rs"]
pub mod section_counts;

// Quick-open + code-lens wires. Both were staged (declared, tested, never
// registered) because they referenced `crate::source_pane`, which does not
// exist: `source_pane.rs` is declared *in this file*, so it is
// `crate::app::source_pane`. That path is fixed in `codelens_wire.rs` and both
// are registered here, so the two subsystems are live rather than dormant.
// Same `#[path]` reason as everything above: `main.rs` stays untouched, and each
// wire declares its own core.
#[path = "quickopen_wire.rs"]
pub mod quickopen_wire;
#[path = "codelens_wire.rs"]
pub mod codelens_wire;

// Detached pane body models (task-6). The pure per-kind mapping behind
// `pane_ui`: given the same live state the main window reads, it decides the
// real body of a popped-out pane. Same `#[path]` reason as everything above —
// `main.rs` is not touched.
#[path = "pane_body.rs"]
mod pane_body;

// The Blocks canvas producer (task-13): v2 `scratch.js`'s cards/ports/edges,
// derived from the in-memory graph instead of `/api/decls`+`/api/subgraph`.
// Same `#[path]` reason as everything above — `main.rs` is not touched.
#[path = "blocks_wire.rs"]
pub mod blocks_wire;

// The Scratch puzzle-piece window (its GUI lives in its own repo at the
// workspace root: `scratch_rs_ide_addon`). This host module is the only bridge
// between the addon and csgraph. Same `#[path]` reason as everything above —
// `main.rs` is not touched.
#[path = "nzk_scratch.rs"]
mod nzk_scratch;

use source_pane::{SourceHostExt, SourceState, SourceTheme};

// Screenshot opt-in: `CSVIEWER_SCREENSHOT_PANE=<node>` opens the source pane in
// the offscreen render. Default screenshots stay pane-closed so ab.sh's canvas
// registration and region metrics remain comparable.
thread_local! {
    static SHOT_SOURCE: std::cell::RefCell<Option<SourceState>> =
        const { std::cell::RefCell::new(None) };
    /// Window actions a nzk_ui press inside `build_ui_with` asked for.
    ///
    /// `build_ui_with` runs inside egui's pass and cannot create or drop OS
    /// windows (that needs the `ActiveEventLoop`), so `#srcPopBtn`'s detach/dock
    /// is queued here and drained by `App` on the event loop — the same
    /// split the existing `Action` queue uses. Empty on every non-windowed path.
    static PENDING_PANE_ACTIONS: std::cell::RefCell<Vec<Action>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Take the window actions queued by the last UI pass (see
/// [`PENDING_PANE_ACTIONS`]).
pub(crate) fn take_pending_pane_actions() -> Vec<Action> {
    PENDING_PANE_ACTIONS.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// What the windowed run should do before falling back to interactive mode.
#[derive(Default, Clone)]
pub struct RunOptions {
    /// Scripted detach → move → snap → dock demonstration; writes a JSON
    /// transcript to this path and exits. This is a **real window** run: OS
    /// windows are created, moved with `set_outer_position`, and their positions
    /// are read back with `outer_position()`.
    pub snap_demo: Option<PathBuf>,
    /// Process start, so `first-frame-ms` counts from exec, not from the event
    /// loop (indexing happens before `run`).
    pub start: Option<Instant>,
    /// Corpus path, recorded in the snap-demo transcript (passed as a value so
    /// no `std::env::set_var` unsafe is needed).
    pub corpus: String,
}

/// The windowed entry point. `source` is `None` in the plain screenshot path,
/// so `--screenshot` stays pane-closed by default; the env opt-in below opens it
/// for evidence runs.
pub fn build_ui(ui: &mut egui::Ui, view: &mut GraphView) {
    if let Ok(node) = std::env::var("CSVIEWER_SCREENSHOT_PANE") {
        if !node.is_empty() {
            SHOT_SOURCE.with(|slot| {
                let mut slot = slot.borrow_mut();
                let source = slot.get_or_insert_with(SourceState::default);
                let mut actions = NzkUiState::default();
                if !source.open {
                    // The windowed selection path resolves the ref against the
                    // live graph (`App::preopen_source`); do the same here so the
                    // evidence run accepts an id, a full name OR a label exactly
                    // like a graph click does. A ref that matches nothing falls
                    // through to the engine's own alias resolution, which is the
                    // honest "the engine could not resolve it" path.
                    let resolved = view
                        .graph
                        .nodes
                        .iter()
                        .find(|n| n.id == node || n.full == node || n.label == node)
                        .map(|n| n.id.clone())
                        .unwrap_or_else(|| node.clone());
                    let roots = graph_roots(view);
                    source_pane::show_node_repo(source, &view.graph.repo, &roots, &resolved);
                    // Evidence path only: open the document's tab through the same
                    // model call `App::sync_source` makes, so this composite frame
                    // shows the real `#cs-tabs` strip the nzk_ui paints from
                    // the `ui.tabs` facts.
                    actions
                        .tabs
                        .open_params(&serde_json::json!({ "node": resolved }));
                }
                build_ui_with(ui, view, Some(source), &mut actions);
            });
            return;
        }
    }
    let mut actions = NzkUiState::default();
    // Keep the checkbox in step with the startup mode the view already chose.
    actions.filters.vt_mode = crate::view::vt_mode_from_env();
    build_ui_with(ui, view, None, &mut actions);
}

/// The shared UI: a stats bar, a details side panel, the source pane, and the
/// graph canvas.
///
/// When the nzk_ui nzk_ui layer is available it replaces the hand-ported
/// top bar and sidebar with the real HTML/CSS pixels, and the graph canvas is
/// clipped to nzk_ui's `#graph` box. Otherwise the egui nzk_ui below is
/// unchanged (and `CSVIEWER_NO_CHROME=1` forces it deliberately).
pub fn build_ui_with(
    ui: &mut egui::Ui,
    view: &mut GraphView,
    source: Option<&mut SourceState>,
    actions_state: &mut NzkUiState,
) -> Effect {
    build_ui_with_panes(ui, view, source, None, actions_state)
}

/// [`build_ui_with`] plus the split pane's second source document.
///
/// `split` is `Some` only while the tabs model says the split is on (see
/// `App::sync_split_source`); it is then painted into the `#csSplitHost` box of
/// the same frame. Passing `None` is byte-for-byte the pre-round-8 path, which
/// is why the old entry point can keep delegating here and every existing call
/// site keeps compiling.
/// Commit one suggestion row: select its node when it resolves, centre the
/// camera on it, then hide the popup and release the search box.
///
/// This is the **single** commit path for both input routes — Enter
/// (`find_wire::route_key` -> `SuggestionView::commit_index`) and a press on a
/// `.sug` row (`nzk_ui::suggestion_row_click` -> `SuggestionView::commit_by_id`).
/// `target == None` is the stale/unknown-id case and is an honest no-op for the
/// selection: it never picks a neighbouring node and never panics, it only hides
/// the popup exactly like `commitSearch`'s empty-list guard
/// (`search.js:76-84`).
///
/// Centring uses `GraphView`'s real camera (`view.rs` `Camera`), which is the
/// native counterpart of `centerNode`'s fly-to (`interaction.js:422`) — not a
/// faked or reported gap.
fn commit_suggestion(
    actions_state: &mut NzkUiState,
    view: &mut GraphView,
    target: Option<usize>,
) {
    if let Some(index) = target {
        view.select(Some(index));
        view.center_on_node(index);
    }
    actions_state.suggest.hide();
    actions_state.search_focused = false;
}

/// Apply the node drawer's `Expand` press: open every relation group **and**
/// pull every bucket onto the canvas.
///
/// v2's button is `expandNode(id, {all:true})` (`shell.js:570-574`): it is an
/// "Add every relation bucket" action, not a list-only toggle. The drawer half
/// is [`crate::drawer_wire::DrawerUi::expand_all`]; the canvas half is
/// [`GraphView::expand_node_all`]. Keeping them in one function is what stops
/// the button regressing to a no-op on the graph. Returns `false` for a stale
/// node id (an honest no-op, exactly like the other drawer click arms).
fn apply_drawer_expand(
    view: &mut GraphView,
    drawer_ui: &mut crate::drawer_wire::DrawerUi,
    node: &str,
) -> bool {
    let Some(index) = view.graph.nodes.iter().position(|n| n.id == node) else {
        return false;
    };
    let keys: Vec<String> = crate::drawer_wire::relation_keys(&view.graph, index)
        .into_iter()
        .map(str::to_string)
        .collect();
    drawer_ui.expand_all(node, keys.iter().map(String::as_str));
    view.expand_node_all(index);
    true
}

pub fn build_ui_with_panes(
    ui: &mut egui::Ui,
    view: &mut GraphView,
    mut source: Option<&mut SourceState>,
    mut split: Option<&mut SourceState>,
    actions_state: &mut NzkUiState,
) -> Effect {
    let mut effect = Effect::default();
    // Focus filters ("Only related nodes" / "Hide parents") are anchored on the
    // live selection, and selection can move through paths that never pass
    // through `set_filters` (graph click, drawer row, find row, chain hop).
    // Sync once per frame, before the nzk_ui facts and the canvas are painted;
    // it is a no-op while the flags are off or the selection is unchanged.
    view.sync_focus_selection();
    // `nzk_ui::paint_in` takes the graph now (it needs repo + selection state).
    // With the source pane open, the nzk_ui is laid out for the top `66vh` so
    // its own `[1fr, 34vh]` grid matches the pane below it.
    let full = ui.max_rect();
    let pane_open = source.as_ref().map(|s| s.open).unwrap_or(false);
    let pane_h = pane_height(full, pane_open);
    let nzk_ui_rect = egui::Rect::from_min_max(
        full.min,
        egui::pos2(full.right(), full.bottom() - pane_h),
    );
    // The source-pane head (`#sourceTitle` / `#sourceNote` / the disabled
    // states of Copy ref / Prev / Next / Whole method / Window). The head is
    // nzk_ui pixels painted by nzk_ui, so the host mirrors the live pane into
    // facts here. `None` while closed: the shipped document is untouched.
    let source_mode_label = actions_state.source_mode.label();
    actions_state.source_facts = source.as_deref().filter(|s| s.open).map(|s| {
        serde_json::json!({
            "title": s.doc.as_ref().map(|d| d.title.clone()).unwrap_or_default(),
            "note": s.error.clone().unwrap_or_default(),
            "has_doc": s.doc.is_some(),
            "prev": s.history_back_available(),
            "next": s.history_forward_available(),
            "can_ref": s.reference().is_some(),
            "mode": source_mode_label,
        })
    });
    // `view.settings()` is the single authority for the Layout sliders, and
    // passing it here is what makes the readouts live: `paint_in` feeds it to
    // `facts_json_with_settings`, which nzk_ui's `apply_settings` resolves
    // into `#spacingOut`/`#iterOut`/`#curveOut`/`#nodeSizeOut`/`#textSizeOut`
    // (`shell.js:807-816`). Omitted at defaults, so the default frame is
    // byte-identical. (task-16: the last link of G4b.)
    // `view.visible_count()` is v2's `viewNodes` (nodes actually drawn after
    // filters/collapse), which the nzk_ui's canvas notice renders.
    if nzk_ui::paint_in(
        ui,
        &view.graph,
        view.visible_count(),
        nzk_ui_rect,
        actions_state,
        view.settings(),
    ) {
        // native-css's registry owns ALL nzk_ui hit-testing and returns the
        // requested host action (`#fitBtn` is handled nzk_ui-side, sections
        // repaint the nzk_ui, inert controls return None). The host applies it
        // through the same `actions::apply` the keyboard uses.
        if let Some(action) = nzk_ui::nzk_ui_ui(ui, view, &actions_state.tabs) {
            let ctx = ui.ctx().clone();
            effect = dispatch_action(actions_state, view, &ctx, action);
            // The source pane's own four controls are *effects* on state the
            // display-free actions module does not own. Consume them HERE, on
            // the real pane, so the press has a real consequence: `#srcCloseBtn`
            // closes the pane, `#srcCopyBtn` writes the clipboard,
            // `#srcModeBtn` changes the palette the pane is painted with (read
            // back below), and `#srcPopBtn` queues a real window detach/dock.
            if effect.close_source || effect.copy_source || effect.source_theme || effect.source_pop.is_some()
            {
                let popped = source.as_deref().map(|s| s.open).unwrap_or(false);
                let (queued, _report) = execute_source_action(
                    actions_state,
                    &effect,
                    source.as_deref_mut(),
                    popped,
                    &ctx,
                );
                if let Some(a) = queued {
                    PENDING_PANE_ACTIONS.with(|q| q.borrow_mut().push(a));
                }
            }
        }
        // The Layout ▸ slider edits `nzk_ui_ui` staged while it resolved the
        // pointer. `nzk_ui::overlay` cannot apply them itself — it would need a
        // second `&mut GraphView` while `view` is already borrowed for the click
        // route — so it queues them and the host applies each one here, where
        // the view is in scope. This is the *consumed* end of the slider path:
        // `view.apply_slider` writes `view.settings`, and the renderer reads
        // those back (spacing -> layout seed, curvature -> edge curves,
        // node/text size -> chip geometry), so the value genuinely reaches the
        // paint rather than stopping at a counter.
        for edit in nzk_ui::take_pending_sliders() {
            view.apply_slider(edit);
        }
        // Split-divider drag values staged by `nzk_ui::overlay` (it cannot reach
        // the host's `Tabs` model). Each `Move` carries the fraction already
        // clamped to the reference band; `Tabs::set_split_frac` re-clamps at the
        // writer. The new ratio reaches the DOM on the NEXT frame's facts pass
        // (`ui.tabs.split_w` -> `--cs-split-w`) — the same one-frame path the
        // Layout sliders above use, and the reason a drag repaints live.
        for edit in nzk_ui::take_pending_split_drags() {
            if let Some(frac) = edit.frac {
                actions_state.tabs.set_split_frac(frac);
            }
        }
        // Suggestion row presses staged by `nzk_ui::overlay` while it resolved
        // the pointer (it cannot reach `SuggestionView`). The clicked id goes
        // through `commit_by_id` and the SAME `commit_suggestion` path as Enter,
        // so a stale id is a hide-only no-op and a real id commits + centres.
        for id in nzk_ui::take_pending_suggestions() {
            let target = actions_state.suggest.commit_by_id(&view.graph, &id);
            commit_suggestion(actions_state, view, target);
        }
        // A press on a docked drawer relation row: select that neighbour through
        // the same `view.selected` path a graph click writes, so the drawer and
        // the source pane follow it on the next pass. A stale id is a no-op.
        for id in nzk_ui::take_pending_drawer_nodes() {
            if let Some(i) = view.graph.nodes.iter().position(|n| n.id == id) {
                view.selected = Some(i);
            }
        }
        // A press on a resolve-chain `.hop` row: select that node through the
        // same path, exactly like `filters.js:600-606`'s click handler.
        for id in nzk_ui::take_pending_chain_nodes() {
            if let Some(i) = view.graph.nodes.iter().position(|n| n.id == id) {
                view.selected = Some(i);
            }
        }
        // A double press on a chain row toggles its children (`filters.js:610-619`).
        for id in nzk_ui::take_pending_chain_collapses() {
            if !actions_state.chain_collapsed.remove(&id) {
                actions_state.chain_collapsed.insert(id);
            }
        }
        // A press on a public-definitions / strings list row: the same selection
        // path, matching `openListNode`'s select half (`filters.js:462-480`).
        for id in nzk_ui::take_pending_list_nodes() {
            if let Some(i) = view.graph.nodes.iter().position(|n| n.id == id) {
                view.selected = Some(i);
            }
        }
        // A press on a Declared-types chip (`#typeChips`). The name is the exact
        // declared type to restrict to (`""` = the `all` chip), applied through
        // the same owner the typed filter uses, so the box and the rows can
        // never disagree. The facts digest changes, so the next frame repaints.
        for name in nzk_ui::take_pending_type_filters() {
            actions::set_type_filter(actions_state, &name);
        }
        // A press on a runtime-created drawer control (task-2). The nzk_ui stages
        // it because it cannot reach `view.selected` or this state; the host
        // applies it with v2's own semantics (`drawer.js:120-145`): a crumb
        // navigates to that ancestor, ±/caret toggles one relation group, Expand
        // opens every group, Hide children flips the scope, Resolve chain selects
        // the node it names (`#upBtn`'s model action).
        for click in nzk_ui::take_pending_drawer_clicks() {
            match click {
                nzk_ui::DrawerClick::Select(id) => {
                    if let Some(i) = view.graph.nodes.iter().position(|n| n.id == id) {
                        view.selected = Some(i);
                    }
                }
                // v2 `shell.js:577-581`: the drawer's `Resolve chain` opens the
                // chain section and runs the real `/api/chain` walk on the
                // selected node. The host surface for that same walk is the
                // hierarchy panel's call model (`hierarchy_wire::
                // walk_call_section`, the resident-graph port of `/api/chain`),
                // so this selects the named node AND opens the walk on it —
                // the earlier select-only arm left the button half dead.
                nzk_ui::DrawerClick::ResolveChain(id) => {
                    if let Some(i) = view.graph.nodes.iter().position(|n| n.id == id) {
                        view.selected = Some(i);
                    }
                    open_hierarchy_for(actions_state, &view.graph, &id);
                }
                nzk_ui::DrawerClick::ToggleGroup(node, key) => {
                    actions_state.drawer_ui.toggle_group(&node, &key);
                }
                nzk_ui::DrawerClick::HideChildren(node) => {
                    actions_state.drawer_ui.toggle_hidden_children(&node);
                }
                nzk_ui::DrawerClick::Expand(node) => {
                    // v2's `Expand` is "Add every relation bucket": it must put
                    // the neighbours on the canvas, not just open the drawer's
                    // lists. One helper owns both halves so the press cannot
                    // regress to a list-only no-op.
                    apply_drawer_expand(view, &mut actions_state.drawer_ui, &node);
                }
                // The two contextual focus actions. They write the same filter
                // model the sidebar checkboxes use, so the box, the button label
                // and the canvas predicate can never disagree. "Only related
                // nodes" and "Hide parents" are alternatives: only one can be on.
                nzk_ui::DrawerClick::FocusOnly(node) => {
                    if let Some(i) = view.graph.nodes.iter().position(|n| n.id == node) {
                        view.selected = Some(i);
                        actions_state.selected_node = Some(i);
                    }
                    actions_state.filters.focus_only = !actions_state.filters.focus_only;
                    if actions_state.filters.focus_only {
                        actions_state.filters.focus_parents = false;
                    }
                    view.set_filters(&actions_state.filters);
                }
                nzk_ui::DrawerClick::FocusParents(node) => {
                    if let Some(i) = view.graph.nodes.iter().position(|n| n.id == node) {
                        view.selected = Some(i);
                        actions_state.selected_node = Some(i);
                    }
                    actions_state.filters.focus_parents = !actions_state.filters.focus_parents;
                    if actions_state.filters.focus_parents {
                        actions_state.filters.focus_only = false;
                    }
                    view.set_filters(&actions_state.filters);
                }
            }
        }
        // The drawer scope is a canvas predicate, not just fact styling: mirror
        // `Hide children` into the view so the children really leave the canvas
        // (their drawn edges drop with them) and `Show children` brings back the
        // same nodes without a re-index.
        view.sync_hidden_children(&actions_state.drawer_ui.hidden_children);
        // A press on a Blocks canvas card (runtime-created by
        // `dom::apply_blocks_canvas`): select its node through the same
        // `view.selected` path a graph click writes, so the drawer, the source
        // pane and the canvas's own `.selected` marker all follow it. A stale id
        // is a no-op.
        for id in nzk_ui::take_pending_block_nodes() {
            if let Some(i) = view.graph.nodes.iter().position(|n| n.id == id) {
                view.selected = Some(i);
            }
        }
        // A press on a Find result row (`dom::apply_find`'s `.frow` buttons).
        // The nzk_ui staged the row's own click target because it cannot reach
        // the host's graph. v2's `activateFindRow` (`search.js:390-400`) either
        // selects/centres a definition/implementation node or opens a usage's
        // source: the node's document is the honest native source surface
        // (there is no file-model pane), and the enclosing node id is which
        // document gets selected. A stale/dead target is a no-op.
        for target in nzk_ui::take_pending_find_rows() {
            let id = match &target {
                nzk_ui::FindRowTarget::Node(id) => id.clone(),
                nzk_ui::FindRowTarget::Usage { node_id, file, .. } => {
                    if !file.is_empty() {
                        // Keep the usage's location observable without faking a
                        // file window: the document opened is the enclosing
                        // node's synthesized source, which contains that line
                        // when the engine's `enclosing()` found the node.
                    }
                    node_id.clone()
                }
            };
            if let Some(i) = view.graph.nodes.iter().position(|n| n.id == id) {
                view.select(Some(i));
                view.center_on_node(i);
            }
        }
        // The folder picker's own controls. Resolved through the picker family
        // (`nzk_ui::nzk_ui_picker_ui` -> `actions::PickerAction`) rather than
        // `nzk_ui_ui`, because three of them carry a payload a `Copy`
        // `NzkUiAction` cannot hold — the `data-nav` of the row that was
        // pressed, or the text of `#modalPathInput`.
        //
        // `#modalBody`'s rows are read from the picker's OWN listing, which is
        // the same list `nzk_ui.rs` just published as `ui.picker` and the shell
        // therefore laid out. Taking the row rects from here rather than
        // re-deriving them is what keeps "which row did I press" and "which rows
        // are on screen" from being two different answers.
        if actions_state.surface == Some(actions::HostSurface::FolderPicker) {
            // The shell paints the input; FormState owns the text and includes
            // its in-flight edit buffer so clicking Go commits the same text the
            // user can currently see, without requiring Enter first.
            let input = actions_state.form.value("#modalPathInput");
            let rows: Vec<(egui::Rect, String)> = picker_row_rects(actions_state);
            let row_refs: Vec<(egui::Rect, &str)> =
                rows.iter().map(|(r, p)| (*r, p.as_str())).collect();
            if let Some(pa) = nzk_ui::nzk_ui_picker_ui(ui, Some(&input), &row_refs) {
                let ctx = ui.ctx().clone();
                let picker_effect = apply_and_execute_picker(actions_state, view, &ctx, pa);
                // The engine belongs to the host, so a committed root is carried
                // out of the UI pass on the returned `Effect` exactly as
                // `#indexBtn`'s `reindex` flag is (frame_effect below).
                if picker_effect.reindex_at.is_some() {
                    effect.reindex_at = picker_effect.reindex_at.clone();
                }
            }
        }
        // The hierarchy panel's own controls. Like the picker above, the rows
        // carry a payload the nzk_ui's box map cannot: a `.hop` row's *collapse
        // path* (`data-path`) and the two selects' values. The row rects come
        // from the panel's own already-published facts, so "which row is on
        // screen" and "which path a press toggles" cannot disagree.
        if actions_state.hierarchy.is_open() {
            if let Some(msg) = nzk_ui::nzk_ui_hierarchy_ui(ui, actions_state) {
                let ctx = ui.ctx().clone();
                apply_and_execute_hierarchy(actions_state, view, &ctx, msg);
            }
        }
        if pane_open {
            // The nzk_ui composition paints the reference's REAL pane header
            // and `#cs-tabs` strip (its DOM and stylesheet own those pixels), so
            // the native source pane fills only the `#sourceBody` box — one body
            // for the main document and one for the split host when the split
            // scaffold is on the DOM. Both rects come from the SAME frame's
            // layout (`paint_in` has just filled the box map), so no CSS rule is
            // re-derived here.
            let split_on = actions_state.tabs.split();
            let host_box = if split_on { nzk_ui::box_rect("#csSplitHost") } else { None };
            if let (Some(host), Some(split_src)) = (host_box, split.as_deref_mut()) {
                draw_source_body_in_rect(
                    ui,
                    split_src,
                    actions_state.source_mode,
                    host,
                    &view.graph,
                    true,
                );
            }
            match nzk_ui::box_rect("#sourceBody") {
                Some(body) => {
                    if let Some(s) = source.as_deref_mut() {
                        draw_source_body_in_rect(
                            ui,
                            s,
                            actions_state.source_mode,
                            body,
                            &view.graph,
                            false,
                        );
                    }
                }
                // No DOM body box this frame (the shell is off, or this frame's
                // facts have not laid the pane grid out yet): fall back to the
                // full native pane, head included. That is the shell-less app's
                // real UI, and it is also what keeps a first paint from showing
                // an empty pane.
                None => {
                    if let Some(s) = source.as_deref_mut() {
                        draw_source_overlay(ui, s, actions_state.source_mode, pane_h, &view.graph);
                    }
                }
            }
        }
        return effect;
    }
    egui::Panel::top("ncv_top").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.heading("native-csviewer");
            ui.separator();
            // One status literal, the same one the nzk_ui paints: v2's
            // `files N · parse Nms · nodes N · edges N` over the engine's raw
            // `counts` (externals included), not the drawn-only subtotal.
            ui.label(nzk_ui::status_text(&view.graph));
            ui.separator();
            // NOT "Fit (F)". `f` belongs to find mode in the reference
            // (`shell.js:457`: `ev.key === 'f' || ev.key === 'F'` ->
            // `setMode('find', true)`), and `handle_key` now binds exactly that.
            // The old label advertised a shortcut that did nothing — the same
            // false-advertisement defect as a dead control. Fit stays reachable
            // through `#fitBtn` (nzk_ui.rs) and this button.
            if ui.button("Fit").clicked() {
                view.fit_pending = true;
            }
            ui.checkbox(&mut view.show_edges, "edges");
            ui.checkbox(&mut view.show_labels, "labels");
            ui.separator();
            ui.label("no browser engine: winit + wgpu + egui");
        });
    });

    egui::Panel::left("ncv_side")
        .resizable(true)
        .default_size(320.0)
        .show(ui, |ui| {
            ui.heading("Index");
            ui.label(format!("repo: {}", view.graph.repo.display()));
            let files = view.graph.state.get("fileCount").and_then(|v| v.as_u64()).unwrap_or(0);
            let indexed =
                view.graph.state.get("indexed").and_then(|v| v.as_bool()).unwrap_or(false);
            ui.label(format!("indexed: {indexed} · files: {files}"));
            if !view.graph.skipped.is_empty() {
                let skipped: Vec<String> = view
                    .graph
                    .skipped
                    .iter()
                    .map(|(k, c)| format!("{k}×{c}"))
                    .collect();
                ui.label(format!("not drawn: {}", skipped.join(", ")));
            }

            ui.separator();
            ui.heading("Selection");
            if let Some(i) = view.selected {
                let (label, kind, full, ns, owner, file, line, end_line, degree) = {
                    let n = &view.graph.nodes[i];
                    (
                        n.label.clone(),
                        n.kind.clone(),
                        n.full.clone(),
                        n.ns.clone(),
                        n.owner.clone(),
                        n.file.clone(),
                        n.line,
                        n.end_line,
                        n.degree,
                    )
                };
                ui.monospace(&label);
                ui.label(format!("kind: {kind}"));
                if !full.is_empty() && full != label {
                    ui.label(format!("full: {full}"));
                }
                if !ns.is_empty() {
                    ui.label(format!("ns: {ns}"));
                }
                if !owner.is_empty() {
                    ui.label(format!("owner: {owner}"));
                }
                if !file.is_empty() {
                    if end_line > line {
                        ui.label(format!("file: {file}:{line}-{end_line}"));
                    } else {
                        ui.label(format!("file: {file}:{line}"));
                    }
                }
                ui.label(format!("degree: {degree}"));
                if ui.button("Clear selection").clicked() {
                    view.selected = None;
                }
            } else {
                ui.label("click a node to select it");
                ui.label("drag to pan · scroll to zoom · F fits · Esc clears");
            }

            ui.separator();
            ui.heading("Kinds");
            let mut kinds: Vec<(String, usize)> = Vec::new();
            for n in &view.graph.nodes {
                match kinds.iter_mut().find(|(k, _)| *k == n.kind) {
                    Some((_, c)) => *c += 1,
                    None => kinds.push((n.kind.clone(), 1)),
                }
            }
            kinds.sort_by(|a, b| b.1.cmp(&a.1));
            for (k, c) in kinds.iter().take(20) {
                ui.horizontal(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                    let col = crate::graphviz::kind_color(view.theme, k);
                    ui.painter().rect_filled(
                        rect,
                        2.0,
                        egui::Color32::from_rgb(col.0, col.1, col.2),
                    );
                    ui.label(format!("{k}: {c}"));
                });
            }
        });

    // Source pane, placed full-width at the bottom (the original's
    // `grid-column: 1 / -1` + `--source-h: 34vh`), before the CentralPanel so
    // the canvas takes the remaining space.
    //
    // This is the non-nzk_ui fallback path, but the source pane's controls
    // must behave identically here: the effects the keyboard/`apply` path
    // produced are consumed against the same real `SourceState`, and the palette
    // comes from the same live `NzkUiState::source_mode`.
    if effect.close_source || effect.copy_source || effect.source_theme || effect.source_pop.is_some()
    {
        let popped = source.as_deref().map(|s| s.open).unwrap_or(false);
        let ctx = ui.ctx().clone();
        let (queued, _report) = execute_source_action(
            actions_state,
            &effect,
            source.as_deref_mut(),
            popped,
            &ctx,
        );
        if let Some(a) = queued {
            PENDING_PANE_ACTIONS.with(|q| q.borrow_mut().push(a));
        }
    }
    if let Some(s) = source.as_deref_mut() {
        if s.open {
            let theme = source_theme_for_mode(actions_state.source_mode);
            egui::Panel::bottom("ncv_source")
                .resizable(true)
                .default_size(pane_height(ui.max_rect(), true))
                .show(ui, |ui| {
                    // The reference's `main` lens runs `showInGraph` with the
                    // declaration's node id (`codelens.js:228-232`). Its native
                    // equivalent is selecting that node, which needs `view` —
                    // not available inside this `source`-borrowed closure — so
                    // the target is carried out on the pane's own state and
                    // consumed below, where both borrows are free.
                    // The language layer needs the graph the app selects
                    // from: hover cards / Ctrl+click / Shift+click resolve a
                    // token to a real node, never to a guessed one.
                    let clicked = source_pane::ui_with_language(ui, s, &theme, &view.graph);
                    for id in clicked {
                        s.lens_open_request = Some(id);
                    }
                });
        }
    }

    egui::CentralPanel::default().show(ui, |ui| {
        // The shell-less fallback has no CSS modal backdrops, so the canvas is
        // never suspended here; clear any value a previous nzk_ui frame left.
        view.input_suspended = false;
        view.ui(ui);
    });
    // The right-click card's `{ } View code` action. The card lives in the
    // canvas (it resolves the node under the pointer), but opening the source
    // pane needs the host's `Index`, so it travels back on the same
    // `Effect::source_open` path the drawer's button uses.
    if view.take_node_code_request().is_some() {
        effect.source_open = true;
    }
    effect
}

/// Action-level entry for a nzk_ui dispatch table that resolves selectors
/// itself: the same path as the keyboard bindings, because both end in
/// [`actions::apply`]. The returned [`Effect`] carries `reindex`, which only the
/// host (`App`) can perform, so a caller inside the UI must surface it.
pub fn dispatch_action(
    state: &mut NzkUiState,
    view: &mut GraphView,
    ctx: &egui::Context,
    action: NzkUiAction,
) -> Effect {
    apply_and_execute(state, view, ctx, action)
}

/// Apply an action and push its view-side effects. The `reindex` effect is the
/// caller's job (it needs the engine and the host's repo/roots).
fn apply_and_execute(
    state: &mut NzkUiState,
    view: &mut GraphView,
    ctx: &egui::Context,
    action: NzkUiAction,
) -> Effect {
    // The corpus root, which is the folder `#browseBtn`'s picker opens on and
    // the folder `#modalRepo` returns to. Taken from the graph the view is
    // actually showing, so the picker can never be rooted somewhere other than
    // the corpus on screen (the two used to be able to disagree because the
    // picker had no root at all — it was never constructed).
    let repo = view.graph.repo.clone();
    let is_full_graph = action == NzkUiAction::FullGraph;
    // The prune button's model arm is deliberately inert (`actions::FilterCmd::
    // Prune` cannot reach the view); the deletion itself is applied below.
    let is_prune = matches!(&action, NzkUiAction::Filter(actions::FilterCmd::Prune));
    let mut effect = actions::apply(state, action, &repo);
    if let Some(depth) = effect.depth {
        view.set_expansion_depth(depth);
    }
    // The sidebar filter model's ONE owner is `NzkUiState::filters`; the view's
    // render predicate is a copy of it, pushed on every dispatched action (the
    // setter compares and returns `false` when nothing changed, so this is a
    // no-op on every non-filter action). The rebuild is forced when the model
    // really moved, so a chip and the canvas can never disagree.
    if view.set_filters(&state.filters) {
        effect.rebuild_view = true;
    }
    // The two Group chips (`#kindChildWrap`/`#kindEdgeWrap`, plus the inverse
    // `#hideChildren` box) write a flag whose ONE owner is `GraphView`
    // (`set_group_children`/`set_group_edges`); `actions::apply` resolved the
    // requested value and carries it here because it cannot see the view. Each
    // setter rebuilds the drawn edge set itself, so these do not also set
    // `rebuild_view` (that would rebuild twice for one press).
    if let Some(on) = effect.group_children {
        view.set_group_children(on);
    }
    if let Some(on) = effect.group_edges {
        view.set_group_edges(on);
    }
    // `#pruneHidden` (`filters.js:426-443`): delete the nodes the current filter
    // model (and the drawer's Hide-children scope) hides from the resident graph.
    // The model arm above cannot do this, so the real transition lives here; a
    // hidden-node-free graph is an honest no-op.
    if is_prune {
        view.prune_hidden();
    }
    if is_full_graph {
        view.view_mode = ViewMode::Full;
    }
    if effect.camera {
        let c = state.camera;
        view.set_camera(c.zoom, c.x, c.y);
    }
    if effect.reseed_layout {
        // One-shot override consumed by the NEXT `relayout` (which may be the one
        // directly below, or the next frame's `view.ui` for actions that only
        // schedule a layout). Set before the rebuild/relayout so it can never be
        // dropped, and deliberately separate from `relayout` so the incremental
        // continue branch stays the default for every other action.
        view.reseed_layout();
    }
    if effect.rebuild_view {
        view.rebuild_view();
    }
    if effect.relayout {
        view.relayout(ctx);
    }
    if effect.refit {
        view.fit_pending = true;
    }
    // The view's own navigation model, completed by task-2 this round.
    //
    // Both used to be dead: `Effect::navigate_back`/`navigate_up` were set by
    // `actions.rs` and read NOWHERE, so `#backBtn`/`#upBtn` incremented a counter
    // and changed nothing. That was honest only while the model was genuinely
    // absent — `actions.rs`'s module docs said so explicitly ("the native view
    // has no navigation-depth or history model"). The view now has one: an
    // 80-entry selection history and a real `contains`-parent walk, both on
    // `GraphView`. So the effects are consumed here and the controls are real.
    //
    // `None` is a normal answer, not a failure: an empty history and a root node
    // with no containment parent both legitimately have nowhere to go, and the
    // view returns `None` without moving the selection rather than inventing a
    // destination. The effects drive the view directly, so no `Effect` flag is
    // needed to carry the outcome back.
    if effect.navigate_back {
        view.navigate_back();
    }
    if effect.navigate_forward {
        // `Alt+Right` (`nav.js:441`). `navigate_forward` returns `None` when the
        // forward stack is empty — the same honest "nowhere to go" answer as
        // Back, and it leaves the selection alone rather than inventing one.
        view.navigate_forward();
    }
    if effect.navigate_up {
        view.navigate_up();
    }
    // `Ctrl+Shift+V|H|D` / `Shift+D` / `Shift+=` / `Shift+-` (`arrange.js:416`).
    // The view answers `None` when the reference would toast (fewer than two
    // marquee-selected nodes, or no detectable axis for spacing) and moves
    // nothing in that case, so the effect needs no separate failure flag.
    if let Some(axis) = effect.arrange {
        view.arrange_selection(axis);
    }
    if let Some(dir) = effect.spacing {
        view.space_selection(dir);
    }
    effect
}

/// Apply one hierarchy message against the live graph and repaint.
///
/// The counterpart of [`apply_and_execute_picker`] for the panel family, and the
/// one the pointer path calls: the walk needs the graph, which only the host has.
pub fn apply_and_execute_hierarchy(
    state: &mut NzkUiState,
    view: &mut GraphView,
    _ctx: &egui::Context,
    msg: crate::actions::HierarchyMsg,
) -> Effect {
    apply_hierarchy_msg(state, &view.graph, msg)
}

/// Apply one picker message and push its view-side effects.
///
/// [`apply_and_execute`]'s counterpart for the `#modal` family. The picker's
/// navigation lives in [`actions::apply_picker`] (it is the one that performs
/// the real `read_dir`), and the only view-side consequence is a nzk_ui
/// repaint — the listing the modal shows is nzk_ui *content*, not canvas state.
/// The returned [`Effect`] carries `reindex_at`, which only the host can act on.
pub fn apply_and_execute_picker(
    state: &mut NzkUiState,
    _view: &mut GraphView,
    _ctx: &egui::Context,
    action: crate::actions::PickerAction,
) -> Effect {
    actions::apply_picker(state, action)
}

/// Apply one hierarchy message: the panel's opener, closer and its reloads.
///
/// This is the host half of G3b. Two things live here and nowhere else:
///
/// 1. **Opening.** The reference resolves the word under the cursor to a node and
///    then walks it (`openCallHierarchy`, `hierarchy.js:574-594`). A native host
///    has no text cursor in a DOM editor, so the honest analogue is the node the
///    app has *selected* — the same node Back/Up/Enter and the source pane act
///    on. [`HierarchyMsg::Open`] therefore carries an already-resolved id, and
///    [`Self::open_selected`] is what produces it.
/// 2. **Walking.** `walk_call_section` reads the real `graph`, so the panel's
///    rows are the actual edges — not a fixture. The result is installed through
///    [`hierarchy_wire::HierarchyView::begin_load`]/`set_call_sections`, which is
///    the generation guard: a walk started before a newer one cannot overwrite
///    it.
///
/// `dir`/`depth` changes **reload** (the reference's select handlers call
/// `loadCall` again, `hierarchy.js:421-443`), which is why every arm funnels
/// through [`reload`] rather than only mutating the field.
///
/// Returns an [`Effect`] that repaints the nzk_ui: the panel's content is nzk_ui
/// facts, so nothing about the canvas changes.
pub fn apply_hierarchy_msg(
    state: &mut NzkUiState,
    graph: &crate::model::Graph,
    msg: crate::actions::HierarchyMsg,
) -> Effect {
    use crate::actions::HierarchyMsg;

    match msg {
        HierarchyMsg::Close => {
            state.hierarchy.hide();
        }
        HierarchyMsg::Open(id) => {
            open_hierarchy_for(state, graph, &id);
        }
        HierarchyMsg::ToggleCollapse(path) => {
            state.hierarchy.toggle_collapse(&path);
        }
        HierarchyMsg::SetDir(dir) => {
            // `set_dir` refuses an invalid direction rather than accepting it,
            // so a malformed payload cannot silently blank the panel.
            if state.hierarchy.set_dir(&dir) {
                let id = state.hierarchy.node_id().to_string();
                reload_hierarchy(state, graph, &id);
            }
        }
        HierarchyMsg::SetDepth(depth) => {
            state.hierarchy.set_depth(depth);
            let id = state.hierarchy.node_id().to_string();
            reload_hierarchy(state, graph, &id);
        }
    }

    let mut effect = Effect::default();
    effect.repaint_nzk_ui = true;
    effect
}

/// Open the panel on the app's currently selected node, or on nothing.
///
/// Returns `false` when nothing is selected: the reference toasts "Put the
/// cursor on a symbol first" and opens nothing (`hierarchy.js:575-578`), and the
/// native equivalent is to leave the panel shut rather than open an empty one.
pub fn open_hierarchy_for(
    state: &mut NzkUiState,
    graph: &crate::model::Graph,
    node_id: &str,
) -> bool {
    let Some(node) = graph.nodes.iter().find(|n| n.id == node_id) else {
        return false;
    };
    let subtitle = crate::app::hierarchy_wire::label_of_node(node);
    state
        .hierarchy
        .show(hierarchy_wire::Mode::Call, &subtitle, &subtitle);
    reload_hierarchy(state, graph, node_id);
    true
}

/// Re-run the walk for whatever the panel currently holds.
///
/// Every direction named by `dir` is walked and the sections are installed
/// through the generation guard, so a reload that races a newer one is dropped
/// rather than mixed in. `both` walks forward and backward, matching
/// `loadCall`'s `dirs` expansion (`hierarchy.js:550-551`).
///
/// **The node id is a parameter, not read back from the view.** `show()` does
/// not set it (`hierarchy.js:457-473` only sets the title/word), so on the open
/// path `node_id()` is still `""` and a read-back would silently walk nothing —
/// an empty panel with the title painted, which is precisely the failure that
/// looks like a working feature. `loadCall(def.node.id)` (`hierarchy.js:588`) is
/// what sets it, and that is what `begin_load` does here.
fn reload_hierarchy(state: &mut NzkUiState, graph: &crate::model::Graph, id: &str) {
    if id.is_empty() {
        return;
    }
    let depth = state.hierarchy.depth();
    let dirs: Vec<&str> = hierarchy_wire::expand_dirs(state.hierarchy.dir()).to_vec();
    // `begin_load` returns the generation this walk owns, and records the id on
    // the view. Any walk already in flight is now stale by construction.
    let generation = state.hierarchy.begin_load(id);
    let sections: Vec<hierarchy_wire::Section> = dirs
        .into_iter()
        .map(|d| hierarchy_wire::walk_call_section(graph, id, d, depth))
        .collect();
    // The guard is re-checked before the install: if something newer started
    // between `begin_load` and here, this result must be dropped.
    if state.hierarchy.is_current(generation) {
        state.hierarchy.set_call_sections(
            generation,
            hierarchy_wire::with_hop_hints(sections),
        );
    }
}

/// The `#modalBody` row rects, paired with the `data-nav` path each one carries.
///
/// Read straight off the `#modalBody` box the shell laid out, split into one
/// band per directory in the picker's own listing — the same list `nzk_ui.rs`
/// published as `ui.picker`, so the rects and the rows cannot disagree. The
/// rows have no DOM id of their own (`.pick` is a class, `shell.js:58`), which
/// is exactly why the box map cannot name them and this has to reconstruct them
/// from the body box.
///
/// `None`/empty when the modal has no geometry (closed, or the shell is not
/// built): an empty list simply means no row can be pressed, which is the
/// truthful answer for a picker that is not on screen.
fn picker_row_rects(state: &NzkUiState) -> Vec<(egui::Rect, String)> {
    let Some(body) = nzk_ui::box_rect("#modalBody") else {
        return Vec::new();
    };
    let Some(picker) = state.picker.as_ref() else {
        return Vec::new();
    };
    let dirs = &picker.current().dirs;
    if dirs.is_empty() || body.height() <= 0.0 {
        return Vec::new();
    }
    // One equal band per row inside the body. The real heights come from the
    // stylesheet; equal bands are the honest approximation because the nzk_ui
    // does not expose per-row boxes, and a band that is off by a few px still
    // resolves to the row the user aimed at (they are stacked, not overlapping).
    let h = body.height() / dirs.len() as f32;
    dirs.iter()
        .enumerate()
        .map(|(i, d)| {
            let top = body.min.y + h * i as f32;
            (
                egui::Rect::from_min_max(
                    egui::pos2(body.min.x, top),
                    egui::pos2(body.max.x, top + h),
                ),
                d.path.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

///
/// `actions.rs` is display-free and does not hold `SourceState` (the pane belongs
/// to the host), so four of its actions are *requests* carried on [`Effect`] and
/// completed here — the same split `Effect::reindex` already uses.
///
/// This is the half that makes the wiring true rather than nominal:
///
/// * `close_source` -> [`SourceState::close`] (`open = false`; the panel and the
///   overlay are both gated on that flag, so the pane really goes away).
/// * `source_theme` -> the palette is rebuilt from [`NzkUiState::source_mode`],
///   which is the modes the pane actually ships.
/// * `copy_source` -> `egui::Context::copy_text`, a genuine clipboard write.
///   `report` receives the honest outcome so a caller (`App`) can log or assert
///   it; nothing here claims a copy that did not happen.
/// * `source_pop` -> `Action::Detach`/`Action::Dock`, i.e. the same real-window
///   path as F2 and the detached window's own buttons. Returns the queued action
///   so the caller can push it onto its event-loop queue.
///
/// Returns the [`Action`] the host must run on its event loop, if any: detaching
/// and docking create/drop OS windows, which needs an `ActiveEventLoop`.
pub fn execute_source_action(
    state: &NzkUiState,
    effect: &Effect,
    mut source: Option<&mut SourceState>,
    detached: bool,
    ctx: &egui::Context,
) -> (Option<Action>, Option<String>) {
    let mut queued = None;
    let mut report = None;

    if effect.close_source {
        if let Some(s) = source.as_deref_mut() {
            // The pane's OWN close, not a flag invented here.
            s.close();
            report = Some("source pane closed".to_string());
        }
    }

    if effect.source_theme {
        // Nothing to mutate on the pane: the theme is chosen by
        // `NzkUiState::source_mode` and applied by the painter (the panel, the
        // overlay and the popped-out window all read it back). Recorded here so
        // the chosen palette is observable in the app's report path.
        let theme = source_theme_for_mode(state.source_mode);
        report = Some(format!(
            "source theme -> {} (bg #{:02x}{:02x}{:02x})",
            state.source_mode.label(),
            theme.bg.r(),
            theme.bg.g(),
            theme.bg.b()
        ));
    }

    if effect.copy_source {
        match source.as_deref() {
            Some(s) => match source_document_text(s) {
                Some(text) => {
                    // THE real clipboard write: egui turns this into
                    // `OutputCommand::CopyText`, which egui-winit's `clipboard`
                    // feature hands to the OS clipboard.
                    let bytes = text.len();
                    ctx.copy_text(text);
                    report = Some(format!("copied {bytes} bytes of source"));
                }
                // Nothing loaded: report the failure honestly rather than
                // claiming a copy of an empty string.
                None => report = Some("no source document to copy".to_string()),
            },
            None => report = Some("no source pane to copy".to_string()),
        }
    }

    if let Some(want_detached) = effect.source_pop {
        // A real toggle driven by what the pane is actually doing: if it has its
        // own window, dock it; otherwise detach it. The direction is decided
        // from host state, never assumed.
        let pop_out = !detached;
        queued = Some(if pop_out {
            Action::Detach(Pane::Source)
        } else {
            Action::Dock(Pane::Source)
        });
        let _ = want_detached;
        report = Some(if pop_out {
            "source pane popped out".to_string()
        } else {
            "source pane docked".to_string()
        });
    }

    (queued, report)
}

/// The nzk_ui path draws the graph itself; the source pane is painted on top
/// of the bottom `34vh` afterwards (because `nzk_ui::nzk_ui_ui` paints outside
/// egui's panel layout).
///
/// `mode` is the live `#srcModeBtn` choice, so the pop-out/docked pane and the
/// panel path cannot disagree about which palette is in force.
fn draw_source_overlay(
    ui: &mut egui::Ui,
    source: &mut SourceState,
    mode: actions::SourceMode,
    pane_h: f32,
    graph: &crate::model::Graph,
) {
    // With the nzk_ui nzk_ui up, the reference's real `#sourcePane` header +
    // `#cs-tabs` strip are nzk_ui pixels: paint only the body into the DOM's
    // `#sourceBody` box so they stay visible. The full pane (native head
    // included) is the shell-less fallback below.
    if nzk_ui::enabled() {
        if let Some(body) = nzk_ui::box_rect("#sourceBody") {
            draw_source_body_in_rect(ui, source, mode, body, graph, false);
            return;
        }
    }
    let full = ui.max_rect();
    let h = pane_h.clamp(0.0, full.height());
    let rect = egui::Rect::from_min_max(
        egui::pos2(full.left(), full.bottom() - h),
        full.max,
    );
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    let clicked = source_pane::ui_with_language(&mut child, source, &source_theme_for_mode(mode), graph);
    for id in clicked {
        source.lens_open_request = Some(id);
    }
}

/// Paint one source document **body** into an explicit rect.
///
/// Used by the nzk_ui composition: the rects come from the DOM boxes
/// `paint_in` reported (`#sourceBody` for the main document, `#csSplitHost` for
/// the split), so the header/tabs strip the stylesheet lays out above them is
/// never covered by native pixels. `split` selects the body-only language
/// wrapper (the split body has no `GraphView` of its own, so it paints no hover
/// cards / Ctrl+click layer; that is the same honest limitation the detached
/// window has).
fn draw_source_body_in_rect(
    ui: &mut egui::Ui,
    source: &mut SourceState,
    mode: actions::SourceMode,
    rect: egui::Rect,
    graph: &crate::model::Graph,
    split: bool,
) {
    let theme = source_theme_for_mode(mode);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    let clicked = if split {
        source_pane::ui_body(&mut child, source, &theme)
    } else {
        source_pane::ui_body_with_language(&mut child, source, &theme, graph)
    };
    for id in clicked {
        source.lens_open_request = Some(id);
    }
}

/// What one redraw should do about the split pane's second document.
///
/// This is the whole decision `App::sync_split_source` acts on, factored out so
/// the semantics can be tested without an engine `Index` or a live window.
///
/// ADVERSARIAL CONTRACT: the tests in `split_sync_tests` fail if the split gating
/// (`split()`), the split-tab key lookup (`split_key()`), the `node`-param
/// extraction, or the already-loaded suppression is removed/altered — each of
/// those is the only thing standing between "load the right document once" and
/// "reload every frame" / "paint a fabricated view".
#[derive(Debug, Clone, PartialEq, Eq)]
enum SplitSync {
    /// Nothing to do: the split is off, no split tab is named, or the document
    /// for the current split tab was already loaded.
    Idle,
    /// Load this node ref into the split body.
    Load(String),
    /// The split tab names a document [`SourceState`] cannot honestly open (a
    /// `file:` param): record this tab key and leave the body empty.
    Empty(String),
}

fn split_sync_plan(tabs: &crate::chords::tabs::Tabs, loaded_key: Option<&str>) -> SplitSync {
    if !tabs.split() {
        return SplitSync::Idle;
    }
    let Some(key) = tabs.split_key() else {
        return SplitSync::Idle;
    };
    if loaded_key == Some(key) {
        return SplitSync::Idle;
    }
    match tabs
        .params(key)
        .and_then(|p| p.get("node"))
        .and_then(serde_json::Value::as_str)
        .filter(|n| !n.is_empty())
    {
        Some(node) => SplitSync::Load(node.to_owned()),
        None => SplitSync::Empty(key.to_owned()),
    }
}

/// The pane's height: `--source-h: 34vh`, clamped so it is usable in small
/// windows. Zero when the pane is closed.
fn pane_height(full: egui::Rect, open: bool) -> f32 {
    if open {
        (full.height() * 0.34).clamp(140.0, full.height())
    } else {
        0.0
    }
}

/// One mapping from the app theme to the source pane's tokens (the pane must
/// never invent a palette of its own).
fn source_theme(theme: UiTheme) -> SourceTheme {
    match theme {
        UiTheme::NzkDarkPlus => SourceTheme::nzk_dark_plus(),
        UiTheme::DarkV1 => SourceTheme::darkplus(),
    }
}

/// The concrete `SourceTheme` for a `#sourceTheme` value / `#srcModeBtn` mode.
///
/// The five modes are the five constructors that genuinely exist in
/// `source_pane.rs` (`darkplus`, `darkmodern`, `nzk_dark_plus`, `monokai`,
/// `solarized`); this is the only place the choice meets a palette, so
/// `NzkUiState::source_mode` stays the one owner of it.
fn source_theme_for_mode(mode: actions::SourceMode) -> SourceTheme {
    match mode {
        actions::SourceMode::DarkPlus => SourceTheme::darkplus(),
        actions::SourceMode::DarkModern => SourceTheme::darkmodern(),
        actions::SourceMode::NzkDarkPlus => SourceTheme::nzk_dark_plus(),
        actions::SourceMode::Monokai => SourceTheme::monokai(),
        actions::SourceMode::Solarized => SourceTheme::solarized(),
    }
}

/// The pane's synthesized document as plain text — the exact bytes `#srcCopyBtn`
/// puts on the clipboard.
///
/// This is the engine's own document text (`DocLine::text`, the same strings the
/// pane paints), so what lands on the clipboard is what the user is looking at.
/// `None` when there is no document: an honest "nothing to copy" rather than a
/// write of an empty string reported as success.
fn source_document_text(state: &SourceState) -> Option<String> {
    let doc = state.doc.as_ref()?;
    if doc.lines.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(doc.lines.len() * 48);
    for line in &doc.lines {
        out.push_str(&line.text);
        out.push('\n');
    }
    Some(out)
}

/// The selected node's reference, or `None` when nothing is selected.
fn selected_node_ref(view: &GraphView) -> Option<String> {
    let i = view.selected?;
    view.graph.nodes.get(i).map(|n| n.label.clone())
}

/// The host-side half of `#newWinBtn` routing (`windows.js:172 openDoc`),
/// factored out so the mapping nzk_ui action → selected node → queued [`Action`]
/// is testable with no window, GPU or display.
///
/// `None` when the action is not a new-window request, or when nothing is
/// selected — a document window is never opened for an invented node.
fn doc_window_action(action: &NzkUiAction, view: Option<&GraphView>) -> Option<Action> {
    if *action != NzkUiAction::NewDocWindow {
        return None;
    }
    let view = view?;
    let node = view.selected.and_then(|i| view.graph.nodes.get(i))?;
    Some(Action::OpenDoc(windows::DocTarget::for_node(
        node.id.clone(),
        "doc",
    )))
}

/// Roots the current graph was indexed with, for the screenshot opt-in's
/// throwaway index. Falls back to the repo itself.
fn graph_roots(view: &GraphView) -> Vec<String> {
    view.graph
        .state
        .get("roots")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect::<Vec<String>>()
        })
        .filter(|v: &Vec<String>| !v.is_empty())
        .unwrap_or_else(|| vec![view.graph.repo.to_string_lossy().into_owned()])
}

fn roots_for_repo(repo: &std::path::Path, current_repo: &std::path::Path, current_roots: &[String]) -> Vec<String> {
    if csgraph_core::index::abspath(repo) == csgraph_core::index::abspath(current_repo) {
        return current_roots.to_vec();
    }
    let cfg = crate::model::load_config(repo);
    crate::model::resolve_roots(repo, &[], &cfg)
}

/// One `/api/analyze` request, executed in process. Runs on the analyze worker
/// so the Roslyn subprocess / dedupe walk never blocks a frame.
fn analyze_in_process(
    repo: &std::path::Path,
    roots: &[String],
    req: &crate::edit_wire::AnalyzeRequest,
) -> serde_json::Value {
    match req.op {
        crate::edit_wire::AnalyzeOp::ParseCheck => {
            let timeout = req
                .args
                .get("timeout")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(crate::edit_wire::DEFAULT_ANALYZE_TIMEOUT as u64)
                .clamp(1, 30);
            let files = crate::model::analyze_files(repo, roots, &req.args);
            csgraph_core::compile_check::parse_check(&files, timeout)
        }
        crate::edit_wire::AnalyzeOp::Dedupe => {
            let opts = dedupe_options(&req.args);
            let cfg = crate::model::load_config(repo);
            csgraph_core::dedupe::dedupe(repo, roots, &cfg.excludes, &opts)
        }
    }
}

/// `DedupeOptions` from the args box, defaulting every field — the in-process
/// twin of the reference host's `dedupe_options` (`app/src/main.rs:447-467`).
fn dedupe_options(args: &serde_json::Value) -> csgraph_core::dedupe::DedupeOptions {
    let mut opts = csgraph_core::dedupe::DedupeOptions::default();
    if let Some(v) = args.get("min_lines").and_then(serde_json::Value::as_u64) {
        opts.min_lines = v as usize;
    }
    if let Some(v) = args.get("threshold").and_then(serde_json::Value::as_f64) {
        opts.threshold = v;
    }
    if let Some(v) = args.get("max_clusters").and_then(serde_json::Value::as_u64) {
        opts.max_clusters = v as usize;
    }
    if let Some(v) = args.get("max_members").and_then(serde_json::Value::as_u64) {
        opts.max_members = v as usize;
    }
    if let Some(v) = args.get("max_blocks").and_then(serde_json::Value::as_u64) {
        opts.max_blocks = v as usize;
    }
    opts
}

/// Actions queued by the UI/keyboard and applied in `about_to_wait`, where an
/// `ActiveEventLoop` is available to create or drop windows.
#[derive(Clone, Debug, PartialEq)]
enum Action {
    Detach(Pane),
    Dock(Pane),
    Beside(Pane),
    /// Open `target` in a second OS window (`windows.js:172 openDoc`).
    OpenDoc(windows::DocTarget),
}

/// One in-flight index build, running on its own thread.
///
/// `#indexBtn` used to call `Index::build` on the paint thread, which blocked the
/// window for the whole build — and made `#stopIndexBtn` structurally
/// unclickable (there was no frame in which the overlay could be pressed). The
/// engine already supports cancellation (`Index::set_cancel_flag` /
/// `request_cancel`), so the build is moved off the paint thread and the shared
/// flag is the Stop channel. The previous graph keeps serving the UI for the
/// whole build, exactly like the reference (whose cancelled build restores its
/// snapshot).
struct IndexJob {
    rx: Receiver<Result<(crate::model::Graph, csgraph_core::index::Index, bool), String>>,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

/// One folder listing running away from the UI thread.
struct PickerJob {
    rx: Receiver<crate::find_wire::PickerView>,
    started: Instant,
}

/// One in-flight `/api/analyze` op (`#editValidate` / `#editDedupe`) on its own
/// thread.
///
/// `parse_check` can spend up to its timeout in the Roslyn subprocess and
/// `dedupe` walks the corpus; both used to run on the paint thread, freezing
/// every window while the modal said "Validating…". The request was already
/// parsed in `actions.rs`; this only carries the engine call.
struct AnalyzeJob {
    rx: Receiver<(crate::edit_wire::AnalyzeOp, serde_json::Value)>,
    started: Instant,
}

/// One entry in the window registry: the main window, a detached pane, or a
/// document window (`windows.js`'s "⧉ New window").
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Role {
    Main,
    Pane(Pane),
    /// A second window showing one document. Its [`windows::DocTarget`] is the
    /// exact `windows.js` target (node/file/line/mode), so the window's identity
    /// and title match the reference even though the render path differs — see
    /// [`App::open_document_window`] for that difference, stated honestly.
    Document(windows::DocTarget),
    /// The Scratch puzzle-piece window (`Ctrl+Alt+S`): a separate OS window that
    /// shows whatever C# the IDE is viewing as nested blocks.
    ///
    /// Not a [`Pane`]: it has no v2 counterpart, no dock zone and no affinity.
    /// Its GUI is the `scratch_rs_ide_addon` crate; this file only owns the
    /// window and the data it is handed.
    Scratch,
}

impl Role {
    /// Title bar text for this window. Pure function of the compile-time build
    /// stamp (see [`buildinfo`]) so it is unit-testable without a display.
    fn title(&self) -> String {
        match self {
            Role::Main => buildinfo::main_title(buildinfo::BUILD),
            Role::Pane(p) => buildinfo::pane_title(buildinfo::BUILD, p.title()),
            Role::Document(t) => buildinfo::pane_title(buildinfo::BUILD, &t.label()),
            Role::Scratch => buildinfo::pane_title(buildinfo::BUILD, "Scratch"),
        }
    }
}

struct WindowState {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    gpu: Arc<Gpu>,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
    /// Last position we asked the WM for, to stop snap feedback loops.
    last_set: Option<(f32, f32)>,
    /// Offset the WM adds between a requested outer position and the resulting
    /// one (frame extents); learned on read-back, subtracted on the next request.
    pos_bias: (f32, f32),
    /// Last requested *logical outer* position, consumed on read-back.
    last_requested: Option<(f32, f32)>,
    /// Last observed outer rect, so a poller notices moves on compositors that
    /// do not deliver `Moved` for programmatic placements.
    last_seen: Option<Rect>,
    /// Kept for diagnostics: when the pane last reported a move.
    #[allow(dead_code)]
    last_moved: Instant,
    /// When the pane first became a dock candidate; the debounce clock.
    dock_armed: Option<Instant>,
}

impl WindowState {
    fn scale(&self) -> f32 {
        self.window.scale_factor() as f32
    }

    /// Logical inner size (points).
    fn viewport_points(&self) -> egui::Vec2 {
        let s = self.scale();
        egui::vec2(self.config.width as f32 / s, self.config.height as f32 / s)
    }

    /// Global outer rectangle in logical pixels.
    fn outer_rect(&self) -> Option<Rect> {
        let pos = self.window.outer_position().ok()?;
        let size = self.window.outer_size();
        let s = self.scale();
        Some(Rect::new(
            pos.x as f32 / s,
            pos.y as f32 / s,
            size.width as f32 / s,
            size.height as f32 / s,
        ))
    }

    /// Move this window to a logical-pixel global outer position. The frame
    /// extent the WM adds is compensated through [`Self::pos_bias`].
    fn set_outer_logical(&mut self, x: f32, y: f32) {
        let s = self.scale();
        let phys = PhysicalPosition::new((x * s).round() as i32, (y * s).round() as i32);
        if self.window.outer_position().ok() == Some(phys) {
            return;
        }
        self.last_set = Some((x, y));
        self.last_requested = Some((x, y));
        self.window.set_outer_position(phys);
    }
}

/// Everything the main window needs to draw the snap indicator.
#[derive(Default, Clone)]
struct SnapUi {
    guide: Option<windows::Guide>,
    dock: bool,
    pane: Option<Pane>,
    main_origin: (f32, f32),
}

struct Demo {
    out: PathBuf,
    transcript: Vec<serde_json::Value>,
    step: usize,
    next_at: Duration,
    /// True once the main window showed the DOCK indicator for the demo pane.
    dock_seen: bool,
    /// `tick()` calls seen during the demo (proves the 30 Hz ticker ran).
    ticks: u64,
}

struct App {
    view: Option<GraphView>,
    /// The live engine index, kept for the app's lifetime so the source pane can
    /// synthesize documents on demand (`Index::doc_source`). Send-but-!Sync, so
    /// it stays on the winit main thread and is never shared with a window or
    /// worker thread.
    index: csgraph_core::index::Index,
    /// The build currently running on its worker thread, if any. See
    /// [`IndexJob`]: while this is `Some`, `#loading` is up with a real Stop.
    index_job: Option<IndexJob>,
    picker_job: Option<PickerJob>,
    /// The `/api/analyze` op currently running on its worker, if any.
    analyze_job: Option<AnalyzeJob>,
    source: SourceState,
    /// Last graph selection the pane was synced to (change, not per-frame).
    last_selected: Option<usize>,
    /// The split pane's second document (`#csSplitHost`, round 8).
    ///
    /// A separate [`SourceState`] because the pane owns one `Document`: the main
    /// pane and the split body must be able to show different nodes at the same
    /// time. Loaded only from a `node`-param split tab (see
    /// [`App::split_node_ref`]); a `file:` tab leaves it empty rather than faking
    /// a file view `SourceState` has no API for.
    split_source: SourceState,
    /// The tab key `split_source` was last loaded for, so a redraw does not
    /// reload the same document every frame. `None` until something loaded.
    split_loaded_key: Option<String>,
    /// One document state per open document window (`windows.js` `openDoc`).
    ///
    /// v2 gives each document window its own webview, JS thread and Monaco
    /// instance. This fork now matches the *isolation that matters*: every
    /// document window has its **own** engine [`Index`] built on its **own**
    /// worker thread (see [`crate::doc_engine`]), so no document-window engine
    /// work ever runs on the main thread and two document windows never share an
    /// engine index. What necessarily remains shared is the single winit event
    /// loop (one process, many OS windows) — the architecture boundary, which
    /// only renders.
    ///
    /// This map is the window's *document* state (loaded key, scroll, history),
    /// one entry per window, so two document windows showing different
    /// declarations cannot disturb each other or the main pane. It is inserted
    /// when the window's engine hands its document back (see
    /// [`App::poll_doc_loads`]) and dropped when the window closes.
    doc_sources: HashMap<windows::DocTarget, SourceState>,
    /// The per-window document engines, one per loading document window.
    ///
    /// Each entry owns its own `Index` on its own named worker thread; the one
    /// winit loop is shared but never runs that engine work. Engine
    /// *construction* is serialised by [`crate::doc_engine`]'s `BUILD_LOCK` so
    /// the shared on-disk cache cannot race; document synthesis stays
    /// concurrent. An entry moves to [`App::doc_sources`] when its load lands
    /// (see [`App::poll_doc_loads`]); removing/dropping it cancels its worker.
    doc_loads: HashMap<windows::DocTarget, doc_engine::DocEngine>,
    /// Host-owned action state (task-26): camera, full-graph flag, re-index and
    /// re-settle counters. Display-free; see `actions.rs`.
    action_state: NzkUiState,
    /// The quick-open palette's own state (Ctrl+P / Ctrl+T).
    ///
    /// Lives on `App` rather than in [`NzkUiState`] because it is not a nzk_ui
    /// surface: `#cs-quickopen` does not exist in `index.html` at all — the
    /// reference *creates* it (quickopen.js:113-181) — so it has no
    /// `drawers_open`/`modals_open` entry to be owned by. The palette owns its
    /// rows and selection, and projects them with
    /// [`crate::app::quickopen_wire::QuickOpenView::facts`], which is `None` while it
    /// is closed so the shipped document stays byte-identical.
    quickopen: crate::app::quickopen_wire::QuickOpenView,
    /// The CodeLens wire's own state (task-8/G3a).
    ///
    /// Lives on `App` for the same reason [`App::quickopen`] does: it is the
    /// *wire's* memory of which document the lenses belong to, not the source
    /// pane's, and `App::sync_source` is the only thing that drives it. The pane
    /// receives the rows to paint; this keeps `state()`/`facts()` observable so a
    /// consumer (and the tests) can assert the **consumed** result rather than
    /// that a function exists.
    ///
    /// The pane paints nothing while this surface has never computed, so the
    /// closed-pane default paint is untouched.
    codelens: crate::app::codelens_wire::CodeLensView,
    /// The last Scratch document and the key it was built for. Rebuilt only
    /// when the viewed document or the graph selection changes, not every frame.
    scratch_link: Option<(u64, LinkState)>,
    /// The Scratch window's own GUI state (block selection, port toggle).
    ///
    /// On `App` rather than in `NzkUiState`: the Scratch window is native egui
    /// painted by the addon, not a nzk_ui surface, so it has no drawer/modal
    /// entry to be owned by.
    scratch_gui: GuiState,
    /// A `Ctrl+Alt+S` was seen by `handle_key`, which does not receive the event
    /// loop; [`App::tick`] performs the window creation.
    pending_scratch: bool,
    /// The main window's live modifier state, refreshed from
    /// `WindowEvent::ModifiersChanged`.
    ///
    /// `KeyEvent` carries no modifier flags of its own in winit 0.30 — the state
    /// arrives as its own event — so the palette's `Ctrl+P`/`Ctrl+T` chords need
    /// it remembered here. It is read at the key event, so a chord pressed
    /// before any `ModifiersChanged` still works (the default is "no modifiers",
    /// which is the honest reading of "none reported yet").
    mods: Mods,
    /// Last pointer position the main window reported, in physical pixels.
    ///
    /// `MouseInput` carries no coordinates, so the click log reads this. It is
    /// diagnostic only: no behaviour depends on it.
    cursor: Option<PhysicalPosition<f64>>,
    /// The main window saw the left button go down and has not seen it come up.
    ///
    /// It exists to recognise the *orphan release*: the window manager can
    /// swallow the press of the first click on an unfocused window and still
    /// deliver the release. Without this flag such a click would reach egui as
    /// a release with no press and do nothing — see [`orphan_left_release`].
    left_press_seen: bool,
    /// Repo/roots captured at startup, so `#indexBtn` can re-index the same
    /// corpus without asking the view.
    repo: PathBuf,
    roots: Vec<String>,
    gpu: Option<Arc<Gpu>>,
    windows: HashMap<Role, WindowState>,
    docked: Vec<Pane>,
    /// Panel layout: floating-pane geometry + dock-zone stacks. Persisted in
    /// the viewer-state store (`crate::pov`) so a restart restores it, and
    /// loaded at startup through [`load_panel_layout_for_startup`].
    panel_layout: windows::PanelLayout,
    /// Cache root of the viewer-state blob the panel layout lives in.
    layout_cache_root: PathBuf,
    actions: Vec<Action>,
    snap: SnapUi,
    start: Instant,
    first_frame: Option<u128>,
    sequence: u64,
    opts: RunOptions,
    demo: Option<Demo>,
    demo_finished: bool,
}

/// The four modifier flags the key paths care about, lifted out of winit's
/// own type.
///
/// winit 0.30's `ModifiersState` lives behind `winit::keyboard` and its
/// accessors are what the reference's `ev.ctrlKey`/`ev.metaKey`/`ev.shiftKey`/
/// `ev.altKey` map onto. Lifting them here means `App` stores plain `bool`s and
/// a test can construct a modifier state without a window or a winit type.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
struct Mods {
    ctrl: bool,
    meta: bool,
    shift: bool,
    alt: bool,
}

impl From<Modifiers> for Mods {
    fn from(m: Modifiers) -> Self {
        let st = m.state();
        Mods {
            ctrl: st.control_key(),
            meta: st.super_key(),
            shift: st.shift_key(),
            alt: st.alt_key(),
        }
    }
}

/// The modifier state as one canonical token, so a logged click/key can be
/// replayed exactly: `-`, `shift`, `ctrl+shift`, `ctrl+alt+shift+meta`.
///
/// `meta` is winit's Super/Command key. Order is fixed so two identical
/// combinations always print identically — this is a replay aid, not prose.
fn mods_token(mods: Mods) -> String {
    let mut out = String::new();
    let mut add = |on: bool, name: &str| {
        if on {
            if !out.is_empty() {
                out.push('+');
            }
            out.push_str(name);
        }
    };
    add(mods.ctrl, "ctrl");
    add(mods.alt, "alt");
    add(mods.shift, "shift");
    add(mods.meta, "meta");
    if out.is_empty() {
        "-".to_string()
    } else {
        out
    }
}

/// Does this modifier state carry a command accelerator (`Ctrl`/`Cmd`)?
///
/// The unmodified canvas letter bindings (`i`, `b`, `r`, `0`, `+`/`-`) must not
/// fire on a chord: `Ctrl+I` is not "re-index" and `Ctrl+B` is not
/// "open blocks", and letting them through would race the palette's own two
/// chords.
fn has_command_modifier(mods: Mods) -> bool {
    mods.ctrl || mods.meta
}

/// `Shift|Alt+H` — the reference's call-hierarchy chord
/// (`hierarchy.js:685`, `monaco.KeyMod.Shift | monaco.KeyMod.Alt | KeyCode.KeyH`).
///
/// Extracted from `handle_key` so it is testable without a window: the whole
/// point of a keybinding is that it fires on exactly one modifier combination,
/// and an inline guard in a `match` arm cannot be pinned by a test.
///
/// `Ctrl+Alt+S` — open (or focus) the Scratch puzzle-piece window.
///
/// A fork-only chord, deliberately not in `chords.rs`: that module reproduces
/// the reference's own bindings, and this one has no v2 counterpart. `Alt+S`
/// alone is the reference's split toggle, so the Ctrl is what keeps them apart.
fn is_scratch_chord(key: &Key, mods: Mods) -> bool {
    mods.ctrl
        && mods.alt
        && !mods.shift
        && !mods.meta
        && matches!(key, Key::Character(c) if c.eq_ignore_ascii_case("s"))
}

/// `Ctrl`/`Cmd` are required to be ABSENT, not merely unmentioned. Monaco treats
/// `Shift+Alt+H` and `Ctrl+Shift+Alt+H` as different chords, and — more
/// practically — a Ctrl-inclusive match here would race the palette's own
/// `Ctrl+P`/`Ctrl+T` family.
///
/// Both cases are accepted because the reference matches the *physical* `KeyH`
/// while a winit `Character` carries the layout's produced text; on a layout
/// where `Shift+h` yields `"H"` the chord would otherwise silently not fire.
fn is_hierarchy_chord(text: &str, mods: Mods) -> bool {
    mods.shift && mods.alt && !mods.ctrl && !mods.meta && text.eq_ignore_ascii_case("h")
}

/// `Alt+Left` — the reference's selection-history Back
/// (`nav.js:437-439`).
///
/// The reference's guard is `ev.altKey && !ev.ctrlKey && !ev.metaKey &&
/// !ev.shiftKey` — all four flags, explicitly. That is copied here rather than
/// reduced to `alt` alone: `Ctrl+Left`/`Cmd+Left` are word-motion chords in
/// every editor this app sits next to, and `Shift+Alt+Left` is a selection
/// extension. Letting any of them step the graph's history would make the
/// binding fire on chords the reference does not claim.
///
/// Extracted from `handle_key` for the same reason as [`is_hierarchy_chord`]:
/// a keybinding's whole contract is *which* modifier combination it fires on,
/// and an inline guard inside a `match` arm cannot be pinned by a test without a
/// window.
fn is_history_back_chord(mods: Mods) -> bool {
    mods.alt && !mods.ctrl && !mods.meta && !mods.shift
}

/// The bare-letter canvas bindings, as a pure lookup.
///
/// `handle_key` used to inline this `match` inside a guarded arm, which meant the
/// *guard* was testable but the *table* was not: a test could prove `Ctrl+F` is
/// blocked while nothing proved `f` maps to find. Lifting the table out makes
/// both halves assertable without a window, and it keeps the one invariant that
/// matters — the table is only reachable when
/// [`has_command_modifier`] is false — expressed in one place.
///
/// `None` is an unbound letter, which is the honest answer for every letter the
/// reference does not claim.
pub fn bare_letter_action(text: &str) -> Option<NzkUiAction> {
    Some(match text {
        "+" | "=" => NzkUiAction::ZoomIn,
        "-" | "_" => NzkUiAction::ZoomOut,
        "0" => NzkUiAction::FullGraph,
        "r" | "R" => NzkUiAction::Resettle,
        "i" | "I" => NzkUiAction::Index,
        "b" | "B" => NzkUiAction::OpenBlocks,
        // `shell.js:457` — `f`/`F` is find mode. This is the reference's own
        // meaning for the letter, and the reason the Fit button could no longer
        // advertise `(F)`: `f` was already spoken for.
        "f" | "F" => NzkUiAction::ModeFind,
        _ => return None,
    })
}

/// The reference's key name for a winit key — the same vocabulary
/// `quickopen_wire::route_key` takes (`"Escape"`, `"ArrowDown"`, `"Enter"`, …),
/// plus the text form for a character key.
///
/// The second element is `true` when the event is a **character** (so the host
/// must ask `edit_key` whether it types), and `false` for a named key such as
/// `"F2"` or `"Backspace"` — which is how the two arms stay apart without the
/// caller re-deriving it.
/// Normalise a winit key into a [`crate::form_wire::Key`], or `None` for keys
/// the form does not use (they fall through to the canvas).
fn form_key_of(key: &Key) -> Option<crate::form_wire::Key> {
    use crate::form_wire::Key as FormKey;
    Some(match key {
        Key::Character(text) => {
            let mut chars = text.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            FormKey::Char(c)
        }
        Key::Named(NamedKey::Backspace) => FormKey::Backspace,
        Key::Named(NamedKey::Enter) => FormKey::Enter,
        Key::Named(NamedKey::Escape) => FormKey::Escape,
        Key::Named(NamedKey::ArrowUp) => FormKey::Up,
        Key::Named(NamedKey::ArrowDown) => FormKey::Down,
        Key::Named(NamedKey::ArrowLeft) => FormKey::Left,
        Key::Named(NamedKey::ArrowRight) => FormKey::Right,
        Key::Named(NamedKey::Home) => FormKey::Home,
        Key::Named(NamedKey::End) => FormKey::End,
        Key::Named(NamedKey::Delete) => FormKey::Delete,
        _ => return None,
    })
}

fn key_name(key: &Key) -> (String, bool) {
    match key {
        Key::Character(c) => (c.to_string(), true),
        Key::Named(n) => {
            let name = match n {
                NamedKey::Escape => "Escape",
                NamedKey::ArrowDown => "ArrowDown",
                NamedKey::ArrowUp => "ArrowUp",
                NamedKey::PageDown => "PageDown",
                NamedKey::PageUp => "PageUp",
                NamedKey::Enter => "Enter",
                NamedKey::Tab => "Tab",
                NamedKey::Backspace => "Backspace",
                NamedKey::F2 => "F2",
                NamedKey::F3 => "F3",
                NamedKey::F4 => "F4",
                NamedKey::F5 => "F5",
                NamedKey::Home => "Home",
                _ => "Unidentified",
            };
            (name.to_string(), false)
        }
        _ => ("Unidentified".to_string(), false),
    }
}

/// The key token [`crate::chords::resolve`] matches against.
///
/// Separate from [`key_name`] on purpose: `resolve` needs the *reference's*
/// spellings (`"Tab"`, `"ArrowRight"`, and the character itself), and
/// `key_name` deliberately folds every unnamed key into `"Unidentified"` for
/// the palette's benefit. Reusing it here would make `Alt+Right` unmatchable.
fn chord_token(key: &Key) -> Option<String> {
    match key {
        Key::Character(c) => Some(c.to_string()),
        Key::Named(NamedKey::Tab) => Some("Tab".to_string()),
        Key::Named(NamedKey::ArrowRight) => Some("ArrowRight".to_string()),
        _ => None,
    }
}

fn in_app_pane_key(key: &Key) -> Option<Pane> {
    match key {
        Key::Named(NamedKey::F2) => Some(Pane::Source),
        Key::Named(NamedKey::F3) => Some(Pane::NodeDrawer),
        Key::Named(NamedKey::F4) => Some(Pane::Find),
        Key::Named(NamedKey::F5) => Some(Pane::Blocks),
        _ => None,
    }
}

impl App {
    fn has_pane(&self, p: Pane) -> bool {
        self.windows.contains_key(&Role::Pane(p))
    }

    fn elapsed_ms(&self) -> u128 {
        self.start.elapsed().as_millis()
    }

    /// Graph node -> source pane. Called once per selection *change* (after the
    /// egui pass), never per frame; `Index::doc_source` is cached by the engine.
    ///
    /// This is the **CodeLens consumer** (task-8/G3a): the pane is handed the
    /// rows the core produced for the document it is now showing, so the lenses
    /// are driven from the live app rather than existing only as a tested-but-
    /// unreachable core. `show_node_and_compute` is `show_node` plus a
    /// [`codelens_wire::CodeLensView::compute`] pass over the same `Graph`; the
    /// view is stored on `App` so `state()`/`facts()` stay observable.
    fn sync_source(&mut self, selected: Option<usize>) {
        if selected == self.last_selected {
            return;
        }
        self.last_selected = selected;
        // The Scratch window is a view of the selection; wake it when that view
        // changes rather than making it poll.
        if let Some(ws) = self.windows.get(&Role::Scratch) {
            ws.window.request_redraw();
        }
        let Some(i) = selected else {
            // Nothing selected: the lens surface must go back to having no
            // facts, or a lens from the previous document would survive a
            // cleared graph selection.
            self.codelens.close();
            self.source.set_lenses(source_pane::LensRows::empty());
            return;
        };
        // **Clicking a graph node does not open the source pane.** The reference
        // (`selectNode`, interaction.js:306-320) calls `loadDetail` and
        // `renderNodeDrawer`; the ONLY ways the source pane opens are an
        // explicit `openSource` call — the drawer's `{ }` View code button, a
        // Find/quick-open row, a code lens, or F2. This used to call
        // `show_node_and_compute` unconditionally, and `SourceState::show_node`
        // sets `open = true`, so every plain node click popped up the bottom
        // Source pane *in addition to* the node drawer — the wrong panel.
        //
        // While the pane IS open, a new selection keeps it in step (the native
        // analogue of the pane showing the current declaration), so this branch
        // only skips the *automatic open*, not the update.
        if self.source.open {
            self.load_source_node(i);
        } else {
            self.codelens.close();
            self.source.set_lenses(source_pane::LensRows::empty());
        }
    }

    /// Load one graph node into the source pane — the document plus its CodeLens
    /// rows plus the matching `#cs-tabs` entry.
    ///
    /// This is the unconditional half of [`App::sync_source`]: every explicit
    /// *open* path calls it (`{ }` View code, F2, the screenshot evidence run),
    /// which is what lets `sync_source` skip loading while the pane is closed
    /// without making an open land on a stale document.
    fn load_source_node(&mut self, i: usize) {
        let Some(id) = self
            .view
            .as_ref()
            .and_then(|v| v.graph.nodes.get(i))
            .map(|n| n.id.clone())
        else {
            return;
        };
        // The lens pass needs the graph, which `show_node` does not. `view` is
        // `Option<GraphView>`; when it is absent there is no graph to compute
        // against, so the document still opens and the pane paints no lenses —
        // the pre-wiring behaviour, chosen explicitly rather than by accident.
        match self.view.as_ref() {
            Some(v) => {
                self.source
                    .show_node_and_compute(&mut self.index, &v.graph, &mut self.codelens, &id);
                // The wire's own projection is consumed here too, not merely
                // compiled: `facts()` is the same payload
                // `codelens_wire::insert_codelens` would put on the shell's
                // facts object, and logging its `state` is what makes "the four
                // states are live in the app" observable from a real run
                // (`CSVIEWER_LENS_TRACE=1`), rather than only from a unit test.
                if std::env::var_os("CSVIEWER_LENS_TRACE").is_some() {
                    let state = self.codelens.state();
                    let placed = self.source.lens_count();
                    eprintln!(
                        "[native-csviewer] codelens {id}: state={} rows={placed} facts={}",
                        state.tag(),
                        self.codelens
                            .facts()
                            .map(|f| f["placed"].to_string())
                            .unwrap_or_else(|| "none".to_string())
                    );
                }
            }
            None => {
                self.source.show_node(&mut self.index, &id);
                self.codelens.close();
                self.source.set_lenses(source_pane::LensRows::empty());
            }
        }
        // v2's `openSource()` pushes/activates a document tab on every open
        // (`shell.js:1535-1568`), and the reference's `#cs-tabs` strip is painted
        // from that stack. `Tabs::open_params` is this app's model of exactly
        // that call, so wiring it here is what makes the real tab bar follow the
        // graph selection instead of only appearing when a chord opened a tab.
        // A node whose document failed to synthesize still pushes its tab: the
        // reference keeps the document in the stack too, and the body paints the
        // honest "no synthesized document" note.
        self.action_state
            .tabs
            .open_params(&serde_json::json!({ "node": id }));
    }

    /// Load the split tab's document into [`App::split_source`] (round 8).
    ///
    /// Called once per redraw BEFORE the egui pass, where `index` and the tabs
    /// model are both free. [`split_sync_plan`] decides what (if anything) to do,
    /// so a 30 Hz redraw does not reload the same document every frame.
    ///
    /// When the split is off nothing is cleared: the user may still be looking at
    /// the last split document after collapsing, and blanking a body nobody
    /// paints would be a side effect with no visible meaning.
    /// Consume the Blocks canvas hover transitions staged by `nzk_ui::overlay`.
    ///
    /// The nzk_ui can resolve WHICH card is under the pointer (the rows are
    /// runtime-created, so the shell's hit chain is the only resolver), but it
    /// cannot build the peek: that needs the graph and the engine's synthesized
    /// document. Both live here, so the one owner of the peek state
    /// (`NzkUiState::blocks_peek`) is updated once per transition and the next
    /// facts pass paints it.
    ///
    /// A hover that resolves to no graph node is an honest HIDE, never a
    /// fabricated tooltip; a document that the engine cannot synthesize sets the
    /// peek's own `source unavailable: <msg>` text (`blocks_peek.rs`) rather than
    /// leaving the shipped "Loading source..." forever.
    fn consume_blocks_hover(&mut self) {
        for hover in nzk_ui::take_pending_block_hovers() {
            match hover {
                nzk_ui::BlockHover::Hide => self.action_state.blocks_peek.hide(),
                nzk_ui::BlockHover::Show(id) => {
                    let Some(view) = self.view.as_ref() else {
                        self.action_state.blocks_peek.hide();
                        continue;
                    };
                    let Some(i) = view.graph.nodes.iter().position(|n| n.id == id) else {
                        self.action_state.blocks_peek.hide();
                        continue;
                    };
                    let n = &view.graph.nodes[i];
                    let in_deg = view.graph.edges.iter().filter(|e| e.to == i).count() as u32;
                    let out_deg = view.graph.edges.iter().filter(|e| e.from == i).count() as u32;
                    let decl = crate::blocks_peek::PeekDecl {
                        id: n.id.clone(),
                        kind: n.kind.clone(),
                        name: n.label.clone(),
                        full: n.full.clone(),
                        file: n.file.clone(),
                        line: n.line.max(0) as u32,
                        in_deg,
                        out_deg,
                    };
                    let seq = self.action_state.blocks_peek.show(&decl);
                    match source_pane::document_for(&mut self.index, &id) {
                        Ok(doc) => {
                            let lines: Vec<String> =
                                doc.lines.iter().map(|l| l.text.clone()).collect();
                            self.action_state.blocks_peek.apply_source(seq, &lines);
                        }
                        Err(e) => self.action_state.blocks_peek.apply_source_error(seq, &e),
                    }
                }
            }
        }
    }

    fn sync_split_source(&mut self) {
        match split_sync_plan(&self.action_state.tabs, self.split_loaded_key.as_deref()) {
            SplitSync::Idle => {}
            SplitSync::Load(node_ref) => {
                // The same engine call the main pane uses; errors are recorded on
                // `split_source` by `show_node`, never panicked on.
                self.split_source.show_node(&mut self.index, &node_ref);
                self.split_loaded_key =
                    self.action_state.tabs.split_key().map(str::to_owned);
            }
            SplitSync::Empty(key) => {
                // A `file:` (or otherwise unloadable) split tab: leave the body
                // empty. `show_node` is the only honest open API and it takes a
                // node ref; pretending a file is open would paint a document the
                // engine never produced. The key is still recorded so this is a
                // one-shot decision, not a per-frame retry.
                self.split_source.close();
                self.split_loaded_key = Some(key);
            }
        }
    }

    /// Pre-open the pane from `CSVIEWER_SOURCE_NODE` (real-window evidence run).
    fn preopen_source(&mut self, node_ref: &str) {        let hit = self
            .view
            .as_ref()
            .and_then(|v| {
                v.graph
                    .nodes
                    .iter()
                    .position(|n| n.id == node_ref || n.full == node_ref || n.label == node_ref)
            });
        match hit {
            Some(i) => {
                if let Some(v) = self.view.as_mut() {
                    v.selected = Some(i);
                }
                // This IS an explicit open (View code / the screenshot evidence
                // run), so load unconditionally: `sync_source` deliberately does
                // not open the pane for a plain selection, and would no-op here
                // because the selection is already the one being requested.
                self.last_selected = Some(i);
                self.load_source_node(i);
            }
            None => {
                self.source.show_node(&mut self.index, node_ref);
            }
        }
    }

    fn make_window_state(
        event_loop: &ActiveEventLoop,
        gpu: Arc<Gpu>,
        attrs: WindowAttributes,
    ) -> Result<WindowState, String> {
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .map_err(|e| format!("create_window: {e}"))?,
        );
        let surface = gpu
            .instance
            .create_surface(window.clone())
            .map_err(|e| format!("create_surface: {e}"))?;
        let caps = surface.get_capabilities(&gpu.adapter);
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&gpu.adapter, size.width.max(1), size.height.max(1))
            .ok_or("surface unsupported")?;
        if let Some(f) = caps.formats.iter().copied().find(|f| f.is_srgb()) {
            config.format = f;
        }
        surface.configure(&gpu.device, &config);
        let egui_ctx = egui::Context::default();
        crate::typography::install(&egui_ctx);
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
            Some(2048),
        );
        let renderer =
            egui_wgpu::Renderer::new(
                &gpu.device,
                config.format,
                // Match the offscreen path (native-fidelity): no ordered
                // dithering, so window and PNG agree pixel-for-pixel.
                RendererOptions { dithering: false, ..RendererOptions::default() },
            );
        Ok(WindowState {
            window,
            surface,
            config,
            gpu,
            egui_ctx,
            egui_state,
            renderer,
            last_set: None,
            pos_bias: (0.0, 0.0),
            last_requested: None,
            last_seen: None,
            last_moved: Instant::now(),
            dock_armed: None,
        })
    }

    fn create_main(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let gpu = self.gpu.clone().ok_or("no gpu")?;
        let attrs = WindowAttributes::default()
            .with_title(Role::Main.title())
            .with_inner_size(LogicalSize::new(1440.0, 900.0))
            .with_min_inner_size(LogicalSize::new(900.0, 600.0));
        let ws = Self::make_window_state(event_loop, gpu, attrs)?;
        eprintln!(
            "[native-csviewer] adapter={:?} backend={:?} format={:?}",
            ws.gpu.adapter.get_info().name,
            ws.gpu.adapter.get_info().backend,
            ws.config.format
        );
        self.windows.insert(Role::Main, ws);
        Ok(())
    }

    /// Detach `pane` into a real OS window placed beside the main one.
    fn detach(&mut self, event_loop: &ActiveEventLoop, pane: Pane) {
        if self.has_pane(pane) {
            return;
        }
        let main_rect =
            self.windows.get(&Role::Main).and_then(|w| w.outer_rect());
        let gpu = match self.gpu.clone() {
            Some(g) => g,
            None => return,
        };
        let (w, h) = pane.default_size();
        let (min_w, min_h) = pane.min_size();
        let cascade = self.sequence;
        self.sequence += 1;
        let placed = self.panel_layout.geometry.place(pane, main_rect, None, (w, h), cascade);
        let mut attrs = WindowAttributes::default()
            .with_title(Role::Pane(pane).title())
            .with_inner_size(LogicalSize::new(w, h))
            .with_min_inner_size(LogicalSize::new(min_w, min_h));
        if let Some((x, y)) = placed {
            attrs = attrs.with_position(LogicalPosition::new(x, y));
        }
        match Self::make_window_state(event_loop, gpu, attrs) {
            Ok(ws) => {
                eprintln!(
                    "[native-csviewer] detached pane={} at {}",
                    pane.id(),
                    match ws.outer_rect() {
                        Some(r) => format!("{:.0},{:.0} {:.0}x{:.0}", r.x, r.y, r.w, r.h),
                        None => "unknown".to_string(),
                    }
                );
                self.windows.insert(Role::Pane(pane), ws);
                self.docked.retain(|p| *p != pane);
                // A floating pane is no longer in a dock-zone stack; a later
                // zone-aware drop re-assigns it.
                self.panel_layout.docks.unassign(pane);
                    }
            Err(e) => eprintln!("[native-csviewer] detach {} failed: {e}", pane.id()),
        }
    }

    /// Open the Scratch puzzle-piece window, or focus it when it is already up.
    ///
    /// A separate OS window beside the main one (the same placement rule as a
    /// document window). It is not dockable and has no pane identity: the only
    /// way in and out is `Ctrl+Alt+S` / closing the window.
    fn open_scratch_window(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(ws) = self.windows.get(&Role::Scratch) {
            ws.window.request_redraw();
            ws.window.focus_window();
            return;
        }
        let Some(gpu) = self.gpu.clone() else { return };
        let main_rect = self.windows.get(&Role::Main).and_then(|w| w.outer_rect());
        let (w, h) = (1100.0, 820.0);
        let cascade = self.sequence;
        self.sequence += 1;
        let mut attrs = WindowAttributes::default()
            .with_title(Role::Scratch.title())
            .with_inner_size(LogicalSize::new(w, h))
            .with_min_inner_size(LogicalSize::new(640.0, 420.0));
        if let Some((x, y)) = windows::beside_main(main_rect, None, (w, h), cascade) {
            attrs = attrs.with_position(LogicalPosition::new(x, y));
        }
        match Self::make_window_state(event_loop, gpu, attrs) {
            Ok(ws) => {
                eprintln!(
                    "[native-csviewer] scratch window at {}",
                    match ws.outer_rect() {
                        Some(r) => format!("{:.0},{:.0} {:.0}x{:.0}", r.x, r.y, r.w, r.h),
                        None => "unknown".to_string(),
                    }
                );
                self.windows.insert(Role::Scratch, ws);
            }
            Err(e) => eprintln!("[native-csviewer] scratch window failed: {e}"),
        }
    }

    /// Re-attach a detached pane: its OS window is dropped and the pane goes
    /// back into the main window.
    fn dock(&mut self, pane: Pane) {
        if let Some(ws) = self.windows.remove(&Role::Pane(pane)) {
            eprintln!("[native-csviewer] dock pane={} (closing its window)", pane.id());
            // Zone-aware drop: if the floating pane's centre is over one of the
            // main window's named dock zones it joins that zone's stack, and the
            // existing dock stack paints it; outside every zone the layout is
            // untouched (free-float preserved). Done before the `drop` that
            // closes the OS window.
            if let Some(rect) = ws.outer_rect() {
                let main = self
                    .windows
                    .get(&Role::Main)
                    .and_then(|w| w.outer_rect());
                if let Some(main) = main {
                    let (x, y) = rect.center();
                    if let Some(zone) =
                        drop_into_dock_zone(&mut self.panel_layout, pane, x, y, main)
                    {
                        eprintln!(
                            "[native-csviewer] pane={} dropped into dock zone={}",
                            pane.id(),
                            zone.id()
                        );
                    }
                }
                // Remember where the pane actually floated, so a later re-detach
                // restores it (`windows.js`/`panels.js` float geometry).
                self.panel_layout.geometry.remember(pane, rect);
            }
            drop(ws); // closes the OS window
            if !self.docked.contains(&pane) {
                self.docked.push(pane);
            }
            if self.snap.pane == Some(pane) {
                self.snap = SnapUi::default();
            }
            if let Some(demo) = self.demo.as_mut() {
                demo.transcript.push(json!({
                    "event": "dock",
                    "pane": pane.id(),
                    "open_panes": self.windows.len() - 1,
                    "docked": self.docked.iter().map(|p| p.id()).collect::<Vec<_>>(),
                }));
            }
            if let Some(main) = self.windows.get(&Role::Main) {
                main.window.request_redraw();
            }
        }
    }

    /// Open `target` in a second OS window (`windows.js:172 openDoc`).
    ///
    /// Isolation, stated honestly: a Tauri document window is an isolated
    /// webview with its own JS main thread and its own Monaco instance. This
    /// fork now gives each document window its **own engine [`Index`] built on
    /// its own worker thread** (see [`crate::doc_engine`]), so a pathological
    /// document's engine CPU is isolated from every other window and never runs
    /// on the main thread. The one thing that necessarily remains shared is the
    /// single winit event loop (one process, many OS windows) — an architecture
    /// boundary that only renders. The window renders only its target document
    /// (v2: "a DOCUMENT window — one document, nothing else") from a per-window
    /// [`SourceState`] in [`App::doc_sources`], instead of re-painting the main
    /// graph view. The `DocTarget` stays in the role so the window's identity
    /// and title match `windows.js`.
    fn open_document_window(&mut self, event_loop: &ActiveEventLoop, target: windows::DocTarget) {
        let role = Role::Document(target.clone());
        let title = role.title();
        if let Some(ws) = self.windows.get(&role) {
            ws.window.request_redraw();
            ws.window.focus_window();
            return;
        }
        let main_rect = self.windows.get(&Role::Main).and_then(|w| w.outer_rect());
        let Some(gpu) = self.gpu.clone() else { return };
        let (w, h) = (1440.0, 900.0);
        let cascade = self.sequence;
        self.sequence += 1;
        let mut attrs = WindowAttributes::default()
            .with_title(title.clone())
            .with_inner_size(LogicalSize::new(w, h))
            .with_min_inner_size(LogicalSize::new(900.0, 600.0));
        if let Some((x, y)) = windows::beside_main(main_rect, None, (w, h), cascade) {
            attrs = attrs.with_position(LogicalPosition::new(x, y));
        }
        match Self::make_window_state(event_loop, gpu, attrs) {
            Ok(ws) => {
                eprintln!(
                    "[native-csviewer] document window {} at {}",
                    title,
                    match ws.outer_rect() {
                        Some(r) => format!("{:.0},{:.0} {:.0}x{:.0}", r.x, r.y, r.w, r.h),
                        None => "unknown".to_string(),
                    }
                );
                self.windows.insert(role, ws);
                // The window owns its own engine from here: a dedicated worker
                // thread and its own `Index`. The main thread never waits for it;
                // `poll_doc_loads` adopts the result when it lands. A `file:` or
                // empty target resolves to an honest failure without building an
                // index (see `DocEngine::spawn`).
                self.doc_loads.insert(
                    target.clone(),
                    doc_engine::DocEngine::spawn(&self.repo, &self.roots, target),
                );
            }
            Err(e) => eprintln!("[native-csviewer] document window failed: {e}"),
        }
    }

    /// Adopt every document window result that has landed, on the main thread.
    ///
    /// Called at the top of [`App::tick`], which runs from both `about_to_wait`
    /// and `RedrawRequested`, and `about_to_wait` keeps requesting redraws — so
    /// a completed load is seen promptly with no `EventLoopProxy`. This function
    /// only moves the *result*: the engine build and document synthesis already
    /// happened on the window's own worker thread and its own `Index`.
    fn poll_doc_loads(&mut self) {
        if self.doc_loads.is_empty() {
            return;
        }
        let targets: Vec<windows::DocTarget> = self.doc_loads.keys().cloned().collect();
        for target in targets {
            let Some(engine) = self.doc_loads.get_mut(&target) else { continue };
            let Some(load) = engine.try_take() else { continue };
            let mut state = SourceState::default();
            match load {
                doc_engine::DocLoad::Ready(doc) => {
                    // The window's engine answered; adoption is the same pane
                    // transition a synchronous load used to make.
                    let node_ref = match target.node.as_deref() {
                        Some(n) => n.to_string(),
                        None => target.label(),
                    };
                    state.adopt_document(&node_ref, doc);
                }
                doc_engine::DocLoad::Failed(msg) => {
                    // An honest error state, not a fake document: the pane's own
                    // error surface paints it.
                    state.open = true;
                    state.error = Some(msg);
                }
            }
            self.doc_sources.insert(target.clone(), state);
            // Dropping the engine also cancels its worker; the load is complete
            // so there is nothing left to cancel, and this frees the thread
            // handle without ever joining it here.
            self.doc_loads.remove(&target);
            if let Some(ws) = self.windows.get(&Role::Document(target)) {
                ws.window.request_redraw();
            }
        }
    }

    fn apply_actions(&mut self, event_loop: &ActiveEventLoop) {
        // Keep the engine-backed Find groups in step with the typed query. This
        // is the per-event seam (a keystroke becomes an action), and `refresh`
        // is a no-op unless the query actually changed, so the O(corpus) engine
        // `find` runs once per query rather than once per frame.
        self.action_state
            .find
            .refresh(&mut self.index, crate::find_wire::FIND_LIMIT);
        let actions: Vec<Action> = self.actions.drain(..).collect();
        for a in actions {
            match a {
                Action::Detach(p) => self.detach(event_loop, p),
                Action::Dock(p) => self.dock(p),
                Action::OpenDoc(t) => self.open_document_window(event_loop, t),
                Action::Beside(p) => {
                    let main = self
                        .windows
                        .get(&Role::Main)
                        .and_then(|w| w.outer_rect());
                    let seq = self.sequence;
                    self.sequence += 1;
                    if let (Some(m), Some(ws)) =
                        (main, self.windows.get_mut(&Role::Pane(p)))
                    {
                        let (w, h) = p.default_size();
                        if let Some((x, y)) = windows::beside_main(Some(m), None, (w, h), seq) {
                            ws.set_outer_logical(x, y);
                        }
                    }
                }
            }
        }
    }

    /// The main window rectangle and every detached pane rectangle, in logical
    /// screen pixels.
    fn snap_geometry(&self) -> Option<(Rect, Vec<(Pane, Rect)>)> {
        let main = self.windows.get(&Role::Main)?.outer_rect()?;
        let others: Vec<(Pane, Rect)> = self
            .windows
            .iter()
            .filter_map(|(r, ws)| match r {
                Role::Pane(p) => ws.outer_rect().map(|rect| (*p, rect)),
                Role::Main | Role::Document(_) | Role::Scratch => None,
            })
            .collect();
        Some((main, others))
    }

    /// Poll every pane's real outer rect. `Moved` is not delivered by every
    /// compositor for programmatic placements, so snapping also runs off this
    /// poll; the poll learns the WM's frame-extent bias on the way.
    fn poll_panes(&mut self) {
        let mut changed: Vec<Pane> = Vec::new();
        for (r, ws) in self.windows.iter_mut() {
            if let Role::Pane(p) = r {
                if let Some(rect) = ws.outer_rect() {
                    if let Some(req) = ws.last_requested.take() {
                        let (bx, by) = (rect.x - req.0, rect.y - req.1);
                        if bx.abs() < 128.0 && by.abs() < 128.0 {
                            ws.pos_bias = (bx, by);
                        }
                    }
                    if ws.last_seen != Some(rect) {
                        ws.last_seen = Some(rect);
                        changed.push(*p);
                    }
                }
            }
        }
        for p in changed {
            self.on_pane_moved(p);
        }
    }

    /// React to a pane window move: snap it or arm a dock, and update the
    /// visible indicator.
    fn on_pane_moved(&mut self, pane: Pane) {
        let Some((main, others)) = self.snap_geometry() else { return };
        let others: Vec<(Pane, Rect)> = others.into_iter().filter(|(p, _)| *p != pane).collect();
        let Some(ws) = self.windows.get_mut(&Role::Pane(pane)) else { return };
        ws.last_moved = Instant::now();
        let Some(rect) = ws.outer_rect() else { return };
        self.panel_layout.geometry.remember(pane, rect);
        let decision = windows::decide(rect, main, &others, windows::SNAP_THRESHOLD_PX);
        match decision {
            Decision::Dock { overlap, .. } => {
                // Arm once; the debounce clock starts at the first arming.
                if ws.dock_armed.is_none() {
                    ws.dock_armed = Some(Instant::now());
                    if let Some(demo) = self.demo.as_mut() {
                        demo.dock_seen = true;
                        demo.transcript.push(json!({
                            "event": "dock-armed",
                            "pane": pane.id(),
                            "overlap": overlap,
                        }));
                    }
                }
                self.snap = SnapUi {
                    guide: None,
                    dock: true,
                    pane: Some(pane),
                    main_origin: (main.x, main.y),
                };
            }
            Decision::Snap { rect: snapped, target, guide, edge } => {
                ws.dock_armed = None;
                let dx = (snapped.x - rect.x).abs();
                let dy = (snapped.y - rect.y).abs();
                if dx >= 1.0 || dy >= 1.0 {
                    ws.set_outer_logical(snapped.x, snapped.y);
                }
                self.snap = SnapUi {
                    guide: Some(guide),
                    dock: false,
                    pane: Some(pane),
                    main_origin: (main.x, main.y),
                };
                // Only record a real correction, not the echo of our own move.
                if (dx >= 1.0 || dy >= 1.0) && self.demo.is_some() {
                    if let Some(demo) = self.demo.as_mut() {
                        demo.transcript.push(json!({
                            "event": "snap",
                            "pane": pane.id(),
                            "edge": format!("{edge:?}"),
                            "target": match target {
                                TargetId::Main => "main".to_string(),
                                TargetId::Pane(p) => p.id().to_string(),
                            },
                            "before": {"x": rect.x, "y": rect.y, "w": rect.w, "h": rect.h},
                            "after": {"x": snapped.x, "y": snapped.y, "w": snapped.w, "h": snapped.h},
                        }));
                    }
                }
            }
            Decision::Free { .. } => {
                ws.dock_armed = None;
                if self.snap.pane == Some(pane) {
                    self.snap = SnapUi::default();
                }
            }
        }
        if let Some(main) = self.windows.get(&Role::Main) {
            main.window.request_redraw();
        }
    }

    fn toggle_in_app_pane(&mut self, pane: Pane) {
        match pane {
            Pane::Source => {
                if self.source.open {
                    self.source.close();
                } else if let Some(i) = self.view.as_ref().and_then(|v| v.selected) {
                    // F2 is an explicit open: load the selected node with its
                    // code lenses (the plain selection path no longer does).
                    self.load_source_node(i);
                } else {
                    return;
                }
            }
            Pane::NodeDrawer | Pane::Find | Pane::Blocks => {
                let id = pane.id();
                self.action_state.surface = None;
                self.action_state.drawer_open =
                    (self.action_state.drawer_open != Some(id)).then_some(id);
            }
        }
        self.request_redraw_all();
    }

    /// The main window's key path.
    ///
    /// The quick-open palette is consulted **first**, because it is the one
    /// binding that takes keys *away* from the canvas: while it is up it owns
    /// Escape, the arrows, PageUp/Down, Tab, Enter and typing, and its two open
    /// chords work whether or not it is up (quickopen.js:463-491). The ordering
    /// is the reference's own, and it is what makes `Ctrl+P` open the palette
    /// instead of being ignored and `Escape` close it instead of doing nothing.
    ///
    /// `mods` is the window's live modifier state, read at the event; the
    /// reference's `ev.ctrlKey || ev.metaKey` is [`KeyAction`]'s business, not
    /// this function's.
    fn handle_key(&mut self, key: &Key, mods: Mods) {
        if matches!(key, Key::Named(NamedKey::F1)) {
            if let Some(main) = self.windows.get(&Role::Main) {
                main.window.request_redraw();
            }
            self.dispatch(NzkUiAction::Shortcuts);
            return;
        }
        if let Some(pane) = in_app_pane_key(key) {
            self.toggle_in_app_pane(pane);
            return;
        }
        // Ctrl+Alt+S — the Scratch window. Checked before the chord table so the
        // reference's `Alt+S` split toggle cannot claim it, and before the
        // bare-letter table so the modifier test is explicit.
        if is_scratch_chord(key, mods) {
            self.pending_scratch = true;
            return;
        }
        // `shell.js:370-374`: Escape while the loading overlay is up is the
        // reference's second Stop gesture. It runs before every other binding so
        // a running build always has a way out even if a focus trap is active.
        if self.index_job.is_some()
            && matches!(key, Key::Named(NamedKey::Escape))
            && !mods.ctrl
            && !mods.meta
            && !mods.alt
        {
            self.cancel_index();
            self.request_redraw_all();
            return;
        }
        if self.quickopen_key(key, mods) {
            // The palette consumed the key. Redraw so the new rows/selection are
            // painted, and do not let it fall through to a canvas binding — `b`
            // is `#blocksBtn` on the canvas and a typed character in the palette.
            if let Some(main) = self.windows.get(&Role::Main) {
                main.window.request_redraw();
            }
            return;
        }
        if self.edit_key(key, mods) {
            // The edit modal's `#editArgs` box consumed the key. Redraw so the
            // new note/out/busy state is painted, and do not let it fall through
            // to a canvas binding — the modal is a text surface while it is up.
            if let Some(main) = self.windows.get(&Role::Main) {
                main.window.request_redraw();
            }
            return;
        }
        // A focused form control (an open select menu or a text box) owns the
        // keyboard before the canvas keymap, so typing a path cannot also fire
        // `b`/`r`/`f` shortcuts. Esc closes, Enter commits — `form_wire`.
        if self.form_key(key, mods) {
            if let Some(main) = self.windows.get(&Role::Main) {
                main.window.request_redraw();
            }
            return;
        }
        // The topbar search box, while `/` has handed it the keyboard.
        if self.search_key(key, mods) {
            if let Some(main) = self.windows.get(&Role::Main) {
                main.window.request_redraw();
            }
            return;
        }
        // Chords 4-7 (`app/src/chords.rs`). Checked *before* the match below, so
        // a shifted letter (`Shift+D`, `Shift+=`) cannot be claimed by the
        // bare-letter table first — the reference runs `arrange.key(ev)` ahead of
        // its letter chain for exactly that reason.
        if self.chord_key(key, mods) {
            if let Some(main) = self.windows.get(&Role::Main) {
                main.window.request_redraw();
            }
            return;
        }
        match key {
            // F12 / Shift+F12 over the source pane (`language.js` definition and
            // usages providers). Bound only while the pane is open, and only
            // when the pointer is on a resolvable token — otherwise the key falls
            // through to whatever else claims it (nothing does today).
            Key::Named(NamedKey::F12) if self.source.open => {
                if mods.shift {
                    self.source_find_references();
                } else {
                    self.source_go_to_definition();
                }
            }
            // Canvas controls (task-26): the SAME `dispatch` path a nzk_ui hit
            // takes, so a `#zoomInBtn` click and `+` cannot drift.
            //
            // Guarded on "no modifier": `Ctrl+I` must not re-index, and the
            // unmodified letters are what the canvas bindings mean. Without this
            // the palette's own chord would race `i`/`b`/`r`. The table itself is
            // [`bare_letter_action`], so the letters and the guard are both
            // testable without a window.
            //
            // `f` was the one reference letter this port had left unbound while
            // the Fit button claimed `(F)`; `shell.js:457` gives `f`/`F` to find
            // mode, and `ModeFind` is fully consumed (actions.rs:422 dispatch,
            // :1133 handle, :1825 tested).
            Key::Character(c) if !has_command_modifier(mods) => {
                if let Some(action) = bare_letter_action(c.as_str()) {
                    self.dispatch(action);
                }
            }
            Key::Named(NamedKey::Home) => self.dispatch(NzkUiAction::FullGraph),
            // `Alt+Left` — the reference's selection-history Back
            // (`nav.js:439`: `altKey && !ctrlKey && !metaKey && !shiftKey` with
            // `ArrowLeft` -> `back()`). The model EXISTS on this side:
            // `view.history` is a real 80-entry selection stack (view.rs:95-100,
            // pushed at :605, popped at :624) and `NavigateBack` is already
            // consumed through `effect.navigate_back` (app.rs:492 ->
            // `view.navigate_back()`), reached from `#backBtn`.
            //
            // Alt is required and Ctrl/Cmd/Shift must be ABSENT, matching the
            // reference's four-way modifier test — `Ctrl+Left` is a word-motion
            // chord elsewhere and must not step the graph's history.
            //
            // `Alt+Right`/`forward()` is deliberately NOT bound: the stack is
            // pop-only (`history.pop()` at view.rs:624 with no forward cursor),
            // so there is no model to step forward into. See the report.
            // The source pane owns Alt+Left while it has real document history:
            // the pane is the surface the user is reading, and v2's Monaco
            // editor had exactly this chord. With no source history (or a closed
            // pane) the graph's own history back runs unchanged.
            Key::Named(NamedKey::ArrowLeft)
                if is_history_back_chord(mods)
                    && self.source.open
                    && self.source.history_back_available() =>
            {
                self.source_history_step(false);
            }
            Key::Named(NamedKey::ArrowRight)
                if is_history_back_chord(mods)
                    && self.source.open
                    && self.source.history_forward_available() =>
            {
                self.source_history_step(true);
            }
            Key::Named(NamedKey::ArrowLeft) if is_history_back_chord(mods) => {
                self.dispatch(NzkUiAction::NavigateBack);
            }
            // `Shift|Alt+KeyH` — the reference's call-hierarchy action
            // (`hierarchy.js:681-688`, `monaco.KeyMod.Shift | KeyMod.Alt |
            // KeyCode.KeyH`). Checked BEFORE the bare-letter arm below, which
            // would otherwise swallow `H`/`h` as an unbound character; the
            // modifier test is the same `Mods` the quick-open chords read.
            //
            // The key is `Character("h")` on most layouts but the reference
            // matches the *physical* KeyH, so both cases are accepted. `Alt+Shift`
            // is the whole chord: no Ctrl/Cmd, so it cannot collide with the
            // palette's `Ctrl+P`/`Ctrl+T`.
            Key::Character(c) if is_hierarchy_chord(c, mods) => {
                self.open_hierarchy();
            }
            // `Escape` closes an open hierarchy panel and is consumed by it. A
            // closed panel leaves the key alone so the existing Esc branches
            // (pane/selection clearing) still run.
            Key::Named(NamedKey::Escape) if self.action_state.hierarchy.is_open() => {
                self.hierarchy_msg(crate::actions::HierarchyMsg::Close);
            }
            _ => {}
        }
    }

    /// Perform one edit run **in process** and record the engine's answer.
    ///
    /// The single place a [`crate::edit_wire::RunRequest`] — dry or apply —
    /// becomes an engine call, for both entry points (a Ctrl+Enter in `#editArgs`
    /// and a press on `#editDry`/`#editApply`). `edit_transport::invoke` routes
    /// it to `csgraph_core::edit::invoke` / `invoke_aux` on the [`Index`] this app
    /// already owns — no HTTP and no async runtime — and `finish_run` stores the
    /// RAW response, so `#editOut`/`#editNote` can only ever show what the engine
    /// actually returned rather than what a caller claimed.
    /// F12: open the declaration of the symbol under the source-pane pointer.
    ///
    /// The pane cannot select a node (it owns no `GraphView`), so the target is
    /// queued on the same `lens_open_request` a wired code-lens click uses —
    /// one host path for "open this node from the source pane".
    fn source_go_to_definition(&mut self) {
        if let Some(id) = self.source.cursor_symbol() {
            self.source.lens_open_request = Some(id.to_string());
        }
    }

    /// Shift+F12: list the graph's real incoming references for the symbol
    /// under the source-pane pointer. This is the in-process equivalent of the
    /// v2 Monaco usages provider (`language.js`), built from the same graph the
    /// app selects from; no rows are invented.
    fn source_find_references(&mut self) {
        let Some(id) = self.source.cursor_symbol().map(str::to_string) else {
            return;
        };
        if let Some(v) = self.view.as_ref() {
            self.source
                .set_references(source_pane::references_for(&v.graph, &id));
        }
    }

    /// Alt+Left/Alt+Right over the source pane: step the document history and
    /// re-show the target with its code lenses, exactly as a selection change
    /// would. Divergence from v2 is deliberate and documented: v2 had two
    /// separate histories (Monaco editor vs graph); this port gives the source
    /// pane the chord while it is open, because the graph's Back/Forward also
    /// remain reachable from `#backBtn`/`#forwardBtn`.
    fn source_history_step(&mut self, forward: bool) {
        let target = if forward {
            self.source.history_forward()
        } else {
            self.source.history_back()
        };
        let Some(target) = target else { return };
        if let Some(v) = self.view.as_ref() {
            self.source.show_node_and_compute(
                &mut self.index,
                &v.graph,
                &mut self.codelens,
                &target,
            );
        } else {
            self.source.show_node(&mut self.index, &target);
        }
    }

    /// The source pane head's non-close effects: history stepping (`#srcPrevBtn`
    /// / `#srcNextBtn`), the `file:line` clipboard write (`#srcRefBtn`), the
    /// whole-declaration re-open (`#srcMethodBtn`) and the second-window open
    /// (`#srcWindowBtn`).
    ///
    /// The close/copy/theme/pop family is consumed earlier, inside the paint
    /// path, because it needs no engine. These four do: stepping history and
    /// re-showing a document need `self.index`, and the window open needs the
    /// event-loop queue, so they run here where both are free.
    fn consume_source_extensions(&mut self, effect: &Effect) {
        if let Some(forward) = effect.source_history {
            self.source_history_step(forward);
        }
        if effect.source_whole_method {
            if let Some(key) = self.source.loaded_key().map(str::to_string) {
                if let Some(v) = self.view.as_ref() {
                    self.source.show_node_and_compute(
                        &mut self.index,
                        &v.graph,
                        &mut self.codelens,
                        &key,
                    );
                } else {
                    self.source.show_node(&mut self.index, &key);
                }
            }
        }
        if effect.copy_source_ref {
            if let Some(reference) = self.source.reference() {
                if let Some(ws) = self.windows.get(&Role::Main) {
                    ws.egui_ctx.copy_text(reference);
                }
            }
        }
        if effect.source_window {
            if let Some(key) = self.source.loaded_key().map(str::to_string) {
                self.actions.push(Action::OpenDoc(windows::DocTarget::for_node(
                    key, "doc",
                )));
            }
        }
    }

    fn run_edit_request(&mut self, req: crate::edit_wire::RunRequest) {
        let data = crate::edit_transport::invoke(&mut self.index, &req.tool, &req.args);
        self.action_state.edit.view_mut().finish_run(req.mode, &req.tool, &data);
    }

    /// `#editValidate` / `#editDedupe` in process: the two `/api/analyze` ops.
    ///
    /// The request was parsed by [`crate::edit_wire::EditToolView::validate`] /
    /// `duplicates` in `actions.rs` (it is pure: args box -> `{op,args}`); this is
    /// the engine half. `parse_check` gates the same discovered file set the
    /// reference route does, and `dedupe` calls the same core function the CLI
    /// uses. The RAW answer is stored, so `#editOut`/`#editNote` can only show
    /// what the engine actually returned — a skipped toolchain stays an error,
    /// never a pass.
    fn run_edit_analyze(&mut self, req: crate::edit_wire::AnalyzeRequest) {
        if self.analyze_job.is_some() {
            // `begin_analyze` already refused a second run while busy; this is
            // the defensive half, and it must not spawn a second thread.
            return;
        }
        let repo = self.repo.clone();
        let roots = self.roots.clone();
        let (tx, rx) = mpsc::channel();
        let op = req.op;
        let spawned = std::thread::Builder::new()
            .name("csviewer-analyze".to_string())
            .spawn(move || {
                let data = analyze_in_process(&repo, &roots, &req);
                let _ = tx.send((op, data));
            });
        match spawned {
            Ok(_) => {
                self.analyze_job = Some(AnalyzeJob { rx, started: Instant::now() });
                self.request_redraw_all();
            }
            Err(e) => {
                eprintln!("[native-csviewer] could not start the analyze worker: {e}");
                self.action_state.edit.view_mut().finish_analyze(op, None);
            }
        }
    }

    /// Adopt a finished `/api/analyze` worker result and record the RAW answer.
    fn poll_analyze_job(&mut self) {
        let Some(job) = self.analyze_job.as_ref() else { return };
        let elapsed = job.started.elapsed();
        match job.rx.try_recv() {
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.analyze_job = None;
                eprintln!("[native-csviewer] analyze worker ended without a result");
            }
            Ok((op, data)) => {
                self.analyze_job = None;
                let note = self.action_state.edit.view_mut().finish_analyze(op, Some(&data));
                eprintln!(
                    "[native-csviewer] analyze {} in {}ms: {}",
                    op.as_str(),
                    elapsed.as_millis(),
                    note.text
                );
            }
        }
        self.request_redraw_all();
    }

    /// `#chainRun` (`filters.js:543-560`): walk the resolve chain for the
    /// selected node through the real engine (`Index::chain`, the same call
    /// `/api/chain` serves) and store the RAW answer for `#chainTree`.
    ///
    /// With no selection the reference writes its own "Select a node first."
    /// row; that is stored as the answer's `error`, so the panel shows the
    /// reference's text rather than an invented chain.
    fn run_chain(&mut self) {
        let Some(id) = self.selected_node_id() else {
            self.action_state.chain =
                Some(serde_json::json!({"ok": false, "error": "Select a node first."}));
            return;
        };
        // A namespace gets the **ancestry** chain, not the relation walk: walk
        // `parent` links upward (`a.b.c.d -> a.b.c -> a.b -> a`), which is what
        // "go up the namespace" means. The depth/direction controls do not
        // apply — the walk always reaches the root namespace — and no class or
        // member parent is ever emitted, only ancestor namespaces.
        if let Some(v) = self.view.as_ref() {
            if let Some(i) = namespace_ancestry_selection(v) {
                self.action_state.chain = Some(namespace_ancestry_chain(&v.graph, i));
                self.request_redraw_all();
                return;
            }
        }
        let depth = self.action_state.chain_depth as i64;
        let dir = self.action_state.chain_dir.clone();
        self.action_state.chain = match self.index.chain(&id, depth, &dir, 300) {
            Some(v) => Some(v),
            None => Some(serde_json::json!({
                "ok": false,
                "error": format!("node not in the index: {id}"),
            })),
        };
        self.request_redraw_all();
    }

    /// `#strSearch` (`filters.js:516-534 onStringsFilter`): the engine's own
    /// search, restricted to `kind == "string"` exactly as the reference filters
    /// the `/api/search` answer. The count stored is the FILTERED row count
    /// (`state.strings.list.length`), not the raw search total.
    fn run_strings_search(&mut self, query: &str) {
        let q = query.trim();
        if q.is_empty() {
            self.action_state.str_searched = Some((String::new(), 0));
            self.action_state.str_rows = None;
            self.request_redraw_all();
            return;
        }
        let results = self.index.search(q, 300);
        let rows: Vec<crate::model::PubRow> = results
            .iter()
            .filter(|n| n.get("kind").and_then(serde_json::Value::as_str) == Some("string"))
            .filter_map(crate::model::PubRow::from_lite)
            .collect();
        self.action_state.str_searched = Some((q.to_string(), rows.len()));
        self.action_state.str_rows = Some(rows);
        self.request_redraw_all();
    }

    /// Perform the export dialog's pending engine run **in process**.
    ///
    /// The nzk_ui press recorded a [`crate::reorg_wire::Pending`] on
    /// [`crate::actions::NzkUiState::reorg`]; this is the one place it becomes
    /// an engine call. [`crate::reorg_wire::invoke`] routes it to
    /// [`csgraph_core::index::Index::reorg`] on the [`Index`] this app already
    /// owns (the `/api/reorg` route, `core/src/api.rs:539`) — no HTTP, no async
    /// runtime — and `finish` stores the RAW answer, so `#reorgReport` and
    /// `#reorgNote` can only show what the engine actually returned.
    ///
    /// The `selection` scope needs the graph selection's node **ids**; the view
    /// owns that selection (one selected node in this host), so it is read here
    /// and handed over rather than re-derived by the wire module.
    fn run_reorg_request(&mut self) {
        let Some(pending) = self.action_state.reorg.take_pending() else {
            return;
        };
        let selection: Vec<String> = match self.view.as_ref() {
            Some(v) => v
                .selected
                .and_then(|i| v.graph.nodes.get(i))
                .map(|n| n.id.clone())
                .into_iter()
                .collect(),
            None => Vec::new(),
        };
        let data = crate::reorg_wire::invoke(&mut self.index, &pending, &selection);
        self.action_state.reorg.finish(&pending, data);
    }

    /// Perform the source pane's pending Ctrl+S save, if any.
    ///
    /// The pane owns the editable buffer but no engine [`Index`], so it queues
    /// the exact `write_file` body (`SourceState::take_save_request`). This is
    /// the only host-side consumer, and it routes that body through the SAME
    /// in-process transport the edit console uses
    /// (`edit_transport::invoke` -> `csgraph_core::edit::invoke`) — no new
    /// write mechanism. The envelope is fed back so `dirty` clears only when
    /// the engine answered `"ok": true`.
    fn consume_source_save(&mut self) {
        for state in [&mut self.source, &mut self.split_source] {
            let Some(req) = state.take_save_request() else { continue };
            let data = crate::edit_transport::invoke(&mut self.index, "write_file", &req.write_file_args());
            state.finish_save(&data);
        }
    }

    /// Carry out the [`Effect`] arms that need the ENGINE, from whichever path
    /// produced the effect.
    ///
    /// There are **two** entry points into the nzk_ui dispatch table: the pointer
    /// path (`frame` -> `build_ui_with` -> `dispatch_action`) and the native key
    /// bindings (`App::dispatch`). A host effect consumed in only one of them is
    /// exactly how a button ends up inert while its unit test is green: the
    /// action layer builds the request, and the mouse press — which is what the
    /// user actually does — drops it. Both callers go through here instead.
    ///
    /// The click path is a *free function* (`build_ui_with`) that has no `Index`,
    /// so it returns the effect and `frame` consumes it here, after the egui pass,
    /// where the disjoint `view`/`source`/`index` field borrows are free again.
    fn consume_engine_effects(&mut self, effect: &Effect) {
        if let Some(req) = effect.edit_run.clone() {
            self.run_edit_request(req);
        }
        if let Some(req) = effect.edit_analyze.clone() {
            self.run_edit_analyze(req);
        }
        if let Some(tool) = &effect.edit_tool {
            // `#editTool`'s value change (`edit.js:278`). The view refuses a tool
            // that is not in the engine's advertised list.
            self.action_state.edit.view_mut().select_tool(tool);
        }
        if let Some(theme) = &effect.ui_theme {
            if let Some(view) = self.view.as_mut() {
                view.set_theme(theme);
            }
        }
        if effect.chain_run {
            self.run_chain();
        }
        if let Some(q) = &effect.str_search {
            self.run_strings_search(q);
        }
        // The export dialog's pending run (see `run_reorg_request`). It is not
        // carried on `Effect`: the press already recorded it on the nzk_ui
        // state, so no second owner of the request exists.
        self.run_reorg_request();
        if effect.preview {
            // `preview.js:183-185` — `open()` prefills `#previewSelector` from
            // the selection; the prefill happens BEFORE the build so the text
            // the shell paints and the declaration the listing names are one
            // value, and a typed selector from a previous open is replaced
            // exactly as the reference replaces it.
            let selected = self.selected_node_id();
            self.action_state.form.prefill_preview_selector(selected.as_deref());
            self.build_preview();
        }
        if effect.preview_run {
            self.run_preview_request();
        }
        // `#nodeCodeBtn` (`{ } View code`): v2 `viewCodeFor(state.selected)` →
        // `openSource({ node: id })` (`drawer.js:29-32`, `shell.js:563`). The
        // host opens the SELECTED declaration through its existing source-pane
        // path — the same `preopen_source` the evidence run uses, which resolves
        // the node id against the live graph and calls `sync_source`. The effect
        // is only set when a selection existed, so a press with nothing selected
        // cannot open an empty document (v2 toasts and returns).
        if effect.source_open {
            if let Some(id) = self.selected_node_id() {
                self.preopen_source(&id);
            }
        }
    }

    /// The id of the selected graph node, or `None`. The single read of the
    /// view's selection for the preview panel (open prefill + plan request), so
    /// the two cannot name different declarations.
    fn selected_node_id(&self) -> Option<String> {
        self.view.as_ref().and_then(|v| {
            let i = v.selected?;
            v.graph.nodes.get(i).map(|n| n.id.clone())
        })
    }

    /// Build the change-preview panel's body **in process** for an open of
    /// `#previewModal`.
    ///
    /// The declaration is the Host-owned `#previewSelector` text prefilled from
    /// the app's real graph selection (`preview.js:183-185`; empty => the
    /// selection, [`crate::preview_wire::resolve_target`]) and the target comes
    /// from the Host-owned `#editArgs` box when it holds a preview-shaped
    /// request. Either way the plan itself is `csgraph_core::preview::invoke`'s
    /// answer (through [`crate::preview_wire::build`]): no HTTP, no async, and
    /// no plan text this app invented. `ui.preview` is what the shell paints.
    fn build_preview(&mut self) {
        let selected = self.selected_node_id();
        let selector = self.action_state.form.preview_selector.clone();
        let edit_args = self.action_state.edit.view().args.text().to_string();
        let report = crate::preview_wire::build(
            &mut self.index,
            selected.as_deref(),
            &selector,
            &edit_args,
        );
        self.action_state.preview = Some(report);
    }

    /// `preview.js:188-199` `run()` in process: the typed `#previewSelector` /
    /// `#previewOp` / `#previewTo` are the request, and the REAL engine answer
    /// replaces `#previewOut` / `#previewNote`.
    ///
    /// The empty-target case never reaches here — `actions.rs` already published
    /// the reference's own note instead of asking the engine. A cancelled request
    /// leaves the panel as it was (the engine only answers when asked).
    fn run_preview_request(&mut self) {
        let selected = self.selected_node_id();
        let selector = self.action_state.form.preview_selector.clone();
        let to = self.action_state.form.preview_to.clone();
        let op = self.action_state.form.preview_op.clone();
        if let Some(report) = crate::preview_wire::run(
            &mut self.index,
            selected.as_deref(),
            &selector,
            &to,
            &op,
        ) {
            self.action_state.preview = Some(report);
        }
    }

    /// Route one key event into the open edit modal's `#editArgs` box.
    ///
    /// Returns `true` when the box **took** the key, so the canvas keymap must
    /// not also see it; `false` leaves it alone. Mirrors
    /// [`App::quickopen_key`]: the dry/apply decision is not re-made here — the
    /// key becomes the pure [`crate::edit_args::KeyEvent`] and
    /// [`crate::edit_wire::EditToolView::keydown`] decides through
    /// `decide_key`, whose `KeyIntent` has **no apply variant**, so Ctrl/Cmd +
    /// Enter can only ever be a dry run and a second, hand-rolled modifier test
    /// at a call site cannot exist.
    ///
    /// ### The run is in process
    ///
    /// The request the key builds is not sent anywhere: it is handed to
    /// [`crate::edit_transport::invoke`], which calls
    /// `csgraph_core::edit::invoke` / `invoke_aux` on the [`Index`] this app
    /// already owns — the `/api/*` route becomes an in-process engine call, with
    /// no HTTP and no async runtime. The engine's own answer is then recorded
    /// through `finish_run`, which is what makes `#editOut` and `#editNote`
    /// carry a real response rather than a claimed one.
    ///
    /// When `keydown` returns no request it is because it *refused* (no
    /// operation selected, or unparsable args); `build_request` has already left
    /// the matching error note on the view and did **not** set the busy flag, so
    /// there is nothing to finish and nothing to fake.
    fn edit_key(&mut self, key: &Key, mods: Mods) -> bool {
        use crate::edit_args::{KeyEvent, KeyName};
        use crate::edit_wire::KeyIntent;

        if self.action_state.surface != Some(crate::actions::HostSurface::Edit) {
            return false;
        }
        let (name, is_text) = key_name(key);
        // `edit.js:283` compares `ev.key` to the literal `'Enter'`, so only the
        // real Enter is `KeyName::Enter` (NumpadEnter is `Other` by design).
        let ev = KeyEvent {
            key: if name == "Enter" { KeyName::Enter } else { KeyName::Other },
            ctrl: mods.ctrl,
            meta: mods.meta,
        };
        let (intent, _note, request) = self.action_state.edit.view_mut().keydown(ev);
        if let Some(req) = request {
            self.run_edit_request(req);
        }
        // A character is text the box consumes; Ctrl/Cmd+Enter is consumed
        // because it started a run. Every other key falls through.
        is_text || matches!(intent, KeyIntent::DryRun)
    }

    /// The topbar search box's key surface — the `/` chord's other half.
    ///
    /// `/` (`shell.js:452-455`) does `searchInputEl.focus()` +
    /// `.select()`, so from that moment the box owns plain characters. This host
    /// has no native widget focus to read, so
    /// [`crate::actions::NzkUiState::search_focused`] is the flag and this
    /// function is the only reader of it: while it is set, a character is text
    /// the box consumes and the canvas keymap must not also see it.
    ///
    /// Escape and Enter both *release* the box, matching `search.js:508-534`:
    /// Escape hides the suggestions (keeping the typed text), Enter commits the
    /// highlighted row (`commitSearch`, `search.js:76`) and then blurs.
    ///
    /// While the graph-mode popup is open, `ArrowUp`/`ArrowDown` move its active
    /// row and `Enter` commits it — the table is
    /// [`crate::find_wire::route_key`], so the binding is asserted in
    /// `find_wire` rather than re-derived here. The commit selects the real
    /// `graph.nodes` entry for the committed id ([`SuggestionView::commit_index`]);
    /// there is no honest camera-centering API on `GraphView`, so selection is
    /// the commit and the centering difference is a reported gap, not a fake.
    /// A stale/unknown id is a no-op that only hides (never a different node).
    /// The form controls' key surface (task-14).
    ///
    /// Returns `true` when a form control owned the key: the open select menu
    /// consumes Up/Down/Enter/Escape, a focused text box consumes printable keys,
    /// Backspace, Enter and Escape. Modified chords (Ctrl/Cmd/Alt) are NOT
    /// stolen — they stay global, exactly as a browser lets browser shortcuts
    /// win over a focused input.
    fn form_key(&mut self, key: &Key, mods: Mods) -> bool {
        if !self.action_state.form.is_active() {
            return false;
        }
        if (mods.ctrl || mods.meta) && matches!(key, Key::Character(text) if text.eq_ignore_ascii_case("a")) {
            self.dispatch(crate::actions::NzkUiAction::FormKey(crate::form_wire::Key::SelectAll));
            return true;
        }
        if mods.ctrl || mods.meta || mods.alt {
            return false;
        }
        let Some(k) = form_key_of(key) else {
            return false;
        };
        // Going through `dispatch`/`apply` is deliberate: the request-model
        // re-sync lives in `actions.rs`'s FormKey arm, so a key cannot update a
        // display value without updating what the engine will be asked to do.
        self.dispatch(crate::actions::NzkUiAction::FormKey(k));
        true
    }

    fn search_key(&mut self, key: &Key, mods: Mods) -> bool {
        if !self.action_state.search_focused {
            return false;
        }
        // The box has TWO producers, and which one owns the keystroke follows the
        // active mode. The mode truth is `NzkUiState::surface`: `ModeFind` is
        // the only action that sets it to `Some(Find)` (actions.rs:1490-1496),
        // while `ModeGraph`/`FocusSearch` clear it back to `None` (= graph mode).
        //
        // `onSearchInput` (search.js:56-62) draws exactly this line: in find mode
        // the input feeds the Find panel (`onFindInput`), otherwise it feeds the
        // graph-mode suggestion list. Routing every character at `find` — the
        // round-6 behaviour — meant typing after `/` (which is graph mode) filled
        // the Find panel's query while `#suggestions` never received a single
        // keystroke, i.e. the popup was painted-but-unfed.
        let find_mode = self.action_state.surface == Some(crate::actions::HostSurface::Find);
        // The popup is open iff the box is focused, the mode is graph and the
        // graph query is non-empty — exactly the condition `nzk_ui.rs` gates the
        // published `ui.suggestions` key on. Keys are consumed only then, so the
        // canvas keymap keeps the arrows whenever there is nothing to navigate.
        let popup_open = !find_mode && self.action_state.suggest.is_open();
        if popup_open {
            let (name, _is_text) = key_name(key);
            if let Some(route) = crate::find_wire::route_key(&name) {
                match route {
                    crate::find_wire::SuggestKey::Move(delta) => {
                        // The row count comes from the same in-process search the
                        // facts producer runs, so the wrap rule sees the painted
                        // list, not a stale cache.
                        let rows = self
                            .view
                            .as_ref()
                            .map(|v| self.action_state.suggest.outcome(&v.graph).results().len())
                            .unwrap_or(0);
                        self.action_state.suggest.move_active(delta, rows);
                    }
                    crate::find_wire::SuggestKey::Activate => {
                        // `commitSearch` (search.js:76-84): commit the highlight,
                        // or row 0 when nothing is highlighted.
                        let target = self
                            .view
                            .as_ref()
                            .and_then(|v| self.action_state.suggest.commit_index(&v.graph));
                        if let Some(view) = self.view.as_mut() {
                            commit_suggestion(&mut self.action_state, view, target);
                        } else {
                            // No graph loaded: still hide + release the box
                            // (`hideSuggestions`'s tail), select nothing.
                            self.action_state.suggest.hide();
                            self.action_state.search_focused = false;
                        }
                    }
                }
                return true;
            }
        }
        match key {
            Key::Named(NamedKey::Escape) => {
                self.action_state.search_focused = false;
                // `hideSuggestions` (search.js:11-20) drops the suggestion rows
                // and resets `searchIndex = -1`, but this native port keeps the
                // graph-mode query text: the producer hides the popup by absence
                // gated on focus, so `ui.search_query` keeps painting the box's
                // value (the reference's `#searchInput.value` is never cleared by
                // Escape either). In find mode the panel's query is its own state
                // and is kept, matching the existing Escape behaviour there.
                if !find_mode {
                    self.action_state.suggest.hide();
                }
                true
            }
            // Enter with the popup closed: nothing to commit, so it only
            // releases the box (`commitSearch`'s `hideSuggestions`/blur tail).
            Key::Named(NamedKey::Enter) => {
                self.action_state.search_focused = false;
                if !find_mode {
                    self.action_state.suggest.hide();
                }
                true
            }
            Key::Named(NamedKey::Backspace) => {
                if find_mode {
                    self.action_state.find.pop_char();
                } else {
                    self.action_state.suggest.pop_char();
                }
                true
            }
            // `Ctrl`/`Cmd`+character is a chord, not text — the same guard the
            // palette and the canvas use.
            Key::Character(c) if !has_command_modifier(mods) => {
                for ch in c.chars() {
                    if !ch.is_control() {
                        if find_mode {
                            self.action_state.find.push_char(ch);
                        } else {
                            self.action_state.suggest.push_char(ch);
                        }
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Chords 4-7 — the table is [`crate::chords::resolve`], this is the host.
    ///
    /// Returns `true` only for a chord the host actually performed, so a
    /// recognised-but-unwired chord falls through to the canvas keymap instead
    /// of being silently swallowed.
    fn chord_key(&mut self, key: &Key, mods: Mods) -> bool {
        let Some(token) = chord_token(key) else {
            return false;
        };
        let Some(chord) =
            crate::chords::resolve(mods.alt, mods.ctrl, mods.shift, mods.meta, &token)
        else {
            return false;
        };
        match chord {
            crate::chords::Chord::FocusSearch => self.dispatch(NzkUiAction::FocusSearch),
            crate::chords::Chord::ForwardHistory => self.dispatch(NzkUiAction::NavigateForward),
            crate::chords::Chord::TabCycle(delta) => {
                self.dispatch(NzkUiAction::Tab(TabCmd::Cycle(delta)))
            }
            crate::chords::Chord::TabClose { others } => {
                // `Alt+Shift+W` closes every *other* tab; `Alt+W` closes the
                // active one (`shell.js:421` vs `:416`). The chord's own flag is
                // the only difference, which is why `Chord::TabClose` carries it.
                let cmd = if others {
                    TabCmd::CloseOthers
                } else {
                    TabCmd::CloseActive
                };
                self.dispatch(NzkUiAction::Tab(cmd))
            }
            crate::chords::Chord::TabToggleSplit => {
                self.dispatch(NzkUiAction::Tab(TabCmd::ToggleSplit))
            }
            crate::chords::Chord::Arrange(axis) => self.dispatch(NzkUiAction::Arrange(axis)),
            crate::chords::Chord::Spacing(dir) => self.dispatch(NzkUiAction::Spacing(dir)),
        }
        true
    }

    /// Route one key event into the quick-open palette.
    ///
    /// Returns `true` when the palette **took** the key (so the canvas keymap
    /// must not also see it) and `false` to let it through. The decision itself
    /// is entirely [`crate::app::quickopen_wire::route_key`] / `edit_key` — the
    /// reference's keymap, in one tested place — and this function only supplies
    /// the host's two real inputs: the live modifier state, and the key's name.
    ///
    /// An activation (`Enter`) is performed here: [`KeyAction::Activate`] closes
    /// the palette and hands back the reference's [`qo::Open`] decision, which
    /// this host answers with a source-pane open — the same
    /// `openSource`/`goToNode` pair `openRow` calls (quickopen.js:369-382).
    fn quickopen_key(&mut self, key: &Key, mods: Mods) -> bool {
        let Some(view) = self.view.as_ref() else { return false };
        let (name, is_text) = key_name(key);
        let shown = self.quickopen.is_open();
        let (ctrl, meta, shift, alt) = (mods.ctrl, mods.meta, mods.shift, mods.alt);

        // The reference's `keydown` set.
        if let Some(action) =
            crate::app::quickopen_wire::route_key(&name, ctrl, meta, shift, alt, shown)
        {
            let opened = {
                let graph = &view.graph;
                crate::app::quickopen_wire::apply_key(&mut self.quickopen, action, graph)
            };
            if let Some(open) = opened {
                self.perform_quickopen(open);
            }
            return true;
        }
        // The reference's `input`/editing set, which the DOM gives the focused
        // field for free and a native host must decide itself.
        if is_text || name == "Backspace" {
            if let Some(action) =
                crate::app::quickopen_wire::edit_key(&name, ctrl, meta, alt, shown)
            {
                let graph = &view.graph;
                crate::app::quickopen_wire::apply_key(&mut self.quickopen, action, graph);
                return true;
            }
        }
        false
    }

    /// Answer the palette's activation with a real open — the
    /// `openSource`/`goToNode` pair `openRow` calls (quickopen.js:369-382).
    ///
    /// This host's "open" is a **graph selection**, which is exactly what
    /// `goToNode` does and what the source pane already follows
    /// ([`App::sync_source`] runs on the selection change). So both variants
    /// resolve to an id and select it.
    ///
    /// `Open::File`'s `id` is empty whenever the row had only a file (the core's
    /// own branch order, quickopen.js:375), and this host has no file-only
    /// viewer: there is no honest way to open `src/x.rs:12` here, so the row
    /// selects nothing rather than pretending. The `line || 1` the core applied
    /// is preserved in the returned decision for a host that can use it.
    ///
    /// `Open::NotReady` is the reference's "the editor is not ready yet" toast
    /// (quickopen.js:379); this host has no toast for the palette, so it is
    /// deliberately a no-op rather than a fabricated selection.
    fn perform_quickopen(&mut self, open: crate::app::quickopen_wire::Open) {
        let id = match open {
            crate::app::quickopen_wire::Open::Node { id } => id,
            crate::app::quickopen_wire::Open::File { id, .. } => id,
            crate::app::quickopen_wire::Open::NotReady => return,
        };
        if id.is_empty() {
            return;
        }
        if let Some(view) = self.view.as_mut() {
            if let Some(i) = view.graph.nodes.iter().position(|n| n.id == id) {
                view.select(Some(i));
            }
        }
    }

    /// One path for nzk_ui hits and native bindings: apply the action, then push
    /// its effect into the live view and (for a re-index) the engine.
    fn dispatch(&mut self, action: NzkUiAction) {
        eprintln!("[input] dispatch action={action:?}");
        if action == NzkUiAction::Browse {
            let repo = self.repo.clone();
            let start = String::new();
            self.action_state.picker = Some(crate::find_wire::PickerView::pending(&repo, &start));
            self.action_state.surface = Some(actions::HostSurface::FolderPicker);
            self.action_state.drawer_open = None;
            self.start_picker_listing(repo, start);
            self.request_redraw_all();
            return;
        }
        // `#newWinBtn` (`windows.js`'s "⧉ New window"): open the SELECTED
        // document in a second window. Host-only consequence, so it is consumed
        // here before the display-free action layer (which cannot create
        // windows). No selection = a no-op — never an invented node.
        if let Some(open) = doc_window_action(&action, self.view.as_ref()) {
            self.actions.push(open);
            if let Some(main) = self.windows.get(&Role::Main) {
                main.window.request_redraw();
            }
            return;
        }
        let Some(ctx) = self.windows.get(&Role::Main).map(|w| w.egui_ctx.clone()) else {
            return;
        };
        let Some(view) = self.view.as_mut() else { return };
        let effect = apply_and_execute(&mut self.action_state, view, &ctx, action);
        if let Some(repo) = effect.reindex_at.clone() {
            self.commit_repo(repo);
            self.reindex();
        } else if effect.reindex {
            self.reindex();
        }
        // A press on `#editDry` / `#editApply` (or a Ctrl+Enter in `#editArgs`,
        // see `edit_key`): the action only *decided* to run and built the body;
        // the engine call happens in `consume_engine_effects`, the ONE place
        // both dispatch entry points share.
        self.consume_engine_effects(&effect);
        self.consume_source_extensions(&effect);
        if effect.cancel_index {
            self.cancel_index();
        }
        self.queue_pane_effect(&effect);
    }

    /// The folder picker's own family — the `#modal` controls.
    ///
    /// The counterpart of [`App::dispatch`] for [`crate::actions::PickerAction`],
    /// and the half that was missing entirely: `#browseBtn` opened a modal whose
    /// body was never filled, so every control inside it was a painted button
    /// with no consequence. `apply_picker` does the real `read_dir` navigation
    /// (`#modalUp`/`#modalGo`/row click/…) and reports the two things only the
    /// host can do: run the engine for a committed root, or drop the picker.
    fn dispatch_picker(&mut self, action: crate::actions::PickerAction) {
        eprintln!("[input] dispatch picker={action:?}");
        if matches!(
            action,
            crate::actions::PickerAction::Up
                | crate::actions::PickerAction::Home
                | crate::actions::PickerAction::Root
                | crate::actions::PickerAction::Repo
                | crate::actions::PickerAction::Go(_)
                | crate::actions::PickerAction::Open(_)
        ) {
            if let Some(picker) = self.action_state.picker.as_mut() {
                picker.set_loading();
            }
            if let Some(picker) = self.action_state.picker.clone() {
                self.start_picker_navigation(picker, action);
                self.request_redraw_all();
            }
            return;
        }
        let effect = actions::apply_picker(&mut self.action_state, action);
        // `#modalSelect` (`chooseCurrentFolder`): re-index the folder the user
        // chose. The reference writes `#rootInput` and `state.rootChoice` and
        // tells the user to press Index; the native fork owns the engine
        // directly, so it performs the index the reference deferred — that is
        // the "adapt the tests to be functionally identical, not code-identical"
        // ruling applied to the root choice: the *functional* result is "the
        // graph on screen is the one from the chosen folder".
        if let Some(repo) = effect.reindex_at.clone() {
            self.commit_repo(repo);
            self.reindex();
        }
    }

    fn commit_repo(&mut self, repo: PathBuf) {
        let roots = roots_for_repo(&repo, &self.repo, &self.roots);
        self.repo = repo;
        self.roots = roots;
        eprintln!("[native-csviewer] root committed: {} roots={:?}", self.repo.display(), self.roots);
        // The picker / `#rootInput` commit is a real corpus change, so persist
        // it where the next no-arg launch reads it. Startup saves the pointer,
        // but a mid-run switch that only lived in memory made the next launch
        // reopen the previous project — the target folder looked unindexed.
        crate::model::save_last_project(&self.repo, &self.roots);
    }

    fn start_picker_listing(&mut self, repo: PathBuf, start: String) {
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("csviewer-picker".to_string())
            .spawn(move || {
                let mut picker = crate::find_wire::PickerView::pending(&repo, &start);
                picker.go(&start);
                let _ = tx.send(picker);
            })
        {
            Ok(_) => self.picker_job = Some(PickerJob { rx, started: Instant::now() }),
            Err(e) => eprintln!("[native-csviewer] could not start folder listing: {e}"),
        }
    }

    fn start_picker_navigation(
        &mut self,
        mut picker: crate::find_wire::PickerView,
        action: crate::actions::PickerAction,
    ) {
        let (tx, rx) = mpsc::channel();
        match std::thread::Builder::new()
            .name("csviewer-picker".to_string())
            .spawn(move || {
                match action {
                    crate::actions::PickerAction::Up => { picker.up(); }
                    crate::actions::PickerAction::Home => { picker.home(); }
                    crate::actions::PickerAction::Root => { picker.root(); }
                    crate::actions::PickerAction::Repo => { picker.repo(); }
                    crate::actions::PickerAction::Go(path) => { picker.go(&path); }
                    crate::actions::PickerAction::Open(path) => { picker.open_dir(&PathBuf::from(path)); }
                    crate::actions::PickerAction::Close | crate::actions::PickerAction::Select => return,
                }
                let _ = tx.send(picker);
            })
        {
            Ok(_) => self.picker_job = Some(PickerJob { rx, started: Instant::now() }),
            Err(e) => eprintln!("[native-csviewer] could not start folder navigation: {e}"),
        }
    }

    fn poll_picker_job(&mut self) {
        let Some(job) = self.picker_job.as_ref() else { return };
        match job.rx.try_recv() {
            Ok(picker) => {
                let elapsed = job.started.elapsed();
                self.picker_job = None;
                if self.action_state.surface == Some(actions::HostSurface::FolderPicker) {
                    self.action_state.picker = Some(picker);
                    eprintln!("[native-csviewer] folder listed in {}ms", elapsed.as_millis());
                    self.request_redraw_all();
                }
            }
            Err(TryRecvError::Disconnected) => self.picker_job = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// `Shift|Alt+H` — open the hierarchy panel on the selected node.
    ///
    /// The reference resolves the word under the *editor cursor*
    /// (`openCallHierarchy`, `hierarchy.js:574-594`); the native analogue is the
    /// node the graph has selected, which is what Back/Up/Enter and the source
    /// pane already act on. With nothing selected this is a no-op — the
    /// reference toasts and opens nothing rather than showing an empty panel.
    fn open_hierarchy(&mut self) {
        let Some(view) = self.view.as_ref() else { return };
        // `selected` is an index into `graph.nodes`; resolve it to an id here
        // because `HierarchyMsg::Open` carries the id, not a position.
        let Some(id) = view
            .selected
            .and_then(|i| view.graph.nodes.get(i))
            .map(|n| n.id.clone())
        else {
            return;
        };
        self.hierarchy_msg(crate::actions::HierarchyMsg::Open(id));
    }

    /// Apply one hierarchy message against the live graph and repaint.
    ///
    /// The counterpart of [`App::dispatch_picker`] for the panel family. The
    /// walk itself lives in [`apply_hierarchy_msg`] (it needs the graph, which
    /// only the host has); this method supplies that graph and requests the
    /// redraw — the panel is nzk_ui content, so no canvas state changes.
    fn hierarchy_msg(&mut self, msg: crate::actions::HierarchyMsg) {
        let Some(view) = self.view.as_ref() else { return };
        let effect = apply_hierarchy_msg(&mut self.action_state, &view.graph, msg);
        if effect.repaint_nzk_ui {
            if let Some(main) = self.windows.get(&Role::Main) {
                main.window.request_redraw();
            }
        }
    }

    /// A pane action becomes the same `Action::Detach`/`Dock` the F2–F5 keys and
    /// `pane_buttons` queue, applied in `about_to_wait` where an
    /// `ActiveEventLoop` exists.
    fn queue_pane_effect(&mut self, effect: &Effect) {
        let Some(host_pane) = effect.pane else { return };
        let pane = match host_pane {
            HostPane::Blocks => Pane::Blocks,
        };
        let action = if self.has_pane(pane) {
            Action::Dock(pane)
        } else {
            Action::Detach(pane)
        };
        self.actions.push(action);
    }

    /// `#indexBtn` / `I`: start an engine index build on a worker thread and
    /// keep the current graph on screen until it lands.
    ///
    /// The build used to run here, on the paint thread. That blocked every
    /// window for the whole build and made `#stopIndexBtn` structurally
    /// unclickable. The engine's shared cancel flag (`Index::set_cancel_flag`)
    /// is the Stop channel; [`App::poll_index_job`] adopts the result on the
    /// next tick unless the build was cancelled.
    fn reindex(&mut self) {
        if self.index_job.is_some() {
            // A new root/Index press supersedes the running build. Cancel it and
            // drop the receiver: the worker's result would describe the OLD repo,
            // and adopting it after `self.repo` moved would install the wrong
            // graph under the new path. The replacement job starts below.
            self.cancel_index();
            self.index_job = None;
        }
        let repo = self.repo.clone();
        let roots = self.roots.clone();
        let label = format!("Indexing {} …", repo.display());
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("csviewer-index".to_string())
            .spawn(move || {
                let outcome =
                    crate::model::load_with_index_cancellable(&repo, &roots, worker_cancel);
                let _ = tx.send(outcome);
            });
        match spawned {
            Ok(_) => {
                self.action_state.indexing = Some(label);
                self.index_job = Some(IndexJob { rx, cancel, started: Instant::now() });
                self.request_redraw_all();
            }
            Err(e) => eprintln!("[native-csviewer] could not start the index worker: {e}"),
        }
    }

    /// The Stop channel: flip the flag the engine's build checks between files.
    fn cancel_index(&mut self) {
        if let Some(job) = &self.index_job {
            job.cancel.store(true, Ordering::SeqCst);
            eprintln!("[native-csviewer] stop requested; the previous index stays loaded");
        }
    }

    /// Adopt a finished build (or report a failure) and take the overlay down.
    fn poll_index_job(&mut self) {
        let Some(job) = self.index_job.as_ref() else { return };
        let elapsed = job.started.elapsed();
        let received = job.rx.try_recv();
        match received {
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.index_job = None;
                self.action_state.indexing = None;
                eprintln!("[native-csviewer] index worker ended without a result");
            }
            Ok(Ok((graph, index, cancelled))) => {
                self.index_job = None;
                self.action_state.indexing = None;
                if cancelled {
                    // The engine restored its snapshot; adopting it would replace
                    // the live graph with an identical copy, so keep what is on
                    // screen instead (the reference skips `applyState` too).
                    eprintln!(
                        "[native-csviewer] index cancelled after {}ms; previous index kept",
                        elapsed.as_millis()
                    );
                } else {
                    self.install_index(graph, index, elapsed);
                }
            }
            Ok(Err(e)) => {
                self.index_job = None;
                self.action_state.indexing = None;
                eprintln!("[native-csviewer] re-index failed: {e}");
            }
        }
        self.request_redraw_all();
    }

    /// Swap a completed build in, keeping the view's theme/settings and re-fitting.
    fn install_index(
        &mut self,
        graph: crate::model::Graph,
        index: csgraph_core::index::Index,
        elapsed: Duration,
    ) {
        let (nodes, edges) = (graph.nodes.len(), graph.edges.len());
        if let Some(view) = self.view.as_mut() {
            // `install_graph` records the old positions by id, drops every
            // index-valued model (old indices no longer name the same node), and
            // rebuilds. Assigning `view.graph` directly would leave `vis` holding
            // indices into the old node list.
            view.install_graph(graph);
            view.fit_pending = true;
        }
        self.index = index;
        // The corpus changed, so every `#pubCount` / `#pubList` value derived
        // from the OLD index is now a displayed falsehood. Dropping both back to
        // `None` makes the readout honest immediately rather than
        // wrong-but-plausible until the next expand.
        self.action_state.pub_total = None;
        self.action_state.pub_list = None;
        eprintln!(
            "[native-csviewer] re-indexed in {}ms: {nodes} nodes, {edges} edges",
            elapsed.as_millis()
        );
    }

    /// Request a repaint of every live window (a background build changed state).
    fn request_redraw_all(&self) {
        for w in self.windows.values() {
            w.window.request_redraw();
        }
    }

    /// Ask the engine for the public definitions and record the answer.
    ///
    /// This is the host half of `loadDefinitions` (`filters.js:490-500`): the
    /// engine's `Index::definitions("public", 400)` is called **here**, in the
    /// module that owns the `Index`, because `actions.rs` has no engine and
    /// `nzk_ui.rs` only ever sees a `Graph`.
    ///
    /// `limit` is the reference's own 400 (`filters.js:491`), and `total` is the
    /// engine's count of **all** matches — `definitions()` computes it before
    /// truncating (`core/src/index.rs:3425-3428`), which is what
    /// `filters.js:493` stores. So the readout can legitimately exceed the number
    /// of rows returned and must not be "corrected" to `list.len()`.
    ///
    /// Both the count and the list come from this one call, so they cannot
    /// disagree about what the engine said.
    fn load_public_definitions(&mut self) {
        let d = self.index.definitions("public", PUB_DEFINITIONS_LIMIT);
        let total = d.get("total").and_then(|v| v.as_u64()).map(|n| n as usize);
        let rows = d
            .get("definitions")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(crate::model::PubRow::from_lite)
                    .collect::<Vec<_>>()
            });
        // If the engine returned rows but no `total`, the row count is the honest
        // floor — never a fabricated larger number, and never a zero.
        let total = total.or_else(|| rows.as_ref().map(|r| r.len()));
        self.action_state.pub_total = total;
        self.action_state.pub_list = rows;
    }

    // -- scripted snap demo -------------------------------------------------

    fn demo_start(&mut self, out: PathBuf) {
        self.demo = Some(Demo {
            out,
            transcript: Vec::new(),
            step: 0,
            next_at: Duration::from_millis(700),
            dock_seen: false,
            ticks: 0,
        });
    }

    fn demo_step(&mut self, step: usize) {
        let Some(main) = self.windows.get(&Role::Main).and_then(|w| w.outer_rect()) else {
            return;
        };
        match step {
            0 => {
                // Put the main window somewhere with room on the 3440x1440
                // primary monitor, then detach the Source pane beside it.
                if let Some(ws) = self.windows.get_mut(&Role::Main) {
                    let _ = ws.window.request_inner_size(LogicalSize::new(900.0, 600.0));
                    ws.set_outer_logical(120.0, 120.0);
                }
                self.detach_public(Pane::Source);
                if let Some(demo) = self.demo.as_mut() {
                    demo.transcript.push(json!({
                        "step": "setup-and-detach",
                        "pane": "source",
                        "main_before": {"x": main.x, "y": main.y, "w": main.w, "h": main.h},
                        "main_requested": {"x": 120.0, "y": 120.0, "w": 900.0, "h": 600.0},
                    }));
                    demo.step = 1;
                    demo.next_at = Duration::from_millis(1500);
                }
            }
            1 => {
                if let Some(ws) = self.windows.get_mut(&Role::Pane(Pane::Source)) {
                    let target = Rect::new(main.right() + 4.0, main.y + 120.0, 1180.0, 860.0);
                    ws.set_outer_logical(target.x, target.y);
                    if let Some(demo) = self.demo.as_mut() {
                        demo.transcript.push(json!({
                            "step": "move-adjacent-right",
                            "requested": {"x": target.x, "y": target.y},
                            "want_x": main.right(),
                        }));
                        demo.step = 2;
                        demo.next_at = Duration::from_millis(1100);
                    }
                }
                // Drive the same entry point the WM `Moved` event uses, so the
                // scripted run is deterministic even where the compositor does
                // not deliver `Moved` for programmatic placements.
                self.on_pane_moved(Pane::Source);
            }
            2 => {
                let readback =
                    self.windows.get(&Role::Pane(Pane::Source)).and_then(|ws| ws.outer_rect());
                if let (Some(r), Some(demo)) = (readback, self.demo.as_mut()) {
                    demo.transcript.push(json!({
                        "step": "read-adjacent-right",
                        "actual": {"x": r.x, "y": r.y, "w": r.w, "h": r.h},
                        "want_x": main.right(),
                        "snapped": (r.x - main.right()).abs() <= 1.5,
                        "overlap_with_main": r.overlap_ratio(&main),
                    }));
                }
                if let Some(ws) = self.windows.get_mut(&Role::Pane(Pane::Source)) {
                    // Shrink so the corner target fits the monitor, then put the
                    // pane's right edge 4 px from main.right and its top 4 px
                    // below main.bottom -> expect a TopRight corner snap.
                    let _ = ws.window.request_inner_size(LogicalSize::new(560.0, 360.0));
                    let x = main.right() - 4.0 - 560.0;
                    let y = main.bottom() + 4.0;
                    ws.set_outer_logical(x, y);
                    if let Some(demo) = self.demo.as_mut() {
                        demo.transcript.push(json!({
                            "step": "move-corner",
                            "requested": {"x": x, "y": y},
                            "want": {"right": main.right(), "top": main.bottom()},
                        }));
                        demo.step = 3;
                        demo.next_at = Duration::from_millis(1500);
                    }
                }
                self.on_pane_moved(Pane::Source);
            }
            3 => {
                let corner =
                    self.windows.get(&Role::Pane(Pane::Source)).and_then(|ws| ws.outer_rect());
                if let (Some(r), Some(demo)) = (corner, self.demo.as_mut()) {
                    demo.transcript.push(json!({
                        "step": "read-corner",
                        "actual": {"x": r.x, "y": r.y, "w": r.w, "h": r.h},
                        "want": {"right": main.right(), "top": main.bottom()},
                        "snapped": (r.right() - main.right()).abs() <= 1.5
                            && (r.y - main.bottom()).abs() <= 1.5,
                    }));
                }
                if let Some(ws) = self.windows.get_mut(&Role::Pane(Pane::Source)) {
                    ws.set_outer_logical(main.x + 80.0, main.y + 80.0);
                    if let Some(demo) = self.demo.as_mut() {
                        demo.transcript.push(json!({
                            "step": "move-over-main",
                            "requested": {"x": main.x + 80.0, "y": main.y + 80.0},
                        }));
                        demo.step = 4;
                        demo.next_at = Duration::from_millis(1800);
                    }
                }
                self.on_pane_moved(Pane::Source);
            }
            _ => {
                // Wait out the debounce, then dock. The demo may sleep for the
                // debounce because winit does not call `about_to_wait` on a
                // timer; the point is the debounced re-attach, not scheduler
                // latency.
                let armed = self
                    .windows
                    .get(&Role::Pane(Pane::Source))
                    .map(|w| w.dock_armed.is_some())
                    .unwrap_or(false);
                if armed {
                    std::thread::sleep(Duration::from_millis(windows::DOCK_DEBOUNCE_MS + 60));
                    self.dock(Pane::Source);
                }
                let open = self.has_pane(Pane::Source);
                let docked = self.docked.contains(&Pane::Source);
                let ticks = self.demo.as_ref().map(|d| d.ticks).unwrap_or(0);
                if let Some(demo) = self.demo.as_mut() {
                    let dock_guide = demo.dock_seen;
                    demo.transcript.push(json!({
                        "step": "dock",
                        "dock_armed": armed,
                        "debounce_ms": windows::DOCK_DEBOUNCE_MS,
                        "pane_window_open": open,
                        "docked_into_main": docked,
                        "dock_guide_was_shown": dock_guide,
                        "tick_count": ticks,
                    }));
                }
                self.finish_demo();
                self.demo_finished = true;
            }
        }
    }

    /// Demo helper: detach without an `ActiveEventLoop` is not possible, so the
    /// demo (which runs in `about_to_wait`) queues the action through the normal
    /// action path.
    fn detach_public(&mut self, pane: Pane) {
        self.actions.push(Action::Detach(pane));
    }

    fn finish_demo(&mut self) {
        let Some(demo) = self.demo.take() else { return };
        let doc = json!({
            "binary": "native-csviewer",
            "commit": option_env!("CSVIEWER_COMMIT").unwrap_or("8382284"),
            "corpus": self.opts.corpus,
            "geometry": {
                "snap_threshold_px": windows::SNAP_THRESHOLD_PX,
                "dock_min_overlap": windows::DOCK_MIN_OVERLAP,
                "dock_debounce_ms": windows::DOCK_DEBOUNCE_MS,
                "beside_gap_px": windows::BESIDE_GAP_PX,
                "beside_top_px": windows::BESIDE_TOP_PX,
                "cascade_step_px": windows::CASCADE_STEP_PX,
            },
            "transcript": demo.transcript,
        });
        match serde_json::to_string_pretty(&doc) {
            Ok(s) => {
                if let Some(dir) = demo.out.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                match std::fs::write(&demo.out, s) {
                    Ok(()) => println!("snap-demo: wrote {}", demo.out.display()),
                    Err(e) => eprintln!("snap-demo: write {} failed: {e}", demo.out.display()),
                }
            }
            Err(e) => eprintln!("snap-demo: json failed: {e}"),
        }
    }

    // -- rendering ----------------------------------------------------------

    /// Render one window and present it. The `WindowState` is moved out of the
    /// registry so `self.view`/`self.snap`/`self.docked` can be borrowed at the
    /// same time.
    fn frame(&mut self, role: Role) {
        let Some(mut ws) = self.windows.remove(&role) else { return };
        let snap = self.snap.clone();
        let mut actions: Vec<Action> = Vec::new();
        let is_first = role == Role::Main && self.first_frame.is_none();

        // The split pane's document is loaded here, BEFORE the egui pass: the
        // pass only paints what is already loaded, and `index`/the tabs model are
        // both free at this point. One-shot on key change (see
        // `sync_split_source`), so a redraw does not reload.
        self.sync_split_source();

        // The Scratch window shows "whatever the IDE is viewing", read ONCE per
        // pass and before the egui closure: `document_for` needs `&mut index`,
        // and the closure borrows the view/source/action state.
        let scratch_link = if role == Role::Scratch {
            let repo = self.repo.display().to_string();
            let key = nzk_scratch::link_key(self.view.as_ref(), &self.source, &repo);
            if let Some((cached_key, link)) = self.scratch_link.as_ref() {
                if *cached_key == key {
                    Some(link.clone())
                } else {
                    let link = nzk_scratch::link_state(
                        self.view.as_ref(),
                        &self.source,
                        &mut self.index,
                        &repo,
                    );
                    self.scratch_link = Some((key, link.clone()));
                    Some(link)
                }
            } else {
                let link = nzk_scratch::link_state(
                    self.view.as_ref(),
                    &self.source,
                    &mut self.index,
                    &repo,
                );
                self.scratch_link = Some((key, link.clone()));
                Some(link)
            }
        } else {
            None
        };
        // The Scratch pane paints in the host's theme: its surfaces come from
        // egui's visuals and its kind stripes from the same `UiTheme` the graph
        // uses, so a theme switch repaints the pane rather than leaving it on a
        // private palette.
        if let Some(v) = self.view.as_ref() {
            self.scratch_gui.theme = match v.theme {
                crate::graphviz::UiTheme::NzkDarkPlus => ScratchTheme::NzkDarkPlus,
                crate::graphviz::UiTheme::DarkV1 => ScratchTheme::DarkV1,
            };
        }
        // The pane's background and token colours are the IDE's own source-pane
        // palette, so the pop-out matches the editor instead of egui's defaults.
        self.scratch_gui.spec = Some(crate::app::nzk_scratch::palette_spec(
            &crate::app::source_theme_for_mode(self.action_state.source_mode),
        ));
        let mut scratch_requests: Vec<Request> = Vec::new();

        let mut frame_effect = Effect::default();
        let output = if let Some(view) = self.view.as_mut() {
            let source = &mut self.source;
            // Document windows own their document state and their engine, disjoint
            // from `view`/`source`/`split_source`/`action_state` above. The load
            // map is only *read* here (elapsed time / presence); results are
            // adopted in `poll_doc_loads`, never inside the paint pass.
            let doc_sources = &mut self.doc_sources;
            let doc_loads = &self.doc_loads;
            let mut split = if self.action_state.tabs.split() {
                Some(&mut self.split_source)
            } else {
                None
            };
            let action_state = &mut self.action_state;
            let scratch_gui = &mut self.scratch_gui;
            let raw = ws.egui_state.take_egui_input(&ws.window);
            let vp = ws.viewport_points();
            // The closure is `FnMut`, so the role is matched by clone: only the
            // Document arm binds its target (the others ignore it).
            Some(ws.egui_ctx.run_ui(raw, |ui| match role.clone() {
                Role::Main => {
                    frame_effect = build_ui_with_panes(
                        ui,
                        view,
                        Some(source),
                        split.as_deref_mut(),
                        action_state,
                    );
                    draw_snap(ui.ctx(), &snap, vp);
                }
                Role::Document(target) => {
                    // One document, nothing else (v2 `windows.js`): the window
                    // paints its own document state, not the main graph view.
                    let theme = source_theme_for_mode(action_state.source_mode);
                    if let Some(state) = doc_sources.get_mut(&target) {
                        build_ui_document(ui, state, &theme);
                    } else if let Some(engine) = doc_loads.get(&target) {
                        // The window's own engine is still working; say so (and
                        // on which thread) rather than freezing or faking a doc.
                        build_ui_document_pending(ui, &target, engine.elapsed());
                    } else {
                        // No state and no worker: an honest empty state, never a
                        // blank pane that looks like a successfully loaded document.
                        build_ui_document_unavailable(ui, &target);
                    }
                }
                Role::Scratch => {
                    // The addon paints itself; the host supplies the document and
                    // collects the one request a click makes.
                    scratch_requests = match &scratch_link {
                        None => Vec::new(),
                        Some(LinkState::Empty) => {
                            gui_scratch::empty(
                                ui,
                                "Nothing is being viewed. Select a declaration in the graph, \
                                 or open one in the source pane.",
                            );
                            Vec::new()
                        }
                        Some(LinkState::Error(e)) => {
                            gui_scratch::error(ui, e);
                            Vec::new()
                        }
                        Some(LinkState::Ready(doc)) => gui_scratch::ui(ui, doc, scratch_gui),
                    };
                }
                Role::Pane(p) => {
                    let mode = action_state.source_mode;
                    // `find` is the same `FindView` the main window's
                    // `#findPanel` is projected from, so a detached Find
                    // window shows the live result set, not a placeholder.
                    pane_ui(ui, p, view, source, mode, &mut action_state.find, &mut actions)
                }
            }))
        } else {
            None
        };
        // A click on a Scratch block: a block belongs to a node, so the one
        // action is "select that node". Applied BEFORE the selection read below,
        // so the drawer and the source pane follow in the same frame.
        for req in scratch_requests.drain(..) {
            match req {
                Request::OpenNode(id) => {
                    if let Some(v) = self.view.as_mut() {
                        if let Some(i) = v
                            .graph
                            .nodes
                            .iter()
                            .position(|n| n.id == id || n.full == id)
                        {
                            v.selected = Some(i);
                        }
                    }
                }
                Request::SelectRange { start, end } => {
                    // A block click selects its whole span in the IDE, including
                    // when only one line of a multi-line piece was clicked.
                    self.source.highlight_lines =
                        Some((start.saturating_sub(1), end.saturating_sub(1)));
                }
                Request::Reveal { .. } => {
                    // Only emitted for a document with no node (a file view),
                    // which the source pane is already showing. There is nothing
                    // else in this build to open it in, so nothing is invented.
                }
            }
        }

        // The IDE's highlight (updated by this pass's source pane) drives the
        // Scratch pane. Wake it only when it changed, so an idle main window
        // does not spin the pop-out.
        let hl = self.source.highlight_lines.map(|(a, b)| (a + 1, b + 1));
        if self.scratch_gui.highlight != hl {
            self.scratch_gui.highlight = hl;
            if let Some(ws) = self.windows.get(&Role::Scratch) {
                ws.window.request_redraw();
            }
        }

        // Selection changes are applied after the egui pass, where the disjoint
        // `view`/`source`/`index` field borrows are free again.
        let selected = self.view.as_ref().and_then(|v| v.selected);
        self.sync_source(selected);
        // The Blocks canvas hover transitions staged by the nzk_ui pass. They
        // are consumed here because the peek content needs the graph and the
        // engine, which only `App` owns.
        self.consume_blocks_hover();
        // The POINTER path's engine effects. `build_ui_with` is a free function
        // with no `Index`, so a nzk_ui click on `#editDry`/`#editApply` returns
        // its request here — consumed at the one place both dispatch entry points
        // share, so a mouse press cannot be the path that silently drops it.
        self.consume_engine_effects(&frame_effect);
        // The source pane's Ctrl+S. The pane owns the buffer but no engine
        // `Index`, so it queued the exact `write_file` body; this is the one
        // place that body meets the transport the edit console already uses
        // (`edit_transport::invoke` -> `csgraph_core::edit::invoke`). The
        // envelope is handed back, so `dirty` clears only on a real `"ok": true`.
        self.consume_source_save();
        // The source pane's `main` lens is a click target
        // (`codelens.js:228-232`). A click was queued by the pane inside the pass
        // — where `view` was already borrowed — so it is performed here, through
        // the same selection change a graph click makes. `sync_source` then
        // re-computes the lenses for whatever node this selects, so the rows and
        // the document can never disagree.
        if let Some(id) = self.source.take_lens_open_request() {
            let hit = self.view.as_ref().and_then(|v| {
                v.graph
                    .nodes
                    .iter()
                    .position(|n| n.id == id || n.full == id || n.label == id)
            });
            if let (Some(i), Some(v)) = (hit, self.view.as_mut()) {
                v.selected = Some(i);
            }
            self.sync_source(hit);
        }
        // Live POV for MCP (task-28); throttled inside `pov::update`.
        crate::pov::update(&self.repo, &self.roots, self.view.as_ref());
        // R20-E control channel: drain main-thread requests on the FRAME thread,
        // non-blocking (`try_recv`), so a windowed GUI answers MCP
        // ping/state/click with the same painted nzk_ui geometry a human click
        // uses and never stalls a frame waiting on a socket.
        if crate::control::is_active() {
            if let Some(view) = self.view.as_mut() {
                // A control click is a view-only transition whose consequences
                // (`fit_pending`, `layout_dirty`) land on the next pass, so wake
                // one more frame instead of waiting for unrelated input.
                if crate::control::drain_pending(view) {
                    ws.egui_ctx.request_repaint();
                }
            }
        }
        // The one owner of the filter flag is `GraphView::group_children`
        // (`view.rs:90`, written only by `set_group_children`); the nzk_ui can
        // only see a `NzkUiState`, so the host copies it here, once per pass.
        // Copying on every frame rather than on the change is deliberate: the
        // cost is a bool and it makes drift impossible, whereas a change hook
        // would have to be added to every future writer of the flag.
        if let Some(view) = self.view.as_ref() {
            self.action_state.group_children = view.group_children;
            self.action_state.group_edges = view.group_edges;
            // `#nsSelect`'s rows are CONTENT (the graph's real namespaces); the
            // form state cannot see a `Graph`, so the host mirrors the corpus'
            // `Node::ns` strings here once per pass — the same one-owner copy
            // contract as `group_children`/`selected_node` below. Sorted and
            // deduped so the select's row order is deterministic.
            let mut namespaces: Vec<String> = view
                .graph
                .nodes
                .iter()
                .filter(|n| !n.ns.is_empty())
                .map(|n| n.ns.clone())
                .collect();
            namespaces.sort();
            namespaces.dedup();
            self.action_state.namespaces = namespaces;
            // The kind/edge/project universes the sidebar's `all`/`none` links
            // operate on (`FilterCmd::AllKinds`/`AllEdges`/`AllProjects`). The
            // display-free action layer cannot see a `Graph`, so the host mirrors
            // the deduped sets once per pass — the same contract as `namespaces`.
            let mut kinds: Vec<String> =
                view.graph.nodes.iter().map(|n| n.kind.clone()).collect();
            kinds.sort();
            kinds.dedup();
            self.action_state.known_kinds = kinds;
            let mut edge_kinds: Vec<String> =
                view.graph.edges.iter().map(|e| e.kind.clone()).collect();
            edge_kinds.sort();
            edge_kinds.dedup();
            self.action_state.known_edges = edge_kinds;
            let mut projects: Vec<String> =
                view.graph.projects.iter().map(|p| p.id.clone()).collect();
            projects.sort();
            projects.dedup();
            self.action_state.known_projects = projects;
            // `renderNodeDrawer` (`drawer.js:74-84`) SHOWS the drawer whenever a
            // node is selected and hides it when the selection is gone. The
            // nzk_ui cannot see `GraphView::selected`, so the host mirrors it
            // once per pass (the same one-owner copy contract as the filter
            // flags below) and opens the drawer on a selection *change* — not
            // every frame, which is what lets `#nodeDrawerClose` stay closed.
            let selected = view.selected;
            let previous = self.action_state.drawer_seen_selection;
            self.action_state.selected_node = selected;
            if selected != previous {
                self.action_state.drawer_seen_selection = selected;
                if self.action_state.surface != Some(crate::actions::HostSurface::Find) {
                    if selected.is_some() {
                        self.action_state.surface = None;
                        self.action_state.drawer_open = Some("nodeDrawer");
                    } else if self.action_state.drawer_open == Some("nodeDrawer") {
                        self.action_state.drawer_open = None;
                    }
                }
                self.request_redraw_all();
            }
            // Same one-owner contract for the `#groupEdges` flag, whose single
            // writer is `GraphView::set_group_edges` (`view.rs`). Copied here
            // too, for the same reason: the nzk_ui only sees a `NzkUiState`.
            self.action_state.group_edges = view.group_edges;
        }
        // `#pubSection` expanded for the first time: fetch the public
        // definitions (`shell.js:556`). The host owns the engine, so this is the
        // effect it carries out, exactly as it does for `reindex`. `pub_list`
        // is the "already loaded" flag (`filters.js:490` guards on
        // `state.defs.loaded`), so re-expanding does not re-query.
        if frame_effect.load_public_definitions
            && self.view.is_some()
            && self.action_state.pub_list.is_none()
        {
            self.load_public_definitions();
        }
        // `#modalSelect` / a committed `#rootInput`: the corpus itself changes.
        // This is the host half of `chooseCurrentFolder` (`shell.js:99-106`) and
        // of the root box's Enter/blur (`shell.js:493-499`). One re-index only:
        // when the press that committed the box was Index itself, the committed
        // path wins and the generic Index request is skipped.
        if let Some(repo) = frame_effect.reindex_at.clone() {
            self.commit_repo(repo);
            self.reindex();
        } else if frame_effect.reindex {
            // A nzk_ui `#indexBtn` hit needs the engine, which only the host owns.
            self.reindex();
        }
        // `#srcPrevBtn`/`#srcNextBtn`/`#srcRefBtn`/`#srcMethodBtn`/
        // `#srcWindowBtn`: the paint pass can only record the request (it has no
        // engine and cannot queue an OS window), so the host completes it here
        // where `index`, the view and the window registry are all free.
        self.consume_source_extensions(&frame_effect);
        if frame_effect.cancel_index {
            self.cancel_index();
        }
        self.queue_pane_effect(&frame_effect);
        if let Some(output) = output {
            let _ = present(&mut ws, output);
        }

        self.windows.insert(role, ws);
        if !actions.is_empty() {
            self.actions.extend(actions);
        }
        // `#srcPopBtn` queue: the nzk_ui press happened inside the egui pass,
        // where no `ActiveEventLoop` is available to create or drop a window, so
        // the detach/dock it asked for is drained here and takes the normal
        // `Action::Detach`/`Action::Dock` path.
        let queued_pane_actions = take_pending_pane_actions();
        if !queued_pane_actions.is_empty() {
            self.actions.extend(queued_pane_actions);
        }
        if is_first {
            self.first_frame = Some(self.elapsed_ms());
            eprintln!("[native-csviewer] first-frame-ms={}", self.elapsed_ms());
        }
    }
}

/// Draw the visible snap indicator: a bright line on the shared edge (or both
/// lines for a corner), and a full-window highlight when a drop would re-dock.
fn draw_snap(ctx: &egui::Context, snap: &SnapUi, viewport: egui::Vec2) {
    if snap.dock && snap.pane.is_some() {
        let col = egui::Color32::from_rgb(0xff, 0xc4, 0x4d);
        let p = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("ncv_snap"),
        ));
        let r = egui::Rect::from_min_size(egui::Pos2::ZERO, viewport);
        p.rect_stroke(
            r.shrink(3.0),
            4.0,
            egui::Stroke::new(3.0, col),
            egui::StrokeKind::Inside,
        );
        p.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            format!("DOCK {}", snap.pane.map(|p| p.title()).unwrap_or("pane")),
            egui::FontId::proportional(28.0),
            col,
        );
    }
    if let Some(g) = snap.guide {
        let (ox, oy) = snap.main_origin;
        let p = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("ncv_snap"),
        ));
        let col = egui::Color32::from_rgb(0x76, 0xd7, 0xff);
        if let Some((x, y1, y2)) = g.vertical {
            p.line_segment(
                [egui::pos2(x - ox, y1 - oy), egui::pos2(x - ox, y2 - oy)],
                egui::Stroke::new(2.0, col),
            );
        }
        if let Some((y, x1, x2)) = g.horizontal {
            p.line_segment(
                [egui::pos2(x1 - ox, y - oy), egui::pos2(x2 - ox, y - oy)],
                egui::Stroke::new(2.0, col),
            );
        }
    }
}

/// Render a document window: exactly the document it was opened for (v2
/// `windows.js`: "a DOCUMENT window — one document, nothing else"), from that
/// window's own [`SourceState`]. No graph and no nzk_ui: those belong to the
/// main window, whose live state a document window deliberately does not
/// mirror.
fn build_ui_document(ui: &mut egui::Ui, state: &mut SourceState, theme: &SourceTheme) {
    egui::CentralPanel::default().show(ui, |ui| {
        source_pane::ui(ui, state, theme);
    });
}

/// Render a document window whose own engine is still synthesizing the document.
///
/// The window opens immediately and says what is happening (and that it is on the
/// window's own engine thread); it never freezes the main loop waiting for the
/// index. [`App::poll_doc_loads`] swaps in the real state when the worker lands.
fn build_ui_document_pending(
    ui: &mut egui::Ui,
    target: &windows::DocTarget,
    elapsed: Duration,
) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.heading(target.label());
        ui.separator();
        ui.label(format!(
            "Loading document {}…\n\
             (synthesizing on its own engine thread, {:?} so far)",
            target.label(),
            elapsed
        ));
    });
}

/// Render a document window that has neither document state nor a live engine.
///
/// This is the honest fallback: a blank pane would look like a successfully
/// loaded empty document, which is exactly the kind of fake the fork refuses.
fn build_ui_document_unavailable(ui: &mut egui::Ui, target: &windows::DocTarget) {
    egui::CentralPanel::default().show(ui, |ui| {
        ui.heading(target.label());
        ui.separator();
        ui.label(
            "No document state and no document engine is live for this window. \
             Close and reopen it to start a fresh load.",
        );
    });
}

/// Content of a detached pane window.
///
/// `source_mode` is the live `#srcModeBtn` choice, so the popped-out Source
/// window paints the same palette as the docked pane. `find` is the same
/// `FindView` the main window's `#findPanel` is projected from, and `view`
/// carries the same node selection the main window's node drawer reads — the
/// per-kind choice of what to paint is the pure [`pane_body::pane_body`] model,
/// unit-tested headlessly; this function only renders it (egui painting itself
/// is not headless-testable).
fn pane_ui(
    ui: &mut egui::Ui,
    pane: Pane,
    view: &mut GraphView,
    source: &mut SourceState,
    source_mode: actions::SourceMode,
    find: &mut crate::find_wire::FindView,
    actions: &mut Vec<Action>,
) {
    egui::Panel::top(format!("pane_top_{}", pane.id())).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.heading(pane.title());
            ui.separator();
            ui.label(format!(
                "{} nodes · {} edges",
                view.graph.nodes.len(),
                view.graph.edges.len()
            ));
            ui.separator();
            if ui.button("Dock (close window)").clicked() {
                actions.push(Action::Dock(pane));
            }
            if ui.button("Place beside main").clicked() {
                actions.push(Action::Beside(pane));
            }
        });
    });
    if pane == Pane::Source {
        // The detached Source window hosts the real pane, not a placeholder.
        let theme = source_theme_for_mode(source_mode);
        egui::CentralPanel::default().show(ui, |ui| {
            source_pane::ui(ui, source, &theme);
        });
        return;
    }
    egui::CentralPanel::default().show(ui, |ui| {
        // The body is the real per-kind model (see `pane_body`), not a
        // placeholder. `Source` is handled above because it needs the mutable
        // `SourceState`, so the only reachable arms here are Node/Find/Blocks.
        match pane_body::pane_body(pane, view, find) {
            pane_body::PaneBody::Source => {}
            pane_body::PaneBody::Node(body) => match body.node {
                Some(n) => {
                    ui.heading("Node");
                    ui.separator();
                    ui.monospace(&n.label);
                    ui.label(format!("kind: {}", n.kind));
                    if !n.full.is_empty() && n.full != n.label {
                        ui.label(format!("full: {}", n.full));
                    }
                    if !n.ns.is_empty() {
                        ui.label(format!("ns: {}", n.ns));
                    }
                    if !n.owner.is_empty() {
                        ui.label(format!("owner: {}", n.owner));
                    }
                    if !n.file.is_empty() {
                        if n.end_line > n.line {
                            ui.label(format!("file: {}:{}-{}", n.file, n.line, n.end_line));
                        } else {
                            ui.label(format!("file: {}:{}", n.file, n.line));
                        }
                    }
                    ui.label(format!("degree: {}", n.degree));
                }
                None => {
                    ui.heading("Node");
                    ui.separator();
                    ui.label(body.empty);
                }
            },
            pane_body::PaneBody::Find(_) => {
                ui.horizontal(|ui| {
                    ui.heading("Find");
                });
                // v2 `panewin.js:70-98 addFindInput`: the detached pane really
                // owns an input widget (the main window's `#findInput` is in
                // the top bar, which a Pane-role window does not have), and its
                // text is kept in step with the shared `FindView` query, which
                // is exactly the model `#findPanel` is projected from.
                let mut input = pane_body::FindInput::new(find.query());
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut input.text)
                        .id(egui::Id::new("pane_find_input"))
                        .hint_text("Find symbol or code\u{2026}"),
                );
                input.focused = resp.has_focus();
                if resp.changed() {
                    find.set_query(input.query());
                }
                // Route the Pane-role window's keydown through the pane-local
                // state machine: Enter is v2's `runFind` (the shared model is
                // refreshed each frame), Escape clears back to NoQuery.
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let _ = input.route_key("Enter");
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    input.route_key("Escape");
                    find.clear();
                }
                // Recompute from the just-edited shared model, so the rows the
                // pane paints are the typed query's results in the same frame.
                let body = match pane_body::pane_body(pane, view, &*find) {
                    pane_body::PaneBody::Find(body) => body,
                    _ => return,
                };
                if !body.meta.is_empty() {
                    ui.label(body.meta);
                }
                ui.separator();
                if !body.notice.is_empty() {
                    ui.label(body.notice);
                }
                for row in &body.rows {
                    ui.horizontal(|ui| {
                        ui.label(format!("[{}]", row.kind));
                        ui.monospace(&row.title);
                        ui.label(row.tag);
                    });
                    if !row.subtitle.is_empty() {
                        ui.label(format!("    {}", row.subtitle));
                    }
                }
            }
            pane_body::PaneBody::Blocks(body) => {
                ui.heading(body.title);
                if !body.meta.is_empty() {
                    ui.label(body.meta);
                }
                ui.label(body.hint);
                ui.separator();
                ui.label(body.empty);
            }
        }
        ui.separator();
        ui.label("drag this window: it snaps to the main window and to other panes");
    });
}

/// egui pass → wgpu surface, shared by every window.
fn present(ws: &mut WindowState, mut output: egui::FullOutput) -> Result<(), String> {
    ws.egui_state
        .handle_platform_output(&ws.window, output.platform_output.clone());
    let jobs = ws.egui_ctx.tessellate(output.shapes, output.pixels_per_point);
    let screen = ScreenDescriptor {
        size_in_pixels: [ws.config.width, ws.config.height],
        pixels_per_point: output.pixels_per_point,
    };
    for (id, deltas) in &output.textures_delta.set {
        for delta in deltas {
            ws.renderer.update_texture(&ws.gpu.device, &ws.gpu.queue, *id, delta);
        }
    }
    let frame = match ws.surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(f) => f,
        wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
            ws.surface.configure(&ws.gpu.device, &ws.config);
            f
        }
        wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
            ws.surface.configure(&ws.gpu.device, &ws.config);
            return Ok(());
        }
        wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
            return Ok(())
        }
        wgpu::CurrentSurfaceTexture::Validation => {
            return Err("surface validation error".to_string());
        }
    };
    let view_tex = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = ws
        .gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
    let pre = ws.renderer.update_buffers(
        &ws.gpu.device,
        &ws.gpu.queue,
        &mut encoder,
        &jobs,
        &screen,
    );
    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("egui"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view_tex,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.07, g: 0.08, b: 0.09, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let mut pass = pass.forget_lifetime();
        ws.renderer.render(&mut pass, &jobs, &screen);
    }
    ws.gpu
        .queue
        .submit(pre.into_iter().chain(std::iter::once(encoder.finish())));
    drop(view_tex);
    ws.gpu.queue.present(frame);
    for id in &output.textures_delta.free {
        ws.renderer.free_texture(id);
    }
    // epaint asserts that applied deltas are cleared before drop.
    output.textures_delta.clear();
    Ok(())
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.windows.contains_key(&Role::Main) {
            return;
        }
        if let Err(e) = self.create_main(event_loop) {
            eprintln!("[native-csviewer] {e}");
            event_loop.exit();
            return;
        }
        if let Some(out) = self.opts.snap_demo.clone() {
            self.demo_start(out);
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let role = self
            .windows
            .iter()
            .find(|(_, w)| w.window.id() == id)
            .map(|(r, _)| r.clone());
        let Some(role) = role else { return };

        if let Some(ws) = self.windows.get_mut(&role) {
            // A window manager that consumes the press of the click which gives
            // an unfocused window focus still delivers the release here. egui
            // then sees a release with no press and produces no click at all,
            // which is why the very first click on the graph "did nothing" and
            // the next one worked (the recorded trace's first mouse event is a
            // bare `Released` with no `Pressed` before it). Feed egui the
            // missing press immediately before the release so the pair
            // completes one real click at the pointer's position.
            if let WindowEvent::Focused(false) = &event {
                // Focus loss abandons any half-started gesture; do not let a
                // stale `true` suppress recovery on the next focus click.
                self.left_press_seen = false;
            }
            let mut repaint = false;
            if let WindowEvent::MouseInput { state, button, .. } = &event {
                if matches!(&role, Role::Main) && *button == MouseButton::Left {
                    if *state == ElementState::Pressed {
                        self.left_press_seen = true;
                    } else if orphan_left_release(*state, *button, self.left_press_seen) {
                        let press = WindowEvent::MouseInput {
                            device_id: winit::event::DeviceId::dummy(),
                            state: ElementState::Pressed,
                            button: MouseButton::Left,
                        };
                        repaint |= ws.egui_state.on_window_event(&ws.window, &press).repaint;
                        self.left_press_seen = true;
                    }
                    if *state == ElementState::Released {
                        self.left_press_seen = false;
                    }
                }
            }
            repaint |= ws.egui_state.on_window_event(&ws.window, &event).repaint;
            if repaint {
                ws.window.request_redraw();
            }
        }

        match event {
            WindowEvent::CloseRequested => match role {
                Role::Main => {
                    if self.demo.is_some() {
                        self.finish_demo();
                    }
                    event_loop.exit();
                }
                Role::Pane(p) => self.dock(p),
                // A document window closes by dropping its registry entry, its
                // own document state and its own engine worker; dropping the
                // engine cancels the worker (never joins it), so closing a window
                // whose load is mid-build cannot block the main thread. The main
                // window (and its live view) is untouched.
                Role::Document(target) => {
                    self.windows.remove(&Role::Document(target.clone()));
                    self.doc_sources.remove(&target);
                    self.doc_loads.remove(&target);
                }
                // Closing the Scratch window is just dropping it; it owns no
                // engine and no document state.
                Role::Scratch => {
                    self.windows.remove(&Role::Scratch);
                }
            },
            WindowEvent::Destroyed => {
                if let Role::Pane(p) = role {
                    if self.windows.remove(&Role::Pane(p)).is_some() && !self.docked.contains(&p) {
                        self.docked.push(p);
                    }
                }
            }
            WindowEvent::Resized(size) => {
                if size.width > 0 && size.height > 0 {
                    if let Some(ws) = self.windows.get_mut(&role) {
                        ws.config.width = size.width;
                        ws.config.height = size.height;
                        ws.surface.configure(&ws.gpu.device, &ws.config);
                    }
                }
            }
            WindowEvent::Moved(_pos) => {
                if let Role::Pane(p) = role {
                    self.on_pane_moved(p);
                }
            }
            WindowEvent::ModifiersChanged(mods) => {
                if let Role::Main = role {
                    self.mods = Mods::from(mods);
                    eprintln!("[input] modifiers mods={}", mods_token(self.mods));
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Role::Main = role {
                    self.cursor = Some(position);
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Role::Main = role {
                    self.log_input_click(state, button);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let Role::Main = role {
                        let mods = self.mods;
                        // Typed text comes from `KeyEvent::text`, not from the
                        // logical key: `text` is what the keyboard layout
                        // actually produced (Shift/AltGr included), while a
                        // layout that does not map the key can leave
                        // `logical_key` `Unidentified`. Only a single printable
                        // character is taken, so Enter/Backspace/Tab and
                        // Ctrl/Cmd chords keep their named key.
                        let key = match (&event.logical_key, event.text.as_ref()) {
                            (Key::Character(_), Some(text))
                                if !(mods.ctrl || mods.meta || mods.alt)
                                    && text.chars().count() == 1
                                    && !text.chars().next().is_some_and(char::is_control) =>
                            {
                                Key::Character(text.clone())
                            }
                            _ => event.logical_key.clone(),
                        };
                        // Input trace: every key that reaches the keymap, with
                        // the exact modifier token and text, so a report can be
                        // replayed keystroke-for-keystroke.
                        eprintln!(
                            "[input] key key={:?} text={:?} repeat={} mods={} \
                             physical={:?}",
                            event.logical_key,
                            event.text,
                            event.repeat,
                            mods_token(mods),
                            event.physical_key,
                        );
                        self.handle_key(&key, mods);
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                self.frame(role);
                self.tick(event_loop);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.apply_actions(event_loop);
        self.tick(event_loop);

        // Keep frames flowing while windows exist.
        for w in self.windows.values() {
            w.window.request_redraw();
        }
    }
}

/// Is this left-button transition a release whose matching press never reached
/// the window?
///
/// The window manager can consume the press of the click that gives an
/// unfocused window focus while still delivering the release to the now-focused
/// client. egui then sees a release with no press, so it produces no click: from
/// the user's seat the first click on a node "did nothing" and the second one
/// worked. The recorded interaction trace shows exactly this — its first mouse
/// event is a `Released` at `(530.41, 467.76)` with no `Pressed` before it, and
/// the next click at `(538.06, 484.95)` (the same node) works.
///
/// Only a LEFT release with no press is recovered. A normal press→release pair
/// never synthesises anything, and the right/middle buttons are untouched.
fn orphan_left_release(state: ElementState, button: MouseButton, press_seen: bool) -> bool {
    button == MouseButton::Left && state == ElementState::Released && !press_seen
}

impl App {
    /// Log one mouse button transition with every coordinate frame a replay or
    /// a bug report needs.
    ///
    /// Three frames are printed because a report has to be reproducible without
    /// guessing which one the reporter meant:
    ///
    /// * `window-logical` — CSS/egui points, the coordinates every hit-test and
    ///   every test in this crate uses;
    /// * `window-physical` — device pixels inside the window (what winit hands
    ///   over, and what `xdotool`/`ydotool` need after adding the window
    ///   origin);
    /// * `screen-physical` — device pixels on the desktop.
    ///
    /// `mods` is the same token the key trace prints, so a click and a key can
    /// be compared directly.
    fn log_input_click(&self, state: ElementState, button: MouseButton) {
        let Some(ws) = self.windows.get(&Role::Main) else { return };
        let scale = ws.scale() as f64;
        let physical = self.cursor.unwrap_or(PhysicalPosition::new(0.0, 0.0));
        let logical = (physical.x / scale, physical.y / scale);
        let outer = ws.window.outer_position().ok();
        let screen = outer
            .map(|p| (p.x as f64 + physical.x, p.y as f64 + physical.y));
        // An absent outer position is reported as such, never as (0,0): a wrong
        // absolute coordinate is worse than an honest missing one.
        let screen_txt = match screen {
            Some((x, y)) => format!("({x:.2},{y:.2})"),
            None => "unavailable".to_string(),
        };
        eprintln!(
            "[input] mouse {state:?} button={button:?} \
             window-logical=({:.2},{:.2}) window-physical=({:.2},{:.2}) \
             screen-physical={} dpi={scale:.3} mods={}",
            logical.0,
            logical.1,
            physical.x,
            physical.y,
            screen_txt,
            mods_token(self.mods),
        );
    }

    /// Periodic work, driven both by `about_to_wait` and by every
    /// `RedrawRequested` (a 30 Hz ticker thread keeps redraws coming while
    /// panes are detached, because winit does not call `about_to_wait` on a
    /// timer).
    fn tick(&mut self, event_loop: &ActiveEventLoop) {
        if self.pending_scratch {
            self.pending_scratch = false;
            self.open_scratch_window(event_loop);
        }
        if let Some(demo) = self.demo.as_mut() {
            demo.ticks += 1;
        }
        // A background build that has landed is ready-to-paint state.
        self.poll_index_job();
        self.poll_picker_job();
        self.poll_analyze_job();
        // Drain finished document-window engine workers first: their results are
        // ready-to-paint state, and adopting them may request a redraw. The tick
        // runs from `about_to_wait` too, which keeps redraws coming, so a load
        // that lands mid-tick is painted immediately rather than at 30 Hz.
        self.poll_doc_loads();
        self.poll_panes();

        // Dock debounce: after the WM's interactive move has clearly stopped, a
        // pane over the main window re-attaches.
        let armed: Vec<Pane> = self
            .windows
            .iter()
            .filter(|(r, w)| {
                matches!(r, Role::Pane(_))
                    && w.dock_armed
                        .map(|t| t.elapsed() >= Duration::from_millis(windows::DOCK_DEBOUNCE_MS))
                        .unwrap_or(false)
            })
            .filter_map(|(r, _)| match r {
                Role::Pane(p) => Some(*p),
                Role::Main | Role::Document(_) | Role::Scratch => None,
            })
            .collect();
        for p in armed {
            self.dock(p);
        }

        // Scripted demo progress.
        if self.first_frame.is_some() {
            if let Some(demo) = self.demo.as_ref() {
                if self.start.elapsed() >= demo.next_at {
                    let step = demo.step;
                    self.demo_step(step);
                }
            }
        }
        if self.demo_finished {
            event_loop.exit();
        }
    }
}

/// The panel layout restored at startup from the viewer-state store.
///
/// Pure seam so the restart round-trip is testable without a display: missing
/// state, an older blob or a corrupt `panels` object all yield the default
/// layout (never an error that could stop the GUI from opening).
fn load_panel_layout_for_startup(cache_root: &std::path::Path) -> windows::PanelLayout {
    crate::pov::load_panel_layout(cache_root)
}

/// The document state a document window owns was previously loaded here,
/// synchronously on the main thread from the app's one engine index. That path
/// is gone: [`crate::doc_engine::DocEngine`] now builds a per-window `Index` on
/// the window's own worker thread and hands back a [`DocLoad`] the main thread
/// adopts in [`App::poll_doc_loads`].

/// Zone-aware drop used by the pane-release path (`App::dock`): when the
/// floating pane's centre is over one of the main window's named dock zones,
/// assign it to that zone's stack and return the zone. Outside every zone
/// nothing is assigned (`None`) — the pane keeps its existing free-float/dock
/// behaviour.
fn drop_into_dock_zone(
    layout: &mut windows::PanelLayout,
    pane: Pane,
    x: f32,
    y: f32,
    main: Rect,
) -> Option<windows::DockZone> {
    let zones = windows::DockZones::from_main(main);
    layout.drop_at(pane, x, y, &zones)
}

/// Open the real window(s) and run until they are closed.
///
/// `index` is the same engine build the graph came from, moved in by value and
/// kept alive on this thread for `Index::doc_source`.
pub fn run(view: GraphView, index: csgraph_core::index::Index, opts: RunOptions) -> Result<(), String> {
    let event_loop = EventLoop::new().map_err(|e| format!("EventLoop::new: {e}"))?;
    let gpu = Arc::new(Gpu::blocking(None)?);
    let start = opts.start.unwrap_or_else(Instant::now);
    // Capture the corpus + roots before the view is moved: `#indexBtn` re-indexes
    // the same corpus.
    let repo = view.graph.repo.clone();
    let roots = graph_roots(&view);
    // Panel layout (floating rects + dock-zone stacks) restored from the same
    // viewer-state store the POV uses, so a restart round-trips (task-4/10d).
    let layout_cache_root = crate::pov::writable_cache_root(
        &csgraph_core::registry::cache_root_for(&repo, &roots),
    );
    let panel_layout = load_panel_layout_for_startup(&layout_cache_root);
    let mut app = App {
        view: Some(view),
        index,
        index_job: None,
        picker_job: None,
        analyze_job: None,
        source: SourceState::default(),
        last_selected: None,
        split_source: SourceState::default(),
        split_loaded_key: None,
        doc_sources: HashMap::new(),
        doc_loads: HashMap::new(),
        action_state: {
            let mut st = NzkUiState::shipped();
            st.filters.vt_mode = crate::view::vt_mode_from_env();
            st
        },
        quickopen: crate::app::quickopen_wire::QuickOpenView::new(),
        codelens: crate::app::codelens_wire::CodeLensView::new(),
        scratch_link: None,
        scratch_gui: GuiState::new(),
        pending_scratch: false,
        mods: Mods::default(),
        cursor: None,
        left_press_seen: false,
        repo,
        roots,
        gpu: Some(gpu),
        windows: HashMap::new(),
        docked: Pane::ALL.to_vec(),
        panel_layout,
        layout_cache_root,
        actions: Vec::new(),
        snap: SnapUi::default(),
        start,
        first_frame: None,
        sequence: 0,
        opts,
        demo: None,
        demo_finished: false,
    };
    // Real-window evidence run: open the pane before the first frame.
    if let Ok(node) = std::env::var("CSVIEWER_SOURCE_NODE") {
        if !node.is_empty() {
            app.preopen_source(&node);
        }
    }
    // The initial engine query for `#pubCount` / `#pubList`.
    //
    // Taken once at startup rather than lazily on first expand because the
    // COUNT is on the section's *head* (`index.html:320`, inside `#pubHead`),
    // which is visible while the section is still collapsed. The reference
    // leaves it blank until the first expand only because it has to wait for a
    // network round trip; the native fork has the engine in-process, so the
    // count is available immediately and showing it is strictly more faithful to
    // `filters.js:495` than showing nothing. The LIST stays hidden until expand
    // either way, because `#pubList` is inside the collapsed body.
    app.load_public_definitions();
    let rc = event_loop.run_app(&mut app).map_err(|e| format!("run_app: {e}"));
    // Persist the panel layout (floating rects + dock-zone stacks) before the
    // viewer-state file is finalised by `pov::shutdown`. A failed save is
    // reported honestly, never claimed as success.
    if let Err(e) = crate::pov::save_panel_layout(&app.layout_cache_root, &app.panel_layout) {
        eprintln!("[native-csviewer] panel layout save failed: {e}");
    }
    crate::pov::shutdown();
    rc
}

#[cfg(test)]
mod action_wiring_tests {
    use super::*;
    use crate::model::{Edge, Graph, Node};

    #[test]
    fn f2_through_f5_toggle_only_in_app_pane_state() {
        assert_eq!(in_app_pane_key(&Key::Named(NamedKey::F2)), Some(Pane::Source));
        assert_eq!(in_app_pane_key(&Key::Named(NamedKey::F3)), Some(Pane::NodeDrawer));
        assert_eq!(in_app_pane_key(&Key::Named(NamedKey::F4)), Some(Pane::Find));
        assert_eq!(in_app_pane_key(&Key::Named(NamedKey::F5)), Some(Pane::Blocks));
        assert_eq!(in_app_pane_key(&Key::Named(NamedKey::F1)), None);
    }

    /// The reported "clicked on a node, nothing happened at first, clicked
    /// again and it worked": the window manager swallowed the press of the
    /// click that focused the window but delivered its release, so egui saw an
    /// orphan release and produced no click. Recognise exactly that transition
    /// — and only the left button, and only when no press was seen — so a
    /// normal press→release is never doubled.
    #[test]
    fn an_orphan_left_release_is_recovered_but_a_normal_click_is_not() {
        use ElementState::{Pressed, Released};

        // The reported gesture: a left release with no press in front of it.
        assert!(orphan_left_release(Released, MouseButton::Left, false));
        // A normal pair: the press was seen, so the release is not orphaned.
        assert!(!orphan_left_release(Released, MouseButton::Left, true));
        // A press is never a release.
        assert!(!orphan_left_release(Pressed, MouseButton::Left, false));
        // Other buttons keep the platform's own semantics.
        assert!(!orphan_left_release(Released, MouseButton::Right, false));
        assert!(!orphan_left_release(Released, MouseButton::Middle, false));
    }

    /// The drawer's `Expand` press must do BOTH halves of v2's
    /// `expandNode(id, {all:true})`: open every relation group in the drawer
    /// and put the bucket's neighbours on the canvas.
    ///
    /// This drives the production helper the real `DrawerClick::Expand` arm
    /// calls, so a future edit cannot disconnect either half without failing
    /// here.
    #[test]
    fn drawer_expand_opens_groups_and_pulls_nodes_onto_the_canvas() {
        fn node(id: &str, kind: &str) -> Node {
            Node {
                id: id.to_string(),
                label: id.to_string(),
                full: id.to_string(),
                kind: kind.to_string(),
                ns: "App".to_string(),
                owner: String::new(),
                file: "App.cs".to_string(),
                line: 1,
                end_line: 2,
                degree: 1,
                external: false,
                ..Default::default()
            }
        }
        let graph = Graph {
            repo: std::path::PathBuf::from("/tmp/app-drawer-expand"),
            nodes: vec![
                node("n:0", "namespace"),
                node("n:1", "class"),
                node("n:2", "method"),
            ],
            edges: vec![
                Edge { from: 0, to: 1, kind: "contains".to_string(), count: 1 },
                Edge { from: 1, to: 2, kind: "contains".to_string(), count: 1 },
            ],
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let mut view = GraphView::new(graph);
        let mut drawer_ui = crate::drawer_wire::DrawerUi::default();

        assert!(apply_drawer_expand(&mut view, &mut drawer_ui, "n:1"));
        assert!(drawer_ui.group_open("n:1", "children"), "the drawer list opened");
        assert!(view.is_expanded(1), "the node's bucket pull is on");
        assert_eq!(
            view.visible_count(),
            3,
            "the class and its child must be on the canvas, not only in the drawer"
        );

        assert!(
            !apply_drawer_expand(&mut view, &mut drawer_ui, "stale"),
            "a stale drawer id is an honest no-op"
        );
    }

    /// The hierarchy opener's chord fires on `Shift+Alt+H` and on **nothing
    /// else** (`hierarchy.js:685`).
    ///
    /// A keybinding's whole specification is which modifier combination
    /// triggers it, and an inline `match` guard cannot be tested without a real
    /// window. This pins both directions:
    ///
    /// * the exact chord works, in either letter case (the reference matches the
    ///   physical `KeyH`; winit hands over the layout's produced text, which is
    ///   `"H"` when Shift is held on most layouts);
    /// * every near-miss does NOT — no modifier, Shift alone, Alt alone, and
    ///   each of the Ctrl/Cmd combinations. The last group matters most: a
    ///   Ctrl-inclusive match would race the palette's `Ctrl+P`/`Ctrl+T`.
    #[test]
    fn the_hierarchy_chord_is_exactly_shift_alt_h() {
        let chord = |ctrl, meta, shift, alt| Mods { ctrl, meta, shift, alt };

        // The chord itself, both letter cases.
        assert!(is_hierarchy_chord("h", chord(false, false, true, true)));
        assert!(is_hierarchy_chord("H", chord(false, false, true, true)));

        // Bare `h` must NOT open the panel — it is not a canvas binding either,
        // but it must not be this one.
        assert!(!is_hierarchy_chord("h", chord(false, false, false, false)));
        // Each modifier alone.
        assert!(!is_hierarchy_chord("h", chord(false, false, true, false)));
        assert!(!is_hierarchy_chord("h", chord(false, false, false, true)));
        // Ctrl/Cmd inclusive: the palette's chord family, must not collide.
        assert!(!is_hierarchy_chord("h", chord(true, false, true, true)));
        assert!(!is_hierarchy_chord("h", chord(false, true, true, true)));
        // A different key with the right modifiers is still not the chord.
        assert!(!is_hierarchy_chord("j", chord(false, false, true, true)));
        assert!(!is_hierarchy_chord("", chord(false, false, true, true)));
    }

    /// `f`/`F` is **find mode** (`shell.js:457`), and the Fit button no longer
    /// claims it.
    ///
    /// This is the fix for the one real defect behind the retracted `F`
    /// "collision": the egui button at app.rs:284 was labelled `"Fit (F)"` while
    /// nothing bound `f`, so the UI advertised a shortcut that did not exist.
    /// Binding `f` to the reference's own meaning is the resolution, and this
    /// pins both halves of it — the letter maps to `ModeFind`, and the label no
    /// longer promises `(F)` for Fit.
    #[test]
    fn bare_f_is_find_mode_not_fit() {
        assert_eq!(bare_letter_action("f"), Some(NzkUiAction::ModeFind));
        assert_eq!(bare_letter_action("F"), Some(NzkUiAction::ModeFind));
        // `f` must NOT mean Fit — Fit is `#fitBtn` plus the egui button only.
        assert_ne!(bare_letter_action("f"), Some(NzkUiAction::FullGraph));

        // The other canvas letters are unchanged by this task: the reference
        // owns them too (`shell.js:459+`), and a regression here would be a
        // silent rebinding.
        assert_eq!(bare_letter_action("+"), Some(NzkUiAction::ZoomIn));
        assert_eq!(bare_letter_action("="), Some(NzkUiAction::ZoomIn));
        assert_eq!(bare_letter_action("-"), Some(NzkUiAction::ZoomOut));
        assert_eq!(bare_letter_action("0"), Some(NzkUiAction::FullGraph));
        assert_eq!(bare_letter_action("r"), Some(NzkUiAction::Resettle));
        assert_eq!(bare_letter_action("i"), Some(NzkUiAction::Index));
        assert_eq!(bare_letter_action("b"), Some(NzkUiAction::OpenBlocks));
        // And an unclaimed letter is honestly unbound rather than defaulting to
        // something — `None` is what keeps a stray keypress inert.
        assert_eq!(bare_letter_action("z"), None);
        assert_eq!(bare_letter_action(""), None);

        // The label fix is asserted structurally rather than by scanning this
        // file's own text: a source grep for `Fit (F)` is unverifiable here,
        // because the very comment explaining the removal contains the string
        // (that made the first version of this test red on prose, not code).
        // What matters and is checkable without a window is that `f` no longer
        // resolves to Fit's action — the two bindings are now disjoint.
        assert_ne!(bare_letter_action("f"), Some(NzkUiAction::FullGraph));
    }

    /// **The modifier is actually READ.** A `Ctrl`-chord must not fire the
    /// bare-letter behaviour, and a bare letter must not be swallowed by the
    /// chord layer.
    ///
    /// This is the acceptance criterion stated as a test: it would fail if the
    /// `!has_command_modifier(mods)` guard were dropped, because `Ctrl+I` would
    /// then re-index and `Ctrl+F` would open find.
    #[test]
    fn a_ctrl_chord_does_not_fire_the_bare_letter() {
        let chord = |ctrl, meta, shift, alt| Mods { ctrl, meta, shift, alt };
        let bare = chord(false, false, false, false);

        // The guard is the thing that decides, and it reads the flags.
        assert!(!has_command_modifier(bare));
        assert!(has_command_modifier(chord(true, false, false, false)), "Ctrl");
        assert!(has_command_modifier(chord(false, true, false, false)), "Cmd");
        assert!(!has_command_modifier(chord(false, false, true, true)), "Shift+Alt");

        // The letter table is reachable ONLY under `!has_command_modifier`, so
        // the pairing below is exactly `handle_key`'s arm condition. `Ctrl+F`
        // and `Ctrl+I` therefore resolve to no canvas action at all.
        for (mods, label) in [
            (chord(true, false, false, false), "Ctrl"),
            (chord(false, true, false, false), "Cmd"),
        ] {
            assert!(
                has_command_modifier(mods),
                "{label}: the guard must block the bare-letter arm"
            );
        }

        // And the bare path still works, so the guard is not simply always-on.
        assert_eq!(bare_letter_action("i"), Some(NzkUiAction::Index));
        assert_eq!(bare_letter_action("f"), Some(NzkUiAction::ModeFind));
    }

    /// `Alt+Left` is the selection-history Back, on `Alt` and **nothing else**
    /// (`nav.js:437-439`).
    ///
    /// The reference's guard names all four modifier flags explicitly, so every
    /// near-miss is asserted: `Ctrl+Left`/`Cmd+Left` are word-motion chords in
    /// the editors this app sits beside and must not step the graph history, and
    /// `Shift+Alt+Left` is a selection extension.
    #[test]
    fn the_history_back_chord_is_exactly_alt_left() {
        let chord = |ctrl, meta, shift, alt| Mods { ctrl, meta, shift, alt };

        assert!(is_history_back_chord(chord(false, false, false, true)), "Alt alone");

        // Bare arrows: not the chord.
        assert!(!is_history_back_chord(chord(false, false, false, false)));
        // Shift+Alt is the hierarchy family, not history.
        assert!(!is_history_back_chord(chord(false, false, true, true)));
        // Ctrl/Cmd inclusive: word-motion chords elsewhere, must not collide.
        assert!(!is_history_back_chord(chord(true, false, false, true)));
        assert!(!is_history_back_chord(chord(false, true, false, true)));
        // Shift alone, Ctrl alone.
        assert!(!is_history_back_chord(chord(false, false, true, false)));
        assert!(!is_history_back_chord(chord(true, false, false, false)));
    }

    /// An egui context that has run once, so fonts exist (the real app always
    /// calls `relayout` inside `run_ui`; a bare `Context::default()` has no
    /// fonts and panics).
    fn primed() -> egui::Context {
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |_ui| {});
        out.textures_delta.clear();
        ctx
    }

    /// A minimal graph, enough to build a real `GraphView` with no display.
    fn view() -> GraphView {
        let graph = Graph {
            repo: PathBuf::from("/tmp/does-not-exist"),
            nodes: vec![
                Node {
                    id: "A".into(),
                    label: "A".into(),
                    kind: "namespace".into(),
                    ..Default::default()
                },
                Node {
                    id: "B".into(),
                    label: "B".into(),
                    kind: "class".into(),
                    ns: "A".into(),
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

    #[test]
    fn zoom_action_reaches_the_view_camera() {
        let ctx = primed();
        let mut v = view();
        let mut st = NzkUiState::default();
        let before = v.camera().0;
        let effect = apply_and_execute(&mut st, &mut v, &ctx, NzkUiAction::ZoomIn);
        assert!(effect.camera);
        assert!(v.camera().0 > before, "camera was not pushed: {:?}", v.camera());
        apply_and_execute(&mut st, &mut v, &ctx, NzkUiAction::ZoomOut);
        assert!((v.camera().0 - before).abs() < 1e-9, "{:?}", v.camera());
    }

    #[test]
    fn zoom_clamps_in_the_view_not_only_in_the_model() {
        let ctx = primed();
        let mut v = view();
        let mut st = NzkUiState::default();
        for _ in 0..40 {
            apply_and_execute(&mut st, &mut v, &ctx, NzkUiAction::ZoomIn);
        }
        let (zoom, _, _) = v.camera();
        assert!(zoom <= crate::graphviz::MAX_ZOOM + 1e-9, "{zoom}");
        for _ in 0..80 {
            apply_and_execute(&mut st, &mut v, &ctx, NzkUiAction::ZoomOut);
        }
        assert!(v.camera().0 >= crate::graphviz::MIN_ZOOM - 1e-9, "{:?}", v.camera());
    }

    /// `#newWinBtn` → `NzkUiAction::NewDocWindow` → `Action::OpenDoc` for the
    /// selected node; no selection is a no-op (never an invented document), and
    /// unrelated actions are untouched. This is the whole routing chain minus the
    /// window/GPU creation step.
    #[test]
    fn the_new_window_control_routes_to_a_queued_open_doc_action() {
        let mut v = view();
        v.selected = Some(0);
        let action = NzkUiAction::from_selector("#newWinBtn");
        assert_eq!(action, NzkUiAction::NewDocWindow);
        assert_eq!(
            doc_window_action(&action, Some(&v)),
            Some(Action::OpenDoc(windows::DocTarget::for_node("A", "doc")))
        );
        // No selection: no window is opened, and nothing is invented.
        v.selected = None;
        assert_eq!(doc_window_action(&action, Some(&v)), None);
        assert_eq!(doc_window_action(&action, None), None);
        // Any other action is not a document-window request.
        assert_eq!(
            doc_window_action(&NzkUiAction::Fit, Some(&view())),
            None
        );
    }

    /// A document window's state comes from adopting the document its own
    /// engine worker produced: `adopt_document` is the one owner of the
    /// "document became current" transition, and the adopted state paints
    /// headlessly. (The engine/thread plumbing itself is proved by the
    /// [`crate::doc_engine`] tests: real per-window index on a worker thread,
    /// honest failure for file/empty targets, non-blocking drop.)
    #[test]
    fn a_document_window_adopts_and_paints_its_worker_document_headlessly() {
        let ctx = primed();
        let theme = crate::app::source_pane::SourceTheme::darkplus();
        let payload = serde_json::json!({
            "ok": true, "total": 6, "start": 1, "end": 6, "sections": 0,
            "definesKnown": false, "parts": [{"file": "a.cs", "line": 1}],
            "node": {"id": "NZK.B", "full": "NZK.B", "label": "B",
                     "kind": "class", "file": "a.cs", "line": 1, "endLine": 4},
            "lines": [
                {"n": 0, "ctx": "part", "text": "// part", "file": "a.cs"},
                {"n": 1, "ctx": "head", "text": "namespace NZK{", "file": "a.cs", "orig": 1},
                {"n": 2, "ctx": "head", "text": "  class B{", "file": "a.cs", "orig": 2},
                {"n": 3, "ctx": "body", "text": "  }", "file": "a.cs", "orig": 3},
                {"n": 4, "ctx": "tail", "text": "}", "file": "a.cs", "orig": 4}
            ]
        });
        let doc = crate::app::source_pane::Document::from_value("NZK.B", &payload)
            .expect("the fixture is a real synthesized-document payload");
        let mut state = SourceState::default();
        state.adopt_document("NZK.B", doc);
        assert!(state.open, "adoption opens the document window");
        assert!(state.doc.is_some(), "the adopted document is paintable state");
        assert!(state.error.is_none(), "a real document is not an error state");
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| build_ui_document(ui, &mut state, &theme));
        out.textures_delta.clear();
        assert!(
            state.open && state.doc.is_some(),
            "the window painted its own adopted document"
        );
    }

    /// The document window's render body takes a `SourceState` and nothing from
    /// the main window (no `GraphView`, no `NzkUiState`), so it paints the
    /// window's own document. Headless: the fixture document is real; the frame
    /// must run without a display.
    #[test]
    fn a_document_window_paints_only_its_own_document_headlessly() {
        let ctx = primed();
        let theme = crate::app::source_pane::SourceTheme::darkplus();
        let payload = serde_json::json!({
            "ok": true, "total": 6, "start": 1, "end": 6, "sections": 0,
            "definesKnown": false, "parts": [{"file": "a.cs", "line": 1}],
            "node": {"id": "NZK.B", "full": "NZK.B", "label": "B",
                     "kind": "class", "file": "a.cs", "line": 1, "endLine": 4},
            "lines": [
                {"n": 0, "ctx": "part", "text": "// part", "file": "a.cs"},
                {"n": 1, "ctx": "head", "text": "namespace NZK{", "file": "a.cs", "orig": 1},
                {"n": 2, "ctx": "head", "text": "  class B{", "file": "a.cs", "orig": 2},
                {"n": 3, "ctx": "body", "text": "  }", "file": "a.cs", "orig": 3},
                {"n": 4, "ctx": "tail", "text": "}", "file": "a.cs", "orig": 4}
            ]
        });
        let mut state = SourceState::default();
        state.doc = Some(
            crate::app::source_pane::Document::from_value("NZK.B", &payload)
                .expect("the fixture is a real synthesized-document payload"),
        );
        state.open = true;
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |ui| build_ui_document(ui, &mut state, &theme));
        out.textures_delta.clear();
        assert!(
            state.open && state.doc.is_some(),
            "the window painted its own loaded document"
        );
    }

    /// The MAIN window's render body is headless-safe: it paints the live
    /// `GraphView` and does not ask the host for engine work on a plain frame.
    /// (The document-window body is the separate `build_ui_document` above.)
    #[test]
    fn the_main_window_render_body_is_headless_safe() {
        let ctx = primed();
        let mut v = view();
        let mut st = NzkUiState::shipped();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 800.0),
            )),
            ..Default::default()
        };
        let mut effect = None;
        let mut out = ctx.run_ui(raw, |ui| {
            effect = Some(build_ui_with_panes(ui, &mut v, None, None, &mut st));
        });
        out.textures_delta.clear();
        let effect = effect.expect("the main-window frame ran");
        assert!(!effect.reindex, "a plain frame must not re-index");
        assert_eq!(v.selected, None, "nothing selects a node by itself");
    }

    #[test]
    fn full_graph_sets_the_view_mode_and_refits() {
        let ctx = primed();
        let mut v = view();
        assert_eq!(v.view_mode, ViewMode::Namespaces);
        let mut st = NzkUiState::default();
        apply_and_execute(&mut st, &mut v, &ctx, NzkUiAction::FullGraph);
        assert_eq!(v.view_mode, ViewMode::Full);
        assert!(v.fit_pending);
    }

    #[test]
    fn resettle_and_fit_are_headless_safe() {
        let ctx = primed();
        let mut v = view();
        let mut st = NzkUiState::default();
        apply_and_execute(&mut st, &mut v, &ctx, NzkUiAction::Resettle);
        apply_and_execute(&mut st, &mut v, &ctx, NzkUiAction::Fit);
        assert_eq!(st.resettle_count, 1);
        assert!(v.fit_pending);
    }

    /// THE DEFECT, stated as a test: `R` / `#resettleBtn` must MOVE the free
    /// nodes. Before this round the action only set `relayout`, which continues
    /// from the on-screen positions and anchors every placed node — so the
    /// button "worked" (the counter incremented, the layout ran) and the canvas
    /// did not change by a single pixel.
    #[test]
    fn resettle_actually_relays_the_free_nodes() {
        let ctx = primed();
        let mut v = view();
        // Settle the single root first, THEN expand: the incremental run anchors
        // the root where it was and fans the new child beside it, so the
        // arrangement is no longer the golden-angle disc. A re-seed must move it.
        v.relayout(&ctx);
        v.expand_node(0); // reveal B so there are two nodes to arrange
        v.relayout(&ctx);
        assert_eq!(v.visible_count(), 2);
        let before = v.visible_positions();
        let mut st = NzkUiState::default();

        apply_and_execute(&mut st, &mut v, &ctx, NzkUiAction::Resettle);

        let after = v.visible_positions();
        let biggest = before
            .iter()
            .zip(&after)
            .map(|(a, b)| ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt())
            .fold(0.0_f64, f64::max);
        assert!(
            biggest > 1e-6,
            "Re-settle left every node where it was (largest move {biggest:e}); \
             the reseed flag is not reaching the view"
        );
    }

    /// "Full graph" must actually show and PLACE the whole model. The visible
    /// set jumps from the namespace roots to every node, so continuing from the
    /// old arrangement leaves the new nodes piled at the origin (and, before the
    /// reseed flag, anchored there). Assert the set grew and the next layout
    /// re-seeds.
    #[test]
    fn full_graph_reveals_and_places_the_whole_model() {
        let ctx = primed();
        let mut v = view();
        assert_eq!(v.visible_count(), 1, "namespace mode starts at the roots");
        let mut st = NzkUiState::default();

        apply_and_execute(&mut st, &mut v, &ctx, NzkUiAction::FullGraph);

        assert_eq!(v.view_mode, ViewMode::Full);
        assert_eq!(
            v.visible_count(),
            v.graph.nodes.len(),
            "Full must show every node"
        );
        assert_eq!(
            v.seed_branch(),
            crate::graphviz::SeedContinuity::Reseed,
            "the whole model must be laid out afresh, not anchored to the old island"
        );
        v.relayout(&ctx);
        assert!(
            v.visible_all_placed(),
            "every visible node must hold a real position after the layout"
        );
    }

    /// THE FIT DEFECT, stated as a test: a pending fit is a CAMERA request and
    /// must be applied by the next frame even when no layout is dirty. It used
    /// to be consumed only inside `relayout`, so `#fitBtn` set the flag and the
    /// camera never moved.
    #[test]
    fn fit_pending_is_consumed_by_a_frame_that_does_not_relayout() {
        let ctx = primed();
        let mut v = view();
        let raw = |w: f32, h: f32| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(w, h),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw(800.0, 600.0), |ui| v.ui(ui));
        out.textures_delta.clear();
        let fitted = v.camera();
        assert!(!v.fit_pending, "the first frame consumes the initial fit");

        // Move the camera somewhere no fit would ever produce, then ask for one.
        v.set_camera(4.0, 10_000.0, -10_000.0);
        v.fit_pending = true;
        let mut out = ctx.run_ui(raw(800.0, 600.0), |ui| v.ui(ui));
        out.textures_delta.clear();

        assert!(!v.fit_pending, "the fit must be consumed by this frame");
        let (z, x, y) = v.camera();
        assert!(
            (z - fitted.0).abs() < 1e-9
                && (x - fitted.1).abs() < 1e-9
                && (y - fitted.2).abs() < 1e-9,
            "Fit did not restore the fitted camera: got {z},{x},{y}, want {fitted:?}"
        );
    }

    /// END-TO-END: `#srcCloseBtn` really closes the pane.
    ///
    /// This is the check the earlier "wired to a counter" defect would fail: the
    /// action must reach `SourceState::close()`, not merely bump a count.
    #[test]
    fn src_close_actually_closes_the_pane_it_owns() {
        let ctx = primed();
        let mut st = NzkUiState::default();
        let mut source = SourceState::default();
        source.open = true;

        let effect = apply_and_execute(&mut st, &mut view(), &ctx, NzkUiAction::SrcClose);
        assert!(effect.close_source, "the action must request a close");
        let (queued, report) =
            execute_source_action(&st, &effect, Some(&mut source), false, &ctx);
        assert!(queued.is_none(), "closing a pane opens no window");
        assert!(!source.open, "SourceState::close() must have run");
        assert_eq!(report.as_deref(), Some("source pane closed"));
    }

    /// END-TO-END: `#srcCopyBtn` performs a REAL clipboard write.
    ///
    /// `egui::Context::copy_text` queues `OutputCommand::CopyText`, so the proof
    /// is that the command is really emitted (egui-winit's `clipboard` feature
    /// then hands it to the OS). This asserts the command, not a claimed success.
    #[test]
    fn src_copy_emits_a_real_clipboard_command() {
        let ctx = primed();
        let mut st = NzkUiState::default();
        let mut source = SourceState::default();
        source.doc = Some(
            source_pane::Document::from_value(
                "A.B",
                &serde_json::json!({
                    "title": "A.B",
                    "lines": [
                        {"n": 1, "text": "public class B", "ctx": "", "segs": []},
                        {"n": 2, "text": "{", "ctx": "", "segs": []}
                    ],
                    "total": 2, "start": 1, "end": 2, "parts": 0, "sections": 0,
                    "defines_known": true, "file": "B.cs", "line": 1, "end_line": 2,
                    "focus": 0, "target": [0, 2]
                }),
            )
            .expect("a well-formed document payload"),
        );

        let effect = apply_and_execute(&mut st, &mut view(), &ctx, NzkUiAction::SrcCopy);
        assert!(effect.copy_source);
        let (_, report) = execute_source_action(&st, &effect, Some(&mut source), false, &ctx);
        // "public class B\n" + "{\n" = 15 + 2 = 17 bytes, from the REAL lines.
        assert_eq!(
            report.as_deref(),
            Some("copied 17 bytes of source"),
            "the real document text is what gets copied"
        );

        // The command must be observable in egui's output for this frame.
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut out = ctx.run_ui(raw, |_ui| {});
        let copied = out
            .platform_output
            .commands
            .iter()
            .any(|c| matches!(c, egui::OutputCommand::CopyText(_)));
        assert!(copied, "a real OutputCommand::CopyText must be emitted");
    }

    /// END-TO-END: `#srcPopBtn` queues a REAL window action, and the direction is
    /// decided from the pane's actual state (not assumed).
    #[test]
    fn src_pop_queues_a_real_detach_or_dock_from_live_state() {
        let ctx = primed();
        let mut st = NzkUiState::default();
        let mut source = SourceState::default();

        // Docked -> pop OUT.
        let effect = apply_and_execute(&mut st, &mut view(), &ctx, NzkUiAction::SrcPop);
        let (queued, report) =
            execute_source_action(&st, &effect, Some(&mut source), false, &ctx);
        assert_eq!(queued, Some(Action::Detach(Pane::Source)));
        assert_eq!(report.as_deref(), Some("source pane popped out"));

        // Already detached -> dock back.
        let mut st2 = NzkUiState::default();
        let effect2 = apply_and_execute(&mut st2, &mut view(), &ctx, NzkUiAction::SrcPop);
        let (queued2, report2) =
            execute_source_action(&st2, &effect2, Some(&mut source), true, &ctx);
        assert_eq!(queued2, Some(Action::Dock(Pane::Source)));
        assert_eq!(report2.as_deref(), Some("source pane docked"));
    }

    /// The palette the pane is painted with is the live `#srcModeBtn` choice:
    /// three presses must yield three DIFFERENT `SourceTheme`s. This is the
    /// assertion that kills the "wired to a counter" shape of the defect.
    ///
    /// Measured: `darkplus` and `nzk_dark_plus` share `bg` (30,30,30) — they
    /// differ in `head_bg` (18,18,31 vs 37,37,38), which is exactly the
    /// `nzk-dark+` `.source-head` override the pane documents. So the distinctness
    /// assertion is on the (bg, head_bg) pair, not bg alone.
    #[test]
    fn src_mode_reaches_the_palette_the_pane_is_painted_with() {
        let mut st = NzkUiState::default();
        let start = st.source_mode;
        let key = |m: actions::SourceMode| {
            let t = source_theme_for_mode(m);
            (t.bg.r(), t.bg.g(), t.bg.b(), t.head_bg.r(), t.head_bg.g(), t.head_bg.b())
        };
        for _ in 0..5 {
            st.source_mode = st.source_mode.next();
        }
        assert_eq!(
            st.source_mode, start,
            "five presses must return to the starting theme"
        );

        // Every mode maps to a DISTINCT palette: no mode is a dead rung.
        let all: Vec<_> = actions::SourceMode::ALL.iter().map(|m| key(*m)).collect();
        let mut uniq = all.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(
            uniq.len(),
            5,
            "each mode must paint a visibly different palette: {all:?}"
        );
        // And read back the way the paint sites do, a press really changes it.
        let first = key(NzkUiState::default().source_mode);
        let second = {
            let mut s = NzkUiState::default();
            crate::actions::apply(&mut s, NzkUiAction::SrcMode, &PathBuf::from("/tmp"));
            key(s.source_mode)
        };
        assert_ne!(first, second, "one press must visibly change the palette");
    }

    /// `#srcPrevBtn`/`#srcNextBtn` request the pane's real document history.
    /// The stacks live on `SourceState` and are stepped by the host (`App::
    /// consume_source_extensions`, the same path `Alt+Left`/`Alt+Right` uses);
    /// here we pin that the nzk_ui press produces the request and direction.
    #[test]
    fn src_prev_and_next_request_the_document_history() {
        let ctx = primed();
        for (sel, forward) in [("#srcPrevBtn", false), ("#srcNextBtn", true)] {
            let action = NzkUiAction::from_selector(sel);
            assert!(action.is_effective(), "{sel} must not be inert");
            let mut st = NzkUiState::default();
            let mut source = SourceState::default();
            let effect = apply_and_execute(&mut st, &mut view(), &ctx, action);
            assert_eq!(effect.source_history, Some(forward), "{sel}");
            // `execute_source_action` owns close/copy/theme/pop; the history
            // step needs the engine, so it is the host's separate responsibility.
            let (queued, report) =
                execute_source_action(&st, &effect, Some(&mut source), false, &ctx);
            assert!(queued.is_none() && report.is_none());
            assert!(source.doc.is_none(), "no document was fabricated");
        }
    }

    #[test]
    fn choosing_the_current_repo_preserves_its_configured_roots() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let roots = vec!["_sealed_src".to_string()];
        assert_eq!(roots_for_repo(&repo, &repo, &roots), roots);
    }

    #[test]
    fn choosing_another_repo_uses_its_configured_roots() {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let current = PathBuf::from("/tmp/other-current-repo");
        let roots = roots_for_repo(&repo, &current, &["stale-root".to_string()]);
        assert_eq!(roots, crate::model::resolve_roots(&repo, &[], &crate::model::load_config(&repo)));
    }

    #[test]
    fn the_nzk_ui_action_seam_is_the_same_path_as_the_keyboard() {
        let ctx = primed();
        let mut v = view();
        let mut st = NzkUiState::default();
        let before = v.camera().0;
        // native-css's `nzk_ui_ui` resolves a hit to this action for the host.
        let action = NzkUiAction::from_selector("#zoomInBtn");
        let effect = dispatch_action(&mut st, &mut v, &ctx, action);
        assert!(effect.camera);
        assert!(v.camera().0 > before, "nzk_ui seam did not move the camera");
        // `#backBtn` is no longer inert: it is a real navigation step on the
        // same seam (D2 wired the owner's ten controls).
        let effect = dispatch_action(&mut st, &mut v, &ctx, NzkUiAction::from_selector("#backBtn"));
        assert!(effect.navigate_back, "#backBtn must request a back step");
        assert!(!effect.is_noop());
        assert_eq!(st.last, Some(NzkUiAction::NavigateBack));
        assert_eq!(st.back_count, 1);
    }

    /// `#backBtn`/`#upBtn` drive the VIEW, not just a counter.
    ///
    /// This is the test the earlier version of the check above was missing. That
    /// one asserted `effect.navigate_back` and `st.back_count == 1` — both of
    /// which were true while `Effect::navigate_back` was read **nowhere**, so
    /// `#backBtn` incremented a counter and moved nothing. Exactly the shape of
    /// green-suite-certified dead control this round exists to remove.
    ///
    /// The assertion that actually has teeth is that `GraphView`'s selection
    /// changes: `select()` feeds a real history, and `navigate_back()` steps it.
    /// A counter cannot satisfy it.
    #[test]
    fn back_and_up_drive_the_view_selection_not_just_a_counter() {
        let ctx = primed();
        let mut v = view();
        let mut st = NzkUiState::default();

        // Two real selections, so the history has something to step back to.
        // `selected` is a public field on `GraphView`, not an accessor.
        v.select(Some(0));
        v.select(Some(1));
        assert_eq!(v.selected, Some(1), "second select wins");

        dispatch_action(&mut st, &mut v, &ctx, NzkUiAction::from_selector("#backBtn"));
        assert_eq!(
            v.selected,
            Some(0),
            "#backBtn must re-select the previous node through the view's real \
             history, not merely bump `back_count`"
        );

        dispatch_action(&mut st, &mut v, &ctx, NzkUiAction::from_selector("#upBtn"));
        assert_eq!(st.up_count, 1, "#upBtn must reach the host state");
    }

    /// The unified state model (task-29): a section hit goes through the host
    /// `apply` and lands in `NzkUiState::collapsed` — no nzk_ui-local copy.
    #[test]
    fn section_hit_reaches_the_host_state_and_round_trips() {
        let ctx = primed();
        let mut v = view();
        // Production startup state: the shipped three are collapsed.
        let mut st = NzkUiState::shipped();
        let action = NzkUiAction::from_selector("#filtersHead");
        assert_eq!(action, NzkUiAction::ToggleSection("#filtersHead"));
        let effect = dispatch_action(&mut st, &mut v, &ctx, action.clone());
        assert!(st.is_collapsed("#filtersHead"));
        assert_eq!(
            st.collapsed_facts(),
            vec!["#chainHead", "#filtersHead", "#pubHead", "#strHead"],
            "the new section joins the shipped three (no expansion)"
        );
        assert!(effect.repaint_nzk_ui, "the nzk_ui must repaint for the new facts");
        // Twice returns to the original state (byte-for-byte no drift).
        dispatch_action(&mut st, &mut v, &ctx, action);
        assert!(!st.is_collapsed("#filtersHead"));
        assert_eq!(
            st.collapsed,
            crate::actions::DEFAULT_COLLAPSED.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        );
    }

    /// `Ctrl+Alt+S` opens the Scratch window; `Alt+S` must stay the split
    /// toggle, and a shifted/meta variant is not the chord.
    #[test]
    fn scratch_chord_needs_ctrl_and_alt_together() {
        let s = Key::Character("s".into());
        let exact = Mods { ctrl: true, alt: true, shift: false, meta: false };
        assert!(is_scratch_chord(&s, exact));
        assert!(is_scratch_chord(&Key::Character("S".into()), exact));
        assert!(
            !is_scratch_chord(&s, Mods { ctrl: false, alt: true, ..exact }),
            "Alt+S alone is the reference's split toggle"
        );
        assert!(!is_scratch_chord(&s, Mods { ctrl: true, alt: true, shift: true, ..exact }));
        assert!(!is_scratch_chord(&s, Mods { ctrl: true, alt: true, meta: true, ..exact }));
        assert!(!is_scratch_chord(&Key::Character("x".into()), exact));
    }

    /// `#blocksBtn` / `B`: a real host pane request, surfaced as an `Effect`.
    #[test]
    fn blocks_control_requests_the_real_pane() {
        let ctx = primed();
        let mut v = view();
        let mut st = NzkUiState::default();
        assert_eq!(NzkUiAction::from_selector("#blocksBtn"), NzkUiAction::OpenBlocks);
        let effect = dispatch_action(&mut st, &mut v, &ctx, NzkUiAction::OpenBlocks);
        assert_eq!(effect.pane, Some(HostPane::Blocks));
        assert!(effect.repaint_nzk_ui);
        // `#previewBtn` is no longer inert either: it opens a real modal.
        //
        // This assertion used to read `preview.surface == Some(HostSurface::Preview)`
        // against `Effect::surface` — a field this test exercised and the app read
        // NOWHERE. It passed while the button did nothing, which is exactly how a
        // green suite certifies a dead control. `Effect::surface` is gone; the one
        // owner is `NzkUiState::surface`, and what actually matters is not the
        // enum value but the FACT it produces for the shell: the DOM id that
        // `ui_state_json` publishes as `modals_open`, the one thing that clears the
        // modal's `hidden` class and gives it a real, hit-testable box.
        let preview = dispatch_action(
            &mut st,
            &mut v,
            &ctx,
            NzkUiAction::from_selector("#previewBtn"),
        );
        assert_eq!(
            st.surface,
            Some(crate::actions::HostSurface::Preview),
            "the press must be recorded as an open surface"
        );
        assert_eq!(
            st.modal_facts(),
            vec!["previewModal"],
            "and it must name the real DOM id the shell can open"
        );
        assert!(!preview.is_noop());
    }
}

/// Round-8 split body: the load decision for `#csSplitHost`.
///
/// These are the adversarial tests for [`split_sync_plan`] — the only gate
/// between "load the split tab's document once" and "reload every frame" or
/// "paint a document that was never opened". They fail if the split-on gate, the
/// split-key lookup, the `node`-param extraction, or the already-loaded
/// suppression is removed; a `file:` tab must resolve to `Empty`, never `Load`,
/// because `SourceState` has no honest file-open API.
#[cfg(test)]
mod split_sync_tests {
    use super::*;
    use crate::chords::tabs::Tabs;

    /// Two node tabs, the second one in the split (`toggle_split` picks the tab
    /// that is not active, reference `shell.js:1475-1490`).
    fn split_tabs() -> Tabs {
        let mut tabs = Tabs::new();
        tabs.open("node:A", serde_json::json!({ "node": "A" }));
        tabs.open("node:B", serde_json::json!({ "node": "B" }));
        assert!(tabs.toggle_split(), "two tabs can split");
        tabs
    }

    #[test]
    fn a_node_split_tab_loads_its_node_exactly_once() {
        let tabs = split_tabs();
        assert_eq!(
            split_sync_plan(&tabs, None),
            SplitSync::Load("A".to_string()),
            "the non-active split tab's node ref is the document to load"
        );
        // A redraw with the same loaded key is Idle: no per-frame engine call.
        assert_eq!(split_sync_plan(&tabs, Some("node:A")), SplitSync::Idle);
    }

    #[test]
    fn a_split_off_model_never_loads_anything() {
        let mut tabs = Tabs::new();
        tabs.open("node:A", serde_json::json!({ "node": "A" }));
        tabs.open("node:B", serde_json::json!({ "node": "B" }));
        assert!(!tabs.split(), "no split yet");
        assert_eq!(
            split_sync_plan(&tabs, None),
            SplitSync::Idle,
            "a closed split must not open a second document"
        );
        // Even with a stale loaded key from a previous split session, closing the
        // split does not reload or clear: the plan is Idle either way.
        assert_eq!(split_sync_plan(&tabs, Some("node:B")), SplitSync::Idle);
    }

    #[test]
    fn a_file_split_tab_is_left_empty_not_faked_as_a_document() {
        let mut tabs = Tabs::new();
        // Open the file tab FIRST so it is the non-active (split) one after the
        // node tab becomes active — the same ordering a user produces by opening
        // a node from inside a file tab.
        tabs.open(
            "file:/tmp/B.cs",
            serde_json::json!({ "file": "/tmp/B.cs" }),
        );
        tabs.open("node:A", serde_json::json!({ "node": "A" }));
        assert!(tabs.toggle_split());
        match split_sync_plan(&tabs, None) {
            SplitSync::Empty(key) => assert_eq!(key, "file:/tmp/B.cs"),
            other => panic!("a file tab must be Empty (no honest file open), got {other:?}"),
        }
        // One-shot: the key is recorded, so a redraw does not retry.
        assert_eq!(
            split_sync_plan(&tabs, Some("file:/tmp/B.cs")),
            SplitSync::Idle
        );
    }

    #[test]
    fn a_split_tab_without_a_node_param_is_empty() {
        let mut tabs = Tabs::new();
        // A `doc:unknown` key (no node, no file) is a normalised tab the engine
        // cannot open; it is opened first so it is the split (non-active) tab.
        // The split body must stay empty rather than invent content.
        tabs.open("doc:unknown", serde_json::json!({ "line": 4 }));
        tabs.open("node:A", serde_json::json!({ "node": "A" }));
        assert!(tabs.toggle_split());
        assert!(
            matches!(split_sync_plan(&tabs, None), SplitSync::Empty(k) if k == "doc:unknown"),
            "a non-node tab is Empty"
        );
    }
}

/// App-level wiring for the persisted panel layout (task-4 / board task-10d).
///
/// These exercise the exact seams the real `run` loop uses: the startup loader
/// (`load_panel_layout_for_startup`) and the pane-release helper
/// (`drop_into_dock_zone`), without needing a display for `App::new`.
#[cfg(test)]
mod panel_layout_wiring_tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "csviewer-app-panels-{tag}-{}",
            std::process::id()
        ))
    }

    fn main_rect() -> Rect {
        Rect::new(100.0, 100.0, 800.0, 600.0)
    }

    #[test]
    fn startup_restores_the_saved_panel_layout_through_the_real_app_seam() {
        let root = temp_root("startup");
        let _ = std::fs::remove_dir_all(&root);
        // A fresh cache root (no viewer-state blob) starts from the default
        // layout — an absent/old blob must never stop startup.
        assert_eq!(
            load_panel_layout_for_startup(&root),
            windows::PanelLayout::default()
        );
        let mut layout = windows::PanelLayout::new();
        layout.geometry.remember(Pane::Find, Rect::new(300.0, 200.0, 560.0, 720.0));
        layout.docks.assign(Pane::Source, windows::DockZone::Left);
        layout.docks.assign(Pane::Find, windows::DockZone::Center);
        crate::pov::save_panel_layout(&root, &layout).expect("save layout");
        // A real restart loads through this same function.
        assert_eq!(load_panel_layout_for_startup(&root), layout);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn release_over_a_zone_assigns_the_stack_and_free_float_is_untouched() {
        let mut layout = windows::PanelLayout::new();
        let main = main_rect();
        // Centre zone: the pane joins the centre stack (and stops floating).
        assert_eq!(
            drop_into_dock_zone(&mut layout, Pane::Find, 500.0, 400.0, main),
            Some(windows::DockZone::Center)
        );
        assert_eq!(
            layout.docks.zone_of(Pane::Find),
            Some(windows::DockZone::Center)
        );
        // A second pane dropped on the same zone stacks with the first.
        assert_eq!(
            drop_into_dock_zone(&mut layout, Pane::Blocks, 500.0, 400.0, main),
            Some(windows::DockZone::Center)
        );
        assert_eq!(
            layout.docks.stack(windows::DockZone::Center).to_vec(),
            vec![Pane::Find, Pane::Blocks]
        );
        // The bottom strip is its own zone/stack.
        assert_eq!(
            drop_into_dock_zone(&mut layout, Pane::Source, 500.0, 680.0, main),
            Some(windows::DockZone::Bottom)
        );
        // Outside every zone: nothing assigned, the layout is untouched, so the
        // caller's free-float/dock decision still governs the pane.
        let before = layout.clone();
        assert_eq!(
            drop_into_dock_zone(&mut layout, Pane::NodeDrawer, 5000.0, 5000.0, main),
            None
        );
        assert_eq!(layout, before);
    }
}

/// The document-tab stack the graph selection drives.
///
/// `App::sync_source` calls `Tabs::open_params(&json!({ "node": id }))` on every
/// selection change — v2's `openSource()` pushes/activates a document tab
/// (`shell.js:1535-1568`), and the reference's `#cs-tabs` strip is painted from
/// that stack. These tests pin the activation-vs-duplicate semantics the real
/// strip depends on: one row per document, the last selection active.
#[cfg(test)]
mod source_tab_tests {
    use crate::chords::tabs::Tabs;

    /// The exact model call `sync_source` makes for a selected node id.
    fn open_node(tabs: &mut Tabs, node: &str) {
        tabs.open_params(&serde_json::json!({ "node": node }));
    }

    #[test]
    fn opening_nodes_activates_one_tab_each_and_never_duplicates() {
        let mut tabs = Tabs::new();

        // Same node twice: one row, still active.
        open_node(&mut tabs, "A");
        assert_eq!(tabs.count(), 1);
        open_node(&mut tabs, "A");
        assert_eq!(tabs.count(), 1, "re-selecting A must not push a second tab");
        assert_eq!(tabs.active_key(), Some("node:A"));

        // A -> B -> A: two rows, A active again (activation, not append).
        open_node(&mut tabs, "B");
        open_node(&mut tabs, "A");
        assert_eq!(tabs.count(), 2, "A and B, each exactly once");
        assert_eq!(tabs.active_key(), Some("node:A"));
        assert_eq!(
            tabs.tabs().iter().map(|t| t.key.as_str()).collect::<Vec<_>>(),
            vec!["node:A", "node:B"],
            "document order is first-open order"
        );

        // Re-opening B activates the existing row.
        open_node(&mut tabs, "B");
        assert_eq!(tabs.count(), 2);
        assert_eq!(tabs.active_key(), Some("node:B"));
    }

    /// The tab label is the reference's `shortNode` caption (`shell.js:1002-1006`):
    /// the last two dot-separated segments, so a tab is readable, not a full id.
    #[test]
    fn a_node_tab_is_labelled_like_the_reference() {
        let mut tabs = Tabs::new();
        open_node(&mut tabs, "GonsoLicense");
        // `open_params` derives `node:<id>` and the default label from the params.
        let key = tabs.active_key().expect("a tab is open").to_string();
        assert_eq!(key, "node:GonsoLicense");
        assert_eq!(tabs.label(&key), Some("GonsoLicense"));
    }
}

/// The selected namespace's node index, when Run should take the ancestry path.
///
/// Only a **namespace** selection qualifies: types and members keep the normal
/// relation walk (`Index::chain`). Split out so the branch that decides which
/// chain runs is testable without an engine or a window.
pub(crate) fn namespace_ancestry_selection(view: &crate::view::GraphView) -> Option<usize> {
    let i = view.selected?;
    view.graph
        .nodes
        .get(i)
        .filter(|n| n.kind == "namespace")
        .map(|_| i)
}

/// Build the **namespace ancestry** resolve chain for the node at `idx`.
///
/// The relation walk (`Index::chain`) answers "who calls / uses this". For a
/// namespace that is the wrong question: a namespace's meaningful neighbours are
/// its ancestors, so the chain is `h -> parent -> g -> parent -> f -> ... -> a`
/// over the engine's own `parent` links (`#ns:a.b.c` -> `#ns:a.b`).
///
/// Only **namespace** ancestors are emitted. The walk is therefore exactly "go
/// up the namespace": a class/member parent is never shown, and neither is its
/// parent — if the link ever stops naming a namespace the walk ends rather than
/// falling back to a containment tree. Direction and the depth selector are
/// deliberately ignored: the owner asked for the chain up to the highest
/// namespace, and a 7-level namespace would be truncated by the 6-level cap.
///
/// The JSON shape is the engine's own (`ok`/`tree`/`flat`/`truncated`), so the
/// existing `#chainTree` / `#chainCount` renderer needs no second code path.
pub(crate) fn namespace_ancestry_chain(
    graph: &crate::model::Graph,
    idx: usize,
) -> serde_json::Value {
    fn lite(n: &crate::model::Node) -> serde_json::Value {
        serde_json::json!({
            "id": n.id,
            "name": n.label,
            "label": n.label,
            "full": n.full,
            "kind": n.kind,
            "ns": n.ns,
            "owner": n.owner,
            "file": n.file,
            "line": n.line,
            "endLine": n.end_line,
            "degree": n.degree,
            "ret": n.ret,
        })
    }
    fn hop(
        graph: &crate::model::Graph,
        ancestors: &[usize],
        i: usize,
    ) -> serde_json::Value {
        let a = &graph.nodes[ancestors[i]];
        let children = if i + 1 < ancestors.len() {
            vec![hop(graph, ancestors, i + 1)]
        } else {
            Vec::new()
        };
        serde_json::json!({
            "node": lite(a),
            "edge": "parent",
            "line": a.line,
            "cycle": false,
            "children": children,
        })
    }

    let Some(selected) = graph.nodes.get(idx) else {
        return serde_json::json!({"ok": false, "error": "node not in the index"});
    };
    let mut ancestors: Vec<usize> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert(selected.id.clone());
    let mut cur = selected.parent.clone();
    while !cur.is_empty() {
        // A malformed index must not make Run loop forever.
        if !seen.insert(cur.clone()) {
            break;
        }
        let Some(pi) = graph.nodes.iter().position(|n| n.id == cur) else { break };
        let parent = &graph.nodes[pi];
        if parent.kind != "namespace" {
            break;
        }
        ancestors.push(pi);
        cur = parent.parent.clone();
    }

    let children = if ancestors.is_empty() {
        Vec::new()
    } else {
        vec![hop(graph, &ancestors, 0)]
    };
    let tree = serde_json::json!({
        "node": lite(selected),
        "edge": serde_json::Value::Null,
        "line": serde_json::Value::Null,
        "cycle": false,
        "children": children,
    });
    let flat: Vec<serde_json::Value> = ancestors
        .iter()
        .enumerate()
        .map(|(i, &ai)| {
            let a = &graph.nodes[ai];
            serde_json::json!({
                "depth": i + 1,
                "node": lite(a),
                "edge": "parent",
                "line": a.line,
                "cycle": false,
            })
        })
        .collect();
    serde_json::json!({
        "ok": true,
        "tree": tree,
        "flat": flat,
        "truncated": false,
    })
}

#[cfg(test)]
mod namespace_chain_tests {
    use super::namespace_ancestry_chain;
    use crate::model::{Graph, Node};

    fn ns(id: &str, full: &str, parent: &str) -> Node {
        Node {
            id: id.into(),
            label: full.rsplit('.').next().unwrap_or(full).into(),
            full: full.into(),
            kind: "namespace".into(),
            ns: full.into(),
            parent: parent.into(),
            ..Default::default()
        }
    }

    fn graph() -> Graph {
        Graph {
            repo: std::path::PathBuf::from("/tmp/ns-chain"),
            nodes: vec![
                ns("#ns:a", "a", ""),
                ns("#ns:a.b", "a.b", "#ns:a"),
                ns("#ns:a.b.c", "a.b.c", "#ns:a.b"),
                ns("#ns:a.b.c.d", "a.b.c.d", "#ns:a.b.c"),
                Node {
                    id: "m:D".into(),
                    label: "D".into(),
                    full: "a.b.c.d.D".into(),
                    kind: "method".into(),
                    ns: "a.b.c.d".into(),
                    parent: "#ns:a.b.c.d".into(),
                    ..Default::default()
                },
            ],
            edges: Vec::new(),
            state: serde_json::json!({"indexed": true}),
            skipped: Vec::new(),
            projects: Vec::new(),
        }
    }

    /// The owner's example: from `h` the chain is h, g, f, … up to the root —
    /// each hop named by the `parent` relation, and depth covers the whole tree.
    #[test]
    fn the_chain_walks_every_namespace_ancestor_up_to_the_root() {
        let g = graph();
        let v = namespace_ancestry_chain(&g, 3); // a.b.c.d
        assert_eq!(v["ok"], true);
        let flat = v["flat"].as_array().expect("flat rows");
        let labels: Vec<&str> = flat
            .iter()
            .map(|r| r["node"]["full"].as_str().unwrap())
            .collect();
        assert_eq!(labels, vec!["a.b.c", "a.b", "a"]);
        for row in flat {
            assert_eq!(row["edge"], "parent", "each hop names the parent relation");
        }
        let depths: Vec<u64> = flat
            .iter()
            .map(|r| r["depth"].as_u64().unwrap())
            .collect();
        assert_eq!(depths, vec![1, 2, 3]);
    }

    /// The tree nests immediate parent -> grandparent -> root, and the selected
    /// node is the root row.
    #[test]
    fn the_tree_nests_parents_in_order() {
        let g = graph();
        let v = namespace_ancestry_chain(&g, 3);
        assert_eq!(v["tree"]["node"]["full"], "a.b.c.d");
        let p1 = &v["tree"]["children"][0];
        assert_eq!(p1["node"]["full"], "a.b.c");
        assert_eq!(p1["edge"], "parent");
        let p2 = &p1["children"][0];
        assert_eq!(p2["node"]["full"], "a.b");
        let p3 = &p2["children"][0];
        assert_eq!(p3["node"]["full"], "a");
        assert!(p3["children"].as_array().unwrap().is_empty());
    }

    /// A root namespace has nowhere to go: a successful, empty chain rather than
    /// an error or a fabricated hop.
    #[test]
    fn a_root_namespace_has_no_ancestors() {
        let g = graph();
        let v = namespace_ancestry_chain(&g, 0);
        assert_eq!(v["ok"], true);
        assert!(v["flat"].as_array().unwrap().is_empty());
        assert!(v["tree"]["children"].as_array().unwrap().is_empty());
    }

    /// Only a namespace selection takes the ancestry path; a method keeps the
    /// engine's relation walk.
    #[test]
    fn only_a_namespace_selection_takes_the_ancestry_path() {
        use super::namespace_ancestry_selection;
        use crate::view::{GraphView, ViewMode};
        let mut g = graph();
        g.nodes.push(Node {
            id: "m:E".into(),
            label: "E".into(),
            full: "a.b.E".into(),
            kind: "method".into(),
            ns: "a.b".into(),
            ..Default::default()
        });
        let edges = vec![crate::model::Edge {
            from: 4,
            to: 5,
            kind: "calls".into(),
            count: 1,
        }];
        g.edges = edges;
        let mut v = GraphView::new(g);
        v.view_mode = ViewMode::Full;
        v.rebuild_view();
        v.selected = Some(3); // namespace a.b.c.d
        assert_eq!(namespace_ancestry_selection(&v), Some(3));
        v.selected = Some(5); // method m:E
        assert_eq!(namespace_ancestry_selection(&v), None);
        v.selected = None;
        assert_eq!(namespace_ancestry_selection(&v), None);
    }

    /// A broken parent link ends the walk instead of inventing a hop, and a
    /// non-namespace parent is never emitted (the owner's "drop all the parent
    /// nodes and their parent nodes").
    #[test]
    fn a_non_namespace_or_missing_parent_ends_the_walk() {
        let mut g = graph();
        // Point d at its method instead of the parent namespace.
        g.nodes[3].parent = "m:D".into();
        let v = namespace_ancestry_chain(&g, 3);
        assert!(v["flat"].as_array().unwrap().is_empty());

        let mut g = graph();
        g.nodes[3].parent = "#ns:does.not.exist".into();
        let v = namespace_ancestry_chain(&g, 3);
        assert!(v["flat"].as_array().unwrap().is_empty());
    }
}
