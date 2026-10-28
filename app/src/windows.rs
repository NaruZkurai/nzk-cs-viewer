//! Attach / snap geometry for the fork's multi-window shell.
//!
//! Everything here is **pure geometry in logical screen pixels** — no winit,
//! no egui, no I/O — so the decision rules are testable in isolation and can be
//! compiled standalone:
//!
//! ```text
//! rustc --test app/src/windows.rs -o /tmp/windows-test && /tmp/windows-test
//! ```
//!
//! The runtime half (`app/src/app.rs`) only feeds real `outer_position()` /
//! `outer_size()` rectangles in and applies the returned rectangle with
//! `Window::set_outer_position`.
//!
//! # Rules (authoritative)
//!
//! * `SNAP_THRESHOLD_PX = 16` logical px: a dragged edge snaps when its distance
//!   to a target edge is `<= 16`; otherwise the move is free.
//! * Edges: all four of the dragged window (left/right/top/bottom) are tested
//!   against all four of each target (main window + every other pane window).
//!   Both adjacency (`dragged.left -> target.right`) and alignment
//!   (`dragged.right -> target.right`) are candidates.
//! * Corners: when x and y snap to the *same* target the guide is a corner
//!   (`SnapEdge::TopLeft`-style) and both guide lines are drawn.
//! * Target choice: prefer a target that snaps **both** axes (a corner) over
//!   one that snaps a single axis; then the smallest total correction
//!   `|dx| + |dy|`; then the target whose centre is closest to the dragged
//!   centre. A target with only one axis in range still beats no snap.
//! * Dock: if `main.overlap_ratio(dragged) >= DOCK_MIN_OVERLAP (0.5)` or the
//!   dragged centre is inside the main rectangle, the decision is `Dock` and
//!   the pane is re-attached (its OS window closed) on release.
//! * Dock target: the **main window only** — a pane dropped over another pane
//!   window snaps to it but does not dock into it.
//! * Dock affinity: a pane may only dock into its own home target, mirroring
//!   `panels.js`'s `dockZoneAt`/`dockAffinity` (v2 offers a pane only its home
//!   zone). Translated to the five-zone model: Source -> the bottom strip (v2's
//!   `source` row), the canvas-overlay panes (NodeDrawer, Find, and v3-only
//!   Blocks) -> the centre (v2's `#canvasWrap` target). A drop on any other zone
//!   falls back to free-float, exactly as v2's `rememberFloating` does. The v2
//!   literals live in [`V2_DOCK_AFFINITY`] and are asserted in the tests.
//! * `beside_main` mirrors the original Tauri host's placement: a column
//!   `BESIDE_GAP_PX = 10` right of the main window and `BESIDE_TOP_PX = 28`
//!   below its top, cascaded by `CASCADE_STEP_PX = 32 * (n % 6)`, clamped to
//!   the monitor with `MONITOR_MARGIN_PX = 8`; when there is no room right it
//!   cascades from the main window's top-left (+48) or below it.

/// A rectangle in logical screen pixels (top-left origin).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w * 0.5, self.y + self.h * 0.5)
    }
    pub fn area(&self) -> f32 {
        (self.w.max(0.0)) * (self.h.max(0.0))
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.right() && py >= self.y && py <= self.bottom()
    }
    pub fn intersection_area(&self, o: &Rect) -> f32 {
        let w = (self.right().min(o.right()) - self.x.max(o.x)).max(0.0);
        let h = (self.bottom().min(o.bottom()) - self.y.max(o.y)).max(0.0);
        w * h
    }
    /// Intersection area as a fraction of **this** rectangle's area.
    pub fn overlap_ratio(&self, o: &Rect) -> f32 {
        let a = self.area();
        if a <= 0.0 {
            return 0.0;
        }
        self.intersection_area(o) / a
    }
}

/// A pane that can live docked in the main window or detached in its own OS
/// window. Sizes/titles mirror `rust-only-csviewer/app/src/main.rs`
/// (`pane_title` / `pane_size`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Pane {
    Source,
    NodeDrawer,
    Find,
    Blocks,
}

impl Pane {
    pub const ALL: [Pane; 4] = [Pane::Source, Pane::NodeDrawer, Pane::Find, Pane::Blocks];

    pub fn id(self) -> &'static str {
        match self {
            Pane::Source => "source",
            Pane::NodeDrawer => "nodeDrawer",
            Pane::Find => "findPanel",
            Pane::Blocks => "blocks",
        }
    }
    /// Inverse of [`Pane::id`], for the persisted layout blob. `None` for an
    /// unknown id so an older/newer blob with extra panes is tolerated.
    pub fn from_id(s: &str) -> Option<Self> {
        match s {
            "source" => Some(Pane::Source),
            "nodeDrawer" => Some(Pane::NodeDrawer),
            "findPanel" => Some(Pane::Find),
            "blocks" => Some(Pane::Blocks),
            _ => None,
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Pane::Source => "Source",
            Pane::NodeDrawer => "Node",
            Pane::Find => "Find",
            Pane::Blocks => "Blocks",
        }
    }
    /// `(inner_width, inner_height)` from the original `pane_size`.
    pub fn default_size(self) -> (f32, f32) {
        match self {
            Pane::Source => (1180.0, 860.0),
            Pane::NodeDrawer => (440.0, 620.0),
            Pane::Find => (560.0, 720.0),
            Pane::Blocks => (720.0, 640.0),
        }
    }
    pub fn min_size(self) -> (f32, f32) {
        match self {
            Pane::Source => (520.0, 320.0),
            Pane::NodeDrawer => (320.0, 240.0),
            Pane::Find => (340.0, 280.0),
            Pane::Blocks => (320.0, 260.0),
        }
    }

    /// The dock zones this pane may join, mirroring `panels.js`'s
    /// `dockZoneAt` + `dockAffinity` (`panels.js:344-365`). v2 gives a pane
    /// only its own home target, so this is a one-element list. A drop on any
    /// other zone is refused and the pane keeps floating.
    pub fn dock_affinity(self) -> &'static [DockZone] {
        match V2_DOCK_AFFINITY.iter().find(|row| row.pane == self.id()) {
            Some(row) => row.zones,
            // Every [`Pane`] has a row; an empty slice would mean "no docking".
            None => &[],
        }
    }

    /// Whether `zone` is one of this pane's affine dock targets.
    pub fn can_dock(self, zone: DockZone) -> bool {
        self.dock_affinity().contains(&zone)
    }
}

pub const SNAP_THRESHOLD_PX: f32 = 16.0;
/// >50% of the dragged window over the main window (or its centre inside)
/// re-docks the pane.
pub const DOCK_MIN_OVERLAP: f32 = 0.5;
pub const BESIDE_GAP_PX: f32 = 10.0;
pub const BESIDE_TOP_PX: f32 = 28.0;
pub const CASCADE_STEP_PX: f32 = 32.0;
pub const CASCADE_MOD: u64 = 6;
pub const MONITOR_MARGIN_PX: f32 = 8.0;
/// A snapping pane is left alone for this long before it may auto-dock, so the
/// window manager's interactive move has clearly ended.
pub const DOCK_DEBOUNCE_MS: u64 = 250;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapEdge {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// Which rectangle a snap/dock decision refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetId {
    Main,
    Pane(Pane),
}

