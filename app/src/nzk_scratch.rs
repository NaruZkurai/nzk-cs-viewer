//! Host glue for the Scratch addon: what cs viewer is viewing → `IdeView`.
//!
//! The addon (`scratch_rs_ide_addon`, its own git repo) knows nothing about
//! csgraph. This is the one file that bridges the two, and it is deliberately
//! the *only* one: the addon's model takes an
//! [`IdeView`](scratch_rs_ide_addon::idelink_scratch::IdeView), and this module
//! fills it from the app's real state.
//!
//! # Which surface wins
//!
//! The **source pane's loaded document** is the truth when it is open, because
//! that is literally what the IDE is showing. The graph selection is the
//! fallback: it synthesizes the node's document through the same
//! `Index::doc_source` call the source pane uses, so the blocks are the same
//! text either way.
//!
//! Nothing is invented: no selection and no document is `None`, which the addon
//! paints as its empty state.

use std::hash::{Hash, Hasher};

use csgraph_core::index::Index;
use scratch_rs_ide_addon::gui_scratch::PaletteSpec;
use scratch_rs_ide_addon::idelink_scratch::{self, IdeView, LinkState};

use crate::model::Graph;
use crate::app::source_pane::{self, SourceState, SourceTheme};
use crate::view::GraphView;

/// Build the addon's view of the code the IDE is showing.
///
/// `repo` is only a label for the context strip.
pub fn ide_view(
    view: Option<&GraphView>,
    source: &SourceState,
    index: &mut Index,
    repo: &str,
) -> Option<IdeView> {
    // 1. The source pane, when it holds a document.
    if source.open {
        if let Some(doc) = source.doc.as_ref() {
            if !doc.lines.is_empty() {
                let node = view.and_then(|v| find_node(&v.graph, &doc.node_ref));
                let text = doc.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
                return Some(IdeView {
                    repo: repo.to_string(),
                    node_id: doc.node_ref.clone(),
                    title: match node {
                        Some(n) if !n.label.is_empty() => n.label.clone(),
                        _ if !doc.title.is_empty() => doc.title.clone(),
                        _ => doc.node_ref.clone(),
                    },
                    kind: node.map(|n| n.kind.clone()).unwrap_or_default(),
                    file: doc.file.clone(),
                    line: doc.line,
                    end_line: doc.end_line,
                    source: text,
                    ret: node.and_then(|n| non_empty(&n.ret)),
                    from_source_pane: true,
                });
            }
        }
    }

    // 2. The graph selection, synthesized through the engine.
    let (graph, sel) = view.and_then(|v| v.selected.map(|s| (&v.graph, s)))?;
    let node = graph.nodes.get(sel)?;
    let doc = source_pane::document_for(index, &node.id).ok()?;
    let text = doc.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
    Some(IdeView {
        repo: repo.to_string(),
        node_id: node.id.clone(),
        title: if node.label.is_empty() { node.full.clone() } else { node.label.clone() },
        kind: node.kind.clone(),
        file: if doc.file.is_empty() { node.file.clone() } else { doc.file.clone() },
        line: if doc.line > 0 { doc.line } else { node.line },
        end_line: if doc.end_line > 0 { doc.end_line } else { node.end_line },
        source: text,
        ret: non_empty(&node.ret),
        from_source_pane: false,
    })
}

/// A cheap identity for "the Scratch document would be the same this frame".
///
/// The pane used to re-parse the viewed source on every repaint. Parsing is not
/// free, and nothing changes between frames unless the IDE document or the
/// graph selection does — so the host compares this key and reuses the last
/// `LinkState` when it matches. It hashes the viewed text (not just the node id,
/// because an edit or a re-synthesized document can change under the same ref)
/// and the fallback selection.
pub fn link_key(
    view: Option<&GraphView>,
    source: &SourceState,
    repo: &str,
) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    repo.hash(&mut h);
    source.open.hash(&mut h);
    if let Some(doc) = source.doc.as_ref() {
        doc.node_ref.hash(&mut h);
        doc.title.hash(&mut h);
        doc.file.hash(&mut h);
        doc.line.hash(&mut h);
        doc.end_line.hash(&mut h);
        doc.lines.len().hash(&mut h);
        for l in &doc.lines {
            l.text.hash(&mut h);
        }
    }
    if let Some(v) = view {
        if let Some(i) = v.selected {
            if let Some(n) = v.graph.nodes.get(i) {
                n.id.hash(&mut h);
                n.full.hash(&mut h);
                n.file.hash(&mut h);
                n.line.hash(&mut h);
            }
        }
    }
    h.finish()
}

