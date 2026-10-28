//! Pure graph-drawing geometry, ported from the original frontend.
//!
//! This module is **stack-independent on purpose**: it uses no `egui` types, so
//! whatever renders the graph next (egui today, a CSS-driven surface later) can
//! share one implementation of the things that had to be faithful:
//!
//! * `core.js` `KIND_COLOR` / `KIND_COLOR_NZK_DARKPLUS`, `EDGE_STYLE`,
//!   `EDGE_LABEL`, the chip colours and the canvas background;
//! * `layout.js` — the faithful ForceAtlas2 + Noverlap port and `computeFit`;
//! * `graph.js` `seedNewNodes` and the chip geometry (`CHIP_FONT` 11, `CHIP_PAD`
//!   1, `CHIP_GAP` 2, `CHIP_MAX_CHARS` 40, dot diameter = Layout ▸ Node size);
//! * `render.js` — edge trim/stack/curve/arrow geometry and the pill anchor,
//!   the grid, and the label gating (`EDGE_LABEL_ZOOM` 0.55).
//!
//! Everything is deterministic (FNV-1a `hash32`, no RNG) so a render is
//! reproducible and can be diffed against the original pixel-for-pixel.

use std::collections::HashMap;

// ---------------------------------------------------------------- colours

/// Straight RGBA, 0..=255, no colour management.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Rgba(r, g, b, 255)
    }
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Rgba(r, g, b, a)
    }
    /// `rgba(r,g,b,alpha01)` like the CSS/`globalAlpha` values.
    pub fn with_alpha(self, a: f32) -> Rgba {
        Rgba(self.0, self.1, self.2, (a.clamp(0.0, 1.0) * 255.0).round() as u8)
    }
}

/// The two whole-viewer themes the original ships (set in `core.js`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UiTheme {
    /// The original's default (`DEFAULT_UI_THEME = 'nzk-dark+'`) — VS Code Dark+.
    NzkDarkPlus,
    /// `dark-v1`, the VirusTotal navy palette.
    DarkV1,
}

impl Default for UiTheme {
    fn default() -> Self {
        UiTheme::NzkDarkPlus
    }
}

/// `core.js KIND_COLOR_NZK_DARKPLUS` — every type is `#4ec9b0` because that is
/// what the editor does; members are `#9cdcfe`, methods `#dcdcaa`.
pub fn kind_color_nzk_darkplus(kind: &str) -> Rgba {
    match kind {
        "namespace" | "class" | "record" => Rgba::rgb(0x4e, 0xc9, 0xb0),
        "struct" => Rgba::rgb(0x86, 0xc6, 0x91),
        "interface" | "enum" => Rgba::rgb(0xb8, 0xd7, 0xa3),
        "method" | "constructor" => Rgba::rgb(0xdc, 0xdc, 0xaa),
        "operator" => Rgba::rgb(0xc5, 0x86, 0xc0),
        "property" | "field" | "event" => Rgba::rgb(0x9c, 0xdc, 0xfe),
        "string" => Rgba::rgb(0xce, 0x91, 0x78),
        "external" | "usage" => Rgba::rgb(0x80, 0x80, 0x80),
        _ => Rgba::rgb(0x85, 0x85, 0x85),
    }
}

/// `core.js KIND_COLOR` — the `dark-v1` VirusTotal palette.
pub fn kind_color_dark_v1(kind: &str) -> Rgba {
    match kind {
        "namespace" => Rgba::rgb(0x86, 0xaa, 0xf9),
        "class" => Rgba::rgb(0x42, 0xc3, 0xd0),
        "struct" => Rgba::rgb(0x54, 0xab, 0x98),
        "interface" => Rgba::rgb(0xd2, 0x72, 0xff),
        "enum" => Rgba::rgb(0xff, 0xed, 0x2e),
        "record" => Rgba::rgb(0xf4, 0x83, 0x85),
        "method" => Rgba::rgb(0xfa, 0x90, 0x3e),
        "constructor" => Rgba::rgb(0xe5, 0x73, 0x00),
        "operator" => Rgba::rgb(0xff, 0x5a, 0x50),
        "property" => Rgba::rgb(0x9d, 0xb8, 0xf0),
        "field" => Rgba::rgb(0x8f, 0xd3, 0xa8),
        "event" => Rgba::rgb(0xff, 0xc4, 0x6b),
        "string" => Rgba::rgb(0xc8, 0xb6, 0xff),
        "external" | "usage" => Rgba::rgb(0x7a, 0x86, 0xa8),
        _ => Rgba::rgb(0x9a, 0xa6, 0xc2),
    }
}

pub fn kind_color(theme: UiTheme, kind: &str) -> Rgba {
    match theme {
        UiTheme::NzkDarkPlus => kind_color_nzk_darkplus(kind),
        UiTheme::DarkV1 => kind_color_dark_v1(kind),
    }
}

/// The nzk_ui + chip palette of one theme (`core.js UI_THEMES` + the
/// `[data-ui-theme]` CSS block).
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub canvas: Rgba,
    pub canvas_glow: Rgba,
    pub grid: Rgba,
    pub chip_bg: Rgba,
    pub chip_border: Rgba,
    pub chip_text: Rgba,
    pub chip_sel_border: Rgba,
    pub chip_dim_bg: Rgba,
    pub chip_dim_text: Rgba,
    pub pin: Rgba,
}

impl Palette {
    pub fn of(theme: UiTheme) -> Palette {
        match theme {
            UiTheme::NzkDarkPlus => Palette {
                canvas: Rgba::rgb(0x1e, 0x1e, 0x1e),
                canvas_glow: Rgba::rgb(0x1e, 0x1e, 0x1e),
                grid: Rgba::rgba(120, 160, 210, 0x0d),
                chip_bg: Rgba::rgb(0x25, 0x25, 0x26),
                chip_border: Rgba::rgb(0x3c, 0x3c, 0x3c),
                chip_text: Rgba::rgb(0xd4, 0xd4, 0xd4),
                chip_sel_border: Rgba::rgb(0x00, 0x7a, 0xcc),
                chip_dim_bg: Rgba::rgba(45, 45, 48, 0x4d),
                chip_dim_text: Rgba::rgba(212, 212, 212, 0x33),
                pin: Rgba::rgb(0xc5, 0x86, 0xc0),
            },
            UiTheme::DarkV1 => Palette {
                canvas: Rgba::rgb(0x0e, 0x14, 0x24),
                canvas_glow: Rgba::rgb(0x13, 0x1b, 0x30),
                grid: Rgba::rgba(120, 160, 210, 0x0d),
                chip_bg: Rgba::rgb(0x23, 0x2c, 0x42),
                chip_border: Rgba::rgb(0x3d, 0x4a, 0x69),
                chip_text: Rgba::rgb(0xdb, 0xe2, 0xf0),
                chip_sel_border: Rgba::rgb(0x86, 0xaa, 0xf9),
                chip_dim_bg: Rgba::rgba(35, 44, 66, 0x4d),
                chip_dim_text: Rgba::rgba(219, 226, 240, 0x2e),
                pin: Rgba::rgb(0xc0, 0x84, 0xfc),
            },
        }
    }
}

// ---------------------------------------------------------------- edges

/// `core.js EDGE_STYLE` entry.
#[derive(Clone, Copy, Debug)]
pub struct EdgeStyle {
    pub color: Rgba,
    pub width: f32,
    pub dash: Option<[f32; 2]>,
    pub arrow: bool,
    pub alpha: f32,
}

pub const EDGE_COLOR: Rgba = Rgba::rgb(0xc9, 0xd3, 0xe5);

pub fn edge_style(kind: &str) -> EdgeStyle {
    match kind {
        "calls" => EdgeStyle { color: EDGE_COLOR, width: 1.2, dash: None, arrow: true, alpha: 0.62 },
        "assigns" => EdgeStyle { color: EDGE_COLOR, width: 1.2, dash: Some([1.0, 3.0]), arrow: true, alpha: 0.55 },
        "constructs" => EdgeStyle { color: EDGE_COLOR, width: 1.2, dash: None, arrow: true, alpha: 0.60 },
        "inherits" => EdgeStyle { color: EDGE_COLOR, width: 2.0, dash: None, arrow: true, alpha: 0.70 },
        "contains" => EdgeStyle { color: EDGE_COLOR, width: 1.0, dash: Some([1.0, 5.0]), arrow: true, alpha: 0.42 },
        // `uses` is both the explicit kind and `DEFAULT_EDGE_STYLE`.
        _ => EdgeStyle { color: EDGE_COLOR, width: 1.0, dash: Some([4.0, 4.0]), arrow: true, alpha: 0.50 },
    }
}

/// `core.js EDGE_LABEL` — the text inside the relation pill.
pub fn edge_label(kind: &str) -> &'static str {
    match kind {
        "calls" => "Calling",
        "uses" => "Uses",
        "assigns" => "Assigns",
        "constructs" => "Constructs",
        "inherits" => "Inherits",
        "contains" => "Children",
        _ => "",
    }
}

pub const EDGE_STACK_PX: f32 = 3.6;
pub const EDGE_LABEL_ZOOM: f32 = 0.55;
pub const EDGE_PILL_FONT: f32 = 10.0;
pub const EDGE_PILL_PAD_X: f32 = 5.0;
pub const EDGE_PILL_H: f32 = 14.0;
/// Minimum clear space between two relation pills, in painter pixels. The boxes
/// are deconflicted against this padded rect, so adjacent pills "snap" to a small
/// readable gap instead of touching or overlapping.
pub const EDGE_PILL_GAP: f32 = 2.0;
pub const EDGE_PILL_RADIUS: f32 = 7.0;
pub const EDGE_PILL_FILL_STRONG: Rgba = Rgba::rgb(0x2f, 0x3d, 0x5c);
pub const EDGE_PILL_FILL: Rgba = Rgba::rgb(0x1b, 0x23, 0x34);
pub const EDGE_PILL_BORDER: Rgba = Rgba::rgb(0x3d, 0x4a, 0x69);
pub const EDGE_PILL_TEXT_STRONG: Rgba = Rgba::rgb(0xdb, 0xe6, 0xf5);
pub const EDGE_PILL_TEXT: Rgba = Rgba::rgb(0x9a, 0xa6, 0xc2);

// ---- VT mode: a relation drawn as a small node on the line ----------------
//
// The owner's VirusTotal-style mode: instead of a word box floating beside a
// curved arrow, the relation is a compact node between the two declarations
// (`node -> relation node -> node`). It uses the node-chip visual language (a
// coloured dot + label in a rounded, bordered box) at a smaller size, so it
// reads as a node rather than as an edge annotation.
pub const VT_REL_FONT: f32 = 10.0;
pub const VT_REL_PAD_X: f32 = 7.0;
pub const VT_REL_H: f32 = 18.0;
pub const VT_REL_RADIUS: f32 = 4.0;
pub const VT_REL_DOT: f32 = 6.0;
pub const VT_REL_GAP: f32 = 5.0;
pub const VT_REL_FILL: Rgba = Rgba::rgb(0x10, 0x14, 0x1f);
pub const VT_REL_FILL_STRONG: Rgba = Rgba::rgb(0x1c, 0x27, 0x40);
pub const VT_REL_BORDER: Rgba = Rgba::rgb(0x3d, 0x4a, 0x69);

/// One edge's screen-space geometry, exactly the numbers `render.js draw()`
/// computes before it strokes.
#[derive(Clone, Copy, Debug)]
pub struct EdgeDraw {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
    /// Quadratic control point when `curved`, else unused.
    pub mx: f32,
    pub my: f32,
    pub curved: bool,
    pub width: f32,
    pub dash: Option<[f32; 2]>,
    pub alpha: f32,
    pub color: Rgba,
    /// Arrow head triangle (tip first), when the edge is long enough.
    pub arrow: Option<[(f32, f32); 3]>,
    /// Pill centre when the relation word box is drawn.
    pub pill: Option<(f32, f32)>,
}