/// The visible indicator the main window paints while a pane is snapped:
/// a full-height/width line at the shared edge, or both lines for a corner.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Guide {
    pub edge: SnapEdge,
    /// Vertical guide (left/right alignment): `(x, y_from, y_to)`.
    pub vertical: Option<(f32, f32, f32)>,
    /// Horizontal guide (top/bottom alignment): `(y, x_from, x_to)`.
    pub horizontal: Option<(f32, f32, f32)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decision {
    /// The pane is over the main window: close its OS window and re-attach it.
    Dock { overlap: f32, target: TargetId },
    /// Stay detached at the snapped rectangle.
    Snap { rect: Rect, edge: SnapEdge, target: TargetId, guide: Guide },
    /// No edge within threshold: free move.
    Free { rect: Rect },
}

fn axis_candidates(drag_lo: f32, drag_hi: f32, tgt_lo: f32, tgt_hi: f32) -> [(f32, f32); 4] {
    // (corrected drag position, delta) for: align-lo, align-hi, adjacent (lo->hi), adjacent (hi->lo)
    [
        (tgt_lo, tgt_lo - drag_lo),
        (tgt_hi - (drag_hi - drag_lo), tgt_hi - drag_hi),
        (tgt_hi, tgt_hi - drag_lo),
        (tgt_lo - (drag_hi - drag_lo), tgt_lo - drag_hi),
    ]
}

/// Best `(new_lo, delta, edge)` on one axis, or `None` when nothing is inside
/// `threshold`. Same-edge alignment (indices 0/1) beats adjacency (2/3) on a
/// tie of `|delta|`; otherwise the smallest correction wins.
fn best_axis(
    drag_lo: f32,
    drag_hi: f32,
    tgt_lo: f32,
    tgt_hi: f32,
    threshold: f32,
    lo_edge: SnapEdge,
    hi_edge: SnapEdge,
) -> Option<(f32, f32, SnapEdge)> {
    let mut best: Option<(f32, usize, f32, f32, SnapEdge)> = None; // (|delta|, rank, pos, delta, edge)
    for (i, (pos, delta)) in axis_candidates(drag_lo, drag_hi, tgt_lo, tgt_hi)
        .into_iter()
        .enumerate()
    {
        if delta.abs() > threshold {
            continue;
        }
        let rank = if i < 2 { 0 } else { 1 };
        // i0: drag.lo aligns to tgt.lo (drag.lo moves)
        // i1: drag.hi aligns to tgt.hi (drag.hi moves)
        // i2: drag.lo sits against tgt.hi (drag.lo moves)
        // i3: drag.hi sits against tgt.lo (drag.hi moves)
        let edge = if i == 0 || i == 2 { lo_edge } else { hi_edge };
        let key = (delta.abs(), rank, i);
        let better = match &best {
            None => true,
            Some(b) => (key.0, key.1) < (b.0, b.1),
        };
        if better {
            best = Some((delta.abs(), rank, pos, delta, edge));
        }
    }
    best.map(|(_, _, pos, delta, edge)| (pos, delta, edge))
}

/// Decide what a dragged pane window should do against the main window and the
/// other detached pane windows.
pub fn decide(
    dragged: Rect,
    main: Rect,
    others: &[(Pane, Rect)],
    threshold: f32,
) -> Decision {
    let (cx, cy) = dragged.center();
    let overlap = dragged.overlap_ratio(&main);
    if overlap >= DOCK_MIN_OVERLAP || main.contains(cx, cy) {
        return Decision::Dock { overlap, target: TargetId::Main };
    }

    let mut targets: Vec<(TargetId, Rect)> = vec![(TargetId::Main, main)];
    targets.extend(others.iter().map(|(p, r)| (TargetId::Pane(*p), *r)));

    let (dcx, dcy) = dragged.center();
    let mut best: Option<(Decision, u8, f32, f32)> = None; // (decision, unsnapped axes, cost, centre distance)

    for (id, t) in targets {
        let bx = best_axis(dragged.x, dragged.right(), t.x, t.right(), threshold, SnapEdge::Left, SnapEdge::Right);
        let by = best_axis(dragged.y, dragged.bottom(), t.y, t.bottom(), threshold, SnapEdge::Top, SnapEdge::Bottom);
        if bx.is_none() && by.is_none() {
            continue;
        }
        let (x, dx, xe) = bx.map(|v| (v.0, v.1, Some(v.2))).unwrap_or((dragged.x, 0.0, None));
        let (y, dy, ye) = by.map(|v| (v.0, v.1, Some(v.2))).unwrap_or((dragged.y, 0.0, None));
        let rect = Rect::new(x, y, dragged.w, dragged.h);
        let (tcx, tcy) = t.center();
        let dist = ((dcx - tcx).powi(2) + (dcy - tcy).powi(2)).sqrt();
        let cost = dx.abs() + dy.abs();
        let unsnapped = u8::from(xe.is_none()) + u8::from(ye.is_none());

        let vertical = xe.map(|_| {
            let y1 = rect.y.min(t.y);
            let y2 = rect.bottom().max(t.bottom());
            (x, y1, y2)
        });
        let horizontal = ye.map(|_| {
            let x1 = rect.x.min(t.x);
            let x2 = rect.right().max(t.right());
            (y, x1, x2)
        });
        let edge = match (xe, ye) {
            (Some(SnapEdge::Left), Some(SnapEdge::Top)) => SnapEdge::TopLeft,
            (Some(SnapEdge::Right), Some(SnapEdge::Top)) => SnapEdge::TopRight,
            (Some(SnapEdge::Left), Some(SnapEdge::Bottom)) => SnapEdge::BottomLeft,
            (Some(SnapEdge::Right), Some(SnapEdge::Bottom)) => SnapEdge::BottomRight,
            (Some(e), None) | (None, Some(e)) => e,
            _ => SnapEdge::Left,
        };
        let guide = Guide { edge, vertical, horizontal };
        let decision = Decision::Snap { rect, edge, target: id, guide };
        let better = match &best {
            None => true,
            Some((_, u, c, d)) => {
                (unsnapped, cost) < (*u, *c)
                    || ((unsnapped, cost) == (*u, *c) && dist < *d)
            }
        };
        if better {
            best = Some((decision, unsnapped, cost, dist));
        }
    }

    match best {
        Some((d, _, _, _)) => d,
        None => Decision::Free { rect: dragged },
    }
}