/// The addon's ready/empty/error state for the current IDE selection.
pub fn link_state(
    view: Option<&GraphView>,
    source: &SourceState,
    index: &mut Index,
    repo: &str,
) -> LinkState {
    match ide_view(view, source, index, repo) {
        Some(v) => idelink_scratch::build(&v),
        None => LinkState::Empty,
    }
}

/// The Scratch pane's palette, taken from the IDE's own source-pane theme so
/// the pop-out is the same surface as the editor beside it — not an egui
/// default grey with different token colours.
pub fn palette_spec(t: &SourceTheme) -> PaletteSpec {
    PaletteSpec {
        bg: t.bg,
        head_bg: t.head_bg,
        fg: t.fg,
        dim: t.text_dim,
        border: t.border,
        accent: t.accent,
        comment: t.comment,
        keyword: t.keyword,
        control: t.control,
        ty: t.ty,
        iface: t.iface,
        string: t.string,
        number: t.number,
        preproc: t.preproc,
        function: t.function,
        variable: t.variable,
        op: t.op,
    }
}

fn find_node<'g>(graph: &'g Graph, node_ref: &str) -> Option<&'g crate::model::Node> {
    graph
        .nodes
        .iter()
        .find(|n| n.id == node_ref || n.full == node_ref || n.label == node_ref)
}

fn non_empty(s: &str) -> Option<String> {
    if s.trim().is_empty() { None } else { Some(s.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::source_pane::Document;
    use scratch_rs_ide_addon::idelink_scratch::LinkState;

    /// The host half of the bridge, end to end: a loaded source-pane document
    /// becomes an `IdeView` and then puzzle pieces, with types on the ports.
    #[test]
    fn the_open_document_becomes_typed_blocks() {
        let mut index = csgraph_core::index::Index::new(std::path::PathBuf::from("/tmp"));
        let mut source = SourceState::default();
        source.open = true;
        source.doc = Some(
            Document::from_value(
                "#m:Demo.Counter.Add",
                &serde_json::json!({
                    "start": 1,
                    "end": 4,
                    "node": {
                        "full": "Demo.Counter.Add",
                        "file": "Demo.cs",
                        "line": 8,
                        "endLine": 18
                    },
                    "lines": [
                        {"n": 1, "text": "void Add(int n)"},
                        {"n": 2, "text": "{"},
                        {"n": 3, "text": "    if (n <= 0) { return; }"},
                        {"n": 4, "text": "}"}
                    ]
                }),
            )
            .expect("document"),
        );

        let state = link_state(None, &source, &mut index, "demo");
        let LinkState::Ready(doc) = state else {
            panic!("expected a ready document, got {state:?}");
        };
        assert!(doc.from_source_pane, "the pane is the truth when it is open");
        assert_eq!(doc.kind, "", "no graph node means no kind, not a guess");
        let method = &doc.blocks[0];
        assert_eq!(method.head, "void Add");
        assert_eq!(method.ports[0].label(), "n: int");
        assert_eq!(method.ports[1].label(), "-> void");
        assert_eq!(doc.count(), 3, "method, if, return");
    }

    #[test]
    fn no_view_and_no_document_is_empty() {
        let mut index = csgraph_core::index::Index::new(std::path::PathBuf::from("/tmp"));
        let source = SourceState::default();
        assert_eq!(
            link_state(None, &source, &mut index, "demo"),
            LinkState::Empty
        );
    }

    #[test]
    fn non_empty_never_invents_a_type() {
        assert_eq!(non_empty(""), None);
        assert_eq!(non_empty("   "), None);
        assert_eq!(non_empty("void"), Some("void".into()));
    }
}
