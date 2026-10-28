//! The Blocks canvas **producer** (task-13): v2 `scratch.js` cards/ports/edges.
//!
//! v2 rendered the Blocks panel from four read routes: `/api/decls` (the scope's
//! declarations → block cards), `/api/subgraph` (reference edges between those
//! cards), `/api/source` + `/api/node` (the hover peek, already ported as
//! [`crate::blocks_peek`]). This fork holds the whole graph in memory, so the
//! same model is derivable from [`Graph`] with no I/O — exactly the substitution
//! `codelens.rs::decls_for` already makes for `/api/decls`.
//!
//! The scope rule is the reference's `loadScope` (`scratch.js:229-247`): the
//! anchor is the selected node; if it is a member, the cards are its declaring
//! type's declarations; if it is a namespace/type, the cards are its own.
//! Cards are members in **graph order** (the engine's declaration order — the
//! reference never sorts either, `scratch.js:314`), capped at
//! [`MAX_CARDS`] (`scratch.js:33`).
//!
//! Layout is deterministic and pure: the CSS grid does the visual placement
//! (`style.css .blocks-cards` is `grid-template-columns: repeat(auto-fill,
//! minmax(232px, 1fr)); gap: 34px 30px`), and this module derives the **edge
//! SVG** from it. v2 measured the DOM (`scratch.js:378-414 portPoint`); this
//! port now does the same through the shell's measured-geometry channel:
//! `RECTS=` asks the nzk_ui for the painted `.block-card`, `.bc-ports` and
//! `.port-dot` boxes in the same layout pass that paints the nzk_ui, and
//! [`geometry_from_tsv`] turns those `@rect` rows into [`BlocksGeometry`].
//! [`blocks_model_measured`] then anchors every endpoint to the measured port
//! box centre. The constants below remain the **truthful fallback** for a frame
//! the shell has not reported on (one-shot transport, no `RECTS` support, a
//! closed Blocks panel): they are never presented as measurements.
//!
//! Honest emptiness: no anchor / no graph / no declarations ⇒ [`None`] (the
//! shipped empty `#blocksStage` stays untouched), and an anchor with an empty
//! scope is a [`BlocksModel::Empty`] with the reference's own sentence.

use std::cell::RefCell;
use std::collections::BTreeMap;

use crate::graphviz::{kind_color, Rgba};
use crate::model::Graph;

/// The `RECTS=` selectors the host asks the shell for, in the order the shell
/// emits them. Keys are the wire keys of the `@rect` rows below; selectors are
/// deliberately space-free (`>` child combinators) because the request field is
/// space-delimited. `data-id` on every row is the nearest card id, so the host
/// can map a port dot back to its card without embedding ids in selectors.
///
/// The `in` selector matches several dots per card; document order is slot
/// order, which is exactly how `scratch.js` stacks parameter ports.
pub const PORT_RECT_SELECTORS: [(&str, &str); 5] = [
    ("origin", "#blocksCards"),
    ("card", "#blocksCards>.block-card"),
    ("ports", "#blocksCards>.block-card>.bc-ports"),
    ("out", "#blocksCards>.block-card>.bc-ports>.bc-out>.port.port-out>.port-dot"),
    ("in", "#blocksCards>.block-card>.bc-ports>.bc-in>.port.port-in>.port-dot"),
];

/// `scratch.js:33` — `var MAX_CARDS = 160;`
pub const MAX_CARDS: usize = 160;
/// `style.css .blocks-cards` — `minmax(232px, 1fr)`.
pub const CARD_W: f32 = 232.0;
/// `style.css .blocks-cards` — `gap: 34px 30px` (row gap / column gap).
pub const GAP_X: f32 = 30.0;
pub const GAP_Y: f32 = 34.0;
/// Painted card height: the same row pitch [`layout_cards`] uses, so a card's
/// painted box is `(x, y, CARD_W, CARD_H)`.
pub const CARD_H: f32 = CARD_W * 0.72;
/// `style.css .port-dot` is an 8x8 dot — the port's painted anchor.
pub const PORT_DOT_HALF: f32 = 4.0;
/// `.bc-ports` is the row above `.bc-foot` (`style.css:2340-2348`; the card is
/// head/sig/ports/foot), so its vertical centre sits at ~0.70 of the painted
/// card height. This is the fallback anchored before the shell has measured the
/// port row; when [`BlockCard::measured`] is present it is not consulted.
pub const PORT_ROW_FRAC: f32 = 0.70;
/// Columns the edge geometry assumes. The shipped grid is `auto-fill` and its
/// real column count depends on the panel width; 3 is the reference's typical
/// 1180px-wide pane at 232px cards. Fallback only: measured card rectangles
/// replace this layout as soon as the shell reports them.
pub const ASSUMED_COLS: usize = 3;

/// `scratch.js:35-36` — the kinds whose cards are containers.
const TYPE_KINDS: [&str; 6] = ["namespace", "class", "struct", "interface", "enum", "record"];

/// `scratch.js:44-46` — the card kind on the anchor's member scope.
pub fn is_container(kind: &str) -> bool {
    TYPE_KINDS.contains(&kind)
}