/// Placement for a freshly detached pane: beside the main window when the
/// monitor has room, cascaded when it does not. `monitor` may be `None`
/// (unknown geometry) in which case only the main rectangle is used.
///
/// Returns `None` when there is no geometry at all (caller centres instead).
pub fn beside_main(
    main: Option<Rect>,
    monitor: Option<Rect>,
    size: (f32, f32),
    cascade: u64,
) -> Option<(f32, f32)> {
    let (w, h) = size;
    let step = (cascade % CASCADE_MOD) as f32 * CASCADE_STEP_PX;
    let (mx, my, mw, mh) = match (main, monitor) {
        (Some(m), _) => (m.x, m.y, m.w, m.h),
        (None, Some(mon)) => (mon.x, mon.y, 0.0, mon.h * 0.6),
        (None, None) => return None,
    };

    let mut x = mx + mw + BESIDE_GAP_PX + step;
    let mut y = my + BESIDE_TOP_PX + step;

    if let Some(mon) = monitor {
        if x + w > mon.right() - MONITOR_MARGIN_PX {
            x = (mx + 48.0 + step)
                .min(mon.right() - w - MONITOR_MARGIN_PX)
                .max(mon.x + MONITOR_MARGIN_PX);
            let below = my + mh + BESIDE_GAP_PX + step;
            y = if below + h <= mon.bottom() - 48.0 {
                below
            } else {
                (my + 48.0 + step)
                    .min((mon.bottom() - h - 48.0).max(mon.y + MONITOR_MARGIN_PX))
            };
        }
        x = x.max(mon.x + MONITOR_MARGIN_PX);
        y = y.max(mon.y + MONITOR_MARGIN_PX);
    }
    Some((x, y))
}

// ------------------------------------------------ document windows (windows.js)

/// The target a document window was opened for — `windows.js`'s `target()`
/// (`__CS_DOC__` / query string `win=doc&node|file&line&mode`).
///
/// `windows.js:98-131` normalises the injected `window.__CS_DOC__` first and
/// falls back to `URLSearchParams`; the native fork has no webview injection, so
/// the query-string form is the one canonical representation here, and
/// [`DocTarget::parse_query`] / [`DocTarget::query`] are exact inverses.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct DocTarget {
    /// `node` parameter (a node id/ref), when the document is a node document.
    pub node: Option<String>,
    /// `file` parameter, when the document is a file document.
    pub file: Option<String>,
    /// 1-based line (`0` when absent, like `Number(q.get('line')) || 0`).
    pub line: u32,
    /// `doc` | `raw` | `effective`; default `doc` (`windows.js:126`).
    pub mode: String,
}

impl DocTarget {
    /// `windows.js:172` `openDoc(node, mode)` — a node document.
    pub fn for_node(node: impl Into<String>, mode: impl Into<String>) -> Self {
        let mode = mode.into();
        DocTarget {
            node: Some(node.into()),
            file: None,
            line: 0,
            mode: if mode.is_empty() { "doc".to_string() } else { mode },
        }
    }

    /// `windows.js:158-167` `splitFileLine(s)` — split a `path:line` string into
    /// a file target; `None` when there is no positive line suffix.
    pub fn split_file_line(s: &str) -> Option<Self> {
        let (file, line) = s.rsplit_once(':')?;
        if file.is_empty() {
            return None;
        }
        let line = line.parse::<u32>().ok().filter(|n| *n > 0)?;
        Some(DocTarget {
            node: None,
            file: Some(file.to_string()),
            line,
            mode: "doc".to_string(),
        })
    }

    /// `windows.js:169-177` `docQuery(node, mode)` — the canonical query string.
    pub fn query(&self) -> String {
        let mut out = String::from("win=doc");
        if let Some(node) = &self.node {
            out.push_str("&node=");
            out.push_str(node);
        }
        if let Some(file) = &self.file {
            out.push_str("&file=");
            out.push_str(file);
        }
        if self.line > 0 {
            out.push_str("&line=");
            out.push_str(&self.line.to_string());
        }
        if !self.mode.is_empty() {
            out.push_str("&mode=");
            out.push_str(&self.mode);
        }
        out
    }

    /// Inverse of [`DocTarget::query`]: `windows.js`'s `target()` query fallback.
    /// `None` unless the query says `win=doc`.
    pub fn parse_query(query: &str) -> Option<Self> {
        let mut target = DocTarget {
            node: None,
            file: None,
            line: 0,
            mode: String::new(),
        };
        let mut is_doc = false;
        for pair in query.trim_start_matches('?').split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k {
                "win" => is_doc = v == "doc",
                "node" => target.node = (!v.is_empty()).then(|| v.to_string()),
                "file" => target.file = (!v.is_empty()).then(|| v.to_string()),
                "line" => target.line = v.parse::<u32>().unwrap_or(0),
                "mode" => target.mode = v.to_string(),
                _ => {}
            }
        }
        if !is_doc {
            return None;
        }
        if target.mode.is_empty() {
            target.mode = "doc".to_string();
        }
        Some(target)
    }

    /// `windows.js:106-118` `isDocWindow` — true for a document-window target.
    pub fn is_document_window(&self) -> bool {
        true
    }

    /// A stable identity for the window registry: two `openDoc` calls for the
    /// same node+mode are the SAME document window (the reference creates a new
    /// Tauri window per call, but one window per document is strictly less
    /// surprising and keeps the fork's `HashMap<Role, _>` registry finite).
    pub fn key(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.node.as_deref().unwrap_or(""),
            self.file.as_deref().unwrap_or(""),
            self.line,
            self.mode
        )
    }

    /// Human title for the window: `windows.js`'s document title is the node id
    /// or `file:line` (`pane_title`-style suffix comes from the host).
    pub fn label(&self) -> String {
        match (&self.node, &self.file) {
            (Some(n), _) => n.clone(),
            (None, Some(f)) if self.line > 0 => format!("{f}:{}", self.line),
            (None, Some(f)) => f.clone(),
            (None, None) => "document".to_string(),
        }
    }
}

// --------------------------------------------- floating pane geometry memory

/// Per-pane floating geometry memory: the position/size a detached pane window
/// had when it was last docked, so re-detaching restores it instead of throwing
/// the pane back to a default cascade slot. In-memory only — this module has no
/// store, and the host's settings JSON carries no window section (stated
/// honestly rather than silently persisting to a file nobody reads).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelGeometry {
    /// Last known outer rectangle per pane, most-recently-seen last.
    last: Vec<(Pane, Rect)>,
}

impl PanelGeometry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the live rectangle of a detached pane (on move / on dock).
    pub fn remember(&mut self, pane: Pane, rect: Rect) {
        if let Some(slot) = self.last.iter_mut().find(|(p, _)| *p == pane) {
            slot.1 = rect;
        } else {
            self.last.push((pane, rect));
        }
    }

    /// The remembered rectangle for `pane`, if any.
    pub fn remembered(&self, pane: Pane) -> Option<Rect> {
        self.last
            .iter()
            .find(|(p, _)| *p == pane)
            .map(|(_, r)| *r)
    }

    pub fn forget(&mut self, pane: Pane) {
        self.last.retain(|(p, _)| *p != pane);
    }

    /// Placement for a pane that is being detached now.
    ///
    /// Restores the remembered rectangle when its title-bar origin is still
    /// inside the monitor (minus a margin), clamped fully on-screen; otherwise
    /// falls back to [`beside_main`]. This is the v2 behaviour in spirit:
    /// `panels.js` asks the host to open a window at the pane's floating rect
    /// and only falls back to a default placement when it has none.
    pub fn place(
        &self,
        pane: Pane,
        main: Option<Rect>,
        monitor: Option<Rect>,
        size: (f32, f32),
        cascade: u64,
    ) -> Option<(f32, f32)> {
        if let Some(prev) = self.remembered(pane) {
            let (mut x, mut y) = (prev.x, prev.y);
            if let Some(mon) = monitor {
                let visible = x + MONITOR_MARGIN_PX < mon.right()
                    && y + MONITOR_MARGIN_PX < mon.bottom()
                    && x > mon.x - MONITOR_MARGIN_PX
                    && y > mon.y - MONITOR_MARGIN_PX;
                if visible {
                    x = x
                        .min(mon.right() - MONITOR_MARGIN_PX)
                        .max(mon.x + MONITOR_MARGIN_PX);
                    y = y
                        .min(mon.bottom() - MONITOR_MARGIN_PX)
                        .max(mon.y + MONITOR_MARGIN_PX);
                    return Some((x, y));
                }
            } else {
                return Some((x, y));
            }
        }
        beside_main(main, monitor, size, cascade)
    }
}

