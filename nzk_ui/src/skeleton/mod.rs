/*! The shipped nzk_ui as a **Rust-built DOM tree**.

The reference frontend ships its static skeleton in `frontend/index.html` and
nzk_ui used to `read_to_string` that file at runtime. That made the Rust
binaries depend on the reference project's file being present. This module
replaces that disk read: the whole static document is expressed as Rust data
([`Node`]) and constructed node-by-node into the Blitz document with
[`build_document`]. No HTML string is embedded (string-embedding macros are banned for
HTML), and the CSS is still read from a file at runtime (see [`Sheets`]).

The tree mirrors `frontend/index.html` element-for-element (the seven style
sheets the old `inline_stylesheets` splice produced are the only delta: the
`<link rel=stylesheet>` becomes five `<style>` elements at the same position).
Parity is proven by `testsupport`, which parses the real `index.html` and
compares canonical dumps of the element tree; the runtime constructors no
longer touch it. */

use blitz_dom::node::Attribute;
use blitz_dom::{BaseDocument, DocumentConfig, DocumentMutator, LocalName, QualName};

pub mod canvas;
pub mod drawers;
pub mod filters;
pub mod head;
pub mod modals_a;
pub mod modals_b;
pub mod side_rest;
pub mod topbar;

#[cfg(test)]
pub mod testsupport;

/// Element namespace. Almost everything is HTML; `<svg>` subtrees are not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ns {
    Html,
    Svg,
}

/// One node of the static document.
#[derive(Clone, Debug)]
pub enum Node {
    Element {
        ns: Ns,
        tag: &'static str,
        attrs: Vec<(&'static str, &'static str)>,
        children: Vec<Node>,
    },
    Text(String),
}

/// An HTML element.
pub fn el(
    tag: &'static str,
    attrs: &[(&'static str, &'static str)],
    children: Vec<Node>,
) -> Node {
    Node::Element {
        ns: Ns::Html,
        tag,
        attrs: attrs.to_vec(),
        children,
    }
}

/// An SVG element (the shortcuts gear icon is the only consumer).
pub fn svg_el(
    tag: &'static str,
    attrs: &[(&'static str, &'static str)],
    children: Vec<Node>,
) -> Node {
    Node::Element {
        ns: Ns::Svg,
        tag,
        attrs: attrs.to_vec(),
        children,
    }
}

/// A text node. HTML entities are already decoded (`&amp;` -> `&`).
pub fn t(text: impl Into<String>) -> Node {
    Node::Text(text.into())
}

/// The stylesheet text the document is built with. `style` is the real
/// `static/style.css`; the other four are the injected sheets previously
/// spliced into `<head>` by `dom::inline_stylesheets`. They are read from
/// `assets/css/` at runtime, never stored in Rust source.
pub struct Sheets {
    pub style: String,
    pub compat: String,
    pub problems: String,
    pub hierarchy: String,
    pub quickopen: String,
}

impl Sheets {
    /// Load the five sheets. `style` is passed in because `static/style.css`
    /// lives in the reference frontend and is loaded by the caller.
    pub fn load(style: &str) -> Result<Self, String> {
        use crate::dom::assets;
        let read = |name: &str| assets::read_sheet(name);
        Ok(Self {
            style: style.to_string(),
            compat: read(assets::SHEET_NAMES[0])?,
            problems: read(assets::SHEET_NAMES[1])?,
            hierarchy: read(assets::SHEET_NAMES[2])?,
            quickopen: read(assets::SHEET_NAMES[3])?,
        })
    }
}

/// The whole static document: `<html lang="en">` with head and body.
pub fn document(sheets: &Sheets) -> Node {
    el(
        "html",
        &[("lang", "en")],
        vec![
            el("head", &[], head::nodes(sheets)),
            el("body", &[], body_nodes()),
        ],
    )
}

fn body_nodes() -> Vec<Node> {
    let mut body = topbar::nodes();
    let canvas_children = {
        let mut c = canvas::nodes();
        c.extend(drawers::nodes());
        c
    };
    let side_children = {
        let mut s = filters::nodes();
        s.extend(side_rest::nodes());
        s
    };
    body.push(el(
        "main",
        &[("class", "layout")],
        vec![
            el(
                "section",
                &[("id", "canvasWrap"), ("class", "canvas-wrap")],
                canvas_children,
            ),
            el("aside", &[("class", "side")], side_children),
            // `#sourcePane` sits in `main`, after `aside.side`, not inside it
            // (the brief's "children 1-4" was off by one; `</aside>` closes at
            // reference line 361 and the pane follows at line 363).
            side_rest::source_pane(),
        ],
    ));
    body.extend(modals_a::nodes());
    body.extend(modals_b::nodes());
    body
}

