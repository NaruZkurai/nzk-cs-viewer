/*! Test-only oracle: parse the **real** `frontend/index.html` and compare its
/// element tree with the Rust-built [`super::Node`] tree.
///
/// Production code never reads `index.html`; this module is `#[cfg(test)]` and
/// exists precisely to prove the Rust tree is element-for-element equivalent to
/// the reference skeleton. Whitespace-only text nodes are ignored on both sides
/// (the reference is pretty-printed; the Rust tree carries only meaningful
/// text). Layout parity on top of structural parity is asserted separately in
/// `lib.rs` by comparing every probe box against a reference-parsed shell. */

use blitz_dom::{BaseDocument, DocumentConfig};
use blitz_html::HtmlDocument;

use super::Node;

/// A `BaseDocument` parsed from the reference `index.html`, exactly the way
/// `NzkUi::build` used to parse it (same stylesheet splice, same config).
pub struct Reference {
    doc: BaseDocument,
    css: String,
}

impl Reference {
    pub fn new() -> Self {
        let fe = crate::NzkUi::frontend_dir();
        let raw = std::fs::read_to_string(fe.join("index.html")).expect("read reference index.html");
        let css_path = fe.join("static/style.css");
        let css = std::fs::read_to_string(&css_path).expect("read reference style.css");
        let html = crate::dom::inline_stylesheets(&raw, &css_path, &css)
            .expect("splice the reference stylesheets");
        let config = DocumentConfig {
            viewport: Some(blitz_traits::shell::Viewport::new(
                1600,
                1000,
                1.0,
                blitz_traits::shell::ColorScheme::Dark,
            )),
            base_url: Some("http://localhost/".to_string()),
            html_parser_provider: Some(std::sync::Arc::new(blitz_html::HtmlProvider)),
            ..Default::default()
        };
        // The parser is only used as the oracle; `NzkUi` itself builds in Rust.
        let doc = HtmlDocument::from_html(&html, config).into_inner();
        Self { doc, css }
    }

    /// The real `static/style.css` the reference document was spliced with.
    pub fn style(&self) -> &str {
        &self.css
    }

    /// Number of nodes matching a selector.
    pub fn count(&self, selector: &str) -> usize {
        self.doc
            .query_selector_all(selector)
            .map(|ids| ids.len())
            .unwrap_or(0)
    }

    fn node_id(&self, selector: &str) -> Option<usize> {
        self.doc.query_selector(selector).ok().flatten()
    }

    /// Canonical dump of the subtree rooted at `selector`.
    pub fn dump_subtree(&self, selector: &str) -> String {
        let id = self
            .node_id(selector)
            .unwrap_or_else(|| panic!("reference has no element matching {selector:?}"));
        let mut out = String::new();
        self.dump_into(id, 0, &mut out);
        out
    }

    /// One canonical dump per element/text child of `selector`, skipping
    /// whitespace-only text nodes -- i.e. the list a region module returns.
    pub fn dump_children(&self, selector: &str) -> Vec<String> {
        let id = self
            .node_id(selector)
            .unwrap_or_else(|| panic!("reference has no element matching {selector:?}"));
        let node = self.doc.get_node(id).expect("reference node");
        let mut out = Vec::new();
        for &child_id in &node.children {
            let child = self.doc.get_node(child_id).expect("reference child");
            match &child.data {
                blitz_dom::NodeData::Text(text) => {
                    let trimmed = text.content.trim();
                    if !trimmed.is_empty() {
                        out.push(format!("text={trimmed:?}"));
                    }
                }
                blitz_dom::NodeData::Comment => {}
                blitz_dom::NodeData::Element(_) | blitz_dom::NodeData::AnonymousBlock(_) => {
                    let mut s = String::new();
                    self.dump_into(child_id, 0, &mut s);
                    out.push(s.trim_end().to_string());
                }
                blitz_dom::NodeData::Document => {}
            }
        }
        out
    }

    fn dump_into(&self, id: usize, depth: usize, out: &mut String) {
        let node = self.doc.get_node(id).expect("reference node");
        let indent = "  ".repeat(depth);
        match &node.data {
            blitz_dom::NodeData::Text(text) => {
                let trimmed = text.content.trim();
                if !trimmed.is_empty() {
                    out.push_str(&indent);
                    out.push_str(&format!("text={trimmed:?}\n"));
                }
            }
            blitz_dom::NodeData::Comment | blitz_dom::NodeData::Document => {}
            blitz_dom::NodeData::Element(el) | blitz_dom::NodeData::AnonymousBlock(el) => {
                out.push_str(&indent);
                let ns = if el.name.ns == blitz_dom::ns!(html) {
                    "html"
                } else if el.name.ns == blitz_dom::ns!(svg) {
                    "svg"
                } else {
                    "other"
                };
                out.push_str(&format!("{}:{}", ns, el.name.local));
                let mut attrs: Vec<(String, String)> = el
                    .attrs
                    .iter()
                    .map(|a| (a.name.local.to_string(), a.value.clone()))
                    .collect();
                attrs.sort();
                out.push('[');
                for (i, (name, value)) in attrs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&format!("{name}={value:?}"));
                }
                out.push_str("]\n");
                for &child_id in &node.children {
                    self.dump_into(child_id, depth + 1, out);
                }
            }
        }
    }
}

