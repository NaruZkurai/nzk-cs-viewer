//! Host-side action seam for the nzk_ui dispatch table (task-26).
//!
//! **Display-free by construction**: nothing in this file mentions `egui`, a
//! window, or a GPU, so the behaviour of every action is unit-testable with no
//! display. The nzk_ui (which is a static rasteriser) resolves a click to a
//! `NzkUiAction`; the host calls [`apply`] to mutate [`NzkUiState`] and gets
//! back an [`Effect`] describing what it must do (push the camera into the view,
//! rebuild the visible set, re-run the layout, re-index, repaint the nzk_ui).
//!
//! **One path for nzk_ui hits and native bindings.** `app.rs` dispatches
//! keyboard bindings through exactly the same [`apply`] call, so a `#zoomInBtn`
//! click and a `+` key press cannot drift apart.
//!
//! ### Deliberately not implemented
//!
//! * `#backBtn` / `#upBtn` — **no longer in this list.** This paragraph used to
//!   read "the native view has no navigation-depth or history model to act on:
//!   `GraphView::pick` only *selects*, `ViewMode` is a flat `Namespaces | Full`,
//!   and there is no expanded-namespace stack". Task-2 built exactly that model,
//!   so the justification is gone and the controls are wired:
//!   [`NzkUiAction::NavigateBack`]/[`NavigateUp`] set
//!   [`Effect::navigate_back`]/[`navigate_up`], and `app.rs` consumes them
//!   against `GraphView::navigate_back()`/`navigate_up()` — a real 80-entry
//!   selection history (parity: `interaction.js selectNode`) and a real
//!   `contains`-parent walk, not a guessed "collapse the most recent".
//!   A `None` from either is the honest answer for an empty history or a root
//!   node, and the view then leaves the selection untouched.
//!
//!   The lesson is worth keeping: this module's docs are a claim about the
//!   *state of the world*, so when the world changes they must change with it.
//!   A doc that says "this is impossible" long after it became possible is the
//!   same defect class as a control that reports success without acting.
//! * `#indexBtn` is implemented as a *request*: this module is display-free and
//!   does not own the engine, so it bumps `reindex_count` and returns
//!   `Effect.reindex`; `app.rs` performs the actual `Index` rebuild.
//! * `#srcPrevBtn`/`#srcNextBtn` — wired to the pane's real **document
//!   history**. The earlier revision of this doc claimed the model did not
//!   exist, but `SourceState` has carried `history`/`forward` stacks (and
//!   `Alt+Left`/`Alt+Right` have stepped them) since the source pane landed.
//!   The buttons are the same transition through the nzk_ui dispatch table, not
//!   a second, guessed list: [`NzkUiAction::SrcPrev`]/[`SrcNext`] request a
//!   step and the host calls `SourceState::history_back`/`history_forward`.
//!
//! ### Clipboard
//!
//! `#srcCopyBtn` **is** wired, because a real clipboard write exists:
//! `egui::Context::copy_text` (`vendor/egui/src/context.rs:1995`, emitting
//! `OutputCommand::CopyText`) and this app enables `egui-winit`'s `clipboard`
//! feature (`app/Cargo.toml:19`). This module stays display-free by *requesting*
//! the copy ([`Effect::copy_source`]) and letting `app.rs`, which owns the
//! `egui::Context`, perform it.

use std::path::{Path, PathBuf};

use crate::picker::Listing;
use crate::graphviz::{
    CURVE_MAX, CURVE_MIN, ITER_MAX, ITER_MIN, NODE_SIZE_MAX, NODE_SIZE_MIN, SPACING_MAX,
    SPACING_MIN, TEXT_SIZE_MAX, TEXT_SIZE_MIN,
};

/// One document-tab operation — the payload of [`NzkUiAction::Tab`].
///
/// The four operations are the four `window.CsTabs` calls the reference's key
/// block makes (`shell.js:410-431`), kept as one enum so `NzkUiAction` gains a
/// single variant instead of four near-identical ones. The model itself is
/// [`crate::chords::tabs::Tabs`].
///
/// Round 7 adds the two **keyed** commands the runtime tab bar's controls issue
/// (`shell.js:1250-1266`: `.cs-tab-x[data-close]` closes its row, a
/// `.cs-tab[data-key]` press activates its row). They carry a payload, so the
/// enum is `Clone` rather than `Copy`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TabCmd {
    /// `Ctrl+Tab` / `Ctrl+Shift+Tab` — `CsTabs.cycle(shiftKey ? -1 : 1)`.
    Cycle(isize),
    /// `Alt+W` — `CsTabs.closeActive()`.
    CloseActive,
    /// `Alt+Shift+W` — `CsTabs.closeOthers()`.
    CloseOthers,
    /// `Alt+S` — `CsTabs.toggleSplit()`.
    ToggleSplit,
    /// A press on `.cs-tab[data-key]` — the reference's `activateByKey(key)`
    /// (`shell.js:1259-1260`, `:1269-1272`).
    Activate(String),
    /// A press on `.cs-tab-x[data-close]` — the reference's
    /// `closeTab(key)` (`shell.js:1252-1253`).
    Close(String),
}

/// The actions the nzk_ui can request. Small on purpose; an unknown selector
/// maps to `None` and is a no-op.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NzkUiAction {
    /// Inert control or unknown selector: the host must do nothing.
    None,
    /// `#newWinBtn` — open the selected document in its own window
    /// (`windows.js:172 openDoc`, the `⧉ New window` control). The host owns the
    /// window registry, so only it can act on this; the display-free action
    /// layer just names the request.
    NewDocWindow,
    /// One sidebar filter operation (`filters.js`): a kind/edge chip toggle,
    /// the namespace select, reset, or prune. Mutates [`NzkUiState::filters`];
    /// the host applies the model to the view and the nzk_ui facts.
    Filter(FilterCmd),
    /// Fit the visible graph into the canvas (`#fitBtn`, `F`).
    Fit,
    /// Zoom the camera in one step (`#zoomInBtn`, `+`/`=`).
    ZoomIn,
    /// Zoom the camera out one step (`#zoomOutBtn`, `-`).
    ZoomOut,
    /// Show the whole model, not just namespaces (`#wholeGraphBtn`, `0`/`Home`).
    FullGraph,
    /// Re-run the layout ("Re-settle", `#resettleBtn`, `R`).
    Resettle,
    /// Re-index the corpus (`#indexBtn`, `I`).
    Index,
    /// Toggle a nzk_ui sidebar section's collapsed state (the section DOM ids
    /// `#filtersHead`/`#pubHead`/`#strHead`/`#chainHead`/`#problemsHead`, plus
    /// `#relationsHead`). The id is the selector the nzk_ui already reports, so
    /// there is exactly one owner of `NzkUiState::collapsed`.
    ToggleSection(&'static str),
    /// Open/close the Blocks pane (the real multi-window pane). Named for what
    /// it does — it is not the original "Blocks" editor mode. `#blocksBtn`
    /// toggles it; `#blocksClose` closes it (`b` toggles, Esc closes).
    OpenBlocks,
    /// Walk back to the previously selected node (`#backBtn`). The view owns the
    /// history stack; the host only asks for the step.
    NavigateBack,
    /// Step forward through the selection history again — `Alt+Right`
    /// (`nav.js:441`), the mirror of [`NzkUiAction::NavigateBack`].
    ///
    /// A separate action rather than a payload on Back: the reference binds the
    /// two keys separately with separate guards (`Alt+Left`/`Alt+Right`, both
    /// requiring every other modifier absent), and there is no `#forwardBtn` to
    /// share a press with, so a payload would only invent a second caller.
    NavigateForward,
    /// Walk up to the containment parent — namespace / declaring type
    /// (`#upBtn`).
    NavigateUp,
    /// Show the graph (`#modeGraph`). The counterpart of Find.
    ModeGraph,
    /// Show Find (`#modeFind`) — the search-results surface instead of the node
    /// drawer.
    ModeFind,
    /// Focus the topbar search box — the `/` chord (`shell.js:452-455`).
    ///
    /// The reference's `/` arm does two things in one breath:
    /// `setMode('graph', false)` and then `searchInputEl.focus()` +
    /// `.select()`. The first half is leaving Find, which this side models as
    /// clearing [`NzkUiState::surface`] / [`NzkUiState::drawer_open`]; the
    /// second is ownership of the keyboard, which this side models as
    /// [`NzkUiState::search_focused`]. The two are set together here so the
    /// chord cannot leave the app in "find panel open *and* search box focused",
    /// which the reference cannot express either.
    FocusSearch,
    /// One document-tab operation (`shell.js:410-431`), applied to
    /// [`NzkUiState::tabs`].
    ///
    /// The state is now **painted**: `app/src/nzk_ui.rs::ui_state_json` publishes
    /// the open stack as the `ui.tabs` facts object, and `nzk_ui::dom`'s
    /// `apply_tabs` renders `#cs-tabs` (created at runtime, exactly where
    /// `shell.js:1194-1197` puts it) with the per-tab active/split marks and the
    /// split/close-others actions. Clicking those bar controls is not routed
    /// back into the app yet — the chords above are the working input path.
    Tab(TabCmd),
    /// `Ctrl+Shift+V|H|D` or `Shift+D` — arrange the marquee selection along an
    /// axis (`arrange.js:289`).
    ///
    /// An effect flag rather than a state change: the nodes that move live in
    /// `GraphView`, and `actions.rs` is display-free.
    Arrange(crate::chords::Axis),
    /// `Shift+=` / `Shift+-` — widen or tighten the spacing along the axis the
    /// selection is already laid out on (`arrange.js:302`).
    Spacing(i32),
    /// Pick the folder to index (`#browseBtn`). Opens the shell's own folder
    /// picker modal (`#modal`); this module is display-free and only requests it.
    Browse,
    /// Export the current model (`#exportBtn`). A UI surface the host opens.
    Export,
    /// Open the editor (`#editBtn`).
    Edit,
    /// Press one of the editor's own run buttons — `#editDry` or `#editApply`
    /// (`edit.js:276-277`).
    ///
    /// A press is not a mode: `#editDry` asks for a dry run and `#editApply` for
    /// a writing one, and the two are the *same* gesture on the same pair of
    /// buttons. Carrying the mode as a payload is what lets this enum stay the
    /// one place a press is turned into a request — and it is the ONLY reason
    /// `actions.rs` can reach `press_apply` at all, because `RunMode` is the
    /// argument that policy takes.
    ///
    /// This arm deliberately does **not** build a request body. It calls
    /// [`crate::edit_wire::EditToolView::press_apply`] /
    /// [`crate::edit_wire::EditToolView::press_dry`], which own the busy /
    /// no-tool / args-parse guards, so a press cannot skip them.
    EditRun(crate::edit_args::RunMode),
    /// One press on the export dialog's foot buttons (`#reorgVerify`,
    /// `#reorgZip`, `#reorgWrite`, `#reorgCompile`) — `reorg.js:160-200`.
    ///
    /// Carries the engine action, like [`NzkUiAction::EditRun`] carries the run
    /// mode: the four buttons are four different `/api/reorg` actions. The host
    /// owns the `Index`, so this arm only records the request on
    /// [`NzkUiState::reorg`]; `app.rs` runs it in process.
    ReorgRun(crate::reorg_wire::Action),
    /// A press on a form control (`#reorgMode`, `#reorgHeader`, `#reorgDest`,
    /// `#previewTo`, `#modalPathInput`, …). The payload is the control's DOM id,
    /// so `CONTROLS`/`resolve_selector` stay the single owner of "what is a form
    /// control" and this arm cannot invent a second list. `apply` hands it to
    /// [`crate::form_wire::FormState`], the pure state machine.
    FormOpen(&'static str),
    /// One key while a form control owns the keyboard (menu open or input
    /// focused). Routed from `app.rs`'s key path so the canvas keymap cannot
    /// also see it.
    FormKey(crate::form_wire::Key),
    /// A press on one of an open select menu's option rows. The payload is the
    /// row index; the commit is the SAME `FormState::set_value` path the
    /// keyboard Enter uses.
    FormMenuCommit(usize),
    /// Open the preview pane (`#previewBtn`).
    Preview,
    /// A press on the drawer's `{ } View code` header button (`#nodeCodeBtn`).
    ///
    /// v2 wires it at `shell.js:563` to `viewCodeFor(state.selected)` →
    /// `openSource({ node: id })` (`drawer.js:29-32`). The host owns the source
    /// pane, so this arm only records the request on [`Effect::source_open`];
    /// `app.rs` opens the SELECTED declaration via `App::preopen_source` (the
    /// round-9 host path, not a new engine call). No selection = no request.
    SourceOpen,
    /// A press on `#previewRun` (or Enter in `#previewTo`) — `preview.js:188-199`.
    ///
    /// Like [`NzkUiAction::EditRun`], this is the *decision* only: the request is
    /// built from the Host-owned form values (`#previewSelector`/`#previewOp`/
    /// `#previewTo`) and the engine call happens in `app.rs`, which owns the
    /// `Index`. A flag (`Effect::preview_run`) rather than a payload because the
    /// values live on [`NzkUiState::form`], which the host already holds.
    PreviewRun,
    /// Show the shortcuts/settings sheet (`#shortcutsBtn`, `?`).
    Shortcuts,
    /// Focus the namespace filter (`#nsSelect`) — selecting a value is the
    /// shell's own control; this is the press/activation.
    FocusNamespaceFilter,
    /// Open or close one overlay **drawer** — the name is its DOM id:
    /// `nodeDrawer`, `findPanel` or `blocksPanel`.
    ///
    /// This is the actions.rs half of the host-owned drawer open state. The
    /// other half is `nzk_ui::dom::UiState::drawers_open`, which clears the
    /// `hidden` class so the real stylesheet lays the panel out and gives it a
    /// non-zero, hit-testable box. While a drawer is hidden it has NO box at
    /// all (`0 0 0 0`), so nothing can hit it — being in the nzk_ui's probe list
    /// is not the same as being reachable.
    ///
    /// The `class="modal-backdrop"` family has its own channel,
    /// [`NzkUiAction::ToggleModal`]; the two clear each other so only one
    /// overlay is ever up.
    ToggleDrawer(&'static str),
    /// Open or close one overlay **modal** — the name is its DOM id in the
    /// `class="modal-backdrop"` family: `shortcutsModal`, `editModal`,
    /// `previewModal`, `reorgModal` or `modal`.
    ///
    /// The mirror of [`NzkUiAction::ToggleDrawer`] on the modal family. The
    /// field it writes, [`NzkUiState::surface`], is serialised into
    /// `nzk_ui::dom::UiState::modals_open`, whose allow-list clears the
    /// shipped `hidden` class so the real `.modal-backdrop` rules lay the dialog
    /// out. Unlike a drawer an open modal covers the whole viewport (its
    /// backdrop is `position: fixed; inset: 0`), which is why it also swallows
    /// every hit outside its card and why at most one may be open.
    ToggleModal(HostSurface),
    /// Close the source pane (`#srcCloseBtn`) — the pane's own head "×".
    ///
    /// The behaviour already exists as `source_pane::SourceState::close()`
    /// (source_pane.rs:455, sets `open = false`), and `app.rs` gates the pane's
    /// panel on exactly that flag. This action is the dispatch-table route to
    /// it, so the painted ✕ is the same code path as the in-pane button.
    SrcClose,
    /// Cycle the source pane's colour theme (`#srcModeBtn`) through the
    /// [`SourceMode`]s that genuinely exist in `source_pane::SourceTheme`.
    SrcMode,
    /// Pop the source pane out into its own OS window / dock it back
    /// (`#srcPopBtn`). A *toggle*: the host reads the pane's current window
    /// state and does the opposite, so one control does both.
    SrcPop,
    /// Copy the synthesized document to the system clipboard (`#srcCopyBtn`).
    ///
    /// A real clipboard write, not a no-op: `egui::Context::copy_text` exists in
    /// egui 0.37 (`context.rs:1995`, emitting `OutputCommand::CopyText`) and this
    /// app builds `egui-winit` with the `clipboard` feature (app/Cargo.toml:19),
    /// so the command reaches the OS clipboard. This module is display-free, so
    /// it only *requests* the copy via [`Effect::copy_source`]; `app.rs` holds
    /// the `egui::Context` and performs it.
    SrcCopy,
    /// Step the source pane's document history back (`#srcPrevBtn`).
    ///
    /// The pane DOES own an ordered set of documents: `SourceState::history`
    /// records every document it replaced, exactly as `source.js` did, and
    /// `Alt+Left` already steps it. The head button is the same transition, so
    /// it is a request ([`Effect::source_history`]) the host completes against
    /// the pane it owns.
    SrcPrev,
    /// Step the source pane's document history forward (`#srcNextBtn`).
    SrcNext,
    /// Copy the open document's `file:line` reference (`#srcRefBtn`,
    /// `source.js:546-550`).
    SrcRef,
    /// Re-open the whole declaration for the current document
    /// (`#srcMethodBtn`, `source.js:478-482`).
    SrcMethod,
    /// Open the current document in a second OS window (`#srcWindowBtn`,
    /// `shell.js:680-683`).
    SrcWindow,
    /// Stop an in-flight index (`#stopIndexBtn`, Esc while it runs). A request:
    /// the host owns the worker and its cancel flag.
    StopIndex,
    /// Run one `/api/analyze` op from the Edit dialog (`#editValidate` /
    /// `#editDedupe`). The request is parsed from the args box here (pure); the
    /// host performs the engine call.
    EditAnalyze(crate::edit_wire::AnalyzeOp),
    /// Run the resolve chain for the selected node (`#chainRun`) through the
    /// real engine `Index::chain`; the host performs it.
    ChainRun,
    /// Toggle the chain's flat list (`#chainFlatBtn`).
    ChainFlat,
}

/// One message from the folder-picker modal (`#modal`) into the host.
///
/// ### Why this is a separate channel from [`NzkUiAction`]
///
/// [`NzkUiAction`] is keyed by a *DOM selector* (`NzkUiAction::from_selector`,
/// :276) and every variant is therefore a stateless press on a fixed element: a
/// `&'static str` selector in, an action out. Four of the picker's controls are
/// like that — `#modalUp`, `#modalHome`, `#modalRoot`, `#modalRepo` are fixed
/// ids — but three are **not**:
///
/// * a `#modalBody` row press carries the row's `data-nav` *path*
///   (`shell.js:58` puts an absolute path in that attribute and `:760` reads it
///   back with `btn.getAttribute('data-nav')`);
/// * `#modalGo` carries the *text* of `#modalPathInput`, and so does Enter in
///   that input (`shell.js:750,753`);
/// * `#modalSelect` carries nothing, but its result — the folder to re-index —
///   is host state, not a flag.
///
/// `NzkUiAction`'s fields are all `Copy` (`HostSurface`, `&'static str`), so
/// there is no honest `NzkUiAction` for a `String` payload without adding a
/// lifetime to the whole enum. This is the seam `sel` in
/// `app.rs:nzk_ui_click_with` already documents as "the modal's own controls
/// need a second pass"; this is that pass, as its own type.
///
/// ### It is still display-free
///
/// Nothing here names `egui`, a window or a GPU. The host turns one of these
/// into a [`crate::find_wire::PickerView`] navigation (which is real `read_dir`
/// I/O, exactly like the `#indexBtn` engine work `Effect::reindex` defers) or
/// into the committed root. `actions.rs` only routes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PickerAction {
    /// `#modalClose`, `#modalCancel` or a backdrop press: close the modal
    /// (`closeModal`, `shell.js:14-17`).
    ///
    /// Distinct from `NzkUiAction::ToggleModal(HostSurface::FolderPicker)`,
    /// which *toggles*. The backdrop must always close, never open: a press that
    /// misses the card landing on the backdrop and re-opening the dialog is the
    /// bug `shell.js:761` (`if (ev.target === modalEl) closeModal()`) avoids.
    Close,
    /// `#modalUp` — go to the parent folder (or stay put at `/`).
    Up,
    /// `#modalHome` — go to the user's home directory.
    Home,
    /// `#modalRoot` — go to the filesystem root.
    Root,
    /// `#modalRepo` — go back to the folder being indexed
    /// (`browseTo(state.root || '')`, `shell.js:754`).
    Repo,
    /// `#modalGo`, or Enter in `#modalPathInput`: browse the typed path
    /// (`shell.js:750,753`). Empty means the repo root (`browseTo(start || '')`,
    /// `shell.js:96`).
    Go(String),
    /// A `.pick[data-nav]` row press inside `#modalBody`: descend into that
    /// folder (`shell.js:758-761`). The payload is the row's absolute path.
    Open(String),
    /// `#modalSelect` — commit the folder on screen (`chooseCurrentFolder`,
    /// `shell.js:99-106`). The host decides what "commit" means; this module
    /// cannot, and does not pretend to.
    Select,
}

impl PickerAction {
    /// The modal control -> message. `NzkUiAction::from_selector`'s counterpart
    /// for the picker's own family.
    ///
    /// `#modal` (the backdrop) is deliberately **absent**, for the same reason
    /// `#reorgModal`/`#editModal`/`#previewModal`/`#shortcutsModal` are absent
    /// from `NzkUiAction::from_selector` (:316-320): it is `position: fixed;
    /// inset: 0` once open, so every press that misses the card lands on it.
    /// `PickerAction::Close` is reachable from the two real close controls and
    /// from the host's own backdrop hit-test, which is where the "did the press
    /// land on the backdrop?" question belongs — the resolver knows, a selector
    /// table does not.
    ///
    /// `#modalBody` and `#modalPathInput` are also absent: both carry a payload
    /// the host has (the row's `data-nav`, the input's text), so they are
    /// constructed by the host rather than resolved from a fixed id. That is why
    /// this returns `Option` — `None` is "not a picker control", not an error.
    pub fn from_selector(selector: &str) -> Option<PickerAction> {
        match selector {
            "#modalClose" | "#modalCancel" => Some(PickerAction::Close),
            "#modalUp" => Some(PickerAction::Up),
            "#modalHome" => Some(PickerAction::Home),
            "#modalRoot" => Some(PickerAction::Root),
            "#modalRepo" => Some(PickerAction::Repo),
            // No payload of its own: it browses what the input holds, and that
            // text reaches the host as `PickerAction::Go(..)`.
            "#modalSelect" => Some(PickerAction::Select),
            _ => None,
        }
    }

    /// Stable human label (logs, window titles, tests) — `NzkUiAction::label`'s
    /// counterpart.
    pub fn label(&self) -> &'static str {
        match self {
            PickerAction::Close => "picker-close",
            PickerAction::Up => "picker-up",
            PickerAction::Home => "picker-home",
            PickerAction::Root => "picker-root",
            PickerAction::Repo => "picker-repo",
            PickerAction::Go(_) => "picker-go",
            PickerAction::Open(_) => "picker-open",
            PickerAction::Select => "picker-select",
        }
    }

    /// Does this message need the *filesystem* to be answered?
    ///
    /// `Close` and `Select` do not: closing is a state change, and committing a
    /// folder is bookkeeping. The other six all end in `browse_to`, i.e. a real
    /// `read_dir`. Named as a predicate so the host can tell "this press had a
    /// real consequence" from "this press asked for I/O" without matching on the
    /// enum a second time.
    pub fn browses(&self) -> bool {
        !matches!(self, PickerAction::Close | PickerAction::Select)
    }
}

/// The source pane's colour themes, in cycle order.
///
/// These are the three `SourceTheme` constructors that genuinely exist
/// (`source_pane.rs:111` darkplus, `:149` darkmodern, `:168` nzk_dark_plus).
/// Kept here as a tiny display-free enum so `#srcModeBtn` has one owner of the
/// cycle; `app.rs` maps a mode to the concrete `SourceTheme`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SourceMode {
    /// VS Code Dark+ — the shipped editor default (`SourceTheme::darkplus`).
    #[default]
    DarkPlus,
    /// VS Code Dark Modern (`SourceTheme::darkmodern`).
    DarkModern,
    /// The `nzk-dark+` head on the darkplus token palette
    /// (`SourceTheme::nzk_dark_plus`).
    NzkDarkPlus,
    /// Monokai (`SourceTheme::monokai`, `#sourceTheme`'s third option).
    Monokai,
    /// Solarized Dark (`SourceTheme::solarized`, `#sourceTheme`'s fourth).
    Solarized,
}

impl SourceMode {
    /// Every mode, in cycle order.
    pub const ALL: [SourceMode; 5] = [
        SourceMode::DarkPlus,
        SourceMode::DarkModern,
        SourceMode::NzkDarkPlus,
        SourceMode::Monokai,
        SourceMode::Solarized,
    ];

    /// The next mode in the cycle; `#srcModeBtn` presses walk this.
    pub fn next(self) -> SourceMode {
        match self {
            SourceMode::DarkPlus => SourceMode::DarkModern,
            SourceMode::DarkModern => SourceMode::NzkDarkPlus,
            SourceMode::NzkDarkPlus => SourceMode::Monokai,
            SourceMode::Monokai => SourceMode::Solarized,
            SourceMode::Solarized => SourceMode::DarkPlus,
        }
    }

    /// `#sourceTheme`'s option value -> mode (`editor.js:152 themeId`). `None`
    /// for an unknown name: a select cannot invent a palette.
    pub fn from_theme_name(name: &str) -> Option<SourceMode> {
        match name {
            "darkplus" => Some(SourceMode::DarkPlus),
            "darkmodern" => Some(SourceMode::DarkModern),
            "monokai" => Some(SourceMode::Monokai),
            "solarized" => Some(SourceMode::Solarized),
            "nzk-dark+" => Some(SourceMode::NzkDarkPlus),
            _ => None,
        }
    }

    /// Stable label for tests, logs and a future in-pane indicator.
    pub fn label(self) -> &'static str {
        match self {
            SourceMode::DarkPlus => "darkplus",
            SourceMode::DarkModern => "darkmodern",
            SourceMode::NzkDarkPlus => "nzk-dark+",
            SourceMode::Monokai => "monokai",
            SourceMode::Solarized => "solarized",
        }
    }
}

/// A UI surface an action asks the host to open or toggle.
///
/// These are real shell panels/modals that `index.html` defines and `nzk_ui`
/// lays out. `actions.rs` is display-free, so it names the surface and lets the
/// host decide how to show it — the same seam as [`HostPane`].
///
/// [`HostSurface::element`] is the **one** translation from this enum to a DOM
/// id, and it is what [`NzkUiState::modal_facts`] hands to nzk_ui's
/// `UiState::modals_open`. Keeping the mapping here rather than at the call site
/// is what stops the nzk_ui's idea of "which modal is open" from drifting away
/// from the id the shell actually un-hides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostSurface {
    /// `#shortcutsModal` — the shortcuts & settings sheet.
    Shortcuts,
    /// `#editModal` — the editor.
    Edit,
    /// `#previewModal` — the preview pane.
    Preview,
    /// `#reorgModal` — the reorganisation/export dialog (`#exportBtn`).
    Export,
    /// `#findPanel` — the Find results drawer. A **drawer**, not a modal: it is
    /// `class="drawer hidden"`, so it drains through `drawers_open`.
    Find,
    /// `#nodeDrawer` — the node properties/relations drawer. Also a drawer.
    NodeDrawer,
    /// The folder picker `#browseBtn` opens — the shell's own `#modal`.
    ///
    /// Nothing in the top bar is named "modal", so this is the one family member
    /// without a dedicated opener button; it is still opened by a real caller
    /// chain: `shell.js:473` `#browseBtn` click -> `openPicker` (:94) ->
    /// `openModal` (:11) -> `modalEl.classList.remove('hidden')` on `#modal`
    /// (`shell.js:345` captures `modalEl = $('#modal')`). It is a shell overlay
    /// like the other four, not a host-native dialog.
    FolderPicker,
}

impl HostSurface {
    /// The DOM id whose `hidden` class this surface clears.
    ///
    /// The ids are the real ones in `frontend/index.html`; this is the ONLY
    /// place the two are tied together, so the nzk_ui's idea of "which overlay
    /// is open" cannot drift from the element the shell actually un-hides.
    pub fn element(self) -> &'static str {
        match self {
            HostSurface::Shortcuts => "shortcutsModal",
            HostSurface::Edit => "editModal",
            HostSurface::Preview => "previewModal",
            HostSurface::Export => "reorgModal",
            HostSurface::Find => "findPanel",
            HostSurface::NodeDrawer => "nodeDrawer",
            HostSurface::FolderPicker => "modal",
        }
    }
}

/// A real OS pane the host can open. Deliberately tiny: only panes that exist
/// and whose operation is implemented are listed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HostPane {
    /// The multi-window Blocks pane (`Pane::Blocks`, F5 toggles it).
    Blocks,
}

