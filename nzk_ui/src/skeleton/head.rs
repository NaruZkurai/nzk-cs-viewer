/*! `<head>`: the five inlined stylesheets and the classic script tags.

The reference `index.html` has `<link rel="stylesheet" href="/static/style.css">`
at the fifth child position; `dom::inline_stylesheets` used to replace that one
`<link>` with five `<style>` elements. Building the tree in Rust does the same
replacement structurally, and the sheet text still comes from the caller /
`assets/css/` at runtime. */

use super::{el, t, Node, Sheets};

pub fn nodes(sheets: &Sheets) -> Vec<Node> {
    let mut head = vec![
        el("base", &[("href", "/")], vec![]),
        el("meta", &[("charset", "utf-8")], vec![]),
        el(
            "meta",
            &[
                ("name", "viewport"),
                ("content", "width=device-width, initial-scale=1"),
            ],
            vec![],
        ),
        el(
            "title",
            &[],
            vec![t("csgraph \u{2014} function relation graph")],
        ),
        style("csviewer-real-stylesheet", &sheets.style),
        style("nzk_ui-compat-shim", &sheets.compat),
        style("cs-problems-style", &sheets.problems),
        style("cs-hier-style", &sheets.hierarchy),
        style("cs-quickopen-style", &sheets.quickopen),
    ];
    head.push(script("/vendor/monaco/min/vs/loader.js", false));
    for src in [
        "/static/core.js",
        "/static/graph.js",
        "/static/layout.js",
        "/static/render.js",
        "/static/interaction.js",
        "/static/drawer.js",
        "/static/search.js",
        "/static/editor.js",
        "/static/language.js",
        "/static/hover.js",
        "/static/symbols.js",
        "/static/semantics.js",
        "/static/codelens.js",
        "/static/hierarchy.js",
        "/static/quickopen.js",
        "/static/nav.js",
        "/static/problems.js",
        "/static/windows.js",
        "/static/sync.js",
        "/static/panewin.js",
        "/static/source.js",
        "/static/filters.js",
        "/static/panels.js",
        "/static/shell.js",
        "/static/scratch.js",
        "/static/reorg.js",
        "/static/edit.js",
        "/static/preview.js",
        "/static/arrange.js",
        "/static/app.js",
    ] {
        head.push(script(src, true));
    }
    head
}

fn style(id: &'static str, css: &str) -> Node {
    el("style", &[("id", id)], vec![t(css)])
}

fn script(src: &'static str, defer: bool) -> Node {
    if defer {
        el("script", &[("src", src), ("defer", "")], vec![])
    } else {
        el("script", &[("src", src)], vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::super::testsupport;
    use super::*;

    fn sheets() -> Sheets {
        let fe = crate::NzkUi::frontend_dir();
        let css = std::fs::read_to_string(fe.join("static/style.css")).expect("read style.css");
        Sheets::load(&css).expect("read injected sheets")
    }

    /// The script/stylesheet skeleton of `<head>` matches the reference parse,
    /// minus the deliberately removed `tauri-shim.js` node.
    #[test]
    fn head_matches_reference() {
        let reference = testsupport::Reference::new();
        assert_eq!(
            reference.count("head > *") - 1,
            nodes(&sheets()).len(),
            "the only divergence from the reference is the removed tauri-shim node"
        );
        testsupport::assert_children_match_ignoring(&nodes(&sheets()), "head", "tauri-shim.js");
    }

    /// The removed shim must not creep back in: it was a `<script>` node the
    /// old Tauri frontend needed, and nzk_ui never executes scripts.
    #[test]
    fn tauri_shim_is_not_emitted() {
        let has_shim = nodes(&sheets()).iter().any(|n| match n {
            Node::Element { tag, attrs, .. } => {
                *tag == "script"
                    && attrs
                        .iter()
                        .any(|(k, v)| *k == "src" && v.contains("tauri-shim"))
            }
            Node::Text(_) => false,
        });
        assert!(!has_shim, "the head must not carry a tauri-shim script");
    }

    /// The five style ids the old splice produced are preserved, in order.
    #[test]
    fn style_ids_preserved_in_order() {
        let ids: Vec<String> = nodes(&sheets())
            .iter()
            .filter_map(|n| match n {
                Node::Element {
                    tag: "style", attrs, ..
                } => attrs
                    .iter()
                    .find(|(k, _)| *k == "id")
                    .map(|(_, v)| (*v).to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            [
                "csviewer-real-stylesheet",
                "nzk_ui-compat-shim",
                "cs-problems-style",
                "cs-hier-style",
                "cs-quickopen-style"
            ]
        );
    }
}
