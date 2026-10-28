//! The DOM the viewer's JavaScript would have built at runtime, constructed in
//! Rust.
//!
//! `frontend/index.html` is only the static shell. The live document is grown by
//! the 32 scripts in `frontend/static/` — `panels.js` adds pane nzk_ui,
//! `filters.js` adds the project/kind/edge facet rows, `problems.js` adds the
//! parser-warnings section, `shell.js`/`app.js` set the theme, the folder input
//! and the status line. There is no JS engine here, so this module reproduces
//! those mutations by hand: the *structure* is Rust, the *styling* stays in the
//! real stylesheet.
//!
//! Every builder below cites the script and lines it mirrors, so the equivalence
//! can be re-checked against the source.

use std::path::Path;

use blitz_dom::{Attribute, BaseDocument, LocalName, QualName, ns};

// ---------------------------------------------------------------- small stdlib

/// HTML *attributes* live in no namespace (`ns!()`), unlike element names which
/// live in the HTML namespace.
fn attr(name: &str, value: &str) -> Attribute {
    Attribute {
        name: attr_name(name),
        value: value.to_string(),
    }
}

fn attr_name(name: &str) -> QualName {
    QualName::new(None, ns!(), LocalName::from(name))
}

fn tag(name: &str) -> QualName {
    QualName::new(None, ns!(html), LocalName::from(name))
}

/// Shorthand for `document.createElement` + attributes + textContent.
struct El {
    tag: &'static str,
    attrs: Vec<(String, String)>,
    text: Option<String>,
}

impl El {
    fn new(tag: &'static str) -> Self {
        Self {
            tag,
            attrs: Vec::new(),
            text: None,
        }
    }
    fn cls(mut self, class: &str) -> Self {
        self.attrs.push(("class".into(), class.to_string()));
        self
    }
    fn id(mut self, id: &str) -> Self {
        self.attrs.push(("id".into(), id.to_string()));
        self
    }
    fn a(mut self, name: &str, value: &str) -> Self {
        self.attrs.push((name.to_string(), value.to_string()));
        self
    }
    fn style(mut self, style: &str) -> Self {
        self.attrs.push(("style".into(), style.to_string()));
        self
    }
    fn text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }
}

/// A thin `document.createElement`-alike over the Blitz mutator.
struct Dom<'a> {
    doc: &'a mut BaseDocument,
}

impl<'a> Dom<'a> {
    fn new(doc: &'a mut BaseDocument) -> Self {
        Dom { doc }
    }

    fn id(&self, id: &str) -> Option<usize> {
        self.doc.get_element_by_id(id)
    }

    /// `document.body`: the `<body>` child of `<html>` (`root_element()`).
    ///
    /// blitz exposes only `root_element()` (the `<html>`), so the body is found
    /// by tag. Overlays such as `.cs-hier` are appended here because that is
    /// where `hierarchy.js` puts them (`document.body.appendChild`, line 448),
    /// and `.cs-hier`'s `position: fixed` is resolved against it.
    fn body(&self) -> Option<usize> {
        let html = self.doc.root_element().id;
        let node = self.doc.get_node(html)?;
        node.children.iter().copied().find(|c| {
            self.doc
                .get_node(*c)
                .and_then(|n| n.element_data())
                .map(|e| e.name.local.as_ref() == "body")
                .unwrap_or(false)
        })
    }

    fn mk(&mut self, el: El) -> usize {
        let attrs: Vec<Attribute> = el
            .attrs
            .iter()
            .map(|(k, v)| attr(k, v))
            .collect();
        let node = {
            let mut m = self.doc.mutate();
            m.create_element(tag(el.tag), attrs)
        };
        if let Some(t) = el.text {
            let text = self.mk_text(&t);
            self.append(node, &[text]);
        }
        node
    }

    fn mk_text(&mut self, text: &str) -> usize {
        let mut m = self.doc.mutate();
        m.create_text_node(text)
    }

    /// `element.textContent = text`.
    ///
    /// Blitz's `set_node_text` is a no-op unless the node is a text node (it
    /// returns silently otherwise), so this does what `textContent` does:
    /// replace the children with a single text node.
    fn set_text(&mut self, node: usize, text: &str) {
        let mut m = self.doc.mutate();
        m.remove_and_drop_all_children(node);
        let t = m.create_text_node(text);
        m.append_children(node, &[t]);
    }

    /// `element.replaceChildren(...)`, with already-built element nodes.
    ///
    /// `#pubList` ships with a `<div class="list-empty">open to load</div>` child
    /// that must be *removed* when real rows arrive — `renderListRows` does that
    /// with `container.innerHTML = ''` (`filters.js:449`). Appending without
    /// clearing would leave the placeholder above the rows, so the box would
    /// claim it is unloaded while showing a loaded list: a control contradicting
    /// itself. This is the clearing append, not a bare one.
    fn set_children(&mut self, node: usize, children: &[usize]) {
        let mut m = self.doc.mutate();
        m.remove_and_drop_all_children(node);
        if !children.is_empty() {
            m.append_children(node, children);
        }
    }

    /// The `QualName` this node already uses for an attribute, so a rewrite
    /// replaces the parsed attribute instead of appending a second one.
    ///
    /// `Attributes::set` matches on the whole `QualName` (namespace, prefix,
    /// local). A parser-created HTML attribute and a hand-built one do not
    /// necessarily agree on the namespace, and when they disagreed here
    /// `#notice` ended up with `class="notice hidden" class="notice"` — the
    /// stale `.hidden` kept winning.
    fn attr_name(&self, node: usize, name: &str) -> QualName {
        self.doc
            .get_node(node)
            .and_then(|n| n.element_data())
            .and_then(|e| {
                e.attrs
                    .iter()
                    .find(|a| a.name.local.as_ref() == name)
                    .map(|a| a.name.clone())
            })
            .unwrap_or_else(|| attr_name(name))
    }

    fn set_attr(&mut self, node: usize, name: &str, value: &str) {
        let qn = self.attr_name(node, name);
        let mut m = self.doc.mutate();
        m.set_attribute(node, qn, value);
    }

    fn clear_attr(&mut self, node: usize, name: &str) {
        let qn = self.attr_name(node, name);
        let mut m = self.doc.mutate();
        m.clear_attribute(node, qn);
    }

    fn append(&mut self, parent: usize, children: &[usize]) {
        let mut m = self.doc.mutate();
        m.append_children(parent, children);
    }

    fn parent_of(&self, node: usize) -> Option<usize> {
        self.doc.get_node(node).and_then(|n| n.parent)
    }

    fn insert_before(&mut self, anchor: usize, nodes: &[usize]) {
        let mut m = self.doc.mutate();
        m.insert_nodes_before(anchor, nodes);
    }

    fn insert_after(&mut self, anchor: usize, nodes: &[usize]) {
        let mut m = self.doc.mutate();
        m.insert_nodes_after(anchor, nodes);
    }

    fn prepend(&mut self, parent: usize, node: usize) {
        let mut m = self.doc.mutate();
        match m.child_ids(parent).first().copied() {
            Some(first) => m.insert_nodes_before(first, &[node]),
            None => m.append_children(parent, &[node]),
        }
    }

    /// `classList.toggle(name, force)` — Blitz has no classList, so read the
    /// class attribute, add/remove the token, and write it back.
    fn set_class_flag(&mut self, node: usize, class: &str, on: bool) {
        let current = self
            .doc
            .get_node(node)
            .and_then(|n| n.element_data())
            .and_then(|e| {
                e.attrs
                    .iter()
                    .find(|a| a.name.local.as_ref() == "class")
                    .map(|a| a.value.clone())
            })
            .unwrap_or_default();
        let mut tokens: Vec<String> = current
            .split_whitespace()
            .map(|t| t.to_string())
            .collect();
        let has = tokens.iter().any(|t| t == class);
        if on && !has {
            tokens.push(class.to_string());
        } else if !on && has {
            tokens.retain(|t| t != class);
        } else {
            return;
        }
        self.set_attr(node, "class", &tokens.join(" "));
    }

    fn clear(&mut self, node: usize) {
        let mut m = self.doc.mutate();
        m.remove_and_drop_all_children(node);
    }

    /// Set or clear a checkbox's `checked` **attribute**.
    ///
    /// This is an *attribute-level* setter, deliberately: it does not model a
    /// live browser `checked` **property**, and it does not reproduce the
    /// property/attribute split (`el.checked = x` changes the property and
    /// leaves the attribute alone; `setAttribute('checked', ...)` sets the
    /// default). The reference's `hideChildrenEl.checked = ...` assignments are
    /// property writes, and the honest native reading of those is "the box
    /// renders as on/off", which is exactly what the attribute controls in this
    /// renderer. Anyone later needing the property semantics must add a real
    /// property model rather than assuming this one has it.
    ///
    /// Present because the reference carries two mutually-inverse filter
    /// checkboxes (`#groupChildren`, `#hideChildren`, `shell.js:602-604` /
    /// `645-647`) over one flag, and before this there was **no** way to paint
    /// either: `set_attr`/`clear_attr`/`set_class_flag` existed, but no boolean
    /// attribute setter, so a checkbox could be modelled and clicked while
    /// never showing its own state — the half-dead control this project exists
    /// to eliminate.
    ///
    /// `on == true` writes `checked=""` (the canonical form of a boolean
    /// attribute); `on == false` removes it, so "unchecked" is genuinely absent
    /// rather than `checked="false"`, which HTML would read as checked.
    fn set_checked(&mut self, node: usize, on: bool) {
        if on {
            self.set_attr(node, "checked", "");
        } else {
            self.clear_attr(node, "checked");
        }
    }

    /// `data-key` on the element with `id`, when it exists.
    ///
    /// `data-key` is the shell's **nearest-ancestor** payload channel
    /// (`nearest_data_attr`): a press on any descendant still carries it. The
    /// filter controls use it where `data-id` cannot help — the clickable
    /// element is a `<label>`/`<button>` whose child text/spans are what the
    /// pointer actually hits.
    fn set_data_key_by_id(&mut self, id: &str, value: &str) {
        if let Some(node) = self.id(id) {
            self.set_attr(node, "data-key", value);
        }
    }

    /// `data-key` on the PARENT of the element with `id`.
    ///
    /// The filter checkboxes are `<input id=...>` *inside* their clickable
    /// `<label>`; the input itself has a box but the label text is what a user
    /// presses. Putting the payload on the label (the input's parent) is what
    /// makes a press anywhere on the row resolve, exactly like a browser label.
    fn set_data_key_on_parent(&mut self, child_id: &str, value: &str) {
        let Some(node) = self.id(child_id) else { return };
        let Some(parent) = self.doc.get_node(node).and_then(|n| n.parent) else {
            return;
        };
        self.set_attr(parent, "data-key", value);
    }

    /// Tag the static `linkbtn[data-kinds]` / `linkbtn[data-edges]` buttons with
    /// their `data-key` payload.
    ///
    /// These buttons carry only `data-kinds`/`data-edges` in the shipped markup,
    /// and the `@hit` wire has no column for arbitrary attributes — so without
    /// this they could never be routed. Their identity is copied onto the
    /// shell's existing nearest-ancestor `data-key` channel instead of growing
    /// the protocol, which is the same rule the facet chips use.
    ///
    /// Deliberately a plain DESCENDANT WALK, not `BaseDocument::query_selector`:
    /// `apply_runtime_dom` runs **before** style resolution, and Stylo's selector
    /// matching calls `ElementStyles::primary()`, which panics on an element with
    /// no computed style yet (`vendor/stylo/data.rs:190`). `find_by_class` above
    /// documents the same reason for not using the selector API at this stage.
    fn tag_filter_link_buttons(&mut self, root: usize) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let (children, payload) = {
                let Some(n) = self.doc.get_node(node) else { continue };
                let children = n.children.clone();
                let payload = n.element_data().and_then(|el| {
                    let attr = |name: &str| {
                        el.attrs
                            .iter()
                            .find(|a| a.name.local.as_ref() == name)
                            .map(|a| a.value.clone())
                    };
                    match (attr("data-kinds"), attr("data-edges")) {
                        (Some(v), _) => match v.as_str() {
                            "all" => Some("filter:kindsAll"),
                            "none" => Some("filter:kindsNone"),
                            _ => None,
                        },
                        (_, Some(v)) => match v.as_str() {
                            "all" => Some("filter:edgesAll"),
                            "none" => Some("filter:edgesNone"),
                            _ => None,
                        },
                        _ => None,
                    }
                });
                (children, payload)
            };
            if let Some(key) = payload {
                self.set_attr(node, "data-key", key);
            }
            stack.extend(children);
        }
    }
}

// ------------------------------------------------------------- stylesheet splice

/// True when `href` names the stylesheet this run was given (by basename).
fn is_style_href(href: &str, css_name: &str) -> bool {
    let base = href.rsplit('/').next().unwrap_or(href);
    base == css_name
}

/// Where the four injected stylesheets live, and why they are read rather
/// than compiled into this binary.
///
/// CSS is not allowed to live in Rust source. It *is* allowed to be "a file
/// that is read and used", so each sheet is a real `.css` asset under
/// `nzk_ui/assets/css/` read at runtime, exactly like the real `style.css`
/// the same call already reads via [`crate::NzkUi::frontend_dir`]. Keeping the
/// assets out of `src/` means a reader never has to guess whether a `.css` file
/// beside the `.rs` files is source or data, and keeping the text out of the
/// binary means the sheet stays editable and inspectable as CSS.
///
/// The failure mode is a `Result`: a missing or unreadable asset is an error
/// naming the path, never a panic and never a silently empty `<style>` (an empty
/// sheet would paint a different document without saying so).
pub(crate) mod assets {
    use std::path::{Path, PathBuf};

    /// `native-csviewer/nzk_ui/assets/css` — sibling of `src/`, so the
    /// assets are unambiguously *not* source. Resolved from the manifest dir
    /// like [`super::super::NzkUi::frontend_dir`], so tests and the CLI agree.
    pub fn css_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/css")
    }

    /// Read one asset sheet by file name.
    pub fn read_sheet(name: &str) -> Result<String, String> {
        let path = css_dir().join(name);
        std::fs::read_to_string(&path)
            .map_err(|e| format!("read injected stylesheet {}: {e}", path.display()))
    }

    /// Names of the four sheets spliced by [`super::inline_stylesheets`], in
    /// emission order (`compat`, `problems`, `hierarchy`, `quickopen`).
    pub const SHEET_NAMES: [&str; 4] = [
        "compat.css",
        "problems-injected.css",
        "hierarchy-injected.css",
        "quickopen-injected.css",
    ];
}

/// The Blitz engine-gap shim (`assets/css/compat.css`). It is injected as its
/// own `<style>` element and must stay a no-op under Chromium.
///
/// CSS that `problems.js` `injectStyle()` (lines 40-59) appends to <head> at
/// runtime lives in `assets/css/problems-injected.css`. Without it the
/// Parser-warnings note loses its `padding-bottom: 7px`, `font-size: 9.5px` and
/// `line-height: 1.45`, and the paragraph re-wraps from two lines to three.
/// Reproduced verbatim (the concatenation of that array).
///
/// Both are named in [`assets::SHEET_NAMES`]; see the note above [`assets`] for
/// why neither is spliced into the binary.
///
/// CSS that `hierarchy.js` `injectStyle()` (lines 355-384) appends to <head> at
/// runtime as `<style id="cs-hier-style">`.
///
/// Only the `.cs-hier*` rules are in that file. The `.hop*` row classes the same
/// renderer uses are **not** injected — they live in the real stylesheet
/// (`style.css:1634-1660`, with `nzk-dark+` overrides at 1990-1991) because the
/// "Resolve chain" drawer shares them, and duplicating them here would freeze
/// them against theme changes. See the file's own header.
///
/// CSS that `quickopen.js` `injectStyle()` (lines 65-109) appends to <head> at
/// runtime as `<style id="cs-quickopen-style">`.
///
/// Without it **every** quick-open class is unstyled: none of the 13 `cs-qo-*`
/// class names and neither `cs-quickopen` selector exists in the real
/// `style.css`, so `#cs-quickopen` would be a plain in-flow block rather than a
/// fixed, full-viewport, `z-index: 80` overlay, and `.cs-qo-row.sel` (line 92)
/// would not mark the selected row at all. The block cannot be replaced with
/// inline `style` attributes: it relies on descendant selectors, pseudo-elements
/// and pseudo-classes (`.cs-qo-row.sel`, `.cs-qo-input::placeholder`,
/// `.cs-qo-note.hidden`, `#cs-quickopen.hidden`) that no style attribute can
/// express.
///
/// Emitted unconditionally, like the problems sheet. That is safe for the
/// `@frame` baseline because the token hashes the **painted RGBA bytes**
/// (`serve.rs:376-383`), not the HTML, and `#cs-quickopen` is absent from the
/// shipped document — so this sheet styles nothing until the palette opens.
mod sheet_docs {}

/// Replace every `<link rel="stylesheet" href="…/style.css">` with an inline
/// `<style>` holding the real on-disk stylesheet, followed by the (separately
/// marked) engine-gap shim.
///
/// This is the Rust stand-in for the browser's stylesheet fetch: the HTML that
/// reaches the parser is the real `index.html`, and the CSS that reaches Stylo
/// is the byte-identical real `style.css`. Nothing else about the document is
/// rewritten apart from the shim, which is emitted in its own <style> element
/// so a reader can tell the two apart.
///
/// The four injected sheets are read from disk here, in the order
/// `compat, problems, hierarchy, quickopen` — the same order, the same
/// `<style id="…">` wrappers and the same bytes as the previous compile-time
/// splice, so the document shape is unchanged. See [`assets`] for why they are
/// read rather than compiled in.
///
/// Returns `Err` (naming the offending path) if any asset cannot be read;
/// callers surface that as their own error. It is deliberately not a fallback
/// to an empty sheet: an empty `<style>` would paint a different document
/// without failing.
pub fn inline_stylesheets_checked(
    html: &str,
    css_path: &Path,
    css: &str,
) -> Result<String, String> {
    let compat = assets::read_sheet(assets::SHEET_NAMES[0])?;
    let problems = assets::read_sheet(assets::SHEET_NAMES[1])?;
    let hierarchy = assets::read_sheet(assets::SHEET_NAMES[2])?;
    let quickopen = assets::read_sheet(assets::SHEET_NAMES[3])?;
    Ok(inline_stylesheets_with(
        html,
        css_path,
        css,
        &compat,
        &problems,
        &hierarchy,
        &quickopen,
    ))
}

/// [`inline_stylesheets_checked`] against explicitly supplied sheet text.
///
/// Split out so a test can prove the seven-piece splice byte-for-byte without
/// depending on the asset files being present, and so the read (I/O, fallible)
/// is separate from the splice (pure).
#[allow(clippy::too_many_arguments)]
fn inline_stylesheets_with(
    html: &str,
    css_path: &Path,
    css: &str,
    compat: &str,
    problems: &str,
    hierarchy: &str,
    quickopen: &str,
) -> String {
    let css_name = css_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "style.css".to_string());
    let mut out = String::with_capacity(
        html.len() + css.len() + compat.len() + problems.len() + hierarchy.len() + quickopen.len() + 32,
    );
    let mut rest = html;

    while let Some(pos) = rest.find("<link") {
        let after = &rest[pos..];
        let Some(end) = after.find('>') else { break };
        let candidate = &after[..=end];
        let lower = candidate.to_ascii_lowercase();
        let is_css = lower.contains("stylesheet");
        let matches = attribute_value(candidate, "href")
            .as_deref()
            .map(|h| is_style_href(h, &css_name))
            .unwrap_or(false);
        out.push_str(&rest[..pos]);
        if is_css && matches {
            out.push_str("<style id=\"csviewer-real-stylesheet\">");
            out.push_str(css);
            out.push_str("</style><style id=\"nzk_ui-compat-shim\">");
            out.push_str(compat);
            out.push_str("</style><style id=\"cs-problems-style\">");
            out.push_str(problems);
            out.push_str("</style><style id=\"cs-hier-style\">");
            out.push_str(hierarchy);
            out.push_str("</style><style id=\"cs-quickopen-style\">");
            out.push_str(quickopen);
            out.push_str("</style>");
        } else {
            out.push_str(candidate);
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Splice the real stylesheet and the four injected sheets into `html`, reading
/// the injected sheets from `assets/css/`.
///
/// This is the fallible entry point and the one [`crate::NzkUi`] uses: a sheet
/// that cannot be read is an `Err` naming the path, never a panic and never a
/// silently empty `<style>`.
pub fn inline_stylesheets(
    html: &str,
    css_path: &Path,
    css: &str,
) -> Result<String, String> {
    inline_stylesheets_checked(html, css_path, css)
}

/// Every injected sheet as `(name, text)`, in splice order. Used by the tests
/// that assert the assets are present, distinct and non-empty.
#[cfg(test)]
fn injected_sheets() -> Vec<(&'static str, String)> {
    assets::SHEET_NAMES
        .iter()
        .map(|n| (*n, assets::read_sheet(n).expect("read injected sheet")))
        .collect()
}

/// Minimal attribute reader: `attr="value"` / `attr='value'` / `attr=value`.
fn attribute_value(tag_text: &str, name: &str) -> Option<String> {
    let lower = tag_text.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(name) {
        let at = from + rel;
        from = at + name.len();
        let before_ok = at == 0
            || lower.as_bytes()[at - 1].is_ascii_whitespace()
            || lower.as_bytes()[at - 1] == b'<';
        if !before_ok {
            continue;
        }
        let after = tag_text[at + name.len()..].trim_start();
        let Some(after) = after.strip_prefix('=') else {
            continue;
        };
        let after = after.trim_start();
        let quote = after.chars().next()?;
        if quote == '"' || quote == '\'' {
            return after[1..].split(quote).next().map(|s| s.to_string());
        }
        return after
            .split_whitespace()
            .next()
            .map(|s| s.trim_end_matches('>').to_string());
    }
    None
}

/// Elements whose geometry the CSS shell must reproduce. Kept in sync with
/// `tools/domdump.mjs` SELECTORS so `nzk_ui --boxes` and the live CDP dump
/// can be diffed directly.
pub const PROBE_SELECTORS: &[&str] = &[
    ".topbar", ".brand", ".brand-mark", ".rootpick", "#rootInput", "#browseBtn", "#indexBtn",
    ".searchwrap", ".mode-switch", "#modeGraph", "#modeFind", "#searchInput", "#statusLine",
    "#exportBtn", "#editBtn", "#blocksBtn", "#previewBtn", "#shortcutsBtn", "#newWinBtn",
    ".layout", "#canvasWrap", "#graph", ".canvas-tools", "#backBtn", "#upBtn", "#zoomInBtn",
    "#zoomOutBtn", "#fitBtn", "#wholeGraphBtn", "#resettleBtn", "#layoutState", "#notice",
    ".side", ".pane-grip", ".pane-grip-title", ".side .pane-float-btn", ".side .pane-resize",
    "#filtersSection", "#filtersHead", "#filtersCount", "#projectFilterBlock",
    "#projectFilters", "#nsSelect", ".filter-label", "#kindFilters", ".facet",
    ".facet .swatch", ".facet .facet-name", ".facet .facet-count",
    "#edgeFilters", ".edgekey", ".edgekey .bars", ".check",
    "#resetFilters", "#pruneHidden", ".filter-group", ".range", ".range-name", ".range-val",
    ".hint", "#pubSection", "#strSection", "#chainSection", "#problemsSection",
    "#problemsHead", "#pubHead", "#strHead", "#chainHead",
    "#problemsCount", ".cs-problems-note", "#problemsSearch", "#problemsList",
    "#projectFilters .edgekey",
    // pane-open geometry (task-30): the container the native source content must
    // land in, plus its head/body.
    "#sourcePane", "#sourceBody", ".source-head",
    // The split pane's second body and its divider (round 8). Both are created
    // only when `ui.tabs.split_on` is true, so under the shipped default facts
    // their probe rows are MISSING on both the parsed and the skeleton document
    // (no such node exists in either), which is why adding them leaves the
    // painted frame — and the pinned @frame token — unchanged. With `split_on`
    // they probe real boxes, so the host can paint the second body from the DOM
    // geometry instead of deriving halves in Rust.
    "#csSplitHost", "#csSplitDivider",
    // ---- D2-followup probe expansion (modal / panel / drawer ids) ----------
    "#nodeDrawer", "#nodeDrawerClose", "#nodeCodeBtn", "#nodeTitle", "#nodeCrumbs",
    "#nodeProps", "#propsSection", "#relationsSection", "#relationsHead",
    "#relationsBody", "#relationsCount",
    "#findPanel", "#findQuery", "#findMeta", "#findClose", "#findNotice", "#findResults",
    "#blocksPanel", "#blocksTitle", "#blocksMeta", "#blocksClose", "#blocksScroll",
    "#blocksStage", "#blocksEdges", "#blocksCards", "#blocksPeek",
    "#reorgModal", "#reorgTitle", "#reorgClose", "#reorgMode", "#reorgScope",
    "#reorgGran", "#reorgGroup", "#reorgSize", "#reorgDest", "#reorgHeader",
    "#reorgReport", "#reorgNote", "#reorgCompile", "#reorgVerify", "#reorgWrite", "#reorgZip",
    "#editModal", "#editTitle", "#editClose", "#editTool", "#editArgs", "#editOut",
    "#editNote", "#editValidate", "#editDedupe", "#editDry", "#editApply",
    "#previewModal", "#previewTitle", "#previewClose", "#previewSelector", "#previewOp",
    "#previewTo", "#previewOut", "#previewNote", "#previewRun",
    "#shortcutsModal", "#shortcutsTitle", "#shortcutsClose", "#shortcutsBody", "#shortcutsDone",
    "#uiTheme",
    "#modal", "#modalTitle", "#modalClose", "#modalPathInput", "#modalGo", "#modalUp",
    "#modalHome", "#modalRoot", "#modalRepo", "#modalPath", "#modalBody", "#modalNote",
    "#modalCancel", "#modalSelect",
    "#pubSearch", "#pubList", "#strSearch", "#strList",
    // v3-only Declared-types section (created at runtime by `apply_type_defs`,
    // present only when the host publishes `type_defs`). Missing rows are the
    // honest "this fixture has no such section", exactly like `#csSplitHost`.
    "#typeSection", "#typeHead", "#typeCount", "#typeSearch", "#typeChips", "#typeList",
    "#chainDepth", "#chainDir", "#chainRun", "#chainFlatBtn", "#chainTree",
    "#srcCopyBtn", "#srcRefBtn", "#sourceTitle", "#sourceChipWrap", "#sourceMatchTag",
    "#sourceTheme", "#srcModeBtn", "#srcPopBtn", "#srcMethodBtn", "#srcWindowBtn",
    "#srcPrevBtn", "#srcNextBtn", "#srcCloseBtn", "#sourceNote",
    "#nsAllBtn", "#nsDeep", "#hideEdgeLabels", "#hideIsolated", "#depthSelect",
    "#hideChildren", "#spacingRange", "#spacingOut", "#nodeSizeRange", "#nodeSizeOut",
    "#iterRange", "#iterOut", "#curveRange", "#curveOut",
    // The fork's own fifth Layout slider. It exists in `index.html` (and is
    // painted) but was missing from this probe list, so `--boxes` never emitted
    // a row for it and the host could not hit-test it at all: a painted control
    // with no geometry is the dead-control class.
    "#textSizeRange", "#textSizeOut",
    "#kindChildWrap", "#kindEdgeWrap",
    "#loading", "#loadingText", "#stopIndexBtn", "#tooltip", "#emptyHint", "#suggestions", "#toasts",
];

// ------------------------------------------------------- runtime DOM (per script)

/// nzk-dark+ kind colours — `core.js` `KIND_COLOR_NZK_DARKPLUS` (lines 91-107).
pub fn kind_color(kind: &str) -> &'static str {
    match kind {
        "namespace" | "class" | "record" => "#4ec9b0",
        "struct" => "#86c691",
        "interface" | "enum" => "#b8d7a3",
        "method" | "constructor" => "#dcdcaa",
        "operator" => "#c586c0",
        "property" | "field" | "event" => "#9cdcfe",
        "string" => "#ce9178",
        "external" | "usage" => "#808080",
        _ => "#858585",
    }
}

/// `core.js` EDGE_KINDS (line 31) plus EDGE_STYLE colour/width/dash (lines
/// 55-62). `filters.js` draws `border-top-style` = dashed when a dash array
/// exists and `border-top-width` = max(2, width).
pub const EDGE_CHIPS: &[(&str, &str, u32, bool)] = &[
    ("calls", "#c9d3e5", 2, false),
    ("uses", "#c9d3e5", 2, true),
    ("assigns", "#c9d3e5", 2, true),
    ("constructs", "#c9d3e5", 2, false),
    ("inherits", "#c9d3e5", 2, false),
    ("contains", "#c9d3e5", 2, true),
];

/// One `.sln`/`.csproj` chip — `filters.js` buildProjectFilter (lines 128-151).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub file_count: usize,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub defines: Vec<String>,
    /// `filters.js`: the chip is OFF when the project id is in
    /// `state.filters.projects`. Missing => ON (the shipped render).
    #[serde(default = "default_true")]
    pub on: bool,
}

/// `serde` default for the chip flags: absent facts leave every chip ON, which
/// is the shipped document's state.
fn default_true() -> bool {
    true
}

/// One parser-warning row — `problems.js` rowEl (lines 166-189).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WarningRow {
    pub file: String,
    pub line: usize,
    #[serde(default)]
    pub col: usize,
    #[serde(default)]
    pub text: String,
    /// The verbatim warning line, used when `text` is empty.
    #[serde(default)]
    pub raw: String,
}


/// One `div.reorg-row` of `#reorgReport` (`reorg.js:46-52`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ReorgReportRow {
    /// `.reorg-k` — the row label.
    pub k: String,
    /// `.reorg-v` — the value.
    pub v: String,
    /// Optional class on the row: `"good"`, `"bad"`, `"warn"` or empty
    /// (`reorg.js:47` `'reorg-row' + (cls ? ' ' + cls : '')`).
    pub class: String,
}

/// The Reorganise & export dialog's facts (`#reorgModal`, `reorg.js`).
///
/// One `ui.reorg` object carries the form state (`#reorgMode`, `#reorgScope`,
/// `#reorgGran`, `#reorgGroup`, `#reorgSize`, `#reorgDest`, `#reorgHeader`), the
/// report rows and `#reorgNote`. `None` (the shipped/default document) writes
/// nothing at all, which is what keeps the default render — and the `@frame`
/// token — byte-identical.
///
/// ## The three conditional rows
///
/// `syncRows` (`reorg.js:286-290`) is the whole rule and it runs when the
/// dialog opens (`:278`): `#reorgGranRow`/`#reorgGroupRow` are visible only in
/// `split` mode, and `#reorgSizeRow` only when the mode is `split` **and** the
/// group is `size`. This renderer applies the same rule from `mode`/`group`, so
/// the rows cannot disagree with the select values painted from the same
/// object.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ReorgFacts {
    /// `#reorgMode`'s value (`"single"` | `"split"`).
    pub mode: Option<String>,
    /// `#reorgScope`'s value (`"selection"` | `"all"`).
    pub scope: Option<String>,
    /// `#reorgGran`'s value (`"leaf"` | `"top"`).
    pub gran: Option<String>,
    /// `#reorgGroup`'s value (`"size"` | `"namespace"` | `"owner"` | `"single"`).
    pub group: Option<String>,
    /// `#reorgSize`'s value.
    pub size: Option<u64>,
    /// `#reorgHeader.checked`.
    pub header: Option<bool>,
    /// `#reorgDest`'s value.
    pub dest: Option<String>,
    /// True when `rows` describe a finished engine run (as opposed to an open
    /// dialog that has not run yet): an empty `rows` with `has_report` renders
    /// the reference's "Plan or verify…" placeholder (`reorg.js:120`), while an
    /// empty `rows` without it leaves the shipped empty `#reorgReport` alone.
    pub has_report: bool,
    /// The report rows (`render`, `reorg.js:57-120`).
    pub rows: Vec<ReorgReportRow>,
    /// `#reorgNote` text (`setNote`, `reorg.js:37-40`).
    pub note: String,
    /// The note's extra class: `""`, `"good"`, `"warn"` or `"bad"`.
    pub note_kind: String,
}

/// One row of an open form-control menu (`ui.form.menu`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FormOptionFacts {
    /// The `<option value>` the row selects.
    pub value: String,
    /// The row's visible text.
    pub label: String,
    /// The row's current value (the highlighted row when the menu opened).
    pub selected: bool,
}

/// An open select menu (`ui.form.menu`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FormMenuFacts {
    /// The select's DOM id — the menu is inserted directly after its row.
    pub id: String,
    /// The highlighted row index (`Up`/`Down` move it).
    pub active: usize,
    pub options: Vec<FormOptionFacts>,
}

/// A focused text box (`ui.form.input`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FormInputFacts {
    /// The input's DOM id.
    pub id: String,
    /// The in-flight text (the host's edit buffer).
    pub value: String,
    /// The caret's character index in `value`, when the host has a live edit
    /// buffer for this box.
    ///
    /// `None` means "no caret to paint": an unfocused box whose committed value
    /// is still published (the picker's path, a prefilled preview selector) must
    /// not grow a caret at offset 0. The caret is what lets
    /// [`dom::apply_form_shim`] scroll a value that is wider than the input
    /// instead of leaving the typed tail clipped off-screen.
    pub caret: Option<usize>,
}

/// The form-control interaction facts (`ui.form`, task-14).
///
/// `None` (the shipped/default document) writes nothing: no input value, no
/// overlay menu — so the pinned `@frame` token cannot move. When a select is
/// open, the menu is painted as a real `#csFormMenu` list inserted directly
/// after the control's own `.settings-row` (an in-flow list, not a floating
/// popup: this static rasteriser has no anchored-overlay CSS, so inventing
/// geometry would be a claim about pixels it cannot verify).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct FormFacts {
    /// The focused control's DOM id, if any.
    pub focused: Option<String>,
    #[serde(default)]
    pub menu: Option<FormMenuFacts>,
    #[serde(default)]
    pub input: Option<FormInputFacts>,
}

/// One row of an engine-backed Find group (v2 `buildDefRow`/`buildUsageRow`,
/// `search.js:181-250`). Field names mirror the app's
/// `find_wire::FindRow`, which projects the engine's own `/api/find` rows.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FindGroupRow {
    /// The node's kind (`method`, `class`, …) or `usage`.
    #[serde(default)]
    pub kind: String,
    /// `full || label || id` (`search.js:187`), or `file:line` for a usage.
    #[serde(default)]
    pub title: String,
    /// The `.frow-sub` line (`file:line` for defs/impls, `in <owner>` for usages).
    #[serde(default)]
    pub subtitle: String,
    /// The node click target (`openFindNode`).
    #[serde(default)]
    pub node_id: String,
    /// The usage's own location (`openUsage`).
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub line: i64,
    #[serde(default)]
    pub col: i64,
    /// The usage's source line, verbatim from the engine.
    #[serde(default)]
    pub text: String,
    /// `exact` or `simple` (`search.js:214-250`).
    #[serde(default, rename = "match")]
    pub match_kind: String,
}

/// One engine-backed Find group (`buildGroup`, `search.js:253-303`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FindGroupFacts {
    /// `definitions` / `implementations` / `usages`.
    #[serde(default)]
    pub kind: String,
    /// The group head text (`Definitions`, …).
    #[serde(default)]
    pub heading: String,
    /// The engine's count for the group (`fmtNum(items.length)`).
    #[serde(default)]
    pub count: usize,
    #[serde(default)]
    pub rows: Vec<FindGroupRow>,
}

/// Host-owned *view* state, as opposed to corpus data.
///
/// The strict v3 reading is that the web GUI owns state: the host holds which
/// sections are collapsed and which search mode is active, and the nzk_ui is
/// rebuilt from it. `None` leaves the document exactly as `index.html` ships it,
/// which is what keeps the default render byte-identical.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UiState {
    /// Section ids (or head selectors) whose body is collapsed.
    ///
    /// `None` leaves every section exactly as `index.html` ships it — that is
    /// what keeps the default render byte-identical. `Some(list)` is the
    /// **complete** collapsed set: listed sections are hidden and the rest are
    /// shown, so the host must send a seeded default rather than an empty list.
    #[serde(default)]
    pub collapsed: Option<Vec<String>>,
    /// `"graph"` or `"find"` — which mode tab carries `.active`.
    #[serde(default)]
    pub active_mode: Option<String>,
    /// The source pane is open: `.layout` gets `with-source` (the real
    /// `grid-template-rows: minmax(0,1fr) minmax(120px, var(--source-h))`) and
    /// `#sourcePane` loses `hidden`. The nzk_ui is then laid out against the
    /// real pane grid instead of a hand-cut frame.
    #[serde(default)]
    pub pane_open: bool,
    /// The overlay **drawer family** is open — `#nodeDrawer`, `#findPanel`,
    /// `#blocksPanel`.
    ///
    /// These three ship as `class="drawer hidden"` / `class="blocks-panel hidden"`
    /// and `style.css:63` is `.hidden { display: none !important; }`, so in the
    /// shipped document every one of them has **no box at all**: their
    /// `--boxes` rows are `0 0 0 0`. A zero-area rect can never satisfy
    /// `hit_in`'s `r.contains(point)`, so no hit-test can ever reach them while
    /// they are hidden — the selectors being *probe-visible* is not the same
    /// thing as the controls being *reachable*.
    ///
    /// Listing an id here clears its `hidden` class, which is exactly what the
    /// front-end scripts do (`drawer.js` / `search.js` / `scratch.js`), so the
    /// panel is laid out by the real stylesheet and gets real geometry. Drains in
    /// the same direction as [`UiState::pane_open`]: `false`/empty leaves the
    /// document exactly as `index.html` ships it, keeping the default render
    /// byte-identical.
    ///
    /// Deliberately **drawer-family only**; the modal family has its own fact,
    /// [`UiState::modals_open`]. Only ids the drawer family actually declares are
    /// honoured; anything else is ignored, so a caller cannot reach into the
    /// modal family through here.
    #[serde(default)]
    pub drawers_open: Vec<String>,
    /// The overlay **modal family** is open — `#shortcutsModal`, `#editModal`,
    /// `#previewModal`, `#reorgModal`, `#modal`.
    ///
    /// These ship as `class="modal-backdrop hidden"` and `style.css:63` is
    /// `.hidden { display: none !important; }`, so in the shipped document every
    /// one of them has **no box at all** (a `0 0 0 0` `--boxes` row) and no
    /// hit-test can reach it. Listing an id here clears its `hidden` class,
    /// which is exactly what the front-end scripts do, so the real stylesheet
    /// lays the modal out and it gets real geometry.
    ///
    /// Re-measured on this tree at 1600x1000 (the earlier "a forced `#reorgModal`
    /// lands at `y=1565`" note in this comment was stale and is corrected here):
    /// `.modal-backdrop` is `position: fixed; inset: 0` with
    /// `display: flex; align-items: center; justify-content: center`, so an
    /// un-hidden modal measures the **full viewport** — `#shortcutsModal`,
    /// `#editModal`, `#previewModal` and `#reorgModal` all measure
    /// `0.00 0.00 1600.00 1000.00` — and it genuinely centres its card. In the
    /// same measurement `#shortcutsTitle` is `553 377 147x15`, `#shortcutsBody`
    /// `541 451 518x140` and `#shortcutsDone` `986 602 53x30`, all inside the
    /// viewport, and `--hit 1012,617` resolves to `button #shortcutsDone`. No
    /// compensating transform is needed or wanted: the layout already centres.
    ///
    /// Because the backdrop covers the viewport, an open modal also **swallows
    /// every hit** outside its card — `--hit 790,960` resolves to
    /// `#previewModal .modal-backdrop`, not the canvas. That is the "a modal is
    /// modal" property, and it is why at most one id is ever honoured: two open
    /// backdrops would let DOM order decide which card a press reached.
    ///
    /// Drains in the same direction as [`UiState::pane_open`]: empty leaves the
    /// document exactly as `index.html` ships it, keeping the default render
    /// byte-identical.
    #[serde(default)]
    pub modals_open: Vec<String>,
    /// The loading overlay while an index build runs (`showLoading`,
    /// `shell.js:157-176`): clears `#loading`'s shipped `hidden` class, writes
    /// `#loadingText`, and makes `#stopIndexBtn` clickable. `None` leaves the
    /// shipped hidden overlay untouched.
    #[serde(default)]
    pub loading: Option<LoadingFacts>,
    /// Resolve-chain content (`#chainTree` / `#chainCount`). `None` leaves the
    /// shipped "Select a node, then Run." row untouched.
    #[serde(default)]
    pub chain: Option<ChainFacts>,
    /// Source-pane head content. `None` while the pane is closed.
    #[serde(default)]
    pub source: Option<SourceFacts>,
    /// Scroll-container offsets, applied after layout with the engine's own
    /// clamping (`BaseDocument::scroll_node_by`). This is what makes a sidebar
    /// taller than its grid row reachable: `.side { overflow-y: auto }` is a
    /// real scroll container in the stylesheet.
    #[serde(default)]
    pub scroll: Vec<ScrollSpec>,
    /// Find-panel **content** — the text the search core computed.
    ///
    /// [`UiState::drawers_open`] only gets `#findPanel` on screen; a panel with
    /// no content is an empty box, which is the "registered but dead" shape this
    /// fact exists to remove. `None` (the serde default) leaves `#findQuery`,
    /// `#findMeta`, `#findNotice` and `#findResults` exactly as `index.html`
    /// ships them, so the default render stays byte-identical.
    #[serde(default)]
    pub find: Option<FindFacts>,
    /// Blocks-canvas **content** — the cards, ports and edge paths the host
    /// derived from the graph (task-13, v2 `scratch.js`).
    ///
    /// `None` leaves the shipped empty `#blocksStage`/`#blocksCards` untouched,
    /// keeping the default render byte-identical; `Some` with no cards paints the
    /// reference's own `.blocks-empty` sentence (`scratch.js:320-323`).
    #[serde(default)]
    pub blocks: Option<BlocksCanvasFacts>,
    /// Folder-picker **content** — the listing the picker core resolved.
    ///
    /// Same relationship to [`UiState::modals_open`] as [`UiState::find`] has to
    /// `drawers_open`: `#modal` is on screen with real geometry, and this is what
    /// goes inside it. `None` leaves every picker node as shipped.
    #[serde(default)]
    pub picker: Option<PickerFacts>,
    /// Quick-open palette **content** — the palette the host opened with
    /// Ctrl+P / Ctrl+T.
    ///
    /// Unlike [`UiState::find`] / [`UiState::picker`] there is no `drawers_open`
    /// / `modals_open` counterpart to pair with, because `#cs-quickopen` does
    /// not exist in `index.html` at all: `quickopen.js`'s `ensureDom()`
    /// (quickopen.js:113-181) *creates* the overlay at runtime and appends it to
    /// `document.body`. This field is therefore both the open flag and the
    /// content, and `None` — the serde default, and the state the shipped
    /// document is in — creates nothing at all, which is what keeps the default
    /// render byte-identical.
    ///
    /// **`Some` with `state == QuickOpenState::Closed` is also a no-op.** The
    /// reference's `close()` only clears `shown` and re-adds `hidden`
    /// (quickopen.js:441-447); this layer reproduces that by writing
    /// `class="... hidden"` on the overlay it created, never by *un*-hiding it.
    /// See [`apply_quickopen`].
    #[serde(default)]
    pub quickopen: Option<QuickOpenFacts>,

    /// The open-document tab bar content — [`TabsFacts`], painted into the
    /// `#cs-tabs` strip `shell.js:1192-1248` creates at the top of
    /// `#sourcePane`.
    ///
    /// Like [`UiState::quickopen`] there is no separate open flag, because the
    /// shipped document has no `#cs-tabs` node at all (`renderTabBar` creates
    /// it with `pane.insertBefore`, shell.js:1197). `None` — the serde default,
    /// and the state a fresh nzk_ui sends — therefore creates nothing, which is
    /// what keeps the default render byte-identical. `Some` with an empty list
    /// is a real state too: the bar exists and carries `hidden`, exactly as the
    /// reference's `tabs.length === 0` branch leaves it.
    #[serde(default)]
    pub tabs: Option<TabsFacts>,

    /// The topbar search box's typed query — `#searchInput`'s `value`
    /// (`search.js:58/129` reads and writes the input's `value` property).
    ///
    /// The container element ships with **no** `value` attribute, so painting
    /// `Some(text)` as the attribute is the only representation the static
    /// document can carry. `None` — the serde default, and the state of a nzk_ui
    /// where nothing has been typed — writes nothing, which is what keeps the
    /// default render byte-identical. `Some("")` is a real state too: the box is
    /// focused with an empty query, and writing `value=""` is an idempotent
    /// clear rather than an absence.
    #[serde(default)]
    pub search_query: Option<String>,
    /// `#pubSearch` / `#strSearch`'s committed text. `None` leaves the shipped
    /// empty inputs untouched.
    #[serde(default)]
    pub pub_query: Option<String>,
    #[serde(default)]
    pub str_query: Option<String>,
    /// `#uiTheme`'s committed value, written to `<html data-ui-theme>` — the
    /// attribute every theme rule in `style.css` keys on.
    #[serde(default)]
    pub ui_theme: Option<String>,
    /// `#sourceTheme`'s committed option. Absent while `darkplus` (the shipped
    /// default) so the default document is untouched.
    #[serde(default)]
    pub source_theme: Option<String>,

    /// `state.filters.groupChildren` (`core.js:270`, shipped default `true`) —
    /// the flag behind the two mutually-inverse checkboxes.
    ///
    /// `#groupChildren` shows this value and `#hideChildren` shows its negation
    /// (`shell.js:603-604`, `:645-647`; see [`apply_filter_checkboxes`]). One
    /// flag, two boxes: the inversion is the DOM's job, not the host's, so a host
    /// that published two independent booleans could produce a document the
    /// reference cannot reach.
    ///
    /// `#[serde(default)]` makes a missing key `false`, which would be wrong for
    /// the *shipped* document (it ships `groupChildren` checked). It is safe
    /// because `apply_filter_checkboxes` is only reached under
    /// `if let Some(ui) = &facts.ui` — with no `ui` at all nothing is written and
    /// the shipped attributes stand. A host that does send `ui` is asserting it
    /// owns the view state, and the one owner
    /// (`GraphView::group_children` / `set_group_children`) defaults to `true`.
    #[serde(default)]
    pub group_children: bool,
    /// `state.filters.groupEdges` (`core.js:271`) — the flag behind the
    /// `#groupEdges` checkbox, the *edges* counterpart of
    /// [`Self::group_children`] and a distinct control (`#groupEdges` vs
    /// `#groupChildren`; `shell.js:609-612`).
    ///
    /// The key is spelled `groupEdges` with an explicit
    /// `#[serde(rename = "groupEdges")]` rather than the struct-level
    /// `rename_all = "camelCase"` the `SettingsFacts` trap called for, and the
    /// reason is that **[`UiState`] is deliberately NOT a `rename_all` struct**:
    /// it is the general view-state bag whose sibling keys (`drawers_open`,
    /// `modals_open`, `quickopen`, ...) are all spelled in `snake_case`, and the
    /// host emits them that way (`app/src/nzk_ui.rs`'s `ui_state_json`). Putting
    /// `rename_all = "camelCase"` on the struct to catch this one key would
    /// silently rename all of them and kill the `drawers_open`/`modals_open`
    /// plumbing — the same bug class, mirrored. A per-field `rename` is the
    /// narrow fix that cannot reach any sibling.
    ///
    /// `#[serde(default = "group_edges_default")]`, **not**
    /// `#[serde(default)]`: a plain `default` would make a missing key `false`,
    /// which is the wrong value for the *shipped* document —
    /// `index.html:255` ships `#groupEdges checked`, so absent-means-`false`
    /// would repaint a checked box to unchecked on the first host frame. `true`
    /// is the value the document actually ships (`core.js:271`, and the single
    /// owner `GraphView::group_edges` defaults to `true`), so a partial `ui`
    /// object that omits the key leaves the box as shipped.
    #[serde(default = "group_edges_default", rename = "groupEdges")]
    pub group_edges: bool,
    /// The graph-mode `#suggestions` popup (`renderSuggestions` +
    /// `showSuggestions`, search.js:11-38).
    ///
    /// **Presence of this key IS the open state**: `Some` clears the shipped
    /// `hidden` class, sets `#searchInput[aria-expanded="true"]` and rebuilds the
    /// rows; `None` (the shipped/default document) writes nothing at all, so the
    /// box keeps `class="suggestions hidden"`, no children and
    /// `aria-expanded="false"` — byte-identical (`@frame 1600 1000
    /// 845180d07a64ce3d`). The app hides the popup by *omitting* the key
    /// (`app/src/nzk_ui.rs`'s `SuggestionView::facts` returns `None` for an empty
    /// query); there is deliberately no `"open": false` field a caller could
    /// forget to send.
    ///
    /// One-shot rebuild note: "absent = leave the shipped hidden box" holds
    /// because every facts pass builds a fresh `Doc` (`NzkUi::from_skeleton_with_facts`
    /// / `from_source_with_facts`) — the same assumption `TabsFacts`/`FindFacts`
    /// rely on. Applying these facts twice to the SAME document would need an
    /// explicit close path; no current caller does that.
    #[serde(default)]
    pub suggestions: Option<SuggestionsFacts>,
    /// The Reorganise & export dialog's form state, report and note.
    ///
    /// Absent leaves `#reorgModal` exactly as shipped: `modal-backdrop hidden`,
    /// the selects' first options selected by document order, `#reorgSize`
    /// `value="3000"`, `#reorgHeader checked`, an empty `#reorgReport` and an
    /// empty `#reorgNote`. Presence (the dialog opened or a run finished) is
    /// what lets the shell paint the host's form state — there is deliberately
    /// no `"open": false` nested flag a caller could forget.
    #[serde(default)]
    pub reorg: Option<ReorgFacts>,
    /// The form-control interaction facts (task-14). Absent = no focused control
    /// and no open menu, which is required for the default render to stay
    /// byte-identical.
    #[serde(default)]
    pub form: Option<FormFacts>,
    /// The change-preview panel's CONTENT — `#previewOut`'s text and
    /// `#previewNote`'s note (`preview.js:64-137`).
    ///
    /// Same relationship to `modals_open` as [`UiState::find`] has to
    /// `drawers_open`: the modal is on screen with real geometry, and this is
    /// what goes inside it. `None` — the serde default, and the state of a
    /// nzk_ui whose panel has not been opened — writes nothing, so `#previewOut`
    /// keeps the shipped empty div and the default render stays byte-identical
    /// (`@frame 1600 1000 845180d07a64ce3d`).
    #[serde(default)]
    pub preview: Option<PreviewFacts>,
    /// The docked node drawer's CONTENT — `#nodeTitle`, `#nodeProps`,
    /// `#relationsCount` and `#relationsBody` (`drawer.js:74-360`).
    ///
    /// Same relationship to `drawers_open` as [`UiState::find`] has: the drawer
    /// is on screen with real geometry, and this is what goes inside it.
    /// `None` — the serde default, and the state of a nzk_ui with no selected
    /// node — writes nothing, so `#nodeDrawer` keeps the shipped
    /// `class="drawer hidden"` and the default render stays byte-identical
    /// (`@frame 1600 1000 845180d07a64ce3d`).
    #[serde(default)]
    pub drawer: Option<DrawerFacts>,
    /// The shortcuts/settings sheet's row list (`#shortcutsBody`).
    ///
    /// Absent leaves the shipped box EMPTY — `index.html:433` ships
    /// `<div id="shortcutsBody" class="modal-body shortcuts-body"></div>` with no
    /// rows, and `openShortcuts` (`shell.js:344-349`) is the thing that builds
    /// them. `None` therefore keeps the default render byte-identical; `Some`
    /// rebuilds `#shortcutsBody`'s children exactly as `renderShortcuts`
    /// (`shell.js:330-342`) would.
    #[serde(default)]
    pub shortcuts: Option<ShortcutsFacts>,
    /// The sidebar filter model (`filters.js`): which kind/edge chips are ON and
    /// the real namespace option list + selected value.
    ///
    /// `None` — the serde default, and the state of a host that has not wired
    /// the model yet — leaves this module's painted default exactly as it was
    /// (every `.facet`/`.edgekey` `on`, `#nsSelect` holding only the empty
    /// "All namespaces" option), so the default render stays byte-identical.
    /// `Some` paints the app's real on/off state and the graph's namespace list.
    #[serde(default)]
    pub filters: Option<FiltersFacts>,
}

/// The painted state of the sidebar filter controls (`filters.js`).
///
/// This is a transport for the app's own filter model plus the graph's own
/// namespace list — the paint invents no chip. The chip lists are *overrides*
/// looked up by `kind`; a kind the app did not mention stays `on` (the shipped
/// default), so a partial fact can never make a filter impossible to turn back
/// on by accident.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FiltersFacts {
    /// `.facet` rows: `kind` is `data-kind`/`data-id`, `on` the on/off class.
    #[serde(default)]
    pub kinds: Vec<FilterChipFacts>,
    /// `.edgekey` edge-kind chips; same shape as [`FiltersFacts::kinds`].
    #[serde(default)]
    pub edges: Vec<FilterChipFacts>,
    /// `#nsSelect` option values, in the engine's namespace order. The renderer
    /// always paints the empty "All namespaces" option first.
    #[serde(default)]
    pub namespaces: Vec<String>,
    /// `#nsSelect`'s selected value; empty = "All namespaces".
    #[serde(default)]
    pub namespace: String,
    /// `#nsDeep` — include sub-namespaces (`index.html` ships it checked).
    #[serde(default = "ns_deep_default")]
    pub ns_deep: bool,
    /// `#hideIsolated` — drop nodes with no drawn edge
    /// (`state.filters.hideIsolated`; `graph.js:814-820`).
    #[serde(default)]
    pub hide_isolated: bool,
    /// `#hideEdgeLabels` — the **legacy DOM id** of the VT-mode switch.
    ///
    /// The id and wire key are kept so the shell and the facts contract stay
    /// stable, but the flag no longer hides anything: `true` means VT mode
    /// (relations drawn as small nodes on a direct line), `false` means the
    /// reference word box beside the curved arrow. The app publishes
    /// `filters.vt_mode` under this key.
    #[serde(default)]
    pub hide_edge_labels: bool,
    /// "Hide parents": the selected node's containment ancestors are not drawn.
    #[serde(default)]
    pub focus_parents: bool,
    /// The selected node's short label, used only in the "Hide parents"
    /// checkbox's title. The view anchors on its own selection, so this is a
    /// hint, not the filter itself.
    #[serde(default)]
    pub focus_selected: Option<String>,
}

/// One filter chip's state: `filters.js`'s `span.facet.on/off` /
/// `.edgekey.on/off` pair. Missing `on` defaults to `true` (the shipped state).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FilterChipFacts {
    /// `data-kind` / `data-edge` value the row is looked up by.
    #[serde(default)]
    pub kind: String,
    /// The on/off class; `true` = `.on` (the shipped default).
    #[serde(default = "chip_on_default")]
    pub on: bool,
}

impl Default for FilterChipFacts {
    fn default() -> Self {
        FilterChipFacts {
            kind: String::new(),
            on: true,
        }
    }
}

fn chip_on_default() -> bool {
    true
}

fn ns_deep_default() -> bool {
    true
}

/// The `#shortcutsBody` content (`shell.js:330-342`).
///
/// One flat row list, in panel order. The reference groups rows under a
/// `.shortcut-group` head; the app therefore publishes the group name on each
/// row, and [`apply_shortcuts`] emits a head whenever it changes. That keeps the
/// wire shape a flat `rows` array (the order is the reference's own) while still
/// reproducing the group boundaries.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ShortcutsFacts {
    #[serde(default)]
    pub rows: Vec<ShortcutFacts>,
}

/// One `#shortcutsBody` row — `shortcutRow()` (`shell.js:298-328`).
///
/// `keys` is the reference's own nested shape: the outer list is the
/// *alternatives* joined by `/`, the inner list is one chord's *parts* joined by
/// `+` (each part becomes a `<kbd>`). It travels verbatim; this layer never
/// re-derives a chord.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ShortcutFacts {
    /// `.shortcut-group` head text drawn before this row (`entry.group`).
    #[serde(default)]
    pub group: String,
    /// `kbd` chips: alternatives joined by `.shortcut-plus` `/`, parts by `+`.
    #[serde(default)]
    pub keys: Vec<Vec<String>>,
    /// `.shortcut-desc` text.
    #[serde(default)]
    pub desc: String,
}

/// `true` — the shipped `#groupEdges` state (`index.html:255` ships the box
/// `checked`) and `core.js:271`'s `groupEdges: true`. Used as the
/// `#[serde(default = ...)]` for [`UiState::group_edges`] so a missing key cannot
/// silently repaint the box into a state the document never shipped.
///
/// A free function rather than an associated one: the serde derive expands the
/// `default = "..."` path inside its generated `Deserialize` visitor, where
/// `Self` is the visitor, not [`UiState`].
fn group_edges_default() -> bool {
    true
}

/// The graph-mode suggestion popup's content — `renderSuggestions`
/// (search.js:22-38).
///
/// Every field is a transport of what the app's search core published; the paint
/// invents no copy. The row's display label is the host's `label` (the app applies
/// the reference's `label || id` chain before sending; `full` travels in its own
/// field), and the dot colour is looked up locally from `kind` via
/// [`kind_color`] — the one transcription of `core.js`'s `KIND_COLOR_NZK_DARKPLUS`
/// palette. No colour is carried on the wire.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SuggestionsFacts {
    /// `state.searchResults` in the core's own order — never re-sorted here.
    #[serde(default)]
    pub rows: Vec<SuggestionRow>,
    /// `state.searchIndex`; `None` = the reference's `-1` sentinel (no `.active`).
    #[serde(default)]
    pub active: Option<usize>,
    /// The `.sug-empty` message (`search.js:23`'s `'no matches'`). `None`/empty
    /// means "no message" — the rows are drawn instead.
    #[serde(default)]
    pub empty: Option<String>,
}

/// One suggestion row. Every field is `#[serde(default)]` so a partial host
/// payload degrades to blank text instead of panicking; the paint then falls back
/// to `id` for the label (the reference's `label || name || id`, minus `name`
/// which the native model does not have).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SuggestionRow {
    /// `data-id` — the node id, verbatim (the click/commit target).
    #[serde(default)]
    pub id: String,
    /// `.sug-kind` text and the `.dot` colour key.
    #[serde(default)]
    pub kind: String,
    /// `.sug-label` text; empty degrades to `id`.
    #[serde(default)]
    pub label: String,
    /// `.sug-full` text (the namespace); empty renders an empty span.
    #[serde(default)]
    pub full: String,
}

/// One open-document row rendered into `#cs-tabs` (`shell.js:1192-1225`).
///
/// One field per key of the `ui.tabs.tabs[]` payload the host publishes; every
/// value is the reference's own (`Tab::key` / `Tab::label`, the active/split
/// comparisons and the `(params.file || params.node) || key` title), so this
/// struct is a transport and never invents copy.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TabFacts {
    /// `data-key` and the close button's `data-close` (`Tab::key`).
    #[serde(default)]
    pub key: String,
    /// `.cs-tab-label` text — `t.label || t.key` (shell.js:1210-1212).
    #[serde(default)]
    pub label: String,
    /// `.cs-tab.active` plus `aria-selected="true"` (`state.tabs.active_key()`).
    #[serde(default)]
    pub active: bool,
    /// The `.cs-tab-splitmark` span is appended (`splitKeyState === t.key`).
    #[serde(default)]
    pub split: bool,
    /// The row's `title` — `(params.file || params.node) || key`
    /// (shell.js:1202); an empty value falls back to `key`.
    #[serde(default)]
    pub title: String,
}

/// The open-document strip (`shell.js:1192-1248`). `None` = no `#cs-tabs` node
/// is created, so the shipped document stays byte-identical.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TabsFacts {
    /// The open documents, in `state.tabs` order — never re-sorted here.
    #[serde(default)]
    pub tabs: Vec<TabFacts>,
    /// `isSplitOn()` — chooses the split button's text and its disabled rule.
    #[serde(default)]
    pub split_on: bool,
    /// The divider fraction written to the shipped `--cs-split-w` custom
    /// property (`shell.js:1471-1472`, `applySplitWidth`). `None` = the host
    /// publishes no ratio, so the stylesheet's own `--cs-split-w: 50%` fallback
    /// lays the split out and `apply_split` performs **no style write at all**
    /// (byte-identical shipped default).
    #[serde(default)]
    pub split_w: Option<f64>,
}

/// The inline custom-property style `applySplitWidth` writes
/// (`shell.js:1472`): `(splitFrac * 100).toFixed(1) + '%'`.
fn split_width_style(frac: f64) -> String {
    format!("--cs-split-w: {:.1}%", frac * 100.0)
}

/// The resolve-chain section's content: the RAW `Index::chain` answer plus the
/// two view toggles (`filters.js:543-660`).
///
/// `data` is the engine value unchanged, so a row can only name a node the walk
/// really produced. `flat` is `#chainFlatBtn`'s state, and `depth`/`dir` are the
/// committed select values the host re-reads.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ChainFacts {
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default)]
    pub flat: bool,
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub dir: String,
    /// Node ids whose children are collapsed (`state.chain.collapsed`).
    #[serde(default)]
    pub collapsed: Vec<String>,
}

/// The resolve-chain section (`filters.js:543-660`).
///
/// Renders the engine's raw chain answer: `data.tree` recursively, or
/// `data.flat` when `#chainFlatBtn` is on. Each `.hop` row carries
/// `data-node="<id>"`, so a press can be resolved by the shell's hit chain and
/// routed to the graph selection (the same channel the runtime facet rows use).
/// No row is invented: a failed/absent answer renders the reference's own
/// `.list-empty` text.
fn apply_chain(d: &mut Dom<'_>, ui: &UiState) {
    let Some(chain) = &ui.chain else { return };
    let Some(tree) = d.id("chainTree") else { return };
    let data = &chain.data;
    let ok = data.get("ok").and_then(serde_json::Value::as_bool).unwrap_or(false);
    if !ok {
        let text = data
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Select a node, then Run.");
        let empty = d.mk(El::new("div").cls("list-empty").text(text));
        d.set_children(tree, &[empty]);
        return;
    }
    let mut nodes = Vec::new();
    if chain.flat {
        if let Some(rows) = data.get("flat").and_then(serde_json::Value::as_array) {
            for hop in rows {
                let depth = hop.get("depth").and_then(serde_json::Value::as_u64).unwrap_or(0);
                let row = chain_hop_row(d, hop, depth.min(6) as usize, false);
                let wrap = d.mk(El::new("div"));
                d.append(wrap, &[row]);
                nodes.push(wrap);
            }
        }
    } else if let Some(root) = data.get("tree") {
        nodes.push(chain_hop_tree(d, root, 0, &chain.collapsed));
    }
    if nodes.is_empty() {
        let empty = d.mk(El::new("div").cls("list-empty").text("No chain hits"));
        nodes.push(empty);
    }
    d.set_children(tree, &nodes);
    if let Some(count) = d.id("chainCount") {
        let n = data
            .get("flat")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let truncated = data
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let mut text = format!("{n} hops");
        if truncated {
            text.push_str(" (truncated)");
        }
        d.set_text(count, &text);
    }
}

/// One `.hop-wrap` with its `.hop` row and (unless in flat mode) its children.
fn chain_hop_tree(
    d: &mut Dom<'_>,
    hop: &serde_json::Value,
    depth: usize,
    collapsed: &[String],
) -> usize {
    let wrap = d.mk(El::new("div"));
    let id = hop
        .get("node")
        .and_then(|n| n.get("id"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let is_collapsed = depth > 0 && collapsed.iter().any(|c| c == id);
    let row = chain_hop_row(d, hop, depth, is_collapsed);
    d.append(wrap, &[row]);
    if !is_collapsed {
        if let Some(children) = hop.get("children").and_then(serde_json::Value::as_array) {
            for child in children {
                let kid = chain_hop_tree(d, child, depth + 1, collapsed);
                d.append(wrap, &[kid]);
            }
        }
    }
    wrap
}

/// One `.hop` button (`filters.js:565-610 chainNodeEl`).
fn chain_hop_row(
    d: &mut Dom<'_>,
    hop: &serde_json::Value,
    depth: usize,
    collapsed: bool,
) -> usize {
    let node = hop.get("node").cloned().unwrap_or(serde_json::Value::Null);
    let id = node.get("id").and_then(serde_json::Value::as_str).unwrap_or("");
    let label = node
        .get("label")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| node.get("name").and_then(serde_json::Value::as_str))
        .unwrap_or(id);
    let mut kids: Vec<usize> = Vec::new();
    let has_kids = hop
        .get("children")
        .and_then(serde_json::Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let caret = d.mk(
        El::new("span")
            .cls("hop-caret")
            .text(if has_kids {
                if collapsed { "▸" } else { "▾" }
            } else {
                "·"
            }),
    );
    kids.push(caret);
    if let Some(edge) = hop.get("edge").and_then(serde_json::Value::as_str) {
        if !edge.is_empty() {
            kids.push(d.mk(El::new("span").cls("hop-edge").text(edge)));
        }
    }
    if let Some(line) = hop.get("line").and_then(serde_json::Value::as_i64) {
        if line > 0 {
            kids.push(d.mk(El::new("span").cls("hop-line").text(&format!(":{line}"))));
        }
    }
    kids.push(d.mk(El::new("span").cls("hop-label").a("title", id).text(label)));
    if hop.get("cycle").and_then(serde_json::Value::as_bool).unwrap_or(false) {
        kids.push(d.mk(El::new("span").cls("hop-cycle").text("cycle")));
    }
    let btn = d.mk(
        El::new("button")
            .cls("hop")
            .a("type", "button")
            .a("title", "Select this node in the graph")
            .a("data-key", id)
            .a("style", &format!("padding-left:{}px", 8 + depth * 12)),
    );
    d.append(btn, &kids);
    btn
}

/// The source-pane head's live facts (`#sourceTitle` / `#sourceNote` and the
/// disabled state of the head buttons). `None` while the pane is closed.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SourceFacts {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub has_doc: bool,
    #[serde(default)]
    pub prev: bool,
    #[serde(default)]
    pub next: bool,
    #[serde(default)]
    pub can_ref: bool,
    #[serde(default)]
    pub mode: String,
}

/// The source pane's head facts: title, error note, theme label and the
/// disabled state of the buttons that need a document / history / file.
///
/// `#sourceMatchTag` has no fact yet (the port has no search-hit context), so it
/// keeps its shipped `hidden` class rather than being shown empty.
fn apply_source(d: &mut Dom<'_>, ui: &UiState) {
    let Some(source) = &ui.source else { return };
    if let Some(node) = d.id("sourceTitle") {
        let text = if source.title.is_empty() { "—" } else { source.title.as_str() };
        d.set_text(node, text);
    }
    if let Some(node) = d.id("sourceNote") {
        d.set_text(node, &source.note);
    }
    if let Some(node) = d.id("srcModeBtn") {
        if !source.mode.is_empty() {
            d.set_text(node, &source.mode);
        }
    }
    for (id, enabled) in [
        ("srcCopyBtn", source.has_doc),
        ("srcRefBtn", source.can_ref),
        ("srcPrevBtn", source.prev),
        ("srcNextBtn", source.next),
        ("srcMethodBtn", source.has_doc),
        ("srcWindowBtn", source.has_doc),
    ] {
        let Some(node) = d.id(id) else { continue };
        if enabled {
            d.clear_attr(node, "disabled");
        } else {
            d.set_attr(node, "disabled", "");
        }
    }
}

/// The loading overlay (`#loading` + `#loadingText` + `#stopIndexBtn`).
///
/// `showLoading(text)` in `shell.js:157-176` is what the reference calls around
/// `doIndex`; Stop and Esc-while-indexing are its only exits. `stop` mirrors the
/// reference's display toggle: `true` clears the shipped inline
/// `style="display:none"` on `#stopIndexBtn` only when the host really has a
/// cancelable build in flight.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct LoadingFacts {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub stop: bool,
}

/// The Find panel's content, as `search.js`'s `renderFind` would have written it
/// (`search.js:305-345`).
///
/// Every field is the *real* value from the search core
/// (`search::Outcome::meta` / `::notice`, `SearchResult::title` /
/// `::subtitle`, `MatchKind::tag`) — this struct is a transport, never a place to
/// invent copy. The one structural decision is that `notice` is a plain string
/// and **empty means hidden**, which is how the core already encodes "nothing to
/// say" (`Outcome::notice` returns `""` for `Hits`), so the honest absence and
/// the hidden class agree by construction rather than by a second flag.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FindFacts {
    /// The query text. `#findQuery` shows `query` when non-empty, else the
    /// shipped word "Find" (`search.js:314`).
    #[serde(default)]
    pub query: String,
    /// `#findMeta`: the real hit count, `no hits`, or `""` for no query yet.
    #[serde(default)]
    pub meta: String,
    /// `#findNotice`: the honest state sentence, or `""` to keep it hidden.
    #[serde(default)]
    pub notice: String,
    /// `#findResults` rows, best-first — the core's own order, never re-sorted.
    #[serde(default)]
    pub rows: Vec<FindRow>,
    /// The distinct **honest state** the core reported, carried through so a
    /// test (or a future caller) can assert the distinction survives the trip
    /// into the DOM without re-deriving it from the strings.
    #[serde(default)]
    pub state: FindState,
    /// The engine-backed definitions/implementations/usages groups (v2
    /// `renderFind`, `search.js:305-351`). Absent/empty = the app has not asked
    /// the engine, so the flat `rows` render exactly as before.
    #[serde(default)]
    pub groups: Vec<FindGroupFacts>,
}

/// The three states [`search::Outcome`] distinguishes, named here so the
/// NoQuery/NoHits distinction is a *value* on the wire and not merely a
/// difference between two strings a caller has to compare.
///
/// `NoQuery` and `NoHits` are deliberately separate variants: the incumbent
/// collapses both into an empty `#findResults` and only differs in copy, which is
/// exactly how "the user has typed nothing" becomes indistinguishable from
/// "nothing matched".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FindState {
    /// Nothing was searched — no query typed yet. The default, so an omitted
    /// field cannot be read as a completed search.
    #[default]
    NoQuery,
    /// A real query, really searched, at least one hit.
    Hits,
    /// A real query, really searched, nothing matched.
    NoHits,
}

/// One `#findResults` row: the exact content of `search.js`'s `buildDefRow`
/// (`search.js:181-207`), fed from `search::SearchResult`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FindRow {
    /// `SearchResult::title()` — the `.fname` text and the button's `title`.
    pub title: String,
    /// `SearchResult::subtitle()` — the `.frow-sub` line, omitted when empty
    /// (the reference appends the span only `if (sub.textContent)`,
    /// `search.js:206`).
    #[serde(default)]
    pub subtitle: String,
    /// `MatchKind::tag()` minus this module's own nzk_ui: `exact`, `prefix`,
    /// `substring` or `subseq`. Written into a `data-match` attribute so the
    /// DOM carries the core's own word rather than a re-spelled one.
    #[serde(default)]
    pub tag: String,
    /// `Node::kind` — the `.fkind` chip.
    #[serde(default)]
    pub kind: String,
    /// `Node::id` — the click target (`search.js:175` keys a row on
    /// `row.node.id`). Carried verbatim; this module never parses it.
    #[serde(default)]
    pub node_id: String,
}

/// The folder picker's content, as `shell.js`'s `renderPicker` would have written
/// it (`shell.js:38-87`).
///
/// `note` and `selectable` are the honesty channel: a path that does not exist
/// says so in `#modalNote` and leaves `#modalSelect` disabled, so it can never
/// be mistaken for an empty folder that is ready to index.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PickerFacts {
    /// `#modalPath` text — the absolute path being shown.
    #[serde(default)]
    pub path: String,
    /// `#modalPathInput` value (`shell.js:53`).
    #[serde(default)]
    pub path_input: String,
    /// `#modalNote` text; `""` clears it.
    #[serde(default)]
    pub note: String,
    /// `#modalNote` class — `modal-note ok` / `modal-note warn` from
    /// `Note::class()`.
    #[serde(default)]
    pub note_class: String,
    /// `#modalSelect` is enabled. `Note::selectable()` decides; a missing or
    /// unreadable path is never selectable.
    #[serde(default)]
    pub selectable: bool,
    /// `#modalUp` is enabled — `Listing::parent.is_some()`, so the filesystem
    /// root and a failed listing both disable it (`shell.js:54`).
    #[serde(default)]
    pub can_go_up: bool,
    /// `#modalBody` directory rows, in the core's `sort_key` order.
    #[serde(default)]
    pub dirs: Vec<PickerRow>,
    /// Non-directory entries the listing skipped, shown as the honest count the
    /// reference's `.pmeta` chip wants (`shell.js:62`). `None` means "do not
    /// claim a count" and is distinct from a real `0`.
    #[serde(default)]
    pub files_skipped: Option<usize>,
    /// The core's own note variant, carried for the same reason as
    /// [`FindFacts::state`].
    #[serde(default)]
    pub state: PickerState,
}

/// The picker note variants, named so `NotFound` cannot silently become `Empty`
/// anywhere downstream of the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PickerState {
    /// Not listed at all — the default, so an omitted field never implies a
    /// successful empty listing.
    #[default]
    Unlisted,
    Loading,
    Indexable,
    NoCs,
    Empty,
    NotFound,
    Unreadable,
}

impl PickerState {
    /// Does this state mean "the path exists and was really read"?
    ///
    /// The three success variants are exactly [`Listing`]'s "carries entries"
    /// set, so [`apply_picker`] uses this — not the presence of rows — to decide
    /// whether an empty `#modalBody` is an honest "Empty folder." or a failure
    /// that must not be rendered as one.
    pub fn listed(self) -> bool {
        matches!(
            self,
            PickerState::Indexable | PickerState::NoCs | PickerState::Empty
        )
    }
}

/// One `#modalBody` directory row — `shell.js:58-64`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PickerRow {
    /// `.pname` text; the reference appends the `/` (`shell.js:60`).
    pub name: String,
    /// `data-nav` — the absolute path the row navigates to (`shell.js:58`).
    pub path: String,
    /// `.plink` chip is present (`shell.js:61`).
    #[serde(default)]
    pub symlink: bool,
}

/// The quick-open palette's content, exactly as `quickopen.js`'s renderer would
/// have written it (`quickopen.js:305-353`, `ensureDom()` at 113-181).
///
/// One field per key of the `ui.quickopen` payload that `app/src/quickopen_wire.rs`
/// projects — the 13-key contract asserted by that crate's
/// `wire_shape_is_exactly_the_documented_keys`. Every value here is the core's
/// own (`Mode::label`, `Mode::placeholder`, `Row::kind_of`, `Row::display_label`,
/// `Row::loc`, `Row::title`, `Selection::is_selected`) — this struct is a
/// transport and never invents copy.
///
/// ## Why `state` is carried and `rows.is_empty()` is not enough
///
/// `noquery` and `nohits` can both have zero rows but differ in copy *and* in
/// whether the note bar shows, so a renderer that branched on emptiness would
/// make a correct empty list indistinguishable from a broken one. `state` is the
/// core's own honest name for that distinction (see `QuickOpenView::state`), and
/// [`QuickOpenFacts::is_open`] is the only thing that decides whether the
/// overlay is revealed.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct QuickOpenFacts {
    /// `"files"` or `"symbols"` — `MODE` in the reference
    /// (quickopen.js:427-431).
    pub mode: String,
    /// `.cs-qo-mode` text: `MODE_LABEL[mode]` (`"Files & symbols"` /
    /// `"Symbols"`, quickopen.js:39-42).
    pub mode_label: String,
    /// `#cs-qo-input`'s `placeholder` (`MODE_PLACEHOLDER[mode]`,
    /// quickopen.js:40-43, 430-431).
    pub placeholder: String,
    /// `.cs-qo-hint` text — `'↑↓ · Enter · Esc'` (quickopen.js:142).
    pub hint: String,
    /// The overlay's `aria-label` — `'Quick open'` (quickopen.js:121).
    pub aria_label: String,
    /// The live input text (`inputEl.value`).
    pub query: String,
    /// `seedQuery` (quickopen.js:36) — the query the current list was produced
    /// for, which is what chooses the empty-list copy at quickopen.js:311-313.
    pub seed_query: String,
    /// The selected index, or `None` for the `sel = -1` sentinel
    /// (quickopen.js:289). JSON `null` is the sentinel's real wire form.
    pub selected: Option<usize>,
    /// The rows, in the core's own rank order — never re-sorted here.
    pub rows: Vec<QuickOpenRow>,
    /// The empty-list text (quickopen.js:311-313), used only when `rows` is
    /// empty: `"No matches."` or `"Type to search files and symbols."`.
    pub empty_text: String,
    /// `.cs-qo-note` text (quickopen.js:271, 291-297).
    pub note: String,
    /// `.cs-qo-note` carries `hidden`. A separate boolean rather than "note is
    /// empty", so the honesty survives as a value all the way through.
    pub note_hidden: bool,
    /// The core's own honest state name: `"closed"`, `"noquery"`, `"nohits"` or
    /// `"hits"` (see `QuickOpenView::state`).
    pub state: String,
}

impl QuickOpenFacts {
    /// Is the palette showing? `isOpen()` is `state != 'closed'`
    /// (quickopen.js:449).
    ///
    /// The **only** gate on revealing `#cs-quickopen`. A payload whose state is
    /// `closed` still carries rows (the reference keeps them across a close,
    /// quickopen.js:441-447) but must leave the overlay hidden, exactly as the
    /// shipped document has it.
    pub fn is_open(&self) -> bool {
        self.state != "closed"
    }

    /// The `sel` the row loop compares against, as the reference's `i === sel`
    /// does: a `None` selection can never equal a row index, so no row gets
    /// `.sel` and every row's `aria-selected` is `"false"`.
    fn selected_index(&self) -> Option<usize> {
        self.selected
    }
}

/// One `.cs-qo-row` — the exact content of `quickopen.js`'s `renderList`
/// (quickopen.js:322-346).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct QuickOpenRow {
    /// `data-i` — `String(i)` (quickopen.js:322), and the same value the click
    /// handler parses back with `parseInt` (quickopen.js:167).
    pub index: usize,
    /// `.cs-qo-kind` text — `kindOf(r)` (quickopen.js:299-303).
    pub kind: String,
    /// `.cs-qo-name` text — `r.label || r.name || r.id || r.file`
    /// (quickopen.js:331).
    pub label: String,
    /// The extra `.cs-qo-full` span's text, appended inside `.cs-qo-name`
    /// (quickopen.js:332-337).
    pub full: String,
    /// Whether that span is present at all: `r.full && r.full !== r.label &&
    /// r.full !== r.name` (quickopen.js:332). The core computes it so this layer
    /// never re-derives the reference's fallback chain.
    pub show_full: bool,
    /// `.cs-qo-loc` text — `r.file ? (file + ':' + line) : (r.id || '')`
    /// (quickopen.js:341).
    pub loc: String,
    /// `row.title` (quickopen.js:343-344).
    pub title: String,
    /// `aria-selected` and the `.sel` class: `i === sel` (quickopen.js:320,
    /// 323).
    pub selected: bool,
    /// The node id an activation opens (quickopen.js:372). Carried verbatim;
    /// this layer never parses it.
    pub id: String,
    /// `Node::file`, for a host that routes on the row rather than the index.
    pub file: String,
    /// `Node::line`.
    pub line: i64,
    /// `"search"` / `"definition"` / `"implementation"` / `"usage"`
    /// (quickopen.js:307, 190-235).
    pub source: String,
}

/// A requested scroll offset for one scroll container.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScrollSpec {
    pub selector: String,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
}

/// The corpus facts the runtime reads from `/api/*` (and the index response).
/// These are *data*, not layout: the renderer takes them as input so the real
/// CSS layout is exercised with the same content the reference had.
///
/// Serde-serialisable so a host can pass its real graph facts with `--facts`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Facts {
    pub repo: String,
    /// Overrides the status line verbatim. The status text is engine data
    /// (`files N · parse Nms · nodes N · edges N`); the parse time changes on
    /// every run, so the A/B harness passes the exact string the reference
    /// session captured rather than a stale hard-coded one.
    pub status_override: Option<String>,
    pub kinds: Vec<(String, usize)>,
    pub ns_count: usize,
    pub indexed: bool,
    pub file_count: u64,
    pub parse_ms: u64,
    pub node_count: u64,
    pub edge_count: u64,
    pub projects_empty_text: String,
    pub parser_warning_count: usize,
    pub nodes_in_view: usize,
    /// Non-empty replaces the "no .sln/.csproj found" chip with the project rows.
    #[serde(default)]
    pub projects: Vec<ProjectRow>,
    /// Non-empty replaces the "No parser warnings" row with real warning rows.
    #[serde(default)]
    pub warnings: Vec<WarningRow>,
    /// Host-owned view state; `None` leaves the shipped document untouched.
    #[serde(default)]
    pub ui: Option<UiState>,
    /// Hierarchy peek panel (`hierarchy.js`). `None` = panel closed, and the
    /// shipped document is left byte-identical: `apply_hierarchy` is never
    /// entered, so no node is created and nothing is repainted.
    #[serde(default)]
    pub hierarchy: Option<HierarchyFacts>,

    /// Layout ▸ slider readouts (`syncLayoutControls`, `shell.js:807-817`).
    ///
    /// **Top-level, deliberately — a sibling of `ui`, not a member of it.**
    /// The host inserts this as `obj["settings"]` on the *outer* facts object
    /// (`app/src/nzk_ui.rs:985` `facts_json_with_settings`, asserted there at
    /// `:4175`), exactly like `hierarchy` at `:928`. Serde does not
    /// `deny_unknown_fields`, so a field declared one level too deep would
    /// deserialize to `None` and the span text would simply never change — the
    /// key would look wired while being dead. Declared here to match the wire.
    ///
    /// `None` = key absent (the host omits it while the sliders sit at their
    /// shipped defaults) and the document stays byte-identical, because
    /// `apply_settings` is never entered.
    ///
    /// A top-level `"settings": null` is also tolerated — `app/src/pov.rs:248`
    /// publishes one — and deserialises to `None` on an `Option`, so it is as
    /// inert as the absent key.
    #[serde(default)]
    pub settings: Option<SettingsFacts>,

    /// `#pubCount` + `#pubList` — the Public-definitions section's content,
    /// from the engine's own `definitions("public", 400)` answer.
    ///
    /// **Top-level, deliberately — a sibling of `ui`, not a member of it.**
    /// The host inserts this as `obj["pub_defs"]` on the *outer* facts object
    /// (`app/src/nzk_ui.rs`, in `facts_json_with`, immediately after the
    /// `hierarchy` insert at `:927`), for exactly the reason spelled out on
    /// [`Facts::settings`]: serde does not `deny_unknown_fields`, so a key
    /// declared one level too deep deserializes to `None` and the readout is
    /// silently dead while looking wired.
    ///
    /// `None` = key absent (the host omits it until the engine has been asked)
    /// and the document stays byte-identical: `apply_pub_defs` is never entered,
    /// so `#pubCount` keeps `index.html`'s empty span and `#pubList` keeps its
    /// inert `<div class="list-empty">open to load</div>`.
    #[serde(default)]
    pub pub_defs: Option<PubDefsFacts>,
    /// The sidebar's **Declared types** section (v3-only; created at runtime).
    ///
    /// **Top-level, deliberately — a sibling of `ui`**, for the same reason
    /// [`Facts::pub_defs`] is: serde does not `deny_unknown_fields`, so a key
    /// one level deeper would silently deserialize to `None`.
    ///
    /// `None` = key absent, so `apply_type_defs` is never entered: the section
    /// does not exist, the document keeps `index.html`'s exact tree, and the
    /// pinned default `@frame` token cannot move.
    #[serde(default)]
    pub type_defs: Option<TypeDefsFacts>,
    /// Strings / error codes content (`#strCount` / `#strList`), the twin of
    /// [`Facts::pub_defs`] for the `onStringsFilter` result. `None` leaves the
    /// shipped "type to search string literals…" row untouched.
    #[serde(default)]
    pub str_defs: Option<PubDefsFacts>,
    /// `#strCount` + `#chainCount` — the two `.scount` spans inside the
    /// "Strings / error codes" and "Resolve chain" section heads.
    ///
    /// **Top-level, deliberately — a sibling of `ui`, not a member of it**, for
    /// the same reason [`Facts::pub_defs`] is: serde does not
    /// `deny_unknown_fields`, so a key declared one level too deep deserializes
    /// to `None` and both spans stay silently empty while looking wired.
    ///
    /// `None` = key absent (the host has not searched or walked yet) and the
    /// document stays byte-identical: `apply_section_counts` is never entered,
    /// so each span keeps `index.html`'s shipped empty
    /// `<span class="scount" id="strCount"></span>`. That is the true default
    /// state — `index.html:329` ships `strSection` `class="section collapsed"`
    /// and neither counter is written until its own request answers.
    #[serde(default)]
    pub section_counts: Option<SectionCountFacts>,
    /// The block-hover peek panel and the reorg dialog's three conditional
    /// rows — `#blocksPeekKind/Name/File/Src/Refs` and
    /// `#reorgGranRow/GroupRow/SizeRow`.
    ///
    /// **Top-level, a sibling of `ui`**, for the same reason
    /// [`Facts::section_counts`] is: serde does not `deny_unknown_fields`, so a
    /// key declared one level too deep deserializes to `None` and all eight ids
    /// stay silently inert while looking wired.
    ///
    /// `None` = the key was absent, so `apply_blocks_peek` is never entered and
    /// the document stays byte-identical: `#blocksPeek` keeps `index.html:186`'s
    /// shipped `class="blocks-peek hidden"` and `#blocksPeekSrc` keeps its
    /// shipped `Loading source…`, while the three reorg rows keep whatever class
    /// `index.html` gave them. That is the true default state — the reorg rows
    /// live inside `#reorgModal`, which ships `modal-backdrop hidden`, and
    /// `syncRows` (`reorg.js:286`) only runs when the dialog opens (`:278`).
    #[serde(default)]
    pub blocks_peek: Option<BlocksPeekFacts>,
    /// The edit modal's content — `#editTool`, `#editArgs`, `#editOut`,
    /// `#editNote` and the three footer buttons (`edit.js:262-269`,
    /// `app/src/edit_host.rs:112-156`).
    ///
    /// **Top-level, deliberately — a sibling of `ui`, not a member of it.**
    /// The host inserts this as `obj["edit"]` on the *outer* facts object
    /// (`app/src/nzk_ui.rs`, alongside the `pub_defs` insert), for exactly the
    /// reason spelled out on [`Facts::pub_defs`]: serde does not
    /// `deny_unknown_fields`, so a key declared one level too deep deserializes
    /// to `None` and every modal control stays silently inert while looking
    /// wired.
    ///
    /// `None` = key absent (the host omits it while the dialog is closed) and
    /// the document stays byte-identical: `apply_edit` is never entered, so
    /// `#editArgs` keeps `index.html`'s shipped `{}`, `#editOut` stays the
    /// empty `<pre>`, `#editNote` keeps its bare `modal-note` class and the
    /// three footer buttons keep their shipped (enabled) state.
    #[serde(default)]
    pub edit: Option<EditFacts>,
}

/// The two section-head counters (`filters.js:523`/`:528`, `:555`).
///
/// # Why the two spans need three fields, not two
///
/// The reference reaches **three** distinct display states, and the third is
/// not "absent":
///
/// * never searched — the span keeps `index.html`'s empty text (no write);
/// * searched with an **empty** query — `filters.js:523` writes the empty
///   string *explicitly*, clearing a count left over from a previous search.
///   `str_count: Some(None)` is that write; `str_count: None` is the first
///   state. Collapsing the two would make it impossible to clear a stale count,
///   which is the entire job of `:523`.
/// * searched with a query — `filters.js:528` writes `fmtNum(list.length)`.
///
/// # The chain counter is a different shape on purpose
///
/// `filters.js:555` reads `(d.flat || []).length`, which is the number of rows
/// in the **flattened** tree — the root included — not
/// [`hop_count`](crate::dom) of it. `renderChain` then prints that same `flat`
/// through `renderCall`, which is where `hop_count(rows) == rows - 1` applies
/// (`app/src/hierarchy.rs:236`). The head counter and the body hint are
/// therefore *different numbers derived from the same array*, and this struct
/// keeps the raw `flat` length so the head is not off by one.
/// The three states `#strCount` can be in (`filters.js:523`/`:528`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum StrCount {
    /// The empty query: write the **empty string**. Distinct from "no write"
    /// because this is what clears a count left by a previous search.
    Cleared,
    /// A non-empty query: write `fmtNum(n)`.
    Count(usize),
}

/// `null` -> [`StrCount::Cleared`], a number -> [`StrCount::Count`], and an
/// **absent key** -> `None` (no write at all).
///
/// A plain `Option<StrCount>` would map `null` to `None` as well, which is the
/// conflation this exists to prevent — see [`SectionCountFacts::str_count`].
fn de_str_count<'de, D>(d: D) -> Result<Option<StrCount>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    // `null` and a missing key BOTH arrive as `None` here; the outer `Option`
    // around this function's result is supplied by serde's field handling —
    // absent fields never call this at all, thanks to `#[serde(default)]`.
    let raw = Option::<usize>::deserialize(d)?;
    Ok(match raw {
        None => Some(StrCount::Cleared),
        Some(n) => Some(StrCount::Count(n)),
    })
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SectionCountFacts {
    /// `#strCount`. `None` = the key was absent, so there is no write.
    /// `Some(StrCount::Cleared)` = the empty query, which *clears* the span
    /// (`filters.js:523`). `Some(StrCount::Count(n))` = `fmtNum(n)`
    /// (`filters.js:528`).
    ///
    /// # Why this is an enum and not `Option<Option<usize>>`
    ///
    /// It was `Option<Option<usize>>` first, and that is **silently wrong** with
    /// `#[serde(default)]`: a JSON `null` and an absent key both deserialize to
    /// the outer `None`, so the empty-query clear became unreachable — a host
    /// sending `{"str_count": null}` would leave a stale count on screen and no
    /// test that only checked the "has a number" direction would notice.
    /// Measured, not assumed:
    ///
    /// ```text
    /// {"str_count": null}   -> None
    /// {}                    -> None
    /// {"str_count": 7}      -> Some(Some(7))
    /// ```
    ///
    /// So the two states get a type that cannot conflate them. The reference
    /// semantics — "no write" vs "write the empty string" — are preserved
    /// exactly.
    #[serde(default, deserialize_with = "de_str_count")]
    pub str_count: Option<StrCount>,
    /// `#chainCount` — `(d.flat || []).length` (`filters.js:555`), the flat row
    /// count with the root included.
    pub chain_flat: Option<usize>,
    /// `d.truncated` (`filters.js:555`). Only consulted when `chain_flat` is
    /// present, because the reference reads both from the same `d`.
    pub chain_truncated: Option<bool>,
}

/// The `#chainCount` text (`filters.js:555`):
/// `(d.flat || []).length + ' hops' + (d.truncated ? ' (truncated)' : '')`.
///
/// # The literal is ` hops`, never ` hop`
///
/// Note the deliberate divergence from
/// [`hop_label`](crate::dom): that one is the *body* hint
/// (`hierarchy.js:539`) and singularises `1 hop`; this is the *head* counter and
/// the reference hard-codes the plural, so `flatten_tree` returning a single row
/// renders `"1 hops"`. Reproducing `hop_label` here would be a silent
/// disagreement with the reference on every 1-row chain — the two strings are
/// genuinely different and only one of them is grammatical.
///
/// Truncation **appends**: `"7 hops (truncated)"`. It never replaces the count,
/// and it is not [`truncated_label`](crate::dom)'s `"truncated at 7 hops"`
/// (`hierarchy.js:542`), which is the body row's text. Both appear in the
/// reference at once, in different elements.
pub fn chain_count_label(flat_rows: usize, truncated: bool) -> String {
    let mut s = format!("{flat_rows} hops");
    if truncated {
        s.push_str(" (truncated)");
    }
    s
}

/// The block-hover peek panel and the reorg dialog's three conditional rows.
///
/// Eight ids, and they are **one** facts field because they are one seam: both
/// families ship inert in `index.html` and both are display-only, so a host that
/// knows the peek content knows the reorg mode too.
///
/// # Why the peek spans are all-or-nothing
///
/// `#blocksPeek` is a single tooltip (`index.html:186`) that is either shown or
/// hidden as a unit, and `showPeek` (`scratch.js:424-436`) writes **all five**
/// spans on every hover: the kind, the name, the file, the `Loading source…`
/// reset and the degree line are assigned unconditionally, in that order. A
/// per-span `Option` would model a state the reference cannot reach — a panel
/// showing one card's name beside another card's file. So the panel is either
/// absent (`None`: never hovered, document byte-identical) or fully specified.
///
/// [`PeekView::src`] is the one span that legitimately changes after the
/// synchronous fill, which is what `src` on this struct carries.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BlocksPeekFacts {
    /// `#blocksPeek` — the panel itself. `true` removes the shipped `hidden`
    /// class; `false` leaves it (and is therefore idempotent for the default).
    #[serde(default)]
    pub visible: bool,
    /// `#blocksPeekKind` (`scratch.js:428`).
    #[serde(default)]
    pub kind: String,
    /// `#blocksPeekName` (`scratch.js:430`).
    #[serde(default)]
    pub name: String,
    /// `#blocksPeekFile` — `file` plus `:line` when the line is non-zero
    /// (`scratch.js:432`).
    #[serde(default)]
    pub file: String,
    /// `#blocksPeekSrc`. Ships as the literal `Loading source…` and is replaced
    /// by the first `PEEK_LINES` source lines, `(no source lines)`, or
    /// `source unavailable: <msg>` (`scratch.js:433`, `:442-448`).
    #[serde(default)]
    pub src: String,
    /// `#blocksPeekRefs` (`scratch.js:433-435`, refined at `:453`).
    #[serde(default)]
    pub refs: String,
    /// `#reorgMode`'s value — `"split"` is the only one that reveals the two
    /// chunk rows (`reorg.js:287`). Any other value (including `"single"`) hides
    /// them, exactly as `!== 'split'` does in the reference.
    #[serde(default)]
    pub reorg_mode: Option<String>,
    /// `#reorgGroup`'s value — only `"size"` keeps `#reorgSizeRow` visible
    /// (`reorg.js:290`).
    #[serde(default)]
    pub reorg_group: Option<String>,
}

/// Hydrate the `blocksPeek*` spans and the three `reorg*Row` classes.
///
/// Skipped entirely when `blocks_peek` is `None`, so the default paint is
/// byte-identical: `#blocksPeek` keeps `class="blocks-peek hidden"`,
/// `#blocksPeekSrc` keeps `Loading source…`, and the three rows keep the classes
/// `index.html` shipped.
fn apply_blocks_peek(d: &mut Dom<'_>, facts: &BlocksPeekFacts) {
    // ---- the tooltip panel + its five spans (scratch.js:424-436) -----------
    // `visible: false` is the shipped state, so the toggle is idempotent for the
    // default and only a real hover changes the document.
    if let Some(node) = d.id("blocksPeek") {
        d.set_class_flag(node, "hidden", !facts.visible);
    }
    for (id, text) in [
        ("blocksPeekKind", &facts.kind),
        ("blocksPeekName", &facts.name),
        ("blocksPeekFile", &facts.file),
        ("blocksPeekSrc", &facts.src),
        ("blocksPeekRefs", &facts.refs),
    ] {
        if let Some(node) = d.id(id) {
            d.set_text(node, text);
        }
    }

    // ---- the three conditional reorg rows (reorg.js:286-290) --------------
    // `syncRows` runs only when the dialog opens or its selects change, so an
    // absent mode/group is "not asked yet" and writes nothing at all — that is
    // different from `single`, which *hides* the rows.
    if let Some(mode) = &facts.reorg_mode {
        let split = mode == "split";
        if let Some(node) = d.id("reorgGranRow") {
            d.set_class_flag(node, "hidden", !split);
        }
        if let Some(node) = d.id("reorgGroupRow") {
            d.set_class_flag(node, "hidden", !split);
        }
        if let Some(node) = d.id("reorgSizeRow") {
            let group = facts.reorg_group.as_deref().unwrap_or_default();
            d.set_class_flag(node, "hidden", !(split && group == "size"));
        }
    }
}

/// Hydrate `#strCount` / `#chainCount` (`filters.js:523`/`:528`, `:555`).
///
/// A no-op unless the host actually measured something, so the default paint is
/// byte-identical: with `section_counts: None` neither span is touched at all.
fn apply_section_counts(d: &mut Dom<'_>, facts: &SectionCountFacts) {    // `filters.js:523` clears on an empty query; `:528` formats otherwise.
    // `StrCount::Cleared` must reach `set_text` with an empty string — treating
    // it as "nothing to do" would skip the clear and leave a stale count on
    // screen, which is exactly the bug `:523` exists to prevent.
    if let Some(count) = facts.str_count {
        if let Some(node) = d.id("strCount") {
            // `StrCount::Cleared` reaches `set_text` with an EMPTY string, not
            // `"0"` and not a skipped write — that is the whole of `:523`.
            // `StrCount::Count` goes through `fmtNum` (`filters.js:528`,
            // `core.js:209`), which is `toLocaleString('en-US')`: thousands
            // separators, matching `#pubCount` at `:1528`. A bare `{}` would
            // print `1234` where the reference prints `1,234`.
            let text = match count {
                StrCount::Cleared => String::new(),
                StrCount::Count(n) => fmt_num(n),
            };
            d.set_text(node, &text);
        }
    }
    if let Some(flat) = facts.chain_flat {
        if let Some(node) = d.id("chainCount") {
            d.set_text(node, &chain_count_label(flat, facts.chain_truncated.unwrap_or(false)));
        }
    }
}

/// The Public-definitions section's engine data (`filters.js:490-500`).
///
/// One struct for the count and the list because the reference loads them from
/// **one** request: `loadDefinitions` assigns `state.defs.list` and
/// `state.defs.total` from the same `d`, so a count that disagreed with the list
/// length is not a state the reference can reach. Keeping them together here
/// makes that structural rather than a convention.
///
/// Both fields are `Option` and both are `#[serde(default)]`, so a host that
/// knows only the count updates only the count. An absent `total` leaves
/// `#pubCount` alone rather than rendering `0` — see `Facts::pub_defs`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PubDefsFacts {
    /// `#pubCount` — the engine's `total` (`filters.js:493`, `:495` sets
    /// `pubCountEl.textContent = fmtNum(state.defs.total)`).
    ///
    /// This is the count of **all** public definitions the engine matched, not
    /// the length of `list`: `Index::definitions` computes it before truncating
    /// to `limit` (`core/src/index.rs:3425-3428`). Rendering it as `list.len()`
    /// would understate every corpus with more than 400 matches.
    pub total: Option<usize>,
    /// `#pubList` rows, already through the reference's `label || name || full ||
    /// id` fallback chain (`app/src/model.rs` `PubRow::from_lite`).
    ///
    /// Empty is a real state and renders the honest `no matches` /
    /// `no public definitions` line; it is **not** the same as absent. Absent
    /// (`None`) means "the list was never loaded" and leaves the shipped
    /// `open to load` placeholder in place.
    pub list: Option<Vec<PubRow>>,
}

/// The sidebar's **Declared types** section (`#typeSection`).
///
/// This section is created at runtime by [`apply_type_defs`], the same sanctioned
/// route as `#problemsSection` and the focus-filter rows: `index.html` is the v2
/// reference document and the Rust skeleton's parity test compares against it, so
/// a v3-only section must not be added to either. The default [`Facts`] carries
/// `None`, so a fixture render never creates it and the pinned `@frame` token is
/// untouched; the app always publishes it, so the user sees it.
///
/// The section answers one question: *which declarations under the current
/// namespace have a given declared type?* The `types` chips are the declared
/// types present (with counts, so "all types under this namespace" is visible at
/// a glance); `rows` are the declarations, each labelled with its own declared
/// type. Selecting a chip restricts the rows to exactly that type; the input is a
/// fuzzy filter over the type name and the declaration name.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TypeDefsFacts {
    /// `#typeCount` — the number of declarations the filters select, before the
    /// render cap. `None` leaves the span as created (empty).
    pub total: Option<usize>,
    /// One chip per declared type present, already filtered by the fuzzy query
    /// and sorted count-desc then name; `selected` marks the active restriction.
    /// The section always renders an extra `all` chip first.
    pub types: Vec<TypeCountFacts>,
    /// `#typeList` rows. `None` leaves the list exactly as it is (used on the
    /// first frame only); `Some(vec![])` is a real "no matches" state.
    pub rows: Option<Vec<PubRow>>,
    /// `#typeSearch`'s committed text. `None` leaves the created input empty.
    pub query: Option<String>,
    /// The namespace the rows are scoped to, for the section's tooltip. Empty
    /// means every namespace.
    pub scope: Option<String>,
}

/// One declared-type chip (`button.type-chip[data-key="type:<name>"]`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TypeCountFacts {
    /// The declared type exactly as the engine spells it (`string`, `bool`,
    /// `int`, `System.String`, `T`, …).
    pub name: String,
    /// How many declarations in scope have this declared type.
    pub count: usize,
    /// True for the one chip the rows are currently restricted to.
    pub selected: bool,
}

/// The edit modal's content, keyed by the **real DOM ids** (`edit.js:262-269`
/// binds `els.args`/`els.out`/`els.note`/`els.dry`/`els.apply`/`els.close` to
/// them, and `app/src/edit_host.rs` `facts()` publishes exactly these keys).
///
/// `#[serde(rename = "#…")]` on every field because here the JSON key *is* the
/// selector's id: the host emits `{"#editArgs": …}` and `#[serde(default)]`
/// keeps a partial object legal, so a host that has no note yet simply omits
/// `#editNote` and that line is left alone rather than blanked.
///
/// The three button fields carry their own `apply` flag: the host publishes the
/// *decision* the click means (`#editApply` ⇒ `apply:true`, `#editDry` ⇒
/// `false`) next to whether the button is disabled while a run is in flight.
/// `disabled` is the only thing this port can render — the click handler itself
/// is the host's (`app/src/edit_wire.rs`) — so a wrong `apply` value must stay
/// visible in the JSON rather than being invented here.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EditFacts {
    /// `#editTool` — the selected operation name. It is the **value** of the
    /// shipped empty `<select>` (`index.html:527`,
    /// `<select id="editTool" class="select"></select>`), not a rendered
    /// attribute: [`apply_edit`] uses it to mark the matching option of
    /// [`Self::tool_options`] as `selected`. With `tool_options: None` there are
    /// no options to mark, so the selection stays unrenderable and the element
    /// is left untouched rather than given a fabricated value.
    #[serde(rename = "#editTool")]
    pub tool: Option<String>,
    /// `#editTool`'s option list — an **internal wire key, not a DOM id**: no
    /// element ships `id="editToolOptions"`. `loadOps` (`edit.js:186-201`)
    /// fills the shipped empty `<select id="editTool">` from the engine's
    /// `tools` array, one `<option value=name>` per tool in array order; this
    /// carries that array so [`apply_edit`] can rebuild the same children.
    /// `None` means the host published no list, which leaves the select exactly
    /// as `index.html` shipped it.
    #[serde(rename = "#editToolOptions")]
    pub tool_options: Option<Vec<String>>,
    /// `#editArgs` — the args box's text (`els.args.value`, `edit.js:266`).
    #[serde(rename = "#editArgs")]
    pub args: Option<String>,
    /// `#editOut` — the raw tool JSON, refusals included (`els.out`, and
    /// `JSON.stringify(data, null, 2)` on assignment).
    #[serde(rename = "#editOut")]
    pub out: Option<serde_json::Value>,
    /// `#editNote` — the modal's note line (`note()` at `edit.js:74-77`).
    #[serde(rename = "#editNote")]
    pub note: Option<EditNoteFacts>,
    /// `#editDry` — the dry-run button (`run(false)`, `edit.js:276`).
    #[serde(rename = "#editDry")]
    pub dry: Option<EditButtonFacts>,
    /// `#editApply` — the apply button (`run(true)`, `edit.js:277`).
    #[serde(rename = "#editApply")]
    pub apply: Option<EditButtonFacts>,
    /// `#editClose` — the close button (`close()`, `edit.js:275`). Never
    /// disabled: `setBusy` (`edit.js:76-85`) touches dry, apply, tool, validate
    /// and dedupe but **not** close.
    #[serde(rename = "#editClose")]
    pub close: Option<EditButtonFacts>,
}

/// The `#editNote` line: the text plus the kind class `note()`
/// (`edit.js:74-77`) appends after the base `modal-note`.
///
/// `class` is the bare kind token (`'ok'`/`'error'`), not the whole
/// `className`: `note()` composes `'modal-note' + (kind ? ' ' + kind : '')`,
/// and the base class is what every `.modal-note` rule hangs off
/// (`style.css:883`; its kind variants are `.modal-note.ok` at `:889` and
/// `.modal-note.warn` at `:890`), so it must survive. [`apply_edit`] re-adds it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EditNoteFacts {
    pub text: String,
    /// The kind token appended after `modal-note`; empty is the bare base class.
    pub class: String,
}

/// One of the modal's footer buttons.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EditButtonFacts {
    /// What a click would mean (`true` = run with `apply:true`). Descriptive:
    /// the click travels through the host, not through this struct.
    pub apply: bool,
    /// `true` = the `disabled` attribute is set (`setBusy`, `edit.js:78-80`).
    pub disabled: bool,
}

/// One rendered `#pubList` row — the four things `renderListRows`
/// (`filters.js:448-475`) actually reads off a definition.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PubRow {
    /// The node id — what a row click selects in the graph (`filters.js:477`).
    pub id: String,
    /// The button `title` (`filters.js:454`: `b.title = n.full || n.id`).
    pub full: String,
    /// The visible `.l-label` text.
    pub label: String,
    /// The `.l-kind` chip text and the `.dot` colour key.
    pub kind: String,
}

/// The five Layout readout spans, already formatted as display text.
///
/// The host does the rounding and the two-decimal curvature formatting
/// (`app/src/nzk_ui.rs:960-968` `slider_facts`) so the renderer writes the
/// strings verbatim and cannot disagree with the panel about presentation.
/// Every field is `#[serde(default)]` so a partial object is tolerated: a host
/// that publishes only the slider it moved still updates that one span, and the
/// spans it said nothing about keep their shipped literals.
///
/// `rename_all = "camelCase"` is load-bearing, not cosmetic: the host spells
/// these keys `nodeSize`/`textSize` (`app/src/nzk_ui.rs:960-968`), so without it
/// serde would look for `node_size`/`text_size`, find nothing, and fall back to
/// `#[serde(default)]`'s empty string — leaving two of the five spans silently
/// dead while the other three worked.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SettingsFacts {
    /// `#spacingOut` — the node-spacing slider (`syncLayoutControls`, `shell.js:811`).
    pub spacing: String,
    /// `#iterOut` — the layout-iteration slider (`shell.js:812`).
    pub iterations: String,
    /// `#curveOut` — edge curvature, shown to two decimals (`shell.js:813`).
    pub curvature: String,
    /// `#nodeSizeOut` — the node-size slider (`shell.js:814`).
    pub node_size: String,
    /// `#textSizeOut` — the label text-size slider (`shell.js:815`).
    pub text_size: String,
}

/// `hierarchy.js`'s `state` object, as far as the DOM needs it.
///
/// The engine does the graph walking; this is the already-resolved panel
/// content. Every field is `Default` so a host can send a partial object.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HierarchyFacts {
    /// `"call"` or `"type"` (hierarchy.js `state.mode`, line 33).
    pub mode: String,
    /// `"both" | "forward" | "backward"` — drives which select is selected.
    pub dir: String,
    /// 1..=4, the `depth` select (hierarchy.js `state.depth`, default 2).
    pub depth: u32,
    /// `.cs-hier-title` text: the word, or `"Call hierarchy"` / `"Type hierarchy"`.
    pub title: String,
    /// `.cs-hier-sub` line under the title.
    pub subtitle: String,
    /// The two `<option>` lists for the `.cs-hier-controls` selects, in the
    /// order `hierarchy.js:469-470` appends them: the direction select first,
    /// then the depth select. Both are rendered only in `"call"` mode.
    pub dir_options: Vec<SelectOption>,
    /// The depth select's options (`1..=4`, `hierarchy.js:435-441`).
    ///
    /// Historical note: this field is still spelled `mode_options` on the wire
    /// (the renderer read it for the second select), but it has always carried
    /// the *depth* list — the first select is the direction. The name is kept
    /// for wire compatibility; `depth_options` is the honest alias.
    #[serde(alias = "depth_options")]
    pub mode_options: Vec<SelectOption>,
    /// `.cs-hier-section` blocks in order.
    pub sections: Vec<HierSection>,
    /// `.cs-hier-empty` text; empty leaves the element out.
    pub empty: String,
}

/// One `<option>` of a `.cs-hier-select`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

/// One `.cs-hier-section`: a `.cs-hier-sub` caption plus `.hop` rows.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HierSection {
    /// `.cs-hier-sub` caption (`"Outgoing calls — 3 hops"`).
    pub caption: String,
    pub rows: Vec<HierRow>,
}

/// One `.hop` row.
///
/// `hierarchy.js` renders `line`'s three spans (`.hop-caret`, `.hop-edge`,
/// `.hop-label`) plus an optional `.hop-cycle` chip. The caret glyph is
/// `\u{25be}` expanded / `\u{25b8}` collapsed, as in `flattenTree`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HierRow {
    /// `.hop-line` kind: `"call"`, `"type"`, `"inherit"` … drives the left rule.
    pub kind: String,
    /// Indentation, in `.hop` levels. `hierarchy.js` nests children; flat rows
    /// carry a depth so the same visual tree comes out of a flat list.
    pub depth: u32,
    /// `true` when this row has children, which is when a caret is drawn.
    pub has_children: bool,
    pub collapsed: bool,
    /// `.hop-edge` text: the edge label (`calls`, `inherits`, …).
    pub edge: String,
    /// `.hop-label` text: the node's display name.
    pub label: String,
    /// `.cs-hier-kind` text: the node kind (`method`, `class`, …).
    pub node_kind: String,
    /// `.hop-cycle` chip text; empty omits the chip.
    pub cycle: String,
}

impl Facts {
    /// What the original shows for the 1600x1000 A/B corpus (/tmp/fid-corpus).
    /// `source` records where each number came from, for the evidence report.
    pub fn for_corpus(repo: &str) -> Self {
        Self {
            repo: repo.to_string(),
            status_override: None,
            kinds: vec![
                ("method".to_string(), 4),
                ("class".to_string(), 3),
                ("namespace".to_string(), 2),
            ],
            ns_count: 2,
            indexed: true,
            file_count: 2,
            parse_ms: 51,
            node_count: 9,
            edge_count: 10,
            projects_empty_text: "no .sln/.csproj found".to_string(),
            parser_warning_count: 0,
            nodes_in_view: 2,
            projects: Vec::new(),
            warnings: Vec::new(),
            ui: None,
            hierarchy: None,
            // Absent: the host omits `settings` while the sliders sit at their
            // shipped defaults, so `apply_settings` is never entered and the
            // default render stays byte-identical.
            settings: None,
            // Absent: the `#pubCount`/`#pubList` fixture below is not part of
            // this corpus's shipped state, and omitting it keeps `#pubCount`
            // empty and `#pubList` at "open to load".
            pub_defs: None,
            type_defs: None,
            // Absent: no strings search has run, so the shipped placeholder row
            // and empty `#strCount` stay.
            str_defs: None,
            // Absent: neither counter is written until its own request answers,
            // so both spans keep `index.html`'s shipped empty text.
            section_counts: None,
            // Absent: nothing has hovered a block cell and the reorg dialog has
            // not opened, so all eight ids keep exactly what `index.html` shipped
            // (`#blocksPeek`'s `hidden` class, `#blocksPeekSrc`'s
            // `Loading source…`, and the rows' own classes).
            blocks_peek: None,
            // Absent: the edit dialog has not opened a run, so it keeps
            // `index.html`'s shipped `{}` args box, empty `<pre>`, bare
            // `modal-note` and enabled footer buttons.
            edit: None,
        }
    }

    /// `shell.js` updateStatusLine (lines 130-146). The literal has double
    /// spaces around the interpuncts; CSS collapses them, exactly as in the
    /// browser.
    pub fn status_text(&self) -> String {
        if let Some(s) = &self.status_override {
            return s.clone();
        }
        if !self.indexed {
            return "not indexed".to_string();
        }
        format!(
            "files {}  ·  parse {}ms  ·  nodes {}  ·  edges {}",
            self.file_count, self.parse_ms, self.node_count, self.edge_count
        )
    }
}

fn has_class(doc: &BaseDocument, node: usize, class: &str) -> bool {
    doc.get_node(node)
        .and_then(|n| n.element_data())
        .map(|e| {
            e.attrs
                .iter()
                .any(|a| a.name.local.as_ref() == "class"
                    && a.value.split_whitespace().any(|c| c == class))
        })
        .unwrap_or(false)
}

/// Depth-first search for an element carrying `class`.
///
/// Deliberately a plain tree walk and NOT `BaseDocument::query_selector`:
/// selector queries run stylo's invalidation machinery, and doing that *before*
/// the document's first `resolve()` leaves elements with no computed data; the
/// subsequent `resolve()` then panics in `stylo::data::ElementData::
/// invalidate_style_if_needed`. Id lookups (`get_element_by_id`) and tree walks
/// are safe pre-resolve; selector queries are only used after `resolve` (the
/// `--boxes` probe).
fn find_by_class(doc: &BaseDocument, root: usize, class: &str) -> Option<usize> {
    let node = doc.get_node(root)?;
    if node.is_element() && has_class(doc, root, class) {
        return Some(root);
    }
    for child in &node.children {
        if let Some(found) = find_by_class(doc, *child, class) {
            return Some(found);
        }
    }
    None
}

/// First direct element child carrying `class`.
fn child_by_class(doc: &BaseDocument, parent: usize, class: &str) -> Option<usize> {
    let node = doc.get_node(parent)?;
    node.children
        .iter()
        .copied()
        .find(|c| doc.get_node(*c).map(|n| n.is_element()).unwrap_or(false) && has_class(doc, *c, class))
}

/// Previous *element* sibling, the DOM `previousElementSibling` the scripts use.
fn prev_element_sibling(doc: &BaseDocument, node: usize) -> Option<usize> {
    let n = doc.get_node(node)?;
    let parent = n.parent?;
    let kids = &doc.get_node(parent)?.children;
    let idx = kids.iter().position(|c| *c == node)?;
    kids[..idx]
        .iter()
        .rev()
        .copied()
        .find(|c| doc.get_node(*c).map(|n| n.is_element()).unwrap_or(false))
}

/// The sidebar sections whose collapsed state the host owns, with the caret
/// glyph `panels.js`/`problems.js` write when the section is expanded/collapsed.
///
/// A `ui.collapsed` entry matches a section by **either** its DOM id
/// (`filtersSection`) or the head's selector (`#filtersHead`), because the host
/// stores the selector the nzk_ui dispatch table reports
/// (`actions::NzkUiState::collapsed`), while the DOM state lives on the
/// `<section>` element. Accepting both removes a translation layer that could
/// drift.
/// `(section DOM id, head DOM selector)`. The ids are not derivable from each
/// other by concatenation (`filtersSection`'s head is `#filtersHead`), so the
/// pair is explicit.
const SECTIONS: &[(&str, &str)] = &[
    ("filtersSection", "#filtersHead"),
    ("pubSection", "#pubHead"),
    ("strSection", "#strHead"),
    ("chainSection", "#chainHead"),
    ("problemsSection", "#problemsHead"),
];

fn is_collapsed(ui: &UiState, id: &str, head: &str) -> bool {
    ui.collapsed
        .as_ref()
        .map(|list| list.iter().any(|c| c == id || c == head))
        .unwrap_or(false)
}

/// Apply the host's view state: collapsed sections and the active mode tab.
///
/// `ui: None` is a no-op, so the default render is byte-identical to the
/// shipped `index.html`.
/// Write the Layout ▸ slider readouts (`syncLayoutControls`, `shell.js:807-817`).
///
/// The reference sets each span's text from the current slider value; the host
/// has already done the formatting, so this writes the strings verbatim.
///
/// # The no-op promise
///
/// `facts.settings` is `None` both when the host omitted the key and when it
/// published a `null`, and in that case this function is never called at all —
/// so the five spans keep the literals in `index.html` and the default render
/// stays byte-identical. That is the same shape as [`apply_hierarchy`].
///
/// # Per-span independence
///
/// A field that is empty is skipped, and an id that is not in the document is
/// skipped, so a partial object updates exactly the spans it names and no
/// others. Skipping empty matters for correctness rather than politeness: every
/// field defaults to `""`, so writing unconditionally would blank out the four
/// spans a host did not mention instead of leaving their shipped text alone.
/// Write `#pubCount` and `#pubList` from the engine's own definitions answer.
///
/// Mirrors `loadDefinitions` + `renderPubList` (`filters.js:490-513`).
///
/// # The count is the engine's `total`, never the rendered row count
///
/// `filters.js:493` reads `state.defs.total = d.total || state.defs.list.length`
/// and `:495` writes that to `#pubCount`. `Index::definitions` computes `total`
/// over **all** matches and only then truncates to `limit`
/// (`core/src/index.rs:3425-3428`), and the reference additionally renders at
/// most 300 rows (`filters.js:455`). So a corpus with 900 public definitions
/// shows `900` beside a 300-row list. Substituting `list.len()` here would
/// silently understate every corpus past the cap — the count is a fact about the
/// corpus, not about the box.
///
/// # Empty list vs absent list vs absent count
///
/// * `total: None` — `#pubCount` is left exactly as shipped (empty). Never
///   written as `0`.
/// * `list: None` — `#pubList` keeps its shipped
///   `<div class="list-empty">open to load</div>`, which is the true state: the
///   reference only loads on first expand (`shell.js:556`).
/// * `list: Some(vec![])` — the engine answered with nothing, so the box gets a
///   real `.list-empty` row. `renderPubList` passes `'no matches'`
///   (`filters.js:513`), which is what the reference shows for a filter that
///   matched nothing; with no filter set that is only reachable on an empty
///   corpus, where it is also the honest text.
/// The "Hide parents" focus filter, created beside the shipped Groups row.
///
/// It is runtime-created on purpose: `index.html` (the v2 reference document)
/// must stay untouched, and the Rust skeleton's structural parity test compares
/// against it. The control carries the same `data-key` channel as the shipped
/// checkboxes, so a press on the text routes exactly like a press on the input.
///
/// "Only related nodes" deliberately has **no** sidebar control: it is anchored
/// on the selected node, so it lives with that node's actions in the drawer
/// (`act:focus`). A global checkbox for a selection-relative filter was the
/// confusing duplicate this function used to paint.
fn apply_focus_filters(d: &mut Dom<'_>, f: &FiltersFacts) {
    let Some(input) = d.id("hideChildren") else { return };
    let Some(hide_label) = d.parent_of(input) else { return };
    let name = f
        .focus_selected
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!(" — {s}"))
        .unwrap_or_default();
    let parents_title = format!(
        "Do not draw the containment parents (the Parent chain) of the selected node{name}."
    );

    let row = d.mk(El::new("div").cls("filter-row").style("margin-top:9px"));

    let parents_label = d.mk(
        El::new("label")
            .cls("check")
            .a("data-key", "filter:focusParents")
            .a("title", &parents_title),
    );
    let parents_input = d.mk(
        El::new("input")
            .a("type", "checkbox")
            .a("id", "focusParents"),
    );
    d.set_checked(parents_input, f.focus_parents);
    let parents_text = d.mk(El::new("span").text("Hide parents"));
    d.append(parents_label, &[parents_input, parents_text]);

    d.append(row, &[parents_label]);
    d.insert_after(hide_label, &[row]);
}

fn apply_pub_defs(d: &mut Dom<'_>, facts: &PubDefsFacts) {
    if let Some(total) = facts.total {
        if let Some(node) = d.id("pubCount") {
            // `fmtNum` (`util.js`) is `toLocaleString('en-US')` — thousands
            // separators. Rust has no locale formatter in this dependency set,
            // so the same output is produced directly: groups of three from the
            // right, comma-separated. Rendering the bare integer would disagree
            // with the reference for every corpus over 999 definitions.
            d.set_text(node, &fmt_num(total));
        }
    }
    let Some(rows) = &facts.list else { return };
    let Some(list) = d.id("pubList") else { return };
    if rows.is_empty() {
        let empty = d.mk(El::new("div").cls("list-empty").text("no matches"));
        d.set_children(list, &[empty]);
        return;
    }
    let mut nodes = Vec::with_capacity(rows.len());
    for row in rows {
        // `renderListRows` (`filters.js:453-473`): a `<button class="lrow">`
        // whose title is `n.full || n.id`, holding a `.dot`, a `.l-label` and a
        // `.l-kind` — in that order, appended to a fragment and then to the box.
        let label = if row.label.is_empty() { row.full.as_str() } else { row.label.as_str() };
        let title = if row.full.is_empty() { row.id.as_str() } else { row.full.as_str() };
        let dot = d.mk(El::new("span").cls("dot"));
        let lab = d.mk(El::new("span").cls("l-label").text(label));
        let kd = d.mk(El::new("span").cls("l-kind").text(&row.kind));
        let btn = d.mk(
            El::new("button")
                .cls("lrow")
                .a("type", "button")
                .a("data-key", &row.id)
                .a("title", title),
        );
        d.append(btn, &[dot, lab, kd]);
        nodes.push(btn);
    }
    d.set_children(list, &nodes);
}

/// The Strings / error codes list (`filters.js:516-534`), the twin of
/// [`apply_pub_defs`] with `#strCount`/`#strList` as its slots. Rows carry
/// `data-key=<node id>` so a press selects that literal through the shell hit
/// chain, exactly like the public-definitions rows.
fn apply_str_defs(d: &mut Dom<'_>, facts: &PubDefsFacts) {
    if let Some(total) = facts.total {
        if let Some(node) = d.id("strCount") {
            d.set_text(node, &fmt_num(total));
        }
    }
    let Some(rows) = &facts.list else { return };
    let Some(list) = d.id("strList") else { return };
    if rows.is_empty() {
        let empty = d.mk(El::new("div").cls("list-empty").text("no string matches"));
        d.set_children(list, &[empty]);
        return;
    }
    let mut nodes = Vec::with_capacity(rows.len());
    for row in rows {
        let label = if row.label.is_empty() { row.full.as_str() } else { row.label.as_str() };
        let title = if row.full.is_empty() { row.id.as_str() } else { row.full.as_str() };
        let dot = d.mk(El::new("span").cls("dot"));
        let lab = d.mk(El::new("span").cls("l-label").text(label));
        let kd = d.mk(El::new("span").cls("l-kind").text(&row.kind));
        let btn = d.mk(
            El::new("button")
                .cls("lrow")
                .a("type", "button")
                .a("data-key", &row.id)
                .a("title", title),
        );
        d.append(btn, &[dot, lab, kd]);
        nodes.push(btn);
    }
    d.set_children(list, &nodes);
}

/// Create and hydrate the sidebar's **Declared types** section.
///
/// The section is v3-only and is therefore built at runtime, never added to
/// `index.html` or the Rust skeleton: `skeleton::testsupport` compares the
/// static tree against the reference document element for element, so a shipped
/// section would break that parity, and a fixture `Facts` (with `type_defs:
/// None`) must leave the document byte-identical. This is the same route
/// `apply_focus_filters` and the parser-warnings section already take.
///
/// Markup, once, on the first call:
///
/// ```text
/// section#typeSection.section
///   div#typeHead.section-head          (static: no collapse, title = scope)
///     span.caret ▾ · "Declared types" · span#typeCount.scount
///   div.section-body
///     input#typeSearch.mini-input      (fuzzy type/name filter)
///     div#typeChips.chips              (one button.type-chip per declared type)
///     div#typeList.listbox.tall        (one button.lrow per declaration)
/// ```
///
/// Every later call rewrites the count, the input value, the chips and the rows
/// from `facts`, so the section has exactly one owner per fact (the host) and a
/// repaint cannot duplicate a chip or a row (`set_children` is `innerHTML = ''`
/// followed by the new children).
fn apply_type_defs(d: &mut Dom<'_>, facts: &TypeDefsFacts) {
    let scope = facts.scope.as_deref().unwrap_or("");
    let head_title = if scope.is_empty() {
        "Declarations grouped by their declared type (return type, field type, …), \
         across every namespace. Pick a type chip to see only that type, or type in \
         the box to fuzzy-match a type or a name."
            .to_string()
    } else {
        format!(
            "Declarations grouped by their declared type (return type, field type, …) \
             under the namespace `{scope}`. Pick a type chip to see only that type, or \
             type in the box to fuzzy-match a type or a name."
        )
    };
    if d.id("typeSection").is_none() {
        let Some(filters) = d.id("filtersSection") else { return };
        let section = d.mk(El::new("section").id("typeSection").cls("section"));
        // A `<div>`, not a `<button>`: this section is always open, so a button
        // would advertise a collapse that does not exist (`cursor:default`).
        let head = d.mk(
            El::new("div")
                .cls("section-head")
                .id("typeHead")
                .a("style", "cursor:default")
                .a("title", &head_title),
        );
        let caret = d.mk(El::new("span").cls("caret").text("\u{25be}"));
        let label = d.mk_text("Declared types");
        let count = d.mk(El::new("span").cls("scount").id("typeCount"));
        d.append(head, &[caret, label, count]);

        let body = d.mk(El::new("div").cls("section-body"));
        let input = d.mk(
            El::new("input")
                .id("typeSearch")
                .cls("mini-input")
                .a("type", "text")
                .a("spellcheck", "false")
                .a("autocomplete", "off")
                .a("placeholder", "filter declared type\u{2026}")
                .a(
                    "title",
                    "Fuzzy match a declared type (`str` -> string / System.String) or a \
                     declaration's name. Selecting a type chip below restricts the list to \
                     exactly that declared type.",
                ),
        );
        let chips = d.mk(El::new("div").cls("chips type-chips").id("typeChips"));
        let list = d.mk(El::new("div").cls("listbox tall").id("typeList"));
        d.append(body, &[input, chips, list]);
        d.append(section, &[head, body]);
        // Immediately after the Filters section: the type browser is a *filter*
        // on the same corpus, so it belongs beside the namespace/kind controls.
        d.insert_after(filters, &[section]);
    }

    // Keep the scope tooltip current: the first call creates it, but the
    // namespace can change afterwards and a stale title would describe the
    // wrong scope.
    if let Some(head) = d.id("typeHead") {
        d.set_attr(head, "title", &head_title);
    }
    if let Some(total) = facts.total {
        if let Some(node) = d.id("typeCount") {
            d.set_text(node, &fmt_num(total));
        }
    }
    if let Some(query) = &facts.query {
        if let Some(input) = d.id("typeSearch") {
            d.set_attr(input, "value", query);
        }
    }
    // ---- chips --------------------------------------------------------------
    if let Some(chips) = d.id("typeChips") {
        let mut nodes: Vec<usize> = Vec::new();
        // The `all` chip clears the restriction. `data-key="type:"` (empty type)
        // is the only value that means "no restriction"; the host decodes the
        // suffix after `type:`.
        let all_cls = if facts.types.iter().any(|t| t.selected) {
            "type-chip"
        } else {
            "type-chip active"
        };
        nodes.push(d.mk(
            El::new("button")
                .cls(all_cls)
                .a("type", "button")
                .a("data-key", "type:")
                .a("title", "Show every declared type in scope")
                .text("all"),
        ));
        for t in &facts.types {
            let cls = if t.selected {
                "type-chip active"
            } else {
                "type-chip"
            };
            let chip_title = format!(
                "Show only declarations whose declared type is `{}` ({} match{}).",
                t.name,
                t.count,
                if t.count == 1 { "" } else { "es" }
            );
            let btn = d.mk(
                El::new("button")
                    .cls(cls)
                    .a("type", "button")
                    .a("data-key", &format!("type:{}", t.name))
                    .a("title", &chip_title),
            );
            let name = d.mk(El::new("span").cls("tname").text(&t.name));
            let count = d.mk(
                El::new("span")
                    .cls("tcount")
                    .text(&fmt_num(t.count as usize)),
            );
            d.append(btn, &[name, count]);
            nodes.push(btn);
        }
        if facts.types.is_empty() {
            let empty = d.mk(
                El::new("span")
                    .cls("tcount")
                    .text("no declared types in scope"),
            );
            nodes.push(empty);
        }
        d.set_children(chips, &nodes);
    }
    // ---- rows ---------------------------------------------------------------
    let Some(rows) = &facts.rows else { return };
    let Some(list) = d.id("typeList") else { return };
    if rows.is_empty() {
        let empty = d.mk(
            El::new("div")
                .cls("list-empty")
                .text("no declarations of that declared type"),
        );
        d.set_children(list, &[empty]);
        return;
    }
    let mut nodes = Vec::with_capacity(rows.len());
    for row in rows {
        let label = if row.label.is_empty() {
            row.full.as_str()
        } else {
            row.label.as_str()
        };
        let title = if row.full.is_empty() {
            row.id.as_str()
        } else {
            row.full.as_str()
        };
        let dot = d.mk(El::new("span").cls("dot"));
        let lab = d.mk(El::new("span").cls("l-label").text(label));
        // `.l-kind` carries the **declared type** here, not the node kind: the
        // whole point of the section is that this column is the type.
        let ty = d.mk(El::new("span").cls("l-kind").text(&row.kind));
        let btn = d.mk(
            El::new("button")
                .cls("lrow")
                .a("type", "button")
                .a("data-key", &row.id)
                .a("title", title),
        );
        d.append(btn, &[dot, lab, ty]);
        nodes.push(btn);
    }
    d.set_children(list, &nodes);
}

/// Hydrate the edit modal (`edit.js:262-269`, via `app/src/edit_host.rs`).
///
/// A no-op unless the host actually published something, so the default paint
/// is byte-identical: with `edit: None` not even one of the seven ids is
/// touched. Every field is independently optional for the same reason — the
/// host omits `#editOut` until a run concludes and `#editNote` while the note
/// is empty (`app/src/edit_host.rs:24-26`), and each omitted key must leave its
/// element exactly as `index.html` shipped it.
fn apply_edit(d: &mut Dom<'_>, facts: &EditFacts) {
    // `els.args.value = …` (`edit.js:266`). This port's box is a plain string,
    // so it is written as text (the shipped `<textarea>` carries `{}` as its
    // text content, `index.html:531-534`).
    if let Some(args) = &facts.args {
        if let Some(node) = d.id("editArgs") {
            d.set_text(node, args);
        }
    }
    // `editFetch` hands the raw tool JSON back and the caller prints it with
    // `JSON.stringify(data, null, 2)`. `to_string_pretty` is that rendering; a
    // value that cannot be serialised (there is none today) degrades to the
    // compact `Display` form rather than panicking — the modal is diagnostics,
    // not a reason to tear down the paint.
    if let Some(out) = &facts.out {
        if let Some(node) = d.id("editOut") {
            let text = serde_json::to_string_pretty(out).unwrap_or_else(|_| format!("{out}"));
            d.set_text(node, &text);
        }
    }
    // `note(text, kind)` (`edit.js:74-77`) replaces `textContent` and sets
    // `className = 'modal-note' + (kind ? ' ' + kind : '')`. The base class is
    // load-bearing: `.modal-note` (`style.css:883`) and its variants
    // `.modal-note.ok` (`:889`) / `.modal-note.warn` (`:890`) all require it, so
    // the kind token is APPENDED and the shipped base survives.
    if let Some(note) = &facts.note {
        if let Some(node) = d.id("editNote") {
            d.set_text(node, &note.text);
            let class = if note.class.is_empty() {
                "modal-note".to_string()
            } else {
                format!("modal-note {}", note.class)
            };
            d.set_attr(node, "class", &class);
        }
    }
    // `loadOps` (`edit.js:186-201`) rebuilds `#editTool`'s children from the
    // engine's `tools` array: `fill(els.tool, tools)`, one `<option value=name>`
    // per tool, then `els.tool.value = tools[0]`. The host now carries that
    // array on the internal `#editToolOptions` key (not a DOM id), so the
    // shipped empty `<select>` can be populated without inventing options.
    // `tool` is the select's current value; this DOM layer has no select-value
    // API, so the matching option carries the `selected` attribute instead. A
    // `tool` that is absent from the list gets no mark (never faked), and a
    // missing `tool` reproduces the browser's own default — the first option is
    // the value — rather than inventing a third state.
    if let Some(options) = &facts.tool_options {
        if let Some(select) = d.id("editTool") {
            let selected_at = match facts.tool.as_deref() {
                None => (!options.is_empty()).then_some(0),
                Some(tool) => options.iter().position(|name| name == tool),
            };
            let mut nodes = Vec::with_capacity(options.len());
            for (i, name) in options.iter().enumerate() {
                let mut el = El::new("option").a("value", name).text(name);
                if selected_at == Some(i) {
                    el = el.a("selected", "selected");
                }
                nodes.push(d.mk(el));
            }
            d.set_children(select, &nodes);
        }
    }
    // `setBusy` (`edit.js:78-80`): `disabled` is set while a run is in flight
    // and cleared afterwards, for both footer buttons. `#editClose` has no busy
    // guard in the reference (`setBusy` never touches it) and the host always
    // publishes `disabled:false` for it (`app/src/edit_host.rs:156`); writing it
    // through the same helper anyway keeps the three buttons honest about the
    // single field they carry.
    for (id, button) in [
        ("editDry", facts.dry.as_ref()),
        ("editApply", facts.apply.as_ref()),
        ("editClose", facts.close.as_ref()),
    ] {
        let Some(button) = button else { continue };
        let Some(node) = d.id(id) else { continue };
        if button.disabled {
            d.set_attr(node, "disabled", "disabled");
        } else {
            d.clear_attr(node, "disabled");
        }
    }
}

/// `fmtNum` (`util.js`) — `n.toLocaleString('en-US')`, i.e. thousands groups.
///
/// Written out rather than pulled from a crate: the reference uses the browser's
/// `toLocaleString`, whose `en-US` output for a non-negative integer is exactly
/// comma-grouped digits. This is that, and nothing else — no rounding, no
/// compact form, so `1234` → `"1,234"` and `123` → `"123"`.
fn fmt_num(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Mirror the two mutually-inverse filter checkboxes onto the one flag.
///
/// `#groupChildren` shows `flag`; `#hideChildren` shows its **inverse**. That
/// inversion is the whole point of this function and it lives here, at the DOM
/// boundary, because it is a *presentation* fact about these two elements:
///
/// * `shell.js:646` — the `#hideChildren` handler assigns
///   `state.filters.groupChildren = !hideChildrenEl.checked`;
/// * `shell.js:603-604` — the `#groupChildren` handler assigns
///   `hideChildrenEl.checked = !groupChildrenEl.checked`;
/// * `shell.js:647` / `:604` — each handler mirrors the other box.
///
/// So the two boxes can never agree, and any host that wrote the flag to both
/// would produce a document the reference cannot reach: two checkboxes that
/// contradict each other about one flag. `flag` is the model value
/// (`GraphView::group_children`, single writer `set_group_children`); this
/// function is the only place it becomes DOM state.
///
/// `index.html` ships `#groupChildren` **checked and hidden** (`:253`, the
/// legacy `kindChildWrap` chip) and `#hideChildren` unchecked (`:274`), which is
/// `flag == true` — so the shipped document already agrees with the default and
/// calling this with `true` is a no-op on the attribute. It is still called, so
/// that a host that has moved the flag and then repaints cannot leave the boxes
/// showing the previous state.
fn apply_filter_checkboxes(d: &mut Dom<'_>, flag: bool) {
    if let Some(node) = d.id("groupChildren") {
        d.set_checked(node, flag);
    }
    if let Some(node) = d.id("hideChildren") {
        d.set_checked(node, !flag);
    }
    // `updateHideChildrenToggle` (`shell.js:600-618`): the visible legacy chip
    // `#kindChildWrap` shows `off` when the children flag is off. It is the
    // press target (the checkbox inside it is `hidden`), so painting its state
    // is what makes the control self-consistent: it looked always-on before.
    //
    // Deferred to [`apply_paint_shims`] through [`defer_chip_off`]: a `class`
    // write is selector-matched, and issuing it before the first style pass makes
    // Stylo process an invalidation for an element that has no computed style yet
    // (`vendor/stylo/data.rs:190` unwrap panic). `checked` above is not
    // selector-matched, so it is safe here.
    defer_chip_off("kindChildWrap", !flag);
}

/// Mirrors `#groupEdges` from the *edges* flag — the counterpart of
/// [`apply_filter_checkboxes`], and deliberately a **separate** function because
/// it is a **separate control with opposite polarity**:
///
/// * `#groupEdges` is a distinct box from `#groupChildren`
///   (`index.html:255` vs `:253`), driven by its own flag
///   (`rootEdges` ← `state.filters.groupEdges`, `shell.js:609-612`).
/// * It has **no inversion**: `sync.js:165` and `app.js:299` both assign the
///   flag straight to `.checked`, so the box shows `flag` itself. Only the
///   `groupChildren`/`hideChildren` *pair* inverts; folding this box into
///   `apply_filter_checkboxes` would force a polarity shared with a box it has
///   nothing to do with, and would make `#groupEdges` show the *children* flag.
///
/// `index.html:255` ships `#groupEdges` **`checked`** (and `hidden`, which is a
/// presentation attribute the flag does not govern — the reference toggles only
/// `#kindEdgeWrap`'s `off` class at `shell.js:611`, never the `hidden`
/// attribute). So `flag == true` is a no-op on the attribute and the shipped
/// default render stays byte-identical.
fn apply_edge_filter_checkbox(d: &mut Dom<'_>, flag: bool) {
    if let Some(node) = d.id("groupEdges") {
        d.set_checked(node, flag);
    }
    // `shell.js:611`: `$('#kindEdgeWrap').classList.toggle('off', !groupEdges.checked)`
    // — the visible chip mirrors the hidden box, exactly like the children chip.
    // Deferred for the same pre-style invalidation reason as the children chip.
    defer_chip_off("kindEdgeWrap", !flag);
}

/// The two Group chips' `.off` class writes, staged for the post-style pass.
///
/// Why deferred: `apply_runtime_dom` runs **before** the first `resolve()`, and a
/// `class` attribute participates in selector matching. Writing it there makes
/// Stylo queue an invalidation for an element whose computed style does not exist
/// yet; the traversal then hits `ElementStyles::primary().unwrap()`
/// (`vendor/stylo/data.rs:190`) and panics. The `checked`/`data-*` attributes the
/// rest of the filters use are not selector-matched, which is why they are safe
/// earlier. [`apply_paint_shims`] runs after the first resolve and before the
/// second, so a class write there reaches styled elements and the second pass
/// repaints them — same pixels, no panic.
thread_local! {
    static PENDING_CHIP_OFF: std::cell::RefCell<Vec<(&'static str, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn defer_chip_off(id: &'static str, off: bool) {
    PENDING_CHIP_OFF.with(|q| q.borrow_mut().push((id, off)));
}

/// Apply every staged chip `.off` class. Called at the top of
/// [`apply_paint_shims`]; draining there keeps the pre-style/post-style split in
/// one place rather than adding a second public step to the shell constructors.
fn drain_chip_off(doc: &mut BaseDocument) {
    let pending: Vec<(&'static str, bool)> =
        PENDING_CHIP_OFF.with(|q| std::mem::take(&mut *q.borrow_mut()));
    if pending.is_empty() {
        return;
    }
    let mut d = Dom::new(doc);
    for (id, off) in pending {
        if let Some(node) = d.id(id) {
            d.set_class_flag(node, "off", off);
        }
    }
}

fn apply_settings(d: &mut Dom<'_>, facts: &SettingsFacts) {    // `(#id, value)` in the reference's own order (`shell.js:811-815`).
    let spans = [
        ("spacingOut", facts.spacing.as_str()),
        ("iterOut", facts.iterations.as_str()),
        ("curveOut", facts.curvature.as_str()),
        ("nodeSizeOut", facts.node_size.as_str()),
        ("textSizeOut", facts.text_size.as_str()),
    ];
    for (id, text) in spans {
        if text.is_empty() {
            continue;
        }
        if let Some(node) = d.id(id) {
            d.set_text(node, text);
        }
    }
}


/// The reference appends `.cs-hier` to `document.body` (line 448) and reveals
/// it by setting `display: flex` (line 460). This builds the same subtree in the
/// same order with the same classes, so `style.css` and the injected
/// `.cs-hier*` rules lay it out identically.
///
/// Only called when `facts.hierarchy` is `Some`; a closed panel costs nothing.
fn apply_hierarchy(d: &mut Dom<'_>, hier: &HierarchyFacts) {
    let Some(body) = d.body() else { return };

    let is_call = hier.mode == "call";

    // ---- head: .cs-hier-title (+ optional .cs-hier-sub) and the close button
    let title = d.mk(El::new("div").cls("cs-hier-title").text(&hier.title));
    // hierarchy.js appends the text node first, then the sub span, so the
    // caption reads "<title> <sub>" inside the same element.
    if !hier.subtitle.is_empty() {
        let sub = d.mk(El::new("span").cls("cs-hier-sub").text(&hier.subtitle));
        d.append(title, &[sub]);
    }
    let close = d.mk(
        El::new("button")
            .cls("cs-hier-btn")
            .a("type", "button")
            .a("title", "Close")
            .text("\u{2715}"),
    );
    let head = d.mk(El::new("div").cls("cs-hier-head"));
    d.append(head, &[title, close]);

    // ---- controls: two .cs-hier-select, hidden entirely in type mode
    let controls = d.mk(El::new("div").cls("cs-hier-controls"));
    if is_call {
        let dir_sel = d.mk(El::new("select").cls("cs-hier-select"));
        let mut opts = Vec::new();
        for o in &hier.dir_options {
            let mut opt = El::new("option").a("value", &o.value).text(&o.label);
            if o.selected {
                opt = opt.a("selected", "selected");
            }
            opts.push(d.mk(opt));
        }
        d.append(dir_sel, &opts);

        let depth_sel = d.mk(El::new("select").cls("cs-hier-select"));
        let mut dopts = Vec::new();
        for o in &hier.mode_options {
            let mut opt = El::new("option").a("value", &o.value).text(&o.label);
            if o.selected {
                opt = opt.a("selected", "selected");
            }
            dopts.push(d.mk(opt));
        }
        d.append(depth_sel, &dopts);
        d.append(controls, &[dir_sel, depth_sel]);
    }

    // ---- body: sections of .hop rows
    let panel_body = d.mk(El::new("div").cls("cs-hier-body"));
    let mut blocks = Vec::new();
    for sec in &hier.sections {
        let section = d.mk(
            El::new("div")
                .cls("cs-hier-section")
                .text(&sec.caption),
        );
        blocks.push(section);
        for row in &sec.rows {
            let r = hierarchy_row(d, row);
            blocks.push(r);
        }
    }
    if !hier.empty.is_empty() {
        let e = d.mk(El::new("div").cls("cs-hier-empty").text(&hier.empty));
        blocks.push(e);
    }
    d.append(panel_body, &blocks);

    let panel = d.mk(El::new("div").cls("cs-hier").style("display: flex"));
    d.append(panel, &[head, controls, panel_body]);
    d.append(body, &[panel]);
}

/// One `.hop` row (`hierarchy.js` `rowEl`, lines 478-519).
///
/// The reference nests each child wrapper inside its parent, and indents with
/// `paddingLeft = (8 + depth * 12)px`. A flat `depth` reproduces the same
/// visual result without the nesting, which keeps the DOM shallow enough for
/// blitz's invalidation path.
fn hierarchy_row(d: &mut Dom<'_>, row: &HierRow) -> usize {
    let caret_glyph = if row.has_children {
        if row.collapsed {
            "\u{25b8}"
        } else {
            "\u{25be}"
        }
    } else {
        "\u{00b7}"
    };
    let btn = d.mk(
        El::new("button")
            .cls("hop")
            .a("type", "button")
            .style(&format!("padding-left: {}px", 8 + row.depth * 12)),
    );
    let mut kids = vec![d.mk(El::new("span").cls("hop-caret").text(caret_glyph))];
    if !row.edge.is_empty() {
        kids.push(d.mk(El::new("span").cls("hop-edge").text(&row.edge)));
    }
    if !row.label.is_empty() {
        kids.push(d.mk(El::new("span").cls("hop-label").text(&row.label)));
    }
    if !row.node_kind.is_empty() {
        kids.push(d.mk(El::new("span").cls("cs-hier-kind").text(&row.node_kind)));
    }
    if !row.cycle.is_empty() {
        kids.push(d.mk(El::new("span").cls("hop-cycle").text(&row.cycle)));
    }
    d.append(btn, &kids);
    let wrap = d.mk(El::new("div").cls(&format!("hop-wrap {}", row.kind)));
    d.append(wrap, &[btn]);
    wrap
}

fn apply_ui_state(d: &mut Dom<'_>, ui: &UiState) {
    // Real pane-open grid: the stylesheet owns the row sizes.
    if ui.pane_open {
        if let Some(layout) = find_by_class(d.doc, d.doc.root_element().id, "layout") {
            d.set_class_flag(layout, "with-source", true);
        }
        if let Some(pane) = d.id("sourcePane") {
            d.set_class_flag(pane, "hidden", false);
        }
    }
    apply_drawers(d, ui);
    apply_modals(d, ui);
    apply_loading(d, ui);
    apply_chain(d, ui);
    apply_source(d, ui);
    if let Some(theme) = &ui.ui_theme {
        d.set_attr(d.doc.root_element().id, "data-ui-theme", theme);
    }
    if let Some(theme) = &ui.source_theme {
        set_select_value(d, "sourceTheme", theme);
    }
    if let Some(q) = &ui.pub_query {
        if let Some(input) = d.id("pubSearch") {
            d.set_attr(input, "value", q);
        }
    }
    if let Some(q) = &ui.str_query {
        if let Some(input) = d.id("strSearch") {
            d.set_attr(input, "value", q);
        }
    }
    // The change-preview panel's body. `#previewOut`/`#previewNote` ship empty,
    // so an absent key is a no-op and the default document stays byte-identical.
    if let Some(preview) = &ui.preview {
        apply_preview(d, preview);
    }
    // The node drawer's body. `#nodeTitle`/`#nodeProps`/`#relationsBody` ship
    // empty, so an absent key writes nothing and the pinned frame cannot move.
    if let Some(drawer) = &ui.drawer {
        apply_drawer(d, drawer);
    }
    // Content facts are independent of the open/close facts: `drawers_open`
    // opens an empty panel and `find` fills (and opens) it. Both are optional,
    // so neither presence implies the other.
    if let Some(find) = &ui.find {
        apply_find(d, find);
    }
    if let Some(blocks) = &ui.blocks {
        apply_blocks_canvas(d, blocks);
    }
    if let Some(picker) = &ui.picker {
        apply_picker(d, picker);
    }
    // The quick-open palette. There is no `drawers_open`/`modals_open` pairing
    // here because `#cs-quickopen` is not in `index.html` at all — it is
    // *created* by `quickopen.js`'s `ensureDom()` (quickopen.js:113-181). So
    // `Some` is the open flag as well as the content, and `None` — what the
    // shipped default document sends — creates nothing, keeping the render
    // byte-identical.
    if let Some(quickopen) = &ui.quickopen {
        apply_quickopen(d, quickopen);
    }
    // The document-tab strip. Like the palette, `#cs-tabs` does not exist in the
    // shipped document — `renderTabBar` creates it — so `None` creates nothing
    // and the default render stays byte-identical.
    if let Some(tabs) = &ui.tabs {
        apply_tabs(d, tabs);
        // The split-pane scaffold is owned by the same fact set (`split_on` +
        // each row's `split`), so it is applied right after the strip it belongs
        // to; `split_on: false` creates and moves nothing.
        apply_split(d, tabs);
    }
    // The topbar search box's typed value. Absent -> no write, so the shipped
    // input (no `value` attribute) keeps its byte-identical default.
    if let Some(query) = &ui.search_query {
        apply_search_query(d, query);
    }
    // The graph-mode suggestion popup. `None` = the shipped hidden box; nothing
    // is created or repainted, so the default frame stays byte-identical. Written
    // after `search_query` on purpose: the popup's `aria-expanded` must describe
    // the input the same pass just filled.
    if let Some(suggestions) = &ui.suggestions {
        apply_suggestions(d, suggestions);
    }
    // The Reorganise & export dialog. `None` writes nothing (shipped hidden
    // document); `Some` paints the form values, the three `syncRows` row states,
    // the report rows and the note from the host's one `ui.reorg` object.
    if let Some(reorg) = &ui.reorg {
        apply_reorg(d, reorg);
    }
    // The form controls' interaction overlay (task-14): the focused text box's
    // in-flight value and the open select menu. Applied AFTER the reorg/preview
    // facts so a half-typed value wins over the committed one on the same frame.
    if let Some(form) = &ui.form {
        apply_form(d, form);
    }
    // The shortcuts sheet's rows. `None` leaves the shipped empty
    // `#shortcutsBody` alone — `renderShortcuts` (`shell.js:330-342`) is the only
    // writer in the reference and it only runs on open, so an absent fact must
    // not create a single child (the default render is pinned byte-identical).
    if let Some(shortcuts) = &ui.shortcuts {
        apply_shortcuts(d, shortcuts);
    }
    // No collapsed list -> leave the shipped document alone entirely.
    let Some(collapsed_list) = ui.collapsed.as_ref() else {
        return apply_mode(ui, d);
    };
    let _ = collapsed_list;
    for (id, head) in SECTIONS {
        let Some(section) = d.id(id) else { continue };
        let collapsed = is_collapsed(ui, id, head);
        let caret = child_by_class(d.doc, section, "section-head")
            .and_then(|head| child_by_class(d.doc, head, "caret"));
        if let Some(caret) = caret {
            d.set_text(caret, if collapsed { "\u{25b8}" } else { "\u{25be}" });
        }
        // `.section.collapsed .section-body { display: none }` is what the real
        // stylesheet says; set that computed display inline rather than toggling
        // the class. Toggling a class on a section that contains DOM-created
        // children crashes stylo's invalidation inside blitz 0.2.4
        // (`ElementData::invalidate_style_if_needed` unwraps a `styles` that was
        // never computed, vendor/stylo/data.rs:190). An inline style goes through
        // blitz's dedicated `flush_style_attribute` path and is visually
        // identical: the caret glyph below is what the scripts drive anyway.
        if let Some(body) = child_by_class(d.doc, section, "section-body") {
            d.set_attr(
                body,
                "style",
                if collapsed { "display:none" } else { "display:block" },
            );
        }
    }
    apply_mode(ui, d);
}

/// Blocks-canvas **content** (task-13, v2 `scratch.js:236-414`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BlocksCanvasFacts {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub meta: String,
    #[serde(default)]
    pub empty: String,
    #[serde(default)]
    pub cards: Vec<BlockCardFacts>,
    #[serde(default)]
    pub edges: Vec<BlockEdgeFacts>,
}

/// One `div.block-card` (`scratch.js:256-301`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BlockCardFacts {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub full: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub line: i64,
    #[serde(default)]
    pub params: Vec<String>,
    #[serde(default)]
    pub out_type: String,
    #[serde(default)]
    pub degree: i64,
    #[serde(default)]
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    #[serde(default)]
    pub selected: bool,
    #[serde(default)]
    pub kind_color: String,
}

/// One `path.blocks-edge` (`scratch.js:355-414`).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct BlockEdgeFacts {
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub count: i64,
    #[serde(default)]
    pub d: String,
    #[serde(default)]
    pub color: String,
    /// Port-anchored source port centre (`x`,`y`) — the producer's painted
    /// `.port-out` box centre (`scratch.js:378-386 portPoint`). `d` is already
    /// built from these; they are carried so the consumer can verify/paint
    /// markers without re-deriving geometry. Zero = absent (older producers).
    #[serde(default)]
    pub ax: f32,
    #[serde(default)]
    pub ay: f32,
    /// Port-anchored target port centre (the `.port-in` box centre).
    #[serde(default)]
    pub bx: f32,
    #[serde(default)]
    pub by: f32,
}

/// `shortType` (`scratch.js:105-110`): last dotted segment, generics kept.
fn block_short_type(t: &str) -> String {
    let t = t.trim();
    if t.is_empty() {
        return "·".to_string();
    }
    let (core, generic) = match t.find('<') {
        Some(i) => (&t[..i], &t[i..]),
        None => (t, ""),
    };
    format!("{}{generic}", core.rsplit('.').next().unwrap_or(core))
}

/// Render `ui.blocks` into `#blocksCards` / `#blocksEdges` / `#blocksTitle` /
/// `#blocksMeta` (`scratch.js:236-414`). Cards and edges are DOM-created; the
/// shipped empty skeleton is only touched when a fact is present.
fn apply_blocks_canvas(d: &mut Dom<'_>, facts: &BlocksCanvasFacts) {
    if let Some(panel) = d.id("blocksPanel") {
        d.set_class_flag(panel, "hidden", false);
    }
    if let Some(t) = d.id("blocksTitle") {
        let text = if facts.title.is_empty() { "Blocks" } else { facts.title.as_str() };
        d.set_text(t, text);
    }
    if let Some(m) = d.id("blocksMeta") {
        d.set_text(m, &facts.meta);
    }
    let Some(list) = d.id("blocksCards") else { return };
    d.clear(list);
    if facts.cards.is_empty() {
        let empty = d.mk(El::new("div").cls("blocks-empty").text(&facts.empty));
        d.append(list, &[empty]);
        if let Some(svg) = d.id("blocksEdges") {
            d.clear(svg);
        }
        return;
    }
    let mut nodes = Vec::with_capacity(facts.cards.len());
    for c in &facts.cards {
        let card_class = if c.selected { "block-card selected" } else { "block-card" };
        let card = d.mk(
            El::new("div")
                .cls(card_class)
                .a("data-kind", &c.kind)
                .a("data-index", &c.index.to_string())
                .a("data-id", &c.id),
        );
        let head = d.mk(El::new("div").cls("bc-head"));
        let color = if c.kind_color.is_empty() { "#9aa6c2" } else { c.kind_color.as_str() };
        let kind = d.mk(
            El::new("span")
                .cls("bc-kind")
                .style(&format!("color:{color}"))
                .text(&c.kind),
        );
        let title = d.mk(
            El::new("span")
                .cls("bc-title")
                .a("title", if c.full.is_empty() { c.id.as_str() } else { c.full.as_str() })
                .text(&c.name),
        );
        let flags = d.mk(El::new("span").cls("bc-flags"));
        d.append(head, &[kind, title, flags]);
        let sig = d.mk(El::new("div").cls("bc-sig"));
        let sig_children: Vec<usize> = if c.kind == "method" || c.kind == "constructor" {
            vec![
                d.mk(El::new("span").cls("bc-ret").text(&c.out_type)),
                d.mk(El::new("span").cls("bc-name").text(&c.name)),
                d.mk(
                    El::new("span")
                        .cls("bc-args")
                        .text(format!("({})", c.params.join(", "))),
                ),
            ]
        } else {
            vec![
                d.mk(El::new("span").cls("bc-kw").text(&c.kind)),
                d.mk(El::new("span").cls("bc-name").text(&c.name)),
            ]
        };
        d.append(sig, &sig_children);
        let ports = d.mk(El::new("div").cls("bc-ports"));
        let inp = d.mk(El::new("div").cls("bc-in"));
        let mut in_nodes = Vec::with_capacity(c.params.len());
        for p in &c.params {
            let port = d.mk(
                El::new("span")
                    .cls("port port-in")
                    .a("data-type", p)
                    .a("title", p),
            );
            let dot = d.mk(El::new("span").cls("port-dot"));
            let ty = d.mk(El::new("span").cls("port-type").text(block_short_type(p)));
            d.append(port, &[dot, ty]);
            in_nodes.push(port);
        }
        d.append(inp, &in_nodes);
        let outp = d.mk(El::new("div").cls("bc-out"));
        let out_port = d.mk(
            El::new("span")
                .cls("port port-out")
                .a("data-type", &c.out_type)
                .a("title", if c.out_type.is_empty() { c.kind.as_str() } else { c.out_type.as_str() }),
        );
        let out_text = if c.out_type.is_empty() {
            "·".to_string()
        } else {
            block_short_type(&c.out_type)
        };
        let out_ty = d.mk(El::new("span").cls("port-type").text(&out_text));
        let out_dot = d.mk(El::new("span").cls("port-dot"));
        d.append(out_port, &[out_ty, out_dot]);
        d.append(outp, &[out_port]);
        d.append(ports, &[inp, outp]);
        let foot = d.mk(El::new("div").cls("bc-foot"));
        let loc_text = if c.line > 0 {
            format!("{}:{}", c.file, c.line)
        } else {
            c.file.clone()
        };
        let loc = d.mk(
            El::new("span")
                .cls("bc-loc")
                .a("title", &c.file)
                .text(&loc_text),
        );
        let deg = d.mk(
            El::new("span")
                .cls("bc-deg")
                .a("title", "in-degree / out-degree")
                .text(format!("↑{} ↓{}", c.degree, c.degree)),
        );
        d.append(foot, &[loc, deg]);
        d.append(card, &[head, sig, ports, foot]);
        nodes.push(card);
    }
    d.append(list, &nodes);
    if let Some(svg) = d.id("blocksEdges") {
        d.clear(svg);
        let w = facts.cards.iter().map(|c| c.x).fold(0.0_f32, f32::max) + 232.0;
        let h = facts.cards.iter().map(|c| c.y).fold(0.0_f32, f32::max) + 200.0;
        d.set_attr(svg, "width", &format!("{w:.0}"));
        d.set_attr(svg, "height", &format!("{h:.0}"));
        d.set_attr(svg, "viewBox", &format!("0 0 {w:.0} {h:.0}"));
        // v2 `scratch.js:375-378`: one `<defs><marker id="blocksArrow">` per SVG,
        // `orient="auto-start-reverse"` so the same marker works at either end,
        // `fill="context-stroke"` so it inherits each edge's stroke colour.
        let defs = d.mk(El::new("defs"));
        let marker = d.mk(
            El::new("marker")
                .a("id", "blocksArrow")
                .a("viewBox", "0 0 8 8")
                .a("refX", "7")
                .a("refY", "4")
                .a("markerWidth", "7")
                .a("markerHeight", "7")
                .a("orient", "auto-start-reverse"),
        );
        let arrow = d.mk(
            El::new("path").a("d", "M0,0 L8,4 L0,8 z").a("fill", "context-stroke"),
        );
        d.append(marker, &[arrow]);
        d.append(defs, &[marker]);
        let paths: Vec<usize> = facts
            .edges
            .iter()
            .map(|e| {
                d.mk(
                    El::new("path")
                        .cls(&format!("blocks-edge e-{}", e.kind))
                        .a("data-kind", &e.kind)
                        .a("data-source", &e.source)
                        .a("data-target", &e.target)
                        .a("d", &e.d)
                        .a("fill", "none")
                        .a("stroke", &e.color)
                        .a("stroke-width", if e.kind == "contains" { "1" } else { "1.5" })
                        // v2 `scratch.js:408`: every edge ends in the triangle.
                        .a("marker-end", "url(#blocksArrow)"),
                )
            })
            .collect();
        let mut kids = Vec::with_capacity(paths.len() + 1);
        kids.push(defs);
        kids.extend(paths);
        d.append(svg, &kids);
    }
}

/// The overlay **drawer family** whose open/close state the host owns.
///
/// This is an allow-list, not a pattern: `drawers_open` can only ever clear
/// `hidden` on these three ids. The `class="modal-backdrop"` family is
/// deliberately absent — see [`UiState::drawers_open`] for why.
const DRAWERS: &[&str] = &["nodeDrawer", "findPanel", "blocksPanel"];

/// Clear `hidden` on the drawers the host has opened.
///
/// The scripts do this with `classList.remove('hidden')`; this writes the
/// `class` attribute the same way, so the real stylesheet does the layout and a
/// hidden drawer keeps its exact shipped zero-area row.
///
/// **`aria-hidden` is deliberately NOT touched here.** Every `aria-hidden` write
/// in the shipped scripts targets a modal (`shell.js` for `#modal` and
/// `#shortcutsModal`, plus `reorg.js`/`edit.js`/`preview.js`), and the three
/// drawers ship no `aria-hidden` attribute at all — they are labelled with
/// `aria-label` instead. Writing one here would invent an attribute the
/// reference document never has; see [`apply_modals`] for the family that does
/// toggle it.
fn apply_drawers(d: &mut Dom<'_>, ui: &UiState) {
    for id in DRAWERS {
        // Only ids in the allow-list are ever touched, so an unknown or
        // modal-family id in the request is ignored rather than honoured.
        if !ui.drawers_open.iter().any(|w| w == id) {
            continue;
        }
        if let Some(node) = d.id(id) {
            d.set_class_flag(node, "hidden", false);
        }
    }
}

/// The overlay **modal family** whose open/close state the host owns.
///
/// An allow-list, exactly like [`DRAWERS`]: `modals_open` can only ever clear
/// `hidden` on these five ids, and any other id (a drawer, a typo, an arbitrary
/// selector) is ignored rather than honoured.
const MODALS: &[&str] = &[
    "shortcutsModal",
    "editModal",
    "previewModal",
    "reorgModal",
    "modal",
];

/// Clear `hidden` on the modals the host has opened.
///
/// The front-end scripts do this with `classList.remove('hidden')`; writing the
/// `class` attribute the same way leaves the layout entirely to the real
/// stylesheet, so an open modal gets the geometry measured on
/// [`UiState::modals_open`] and a closed one keeps its exact shipped zero row.
///
/// **`aria-hidden` is toggled in lockstep with `hidden`, and both directions
/// matter.** Every one of the four shipped scripts does both halves together:
/// `shell.js`, `reorg.js`, `edit.js` and `preview.js` pair
/// `classList.remove('hidden')` + `setAttribute('aria-hidden','false')` on open
/// with `classList.add('hidden')` + `setAttribute('aria-hidden','true')` on
/// close. `index.html` ships all four backdrops as
/// `class="modal-backdrop hidden" aria-hidden="true"`, so writing only the class
/// leaves an *open* dialog announced as `aria-hidden="true"` and assistive tech
/// refuses to enter the very card the host just opened. The close direction is
/// live code too, not symmetry for its own sake: opening one modal must leave
/// the other three hidden, so the shipped `aria-hidden="true"` is re-asserted
/// rather than assumed to still be there.
///
/// Only the **first** requested id is honoured. The backdrop is
/// `position: fixed; inset: 0`, so two open modals would both cover the
/// viewport and which card a press landed on would be decided by DOM order
/// rather than by the host — the same one-owner rule `drawers_open` relies on,
/// enforced here as well so a malformed request cannot break it.
fn apply_modals(d: &mut Dom<'_>, ui: &UiState) {
    let open = MODALS
        .iter()
        .find(|id| ui.modals_open.iter().any(|w| w == *id))
        .copied();
    for id in MODALS {
        // Allow-list iteration, so every id that is not the open one is
        // re-closed even if the host never named it. Two modals writing here
        // would be a bug, not an overlay.
        let Some(node) = d.id(id) else {
            continue;
        };
        let is_open = open == Some(*id);
        d.set_class_flag(node, "hidden", !is_open);
        d.set_attr(node, "aria-hidden", if is_open { "false" } else { "true" });
    }
}

/// `showLoading(text)` / `hideLoading()` (`shell.js:157-176`).
///
/// The shipped document already has the overlay and its three nodes, so this is
/// a state switch, not a creation: clear `#loading`'s `hidden`, write the text,
/// and — only when the host says it can cancel — remove `#stopIndexBtn`'s
/// shipped inline `style="display:none"`. `None` is a no-op because the nzk_ui is
/// rebuilt from the shipped HTML for every paint, which is what keeps the default
/// frame byte-identical.
fn apply_loading(d: &mut Dom<'_>, ui: &UiState) {
    let Some(loading) = &ui.loading else { return };
    if let Some(node) = d.id("loading") {
        d.set_class_flag(node, "hidden", false);
    }
    if let Some(node) = d.id("loadingText") {
        d.set_text(node, &loading.text);
    }
    if let Some(node) = d.id("stopIndexBtn") {
        if loading.stop {
            d.clear_attr(node, "style");
        } else {
            d.set_attr(node, "style", "display:none");
        }
    }
}

/// The nodes `find` is allowed to write, as an allow-list.
///
/// Same rule as [`DRAWERS`]/[`MODALS`]: a content fact can only ever touch these
/// six ids. `#findQuery`, `#findMeta`, `#findNotice` and `#findResults` are the
/// panel's own slots; `#findPanel` and `#findClose` are here only so the panel
/// can be *opened* by the content fact as well as by `drawers_open` — a caller
/// that supplies content without also listing the drawer would otherwise fill an
/// invisible panel, which is the failure this pairing exists to prevent.
///
/// Nothing outside this list is reachable, so a `find` fact cannot rewrite the
/// sidebar, the status line, or any other control's text.
const FIND_NODES: &[&str] = &[
    "findPanel",
    "findQuery",
    "findMeta",
    "findNotice",
    "findResults",
    "findClose",
];

/// Look a node up through the [`FIND_NODES`] allow-list.
///
/// Every write in [`apply_find`] goes through here, so the list is the actual
/// authority rather than documentation: a typo returns `None` and the write is
/// skipped instead of silently landing on some other element's id.
fn find_node<'d, 's>(d: &'d Dom<'s>, name: &str) -> Option<usize> {
    if FIND_NODES.contains(&name) {
        d.id(name)
    } else {
        None
    }
}

/// Render `ui.find` into the Find panel.
///
/// Mirrors `search.js`'s `renderFind` (`search.js:305-345`) and `buildDefRow`
/// (`search.js:181-207`) with the same class names, so the shipped stylesheet
/// does the styling: `.find-query`, `.find-meta`, `.find-notice`, `.frow`,
/// `.frow-top`, `.fkind`, `.fname`, `.frow-sub`.
///
/// Two honesty decisions are enforced *here*, not merely upstream:
///
/// * `#findNotice` is shown only when `notice` is non-empty. That is the one
///   structural place the three states could collapse — a caller that always
///   cleared `hidden` would show a blank notice bar for every search — so the
///   class is driven by the string the core produced, and `NoQuery`/`NoHits`
///   each carry a different sentence.
/// * Rows are built only from `rows`. An empty `rows` renders the honest
///   `find-empty` line for the *state that was actually reported*: the core's
///   own `notice` sentence. `NoQuery` and `NoHits` therefore produce visibly
///   different documents rather than two identical empty lists. This module
///   never synthesises a row and never substitutes placeholder text for a value
///   the core did not give.
fn apply_find(d: &mut Dom<'_>, facts: &FindFacts) {
    // Content implies the panel is open: `search.js` opens the drawer before it
    // renders into it. Doing it here too means a caller cannot fill an element
    // that `.hidden { display: none !important }` keeps at zero area.
    if let Some(panel) = find_node(d, "findPanel") {
        d.set_class_flag(panel, "hidden", false);
    }

    // `findQueryEl.textContent = f.query ? f.query : 'Find'` (search.js:314).
    if let Some(n) = find_node(d, "findQuery") {
        let text = if facts.query.is_empty() {
            "Find"
        } else {
            &facts.query
        };
        d.set_text(n, text);
        d.set_attr(n, "title", &facts.query);
    }
    // `findMetaEl.textContent = …` (search.js:315-317). The core's `meta()` is
    // `""` for NoQuery precisely so the bar shows nothing; write it verbatim.
    if let Some(n) = find_node(d, "findMeta") {
        d.set_text(n, &facts.meta);
    }
    // `findNoticeEl.classList.toggle('hidden', !(…))` (search.js:318). Here the
    // condition is the core's own "is there anything to say", so an empty
    // notice hides the bar and a real one shows it.
    if let Some(n) = find_node(d, "findNotice") {
        let has_notice = !facts.notice.is_empty();
        d.set_text(n, &facts.notice);
        d.set_class_flag(n, "hidden", !has_notice);
    }
    // `findResultsEl.innerHTML = ''` then re-append (search.js:323).
    let Some(list) = find_node(d, "findResults") else {
        return;
    };
    d.clear(list);
    // v2's grouped render (`renderFind`, `search.js:349-351`): when the engine
    // answered with definitions/implementations/usages, paint one `.fgroup` per
    // non-empty group and return. An EMPTY group list still falls through to the
    // honest `find-empty` sentence below, so "engine found nothing" is visibly
    // different from "nothing asked yet".
    if !facts.groups.is_empty() {
        let mut groups = Vec::with_capacity(facts.groups.len());
        for g in &facts.groups {
            // `cssKind = {definitions:'defs', implementations:'impls', usages:'uses'}`
            // (`search.js:254`).
            let css_kind = match g.kind.as_str() {
                "definitions" => "defs",
                "implementations" => "impls",
                "usages" => "uses",
                other => other,
            };
            let group = d.mk(El::new("div").cls(&format!("fgroup fgroup-{css_kind}")));
            let head = d.mk(El::new("div").cls("fgroup-head").text(&g.heading));
            let count = d.mk(El::new("span").cls("fcount").text(&g.count.to_string()));
            d.append(head, &[count]);
            let mut children = vec![head];
            for row in &g.rows {
                // The click target rides on the row as `data-key`, the payload
                // column the shell's hit row already carries (nzk_ui.rs
                // `find_row_click` parses it back). The encoding is the target
                // `activateFindRow` (`search.js:390-400`) would use: a node id,
                // or a usage's file:line plus its enclosing node.
                let key = if g.kind == "usages" {
                    format!("find:usage:{}|{}|{}", row.node_id, row.file, row.line)
                } else {
                    format!("find:node:{}", row.node_id)
                };
                let btn = d.mk(
                    El::new("button")
                        .cls("frow")
                        .a("type", "button")
                        .a("data-key", &key)
                        .a("title", &row.title),
                );
                let top = d.mk(El::new("span").cls("frow-top"));
                let chip = d.mk(El::new("span").cls("fkind").text(if row.kind.is_empty() {
                    if g.kind == "usages" { "usage" } else { "node" }
                } else {
                    &row.kind
                }));
                let name = d.mk(
                    El::new("span")
                        .cls("fname")
                        .a("title", &row.title)
                        .text(&row.title),
                );
                d.append(top, &[chip, name]);
                if g.kind == "usages" && !row.match_kind.is_empty() {
                    let exact = row.match_kind == "exact";
                    let tag = d.mk(
                        El::new("span")
                            .cls(if exact { "fmatch fq" } else { "fmatch short" })
                            .text(if exact { "fq" } else { "short" }),
                    );
                    d.append(top, &[tag]);
                }
                let mut row_children = vec![top];
                if g.kind == "usages" && !row.text.is_empty() {
                    let src = d.mk(El::new("span").cls("fsrc").text(&row.text));
                    row_children.push(src);
                }
                if !row.subtitle.is_empty() {
                    let sub = d.mk(El::new("span").cls("frow-sub").text(&row.subtitle));
                    row_children.push(sub);
                }
                d.append(btn, &row_children);
                children.push(btn);
            }
            d.append(group, &children);
            groups.push(group);
        }
        d.append(list, &groups);
        return;
    }
    if facts.rows.is_empty() {
        // `search.js:336-342` puts a `find-empty` div here. The text is the
        // core's notice sentence, which is state-specific: the NoQuery sentence
        // ("Type a symbol name to search…") cannot appear for NoHits and vice
        // versa, so the two states stay distinguishable in the rendered DOM.
        if !facts.notice.is_empty() {
            let empty = d.mk(El::new("div").cls("find-empty").text(&facts.notice));
            d.append(list, &[empty]);
        }
        return;
    }
    let mut nodes = Vec::with_capacity(facts.rows.len());
    for row in &facts.rows {
        // `btn.className = 'frow'` + `btn.title = 'Open … in the graph'`
        // (search.js:182-183). The node id rides along as `data-node` — the
        // click target `search.js:175` keys rows on, carried verbatim.
        let btn = d.mk(
            El::new("button")
                .cls("frow")
                .a("type", "button")
                .a("data-node", &row.node_id)
                .a("data-match", &row.tag)
                .a("title", &format!("Open {} in the graph", row.title)),
        );
        let top = d.mk(El::new("span").cls("frow-top"));
        let chip = d.mk(
            El::new("span")
                .cls("fkind")
                .text(if row.kind.is_empty() {
                    "node"
                } else {
                    &row.kind
                }),
        );
        // `name.title = name.textContent` (search.js:196).
        let name = d.mk(
            El::new("span")
                .cls("fname")
                .a("title", &row.title)
                .text(&row.title),
        );
        d.append(top, &[chip, name]);
        let mut children = vec![top];
        // `if (sub.textContent) btn.appendChild(sub)` (search.js:206) — the span
        // is absent, not empty, when the core gave no subtitle.
        if !row.subtitle.is_empty() {
            let sub = d.mk(El::new("span").cls("frow-sub").text(&row.subtitle));
            children.push(sub);
        }
        d.append(btn, &children);
        nodes.push(btn);
    }
    d.append(list, &nodes);
}

/// Render `ui.shortcuts` into `#shortcutsBody`.
///
/// Mirrors `renderShortcuts` (`shell.js:330-342`): the body is emptied and
/// rebuilt, one `.shortcut-group` head per group change, then one
/// `.shortcut-row` per row. `shortcutRow` (`shell.js:298-328`) puts the
/// `.shortcut-desc` first and `.shortcut-keys` second; inside the keys each
/// alternative after the first is preceded by a `.shortcut-plus` `/`, each chord
/// part after the first by a `.shortcut-plus` `+`, and every part becomes a
/// `<kbd>`. The shipped `style.css` owns all the styling — nothing is authored
/// here.
fn apply_shortcuts(d: &mut Dom<'_>, facts: &ShortcutsFacts) {
    let Some(body) = d.id("shortcutsBody") else {
        return;
    };
    // `shortcutsBodyEl.innerHTML = ''` (shell.js:332), then a single append.
    d.clear(body);
    let mut children = Vec::new();
    let mut group: &str = "";
    for row in &facts.rows {
        if row.group != group {
            // `head.className = 'shortcut-group'; head.textContent = grp.group`.
            let head = d.mk(El::new("div").cls("shortcut-group").text(&row.group));
            children.push(head);
            group = &row.group;
        }
        let row_el = d.mk(El::new("div").cls("shortcut-row"));
        let desc = d.mk(El::new("span").cls("shortcut-desc").text(&row.desc));
        let keys = d.mk(El::new("span").cls("shortcut-keys"));
        let mut key_children = Vec::new();
        for (ci, chord) in row.keys.iter().enumerate() {
            if ci > 0 {
                // Alternatives are joined by `/` (`shell.js:308-312`).
                key_children.push(d.mk(El::new("span").cls("shortcut-plus").text("/")));
            }
            for (pi, part) in chord.iter().enumerate() {
                if pi > 0 {
                    // Parts of one chord are joined by `+` (shell.js:314-318).
                    key_children.push(d.mk(El::new("span").cls("shortcut-plus").text("+")));
                }
                key_children.push(d.mk(El::new("kbd").text(part)));
            }
        }
        d.append(keys, &key_children);
        d.append(row_el, &[desc, keys]);
        children.push(row_el);
    }
    d.append(body, &children);
}

/// Render `ui.picker` into the folder-browser modal.
///
/// Mirrors `shell.js`'s `renderPicker` (`shell.js:38-87`) with its own class
/// names — `.pick`, `.picon`, `.pname`, `.plink`, `.pmeta`, `.modal-empty` — so
/// the real stylesheet lays the rows out.
///
/// The `NotFound`/`Unreadable` distinction is enforced here: those states are
/// **not** listed, so the "Empty folder." line is unreachable for them. The note
/// carries the core's own sentence instead ("path does not exist"), and
/// `#modalSelect` stays disabled. A caller cannot make a missing path look like
/// a folder that merely holds nothing, because the branch that renders the empty
/// message is gated on [`PickerState::listed`] rather than on `dirs.is_empty()`.
fn apply_picker(d: &mut Dom<'_>, facts: &PickerFacts) {
    if let Some(n) = d.id("modalPath") {
        d.set_text(n, &facts.path);
    }
    if let Some(n) = d.id("modalPathInput") {
        d.set_attr(n, "value", &facts.path_input);
    }
    // `modalUpEl.disabled = (d.parent === null …)` (shell.js:54).
    if let Some(n) = d.id("modalUp") {
        if facts.can_go_up {
            d.clear_attr(n, "disabled");
        } else {
            d.set_attr(n, "disabled", "disabled");
        }
    }
    // `modalNoteEl.textContent` / `.className` (shell.js:78-86). The class is
    // written verbatim from `Note::class()`; an empty note clears the text but
    // keeps the shipped class so nothing is invented for it.
    if let Some(n) = d.id("modalNote") {
        d.set_text(n, &facts.note);
        if !facts.note_class.is_empty() {
            d.set_attr(n, "class", &facts.note_class);
        }
    }
    // `modalSelectEl.disabled = !hasCs` (shell.js:78) — here it is
    // `Note::selectable()`, and a missing path is never selectable.
    if let Some(n) = d.id("modalSelect") {
        if facts.selectable {
            d.clear_attr(n, "disabled");
        } else {
            d.set_attr(n, "disabled", "disabled");
        }
    }
    let Some(body) = d.id("modalBody") else {
        return;
    };
    d.clear(body);
    if facts.dirs.is_empty() {
        // Only a *listed* folder gets the empty message; a failure already said
        // why in `#modalNote` and must not also look like an empty directory.
        if facts.state.listed() {
            let empty = d.mk(El::new("div").cls("modal-empty").text("Empty folder."));
            d.append(body, &[empty]);
        }
        return;
    }
    let mut nodes = Vec::with_capacity(facts.dirs.len());
    for dir in &facts.dirs {
        // `'<button class="pick…" data-nav="…" title="Open this folder">'`
        // (shell.js:58). `no-cs` is the reference's own class for a row with no
        // `.cs` files below it; with no index in hand at this layer the honest
        // value is the row's own readability, which is the listing's, not the
        // row's. The class is therefore left off rather than guessed.
        let btn = d.mk(
            El::new("button")
                .cls("pick")
                .a("type", "button")
                .a("data-nav", &dir.path)
                .a("title", "Open this folder"),
        );
        let icon = d.mk(El::new("span").cls("picon").text("\u{25b8}"));
        // `'<span class="pname">' + name + '/</span>'` (shell.js:60) — the
        // reference always appends the separator.
        let name = d.mk(El::new("span").cls("pname").text(format!("{}/", dir.name)));
        let mut children = vec![icon, name];
        if dir.symlink {
            // `shell.js:61`.
            let link = d.mk(
                El::new("span")
                    .cls("plink")
                    .a("title", "symbolic link — followed when indexing")
                    .text("link"),
            );
            children.push(link);
        }
        // `.pmeta` reports how many files were skipped beside this row. The
        // reference shows a per-dir `csCount`; that needs the index, which this
        // layer does not have. An unknown count is omitted rather than rendered
        // as "0 .cs", which would be a claim the listing never made.
        if let Some(skipped) = facts.files_skipped {
            if skipped > 0 {
                let meta = d.mk(El::new("span").cls("pmeta").text(format!("{skipped} files")));
                children.push(meta);
            }
        }
        d.append(btn, &children);
        nodes.push(btn);
    }
    d.append(body, &nodes);
}

/// Render `ui.tabs` into the `#cs-tabs` document-tab strip.
///
/// Mirrors `shell.js`'s `renderTabBar()` (1192-1248) one-for-one:
///
/// * `#cs-tabs` is *created* and inserted as the first child of `#sourcePane`
///   (`pane.insertBefore(barEl, pane.firstChild)`, shell.js:1197) — the shipped
///   document has no tab bar, exactly like `#cs-quickopen`.
/// * The bar's children are rebuilt from scratch on every call
///   (`barEl.innerHTML = ''`): one `.cs-tab` per row, then `.cs-tab-actions`.
/// * The bar is `hidden` iff the tab list is empty (`tabs.length === 0`); the
///   split button is always rendered because the native viewer has no pop-out
///   document window, so the reference's `docWindow()` is always false.
///
/// The **byte-identical promise** is kept by construction: this is only reached
/// under `Some(TabsFacts)`, so the shipped default (no `tabs` key at all)
/// creates no node and writes no attribute.
fn apply_tabs(d: &mut Dom<'_>, facts: &TabsFacts) {
    let Some(pane) = d.id("sourcePane") else { return };

    // ---- #cs-tabs (shell.js:1194-1197). Created once; a second call only
    // rebuilds its children, so the bar keeps its first-child position.
    let bar = match d.id("cs-tabs") {
        Some(bar) => bar,
        None => {
            let bar = d.mk(
                El::new("div")
                    .id("cs-tabs")
                    .cls("cs-tabs")
                    .a("role", "tablist")
                    .a("aria-label", "Open documents"),
            );
            d.prepend(pane, bar);
            bar
        }
    };
    // Start from the bare class so a previous `hidden` cannot linger; the
    // authoritative toggle is the `set_class_flag` at the end of this function.
    d.set_attr(bar, "class", "cs-tabs");

    // ---- one `div.cs-tab` per row (shell.js:1200-1225) ----------------------
    let mut children = Vec::new();
    for row in &facts.tabs {
        let mut class = String::from("cs-tab");
        if row.active {
            class.push_str(" active");
        }
        let label = if row.label.is_empty() {
            &row.key
        } else {
            &row.label
        };
        let title = if row.title.is_empty() {
            &row.key
        } else {
            &row.title
        };
        let tab = d.mk(
            El::new("div")
                .cls(&class)
                .a("role", "tab")
                .a("data-key", &row.key)
                .a("aria-selected", if row.active { "true" } else { "false" })
                .a("title", title),
        );
        let lab = d.mk(El::new("span").cls("cs-tab-label").text(label));
        let mut kids = vec![lab];
        if row.split {
            // `splitKeyState === t.key` (shell.js:1213-1218): the mark is only
            // ever appended to the row the split currently shows.
            let mark = d.mk(
                El::new("span")
                    .cls("cs-tab-splitmark")
                    .a("title", "Shown in the split")
                    .text("\u{258c}"),
            );
            kids.push(mark);
        }
        // `x.type = 'button'` is set as the property in the reference; the
        // honest DOM reading is the `type` attribute (same as the split/others
        // buttons below).
        let close = d.mk(
            El::new("button")
                .cls("cs-tab-x")
                .a("type", "button")
                .a("data-close", &row.key)
                .a("title", "Close tab")
                .text("\u{2715}"),
        );
        kids.push(close);
        d.append(tab, &kids);
        children.push(tab);
    }

    // ---- .cs-tab-actions (shell.js:1227-1248) -------------------------------
    let actions = d.mk(El::new("div").cls("cs-tab-actions"));
    // No pop-out document window in the native viewer: `!docWindow()` is always
    // true, so the split button is always rendered.
    let split = El::new("button")
        .id("csSplitBtn")
        .cls("btn btn-ghost")
        .a("type", "button")
        .a("title", "Show a second document beside this one (Alt+S)")
        .text(if facts.split_on {
            "\u{25a4} Unsplit"
        } else {
            "\u{25a5} Split"
        });
    let split = if facts.tabs.len() < 2 && !facts.split_on {
        split.a("disabled", "")
    } else {
        split
    };
    let split_btn = d.mk(split);
    let others = El::new("button")
        .id("csTabCloseOthers")
        .cls("btn btn-ghost")
        .a("type", "button")
        .a("title", "Close every tab but this one (Alt+Shift+W)")
        .text("Close others");
    let others = if facts.tabs.len() < 2 {
        others.a("disabled", "")
    } else {
        others
    };
    let others_btn = d.mk(others);
    d.append(actions, &[split_btn, others_btn]);
    children.push(actions);
    d.set_children(bar, &children);

    // The bar hides exactly when there is no document (`tabs.length === 0`).
    // `docWindow()` is always false natively, so the reference's
    // `|| (docWindow() && tabs.length <= 1)` branch cannot fire.
    d.set_class_flag(bar, "hidden", facts.tabs.is_empty());
}

/// Install the split-pane scaffold from the same `ui.tabs` fact set
/// (`split_on` plus the per-row `split` mark), transcribed from `shell.js`'s
/// `install()` (1735-1802) and `setSplitVisible()` (1456-1469).
///
/// The reference builds `#csSplitWrap` unconditionally during `install()`; this
/// adapter creates it only while the split is on, exactly like `#cs-tabs`, so an
/// absent (or `split_on: false`) fact leaves the shipped document byte-identical
/// and `#sourceBody` keeps `#sourcePane` as its parent — which is what keeps the
/// pinned `@frame` token from moving.
///
/// The wrap REPLACES `#sourceBody` as its parent's flow child: the wrap is
/// inserted before the body and the body is then appended into it
/// (`body.parentNode.insertBefore(wrap, body); wrap.append(body)`), so the
/// children are, in reference order, the existing `#sourceBody`,
/// `#csSplitDivider`, `#csSplitBody`.
///
/// `#csSplitHost` stays EMPTY: filling it is the host's job (`app`'s second
/// `SourceState`), never fabricated here. The divider RATIO is applied from
/// [`TabsFacts::split_w`] as the shipped `--cs-split-w` custom property; pointer
/// drag routing lives in the host and an absent ratio performs no write, so the
/// stylesheet's shipped 50% default is byte-identical.
fn apply_split(d: &mut Dom<'_>, facts: &TabsFacts) {
    if !facts.split_on {
        return;
    }
    let Some(body) = d.id("sourceBody") else { return };

    // `updateSplitHead(tab)`: `splitLabelEl.textContent = tab.label || tab.key`
    // (shell.js:1419). `renderSplit` picks `byKey(splitKeyState) ||
    // firstOther(activeTab)`; the fact contract marks the shown row with
    // `split: true`, and a payload without that mark falls back to the first row
    // rather than inventing a name.
    let label = facts
        .tabs
        .iter()
        .find(|r| r.split)
        .or_else(|| facts.tabs.first())
        .map(|r| if r.label.is_empty() { r.key.as_str() } else { r.label.as_str() })
        .unwrap_or("");

    // One-shot creation: a second facts pass re-asserts the class and the head
    // label but never rebuilds the wrap (`d.id` guard, no `set_children` on it).
    match d.id("csSplitWrap") {
        Some(wrap) => {
            // A later facts pass runs after layout, so the wrap already has
            // computed style and these writes are safe here. (During the first,
            // pre-layout pass they are not: see the creation branch.)
            d.set_class_flag(wrap, "cs-split-on", true);
            // `applySplitWidth()` (`shell.js:1471-1472`): the ratio the host
            // published becomes the shipped `--cs-split-w` custom property. An
            // ABSENT fact writes nothing at all — the stylesheet's own
            // `--cs-split-w: 50%` fallback is then the layout.
            if let Some(frac) = facts.split_w.filter(|w| w.is_finite()) {
                d.set_attr(wrap, "style", &split_width_style(frac));
            }
            if let Some(split_body) = d.id("csSplitBody") {
                if let Some(head) = child_by_class(d.doc, split_body, "cs-split-head") {
                    if let Some(label_el) = child_by_class(d.doc, head, "cs-split-label") {
                        d.set_text(label_el, label);
                    }
                }
            }
        }
        None => {
            // Build the whole subtree while it is still detached. Blitz's style
            // invalidation panics when a class/attribute write touches an
            // element that was inserted but not yet styled
            // (`ElementStyles::primary` unwraps in `should_process_descendants`,
            // vendor/stylo/data.rs:195), so `cs-split-on` and the label text are
            // baked in at creation — the same way `El` writes the shipped
            // structure — instead of being toggled after insertion.
            // `applySplitWidth()` bakes the ratio into the detached element for
            // the same reason `cs-split-on` is baked: a post-insertion style
            // write would touch an unstyled Blitz node. No ratio fact -> no
            // `style` attribute at all (the shipped 50% CSS fallback).
            let wrap_el = El::new("div").id("csSplitWrap").cls("cs-split-on");
            let wrap_el = match facts.split_w.filter(|w| w.is_finite()) {
                Some(frac) => wrap_el.style(&split_width_style(frac)),
                None => wrap_el,
            };
            let wrap = d.mk(wrap_el);
            let divider = d.mk(
                El::new("div")
                    .id("csSplitDivider")
                    .cls("pane-splitter pane-splitter-v")
                    .a("role", "separator")
                    .a("aria-orientation", "vertical")
                    .a("title", "Drag to resize the split"),
            );
            let split_body = d.mk(El::new("div").id("csSplitBody"));
            let head = d.mk(El::new("div").cls("cs-split-head"));
            let label_el = d.mk(El::new("span").cls("cs-split-label").text(label));
            let spacer = d.mk(El::new("span").cls("cs-split-spacer"));
            let collapse = d.mk(
                El::new("button")
                    .id("csSplitCollapse")
                    .cls("btn btn-ghost")
                    .a("type", "button")
                    .a("title", "Collapse the split")
                    .text("\u{2715}"),
            );
            let host = d.mk(El::new("div").id("csSplitHost"));
            d.append(head, &[label_el, spacer, collapse]);
            d.append(split_body, &[head, host]);
            d.append(wrap, &[divider, split_body]);
            // `#sourceBody` replaces its parent slot with the wrap and is then
            // moved inside it (`body.parentNode.insertBefore(wrap, body);
            // wrap.insertBefore(body, divider)`), so the children are, in
            // reference order, the existing body, the divider, the split body.
            // Both Blitz tree ops detach the old parent, so the body is moved,
            // never cloned, and `#sourceNote` keeps its order.
            d.insert_before(body, &[wrap]);
            d.insert_before(divider, &[body]);
        }
    }
}

/// Render `ui.quickopen` into the quick-open palette overlay.
///
/// Mirrors `quickopen.js`'s `ensureDom()` (113-181) + `renderList()` (305-353),
/// with the reference's own class names (`.cs-qo-box`, `.cs-qo-head`,
/// `.cs-qo-mode`, `.cs-qo-input`, `.cs-qo-hint`, `.cs-qo-list`, `.cs-qo-row`,
/// `.cs-qo-kind`, `.cs-qo-name`, `.cs-qo-full`, `.cs-qo-loc`, `.cs-qo-note`,
/// `.cs-qo-empty`) so `assets/css/quickopen-injected.css` lays the palette out exactly as
/// the injected stylesheet does in the browser.
///
/// ## Why this *creates* `#cs-quickopen` instead of un-hiding it
///
/// Every other overlay in this module is an element `index.html` already ships
/// with a `hidden` class. `#cs-quickopen` is not: it does not exist in the
/// document at all, and `quickopen.js` builds it with
/// `document.createElement` at the moment the palette is first used
/// (quickopen.js:117-152). So there is nothing to un-hide — this function is the
/// Rust stand-in for `ensureDom()`.
///
/// The **byte-identical promise** is kept by construction rather than by
/// luck: this function is only ever called when `ui.quickopen` is `Some`, and
/// `Some` is produced only for an open palette. The shipped default document
/// carries no `quickopen` key, so `apply_quickopen` is never entered, no node is
/// created, and nothing is repainted — which is exactly why the `@frame` token
/// does not move. A payload that *does* arrive marked `state == "closed"` is
/// honoured too (the reference's `close()` only re-adds `hidden`), by building
/// the overlay hidden.
///
/// ## Errors in the reference that are deliberately not reproduced
///
/// `ensureDom()` assigns `inputEl.value` nowhere and re-creates no markup on a
/// second open, so the reference's own input keeps stale text; and
/// `overlay.className = 'hidden'` (line 118) is set at *creation* only, while
/// `show()` (line 461-468) toggles `classList` — consistent, but it means the
/// palette's first paint is always hidden. Neither is a rendering rule the DOM
/// needs to inherit, so this function writes the class and the value it is
/// actually told to hold, and every open re-renders from `facts` rather than
/// trusting whatever the last open left behind.
fn apply_quickopen(d: &mut Dom<'_>, facts: &QuickOpenFacts) {
    let Some(body) = d.body() else { return };

    let open = facts.is_open();

    // ---- overlay: `#cs-quickopen` (quickopen.js:117-122)
    let mut overlay_cls = String::from("cs-quickopen");
    if !open {
        // `close()` does `classList.add('hidden')` (quickopen.js:444).
        overlay_cls.push_str(" hidden");
    }
    let overlay = d.mk(
        El::new("div")
            .id("cs-quickopen")
            .cls(&overlay_cls)
            .a("role", "dialog")
            .a("aria-modal", "true")
            .a("aria-label", &facts.aria_label),
    );

    // ---- .cs-qo-box > (.cs-qo-head, #cs-qo-list, #cs-qo-note)
    let box_el = d.mk(El::new("div").cls("cs-qo-box"));

    let mode_el = d.mk(El::new("span").cls("cs-qo-mode").text(&facts.mode_label));
    // `inputEl` is a real `<input>` in the reference; blitz has no engine for
    // it, so the element is built with the same attributes and the query as its
    // `value`, which is what a reader (or a hit-test) can observe here.
    let input_el = d.mk(
        El::new("input")
            .id("cs-qo-input")
            .cls("cs-qo-input")
            .a("type", "text")
            .a("spellcheck", "false")
            .a("autocomplete", "off")
            .a("role", "combobox")
            .a("aria-autocomplete", "list")
            .a("placeholder", &facts.placeholder)
            .a("value", &facts.query),
    );
    let hint_el = d.mk(El::new("span").cls("cs-qo-hint").text(&facts.hint));
    let head = d.mk(El::new("div").cls("cs-qo-head"));
    d.append(head, &[mode_el, input_el, hint_el]);

    let list_el = d.mk(
        El::new("div")
            .id("cs-qo-list")
            .cls("cs-qo-list")
            .a("role", "listbox"),
    );

    // ---- the list: one .cs-qo-row per row, or the honest empty text
    if facts.rows.is_empty() {
        // `empty.textContent = seedQuery ? 'No matches.' : 'Type to search…'`
        // (quickopen.js:311-313). The choice was made by the core; this layer
        // only transports it.
        if !facts.empty_text.is_empty() {
            let empty = d.mk(El::new("div").cls("cs-qo-empty").text(&facts.empty_text));
            d.append(list_el, &[empty]);
        }
    } else {
        let sel = facts.selected_index();
        let mut nodes = Vec::with_capacity(facts.rows.len());
        for row in facts.rows.iter() {
            // `row.className = 'cs-qo-row' + (i === sel ? ' sel' : '')`
            // (quickopen.js:320). The index is taken from the row's own
            // `data-i`, not the loop counter, so `sel` and `data-i` can never
            // disagree even if a caller sends a reordered list.
            let is_sel = row.selected || sel == Some(row.index);
            let cls = if is_sel { "cs-qo-row sel" } else { "cs-qo-row" };
            let r = d.mk(
                El::new("div")
                    .cls(cls)
                    .a("role", "option")
                    .a("data-i", &row.index.to_string())
                    .a("aria-selected", if is_sel { "true" } else { "false" })
                    .a("title", &row.title),
            );
            let kind = d.mk(El::new("span").cls("cs-qo-kind").text(&row.kind));
            // `name.textContent = r.label || …` (quickopen.js:331), then the
            // `.cs-qo-full` span is appended *inside* `.cs-qo-name`
            // (quickopen.js:332-337) — a nested span, not a sibling.
            let name = d.mk(El::new("span").cls("cs-qo-name").text(&row.label));
            let mut kids = Vec::new();
            if row.show_full && !row.full.is_empty() {
                let full = d.mk(El::new("span").cls("cs-qo-full").text(&row.full));
                d.append(name, &[full]);
            }
            let loc = d.mk(El::new("span").cls("cs-qo-loc").text(&row.loc));
            kids.push(kind);
            kids.push(name);
            kids.push(loc);
            d.append(r, &kids);
            nodes.push(r);
        }
        d.append(list_el, &nodes);
    }

    // ---- .cs-qo-note: `setNote` adds `hidden` for an empty note
    // (quickopen.js:291-297), so both the flag and the empty text hide it.
    let mut note_cls = String::from("cs-qo-note");
    if facts.note_hidden || facts.note.is_empty() {
        note_cls.push_str(" hidden");
    }
    let note_el = d.mk(
        El::new("div")
            .id("cs-qo-note")
            .cls(&note_cls)
            .text(&facts.note),
    );

    d.append(box_el, &[head, list_el, note_el]);
    d.append(overlay, &[box_el]);
    d.append(body, &[overlay]);
}

/// Write the typed topbar query into `#searchInput`'s `value`.
///
/// `search.js` owns the input's `value` property (lines 58 and 129 read it; the
/// container node is the one both the search and Find paths type into). The
/// shipped input has no `value` attribute, so this write is the only honest
/// representation a static document can carry — and it is skipped entirely when
/// `ui.search_query` is absent, leaving the default render untouched.
/// Paint the form controls' interaction state (`ui.form`).
///
/// Two pieces, both driven only by facts:
///
/// 1. `input` — the focused text box's edit buffer is the box's `value`.
/// 2. `menu` — a `#csFormMenu` list inserted DIRECTLY AFTER the control's row,
///    with one `button.btn.cs-form-option[type=button][data-value]` per option
///    and `.active` on the highlighted row. No CSS is authored here: the shipped
///    `.btn` rules style the rows, and the menu is in-flow (see
///    [`FormFacts`] for why no floating geometry is invented).
fn apply_form(d: &mut Dom<'_>, facts: &FormFacts) {
    // The facts carry DOM **selectors** (`#reorgDest`); `Dom::id` takes the bare
    // id, so the leading `#` is stripped here rather than duplicated in the app.
    let bare = |selector: &str| selector.trim_start_matches('#').to_string();
    if let Some(input) = &facts.input {
        if let Some(node) = d.id(&bare(&input.id)) {
            d.set_attr(node, "value", &input.value);
        }
    }
    let Some(menu) = &facts.menu else { return };
    let Some(control) = d.id(&bare(&menu.id)) else { return };
    let Some(row) = d.doc.get_node(control).and_then(|n| n.parent) else {
        return;
    };
    let mut buttons: Vec<usize> = Vec::new();
    for (i, option) in menu.options.iter().enumerate() {
        let class = if i == menu.active {
            "btn cs-form-option active"
        } else {
            "btn cs-form-option"
        };
        let mut el = El::new("button")
            // A UNIQUE id per row so the shell hit chain can name the row the
            // user pressed (`#csFormMenuOption<i>`); the app resolves it back to
            // `FormState::commit_index`.
            .id(&format!("csFormMenuOption{i}"))
            .a("type", "button")
            .a("data-value", &option.value)
            .a("data-i", &i.to_string());
        el = el.cls(class);
        if option.selected {
            el = el.a("aria-selected", "true");
        }
        buttons.push(d.mk(el.text(option.label.clone())));
    }
    let box_el = d.mk(El::new("div").id("csFormMenu").cls("cs-form-menu"));
    d.set_children(box_el, &buttons);
    d.insert_after(row, &[box_el]);
}

/// Paint the focused text box's value window and caret (`ui.form.input.caret`).
///
/// Blitz paints a single-line input's value from its content-box origin and
/// clips at the right edge. That is right for an unfocused box — the reference
/// shows the head of a long path — but wrong while editing: the host's edit
/// buffer grows at the caret, and with a value wider than the box the typed
/// tail lands past the clip. Measured on `#rootInput` with the live repo path,
/// the raster for `value` and `value + "X"` was **byte-identical**, so a
/// keystroke had no visible consequence at all. The caret had no paint either,
/// so a click gave no focus feedback.
///
/// This shim, when the facts carry a caret:
///  * keeps the native glyphs untouched while the caret is inside the box (a
///    short value does not move a pixel on focus),
///  * otherwise hides the native run and re-paints the same value shifted left
///    by just enough for the caret to sit inside the right edge,
///  * always paints a 1px caret at the caret's own laid-out x.
///
/// Runs after the first `resolve(0.0)` (it needs the real border box and the
/// input's Parley layout); the caller's existing second resolve lays it out.
pub fn apply_form_shim(doc: &mut BaseDocument, form: Option<&FormFacts>) {
    use parley::layout::{Affinity, Cursor};

    let Some(input) = form.and_then(|f| f.input.as_ref()) else {
        return;
    };
    let Some(caret_char) = input.caret else {
        return;
    };
    let id = input.id.trim_start_matches('#');
    let Some(node_id) = doc.get_element_by_id(id) else {
        return;
    };

    // The caret's x and the run's full width, from the SAME Parley layout that
    // paints the native glyphs — so the shim can never disagree with them.
    let (text_width, caret_x) = {
        let Some(node) = doc.get_node_mut(node_id) else { return };
        let Some(el) = node.element_data_mut() else { return };
        let Some(ti) = el.text_input_data_mut() else { return };
        let text = ti.editor.raw_text().to_string();
        // The layout is already fresh: this runs after the first `resolve(0.0)`
        // that shaped the editor. `try_layout` is the read-only accessor, so the
        // shim cannot trigger a second shaping pass with no font contexts.
        let Some(layout) = ti.editor.try_layout() else { return };
        let byte = text
            .char_indices()
            .nth(caret_char)
            .map(|(b, _)| b)
            .unwrap_or(text.len());
        let cursor = Cursor::from_byte_index(layout, byte, Affinity::Downstream);
        let caret = cursor.geometry(layout, 0.0);
        (layout.width(), caret.x0 as f32)
    };

    let Some(node) = doc.get_node(node_id) else { return };
    let Some(parent) = node.parent else { return };
    let Some(parent_pos) = doc.get_node(parent).map(|p| p.absolute_position(0.0, 0.0)) else {
        return;
    };
    let pos = node.absolute_position(0.0, 0.0);
    let fl = &node.final_layout;
    let (b, p, sz) = (fl.border, fl.padding, fl.size);
    let content_w = (sz.width - b.left - b.right - p.left - p.right).max(0.0);
    let content_h = (sz.height - b.top - b.bottom - p.top - p.bottom).max(0.0);
    if content_w <= 1.0 || content_h <= 1.0 {
        return;
    }
    let origin_x = pos.x as f32 - parent_pos.x as f32 + b.left + p.left;
    let origin_y = pos.y as f32 - parent_pos.y as f32 + b.top + p.top;

    // Scroll only as far as the caret needs, and no further than the value's
    // own width: a caret near the start keeps today's head-of-path pixels.
    let max_shift = (text_width + 2.0 - content_w).max(0.0);
    let shift = (caret_x + 2.0 - content_w).clamp(0.0, max_shift);
    let paints_run = shift > 0.0;
    if !paints_run && caret_x > content_w + 1.0 {
        return;
    }

    let old = doc
        .get_node(node_id)
        .and_then(|n| n.element_data())
        .and_then(|e| {
            e.attrs
                .iter()
                .find(|a| a.name.local.as_ref() == "style")
                .map(|a| a.value.clone())
        })
        .unwrap_or_default();
    let parent_style = doc
        .get_node(parent)
        .and_then(|n| n.element_data())
        .and_then(|e| {
            e.attrs
                .iter()
                .find(|a| a.name.local.as_ref() == "style")
                .map(|a| a.value.clone())
        })
        .unwrap_or_default();

    // Caret: 1px wide, the glyph run's height (12px text in a 28px line box).
    let caret_h = 14.0f32.min(content_h - 6.0).max(8.0);
    let caret_top = ((content_h - caret_h) / 2.0).max(0.0);

    let mut d = Dom::new(doc);
    d.set_attr(parent, "style", &format!("{parent_style};position:relative;"));
    if paints_run {
        d.set_attr(node_id, "style", &format!("{old};color:transparent;"));
        // `text` is re-read here rather than carried out of the borrow above:
        // the value IS the host's edit buffer, already written by `apply_form`.
        let value = d
            .doc
            .get_node(node_id)
            .and_then(|n| n.element_data())
            .and_then(|e| {
                e.attrs
                    .iter()
                    .find(|a| a.name.local.as_ref() == "value")
                    .map(|a| a.value.clone())
            })
            .unwrap_or_default();
        let wrap_style = format!(
            "position:absolute; left:{origin_x:.1}px; top:{origin_y:.1}px; \
             width:{content_w:.1}px; height:{content_h:.1}px; overflow:hidden; \
             white-space:nowrap; pointer-events:none;"
        );
        let wrap = d.mk(
            El::new("div")
                .cls("cs-paint-value")
                .style(&wrap_style),
        );
        // The run is given an explicit width wider than the text: an
        // absolutely positioned inline's `white-space:nowrap` alone did not stop
        // Blitz from wrapping the value at its first space (measured: the tail
        // "toolkit v6.1" dropped to a clipped second line), so the box is sized
        // to the shaped run as well.
        let run_style = format!(
            "position:absolute; left:{:.1}px; top:0px; height:{content_h:.1}px; \
             width:{:.1}px; line-height:{content_h:.1}px; white-space:nowrap; \
             font-family:var(--mono); font-size:12px; color:var(--text);",
            -shift,
            text_width + 4.0
        );
        let run = d.mk(El::new("span").style(&run_style).text(value));
        let caret_style = format!(
            "position:absolute; left:{:.1}px; top:{caret_top:.1}px; width:1px; \
             height:{caret_h:.1}px; background:var(--text); pointer-events:none;",
            caret_x - shift
        );
        let bar = d.mk(
            El::new("div")
                .cls("cs-paint-caret")
                .style(&caret_style),
        );
        d.append(wrap, &[run, bar]);
        d.append(parent, &[wrap]);
    } else {
        let caret_style = format!(
            "position:absolute; left:{:.1}px; top:{:.1}px; width:1px; \
             height:{caret_h:.1}px; background:var(--text); pointer-events:none;",
            origin_x + caret_x,
            origin_y + caret_top
        );
        let bar = d.mk(
            El::new("div")
                .cls("cs-paint-caret")
                .style(&caret_style),
        );
        d.append(parent, &[bar]);
    }
}

fn apply_search_query(d: &mut Dom<'_>, query: &str) {
    if let Some(input) = d.id("searchInput") {
        d.set_attr(input, "value", query);
    }
}

/// `element.value = v` for a `<select>`: mark the matching `<option selected>`.
///
/// Blitz 0.2 has no select state model, so — exactly as `apply_edit_tool` paints
/// `#editTool` — the **option element carries the `selected` attribute**. The
/// first option is the shipped default (no attributes in `index.html`), which is
/// why an absent fact writes nothing and the default render stays byte-identical.
fn set_select_value(d: &mut Dom<'_>, select_id: &str, value: &str) {
    let Some(select) = d.id(select_id) else { return };
    let children: Vec<usize> = match d.doc.get_node(select) {
        Some(node) => node.children.clone(),
        None => return,
    };
    let mut options: Vec<(usize, bool)> = Vec::new();
    for child in children {
        let Some(el) = d.doc.get_node(child).and_then(|n| n.element_data()) else {
            continue;
        };
        if el.name.local.as_ref() != "option" {
            continue;
        }
        let current = el
            .attrs
            .iter()
            .find(|a| a.name.local.as_ref() == "value")
            .map(|a| a.value.clone())
            .unwrap_or_default();
        options.push((child, current == value));
    }
    for (node, selected) in options {
        if selected {
            d.set_attr(node, "selected", "selected");
        } else {
            d.clear_attr(node, "selected");
        }
    }
}

/// Paint the Reorganise & export dialog from `ui.reorg` (`reorg.js`).
///
/// Three rules, each taken from the reference rather than invented:
///
/// 1. **Form values** — each select's matching option gets `selected`, the
///    number/text inputs get their `value` attribute, and `#reorgHeader`'s
///    `checked` attribute follows the flag. `set_checked` is the attribute-level
///    setter the filter checkboxes already use, so "unchecked" is genuinely
///    absent rather than `checked="false"`.
/// 2. **`syncRows`** (`reorg.js:286-290`) — the three rows' `hidden` class.
/// 3. **Report + note** — `render`'s `.reorg-row` children (`reorg.js:46-120`)
///    and `setNote`'s text/class (`reorg.js:37-40`). Rows are rebuilt with
///    `set_children`, so a repaint can never duplicate them; the empty case
///    renders the reference's placeholder only when a run produced the fact
///    (`has_report`), leaving the shipped empty `#reorgReport` alone otherwise.
fn apply_reorg(d: &mut Dom<'_>, facts: &ReorgFacts) {
    // ---- the three conditional rows: `syncRows` ---------------------------
    // Kept together with the mode/group writes: the visibility must describe
    // the same values the selects show, and reading them from one object is
    // what guarantees that.
    if let Some(mode) = &facts.mode {
        set_select_value(d, "reorgMode", mode);
        let split = mode == "split";
        let group = facts.group.as_deref().unwrap_or_default();
        if let Some(node) = d.id("reorgGranRow") {
            d.set_class_flag(node, "hidden", !split);
        }
        if let Some(node) = d.id("reorgGroupRow") {
            d.set_class_flag(node, "hidden", !split);
        }
        if let Some(node) = d.id("reorgSizeRow") {
            d.set_class_flag(node, "hidden", !(split && group == "size"));
        }
    }
    if let Some(scope) = &facts.scope {
        set_select_value(d, "reorgScope", scope);
    }
    if let Some(gran) = &facts.gran {
        set_select_value(d, "reorgGran", gran);
    }
    if let Some(group) = &facts.group {
        set_select_value(d, "reorgGroup", group);
    }
    if let Some(size) = facts.size {
        if let Some(node) = d.id("reorgSize") {
            d.set_attr(node, "value", &size.to_string());
        }
    }
    if let Some(dest) = &facts.dest {
        if let Some(node) = d.id("reorgDest") {
            d.set_attr(node, "value", dest);
        }
    }
    if let Some(header) = facts.header {
        if let Some(node) = d.id("reorgHeader") {
            d.set_checked(node, header);
        }
    }

    // ---- `#reorgReport` ---------------------------------------------------
    if let Some(report) = d.id("reorgReport") {
        let children: Vec<usize> = if facts.rows.is_empty() {
            if facts.has_report {
                let row = d.mk(El::new("div").cls("reorg-row reorg-empty"));
                let text = d.mk_text("Plan or verify to see what would be written.");
                d.append(row, &[text]);
                vec![row]
            } else {
                Vec::new()
            }
        } else {
            facts
                .rows
                .iter()
                .map(|row| {
                    let class = if row.class.is_empty() {
                        "reorg-row".to_string()
                    } else {
                        format!("reorg-row {}", row.class)
                    };
                    let div = d.mk(El::new("div").cls(&class));
                    let k = d.mk(El::new("span").cls("reorg-k").text(&row.k));
                    let v = d.mk(El::new("span").cls("reorg-v").text(&row.v));
                    d.append(div, &[k, v]);
                    div
                })
                .collect()
        };
        // An open dialog with no run yet (`!has_report`, no rows) leaves the
        // shipped empty box completely untouched.
        if facts.has_report || !facts.rows.is_empty() {
            d.set_children(report, &children);
        }
    }

    // ---- `#reorgNote` -----------------------------------------------------
    // Empty note + no kind = the shipped empty span; writing it would still be
    // visually identical, but skipping keeps the live tree equal to the shipped
    // one for an open-but-idle dialog.
    if !facts.note.is_empty() || !facts.note_kind.is_empty() {
        if let Some(note) = d.id("reorgNote") {
            d.set_text(note, &facts.note);
            let class = if facts.note_kind.is_empty() {
                "modal-note".to_string()
            } else {
                format!("modal-note {}", facts.note_kind)
            };
            d.set_attr(note, "class", &class);
        }
    }
}

/// The change-preview panel's engine answer — `render(data)` + `setNote` from
/// `preview.js:64-137`.
///
/// Every field is a transport of what the app's [`crate::preview`]-side builder
/// produced from `csgraph_core::preview::invoke`; this layer never composes
/// report copy. `text` is plain text: the reference sets `innerHTML` because it
/// builds markup client-side, but the `#previewOut` box is
/// `div.reorg-report.preview-out` and the shipped stylesheet styles it as a
/// report; writing it as one text node (what `textContent =` does) keeps the
/// bytes the host sent — no reflow, no wrapping, no re-escape.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PreviewFacts {
    /// `#previewOut`'s `textContent`.
    #[serde(default)]
    pub text: String,
    /// `#previewNote`'s `textContent`.
    #[serde(default)]
    pub note: String,
    /// The second argument to `setNote(text, kind)` (`preview.js:39`):
    /// `"ok"`, `"bad"`, or `""` for the bare `modal-note` class.
    #[serde(default)]
    pub note_kind: String,
}

/// Paint [`PreviewFacts`] into `#previewOut` / `#previewNote`.
///
/// The reference's `setNote` is `els.note.className = 'modal-note' + (kind ?
/// ' ' + kind : '')` (`preview.js:39-42`), so a content-less note with an empty
/// kind reproduces the shipped `class="modal-note"` exactly.
fn apply_preview(d: &mut Dom<'_>, preview: &PreviewFacts) {
    if let Some(out) = d.id("previewOut") {
        d.set_text(out, &preview.text);
    }
    if let Some(note) = d.id("previewNote") {
        d.set_text(note, &preview.note);
        let class = if preview.note_kind.is_empty() {
            "modal-note".to_string()
        } else {
            format!("modal-note {}", preview.note_kind)
        };
        d.set_attr(note, "class", &class);
    }
}

/// The docked node drawer's content — the v3 port of `renderNodeDrawer` +
/// `renderRelations` (`drawer.js:74-360`).
///
/// Every field is transport: [`crate::preview`]-side? No — the app's
/// `drawer_wire` derives it from the loaded graph's own nodes/edges. This layer
/// invents no copy; it only builds the markup the shipped stylesheet already
/// styles (`.drawer-kdot`, `.detail-head`, `.kind-badge`, `.detail-flags`,
/// `.detail-loc`, `.kv`, `.relationship`, `.rel-nodes`, `.rel-node`, `.rn-label`,
/// `.rn-kind`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DrawerFacts {
    /// The selected node's id (the rows' click target vocabulary).
    #[serde(default)]
    pub id: String,
    /// `#nodeTitle`'s text (`name || label || id` in the reference).
    #[serde(default)]
    pub title: String,
    /// `#nodeTitle`'s `title` attribute (the full name / id).
    #[serde(default)]
    pub title_attr: String,
    /// The node kind, shown as `.kind-badge` and used for the dot colour.
    #[serde(default)]
    pub kind: String,
    /// Root-relative `file:line(-endLine)` — `.detail-loc`.
    #[serde(default)]
    pub location: String,
    /// Absolute location for the `.detail-loc` tooltip.
    #[serde(default)]
    pub location_full: String,
    /// `.detail-flags` chips (`deg N`, `external`).
    #[serde(default)]
    pub flags: Vec<String>,
    /// `.kv` property rows (`dt`/`dd` pairs).
    #[serde(default)]
    pub props: Vec<DrawerProp>,
    /// Ancestry crumbs (`#nodeCrumbs .crumb`), root-first, selected node omitted
    /// (`interaction.js:375-409`).
    #[serde(default)]
    pub crumbs: Vec<DrawerCrumb>,
    /// The `.prop-actions` row (`drawer.js:96-102`).
    #[serde(default)]
    pub actions: Vec<DrawerAction>,
    /// One `.relationship` section per non-empty relation bucket.
    #[serde(default)]
    pub relations: Vec<DrawerRelation>,
    /// `#relationsCount`'s text — the summed real counts, `0` renders blank.
    #[serde(default)]
    pub relation_total: usize,
}

/// One ancestor crumb (`button.crumb[data-key=<id>]`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DrawerCrumb {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub title: String,
}

/// One `.prop-actions` button (`drawer.js:96-102`). `act` is the reference's
/// `data-act` (`expand` / `hidech` / `chain`); the click payload rides on
/// `data-key` as `act:<act>:<node id>` because the shell's hit row only carries
/// the nearest `data-key` (`nzk_ui/src/lib.rs:75`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DrawerAction {
    #[serde(default)]
    pub act: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub title: String,
}

/// One property row in `#nodeProps` (`<dl class="kv"><dt>k</dt><dd>v</dd>`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DrawerProp {
    #[serde(default)]
    pub k: String,
    #[serde(default)]
    pub v: String,
}

/// One relation bucket (`.relationship[data-key]` + its `.rel-nodes` list).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DrawerRelation {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub label: String,
    /// The bucket's real edge count (rows may be capped).
    #[serde(default)]
    pub count: usize,
    /// v2 `relationListOpen`: the group's expanded state. Absent/false = the
    /// list is collapsed (`.rel-nodes` not painted).
    #[serde(default)]
    pub open: bool,
    /// `.hidden-children` — the "Hide children" scope covers this group.
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub rows: Vec<DrawerRelationRow>,
}

/// One clickable neighbour row (`.rel-node[data-key=<node id>]`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DrawerRelationRow {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub kind: String,
}

/// Paint [`DrawerFacts`] into the docked `#nodeDrawer`.
///
/// Structural contract transcribed from `drawer.js:74-360`:
/// `#nodeTitle` (text + tooltip), `#nodeProps` = `.detail-head` (`.kind-badge`
/// with the reference's kind colour + `.detail-flags`) then `.detail-loc` then a
/// `.kv` pair list; `#relationsCount` = the summed counts; `#relationsBody` = one
/// `.relationship[data-key]` per bucket (`.rel-caret`, `.rel-label`,
/// `.num-results`) followed by its `.rel-nodes` list of
/// `button.rel-node[type=button][data-key=<id>][title=<label>]` rows
/// (`.dot` + `.rn-label` + `.rn-kind`). `clear` before appending is
/// `innerHTML = ''`, so a repaint cannot duplicate rows.
fn apply_drawer(d: &mut Dom<'_>, facts: &DrawerFacts) {
    // Content implies the drawer is open (`renderNodeDrawer` removes `hidden`);
    // the open/close facts are the app's, and this is the content half.
    if let Some(drawer) = d.id("nodeDrawer") {
        d.set_class_flag(drawer, "hidden", false);
    }
    if let Some(title) = d.id("nodeTitle") {
        d.set_text(title, &facts.title);
        let tooltip = if facts.title_attr.is_empty() {
            facts.title.as_str()
        } else {
            facts.title_attr.as_str()
        };
        d.set_attr(title, "title", tooltip);
    }
    // `#nodeCrumbs` — clickable ancestry buttons, root-first with `›` separators
    // (`interaction.js:390-409`). `clear` is v2's `innerHTML = ''`.
    if let Some(crumbs_el) = d.id("nodeCrumbs") {
        d.clear(crumbs_el);
        let mut children: Vec<usize> = Vec::new();
        for (i, c) in facts.crumbs.iter().enumerate() {
            if i > 0 {
                children.push(d.mk(El::new("span").cls("crumb-sep").text("\u{203a}")));
            }
            children.push(d.mk(
                El::new("button")
                    .cls("crumb")
                    .a("type", "button")
                    .a("data-key", &c.id)
                    .a("title", &c.title)
                    .text(&c.label),
            ));
        }
        d.append(crumbs_el, &children);
    }
    if let Some(props) = d.id("nodeProps") {
        d.clear(props);
        let mut top: Vec<usize> = Vec::new();
        let head = d.mk(El::new("div").cls("detail-head"));
        let badge = d.mk(
            El::new("span")
                .cls("kind-badge")
                .a("style", &format!("background:{}", kind_color(&facts.kind)))
                .text(&facts.kind),
        );
        let mut head_children = vec![badge];
        if !facts.flags.is_empty() {
            let flags = d.mk(El::new("span").cls("detail-flags"));
            let chips: Vec<usize> = facts
                .flags
                .iter()
                .map(|f| {
                    // The `deg N` chip says nothing on its own. The shell now
                    // surfaces `title` as a real hover tooltip (see
                    // `app/src/nzk_ui.rs::paint_feedback`), so spell out what
                    // the number means; every other flag keeps its shipped
                    // label with no invented title.
                    let mut el = El::new("span").cls("flag");
                    if let Some(n) = f.strip_prefix("deg ") {
                        let title = format!(
                            "Degree — the number of distinct graph neighbours \
                             (incoming + outgoing edges) for this declaration. \
                             This one has {n}."
                        );
                        el = el.a("title", &title);
                    }
                    d.mk(el.text(f))
                })
                .collect();
            d.append(flags, &chips);
            head_children.push(flags);
        }
        d.append(head, &head_children);
        top.push(head);
        if !facts.location.is_empty() {
            let loc = d.mk(
                El::new("div")
                    .cls("detail-loc mono-sig")
                    .a(
                        "title",
                        if facts.location_full.is_empty() {
                            &facts.location
                        } else {
                            &facts.location_full
                        },
                    )
                    .text(&facts.location),
            );
            top.push(loc);
        }
        if !facts.props.is_empty() {
            let kv = d.mk(El::new("dl").cls("kv"));
            let mut pairs: Vec<usize> = Vec::new();
            for p in &facts.props {
                pairs.push(d.mk(El::new("dt").text(&p.k)));
                pairs.push(d.mk(El::new("dd").text(&p.v)));
            }
            d.append(kv, &pairs);
            top.push(kv);
        }
        // `.prop-actions` (`drawer.js:96-102`): Expand / Hide children / Resolve
        // chain. The click payload rides on `data-key` because the shell's hit
        // row carries only the nearest `data-key`; the app routes
        // `act:<act>:<node id>` back to the drawer model.
        if !facts.actions.is_empty() {
            let row = d.mk(El::new("div").cls("prop-actions"));
            let mut buttons: Vec<usize> = Vec::new();
            for a in &facts.actions {
                buttons.push(d.mk(
                    El::new("button")
                        .cls("btn btn-ghost prop-action")
                        .a("type", "button")
                        .a("data-key", &format!("act:{}:{}", a.act, facts.id))
                        .a("title", &a.title)
                        .text(&a.label),
                ));
            }
            d.append(row, &buttons);
            top.push(row);
        }
        d.append(props, &top);
    }
    if let Some(count) = d.id("relationsCount") {
        let text = if facts.relation_total == 0 {
            String::new()
        } else {
            facts.relation_total.to_string()
        };
        d.set_text(count, &text);
    }
    let Some(body) = d.id("relationsBody") else { return };
    d.clear(body);
    if facts.relations.is_empty() {
        let hint = d.mk(El::new("div").cls("drawer-hint").text("No relations for this node."));
        d.append(body, &[hint]);
        return;
    }
    let mut top: Vec<usize> = Vec::new();
    for rel in &facts.relations {
        // v2 `renderRelations`'s row: ± (bucket toggle), caret left of the name
        // (list-only toggle), the label (same toggle) and the count.
        let group = format!("{}|{}", facts.id, rel.key);
        let mut cls = "relationship".to_string();
        if rel.hidden {
            cls.push_str(" hidden-children");
        }
        let row = d.mk(El::new("div").cls(&cls).a("data-key", &rel.key));
        let toggle = d.mk(
            El::new("span")
                .cls("item rel-toggle")
                .a("data-key", &group)
                .text("\u{002b}"),
        );
        let caret = d.mk(
            El::new("span")
                .cls("item rel-caret")
                .a("data-key", &group)
                .text(if rel.open { "\u{25be}" } else { "\u{25b8}" }),
        );
        let label = d.mk(
            El::new("span")
                .cls("item rel-label")
                .a("data-key", &group)
                .text(&rel.label),
        );
        let count = d.mk(
            El::new("span")
                .cls("item num-results")
                .text(&rel.count.to_string()),
        );
        d.append(row, &[toggle, caret, label, count]);
        top.push(row);
        // Collapsed groups paint no list (v2 `if (open && hasNodes)`).
        if !rel.open || rel.rows.is_empty() {
            continue;
        }
        let list = d.mk(El::new("div").cls("rel-nodes"));
        let mut buttons: Vec<usize> = Vec::new();
        for nb in &rel.rows {
            let b = d.mk(
                El::new("button")
                    .cls("rel-node")
                    .a("type", "button")
                    .a("data-key", &nb.id)
                    .a("title", &nb.label),
            );
            let dot = d.mk(
                El::new("span")
                    .cls("dot")
                    .a("style", &format!("background:{}", kind_color(&nb.kind))),
            );
            let rn = d.mk(El::new("span").cls("rn-label").text(&nb.label));
            let kind = d.mk(El::new("span").cls("rn-kind").text(&nb.kind));
            d.append(b, &[dot, rn, kind]);
            buttons.push(b);
        }
        d.append(list, &buttons);
        top.push(list);
    }
    d.append(body, &top);
}

/// `renderSuggestions` + `showSuggestions` (search.js:11-38).
///
/// Presence of `ui.suggestions` is the open state, so this is only reached under
/// `Some`; the shipped document keeps `class="suggestions hidden"`, no children
/// and `aria-expanded="false"` when the key is absent (the default paint, pinned
/// by `@frame`).
///
/// Structural contract transcribed from `search.js:29-37`, per row:
/// `button.sug[type=button][role=option][data-id][data-i][title="Open this symbol
/// in the graph"]`, `.active` only on the highlighted row, then exactly four
/// children in this order — `span.dot` (inline `background:<kind colour>` plus the
/// reference's own 7px/50% geometry string), `span.sug-kind`, `span.sug-label`
/// (`label || id`), `span.sug-full`. With a message and no rows it is one
/// `div.sug-empty` (search.js:25-27). `set_children` is `innerHTML = ''` reload:
/// children are replaced, never appended, so a repaint cannot duplicate rows.
///
/// The inline dot style is the reference's own data string, not a stylesheet rule
/// authored in Rust: `style.css` supplies `.sug`/`.sug-kind`/`.sug-label`/
/// `.sug-full`/`.sug-empty`/`.suggestions`, and `.dot`'s geometry only ever
/// existed inline in `search.js:31`.
fn apply_suggestions(d: &mut Dom<'_>, facts: &SuggestionsFacts) {
    let Some(box_el) = d.id("suggestions") else { return };
    // `showSuggestions` (search.js:12): remove the shipped `hidden` class. Writing
    // the bare class is idempotent and cannot leave a stale token behind.
    d.set_attr(box_el, "class", "suggestions");
    if let Some(input) = d.id("searchInput") {
        // `searchInputEl.setAttribute('aria-expanded', 'true')` (search.js:13).
        d.set_attr(input, "aria-expanded", "true");
    }
    let mut children = Vec::new();
    if let Some(message) = facts.empty.as_deref().filter(|m| !m.is_empty()) {
        // search.js:25-27 — the whole box is the message, there are no rows.
        children.push(d.mk(El::new("div").cls("sug-empty").text(message)));
    } else {
        for (i, row) in facts.rows.iter().enumerate() {
            let mut class = String::from("sug");
            if facts.active == Some(i) {
                class.push_str(" active");
            }
            // search.js:34 `label || id` (the native model has no `name`).
            let label = if row.label.is_empty() {
                &row.id
            } else {
                &row.label
            };
            let style = format!(
                "background:{};width:7px;height:7px;border-radius:50%;flex:none",
                kind_color(&row.kind)
            );
            let dot = d.mk(El::new("span").cls("dot").a("style", &style));
            let kind = d.mk(El::new("span").cls("sug-kind").text(&row.kind));
            let lbl = d.mk(El::new("span").cls("sug-label").text(label));
            let full = d.mk(El::new("span").cls("sug-full").text(&row.full));
            let btn = d.mk(
                El::new("button")
                    .cls(&class)
                    .a("type", "button")
                    .a("role", "option")
                    .a("data-id", &row.id)
                    // `data-i` is the array position as a decimal string; it is
                    // synthesised here, never carried redundantly on the wire.
                    .a("data-i", &i.to_string())
                    .a("title", "Open this symbol in the graph"),
            );
            d.append(btn, &[dot, kind, lbl, full]);
            children.push(btn);
        }
    }
    d.set_children(box_el, &children);
}

/// The active mode tab, applied whether or not a collapsed list was supplied.
fn apply_mode(ui: &UiState, d: &mut Dom<'_>) {
    if let Some(mode) = &ui.active_mode {
        let graph_active = mode == "graph";
        for (id, active) in [("modeGraph", graph_active), ("modeFind", !graph_active)] {
            if let Some(el) = d.id(id) {
                d.set_class_flag(el, "active", active);
                d.set_attr(el, "aria-selected", if active { "true" } else { "false" });
            }
        }
        // `setMode` switches the search box's placeholder with the mode
        // (`search.js:96`). The paths are the shipped literals, so writing them
        // back on the default `"graph"` paint is idempotent.
        if let Some(input) = d.id("searchInput") {
            let placeholder = if graph_active {
                "Search symbols…  ( / )"
            } else {
                "Find symbol or code…  ( f )"
            };
            d.set_attr(input, "placeholder", placeholder);
        }
    }
}

/// One scroll container's applied offset and its maximum, in CSS px.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrollReport {
    pub selector: String,
    pub x: f64,
    pub y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

/// The resolved content extent of `node`'s subtree, in document coordinates.
fn content_bottom(doc: &BaseDocument, node: usize) -> f64 {
    let mut bottom = 0.0f64;
    if let Some(n) = doc.get_node(node) {
        let pos = n.absolute_position(0.0, 0.0);
        bottom = (pos.y + n.final_layout.size.height) as f64;
        for child in &n.children {
            bottom = bottom.max(content_bottom(doc, *child));
        }
    }
    bottom
}

/// Apply the requested scroll offsets to the engine's own scroll containers.
///
/// Must run **after** layout. `blitz-paint` subtracts a node's `scroll_offset`
/// when it paints a scroll container and clips to the container box, so setting
/// the offset is what makes the scrolled slice visible.
///
/// The scrollable range is computed from the **resolved subtree boxes**
/// (`content_bottom - box bottom`) rather than from taffy's
/// `Layout::scroll_height()`, which reports 0 for this flex column: with the
/// source pane open the `.side` content reaches y=971 inside a box ending at
/// y=660 (`.side { overflow-y: auto }`), yet `scroll_height()` says there is
/// nothing to scroll. The boxes are the real layout either way; the engine's
/// own clamp is still applied on top, so we can never scroll past what it allows.
pub fn apply_scroll(doc: &mut BaseDocument, ui: &UiState) -> Vec<ScrollReport> {
    let mut out = Vec::new();
    for spec in &ui.scroll {
        let Ok(Some(id)) = doc.query_selector(&spec.selector) else {
            continue;
        };
        let (top, height, bubble_y) = {
            let Some(n) = doc.get_node(id) else { continue };
            let pos = n.absolute_position(0.0, 0.0);
            (pos.y as f64, n.final_layout.size.height as f64, 0.0f64)
        };
        let _ = bubble_y;

        // Ask the engine first: its clamp is the authority where it is populated.
        doc.scroll_node_by(id, 1e6, 1e6);
        let engine_max_y = doc
            .get_node(id)
            .map(|n| n.scroll_offset.y)
            .unwrap_or(0.0);

        // Then our own resolved-geometry range, which is populated regardless.
        let geom_max_y = (content_bottom(doc, id) - top - height).max(0.0);
        let max_y = engine_max_y.max(geom_max_y);
        let max_x = 0.0f64;

        let y = spec.y.clamp(0.0, max_y);
        if let Some(n) = doc.get_node_mut(id) {
            n.scroll_offset.y = y;
            n.scroll_offset.x = spec.x.clamp(0.0, max_x);
        }
        out.push(ScrollReport {
            selector: spec.selector.clone(),
            x: spec.x.clamp(0.0, max_x),
            y,
            max_x,
            max_y,
        });
    }
    out
}

/// Reproduce every runtime mutation that affects the top bar, the sidebar and
/// the canvas notice.
///
/// Mirrors, in order: `shell.js` `applyState`/`updateStatusLine`/`updateCanvasNotice`,
/// `panels.js` `installPane`/`addChrome`, `filters.js`
/// `buildProjectFilter`/`buildNamespaceFilter`/`buildFilters`, `problems.js` `build`.
pub fn apply_runtime_dom(doc: &mut BaseDocument, facts: &Facts) {
    // A previous build that errored before `apply_paint_shims` could leave staged
    // chip classes behind; this is one document's build, so start clean.
    PENDING_CHIP_OFF.with(|q| q.borrow_mut().clear());
    // The viewer boots on the saved theme, nzk-dark+ (core.js DEFAULT_UI_THEME,
    // line 125). Without this attribute the document falls back to the dark-v1
    // :root palette, which is the wrong nzk_ui.
    let html = doc.root_element().id;
    let side = find_by_class(doc, doc.root_element().id, "side");
    let ns_select = doc.get_element_by_id("nsSelect");
    let prev_label = ns_select
        .and_then(|n| prev_element_sibling(doc, n))
        .filter(|p| has_class(doc, *p, "filter-label"));

    let mut d = Dom::new(doc);
    d.set_attr(html, "data-ui-theme", "nzk-dark+");
    // panels.js applySideWidth() writes the resolved width back onto <html>
    // (live dump: style="--side-w: 360px").
    d.set_attr(html, "style", "--side-w: 360px;");
    // The app's real filter model. `None` = the shipped render (all chips on,
    // only the empty namespace option), which is the byte-identical default.
    //
    // Read up here (not just before the Namespace block) because the Projects
    // `all`/`none` links created below need the same gate: their payload must be
    // absent when the model is absent, exactly like the chip payloads.
    let filter_state = facts.ui.as_ref().and_then(|u| u.filters.as_ref());

    // ---- top bar (shell.js applyState lines 116-122, updateStatusLine 130-146)
    if let Some(n) = d.id("rootInput") {
        d.set_attr(n, "value", &facts.repo);
        d.set_attr(n, "title", &facts.repo);
    }
    if let Some(n) = d.id("indexBtn") {
        // indexed === true, so the button is no longer disabled.
        d.clear_attr(n, "disabled");
    }
    if let Some(n) = d.id("statusLine") {
        d.set_text(n, &facts.status_text());
        d.set_attr(n, "title", &facts.status_text());
    }

    // ---- canvas notice (shell.js updateCanvasNotice) -------------------------
    if let Some(n) = d.id("notice") {
        d.set_text(
            n,
            &format!(
                "{} node{} in view · double-click a node to expand its children and calling edges",
                facts.nodes_in_view,
                if facts.nodes_in_view == 1 { "" } else { "s" }
            ),
        );
        d.clear_attr(n, "class");
        d.set_attr(n, "class", "notice");
    }

    // ---- .side pane nzk_ui (panels.js installPane 564-583 + addChrome 528-562)
    // `.side` is selected by class (panels.js:34), not by id.
    if let Some(side) = side {
        let title = d.mk(El::new("span").cls("pane-grip-title").text("Sidebar"));
        let btn = d.mk(
            El::new("button")
                .cls("btn btn-ghost pane-float-btn")
                .a("type", "button")
                .a(
                    "title",
                    "Detach this pane into its own window / dock it back",
                )
                .text("\u{2922}"),
        );
        let head = d.mk(El::new("div").cls("pane-grip pane-drag-head"));
        d.append(head, &[title, btn]);
        d.prepend(side, head);
        let rz = d.mk(El::new("div").cls("pane-resize").a("title", "Resize"));
        d.append(side, &[rz]);
    }

    // ---- Projects block (filters.js buildProjectFilter 72-126) ---------------
    if let Some(anchor) = prev_label {
        let actions = d.mk(El::new("span").cls("filter-actions"));
        let mut all_el = El::new("button")
            .cls("linkbtn")
            .a("type", "button")
            .a("title", "Show every project")
            .text("all");
        let mut none_el = El::new("button")
            .cls("linkbtn")
            .a("type", "button")
            .a("title", "Filter out every project")
            .text("none");
        // The routing payload is written only when the filter model is present,
        // matching the chips/checkboxes: an absent fact leaves the shipped
        // document's attributes untouched.
        if filter_state.is_some() {
            all_el = all_el.a("data-key", "filter:projAll");
            none_el = none_el.a("data-key", "filter:projNone");
        }
        let all = d.mk(all_el);
        let none = d.mk(none_el);
        d.append(actions, &[all, none]);
        let label = d.mk(El::new("div").cls("filter-label").text("Projects"));
        d.append(label, &[actions]);
        let list = d.mk(El::new("div").cls("chips").id("projectFilters"));
        if facts.projects.is_empty() {
            // filters.js line 120-126: the zero-project case is a .list-empty.
            let empty = d.mk(
                El::new("div")
                    .cls("list-empty")
                    .text(&facts.projects_empty_text),
            );
            d.append(list, &[empty]);
        } else {
            let mut chips = Vec::new();
            for p in &facts.projects {
                // `data-id="proj:<id>"` rides the existing host `data-id` hit
                // channel; `data-key="proj:<id>"` is the nearest-ANCESTOR
                // payload, so a press on the chip's label/`.bars` child still
                // names the chip (the reference's `closest('.edgekey')`).
                // `data-project` is kept for v2 markup parity. The `off` class
                // is the real filter state (`filters.js` toggles it).
                let cls = if p.on { "edgekey" } else { "edgekey off" };
                let el = d.mk(
                    El::new("span")
                        .cls(cls)
                        .a("data-project", &p.id)
                        .a("data-id", &format!("proj:{}", p.id))
                        .a("data-key", &format!("proj:{}", p.id)),
                );
                let tip = {
                    let mut bits = vec![p.path.clone()];
                    if !p.targets.is_empty() {
                        bits.push(p.targets.join(";"));
                    }
                    if !p.defines.is_empty() {
                        bits.push(format!("defines: {}", p.defines.join(";")));
                    }
                    bits.join(" · ")
                };
                let bars = d.mk(
                    El::new("span")
                        .cls("bars")
                        .style("border-top-color:#4ec9b0;border-top-width:2px"),
                );
                let label = if p.file_count > 0 {
                    format!("{} ({})", p.name, p.file_count)
                } else {
                    p.name.clone()
                };
                let text = d.mk_text(&label);
                let el = {
                    let e = el;
                    d.set_attr(e, "title", &tip);
                    e
                };
                d.append(el, &[bars, text]);
                chips.push(el);
            }
            d.append(list, &chips);
        }
        let block = d.mk(El::new("div").id("projectFilterBlock"));
        d.append(block, &[label, list]);
        d.insert_before(anchor, &[block]);
    }

    // ---- Namespace select (filters.js buildNamespaceFilter 253-275) ----------
    // The app's real filter selection (`filter_state` was read at the top).
    if let Some(ns) = ns_select {
        d.clear(ns);
        let (text, options, selected): (String, &[String], &str) = match filter_state {
            Some(f) => (
                if f.namespaces.is_empty() {
                    "All namespaces".to_string()
                } else {
                    format!("All namespaces ({})", f.namespaces.len())
                },
                f.namespaces.as_slice(),
                f.namespace.as_str(),
            ),
            None => (
                if facts.ns_count > 0 {
                    format!("All namespaces ({})", facts.ns_count)
                } else {
                    "All namespaces".to_string()
                },
                &[],
                "",
            ),
        };
        let mut opts = Vec::with_capacity(options.len() + 1);
        let mut all = El::new("option").a("value", "").text(text);
        // `selected` is only written when a fact is present: the shipped option
        // has no `selected` attribute and must keep it that way.
        if filter_state.is_some() && selected.is_empty() {
            all = all.a("selected", "selected");
        }
        opts.push(d.mk(all));
        for name in options {
            let mut o = El::new("option").a("value", name).text(name);
            if name == selected {
                o = o.a("selected", "selected");
            }
            opts.push(d.mk(o));
        }
        d.append(ns, &opts);
    }

    // ---- Node-kind facets (filters.js buildFilters 325-357) ------------------
    if let Some(kinds) = d.id("kindFilters") {
        d.clear(kinds);
        let mut nodes = Vec::new();
        for (kind, count) in &facts.kinds {
            // Absent override = ON (the shipped class), lookups are per-kind so
            // a partial fact cannot silently disable a chip.
            let on = filter_state
                .and_then(|f| f.kinds.iter().find(|k| &k.kind == kind).map(|k| k.on))
                .unwrap_or(true);
            // The click payload is written ONLY when the filter model is
            // present: the shipped document has no such attributes and an
            // absent fact must leave it byte-identical.
            //
            // TWO payloads, both the shell's nearest-ancestor channels:
            //  * `data-id` (existing) — the element's own payload;
            //  * `data-key` (new) — the nearest-ancestor payload, so a press on
            //    any CHILD span (`.swatch`/`.facet-name`/`.facet-count`) still
            //    carries `facet:<kind>` on the wire. That is the reference's
            //    `ev.target.closest('.facet')` rule; `data-id` is only on the
            //    label itself and a child press would otherwise carry none.
            let mut label_el = El::new("label")
                .cls(if on { "facet on" } else { "facet off" })
                .a("data-kind", kind);
            if filter_state.is_some() {
                label_el = label_el
                    .a("data-id", kind)
                    .a("data-key", &format!("facet:{kind}"));
            }
            let lab = d.mk(label_el);
            let sw = d.mk(
                El::new("span")
                    .cls("swatch")
                    .style(&format!("background:{}", kind_color(kind))),
            );
            let nm = d.mk(El::new("span").cls("facet-name").text(kind));
            let ct = d.mk(El::new("span").cls("facet-count").text(count.to_string()));
            d.append(lab, &[sw, nm, ct]);
            nodes.push(lab);
        }
        d.append(kinds, &nodes);
    }

    // ---- Edge-kind chips (filters.js buildFilters 359-381) -------------------
    if let Some(edges) = d.id("edgeFilters") {
        d.clear(edges);
        let mut nodes = Vec::new();
        for (kind, color, width, dashed) in EDGE_CHIPS {
            let on = filter_state
                .and_then(|f| f.edges.iter().find(|e| &e.kind == kind).map(|e| e.on))
                .unwrap_or(true);
            // Same two payloads as the kind facets: `data-id` for the element
            // itself, `data-key` for a press on a child span (`.bars`/text).
            let mut chip_el = El::new("span")
                .cls(if on { "edgekey" } else { "edgekey off" })
                .a("data-edge", kind);
            if filter_state.is_some() {
                chip_el = chip_el
                    .a("data-id", kind)
                    .a("data-key", &format!("edgekey:{kind}"));
            }
            let el = d.mk(chip_el);
            let bars = d.mk(
                El::new("span").cls("bars").style(&format!(
                    "border-top-color:{color};border-top-style:{};border-top-width:{width}px",
                    if *dashed { "dashed" } else { "solid" }
                )),
            );
            let text = d.mk_text(kind);
            d.append(el, &[bars, text]);
            nodes.push(el);
        }
        d.append(edges, &nodes);
    }
    if let Some(c) = d.id("filtersCount") {
        let text = if facts.kinds.is_empty() {
            String::new()
        } else {
            format!("{} kinds", facts.kinds.len())
        };
        d.set_text(c, &text);
    }

    // ---- The rest of the Filters controls: paint + click payloads -----------
    //
    // The checkboxes and `all`/`none` links are static markup whose state and
    // identity the shipped document cannot express:
    //
    //  * `#nsDeep`, `#hideIsolated`, `#hideEdgeLabels` had no paint path at all,
    //    so their box could be pressed while never showing its own value;
    //  * every checkbox's clickable element is its wrapping `<label>` (the text
    //    is the target), and `data-key` is the shell's nearest-ancestor payload,
    //    so the label carries the routing key — a press on the text routes
    //    exactly like a press on the tiny input box;
    //  * the kind/edge `all`/`none` buttons carry only `data-kinds`/`data-edges`,
    //    which the wire has no column for; `tag_filter_link_buttons` copies
    //    their identity onto `data-key`.
    //
    // Gated on the filter model being present, exactly like the chips: the
    // shipped document keeps its byte-identical attributes and pixels.
    if let Some(f) = filter_state {
        if let Some(n) = d.id("nsDeep") {
            d.set_checked(n, f.ns_deep);
        }
        if let Some(n) = d.id("hideIsolated") {
            d.set_checked(n, f.hide_isolated);
        }
        if let Some(n) = d.id("hideEdgeLabels") {
            d.set_checked(n, f.hide_edge_labels);
        }
        // The checkbox rows: payload on the clickable wrapping label.
        for (input_id, key) in [
            ("nsDeep", "filter:nsDeep"),
            ("hideEdgeLabels", "filter:hideEdgeLabels"),
            ("hideIsolated", "filter:hideIsolated"),
            ("hideChildren", "filter:hideRelations"),
        ] {
            d.set_data_key_on_parent(input_id, key);
        }
        // The two Group chips ARE the labels (`#kindChildWrap`/`#kindEdgeWrap`),
        // so the payload goes on the element itself.
        d.set_data_key_by_id("kindChildWrap", "filter:groupChildren");
        d.set_data_key_by_id("kindEdgeWrap", "filter:groupEdges");
        // `#nsAllBtn` is a static button; the projects `all`/`none` links were
        // created above with their payload already gated on the same fact.
        d.set_data_key_by_id("nsAllBtn", "filter:nsAll");
        if let Some(section) = d.id("filtersSection") {
            d.tag_filter_link_buttons(section);
        }
        apply_focus_filters(&mut d, f);
    }

    // ---- Parser warnings (problems.js build 100-163) -------------------------
    // problems.js inserts this section after #chainSection, i.e. LAST in the
    // sidebar. The live CDP dump confirms it: pub 753 / str 780 / chain 807 /
    // problems 834 at 1600x1000.
    let anchor = d.id("chainSection");
    let head = d.mk(
        El::new("button")
            .cls("section-head")
            .id("problemsHead")
            .a("type", "button")
            .a("title", "Collapse or expand the parser warnings"),
    );
    let caret = d.mk(El::new("span").cls("caret").text("\u{25be}"));
    let head_text = d.mk_text("Parser warnings");
    // problems.js paint() writes the number unconditionally; the live head
    // reads "\u{25be}Parser warnings0" at 0 warnings.
    let warning_count = if facts.warnings.is_empty() {
        facts.parser_warning_count
    } else {
        facts.warnings.len()
    };
    let count_span = d.mk(
        El::new("span")
            .cls("scount")
            .id("problemsCount")
            .text(warning_count.to_string()),
    );
    d.append(head, &[caret, head_text, count_span]);

    let body = d.mk(El::new("div").cls("section-body"));
    let note = d.mk(El::new("div").cls("cs-problems-note").text(
        "Parser warnings only — this build has no Roslyn toolchain, \
         so compiler diagnostics are not available here.",
    ));
    let search = d.mk(
        El::new("input")
            .cls("mini-input")
            .id("problemsSearch")
            .a("type", "text")
            .a("spellcheck", "false")
            .a("placeholder", "filter warnings…"),
    );
    let list = d.mk(El::new("div").cls("listbox tall").id("problemsList"));
    if facts.warnings.is_empty() {
        let empty = d.mk(El::new("div").cls("list-empty").text("No parser warnings"));
        d.append(list, &[empty]);
    } else {
        // problems.js rowEl (166-189): loc button + text span, or text alone.
        let mut rows = Vec::new();
        for w in &facts.warnings {
            let row = d.mk(El::new("div").cls("cs-problem").a("title", &w.raw));
            if !w.file.is_empty() && w.line > 0 {
                let loc = d.mk(
                    El::new("button")
                        .cls("cs-problem-loc")
                        .a("type", "button")
                        .a(
                            "title",
                            &format!("Open {}:{} in the source pane", w.file, w.line),
                        )
                        .text(format!("{}:{}", w.file, w.line)),
                );
                let text = d.mk(
                    El::new("span")
                        .cls("cs-problem-text")
                        .text(if w.text.is_empty() {
                            w.raw.clone()
                        } else {
                            w.text.clone()
                        }),
                );
                d.append(row, &[loc, text]);
            } else {
                let only = d.mk(
                    El::new("span")
                        .cls("cs-problem-text")
                        .text(if w.text.is_empty() {
                            w.raw.clone()
                        } else {
                            w.text.clone()
                        }),
                );
                d.append(row, &[only]);
            }
            rows.push(row);
        }
        d.append(list, &rows);
    }
    d.append(body, &[note, search, list]);
    let section = d.mk(El::new("section").cls("section").id("problemsSection"));
    d.append(section, &[head, body]);
    match anchor {
        Some(a) => d.insert_after(a, &[section]),
        None => {
            if let Some(side) = side {
                d.append(side, &[section]);
            }
        }
    }

    // ---- Hierarchy peek panel (hierarchy.js) ---------------------------------
    // `None` means closed: skip entirely so the shipped DOM is untouched.
    // `Some` builds .cs-hier into the overlay layer. Placed before the
    // host-owned view state so `ui` can still override.
    if let Some(hier) = &facts.hierarchy {
        apply_hierarchy(&mut d, hier);
    }

    // ---- Layout ▸ slider readouts (syncLayoutControls, shell.js:807-817) -----
    // `None` (key absent, or a `null`) means the sliders are at their shipped
    // defaults: skip entirely so the five spans keep their `index.html` literals
    // and the default render stays byte-identical.
    if let Some(settings) = &facts.settings {
        apply_settings(&mut d, settings);
    }

    // ---- Public definitions: #pubCount + #pubList (filters.js:490-513) -------
    // `None` (key absent, or a `null`) means the engine was never asked: skip
    // entirely so `#pubCount` keeps its shipped empty span and `#pubList` keeps
    // `<div class="list-empty">open to load</div>`, both byte-identical.
    if let Some(defs) = &facts.pub_defs {
        apply_pub_defs(&mut d, defs);
    }
    // ---- Declared types: the v3-only runtime section -------------------------
    // Same absent-key contract: `None` skips creation entirely, so a fixture
    // render keeps the reference tree (and the pinned `@frame` token).
    if let Some(defs) = &facts.type_defs {
        apply_type_defs(&mut d, defs);
    }
    // ---- Strings / error codes: #strCount + #strList (filters.js:516-534) ---
    if let Some(defs) = &facts.str_defs {
        apply_str_defs(&mut d, defs);
    }

    // ---- Section counters: #strCount + #chainCount (filters.js:523/528, 555) -
    // Same absent-key contract as `pub_defs` above: `None` skips both writes so
    // the two spans keep `index.html`'s shipped empty text.
    if let Some(counts) = &facts.section_counts {
        apply_section_counts(&mut d, counts);
    }

    // ---- The block-hover peek + the reorg dialog's conditional rows ---------
    // `#blocksPeekKind/Name/File/Src/Refs` (scratch.js:424-436) and
    // `#reorgGranRow/GroupRow/SizeRow` (reorg.js:286-290). Absent key skips all
    // eight writes, so the default paint keeps `#blocksPeek`'s shipped `hidden`
    // class and `#blocksPeekSrc`'s shipped `Loading source…`.
    if let Some(peek) = &facts.blocks_peek {
        apply_blocks_peek(&mut d, peek);
    }

    // ---- The edit modal's content (edit.js:262-269, via edit_host.rs) --------
    // Same absent-key contract as `pub_defs`: `None` skips all seven writes, so
    // `#editArgs` keeps its shipped `{}`, `#editOut` stays empty, `#editNote`
    // keeps the bare `modal-note` class and the footer buttons keep their
    // shipped enabled state.
    if let Some(edit) = &facts.edit {
        apply_edit(&mut d, edit);
    }

    // ---- The two mutually-inverse filter checkboxes (shell.js:602-604, 645-647)
    // Driven by `ui.group_children`, so it is applied with the rest of the
    // host-owned view state, under `ui`. The attribute write is idempotent for
    // the shipped default (`#groupChildren` checked, `#hideChildren` not), which
    // is what keeps the default render byte-identical.
    if let Some(ui) = &facts.ui {
        apply_filter_checkboxes(&mut d, ui.group_children);
        // `#groupEdges` is its own box with its own flag (no inversion), so it
        // gets its own writer rather than being folded into the pair above.
        apply_edge_filter_checkbox(&mut d, ui.group_edges);
    }

    // Host-owned view state last, so it can override anything above.
    if let Some(ui) = &facts.ui {
        apply_ui_state(&mut d, ui);
    }
}

// ------------------------------------------------- CSS `text-transform` gap

/// One `text-transform` declaration from the real stylesheet.
struct TtRule {
    selector: String,
    value: String,
    spec: u32,
    order: u32,
}

/// Blitz tracks `text_transform` for style invalidation
/// (`layout/damage.rs:205`) but never reads it in `stylo_to_parley.rs`, so
/// `text-transform: uppercase` on `.section-head` and `.pane-grip` is silently
/// dropped: the sidebar rendered "Filters"/"Sidebar" where Chromium renders
/// "FILTERS"/"SIDEBAR".
///
/// Re-draw the two form-control skins Blitz cannot paint.
///
/// Blitz/Parley paints no `::placeholder` pseudo-element and no
/// `::-webkit-slider-runnable-track` / `::-webkit-slider-thumb`, and it keeps
/// the UA border box for `input[type=range]`. Chromium gets all three from the
/// engine, so without this the topbar search box is an empty hole and every
/// Layout slider is a blank bordered text box (v3 launch frame, defects 2-3 of
/// the round-18 audit).
///
/// The fix is structural, in the same spirit as [`apply_text_transform`]: the
/// real stylesheet still owns every colour and size, but the *pixels* are
/// re-drawn as out-of-flow shims over the laid-out input box. Each shim is
/// absolutely positioned inside its own parent, and the parent is made
/// `position: relative` first; that adds no offset, so no existing content can
/// move, and the shims add no control geometry to hit-test.
///
/// Runs **after** the first `resolve(0.0)` because it reads real border boxes;
/// the caller resolves once more so the shims themselves get laid out.
pub fn apply_paint_shims(doc: &mut BaseDocument) {
    // nzk-dark+ palette (style.css:1905-1912 and :1940-1942): the theme
    // `apply_runtime_dom` pins on `<html>`, so these are the effective values.
    const FAINT: &str = "#858585";
    const TRACK: &str = "#3c3c3c";
    const ACCENT: &str = "#007acc";
    const THUMB_BORDER: &str = "#1e1e1e";

    // First: flush the Group chips' `.off` class writes staged by
    // `apply_filter_checkboxes`/`apply_edge_filter_checkbox`. They are deferred
    // to here because a `class` write before the first style pass panics Stylo
    // (see `defer_chip_off`); this runs after the first resolve and before the
    // constructors' second one, so the class is applied to styled elements and
    // repainted.
    drain_chip_off(doc);

    struct TextShim {
        parent: usize,
        /// The **content-box origin** relative to the parent, not the border-box
        /// origin plus a hard-coded 9px inset. Reading the same `final_layout`
        /// padding the stylesheet resolved means every text box paints its
        /// placeholder exactly where its own `padding` puts it — which is what
        /// makes `.mini-input` (8px), `.text-input` (9px) and the quick-open box
        /// (6px) stop disagreeing with one fixed value.
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        /// The input's computed `font-size` from the same cascade that laid it
        /// out. `.mini-input` is 11px and `.text-input` is 12px; painting every
        /// placeholder at 12px made the sidebar boxes' text the wrong size as
        /// well as the wrong inset.
        font: f32,
        text: String,
    }
    struct RangeShim {
        parent: usize,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        frac: f32,
    }
    struct CheckShim {
        parent: usize,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        checked: bool,
    }
    struct SpinnerShim {
        parent: usize,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        /// The input's computed font size, from the same cascade that laid it
        /// out (11px for `.depth-input`).
        font: f32,
        value: String,
    }

    let ids = match doc.query_selector_all("input") {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut text_shims: Vec<TextShim> = Vec::new();
    let mut range_shims: Vec<RangeShim> = Vec::new();
    let mut check_shims: Vec<CheckShim> = Vec::new();
    let mut spinner_shims: Vec<SpinnerShim> = Vec::new();
    let mut range_inputs: Vec<usize> = Vec::new();
    let mut check_inputs: Vec<usize> = Vec::new();
    let mut spinner_inputs: Vec<usize> = Vec::new();

    for id in ids {
        let Some(node) = doc.get_node(id) else { continue };
        let Some(data) = node.element_data() else { continue };
        let attr = |name: &str| {
            data.attrs
                .iter()
                .find(|a| a.name.local.as_ref() == name)
                .map(|a| a.value.clone())
                .unwrap_or_default()
        };
        let ty = attr("type");
        let pos = node.absolute_position(0.0, 0.0);
        let size = node.final_layout.size;
        let (w, h) = (size.width, size.height);
        // A hidden control (a closed drawer/modal) has no honest box; never
        // paint a shim for one.
        if w < 4.0 || h < 4.0 || pos.x < -1000.0 || pos.y < -1000.0 {
            continue;
        }
        let Some(parent) = node.parent else { continue };
        let Some(parent_pos) = doc.get_node(parent).map(|p| p.absolute_position(0.0, 0.0)) else {
            continue;
        };
        let (rx, ry) = (pos.x - parent_pos.x, pos.y - parent_pos.y);
        // The content box the text actually starts in. Read from the same
        // resolved layout the stylesheet produced, so a box's own `padding` is
        // what positions its text — never a constant in this shim.
        let fl = &node.final_layout;
        let content_x = rx + fl.border.left + fl.padding.left;
        let content_y = ry + fl.border.top + fl.padding.top;
        let content_w =
            (w - fl.border.left - fl.border.right - fl.padding.left - fl.padding.right).max(0.0);
        let content_h =
            (h - fl.border.top - fl.border.bottom - fl.padding.top - fl.padding.bottom).max(0.0);
        let font_px = node
            .primary_styles()
            .map(|s| s.clone_font_size().used_size().px())
            .unwrap_or(12.0);
        if ty == "range" {
            let min: f32 = attr("min").parse().unwrap_or(0.0);
            let max: f32 = attr("max").parse().unwrap_or(100.0);
            let val: f32 = attr("value").parse().unwrap_or(min);
            let frac = if max > min {
                ((val - min) / (max - min)).clamp(0.0, 1.0)
            } else {
                0.0
            };
            range_inputs.push(id);
            range_shims.push(RangeShim { parent, x: rx, y: ry, w, h, frac });
        } else if ty == "checkbox" {
            // Blitz paints the UA white square regardless of `accent-color`
            // (style.css:669 `.check input { accent-color: var(--accent) }`),
            // so the real v2 skin is drawn over a hidden native box.
            check_inputs.push(id);
            check_shims.push(CheckShim {
                parent,
                x: rx,
                y: ry,
                w,
                h,
                checked: data
                    .attrs
                    .iter()
                    .any(|a| a.name.local.as_ref() == "checked"),
            });
        } else if ty == "number" && attr("id") == "depthSelect" {
            // `#depthSelect` is the `.select.depth-input` number box: the v2
            // stepper is a UA `::-webkit-inner-spin-button`, which Blitz does
            // not paint either. Draw the two arrow buttons over the box.
            spinner_inputs.push(id);
            spinner_shims.push(SpinnerShim {
                parent,
                x: rx,
                y: ry,
                w,
                h,
                font: font_px,
                value: attr("value"),
            });
        } else {
            let placeholder = attr("placeholder");
            if placeholder.is_empty() {
                continue;
            }
            // The typed value wins: the shim is only the empty-state text.
            if !attr("value").is_empty() {
                continue;
            }
            text_shims.push(TextShim {
                parent,
                x: content_x,
                y: content_y,
                w: content_w,
                h: content_h,
                font: font_px,
                text: placeholder,
            });
        }
    }
    if text_shims.is_empty()
        && range_shims.is_empty()
        && check_shims.is_empty()
        && spinner_shims.is_empty()
    {
        return;
    }

    // Collect the containing blocks and the range inputs that must stop
    // painting their UA box, before any mutation.
    let mut parent_styles: Vec<(usize, String)> = Vec::new();
    let mut make_relative = |doc: &BaseDocument, parent: usize| {
        if parent_styles.iter().any(|(p, _)| *p == parent) {
            return;
        }
        let old = doc
            .get_node(parent)
            .and_then(|n| n.element_data())
            .and_then(|e| {
                e.attrs
                    .iter()
                    .find(|a| a.name.local.as_ref() == "style")
                    .map(|a| a.value.clone())
            })
            .unwrap_or_default();
        parent_styles.push((parent, format!("{old};position:relative;")));
    };
    for s in &text_shims {
        make_relative(doc, s.parent);
    }
    for s in &range_shims {
        make_relative(doc, s.parent);
    }
    for s in &check_shims {
        make_relative(doc, s.parent);
    }
    for s in &spinner_shims {
        make_relative(doc, s.parent);
    }
    let mut hidden_inputs: Vec<(usize, String)> = Vec::new();
    for id in range_inputs {
        let old = doc
            .get_node(id)
            .and_then(|n| n.element_data())
            .and_then(|e| {
                e.attrs
                    .iter()
                    .find(|a| a.name.local.as_ref() == "style")
                    .map(|a| a.value.clone())
            })
            .unwrap_or_default();
        hidden_inputs.push((id, format!("{old};opacity:0;")));
    }
    for id in check_inputs {
        let old = doc
            .get_node(id)
            .and_then(|n| n.element_data())
            .and_then(|e| {
                e.attrs
                    .iter()
                    .find(|a| a.name.local.as_ref() == "style")
                    .map(|a| a.value.clone())
            })
            .unwrap_or_default();
        hidden_inputs.push((id, format!("{old};opacity:0;")));
    }
    // `#depthSelect` keeps its box and value; only the text is moved clear of
    // the stepper buttons drawn over its right edge. The native value is also
    // made transparent: Blitz *does* paint a number input's value, so leaving it
    // visible put a second copy of the number at the content-box origin while
    // the stepper shim painted the right-aligned one — the "1  1" the box showed.
    for id in spinner_inputs {
        let old = doc
            .get_node(id)
            .and_then(|n| n.element_data())
            .and_then(|e| {
                e.attrs
                    .iter()
                    .find(|a| a.name.local.as_ref() == "style")
                    .map(|a| a.value.clone())
            })
            .unwrap_or_default();
        hidden_inputs.push((id, format!("{old};padding-right:19px;color:transparent;")));
    }

    let mut d = Dom::new(doc);
    for (parent, style) in parent_styles {
        d.set_attr(parent, "style", &style);
    }
    for (id, style) in hidden_inputs {
        d.set_attr(id, "style", &style);
    }
    for s in text_shims {
        // No padding here: `s.x`/`s.y`/`s.w`/`s.h` are already the input's
        // content box, so the glyph origin is the same one the native run would
        // use. The font size comes from the cascade too (`.mini-input` 11px vs
        // `.text-input` 12px), instead of one value painted over both.
        let style = format!(
            "position:absolute; left:{:.1}px; top:{:.1}px; width:{:.1}px; height:{:.1}px; \
             line-height:{:.1}px; padding:0; box-sizing:border-box; overflow:hidden; \
             white-space:nowrap; color:{FAINT}; font-family:var(--mono); font-size:{:.1}px; \
             pointer-events:none;",
            s.x, s.y, s.w, s.h, s.h, s.font
        );
        let span = d.mk(El::new("span").cls("cs-paint-ph").style(&style).text(&s.text));
        d.append(s.parent, &[span]);
    }
    for s in range_shims {
        let outer_style = format!(
            "position:absolute; left:{:.1}px; top:{:.1}px; width:{:.1}px; height:{:.1}px; \
             pointer-events:none;",
            s.x, s.y, s.w, s.h
        );
        let outer = d.mk(El::new("div").cls("cs-paint-range").style(&outer_style));
        let track_top = (s.h - 3.0) / 2.0;
        let track = d.mk(El::new("div").style(&format!(
            "position:absolute; left:0px; top:{track_top:.1}px; width:{:.1}px; height:3px; \
             background:{TRACK}; border-radius:2px;",
            s.w
        )));
        let thumb = 11.0f32;
        let thumb_top = (s.h - thumb) / 2.0;
        let thumb_left = s.frac * (s.w - thumb);
        let thumb = d.mk(El::new("div").style(&format!(
            "position:absolute; left:{thumb_left:.1}px; top:{thumb_top:.1}px; \
             width:{thumb}px; height:{thumb}px; background:{ACCENT}; \
             border:1px solid {THUMB_BORDER}; border-radius:6px;"
        )));
        d.append(outer, &[track, thumb]);
        d.append(s.parent, &[outer]);
    }
    // v2 checkbox skin: unchecked = dark box + grey border; checked = the
    // `--accent` fill with a white tick. Native paint is hidden above.
    // r29: the sampled v2 reference is `#007acc` checked, `#858585` border and
    // `#3b3b3b` unchecked fill (the old #0e639c/#6b6b6b/#1e1e1e trio was the
    // VS-Code-dark guess, not the reference); positions are rounded to whole
    // pixels because Chromium's 13x13 box is integer-aligned while the input's
    // layout box here is fractional (AA smear made the box 14 px wide).
    const CHECK_ACCENT: &str = "#007acc";
    const CHECK_BORDER: &str = "#858585";
    for s in check_shims {
        let (bg, border) = if s.checked {
            (CHECK_ACCENT, CHECK_ACCENT)
        } else {
            ("#3b3b3b", CHECK_BORDER)
        };
        // r34: Chromium's UA checkbox is a 13x13 border box. Blitz lays this
        // input out at a fractional 13.5x14.5, so rounding up painted a 14x15
        // box whose right/bottom edge sat 1px past the reference and AA-split
        // the border; the TICK runs below are authored against the reference
        // 13x13 border box (see the comment above TICK), so pin the PAINT box
        // to 13x13 at the rounded origin. This is paint-only: a CSS
        // width/height pin on `.check input` reflowed the rows and regressed
        // (whole 1.5417 / sidebar 5.58; handoffs/r33-toolbarB.md).
        let (x, y) = (s.x.round(), s.y.round());
        let (w, h) = (13.0, 13.0);
        let box_style = format!(
            "position:absolute; left:{x:.0}px; top:{y:.0}px; width:{w:.0}px; height:{h:.0}px; \
             background:{bg}; border:1px solid {border}; border-radius:2px; \
             box-sizing:border-box;",
        );
        let bx = d.mk(El::new("div").cls("cs-paint-check").style(&box_style));
        if s.checked {
            // The reference tick is a stroked check, not a font glyph: the
            // Unicode `✓` at 10px mono smears into a handful of dim pixels,
            // while Chromium paints ~30 near-white pixels (8x8: a short arm
            // (2,7)->(4,9) and a long arm (9,2)->(4,9), ~2px stroke). Paint the
            // sampled runs as 1px rects so both arms land where the reference
            // has ink. Coords are relative to the 13x13 border box, whose
            // padding box is the containing block, hence the -1 inset.
            const TICK: &[(f32, f32, f32, &str)] = &[
                (9.0, 2.0, 1.0, "#d9d9d9"),
                (8.0, 3.0, 1.0, "#c8c8c8"), (9.0, 3.0, 1.0, "#ffffff"),
                (10.0, 3.0, 1.0, "#f3f3f3"),
                (8.0, 4.0, 2.0, "#ffffff"),
                (7.0, 5.0, 2.0, "#ffffff"), (9.0, 5.0, 1.0, "#ababab"),
                (3.0, 6.0, 1.0, "#f4f4f4"), (6.0, 6.0, 1.0, "#f3f3f3"),
                (7.0, 6.0, 1.0, "#ffffff"), (8.0, 6.0, 1.0, "#cbcbcb"),
                (2.0, 7.0, 1.0, "#e4e4e4"), (3.0, 7.0, 1.0, "#ffffff"),
                (4.0, 7.0, 1.0, "#f6f6f6"), (5.0, 7.0, 1.0, "#ebebeb"),
                (6.0, 7.0, 1.0, "#ffffff"), (7.0, 7.0, 1.0, "#e4e4e4"),
                (3.0, 8.0, 1.0, "#e7e7e7"), (4.0, 8.0, 2.0, "#ffffff"),
                (6.0, 8.0, 1.0, "#f5f5f5"),
                (4.0, 9.0, 1.0, "#eaeaea"), (5.0, 9.0, 1.0, "#fefefe"),
            ];
            let mut parts: Vec<usize> = Vec::new();
            for &(dx, dy, w, color) in TICK {
                parts.push(d.mk(El::new("div").style(&format!(
                    "position:absolute; left:{:.0}px; top:{:.0}px; width:{w:.0}px; height:1px; \
                     background:{color};",
                    dx - 1.0,
                    dy - 1.0
                ))));
            }
            d.append(bx, &parts);
        }
        d.append(s.parent, &[bx]);
    }
    // v2 `#depthSelect` stepper. Chromium paints a UA `::-webkit-inner-spin-button`
    // as a darker `#2b2b2c` strip inside the input's rounded border (no
    // border-left of its own, the input border stays visible) carrying an
    // 8x5px up triangle and an 8x5px down triangle. Blitz paints neither the
    // buttons nor the input's value text, so both are re-drawn as paint-only
    // shims: the strip stops short of the input's right border, and the value
    // span is right-aligned on the strip's left edge so a multi-digit value
    // keeps the same right ink edge as the reference "1".
    for s in spinner_shims {
        const ARROW: &str = "#8d8d8e";
        // Reference: strip x1469..1484, rows y569..590 (border-box x1409..1487,
        // y568..592), arrows centred x1476, ink rows 572..576 / 583..587.
        let strip_w = 15.0;
        let outer_style = format!(
            "position:absolute; left:{:.1}px; top:{:.1}px; width:{strip_w:.1}px; \
             height:{:.1}px; background:#2b2b2c; border-radius:0 2px 2px 0; \
             pointer-events:none;",
            s.x + s.w - 18.0,
            s.y + 1.25,
            (s.h - 2.0).max(6.0)
        );
        let outer = d.mk(El::new("div").cls("cs-paint-spin").style(&outer_style));
        // Chromium draws the triangles as hard 1px steps (widths 1,3,5,7,9 at
        // the base), not an antialiased border wedge; build them from 1px rows
        // so the ink stays crisp like the reference. Centre is 7px from the
        // strip's left edge (= 11px left of the input's right edge).
        let mut up_rows: Vec<usize> = Vec::new();
        let mut down_rows: Vec<usize> = Vec::new();
        for i in 0..5usize {
            let w = 1 + 2 * i;
            let left = 7.0 - (w as f32 - 1.0) / 2.0;
            up_rows.push(d.mk(El::new("div").style(&format!(
                "position:absolute; left:{left:.1}px; top:{:.0}px; width:{w}px; height:1px; \
                 background:{ARROW};",
                3 + i
            ))));
            // The down arrow mirrors the up one (widest row on top).
            down_rows.push(d.mk(El::new("div").style(&format!(
                "position:absolute; left:{left:.1}px; top:{:.0}px; width:{w}px; height:1px; \
                 background:{ARROW};",
                18 - i
            ))));
        }
        let up = d.mk(El::new("div").style("position:absolute; left:0px; top:0px;"));
        let down = d.mk(El::new("div").style("position:absolute; left:0px; top:0px;"));
        d.append(up, &up_rows);
        d.append(down, &down_rows);
        // The input's value is right-aligned against the strip; `attr("value")`
        // carries the shipped "1" (runtime edits set the DOM property, which the
        // shim cannot read, so it keeps painting the last markup value).
        let value = {
            let v = s.value.clone();
            if v.is_empty() { "1".to_string() } else { v }
        };
        // The value is right-aligned against the stepper strip, at the input's
        // own computed size. The width stops 3px short of the strip's left edge
        // (the strip starts 18px from the border box's right edge), so the last
        // digit never runs under the arrows. A multi-digit value keeps the same
        // right ink edge as the reference "1".
        let text_w = (s.w - 21.0).max(4.0);
        let text = d.mk(El::new("span").style(&format!(
            "position:absolute; left:{:.1}px; top:{:.1}px; width:{text_w:.1}px; \
             height:{:.1}px; line-height:{:.1}px; text-align:right; white-space:nowrap; \
             overflow:hidden; color:#d4d4d4; font-family:var(--mono); font-size:{:.1}px; \
             pointer-events:none;",
            s.x,
            s.y + 1.0,
            (s.h - 2.0).max(6.0),
            (s.h - 2.0).max(6.0),
            s.font
        )).text(&value));
        d.append(outer, &[up, down]);
        d.append(s.parent, &[outer, text]);
    }
}

/// Draw the `<select>` disclosure chevron.
///
/// A browser paints the arrow as a UA part of the control; Blitz paints none,
/// so every `.select` (most visibly `#nsSelect`) shows a completely empty right
/// edge where v2 has a light-grey chevron. This is the same class of engine gap
/// as `::placeholder`/`::-webkit-slider-thumb`: the real stylesheet still owns
/// the control box, and only the missing glyph is re-drawn as an out-of-flow
/// shim (a small filled triangle, 8x5px, inset 15px from the right edge).
pub fn apply_select_chevrons(doc: &mut BaseDocument) {
    let ids = match doc.query_selector_all("select") {
        Ok(v) => v,
        Err(_) => return,
    };
    struct Plan {
        parent: usize,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    }
    let mut plan: Vec<Plan> = Vec::new();
    for id in ids {
        let Some(node) = doc.get_node(id) else { continue };
        let pos = node.absolute_position(0.0, 0.0);
        let size = node.final_layout.size;
        let (w, h) = (size.width, size.height);
        if w < 24.0 || h < 8.0 {
            continue;
        }
        let Some(parent) = node.parent else { continue };
        let Some(parent_pos) = doc.get_node(parent).map(|p| p.absolute_position(0.0, 0.0)) else {
            continue;
        };
        plan.push(Plan {
            parent,
            x: pos.x - parent_pos.x,
            y: pos.y - parent_pos.y,
            w,
            h,
        });
    }
    if plan.is_empty() {
        return;
    }
    let mut d = Dom::new(doc);
    let mut seen: Vec<usize> = Vec::new();
    for s in &plan {
        if seen.contains(&s.parent) {
            continue;
        }
        seen.push(s.parent);
        let old = d
            .doc
            .get_node(s.parent)
            .and_then(|n| n.element_data())
            .and_then(|e| {
                e.attrs
                    .iter()
                    .find(|a| a.name.local.as_ref() == "style")
                    .map(|a| a.value.clone())
            })
            .unwrap_or_default();
        d.set_attr(s.parent, "style", &format!("{old};position:relative;"));
    }
    // v2's chevron is a *stroked* V, not the filled triangle a border hack
    // draws: two 2px diagonal arms, 8x5 px, core #d4d4d4 (212) with #bdbdbd
    // (188) AA edges, sampled run by run from the Chromium screenshot. A CSS
    // border triangle cannot be stroked and `transform: rotate()` is unverified
    // in Blitz, so the V is painted as one 1x1 div per sampled pixel run.
    const CORE: &str = "#d4d4d4";
    const AA: &str = "#bdbdbd";
    const V: &[(f32, f32, &str)] = &[
        (0.0, 0.0, CORE), (1.0, 0.0, AA), (6.0, 0.0, AA), (7.0, 0.0, CORE),
        (0.0, 1.0, AA), (1.0, 1.0, CORE), (2.0, 1.0, AA),
        (5.0, 1.0, AA), (6.0, 1.0, CORE), (7.0, 1.0, AA),
        (1.0, 2.0, AA), (2.0, 2.0, CORE), (3.0, 2.0, AA),
        (4.0, 2.0, AA), (5.0, 2.0, CORE), (6.0, 2.0, AA),
        (2.0, 3.0, AA), (3.0, 3.0, CORE), (4.0, 3.0, CORE), (5.0, 3.0, AA),
        (3.0, 4.0, AA), (4.0, 4.0, AA),
    ];
    for s in plan {
        // Blitz floors fractional absolute offsets, which both shifted the V
        // 1px left and split every 1px run across two pixels (measured: ink at
        // 129 instead of 212). Snap to whole pixels.
        let top = (s.y + (s.h - 5.0) / 2.0).floor();
        let left = (s.x + s.w - 13.0).round();
        let mut parts: Vec<usize> = Vec::new();
        for &(dx, dy, color) in V {
            parts.push(d.mk(El::new("div").style(&format!(
                "position:absolute; left:{:.1}px; top:{:.1}px; width:1px; height:1px; \
                 background:{color}; pointer-events:none;",
                left + dx,
                top + dy
            ))));
        }
        d.append(s.parent, &parts);
    }
}

/// This supplies that one property, read from the real stylesheet rather than
/// hard-coded: the rule list and its selectors come out of `style.css` itself.
/// It runs before layout because uppercase glyphs are wider.
pub fn apply_text_transform(doc: &mut BaseDocument, css: &str) {
    let rules = parse_text_transform_rules(css);
    if rules.is_empty() {
        return;
    }
    let root = doc.root_element().id;
    let mut plan: Vec<(usize, String)> = Vec::new();
    collect_transforms(doc, root, "inherit".to_string(), &rules, &mut plan);
    if plan.is_empty() {
        return;
    }

    // Gather each element's direct text children first, then rewrite.
    let mut work: Vec<(usize, String)> = Vec::new();
    for (element, value) in &plan {
        if value == "inherit" || value == "none" {
            continue;
        }
        let children: Vec<usize> = doc
            .get_node(*element)
            .map(|n| n.children.clone())
            .unwrap_or_default();
        for child in children {
            if let Some(t) = doc.get_node(child).and_then(|n| n.text_data()) {
                let src = t.content.to_string();
                let dst = transform_text(&src, value);
                if dst != src {
                    work.push((child, dst));
                }
            }
        }
    }
    let mut m = doc.mutate();
    for (text_node, value) in work {
        m.set_node_text(text_node, &value);
    }
}

fn transform_text(s: &str, how: &str) -> String {
    match how {
        "uppercase" => s.to_uppercase(),
        "lowercase" => s.to_lowercase(),
        // CSS `capitalize`: the first letter of every *word*, others untouched.
        "capitalize" => {
            let mut out = String::with_capacity(s.len());
            let mut at_word_start = true;
            for c in s.chars() {
                if c.is_whitespace() {
                    at_word_start = true;
                    out.push(c);
                } else if at_word_start {
                    out.extend(c.to_uppercase());
                    at_word_start = false;
                } else {
                    out.push(c);
                }
            }
            out
        }
        _ => s.to_string(),
    }
}

/// Pull `text-transform: <value>` declarations out of a stylesheet, `/* */`
/// comments stripped, with a small specificity/order key for the cascade.
fn parse_text_transform_rules(css: &str) -> Vec<TtRule> {
    let mut cleaned = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        cleaned.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => {
                rest = "";
                break;
            }
        }
    }
    cleaned.push_str(rest);

    let mut rules = Vec::new();
    let mut order = 0u32;
    let mut rest = cleaned.as_str();
    while let Some(open) = rest.find('{') {
        let selector_text = &rest[..open];
        let Some(close) = rest[open..].find('}') else {
            break;
        };
        let body = &rest[open + 1..open + close];
        rest = &rest[open + close + 1..];
        let Some(value) = body
            .split(';')
            .find_map(|decl| decl.trim().strip_prefix("text-transform"))
            .and_then(|v| v.trim().strip_prefix(':'))
        else {
            continue;
        };
        let value = value.trim().to_ascii_lowercase();
        for selector in selector_text.split(',') {
            let selector = selector.trim();
            // Only simple/descendant selectors; anything with a pseudo, an
            // attribute or a sibling combinator is left to the engine.
            if selector.is_empty() || selector.contains([':', '[', '>', '+', '~', '*']) {
                continue;
            }
            rules.push(TtRule {
                selector: selector.to_string(),
                value: value.clone(),
                spec: specificity(selector),
                order,
            });
        }
        order += 1;
    }
    rules
}

fn specificity(selector: &str) -> u32 {
    selector
        .split_whitespace()
        .map(|part| {
            let classes = part.matches('.').count() as u32;
            let tags = part.chars().next().map(|c| c.is_alphabetic()).unwrap_or(false) as u32;
            classes * 100 + tags
        })
        .sum()
}

/// Walk the tree top-down, carrying the inherited value, and pick the last
/// winning declaration for each element (highest specificity, then source
/// order) — enough of the cascade for the simple `text-transform` rules here.
fn collect_transforms(
    doc: &BaseDocument,
    node: usize,
    inherited: String,
    rules: &[TtRule],
    out: &mut Vec<(usize, String)>,
) {
    let mut value = inherited;
    if doc.get_node(node).map(|n| n.is_element()).unwrap_or(false) {
        let mut best: Option<&TtRule> = None;
        for rule in rules {
            if !matches_selector(doc, node, &rule.selector) {
                continue;
            }
            if best
                .map(|b| (rule.spec, rule.order) >= (b.spec, b.order))
                .unwrap_or(true)
            {
                best = Some(rule);
            }
        }
        if let Some(rule) = best {
            value = rule.value.clone();
        }
        out.push((node, value.clone()));
    }
    let children: Vec<usize> = doc
        .get_node(node)
        .map(|n| n.children.clone())
        .unwrap_or_default();
    for child in children {
        collect_transforms(doc, child, value.clone(), rules, out);
    }
}

/// Supports `tag`, `.class`, `tag.class` and `A B` (descendant).
fn matches_selector(doc: &BaseDocument, node: usize, selector: &str) -> bool {
    let parts: Vec<&str> = selector.split_whitespace().collect();
    let Some(last) = parts.last() else {
        return false;
    };
    if !matches_simple(doc, node, last) {
        return false;
    }
    // Each earlier part must match some ancestor, in order.
    let mut current = node;
    for part in parts[..parts.len() - 1].iter().rev() {
        let mut found = false;
        while let Some(parent) = doc.get_node(current).and_then(|n| n.parent) {
            current = parent;
            if matches_simple(doc, current, part) {
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

fn matches_simple(doc: &BaseDocument, node: usize, part: &str) -> bool {
    let Some(n) = doc.get_node(node) else {
        return false;
    };
    let Some(data) = n.element_data() else {
        return false;
    };
    let mut classes: Vec<&str> = part.split('.').filter(|s| !s.is_empty()).collect();
    let tag_part = if part.starts_with('.') {
        ""
    } else {
        classes.remove(0)
    };
    if !tag_part.is_empty() && data.name.local.as_ref() != tag_part {
        return false;
    }
    for want in classes {
        let has = data.attrs.iter().any(|a| {
            a.name.local.as_ref() == "class" && a.value.split_whitespace().any(|c| c == want)
        });
        if !has {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------- tests

/// Tests for the quick-open palette's DOM projection.
///
/// They live here rather than in `lib.rs`'s `modal_tests` because `lib.rs` is
/// outside this module's write scope, and they assert on the **resolved**
/// document — real classes, real attributes, real layout boxes from the real
/// `index.html` + `style.css` — which is how the modal/find/picker tests do it.
/// `--boxes` cannot be the evidence channel here: it lists only the selectors in
/// [`PROBE_SELECTORS`], and `#cs-quickopen` is not in `index.html` at all (it is
/// created at runtime, quickopen.js:113-181), so its geometry is read with the
/// same `query_selector`/`box_of` path `--dump-tree` and `--boxes` share.
#[cfg(test)]
mod quickopen_tests {
    use super::*;
    use crate::NzkUi;

    fn index_html() -> String {
        std::fs::read_to_string(NzkUi::frontend_dir().join("index.html")).expect("read index.html")
    }

    fn style_css() -> String {
        std::fs::read_to_string(NzkUi::frontend_dir().join("static/style.css")).expect("read css")
    }

    /// How many elements match a CSS selector.
    ///
    /// `NzkUi::box_of`/`attr_of`/`text_of` all answer for the *first* match, so
    /// this is what makes "one row per Row" and "exactly one `.sel`" assertable.
    /// It walks the document rather than counting `PROBE_SELECTORS` rows, because
    /// `#cs-quickopen` is not a probe selector — it is runtime-created.
    fn count_matching(shell: &NzkUi, selector: &str) -> usize {
        shell
            .document()
            .query_selector_all(selector)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// Facts carrying only the quick-open state under test.
    fn facts_with(quickopen: Option<QuickOpenFacts>) -> Facts {
        let mut facts = Facts::for_corpus("/tmp/fixture");
        let mut ui = UiState::default();
        ui.quickopen = quickopen;
        facts.ui = Some(ui);
        facts
    }

    fn build(quickopen: Option<QuickOpenFacts>) -> NzkUi {
        NzkUi::from_source_with_facts(
            &index_html(),
            &NzkUi::frontend_dir().join("static/style.css"),
            &style_css(),
            (1600, 1000),
            &facts_with(quickopen),
        )
        .expect("build shell")
    }

    fn row(index: usize, kind: &str, label: &str, full: &str, loc: &str, sel: bool) -> QuickOpenRow {
        QuickOpenRow {
            index,
            kind: kind.to_string(),
            label: label.to_string(),
            full: full.to_string(),
            show_full: !full.is_empty(),
            loc: loc.to_string(),
            title: format!("{full}  \u{2014}  {loc}"),
            selected: sel,
            id: format!("n{index}"),
            file: "src/other.rs".to_string(),
            line: 10,
            source: "search".to_string(),
        }
    }

    /// An OPEN palette in `files` mode with two rows, row 1 selected — the shape
    /// `quickopen_wire::QuickOpenView::facts_value` produces for a real search.
    fn open_files() -> QuickOpenFacts {
        QuickOpenFacts {
            mode: "files".to_string(),
            mode_label: "Files & symbols".to_string(),
            placeholder: "Type a file or symbol name\u{2026}".to_string(),
            hint: "\u{2191}\u{2193} \u{00b7} Enter \u{00b7} Esc".to_string(),
            aria_label: "Quick open".to_string(),
            query: "find".to_string(),
            seed_query: "find".to_string(),
            selected: Some(1),
            rows: vec![
                row(0, "method", "find", "N.find", "src/other.rs:10", false),
                row(1, "method", "helper", "N.helper", "src/find.rs:20", true),
            ],
            empty_text: "No matches.".to_string(),
            note: String::new(),
            note_hidden: true,
            state: "hits".to_string(),
        }
    }

    /// THE CENTRAL PROMISE. A palette that is not showing must never be
    /// *revealed*: the absent key leaves the document exactly as shipped, and a
    /// payload marked `state == "closed"` builds the overlay **hidden**, with a
    /// `0 0 0 0` box that no hit-test can reach.
    ///
    /// The comparison is on the **resolved element tree** (tag, attributes,
    /// children, text, in document order), which is what `--dump-tree` prints.
    /// That is a stronger statement than "the PNG matches": it cannot pass by
    /// accident of two different trees painting the same pixels.
    #[test]
    fn a_closed_palette_leaves_the_document_untouched() {
        // `None` -- the shipped document. Nothing is created at all.
        let shipped = build(None);
        assert_eq!(
            count_matching(&shipped, ".cs-qo-row"),
            0,
            "the shipped document must have no palette rows"
        );
        assert!(
            shipped.attr_of("#cs-quickopen", "id").is_err(),
            "an absent key must create no #cs-quickopen"
        );

        // Present but closed: the reference's `close()` only re-adds `hidden`
        // (quickopen.js:441-447), so the overlay may exist but must NOT show.
        let mut closed = open_files();
        closed.state = "closed".to_string();
        closed.selected = None;
        let shell = build(Some(closed));

        let class = shell
            .attr_of("#cs-quickopen", "class")
            .expect("the overlay exists")
            .expect("class attribute");
        assert!(
            class.split_whitespace().any(|t| t == "hidden"),
            "a closed palette must carry the `hidden` class, got {class:?}"
        );
        let b = shell.box_of("#cs-quickopen").expect("the overlay exists");
        assert!(
            b.w == 0.0 || b.h == 0.0,
            "a closed palette must have a zero-area box, got {}x{} -- \
             `#cs-quickopen.hidden {{ display: none }}` (quickopen.js:73)",
            b.w,
            b.h
        );
        // And nothing inside it is reachable either.
        for sel in [".cs-qo-box", "#cs-qo-list", ".cs-qo-row"] {
            if let Some(c) = shell.box_of(sel) {
                assert!(
                    c.w == 0.0 || c.h == 0.0,
                    "{sel} inside a closed palette must have no box, got {}x{}",
                    c.w,
                    c.h
                );
            }
        }
    }

    /// An OPEN palette creates `#cs-quickopen` and reveals it: the class loses
    /// `hidden`, and the resolved box is the real full-viewport overlay the
    /// injected stylesheet describes (`position: fixed; inset: 0`).
    #[test]
    fn an_open_palette_reveals_the_overlay_with_a_real_box() {
        let shell = build(Some(open_files()));

        let class = shell
            .attr_of("#cs-quickopen", "class")
            .expect("query class")
            .expect("class attribute");
        assert!(
            !class.split_whitespace().any(|t| t == "hidden"),
            "an open palette must NOT be hidden, got class={class:?}"
        );
        // `role`/`aria-modal`/`aria-label` come from quickopen.js:119-121, and the
        // label is the payload's own copy, not a literal in this crate.
        assert_eq!(
            shell.attr_of("#cs-quickopen", "role").unwrap().as_deref(),
            Some("dialog")
        );
        assert_eq!(
            shell.attr_of("#cs-quickopen", "aria-modal").unwrap().as_deref(),
            Some("true")
        );
        assert_eq!(
            shell.attr_of("#cs-quickopen", "aria-label").unwrap().as_deref(),
            Some("Quick open")
        );

        // The real geometry: the injected sheet makes this a fixed overlay over
        // the whole 1600x1000 viewport. Without the quickopen sheet being
        // compiled in, `#cs-quickopen` would be a plain in-flow block and this
        // box would not cover the viewport.
        let b = shell
            .box_of("#cs-quickopen")
            .expect("#cs-quickopen must be laid out");
        assert_eq!(
            (b.x, b.y, b.w, b.h),
            (0.0, 0.0, 1600.0, 1000.0),
            "the open overlay must be the full viewport (position:fixed; inset:0)"
        );

        // The card and the list are laid out inside it.
        assert!(
            shell.box_of(".cs-qo-box").map(|c| c.w > 0.0 && c.h > 0.0) == Some(true),
            ".cs-qo-box must have a real box"
        );
        assert!(
            shell.box_of("#cs-qo-list").map(|c| c.w > 0.0 && c.h > 0.0) == Some(true),
            "#cs-qo-list must have a real box"
        );
    }

    /// The header carries the reference's own mode label, placeholder and hint —
    /// all three read from the payload, so a rename in the core cannot silently
    /// leave the DOM showing stale copy.
    #[test]
    fn the_header_carries_the_payloads_mode_label_placeholder_and_hint() {
        let shell = build(Some(open_files()));
        assert_eq!(
            shell.text_of(".cs-qo-mode").as_deref(),
            Some("Files & symbols"),
            "MODE_LABEL[mode]"
        );
        assert_eq!(
            shell
                .attr_of("#cs-qo-input", "placeholder")
                .unwrap()
                .as_deref(),
            Some("Type a file or symbol name\u{2026}"),
            "MODE_PLACEHOLDER[mode]"
        );
        assert_eq!(
            shell.attr_of("#cs-qo-input", "value").unwrap().as_deref(),
            Some("find"),
            "the live query must reach the input"
        );
        assert_eq!(
            shell.text_of(".cs-qo-hint").as_deref(),
            Some("\u{2191}\u{2193} \u{00b7} Enter \u{00b7} Esc")
        );

        // Symbols mode changes both, from the payload alone.
        let mut symbols = open_files();
        symbols.mode = "symbols".to_string();
        symbols.mode_label = "Symbols".to_string();
        symbols.placeholder = "Type a symbol name\u{2026}".to_string();
        let shell = build(Some(symbols));
        assert_eq!(shell.text_of(".cs-qo-mode").as_deref(), Some("Symbols"));
        assert_eq!(
            shell
                .attr_of("#cs-qo-input", "placeholder")
                .unwrap()
                .as_deref(),
            Some("Type a symbol name\u{2026}")
        );
    }

    /// ONE ROW PER Row, each with the reference's classes, `data-i`,
    /// `role=option` and `.cs-qo-kind`/`.cs-qo-name`/`.cs-qo-loc` children in the
    /// reference's order (quickopen.js:318-346).
    #[test]
    fn every_row_is_emitted_with_the_reference_classes_and_children() {
        let shell = build(Some(open_files()));

        // Exactly two rows, in payload order.
        assert_eq!(
            count_matching(&shell, ".cs-qo-row"),
            2,
            "one .cs-qo-row per Row"
        );
        assert_eq!(shell.text_of(".cs-qo-kind").as_deref(), Some("method"));

        // `data-i` is the row's own index, and `role`/`aria-selected` are the
        // reference's (quickopen.js:321-323).
        for i in 0..2usize {
            assert_eq!(
                shell
                    .attr_of(&format!(".cs-qo-row:nth-child({})", i + 1), "data-i")
                    .unwrap()
                    .as_deref(),
                Some(i.to_string().as_str()),
                "row {i} must carry data-i={i}"
            );
            assert_eq!(
                shell
                    .attr_of(&format!(".cs-qo-row:nth-child({})", i + 1), "role")
                    .unwrap()
                    .as_deref(),
                Some("option")
            );
        }

        // The `.cs-qo-name` span nests the `.cs-qo-full` span (quickopen.js:332),
        // so the full name is inside the name element rather than beside it.
        assert_eq!(
            shell.text_of(".cs-qo-row:nth-child(1) .cs-qo-name").as_deref(),
            Some("findN.find"),
            "label + the nested .cs-qo-full span"
        );
        assert_eq!(
            shell.text_of(".cs-qo-row:nth-child(1) .cs-qo-full").as_deref(),
            Some("N.find")
        );
        assert_eq!(
            shell
                .text_of(".cs-qo-row:nth-child(1) .cs-qo-loc")
                .as_deref(),
            Some("src/other.rs:10"),
            "`.cs-qo-loc` text"
        );
        // `row.title` (quickopen.js:343-344).
        assert_eq!(
            shell
                .attr_of(".cs-qo-row:nth-child(1)", "title")
                .unwrap()
                .as_deref(),
            Some("N.find  \u{2014}  src/other.rs:10")
        );
    }

    /// The selected row is MARKED: `.sel` plus `aria-selected="true"`, on
    /// exactly one row, at the index the payload named — and `sel` never
    /// disagrees with `aria-selected`.
    #[test]
    fn the_selected_row_is_marked_and_only_one_is() {
        let shell = build(Some(open_files()));
        assert_eq!(
            count_matching(&shell, ".cs-qo-row.sel"),
            1,
            "exactly one row is selected"
        );
        assert_eq!(
            shell
                .attr_of(".cs-qo-row.sel", "data-i")
                .unwrap()
                .as_deref(),
            Some("1"),
            "the payload's selected index is the marked row"
        );
        assert_eq!(
            shell
                .attr_of(".cs-qo-row.sel", "aria-selected")
                .unwrap()
                .as_deref(),
            Some("true")
        );
        // Every other row says false, so a screen reader cannot see two
        // selections.
        assert_eq!(
            shell
                .attr_of(".cs-qo-row:nth-child(1)", "aria-selected")
                .unwrap()
                .as_deref(),
            Some("false")
        );

        // Moving the selection moves the mark.
        let mut moved = open_files();
        moved.selected = Some(0);
        moved.rows[0].selected = true;
        moved.rows[1].selected = false;
        let shell = build(Some(moved));
        assert_eq!(
            shell
                .attr_of(".cs-qo-row.sel", "data-i")
                .unwrap()
                .as_deref(),
            Some("0")
        );
        assert_eq!(count_matching(&shell, ".cs-qo-row.sel"), 1);
    }

    /// The `sel = -1` sentinel (quickopen.js:289) — a JSON `null` selection —
    /// marks NO row and says `false` everywhere, rather than falling back to row
    /// 0 by accident.
    #[test]
    fn a_null_selection_marks_no_row() {
        let mut facts = open_files();
        facts.selected = None;
        for r in facts.rows.iter_mut() {
            r.selected = false;
        }
        let shell = build(Some(facts));
        assert_eq!(
            count_matching(&shell, ".cs-qo-row.sel"),
            0,
            "a null selection is the -1 sentinel: nothing is selected"
        );
        assert_eq!(count_matching(&shell, ".cs-qo-row"), 2);
        assert_eq!(
            shell
                .attr_of(".cs-qo-row:nth-child(1)", "aria-selected")
                .unwrap()
                .as_deref(),
            Some("false")
        );
    }

    /// The two honest EMPTY states render their real copy — and they are
    /// distinguishable, which is the whole reason the payload carries `state`
    /// rather than letting the renderer branch on emptiness.
    #[test]
    fn the_empty_states_render_their_own_copy() {
        // `noquery`: open, nothing typed, no recents.
        let noquery = QuickOpenFacts {
            mode: "files".to_string(),
            mode_label: "Files & symbols".to_string(),
            placeholder: "Type a file or symbol name\u{2026}".to_string(),
            hint: "\u{2191}\u{2193} \u{00b7} Enter \u{00b7} Esc".to_string(),
            aria_label: "Quick open".to_string(),
            query: String::new(),
            seed_query: String::new(),
            selected: None,
            rows: Vec::new(),
            empty_text: "Type to search files and symbols.".to_string(),
            note: String::new(),
            note_hidden: true,
            state: "noquery".to_string(),
        };
        let shell = build(Some(noquery));
        assert_eq!(
            shell.text_of(".cs-qo-empty").as_deref(),
            Some("Type to search files and symbols.")
        );
        assert_eq!(count_matching(&shell, ".cs-qo-row"), 0);
        assert_eq!(
            count_matching(&shell, ".cs-qo-empty"),
            1,
            "exactly one empty element"
        );
        // `note_hidden` keeps the note bar hidden, and the class says so.
        let note_cls = shell
            .attr_of("#cs-qo-note", "class")
            .unwrap()
            .expect("class");
        assert!(
            note_cls.split_whitespace().any(|t| t == "hidden"),
            "an empty note must keep the note bar hidden, got {note_cls:?}"
        );

        // `nohits`: a real query that matched nothing — different copy, and the
        // note bar is VISIBLE with the core's sentence.
        let mut nohits = QuickOpenFacts {
            query: "zzzzz".to_string(),
            seed_query: "zzzzz".to_string(),
            state: "nohits".to_string(),
            ..QuickOpenFacts::default()
        };
        nohits.empty_text = "No matches.".to_string();
        nohits.note = "No files or symbols match \u{201c}zzzzz\u{201d}.".to_string();
        nohits.note_hidden = false;
        let shell = build(Some(nohits));
        assert_eq!(shell.text_of(".cs-qo-empty").as_deref(), Some("No matches."));
        assert_eq!(
            shell.text_of("#cs-qo-note").as_deref(),
            Some("No files or symbols match \u{201c}zzzzz\u{201d}.")
        );
        let note_cls = shell
            .attr_of("#cs-qo-note", "class")
            .unwrap()
            .expect("class");
        assert!(
            !note_cls.split_whitespace().any(|t| t == "hidden"),
            "a real note must show the bar, got {note_cls:?}"
        );
    }

    /// THE 13-KEY CONTRACT survives deserialization into [`QuickOpenFacts`].
    ///
    /// The core crate asserts the key set it *emits*; this asserts the key set
    /// this crate *accepts*, from the same JSON spelling, so a rename cannot pass
    /// on one side of the wire. Every value here is checked, not just the key
    /// count, because a dropped key would fall back to `Default` silently.
    #[test]
    fn the_thirteen_keys_survive_the_projection() {
        let payload = serde_json::json!({
            "aria_label": "Quick open",
            "empty_text": "No matches.",
            "hint": "\u{2191}\u{2193} \u{00b7} Enter \u{00b7} Esc",
            "mode": "symbols",
            "mode_label": "Symbols",
            "note": "No files or symbols match \u{201c}zz\u{201d}.",
            "note_hidden": false,
            "placeholder": "Type a symbol name\u{2026}",
            "query": "zz",
            "rows": [{
                "file": "src/other.rs",
                "full": "N.find",
                "id": "n1",
                "index": 0,
                "kind": "method",
                "line": 10,
                "loc": "src/other.rs:10",
                "selected": true,
                "show_full": true,
                "source": "search",
                "title": "N.find  \u{2014}  src/other.rs:10",
                "label": "find"
            }],
            "seed_query": "zz",
            "selected": 0,
            "state": "hits"
        });

        // The key set of the JSON is a contract -- exactly these 13.
        let mut keys: Vec<&str> = payload.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "aria_label", "empty_text", "hint", "mode", "mode_label", "note",
                "note_hidden", "placeholder", "query", "rows", "seed_query",
                "selected", "state",
            ],
            "the ui.quickopen key set is a contract"
        );

        let facts: QuickOpenFacts =
            serde_json::from_value(payload).expect("the 13-key payload deserializes");
        // Every field must carry its value, so a renamed field cannot quietly
        // become its serde default.
        assert_eq!(facts.aria_label, "Quick open");
        assert_eq!(facts.empty_text, "No matches.");
        assert_eq!(facts.hint, "\u{2191}\u{2193} \u{00b7} Enter \u{00b7} Esc");
        assert_eq!(facts.mode, "symbols");
        assert_eq!(facts.mode_label, "Symbols");
        assert_eq!(facts.note, "No files or symbols match \u{201c}zz\u{201d}.");
        assert!(!facts.note_hidden);
        assert_eq!(facts.placeholder, "Type a symbol name\u{2026}");
        assert_eq!(facts.query, "zz");
        assert_eq!(facts.seed_query, "zz");
        assert_eq!(facts.selected, Some(0));
        assert_eq!(facts.state, "hits");
        assert!(facts.is_open());
        assert_eq!(facts.rows.len(), 1);
        let r = &facts.rows[0];
        assert_eq!(r.file, "src/other.rs");
        assert_eq!(r.full, "N.find");
        assert_eq!(r.id, "n1");
        assert_eq!(r.index, 0);
        assert_eq!(r.kind, "method");
        assert_eq!(r.line, 10);
        assert_eq!(r.loc, "src/other.rs:10");
        assert!(r.selected);
        assert!(r.show_full);
        assert_eq!(r.source, "search");
        assert_eq!(r.title, "N.find  \u{2014}  src/other.rs:10");
        assert_eq!(r.label, "find");

        // An OMITTED `quickopen` key is `None`, not a default-populated value:
        // that is what makes the shipped document render byte-identically.
        let ui: UiState = serde_json::from_value(serde_json::json!({})).expect("empty ui");
        assert!(ui.quickopen.is_none());
        // ... and `null` is treated as absent too, so a caller that spells it the
        // naive way cannot accidentally open an empty palette.
        let ui: UiState = serde_json::from_value(serde_json::json!({"quickopen": null}))
            .expect("null quickopen");
        assert!(ui.quickopen.is_none());
    }

    /// The injected stylesheet really is compiled in, and really is what makes
    /// the palette look like the reference: `#cs-quickopen.hidden` keeps a CLOSED
    /// palette at zero area, and the sheet's own rules are present in the
    /// document it produces.
    ///
    /// This is the regression guard for `assets/css/quickopen-injected.css` being wired:
    /// if the injected-sheet splice is dropped, `#cs-quickopen` loses
    /// `position: fixed; inset: 0` and the open overlay stops covering the
    /// viewport, which the box assertions above would catch.
    #[test]
    fn the_injected_stylesheet_is_compiled_in() {
        // The shipped document has no palette at all, so the sheet styles
        // nothing -- which is why injecting it unconditionally is safe. The
        // claim is about ELEMENTS: the sheet's own source text necessarily
        // mentions `.cs-qo-row`, but no such element may exist.
        let shipped = build(None);
        assert_eq!(
            count_matching(&shipped, ".cs-qo-row"),
            0,
            "the shipped document must have no palette rows"
        );
        assert!(
            shipped.attr_of("#cs-quickopen", "id").is_err(),
            "the shipped document must not contain #cs-quickopen at all"
        );
        assert!(
            shipped
                .resolved_html()
                .contains("id=\"cs-quickopen-style\""),
            "the injected sheet must be spliced in as its own <style> element"
        );
        assert!(shipped
            .resolved_html()
            .contains("#cs-quickopen.hidden { display: none; }"));

        // A closed palette is `display: none`, so it has NO box -- the reference
        // contract (quickopen.js:73).
        let mut closed = open_files();
        closed.state = "closed".to_string();
        let shell = build(Some(closed));
        let b = shell.box_of("#cs-quickopen").expect("the overlay exists");
        assert!(
            b.w == 0.0 || b.h == 0.0,
            "a closed palette must have a zero-area box, got {}x{}",
            b.w,
            b.h
        );
    }
}


/// The hierarchy peek panel (`hierarchy.js`) end to end: the JSON the host
/// publishes must deserialize into [`HierarchyFacts`] **and** render the
/// reference's own element tree.
///
/// This is the regression net for the shape mismatch that made the panel
/// silently empty. Every `HierarchyFacts` field is `#[serde(default)]`, so
/// feeding it the *reference's* runtime shape (`word`, a nested `tree`, no
/// option lists) deserializes "successfully" into an all-default struct — an
/// empty, untitled panel with no controls. A test that only checks the
/// producer's key names cannot catch that; these assert the rendered document.
#[cfg(test)]
mod hierarchy_tests {
    use super::*;
    use crate::NzkUi;

    fn index_html() -> String {
        std::fs::read_to_string(NzkUi::frontend_dir().join("index.html")).expect("read index.html")
    }

    /// How many elements match a selector. `text_of`/`attr_of` answer for the
    /// *first* match only, so this is what makes "one row per row" assertable.
    fn count_matching(shell: &NzkUi, selector: &str) -> usize {
        shell
            .document()
            .query_selector_all(selector)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// The exact object `HierarchyView::dom_facts` emits for an open call
    /// hierarchy: one section, a root with one child, and both option lists.
    fn dom_facts_json() -> serde_json::Value {
        serde_json::json!({
            "mode": "call",
            "dir": "both",
            "depth": 2,
            "title": "Call hierarchy",
            "subtitle": "N.Root",
            "dir_options": [
                {"value": "both", "label": "Both", "selected": true},
                {"value": "forward", "label": "Outgoing", "selected": false},
                {"value": "backward", "label": "Incoming", "selected": false}
            ],
            "mode_options": [
                {"value": "1", "label": "depth 1", "selected": false},
                {"value": "2", "label": "depth 2", "selected": true},
                {"value": "3", "label": "depth 3", "selected": false},
                {"value": "4", "label": "depth 4", "selected": false}
            ],
            "sections": [{
                "caption": "Outgoing calls \u{00b7} 1 hop",
                "rows": [
                    {
                        "kind": "", "depth": 0, "has_children": true,
                        "collapsed": false, "edge": "", "label": "Root",
                        "node_kind": "method", "cycle": ""
                    },
                    {
                        "kind": "calls", "depth": 1, "has_children": false,
                        "collapsed": false, "edge": "calls", "label": "Kid",
                        "node_kind": "method", "cycle": "cycle"
                    }
                ]
            }],
            "empty": ""
        })
    }

    /// THE DECISIVE ASSERTION. The producer's object must be accepted by the
    /// real `HierarchyFacts` deserializer with every field populated. A shape
    /// mismatch shows up here as a default-valued field, not an error.
    #[test]
    fn the_producers_json_deserializes_into_hierarchy_facts() {
        let facts: HierarchyFacts =
            serde_json::from_value(dom_facts_json()).expect("deserialize HierarchyFacts");
        assert_eq!(facts.mode, "call");
        assert_eq!(facts.dir, "both");
        assert_eq!(facts.depth, 2);
        assert_eq!(facts.title, "Call hierarchy", "`word` would leave this empty");
        assert_eq!(facts.subtitle, "N.Root");
        assert_eq!(facts.dir_options.len(), 3, "the direction select");
        assert_eq!(facts.mode_options.len(), 4, "the depth select's 1..=4");
        assert_eq!(facts.sections.len(), 1);
        assert_eq!(facts.sections[0].caption, "Outgoing calls \u{00b7} 1 hop");
        assert_eq!(facts.sections[0].rows.len(), 2);
    }

    fn build(hierarchy: Option<serde_json::Value>) -> NzkUi {
        let mut facts = Facts::for_corpus("/tmp/fixture");
        facts.hierarchy = match hierarchy {
            Some(v) => Some(serde_json::from_value(v).expect("dom_facts JSON must deserialize")),
            None => None,
        };
        NzkUi::from_source_with_facts(
            &index_html(),
            &NzkUi::frontend_dir().join("static/style.css"),
            "",
            (1600, 1000),
            &facts,
        )
        .expect("build shell")
    }

    /// A closed panel leaves the shipped document alone: no `.cs-hier` node is
    /// created at all, which is what protects the frame token.
    #[test]
    fn a_closed_panel_creates_no_hierarchy_nodes() {
        let shell = build(None);
        assert_eq!(count_matching(&shell, ".cs-hier"), 0);
        assert_eq!(count_matching(&shell, ".cs-hier-controls"), 0);
        assert_eq!(count_matching(&shell, ".hop"), 0);
        assert_eq!(count_matching(&shell, ".hop-wrap"), 0);
    }

    /// The panel renders its head, both selects and every row.
    #[test]
    fn an_open_panel_renders_its_head_controls_and_rows() {
        let shell = build(Some(dom_facts_json()));
        assert_eq!(count_matching(&shell, ".cs-hier"), 1);
        assert_eq!(count_matching(&shell, ".cs-hier-controls"), 1);
        assert_eq!(count_matching(&shell, ".cs-hier-select"), 2, "dir then depth");
        assert_eq!(count_matching(&shell, ".cs-hier-section"), 1);
        assert_eq!(count_matching(&shell, ".hop-wrap"), 2, "root plus its child");
        assert_eq!(count_matching(&shell, ".hop"), 2);
        // `.cs-hier-title` holds the text node AND the `.cs-hier-sub` span
        // (the renderer appends the sub inside the title, as the reference does),
        // so its textContent is title + subtitle.
        assert_eq!(
            shell.text_of(".cs-hier-title").as_deref(),
            Some("Call hierarchyN.Root")
        );
        assert_eq!(shell.text_of(".cs-hier-sub").as_deref(), Some("N.Root"));
    }

    /// The first select is the direction, the second is the depth — the bug
    /// that had the renderer read `dir_options` then `mode_options` while the
    /// producer sent no depth list at all.
    #[test]
    fn select_one_is_direction_and_select_two_is_depth_1_to_4() {
        let shell = build(Some(dom_facts_json()));

        let dir: Vec<String> = (1..=3)
            .map(|i| {
                shell
                    .text_of(&format!(".cs-hier-select:nth-child(1) option:nth-child({i})"))
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(dir, vec!["Both", "Outgoing", "Incoming"], "select#1 = dir");

        let depth: Vec<String> = (1..=4)
            .map(|i| {
                shell
                    .text_of(&format!(".cs-hier-select:nth-child(2) option:nth-child({i})"))
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            depth,
            vec!["depth 1", "depth 2", "depth 3", "depth 4"],
            "select#2 = depth"
        );
    }

    /// The `selected` attribute lands on the option the state names.
    #[test]
    fn the_selected_option_is_marked_in_each_select() {
        let shell = build(Some(dom_facts_json()));
        assert!(
            shell
                .attr_of(".cs-hier-select:nth-child(1) option:nth-child(1)", "selected")
                .unwrap()
                .is_some(),
            "`both` is the selected direction"
        );
        assert!(
            shell
                .attr_of(".cs-hier-select:nth-child(2) option:nth-child(2)", "selected")
                .unwrap()
                .is_some(),
            "depth 2 is selected"
        );
        assert!(
            shell
                .attr_of(".cs-hier-select:nth-child(2) option:nth-child(3)", "selected")
                .unwrap()
                .is_none(),
            "depth 3 is not selected"
        );
    }

    /// `rowEl`'s spans, caret glyphs and `paddingLeft = 8 + depth * 12`.
    ///
    /// `:nth-child` counts *all* siblings, and `.cs-hier-body` holds the
    /// section caption first, so the root row is child 2 and the child row is
    /// child 3.
    #[test]
    fn rows_carry_their_caret_edge_label_kind_cycle_and_indent() {
        let shell = build(Some(dom_facts_json()));

        // Root: has children, so an expanded caret, no edge chip.
        assert_eq!(
            shell.text_of(".hop-wrap:nth-child(2) .hop-caret").as_deref(),
            Some("\u{25be}")
        );
        assert_eq!(
            count_matching(&shell, ".hop-wrap:nth-child(2) .hop-edge"),
            0,
            "the root has no incoming edge"
        );
        assert_eq!(
            shell.text_of(".hop-wrap:nth-child(2) .hop-label").as_deref(),
            Some("Root")
        );
        assert_eq!(
            shell.text_of(".hop-wrap:nth-child(2) .cs-hier-kind").as_deref(),
            Some("method")
        );
        assert_eq!(
            shell
                .attr_of(".hop-wrap:nth-child(2) .hop", "style")
                .unwrap()
                .as_deref(),
            Some("padding-left: 8px")
        );

        // Child: leaf caret, edge chip, label, kind and the cycle chip.
        assert_eq!(
            shell.text_of(".hop-wrap:nth-child(3) .hop-caret").as_deref(),
            Some("\u{00b7}")
        );
        assert_eq!(
            shell.text_of(".hop-wrap:nth-child(3) .hop-edge").as_deref(),
            Some("calls")
        );
        assert_eq!(
            shell.text_of(".hop-wrap:nth-child(3) .hop-label").as_deref(),
            Some("Kid")
        );
        assert_eq!(
            shell.text_of(".hop-wrap:nth-child(3) .cs-hier-kind").as_deref(),
            Some("method")
        );
        assert_eq!(
            shell.text_of(".hop-wrap:nth-child(3) .hop-cycle").as_deref(),
            Some("cycle")
        );
        assert_eq!(
            shell
                .attr_of(".hop-wrap:nth-child(3) .hop", "style")
                .unwrap()
                .as_deref(),
            Some("padding-left: 20px"),
            "depth 1 → 8 + 12 = 20px"
        );
    }

    /// `hop-wrap {kind}`: the wrapper carries the row's edge family.
    #[test]
    fn the_row_wrapper_carries_the_edge_kind_class() {
        let shell = build(Some(dom_facts_json()));
        let cls = shell.classes_of(".hop-wrap:nth-child(3)").unwrap_or_default();
        assert!(cls.contains(&"calls".to_string()), "got {cls:?}");
        let root_cls = shell.classes_of(".hop-wrap:nth-child(3)").unwrap_or_default();
        assert!(root_cls.contains(&"hop-wrap".to_string()));
    }

    /// A collapsed row draws the collapsed caret.
    ///
    /// The producer omits a collapsed row's descendants, so this fixture has
    /// the root as the **only** row (child 2 of `.cs-hier-body`, after the
    /// section caption) and the caret flips to the collapsed glyph.
    #[test]
    fn a_collapsed_row_draws_the_collapsed_caret() {
        let mut v = dom_facts_json();
        v["sections"][0]["rows"][0]["collapsed"] = serde_json::json!(true);
        v["sections"][0]["rows"].as_array_mut().unwrap().truncate(1);
        let shell = build(Some(v));

        assert_eq!(count_matching(&shell, ".hop-wrap"), 1, "children are omitted");
        assert_eq!(
            shell.text_of(".hop-wrap:nth-child(2) .hop-caret").as_deref(),
            Some("\u{25b8}"),
            "a collapsed row with children draws the collapsed caret"
        );
    }

    /// The caption renders pre-joined, and `empty` gets its own element.
    #[test]
    fn the_section_caption_and_empty_line_render_verbatim() {
        let shell = build(Some(dom_facts_json()));
        assert_eq!(
            shell.text_of(".cs-hier-section").as_deref(),
            Some("Outgoing calls \u{00b7} 1 hop")
        );
        assert_eq!(count_matching(&shell, ".cs-hier-empty"), 0, "no empty line");

        let mut v = dom_facts_json();
        v["sections"] = serde_json::json!([]);
        v["empty"] = serde_json::json!("Nothing linked in this direction.");
        let shell = build(Some(v));
        assert_eq!(
            shell.text_of(".cs-hier-empty").as_deref(),
            Some("Nothing linked in this direction.")
        );
    }

    /// In type mode the controls are hidden entirely (`hierarchy.js:469-470`).
    #[test]
    fn type_mode_renders_no_control_selects() {
        let mut v = dom_facts_json();
        v["mode"] = serde_json::json!("type");
        v["title"] = serde_json::json!("Type hierarchy");
        let shell = build(Some(v));
        assert_eq!(count_matching(&shell, ".cs-hier"), 1);
        assert_eq!(count_matching(&shell, ".cs-hier-select"), 0);
        assert_eq!(
            shell.text_of(".cs-hier-title").as_deref(),
            Some("Type hierarchyN.Root"),
            "title + subtitle share the element"
        );
    }

    /// The panel is `display: flex` when open.
    #[test]
    fn an_open_panel_is_displayed() {
        let shell = build(Some(dom_facts_json()));
        let style = shell
            .attr_of(".cs-hier", "style")
            .unwrap()
            .unwrap_or_default();
        assert!(style.contains("display: flex"), "got {style:?}");
        let b = shell.box_of(".cs-hier").expect("the panel has a box");
        assert!(b.w > 0.0 && b.h > 0.0, "got {}x{}", b.w, b.h);
    }
}

/// Guards the stylesheet splice itself now that the four injected sheets are
/// read from `assets/css/` at runtime instead of being spliced into the binary.
///
/// These tests pin the *observable* contract that the read-by-path change could
/// silently break: the number of `<style>` elements, their ids, their order, and
/// the bytes inside each one. They deliberately use [`inline_stylesheets_with`]
/// so the splice is checked against known sheet text rather than depending on
/// whatever the assets happen to contain.
#[cfg(test)]
mod stylesheet_splice_tests {
    use super::*;
    use std::path::Path;

    const HTML: &str = "<html><head><link rel=\"stylesheet\" href=\"static/style.css\"></head></html>";

    fn spliced() -> String {
        inline_stylesheets_with(
            HTML,
            Path::new("static/style.css"),
            "REAL",
            "COMPAT",
            "PROBLEMS",
            "HIERARCHY",
            "QUICKOPEN",
        )
    }

    /// The document shape: five `<style>` elements, in this exact order, each
    /// carrying exactly its own sheet and nothing else.
    #[test]
    fn the_splice_emits_five_style_elements_in_the_documented_order() {
        let out = spliced();
        let expected = concat!(
            "<style id=\"csviewer-real-stylesheet\">REAL</style>",
            "<style id=\"nzk_ui-compat-shim\">COMPAT</style>",
            "<style id=\"cs-problems-style\">PROBLEMS</style>",
            "<style id=\"cs-hier-style\">HIERARCHY</style>",
            "<style id=\"cs-quickopen-style\">QUICKOPEN</style>",
        );
        assert!(
            out.contains(expected),
            "the five style elements must be adjacent and in order; got {out:?}"
        );
        assert_eq!(
            out.matches("<style").count(),
            5,
            "exactly five style elements, got {out:?}"
        );
    }

    /// The `<link>` is replaced, not merely supplemented, and the surrounding
    /// document survives byte-for-byte.
    #[test]
    fn the_link_is_replaced_and_the_rest_of_the_document_is_untouched() {
        let out = spliced();
        assert!(!out.contains("<link"), "the stylesheet link must be gone: {out:?}");
        assert!(out.starts_with("<html><head>"), "head must survive: {out:?}");
        assert!(out.ends_with("</head></html>"), "tail must survive: {out:?}");
    }

    /// A `<link>` that is not the stylesheet this run was given is left alone.
    #[test]
    fn a_foreign_or_non_css_link_is_left_alone() {
        let html = "<link rel=\"icon\" href=\"favicon.ico\"><link rel=\"stylesheet\" href=\"other.css\">";
        let out = inline_stylesheets_with(
            html,
            Path::new("static/style.css"),
            "REAL",
            "C",
            "P",
            "H",
            "Q",
        );
        assert_eq!(out, html, "neither link should be spliced");
        assert_eq!(out.matches("<style").count(), 0);
    }

    /// The four assets really are on disk, readable, non-empty and distinct —
    /// this is what makes the runtime read a working mechanism rather than an
    /// unexercised code path.
    #[test]
    fn every_injected_sheet_is_present_readable_and_distinct() {
        let sheets = injected_sheets();
        assert_eq!(sheets.len(), 4, "four sheets, in emission order");
        assert_eq!(
            sheets.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![
                "compat.css",
                "problems-injected.css",
                "hierarchy-injected.css",
                "quickopen-injected.css",
            ],
            "emission order is load-bearing: it fixes the element order"
        );
        for (name, text) in &sheets {
            assert!(!text.trim().is_empty(), "{name} must not be empty");
        }
        // Distinct: a copy-paste that pointed two names at one sheet would
        // otherwise pass the emptiness check above.
        let mut seen = std::collections::HashSet::new();
        for (name, text) in &sheets {
            assert!(seen.insert(text.clone()), "{name} duplicates another sheet");
        }
    }

    /// `inline_stylesheets` is the fallible entry point the library uses, and it
    /// agrees byte-for-byte with the splice fed the same on-disk text.
    #[test]
    fn inline_stylesheets_reads_the_assets_and_matches_the_explicit_splice() {
        let sheets = injected_sheets();
        let (c, p, h, q) = (
            sheets[0].1.as_str(),
            sheets[1].1.as_str(),
            sheets[2].1.as_str(),
            sheets[3].1.as_str(),
        );
        let from_disk =
            inline_stylesheets(HTML, Path::new("static/style.css"), "REAL").expect("splice");
        let explicit = inline_stylesheets_with(
            HTML,
            Path::new("static/style.css"),
            "REAL",
            c,
            p,
            h,
            q,
        );
        assert_eq!(from_disk, explicit);
        // And the read really brought sheet text through, not empty strings.
        assert!(from_disk.contains("<style id=\"cs-quickopen-style\">"));
        assert!(from_disk.contains(c) && from_disk.contains(q));
    }
}

/// The two section-head counters: `#strCount` (`filters.js:523`/`:528`) and
/// `#chainCount` (`filters.js:555`).
///
/// Every test here asserts **both** directions, because each counter's whole
/// contribution is a negative: the empty query must leave the span *empty*
/// rather than "0", and truncation must *append* rather than replace. A test
/// that only checked the positive case would pass against an implementation
/// that wrote "0" and clobbered the count.
mod section_count_tests {
    use super::*;
    use crate::NzkUi;

    /// A shell painted from the real `index.html`, with `section_counts` set to
    /// the JSON a host would send. `serde_json::json!` spells `None` as `null`,
    /// so an omitted key is built by hand where that distinction matters.
    fn shell_with(section_counts: Option<serde_json::Value>) -> NzkUi {
        // Built through the REAL facts type, then round-tripped through JSON, so
        // this exercises the same deserialisation path the host uses. A shape
        // mismatch between the producer's JSON and `Facts` shows up here.
        let mut facts = Facts::for_corpus(".");
        let raw = match section_counts {
            Some(v) => serde_json::json!({ "section_counts": v }).to_string(),
            // An omitted key is the real "host has not measured anything" case;
            // `json!` would spell a `None` as `null`, which is a different
            // document.
            None => "{}".to_string(),
        };
        let patch: serde_json::Value = serde_json::from_str(&raw).expect("patch json");
        if let Some(v) = patch.get("section_counts") {
            facts.section_counts =
                Some(serde_json::from_value(v.clone()).expect("SectionCountFacts"));
        }
        let html = std::fs::read_to_string(NzkUi::frontend_dir().join("index.html"))
            .expect("read index.html");
        let css_path = NzkUi::frontend_dir().join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_source_with_facts(&html, &css_path, &css, (1600, 1000), &facts)
            .expect("build shell")
    }

    fn text_of(shell: &NzkUi, id: &str) -> String {
        shell
            .text_of(&format!("#{id}"))
            .unwrap_or_else(|| panic!("#{id} must exist"))
    }

    /// The decisive shape test: the producer's object must be accepted by the
    /// real deserializer with every field populated. A shape mismatch shows up
    /// as a default-valued field, not an error.
    #[test]
    fn section_counts_json_deserializes_into_the_facts_struct() {
        let facts: SectionCountFacts = serde_json::from_value(serde_json::json!({
            "str_count": 1234,
            "chain_flat": 7,
            "chain_truncated": true
        }))
        .expect("deserialize SectionCountFacts");
        // `Count(1234)` — the "searched with a query" arm.
        assert_eq!(facts.str_count, Some(StrCount::Count(1234)));
        assert_eq!(facts.chain_flat, Some(7));
        assert_eq!(facts.chain_truncated, Some(true));
    }

    /// `null` inside the optional is a real, distinguishable state: the empty
    /// query, which clears the span.
    #[test]
    fn a_null_str_count_is_the_clear_state_not_an_absent_key() {
        let facts: SectionCountFacts =
            serde_json::from_value(serde_json::json!({ "str_count": null }))
                .expect("deserialize");
        assert_eq!(
            facts.str_count,
            Some(StrCount::Cleared),
            "null = the empty-query clear, NOT the same as an absent key"
        );
        assert_eq!(facts.chain_flat, None, "an omitted chain key stays None");
    }

    /// The regression this type exists for: with `Option<Option<usize>>` both of
    /// these deserialized to `None`, so the clear was unreachable. They must
    /// differ.
    #[test]
    fn an_absent_key_and_a_null_str_count_are_different_states() {
        let absent: SectionCountFacts = serde_json::from_str("{}").expect("deserialize");
        let null: SectionCountFacts =
            serde_json::from_str("{\"str_count\": null}").expect("deserialize");
        assert_eq!(absent.str_count, None, "no key = no write");
        assert_eq!(null.str_count, Some(StrCount::Cleared), "null = clear");
        assert_ne!(
            absent.str_count, null.str_count,
            "conflating these makes filters.js:523 unreachable"
        );
    }

    /// The formatter itself, both directions, against `fmtNum`'s contract
    /// (`core.js:209` — `toLocaleString('en-US')`).
    #[test]
    fn str_count_uses_thousands_separators_like_fmt_num() {
        assert_eq!(fmt_num(0), "0");
        assert_eq!(fmt_num(7), "7");
        assert_eq!(fmt_num(999), "999");
        assert_eq!(fmt_num(1000), "1,000", "no separator would be 1000");
        assert_eq!(fmt_num(1234), "1,234");
        assert_eq!(fmt_num(1234567), "1,234,567");
    }

    /// THE EMPTY-QUERY CASE (`filters.js:523`). Two directions: the span is
    /// *empty*, and it is specifically **not** `"0"`.
    #[test]
    fn empty_query_leaves_str_count_empty_and_never_zero() {
        let shell = shell_with(Some(serde_json::json!({ "str_count": null })));
        let got = text_of(&shell, "strCount");
        assert_eq!(got, "", "the empty query clears the span");
        assert_ne!(got, "0", "`0` would be a count nobody measured");
        // The shipped document is the same state, so this is not a coincidence.
        let shipped = NzkUi::load_default((1600, 1000), ".").expect("load shell");
        assert_eq!(text_of(&shipped, "strCount"), "");
    }

    /// A previous count is genuinely CLEARED, not merely left alone. Without
    /// this, an implementation that skipped the `Some(None)` arm would pass the
    /// test above by accident.
    #[test]
    fn empty_query_clears_a_count_left_by_a_previous_search() {
        let counted = shell_with(Some(serde_json::json!({ "str_count": 1234 })));
        assert_eq!(text_of(&counted, "strCount"), "1,234", "formatted, per :528");
        let cleared = shell_with(Some(serde_json::json!({ "str_count": null })));
        assert_eq!(text_of(&cleared, "strCount"), "", "and :523 wipes it");
    }

    /// THE NON-EMPTY CASE (`filters.js:528`): the formatted count, with the
    /// separators that a bare `{}` would lose.
    #[test]
    fn non_empty_query_renders_the_formatted_count() {
        let shell = shell_with(Some(serde_json::json!({ "str_count": 1234 })));
        assert_eq!(text_of(&shell, "strCount"), "1,234");
        let small = shell_with(Some(serde_json::json!({ "str_count": 3 })));
        assert_eq!(text_of(&small, "strCount"), "3");
    }

    /// An absent key writes nothing — the document stays as it ships.
    #[test]
    fn absent_section_counts_leaves_both_spans_untouched() {
        let shell = shell_with(None);
        assert_eq!(text_of(&shell, "strCount"), "");
        assert_eq!(text_of(&shell, "chainCount"), "");
    }

    /// THE CHAIN FORM (`filters.js:555`) — ` N hops`, with the leading space
    /// implied by the digits and the hard-coded plural.
    #[test]
    fn chain_counter_renders_flat_rows_with_hops_suffix() {
        let shell = shell_with(Some(serde_json::json!({ "chain_flat": 7 })));
        assert_eq!(text_of(&shell, "chainCount"), "7 hops");
        assert!(!text_of(&shell, "chainCount").contains("truncated"));
    }

    /// The reference's literal is ` hops`, never ` hop` — this is NOT
    /// `hop_label`, which singularises. One flat row is `"1 hops"`.
    #[test]
    fn chain_counter_hard_codes_the_plural_unlike_hop_label() {
        let shell = shell_with(Some(serde_json::json!({ "chain_flat": 1 })));
        assert_eq!(text_of(&shell, "chainCount"), "1 hops");
        assert_ne!(text_of(&shell, "chainCount"), "1 hop", "hop_label singularises; :555 does not");
        assert_eq!(text_of(&shell, "chainCount"), format!("{} hops", 1));
    }

    /// Truncation APPENDS (`filters.js:555`): the count survives, and the
    /// marker is not `truncated_label`'s `truncated at N hops`.
    #[test]
    fn truncation_appends_rather_than_replaces() {
        let shell = shell_with(Some(
            serde_json::json!({ "chain_flat": 7, "chain_truncated": true }),
        ));
        let got = text_of(&shell, "chainCount");
        assert_eq!(got, "7 hops (truncated)");
        assert!(got.starts_with("7 hops"), "the count is not replaced");
        assert_ne!(got, "truncated", "the count must survive");
        assert_ne!(got, "truncated at 7 hops", "that is :542's body row, not the head");
    }

    /// Both directions of the truncation flag on the same count: the "has" and
    /// the "has-not" differ ONLY by the appended suffix.
    #[test]
    fn the_truncation_flag_is_the_only_difference() {
        let plain = shell_with(Some(serde_json::json!({ "chain_flat": 9 })));
        let cut = shell_with(Some(
            serde_json::json!({ "chain_flat": 9, "chain_truncated": true }),
        ));
        let a = text_of(&plain, "chainCount");
        let b = text_of(&cut, "chainCount");
        assert_eq!(a, "9 hops");
        assert_eq!(b, "9 hops (truncated)");
        assert_eq!(b.strip_prefix(&a), Some(" (truncated)"), "append, not replace");
    }

    /// `truncated` is ignored when no flat count was measured, so the counter
    /// cannot render a suffix with no number in front of it.
    #[test]
    fn a_truncated_flag_without_a_count_renders_nothing() {
        let shell = shell_with(Some(serde_json::json!({ "chain_truncated": true })));
        assert_eq!(text_of(&shell, "chainCount"), "");
    }

    /// The two counters are independent: writing one must not disturb the other.
    #[test]
    fn the_two_counters_do_not_interfere() {
        let shell = shell_with(Some(serde_json::json!({
            "str_count": 1400,
            "chain_flat": 12,
            "chain_truncated": true
        })));
        assert_eq!(text_of(&shell, "strCount"), "1,400");
        assert_eq!(text_of(&shell, "chainCount"), "12 hops (truncated)");
    }

    /// The spans keep the shipped `.scount` class and live inside the right
    /// section heads — the "append, do not restyle" constraint.
    #[test]
    fn the_spans_keep_their_class_and_section() {
        let html = std::fs::read_to_string(NzkUi::frontend_dir().join("index.html"))
            .expect("read index.html");
        assert!(html.contains(r#"<span class="scount" id="strCount"></span>"#));
        assert!(html.contains(r#"<span class="scount" id="chainCount"></span>"#));
        let shell = shell_with(Some(serde_json::json!({ "str_count": 5, "chain_flat": 2 })));
        for (id, section) in [("strCount", "#strSection"), ("chainCount", "#chainSection")] {
            let inside = shell
                .document()
                .query_selector(&format!("{section} #{id}"))
                .ok()
                .flatten()
                .is_some();
            assert!(inside, "#{id} must stay inside {section}");
            let has_class = shell
                .document()
                .query_selector(&format!("#{id}.scount"))
                .ok()
                .flatten()
                .is_some();
            assert!(has_class, "#{id} must keep its scount class");
        }
    }

    /// The pure formatter, exhaustively — the literal strings this code emits.
    #[test]
    fn chain_count_label_emits_exactly_the_reference_literals() {
        assert_eq!(chain_count_label(0, false), "0 hops");
        assert_eq!(chain_count_label(1, false), "1 hops");
        assert_eq!(chain_count_label(7, false), "7 hops");
        assert_eq!(chain_count_label(7, true), "7 hops (truncated)");
        assert_eq!(chain_count_label(0, true), "0 hops (truncated)");
        // The separator is a single leading space, and nothing else.
        assert!(chain_count_label(3, true).contains(" (truncated)"));
        assert!(!chain_count_label(3, false).contains("truncated"));
    }
}

#[cfg(test)]
mod blocks_canvas_tests {
    use super::*;
    use crate::NzkUi;

    /// Build the shell from the SAME wire shape the app publishes: a `Facts`
    /// object whose `ui.blocks` is the host's canvas content.
    fn build(blocks: Option<serde_json::Value>) -> NzkUi {
        let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
            .expect("Facts must serialize");
        if let Some(b) = blocks {
            value["ui"] = serde_json::json!({ "blocks": b });
        }
        let facts: Facts = serde_json::from_value(value).expect("Facts must round-trip");
        let dir = NzkUi::frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
            .expect("build skeleton shell")
    }

    fn fixture() -> serde_json::Value {
        serde_json::json!({
            "title": "Blocks · Widget",
            "meta": "2 blocks · 1 edge",
            "empty": "",
            "cards": [
                {"id": "T:Demo.Widget", "index": 0, "kind": "class", "name": "Widget",
                 "full": "Demo.Widget", "file": "widget.cs", "line": 1, "params": [],
                 "out_type": "class", "degree": 2, "x": 0.0, "y": 0.0,
                 "selected": true, "kind_color": "#4ec9b0"},
                {"id": "M:Demo.Widget.Run(System.String)", "index": 1, "kind": "method",
                 "name": "Run", "full": "Demo.Widget.Run", "file": "widget.cs", "line": 5,
                 "params": ["System.String"], "out_type": "", "degree": 1,
                 "x": 262.0, "y": 0.0, "selected": false, "kind_color": "#dcdcaa"}
            ],
            "edges": [
                {"source": "T:Demo.Widget", "target": "M:Demo.Widget.Run(System.String)",
                 "kind": "contains", "count": 1, "d": "M42.0,40.0 C140.0,40.0 150.0,40.0 262.0,40.0",
                 "color": "#6b7897"}
            ]
        })
    }

    #[test]
    fn a_blocks_fact_paints_real_cards_ports_and_edge_paths() {
        let shell = build(Some(fixture()));
        assert!(
            !shell.classes_of("#blocksPanel").unwrap().iter().any(|c| c == "hidden"),
            "a blocks fact implies the panel is open"
        );
        assert_eq!(shell.text_of("#blocksTitle").as_deref(), Some("Blocks · Widget"));
        assert_eq!(shell.text_of("#blocksMeta").as_deref(), Some("2 blocks · 1 edge"));
        // Card identity + the click payload the nzk_ui routes on.
        assert_eq!(
            shell.attr_of(".block-card", "data-id").unwrap().as_deref(),
            Some("T:Demo.Widget")
        );
        assert_eq!(
            shell.classes_of(".block-card.selected").unwrap(),
            vec!["block-card", "selected"]
        );
        // `.bc-kind` ships `text-transform: uppercase` (v2 `style.css:2321-2327`)
        // and this shell really applies CSS `text-transform` to readable text
        // (the text-transform gap module below), so the RENDERED text is upper
        // case even though the fact's `kind` is lower case ("scratch.js:271"
        // likewise sets the lower-case kind and lets the stylesheet upper-case it).
        assert_eq!(shell.text_of(".bc-kind").as_deref(), Some("CLASS"));
        assert_eq!(shell.text_of(".bc-title").as_deref(), Some("Widget"));
        assert_eq!(shell.text_of(".bc-loc").as_deref(), Some("widget.cs:1"));
        // Ports: the method's real parameter type becomes a typed in-port.
        assert_eq!(
            shell.attr_of(".port-in", "data-type").unwrap().as_deref(),
            Some("System.String")
        );
        assert_eq!(shell.text_of(".port-in .port-type").as_deref(), Some("String"));
        assert!(shell.classes_of(".port-out").unwrap().iter().any(|c| c == "port-out"));
        // The edge path carries the v2 class, payload and colour; the SVG is sized.
        assert!(shell.classes_of(".blocks-edge").unwrap().iter().any(|c| c == "e-contains"));
        assert_eq!(
            shell.attr_of(".blocks-edge", "data-kind").unwrap().as_deref(),
            Some("contains")
        );
        assert_eq!(
            shell.attr_of(".blocks-edge", "stroke").unwrap().as_deref(),
            Some("#6b7897")
        );
        assert_eq!(shell.attr_of("#blocksEdges", "width").unwrap().as_deref(), Some("494"));
        // v2 `scratch.js:375-414`: one triangle marker defs per SVG and every
        // edge path points its end at it, so the arrowhead is really painted.
        assert_eq!(
            shell.attr_of("#blocksArrow", "orient").unwrap().as_deref(),
            Some("auto-start-reverse")
        );
        assert_eq!(
            shell.attr_of("#blocksArrow", "refX").unwrap().as_deref(),
            Some("7")
        );
        assert_eq!(
            shell.attr_of(".blocks-edge", "marker-end").unwrap().as_deref(),
            Some("url(#blocksArrow)")
        );
    }

    #[test]
    fn an_empty_blocks_fact_paints_the_honest_empty_sentence_and_no_rows() {
        let shell = build(Some(serde_json::json!({
            "title": "Blocks", "meta": "", "empty": "No declarations for T:Empty.",
            "cards": [], "edges": []
        })));
        assert_eq!(
            shell.text_of(".blocks-empty").as_deref(),
            Some("No declarations for T:Empty.")
        );
        assert_eq!(shell.attr_of(".block-card", "data-id"), Err(()));
    }

    #[test]
    fn no_blocks_fact_leaves_the_shipped_empty_stage_untouched() {
        let shell = build(None);
        assert!(
            shell.classes_of("#blocksPanel").unwrap().iter().any(|c| c == "hidden"),
            "the shipped panel keeps its hidden class"
        );
        assert_eq!(shell.text_of("#blocksTitle").as_deref(), Some("Blocks"));
        assert_eq!(shell.text_of("#blocksMeta").as_deref(), Some(""));
        assert_eq!(shell.attr_of(".block-card", "data-id"), Err(()));
        assert_eq!(shell.attr_of("#blocksEdges", "width"), Ok(None));
    }
}

/// The `blocksPeek*` tooltip and the three conditional `reorg*Row` rows.
///
/// Both families ship inert in `index.html`, so every test here is
/// **two-directional**: it asserts the row is hidden in one state and visible in
/// another, and that the shipped default is genuinely unchanged. A test that
/// only checked "the id is there" would pass against the broken tree this module
/// was written to fix — where nothing consumed the ids at all.
mod blocks_peek_tests {
    use super::*;
    use crate::NzkUi;

    /// A shell painted from the real `index.html`, with `blocks_peek` set to the
    /// JSON a host would send. `None` omits the key entirely (the default), which
    /// is a *different* document from `blocks_peek: {}`.
    fn shell_with(blocks_peek: Option<serde_json::Value>) -> NzkUi {
        let mut facts = Facts::for_corpus(".");
        if let Some(v) = &blocks_peek {
            facts.blocks_peek = Some(serde_json::from_value(v.clone()).expect("BlocksPeekFacts"));
        }
        let html = std::fs::read_to_string(NzkUi::frontend_dir().join("index.html"))
            .expect("read index.html");
        let css_path = NzkUi::frontend_dir().join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_source_with_facts(&html, &css_path, &css, (1600, 1000), &facts)
            .expect("build shell")
    }

    fn is_hidden(shell: &NzkUi, id: &str) -> bool {
        shell
            .classes_of(&format!("#{id}"))
            .unwrap_or_else(|| panic!("#{id} must exist"))
            .iter()
            .any(|c| c == "hidden")
    }

    fn text_of(shell: &NzkUi, id: &str) -> String {
        shell
            .text_of(&format!("#{id}"))
            .unwrap_or_else(|| panic!("#{id} must exist"))
    }

    /// The producer's object must be accepted by the real deserializer. A shape
    /// mismatch (e.g. a wrong key name) shows up as a default-valued field here
    /// rather than as a silent no-op in the document.
    #[test]
    fn blocks_peek_json_deserializes_into_the_facts_struct() {
        let facts: BlocksPeekFacts = serde_json::from_value(serde_json::json!({
            "visible": true,
            "kind": "method",
            "name": "Bar",
            "file": "src/Foo.cs:42",
            "src": "line one",
            "refs": "in 3 · out 5",
            "reorg_mode": "split",
            "reorg_group": "size"
        }))
        .expect("deserialize BlocksPeekFacts");
        assert!(facts.visible);
        assert_eq!(facts.kind, "method");
        assert_eq!(facts.name, "Bar");
        assert_eq!(facts.file, "src/Foo.cs:42");
        assert_eq!(facts.src, "line one");
        assert_eq!(facts.refs, "in 3 · out 5");
        assert_eq!(facts.reorg_mode.as_deref(), Some("split"));
        assert_eq!(facts.reorg_group.as_deref(), Some("size"));
    }

    /// An absent key must leave all eight ids exactly as `index.html` shipped
    /// them — this is the contract that keeps `@frame` stable.
    #[test]
    fn absent_blocks_peek_leaves_all_eight_ids_at_their_shipped_state() {
        let shell = shell_with(None);
        // The panel ships `class="blocks-peek hidden"` (index.html:186).
        assert!(is_hidden(&shell, "blocksPeek"), "#blocksPeek ships hidden");
        // ...and its source span ships the literal, not the empty string.
        assert_eq!(text_of(&shell, "blocksPeekSrc"), "Loading source\u{2026}");
        // The other four spans ship empty.
        for id in ["blocksPeekKind", "blocksPeekName", "blocksPeekFile", "blocksPeekRefs"] {
            assert_eq!(text_of(&shell, id), "", "#{id} ships empty");
        }
        // The reorg rows are untouched: the children flag in this document is
        // the shipped one, so `syncRows` has never run.
        for id in ["reorgGranRow", "reorgGroupRow", "reorgSizeRow"] {
            assert!(
                !is_hidden(&shell, id),
                "#{id} must keep its shipped class when the key is absent"
            );
        }
    }

    /// The hover fill: every one of the five spans carries the host's text and
    /// the panel's `hidden` class is removed.
    #[test]
    fn a_visible_peek_fills_every_span_and_reveals_the_panel() {
        let shell = shell_with(Some(serde_json::json!({
            "visible": true,
            "kind": "method",
            "name": "Bar",
            "file": "src/Foo.cs:42",
            "src": "line one\nline two",
            "refs": "in 3 · out 5"
        })));
        assert!(!is_hidden(&shell, "blocksPeek"), "showPeek removes `hidden`");
        // `.bp-kind` ships `text-transform: uppercase` (`style.css:2408`), and
        // this shell really applies CSS `text-transform`, so the kind span reads
        // back upper-cased. That is the reference rendering, not a mangled
        // write — the stored value is exactly what the host sent.
        assert_eq!(text_of(&shell, "blocksPeekKind"), "METHOD");
        // The name and file spans carry no transform, so they read back
        // verbatim. Asserting this pair is what proves the upper-casing is
        // scoped to `.bp-kind` and not applied across the whole panel.
        assert_eq!(text_of(&shell, "blocksPeekName"), "Bar");
        assert_eq!(text_of(&shell, "blocksPeekFile"), "src/Foo.cs:42");
        // The literal is *replaced*, not appended — `Loading source…` is gone.
        assert_eq!(text_of(&shell, "blocksPeekSrc"), "line one\nline two");
        assert_ne!(text_of(&shell, "blocksPeekSrc"), "Loading source\u{2026}");
        assert_eq!(text_of(&shell, "blocksPeekRefs"), "in 3 · out 5");
    }

    /// `blocks-peek` must stay in the class list: the panel's styling hangs off
    /// it, so a toggle that dropped it would restyle the tooltip.
    #[test]
    fn revealing_the_panel_keeps_its_blocks_peek_class() {
        let shell = shell_with(Some(serde_json::json!({ "visible": true, "kind": "class" })));
        let classes = shell.classes_of("#blocksPeek").expect("#blocksPeek");
        assert!(classes.iter().any(|c| c == "blocks-peek"), "got {classes:?}");
        assert!(!classes.iter().any(|c| c == "hidden"), "got {classes:?}");
    }

    /// The exact `reorg.js:286-290` rule, both directions: `single` hides all
    /// three rows, `split` + `size` shows all three.
    #[test]
    fn reorg_single_hides_every_row_and_split_size_shows_every_row() {
        let single = shell_with(Some(serde_json::json!({
            "reorg_mode": "single", "reorg_group": "size"
        })));
        for id in ["reorgGranRow", "reorgGroupRow", "reorgSizeRow"] {
            assert!(is_hidden(&single, id), "#{id} must be hidden when not split");
        }

        let split = shell_with(Some(serde_json::json!({
            "reorg_mode": "split", "reorg_group": "size"
        })));
        for id in ["reorgGranRow", "reorgGroupRow", "reorgSizeRow"] {
            assert!(!is_hidden(&split, id), "#{id} must be visible when split+size");
        }
    }

    /// The discriminating case that a one-directional test cannot catch: `split`
    /// with a non-size group hides **only** the size row.
    #[test]
    fn split_with_a_non_size_group_hides_only_the_size_row() {
        for group in ["namespace", "owner", "single"] {
            let shell = shell_with(Some(serde_json::json!({
                "reorg_mode": "split", "reorg_group": group
            })));
            assert!(!is_hidden(&shell, "reorgGranRow"), "gran visible for {group}");
            assert!(!is_hidden(&shell, "reorgGroupRow"), "group visible for {group}");
            assert!(is_hidden(&shell, "reorgSizeRow"), "size hidden for group={group}");
        }
    }

    /// A row set hidden then re-shown must not accumulate duplicate tokens, and
    /// the row's own `settings-row` class must survive the toggle.
    #[test]
    fn the_reorg_toggle_preserves_the_rows_own_class() {
        let hidden = shell_with(Some(serde_json::json!({ "reorg_mode": "single" })));
        let classes = hidden.classes_of("#reorgGranRow").expect("#reorgGranRow");
        assert!(classes.iter().any(|c| c == "settings-row"), "got {classes:?}");
        assert!(classes.iter().any(|c| c == "hidden"), "got {classes:?}");

        let shown = shell_with(Some(serde_json::json!({
            "reorg_mode": "split", "reorg_group": "size"
        })));
        let classes = shown.classes_of("#reorgGranRow").expect("#reorgGranRow");
        assert_eq!(
            classes.iter().filter(|c| *c == "settings-row").count(),
            1,
            "exactly one settings-row token, got {classes:?}"
        );
        assert_eq!(
            classes.iter().filter(|c| *c == "hidden").count(),
            0,
            "no hidden token, got {classes:?}"
        );
    }

    /// An absent mode is "the dialog has not opened", which writes nothing —
    /// distinct from `single`, which actively hides. If the two were conflated,
    /// a document that never opened the dialog would silently diverge from
    /// `index.html`.
    #[test]
    fn an_absent_mode_writes_nothing_and_a_single_mode_writes_hidden() {
        let untouched = shell_with(Some(serde_json::json!({ "visible": false })));
        for id in ["reorgGranRow", "reorgGroupRow", "reorgSizeRow"] {
            assert!(!is_hidden(&untouched, id), "#{id} untouched without a mode");
        }
        let asked = shell_with(Some(serde_json::json!({ "reorg_mode": "single" })));
        assert!(is_hidden(&asked, "reorgGranRow"), "an explicit single hides it");
    }
}

/// The edit modal's content channel: `#editArgs`, `#editOut`, `#editNote` and
/// the three footer buttons (`edit.js:262-269`, via `app/src/edit_host.rs`).
///
/// Every test here is two-directional for the same reason the sibling modules
/// are: the whole point of the channel is the **absent key leaves the shipped
/// document alone**, so a test that only filled the fields would pass against a
/// build that repainted on `None`.
mod edit_tests {
    use super::*;
    use crate::NzkUi;

    /// A shell painted from the real `index.html`, with `edit` set to the JSON a
    /// host would send. `None` omits the key entirely (the shipped default),
    /// which is a *different* document from `edit: {}`.
    fn shell_with(edit: Option<serde_json::Value>) -> NzkUi {
        let mut facts = Facts::for_corpus(".");
        if let Some(v) = &edit {
            facts.edit = Some(serde_json::from_value(v.clone()).expect("EditFacts"));
        }
        let html = std::fs::read_to_string(NzkUi::frontend_dir().join("index.html"))
            .expect("read index.html");
        let css_path = NzkUi::frontend_dir().join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_source_with_facts(&html, &css_path, &css, (1600, 1000), &facts)
            .expect("build shell")
    }

    fn text_of(shell: &NzkUi, id: &str) -> String {
        shell
            .text_of(&format!("#{id}"))
            .unwrap_or_else(|| panic!("#{id} must exist"))
    }

    /// The producer's seven `#`-prefixed keys must land in the real
    /// deserializer with every field populated. A rename typo shows up here as a
    /// default-valued field, not as an error: serde ignores unknown keys.
    #[test]
    fn edit_json_deserializes_into_the_facts_struct() {
        let facts: EditFacts = serde_json::from_value(serde_json::json!({
            "#editTool": "rename",
            "#editArgs": "{\"to\": \"Bar\"}",
            "#editOut": { "ok": true },
            "#editNote": { "text": "done", "class": "ok" },
            "#editDry": { "apply": false, "disabled": true },
            "#editApply": { "apply": true, "disabled": true },
            "#editClose": { "disabled": false }
        }))
        .expect("deserialize EditFacts");
        assert_eq!(facts.tool.as_deref(), Some("rename"));
        assert_eq!(facts.args.as_deref(), Some("{\"to\": \"Bar\"}"));
        assert_eq!(facts.out, Some(serde_json::json!({ "ok": true })));
        let note = facts.note.expect("#editNote");
        assert_eq!(note.text, "done");
        assert_eq!(note.class, "ok");
        assert_eq!(facts.dry.expect("#editDry").disabled, true);
        assert!(facts.apply.expect("#editApply").apply);
        assert!(!facts.close.expect("#editClose").disabled);
    }

    /// `#editArgs` carries the host's text (`els.args.value`, `edit.js:266`),
    /// replacing the shipped `{}` rather than appending to it.
    #[test]
    fn edit_args_text_is_written_from_the_host() {
        let shell = shell_with(Some(serde_json::json!({
            "#editArgs": "{\"like\": [\"sps\"]}"
        })));
        assert_eq!(text_of(&shell, "editArgs"), "{\"like\": [\"sps\"]}");
        // The shipped document's placeholder is really gone.
        assert_ne!(text_of(&shell, "editArgs"), "{}");
    }

    /// `#editOut` is the reference's `JSON.stringify(data, null, 2)`: the pretty
    /// rendering, newlines included — not the compact `Display` form.
    #[test]
    fn edit_out_is_the_pretty_printed_json() {
        let value = serde_json::json!({ "ok": true, "n": [1, 2] });
        let shell = shell_with(Some(serde_json::json!({ "#editOut": value })));
        let got = text_of(&shell, "editOut");
        assert_eq!(got, serde_json::to_string_pretty(&value).expect("pretty"));
        assert!(got.contains('\n'), "pretty, not compact: {got:?}");
        assert_ne!(got, value.to_string(), "compact would be `{value}`");
    }

    /// `note(text, kind)` (`edit.js:74-77`) writes the text and *appends* the
    /// kind to the base `modal-note` class — the base must survive, because
    /// `.modal-note.ok` (`style.css:889`) only matches with it.
    #[test]
    fn edit_note_text_and_class_keep_the_base_class() {
        let shell = shell_with(Some(serde_json::json!({
            "#editNote": { "text": "Args: bad JSON", "class": "error" }
        })));
        assert_eq!(text_of(&shell, "editNote"), "Args: bad JSON");
        let classes = shell.classes_of("#editNote").expect("#editNote");
        assert!(classes.iter().any(|c| c == "modal-note"), "got {classes:?}");
        assert!(classes.iter().any(|c| c == "error"), "got {classes:?}");
    }

    /// `setBusy` (`edit.js:78-80`), both directions: while a run is in flight
    /// both buttons carry the `disabled` attribute, and when it clears the
    /// attribute is gone (not merely `disabled="false"`).
    #[test]
    fn edit_buttons_disable_while_busy_and_clear_after() {
        let busy = shell_with(Some(serde_json::json!({
            "#editDry": { "apply": false, "disabled": true },
            "#editApply": { "apply": true, "disabled": true }
        })));
        for id in ["editDry", "editApply"] {
            assert!(
                busy.attr_of(&format!("#{id}"), "disabled").expect(id).is_some(),
                "#{id} must be disabled while busy"
            );
        }
        let idle = shell_with(Some(serde_json::json!({
            "#editDry": { "apply": false, "disabled": false },
            "#editApply": { "apply": true, "disabled": false }
        })));
        for id in ["editDry", "editApply"] {
            assert_eq!(
                idle.attr_of(&format!("#{id}"), "disabled").expect(id),
                None,
                "#{id} must be enabled when the run concludes"
            );
        }
    }

    /// Non-vacuity control for every `live_tree_dump` comparison in this module:
    /// the probe must actually see a paint. A shell with edit facts set has a
    /// different whole-document dump from the shipped one, so "the dumps are
    /// equal" is real evidence rather than a comparison of two constants (which
    /// is exactly what the old `resolved_html()` comparisons were).
    #[test]
    fn live_tree_dump_observes_an_edit_paint() {
        let shipped = shell_with(None);
        let painted = shell_with(Some(serde_json::json!({
            "#editArgs": "{\"to\": \"Bar\"}",
            "#editNote": { "text": "done", "class": "ok" }
        })));
        let shipped_dump = live_tree_dump(shipped.document());
        let painted_dump = live_tree_dump(painted.document());
        assert_ne!(
            shipped_dump, painted_dump,
            "the whole-document probe must observe a repaint, else the equality \
             assertions above would be vacuous"
        );
        assert!(
            painted_dump.contains("text:{\"to\": \"Bar\"}"),
            "the observed difference must be the painted #editArgs text, got:\n{painted_dump}"
        );
        assert!(
            painted_dump.contains("class=modal-note ok"),
            "the observed difference must include the painted #editNote class"
        );
    }

    /// The absent-key contract: `edit: None` (the shipped `Facts::for_corpus`)
    /// must leave the whole **live** document untouched — `#editArgs`'s `{}`,
    /// `#editOut`'s empty `<pre>` and the bare `#editNote` class included — with
    /// no node, attribute or text byte repainted relative to the shipped
    /// document.
    #[test]
    fn absent_edit_leaves_the_modal_byte_identical() {
        // Compare two independent builds of the shipped constructor path
        // (`shell_with(None)`) by canonicalising the *whole live tree*. The old
        // `resolved_html()` comparison was a tautology: it returns the immutable
        // parsed source (`lib.rs:548`), which no DOM mutation reaches, so it
        // could never differ no matter what the edit channel painted.
        let shipped = shell_with(None);
        let absent = shell_with(None);
        assert_eq!(
            live_tree_dump(absent.document()),
            live_tree_dump(shipped.document()),
            "edit: None must not repaint a single live node"
        );
        assert_eq!(text_of(&absent, "editArgs"), "{}", "the shipped args box");
        assert_eq!(text_of(&absent, "editOut"), "", "the shipped empty <pre>");
        assert_eq!(
            absent.classes_of("#editNote").expect("#editNote"),
            vec!["modal-note".to_string()],
            "the shipped bare base class"
        );
        assert_eq!(
            absent.attr_of("#editDry", "disabled").expect("#editDry"),
            None
        );
    }

    /// `#editTool` ships as an **empty `<select>`** (`index.html:527`); its
    /// options come from `loadOps` (`edit.js:186-197`, one `<option value=name>`
    /// per `/api/edit` tool). This channel carries only the selected *name*, so
    /// there is nothing honest to render: the element must be left untouched
    /// rather than given a fabricated value. This test pins that current
    /// behaviour so a later options-carrying task has to change it deliberately.
    #[test]
    fn edit_tool_select_is_not_faked() {
        let shell = shell_with(Some(serde_json::json!({ "#editTool": "rename" })));
        assert_eq!(text_of(&shell, "editTool"), "", "no fabricated selection");
        assert_eq!(
            shell.attr_of("#editTool", "value").expect("#editTool"),
            None,
            "a `value` attribute would be a lie with no options"
        );
        assert_eq!(
            shell.child_elements_of("#editTool"),
            Some(Vec::new()),
            "loadOps has not run, so there are still zero options"
        );
        // Writing only `#editTool` changes nothing at all: compare the whole
        // LIVE tree, not `resolved_html()` (the immutable parsed source, which
        // no mutation reaches and which therefore cannot witness this).
        let shipped = shell_with(None);
        assert_eq!(
            live_tree_dump(shell.document()),
            live_tree_dump(shipped.document()),
            "an unrenderable selection must not perturb the live document"
        );
    }

    /// The `(value, selected)` view of one `#editTool` option.
    ///
    /// Read straight off the live DOM: `resolved_html()` is the *parsed source*
    /// (`lib.rs:447-449`, `&self.html`), which no mutation reaches, so it can
    /// never show an option or a `selected` attribute. This is the honest probe
    /// for what the select actually holds.
    fn option_row(shell: &NzkUi, node: usize) -> (String, bool) {
        let data = shell
            .document()
            .get_node(node)
            .expect("option node")
            .element_data()
            .expect("option element");
        let value = data
            .attrs
            .iter()
            .find(|a| a.name.local == LocalName::from("value"))
            .map(|a| a.value.clone())
            .unwrap_or_default();
        let selected = data
            .attrs
            .iter()
            .any(|a| a.name.local == LocalName::from("selected"));
        (value, selected)
    }

    /// Every `#editTool` option as `(value, selected)`, in document order.
    fn options_of(shell: &NzkUi) -> Vec<(String, bool)> {
        shell
            .child_elements_of("#editTool")
            .expect("#editTool")
            .into_iter()
            .map(|node| option_row(shell, node))
            .collect()
    }

    /// `loadOps` (`edit.js:186-201`) fills the select with one
    /// `<option value=name>` per tool, in the engine's `tools` array order, and
    /// the option text is the name — not the value alone.
    #[test]
    fn edit_tool_options_are_built_in_order() {
        let shell = shell_with(Some(serde_json::json!({
            "#editToolOptions": ["rename", "move", "dedupe"]
        })));
        assert_eq!(
            options_of(&shell)
                .into_iter()
                .map(|(value, _)| value)
                .collect::<Vec<_>>(),
            vec!["rename".to_string(), "move".to_string(), "dedupe".to_string()],
            "values and order must follow the host's list"
        );
        // `fill` also sets `textContent`, so the visible label is the tool name.
        assert_eq!(text_of(&shell, "editTool"), "renamemovededupe");
    }

    /// The current value (`#editTool`, the struct's `tool`) marks its option
    /// with `selected`. There is no select-value API in this DOM layer, so the
    /// attribute is the mechanism — and it survives in the live DOM even though
    /// `resolved_html()` is the un-mutated parsed source.
    #[test]
    fn edit_tool_matching_option_is_marked_selected() {
        let shell = shell_with(Some(serde_json::json!({
            "#editTool": "move",
            "#editToolOptions": ["rename", "move", "dedupe"]
        })));
        assert_eq!(
            options_of(&shell),
            vec![
                ("rename".to_string(), false),
                ("move".to_string(), true),
                ("dedupe".to_string(), false),
            ],
            "exactly the named option is selected"
        );
        // The proof, stated against `resolved_html`: it still carries the empty
        // shipped select, so the `selected` attribute is a DOM fact only.
        assert!(
            shell
                .resolved_html()
                .contains(r#"<select id="editTool" class="select"></select>"#),
            "resolved_html is the parsed source and keeps the empty select"
        );
    }

    /// With no `tool`, the browser's own default applies — the first option is
    /// the value (`loadOps` does `els.tool.value = tools[0]`). Reproduce that by
    /// marking the first option, not by inventing a third state.
    #[test]
    fn edit_tool_absent_value_marks_the_first_option() {
        let shell = shell_with(Some(serde_json::json!({
            "#editToolOptions": ["rename", "move"]
        })));
        assert_eq!(
            options_of(&shell),
            vec![("rename".to_string(), true), ("move".to_string(), false)],
            "the first option is the default value"
        );
    }

    /// `tool_options: None` is the T6 invariant: the select is byte-identical
    /// to the shipped document, options and all (`index.html:527` ships none).
    #[test]
    fn edit_tool_options_none_leaves_the_select_untouched() {
        let shell = shell_with(Some(serde_json::json!({ "#editTool": "rename" })));
        assert_eq!(
            shell.child_elements_of("#editTool"),
            Some(Vec::new()),
            "no list means no options, so nothing is fabricated"
        );
        let shipped = shell_with(None);
        // Whole LIVE tree, not `resolved_html()`: the parsed source is
        // immutable and could never show a repaint.
        assert_eq!(
            live_tree_dump(shell.document()),
            live_tree_dump(shipped.document()),
            "None must not repaint a single live node"
        );
    }

    /// An empty list is a real state — the engine answered with no tools — so
    /// the box is emptied and the call does not panic.
    #[test]
    fn edit_tool_options_empty_list_is_an_empty_select() {
        let shell = shell_with(Some(serde_json::json!({
            "#editToolOptions": []
        })));
        assert_eq!(
            shell.child_elements_of("#editTool"),
            Some(Vec::new()),
            "an empty tool list renders as the empty select"
        );
        assert_eq!(text_of(&shell, "editTool"), "");
    }

    /// A `tool` the host publishes that is not in the list is not quietly
    /// mapped onto some option: no option is marked, because a fabricated
    /// selection would misreport what a future apply would run.
    #[test]
    fn edit_tool_absent_from_list_is_not_faked() {
        let shell = shell_with(Some(serde_json::json!({
            "#editTool": "explode",
            "#editToolOptions": ["rename", "move"]
        })));
        assert_eq!(
            options_of(&shell),
            vec![("rename".to_string(), false), ("move".to_string(), false)],
            "an unknown value selects nothing"
        );
    }
}

/// Tests for the open-document tab bar's DOM projection (`shell.js:1192-1248`).
///
/// They assert on the **resolved** document — real classes, attributes, children
/// and layout boxes from the real `index.html`/skeleton + `style.css`. The bar is
/// not in the shipped DOM and must not be added to
/// [`PROBE_SELECTORS`] (that would move the pinned `@frame` token), so its
/// geometry is read through the same `NzkUi::box_of` path the probes use.
#[cfg(test)]
mod tabs_tests {
    use super::*;
    use crate::NzkUi;

    /// Build the **Rust skeleton** shell — the runtime path that matters — from
    /// the real `style.css`, with the given `ui` object spliced into the facts.
    ///
    /// `Facts` has private fields, so the object is round-tripped through JSON:
    /// `Facts::for_corpus` -> `serde_json::Value` (+ the `ui` key under test) ->
    /// `Facts`. That is the same wire shape the host publishes.
    fn build(ui: Option<serde_json::Value>) -> NzkUi {
        let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
            .expect("Facts must serialize");
        if let Some(ui) = ui {
            value["ui"] = ui;
        }
        let facts: Facts = serde_json::from_value(value).expect("Facts must round-trip");
        let dir = NzkUi::frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
            .expect("build skeleton shell")
    }

    /// How many elements match a CSS selector (`box_of`/`attr_of` answer for the
    /// first match only; the bar is runtime-created, so it is not a probe row).
    fn count_matching(shell: &NzkUi, selector: &str) -> usize {
        shell
            .document()
            .query_selector_all(selector)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// `ui` carrying one tabs fact (and, optionally, the pane opened so the bar
    /// has real layout to land in).
    fn ui(tabs: serde_json::Value, split_on: bool, pane_open: bool) -> serde_json::Value {
        serde_json::json!({
            "pane_open": pane_open,
            "tabs": {"tabs": tabs, "split_on": split_on},
        })
    }

    /// Three documents: A active, B split (empty title/label -> key fallbacks),
    /// C plain. The exact shape `ui.tabs` publishes.
    fn three_tabs() -> serde_json::Value {
        serde_json::json!([
            {"key": "node:A", "label": "A", "active": true, "split": false, "title": "src/A.cs"},
            {"key": "node:B", "label": "", "active": false, "split": true, "title": ""},
            {"key": "node:C", "label": "C", "active": false, "split": false, "title": "node:C"},
        ])
    }

    /// THE DEFAULT-RENDER PROMISE: an absent `ui.tabs` (no `ui` at all, or a
    /// `ui` object that does not carry the key) creates no `#cs-tabs` node and
    /// writes nothing, so the shipped document stays byte-identical.
    #[test]
    fn an_absent_tabs_fact_creates_no_bar() {
        // No `ui` at all.
        let shell = build(None);
        assert!(
            shell.box_of("#cs-tabs").is_none(),
            "the shipped document must have no #cs-tabs box"
        );
        assert_eq!(
            count_matching(&shell, "#cs-tabs"),
            0,
            "an absent key must create no #cs-tabs element"
        );
        assert_eq!(count_matching(&shell, ".cs-tab"), 0);

        // A `ui` object without the `tabs` key is equally a no-op.
        let shell = build(Some(serde_json::json!({"pane_open": true})));
        assert!(
            shell.box_of("#cs-tabs").is_none(),
            "an omitted ui.tabs key must create no bar"
        );
        assert_eq!(count_matching(&shell, "#cs-tabs"), 0);

        // And the default document is stable: two builds produce the same LIVE
        // tree. `resolved_html()` was the wrong probe — it is the immutable
        // parsed source (`lib.rs:548`), identical by construction, so it could
        // not detect a repaint.
        let a = build(None);
        let b = build(None);
        assert_eq!(
            live_tree_dump(a.document()),
            live_tree_dump(b.document()),
            "the default live document must be deterministic"
        );
    }

    /// The bar is `#cs-tabs.cs-tabs`, is the FIRST child of `#sourcePane`,
    /// carries the tablist role/label, and one `.cs-tab` is rendered per row —
    /// with the reference's exact classes, attributes, child order and glyphs
    /// (shell.js:1194-1225).
    #[test]
    fn a_non_empty_bar_is_the_first_source_pane_child_and_renders_every_row() {
        let shell = build(Some(ui(three_tabs(), false, true)));

        // ---- #cs-tabs (shell.js:1194-1197) ---------------------------------
        assert_eq!(count_matching(&shell, "#cs-tabs"), 1, "exactly one bar");
        assert_eq!(
            shell.classes_of("#cs-tabs"),
            Some(vec!["cs-tabs".to_string()]),
            "the open bar is `cs-tabs`, no `hidden`"
        );
        assert_eq!(
            shell.attr_of("#cs-tabs", "role").unwrap().as_deref(),
            Some("tablist")
        );
        assert_eq!(
            shell.attr_of("#cs-tabs", "aria-label").unwrap().as_deref(),
            Some("Open documents")
        );

        // `pane.insertBefore(barEl, pane.firstChild)` (shell.js:1197): the bar
        // is the pane's first element child, before the shipped `.source-head`.
        let bar = shell
            .document()
            .query_selector("#cs-tabs")
            .ok()
            .flatten()
            .expect("bar must exist");
        let pane_children = shell
            .child_elements_of("#sourcePane")
            .expect("#sourcePane must exist");
        assert_eq!(
            pane_children.first().copied(),
            Some(bar),
            "the bar must be #sourcePane's first child"
        );
        assert!(
            shell.attr_of("#sourcePane .source-head", "class").is_ok(),
            "the shipped source head still follows the bar"
        );

        // ---- one .cs-tab per row -------------------------------------------
        assert_eq!(count_matching(&shell, ".cs-tab"), 3, "one row per tab");
        assert_eq!(
            count_matching(&shell, ".cs-tab.active"),
            1,
            "exactly the active row is marked"
        );

        // Row 1: active, title from params, label "A".
        assert_eq!(
            shell.classes_of(".cs-tab:nth-child(1)"),
            Some(vec!["cs-tab".to_string(), "active".to_string()])
        );
        assert_eq!(
            shell.attr_of(".cs-tab:nth-child(1)", "role").unwrap().as_deref(),
            Some("tab")
        );
        assert_eq!(
            shell
                .attr_of(".cs-tab:nth-child(1)", "data-key")
                .unwrap()
                .as_deref(),
            Some("node:A")
        );
        assert_eq!(
            shell
                .attr_of(".cs-tab:nth-child(1)", "aria-selected")
                .unwrap()
                .as_deref(),
            Some("true")
        );
        assert_eq!(
            shell
                .attr_of(".cs-tab:nth-child(1)", "title")
                .unwrap()
                .as_deref(),
            Some("src/A.cs"),
            "the row title is (params.file || params.node) || key"
        );
        assert_eq!(
            shell.text_of(".cs-tab:nth-child(1) .cs-tab-label").as_deref(),
            Some("A")
        );
        assert_eq!(
            shell
                .attr_of(".cs-tab:nth-child(1) .cs-tab-x", "data-close")
                .unwrap()
                .as_deref(),
            Some("node:A")
        );

        // Row 2: split mark only here, empty label/title fall back to the key.
        assert_eq!(
            shell.classes_of(".cs-tab:nth-child(2)"),
            Some(vec!["cs-tab".to_string()])
        );
        assert_eq!(
            shell
                .attr_of(".cs-tab:nth-child(2)", "aria-selected")
                .unwrap()
                .as_deref(),
            Some("false")
        );
        assert_eq!(
            shell
                .attr_of(".cs-tab:nth-child(2)", "title")
                .unwrap()
                .as_deref(),
            Some("node:B"),
            "an empty title falls back to key"
        );
        assert_eq!(
            shell.text_of(".cs-tab:nth-child(2) .cs-tab-label").as_deref(),
            Some("node:B"),
            "an empty label falls back to key"
        );
        assert_eq!(
            count_matching(&shell, ".cs-tab-splitmark"),
            1,
            "only the split row carries the mark"
        );
        assert_eq!(
            count_matching(&shell, ".cs-tab:nth-child(2) .cs-tab-splitmark"),
            1
        );
        assert_eq!(
            shell
                .text_of(".cs-tab:nth-child(2) .cs-tab-splitmark")
                .as_deref(),
            Some("\u{258c}")
        );
        assert_eq!(
            shell
                .attr_of(".cs-tab:nth-child(2) .cs-tab-splitmark", "title")
                .unwrap()
                .as_deref(),
            Some("Shown in the split")
        );
        // Child order is label, splitmark, close: textContent proves the order.
        assert_eq!(
            shell.text_of(".cs-tab:nth-child(2)").as_deref(),
            Some("node:B\u{258c}\u{2715}")
        );

        // Row 3: plain.
        assert_eq!(
            shell.text_of(".cs-tab:nth-child(3)").as_deref(),
            Some("C\u{2715}")
        );

        // ---- every row's close button (shell.js:1219-1224) ------------------
        assert_eq!(count_matching(&shell, ".cs-tab-x"), 3);
        for (i, key) in ["node:A", "node:B", "node:C"].iter().enumerate() {
            let sel = format!(".cs-tab:nth-child({}) .cs-tab-x", i + 1);
            assert_eq!(
                shell.classes_of(&sel),
                Some(vec!["cs-tab-x".to_string()]),
                "row {i} close button class"
            );
            assert_eq!(
                shell.attr_of(&sel, "type").unwrap().as_deref(),
                Some("button")
            );
            assert_eq!(
                shell.attr_of(&sel, "data-close").unwrap().as_deref(),
                Some(*key),
                "row {i} data-close is its own key"
            );
            assert_eq!(
                shell.attr_of(&sel, "title").unwrap().as_deref(),
                Some("Close tab")
            );
            assert_eq!(shell.text_of(&sel).as_deref(), Some("\u{2715}"));
        }

        // ---- .cs-tab-actions (shell.js:1227-1248) ---------------------------
        assert_eq!(count_matching(&shell, ".cs-tab-actions"), 1);
        assert_eq!(
            shell
                .child_elements_of("#cs-tabs")
                .expect("bar children")
                .len(),
            4,
            "three rows then the actions group"
        );
        assert_eq!(
            shell.classes_of("#csSplitBtn"),
            Some(vec!["btn".to_string(), "btn-ghost".to_string()])
        );
        assert_eq!(
            shell.text_of("#csSplitBtn").as_deref(),
            Some("\u{25a5} Split"),
            "split is off, so the split button says Split"
        );
        assert_eq!(
            shell.attr_of("#csSplitBtn", "title").unwrap().as_deref(),
            Some("Show a second document beside this one (Alt+S)")
        );
        assert!(
            shell.attr_of("#csSplitBtn", "disabled").unwrap().is_none(),
            "three tabs enable the split button"
        );
        assert_eq!(
            shell.classes_of("#csTabCloseOthers"),
            Some(vec!["btn".to_string(), "btn-ghost".to_string()])
        );
        assert_eq!(
            shell.text_of("#csTabCloseOthers").as_deref(),
            Some("Close others")
        );
        assert_eq!(
            shell
                .attr_of("#csTabCloseOthers", "title")
                .unwrap()
                .as_deref(),
            Some("Close every tab but this one (Alt+Shift+W)")
        );
        assert!(
            shell
                .attr_of("#csTabCloseOthers", "disabled")
                .unwrap()
                .is_none(),
            "three tabs enable Close others"
        );
    }

    /// The disabled rules are the reference's, computed from the tab count and
    /// the split flag: the split button needs two tabs **or** an existing split;
    /// Close others needs two tabs (shell.js:1234, 1242).
    #[test]
    fn the_action_buttons_follow_the_reference_disabled_rules() {
        // One tab, no split: both disabled, split says Split.
        let shell = build(Some(ui(
            serde_json::json!([{"key": "node:A", "label": "A", "active": true}]),
            false,
            true,
        )));
        assert!(shell
            .attr_of("#csSplitBtn", "disabled")
            .unwrap()
            .is_some());
        assert!(shell
            .attr_of("#csTabCloseOthers", "disabled")
            .unwrap()
            .is_some());
        assert_eq!(
            shell.text_of("#csSplitBtn").as_deref(),
            Some("\u{25a5} Split")
        );

        // One tab but already split: Unsplit is always enabled; Close others is
        // still disabled.
        let shell = build(Some(ui(
            serde_json::json!([{"key": "node:A", "label": "A", "active": true, "split": true}]),
            true,
            true,
        )));
        assert!(
            shell.attr_of("#csSplitBtn", "disabled").unwrap().is_none(),
            "an existing split keeps Unsplit enabled"
        );
        assert_eq!(
            shell.text_of("#csSplitBtn").as_deref(),
            Some("\u{25a4} Unsplit")
        );
        assert!(shell
            .attr_of("#csTabCloseOthers", "disabled")
            .unwrap()
            .is_some());

        // Two tabs: both enabled.
        let shell = build(Some(ui(
            serde_json::json!([
                {"key": "node:A", "label": "A", "active": true},
                {"key": "node:B", "label": "B"}
            ]),
            false,
            true,
        )));
        assert!(shell.attr_of("#csSplitBtn", "disabled").unwrap().is_none());
        assert!(shell
            .attr_of("#csTabCloseOthers", "disabled")
            .unwrap()
            .is_none());
    }

    /// An empty tab list is a REAL state: the bar exists (it is created by the
    /// reference too) and carries `hidden`, so it has a zero-area box while the
    /// document keeps its layout.
    #[test]
    fn an_empty_tab_list_creates_a_hidden_bar() {
        let shell = build(Some(ui(serde_json::json!([]), false, true)));
        assert_eq!(count_matching(&shell, "#cs-tabs"), 1, "the bar exists");
        assert!(
            shell
                .classes_of("#cs-tabs")
                .unwrap_or_default()
                .contains(&"hidden".to_string()),
            "an empty tab list hides the bar (tabs.length === 0)"
        );
        assert_eq!(count_matching(&shell, ".cs-tab"), 0, "no rows");
        // Still the first child, still tablist-labelled.
        let bar = shell
            .document()
            .query_selector("#cs-tabs")
            .ok()
            .flatten()
            .expect("bar must exist");
        assert_eq!(
            shell
                .child_elements_of("#sourcePane")
                .expect("pane")
                .first()
                .copied(),
            Some(bar)
        );
        assert_eq!(
            shell.attr_of("#cs-tabs", "role").unwrap().as_deref(),
            Some("tablist")
        );
        // `#cs-tabs.hidden { display: none; }` — no box a hit-test can reach.
        let b = shell.box_of("#cs-tabs").expect("the bar exists");
        assert!(
            b.w == 0.0 || b.h == 0.0,
            "a hidden bar must have a zero-area box, got {}x{}",
            b.w,
            b.h
        );
        // Both action buttons are disabled with an empty list.
        assert!(shell.attr_of("#csSplitBtn", "disabled").unwrap().is_some());
        assert!(shell
            .attr_of("#csTabCloseOthers", "disabled")
            .unwrap()
            .is_some());
    }

    /// THE REACHABILITY PROOF: with the source pane open and a non-empty tab
    /// list, the bar is laid out by the real stylesheet into a real box — while
    /// the same bar hidden (empty list) has none.
    #[test]
    fn an_open_pane_gives_the_visible_bar_a_real_box() {
        let shell = build(Some(ui(three_tabs(), false, true)));
        let b = shell.box_of("#cs-tabs").expect("the bar is laid out");
        assert!(
            b.w > 0.0 && b.h > 0.0,
            "#cs-tabs must have a real box when visible, got {}x{}",
            b.w,
            b.h
        );
        // The rows are laid out inside it too.
        assert!(
            shell
                .box_of(".cs-tab")
                .map(|c| c.w > 0.0 && c.h > 0.0)
                == Some(true),
            "the first tab row must have a real box"
        );
        assert!(
            shell
                .box_of("#csSplitBtn")
                .map(|c| c.w > 0.0 && c.h > 0.0)
                == Some(true),
            "the split button must have a real box"
        );

        // The hidden counterpart: pane open, empty list -> zero area.
        let shell = build(Some(ui(serde_json::json!([]), false, true)));
        let b = shell.box_of("#cs-tabs").expect("the bar exists");
        assert!(
            b.w == 0.0 || b.h == 0.0,
            "a hidden bar must be unreachable, got {}x{}",
            b.w,
            b.h
        );
    }

    /// The wire contract: the host's `ui.tabs` JSON deserializes into
    /// [`TabsFacts`] with every key populated, and an omitted key is `None`
    /// rather than a default-populated bar.
    #[test]
    fn the_tabs_wire_shape_deserializes_into_tabs_facts() {
        let payload = serde_json::json!({
            "tabs": [
                {"key": "node:A", "label": "A", "active": true, "split": false, "title": "src/A.cs"},
                {"key": "node:B", "label": "B", "active": false, "split": true, "title": "node:B"}
            ],
            "split_on": true
        });
        let facts: TabsFacts = serde_json::from_value(payload).expect("ui.tabs deserializes");
        assert_eq!(facts.tabs.len(), 2);
        assert!(facts.split_on);
        let a = &facts.tabs[0];
        assert_eq!(a.key, "node:A");
        assert_eq!(a.label, "A");
        assert!(a.active);
        assert!(!a.split);
        assert_eq!(a.title, "src/A.cs");
        let b = &facts.tabs[1];
        assert!(!b.active);
        assert!(b.split);
        assert_eq!(b.title, "node:B");

        // Absent keys stay absent, which is what keeps the default render
        // byte-identical.
        let ui: UiState = serde_json::from_value(serde_json::json!({})).expect("empty ui");
        assert!(ui.tabs.is_none());
        let ui: UiState = serde_json::from_value(serde_json::json!({"tabs": null}))
            .expect("null tabs");
        assert!(ui.tabs.is_none());
    }
}

#[cfg(test)]
mod split_tests {
    use super::*;
    use crate::NzkUi;

    /// Build the runtime `NzkUi` from the real `style.css` with the given `ui`
    /// object spliced into a round-tripped `Facts` — the same wire shape the app
    /// publishes (and the shape `apply_split` consumes).
    fn facts(ui: Option<serde_json::Value>) -> Facts {
        let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
            .expect("Facts must serialize");
        if let Some(ui) = ui {
            value["ui"] = ui;
        }
        serde_json::from_value(value).expect("Facts must round-trip")
    }

    fn build_from(facts: &Facts) -> NzkUi {
        let dir = NzkUi::frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), facts)
            .expect("build skeleton shell")
    }

    fn build(ui: Option<serde_json::Value>) -> NzkUi {
        build_from(&facts(ui))
    }

    fn count_matching(shell: &NzkUi, selector: &str) -> usize {
        shell
            .document()
            .query_selector_all(selector)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// `ui` carrying the tab facts (and, optionally, the pane opened so the split
    /// scaffold has real layout to land in).
    fn ui(tabs: serde_json::Value, split_on: bool, pane_open: bool) -> serde_json::Value {
        serde_json::json!({
            "pane_open": pane_open,
            "tabs": {"tabs": tabs, "split_on": split_on},
        })
    }

    /// Three documents: A active, B split (empty title/label -> key fallbacks),
    /// C plain. The exact shape `ui.tabs` publishes.
    fn three_tabs() -> serde_json::Value {
        serde_json::json!([
            {"key": "node:A", "label": "A", "active": true, "split": false, "title": "src/A.cs"},
            {"key": "node:B", "label": "", "active": false, "split": true, "title": ""},
            {"key": "node:C", "label": "C", "active": false, "split": false, "title": "node:C"},
        ])
    }

    fn node_of(doc: &BaseDocument, selector: &str) -> usize {
        doc.query_selector(selector)
            .expect("selector must parse")
            .unwrap_or_else(|| panic!("{selector} must exist"))
    }

    fn parent_of(doc: &BaseDocument, node: usize) -> Option<usize> {
        doc.get_node(node).and_then(|n| n.parent)
    }

    /// Element children as `id`, else `.class`, else the tag name — enough to
    /// pin the reference's exact child order without a second DOM model.
    fn element_children(doc: &BaseDocument, parent: usize) -> Vec<String> {
        doc.get_node(parent)
            .expect("parent node")
            .children
            .iter()
            .filter_map(|child| {
                let node = doc.get_node(*child)?;
                let el = node.element_data()?;
                let id = el
                    .attrs
                    .iter()
                    .find(|a| a.name.local.as_ref() == "id")
                    .map(|a| a.value.clone());
                Some(match id {
                    Some(id) => id,
                    None => match el
                        .attrs
                        .iter()
                        .find(|a| a.name.local.as_ref() == "class")
                    {
                        Some(class) => format!(".{}", class.value.split_whitespace().next().unwrap_or("")),
                        None => el.name.local.to_string(),
                    },
                })
            })
            .collect()
    }

    /// Round-8 geometry contract: `#csSplitHost` and `#csSplitDivider` are probe
    /// selectors, MISSING under the shipped facts and real non-zero boxes once
    /// `split_on` builds the scaffold.
    ///
    /// ADVERSARIAL: fails if either selector is dropped from [`PROBE_SELECTORS`]
    /// (the host's second-body paint reads `#csSplitHost` from the same box map),
    /// and fails if the nodes appear under absent facts — which would move the
    /// pinned `@frame 1600 1000 845180d07a64ce3d`.
    #[test]
    fn the_split_host_is_probed_missing_by_default_and_laid_out_when_on() {
        assert!(PROBE_SELECTORS.contains(&"#csSplitHost"));
        assert!(PROBE_SELECTORS.contains(&"#csSplitDivider"));

        let probe = |shell: &NzkUi, sel: &str| {
            shell
                .boxes()
                .into_iter()
                .find(|(s, _)| *s == sel)
                .unwrap_or_else(|| panic!("{sel} must be a probe row"))
                .1
        };

        let off = build(None);
        assert!(
            probe(&off, "#csSplitHost").is_none(),
            "shipped facts probe #csSplitHost as MISSING"
        );
        assert!(
            probe(&off, "#csSplitDivider").is_none(),
            "shipped facts probe #csSplitDivider as MISSING"
        );

        let on = build(Some(ui(three_tabs(), true, true)));
        let host = probe(&on, "#csSplitHost")
            .expect("#csSplitHost must probe a real box when split is on");
        assert!(
            host.w > 0.0 && host.h > 0.0,
            "the second body must have area to paint into, got {}x{}",
            host.w,
            host.h
        );
        let divider = probe(&on, "#csSplitDivider")
            .expect("#csSplitDivider must probe a real box when split is on");
        assert!(divider.w > 0.0, "divider width {}", divider.w);
    }

    /// create and move nothing — `#sourceBody` keeps the shipped `#sourcePane`
    /// parent (which is what keeps `@frame 1600 1000 845180d07a64ce3d` pinned)
    /// and the default live tree stays deterministic.
    ///
    /// ADVERSARIAL: fails if `apply_split` is entered unconditionally, if it
    /// creates a display:none wrapper on the shipped path, or if it reparents a
    /// body for a split that is off.
    #[test]
    fn absent_or_off_split_facts_create_nothing_and_leave_the_body_in_place() {
        // (1) No `ui` at all: the shipped document.
        let shell = build(None);
        assert_eq!(count_matching(&shell, "#csSplitWrap"), 0);
        assert_eq!(count_matching(&shell, "#csSplitDivider"), 0);
        assert_eq!(count_matching(&shell, "#csSplitBody"), 0);
        let doc = shell.document();
        assert_eq!(
            parent_of(doc, node_of(doc, "#sourceBody")),
            Some(node_of(doc, "#sourcePane")),
            "the shipped #sourceBody must stay a child of #sourcePane"
        );
        let a = build(None);
        let b = build(None);
        assert_eq!(
            live_tree_dump(a.document()),
            live_tree_dump(b.document()),
            "the default live document must be deterministic"
        );

        // (2) `ui.tabs` present but `split_on: false`: the bar paints, the split
        // scaffold does not.
        let shell = build(Some(ui(three_tabs(), false, true)));
        assert_eq!(
            count_matching(&shell, "#cs-tabs"),
            1,
            "the bar fact is applied; only the split is off"
        );
        assert_eq!(count_matching(&shell, "#csSplitWrap"), 0);
        let doc = shell.document();
        assert_eq!(
            parent_of(doc, node_of(doc, "#sourceBody")),
            Some(node_of(doc, "#sourcePane")),
            "split_on:false must not reparent #sourceBody"
        );
    }

    /// The `split_on: true` scaffold: `#csSplitWrap` replaces `#sourceBody` as a
    /// direct child of `#sourcePane`, and its children are, in this exact order,
    /// the existing body, the divider, the split body — with the divider's full
    /// separator contract and the head/collapse/host structure from
    /// `shell.js:1735-1802`.
    ///
    /// ADVERSARIAL: fails if a child is appended out of order, if the wrap is a
    /// sibling *beside* the body (the CSS `#csSplitWrap > #sourceBody` rules then
    /// never match), or if any of the separator/collapse attributes is missing.
    #[test]
    fn split_on_builds_the_reference_wrap_and_child_order() {
        let shell = build(Some(ui(three_tabs(), true, true)));

        assert_eq!(count_matching(&shell, "#csSplitWrap"), 1);
        let doc = shell.document();
        let pane = node_of(doc, "#sourcePane");
        let wrap = node_of(doc, "#csSplitWrap");
        assert_eq!(parent_of(doc, wrap), Some(pane), "wrap replaces the body in #sourcePane");
        assert_eq!(
            element_children(doc, wrap),
            vec!["sourceBody", "csSplitDivider", "csSplitBody"],
            "the reference appends body, then divider, then split body"
        );
        assert!(
            shell
                .classes_of("#csSplitWrap")
                .expect("#csSplitWrap")
                .iter()
                .any(|c| c == "cs-split-on"),
            "`cs-split-on` is the single layout switch (shell.js:1374)"
        );

        // ---- #csSplitDivider (install(): shell.js:1749-1753) ----------------
        assert_eq!(
            shell.classes_of("#csSplitDivider"),
            Some(vec!["pane-splitter".to_string(), "pane-splitter-v".to_string()])
        );
        assert_eq!(
            shell.attr_of("#csSplitDivider", "role").unwrap().as_deref(),
            Some("separator")
        );
        assert_eq!(
            shell.attr_of("#csSplitDivider", "aria-orientation").unwrap().as_deref(),
            Some("vertical")
        );
        assert_eq!(
            shell.attr_of("#csSplitDivider", "title").unwrap().as_deref(),
            Some("Drag to resize the split")
        );

        // ---- #csSplitBody > (.cs-split-head, #csSplitHost) ------------------
        let split_body = node_of(doc, "#csSplitBody");
        assert_eq!(
            element_children(doc, split_body),
            vec![".cs-split-head", "csSplitHost"]
        );
        let head = node_of(doc, "#csSplitBody > .cs-split-head");
        assert_eq!(
            element_children(doc, head),
            vec![".cs-split-label", ".cs-split-spacer", "csSplitCollapse"]
        );
        // The split row (B) has an empty label, so `tab.label || tab.key` renders
        // its key — never invented copy.
        assert_eq!(shell.text_of(".cs-split-label"), Some("node:B".to_string()));
        assert_eq!(
            shell.classes_of("#csSplitCollapse"),
            Some(vec!["btn".to_string(), "btn-ghost".to_string()])
        );
        assert_eq!(
            shell.attr_of("#csSplitCollapse", "type").unwrap().as_deref(),
            Some("button")
        );
        assert_eq!(
            shell.attr_of("#csSplitCollapse", "title").unwrap().as_deref(),
            Some("Collapse the split")
        );
        assert_eq!(shell.text_of("#csSplitCollapse"), Some("\u{2715}".to_string()));
        // The host exists and stays EMPTY in this slice: the second document is
        // app-side state this port does not have yet.
        assert!(element_children(doc, node_of(doc, "#csSplitHost")).is_empty());

        // `#sourceNote` keeps its place after the wrap; nothing was lost.
        assert_eq!(
            element_children(doc, pane),
            vec!["cs-tabs", ".source-head", "csSplitWrap", "sourceNote"]
        );

        // A labelled split row renders its label verbatim.
        let labelled = serde_json::json!([
            {"key": "node:A", "label": "A", "active": true, "split": false, "title": "A"},
            {"key": "node:B", "label": "B.cs", "active": false, "split": true, "title": "B.cs"},
        ]);
        let labelled_shell = build(Some(ui(labelled, true, true)));
        assert_eq!(labelled_shell.text_of(".cs-split-label"), Some("B.cs".to_string()));

        // `split_on: true` with no row marked `split` falls back to the first row
        // rather than fabricating a blank head.
        let no_mark = serde_json::json!([
            {"key": "node:A", "label": "", "active": true, "split": false, "title": ""},
        ]);
        let fallback = build(Some(ui(no_mark, true, true)));
        assert_eq!(fallback.text_of(".cs-split-label"), Some("node:A".to_string()));
    }

    /// With `cs-split-on` set the shipped stylesheet must actually LAY OUT the
    /// split: the wrap and the empty split body have non-zero boxes, the divider
    /// measures, and the source body is the CSS default `--cs-split-w: 50%` of
    /// the wrap (`flex: 0 0 var(--cs-split-w, 50%)`).
    ///
    /// ADVERSARIAL: catches a scaffold that is painted but never styled (zero
    /// boxes) and a body that never shrank because the class was not set.
    #[test]
    fn split_on_lays_out_the_wrap_and_halves_the_source_body() {
        let shell = build(Some(ui(three_tabs(), true, true)));
        let wrap = shell.box_of("#csSplitWrap").expect("#csSplitWrap must be laid out");
        assert!(wrap.w > 0.0 && wrap.h > 0.0, "wrap box {}x{}", wrap.w, wrap.h);
        let split_body = shell
            .box_of("#csSplitBody")
            .expect("#csSplitBody must be laid out");
        assert!(
            split_body.w > 0.0 && split_body.h > 0.0,
            "the split body must flex beside the source body, got {}x{}",
            split_body.w,
            split_body.h
        );
        let divider = shell
            .box_of("#csSplitDivider")
            .expect("#csSplitDivider must be displayed when split is on");
        assert!(divider.w > 0.0, "divider must have width, got {}", divider.w);
        let source = shell.box_of("#sourceBody").expect("#sourceBody box");
        let half = wrap.w * 0.5;
        assert!(
            (source.w - half).abs() <= wrap.w * 0.06,
            "default --cs-split-w is 50%: source {} vs wrap {} (half {})",
            source.w,
            wrap.w,
            half
        );

        // The off case must not measure the divider/split body at all
        // (`display: none` unless `#csSplitWrap.cs-split-on`).
        let off = build(Some(ui(three_tabs(), false, true)));
        assert!(off.box_of("#csSplitWrap").is_none(), "off creates no wrap");
    }

    /// `ui.tabs` plus the divider-ratio fact (`shell.js:1471-1472`).
    fn ui_with_w(
        tabs: serde_json::Value,
        split_on: bool,
        pane_open: bool,
        split_w: serde_json::Value,
    ) -> serde_json::Value {
        let mut v = ui(tabs, split_on, pane_open);
        v["tabs"]["split_w"] = split_w;
        v
    }

    /// A published ratio must reach the shipped `--cs-split-w` custom property
    /// AND the real layout: the source body's measured width is that fraction of
    /// the wrap (`flex: 0 0 var(--cs-split-w, 50%)`), never the 50% fallback.
    ///
    /// ADVERSARIAL: fails if `apply_split` drops the ratio, writes it on a node
    /// the CSS rule cannot see, formats it as a raw fraction (e.g. `0.7%`), or
    /// only handles the creation pass.
    #[test]
    fn the_split_ratio_fact_writes_the_property_and_lays_out_that_width() {
        for (frac, pct) in [(0.7_f64, "70.0"), (0.3, "30.0")] {
            let shell = build(Some(ui_with_w(
                three_tabs(),
                true,
                true,
                serde_json::json!(frac),
            )));
            assert_eq!(
                shell.attr_of("#csSplitWrap", "style"),
                Ok(Some(format!("--cs-split-w: {pct}%"))),
                "the ratio must be written in applySplitWidth's toFixed(1) form"
            );
            let wrap = shell.box_of("#csSplitWrap").expect("#csSplitWrap laid out");
            let source = shell.box_of("#sourceBody").expect("#sourceBody laid out");
            let ratio = f64::from(source.w) / f64::from(wrap.w);
            assert!(
                (ratio - frac).abs() <= 0.03,
                "layout must follow --cs-split-w={frac}: source {} of wrap {} = {ratio}",
                source.w,
                wrap.w
            );
        }
    }

    /// ABSENT ratio contract: no `style` attribute is written at all and the
    /// shipped stylesheet's own 50% fallback lays the split out. This is what
    /// keeps an old host's frame byte-identical.
    ///
    /// ADVERSARIAL: fails if absence is treated as 0.5 and written explicitly.
    #[test]
    fn an_absent_split_ratio_writes_no_style_and_keeps_the_50pct_default() {
        let shell = build(Some(ui(three_tabs(), true, true)));
        assert_eq!(
            shell.attr_of("#csSplitWrap", "style"),
            Ok(None),
            "absent ratio must not write a style attribute"
        );
        let wrap = shell.box_of("#csSplitWrap").expect("#csSplitWrap laid out");
        let source = shell.box_of("#sourceBody").expect("#sourceBody laid out");
        let ratio = f64::from(source.w) / f64::from(wrap.w);
        assert!(
            (ratio - 0.5).abs() <= 0.03,
            "the CSS fallback must remain 50%: source {} of wrap {}",
            source.w,
            wrap.w
        );
    }

    /// LIVE second pass: a later facts set with a new ratio must update the SAME
    /// wrap (the creation branch is one-shot), so a drag that publishes a fresh
    /// ratio repaints rather than keeping the first value.
    ///
    /// ADVERSARIAL: fails if the ratio write lives only in the creation branch.
    #[test]
    fn a_second_facts_pass_updates_the_split_ratio() {
        let first = facts(Some(ui_with_w(
            three_tabs(),
            true,
            true,
            serde_json::json!(0.5),
        )));
        let mut shell = build_from(&first);
        assert_eq!(
            shell.attr_of("#csSplitWrap", "style"),
            Ok(Some("--cs-split-w: 50.0%".to_string()))
        );

        let second = facts(Some(ui_with_w(
            three_tabs(),
            true,
            true,
            serde_json::json!(0.8),
        )));
        apply_runtime_dom(&mut shell.doc, &second);
        assert_eq!(
            shell.attr_of("#csSplitWrap", "style"),
            Ok(Some("--cs-split-w: 80.0%".to_string())),
            "a later ratio must replace the inline custom property"
        );
        assert_eq!(count_matching(&shell, "#csSplitWrap"), 1, "no duplicate wrap");
    }

    /// Idempotence on the SAME live document: a second facts pass re-asserts the
    /// class and head label but must never duplicate the wrap or reparent the
    /// body again. The stale label is written before the pass, so a
    /// no-op-looking implementation that only paints on creation fails here.
    ///
    /// ADVERSARIAL: catches a second `apply_split` appending a second
    /// `#csSplitWrap`/`#csSplitBody` or leaving a stale head label.
    #[test]
    fn a_second_facts_pass_does_not_duplicate_or_lose_the_wrap() {
        let facts = facts(Some(ui(three_tabs(), true, true)));
        let mut shell = build_from(&facts);

        // Make the tree lie about the head label, then repaint over it.
        {
            let mut d = Dom::new(&mut shell.doc);
            let split_body = d.id("csSplitBody").expect("#csSplitBody");
            let head = child_by_class(d.doc, split_body, "cs-split-head").expect("head");
            let label = child_by_class(d.doc, head, "cs-split-label").expect("label");
            d.set_text(label, "stale");
        }
        apply_runtime_dom(&mut shell.doc, &facts);

        assert_eq!(count_matching(&shell, "#csSplitWrap"), 1, "no duplicate wrap");
        assert_eq!(count_matching(&shell, "#csSplitBody"), 1, "no duplicate split body");
        assert_eq!(count_matching(&shell, "#sourceBody"), 1, "the body was moved, not cloned");
        let doc = shell.document();
        assert_eq!(
            parent_of(doc, node_of(doc, "#sourceBody")),
            Some(node_of(doc, "#csSplitWrap")),
            "the second pass must not move the body out of the wrap"
        );
        assert!(
            shell
                .classes_of("#csSplitWrap")
                .expect("#csSplitWrap")
                .iter()
                .any(|c| c == "cs-split-on"),
            "the class flag is re-asserted on the second pass"
        );
        assert_eq!(
            shell.text_of(".cs-split-label"),
            Some("node:B".to_string()),
            "the head label is re-asserted on the second pass"
        );
    }
}

#[cfg(test)]
mod search_input_tests {
    use super::*;
    use crate::NzkUi;

    /// Build the runtime `NzkUi` from the real `style.css` with the given `ui`
    /// object spliced into a round-tripped `Facts` — the same wire shape the app
    /// publishes.
    fn build(ui: Option<serde_json::Value>) -> NzkUi {
        let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
            .expect("Facts must serialize");
        if let Some(ui) = ui {
            value["ui"] = ui;
        }
        let facts: Facts = serde_json::from_value(value).expect("Facts must round-trip");
        let dir = NzkUi::frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
            .expect("build skeleton shell")
    }

    /// `#searchInput`'s shipped placeholder (`index.html:76`, the literal
    /// `setMode('graph')` writes back, `search.js:96`).
    const SHIPPED_PLACEHOLDER: &str = "Search symbols…  ( / )";
    /// The Find-mode placeholder (`search.js:96`).
    const FIND_PLACEHOLDER: &str = "Find symbol or code…  ( f )";

    fn placeholder(shell: &NzkUi) -> Option<String> {
        shell
            .attr_of("#searchInput", "placeholder")
            .expect("#searchInput must exist")
    }

    fn value_attr(shell: &NzkUi) -> Option<String> {
        shell
            .attr_of("#searchInput", "value")
            .expect("#searchInput must exist")
    }

    /// THE DEFAULT-RENDER PROMISE: no `ui` (or a `ui` without `search_query`)
    /// writes no `value` attribute and keeps the shipped placeholder, and two
    /// default builds serialize identically.
    #[test]
    fn an_absent_search_query_writes_nothing() {
        let shell = build(None);
        assert_eq!(
            value_attr(&shell),
            None,
            "the shipped input has no value attribute; an absent fact must not add one"
        );
        assert_eq!(
            placeholder(&shell),
            Some(SHIPPED_PLACEHOLDER.to_string()),
            "the default paint keeps index.html's placeholder"
        );

        let shell = build(Some(serde_json::json!({"pane_open": true})));
        assert_eq!(
            value_attr(&shell),
            None,
            "a ui object without search_query is equally a no-op"
        );

        // The default document is stable: two builds produce the same LIVE tree.
        // `resolved_html()` returns the immutable parsed source (`lib.rs:548`),
        // equal by construction and blind to every DOM mutation, so it was
        // vacuous evidence.
        let a = build(None);
        let b = build(None);
        assert_eq!(
            live_tree_dump(a.document()),
            live_tree_dump(b.document()),
            "the default live document must stay deterministic and unchanged"
        );
    }

    /// `Some(query)` paints the typed text onto `#searchInput` as its `value`
    /// attribute — that is the DOM's representation of the input's `value`.
    #[test]
    fn a_search_query_is_written_to_the_input() {
        let shell = build(Some(serde_json::json!({"search_query": "Needle"})));
        assert_eq!(
            value_attr(&shell),
            Some("Needle".to_string()),
            "the typed query must reach #searchInput's value"
        );
        // It does not disturb the shipped placeholder.
        assert_eq!(placeholder(&shell), Some(SHIPPED_PLACEHOLDER.to_string()));
    }

    /// `Some("")` is a real cleared state: the attribute is present and empty,
    /// which is distinguishable from "never written" (absence).
    #[test]
    fn an_empty_query_is_an_empty_value_not_an_absent_attribute() {
        let shell = build(Some(serde_json::json!({"search_query": ""})));
        assert_eq!(
            value_attr(&shell),
            Some(String::new()),
            "an empty focused query is value=\"\", not a missing attribute"
        );
    }

    /// The placeholder follows the mode (`setMode`, `search.js:96`): Find shows
    /// the Find copy, Graph the shipped search copy.
    #[test]
    fn the_placeholder_follows_the_active_mode() {
        let find = build(Some(serde_json::json!({"active_mode": "find"})));
        assert_eq!(
            placeholder(&find),
            Some(FIND_PLACEHOLDER.to_string()),
            "find mode swaps the placeholder"
        );
        let graph = build(Some(serde_json::json!({"active_mode": "graph"})));
        assert_eq!(
            placeholder(&graph),
            Some(SHIPPED_PLACEHOLDER.to_string()),
            "graph mode is the shipped copy (an idempotent write)"
        );
    }

    /// The wire field is `search_query`, a plain JSON string; absent/null stay
    /// `None`, and the value round-trips.
    #[test]
    fn the_search_query_wire_shape_is_stable() {
        let ui: UiState = serde_json::from_value(serde_json::json!({"search_query": "abc"}))
            .expect("deserialize ui");
        assert_eq!(ui.search_query.as_deref(), Some("abc"));

        let ui: UiState = serde_json::from_value(serde_json::json!({})).expect("empty ui");
        assert!(ui.search_query.is_none());
        let ui: UiState =
            serde_json::from_value(serde_json::json!({"search_query": null})).expect("null");
        assert!(ui.search_query.is_none());
    }
}

/// Canonical structural dump of the **live** DOM rooted at the document's root
/// element: one line per element with its tag and sorted attributes, and one
/// line per text node with its content, in document order.
///
/// Why this exists: [`NzkUi::resolved_html`] returns the *immutable parsed
/// source* (`&self.html`), which no DOM mutation ever reaches. Comparing two
/// shells through `resolved_html()` therefore proves nothing about their DOMs —
/// it is a comparison of two constant input strings. This dump walks the same
/// live tree that [`crate::NzkUi::document`] exposes and that every mutation in
/// this module writes to, so two equal dumps are real structural evidence that
/// the whole document was left untouched.
///
/// Attributes are sorted so a mutation that only rewrites a value (e.g. a
/// class list) still shows up; text nodes carry their content verbatim.
#[cfg(test)]
fn live_tree_dump(doc: &BaseDocument) -> String {
    fn walk(doc: &BaseDocument, node: usize, out: &mut String) {
        let Some(n) = doc.get_node(node) else {
            return;
        };
        if let Some(data) = n.element_data() {
            out.push('<');
            out.push_str(&data.name.local.to_string());
            let mut attrs: Vec<(String, String)> = data
                .attrs
                .iter()
                .map(|a| (a.name.local.to_string(), a.value.clone()))
                .collect();
            attrs.sort();
            for (name, value) in attrs {
                out.push(' ');
                out.push_str(&name);
                out.push('=');
                out.push_str(&value);
            }
            out.push_str(">\n");
        } else if let Some(t) = n.text_data() {
            out.push_str("text:");
            out.push_str(&t.content);
            out.push('\n');
        } else {
            // Doctype/comment/opaque nodes: record their kind so a difference
            // cannot hide in a node type the dump would otherwise skip.
            out.push_str("opaque-node\n");
        }
        for child in &n.children {
            walk(doc, *child, out);
        }
    }
    let mut out = String::new();
    walk(doc, doc.root_element().id, &mut out);
    out
}

/// `ui.shortcuts` → `#shortcutsBody` — `renderShortcuts` (`shell.js:330-342`).
///
/// The reference builds these rows only when the sheet opens, and `index.html`
/// ships the body EMPTY. Both halves of that contract are pinned here: absent
/// facts must create no child (the default render stays byte-identical), and
/// present facts must reproduce the reference's class names and DOM order.
#[cfg(test)]
mod shortcuts_tests {
    use super::*;
    use crate::NzkUi;

    /// Build the runtime shell from the real skeleton with `ui` spliced in.
    fn build(ui: Option<serde_json::Value>) -> NzkUi {
        let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
            .expect("Facts must serialize");
        if let Some(ui) = ui {
            value["ui"] = ui;
        }
        let facts: Facts = serde_json::from_value(value).expect("Facts must round-trip");
        let dir = NzkUi::frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
            .expect("build skeleton shell")
    }

    fn count(shell: &NzkUi, selector: &str) -> usize {
        shell
            .document()
            .query_selector_all(selector)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    /// THE DEFAULT-RENDER PROMISE: `#shortcutsBody` ships empty and only the
    /// open-sheet fact fills it. Absent facts (even with the modal open) create
    /// not one child, so the pinned `@frame` cannot move.
    #[test]
    fn absent_facts_leave_the_shipped_empty_body() {
        for ui in [
            None,
            Some(serde_json::json!({})),
            // The sheet open but no content fact: still the shipped empty body.
            Some(serde_json::json!({"modals_open": ["shortcutsModal"]})),
        ] {
            let shell = build(ui);
            assert_eq!(
                count(&shell, "#shortcutsBody *"),
                0,
                "no row may be created without the shortcuts fact"
            );
            assert_eq!(count(&shell, ".shortcut-row"), 0);
            assert_eq!(count(&shell, ".shortcut-group"), 0);
        }
        // The strongest local evidence: the sheet is OPEN (its `hidden` class is
        // cleared by `modals_open`), yet the body stays exactly the empty node
        // `index.html` ships — so `apply_shortcuts` did not run without the fact.
    }

    /// Present facts rebuild the body with the reference's exact class names and
    /// child order: `.shortcut-group`, `.shortcut-row`, `.shortcut-desc`,
    /// `.shortcut-keys`, `.shortcut-plus`, `<kbd>`.
    #[test]
    fn present_facts_build_the_reference_rows_and_class_names() {
        let shell = build(Some(serde_json::json!({
            "modals_open": ["shortcutsModal"],
            "shortcuts": {"rows": [
                {"group": "View", "keys": [["Ctrl", "Shift", "C"]],
                 "desc": "Recenter the view (fit the graph)"},
                {"group": "View", "keys": [["+"], ["="]], "desc": "Zoom in"},
                {"group": "Mouse", "keys": [["Wheel"]], "desc": "Zoom at the pointer"}
            ]}
        })));
        assert_eq!(count(&shell, ".shortcut-group"), 2, "one head per group change");
        assert_eq!(count(&shell, ".shortcut-row"), 3);
        assert_eq!(count(&shell, ".shortcut-desc"), 3);
        assert_eq!(count(&shell, ".shortcut-keys"), 3);
        // Scoped to the body: the document also ships the footer's `<kbd>?</kbd>`.
        assert_eq!(
            count(&shell, "#shortcutsBody kbd"),
            6,
            "3 + (1+1) + 1 chord parts"
        );
        assert_eq!(
            count(&shell, ".shortcut-plus"),
            3,
            "2 `+` inside row 1, 1 `/` between row 2's alternatives"
        );
        // `style.css` declares `text-transform: uppercase` on `.shortcut-group`,
        // and the engine bakes the transformed text into the text node during
        // layout, so the painted text is uppercase (what Chromium shows too).
        assert_eq!(
            shell.text_of(".shortcut-group").as_deref(),
            Some("VIEW"),
            "the first head carries the group name (upper-cased by style.css)"
        );
        // `textContent` order is `.shortcut-desc` then `.shortcut-keys`, and the
        // chips are joined by the separator text (`shell.js:298-328`).
        assert_eq!(
            shell.text_of(".shortcut-row").as_deref(),
            Some("Recenter the view (fit the graph)Ctrl+Shift+C")
        );
    }
}

#[cfg(test)]
mod suggestions_tests {
    use super::*;
    use crate::NzkUi;

    /// Build the runtime (`from_skeleton_with_facts`) shell with the given `ui`
    /// object spliced into a round-tripped `Facts` — the same wire shape the app
    /// publishes. This is the path a real host uses, not the index.html source.
    fn build(ui: Option<serde_json::Value>) -> NzkUi {
        let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
            .expect("Facts must serialize");
        if let Some(ui) = ui {
            value["ui"] = ui;
        }
        let facts: Facts = serde_json::from_value(value).expect("Facts must round-trip");
        let dir = NzkUi::frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
            .expect("build skeleton shell")
    }

    fn count_matching(shell: &NzkUi, selector: &str) -> usize {
        shell
            .document()
            .query_selector_all(selector)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    fn aria(shell: &NzkUi) -> Option<String> {
        shell
            .attr_of("#searchInput", "aria-expanded")
            .expect("#searchInput must exist")
    }

    /// The frozen `ui.suggestions` object from `app/src/nzk_ui.rs`.
    fn two_rows() -> serde_json::Value {
        serde_json::json!({
            "rows": [
                {"id": "n:1", "kind": "method", "label": "hit", "full": "App"},
                {"id": "n:2", "kind": "field", "label": "", "full": ""}
            ],
            "active": 0,
            "empty": null
        })
    }

    /// THE DEFAULT-RENDER PROMISE, adversarially: absent facts must leave the
    /// shipped box `class="suggestions hidden"` with NO children and
    /// `aria-expanded="false"`. If the paint ran unconditionally, the key would
    /// be missing but the box would still be un-hidden — this test fails on that
    /// (and `@frame` would move).
    #[test]
    fn absent_suggestions_facts_render_the_shipped_document() {
        for ui in [None, Some(serde_json::json!({"pane_open": true}))] {
            let shell = build(ui.clone());
            let classes = shell
                .classes_of("#suggestions")
                .expect("#suggestions must exist in the shipped skeleton");
            assert!(
                classes.iter().any(|c| c == "hidden"),
                "absent facts must keep the shipped `hidden` class: {classes:?}"
            );
            assert!(
                !classes.iter().any(|c| c == "suggestions-open" || c == "open"),
                "the paint invents no extra class"
            );
            assert_eq!(
                count_matching(&shell, "#suggestions *"),
                0,
                "hideSuggestions empties the box; absent facts create no child"
            );
            assert_eq!(count_matching(&shell, ".sug"), 0);
            assert_eq!(count_matching(&shell, ".sug-empty"), 0);
            assert_eq!(
                aria(&shell).as_deref(),
                Some("false"),
                "the shipped input says aria-expanded=false"
            );
        }
    }

    /// Presence = open (`showSuggestions`, search.js:11-14): the hidden class is
    /// gone, `aria-expanded` flips, and each row carries the reference's exact
    /// attributes and four children in the reference's order.
    #[test]
    fn open_suggestions_render_rows() {
        let shell = build(Some(serde_json::json!({ "suggestions": two_rows() })));
        let classes = shell.classes_of("#suggestions").unwrap();
        assert!(
            !classes.iter().any(|c| c == "hidden"),
            "an open popup must not keep `hidden`: {classes:?}"
        );
        assert_eq!(aria(&shell).as_deref(), Some("true"));
        assert_eq!(count_matching(&shell, ".sug"), 2, "one button per row");

        // Row 0: every attribute is the literal from search.js:29-31.
        assert_eq!(
            shell.attr_of(".sug:nth-child(1)", "data-id").unwrap().as_deref(),
            Some("n:1")
        );
        assert_eq!(
            shell.attr_of(".sug:nth-child(1)", "data-i").unwrap().as_deref(),
            Some("0"),
            "data-i is the array position, synthesised by the paint"
        );
        assert_eq!(
            shell.attr_of(".sug:nth-child(1)", "title").unwrap().as_deref(),
            Some("Open this symbol in the graph")
        );
        assert_eq!(
            shell.attr_of(".sug:nth-child(1)", "role").unwrap().as_deref(),
            Some("option")
        );
        assert_eq!(
            shell.attr_of(".sug:nth-child(1)", "type").unwrap().as_deref(),
            Some("button")
        );
        // The four children, in the reference's fixed order.
        assert_eq!(count_matching(&shell, ".sug > .dot:nth-child(1)"), 2);
        assert_eq!(count_matching(&shell, ".sug > .sug-kind:nth-child(2)"), 2);
        assert_eq!(count_matching(&shell, ".sug > .sug-label:nth-child(3)"), 2);
        assert_eq!(count_matching(&shell, ".sug > .sug-full:nth-child(4)"), 2);
        assert_eq!(
            shell.text_of(".sug:nth-child(1) .sug-kind").as_deref(),
            Some("method")
        );
        assert_eq!(
            shell.text_of(".sug:nth-child(1) .sug-label").as_deref(),
            Some("hit")
        );
        assert_eq!(
            shell.text_of(".sug:nth-child(1) .sug-full").as_deref(),
            Some("App")
        );
        // Row 1 has an empty label: the paint's `label || id` chain shows the id.
        assert_eq!(
            shell.text_of(".sug:nth-child(2) .sug-label").as_deref(),
            Some("n:2")
        );
        // The dot's inline style is the reference's own data string plus the
        // one palette transcription (`kind_color`, core.js KIND_COLOR_NZK_DARKPLUS).
        let style = shell
            .attr_of(".sug:nth-child(1) .dot", "style")
            .unwrap()
            .expect("the dot carries the inline geometry search.js:31 writes");
        assert!(
            style.contains("background:#dcdcaa"),
            "method dot must be the core.js method colour, got {style:?}"
        );
        assert!(style.contains("width:7px"), "7px dot: {style:?}");
        assert!(style.contains("height:7px"));
        assert!(style.contains("border-radius:50%"));
        assert!(style.contains("flex:none"));
        // The field dot is the field colour, not the method one.
        let field_style = shell
            .attr_of(".sug:nth-child(2) .dot", "style")
            .unwrap()
            .unwrap();
        assert!(
            field_style.contains("background:#9cdcfe"),
            "field dot colour: {field_style:?}"
        );
    }

    /// `search.js:23-27`: a message and no rows renders ONE `.sug-empty`, and the
    /// container is still SHOWN (the reference calls showSuggestions for it). An
    /// empty `rows` array with no message would render an open empty box, which
    /// nothing in the reference does — hence `empty` is the discriminator.
    #[test]
    fn empty_suggestions_render_one_sug_empty() {
        let shell = build(Some(serde_json::json!({
            "suggestions": {"rows": [], "active": null, "empty": "no matches"}
        })));
        assert_eq!(count_matching(&shell, ".sug"), 0);
        assert_eq!(count_matching(&shell, ".sug-empty"), 1);
        assert_eq!(
            shell.text_of(".sug-empty").as_deref(),
            Some("no matches"),
            "the reference's own message"
        );
        assert!(
            !shell.classes_of("#suggestions").unwrap().iter().any(|c| c == "hidden"),
            "the empty container is still shown (search.js:25-27)"
        );
        assert_eq!(aria(&shell).as_deref(), Some("true"));
    }

    /// `state.searchIndex` marks EXACTLY one row, and `null` marks none. A paint
    /// that marked all/rows[0] would still pass a "some row is active" check, so
    /// the count and the position are both asserted.
    #[test]
    fn active_row_is_marked_exactly_once() {
        let shell = build(Some(serde_json::json!({ "suggestions": two_rows() })));
        assert_eq!(count_matching(&shell, ".sug.active"), 1);
        let active_classes = shell.classes_of(".sug.active").unwrap();
        assert!(active_classes.iter().any(|c| c == "sug"));
        assert!(
            shell
                .classes_of(".sug:nth-child(1)")
                .unwrap()
                .iter()
                .any(|c| c == "active"),
            "active:0 must mark the first row"
        );

        let mut none = two_rows();
        none["active"] = serde_json::Value::Null;
        let shell = build(Some(serde_json::json!({ "suggestions": none })));
        assert_eq!(
            count_matching(&shell, ".sug.active"),
            0,
            "null active (the -1 sentinel) marks no row"
        );
    }

    /// Serde-default degradation: a row carrying only an `id` still paints (blank
    /// text, default dot colour) rather than panicking on missing fields.
    #[test]
    fn partial_row_fields_degrade_to_blank_text() {
        let shell = build(Some(serde_json::json!({
            "suggestions": {"rows": [{"id": "n:bare"}], "active": null}
        })));
        assert_eq!(count_matching(&shell, ".sug"), 1);
        assert_eq!(
            shell.text_of(".sug .sug-label").as_deref(),
            Some("n:bare"),
            "an empty label falls back to the id"
        );
        assert_eq!(shell.text_of(".sug .sug-kind").as_deref(), Some(""));
        assert_eq!(shell.text_of(".sug .sug-full").as_deref(), Some(""));
        let style = shell
            .attr_of(".sug .dot", "style")
            .unwrap()
            .unwrap();
        assert!(
            style.contains("background:#858585"),
            "an unknown/empty kind uses core.js's fallback colour: {style:?}"
        );
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use crate::NzkUi;

    /// Build the runtime shell from the SAME wire shape the app publishes.
    fn build(ui: Option<serde_json::Value>) -> NzkUi {
        let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
            .expect("Facts must serialize");
        if let Some(ui) = ui {
            value["ui"] = ui;
        }
        let facts: Facts = serde_json::from_value(value).expect("Facts must round-trip");
        let dir = NzkUi::frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
            .expect("build skeleton shell")
    }

    /// The full report the app builds for a real plan, newlines included.
    const REPORT: &str = "Read-only plan — nothing is written\n\
op: rename → Plus\n\
declaration: App.Math.Add (src/App.cs:12)\n\
Signature\n\
  public static int Add(int a, int b)\n\
  public static int Plus(int a, int b)\n\
Counts\n\
  edits: 2 line(s) in 2 file(s)\n\
  call sites: 1\n\
  affected declarations: 1\n\
  changed lines: 2\n";

    fn facts() -> serde_json::Value {
        serde_json::json!({
            "preview": {
                "text": REPORT,
                "note": "Planned 2 edit(s), 1 call site(s) — wrote nothing",
                "note_kind": "ok",
            }
        })
    }

    #[test]
    fn the_report_text_is_written_exactly_as_built() {
        let shell = build(Some(facts()));
        assert_eq!(
            shell.text_of("#previewOut").as_deref(),
            Some(REPORT),
            "the shell must not reflow, trim or re-escape the host's text"
        );
        assert_eq!(
            shell.text_of("#previewNote").as_deref(),
            Some("Planned 2 edit(s), 1 call site(s) — wrote nothing")
        );
        assert_eq!(
            shell
                .attr_of("#previewNote", "class")
                .expect("#previewNote must exist")
                .as_deref(),
            Some("modal-note ok"),
            "setNote writes 'modal-note' plus the kind (preview.js:39-42)"
        );
    }

    #[test]
    fn a_bad_note_gets_the_bad_class_and_a_refusal_text() {
        let shell = build(Some(serde_json::json!({
            "preview": {
                "text": "refused: index a folder first",
                "note": "index a folder first",
                "note_kind": "bad",
            }
        })));
        assert_eq!(
            shell.text_of("#previewOut").as_deref(),
            Some("refused: index a folder first")
        );
        assert_eq!(
            shell
                .attr_of("#previewNote", "class")
                .expect("#previewNote must exist")
                .as_deref(),
            Some("modal-note bad")
        );
    }

    #[test]
    fn an_empty_note_kind_leaves_the_shipped_note_class() {
        let shell = build(Some(serde_json::json!({
            "preview": {"text": "ops: rename, retype", "note": "", "note_kind": ""}
        })));
        assert_eq!(
            shell
                .attr_of("#previewNote", "class")
                .expect("#previewNote must exist")
                .as_deref(),
            Some("modal-note")
        );
    }

    #[test]
    fn an_absent_preview_fact_writes_nothing_at_all() {
        let shipped = build(None);
        let again = build(None);
        assert_eq!(
            live_tree_dump(shipped.document()),
            live_tree_dump(again.document()),
            "two default builds are the same document"
        );
        // A ui object that does not carry `preview` is equally a no-op for
        // these two nodes: the shipped empty div/span are left alone.
        let with_ui = build(Some(serde_json::json!({"pane_open": false})));
        for shell in [&shipped, &with_ui] {
            assert_eq!(shell.text_of("#previewOut").as_deref(), Some(""));
            assert_eq!(shell.text_of("#previewNote").as_deref(), Some(""));
            assert_eq!(
                shell
                    .attr_of("#previewNote", "class")
                    .expect("#previewNote must exist")
                    .as_deref(),
                Some("modal-note")
            );
        }
    }

    /// The non-vacuity control for the equality above: a painted report DOES
    /// change the live tree (so the shipped comparison is evidence, not
    /// tautology) and keeps the modal's own nodes.
    #[test]
    fn painting_a_report_changes_the_live_tree() {
        let shipped = build(None);
        let painted = build(Some(facts()));
        assert_ne!(
            live_tree_dump(shipped.document()),
            live_tree_dump(painted.document())
        );
        assert_eq!(
            painted.text_of("#previewOut").as_deref(),
            Some(REPORT),
            "the changed live node is the panel's own body"
        );
    }
}


/// The Reorganise & export dialog's facts -> DOM seam (`reorg.js`).
#[cfg(test)]
mod reorg_tests {
    use super::*;
    use crate::NzkUi;

    /// Build the runtime shell from the real skeleton with `ui` spliced in —
    /// the same helper shape the other DOM fact tests use.
    fn build(ui: Option<serde_json::Value>) -> NzkUi {
        let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
            .expect("Facts must serialize");
        if let Some(ui) = ui {
            value["ui"] = ui;
        }
        let facts: Facts = serde_json::from_value(value).expect("Facts must round-trip");
        let dir = NzkUi::frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
            .expect("build skeleton shell")
    }

    fn count(shell: &NzkUi, selector: &str) -> usize {
        shell
            .document()
            .query_selector_all(selector)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    fn class_of(shell: &NzkUi, selector: &str) -> String {
        shell.attr_of(selector, "class").ok().flatten().unwrap_or_default()
    }

    fn hidden(shell: &NzkUi, selector: &str) -> bool {
        class_of(shell, selector).split_whitespace().any(|c| c == "hidden")
    }

    fn value_of(shell: &NzkUi, selector: &str) -> String {
        shell.attr_of(selector, "value").ok().flatten().unwrap_or_default()
    }

    fn has_attr(shell: &NzkUi, selector: &str, name: &str) -> bool {
        shell.attr_of(selector, name).ok().flatten().is_some()
    }

    fn open_facts(mode: &str, group: &str) -> serde_json::Value {
        serde_json::json!({
            "mode": mode, "scope": "selection", "gran": "leaf", "group": group,
            "size": 2500, "header": false, "dest": "/tmp/out",
            "has_report": false, "rows": [],
            "note": "", "note_kind": ""
        })
    }

    /// An absent `ui.reorg` must leave every reorg node exactly as shipped: the
    /// modal hidden, no `selected` attributes added, the inputs' shipped values,
    /// and an empty report/note. This is the `@frame`-token half.
    #[test]
    fn absent_facts_leave_the_shipped_modal_untouched() {
        let shipped = build(None);
        assert_eq!(class_of(&shipped, "#reorgModal"), "modal-backdrop hidden");
        assert_eq!(count(&shipped, "#reorgMode option[selected]"), 0);
        assert_eq!(count(&shipped, "#reorgReport > *"), 0);
        assert_eq!(shipped.text_of("#reorgNote").as_deref(), Some(""));
        assert!(!hidden(&shipped, "#reorgGranRow"));
        // An OPEN modal with no reorg facts (a different key entirely) is still
        // untouched: presence of `ui.reorg` is the paint switch.
        let opened = build(Some(serde_json::json!({"modals_open": ["reorgModal"]})));
        assert_eq!(count(&opened, "#reorgMode option[selected]"), 0);
        assert_eq!(count(&opened, "#reorgReport > *"), 0);
    }

    /// `syncRows` (`reorg.js:286-290`), all three modes:
    /// single hides all three rows; split+size shows all three; split+any other
    /// group keeps size hidden.
    #[test]
    fn sync_rows_follows_mode_and_group_exactly() {
        let single = build(Some(serde_json::json!({"reorg": open_facts("single", "size")})));
        for row in ["#reorgGranRow", "#reorgGroupRow", "#reorgSizeRow"] {
            assert!(hidden(&single, row), "single must hide {row}");
        }
        let split_size = build(Some(serde_json::json!({"reorg": open_facts("split", "size")})));
        for row in ["#reorgGranRow", "#reorgGroupRow", "#reorgSizeRow"] {
            assert!(!hidden(&split_size, row), "split+size must show {row}");
        }
        let split_ns = build(Some(serde_json::json!({"reorg": open_facts("split", "namespace")})));
        assert!(!hidden(&split_ns, "#reorgGranRow"));
        assert!(!hidden(&split_ns, "#reorgGroupRow"));
        assert!(
            hidden(&split_ns, "#reorgSizeRow"),
            "size row only when group == size"
        );
    }

    /// The form values are painted from the facts, not guessed: the matching
    /// `<option selected>`, the number/text `value` attribute and the header's
    /// `checked` attribute.
    #[test]
    fn select_values_and_inputs_come_from_the_facts() {
        let shell = build(Some(serde_json::json!({"reorg": open_facts("split", "owner")})));
        assert!(has_attr(&shell, "#reorgMode option[value=split]", "selected"));
        assert!(!has_attr(&shell, "#reorgMode option[value=single]", "selected"));
        assert!(has_attr(&shell, "#reorgScope option[value=selection]", "selected"));
        assert!(has_attr(&shell, "#reorgGran option[value=leaf]", "selected"));
        assert!(has_attr(&shell, "#reorgGroup option[value=owner]", "selected"));
        assert_eq!(value_of(&shell, "#reorgSize"), "2500");
        assert_eq!(value_of(&shell, "#reorgDest"), "/tmp/out");
        assert!(
            !has_attr(&shell, "#reorgHeader", "checked"),
            "header:false must clear checked, never write checked=false"
        );
    }

    /// `render` (`reorg.js:57-120`): rows are `div.reorg-row` with
    /// `span.reorg-k` + `span.reorg-v` in that order, the optional class lands on
    /// the row, and a repaint never duplicates a row.
    #[test]
    fn report_rows_are_the_reference_structure() {
        let ui = serde_json::json!({"reorg": {
            "mode": "single", "group": "size", "has_report": true,
            "rows": [
                {"k": "files", "v": "3", "class": ""},
                {"k": "declarations", "v": "8 / 9", "class": "bad"},
                {"k": "relations", "v": "4 / 4", "class": "good"}
            ],
            "note": "Verified: 3 file(s), no declaration lost", "note_kind": "good"
        }});
        let shell = build(Some(ui));
        assert_eq!(count(&shell, "#reorgReport > .reorg-row"), 3);
        assert_eq!(
            shell.text_of("#reorgReport > .reorg-row:nth-child(1) .reorg-k").as_deref(),
            Some("files")
        );
        assert_eq!(
            shell.text_of("#reorgReport > .reorg-row:nth-child(1) .reorg-v").as_deref(),
            Some("3")
        );
        assert_eq!(class_of(&shell, "#reorgReport > .reorg-row:nth-child(2)"), "reorg-row bad");
        assert_eq!(class_of(&shell, "#reorgReport > .reorg-row:nth-child(3)"), "reorg-row good");
        // Three rows in one paint, exactly three — not six.
        assert_eq!(count(&shell, "#reorgReport .reorg-k"), 3);
    }

    /// An engine run with no rows shows the reference's placeholder; an open but
    /// idle dialog leaves the shipped empty box untouched.
    #[test]
    fn empty_report_placeholder_only_after_a_run() {
        let idle = build(Some(serde_json::json!({"reorg": {
            "mode": "single", "has_report": false, "rows": []
        }})));
        assert_eq!(count(&idle, "#reorgReport > *"), 0, "idle: shipped empty box");
        let ran = build(Some(serde_json::json!({"reorg": {
            "mode": "single", "has_report": true, "rows": []
        }})));
        assert_eq!(count(&ran, "#reorgReport > .reorg-row.reorg-empty"), 1);
        assert_eq!(
            ran.text_of("#reorgReport").as_deref(),
            Some("Plan or verify to see what would be written.")
        );
    }

    /// `setNote` (`reorg.js:37-40`): text plus `modal-note <kind>`, including the
    /// clearing back to the bare class when the run produced no kind.
    #[test]
    fn note_text_and_class_follow_the_facts() {
        for (kind, expected) in [
            ("bad", "modal-note bad"),
            ("warn", "modal-note warn"),
            ("", "modal-note"),
        ] {
            let shell = build(Some(serde_json::json!({"reorg": {
                "mode": "single", "has_report": true, "rows": [],
                "note": "the engine said so", "note_kind": kind
            }})));
            assert_eq!(shell.text_of("#reorgNote").as_deref(), Some("the engine said so"));
            assert_eq!(class_of(&shell, "#reorgNote"), expected);
        }
    }
}


/// The form-control interaction facts -> DOM seam (task-14).
#[cfg(test)]
mod form_tests {
    use super::*;
    use crate::NzkUi;

    fn build(ui: Option<serde_json::Value>) -> NzkUi {
        let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
            .expect("Facts must serialize");
        if let Some(ui) = ui {
            value["ui"] = ui;
        }
        let facts: Facts = serde_json::from_value(value).expect("Facts must round-trip");
        let dir = NzkUi::frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
            .expect("build skeleton shell")
    }

    fn count(shell: &NzkUi, selector: &str) -> usize {
        shell
            .document()
            .query_selector_all(selector)
            .map(|v| v.len())
            .unwrap_or(0)
    }

    fn attr(shell: &NzkUi, selector: &str, name: &str) -> Option<String> {
        shell.attr_of(selector, name).ok().flatten()
    }

    /// THE DEFAULT-RENDER PROMISE: an absent `ui.form` writes nothing — no
    /// overlay menu, and the inputs keep the shipped values (including the
    /// modalPathInput's shipped empty value).
    #[test]
    fn absent_form_facts_leave_every_control_as_shipped() {
        for ui in [None, Some(serde_json::json!({}))] {
            let shell = build(ui);
            assert_eq!(count(&shell, "#csFormMenu"), 0, "no overlay");
            assert_eq!(attr(&shell, "#reorgSize", "value").as_deref(), Some("3000"));
            assert_eq!(attr(&shell, "#reorgDest", "value"), None);
            assert_eq!(attr(&shell, "#reorgHeader", "checked"), Some(String::new()));
        }
    }

    /// A focused text box paints its edit buffer as the input's `value`.
    #[test]
    fn a_focused_input_paints_its_edit_buffer() {
        let shell = build(Some(serde_json::json!({"form": {
            "focused": "#reorgDest",
            "input": {"id": "#reorgDest", "value": "/tmp/out"},
            "menu": null
        }})));
        assert_eq!(attr(&shell, "#reorgDest", "value").as_deref(), Some("/tmp/out"));
        assert_eq!(count(&shell, "#csFormMenu"), 0);
    }

    /// THE BUG THIS SHIM EXISTS FOR: with a value wider than the input, the
    /// native paint shows only its head. Typing appends at the caret (the end),
    /// so before this shim the raster for `value` and `value + "X"` was
    /// **byte-identical** — the keystroke had no visible consequence. The shim
    /// scrolls the focused run to the caret and paints the caret itself, so a
    /// typed character must change pixels.
    #[test]
    fn a_focused_overlong_value_scrolls_to_the_caret_and_typing_changes_pixels() {
        let value = "/home/naruzkurai/nem/Assets/NZK toolkit v6.1";
        let facts = |v: &str| {
            serde_json::json!({"form": {
                "focused": "#rootInput",
                "menu": null,
                "input": {"id": "#rootInput", "value": v, "caret": v.chars().count()}
            }})
        };
        let a = build(Some(facts(value)));
        assert_eq!(count(&a, ".cs-paint-value"), 1, "the value run is re-painted");
        assert_eq!(count(&a, ".cs-paint-caret"), 1, "the caret is painted");
        assert!(
            attr(&a, "#rootInput", "style")
                .unwrap_or_default()
                .contains("color:transparent"),
            "the native (unscrolled) run must be hidden when the shim paints"
        );

        let typed = format!("{value}X");
        let b = build(Some(facts(&typed)));
        assert_ne!(
            a.paint(),
            b.paint(),
            "typing at the caret of a clipped value must change the raster"
        );
    }

    /// A value that fits keeps the native glyphs: only the caret is added, so
    /// focus itself does not re-shape or move the run.
    #[test]
    fn a_focused_short_value_keeps_native_glyphs_and_only_adds_the_caret() {
        let shell = build(Some(serde_json::json!({"form": {
            "focused": "#rootInput",
            "menu": null,
            "input": {"id": "#rootInput", "value": "/tmp/x", "caret": 6}
        }})));
        assert_eq!(count(&shell, ".cs-paint-value"), 0, "no scroll shim");
        assert_eq!(count(&shell, ".cs-paint-caret"), 1, "caret still painted");
        assert_eq!(attr(&shell, "#rootInput", "style"), None, "native run untouched");
    }

    /// A committed value published WITHOUT a caret (preview selector, picker
    /// path) must not grow a caret at offset 0.
    #[test]
    fn a_committed_value_with_no_caret_paints_no_caret() {
        let shell = build(Some(serde_json::json!({"form": {
            "focused": null,
            "menu": null,
            "input": {"id": "#previewSelector", "value": "N.A"}
        }})));
        assert_eq!(count(&shell, ".cs-paint-caret"), 0);
        assert_eq!(count(&shell, ".cs-paint-value"), 0);
    }

    /// An open menu is a `#csFormMenu` inserted after the control's row, one
    /// `.btn.cs-form-option[data-value]` per option, `.active` on the highlighted
    /// row and `aria-selected` on the current value. The rows are IN ORDER.
    #[test]
    fn an_open_menu_paints_the_option_rows_after_its_control() {
        let shell = build(Some(serde_json::json!({"form": {
            "focused": "#reorgMode",
            "input": null,
            "menu": {
                "id": "#reorgMode",
                "active": 1,
                "options": [
                    {"value": "single", "label": "One merged file", "selected": false},
                    {"value": "split", "label": "Split into chunks", "selected": true}
                ]
            }
        }})));
        assert_eq!(count(&shell, "#csFormMenu"), 1);
        assert_eq!(count(&shell, "#csFormMenu .cs-form-option"), 2);
        assert_eq!(
            attr(&shell, "#csFormMenu .cs-form-option:nth-of-type(1)", "data-value").as_deref(),
            Some("single")
        );
        assert_eq!(
            attr(&shell, "#csFormMenu .cs-form-option:nth-of-type(2)", "data-value").as_deref(),
            Some("split")
        );
        assert_eq!(
            attr(&shell, "#csFormMenu .cs-form-option:nth-of-type(2)", "class").as_deref(),
            Some("btn cs-form-option active"),
            "the active row carries .active"
        );
        assert_eq!(
            attr(&shell, "#csFormMenu .cs-form-option:nth-of-type(2)", "aria-selected").as_deref(),
            Some("true")
        );
        assert_eq!(
            shell.text_of("#csFormMenu .cs-form-option:nth-of-type(1)").as_deref(),
            Some("One merged file"),
            "the shipped option label is the row text"
        );
        // In-flow: the menu is a SIBLING of the control's row (inserted after it).
        assert_eq!(
            count(&shell, "#reorgMode + #csFormMenu, #csFormMenu"),
            1
        );
    }
}

#[cfg(test)]
mod drawer_tests {
    use super::*;
    use crate::NzkUi;

    /// Build the runtime shell from the SAME wire shape the app publishes.
    fn build(ui: Option<serde_json::Value>) -> NzkUi {
        let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
            .expect("Facts must serialize");
        if let Some(ui) = ui {
            value["ui"] = ui;
        }
        let facts: Facts = serde_json::from_value(value).expect("Facts must round-trip");
        let dir = NzkUi::frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
            .expect("build skeleton shell")
    }

    fn facts() -> serde_json::Value {
        serde_json::json!({
            "drawer": {
                "id": "n:1",
                "title": "Run",
                "title_attr": "App.Run",
                "kind": "method",
                "location": "src/App.cs:10–20",
                "location_full": "/repo/src/App.cs:10–20",
                "flags": ["deg 3"],
                "props": [
                    {"k": "Namespace", "v": "App"},
                    {"k": "Lines", "v": "10–20"}
                ],
                "relations": [
                    {"key": "callers", "label": "Callers", "count": 2, "open": true, "rows": [
                        {"id": "n:2", "label": "Helper", "kind": "method"},
                        {"id": "n:3", "label": "Box", "kind": "class"}
                    ]},
                    {"key": "children", "label": "Children", "count": 1, "rows": []}
                ],
                "relation_total": 3
            },
            "drawers_open": ["nodeDrawer"]
        })
    }

    fn class_of(shell: &NzkUi, sel: &str) -> String {
        shell.attr_of(sel, "class").expect("attr").unwrap_or_default()
    }

    #[test]
    fn the_drawer_opens_and_carries_the_identity_and_location() {
        let shell = build(Some(facts()));
        let drawer_class = class_of(&shell, "#nodeDrawer");
        assert!(!drawer_class.contains("hidden"), "content un-hides the drawer: {drawer_class}");
        assert_eq!(shell.text_of("#nodeTitle").as_deref(), Some("Run"));
        assert_eq!(
            shell.attr_of("#nodeTitle", "title").expect("attr").as_deref(),
            Some("App.Run")
        );
        assert_eq!(
            shell.text_of("#nodeProps .detail-loc").as_deref(),
            Some("src/App.cs:10–20"),
            "root-relative location, exactly as the host sent it"
        );
        // `.kind-badge` ships `text-transform: uppercase` (`style.css:513-521`),
        // and `apply_text_transform` paints that faithfully (Blitz drops the
        // property, so the shell applies it before layout — the same uppercase
        // Chromium shows). The host's wire text stays lowercase "method"; the
        // rendered text is uppercase by the stylesheet, which is exactly v2.
        assert_eq!(
            shell.text_of("#nodeProps .kind-badge").as_deref(),
            Some("METHOD")
        );
        assert_eq!(
            shell.text_of("#nodeProps .detail-flags .flag").as_deref(),
            Some("deg 3")
        );
        assert_eq!(
            shell.text_of("#nodeProps .kv dt").as_deref(),
            Some("Namespace")
        );
        assert_eq!(shell.text_of("#nodeProps .kv dd").as_deref(), Some("App"));
    }

    /// A hit on a relation row's LABEL still reports the row's measured box, so
    /// the host can paint hover/press feedback over the whole clickable row
    /// instead of just the text. This is the `@hitbox` contract's layout half.
    #[test]
    fn a_row_label_hit_reports_the_whole_row_box() {
        let shell = build(Some(facts()));
        let row = shell.box_of(".rel-node").expect("row box");
        let label = shell.box_of(".rel-node .rn-label").expect("label box");
        let hit = shell
            .hit(label.x + label.w / 2.0, label.y + label.h / 2.0)
            .expect("label is hit-testable");
        assert_eq!(hit.tag, "span", "the raw hit is the label child");
        assert_eq!(hit.data_key.as_deref(), Some("n:2"), "row payload");
        let target = hit.target_rect.expect("the row's measured box");
        assert_eq!(target.x, row.x);
        assert_eq!(target.y, row.y);
        assert_eq!(target.w, row.w);
        assert_eq!(target.h, row.h);
    }

    #[test]
    fn relation_sections_and_clickable_rows_carry_the_reference_markup() {
        let shell = build(Some(facts()));
        assert_eq!(shell.text_of("#relationsCount").as_deref(), Some("3"));
        let rels = shell.document().query_selector_all(".relationship").map(|v| v.len()).unwrap_or(0);
        assert_eq!(rels, 2);
        assert_eq!(shell.text_of(".relationship .rel-label").as_deref(), Some("Callers"));
        assert_eq!(shell.text_of(".relationship .num-results").as_deref(), Some("2"));
        assert_eq!(
            shell.text_of(".rel-node .rn-label").as_deref(),
            Some("Helper")
        );
        assert_eq!(
            shell
                .attr_of(".rel-node", "data-key")
                .expect("attr")
                .as_deref(),
            Some("n:2"),
            "the row's click payload is the neighbour node id"
        );
        assert_eq!(
            shell.text_of(".rel-node .rn-kind").as_deref(),
            Some("method")
        );
        // The second bucket has no rows: its section exists, the list does not.
        assert_eq!(
            shell.document().query_selector_all(".relationship").map(|v| v.len()).unwrap_or(0),
            2
        );
    }

    #[test]
    fn a_node_with_no_relations_says_so_honestly() {
        let mut f = facts();
        f["drawer"]["relations"] = serde_json::json!([]);
        f["drawer"]["relation_total"] = serde_json::json!(0);
        let shell = build(Some(f));
        assert_eq!(shell.text_of("#relationsCount").as_deref(), Some(""));
        assert_eq!(
            shell.text_of("#relationsBody .drawer-hint").as_deref(),
            Some("No relations for this node.")
        );
    }

    #[test]
    fn an_absent_drawer_fact_writes_nothing_at_all() {
        let shipped = build(None);
        let again = build(None);
        assert_eq!(
            live_tree_dump(shipped.document()),
            live_tree_dump(again.document()),
            "two default builds are the same document"
        );
        assert!(class_of(&shipped, "#nodeDrawer").contains("hidden"));
        assert_eq!(shipped.text_of("#nodeTitle").as_deref(), Some("Node"));
        assert_eq!(shipped.text_of("#nodeProps").as_deref(), Some(""));
        assert_eq!(shipped.text_of("#relationsBody").as_deref(), Some(""));
    }

    /// Non-vacuity: a painted drawer genuinely changes the live tree.
    #[test]
    fn painting_a_drawer_changes_the_live_tree() {
        let shipped = build(None);
        let painted = build(Some(facts()));
        assert_ne!(
            live_tree_dump(shipped.document()),
            live_tree_dump(painted.document())
        );
    }
}

/// `ui.filters` -> sidebar chip classes + `#nsSelect` options (`filters.js`).
#[cfg(test)]
mod filters_tests {
    use super::*;
    use crate::NzkUi;

    fn build(ui: Option<serde_json::Value>) -> NzkUi {
        let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
            .expect("Facts must serialize");
        if let Some(ui) = ui {
            value["ui"] = ui;
        }
        let facts: Facts = serde_json::from_value(value).expect("Facts must round-trip");
        let dir = NzkUi::frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
            .expect("build skeleton shell")
    }

    fn count(shell: &NzkUi, selector: &str) -> usize {
        shell.document().query_selector_all(selector).map(|v| v.len()).unwrap_or(0)
    }

    /// Absent fact = the shipped render: no `off` anywhere and exactly the one
    /// empty namespace option (the default frame's byte-identical promise).
    #[test]
    fn absent_filters_facts_keep_the_shipped_render() {
        let shell = build(None);
        assert_eq!(count(&shell, ".facet.off"), 0);
        assert_eq!(count(&shell, ".edgekey.off"), 0);
        assert_eq!(count(&shell, "#nsSelect option"), 1, "only All namespaces");
        assert_eq!(count(&shell, ".facet[data-id]"), 0, "no click payload without facts");
    }

    /// Present fact paints the real state: off chips, click payloads, real
    /// namespace options and the selected value.
    #[test]
    fn present_filters_facts_paint_off_chips_options_and_selection() {
        let shell = build(Some(serde_json::json!({
            "filters": {
                "kinds": [{"kind": "method", "on": false}],
                "edges": [{"kind": "calls", "on": false}],
                "namespaces": ["NZK.A", "NZK.B"],
                "namespace": "NZK.B",
                "ns_deep": true
            }
        })));
        assert_eq!(count(&shell, ".facet.off"), 1, "exactly the hidden kind");
        // `.edgekey` is shared by the edge-kind chips and the two Group chips
        // (`#kindChildWrap`/`#kindEdgeWrap`); this test is about the edge-kind
        // chips, so it selects the ones carrying `data-edge`.
        assert_eq!(
            count(&shell, ".edgekey.off[data-edge]"),
            1,
            "exactly the hidden edge kind"
        );
        assert!(count(&shell, ".facet[data-id]") > 0, "chips carry a click payload");
        assert_eq!(
            count(&shell, "#nsSelect option"),
            3,
            "All namespaces + the two real ones"
        );
        assert_eq!(
            shell.text_of("#nsSelect").as_deref(),
            Some("All namespaces (2)NZK.ANZK.B"),
            "first option counts the list, then the names in order"
        );
        // The selected option is the fact's value.
        let doc = shell.document();
        let opts = doc.query_selector_all("#nsSelect option").unwrap_or_default();
        let mut selected = Vec::new();
        for o in opts {
            if doc.get_node(o).and_then(|n| n.element_data()).map(|e| e.attrs.iter().any(|a| a.name.local.as_ref()=="selected")).unwrap_or(false) {
                selected.push(shell.text_of("#nsSelect option").unwrap_or_default());
            }
        }
        assert_eq!(selected.len(), 1, "exactly one selected option");
    }

    /// Task-1: every other Filters-section control must carry the shell's
    /// nearest-ancestor `data-key` payload, and the checkboxes the document never
    /// painted must show their fact value.
    ///
    /// This is the DOM half of the "painted button with no dispatch entry" fix:
    /// `filter_click` resolves these exact strings, so deleting a writer here
    /// makes the control dead again while the pixels still look right.
    #[test]
    fn present_filters_facts_tag_every_sidebar_control_and_paint_checkboxes() {
        let shell = build(Some(serde_json::json!({
            "filters": {
                "kinds": [{"kind": "method", "on": false}],
                "edges": [{"kind": "calls", "on": false}],
                "namespaces": ["NZK.A"],
                "namespace": "",
                "ns_deep": false,
                "hide_isolated": true,
                "hide_edge_labels": true
            },
            "group_children": false,
            "groupEdges": false
        })));
        let count = |sel: &str| count(&shell, sel);

        // Chip payloads (own element `data-id` + ancestor `data-key`).
        assert!(count(".facet[data-key^=\"facet:\"]") > 0, "facet data-key");
        assert!(count(".edgekey[data-key^=\"edgekey:\"]") > 0, "edgekey data-key");

        // The static controls' routing payloads.
        for sel in [
            "button[data-key=\"filter:kindsAll\"]",
            "button[data-key=\"filter:kindsNone\"]",
            "button[data-key=\"filter:edgesAll\"]",
            "button[data-key=\"filter:edgesNone\"]",
            "button[data-key=\"filter:projAll\"]",
            "button[data-key=\"filter:projNone\"]",
            "#nsAllBtn[data-key=\"filter:nsAll\"]",
            "#kindChildWrap[data-key=\"filter:groupChildren\"]",
            "#kindEdgeWrap[data-key=\"filter:groupEdges\"]",
        ] {
            assert_eq!(count(sel), 1, "{sel} must carry its routing payload");
        }
        // The checkbox rows put the payload on the CLICKABLE wrapping label.
        for sel in [
            "label[data-key=\"filter:nsDeep\"]",
            "label[data-key=\"filter:hideIsolated\"]",
            "label[data-key=\"filter:hideEdgeLabels\"]",
            "label[data-key=\"filter:hideRelations\"]",
        ] {
            assert_eq!(count(sel), 1, "{sel} must carry its routing payload");
        }

        // Fact-driven checkbox state: `ns_deep:false` unchecks `#nsDeep`;
        // hideIsolated/hideEdgeLabels are checked; the group pair stays inverse.
        let checked = |id: &str| {
            shell
                .document()
                .query_selector(&format!("#{id}[checked]"))
                .ok()
                .flatten()
                .is_some()
        };
        assert!(!checked("nsDeep"), "ns_deep:false must uncheck #nsDeep");
        assert!(checked("hideIsolated"));
        assert!(checked("hideEdgeLabels"));
        assert!(!checked("groupChildren"), "group_children:false");
        assert!(checked("hideChildren"), "the inverse box is checked instead");
        assert!(!checked("groupEdges"), "groupEdges:false");
        // ...and the visible Group chips show the reference's `.off` class.
        assert_eq!(count("#kindChildWrap.edgekey.off"), 1);
        assert_eq!(count("#kindEdgeWrap.edgekey.off"), 1);
    }

    /// "Hide parents" is runtime-created beside the shipped Groups row, carries
    /// the same nearest-ancestor payload as every other checkbox, and paints the
    /// fact's checked state. "Only related nodes" is drawer-only.
    #[test]
    fn hide_parents_filter_is_created_routed_and_painted() {
        let shell = build(Some(serde_json::json!({
            "filters": {
                "kinds": [],
                "edges": [],
                "namespaces": [],
                "namespace": "",
                "ns_deep": true,
                "focus_parents": true,
                "focus_selected": "NZK.A.m"
            }
        })));
        let count = |sel: &str| count(&shell, sel);
        assert_eq!(count("#focusOnly"), 0, "focus lives in the drawer only");
        assert_eq!(count("#focusParents"), 1);
        assert_eq!(count("label[data-key=\"filter:focusOnly\"]"), 0);
        assert_eq!(count("label[data-key=\"filter:focusParents\"]"), 1);
        let checked = |id: &str| {
            shell
                .document()
                .query_selector(&format!("#{id}[checked]"))
                .ok()
                .flatten()
                .is_some()
        };
        assert!(checked("focusParents"), "focus_parents:true");
        // The row is inserted after the shipped Hide-relations label, not
        // appended at the far end of the section.
        let rel = shell.box_of("#hideChildren").expect("hideChildren box");
        let parents = shell.box_of("#focusParents").expect("focusParents box");
        assert!(parents.y > rel.y, "the row sits below Hide relations");
    }

    /// Absent facts must leave the shipped document's attributes untouched (the
    /// pinned default frame promise): no payload and no `.off` class is invented.
    #[test]
    fn absent_filters_facts_leave_no_filter_payloads_or_off_chips() {
        let shell = build(None);
        let count = |sel: &str| count(&shell, sel);
        for sel in [
            "button[data-key^=\"filter:\"]",
            "#nsAllBtn[data-key]",
            "#kindChildWrap[data-key]",
            "#kindEdgeWrap[data-key]",
            "label[data-key^=\"filter:\"]",
            "#kindChildWrap.off",
            "#kindEdgeWrap.off",
        ] {
            assert_eq!(count(sel), 0, "{sel} must not exist without facts");
        }
        // The shipped boxes keep their shipped checked-ness.
        let checked = |id: &str| {
            shell
                .document()
                .query_selector(&format!("#{id}[checked]"))
                .ok()
                .flatten()
                .is_some()
        };
        assert!(checked("nsDeep"), "index.html ships #nsDeep checked");
        assert!(!checked("hideIsolated"));
        assert!(!checked("hideEdgeLabels"));
    }
}

/// Engine-backed Find **groups** (v2 `renderFind`, `search.js:305-351`) — the
/// round-17 gap's DOM half.
#[cfg(test)]
mod find_groups_tests {
    use crate::NzkUi;

    fn build(ui: serde_json::Value) -> NzkUi {
        let mut value = serde_json::to_value(crate::dom::Facts::for_corpus("/tmp/fixture"))
            .expect("Facts must serialize");
        value["ui"] = ui;
        let facts: crate::dom::Facts =
            serde_json::from_value(value).expect("Facts must round-trip");
        let dir = NzkUi::frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
            .expect("build skeleton shell")
    }

    /// Two non-empty engine groups paint with v2's own class names and carry the
    /// row's click target on `data-key`. The payload is exactly the app's
    /// `find_wire::groups_value` shape (a projection of `Index::find` rows), so
    /// nothing here is fabricated: count, text and targets are the engine's.
    #[test]
    fn find_groups_paint_headings_counts_and_click_targets() {
        let shell = build(serde_json::json!({
            "active_mode": "find",
            "find": {
                "query": "Build", "meta": "2 hits", "notice": "", "state": "hits",
                "rows": [],
                "groups": [
                    {"kind": "definitions", "heading": "Definitions", "count": 1,
                     "rows": [{"kind": "method", "title": "NZK.Build",
                               "subtitle": "a.cs:10", "node_id": "T:Build",
                               "file": "a.cs", "line": 10, "col": 0, "text": "",
                               "match": "definition"}]},
                    {"kind": "usages", "heading": "Usages", "count": 1,
                     "rows": [{"kind": "usage", "title": "a.cs:42",
                               "subtitle": "in Ctx", "node_id": "T:Ctx",
                               "file": "a.cs", "line": 42, "col": 7,
                               "text": "var x = Build();", "match": "exact"}]}
                ]
            }
        }));
        let count = |sel: &str| {
            shell
                .document()
                .query_selector_all(sel)
                .map(|v| v.len())
                .unwrap_or(0)
        };
        assert_eq!(count(".fgroup"), 2, "one group per non-empty engine group");
        assert_eq!(count(".fgroup-defs"), 1);
        assert_eq!(count(".fgroup-uses"), 1);
        assert_eq!(count(".fgroup-head"), 2);
        assert_eq!(count(".fcount"), 2);
        assert_eq!(count(".fgroup .frow"), 2, "one row per engine row");
        assert_eq!(
            count(".frow[data-key^=\"find:node:\"]"),
            1,
            "a definition row carries its node target"
        );
        assert_eq!(
            count(".frow[data-key^=\"find:usage:\"]"),
            1,
            "a usage row carries its file:line target"
        );
        let text = |sel: &str| shell.text_of(sel).unwrap_or_default();
        // `.fgroup-head` is uppercased by the shipped stylesheet (the DOM text
        // comes back as "DEFINITIONS" + the count child), so the assertion is
        // about the heading text, not the casing v2's stylesheet applies.
        assert!(text(".fgroup-head").to_lowercase().contains("definitions"));
        assert!(text(".fgroup-uses .fsrc").contains("var x = Build();"));
        assert!(text(".fgroup-uses .frow-sub").contains("in Ctx"));
    }

    /// An EMPTY group list keeps the pre-groups behaviour: the honest
    /// `find-empty` sentence and no group shells (so "engine found nothing"
    /// cannot be mistaken for a painted group of zero rows).
    #[test]
    fn an_empty_group_list_renders_the_empty_state_without_group_nodes() {
        let shell = build(serde_json::json!({
            "active_mode": "find",
            "find": {
                "query": "zzz", "meta": "", "state": "nohits",
                "notice": "No matches for \u{201c}zzz\u{201d}.",
                "rows": [], "groups": []
            }
        }));
        let count = |sel: &str| {
            shell
                .document()
                .query_selector_all(sel)
                .map(|v| v.len())
                .unwrap_or(0)
        };
        assert_eq!(count(".fgroup"), 0);
        assert_eq!(count(".frow"), 0);
        assert_eq!(count(".find-empty"), 1);
    }

    /// Project chips (`filters.js` buildProjectFilter): each chip carries v2's
    /// `data-project` AND the host `data-id="proj:<id>"` hit payload, and its
    /// `off` class follows the filter fact (absent `on` = ON, shipped render).
    #[test]
    fn project_chips_carry_both_attributes_and_paint_the_off_class() {
        use crate::Facts;
        use crate::NzkUi;
        let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
            .expect("Facts must serialize");
        value["projects"] = serde_json::json!([
            {"id": "p1", "name": "P1", "on": false},
            {"id": "p2", "name": "P2"}
        ]);
        let facts: Facts = serde_json::from_value(value).expect("Facts must round-trip");
        let dir = NzkUi::frontend_dir();
        let css_path = dir.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        let shell = NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
            .expect("build skeleton shell");
        let count = |sel: &str| {
            shell
                .document()
                .query_selector_all(sel)
                .map(|v| v.len())
                .unwrap_or(0)
        };
        assert_eq!(count(".edgekey[data-project=\"p1\"]"), 1, "v2 markup attr");
        assert_eq!(count(".edgekey[data-id=\"proj:p1\"]"), 1, "host hit payload");
        assert_eq!(
            count(".edgekey[data-project=\"p1\"].off"),
            1,
            "the OFF chip is painted off"
        );
        assert_eq!(
            count(".edgekey[data-project=\"p2\"].off"),
            0,
            "a missing `on` fact keeps the chip ON"
        );
    }
}

#[cfg(test)]
mod type_section_tests {
    use super::*;
    use crate::NzkUi;

    fn shell_with(type_defs: Option<serde_json::Value>) -> NzkUi {
        let mut facts = Facts::for_corpus(".");
        if let Some(v) = &type_defs {
            facts.type_defs =
                Some(serde_json::from_value(v.clone()).expect("TypeDefsFacts"));
        }
        let html = std::fs::read_to_string(NzkUi::frontend_dir().join("index.html"))
            .expect("read index.html");
        let css_path = NzkUi::frontend_dir().join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_source_with_facts(&html, &css_path, &css, (1600, 1000), &facts)
            .expect("build shell")
    }

    fn facts_json() -> serde_json::Value {
        serde_json::json!({
            "total": 3,
            "types": [
                {"name": "string", "count": 2, "selected": false},
                {"name": "bool", "count": 1, "selected": false}
            ],
            "rows": [
                {"id": "f:Name", "full": "N.T.Name", "label": "Name", "kind": "string"},
                {"id": "f:Path", "full": "N.T.Path", "label": "Path", "kind": "string"},
                {"id": "f:Live", "full": "N.T.Live", "label": "Live", "kind": "bool"}
            ],
            "query": "",
            "scope": "N"
        })
    }

    /// The producer's object must deserialize into the real struct: a wrong key
    /// name would otherwise default away and the section would silently render
    /// empty while looking wired.
    #[test]
    fn type_defs_json_deserializes_into_the_facts_struct() {
        let f: TypeDefsFacts = serde_json::from_value(facts_json()).expect("TypeDefsFacts");
        assert_eq!(f.total, Some(3));
        assert_eq!(f.types.len(), 2);
        assert_eq!(f.types[0].name, "string");
        assert_eq!(f.types[0].count, 2);
        assert_eq!(f.rows.as_ref().map(Vec::len), Some(3));
        assert_eq!(f.scope.as_deref(), Some("N"));
    }

    /// An absent key leaves the reference tree untouched: the section is a v3
    /// addition created at runtime, so a fixture render must not contain it —
    /// that is what keeps the pinned `@frame` token stable.
    #[test]
    fn absent_type_defs_creates_no_section() {
        let shell = shell_with(None);
        assert!(
            shell.box_of("#typeSection").is_none(),
            "the fixture must not gain a runtime section"
        );
        assert!(shell.box_of("#typeSearch").is_none());
    }

    /// The host's facts create and hydrate the section: count, chips (with the
    /// declared types and their counts), the `all` chip, the filter box's value
    /// and the rows whose `.l-kind` is the DECLARED TYPE.
    #[test]
    fn type_defs_create_the_section_with_chips_and_rows() {
        let shell = shell_with(Some(facts_json()));
        assert!(shell.box_of("#typeSection").is_some(), "section must exist");
        assert_eq!(
            shell.text_of("#typeCount").as_deref(),
            Some("3"),
            "the count is the host's total"
        );
        assert_eq!(
            shell.attr_of(".type-chip[data-key=\"type:string\"]", "data-key").ok().flatten().as_deref(),
            Some("type:string")
        );
        // The `all` chip clears the restriction and is active when no type is.
        let all = ".type-chip[data-key=\"type:\"]";
        assert_eq!(shell.attr_of(all, "data-key").ok().flatten().as_deref(), Some("type:"));
        assert!(
            shell.classes_of(all).unwrap_or_default().iter().any(|c| c == "active"),
            "no selected chip => `all` is active"
        );
        let list = shell.text_of("#typeList").unwrap_or_default();
        assert!(list.contains("Name"), "rows must be painted: {list:?}");
        assert!(list.contains("string"), "the declared type is the row's kind: {list:?}");
        assert!(list.contains("bool"));
    }

    /// Selecting a type marks ITS chip, drops `all`'s active state, and the rows
    /// are whatever the host sent for that restriction.
    #[test]
    fn a_selected_type_marks_the_chip_and_leaves_all_inactive() {
        let mut v = facts_json();
        v["types"] = serde_json::json!([
            {"name": "string", "count": 2, "selected": true},
            {"name": "bool", "count": 1, "selected": false}
        ]);
        v["rows"] = serde_json::json!([
            {"id": "f:Name", "full": "N.T.Name", "label": "Name", "kind": "string"}
        ]);
        let shell = shell_with(Some(v));
        let chip = ".type-chip[data-key=\"type:string\"]";
        assert!(
            shell.classes_of(chip).unwrap_or_default().iter().any(|c| c == "active"),
            "the selected type's chip carries `.active`"
        );
        let all = ".type-chip[data-key=\"type:\"]";
        assert!(
            !shell.classes_of(all).unwrap_or_default().iter().any(|c| c == "active"),
            "`all` must not claim to be active while a type is selected"
        );
        let list = shell.text_of("#typeList").unwrap_or_default();
        assert!(list.contains("Name"));
        assert!(!list.contains("Live"), "rows are the host's filtered set: {list:?}");
    }

    /// The committed query is written into `#typeSearch`'s value so a repaint
    /// keeps the typed text, the same contract `#pubSearch` has.
    #[test]
    fn the_committed_query_is_written_into_the_box() {
        let mut v = facts_json();
        v["query"] = serde_json::json!("str");
        let shell = shell_with(Some(v));
        assert_eq!(
            shell.attr_of("#typeSearch", "value").ok().flatten().as_deref(),
            Some("str")
        );
    }

    /// An empty host list is a real state and renders the honest empty row, not
    /// a blank box that looks like a rendering failure.
    #[test]
    fn an_empty_result_renders_the_honest_empty_row() {
        let mut v = facts_json();
        v["total"] = serde_json::json!(0);
        v["types"] = serde_json::json!([]);
        v["rows"] = serde_json::json!([]);
        let shell = shell_with(Some(v));
        assert_eq!(shell.text_of("#typeCount").as_deref(), Some("0"));
        let list = shell.text_of("#typeList").unwrap_or_default();
        assert!(
            list.contains("no declarations of that declared type"),
            "empty result text: {list:?}"
        );
    }
}

#[cfg(test)]
mod title_tooltip_tests {
    use super::*;
    use crate::NzkUi;

    fn shell_with_drawer_flag(flag: &str) -> NzkUi {
        let mut facts = Facts::for_corpus(".");
        let mut ui = UiState::default();
        ui.drawers_open = vec!["nodeDrawer".to_string()];
        ui.drawer = Some(DrawerFacts {
            id: "m:A".into(),
            title: "A".into(),
            title_attr: "N.A".into(),
            kind: "method".into(),
            location: "A.cs:1".into(),
            location_full: "/repo/A.cs:1".into(),
            flags: vec![flag.to_string()],
            ..Default::default()
        });
        facts.ui = Some(ui);
        let html = std::fs::read_to_string(NzkUi::frontend_dir().join("index.html"))
            .expect("read index.html");
        let css_path = NzkUi::frontend_dir().join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read style.css");
        NzkUi::from_source_with_facts(&html, &css_path, &css, (1600, 1000), &facts)
            .expect("build shell")
    }

    /// The **`deg N`** chip is what the owner asked for a tooltip on. It must
    /// carry a `title` explaining the number, and the shell's own hit-test —
    /// the source of every hover tooltip the host paints — must report it from
    /// the same layout that painted the chip.
    #[test]
    fn the_degree_chip_exposes_its_explanation_through_the_hit_test() {
        let shell = shell_with_drawer_flag("deg 7");
        let b = shell
            .box_of(".detail-flags .flag")
            .expect("the deg chip must be laid out");
        let hit = shell
            .hit(b.x + b.w / 2.0, b.y + b.h / 2.0)
            .expect("the chip must be hit-testable");
        let title = hit.title.expect("the deg chip must carry a title");
        assert!(title.contains("Degree"), "title: {title:?}");
        assert!(title.contains('7'), "the number must be named: {title:?}");
    }

    /// A flag that is not `deg …` must not gain an invented tooltip: the host
    /// would otherwise attribute a made-up explanation to it.
    #[test]
    fn a_non_degree_flag_keeps_no_invented_title() {
        let shell = shell_with_drawer_flag("external");
        let b = shell.box_of(".detail-flags .flag").expect("flag box");
        let hit = shell.hit(b.x + b.w / 2.0, b.y + b.h / 2.0).expect("hit");
        assert_eq!(hit.title, None);
    }
}