impl NzkUiAction {
    /// The nzk_ui DOM selector -> action. `None` for controls that are drawn but
    /// have no defined behaviour (the facets, the containers, …) so the dispatch
    /// table can pass any hit through this without special cases.
    ///
    /// Takes `&'static str` because the section ids are stored verbatim in
    /// [`NzkUiAction::ToggleSection`]; the nzk_ui's `CONTROLS` selectors are
    /// `&'static str` already.
    pub fn from_selector(selector: &'static str) -> NzkUiAction {
        match selector {
            "#fitBtn" => NzkUiAction::Fit,
            "#newWinBtn" => NzkUiAction::NewDocWindow,
            "#zoomInBtn" => NzkUiAction::ZoomIn,
            "#zoomOutBtn" => NzkUiAction::ZoomOut,
            "#wholeGraphBtn" => NzkUiAction::FullGraph,
            "#resettleBtn" => NzkUiAction::Resettle,
            "#indexBtn" => NzkUiAction::Index,
            "#blocksBtn" => NzkUiAction::OpenBlocks,
            // `#blocksClose` CLOSES the Blocks drawer. It used to map to
            // `OpenBlocks` — the same action as the button that opens it — which
            // made the close button a no-op-on-an-open-pane (or worse, an
            // opener). It is a drawer control now, with real close semantics.
            // The whole drawer family is listed together further down, so each
            // id has exactly one arm.
            "#backBtn" => NzkUiAction::NavigateBack,
            "#upBtn" => NzkUiAction::NavigateUp,
            "#modeGraph" => NzkUiAction::ModeGraph,
            "#modeFind" => NzkUiAction::ModeFind,
            // A press in the topbar search box focuses it, the same state the
            // `/` chord sets (`shell.js:452-455`).
            "#searchInput" => NzkUiAction::FocusSearch,
            "#browseBtn" => NzkUiAction::Browse,
            "#exportBtn" => NzkUiAction::Export,
            "#editBtn" => NzkUiAction::Edit,
            // The editor's run buttons (`edit.js:276-277`). Without these two
            // arms the only thing that could start a run was a Ctrl+Enter in the
            // args box, so `#editApply` was a painted button with no consequence
            // and the modal could dry-run but never write.
            "#editDry" => NzkUiAction::EditRun(crate::edit_args::RunMode::Dry),
            "#editApply" => NzkUiAction::EditRun(crate::edit_args::RunMode::Apply),
            "#editValidate" => NzkUiAction::EditAnalyze(crate::edit_wire::AnalyzeOp::ParseCheck),
            "#editDedupe" => NzkUiAction::EditAnalyze(crate::edit_wire::AnalyzeOp::Dedupe),
            "#previewBtn" => NzkUiAction::Preview,
            // The drawer's `{ } View code` (v2 `shell.js:563` →
            // `viewCodeFor(state.selected)`). Host-owned: the source pane is
            // host state, so the action is a request the host performs.
            "#nodeCodeBtn" => NzkUiAction::SourceOpen,
            "#previewRun" => NzkUiAction::PreviewRun,
            "#shortcutsBtn" => NzkUiAction::Shortcuts,
            "#nsSelect" => NzkUiAction::FormOpen("#nsSelect"),
            "#depthSelect" => NzkUiAction::FormOpen("#depthSelect"),
            "#rootInput" => NzkUiAction::FormOpen("#rootInput"),
            // The folder picker's path box. Registered as a Host control and in
            // the form model, but this mapping omitted it, so a press on it
            // resolved to `NzkUiAction::None` and the box never took focus:
            // every typed character fell through to the canvas keymap. Opening
            // the form control is exactly what `#rootInput` does one line up.
            "#modalPathInput" => NzkUiAction::FormOpen("#modalPathInput"),
            "#stopIndexBtn" => NzkUiAction::StopIndex,
            "#editTool" => NzkUiAction::FormOpen("#editTool"),
            "#uiTheme" => NzkUiAction::FormOpen("#uiTheme"),
            "#chainDepth" => NzkUiAction::FormOpen("#chainDepth"),
            "#chainDir" => NzkUiAction::FormOpen("#chainDir"),
            "#chainRun" => NzkUiAction::ChainRun,
            "#chainFlatBtn" => NzkUiAction::ChainFlat,
            "#pubSearch" => NzkUiAction::FormOpen("#pubSearch"),
            "#strSearch" => NzkUiAction::FormOpen("#strSearch"),
            // The v3-only Declared-types filter. Same contract as the two search
            // boxes: a press focuses the box, and every keystroke re-filters the
            // resident graph (`sync_search_edits`).
            "#typeSearch" => NzkUiAction::FormOpen("#typeSearch"),
            "#sourceTheme" => NzkUiAction::FormOpen("#sourceTheme"),
            // The rest of the Filters section's static controls (task-1). Each
            // press is a model transition owned by `FilterState`/the group flags;
            // before this arm existed they had no dispatch entry at all, which is
            // why they were painted-but-dead. The runtime-created chip rows reach
            // the same `FilterCmd`s through `nzk_ui::filter_click` (their payload
            // rides `data-key`), so there is one transition per control, not a
            // second copy.
            "#nsAllBtn" => NzkUiAction::Filter(FilterCmd::AllNamespaces),
            "#nsDeep" => NzkUiAction::Filter(FilterCmd::FlipNsDeep),
            "#hideEdgeLabels" => NzkUiAction::Filter(FilterCmd::FlipVtMode),
            "#hideIsolated" => NzkUiAction::Filter(FilterCmd::FlipHideIsolated),
            // `#hideChildren` and `#kindChildWrap` are the two inverse boxes over
            // ONE flag (`shell.js:602-604`, `:645-647`); both flip it.
            "#hideChildren" | "#kindChildWrap" => {
                NzkUiAction::Filter(FilterCmd::FlipGroupChildren)
            }
            "#kindEdgeWrap" => NzkUiAction::Filter(FilterCmd::FlipGroupEdges),
            // Sidebar filter controls (`filters.js`). The registry lists both as
            // `Host` (they have real consequences now), so `from_selector` must
            // name them: `#resetFilters` restores the shipped filter model and
            // `#pruneHidden` is the honest no-op (no prune API in v3 — see
            // `FilterCmd::Prune`). Without these arms the dispatch anti-drift
            // test rightly fails: a Host control with no action is a dead button.
            "#resetFilters" => NzkUiAction::Filter(FilterCmd::Reset),
            "#pruneHidden" => NzkUiAction::Filter(FilterCmd::Prune),
            // The source pane's own head controls. Every one now has a real
            // behaviour in `source_pane.rs`; the prev/next pair steps the pane's
            // document history (the same stacks `Alt+Left`/`Alt+Right` use).
            "#srcCloseBtn" => NzkUiAction::SrcClose,
            "#srcModeBtn" => NzkUiAction::SrcMode,
            "#srcPopBtn" => NzkUiAction::SrcPop,
            "#srcCopyBtn" => NzkUiAction::SrcCopy,
            "#srcPrevBtn" => NzkUiAction::SrcPrev,
            "#srcNextBtn" => NzkUiAction::SrcNext,
            "#srcRefBtn" => NzkUiAction::SrcRef,
            "#srcMethodBtn" => NzkUiAction::SrcMethod,
            "#srcWindowBtn" => NzkUiAction::SrcWindow,
            // The overlay drawer family. Container and close button are the same
            // toggle, so both a press on the panel and a press on its ✕ close it.
            "#nodeDrawer" | "#nodeDrawerClose" => NzkUiAction::ToggleDrawer("nodeDrawer"),
            "#findPanel" | "#findClose" => NzkUiAction::ToggleDrawer("findPanel"),
            "#blocksPanel" | "#blocksClose" => NzkUiAction::ToggleDrawer("blocksPanel"),
            // The overlay modal family. A modal's backdrop covers the viewport,
            // so its container is NOT its own toggle the way a drawer's is: the
            // backdrop is what a press lands on when the point misses the card,
            // and hitting it would close the dialog a press was trying to use.
            // Only the ✕ and the explicit Done/close controls toggle it.
            "#shortcutsClose" | "#shortcutsDone" => {
                NzkUiAction::ToggleModal(HostSurface::Shortcuts)
            }
            "#editClose" => NzkUiAction::ToggleModal(HostSurface::Edit),
            "#previewClose" => NzkUiAction::ToggleModal(HostSurface::Preview),
            "#reorgClose" => NzkUiAction::ToggleModal(HostSurface::Export),
            // The export dialog's four foot buttons (`reorg.js:172,183,190,200`).
            // Their consequence is an ENGINE action, so they map to
            // [`NzkUiAction::ReorgRun`]; `plan` has no shipped button (the
            // reference creates `#reorgPlan` at runtime, `reorg.js:326-331`).
            "#reorgVerify" => NzkUiAction::ReorgRun(crate::reorg_wire::Action::Verify),
            "#reorgZip" => NzkUiAction::ReorgRun(crate::reorg_wire::Action::Zip),
            "#reorgWrite" => NzkUiAction::ReorgRun(crate::reorg_wire::Action::Write),
            "#reorgCompile" => NzkUiAction::ReorgRun(crate::reorg_wire::Action::Compile),
            // The form controls (task-14). Each press opens its menu / focuses
            // its box / toggles its checkbox through `form_wire`.
            "#reorgMode" => NzkUiAction::FormOpen("#reorgMode"),
            "#reorgScope" => NzkUiAction::FormOpen("#reorgScope"),
            "#reorgGran" => NzkUiAction::FormOpen("#reorgGran"),
            "#reorgGroup" => NzkUiAction::FormOpen("#reorgGroup"),
            "#reorgSize" => NzkUiAction::FormOpen("#reorgSize"),
            "#reorgDest" => NzkUiAction::FormOpen("#reorgDest"),
            "#reorgHeader" => NzkUiAction::FormOpen("#reorgHeader"),
            "#previewSelector" => NzkUiAction::FormOpen("#previewSelector"),
            "#previewTo" => NzkUiAction::FormOpen("#previewTo"),
            "#previewOp" => NzkUiAction::FormOpen("#previewOp"),
            "#modalPathInput" => NzkUiAction::FormOpen("#modalPathInput"),
            // The folder picker's own close controls (`shell.js:742-743` wires
            // both to `closeModal()`), plus the modal's backdrop-click close at
            // :761, which is the `#modal` backdrop itself and so stays inert
            // here for the same reason as the other four backdrops.
            "#modalClose" | "#modalCancel" => {
                NzkUiAction::ToggleModal(HostSurface::FolderPicker)
            }
            // `#modalGo` and the picker's navigation controls (`#modalUp`,
            // `#modalHome`, `#modalRoot`, `#modalRepo`, `#modalSelect`) belong to
            // the *picker* family: their consequences are `PickerAction`s
            // ([`apply_picker`]), not [`NzkUiAction`]s, because a `PickerAction`
            // can carry the payload they need (`Go(String)`) and this enum is
            // `Copy`. `picker_action` in `nzk_ui.rs` resolves them; mapping them
            // here too would give each id two owners, which is the drift the
            // CONTROLS/from_selector anti-drift test exists to prevent — so they
            // deliberately fall through to `None` below.
            "#modalGo" => NzkUiAction::None,
            "#filtersHead" | "#pubHead" | "#strHead" | "#chainHead" | "#problemsHead"
            | "#relationsHead" => NzkUiAction::ToggleSection(selector),
            _ => NzkUiAction::None,
        }
    }

    /// Stable human label (window titles / logs / tests).
    pub fn label(&self) -> &'static str {
        match self {
            NzkUiAction::None => "none",
            NzkUiAction::NewDocWindow => "new-doc-window",
            NzkUiAction::Fit => "fit",
            NzkUiAction::ZoomIn => "zoom-in",
            NzkUiAction::ZoomOut => "zoom-out",
            NzkUiAction::FullGraph => "full-graph",
            NzkUiAction::Resettle => "resettle",
            NzkUiAction::Index => "index",
            NzkUiAction::ToggleSection(_) => "toggle-section",
            NzkUiAction::OpenBlocks => "open-blocks",
            NzkUiAction::NavigateBack => "navigate-back",
            NzkUiAction::NavigateForward => "navigate-forward",
            NzkUiAction::NavigateUp => "navigate-up",
            NzkUiAction::ModeGraph => "mode-graph",
            NzkUiAction::ModeFind => "mode-find",
            NzkUiAction::FocusSearch => "focus-search",
            NzkUiAction::Tab(_) => "tab",
            NzkUiAction::Arrange(_) => "arrange",
            NzkUiAction::Spacing(_) => "spacing",
            NzkUiAction::Browse => "browse",
            NzkUiAction::Export => "export",
            NzkUiAction::Edit => "edit",
            NzkUiAction::EditRun(_) => "edit-run",
            NzkUiAction::EditAnalyze(_) => "edit-analyze",
            NzkUiAction::ChainRun => "chain-run",
            NzkUiAction::ChainFlat => "chain-flat",
            NzkUiAction::ReorgRun(_) => "reorg-run",
            NzkUiAction::FormOpen(_) => "form-open",
            NzkUiAction::FormKey(_) => "form-key",
            NzkUiAction::FormMenuCommit(_) => "form-menu-commit",
            NzkUiAction::Preview => "preview",
            NzkUiAction::SourceOpen => "source-open",
            NzkUiAction::PreviewRun => "preview-run",
            NzkUiAction::Shortcuts => "shortcuts",
            NzkUiAction::FocusNamespaceFilter => "focus-ns-filter",
            NzkUiAction::ToggleDrawer(_) => "toggle-drawer",
            NzkUiAction::ToggleModal(_) => "toggle-modal",
            NzkUiAction::SrcClose => "src-close",
            NzkUiAction::SrcMode => "src-mode",
            NzkUiAction::SrcPop => "src-pop",
            NzkUiAction::Filter(_) => "filter",
            NzkUiAction::SrcCopy => "src-copy",
            NzkUiAction::SrcPrev => "src-prev",
            NzkUiAction::SrcNext => "src-next",
            NzkUiAction::SrcRef => "src-ref",
            NzkUiAction::SrcMethod => "src-method",
            NzkUiAction::SrcWindow => "src-window",
            NzkUiAction::StopIndex => "stop-index",
            NzkUiAction::ChainRun => "chain-run",
            NzkUiAction::ChainFlat => "chain-flat",
        }
    }

    /// True when the action does something on the host.
    pub fn is_effective(&self) -> bool {
        self != &NzkUiAction::None
    }
}

/// The sidebar sections `index.html` ships collapsed, by their head selector.
///
/// `ui.collapsed = null` means "leave the shipped document alone"; `Some(list)`
/// means "this is the complete collapsed set". Seeding the default with the
/// shipped three makes the first frame match the reference and every toggle a
/// pure delta — without it, the first toggle would expand all three.
pub const DEFAULT_COLLAPSED: [&str; 3] = ["#pubHead", "#strHead", "#chainHead"];

/// The overlay **drawer** DOM ids, mirroring `nzk_ui::dom::DRAWERS`.
///
/// The same three ids the shell's drawer allow-list honours. Kept here so the
/// facts this module emits can be checked *before* they cross the wire: a
/// `ToggleDrawer` naming anything else produces no drawer fact at all, rather
/// than a fact nzk_ui would silently drop.
pub const DRAWER_IDS: [&str; 3] = ["nodeDrawer", "findPanel", "blocksPanel"];

/// A display-free camera snapshot (same units as `graphviz::Camera`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CameraState {
    pub zoom: f64,
    pub x: f64,
    pub y: f64,
}

impl Default for CameraState {
    fn default() -> Self {
        CameraState { zoom: 1.0, x: 0.0, y: 0.0 }
    }
}

/// What the host must do after [`apply`]. Plain scalars so the host can apply
/// them in any order and tests can assert the exact set.
///
/// `Copy` was dropped when [`Effect::reindex_at`] gained a `PathBuf` — the one
/// non-`Copy` field, and the reason every caller now takes it by reference
/// (`&effect`) or moves it once. `Clone` remains, so a caller that needs to read
/// a flag after handing the effect on still can.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Effect {
    /// Push `NzkUiState::camera` into the view.
    pub camera: bool,
    /// Recompute the visible node/edge set (`view.rebuild_view()`).
    pub rebuild_view: bool,
    /// Re-run the layout (`view.relayout(ctx)`).
    pub relayout: bool,
    /// Lay the visible set out from scratch on the next run
    /// (`view.reseed_layout()`), instead of continuing from the positions on
    /// screen.
    ///
    /// `relayout` alone CONTINUES: it keeps every node that already had a
    /// position anchored, so over an unchanged set it moves nothing — correct
    /// for an incremental expansion, and the reason an explicit "Re-settle" must
    /// ask for this flag rather than only for a relayout. Without it
    /// `#resettleBtn` was a painted button whose only consequence was a counter
    /// (`resettle_count`), and "Full graph" seeded the thousands of newly
    /// revealed nodes at the origin.
    pub reseed_layout: bool,
    /// Request a fit on the next frame (`view.fit_pending = true`).
    pub refit: bool,
    /// Re-run the engine index (host-only; this module never touches the engine).
    pub reindex: bool,
    /// The nzk_ui must be repainted (a static rasteriser re-renders on demand).
    pub repaint_nzk_ui: bool,
    /// Open/close a real OS pane, if the action asks for one.
    pub pane: Option<HostPane>,
    /// Step the view's navigation history back one entry.
    pub navigate_back: bool,
    /// Step the view's navigation history forward one entry — the half that was
    /// missing until `GraphView::navigate_forward` (view.rs) existed. `Alt+Right`
    /// is its only producer.
    pub navigate_forward: bool,
    /// Arrange the marquee selection along this axis (`arrange.js:289`). `None`
    /// when no arrange was requested; the view answers `None` itself when the
    /// selection is too small, so no separate "failed" flag is needed.
    pub arrange: Option<crate::chords::Axis>,
    /// Widen (`+1`) or tighten (`-1`) the selection's spacing
    /// (`arrange.js:302`).
    pub spacing: Option<i32>,
    /// Step the view's navigation up to the containment parent.
    pub navigate_up: bool,
    /// Close the source pane (`#srcCloseBtn`): the host calls
    /// `SourceState::close()` on the pane it owns. `actions.rs` never holds the
    /// pane's state, exactly like `reindex`.
    pub close_source: bool,
    /// The pane's theme changed; the host rebuilds its `SourceTheme` from
    /// [`NzkUiState::source_mode`] on the next paint.
    pub source_theme: bool,
    /// Pop the source pane out, or dock it back:
    /// `Some(true)` = detach into its own OS window, `Some(false)` = dock it
    /// into the main window, `None` = nothing to do.
    pub source_pop: Option<bool>,
    /// Copy the pane's synthesized document to the system clipboard. The host
    /// owns the `egui::Context` this module cannot name, so the copy happens
    /// there via `Context::copy_text`.
    pub copy_source: bool,
    /// Re-index the corpus at a **new** root — `#modalSelect`'s consequence
    /// (`chooseCurrentFolder`, `shell.js:99-106`).
    ///
    /// The payload is the folder to index, absolute and already resolved by the
    /// picker core. `None` means "no commit was requested"; `Some(repo)` means
    /// the host must re-run the engine against `repo`. This is deliberately not
    /// a bare [`Effect::reindex`] flag: `#indexBtn` re-indexes the *current*
    /// corpus, whereas Select changes *which* corpus that is, so the two cannot
    /// share one boolean without the host guessing where the root came from.
    pub reindex_at: Option<PathBuf>,
    /// Open (or close) the call/type hierarchy panel, and/or reload the walk.
    ///
    /// `None` = the hierarchy was not touched. `Some(_)` carries the *request*,
    /// not the result: this module never walks the graph (that is
    /// [`crate::app::hierarchy_wire`]'s `walk_call_section`, the host's job to
    /// call), exactly as [`Effect::reindex`] never runs the engine.
    pub hierarchy: Option<HierarchyMsg>,
    /// A Layout ▸ slider edit, live or committed.
    ///
    /// `None` = no slider was touched. This is a second channel for the same
    /// reason [`Effect::hierarchy`] is: a `NzkUiAction` cannot hold a value
    /// (its fields are all `Copy` and its variants are payload-free), and a
    /// range input is precisely a *value*, produced by a pointer position.
    /// `actions.rs` also cannot map a pointer to a value — it has no
    /// `FrameGeometry`. So this carries the already-resolved edit and the host
    /// applies it, exactly as `reindex_at` carries a path.
    pub slider: Option<SliderEdit>,
    /// `#pubSection` was expanded for the first time, so the host must fetch the
    /// public-definitions list.
    ///
    /// This is the native reading of the reference's lazy load
    /// (`shell.js:556`: expanding `#pubSection` calls `loadDefinitions()`), and
    /// it is a *host* effect for the same reason [`Effect::reindex`] is:
    /// `actions.rs` has no `Index`, and the definitions come from the engine.
    ///
    /// A flag rather than a payload, unlike [`Effect::slider`], because there is
    /// nothing to carry: the query is fixed (`scope="public"`, `limit=400`,
    /// `filters.js:491`) and the answer is the engine's. `true` means "load it if
    /// it is not already loaded" — the host checks
    /// [`NzkUiState::pub_list`], which is the one place "already loaded" lives,
    /// mirroring `if (state.defs.loaded) return` at `filters.js:490`.
    pub load_public_definitions: bool,
    /// A run the edit modal asked for, for the host to perform.
    ///
    /// `None` = no run was requested. The payload is the request
    /// [`crate::edit_wire::EditToolView::press_apply`] /
    /// [`crate::edit_wire::EditToolView::press_dry`] built — the host sends it
    /// through [`crate::edit_transport::invoke`] and records the answer with
    /// `finish_run`.
    ///
    /// This is the payload channel for the same reason [`Effect::slider`] and
    /// [`Effect::hierarchy`] are: `actions.rs` has no `Index`, so it can decide
    /// *that* a run happened and *which* mode it was, but it cannot perform one.
    /// Without this field a press on `#editApply` could only set the busy flag
    /// and never reach the engine — the modal would dry-run and never write.
    pub edit_run: Option<crate::edit_wire::RunRequest>,
    /// `#previewBtn` opened the change-preview panel, so the host must build its
    /// content (`#previewOut` / `#previewNote`) from the engine.
    ///
    /// A flag rather than a payload, like [`Effect::load_public_definitions`]:
    /// `actions.rs` has no `Index`, and the request (the graph selection + the
    /// pending `#editArgs`) is host state it cannot see. `true` means "build it
    /// now if nothing is already built"; the host does the build in process.
    pub preview: bool,
    /// `#previewRun` was pressed (or Enter was committed in `#previewTo`), so the
    /// host must build the request from the typed `#previewSelector`/`#previewOp`/
    /// `#previewTo` and run it through the REAL engine.
    ///
    /// Same channel shape as [`Effect::preview`], and for the same reason: the
    /// values live on `NzkUiState::form` (which the host holds) and the plan is
    /// an engine answer this display-free layer cannot produce.
    pub preview_run: bool,
    /// `#nodeCodeBtn` — the drawer's `{ } View code` (`shell.js:563` →
    /// `viewCodeFor(state.selected)`). The host opens the SELECTED declaration
    /// in its source pane (`App::preopen_source`); this arm cannot, because the
    /// pane and its `Index` are host state.
    ///
    /// `true` only when a node is selected: v2's `viewCodeFor` toasts
    /// "Select a node first" and returns without opening, so a press with no
    /// selection must not open an empty document.
    pub source_open: bool,
    /// `#kindChildWrap` / `#hideChildren` pressed: the host must push this value
    /// into `GraphView::set_group_children` (the single writer for the flag).
    ///
    /// A payload, not a bare flag like the other effects, because the flag is
    /// binary and two *inverse* controls flip it: carrying the resolved value is
    /// what stops a flip press from being applied twice (once in `actions.rs`,
    /// once by the view) and landing on the original state.
    pub group_children: Option<bool>,
    /// `#kindEdgeWrap` / hidden `#groupEdges` box pressed, for
    /// `GraphView::set_group_edges` — the exact counterpart of
    /// [`Effect::group_children`].
    pub group_edges: Option<bool>,
    /// `#depthSelect` committed a new graph-neighborhood expansion depth.
    pub depth: Option<u32>,
    /// `#srcPrevBtn` (`Some(false)`) / `#srcNextBtn` (`Some(true)`) asked the
    /// host to step the source pane's document history. The stacks live on
    /// `SourceState`, which this module does not own.
    pub source_history: Option<bool>,
    /// `#srcRefBtn` asked for a `file:line` clipboard write (`source.js:546`).
    pub copy_source_ref: bool,
    /// `#srcMethodBtn` asked for the whole declaration to be re-opened for the
    /// document currently in the pane (`source.js:478-482`).
    pub source_whole_method: bool,
    /// `#srcWindowBtn` asked to show the current document in a second OS
    /// window (`shell.js:680-683`).
    pub source_window: bool,
    /// Stop an in-flight index: the host flips the worker's shared cancel flag
    /// (the engine restores the previous index and returns `cancelled: true`).
    pub cancel_index: bool,
    /// `#editValidate` / `#editDedupe` built a real `/api/analyze` request from
    /// the args box; the host performs it and records the raw answer.
    pub edit_analyze: Option<crate::edit_wire::AnalyzeRequest>,
    /// `#editTool` selected a different operation in the Edit dialog.
    pub edit_tool: Option<String>,
    /// `#uiTheme` selected a different whole-viewer theme.
    pub ui_theme: Option<String>,
    /// `#chainDepth`/`#chainDir` changed the resolve-chain request.
    pub chain_config: Option<(u32, String)>,
    /// `#chainRun` asked for a chain walk for the selected node.
    pub chain_run: bool,
    /// `#strSearch` committed a query the host must run through the engine.
    pub str_search: Option<String>,
}

/// Which Layout ▸ slider a [`SliderEdit`] addresses.
///
/// The four the reference wires (`shell.js:818-869`), in its own order. One enum
/// for all four rather than four `Effect` flags, because the *value* is the whole
/// point and a flag cannot carry it — the same reasoning that made
/// [`HierarchyMsg`] an enum instead of four booleans.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SliderKind {
    /// `#spacingRange` — `state.settings.spacing`, 8..=240, step 1.
    Spacing,
    /// `#nodeSizeRange` — `state.settings.nodeSize`, 2..=24, step 1.
    NodeSize,
    /// `#iterRange` — `state.settings.iterations`, 300..=1500, step 50.
    Iterations,
    /// `#curveRange` — `state.settings.curvature`, 0..=0.2, step 0.01.
    Curve,
    /// `#textSizeRange` — the fork's own addition to the Layout group.
    ///
    /// Not in the reference: this port ships a separate chip-font constant
    /// (`graphviz::CHIP_FONT`), so the user asked for a control over it. It
    /// follows the *node size* shape (live update, re-settle on release) because
    /// both feed measured chip geometry.
    TextSize,
}

impl SliderKind {
    /// The slider's DOM id — the key the geometry and the pointer are resolved
    /// against, and the one `nzk_ui` lays out.
    pub fn selector(self) -> &'static str {
        match self {
            SliderKind::Spacing => "#spacingRange",
            SliderKind::NodeSize => "#nodeSizeRange",
            SliderKind::Iterations => "#iterRange",
            SliderKind::Curve => "#curveRange",
            SliderKind::TextSize => "#textSizeRange",
        }
    }

    /// The readout span beside the slider (`syncLayoutControls`,
    /// `shell.js:807-816`).
    pub fn readout_selector(self) -> &'static str {
        match self {
            SliderKind::Spacing => "#spacingOut",
            SliderKind::NodeSize => "#nodeSizeOut",
            SliderKind::Iterations => "#iterOut",
            SliderKind::Curve => "#curveOut",
            SliderKind::TextSize => "#textSizeOut",
        }
    }

    /// `(min, max, step)` exactly as `index.html`'s `min`/`max`/`step`
    /// attributes declare them. The pointer-to-value mapping and the clamp both
    /// read this, so the control and the model cannot disagree about the range.
    pub fn range(self) -> (f64, f64, f64) {
        match self {
            SliderKind::Spacing => (SPACING_MIN, SPACING_MAX, 1.0),
            SliderKind::NodeSize => (NODE_SIZE_MIN as f64, NODE_SIZE_MAX as f64, 1.0),
            SliderKind::Iterations => (ITER_MIN as f64, ITER_MAX as f64, 50.0),
            SliderKind::Curve => (CURVE_MIN, CURVE_MAX, 0.01),
            SliderKind::TextSize => (TEXT_SIZE_MIN as f64, TEXT_SIZE_MAX as f64, 1.0),
        }
    }

    /// The four the reference wires, in `shell.js:818-869` order.
    pub const ALL: [SliderKind; 5] = [
        SliderKind::Spacing,
        SliderKind::NodeSize,
        SliderKind::Iterations,
        SliderKind::Curve,
        SliderKind::TextSize,
    ];

    /// Resolve a DOM selector back to its slider.
    pub fn from_selector(selector: &str) -> Option<SliderKind> {
        SliderKind::ALL.into_iter().find(|k| k.selector() == selector)
    }
}

/// One range-input edit: which slider, what value, and which half of the
/// gesture produced it.
///
/// # Live vs release is part of the payload, not a caller's convention
///
/// The reference distinguishes them per control (`shell.js:818-869`): `input`
/// (dragging) updates the *readout* — and for spacing/curve/nodeSize the live
/// value too — while `change` (release) is what re-runs the layout, because the
/// FA2 pass is heavy. Encoding the phase in the message means the host cannot
/// accidentally re-settle on every pixel of a drag: the timing is data, and a
/// test can assert on it directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SliderEdit {
    pub kind: SliderKind,
    /// The value the control's range mapping produced, already clamped to
    /// [`SliderKind::range`] (the reference clamps in the `change` handler,
    /// `shell.js:825`).
    ///
    /// Held as the f64's **bit pattern**, not as an `f64`: [`Effect`] derives
    /// `Eq` (it is compared as a whole to detect a no-op), and `f64` is only
    /// `PartialEq`, so a bare field would not compile. `SliderEdit::value`
    /// converts back, and every value that reaches here has already been
    /// snapped and clamped, so no NaN can be stored.
    value_bits: u64,
    pub phase: SliderPhase,
}

impl SliderEdit {
    pub fn new(kind: SliderKind, value: f64, phase: SliderPhase) -> SliderEdit {
        let (min, max, _) = kind.range();
        // `clamp` also normalises NaN to `max` (f64::clamp returns the bound when
        // the value is NaN and min <= max), so the stored bits are never NaN.
        let v = value.clamp(min, max);
        SliderEdit { kind, value_bits: v.to_bits(), phase }
    }

    /// The value, as an `f64` again.
    pub fn value(self) -> f64 {
        f64::from_bits(self.value_bits)
    }

    /// Does this edit change the *view's* model (as opposed to only the readout)?
    ///
    /// The reference's live handlers write `state.settings` for curve
    /// (`shell.js:839`), nodeSize (:845) and — on `change` — spacing (:825) and
    /// iterations (:834). Its `input` handler for spacing and iterations writes
    /// only the readout, which is the one asymmetry worth preserving: dragging
    /// the FA2 iteration count must not start a run per pixel.
    pub fn writes_settings(self) -> bool {
        match self.phase {
            SliderPhase::Live => matches!(
                self.kind,
                SliderKind::Curve | SliderKind::NodeSize | SliderKind::TextSize
            ),
            SliderPhase::Release => true,
        }
    }

    /// Does this edit require the layout to be re-run once it is applied?
    ///
    /// `change` on spacing/iterations/nodeSize re-runs FA2; curve only saves
    /// (`shell.js:841`), and the fork's text size re-settles like node size.
    pub fn resettles(self) -> bool {
        self.phase == SliderPhase::Release
            && matches!(
                self.kind,
                SliderKind::Spacing
                    | SliderKind::Iterations
                    | SliderKind::NodeSize
                    | SliderKind::TextSize
            )
    }
}