/// One rendered block card (`scratch.js:256-301 cardHtml`).
#[derive(Clone, Debug, PartialEq)]
pub struct BlockCard {
    pub id: String,
    pub index: usize,
    pub kind: String,
    /// `d.name` — the graph's label.
    pub name: String,
    /// `d.full || d.id` for the title tooltip.
    pub full: String,
    /// `d.file`'s basename (`shortId`/`basename`, `scratch.js:96-104`).
    pub file: String,
    pub line: i64,
    /// `d.ret` — unknown in the compact graph model; empty (no port type).
    pub ret: String,
    /// Method/constructor parameter types, parsed from the id's `(…)`.
    pub params: Vec<String>,
    /// `d.inDeg` / `d.outDeg` — the graph's `degree` (see [`BlockCard::degree`]).
    pub degree: i64,
    /// Card rectangle for the edge geometry (computed grid slot; the measured
    /// box in [`BlockCard::measured`] supersedes it when present).
    pub x: f32,
    pub y: f32,
    /// Painted geometry reported by the shell for this card this frame, if any.
    /// `None` means the endpoints fall back to the stylesheet constants above.
    pub measured: Option<MeasuredCard>,
}

impl BlockCard {
    /// `shortType` of the out-port: return type, else the kind for containers,
    /// else `value` for fields/properties/events (`scratch.js:267-269`).
    pub fn out_type(&self) -> String {
        if !self.ret.is_empty() {
            return self.ret.clone();
        }
        if is_container(&self.kind) {
            return self.kind.clone();
        }
        match self.kind.as_str() {
            "field" | "property" | "event" => "value".to_string(),
            _ => String::new(),
        }
    }
}

/// One reference edge between two rendered cards (`scratch.js:355-368`).
#[derive(Clone, Debug, PartialEq)]
pub struct BlockEdge {
    pub source: String,
    pub target: String,
    pub kind: String,
    pub count: i64,
    /// SVG path `d` for the cubic drawn in `#blocksEdges` (`scratch.js:399-408`).
    pub path_d: String,
    /// `EDGE_COLOR[e.kind]` fallback `#6b7897` (`scratch.js:36-39`).
    pub color: String,
    /// Port-anchored endpoint centres: `(ax, ay)` is the source's out-port box
    /// centre and `(bx, by)` the target's in-port box centre (`scratch.js:378`).
    pub ax: f32,
    pub ay: f32,
    pub bx: f32,
    pub by: f32,
}

/// The whole Blocks model for one anchor.
#[derive(Clone, Debug, PartialEq)]
pub struct BlocksModel {
    /// `scratch.js:299` — `Blocks · <shortId(scope)>`.
    pub title: String,
    /// `scratch.js:374` — `N blocks · M edges [+K more]`.
    pub meta: String,
    pub cards: Vec<BlockCard>,
    pub edges: Vec<BlockEdge>,
    /// `scratch.js:320-323` — the reference's empty-stage sentence.
    pub empty: String,
}

/// The declaration scope for an anchor (`scratch.js:229-247 loadScope`).
///
/// A member renders its declaring type; a container (and anything that cannot
/// be resolved) renders itself.
pub fn scope_of(graph: &Graph, anchor: &str) -> String {
    let Some(n) = graph.nodes.iter().find(|n| n.id == anchor) else {
        return anchor.to_string();
    };
    if is_container(&n.kind) {
        n.id.clone()
    } else if !n.owner.is_empty() {
        n.owner.clone()
    } else {
        n.id.clone()
    }
}

/// The cards for `scope`: the scope node itself plus every member whose
/// `owner` is the scope, in graph order (`/api/decls`'s order).
pub fn cards_for(graph: &Graph, scope: &str) -> Vec<BlockCard> {
    let mut cards = Vec::new();
    for (i, n) in graph.nodes.iter().enumerate() {
        let member = n.owner == scope;
        let is_scope = n.id == scope;
        if !member && !is_scope {
            continue;
        }
        let params = if n.kind == "method" || n.kind == "constructor" {
            split_params(&n.id)
        } else {
            Vec::new()
        };
        cards.push(BlockCard {
            id: n.id.clone(),
            index: i,
            kind: n.kind.clone(),
            name: n.label.clone(),
            full: if n.full.is_empty() { n.id.clone() } else { n.full.clone() },
            file: basename(&n.file),
            line: n.line,
            ret: String::new(),
            params,
            degree: n.degree,
            x: 0.0,
            y: 0.0,
            measured: None,
        });
        if cards.len() >= MAX_CARDS {
            break;
        }
    }
    cards
}

/// Assign grid rectangles: `row = i / cols`, `col = i % cols`, references the
/// same `minmax(232px,1fr)` + `34px 30px` gaps the stylesheet uses.
pub fn layout_cards(cards: &mut [BlockCard], cols: usize) {
    let cols = cols.max(1);
    for (i, c) in cards.iter_mut().enumerate() {
        let row = i / cols;
        let col = i % cols;
        c.x = col as f32 * (CARD_W + GAP_X);
        c.y = row as f32 * (CARD_W * 0.72 + GAP_Y);
    }
}