/// Canonical dump of a Rust-built subtree, in the same format as
/// [`Reference::dump_subtree`].
pub fn dump_node(node: &Node) -> String {
    let mut out = String::new();
    dump_node_into(node, 0, &mut out);
    out
}

fn dump_node_into(node: &Node, depth: usize, out: &mut String) {
    let indent = "  ".repeat(depth);
    match node {
        Node::Text(text) => {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                out.push_str(&indent);
                out.push_str(&format!("text={trimmed:?}\n"));
            }
        }
        Node::Element {
            ns,
            tag,
            attrs,
            children,
        } => {
            out.push_str(&indent);
            let ns = match ns {
                super::Ns::Html => "html",
                super::Ns::Svg => "svg",
            };
            out.push_str(&format!("{ns}:{tag}"));
            let mut attrs: Vec<(String, String)> = attrs
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect();
            attrs.sort();
            out.push('[');
            for (i, (name, value)) in attrs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&format!("{name}={value:?}"));
            }
            out.push_str("]\n");
            for child in children {
                dump_node_into(child, depth + 1, out);
            }
        }
    }
}

/// Assert one Rust-built subtree equals the reference subtree for `selector`.
pub fn assert_subtree_match(node: &Node, selector: &str) {
    let reference = Reference::new();
    let expected = reference.dump_subtree(selector);
    let actual = dump_node(node);
    assert_same(&expected, &actual, selector);
}

/// Assert `nodes` equals the reference's children of `selector`, starting at
/// child index `skip`. (`skip` lets a region own a slice of a parent's children
/// without waiting for the sibling regions.)
pub fn assert_children_match(nodes: &[Node], selector: &str, skip: usize) {
    let reference = Reference::new();
    let all = reference.dump_children(selector);
    let end = skip + nodes.len();
    if all.len() < end {
        panic!(
            "{selector:?} has only {} element children; skipping {skip} and taking {} needs {end}",
            all.len(),
            nodes.len()
        );
    }
    let expected = all[skip..end].to_vec();
    let actual: Vec<String> = nodes.iter().map(|n| dump_node(n).trim_end().to_string()).collect();
    for (i, (want, got)) in expected.iter().zip(actual.iter()).enumerate() {
        if want != got {
            panic!(
                "child #{} of {selector:?} differs from the reference\n--- reference ---\n{want}\n--- rust ---\n{got}",
                skip + i
            );
        }
    }
    assert_eq!(
        actual.len(),
        expected.len(),
        "{selector:?}: child count differs from the reference (rust {} vs reference {})",
        actual.len(),
        expected.len()
    );
}

/// As [`assert_children_match`], but drops every reference child whose dump
/// contains `needle` before comparing. Used for the one deliberate divergence
/// from the reference document: the removed `tauri-shim.js` script node.
pub fn assert_children_match_ignoring(nodes: &[Node], selector: &str, needle: &str) {
    let reference = Reference::new();
    let expected: Vec<String> = reference
        .dump_children(selector)
        .into_iter()
        .filter(|s| !s.contains(needle))
        .collect();
    let actual: Vec<String> = nodes.iter().map(|n| dump_node(n).trim_end().to_string()).collect();
    for (i, (want, got)) in expected.iter().zip(actual.iter()).enumerate() {
        if want != got {
            panic!(
                "child #{} of {selector:?} differs from the reference (ignoring {needle:?})\n--- reference ---\n{want}\n--- rust ---\n{got}",
                i
            );
        }
    }
    assert_eq!(
        actual.len(),
        expected.len(),
        "{selector:?}: child count differs from the reference after ignoring {needle:?} (rust {} vs reference {})",
        actual.len(),
        expected.len()
    );
}

fn assert_same(expected: &str, actual: &str, label: &str) {
    if expected == actual {
        return;
    }
    let clip = |line: &str| -> String {
        if line.chars().count() > 240 {
            let head: String = line.chars().take(240).collect();
            format!("{head}... ({len} chars)", len = line.chars().count())
        } else {
            line.to_string()
        }
    };
    let want: Vec<String> = expected.lines().map(clip).collect();
    let got: Vec<String> = actual.lines().map(clip).collect();
    let mut detail = String::new();
    for i in 0..want.len().max(got.len()) {
        match (want.get(i), got.get(i)) {
            (Some(w), Some(g)) if w == g => {}
            (Some(w), Some(g)) => {
                detail.push_str(&format!(
                    "first difference at line {}:\n  reference: {w}\n  rust:      {g}\n",
                    i + 1
                ));
                break;
            }
            (Some(w), None) => {
                detail.push_str(&format!(
                    "rust tree ended early at line {}; reference: {w}\n",
                    i + 1
                ));
                break;
            }
            (None, Some(g)) => {
                detail.push_str(&format!(
                    "rust tree has an extra line {}: {g}\n",
                    i + 1
                ));
                break;
            }
            (None, None) => {}
        }
    }
    panic!("{label} differs from the reference\n{detail}");
}
