//! Host glue for the IDE pane, which now lives in `nzkide_module`.
//!
//! The pane itself (theme, document model, painting, editing, galley cache,
//! highlighting) is the separate `nzkide_module` crate. This file is the only
//! place that knows both that crate and this host's graph:
//!
//! * it re-exports the pane API so existing `source_pane::…` call sites keep
//!   resolving;
//! * it implements [`Language`] over `model::Graph`, so hover / Ctrl+click /
//!   references can only name a real node;
//! * it adapts the host's `CodeLensView` rows into the pane's painted
//!   [`LensRows`].
//!
//! Keeping the graph on this side is deliberate: `nzkide_module` has no
//! `model::Graph`, no `CodeLensView` and no chrome, which is what makes it a
//! separable IDE module rather than a second name for the app.

#![allow(dead_code)]

use csgraph_core::index::Index;

use crate::app::codelens_wire::{CodeLensView, LensRow};
use crate::model::Graph;

// The pane, its theme, document model, state, painting and the highlighter.
pub use nzkide_module::gui_nzkide::*;
use nzkide_module::idelink_nzkide::Language;

/// Bridge this host's graph to the pane's semantic queries.
struct GraphLanguage<'a>(&'a Graph);

impl Language for GraphLanguage<'_> {
    fn symbol_for_token(&self, doc_file: &str, token: &str) -> Option<SymbolHit> {
        symbol_for_token(self.0, doc_file, token)
    }

    fn references_for(&self, node_ref: &str) -> Vec<RefRow> {
        references_for(self.0, node_ref)
    }
}

/// The full pane with the graph language layer.
pub fn ui_with_language(
    ui: &mut egui::Ui,
    state: &mut SourceState,
    theme: &SourceTheme,
    graph: &Graph,
) -> Vec<String> {
    nzkide_module::gui_nzkide::ui_with_language(ui, state, theme, &GraphLanguage(graph))
}

/// The body-only pane with the graph language layer (nzk_ui composition).
pub fn ui_body_with_language(
    ui: &mut egui::Ui,
    state: &mut SourceState,
    theme: &SourceTheme,
    graph: &Graph,
) -> Vec<String> {
    nzkide_module::gui_nzkide::ui_body_with_language(ui, state, theme, &GraphLanguage(graph))
}

/// Resolve a token to a **unique** node in the graph.
///
/// Matching is on the declaration name (`label`), the fully-qualified name, or
/// a `.<token>` suffix of it. When several nodes match, a candidate declared in
/// the open document's file wins (the common same-file case); if that still
/// leaves more than one, the answer is `None` — an ambiguous token must not
/// produce a guessed card or a jump to an arbitrary overload.
pub fn symbol_for_token(graph: &Graph, doc_file: &str, token: &str) -> Option<SymbolHit> {
    if token.is_empty() {
        return None;
    }
    let suffix = format!(".{token}");
    let mut cands: Vec<usize> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| {
            n.label == token
                || n.full == token
                || n.id == token
                || (!n.full.is_empty() && n.full.ends_with(&suffix))
        })
        .map(|(i, _)| i)
        .collect();
    if cands.len() > 1 {
        let same_file: Vec<usize> = cands
            .iter()
            .copied()
            .filter(|&i| !doc_file.is_empty() && graph.nodes[i].file == doc_file)
            .collect();
        if !same_file.is_empty() {
            cands = same_file;
        }
    }
    if cands.len() != 1 {
        return None;
    }
    let i = cands[0];
    let n = &graph.nodes[i];
    let refs = graph.edges.iter().filter(|e| e.to == i).count();
    Some(SymbolHit {
        node_id: n.id.clone(),
        kind: n.kind.clone(),
        label: n.label.clone(),
        full: if n.full.is_empty() { n.label.clone() } else { n.full.clone() },
        decl: format!("{}:{}", n.file, n.line),
        refs,
    })
}