/// Which half of a range gesture an edit came from (`input` vs `change`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SliderPhase {
    /// `input` — the pointer is still down and moving.
    Live,
    /// `change` — the pointer was released, or the value was committed.
    Release,
}

/// One request against the hierarchy panel (`hierarchy.js:548-594, 681-688`).
///
/// A separate channel from [`NzkUiAction`] for the same reason
/// [`PickerAction`] is one: `NzkUiAction`'s fields are all `Copy`
/// (`HostSurface`, `&'static str`), while a hierarchy row press carries the
/// row's **path** (`toggle_collapse`'s key, built by the producer) and the
/// selects carry a direction/depth. Adding a `String` to `NzkUiAction` would
/// put a lifetime on the whole enum.
///
/// It is still display-free: nothing here names `egui`, a window or a GPU.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HierarchyMsg {
    /// `Shift|Alt+KeyH` on the selected node: open the panel and walk it
    /// (`openCallHierarchy`, `hierarchy.js:574-594`).
    ///
    /// The payload is the resolved node id. The host resolves the selection to
    /// an id *before* dispatching, because only the host can see the view's
    /// selected node — `actions.rs` has no graph.
    Open(String),
    /// Close the panel. The reference has no explicit close key (its panel is a
    /// peek overlay); `Esc` is this host's, and it must consume the key.
    Close,
    /// `dblclick` on a `.hop` row: toggle that row's collapse
    /// (`hierarchy.js:501-505`). The payload is the row's path, which is the
    /// producer's key — the same `""`/`/0:id` scheme `dom_facts` emits.
    ToggleCollapse(String),
    /// A `.cs-hier-select` change: reload the walk in a new direction
    /// (`hierarchy.js:421-433`).
    SetDir(String),
    /// A `.cs-hier-select` change: reload the walk at a new depth
    /// (`hierarchy.js:435-443`).
    SetDepth(u32),
}

impl Effect {
    /// True when the action changed nothing.
    pub fn is_noop(&self) -> bool {
        *self == Effect::default()
    }
}

/// One shortcut row of the shortcuts/settings sheet (`#shortcutsBody`).
///
/// The reference keeps the panel in `SHORTCUTS` (`shell.js:251-287`) and folds
/// `arrange.shortcuts()` (`arrange.js:442-468`) in before the `Mouse` group via
/// `shortcutGroups()` (`shell.js:290-296`). This is that same list, in the same
/// order, as data. The list is constant in the reference too (there is no
/// rebinding UI yet), so it is owned here as the app's own fact rather than
/// hard-wired into the renderer, which paints facts.
///
/// `keys` keeps the reference's nested shape: the outer list is the
/// *alternatives* joined by `/`, the inner list is one chord's *parts* joined by
/// `+` (each part becomes a `<kbd>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortcutRow {
    /// The `.shortcut-group` head this row sits under; a head is drawn when it
    /// changes from the previous row.
    pub group: &'static str,
    /// Alternatives (outer, `/`-joined) of chord parts (inner, `+`-joined).
    pub keys: &'static [&'static [&'static str]],
    /// `.shortcut-desc` text.
    pub desc: &'static str,
}

/// The complete `#shortcutsBody` row list, in panel order — `SHORTCUTS`
/// (`shell.js:251-287`) with `arrange.shortcuts()` (`arrange.js:442-468`)
/// inserted before `Mouse`, exactly `shortcutGroups()` (`shell.js:290-296`) does.
/// Verbatim copy: the glyphs (`−`, `↑`, `↓`, `°`, `’`, …) are the reference's.
pub const SHORTCUT_ROWS: &[ShortcutRow] = &[
    ShortcutRow {
        group: "View",
        keys: &[&["Ctrl", "Shift", "C"]],
        desc: "Recenter the view (fit the graph)",
    },
    ShortcutRow {
        group: "View",
        keys: &[&["+"], &["="]],
        desc: "Zoom in",
    },
    ShortcutRow {
        group: "View",
        keys: &[&["−"], &["_"]],
        desc: "Zoom out",
    },
    ShortcutRow {
        group: "View",
        keys: &[&["R"]],
        desc: "Re-settle the layout",
    },
    ShortcutRow {
        group: "Navigate",
        keys: &[&["/"]],
        desc: "Search symbols",
    },
    ShortcutRow {
        group: "Navigate",
        keys: &[&["F"]],
        desc: "Find definitions / usages",
    },
    ShortcutRow {
        group: "Navigate",
        keys: &[&["Backspace"]],
        desc: "Back to the previous node",
    },
    ShortcutRow {
        group: "Navigate",
        keys: &[&["↑"], &["↓"]],
        desc: "Move the selection (Find results)",
    },
    ShortcutRow {
        group: "Navigate",
        keys: &[&["Enter"]],
        desc: "Open the selected result",
    },
    ShortcutRow {
        group: "Navigate",
        keys: &[&["Esc"]],
        desc: "Close panels / clear the selection",
    },
    ShortcutRow {
        group: "Code",
        keys: &[&["C"]],
        desc: "View the selected or hovered node’s code",
    },
    ShortcutRow {
        group: "Tabs",
        keys: &[&["Ctrl", "Tab"]],
        desc: "Next document tab",
    },
    ShortcutRow {
        group: "Tabs",
        keys: &[&["Ctrl", "Shift", "Tab"]],
        desc: "Previous document tab",
    },
    ShortcutRow {
        group: "Tabs",
        keys: &[&["Alt", "W"]],
        desc: "Close the active document tab",
    },
    ShortcutRow {
        group: "Tabs",
        keys: &[&["Alt", "Shift", "W"]],
        desc: "Close every other tab",
    },
    ShortcutRow {
        group: "Tabs",
        keys: &[&["Alt", "S"]],
        desc: "Split the source pane / collapse the split",
    },
    ShortcutRow {
        group: "Select",
        keys: &[&["drag"]],
        desc: "Box-select the nodes inside the box",
    },
    ShortcutRow {
        group: "Select",
        keys: &[&["Middle", "drag"]],
        desc: "Pan the canvas",
    },
    ShortcutRow {
        group: "Select",
        keys: &[&["Shift", "drag"]],
        desc: "Box-select (same as a plain drag on empty canvas)",
    },
    ShortcutRow {
        group: "Select",
        keys: &[&["Shift", "click"]],
        desc: "Highlight one node",
    },
    ShortcutRow {
        group: "Select",
        keys: &[&["Ctrl", "click"]],
        desc: "Add / remove a node from the selection",
    },
    ShortcutRow {
        group: "Arrange",
        keys: &[&["Ctrl", "Shift", "V"]],
        desc: "Stack the selection vertically in step order (press again to flip)",
    },
    ShortcutRow {
        group: "Arrange",
        keys: &[&["Ctrl", "Shift", "H"]],
        desc: "Lay the selection out horizontally in step order (press again to flip)",
    },
    ShortcutRow {
        group: "Arrange",
        keys: &[&["Ctrl", "Shift", "D"]],
        desc: "Align the selection on a 45° diagonal",
    },
    ShortcutRow {
        group: "Arrange",
        keys: &[&["Shift", "+"]],
        desc: "Widen the arranged spacing",
    },
    ShortcutRow {
        group: "Arrange",
        keys: &[&["Shift", "−"]],
        desc: "Tighten the arranged spacing",
    },
    ShortcutRow {
        group: "Mouse",
        keys: &[&["Double-click"]],
        desc: "Expand a node",
    },
    ShortcutRow {
        group: "Mouse",
        keys: &[&["Right-click"]],
        desc: "Pin the node card open",
    },
    ShortcutRow {
        group: "Mouse",
        keys: &[&["Drag node"]],
        desc: "Pin a node in place",
    },
    ShortcutRow {
        group: "Mouse",
        keys: &[&["Alt", "click"]],
        desc: "Pin / unpin a node",
    },
    ShortcutRow {
        group: "Mouse",
        keys: &[&["Middle", "drag"]],
        desc: "Pan the view",
    },
    ShortcutRow {
        group: "Mouse",
        keys: &[&["Wheel"]],
        desc: "Zoom at the pointer",
    },
    ShortcutRow {
        group: "Panes",
        keys: &[&["F2"]],
        desc: "Toggle the source pane",
    },
    ShortcutRow {
        group: "Panes",
        keys: &[&["F3"]],
        desc: "Toggle the node drawer",
    },
    ShortcutRow {
        group: "Panes",
        keys: &[&["F4"]],
        desc: "Toggle Find",
    },
    ShortcutRow {
        group: "Panes",
        keys: &[&["F5"]],
        desc: "Toggle Blocks",
    },
    ShortcutRow {
        group: "Help",
        keys: &[&["?"]],
        desc: "Open this shortcuts panel",
    },
];

/// The sidebar filter model (`filters.js`): which kind/edge chips are OFF and
/// the namespace selection.
///
/// **Default = empty = every chip ON, all namespaces** — the shipped render.
/// The set stores the *hidden* kinds/edges rather than the visible ones so that
/// a chip the app has never heard of is ON by construction: a corpus can gain a
/// node kind between runs and the filter model can never silently hide it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterState {
    /// Kinds whose chip is OFF (`filters.js`'s `state.filters.kinds` inverse).
    pub hidden_kinds: std::collections::BTreeSet<String>,
    /// Edge kinds whose chip is OFF.
    pub hidden_edges: std::collections::BTreeSet<String>,
    /// Project ids whose chip is OFF (`filters.js`'s `state.filters.projects`).
    pub hidden_projects: std::collections::BTreeSet<String>,
    /// The `#nsSelect` value; empty = "All namespaces".
    pub namespace: String,
    /// `#nsDeep` — include sub-namespaces (`index.html` ships it checked).
    pub ns_deep: bool,
    /// `#hideIsolated` — drop nodes with no drawn edge (`core.js`'s
    /// `state.filters.hideIsolated`; `graph.js:814-820` filters them out of
    /// `state.view` after the edge set is known).
    pub hide_isolated: bool,
    /// `#hideEdgeLabels` (legacy id) — **VT mode**. Off: the relation is the
    /// reference word box beside a curved arrow. On: the edge is a direct line
    /// with the relation drawn as a small node on it (VirusTotal style), so the
    /// relation is never hidden — only its presentation changes.
    pub vt_mode: bool,
    /// "Only related nodes" — draw the selected node and every node it shares
    /// an edge with, in both directions.
    ///
    /// Not a v2 sidebar flag: the native canvas gained a focus filter because a
    /// 9k-node corpus cannot be read through chips alone. It is still a *view*
    /// predicate over the resident graph, never a re-index, and it uses the
    /// live selection as its anchor so selecting another node (a relation row,
    /// a find row, a chain hop) moves the focus with the selection. The relation
    /// set is the drawer's own (`all_neighbours`), so the canvas and the
    /// relation buckets cannot disagree about what "related" means.
    pub focus_only: bool,
    /// "Hide parents" — drop the selected node's containment ancestors from the
    /// canvas. The ancestor chain is the `contains` edge walked upward, the same
    /// relation the drawer shows as its `Parent` bucket, so the canvas and the
    /// drawer cannot disagree about what a parent is.
    pub focus_parents: bool,
}

impl Default for FilterState {
    /// The shipped model: every chip ON, all namespaces, `#nsDeep` checked.
    /// Hand-written because `derive(Default)` would set `ns_deep: false` and
    /// silently change the namespace filter's shipped default.
    fn default() -> Self {
        FilterState {
            hidden_kinds: std::collections::BTreeSet::new(),
            hidden_edges: std::collections::BTreeSet::new(),
            hidden_projects: std::collections::BTreeSet::new(),
            namespace: String::new(),
            ns_deep: true,
            hide_isolated: false,
            vt_mode: false,
            focus_only: false,
            focus_parents: false,
        }
    }
}

impl FilterState {
    /// True when nothing is filtered (every chip on, all namespaces).
    pub fn is_default(&self) -> bool {
        self.hidden_kinds.is_empty()
            && self.hidden_edges.is_empty()
            && self.hidden_projects.is_empty()
            && self.namespace.is_empty()
            && self.ns_deep
            && !self.hide_isolated
            && !self.vt_mode
            && !self.focus_only
            && !self.focus_parents
    }
}

/// One press on the sidebar filter controls (`filters.js`).
///
/// The commands are model transitions, not DOM writes: `actions::apply` owns
/// [`FilterState`], and the host mirrors the resulting model into the view's
/// render predicate and into the `ui.filters` facts (chip paint).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilterCmd {
    /// A `.facet[data-id="<kind>"]` press: toggle that node kind.
    ToggleKind(String),
    /// A `.edgekey[data-id="<edge kind>"]` press: toggle that edge kind.
    ToggleEdge(String),
    /// A project chip press (`filters.js` `state.filters.projects`): toggle one
    /// project id. The chip carries `data-id="proj:<id>"` (nzk_ui paints it
    /// alongside v2's `data-project`) so the one `data-id` carrier routes it.
    ToggleProject(String),
    /// A `#nsSelect` change: `""` = all namespaces.
    SetNamespace(String),
    /// A `#nsDeep` change.
    SetNsDeep(bool),
    /// `#resetFilters`: every chip back on, all namespaces, `#nsDeep` checked.
    Reset,
    /// A `linkbtn[data-kinds]` press: `filters.js` `setAllKinds(on)` — show every
    /// node kind (`true`) or hide every one (`false`). The kind universe is
    /// [`NzkUiState::known_kinds`], which the host mirrors from the graph (this
    /// display-free layer cannot see a `Graph`).
    AllKinds(bool),
    /// A `linkbtn[data-edges]` press: `filters.js` `setAllEdges(on)`.
    AllEdges(bool),
    /// The Projects block's `all`/`none` links: `setAllProjects(on)`
    /// (`filters.js:80-96`). The project universe is
    /// [`NzkUiState::known_projects`], mirrored from `Graph::projects`.
    AllProjects(bool),
    /// `#nsAllBtn`: `setNamespaceFilter('')` — every namespace.
    ///
    /// Modelled as its own command rather than reusing
    /// [`FilterCmd::SetNamespace`] so a press on the button and the select's
    /// "All namespaces" row cannot drift: both end in the same `""` value, and
    /// this arm is the one that also clears the select's own value display.
    AllNamespaces,
    /// `#nsDeep` checked ↔ unchecked (`filters.js` `setNamespaceDeep`).
    FlipNsDeep,
    /// `#hideIsolated` checked ↔ unchecked (`state.filters.hideIsolated`).
    FlipHideIsolated,
    /// `#hideEdgeLabels` checked ↔ unchecked (the legacy id of VT mode).
    FlipVtMode,
    /// "Hide parents" checked ↔ unchecked (`FilterState::focus_parents`).
    FlipFocusParents,
    /// `#kindChildWrap` / `#hideChildren`: ONE flag, two inverse boxes
    /// (`shell.js:602-604`, `:645-647`). The action flips
    /// [`NzkUiState::group_children`]; the host pushes it into
    /// `GraphView::set_group_children` and the nzk_ui paints both boxes from it.
    FlipGroupChildren,
    /// `#kindEdgeWrap` / hidden `#groupEdges` box: flip
    /// [`NzkUiState::group_edges`] (`shell.js:609-612`).
    FlipGroupEdges,
    /// `#pruneHidden`: remove the nodes the filters hide from the graph.
    ///
    /// The deletion is a **view** transition (`GraphView::prune_hidden`, which
    /// keeps the resident `Graph` indices consistent with every index-valued
    /// model), so this actions-layer arm deliberately mutates no model state and
    /// returns `false`: `app.rs::apply_and_execute` sees this action and calls
    /// the view method after the filter push. Keeping the split here is what
    /// stops the button from being a dead control while `actions.rs` stays free
    /// of `GraphView`.
    Prune,
}

/// Host-owned state the actions mutate. Deliberately plain data: native-css can
/// hold or pass it without any lifetime tie to `egui`.
#[derive(Clone, Debug)]
pub struct NzkUiState {
    pub camera: CameraState,
    /// Multiplier per zoom step.
    pub zoom_step: f64,
    pub zoom_min: f64,
    pub zoom_max: f64,
    /// True once "Full graph" has been requested (host sets `ViewMode::Full`).
    pub full_graph: bool,
    /// Monotone count of accepted re-settle requests.
    pub resettle_count: u64,
    /// Monotone count of accepted re-index requests.
    pub reindex_count: u64,
    /// Collapsed sidebar sections, by the nzk_ui's DOM selector (`#filtersHead`,
    /// …). This is the ONE owner of the collapsed list; the nzk_ui reads it to
    /// build its `ui.collapsed` facts and must not keep its own copy.
    pub collapsed: Vec<String>,
    /// Which shell surface is open, if any (`#shortcutsModal`, `#findPanel`, …).
    /// One owner, so a press on one control cannot leave another surface open.
    ///
    /// This is the field the nzk_ui serialises into the shell's `UiState`: a
    /// modal surface becomes `UiState::modals_open`, a drawer surface becomes
    /// `UiState::drawers_open`, and a surface with no shell element (the native
    /// folder chooser) becomes neither. It is the ONLY thing that makes one of
    /// those boxes non-zero, and therefore the only thing that makes the
    /// controls inside it hit-testable at all.
    pub surface: Option<HostSurface>,
    /// The open overlay **drawer**, by its DOM id (`nodeDrawer`, `findPanel`,
    /// `blocksPanel`), or `None` when every drawer is closed.
    ///
    /// One owner, like [`NzkUiState::surface`]: a drawer at a time, so a press
    /// can never leave two overlapping panels laid out on top of each other.
    /// This is the field the nzk_ui serialises into `ui.drawers_open`; it is the
    /// ONLY thing that makes a drawer's box non-zero, and therefore the only
    /// thing that makes its controls hit-testable at all.
    ///
    /// It is disjoint from [`NzkUiState::surface`] by construction:
    /// [`NzkUiAction::ToggleDrawer`] clears `surface` and
    /// [`NzkUiAction::ToggleModal`] clears this, so the two channels can never
    /// both name the same element and a press can never leave two overlays up.
    pub drawer_open: Option<&'static str>,
    /// The Find panel's own state (`#findQuery`, `#findMeta`, `#findNotice`,
    /// `#findResults`).
    ///
    /// Kept here rather than in the view because the query is *nzk_ui* state:
    /// it survives selection changes, and `#findPanel`'s open-ness is already
    /// owned by [`NzkUiState::drawer_open`]. `FindView` stores only the query
    /// text and re-runs the search at projection time, so a re-index can never
    /// serve a hit list from a graph that no longer exists.
    pub find: crate::find_wire::FindView,
    /// The graph-mode `#suggestions` popup state (the topbar box's other mode).
    ///
    /// Same contract as [`NzkUiState::find`]: only the query (+ the active row)
    /// is remembered and `search_default` re-runs at projection time. Kept here
    /// — not in the view — because it is nzk_ui state that survives selection
    /// changes. While the query is empty it publishes **no** facts at all, so the
    /// shipped `#suggestions` box stays hidden and byte-identical.
    pub suggest: crate::find_wire::SuggestionView,
    /// The shortcuts/settings sheet's row list (`#shortcutsBody`).
    ///
    /// Constant reference data ([`SHORTCUT_ROWS`]): `SHORTCUTS` in
    /// `shell.js:251-287` plus `arrange.shortcuts()`, which is exactly what the
    /// reference's `renderShortcuts` paints on open. Owned here — rather than
    /// hard-wired in the renderer — so the app stays the single publisher of its
    /// own fact; the sheet's *open-ness* is [`NzkUiState::surface`], and that is
    /// what decides whether `ui_state_json` publishes the rows at all.
    pub shortcuts: &'static [ShortcutRow],
    /// The sidebar filter model (`filters.js`) — see [`FilterState`].
    ///
    /// Default = every chip ON / all namespaces, which is what keeps the shipped
    /// render byte-identical; a facet/edge press or Reset mutates this through
    /// [`NzkUiAction::Filter`], and the host mirrors it into the view predicate
    /// and the `ui.filters` facts.
    pub filters: FilterState,
    /// The depth input's committed value; one hop matches the existing view.
    pub depth: u32,
    /// True while the `/` chord has handed the keyboard to the topbar search
    /// box (`shell.js:452-455`).
    ///
    /// There is no native widget focus to read: on this side the winit loop
    /// delivers every key to [`crate::app::App::handle_key`], so "the search box
    /// is focused" is a state this module has to *own* rather than observe. It
    /// is the flag that turns plain characters into
    /// [`crate::find_wire::FindView::push_char`] calls, which is what makes
    /// typing reach `#findQuery` at all — and it is cleared by Escape and by
    /// Enter, the two keys the reference's `commitSearch`/blur paths use.
    pub search_focused: bool,
    /// The open document stack (`window.CsTabs`, `shell.js:910-1871`) — one
    /// owner for the tab chords. `Alt+W`/`Alt+Shift+W`/`Alt+S`/`Ctrl+Tab` all
    /// act on this and nothing else.
    pub tabs: crate::chords::tabs::Tabs,
    /// The folder picker's own state (`#modalPath`, `#modalBody`, `#modalNote`,
    /// `#modalSelect`), for the `#browseBtn` -> `#modal` flow.
    ///
    /// `Option` because a [`crate::find_wire::PickerView`] needs a repo root, and
    /// there is no honest root to invent before one exists. `None` means "no
    /// picker has been opened", which is a different state from "opened at an
    /// empty path" and must stay distinguishable.
    pub picker: Option<crate::find_wire::PickerView>,
    /// The call/type hierarchy panel's own state (`.cs-hier-title`,
    /// `.cs-hier-controls`, `.cs-hier-body`), for the `Shift+Alt+H` flow.
    ///
    /// Kept here rather than in the view for the same reason as
    /// [`NzkUiState::find`]: it is *nzk_ui* state that survives selection
    /// changes, and it is the one owner of the panel's `mode`/`dir`/`depth`/
    /// `collapse`, so a repaint can never disagree with what a click produced.
    ///
    /// The panel starts closed, and a closed view yields `None` from
    /// `dom_facts()` — which is what keeps the shipped document byte-identical
    /// (`@frame 1600 1000 4bd131fa71b2ccc8`). The key is therefore *absent*
    /// rather than `null` on the default paint.
    pub hierarchy: crate::app::hierarchy_wire::HierarchyView,
    /// The `#pubCount` readout: how many public definitions the **engine** found
    /// (`Index::definitions("public", N).total`).
    ///
    /// # Why this cannot be recomputed here
    ///
    /// The engine's predicate is `n.get("access") == Some("public")` over the
    /// **raw** node JSON (`core/src/index.rs:3415`), and that raw JSON is
    /// discarded when the payload is converted to [`crate::model::Node`]:
    /// `Node` (`app/src/model.rs:43-55`) carries id/label/full/kind/ns/owner/
    /// file/line/end_line/degree/external and **no `access`**. So there is no
    /// way to recompute this count from a `Graph`, and re-deriving it would be a
    /// second implementation of the predicate that could silently disagree with
    /// the engine. The number therefore comes from the one authority.
    ///
    /// # Why `Option`, never `Some(0)`
    ///
    /// `None` means "no count was ever supplied" and makes the readout **absent**
    /// — the span keeps the shipped `index.html` content. A false `0` would be
    /// displayed as a fact ("this corpus has no public definitions") when the
    /// truth is that nobody asked the engine. An absent readout is honest; a
    /// wrong zero is a displayed falsehood.
    pub pub_total: Option<usize>,
    /// The `#pubList` rows, mirroring `loadDefinitions` (`filters.js:490-500`).
    ///
    /// Loaded from the **same** `Index::definitions("public", N)` call as
    /// [`NzkUiState::pub_total`], so the count and the list can never disagree:
    /// they are two views of one answer. `None` means "not loaded yet" — the
    /// panel ships collapsed and the reference only fetches on first expand
    /// (`shell.js:556` `if (sec === pubSectionEl) loadDefinitions();`), so the
    /// default paint must not carry a list.
    pub pub_list: Option<Vec<crate::model::PubRow>>,
    /// `#uiTheme`'s committed value. The host mirrors it into the graph view's
    /// palette (`UiTheme`), so the selected theme is what the canvas paints.
    pub ui_theme: String,
    /// The source pane's head facts (`#sourceTitle`, `#sourceNote`, button
    /// disabled states), mirrored from the live `SourceState` by the paint pass.
    /// `None` while the pane is closed, so the shipped document is untouched.
    pub source_facts: Option<serde_json::Value>,
    /// `#pubSearch` / `#strSearch` filters and the strings result (`filters.js`
    /// `renderPubList` / `onStringsFilter`).
    pub pub_query: String,
    pub str_query: String,
    /// `#typeSearch`'s committed text — the fuzzy filter over a declaration's
    /// **declared type** (`#typeSection`, v3-only).
    ///
    /// Kept on the host state, not in the renderer, for the same reason as
    /// `pub_query`: it survives selection changes, and a repaint re-reads it.
    /// [`Self::type_filter`] is the *exact* restriction chosen from a type chip;
    /// typing in the box clears it, because the query is then the authority.
    pub type_query: String,
    /// The declared type a chip restricted the list to, if any. `None` = every
    /// declared type. Set by a `.type-chip[data-key="type:<name>"]` press.
    pub type_filter: Option<String>,
    /// `(query, hit count)` after the last committed strings search. `None` =
    /// never searched, so `#strCount` stays empty rather than showing a
    /// fabricated zero.
    pub str_searched: Option<(String, usize)>,
    pub str_rows: Option<Vec<crate::model::PubRow>>,
    /// `#chainDepth` / `#chainDir` / `#chainFlatBtn` and the last engine chain
    /// answer. `chain` is the RAW `Index::chain` JSON, so `#chainTree` can only
    /// show nodes the engine actually walked.
    pub chain_depth: u32,
    pub chain_dir: String,
    pub chain_flat: bool,
    pub chain: Option<serde_json::Value>,
    /// Rows whose children are collapsed (`filters.js:610-619`'s
    /// `state.chain.collapsed`), keyed by node id.
    pub chain_collapsed: std::collections::BTreeSet<String>,
    /// `state.filters.groupChildren` (`core.js:270`), the flag behind the two
    /// mutually-inverse filter checkboxes.
    ///
    /// A *copy* of the single owner, `GraphView::group_children`
    /// (`app/src/view.rs:90`, written only through `set_group_children` at
    /// `:676`). It is copied here rather than read from the view because
    /// `facts_json_with` only ever sees a [`NzkUiState`]; the host refreshes it
    /// from the view on every pass, so the two cannot drift. The nzk_ui never
    /// writes it.
    ///
    /// Defaults to `true`, matching `index.html`'s shipped
    /// `#groupChildren checked` (`:253`) and `#hideChildren` unchecked (`:274`),
    /// so a default `NzkUiState` publishes the state the document is already in.
    pub group_children: bool,
    /// `state.filters.groupEdges` (`core.js:271`) — the flag behind
    /// `#groupEdges`, the *edges* counterpart of [`Self::group_children`] and a
    /// distinct control from it.
    ///
    /// A *copy* of the single owner, `GraphView::group_edges`
    /// (`app/src/view.rs`, written only through `set_group_edges`), copied here
    /// for the same reason as `group_children`: `facts_json_with` only ever sees
    /// a [`NzkUiState`], and the host refreshes it from the view on every pass,
    /// so the nzk_ui cannot disagree with the flag the graph was rebuilt from.
    ///
    /// Defaults to `true`, matching `index.html:255`'s shipped
    /// `#groupEdges checked` and `core.js:271`'s `groupEdges: true`. Note the box
    /// also ships `hidden` — but hidden is not absent: the flag is live and the
    /// document is already in the `true` state, so a default `NzkUiState`
    /// publishes the state the document shipped.
    pub group_edges: bool,
    /// Monotone count of accepted back-navigation steps.
    pub back_count: u64,
    /// Monotone count of accepted up-navigation steps.
    pub up_count: u64,
    /// The source pane's theme, as the `#srcModeBtn` cycle owns it. One owner:
    /// the host builds its `SourceTheme` from this rather than keeping a second
    /// copy of the choice.
    pub source_mode: SourceMode,
    /// Monotone count of accepted `#srcModeBtn` presses, so a press that only
    /// changes the theme is still observably a real action.
    pub source_mode_count: u64,
    /// Monotone count of accepted `#srcCloseBtn` presses.
    pub source_close_count: u64,
    /// Monotone count of accepted `#srcPopBtn` presses.
    pub source_pop_count: u64,
    /// Monotone count of accepted `#srcCopyBtn` presses — the request count.
    /// Whether bytes actually reached the clipboard is the *host's* answer, so
    /// this module counts requests, not successes, and never claims a copy it
    /// did not perform.
    pub source_copy_count: u64,
    /// Last effective action (debug/tests); `None` until one is accepted.
    pub last: Option<NzkUiAction>,
    /// The loading overlay's text while an index build runs off the paint
    /// thread; `None` = no build in flight (the shipped `#loading.hidden`
    /// document stays untouched). The host owns this flag because it owns the
    /// worker.
    pub indexing: Option<String>,
    /// Last accepted **picker** message (debug/tests), the
    /// [`PickerAction`] counterpart of [`NzkUiState::last`].
    ///
    /// The two are separate for the same reason the two enums are: a picker
    /// message carries a payload (`Go(String)`) that a `Copy` `NzkUiAction`
    /// cannot, and `#browseBtn`'s press is a `NzkUiAction` while everything
    /// inside the modal it opens is a `PickerAction`. Keeping one `last` field
    /// for both would mean discarding the payload on the way in.
    pub last_picker: Option<PickerAction>,
    /// The `#editArgs` box and its Ctrl/Cmd+Enter policy
    /// ([`crate::edit_args`]).
    ///
    /// The reference's edit modal is a real, shipped surface
    /// (`index.html:512-546`; `edit.js`, 296 lines) with no counterpart here, so
    /// the box's *meaning* is modelled in the pure module and owned here, where
    /// the modal's other state already lives.
    ///
    /// Crucially the dry-run/apply decision is a **value** produced by
    /// [`crate::edit_args::decide_key`], not a closure buried in a UI callback:
    /// the one thing that must never invert is that Ctrl/Cmd+Enter maps to
    /// `run(false)` (`edit.js:285`), so it can never be an apply.
    /// The edit modal's own state (`#editModal`, `edit.js`), for the
    /// `#editBtn` flow.
    ///
    /// This is the **one owner** of the modal's working view: the args box, the
    /// operation list, the selection, the busy flag, the output and the note.
    /// The standalone `edit_args` mirror that used to sit here was removed when
    /// this field landed — two [`crate::edit_args::EditArgsView`]s would have
    /// been two owners of the same text, and the one the modal never read would
    /// have been the one a future edit silently updated.
    ///
    /// Always present (like [`NzkUiState::hierarchy`]) and closed by default;
    /// [`NzkUiState::edit_facts`] returns `None` unless [`NzkUiState::surface`]
    /// is [`HostSurface::Edit`], so the modal's key stays out of the shipped
    /// document until `#editBtn` is pressed. See
    /// [`NzkUiState::open_edit`] for the seeding of the operation list.
    pub edit: crate::edit_host::EditHost,
    /// The Reorganise & export dialog's own state (`#reorgModal`) — the form
    /// values, the last engine answer, and `#reorgNote`.
    ///
    /// Same contract as [`NzkUiState::edit`]: always present, closed by
    /// default, and [`ReorgView::facts`] publishes nothing while the dialog has
    /// never been opened or run, so the shipped document stays byte-identical.
    pub reorg: crate::reorg_wire::ReorgView,
    /// The form-control interaction state (task-14): the reorg/preview/picker
    /// values that are editable, plus the open menu / focused input.
    ///
    /// Always present and idle by default; [`crate::form_wire::FormState::facts`]
    /// publishes nothing while idle, so the shipped document stays
    /// byte-identical.
    pub form: crate::form_wire::FormState,
    /// The change-preview panel's last built report (`#previewOut` /
    /// `#previewNote`), for the `#previewBtn` flow.
    ///
    /// `None` = nothing has been built yet. [`NzkUiState::preview_facts`]
    /// returns `None` unless [`NzkUiState::surface`] is
    /// [`HostSurface::Preview`], so the key is absent from the shipped paint and
    /// the default render stays byte-identical (`@frame 1600 1000
    /// 4bd131fa71b2ccc8`).
    pub preview: Option<crate::preview_wire::PreviewReport>,
    /// The graph selection, mirrored from [`crate::view::GraphView::selected`]
    /// once per host pass — the same one-owner copy contract as
    /// [`NzkUiState::group_children`].
    ///
    /// The nzk_ui publishes `ui.drawer` from it (content built by
    /// [`crate::drawer_wire`] from the live graph + this index). It is `None`
    /// until a node is selected, and a stale index is handled honestly by the
    /// builder returning `None` — no fabricated drawer.
    pub selected_node: Option<usize>,
    /// The graph's namespace list, mirrored from the live `GraphView` once per
    /// pass (the same one-owner-copy contract as [`NzkUiState::selected_node`]).
    ///
    /// `#nsSelect`'s rows are CONTENT: the form state cannot see a `Graph`, so
    /// the host copies the corpus' real `Node::ns` strings here and
    /// `NzkUiAction::FormOpen("#nsSelect")` turns them into the menu rows.
    /// Sorted/deduped by the host, so the menu order is the engine's own order.
    pub namespaces: Vec<String>,
    /// The graph's node-kind universe, mirrored from the live `GraphView` once
    /// per pass (same one-owner-copy contract as [`NzkUiState::namespaces`]).
    ///
    /// The sidebar's `kinds all`/`kinds none` buttons are
    /// [`FilterCmd::AllKinds`], and that transition needs the full set of kinds
    /// the corpus actually has. This display-free layer cannot see a `Graph`, so
    /// the host copies the sorted, deduped kind list here — exactly as it does
    /// for the namespace options. Empty means "nothing known yet", which makes
    /// `AllKinds(false)` an honest no-op rather than hiding an invented list.
    pub known_kinds: Vec<String>,
    /// The graph's edge-kind universe, mirrored like
    /// [`NzkUiState::known_kinds`] for `edges all`/`edges none`
    /// ([`FilterCmd::AllEdges`]).
    pub known_edges: Vec<String>,
    /// The project ids the graph knows, mirrored for
    /// [`FilterCmd::AllProjects`] — the Projects block's `all`/`none` links.
    pub known_projects: Vec<String>,
    /// The Blocks canvas hover peek (`crate::blocks_peek::Peek`, task-13). Its
    /// five spans are published as the top-level `blocks_peek` fact only while a
    /// card is hovered, so the shipped hidden tooltip is untouched otherwise.
    pub blocks_peek: crate::blocks_peek::Peek,
    /// True once a selection has been shown in the drawer, so the host can tell
    /// a *selection change* (open the drawer, `drawer.js:74-84`) from a steady
    /// selection (leave the user's close alone).
    pub drawer_seen_selection: Option<usize>,
    /// The docked drawer's own interactive state (task-2): which relation groups
    /// are expanded and which nodes hide their contains-children
    /// (`drawer.js:120-145`). Owned here so the facts are a function of it; the
    /// nzk_ui advances it from presses on the runtime-created crumb / ± / caret /
    /// prop-action rows through [`crate::nzk_ui`]'s staging queue.
    pub drawer_ui: crate::drawer_wire::DrawerUi,
}