/// A painted port box in the cards-layer coordinate space (`scratch.js:378-386
/// portPoint` measures exactly this box).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PortBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl PortBox {
    /// The port's painted centre — what v2's `portPoint` returns.
    pub fn center(&self) -> (f32, f32) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    fn shifted(self, dx: f32, dy: f32) -> Self {
        PortBox { x: self.x - dx, y: self.y - dy, ..self }
    }
}

/// The painted geometry of one Blocks card, as measured by the nzk_ui layout
/// pass (`@rect` rows in the `RECTS=` response) and normalised to the
/// cards-layer origin (`#blocksCards`'s top-left).
///
/// Every field is what the shell actually laid out; `None` means the shell did
/// not report that sub-box this frame, and the corresponding constant fallback
/// stays in force. This is the difference between "measured" and "assumed", and
/// it is carried explicitly rather than flattened into numbers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MeasuredCard {
    /// The `.block-card` border box (grid-placed, so never an assumed column).
    pub card: Option<PortBox>,
    /// The `.bc-ports` row box.
    pub ports: Option<PortBox>,
    /// The right-edge out-port `.port-dot`.
    pub out_dot: Option<PortBox>,
    /// Every in-port `.port-dot`, in document order = parameter slot order.
    pub in_dots: Vec<PortBox>,
}

/// Measured Blocks geometry for one frame, keyed by card `data-id`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BlocksGeometry {
    /// `#blocksCards` size (origin was subtracted from every other box).
    pub origin: PortBox,
    pub cards: BTreeMap<String, MeasuredCard>,
}

impl BlocksGeometry {
    /// The measured card box for `id`, if the shell laid that card out.
    pub fn card(&self, id: &str) -> Option<&MeasuredCard> {
        self.cards.get(id)
    }
}

// The last measured geometry reported by the shell, per thread.
//
// The host builds facts *before* it asks the shell for a frame, so the geometry
// from the previous frame's `@rect` rows is what anchors this frame's edges.
// That is correct rather than stale: card placement is CSS grid layout and does
// not depend on the edge paths, so one frame of feedback reaches the fixed point
// immediately (and `set_measured_geometry` is replaced — never merged — on
// every frame).
thread_local! {
    static MEASURED: RefCell<Option<BlocksGeometry>> = const { RefCell::new(None) };
}

/// Replace the cached measured geometry (or clear it when the shell reported no
/// `@rect` rows — e.g. a closed Blocks panel — so the fallback is honest).
pub fn set_measured_geometry(g: Option<BlocksGeometry>) {
    MEASURED.with(|slot| *slot.borrow_mut() = g);
}

/// The geometry from the previous frame, if any.
pub fn measured_geometry() -> Option<BlocksGeometry> {
    MEASURED.with(|slot| slot.borrow().clone())
}