/// Every node with an edge pointing at `node_ref`, sorted by site.
///
/// An unknown node yields an empty list, never fabricated rows.
pub fn references_for(graph: &Graph, node_ref: &str) -> Vec<RefRow> {
    let Some(target) = graph
        .nodes
        .iter()
        .position(|n| n.id == node_ref || n.full == node_ref || n.label == node_ref)
    else {
        return Vec::new();
    };
    let mut rows: Vec<RefRow> = graph
        .edges
        .iter()
        .filter(|e| e.to == target && e.from != target)
        .filter_map(|e| {
            let n = graph.nodes.get(e.from)?;
            Some(RefRow {
                node_id: n.id.clone(),
                label: if n.full.is_empty() { n.label.clone() } else { n.full.clone() },
                kind: e.kind.clone(),
                file: n.file.clone(),
                line: n.line,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        (a.file.as_str(), a.line, a.node_id.as_str(), a.kind.as_str()).cmp(&(
            b.file.as_str(),
            b.line,
            b.node_id.as_str(),
            b.kind.as_str(),
        ))
    });
    rows.dedup();
    rows
}

/// Build the painted lens rows from the host's core rows and the document.
pub fn lens_rows_for(doc: &Document, rows: &[LensRow]) -> LensRows {
    let mut out = LensRows::empty();
    for r in rows {
        let present = doc
            .lines
            .iter()
            .any(|l| l.n == r.line && l.n != 0 && !l.file.is_empty());
        if r.line < 1 || !present {
            continue;
        }
        let n = r.line as usize;
        if out.lines.len() <= n {
            out.lines.resize_with(n + 1, Vec::new);
        }
        out.lines[n].push(LensText {
            line: r.line,
            kind: r.kind.tag(),
            node_id: r.node_id.clone(),
            title: r.title.clone(),
            command_wired: r
                .commands
                .iter()
                .any(|c| c == "csgraph.codelens.showInGraph"),
        });
    }
    out.lens_lines = (1..out.lines.len())
        .filter(|&n| !out.lines[n].is_empty())
        .map(|n| n as i64)
        .collect();
    out
}

/// Host-side extension: load a document and compute its code lenses.
///
/// Kept as an extension trait so every existing `self.source.show_node_and_compute(…)`
/// call site keeps its spelling while the method no longer has to live in the
/// host-independent pane crate.
pub trait SourceHostExt {
    fn show_node_and_compute(
        &mut self,
        index: &mut Index,
        graph: &Graph,
        view: &mut CodeLensView,
        node_ref: &str,
    ) -> bool;
}

impl SourceHostExt for SourceState {
    fn show_node_and_compute(
        &mut self,
        index: &mut Index,
        graph: &Graph,
        view: &mut CodeLensView,
        node_ref: &str,
    ) -> bool {
        let found = self.show_node(index, node_ref);
        match self.doc.as_ref() {
            Some(doc) => {
                view.compute(graph, doc, crate::app::codelens_wire::MAX_DECLS);
                let lenses = lens_rows_for(doc, view.rows());
                self.set_lenses(lenses);
            }
            None => {
                view.close();
                self.set_lenses(LensRows::empty());
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, Node};

    fn graph() -> Graph {
        let node = |id: &str, label: &str, full: &str, kind: &str, file: &str, line: i64| Node {
            id: id.to_string(),
            label: label.to_string(),
            full: full.to_string(),
            kind: kind.to_string(),
            file: file.to_string(),
            line,
            ..Node::default()
        };
        Graph {
            repo: std::path::PathBuf::from("/tmp/nzkide-host"),
            nodes: vec![
                node("n:0", "Alpha", "N.Alpha", "class", "a.cs", 3),
                node("n:1", "m", "N.Alpha.m", "method", "a.cs", 7),
                node("n:2", "m", "N.Beta.m", "method", "b.cs", 9),
                node("n:3", "run", "N.Beta.run", "method", "b.cs", 12),
            ],
            edges: vec![
                Edge { from: 0, to: 1, kind: "contains".into(), count: 1 },
                Edge { from: 2, to: 1, kind: "calls".into(), count: 2 },
                Edge { from: 3, to: 1, kind: "uses".into(), count: 1 },
            ],
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        }
    }

    #[test]
    fn symbol_lookup_is_unambiguous_and_reports_real_refs() {
        let g = graph();
        let hit = symbol_for_token(&g, "b.cs", "m").expect("same-file m");
        assert_eq!(hit.node_id, "n:2");
        assert_eq!(hit.decl, "b.cs:9");
        let a = symbol_for_token(&g, "a.cs", "m").expect("a.cs m");
        assert_eq!(a.node_id, "n:1");
        assert_eq!(a.refs, 3);
        assert_eq!(symbol_for_token(&g, "", "m"), None, "ambiguity is not guessed");
        assert_eq!(symbol_for_token(&g, "b.cs", "NotASymbol"), None);
    }

    #[test]
    fn references_are_incoming_edges_sorted_by_site() {
        let g = graph();
        let rows = references_for(&g, "n:1");
        assert_eq!(rows.len(), 3);
        assert!(rows[0].line_text().contains("a.cs:3"));
        assert!(rows[1].line_text().contains("b.cs:9"));
        assert!(rows[2].line_text().contains("b.cs:12"));
        assert!(references_for(&g, "n:missing").is_empty());
    }

    #[test]
    fn lens_rows_are_routed_only_to_document_lines() {
        let line = DocLine {
            n: 1,
            ctx: "body".into(),
            file: "a.cs".into(),
            orig: 1,
            active: true,
            text: "void A()".into(),
            segs: nzkide_module::highlight_nzkide::highlight_document("void A()").remove(0),
        };
        let doc = Document::from_lines_for_tests("NZK.A", "a.cs", vec![line], 1, 1);
        let rows = vec![
            LensRow {
                line: 1,
                kind: crate::app::codelens_wire::LensKind::Main,
                node_id: "NZK.A".into(),
                title: "1 references · show in graph".into(),
                commands: vec!["csgraph.codelens.showInGraph".into()],
                counts: Default::default(),
                decl: Default::default(),
            },
            LensRow {
                line: 400,
                kind: crate::app::codelens_wire::LensKind::Main,
                node_id: "NZK.Far".into(),
                title: "not on screen".into(),
                commands: vec!["csgraph.codelens.showInGraph".into()],
                counts: Default::default(),
                decl: Default::default(),
            },
        ];
        let lenses = lens_rows_for(&doc, &rows);
        assert_eq!(lenses.lens_lines, vec![1]);
        assert_eq!(lenses.for_line(1).len(), 1);
        assert!(lenses.for_line(400).is_empty());
        assert!(lenses.for_line(0).is_empty());
    }

    #[test]
    fn the_language_adapter_only_answers_real_facts() {
        let g = graph();
        let lang = GraphLanguage(&g);
        assert_eq!(
            lang.symbol_for_token("a.cs", "run").map(|h| h.node_id),
            Some("n:3".to_string())
        );
        assert!(lang.symbol_for_token("a.cs", "nope").is_none());
        assert_eq!(lang.references_for("n:1").len(), 3);
    }
}