impl Default for NzkUiState {
    fn default() -> Self {
        NzkUiState {
            camera: CameraState::default(),
            zoom_step: 1.25,
            zoom_min: 0.02,
            zoom_max: 8.0,
            full_graph: false,
            resettle_count: 0,
            reindex_count: 0,
            collapsed: Vec::new(),
            surface: None,
            drawer_open: None,
            find: crate::find_wire::FindView::default(),
            suggest: crate::find_wire::SuggestionView::default(),
            shortcuts: SHORTCUT_ROWS,
            filters: FilterState::default(),
            depth: crate::form_wire::DEPTH_DEFAULT,
            search_focused: false,
            tabs: crate::chords::tabs::Tabs::new(),
            picker: None,
            hierarchy: crate::app::hierarchy_wire::HierarchyView::new(),
            // `None`, not `Some(0)`: no engine query has been made yet, so the
            // honest state is "no count supplied" and the readout stays absent.
            pub_total: None,
            pub_list: None,
            type_query: String::new(),
            type_filter: None,
            ui_theme: "nzk-dark+".to_string(),
            source_facts: None,
            pub_query: String::new(),
            str_query: String::new(),
            str_searched: None,
            str_rows: None,
            chain_depth: 3,
            chain_dir: "forward".to_string(),
            chain_flat: false,
            chain: None,
            chain_collapsed: std::collections::BTreeSet::new(),
            // `index.html` ships `#groupChildren` checked (`:253`), which is
            // `groupChildren: true` (`core.js:270`) — the same default the single
            // owner `GraphView::group_children` uses (`view.rs:196`).
            group_children: true,
            // `index.html:255` ships `#groupEdges checked` and `core.js:271`
            // ships `groupEdges: true` — the same default the single owner
            // `GraphView::group_edges` uses in `view.rs`.
            group_edges: true,
            back_count: 0,
            up_count: 0,
            source_mode: SourceMode::default(),
            source_mode_count: 0,
            source_close_count: 0,
            source_pop_count: 0,
            source_copy_count: 0,
            last: None,
            indexing: None,
            last_picker: None,
            // `#editModal` ships with the `hidden` class (`index.html:512`), so
            // a fresh state is a CLOSED host; the args box keeps the reference's
            // literal `{}` (`index.html:531`) inside the host's own view.
            edit: crate::edit_host::EditHost::new(),
            // A fresh export dialog: the shipped form defaults, no engine
            // answer, no note — i.e. nothing published to the shell yet.
            reorg: crate::reorg_wire::ReorgView::default(),
            form: crate::form_wire::FormState::default(),
            // Nothing built yet, so `ui.preview` is absent until `#previewBtn`
            // opens the panel and the engine answers.
            preview: None,
            selected_node: None,
            namespaces: Vec::new(),
            // Nothing mirrored yet: `AllKinds(false)`/`AllEdges(false)` are then
            // honest no-ops instead of hiding an invented universe.
            known_kinds: Vec::new(),
            known_edges: Vec::new(),
            known_projects: Vec::new(),
            blocks_peek: crate::blocks_peek::Peek::new(),
            drawer_seen_selection: None,
            drawer_ui: crate::drawer_wire::DrawerUi::default(),
        }
    }
}

impl NzkUiState {
    /// The app's startup state: the sections `index.html` ships collapsed.
    ///
    /// Kept out of `Default` so unit tests and the stateless screenshot path see
    /// an empty set (`ui.collapsed = null`, which leaves the shipped document
    /// alone); the interactive app seeds this once so the **first toggle is a
    /// delta** rather than expanding the three shipped sections.
    pub fn shipped() -> NzkUiState {
        let mut state = NzkUiState::default();
        state.collapsed = DEFAULT_COLLAPSED.iter().map(|s| s.to_string()).collect();
        state
    }

    /// Is `section` (a DOM selector like `#filtersHead`) collapsed?
    pub fn is_collapsed(&self, section: &str) -> bool {
        self.collapsed.iter().any(|s| s == section)
    }

    /// Handle a keydown in the `#editArgs` box and return what the app must do.
    ///
    /// This is the live consumer of [`crate::edit_args::decide_key`]: the app's
    /// key routing calls this instead of testing modifiers itself, so there is
    /// exactly one place in the tree where Ctrl/Cmd+Enter becomes a run — and it
    /// is the place that hard-codes [`crate::edit_args::RunMode::Dry`]
    /// (`edit.js:285`'s literal `false`). A second, hand-rolled test at a call
    /// site is exactly how the dry run would drift into an apply.
    ///
    /// The returned note is `None` when the key was not handled, and the caller
    /// must then leave `#editNote` untouched.
    pub fn edit_args_keydown(
        &mut self,
        ev: crate::edit_args::KeyEvent,
    ) -> (crate::edit_args::KeyOutcome, Option<crate::edit_args::Note>) {
        // The modal's working view is the one owner of the args text (see
        // [`NzkUiState::edit`]); routing through it here is what keeps a key
        // pressed in `#editArgs` and the text the modal publishes the same
        // `EditArgsView`.
        self.edit.view_mut().args.keydown(ev)
    }

    /// Open the folder picker on `repo_root` and take its FIRST real listing.
    ///
    /// This is the missing half of `#browseBtn`. `NzkUiAction::Browse` set
    /// [`NzkUiState::surface`] and stopped, so the modal gained a box and a
    /// title while `#modalBody` stayed exactly as `index.html` shipped it
    /// (empty) and `NzkUiState::picker` stayed `None` forever. A modal that
    /// opens onto an empty body is not "the picker" — it is the frame of one.
    ///
    /// ### Why `go("")` and not `PickerView::rooted_at`
    ///
    /// [`crate::find_wire::PickerView::new`] deliberately lists **nothing** (its
    /// own docs: "a `Picker::new` does not read the filesystem, so claiming a
    /// listing before one was requested would make the modal assert something no
    /// `read_dir` backed"), and `facts()` returns `None` until a listing exists.
    /// So the open path must request one, and the reference's request is
    /// `browseTo(startPath || '')` (`shell.js:96`) — the empty string, which the
    /// picker core resolves to the repo root (`picker.rs` `resolve_input`).
    ///
    /// `start` is the caller's `state.rootChoice` when it has one
    /// (`shell.js:473`: `openPicker(state.rootChoice && state.rootChoice !== '.'
    /// ? state.rootChoice : '')`). Pass `""` when there is no remembered choice.
    ///
    /// Returns the resulting listing, so a caller can assert on what the modal
    /// will actually show rather than on the fact that a function ran.
    pub fn open_picker(&mut self, repo_root: &Path, start: &str) -> &Listing {
        let mut view = crate::find_wire::PickerView::new(repo_root, start);
        // THE first listing: this is what makes `facts()` non-`None`, and
        // therefore what makes `nzk_ui.rs` publish `ui.picker` at all.
        view.go(start);
        self.picker = Some(view);
        self.surface = Some(HostSurface::FolderPicker);
        self.drawer_open = None;
        self.last = Some(NzkUiAction::Browse);
        // Safe to unwrap by construction: `view` was just assigned, and `go`
        // always stores a listing.
        self.picker.as_ref().expect("picker was just set").current()
    }

    /// The facts payload the nzk_ui renders from: the collapsed list in a
    /// stable order, so the same state always produces the same nzk_ui pixels.
    pub fn collapsed_facts(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.collapsed.iter().map(String::as_str).collect();
        out.sort_unstable();
        out
    }

    /// The open drawers, in nzk_ui's `UiState::drawers_open` shape.
    ///
    /// At most one, but a list because that is the wire shape (and the shape
    /// that lets nzk_ui's allow-list reject anything else). Empty means every
    /// drawer is shut and every drawer box is `0 0 0 0` — i.e. unreachable.
    ///
    /// The id is checked against [`DRAWER_IDS`] rather than passed through, so a
    /// `ToggleDrawer("reorgModal")` cannot smuggle a modal into the drawer
    /// channel. nzk_ui would ignore the id anyway; this makes the separation
    /// hold on *this* side of the wire too, so the two facts can never disagree
    /// about which family an overlay belongs to.
    ///
    /// A drawer surface reached through [`NzkUiState::surface`] (`#findPanel`)
    /// is folded in here rather than getting a second channel, so there is
    /// exactly one id per open overlay no matter which control opened it.
    pub fn drawer_facts(&self) -> Vec<&'static str> {
        if let Some(id) = self.drawer_open.filter(|id| DRAWER_IDS.contains(id)) {
            return vec![id];
        }
        match self.surface.map(HostSurface::element) {
            Some(id) if DRAWER_IDS.contains(&id) => vec![id],
            _ => Vec::new(),
        }
    }

    /// The open modals, in nzk_ui's `UiState::modals_open` shape.
    ///
    /// At most one, for the same wire reason as [`NzkUiState::drawer_facts`].
    /// Empty means every modal keeps its shipped `hidden` class, every modal box
    /// is `0 0 0 0`, and therefore no control inside a modal is reachable.
    pub fn modal_facts(&self) -> Vec<&'static str> {
        match self.surface.map(HostSurface::element) {
            Some(id) if !DRAWER_IDS.contains(&id) => vec![id],
            _ => Vec::new(),
        }
    }

    /// Open `#editModal` and SEED its operation list from the engine's own tool
    /// constants (`edit.js:232-245` fetches `GET /api/edit`).
    ///
    /// This is the missing half of `#editBtn`, exactly as
    /// [`NzkUiState::open_picker`] is the missing half of `#browseBtn`.
    /// [`NzkUiAction::Edit`] used to set [`NzkUiState::surface`] and stop, so
    /// the modal un-hid onto a body `index.html` shipped empty — `#editTool`
    /// listed no operations and `#editArgs` had no owner. The operation list is
    /// taken from [`crate::edit_transport::tool_names`], i.e. from
    /// `csgraph_core::edit::TOOLS` ++ `AUX_TOOLS`, so the list the modal shows
    /// cannot drift from the tools the in-process transport can actually route.
    ///
    /// The visibility owner stays [`NzkUiState::surface`] (that is what
    /// `modal_facts` serialises); the host's own open flag is brought into step
    /// here, and [`NzkUiState::edit_facts`] is gated on `surface` as well, so
    /// the two can never disagree about what the document carries.
    pub fn open_edit(&mut self) {
        self.edit.open();
        let names = crate::edit_transport::tool_names();
        let count = names.len();
        // The wire shape `EditToolView::load_ops` expects (see `edit_wire.rs`),
        // built from the engine constants rather than a hand-copied literal.
        let _ = self.edit.view_mut().load_ops(&serde_json::json!({
            "ok": true,
            "count": count,
            "tools": names,
        }));
        self.surface = Some(HostSurface::Edit);
        self.drawer_open = None;
    }

    /// Close `#editModal` — the counterpart of [`NzkUiState::open_edit`].
    ///
    /// The working view is deliberately **not** reset (`edit.js` hides the
    /// element and keeps its state), so reopening shows what the user left.
    pub fn close_edit(&mut self) {
        self.edit.close();
        if self.surface == Some(HostSurface::Edit) {
            self.surface = None;
        }
    }

    /// The edit modal's facts document, or `None` while the modal is not the
    /// open surface.
    ///
    /// Gated on [`NzkUiState::surface`] — the one owner of which overlay is up
    /// — rather than on the host's internal flag, so a `ToggleModal(Shortcuts)`
    /// that replaces the edit dialog cannot leave a stale `edit` key in the
    /// document after the element has been given the `hidden` class back.
    pub fn edit_facts(&self) -> Option<serde_json::Value> {
        if self.surface != Some(HostSurface::Edit) {
            return None;
        }
        self.edit.facts()
    }

    /// The `#previewModal` CONTENT (`#previewOut` / `#previewNote`), for the
    /// `#previewBtn` flow.
    ///
    /// `None` while the panel is not the open surface: the key is then absent,
    /// so `#previewOut`/`#previewNote` keep the shipped document's empty state
    /// and the default render stays byte-identical. The content itself is the
    /// in-process engine answer [`crate::preview_wire::PreviewReport`] — this is
    /// a transport of that answer, never copy written here.
    pub fn preview_facts(&self) -> Option<serde_json::Value> {
        if self.surface != Some(HostSurface::Preview) {
            return None;
        }
        self.preview.as_ref().map(crate::preview_wire::PreviewReport::facts)
    }
}

/// Decide what a `#previewRun` press (or Enter in `#previewTo`) must do.
///
/// `preview.js:188-199`: an empty `#previewTo` is the reference's own note and
/// **no** request is made; a non-empty target becomes [`Effect::preview_run`],
/// which the host performs in process. One function so the button and the
/// keystroke cannot decide differently.
/// Mirror the `#nsSelect` value the form committed into the ONE filter model.
///
/// The view predicate consumes `FilterState::namespace` (via `view.set_filters`,
/// pushed by `apply_and_execute` after every dispatch) and the shell paints the
/// same value from `ui.filters`; the form's committed text is the input to both.
/// Only touched when the menu/Enter actually changed something, so reorg-only
/// form edits can never reset the namespace filter.
fn sync_form_filters(state: &mut NzkUiState) {
    let ns = state.form.current("#nsSelect");
    if state.filters.namespace != ns {
        state.filters.namespace = ns;
    }
}

fn request_preview_run(state: &mut NzkUiState, effect: &mut Effect) {
    if state.form.preview_to.trim().is_empty() {
        state.preview = Some(crate::preview_wire::missing_target());
    } else {
        effect.preview_run = true;
    }
    effect.repaint_nzk_ui = true;
}

/// Mirror the form's theme / chain values into the host state and report the
/// transitions the host must apply.
///
/// The chain request model is one owner ([`NzkUiState::chain_depth`] /
/// `chain_dir`), so a committed select cannot quietly diverge from the values
/// `#chainRun` reads.
fn sync_display_edits(state: &mut NzkUiState, effect: &mut Effect) {
    if state.form.ui_theme != state.ui_theme {
        state.ui_theme = state.form.ui_theme.clone();
        effect.ui_theme = Some(state.ui_theme.clone());
    }
    let depth = state.form.chain_depth;
    let dir = state.form.chain_dir.clone();
    if depth != state.chain_depth || dir != state.chain_dir {
        state.chain_depth = depth;
        state.chain_dir = dir.clone();
        effect.chain_config = Some((depth, dir));
    }
    if state.form.source_theme != state.source_mode.label() {
        if let Some(mode) = SourceMode::from_theme_name(&state.form.source_theme) {
            state.source_mode = mode;
            effect.source_theme = true;
            effect.repaint_nzk_ui = true;
        }
    }
}

/// Mirror `#pubSearch` / `#strSearch` into the host state.
///
/// The public-definitions filter is pure (it re-filters the already-loaded
/// rows, `filters.js:504-512`), so it applies on every keystroke. The strings
/// filter needs the engine (`/api/search` + kind == "string"), so `str_commit`
/// is true only for Enter / click-away — the reference's debounce is replaced by
/// an explicit commit rather than a timer thread. An emptied strings box clears
/// the count and the list immediately (`filters.js:523-528`).
pub(crate) fn sync_search_edits(state: &mut NzkUiState, effect: &mut Effect, str_commit: bool) {
    // Read the VISIBLE value (the in-flight edit buffer while focused), not the
    // last committed model value: the public list re-filters on every keystroke,
    // and the strings box must show what is typed even before Enter commits it.
    let pub_now = state.form.value("#pubSearch");
    if pub_now != state.pub_query {
        state.pub_query = pub_now;
        effect.repaint_nzk_ui = true;
    }
    let str_now = state.form.value("#strSearch");
    if str_now != state.str_query {
        state.str_query = str_now;
        if state.str_query.trim().is_empty() {
            state.str_searched = Some((String::new(), 0));
            state.str_rows = None;
        } else if str_commit {
            effect.str_search = Some(state.str_query.clone());
        }
        effect.repaint_nzk_ui = true;
    } else if str_commit && !state.str_query.trim().is_empty() {
        effect.str_search = Some(state.str_query.clone());
    }
    // `#typeSearch` is pure too: the declared types and the rows are already in
    // the resident graph, so every keystroke re-filters them with no engine
    // round trip. Typing CLEARS an exact chip restriction — otherwise the chip
    // and the query could silently both be active and the rows would be the
    // intersection of two filters the user cannot see.
    let type_now = state.form.value("#typeSearch");
    if type_now != state.type_query {
        state.type_query = type_now;
        state.type_filter = None;
        effect.repaint_nzk_ui = true;
    }
}

/// Apply a Declared-types chip press (`data-key = "type:<name>"`).
///
/// The write half of `nzk_ui::type_chip_click`. An empty `<name>` is the `all`
/// chip and clears both the exact restriction and the box text; a name sets the
/// exact restriction *and* mirrors it into `#typeSearch`, so the visible filter
/// and the active rows can never disagree (typing then clears the restriction —
/// see [`sync_search_edits`]).
pub fn set_type_filter(state: &mut NzkUiState, name: &str) {
    let name = name.trim();
    if name.is_empty() {
        state.type_filter = None;
        state.type_query.clear();
    } else {
        state.type_filter = Some(name.to_string());
        state.type_query = name.to_string();
    }
    state.form.set_value("#typeSearch", &state.type_query);
}