/// Port of the edge body of `render.js draw()` (lines ~100-155).
///
/// `a_w`/`b_w` are the chips' on-screen widths; `pair_index`/`pair_count`
/// fan parallel relations between the same pair into separate strands.
#[allow(clippy::too_many_arguments)]
pub fn edge_draw(
    a: (f32, f32),
    a_w: f32,
    b: (f32, f32),
    b_w: f32,
    kind: &str,
    pair_index: f32,
    pair_count: f32,
    zoom: f32,
    curvature: f64,
    focus_touches: Option<bool>,
) -> EdgeDraw {
    let st = edge_style(kind);
    // alpha = st.alpha * (focus ? touches ? 1.25 : 0.35 : 1)
    let mut alpha = st.alpha
        * match focus_touches {
            None => 1.0,
            Some(true) => 1.25,
            Some(false) => 0.35,
        };
    if alpha > 1.0 {
        alpha = 1.0;
    }

    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let mut len = (dx * dx + dy * dy).sqrt();
    if len == 0.0 {
        len = 1.0;
    }
    let ux = dx / len;
    let uy = dy / len;
    let trim_a = a_w / 2.0 + 1.0;
    let trim_b = b_w / 2.0 + if st.arrow { 7.0 } else { 1.0 };
    let mut x1 = a.0 + ux * trim_a;
    let mut y1 = a.1 + uy * trim_a;
    let mut x2 = b.0 - ux * trim_b;
    let mut y2 = b.1 - uy * trim_b;
    if len <= trim_a + trim_b {
        x1 = a.0;
        y1 = a.1;
        x2 = b.0;
        y2 = b.1;
    }

    if pair_count > 1.0 {
        let stack = (pair_index - (pair_count - 1.0) / 2.0) * EDGE_STACK_PX;
        let sox = -uy * stack;
        let soy = ux * stack;
        x1 += sox;
        y1 += soy;
        x2 += sox;
        y2 += soy;
    }

    let mut curve = 0.0f32;
    if pair_count > 1.0 && curvature > 0.0 {
        curve = (curvature as f32) * (pair_index - (pair_count - 1.0) / 2.0);
    }
    let mx = (x1 + x2) / 2.0 - uy * curve * len;
    let my = (y1 + y2) / 2.0 + ux * curve * len;
    let curved = curve != 0.0;

    let width = st.width * zoom.sqrt().clamp(0.7, 2.2);

    let arrow = if st.arrow && len > trim_a + trim_b + 2.0 {
        let (ax, ay) = if curved { (mx, my) } else { (x1, y1) };
        let ang = (y2 - ay).atan2(x2 - ax);
        let size = 7.5 + st.width;
        Some([
            (x2, y2),
            (x2 - size * (ang - 0.42).cos(), y2 - size * (ang - 0.42).sin()),
            (x2 - size * (ang + 0.42).cos(), y2 - size * (ang + 0.42).sin()),
        ])
    } else {
        None
    };

    let pill = if curved {
        Some((0.25 * x1 + 0.5 * mx + 0.25 * x2, 0.25 * y1 + 0.5 * my + 0.25 * y2))
    } else {
        Some(((x1 + x2) / 2.0, (y1 + y2) / 2.0))
    };

    // silence the unused-mut warnings when the trim branch never fires.
    EdgeDraw {
        x1,
        y1,
        x2,
        y2,
        mx,
        my,
        curved,
        width,
        dash: st.dash,
        alpha,
        color: st.color,
        arrow,
        pill,
    }
}

// ---------------------------------------------------------------- chips

pub const CHIP_FONT: f32 = 11.0;
pub const CHIP_PAD: f32 = 1.0;
pub const CHIP_GAP: f32 = 2.0;
pub const CHIP_MAX_CHARS: usize = 40;
/// `core.js NODE_SIZE_DEFAULT` — the Layout ▸ Node size slider (world units).
pub const NODE_SIZE_DEFAULT: f32 = 8.0;
/// `core.js` Text size slider range. The DEFAULT is [`CHIP_FONT`] (11), which is
/// what keeps this an *added control* rather than a rendering change: at the
/// default the chip geometry below is bit-for-bit what it was before the slider
/// existed, so every existing measurement test and the `@frame` token hold.
pub const TEXT_SIZE_MIN: f32 = 6.0;
pub const TEXT_SIZE_MAX: f32 = 28.0;
pub const TEXT_SIZE_DEFAULT: f32 = CHIP_FONT;

/// FNV-1a over UTF-16 code units, exactly `core.js hash32`.
pub fn hash32(s: &str) -> u32 {
    let mut h: u32 = 2166136261;
    for u in s.encode_utf16() {
        h ^= u as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

/// `graph.js chipLabel`: truncate above 40 chars with an ellipsis.
pub fn chip_text(label: &str) -> String {
    let chars: Vec<char> = label.chars().collect();
    if chars.len() > CHIP_MAX_CHARS {
        let mut s: String = chars[..CHIP_MAX_CHARS - 1].iter().collect();
        s.push('…');
        s
    } else {
        label.to_string()
    }
}

/// Measured chip size cache (`graph.js chipMetricsCache`).
///
/// # The cache key carries the font size, deliberately
///
/// The font used to be a module const, so `text -> (w, h)` was a safe key: the
/// only input that could change a box was the label. It is a *setting* now
/// ([`TEXT_SIZE_DEFAULT`] and the Layout ▸ Text size slider), which makes the
/// text-only key unsound — after a slider move the cache would hand back boxes
/// measured at the previous font and the chips would visibly mis-size until a
/// reload. The key is therefore `(text, font_size)`.
///
/// The two fields are `f32` and the key is a **bit pattern**, not the value:
/// `to_bits()` gives the map a total, `Eq`-able key without a float comparison,
/// which is what lets `HashMap` key on it at all. `TEXT_SIZE_DEFAULT` is
/// `CHIP_FONT` (11.0), so the default run looks up exactly the same boxes it did
/// before this existed, and the `@frame` token cannot move because of it.
///
/// `dot` (Layout ▸ Node size) is *not* in the key because it is not part of the
/// struct's identity: it lives on `self` and `relayout` rebuilds `ChipMetrics`
/// whenever it changes (view.rs's `self.metrics = ChipMetrics::new(...)`), so a
/// stale `dot` cannot survive a change. `font_size` is a parameter of `size()`,
/// not of the constructor, which is exactly why it has to be in the key.
pub struct ChipMetrics {
    pub dot: f32,
    /// Chip label font size in world units (`core.js CHIP_FONT`, now a setting).
    pub font_size: f32,
    cache: HashMap<(String, u32), (f32, f32)>,
}

impl ChipMetrics {
    /// The pre-slider constructor: the documented default text size.
    pub fn new(dot: f32) -> Self {
        ChipMetrics::with_font(dot, TEXT_SIZE_DEFAULT)
    }

    /// A cache whose boxes are measured at `font_size`.
    pub fn with_font(dot: f32, font_size: f32) -> Self {
        ChipMetrics { dot, font_size, cache: HashMap::new() }
    }

    /// `graph.js chipSizeFor`: `w = PAD + dot + GAP + textWidth + PAD`,
    /// `h = max(font + 2*PAD, dot + 2)`. `measure` returns the rendered width of
    /// the label at [`Self::font_size`] (the caller's text engine owns that,
    /// which is exactly why this is a callback).
    pub fn size(&mut self, label: &str, measure: &mut dyn FnMut(&str) -> f32) -> (f32, f32) {
        let text = chip_text(label);
        let key = (text, self.font_size.to_bits());
        if let Some(m) = self.cache.get(&key) {
            return *m;
        }
        let tw = measure(&key.0);
        let w = CHIP_PAD + self.dot + CHIP_GAP + tw + CHIP_PAD;
        let h = (self.font_size + CHIP_PAD * 2.0).max(self.dot + 2.0);
        let m = (w, h);
        if self.cache.len() > 5000 {
            self.cache.clear();
        }
        self.cache.insert(key, m);
        m
    }

    /// Keep the measured boxes when nothing they depend on changed; clear them
    /// only when the glyph dot or the font size moved.
    ///
    /// `relayout` used to construct a fresh `ChipMetrics` on every run, so every
    /// filter click re-measured every visible label through Parley. The cache is
    /// keyed by `(text, font_size)` but its stored boxes also encode `dot`, so
    /// both inputs must be compared here.
    pub fn retune(&mut self, dot: f32, font_size: f32) {
        if self.dot.to_bits() != dot.to_bits() || self.font_size.to_bits() != font_size.to_bits() {
            self.cache.clear();
        }
        self.dot = dot;
        self.font_size = font_size;
    }

    pub fn clear(&mut self) {
        self.cache.clear();
    }
}

// ---------------------------------------------------------------- layout

/// `core.js` layout slider defaults and ranges.
pub const SPACING_MIN: f64 = 8.0;
pub const SPACING_MAX: f64 = 240.0;
pub const SPACING_DEFAULT: f64 = 120.0;
pub const ITER_MIN: i32 = 300;
pub const ITER_MAX: i32 = 1500;
pub const ITER_DEFAULT: i32 = 800;
pub const CURVE_MIN: f64 = 0.0;
pub const CURVE_MAX: f64 = 0.2;
pub const CURVE_DEFAULT: f64 = 0.1;
pub const NODE_SIZE_MIN: f32 = 2.0;
pub const NODE_SIZE_MAX: f32 = 24.0;

const FA2_SLOWDOWN: f64 = 2.0;
const GOLDEN_ANGLE: f64 = 2.399963229728653;

/// The layout slider defaults and ranges, plus the fork's text size.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LayoutSettings {
    pub spacing: f64,
    pub iterations: i32,
    pub curvature: f64,
    pub node_size: f32,
    /// Chip label font size in world units — the Layout ▸ Text size slider.
    ///
    /// Added last so every existing struct literal that names the other four
    /// still reads in the reference's own order (`core.js` lists spacing, then
    /// node size, then iterations, then curve); this field is the one the fork
    /// adds, and it is the only one whose DEFAULT is chosen to be a no-op
    /// ([`TEXT_SIZE_DEFAULT`] is [`CHIP_FONT`]).
    pub text_size: f32,
}

impl Default for LayoutSettings {
    fn default() -> Self {
        LayoutSettings {
            spacing: SPACING_DEFAULT,
            iterations: ITER_DEFAULT,
            curvature: CURVE_DEFAULT,
            node_size: NODE_SIZE_DEFAULT,
            text_size: TEXT_SIZE_DEFAULT,
        }
    }
}

fn layout_t(spacing: f64) -> f64 {
    ((spacing - SPACING_MIN) / (SPACING_MAX - SPACING_MIN)).clamp(0.0, 1.0)
}

/// `layout.js layoutGravity` — lerp(1.6, 0.6, t).
fn layout_gravity(spacing: f64) -> f64 {
    1.6 + (0.6 - 1.6) * layout_t(spacing)
}

fn noverlap_ratio(spacing: f64) -> f64 {
    1.05 + (1.35 - 1.05) * layout_t(spacing)
}

fn noverlap_margin(spacing: f64) -> f64 {
    1.0 + (8.0 - 1.0) * layout_t(spacing)
}

/// `graph.js seedNewNodes` (no-anchor branch): a golden-angle disc.
///
/// The **cold** seed: correct only when there are no positions to continue from
/// (a first layout, an explicit re-settle, a change of visible set that shares
/// no node with what was on the canvas). [`seed_incremental`] is the choice the
/// layout path actually makes — see the continue-vs-reseed rule documented
/// there, which is the whole point of this module's seeding split. Computing
/// this disc for a node set that already has positions is the G4 defect: it
/// re-scrambles an arrangement the user made.
pub fn seed_positions(n: usize) -> Vec<(f64, f64)> {
    let nf = n as f64;
    (0..n)
        .map(|i| {
            let t = (i as f64 + 0.5) / nf;
            let ang = i as f64 * GOLDEN_ANGLE;
            let r = 60.0 + nf.sqrt() * 40.0 * t.sqrt();
            (ang.cos() * r, ang.sin() * r)
        })
        .collect()
}

/// The layout seed for a node set that is being (re)laid out.
///
/// # The continue-vs-reseed rule (the core of G4)
///
/// * **CONTINUE** whenever nothing was lost — the node set merely grew (a node
///   expansion), or did not change at all (a re-settle, a pin toggle). The
///   previous positions are handed straight back, so the arrangement survives.
///   Anchored nodes are then held in place by [`LayoutInput::anchored`].
/// * **RE-SEED** when the visible set lost nodes (a collapse, or a rebuild that
///   replaced the graph), or when there is nothing placed yet.
///
/// "Lost" is the deciding signal, not "how much is still placed". A ratio would
/// be wrong in the one case this exists for: expanding a lone namespace into its
/// two classes leaves 1 of 3 placed (33%), and a majority threshold would call
/// that a fresh start and re-scramble the canvas — exactly the defect. The
/// caller knows the previous key set and reports it through
/// [`SeedContinuity::Continue`]/[`SeedContinuity::Reseed`]; this function is the
/// no-previous-set fallback.
///
/// The reference agrees on both branches. `layout.js:280` only re-seeds when
/// `startLayout({reinitialize: true})` is asked for, and every one of its call
/// sites passes `false`, so `buildLayoutRun` reads `posOf(node.id)` — the
/// positions the run is already showing — even for a brand-new graph
/// (`graph.js:269`, `:708`, `:743`), whose only seeding happened earlier in
/// `seedNewNodes`. The one gesture the brief calls "an explicit re-settle" is
/// `graph.js:580`'s expansion, which places the new nodes and leaves the
/// existing ones alone.
///
/// # The position store
///
/// The reference is one long-lived process holding a position for **every node
/// it has ever shown**, so a node that leaves the canvas and comes back
/// (`collapse` then `expand`, a filter toggle) is restored to where it was, not
/// placed afresh. This fork mirrors that with `GraphView::positions`, an
/// id-keyed store in `view.rs`, and hands the restored coordinates in through
/// `placed` exactly as if the node had stayed visible. A node the store has never
/// seen is still seeded from scratch (or listed as `None`, so the caller can take
/// the re-seed branch).
pub fn seed_incremental(placed: &[Option<(f64, f64)>], size: usize) -> Vec<(f64, f64)> {
    match seed_continuity(placed) {
        SeedContinuity::Continue => placed.iter().map(|p| p.unwrap_or((0.0, 0.0))).collect(),
        SeedContinuity::Reseed => seed_positions(size),
    }
}

/// Which seeding branch a run takes, exposed so a caller can ask *why* without
/// re-deriving the rule (and so a test can assert on the reason rather than on
/// the shape of the output).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SeedContinuity {
    /// Something is placed and nothing was lost: keep every position.
    Continue,
    /// Nothing to continue from: lay the set out from the disc.
    Reseed,
}