/// Parse the shell's `@rect` TSV rows into [`BlocksGeometry`].
///
/// Wire shape (one row per matched element, `serve.rs::rect_row`):
/// `@rect\t<key>\t<data-id>\t<x>\t<y>\t<w>\t<h>` with two decimals. `key` is one
/// of the [`PORT_RECT_SELECTORS`] keys. The `origin` row supplies the
/// cards-layer origin and every other box is shifted into that space, so the
/// result is directly comparable with the computed card grid.
///
/// Returns `None` when there is no usable `origin` row: without it the absolute
/// viewport coordinates cannot be converted to cards-layer coordinates, and a
/// half-converted geometry would be worse than the fallback.
pub fn geometry_from_tsv(tsv: &str) -> Option<BlocksGeometry> {
    let mut origin: Option<PortBox> = None;
    let mut raw: BTreeMap<String, (Option<PortBox>, Option<PortBox>, Option<PortBox>, Vec<PortBox>)> =
        BTreeMap::new();
    for line in tsv.lines() {
        let mut it = line.split('\t');
        if it.next() != Some("@rect") {
            continue;
        }
        let (Some(key), Some(id), Some(x), Some(y), Some(w), Some(h)) =
            (it.next(), it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        let (Ok(x), Ok(y), Ok(w), Ok(h)) = (
            x.trim().parse::<f32>(),
            y.trim().parse::<f32>(),
            w.trim().parse::<f32>(),
            h.trim().parse::<f32>(),
        ) else {
            continue;
        };
        let rect = PortBox { x, y, w, h };
        match key {
            "origin" => origin = Some(rect),
            // Every card-scoped row must name its card; an anonymous card row
            // cannot be attributed and is dropped rather than guessed.
            _ if id.is_empty() => {}
            "card" | "ports" | "out" | "in" => {
                let entry = raw.entry(id.to_string()).or_default();
                match key {
                    "card" => entry.0 = Some(rect),
                    "ports" => entry.1 = Some(rect),
                    "out" => entry.2 = Some(rect),
                    _ => entry.3.push(rect),
                }
            }
            _ => {}
        }
    }
    let origin = origin?;
    let cards = raw
        .into_iter()
        .map(|(id, (card, ports, out_dot, in_dots))| {
            let shift = |b: PortBox| b.shifted(origin.x, origin.y);
            (
                id,
                MeasuredCard {
                    card: card.map(shift),
                    ports: ports.map(shift),
                    out_dot: out_dot.map(shift),
                    in_dots: in_dots.into_iter().map(shift).collect(),
                },
            )
        })
        .collect();
    Some(BlocksGeometry {
        origin: PortBox { x: 0.0, y: 0.0, w: origin.w, h: origin.h },
        cards,
    })
}

/// The card's painted box: the measured rectangle when the shell reported one,
/// else the computed grid slot `(x, y, CARD_W, CARD_H)`.
pub fn card_box(c: &BlockCard) -> PortBox {
    c.measured
        .as_ref()
        .and_then(|m| m.card)
        .unwrap_or(PortBox { x: c.x, y: c.y, w: CARD_W, h: CARD_H })
}

/// The out-port dot box: measured when available, else the fallback placement on
/// the card's right edge in the assumed ports row.
pub fn port_box_out(c: &BlockCard) -> PortBox {
    if let Some(b) = c.measured.as_ref().and_then(|m| m.out_dot) {
        return b;
    }
    PortBox {
        x: c.x + CARD_W - 2.0 * PORT_DOT_HALF,
        y: c.y + CARD_H * PORT_ROW_FRAC - PORT_DOT_HALF,
        w: 2.0 * PORT_DOT_HALF,
        h: 2.0 * PORT_DOT_HALF,
    }
}

/// The in-port dot box for `slot`: measured when the shell reported that slot,
/// else the fallback left-edge placement staggered by the `gap: 3px` row pitch.
pub fn port_box_in(c: &BlockCard, slot: usize) -> PortBox {
    if let Some(b) = c.measured.as_ref().and_then(|m| m.in_dots.get(slot)) {
        return *b;
    }
    let step = 2.0 * PORT_DOT_HALF + 3.0;
    PortBox {
        x: c.x,
        y: c.y + CARD_H * PORT_ROW_FRAC - PORT_DOT_HALF + slot as f32 * step,
        w: 2.0 * PORT_DOT_HALF,
        h: 2.0 * PORT_DOT_HALF,
    }
}

/// The cubic's two control points, verbatim from `scratch.js:399-404`.
pub fn cubic_controls(a: (f32, f32), b: (f32, f32), nest: i64) -> ((f32, f32), (f32, f32)) {
    let dy = nest as f32 * 4.0;
    let dx = 30.0_f32.max((b.0 - a.0).abs() * 0.42);
    ((a.0 + dx, a.1 + dy), (b.0 - dx, b.1 + dy))
}

/// Port-anchored cubic path: v2's `drawEdges` builds exactly this from the two
/// painted port centres, so both endpoints lie on a port box by construction.
pub fn port_anchored_path(src: &BlockCard, dst: &BlockCard, nest: i64) -> String {
    port_path(port_box_out(src).center(), port_box_in(dst, 0).center(), nest)
}

/// The marker tip is the target port centre; the arrow points along the last
/// cubic segment (control point 2 → tip), which is what
/// `orient="auto-start-reverse"` rotates the triangle to.
pub fn arrow_direction(a: (f32, f32), b: (f32, f32), nest: i64) -> (f32, f32) {
    let (_, c2) = cubic_controls(a, b, nest);
    let tip = (b.0, b.1 + nest as f32 * 4.0);
    (tip.0 - c2.0, tip.1 - c2.1)
}

/// Cubic path between a source's out-port (right edge) and a target's in-port
/// (left edge), verbatim from `scratch.js:399-408`.
fn port_path(a: (f32, f32), b: (f32, f32), nest: i64) -> String {
    let dy = nest as f32 * 4.0;
    let dx = 30.0_f32.max((b.0 - a.0).abs() * 0.42);
    format!(
        "M{:.1},{:.1} C{:.1},{:.1} {:.1},{:.1} {:.1},{:.1}",
        a.0,
        a.1 + dy,
        a.0 + dx,
        a.1 + dy,
        b.0 - dx,
        b.1 + dy,
        b.0,
        b.1 + dy
    )
}

/// `EDGE_COLOR` — `scratch.js:36-39`.
pub fn edge_color(kind: &str) -> &'static str {
    match kind {
        "calls" => "#86aaf9",
        "uses" => "#42c3d0",
        "assigns" => "#ffed2e",
        "constructs" => "#fa903e",
        "inherits" => "#d272ff",
        "contains" => "#6b7897",
        _ => "#6b7897",
    }
}

/// The whole Blocks model for `anchor`, or `None` when the shipped empty stage
/// must stay untouched (no anchor, or an anchor the graph does not hold).
///
/// This is the fallback-only entry point; [`blocks_model_measured`] is what the
/// host calls once the shell has reported geometry.
pub fn blocks_model(graph: &Graph, anchor: Option<&str>) -> Option<BlocksModel> {
    blocks_model_measured(graph, anchor, None)
}