/// Take a committed `#rootInput` and turn it into the host's re-index request.
///
/// The reference writes the typed path to `state.rootChoice` and lets the Index
/// press use it (`shell.js:493-499`); this host owns the engine, so a committed
/// path is resolved against the current repo (the same `resolve_input` the
/// picker uses) and carried on [`Effect::reindex_at`].
/// Mutate `state` for `action` and report what the host must do.
///
/// Pure in the only sense that matters here: no egui, no windows, no GPU. It
/// *does* touch the filesystem for two actions — [`NzkUiAction::Browse`] lists
/// the folder the picker opens on, because a modal opened onto an empty body is
/// not the picker (see [`NzkUiState::open_picker`]), and Enter in
/// `#modalPathInput` browses the typed path through [`apply_picker`]'s `Go`
/// (`shell.js:752-753`). `repo` is the corpus root, which the host owns
/// (`GraphView::graph.repo`); it is the folder the picker is rooted at and the
/// folder `#modalRepo` navigates back to, so there is no honest default and it
/// is a parameter rather than a guess.
///
/// `NzkUiAction::None` is an explicit no-op (inert control), never a panic.
pub fn apply(state: &mut NzkUiState, action: NzkUiAction, repo: &Path) -> Effect {
    let mut effect = Effect::default();
    // Keep the whole action for `state.last`: the `Filter` arm destructures its
    // payload by value, so the original cannot be stored after the match.
    // Cloning once here is semantically identical (same action recorded) and
    // keeps the tail of this function untouched.
    let last_action = action.clone();
    // A click-away from an open form menu / focused box closes the popup and is
    // SWALLOWED, exactly like a native `<select>`: the press that dismisses the
    // menu must not also press the control underneath it. Form presses are the
    // only exceptions (they replace the popup).
    if state.form.is_active()
        && !matches!(
            action,
            NzkUiAction::FormOpen(_)
                | NzkUiAction::FormKey(_)
                | NzkUiAction::FormMenuCommit(_)
                | NzkUiAction::FocusSearch
                | NzkUiAction::Index
                // `#previewRun` commits the focused box first (a browser blurs
                // the input before the click), so it must not be swallowed as a
                // click-away — otherwise Run needs two presses while typing.
                | NzkUiAction::PreviewRun
                // Stop must always reach the running build, even if a sidebar
                // control happened to hold focus when the overlay appeared.
                | NzkUiAction::StopIndex
        )
    {
        let old_depth = state.depth;
        let was_str_search = state.form.focus == Some("#strSearch");
        state.form.commit_focused();
        state.form.sync_reorg(&mut state.reorg.request);
        sync_form_filters(state);
        state.depth = state.form.depth;
        if state.depth != old_depth {
            effect.depth = Some(state.depth);
        }
        sync_display_edits(state, &mut effect);
        sync_search_edits(state, &mut effect, was_str_search);
        effect.repaint_nzk_ui = true;
        return effect;
    }
    let should_blur_form = match &action {
        NzkUiAction::FormOpen(id) => state.form.focus != Some(*id),
        NzkUiAction::FocusSearch | NzkUiAction::Index => true,
        _ => false,
    };
    if should_blur_form && state.form.is_active() {
            let old_depth = state.depth;
            let was_str_search = state.form.focus == Some("#strSearch");
            state.form.commit_focused();
            state.form.sync_reorg(&mut state.reorg.request);
            sync_form_filters(state);
            state.depth = state.form.depth;
            if state.depth != old_depth {
                effect.depth = Some(state.depth);
            }
            sync_display_edits(state, &mut effect);
            sync_search_edits(state, &mut effect, was_str_search);
    }
    match action {
        // A document-window request is consumed by the HOST before this
        // display-free layer (it is the only layer that owns a window registry);
        // if it ever reaches here it is an explicit no-op.
        NzkUiAction::NewDocWindow => {}
        NzkUiAction::Filter(ref cmd) => {
            let changed = match cmd {
                FilterCmd::ToggleKind(kind) => {
                    if !state.filters.hidden_kinds.remove(kind) {
                        state.filters.hidden_kinds.insert(kind.clone());
                    }
                    true
                }
                FilterCmd::ToggleEdge(kind) => {
                    if !state.filters.hidden_edges.remove(kind) {
                        state.filters.hidden_edges.insert(kind.clone());
                    }
                    true
                }
                FilterCmd::ToggleProject(id) => {
                    if !state.filters.hidden_projects.remove(id) {
                        state.filters.hidden_projects.insert(id.clone());
                    }
                    true
                }
                FilterCmd::AllKinds(on) => {
                    // `filters.js:385-392` `setAllKinds`: rebuild the visible set
                    // from the kind universe. The universe is the host-mirrored
                    // graph list; empty means "nothing known", so hiding every
                    // kind is a no-op rather than a fabricated one.
                    if *on {
                        state.filters.hidden_kinds.clear();
                    } else {
                        state.filters.hidden_kinds =
                            state.known_kinds.iter().cloned().collect();
                    }
                    true
                }
                FilterCmd::AllEdges(on) => {
                    // `filters.js:394-397` `setAllEdges`.
                    if *on {
                        state.filters.hidden_edges.clear();
                    } else {
                        state.filters.hidden_edges =
                            state.known_edges.iter().cloned().collect();
                    }
                    true
                }
                FilterCmd::AllProjects(on) => {
                    // `filters.js` `setAllProjects` — the project chip row's
                    // "all"/"none". `hidden_projects` is the inverse set.
                    if *on {
                        state.filters.hidden_projects.clear();
                    } else {
                        state.filters.hidden_projects =
                            state.known_projects.iter().cloned().collect();
                    }
                    true
                }
                FilterCmd::SetNamespace(ns) => {
                    state.filters.namespace = ns.clone();
                    state.form.set_value("#nsSelect", ns);
                    true
                }
                FilterCmd::AllNamespaces => {
                    // `setNamespaceFilter('')` (`shell.js:601`). The select's own
                    // displayed value is the second view of the same state, so it
                    // is cleared through the one value writer.
                    state.filters.namespace = String::new();
                    state.form.set_value("#nsSelect", "");
                    true
                }
                FilterCmd::SetNsDeep(on) => {
                    state.filters.ns_deep = *on;
                    true
                }
                FilterCmd::FlipNsDeep => {
                    state.filters.ns_deep = !state.filters.ns_deep;
                    true
                }
                FilterCmd::FlipHideIsolated => {
                    state.filters.hide_isolated = !state.filters.hide_isolated;
                    true
                }
                FilterCmd::FlipVtMode => {
                    state.filters.vt_mode = !state.filters.vt_mode;
                    true
                }
                FilterCmd::FlipFocusParents => {
                    state.filters.focus_parents = !state.filters.focus_parents;
                    if state.filters.focus_parents {
                        state.filters.focus_only = false;
                    }
                    true
                }
                FilterCmd::FlipGroupChildren => {
                    state.group_children = !state.group_children;
                    effect.group_children = Some(state.group_children);
                    // The view owns the rebuild for this flag (`set_group_children`
                    // rebuilds), so the general filter rebuild must not also run.
                    false
                }
                FilterCmd::FlipGroupEdges => {
                    state.group_edges = !state.group_edges;
                    effect.group_edges = Some(state.group_edges);
                    false
                }
                FilterCmd::Reset => {
                    state.filters = FilterState::default();
                    // The select's own value is a second view of the same state;
                    // resetting one without the other would leave `#nsSelect`
                    // showing a namespace the predicate no longer filters by.
                    state.form.set_value("#nsSelect", "");
                    // `filters.js:406-419` resetFilters also restores both group
                    // flags. Those live on the view, so they travel as effects.
                    state.group_children = true;
                    state.group_edges = true;
                    effect.group_children = Some(true);
                    effect.group_edges = Some(true);
                    true
                }
                // Model-only layer: `app.rs` applies the real deletion through
                // `GraphView::prune_hidden` after this arm (see `FilterCmd::Prune`),
                // so no model state may change here.
                FilterCmd::Prune => false,
            };
            effect.rebuild_view = changed;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::None => return effect,
        NzkUiAction::Fit => {
            effect.refit = true;
        }
        NzkUiAction::ZoomIn => {
            let z = (state.camera.zoom * state.zoom_step).clamp(state.zoom_min, state.zoom_max);
            if z != state.camera.zoom {
                state.camera.zoom = z;
                effect.camera = true;
            }
        }
        NzkUiAction::ZoomOut => {
            let z = (state.camera.zoom / state.zoom_step).clamp(state.zoom_min, state.zoom_max);
            if z != state.camera.zoom {
                state.camera.zoom = z;
                effect.camera = true;
            }
        }
        NzkUiAction::FullGraph => {
            state.full_graph = true;
            effect.rebuild_view = true;
            // The visible set jumps from namespace roots to the whole model: the
            // nodes the user was looking at are a tiny island in a set the layout
            // has never placed. Continuing from their positions would anchor the
            // island and drop the rest on the origin, so "Full graph" re-seeds —
            // the reference's `loadFullGraph` does the same (`graph.js:742`:
            // `clearGraph()` then `seedNewNodes`).
            effect.reseed_layout = true;
            effect.refit = true;
        }
        NzkUiAction::Resettle => {
            state.resettle_count += 1;
            effect.rebuild_view = true;
            effect.relayout = true;
            // An explicit re-settle is a hard reset, not the incremental
            // "continue" branch `relayout` takes by default: over an unchanged
            // set the continue run anchors every node and moves nothing, which
            // is exactly the "Re-settle does nothing" defect.
            effect.reseed_layout = true;
            // A re-seed moves the whole arrangement, so refit for the same
            // reason "Full graph" does: without it the new disc can land off
            // the camera the old arrangement was framed by, and the press looks
            // dead even though the layout really ran.
            effect.refit = true;
        }
        NzkUiAction::Index => {
            let root = state.form.value("#rootInput");
            if !root.trim().is_empty() {
                effect.reindex_at = Some(crate::picker::resolve_input(&root, repo));
            }
            state.form.commit_focused();
            state.reindex_count += 1;
            effect.reindex = true;
            effect.rebuild_view = true;
            effect.refit = true;
        }
        NzkUiAction::ToggleSection(section) => {
            let mut expanded = false;
            if let Some(i) = state.collapsed.iter().position(|s| s == section) {
                state.collapsed.remove(i);
                expanded = true;
            } else {
                state.collapsed.push(section.to_string());
            }
            // Expanding `#pubSection` is what triggers the lazy load in the
            // reference: `shell.js:556` runs `if (sec === pubSectionEl)
            // loadDefinitions();` inside the section's own click handler, i.e.
            // only on the expand edge (the `!collapsed` branch) — collapsing the
            // section again does NOT refetch. `expanded` reproduces that edge
            // exactly, so the engine is asked once per open, not once per click.
            //
            // The host does the fetch (it owns the `Index`); this module only
            // records that the edge happened, exactly as `reindex` carries
            // `#indexBtn` out of here.
            if expanded && section == "#pubHead" {
                effect.load_public_definitions = true;
            }
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::OpenBlocks => {
            // `#blocksBtn` toggles; `#blocksClose` closes. Both are the Blocks
            // pane, so the host decides open-vs-close from the request.
            effect.pane = Some(HostPane::Blocks);
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::NavigateBack => {
            // Pure counter: the history stack lives in the view, and this module
            // must not grow a second copy of it.
            state.back_count += 1;
            effect.navigate_back = true;
        }
        NzkUiAction::NavigateUp => {
            state.up_count += 1;
            effect.navigate_up = true;
        }
        NzkUiAction::NavigateForward => {
            // Pure counter, like Back: the stack stays in the view. `do_count`
            // rather than a new field — the reference has no separate forward
            // counter, and the two movements are the same gesture in opposite
            // directions.
            state.back_count += 1;
            effect.navigate_forward = true;
        }
        NzkUiAction::ModeGraph => {
            // Leaving Find: the results surface closes and its drawer with it.
            state.surface = None;
            state.drawer_open = None;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::ModeFind => {
            // Find IS the `#findPanel` drawer surface, so this replaces whatever
            // drawer was open rather than leaving a stale id that
            // `drawer_facts()` would prefer over it.
            state.surface = Some(HostSurface::Find);
            state.drawer_open = None;
            // `f` is `setMode('find', true)` (shell.js:459), and the second
            // argument focuses + selects the shared input. Without this flag the
            // find-mode branch of `search_key` was unreachable in a live session
            // (7A gap 2); the reference can reach it, so the focus handover is
            // wired here rather than invented.
            state.search_focused = true;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::FocusSearch => {
            // `shell.js:452-455`: `setMode('graph', false)` then
            // `searchInputEl.focus()`. The first half is the same clearance
            // `ModeGraph` performs, so it is written the same way rather than
            // re-derived; the second half is the keyboard handover.
            //
            // `setMode('graph', false)` is passed `false` for its focus
            // argument, so the box is focused by the NEXT statement, not by
            // `setMode` — which is why the two effects land together here and
            // why a caller cannot get one without the other.
            state.surface = None;
            state.drawer_open = None;
            state.form.close();
            state.search_focused = true;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::Arrange(axis) => {
            effect.arrange = Some(axis);
        }
        NzkUiAction::Spacing(dir) => {
            effect.spacing = Some(dir);
        }
        NzkUiAction::Tab(ref cmd) => {
            // `shell.js:410-431`. No effect flag: like the picker, the whole
            // observable change is inside `NzkUiState`, so the host repaints
            // from state it already holds. Each call's return is the reference's
            // own answer (the activated key, the closed keys, the new split
            // flag); the nzk_ui has no second consumer for them yet — the tab
            // bar is not rendered — so they are discarded explicitly rather than
            // silently.
            match cmd {
                TabCmd::Cycle(delta) => {
                    let _ = state.tabs.cycle(*delta);
                }
                TabCmd::CloseActive => {
                    let _ = state.tabs.close_active();
                }
                TabCmd::CloseOthers => {
                    let _ = state.tabs.close_others();
                }
                TabCmd::ToggleSplit => {
                    let _ = state.tabs.toggle_split();
                }
                TabCmd::Activate(key) => {
                    // `activateByKey` (`shell.js:1269-1272`): re-opening an
                    // already-open key is exactly what `Tabs::open` does to an
                    // existing row — it merges the params and sets the active
                    // key — so no parallel "activate" model is introduced. A key
                    // that is not open is a stale press and is ignored.
                    if let Some(params) = state.tabs.params(key).cloned() {
                        let _ = state.tabs.open(key, serde_json::Value::Object(params));
                    }
                }
                TabCmd::Close(key) => {
                    // `closeTab(key)` (`shell.js:1316-1338`). The model closes
                    // the ACTIVE document, so a non-active row is first activated
                    // through the same `Tabs::open` path and then closed; both
                    // steps are the reference's own operations, and a key that
                    // is not open is ignored.
                    if state.tabs.active_key() != Some(key.as_str()) {
                        if let Some(params) = state.tabs.params(key).cloned() {
                            let _ = state.tabs.open(key, serde_json::Value::Object(params));
                        }
                    }
                    if state.tabs.active_key() == Some(key.as_str()) {
                        let _ = state.tabs.close_active();
                    }
                }
            }
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::Browse => {
            // `#browseBtn` (`shell.js:472-474`) -> `openPicker(...)` (:94) ->
            // `openModal()` (:11) + `browseTo(start || '')` (:96).
            //
            // This arm used to do only the `openModal()` half: it set `surface`
            // and left `NzkUiState::picker` at `None`, so `#modal` un-hid onto a
            // body that `index.html` shipped empty and no listing ever reached
            // the renderer. `open_picker` performs the `browseTo` too, which is
            // what makes `ui.picker` non-`None` and therefore what makes
            // `nzk_ui::dom::apply_picker` (dom.rs:1393) run at all.
            //
            // The start path is the repo root, not a remembered `rootChoice`:
            // the native app re-reads its root from the corpus it actually
            // indexed, so an empty start resolves to that same folder
            // (`picker.rs::resolve_input`) instead of a second copy of the
            // choice that could disagree with the graph on screen.
            state.open_picker(repo, "");
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::Export => {
            state.surface = Some(HostSurface::Export);
            state.drawer_open = None;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::ReorgRun(action) => {
            // A press on one of the export dialog's foot buttons
            // (`reorg.js:172-200`). Guarded on the modal being the open surface,
            // exactly like `EditRun`: a press that arrives after the dialog was
            // closed has nothing to run. The request is recorded here; the
            // engine call happens in `app.rs`, which owns the `Index`.
            if state.surface == Some(HostSurface::Export) {
                state.reorg.begin(action);
                effect.repaint_nzk_ui = true;
            }
        }
        NzkUiAction::FormOpen(id) => {
            state.search_focused = false;
            // A press on a form control. `FormState::click` is the ONE decision
            // (menu / focus / toggle); the reorg request model is then re-synced
            // from the form so the next engine run uses the value on screen.
            //
            // `#previewOp`'s rows are the engine's accepted ops
            // (`csgraph_core::preview`'s own list, `preview_wire::OPS`), not the
            // shipped markup's static options — fed to the model before opening.
            if id == "#previewOp" {
                state.form.set_preview_ops(
                    crate::preview_wire::OPS
                        .iter()
                        .map(|op| {
                            let label = match *op {
                                "rename" => "Rename",
                                "retype" => "Retype",
                                other => other,
                            };
                            (op.to_string(), label.to_string())
                        })
                        .collect(),
                );
            }
            if id == "#nsSelect" {
                // `filters.js` `buildNamespace`/`#nsSelect`: the rows are the
                // graph's namespaces with the reference's count on "All
                // namespaces". Fed to the model before opening, and the menu
                // highlights the filter's current value.
                let namespaces = state.namespaces.clone();
                let mut rows: Vec<(String, String)> =
                    Vec::with_capacity(namespaces.len() + 1);
                rows.push((
                    String::new(),
                    format!("All namespaces ({})", namespaces.len()),
                ));
                for ns in &namespaces {
                    rows.push((ns.clone(), ns.clone()));
                }
                state.form.set_ns_options(rows);
                state.form.set_value("#nsSelect", &state.filters.namespace.clone());
            }
            if id == "#depthSelect" {
                state.form.set_value("#depthSelect", &state.depth.to_string());
            }
            if id == "#rootInput" && state.form.root.is_empty() {
                // Seed the box with the live repo WITHOUT committing it: focusing
                // the field must not re-index the corpus by itself.
                state.form.seed_root(&repo.to_string_lossy());
            }
            if id == "#modalPathInput" && state.form.modal_path.is_empty() {
                // The picker paints the listed path in this box; focusing it must
                // start from that SAME text, not from an empty buffer. Otherwise
                // the click visibly blanks the path and a typed relative folder
                // resolves against the repo instead of editing what was shown.
                if let Some(shown) = state
                    .picker
                    .as_ref()
                    .and_then(|pk| pk.facts())
                    .and_then(|f| f.get("path_input").and_then(|v| v.as_str()).map(str::to_string))
                {
                    state.form.set_value("#modalPathInput", &shown);
                }
            }
            if id == "#editTool" {
                // `edit.js:196-201` repopulates the select from the engine's tool
                // list; the current selection is the view's own.
                if let Some(tools) = state.edit.view().tools().map(|t| t.to_vec()) {
                    state.form.set_edit_tools(&tools);
                }
                if let Some(sel) = state.edit.view().selected() {
                    state.form.set_value("#editTool", sel);
                }
            }
            if id == "#uiTheme" {
                state.form.set_value("#uiTheme", &state.ui_theme.clone());
            }
            if id == "#sourceTheme" {
                state.form.set_value("#sourceTheme", state.source_mode.label());
            }
            if id == "#chainDepth" {
                state.form.set_value("#chainDepth", &state.chain_depth.to_string());
            }
            if id == "#chainDir" {
                state.form.set_value("#chainDir", &state.chain_dir.clone());
            }
            if id == "#pubSearch" {
                state.form.set_value("#pubSearch", &state.pub_query.clone());
            }
            if id == "#strSearch" {
                state.form.set_value("#strSearch", &state.str_query.clone());
            }
            if state.form.click(id) {
                state.form.sync_reorg(&mut state.reorg.request);
                effect.repaint_nzk_ui = true;
            }
        }
        NzkUiAction::FormKey(key) => {
            // One key while a form control owns the keyboard (routed by
            // `App::form_key`). A changed value re-syncs the request model.
            let old_depth = state.depth;
            let was_str_search = state.form.focus == Some("#strSearch");
            let enter = key == crate::form_wire::Key::Enter;
            if state.form.key(key) == crate::form_wire::KeyOutcome::Changed {
                state.form.sync_reorg(&mut state.reorg.request);
                sync_form_filters(state);
                state.depth = state.form.depth;
                if state.depth != old_depth {
                    effect.depth = Some(state.depth);
                }
                sync_display_edits(state, &mut effect);
                sync_search_edits(state, &mut effect, was_str_search && enter);
                effect.repaint_nzk_ui = true;
                // Enter in `#previewTo` is the reference's Run gesture;
                // `form_wire` records the one-shot request, and the SAME
                // decision the `#previewRun` press makes happens here.
                if state.form.take_preview_run() && state.surface == Some(HostSurface::Preview) {
                    request_preview_run(state, &mut effect);
                }
                // Enter in `#rootInput` is the reference's `doIndex()`
                // (`shell.js:494-496`): the typed folder is committed and the
                // host re-indexes it. A blur commit deliberately does not, so a
                // click elsewhere cannot start a build the user did not ask for.
                if let Some(root) = state.form.take_root_commit() {
                    effect.reindex_at = Some(crate::picker::resolve_input(&root, repo));
                }
                // Enter in `#modalPathInput` is `browseTo(value)`
                // (`shell.js:752-753`). The picker's own `Go` handler performs
                // the same `read_dir`, so there is one navigation path.
                if state.form.take_modal_go() {
                    let text = state.form.value("#modalPathInput");
                    let picker_effect = apply_picker(state, PickerAction::Go(text));
                    effect.repaint_nzk_ui |= picker_effect.repaint_nzk_ui;
                    if picker_effect.reindex_at.is_some() {
                        effect.reindex_at = picker_effect.reindex_at;
                    }
                }
            }
        }
        NzkUiAction::FormMenuCommit(index) => {
            // A press on an open menu row (`#csFormMenuOption<i>`, resolved by
            // `nzk_ui::form_menu_click`). Commits through the same value writer as
            // Enter, then re-syncs the request model.
            let old_depth = state.depth;
            let choice = state.form.menu_choice(index);
            if state.form.commit_index(index) {
                if let Some((id, value)) = choice {
                    if id == "#editTool" {
                        effect.edit_tool = Some(value);
                    }
                }
                state.form.sync_reorg(&mut state.reorg.request);
                sync_form_filters(state);
                state.depth = state.form.depth;
                if state.depth != old_depth {
                    effect.depth = Some(state.depth);
                }
                sync_display_edits(state, &mut effect);
                effect.repaint_nzk_ui = true;
            }
        }
        NzkUiAction::Edit => {
            // `#editBtn` (`edit.js:249-252`) -> open `#editModal` AND populate
            // it. Setting `surface` alone gave the dialog a box and a title while
            // `#editTool`/`#editArgs` stayed as `index.html` shipped them; the
            // seeding lives in `open_edit` so every open path gets it.
            state.open_edit();
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::SourceOpen => {
            // `#nodeCodeBtn`: v2 `viewCodeFor(state.selected)` →
            // `openSource({ node: id })` (`drawer.js:29-32`, wired at
            // `shell.js:563`). The host owns the source pane, so this displays
            // free layer only decides *whether* an open is wanted. No selection
            // means nothing to open (v2 toasts and returns); a real selection is
            // carried as the request and the host opens that node in process.
            effect.source_open = state.selected_node.is_some();
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::EditRun(mode) => {
            // A press on `#editDry` / `#editApply` (`edit.js:276-277`). The
            // request is built by the view's own producers — `press_apply` is the
            // SOLE constructor of `RunMode::Apply` in this tree, and this arm
            // must never assemble an apply body itself, or the busy / no-tool /
            // args-parse guards it owns would be skipped.
            //
            // Guarded on the modal actually being the open surface: a press that
            // arrived after the dialog was closed has nothing to run.
            if state.surface == Some(HostSurface::Edit) {
                let pressed = match mode {
                    crate::edit_args::RunMode::Dry => state.edit.view_mut().press_dry(),
                    crate::edit_args::RunMode::Apply => state.edit.view_mut().press_apply(),
                };
                match pressed {
                    // Hand the built request to the host; `actions.rs` has no
                    // `Index`, so the engine call happens in `app.rs`.
                    Ok(req) => effect.edit_run = Some(req),
                    // Refused (busy, no operation, unparsable args): the view
                    // already carries the matching note, and `build_request` did
                    // not set the busy flag, so there is nothing to finish.
                    Err(_) => {}
                }
                effect.repaint_nzk_ui = true;
            }
        }
        NzkUiAction::EditAnalyze(op) => {
            // `#editValidate` (`edit.js:172-177`) / `#editDedupe` (`:179-183`).
            // The args box is parsed by the view's own producers, exactly like
            // the run buttons above; a bad box records the reference's
            // `Args: …` note and sends nothing.
            if state.surface == Some(HostSurface::Edit) {
                let built = match op {
                    crate::edit_wire::AnalyzeOp::ParseCheck => state.edit.view().validate(),
                    crate::edit_wire::AnalyzeOp::Dedupe => state.edit.view().duplicates(),
                };
                match built {
                    Ok(req) => {
                        if state.edit.view_mut().begin_analyze(op).is_none() {
                            effect.edit_analyze = Some(req);
                        }
                    }
                    Err(note) => state.edit.view_mut().set_note(note),
                }
                effect.repaint_nzk_ui = true;
            }
        }
        NzkUiAction::PreviewRun => {
            // `preview.js:188-199` `run()`: only while the panel is open. The
            // focused box is committed first (the browser's blur-before-click),
            // then the v2 empty-target note is decided HERE; a non-empty target
            // becomes `Effect::preview_run`, which `app.rs` turns into the real
            // in-process engine call.
            if state.surface == Some(HostSurface::Preview) {
                state.form.commit_focused();
                request_preview_run(state, &mut effect);
            }
        }
        NzkUiAction::Preview => {
            state.surface = Some(HostSurface::Preview);
            state.drawer_open = None;
            // The panel's BODY is engine data (`csgraph_core::preview::invoke`),
            // so the host builds it after this dispatch — `actions.rs` can only
            // record that the panel opened.
            effect.preview = true;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::Shortcuts => {
            // A toggle: the sheet is open, or pressing again closes it.
            let next = if state.surface == Some(HostSurface::Shortcuts) {
                None
            } else {
                Some(HostSurface::Shortcuts)
            };
            state.surface = next;
            state.drawer_open = None;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::ToggleModal(which) => {
            // A real toggle with one owner, the modal mirror of
            // `ToggleDrawer`: pressing the same close control twice leaves every
            // modal shut, and a modal replaces any drawer that was open so two
            // viewport-covering backdrops can never be up at once.
            let next = if state.surface == Some(which) {
                None
            } else {
                Some(which)
            };
            match next {
                // `#editClose` re-opened the dialog (or `ToggleModal(Edit)`
                // arrived from anywhere else): go through the one seeding path so
                // the modal can never be shown unpopulated.
                Some(HostSurface::Edit) => state.open_edit(),
                other => {
                    state.surface = other;
                    state.drawer_open = None;
                }
            }
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::FocusNamespaceFilter => {
            // The select's *value* is the shell's own control. The host action is
            // to bring the filter into play; that is a nzk_ui repaint, not a
            // guessed selection.
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::ToggleDrawer(which) => {
            // A real toggle with one owner: pressing the drawer's control again
            // (or pressing the other drawer's) leaves exactly one drawer open.
            state.drawer_open = if state.drawer_open == Some(which) {
                None
            } else {
                Some(which)
            };
            // The two overlay channels are mutually exclusive. A modal's
            // backdrop covers the whole viewport, so a drawer opened under one
            // would be visible only through it; closing the modal here is what
            // makes "one overlay at a time" true across BOTH families rather
            // than only within each. Only an overlay surface is cleared — a
            // non-overlay `surface` cannot be open at the same time anyway.
            if state.surface.map(HostSurface::element).is_some_and(|id| !DRAWER_IDS.contains(&id)) {
                state.surface = None;
            }
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::SrcClose => {
            // The pane's own `close()`. `actions.rs` does not hold the pane, so
            // the host performs it; the counter makes the accepted request
            // observable here.
            state.source_close_count += 1;
            effect.close_source = true;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::SrcMode => {
            // A REAL cycle over the three `SourceTheme`s that exist. The mode is
            // the one owner of the choice; the host derives the palette from it.
            state.source_mode = state.source_mode.next();
            state.source_mode_count += 1;
            effect.source_theme = true;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::SrcPop => {
            // Pop out / dock. The host knows whether the pane currently has its
            // own window; this module only records the request, so the host
            // decides the direction — a control that "reports success" without
            // that decision would be the faked kind.
            state.source_pop_count += 1;
            effect.source_pop = Some(true);
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::SrcCopy => {
            // Request the clipboard write. Whether it lands is the host's
            // result; this module never claims a copy it did not perform.
            state.source_copy_count += 1;
            effect.copy_source = true;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::SrcPrev => {
            // `sourceStep(-1)` (`source.js:502-536`): the pane's own document
            // history. A request, because the stacks live on `SourceState`.
            effect.source_history = Some(false);
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::SrcNext => {
            effect.source_history = Some(true);
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::SrcRef => {
            // The reference's `copySourceRef` copies `file:line`; the host owns
            // the clipboard and the document.
            effect.copy_source_ref = true;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::SrcMethod => {
            // `sourceWholeMethod` re-opens the declaration; this pane's document
            // already IS the whole declaration, so the host re-shows it centred.
            effect.source_whole_method = true;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::SrcWindow => {
            // `shell.js:680-683` opens the current source in a new window. The
            // host owns the window registry.
            effect.source_window = true;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::StopIndex => {
            // Flip the shared flag the in-flight build checks; the worker reports
            // `cancelled: true` and the host keeps its current graph.
            effect.cancel_index = true;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::ChainRun => {
            // `filters.js:543-560 runChain`: the walk itself needs the engine.
            effect.chain_run = true;
            effect.repaint_nzk_ui = true;
        }
        NzkUiAction::ChainFlat => {
            // Pure view toggle (`state.chain.flat`); no engine work.
            state.chain_flat = !state.chain_flat;
            effect.repaint_nzk_ui = true;
        }
    }
    // ONE place keeps the edit host's own flag in step with the ONE visibility
    // owner, `surface`. Every arm that closes the dialog — the ✕
    // (`ToggleModal`), the drawer family, `ModeFind`, another modal — leaves
    // `surface` off `Edit`, and this is what returns `EditHost::facts` to
    // `None`. Doing it per-arm is how the drawer path gets missed: `ToggleDrawer`
    // clears a non-drawer `surface` without ever naming the edit modal, so a
    // close test that only pressed `#editClose` would still pass while the
    // dialog's contents stayed publishable.
    if state.surface != Some(HostSurface::Edit) {
        state.edit.close();
    }
    state.last = Some(last_action);
    effect.repaint_nzk_ui = true;
    effect
}

/// Route one [`PickerAction`] into the picker's real state and report what the
/// host must do.
///
/// ### Why this is not part of [`apply`]
///
/// `apply` is pure in a strong sense: no I/O, and every one of its arms is a
/// state mutation plus flags. Four of the picker's six messages are exactly like
/// that — `Up`/`Home`/`Root`/`Repo` are one `Picker::*` call each — but every one
/// of those calls ends in `browse_to`, i.e. a real `read_dir`. Doing that inside
/// `apply` would make an action-application touch the filesystem, which is the
/// property that lets `apply` be tested with no display *and* no corpus.
///
/// So the filesystem work lives here, in its own function, and `app.rs` calls it
/// for the picker's own controls the way it already calls
/// [`execute_source_action`] for the pane's. Nothing about it is impure beyond
/// the `read_dir` the core genuinely performs, and that is a real consequence
/// rather than a faked one — which is the difference between this and a handler
/// that only bumps a counter.
///
/// ### The two messages that reach past the picker
///
/// * [`PickerAction::Close`] clears [`NzkUiState::surface`] itself, because
///   that field is this module's own state and the modal being shut is a fact
///   about the nzk_ui, not about the listing.
/// * [`PickerAction::Select`] does **not** re-index here: it puts the committed
///   path on [`Effect::reindex_at`] and lets the host run the engine. It DOES
///   close the modal and drop the picker, because the reference's
///   `chooseCurrentFolder` ends in `closeModal()` (`shell.js:99-106`) and a modal
///   that stays open after a successful Select reads as "the press did nothing".
///   A refused Select (missing/unreadable folder) keeps the modal open so the
///   note explaining the refusal stays on screen.
///
/// When no picker is open (`state.picker` is `None`) this is a no-op returning
/// the default effect: there is no listing to navigate, and inventing one from a
/// root this module does not have would be the same class of bug as the undriven
/// `PickerView` this function exists to feed.
pub fn apply_picker(state: &mut NzkUiState, action: PickerAction) -> Effect {
    let mut effect = Effect::default();

    if action == PickerAction::Close {
        // `closeModal()` (`shell.js:14-17`). Also drops the picker: a closed
        // modal has no listing, and keeping one would let `ui.picker` publish
        // `#modalBody` rows into a hidden backdrop — content in a surface that
        // is not on screen.
        state.surface = None;
        state.picker = None;
        // A closed modal cannot keep a text box focused: its keys would go to a
        // control that is no longer on screen.
        state.form.close();
        state.last_picker = Some(PickerAction::Close);
        effect.repaint_nzk_ui = true;
        return effect;
    }

    let Some(picker) = state.picker.as_mut() else {
        return effect;
    };

    // Set by a successful `#modalSelect`; the close is applied after the borrow
    // on `state.picker` ends.
    let mut close_after = false;

    match &action {
        PickerAction::Close => unreachable!("handled above"),
        // `browseTo(parent)` (shell.js:748) — the core's own no-op at `/`.
        PickerAction::Up => {
            picker.up();
        }
        PickerAction::Home => {
            picker.home();
        }
        // `browseTo('/')` (shell.js:756).
        PickerAction::Root => {
            picker.root();
        }
        // `browseTo(state.root || '')` (shell.js:754).
        PickerAction::Repo => {
            picker.repo();
        }
        // `browseTo(modalPathInputEl.value.trim())` (shell.js:750,753). After a
        // successful list `renderPicker` overwrites the box with the resolved
        // `d.path` (`shell.js:53`), which is what `PickerView::go` now records.
        PickerAction::Go(text) => {
            picker.go(text);
        }
        // A `.pick[data-nav]` row: an absolute path the listing itself emitted.
        // `show` (not `go`) because nothing was typed — the input follows the
        // folder that was actually opened.
        PickerAction::Open(path) => {
            picker.show(Path::new(path));
        }
        PickerAction::Select => {
            // `chooseCurrentFolder` writes `#rootInput`, sets `state.rootChoice`
            // and closes the modal — it does NOT index. The host completes the
            // commit; `None` here is the honest "this folder is not selectable"
            // (missing/unreadable), which is the whole reason `chosen_path`
            // returns an `Option`.
            if let Some(chosen) = picker.choose_current_folder() {
                // Mirror `rootInputEl.value = p` (`shell.js:101`): the top bar
                // must show the folder that is about to be indexed.
                state.form.seed_root(&chosen.to_string_lossy());
                effect.reindex_at = Some(chosen);
                close_after = true;
            }
        }
    }

    if close_after {
        state.picker = None;
        state.surface = None;
        // The path box was dismissed with the modal; releasing its focus stops
        // the next keystroke from editing an invisible control.
        state.form.close();
    } else if action.browses() {
        // A press on Up/Home/Root/Repo/row/Go re-lists the folder, and
        // `renderPicker` writes the RESOLVED `d.path` into the input
        // (`shell.js:53`). Drop the edit buffer and record that path, so the
        // focused-box fact cannot paint stale typed text over the new listing.
        state.form.close();
        if let Some(path) = state
            .picker
            .as_ref()
            .map(|pk| pk.current().path.to_string_lossy().into_owned())
        {
            state.form.set_value("#modalPathInput", &path);
        }
    }

    state.last_picker = Some(action);
    effect.repaint_nzk_ui = true;
    effect
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corpus root for the actions that need one.
    ///
    /// Deliberately **the crate's own directory**, not a scratch dir and not
    /// `/`: it exists on every machine this runs on, it is a real directory with
    /// real sub-directories, and it is what `#browseBtn` is actually pointed at
    /// when the app is launched from this checkout. A test that opened the
    /// picker on a fabricated `/tmp/nothing` would pass while proving nothing
    /// about the listing the modal shows (see
    /// `browse_publishes_a_real_listing_through_the_wire`).
    ///
    /// Ids that never reach the picker ignore it, which is why it is a parameter
    /// rather than a field: `apply` must stay callable without a corpus.
    /// task-14 integration: the press path (`FormOpen`) and both commit paths
    /// (`FormKey Enter` and the mouse `FormMenuCommit`) go through `apply`, and
    /// the value lands in the reorg request model the engine is called with.
    /// A click-away press is swallowed (no `Fit`) so the dismissing press cannot
    /// also act on the control behind the menu.
    #[test]
    fn form_select_menu_commits_through_apply_into_the_reorg_request() {
        let mut state = NzkUiState::default();
        state.surface = Some(HostSurface::Export);
        let effect = apply(&mut state, NzkUiAction::FormOpen("#reorgGroup"), &test_repo());
        assert!(effect.repaint_nzk_ui, "opening the menu repaints");
        assert_eq!(
            state.form.menu.as_ref().map(|m| m.options.len()),
            Some(4),
            "the four shipped grouping options"
        );
        // Keyboard highlight, then the MOUSE commit of row 2 ("owner").
        apply(
            &mut state,
            NzkUiAction::FormKey(crate::form_wire::Key::Down),
            &test_repo(),
        );
        apply(
            &mut state,
            NzkUiAction::FormKey(crate::form_wire::Key::Down),
            &test_repo(),
        );
        let effect = apply(&mut state, NzkUiAction::FormMenuCommit(2), &test_repo());
        assert!(effect.repaint_nzk_ui);
        assert!(state.form.menu.is_none(), "the commit closes the menu");
        assert_eq!(state.form.reorg.group, "owner");
        assert_eq!(state.reorg.request.group.as_str(), "owner");

        // Click-away: with the menu open again, an unrelated control press is
        // swallowed (it closes the menu and does not also Fit).
        apply(&mut state, NzkUiAction::FormOpen("#reorgMode"), &test_repo());
        let effect = apply(&mut state, NzkUiAction::Fit, &test_repo());
        assert!(!effect.refit, "the click-away press must not also Fit");
        assert!(effect.repaint_nzk_ui);
        assert!(!state.form.is_active());
    }

    #[test]
    fn depth_text_commits_on_click_away_and_produces_a_view_effect() {
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::FormOpen("#depthSelect"), &test_repo());
        apply(
            &mut state,
            NzkUiAction::FormKey(crate::form_wire::Key::Backspace),
            &test_repo(),
        );
        for c in "4".chars() {
            apply(
                &mut state,
                NzkUiAction::FormKey(crate::form_wire::Key::Char(c)),
                &test_repo(),
            );
        }

        let effect = apply(&mut state, NzkUiAction::Fit, &test_repo());
        assert!(!effect.refit, "the click that blurs the field is swallowed");
        assert_eq!(effect.depth, Some(4));
        assert_eq!(state.depth, 4);
        assert!(!state.form.is_active());
    }

    #[test]
    fn display_selects_commit_into_real_effects() {
        let repo = test_repo();
        // `#uiTheme`: pick `dark-v1` (option index 1).
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::FormOpen("#uiTheme"), &repo);
        let effect = apply(&mut state, NzkUiAction::FormMenuCommit(1), &repo);
        assert_eq!(effect.ui_theme.as_deref(), Some("dark-v1"));
        assert_eq!(state.ui_theme, "dark-v1");

        // `#chainDepth`: option index 4 is depth 5.
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::FormOpen("#chainDepth"), &repo);
        let effect = apply(&mut state, NzkUiAction::FormMenuCommit(4), &repo);
        assert_eq!(effect.chain_config, Some((5, "forward".to_string())));
        assert_eq!(state.chain_depth, 5);

        // `#sourceTheme`: option index 2 is Monokai.
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::FormOpen("#sourceTheme"), &repo);
        let effect = apply(&mut state, NzkUiAction::FormMenuCommit(2), &repo);
        assert!(effect.source_theme, "the pane must repaint with the new palette");
        assert_eq!(state.source_mode, SourceMode::Monokai);

        // `#chainFlatBtn` is a real toggle; `#chainRun` asks the host to walk.
        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::ChainFlat, &repo);
        assert!(state.chain_flat && effect.repaint_nzk_ui);
        let effect = apply(&mut state, NzkUiAction::ChainRun, &repo);
        assert!(effect.chain_run);

        // Stop is the index worker's cancel request.
        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::StopIndex, &repo);
        assert!(effect.cancel_index);

        // A press in the topbar search box focuses it (the `/` chord state).
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::FocusSearch, &repo);
        assert!(state.search_focused);
    }

    #[test]
    fn edit_tool_select_routes_through_the_edit_view() {
        let repo = test_repo();
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Edit, &repo);
        let tools = state.edit.view().tools().expect("the edit dialog loads tools").to_vec();
        assert!(tools.len() > 1, "the engine advertises more than one tool");
        apply(&mut state, NzkUiAction::FormOpen("#editTool"), &repo);
        let effect = apply(&mut state, NzkUiAction::FormMenuCommit(1), &repo);
        assert_eq!(effect.edit_tool.as_deref(), Some(tools[1].as_str()));
    }

    #[test]
    fn sidebar_searches_commit_their_queries() {
        let repo = test_repo();
        // Public definitions filter: updates on every keystroke (pure re-filter).
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::FormOpen("#pubSearch"), &repo);
        for c in "abc".chars() {
            apply(&mut state, NzkUiAction::FormKey(crate::form_wire::Key::Char(c)), &repo);
        }
        assert_eq!(state.pub_query, "abc");

        // Strings search: the engine runs on Enter, not on every character.
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::FormOpen("#strSearch"), &repo);
        for c in "Sps".chars() {
            apply(&mut state, NzkUiAction::FormKey(crate::form_wire::Key::Char(c)), &repo);
        }
        let effect = apply(
            &mut state,
            NzkUiAction::FormKey(crate::form_wire::Key::Enter),
            &repo,
        );
        assert_eq!(effect.str_search.as_deref(), Some("Sps"));
    }

    #[test]
    fn typing_a_root_path_waits_for_index_and_does_not_reindex_on_blur() {
        let repo = test_repo();
        let mut state = NzkUiState::default();
        // Opening the box seeds it with the live repo and commits NOTHING.
        let effect = apply(&mut state, NzkUiAction::FormOpen("#rootInput"), &repo);
        assert_eq!(effect.reindex_at, None, "focusing the box must not re-index");
        assert_eq!(state.form.value("#rootInput"), repo.to_string_lossy());

        // Clear it and type an absolute folder, then Enter.
        let mut actions = vec![];
        for _ in 0..state.form.value("#rootInput").chars().count() {
            actions.push(NzkUiAction::FormKey(crate::form_wire::Key::Backspace));
        }
        for c in "/tmp/ncv-root".chars() {
            actions.push(NzkUiAction::FormKey(crate::form_wire::Key::Char(c)));
        }
        let mut effect = Effect::default();
        for a in actions {
            effect = apply(&mut state, a, &repo);
        }
        assert_eq!(effect.reindex_at, None, "typing must not start indexing");
        assert_eq!(state.form.value("#rootInput"), "/tmp/ncv-root");

        let blur = apply(&mut state, NzkUiAction::FocusSearch, &repo);
        assert_eq!(blur.reindex_at, None, "focus transfer must not start indexing");
        assert!(state.search_focused, "the click must transfer focus to Search");
        assert!(!state.form.is_active(), "the root field must release focus");

        apply(&mut state, NzkUiAction::FormOpen("#rootInput"), &repo);
        assert!(!state.search_focused, "clicking Root must release Search focus");
        let effect = apply(&mut state, NzkUiAction::Index, &repo);
        assert_eq!(
            effect.reindex_at,
            Some(PathBuf::from("/tmp/ncv-root")),
            "Index must use the typed folder"
        );
        assert_eq!(state.form.root, "/tmp/ncv-root");
        assert!(!state.form.is_active());
    }

    #[test]
    fn clicking_index_while_the_root_box_is_focused_indexes_the_typed_path() {
        let repo = test_repo();
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::FormOpen("#rootInput"), &repo);
        for _ in 0..state.form.value("#rootInput").chars().count() {
            apply(&mut state, NzkUiAction::FormKey(crate::form_wire::Key::Backspace), &repo);
        }
        for c in "/tmp/ncv-index".chars() {
            apply(&mut state, NzkUiAction::FormKey(crate::form_wire::Key::Char(c)), &repo);
        }
        // The browser blurs the input before the click reaches Index.
        let effect = apply(&mut state, NzkUiAction::Index, &repo);
        assert_eq!(effect.reindex_at, Some(PathBuf::from("/tmp/ncv-index")));
        assert!(effect.reindex, "Index remains the explicit re-index request");
    }

    /// **Enter in `#rootInput` re-indexes the typed folder** — the reference's
    /// `doIndex()` (`shell.js:494-496`). Typing alone still waits; only the
    /// explicit Enter (or Index press) starts a build, and it is one-shot.
    #[test]
    fn enter_in_the_root_box_reindexes_the_typed_folder_once() {
        let repo = test_repo();
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::FormOpen("#rootInput"), &repo);
        for _ in 0..state.form.value("#rootInput").chars().count() {
            apply(&mut state, NzkUiAction::FormKey(crate::form_wire::Key::Backspace), &repo);
        }
        for c in "/tmp/ncv-enter".chars() {
            let effect =
                apply(&mut state, NzkUiAction::FormKey(crate::form_wire::Key::Char(c)), &repo);
            assert_eq!(effect.reindex_at, None, "typing must not build");
        }
        let effect = apply(&mut state, NzkUiAction::FormKey(crate::form_wire::Key::Enter), &repo);
        assert_eq!(
            effect.reindex_at,
            Some(crate::picker::resolve_input("/tmp/ncv-enter", &repo)),
            "Enter must re-index the typed root"
        );
        assert!(!state.form.is_active(), "Enter commits and blurs the box");
        // One-shot: a second Enter cannot start a second build.
        let again = apply(&mut state, NzkUiAction::FormKey(crate::form_wire::Key::Enter), &repo);
        assert_eq!(again.reindex_at, None, "the commit is consumed once");
    }

    /// **Enter in `#modalPathInput` browses the typed path** — `browseTo(value)`
    /// (`shell.js:752-753`). The modal stays open on the resulting listing.
    #[test]
    fn enter_in_the_picker_path_browses_the_typed_folder() {
        let scratch = Scratch::new("go-enter");
        let inner = scratch.dir("inner");
        scratch.dir("other");

        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Browse, &scratch.path);
        apply(&mut state, NzkUiAction::FormOpen("#modalPathInput"), &scratch.path);
        assert!(state.form.is_active(), "the path box owns the keyboard");
        // Focusing seeds the box from the listing rather than blanking it.
        assert_eq!(
            state.form.value("#modalPathInput"),
            crate::picker::normalize(&scratch.path).to_string_lossy()
        );
        for _ in 0..state.form.value("#modalPathInput").chars().count() {
            apply(&mut state, NzkUiAction::FormKey(crate::form_wire::Key::Backspace), &scratch.path);
        }
        for c in inner.to_string_lossy().chars() {
            apply(&mut state, NzkUiAction::FormKey(crate::form_wire::Key::Char(c)), &scratch.path);
        }
        let effect = apply(&mut state, NzkUiAction::FormKey(crate::form_wire::Key::Enter), &scratch.path);
        assert_eq!(effect.reindex_at, None, "Go browses, it does not index");
        assert!(state.surface == Some(HostSurface::FolderPicker), "the modal stays open");
        assert_eq!(
            published_picker_facts(&state)["path"],
            crate::picker::normalize(&inner).to_string_lossy().as_ref(),
            "Enter must browse the typed absolute path"
        );
    }

    /// A navigation press while the path box is focused must END the edit. The
    /// picker now paints the listed path; a surviving edit buffer would paint
    /// the stale typed text over it on the next frame.
    #[test]
    fn picker_navigation_drops_the_path_edit_buffer() {
        let scratch = Scratch::new("nav-edit");
        scratch.dir("inner");
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Browse, &scratch.path);
        apply(&mut state, NzkUiAction::FormOpen("#modalPathInput"), &scratch.path);
        for c in "zzz".chars() {
            apply(&mut state, NzkUiAction::FormKey(crate::form_wire::Key::Char(c)), &scratch.path);
        }
        assert!(state.form.is_active());
        apply_picker(&mut state, PickerAction::Up);
        assert!(!state.form.is_active(), "navigation must release the box");
        let facts = published_picker_facts(&state);
        assert_eq!(
            facts["path_input"], facts["path"],
            "the bar must show the listed folder, not the abandoned edit"
        );
        assert_eq!(
            facts["path"],
            crate::picker::normalize(&std::env::temp_dir()).to_string_lossy().as_ref()
        );
    }

    fn test_repo() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// The `ui.picker` object the render facts will carry — the CONSUMED form.
    ///
    /// This is a thin wrapper over the same public projection `nzk_ui.rs`
    /// publishes (`PickerView::facts`, which `ui_state_json` copies verbatim into
    /// `ui.picker`) — not a re-implementation. That distinction is the whole
    /// point: the bug this task fixes was a handler that ran while NOTHING
    /// consumed it, so a test that asserted the handler (or a hand-rolled
    /// approximation of the payload) would have passed before the fix too.
    ///
    /// Returns `Null` when no picker facts exist. `nzk_ui.rs` OMITS `ui.picker`
    /// in that case rather than sending `null` (so the shipped document and the
    /// `@frame` token `1600 1000 4bd131fa71b2ccc8` stay untouched); `Null` here
    /// is how a test names "absent", and `closed_picker` asserts exactly that.
    fn published_picker_facts(state: &NzkUiState) -> serde_json::Value {
        state
            .picker
            .as_ref()
            .and_then(|pk| pk.facts())
            .unwrap_or(serde_json::Value::Null)
    }

    #[test]
    fn inert_action_is_a_noop_not_a_panic() {
        let before = NzkUiState::default();
        let mut state = before.clone();
        let effect = apply(&mut state, NzkUiAction::None, &test_repo());
        assert!(effect.is_noop(), "unknown/inert control must do nothing");
        assert_eq!(state.camera, before.camera);
        assert_eq!(state.resettle_count, 0);
        assert_eq!(state.reindex_count, 0);
        assert_eq!(state.last, None);
    }

    #[test]
    fn fit_only_requests_a_refit() {
        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::Fit, &test_repo());
        assert!(effect.refit);
        assert!(!effect.camera && !effect.rebuild_view && !effect.relayout && !effect.reindex);
        assert!(effect.repaint_nzk_ui);
        assert_eq!(state.last, Some(NzkUiAction::Fit));
    }

    #[test]
    fn zoom_steps_mutate_only_the_camera() {
        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::ZoomIn, &test_repo());
        assert_eq!(state.camera.zoom, 1.25);
        assert!(effect.camera && !effect.rebuild_view && !effect.refit);
        let effect = apply(&mut state, NzkUiAction::ZoomOut, &test_repo());
        assert_eq!(state.camera.zoom, 1.0);
        assert!(effect.camera);
        // x/y are untouched by a zoom step.
        assert_eq!((state.camera.x, state.camera.y), (0.0, 0.0));
    }

    #[test]
    fn zoom_clamps_and_then_reports_no_change() {
        let mut state = NzkUiState { zoom_max: 1.25, ..Default::default() };
        let effect = apply(&mut state, NzkUiAction::ZoomIn, &test_repo());
        assert_eq!(state.camera.zoom, 1.25);
        assert!(effect.camera);
        let effect = apply(&mut state, NzkUiAction::ZoomIn, &test_repo());
        assert_eq!(state.camera.zoom, 1.25, "already at max");
        assert!(!effect.camera, "no movement -> no camera effect");
        assert!(effect.repaint_nzk_ui, "the request still repaints");
    }

    #[test]
    fn full_graph_rebuilds_and_refits() {
        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::FullGraph, &test_repo());
        assert!(state.full_graph);
        assert!(effect.rebuild_view && effect.refit);
        // A set that jumps from the roots to the whole model is not an
        // incremental growth: it must re-seed or the new nodes stay at the
        // origin/anchor the old island (see `Effect::reseed_layout`).
        assert!(effect.reseed_layout, "Full graph must lay the whole model out afresh");
        assert!(!effect.relayout && !effect.reindex && !effect.camera);
    }

    #[test]
    fn resettle_rebuilds_and_relayouts() {
        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::Resettle, &test_repo());
        assert_eq!(state.resettle_count, 1);
        assert!(effect.rebuild_view && effect.relayout);
        // Without the re-seed the relayout continues from the on-screen
        // positions and anchors every node, so `R` / `#resettleBtn` moved
        // nothing at all.
        assert!(effect.reseed_layout, "an explicit re-settle must re-seed");
        // The re-seed moves everything, so the camera must follow it; otherwise
        // the new arrangement can land outside the old viewport.
        assert!(effect.refit, "a re-seed must refit the camera");
        assert!(!effect.reindex);
    }

    #[test]
    fn index_requests_a_reindex_and_does_not_touch_the_engine_here() {
        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::Index, &test_repo());
        assert_eq!(state.reindex_count, 1);
        assert!(effect.reindex && effect.rebuild_view && effect.refit);
        // Pure module: it must not have built anything.
        assert!(effect.camera == false && effect.relayout == false);
    }

    #[test]
    fn shipped_seeds_the_sections_the_document_collapses() {
        // Default is empty: `ui.collapsed = null` leaves the shipped document
        // alone, which is right for the stateless screenshot path.
        assert!(NzkUiState::default().collapsed.is_empty());
        // The interactive app seeds this so the first toggle is a delta.
        let state = NzkUiState::shipped();
        assert_eq!(
            state.collapsed_facts(),
            vec!["#chainHead", "#pubHead", "#strHead"],
            "index.html ships pubSection/strSection/chainSection collapsed"
        );
        assert!(state.is_collapsed("#pubHead") && state.is_collapsed("#strHead"));
        assert!(state.is_collapsed("#chainHead"));
        assert!(!state.is_collapsed("#filtersHead"));
    }

    #[test]
    fn shipped_plus_toggle_keeps_the_shipped_three() {
        let mut state = NzkUiState::shipped();
        apply(&mut state, NzkUiAction::ToggleSection("#filtersHead"), &test_repo());
        assert_eq!(
            state.collapsed_facts(),
            vec!["#chainHead", "#filtersHead", "#pubHead", "#strHead"],
            "the first toggle must not expand the shipped sections"
        );
        assert!(state.is_collapsed("#pubHead") && state.is_collapsed("#strHead"));
    }

    #[test]
    fn toggle_section_round_trips_and_is_the_only_owner() {
        let mut state = NzkUiState::default();
        let before = state.clone();
        assert!(!state.is_collapsed("#filtersHead"));

        let effect = apply(&mut state, NzkUiAction::ToggleSection("#filtersHead"), &test_repo());
        assert!(state.is_collapsed("#filtersHead"));
        assert_eq!(state.collapsed_facts(), vec!["#filtersHead"]);
        assert!(effect.repaint_nzk_ui, "a collapsed section repaints the nzk_ui");
        assert!(effect.rebuild_view == false && effect.camera == false);

        // Toggling again returns byte-for-byte to the original state.
        let effect = apply(&mut state, NzkUiAction::ToggleSection("#filtersHead"), &test_repo());
        assert!(!state.is_collapsed("#filtersHead"));
        assert_eq!(state.collapsed, before.collapsed);
        assert!(effect.repaint_nzk_ui);
    }

    #[test]
    fn collapsed_facts_are_stable_and_independent_per_section() {
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::ToggleSection("#strHead"), &test_repo());
        apply(&mut state, NzkUiAction::ToggleSection("#filtersHead"), &test_repo());
        assert_eq!(
            state.collapsed_facts(),
            vec!["#filtersHead", "#strHead"],
            "facts order must be deterministic for reproducible nzk_ui pixels"
        );
        assert!(state.is_collapsed("#filtersHead") && state.is_collapsed("#strHead"));
        assert!(!state.is_collapsed("#pubHead"));
    }

    #[test]
    fn open_blocks_reports_the_real_pane() {
        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::OpenBlocks, &test_repo());
        assert_eq!(effect.pane, Some(HostPane::Blocks));
        assert!(effect.repaint_nzk_ui);
        assert!(NzkUiAction::OpenBlocks.is_effective());
    }

    #[test]
    fn selectors_map_the_wired_controls_and_nothing_else() {
        assert_eq!(NzkUiAction::from_selector("#fitBtn"), NzkUiAction::Fit);
        assert_eq!(NzkUiAction::from_selector("#zoomInBtn"), NzkUiAction::ZoomIn);
        assert_eq!(NzkUiAction::from_selector("#zoomOutBtn"), NzkUiAction::ZoomOut);
        assert_eq!(NzkUiAction::from_selector("#wholeGraphBtn"), NzkUiAction::FullGraph);
        assert_eq!(NzkUiAction::from_selector("#resettleBtn"), NzkUiAction::Resettle);
        assert_eq!(NzkUiAction::from_selector("#indexBtn"), NzkUiAction::Index);
        assert_eq!(NzkUiAction::from_selector("#blocksBtn"), NzkUiAction::OpenBlocks);
        for sel in ["#filtersHead", "#pubHead", "#strHead", "#chainHead", "#problemsHead"] {
            assert_eq!(
                NzkUiAction::from_selector(sel),
                NzkUiAction::ToggleSection(sel),
                "{sel}"
            );
        }
        // Drawn-but-undefined controls are inert, by name. These are the ones
        // still honestly unwired; the owner's ten (#backBtn, #upBtn, #modeGraph,
        // #modeFind, #browseBtn, #exportBtn, #editBtn, #previewBtn,
        // #shortcutsBtn, #nsSelect) are asserted to be EFFECTIVE below.
        //
        // The drawer family is NO LONGER in this list: it is wired now. The
        // modal-backdrop family stays inert because it cannot be reached at all
        // while it is `display:none` — see `nzk_ui::dom::UiState::drawers_open`.
        // The source pane's whole head is wired now, including prev/next (the
        // pane's document history), and Stop is wired to the index worker's
        // cancel flag; only the backdrops remain.
        for sel in ["#shortcutsModal", "#editModal", "#previewModal", "#reorgModal", ""] {
            assert_eq!(NzkUiAction::from_selector(sel), NzkUiAction::None, "{sel}");
        }
        for sel in ["#srcPrevBtn", "#srcNextBtn", "#srcRefBtn", "#srcMethodBtn",
                    "#srcWindowBtn", "#stopIndexBtn"] {
            assert!(
                NzkUiAction::from_selector(sel).is_effective(),
                "{sel} must be wired"
            );
        }
        // The sidebar filter controls are NO LONGER inert: task-11 wired both the
        // app-side model (`NzkUiState::filters`) and their registry owner
        // (`ControlOwner::Host`), so they must map to a real action — Reset
        // restores the shipped model, Prune is completed host-side by the view.
        assert_eq!(
            NzkUiAction::from_selector("#resetFilters"),
            NzkUiAction::Filter(FilterCmd::Reset)
        );
        // r39's sidebar slice wired the namespace "All" reset button, so it is no
        // longer in the inert list above: the press must clear the namespace
        // filter through the same model the select uses.
        assert_eq!(
            NzkUiAction::from_selector("#nsAllBtn"),
            NzkUiAction::Filter(FilterCmd::AllNamespaces)
        );
        assert_eq!(
            NzkUiAction::from_selector("#pruneHidden"),
            NzkUiAction::Filter(FilterCmd::Prune)
        );
        assert_eq!(NzkUiAction::from_selector("#relationsHead"),
                   NzkUiAction::ToggleSection("#relationsHead"));
        // The drawer family IS wired, and container + close button agree.
        assert_eq!(NzkUiAction::from_selector("#nodeDrawer"),
                   NzkUiAction::ToggleDrawer("nodeDrawer"));
        assert_eq!(NzkUiAction::from_selector("#nodeDrawerClose"),
                   NzkUiAction::ToggleDrawer("nodeDrawer"));
        assert_eq!(NzkUiAction::from_selector("#findPanel"),
                   NzkUiAction::ToggleDrawer("findPanel"));
        assert_eq!(NzkUiAction::from_selector("#findClose"),
                   NzkUiAction::ToggleDrawer("findPanel"));
        assert_eq!(NzkUiAction::from_selector("#blocksPanel"),
                   NzkUiAction::ToggleDrawer("blocksPanel"));
        assert_eq!(NzkUiAction::from_selector("#blocksClose"),
                   NzkUiAction::ToggleDrawer("blocksPanel"));
    }

    /// A drawer toggle is a REAL state change with ONE owner: opening one closes
    /// the other, and pressing the same control twice leaves every drawer shut.
    #[test]
    fn the_drawer_toggle_has_one_owner_and_round_trips() {
        let mut state = NzkUiState::default();
        assert_eq!(state.drawer_open, None, "all drawers ship closed");

        let effect = apply(&mut state, NzkUiAction::ToggleDrawer("nodeDrawer"), &test_repo());
        assert!(effect.repaint_nzk_ui, "opening a drawer must repaint the nzk_ui");
        assert_eq!(state.drawer_open, Some("nodeDrawer"));

        // A different drawer REPLACES it — never two open at once.
        apply(&mut state, NzkUiAction::ToggleDrawer("findPanel"), &test_repo());
        assert_eq!(state.drawer_open, Some("findPanel"));

        // Pressing the open drawer's own control closes it.
        apply(&mut state, NzkUiAction::ToggleDrawer("findPanel"), &test_repo());
        assert_eq!(state.drawer_open, None);

        // And the facts the nzk_ui renders from carry exactly that one id.
        apply(&mut state, NzkUiAction::ToggleDrawer("blocksPanel"), &test_repo());
        assert_eq!(
            state.drawer_facts(),
            vec!["blocksPanel"],
            "the open drawer must reach nzk_ui's UiState::drawers_open"
        );
    }

    /// The two overlay channels are DISJOINT, and each overlay is opened only by
    /// the channel that owns its family.
    ///
    /// This guard used to assert that the modal-backdrop family was *inert
    /// outright*, on the premise that a modal could never have an honest rect.
    /// That premise was measured false this round: `.modal-backdrop` is
    /// `position: fixed; inset: 0` with `display:flex; align-items:center;
    /// justify-content:center`, so clearing `hidden` centres the card and
    /// `#shortcutsDone` becomes a real hit target (see
    /// `nzk_ui::dom::UiState::modals_open` for the numbers). The family is
    /// wired now, so the property worth protecting is the narrower one:
    /// **neither channel can open the other's family**, and the modal family is
    /// never reachable by accident.
    #[test]
    fn a_drawer_toggle_can_never_open_a_modal_and_vice_versa() {
        // The whole modal family, by DOM id, is unreachable from the drawer
        // channel. nzk_ui's drawer allow-list would ignore these anyway; this
        // proves the id never even leaves this module as a drawer fact.
        for id in ["reorgModal", "editModal", "previewModal", "shortcutsModal", "modal"] {
            assert!(
                !DRAWER_IDS.contains(&id),
                "{id} is a modal; it must not be in the drawer allow-list"
            );
            let mut state = NzkUiState::default();
            apply(&mut state, NzkUiAction::ToggleDrawer(id), &test_repo());
            assert!(
                state.drawer_facts().is_empty(),
                "a modal id must never reach drawers_open, got {:?}",
                state.drawer_facts()
            );
            assert!(
                state.modal_facts().is_empty(),
                "{id} must not be opened by a drawer toggle"
            );
        }

        // ...and no drawer id can be opened by the modal channel.
        for id in DRAWER_IDS {
            let mut state = NzkUiState::default();
            apply(&mut state, NzkUiAction::ToggleDrawer(id), &test_repo());
            assert_eq!(state.drawer_facts(), vec![id]);
            assert!(
                state.modal_facts().is_empty(),
                "{id} is a drawer; it must not reach modals_open"
            );
        }

        // The modal backdrops themselves are NOT controls. A press that misses
        // the card lands on the backdrop, and treating that as a toggle would
        // close the dialog the user was aiming into. Only the ✕ and the explicit
        // Done/close buttons toggle a modal.
        for sel in ["#reorgModal", "#editModal", "#previewModal", "#shortcutsModal", "#modal"] {
            assert_eq!(
                NzkUiAction::from_selector(sel),
                NzkUiAction::None,
                "{sel} is a backdrop, not a control: it must stay inert"
            );
        }

        // ...and each modal's own close control DOES open it, through the modal
        // channel only.
        let closes = [
            ("#shortcutsClose", HostSurface::Shortcuts, "shortcutsModal"),
            ("#shortcutsDone", HostSurface::Shortcuts, "shortcutsModal"),
            ("#editClose", HostSurface::Edit, "editModal"),
            ("#previewClose", HostSurface::Preview, "previewModal"),
            ("#reorgClose", HostSurface::Export, "reorgModal"),
            ("#modalClose", HostSurface::FolderPicker, "modal"),
            ("#modalCancel", HostSurface::FolderPicker, "modal"),
        ];
        for (sel, want, id) in closes {
            assert_eq!(
                NzkUiAction::from_selector(sel),
                NzkUiAction::ToggleModal(want),
                "{sel} must toggle {want:?}"
            );
            let mut state = NzkUiState::default();
            apply(&mut state, NzkUiAction::from_selector(sel), &test_repo());
            assert_eq!(state.modal_facts(), vec![id], "{sel} must open {id}");
            assert!(
                state.drawer_facts().is_empty(),
                "{sel} must not open anything through the drawer channel"
            );
        }
    }

    /// Every modal id the shell can lay out is reachable from a real control,
    /// and each such control opens EXACTLY its own modal.
    ///
    /// This is the anti-regression guard for the modal family the way
    /// `the_ten_named_controls_each_dispatch_a_real_action` is for the top bar:
    /// if a modal's opener ever goes back to `None`, this names it.
    #[test]
    fn every_modal_is_openable_by_a_real_control() {
        let openers = [
            ("#shortcutsBtn", HostSurface::Shortcuts, "shortcutsModal"),
            ("#editBtn", HostSurface::Edit, "editModal"),
            ("#previewBtn", HostSurface::Preview, "previewModal"),
            ("#exportBtn", HostSurface::Export, "reorgModal"),
            ("#browseBtn", HostSurface::FolderPicker, "modal"),
        ];
        for (sel, want, id) in openers {
            let action = NzkUiAction::from_selector(sel);
            assert_ne!(action, NzkUiAction::None, "{sel} must not be inert");
            assert_eq!(action.label(), want_action_label(sel), "{sel} label drift");

            let mut state = NzkUiState::default();
            let effect = apply(&mut state, action.clone(), &test_repo());
            assert!(
                !effect.is_noop(),
                "{sel} -> {action:?} is a no-op; that is an inert control with extra steps"
            );
            assert_eq!(
                state.modal_facts(),
                vec![id],
                "{sel} must open {id}, the id the shell un-hides"
            );
            assert!(
                state.drawer_facts().is_empty(),
                "{sel} must not also open a drawer"
            );
        }
    }

    /// The top-bar labels, so `every_modal_is_openable_by_a_real_control` can
    /// assert the identity of what each opener dispatches (not just its family).
    fn want_action_label(sel: &str) -> &'static str {
        match sel {
            "#shortcutsBtn" => "shortcuts",
            "#editBtn" => "edit",
            "#previewBtn" => "preview",
            "#exportBtn" => "export",
            "#browseBtn" => "browse",
            other => panic!("unexpected opener {other}"),
        }
    }

    /// A modal toggle is a REAL state change with ONE owner, exactly like the
    /// drawer toggle: pressing the same control twice leaves every modal shut,
    /// and opening a second modal REPLACES the first rather than stacking two
    /// viewport-covering backdrops.
    #[test]
    fn the_modal_toggle_has_one_owner_and_round_trips() {
        let mut state = NzkUiState::default();
        assert!(state.modal_facts().is_empty(), "every modal ships hidden");

        let effect = apply(&mut state, NzkUiAction::Shortcuts, &test_repo());
        assert!(effect.repaint_nzk_ui, "opening a modal must repaint the nzk_ui");
        assert_eq!(state.modal_facts(), vec!["shortcutsModal"]);

        // A different modal REPLACES it — never two open at once, because both
        // backdrops cover the whole viewport.
        apply(&mut state, NzkUiAction::Preview, &test_repo());
        assert_eq!(state.modal_facts(), vec!["previewModal"]);

        // Pressing the open modal's own control closes it.
        apply(&mut state, NzkUiAction::from_selector("#previewClose"), &test_repo());
        assert!(state.modal_facts().is_empty());
    }

    /// A modal and a drawer can never both be open: opening either clears the
    /// other, so the two facts are never both non-empty.
    #[test]
    fn a_modal_and_a_drawer_are_never_open_together() {
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Shortcuts, &test_repo());
        assert_eq!(state.modal_facts(), vec!["shortcutsModal"]);
        assert!(state.drawer_facts().is_empty());

        // A drawer press closes the modal.
        apply(&mut state, NzkUiAction::ToggleDrawer("nodeDrawer"), &test_repo());
        assert_eq!(state.drawer_facts(), vec!["nodeDrawer"]);
        assert!(
            state.modal_facts().is_empty(),
            "opening a drawer must close the viewport-covering modal"
        );

        // ...and a modal press closes the drawer.
        apply(&mut state, NzkUiAction::Edit, &test_repo());
        assert_eq!(state.modal_facts(), vec!["editModal"]);
        assert!(
            state.drawer_facts().is_empty(),
            "opening a modal must close the drawer"
        );
    }

    /// `FocusSearch` (`/`, `shell.js:452-455`) does `setMode('graph', false)`
    /// and then focuses the box. Both halves must land together: a state where
    /// the find drawer is open *and* the search box owns the keyboard is one the
    /// reference cannot reach.
    ///
    /// Also pins the 7A gap-2 fix: `f`/`#modeFind` is `setMode('find', true)`
    /// (`shell.js:459`), whose second argument focuses the shared input, so
    /// Find mode now takes the keyboard too and the find-mode branch of
    /// `search_key` is reachable.
    #[test]
    fn focus_search_closes_find_and_takes_the_keyboard() {
        let mut state = NzkUiState::default();
        assert!(
            !state.search_focused,
            "the box must not own the keyboard before `/` is pressed"
        );

        // Open Find first, so the test proves FocusSearch *closes* it rather
        // than merely starting from a clean slate.
        apply(&mut state, NzkUiAction::ModeFind, &test_repo());
        assert_eq!(state.drawer_facts(), vec!["findPanel"]);
        assert!(
            state.search_focused,
            "`setMode('find', true)` focuses the input, so `f` owns the keyboard"
        );

        let effect = apply(&mut state, NzkUiAction::FocusSearch, &test_repo());
        assert!(
            state.drawer_facts().is_empty(),
            "`setMode('graph')` must close the find drawer"
        );
        assert_eq!(state.surface, None, "no modal surface either");
        assert!(
            state.search_focused,
            "the second half of `/` is the keyboard handover"
        );
        assert!(effect.repaint_nzk_ui, "the focus change must repaint");
    }

    /// `Alt+Right` (`nav.js:441`) requests a **forward** step, and it is a
    /// distinct effect from Back — wiring it onto `navigate_back` would make the
    /// key go backwards while looking wired.
    #[test]
    fn navigate_forward_requests_a_forward_step() {
        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::NavigateForward, &test_repo());
        assert!(
            effect.navigate_forward,
            "Alt+Right must set the forward effect"
        );
        assert!(
            !effect.navigate_back,
            "a forward step must not also step back"
        );
    }

    /// The tab chords (`Alt+W`, `Alt+Shift+W`, `Alt+S`, `Ctrl+Tab`) reach the
    /// real document stack: `NzkUiAction::Tab` is the only writer, and each
    /// command moves the reference's own state (`shell.js:410-431`).
    #[test]
    fn tab_chords_reach_the_document_stack() {
        let mut state = NzkUiState::default();
        assert!(state.tabs.is_empty(), "a fresh nzk_ui has no open documents");

        state.tabs.open("a", serde_json::json!({ "node": "A" }));
        state.tabs.open("b", serde_json::json!({ "node": "B" }));
        assert_eq!(state.tabs.count(), 2);

        // `Alt+S` toggles the split. Done with two documents open, because the
        // reference disables the split control below two (`shell.js:326`:
        // `splitBtn.disabled = tabs.length < 2 && !isSplitOn()`), so toggling a
        // single-tab stack would be testing the disabled path instead.
        apply(&mut state, NzkUiAction::Tab(TabCmd::ToggleSplit), &test_repo());
        let first = state.tabs.split();
        apply(&mut state, NzkUiAction::Tab(TabCmd::ToggleSplit), &test_repo());
        assert_ne!(first, state.tabs.split(), "toggleSplit must toggle");

        // `Ctrl+Tab` cycles without changing how many documents are open.
        apply(&mut state, NzkUiAction::Tab(TabCmd::Cycle(1)), &test_repo());
        assert_eq!(state.tabs.count(), 2, "cycling does not open or close tabs");
        assert!(state.tabs.active_key().is_some(), "cycling activates one");

        // `Alt+W` closes just the active document.
        apply(&mut state, NzkUiAction::Tab(TabCmd::CloseActive), &test_repo());
        assert_eq!(state.tabs.count(), 1, "closeActive removes exactly one");
    }

    /// Round-7 bar payloads: the two keyed `TabCmd`s act through the SAME
    /// `chords::tabs::Tabs` model the chords use — no parallel model. Adversarial:
    /// deleting either dispatch arm leaves the stack untouched and fails these
    /// assertions.
        /// The sidebar filter model (`filters.js`): a chip toggle is a real model
    /// transition, Reset restores the shipped state, and Prune changes nothing
    /// (the deletion is the view half, applied by `app.rs`, so the model must
    /// stay untouched here rather than faking a deletion).
    #[test]
    fn filter_commands_toggle_reset_and_prune_honestly() {
        let mut state = NzkUiState::default();
        assert!(state.filters.is_default(), "shipped default: every chip on");
        let mut e = apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::ToggleKind("method".to_string())),
            &test_repo(),
        );
        assert_eq!(
            state.filters.hidden_kinds.iter().cloned().collect::<Vec<_>>(),
            vec!["method".to_string()],
            "one press hides exactly that kind"
        );
        assert!(e.rebuild_view, "the canvas predicate must be rebuilt");
        // A second press turns it back on.
        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::ToggleKind("method".to_string())),
            &test_repo(),
        );
        assert!(state.filters.hidden_kinds.is_empty());
        // Edge chip + namespace are independent axes.
        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::ToggleEdge("calls".to_string())),
            &test_repo(),
        );
        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::SetNamespace("NZK.A".to_string())),
            &test_repo(),
        );
        assert_eq!(state.filters.hidden_edges.iter().cloned().collect::<Vec<_>>(), vec!["calls".to_string()]);
        assert_eq!(state.filters.namespace, "NZK.A");
        // Reset clears every axis back to the shipped render.
        apply(&mut state, NzkUiAction::Filter(FilterCmd::Reset), &test_repo());
        assert!(state.filters.is_default());
        // Prune: the model must stay unchanged — the host applies the real
        // deletion to the view (no rebuild flag from this layer).
        state.filters.hidden_kinds.insert("class".to_string());
        let before = state.filters.clone();
        e = apply(&mut state, NzkUiAction::Filter(FilterCmd::Prune), &test_repo());
        assert_eq!(state.filters, before, "prune must not pretend to delete");
        assert!(!e.rebuild_view);
        assert!(e.repaint_nzk_ui);
    }

    /// Task-1: the rest of the Filters section (`all`/`none`, namespace all,
    /// the four checkbox flips, and the two Group chips) must be real model
    /// transitions with the reference's semantics.
    #[test]
    fn the_remaining_sidebar_filter_controls_are_real_transitions() {
        let mut state = NzkUiState::default();
        // The host mirrors the corpus universes; without them "none" is an
        // honest no-op rather than an invented list.
        state.known_kinds = vec!["class".into(), "method".into()];
        state.known_edges = vec!["calls".into(), "contains".into()];
        state.known_projects = vec!["p1".into(), "p2".into()];

        // --- all / none ---------------------------------------------------
        let e = apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::AllKinds(false)),
            &test_repo(),
        );
        assert_eq!(
            state.filters.hidden_kinds.iter().cloned().collect::<Vec<_>>(),
            vec!["class".to_string(), "method".to_string()],
            "kinds none hides the whole known universe"
        );
        assert!(e.rebuild_view && e.repaint_nzk_ui);
        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::AllKinds(true)),
            &test_repo(),
        );
        assert!(state.filters.hidden_kinds.is_empty(), "kinds all restores");

        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::AllEdges(false)),
            &test_repo(),
        );
        assert_eq!(
            state.filters.hidden_edges.iter().cloned().collect::<Vec<_>>(),
            vec!["calls".to_string(), "contains".to_string()]
        );
        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::AllEdges(true)),
            &test_repo(),
        );
        assert!(state.filters.hidden_edges.is_empty());

        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::AllProjects(false)),
            &test_repo(),
        );
        assert_eq!(
            state.filters.hidden_projects.iter().cloned().collect::<Vec<_>>(),
            vec!["p1".to_string(), "p2".to_string()]
        );
        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::AllProjects(true)),
            &test_repo(),
        );
        assert!(state.filters.hidden_projects.is_empty());

        // --- namespace all + ns-deep flip ---------------------------------
        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::SetNamespace("NZK.A".into())),
            &test_repo(),
        );
        assert_eq!(state.form.current("#nsSelect"), "NZK.A");
        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::AllNamespaces),
            &test_repo(),
        );
        assert_eq!(state.filters.namespace, "");
        assert_eq!(state.form.current("#nsSelect"), "", "select display follows");
        assert!(state.filters.ns_deep, "shipped default");
        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::FlipNsDeep),
            &test_repo(),
        );
        assert!(!state.filters.ns_deep, "flip turns include-sub-namespaces off");
        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::FlipNsDeep),
            &test_repo(),
        );
        assert!(state.filters.ns_deep);
        assert!(state.filters.is_default());

        // --- hide isolated / hide edge labels -----------------------------
        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::FlipHideIsolated),
            &test_repo(),
        );
        assert!(state.filters.hide_isolated);
        assert!(!state.filters.is_default());
        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::FlipVtMode),
            &test_repo(),
        );
        assert!(state.filters.vt_mode);
        apply(&mut state, NzkUiAction::Filter(FilterCmd::Reset), &test_repo());
        assert!(!state.filters.hide_isolated && !state.filters.vt_mode);

        // --- the two Group chips ------------------------------------------
        let e = apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::FlipGroupChildren),
            &test_repo(),
        );
        assert!(!state.group_children);
        assert_eq!(
            e.group_children,
            Some(false),
            "the resolved value travels to GraphView::set_group_children"
        );
        assert!(!e.rebuild_view, "the view's own setter rebuilds; do not double-build");
        apply(
            &mut state,
            NzkUiAction::Filter(FilterCmd::FlipGroupEdges),
            &test_repo(),
        );
        assert!(!state.group_edges);
        // Reset restores BOTH group flags to the shipped `true`.
        let e = apply(&mut state, NzkUiAction::Filter(FilterCmd::Reset), &test_repo());
        assert!(state.group_children && state.group_edges);
        assert_eq!((e.group_children, e.group_edges), (Some(true), Some(true)));
    }