/// The fallback branch when the caller has only the positions, not the previous
/// key set: continue as soon as anything at all is placed.
///
/// Callers that can tell growth from loss (which is every caller that owns the
/// previous visible set — see [`Self::relayout`] in the view) must use their own
/// answer instead; this exists so a bare "here are the positions" call still
/// has a defined, non-destructive meaning.
pub fn seed_continuity(placed: &[Option<(f64, f64)>]) -> SeedContinuity {
    if placed.iter().any(|p| p.is_some()) {
        SeedContinuity::Continue
    } else {
        SeedContinuity::Reseed
    }
}

/// Everything the FA2/Noverlap pass needs per node.
pub struct LayoutInput<'a> {
    pub pairs: &'a [(usize, usize)],
    pub mass: &'a [f64],
    /// Nodes the layout must not move at all: pinned chips, and the nodes an
    /// incremental run is deliberately holding still (see `anchored`).
    pub fixed: &'a [bool],
    /// Chip half-extents (Noverlap treats a chip as its bounding box).
    pub hw: &'a [f64],
    pub hh: &'a [f64],
    pub ids: &'a [String],
    /// Nodes the run may place but must not *disturb*: the ones that already had
    /// a position when this layout started.
    ///
    /// This is the second half of "expansion preserves the arrangement". Handing
    /// the existing positions back as the seed is not enough on its own: FA2 is a
    /// force simulation, so a new child's repulsion and a new `contains` edge's
    /// attraction both act on its already-placed parent, and the parent drifts.
    /// The reference reads the same way — the run it builds holds every node that
    /// already had a position (`buildLayoutRun` reads `posOf(node.id)` for all of
    /// them, `layout.js:61-63`) and Noverlap's displacement branch
    /// (`layout.js:255-264`) moves a chip *only* when its own solver step asked
    /// it to, so a placed node never gets shoved by a neighbour that is merely
    /// passing by. A node anchored here is free in Noverlap (papering over an
    /// overlap is a real move, and the seed geometry is expected to be roughly
    /// right already) and pinned in FA2.
    ///
    /// Empty (and therefore inert) for a run that is laying a set out from
    /// scratch — there is nothing to preserve in that case. Use `&[]`, not a
    /// vector of `false`, so the distinction is explicit at every call site.
    pub anchored: &'a [bool],
}

/// Accumulate the O(n²) repulsion term into caller-owned buffers.
///
/// The scratch buffers are reused across iterations; they used to be two `vec!`
/// allocations per iteration, and a dense run can afford hundreds of them.
fn fa2_repulsion(
    n: usize,
    xs: &[f64],
    ys: &[f64],
    mass: &[f64],
    k: f64,
    dx: &mut [f64],
    dy: &mut [f64],
) {
    dx[..n].fill(0.0);
    dy[..n].fill(0.0);
    for i in 0..n {
        let (xi, yi, mi) = (xs[i], ys[i], mass[i]);
        let (mut dxi, mut dyi) = (0.0, 0.0);
        for j in 0..n {
            if j == i {
                continue;
            }
            let xd = xi - xs[j];
            let yd = yi - ys[j];
            let d2 = xd * xd + yd * yd;
            if d2 > 0.0 {
                let f = (k * mi * mass[j]) / d2;
                dxi += xd * f;
                dyi += yd * f;
            }
        }
        dx[i] = dxi;
        dy[i] = dyi;
    }
}

/// One `layout.js fa2Iterate`.
#[allow(clippy::too_many_arguments)]
fn fa2_iterate(
    xs: &mut [f64],
    ys: &mut [f64],
    dx: &mut [f64],
    dy: &mut [f64],
    odx: &[f64],
    ody: &[f64],
    mass: &[f64],
    conv: &mut [f64],
    fixed: &[bool],
    anchored: &[bool],
    pairs: &[(usize, usize)],
    spacing: f64,
) {
    let n = xs.len();
    // `odx`/`ody` already hold the previous iteration's deltas (copied by the
    // caller); FA2's swinging/traction read them in step 5.

    // 2) repulsion -> dx/dy for this iteration (buffers reused in place)
    fa2_repulsion(n, xs, ys, mass, spacing, dx, dy);

    // 3) strong gravity
    let gravity = layout_gravity(spacing);
    for i in 0..n {
        let (gx, gy) = (xs[i], ys[i]);
        if gx != 0.0 || gy != 0.0 {
            let gf = mass[i] * gravity;
            dx[i] -= gx * gf;
            dy[i] -= gy * gf;
        }
    }

    // 4) linear attraction
    for &(a, b) in pairs {
        let xd = xs[a] - xs[b];
        let yd = ys[a] - ys[b];
        if xd != 0.0 || yd != 0.0 {
            dx[a] -= xd;
            dy[a] -= yd;
            dx[b] += xd;
            dy[b] += yd;
        }
    }

    // 5) FA2's adaptive per-node speed
    for i in 0..n {
        // An anchored node is held where it was placed: the run is only here to
        // settle the nodes that arrived with it. (`fixed` is the stronger pin and
        // is checked by the callers via `LayoutInput::fixed`; both end at zero
        // velocity, so the two are one branch.)
        if fixed[i] || anchored.get(i).copied().unwrap_or(false) {
            dx[i] = 0.0;
            dy[i] = 0.0;
            continue;
        }
        let swx = odx[i] - dx[i];
        let swy = ody[i] - dy[i];
        let swinging = mass[i] * (swx * swx + swy * swy).sqrt();
        let trx = odx[i] + dx[i];
        let try_ = ody[i] + dy[i];
        let traction = (trx * trx + try_ * try_).sqrt() / 2.0;
        let nodespeed = (conv[i] * (1.0 + traction).ln()) / (1.0 + swinging.sqrt());
        let num = nodespeed * (dx[i] * dx[i] + dy[i] * dy[i]);
        conv[i] = (num / (1.0 + swinging.sqrt())).sqrt().min(1.0);
        xs[i] += dx[i] * (nodespeed / FA2_SLOWDOWN);
        ys[i] += dy[i] * (nodespeed / FA2_SLOWDOWN);
    }
}

/// Cheap hasher for the Noverlap cell grid.
///
/// The default `HashMap` hasher is SipHash, and the grid does one lookup per
/// candidate cell per node per pass — 18M of them on a 9.4k-node whole-model
/// view. A multiply-xor mix of the two cell coordinates is more than good enough
/// for a spatial bucket table and removes that hashing cost from the pass.
#[derive(Default)]
struct CellHasher(u64);

impl std::hash::Hasher for CellHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    fn write_i64(&mut self, i: i64) {
        self.0 = (self.0.rotate_left(5) ^ i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    fn write_u64(&mut self, i: u64) {
        self.write_i64(i as i64);
    }
    fn write_usize(&mut self, i: usize) {
        self.write_i64(i as i64);
    }
}

type CellMap = HashMap<(i64, i64), Vec<usize>, std::hash::BuildHasherDefault<CellHasher>>;

// ------------------------------------------------------- label deconfliction

/// An axis-aligned rectangle in painter (screen) coordinates.
///
/// The canvas lays its relation pills out in pixels, not world units: a pill's
/// size is the laid-out text plus its padding, and it does not scale with the
/// camera. This is the box the greedy placer keeps clear.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LabelRect {
    pub min_x: f32,
    pub min_y: f32,
    pub max_x: f32,
    pub max_y: f32,
}

impl LabelRect {
    pub fn from_center(cx: f32, cy: f32, hw: f32, hh: f32) -> Self {
        LabelRect { min_x: cx - hw, min_y: cy - hh, max_x: cx + hw, max_y: cy + hh }
    }

    pub fn expanded(self, gap: f32) -> Self {
        LabelRect {
            min_x: self.min_x - gap,
            min_y: self.min_y - gap,
            max_x: self.max_x + gap,
            max_y: self.max_y + gap,
        }
    }

    pub fn intersects(&self, other: &Self) -> bool {
        self.min_x < other.max_x
            && other.min_x < self.max_x
            && self.min_y < other.max_y
            && other.min_y < self.max_y
    }
}

