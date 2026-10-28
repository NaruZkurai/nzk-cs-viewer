//! nzk_ui — a CSS-driven nzk_ui painter for the native CS Viewer.
//!
//! It builds the REAL nzk_ui skeleton in Rust (equivalent to
//! `frontend/index.html`) and reads `frontend/static/style.css` in
//! Rust (html5ever -> Blitz DOM -> Stylo cascade -> Taffy layout -> Parley text
//! -> Vello paint) and can hand back both the resolved box geometry and an
//! RGBA8 raster. There is no browser engine, no webview and no JavaScript: the
//! stylesheet is the specification, and the DOM the 32 scripts would have built
//! at runtime is constructed in Rust (`dom::apply_runtime_dom`).
//!
//! The intended integration is **nzk_ui painter, not interactive shell**:
//! native widgets own behaviour, this crate tells them *where* every element
//! is (`NzkUi::box_of`) and can paint the static nzk_ui behind them
//! (`NzkUi::paint_into`).
//!
//! ```no_run
//! let mut shell = nzk_ui::NzkUi::load_default((1600, 1000), "/tmp/fid-corpus")?;
//! let side = shell.box_of(".side").unwrap();
//! let mut rgba = Vec::new();
//! shell.paint_into(&mut rgba);
//! # Ok::<(), anyhow::Error>(())
//! ```

pub mod dom;
pub mod serve;
pub mod skeleton;
pub mod sha256;

use std::any::Any;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyrender::ImageRenderer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::{BaseDocument, Document, DocumentConfig, LocalName, QualName};
use blitz_html::HtmlDocument;
use blitz_traits::shell::{ColorScheme, Viewport};

pub use dom::Facts;

/// A resolved border box in document coordinates (CSS px, physical pixels at
/// the 1.0 scale factor this renderer uses).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Box2 {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// One measured element rectangle plus the nearest `data-id` on the element or
/// one of its ancestors.
///
/// This is the per-element geometry channel behind the `RECTS=` request field
/// (see `serve.rs`): the host asks for the `.port`/`.port-dot` boxes of every
/// Blocks card and keys each row by the card's `data-id`, so the edge model can
/// anchor to the boxes the layout pass actually painted instead of assuming a
/// stylesheet fraction.
#[derive(Clone, Debug, PartialEq)]
pub struct MeasuredRect {
    /// Nearest `data-id` on the matched element or an ancestor (card key).
    pub data_id: Option<String>,
    /// Resolved border box in document coordinates.
    pub rect: Box2,
}

/// Where the wall-clock went for the last load/paint, so a caller can see
/// whether it is paying for layout or for rasterisation.
#[derive(Clone, Copy, Debug, Default)]
pub struct Timings {
    pub parse_style_layout: Duration,
    pub paint_commands: Duration,
    pub raster: Duration,
}

impl Timings {
    pub fn total_paint(&self) -> Duration {
        self.paint_commands + self.raster
    }
}

/// One ancestor element of a hit, nearest-first.
///
/// The report carries only the two routing attributes a `closest()` walk needs;
/// the `data-*` payloads are already resolved from the hit itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HitAncestor {
    pub id: Option<String>,
    pub class: Option<String>,
}

/// One element as reported by `NzkUi::hit`.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub node_id: usize,
    /// The painted border box of the nearest element that carries a routing
    /// payload (`data-key`/`data-id`/`data-nav`), falling back to the hit
    /// element itself.
    ///
    /// A press lands on a row's *child* span, so the hit node's own rect is only
    /// the label, not the clickable row. This is the rectangle the host paints
    /// hover/press feedback over, measured by the same layout that painted the
    /// pixels, so the affordance cannot describe a different row.
    pub target_rect: Option<Box2>,
    pub tag: String,
    pub id: Option<String>,
    pub class: Option<String>,
    /// The nearest `data-key` on this element or one of its ancestors.
    ///
    /// The `#cs-tabs` bar is created at runtime by `dom::apply_tabs`, so its
    /// rows are not in the static `PROBE_SELECTORS` box map and a press on one
    /// can only be routed from the hit chain. Resolving the payload here, in the
    /// same layout that painted, is the reference's `closest('[data-close]')` /
    /// `.cs-tab[data-key]` walk (`shell.js:1250-1266`): a press on a row's
    /// `.cs-tab-label` child still names the row.
    pub data_key: Option<String>,
    /// The nearest `data-close` on this element or one of its ancestors
    /// (`.cs-tab-x[data-close]`, `shell.js:1216-1222`).
    pub data_close: Option<String>,
    /// The nearest `data-id` on this element or one of its ancestors.
    ///
    /// The graph-mode suggestion popup's rows are created at runtime by
    /// `dom::apply_suggestions` and carry `data-id` = the node id
    /// (`search.js:30`). Like the two payloads above this is resolved in the
    /// same layout that painted, so a press on a `.sug-label` child still names
    /// the row the reference's `ev.target.closest('.sug')` would commit
    /// (`shell.js:527-536`).
    pub data_id: Option<String>,
    /// The nearest `data-nav` on this element or one of its ancestors.
    ///
    /// The folder picker's `.pick` rows are created at runtime by
    /// `dom::apply_picker` and carry the absolute folder path they descend into
    /// (`shell.js:58`). Unlike the three payloads above this one is text (a
    /// path), but the rule is identical: it is resolved in the layout that
    /// painted, so a press on a row's `.pname` child still names the row, and
    /// the host never has to guess the row geometry from an equal-band
    /// approximation.
    pub data_nav: Option<String>,
    /// The nearest `title` attribute on the hit element or one of its ancestors.
    ///
    /// The shell is a static raster: a browser would draw the `title` string as
    /// a hover tooltip, but a painted PNG has no browser to do it. This resolves
    /// the same `closest('[title]')` answer a browser tooltip would show, from
    /// the layout that painted, so the host can paint it natively (see
    /// `app/src/nzk_ui.rs::paint_feedback`). Newlines and tabs are collapsed to
    /// single spaces by the wire encoder, so the value is one line.
    ///
    /// `None` means nothing under the pointer carries a `title` — never "the
    /// tooltip is empty".
    pub title: Option<String>,
    /// Every ancestor element that carries an `id` or a `class`, nearest-first,
    /// excluding the hit element itself.
    ///
    /// This is the half of the reference's `ev.target.closest(...)` that a
    /// single-node hit cannot answer, and it is load-bearing rather than
    /// cosmetic:
    ///
    /// * a button's text is painted as an **anonymous inline box**, so clicking
    ///   the label of `Expand` / `Hide children` / a tab reports that box (no
    ///   id, no class) instead of the `<button>`;
    /// * a row's child span (`.rn-label`, `.dot`, `.fname`) carries its own
    ///   class, not the row's (`.rel-node`, `.frow`).
    ///
    /// The host's resolvers match classes (`prop-action`, `rel-node`, `cs-tab`,
    /// …), so without the ancestor trail those presses were dead in their text
    /// area and worked only on the padding — the reported "I have to click some
    /// buttons twice". The trail is resolved from the same layout that painted,
    /// so a hit and its ancestors cannot come from different trees.
    pub ancestors: Vec<HitAncestor>,
}

/// A [`Document`] nzk_ui can construct itself.
///
/// Blitz's `HtmlDocument` can only be created by parsing an HTML string (its
/// `inner` field is private). The Rust-built tree needs the very same document
/// abstraction, so the wrapper lives here: it is the same `Deref`/`DerefMut`
/// newtype plus the one required trait method.
pub struct Doc {
    inner: BaseDocument,
}

impl Deref for Doc {
    type Target = BaseDocument;
    fn deref(&self) -> &BaseDocument {
        &self.inner
    }
}

impl DerefMut for Doc {
    fn deref_mut(&mut self) -> &mut BaseDocument {
        &mut self.inner
    }
}

impl Document for Doc {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl From<Doc> for BaseDocument {
    fn from(doc: Doc) -> BaseDocument {
        doc.inner
    }
}

/// A parsed, styled and laid-out viewer nzk_ui.
pub struct NzkUi {
    doc: Doc,
    /// Applied scroll offsets and the engine-clamped maxima, in CSS px.
    scroll: Vec<dom::ScrollReport>,
    /// The static skeleton as HTML text (serialised from the Rust tree; an
    /// `--html` override is stored pre-splice). Kept for `resolved_html`.
    html: String,
    size: (u32, u32),
    timings: Timings,
}

fn frontend_dir() -> PathBuf {
    // repo/nzk_ui/ -> repo/frontend
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../frontend")
}

impl NzkUi {
    /// The real frontend shipped in this repository.
    pub fn frontend_dir() -> PathBuf {
        frontend_dir()
    }

    /// Load the real `index.html` + `static/style.css` and lay them out once.
    pub fn load_default(size: (u32, u32), repo: &str) -> anyhow::Result<Self> {
        Self::load_with_status(size, repo, None)
    }

    /// As [`NzkUi::load_default`], with the status-line text supplied by the
    /// caller (the A/B harness passes what the reference session reported).
    pub fn load_with_status(
        size: (u32, u32),
        repo: &str,
        status: Option<&str>,
    ) -> anyhow::Result<Self> {
        let fe = frontend_dir();
        let css_path = fe.join("static/style.css");
        let css = std::fs::read_to_string(&css_path)?;
        Self::from_skeleton_with_status(&css_path, &css, size, repo, status)
    }