/// The Blocks model anchored to `geometry` when the shell measured this frame.
///
/// `geometry` is [`BlocksGeometry`] from the previous frame's `@rect` rows (or an
/// explicit value in tests). Every card the shell measured takes its painted
/// `.block-card` rectangle, and [`port_box_out`]/[`port_box_in`] then return the
/// measured `.port-dot` boxes. Cards the shell did not report — and the whole
/// model when `geometry` is `None` — keep the stylesheet-constant fallback, so
/// this never invents a measurement.
pub fn blocks_model_measured(
    graph: &Graph,
    anchor: Option<&str>,
    geometry: Option<&BlocksGeometry>,
) -> Option<BlocksModel> {
    let anchor = anchor.filter(|a| !a.is_empty())?;
    let scope = scope_of(graph, anchor);
    let mut cards = cards_for(graph, &scope);
    if cards.is_empty() {
        return Some(BlocksModel {
            title: "Blocks".to_string(),
            meta: String::new(),
            cards,
            edges: Vec::new(),
            empty: format!("No declarations for {scope}."),
        });
    }
    layout_cards(&mut cards, ASSUMED_COLS);
    // Measured geometry supersedes the computed grid slot per card. Card
    // placement is CSS grid, not derived from these numbers, so using the
    // previous frame's boxes converges immediately (see `MEASURED`).
    if let Some(g) = geometry {
        for c in &mut cards {
            if let Some(m) = g.card(&c.id) {
                if let Some(b) = m.card {
                    c.x = b.x;
                    c.y = b.y;
                }
                c.measured = Some(m.clone());
            }
        }
    }
    // Reference edges among rendered cards only; edges touching a card that was
    // not rendered are dropped (`scratch.js:355-358`).
    let mut edges = Vec::new();
    let mut seen: Vec<(String, String, i64)> = Vec::new();
    for e in &graph.edges {
        let Some(src) = graph.nodes.get(e.from) else { continue };
        let Some(dst) = graph.nodes.get(e.to) else { continue };
        let (Some(sc), Some(dc)) = (
            cards.iter().find(|c| c.id == src.id),
            cards.iter().find(|c| c.id == dst.id),
        ) else {
            continue;
        };
        let nest = seen
            .iter_mut()
            .find(|(s, t, _)| s == &sc.id && t == &dc.id)
            .map(|(_, _, n)| {
                let cur = *n;
                *n += 1;
                cur
            })
            .unwrap_or_else(|| {
                seen.push((sc.id.clone(), dc.id.clone(), 1));
                0
            });
        // v2 measures the painted `.port-out`/`.port-in` boxes; this host uses
        // the shell's measured boxes when reported and the stylesheet constants
        // otherwise. Either way the endpoints are real port-box centres.
        let a = port_box_out(sc).center();
        let b = port_box_in(dc, 0).center();
        edges.push(BlockEdge {
            source: sc.id.clone(),
            target: dc.id.clone(),
            kind: e.kind.clone(),
            count: e.count,
            path_d: port_path(a, b, nest),
            color: edge_color(&e.kind).to_string(),
            ax: a.0,
            ay: a.1,
            bx: b.0,
            by: b.1,
        });
    }
    let mut title_tail = scope.split('.').next_back().unwrap_or(&scope).to_string();
    if title_tail.is_empty() {
        title_tail = scope.clone();
    }
    let mut meta = format!("{} blocks · {} edges", cards.len(), edges.len());
    let more = graph
        .nodes
        .iter()
        .filter(|n| n.owner == scope)
        .count()
        .saturating_sub(cards.len());
    if more > 0 {
        meta.push_str(&format!(" · +{more} more"));
    }
    Some(BlocksModel {
        title: format!("Blocks · {title_tail}"),
        meta,
        cards,
        edges,
        empty: String::new(),
    })
}