#[test]
    fn tab_bar_commands_activate_and_close_by_key() {
        let mut state = NzkUiState::default();
        state.tabs.open("a", serde_json::json!({ "node": "A" }));
        state.tabs.open("b", serde_json::json!({ "node": "B" }));
        state.tabs.open("c", serde_json::json!({ "node": "C" }));
        assert_eq!(state.tabs.active_key(), Some("c"));

        // A row press activates that row.
        apply(
            &mut state,
            NzkUiAction::Tab(TabCmd::Activate("a".to_string())),
            &test_repo(),
        );
        assert_eq!(
            state.tabs.active_key(),
            Some("a"),
            "Activate(key) must make that document active"
        );

        // The ✕ closes the named row, not merely the active one.
        apply(
            &mut state,
            NzkUiAction::Tab(TabCmd::Close("b".to_string())),
            &test_repo(),
        );
        assert_eq!(state.tabs.count(), 2, "Close(key) removes exactly one");
        assert!(state.tabs.params("b").is_none(), "the named tab is closed");
        assert!(
            state.tabs.params("a").is_some(),
            "another tab must survive the close"
        );

        // Closing the active row moves the active document as close_active does.
        apply(
            &mut state,
            NzkUiAction::Tab(TabCmd::Close("a".to_string())),
            &test_repo(),
        );
        assert_eq!(state.tabs.count(), 1);
        assert!(state.tabs.params("a").is_none());
        assert_eq!(
            state.tabs.active_key(),
            Some("c"),
            "closeTab moves focus to the next document"
        );

        // A stale key is a no-op, never a panic or a fabricated tab.
        state.tabs.open("d", serde_json::json!({ "node": "D" }));
        let before = state.tabs.count();
        apply(
            &mut state,
            NzkUiAction::Tab(TabCmd::Close("gone".to_string())),
            &test_repo(),
        );
        apply(
            &mut state,
            NzkUiAction::Tab(TabCmd::Activate("gone".to_string())),
            &test_repo(),
        );
        assert_eq!(state.tabs.count(), before);
        assert_eq!(state.tabs.active_key(), Some("d"));
    }

    /// `Ctrl+Shift+V|H|D` / `Shift+D` / `Shift+=` reach the view through their
    /// own effect fields — not through the navigation flags, which would make
    /// them move the selection while looking wired.
    #[test]
    fn arrange_and_spacing_request_their_own_effects() {
        let mut state = NzkUiState::default();
        let effect = apply(
            &mut state,
            NzkUiAction::Arrange(crate::chords::Axis::Vertical),
            &test_repo(),
        );
        assert_eq!(effect.arrange, Some(crate::chords::Axis::Vertical));
        assert!(effect.spacing.is_none(), "an arrange is not a spacing");

        let effect = apply(&mut state, NzkUiAction::Spacing(1), &test_repo());
        assert_eq!(effect.spacing, Some(1));
        assert!(effect.arrange.is_none(), "a spacing is not an arrange");
        assert!(
            !effect.navigate_back && !effect.navigate_forward,
            "geometry chords must not step the selection history"
        );
    }

    /// `#modeFind` and a drawer are the SAME shell element (`#findPanel`), so
    /// switching to Find must not leave a stale `drawer_open` that
    /// `drawer_facts()` would prefer over it — one id, one owner.
    #[test]
    fn mode_find_and_the_find_drawer_are_one_owner() {
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::ToggleDrawer("nodeDrawer"), &test_repo());
        assert_eq!(state.drawer_facts(), vec!["nodeDrawer"]);

        apply(&mut state, NzkUiAction::ModeFind, &test_repo());
        assert_eq!(
            state.drawer_facts(),
            vec!["findPanel"],
            "Find must replace the open drawer, not be shadowed by it"
        );
        assert!(state.modal_facts().is_empty(), "Find is a drawer, not a modal");

        // Leaving Find closes it.
        apply(&mut state, NzkUiAction::ModeGraph, &test_repo());
        assert!(state.drawer_facts().is_empty());
        assert!(state.modal_facts().is_empty());
    }

    // -------------------------------------------------------------- edit modal

    /// `#editBtn` must open a **populated** modal, not merely a visible one. The
    /// operation list has to be the engine's own (`TOOLS` ++ `AUX_TOOLS`, via
    /// `edit_transport::tool_names`), because a hand-copied list could advertise
    /// an operation the in-process transport cannot route — or miss one it can.
    #[test]
    fn edit_btn_opens_a_modal_seeded_from_the_engine_tool_list() {
        let mut state = NzkUiState::default();
        assert_eq!(state.edit_facts(), None, "no edit key exists before the first open");
        assert!(!state.edit.is_open(), "and the host says so too");

        apply(&mut state, NzkUiAction::Edit, &test_repo());

        assert_eq!(state.modal_facts(), vec!["editModal"]);
        let engine = crate::edit_transport::tool_names();
        assert_eq!(engine.len(), 19, "14 edit tools + 5 aux, straight from the engine");
        assert_eq!(
            state.edit.view().tools(),
            Some(engine.as_slice()),
            "the modal must offer exactly the engine's tools, in order"
        );
        // The first offered operation is the default selection (`edit.js:196-201`
        // builds the options; a `<select>` selects the first).
        assert_eq!(state.edit.view().selected(), Some(engine[0].as_str()));

        let facts = state.edit_facts().expect("an open modal publishes facts");
        // The FROZEN wire keys `nzk_ui::dom::EditFacts` deserialises. If these
        // are renamed the shell channel silently drops every one of them.
        assert!(facts.get("#editArgs").is_some(), "#editArgs is the box's real text");
        assert_eq!(facts["#editDry"]["apply"], serde_json::json!(false));
        assert_eq!(facts["#editApply"]["apply"], serde_json::json!(true));
        assert_eq!(facts["#editClose"]["disabled"], serde_json::json!(false));
    }

    /// The modal ships hidden, so the default paint must carry no `edit` key at
    /// all — the same absent-not-null contract `hierarchy`/`pub_defs` use.
    #[test]
    fn a_closed_edit_modal_publishes_no_key() {
        let state = NzkUiState::default();
        assert_eq!(state.edit_facts(), None);
        assert!(
            state.modal_facts().is_empty(),
            "`#editModal` ships with the `hidden` class"
        );
    }

    /// `#editClose` closes the dialog AND stops the contents from being
    /// published — the key has to come back out of the document.
    #[test]
    fn edit_close_removes_the_facts_key_again() {
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Edit, &test_repo());
        assert!(state.edit_facts().is_some());

        apply(
            &mut state,
            NzkUiAction::from_selector("#editClose"),
            &test_repo(),
        );

        assert!(state.modal_facts().is_empty(), "the dialog is hidden again");
        assert_eq!(state.edit_facts(), None, "so its key must be gone");
        assert!(!state.edit.is_open());
        // Reopening shows what the user left, not a blank modal.
        apply(&mut state, NzkUiAction::Edit, &test_repo());
        assert!(state.edit_facts().is_some());
    }

    /// The regression this wiring was written around: a path that closes the
    /// modal WITHOUT naming it — a drawer clearing a non-drawer `surface` — must
    /// still take the facts key out. Guarding only the `#editClose` arm leaves
    /// the dialog's contents publishable while `#editModal` is hidden.
    #[test]
    fn a_drawer_replacing_the_edit_modal_also_removes_the_key() {
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Edit, &test_repo());
        assert!(state.edit_facts().is_some());

        apply(
            &mut state,
            NzkUiAction::ToggleDrawer("nodeDrawer"),
            &test_repo(),
        );

        assert_eq!(state.drawer_facts(), vec!["nodeDrawer"]);
        assert_eq!(
            state.edit_facts(),
            None,
            "a non-edit overlay must not leave the dialog's contents in the document"
        );
        assert!(!state.edit.is_open());
    }

    /// `surface` is the ONE visibility owner; the host's own flag is derived from
    /// it at the end of every action. Pin the equivalence across the whole set of
    /// transitions, including the ones that close the modal without naming it.
    #[test]
    fn the_visibility_owner_and_the_edit_host_flag_never_disagree() {
        let transitions = [
            NzkUiAction::Edit,
            NzkUiAction::ToggleModal(HostSurface::Shortcuts),
            NzkUiAction::Edit,
            NzkUiAction::ToggleModal(HostSurface::Edit),
            NzkUiAction::Edit,
            NzkUiAction::ToggleDrawer("findPanel"),
            NzkUiAction::Edit,
            NzkUiAction::ModeFind,
            NzkUiAction::Edit,
            NzkUiAction::Preview,
            NzkUiAction::Edit,
            NzkUiAction::Shortcuts,
            NzkUiAction::Edit,
        ];
        let mut state = NzkUiState::default();
        for action in transitions {
            apply(&mut state, action.clone(), &test_repo());
            let shown = state.surface == Some(HostSurface::Edit);
            assert_eq!(
                state.edit.is_open(),
                shown,
                "{action:?} left the host flag out of step with `surface`"
            );
            assert_eq!(
                state.edit_facts().is_some(),
                shown,
                "{action:?} published facts that disagree with the visible surface"
            );
        }
    }

    /// The args box is routed through the host's ONE working view, so Ctrl+Enter
    /// starts a DRY run there — `KeyIntent` has no apply variant to reach.
    ///
    /// Note the two entry points and which one starts a run: `edit_args_keydown`
    /// is the *pure* model (it reports the intent but starts nothing), while
    /// `EditHost::on_key` is the run-starting one. Asserting `busy` against the
    /// pure entry point would be asserting a behaviour it deliberately does not
    /// have.
    #[test]
    fn a_key_in_the_args_box_runs_through_the_host_view() {
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Edit, &test_repo());
        assert!(!state.edit.view().is_busy());

        // The pure model reports the decision...
        let (outcome, _note) =
            state.edit_args_keydown(crate::edit_args::KeyEvent::ctrl_enter());
        assert!(outcome.consumed, "Ctrl+Enter is consumed (edit.js:284)");
        assert_eq!(outcome.run, Some(crate::edit_args::RunMode::Dry));
        assert!(
            !state.edit.view().is_busy(),
            "the pure model decides, it does not start the run"
        );

        // ...and the host entry point acts on it.
        let intent = state.edit.on_key(&crate::edit_args::KeyEvent::ctrl_enter());
        assert_eq!(intent, crate::edit_wire::KeyIntent::DryRun);
        assert!(
            state.edit.view().is_busy(),
            "the dry run is in flight on the host's own view"
        );
        assert_eq!(
            state.edit_facts().unwrap()["#editDry"]["disabled"],
            serde_json::json!(true),
            "and the published facts say both run buttons are disabled"
        );
    }

    /// A press on `#editApply` must produce a **writing** request, and `#editDry`
    /// a non-writing one.
    ///
    /// This pins the defect this wiring closes: `press_apply` had no production
    /// caller at all, so the button set the busy flag and stopped, and the modal
    /// could dry-run but never write. `Effect::edit_run` is what carries the
    /// request to the one place that owns the engine.
    #[test]
    fn edit_apply_produces_a_writing_request_and_edit_dry_never_does() {
        use crate::edit_args::RunMode;
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Edit, &test_repo());

        // `#editDry` is `run(false)` (`edit.js:276`).
        assert_eq!(
            NzkUiAction::from_selector("#editDry"),
            NzkUiAction::EditRun(RunMode::Dry)
        );
        let effect = apply(&mut state, NzkUiAction::EditRun(RunMode::Dry), &test_repo());
        let req = effect.edit_run.expect("a press on #editDry builds a request");
        assert_eq!(req.mode, RunMode::Dry);
        assert!(!req.writes(), "the dry button must never write");
        // Conclude it, or the next press is refused as busy.
        state
            .edit
            .view_mut()
            .finish_run(req.mode, &req.tool, &serde_json::json!({ "ok": true }));

        // `#editApply` is `run(true)` (`edit.js:277`) — the SOLE apply producer,
        // reached through `press_apply` and nowhere else.
        assert_eq!(
            NzkUiAction::from_selector("#editApply"),
            NzkUiAction::EditRun(RunMode::Apply)
        );
        let effect = apply(&mut state, NzkUiAction::EditRun(RunMode::Apply), &test_repo());
        let req = effect.edit_run.expect("a press on #editApply builds a request");
        assert_eq!(req.mode, RunMode::Apply);
        assert!(req.writes(), "the apply button MUST produce a writing request");
        assert_eq!(
            req.body()["args"]["apply"],
            serde_json::json!(true),
            "`if (apply) args.apply = true` (edit.js:217)"
        );
    }

    /// A second press while a run is in flight is refused with a note and NO
    /// request — `run` checks `busy` first (`edit.js:207`) and returns silently.
    #[test]
    fn a_second_edit_press_while_busy_is_refused() {
        use crate::edit_args::RunMode;
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Edit, &test_repo());

        let first = apply(&mut state, NzkUiAction::EditRun(RunMode::Apply), &test_repo());
        assert!(first.edit_run.is_some(), "the first press runs");
        assert!(state.edit.view().is_busy());

        let second = apply(&mut state, NzkUiAction::EditRun(RunMode::Apply), &test_repo());
        assert!(second.edit_run.is_none(), "a second run while busy is refused");
        assert!(state.edit.view().is_busy(), "and the note is left alone");
    }

    /// A run press with the modal closed has nothing to run: no request, and the
    /// view is not made busy. A button that "ran" a hidden dialog would be the
    /// faked kind of consequence.
    #[test]
    fn an_edit_press_with_the_modal_closed_does_nothing() {
        use crate::edit_args::RunMode;
        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::EditRun(RunMode::Apply), &test_repo());
        assert!(effect.edit_run.is_none());
        assert!(!state.edit.view().is_busy(), "nothing was started");
    }

    /// `HostSurface::element` is the single enum -> DOM id translation, and it
    /// names the real ids in `frontend/index.html`.
    #[test]
    fn every_surface_maps_to_its_real_shell_element() {
        let want = [
            (HostSurface::Shortcuts, "shortcutsModal"),
            (HostSurface::Edit, "editModal"),
            (HostSurface::Preview, "previewModal"),
            (HostSurface::Export, "reorgModal"),
            (HostSurface::Find, "findPanel"),
            (HostSurface::NodeDrawer, "nodeDrawer"),
            (HostSurface::FolderPicker, "modal"),
        ];
        for (surface, id) in want {
            assert_eq!(surface.element(), id, "{surface:?} must map to #{id}");
        }
        // The two families partition the surfaces: every id is in exactly one.
        for (surface, id) in want {
            let is_drawer = DRAWER_IDS.contains(&id);
            assert_eq!(
                is_drawer,
                matches!(surface, HostSurface::Find | HostSurface::NodeDrawer),
                "{surface:?} is on the wrong side of the drawer/modal split"
            );
        }
    }
    /// The ten controls the owner named must each map to a REAL action — not to
    /// `None`, which is what "inert" means here. This is the anti-regression
    /// guard for the D2 instruction: if any of these ever goes back to inert,
    /// this test names it.
    #[test]
    fn the_ten_named_controls_each_dispatch_a_real_action() {
        let expected = [
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
        for (sel, want) in expected {
            let got = NzkUiAction::from_selector(sel);
            assert_eq!(got, want, "{sel} must dispatch {want:?}");
            assert!(got.is_effective(), "{sel} is still inert");
            // And it must actually change host state or request host work.
            let mut state = NzkUiState::default();
            let effect = apply(&mut state, got.clone(), &test_repo());
            assert!(
                !effect.is_noop(),
                "{sel} -> {got:?} is a no-op; that is an inert control with extra steps"
            );
        }
    }

    #[test]
    fn labels_are_stable() {
        assert_eq!(NzkUiAction::ZoomIn.label(), "zoom-in");
        assert_eq!(NzkUiAction::None.label(), "none");
        assert_eq!(NzkUiAction::ToggleSection("#x").label(), "toggle-section");
        assert_eq!(NzkUiAction::OpenBlocks.label(), "open-blocks");
        assert_eq!(NzkUiAction::SrcClose.label(), "src-close");
        assert_eq!(NzkUiAction::SrcMode.label(), "src-mode");
        assert_eq!(NzkUiAction::SrcPop.label(), "src-pop");
        assert_eq!(NzkUiAction::SrcCopy.label(), "src-copy");
        assert!(!NzkUiAction::None.is_effective());
        assert!(NzkUiAction::Index.is_effective());
    }

    /// The source pane's four genuinely-wired controls each dispatch a DISTINCT
    /// real action, and each one actually does something (a no-op would be an
    /// inert control with extra steps).
    #[test]
    fn the_four_wired_source_controls_each_dispatch_a_distinct_real_action() {
        let expected = [
            ("#srcCloseBtn", NzkUiAction::SrcClose),
            ("#srcModeBtn", NzkUiAction::SrcMode),
            ("#srcPopBtn", NzkUiAction::SrcPop),
            ("#srcCopyBtn", NzkUiAction::SrcCopy),
        ];
        let mut seen: Vec<NzkUiAction> = Vec::new();
        for (sel, want) in expected {
            let got = NzkUiAction::from_selector(sel);
            assert_eq!(got, want, "{sel} must dispatch {want:?}");
            assert!(got.is_effective(), "{sel} is still inert");
            assert!(!seen.contains(&got), "{sel} shares an action; not distinct");
            seen.push(got.clone());

            // Each one must change host state or request real host work.
            let mut state = NzkUiState::default();
            let effect = apply(&mut state, got.clone(), &test_repo());
            assert!(
                !effect.is_noop(),
                "{sel} -> {got:?} is a no-op; that is a faked control"
            );
        }
        assert_eq!(seen.len(), 4, "four distinct actions, no aliasing");
    }

    /// `#srcCloseBtn` requests the pane's OWN close, and the request is counted:
    /// `actions.rs` holds no `SourceState`, so it asks the host to close it.
    #[test]
    fn src_close_requests_the_host_close() {
        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::SrcClose, &test_repo());
        assert!(effect.close_source, "the host must be asked to close the pane");
        assert!(effect.repaint_nzk_ui);
        assert_eq!(state.source_close_count, 1);
        // It must not pretend to do anything display-side.
        assert!(!effect.camera && !effect.rebuild_view && !effect.relayout);
    }

    /// `#srcModeBtn` cycles the three themes that genuinely exist, and returns to
    /// the start after three presses — a real cycle, not a counter.
    #[test]
    fn src_mode_cycles_the_real_themes() {
        assert_eq!(SourceMode::ALL.len(), 5, "the five SourceTheme constructors");
        let mut state = NzkUiState::default();
        assert_eq!(state.source_mode, SourceMode::DarkPlus, "ships darkplus");

        let mut seen = vec![state.source_mode.label()];
        for _ in 0..5 {
            let effect = apply(&mut state, NzkUiAction::SrcMode, &test_repo());
            assert!(effect.source_theme, "a mode press repaints with a new palette");
            seen.push(state.source_mode.label());
        }
        assert_eq!(state.source_mode_count, 5);
        assert_eq!(
            seen,
            vec!["darkplus", "darkmodern", "nzk-dark+", "monokai", "solarized", "darkplus"],
            "the cycle visits every real theme and returns"
        );
        // Every mode in the cycle is distinct from its neighbour.
        for m in SourceMode::ALL {
            assert_ne!(m, m.next(), "{m:?} does not advance");
        }
    }

    /// `#srcPopBtn` asks the host to pop/dock, and the direction is the HOST's
    /// decision (`Some(true)` = "pop out"); this module never claims which way.
    #[test]
    fn src_pop_requests_a_real_window_toggle() {
        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::SrcPop, &test_repo());
        assert_eq!(effect.source_pop, Some(true));
        assert_eq!(state.source_pop_count, 1);
        assert!(effect.repaint_nzk_ui);
    }

    /// `#srcCopyBtn` requests a real clipboard write — it does NOT report success
    /// itself, because the copy happens in the host where the `egui::Context`
    /// lives. The counter is a count of REQUESTS, and the effect is what makes the
    /// write happen.
    #[test]
    fn src_copy_requests_a_clipboard_write_without_faking_it() {
        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::SrcCopy, &test_repo());
        assert!(effect.copy_source, "the host must perform the clipboard write");
        assert_eq!(state.source_copy_count, 1, "the request is observable");
        assert!(effect.repaint_nzk_ui);
    }

    /// `#srcPrevBtn`/`#srcNextBtn` step the pane's REAL document history. The
    /// earlier revision of this test pinned them inert because the model did not
    /// exist; `SourceState` has had `history`/`forward` stacks since the source
    /// pane landed, so the honest test now pins the requested direction.
    #[test]
    fn src_prev_and_next_request_the_panes_document_history() {
        for (sel, forward) in [("#srcPrevBtn", false), ("#srcNextBtn", true)] {
            let action = NzkUiAction::from_selector(sel);
            assert!(action.is_effective(), "{sel} must claim its real behaviour");
            let mut state = NzkUiState::default();
            let want = action.clone();
            let effect = apply(&mut state, action, &test_repo());
            assert_eq!(effect.source_history, Some(forward), "{sel}");
            assert!(effect.repaint_nzk_ui);
            assert_eq!(state.last, Some(want));
        }
        // The other three head buttons are real requests too, not decoration.
        for (sel, check) in [
            ("#srcRefBtn", 0usize),
            ("#srcMethodBtn", 1),
            ("#srcWindowBtn", 2),
        ] {
            let action = NzkUiAction::from_selector(sel);
            assert!(action.is_effective(), "{sel} must have an action");
            let mut state = NzkUiState::default();
            let effect = apply(&mut state, action, &test_repo());
            let hit = match check {
                0 => effect.copy_source_ref,
                1 => effect.source_whole_method,
                _ => effect.source_window,
            };
            assert!(hit, "{sel} must carry its host effect");
            assert!(effect.repaint_nzk_ui);
        }
    }

    // ================== the folder picker (G2) ==============================
    //
    // The whole point of these tests: `#browseBtn`'s handler EXISTED and nothing
    // consumed it. `NzkUiAction::Browse` set `surface` and left
    // `NzkUiState::picker` at `None` forever, so `#modal` un-hid onto an empty
    // body and `nzk_ui.rs`'s `ui.picker` was never published. Asserting "the
    // handler ran" is exactly the evidence that was already available and was
    // not enough — so every test below asserts the CONSUMED result: the listing
    // that reaches the render facts, or the root the host is told to index.

    /// A scratch directory tree for the picker tests.
    ///
    /// Same shape as `picker.rs`'s own `TempDir`, and for the same reason: a
    /// per-pid, per-test unique path under the OS temp dir, removed on `Drop` so
    /// a panicking assertion still cleans up. Duplicated rather than shared
    /// because `picker.rs`'s is `#[cfg(test)]`-private to that module; the
    /// alternative — making it `pub(crate)` — would put a test helper in the
    /// shipping API.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ncv-actions-picker-{}-{}-{}",
                tag,
                std::process::id(),
                n
            ));
            std::fs::create_dir_all(&path).expect("create scratch dir");
            Scratch { path }
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let p = self.path.join(rel);
            std::fs::create_dir_all(&p).expect("create subdir");
            p
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// **The closed picker must not change the render.**
    ///
    /// `ui.picker` is OMITTED (not `null`) while no picker exists, which is what
    /// keeps the shipped `index.html` untouched and the `@frame` token at
    /// `1600 1000 4bd131fa71b2ccc8`. This is the invariant that makes the fix
    /// safe rather than a repaint change.
    #[test]
    fn a_closed_picker_publishes_no_facts() {
        let state = NzkUiState::default();
        assert!(state.picker.is_none(), "no picker before one is opened");
        assert_eq!(state.modal_facts(), Vec::<&str>::new(), "no modal is open");
        assert!(
            published_picker_facts(&state).is_null(),
            "a closed picker must OMIT ui.picker, not send null"
        );
        // Dispatch: with no picker open, a picker message is an honest no-op.
        let mut state = NzkUiState::default();
        let effect = apply_picker(&mut state, PickerAction::Up);
        assert!(effect.is_noop(), "no picker open -> nothing to navigate");
        assert_eq!(state.last_picker, None, "and nothing claims to have happened");
    }

    /// **`#browseBtn` produces a real listing, and it reaches the wire.**
    ///
    /// This is the test the task exists for. It asserts `state.picker` is
    /// `Some`, that the listing has real directory rows read from a real
    /// filesystem, and that `nzk_ui.rs`'s projection turns that into a
    /// `ui.picker` payload whose `dirs` are the rows `apply_picker` will render.
    /// Asserting the handler alone (which passed before this task) would not
    /// catch the break; asserting the payload does.
    #[test]
    fn browse_opens_the_picker_and_publishes_a_listing() {
        let scratch = Scratch::new("browse");
        scratch.dir("alpha");
        scratch.dir("beta");
        std::fs::create_dir_all(scratch.path.join("alpha/inner")).unwrap();

        let mut state = NzkUiState::default();
        let effect = apply(&mut state, NzkUiAction::Browse, &scratch.path);

        assert!(effect.repaint_nzk_ui, "#browseBtn must repaint the nzk_ui");
        assert_eq!(
            state.modal_facts(),
            vec!["modal"],
            "#browseBtn must open #modal through the modal channel"
        );
        let picker = state.picker.as_ref().expect("Browse must CONSTRUCT the picker");

        // The listing is real: both directories, from a real `read_dir`.
        let listing = picker.current();
        assert_eq!(listing.path, crate::picker::normalize(&scratch.path));
        let names: Vec<&str> = listing.dirs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"], "the listing must be the real one");

        // THE CONSUMED RESULT: the facts nzk_ui's `apply_picker` renders.
        let facts = published_picker_facts(&state);
        assert!(
            !facts.is_null(),
            "an open picker MUST publish ui.picker — this is the wire that was dead"
        );
        assert_eq!(facts["path"], listing.path.to_string_lossy().as_ref());
        assert!(facts["selectable"].as_bool().unwrap(), "a real dir is selectable");
        let dirs = facts["dirs"].as_array().expect("dirs must be an array");
        let row_names: Vec<&str> = dirs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(row_names, vec!["alpha", "beta"], "the rows are the listing");
        // `data-nav` must be the absolute path the row descends into.
        for d in dirs {
            let p = d["path"].as_str().unwrap();
            assert!(Path::new(p).is_absolute(), "data-nav must be absolute: {p}");
            assert!(Path::new(p).is_dir(), "data-nav must be a directory: {p}");
        }
    }

    /// Navigation CHANGES the listing through the wire — not just a counter.
    #[test]
    fn picker_navigation_changes_the_published_listing() {
        let scratch = Scratch::new("nav");
        scratch.dir("outer/inner");
        scratch.dir("sibling");

        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Browse, &scratch.path);
        let at_root: Vec<String> = published_picker_facts(&state)["dirs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(at_root, vec!["outer", "sibling"]);

        // A row press descends (`data-nav`, `shell.js:58`).
        let outer = scratch.path.join("outer");
        let effect = apply_picker(&mut state, PickerAction::Open(outer.to_string_lossy().into()));
        assert!(effect.repaint_nzk_ui);
        let facts = &published_picker_facts(&state);
        assert_eq!(facts["path"], outer.to_string_lossy().as_ref());
        let inside: Vec<&str> =
            facts["dirs"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(inside, vec!["inner"], "the listing must have MOVED");

        // `#modalRepo` comes back to the folder being indexed (`shell.js:754`).
        apply_picker(&mut state, PickerAction::Repo);
        let back = &published_picker_facts(&state);
        assert_eq!(back["path"], crate::picker::normalize(&scratch.path).to_string_lossy().as_ref());
        assert_eq!(back["dirs"].as_array().unwrap().len(), 2, "back at the root listing");

        // `#modalUp` from the repo root reaches the temp parent, and the same
        // real listing appears — a no-op would leave the path unchanged.
        apply_picker(&mut state, PickerAction::Up);
        let up = &published_picker_facts(&state);
        assert_eq!(
            up["path"],
            crate::picker::normalize(&std::env::temp_dir()).to_string_lossy().as_ref(),
            "up must reach the real parent"
        );
    }

    /// **`#modalSelect` commits the chosen folder — the consumed result.**
    ///
    /// `chooseCurrentFolder` (`shell.js:99-106`) does not index; it commits the
    /// path. In this fork the commit is the engine request, so the assertion is
    /// on `Effect::reindex_at` carrying the folder that was actually listed —
    /// not on a counter, and not on "a handler ran".
    #[test]
    fn select_commits_the_chosen_folder_for_reindexing() {
        let scratch = Scratch::new("select");
        let wanted = scratch.dir("wanted");
        scratch.dir("other");

        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Browse, &scratch.path);

        // Open the folder that will be committed, then Select it.
        apply_picker(&mut state, PickerAction::Open(wanted.to_string_lossy().into()));
        let effect = apply_picker(&mut state, PickerAction::Select);

        assert_eq!(
            effect.reindex_at.as_deref(),
            Some(crate::picker::normalize(&wanted).as_path()),
            "Select must commit the folder that was on screen"
        );
        // `chooseCurrentFolder` ends in `closeModal()` (`shell.js:99-106`): a
        // successful Select dismisses the browser. The choice itself survives on
        // the effect, which is what the host re-indexes.
        assert_eq!(state.last_picker, Some(PickerAction::Select));
        assert!(state.picker.is_none(), "Select must drop the listing");
        assert_eq!(state.surface, None, "Select must close the modal");
        assert_eq!(
            state.form.value("#rootInput"),
            wanted.to_string_lossy(),
            "Select must write the chosen path into #rootInput"
        );
    }

    /// A folder that is not selectable must NEVER be committed.
    ///
    /// `Note::NotFound`/`Unreadable` are the cases the core refuses; the
    /// commit path must inherit that refusal rather than send them to the
    /// engine, which would re-index a corpus that does not exist.
    #[test]
    fn select_refuses_a_path_that_is_not_selectable() {
        let scratch = Scratch::new("nosel");
        scratch.dir("real");
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Browse, &scratch.path);

        let gone = scratch.path.join("does-not-exist");
        apply_picker(&mut state, PickerAction::Open(gone.to_string_lossy().into()));
        let effect = apply_picker(&mut state, PickerAction::Select);
        assert_eq!(effect.reindex_at, None, "a missing folder must not be committed");
        // And the modal says so rather than rendering an empty folder.
        let facts = &published_picker_facts(&state);
        assert_eq!(facts["state"], "notfound");
        assert_eq!(facts["selectable"], false);
    }

    /// `#modalClose`/`#modalCancel`/backdrop close the modal AND drop the
    /// picker, so a closed modal cannot publish rows into a hidden backdrop.
    #[test]
    fn closing_the_picker_drops_its_listing() {
        let scratch = Scratch::new("close");
        scratch.dir("a");
        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Browse, &scratch.path);
        assert!(state.picker.is_some());

        let effect = apply_picker(&mut state, PickerAction::Close);
        assert!(effect.repaint_nzk_ui);
        assert!(state.picker.is_none(), "a closed modal has no listing");
        assert_eq!(state.modal_facts(), Vec::<&str>::new(), "#modal is hidden again");
        assert!(
            published_picker_facts(&state).is_null(),
            "and ui.picker is omitted again"
        );
    }

    /// The picker's controls are reachable from their real ids, and each maps to
    /// the message the reference's wiring sends (`shell.js:742-762`).
    #[test]
    fn picker_controls_resolve_to_their_reference_messages() {
        let cases = [
            ("#modalClose", PickerAction::Close),
            ("#modalCancel", PickerAction::Close),
            ("#modalUp", PickerAction::Up),
            ("#modalHome", PickerAction::Home),
            ("#modalRoot", PickerAction::Root),
            ("#modalRepo", PickerAction::Repo),
            ("#modalSelect", PickerAction::Select),
        ];
        for (sel, want) in cases {
            let got = PickerAction::from_selector(sel);
            assert_eq!(
                got.as_ref().map(PickerAction::label),
                Some(want.label()),
                "{sel} must resolve to {want:?} (shell.js:742-762)"
            );
            assert_eq!(got, Some(want), "{sel} payload drift");
        }
        // `#modalGo` carries the input text, so it is built by the host rather
        // than resolved from an id — but it must not resolve to something ELSE.
        assert_eq!(PickerAction::from_selector("#modalGo"), None);
        // The backdrop is not a control (a press that misses the card lands on
        // it); closing is host-side, exactly as for the other four modals.
        assert_eq!(PickerAction::from_selector("#modal"), None);
        // And `#modalBody` is a container, not a control: the press resolves
        // through the `.pick[data-nav]` row, which has no id at all.
        assert_eq!(PickerAction::from_selector("#modalBody"), None);

        assert!(PickerAction::Up.browses() && PickerAction::Go(String::new()).browses());
        assert!(!PickerAction::Close.browses() && !PickerAction::Select.browses());
    }

    /// `#modalRepo` needs a repo root, and the picker's is the one it was opened
    /// with — so a picker cannot navigate to a different corpus than the graph.
    #[test]
    fn the_picker_is_rooted_at_the_repo_it_was_opened_with() {
        let a = Scratch::new("repo-a");
        let b = Scratch::new("repo-b");
        a.dir("in-a");
        b.dir("in-b");

        let mut state = NzkUiState::default();
        apply(&mut state, NzkUiAction::Browse, &a.path);
        assert_eq!(
            state.picker.as_ref().unwrap().repo_root(),
            crate::picker::normalize(&a.path).as_path()
        );
        apply_picker(&mut state, PickerAction::Repo);
        let facts = published_picker_facts(&state);
        let names: Vec<&str> =
            facts["dirs"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["in-a"], "Repo goes to the picker's OWN root");

        // Reopening on a different root actually re-roots it.
        apply(&mut state, NzkUiAction::Browse, &b.path);
        assert_eq!(
            state.picker.as_ref().unwrap().repo_root(),
            crate::picker::normalize(&b.path).as_path()
        );
    }
}