impl PanelGeometry {
    /// The remembered `(pane, rect)` pairs, most-recently-seen last. The
    /// viewer-state store serialises exactly this list.
    pub fn entries(&self) -> &[(Pane, Rect)] {
        &self.last
    }

    /// Rebuild from persisted entries, tolerating duplicates (last wins).
    pub fn from_entries(entries: Vec<(Pane, Rect)>) -> Self {
        let mut me = Self::default();
        for (pane, rect) in entries {
            me.remember(pane, rect);
        }
        me
    }
}

// ---------------------------------------------------------------- dock zones

/// Each edge dock zone is this fraction of the main rectangle's corresponding
/// dimension deep; the centre zone is the remaining middle.
pub const DOCK_ZONE_FRACTION: f32 = 0.25;

/// Named dock targets on the main window (`panels.js`'s `dockZoneAt` /
/// `dockAffinity`): four edge strips plus a centre target that holds a
/// multi-pane stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DockZone {
    Left,
    Right,
    Top,
    Bottom,
    Center,
}

impl DockZone {
    pub const ALL: [DockZone; 5] = [
        DockZone::Left,
        DockZone::Right,
        DockZone::Top,
        DockZone::Bottom,
        DockZone::Center,
    ];

    pub fn id(self) -> &'static str {
        match self {
            DockZone::Left => "left",
            DockZone::Right => "right",
            DockZone::Top => "top",
            DockZone::Bottom => "bottom",
            DockZone::Center => "center",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        match s {
            "left" => Some(DockZone::Left),
            "right" => Some(DockZone::Right),
            "top" => Some(DockZone::Top),
            "bottom" => Some(DockZone::Bottom),
            "center" => Some(DockZone::Center),
            _ => None,
        }
    }
}

/// One row of v2's dock-affinity table, keeping the `panels.js` literals so the
/// translation into this module's five-zone model is checkable rather than
/// guessed (`panels.js:34-37` for `PANES`, `panels.js:344-365` for
/// `dockZoneAt`/`dockAffinity`).
#[allow(dead_code)] // `slot`/`v2_zone` are the v2 provenance oracle read by the tests.
pub struct V2DockAffinity {
    /// `PANES[].id` in `panels.js`.
    pub pane: &'static str,
    /// `PANES[].slot`: `col`, `row` or `abs`.
    pub slot: &'static str,
    /// The zone literal v2's `dockZoneAt` returns for this slot: `side`,
    /// `source`, or the pane's own id for an `abs` pane over the canvas.
    pub v2_zone: &'static str,
    /// The equivalent zone in this module's generic five-zone model.
    pub zones: &'static [DockZone],
}

/// v2's per-panel dock affinity (`panels.js`), translated to five zones.
///
/// v2 is not a 5x4 zone matrix: each pane has exactly one home target. A `row`
/// pane is offered the `source` strip (bottom 32% of the layout), a `col` pane
/// the `side` strip (right 22%), and an `abs` pane over the canvas is offered
/// its own id with `#canvasWrap` as target; `dockAffinity` accepts exactly that
/// home target and refuses everything else, so a refused drop keeps the pane
/// floating (`rememberFloating`).
///
/// Translation: v2 `source` -> [`DockZone::Bottom`] (the bottom row); v2 `abs`
/// panes -> [`DockZone::Center`] (the canvas). The v2 `side`/`col` panel has no
/// [`Pane`] in this build — the sidebar is painted by the main window — so no
/// row maps it to [`DockZone::Right`]. `blocks` is v3-only and follows the
/// canvas-overlay rule.
pub const V2_DOCK_AFFINITY: &[V2DockAffinity] = &[
    V2DockAffinity {
        pane: "source",
        slot: "row",
        v2_zone: "source",
        zones: &[DockZone::Bottom],
    },
    V2DockAffinity {
        pane: "nodeDrawer",
        slot: "abs",
        v2_zone: "nodeDrawer",
        zones: &[DockZone::Center],
    },
    V2DockAffinity {
        pane: "findPanel",
        slot: "abs",
        v2_zone: "findPanel",
        zones: &[DockZone::Center],
    },
    V2DockAffinity {
        pane: "blocks",
        slot: "abs",
        v2_zone: "blocks",
        zones: &[DockZone::Center],
    },
];

/// A dock zone and the snap rectangle it covers on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockZoneRect {
    pub zone: DockZone,
    pub rect: Rect,
}

/// The dock targets derived from the live main-window rectangle. The five
/// rectangles tile the main window: four `DOCK_ZONE_FRACTION`-deep edges and a
/// centre target between them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DockZones {
    zones: Vec<DockZoneRect>,
}

impl DockZones {
    pub fn from_main(main: Rect) -> Self {
        let f = DOCK_ZONE_FRACTION;
        let w = main.w.max(0.0);
        let h = main.h.max(0.0);
        let zw = w * f;
        let zh = h * f;
        let (cx, cy) = (main.x + zw, main.y + zh);
        let (cw, ch) = (w - 2.0 * zw, h - 2.0 * zh);
        Self {
            zones: vec![
                DockZoneRect { zone: DockZone::Left, rect: Rect::new(main.x, cy, zw, ch) },
                DockZoneRect {
                    zone: DockZone::Right,
                    rect: Rect::new(main.x + w - zw, cy, zw, ch),
                },
                DockZoneRect { zone: DockZone::Top, rect: Rect::new(cx, main.y, cw, zh) },
                DockZoneRect {
                    zone: DockZone::Bottom,
                    rect: Rect::new(cx, main.y + h - zh, cw, zh),
                },
                DockZoneRect { zone: DockZone::Center, rect: Rect::new(cx, cy, cw, ch) },
            ],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.zones.is_empty()
    }

    pub fn rect(&self, zone: DockZone) -> Option<Rect> {
        self.zones.iter().find(|z| z.zone == zone).map(|z| z.rect)
    }

    /// The rectangle a pane snaps to when dropped on `zone`.
    pub fn snap_rect(&self, zone: DockZone) -> Option<Rect> {
        self.rect(zone)
    }

    /// Zone containing the drop point. `None` means the drop is outside every
    /// zone, so the caller keeps the pane free-floating.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<DockZone> {
        if let Some(z) = self
            .zones
            .iter()
            .find(|z| z.zone == DockZone::Center && z.rect.contains(x, y))
        {
            return Some(z.zone);
        }
        self.zones
            .iter()
            .find(|z| z.rect.contains(x, y))
            .map(|z| z.zone)
    }