    /// Load a specific `index.html` + `style.css` pair.
    pub fn load(
        html_path: &Path,
        css_path: &Path,
        size: (u32, u32),
        repo: &str,
    ) -> anyhow::Result<Self> {
        let raw_html = std::fs::read_to_string(html_path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", html_path.display()))?;
        let css = std::fs::read_to_string(css_path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", css_path.display()))?;
        Self::from_source(&raw_html, css_path, &css, size, repo)
    }

    /// Load from in-memory source (the same `inline_stylesheets` splice the CLI
    /// uses: the CSS bytes reach Stylo unchanged, the HTML is untouched apart
    /// from the `<link>` -> `<style>` swap and the engine-gap shim).
    pub fn from_source(
        raw_html: &str,
        css_path: &Path,
        css: &str,
        size: (u32, u32),
        repo: &str,
    ) -> anyhow::Result<Self> {
        Self::from_source_with_status(raw_html, css_path, css, size, repo, None)
    }

    /// The full constructor: `facts` supplies every piece of `/api/*` data the
    /// nzk_ui shows (kind/namespace counts, the status line, the notice).
    pub fn from_source_with_facts(
        raw_html: &str,
        css_path: &Path,
        css: &str,
        size: (u32, u32),
        facts: &Facts,
    ) -> anyhow::Result<Self> {
        Self::build(raw_html, css_path, css, size, facts.clone())
    }

    /// The full constructor: `status` overrides the runtime's status-line text.
    pub fn from_source_with_status(
        raw_html: &str,
        css_path: &Path,
        css: &str,
        size: (u32, u32),
        repo: &str,
        status: Option<&str>,
    ) -> anyhow::Result<Self> {
        let mut facts = Facts::for_corpus(repo);
        facts.status_override = status.map(|s| s.to_string());
        Self::build(raw_html, css_path, css, size, facts)
    }

    /// Build the nzk_ui from the **Rust-built** static tree.
    ///
    /// This is the endgame constructor: it never reads `frontend/index.html`.
    /// Only two kinds of bytes still come from disk, both at runtime as the
    /// project rules require: `static/style.css` (passed in by the caller) and
    /// the four injected sheets under `nzk_ui/assets/css/`.
    pub fn from_skeleton_with_facts(
        _css_path: &Path,
        css: &str,
        size: (u32, u32),
        facts: &Facts,
    ) -> anyhow::Result<Self> {
        let sheets = skeleton::Sheets::load(css).map_err(|e| anyhow::anyhow!("{e}"))?;
        let root = skeleton::document(&sheets);
        let html = skeleton::serialize(&root);
        let (w, h) = size;
        let t0 = Instant::now();
        let config = DocumentConfig {
            viewport: Some(Viewport::new(w, h, 1.0, ColorScheme::Dark)),
            base_url: Some("http://localhost/".to_string()),
            html_parser_provider: Some(Arc::new(blitz_html::HtmlProvider)),
            ..Default::default()
        };
        let mut doc = Doc {
            inner: skeleton::build_document(config, &root),
        };
        dom::apply_runtime_dom(&mut doc, facts);
        // Blitz drops `text-transform`; supply it from the real stylesheet
        // before layout, because uppercase glyphs are wider.
        dom::apply_text_transform(&mut doc, css);
        doc.resolve(0.0);
        // Blitz cannot paint `::placeholder` or the range track/thumb; lay
        // down the shims over the real boxes, then resolve them.
        dom::apply_paint_shims(&mut doc);
        // The focused text box's value window + caret. Needs the Parley layout
        // the first resolve built, so it runs beside the paint shims.
        dom::apply_form_shim(&mut doc, facts.ui.as_ref().and_then(|u| u.form.as_ref()));
        dom::apply_select_chevrons(&mut doc);
        doc.resolve(0.0);
        let parse_style_layout = t0.elapsed();
        let scroll = match facts.ui.as_ref() {
            Some(ui) => dom::apply_scroll(&mut doc, ui),
            None => Vec::new(),
        };
        Ok(Self {
            doc,
            scroll,
            html,
            size,
            timings: Timings {
                parse_style_layout,
                ..Default::default()
            },
        })
    }

    /// [`NzkUi::from_skeleton_with_facts`] with the runtime status line.
    pub fn from_skeleton_with_status(
        css_path: &Path,
        css: &str,
        size: (u32, u32),
        repo: &str,
        status: Option<&str>,
    ) -> anyhow::Result<Self> {
        let mut facts = Facts::for_corpus(repo);
        facts.status_override = status.map(|s| s.to_string());
        Self::from_skeleton_with_facts(css_path, css, size, &facts)
    }

    fn build(
        raw_html: &str,
        css_path: &Path,
        css: &str,
        size: (u32, u32),
        facts: Facts,
    ) -> anyhow::Result<Self> {
        let html = dom::inline_stylesheets(raw_html, css_path, css)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let (w, h) = size;
        let t0 = Instant::now();
        let config = DocumentConfig {
            viewport: Some(Viewport::new(w, h, 1.0, ColorScheme::Dark)),
            base_url: Some("http://localhost/".to_string()),
            html_parser_provider: Some(Arc::new(blitz_html::HtmlProvider)),
            ..Default::default()
        };
        let mut doc = Doc {
            inner: HtmlDocument::from_html(&html, config).into_inner(),
        };
        dom::apply_runtime_dom(&mut doc, &facts);
        // Blitz drops `text-transform`; supply it from the real stylesheet
        // before layout, because uppercase glyphs are wider.
        dom::apply_text_transform(&mut doc, css);
        doc.resolve(0.0);
        // Blitz cannot paint `::placeholder` or the range track/thumb; lay
        // down the shims over the real boxes, then resolve them.
        dom::apply_paint_shims(&mut doc);
        // The focused text box's value window + caret. Needs the Parley layout
        // the first resolve built, so it runs beside the paint shims.
        dom::apply_form_shim(&mut doc, facts.ui.as_ref().and_then(|u| u.form.as_ref()));
        dom::apply_select_chevrons(&mut doc);
        doc.resolve(0.0);
        let parse_style_layout = t0.elapsed();
        // Scroll is applied after layout: the engine clamps against the resolved
        // scrollable range, and the painter offsets a scroll container by it.
        let scroll = match facts.ui.as_ref() {
            Some(ui) => dom::apply_scroll(&mut doc, ui),
            None => Vec::new(),
        };
        Ok(Self {
            doc,
            scroll,
            html,
            size,
            timings: Timings {
                parse_style_layout,
                ..Default::default()
            },
        })
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The applied scroll offsets and their engine-clamped maxima.
    pub fn scroll(&self) -> &[dom::ScrollReport] {
        &self.scroll
    }

    pub fn timings(&self) -> Timings {
        self.timings
    }

    /// The underlying document, for callers that need Blitz itself.
    pub fn document(&self) -> &BaseDocument {
        &self.doc
    }

    /// Resize the viewport and re-run style+layout (no reparse).
    pub fn resize(&mut self, width: u32, height: u32) {
        let t0 = Instant::now();
        self.doc
            .set_viewport(Viewport::new(width, height, 1.0, ColorScheme::Dark));
        self.doc.resolve(0.0);
        self.size = (width, height);
        self.timings.parse_style_layout = t0.elapsed();
    }

    /// Re-read `style.css` and rebuild the nzk_ui's static skeleton (needed
    /// only when a stylesheet changes). The skeleton tree itself is Rust data,
    /// so there is no `index.html` to re-read.
    pub fn reload(&mut self, repo: &str) -> anyhow::Result<()> {
        let css_name = frontend_dir().join("static/style.css");
        let css = std::fs::read_to_string(&css_name)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", css_name.display()))?;
        *self = Self::from_skeleton_with_status(&css_name, &css, self.size, repo, None)?;
        Ok(())
    }

    fn box_of_node(&self, node_id: usize) -> Option<Box2> {
        let n = self.doc.get_node(node_id)?;
        if !n.is_element() {
            return None;
        }
        let p = n.absolute_position(0.0, 0.0);
        let s = n.final_layout.size;
        Some(Box2 {
            x: p.x,
            y: p.y,
            w: s.width,
            h: s.height,
        })
    }

    /// Resolved border box of the first element matching a CSS selector.
    pub fn box_of(&self, selector: &str) -> Option<Box2> {
        let id = self.doc.query_selector(selector).ok().flatten()?;
        self.box_of_node(id)
    }

    /// Every geometry probe selector, in the fixed order shared with
    /// `tools/domdump.mjs` so the two can be diffed line by line.
    pub fn boxes(&self) -> Vec<(&'static str, Option<Box2>)> {
        dom::PROBE_SELECTORS
            .iter()
            .map(|sel| {
                let b = self
                    .doc
                    .query_selector(sel)
                    .ok()
                    .flatten()
                    .and_then(|id| self.box_of_node(id));
                (*sel, b)
            })
            .collect()
    }

    /// Every element matching `selector`, in document order, each with its
    /// resolved border box and the nearest `data-id` on the element or an
    /// ancestor.
    ///
    /// This is the measurement half of the `RECTS=` serve field: the host names
    /// the selectors once and gets the painted boxes back from **the same
    /// layout pass** that produced the raster. A selector that matches nothing
    /// yields an empty vector (not an error), so "not painted this frame" is
    /// honest emptiness rather than an invented rectangle.
    pub fn rects_of(&self, selector: &str) -> Vec<MeasuredRect> {
        let Ok(ids) = self.doc.query_selector_all(selector) else {
            return Vec::new();
        };
        ids.into_iter()
            .filter_map(|id| {
                Some(MeasuredRect {
                    data_id: self.nearest_data_attr(id, "data-id"),
                    rect: self.box_of_node(id)?,
                })
            })
            .collect()
    }

    /// `textContent` of the first element matching `selector`.
    ///
    /// Read-only inspection, the counterpart to `--dump-tree` that a test can
    /// assert on. Text nodes are concatenated in document order, which is what
    /// `textContent` means; an element with no text yields `Some("")` while a
    /// selector matching nothing yields `None`, so "rendered empty" and "not
    /// there" stay distinguishable.
    pub fn text_of(&self, selector: &str) -> Option<String> {
        let id = self.doc.query_selector(selector).ok().flatten()?;
        let mut out = String::new();
        self.collect_text(id, &mut out);
        Some(out)
    }

    fn collect_text(&self, node_id: usize, out: &mut String) {
        let Some(node) = self.doc.get_node(node_id) else {
            return;
        };
        if let Some(t) = node.text_data() {
            out.push_str(&t.content);
            return;
        }
        let children = node.children.clone();
        for c in children {
            self.collect_text(c, out);
        }
    }

    /// The named attribute of the first element matching `selector`.
    ///
    /// `Ok(Some(v))` = present, `Ok(None)` = the element exists and has no such
    /// attribute (e.g. no `disabled`), `Err(())` = no element matched at all.
    pub fn attr_of(&self, selector: &str, name: &str) -> Result<Option<String>, ()> {
        let id = self.doc.query_selector(selector).ok().flatten().ok_or(())?;
        let node = self.doc.get_node(id).ok_or(())?;
        let data = node.element_data().ok_or(())?;
        Ok(data
            .attrs
            .iter()
            .find(|a| a.name.local == LocalName::from(name))
            .map(|a| a.value.clone()))
    }

    /// The child element ids of the node matching `selector`.
    pub fn child_elements_of(&self, selector: &str) -> Option<Vec<usize>> {
        let id = self.doc.query_selector(selector).ok().flatten()?;
        let node = self.doc.get_node(id)?;
        Some(
            node.children
                .iter()
                .copied()
                .filter(|c| {
                    self.doc
                        .get_node(*c)
                        .map(|n| n.element_data().is_some())
                        .unwrap_or(false)
                })
                .collect(),
        )
    }

    /// `class` list of the node matching `selector`.
    pub fn classes_of(&self, selector: &str) -> Option<Vec<String>> {
        let raw = self.attr_of(selector, "class").ok().flatten()?;
        Some(raw.split_whitespace().map(str::to_string).collect())
    }

    /// The deepest element under a point, the hit-test the integrator needs to
    /// route a click to a native widget.
    pub fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        let h = self.doc.hit(x, y)?;
        let node_id = h.node_id;
        let n = self.doc.get_node(node_id)?;
        let data = n.element_data()?;
        let attr = |name: &str| {
            data.attrs
                .iter()
                .find(|a| a.name.local == LocalName::from(name))
                .map(|a| a.value.clone())
        };
        // The row a press is directed at: the nearest ancestor that carries a
        // routing payload, else the element under the pointer. `data-key` is the
        // primary channel (rows, tabs, drawer controls); `data-id`/`data-nav`
        // cover rows whose identity is a node id or a folder path.
        let target = self
            .nearest_payload_node(node_id, "data-key")
            .or_else(|| self.nearest_payload_node(node_id, "data-id"))
            .or_else(|| self.nearest_payload_node(node_id, "data-nav"))
            .unwrap_or(node_id);
        Some(Hit {
            node_id,
            target_rect: self.box_of_node(target),
            tag: data.name.local.to_string(),
            id: attr("id"),
            class: attr("class"),
            data_key: self.nearest_data_attr(node_id, "data-key"),
            data_close: self.nearest_data_attr(node_id, "data-close"),
            data_id: self.nearest_data_attr(node_id, "data-id"),
            data_nav: self.nearest_data_attr(node_id, "data-nav"),
            title: self
                .nearest_data_attr(node_id, "title")
                .filter(|t| !t.is_empty()),
            ancestors: self.hit_ancestors(node_id),
        })
    }

    /// The nearest-first `id`/`class` trail above a hit node, excluding the node
    /// itself and skipping ancestors that carry neither (anonymous boxes).
    ///
    /// Mirrors [`NzkUi::nearest_data_attr`]'s walk, but keeps every element the
    /// browser's `closest()` could match rather than only the nearest one. The
    /// number of hops is bounded: the deepest routed row is always a handful of
    /// elements above the text, and an unbounded walk would let a hostile tree
    /// grow the wire row without limit.
    fn hit_ancestors(&self, node_id: usize) -> Vec<HitAncestor> {
        const MAX_ANCESTORS: usize = 24;
        let mut out = Vec::new();
        let mut cur = self.doc.get_node(node_id).and_then(|n| n.parent);
        while let Some(id) = cur {
            let Some(node) = self.doc.get_node(id) else { break };
            if let Some(data) = node.element_data() {
                let attr = |name: &str| {
                    data.attrs
                        .iter()
                        .find(|a| a.name.local == LocalName::from(name))
                        .map(|a| a.value.clone())
                        .filter(|v| !v.is_empty())
                };
                let a = HitAncestor { id: attr("id"), class: attr("class") };
                if a.id.is_some() || a.class.is_some() {
                    out.push(a);
                }
            }
            if out.len() >= MAX_ANCESTORS {
                break;
            }
            cur = node.parent;
        }
        out
    }

    /// The value of `data-<name>` on `node_id` or its nearest ancestor that has
    /// it, walking the element tree the way a browser's `closest()` does.
    ///
    /// This is what makes a press on a `.cs-tab`'s child elements (the
    /// `.cs-tab-label` span, the `.cs-tab-x` button, the split mark) still name
    /// its row's `data-key`/`data-close`. Only element nodes are considered: a
    /// text node cannot carry an attribute.
    fn nearest_payload_node(&self, node_id: usize, name: &str) -> Option<usize> {
        let local = LocalName::from(name);
        let mut cur = Some(node_id);
        while let Some(id) = cur {
            let node = self.doc.get_node(id)?;
            if let Some(data) = node.element_data() {
                if data.attrs.iter().any(|a| a.name.local == local) {
                    return Some(id);
                }
            }
            cur = node.parent;
        }
        None
    }

    /// The value of `data-<name>` on `node_id` or its nearest ancestor that has
    /// it, walking the element tree the way a browser's `closest()` does.
    fn nearest_data_attr(&self, node_id: usize, name: &str) -> Option<String> {
        let local = LocalName::from(name);
        let mut cur = Some(node_id);
        while let Some(id) = cur {
            let node = self.doc.get_node(id)?;
            if let Some(data) = node.element_data() {
                if let Some(a) = data.attrs.iter().find(|a| a.name.local == local) {
                    return Some(a.value.clone());
                }
            }
            cur = node.parent;
        }
        None
    }

    /// Paint the nzk_ui into an RGBA8 buffer (`width * height * 4`, row-major,
    /// opaque).
    pub fn paint_into(&self, out: &mut Vec<u8>) {
        let (w, h) = self.size;
        let mut renderer = VelloCpuImageRenderer::new(w, h);
        renderer.render_to_vec(
            |scene| blitz_paint::paint_scene(scene, &self.doc, 1.0, w, h),
            out,
        );
        out.resize((w as usize) * (h as usize) * 4, 0);
        for px in out.chunks_exact_mut(4) {
            px[3] = 255;
        }
    }

    /// Paint and return a fresh RGBA8 buffer.
    pub fn paint(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        self.paint_into(&mut buf);
        buf
    }

    /// Paint with stage timings folded into the returned report.
    pub fn paint_timed(&mut self) -> (Vec<u8>, Timings) {
        let (w, h) = self.size;
        let mut renderer = VelloCpuImageRenderer::new(w, h);
        let mut out: Vec<u8> = Vec::new();
        let scene = std::cell::Cell::new(Duration::ZERO);
        let t0 = Instant::now();
        renderer.render_to_vec(
            |s| {
                let a = Instant::now();
                blitz_paint::paint_scene(s, &self.doc, 1.0, w, h);
                scene.set(a.elapsed());
            },
            &mut out,
        );
        let total = t0.elapsed();
        out.resize((w as usize) * (h as usize) * 4, 0);
        for px in out.chunks_exact_mut(4) {
            px[3] = 255;
        }
        let timings = Timings {
            parse_style_layout: self.timings.parse_style_layout,
            paint_commands: scene.get(),
            raster: total.saturating_sub(scene.get()),
        };
        self.timings = timings;
        (out, timings)
    }

    /// Save the current paint as a PNG.
    pub fn write_png(&self, path: &Path) -> anyhow::Result<()> {
        let (w, h) = self.size;
        let rgba = self.paint();
        let img = image::RgbaImage::from_raw(w, h, rgba)
            .ok_or_else(|| anyhow::anyhow!("buffer is not a {w}x{h} RGBA image"))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        img.save(path)?;
        Ok(())
    }

    /// `--dump-tree` equivalent.
    pub fn print_tree(&self) {
        self.doc.print_tree();
    }

    /// Unused hook so callers can see the exact document that was parsed.
    pub fn resolved_html(&self) -> &str {
        &self.html
    }
}

/// A `QualName` for an HTML attribute, matching what the HTML parser produces.
pub fn attr_qualname(name: &str) -> QualName {
    QualName::new(None, blitz_dom::ns!(), LocalName::from(name))
}

#[cfg(test)]
mod modal_tests {
    use super::*;
    use dom::{Facts, UiState};

    /// Facts carrying only the host view state under test.
    fn facts_with(ui: UiState) -> Facts {
        let mut facts = Facts::for_corpus("/tmp/fixture");
        facts.ui = Some(ui);
        facts
    }

    fn ui(modals: &[&str]) -> UiState {
        UiState {
            modals_open: modals.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// The whole point of [`UiState::modals_open`], measured on the REAL
    /// `index.html` + `style.css`: a shipped modal has a zero-area box, and
    /// listing its id clears `hidden` so the real `.modal-backdrop` rules lay it
    /// out into a non-zero, hit-testable box.
    ///
    /// This is the regression guard for the family. It fails if the allow-list
    /// stops honouring an id, if a modal's shipped class stops being `hidden`,
    /// or if the stylesheet stops making an un-hidden backdrop a real box.
    #[test]
    fn modals_open_clears_hidden_and_gives_a_real_box() {
        const MODALS: [&str; 5] = [
            "shortcutsModal",
            "editModal",
            "previewModal",
            "reorgModal",
            "modal",
        ];

        // CLOSED (the shipped document): every modal is zero-area.
        let mut shell = NzkUi::from_source_with_facts(
            &std::fs::read_to_string(frontend_dir().join("index.html")).expect("read index.html"),
            &frontend_dir().join("static/style.css"),
            &std::fs::read_to_string(frontend_dir().join("static/style.css")).expect("read css"),
            (1600, 1000),
            &facts_with(UiState::default()),
        )
        .expect("build shell");
        for id in MODALS {
            let b = shell
                .box_of(&format!("#{id}"))
                .unwrap_or_else(|| panic!("#{id} must exist in the real index.html"));
            assert!(
                b.w == 0.0 || b.h == 0.0,
                "#{id} ships class=\"hidden\" and must have a zero-area box, got {}x{}",
                b.w,
                b.h
            );
        }

        // OPENED, one modal at a time: a real box, and a CHILD inside it is
        // reachable. The backdrop itself covers the whole viewport, so the
        // hit-test that means anything is the one on the card's contents.
        let children = [
            ("shortcutsModal", "#shortcutsDone"),
            ("editModal", "#editClose"),
            ("previewModal", "#previewClose"),
            ("reorgModal", "#reorgClose"),
            ("modal", "#modalSelect"),
        ];
        for (id, child) in children {
            let mut shell = NzkUi::from_source_with_facts(
                &std::fs::read_to_string(frontend_dir().join("index.html"))
                    .expect("read index.html"),
                &frontend_dir().join("static/style.css"),
                &std::fs::read_to_string(frontend_dir().join("static/style.css"))
                    .expect("read css"),
                (1600, 1000),
                &facts_with(ui(&[id])),
            )
            .expect("build shell");

            let b = shell
                .box_of(&format!("#{id}"))
                .unwrap_or_else(|| panic!("#{id} must still exist when open"));
            assert!(
                b.w > 0.0 && b.h > 0.0,
                "open #{id} must have a real box, got {}x{}",
                b.w,
                b.h
            );

            let c = shell
                .box_of(child)
                .unwrap_or_else(|| panic!("{child} must be laid out inside open #{id}"));
            assert!(
                c.w > 0.0 && c.h > 0.0,
                "{child} inside open #{id} must be laid out, got {}x{}",
                c.w,
                c.h
            );
        }
    }

    /// An id outside the allow-list changes nothing: `modals_open` cannot
    /// un-hide a drawer, a typo, or an arbitrary selector.
    #[test]
    fn modals_open_is_an_allow_list_and_ignores_everything_else() {
        for bad in ["nodeDrawer", "notAModal", "", "shortcutsModal "] {
            let mut shell = NzkUi::from_source_with_facts(
                &std::fs::read_to_string(frontend_dir().join("index.html"))
                    .expect("read index.html"),
                &frontend_dir().join("static/style.css"),
                &std::fs::read_to_string(frontend_dir().join("static/style.css"))
                    .expect("read css"),
                (1600, 1000),
                &facts_with(ui(&[bad])),
            )
            .expect("build shell");
            let b = shell
                .box_of("#shortcutsModal")
                .expect("#shortcutsModal must exist");
            assert!(
                b.w == 0.0 || b.h == 0.0,
                "{bad:?} is not in the modal allow-list and must open nothing, \
                 but #shortcutsModal measured {}x{}",
                b.w,
                b.h
            );
        }
    }

    /// `aria-hidden` must mirror `hidden` on every backdrop, in BOTH directions.
    ///
    /// The four shipped scripts never touch one without the other:
    /// `shell.js`, `reorg.js`, `edit.js` and `preview.js` all pair
    /// `classList.remove('hidden')` with `setAttribute('aria-hidden','false')`
    /// on open, and `classList.add('hidden')` with
    /// `setAttribute('aria-hidden','true')` on close.
    ///
    /// `index.html` ships all four backdrops as `class="modal-backdrop hidden"
    /// aria-hidden="true"`, so a shell that only toggled `hidden` would leave a
    /// visibly open dialog still announced as `aria-hidden="true"` to
    /// assistive tech — an open dialog that screen readers refuse to enter.
    #[test]
    fn modals_open_keeps_aria_hidden_in_lockstep_with_hidden() {
        const BACKDROPS: [(&str, &str); 4] = [
            ("shortcutsModal", "#shortcutsDone"),
            ("editModal", "#editClose"),
            ("previewModal", "#previewClose"),
            ("reorgModal", "#reorgClose"),
        ];

        for (id, child) in BACKDROPS {
            // The exact Electron contract: hidden <=> aria-hidden="true".
            let build = |modals: &[&str]| {
                NzkUi::from_source_with_facts(
                    &std::fs::read_to_string(frontend_dir().join("index.html"))
                        .expect("read index.html"),
                    &frontend_dir().join("static/style.css"),
                    &std::fs::read_to_string(frontend_dir().join("static/style.css"))
                        .expect("read css"),
                    (1600, 1000),
                    &facts_with(ui(modals)),
                )
                .expect("build shell")
            };

            // CLOSED: shipped markup says hidden + aria-hidden="true", and with
            // no modal requested BOTH must still say so.
            let closed = build(&[]);
            let hidden = closed
                .attr_of(&format!("#{id}"), "class")
                .expect("query class")
                .unwrap_or_default();
            assert!(
                hidden.split_whitespace().any(|t| t == "hidden"),
                "closed #{id} must ship the `hidden` class, got class={hidden:?}"
            );
            let aria = closed
                .attr_of(&format!("#{id}"), "aria-hidden")
                .expect("query aria-hidden");
            assert_eq!(
                aria.as_deref(),
                Some("true"),
                "closed #{id} must stay aria-hidden=\"true\", got {aria:?}"
            );

            // OPENED: `hidden` cleared AND aria-hidden flipped to "false".
            let open = build(&[id]);
            let class = open
                .attr_of(&format!("#{id}"), "class")
                .expect("query class")
                .unwrap_or_default();
            assert!(
                !class.split_whitespace().any(|t| t == "hidden"),
                "open #{id} must have `hidden` cleared, got class={class:?}"
            );
            let aria = open
                .attr_of(&format!("#{id}"), "aria-hidden")
                .expect("query aria-hidden");
            assert_eq!(
                aria.as_deref(),
                Some("false"),
                "open #{id} must be aria-hidden=\"false\" so assistive tech can \
                 enter the open dialog, got {aria:?}"
            );

            // The dialog is genuinely reachable, so this is not an attribute
            // assertion floating free of layout.
            let c = open
                .box_of(child)
                .unwrap_or_else(|| panic!("{child} must be laid out inside open #{id}"));
            assert!(
                c.w > 0.0 && c.h > 0.0,
                "{child} inside open #{id} must be laid out, got {}x{}",
                c.w,
                c.h
            );
        }
    }

    /// Opening ONE modal must leave the other three at `aria-hidden="true"`.
    ///
    /// The scripts' close branch is not dead code: opening `editModal` closes
    /// the shortcuts overlay, and it must not leave the other backdrops
    /// announced as available just because the shell ran a modal pass.
    #[test]
    fn opening_one_modal_leaves_the_others_aria_hidden() {
        const ALL: [(&str, &str); 4] = [
            ("shortcutsModal", "#shortcutsDone"),
            ("editModal", "#editClose"),
            ("previewModal", "#previewClose"),
            ("reorgModal", "#reorgClose"),
        ];

        for (open_id, _) in ALL {
            let mut shell = NzkUi::from_source_with_facts(
                &std::fs::read_to_string(frontend_dir().join("index.html"))
                    .expect("read index.html"),
                &frontend_dir().join("static/style.css"),
                &std::fs::read_to_string(frontend_dir().join("static/style.css"))
                    .expect("read css"),
                (1600, 1000),
                &facts_with(ui(&[open_id])),
            )
            .expect("build shell");

            for (id, _) in ALL {
                let aria = shell
                    .attr_of(&format!("#{id}"), "aria-hidden")
                    .expect("query aria-hidden");
                let expected = if id == open_id { "false" } else { "true" };
                assert_eq!(
                    aria.as_deref(),
                    Some(expected),
                    "with #{open_id} open, #{id} must be aria-hidden={expected:?}, \
                     got {aria:?}"
                );
            }
        }
    }

    // ---- W7: find-panel and folder-picker CONTENT -------------------------
    //
    // `drawers_open`/`modals_open` only get an overlay *on screen*. These tests
    // cover the second half of the wire: the text and rows the search and picker
    // cores computed must actually land in the real document, and the two honest
    // state distinctions (`NoQuery` vs `NoHits`, `NotFound` vs `Empty`) must
    // survive all the way into the rendered DOM rather than collapsing into the
    // same empty box.

    /// Build a shell from the real frontend with `ui` as the only facts.
    fn shell_with(ui: UiState) -> NzkUi {
        NzkUi::from_source_with_facts(
            &std::fs::read_to_string(frontend_dir().join("index.html")).expect("read index.html"),
            &frontend_dir().join("static/style.css"),
            &std::fs::read_to_string(frontend_dir().join("static/style.css")).expect("read css"),
            (1600, 1000),
            &facts_with(ui),
        )
        .expect("build shell")
    }

    /// A `find` payload as `app::find_wire::FindView::facts_value` produces it.
    fn find_ui(query: &str, meta: &str, notice: &str, state: &str, rows: &[(&str, &str)]) -> UiState {
        let mut facts = dom::FindFacts {
            query: query.to_string(),
            meta: meta.to_string(),
            notice: notice.to_string(),
            state: serde_json::from_value(serde_json::Value::String(state.to_string()))
                .expect("state must be one of nzk_ui's FindState variants"),
            ..Default::default()
        };
        for (title, tag) in rows {
            facts.rows.push(dom::FindRow {
                title: (*title).to_string(),
                subtitle: format!("{tag} subtitle"),
                tag: (*tag).to_string(),
                kind: "method".to_string(),
                node_id: format!("n:{title}"),
            });
        }
        UiState {
            drawers_open: vec!["findPanel".to_string()],
            find: Some(facts),
            ..Default::default()
        }
    }

    /// The headline content assertion: a real hit list reaches `#findResults` as
    /// the reference's own row markup, and `#findQuery`/`#findMeta` carry the
    /// core's text.
    #[test]
    fn find_content_reaches_the_real_dom() {
        let mut shell = shell_with(find_ui(
            "Socket",
            "2 hits",
            "",
            "hits",
            &[("App.Socket", "exact"), ("App.SocketPlug", "prefix")],
        ));

        // The panel is open (no `hidden`) and therefore a real box.
        let b = shell.box_of("#findPanel").expect("#findPanel must exist");
        assert!(
            b.w > 0.0 && b.h > 0.0,
            "an open find panel must have real geometry, got {}x{}",
            b.w,
            b.h
        );
        assert!(
            !shell
                .classes_of("#findPanel")
                .expect("findPanel classes")
                .contains(&"hidden".to_string()),
            "#findPanel must lose its shipped `hidden` class"
        );

        // The bar shows the typed query and the real hit count.
        assert_eq!(shell.text_of("#findQuery").as_deref(), Some("Socket"));
        assert_eq!(shell.text_of("#findMeta").as_deref(), Some("2 hits"));

        // `Hits` carries no notice, so the notice bar stays hidden — and its
        // text is empty rather than a stale sentence.
        assert!(
            shell
                .classes_of("#findNotice")
                .expect("findNotice classes")
                .contains(&"hidden".to_string()),
            "#findNotice must stay hidden when the core had nothing to say"
        );

        // Two real rows, in the core's order, each with the core's own tag.
        let rows = shell
            .child_elements_of("#findResults")
            .expect("findResults must exist");
        assert_eq!(rows.len(), 2, "one .frow per hit");

        // The whole list text carries both hits, and a selector matching nothing
        // is `None` rather than fabricated text.
        let all = shell.text_of("#findResults").expect("findResults text");
        assert!(all.contains("App.Socket"), "row titles must be in the DOM: {all:?}");
        assert!(
            all.contains("App.SocketPlug"),
            "every hit must be rendered: {all:?}"
        );
        assert!(shell.text_of("#no-such-element-anywhere").is_none());
    }

    /// The rows carry the core's `data-node`/`data-match`, which is what makes a
    /// row a real click target for a specific node.
    #[test]
    fn find_rows_carry_node_and_match_attributes() {
        let shell = shell_with(find_ui("Socket", "1 hits", "", "hits", &[("App.Socket", "exact")]));
        // The only `.frow` in the document is ours.
        assert_eq!(
            shell.attr_of(".frow", "data-node").ok().flatten().as_deref(),
            Some("n:App.Socket")
        );
        assert_eq!(
            shell.attr_of(".frow", "data-match").ok().flatten().as_deref(),
            Some("exact")
        );
        // `.fname` repeats the title as its own `title`, per `search.js:196`.
        assert_eq!(
            shell.attr_of(".fname", "title").ok().flatten().as_deref(),
            Some("App.Socket")
        );
    }

    /// `NoQuery` renders the core's INSTRUCTION, not an empty list, and the
    /// query slot falls back to the shipped word "Find" (`search.js:314`).
    #[test]
    fn find_noquery_renders_instruction_copy() {
        let notice = "Type a symbol name to search. Matches rank exact, then \
                      prefix, then substring, then subsequence.";
        let shell = shell_with(find_ui("", "", notice, "noquery", &[]));

        assert_eq!(shell.text_of("#findQuery").as_deref(), Some("Find"));
        // No query means no hit count at all — not "0 hits".
        assert_eq!(shell.text_of("#findMeta").as_deref(), Some(""));
        assert!(
            !shell
                .classes_of("#findNotice")
                .expect("findNotice classes")
                .contains(&"hidden".to_string()),
            "a real notice must un-hide #findNotice"
        );
        let body = shell.text_of("#findResults").expect("findResults exists");
        assert!(
            body.contains("Type a symbol name to search"),
            "the empty state must carry the core's own sentence, got {body:?}"
        );
    }

    /// `NoHits` is a DIFFERENT document from `NoQuery`: different notice, a real
    /// `no hits` count. This is the distinction the whole wire exists to keep.
    #[test]
    fn find_nohits_is_distinguishable_from_noquery() {
        let noquery = shell_with(find_ui(
            "",
            "",
            "Type a symbol name to search. Matches rank exact, then prefix, \
             then substring, then subsequence.",
            "noquery",
            &[],
        ));
        let nohits = shell_with(find_ui(
            "zzzz",
            "no hits",
            "No symbol matched this query.",
            "nohits",
            &[],
        ));

        assert_eq!(nohits.text_of("#findQuery").as_deref(), Some("zzzz"));
        assert_eq!(nohits.text_of("#findMeta").as_deref(), Some("no hits"));
        assert_ne!(
            noquery.text_of("#findResults"),
            nohits.text_of("#findResults"),
            "NoQuery and NoHits must not render identical bodies"
        );
        assert_ne!(
            noquery.text_of("#findNotice"),
            nohits.text_of("#findNotice"),
            "NoQuery and NoHits must not share a notice sentence"
        );
        // Both really did render something.
        assert!(nohits
            .text_of("#findResults")
            .unwrap()
            .contains("No symbol matched this query."));
    }

    /// A `find` fact cannot reach anything outside the Find panel's nodes.
    #[test]
    fn find_content_cannot_escape_the_panel() {
        let shell = shell_with(find_ui("Socket", "1 hits", "", "hits", &[("App.Socket", "exact")]));
        // The sidebar/status text is untouched by a find fact.
        let repo_chip = shell.text_of("#repoChip").unwrap_or_default();
        assert!(
            !repo_chip.contains("App.Socket"),
            "a find row must not leak into #repoChip: {repo_chip:?}"
        );
    }

    // ---- picker -----------------------------------------------------------

    /// A `picker` payload as `app::find_wire::PickerView::facts_of` produces it.
    fn picker_ui(
        note: &str,
        note_class: &str,
        selectable: bool,
        can_go_up: bool,
        state: &str,
        dirs: &[(&str, bool)],
    ) -> UiState {
        let mut facts = dom::PickerFacts {
            path: "/tmp/fid-corpus".to_string(),
            path_input: "/tmp/fid-corpus".to_string(),
            note: note.to_string(),
            note_class: note_class.to_string(),
            selectable,
            can_go_up,
            state: serde_json::from_value(serde_json::Value::String(state.to_string()))
                .expect("state must be one of nzk_ui's PickerState variants"),
            ..Default::default()
        };
        for (name, symlink) in dirs {
            facts.dirs.push(dom::PickerRow {
                name: (*name).to_string(),
                path: format!("/tmp/fid-corpus/{name}"),
                symlink: *symlink,
            });
        }
        UiState {
            modals_open: vec!["modal".to_string()],
            picker: Some(facts),
            ..Default::default()
        }
    }

    /// A real listing reaches `#modalBody` as the reference's `.pick` rows, with
    /// the `/` suffix, the symlink chip and the path in `data-nav`.
    #[test]
    fn picker_content_reaches_the_real_dom() {
        let mut shell = shell_with(picker_ui(
            "no .cs files in this subtree",
            "modal-note warn",
            true,
            true,
            "nocs",
            &[("src", false), ("link", true)],
        ));

        let b = shell.box_of("#modal").expect("#modal must exist");
        assert!(
            b.w > 0.0 && b.h > 0.0,
            "an open picker modal must have real geometry, got {}x{}",
            b.w,
            b.h
        );
        assert_eq!(
            shell.text_of("#modalPath").as_deref(),
            Some("/tmp/fid-corpus")
        );
        assert_eq!(
            shell.attr_of("#modalPathInput", "value").ok().flatten().as_deref(),
            Some("/tmp/fid-corpus")
        );

        // Two rows, each naming a real sub-directory with the reference's `/`.
        let body = shell.text_of("#modalBody").expect("#modalBody exists");
        assert!(body.contains("src/"), "row label must carry the `/`: {body:?}");
        assert!(body.contains("link/"), "every dir must render: {body:?}");
        assert_eq!(
            shell
                .child_elements_of("#modalBody")
                .expect("modalBody")
                .len(),
            2
        );
        // `data-nav` is the absolute path the row descends into.
        assert_eq!(
            shell.attr_of(".pick", "data-nav").ok().flatten().as_deref(),
            Some("/tmp/fid-corpus/src")
        );
        // The symlink row is flagged, not hidden.
        assert!(shell.attr_of(".plink", "title").is_ok());

        // The note carries the core's sentence and its class.
        assert_eq!(
            shell.text_of("#modalNote").as_deref(),
            Some("no .cs files in this subtree")
        );
        assert!(shell
            .classes_of("#modalNote")
            .expect("modalNote classes")
            .contains(&"warn".to_string()));
    }

    /// `#modalSelect` and `#modalUp` follow the payload: a selectable folder with
    /// a parent enables both (no `disabled` attribute at all).
    #[test]
    fn picker_enables_select_and_up_when_the_core_said_so() {
        let shell = shell_with(picker_ui(
            "indexable",
            "modal-note ok",
            true,
            true,
            "indexable",
            &[("src", false)],
        ));
        assert_eq!(
            shell.attr_of("#modalSelect", "disabled").ok().flatten(),
            None,
            "#modalSelect must be enabled for a selectable folder"
        );
        assert_eq!(
            shell.attr_of("#modalUp", "disabled").ok().flatten(),
            None,
            "#modalUp must be enabled when a parent exists"
        );
    }

    /// **The bug this wire exists to prevent.** A `NotFound` listing must not
    /// render as an empty folder: no "Empty folder.", an honest note, and a
    /// disabled Select.
    #[test]
    fn picker_notfound_never_looks_like_an_empty_folder() {
        let shell = shell_with(picker_ui(
            "path does not exist",
            "modal-note warn",
            false,
            false,
            "notfound",
            &[],
        ));

        let body = shell.text_of("#modalBody").expect("#modalBody exists");
        assert_eq!(
            body, "",
            "a failed listing must NOT render the empty-folder message, got {body:?}"
        );
        assert!(
            !body.contains("Empty folder"),
            "NotFound must never claim the folder is empty"
        );
        assert_eq!(
            shell.text_of("#modalNote").as_deref(),
            Some("path does not exist")
        );
        // Both controls are disabled for a path we could not read.
        assert_eq!(
            shell.attr_of("#modalSelect", "disabled").ok().flatten().as_deref(),
            Some("disabled"),
            "#modalSelect must be disabled for a non-selectable path"
        );
        assert_eq!(
            shell.attr_of("#modalUp", "disabled").ok().flatten().as_deref(),
            Some("disabled"),
            "#modalUp must be disabled when there is no parent"
        );
    }

    /// An `Empty` (but listed) folder is the honest opposite: the message IS
    /// rendered, and Select stays enabled because the folder really is usable.
    #[test]
    fn picker_empty_listed_folder_does_render_the_message() {
        let shell = shell_with(picker_ui(
            "folder is empty",
            "modal-note warn",
            true,
            true,
            "empty",
            &[],
        ));
        let body = shell.text_of("#modalBody").expect("#modalBody exists");
        assert!(
            body.contains("Empty folder."),
            "a listed empty folder must say so, got {body:?}"
        );
        assert_eq!(
            shell.attr_of("#modalSelect", "disabled").ok().flatten(),
            None,
            "an empty but readable folder keeps Select enabled"
        );
    }

    /// `Unlisted` (the serde default) must also refuse to render the empty
    /// message: nothing was read, so nothing may be claimed.
    #[test]
    fn picker_unlisted_state_claims_nothing() {
        let shell = shell_with(picker_ui("", "", false, false, "unlisted", &[]));
        assert_eq!(
            shell.text_of("#modalBody").expect("#modalBody exists"),
            "",
            "Unlisted must not render the empty-folder message"
        );
    }

    /// The two facts are independent: a find payload must not touch the modal,
    /// and a picker payload must not touch the Find panel.
    #[test]
    fn find_and_picker_facts_do_not_reach_each_other() {
        // find only: the modal body stays as shipped.
        let find_only = shell_with(find_ui("Socket", "1 hits", "", "hits", &[("App.Socket", "exact")]));
        assert_eq!(
            find_only.text_of("#modalBody").expect("#modalBody exists"),
            "",
            "a find fact must not populate the picker's body"
        );
        assert_eq!(find_only.text_of("#modalNote").as_deref(), Some(""));

        // picker only: the Find panel keeps its shipped (hidden) text.
        let picker_only = shell_with(picker_ui(
            "indexable",
            "modal-note ok",
            true,
            true,
            "indexable",
            &[("src", false)],
        ));
        assert!(
            picker_only
                .classes_of("#findPanel")
                .expect("findPanel classes")
                .contains(&"hidden".to_string()),
            "a picker fact must not open the Find panel"
        );
        assert!(!picker_only
            .text_of("#findResults")
            .unwrap_or_default()
            .contains("App.Socket"));
    }

    /// No `find`/`picker` facts at all leaves the shipped document alone — the
    /// property that keeps the default paint byte-identical.
    #[test]
    fn absent_find_and_picker_facts_leave_the_document_shipped() {
        let shell = shell_with(UiState::default());
        assert!(
            shell
                .classes_of("#findPanel")
                .expect("findPanel classes")
                .contains(&"hidden".to_string()),
            "#findPanel must stay hidden with no find fact"
        );
        assert_eq!(shell.text_of("#findResults").as_deref(), Some(""));
        assert_eq!(shell.text_of("#modalBody").as_deref(), Some(""));
        assert!(
            shell
                .classes_of("#findNotice")
                .expect("findNotice classes")
                .contains(&"hidden".to_string()),
            "#findNotice stays hidden with no find fact"
        );
    }

    // ---- Layout ▸ slider readouts (`settings` facts) ----------------------
    //
    // The host publishes live slider values at TOP-LEVEL key `settings`
    // (`app/src/nzk_ui.rs:985`, a sibling of `ui`) and omits the key entirely
    // while the sliders sit at their shipped defaults. Before this suite the key
    // was silently dropped by serde, so all five readouts kept the literals in
    // `index.html`. These tests assert the CONSUMED result on the resolved DOM:
    // the actual text of each `<output>`, not that a struct field exists.

    /// The shipped `index.html` literals for the five readouts, verbatim
    /// (index.html:289,294,299,304,309). Used to prove the no-op path leaves
    /// them alone — and that a supplied value really did overwrite one.
    const SHIPPED: [(&str, &str); 5] = [
        ("#spacingOut", "120"),
        ("#nodeSizeOut", "8"),
        ("#iterOut", "800"),
        ("#curveOut", "0.10"),
        ("#textSizeOut", "11"),
    ];

    /// Build a shell whose facts carry a **top-level** `settings` object that
    /// was parsed from JSON, not hand-built.
    ///
    /// Going through the wire is the point: it proves the field actually
    /// deserialises from the `settings` key the host writes
    /// (`app/src/nzk_ui.rs:985`), and that the camelCase keys inside it land on
    /// the right fields. A hand-built `SettingsFacts` would pass even if the
    /// field were nested wrongly or the keys misnamed — both of which fail
    /// silently as `None`/`Default` on the real path.
    ///
    /// `settings_json` is the value of the top-level `settings` key, or `None`
    /// to model the host omitting the key entirely.
    fn shell_with_settings_json(settings_json: Option<&str>) -> NzkUi {
        let mut facts = Facts::for_corpus("/tmp/fixture");
        facts.settings = match settings_json {
            None => None,
            Some("null") => {
                // The host also publishes an explicit `null` (app/src/pov.rs:248);
                // model that through the wire so the `Option` handling is proven.
                serde_json::from_str::<Option<dom::SettingsFacts>>("null")
                    .expect("a top-level null must deserialise to None")
            }
            Some(v) => Some(
                serde_json::from_str::<dom::SettingsFacts>(v)
                    .expect("settings JSON must deserialise into SettingsFacts"),
            ),
        };
        NzkUi::from_source_with_facts(
            &std::fs::read_to_string(frontend_dir().join("index.html")).expect("read index.html"),
            &frontend_dir().join("static/style.css"),
            &std::fs::read_to_string(frontend_dir().join("static/style.css")).expect("read css"),
            (1600, 1000),
            &facts,
        )
        .expect("build shell")
    }

    /// (a) THE NO-OP PROMISE. With no `settings` key at all — and with an
    /// explicit `null`, which the host also emits (`app/src/pov.rs:248`) — every
    /// readout keeps its shipped literal, so the default render is untouched.
    #[test]
    fn absent_or_null_settings_leave_every_readout_at_its_shipped_literal() {
        for settings in [None, Some("null")] {
            let shell = shell_with_settings_json(settings);
            for (sel, shipped) in SHIPPED {
                assert_eq!(
                    shell.text_of(sel).as_deref(),
                    Some(shipped),
                    "{sel} must keep its shipped literal {shipped:?} when settings is {settings:?}"
                );
            }
        }
    }

    /// (b) THE CONSUMED RESULT. A `settings` object published the way the host
    /// publishes it drives all five spans.
    ///
    /// The values deliberately differ from both the shipped literals AND from
    /// `slider_facts`' own defaults, so a span that silently fell back to either
    /// one cannot masquerade as a pass.
    #[test]
    fn settings_json_updates_every_readout_span() {
        let shell = shell_with_settings_json(Some(
            r#"{"spacing":"333","iterations":"777","curvature":"0.42",
                "nodeSize":"15","textSize":"21"}"#,
        ));
        for (sel, want) in [
            ("#spacingOut", "333"),
            ("#iterOut", "777"),
            ("#curveOut", "0.42"),
            ("#nodeSizeOut", "15"),
            ("#textSizeOut", "21"),
        ] {
            assert_eq!(
                shell.text_of(sel).as_deref(),
                Some(want),
                "{sel} must carry the published value, not its shipped literal \
                 and not slider_facts' default"
            );
        }
    }

    /// The camelCase wire keys are the ones that must specialise. `nodeSize` and
    /// `textSize` are the two that would silently stay at the shipped literal if
    /// `SettingsFacts` forgot `rename_all = "camelCase"` and looked for
    /// `node_size`/`text_size` instead.
    #[test]
    fn settings_camel_case_keys_are_the_ones_that_land() {
        // Only `nodeSize`/`textSize` supplied: those two move, the other three
        // keep their shipped literals because their fields default to "".
        let shell =
            shell_with_settings_json(Some(r#"{"nodeSize":"31","textSize":"19"}"#));
        assert_eq!(shell.text_of("#nodeSizeOut").as_deref(), Some("31"));
        assert_eq!(shell.text_of("#textSizeOut").as_deref(), Some("19"));
        for (sel, shipped) in [("#spacingOut", "120"), ("#iterOut", "800"), ("#curveOut", "0.10")] {
            assert_eq!(
                shell.text_of(sel).as_deref(),
                Some(shipped),
                "{sel} was not in the object and must keep its shipped literal"
            );
        }
    }

    /// A partial object updates only what it names: an empty string is skipped
    /// rather than written, so a host that publishes one slider cannot blank out
    /// the four it said nothing about.
    #[test]
    fn a_settings_object_that_names_one_slider_updates_only_that_span() {
        let shell = shell_with_settings_json(Some(r#"{"curvature":"0.99"}"#));
        assert_eq!(shell.text_of("#curveOut").as_deref(), Some("0.99"));
        for (sel, shipped) in [
            ("#spacingOut", "120"),
            ("#iterOut", "800"),
            ("#nodeSizeOut", "8"),
            ("#textSizeOut", "11"),
        ] {
            assert_eq!(
                shell.text_of(sel).as_deref(),
                Some(shipped),
                "{sel} must not be blanked by a settings object that omitted it"
            );
        }
    }

    // ==================================================== #pubCount / #pubList
    //
    // Everything below is measured on the REAL `index.html`: the same file the
    // browser renders, with the same `#pubSection` / `#pubList` subtree. The
    // assertions are on the RESOLVED document (text content, attribute
    // presence, child elements), never on `Facts` itself — the whole point of
    // this section is the gap between "the key was published" and "the span
    // changed", which is exactly where a half-wired control hides.

    /// Build a shell from the real front end with a `pub_defs` object.
    fn shell_with_pub_defs_json(pub_defs_json: Option<&str>) -> NzkUi {
        let mut facts = Facts::for_corpus("/tmp/fixture");
        facts.pub_defs = match pub_defs_json {
            None => None,
            Some("null") => serde_json::from_str::<Option<dom::PubDefsFacts>>("null")
                .expect("a top-level null must deserialise to None"),
            Some(v) => Some(
                serde_json::from_str::<dom::PubDefsFacts>(v)
                    .expect("pub_defs JSON must deserialise into PubDefsFacts"),
            ),
        };
        NzkUi::from_source_with_facts(
            &std::fs::read_to_string(frontend_dir().join("index.html")).expect("read index.html"),
            &frontend_dir().join("static/style.css"),
            &std::fs::read_to_string(frontend_dir().join("static/style.css")).expect("read css"),
            (1600, 1000),
            &facts,
        )
        .expect("build shell")
    }

    /// (a) THE INERT PATH. With no `pub_defs` key — and with an explicit `null`
    /// — `#pubCount` keeps the empty span `index.html` ships and `#pubList`
    /// keeps its `<div class="list-empty">open to load</div>` placeholder.
    ///
    /// This is the guard that stops the default render moving, and it is
    /// asserted as the resolved text, because an empty `<span>` and a span
    /// reading `"0"` are the same thing to every check that only looks at
    /// whether a key was sent.
    #[test]
    fn no_pub_defs_key_leaves_the_count_span_and_the_list_shipped() {
        for pub_defs in [None, Some("null")] {
            let shell = shell_with_pub_defs_json(pub_defs);
            assert_eq!(
                shell.text_of("#pubCount").as_deref(),
                Some(""),
                "with pub_defs={pub_defs:?} the count span must stay empty, \
                 not read a fabricated 0"
            );
            assert_eq!(
                shell.text_of("#pubList").as_deref(),
                Some("open to load"),
                "with pub_defs={pub_defs:?} the list must keep index.html's \
                 placeholder"
            );
        }
    }

    /// (b) THE CONSUMED COUNT. The engine's `total` reaches the resolved
    /// `#pubCount` span.
    ///
    /// `1234` is chosen deliberately: it is larger than the 300-row render cap
    /// and carries a thousands separator under `fmtNum` (`util.js`
    /// `toLocaleString('en-US')`), so this fails if the span is fed `list.len()`
    /// or rendered as a bare integer.
    #[test]
    fn the_engine_total_reaches_the_resolved_pubcount_span() {
        let shell = shell_with_pub_defs_json(Some(r#"{"total":1234}"#));
        assert_eq!(
            shell.text_of("#pubCount").as_deref(),
            Some("1,234"),
            "#pubCount must carry the engine's total, comma-grouped like fmtNum"
        );
    }

    /// The count is the engine's `total`, NOT the number of rows returned.
    ///
    /// A page of 2 rows with `total: 900` must read `900`: `Index::definitions`
    /// computes `total` before truncating to `limit`
    /// (`core/src/index.rs:3425-3428`), and `filters.js:493` stores exactly
    /// that. Reading `list.len()` here would understate every corpus past the
    /// limit — the specific bug this test exists to catch.
    #[test]
    fn pubcount_is_the_engine_total_not_the_returned_row_count() {
        let shell = shell_with_pub_defs_json(Some(
            r#"{"total":900,"list":[
                {"id":"a","full":"A.m","label":"m","kind":"method"},
                {"id":"b","full":"B.n","label":"n","kind":"method"}
            ]}"#,
        ));
        assert_eq!(
            shell.text_of("#pubCount").as_deref(),
            Some("900"),
            "the readout is a fact about the corpus, not about the size of the page"
        );
        assert_eq!(
            shell.child_elements_of("#pubList").map(|c| c.len()),
            Some(2),
            "…while the box shows exactly the rows it was given"
        );
    }

    /// (c) THE CONSUMED LIST. Real rows reach `#pubList`, each with the three
    /// spans `renderListRows` builds and the `title` it sets.
    #[test]
    fn real_rows_reach_the_resolved_publist() {
        let shell = shell_with_pub_defs_json(Some(
            r#"{"total":2,"list":[
                {"id":"M:Foo.Bar","full":"Foo.Bar","label":"Bar","kind":"method"},
                {"id":"T:Foo","full":"Foo","label":"Foo","kind":"class"}
            ]}"#,
        ));

        // The shipped placeholder must be GONE — `renderListRows` clears the
        // container (`filters.js:449`). A list that still says "open to load"
        // while showing rows contradicts itself.
        //
        // `textContent` concatenates with no separator (the three spans are
        // inline elements with no whitespace text nodes between them), so the
        // expected string is the raw concatenation — which is what the browser
        // would report too.
        assert_eq!(
            shell.text_of("#pubList").as_deref(),
            Some("BarmethodFooclass"),
            "the rows must replace the placeholder, in order, each as label+kind"
        );
        assert_eq!(
            shell.child_elements_of("#pubList").map(|c| c.len()),
            Some(2),
            "two published rows must be two button children"
        );

        // The `title` comes from `n.full || n.id` (`filters.js:454`).
        assert_eq!(
            shell.attr_of("#pubList .lrow", "title"),
            Ok(Some("Foo.Bar".to_string())),
            "a row's title is its full name"
        );
        // `.l-kind` and the `.dot` are real child elements.
        assert_eq!(
            shell.classes_of("#pubList .lrow"),
            Some(vec!["lrow".to_string()]),
            "the row button carries .lrow"
        );
        assert_eq!(
            shell.classes_of("#pubList .l-label").is_some(),
            true,
            "each row has a .l-label span"
        );
        assert_eq!(
            shell.classes_of("#pubList .l-kind").is_some(),
            true,
            "each row has a .l-kind span"
        );
        assert_eq!(
            shell.classes_of("#pubList .dot").is_some(),
            true,
            "each row has a .dot span"
        );
    }

    /// An EMPTY list is a real, honest state: the box gets a `.list-empty` row
    /// reading `no matches` (`filters.js:513`), which is distinguishable from
    /// "never loaded".
    #[test]
    fn an_empty_published_list_renders_the_honest_empty_row() {
        let shell = shell_with_pub_defs_json(Some(r#"{"total":0,"list":[]}"#));
        assert_eq!(
            shell.text_of("#pubList").as_deref(),
            Some("no matches"),
            "an empty list must render the reference's empty line, not the \
             shipped 'open to load' placeholder"
        );
        assert_eq!(
            shell.text_of("#pubCount").as_deref(),
            Some("0"),
            "a real 0 from the engine IS shown — total:0 is a measured fact, \
             unlike an absent key"
        );
    }

    /// `list: null` (not loaded) and `list: []` (loaded, empty) must differ.
    /// Collapsing them would make "we never asked" indistinguishable from "the
    /// engine found nothing".
    #[test]
    fn an_unloaded_list_is_distinguishable_from_a_loaded_empty_one() {
        let unloaded = shell_with_pub_defs_json(Some(r#"{"total":7}"#));
        assert_eq!(
            unloaded.text_of("#pubList").as_deref(),
            Some("open to load"),
            "no list key at all means 'not loaded yet'"
        );
        assert_eq!(
            unloaded.text_of("#pubCount").as_deref(),
            Some("7"),
            "…while the count, which WAS supplied, still lands"
        );

        let loaded_empty = shell_with_pub_defs_json(Some(r#"{"total":7,"list":[]}"#));
        assert_eq!(
            loaded_empty.text_of("#pubList").as_deref(),
            Some("no matches"),
            "an empty list that WAS loaded must say so"
        );
    }

    // ============================================== the filter checkbox pair
    //
    // `#groupChildren` and `#hideChildren` are two boxes over ONE flag
    // (`core.js:270`), and they always disagree (`shell.js:603-604`, `:645-647`).
    // Both directions are asserted on BOTH resolved attributes: a test that only
    // checked one box would pass for a host that wrote the flag to both, which
    // is a document the reference cannot produce.

    fn shell_with_group_children(flag: Option<bool>) -> NzkUi {
        let mut facts = Facts::for_corpus("/tmp/fixture");
        facts.ui = match flag {
            None => None,
            Some(on) => Some(UiState {
                group_children: on,
                ..Default::default()
            }),
        };
        NzkUi::from_source_with_facts(
            &std::fs::read_to_string(frontend_dir().join("index.html")).expect("read index.html"),
            &frontend_dir().join("static/style.css"),
            &std::fs::read_to_string(frontend_dir().join("static/style.css")).expect("read css"),
            (1600, 1000),
            &facts,
        )
        .expect("build shell")
    }

    /// Read a checkbox's `checked` state as a bool. `Ok(None)` (no attribute) is
    /// false; anything else present is true.
    fn checked(shell: &NzkUi, sel: &str) -> bool {
        shell
            .attr_of(sel, "checked")
            .unwrap_or_else(|_| panic!("{sel} must exist in the real index.html"))
            .is_some()
    }

    /// The shipped document, untouched: `#groupChildren` checked (`:253`) and
    /// `#hideChildren` unchecked (`:274`). With no `ui` at all nothing is
    /// written, and this is the state every existing `@frame` test relies on.
    #[test]
    fn the_shipped_checkbox_pair_needs_no_ui_facts() {
        let shell = shell_with_group_children(None);
        assert!(checked(&shell, "#groupChildren"), "index.html ships it checked");
        assert!(!checked(&shell, "#hideChildren"), "index.html ships it unchecked");
    }

    /// (d) THE INVERSION, IN BOTH DIRECTIONS, ON BOTH RESOLVED ATTRIBUTES.
    ///
    /// `flag == true` ⇒ `#groupChildren` checked, `#hideChildren` not.
    /// `flag == false` ⇒ the exact opposite, on both.
    ///
    /// The two boxes must never agree; that invariant is asserted for each
    /// state rather than assumed, because "the flag reached one box" is the
    /// half-wired failure mode this test is built to catch.
    #[test]
    fn the_group_children_flag_inverts_across_both_resolved_checkboxes() {
        let grouped = shell_with_group_children(Some(true));
        assert!(checked(&grouped, "#groupChildren"), "flag=true ⇒ #groupChildren on");
        assert!(!checked(&grouped, "#hideChildren"), "flag=true ⇒ #hideChildren off");

        let ungrouped = shell_with_group_children(Some(false));
        assert!(
            !checked(&ungrouped, "#groupChildren"),
            "flag=false must CLEAR #groupChildren's checked attribute"
        );
        assert!(
            checked(&ungrouped, "#hideChildren"),
            "flag=false must SET #hideChildren's checked attribute"
        );

        // The pair is genuinely inverted, not just both-written.
        for (label, shell) in [("true", &grouped), ("false", &ungrouped)] {
            assert_ne!(
                checked(shell, "#groupChildren"),
                checked(shell, "#hideChildren"),
                "with group_children={label} the two boxes must disagree"
            );
        }
    }

    /// A missing `group_children` key inside a present `ui` object defaults to
    /// `false`, which INVERTS the shipped pair. That is the honest reading of an
    /// explicit `ui` object that did not mention the flag only insofar as the
    /// host is asserting it owns the view state — and it is worth pinning so the
    /// behaviour is a decision rather than an accident.
    #[test]
    fn an_explicit_ui_without_the_flag_writes_the_false_state() {
        let shell = shell_with_group_children(Some(false));
        assert_ne!(
            shell.attr_of("#groupChildren", "checked").unwrap(),
            Some(String::new()),
            "false means the attribute is removed, not set to a falsy string"
        );
    }

    // ====================================================== `#groupEdges`
    //
    // The *edges* counterpart of `#groupChildren`, and a genuinely distinct
    // control: `index.html:255` ships it `checked` **and `hidden`**, driven by
    // the separate `state.filters.groupEdges` flag (`core.js:271`,
    // `shell.js:609-612`). It has NO inversion — `sync.js:165`/`app.js:299`
    // assign the flag straight to `.checked` — so unlike the pair above the box
    // shows the flag itself. These tests exist to catch the two failure modes
    // that matter: the key not landing at all (the `rename_all` trap, which is
    // why one case below goes through raw JSON), and this box being wired to
    // the *children* flag by aliasing.

    /// Build a shell whose `ui` is the given raw JSON object — the wire form,
    /// not a Rust struct literal. This is what makes the camelCase assertion
    /// non-vacuous: a struct literal would bypass the deserializer entirely and
    /// pass even if serde were looking for `group_edges`.
    fn shell_with_ui_json(ui: &str) -> NzkUi {
        let mut facts = Facts::for_corpus("/tmp/fixture");
        facts.ui = Some(
            serde_json::from_str(ui).unwrap_or_else(|e| panic!("ui JSON must parse: {e}")),
        );
        NzkUi::from_source_with_facts(
            &std::fs::read_to_string(frontend_dir().join("index.html")).expect("read index.html"),
            &frontend_dir().join("static/style.css"),
            &std::fs::read_to_string(frontend_dir().join("static/style.css")).expect("read css"),
            (1600, 1000),
            &facts,
        )
        .expect("build shell")
    }

    fn shell_with_group_edges(flag: bool) -> NzkUi {
        shell_with_ui_json(&format!(r#"{{"groupEdges":{flag}}}"#))
    }

    /// The shipped document, untouched: nothing writes `#groupEdges` when there
    /// is no `ui` at all, so it keeps `index.html:255`'s `checked` — the default
    /// is ON, which is the single most important fact about this control.
    #[test]
    fn the_shipped_group_edges_box_is_checked_and_default_on() {
        let shipped = shell_with_ui_json("{}");
        assert!(
            checked(&shipped, "#groupEdges"),
            "#groupEdges ships checked (index.html:255) and is hidden, so with no \
             ui facts it must still be on — the default is true"
        );
        // Same box, no `ui` at all: the other way the default is reached.
        let mut facts = Facts::for_corpus("/tmp/fixture");
        facts.ui = None;
        let bare = NzkUi::from_source_with_facts(
            &std::fs::read_to_string(frontend_dir().join("index.html")).expect("read index.html"),
            &frontend_dir().join("static/style.css"),
            &std::fs::read_to_string(frontend_dir().join("static/style.css")).expect("read css"),
            (1600, 1000),
            &facts,
        )
        .expect("build shell");
        assert!(
            checked(&bare, "#groupEdges"),
            "with no ui object at all the box keeps its shipped checked state"
        );
    }

    /// (a) THE CAMEL_CASE TRAP, THROUGH THE REAL DESERIALIZER. The wire key is
    /// `groupEdges`; if the field were not renamed, this object would leave the
    /// field at its default and the `false` case below would silently pass as
    /// "already false". So both the parse and the *effect* are asserted.
    #[test]
    fn the_group_edges_wire_key_is_camel_case_and_the_effect_lands() {
        let ui: UiState = serde_json::from_str(r#"{"groupEdges":false}"#)
            .expect("the camelCase key must parse");
        assert!(
            !ui.group_edges,
            "the key `groupEdges` must reach `UiState::group_edges`; if this reads \
             back `true` (the serde default) the rename did not land and the whole \
             control is silently dead"
        );

        // And the snake_case spelling must NOT be the wire key: if it were, the
        // host's real `groupEdges` payload would be ignored.
        let snake: UiState = serde_json::from_str(r#"{"group_edges":false}"#)
            .expect("an unknown key is tolerated, not an error");
        assert!(
            snake.group_edges,
            "`group_edges` is NOT the wire key — a payload using it must fall \
             through to the default `true`, proving the rename is load-bearing"
        );
    }

    /// (b) THE MIRROR, BOTH DIRECTIONS, AND (c) NO INVERSION.
    ///
    /// `flag == true` ⇒ checked; `flag == false` ⇒ **cleared**. And unlike
    /// `#hideChildren`, `#groupEdges` must follow the flag directly rather than
    /// invert it — so this box and `#groupChildren` agree when the two flags
    /// agree, and the box is not secretly reading the children flag.
    #[test]
    fn the_group_edges_box_mirrors_the_flag_without_inverting_it() {
        let on = shell_with_group_edges(true);
        assert!(checked(&on, "#groupEdges"), "flag=true ⇒ #groupEdges checked");

        let off = shell_with_group_edges(false);
        assert!(
            !checked(&off, "#groupEdges"),
            "flag=false must CLEAR #groupEdges's checked attribute — this is the \
             assertion that fails if the mirror is not wired at all"
        );
        assert_ne!(
            checked(&on, "#groupEdges"),
            checked(&off, "#groupEdges"),
            "the two states must be distinguishable, or the flag changes nothing"
        );

        // No inversion, and no aliasing. `#groupEdges` must show `groupEdges`
        // itself, never the *children* flag and never its negation. The check is
        // done on an explicit `group_children` value so this cannot be confused
        // with the separate, already-pinned behaviour that an omitted
        // `group_children` inside a present `ui` defaults to `false`.
        //
        // Note the two spellings are genuinely different and both real: the
        // *children* key is snake_case (`group_children`, a plain `UiState`
        // field), while the *edges* key is camelCase (`groupEdges`, renamed
        // per-field). This object therefore exercises both key shapes at once.
        let no_alias = shell_with_ui_json(r#"{"group_children":true,"groupEdges":false}"#);
        assert!(
            checked(&no_alias, "#groupChildren"),
            "the children flag was set true explicitly and must be honoured"
        );
        assert!(
            !checked(&no_alias, "#groupEdges"),
            "#groupEdges must show its OWN flag, not #groupChildren's — reading \
             true here would mean the two controls were aliased"
        );
    }

    /// The two flags are independent in the other direction too: turning the
    /// *children* flag off must leave `#groupEdges` alone, and `#groupEdges`
    /// must not be reachable through `#hideChildren`'s inversion.
    #[test]
    fn the_two_group_flags_are_independent_controls() {
        // group_children=false via the wire (snake_case, its real spelling),
        // groupEdges omitted by the host.
        let children_off = shell_with_ui_json(r#"{"group_children":false}"#);
        assert!(
            !checked(&children_off, "#groupChildren"),
            "the children flag itself must land"
        );
        assert!(
            checked(&children_off, "#groupEdges"),
            "#groupEdges defaults to true and must be unaffected by the children flag"
        );

        // Both flags in one object: each box follows its own flag, and
        // `#hideChildren` inverts only the children flag.
        let both = shell_with_ui_json(r#"{"group_children":false,"groupEdges":false}"#);
        assert!(!checked(&both, "#groupChildren"), "children flag false");
        assert!(checked(&both, "#hideChildren"), "the inverse box inverts it");
        assert!(!checked(&both, "#groupEdges"), "edges flag false, on its own flag");
    }
    /// The endgame proof at layout level: a [`NzkUi`] built from the Rust
    /// skeleton must lay out to **exactly** the same border boxes as one parsed
    /// from the real `index.html`, for every geometry probe selector.
    ///
    /// Structural parity is proven in `skeleton`; this pins that the constructed
    /// document computes the same geometry too, so the runtime can stop reading
    /// `index.html` without moving a pixel.
    #[test]
    fn skeleton_shell_boxes_equal_reference_parse() {
        let dir = frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read css");
        let raw =
            std::fs::read_to_string(dir.join("index.html")).expect("read reference index.html");
        let facts = Facts::for_corpus("/tmp/fixture");
        let parsed = NzkUi::from_source_with_facts(&raw, &css_path, &css, (1600, 1000), &facts)
            .expect("reference-parsed shell");
        let built = NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
            .expect("skeleton shell");
        let parsed_boxes = parsed.boxes();
        let built_boxes = built.boxes();
        assert_eq!(parsed_boxes.len(), built_boxes.len());
        let close = |a: f32, b: f32| (a - b).abs() <= 0.01;
        let mut diffs = Vec::new();
        for (i, ((sel, want), (built_sel, got))) in
            parsed_boxes.iter().zip(built_boxes.iter()).enumerate()
        {
            assert_eq!(sel, built_sel, "probe row {i} names differ");
            match (want, got) {
                (None, None) => {}
                (Some(w), Some(g))
                    if close(w.x, g.x)
                        && close(w.y, g.y)
                        && close(w.w, g.w)
                        && close(w.h, g.h) => {}
                (want, got) => diffs.push(format!(
                    "{i:02} {sel}: parsed {want:?} vs skeleton {got:?}"
                )),
            }
        }
        assert!(
            diffs.is_empty(),
            "{} of {} probe boxes moved between the parsed and skeleton documents:\n{}",
            diffs.len(),
            parsed_boxes.len(),
            diffs.join("\n")
        );
    }

}

#[cfg(test)]
mod interaction_surface_tests {
    use super::*;
    use dom::{ChainFacts, Facts, LoadingFacts, PubDefsFacts, PubRow, UiState};

    fn shell_with(ui: UiState) -> NzkUi {
        let mut facts = Facts::for_corpus("/tmp/fixture");
        facts.ui = Some(ui);
        NzkUi::from_source_with_facts(
            &std::fs::read_to_string(frontend_dir().join("index.html")).expect("read index.html"),
            &frontend_dir().join("static/style.css"),
            &std::fs::read_to_string(frontend_dir().join("static/style.css")).expect("read css"),
            (1600, 1000),
            &facts,
        )
        .expect("build shell")
    }

    /// `#stopIndexBtn` ships `style="display:none"` inside a hidden overlay. A
    /// live build must clear both, or Stop is painted-but-unreachable.
    #[test]
    fn loading_overlay_makes_stop_a_real_control() {
        let shell = shell_with(UiState {
            loading: Some(LoadingFacts {
                text: "Indexing /corpus …".to_string(),
                stop: true,
            }),
            ..Default::default()
        });
        let overlay = shell.box_of("#loading").expect("#loading exists");
        assert!(overlay.w > 0.0 && overlay.h > 0.0, "overlay must be laid out");
        let stop = shell.box_of("#stopIndexBtn").expect("#stopIndexBtn exists");
        assert!(
            stop.w > 0.0 && stop.h > 0.0,
            "Stop must clear its inline display:none and get a real box, got {}x{}",
            stop.w,
            stop.h
        );
        assert_eq!(
            shell.text_of("#loadingText").as_deref(),
            Some("Indexing /corpus …")
        );
    }

    /// The resolve-chain tree must paint the engine's rows, carry a clickable
    /// `data-key`, and set the section count. A static "Select a node" box would
    /// pass a weaker assertion but is the dead-control shape this guards.
    #[test]
    fn chain_rows_paint_with_data_keys_and_count() {
        let shell = shell_with(UiState {
            // The Resolve-chain section ships collapsed; open it so the rows are
            // laid out and a box (not just text) can be asserted.
            collapsed: Some(Vec::new()),
            chain: Some(ChainFacts {
                data: serde_json::json!({
                    "ok": true,
                    "flat": [{"depth": 1, "node": {"id": "a", "label": "Alpha"}}],
                    "truncated": false,
                    "tree": {
                        "node": {"id": "root", "label": "Root"},
                        "edge": null,
                        "line": null,
                        "cycle": false,
                        "children": [{
                            "node": {"id": "a", "label": "Alpha"},
                            "edge": "calls",
                            "line": 12,
                            "cycle": false,
                            "children": []
                        }]
                    }
                }),
                flat: false,
                depth: 3,
                dir: "forward".to_string(),
                collapsed: Vec::new(),
            }),
            ..Default::default()
        });
        let text = shell.text_of("#chainTree").expect("#chainTree exists");
        assert!(text.contains("Root") && text.contains("Alpha"), "rendered: {text}");
        let row = shell.box_of(".hop").expect("a .hop row must be laid out");
        assert!(row.w > 0.0 && row.h > 0.0);
        assert_eq!(
            shell.attr_of(".hop", "data-key").expect("query data-key"),
            Some("root".to_string()),
            "a row must name the node it selects"
        );
        assert_eq!(shell.text_of("#chainCount").as_deref(), Some("1 hops"));
    }

    /// `#strList` must render the engine's string hits with data keys, not the
    /// shipped "type to search" placeholder.
    #[test]
    fn strings_rows_paint_with_data_keys() {
        let mut facts = Facts::for_corpus("/tmp/fixture");
        facts.str_defs = Some(PubDefsFacts {
            total: Some(1),
            list: Some(vec![PubRow {
                id: "lit:1".to_string(),
                full: "NZK.Sps".to_string(),
                label: "Sps".to_string(),
                kind: "string".to_string(),
            }]),
        });
        let shell = NzkUi::from_source_with_facts(
            &std::fs::read_to_string(frontend_dir().join("index.html")).expect("read index.html"),
            &frontend_dir().join("static/style.css"),
            &std::fs::read_to_string(frontend_dir().join("static/style.css")).expect("read css"),
            (1600, 1000),
            &facts,
        )
        .expect("build shell");
        assert_eq!(shell.text_of("#strCount").as_deref(), Some("1"));
        assert!(shell.text_of("#strList").unwrap_or_default().contains("Sps"));
        assert_eq!(
            shell.attr_of("#strList .lrow", "data-key").expect("query row"),
            Some("lit:1".to_string())
        );
    }
}

#[cfg(test)]
mod source_head_tests {
    use super::*;
    use dom::{Facts, SourceFacts, UiState};

    #[test]
    fn source_head_paints_live_title_note_and_button_states() {
        let mut facts = Facts::for_corpus("/tmp/fixture");
        facts.ui = Some(UiState {
            pane_open: true,
            source: Some(SourceFacts {
                title: "NZK.B.compare".to_string(),
                note: "no declaration file".to_string(),
                has_doc: true,
                prev: false,
                next: true,
                can_ref: true,
                mode: "monokai".to_string(),
            }),
            ..Default::default()
        });
        let shell = NzkUi::from_source_with_facts(
            &std::fs::read_to_string(frontend_dir().join("index.html")).expect("read index.html"),
            &frontend_dir().join("static/style.css"),
            &std::fs::read_to_string(frontend_dir().join("static/style.css")).expect("read css"),
            (1600, 1000),
            &facts,
        )
        .expect("build shell");
        assert_eq!(shell.text_of("#sourceTitle").as_deref(), Some("NZK.B.compare"));
        assert_eq!(shell.text_of("#sourceNote").as_deref(), Some("no declaration file"));
        assert_eq!(shell.text_of("#srcModeBtn").as_deref(), Some("monokai"));
        assert_eq!(
            shell.attr_of("#srcPrevBtn", "disabled").expect("query disabled"),
            Some(String::new()),
            "no history -> Prev is disabled"
        );
        assert_eq!(
            shell.attr_of("#srcNextBtn", "disabled").expect("query disabled"),
            None,
            "available history -> Next is enabled"
        );
        assert_eq!(
            shell.attr_of("#srcCopyBtn", "disabled").expect("query disabled"),
            None
        );
    }
}