/// `#pubSection`'s lazy-load edge, and the effect channel it uses.
///
/// `shell.js:556` fires `loadDefinitions()` only on the EXPAND edge of the
/// section's own click handler (`if (!collapsed)`). Collapsing again does not
/// refetch. These tests pin that edge, because getting it wrong is invisible:
/// fetching on every click still populates the list, so only the request COUNT
/// distinguishes "once per open" from "once per press" — and only a test that
/// asserts the negative case (collapse ⇒ no effect) can tell them apart.
#[cfg(test)]
mod pub_section_tests {
    use super::*;

    fn repo() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// Expanding `#pubHead` must ask the host to load.
    #[test]
    fn expanding_pub_section_requests_the_definition_load() {
        // `shipped()` collapses `#pubHead` (index.html ships it collapsed), so
        // one toggle is the EXPAND edge.
        let mut state = NzkUiState::shipped();
        assert!(state.is_collapsed("#pubHead"), "index.html ships pubSection collapsed");

        let effect = apply(&mut state, NzkUiAction::ToggleSection("#pubHead"), &repo());
        assert!(
            effect.load_public_definitions,
            "expanding #pubSection must request the engine load (shell.js:556)"
        );
        assert!(!state.is_collapsed("#pubHead"), "…and the section is now open");
    }