/// `scratch.js:85-95` — method/constructor parameter list from the id's
/// trailing `(…)`, depth-aware, capped at 8. `Vec::new()` when there is none.
pub fn split_params(id: &str) -> Vec<String> {
    let Some(open) = id.rfind('(') else { return Vec::new() };
    if !id.trim_end().ends_with(')') {
        return Vec::new();
    }
    let inner = &id[open + 1..id.rfind(')').unwrap_or(id.len())];
    if inner.trim().is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    for c in inner.chars() {
        match c {
            '<' | '(' | '[' | '{' => depth += 1,
            '>' | ')' | ']' | '}' => depth = (depth - 1).max(0),
            ',' if depth == 0 => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out.truncate(8);
    out
}

/// `scratch.js:105-110` — `basename`.
pub fn basename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_string()
}

/// `scratch.js:112-116` — `shortType`: the last dotted segment, angle brackets kept.
pub fn short_type(t: &str) -> String {
    let t = t.trim();
    if t.is_empty() {
        return "·".to_string();
    }
    let (core, generic) = match t.find('<') {
        Some(i) => (&t[..i], &t[i..]),
        None => (t, ""),
    };
    let tail = core.rsplit('.').next().unwrap_or(core);
    format!("{tail}{generic}")
}

/// The `ui.blocks` fact (projected by `nzk_ui.rs ui_state_json`). `None` keeps
/// the shipped document untouched; the JSON shape is what nzk_ui's
/// `BlocksFacts` deserializes.
pub fn blocks_facts(graph: &Graph, anchor: Option<&str>, selected: Option<&str>) -> Option<serde_json::Value> {
    let geometry = measured_geometry();
    let model = blocks_model_measured(graph, anchor, geometry.as_ref())?;
    let cards: Vec<serde_json::Value> = model
        .cards
        .iter()
        .map(|c| {
            let Rgba(r, g, b, _) = kind_color(crate::graphviz::UiTheme::NzkDarkPlus, &c.kind);
            serde_json::json!({
                "id": c.id,
                "index": c.index,
                "kind": c.kind,
                "name": c.name,
                "full": c.full,
                "file": c.file,
                "line": c.line,
                "params": c.params,
                "out_type": c.out_type(),
                "degree": c.degree,
                "x": c.x,
                "y": c.y,
                "selected": selected == Some(c.id.as_str()),
                "kind_color": format!("#{r:02x}{g:02x}{b:02x}"),
            })
        })
        .collect();
    let edges: Vec<serde_json::Value> = model
        .edges
        .iter()
        .map(|e| {
            serde_json::json!({
                "source": e.source, "target": e.target, "kind": e.kind,
                "count": e.count, "d": e.path_d, "color": e.color,
                "ax": e.ax, "ay": e.ay, "bx": e.bx, "by": e.by,
            })
        })
        .collect();
    Some(serde_json::json!({
        "title": model.title,
        "meta": model.meta,
        "empty": model.empty,
        "cards": cards,
        "edges": edges,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, Node};
    use std::path::PathBuf;

    fn graph() -> Graph {
        let mk = |id: &str, label: &str, kind: &str, owner: &str, file: &str, line: i64| Node {
            id: id.into(),
            label: label.into(),
            full: id.into(),
            kind: kind.into(),
            owner: owner.into(),
            file: file.into(),
            line,
            degree: 1,
            ..Default::default()
        };
        Graph {
            repo: PathBuf::from("/tmp/does-not-exist"),
            nodes: vec![
                mk("N:Demo", "Demo", "namespace", "", "", 0),
                mk("T:Demo.Widget", "Widget", "class", "N:Demo", "src/widget.cs", 1),
                mk(
                    "M:Demo.Widget.Run(System.String,System.Int32)",
                    "Run",
                    "method",
                    "T:Demo.Widget",
                    "src/widget.cs",
                    5,
                ),
                mk("P:Demo.Widget.Count", "Count", "property", "T:Demo.Widget", "src/widget.cs", 9),
            ],
            edges: vec![
                Edge { from: 1, to: 2, kind: "contains".into(), count: 1 },
                Edge { from: 2, to: 3, kind: "uses".into(), count: 2 },
            ],
            state: serde_json::json!({"indexed": true, "fileCount": 1}),
            skipped: Vec::new(),
            projects: Vec::new(),
        }
    }

    #[test]
    fn a_member_anchor_scopes_to_its_declaring_type_and_renders_its_members() {
        let g = graph();
        let scope = scope_of(&g, "M:Demo.Widget.Run(System.String,System.Int32)");
        assert_eq!(scope, "T:Demo.Widget");
        let m = blocks_model(&g, Some("M:Demo.Widget.Run(System.String,System.Int32)")).unwrap();
        assert_eq!(m.title, "Blocks · Widget");
        // Type + its two members, graph order (declaration order).
        let ids: Vec<&str> = m.cards.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "T:Demo.Widget",
                "M:Demo.Widget.Run(System.String,System.Int32)",
                "P:Demo.Widget.Count"
            ]
        );
        assert_eq!(m.meta, "3 blocks · 2 edges");
    }

    #[test]
    fn ports_come_from_the_real_id_parameters_and_the_kind_fallback() {
        let g = graph();
        let run = split_params("M:Demo.Widget.Run(System.String,System.Int32)");
        assert_eq!(run, vec!["System.String", "System.Int32"]);
        assert_eq!(short_type(&run[0]), "String");
        assert!(split_params("M:No.Parens").is_empty());
        let m = blocks_model(&g, Some("T:Demo.Widget")).unwrap();
        let run_card = m.cards.iter().find(|c| c.name == "Run").unwrap();
        assert_eq!(run_card.params.len(), 2);
        let cls = m.cards.iter().find(|c| c.name == "Widget").unwrap();
        assert_eq!(cls.out_type(), "class");
        let prop = m.cards.iter().find(|c| c.name == "Count").unwrap();
        assert_eq!(prop.out_type(), "value");
    }

    #[test]
    fn edges_are_drawn_only_between_rendered_cards_with_deterministic_paths() {
        let g = graph();
        let m = blocks_model(&g, Some("T:Demo.Widget")).unwrap();
        assert_eq!(m.edges.len(), 2);
        assert_eq!(m.edges[0].kind, "contains");
        assert_eq!(m.edges[1].kind, "uses");
        assert_eq!(m.edges[1].count, 2);
        // Deterministic: the same inputs produce the same path data.
        let again = blocks_model(&g, Some("T:Demo.Widget")).unwrap();
        assert_eq!(m.edges, again.edges);
        assert!(m.edges[0].path_d.starts_with("M"));
        assert_eq!(m.edges[0].color, "#6b7897");
        assert_eq!(m.edges[1].color, "#42c3d0");
    }

    #[test]
    fn no_anchor_or_unknown_anchor_is_an_honest_empty_not_a_guess() {
        let g = graph();
        assert!(blocks_model(&g, None).is_none());
        assert!(blocks_model(&g, Some("")).is_none());
        let m = blocks_model(&g, Some("T:Missing")).unwrap();
        assert!(m.cards.is_empty());
        assert_eq!(m.empty, "No declarations for T:Missing.");
        assert!(blocks_facts(&g, None, None).is_none());
    }

    #[test]
    fn facts_carry_the_selected_card_and_the_model_counts() {
        let g = graph();
        let f = blocks_facts(&g, Some("T:Demo.Widget"), Some("T:Demo.Widget")).unwrap();
        assert_eq!(f["meta"], "3 blocks · 2 edges");
        let cards = f["cards"].as_array().unwrap();
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0]["selected"], true);
        assert_eq!(cards[1]["selected"], false);
        assert_eq!(f["edges"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn edge_endpoints_land_on_the_painted_port_boxes() {
        let g = graph();
        let m = blocks_model(&g, Some("T:Demo.Widget")).unwrap();
        for e in &m.edges {
            let src = m.cards.iter().find(|c| c.id == e.source).unwrap();
            let dst = m.cards.iter().find(|c| c.id == e.target).unwrap();
            let out = port_box_out(src);
            let inp = port_box_in(dst, 0);
            // The recorded endpoints ARE the port box centres, so the cubic
            // starts/ends inside a painted port box, never on an invented point.
            assert_eq!((e.ax, e.ay), out.center());
            assert_eq!((e.bx, e.by), inp.center());
            let inside = |b: PortBox, (x, y): (f32, f32)| {
                x >= b.x && x <= b.x + b.w && y >= b.y && y <= b.y + b.h
            };
            assert!(inside(out, (e.ax, e.ay)));
            assert!(inside(inp, (e.bx, e.by)));
            assert!(e.path_d.starts_with(&format!("M{:.1},{:.1} C", e.ax, e.ay)));
            assert!(e.path_d.ends_with(&format!("{:.1},{:.1}", e.bx, e.by)));
        }
    }

    #[test]
    fn arrow_points_along_the_last_segment_into_the_target_port() {
        let g = graph();
        let m = blocks_model(&g, Some("T:Demo.Widget")).unwrap();
        for e in &m.edges {
            let a = (e.ax, e.ay);
            let b = (e.bx, e.by);
            let (dx, _dy) = arrow_direction(a, b, 0);
            // Target is in the next grid column: the marker tip's tangent has a
            // positive x component (orient=auto-start-reverse only turns it).
            assert!(dx > 0.0, "arrow must point toward the target port: dx={dx}");
        }
    }

    #[test]
    fn layout_is_a_deterministic_grid() {
        let mut cards: Vec<BlockCard> = (0..4)
            .map(|i| BlockCard {
                id: format!("id{i}"),
                index: i,
                kind: "field".into(),
                name: format!("n{i}"),
                full: String::new(),
                file: String::new(),
                line: 0,
                ret: String::new(),
                params: Vec::new(),
                degree: 0,
                x: 0.0,
                y: 0.0,
                measured: None,
            })
            .collect();
        layout_cards(&mut cards, 3);
        assert_eq!((cards[0].x, cards[0].y), (0.0, 0.0));
        assert_eq!(cards[1].x, CARD_W + GAP_X);
        assert_eq!(cards[3].x, 0.0);
        assert!(cards[3].y > 0.0);
    }

    fn measured_graph_geometry() -> BlocksGeometry {
        // Absolute shell coordinates; the parser normalises by the origin. The
        // cards are placed by the CSS grid at x=100 and x=362, and the dots sit
        // where Taffy says they do (note the out dot is 228px into the card,
        // NOT the 224px the fallback constant would produce).
        // Expected geometry in cards-layer coordinates, i.e. already normalised
        // by the origin (100, 200) the parser subtracts.
        let mut cards = BTreeMap::new();
        cards.insert(
            "T:Demo.Widget".to_string(),
            MeasuredCard {
                card: Some(PortBox { x: 0.0, y: 0.0, w: 232.0, h: 167.0 }),
                ports: Some(PortBox { x: 0.0, y: 115.0, w: 232.0, h: 12.0 }),
                out_dot: Some(PortBox { x: 224.0, y: 117.0, w: 8.0, h: 8.0 }),
                in_dots: Vec::new(),
            },
        );
        cards.insert(
            "M:Demo.Widget.Run(System.String,System.Int32)".to_string(),
            MeasuredCard {
                card: Some(PortBox { x: 262.0, y: 0.0, w: 232.0, h: 167.0 }),
                ports: Some(PortBox { x: 262.0, y: 115.0, w: 232.0, h: 12.0 }),
                out_dot: Some(PortBox { x: 486.0, y: 117.0, w: 8.0, h: 8.0 }),
                in_dots: vec![PortBox { x: 262.0, y: 117.0, w: 8.0, h: 8.0 }],
            },
        );
        let tsv = "\
@rect\torigin\t\t100.00\t200.00\t800.00\t600.00
@rect\tcard\tT:Demo.Widget\t100.00\t200.00\t232.00\t167.00
@rect\tports\tT:Demo.Widget\t100.00\t315.00\t232.00\t12.00
@rect\tout\tT:Demo.Widget\t324.00\t317.00\t8.00\t8.00
@rect\tcard\tM:Demo.Widget.Run(System.String,System.Int32)\t362.00\t200.00\t232.00\t167.00
@rect\tports\tM:Demo.Widget.Run(System.String,System.Int32)\t362.00\t315.00\t232.00\t12.00
@rect\tout\tM:Demo.Widget.Run(System.String,System.Int32)\t586.00\t317.00\t8.00\t8.00
@rect\tin\tM:Demo.Widget.Run(System.String,System.Int32)\t362.00\t317.00\t8.00\t8.00
";
        let parsed = geometry_from_tsv(tsv).expect("origin present");
        assert_eq!(parsed, BlocksGeometry { origin: PortBox { x: 0.0, y: 0.0, w: 800.0, h: 600.0 }, cards });
        parsed
    }

    #[test]
    fn geometry_parser_normalises_by_the_cards_layer_origin() {
        let g = measured_graph_geometry();
        assert_eq!(g.card("T:Demo.Widget").unwrap().card.unwrap().x, 0.0);
        assert_eq!(
            g.card("M:Demo.Widget.Run(System.String,System.Int32)").unwrap().card.unwrap().x,
            262.0
        );
        // Without an origin the absolute coordinates cannot be converted, so the
        // parser refuses rather than returning a half-shifted geometry.
        assert!(geometry_from_tsv("@rect\tcard\tT:A\t1\t2\t3\t4\n").is_none());
        assert!(geometry_from_tsv("").is_none());
        // Malformed / anonymous rows are dropped, never guessed.
        assert!(geometry_from_tsv(
            "@rect\torigin\t\t0\t0\t10\t10\n@rect\tcard\tT:A\t1\t2\n@rect\tcard\t\t1\t2\t3\t4\n"
        )
        .unwrap()
        .cards
        .is_empty());
    }

    #[test]
    fn measured_ports_anchor_edges_to_the_laid_out_dot_centres() {
        let g = graph();
        let geom = measured_graph_geometry();
        let m = blocks_model_measured(&g, Some("T:Demo.Widget"), Some(&geom)).unwrap();
        let contains = m.edges.iter().find(|e| e.kind == "contains").unwrap();
        // T's measured out dot rel centre: (324-100+4, 317-200+4) = (228, 121).
        assert_eq!((contains.ax, contains.ay), (228.0, 121.0));
        // M's measured in dot rel centre: (362-100+4, 317-200+4) = (266, 121).
        assert_eq!((contains.bx, contains.by), (266.0, 121.0));
        assert!(contains.path_d.starts_with("M228.0,121.0 C"));
        // The measured card rectangle also replaces the assumed grid slot.
        let t = m.cards.iter().find(|c| c.id == "T:Demo.Widget").unwrap();
        assert_eq!((t.x, t.y), (0.0, 0.0));
        let run = m
            .cards
            .iter()
            .find(|c| c.id == "M:Demo.Widget.Run(System.String,System.Int32)")
            .unwrap();
        assert_eq!((run.x, run.y), (262.0, 0.0));
    }

    #[test]
    fn unmeasured_cards_keep_the_stylesheet_constant_fallback() {
        let g = graph();
        let geom = measured_graph_geometry();
        let m = blocks_model_measured(&g, Some("T:Demo.Widget"), Some(&geom)).unwrap();
        // `P:Demo.Widget.Count` has no @rect rows, so its fallback box is used.
        let p = m.cards.iter().find(|c| c.id == "P:Demo.Widget.Count").unwrap();
        assert!(p.measured.is_none());
        assert_eq!(port_box_out(p), PortBox {
            x: p.x + CARD_W - 2.0 * PORT_DOT_HALF,
            y: p.y + CARD_H * PORT_ROW_FRAC - PORT_DOT_HALF,
            w: 2.0 * PORT_DOT_HALF,
            h: 2.0 * PORT_DOT_HALF,
        });
        // No geometry at all is exactly the pre-channel model.
        assert_eq!(
            blocks_model_measured(&g, Some("T:Demo.Widget"), None).unwrap().edges,
            blocks_model(&g, Some("T:Demo.Widget")).unwrap().edges
        );
    }

    #[test]
    fn facts_use_the_cached_measured_geometry_and_clear_back_to_the_fallback() {
        let g = graph();
        set_measured_geometry(Some(measured_graph_geometry()));
        let f = blocks_facts(&g, Some("T:Demo.Widget"), None).unwrap();
        set_measured_geometry(None);
        let edges = f["edges"].as_array().unwrap();
        let contains = edges.iter().find(|e| e["kind"] == "contains").unwrap();
        assert_eq!(contains["ax"], 228.0);
        assert_eq!(contains["bx"], 266.0);
        // Cleared cache → fallback endpoints, not the stale measured ones. The
        // x centre happens to coincide (224+4 = 228); the y centre does not
        // (fallback PORT_ROW_FRAC puts it at 116.928, measured at 121).
        let f2 = blocks_facts(&g, Some("T:Demo.Widget"), None).unwrap();
        let edges2 = f2["edges"].as_array().unwrap();
        let contains2 = edges2.iter().find(|e| e["kind"] == "contains").unwrap();
        assert_ne!(contains2["ay"], 121.0);
    }
}
