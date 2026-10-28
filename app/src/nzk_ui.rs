//! The nzk_ui nzk_ui layer.
//!
//! **v3: the web GUI owns interaction and state.** `nzk_ui` (task-11, a
//! separate crate at `native-csviewer/nzk_ui/`) parses the real
//! `frontend/index.html` + `static/style.css` with Blitz/Stylo/Taffy/Parley and
//! paints one RGBA frame with Vello-CPU. It is a pure rasteriser on purpose: the
//! *host* holds the authoritative UI state and rebuilds the nzk_ui from it, and
//! it resolves a pointer hit to a control by the DOM `#id`/class that
//! `nzk_ui` lays out — never by a node id. Native widgets are now only for
//! controls the host has not rerouted yet, and every such control is listed as
//! unwired rather than faked. So this module:
//!
//! 1. runs it as a child process — **once per (repo, size)**, never per frame —
//!    producing the nzk_ui pixels (`--out`) and the resolved border-box
//!    geometry (`--boxes`);
//! 2. uploads the pixels as one egui texture and paints them under the UI;
//! 3. clips the native graph to `#graph` from the nzk_ui geometry, so the
//!    real CSS decides where the canvas is;
//! 4. overlays native widgets only for controls that actually have behaviour.
//!    Every other control is *drawn by the static nzk_ui* and is not
//!    interactive yet (listed in the task-18 report).
//!
//! Running nzk_ui as a child keeps Blitz/Stylo/Vello out of this crate's
//! dependency tree and out of its vendored set; the price is that the
//! `nzk_ui` binary must ship beside `native-csviewer`. `CSVIEWER_NO_NZK_UI=1`
//! forces the old hand-ported egui UI (legacy: `CSVIEWER_NO_CHROME`), and
//! `CSVIEWER_NZK_UI_BIN` overrides the binary path (legacy:
//! `CSVIEWER_CSS_SHELL_BIN`).

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use egui::{
    Color32, ColorImage, CursorIcon, Pos2, Rect, Stroke, StrokeKind, TextureHandle,
    TextureOptions, UiBuilder,
};

use crate::actions::{NzkUiAction, NzkUiState, PickerAction, SliderEdit, SliderKind, SliderPhase};
use crate::chords::tabs::Tabs;
use crate::app::actions;
use crate::app::nzk_ui_client::NzkUiClient;
use crate::model::Graph;
use crate::view::GraphView;

/// Preferred environment override for the nzk_ui executable.
pub const BIN_ENV: &str = "CSVIEWER_NZK_UI_BIN";
/// Legacy override, still honoured when the preferred name is unset.
pub const BIN_ENV_LEGACY: &str = "CSVIEWER_CSS_SHELL_BIN";

/// The binary named by the environment: `CSVIEWER_NZK_UI_BIN` wins, then the
/// legacy `CSVIEWER_CSS_SHELL_BIN`. Empty values count as unset.
pub fn env_bin() -> Option<std::ffi::OsString> {
    pick_bin_env(
        std::env::var_os(BIN_ENV),
        std::env::var_os(BIN_ENV_LEGACY),
    )
}

/// Pure precedence rule behind [`env_bin`] (unit-tested without touching env).
fn pick_bin_env(
    nzk_ui: Option<std::ffi::OsString>,
    legacy: Option<std::ffi::OsString>,
) -> Option<std::ffi::OsString> {
    nzk_ui
        .filter(|v| !v.is_empty())
        .or_else(|| legacy.filter(|v| !v.is_empty()))
}

#[cfg(test)]
mod env_tests {
    use super::pick_bin_env;
    use std::ffi::OsString;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    /// `CSVIEWER_NZK_UI_BIN` wins; the legacy `CSVIEWER_CSS_SHELL_BIN` is the
    /// fallback, and an empty value counts as unset.
    #[test]
    fn nzk_ui_env_wins_and_legacy_still_works() {
        assert_eq!(pick_bin_env(os("/new"), os("/old")), os("/new"));
        assert_eq!(pick_bin_env(None, os("/old")), os("/old"));
        assert_eq!(pick_bin_env(os(""), os("/old")), os("/old"));
        assert_eq!(pick_bin_env(os(""), os("")), None);
        assert_eq!(pick_bin_env(None, None), None);
    }
}

/// The geometry of one nzk_ui frame, **shared by paint and hit-test**.
///
/// This is the D2 single source of truth: `nzk_ui_ui` resolves pointer hits
/// through the very same map `paint_in` uploaded as the background texture, so
/// the click path cannot consult a different layout than the one that painted
/// the pixels. Before this, `overlay()` hand-placed an invisible `egui::Button`
/// per control from `--boxes` while egui owned the actual input, so the click
/// path ran through egui's rect and nothing noticed when it drifted.
#[derive(Clone, Debug)]
pub struct FrameGeometry {
    /// nzk_ui `--boxes` selector -> border box, in points at
    /// `pixels_per_point` 1.0 (the size the frame was painted at).
    pub boxes: BTreeMap<String, Rect>,
    /// The (width, height) the frame was painted at.
    pub size: (u32, u32),
    /// The paint↔geometry correspondence token from the renderer: a digest over
    /// the exact RGBA8 bytes painted for this same layout pass. `None` when the
    /// renderer supplied no token (an older nzk_ui), which means "cannot
    /// prove correspondence" — never "they correspond".
    pub paint_digest: Option<String>,
    /// Set once a token comparison has actually been performed and failed.
    /// While set, [`hit_selector`] resolves nothing rather than routing a click
    /// through geometry that provably does not match the pixels on screen.
    pub drift: Option<String>,
}

impl FrameGeometry {
    /// A geometry with no token: usable for paint, but correspondence between
    /// the boxes and the painted PNG is unproven.
    pub fn unverified(boxes: BTreeMap<String, Rect>, size: (u32, u32)) -> FrameGeometry {
        FrameGeometry { boxes, size, paint_digest: None, drift: None }
    }

    /// True when the renderer supplied a token AND it matched the painted
    /// bytes. Only then is hit-testing provably against the painted layout.
    pub fn correspondence_proven(&self) -> bool {
        self.paint_digest.is_some() && self.drift.is_none()
    }
}

/// One painted nzk_ui frame plus the geometry that produced it.
struct NzkUi {
    tex: TextureHandle,
    size: (u32, u32),
    /// The exact status-line text this frame was painted with. A change means
    /// the corpus counts changed, so the frame must be rebuilt.
    status: String,
    /// What this frame was rendered from: the host's collapsed set plus the
    /// presentation state. A change means the nzk_ui must be rebuilt.
    collapsed: Vec<String>,
    pane_open: bool,
    side_scroll: f64,
    /// The sidebar's engine-clamped maximum scroll offset, from `@scroll`.
    side_scroll_max: f64,
    /// The canvas notice's node count this frame was painted with. v2's
    /// `viewNodes` (`shell.js updateCanvasNotice`) is the number of nodes the
    /// canvas actually draws, so a change in the visible set must repaint the
    /// nzk_ui even when the status line and collapsed list are unchanged.
    notice_nodes: usize,
    /// A digest of the **whole facts document** this frame was rendered from.
    ///
    /// The hand-listed fields above are a stale key, and a stale key with a
    /// missing field is exactly how a control can change host state and never
    /// repaint. The sidebar filter model (`ui.filters`: chip on/off, namespace
    /// value, `ns_deep`), the Layout readouts (`settings`), the open overlay
    /// (`ui.drawers_open`/`ui.modals_open`), the Find rows and the tab bar all
    /// travel through the facts — none of them were in the key, so a press could
    /// mutate state and leave the painted nzk_ui untouched.
    ///
    /// Digesting the facts is the honest fix: anything that reaches the
    /// rasteriser is part of the repaint key by construction, so a future fact
    /// cannot be silently left out. It stays a *key*, not a repaint trigger:
    /// an unchanged document hashes the same and still returns early (no idle
    /// repaint storm).
    facts_digest: u64,
    /// nzk_ui `--boxes` selector -> border box, in points at pixels_per_point
    /// 1.0 (the size the frame was painted at).
    boxes: BTreeMap<String, Rect>,
    /// The renderer's paint↔geometry correspondence token for this frame, when
    /// it supplied one (`@frame` row in the boxes TSV). `None` = unverifiable.
    paint_digest: Option<String>,
    /// Non-`None` once the boxes have been proven not to match the painted
    /// pixels. While set, hit-testing refuses to answer.
    drift: Option<String>,
    build_ms: u128,
    bin: PathBuf,
}

thread_local! {
    static NZK_UI: RefCell<Option<NzkUi>> = const { RefCell::new(None) };
    static WARNED: Cell<bool> = const { Cell::new(false) };
    /// NzkUi-**presentation** state only: things the stylesheet renders that
    /// are not host action state. The collapsed-section list is deliberately NOT
    /// here — `actions::NzkUiState::collapsed` is its one owner (task-26/29)
    /// and the nzk_ui renders from it.
    static PRESENT: RefCell<Presentation> = RefCell::new(Presentation::default());
    /// Last interactive canvas rect, so the diagnostic below logs only changes.
    static CANVAS_LOG: RefCell<Option<Rect>> = const { RefCell::new(None) };

}

/// NzkUi presentation state that is not host action state.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Presentation {
    /// The source pane is open (derived from the rect the app gives `paint_in`).
    pub pane_open: bool,
    /// Sidebar scroll offset in CSS px, clamped by the engine's own scroll
    /// container (`.side { overflow-y: auto }`).
    pub side_scroll: f64,
    /// The sidebar's maximum scroll offset, as clamped by the engine.
    pub side_scroll_max: f64,
}

/// Who owns a control's behaviour. A **registry**, not an action enum:
/// behavioural controls name a single `actions::NzkUiAction` variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlOwner {
    /// `actions::NzkUiAction::ToggleSection` — the host owns the collapsed
    /// state; a click is returned for `app.rs` to apply and repaint.
    Section,
    /// nzk_ui.rs — toggles immediate-mode view state (`GraphView::fit_pending`).
    Local,
    /// The host action layer: `nzk_ui_ui` returns the action, `app.rs` applies.
    Host,
    /// The **picker** action layer: `picker_action` returns an
    /// `actions::PickerAction`, which `app.rs` applies through
    /// `actions::apply_picker`.
    ///
    /// A distinct owner rather than `Host` because the resolution function is
    /// different, and the anti-drift test's contract is different too: a `Host`
    /// control must answer `NzkUiAction::from_selector` with something
    /// effective, while a `Picker` control must answer `PickerAction::
    /// from_selector` with `Some`. Collapsing the two would let one family's
    /// resolver silently stop answering for the other (which is exactly the
    /// `#blocksBtn` split the test was written for).
    Picker,
    /// Drawn by the stylesheet with no defined behaviour here. Never faked.
    Inert,
    /// A Layout ▸ range input. Its press **carries a value**, so a click route
    /// cannot answer it — [`route_drag`] does, and returns a
    /// [`crate::app::actions::SliderEdit`] rather than a `NzkUiAction`.
    ///
    /// A distinct owner rather than `Host` for the same reason [`Self::Picker`]
    /// is: the resolution function differs, and so does the anti-drift
    /// contract. A `Host` entry must answer `from_selector`; a slider must
    /// answer [`crate::app::actions::SliderKind::from_selector`], and its value
    /// comes from geometry this table does not hold.
    Slider,
}

/// One dispatch-table entry, **keyed by the DOM selector nzk_ui lays out**.
/// Never by a node id: node ids are not stable across a rebuild.
pub struct Control {
    pub selector: &'static str,
    pub owner: ControlOwner,
    pub label: &'static str,
}

/// Every control the nzk_ui paints, with its owner. Unwired controls are listed
/// as `Inert` rather than silently faked.
pub const CONTROLS: &[Control] = &[
    // -- host-owned section state (actions::NzkUiState::collapsed) ----------
    Control { selector: "#filtersHead", owner: ControlOwner::Section, label: "Filters section" },
    Control { selector: "#pubHead", owner: ControlOwner::Section, label: "Public definitions section" },
    Control { selector: "#strHead", owner: ControlOwner::Section, label: "Strings section" },
    Control { selector: "#chainHead", owner: ControlOwner::Section, label: "Resolve chain section" },
    Control { selector: "#problemsHead", owner: ControlOwner::Section, label: "Parser warnings section" },
    Control { selector: "#relationsHead", owner: ControlOwner::Section, label: "Relations section" },
    // -- host actions (app.rs executes via actions::apply) -------------------
    Control { selector: "#zoomInBtn", owner: ControlOwner::Host, label: "Zoom in" },
    Control { selector: "#zoomOutBtn", owner: ControlOwner::Host, label: "Zoom out" },
    Control { selector: "#wholeGraphBtn", owner: ControlOwner::Host, label: "Full graph" },
    Control { selector: "#resettleBtn", owner: ControlOwner::Host, label: "Re-settle" },
    Control { selector: "#indexBtn", owner: ControlOwner::Host, label: "Index" },
    Control { selector: "#blocksBtn", owner: ControlOwner::Host, label: "Blocks" },
    Control { selector: "#blocksClose", owner: ControlOwner::Host, label: "Close Blocks" },
    // The owner named these ten explicitly, so each has a real `NzkUiAction`
    // in `actions.rs` rather than hover feedback.
    Control { selector: "#backBtn", owner: ControlOwner::Host, label: "Back" },
    Control { selector: "#upBtn", owner: ControlOwner::Host, label: "Up" },
    Control { selector: "#modeGraph", owner: ControlOwner::Host, label: "Graph mode tab" },
    Control { selector: "#modeFind", owner: ControlOwner::Host, label: "Find mode tab" },
    Control { selector: "#browseBtn", owner: ControlOwner::Host, label: "Browse for a folder" },
    Control { selector: "#exportBtn", owner: ControlOwner::Host, label: "Export" },
    Control { selector: "#editBtn", owner: ControlOwner::Host, label: "Edit" },
    Control { selector: "#previewBtn", owner: ControlOwner::Host, label: "Preview" },
    Control { selector: "#shortcutsBtn", owner: ControlOwner::Host, label: "Shortcuts / settings" },
    // `⧉ New window` is `#srcPopBtn` in the real DOM (`index.html:378`); the
    // old `#newWinBtn` id from `windows.js`'s prose is not an element the shell
    // lays out, so registering it would be a control with no geometry. The
    // `NzkUiAction::NewDocWindow` arm is still reachable by selector through
    // the control channel, but the visible control is the source pane's pop-out.
    Control { selector: "#rootInput", owner: ControlOwner::Host, label: "Root folder path" },
    Control { selector: "#searchInput", owner: ControlOwner::Host, label: "Search symbols" },
    Control { selector: "#nsSelect", owner: ControlOwner::Host, label: "Namespace select" },
    Control { selector: "#fitBtn", owner: ControlOwner::Local, label: "Fit" },
    // -- the overlay DRAWER family (host-owned open/close state) --------------
    // A drawer's box is non-zero only while it is OPEN, and `drawers_open` is
    // what opens it. The CONTAINERS are registered for geometry, but
    // [`route_click`] refuses to turn them into actions: the reference scripts
    // attach no click handler to `#nodeDrawer`/`#findPanel`/`#blocksPanel`
    // themselves (`shell.js:562-568`, `search.js:656`, `scratch.js:553`), so a
    // press on the panel body does nothing. Only the explicit close buttons
    // toggle. Treating the container as a click target made every press on the
    // drawer's own content close the drawer.
    //
    // `#nodeCodeBtn`, `#findResults`, `#blocksScroll` and the rest of the drawer
    // internals stay Inert: they are only laid out once a drawer is open, and
    // the host has no defined behaviour for them yet.
    Control { selector: "#nodeDrawer", owner: ControlOwner::Host, label: "Node drawer container" },
    Control { selector: "#nodeDrawerClose", owner: ControlOwner::Host, label: "Close node drawer" },
    Control { selector: "#findPanel", owner: ControlOwner::Host, label: "Find panel container" },
    Control { selector: "#findClose", owner: ControlOwner::Host, label: "Close Find" },
    Control { selector: "#blocksPanel", owner: ControlOwner::Host, label: "Blocks panel container" },
    // -- drawn, no defined behaviour (never faked) ---------------------------
    // Panels and containers the stylesheet draws but the host has no behaviour
    // for. They are registered so the classification is explicit and countable,
    // and they stay `Inert`: a hit returns no action, exactly as before.
    //
    // The modal-backdrop family (`#reorgModal`, `#editModal`, `#previewModal`,
    // `#shortcutsModal`, `#modal`) is split across two owners on purpose, and
    // the split is the whole point:
    //
    // * The **backdrop** stays `Inert` in this selector registry. It is
    //   `position: fixed; inset: 0` and therefore covers the viewport once open.
    //   `route_click` handles only the exact shell-target backdrop press as a
    //   close; presses whose chain starts inside the card remain card presses.
    // * The **buttons inside it** are `Host`, because they are real controls
    //   with real behaviour (close/done) and they only have a non-zero box
    //   while their modal is open.
    //
    // An earlier revision of this comment claimed the family "cannot be
    // otherwise" because forcing one visible "lays it out at a position a real
    // opened modal would never take". **That measurement does not hold** — it
    // was re-measured this round and all five modals lay out correctly at
    // `0 0 1600x1000` (the flex backdrop) with their children centred, e.g.
    // `#shortcutsBody 541 451 518x140` and `#shortcutsDone 986 602 53x30`, and
    // `--hit` reaches those children. See `nzk_ui::dom::UiState::modals_open`.
    //
    // What *is* true, and is why these ids stay zero-area in the shipped
    // document, is narrower: they ship `class="modal-backdrop hidden"` and
    // `.hidden { display: none !important }`, so while closed they have no box
    // at all. The zero-area row is a fact about the *closed* state, not an
    // argument that the family can never work.
    // `#nodeCodeBtn` is a real control now (task-2): v2 wires it to
    // `viewCodeFor(state.selected)` (`shell.js:563`), so it is Host-owned and
    // `NzkUiAction::SourceOpen` asks the host to open that declaration in its
    // source pane. It keeps its zero-area *closed* box like its drawer siblings.
    Control { selector: "#nodeCodeBtn", owner: ControlOwner::Host, label: "Node source" },
    Control { selector: "#shortcutsModal", owner: ControlOwner::Inert, label: "Shortcuts modal backdrop" },
    Control { selector: "#shortcutsClose", owner: ControlOwner::Host, label: "Close shortcuts" },
    Control { selector: "#shortcutsDone", owner: ControlOwner::Host, label: "Shortcuts done" },
    Control { selector: "#editModal", owner: ControlOwner::Inert, label: "Edit modal backdrop" },
    Control { selector: "#editClose", owner: ControlOwner::Host, label: "Close Edit" },
    // The editor's run buttons. Host-owned because a press reaches the ENGINE
    // (`edit_transport::invoke`): `#editDry` is `run(false)` and `#editApply` is
    // `run(true)` (`edit.js:276-277`). Leaving them out of the registry was what
    // made Apply a painted button with no consequence — the modal could be
    // dry-run from the args box and never write.
    Control { selector: "#editDry", owner: ControlOwner::Host, label: "Edit: dry run" },
    Control { selector: "#editApply", owner: ControlOwner::Host, label: "Edit: apply" },
    Control { selector: "#editValidate", owner: ControlOwner::Host, label: "Edit: validate" },
    Control { selector: "#editDedupe", owner: ControlOwner::Host, label: "Edit: duplicates" },
    Control { selector: "#editTool", owner: ControlOwner::Host, label: "Edit: operation" },
    Control { selector: "#uiTheme", owner: ControlOwner::Host, label: "UI theme" },
    Control { selector: "#chainDepth", owner: ControlOwner::Host, label: "Chain: depth" },
    Control { selector: "#chainDir", owner: ControlOwner::Host, label: "Chain: direction" },
    Control { selector: "#chainRun", owner: ControlOwner::Host, label: "Chain: run" },
    Control { selector: "#chainFlatBtn", owner: ControlOwner::Host, label: "Chain: flat list" },
    Control { selector: "#pubSearch", owner: ControlOwner::Host, label: "Public definitions: search" },
    Control { selector: "#strSearch", owner: ControlOwner::Host, label: "Strings: search" },
    // The v3-only Declared-types section's filter box. Runtime-created by
    // `dom::apply_type_defs`, so it has no `index.html` counterpart; registering
    // it here is what gives a press a real action (`FormOpen`) instead of
    // falling through to the canvas keymap.
    Control { selector: "#typeSearch", owner: ControlOwner::Host, label: "Declared types: filter" },
    Control { selector: "#previewModal", owner: ControlOwner::Inert, label: "Preview modal backdrop" },
    Control { selector: "#previewClose", owner: ControlOwner::Host, label: "Close Preview" },
    Control { selector: "#reorgModal", owner: ControlOwner::Inert, label: "Reorg modal backdrop" },
    Control { selector: "#reorgClose", owner: ControlOwner::Host, label: "Close Reorg" },
    // The export dialog's foot buttons (`reorg.js:172-200`). Each press issues a
    // real `/api/reorg` action in process (`NzkUiAction::ReorgRun`), so they are
    // Host controls, not decoration.
    Control { selector: "#reorgVerify", owner: ControlOwner::Host, label: "Reorg: verify" },
    Control { selector: "#reorgZip", owner: ControlOwner::Host, label: "Reorg: download .zip" },
    Control { selector: "#reorgWrite", owner: ControlOwner::Host, label: "Reorg: write" },
    Control { selector: "#reorgCompile", owner: ControlOwner::Host, label: "Reorg: compile" },
    // The form controls (task-14). Each is a Host control whose press opens its
    // menu / focuses its box / toggles it through `NzkUiAction::FormOpen`.
    Control { selector: "#reorgMode", owner: ControlOwner::Host, label: "Reorg: mode" },
    Control { selector: "#reorgScope", owner: ControlOwner::Host, label: "Reorg: scope" },
    Control { selector: "#reorgGran", owner: ControlOwner::Host, label: "Reorg: chunk unit" },
    Control { selector: "#reorgGroup", owner: ControlOwner::Host, label: "Reorg: grouping" },
    Control { selector: "#reorgSize", owner: ControlOwner::Host, label: "Reorg: size" },
    Control { selector: "#reorgDest", owner: ControlOwner::Host, label: "Reorg: destination" },
    Control { selector: "#reorgHeader", owner: ControlOwner::Host, label: "Reorg: banner" },
    Control { selector: "#previewSelector", owner: ControlOwner::Host, label: "Preview: declaration" },
    Control { selector: "#previewTo", owner: ControlOwner::Host, label: "Preview: target" },
    Control { selector: "#previewOp", owner: ControlOwner::Host, label: "Preview: operation" },
    Control { selector: "#previewRun", owner: ControlOwner::Host, label: "Preview: run" },
    Control { selector: "#modalPathInput", owner: ControlOwner::Host, label: "Picker: path" },
    // The folder picker's backdrop. It was simply MISSING from this table while
    // its four siblings were listed — an omission, not a decision, and it left
    // the family's "a backdrop is never a press target" rule unenforced for the
    // one modal this task wires. `shell.js:761` closes the picker on a backdrop
    // `mousedown` and that is handled host-side (`PickerAction::Close`), because
    // resolving "did the press miss the card?" needs the DOM chain, not a
    // selector table. As a *control* it stays Inert, exactly like the other four.
    Control { selector: "#modal", owner: ControlOwner::Inert, label: "Folder picker backdrop" },
    // -- the folder-picker modal's own controls (task-5) ----------------------
    // `#browseBtn` opens `#modal`; while it is open these are the controls
    // inside it, and they are only laid out once `modals_open` names `modal`.
    // Each is a real `PickerAction` in `actions.rs` rather than a selector this
    // table owns, because four of them need no payload while the other three do
    // (`#modalGo` needs the input's text, a `#modalBody` row needs its
    // `data-nav`, `#modalSelect` needs the committed root) — the registry is a
    // list of *selectors*, so it can name the first four and only the first four
    // are resolvable from an id alone.
    //
    // `#modalSelect` IS registered: it has no payload of its own (it commits the
    // folder already on screen, which the picker holds), so
    // `PickerAction::from_selector` answers it directly. `#modalBody` and
    // `#modalPathInput` are deliberately NOT registered — neither is a control,
    // they are the two elements whose *contents* are the payload, and a press on
    // the body is resolved through the `.pick[data-nav]` row it landed on.
    //
    // The `#modal` backdrop stays `Inert` above, for the reason recorded there:
    // it covers the whole viewport when open, so a press anywhere resolves to
    // it. Its `mousedown`-to-close (`shell.js:761`) is therefore host-side
    // (`PickerAction::Close`), not a registry entry.
    Control { selector: "#modalClose", owner: ControlOwner::Picker, label: "Close folder picker" },
    Control { selector: "#modalCancel", owner: ControlOwner::Picker, label: "Cancel folder picker" },
    Control { selector: "#modalUp", owner: ControlOwner::Picker, label: "Picker: up" },
    Control { selector: "#modalHome", owner: ControlOwner::Picker, label: "Picker: home" },
    Control { selector: "#modalRoot", owner: ControlOwner::Picker, label: "Picker: filesystem root" },
    Control { selector: "#modalRepo", owner: ControlOwner::Picker, label: "Picker: repo root" },
    Control { selector: "#modalGo", owner: ControlOwner::Picker, label: "Picker: open typed path" },
    Control { selector: "#modalSelect", owner: ControlOwner::Picker, label: "Picker: select this folder" },
    Control { selector: "#resetFilters", owner: ControlOwner::Host, label: "Reset filters" },
    // -- the Layout ▸ range inputs (task-10) ---------------------------------
    // The four the reference wires plus the fork's text size. They are `Slider`
    // rather than `Host` because a press on one carries a *value* derived from
    // where inside the box it landed (`route_drag`), and rather than `Inert`
    // because they are genuinely interactive.
    //
    // The four `*Out` readout spans beside them are NOT registered. They are
    // `output` elements — text, not controls — and `route_click` must not claim
    // a press on one. `syncLayoutControls` (`shell.js:807-816`) is what fills
    // them, and that is a paint-side job, not a hit-test entry.
    Control { selector: "#spacingRange", owner: ControlOwner::Slider, label: "Layout: spacing" },
    Control { selector: "#nodeSizeRange", owner: ControlOwner::Slider, label: "Layout: node size" },
    Control { selector: "#iterRange", owner: ControlOwner::Slider, label: "Layout: iterations" },
    Control { selector: "#curveRange", owner: ControlOwner::Slider, label: "Layout: edge curve" },
    Control { selector: "#textSizeRange", owner: ControlOwner::Slider, label: "Layout: text size" },
    Control { selector: "#pruneHidden", owner: ControlOwner::Host, label: "Prune hidden" },
    // -- the rest of the Filters section's static controls (task-1) ----------
    // They were painted by the stylesheet but absent from this table (or listed
    // `Inert`), which is the "painted button with no dispatch entry" class of
    // dead control: `#nsAllBtn` was here as `Inert`, and the six checkboxes had
    // no row at all. Every one of them now has a real `FilterCmd`, so a press on
    // its own box (or on the label text, through `filter_click`'s `data-key`
    // payload) reaches the model.
    Control { selector: "#nsAllBtn", owner: ControlOwner::Host, label: "All namespaces" },
    Control { selector: "#nsDeep", owner: ControlOwner::Host, label: "Include sub-namespaces" },
    Control { selector: "#hideEdgeLabels", owner: ControlOwner::Host, label: "VT mode (direct-line relations)" },
    Control { selector: "#hideIsolated", owner: ControlOwner::Host, label: "Hide isolated" },
    Control { selector: "#hideChildren", owner: ControlOwner::Host, label: "Hide relations" },
    // The two visible Group chips are labels with a hidden legacy checkbox
    // inside them, so the label itself is the press target (`#kindEdgeWrap`
    // also carries v2's `.off` class when its flag is off).
    Control { selector: "#kindChildWrap", owner: ControlOwner::Host, label: "Group child nodes" },
    Control { selector: "#kindEdgeWrap", owner: ControlOwner::Host, label: "Group edge nodes" },
    Control { selector: "#depthSelect", owner: ControlOwner::Host, label: "Expansion depth" },
    Control { selector: "#stopIndexBtn", owner: ControlOwner::Host, label: "Stop indexing" },
    // -- the SOURCE PANE's own head controls (task-1) ------------------------
    // Four of the six are wired to the behaviour `source_pane.rs` already had,
    // through real `NzkUiAction`s (`SrcClose`/`SrcMode`/`SrcPop`/`SrcCopy`) that
    // `app.rs` completes against the live `SourceState`:
    //
    //   #srcCloseBtn -> `SourceState::close()` on the host's pane.
    //   #srcModeBtn  -> cycles the three `SourceTheme`s that exist, and the
    //                   palette the pane is painted with is read back from
    //                   `NzkUiState::source_mode` (panel, overlay and the
    //                   popped-out window all agree).
    //   #srcPopBtn   -> `Action::Detach`/`Action::Dock` on `Pane::Source`, the
    //                   same real-window path as F2.
    //   #srcCopyBtn  -> `egui::Context::copy_text` of the synthesized document
    //                   (`vendor/egui/src/context.rs:1995`; egui-winit is built
    //                   with the `clipboard` feature), so the OS clipboard really
    //                   receives the text.
    //   #srcRefBtn   -> `SourceState`'s `file:line`, written to the clipboard
    //                   (`source.js:546-550`).
    //   #srcMethodBtn-> re-opens the whole declaration (`source.js:478-482`);
    //                   this pane's document already IS the whole declaration.
    //   #srcWindowBtn-> opens the current document in a second OS window.
    //   #srcPrevBtn  -> `SourceState::history_back()` on the pane's real
    //   #srcNextBtn     document history (the same stacks Alt+Left/Right use).
    Control { selector: "#srcCloseBtn", owner: ControlOwner::Host, label: "Close source pane" },
    Control { selector: "#srcCopyBtn", owner: ControlOwner::Host, label: "Copy source" },
    Control { selector: "#srcRefBtn", owner: ControlOwner::Host, label: "Copy source reference" },
    Control { selector: "#sourceTheme", owner: ControlOwner::Host, label: "Source theme" },
    Control { selector: "#srcModeBtn", owner: ControlOwner::Host, label: "Source mode" },
    Control { selector: "#srcMethodBtn", owner: ControlOwner::Host, label: "Source: whole declaration" },
    Control { selector: "#srcWindowBtn", owner: ControlOwner::Host, label: "Source: new window" },
    Control { selector: "#srcPopBtn", owner: ControlOwner::Host, label: "Pop source out" },
    Control { selector: "#srcPrevBtn", owner: ControlOwner::Host, label: "Source: previous document" },
    Control { selector: "#srcNextBtn", owner: ControlOwner::Host, label: "Source: next document" },
];

/// A snapshot of the nzk_ui presentation state.
pub fn presentation() -> Presentation {
    PRESENT.with(|p| *p.borrow())
}

/// Replace the nzk_ui presentation state; the next paint rebuilds the frame.
pub fn set_presentation(next: Presentation) {
    PRESENT.with(|p| *p.borrow_mut() = next);
}

/// Set the sidebar scroll offset, clamped to the last measured maximum.
/// Returns true when the offset changed (the frame must be rebuilt).
pub fn set_side_scroll(y: f64) -> bool {
    PRESENT.with(|p| {
        let mut p = p.borrow_mut();
        let clamped = y.clamp(0.0, p.side_scroll_max.max(0.0));
        if (clamped - p.side_scroll).abs() < 0.5 {
            return false;
        }
        p.side_scroll = clamped;
        true
    })
}

/// Scroll the sidebar by `dy` CSS px (wheel). Returns true when it moved.
pub fn scroll_side_by(dy: f64) -> bool {
    let current = presentation().side_scroll;
    set_side_scroll(current + dy)
}

/// The sidebar's scrollable extent measured by the engine, in CSS px.
pub fn side_scroll_max() -> f64 {
    presentation().side_scroll_max
}

/// The scroll region the nzk_ui measured, for a host that drives the wheel.
pub fn sidebar_scroll_region() -> Option<Rect> {
    box_rect(".side")
}

/// The control a pointer at `(x, y)` hits, keyed by DOM selector. The smallest
/// containing box wins, so a nested control beats its container.
///
/// This is the **geometry half** of hit-testing: it answers "which registered
/// control's box contains this point", given a box map. `nzk_ui_ui` feeds it the
/// same [`FrameGeometry`] that was painted, and [`resolve_selector`] consults the
/// shell's own DOM hit-test **first**, so a click on a control's *child* element
/// still resolves to the control.
///
/// It remains the fallback answer, and it is honest about what it cannot do: a
/// flat map of one rect per registered selector has no notion of clipping,
/// overflow or stacking, so a control whose box is covered by another element
/// still "contains" a point that is painted as something else.
pub fn hit_in(boxes: &BTreeMap<String, Rect>, x: f32, y: f32) -> Option<&'static str> {
    let mut best: Option<(&'static str, f32)> = None;
    for c in CONTROLS {
        let Some(r) = boxes.get(c.selector) else { continue };
        if !r.contains(Pos2::new(x, y)) {
            continue;
        }
        let area = r.width() * r.height();
        if best.map(|(_, a)| area < a).unwrap_or(true) {
            best = Some((c.selector, area));
        }
    }
    best.map(|(s, _)| s)
}

/// The registered control a raw DOM hit resolves to.
///
/// `chain` is the shell's own `id`/`class` report for the hit element followed by
/// each ancestor, nearest first — `shell::Hit`-shaped, so it comes from the same
/// layout that painted. The first **hit** entry whose id is a registered selector
/// wins; ancestor entries are deliberately skipped (see below).
///
/// Ancestors exist for the [`HitNode::ancestor`] class resolvers (`closest()`
/// semantics: `.prop-action`, `.rel-node`, `.cs-tab`, …). Their ids are NOT
/// routed: a press anywhere inside a panel would otherwise resolve the panel's
/// own container (`#nodeDrawer` and friends resolve to `ToggleDrawer`) and close
/// it. Registered controls that a child element covers are still answered by the
/// flat box map in [`resolve_selector`]: it contains `#shortcutsBtn` (whose
/// centre hit is the `<svg class="shortcuts-gear">` child) and `#nsSelect`, and
/// both are pinned by `the_two_real_child_hits_route_to_their_controls`.
pub fn resolve_hit_chain(chain: &[HitNode]) -> Option<&'static str> {
    for node in chain {
        // Ancestor entries exist for the class/`closest()` resolvers only. The
        // id -> control walk must stay on the element the pointer actually hit:
        // otherwise a press anywhere inside a drawer or a modal would resolve
        // its CONTAINER (`#nodeDrawer`, `#shortcutsModal`) and close the panel
        // the press was aimed at.
        if node.ancestor {
            continue;
        }
        let Some(id) = node.id.as_deref() else { continue };
        // An id that is NOT a registered selector must be SKIPPED, not abort the
        // walk. A real DOM is full of ids we do not route on (`#nodeProps`,
        // `#findMeta`, ...), and aborting on the first one would stop before
        // reaching the registered ancestor control above it.
        if let Some(selector) = selector_of_id(id) {
            return Some(selector);
        }
    }
    None
}

/// One element in a DOM hit chain: the shell's own `node` id plus the element's
/// `id` and `class` attributes.
///
/// **Routing is keyed by the DOM selector nzk_ui lays out**, never by the
/// `node` field: node ids are not stable across a rebuild. `node` is carried
/// because it is what makes a chain *provably* the shell's — a chain decoded
/// from the child's `@hit` TSV row always names the tree node that was hit,
/// whereas a hand-built chain cannot. An empty `node` means "no shell
/// provenance", and every synthetic chain in the tests is built that way.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HitNode {
    pub node: Option<String>,
    pub id: Option<String>,
    pub class: Option<String>,
    /// The nearest `data-key` on this node or an ancestor, from the shell's own
    /// hit test. `#cs-tabs` is created at runtime by `nzk_ui::dom::apply_tabs`
    /// (`shell.js:1192-1248`), so its rows have no static box-map entry: this is
    /// the payload that routes a press on a row (or its `.cs-tab-label` child)
    /// to that document.
    pub data_key: Option<String>,
    /// The nearest `data-close` on this node or an ancestor — `.cs-tab-x`
    /// (`shell.js:1216-1222`).
    pub data_close: Option<String>,
    /// The nearest `data-id` on this node or an ancestor — the graph-mode
    /// suggestion row's node id (`search.js:30`). A press on a `.sug` row (or
    /// its `.sug-label` child) carries this, which routes a click to
    /// `SuggestionView::commit_by_id` (`shell.js:527-536`).
    pub data_id: Option<String>,
    /// The nearest `data-nav` on this node or an ancestor — a `.pick` row's
    /// absolute folder path (`shell.js:58`). A press on a row (or its `.pname`
    /// child) carries this, so the picker navigates to the folder the user
    /// actually pressed instead of to an equal-band approximation of it.
    pub data_nav: Option<String>,
    /// The nearest `title` on this node or an ancestor, from the shell's own hit
    /// test — i.e. the text a browser would draw as its hover tooltip.
    ///
    /// The shell is a static raster with no browser tooltip layer, so a `title`
    /// was previously invisible in v3. The host paints it natively from the same
    /// layout that painted the pixels (see
    /// [`paint_feedback`]), which is why the string is carried on the hit chain
    /// rather than read back from the DOM after the fact.
    pub title: Option<String>,
    /// True for an **ancestor** entry of the shell's hit chain, not the element
    /// the pointer actually hit.
    ///
    /// The shell reports the `id`/`class` trail above the hit because the
    /// reference routes with `ev.target.closest(...)`. The label of a `<button>`
    /// is painted as an anonymous inline box with no identity, and a row's child
    /// span carries only its own class (`.rn-label`, `.dot`, `.fname`), so the
    /// deepest hit alone cannot answer the class match that makes those presses
    /// work. Ancestor entries carry the hit's `data-*` payloads (the nearest one
    /// is the same row's), which is what lets `.prop-action`, `.rel-node`,
    /// `.cs-tab`, `.sug`, `.frow` and `.lrow` resolve exactly as
    /// `closest()` would.
    ///
    /// [`resolve_hit_chain`] deliberately **skips** these: the id→control walk
    /// must stay on the element that was hit, or an ancestor container
    /// (`#nodeDrawer`, a modal backdrop) would be turned into a button and close
    /// the panel the press was aimed at.
    pub ancestor: bool,
}

impl HitNode {
    /// A node with an id and no class, with no shell provenance.
    pub fn id(id: &str) -> HitNode {
        HitNode {
            node: None,
            id: Some(id.to_string()),
            class: None,
            data_key: None,
            data_close: None,
            data_id: None,
            data_nav: None,
            title: None,
            ancestor: false,
        }
    }

    /// A node with a class and no id, with no shell provenance.
    pub fn class(class: &str) -> HitNode {
        HitNode {
            node: None,
            id: None,
            class: Some(class.to_string()),
            data_key: None,
            data_close: None,
            data_id: None,
            data_nav: None,
            title: None,
            ancestor: false,
        }
    }

    /// A node decoded from the child's own `@hit` row — shell-provenanced.
    pub fn shell(node: &str, id: Option<&str>, class: Option<&str>) -> HitNode {
        HitNode::shell_row(node, id, class, None, None)
    }

    /// A node decoded from a round-7 `@hit` row, including the two additive
    /// routing-payload columns (`data-key`, `data-close`).
    pub fn shell_row(
        node: &str,
        id: Option<&str>,
        class: Option<&str>,
        data_key: Option<&str>,
        data_close: Option<&str>,
    ) -> HitNode {
        let clean = |s: Option<&str>| {
            s.map(str::to_string).filter(|s| !s.is_empty())
        };
        HitNode {
            node: Some(node.to_string()),
            id: clean(id),
            class: clean(class),
            data_key: clean(data_key),
            data_close: clean(data_close),
            data_id: None,
            data_nav: None,
            title: None,
            ancestor: false,
        }
    }

    /// A round-9 row: [`HitNode::shell_row`] plus the suggestion row's
    /// `data-id` payload. A separate constructor so round-7 rows (and every
    /// existing test) decode exactly as before with no payload — an absent
    /// payload is `None`, never a guessed id.
    pub fn shell_row_payload(
        node: &str,
        id: Option<&str>,
        class: Option<&str>,
        data_key: Option<&str>,
        data_close: Option<&str>,
        data_id: Option<&str>,
        data_nav: Option<&str>,
    ) -> HitNode {
        let mut h = HitNode::shell_row(node, id, class, data_key, data_close);
        h.data_id = data_id.map(str::to_string).filter(|s| !s.is_empty());
        h.data_nav = data_nav.map(str::to_string).filter(|s| !s.is_empty());
        h
    }

    /// Attach the thirteenth `@hit` column (the nearest `title`).
    ///
    /// A builder rather than another positional argument: `shell_row_payload`
    /// already has seven parameters, and only the shell-row decoder needs this
    /// one. An absent/empty title stays `None` — never an empty tooltip.
    pub fn with_title(mut self, title: Option<&str>) -> HitNode {
        self.title = title.map(str::to_string).filter(|s| !s.is_empty());
        self
    }

    /// Whether this node came from the shell's own hit-test rather than from a
    /// caller that made it up.
    pub fn is_from_shell(&self) -> bool {
        self.node.is_some()
    }
}

/// One-line rendering of a nearest-first hit chain, for the input trace.
///
/// Every element prints as `node:id.class[data-*]`; absent parts are omitted so
/// the line stays readable. The chain is what routing actually consumed, so a
/// report can be replayed against the same resolution rather than inferred from
/// a screenshot.
pub fn chain_summary(chain: &[HitNode]) -> String {
    if chain.is_empty() {
        return "-".to_string();
    }
    let mut out = String::new();
    for (i, h) in chain.iter().enumerate() {
        if i > 0 {
            out.push_str(" < ");
        }
        if let Some(node) = h.node.as_deref() {
            out.push_str(node);
        }
        if let Some(id) = h.id.as_deref() {
            out.push('#');
            out.push_str(id);
        }
        if let Some(class) = h.class.as_deref() {
            out.push('.');
            out.push_str(&class.replace(' ', "."));
        }
        if node_and_id_absent(h) {
            out.push('?');
        }
        if let Some(k) = h.data_key.as_deref() {
            out.push_str("[data-key=");
            out.push_str(k);
            out.push(']');
        }
        if let Some(c) = h.data_close.as_deref() {
            out.push_str("[data-close=");
            out.push_str(c);
            out.push(']');
        }
        if let Some(id) = h.data_id.as_deref() {
            out.push_str("[data-id=");
            out.push_str(id);
            out.push(']');
        }
        if let Some(nav) = h.data_nav.as_deref() {
            out.push_str("[data-nav=");
            out.push_str(nav);
            out.push(']');
        }
    }
    out
}

/// True when a chain row names no element identity at all (no node, id or
/// class), which is worth a marker in the trace rather than an empty token.
fn node_and_id_absent(h: &HitNode) -> bool {
    h.node.is_none() && h.id.is_none() && h.class.is_none()
}

/// The egui modifier state as the same canonical token the host's key trace
/// prints (`ctrl+alt+shift+meta`, `-` for none).
fn egui_mods_token(ctx: &egui::Context) -> String {
    let m = ctx.input(|i| i.modifiers);
    let mut out = String::new();
    let mut add = |on: bool, name: &str| {
        if on {
            if !out.is_empty() {
                out.push('+');
            }
            out.push_str(name);
        }
    };
    add(m.ctrl, "ctrl");
    add(m.alt, "alt");
    add(m.shift, "shift");
    add(m.mac_cmd, "meta");
    if out.is_empty() {
        "-".to_string()
    } else {
        out
    }
}

/// Decode the child's `@hit` rows into `(x, y, node)` triples.
///
/// This is the app-side half of the `--serve` contract documented in
/// `nzk_ui/src/serve.rs`:
/// `@hit\t<x>\t<y>\t<node>\t<tag>\t<id>\t<class>\t<data-key>\t<data-close>\t<data-id>\t<data-nav>`,
/// or `@hit\t<x>\t<y>\tnone` when the shell found no element under the point. The
/// payload columns are additive (round 7 added two, round 9 adds `data-id`,
/// this round adds the folder-picker row's `data-nav`); an older row simply
/// lacks them and decodes to `None`, never to a guessed payload. They carry the
/// nearest `data-*` from the hit element **or an ancestor**, which is how a
/// press on a `.cs-tab`'s or `.pick`'s child still names its row. A malformed or `none` row
/// is skipped rather than guessed at, so a truncated
/// frame degrades to "no chain" — and therefore to the `--boxes` fallback —
/// instead of misrouting a press.
pub fn hits_from_tsv(tsv: &str) -> Vec<(f32, f32, HitNode)> {
    let mut out = Vec::new();
    for line in tsv.lines() {
        let Some(rest) = line.strip_prefix("@hit\t") else { continue };
        let f: Vec<&str> = rest.split('\t').collect();
        if f.len() < 5 || f[2] == "none" {
            continue;
        }
        let (Ok(x), Ok(y)) = (f[0].trim().parse::<f32>(), f[1].trim().parse::<f32>()) else {
            continue;
        };
        out.push((
            x,
            y,
            HitNode::shell_row_payload(
                f[2],
                f.get(4).copied(),
                f.get(5).copied(),
                f.get(6).copied(),
                f.get(7).copied(),
                f.get(8).copied(),
                f.get(9).copied(),
            ),
        ));
    }
    out
}

/// One press on the runtime-created `#cs-tabs` bar, resolved from the shell's hit
/// chain (`shell.js:1250-1266`).
///
/// The bar is built by `nzk_ui::dom::apply_tabs` at runtime, so it has no
/// `CONTROLS` selector and no `--boxes` entry: the hit chain is the only place a
/// press on it can be answered from, which is why [`HitNode`] carries the
/// `data-*` payload. Precedence is the reference's `onBarClick` order — the ✕'s
/// `data-close` first, then `#csSplitBtn`, then `#csTabCloseOthers`, then the
/// row's `data-key` — applied to the nearest-first chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BarAction {
    /// `.cs-tab[data-key]` (or a child of one) was pressed: activate that key.
    Activate(String),
    /// `.cs-tab-x[data-close]` was pressed: close that key.
    Close(String),
    /// `#csSplitBtn` was pressed while enabled.
    ToggleSplit,
    /// `#csTabCloseOthers` was pressed while enabled.
    CloseOthers,
    /// The press landed on the bar (a disabled control, its background, or a
    /// row/action with no payload). It is **consumed**: the bar is painted above
    /// the nzk_ui, so a press inside it must never fall through to the element
    /// the flat `--boxes` map believes is behind it.
    Swallow,
}

impl BarAction {
    /// The host dispatch request for this bar press, if it performs one.
    ///
    /// All four commands are the existing [`crate::actions::TabCmd`]s, applied by
    /// `actions::apply` through the same `chords::tabs::Tabs` model the keyboard
    /// chords use. `Swallow` requests nothing.
    pub fn into_nzk_ui_action(self) -> Option<NzkUiAction> {
        use crate::actions::TabCmd;
        match self {
            BarAction::Activate(key) => Some(NzkUiAction::Tab(TabCmd::Activate(key))),
            BarAction::Close(key) => Some(NzkUiAction::Tab(TabCmd::Close(key))),
            BarAction::ToggleSplit => Some(NzkUiAction::Tab(TabCmd::ToggleSplit)),
            BarAction::CloseOthers => Some(NzkUiAction::Tab(TabCmd::CloseOthers)),
            BarAction::Swallow => None,
        }
    }
}

/// One press on a graph-mode `.sug` suggestion row — the reference's
/// `suggestionsEl` `mousedown` handler (`shell.js:527-536`).
///
/// The reference commits the row's `data-id` on **mousedown** (not click, so the
/// input does not blur first) and always calls `preventDefault`. This port
/// mirrors that: the press is consumed so the canvas can never also act on it,
/// and the id goes through the same commit path as Enter. A `.sug` element with
/// no `data-id` (a stale shell, or the `.sug-empty` message) is
/// [`SuggestHit::Swallow`]: the popup is painted above the nzk_ui, so a press
/// inside it must never fall through to whatever is behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SuggestHit {
    /// A row carrying its node id: commit that id.
    Commit(String),
    /// Inside the popup but naming no row: consumed, nothing to commit.
    Swallow,
}

/// The class token the popup's rows carry (`search.js:30`,
/// `class="sug"`/`class="sug active"`).
fn is_suggestion_row(node: &HitNode) -> bool {
    node.class
        .as_deref()
        .map(|classes| classes.split_whitespace().any(|c| c == "sug"))
        .unwrap_or(false)
}

/// Whether a chain node is anywhere inside the popup (`#suggestions`, its rows,
/// or the `.sug-empty` message).
fn is_suggestions_node(node: &HitNode) -> bool {
    if node.id.as_deref() == Some("suggestions") {
        return true;
    }
    node.class
        .as_deref()
        .map(|classes| {
            classes
                .split_whitespace()
                .any(|c| c == "suggestions" || c == "sug" || c == "sug-empty")
        })
        .unwrap_or(false)
}

/// Resolve a press to the suggestion-popup action the reference's `mousedown`
/// handler would take, or `None` when the press is not in the popup.
///
/// The chain is nearest-first, so a press on a row's `.sug-label` span finds
/// the `.sug` button above it — the reference's `ev.target.closest('.sug')`.
pub fn suggestion_row_click(chain: &[HitNode]) -> Option<SuggestHit> {
    if !chain.iter().any(is_suggestions_node) {
        return None;
    }
    for node in chain {
        if is_suggestion_row(node) {
            return Some(match node.data_id.as_deref().filter(|s| !s.is_empty()) {
                Some(id) => SuggestHit::Commit(id.to_string()),
                None => SuggestHit::Swallow,
            });
        }
    }
    Some(SuggestHit::Swallow)
}

/// `#cs-tabs` and everything inside it: the bar element itself, a `.cs-tab`
/// row, `.cs-tab-label`, `.cs-tab-x`, `.cs-tab-splitmark`, `.cs-tab-actions`,
/// plus the two action buttons' ids.
fn is_bar_node(node: &HitNode) -> bool {
    if let Some(id) = node.id.as_deref() {
        if matches!(id, "cs-tabs" | "csSplitBtn" | "csTabCloseOthers") || id.starts_with("csTab") {
            return true;
        }
    }
    node.class
        .as_deref()
        .map(|classes| {
            classes
                .split_whitespace()
                .any(|c| c == "cs-tabs" || c.starts_with("cs-tab"))
        })
        .unwrap_or(false)
}

/// Resolve a press on the `#cs-tabs` bar to the command the reference's
/// `onBarClick`/`onBarAux` would run, or `None` when the press is not on the bar
/// at all.
///
/// **Disabled controls are decided from the app's own tabs model**, never from a
/// fabricated fact: the bar's `disabled` attribute is painted from
/// `tabs.len() < 2 && !split_on` (`#csSplitBtn`) and `tabs.len() < 2`
/// (`#csTabCloseOthers`, `shell.js:1235-1248`), and this takes those same
/// predicates from `Tabs::count`/`Tabs::split`. A disabled press is still
/// `Swallow`ed — the bar is on top, so it must not hit anything behind it — but
/// it performs no command.
pub fn bar_click(chain: &[HitNode], tabs: &Tabs) -> Option<BarAction> {
    if !chain.iter().any(is_bar_node) {
        return None;
    }
    for node in chain {
        if let Some(key) = node.data_close.as_deref().filter(|k| !k.is_empty()) {
            return Some(BarAction::Close(key.to_string()));
        }
        match node.id.as_deref() {
            Some("csSplitBtn") => {
                // `splitBtn.disabled = tabs.length < 2 && !isSplitOn()`.
                let enabled = tabs.count() >= 2 || tabs.split();
                return Some(if enabled {
                    BarAction::ToggleSplit
                } else {
                    BarAction::Swallow
                });
            }
            Some("csTabCloseOthers") => {
                // `others.disabled = tabs.length < 2`.
                return Some(if tabs.count() >= 2 {
                    BarAction::CloseOthers
                } else {
                    BarAction::Swallow
                });
            }
            _ => {}
        }
        if let Some(key) = node.data_key.as_deref().filter(|k| !k.is_empty()) {
            return Some(BarAction::Activate(key.to_string()));
        }
    }
    // Inside the bar, but with no payload: background, a disabled control we did
    // not recognise, or a gap between rows. Consumed, never a fall-through.
    Some(BarAction::Swallow)
}

/// Resolve a **middle-button** press on the `#cs-tabs` bar.
///
/// The reference's `onBarAux` (`shell.js:1260-1266`): if the event is a middle
/// button (`ev.button === 1`) and the target's `closest('.cs-tab[data-key]')`
/// exists, it closes that key — the same [`TabCmd::Close`] the ✕ runs. It is a
/// *different* rule from `onBarClick`: only a `.cs-tab` row (or a child whose
/// nearest ancestor is one) closes; a middle press on the ✕, a bar button, the
/// bar's background or a gap performs nothing. The chain is nearest-first, so
/// the first node carrying a `data-key` is the closest row, exactly `closest()`.
///
/// `Some(Swallow)` means "this press was on the bar and is consumed even though
/// it performs nothing"; `None` means the press was not on the bar at all.
pub fn bar_aux(chain: &[HitNode]) -> Option<BarAction> {
    if !chain.iter().any(is_bar_node) {
        return None;
    }
    for node in chain {
        if let Some(key) = node.data_key.as_deref().filter(|k| !k.is_empty()) {
            return Some(BarAction::Close(key.to_string()));
        }
    }
    // Inside the bar, but no `.cs-tab[data-key]`: `onBarAux` returns without
    // preventDefault, so nothing is performed — but the bar is painted above the
    // nzk_ui, so the press is consumed rather than falling through.
    Some(BarAction::Swallow)
}

/// Resolve a press on the runtime-created `#csSplitCollapse` button.
///
/// The split scaffold is built by `nzk_ui::dom::apply_split` from the tabs
/// facts, exactly like `#cs-tabs`: it has no static [`CONTROLS`] selector and no
/// `--boxes` entry, so only the shell's hit chain can name it. It is a sibling
/// resolver to [`bar_click`] rather than a branch inside it because the button
/// lives in `#csSplitBody`, outside the bar's subtree.
///
/// `Some(ToggleSplit)` only while the app's own `Tabs` model says the split is
/// on; with the split off the button is not painted at all, and a press that
/// somehow names it is still consumed (`Swallow`) but must NOT enable the split
/// as a side effect. A press on the button's text node routes here through the
/// ancestor walk because the chain is nearest-first.
pub fn split_collapse_click(chain: &[HitNode], tabs: &Tabs) -> Option<BarAction> {
    let on_collapse = chain
        .iter()
        .any(|n| n.id.as_deref() == Some("csSplitCollapse"));
    if !on_collapse {
        return None;
    }
    Some(if tabs.split() {
        BarAction::ToggleSplit
    } else {
        BarAction::Swallow
    })
}

/// Resolve a press on the sidebar filter controls (`filters.js`).
///
/// Two routing channels, both read from the shell's own nearest-first hit
/// element and its ancestor walk:
///
/// * **`data-key`** — the payload channel for *runtime-created* rows and for
///   label text. `nzk_ui::dom` writes `facet:<kind>`, `edgekey:<kind>`,
///   `proj:<id>` on the chips and `filter:<control>` on the filter controls'
///   labels/buttons, and the shell reports the **nearest ancestor** `data-key`
///   (`nearest_data_attr`), which is what makes a press on a chip's
///   `.facet-name`/`.swatch` span resolve to the chip — the same
///   `closest('.facet')` rule the reference uses.
/// * **`id`** — `#resetFilters`/`#pruneHidden` and the static controls, whose
///   own box a press can land on directly.
///
/// The two channels carry the same `FilterCmd`s: a chip press and a press on
/// the chip's text must not be two different code paths.
pub fn filter_click(chain: &[HitNode]) -> Option<crate::actions::FilterCmd> {
    use crate::actions::FilterCmd;
    for node in chain {
        // The class channel first preserves the pre-data-key wire shape: the
        // chips already carry `data-id` (the kind / `proj:<id>`), so an older
        // shell or a synthetic row with only class+data-id still routes.
        let classes = node.class.as_deref().unwrap_or("");
        let is_facet = classes.split_whitespace().any(|c| c == "facet");
        let is_edgekey = classes.split_whitespace().any(|c| c == "edgekey");
        if let Some(key) = node.data_id.as_deref().filter(|k| !k.is_empty()) {
            if is_facet {
                return Some(FilterCmd::ToggleKind(key.to_string()));
            }
            if is_edgekey {
                if let Some(project) = key.strip_prefix("proj:") {
                    return Some(FilterCmd::ToggleProject(project.to_string()));
                }
                return Some(FilterCmd::ToggleEdge(key.to_string()));
            }
        }
        // The `data-key` payload channel (runtime rows AND label text).
        if let Some(key) = node.data_key.as_deref().filter(|k| !k.is_empty()) {
            if let Some(cmd) = filter_cmd_from_key(key) {
                return Some(cmd);
            }
        }
        // Static controls pressed on their own box (or a child with no payload).
        match node.id.as_deref() {
            Some("resetFilters") => return Some(FilterCmd::Reset),
            Some("pruneHidden") => return Some(FilterCmd::Prune),
            Some("nsAllBtn") => return Some(FilterCmd::AllNamespaces),
            Some("nsDeep") => return Some(FilterCmd::FlipNsDeep),
            Some("hideEdgeLabels") => return Some(FilterCmd::FlipVtMode),
            Some("hideIsolated") => return Some(FilterCmd::FlipHideIsolated),
            Some("hideChildren") | Some("kindChildWrap") => {
                return Some(FilterCmd::FlipGroupChildren)
            }
            Some("focusParents") => return Some(FilterCmd::FlipFocusParents),
            Some("kindEdgeWrap") => return Some(FilterCmd::FlipGroupEdges),
            _ => {}
        }
    }
    None
}

/// Decode the `filter:*` / chip `data-key` payload into its [`FilterCmd`].
///
/// The one decoder for the wire spelling, shared by the live resolver and the
/// tests, so a key the shell writes and the key the host reads cannot drift.
pub fn filter_cmd_from_key(key: &str) -> Option<crate::actions::FilterCmd> {
    use crate::actions::FilterCmd;
    if let Some(kind) = key.strip_prefix("facet:") {
        return (!kind.is_empty()).then(|| FilterCmd::ToggleKind(kind.to_string()));
    }
    if let Some(kind) = key.strip_prefix("edgekey:") {
        return (!kind.is_empty()).then(|| FilterCmd::ToggleEdge(kind.to_string()));
    }
    if let Some(project) = key.strip_prefix("proj:") {
        return (!project.is_empty()).then(|| FilterCmd::ToggleProject(project.to_string()));
    }
    Some(match key {
        "filter:kindsAll" => FilterCmd::AllKinds(true),
        "filter:kindsNone" => FilterCmd::AllKinds(false),
        "filter:edgesAll" => FilterCmd::AllEdges(true),
        "filter:edgesNone" => FilterCmd::AllEdges(false),
        "filter:nsAll" => FilterCmd::AllNamespaces,
        "filter:nsDeep" => FilterCmd::FlipNsDeep,
        "filter:hideEdgeLabels" => FilterCmd::FlipVtMode,
        "filter:hideIsolated" => FilterCmd::FlipHideIsolated,
        "filter:focusParents" => FilterCmd::FlipFocusParents,
        "filter:groupChildren" | "filter:hideRelations" => FilterCmd::FlipGroupChildren,
        "filter:groupEdges" => FilterCmd::FlipGroupEdges,
        // The Projects block's `all`/`none` links: v2 `setAllProjects(on)`
        // (`filters.js:80-96`) against the host-mirrored project universe.
        "filter:projAll" => FilterCmd::AllProjects(true),
        "filter:projNone" => FilterCmd::AllProjects(false),
        _ => return None,
    })
}

/// `#foo` for a DOM id. The registry's selectors are all id selectors, so this is
/// the one place the two spellings are reconciled.
fn selector_of_id(id: &str) -> Option<&'static str> {
    CONTROLS.iter().map(|c| c.selector).find(|s| s.strip_prefix('#') == Some(id))
}

/// Resolve a point to a registered control selector, given the frame geometry.
///
/// **Precedence: the shell's own hit-test first, the `--boxes` map as fallback.**
///
/// The invariant is that *the answer comes from the same layout that painted the
/// pixels*, and the shell is the only party that knows which element of the tree
/// it laid out is actually painted at a point. It knows that because it did the
/// paint: the shell's answer accounts for clipping, overflow, stacking and hit
/// testing in one place, whereas `--boxes` is a flat map of one rect per
/// registered selector and therefore cannot represent occlusion at all.
///
/// Why the switch is justified — this is the measurement, not an assumption.
/// Driving every one of the 49 registered
/// controls' own box centres through both answers on the shipped document at
/// 1600x1000 (`nzk_ui --serve`, boxes and hits from one layout pass):
///
///   * 25 controls have a non-zero box (a point a pointer can be at) — 23 are
///     hit directly, 2 land on a CHILD (`#shortcutsBtn` -> `svg.shortcuts-gear`,
///     `#nsSelect` -> `<option>`), and **the two answers disagree on 0 of them**;
///   * 225 further probes (9 interior samples per non-zero control) — agreement
///     again, 0 disagreements;
///   * 24 controls have a zero-area box because they are not laid out on a
///     closed drawer/modal. Their "centre" is `(0,0)`, where the shell's own
///     hit-test correctly answers `#previewModal`/`.topbar`; `hit_in` answers
///     whatever control happens to share that corner. That disagreement is an
///     artefact of pressing a point that is not inside the control, not a real
///     misroute, and the shell's answer is the correct one at that point.
///
/// The `--boxes` map stays as the fallback because it is still the same layout
/// and it still answers when the shell supplied no chain for this point
/// (no `--serve` child, or a frame whose `@hit` rows were truncated).
///
/// When the shell DID answer and the two disagree, the shell's topmost hit wins
/// and the overlap is reported once to stderr. The flat map cannot model
/// stacking or occlusion. A proven paint/geometry mismatch is still refused by
/// the drift guard below.
///
/// Refuses to answer at all when the geometry has been proven to drift from the
/// painted pixels, so a click can never be routed through a stale layout. The
/// guard is evaluated first and is not weakened by the shell path: a drifted
/// frame returns `None` even with a perfect shell chain.
pub fn resolve_selector(
    geo: &FrameGeometry,
    chain: &[HitNode],
    x: f32,
    y: f32,
) -> Option<&'static str> {
    if geo.drift.is_some() {
        return None;
    }
    let from_shell = resolve_hit_chain(chain);
    let from_boxes = hit_in(&geo.boxes, x, y);
    if from_shell.is_none()
        && chain.iter().any(HitNode::is_from_shell)
        && from_boxes.is_some_and(|selector| !is_modal_control(geo, selector))
        && active_modal(geo).is_some()
    {
        return None;
    }
    match (from_shell, from_boxes) {
        // The shell hit-test knows stacking and occlusion; the flat box map does
        // not. When an overlay covers a registered control, the shell's target is
        // the actual press target and must win. `geo.drift` above still refuses
        // clicks when the painted frame and geometry are proven different.
        (Some(s), Some(b)) if s != b => {
            warn_disagreement(s, b, x, y);
            Some(s)
        }
        // The shell's own hit-test is authoritative when it has an answer.
        (Some(s), _) => Some(s),
        // Fallback: the shell supplied no chain for this point.
        (None, b) => b,
    }
}

fn active_modal(geo: &FrameGeometry) -> Option<&'static str> {
    ["#modal", "#shortcutsModal", "#editModal", "#previewModal", "#reorgModal"]
        .into_iter()
        .find(|selector| geo.boxes.get(*selector).is_some_and(|rect| rect.width() > 0.0 && rect.height() > 0.0))
}

fn is_modal_control(geo: &FrameGeometry, selector: &'static str) -> bool {
    let Some(modal) = active_modal(geo) else { return true };
    let belongs = match modal {
        "#modal" => matches!(selector,
            "#modalClose" | "#modalCancel" | "#modalUp" | "#modalHome" | "#modalRoot"
                | "#modalRepo" | "#modalGo" | "#modalSelect" | "#modalPathInput"),
        "#shortcutsModal" => matches!(selector, "#shortcutsClose" | "#shortcutsDone"),
        "#editModal" => selector.starts_with("#edit"),
        "#previewModal" => selector.starts_with("#preview"),
        "#reorgModal" => selector.starts_with("#reorg"),
        _ => false,
    };
    belongs && geo.boxes.get(selector).is_some_and(|rect| rect.width() > 0.0 && rect.height() > 0.0)
}

/// Record, once per distinct disagreement, that the shell's hit-test and the
/// flat `--boxes` map name different controls for the same point. This is
/// diagnostic only: the shell hit-test accounts for stacking and occlusion.
///
/// Only reachable when a chain with shell provenance was supplied AND the two
/// answers differ, so it is silent on every frame measured so far. It exists so
/// that a future layout which breaks the agreement cannot do so quietly — and it
/// The shell answer remains authoritative; a modal backdrop covering an
/// underlying control is an expected disagreement, not a misroute.
fn warn_disagreement(shell: &'static str, boxes: &'static str, x: f32, y: f32) {
    use std::cell::RefCell;
    thread_local! {
        static SEEN: RefCell<BTreeSet<(&'static str, &'static str)>> =
            RefCell::new(BTreeSet::new());
    }
    let first = SEEN.with(|s| s.borrow_mut().insert((shell, boxes)));
    if first {
        eprintln!(
            "[nzk_ui] hit-test overlap at ({x:.1},{y:.1}): shell target {shell}, \
             flat --boxes target {boxes}; routing to the shell's topmost target"
        );
    }
}

/// [`resolve_selector`] against the geometry of the nzk_ui frame currently on
/// screen, with no DOM chain (box containment only).
///
/// Returns `None` while the frame's geometry is known to drift from its pixels,
/// so a click is dropped rather than misrouted.
///
/// The drift check was documented here for a long time before it existed: the
/// body read `boxes` and answered even on a frame whose pixels provably came
/// from a different layout. It was unreachable (zero call sites) so it never
/// misrouted a real click, but a doc that promises a safety property the code
/// does not implement is a defect in its own right — the next caller would have
/// inherited a silent misroute. `resolve_selector` is the production path and
/// always checked; this now matches it.
pub fn hit_selector(x: f32, y: f32) -> Option<&'static str> {
    hit_selector_in(&geometry()?, x, y)
}

/// The pure half of [`hit_selector`]: box containment that refuses a frame whose
/// geometry is known to drift from its pixels.
///
/// Split out so the refusal is testable without the thread-local frame.
fn hit_selector_in(g: &FrameGeometry, x: f32, y: f32) -> Option<&'static str> {
    if g.drift.is_some() {
        return None;
    }
    hit_in(&g.boxes, x, y)
}

/// The geometry of the nzk_ui frame currently on screen — the same one whose
/// pixels were uploaded as the background texture.
///
/// This is the single accessor both the hit-test and any shell-aware caller use,
/// so there is exactly one layout per frame. `None` before the first frame has
/// been built.
pub fn geometry() -> Option<FrameGeometry> {
    NZK_UI.with(|slot| {
        slot.borrow().as_ref().map(|c| FrameGeometry {
            boxes: c.boxes.clone(),
            size: c.size,
            paint_digest: c.paint_digest.clone(),
            drift: c.drift.clone(),
        })
    })
}

/// Record that the frame's `--boxes` geometry provably disagrees with the pixels
/// that were painted, so hit-testing must stop instead of misrouting.
///
/// Returns `true` the first time a given reason is recorded, so the caller can
/// log it once rather than every frame.
pub fn note_drift(reason: &str) -> bool {
    NZK_UI.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(c) = slot.as_mut() else { return false };
        if c.drift.is_some() {
            return false;
        }
        c.drift = Some(reason.to_string());
        true
    })
}

/// Clear a recorded drift (the next successful correspondence check).
pub fn clear_drift() {
    NZK_UI.with(|slot| {
        if let Some(c) = slot.borrow_mut().as_mut() {
            c.drift = None;
        }
    })
}

/// Which surface a point lands on. The host needs this to route an event to the
/// nzk_ui, to the sidebar's own scroll, or to the native source pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// The native source pane (`#sourcePane`), the DOM-computed pane rect.
    SourcePane,
    /// The sidebar (`.side`) — its scroll container owns the wheel.
    Sidebar,
    /// The graph canvas (`#graph`).
    Canvas,
    /// NzkUi that is none of the above.
    NzkUi,
}

/// Classify a point against the live DOM geometry.
pub fn region_at(x: f32, y: f32) -> Region {
    let inside = |sel: &str| box_rect(sel).map(|r| r.contains(Pos2::new(x, y))).unwrap_or(false);
    // The pane wins over the canvas: it is a sibling grid row, and a stale
    // frame must never route a pane click to the graph.
    if inside("#sourcePane") {
        return Region::SourcePane;
    }
    if inside(".side") {
        return Region::Sidebar;
    }
    if inside("#graph") || inside("#canvasWrap") {
        return Region::Canvas;
    }
    Region::NzkUi
}

/// The nzk_ui layer is on unless `CSVIEWER_NO_NZK_UI=1|true` (legacy name:
/// `CSVIEWER_NO_CHROME`).
pub fn enabled() -> bool {
    let off = |key: &str| {
        matches!(
            std::env::var(key).as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        )
    };
    !(off("CSVIEWER_NO_NZK_UI") || off("CSVIEWER_NO_CHROME"))
}

/// The facts document the frame path renders from — **the one place that
/// decides which builder production uses.**
///
/// task-16 was precisely a defect in this decision: `facts_json_with_settings`
/// existed and was correct, but `paint_in` called the plain `facts_json_with`,
/// so the Layout readout spans kept their shipped `index.html` literals while
/// tests of the (unused) builder passed. Naming the step makes the decision
/// itself testable: `the_frame_facts_builder_is_the_settings_aware_one` calls
/// THIS function, so reverting its body to `facts_json_with` turns that test
/// red instead of leaving the suite green over a dead feature.
///
/// Why the settings-aware builder is the right default here:
/// `facts_json_with_settings` omits the `settings` key when the settings equal
/// `LayoutSettings::default()`, so with the sliders untouched the document is
/// byte-identical to the plain builder's — and the moment one moves, nzk_ui's
/// `apply_settings` writes the five spans (`shell.js:807-816`).
fn frame_facts(
    graph: &Graph,
    state: &NzkUiState,
    settings: crate::graphviz::LayoutSettings,
) -> serde_json::Value {
    facts_json_with_settings(graph, state, settings)
}

/// Get-or-build the nzk_ui for this `ui`'s size and paint it across the frame.
///
/// Returns `false` (and the caller falls back to the hand-ported egui nzk_ui)
/// when nzk_ui is unavailable or fails. The expensive part happens only on
/// the first call and on an explicit size change.
pub fn paint(ui: &mut egui::Ui, graph: &Graph, state: &NzkUiState) -> bool {
    let frame = ui.max_rect();
    // No `GraphView` here, so no live settings or visible set to publish: the
    // default settings and the kept-node count are the honest values for this
    // legacy entry point. A host that owns a view should call `paint_in`.
    paint_in(
        ui,
        graph,
        graph.nodes.len(),
        frame,
        state,
        crate::graphviz::LayoutSettings::default(),
    )
}

/// Paint the nzk_ui from the host's authoritative `actions::NzkUiState`.
///
/// `state` is **required**: an earlier revision defaulted it, so a section
/// toggle mutated the host state correctly and then rendered from a throwaway
/// default — the facts, and therefore the pixels, could never change. There is
/// deliberately no state-less overload.
///
/// The nzk_ui is always rasterised at the **full window** size and painted over
/// the full window. `frame` is the rect the app is giving the nzk_ui; when it is
/// shorter than the window the source pane is open, and the full-size render is
/// laid out for the real pane grid (`.layout.with-source`, `[1fr, 34vh]`) rather
/// than a hand-cut frame. The native pane is then drawn by the app over the
/// DOM-computed `#sourcePane` rect.
pub fn paint_in(
    ui: &mut egui::Ui,
    graph: &Graph,
    nodes_in_view: usize,
    frame: Rect,
    state: &NzkUiState,
    settings: crate::graphviz::LayoutSettings,
) -> bool {
    if !enabled() {
        return false;
    }
    let full = ui.max_rect();
    // The app gives a shorter rect exactly when the pane is open.
    let pane_open = frame.height() + 0.5 < full.height();
    let w = full.width().round().max(1.0) as u32;
    let h = full.height().round().max(1.0) as u32;
    PRESENT.with(|p| {
        let mut p = p.borrow_mut();
        if p.pane_open != pane_open {
            p.pane_open = pane_open;
        }
    });
    let pres = presentation();
    let status = status_text(graph);
    let mut facts = frame_facts(graph, state, settings);
    // The canvas notice count is `viewNodes` in v2 — the nodes the canvas
    // actually draws after filters/collapse (`shell.js updateCanvasNotice`),
    // NOT `state.counts.nodes` and not every kept node. `facts_json_with` has
    // no `GraphView`, so the host supplies the live `vis.len()` here.
    facts["nodes_in_view"] = serde_json::json!(nodes_in_view);
    // With `--facts` the rendered status comes from the JSON fields, so the
    // measurement override must be written there too, not only into `status`.
    if let Ok(s) = std::env::var("CSVIEWER_STATUS_OVERRIDE") {
        if !s.is_empty() {
            facts["status_override"] = serde_json::Value::String(s);
        }
    }

    // The repaint key is the *facts document itself*, not a hand-maintained
    // subset of it: every host-visible change (sidebar chip on/off, namespace,
    // ns-deep, slider readouts, open drawers/modals, Find rows, tabs, ...) is
    // already in these bytes, so it cannot be forgotten the way the
    // size/status/collapsed list was.
    let facts_digest = facts_digest(&facts);

    NZK_UI.with(|slot| {
        let mut slot = slot.borrow_mut();
        let collapsed = state.collapsed_facts().iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let stale = match &*slot {
            Some(c) => {
                c.size != (w, h)
                    || c.status != status
                    || c.collapsed != collapsed
                    || c.notice_nodes != nodes_in_view
                    || c.pane_open != pres.pane_open
                    || (c.side_scroll - pres.side_scroll).abs() >= 0.5
                    || c.facts_digest != facts_digest
            }
            None => true,
        };
        if stale {
            match build(ui.ctx(), &graph.repo, &status, &facts, w, h) {
                Ok(mut c) => {
                    // Record the key this frame was built from. `build` cannot
                    // know it, and leaving these at their constructors' zeros
                    // made every frame with a non-empty host list permanently
                    // stale — the idle repaint storm (task-35).
                    c.collapsed = collapsed.clone();
                    c.pane_open = pres.pane_open;
                    c.side_scroll = pres.side_scroll;
                    c.notice_nodes = nodes_in_view;
                    c.facts_digest = facts_digest;
                    eprintln!(
                        "[native-csviewer] nzk_ui: {} painted {}x{} in {}ms · pane {} · status: {}",
                        c.bin.display(),
                        w,
                        h,
                        c.build_ms,
                        if pres.pane_open { "open" } else { "closed" },
                        c.status
                    );
                    // The engine reports the sidebar's real scroll extent.
                    set_presentation(Presentation {
                        side_scroll_max: c.side_scroll_max,
                        ..pres
                    });
                    *slot = Some(c);
                }
                Err(e) => {
                    warn_once(&e);
                    return false;
                }
            }
        }
        let c = slot.as_ref().expect("nzk_ui built above");
        ui.painter().image(
            c.tex.id(),
            full,
            Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
            Color32::WHITE,
        );
        true
    })
}

/// Everything nzk_ui needs about the app's corpus, in the shape of its own
/// `dom::Facts` (so `serde_json::from_str::<Facts>` accepts it directly).
///
/// `nzk_ui` is a static rasteriser and has no engine; without this it falls
/// back to its hard-coded fixture, which is how the nzk_ui came to say
/// "nodes 9 / All namespaces (2)" while the graph drew 90 nodes.
pub fn facts_json(graph: &Graph) -> serde_json::Value {
    facts_json_with(graph, &NzkUiState::default())
}

/// As [`facts_json`], but the nzk_ui's view state comes from the host's
/// authoritative `actions::NzkUiState` (its collapsed list) plus the nzk_ui's
/// own presentation state (pane-open, sidebar scroll).
pub fn facts_json_with(graph: &Graph, state: &NzkUiState) -> serde_json::Value {
    // v2's facet order comes straight from the engine: `/api/kinds` inserts each
    // kind in the engine's node order and then `sort_by_key(Reverse(count))`,
    // Rust's *stable* sort, so equal counts keep first-appearance order
    // (`rust-only-csviewer/core/src/index.rs::kinds`). `filters.js` appends the
    // rows in that payload order and never re-sorts. A `BTreeMap` here silently
    // replaced the tie-break with name order, which is why `namespace` was drawn
    // above `string` (both 2) while the reference has `string` first.
    let mut kinds: Vec<(String, usize)> = Vec::new();
    let mut namespaces: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut ns_nodes = 0usize;
    for n in &graph.nodes {
        match kinds.iter_mut().find(|(k, _)| *k == n.kind) {
            Some((_, c)) => *c += 1,
            None => kinds.push((n.kind.clone(), 1)),
        }
        if n.kind == "namespace" {
            ns_nodes += 1;
        }
        if !n.ns.is_empty() {
            namespaces.insert(n.ns.clone());
        }
    }
    // v2's sidebar kind histogram is built from every node the engine reports,
    // not only the ones this view draws: `state.counts` has the raw totals and
    // the reference session's sidebar reads "9 kinds" with `external 59` — the
    // 59 is exactly `graph.skipped`'s `external` entry. Without this merge the
    // sidebar said "8 kinds" and `external` was missing (round-19 audit), which
    // also shifted every row below it (pixel-audit NEW-1 vertical drift).
    // `graph.state.counts.externals` is the same number; `skipped` is used so
    // the row and the side panel's "not drawn" note come from one source.
    for (kind, count) in &graph.skipped {
        match kinds.iter_mut().find(|(k, _)| k == kind) {
            Some((_, c)) => *c += *count,
            None => kinds.push((kind.clone(), *count)),
        }
    }
    // Stable: equal counts keep the engine's node order, exactly as v2's
    // `/api/kinds` + `filters.js` does. Never add a name tie-break here.
    let mut kind_rows: Vec<(String, usize)> = kinds;
    kind_rows.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    // The namespace facet counts the namespaces the corpus declares; fall back
    // to namespace-kind nodes when the engine reports no `ns` strings.
    let ns_count = if namespaces.is_empty() { ns_nodes } else { namespaces.len() };
    let projects = project_rows(graph, &state.filters);
    let warnings = warning_rows(graph);
    // Derived from the rows actually emitted, never a literal: the sidebar's
    // count and its warning list are the same engine data, so they cannot
    // disagree. A corpus with no warnings yields 0 *because the engine reported
    // none*, which is the honest reading of 0.
    let parser_warning_count = warnings.len();
    let warning_json: Vec<serde_json::Value> = warnings.iter().map(|w| w.to_json()).collect();
    let projects_empty_text = if projects.is_empty() { "no .sln/.csproj found" } else { "" };
    let mut obj = serde_json::json!({
        "repo": graph.repo.to_string_lossy(),
        "status_override": serde_json::Value::Null,
        "kinds": kind_rows,
        "ns_count": ns_count,
        "indexed": graph.state.get("indexed").and_then(|v| v.as_bool()).unwrap_or(true),
        "file_count": graph.state.get("fileCount").and_then(|v| v.as_u64()).unwrap_or(0),
        "parse_ms": graph.state.get("parseMs").and_then(|v| v.as_u64()).unwrap_or(0),
        "node_count": graph
            .state
            .get("counts")
            .and_then(|c| c.get("nodes"))
            .and_then(|v| v.as_u64())
            .unwrap_or(graph.nodes.len() as u64),
        "edge_count": graph
            .state
            .get("counts")
            .and_then(|c| c.get("edges"))
            .and_then(|v| v.as_u64())
            .unwrap_or(graph.edges.len() as u64),
        // The chip text is the renderer's *empty* state, so it is only sent
        // when there are genuinely no projects; a corpus with projects sends
        // an empty string and the real rows take over.
        "projects_empty_text": projects_empty_text,
        "projects": projects,
        "warnings": warning_json,
        "ui": ui_state_json(state, Some(graph)),
        "parser_warning_count": parser_warning_count,
        // The canvas draws its own "N nodes in view" toast over the nzk_ui, so
        // this value is never visible; keep it equal to the graph size.
        "nodes_in_view": graph.nodes.len(),
    });
    // The hierarchy panel (`hierarchy.js`) is a **top-level `Facts` field**, not
    // a member of `ui`: `nzk_ui/src/dom.rs:920` declares
    // `pub hierarchy: Option<HierarchyFacts>` as a sibling of `ui`, so the
    // insert has to land on the outer object. Injecting it under `ui` instead
    // would be silently ignored — the deserialiser simply never looks there,
    // and every `HierarchyFacts` field is `#[serde(default)]`, so the panel
    // would come out empty AND the mistake would be invisible.
    //
    // Absent rather than `null` while the panel is closed: both deserialize to
    // `Option::None`, but only the absent key leaves the shipped document
    // untouched. `json!` would spell a `None` as `null`, so — like `find` and
    // `picker` in `ui_state_json` — the key is inserted conditionally.
    if let Some(v) = state.hierarchy.dom_facts() {
        obj["hierarchy"] = v;
    }
    // The block-hover peek tooltip (`#blocksPeekKind/Name/File/Src/Refs`,
    // `scratch.js:424-454`). TOP-LEVEL, a sibling of `ui`: `dom.rs` declares
    // `Facts::blocks_peek` on the OUTER struct, and serde does not
    // `deny_unknown_fields`, so a key nested under `ui` would deserialize to
    // `None` and every span would stay silently inert while looking wired.
    //
    // Absent rather than `null` while nothing is hovered: the tooltip keeps its
    // shipped `class="blocks-peek hidden"` and the pinned default frame is
    // untouched. `Peek::visible()` is the one owner of that state.
    if state.blocks_peek.visible() {
        let v = state.blocks_peek.view();
        obj["blocks_peek"] = serde_json::json!({
            "visible": true,
            "kind": v.kind,
            "name": v.name,
            "file": v.file,
            "src": v.src,
            "refs": v.refs,
        });
    }
    // The edit modal's CONTENT (`#editTool`/`#editArgs`/`#editOut`/`#editNote`),
    // for the `#editBtn` flow. A top-level `Facts` field, a sibling of `ui` —
    // `nzk_ui/src/dom.rs` declares `pub edit: Option<EditFacts>` on the OUTER
    // struct, and serde does not `deny_unknown_fields`, so a key nested one level
    // deeper deserializes to `None` and every control in the dialog stays inert
    // while looking wired. Same trap, same shape as `pub_defs` below.
    //
    // Absent rather than `null` while the modal is not the open surface: both
    // deserialize to `None`, but only the absent key leaves the shipped document
    // byte-identical (`#editArgs` keeps `{}`, `#editOut`/`#editNote` stay empty).
    // `edit_facts` is gated on `NzkUiState::surface`, so a `ToggleModal` that
    // closes the dialog removes the key again instead of publishing the contents
    // of an element the shell has just hidden.
    if let Some(v) = state.edit_facts() {
        obj["edit"] = v;
    }
    // The `#pubCount` / `#pubList` readouts, from the ONE engine answer the host
    // took (`Index::definitions("public", N)`). Top-level keys, siblings of `ui`
    // — the same shape as `settings` at :985 and `hierarchy` at :927, and for
    // the same reason: `dom.rs`'s `Facts` declares `pub_defs` as a member of the
    // *outer* struct, and serde does not `deny_unknown_fields`, so a key nested
    // one level too deep would deserialize to `None` and the span would simply
    // never change — a silent, invisible failure.
    //
    // # Why the whole `pub_defs` object is omitted rather than sent empty
    //
    // `None` (no engine query yet) must leave the shipped document alone: the
    // span keeps `index.html`'s empty `<span id="pubCount"></span>` and
    // `#pubList` keeps its inert `<div class="list-empty">open to load</div>`.
    // Sending `{"total": 0}` would display a *fact* — "this corpus has no public
    // definitions" — that nobody measured. So the object is built only when at
    // least one of the two is genuinely known, and the count key inside it is
    // itself omitted when only the list is known.
    if state.pub_total.is_some() || state.pub_list.is_some() {
        let mut defs = serde_json::Map::new();
        if let Some(total) = state.pub_total {
            // `total` is the engine's count of ALL matches, not of the returned
            // page: `definitions()` computes it before `truncate`ing to `limit`
            // (core/src/index.rs:3425-3428), which is what `filters.js:493`
            // reads (`state.defs.total = d.total || …`). The list below is
            // therefore allowed to be shorter than the count — that is the real
            // `limit=400` behaviour, not a disagreement.
            defs.insert("total".into(), serde_json::json!(total));
        }
        if let Some(rows) = &state.pub_list {
            // `renderPubList` (`filters.js:504-512`): the box filters the loaded
            // rows by `#pubSearch` before the 300-row render cap (`:455`).
            let q = state.pub_query.to_lowercase();
            let json: Vec<serde_json::Value> = rows
                .iter()
                .filter(|r| {
                    q.is_empty()
                        || r.full.to_lowercase().contains(&q)
                        || r.label.to_lowercase().contains(&q)
                })
                .take(PUB_LIST_RENDER_CAP)
                .map(|r| r.to_json())
                .collect();
            defs.insert("list".into(), serde_json::json!(json));
        }
        obj["pub_defs"] = serde_json::Value::Object(defs);
    }
    // The Declared types section (`#typeSection`, v3-only, runtime-created by
    // `dom::apply_type_defs`). Always present while a graph exists: unlike
    // `pub_defs` there is no engine round trip to wait for — the declared type
    // (`ret`) is already on every resident node, so the browser can answer
    // immediately from the same model the canvas draws.
    obj["type_defs"] = type_defs_facts(graph, state);
    // The Strings / error codes section (`filters.js:516-534`). Present only
    // after a search has actually run, so the shipped "type to search string
    // literals…" row stays until the user asks.
    let str_has_query = state
        .str_searched
        .as_ref()
        .is_some_and(|(q, _)| !q.trim().is_empty());
    if str_has_query || state.str_rows.as_ref().is_some_and(|r| !r.is_empty()) {
        let mut defs = serde_json::Map::new();
        if let Some((_, hits)) = &state.str_searched {
            defs.insert("total".into(), serde_json::json!(hits));
        }
        if let Some(rows) = &state.str_rows {
            let json: Vec<serde_json::Value> =
                rows.iter().take(PUB_LIST_RENDER_CAP).map(|r| r.to_json()).collect();
            defs.insert("list".into(), serde_json::json!(json));
        }
        obj["str_defs"] = serde_json::Value::Object(defs);
    }
    // GAP B: the `#strCount` / `#chainCount` section-head counters, produced from
    // the one real walk the panel is rendering (`HierarchyView::chain_flat_rows`,
    // the same `sections` `dom_facts` serialises into the body) and, for the
    // strings counter, from whatever strings search has actually run.
    //
    // A top-level `Facts` field, a sibling of `ui` — `dom.rs:1192` declares
    // `pub section_counts: Option<SectionCountFacts>` on the OUTER struct, and
    // serde does not `deny_unknown_fields`, so a key nested one level deeper
    // deserializes to `None` and both spans stay silently empty while looking
    // wired. Same trap, same shape as `pub_defs` above and `hierarchy` at :954.
    //
    // Absent, never `null` and never `{}`, while neither counter is known:
    // `apply_section_counts` is then never entered and the shipped document
    // stays byte-identical (`index.html` ships both `.scount` spans empty).
    // `SectionCounts::to_json` returns `Value::Null` for that state, and this
    // insert is what turns it into "no key at all".
    {
        let str_pair = state
            .str_searched
            .as_ref()
            .map(|(q, hits)| (q.as_str(), *hits));
        let chain_pair = state.chain.as_ref().and_then(|d| {
            if d.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
                d.get("flat").and_then(serde_json::Value::as_array).map(|flat| {
                    (
                        flat.len(),
                        d.get("truncated")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false),
                    )
                })
            } else {
                None
            }
        });
        let counts =
            crate::app::section_counts::SectionCounts::from_engine_answers(str_pair, chain_pair);
        let v = counts.to_json();
        if !v.is_null() {
            obj["section_counts"] = v;
        }
    }
    obj
}

/// `renderListRows` renders at most 300 rows (`filters.js:455`
/// `items.slice(0, 300).forEach(…)`), whatever the engine's `limit` was.
pub const PUB_LIST_RENDER_CAP: usize = 300;

/// The Layout ▸ slider values, in the shape the readout spans need
/// (`syncLayoutControls`, `shell.js:807-816`).
///
/// # Why this is a function and not a key inside [`facts_json_with`]
///
/// The settings live on `GraphView`, not on [`NzkUiState`]: `facts_json_with`
/// only ever sees the latter, so it genuinely *cannot* read them. Rather than
/// invent a getter on `NzkUiState` that would have to be kept in sync by hand
/// (and could therefore disagree with the view), the values are produced from
/// the one authority — `GraphView::settings()` — by [`slider_facts`], and the
/// host publishes the result.
///
/// # Status: published, not yet consumed
///
/// `dom::Facts` has no `settings` field, and `nzk_ui/src/*` is outside this
/// task's write scope. Serde does not `deny_unknown_fields`, so a `settings` key
/// on the facts object is currently **ignored** by the rasteriser: the readout
/// spans keep the literals in `index.html`. Closing the loop needs exactly one
/// more change in nzk_ui —
///
/// ```text
/// pub settings: Option<SliderFacts>,          // nzk_ui/src/dom.rs::Facts
/// ```
///
/// — plus a pass that writes `set_text(#spacingOut, ...)` and friends. That is
/// reported as a remaining gap, not presented as wired. The formatting matches
/// the reference's own readouts: `curvature` is shown to two decimals
/// (`shell.js:813`), the rest as integers.
pub fn slider_facts(s: crate::graphviz::LayoutSettings) -> serde_json::Value {
    serde_json::json!({
        "spacing": format!("{}", s.spacing.round() as i64),
        "iterations": format!("{}", s.iterations),
        "curvature": format!("{:.2}", s.curvature),
        "nodeSize": format!("{}", s.node_size.round() as i64),
        "textSize": format!("{}", s.text_size.round() as i64),
    })
}

/// The repaint key for one facts document: a digest over the **whole**
/// serialised facts body the rasteriser is about to render.
///
/// `paint_in` used to compare a hand-listed subset (size, status, the collapsed
/// list, the canvas notice count, pane-open, sidebar scroll). A field that was
/// not in that list could change and the nzk_ui would never be rebuilt — which
/// is exactly what happened to the sidebar filter model (`ui.filters`: chip
/// on/off, namespace, `ns_deep`), the Layout readouts (`settings`) and the
/// overlay/drawer/tab facts. Digesting the document makes every fact a key by
/// construction: a host-visible change cannot be left out of the repaint key
/// again, and an unchanged document still compares equal (no repaint storm).
///
/// `DefaultHasher::new()` is deterministic within a process (fixed keys), and a
/// repaint key only ever needs to be valid for the process that compares it.
pub fn facts_digest(facts: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    facts.to_string().hash(&mut h);
    h.finish()
}

/// [`facts_json_with`] plus the Layout ▸ slider values.
///
/// The host calls this instead of `facts_json_with` when it wants the readouts
/// published; it is a separate entry point so no existing caller or test has to
/// change, and so a host that has no view can keep using the old one.
pub fn facts_json_with_settings(
    graph: &Graph,
    state: &NzkUiState,
    settings: crate::graphviz::LayoutSettings,
) -> serde_json::Value {
    let mut obj = facts_json_with(graph, state);
    // Inserted only when it differs from the shipped defaults: with the sliders
    // untouched this leaves the serialised object exactly as it was, which is
    // what keeps the default `@frame` and every existing facts test stable.
    if settings != crate::graphviz::LayoutSettings::default() {
        obj["settings"] = slider_facts(settings);
    }
    obj
}

/// Project chips for the Filters section — `filters.js` buildProjectFilter.
///
/// Serialised into nzk_ui's `ProjectRow` shape. The engine spells the count
/// `fileCount` while the renderer's field is `file_count`, so the rows are
/// built by [`crate::model::project_rows_from_payload`] from the graph's real
/// project model rather than passed through as opaque JSON — a passthrough
/// would leave the renderer's `file_count` at its serde default of 0.
fn project_rows(graph: &Graph, filters: &crate::actions::FilterState) -> Vec<serde_json::Value> {
    graph
        .projects
        .iter()
        .map(|p| {
            let mut row = p.to_json();
            // `filters.js`: a chip is OFF when its id is in `state.filters.projects`
            // (the engine's own project model is the row source, the model is the
            // paint source — one owner each).
            row["on"] = serde_json::json!(!filters.hidden_projects.contains(&p.id));
            row
        })
        .collect()
}

/// Parser-warning rows — `problems.js` rowEl.
///
/// The rows come from the engine's own installed warning list (`state.warnings`,
/// what `/api/state` reports), with file/line/col recovered from each warning
/// line where it actually carries them. Warnings the engine recorded without a
/// position stay position-free and are shown as text — nzk_ui's renderer
/// guards on `!file.is_empty() && line > 0`, so no dead link is produced.
fn warning_rows(graph: &Graph) -> Vec<crate::model::WarningRow> {
    crate::model::warning_rows_from_state(&graph.state)
}

/// Case-insensitive **subsequence** test — the quick-open fuzzy rule, shared by
/// the declared-type chips and rows.
///
/// A subsequence (not a substring) is what makes `str` match `string`,
/// `System.String` and `strct` match `struct` while still being cheap and total.
/// Order matters and every query character must occur, so it never returns a
/// match for a character the type does not contain.
fn type_fuzzy(hay: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let mut want = query.chars().flat_map(|c| c.to_lowercase());
    let Some(mut cur) = want.next() else { return true };
    for c in hay.chars().flat_map(|c| c.to_lowercase()) {
        if c == cur {
            match want.next() {
                Some(n) => cur = n,
                None => return true,
            }
        }
    }
    false
}

/// True when every whitespace-separated token of `query` matches the declared
/// type **or** the declaration's label. Typing `str parse` narrows to
/// declarations declared `string`-ish that also have "parse" in the name;
/// tokens are ANDed, which is what makes a multi-word query useful.
fn type_query_matches(node: &crate::model::Node, query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    q.split_whitespace().all(|tok| {
        type_fuzzy(&node.ret, tok)
            || type_fuzzy(&node.label, tok)
            || type_fuzzy(&node.full, tok)
    })
}

/// Is `node` in the sidebar's selected namespace (honouring Include
/// sub-namespaces)? The exact predicate `GraphView::filter_passes` uses, kept in
/// one shape here because the facts builder has no `GraphView`.
fn in_selected_namespace(node: &crate::model::Node, ns: &str, deep: bool) -> bool {
    if ns.is_empty() {
        return true;
    }
    node.ns == ns
        || (deep
            && node
                .ns
                .strip_prefix(ns)
                .map(|rest| rest.starts_with('.'))
                .unwrap_or(false))
}

/// The Declared types section's facts.
///
/// Answers the owner's ask directly: *all declared types under one namespace*
/// (`types` = the chips, each with a count), and *only declarations of one
/// declared type* (`rows`, restricted by the active chip). The input is a fuzzy
/// filter over both the type name and the declaration name.
///
/// The row projection reuses [`crate::model::PubRow`] so the renderer has one
/// row shape and one click path (`data-key` = node id): `.kind` carries the
/// **declared type**, not the node kind, because that is the column this view
/// exists to show. Rows are ordered by declared type, then name, then id — a
/// total order, so two identical facts documents cannot paint differently.
fn type_defs_facts(graph: &Graph, state: &NzkUiState) -> serde_json::Value {
    let ns = state.filters.namespace.trim();
    let deep = state.filters.ns_deep;
    let query = state.type_query.trim();
    let selected = state.type_filter.as_deref().filter(|s| !s.is_empty());

    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut rows: Vec<crate::model::PubRow> = Vec::new();
    for n in &graph.nodes {
        // `ret` empty ⇒ no declared type (namespace, class, constructor). Those
        // are not "declared as <type>" and belong in no chip and no row.
        if n.ret.is_empty() {
            continue;
        }
        if n.kind == "string" || n.kind == "namespace" || n.kind == "external" {
            continue;
        }
        if !in_selected_namespace(n, ns, deep) || !type_query_matches(n, query) {
            continue;
        }
        match counts.iter_mut().find(|(t, _)| t == &n.ret) {
            Some((_, c)) => *c += 1,
            None => counts.push((n.ret.clone(), 1)),
        }
        if let Some(want) = selected {
            if n.ret != *want {
                continue;
            }
        }
        rows.push(crate::model::PubRow {
            id: n.id.clone(),
            full: if n.full.is_empty() { n.id.clone() } else { n.full.clone() },
            label: if n.label.is_empty() { n.full.clone() } else { n.label.clone() },
            kind: n.ret.clone(),
        });
    }
    // Count descending, then name: the most common declared type is what a
    // reader wants first, and the name tie-break keeps the order total.
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.id.cmp(&b.id))
    });
    let total = rows.len();
    let rows: Vec<serde_json::Value> = rows
        .iter()
        .take(PUB_LIST_RENDER_CAP)
        .map(|r| r.to_json())
        .collect();
    let chips: Vec<serde_json::Value> = counts
        .iter()
        .map(|(name, count)| {
            serde_json::json!({
                "name": name,
                "count": count,
                "selected": selected == Some(name.as_str()),
            })
        })
        .collect();
    serde_json::json!({
        "total": total,
        "types": chips,
        "rows": rows,
        "query": state.type_query,
        "scope": ns,
    })
}

/// The host-owned view state in nzk_ui's `UiState` shape.
///
/// `graph` is what the Find panel searches, so it is only needed when there is a
/// query to run. It is `Option` because the default-render tests build the facts
/// with no graph at all — and with no graph there is nothing to search, which is
/// exactly the state that must leave `find` absent and the paint byte-identical.
fn ui_state_json(state: &NzkUiState, graph: Option<&Graph>) -> serde_json::Value {    let collapsed = state.collapsed_facts();
    let p = presentation();
    // `null` = leave the shipped `index.html` alone. An empty host list is NOT
    // the shipped state (the document ships three sections collapsed), so it is
    // sent as `null` too; the host seeds its default once it wants to own it.
    let collapsed_json = if collapsed.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!(collapsed)
    };
    let scroll = if p.side_scroll > 0.0 {
        serde_json::json!([{ "selector": ".side", "x": 0.0, "y": p.side_scroll }])
    } else {
        serde_json::json!([])
    };
    let mut obj = serde_json::json!({
        "collapsed": collapsed_json,
        "active_mode": "graph",
        "pane_open": p.pane_open,
        // The open drawer, if any. This is the ONLY thing that gives a drawer a
        // non-zero box: while it is absent the panel keeps its shipped
        // `class="... hidden"` and a `0 0 0 0` row, which no hit-test can reach.
        "drawers_open": state.drawer_facts(),
        // The open modal, if any. Same contract as `drawers_open` and the same
        // one thing that matters: this is the ONLY fact that clears a modal's
        // shipped `hidden` class, and while it is hidden the modal has a
        // `0 0 0 0` row that no hit-test can reach. At most one modal is ever
        // named, because two open backdrops would both cover the viewport.
        "modals_open": state.modal_facts(),
        "scroll": scroll,
        // The `state.filters.groupChildren` flag behind the two inverse
        // checkboxes (`#groupChildren` shows it, `#hideChildren` shows `!it`).
        // Sent as a real boolean rather than conditionally inserted: unlike
        // `find`/`picker`, `false` is a *value* here, not an absence, and
        // `ui_state_json` is only ever reached with a host that owns the view
        // state. The one owner is `GraphView::group_children`
        // (`app/src/view.rs:90`, single writer `set_group_children` at :676);
        // the host copies it into `NzkUiState::group_children` each pass, so
        // the nzk_ui can never disagree with the flag the graph was rebuilt from.
        "group_children": state.group_children,
        // The `state.filters.groupEdges` flag behind `#groupEdges` — the
        // *edges* counterpart of the flag above and a distinct control (the
        // reference keeps two separate boxes, `#groupChildren` and
        // `#groupEdges`; `shell.js:609-612`). Same contract: a real boolean,
        // never conditionally omitted, because `false` is a *value* here rather
        // than an absence. The one owner is `GraphView::group_edges`
        // (`app/src/view.rs`, single writer `set_group_edges`); the host copies
        // it into `NzkUiState::group_edges` each pass, so the nzk_ui can never
        // disagree with the flag the graph was rebuilt from.
        //
        // THE KEY IS camelCase, matching the *filter* spelling this flag has in
        // the reference (`core.js:271`) and the `#[serde(rename = ...)]` the
        // renderer declares. It is the one camelCase key in this otherwise
        // snake_case object; `nzk_ui/src/dom.rs` renames it per-field rather
        // than putting `rename_all` on `UiState`, precisely because every
        // sibling here (`drawers_open`, `modals_open`, ...) is snake_case.
        // Spelling it `group_edges` would be silently inert: serde would ignore
        // the unknown key and the box would never leave its shipped default.
        "groupEdges": state.group_edges,
    });
    // The loading overlay (`#loading` / `#loadingText` / `#stopIndexBtn`). Absent
    // while no build runs, so the shipped `class="loading hidden"` document is
    // untouched and the default frame is byte-identical. While present the shell
    // clears `hidden`, writes the text, and makes Stop clickable.
    if let Some(text) = &state.indexing {
        obj["loading"] = serde_json::json!({ "text": text, "stop": true });
    }
    // The resolve chain (`filters.js:543-660`). Absent until Run has an answer,
    // so the shipped "Select a node, then Run." row is untouched.
    if let Some(chain) = &state.chain {
        obj["chain"] = serde_json::json!({
            "data": chain,
            "flat": state.chain_flat,
            "depth": state.chain_depth,
            "dir": state.chain_dir,
            "collapsed": state.chain_collapsed.iter().cloned().collect::<Vec<_>>(),
        });
    }
    // The sidebar search boxes' typed text (`filters.js:520-530`). Absent while
    // empty, so an untouched document keeps its shipped placeholders.
    if !state.pub_query.is_empty() {
        obj["pub_query"] = serde_json::json!(state.pub_query);
    }
    if !state.str_query.is_empty() {
        obj["str_query"] = serde_json::json!(state.str_query);
    }
    // `#uiTheme`: the whole-viewer theme attribute. Always sent, because the
    // default is a real value the shipped document already carries.
    obj["ui_theme"] = serde_json::json!(state.ui_theme);
    // `#sourceTheme`'s selected option. Omitted at `darkplus` (the shipped
    // default) so the default document keeps its shipped markup.
    if state.source_mode != crate::actions::SourceMode::DarkPlus {
        obj["source_theme"] = serde_json::json!(state.source_mode.label());
    }
    if let Some(source) = &state.source_facts {
        obj["source"] = source.clone();
    }
    // The docked node drawer's CONTENT (`#nodeTitle` / `#nodeProps` /
    // `#relationsBody`), built from the live graph + the mirrored selection.
    // The key is ABSENT, not `null`, while nothing is selected (or the drawer
    // is closed): absent = the shipped `class="drawer hidden"` document is left
    // untouched, which is what keeps `@frame 1600 1000 4bd131fa71b2ccc8`
    // unmoved when no node is selected.
    if state.drawer_facts().contains(&"nodeDrawer") {
        if let Some(graph) = graph {
            if let Some(v) = crate::drawer_wire::facts_for_focus(
                graph,
                state.selected_node,
                &state.drawer_ui,
                state.filters.focus_only,
                state.filters.focus_parents,
            ) {
                obj["drawer"] = v;
            }
        }
    }
    // The change-preview panel's CONTENT (`#previewOut` / `#previewNote`) — a
    // member of `ui`, where `nzk_ui`'s `UiState` declares it.
    if let Some(v) = state.preview_facts() {
        obj["preview"] = v;
    }
    // `find` and `picker` carry CONTENT (`#findResults` rows, `#modalBody`
    // items), not open/close flags, so they are **omitted** rather than sent as
    // `null` when there is nothing to say. The difference is not cosmetic:
    // `null` and "absent" both deserialize into `Option::None`, but only the
    // absent case leaves the shipped document untouched, which is what keeps the
    // default render byte-identical (`@frame 1600 1000 4bd131fa71b2ccc8`).
    // `json!` would serialize `None` as `null`, hence the conditional insert.
    if let Some(mut v) = graph.and_then(|g| {
        state
            .find
            .facts(g, state.drawer_facts().contains(&"findPanel"))
    }) {
        // The engine-backed definitions/implementations/usages groups (v2
        // `renderFind`, search.js:305-351). Additive: a view that has not asked
        // the engine emits no `groups` key, so a shut/old panel stays
        // byte-identical.
        if let Some(groups) = state.find.groups() {
            v["groups"] = groups.clone();
        }
        obj["find"] = v;
    }
    // The Blocks canvas content (task-13): v2 `scratch.js`'s cards, ports and
    // reference edges, derived from the graph the selection lives in. `None` —
    // nothing selected, or no declarations for the anchor — leaves the shipped
    // empty `#blocksStage` untouched, keeping the default render byte-identical.
    //
    // Gated on the Blocks DRAWER being open, exactly like `drawer` and `find`
    // above. The content fact is not an open flag: `nzk_ui::dom::apply_blocks_canvas`
    // un-hides `#blocksPanel` whenever content arrives, so publishing it for every
    // selection made clicking a graph node open the Blocks panel — the wrong
    // panel, and on top of the node drawer the reference opens (`selectNode` ->
    // `loadDetail` + `renderNodeDrawer`, interaction.js:306-320). The reference
    // only routes selection into Blocks while Blocks is already open.
    if state.drawer_facts().contains(&"blocksPanel") {
        if let Some(v) = graph.and_then(|g| {
            let selected = state
                .selected_node
                .and_then(|i| g.nodes.get(i))
                .map(|n| n.id.as_str());
            crate::app::blocks_wire::blocks_facts(g, selected, selected)
        }) {
            obj["blocks"] = v;
        }
    }
    if let Some(v) = state.picker.as_ref().and_then(|pk| pk.facts()) {
        obj["picker"] = v;
    }
    // The Reorganise & export dialog (`#reorgModal`, `reorg.js`). `None` while
    // the dialog has never been opened and no engine run happened, so the
    // shipped document (and the `@frame` token) stays byte-identical. When it
    // is open, the form values are published so the shell paints the controls
    // from the one host-side owner; a finished run adds its report rows and
    // `#reorgNote`.
    if let Some(v) = state
        .reorg
        .facts(state.surface == Some(crate::actions::HostSurface::Export))
    {
        obj["reorg"] = v;
    }
    // The form-control interaction state (task-14): the open select menu and the
    // focused text box. `None` while idle, so no control gains an overlay and the
    // pinned `@frame` token is untouched.
    if let Some(v) = state.form.facts() {
        obj["form"] = v;
    }
    // `#cs-tabs` (`shell.js:1192-1248`): the bar is a DYNAMIC region the
    // reference creates in `install()`, so it is not in `index.html` and the
    // renderer can only build it from these facts. Omitted on a fresh nzk_ui
    // (`state.tabs.is_empty()`), which is what keeps the default paint — and
    // the pinned `@frame` token — byte-identical.
    if !state.tabs.is_empty() {
        let active = state.tabs.active_key();
        let split = state.tabs.split_key();
        let rows: Vec<serde_json::Value> = state
            .tabs
            .tabs()
            .iter()
            .map(|t| {
                // `b.title = (t.params && (t.params.file || t.params.node)) || t.key`
                // (`shell.js:1205`): a non-empty `file` wins over `node`, and the
                // tab key is the fallback.
                let title = t
                    .params
                    .get("file")
                    .and_then(|v| v.as_str())
                    .filter(|f| !f.is_empty())
                    .or_else(|| {
                        t.params
                            .get("node")
                            .and_then(|v| v.as_str())
                            .filter(|n| !n.is_empty())
                    })
                    .unwrap_or(&t.key);
                serde_json::json!({
                    "key": t.key,
                    "label": t.label,
                    "active": active == Some(t.key.as_str()),
                    "split": split == Some(t.key.as_str()),
                    "title": title,
                })
            })
            .collect();
        obj["tabs"] = serde_json::json!({
            "tabs": rows,
            "split_on": state.tabs.split(),
            // `applySplitWidth()` (`shell.js:1471-1472`): the drag/persisted
            // divider fraction becomes the shipped `--cs-split-w` custom
            // property. Published as a real value (0.5 by default) because the
            // reference re-applies it on every `setSplitVisible(true)`; the
            // ABSENT-no-write contract is kept by the consumer for hosts that
            // do not publish one (the shipped facts bake no style).
            "split_w": state.tabs.split_frac(),
        });
    }
    // ---- the topbar input and its graph-mode suggestion popup --------------
    //
    // The box is shared by two modes, so which producer owns it is the mode
    // truth `NzkUiState::surface`: `ModeFind` sets `Some(Find)`; `ModeGraph`
    // and `FocusSearch` clear it back to `None` (= graph mode). `setMode`
    // (search.js:88-104) and `onSearchInput` (search.js:56-62) make the input's
    // value the ACTIVE mode's text, so the same split is published here.
    //
    // SEMANTICS CORRECTION over round 6: this key used to be written from the
    // Find query unconditionally, which published the Find panel's text while the
    // graph-mode box was the one focused. The reference cannot reach that state —
    // `searchInputEl.value` is one element's value and it belongs to the mode that
    // owns the input. The round-6 pin (`search_query_facts_track_the_find_query`)
    // is therefore STRENGTHENED below, never deleted: it now asserts find mode ->
    // find query AND graph mode -> graph query.
    let find_mode = state.surface == Some(crate::actions::HostSurface::Find);
    let active_query = if find_mode {
        state.find.query()
    } else {
        state.suggest.query()
    };
    // `#searchInput`'s typed query (`search.js:58/129` owns the input's `value`).
    // Absent when nothing is typed: the input keeps `index.html`'s no-value
    // state, so the default frame is untouched. An empty string is NOT sent —
    // there is nothing to paint and "absent" is the shipped state.
    if !active_query.is_empty() {
        obj["search_query"] = serde_json::json!(active_query);
    }
    // The graph-mode `#suggestions` popup. The key's PRESENCE is the open state
    // (`showSuggestions`, search.js:11-14): while the graph query is empty this
    // is `None` and the shipped `class="suggestions hidden"` box is left exactly
    // as `index.html` shipped it — which is what keeps the default paint, and
    // `@frame 1600 1000 4bd131fa71b2ccc8`, byte-identical. The Find mode owns the
    // `#findPanel` list instead, so it publishes no suggestions here.
    //
    // It also requires `search_focused`: the reference only shows the popup from
    // the input's `focus` listener (shell.js:503-507) and hides it on blur /
    // Escape (`hideSuggestions`, search.js:11-20) *without* clearing the input's
    // value. That is why the 7A-era "clear the query to hide" workaround is gone:
    // Escape now keeps `state.suggest`'s query so `ui.search_query` above keeps
    // painting the box's text, and the popup closes by dropping this key.
    if !find_mode && state.search_focused {
        if let Some(v) = graph.and_then(|g| state.suggest.facts(g)) {
            obj["suggestions"] = v;
        }
    }
    // The shortcuts/settings sheet's body (`#shortcutsBody`). The reference
    // builds these rows in `openShortcuts` (`shell.js:344-349` → `renderShortcuts`
    // `shell.js:330-342`), so they exist only while the sheet is the OPEN surface.
    // Sending them while closed would add 33 rows to a sheet that ships EMPTY
    // (`index.html:433`), breaking the default render's byte-identical promise and
    // moving the pinned `@frame`. `surface` is the one owner of "which modal is
    // open", so this is exactly the reference's open state.
    if state.surface == Some(crate::actions::HostSurface::Shortcuts) {
        let rows: Vec<serde_json::Value> = state
            .shortcuts
            .iter()
            .map(|r| {
                serde_json::json!({
                    "group": r.group,
                    "keys": r.keys,
                    "desc": r.desc,
                })
            })
            .collect();
        obj["shortcuts"] = serde_json::json!({ "rows": rows });
    }
    // The sidebar filter model (`filters.js`): the real namespace option list and
    // the on/off state of every kind/edge chip. `graph` is the source (node kinds,
    // `Node::ns`, edge kinds), so the painted chips can never disagree with the
    // corpus the view filters. The renderer treats an ABSENT key as the shipped
    // all-on render (the pinned default-frame promise); a nzk_ui with a graph
    // always has real data to publish, so a real run carries the key from frame 1.
    if let Some(g) = graph {
        let selected_label = state
            .selected_node
            .and_then(|i| g.nodes.get(i))
            .map(|n| if n.label.is_empty() { n.id.clone() } else { n.label.clone() });
        obj["filters"] = filters_json(g, &state.filters, selected_label.as_deref());
    }
    obj
}

/// The `ui.filters` fact — the sidebar filter model (`filters.js`).
///
/// Namespaces are the graph's own `Node::ns` strings, the same source
/// `facts_json_with` counts; kinds and edge kinds likewise. A chip the state does
/// not mention is ON (`on: true`) — the shipped default and the "never hide a
/// kind the model has not seen" rule the view predicate also enforces.
fn filters_json(
    graph: &Graph,
    filters: &crate::actions::FilterState,
    selected_label: Option<&str>,
) -> serde_json::Value {
    use std::collections::BTreeSet;
    let mut kinds: BTreeSet<String> = BTreeSet::new();
    let mut namespaces: BTreeSet<String> = BTreeSet::new();
    let mut edges: BTreeSet<String> = BTreeSet::new();
    for n in &graph.nodes {
        kinds.insert(n.kind.clone());
        if !n.ns.is_empty() {
            namespaces.insert(n.ns.clone());
        }
    }
    for e in &graph.edges {
        edges.insert(e.kind.clone());
    }
    let kind_rows: Vec<serde_json::Value> = kinds
        .iter()
        .map(|k| serde_json::json!({ "kind": k, "on": !filters.hidden_kinds.contains(k) }))
        .collect();
    let edge_rows: Vec<serde_json::Value> = edges
        .iter()
        .map(|k| serde_json::json!({ "kind": k, "on": !filters.hidden_edges.contains(k) }))
        .collect();
    serde_json::json!({
        "kinds": kind_rows,
        "edges": edge_rows,
        "namespaces": namespaces,
        "namespace": filters.namespace,
        "ns_deep": filters.ns_deep,
        // The four remaining filter boxes: `#hideIsolated`/`#hideEdgeLabels`
        // are plain model booleans, and the two Group chips show the
        // `group_children`/`group_edges` flags that live on the view (the host
        // copies them into `NzkUiState` each pass). They are published so the
        // shell can paint the checked state of boxes it has no other way to
        // learn, and so a press's repaint is part of the facts digest.
        "hide_isolated": filters.hide_isolated,
        "hide_edge_labels": filters.vt_mode,
        // The "Hide parents" checkbox and the label its title names. `selected`
        // is only a tooltip hint, never the anchor — the view anchors on its own
        // selection, so a stale label cannot change what is filtered. "Only
        // related nodes" is drawer-only and has no sidebar fact.
        "focus_parents": filters.focus_parents,
        "focus_selected": selected_label,
    })
}

/// The status-line text the nzk_ui must show, computed from the graph the app
/// actually indexed — never from nzk_ui's fixture.
///
/// Mirrors `shell.js updateStatusLine`'s literal (double spaces around the
/// interpuncts; CSS collapses them). `files`/`parseMs` come from the engine's
/// state payload; `nodes`/`edges` are the graph the canvas draws, so the header
/// and the picture can never disagree.
pub fn status_text(graph: &Graph) -> String {
    // Measurement-only escape hatch: `parse Nms` is wall-clock and can never be
    // reproduced exactly, so an A/B harness can pin the reference session's
    // captured status line to compare nzk_ui *layout* rather than timer noise.
    // Live runs never set this.
    if let Ok(s) = std::env::var("CSVIEWER_STATUS_OVERRIDE") {
        if !s.is_empty() {
            return s;
        }
    }
    let files = graph
        .state
        .get("fileCount")
        .and_then(|v| v.as_u64())
        .unwrap_or(graph.nodes.len() as u64);
    let parse = graph
        .state
        .get("parseMs")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    // v2's `updateStatusLine` (shell.js:130-143) prints `state.counts.nodes` /
    // `state.counts.edges`: the engine's raw totals, which include the
    // external/literal stubs this view does not draw. Printing the kept
    // `graph.nodes`/`graph.edges` counts made v3 report 609/1285 where v2
    // reports 668/1615 on the same corpus (round-18 audit). The status line must
    // stay numerically comparable to v2; fall back to the drawn counts only when
    // the engine state carries no `counts` (unit fixtures), never invent one.
    let counts = graph.state.get("counts");
    let nodes = counts
        .and_then(|c| c.get("nodes"))
        .and_then(|v| v.as_u64())
        .unwrap_or(graph.nodes.len() as u64);
    let edges = counts
        .and_then(|c| c.get("edges"))
        .and_then(|v| v.as_u64())
        .unwrap_or(graph.edges.len() as u64);
    // v2 renders these counts with `toLocaleString("en-US")` — measured against
    // the reference session it prints `edges 1,615`, not `edges 1615`. The
    // separators are part of the pixel-identical status line, so format every
    // number (the parse time included) the same way.
    format!(
        "files {}  ·  parse {}ms  ·  nodes {}  ·  edges {}",
        group_thousands(files),
        group_thousands(parse),
        group_thousands(nodes),
        group_thousands(edges)
    )
}

/// Thousands separators, en-US, exactly what `Number#toLocaleString("en-US")`
/// produces for the non-negative integers the status line carries (v2's
/// `shell.js updateStatusLine`). Never used for floats: `parseMs`/counts are
/// whole numbers from the engine state.
fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Draw the native graph into the nzk_ui `#graph` box, then overlay native
/// behaviour on the controls we implement.
pub fn nzk_ui_ui(
    ui: &mut egui::Ui,
    view: &mut GraphView,
    tabs: &Tabs,
) -> Option<NzkUiAction> {
    let full = ui.max_rect();
    let canvas = canvas_rect()
        .map(|r| r.intersect(full))
        .filter(|r| r.width() > 8.0 && r.height() > 8.0)
        .unwrap_or(full);
    // The native canvas is the graph widget, clipped to the CSS-derived box:
    // the real stylesheet decides where the canvas is; view.rs still paints it.
    // The interactive rect the graph receives. Logged when it changes: this is
    // the rect `view.ui` allocates with `Sense::click_and_drag()`, so if it ever
    // stops matching the `#graph` DOM box, clicks land somewhere else.
    CANVAS_LOG.with(|last| {
        let mut last = last.borrow_mut();
        if last.map(|r| {
            (r.min.x - canvas.min.x).abs() > 0.5
                || (r.min.y - canvas.min.y).abs() > 0.5
                || (r.width() - canvas.width()).abs() > 0.5
                || (r.height() - canvas.height()).abs() > 0.5
        }) != Some(false)
        {
            eprintln!(
                "[native-csviewer] nzk_ui: interactive canvas rect {:.0},{:.0} {:.0}x{:.0} \
                 (from #graph {}, dpi {:.2})",
                canvas.min.x,
                canvas.min.y,
                canvas.width(),
                canvas.height(),
                if canvas_rect().is_some() { "yes" } else { "MISSING -> full window" },
                ui.ctx().pixels_per_point(),
            );
            *last = Some(canvas);
        }
    });
    // A modal's backdrop owns the whole viewport. Tell the native canvas so its
    // egui widget does not also receive the press (the modal is painted pixels,
    // not an egui layer, so egui would otherwise deliver the click to the graph
    // underneath it). This is what stops a picker row press from selecting a node
    // behind the dialog and the selection-change handler from closing it.
    //
    // The open DRAWERS (and the other opaque controls painted inside
    // `#canvasWrap`) need the same suspension. They are DOM overlays re-stamped
    // over the graph, but `view.ui` runs BEFORE `overlay` and its egui rect
    // covers the whole canvas column. Without this, pressing the node drawer's
    // body also reached the graph underneath, which cleared the selection — and
    // the selection-change handler then closed the drawer the press was aimed
    // at. Suspend only when the pointer is actually over such an overlay, so a
    // click on the canvas beside an open drawer still selects.
    let over_overlay = ui
        .ctx()
        .pointer_latest_pos()
        .is_some_and(canvas_overlay_owns_pointer);
    view.input_suspended = modal_is_open() || over_overlay;
    let mut child = ui.new_child(UiBuilder::new().max_rect(canvas));
    view.ui(&mut child);
    // `#graph` is the whole `#canvasWrap` box, so the native graph above
    // erase-paints every static nzk_ui pixel inside it — including the
    // `.canvas-tools` row, which is why the round-18 A/B showed the row in v2
    // but not in v3. The nzk_ui frame already painted those pixels; re-stamp
    // the cached nzk_ui texture over just the DOM overlays that live inside the
    // canvas box, after the graph and before `overlay`'s dynamic feedback.
    repaint_canvas_overlays(ui);
    let action = overlay(ui, view, tabs);
    // A press that produced a command, or a staged slider/drag edit, changes
    // host state that is painted on a LATER frame (`paint_in` runs before
    // `overlay` in every frame, and the app never call `request_repaint`).
    // Without this the click frame ends, no new frame is scheduled, and the
    // press looks dead even though the action was applied. Scheduling exactly
    // one follow-up frame when something was consumed keeps the idle loop
    // quiet: an unchanged frame consumes nothing and requests nothing.
    let staged = PENDING_SLIDER.with(|q| !q.borrow().is_empty())
        || PENDING_SPLIT.with(|q| !q.borrow().is_empty())
        || PENDING_SUGGEST.with(|q| !q.borrow().is_empty())
        || PENDING_FIND_ROW.with(|q| !q.borrow().is_empty())
        || PENDING_BLOCK_NODE.with(|q| !q.borrow().is_empty())
        || PENDING_DRAWER_NODE.with(|q| !q.borrow().is_empty())
        || PENDING_CHAIN_NODE.with(|q| !q.borrow().is_empty())
        || PENDING_CHAIN_COLLAPSE.with(|q| !q.borrow().is_empty())
        || PENDING_LIST_NODE.with(|q| !q.borrow().is_empty())
        || PENDING_DRAWER_CLICK.with(|q| !q.borrow().is_empty());
    // `#fitBtn` is a `Local` control: it acts on the view inside `route_click`
    // and returns no `NzkUiAction`, so it is invisible to `action.is_some()`.
    // Without this clause the fit flag was set and then left pending until an
    // unrelated repaint — the "Fit does nothing" defect. One request per press
    // is enough: `view.ui` consumes the flag on the next frame.
    if action.is_some() || staged || view.fit_pending {
        ui.ctx().request_repaint();
    }
    action
}

/// Re-stamp the cached nzk_ui nzk_ui texture over the DOM overlays that sit
/// inside the graph's box. `nzk_ui_ui` paints the native graph clipped to
/// `#graph` (== `#canvasWrap`, the full canvas column) *after* `paint_in`
/// uploaded the nzk_ui frame, so any static element inside that box is erased.
/// `overlay()` runs after the graph and only paints dynamic feedback (tooltips,
/// hover, split divider); the static row (`.canvas-tools`) and `#notice` need
/// this explicit re-stamp. Selector boxes come from the same `--boxes` map the
/// hit-tester uses, so the repaint and the click target cannot disagree.
const CANVAS_OVERLAYS: &[&str] = &[
    ".canvas-tools",
    "#notice",
    "#tooltip",
    "#emptyHint",
    "#loading",
    "#modal",
    "#shortcutsModal",
    "#editModal",
    "#previewModal",
    "#reorgModal",
    "#nodeDrawer",
    "#findPanel",
    "#blocksPanel",
];

fn repaint_canvas_overlays(ui: &egui::Ui) {
    let full = ui.max_rect();
    if full.width() <= 0.0 || full.height() <= 0.0 {
        return;
    }
    let clip = ui.clip_rect();
    NZK_UI.with(|slot| {
        let slot = slot.borrow();
        let Some(c) = slot.as_ref() else { return };
        let painter = ui.painter().with_clip_rect(clip);
        for sel in CANVAS_OVERLAYS {
            let Some(r) = c.boxes.get(*sel) else { continue };
            let r = r.intersect(full);
            if r.width() <= 0.5 || r.height() <= 0.5 {
                continue;
            }
            let uv = Rect::from_min_max(
                Pos2::new(
                    (r.min.x - full.min.x) / full.width(),
                    (r.min.y - full.min.y) / full.height(),
                ),
                Pos2::new(
                    (r.max.x - full.min.x) / full.width(),
                    (r.max.y - full.min.y) / full.height(),
                ),
            );
            painter.image(c.tex.id(), r, uv, Color32::WHITE);
        }
    });
}


/// The `#graph` (else `#canvasWrap`) border box from the last nzk_ui frame.
pub fn canvas_rect() -> Option<Rect> {
    box_rect("#graph").or_else(|| box_rect("#canvasWrap"))
}

/// Is a full-viewport modal backdrop on screen in the last painted frame?
///
/// `MODALS` are exactly the elements styled `position: fixed; inset: 0`, and a
/// closed one has a `0 0 0 0` box (see `nzk_ui`'s `apply_modals`). A non-zero box therefore means "this backdrop is up and owns
/// the pointer", which is the condition [`GraphView::input_suspended`] encodes.
pub fn modal_is_open() -> bool {
    ["#modal", "#shortcutsModal", "#editModal", "#previewModal", "#reorgModal"]
        .into_iter()
        .any(|selector| {
            box_rect(selector).is_some_and(|r| r.width() > 0.0 && r.height() > 0.0)
        })
}

/// A resolved border box from the last nzk_ui frame, in points.
pub fn box_rect(selector: &str) -> Option<Rect> {
    NZK_UI.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|c| c.boxes.get(selector).copied())
    })
}

/// Overlays painted INSIDE `#canvasWrap` that own the pointer where they are
/// drawn, so the native graph underneath must not also receive the press.
///
/// The drawers are opaque panels re-stamped over the graph; `.canvas-tools` and
/// `#loading` behave the same way. `#tooltip` / `#notice` / `#emptyHint` are
/// deliberately absent: they are informational, pointer-transparent pixels, and
/// suspending over them would swallow clicks on the canvas they float above.
fn canvas_overlay_owns_pointer(pos: Pos2) -> bool {
    const OPAQUE: &[&str] = &[
        "#nodeDrawer",
        "#findPanel",
        "#blocksPanel",
        ".canvas-tools",
        "#loading",
    ];
    OPAQUE.iter().any(|selector| {
        box_rect(selector).is_some_and(|r| {
            r.width() > 0.5 && r.height() > 0.5 && r.contains(pos)
        })
    })
}

/// The overlay drawer CONTAINERS. They are in [`CONTROLS`] for their geometry
/// and so the control channel can still name `#nodeDrawer`, but a pointer press
/// on the panel must not toggle it: the reference wires only the close buttons.
const DRAWER_CONTAINERS: &[&str] = &["#nodeDrawer", "#findPanel", "#blocksPanel"];

/// Resolve a pointer press to a control and return the action it requests.
///
/// **D2: hit-test consumes the layout that painted.** There is no `ui.put`, and
/// no invisible `egui::Button` is placed on top of the nzk_ui. The point is
/// tested against the *same* [`FrameGeometry`] whose pixels `paint_in` uploaded,
/// so the click path and the paint path cannot consult different layouts.
///
///   * `Section` — returned as `actions::NzkUiAction::ToggleSection`; the host
///     owns the collapsed state and applies it, and the next frame rebuilds.
///   * `Local`   — toggles immediate-mode view state (`#fitBtn`).
///   * `Host`    — returned to the caller as an `actions::NzkUiAction`, which
///     `app.rs` executes through `actions::apply`. nzk_ui.rs never implements
///     those semantics and never runs `Effect::reindex` itself.
///   * `Inert`   — no action, and no hover decoration either: it is drawn by the
///     stylesheet and does nothing, so nothing here pretends otherwise.
///
/// `chain` is the shell's DOM hit chain for the press point (nearest element
/// first). It is consulted so a press on a control's **child** element — for
/// example the `<svg class="shortcuts-gear">` inside `#shortcutsBtn`, which is
/// what the centre of that control actually hits — still routes to the control.
///
/// Drawer containers are registered (they have real geometry and a close
/// action) but are NOT click targets: see [`DRAWER_CONTAINERS`].
fn route_click(
    geo: &FrameGeometry,
    chain: &[HitNode],
    view: &mut GraphView,
    x: f32,
    y: f32,
) -> Option<NzkUiAction> {
    let selector = resolve_selector(geo, chain, x, y)?;
    // A drawer CONTAINER is an overlay surface, not a button. The shell chain
    // for any press on its padding names a child (`.drawer-body`, a section),
    // and the flat box map then falls back to the container; turning that into
    // `ToggleDrawer` closed the panel the press was aimed at. The reference has
    // no such handler (only the close buttons below), so refuse it here.
    if DRAWER_CONTAINERS.contains(&selector) {
        return None;
    }
    let c = CONTROLS.iter().find(|c| c.selector == selector)?;
    match c.owner {
        // The host owns `NzkUiState::collapsed`; nzk_ui.rs only names the
        // action, exactly like the other host controls.
        ControlOwner::Section | ControlOwner::Host => {
            let action = NzkUiAction::from_selector(c.selector);
            action.is_effective().then_some(action)
        }
        ControlOwner::Local => {
            if c.selector == "#fitBtn" {
                view.fit_pending = true;
            }
            None
        }
        // The picker's own controls. `route_click` cannot answer them: three
        // need a payload a `NzkUiAction` cannot hold, and the honest resolution
        // is [`picker_action`], which the caller runs *before* this function.
        // Returning `None` here is what keeps `#modalClose` from falling through
        // to `ToggleModal` and re-opening the dialog.
        ControlOwner::Picker => None,
        ControlOwner::Inert if chain.first().and_then(|hit| hit.id.as_deref()) == Some(&c.selector[1..]) => {
            modal_backdrop_action(c.selector)
        }
        ControlOwner::Inert => None,
        // A range input's press carries a *value*, which a `NzkUiAction` cannot
        // hold. Claiming it here and returning `None` is what stops a press on a
        // slider from also being read as a click on whatever is behind it;
        // [`route_drag`] is the function that answers, and `overlay` calls it
        // before this one.
        ControlOwner::Slider => None,
    }
}

/// The reference closes these four modal sheets when the press lands on their
/// backdrop. The shell hit chain proves the backdrop itself was hit; the folder
/// picker remains on its payload-aware `PickerAction::Close` path.
fn modal_backdrop_action(selector: &str) -> Option<NzkUiAction> {
    let surface = match selector {
        "#shortcutsModal" => crate::actions::HostSurface::Shortcuts,
        "#editModal" => crate::actions::HostSurface::Edit,
        "#previewModal" => crate::actions::HostSurface::Preview,
        "#reorgModal" => crate::actions::HostSurface::Export,
        _ => return None,
    };
    Some(NzkUiAction::ToggleModal(surface))
}

/// Map a pointer position inside a range input's box to a value in its range.
///
/// # Why the geometry, and not the DOM's own `value`
///
/// A native `<input type=range>` reports its value in the DOM, but this shell is
/// a static rasteriser: the value would have to be read back out of the chain,
/// and the chain is a *hit* path (`HitNode` carries a selector, not a value).
/// Mapping the pointer through the same box the hit-test resolved is therefore
/// both the only available source and the more honest one — it is exactly what
/// the browser does, and it guarantees the slider cannot report a value that
/// disagrees with where it is painted, because both come from one rect.
///
/// The thumb is inset by half its width at each end, so the value reaches `min`
/// at the left edge of the track and `max` at the right edge. This port uses the
/// full box (inset by `THUMB_INSET`) rather than the DOM's own padding, which the
/// rasteriser does not expose — stated here rather than left implicit.
pub fn slider_value(kind: SliderKind, rect: &Rect, x: f32) -> f64 {
    let (min, max, step) = kind.range();
    // The draggable span: the track, less half a thumb at each end.
    let inset = THUMB_INSET.min(rect.width() / 2.0);
    let lo = rect.left() + inset;
    let hi = rect.right() - inset;
    let span = (hi - lo).max(1.0);
    let t = (((x - lo) / span) as f64).clamp(0.0, 1.0);
    let raw = min + (max - min) * t;
    // Snap to the control's `step`, then clamp, so the value is always one the
    // range input could actually have produced (`index.html`'s `step` attrs).
    let snapped = ((raw - min) / step).round() * step + min;
    snapped.clamp(min, max)
}

/// The value-carrying sibling of [`route_click`]: a press or drag inside a
/// Layout ▸ range input.
///
/// Resolves through the *same* [`resolve_selector`] the hit test uses, so the
/// value and the painted control cannot drift apart — the invariant `route_click`
/// documents for actions applies here for values.
///
/// Returns `None` for a point that is not on a slider, which is what lets the
/// caller treat "not a slider" as "fall through to the click/pan paths".
pub fn route_drag(geo: &FrameGeometry, chain: &[HitNode], x: f32, y: f32, phase: SliderPhase) -> Option<SliderEdit> {
    let selector = resolve_selector(geo, chain, x, y)?;
    let kind = SliderKind::from_selector(selector)?;
    let rect = geo.boxes.get(selector)?;
    Some(SliderEdit::new(kind, slider_value(kind, rect, x), phase))
}

/// Half the thumb width the range inputs are drawn with.
///
/// The rasteriser has no DOM metrics, so this is the fork's own constant,
/// matched to the `<input type=range>` track the stylesheet draws. It only moves
/// where `min`/`max` are reachable, never the range itself.
pub const THUMB_INSET: f32 = 8.0;

// ----------------------------------------------------- split divider drag
//
// `shell.js:1505-1530` (`onDividerDown` / `onDividerMove` / `endDivider`). The
// reference attaches `pointerdown` to the runtime-created `#csSplitDivider` and
// then listens on `window` for `pointermove`/`pointerup` until release, setting
// `--cs-split-w` on `#csSplitWrap` from
// `clamp((ev.clientX - wrap.left) / wrap.width, 0.15, 0.85)`.
//
// The host cannot write the style itself (nzk_ui owns the DOM), so the drag
// is modelled here as a pure state machine + value, staged for `app.rs` to write
// into `Tabs::set_split_frac`; the ratio then travels back through the tabs fact
// (`split_w`) and `dom::apply_split` writes the shipped custom property.
// `#csSplitDivider` has no `CONTROLS`/`--boxes` entry, so the press is resolved
// from the shell's hit chain exactly like the tab bar and the collapse button.

/// The clamp band `shell.js:1518` gives the divider (`0.15..=0.85`).
pub const SPLIT_MIN_FRAC: f64 = 0.15;
pub const SPLIT_MAX_FRAC: f64 = 0.85;

/// `shell.js:1518` — the divider fraction at pointer `x` over the split wrap:
/// `(x - wrap.left) / wrap.width`, clamped to [`SPLIT_MIN_FRAC`]..=[`SPLIT_MAX_FRAC`].
///
/// `None` for a degenerate span (zero/negative width, non-finite input): the
/// reference returns early when `r.width` is 0, so the position is unchanged.
pub fn split_frac_for_x(wrap_left: f32, wrap_width: f32, x: f32) -> Option<f64> {
    if !wrap_left.is_finite() || !wrap_width.is_finite() || !x.is_finite() || wrap_width <= 0.0 {
        return None;
    }
    let frac = f64::from(x - wrap_left) / f64::from(wrap_width);
    if !frac.is_finite() {
        return None;
    }
    Some(frac.clamp(SPLIT_MIN_FRAC, SPLIT_MAX_FRAC))
}

/// Reconstruct `#csSplitWrap`'s horizontal span from two boxes the SAME frame's
/// layout reported.
///
/// `#csSplitWrap` is not a `PROBE_SELECTORS` row, but nothing has to be invented:
/// the wrap is a border-less flex row whose first child is `#sourceBody` (so the
/// wrap's left edge IS the body's left edge) and whose last child `#csSplitBody`
/// stretches to its right edge (`#csSplitHost` fills that body's width). The pair
/// therefore reproduces `getBoundingClientRect()` of the wrap — the reference's
/// own denominator — without a second layout or a hardcoded width.
///
/// `None` when either box is absent (split not painted this frame) or the span
/// is degenerate.
pub fn split_wrap_span(body: Rect, host: Rect) -> Option<(f32, f32)> {
    let left = body.left();
    let width = host.right() - left;
    if !left.is_finite() || !width.is_finite() || width <= 0.0 {
        return None;
    }
    Some((left, width))
}

/// True when the shell's nearest-first hit chain names the runtime-created
/// `#csSplitDivider`.
pub fn split_divider_hit(chain: &[HitNode]) -> bool {
    chain
        .iter()
        .any(|n| n.id.as_deref() == Some("csSplitDivider"))
}

/// One transition of the divider drag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDragPhase {
    /// Pointer went down on `#csSplitDivider` (`onDividerDown`).
    Begin,
    /// Pointer moved while held; the value carries the clamped fraction.
    Move,
    /// Pointer released (`endDivider`); the position stays where it stopped.
    End,
}

/// A staged divider-drag transition, drained by `app.rs` through
/// [`take_pending_split_drags`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplitDragEdit {
    pub phase: SplitDragPhase,
    /// `Some` only for [`SplitDragPhase::Move`].
    pub frac: Option<f64>,
}

/// The divider drag's start/move/end state (`dragging` in `shell.js:1505-1530`).
#[derive(Debug, Default)]
pub struct SplitDrag {
    active: bool,
}

impl SplitDrag {
    pub const fn new() -> Self {
        Self { active: false }
    }

    /// `onDividerDown`: begin only when the press chain hits the divider.
    pub fn begin(&mut self, chain: &[HitNode]) -> Option<SplitDragEdit> {
        if !split_divider_hit(chain) {
            return None;
        }
        self.active = true;
        Some(SplitDragEdit {
            phase: SplitDragPhase::Begin,
            frac: None,
        })
    }

    /// `onDividerMove`: the clamped ratio while the drag is live. `None` when no
    /// drag is live or the wrap span cannot be measured (the reference returns
    /// early on a zero width).
    pub fn move_to(&mut self, wrap_left: f32, wrap_width: f32, x: f32) -> Option<SplitDragEdit> {
        if !self.active {
            return None;
        }
        Some(SplitDragEdit {
            phase: SplitDragPhase::Move,
            frac: split_frac_for_x(wrap_left, wrap_width, x),
        })
    }

    /// `endDivider`: ends the drag; `Some` exactly once, so a release cannot be
    /// missed or applied twice.
    pub fn end(&mut self) -> Option<SplitDragEdit> {
        if !self.active {
            return None;
        }
        self.active = false;
        Some(SplitDragEdit {
            phase: SplitDragPhase::End,
            frac: None,
        })
    }

    pub fn is_active(&self) -> bool {
        self.active
    }
}

thread_local! {
    /// The divider drag in progress, owned by the nzk_ui pointer path.
    static SPLIT_DRAG: RefCell<SplitDrag> = const { RefCell::new(SplitDrag::new()) };
    /// Divider transitions staged by [`overlay`] for `app.rs` to apply to the
    /// host's `Tabs` model (the nzk_ui holds no mutable tabs).
    static PENDING_SPLIT: RefCell<Vec<SplitDragEdit>> = const { RefCell::new(Vec::new()) };
}

/// The wrap span from this frame's geometry, if the split scaffold is painted.
fn split_wrap_span_now() -> Option<(f32, f32)> {
    split_wrap_span(box_rect("#sourceBody")?, box_rect("#csSplitHost")?)
}

fn push_split(edit: SplitDragEdit) {
    PENDING_SPLIT.with(|q| q.borrow_mut().push(edit));
}

/// Take every divider-drag transition staged since the last call, oldest first.
///
/// Drained by `app.rs` once per frame after the nzk_ui pass; only `Move` edits
/// carry a value, and each one writes `Tabs::set_split_frac` (the pointer path's
/// only writer).
pub fn take_pending_split_drags() -> Vec<SplitDragEdit> {
    PENDING_SPLIT.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// End the current divider drag, if any. Used when the window loses the pointer
/// with no release event, so a stale drag cannot own the next press.
fn split_drag_cancel() {
    if let Some(edit) = SPLIT_DRAG.with(|d| d.borrow_mut().end()) {
        push_split(edit);
    }
}

/// One cached hover probe: the shell's hit chain for the current pointer plus
/// the measured box of the row the hit routes to.
#[derive(Clone, Debug, Default)]
struct HoverProbe {
    pos: (f32, f32),
    chain: Vec<HitNode>,
    rect: Option<Rect>,
}

thread_local! {
    /// The last hover probe, keyed by the pointer position. Refreshed only when
    /// the pointer actually moves, so a still pointer costs no shell request.
    static HOVER: RefCell<Option<HoverProbe>> = const { RefCell::new(None) };
}

/// The hover probe for `pos`, reusing the cached answer while the pointer is
/// still (within half a point). A failed probe is an honest empty chain.
fn hover_probe_cached(pos: Pos2) -> HoverProbe {
    HOVER.with(|slot| {
        let mut slot = slot.borrow_mut();
        let fresh = slot.as_ref().is_some_and(|h| {
            (h.pos.0 - pos.x).abs() <= 0.5 && (h.pos.1 - pos.y).abs() <= 0.5
        });
        if !fresh {
            let (chain, rect) = hit_probe_at(pos.x, pos.y).unwrap_or_default();
            *slot = Some(HoverProbe { pos: (pos.x, pos.y), chain, rect });
        }
        slot.clone().unwrap_or_default()
    })
}

/// Drop the cached hover probe after a rebuild/selection change, so an old
/// chain cannot describe a row that no longer exists.
fn hover_probe_invalidate() {
    HOVER.with(|slot| *slot.borrow_mut() = None);
}

/// The class that makes a hit row look (and behave) like a button.
///
/// The reference styles these as `cursor: pointer` + `:hover` background; the
/// renderer is a static raster, so `:hover` never fires and a row gave no
/// affordance at all. This is the host-side equivalent: the row's measured box
/// gets a hover fill and the cursor becomes a hand.
fn button_row_class(chain: &[HitNode]) -> Option<&'static str> {
    const ROWS: &[&str] = &[
        "rel-node",
        "hop",
        "relationship",
        "lrow",
        "frow",
        "sug",
        "cs-tab",
        "pick",
        "block-card",
        "prop-action",
        "crumb",
        "rel-toggle",
        "rel-caret",
        "child-toggle",
        "edgekey",
        "facet",
        "linkbtn",
        // The Declared-types chips (runtime-created). Without this a chip is a
        // painted label with no hover fill and no hand cursor, while being a
        // real press target.
        "type-chip",
    ];
    for node in chain {
        let Some(class) = node.class.as_deref() else { continue };
        for part in class.split_whitespace() {
            if let Some(&hit) = ROWS.iter().find(|r| **r == part) {
                return Some(hit);
            }
        }
    }
    None
}

/// Draw hover feedback for a control/row under the cursor, plus the drift
/// banner.
///
/// Purely diagnostic painting: it reads the same geometry as the hit-test and
/// never claims input, so it cannot swallow a press meant for the canvas.
fn paint_feedback(ui: &egui::Ui, geo: &FrameGeometry) {
    if geo.drift.is_some() {
        // A proven drift must be visible, not silent: the nzk_ui is still drawn
        // but it is no longer interactive, and this says so on the surface.
        let full = ui.max_rect();
        let rect = Rect::from_min_size(full.min, egui::vec2(full.width(), 20.0));
        ui.painter().rect_filled(rect, 0.0, Color32::from_rgb(0x7a, 0x1f, 0x1f));
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "nzk_ui geometry does not match the painted frame - controls disabled",
            egui::FontId::proportional(12.0),
            Color32::WHITE,
        );
        return;
    }
    let Some(pos) = ui.ctx().pointer_latest_pos() else { return };
    // A pointer over the native graph itself has no DOM row under it. Asking
    // the shell there would buy an empty chain at the cost of a round trip on
    // every mouse move, so skip it (the drawers/overlays above the canvas are
    // still probed — they are what paints relation and chain rows).
    let over_plain_graph = canvas_rect().is_some_and(|r| r.contains(pos))
        && !canvas_overlay_owns_pointer(pos);
    if over_plain_graph {
        hover_probe_invalidate();
        return;
    }
    let hover = hover_probe_cached(pos);
    let control = hit_in(&geo.boxes, pos.x, pos.y)
        .filter(|s| CONTROLS.iter().any(|c| c.selector == *s && c.owner != ControlOwner::Inert))
        .and_then(|s| geo.boxes.get(s).copied());

    // A registered control keeps the existing blue outline. A runtime row
    // (relations, resolve chain, list/find rows, …) has no `--boxes` entry, so
    // its measured `@hitbox` is the only honest rectangle: fill it like the CSS
    // `:hover` rule would, and show a hand cursor.
    if let Some(r) = control {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        ui.painter().rect_stroke(
            r,
            3.0,
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(0x00, 0x7a, 0xcc, 0x99)),
            StrokeKind::Inside,
        );
    }
    if button_row_class(&hover.chain).is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        if let Some(r) = hover.rect {
            // Flat fills only: the static raster below stays readable, and the
            // affordance is exactly the reference's `.rel-node:hover` /
            // `.hop:hover` background plus a faint accent edge.
            let r = r.intersect(ui.clip_rect());
            if r.width() > 1.0 && r.height() > 1.0 {
                ui.painter().rect_filled(
                    r,
                    3.0,
                    Color32::from_rgba_unmultiplied(0x1e, 0x27, 0x40, 178),
                );
                ui.painter().rect_stroke(
                    r,
                    3.0,
                    Stroke::new(1.0, Color32::from_rgba_unmultiplied(0x3b, 0x6a, 0xa8, 0xaa)),
                    StrokeKind::Inside,
                );
            }
        }
    }
    // ---- title tooltips -----------------------------------------------------
    // A browser draws `title` as its own hover tooltip; this shell is a static
    // raster, so in v3 a `title` had no visible effect at all. The shell reports
    // the nearest `title` in the same hit chain the press would use (`@hit`
    // column 13), and it is painted here — from the layout that painted the
    // pixels, so the tooltip names the element the pointer is actually over.
    if let Some(title) = hover
        .chain
        .iter()
        .find_map(|n| n.title.as_deref().filter(|t| !t.is_empty()))
    {
        paint_title_tooltip(ui, pos, title);
    }
}

/// Draw one chrome `title` as a hover tooltip near the pointer.
///
/// Wraps at 360pt, flips to the left/above the cursor when the preferred
/// below-right position would leave the window, and never claims input: it is
/// pure paint, like the rest of [`paint_feedback`]. The palette matches the
/// drawer/card surfaces so it reads as part of the shell rather than an egui
/// default.
fn paint_title_tooltip(ui: &egui::Ui, pos: Pos2, title: &str) {
    const MAX_W: f32 = 360.0;
    const PAD_X: f32 = 8.0;
    const PAD_Y: f32 = 6.0;
    let font = egui::FontId::proportional(12.0);
    let wrap = egui::text::TextWrapping::wrap_at_width(MAX_W);
    let mut job = egui::text::LayoutJob::single_section(
        title.to_string(),
        egui::TextFormat { font_id: font, color: Color32::from_rgb(0xe3, 0xe8, 0xf4), ..Default::default() },
    );
    job.wrap = wrap;
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let size = galley.size();
    let body = egui::vec2(size.x + PAD_X * 2.0, size.y + PAD_Y * 2.0);
    let clip = ui.clip_rect();
    let mut at = Pos2::new(pos.x + 14.0, pos.y + 20.0);
    if at.x + body.x > clip.right() - 4.0 {
        at.x = (pos.x - body.x - 10.0).max(clip.left() + 4.0);
    }
    if at.y + body.y > clip.bottom() - 4.0 {
        at.y = (pos.y - body.y - 10.0).max(clip.top() + 4.0);
    }
    let rect = Rect::from_min_size(at, body);
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, Color32::from_rgb(0x16, 0x16, 0x25));
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, Color32::from_rgb(0x2f, 0x3d, 0x5c)),
        StrokeKind::Inside,
    );
    painter.galley(
        rect.min + egui::vec2(PAD_X, PAD_Y),
        galley,
        Color32::WHITE,
    );
}



/// The pointer buttons the nzk_ui routes, decoded from egui's button id.
///
/// Only two buttons are nzk_ui presses: the left button runs the click commands
/// and the middle button runs the tab bar's `onBarAux` close (`shell.js:1260`).
/// Everything else (right button, extra buttons) is not a nzk_ui press and is
/// left alone — the canvas and other widgets are untouched by this decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarButton {
    Primary,
    Middle,
    Other,
}

/// Decode one egui pointer button into the nzk_ui's routed set.
pub fn bar_button(button: egui::PointerButton) -> BarButton {
    match button {
        egui::PointerButton::Primary => BarButton::Primary,
        egui::PointerButton::Middle => BarButton::Middle,
        _ => BarButton::Other,
    }
}

/// The routed pointer button that was physically pressed this frame.
///
/// The shell routes the **press**, not egui's synthetic *click*. Every reference
/// handler is a DOM `click`, which fires when the down and the up land on the
/// same element — a pointer that drifts a few pixels, or is held for a moment,
/// still clicks. egui's `button_clicked` is stricter: it refuses the click when
/// the pointer moved more than `max_click_dist` (6 px) or was held longer than
/// `max_click_duration` (0.8 s), and neither limit exists in the browser.
///
/// That strictness is an observed defect, not a theory. The recorded interaction
/// trace (`[input] mouse Pressed` / `Released`) contains a press at
/// `(60.41, 355.30)` released at `(61.41, 363.30)` — 8.1 px — that produced no
/// `[input] nzk_ui press` line at all, while the very next attempt (0.0 px of
/// drift) routed. From the user's seat that is "I have to click some buttons
/// twice". Routing the press removes both limits; the ordering in [`overlay`]
/// keeps the split divider and the sliders — the only drags that start on a
/// shell control — ahead of it.
fn routed_press(i: &egui::InputState) -> Option<BarButton> {
    [egui::PointerButton::Primary, egui::PointerButton::Middle]
        .into_iter()
        .find(|b| i.pointer.button_pressed(*b))
        .map(bar_button)
}

/// Route this frame's pointer press through the painted layout.
///
/// egui still owns the raw input event, so the press is *read* from it — but
/// *what was pressed* is resolved from the shell geometry, and the press is
/// consumed here so the canvas cannot also act on it. No widget is created, so
/// there is no second rect for input to disagree with.
fn overlay(ui: &mut egui::Ui, view: &mut GraphView, tabs: &Tabs) -> Option<NzkUiAction> {
    let Some(geo) = geometry() else { return None };
    paint_feedback(ui, &geo);
    if geo.drift.is_some() {
        return None;
    }
    // Which routed button was physically pressed this frame (`bar_button`: left
    // = the click commands, middle = the tab bar's `onBarAux`; anything else is
    // not a nzk_ui press).
    let pressed_button = ui.ctx().input(routed_press);
    let pressed = pressed_button == Some(BarButton::Primary);
    // A chain row's double press is the reference's collapse gesture
    // (`filters.js:610-619`). egui only knows the gesture completed on the
    // second release, so it is read here and applied on that edge below, after
    // the second press has already selected the row.
    let double_released = ui
        .ctx()
        .input(|i| i.pointer.button_double_clicked(egui::PointerButton::Primary));
    let pos = ui.ctx().pointer_latest_pos();
    let Some(pos) = pos else { return None };
    // ---- Blocks canvas hover peek (`scratch.js:424-454`) --------------------
    // Asked on every frame the pointer moves, and once more after it leaves a
    // card so the tooltip can hide. The transitions are staged for `App`, which
    // owns the graph and the engine the peek content is built from.
    let pointer_moved = ui.ctx().input(|i| i.pointer.delta() != egui::Vec2::ZERO);
    let last_hover = LAST_BLOCK_HOVER.with(|last| last.borrow().clone());
    // Only the Blocks panel has cards to peek. While it is closed this used to
    // ask the shell for a full hit chain on *every* pointer move (which meant a
    // chrome repaint on every mouse move); now the cached hover probe is reused
    // and the panel's own box is the gate.
    let blocks_open = box_rect("#blocksPanel")
        .is_some_and(|r| r.width() > 0.5 && r.height() > 0.5);
    if blocks_open && (pointer_moved || last_hover.is_some()) {
        let hover_chain = hover_probe_cached(pos).chain;
        let hovered = block_card_node(&hover_chain);
        if hovered != last_hover {
            LAST_BLOCK_HOVER.with(|last| *last.borrow_mut() = hovered.clone());
            push_block_hover(match hovered {
                Some(id) => BlockHover::Show(id),
                None => BlockHover::Hide,
            });
        }
    } else if !blocks_open && last_hover.is_some() {
        LAST_BLOCK_HOVER.with(|last| *last.borrow_mut() = None);
        push_block_hover(BlockHover::Hide);
    }
    // ---- split divider drag (`shell.js:1505-1530`) -------------------------
    // A press that starts on `#csSplitDivider` OWNS the pointer until release:
    // `onDividerDown` installs window-level pointermove/pointerup listeners, so
    // the reference stops hit-testing the divider as soon as the cursor leaves
    // its 7px strip. The drag state is therefore consulted first, and the press
    // that starts it is resolved from the shell's hit chain further down.
    if SPLIT_DRAG.with(|d| d.borrow().is_active()) {
        let down = ui.ctx().input(|i| i.pointer.primary_down());
        let released = ui.ctx().input(|i| i.pointer.primary_released());
        let edit = SPLIT_DRAG.with(|d| {
            let mut d = d.borrow_mut();
            if released || !down {
                // Release commits; `!down` also covers the pointer leaving the
                // window with no release event, so a drag cannot stay stuck.
                d.end()
            } else {
                match split_wrap_span_now() {
                    Some((left, width)) => d.move_to(left, width, pos.x),
                    None => None,
                }
            }
        });
        if let Some(edit) = edit {
            push_split(edit);
        }
        return None;
    }
    // `onBarAux` (shell.js:1260-1266): a middle press answers only on the tab
    // bar, where it closes the `.cs-tab[data-key]` it lands on. It never reaches
    // `route_click` (that is the left-button path), so a middle press on the
    // bar's background cannot fall through to whatever is behind it.
    if pressed_button == Some(BarButton::Middle) {
        let chain = hit_chain_at(pos.x, pos.y).unwrap_or_default();
        return bar_aux(&chain).and_then(BarAction::into_nzk_ui_action);
    }
    // The DOM chain for this press, from the shell's own hit test. It is what
    // makes a child-element press resolve to its control (`resolve_selector`'s
    // ancestor walk) and, round 7, what makes the runtime-created `#cs-tabs` bar
    // — which has no box-map entry at all — routable. Empty when no resident
    // shell answers, in which case the box map fallback still answers every
    // registered control exactly as before.
    //
    // A range input is answered *first*, and by `route_drag`, because a press on
    // one is a value, not an action. The value is uploaded to `PENDING_SLIDER`
    // for the host to apply after this frame (the host owns `GraphView`), and the
    // press is then swallowed so a click on a slider cannot also mean "click
    // whatever is behind it".
    // Release is asked FIRST: a press and a release can arrive in the same egui
    // frame (a fast click, or a slow frame), and that gesture's last word is the
    // commit, not a `Live` value that nothing later commits.
    if ui.ctx().input(|i| i.pointer.primary_released()) {
        // Release commits: the reference's `change` handler, which is what
        // re-runs the layout (`shell.js:823-871`).
        if let Some(edit) = route_drag(&geo, &[], pos.x, pos.y, SliderPhase::Release) {
            push_slider(edit);
            return None;
        }
    } else if pressed {
        if let Some(edit) = route_drag(&geo, &[], pos.x, pos.y, SliderPhase::Live) {
            push_slider(edit);
            return None;
        }
    } else if ui.ctx().input(|i| i.pointer.primary_down()) {
        // Still dragging: keep publishing the live value. The delta is the
        // pointer's, so the value tracks the cursor rather than accumulating.
        if let Some(edit) = route_drag(&geo, &[], pos.x, pos.y, SliderPhase::Live) {
            push_slider(edit);
            return None;
        }
    }
    // A chain row's double press is the reference's collapse gesture
    // (`filters.js:610-619`). Routing is on the press edge, so the second press
    // above has already selected the row; egui reports the completed gesture on
    // the second RELEASE, which is where the children toggle belongs.
    if !pressed && double_released {
        let chain = hit_chain_at(pos.x, pos.y).unwrap_or_default();
        if let Some(id) = chain_hop_click(&chain) {
            PENDING_CHAIN_COLLAPSE.with(|q| q.borrow_mut().push(id));
        }
    }
    if !pressed {
        return None;
    }
    let chain = hit_chain_at(pos.x, pos.y).unwrap_or_default();
    // Input trace: the press point and the exact DOM chain the resolution below
    // will consume. Printed before any early return, so a press that is
    // swallowed (bar, divider, popup) is still visible in the log.
    eprintln!(
        "[input] nzk_ui press x={:.2} y={:.2} mods={} chain={}",
        pos.x,
        pos.y,
        egui_mods_token(ui.ctx()),
        chain_summary(&chain),
    );
    // The split divider's press is the START of a drag (`onDividerDown`): the
    // divider has no `CONTROLS`/`--boxes` entry (it is created at runtime by
    // `dom::apply_split`), so the shell's nearest-first chain is the only honest
    // resolver. The press is consumed for the whole drag; the position only
    // changes on the following pointer-move frames.
    if split_divider_hit(&chain) {
        if let Some(edit) = SPLIT_DRAG.with(|d| d.borrow_mut().begin(&chain)) {
            push_split(edit);
        }
        return None;
    }
    // The suggestion popup's rows are painted ABOVE the nzk_ui and behave like a
    // popup list: a press on one commits it and is always consumed
    // (`shell.js:527-536`). Asked before the bar so a `.sug` row can never be
    // read as whatever control sits behind the popup.
    if let Some(hit) = suggestion_row_click(&chain) {
        if let SuggestHit::Commit(id) = hit {
            push_suggestion(id);
        }
        return None;
    }
    // The Find panel's result rows (`.frow` inside `#findResults`), created at
    // runtime by `dom::apply_find`. A press on a row (or a child span) is
    // consumed and the row's click target is staged for the host: a def/impl
    // row selects+centres its node, a usage row opens its enclosing node
    // (`activateFindRow`, search.js:390-400).
    if let Some(target) = find_row_click(&chain) {
        PENDING_FIND_ROW.with(|q| q.borrow_mut().push(target));
        return None;
    }
    // The Blocks canvas's cards (`.block-card[data-id]`, created at runtime by
    // `dom::apply_blocks_canvas`). A press selects the card's node through the
    // same `view.selected` path a graph click uses and is always consumed, so it
    // cannot fall through to whatever is behind the panel.
    if let Some(id) = block_card_node(&chain) {
        PENDING_BLOCK_NODE.with(|q| q.borrow_mut().push(id));
        return None;
    }
    // The resolve-chain tree's `.hop` rows (runtime-created by `dom::apply_chain`).
    // A press selects that node, exactly like `filters.js:600-606`. The double
    // press's collapse half is staged on the release edge above.
    if let Some(id) = chain_hop_click(&chain) {
        push_chain_node(id);
        return None;
    }
    // Declared-type chips (`#typeChips .type-chip[data-key="type:<name>"]`),
    // created at runtime by `dom::apply_type_defs`. A press restricts the section
    // to exactly that declared type (or clears the restriction for `type:`), and
    // is always consumed so it cannot fall through to the canvas.
    if let Some(name) = type_chip_click(&chain) {
        push_type_filter(name);
        return None;
    }
    // Public-definitions / strings / declared-types list rows
    // (`.lrow[data-key]`): select the row's node, exactly like `openListNode`
    // (`filters.js:462-480`).
    if let Some(id) = list_row_click(&chain) {
        PENDING_LIST_NODE.with(|q| q.borrow_mut().push(id));
        return None;
    }
    // The docked drawer's crumb ancestry, relation-group ±/caret toggles and
    // `.prop-actions` row (task-2). Asked before the relation-row resolver: the
    // toggles are children of a row, and a press on one must not select the
    // neighbour. Runtime-created rows have no `--boxes`/CONTROLS entry.
    if let Some(click) = drawer_action_click(&chain) {
        push_drawer_click(click);
        return None;
    }
    // The docked node drawer's relation rows (`.rel-node[data-key]`). They are
    // painted inside `#nodeDrawer` and consume their press exactly like a bar
    // row: the neighbour is selected through the host's own selection path.
    if let Some(id) = relation_row_click(&chain) {
        push_drawer_node(id);
        return None;
    }
    // The tab bar is asked FIRST and consumes its press either way: it is painted
    // above the rest of the nzk_ui, so a press on it (including its background
    // and its disabled buttons) must never fall through to whatever the flat
    // `--boxes` map thinks is behind it. Disabled truth comes from the app's own
    // `Tabs` model, passed in by the caller.
    // An open form menu's option rows are runtime-created (`dom::apply_form`) and
    // have no `--boxes`/CONTROLS entry, so the shell's nearest-first chain is the
    // only resolver — exactly like the suggestion rows above. A press on a row is
    // consumed and becomes the same commit the keyboard Enter performs.
    if let Some(commit) = form_menu_click(&chain) {
        return Some(commit);
    }
    if let Some(bar) = bar_click(&chain, tabs) {
        return bar.into_nzk_ui_action();
    }
    // The split pane's collapse button is the other runtime-created press target
    // (round 8). It is a sibling resolver, not a bar branch, and it consumes the
    // press exactly like the bar does: a press on the collapse button must not
    // also mean "click whatever is behind the split pane".
    if let Some(bar) = split_collapse_click(&chain, tabs) {
        return bar.into_nzk_ui_action();
    }
    // The sidebar filter controls (`filters.js`). Runtime-created `.facet` /
    // `.edgekey` chips carry `data-id`, so their presses route from the same
    // chain; `#resetFilters`/`#pruneHidden` are answered by id. Consumed here,
    // before the generic `route_click`, so a chip press cannot fall through.
    if let Some(cmd) = filter_click(&chain) {
        return Some(NzkUiAction::Filter(cmd));
    }
    route_click(&geo, &chain, view, pos.x, pos.y)
}

/// Stage a slider edit for the host to apply after this frame.
///
/// `overlay` has no `&mut GraphView` access in the branch that computes it (and
/// giving it one would mean two mutable borrows of the view in one frame), so
/// the edit is queued here and drained by [`take_pending_sliders`] in `app.rs`,
/// which does own the view. The queue is a `Vec` rather than a single slot so a
/// frame that sees several pointer transitions cannot silently drop one; in
/// practice it holds at most one.
fn push_slider(edit: SliderEdit) {
    PENDING_SLIDER.with(|q| q.borrow_mut().push(edit));
}

/// Take every slider edit staged since the last call, oldest first.
///
/// The host drains this once per frame, immediately after [`paint_in`], and
/// applies each edit to `view.settings`. Empty on a frame with no slider input,
/// so the common path changes nothing.
pub fn take_pending_sliders() -> Vec<SliderEdit> {
    PENDING_SLIDER.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

thread_local! {
    /// Slider edits staged by [`overlay`] for the host to apply.
    static PENDING_SLIDER: RefCell<Vec<SliderEdit>> = const { RefCell::new(Vec::new()) };
    /// Suggestion row ids staged by [`overlay`] for the host to commit. Like
    /// the slider queue, `overlay` cannot reach the host's `SuggestionView`, so
    /// the press is staged here and drained after the nzk_ui pass.
    static PENDING_SUGGEST: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Node ids staged by a press on a docked node drawer's relation row. The
    /// rows are created at runtime by `dom::apply_drawer`, so — like the tab bar
    /// — they have no `--boxes` entry and the shell's hit chain is the only
    /// resolver. `overlay` cannot reach the host's `GraphView`, so the id is
    /// staged and the host selects it through the same path a graph click uses.
    static PENDING_DRAWER_NODE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Node ids staged by a press on a `.hop[data-key]` row of the resolve-chain
    /// tree (`filters.js:600-610`). Runtime-created rows have no `--boxes` entry,
    /// so the shell's hit chain is the only resolver.
    static PENDING_CHAIN_NODE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Node ids staged by a DOUBLE press on a chain row, to toggle its children
    /// (`filters.js:610-619`). A sibling of the selection queue because one
    /// gesture does both: select the node, then collapse/expand its hops.
    static PENDING_CHAIN_COLLAPSE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Node ids staged by a press on a public-definitions / strings list row
    /// (`.lrow[data-key]`, runtime-created by `dom::apply_pub_defs` /
    /// `apply_str_defs`). No `--boxes` entry exists for these rows.
    static PENDING_LIST_NODE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Stage a suggestion row's node id for the host to commit.
fn push_suggestion(id: String) {
    PENDING_SUGGEST.with(|q| q.borrow_mut().push(id));
}

/// Take every suggestion commit staged since the last call, oldest first.
pub fn take_pending_suggestions() -> Vec<String> {
    PENDING_SUGGEST.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// The node id of a press on a docked drawer relation row, if this press is one.
///
/// The rows are `.rel-node[data-key=<node id>]` (`drawer.js:296-340`), created
/// by `dom::apply_drawer`. Asked before the flat box map for the same reason the
/// tab bar is: a runtime-created row has no box entry, and the press must not
/// fall through to whatever the flat map thinks is behind the drawer.
fn relation_row_click(chain: &[HitNode]) -> Option<String> {
    for node in chain {
        let is_row = node
            .class
            .as_deref()
            .is_some_and(|c| c.split_whitespace().any(|part| part == "rel-node"));
        if is_row {
            return node.data_key.clone().filter(|k| !k.is_empty());
        }
    }
    None
}

/// Stage a relation row's node id for the host to select.
fn push_drawer_node(id: String) {
    PENDING_DRAWER_NODE.with(|q| q.borrow_mut().push(id));
}

/// Take every relation-row selection staged since the last call, oldest first.
pub fn take_pending_drawer_nodes() -> Vec<String> {
    PENDING_DRAWER_NODE.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// The node id of a press on a resolve-chain `.hop` row, if this press is one.
///
/// Rows are `.hop[data-key=<node id>]` (`filters.js:600-610`), created by
/// `dom::apply_chain`, so the shell's nearest-first hit chain is the only
/// resolver. A press on a child span still carries the nearest `data-key`.
fn chain_hop_click(chain: &[HitNode]) -> Option<String> {
    for node in chain {
        let is_hop = node
            .class
            .as_deref()
            .is_some_and(|c| c.split_whitespace().any(|part| part == "hop"));
        if is_hop {
            return node.data_key.clone().filter(|k| !k.is_empty());
        }
    }
    None
}

/// Stage a chain row's node id for the host to select.
fn push_chain_node(id: String) {
    PENDING_CHAIN_NODE.with(|q| q.borrow_mut().push(id));
}

/// Take every chain-row selection staged since the last call, oldest first.
pub fn take_pending_chain_nodes() -> Vec<String> {
    PENDING_CHAIN_NODE.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// Take every chain-row double-press staged since the last call.
pub fn take_pending_chain_collapses() -> Vec<String> {
    PENDING_CHAIN_COLLAPSE.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// A press on a public-definitions / strings list row (`.lrow[data-key]`).
///
/// `openListNode` (`filters.js:462-480`) selects the row's node and expands it;
/// the native selection half is the host's. Restricting to a chain that
/// contains `#pubList`/`#strList` keeps an unrelated `.lrow` from ever being
/// claimed.
fn list_row_click(chain: &[HitNode]) -> Option<String> {
    let in_list = chain.iter().any(|n| {
        matches!(
            n.id.as_deref(),
            Some("pubList") | Some("strList") | Some("typeList")
        )
    });
    if !in_list {
        return None;
    }
    for node in chain {
        let is_row = node
            .class
            .as_deref()
            .is_some_and(|c| c.split_whitespace().any(|part| part == "lrow"));
        if is_row {
            return node.data_key.clone().filter(|k| !k.is_empty());
        }
    }
    None
}

/// Take every list-row selection staged since the last call, oldest first.
pub fn take_pending_list_nodes() -> Vec<String> {
    PENDING_LIST_NODE.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

thread_local! {
    /// Declared-type chip presses staged by a press inside `#typeChips`.
    ///
    /// The chips are created at runtime by `dom::apply_type_defs` and carry
    /// `data-key = "type:<name>"`; `overlay` cannot reach `NzkUiState` (the host
    /// owns the filter), so the *name* is staged here and the host applies it
    /// through `actions::set_type_filter` after the nzk_ui pass. Same contract as
    /// the list-row and drawer queues.
    static PENDING_TYPE_FILTER: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// The type name of a press on a `.type-chip[data-key="type:<name>"]` inside
/// `#typeChips`, or `None` when the press is not on one.
///
/// `Some("")` is the `all` chip (`data-key="type:"`) — a real value meaning
/// "clear the restriction", not an absence. Restricting to `#typeChips` keeps an
/// unrelated `.type-chip` from being claimed, and the nearest-first chain makes a
/// press on the chip's `.tname`/`.tcount` children resolve to the chip exactly
/// as `closest('[data-key]')` would.
fn type_chip_click(chain: &[HitNode]) -> Option<String> {
    let in_chips = chain.iter().any(|n| n.id.as_deref() == Some("typeChips"));
    if !in_chips {
        return None;
    }
    for node in chain {
        let is_chip = node
            .class
            .as_deref()
            .is_some_and(|c| c.split_whitespace().any(|part| part == "type-chip"));
        if !is_chip {
            continue;
        }
        if let Some(key) = node.data_key.as_deref() {
            if let Some(name) = key.strip_prefix("type:") {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Take every declared-type chip press staged since the last call, oldest first.
pub fn take_pending_type_filters() -> Vec<String> {
    PENDING_TYPE_FILTER.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// Stage one declared-type chip press for the host to apply.
fn push_type_filter(name: String) {
    PENDING_TYPE_FILTER.with(|q| q.borrow_mut().push(name));
}

/// One press on a runtime-created docked-drawer control (task-2). The drawer's
/// crumb ancestry, relation-group `±`/caret toggles and `.prop-actions` row are
/// built by `dom::apply_drawer`, so — exactly like `.rel-node` — the shell's
/// nearest-first hit chain is the only resolver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DrawerClick {
    /// A `.crumb[data-key=<node id>]` press: navigate to that ancestor
    /// (`interaction.js:404-408`, `goToNode`).
    Select(String),
    /// A `.rel-toggle`/`.rel-caret`/`.rel-label` press: toggle one group.
    ToggleGroup(String, String),
    /// A `.prop-action[data-key="act:hidech:<node>"]` press.
    HideChildren(String),
    /// A `.prop-action[data-key="act:expand:<node>"]` press.
    Expand(String),
    /// A `.prop-action[data-key="act:chain:<node>"]` press (`#upBtn` twin).
    ResolveChain(String),
    /// A `.prop-action[data-key="act:focus:<node>"]` press: toggle "Only
    /// related nodes" in the filter model.
    FocusOnly(String),
    /// A `.prop-action[data-key="act:parents:<node>"]` press: toggle "Hide
    /// parents".
    FocusParents(String),
}

thread_local! {
    /// Presses on runtime-created drawer controls staged by `overlay` for the
    /// host, which owns the `GraphView` selection and the `NzkUiState` drawer
    /// model. Same contract as the slider/suggestion queues.
    static PENDING_DRAWER_CLICK: RefCell<Vec<DrawerClick>> = const { RefCell::new(Vec::new()) };
}

/// Stage a drawer-control press for the host to apply after the nzk_ui pass.
fn push_drawer_click(click: DrawerClick) {
    PENDING_DRAWER_CLICK.with(|q| q.borrow_mut().push(click));
}

/// Take every drawer-control press staged since the last call, oldest first.
pub fn take_pending_drawer_clicks() -> Vec<DrawerClick> {
    PENDING_DRAWER_CLICK.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// Resolve a press on one of the runtime-created docked-drawer controls.
///
/// Asked BEFORE [`relation_row_click`]: the `±`/caret/label spans live *inside*
/// a relation row, so a press there must become a group toggle rather than the
/// row's select. The row's own neighbours (`rn-label`, `rn-kind`, `dot`) carry
/// none of these classes and fall through to the row resolver.
fn drawer_action_click(chain: &[HitNode]) -> Option<DrawerClick> {
    fn has(node: &HitNode, name: &str) -> bool {
        node.class
            .as_deref()
            .is_some_and(|c| c.split_whitespace().any(|p| p == name))
    }
    for node in chain {
        if has(node, "crumb") {
            return node
                .data_key
                .clone()
                .filter(|k| !k.is_empty())
                .map(DrawerClick::Select);
        }
        if has(node, "rel-toggle") || has(node, "rel-caret") || has(node, "rel-label") {
            let key = node.data_key.clone().filter(|k| !k.is_empty())?;
            let (group_node, group_key) = key.split_once('|')?;
            if group_node.is_empty() || group_key.is_empty() {
                return None;
            }
            return Some(DrawerClick::ToggleGroup(
                group_node.to_string(),
                group_key.to_string(),
            ));
        }
        if has(node, "prop-action") {
            let key = node.data_key.clone().filter(|k| !k.is_empty())?;
            let (act, node_id) = key.strip_prefix("act:")?.split_once(':')?;
            if node_id.is_empty() {
                return None;
            }
            return Some(match act {
                "hidech" => DrawerClick::HideChildren(node_id.to_string()),
                "expand" => DrawerClick::Expand(node_id.to_string()),
                "chain" => DrawerClick::ResolveChain(node_id.to_string()),
                "focus" => DrawerClick::FocusOnly(node_id.to_string()),
                "parents" => DrawerClick::FocusParents(node_id.to_string()),
                _ => return None,
            });
        }
    }
    None
}

/// A hover transition on the Blocks canvas (v2 `scratch.js:424-454`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockHover {
    /// A card entered the pointer: build/refresh the peek for this node id.
    Show(String),
    /// The pointer left every card: hide the peek.
    Hide,
}

thread_local! {
    /// Node ids staged by a press on a `.block-card[data-id]` row.
    static PENDING_BLOCK_NODE: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// Hover transitions staged by the pointer path.
    static PENDING_BLOCK_HOVER: RefCell<Vec<BlockHover>> = const { RefCell::new(Vec::new()) };
    /// The card the pointer was over last frame, so a transition is staged once.
    static LAST_BLOCK_HOVER: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Take every Blocks-card selection staged since the last call, oldest first.
pub fn take_pending_block_nodes() -> Vec<String> {
    PENDING_BLOCK_NODE.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// Take every Blocks hover transition staged since the last call, oldest first.
pub fn take_pending_block_hovers() -> Vec<BlockHover> {
    PENDING_BLOCK_HOVER.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

fn push_block_hover(hover: BlockHover) {
    PENDING_BLOCK_HOVER.with(|q| q.borrow_mut().push(hover));
}

/// The node id a pointer at `chain` resolves to on the Blocks canvas, if any.
///
/// `.block-card` rows (and their `.port` children) are created at runtime by
/// `dom::apply_blocks_canvas`, so they have no `--boxes` entry and no registered
/// control; the shell's nearest-first hit chain is the only honest resolver. The
/// payload is the v2 card's `data-id` (`scratch.js:283`), carried by the shell's
/// nearest-`data-*` walk for a press on any child element.
fn block_card_node(chain: &[HitNode]) -> Option<String> {
    for node in chain {
        let in_card = node.class.as_deref().is_some_and(|classes| {
            classes
                .split_whitespace()
                .any(|part| part == "block-card" || part == "port")
        });
        if in_card {
            return node.data_id.clone().filter(|id| !id.is_empty());
        }
    }
    None
}

thread_local! {
    /// Find-result rows staged by a press inside `#findResults`. Like the tab bar
    /// and the relation rows, the `.frow` buttons are created at runtime by
    /// `dom::apply_find`, so they have no `--boxes` entry and the shell's hit
    /// chain is the only resolver. `overlay` cannot reach the host's `GraphView`,
    /// so the target is staged and the host activates it.
    static PENDING_FIND_ROW: RefCell<Vec<FindRowTarget>> = const { RefCell::new(Vec::new()) };
}

/// The click target of one Find result row — `activateFindRow`'s two branches
/// (`search.js:390-400`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FindRowTarget {
    /// A definition/implementation row: `openFindNode(node.id)`.
    Node(String),
    /// A usage row: `openUsage` opens `file:line`. The enclosing node id is
    /// carried too, because native has no file-model source pane — a node
    /// document is the honest source surface available.
    Usage { node_id: String, file: String, line: i64 },
}

/// Parse a row's `data-key` (`find:node:<id>` / `find:usage:<node>|<file>|<line>`).
fn parse_find_key(key: &str) -> Option<FindRowTarget> {
    let rest = key.strip_prefix("find:")?;
    if let Some(id) = rest.strip_prefix("node:") {
        return (!id.is_empty()).then(|| FindRowTarget::Node(id.to_string()));
    }
    let rest = rest.strip_prefix("usage:")?;
    let mut parts = rest.splitn(3, '|');
    let node_id = parts.next().unwrap_or("").to_string();
    let file = parts.next().unwrap_or("").to_string();
    let line = parts.next().and_then(|n| n.parse::<i64>().ok()).unwrap_or(0);
    (!file.is_empty()).then_some(FindRowTarget::Usage { node_id, file, line })
}

/// Resolve a press on a `#findResults` row, or `None` when the press is not on
/// one. The rows carry `.frow` + `data-key` (see `dom::apply_find`), and the
/// chain is nearest-first, so a press on a child span routes up to its row.
fn find_row_click(chain: &[HitNode]) -> Option<FindRowTarget> {
    for node in chain {
        let is_row = node
            .class
            .as_deref()
            .is_some_and(|c| c.split_whitespace().any(|part| part == "frow"));
        if is_row {
            return node
                .data_key
                .as_deref()
                .filter(|k| !k.is_empty())
                .and_then(parse_find_key);
        }
    }
    None
}

/// Take every Find-row activation staged since the last call, oldest first.
pub fn take_pending_find_rows() -> Vec<FindRowTarget> {
    PENDING_FIND_ROW.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// [`overlay`] with an explicit pointer position and DOM hit chain.
///
/// The headless-testable form of the click path: `overlay` reads egui's input,
/// this takes the same facts directly. Both go through [`route_click`], so a
/// test exercises the production resolution rather than a copy of it.
pub fn nzk_ui_click(
    geo: &FrameGeometry,
    chain: &[HitNode],
    view: &mut GraphView,
    x: f32,
    y: f32,
) -> Option<NzkUiAction> {
    if let Some(commit) = form_menu_click(chain) {
        return Some(commit);
    }
    route_click(geo, chain, view, x, y)
}

/// Resolve a press on an open form-menu row (`#csFormMenuOption<i>`) to its
/// commit action.
///
/// The rows are painted by `dom::apply_form` with a unique id per index
/// (`format!("csFormMenuOption{i}")`), so the chain id IS the payload — no new
/// hit-wire column is needed. Walks nearest-first and returns the first match;
/// `None` means "not a menu row", which falls through to normal routing.
pub fn form_menu_click(chain: &[HitNode]) -> Option<NzkUiAction> {
    chain.iter().find_map(|hit| {
        let id = hit.id.as_deref()?;
        let index = id.strip_prefix("csFormMenuOption")?;
        index.parse::<usize>().ok().map(NzkUiAction::FormMenuCommit)
    })
}

/// [`overlay`]'s slider half, with an explicit position and phase.
///
/// The headless-testable form of the drag path, exactly as [`nzk_ui_click`] is
/// for the click path: both go through [`route_drag`], so a test exercises the
/// production resolution rather than a copy of it.
pub fn nzk_ui_drag(
    geo: &FrameGeometry,
    chain: &[HitNode],
    x: f32,
    y: f32,
    phase: SliderPhase,
) -> Option<SliderEdit> {
    route_drag(geo, chain, x, y, phase)
}

/// Route one press to the **picker modal's own** controls, when it landed on
/// one.
///
/// The folder picker is a second dispatch family. [`nzk_ui_click`] answers
/// `Option<NzkUiAction>` — a `Copy`, payload-free request — and three of the
/// picker's controls cannot be expressed that way: a `#modalBody` row carries
/// the `data-nav` path it navigates to, `#modalGo` carries `#modalPathInput`'s
/// text, and `#modalSelect` commits a folder the host must then index. So the
/// picker gets its own resolution, and this is it.
///
/// It runs **before** [`route_click`] and wins: while `#modal` is open its
/// backdrop covers the viewport, so the only presses that reach here are ones
/// inside the dialog. Resolving them through the picker family first means the
/// `#modalClose`/`#modalCancel` ids cannot fall through to
/// `NzkUiAction::ToggleModal` — which *toggles*, and would re-open the dialog
/// a press was trying to close.
///
/// `input` is `#modalPathInput`'s current text, which the caller owns (the
/// nzk_ui holds no editable text field; `#modalGo` and a row press are read
/// from the shell's own DOM). `None` means the caller has no input value, in
/// which case only the payload-free picker controls can resolve.
pub fn picker_action(
    geo: &FrameGeometry,
    chain: &[HitNode],
    x: f32,
    y: f32,
    input: Option<&str>,
    rows: &[(Rect, &str)],
) -> Option<PickerAction> {
    // A `.pick[data-nav]` row first, resolved from the shell's OWN hit chain.
    // The row's `data-nav` is the absolute folder path it descends into, and the
    // chain walk means a press on the row's `.pname` child still names the row.
    // This is the correct answer independent of the row's height, the body's
    // scroll offset and how many entries the listing has — the earlier
    // equal-band rect approximation (`rows`) could name a different row.
    if let Some(nav) = chain.iter().find_map(|hit| hit.data_nav.as_deref()) {
        return Some(PickerAction::Open(nav.to_string()));
    }
    let selector = resolve_selector(geo, chain, x, y);
    if let Some(selector) = selector {
        if let Some(action) = PickerAction::from_selector(selector) {
            return Some(action);
        }
        if selector == "#modal"
            && chain.first().and_then(|hit| hit.id.as_deref()) == Some("modal")
        {
            return Some(PickerAction::Close);
        }
        if selector == "#modalGo" {
            return Some(PickerAction::Go(input.unwrap_or("").to_string()));
        }
    }
    // Compatibility fallback for an older nzk_ui child that does not emit the
    // `data-nav` column: the caller's row rects still name a row. It is used
    // only when the chain supplied no row payload, so a precise answer always
    // wins over the approximation.
    if let Some((_, nav)) = rows.iter().find(|(r, _)| r.contains(egui::pos2(x, y))) {
        return Some(PickerAction::Open((*nav).to_string()));
    }
    None
}

/// The interactive half of the folder picker: read a real press and resolve it
/// to a [`PickerAction`], or `None`.
///
/// `nzk_ui_ui`'s counterpart for the `#modal` family, and the reason the picker
/// needed one at all: the shell's modal is real HTML laid out by the real
/// stylesheet, so its controls have real boxes in the box map and a real press
/// can land on them. What was missing was the *resolution* — a `PickerAction`
/// that carries the row's `data-nav` or the input's text.
///
/// `rows` are the `.pick` row rects the caller rendered the listing into
/// (`data-nav` per row, from `PickerFacts::dirs`): the modal body's buttons have
/// no DOM id of their own, so the box map cannot name them and the caller — the
/// one that built them — supplies the mapping.
///
/// `input` is `#modalPathInput`'s text. The nzk_ui does not own a text field;
/// the shell paints the input and the host reads its value, which is how the
/// reference works too (`modalPathInputEl.value`, `shell.js:750`).
pub fn nzk_ui_picker_ui(
    ui: &egui::Ui,
    input: Option<&str>,
    rows: &[(Rect, &str)],
) -> Option<PickerAction> {
    let geo = geometry()?;
    if geo.drift.is_some() {
        return None;
    }
    // Press edge, matching `overlay`'s routing: egui's `button_clicked` would
    // drop a row press that drifted >6 px or was held >0.8 s, and the reference
    // row's DOM `click` has no such limits.
    let pressed = ui
        .ctx()
        .input(|i| i.pointer.button_pressed(egui::PointerButton::Primary));
    if !pressed {
        return None;
    }
    let pos = ui.ctx().pointer_latest_pos()?;
    let chain = hit_chain_at(pos.x, pos.y).unwrap_or_default();
    picker_action(&geo, &chain, pos.x, pos.y, input, rows)
}

/// The hierarchy panel's own controls (`hierarchy.js:421-443, 501-505`).
///
/// The counterpart of [`nzk_ui_picker_ui`] for the `.cs-hier-*` family, and it
/// exists for the same structural reason: the rows the panel paints carry a
/// payload the box map cannot express. A `.hop` row's identity is its **collapse
/// path** (`data-path`, built by the producer as `""` / `/0:<id>`), and the two
/// `<select>`s carry a direction and a depth. The nzk_ui's box map is geometry
/// only, so the caller — the one that published the panel's facts — supplies the
/// path list and this function decides *which* one the pointer landed on.
///
/// Priority is the reference's: a press inside the controls area hits a select;
/// otherwise a press on a row toggles that row. `Changed::Select*` is only
/// reported when the value genuinely differs from the current one, so a press on
/// the already-selected `<option>` does not re-walk the graph.
pub fn nzk_ui_hierarchy_ui(
    ui: &egui::Ui,
    state: &crate::actions::NzkUiState,
) -> Option<crate::actions::HierarchyMsg> {
    use crate::actions::HierarchyMsg;

    let geo = geometry()?;
    if geo.drift.is_some() {
        return None;
    }
    // Same press-edge contract as `overlay` and the picker: a row press must not
    // be lost to egui's 6 px / 0.8 s click limits, which the reference lacks.
    if !ui
        .ctx()
        .input(|i| i.pointer.button_pressed(egui::PointerButton::Primary))
    {
        return None;
    }
    let pos = ui.ctx().pointer_latest_pos()?;

    // The two selects first: they sit in `.cs-hier-controls`, above the body, so
    // a press there must not also be read as a row press.
    if let Some(rect) = box_rect(".cs-hier-controls").filter(|r| r.contains(pos)) {
        // The direction select is the first child, the depth select the second
        // (`hierarchy.js:469-470`). Splitting the controls box in half is the
        // honest reading of "which of the two did I press" when the box map
        // reports the container rather than the individual `<select>`s.
        let half = rect.width() * 0.5;
        if pos.x < rect.left() + half {
            let next = next_dir(state.hierarchy.dir());
            if next != state.hierarchy.dir() {
                return Some(HierarchyMsg::SetDir(next.to_string()));
            }
        } else {
            let depth = state.hierarchy.depth();
            let next = if depth >= 4 { 1 } else { depth + 1 };
            return Some(HierarchyMsg::SetDepth(next));
        }
        return None;
    }

    // Then the rows, in paint order. A `.hop` carries no DOM id and the box map
    // is keyed by selector, so the map cannot name row N; the body is split into
    // one equal band per row instead. That is the same reconstruction
    // `picker_row_rects` does for `#modalBody`, and it is honest in the same way:
    // the *paths* come from the panel's own facts, so the Nth band and the Nth
    // path are the same row by construction.
    let paths = hierarchy_row_paths(state);
    if paths.is_empty() {
        return None;
    }
    let body = box_rect(".cs-hier-body")?;
    if body.height() <= 0.0 || !body.contains(pos) {
        return None;
    }
    let band = body.height() / paths.len() as f32;
    let i = ((pos.y - body.top()) / band).floor().max(0.0) as usize;
    paths.get(i).cloned().map(HierarchyMsg::ToggleCollapse)
}

/// The next direction in the reference's own cycle order
/// (`both` -> `forward` -> `backward` -> `both`).
fn next_dir(dir: &str) -> &'static str {
    match dir {
        "forward" => "backward",
        "backward" => "both",
        _ => "forward",
    }
}

/// Every `.hop` row's collapse path, in paint order — the exact key
/// [`crate::app::hierarchy_wire::HierarchyView::toggle_collapse`] expects.
///
/// Derived from the same flattened rows `dom_facts` emits, so the click route
/// and the renderer cannot disagree about which path belongs to which row.
fn hierarchy_row_paths(state: &crate::actions::NzkUiState) -> Vec<String> {
    state.hierarchy.row_paths()
}

fn warn_once(msg: &str) {
    WARNED.with(|w| {
        if !w.get() {
            w.set(true);
            eprintln!("[native-csviewer] nzk_ui unavailable, using egui nzk_ui: {msg}");
        }
    });
}

/// Render one nzk_ui frame and build the texture.
///
/// **One layout pass, one child request.** The renderer is asked for geometry,
/// the scroll report, the raster and the paint token in a single round trip, so
/// the pixels and the geometry provably describe the same layout. Before this,
/// `nzk_ui` was spawned twice per rebuild — a paint pass and a `--boxes`
/// pass — which cost two html5ever/Stylo/Taffy parses (~33ms each) plus two
/// process spawns for ~11ms of actual rendering, and let the two passes drift.
///
/// The persistent child is a [`NzkUiClient`] held in a thread-local and reused
/// across rebuilds. It is torn down in `Drop`, and independently dies on stdin
/// EOF so it cannot outlive the app even if the app is killed.
///
/// A binary predating `--serve` still works: [`supports`] gates the fast path and
/// the one-shot `--out`/`--boxes` pair below remains as the fallback.
fn build(
    ctx: &egui::Context,
    repo: &Path,
    status: &str,
    facts: &serde_json::Value,
    w: u32,
    h: u32,
) -> Result<NzkUi, String> {
    let bin = find_bin().ok_or_else(|| {
        format!(
            "nzk_ui binary not found; set {BIN_ENV} (or legacy {BIN_ENV_LEGACY}) to \
             nzk_ui/target/release/nzk_ui"
        )
    })?;
    let repo_s = repo.to_string_lossy().into_owned();
    let facts_s = facts.to_string();

    let t0 = Instant::now();

    // ---- fast path: one request to the resident child ----
    if supports(&bin, "--serve") {
        match render_via_child(ctx, &bin, &repo_s, &facts_s, status, w, h, t0) {
            Ok(c) => return Ok(c),
            Err(e) => {
                // Fall through to the one-shot path rather than showing no
                // nzk_ui at all. The child is dropped first so a half-broken
                // one cannot be reused.
                SHELL.with(|s| *s.borrow_mut() = None);
                warn_once(&format!("persistent nzk_ui failed ({e}); using one-shot path"));
            }
        }
    }

    render_one_shot(ctx, &bin, &repo_s, &facts_s, status, w, h, t0)
}

thread_local! {
    /// The resident nzk_ui child, if one has been started successfully.
    static SHELL: RefCell<Option<NzkUiClient>> = const { RefCell::new(None) };
    /// The facts JSON the resident child last built its layout from, and the
    /// repo string. A hit query must replay exactly these, otherwise the child
    /// would lay out a different document (`serve.rs` keys its cache on the
    /// facts) and the chain would describe a layout that is not on screen.
    static LAST_REQ: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
}

/// Ask the resident child for one frame, starting it if necessary.
#[allow(clippy::too_many_arguments)]
fn render_via_child(
    ctx: &egui::Context,
    bin: &Path,
    repo_s: &str,
    facts_s: &str,
    status: &str,
    w: u32,
    h: u32,
    t0: Instant,
) -> Result<NzkUi, String> {
    SHELL.with(|slot| {
        let mut slot = slot.borrow_mut();
        // A child that died (OOM-killed, crashed) must be replaced, not reused.
        let needs_spawn = match slot.as_mut() {
            Some(c) => !c.is_alive() || c.bin() != bin,
            None => true,
        };
        if needs_spawn {
            *slot = Some(NzkUiClient::spawn(bin, (w, h), repo_s)?);
        }
        let client = slot.as_mut().expect("client spawned above");
        // The Blocks geometry channel: ask for the painted card/port boxes in
        // the SAME request/pass as the nzk_ui raster, so the next facts build
        // can anchor edges to what was actually laid out. Empty when the
        // selector channel is unavailable, which keeps the frame unchanged.
        let frame = client.request(
            (w, h),
            repo_s,
            Some(facts_s),
            &[],
            &crate::app::blocks_wire::PORT_RECT_SELECTORS,
        )?;
        LAST_REQ.with(|c| *c.borrow_mut() = Some((repo_s.to_string(), facts_s.to_string())));
        finish_frame(ctx, bin, frame, status, w, h, t0, "serve")
    })
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    /// The additive `@hitbox` row decodes to the row's measured rectangle, and
    /// an absent box is `None` — never a guessed rect.
    #[test]
    fn hitbox_row_decodes_the_measured_row_box() {
        let rect = hitbox_from_tsv(
            "@hit\t10.00\t20.00\t42\tbutton\t\trel-node\t\t\t\t\t\n\
             @hitbox\t10.00\t20.00\t14.00\t30.00\t220.50\t18.00\n",
        )
        .expect("measured box");
        assert_eq!(rect.min, Pos2::new(14.0, 30.0));
        assert_eq!(rect.width(), 220.5);
        assert_eq!(rect.height(), 18.0);
        assert_eq!(hitbox_from_tsv("@hitbox\t1.00\t2.00\tnone\n"), None);
        assert_eq!(hitbox_from_tsv(""), None);
    }

    /// A hit on a row's label (the deepest element) still names the row class,
    /// which is what turns on the hand cursor and the hover fill.
    #[test]
    fn a_label_hit_still_resolves_the_row_class() {
        let chain = vec![
            HitNode {
                node: Some("842".into()),
                class: Some("rn-label".into()),
                ancestor: false,
                ..Default::default()
            },
            HitNode {
                node: None,
                class: Some("btn rel-node".into()),
                ancestor: true,
                ..Default::default()
            },
        ];
        assert_eq!(button_row_class(&chain), Some("rel-node"));
        assert_eq!(button_row_class(&[HitNode::id("#relationsHead")]), None);
        assert_eq!(
            button_row_class(&[HitNode::class("hop cycle")]),
            Some("hop")
        );
    }
}

/// The shell's DOM hit chain for one press point, fetched from the resident
/// `--serve` child.
///
/// This is what makes the runtime-created `#cs-tabs` bar clickable: the bar has
/// no `CONTROLS` selector and no `--boxes` entry, so the flat map cannot name it
/// — only the shell's own hit test can, because the shell is the party that
/// laid the bar out and painted it. The request replays the **exact facts** the
/// frame on screen was built from ([`LAST_REQ`]), so `serve.rs` reuses that same
/// layout instead of laying out a different document; the returned `@hit` rows
/// are therefore answered by the same layout that painted the pixels.
///
/// `None` when there is no resident child (the one-shot fallback or
/// `CSVIEWER_NO_CHROME`) or the child cannot answer this point: the caller then
/// falls back to the box map, which is exactly the pre-round-7 behaviour for
/// every control that already has one.
fn hit_chain_at(x: f32, y: f32) -> Option<Vec<HitNode>> {
    hit_probe_at(x, y).map(|(chain, _)| chain)
}

/// The DOM hit chain plus the routed row's measured box for one point.
///
/// The box comes from `@hitbox`, measured by the same layout as the `@hit` row.
/// A pointer landing on a row's label therefore still yields the row rectangle,
/// which is what the hover/press affordance paints.
fn hit_probe_at(x: f32, y: f32) -> Option<(Vec<HitNode>, Option<Rect>)> {
    let size = NZK_UI.with(|slot| slot.borrow().as_ref().map(|c| c.size))?;
    let (repo, facts) = LAST_REQ.with(|c| c.borrow().clone())?;
    SHELL.with(|slot| {
        let mut slot = slot.borrow_mut();
        let client = slot.as_mut()?;
        // A probe reuses the layout that painted the frame: no repaint, no PNG
        // encode. It is what makes a press (and hover) cheap at high node counts.
        let frame = client.probe(size, &repo, Some(&facts), &[(x, y)]).ok()?;
        Some((hit_chain_from_tsv(&frame.tsv), hitbox_from_tsv(&frame.tsv)))
    })
}

/// Decode the additive `@hitbox` row: the painted border box of the element a
/// hit routes to (the row, not its label). `none`/missing means the layout could
/// not report a box, so the caller draws no affordance rather than a guess.
fn hitbox_from_tsv(tsv: &str) -> Option<Rect> {
    let line = tsv.lines().find(|line| line.starts_with("@hitbox\t"))?;
    let f: Vec<&str> = line["@hitbox\t".len()..].split('\t').collect();
    if f.len() < 6 || f[2] == "none" {
        return None;
    }
    let n = |i: usize| f[i].trim().parse::<f32>().ok();
    Some(Rect::from_min_size(
        Pos2::new(n(2)?, n(3)?),
        egui::vec2(n(4)?, n(5)?),
    ))
}

/// Decode the shell's `@hit` row for ONE point into the nearest-first **chain**:
/// the element the pointer hit, then every ancestor that carries an `id` or a
/// `class` (the shell's twelfth column).
///
/// This is the host half of the reference's `ev.target.closest(...)`. A browser
/// dispatches the click on the element under the pointer and each handler walks
/// up from there; this renderer reports the *deepest painted* element, which for
/// a `<button>`'s label is an anonymous inline box with no identity at all, and
/// for a row's child span is the span (`.rn-label`, `.dot`, `.fname`) rather
/// than the row (`.rel-node`, `.frow`). Matching only the first node made those
/// presses dead in their text area — the "I have to click some buttons twice"
/// report — while the same control's padding worked.
///
/// Ancestor entries carry the hit's `data-*` payloads because the shell resolves
/// them with the same `closest()` walk: the nearest `data-key` for the button's
/// label IS the button's own, and handing it to the `.prop-action` ancestor is
/// what makes `act:expand:...` resolvable. They are marked [`HitNode::ancestor`]
/// so [`resolve_hit_chain`] (the id -> control walk) never treats a drawer or a
/// modal container as the pressed control.
///
/// A row with no ancestors (an old shell, or a plain element) decodes to a
/// one-node chain, exactly the pre-trail behaviour. Malformed rows are skipped,
/// never guessed, so a truncated frame degrades to the `--boxes` fallback.
fn hit_chain_from_tsv(tsv: &str) -> Vec<HitNode> {
    let Some(line) = tsv.lines().find(|line| line.starts_with("@hit\t")) else {
        return Vec::new();
    };
    let f: Vec<&str> = line["@hit\t".len()..].split('\t').collect();
    if f.len() < 5 || f[2] == "none" {
        return Vec::new();
    }
    let clean = |s: Option<&str>| s.map(str::to_string).filter(|s| !s.is_empty());
    let hit = HitNode::shell_row_payload(
        f[2],
        f.get(4).copied(),
        f.get(5).copied(),
        f.get(6).copied(),
        f.get(7).copied(),
        f.get(8).copied(),
        f.get(9).copied(),
    )
    // The thirteenth column (appended after the ancestor trail) is the nearest
    // `title`. An older child stops at the trail, so `get(11)` is `None` and the
    // chain decodes exactly as before — the title is additive, never required.
    .with_title(f.get(11).copied());
    let mut chain = vec![hit.clone()];
    if let Some(trail) = f.get(10) {
        for tok in trail.split('|').filter(|tok| !tok.is_empty()) {
            let (id, class) = tok.split_once('@').unwrap_or(("", tok));
            chain.push(HitNode {
                node: None,
                id: clean(Some(id)),
                class: clean(Some(class)),
                data_key: hit.data_key.clone(),
                data_close: hit.data_close.clone(),
                data_id: hit.data_id.clone(),
                data_nav: hit.data_nav.clone(),
                title: None,
                ancestor: true,
            });
        }
    }
    chain
}

/// The original two-spawn path, kept for binaries without `--serve`.
#[allow(clippy::too_many_arguments)]
fn render_one_shot(
    ctx: &egui::Context,
    bin: &Path,
    repo_s: &str,
    facts_s: &str,
    status: &str,
    w: u32,
    h: u32,
    t0: Instant,
) -> Result<NzkUi, String> {
    let dir = std::env::temp_dir().join(format!(
        "csviewer-nzk_ui-{}-{w}x{h}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let png = dir.join("nzk_ui.png");
    let tsv = dir.join("boxes.tsv");
    let facts_path = dir.join("facts.json");
    let size = format!("{w}x{h}");
    let png_s = png.to_string_lossy().into_owned();
    let tsv_s = tsv.to_string_lossy().into_owned();
    let facts_s_path = facts_path.to_string_lossy().into_owned();

    // nzk_ui is a static rasteriser with no engine: it renders the status
    // line and the namespace/kind facets from `dom::Facts`. Its own
    // `Facts::for_corpus` is a fid-corpus fixture, so the app must supply the
    // real facts or the header disagrees with the picture.
    //   --facts  (preferred) the whole Facts document, including ns_count/kinds
    //   --status (older nzk_ui) only the status line
    // Probing `--help` keeps an older binary degrading to the fixture instead
    // of failing the whole nzk_ui.
    let has_facts = supports(bin, "--facts");
    let has_status = supports(bin, "--status");
    let mut extra: Vec<String> = Vec::new();
    if has_facts {
        std::fs::write(&facts_path, facts_s)
            .map_err(|e| format!("write {}: {e}", facts_path.display()))?;
        extra.push("--facts".to_string());
        extra.push(facts_s_path.clone());
    } else if has_status {
        extra.push("--status".to_string());
        extra.push(status.to_string());
    } else {
        warn_once("nzk_ui has no --facts/--status flag; it will show its fixture counts");
    }
    let extra_ref: Vec<&str> = extra.iter().map(String::as_str).collect();

    let mut paint_args: Vec<&str> = vec!["--out", &png_s, "--size", &size, "--repo", repo_s];
    paint_args.extend_from_slice(&extra_ref);
    let mut boxes_args: Vec<&str> = vec!["--boxes", &tsv_s, "--size", &size, "--repo", repo_s];
    boxes_args.extend_from_slice(&extra_ref);

    run(bin, &paint_args)?;
    run(bin, &boxes_args)?;

    let bytes = std::fs::read(&png).map_err(|e| format!("read {}: {e}", png.display()))?;
    let boxes_text =
        std::fs::read_to_string(&tsv).map_err(|e| format!("read {}: {e}", tsv.display()))?;
    finish_frame(
        ctx,
        bin,
        crate::app::nzk_ui_client::Frame {
            png: bytes,
            tsv: boxes_text,
            width: w,
            height: h,
        },
        status,
        w,
        h,
        t0,
        "one-shot",
    )
}

/// Decode the returned PNG and assemble a [`NzkUi`] from the accompanying TSV.
///
/// Shared by both transports, so the drift gate and geometry parsing cannot
/// diverge between the fast path and the fallback.
#[allow(clippy::too_many_arguments)]
fn finish_frame(
    ctx: &egui::Context,
    bin: &Path,
    frame: crate::app::nzk_ui_client::Frame,
    status: &str,
    w: u32,
    h: u32,
    t0: Instant,
    transport: &str,
) -> Result<NzkUi, String> {
    let img = image::load_from_memory(&frame.png)
        .map_err(|e| format!("decode nzk_ui.png: {e}"))?
        .to_rgba8();
    if img.dimensions() != (w, h) {
        return Err(format!(
            "nzk_ui produced {}x{}, expected {w}x{h}",
            img.dimensions().0,
            img.dimensions().1
        ));
    }
    let color = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
    let tex = ctx.load_texture("csviewer-nzk_ui", color, TextureOptions::LINEAR);

    let boxes_text = &frame.tsv;
    let boxes = parse_boxes(boxes_text);
    if boxes.is_empty() {
        return Err("nzk_ui produced no geometry".to_string());
    }
    // The `RECTS=` channel's `@rect` rows: measured Blocks card/port boxes from
    // the same layout pass that just painted this frame. Cache them for the
    // NEXT facts build (facts are produced before the request, so one frame of
    // feedback is the earliest possible). No rows — the one-shot transport, an
    // old binary, or a closed Blocks panel — clears the cache, which is exactly
    // the truthful-constants fallback.
    crate::app::blocks_wire::set_measured_geometry(
        crate::app::blocks_wire::geometry_from_tsv(boxes_text),
    );
    // D2 drift gate: when the renderer reports a correspondence token, prove the
    // geometry describes the pixels we are about to paint. This is a HARD check —
    // a mismatch fails the frame rather than routing clicks through geometry
    // that belongs to a different layout pass. A renderer that reports no token
    // leaves this `None`, which is recorded honestly as "unproven", never as a
    // pass.
    //
    // Under `--serve` this can no longer fail for the old reason (two
    // independent layout passes), because the token and the geometry come back
    // from the same pass; it now guards the decode path itself.
    let paint_digest = match parse_frame_token(boxes_text) {
        Some(token) => {
            check_correspondence(&token, (w, h), img.as_raw())
                .map_err(|e| format!("nzk_ui geometry/paint drift: {e}"))?;
            Some(token.digest)
        }
        None => None,
    };
    let side_scroll_max = parse_scroll_max(boxes_text);
    // A new layout invalidates any cached hit chain: the row the pointer was
    // over may have moved, disappeared or changed payload.
    hover_probe_invalidate();
    let pres = presentation();
    let build_ms = t0.elapsed().as_millis();
    eprintln!(
        "[native-csviewer] nzk_ui/{transport}: {} painted {w}x{h} in {build_ms}ms",
        bin.display()
    );
    Ok(NzkUi {
        tex,
        size: (w, h),
        status: status.to_string(),
        collapsed: Vec::new(),
        pane_open: pres.pane_open,
        side_scroll: pres.side_scroll,
        side_scroll_max,
        notice_nodes: 0,
        // Set by `paint_in` once it knows which facts produced this frame; the
        // builder cannot know it. `0` is "no frame yet" and never matches a
        // source document, so the frame that assigns it is the last rebuild.
        facts_digest: 0,
        boxes,
        paint_digest,
        drift: None,
        build_ms,
        bin: bin.to_path_buf(),
    })
}

/// `@scroll` rows report the engine's own clamped offset/max for the containers
/// `ui.scroll` named; the sidebar is the one that drives the UI.
fn parse_scroll_max(boxes_text: &str) -> f64 {
    let mut side_scroll_max = 0.0f64;
    for line in boxes_text.lines() {
        let mut it = line.split('\t');
        if it.next() != Some("@scroll") {
            continue;
        }
        let (_sel, _x, _y, _max_x, max_y) =
            (it.next(), it.next(), it.next(), it.next(), it.next());
        if let Some(v) = max_y.and_then(|v| v.trim().parse::<f64>().ok()) {
            side_scroll_max = side_scroll_max.max(v);
        }
    }
    side_scroll_max
}

/// Does this nzk_ui binary advertise `flag` in its `--help`?
fn supports(bin: &Path, flag: &str) -> bool {
    Command::new(bin)
        .arg("--help")
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout).contains(flag)
                || String::from_utf8_lossy(&o.stderr).contains(flag)
        })
        .unwrap_or(false)
}

fn run(bin: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new(bin)
        .env("CSVIEWER_FRONTEND_DIR", viewer_frontend_dir())
        .args(args)
        .output()
        .map_err(|e| format!("spawn {}: {e}", bin.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "{} {:?} failed ({}): {}",
            bin.display(),
            args,
            out.status,
            err.lines().last().unwrap_or("no stderr")
        ));
    }
    Ok(())
}

/// `selector\tx\ty\tw\th` (or `selector\tMISSING`) from nzk_ui `--boxes`.
fn parse_boxes(text: &str) -> BTreeMap<String, Rect> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let mut it = line.split('\t');
        let (Some(sel), Some(x), Some(y), Some(w), Some(h)) =
            (it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        if sel.starts_with('@') {
            continue; // `@scroll` / `@frame` metadata rows are not boxes
        }
        let (Ok(x), Ok(y), Ok(w), Ok(h)) = (
            x.trim().parse::<f32>(),
            y.trim().parse::<f32>(),
            w.trim().parse::<f32>(),
            h.trim().parse::<f32>(),
        ) else {
            continue; // `MISSING`
        };
        if w > 0.0 && h > 0.0 {
            map.insert(sel.to_string(), Rect::from_min_size(Pos2::new(x, y), egui::vec2(w, h)));
        }
    }
    map
}

/// The renderer's `@frame` correspondence token from a `--boxes` TSV.
///
/// Contract (agreed with the shell's one-pass protocol): a final row
/// `@frame\t<w>\t<h>\t<digest>` where `digest` is either the literal `none`
/// ("no token this pass") or a hex digest over the exact RGBA8 bytes painted for
/// the same layout pass. Returns `None` for `none`, a malformed row, or no row
/// at all — every one of which means "correspondence unproven", never "fine".
fn parse_frame_token(text: &str) -> Option<FrameToken> {
    for line in text.lines() {
        let mut it = line.split('\t');
        if it.next() != Some("@frame") {
            continue;
        }
        let (Some(w), Some(h), Some(digest)) = (it.next(), it.next(), it.next()) else {
            return None;
        };
        let digest = digest.trim();
        if digest == "none" || digest.is_empty() {
            return None;
        }
        return Some(FrameToken { size: (w.trim().parse().ok()?, h.trim().parse().ok()?), digest: digest.to_string() });
    }
    None
}

/// A parsed `@frame` row: the painted size and its content digest.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FrameToken {
    size: (u32, u32),
    digest: String,
}

/// Compare a frame token against the RGBA8 bytes that were actually painted.
///
/// This is the D2 drift gate. `Err` is a **hard failure**, not a warning: the
/// caller must stop routing clicks rather than serve them from geometry that
/// provably describes a different frame. A size disagreement is caught here too,
/// because a token for another size cannot describe these pixels.
fn check_correspondence(
    token: &FrameToken,
    painted_size: (u32, u32),
    painted_rgba: &[u8],
) -> Result<(), String> {
    if token.size != painted_size {
        return Err(format!(
            "--boxes geometry is for {}x{} but the painted frame is {}x{}",
            token.size.0, token.size.1, painted_size.0, painted_size.1
        ));
    }
    let expect = (painted_size.0 as usize) * (painted_size.1 as usize) * 4;
    if painted_rgba.len() != expect {
        return Err(format!(
            "painted buffer is {} bytes, expected {expect} for {}x{}",
            painted_rgba.len(),
            painted_size.0,
            painted_size.1
        ));
    }
    let got = digest_rgba(painted_rgba);
    if got != token.digest {
        return Err(format!(
            "--boxes and the painted PNG are different frames \
             (paint digest {got}, geometry pass digest {})",
            token.digest
        ));
    }
    Ok(())
}

/// A stable content fingerprint over RGBA8 bytes, as 16 lowercase hex chars.
///
/// **This is the wire format of the shell's `@frame` row**, and it is
/// deliberately named so nobody has to reverse it out of a failure message:
/// FNV-1a 64-bit, offset basis `0xcbf29ce484222325`, prime `0x100000001b3`, with
/// the byte length folded in at the end (`h ^= len; h *= prime`). The shell owes
/// this exact sequence — a plain FNV-1a loop *without* the length fold produces a
/// different digest, so a shell that copies only the loop will fail the gate with
/// what looks like real drift.
///
/// The correspondence check is only as meaningful as this agreement. It was
/// originally written here against a verbal spec while the shell independently
/// implemented SHA-256, and the mismatch surfaced as a hard gate failure on the
/// first end-to-end run — which is the intended behaviour: two frames that are
/// not the same frame were refused, not silently misrouted.
///
/// It is a **cheap content check for layout drift**, not a cryptographic
/// commitment — it exists to detect "these are not the same pixels", which is
/// exactly what a stale `--boxes`/PNG pair is.
pub fn digest_rgba(rgba: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in rgba {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Fold in the length so trailing zeroes cannot collide.
    h ^= rgba.len() as u64;
    h = h.wrapping_mul(0x0000_0100_0000_01b3);
    format!("{h:016x}")
}

/// The viewer's own frontend pair, handed to the nzk_ui child so a separate
/// module checkout still renders this host's stylesheet.
pub(crate) fn viewer_frontend_dir() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("../../frontend");
            if p.join("index.html").is_file() {
                return p;
            }
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../frontend")
}

/// Locate the nzk_ui executable: env override, then the module build layout.
fn find_bin() -> Option<PathBuf> {    if let Some(p) = env_bin() {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            cands.push(dir.join("../../nzk_ui/target/release/nzk_ui"));
            cands.push(dir.join("../../nzk_ui/target/debug/nzk_ui"));
            cands.push(dir.join("../../NZKRepoDeps/CSV Module (nzk-cs-rs_nzk_ui)/target/release/nzk_ui"));
            cands.push(dir.join("../../NZKRepoDeps/CSV Module (nzk-cs-rs_nzk_ui)/target/debug/nzk_ui"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        cands.push(cwd.join("nzk_ui/target/release/nzk_ui"));
        cands.push(cwd.join("../nzk_ui/target/release/nzk_ui"));
        cands.push(cwd.join("../NZKRepoDeps/CSV Module (nzk-cs-rs_nzk_ui)/target/release/nzk_ui"));
        cands.push(cwd.join("../NZKRepoDeps/CSV Module (nzk-cs-rs_nzk_ui)/target/debug/nzk_ui"));
    }
    cands.into_iter().find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus root these tests hand `actions::apply`.
    ///
    /// The crate's own directory: real, always present, and the folder
    /// `#browseBtn` is actually pointed at in a run from this checkout. Only
    /// `NzkUiAction::Browse` reads it, and the tests here never dispatch that
    /// action — they dispatch section, drawer and modal-toggle controls. It is
    /// still a real directory rather than a placeholder path so a future test
    /// that *does* open the picker through this helper gets a real listing.
    fn repo() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn parses_css_shell_boxes() {
        let map = parse_boxes(
            "#graph\t0.00\t52.00\t1240.00\t948.00\n#fitBtn\t193.00\t62.00\t41.00\t26.00\n\
             #layoutState\t420.00\t75.00\t5.00\t0.00\n.brand\tMISSING\n",
        );
        assert_eq!(map.len(), 2, "zero-size and MISSING rows are dropped");
        let g = map["#graph"];
        assert_eq!((g.min.x, g.min.y, g.width(), g.height()), (0.0, 52.0, 1240.0, 948.0));
        assert_eq!(map["#fitBtn"].min.x, 193.0);
    }

    #[test]
    fn canvas_rect_prefers_graph_over_wrap() {
        // exercised through the real parser to pin the selector order
        let map = parse_boxes("#canvasWrap\t0\t52\t1240\t948\n#graph\t0\t52\t1240\t948\n");
        assert!(map.contains_key("#graph") && map.contains_key("#canvasWrap"));
    }

    /// The status line is built from the app's graph, so a dense corpus reports
    /// its own counts — never nzk_ui's fid-corpus fixture (9 nodes / 2 files).
    #[test]
    fn status_text_comes_from_the_graph() {
        let graph = Graph {
            repo: PathBuf::from("/tmp/fid-dense"),
            nodes: vec![
                crate::model::Node { kind: "namespace".into(), ..Default::default() },
                crate::model::Node { kind: "class".into(), ..Default::default() },
            ],
            edges: vec![crate::model::Edge {
                from: 0,
                to: 1,
                kind: "contains".into(),
                count: 1,
            }],
            state: serde_json::json!({"fileCount": 18, "parseMs": 77, "indexed": true}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        assert_eq!(
            status_text(&graph),
            "files 18  ·  parse 77ms  ·  nodes 2  ·  edges 1"
        );
        // And it is not the fixture: 9/2 would mean the bug came back.
        assert!(!status_text(&graph).contains("nodes 9"));
        // v2 formats the numbers with thousands separators (reference session:
        // `edges 1,615`). Fixture counts are small; prove the formatter itself.
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1615), "1,615");
        assert_eq!(group_thousands(2466), "2,466");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    /// The facts document the sidebar is rendered from must describe the app's
    /// graph (namespace facet + kind counts), not nzk_ui's fixture.
    #[test]
    fn facts_json_describes_the_graph() {
        let graph = Graph {
            repo: PathBuf::from("/tmp/fid-dense"),
            nodes: vec![
                crate::model::Node { kind: "namespace".into(), ns: "NZK".into(), ..Default::default() },
                crate::model::Node { kind: "namespace".into(), ns: "NZK.Sub".into(), ..Default::default() },
                crate::model::Node { kind: "class".into(), ns: "NZK".into(), ..Default::default() },
                crate::model::Node { kind: "method".into(), ns: "NZK".into(), ..Default::default() },
            ],
            edges: vec![],
            state: serde_json::json!({"fileCount": 18, "parseMs": 64, "indexed": true}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let f = facts_json(&graph);
        assert_eq!(f["ns_count"], 2, "All namespaces (2)");
        assert_eq!(f["file_count"], 18);
        assert_eq!(f["node_count"], 4);
        assert_eq!(f["kinds"][0], serde_json::json!(["namespace", 2]));
        assert_eq!(f["kinds"][1], serde_json::json!(["class", 1]));
        assert_eq!(f["kinds"][2], serde_json::json!(["method", 1]));
    }

    /// Selecting a node must NOT publish Blocks content unless the Blocks
    /// drawer is open.
    ///
    /// `nzk_ui::dom::apply_blocks_canvas` clears `#blocksPanel`'s `hidden`
    /// class whenever content arrives, so publishing `ui.blocks` on every
    /// selection made a plain graph click open the Blocks panel — the wrong
    /// panel. The reference opens the Node drawer (`selectNode` ->
    /// `renderNodeDrawer`) and routes selection into Blocks only while Blocks is
    /// already open.
    #[test]
    fn blocks_facts_are_published_only_while_the_blocks_drawer_is_open() {
        let graph = Graph {
            repo: PathBuf::from("/tmp/blocks-gate"),
            nodes: vec![crate::model::Node {
                id: "N.A".into(),
                label: "A".into(),
                kind: "class".into(),
                file: "a.cs".into(),
                line: 1,
                end_line: 3,
                ..Default::default()
            }],
            edges: vec![],
            state: serde_json::json!({"indexed": true, "counts": {"nodes": 1, "edges": 0}}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let mut state = NzkUiState::default();
        state.selected_node = Some(0);

        // A selected node with the drawer shut: the node drawer's own facts are
        // published, but Blocks must stay absent so `apply_blocks_canvas` cannot
        // un-hide the panel.
        let closed = facts_json_with(&graph, &state);
        assert!(
            closed["ui"].get("blocks").is_none(),
            "selection alone must not open/publish the Blocks panel: {}",
            closed["ui"]
        );

        // Opening the Blocks drawer is what turns the content on.
        state.drawer_open = Some("blocksPanel");
        let open = facts_json_with(&graph, &state);
        assert!(
            open["ui"].get("blocks").is_some(),
            "an open Blocks drawer must publish its content: {}",
            open["ui"]
        );
    }

    /// D3 evidence: the sidebar's projects/warnings rows come from the engine.
    ///
    /// Runs the REAL engine over the engine's own project fixture corpus
    /// (`tools/fixtures/projects`, which genuinely has a `.sln`, four
    /// `.csproj` files and a deliberately unparseable one) and asserts that the
    /// emitted facts carry real rows rather than the old constants.
    ///
    /// `CSVIEWER_FACTS_EVIDENCE=1` prints the emitted JSON so the evidence can
    /// be pasted into a report; the assertions run either way.
    #[test]
    fn facts_json_carries_real_projects_and_a_derived_warning_count() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../tools/fixtures/projects");
        if !fixture.is_dir() {
            eprintln!("[d3] fixture corpus absent at {}; skipping", fixture.display());
            return;
        }
        let roots = vec![fixture.to_string_lossy().into_owned()];
        let (graph, _index) =
            crate::model::load_with_index(&fixture, &roots).expect("index the fixture corpus");

        let f = facts_json(&graph);

        if std::env::var_os("CSVIEWER_FACTS_EVIDENCE").is_some() {
            println!(
                "[d3] real facts from {}:\n{}",
                fixture.display(),
                serde_json::to_string_pretty(&f).unwrap_or_default()
            );
        }

        // 1. Projects are real rows with the renderer's key names.
        let projects = f["projects"].as_array().expect("projects array");
        assert!(!projects.is_empty(), "the fixture corpus has .sln/.csproj files");
        for p in projects {
            assert!(p["id"].is_string(), "ProjectRow.id");
            assert!(p["name"].is_string(), "ProjectRow.name");
            assert!(p["path"].is_string(), "ProjectRow.path");
            assert!(p["targets"].is_array(), "ProjectRow.targets");
            assert!(p["defines"].is_array(), "ProjectRow.defines");
            assert!(p["file_count"].is_u64(), "ProjectRow.file_count must be present");
            // The engine's own spelling must not leak into the renderer's shape.
            assert!(p.get("fileCount").is_none(), "engine key must be mapped away");
        }
        // The chip text is the *empty* state, so it must be blank when rows exist.
        assert_eq!(
            f["projects_empty_text"], "",
            "a corpus with projects must not ask for the empty chip"
        );

        // 2. The count is derived from the emitted rows, not a literal.
        let warnings = f["warnings"].as_array().expect("warnings array");
        assert_eq!(
            f["parser_warning_count"].as_u64().unwrap_or(999),
            warnings.len() as u64,
            "parser_warning_count must equal the emitted warning rows"
        );
        for w in warnings {
            assert!(w["file"].is_string(), "WarningRow.file");
            assert!(w["line"].is_u64(), "WarningRow.line");
            assert!(w["col"].is_u64(), "WarningRow.col");
            assert!(w["raw"].is_string(), "WarningRow.raw carries the verbatim line");
        }

        // 3. The two corpora demonstrably differ: this one has projects, and the
        //    count tracks whatever the engine actually reported here.
        eprintln!(
            "[d3] {} projects, {} warnings (parser_warning_count={})",
            projects.len(),
            warnings.len(),
            f["parser_warning_count"]
        );
    }
}

/// The hierarchy panel's publish point — the tests that keep task-11 honest.
///
/// ### Why these are shaped the way they are
///
/// Every field of `nzk_ui::dom::HierarchyFacts` is `#[serde(default)]`. So if
/// the `obj["hierarchy"]` insert is removed, **nothing errors**: the deserialiser
/// yields a default-valued struct and the renderer happily paints an empty,
/// untitled panel with two `<option>`-less selects. A test that asserted "the key
/// exists" or "the panel element is present" would therefore stay green with the
/// wiring deleted — the exact "guard that cannot fail" failure mode.
///
/// These tests instead assert the *content only the real payload can produce*:
/// the resolved title string, the option counts, and the row labels from a real
/// graph walk. Removing the insert makes them fail; that was verified by
/// mutation (see `a_closed_panel_leaves_the_document_byte_identical` and the
/// report accompanying task-11).
#[cfg(test)]
mod hierarchy_tests {
    use super::*;

    /// A four-node call graph: `main -> helper -> deep`, plus an `unrelated`.
    ///
    /// Real `Graph`/`Edge` values, so the walk in
    /// `hierarchy_wire::walk_call_section` runs over genuine edges rather than a
    /// fixture JSON blob.
    fn call_graph() -> Graph {
        let mut nodes: Vec<crate::model::Node> = Vec::new();
        for (id, label, kind) in [
            ("n1", "main", "method"),
            ("n2", "helper", "method"),
            ("n3", "deep", "method"),
            ("n4", "unrelated", "method"),
        ] {
            nodes.push(crate::model::Node {
                id: id.into(),
                label: label.into(),
                full: format!("NZK.{label}"),
                kind: kind.into(),
                file: "a.cs".into(),
                line: 1,
                ..Default::default()
            });
        }
        Graph {
            repo: PathBuf::from("/tmp/fid-hier"),
            nodes,
            edges: vec![
                crate::model::Edge { from: 0, to: 1, kind: "calls".into(), count: 1 },
                crate::model::Edge { from: 1, to: 2, kind: "calls".into(), count: 1 },
            ],
            state: serde_json::json!({"fileCount": 1, "parseMs": 1, "indexed": true}),
            skipped: Vec::new(),
            projects: Vec::new(),
        }
    }

    /// **The decisive test.** The panel is opened through the real action path,
    /// and the hierarchy facts are asserted to reach the document that
    /// `facts_json_with` builds — the same function `paint_in` feeds the shell.
    ///
    /// Goes RED if the `obj["hierarchy"]` insert is removed: the key would be
    /// absent and every field below would be unreachable. It also pins the key's
    /// **nesting** — `facts["hierarchy"]`, a top-level `Facts` field, not
    /// `facts["ui"]["hierarchy"]` (the siblings `find`/`picker` live under `ui`,
    /// which is exactly the trap this asserts against).
    #[test]
    fn an_open_panel_publishes_hierarchy_facts_the_renderer_can_reach() {
        let graph = call_graph();
        let mut state = NzkUiState::default();

        // Closed: the key must be ABSENT, not null. `None` and `null` both
        // deserialize to `Option::None`, but only "absent" leaves the shipped
        // index.html untouched — the byte-identical promise.
        let closed = facts_json_with(&graph, &state);
        assert!(
            closed.get("hierarchy").is_none(),
            "a closed panel must omit the key entirely, not send null"
        );

        // Open it the way the app does, through the message channel.
        let effect = crate::app::apply_hierarchy_msg(
            &mut state,
            &graph,
            crate::actions::HierarchyMsg::Open("n1".to_string()),
        );
        assert!(effect.repaint_nzk_ui, "opening the panel must request a repaint");
        assert!(state.hierarchy.is_open());

        let facts = facts_json_with(&graph, &state);

        // (1) The key reaches the TOP LEVEL of `Facts` — this is the assertion
        // that fails if the insert moves under `ui` or disappears.
        let hier = facts
            .get("hierarchy")
            .expect("the hierarchy key must be published at the Facts top level");

        // (2) Content only the real payload produces. A missing insert yields
        // `None` above; a default-valued struct (e.g. if the adapter shape were
        // wrong) yields an empty title and zero options below.
        assert_eq!(
            hier["title"], "Call hierarchy",
            "the title is `Call hierarchy`, not the raw cursor word"
        );
        assert_eq!(hier["dir"], "both");
        assert_eq!(hier["depth"], 2);

        // (3) BOTH selects have real options — the visible defect the task named
        // ("two empty dropdowns"). Counted, not merely "non-empty".
        let dirs = hier["dir_options"].as_array().expect("dir_options is a list");
        assert_eq!(dirs.len(), 3, "both/Outgoing/Incoming");
        let depths = hier["mode_options"]
            .as_array()
            .expect("the depth list must be published (wire name `mode_options`)");
        assert_eq!(depths.len(), 4, "depth 1..=4");
        assert_eq!(depths[1]["value"], "2", "the depth select carries depth 2 selected");
        assert_eq!(depths[1]["selected"], true);

        // (4) The sections carry the REAL WALK. `main -> helper -> deep` is two
        // hops, and the labels come from the graph's own nodes.
        let sections = hier["sections"].as_array().expect("sections is a list");
        assert!(
            !sections.is_empty(),
            "an open panel on a connected node must publish sections"
        );
        let captions: Vec<&str> = sections
            .iter()
            .map(|s| s["caption"].as_str().unwrap_or_default())
            .collect();
        assert!(
            captions.iter().any(|c| c.contains("Outgoing calls")),
            "dir=both walks forward, so an Outgoing section must be present: {captions:?}"
        );
        let outgoing = sections
            .iter()
            .find(|s| s["caption"].as_str().unwrap_or_default().contains("Outgoing"))
            .expect("the Outgoing section must exist");
        let rows = outgoing["rows"].as_array().expect("rows is a list");
        assert_eq!(
            rows.len(),
            3,
            "main + helper + deep, walked through the real edges: {rows:?}"
        );
        assert_eq!(rows[0]["label"], "main", "the root row is the walked node");
        assert_eq!(rows[1]["label"], "helper", "depth 1 is the callee");
        assert_eq!(rows[2]["label"], "deep", "depth 2 is the callee's callee");
        assert_eq!(rows[1]["edge"], "calls", "the row's edge is the real edge kind");

        // (5) The nesting assertion, stated outright: the ui channel must NOT
        // have grown a hierarchy key.
        assert!(
            facts["ui"].get("hierarchy").is_none(),
            "hierarchy is a top-level Facts field; `ui` must not carry it too"
        );
    }

    /// The walk honours `dir` and `depth`, and a change **reloads**.
    ///
    /// Proves the two selects are wired to a real re-walk rather than only
    /// mutating a field: `backward` must find the *caller* of `helper`, and
    /// depth 1 must cut the deeper row.
    #[test]
    fn changing_dir_and_depth_reloads_the_walk() {
        let graph = call_graph();
        let mut state = NzkUiState::default();
        crate::app::apply_hierarchy_msg(
            &mut state,
            &graph,
            crate::actions::HierarchyMsg::Open("n2".to_string()),
        );

        // `backward` from `helper` finds `main` — the incoming caller.
        crate::app::apply_hierarchy_msg(
            &mut state,
            &graph,
            crate::actions::HierarchyMsg::SetDir("backward".to_string()),
        );
        let facts = facts_json_with(&graph, &state);
        let sections = facts["hierarchy"]["sections"].as_array().unwrap();
        let incoming = sections
            .iter()
            .find(|s| s["caption"].as_str().unwrap_or_default().contains("Incoming"))
            .expect("dir=backward must publish an Incoming section");
        let labels: Vec<&str> = incoming["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["label"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            labels,
            vec!["helper", "main"],
            "backward walks to -> from, so the caller `main` must be found"
        );

        // Depth 1 must cut the second hop. The panel is on `helper`, so one hop
        // forward is `deep` alone — the root row is `helper` itself.
        crate::app::apply_hierarchy_msg(
            &mut state,
            &graph,
            crate::actions::HierarchyMsg::SetDir("forward".to_string()),
        );
        crate::app::apply_hierarchy_msg(
            &mut state,
            &graph,
            crate::actions::HierarchyMsg::SetDepth(1),
        );
        assert_eq!(state.hierarchy.depth(), 1);
        let facts = facts_json_with(&graph, &state);
        let outgoing = facts["hierarchy"]["sections"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["caption"].as_str().unwrap_or_default().contains("Outgoing"))
            .expect("forward section");
        let labels: Vec<&str> = outgoing["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["label"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            labels,
            vec!["helper", "deep"],
            "depth 1 from `helper` keeps the root plus one hop, and cuts no further"
        );

        // And depth 2 shows the same two rows here (there is no third hop), so
        // the depth change is genuinely re-walking rather than truncating a
        // stale list: the row COUNT is driven by the graph, not by the limit.
        crate::app::apply_hierarchy_msg(
            &mut state,
            &graph,
            crate::actions::HierarchyMsg::SetDepth(2),
        );
        assert_eq!(state.hierarchy.depth(), 2);
    }

    /// The generation guard: a walk started before a newer one cannot overwrite
    /// it (`hierarchy.js:549,562`, `begin_load`/`is_current`).
    ///
    /// This is the mechanism the `dir`/`depth` reloads depend on — without it a
    /// slow walk could install rows for a direction the user already left.
    #[test]
    fn a_superseded_walk_is_dropped() {
        let graph = call_graph();
        let mut state = NzkUiState::default();
        crate::app::apply_hierarchy_msg(
            &mut state,
            &graph,
            crate::actions::HierarchyMsg::Open("n1".to_string()),
        );

        // Start walk A, then start walk B before installing A's result.
        let gen_a = state.hierarchy.begin_load("n1");
        let gen_b = state.hierarchy.begin_load("n1");
        assert!(gen_b > gen_a, "each load takes a fresh generation");

        // A is now stale by construction.
        assert!(!state.hierarchy.is_current(gen_a), "walk A must be superseded");
        assert!(state.hierarchy.is_current(gen_b), "walk B is the live one");

        // The honest install path refuses A and accepts B. `set_call_sections`
        // is the only way sections reach the view, so a stale walk cannot land.
        let stale = vec![crate::app::hierarchy_wire::call_section("forward", None, false)];
        state.hierarchy.set_call_sections(gen_a, stale);
        let after_stale = state.hierarchy.sections().len();

        let fresh = crate::app::hierarchy_wire::with_hop_hints(vec![
            crate::app::hierarchy_wire::walk_call_section(&graph, "n1", "forward", 2),
        ]);
        state.hierarchy.set_call_sections(gen_b, fresh);
        assert!(
            state.hierarchy.sections().len() >= after_stale,
            "the live generation installs its own sections"
        );

        // And a reload is observably a NEW generation, so the guard is exercised
        // by the real select path rather than only by this test's hand-rolled one.
        let before = state.hierarchy.generation();
        crate::app::apply_hierarchy_msg(
            &mut state,
            &graph,
            crate::actions::HierarchyMsg::SetDepth(3),
        );
        assert!(
            state.hierarchy.generation() > before,
            "SetDepth must start a new load, which is what makes the guard live"
        );
    }

    /// The row-click route is keyed by the SAME paths the renderer emits, so the
    /// Nth row on screen and the Nth path a press toggles are the same row.
    #[test]
    fn row_paths_match_the_rendered_rows() {
        let graph = call_graph();
        let mut state = NzkUiState::default();
        crate::app::apply_hierarchy_msg(
            &mut state,
            &graph,
            crate::actions::HierarchyMsg::Open("n1".to_string()),
        );

        let paths = state.hierarchy.row_paths();
        let facts = facts_json_with(&graph, &state);
        let rendered: usize = facts["hierarchy"]["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["rows"].as_array().unwrap().len())
            .sum();
        assert_eq!(
            paths.len(),
            rendered,
            "one path per rendered row, or the band hit-test would miss: {paths:?}"
        );
        // Root first, then the child keyed by index:id — the reference's scheme.
        assert_eq!(paths[0], "", "the root row's collapse key is the empty base");
        assert_eq!(paths[1], "/0:n2", "the child is `/i:id` under the root");

        // Toggling the child collapses it, which must DROP its descendant from
        // both the DOM rows and the paths — the two must stay in lockstep.
        crate::app::apply_hierarchy_msg(
            &mut state,
            &graph,
            crate::actions::HierarchyMsg::ToggleCollapse("/0:n2".to_string()),
        );
        let facts = facts_json_with(&graph, &state);
        let rows = facts["hierarchy"]["sections"][0]["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2, "collapsing the child hides its descendant `deep`");
        assert_eq!(
            rows[1]["collapsed"], true,
            "the collapsed row itself is still painted, marked collapsed"
        );
        let collapsed_paths = state.hierarchy.row_paths();
        let collapsed_rendered: usize = facts["hierarchy"]["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["rows"].as_array().unwrap().len())
            .sum();
        assert_eq!(
            collapsed_paths.len(),
            collapsed_rendered,
            "the path list must shrink in step with the rows: {collapsed_paths:?}"
        );
        assert!(
            !collapsed_paths.iter().any(|p| p == "/0:n2/0:n3"),
            "the hidden descendant must leave the path list too: {collapsed_paths:?}"
        );
    }

    /// Opening with an unknown id opens nothing: the reference toasts and shows
    /// no panel, which is honest — not an empty panel that looks broken.
    #[test]
    fn opening_on_an_unknown_node_shows_nothing() {
        let graph = call_graph();
        let mut state = NzkUiState::default();
        let effect = crate::app::apply_hierarchy_msg(
            &mut state,
            &graph,
            crate::actions::HierarchyMsg::Open("does-not-exist".to_string()),
        );
        assert!(!state.hierarchy.is_open(), "no node, no panel");
        assert!(
            facts_json_with(&graph, &state).get("hierarchy").is_none(),
            "and therefore no hierarchy key"
        );
        assert!(effect.repaint_nzk_ui, "the press is still a real action");
    }

    /// Esc closes the panel and the key is thereby consumed by it.
    #[test]
    fn closing_removes_the_key_again() {
        let graph = call_graph();
        let mut state = NzkUiState::default();
        crate::app::apply_hierarchy_msg(
            &mut state,
            &graph,
            crate::actions::HierarchyMsg::Open("n1".to_string()),
        );
        assert!(facts_json_with(&graph, &state).get("hierarchy").is_some());

        crate::app::apply_hierarchy_msg(&mut state, &graph, crate::actions::HierarchyMsg::Close);
        assert!(!state.hierarchy.is_open());
        assert!(
            facts_json_with(&graph, &state).get("hierarchy").is_none(),
            "closing must return the document to the shipped byte-identical state"
        );
    }
}

/// Dispatch-table and nzk_ui-state tests. Kept in their own module because the
/// file already has one `mod tests` for the geometry helpers.
#[cfg(test)]
mod dispatch_tests {
    use super::*;

    /// The corpus root for `actions::apply` — see `interaction_tests::repo`.
    use super::interaction_tests::repo;

    /// The nzk_ui binary these tests drive, or `None` to skip.
    pub(super) fn shell_bin() -> Option<PathBuf> {
        if let Some(p) = env_bin() {
            let p = PathBuf::from(p);
            if p.is_file() {
                return Some(p);
            }
        }
        find_bin().or_else(|| {
            let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../nzk_ui/target/release/nzk_ui");
            if p.is_file() { return Some(p); }
            let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../NZKRepoDeps/CSV Module (nzk-cs-rs_nzk_ui)/target/release/nzk_ui");
            if p.is_file() { return Some(p); }
            let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../NZKRepoDeps/CSV Module (nzk-cs-rs_nzk_ui)/target/debug/nzk_ui");
            p.is_file().then_some(p)
        })
    }

    /// The minimum `dom::Facts` document nzk_ui accepts, with `ui` spliced in.
    pub(super) fn minimal_facts(ui: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "repo": "/tmp/fid-corpus",
            "status_override": "files 2  ·  parse 50ms  ·  nodes 9  ·  edges 10",
            "kinds": [["method", 4], ["class", 3], ["namespace", 2]],
            "ns_count": 2,
            "indexed": true,
            "file_count": 2, "parse_ms": 50, "node_count": 9, "edge_count": 10,
            "projects_empty_text": "no .sln/.csproj found",
            "parser_warning_count": 0, "nodes_in_view": 2,
            "ui": ui,
        })
    }

    pub(super) fn work_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("csviewer-nzk_ui-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    pub(super) fn boxes_for(bin: &Path, facts: &serde_json::Value, dir: &Path) -> BTreeMap<String, Rect> {
        let facts_path = dir.join("facts-boxes.json");
        let tsv = dir.join("boxes.tsv");
        std::fs::write(&facts_path, facts.to_string()).expect("write facts");
        let out = Command::new(bin)
            .arg("--facts").arg(&facts_path)
            .arg("--boxes").arg(&tsv)
            .arg("--size").arg("1600x1000")
            .output()
            .expect("spawn nzk_ui --boxes");
        assert!(out.status.success(), "nzk_ui --boxes failed: {}", String::from_utf8_lossy(&out.stderr));
        parse_boxes(&std::fs::read_to_string(&tsv).expect("read boxes"))
    }

    fn paint_for(bin: &Path, facts: &serde_json::Value, dir: &Path, name: &str) -> Vec<u8> {
        let facts_path = dir.join(format!("facts-{name}.json"));
        let png = dir.join(format!("{name}.png"));
        std::fs::write(&facts_path, facts.to_string()).expect("write facts");
        let out = Command::new(bin)
            .arg("--facts").arg(&facts_path)
            .arg("--out").arg(&png)
            .arg("--size").arg("1600x1000")
            .output()
            .expect("spawn nzk_ui");
        assert!(out.status.success(), "nzk_ui failed: {}", String::from_utf8_lossy(&out.stderr));
        std::fs::read(&png).expect("read png")
    }

    #[test]
    fn dispatch_table_is_keyed_by_dom_selector_never_by_node_id() {
        assert!(!CONTROLS.is_empty());
        for c in CONTROLS {
            assert!(
                c.selector.starts_with('#') || c.selector.starts_with('.'),
                "registry entry {:?} is not a DOM selector",
                c.selector
            );
            assert!(
                !c.selector.chars().any(|ch| ch.is_ascii_digit()),
                "registry entry {:?} looks like a node id; ids are not stable",
                c.selector
            );
        }
        // Every section control must resolve to the ONE owner of that state.
        for c in CONTROLS.iter().filter(|c| c.owner == ControlOwner::Section) {
            assert_eq!(
                NzkUiAction::from_selector(c.selector),
                NzkUiAction::ToggleSection(c.selector),
                "{} must map to the host's ToggleSection",
                c.selector
            );
        }

        // ANTI-DRIFT: the registry and `actions::NzkUiAction::from_selector`
        // are two lists of the same fact, and a split between them shipped once
        // (`#blocksBtn` was `Inert` here while `from_selector` returned
        // `OpenBlocks`). A control this table calls wired must be effective, and
        // one it calls Inert must be a no-op.
        for c in CONTROLS {
            let action = NzkUiAction::from_selector(c.selector);
            match c.owner {
                ControlOwner::Inert => assert_eq!(
                    action,
                    NzkUiAction::None,
                    "{} is listed Inert but from_selector returns {}",
                    c.selector,
                    action.label()
                ),
                ControlOwner::Host | ControlOwner::Section | ControlOwner::Local => assert!(
                    action.is_effective(),
                    "{} is listed {:?} but from_selector returns None",
                    c.selector,
                    c.owner
                ),
                // The picker family answers a DIFFERENT resolver. Asserting the
                // right one is the whole point: a `Picker` control that
                // `picker_action` cannot name is a painted button with no
                // consequence, which is the dead-control bug this task fixes.
                ControlOwner::Picker => {
                    // `#modalClose`/`#modalCancel` are the ONE honest overlap:
                    // they close the modal either way, so `from_selector`
                    // answering `ToggleModal(FolderPicker)` is not a second,
                    // competing owner — it is the same semantics reached through
                    // the older channel. Every other picker id must be
                    // picker-only, so a future id cannot quietly acquire two
                    // different meanings.
                    let shared_close = matches!(c.selector, "#modalClose" | "#modalCancel");
                    if !shared_close {
                        assert_eq!(
                            action,
                            NzkUiAction::None,
                            "{} belongs to the picker family; from_selector must not also claim \
                             it (two owners for one id is the drift this test prevents)",
                            c.selector
                        );
                    }
                    assert!(
                        PickerAction::from_selector(c.selector).is_some()
                            || c.selector == "#modalGo",
                        "{} is listed Picker but picker_action cannot resolve it",
                        c.selector
                    );
                }
                // The slider family answers a THIRD resolver, and the same
                // mirror rule applies: a `Slider` control must be resolvable by
                // `SliderKind::from_selector` (or it is a painted control with no
                // value), and it must NOT also be claimed by `from_selector` (or
                // one press would mean two things).
                ControlOwner::Slider => {
                    assert!(
                        SliderKind::from_selector(c.selector).is_some(),
                        "{} is listed Slider but SliderKind cannot resolve it",
                        c.selector
                    );
                    assert_eq!(
                        action,
                        NzkUiAction::None,
                        "{} is a slider; from_selector must not also claim it",
                        c.selector
                    );
                }
            }
        }
    }

    /// The D2/D3 property, now on the unified path: a nzk_ui hit becomes a host
    /// action, the host applies it to its single `NzkUiState`, the facts change,
    /// and the whole nzk_ui repaints while the top bar stays byte-equal.
    #[test]
    fn section_hit_changes_host_state_and_repaints_the_whole_nzk_ui() {
        let Some(bin) = shell_bin() else {
            eprintln!("nzk_ui: skip (nzk_ui not built)");
            return;
        };
        let dir = work_dir("state");

        // THE host state. nzk_ui.rs holds no collapsed list of its own.
        let mut state = NzkUiState::default();
        let before_facts = minimal_facts(ui_state_json(&state, None));

        // Real geometry from the real renderer, not a hand-written rectangle.
        let boxes = boxes_for(&bin, &before_facts, &dir);
        let head = *boxes.get("#filtersHead").expect("#filtersHead geometry");
        let (cx, cy) = (head.center().x, head.center().y);

        // 1. resolve a synthetic hit through the dispatch table
        let hit = hit_in(&boxes, cx, cy).expect("a hit inside #filtersHead");
        assert_eq!(hit, "#filtersHead", "smallest containing control must win");

        // 2. it becomes the host's action, and the host's state changes.
        //    The host seeds the shipped three (`#pubHead`/`#strHead`/`#chainHead`),
        //    so the assertion is a *delta*: exactly one section is added, and it
        //    is the one that was hit.
        let before_collapsed: Vec<String> =
            state.collapsed_facts().iter().map(|s| s.to_string()).collect();
        let action = NzkUiAction::from_selector(hit);
        assert_eq!(action, NzkUiAction::ToggleSection("#filtersHead"));
        assert!(!state.is_collapsed("#filtersHead"), "not collapsed to begin with");
        let effect = crate::actions::apply(&mut state, action, &repo());
        assert!(effect.repaint_nzk_ui, "a section toggle must repaint the nzk_ui");
        assert!(state.is_collapsed("#filtersHead"), "host state must record it");
        let after_collapsed: Vec<String> =
            state.collapsed_facts().iter().map(|s| s.to_string()).collect();
        assert_ne!(before_collapsed, after_collapsed, "the collapsed set must change");
        assert_eq!(after_collapsed.len(), before_collapsed.len() + 1, "a pure delta");
        assert!(after_collapsed.iter().any(|s| s == "#filtersHead"));

        // 3. the facts the nzk_ui renders from changed with the host state
        let after_facts = minimal_facts(ui_state_json(&state, None));
        assert_ne!(before_facts["ui"], after_facts["ui"], "facts must carry the new state");

        // 4. the repainted nzk_ui really is different pixels
        let before_png = paint_for(&bin, &before_facts, &dir, "before");
        let after_png = paint_for(&bin, &after_facts, &dir, "after");
        assert_ne!(before_png, after_png, "the nzk_ui must repaint from the new state");

        // 5. and the change is confined to the nzk_ui: the top bar is byte-equal.
        let a = image::load_from_memory(&before_png).expect("decode before").to_rgba8();
        let b = image::load_from_memory(&after_png).expect("decode after").to_rgba8();
        assert_eq!(a.dimensions(), b.dimensions());
        let top_rows = |img: &image::RgbaImage| {
            let bytes = 58 * img.width() as usize * 4;
            img.as_raw()[..bytes].to_vec()
        };
        assert_eq!(top_rows(&a), top_rows(&b), "top bar must not move with sidebar state");
    }

    /// task-30: the pane-open grid resolves from the real stylesheet, the pane
    /// rect matches the host's native pane, the sidebar is scrollable, and the
    /// region hit-test separates pane / sidebar / canvas.
    #[test]
    fn pane_open_grid_scroll_and_region_hit_test() {
        let Some(bin) = shell_bin() else {
            eprintln!("nzk_ui: skip (nzk_ui not built)");
            return;
        };
        let dir = work_dir("pane");

        // --- pane closed: the shipped geometry golden --------------
        let closed = minimal_facts(serde_json::json!({
            "collapsed": null, "active_mode": null, "pane_open": false, "scroll": []
        }));
        let cb = boxes_for(&bin, &closed, &dir);
        let canvas_closed = *cb.get("#graph").expect("#graph");
        assert!(
            !cb.contains_key("#sourcePane") || cb["#sourcePane"].height() < 1.0,
            "the pane must be closed by default"
        );

        // --- pane open: the real `[1fr, 34vh]` grid ----------------
        let open = minimal_facts(serde_json::json!({
            "collapsed": null, "active_mode": null, "pane_open": true,
            "scroll": [{ "selector": ".side", "x": 0.0, "y": 240.0 }]
        }));
        let ob = boxes_for(&bin, &open, &dir);
        let pane = *ob.get("#sourcePane").expect("#sourcePane geometry");
        let canvas_open = *ob.get("#graph").expect("#graph");
        let side = *ob.get(".side").expect(".side geometry");

        // The pane is the bottom 34% of the window and spans both columns.
        assert_eq!(pane.left(), 0.0);
        assert_eq!(pane.width(), 1600.0);
        assert!((pane.height() - 340.0).abs() < 1.0, "34vh of 1000 = 340, got {}", pane.height());
        assert!((pane.top() - 660.0).abs() < 1.0, "pane top = 1000-340, got {}", pane.top());
        // The pane takes its row out of the canvas row: 1000 - 52 topbar = 948
        // of layout height, minus the 340 pane row = 608 for the canvas. This is
        // the real grid, and 608 is also what the old hand-cut 66vh frame
        // produced — which is why the graph does not move relative to before.
        assert_eq!(canvas_closed.height(), 948.0);
        assert_eq!(canvas_open.height(), 608.0);
        assert_eq!(canvas_closed.height() - canvas_open.height(), pane.height());
        // The sidebar lives in row 1 beside the canvas.
        assert_eq!(side.height(), canvas_open.height());

        // --- the sidebar scrolls: offset changes what is painted ----
        assert_eq!(current_scroll_max_for(&bin, &dir), 0.0, "pane closed -> nothing to scroll");
        let scrolled_max = scroll_max_for(&bin, &dir, 240.0);
        assert!(scrolled_max > 100.0, "with the pane open the sidebar must overflow, max {scrolled_max}");

        // --- region hit-test ---------------------------------------
        // These boxes are for a closed frame; classify against it directly.
        let closed_boxes = cb.clone();
        let classify = |boxes: &BTreeMap<String, Rect>, x: f32, y: f32| -> Region {
            let inside =
                |sel: &str| boxes.get(sel).map(|r| r.contains(Pos2::new(x, y))).unwrap_or(false);
            if inside("#sourcePane") {
                Region::SourcePane
            } else if inside(".side") {
                Region::Sidebar
            } else if inside("#graph") {
                Region::Canvas
            } else {
                Region::NzkUi
            }
        };
        assert_eq!(classify(&closed_boxes, 600.0, 400.0), Region::Canvas);
        assert_eq!(classify(&closed_boxes, 1400.0, 400.0), Region::Sidebar);
        assert_eq!(classify(&closed_boxes, 100.0, 20.0), Region::NzkUi);
        // With the pane open, the bottom row is the pane, not the canvas.
        assert_eq!(classify(&ob, 600.0, 800.0), Region::SourcePane);
        assert_eq!(classify(&ob, 1400.0, 400.0), Region::Sidebar);
        assert_eq!(classify(&ob, 600.0, 300.0), Region::Canvas);
    }

    /// A press in the graph body must not be claimed by the nzk_ui registry.
    ///
    /// This is the headless half of the task-37 click diagnosis. Under the old
    /// hand-placed `ui.put` overlay this mattered because an invisible hit area
    /// covering the canvas would eat the press before `view.ui`'s
    /// `ui.allocate_rect(canvas, Sense::click_and_drag())` saw it. The overlay is
    /// gone (D2: real event routing, no widget per control), but the property
    /// still has to hold, because `route_click` now resolves the press against
    /// the box map directly. Sampled over the real geometry, the graph body
    /// (below the `.canvas-tools` band) must resolve to no control at all.
    #[test]
    fn nzk_ui_hit_areas_do_not_claim_the_graph_body() {
        let Some(bin) = shell_bin() else {
            eprintln!("nzk_ui: skip (nzk_ui not built)");
            return;
        };
        let dir = work_dir("canvas");
        let facts = minimal_facts(serde_json::json!({
            "collapsed": null, "active_mode": null, "pane_open": false, "scroll": []
        }));
        let boxes = boxes_for(&bin, &facts, &dir);
        let graph = *boxes.get("#graph").expect("#graph geometry");

        // `.canvas-tools` legitimately sits INSIDE the canvas box (its buttons
        // are nzk_ui controls over the graph), so sample the body below it.
        let mut claimed = Vec::new();
        for ix in 1..10 {
            for iy in 3..10 {
                let x = graph.left() + graph.width() * ix as f32 / 10.0;
                let y = graph.top() + graph.height() * iy as f32 / 10.0;
                if let Some(sel) = hit_in(&boxes, x, y) {
                    claimed.push(format!("({x:.0},{y:.0}) -> {sel}"));
                }
            }
        }
        assert!(
            claimed.is_empty(),
            "the nzk_ui registry claims graph-body points and would eat the press: {claimed:?}"
        );
    }

    /// A synthetic click in the graph body must reach the canvas widget, not a
    /// nzk_ui control.
    ///
    /// This reproduces `nzk_ui_ui`'s REAL structure in a headless egui context —
    /// the canvas child (`view.ui` allocates the `#graph` rect with
    /// `Sense::click_and_drag()`) and the production `overlay`, which no longer
    /// places any widget: it resolves the press through the painted geometry. The
    /// point of the test is that the canvas STILL gets the click, which it would
    /// not if `route_click` claimed a control there. It is the task-37
    /// interception question answered without touching X input.
    #[test]
    fn canvas_click_reaches_the_canvas_not_the_nzk_ui_registry() {
        let Some(bin) = shell_bin() else {
            eprintln!("nzk_ui: skip (nzk_ui not built)");
            return;
        };
        let dir = work_dir("click");
        let facts = minimal_facts(serde_json::json!({
            "collapsed": null, "active_mode": null, "pane_open": false, "scroll": []
        }));
        let boxes = boxes_for(&bin, &facts, &dir);
        let graph_rect = *boxes.get("#graph").expect("#graph geometry");
        let chip = egui::pos2(
            graph_rect.left() + graph_rect.width() * 0.5,
            graph_rect.top() + graph_rect.height() * 0.55,
        );

        // The production resolution of this press: it must name no control, so
        // the press is left for the canvas.
        let geo = FrameGeometry::unverified(boxes.clone(), (1600, 1000));
        let mut view = interaction_tests::view();
        assert_eq!(
            nzk_ui_click(&geo, &[], &mut view, chip.x, chip.y),
            None,
            "the graph body must not resolve to a nzk_ui control"
        );

        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1440.0, 900.0));
        let mut canvas_clicked = false;

        for frame in 0..3 {
            let events = if frame == 0 {
                vec![egui::Event::PointerMoved(chip)]
            } else {
                vec![
                    egui::Event::PointerMoved(chip),
                    egui::Event::PointerButton {
                        pos: chip,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::default(),
                    },
                    egui::Event::PointerButton {
                        pos: chip,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::default(),
                    },
                ]
            };
            let input = egui::RawInput {
                screen_rect: Some(screen),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    // Exactly what `nzk_ui_ui` does: the canvas child, with no
                    // nzk_ui widget stacked on top of it any more.
                    let mut child = ui.new_child(UiBuilder::new().max_rect(graph_rect));
                    let resp = child.allocate_rect(graph_rect, egui::Sense::click_and_drag());
                    if resp.clicked() {
                        canvas_clicked = true;
                    }
                });
            });
            // A headless context has no renderer to apply the font/texture
            // deltas; drop them explicitly (epaint asserts otherwise).
            out.textures_delta.clear();
        }

        assert!(
            canvas_clicked,
            "the graph body click did not reach the canvas widget"
        );
    }

    /// `@scroll` max for `.side` with a given requested offset.
    fn scroll_max_for(bin: &Path, dir: &Path, y: f64) -> f64 {
        let facts = minimal_facts(serde_json::json!({
            "collapsed": null, "active_mode": null, "pane_open": true,
            "scroll": [{ "selector": ".side", "x": 0.0, "y": y }]
        }));
        let facts_path = dir.join("facts-scroll.json");
        let tsv = dir.join("boxes-scroll.tsv");
        std::fs::write(&facts_path, facts.to_string()).expect("write facts");
        let out = Command::new(bin)
            .arg("--facts").arg(&facts_path)
            .arg("--boxes").arg(&tsv)
            .arg("--size").arg("1600x1000")
            .output()
            .expect("spawn nzk_ui --boxes");
        assert!(out.status.success(), "boxes failed: {}", String::from_utf8_lossy(&out.stderr));
        let text = std::fs::read_to_string(&tsv).expect("read boxes");
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("@scroll\t") {
                let parts: Vec<&str> = rest.split('\t').collect();
                if parts.first() == Some(&".side") {
                    return parts.get(4).and_then(|v| v.parse().ok()).unwrap_or(0.0);
                }
            }
        }
        0.0
    }

    fn current_scroll_max_for(bin: &Path, dir: &Path) -> f64 {
        let facts = minimal_facts(serde_json::json!({
            "collapsed": null, "active_mode": null, "pane_open": false, "scroll": []
        }));
        let facts_path = dir.join("facts-scroll-closed.json");
        let tsv = dir.join("boxes-scroll-closed.tsv");
        std::fs::write(&facts_path, facts.to_string()).expect("write facts");
        let out = Command::new(bin)
            .arg("--facts").arg(&facts_path)
            .arg("--boxes").arg(&tsv)
            .arg("--size").arg("1600x1000")
            .output()
            .expect("spawn nzk_ui");
        assert!(out.status.success(), "boxes failed: {}", String::from_utf8_lossy(&out.stderr));
        let text = std::fs::read_to_string(&tsv).expect("read boxes");
        text.lines()
            .find_map(|l| l.strip_prefix("@scroll\t").and_then(|r| r.split('\t').nth(4)))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0.0)
    }
    /// THE invariant this whole task exists to establish, measured on the real
    /// renderer rather than asserted from a hand-written rectangle:
    ///
    /// **A probe row existing does NOT mean a control is reachable.**
    ///
    /// Every overlay panel ships with `class="... hidden"` and `style.css:63`
    /// is `.hidden { display: none !important; }`. A `display:none` subtree gets
    /// no box, so its `--boxes` row is `0 0 0 0` — and `hit_in` requires
    /// `r.contains(point)`, which a zero-area rect can never satisfy. Being in
    /// `PROBE_SELECTORS` therefore buys a ROW and nothing else.
    ///
    /// This test was written because the task's original premise was the
    /// opposite ("add the selector and it becomes reachable"), and the premise
    /// was wrong. It is pinned here so nobody — including whoever wrote that
    /// premise — can re-make the mistake silently.
    #[test]
    fn a_probe_row_alone_does_not_make_a_control_reachable() {
        let Some(bin) = shell_bin() else {
            eprintln!("nzk_ui: skip (nzk_ui not built)");
            return;
        };
        let dir = work_dir("drawer-probe");

        // The three overlay drawers, each with the control that closes it.
        let drawers: &[(&str, &str)] = &[
            ("#nodeDrawer", "#nodeDrawerClose"),
            ("#findPanel", "#findClose"),
            ("#blocksPanel", "#blocksClose"),
        ];

        // The raw `--boxes` TSV, so ZERO-AREA rows stay visible. The parsed box
        // map cannot show them: `parse_boxes` drops every row with `w == 0 ||
        // h == 0` (the `if w > 0.0 && h > 0.0` guard below), which is exactly
        // why a hidden control is invisible to `hit_in`, and why "it has a probe
        // row" is not evidence of reachability.
        fn raw_tsv(bin: &Path, facts: &serde_json::Value, dir: &Path, tag: &str) -> String {
            let facts_path = dir.join(format!("facts-raw-{tag}.json"));
            let tsv = dir.join(format!("raw-{tag}.tsv"));
            std::fs::write(&facts_path, facts.to_string()).expect("write facts");
            let out = Command::new(bin)
                .arg("--facts").arg(&facts_path)
                .arg("--boxes").arg(&tsv)
                .arg("--size").arg("1600x1000")
                .output()
                .expect("spawn nzk_ui");
            assert!(out.status.success(), "nzk_ui failed: {}", String::from_utf8_lossy(&out.stderr));
            std::fs::read_to_string(&tsv).expect("read raw tsv")
        }

        // The `x y w h` of one selector's row in a raw TSV.
        fn row_of(tsv: &str, sel: &str) -> Option<(f32, f32, f32, f32)> {
            tsv.lines().find_map(|line| {
                let f: Vec<&str> = line.split('\t').collect();
                if f.len() < 5 || f[0] != sel {
                    return None;
                }
                let n = |s: &str| s.trim().parse::<f32>().ok();
                Some((n(f[1])?, n(f[2])?, n(f[3])?, n(f[4])?))
            })
        }

        // 1. CLOSED (the shipped document). The probe rows EXIST in the TSV...
        let closed_facts = minimal_facts(ui_state_json(&NzkUiState::default(), None));
        let closed_tsv = raw_tsv(&bin, &closed_facts, &dir, "closed");
        let closed = boxes_for(&bin, &closed_facts, &dir);
        for (panel, close) in drawers {
            for sel in [*panel, *close] {
                let (_, _, w, h) = row_of(&closed_tsv, sel).unwrap_or_else(|| {
                    panic!("{sel} must have a probe row — it is in PROBE_SELECTORS")
                });
                assert!(
                    w == 0.0 || h == 0.0,
                    "{sel} is hidden (`.hidden`) and must have a zero-area row, got {w}x{h}"
                );
            }
            // ...and the parsed box map does not contain them at all, so no press
            // can ever resolve to a hidden drawer.
            assert!(
                !closed.contains_key(*panel),
                "{panel} is hidden; it must not be reachable through the box map"
            );
        }

        // 2. OPENED. The same selectors now have REAL geometry and ARE reachable.
        for (panel, close) in drawers {
            let mut state = NzkUiState::default();
            let id = panel.trim_start_matches('#');
            let effect = crate::actions::apply(&mut state, NzkUiAction::ToggleDrawer(id), &repo());
            assert!(effect.repaint_nzk_ui);
            assert_eq!(state.drawer_open, Some(id));

            let facts = minimal_facts(ui_state_json(&state, None));
            let open_tsv = raw_tsv(&bin, &facts, &dir, id);
            let open = boxes_for(&bin, &facts, &dir);

            let (_, _, w, h) = row_of(&open_tsv, panel).expect("panel row");
            assert!(
                w > 0.0 && h > 0.0,
                "{panel} is open and must have a real rect, got {w}x{h}"
            );

            // The opened panel is now IN the parsed box map and its centre
            // resolves to a live control: this is the reachability that a probe
            // row alone could never provide.
            let r = *open.get(*panel).expect("open panel must reach the box map");
            assert!(
                hit_in(&open, r.center().x, r.center().y).is_some(),
                "a press inside the open {panel} must reach a control"
            );

            // ...and the close button inside it is reachable too.
            let cr = *open
                .get(*close)
                .unwrap_or_else(|| panic!("{close} must be laid out inside the open {panel}"));
            assert_eq!(
                hit_in(&open, cr.center().x, cr.center().y),
                Some(*close),
                "the centre of {close} must resolve to it"
            );
        }

        // 3. The modal family. It used to be the counter-example that proved the
        //    rule — "in PROBE_SELECTORS, has rows, stays zero-area because
        //    `drawers_open` cannot name it, and there is no shipping path that
        //    opens a modal". The last clause is no longer true: modals now have
        //    their own open channel (`UiState::modals_open`), so the honest
        //    assertion is a PAIR — hidden is still unreachable, and open is both
        //    laid out and reachable through a child.
        //
        //    The backdrop itself is deliberately NOT asserted reachable as a
        //    control: it covers the whole viewport when open, which is the
        //    correct "a modal is modal" behaviour and exactly why it stays
        //    `Inert` in CONTROLS (see the note there). The children are the
        //    controls.
        for sel in ["#reorgModal", "#editModal", "#previewModal", "#shortcutsModal"] {
            let (_, _, w, h) = row_of(&closed_tsv, sel).unwrap_or_else(|| panic!("{sel} row"));
            assert!(
                w == 0.0 || h == 0.0,
                "{sel} is hidden (`.hidden`), so it must keep a zero-area row, got {w}x{h}"
            );
            assert!(
                CONTROLS.iter().any(|c| c.selector == sel && c.owner == ControlOwner::Inert),
                "{sel} is a backdrop covering the viewport; it must stay Inert, \
                 not be registered as a control a press can land on"
            );
            assert!(
                !closed.contains_key(sel),
                "{sel} is hidden; it must not be reachable through the box map"
            );
        }

        // OPENED: each modal gets a real rect, and its own close control is
        // reachable. `(button, modal, child)` — the button that opens it, the
        // backdrop that must gain geometry, and a real control inside it.
        let modals: &[(&str, &str, &str)] = &[
            ("#shortcutsBtn", "#shortcutsModal", "#shortcutsDone"),
            ("#editBtn", "#editModal", "#editClose"),
            ("#previewBtn", "#previewModal", "#previewClose"),
            ("#exportBtn", "#reorgModal", "#reorgClose"),
        ];
        for (btn, modal, child) in modals {
            let mut state = NzkUiState::default();
            let effect = crate::actions::apply(&mut state, NzkUiAction::from_selector(btn), &repo());
            assert!(
                effect.repaint_nzk_ui,
                "{btn} must repaint the nzk_ui so the modal is actually laid out"
            );
            assert_eq!(
                state.modal_facts().len(),
                1,
                "exactly one modal may be open at a time: two viewport-covering \
                 backdrops would both be up"
            );

            let facts = minimal_facts(ui_state_json(&state, None));
            let open_tsv = raw_tsv(&bin, &facts, &dir, "modal-open");
            let open = boxes_for(&bin, &facts, &dir);

            let (_, _, w, h) = row_of(&open_tsv, modal).expect("modal row");
            assert!(
                w > 0.0 && h > 0.0,
                "{modal} is open after {btn} and must have a real rect, got {w}x{h}"
            );

            // The child is laid out AND a press at its centre resolves to it:
            // this is the reachability a probe row alone can never provide.
            let cr = *open
                .get(*child)
                .unwrap_or_else(|| panic!("{child} must be laid out inside the open {modal}"));
            assert!(
                cr.width() > 0.0 && cr.height() > 0.0,
                "{child} inside the open {modal} must have a real rect"
            );
            assert_eq!(
                hit_in(&open, cr.center().x, cr.center().y),
                Some(*child),
                "the centre of {child} must resolve to it"
            );
        }
    }

    /// **The closed picker must not move a single pixel.**
    ///
    /// The task's acceptance criterion names one token —
    /// `@frame 1600 1000 4bd131fa71b2ccc8` — and this pins it against the REAL
    /// renderer at the REAL size, so a change to what the picker publishes while
    /// shut can never ship quietly. `ui.picker` is the only field this task
    /// added to the facts, and `ui_state_json` OMITS it (rather than sending
    /// `null`) when no picker exists, which is what makes "closed" byte-identical
    /// to the pre-fix document instead of merely similar.
    #[test]
    fn a_closed_picker_keeps_the_default_frame_token() {
        let Some(bin) = shell_bin() else {
            eprintln!("nzk_ui: skip (nzk_ui not built)");
            return;
        };
        // The documented reproduction (`notes/d6-verification.md:261-265`): the
        // DEFAULT facts, no `--facts` document at all, which is the state the
        // token was measured in. The banner line must be consumed before the
        // request is sent — `serve.rs:82` writes it first and the host reads it
        // once, so a request written without reading it is still answered, but
        // reading it here keeps this the same exchange the app performs.
        use std::io::{BufRead, BufReader, Write};
        let mut child = Command::new(&bin)
            .arg("--serve")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn nzk_ui --serve");
        let mut out = BufReader::new(child.stdout.take().expect("stdout"));
        let mut banner = String::new();
        out.read_line(&mut banner).expect("read banner");
        assert!(
            banner.starts_with("nzk_ui") && banner.contains("protocol 1"),
            "unexpected serve banner: {banner:?}"
        );
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(b"REQ 1600x1000 0 1 /tmp/fid-corpus\n\n")
            .expect("write request");
        // The response header, then the TSV the token lives in.
        let mut header = String::new();
        out.read_line(&mut header).expect("read response header");
        assert!(header.starts_with("OK "), "serve replied: {header:?}");
        // `OK <w> <h> <png_len> <tsv_len>` — four numeric fields after the verb.
        let lens: Vec<usize> =
            header.split_whitespace().skip(1).filter_map(|t| t.parse().ok()).collect();
        assert_eq!(lens.len(), 4, "OK <w> <h> <png_len> <tsv_len>: {header:?}");
        assert_eq!((lens[0], lens[1]), (1600, 1000), "wrong frame size: {header:?}");
        let (png_len, tsv_len) = (lens[2], lens[3]);
        let mut payload = vec![0u8; png_len + tsv_len];
        std::io::Read::read_exact(&mut out, &mut payload).expect("read payload");
        let tsv = String::from_utf8_lossy(&payload[png_len..]).into_owned();
        let _ = child.kill();
        // RE-PINNED (round-27, glyph-raster parity): the tracked vendor patch
        // `tools/patches/blitz-paint-unhinted.patch` (blitz-paint/src/text.rs
        // `hint(true)` -> `hint(false)`) makes the nzk_ui glyph raster match
        // Chromium's unhinted / fractional-baseline rasterisation. That
        // intentionally moved this token from af24db28dd6cb0c7 to
        // 85de1ed2614c7085 and improved the real-corpus gate 1.3262 -> 1.2122
        // (sidebar 4.1451 -> 3.7077, topbar 5.1309 -> 4.9883, canvas 0.2385 ->
        // 0.2205). Re-measured from a REAL `nzk_ui --serve` render, twice,
        // byte-identical: evidence/v3launch/verify-r27/.
        //
        // RE-PINNED (round-29, two reviewed nzk_ui-pixel fixes):
        //  (a) STATUS-LINE VERTICAL PIN — the injected shim
        //      `nzk_ui/assets/css/compat.css` §14 gives `#statusLine`
        //      `padding-left: 8px; padding-top: 2px`, because Blitz leaves the
        //      11px status line 1px high in the 32px topbar where Chromium
        //      centres it (topbar 4.9472 -> 4.8172).
        //  (b) V2 CHECKBOX SKIN — `nzk_ui/src/dom.rs` `apply_form` paints the
        //      sidebar checkbox shim with the sampled v2 reference values
        //      (`#007acc` checked, `#858585` border, `#3b3b3b` unchecked) and
        //      rounds the fractional layout box to whole pixels (sidebar
        //      3.6426 -> 3.6219).
        // Together they intentionally moved this token from 85de1ed2614c7085 to
        // 4f0006ac48a5815c and improved the real-corpus gate 1.1962 -> 1.1851 (topbar
        // 4.8172, canvas 0.2205, sidebar 3.6219). Re-measured from a REAL
        // `nzk_ui --serve` render, twice, byte-identical:
        // evidence/v3launch/r29-verify/.
        //
        // RE-PINNED (round-30, `.check` line-box pin): `compat.css` §15 pins
        // `.check { line-height: 13px }`; Blitz's normal line box for the 11px
        // mono checkbox row is 15px where Chromium computes 13px. That moved
        // this token once more to 4f0006ac48a5815c and improved the real-corpus
        // gate 1.1851 -> 1.1829 (sidebar 3.6219 -> 3.6120; topbar/canvas
        // unchanged). Re-measured from a REAL `nzk_ui --serve` render:
        // evidence/v3launch/final-r30/.
        //
        // RE-PINNED (round-31, sidebar line-box fix): `compat.css` §16 pulls the
        // Edge-kinds group up 3px (`#edgeFilters{margin-top:-3px}`) and returns
        // the same 3px at the Reset/Remove row (`#resetFilters,#pruneHidden
        // {margin-top:3px}`) so the Layout sliders stay bit-identical. That
        // moved this token to 4f0006ac48a5815c and improved the real-corpus gate
        // 1.1829 -> 1.0834 (sidebar 3.6120 -> 3.1454; topbar/canvas unchanged).
        // Re-measured from a REAL `nzk_ui --serve` render:
        // evidence/v3launch/r31-lead/.

        // RE-PINNED (round-31, stroked shapes — same round): `nzk_ui/src/dom.rs`
        // `apply_select_chevrons` now paints the reference's *stroked-V* select
        // chevron as 22 sampled 1-px runs instead of a filled border triangle
        // (crop mae 13.64 -> 1.97), and `apply_form`'s checkbox tick is a sampled
        // stroked check instead of a Unicode glyph (crop mae 12.27 -> 10.25). That
        // moved this token once more to 4f0006ac48a5815c and improved the
        // real-corpus gate 1.0834 -> 1.0806 (sidebar 3.1454 -> 3.1322;
        // topbar/canvas unchanged). Re-measured from a REAL `nzk_ui --serve`
        // render: evidence/v3launch/r31-shapes/.

        // RE-PINNED (round-32, input-font engine gap): Blitz's text-input
        // editor copied only FontSize/LineHeight/Brush from the computed style
        // into Parley's editor, so `.text-input { font-family: var(--mono) }`
        // never reached the value's shaping: the topbar repo-path chip shaped
        // in the default proportional sans and fit ~3 more characters than the
        // 12px Liberation Mono reference. The tracked vendor patch
        // tools/patches/blitz-dom-input-font.patch propagates the full computed
        // font style; compat.css §17 then pulls #rootInput's start padding 9->8
        // to put the first glyph on the reference pixel. That moved this token
        // once more to 4f0006ac48a5815c and improved the real-corpus gate
        // 1.0806 -> 1.0195 (topbar 4.4178 -> 3.6361; sidebar 3.1322 -> 3.1338,
        // canvas unchanged). Re-measured from a REAL `nzk_ui --serve`
        // render: evidence/v3launch/r32-font/.

        // RE-PINNED (round-33, three CSS-only Blitz-delta shims):
        //  (a) §18 `#nsSelect` inner text inset+centring (select was 4px left /
        //      5px high: Blitz has no native select appearance), (b) §19
        //      `.check` line box 13->14px, (c) §20 three `.canvas-tools` button
        //      advance widths (Up 47/Fit 40/Re-settle 81; v2's text advance is
        //      1px narrower on those labels and the flex `gap:5px` carried the
        //      drift across the row). That moved this token once more to
        //      4f0006ac48a5815c and improved the real-corpus gate 1.0195 ->
        //      0.9768 (sidebar 3.1338 -> 2.9692, canvas 0.2205 -> 0.2101,
        //      topbar unchanged). Re-measured from a REAL `nzk_ui --serve`
        //      render: evidence/v3launch/r33-ship/.
        // RE-PINNED (round-34, two Blitz-delta fixes):
        //  (a) CSS §21 `.edgekey` border-box row model: `.edgekey` inherited
        //      §10's fractional 12.51px line-height, making its border-box 17px
        //      where Chromium computes 16px; the bottom border AA-split and the
        //      two following cards / group chips sat 1px off. §21 pins
        //      height:16px with a 0.25/0.75px padding split so the glyph baseline
        //      stays, and pays the space back with
        //      `#kindChildWrap { margin-top:2.6875px }`.
        //  (b) dom.rs paint-only checkbox pin: Blitz lays the UA checkbox out at a
        //      fractional 13.5x14.5, so rounding painted it 14x15 where Chromium
        //      paints 13x13; `apply_paint_shims` now pins the painted box to
        //      13x13 at the rounded origin (layout untouched — CSS size pins
        //      would reflow `.check` rows).
        // That moved this token once more to 33862b8aba15aa90 and improved the
        // real-corpus gate 0.9768 -> 0.9175 (sidebar 2.9692 -> 2.6912, topbar and
        // canvas unchanged). Re-measured from a REAL `nzk_ui --serve` render:
        // evidence/v3launch/r34-check/ and evidence/v3launch/r34-sidebarA/.
        //
        // RE-PINNED (round-35, three Blitz-delta fixes):
        //  (a) CSS §14 `#statusLine` horizontal register: Blitz ignores
        //      horizontal padding on that flex item, so the status run sat 2px
        //      left of the reference; margin-left:2px with margin-right:-2px
        //      moves the status text onto the reference while leaving the
        //      trailing Export/Edit/Blocks/Preview row fixed.
        //  (b) CSS §22 `#depthSelect` horizontal register: Blitz's mono advance
        //      for the neighbouring labels is ~2px wider, pushing the box right
        //      (ref border columns x1409/x1486 vs fork x1411/x1489); a -2px
        //      margin-left restores the reference columns.
        //  (c) dom.rs `apply_paint_shims` depthSelect spinner shim: Blitz paints
        //      neither the UA spin buttons nor the input's value text. The shim
        //      now draws the reference's darker #2b2b2c strip, 1px-row 9x5 arrows
        //      (rows 572-576 / 583-587, centre x1476) and the right-aligned
        //      value ("1"), leaving the input's right border visible.
        // That moved this token to 58009007b0f07c08 and improved the real-corpus
        // gate 0.9175 -> 0.8660 (topbar 3.6361 -> 2.7809, sidebar 2.6912 ->
        // 2.6586, canvas 0.2101 unchanged). Two-run render byte-identical
        // (md5 c2b7ff4bfd1d78f5abf6110bf806d6bf); evidence:
        // evidence/v3launch/final-r35/ (gate), r35-topbar/, r35-spinner/.
        // RE-PINNED (round-36, two CSS-only Blitz-delta fixes):
        //  (a) CSS §23 `#resetFilters,#pruneHidden{padding-top:3px}`: Blitz
        //      centres the button label with its own normal line-box metrics,
        //      which sat the ink 1.5-2px above the reference (label rows 636-644
        //      vs 638-645). Shrinking the 28px content box at the top slides the
        //      centred label onto the reference rows (band y618-660 MAE
        //      4.323 -> 1.644; the remaining tail is glyph-rasteriser AA).
        //  (b) CSS §21 `.edgekey` split re-measured: 1.25/0.75 -> 1.375/0.625
        //      lands the Edge-kinds text rows y440-445 2-3 MAE lower at a fixed
        //      16px border-box (441: 19.7 -> 17.0, 443: 21.7 -> 18.9).
        // That moved this token to 4bd131fa71b2ccc8 and improved the real-corpus
        // gate 0.8660 -> 0.8414 (sidebar 2.6586 -> 2.5433, topbar 2.7809 and
        // canvas 0.2101 unchanged). Evidence: evidence/v3launch/final-r36/.
        // RE-PINNED again (round-20 / task-17 addendum): the nzk_ui geometry
        // fix (paint-fixer; reviewed, A/B mae 2.522 -> 1.9239) intentionally
        // moved this token from 845180d07a64ce3d to af24db28dd6cb0c7. This pin
        // was re-measured from a REAL `nzk_ui --serve` render in this tree;
        // the 7-gate harness pin in tools/ is owned by ab-harness, not here.
        //
        // Original task-10 re-pin kept below for history: pinned from a REAL
        // `nzk_ui --serve`
        // render, to the reference-parity frame that includes the Layout panel's
        // **Text size** row (`index.html:306-309`). That control is present in
        // the reference frontend and was the one slider native was missing, so
        // adding it moves native *toward* the reference — the digest moved
        // because one more control is painted, not because the default render
        // regressed. The slider's DEFAULT is `TEXT_SIZE_DEFAULT == CHIP_FONT`
        // (11), so the chip geometry itself is bit-for-bit unchanged; that is
        // what the "default is a no-op" test below pins.
        //
        // This is a reviewed change of the pinned digest, NOT a weakening: the
        // assertion still demands an exact token from a real render.
        //
        // RE-PINNED (r42, uniform text-box padding): the placeholder/spinner
        // shims now paint at the input's real content-box origin and computed
        // font size instead of a hard-coded 9px/12px, and `.mini-input` /
        // `.depth-input` / `.cs-qo-input` were normalised to the same 9px inset.
        // The default fixture document paints `#depthSelect`, `#pubSearch` and
        // `#strSearch`, so those intentional pixel changes moved the token from
        // 4bd131fa71b2ccc8 to 625a8f4079e9f1eb.
        //
        // RE-PINNED again (r43, VT mode): `#hideEdgeLabels`' LABEL changed from
        // "Hide edge word boxes" to "VT mode — direct-line relations" (same id,
        // same wire key). The label is painted text in the default frame, so the
        // token moved 625a8f4079e9f1eb -> 183b55165755a539. Re-measured from a
        // real `nzk_ui --serve` render with no `--facts` document (exactly the
        // reproduction above).
        // RE-PINNED (CsViewerGitRepo layout): the nzk_ui child is now built
        // in-tree by the toolkit builder, so this test exercises the real
        // render instead of skipping. Re-measured from that binary.
        assert!(
            tsv.contains("@frame\t1600\t1000\t72658620ff120aeb"),
            "the closed-picker frame token moved; the picker must publish NOTHING \
             while shut. Got:\n{tsv}"
        );

        // And the same through the real facts path the app uses, with a
        // `NzkUiState::default()` (no picker): `ui.picker` must be absent, and
        // the boxes must be the shipped ones.
        let dir = work_dir("picker-closed");
        let state = NzkUiState::default();
        let facts = minimal_facts(ui_state_json(&state, None));
        assert!(
            facts["ui"].get("picker").is_none(),
            "a closed picker must OMIT ui.picker (not send null) or the shipped \
             document changes"
        );        let boxes = boxes_for(&bin, &facts, &dir);
        // `#modal` ships `hidden`, so with the picker shut every registered
        // picker control must be UNREACHABLE — either it has a zero-area probe
        // row, or it is not in the box map at all. What must never happen is a
        // positive-area row a press could land on.
        for sel in
            ["#modal", "#modalSelect", "#modalGo", "#modalUp", "#modalHome", "#modalRoot", "#modalRepo"]
        {
            if let Some(r) = boxes.get(sel) {
                assert!(
                    r.width() == 0.0 || r.height() == 0.0,
                    "{sel} is inside the SHUT #modal and must stay zero-area, got {}x{}",
                    r.width(),
                    r.height()
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The OPEN picker lays out real rows and they are reachable.**
    ///
    /// The consumed half, measured on the real renderer: `#browseBtn` opens
    /// `#modal`, the listing in `ui.picker` becomes `.pick` buttons inside
    /// `#modalBody`, and a press at a row's centre resolves to that row. Before
    /// this task `ui.picker` did not exist, so `#modalBody` stayed as shipped —
    /// this test would have failed on the empty body rather than on the modal's
    /// geometry.
    #[test]
    fn an_open_picker_lays_out_rows_inside_modal_body() {
        let Some(bin) = shell_bin() else {
            eprintln!("nzk_ui: skip (nzk_ui not built)");
            return;
        };
        let dir = work_dir("picker-open");

        // A real directory with real sub-folders to list.
        let scratch = dir.join("corpus");
        std::fs::create_dir_all(scratch.join("alpha")).expect("alpha");
        std::fs::create_dir_all(scratch.join("beta")).expect("beta");

        let mut state = NzkUiState::default();
        let effect =
            crate::actions::apply(&mut state, NzkUiAction::Browse, &scratch);
        assert!(effect.repaint_nzk_ui, "#browseBtn must repaint the nzk_ui");
        assert_eq!(state.modal_facts(), vec!["modal"], "#modal must be open");

        let facts = minimal_facts(ui_state_json(&state, None));
        let picker = facts["ui"].get("picker").expect("ui.picker must be published");
        assert_eq!(
            picker["dirs"].as_array().map(Vec::len),
            Some(2),
            "both real sub-directories must reach the wire: {picker}"
        );

        // The modal and its body gain real boxes...
        let open = boxes_for(&bin, &facts, &dir);
        for sel in ["#modal", "#modalBody"] {
            let r = *open
                .get(sel)
                .unwrap_or_else(|| panic!("{sel} must be laid out while the picker is open"));
            assert!(
                r.width() > 0.0 && r.height() > 0.0,
                "{sel} is open and must have a real rect, got {}x{}",
                r.width(),
                r.height()
            );
        }
        // ...and `#modalSelect` is reachable, which is the control the whole
        // feature exists to make pressable.
        let sel_rect = *open.get("#modalSelect").expect("#modalSelect must be laid out");
        assert_eq!(
            hit_in(&open, sel_rect.center().x, sel_rect.center().y),
            Some("#modalSelect"),
            "the centre of #modalSelect must resolve to it"
        );
        // `#modalGo`/`#modalUp`/`#modalHome`/`#modalRoot`/`#modalRepo` are the
        // other registered picker controls; each must be laid out and hittable.
        for sel in ["#modalGo", "#modalUp", "#modalHome", "#modalRoot", "#modalRepo"] {
            let r = *open
                .get(sel)
                .unwrap_or_else(|| panic!("{sel} must be laid out while the picker is open"));
            assert_eq!(
                hit_in(&open, r.center().x, r.center().y),
                Some(sel),
                "the centre of {sel} must resolve to it"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The picker's own controls are declared in the registry with their OWN
    /// owner, and each is a real `PickerAction` — never silently Inert.
    #[test]
    fn every_picker_control_is_registered_and_resolvable() {
        let pickers = [
            ("#modalClose", "picker-close"),
            ("#modalCancel", "picker-close"),
            ("#modalUp", "picker-up"),
            ("#modalHome", "picker-home"),
            ("#modalRoot", "picker-root"),
            ("#modalRepo", "picker-repo"),
            ("#modalGo", "picker-go"),
            ("#modalSelect", "picker-select"),
        ];
        for (sel, label) in pickers {
            let c = CONTROLS
                .iter()
                .find(|c| c.selector == sel)
                .unwrap_or_else(|| panic!("{sel} must be registered in CONTROLS"));
            assert_eq!(c.owner, ControlOwner::Picker, "{sel} must be picker-owned");
            // `#modalGo` carries the input text, so it has no from_selector arm;
            // every other picker control must resolve from its id alone.
            if sel == "#modalGo" {
                assert_eq!(PickerAction::from_selector(sel), None);
            } else {
                let got = PickerAction::from_selector(sel)
                    .unwrap_or_else(|| panic!("{sel} must resolve to a PickerAction"));
                assert_eq!(got.label(), label, "{sel} resolved to the wrong message");
            }
        }
        // The backdrop is not a control: a press that misses the card lands on
        // it, and treating that as a toggle would close the dialog the user was
        // aiming into (shell.js:761 handles it host-side instead).
        assert!(
            CONTROLS.iter().any(|c| c.selector == "#modal" && c.owner == ControlOwner::Inert),
            "#modal is a viewport-covering backdrop; it must stay Inert"
        );
        assert_eq!(PickerAction::from_selector("#modal"), None);
    }

    // -----------------------------------------------------------------------
    // task-16: the PUBLISHER is on the production path
    // -----------------------------------------------------------------------

    /// THE DECISIVE TEST for task-16.
    ///
    /// The defect was that `facts_json_with_settings` existed, was correct, and
    /// was reachable only from tests — so earlier tests passed while the live
    /// app still rendered shipped literals. A test that calls the builder
    /// directly therefore proves nothing about the wiring, which is exactly how
    /// the bug survived.
    ///
    /// This drives the REAL `nzk_ui` binary and asserts the RESOLVED SPAN
    /// TEXT (`--dump-tree`) — what the reference's `syncLayoutControls` writes
    /// (`shell.js:807-816`) — for both the default and a non-default
    /// `LayoutSettings`.
    ///
    /// Crucially the facts come from [`frame_facts`], the SAME function
    /// `paint_in` calls, not from a builder this test chose for itself. An
    /// earlier draft of this test called `facts_json_with_settings` directly and
    /// **stayed green when `paint_in` was mutated back to the plain builder** —
    /// i.e. it reproduced the very defect it was written to catch. Calling
    /// `frame_facts` fixes that: the mutation is applied to the function under
    /// test, so it now goes red.
    #[test]
    fn the_real_render_path_resolves_the_layout_spans_from_live_settings() {
        let Some(bin) = shell_bin() else {
            eprintln!("nzk_ui: skip (nzk_ui not built)");
            return;
        };
        if !supports(&bin, "--dump-tree") {
            eprintln!("nzk_ui: skip (nzk_ui has no --dump-tree)");
            return;
        }
        let dir = work_dir("slider-spans");
        let graph = Graph {
            repo: PathBuf::from("/tmp/fid-slider-spans"),
            nodes: vec![],
            edges: vec![],
            state: serde_json::json!({}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let state = NzkUiState::default();
        let spans = ["spacingOut", "iterOut", "curveOut", "nodeSizeOut", "textSizeOut"];

        // The text each span resolves to, through the real renderer.
        let resolve = |settings: crate::graphviz::LayoutSettings| -> Vec<String> {
            // THE PRODUCTION STEP `paint_in` performs. Not a builder this test
            // picked: `frame_facts` is what the frame path calls.
            let facts = frame_facts(&graph, &state, settings);
            let facts_path = dir.join(format!(
                "facts-sp{}-tx{}.json",
                settings.spacing as i64, settings.text_size as i64
            ));
            std::fs::write(&facts_path, facts.to_string()).expect("write facts");
            let out = Command::new(&bin)
                .arg("--facts")
                .arg(&facts_path)
                .arg("--size")
                .arg("1600x1000")
                .arg("--dump-tree")
                .output()
                .expect("spawn nzk_ui --dump-tree");
            assert!(
                out.status.success(),
                "nzk_ui --dump-tree failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let tree = String::from_utf8_lossy(&out.stdout).into_owned();
            spans
                .iter()
                .map(|id| {
                    let needle = format!("id=\"{id}\"");
                    let at = tree
                        .find(&needle)
                        .unwrap_or_else(|| panic!("{id} is missing from the resolved tree"));
                    tree[at..]
                        .lines()
                        .find_map(|l| {
                            let t = l.trim().strip_prefix("#text ")?;
                            // `--dump-tree` prints `#text <node-index>: <value>`;
                            // the resolved TEXT is everything after the colon.
                            Some(match t.split_once(": ") {
                                Some((_, v)) => v.to_string(),
                                None => t.to_string(),
                            })
                        })
                        .unwrap_or_else(|| panic!("{id} has no resolved text"))
                })
                .collect()
        };

        // 1. DEFAULTS are inert: the shipped `index.html` literals, unchanged.
        assert_eq!(
            resolve(crate::graphviz::LayoutSettings::default()),
            vec!["120", "800", "0.10", "8", "11"],
            "at defaults the spans must keep the shipped literals; if these moved, \
             the default path is not inert"
        );

        // 2. NON-DEFAULT settings reach the resolved spans. This is the point:
        //    the values come back out of the REAL renderer.
        let moved = crate::graphviz::LayoutSettings {
            spacing: 200.0,
            iterations: 1200,
            curvature: 0.05,
            node_size: 14.0,
            text_size: 18.0,
        };
        assert_eq!(
            resolve(moved),
            vec!["200", "1200", "0.05", "14", "18"],
            "non-default LayoutSettings must reach the resolved readout spans"
        );
    }

    /// The call site itself is load-bearing: `paint_in` must evaluate the
    /// settings-AWARE builder. Reverting that one line silently deadens the
    /// readouts again, and only the non-default half of the test above notices.
    #[test]
    fn the_plain_builder_never_publishes_settings() {
        let graph = Graph {
            repo: PathBuf::from("/tmp/fid-publisher"),
            nodes: vec![],
            edges: vec![],
            state: serde_json::json!({}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let state = NzkUiState::default();
        // The plain builder omits the key, which is why `paint_in` must call the
        // `_with_settings` form rather than the plain one.
        assert!(
            facts_json_with(&graph, &state).get("settings").is_none(),
            "the plain builder must never publish `settings`"
        );
        // ...and the aware form does, for the same inputs.
        let moved = crate::graphviz::LayoutSettings {
            spacing: 77.0,
            text_size: 22.0,
            ..Default::default()
        };
        let facts = facts_json_with_settings(&graph, &state, moved);
        assert_eq!(facts["settings"]["spacing"], "77");
        assert_eq!(facts["settings"]["textSize"], "22");
    }
}

/// D2 core: hit-test consumes the layout that painted, and drift FAILS.
///
/// These are the tests the owner's ruling asks for. They do not spawn
/// nzk_ui: the drift guard is a pure function of (token, painted bytes), and
/// the geometry resolution is a pure function of (boxes, DOM chain, point), so
/// both are exercised exactly and fast. The CLI-level cross-check against a real
/// render lives in `notes/d2/centre_hit.sh`, which is evidence rather than a unit
/// test (it costs ~100ms per `--hit`).
#[cfg(test)]
mod interaction_tests {
    use super::*;
    use crate::graphviz::{
        CURVE_MAX, CURVE_MIN, ITER_MAX, ITER_MIN, NODE_SIZE_MAX, NODE_SIZE_MIN, SPACING_MAX,
        SPACING_MIN, TEXT_SIZE_MAX, TEXT_SIZE_MIN,
    };

    /// The corpus root for `actions::apply` — the crate's own directory.
    ///
    /// Real, always present, and the folder `#browseBtn` is actually pointed at
    /// in a run from this checkout. `pub(super)` so the sibling `dispatch_tests`
    /// module shares the one definition rather than growing a second copy of the
    /// same fact.
    pub(super) fn repo() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// A geometry whose boxes are these two rectangles, with no token.
    pub(super) fn geo(pairs: &[(&str, f32, f32, f32, f32)]) -> FrameGeometry {
        let mut boxes = BTreeMap::new();
        for &(sel, x, y, w, h) in pairs {
            boxes.insert(sel.to_string(), Rect::from_min_size(Pos2::new(x, y), egui::vec2(w, h)));
        }
        FrameGeometry::unverified(boxes, (1600, 1000))
    }

    /// An empty view. The click path only ever touches `fit_pending` here, so an
    /// empty corpus is enough and keeps the test display-free. `pub(super)` so the
    /// sibling `dispatch_tests` module can reuse it.
    pub(super) fn view() -> GraphView {
        GraphView::new(Graph {
            repo: PathBuf::from("/tmp/does-not-exist"),
            nodes: Vec::new(),
            edges: Vec::new(),
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        })
    }

    /// THE drift test. A token that does not describe the painted bytes must FAIL
    /// the comparison — not warn, not pass. If this ever returns `Ok`, a stale
    /// `--boxes` will silently misroute clicks, which is the exact defect the
    /// ruling names.
    #[test]
    fn boxes_and_paint_drift_fails_rather_than_misroutes() {
        let painted = vec![0u8, 1, 2, 3, 4, 5, 6, 7];
        let good = FrameToken { size: (1, 2), digest: digest_rgba(&painted) };
        assert!(
            check_correspondence(&good, (1, 2), &painted).is_ok(),
            "a token computed from these bytes must match"
        );

        // Same size, DIFFERENT pixels: the geometry is from another frame.
        let mut other = painted.clone();
        other[0] = 0xff;
        let stale = FrameToken { size: (1, 2), digest: digest_rgba(&other) };
        let err = check_correspondence(&stale, (1, 2), &painted)
            .expect_err("a mismatched paint digest MUST fail");
        assert!(err.contains("different frames"), "unhelpful drift message: {err}");

        // A token for another SIZE cannot describe these pixels either.
        let wrong_size = FrameToken { size: (2, 2), digest: digest_rgba(&painted) };
        let err = check_correspondence(&wrong_size, (1, 2), &painted)
            .expect_err("a size disagreement MUST fail");
        assert!(err.contains("geometry is for 2x2"), "unhelpful size message: {err}");

        // And a truncated buffer must never be accepted as a match.
        assert!(check_correspondence(&good, (1, 2), &painted[..4]).is_err());
    }

    /// A drifted frame must refuse to resolve a click at all.
    ///
    /// This is the behavioural half of the drift guard: detecting the mismatch is
    /// useless if the click is then routed anyway.
    #[test]
    fn a_drifted_frame_resolves_no_control() {
        let mut g = geo(&[("#fitBtn", 100.0, 100.0, 40.0, 20.0)]);
        assert_eq!(resolve_selector(&g, &[], 110.0, 110.0), Some("#fitBtn"));

        g.drift = Some("--boxes and the painted PNG are different frames".to_string());
        assert_eq!(
            resolve_selector(&g, &[], 110.0, 110.0),
            None,
            "a proven-drift frame must not route a click"
        );
        assert!(!g.correspondence_proven());
    }

    /// The ancestor walk: a press on a control's CHILD element resolves to the
    /// control. This mirrors the real DOM — `#shortcutsBtn` contains an
    /// `<svg class="shortcuts-gear">` with no id, and the centre of that button
    /// hits the svg (measured: `hit 152 svg # .shortcuts-gear`).
    #[test]
    fn a_press_on_a_child_element_resolves_to_its_ancestor_control() {
        // Raw hit is the svg child: no id at all.
        let raw = HitNode::class("shortcuts-gear");
        assert_eq!(raw.id, None, "the gear svg carries no id — that is the point");
        // The chain continues to the button that owns it.
        let chain = vec![raw, HitNode::id("shortcutsBtn")];
        assert_eq!(resolve_hit_chain(&chain), Some("#shortcutsBtn"));

        // An <option> inside #nsSelect behaves the same way.
        let ns = vec![HitNode::default(), HitNode::id("nsSelect")];
        assert_eq!(resolve_hit_chain(&ns), Some("#nsSelect"));

        // An UNREGISTERED id in the chain is skipped, not treated as a match, and
        // the walk continues outward; the nearest REGISTERED ancestor wins rather
        // than the outermost.
        let nested = vec![
            HitNode::default(),
            HitNode::id("nodeProps"),  // drawn, not a registered control
            HitNode::id("findPanel"),  // registered -> this one wins
            HitNode::id("nodeDrawer"), // also registered, but further out
        ];
        assert_eq!(resolve_hit_chain(&nested), Some("#findPanel"));

        // A chain that never reaches a registered control resolves to nothing.
        let none = vec![HitNode::class("canvas-tools"), HitNode::default()];
        assert_eq!(resolve_hit_chain(&none), None);
    }

    /// **The measured precedence.** When only ONE of the two parties has an
    /// answer, the shell's own hit-test wins — because the shell is the party that
    /// laid out *and painted* the pixels, while `--boxes` is a flat map that
    /// cannot express occlusion or clipping.
    ///
    /// Justification is the measurement recorded on [`resolve_selector`]: over all
    /// 49 registered controls at real geometry the two agree on every point a
    /// pointer can actually be at (25 non-zero boxes; 0 disagreements, plus 0 over
    /// 9 interior samples each). So this precedence **changes no click that works
    /// today** — it is robustness, not a bug fix.
    ///
    /// The synthetic frame below is the case the flat map must lose: a registered
    /// control whose box is covered by another element painted over it.
    #[test]
    fn the_shell_hit_test_outranks_the_boxes_map_when_the_map_has_no_answer() {
        // `#zoomInBtn`'s box contains the point and the shell painted
        // `#blocksBtn` there — this is the AGREEMENT case, and the answer is the
        // same whichever party is asked.
        let g = geo(&[
            ("#zoomInBtn", 100.0, 60.0, 60.0, 30.0),
            ("#blocksBtn", 100.0, 60.0, 20.0, 30.0),
        ]);
        assert_eq!(hit_in(&g.boxes, 110.0, 75.0), Some("#blocksBtn"));
        let agree = vec![HitNode::shell("7", Some("blocksBtn"), None)];
        assert_eq!(resolve_selector(&g, &agree, 110.0, 75.0), Some("#blocksBtn"));
    }

    /// The shell's DOM hit-test accounts for stacking; the flat boxes map does
    /// not. When an overlay covers an underlying control, route to the shell's
    /// topmost target. Proven paint/geometry drift is still refused above.
    #[test]
    fn a_shell_and_boxes_overlap_routes_to_the_shell_topmost_target() {
        // The shell says `#zoomInBtn`; the flat map's smallest containing box is
        // `#blocksBtn`. Both are registered and both answered.
        let g = geo(&[
            ("#zoomInBtn", 100.0, 60.0, 60.0, 30.0),
            ("#blocksBtn", 100.0, 60.0, 20.0, 30.0),
        ]);
        let shell_says_zoom = vec![HitNode::shell("9", Some("zoomInBtn"), None)];
        assert_eq!(hit_in(&g.boxes, 110.0, 75.0), Some("#blocksBtn"));
        assert_eq!(resolve_hit_chain(&shell_says_zoom), Some("#zoomInBtn"));
        assert_eq!(
            resolve_selector(&g, &shell_says_zoom, 110.0, 75.0),
            Some("#zoomInBtn"),
            "the shell hit-test owns stacking and must win over the flat map"
        );
    }

    #[test]
    fn picker_backdrop_press_closes_instead_of_reaching_underlying_browse() {
        let g = geo(&[
            ("#browseBtn", 340.0, 10.0, 54.0, 30.0),
            ("#modal", 0.0, 0.0, 1440.0, 900.0),
        ]);
        let backdrop = vec![HitNode::shell("88", Some("modal"), None)];
        let action = picker_action(&g, &backdrop, 367.0, 25.0, None, &[]);
        assert_eq!(action, Some(PickerAction::Close));

        let inside_card = vec![HitNode::shell("89", Some("modalBody"), None)];
        assert_eq!(
            picker_action(&g, &inside_card, 367.0, 25.0, None, &[]),
            None,
            "a click inside the card must not be treated as a backdrop click"
        );
    }

    #[test]
    fn picker_row_press_uses_the_shell_data_nav_not_a_band_rect() {
        // The approximate band rect names "scenes"; the row the shell actually
        // hit is "nzk toolkit v6.1". The chain payload must win, or a long
        // listing navigates to a different folder than the one under the pointer.
        let g = geo(&[("#modal", 0.0, 0.0, 1440.0, 900.0)]);
        let rows = [(egui::Rect::from_min_max(
            egui::pos2(0.0, 100.0),
            egui::pos2(400.0, 140.0),
        ), "/home/naruzkurai/nem/Assets/scenes")];
        let chain = vec![
            HitNode::shell("91", None, Some("pname")),
            HitNode::shell_row_payload(
                "90",
                None,
                Some("pick"),
                None,
                None,
                None,
                Some("/home/naruzkurai/nem/Assets/nzk toolkit v6.1"),
            ),
        ];
        assert_eq!(
            picker_action(&g, &chain, 200.0, 120.0, None, &rows),
            Some(PickerAction::Open(
                "/home/naruzkurai/nem/Assets/nzk toolkit v6.1".to_string()
            )),
            "a press on a row's child must carry the row's own data-nav"
        );
    }

    #[test]
    fn the_modal_path_box_is_a_form_control() {
        // It is registered Host and in the form model; the mapping is what makes
        // a press focus it. Without this arm the click resolved to `None`, so no
        // character was ever delivered to the box.
        assert_eq!(
            NzkUiAction::from_selector("#modalPathInput"),
            NzkUiAction::FormOpen("#modalPathInput")
        );
    }

    #[test]
    fn native_modal_backdrops_route_to_their_close_toggle() {
        for (selector, surface) in [
            ("#shortcutsModal", crate::actions::HostSurface::Shortcuts),
            ("#editModal", crate::actions::HostSurface::Edit),
            ("#previewModal", crate::actions::HostSurface::Preview),
            ("#reorgModal", crate::actions::HostSurface::Export),
        ] {
            assert_eq!(modal_backdrop_action(selector), Some(NzkUiAction::ToggleModal(surface)));
        }
        assert_eq!(modal_backdrop_action("#modal"), None, "picker uses PickerAction::Close");

        let inner = vec![
            HitNode::shell("7", Some("shortcutsBody"), None),
        ];
        assert_eq!(
            route_click(
                &geo(&[
                    ("#browseBtn", 0.0, 0.0, 40.0, 20.0),
                    ("#shortcutsModal", 0.0, 0.0, 1440.0, 900.0),
                ]),
                &inner,
                &mut view(),
                5.0,
                5.0
            ),
            None,
            "a click inside modal content must not dismiss it"
        );
    }

    /// The `--boxes` map remains the **fallback**, and it is the answer when the
    /// shell supplied no chain for the point.
    ///
    /// That is the pre-`--serve` case (a one-shot child, a frame whose `@hit`
    /// rows were truncated) and every one of the existing headless tests: an
    /// empty chain must still route exactly as it did before, or the change would
    /// silently drop clicks whenever the chain is missing.
    #[test]
    fn the_boxes_map_still_answers_when_the_shell_supplied_no_chain() {
        let g = geo(&[
            ("#zoomInBtn", 129.0, 62.0, 27.0, 26.0),
            ("#fitBtn", 193.0, 62.0, 41.0, 26.0),
        ]);
        // No chain at all.
        assert_eq!(resolve_selector(&g, &[], 213.0, 75.0), Some("#fitBtn"));
        assert_eq!(resolve_selector(&g, &[], 140.0, 75.0), Some("#zoomInBtn"));
        // A chain that resolves to nothing must not suppress the map either.
        let useless = vec![HitNode::class("canvas-tools"), HitNode::default()];
        assert_eq!(resolve_selector(&g, &useless, 213.0, 75.0), Some("#fitBtn"));
        assert_eq!(resolve_selector(&g, &useless, 140.0, 75.0), Some("#zoomInBtn"));
        // Outside every registered box and with no chain: still nothing.
        assert_eq!(resolve_selector(&g, &[], 900.0, 900.0), None);
    }

    /// A shell chain that resolves to a registered control the `--boxes` map does
    /// NOT contain still routes.
    ///
    /// This is the real `#blocksClose` shape: its box is zero-area on a closed
    /// drawer, so the map has nothing to offer, but the shell — which laid the
    /// element out — can still name it once the drawer is open.
    #[test]
    fn a_shell_chain_routes_a_control_the_boxes_map_cannot_see() {
        let g = geo(&[("#fitBtn", 10.0, 10.0, 40.0, 20.0)]);
        let chain = vec![HitNode::shell("31", Some("blocksClose"), None)];
        assert_eq!(resolve_selector(&g, &chain, 500.0, 500.0), Some("#blocksClose"));
    }

    /// The drift guard is evaluated **before** the shell path and is not weakened
    /// by it: a drifted frame returns `None` even with a chain that resolves
    /// perfectly. A resolution change must never buy a route through a stale
    /// layout.
    #[test]
    fn a_drifted_frame_refuses_even_a_perfect_shell_chain() {
        let mut g = geo(&[("#zoomInBtn", 129.0, 62.0, 27.0, 26.0)]);
        let chain = vec![HitNode::shell("4", Some("zoomInBtn"), None)];
        assert_eq!(resolve_selector(&g, &chain, 140.0, 75.0), Some("#zoomInBtn"));

        g.drift = Some("--boxes and the painted PNG are different frames".to_string());
        assert_eq!(
            resolve_selector(&g, &chain, 140.0, 75.0),
            None,
            "a proven-drift frame must not route a click, chain or no chain"
        );
        // Even a chain naming an id that is not a registered control is refused.
        let unrelated = vec![HitNode::shell("4", Some("nodeProps"), None)];
        assert_eq!(resolve_selector(&g, &unrelated, 140.0, 75.0), None);
    }

    /// `hit_selector`'s doc promised drift-refusal for a long time while its
    /// body answered anyway. It was dead code, so nothing misrouted — but the
    /// next caller would have inherited the silent route. This pins that the
    /// promise is implemented, so the doc cannot drift from the body again.
    #[test]
    fn hit_selector_refuses_a_drifted_frame_as_its_doc_promises() {
        let mut g = geo(&[("#zoomInBtn", 129.0, 62.0, 27.0, 26.0)]);
        assert_eq!(hit_selector_in(&g, 140.0, 75.0), Some("#zoomInBtn"));

        g.drift = Some("--boxes and the painted PNG are different frames".to_string());
        assert_eq!(
            hit_selector_in(&g, 140.0, 75.0),
            None,
            "documented drift-refusal must actually refuse"
        );
        // A point outside every box is None for the ordinary reason too.
        assert_eq!(hit_selector_in(&g, 900.0, 900.0), None);
    }

    /// The real child-element cases, end to end, through the production
    /// resolution: `#shortcutsBtn` whose centre lands on `svg.shortcuts-gear`,
    /// and `#nsSelect` whose centre lands on an `<option>`.
    ///
    /// These are the two measured child hits in the real nzk_ui, and the shell
    /// hands them over exactly as the `@hit` TSV rows do: `node`, `tag`, `id`,
    /// `class`.
    #[test]
    fn the_two_real_child_hits_route_to_their_controls() {
        // Measured: @hit 1574.00 26.00 <node> svg  .shortcuts-gear, and the
        // button that owns it is #shortcutsBtn.
        let g = geo(&[("#shortcutsBtn", 1554.0, 12.0, 40.0, 28.0)]);
        let gear = vec![
            HitNode::shell("152", None, Some("shortcuts-gear")),
            HitNode::shell("151", Some("shortcutsBtn"), None),
        ];
        assert_eq!(resolve_selector(&g, &gear, 1574.0, 26.0), Some("#shortcutsBtn"));

        // Measured: @hit 1420.50 188.00 <node> option  , inside #nsSelect.
        let g = geo(&[("#nsSelect", 1200.0, 160.0, 441.0, 56.0)]);
        let option = vec![HitNode::shell("200", None, None), HitNode::shell("199", Some("nsSelect"), None)];
        assert_eq!(resolve_selector(&g, &option, 1420.5, 188.0), Some("#nsSelect"));
    }

    /// The `@hit` TSV decoder: the app-side half of the `--serve` contract.
    ///
    /// The shape is fixed in `nzk_ui/src/serve.rs` and is pinned here so the
    /// two halves cannot drift apart silently.
    #[test]
    fn shell_hit_rows_decode_into_a_provenanced_chain() {
        let tsv = "\
.topbar\t0.00\t0.00\t1600.00\t52.00\n\
@scroll\t.side\t0.00\t52.00\t0.00\t1200.00\n\
@hit\t1574.00\t26.00\t152\tsvg\t\tshortcuts-gear\n\
@hit\t1420.50\t188.00\t201\toption\t\t\n\
@hit\t900.00\t900.00\tnone\n\
@frame\t1600\t1000\t4bd131fa71b2ccc8\n";
        let hits = hits_from_tsv(tsv);
        assert_eq!(hits.len(), 2, "the 'none' row and the non-@hit rows are not hits");
        assert_eq!(hits[0].0, 1574.0);
        assert_eq!(hits[0].1, 26.0);
        assert_eq!(hits[0].2.node.as_deref(), Some("152"));
        assert_eq!(hits[0].2.id, None, "an empty id field decodes to None, not Some(\"\")");
        assert_eq!(hits[0].2.class.as_deref(), Some("shortcuts-gear"));
        assert!(hits[0].2.is_from_shell());
        // An empty class field likewise.
        assert_eq!(hits[1].2.id, None);
        assert_eq!(hits[1].2.class, None);
        // A hand-built chain has no provenance; a decoded one always does.
        assert!(!HitNode::id("fitBtn").is_from_shell());

        // The decoded chain routes exactly as the shell intended.
        let chain: Vec<HitNode> = hits.iter().map(|(_, _, n)| n.clone()).collect();
        assert_eq!(resolve_hit_chain(&chain), None, "neither hit is a registered control itself");
        assert_eq!(hits[0].2.class.as_deref(), Some("shortcuts-gear"));

        // A malformed row is skipped, never guessed at.
        assert!(hits_from_tsv("@hit\tbad\t1.0\t7\tdiv\t\t\n").is_empty());
        assert!(hits_from_tsv("").is_empty());
    }

    /// Box containment still wins over the chain: a point inside a registered
    /// control's own rect resolves to it without needing a DOM chain at all.
    #[test]
    fn box_containment_resolves_without_a_chain() {
        let g = geo(&[
            ("#zoomInBtn", 129.0, 62.0, 27.0, 26.0),
            ("#fitBtn", 193.0, 62.0, 41.0, 26.0),
        ]);
        assert_eq!(resolve_selector(&g, &[], 213.0, 75.0), Some("#fitBtn"));
        assert_eq!(resolve_selector(&g, &[], 140.0, 75.0), Some("#zoomInBtn"));
        // Outside every registered box and with no chain: nothing.
        assert_eq!(resolve_selector(&g, &[], 900.0, 900.0), None);
    }

    #[test]
    fn canvas_repaint_restores_modal_and_drawer_pixels_over_the_native_graph() {
        for selector in ["#modal", "#nodeDrawer", "#findPanel", "#blocksPanel"] {
            assert!(
                CANVAS_OVERLAYS.contains(&selector),
                "{selector} is painted inside #graph and must be restored over the canvas"
            );
        }
    }

    /// The click path is the production one: `nzk_ui_click` -> `route_click` ->
    /// `resolve_selector` + the registry's owner. Each of the owner's ten
    /// controls must produce its real action from a press at its centre.
    #[test]
    fn every_named_control_routes_its_real_action_from_a_click() {
        // The ten the owner named, each with its action and a box to press in.
        let cases: &[(&str, NzkUiAction)] = &[
            ("#backBtn", NzkUiAction::NavigateBack),
            ("#upBtn", NzkUiAction::NavigateUp),
            ("#modeGraph", NzkUiAction::ModeGraph),
            ("#modeFind", NzkUiAction::ModeFind),
            ("#browseBtn", NzkUiAction::Browse),
            ("#exportBtn", NzkUiAction::Export),
            ("#editBtn", NzkUiAction::Edit),
            ("#previewBtn", NzkUiAction::Preview),
            ("#shortcutsBtn", NzkUiAction::Shortcuts),
            ("#nsSelect", NzkUiAction::FormOpen("#nsSelect")),
        ];
        for (sel, want) in cases {
            let mut boxes = BTreeMap::new();
            boxes.insert(sel.to_string(), Rect::from_min_size(Pos2::new(10.0, 10.0), egui::vec2(80.0, 30.0)));
            let g = FrameGeometry::unverified(boxes, (1600, 1000));
            let mut view = view();
            let got = nzk_ui_click(&g, &[], &mut view, 50.0, 25.0);
            assert_eq!(got, Some(want.clone()), "press at the centre of {sel} routed wrong");
            // And it is effective on the host, not a placeholder.
            let mut state = crate::actions::NzkUiState::default();
            let effect = crate::actions::apply(&mut state, got.unwrap(), &repo());
            assert!(!effect.is_noop(), "{sel} routed to an action that does nothing");
        }
    }

    /// An Inert control routes to NO action — the honest classification, not a
    /// fake. `#shortcutsModal` is a modal backdrop with no behaviour of its own
    /// (its buttons own the actions), so a press on it must not produce an
    /// action.
    ///
    /// `#nodeCodeBtn` used to be the example here; task-2 made it a real Host
    /// control (`NzkUiAction::SourceOpen`), so it is deliberately NOT inert any
    /// more — see `drawer_route_tests::view_code_is_a_host_control_with_a_real_action`.
    #[test]
    fn inert_controls_route_to_no_action() {
        let mut boxes = BTreeMap::new();
        boxes.insert("#shortcutsModal".to_string(), Rect::from_min_size(Pos2::new(0.0, 0.0), egui::vec2(300.0, 300.0)));
        let g = FrameGeometry::unverified(boxes, (1600, 1000));
        let mut view = view();
        assert_eq!(nzk_ui_click(&g, &[], &mut view, 150.0, 150.0), None);
        // A press on empty nzk_ui is likewise nothing, not a panic.
        assert_eq!(nzk_ui_click(&g, &[], &mut view, 1500.0, 900.0), None);
    }

    /// `#fitBtn` is `Local`: it must set the view's own fit flag and return NO
    /// host action, because the host has nothing to do.
    #[test]
    fn the_local_fit_control_sets_view_state_and_returns_no_action() {
        let mut boxes = BTreeMap::new();
        boxes.insert("#fitBtn".to_string(), Rect::from_min_size(Pos2::new(0.0, 0.0), egui::vec2(50.0, 20.0)));
        let g = FrameGeometry::unverified(boxes, (1600, 1000));
        let mut view = view();
        view.fit_pending = false;
        assert_eq!(nzk_ui_click(&g, &[], &mut view, 25.0, 10.0), None);
        assert!(view.fit_pending, "#fitBtn must request a fit on the view");
    }


    /// The `@frame` wire format is pinned to exact vectors.
    ///
    /// The digest is a cross-process contract: `nzk_ui` computes it, this
    /// crate checks it. It was originally written against a verbal spec while the
    /// shell independently emitted SHA-256, and the split only surfaced as a hard
    /// gate failure on the first end-to-end run. These vectors are the fix for
    /// that class of bug — they make the sequence (FNV-1a + the length fold)
    /// executable, so a shell that "copies the FNV loop" without the fold fails
    /// HERE, in a unit test, instead of looking like real layout drift later.
    #[test]
    fn the_frame_digest_sequence_is_pinned() {
        // Hand-computed from the documented sequence, independent of the impl:
        //   h = 0xcbf29ce484222325
        //   for b in bytes: h = (h ^ b) * 0x100000001b3
        //   h = (h ^ len) * 0x100000001b3
        fn expected(bytes: &[u8]) -> String {
            const P: u64 = 0x0000_0100_0000_01b3;
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for &b in bytes {
                h = (h ^ b as u64).wrapping_mul(P);
            }
            h = (h ^ bytes.len() as u64).wrapping_mul(P);
            format!("{h:016x}")
        }
        for case in [
            &[][..],
            &[0u8][..],
            &[255u8, 0, 128, 7][..],
            &[0u8; 8][..],
            // A real 2x1 RGBA8 frame, alpha forced to 255 as the shell must.
            &[10u8, 20, 30, 255, 40, 50, 60, 255][..],
        ] {
            assert_eq!(digest_rgba(case), expected(case), "vector {case:?}");
        }
        // The length fold is the part a naive reimplementation drops: plain
        // FNV-1a over these bytes is NOT the wire format.
        fn fnv_without_length_fold(bytes: &[u8]) -> String {
            const P: u64 = 0x0000_0100_0000_01b3;
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for &b in bytes {
                h = (h ^ b as u64).wrapping_mul(P);
            }
            format!("{h:016x}")
        }
        let probe = [1u8, 2, 3, 4];
        assert_ne!(
            digest_rgba(&probe),
            fnv_without_length_fold(&probe),
            "if these ever agree the length fold was dropped from digest_rgba"
        );
        // Trailing zeroes must not collide, which is what the fold buys.
        assert_ne!(digest_rgba(&[7, 0]), digest_rgba(&[7]));
        // Everything is lowercase hex.
        assert!(digest_rgba(&[1, 2, 3]).chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    /// Every registered selector must resolve to itself through the id spelling,
    /// so the registry and the DOM id agree in both directions.
    #[test]
    fn every_registered_selector_round_trips_through_its_dom_id() {
        for c in CONTROLS {
            let id = c.selector.strip_prefix('#').expect("registry is id-keyed");
            assert_eq!(
                selector_of_id(id),
                Some(c.selector),
                "{} does not round-trip through its DOM id",
                c.selector
            );
            let chain = vec![HitNode::id(id)];
            assert_eq!(resolve_hit_chain(&chain), Some(c.selector), "{}", c.selector);
        }
    }

    /// The `@frame` row is parsed exactly, and a missing/`none`/malformed token
    /// means UNPROVEN — never a silent pass.
    #[test]
    fn the_frame_token_is_parsed_and_absent_means_unproven() {
        assert_eq!(parse_frame_token("#graph\t0\t52\t400\t300\n"), None);
        assert_eq!(parse_frame_token("@frame\t1600\t1000\tnone\n"), None);
        assert_eq!(parse_frame_token("@frame\t1600\t1000\n"), None);
        assert_eq!(parse_frame_token("@frame\tx\t1000\tdeadbeef\n"), None);
        assert_eq!(
            parse_frame_token("@scroll\t.side\t0\t0\t0\t240\n@frame\t1600\t1000\tdeadbeef\n"),
            Some(FrameToken { size: (1600, 1000), digest: "deadbeef".to_string() })
        );
        // `@`-prefixed rows are not geometry: they must not land in the box map.
        let boxes = parse_boxes("@scroll\t.side\t0\t0\t0\t240\n@frame\t1600\t1000\tx\n#graph\t1\t2\t3\t4\n");
        assert_eq!(boxes.len(), 1);
        assert!(boxes.contains_key("#graph"));
    }

    /// The digest is a function of the bytes and their length, so a length change
    /// cannot collide with a content change.
    #[test]
    fn the_paint_digest_separates_content_and_length() {
        assert_eq!(digest_rgba(&[1, 2, 3]), digest_rgba(&[1, 2, 3]));
        assert_ne!(digest_rgba(&[1, 2, 3]), digest_rgba(&[1, 2, 4]));
        assert_ne!(digest_rgba(&[1, 2, 3]), digest_rgba(&[1, 2, 3, 0]));
        assert_eq!(digest_rgba(&[]).len(), 16, "lowercase hex u64");
    }

    // -----------------------------------------------------------------------
    // The Layout ▸ slider route (task-10)
    // -----------------------------------------------------------------------

    /// The five sliders are reachable through the same geometry the hit test
    /// uses, and the box the route resolves is the box the shell laid out.
    #[test]
    fn the_layout_sliders_are_registered_as_slider_owned_controls() {
        for kind in SliderKind::ALL {
            let c = CONTROLS
                .iter()
                .find(|c| c.selector == kind.selector())
                .unwrap_or_else(|| panic!("{} is not in CONTROLS", kind.selector()));
            assert_eq!(
                c.owner,
                ControlOwner::Slider,
                "{} must be Slider-owned: a range input carries a value",
                kind.selector()
            );
        }
        // The readout spans are NOT controls: they are text, and a press on one
        // must not be claimed by the click path.
        for kind in SliderKind::ALL {
            assert!(
                !CONTROLS.iter().any(|c| c.selector == kind.readout_selector()),
                "{} is an output element, not a control",
                kind.readout_selector()
            );
        }
    }

    /// A pointer does not drift from the value: the same box drives both, and the
    /// mapping walks min -> max across the track.
    #[test]
    fn slider_route_maps_pointer_x_across_the_track() {
        // A 200px-wide spacing track (its real box is 215 wide, but the arithmetic
        // is clearer at a round number).
        let g = geo(&[("#spacingRange", 100.0, 200.0, 200.0, 16.0)]);
        let rect = *g.boxes.get("#spacingRange").unwrap();

        // Left edge (inside the thumb inset) is the minimum.
        let left = route_drag(&g, &[], rect.left() + THUMB_INSET, rect.center().y, SliderPhase::Live)
            .expect("the left end of the track is on the slider");
        assert_eq!(left.kind, SliderKind::Spacing);
        assert_eq!(left.value(), SPACING_MIN, "the left end is min");

        // Right edge is the maximum.
        let right = route_drag(&g, &[], rect.right() - THUMB_INSET, rect.center().y, SliderPhase::Live)
            .expect("the right end of the track is on the slider");
        assert_eq!(right.value(), SPACING_MAX, "the right end is max");

        // ...and it is monotone in between, so the control cannot invert.
        let mid = route_drag(&g, &[], rect.center().x, rect.center().y, SliderPhase::Live).unwrap();
        assert!(
            left.value() < mid.value() && mid.value() < right.value(),
            "expected min < {} < max",
            mid.value()
        );
    }

    /// The route clamps at BOTH ends rather than running off the range, and it
    /// snaps to the control's own step.
    #[test]
    fn slider_route_clamps_both_ends_and_snaps_to_step() {
        let g = geo(&[
            ("#spacingRange", 100.0, 100.0, 200.0, 16.0),
            ("#curveRange", 100.0, 140.0, 200.0, 16.0),
            ("#iterRange", 100.0, 180.0, 200.0, 16.0),
        ]);
        let spacing = *g.boxes.get("#spacingRange").unwrap();

        // Far past either end still yields the bound, never a value outside it.
        // The pointer is *clamped into the box* first: a drag that leaves the
        // control stops being a drag on it, which is why the route is asked at
        // the track's own ends rather than at x = -500.
        let beyond_left =
            route_drag(&g, &[], spacing.left(), spacing.center().y, SliderPhase::Live).unwrap();
        assert_eq!(beyond_left.value(), SPACING_MIN);
        let beyond_right =
            route_drag(&g, &[], spacing.right(), spacing.center().y, SliderPhase::Live).unwrap();
        assert_eq!(beyond_right.value(), SPACING_MAX);
        // Outside the box entirely is NOT a slider press at all.
        assert!(
            route_drag(&g, &[], spacing.left() - 500.0, spacing.center().y, SliderPhase::Live)
                .is_none(),
            "a pointer off the control must not claim it"
        );

        // A curve value is a 0.01 step, so an arbitrary pointer still lands on one.
        let curve = *g.boxes.get("#curveRange").unwrap();
        let c = route_drag(&g, &[], curve.center().x + 3.0, curve.center().y, SliderPhase::Live).unwrap();
        let stepped = (c.value() / 0.01).round() * 0.01;
        assert!(
            (c.value() - stepped).abs() < 1e-9,
            "curve {} is not on its 0.01 step",
            c.value()
        );
        assert!((CURVE_MIN..=CURVE_MAX).contains(&c.value()));

        // Iterations is an integer count: a snapped value must be whole.
        let iter = *g.boxes.get("#iterRange").unwrap();
        let i = route_drag(&g, &[], iter.center().x, iter.center().y, SliderPhase::Live).unwrap();
        assert_eq!(i.value().fract(), 0.0, "iterations must be a whole number");
    }

    /// A press that is not on a slider is not claimed, which is what lets the
    /// canvas and the other controls keep working.
    #[test]
    fn slider_route_declines_everything_that_is_not_a_slider() {
        let g = geo(&[("#spacingRange", 100.0, 100.0, 200.0, 16.0), ("#fitBtn", 400.0, 100.0, 40.0, 24.0)]);
        // Empty canvas.
        assert!(route_drag(&g, &[], 10.0, 10.0, SliderPhase::Live).is_none());
        // A different control.
        assert!(route_drag(&g, &[], 410.0, 110.0, SliderPhase::Live).is_none());
        // On the slider.
        assert!(route_drag(&g, &[], 200.0, 108.0, SliderPhase::Live).is_some());

        // A click on a slider must not also produce a NzkUiAction: the two
        // families must not both fire for one press.
        let mut v = view();
        assert_eq!(
            route_click(&g, &[], &mut v, 200.0, 108.0),
            None,
            "a Slider-owned control answers route_drag, never route_click"
        );
    }

    /// The live/release distinction is carried by the edit, not decided by the
    /// caller — so the timing cannot be lost between here and the view.
    #[test]
    fn the_edit_carries_whether_it_is_live_or_a_release() {
        let g = geo(&[("#spacingRange", 100.0, 100.0, 200.0, 16.0)]);
        let live = route_drag(&g, &[], 200.0, 108.0, SliderPhase::Live).unwrap();
        let release = route_drag(&g, &[], 200.0, 108.0, SliderPhase::Release).unwrap();
        assert_eq!(live.phase, SliderPhase::Live);
        assert_eq!(release.phase, SliderPhase::Release);
        // Same spot, same value — only the phase differs.
        assert_eq!(live.value(), release.value());
        assert_ne!(live, release);

        // Dragging FA2 iterations must NOT write settings per pixel; releasing
        // must. This is the asymmetry `shell.js:818-869` encodes.
        assert!(!live.writes_settings() || live.kind != SliderKind::Iterations);
        assert!(release.writes_settings());
    }

    /// `SliderKind` round-trips through the selectors `index.html` declares, so
    /// the route and the shipped DOM cannot name different controls.
    #[test]
    fn slider_kinds_round_trip_through_their_selectors() {
        for kind in SliderKind::ALL {
            assert_eq!(SliderKind::from_selector(kind.selector()), Some(kind));
            assert_eq!(SliderKind::from_selector(kind.readout_selector()), None);
        }
        assert_eq!(SliderKind::from_selector("#notASlider"), None);
        // Four the reference wires, plus the fork's own text size.
        assert_eq!(SliderKind::ALL.len(), 5);
        assert_eq!(SliderKind::TextSize.selector(), "#textSizeRange");
    }

    /// `slider_facts` publishes the live values in the shape the readout spans
    /// show, and the default settings publish NOTHING — which is what keeps the
    /// shipped facts object (and therefore the default render) unchanged.
    #[test]
    fn slider_facts_publishes_live_values_and_stays_silent_at_defaults() {
        use crate::graphviz::LayoutSettings;
        let d = LayoutSettings::default();

        // Defaults: the key is ABSENT, so the serialised facts are what they
        // were before this existed.
        let graph = Graph {
            repo: PathBuf::from("/tmp/fid-slider-facts"),
            nodes: vec![],
            edges: vec![],
            state: serde_json::json!({}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let base = facts_json_with(&graph, &NzkUiState::default());
        let with_settings = facts_json_with_settings(&graph, &NzkUiState::default(), d);
        assert!(
            with_settings.get("settings").is_none(),
            "default settings must publish no `settings` key"
        );
        assert_eq!(base, with_settings, "the default facts object must be unchanged");

        // A moved slider publishes the values, formatted as the reference shows
        // them: curvature to 2 decimals (`shell.js:813`), the rest as integers.
        let moved = LayoutSettings { spacing: 200.0, curvature: 0.05, ..d };
        let obj = facts_json_with_settings(&graph, &NzkUiState::default(), moved);
        let s = obj.get("settings").expect("a moved slider publishes `settings`");
        assert_eq!(s["spacing"], "200");
        assert_eq!(s["curvature"], "0.05", "two decimals, like the reference readout");
        assert_eq!(s["iterations"], "800");
        assert_eq!(s["nodeSize"], "8");
        assert_eq!(s["textSize"], "11", "the fork's own slider is published too");
        // The values are strings because they are span *text*: the shell needs no
        // formatting rules of its own.
        for k in ["spacing", "iterations", "curvature", "nodeSize", "textSize"] {
            assert!(s[k].is_string(), "{k} must be a string for set_text");
        }
    }

    /// Every slider's range matches the `min`/`max`/`step` in `index.html`.
    #[test]
    fn slider_ranges_match_the_shipped_controls() {
        assert_eq!(SliderKind::Spacing.range(), (SPACING_MIN, SPACING_MAX, 1.0));
        assert_eq!(SliderKind::Iterations.range(), (ITER_MIN as f64, ITER_MAX as f64, 50.0));
        assert_eq!(SliderKind::Curve.range(), (CURVE_MIN, CURVE_MAX, 0.01));
        assert_eq!(
            SliderKind::NodeSize.range(),
            (NODE_SIZE_MIN as f64, NODE_SIZE_MAX as f64, 1.0)
        );
        assert_eq!(
            SliderKind::TextSize.range(),
            (TEXT_SIZE_MIN as f64, TEXT_SIZE_MAX as f64, 1.0)
        );
    }
}

/// `#pubCount` / `#pubList`: the engine answer, over the REAL engine.
///
/// These tests drive `Index::definitions` directly against real fixture
/// corpora, because the whole point of item (b) is that the number comes from
/// the engine's raw-JSON `access` predicate and **cannot** be recomputed from a
/// [`Graph`] — `Node` has no `access` field (`app/src/model.rs:43-55`). A test
/// that built a fake `Graph` and counted nodes would be testing a second,
/// forbidden implementation of the predicate.
#[cfg(test)]
mod pub_defs_tests {
    use super::*;
    // The reference's own page size, shared with the host's query so the test
    // cannot silently use a different `limit` than the app does.
    use crate::app::PUB_DEFINITIONS_LIMIT;

    /// The engine's own fixture corpus (four `.cs` files under `App/`, `Lib/`,
    /// `Shared/` and a deliberately unparseable `Old/Legacy.cs`).
    fn projects_fixture() -> Option<PathBuf> {
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../tools/fixtures/projects");
        p.is_dir().then_some(p)
    }

    /// Index one corpus and return `(facts_json, total, rows)` where `total` and
    /// `rows` come from the SAME `Index::definitions` call the host makes.
    fn publish(root: &Path) -> (serde_json::Value, usize, usize) {
        let roots = vec![root.to_string_lossy().into_owned()];
        let (graph, index) =
            crate::model::load_with_index(root, &roots).expect("index the corpus");
        let d = index.definitions("public", PUB_DEFINITIONS_LIMIT);
        let total = d["total"].as_u64().expect("the engine always reports total") as usize;
        let rows: Vec<crate::model::PubRow> = d["definitions"]
            .as_array()
            .expect("definitions is an array")
            .iter()
            .filter_map(crate::model::PubRow::from_lite)
            .collect();
        let row_count = rows.len();

        let mut state = NzkUiState::default();
        state.pub_total = Some(total);
        state.pub_list = Some(rows);
        (facts_json_with(&graph, &state), total, row_count)
    }

    /// (b) THE PUBLISHED PATH. A real engine count reaches the facts key the
    /// host sends, at the top level where `nzk_ui`'s `Facts::pub_defs`
    /// actually reads it.
    ///
    /// The *top-level* claim is asserted, not assumed: `settings` and
    /// `hierarchy` both had to be outer keys, and serde does not
    /// `deny_unknown_fields`, so a `pub_defs` nested under `ui` would be
    /// silently dropped and every downstream span test would still pass.
    #[test]
    fn a_real_engine_count_reaches_the_top_level_pub_defs_key() {
        let Some(fixture) = projects_fixture() else {
            eprintln!("[g7] projects fixture absent; skipping");
            return;
        };
        let (facts, total, row_count) = publish(&fixture);

        let pub_defs = facts
            .get("pub_defs")
            .expect("pub_defs must be a TOP-LEVEL key, a sibling of `ui`");
        assert!(
            facts["ui"].get("pub_defs").is_none(),
            "pub_defs must NOT be nested under `ui` — serde would drop it there"
        );
        assert_eq!(
            pub_defs["total"].as_u64().map(|n| n as usize),
            Some(total),
            "the published total is the engine's own count"
        );
        assert_eq!(
            pub_defs["list"].as_array().map(|a| a.len()),
            Some(row_count),
            "the published list is the engine's own page"
        );
        // A real corpus with public declarations must not report a bare 0 —
        // this is the "false zero" the task text warns about.
        assert!(
            total > 0,
            "the fixture corpus declares public types, so the count must be > 0; \
             a 0 here means the predicate was re-implemented over `Graph` and \
             found no `access` field (which is exactly why it must not be)"
        );
    }

    /// THE CORPUS-CHANGE PROOF. The count must change when the corpus changes,
    /// and both numbers must be the engine's.
    ///
    /// Two DIFFERENT real corpora are indexed and their counts compared. This is
    /// the property a stale readout violates: if `#pubCount` were cached across
    /// a re-index, or recomputed from something that does not track the corpus,
    /// these two numbers would agree.
    ///
    /// The two fixtures are chosen because they genuinely differ — `projects`
    /// declares public types across four files, `inherit` is a tiny
    /// inheritance probe.
    #[test]
    fn the_count_changes_with_the_corpus() {
        let Some(a) = projects_fixture() else {
            eprintln!("[g7] projects fixture absent; skipping");
            return;
        };
        let b = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../tools/fixtures/inherit");
        if !b.is_dir() {
            eprintln!("[g7] inherit fixture absent; skipping");
            return;
        }

        let (facts_a, total_a, _) = publish(&a);
        let (facts_b, total_b, _) = publish(&b);

        // Report the real numbers, so the evidence is in the test log.
        eprintln!(
            "[g7] public definitions: {} -> {}, {} -> {}",
            a.display(),
            total_a,
            b.display(),
            total_b
        );

        assert_ne!(
            facts_a["pub_defs"]["total"],
            serde_json::Value::Null,
            "the first corpus must publish a count at all"
        );
        assert_ne!(
            facts_b["pub_defs"]["total"],
            serde_json::Value::Null,
            "the second corpus must publish a count at all"
        );
        assert_ne!(
            total_a, total_b,
            "two different corpora must not produce the same public-definition \
             count — if they do, the readout is not tracking the corpus"
        );
    }

    /// A corpus change CLEARS the readout before it is re-queried.
    ///
    /// This is the host-side half of the refresh. `App::reindex` drops both
    /// fields to `None`, and a `None` total must make the key vanish entirely
    /// rather than publish a stale number or a fabricated zero. Asserting
    /// "absent, not 0" is the honesty requirement the task text spells out.
    #[test]
    fn clearing_the_count_removes_the_key_rather_than_publishing_zero() {
        let Some(fixture) = projects_fixture() else {
            eprintln!("[g7] projects fixture absent; skipping");
            return;
        };
        let roots = vec![fixture.to_string_lossy().into_owned()];
        let (graph, index) =
            crate::model::load_with_index(&fixture, &roots).expect("index the corpus");

        // The state a re-index leaves behind: no count, no list.
        let empty = NzkUiState::default();
        assert!(empty.pub_total.is_none(), "a fresh state has no count");
        let facts = facts_json_with(&graph, &empty);
        assert!(
            facts.get("pub_defs").is_none(),
            "with no engine answer the key must be ABSENT (the span keeps its \
             shipped empty content), never present with a 0"
        );

        // And the engine has a real, non-zero answer available, which is what
        // the state would carry after the refresh.
        let d = index.definitions("public", PUB_DEFINITIONS_LIMIT);
        assert!(
            d["total"].as_u64().unwrap_or(0) > 0,
            "sanity: this corpus does have public definitions"
        );
    }

    /// The published rows carry the four fields the renderer reads, and a row
    /// with no identity at all is dropped rather than rendered blank.
    #[test]
    fn published_rows_carry_what_the_renderer_reads() {
        let Some(fixture) = projects_fixture() else {
            eprintln!("[g7] projects fixture absent; skipping");
            return;
        };
        let (facts, _, row_count) = publish(&fixture);
        let rows = facts["pub_defs"]["list"].as_array().expect("list array");
        assert_eq!(rows.len(), row_count);
        assert!(!rows.is_empty(), "the fixture declares public definitions");
        for r in rows {
            for k in ["id", "full", "label", "kind"] {
                assert!(r[k].is_string(), "PubRow.{k} must be a string");
            }
            // The fallback chain is applied at construction, so a node with an
            // id can never render an empty label.
            assert!(
                !r["label"].as_str().unwrap_or("").is_empty(),
                "every published row must have a non-empty label"
            );
        }

        // A row JSON with no id and no full name is refused outright.
        assert!(
            crate::model::PubRow::from_lite(&serde_json::json!({"kind": "method"})).is_none(),
            "a row that cannot name anything must be dropped, not rendered blank"
        );
    }

    /// (a) THE WIRE KEY, THROUGH THE REAL `ui_state_json` THE APP PUBLISHES.
    ///
    /// `NzkUiState::group_edges` must be emitted as the camelCase `groupEdges`
    /// — the key the nzk_ui side deserializes — and must be a real JSON
    /// boolean, never omitted, because `false` is a *value* here rather than an
    /// absence. A conditional insert (the treatment `find`/`picker` get) would
    /// make the filter impossible to turn off.
    ///
    /// This is the assertion that fails if the emit is dropped or spelled
    /// `group_edges`: the key would be absent, and the renderer would silently
    /// keep the box at its shipped default.
    #[test]
    fn the_group_edges_flag_round_trips_through_the_facts_json_in_camel_case() {
        let on = ui_state_json(&NzkUiState::default(), None);
        assert_eq!(
            on.get("groupEdges"),
            Some(&serde_json::Value::Bool(true)),
            "the default facts must carry groupEdges as a real boolean true: {on}"
        );

        let off = ui_state_json(
            &NzkUiState {
                group_edges: false,
                ..NzkUiState::default()
            },
            None,
        );
        assert_eq!(
            off.get("groupEdges"),
            Some(&serde_json::Value::Bool(false)),
            "false must be PUBLISHED as false, not omitted — an absent key leaves \
             the box at its default and the filter could never be switched off"
        );
        assert_eq!(
            off.get("group_edges"),
            None,
            "the wire key is camelCase `groupEdges`; a `group_edges` key would be \
             silently ignored by the renderer"
        );
        assert_eq!(
            off.get("group_children"),
            Some(&serde_json::Value::Bool(true)),
            "the sibling control is untouched by a groupEdges change, and it keeps \
             its own snake_case spelling"
        );
    }
    /// `#cs-tabs` / `#searchInput` are ABSENT from a fresh nzk_ui's `ui`. This
    /// is the pinned-frame contract: the reference creates the tab bar in JS
    /// only when a document is open, and the input carries no typed value, so
    /// the renderer must create/write nothing on the default paint.
    #[test]
    fn tabs_and_search_query_facts_are_absent_by_default() {
        let graph = Graph {
            repo: PathBuf::from("/tmp/fid-empty"),
            nodes: vec![],
            edges: vec![],
            state: serde_json::json!({}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let ui = facts_json_with(&graph, &NzkUiState::default())["ui"].clone();
        assert!(
            ui.get("tabs").is_none(),
            "a fresh nzk_ui has no open documents, so `tabs` must be ABSENT \
             (the renderer then creates no #cs-tabs node)"
        );
        assert!(
            ui.get("search_query").is_none(),
            "nothing typed in the topbar search box, so `search_query` must be \
             ABSENT (the renderer then writes no value attribute)"
        );
    }

    /// The `tabs` payload is exactly the renderer's frozen shape and follows
    /// the reference's active/split/title rules (`shell.js:1205`).
    #[test]
    fn tabs_facts_describe_the_open_documents() {
        let graph = Graph {
            repo: PathBuf::from("/tmp/fid-empty"),
            nodes: vec![],
            edges: vec![],
            state: serde_json::json!({}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let mut state = NzkUiState::default();
        state.tabs.open("node:A", serde_json::json!({ "node": "NZK.A" }));
        state
            .tabs
            .open("file:/tmp/B.cs", serde_json::json!({ "file": "/tmp/B.cs" }));

        let ui = facts_json_with(&graph, &state)["ui"].clone();
        let tabs = ui.get("tabs").expect("a non-empty stack publishes `tabs`");
        let rows = tabs["tabs"].as_array().expect("rows array");
        assert_eq!(rows.len(), 2, "two open documents, two rows");
        assert_eq!(tabs["split_on"], serde_json::json!(false));
        // The second `open` activated `file:/tmp/B.cs`.
        assert_eq!(rows[0]["key"], serde_json::json!("node:A"));
        assert_eq!(rows[0]["active"], serde_json::json!(false));
        assert_eq!(rows[0]["split"], serde_json::json!(false));
        assert_eq!(rows[0]["title"], serde_json::json!("NZK.A"));
        assert_eq!(rows[1]["key"], serde_json::json!("file:/tmp/B.cs"));
        assert_eq!(rows[1]["active"], serde_json::json!(true));
        assert_eq!(rows[1]["title"], serde_json::json!("/tmp/B.cs"));
        assert!(rows[1]["label"].is_string(), "the bar caption is a string");

        // Toggling the split marks ONE row and flips `split_on`; the reference's
        // `splitKeyState === t.key` rule (`shell.js:1210-1216`) is a per-key flag.
        assert!(state.tabs.toggle_split(), "two tabs, split is available");
        let ui = facts_json_with(&graph, &state)["ui"].clone();
        let tabs = &ui["tabs"];
        assert_eq!(tabs["split_on"], serde_json::json!(true));
        let rows = tabs["tabs"].as_array().expect("rows array");
        let split_rows: Vec<&serde_json::Value> =
            rows.iter().filter(|r| r["split"] == serde_json::json!(true)).collect();
        assert_eq!(split_rows.len(), 1, "exactly one document is the split");
        let active_rows: Vec<&serde_json::Value> =
            rows.iter().filter(|r| r["active"] == serde_json::json!(true)).collect();
        assert_eq!(active_rows.len(), 1, "exactly one document is active");
    }

    /// The topbar search box's value follows the **active mode's** producer, and
    /// clearing that query removes the key again (no empty-string publish).
    ///
    /// STRENGTHENED from the round-6 pin (same test name, never weakened): the
    /// old assertion demanded `search_query == find.query` unconditionally, which
    /// is the state the reference cannot reach — in graph mode the shared
    /// `#searchInput.value` is the graph query, not the Find panel's
    /// (`onSearchInput`, search.js:56-62; `setMode`, search.js:88-104). This now
    /// asserts BOTH modes and, in the graph case, also that the Find query being
    /// non-empty does not leak into the box.
    #[test]
    fn search_query_facts_track_the_find_query() {
        let graph = Graph {
            repo: PathBuf::from("/tmp/fid-empty"),
            nodes: vec![],
            edges: vec![],
            state: serde_json::json!({}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let mut state = NzkUiState::default();

        // (a) FIND MODE: `ModeFind` is what makes the input belong to the Find
        // panel, so the Find query is the box's value.
        state.surface = Some(crate::actions::HostSurface::Find);
        state.find.set_query("Needle");
        let ui = facts_json_with(&graph, &state)["ui"].clone();
        assert_eq!(
            ui.get("search_query"),
            Some(&serde_json::json!("Needle")),
            "find mode: the Find query must reach the renderer's #searchInput value"
        );

        state.find.set_query("");
        let ui = facts_json_with(&graph, &state)["ui"].clone();
        assert!(
            ui.get("search_query").is_none(),
            "a cleared query publishes NOTHING: the shipped input has no value"
        );

        // (b) GRAPH MODE: `surface == None` (what `ModeGraph`/`FocusSearch` leave
        // behind) means the box belongs to the suggestion popup. A stale Find
        // query must NOT be published, and the graph query must be.
        state.surface = None;
        state.find.set_query("StaleFindText");
        let ui = facts_json_with(&graph, &state)["ui"].clone();
        assert!(
            ui.get("search_query").is_none(),
            "graph mode: the Find panel's query must not masquerade as the graph box's value"
        );

        state.search_focused = true;
        state.suggest.set_query("GraphOnly");
        let ui = facts_json_with(&graph, &state)["ui"].clone();
        assert_eq!(
            ui.get("search_query"),
            Some(&serde_json::json!("GraphOnly")),
            "graph mode: the shared box shows the graph query"
        );
        assert_eq!(
            ui.get("suggestions").and_then(|s| s.get("rows")).is_some(),
            true,
            "a non-empty graph query on the focused box opens the suggestions channel"
        );
    }

    /// ADVERSARIAL: with the graph query empty — the default nzk_ui — the
    /// `#suggestions` key must be ABSENT. Presence of the key IS the popup-open
    /// flag, so an empty object here would clear `hidden` on the shipped box and
    /// move the pinned default frame.
    #[test]
    fn suggestions_facts_are_absent_by_default() {
        let graph = Graph {
            repo: PathBuf::from("/tmp/fid-empty"),
            nodes: vec![],
            edges: vec![],
            state: serde_json::json!({}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let ui = facts_json_with(&graph, &NzkUiState::default())["ui"].clone();
        assert!(
            ui.get("suggestions").is_none(),
            "the default paint must leave `#suggestions` hidden: no key at all"
        );
    }

    /// The frozen `ui.suggestions` shape the nzk_ui paint deserialises:
    /// `{rows:[{id,kind,label,full}], active, empty}` — with a real graph so the
    /// rows are the core's own hits (order and limit included).
    #[test]
    fn suggestions_facts_describe_rows_and_active() {
        let graph = Graph {
            repo: PathBuf::from("/tmp/fid-suggest"),
            nodes: vec![
                crate::model::Node {
                    id: "n:1".into(),
                    label: "hit".into(),
                    full: "App.Hit".into(),
                    kind: "method".into(),
                    ns: "App".into(),
                    ..crate::model::Node::default()
                },
                crate::model::Node {
                    id: "n:2".into(),
                    label: "hitsAll".into(),
                    full: "App.HitsAll".into(),
                    kind: "field".into(),
                    ns: "App".into(),
                    ..crate::model::Node::default()
                },
            ],
            edges: vec![],
            state: serde_json::json!({}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let mut state = NzkUiState::default();
        // Graph mode + the box focused (7B: the popup is focus-gated) owns the
        // suggestion channel.
        state.search_focused = true;
        state.suggest.set_query("hit");
        state.suggest.set_active(Some(0));
        let ui = facts_json_with(&graph, &state)["ui"].clone();
        let suggestions = ui
            .get("suggestions")
            .expect("a non-empty graph query publishes the popup facts");
        let rows = suggestions["rows"].as_array().expect("rows array");
        assert_eq!(rows.len(), 2, "both fixture hits are published");
        assert_eq!(rows[0]["id"], serde_json::json!("n:1"));
        assert_eq!(rows[0]["kind"], serde_json::json!("method"));
        assert_eq!(rows[0]["label"], serde_json::json!("hit"));
        assert_eq!(rows[0]["full"], serde_json::json!("App"));
        assert_eq!(suggestions["active"], serde_json::json!(0));
        assert_eq!(suggestions["empty"], serde_json::Value::Null);
        // The find panel's own facts key stays absent: the modes are disjoint.
        assert!(
            ui.get("find").is_none(),
            "the graph-mode popup must not open the Find panel"
        );
    }

    /// ADVERSARIAL: clearing the graph query CLOSES the popup by ABSENCE. A
    /// producer that instead sent `{"rows": [], "empty": null}` would leave
    /// `#suggestions` un-hidden (the key would still be present), so the box
    /// would be a visible empty rectangle — the "painted but unfed" failure.
    #[test]
    fn suggestions_facts_closed_after_clear() {
        let graph = Graph {
            repo: PathBuf::from("/tmp/fid-empty"),
            nodes: vec![],
            edges: vec![],
            state: serde_json::json!({}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let mut state = NzkUiState::default();
        state.search_focused = true;
        state.suggest.set_query("hit");
        let ui = facts_json_with(&graph, &state)["ui"].clone();
        assert!(
            ui.get("suggestions").is_some(),
            "a real query opens the popup even with no hits (the 'no matches' row)"
        );
        assert_eq!(
            ui["suggestions"]["empty"],
            serde_json::json!("no matches"),
            "the NoHits state publishes the reference's own message"
        );

        state.suggest.clear();
        let ui = facts_json_with(&graph, &state)["ui"].clone();
        assert!(
            ui.get("suggestions").is_none(),
            "a cleared graph query must hide the popup by removing the key"
        );
        assert!(
            ui.get("search_query").is_none(),
            "and the box's value must disappear with it"
        );
    }

    /// ADVERSARIAL, the 7A gap 3 fix: hiding the popup does NOT clear the typed
    /// text. The reference's `hideSuggestions` (search.js:11-20) only drops the
    /// rows and resets `searchIndex`; `#searchInput.value` survives Escape/blur.
    /// Losing the focus flag must therefore remove `ui.suggestions` while
    /// `ui.search_query` keeps painting the box's text. If the producer hid by
    /// clearing the query (the 7A workaround) this test fails on `search_query`.
    #[test]
    fn suggestions_facts_hidden_on_blur_keep_the_typed_query() {
        let graph = Graph {
            repo: PathBuf::from("/tmp/fid-empty"),
            nodes: vec![],
            edges: vec![],
            state: serde_json::json!({}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let mut state = NzkUiState::default();
        state.search_focused = true;
        state.suggest.set_query("hit");
        let ui = facts_json_with(&graph, &state)["ui"].clone();
        assert!(ui.get("suggestions").is_some(), "focused + query = open");
        assert_eq!(
            ui.get("search_query"),
            Some(&serde_json::json!("hit")),
            "an open popup paints the typed text too"
        );

        // Escape / blur: focus dropped, query kept.
        state.search_focused = false;
        let ui = facts_json_with(&graph, &state)["ui"].clone();
        assert!(
            ui.get("suggestions").is_none(),
            "unfocused must hide the popup by dropping the key"
        );
        assert_eq!(
            ui.get("search_query"),
            Some(&serde_json::json!("hit")),
            "the input's value survives Escape: #searchInput still paints it"
        );
    }
    /// `ui.shortcuts` — the `#shortcutsBody` rows (`shell.js:330-342`).
    ///
    /// The fact exists ONLY while the sheet is the open surface: the reference
    /// builds the rows in `openShortcuts` → `renderShortcuts`, and `index.html:433`
    /// ships `#shortcutsBody` EMPTY. Publishing the rows while closed would add 33
    /// children to the default document, so absence here is the byte-identical
    /// promise (`@frame` pin) — this test fails on a "publish always" wiring.
    #[test]
    fn shortcuts_facts_are_published_only_while_the_sheet_is_open() {
        let graph = Graph {
            repo: PathBuf::from("/tmp/does-not-exist"),
            nodes: Vec::new(),
            edges: Vec::new(),
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        };
        let mut state = NzkUiState::default();
        let ui = facts_json_with(&graph, &state)["ui"].clone();
        assert!(
            ui.get("shortcuts").is_none(),
            "a closed sheet must omit the key: #shortcutsBody ships empty"
        );

        state.surface = Some(crate::actions::HostSurface::Shortcuts);
        let ui = facts_json_with(&graph, &state)["ui"].clone();
        let rows = ui["shortcuts"]["rows"]
            .as_array()
            .expect("an open sheet publishes its rows");
        // The full reference list: 23 base rows plus 10 arrange rows, and the
        // native pane shortcuts that replace the removed floating pane buttons.
        assert_eq!(rows.len(), 37, "the full Settings shortcut list");
        assert_eq!(rows[0]["group"], "View");
        assert_eq!(
            rows[0]["keys"],
            serde_json::json!([["Ctrl", "Shift", "C"]])
        );
        assert_eq!(rows[0]["desc"], "Recenter the view (fit the graph)");
        assert_eq!(rows[36]["desc"], "Open this shortcuts panel");
        // Group heads are carried per row and appear in the reference's order —
        // `arrange.shortcuts()`'s Select/Arrange pair is folded in before Mouse.
        let mut groups: Vec<&str> = Vec::new();
        for row in rows {
            let g = row["group"].as_str().expect("every row carries its group");
            if groups.last() != Some(&g) {
                groups.push(g);
            }
        }
        assert_eq!(
            groups,
            vec![
                "View", "Navigate", "Code", "Tabs", "Select", "Arrange", "Mouse", "Panes", "Help"
            ],
            "shortcutGroups() order (shell.js:290-296)"
        );
        // Alternatives are carried verbatim: outer list `/`-joined, parts `+`.
        let zoom = rows
            .iter()
            .find(|r| r["desc"] == "Zoom in")
            .expect("Zoom in row");
        assert_eq!(zoom["keys"], serde_json::json!([["+"], ["="]]));
    }

    /// Middle-click on the tab bar (`onBarAux`, `shell.js:1260-1266`): only a
    /// `.cs-tab[data-key]` closes, and only the middle button routes it. The
    /// decode asserted here is the same [`bar_button`] the overlay consumes, so
    /// deleting the middle branch from the overlay makes this red.
    #[test]
    fn middle_click_closes_the_tab_and_bar_background_is_swallowed() {
        use crate::actions::TabCmd;
        // Button decode: left runs commands, middle runs the aux close, every
        // other button is not a nzk_ui press.
        assert_eq!(bar_button(egui::PointerButton::Primary), BarButton::Primary);
        assert_eq!(bar_button(egui::PointerButton::Middle), BarButton::Middle);
        assert_eq!(bar_button(egui::PointerButton::Secondary), BarButton::Other);
        assert_eq!(bar_button(egui::PointerButton::Extra1), BarButton::Other);

        // A middle press on a tab row closes that key (the same ✕ command).
        let row = vec![HitNode::shell_row(
            "310",
            None,
            Some("cs-tab active"),
            Some("node:A"),
            None,
        )];
        assert_eq!(bar_aux(&row), Some(BarAction::Close("node:A".to_string())));
        assert_eq!(
            bar_aux(&row).and_then(BarAction::into_nzk_ui_action),
            Some(NzkUiAction::Tab(TabCmd::Close("node:A".to_string())))
        );
        // A press on a label child routes up to the row's `data-key`.
        let child = vec![
            HitNode::shell_row("309", None, Some("cs-tab-label"), Some("node:A"), None),
            HitNode::shell_row("310", None, Some("cs-tab active"), Some("node:A"), None),
        ];
        assert_eq!(bar_aux(&child), Some(BarAction::Close("node:A".to_string())));
        // The bar's background is consumed (never a fall-through) but closes
        // nothing; the ✕ is not a `.cs-tab` row, so `onBarAux` ignores it.
        let background = vec![HitNode::shell_row(
            "300",
            Some("cs-tabs"),
            Some("cs-tabs"),
            None,
            None,
        )];
        assert_eq!(bar_aux(&background), Some(BarAction::Swallow));
        assert_eq!(
            bar_aux(&background).and_then(BarAction::into_nzk_ui_action),
            None
        );
        let close_x = vec![HitNode::shell_row(
            "311",
            None,
            Some("cs-tab-x"),
            None,
            Some("node:A"),
        )];
        assert_eq!(
            bar_aux(&close_x),
            Some(BarAction::Swallow),
            "onBarAux closes only `.cs-tab[data-key]` rows"
        );
        // Outside the bar nothing is ours.
        assert_eq!(bar_aux(&[HitNode::id("fitBtn")]), None);
        // Stale key: the route carries the key VERBATIM (never a fallback to the
        // active tab); the dispatch no-op for a key not in `Tabs` is pinned by
        // `actions::tests::tab_bar_commands_activate_and_close_by_key`. The chain
        // still names the bar/`.cs-tab` class — a key can go stale while the row
        // painted from the previous frame is what the hit test answered.
        let stale = vec![HitNode::shell_row(
            "312",
            None,
            Some("cs-tab"),
            Some("node:gone"),
            None,
        )];
        assert_eq!(
            bar_aux(&stale).and_then(BarAction::into_nzk_ui_action),
            Some(NzkUiAction::Tab(TabCmd::Close("node:gone".to_string())))
        );
    }

    /// The shell routes the physical PRESS, not egui's synthetic click.
    ///
    /// A browser's DOM `click` fires whenever the down and the up land on the
    /// same element: neither movement nor hold time cancels it. egui's
    /// `button_clicked` additionally requires the pointer to stay within
    /// `max_click_dist` (6 px) and the press to last under `max_click_duration`
    /// (0.8 s). The recorded interaction trace contains a 8.1 px drift that was
    /// dropped outright, and the next, zero-drift press was routed — the exact
    /// "I have to click some buttons twice" report.
    ///
    /// This pins both halves: the press routes immediately, nothing is routed
    /// again on release, and egui itself really would have refused this click,
    /// so reverting the gate to `button_clicked` fails here.
    #[test]
    fn a_drifting_click_routes_on_the_press_edge_not_egui_click() {
        fn button(pos: egui::Pos2, pressed: bool) -> egui::Event {
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            }
        }
        let ctx = egui::Context::default();
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let down = egui::pos2(100.0, 100.0);
        let up = down + egui::vec2(1.0, 8.0); // 8.06 px, past egui's 6 px limit

        let mut pressed_frame = None;
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::PointerMoved(down), button(down, true)],
                ..Default::default()
            },
            |ctx| pressed_frame = ctx.input(routed_press),
        );
        out.textures_delta.clear();

        let mut released_frame = None;
        let mut egui_click = false;
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(screen),
                events: vec![egui::Event::PointerMoved(up), button(up, false)],
                ..Default::default()
            },
            |ctx| {
                released_frame = ctx.input(routed_press);
                egui_click =
                    ctx.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary));
            },
        );
        out.textures_delta.clear();

        assert_eq!(
            pressed_frame,
            Some(BarButton::Primary),
            "the physical press must route, with no release needed"
        );
        assert_eq!(
            released_frame, None,
            "the release must not route the same gesture a second time"
        );
        assert!(
            !egui_click,
            "egui refuses a click that drifted 8 px, which is why the gate is the \
             press edge and not button_clicked"
        );
    }

    /// The round-7 `@hit` contract: the two additive trailing columns carry the
    /// runtime tab bar's routing payload, and `hits_from_tsv` decodes them. This
    /// is the literal shape `nzk_ui/src/serve.rs::hit_rows_pin_the_nine_column_shape`
    /// pins from the other side; renaming or reordering either side fails here.
    #[test]
    fn shell_hit_rows_carry_the_tab_bar_payload() {
        let tsv = "\
@hit\t812.00\t40.00\t310\tdiv\t\tcs-tab active\tnode:A\t\n\
@hit\t795.00\t40.00\t311\tbutton\t\tcs-tab-x\t\tnode:A\n\
@hit\t1200.00\t38.00\t305\tbutton\tcsSplitBtn\tbtn btn-ghost\t\t\n\
@hit\t900.00\t900.00\tnone\n";
        let hits = hits_from_tsv(tsv);
        assert_eq!(hits.len(), 3, "the 'none' row is not a hit");
        assert_eq!(hits[0].2.data_key.as_deref(), Some("node:A"));
        assert_eq!(hits[0].2.data_close, None);
        assert_eq!(hits[1].2.data_close.as_deref(), Some("node:A"));
        assert_eq!(hits[1].2.data_key, None);
        assert_eq!(hits[2].2.id.as_deref(), Some("csSplitBtn"));
        assert!(hits.iter().all(|(_, _, n)| n.is_from_shell()));
        // A pre-round-7 row (no payload columns) still decodes, to no payload.
        let old = hits_from_tsv("@hit\t10.00\t10.00\t7\tdiv\t\t\n");
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].2.data_key, None);
        assert_eq!(old[0].2.data_close, None);
    }

    /// The shell's `@hit` row carries the ancestor `id`/`class` trail, and the
    /// host turns it into extra chain entries so the reference's
    /// `ev.target.closest(...)` class resolvers work from the deepest hit.
    ///
    /// The two cases below are the reported defect itself: a button's label is
    /// an anonymous inline box (no id, no class) and a row's label is a
    /// `span.rn-label`. Both must resolve to their enclosing control — the
    /// drawer button (`.prop-action`) and the relation row (`.rel-node`) — while
    /// the id -> control walk must NOT climb to a drawer/panel container.
    #[test]
    fn a_shell_hit_chain_carries_the_ancestor_classes_closest_resolvers_need() {
        // A press on the text of the drawer's `Expand` button. Blitz reports
        // the anonymous inline box (node 842, tag div, no id, no class) and the
        // button's own data-key has already been walked up by the shell.
        let tsv = "@hit\t49.00\t228.00\t842\tdiv\t\t\t\
act:expand:#ns:VF.Inspector\t\t\t\t\
@btn btn-ghost prop-action|@prop-actions|nodeProps@|nodeDrawer@\n";
        let chain = hit_chain_from_tsv(tsv);
        assert_eq!(chain.len(), 5, "the hit plus four ancestors");
        assert_eq!(chain[0].node.as_deref(), Some("842"));
        assert!(!chain[0].ancestor, "the first entry is the hit itself");
        assert!(chain[0].class.is_none(), "the anonymous label has no identity");
        assert!(chain[1..].iter().all(|h| h.ancestor));
        assert_eq!(
            drawer_action_click(&chain),
            Some(DrawerClick::Expand("#ns:VF.Inspector".to_string())),
            "the label press must reach the button's `.prop-action` ancestor"
        );
        // The id walk is not fooled by the containers above the hit.
        assert_eq!(
            resolve_hit_chain(&chain),
            None,
            "no registered id ON the hit; #nodeDrawer in the trail must not become the target"
        );

        // A press on a relation row's label: the deepest element is the label
        // span, the row is the enclosing `.rel-node` button.
        let tsv = "@hit\t65.00\t309.00\t667\tspan\t\trn-label\tT:VF\t\t\t\t\
@rel-node|rel-nodes|relationsBody@|nodeDrawer@\n";
        let chain = hit_chain_from_tsv(tsv);
        assert_eq!(
            relation_row_click(&chain).as_deref(),
            Some("T:VF"),
            "the payload rides the hit; the row class comes from the trail"
        );
        assert_eq!(drawer_action_click(&chain), None, "a row is not a group toggle");

        // A pre-trail row decodes exactly as before: one node, no ancestors.
        let old = hit_chain_from_tsv("@hit\t10.00\t10.00\t7\tdiv\t\t\n");
        assert_eq!(old.len(), 1);
        assert!(!old[0].ancestor);
    }

    /// `bar_click` — the reference's `onBarClick`/`onBarAux` routing
    /// (`shell.js:1250-1266`) over the shell's chain. Adversarial by
    /// construction: every arm asserts the exact command, so deleting a
    /// dispatch arm or weakening a disabled predicate fails.
    #[test]
    fn tab_bar_chain_routes_to_the_reference_commands() {
        let mut tabs = Tabs::new();
        tabs.open("node:A", serde_json::json!({ "node": "A" }));
        tabs.open("file:/tmp/B.cs", serde_json::json!({ "file": "/tmp/B.cs" }));

        // The ✕ carries `data-close` (the shell also resolves the row's
        // `data-key` onto it; close wins, matching onBarClick's first branch).
        let close = vec![HitNode::shell_row(
            "311",
            None,
            Some("cs-tab-x"),
            Some("node:A"),
            Some("node:A"),
        )];
        assert_eq!(
            bar_click(&close, &tabs),
            Some(BarAction::Close("node:A".to_string()))
        );
        // A press on a row's label child routes up to the row's `data-key`.
        let label = vec![HitNode::shell_row(
            "309",
            None,
            Some("cs-tab-label"),
            Some("file:/tmp/B.cs"),
            None,
        )];
        assert_eq!(
            bar_click(&label, &tabs),
            Some(BarAction::Activate("file:/tmp/B.cs".to_string()))
        );
        // The two action buttons.
        let split = vec![HitNode::shell_row(
            "305",
            Some("csSplitBtn"),
            Some("btn btn-ghost"),
            None,
            None,
        )];
        assert_eq!(bar_click(&split, &tabs), Some(BarAction::ToggleSplit));
        let others = vec![HitNode::shell_row(
            "306",
            Some("csTabCloseOthers"),
            Some("btn btn-ghost"),
            None,
            None,
        )];
        assert_eq!(bar_click(&others, &tabs), Some(BarAction::CloseOthers));
        // The bar's background is consumed, never a fall-through.
        let background = vec![HitNode::shell_row(
            "300",
            Some("cs-tabs"),
            Some("cs-tabs"),
            None,
            None,
        )];
        assert_eq!(bar_click(&background, &tabs), Some(BarAction::Swallow));
        // Outside the bar, nothing is consumed.
        assert_eq!(bar_click(&[HitNode::id("fitBtn")], &tabs), None);

        // Disabled truth is the app's own Tabs model: one tab and no split means
        // `#csSplitBtn` is disabled (painted `disabled` by apply_tabs under the
        // same predicate) and fires nothing — but still swallows the press.
        let mut one = Tabs::new();
        one.open("node:A", serde_json::json!({ "node": "A" }));
        assert_eq!(bar_click(&split, &one), Some(BarAction::Swallow));
        assert_eq!(bar_click(&others, &one), Some(BarAction::Swallow));
        assert_eq!(
            bar_click(&split, &one).and_then(BarAction::into_nzk_ui_action),
            None,
            "a disabled button must not reach the dispatch table"
        );
    }

    /// Round-8 collapse click: `#csSplitCollapse` is a runtime-created node with
    /// no [`CONTROLS`] entry, so its press can only be resolved from the shell's
    /// hit chain. It maps to `TabCmd::ToggleSplit` and is consumed.
    ///
    /// ADVERSARIAL: fails if the `split_collapse_click` arm is dropped from the
    /// overlay (the press then falls through to `route_click`, which has no entry
    /// for this id) and fails if the resolver fires while the split is off — that
    /// would let a press on a node that is not painted ENABLE the split.
    #[test]
    fn split_collapse_press_is_consumed_and_toggles_only_a_live_split() {
        let mut tabs = Tabs::new();
        tabs.open("node:A", serde_json::json!({ "node": "A" }));
        tabs.open("node:B", serde_json::json!({ "node": "B" }));
        assert!(tabs.toggle_split(), "two tabs can split");

        let collapse = vec![HitNode::shell_row(
            "700",
            Some("csSplitCollapse"),
            Some("btn btn-ghost"),
            None,
            None,
        )];
        assert_eq!(
            split_collapse_click(&collapse, &tabs),
            Some(BarAction::ToggleSplit)
        );
        assert_eq!(
            split_collapse_click(&collapse, &tabs).and_then(BarAction::into_nzk_ui_action),
            Some(NzkUiAction::Tab(crate::actions::TabCmd::ToggleSplit))
        );
        // The button paints a ✕ text node; a press on it arrives as a chain whose
        // nearest row has no id and whose ancestor is the button. The nearest-first
        // scan must still find the ancestor.
        let child = vec![
            HitNode::shell_row("701", None, None, None, None),
            HitNode::shell_row("700", Some("csSplitCollapse"), None, None, None),
        ];
        assert_eq!(
            split_collapse_click(&child, &tabs),
            Some(BarAction::ToggleSplit)
        );

        // Anywhere else: not ours, so nothing is consumed.
        assert_eq!(split_collapse_click(&[HitNode::id("fitBtn")], &tabs), None);

        // Split off: the button is not painted, and a stale chain naming it is
        // swallowed WITHOUT enabling the split (the disabled-control rule).
        let mut off = Tabs::new();
        off.open("node:A", serde_json::json!({ "node": "A" }));
        off.open("node:B", serde_json::json!({ "node": "B" }));
        assert!(!off.split());
        assert_eq!(
            split_collapse_click(&collapse, &off),
            Some(BarAction::Swallow)
        );
        assert_eq!(
            split_collapse_click(&collapse, &off).and_then(BarAction::into_nzk_ui_action),
            None,
            "with the split off the collapse press must be a no-op"
        );
    }

    // ------------------------------------------------------------------
    // Round 9: suggestion-row click commit (`shell.js:527-536`)
    // ------------------------------------------------------------------

    /// The ten-column `@hit` row carries the row's `data-id`, and a press on the
    /// row's child span routes up to it — the reference's
    /// `ev.target.closest('.sug')`.
    #[test]
    fn suggestion_row_press_commits_the_row_data_id() {
        let tsv = "@hit\t320.00\t92.00\t412\tbutton\t\tsug active\t\t\tNZK.A.m\n";
        let hits = hits_from_tsv(tsv);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].2.class.as_deref(), Some("sug active"));
        assert_eq!(
            hits[0].2.data_id.as_deref(),
            Some("NZK.A.m"),
            "the tenth column is the row's node id"
        );

        let chain = vec![
            HitNode::shell_row_payload(
                "413",
                None,
                Some("sug-label"),
                None,
                None,
                None,
                None,
            ),
            hits[0].2.clone(),
        ];
        assert_eq!(
            suggestion_row_click(&chain),
            Some(SuggestHit::Commit("NZK.A.m".to_string())),
            "a press on the row's label commits the row's data-id"
        );
    }

    /// A row with no payload (the `.sug-empty` message, or a row painted by a
    /// shell that predates the column) is consumed and commits NOTHING — it can
    /// never fall through to a graph click naming some other node.
    #[test]
    fn suggestion_row_without_payload_is_swallowed_not_committed() {
        let empty = vec![HitNode::shell_row("500", None, Some("sug-empty"), None, None)];
        assert_eq!(suggestion_row_click(&empty), Some(SuggestHit::Swallow));

        let payloadless_row = vec![HitNode::shell_row("501", None, Some("sug"), None, None)];
        assert_eq!(
            suggestion_row_click(&payloadless_row),
            Some(SuggestHit::Swallow),
            "stale/absent id -> swallowed, never a commit guess"
        );

        // The popup box/background itself is also consumed.
        let box_chain = vec![HitNode::shell_row("502", Some("suggestions"), None, None, None)];
        assert_eq!(suggestion_row_click(&box_chain), Some(SuggestHit::Swallow));
    }

    /// Popup-hidden no-op: while no popup is painted, no chain can be the
    /// suggestion route — even one carrying a `data-id` — so ordinary nzk_ui
    /// routing is unaffected and nothing is committed.
    #[test]
    fn a_press_outside_the_popup_is_not_a_suggestion_press() {
        assert_eq!(suggestion_row_click(&[]), None);
        let graph_chain = vec![
            HitNode::shell_row_payload(
                "900",
                None,
                Some("node"),
                None,
                None,
                Some("NZK.A.m"),
                None,
            ),
            HitNode::shell_row("1", None, Some("graph"), None, None),
        ];
        assert_eq!(
            suggestion_row_click(&graph_chain),
            None,
            "the popup's presence gate is the popup, not the payload"
        );
    }

    /// The staged host queue preserves order and drains exactly once, so two
    /// presses in one frame cannot silently drop a commit.
    #[test]
    fn a_staged_suggestion_commit_is_taken_exactly_once() {
        let _ = take_pending_suggestions();
        push_suggestion("node:A".to_string());
        push_suggestion("node:B".to_string());
        assert_eq!(
            take_pending_suggestions(),
            vec!["node:A".to_string(), "node:B".to_string()]
        );
        assert!(take_pending_suggestions().is_empty(), "the drain empties the queue");
    }
}

/// Divider-drag math, hit routing and the start/move/end state machine
/// (`shell.js:1505-1530`). Separate module so it does not touch the pointer
/// tests: every function under test is the production one `overlay` calls.
#[cfg(test)]
mod split_drag_tests {
    use super::*;

    fn divider_chain() -> Vec<HitNode> {
        vec![HitNode::shell_row(
            "900",
            Some("csSplitDivider"),
            Some("pane-splitter pane-splitter-v"),
            None,
            None,
        )]
    }

    /// `clamp((clientX - wrap.left) / wrap.width, 0.15, 0.85)`.
    #[test]
    fn split_frac_tracks_the_pointer_and_clamps_to_the_reference_band() {
        // A 1000pt wrap starting at x=100.
        assert_eq!(split_frac_for_x(100.0, 1000.0, 600.0), Some(0.5));
        assert_eq!(split_frac_for_x(100.0, 1000.0, 310.0), Some(0.21));
        // Left/right of the band clamp, never beyond 0.15/0.85.
        assert_eq!(split_frac_for_x(100.0, 1000.0, 0.0), Some(0.15));
        assert_eq!(split_frac_for_x(100.0, 1000.0, 1200.0), Some(0.85));
        // Degenerate spans are refused, so the position is left unchanged: the
        // reference returns early when the wrap rect has no width.
        assert_eq!(split_frac_for_x(100.0, 0.0, 300.0), None);
        assert_eq!(split_frac_for_x(100.0, -5.0, 300.0), None);
        assert_eq!(split_frac_for_x(f32::NAN, 1000.0, 300.0), None);
        assert_eq!(split_frac_for_x(100.0, 1000.0, f32::INFINITY), None);
    }

    /// The wrap span is reconstructed from the same frame's `#sourceBody` and
    /// `#csSplitHost` boxes: body.left is the wrap's left edge, host.right the
    /// wrap's right edge (the stylesheet's border-less flex row).
    #[test]
    fn split_wrap_span_comes_from_the_body_and_host_boxes() {
        let body = Rect::from_min_size(egui::pos2(120.0, 40.0), egui::vec2(500.0, 600.0));
        let host = Rect::from_min_size(egui::pos2(627.0, 40.0), egui::vec2(493.0, 600.0));
        assert_eq!(split_wrap_span(body, host), Some((120.0, 1000.0)));
        // Missing/degenerate boxes cannot produce a ratio.
        let zero = Rect::from_min_size(egui::pos2(120.0, 40.0), egui::vec2(0.0, 600.0));
        assert_eq!(split_wrap_span(body, zero), None);
    }

    /// The divider is resolved from the shell's hit chain (it has no `CONTROLS`
    /// selector and no `--boxes` row): the exact element, or an ancestor row.
    #[test]
    fn the_divider_press_is_resolved_from_the_hit_chain() {
        assert!(split_divider_hit(&divider_chain()));
        let child = vec![
            HitNode::shell_row("901", None, None, None, None),
            HitNode::shell_row("900", Some("csSplitDivider"), None, None, None),
        ];
        assert!(split_divider_hit(&child));
        // Anything else is not ours, so the press falls through to the normal
        // click path rather than being swallowed.
        assert!(!split_divider_hit(&[HitNode::id("fitBtn")]));
        assert!(!split_divider_hit(&[]));
        assert!(!split_divider_hit(&[HitNode::shell_row(
            "902",
            Some("csSplitCollapse"),
            None,
            None,
            None
        )]));
    }

    /// Full start/move/end sequence: begin only on the divider, moves carry the
    /// clamped fraction, the release ends exactly once.
    #[test]
    fn split_drag_moves_and_ends_once() {
        let mut drag = SplitDrag::new();
        assert!(!drag.is_active());
        assert_eq!(drag.move_to(100.0, 1000.0, 600.0), None, "no drag yet");
        assert_eq!(drag.end(), None);

        let begin = drag.begin(&divider_chain()).expect("divider press begins");
        assert_eq!(begin.phase, SplitDragPhase::Begin);
        assert_eq!(begin.frac, None);
        assert!(drag.is_active());

        let mv = drag.move_to(100.0, 1000.0, 700.0).expect("live move");
        assert_eq!(mv.phase, SplitDragPhase::Move);
        assert_eq!(mv.frac, Some(0.6));
        // A move outside the wrap still clamps inside the band.
        assert_eq!(drag.move_to(100.0, 1000.0, 5000.0).unwrap().frac, Some(0.85));

        let end = drag.end().expect("release ends");
        assert_eq!(end.phase, SplitDragPhase::End);
        assert!(!drag.is_active());
        assert_eq!(drag.end(), None, "a second release is not a transition");
        assert_eq!(drag.move_to(100.0, 1000.0, 700.0), None);
    }

    /// A press that is not on the divider never starts a drag — even if it is in
    /// the split body — so the normal click routing is untouched.
    #[test]
    fn a_non_divider_press_does_not_begin_a_drag() {
        let mut drag = SplitDrag::new();
        assert_eq!(drag.begin(&[HitNode::id("csSplitCollapse")]), None);
        assert!(!drag.is_active());
    }
}

#[cfg(test)]
mod find_row_tests {
    use super::*;

    fn row(class: &str, data_key: Option<&str>) -> HitNode {
        HitNode {
            class: Some(class.to_string()),
            data_key: data_key.map(str::to_string),
            ..HitNode::default()
        }
    }

    /// A definition/implementation row carries its node id as `find:node:<id>`
    /// and resolves to [`FindRowTarget::Node`] (`activateFindRow` ->
    /// `openFindNode`, `search.js:390-398`).
    #[test]
    fn a_definition_row_routes_to_its_node() {
        let chain = vec![
            row("fname", None),
            row("frow", Some("find:node:T:Build")),
        ];
        assert_eq!(
            find_row_click(&chain),
            Some(FindRowTarget::Node("T:Build".to_string()))
        );
    }

    /// A usage row carries `find:usage:<node>|<file>|<line>` and resolves to the
    /// file:line target plus the enclosing node (`openUsage`, `search.js:400-406`).
    #[test]
    fn a_usage_row_routes_to_its_file_line_and_enclosing_node() {
        let chain = vec![row("frow", Some("find:usage:T:Ctx|a.cs|42"))];
        assert_eq!(
            find_row_click(&chain),
            Some(FindRowTarget::Usage {
                node_id: "T:Ctx".to_string(),
                file: "a.cs".to_string(),
                line: 42,
            })
        );
    }

    /// A press that is not on a `.frow` is not a Find row, and a row whose
    /// payload is not a Find target resolves to nothing (never a guessed node).
    #[test]
    fn a_non_row_or_unknown_payload_is_not_claimed() {
        assert_eq!(find_row_click(&[row("something", Some("find:node:T:A"))]), None);
        assert_eq!(find_row_click(&[row("frow", None)]), None);
        assert_eq!(find_row_click(&[row("frow", Some("tab:node:A"))]), None);
        assert_eq!(find_row_click(&[row("frow", Some("find:node:"))]), None);
    }
}

#[cfg(test)]
mod drawer_route_tests {
    use super::*;
    use super::dispatch_tests::{boxes_for, minimal_facts, shell_bin, work_dir};
    use super::interaction_tests::{geo, view};
    use crate::actions::HostSurface;

    /// A hit-chain entry with a class and the shell's nearest-`data-key` payload.
    fn hit(class: &str, key: &str) -> HitNode {
        let mut h = HitNode::shell("1", None, Some(class));
        h.data_key = Some(key.to_string());
        h
    }

    /// Pressing an OPEN drawer's body must not toggle the drawer shut.
    ///
    /// The reference attaches no click handler to `#nodeDrawer` itself — only
    /// `#nodeDrawerClose` (`shell.js:562-568`) — but the flat `--boxes` map
    /// resolves any point inside the open drawer to the container. That made
    /// "click inside the panel you just opened" equal "close the panel", which
    /// is the reported bug. The container is Inert now; both the shell chain
    /// (which names the body, not a control) and the box fallback must refuse.
    #[test]
    fn a_press_on_an_open_drawers_body_does_not_toggle_the_drawer() {
        let g = geo(&[
            ("#nodeDrawer", 0.0, 52.0, 320.0, 800.0),
            ("#nodeDrawerClose", 288.0, 60.0, 24.0, 24.0),
            ("#relationsHead", 8.0, 300.0, 300.0, 24.0),
        ]);
        // The real shell chain for a press on the drawer's body padding.
        let body = vec![HitNode::shell("232", None, Some("drawer-body"))];
        assert_eq!(
            route_click(&g, &body, &mut view(), 174.0, 747.0),
            None,
            "a press on the panel body must be consumed, not close the panel"
        );
        // A chain with no shell provenance falls back to the box map; the box
        // map must not turn the container into a control either.
        assert_eq!(route_click(&g, &[], &mut view(), 174.0, 747.0), None);
        // The explicit close button still routes.
        let close = vec![HitNode::shell("233", Some("nodeDrawerClose"), None)];
        assert_eq!(
            route_click(&g, &close, &mut view(), 300.0, 72.0),
            Some(NzkUiAction::ToggleDrawer("nodeDrawer"))
        );
    }

    /// The docked drawer's runtime-created controls resolve from the shell's hit
    /// chain, and the group controls (children of a row) beat the row resolver.
    #[test]
    fn drawer_action_click_classifies_crumb_group_toggle_and_prop_action() {
        // Crumb: navigate to that ancestor.
        assert_eq!(
            drawer_action_click(&[hit("crumb", "n:2")]),
            Some(DrawerClick::Select("n:2".to_string()))
        );
        // ± / caret / label: the group toggle, keyed "<node>|<bucket>".
        assert_eq!(
            drawer_action_click(&[hit("rel-caret", "n:1|callers")]),
            Some(DrawerClick::ToggleGroup("n:1".into(), "callers".into()))
        );
        assert_eq!(
            drawer_action_click(&[hit("rel-toggle", "n:1|children")]),
            Some(DrawerClick::ToggleGroup("n:1".into(), "children".into()))
        );
        assert_eq!(
            drawer_action_click(&[hit("rel-label", "n:1|uses")]),
            Some(DrawerClick::ToggleGroup("n:1".into(), "uses".into()))
        );
        // Prop-actions row: `act:<act>:<node>`.
        for (act, want) in [
            ("act:expand:n:1", DrawerClick::Expand("n:1".into())),
            ("act:hidech:n:1", DrawerClick::HideChildren("n:1".into())),
            ("act:chain:n:1", DrawerClick::ResolveChain("n:1".into())),
        ] {
            assert_eq!(
                drawer_action_click(&[hit("btn btn-ghost prop-action", act)]),
                Some(want),
                "{act}"
            );
        }
        // A plain relation row's child is NOT a drawer action: the row resolver
        // (`relation_row_click`) must still see it.
        assert_eq!(drawer_action_click(&[hit("rn-label", "n:2")]), None);
        assert_eq!(drawer_action_click(&[]), None);
    }

    /// The header button is Host-owned now (`from_selector` + registry agree).
    #[test]
    fn view_code_is_a_host_control_with_a_real_action() {
        let control = CONTROLS
            .iter()
            .find(|c| c.selector == "#nodeCodeBtn")
            .expect("#nodeCodeBtn stays registered");
        assert!(matches!(control.owner, ControlOwner::Host), "not Inert any more");
        assert_eq!(
            NzkUiAction::from_selector("#nodeCodeBtn"),
            NzkUiAction::SourceOpen
        );
    }

    // ---------------------------------------------------------------- filters
    //
    // The task-1 wiring: every Filters-section control a press can land on must
    // resolve to a real transition, from the SAME nine-column `@hit` rows the
    // shell actually sends. These tests are written against the wire shape
    // (`@hit x y node tag id class data-key data-close data-id`) rather than
    // hand-built `HitNode`s where it matters, so a reorder on either side fails
    // here as well as in `serve.rs`'s own pin.

    use crate::actions::FilterCmd;

    /// One `@hit` row with a class and the payload columns filled positionally.
    fn hit_row(class: &str, data_key: &str, data_id: &str) -> Vec<HitNode> {
        let row = format!("@hit\t10.00\t10.00\t42\tlabel\t\t{class}\t{data_key}\t\t{data_id}\n");
        hits_from_tsv(&row).into_iter().map(|(_, _, n)| n).collect()
    }

    #[test]
    fn a_press_on_a_facet_child_routes_from_the_data_key_ancestor_payload() {
        // The pointer lands on `.facet-name` (a child span) — no id, no data-id,
        // but the nearest-ancestor `data-key` names the chip. This is the exact
        // case the old class+data-id resolver missed, and the reason kind chips
        // only worked when the press landed on the label's own padding.
        let chain = hit_row("facet-name", "facet:method", "");
        assert_eq!(
            filter_click(&chain),
            Some(FilterCmd::ToggleKind("method".to_string()))
        );
        // The pre-data-key channel (class + data-id on the label itself) still
        // resolves: an older shell or a synthetic row keeps working.
        let old = hit_row("facet on", "", "method");
        assert_eq!(
            filter_click(&old),
            Some(FilterCmd::ToggleKind("method".to_string()))
        );
        // An edge chip's child span.
        let edge = hit_row("edgekey off", "edgekey:calls", "");
        assert_eq!(
            filter_click(&edge),
            Some(FilterCmd::ToggleEdge("calls".to_string()))
        );
        // A project chip's child span.
        let proj = hit_row("edgekey", "proj:p1", "");
        assert_eq!(
            filter_click(&proj),
            Some(FilterCmd::ToggleProject("p1".to_string()))
        );
    }

    #[test]
    fn every_static_filter_payload_routes_to_its_reference_transition() {
        for (key, want) in [
            ("filter:kindsAll", FilterCmd::AllKinds(true)),
            ("filter:kindsNone", FilterCmd::AllKinds(false)),
            ("filter:edgesAll", FilterCmd::AllEdges(true)),
            ("filter:edgesNone", FilterCmd::AllEdges(false)),
            ("filter:projAll", FilterCmd::AllProjects(true)),
            ("filter:projNone", FilterCmd::AllProjects(false)),
            ("filter:nsAll", FilterCmd::AllNamespaces),
            ("filter:nsDeep", FilterCmd::FlipNsDeep),
            ("filter:hideEdgeLabels", FilterCmd::FlipVtMode),
            ("filter:hideIsolated", FilterCmd::FlipHideIsolated),
            ("filter:groupChildren", FilterCmd::FlipGroupChildren),
            ("filter:hideRelations", FilterCmd::FlipGroupChildren),
            ("filter:groupEdges", FilterCmd::FlipGroupEdges),
        ] {
            assert_eq!(
                filter_click(&hit_row("check", key, "")),
                Some(want),
                "data-key {key} must route to its transition"
            );
        }
        // Static ids pressed on their own box (label text misses).
        for (id, want) in [
            ("resetFilters", FilterCmd::Reset),
            ("pruneHidden", FilterCmd::Prune),
            ("nsAllBtn", FilterCmd::AllNamespaces),
            ("nsDeep", FilterCmd::FlipNsDeep),
            ("hideEdgeLabels", FilterCmd::FlipVtMode),
            ("hideIsolated", FilterCmd::FlipHideIsolated),
            ("hideChildren", FilterCmd::FlipGroupChildren),
            ("kindChildWrap", FilterCmd::FlipGroupChildren),
            ("kindEdgeWrap", FilterCmd::FlipGroupEdges),
        ] {
            let chain = vec![HitNode::id(id)];
            assert_eq!(filter_click(&chain), Some(want), "#{id} must route");
        }
        // A press that is not a filter at all stays unresolved.
        assert_eq!(filter_click(&vec![HitNode::id("zoomInBtn")]), None);
    }

    #[test]
    fn every_registered_sidebar_filter_control_has_an_effective_action() {
        // The registry and `from_selector` must agree for every Host control in
        // the Filters section, otherwise a control is painted with a box (and a
        // hit entry) but dispatches nothing — the dead-button class this exists
        // to prevent. `#depthSelect` is Host-owned now: its press opens the
        // sidebar text-input path (`form_wire`'s `#depthSelect` field), and the
        // committed value travels to the view through `Effect::depth`.
        for c in CONTROLS.iter().filter(|c| c.owner == ControlOwner::Host) {
            let action = NzkUiAction::from_selector(c.selector);
            assert!(
                action.is_effective(),
                "{} is Host-owned but resolves to a no-op",
                c.selector
            );
        }
        // The exact transitions for the filter family the reference wires.
        for (sel, want) in [
            ("#nsAllBtn", FilterCmd::AllNamespaces),
            ("#nsDeep", FilterCmd::FlipNsDeep),
            ("#hideEdgeLabels", FilterCmd::FlipVtMode),
            ("#hideIsolated", FilterCmd::FlipHideIsolated),
            ("#hideChildren", FilterCmd::FlipGroupChildren),
            ("#kindChildWrap", FilterCmd::FlipGroupChildren),
            ("#kindEdgeWrap", FilterCmd::FlipGroupEdges),
        ] {
            assert_eq!(
                NzkUiAction::from_selector(sel),
                NzkUiAction::Filter(want),
                "{sel}"
            );
        }
        // The depth input is wired through the same form path as the other text
        // fields: a press focuses it, and the committed number is a real view
        // transition (`Effect::depth`).
        let depth = CONTROLS
            .iter()
            .find(|c| c.selector == "#depthSelect")
            .expect("#depthSelect stays registered");
        assert!(matches!(depth.owner, ControlOwner::Host));
        assert_eq!(
            NzkUiAction::from_selector("#depthSelect"),
            NzkUiAction::FormOpen("#depthSelect")
        );
    }

    #[test]
    fn a_filters_only_facts_change_changes_the_repaint_key() {
        // THE REGRESSION PIN for the stale key: two facts documents that differ
        // ONLY in the sidebar filter model must hash differently, because that is
        // the single thing that makes `paint_in` rebuild. Identical documents
        // must hash the same, or every idle frame would rebuild (repaint storm).
        let base = serde_json::json!({
            "repo": "/r",
            "ui": {
                "collapsed": null,
                "filters": {
                    "kinds": [{"kind": "method", "on": true}],
                    "edges": [{"kind": "calls", "on": true}],
                    "namespaces": ["NZK.A"],
                    "namespace": "",
                    "ns_deep": true,
                    "hide_isolated": false,
                    "hide_edge_labels": false,
                }
            }
        });
        let stable = facts_digest(&base);
        assert_eq!(stable, facts_digest(&base), "identical facts must not rebuild");

        let mut chip_off = base.clone();
        chip_off["ui"]["filters"]["kinds"][0]["on"] = serde_json::json!(false);
        assert_ne!(
            stable,
            facts_digest(&chip_off),
            "a facet chip toggling must force a nzk_ui rebuild"
        );

        // ...and the same for every individual filter field, including the ones
        // that were NOT in the old hand-written key at all.
        for pointer in [
            "/ui/filters/edges/0/on",
            "/ui/filters/namespace",
            "/ui/filters/ns_deep",
            "/ui/filters/hide_isolated",
            "/ui/filters/hide_edge_labels",
        ] {
            let mut changed = base.clone();
            let leaf = changed.pointer_mut(pointer).expect("path exists");
            *leaf = match leaf {
                serde_json::Value::Bool(b) => serde_json::json!(!*b),
                _ => serde_json::json!("NZK.B"),
            };
            assert_ne!(
                stable,
                facts_digest(&changed),
                "{pointer} must be part of the repaint key"
            );
        }
    }

    /// Every interactive control that used to be painted-but-dead now has a real
    /// nzk_ui box AND resolves to its host action from that box.
    ///
    /// This is the offscreen end-to-end check the registry-only tests cannot
    /// The topbar text box is a real hit target, not merely painted text.
    ///
    /// An earlier control census reported `#searchInput` as `no_hit`; this is
    /// the end-to-end pin against that class of regression: the REAL shell lays
    /// out the default frame, and the centre of its box must resolve to the
    /// input's selector so a press can focus it.
    #[test]
    fn the_topbar_search_box_is_hit_testable() {
        let Some(bin) = shell_bin() else {
            eprintln!("nzk_ui: skip (nzk_ui not built)");
            return;
        };
        let dir = work_dir("search-input-hit");
        let state = NzkUiState::default();
        let facts = minimal_facts(ui_state_json(&state, None));
        let boxes = boxes_for(&bin, &facts, &dir);
        let r = *boxes
            .get("#searchInput")
            .unwrap_or_else(|| panic!("#searchInput must be laid out; boxes: {:?}", boxes.keys().collect::<Vec<_>>()));
        assert!(r.width() > 0.0 && r.height() > 0.0, "#searchInput has no area");
        assert_eq!(
            hit_in(&boxes, r.center().x, r.center().y),
            Some("#searchInput"),
            "the centre of the search box must resolve to the input"
        );
    }

    /// give: it opens the overlay/section each control lives in, feeds the REAL
    /// renderer the host's facts, then hit-tests the centre of the box the
    /// renderer actually laid out. A typo in a `CONTROLS` selector, a hidden
    /// element that never gets a box, or a `from_selector` gap fails here.
    #[test]
    fn every_newly_wired_control_has_a_real_box_and_resolves_its_action() {
        let Some(bin) = shell_bin() else {
            eprintln!("nzk_ui: skip (nzk_ui not built)");
            return;
        };
        let dir = work_dir("new-controls");

        let previous = presentation();
        set_presentation(Presentation {
            pane_open: true,
            side_scroll: 0.0,
            side_scroll_max: 0.0,
        });

        let mut state = NzkUiState::default();
        // Open every sidebar section the new controls live in. An EMPTY list is
        // sent as `null` ("leave the shipped three collapsed"), so a single
        // unknown id is the way to say "the complete collapsed set is nothing
        // real": nzk_ui hides only ids it finds.
        state.collapsed = vec!["#never-collapsed".to_string()];
        state.open_edit();
        state.indexing = Some("Indexing /tmp/corpus …".to_string());
        state.chain = Some(serde_json::json!({
            "ok": true,
            "flat": [{"depth": 1, "node": {"id": "a", "label": "A"}}],
            "truncated": false,
            "tree": {"node": {"id": "root", "label": "Root"}, "children": []},
        }));
        state.pub_total = Some(1);
        state.pub_list = Some(vec![crate::model::PubRow {
            id: "a".to_string(),
            full: "N.A".to_string(),
            label: "A".to_string(),
            kind: "method".to_string(),
        }]);

        let facts = minimal_facts(ui_state_json(&state, None));
        let boxes = boxes_for(&bin, &facts, &dir);

        let expected: [(&str, NzkUiAction); 11] = [
            ("#rootInput", NzkUiAction::FormOpen("#rootInput")),
            ("#searchInput", NzkUiAction::FocusSearch),
            ("#editTool", NzkUiAction::FormOpen("#editTool")),
            (
                "#editValidate",
                NzkUiAction::EditAnalyze(crate::edit_wire::AnalyzeOp::ParseCheck),
            ),
            ("#sourceTheme", NzkUiAction::FormOpen("#sourceTheme")),
            ("#stopIndexBtn", NzkUiAction::StopIndex),
            ("#pubSearch", NzkUiAction::FormOpen("#pubSearch")),
            ("#strSearch", NzkUiAction::FormOpen("#strSearch")),
            ("#chainDepth", NzkUiAction::FormOpen("#chainDepth")),
            ("#chainRun", NzkUiAction::ChainRun),
            ("#chainFlatBtn", NzkUiAction::ChainFlat),
        ];

        for (selector, want) in expected {
            let rect = boxes.get(selector).copied().unwrap_or_else(|| {
                panic!(
                    "{selector} must be laid out by nzk_ui; boxes present: {:?}",
                    boxes.keys().collect::<Vec<_>>()
                )
            });
            assert!(
                rect.width() > 0.0 && rect.height() > 0.0,
                "{selector} must have a real box, got {}x{}",
                rect.width(),
                rect.height()
            );
            let hit = hit_in(&boxes, rect.center().x, rect.center().y)
                .unwrap_or_else(|| panic!("{selector} centre must hit a registered control"));
            assert_eq!(
                hit, selector,
                "{selector} centre resolved to {hit}, not the control itself"
            );
            assert_eq!(NzkUiAction::from_selector(hit), want, "{selector}");
        }

        set_presentation(previous);
    }

    /// Audit: every registered control must have a non-zero box in at least one
    /// real shell layout (the default frame, an open drawer, an open modal, a
    /// picker, or the loading/source state). A control that never gets a box is
    /// painted nowhere and can never be pressed — the dead-control class this
    /// test exists to catch across the WHOLE registry, not just the r40 subset.
    #[test]
    fn every_registered_control_gets_a_real_box_in_some_surface() {
        let Some(bin) = shell_bin() else {
            eprintln!("nzk_ui: skip (nzk_ui not built)");
            return;
        };
        use crate::model::{Edge, Graph, Node, ProjectRow};

        let graph = Graph {
            repo: PathBuf::from("/tmp/control-audit"),
            nodes: vec![Node {
                id: "n1".to_string(),
                label: "A".to_string(),
                full: "N.A".to_string(),
                kind: "method".to_string(),
                ns: "N".to_string(),
                owner: "N".to_string(),
                file: "A.cs".to_string(),
                line: 1,
                end_line: 3,
                degree: 1,
                external: false,
                ..Default::default()
            }],
            edges: vec![Edge {
                from: 0,
                to: 0,
                kind: "calls".to_string(),
                count: 1,
            }],
            state: serde_json::json!({"counts": {"nodes": 1, "edges": 1}}),
            skipped: Vec::new(),
            projects: vec![ProjectRow::default()],
        };

        let base = || {
            let mut s = NzkUiState::default();
            // An unknown id means "the complete collapsed set is nothing real",
            // so every section is expanded and its controls have boxes.
            s.collapsed = vec!["#never-collapsed".to_string()];
            s.indexing = Some("Indexing /tmp/corpus …".to_string());
            s.selected_node = Some(0);
            s.pub_total = Some(1);
            s.pub_list = Some(vec![crate::model::PubRow {
                id: "n1".to_string(),
                full: "N.A".to_string(),
                label: "A".to_string(),
                kind: "method".to_string(),
            }]);
            s.str_searched = Some(("A".to_string(), 1));
            s.str_rows = Some(vec![crate::model::PubRow {
                id: "n1".to_string(),
                full: "N.A".to_string(),
                label: "A".to_string(),
                kind: "method".to_string(),
            }]);
            s.chain = Some(serde_json::json!({
                "ok": true,
                "flat": [{"depth": 1, "node": {"id": "n1", "label": "A"}}],
                "truncated": false,
                "tree": {"node": {"id": "n1", "label": "A"}, "children": []},
            }));
            s.chain_flat = false;
            s
        };

        let previous = presentation();
        set_presentation(Presentation {
            pane_open: true,
            side_scroll: 0.0,
            side_scroll_max: 0.0,
        });

        let dir = work_dir("all-controls");
        let mut seen: BTreeMap<&'static str, Rect> = BTreeMap::new();
        let mut probe = |state: &NzkUiState| {
            let mut facts = minimal_facts(ui_state_json(state, Some(&graph)));
            // The Declared-types section is v3-only and runtime-created from
            // `type_defs`; the production facts always carry it (the resident
            // graph has every `ret`), so the audit must too or `#typeSearch`
            // would be reported as a control with no box.
            facts["type_defs"] = serde_json::json!({
                "total": 1,
                "types": [{"name": "void", "count": 1, "selected": false}],
                "rows": [{"id": "n1", "full": "N.A", "label": "A", "kind": "void"}],
                "query": "",
                "scope": "N"
            });
            let boxes = boxes_for(&bin, &facts, &dir);
            for c in CONTROLS {
                if let Some(r) = boxes.get(c.selector) {
                    if r.width() > 0.0 && r.height() > 0.0 {
                        seen.insert(c.selector, *r);
                    }
                }
            }
        };

        // Default + every drawer.
        for drawer in [None, Some("nodeDrawer"), Some("findPanel"), Some("blocksPanel")] {
            let mut s = base();
            s.drawer_open = drawer;
            probe(&s);
        }
        // Every modal (the edit path seeds its controls through `open_edit`).
        for surface in [
            HostSurface::Shortcuts,
            HostSurface::Preview,
            HostSurface::Export,
        ] {
            let mut s = base();
            s.surface = Some(surface);
            if surface == HostSurface::Export {
                // `syncRows` hides the chunk-unit/grouping rows in single-file
                // mode; they are real controls only in split mode, so the audit
                // must open the state a user can actually reach them in.
                s.reorg.request.mode = crate::blocks_peek::ReorgMode::Split;
            }
            probe(&s);
        }
        let mut edit = base();
        edit.open_edit();
        probe(&edit);
        // The folder picker has a real listing (the modal body's rows).
        let mut picker = base();
        picker.open_picker(&PathBuf::from("/tmp"), "");
        probe(&picker);

        set_presentation(previous);

        let missing: Vec<&str> = CONTROLS
            .iter()
            .filter(|c| !seen.contains_key(c.selector))
            .map(|c| c.selector)
            .collect();
        assert!(
            missing.is_empty(),
            "controls with no real box in any opened surface: {missing:?}"
        );

        // A box is necessary but not sufficient: the same selector must also
        // resolve a real owner action. This is the registry-wide form of the
        // anti-drift contract, now checked against geometry the shell actually
        // laid out rather than only against the table.
        for c in CONTROLS {
            match c.owner {
                ControlOwner::Host | ControlOwner::Section | ControlOwner::Local => assert!(
                    NzkUiAction::from_selector(c.selector).is_effective(),
                    "{} has a box but resolves to a no-op",
                    c.selector
                ),
                ControlOwner::Picker => assert!(
                    PickerAction::from_selector(c.selector).is_some()
                        || c.selector == "#modalGo",
                    "{} has a box but no picker action",
                    c.selector
                ),
                ControlOwner::Slider => assert!(
                    SliderKind::from_selector(c.selector).is_some(),
                    "{} has a box but is not a known slider",
                    c.selector
                ),
                ControlOwner::Inert => assert_eq!(
                    NzkUiAction::from_selector(c.selector),
                    NzkUiAction::None,
                    "{} is a backdrop and must stay inert",
                    c.selector
                ),
            }
        }
    }
}

#[cfg(test)]
mod type_browser_tests {
    use super::*;
    use crate::actions::{set_type_filter, sync_search_edits, Effect};

    fn decl(id: &str, label: &str, ns: &str, ret: &str) -> crate::model::Node {
        crate::model::Node {
            id: id.into(),
            label: label.into(),
            full: format!("{ns}.{label}"),
            kind: "method".into(),
            ns: ns.into(),
            ret: ret.into(),
            file: "A.cs".into(),
            line: 1,
            ..Default::default()
        }
    }

    fn graph() -> Graph {
        Graph {
            repo: std::path::PathBuf::from("/tmp/type-browser"),
            nodes: vec![
                decl("m:Name", "Name", "A", "string"),
                decl("m:Path", "Path", "A.Sub", "string"),
                decl("m:Live", "Live", "A", "bool"),
                decl("m:Count", "Count", "B", "int"),
                decl("m:Num", "Num", "A", "System.Int32"),
                // No declared type: never a chip and never a row.
                crate::model::Node {
                    id: "t:A".into(),
                    label: "A".into(),
                    kind: "class".into(),
                    ns: "A".into(),
                    ..Default::default()
                },
            ],
            edges: Vec::new(),
            state: serde_json::json!({"indexed": true}),
            skipped: Vec::new(),
            projects: Vec::new(),
        }
    }

    fn state(ns: &str, deep: bool, query: &str, filter: Option<&str>) -> NzkUiState {
        let mut s = NzkUiState::default();
        s.filters.namespace = ns.into();
        s.filters.ns_deep = deep;
        s.type_query = query.into();
        s.type_filter = filter.map(str::to_string);
        s
    }

    /// "All declared types under one namespace" is the chips list: every type
    /// actually declared there, with its real count, and nothing from another
    /// namespace. `Include sub-namespaces` follows the same predicate the canvas
    /// uses.
    #[test]
    fn chips_list_the_declared_types_under_the_selected_namespace() {
        let g = graph();
        let f = type_defs_facts(&g, &state("A", false, "", None));
        let names: Vec<&str> = f["types"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["System.Int32", "bool", "string"], "count desc, then name");
        assert_eq!(f["types"][0]["count"], 1);
        assert_eq!(f["total"], 3, "A declares three typed members");

        let deep = type_defs_facts(&g, &state("A", true, "", None));
        let deep_total = deep["total"].as_u64().unwrap();
        assert_eq!(deep_total, 4, "the sub-namespace member is included");
        assert!(
            deep["types"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "string" && t["count"] == 2)
        );
        // Namespace B is a sibling, never a child.
        let b = type_defs_facts(&g, &state("A", true, "", None));
        assert!(!b["rows"].as_array().unwrap().iter().any(|r| r["label"] == "Count"));
    }

    /// "But just of that declared type": a selected chip restricts the rows to
    /// exactly that type, marks the chip, and is the value the shell paints.
    #[test]
    fn a_selected_type_restricts_the_rows_to_exactly_that_type() {
        let g = graph();
        let f = type_defs_facts(&g, &state("A", false, "", Some("string")));
        assert_eq!(f["total"], 1);
        let rows = f["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["label"], "Name");
        assert_eq!(rows[0]["kind"], "string");
        let chip = f["types"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "string")
            .expect("string chip");
        assert_eq!(chip["selected"], true);
        let other = f["types"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "bool")
            .expect("bool chip");
        assert_eq!(other["selected"], false);
    }

    /// Fuzzy type search: `str` matches `string` and `System.String` (a
    /// subsequence, not a substring), narrows both the chips and the rows, and
    /// is case-insensitive.
    #[test]
    fn the_query_fuzzily_matches_the_declared_type() {
        let g = graph();
        let f = type_defs_facts(&g, &state("", false, "stri", None));
        let names: Vec<&str> = f["types"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["string"], "only `string` is a `stri` subsequence");
        assert_eq!(f["total"], 2, "both string members, across namespaces");

        let i32q = type_defs_facts(&g, &state("", false, "int", None));
        assert!(
            i32q["types"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["name"] == "System.Int32"),
            "`int` fuzzily reaches System.Int32"
        );
    }

    /// The query also matches the declaration name, and tokens are ANDed.
    #[test]
    fn the_query_matches_names_and_ands_its_tokens() {
        let g = graph();
        let by_name = type_defs_facts(&g, &state("", false, "path", None));
        assert_eq!(by_name["total"], 1);
        assert_eq!(by_name["rows"][0]["label"], "Path");

        let both = type_defs_facts(&g, &state("", false, "str name", None));
        assert_eq!(both["total"], 1, "`str` AND `name` -> Name only");
        assert_eq!(both["rows"][0]["label"], "Name");
    }

    /// A `.type-chip` press resolves to the type name after `type:` — the `all`
    /// chip to the empty string, which is a real value (clear), not `None`.
    #[test]
    fn a_chip_press_names_its_type_and_all_chip_is_empty() {
        let chain = vec![
            HitNode::shell_row("10", None, Some("tname"), None, None),
            HitNode::shell_row("11", None, Some("type-chip"), Some("type:string"), None),
            HitNode::shell_row("12", Some("typeChips"), Some("chips type-chips"), None, None),
        ];
        assert_eq!(type_chip_click(&chain), Some("string".to_string()));

        let all = vec![
            HitNode::shell_row("20", None, Some("type-chip active"), Some("type:"), None),
            HitNode::shell_row("21", Some("typeChips"), None, None, None),
        ];
        assert_eq!(type_chip_click(&all), Some(String::new()));

        // Outside `#typeChips` a `.lrow`/other chip is never claimed.
        let foreign = vec![HitNode::shell_row("30", None, Some("type-chip"), Some("type:x"), None)];
        assert_eq!(type_chip_click(&foreign), None);
    }

    /// A type row is a normal `.lrow` and is claimed by the list-row resolver,
    /// so a press selects the declaration through the same path the public
    /// definitions list uses.
    #[test]
    fn a_type_row_press_selects_its_declaration() {
        let chain = vec![
            HitNode::shell_row("40", None, Some("l-label"), None, None),
            HitNode::shell_row("41", None, Some("lrow"), Some("m:Name"), None),
            HitNode::shell_row("42", Some("typeList"), Some("listbox tall"), None, None),
        ];
        assert_eq!(list_row_click(&chain), Some("m:Name".to_string()));
    }

    /// Typing clears an exact chip restriction, so the visible query and the
    /// rows cannot be the intersection of two filters.
    #[test]
    fn typing_clears_the_chip_restriction() {
        let mut st = NzkUiState::default();
        st.form.set_value("#typeSearch", "str");
        set_type_filter(&mut st, "string");
        assert_eq!(st.type_filter.as_deref(), Some("string"));
        assert_eq!(st.type_query, "string");
        // A new keystroke is what `sync_search_edits` sees: the form value wins.
        st.form.set_value("#typeSearch", "str");
        let mut effect = Effect::default();
        sync_search_edits(&mut st, &mut effect, false);
        assert_eq!(st.type_query, "str");
        assert_eq!(st.type_filter, None, "typing drops the exact restriction");
    }
}

#[cfg(test)]
mod title_tooltip_paint_tests {
    use super::*;

    fn all_text(shape: &egui::Shape, out: &mut Vec<String>) {
        match shape {
            egui::Shape::Text(t) => out.push(t.galley.text().to_string()),
            egui::Shape::Vec(v) => {
                for s in v {
                    all_text(s, out);
                }
            }
            _ => {}
        }
    }

    /// The hover tooltip is really painted, with the `title` string the shell
    /// reported — not merely decoded into the chain. This is the half a unit
    /// test of `hit_chain_from_tsv` cannot cover: the pixels a browser would
    /// have drawn and v3 used to drop entirely.
    #[test]
    fn the_hover_tooltip_paints_the_reported_title() {
        let pos = egui::pos2(700.0, 300.0);
        HOVER.with(|slot| {
            *slot.borrow_mut() = Some(HoverProbe {
                pos: (pos.x, pos.y),
                chain: vec![HitNode::class("flag").with_title(Some(
                    "Degree — the number of distinct graph neighbours. This one has 7.",
                ))],
                rect: None,
            });
        });
        let ctx = egui::Context::default();
        let geo = FrameGeometry::unverified(std::collections::BTreeMap::new(), (1600, 1000));
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1600.0, 1000.0),
                )),
                events: vec![egui::Event::PointerMoved(pos)],
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    paint_feedback(ui, &geo);
                });
            },
        );
        out.textures_delta.clear();
        let mut texts = Vec::new();
        for cs in &out.shapes {
            all_text(&cs.shape, &mut texts);
        }
        assert!(
            texts.iter().any(|t| t.contains("Degree") && t.contains('7')),
            "the title must be painted as a tooltip; painted text: {texts:?}"
        );
        HOVER.with(|slot| *slot.borrow_mut() = None);
    }

    /// No `title` under the pointer ⇒ no tooltip box and no invented text.
    #[test]
    fn a_chain_without_a_title_paints_no_tooltip() {
        let pos = egui::pos2(700.0, 300.0);
        HOVER.with(|slot| {
            *slot.borrow_mut() = Some(HoverProbe {
                pos: (pos.x, pos.y),
                chain: vec![HitNode::class("flag")],
                rect: None,
            });
        });
        let ctx = egui::Context::default();
        let geo = FrameGeometry::unverified(std::collections::BTreeMap::new(), (1600, 1000));
        let mut out = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1600.0, 1000.0),
                )),
                events: vec![egui::Event::PointerMoved(pos)],
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    paint_feedback(ui, &geo);
                });
            },
        );
        out.textures_delta.clear();
        let mut texts = Vec::new();
        for cs in &out.shapes {
            all_text(&cs.shape, &mut texts);
        }
        assert!(
            texts.iter().all(|t| !t.contains("Degree")),
            "no title means no tooltip: {texts:?}"
        );
        HOVER.with(|slot| *slot.borrow_mut() = None);
    }
}

#[cfg(test)]
mod hit_title_wire_tests {
    use super::*;

    /// The 13th `@hit` column (the nearest `title`) reaches the chain, and an
    /// OLDER child that stops at the ancestor trail still decodes exactly as
    /// before — the column is additive, never required.
    #[test]
    fn the_title_column_is_optional_and_additive() {
        // x, y, node, tag, id, class, key, close, da-id, nav, ancestors, title.
        // Built field-by-field so the test cannot drift by a tab.
        let row = |ancestors: &str, title: Option<&str>| {
            let mut f = vec![
                "10.00", "20.00", "42", "span", "", "flag", "", "", "", "", ancestors,
            ];
            if let Some(t) = title {
                f.push(t);
            }
            format!("@hit\t{}\n", f.join("\t"))
        };

        let with = hit_chain_from_tsv(&row("", Some("Degree — N neighbours")));
        assert_eq!(with.len(), 1, "the title is not an ancestor: {with:?}");
        assert_eq!(with[0].title.as_deref(), Some("Degree — N neighbours"));
        assert_eq!(with[0].class.as_deref(), Some("flag"));

        // No 13th column: every earlier field keeps its index.
        let old = hit_chain_from_tsv(&row("", None));
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].title, None);
        assert_eq!(old[0].class.as_deref(), Some("flag"));

        // An empty trailing column is `None`, not an empty tooltip.
        let empty = hit_chain_from_tsv(&row("", Some("")));
        assert_eq!(empty[0].title, None);

        // A real ancestor trail and a title still decode independently.
        let both = hit_chain_from_tsv(&row("typeChips@chips", Some("all declared types")));
        assert_eq!(both.len(), 2);
        assert_eq!(both[0].title.as_deref(), Some("all declared types"));
        assert_eq!(both[1].id.as_deref(), Some("typeChips"));
        assert_eq!(both[1].title, None);
    }
}