    /// Collapsing it again must NOT re-request. This is the half that a
    /// "set the flag on any toggle" implementation gets wrong.
    #[test]
    fn collapsing_pub_section_again_does_not_request_the_load() {
        let mut state = NzkUiState::shipped();
        // Open it (this one legitimately requests).
        let opened = apply(&mut state, NzkUiAction::ToggleSection("#pubHead"), &repo());
        assert!(opened.load_public_definitions);

        // Close it: the reference's handler does not fetch on this edge.
        let closed = apply(&mut state, NzkUiAction::ToggleSection("#pubHead"), &repo());
        assert!(
            !closed.load_public_definitions,
            "collapsing #pubSection must NOT re-request the load — shell.js:556 \
             only fetches inside the `if (!collapsed)` branch"
        );
        assert!(state.is_collapsed("#pubHead"));
    }

    /// No OTHER section may request the public-definitions load. `#strHead` has
    /// its own loader (`loadStrings`) and the rest have none, so a flag set for
    /// every section would fire the wrong query.
    #[test]
    fn no_other_section_requests_the_public_definition_load() {
        for section in [
            "#filtersHead",
            "#strHead",
            "#chainHead",
            "#problemsHead",
            "#relationsHead",
        ] {
            let mut state = NzkUiState::default();
            let effect = apply(&mut state, NzkUiAction::ToggleSection(section), &repo());
            assert!(
                !effect.load_public_definitions,
                "{section} must not trigger the public-definitions load"
            );
        }
    }

    /// The consumed-value default: a default `NzkUiState` carries NO count and
    /// NO list, so the facts object omits the key entirely. `Some(0)` here would
    /// be the false zero the task text forbids.
    #[test]
    fn the_default_state_carries_no_count_and_no_list() {
        let state = NzkUiState::default();
        assert!(state.pub_total.is_none(), "no engine query has happened yet");
        assert!(state.pub_list.is_none(), "and nothing has been loaded");
    }

    /// The flag the nzk_ui publishes starts at the shipped document's own value:
    /// `index.html` ships `#groupChildren checked` (`:253`), which is
    /// `groupChildren: true` (`core.js:270`) and the single owner's default
    /// (`view.rs:196`). If these three ever disagreed, the first frame would
    /// repaint a checkbox into a state the document did not ship.
    #[test]
    fn the_shipped_group_children_flag_matches_the_document() {
        let state = NzkUiState::default();
        assert!(
            state.group_children,
            "a default NzkUiState must publish groupChildren=true, matching \
             index.html's checked #groupChildren and view.rs's own default"
        );
    }

    /// `#groupEdges` (task-24) starts at the shipped document's own value:
    /// `index.html:255` ships `#groupEdges checked`, which is
    /// `groupEdges: true` (`core.js:271`) and the single owner's default
    /// (`GraphView::group_edges`). If these ever disagreed, the first frame would
    /// repaint the box into a state the document did not ship — and because the
    /// box is `hidden`, nothing on screen would reveal the drift.
    #[test]
    fn the_shipped_group_edges_flag_matches_the_document() {
        let state = NzkUiState::default();
        assert!(
            state.group_edges,
            "a default NzkUiState must publish groupEdges=true, matching \
             index.html:255's checked #groupEdges, core.js:271 and view.rs's default"
        );
        // And it is genuinely its own field, not a mirror of the other control.
        let flipped = NzkUiState {
            group_edges: false,
            ..NzkUiState::default()
        };
        assert!(!flipped.group_edges);
        assert!(
            flipped.group_children,
            "setting groupEdges must not move groupChildren: distinct controls"
        );
    }
}

/// `#newWinBtn` routing for the document window (`windows.js:172 openDoc`).
///
/// The window itself is host-only (winit + wgpu, no display in tests), so this
/// layer is the furthest a display-free test can prove: the control resolves to
/// exactly one effective `NzkUiAction`, and that action means "new document
/// window". The host's `App::dispatch` consumes it into `Action::OpenDoc`; the
/// `windows::DocTarget` half of that is tested in `windows.rs`.
#[cfg(test)]
mod window_route_tests {
    use super::*;

    #[test]
    fn new_window_control_routes_to_one_effective_action() {
        let action = NzkUiAction::from_selector("#newWinBtn");
        assert_eq!(action, NzkUiAction::NewDocWindow);
        assert_eq!(action.label(), "new-doc-window");
        assert!(action.is_effective(), "a new-window press must do something");
    }

    #[test]
    fn new_window_selector_still_maps_to_an_effective_action() {
        // `#newWinBtn` is not an element the real DOM lays out (the visible
        // pop-out is `#srcPopBtn`), so it is deliberately absent from the nzk_ui
        // `CONTROLS` registry. The selector map keeps the action for callers
        // that name the operation directly (the control channel and `windows.js`'s
        // own `openDoc` vocabulary), and it must not decay to a no-op while
        // those callers exist.
        assert_ne!(NzkUiAction::from_selector("#newWinBtn"), NzkUiAction::None);
        assert_eq!(NzkUiAction::None.label(), "none");
    }
}

#[cfg(test)]
mod source_open_tests {
    use super::*;

    /// `#nodeCodeBtn` (`{ } View code`) uses v2's exact wiring: it is effective,
    /// and it requests the source pane only when a node is selected. With no
    /// selection the reference toasts and returns, so the effect must stay false
    /// and no empty document may be opened.
    #[test]
    fn view_code_press_requests_the_selected_node_and_never_opens_empty() {
        assert_eq!(
            NzkUiAction::from_selector("#nodeCodeBtn"),
            NzkUiAction::SourceOpen,
            "the header button has exactly one action"
        );
        assert_eq!(NzkUiAction::SourceOpen.label(), "source-open");
        assert!(NzkUiAction::SourceOpen.is_effective());

        let repo = std::path::Path::new("/repo");
        let mut empty = NzkUiState::shipped();
        let effect = apply(&mut empty, NzkUiAction::SourceOpen, repo);
        assert!(!effect.source_open, "no selection = nothing to open");

        let mut state = NzkUiState::shipped();
        state.selected_node = Some(0);
        let effect = apply(&mut state, NzkUiAction::SourceOpen, repo);
        assert!(effect.source_open, "a selection is the request to open it");
    }

    /// `#previewRun` / Enter-in-`#previewTo` — the `preview.js:188-199` run path.
    ///
    /// The action layer decides *that* the host must ask the engine and never
    /// fabricates a plan: an empty target is the reference's own note (`bad`,
    /// `#previewOut` untouched by an engine call), and a typed target sets
    /// `Effect::preview_run` for `app.rs`. The real engine answer is asserted in
    /// `preview_wire`'s `run_hands_the_typed_form_values_to_the_engine`.
    #[test]
    fn a_preview_run_press_asks_the_host_for_the_real_engine_plan() {
        let repo = std::path::Path::new("/repo");
        let mut state = NzkUiState::shipped();
        state.surface = Some(HostSurface::Preview);

        // Empty #previewTo: the reference's note, and NO engine request.
        let effect = apply(&mut state, NzkUiAction::PreviewRun, repo);
        assert!(!effect.preview_run, "nothing typed = no request");
        let note = state.preview.as_ref().expect("the v2 note is published");
        assert_eq!(note.note, crate::preview_wire::MISSING_TARGET_NOTE);
        assert_eq!(note.note_kind, "bad");

        // Typed values: the host is asked to run the REAL engine call.
        state.form.set_value("#previewSelector", "n:9");
        state.form.set_value("#previewOp", "retype");
        state.form.set_value("#previewTo", "Plus");
        let effect = apply(&mut state, NzkUiAction::PreviewRun, repo);
        assert!(effect.preview_run, "a typed target is a run request");
        assert!(effect.repaint_nzk_ui);

        // The control map and the dispatch name agree with the button.
        assert_eq!(
            NzkUiAction::from_selector("#previewRun"),
            NzkUiAction::PreviewRun
        );
        assert_eq!(NzkUiAction::PreviewRun.label(), "preview-run");

        // Enter in #previewTo is the same decision, routed via FormKey.
        apply(&mut state, NzkUiAction::FormOpen("#previewTo"), repo);
        let effect = apply(
            &mut state,
            NzkUiAction::FormKey(crate::form_wire::Key::Char('s')),
            repo,
        );
        assert!(!effect.preview_run, "typing is not a run");
        let effect = apply(
            &mut state,
            NzkUiAction::FormKey(crate::form_wire::Key::Enter),
            repo,
        );
        assert!(effect.preview_run, "Enter in #previewTo runs the plan");
        assert_eq!(state.form.preview_to, "Pluss", "the committed text is the request");

        // A press after the panel was closed has nothing to run.
        state.surface = None;
        let effect = apply(&mut state, NzkUiAction::PreviewRun, repo);
        assert!(!effect.preview_run);
    }

    /// `#previewRun` while a box is focused must commit the typing first (the
    /// browser's change-before-click), not be swallowed as a click-away.
    #[test]
    fn a_preview_run_press_commits_the_focused_target_box() {
        let repo = std::path::Path::new("/repo");
        let mut state = NzkUiState::shipped();
        state.surface = Some(HostSurface::Preview);
        apply(&mut state, NzkUiAction::FormOpen("#previewTo"), repo);
        for c in "Renamed".chars() {
            apply(
                &mut state,
                NzkUiAction::FormKey(crate::form_wire::Key::Char(c)),
                repo,
            );
        }
        assert!(state.form.is_active(), "the box owns the keyboard");
        let effect = apply(&mut state, NzkUiAction::PreviewRun, repo);
        assert!(effect.preview_run, "the run sees the typed text");
        assert_eq!(state.form.preview_to, "Renamed");
        assert!(!state.form.is_active(), "Run blurs the box like a browser click");
    }

    /// Project chips (`filters.js` `state.filters.projects`): the press toggles
    /// one project id, reset restores every chip, and the chip's paint source is
    /// the same model (the `on` flag in `nzk_ui::project_rows`).
    #[test]
    fn a_project_chip_press_toggles_the_project_filter_and_reset_restores_it() {
        use crate::actions::FilterCmd;
        let repo = std::path::Path::new("/repo");
        let mut state = NzkUiState::default();
        assert!(state.filters.is_default());
        let effect = apply(&mut state, NzkUiAction::Filter(FilterCmd::ToggleProject("p1".into())), repo);
        assert!(effect.rebuild_view, "a chip toggle is a real view transition");
        assert!(state.filters.hidden_projects.contains("p1"));
        assert!(!state.filters.is_default());
        // A second press turns it back ON (the chip is a toggle, like v2).
        apply(&mut state, NzkUiAction::Filter(FilterCmd::ToggleProject("p1".into())), repo);
        assert!(state.filters.hidden_projects.is_empty());
        assert!(state.filters.is_default());
        // `#resetFilters` clears projects along with kinds/edges/namespace.
        apply(&mut state, NzkUiAction::Filter(FilterCmd::ToggleProject("p1".into())), repo);
        apply(&mut state, NzkUiAction::Filter(FilterCmd::SetNamespace("NZK.A".into())), repo);
        apply(&mut state, NzkUiAction::Filter(FilterCmd::Reset), repo);
        assert!(state.filters.is_default(), "reset restores every facet, projects included");
        assert!(state.filters.hidden_projects.is_empty());
        assert_eq!(state.filters.namespace, "");
    }

    /// `#nsSelect` end-to-end: rows come from the host-mirrored graph namespaces
    /// (dynamic, never a hardcoded list), and committing a row changes the ONE
    /// filter model the view predicate consumes. Reset clears both copies.
    #[test]
    fn the_namespace_select_options_come_from_the_graph_and_a_change_filters() {
        use crate::form_wire::Key;
        let repo = std::path::Path::new("/repo");
        let mut state = NzkUiState::default();
        // Host mirror (app.rs copies this from the live GraphView each pass).
        state.namespaces = vec!["NZK.A".to_string(), "NZK.B".to_string()];
        // The press is the form-open path now, not the inert focus action.
        assert_eq!(
            NzkUiAction::from_selector("#nsSelect"),
            NzkUiAction::FormOpen("#nsSelect")
        );
        let effect = apply(&mut state, NzkUiAction::FormOpen("#nsSelect"), repo);
        assert!(effect.repaint_nzk_ui);
        let menu = state.form.menu.as_ref().expect("the select menu is open");
        assert_eq!(menu.options.len(), 3, "All namespaces + the two graph rows");
        assert_eq!(menu.options[0].1, "All namespaces (2)");
        assert_eq!(menu.options[1].0, "NZK.A");
        // Nothing changed yet: the filter model still passes everything.
        assert_eq!(state.filters.namespace, "");
        // Down + Enter commits NZK.A, and the change is mirrored into filters.
        apply(&mut state, NzkUiAction::FormKey(Key::Down), repo);
        apply(&mut state, NzkUiAction::FormKey(Key::Enter), repo);
        assert_eq!(state.filters.namespace, "NZK.A");
        assert_eq!(state.form.current("#nsSelect"), "NZK.A");
        assert!(!state.filters.is_default(), "the filter model really moved");
        // Reset restores the filter AND the select's own value.
        apply(&mut state, NzkUiAction::Filter(FilterCmd::Reset), repo);
        assert_eq!(state.filters.namespace, "");
        assert_eq!(state.form.current("#nsSelect"), "");
        assert!(state.filters.is_default());
    }
}