    /// Zone containing the drop point **that `pane` is allowed to dock into**
    /// — v2's `dockZoneAt` plus its `dockAffinity` gate in one step
    /// (`panels.js:322-323`). A point over a zone this pane has no affinity for
    /// reports `None`, so the caller keeps the pane free-floating (v2's
    /// `rememberFloating`), exactly as if the drop had missed every zone.
    pub fn hit_test_for(&self, pane: Pane, x: f32, y: f32) -> Option<DockZone> {
        self.hit_test(x, y).filter(|zone| pane.can_dock(*zone))
    }
}

/// Which dock zone each pane lives in and the stack order inside it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DockAssignment {
    stacks: Vec<(DockZone, Vec<Pane>)>,
}

impl DockAssignment {
    pub fn new() -> Self {
        Self::default()
    }

    /// Put `pane` on the end of `zone`'s stack, removing it from any other.
    ///
    /// This is the raw assignment: it is also what [`DockAssignment::from_stacks`]
    /// uses to restore a persisted layout, so an old blob that predates dock
    /// affinity is loaded as saved rather than silently rewritten. The
    /// interactive drop path goes through [`PanelLayout::drop_at`], which
    /// enforces [`Pane::can_dock`] first.
    pub fn assign(&mut self, pane: Pane, zone: DockZone) {
        self.unassign(pane);
        match self.stacks.iter_mut().find(|(z, _)| *z == zone) {
            Some((_, stack)) => {
                if !stack.contains(&pane) {
                    stack.push(pane);
                }
            }
            None => self.stacks.push((zone, vec![pane])),
        }
        self.stacks.sort_by_key(|(z, _)| *z);
    }

    /// Remove `pane` from whatever stack holds it (it becomes free-floating).
    pub fn unassign(&mut self, pane: Pane) {
        for (_, stack) in self.stacks.iter_mut() {
            stack.retain(|p| *p != pane);
        }
        self.stacks.retain(|(_, stack)| !stack.is_empty());
    }

    pub fn zone_of(&self, pane: Pane) -> Option<DockZone> {
        self.stacks
            .iter()
            .find(|(_, stack)| stack.contains(&pane))
            .map(|(z, _)| *z)
    }

    pub fn stack(&self, zone: DockZone) -> &[Pane] {
        self.stacks
            .iter()
            .find(|(z, _)| *z == zone)
            .map(|(_, s)| s.as_slice())
            .unwrap_or(&[])
    }

    /// The assigned stacks in `DockZone::ALL` order, for persistence.
    pub fn stacks(&self) -> Vec<(DockZone, Vec<Pane>)> {
        let mut out: Vec<(DockZone, Vec<Pane>)> = DockZone::ALL
            .iter()
            .filter_map(|z| {
                let s = self.stack(*z);
                (!s.is_empty()).then(|| (*z, s.to_vec()))
            })
            .collect();
        out.sort_by_key(|(z, _)| *z);
        out
    }

    /// Rebuild from persisted stacks, ignoring panes that appear more than once
    /// (first assignment wins) and empty stacks.
    pub fn from_stacks(stacks: Vec<(DockZone, Vec<Pane>)>) -> Self {
        let mut me = Self::default();
        for (zone, panes) in stacks {
            for pane in panes {
                if me.zone_of(pane).is_none() {
                    me.assign(pane, zone);
                }
            }
        }
        me
    }
}

/// Everything about the panel layout that survives a restart: the floating
/// panes' rectangles plus the dock assignment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PanelLayout {
    pub geometry: PanelGeometry,
    pub docks: DockAssignment,
}