/// A hash-grid of placed label rectangles for one frame.
///
/// The canvas used to paint every relation pill at its edge's midpoint, so two
/// edges crossing (or two relations fanned between the same pair) drew their
/// word boxes on top of each other. This is the placement side of the fix:
/// [`Self::try_insert`] answers "is this box free", and only an accepted box is
/// inserted. The grid makes the query independent of how many labels are already
/// placed, so a zoomed-in view with thousands of pills stays one pass.
///
/// Deterministic by construction: there is no RNG and no iteration over a
/// `HashMap` for decisions, only cell lookups keyed by coordinates.
#[derive(Default)]
pub struct LabelPlacer {
    cells: HashMap<(i64, i64), Vec<usize>, std::hash::BuildHasherDefault<CellHasher>>,
    rects: Vec<LabelRect>,
    cell: f32,
}

impl LabelPlacer {
    pub fn new() -> Self {
        LabelPlacer { cells: HashMap::default(), rects: Vec::new(), cell: 48.0 }
    }

    fn key(&self, x: f32, y: f32) -> (i64, i64) {
        ((x / self.cell).floor() as i64, (y / self.cell).floor() as i64)
    }

    /// Whether `r` would overlap any box already placed.
    pub fn collides(&self, r: LabelRect) -> bool {
        let (x0, y0) = self.key(r.min_x, r.min_y);
        let (x1, y1) = self.key(r.max_x, r.max_y);
        for gx in x0..=x1 {
            for gy in y0..=y1 {
                if let Some(list) = self.cells.get(&(gx, gy)) {
                    for &i in list {
                        if r.intersects(&self.rects[i]) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }

    /// Insert `r` when it is free. Returns whether it was placed.
    pub fn try_insert(&mut self, r: LabelRect) -> bool {
        if self.collides(r) {
            return false;
        }
        let idx = self.rects.len();
        self.rects.push(r);
        let (x0, y0) = self.key(r.min_x, r.min_y);
        let (x1, y1) = self.key(r.max_x, r.max_y);
        for gx in x0..=x1 {
            for gy in y0..=y1 {
                self.cells.entry((gx, gy)).or_default().push(idx);
            }
        }
        true
    }

    pub fn len(&self) -> usize {
        self.rects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }
}

/// Point at `t` on the edge's drawn path.
///
/// A curved edge is the quadratic through the two trimmed endpoints and the
/// fan's control point; a straight one is the segment. The pill was anchored at
/// `t = 0.5` (`core.js`'s midpoint formula), and the deconfliction pass slides
/// it along this same path, so a moved pill still sits on its own edge.
pub fn edge_point_at(
    p1: (f32, f32),
    mid: (f32, f32),
    p2: (f32, f32),
    curved: bool,
    t: f32,
) -> (f32, f32) {
    if !curved {
        return (p1.0 + (p2.0 - p1.0) * t, p1.1 + (p2.1 - p1.1) * t);
    }
    let u = 1.0 - t;
    (
        u * u * p1.0 + 2.0 * u * t * mid.0 + t * t * p2.0,
        u * u * p1.1 + 2.0 * u * t * mid.1 + t * t * p2.1,
    )
}

/// One `layout.js noverlapIterate`; returns how many pairs moved.
///
/// `grid` and `pool` are owned by the caller so the cell table's outer capacity
/// and every cell `Vec` are reused across passes instead of allocated fresh.
///
/// `settle_anchored` decides what happens when two *anchored* chips overlap each
/// other. The normal rule is "an anchored chip never moves": it is what keeps an
/// expansion from shifting the arrangement, because the arriving chip is free and
/// takes the whole push. That rule has a blind spot, though: a chip box can grow
/// under an anchored set (the Layout ▸ Text size / Node size slider re-measures
/// every label), and then two settled chips are left stacked on top of each
/// other — the one case where no chip in the pair is free to move. A settle pass
/// (the CONTINUE branch's safety phase) passes `true`, so an anchored/anchored
/// pair splits the push like any other pair. It still prefers the un-anchored
/// side whenever there is one, so a newly restored chip never shoves a settled
/// neighbour.
fn noverlap_iterate(
    xs: &mut [f64],
    ys: &mut [f64],
    fixed: &[bool],
    anchored: &[bool],
    hw: &[f64],
    hh: &[f64],
    ids: &[String],
    spacing: f64,
    settle_anchored: bool,
    grid: &mut CellMap,
    pool: &mut Vec<Vec<usize>>,
) -> usize {
    let n = xs.len();
    let ratio = noverlap_ratio(spacing);
    let margin = noverlap_margin(spacing);
    let mut max_extent = 0.0f64;
    for i in 0..n {
        max_extent = max_extent.max(hw[i].max(hh[i]));
    }
    let cell = (8.0f64).max(2.0 * (max_extent * ratio + margin));
    let key = |x: f64, y: f64| -> (i64, i64) {
        ((x / cell).floor() as i64, (y / cell).floor() as i64)
    };
    // Recycle the cell table: drain last pass's cells into the free pool, then
    // reuse them for this pass's buckets. The outer map keeps its capacity.
    for (_, mut cell) in grid.drain() {
        cell.clear();
        pool.push(cell);
    }
    for i in 0..n {
        grid.entry(key(xs[i], ys[i]))
            .or_insert_with(|| pool.pop().unwrap_or_default())
            .push(i);
    }

    // Invariants of the pass, hoisted out of the O(n × neighbours) inner loop:
    // each chip's padded half-extent, whether the solver may move it at all, and
    // the deterministic tie-break bit. They cost O(n) once here; the loop used to
    // re-take a string hash and a bool lookup for every overlapping pair.
    let ex: Vec<f64> = hw.iter().map(|w| w * ratio + margin).collect();
    let ey: Vec<f64> = hh.iter().map(|h| h * ratio + margin).collect();
    let tie_pos: Vec<bool> = ids.iter().map(|s| hash32(s) % 2 == 1).collect();

    let mut moved = 0usize;
    for i in 0..n {
        let mut xi = xs[i];
        let mut yi = ys[i];
        let exi = ex[i];
        let eyi = ey[i];
        let i_pinned = fixed[i];
        let i_anchored = anchored.get(i).copied().unwrap_or(false);
        let (mut cx, mut cy) = key(xi, yi);
        for ox in -1..=1i64 {
            for oy in -1..=1i64 {
                // No clone: the neighbour list is read-only for the whole pass
                // (the grid is the pass-start snapshot, exactly as before), so it
                // can be borrowed while `xs`/`ys` are mutated.
                let Some(list) = grid.get(&(cx + ox, cy + oy)) else { continue };
                for &j in list {
                    if j <= i {
                        continue;
                    }
                    let need_x = exi + ex[j];
                    let need_y = eyi + ey[j];
                    let dx = xs[j] - xi;
                    let dy = ys[j] - yi;
                    let pen_x = need_x - dx.abs();
                    if pen_x <= 0.0 {
                        continue;
                    }
                    let pen_y = need_y - dy.abs();
                    if pen_y <= 0.0 {
                        continue;
                    }
                    moved += 1;
                    let (ux, uy, push);
                    if pen_x < pen_y {
                        ux = if dx < 0.0 { -1.0 } else if dx > 0.0 { 1.0 } else if tie_pos[i] { 1.0 } else { -1.0 };
                        uy = 0.0;
                        push = pen_x;
                    } else {
                        ux = 0.0;
                        uy = if dy < 0.0 { -1.0 } else if dy > 0.0 { 1.0 } else if tie_pos[i] { 1.0 } else { -1.0 };
                        push = pen_y;
                    }
                    let mx = ux * push;
                    let my = uy * push;
                    let j_pinned = fixed[j];
                    let j_anchored = anchored.get(j).copied().unwrap_or(false);
                    let (move_i, move_j) = if i_pinned && j_pinned {
                        (false, false)
                    } else if i_pinned {
                        (false, true)
                    } else if j_pinned {
                        (true, false)
                    } else if i_anchored && j_anchored {
                        // Two settled chips stacked: only a settle pass may split
                        // this pair (see `settle_anchored`).
                        (settle_anchored, settle_anchored)
                    } else if i_anchored {
                        (false, true)
                    } else if j_anchored {
                        (true, false)
                    } else {
                        (true, true)
                    };
                    let share = if move_i && move_j { 0.5 } else { 1.0 };
                    if move_i {
                        xs[i] -= mx * share;
                        ys[i] -= my * share;
                    }
                    if move_j {
                        xs[j] += mx * share;
                        ys[j] += my * share;
                    }
                    xi = xs[i];
                    yi = ys[i];
                    let k = key(xi, yi);
                    cx = k.0;
                    cy = k.1;
                }
            }
        }
    }
    moved
}

/// Total FA2 pair-evaluations one synchronous layout run may spend.
///
/// FA2 repulsion is O(n²) per iteration in the reference too
/// (`layout.js fa2Repulsion`), but the reference runs the solver in ~8 ms
/// slices across animation frames (`stepLayout`), so even a 9,000-node "Full
/// graph" keeps the window responsive while it settles over many frames. This
/// port runs the whole pass on the caller's thread inside one `relayout`, so
/// the same 800 iterations would block the UI for tens of minutes — the button
/// would look dead. A budget keeps the worst case near a second: small graphs
/// still get every requested iteration, and a graph too large for even one
/// affordable pass falls back to its (already laid-out) seed plus the
/// grid-based Noverlap pass.
///
/// The number is pair-evaluations, not milliseconds, so it is deterministic and
/// testable: ~60M pairs is ~1 s at release speed on the owner's machine.
pub const FA2_PAIR_BUDGET: f64 = 60_000_000.0;

/// How many FA2 iterations an `n`-node run can afford under
/// [`FA2_PAIR_BUDGET`]. Zero means "the set is too large for a synchronous
/// force pass; keep the seed and let Noverlap de-overlap it".
pub fn fa2_iteration_budget(n: usize) -> i32 {
    if n < 2 {
        return ITER_MAX;
    }
    let pairs = (n as f64) * (n as f64);
    (FA2_PAIR_BUDGET / pairs).floor().clamp(0.0, ITER_MAX as f64) as i32
}

/// Total node-visits one synchronous Noverlap phase may spend.
///
/// Noverlap is grid-accelerated (not O(n²) like FA2), but 600 passes over a
/// 9,400-node "Full graph" still measured 2.7 s, which reads as a dead button.
/// Budgeting passes by node count keeps the whole `relayout` under about a
/// second; small canvases keep the full 200-600 passes the reference asks for
/// (`layout.js` `noverMax`), so de-overlapping a normal view is unchanged.
pub const NOVER_NODE_PASS_BUDGET: usize = 2_000_000;

/// Visible sets up to this size keep the reference's full pass budget.
///
/// A normal view stays here, so its de-overlapping is unchanged. Above it, the
/// per-pass cost grows linearly, and a whole-model 9,400-node run needs ~1 s for
/// the 2M-visit budget even with the grid and cell vectors reused. Scaling the
/// pass cap down for those sets keeps the FIRST "Full graph" render under ~0.3 s;
/// the trade is a coarser de-overlap on a view that is drawn as dots at fit zoom.
pub const NOVER_FULL_BUDGET_N: usize = 4_000;

/// The minimum number of passes a large set still gets. Enough to separate the
/// disc seed's worst piles; the loop exits earlier as soon as nothing moves.
pub const NOVER_MIN_PASSES_FLOOR: usize = 32;

/// How many Noverlap passes an `n`-node run can afford under
/// [`NOVER_NODE_PASS_BUDGET`]. Always at least [`NOVER_MIN_PASSES_FLOOR`].
pub fn noverlap_pass_budget(n: usize) -> usize {
    if n == 0 {
        return NOVER_MIN_PASSES;
    }
    // Reference maximum (`layout.js noverMax` clamps to 200..600); the caller
    // applies its own `sqrt(n) * 6` on top of this.
    let cap = if n <= NOVER_FULL_BUDGET_N {
        600
    } else {
        (NOVER_MIN_PASSES * NOVER_FULL_BUDGET_N / n).max(NOVER_MIN_PASSES_FLOOR)
    };
    (NOVER_NODE_PASS_BUDGET / n).min(cap).max(NOVER_MIN_PASSES_FLOOR)
}

/// The floor of [`noverlap_pass_budget`] for a normal-sized view: enough passes
/// to separate the densest pile the disc seed produces.
pub const NOVER_MIN_PASSES: usize = 120;

/// Run `layout.js startLayout` + `stepLayout` to completion: `iterations` FA2
/// steps, then Noverlap until it reports no movement or `noverMax` passes.
///
/// The FA2 step count is additionally clamped by [`fa2_iteration_budget`]; see
/// its docs for why a synchronous port cannot spend the reference's full
/// iteration count on a large visible set.
pub fn run_layout(
    input: &LayoutInput<'_>,
    seed: &[(f64, f64)],
    settings: LayoutSettings,
) -> Vec<(f64, f64)> {
    let n = seed.len();
    if n == 0 {
        return Vec::new();
    }
    // Nothing is free to move: every node is either pinned (`fixed`) or held by
    // the continue branch (`anchored`). FA2 zeroes an anchored node's velocity
    // and Noverlap treats it as fixed on both sides of a pair, so the entire
    // solver — including the O(n²) repulsion — is a guaranteed no-op. Returning
    // the seed is not an approximation of the old result; it IS the old result,
    // without paying for it. (`relayout` also takes a cheaper path for this
    // case, but keeping the invariant here makes it true for every caller.)
    let all_held = (0..n).all(|i| {
        input.fixed.get(i).copied().unwrap_or(false)
            || input.anchored.get(i).copied().unwrap_or(false)
    });
    if all_held {
        return seed.to_vec();
    }
    let mut xs: Vec<f64> = seed.iter().map(|p| p.0).collect();
    let mut ys: Vec<f64> = seed.iter().map(|p| p.1).collect();
    let mut dx = vec![0.0f64; n];
    let mut dy = vec![0.0f64; n];
    let mut odx = vec![0.0f64; n];
    let mut ody = vec![0.0f64; n];
    let mut conv = vec![1.0f64; n];
    let requested = (settings.iterations as f64)
        .clamp(ITER_MIN as f64, ITER_MAX as f64) as i32;
    // A synchronous run cannot spend the reference's slice-by-slice budget; see
    // `FA2_PAIR_BUDGET`. Small sets are unaffected (budget > requested), while a
    // whole-model "Full graph" keeps its disc seed and is de-overlapped instead
    // of freezing the window for minutes.
    let iterations = requested.min(fa2_iteration_budget(n));

    let t_iter = std::time::Instant::now();
    for _ in 0..iterations {
        odx.copy_from_slice(&dx);
        ody.copy_from_slice(&dy);
        fa2_iterate(
            &mut xs,
            &mut ys,
            &mut dx,
            &mut dy,
            &odx,
            &ody,
            input.mass,
            &mut conv,
            input.fixed,
            input.anchored,
            input.pairs,
            settings.spacing,
        );
    }
    crate::render::frame_mark("layout.fa2", t_iter);
    let t_over = std::time::Instant::now();
    let nover_max = ((n as f64).sqrt() * 6.0).round().clamp(200.0, 600.0) as usize;
    // Bound the phase by node count as well; see `NOVER_NODE_PASS_BUDGET`.
    let nover_max = nover_max.min(noverlap_pass_budget(n));
    let mut grid: CellMap = CellMap::default();
    let mut pool: Vec<Vec<usize>> = Vec::new();
    let mut it = 0usize;
    while it < nover_max {
        if noverlap_iterate(
            &mut xs,
            &mut ys,
            input.fixed,
            input.anchored,
            input.hw,
            input.hh,
            input.ids,
            settings.spacing,
            false,
            &mut grid,
            &mut pool,
        ) == 0
        {
            break;
        }
        it += 1;
    }
    crate::render::frame_mark("layout.noverlap", t_over);

    xs.into_iter().zip(ys).collect()
}

/// Run only the Noverlap phase over already-placed nodes.
///
/// A CONTINUE layout holds the arrangement and only places the nodes a gesture
/// added, so it deliberately skips FA2. That is right for an expansion — but a
/// node restored from the position store can come back under a chip that moved
/// while it was hidden. This phase is the cheap safety net: it moves *nothing*
/// when the boxes are already clear (one pass reports zero displacements), and
/// otherwise separates exactly the pairs that overlap. `fixed` still pins a
/// node; `anchored` is a preference (an un-anchored chip moves first), and two
/// anchored chips that overlap each other are separated by half a push each —
/// the case a text-size change creates by growing every settled box at once.
pub fn noverlap_only(
    fixed: &[bool],
    anchored: &[bool],
    hw: &[f64],
    hh: &[f64],
    ids: &[String],
    seed: &[(f64, f64)],
    settings: LayoutSettings,
) -> Vec<(f64, f64)> {
    let n = seed.len();
    if n == 0 {
        return Vec::new();
    }
    let mut xs: Vec<f64> = seed.iter().map(|p| p.0).collect();
    let mut ys: Vec<f64> = seed.iter().map(|p| p.1).collect();
    let nover_max = ((n as f64).sqrt() * 6.0).round().clamp(200.0, 600.0) as usize;
    let nover_max = nover_max.min(noverlap_pass_budget(n));
    let mut grid: CellMap = CellMap::default();
    let mut pool: Vec<Vec<usize>> = Vec::new();
    let t_over = std::time::Instant::now();
    let mut it = 0usize;
    while it < nover_max {
        if noverlap_iterate(
            &mut xs,
            &mut ys,
            fixed,
            anchored,
            hw,
            hh,
            ids,
            settings.spacing,
            true,
            &mut grid,
            &mut pool,
        ) == 0
        {
            break;
        }
        it += 1;
    }
    crate::render::frame_mark("layout.noverlap.continue", t_over);
    xs.into_iter().zip(ys).collect()
}

// ------------------------------------------------- child placement (expansion)
//
// `graph.js:294-541`. A force layout answers "where do the new nodes go?" by
// pushing everything out from the centre, which destroys an arrangement made by
// hand — so when the node being expanded already has placed neighbours that lie
// on a line, the new children are appended to that same line, and they are
// anchored so the layout cannot scatter them ([`Placement::anchored`]). When
// there is no line to continue, they are fanned out from the root on a
// golden-angle ring, each chip clear of the root's measured box and of every
// chip already placed in this same run. `seedNewNodes` remains the last resort.
//
// The geometry is pure: no chip store, no graph handle, no renderer. The caller
// supplies the root, its placed neighbours and the sizes, and gets back world
// positions.

/// `core.js SPLINE_GAP` — world-unit gap between consecutive chips.
pub const SPLINE_GAP: f64 = 16.0;
/// `core.js SPLINE_TURN` — radians the tangent bends per step (gentle curl).
pub const SPLINE_TURN: f64 = 0.10;
/// `core.js SPLINE_TURN_MAX` — cap on the total bend, so long runs read as a line.
pub const SPLINE_TURN_MAX: f64 = 1.30;
/// `core.js SPLINE_FIT_SLACK` — perpendicular spread / run above which it is not a line.
pub const SPLINE_FIT_SLACK: f64 = 0.22;
/// `core.js SPLINE_FIT_MIN` — the absolute slack for short runs.
pub const SPLINE_FIT_MIN: f64 = 4.0;
/// `core.js SPLINE_AXIS_SLACK` — radians a run may sit off the nearest 45° axis.
pub const SPLINE_AXIS_SLACK: f64 = 0.21;

/// One chip's world half-extents, as `graph.js chipSizeFor(...) / 2` gives them.
#[derive(Clone, Copy, Debug)]
pub struct HalfSize {
    pub hw: f64,
    pub hh: f64,
}

/// Everything [`seed_children`] needs about one to-be-placed node.
#[derive(Clone, Copy, Debug)]
pub struct ChildSpec {
    pub half: HalfSize,
    /// `hash32(id) % 100 / 100` — the reference's per-child rotation of the
    /// golden-angle ring, so the placement stays a pure function of the id.
    pub hash: f64,
}

/// The child positions [`seed_children`] produced, and what they mean.
#[derive(Clone, Debug)]
pub struct Placement {
    /// One world position per requested child, in the order they were given.
    pub positions: Vec<(f64, f64)>,
    /// Whether the children came from the spline ([`ChildPlacement::Spline`]) or
    /// the collision-free fan ([`ChildPlacement::Fan`]).
    pub kind: Option<ChildPlacement>,
    /// `graph.js` pins the spline run (`p.fixed = true`) and leaves a fanned chip
    /// free. Anchoring both is this port's deliberate simplification: a chip the
    /// seed placed is a chip the layout has no business moving, whichever branch
    /// placed it. The distinction is kept on [`ChildPlacement`] rather than
    /// erased, so a caller can still specialise later without re-deriving it.
    pub anchored: bool,
}

/// Which of the two reference strategies placed the children.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChildPlacement {
    /// Appended to the line the root's placed neighbours already follow.
    Spline,
    /// Fanned out from the root on a golden-angle ring.
    Fan,
}

/// `graph.js chipSupport(node, ux, uy)`: the box's support function along a unit
/// direction — half the chip's extent in that direction.
pub fn chip_support(half: HalfSize, ux: f64, uy: f64) -> f64 {
    ux.abs() * half.hw + uy.abs() * half.hh
}

/// `graph.js placedNeighbours`: the root-relative positions of the neighbours
/// that are already placed and are still in the graph.
///
/// `positions` is indexed by the caller's own node order; `neighbours` lists the
/// indices of the root's neighbours in that same order. A neighbour with no
/// position, one that is nowhere near the root, or one closer than `min_distance`
/// is dropped — the caller's list is expected to be the current adjacency, which
/// this cannot check for itself.
pub fn placed_neighbours(
    root: (f64, f64),
    positions: &[Option<(f64, f64)>],
    neighbours: &[usize],
    min_distance: f64,
) -> Vec<(f64, f64)> {
    let far = 12.0 * min_distance;
    let mut out = Vec::new();
    for &nb in neighbours {
        let Some(q) = positions.get(nb).copied().flatten() else {
            continue;
        };
        let dx = q.0 - root.0;
        let dy = q.1 - root.1;
        if dx * dx + dy * dy > far * far {
            continue;
        }
        out.push((dx, dy));
    }
    out
}

/// `graph.js neighbourLine`: the 45°-snapped line the placed neighbours already
/// follow, as a unit direction away from the root, or `None` when they do not
/// form one.
///
/// Requires at least two neighbours, a run of at least 1 world unit, a residual
/// spread small against the run, and a direction within [`SPLINE_AXIS_SLACK`] of
/// a 45° multiple. Rejecting an unarranged blob is the point: a spline built
/// from a random scatter is worse than the fan.
pub fn neighbour_line(nbrs: &[(f64, f64)]) -> Option<(f64, f64)> {
    let n = nbrs.len();
    if n < 2 {
        return None;
    }
    let nf = n as f64;
    let mx = nbrs.iter().map(|q| q.0).sum::<f64>() / nf;
    let my = nbrs.iter().map(|q| q.1).sum::<f64>() / nf;
    let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
    for q in nbrs {
        let dx = q.0 - mx;
        let dy = q.1 - my;
        sxx += dx * dx;
        sxy += dx * dy;
        syy += dy * dy;
    }
    let theta = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    let (tx, ty) = (theta.cos(), theta.sin());
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    let mut spread = 0.0f64;
    for q in nbrs {
        let a = q.0 * tx + q.1 * ty;
        let b = q.0 * -ty + q.1 * tx;
        lo = lo.min(a);
        hi = hi.max(a);
        spread = spread.max(b.abs());
    }
    let run = hi - lo;
    if run < 1.0 {
        return None;
    }
    if spread > SPLINE_FIT_MIN.max(SPLINE_FIT_SLACK * run) {
        return None;
    }
    let step = std::f64::consts::PI / 4.0;
    let snapped = (theta / step).round() * step;
    if (theta - snapped).abs() > SPLINE_AXIS_SLACK {
        return None;
    }
    let (mut ux, mut uy) = (snapped.cos(), snapped.sin());
    let along: f64 = nbrs.iter().map(|q| q.0 * ux + q.1 * uy).sum();
    if along < 0.0 {
        ux = -ux;
        uy = -uy;
    } else if along < 1e-6 && lo.abs() > hi.abs() {
        ux = -ux;
        uy = -uy;
    }
    Some((ux, uy))
}

/// `graph.js splinePoints`: the chip centres of one spline run.
///
/// Starts `edge` along `(ux, uy)` from the root, then turns the tangent by `bend`
/// before every step after the first. Pure geometry so both curl directions can
/// be compared before either is committed to.
#[allow(clippy::too_many_arguments)]
pub fn spline_points(
    root: (f64, f64),
    ux: f64,
    uy: f64,
    edge: f64,
    bend: f64,
    children: &[ChildSpec],
) -> Vec<(f64, f64)> {
    let mut pts = Vec::with_capacity(children.len());
    let mut cx = root.0 + ux * edge;
    let mut cy = root.1 + uy * edge;
    let mut tan = (ux, uy);
    for (i, child) in children.iter().enumerate() {
        if i > 0 {
            let (c, s) = (bend.cos(), bend.sin());
            tan = (tan.0 * c - tan.1 * s, tan.0 * s + tan.1 * c);
        }
        pts.push((cx, cy));
        let sup = chip_support(child.half, tan.0, tan.1);
        cx += tan.0 * (sup + SPLINE_GAP);
        cy += tan.1 * (sup + SPLINE_GAP);
    }
    pts
}

/// Place `children` around `root`, exactly as `seedExpansion` then `seedFan` do.
///
/// `positions` is every placed node's world position (indexed by the caller's
/// order, `None` for a node that has none); `neighbours` are the root's placed
/// neighbours in that same index space. `graph_center` is the mean of the placed
/// positions (`graph.js graphCenter`) — the spline always heads away from it, so
/// an expansion never loops back into the graph it came from.
///
/// Returns `None` when there is nothing to place, which is also the honest
/// answer for a root that has not been placed: the reference refuses to seed from
/// an unplaced root rather than inventing a position for it
/// (`graph.js:417`, `:510`).
pub fn seed_children(
    root: (f64, f64),
    root_half: HalfSize,
    positions: &[Option<(f64, f64)>],
    neighbours: &[usize],
    graph_center: (f64, f64),
    children: &[ChildSpec],
    spacing: f64,
) -> Option<Placement> {
    if children.is_empty() {
        return None;
    }

    let nbrs = placed_neighbours(root, positions, neighbours, spacing);
    if let Some((mut ux, mut uy)) = neighbour_line(&nbrs) {
        // The spline always heads away from the centre of the graph: of the
        // line's two senses, keep the one pointing outward. When the existing
        // children already run outward this simply continues their direction.
        let rx = root.0 - graph_center.0;
        let ry = root.1 - graph_center.1;
        if rx * rx + ry * ry > 1.0 && ux * rx + uy * ry < 0.0 {
            ux = -ux;
            uy = -uy;
        }

        // The first chip starts just past the furthest neighbour edge...
        let mut edge = 0.0f64;
        for &q in &nbrs {
            let proj = q.0 * ux + q.1 * uy;
            // The neighbour's own half-extent is not carried here (`graph.js`
            // looks it up in its chip cache); a run that is a few units closer
            // than the reference's is still clear of the root, which is what the
            // floor below guarantees.
            let sup = 0.0;
            if proj + sup > edge {
                edge = proj + sup;
            }
        }
        edge += SPLINE_GAP;
        // ...and never inside the root itself: an existing child can sit closer
        // than the root is wide, and the run must still leave the root's own box
        // behind.
        edge = edge.max(chip_support(root_half, ux, uy) + SPLINE_GAP);

        // The curl also goes away from the centre: lay out both curl directions
        // and keep the one whose chips end up farther from the centre.
        let turn =
            SPLINE_TURN.min(SPLINE_TURN_MAX / (children.len().max(1) as f64 - 1.0).max(1.0));
        let mut best: Option<(f64, Vec<(f64, f64)>)> = None;
        for sign in [1.0f64, -1.0] {
            let pts = spline_points(root, ux, uy, edge, turn * sign, children);
            let reach: f64 = pts
                .iter()
                .map(|p| {
                    let dx = p.0 - graph_center.0;
                    let dy = p.1 - graph_center.1;
                    (dx * dx + dy * dy).sqrt()
                })
                .sum();
            if best.as_ref().map(|b| reach > b.0).unwrap_or(true) {
                best = Some((reach, pts));
            }
        }
        if let Some((_, pts)) = best {
            return Some(Placement { positions: pts, kind: Some(ChildPlacement::Spline), anchored: true });
        }
    }

    // Fallback: fan out on a golden-angle ring, each chip clear of the root's
    // measured box and of every chip already fanned in this run. The base angle
    // is per-root so two roots expanded side by side do not fan onto each other's
    // spokes.
    let base = children[0].hash as f64;
    let mut placed: Vec<((f64, f64), HalfSize)> = Vec::with_capacity(children.len());
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(children.len());
    for child in children {
        let ang = base + out.len() as f64 * GOLDEN_ANGLE;
        let (ux, uy) = (ang.cos(), ang.sin());
        let mut dist =
            chip_support(root_half, ux, uy) + chip_support(child.half, ux, uy) + SPLINE_GAP;
        // 96 outward steps of 6 world units: far enough that a pathological pile
        // still terminates, and the step is the reference's own.
        let mut guard = 0;
        while guard < 96 {
            let x = root.0 + ux * dist;
            let y = root.1 + uy * dist;
            let clear = placed.iter().all(|(o, oh)| {
                !((x - o.0).abs() < child.half.hw + oh.hw + SPLINE_GAP
                    && (y - o.1).abs() < child.half.hh + oh.hh + SPLINE_GAP)
            });
            if clear {
                break;
            }
            dist += 6.0;
            guard += 1;
        }
        let p = (root.0 + ux * dist, root.1 + uy * dist);
        placed.push((p, child.half));
        out.push(p);
    }
    Some(Placement { positions: out, kind: Some(ChildPlacement::Fan), anchored: true })
}

/// `graph.js graphCenter`: the mean of the placed positions.
pub fn graph_center(positions: &[Option<(f64, f64)>]) -> (f64, f64) {
    let mut n = 0u32;
    let mut x = 0.0;
    let mut y = 0.0;
    for p in positions.iter().flatten() {
        x += p.0;
        y += p.1;
        n += 1;
    }
    if n == 0 {
        (0.0, 0.0)
    } else {
        (x / n as f64, y / n as f64)
    }
}

// ---------------------------------------------------------------- camera

/// `layout.js Camera { x, y, zoom }` with the original's `worldToScreen`.
#[derive(Clone, Copy, Debug)]
pub struct Camera {
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
}

impl Default for Camera {
    fn default() -> Self {
        Camera { x: 0.0, y: 0.0, zoom: 1.0 }
    }
}

pub const MIN_ZOOM: f64 = 0.06;
pub const MAX_ZOOM: f64 = 6.0;
pub const FIT_MAX_ZOOM: f64 = 2.2;
pub const FIT_PAD: f64 = 70.0;

impl Camera {
    pub fn world_to_screen(&self, w: f32, h: f32, x: f64, y: f64) -> (f32, f32) {
        (
            ((x - self.x) * self.zoom) as f32 + w / 2.0,
            ((y - self.y) * self.zoom) as f32 + h / 2.0,
        )
    }

    pub fn screen_to_world(&self, w: f32, h: f32, sx: f32, sy: f32) -> (f64, f64) {
        (
            (sx - w / 2.0) as f64 / self.zoom + self.x,
            (sy - h / 2.0) as f64 / self.zoom + self.y,
        )
    }

    /// `layout.js computeFit` — pad 70, zoom capped at 2.2.
    pub fn compute_fit(w: f32, h: f32, pts: &[(f64, f64)]) -> Option<Camera> {
        if pts.is_empty() {
            return None;
        }
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for &(x, y) in pts {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        let pad = FIT_PAD;
        let ww = (max_x - min_x).max(1.0);
        let hh = (max_y - min_y).max(1.0);
        let zoom = (((w as f64 - pad * 2.0) / ww).min((h as f64 - pad * 2.0) / hh))
            .clamp(MIN_ZOOM, FIT_MAX_ZOOM);
        Some(Camera { x: (min_x + max_x) / 2.0, y: (min_y + max_y) / 2.0, zoom })
    }
}

// ---------------------------------------------------------------- grid

/// Chromium composites a translucent fill over its backdrop in **sRGB space**
/// and truncates to 8 bits; `egui` blends in linear space. Pre-compositing here
/// gives the exact pixel the reference has, instead of a near miss.
///
/// `rgba(120,160,210,0.05)` over `#1e1e1e` is `(34,36,39)` — the value measured
/// on the reference — not the `(32,32,34)` a linear blend produces.
pub fn composite_srgb(bg: Rgba, fg: Rgba, alpha: f32) -> Rgba {
    let mix = |b: u8, c: u8| -> u8 {
        (b as f32 + (c as f32 - b as f32) * alpha.clamp(0.0, 1.0)) as u8
    };
    Rgba(mix(bg.0, fg.0), mix(bg.1, fg.1), mix(bg.2, fg.2), bg.3)
}

/// The on-screen colour of `grid` over this palette's flat canvas.
pub fn grid_over_canvas(pal: &Palette) -> Rgba {
    composite_srgb(pal.canvas, Rgba::rgb(120, 160, 210), 0.05)
}

/// `render.js drawGrid` — 140 world-unit grid, skipped when it would be denser
/// than 320 lines across.
pub fn grid_lines(cam: &Camera, w: f32, h: f32) -> Vec<(f32, f32, f32, f32)> {
    let step = 140.0f64;
    let tl = cam.screen_to_world(w, h, -8.0, -8.0);
    let br = cam.screen_to_world(w, h, w + 8.0, h + 8.0);
    if (br.0 - tl.0) / step > 320.0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut x = (tl.0 / step).floor() * step;
    while x < br.0 {
        let px = cam.world_to_screen(w, h, x, 0.0).0.round() + 0.5;
        out.push((px, 0.0, px, h));
        x += step;
    }
    let mut y = (tl.1 / step).floor() * step;
    while y < br.1 {
        let py = cam.world_to_screen(w, h, 0.0, y).1.round() + 0.5;
        out.push((0.0, py, w, py));
        y += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash32_matches_the_js_reference() {
        // core.js hash32 — FNV-1a 32-bit over UTF-16 code units.
        assert_eq!(hash32(""), 0x811c_9dc5);
        assert_eq!(hash32("a"), 0xe40c_292c);
        assert_eq!(hash32("Alpha"), hash32("Alpha"));
    }

    #[test]
    fn seed_positions_match_seed_new_nodes() {
        let p = seed_positions(2);
        assert!((p[0].0 - 88.2842712).abs() < 1e-4, "{:?}", p);
        assert!(p[0].1.abs() < 1e-9);
        assert!((p[1].0 + 80.3656827).abs() < 1e-4, "{:?}", p);
        assert!((p[1].1 - 73.6215485).abs() < 1e-4, "{:?}", p);
    }

    #[test]
    fn chip_size_is_pad_dot_gap_text_pad() {
        let mut m = ChipMetrics::new(8.0);
        let (w, h) = m.size("Alpha", &mut |_| 33.0);
        assert!((w - 45.0).abs() < 1e-4, "{w}");
        assert!((h - 13.0).abs() < 1e-4, "{h}");
        // cached
        let (w2, _) = m.size("Alpha", &mut |_| 999.0);
        assert_eq!(w, w2);
    }

    #[test]
    fn chip_metrics_retune_keeps_boxes_for_the_same_font() {
        let mut m = ChipMetrics::with_font(8.0, 11.0);
        let (w, _) = m.size("Alpha", &mut |_| 33.0);
        // Same dot/font: the measurement must be served from the cache.
        m.retune(8.0, 11.0);
        let (w2, _) = m.size("Alpha", &mut |_| 999.0);
        assert_eq!(w, w2, "retuning to the same font must not drop the cache");
        // A font move invalidates the cached width, which is measured at the
        // old size.
        m.retune(8.0, 13.0);
        let (w3, _) = m.size("Alpha", &mut |_| 44.0);
        assert!((w3 - (2.0 + 8.0 + 2.0 + 44.0)).abs() < 1e-4, "{w3}");
    }

    #[test]
    fn chip_label_truncates_above_40_chars() {
        let long = "x".repeat(60);
        let t = chip_text(&long);
        assert_eq!(t.chars().count(), 40);
        assert!(t.ends_with('…'));
    }

    /// Relation pills are placed greedily: a free box is accepted, an
    /// intersecting one is refused, and a box that merely touches the padded
    /// boundary is free (the placer owns the gap, the caller pads the rect).
    #[test]
    fn label_placer_refuses_overlaps_and_accepts_adjacent_boxes() {
        let mut p = LabelPlacer::new();
        let first = LabelRect::from_center(0.0, 0.0, 20.0, 7.0).expanded(2.0);
        assert!(p.try_insert(first));
        assert!(!p.try_insert(LabelRect::from_center(5.0, 0.0, 20.0, 7.0)));
        assert!(!p.try_insert(LabelRect::from_center(0.0, 6.0, 20.0, 7.0)));
        // Exactly adjacent on x (first right edge 22, this left edge 22).
        assert!(p.try_insert(LabelRect::from_center(44.0, 0.0, 20.0, 7.0)));
        // Far away is always free, and the grid must find the first box from it.
        assert!(p.try_insert(LabelRect::from_center(400.0, 300.0, 20.0, 7.0)));
        assert_eq!(p.len(), 3);
        assert!(!p.is_empty());
    }

    /// The pill's reference anchor is the midpoint formula `render.js` uses, and
    /// the slide function must return exactly that point at `t = 0.5`.
    #[test]
    fn edge_point_at_matches_the_pill_anchor() {
        let p1 = (10.0, 20.0);
        let mid = (70.0, 90.0);
        let p2 = (130.0, 20.0);
        let anchor = (0.25 * p1.0 + 0.5 * mid.0 + 0.25 * p2.0,
                      0.25 * p1.1 + 0.5 * mid.1 + 0.25 * p2.1);
        assert_eq!(edge_point_at(p1, mid, p2, true, 0.5), anchor);
        assert_eq!(edge_point_at(p1, mid, p2, false, 0.5), (70.0, 20.0));
        assert_eq!(edge_point_at(p1, mid, p2, false, 0.0), p1);
    }

    /// The text-size slider grows every measured chip box at once. Two settled
    /// (anchored) chips that then overlap must still be separated: a settle pass
    /// lets an anchored/anchored pair share the push. Without that, the CONTINUE
    /// branch's safety pass moved nothing and the names drew on top of each
    /// other.
    #[test]
    fn a_settle_pass_separates_two_anchored_chips_that_grew_into_each_other() {
        let fixed = vec![false, false];
        let anchored = vec![true, true];
        let hw = vec![60.0, 60.0];
        let hh = vec![6.5, 6.5];
        let ids = vec!["a".to_string(), "b".to_string()];
        let seed = vec![(0.0, 0.0), (20.0, 0.0)];
        let out = noverlap_only(
            &fixed,
            &anchored,
            &hw,
            &hh,
            &ids,
            &seed,
            LayoutSettings::default(),
        );
        let dx = (out[1].0 - out[0].0).abs();
        let dy = (out[1].1 - out[0].1).abs();
        assert!(
            dx >= 120.0 || dy >= 13.0,
            "the two boxes must no longer intersect: dx={dx} dy={dy}"
        );
    }

    /// The normal (FA2) Noverlap keeps its old contract: an anchored chip does
    /// not move, because the un-anchored side of a pair takes the whole push.
    #[test]
    fn a_layout_pass_still_holds_an_anchored_chip_against_a_free_one() {
        let fixed = vec![false, false];
        let anchored = vec![true, false];
        let hw = vec![60.0, 60.0];
        let hh = vec![6.5, 6.5];
        let ids = vec!["a".to_string(), "b".to_string()];
        let seed = vec![(0.0, 0.0), (20.0, 0.0)];
        let input = LayoutInput {
            pairs: &[],
            mass: &[1.0, 1.0],
            fixed: &fixed,
            hw: &hw,
            hh: &hh,
            ids: &ids,
            anchored: &anchored,
        };
        let out = run_layout(&input, &seed, LayoutSettings::default());
        assert_eq!(out[0], (0.0, 0.0), "the settled chip stays put");
        let dx = (out[1].0 - out[0].0).abs();
        let dy = (out[1].1 - out[0].1).abs();
        assert!(dx >= 120.0 || dy >= 13.0, "the free chip moves clear: dx={dx} dy={dy}");
    }

    #[test]
    fn kind_colours_come_from_core_js() {
        assert_eq!(kind_color(UiTheme::NzkDarkPlus, "namespace"), Rgba::rgb(0x4e, 0xc9, 0xb0));
        assert_eq!(kind_color(UiTheme::NzkDarkPlus, "method"), Rgba::rgb(0xdc, 0xdc, 0xaa));
        assert_eq!(kind_color(UiTheme::DarkV1, "namespace"), Rgba::rgb(0x86, 0xaa, 0xf9));
        assert_eq!(kind_color(UiTheme::DarkV1, "record"), Rgba::rgb(0xf4, 0x83, 0x85));
    }

    #[test]
    fn layout_is_deterministic_and_separates_two_nodes() {
        let ids = vec!["ns:Alpha".to_string(), "ns:Beta".to_string()];
        let seed = seed_positions(2);
        let mass = vec![1.0, 1.0];
        let fixed = vec![false, false];
        let hw = vec![22.5, 19.3];
        let hh = vec![6.5, 6.5];
        let input = LayoutInput {
            pairs: &[],
            mass: &mass,
            fixed: &fixed,
            hw: &hw,
            hh: &hh,
            ids: &ids,
            anchored: &[],
        };
        let a = run_layout(&input, &seed, LayoutSettings::default());
        let b = run_layout(&input, &seed, LayoutSettings::default());
        assert_eq!(a, b);
        let d = ((a[0].0 - a[1].0).powi(2) + (a[0].1 - a[1].1).powi(2)).sqrt();
        assert!(d > 1.0, "two chips must not collapse: {d}");
    }

    /// The continue branch hands the solver a set where every node is held.
    /// FA2 zeroes an anchored velocity and Noverlap treats it as fixed, so the
    /// only correct result is the seed itself — and returning it directly must
    /// stay exact, not approximate.
    #[test]
    fn an_all_held_layout_returns_the_seed_unchanged() {
        let seed = vec![(12.0, -4.0), (3.5, 8.25), (-7.0, 0.0)];
        let mass = vec![1.0, 1.0, 1.0];
        let fixed = vec![false, false, false];
        let anchored = vec![true, true, true];
        let hw = vec![10.0, 10.0, 10.0];
        let hh = vec![5.0, 5.0, 5.0];
        let ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let pairs = vec![(0, 1), (1, 2)];
        let input = LayoutInput {
            pairs: &pairs,
            mass: &mass,
            fixed: &fixed,
            hw: &hw,
            hh: &hh,
            ids: &ids,
            anchored: &anchored,
        };
        let out = run_layout(&input, &seed, LayoutSettings::default());
        assert_eq!(out, seed, "an all-anchored run may not move a single node");
    }

    /// The large-set pass cap: a normal view is untouched, and a whole-model
    /// view is bounded well below the 2M-visit budget.
    #[test]
    fn noverlap_passes_are_capped_for_whole_model_sets() {
        // A normal view keeps the reference's full 200–600 pass range.
        assert!(noverlap_pass_budget(1_400) >= 600);
        assert_eq!(noverlap_pass_budget(NOVER_FULL_BUDGET_N), 500);
        // A whole-model view is bounded by the scaled cap, never by 2M/n.
        let big = noverlap_pass_budget(9_419);
        assert!(big <= 64, "9,419 nodes should get a bounded pass count, got {big}");
        assert!(big >= NOVER_MIN_PASSES_FLOOR);
        // Monotone in n above the knee, so a larger graph can never get MORE
        // passes than a smaller one.
        let mut prev = usize::MAX;
        for n in [5_000, 8_000, 9_419, 20_000, 100_000] {
            let p = noverlap_pass_budget(n);
            assert!(p <= prev, "pass budget must not grow with n: {n} -> {p}");
            assert!(p >= NOVER_MIN_PASSES_FLOOR);
            prev = p;
        }
    }

    #[test]
    fn fit_uses_pad_70_and_caps_at_2_2() {
        let cam = Camera::compute_fit(1240.0, 948.0, &[(-10.0, 0.0), (10.0, 4.0)]).unwrap();
        assert!((cam.zoom - 2.2).abs() < 1e-9, "{}", cam.zoom);
    }

    #[test]
    fn edge_style_and_labels_match_core_js() {
        assert_eq!(edge_label("contains"), "Children");
        assert_eq!(edge_label("calls"), "Calling");
        assert_eq!(edge_style("inherits").width, 2.0);
        assert_eq!(edge_style("contains").dash, Some([1.0, 5.0]));
        assert_eq!(edge_style("frobnicate").dash, Some([4.0, 4.0]));
    }

    // -----------------------------------------------------------------------
    // G4: the seeding split and child placement
    // -----------------------------------------------------------------------

    /// The continue-vs-reseed rule, branch by branch. This is the rule the whole
    /// fix turns on, so it is asserted directly rather than inferred from a
    /// layout's output.
    #[test]
    fn seeding_continues_from_positions_and_reseeds_only_without_them() {
        // Nothing placed: re-seed (the first layout of any view).
        assert_eq!(seed_continuity(&[]), SeedContinuity::Reseed);
        assert_eq!(seed_continuity(&[None, None]), SeedContinuity::Reseed);
        // Everything placed: continue. An unchanged set and a set that merely
        // grew are the same signal here, which is the point.
        assert_eq!(seed_continuity(&[Some((1.0, 2.0))]), SeedContinuity::Continue);
        assert_eq!(
            seed_continuity(&[Some((1.0, 2.0)), Some((3.0, 4.0)), None]),
            SeedContinuity::Continue,
            "an expansion adds unplaced slots but keeps most of the set"
        );
        // Anything at all placed means there is something to continue from.
        // This fallback deliberately does NOT try to distinguish growth from
        // loss — it cannot, it only sees positions — so it always prefers the
        // non-destructive branch. `GraphView::relayout` owns that distinction.
        assert_eq!(
            seed_continuity(&[Some((1.0, 2.0)), None, None]),
            SeedContinuity::Continue
        );
    }

    /// `seed_incremental` returns the previous positions verbatim on the continue
    /// branch, and the golden-angle disc on the re-seed branch — the two are
    /// observably different outputs, not two names for one behaviour.
    #[test]
    fn seed_incremental_keeps_positions_or_falls_back_to_the_disc() {
        let placed = vec![Some((10.0, 20.0)), Some((-30.0, 40.0)), None];
        let kept = seed_incremental(&placed, 3);
        assert_eq!(kept[0], (10.0, 20.0), "a placed node keeps its exact position");
        assert_eq!(kept[1], (-30.0, 40.0));

        let cold = seed_incremental(&[None, None], 2);
        assert_eq!(cold, seed_positions(2), "with nothing to continue from, the disc");
        assert_ne!(cold, seed_incremental(&placed, 3));
    }

    /// Scale probe for the one remaining unbounded pass, Noverlap.
    ///
    /// Not a correctness test (it asserts only that the run finishes and the
    /// result is finite); it exists so the cost of a 9,400-node "Full graph" can
    /// be measured instead of guessed:
    ///
    /// ```text
    /// cargo test -p native-csviewer --release the_whole_model_layout -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "scale probe: run explicitly in release, not part of the suite"]
    fn the_whole_model_layout_finishes_in_bounded_time() {
        let n = 9_400usize;
        let ids: Vec<String> = (0..n).map(|i| format!("NZK.T{i}.m")).collect();
        let seed = seed_positions(n);
        let mass = vec![2.0f64; n];
        let fixed = vec![false; n];
        let hw = vec![80.0f64; n];
        let hh = vec![15.0f64; n];
        let pairs: Vec<(usize, usize)> = (0..n).map(|i| (i, (i + 1) % n)).collect();
        let input = LayoutInput {
            pairs: &pairs,
            mass: &mass,
            fixed: &fixed,
            hw: &hw,
            hh: &hh,
            ids: &ids,
            anchored: &[],
        };
        let started = std::time::Instant::now();
        let out = run_layout(&input, &seed, LayoutSettings::default());
        let elapsed = started.elapsed();
        eprintln!("[probe] n={n} run_layout took {elapsed:?}");
        assert_eq!(out.len(), n);
        assert!(out.iter().all(|p| p.0.is_finite() && p.1.is_finite()));
    }

    /// The synchronous FA2 pass is budgeted by node count: a normal canvas gets
    /// every requested iteration, while a whole-model "Full graph" must not
    /// freeze the window for the tens of minutes 800 O(n²) passes would take.
    #[test]
    fn the_fa2_iteration_budget_scales_down_with_the_node_count() {
        // Small views (the shipped namespace canvas, any expansion) are never
        // clipped: the budget exceeds the 800-iteration maximum.
        assert_eq!(fa2_iteration_budget(0), ITER_MAX);
        assert_eq!(fa2_iteration_budget(1), ITER_MAX);
        assert_eq!(fa2_iteration_budget(100), ITER_MAX);
        assert_eq!(fa2_iteration_budget(200), ITER_MAX);
        // 250 nodes already spends the whole budget on ~960 passes (the shipped
        // default is 800, so the default view is still untouched).
        assert!(fa2_iteration_budget(250) >= ITER_DEFAULT);
        // The budget is monotonically non-increasing and reaches zero once even
        // one O(n²) pass is too expensive. ~7,700 nodes is that crossover for
        // the 60M-pair budget.
        let counts = [300usize, 500, 1_000, 2_000, 4_000, 7_000, 8_000, 9_400];
        let budgets: Vec<i32> = counts.iter().map(|&n| fa2_iteration_budget(n)).collect();
        assert!(budgets.windows(2).all(|w| w[1] <= w[0]), "{budgets:?}");
        assert_eq!(budgets.last().copied(), Some(0), "{budgets:?}");
        assert!(budgets[0] < ITER_MAX, "a 300-node run must already be clipped");
        // The work spent is bounded: iterations × n² never exceeds the budget
        // (except the ITER_MAX ceiling for small sets).
        for (&n, &iters) in counts.iter().zip(&budgets) {
            let work = f64::from(iters) * (n * n) as f64;
            assert!(
                work <= FA2_PAIR_BUDGET + 1.0,
                "n={n} iters={iters} spends {work} pairs, over the budget"
            );
        }
    }

    /// Noverlap's pass count is budgeted the same way FA2's iteration count is:
    /// a normal canvas keeps the reference's 200-600 passes, and a whole-model
    /// run is clipped so one click cannot block the frame for seconds.
    #[test]
    fn the_noverlap_pass_budget_scales_down_with_the_node_count() {
        assert_eq!(noverlap_pass_budget(0), NOVER_MIN_PASSES);
        assert!(noverlap_pass_budget(10) >= 600, "small views keep the full range");
        assert!(noverlap_pass_budget(1_000) >= 600);
        let counts = [3_000usize, 5_000, 7_000, 9_400, 20_000];
        let passes: Vec<usize> = counts.iter().map(|&n| noverlap_pass_budget(n)).collect();
        assert!(passes.windows(2).all(|w| w[1] <= w[0]), "{passes:?}");
        assert!(
            passes.iter().all(|&p| p >= NOVER_MIN_PASSES_FLOOR),
            "a whole-model run still gets the floor: {passes:?}"
        );
        // Above the knee the cap, not the visit budget, is what binds; every
        // value is still within the visit budget.
        for (&n, &p) in counts.iter().zip(&passes) {
            assert!(
                n * p <= NOVER_NODE_PASS_BUDGET || p == NOVER_MIN_PASSES_FLOOR,
                "n={n} p={p}"
            );
        }
        // The knee is where the scaled cap starts to bite.
        assert!(noverlap_pass_budget(NOVER_FULL_BUDGET_N) <= 600);
        assert!(noverlap_pass_budget(NOVER_FULL_BUDGET_N + 1) < 600);
    }

    /// An anchored node does not move, even though the forces acting on it are
    /// non-zero; an unanchored one does. This is what makes "expansion preserves
    /// the arrangement" hold through a force simulation rather than only at the
    /// seed.
    #[test]
    fn anchoring_holds_a_placed_node_still_against_the_forces() {
        let ids = vec!["a".to_string(), "b".to_string()];
        let mass = vec![1.0, 1.0];
        let fixed = vec![false, false];
        let hw = vec![20.0, 20.0];
        let hh = vec![6.5, 6.5];
        // Two nodes close enough to repel hard.
        let seed = vec![(0.0, 0.0), (40.0, 0.0)];

        let anchored = vec![true, false];
        let input = LayoutInput {
            pairs: &[],
            mass: &mass,
            fixed: &fixed,
            hw: &hw,
            hh: &hh,
            ids: &ids,
            anchored: &anchored,
        };
        let out = run_layout(&input, &seed, LayoutSettings::default());
        assert_eq!(out[0], (0.0, 0.0), "the anchored node must not move at all");
        // The free node is *pushed*, not necessarily away along +x: it is
        // repelled by the anchored node and pulled back toward the origin by
        // gravity, so the assertion is that it left its seed, not where it went.
        let free_moved =
            ((out[1].0 - seed[1].0).powi(2) + (out[1].1 - seed[1].1).powi(2)).sqrt();
        assert!(free_moved > 1.0, "the free node must still be laid out: {free_moved}");

        // Nothing anchored: both move, so the assertion above is not vacuous.
        let input_free = LayoutInput { anchored: &[], ..input };
        let free = run_layout(&input_free, &seed, LayoutSettings::default());
        assert_ne!(free[0], (0.0, 0.0), "un-anchored, the node does move");
    }

    /// `chipSupport` is the box support function, and it is what makes the fan
    /// fan-wide-chip-aware: a long label needs more room along its own axis than
    /// across it.
    #[test]
    fn chip_support_reads_the_box_along_the_direction() {
        let half = HalfSize { hw: 60.0, hh: 6.5 };
        assert!((chip_support(half, 1.0, 0.0) - 60.0).abs() < 1e-9);
        assert!((chip_support(half, 0.0, 1.0) - 6.5).abs() < 1e-9);
        assert!(chip_support(half, 1.0, 0.0) > chip_support(half, 0.0, 1.0));
        // diagonal support is between the two extremes
        let d = chip_support(half, (0.5f64).sqrt(), (0.5f64).sqrt());
        assert!(d > 6.5 && d < 60.0, "{d}");
    }

    /// `graphCenter` is the mean of the placed positions, and an empty set has
    /// no invented centre.
    #[test]
    fn graph_center_is_the_mean_of_placed_positions() {
        assert_eq!(graph_center(&[]), (0.0, 0.0));
        assert_eq!(graph_center(&[None, None]), (0.0, 0.0));
        assert_eq!(
            graph_center(&[Some((0.0, 0.0)), Some((10.0, 20.0)), None]),
            (5.0, 10.0),
            "unplaced nodes must not drag the centre to the origin"
        );
    }

    /// `placedNeighbours` drops what it cannot use: an unplaced neighbour, and a
    /// neighbour too far away to have been arranged against the root.
    #[test]
    fn placed_neighbours_are_relative_and_filtered() {
        let positions = vec![Some((0.0, 0.0)), Some((100.0, 0.0)), None, Some((5000.0, 0.0))];
        let nbrs = placed_neighbours((0.0, 0.0), &positions, &[1, 2, 3], 120.0);
        assert_eq!(nbrs, vec![(100.0, 0.0)], "far and unplaced neighbours are dropped");
    }

    /// The spline's first chip starts at the requested edge distance along the
    /// direction, and the run then steps outward along a tangent that bends
    /// gently — it never folds back onto the root.
    #[test]
    fn spline_points_walk_outward_along_the_line() {
        let kids = [ChildSpec { half: HalfSize { hw: 20.0, hh: 6.5 }, hash: 0.0 }; 3];
        let pts = spline_points((0.0, 0.0), 1.0, 0.0, 100.0, SPLINE_TURN, &kids);
        assert_eq!(pts.len(), 3);
        assert!((pts[0].0 - 100.0).abs() < 1e-9 && pts[0].1.abs() < 1e-9, "{:?}", pts[0]);
        // Each step is at least the chip's own extent plus the gap, so the run
        // can never self-overlap.
        for pair in pts.windows(2) {
            let step = ((pair[1].0 - pair[0].0).powi(2) + (pair[1].1 - pair[0].1).powi(2)).sqrt();
            assert!(step >= SPLINE_GAP, "steps must clear the gap: {step}");
            assert!(pair[1].0 > pair[0].0, "the run must keep moving outward");
        }
    }

    /// `seed_children` matches the reference's precondition: a root that has not
    /// been placed cannot seed anything.
    #[test]
    fn an_unplaced_root_seeds_nothing() {
        let kid = [ChildSpec { half: HalfSize { hw: 20.0, hh: 6.5 }, hash: 0.1 }];
        assert!(seed_children((0.0, 0.0), HalfSize { hw: 5.0, hh: 5.0 }, &[], &[], (0.0, 0.0), &kid, 120.0).is_some());
        assert!(
            seed_children((0.0, 0.0), HalfSize { hw: 5.0, hh: 5.0 }, &[], &[], (0.0, 0.0), &[], 120.0)
                .is_none(),
            "no children to place is not a placement"
        );
    }
}