fn qualname(ns: Ns, name: &str) -> QualName {
    let ns = match ns {
        Ns::Html => blitz_dom::ns!(html),
        Ns::Svg => blitz_dom::ns!(svg),
    };
    QualName::new(None, ns, LocalName::from(name))
}

/// Build `node` under `parent_id` with the same primitives the HTML parser's
/// tree sink used: `create_element` + `append_children`, and `create_text_node`
/// for text.
fn build_node(mutr: &mut DocumentMutator<'_>, parent_id: usize, node: &Node) {
    match node {
        Node::Text(text) => {
            let id = mutr.create_text_node(text);
            mutr.append_children(parent_id, &[id]);
        }
        Node::Element {
            ns,
            tag,
            attrs,
            children,
        } => {
            let attrs = attrs
                .iter()
                .map(|(name, value)| Attribute {
                    name: crate::attr_qualname(name),
                    value: (*value).to_string(),
                })
                .collect();
            let id = mutr.create_element(qualname(*ns, tag), attrs);
            mutr.append_children(parent_id, &[id]);
            for child in children {
                build_node(mutr, id, child);
            }
        }
    }
}

/// Construct a fresh [`BaseDocument`] whose tree is `root`, using `config`
/// exactly as the `from_html` path did.
pub fn build_document(config: DocumentConfig, root: &Node) -> BaseDocument {
    let mut doc = BaseDocument::new(config);
    let root_id = doc.root_node().id;
    {
        let mut mutr = doc.mutate();
        build_node(&mut mutr, root_id, root);
    }
    doc
}

/// Serialise a [`Node`] tree back to HTML. Used for the `NzkUi::resolved_html`
/// accessor (the old code kept the parsed source there) and for the
/// `--dump-tree`-style debugging story.
pub fn serialize(root: &Node) -> String {
    let mut out = String::new();
    serialize_into(root, &mut out);
    out
}

fn serialize_into(node: &Node, out: &mut String) {
    match node {
        Node::Text(text) => push_escaped_text(text, out),
        Node::Element {
            tag,
            attrs,
            children,
            ..
        } => {
            out.push('<');
            out.push_str(tag);
            for (name, value) in attrs {
                out.push(' ');
                out.push_str(name);
                out.push_str("=\"");
                push_escaped_attr(value, out);
                out.push('"');
            }
            out.push('>');
            if is_void(tag) {
                return;
            }
            for child in children {
                serialize_into(child, out);
            }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
}

fn is_void(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn push_escaped_text(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
}

fn push_escaped_attr(text: &str, out: &mut String) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The **whole** document, node-for-node, against a parse of the real
    /// `frontend/index.html` with the same stylesheets and viewport. This is
    /// the endgame proof: once it is green the runtime no longer needs to read
    /// that file, because the Rust tree is element-for-element identical —
    /// except the deliberately removed inert `tauri-shim.js` script node.
    #[test]
    fn whole_document_matches_reference() {
        let reference = testsupport::Reference::new();
        let sheets = Sheets::load(reference.style()).expect("load the five stylesheets");
        let root = document(&sheets);
        let actual = testsupport::dump_node(&root);
        let actual = actual.trim_end();
        let expected = reference
            .dump_subtree("html")
            .lines()
            .filter(|line| !line.contains("tauri-shim.js"))
            .collect::<Vec<_>>()
            .join("\n");
        let expected = expected.trim_end();
        if expected == actual {
            return;
        }
        let first = expected
            .lines()
            .zip(actual.lines())
            .enumerate()
            .find(|(_, (w, g))| w != g)
            .map(|(i, (w, g))| format!("first difference at line {}:\n  reference: {w}\n  rust:      {g}", i + 1))
            .unwrap_or_else(|| {
                format!(
                    "line counts differ: reference {} vs rust {}",
                    expected.lines().count(),
                    actual.lines().count()
                )
            });
        panic!("the Rust document differs from the reference index.html\n{first}");
    }
}