impl PanelLayout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle a drop of `pane` at `(x, y)` against `zones`, honouring the
    /// pane's v2 dock affinity. Over an **allowed** zone the pane joins that
    /// zone's stack and stops floating. Outside every zone **or over a zone
    /// this pane may not dock into** the caller keeps the existing
    /// free-float/dock decision, and this returns `None` without touching the
    /// assignment — v2's `dockZoneAt` + `dockAffinity` + `rememberFloating`
    /// sequence (`panels.js:322-323`).
    pub fn drop_at(&mut self, pane: Pane, x: f32, y: f32, zones: &DockZones) -> Option<DockZone> {
        let zone = zones.hit_test_for(pane, x, y)?;
        self.docks.assign(pane, zone);
        self.geometry.forget(pane);
        Some(zone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn main_rect() -> Rect {
        Rect::new(100.0, 100.0, 800.0, 600.0)
    }

    #[test]
    fn snaps_left_edge_within_threshold() {
        // Dragged window far below main, left edge 3 px from main's left.
        let d = decide(Rect::new(97.0, 2000.0, 400.0, 300.0), main_rect(), &[], SNAP_THRESHOLD_PX);
        match d {
            Decision::Snap { rect, edge, target, .. } => {
                assert_eq!(rect.x, 100.0);
                assert_eq!(edge, SnapEdge::Left);
                assert_eq!(target, TargetId::Main);
            }
            other => panic!("expected snap, got {other:?}"),
        }
    }

    #[test]
    fn no_snap_beyond_threshold() {
        // left edge 17 px away -> free.
        let d = decide(Rect::new(83.0, 2000.0, 400.0, 300.0), main_rect(), &[], SNAP_THRESHOLD_PX);
        assert!(matches!(d, Decision::Free { .. }), "{d:?}");
    }

    #[test]
    fn snaps_adjacent_right_of_main() {
        // dragged.left 4 px from main.right -> flush adjacency (rect.x == main.right).
        let d = decide(Rect::new(904.0, 2000.0, 300.0, 400.0), main_rect(), &[], SNAP_THRESHOLD_PX);
        match d {
            Decision::Snap { rect, .. } => assert_eq!(rect.x, 900.0),
            other => panic!("expected snap, got {other:?}"),
        }
    }

    #[test]
    fn snaps_right_edges_together() {
        // dragged.right 5 px from main.right -> right edges align.
        let d = decide(Rect::new(495.0, 2000.0, 400.0, 200.0), main_rect(), &[], SNAP_THRESHOLD_PX);
        match d {
            Decision::Snap { rect, edge, .. } => {
                assert_eq!(rect.right(), 900.0);
                assert_eq!(edge, SnapEdge::Right);
            }
            other => panic!("expected snap, got {other:?}"),
        }
    }

    #[test]
    fn snaps_corner_when_both_axes_match_same_target() {
        // A 300x200 window whose bottom-right corner is 2 px right / 1 px below
        // main's top-left, with no meaningful overlap -> corner snap.
        let d = decide(Rect::new(-198.0, -99.0, 300.0, 200.0), main_rect(), &[], SNAP_THRESHOLD_PX);
        match d {
            Decision::Snap { rect, edge, guide, .. } => {
                assert_eq!((rect.right(), rect.bottom()), (100.0, 100.0));
                assert_eq!(edge, SnapEdge::BottomRight);
                assert!(guide.vertical.is_some() && guide.horizontal.is_some());
            }
            other => panic!("expected corner snap, got {other:?}"),
        }
    }

    #[test]
    fn docks_when_mostly_over_main() {
        // Fully inside main: 100% overlap -> dock.
        let d = decide(Rect::new(300.0, 250.0, 400.0, 300.0), main_rect(), &[], SNAP_THRESHOLD_PX);
        match d {
            Decision::Dock { overlap, target } => {
                assert!(overlap >= DOCK_MIN_OVERLAP);
                assert_eq!(target, TargetId::Main);
            }
            other => panic!("expected dock, got {other:?}"),
        }
    }

    #[test]
    fn docks_when_center_inside_even_under_half_overlap() {
        // 100x100 window whose centre (880,680) is inside main, but only 49% of
        // its area overlaps -> docks by the centre rule.
        let d = decide(Rect::new(830.0, 630.0, 100.0, 100.0), main_rect(), &[], SNAP_THRESHOLD_PX);
        match d {
            Decision::Dock { .. } => {}
            other => panic!("expected dock by centre, got {other:?}"),
        }
    }

    #[test]
    fn does_not_dock_at_forty_percent_overlap() {
        // 40% overlap and the centre outside main -> stays detached.
        let dragged = Rect::new(860.0, 300.0, 100.0, 100.0);
        assert!(dragged.overlap_ratio(&main_rect()) < DOCK_MIN_OVERLAP);
        let d = decide(dragged, main_rect(), &[], SNAP_THRESHOLD_PX);
        assert!(!matches!(d, Decision::Dock { .. }), "{d:?}");
    }

    #[test]
    fn snaps_to_another_pane_window() {
        let other = Rect::new(1500.0, 200.0, 400.0, 300.0);
        let d = decide(
            Rect::new(1503.0, 640.0, 300.0, 200.0), // left edge 3px off `other`, y far
            main_rect(),
            &[(Pane::Find, other)],
            SNAP_THRESHOLD_PX,
        );
        match d {
            Decision::Snap { rect, target, .. } => {
                assert_eq!(rect.x, 1500.0);
                assert_eq!(target, TargetId::Pane(Pane::Find));
            }
            other => panic!("expected snap to pane, got {other:?}"),
        }
    }

    #[test]
    fn nearest_target_wins_on_cost_tie() {
        let near = Rect::new(200.0, 1000.0, 300.0, 200.0);
        let far = Rect::new(200.0, 3000.0, 300.0, 200.0);
        let d = decide(
            Rect::new(202.0, 1002.0, 100.0, 100.0),
            main_rect(),
            &[(Pane::Source, near), (Pane::Blocks, far)],
            SNAP_THRESHOLD_PX,
        );
        match d {
            Decision::Snap { target, .. } => assert_eq!(target, TargetId::Pane(Pane::Source)),
            other => panic!("expected snap, got {other:?}"),
        }
    }

    #[test]
    fn beside_main_uses_documented_gap_and_offset() {
        let mon = Rect::new(0.0, 0.0, 3840.0, 2160.0);
        let (x, y) = beside_main(Some(main_rect()), Some(mon), (440.0, 620.0), 0).unwrap();
        assert_eq!(x, 100.0 + 800.0 + BESIDE_GAP_PX);
        assert_eq!(y, 100.0 + BESIDE_TOP_PX);
        let (x3, y3) = beside_main(Some(main_rect()), Some(mon), (440.0, 620.0), 3).unwrap();
        assert_eq!(x3 - x, 3.0 * CASCADE_STEP_PX);
        assert_eq!(y3 - y, 3.0 * CASCADE_STEP_PX);
    }

    #[test]
    fn beside_main_cascades_when_no_room_right() {
        let mon = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        // main nearly fills the monitor: no column to the right.
        let main = Rect::new(0.0, 0.0, 1900.0, 1040.0);
        let (x, y) = beside_main(Some(main), Some(mon), (440.0, 300.0), 0).unwrap();
        assert!(x >= mon.x + MONITOR_MARGIN_PX);
        assert!(x + 440.0 <= mon.right() - MONITOR_MARGIN_PX || x == mon.right() - 440.0 - MONITOR_MARGIN_PX);
        assert!(y >= mon.y + MONITOR_MARGIN_PX);
    }

    #[test]
    fn beside_main_clamps_without_monitor() {
        assert_eq!(beside_main(None, None, (100.0, 100.0), 0), None);
        let (x, y) = beside_main(Some(Rect::new(10.0, 20.0, 100.0, 50.0)), None, (10.0, 10.0), 0).unwrap();
        assert_eq!((x, y), (10.0 + 100.0 + BESIDE_GAP_PX, 20.0 + BESIDE_TOP_PX));
    }

    #[test]
    fn pane_sizes_match_the_original_host() {
        assert_eq!(Pane::Source.default_size(), (1180.0, 860.0));
        assert_eq!(Pane::NodeDrawer.default_size(), (440.0, 620.0));
        assert_eq!(Pane::Find.default_size(), (560.0, 720.0));
        assert_eq!(Pane::Blocks.default_size(), (720.0, 640.0));
        assert_eq!(Pane::Source.title(), "Source");
        assert_eq!(Pane::NodeDrawer.id(), "nodeDrawer");
    }
}

/// Document-window targets and floating-pane geometry memory (task-17).
///
/// These are the pure halves of `windows.js` (document target/query) and
/// `panels.js` (float geometry restore). They are deliberately separate from the
/// snap/dock tests above: no winit, no egui, no I/O.
#[cfg(test)]
mod window_role_tests {
    use super::*;

    #[test]
    fn doc_target_query_round_trips_windows_js_form() {
        let t = DocTarget::for_node("#ns:Alpha", "doc");
        assert_eq!(t.query(), "win=doc&node=#ns:Alpha&mode=doc");
        assert_eq!(DocTarget::parse_query(&t.query()), Some(t.clone()));

        let raw = DocTarget::for_node("N.M", "raw");
        assert_eq!(raw.query(), "win=doc&node=N.M&mode=raw");
        assert_eq!(DocTarget::parse_query(&raw.query()), Some(raw));
    }

    #[test]
    fn doc_target_file_line_split_matches_windows_js() {
        let t = DocTarget::split_file_line("src/A.cs:42").expect("file:line");
        assert_eq!(t.file.as_deref(), Some("src/A.cs"));
        assert_eq!(t.line, 42);
        assert_eq!(t.node, None);
        assert_eq!(t.query(), "win=doc&file=src/A.cs&line=42&mode=doc");
        assert_eq!(DocTarget::parse_query(&t.query()), Some(t));
        // `windows.js:158-167`: a negative/zero/non-numeric suffix is not a target.
        assert_eq!(DocTarget::split_file_line("src/A.cs:0"), None);
        assert_eq!(DocTarget::split_file_line("src/A.cs"), None);
    }

    #[test]
    fn parse_query_refuses_non_document_windows_and_defaults_doc_mode() {
        assert_eq!(DocTarget::parse_query("win=pane&node=A"), None);
        assert_eq!(DocTarget::parse_query("node=A"), None);
        assert_eq!(
            DocTarget::parse_query("?win=doc&node=A"),
            Some(DocTarget {
                node: Some("A".into()),
                file: None,
                line: 0,
                mode: "doc".into()
            })
        );
    }

    #[test]
    fn doc_target_key_identifies_the_document_window() {
        let a = DocTarget::for_node("A", "doc");
        let b = DocTarget::for_node("A", "doc");
        let raw = DocTarget::for_node("A", "raw");
        assert_eq!(a.key(), b.key(), "same node+mode is the same window");
        assert_ne!(a.key(), raw.key(), "a different mode is a different window");
        assert_eq!(a.label(), "A");
        assert_eq!(DocTarget::split_file_line("/x/A.cs:7").unwrap().label(), "/x/A.cs:7");
    }

    #[test]
    fn panel_geometry_remembers_and_restores_a_detached_rect() {
        let mut geo = PanelGeometry::new();
        assert_eq!(geo.remembered(Pane::Find), None);
        geo.remember(Pane::Find, Rect::new(300.0, 200.0, 560.0, 720.0));
        assert_eq!(
            geo.remembered(Pane::Find),
            Some(Rect::new(300.0, 200.0, 560.0, 720.0))
        );
        let main = Rect::new(100.0, 100.0, 800.0, 600.0);
        let monitor = Rect::new(0.0, 0.0, 1920.0, 1080.0);
        // Re-detaching restores the remembered top-left rather than cascading.
        assert_eq!(
            geo.place(Pane::Find, Some(main), Some(monitor), Pane::Find.default_size(), 3),
            Some((300.0, 200.0))
        );
        // A memory that ended up off-monitor falls back to beside_main.
        geo.remember(Pane::Find, Rect::new(5000.0, 4000.0, 560.0, 720.0));
        assert_eq!(
            geo.place(Pane::Find, Some(main), Some(monitor), Pane::Find.default_size(), 0),
            beside_main(Some(main), Some(monitor), Pane::Find.default_size(), 0)
        );
        // Unremembered panes always cascade beside the main window.
        assert_eq!(
            geo.place(Pane::Source, Some(main), Some(monitor), Pane::Source.default_size(), 0),
            beside_main(Some(main), Some(monitor), Pane::Source.default_size(), 0)
        );
        // Remembering twice replaces rather than duplicates, and forget drops it.
        geo.remember(Pane::Find, Rect::new(10.0, 20.0, 30.0, 40.0));
        assert_eq!(geo.remembered(Pane::Find).unwrap().x, 10.0);
        geo.forget(Pane::Find);
        assert_eq!(geo.remembered(Pane::Find), None);
    }
}

/// Dock zones + layout persistence halves (task-10d).
#[cfg(test)]
mod dock_zone_tests {
    use super::*;

    fn main_rect() -> Rect {
        Rect::new(100.0, 100.0, 800.0, 600.0)
    }

    #[test]
    fn dock_zones_tile_the_main_window_and_hit_test_each_target() {
        let z = DockZones::from_main(main_rect());
        // 25% strips: 200 px left/right, 150 px top/bottom.
        assert_eq!(z.rect(DockZone::Left), Some(Rect::new(100.0, 250.0, 200.0, 300.0)));
        assert_eq!(z.rect(DockZone::Right), Some(Rect::new(700.0, 250.0, 200.0, 300.0)));
        assert_eq!(z.rect(DockZone::Top), Some(Rect::new(300.0, 100.0, 400.0, 150.0)));
        assert_eq!(z.rect(DockZone::Bottom), Some(Rect::new(300.0, 550.0, 400.0, 150.0)));
        assert_eq!(z.rect(DockZone::Center), Some(Rect::new(300.0, 250.0, 400.0, 300.0)));
        assert_eq!(z.hit_test(150.0, 400.0), Some(DockZone::Left));
        assert_eq!(z.hit_test(850.0, 400.0), Some(DockZone::Right));
        assert_eq!(z.hit_test(500.0, 180.0), Some(DockZone::Top));
        assert_eq!(z.hit_test(500.0, 680.0), Some(DockZone::Bottom));
        assert_eq!(z.hit_test(500.0, 400.0), Some(DockZone::Center));
        // Outside the main window: no zone, so the pane stays free-floating.
        assert_eq!(z.hit_test(50.0, 400.0), None);
        assert_eq!(z.hit_test(500.0, 950.0), None);
        // Every zone has a snap rectangle.
        for zone in DockZone::ALL {
            assert!(z.snap_rect(zone).is_some(), "{zone:?}");
        }
        // Ids round-trip (the persistence format uses them).
        for zone in DockZone::ALL {
            assert_eq!(DockZone::from_id(zone.id()), Some(zone));
        }
        assert_eq!(DockZone::from_id("somewhere"), None);
    }

    #[test]
    fn dock_assignment_moves_panes_between_zone_stacks() {
        let mut d = DockAssignment::new();
        d.assign(Pane::Find, DockZone::Left);
        d.assign(Pane::Source, DockZone::Left);
        assert_eq!(d.stack(DockZone::Left), &[Pane::Find, Pane::Source]);
        // Re-assigning moves the pane out of its old stack.
        d.assign(Pane::Find, DockZone::Center);
        assert_eq!(d.stack(DockZone::Left), &[Pane::Source]);
        assert_eq!(d.stack(DockZone::Center), &[Pane::Find]);
        assert_eq!(d.zone_of(Pane::Find), Some(DockZone::Center));
        assert_eq!(d.zone_of(Pane::Blocks), None);
        d.unassign(Pane::Source);
        assert_eq!(d.zone_of(Pane::Source), None);
        assert!(d.stack(DockZone::Left).is_empty());
        // Persisted stacks restore; duplicate panes are dropped, not doubled.
        let restored = DockAssignment::from_stacks(vec![
            (DockZone::Center, vec![Pane::Find, Pane::Blocks]),
            (DockZone::Top, vec![Pane::Blocks]),
        ]);
        assert_eq!(restored.stack(DockZone::Center), &[Pane::Find, Pane::Blocks]);
        assert_eq!(restored.zone_of(Pane::Blocks), Some(DockZone::Center));
        assert_eq!(d.stacks(), vec![(DockZone::Center, vec![Pane::Find])]);
    }

    #[test]
    fn panel_layout_drop_assigns_zone_stacks_and_keeps_free_float() {
        let mut layout = PanelLayout::new();
        layout.geometry.remember(Pane::Find, Rect::new(20.0, 30.0, 400.0, 500.0));
        let zones = DockZones::from_main(main_rect());
        // Over the centre zone: joins that stack and stops floating.
        assert_eq!(layout.drop_at(Pane::Find, 500.0, 400.0, &zones), Some(DockZone::Center));
        assert_eq!(layout.docks.zone_of(Pane::Find), Some(DockZone::Center));
        assert_eq!(layout.geometry.remembered(Pane::Find), None);
        // A second pane dropped on the same zone stacks with it.
        assert_eq!(layout.drop_at(Pane::Blocks, 500.0, 400.0, &zones), Some(DockZone::Center));
        assert_eq!(layout.docks.stack(DockZone::Center), &[Pane::Find, Pane::Blocks]);
        // Outside every zone: free-float, assignment untouched.
        let before = layout.clone();
        assert_eq!(layout.drop_at(Pane::Source, 5000.0, 5000.0, &zones), None);
        assert_eq!(layout, before);
    }

    #[test]
    fn dock_affinity_table_matches_the_v2_panels_js_literals() {
        // `panels.js:34-37` PANES: source=row, nodeDrawer/findPanel=abs.
        // `panels.js:360-365` dockAffinity: `zone === p.id`, or source/row, or
        // side/col — each pane has exactly one home target.
        let rows: Vec<(&str, &str, &str)> = V2_DOCK_AFFINITY
            .iter()
            .map(|r| (r.pane, r.slot, r.v2_zone))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("source", "row", "source"),
                ("nodeDrawer", "abs", "nodeDrawer"),
                ("findPanel", "abs", "findPanel"),
                ("blocks", "abs", "blocks"),
            ]
        );
        // Translation: v2's `source` strip is the bottom row; the abs panes
        // target `#canvasWrap`, i.e. the centre. v2's `side`/col panel has no
        // `Pane` in this build.
        assert_eq!(Pane::Source.dock_affinity(), &[DockZone::Bottom]);
        assert_eq!(Pane::NodeDrawer.dock_affinity(), &[DockZone::Center]);
        assert_eq!(Pane::Find.dock_affinity(), &[DockZone::Center]);
        assert_eq!(Pane::Blocks.dock_affinity(), &[DockZone::Center]);
        assert!(Pane::Source.can_dock(DockZone::Bottom));
        for zone in [DockZone::Left, DockZone::Right, DockZone::Top, DockZone::Center] {
            assert!(!Pane::Source.can_dock(zone), "source must not dock {zone:?}");
        }
        for zone in [DockZone::Left, DockZone::Right, DockZone::Top, DockZone::Bottom] {
            for pane in [Pane::NodeDrawer, Pane::Find, Pane::Blocks] {
                assert!(!pane.can_dock(zone), "{pane:?} must not dock {zone:?}");
            }
        }
    }

    #[test]
    fn a_drop_on_a_disallowed_zone_keeps_the_pane_free_floating() {
        let mut layout = PanelLayout::new();
        layout.docks.assign(Pane::Find, DockZone::Bottom);
        let zones = DockZones::from_main(main_rect());
        let before = layout.clone();
        // Source may only dock bottom; a drop over the left/right strips or the
        // centre is refused, leaving the layout exactly as it was.
        assert_eq!(layout.drop_at(Pane::Source, 150.0, 400.0, &zones), None);
        assert_eq!(layout.drop_at(Pane::Source, 850.0, 400.0, &zones), None);
        assert_eq!(layout.drop_at(Pane::Source, 500.0, 400.0, &zones), None);
        assert_eq!(layout, before);
        // A canvas-overlay pane may only dock the centre, never an edge strip.
        assert_eq!(layout.drop_at(Pane::Find, 500.0, 680.0, &zones), None);
        assert_eq!(layout.drop_at(Pane::Find, 150.0, 400.0, &zones), None);
        assert_eq!(layout, before);
        // The hit-test gate reports the same refusal as the drop path.
        assert_eq!(zones.hit_test_for(Pane::Source, 150.0, 400.0), None);
        assert_eq!(zones.hit_test_for(Pane::Source, 500.0, 400.0), None);
        assert_eq!(
            zones.hit_test_for(Pane::Source, 500.0, 680.0),
            Some(DockZone::Bottom)
        );
    }

    #[test]
    fn an_allowed_drop_docks_into_the_affine_home_zone() {
        let mut layout = PanelLayout::new();
        let zones = DockZones::from_main(main_rect());
        layout.geometry.remember(Pane::Source, Rect::new(20.0, 30.0, 400.0, 300.0));
        assert_eq!(
            layout.drop_at(Pane::Source, 500.0, 680.0, &zones),
            Some(DockZone::Bottom)
        );
        assert_eq!(layout.docks.zone_of(Pane::Source), Some(DockZone::Bottom));
        assert_eq!(layout.geometry.remembered(Pane::Source), None);
        assert_eq!(
            layout.drop_at(Pane::Find, 500.0, 400.0, &zones),
            Some(DockZone::Center)
        );
        // Zone stacks are emitted in the fixed `DockZone::ALL` order, with
        // insertion order preserved inside a zone.
        assert_eq!(layout.docks.stack(DockZone::Center), &[Pane::Find]);
        assert_eq!(
            layout.docks.stacks(),
            vec![
                (DockZone::Bottom, vec![Pane::Source]),
                (DockZone::Center, vec![Pane::Find]),
            ]
        );
    }

    #[test]
    fn a_pre_affinity_saved_layout_loads_tolerantly() {
        // Older blobs could place any pane in any zone under the generic
        // five-zone model. Restore keeps them exactly as saved — no silent
        // rewrite, no lost stack — and only new interactive drops are gated.
        let restored = DockAssignment::from_stacks(vec![
            (DockZone::Top, vec![Pane::Source]),
            (DockZone::Left, vec![Pane::NodeDrawer]),
            (DockZone::Bottom, vec![Pane::Find]),
        ]);
        assert_eq!(restored.zone_of(Pane::Source), Some(DockZone::Top));
        assert_eq!(restored.zone_of(Pane::NodeDrawer), Some(DockZone::Left));
        assert_eq!(restored.zone_of(Pane::Find), Some(DockZone::Bottom));
        assert_eq!(
            restored.stacks(),
            vec![
                (DockZone::Left, vec![Pane::NodeDrawer]),
                (DockZone::Top, vec![Pane::Source]),
                (DockZone::Bottom, vec![Pane::Find]),
            ]
        );
        // Dropping the pane on its affine home later moves it, leaving the
        // other restored stacks alone.
        let mut layout = PanelLayout { docks: restored, ..PanelLayout::default() };
        let zones = DockZones::from_main(main_rect());
        assert_eq!(
            layout.drop_at(Pane::Source, 500.0, 680.0, &zones),
            Some(DockZone::Bottom)
        );
        assert_eq!(layout.docks.zone_of(Pane::Source), Some(DockZone::Bottom));
        assert_eq!(layout.docks.zone_of(Pane::NodeDrawer), Some(DockZone::Left));
    }

    #[test]
    fn default_startup_layout_is_unchanged() {
        // No implicit dock stacks and no geometry: every pane starts floating,
        // exactly as before the affinity layer. A fresh cache root loads this
        // same default (see `pov`'s older-blob tolerance test).
        let layout = PanelLayout::default();
        assert_eq!(layout, PanelLayout::new());
        assert!(layout.docks.stacks().is_empty());
        assert!(layout.geometry.entries().is_empty());
        for pane in Pane::ALL {
            assert_eq!(layout.docks.zone_of(pane), None);
        }
    }

    #[test]
    fn panel_from_id_round_trips_and_tolerates_unknown_ids() {
        for pane in Pane::ALL {
            assert_eq!(Pane::from_id(pane.id()), Some(pane));
        }
        assert_eq!(Pane::from_id("bogus"), None);
    }
}
