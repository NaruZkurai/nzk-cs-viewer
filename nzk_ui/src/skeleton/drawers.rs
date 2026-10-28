/*! Rust-built region: The three drawers inside `#canvasWrap`: `#nodeDrawer`, `#findPanel`, `#blocksPanel`.
//!
//! Transcribed from `frontend/index.html`; parity is asserted against the
//! reference parse by `super::testsupport` (test-only). */

use super::{el, svg_el, t, Node};

pub fn nodes() -> Vec<Node> {
    vec![node_drawer(), find_panel(), blocks_panel()]
}

/// `<aside id="nodeDrawer" class="drawer hidden">` — the selected node's
/// properties and relations.
fn node_drawer() -> Node {
    el(
        "aside",
        &[
            ("id", "nodeDrawer"),
            ("class", "drawer hidden"),
            ("aria-label", "Node properties and relations"),
        ],
        vec![
            el(
                "div",
                &[("class", "drawer-head")],
                vec![
                    el(
                        "span",
                        &[("id", "nodeTitle"), ("class", "drawer-title")],
                        vec![t("Node")],
                    ),
                    el("span", &[("class", "source-spacer")], vec![]),
                    el(
                        "button",
                        &[
                            ("id", "nodeCodeBtn"),
                            ("class", "btn btn-ghost"),
                            ("type", "button"),
                            (
                                "title",
                                "Show this declaration in the source viewer (C)",
                            ),
                        ],
                        vec![t("{ } View code")],
                    ),
                    el(
                        "button",
                        &[
                            ("id", "nodeDrawerClose"),
                            ("class", "btn btn-ghost"),
                            ("type", "button"),
                            ("title", "Close drawer"),
                        ],
                        vec![t("\u{2715}")],
                    ),
                ],
            ),
            el(
                "div",
                &[("id", "nodeCrumbs"), ("class", "crumbs")],
                vec![],
            ),
            el(
                "div",
                &[("class", "drawer-body")],
                vec![
                    el(
                        "section",
                        &[("class", "section"), ("id", "propsSection")],
                        vec![el(
                            "div",
                            &[("class", "section-body"), ("id", "nodeProps")],
                            vec![],
                        )],
                    ),
                    el(
                        "section",
                        &[("class", "section"), ("id", "relationsSection")],
                        vec![
                            el(
                                "button",
                                &[
                                    ("class", "section-head"),
                                    ("id", "relationsHead"),
                                    ("type", "button"),
                                    (
                                        "title",
                                        "Collapse or expand the relation list for the selected node",
                                    ),
                                ],
                                vec![
                                    el(
                                        "span",
                                        &[("class", "caret")],
                                        vec![t("\u{25be}")],
                                    ),
                                    t("Relations"),
                                    el(
                                        "span",
                                        &[("class", "scount"), ("id", "relationsCount")],
                                        vec![],
                                    ),
                                ],
                            ),
                            el(
                                "div",
                                &[
                                    ("class", "section-body"),
                                    ("id", "relationsBody"),
                                    ("style", "padding:0"),
                                ],
                                vec![],
                            ),
                        ],
                    ),
                ],
            ),
        ],
    )
}

/// `<aside id="findPanel" class="drawer hidden">` — find results.
fn find_panel() -> Node {
    el(
        "aside",
        &[
            ("id", "findPanel"),
            ("class", "drawer hidden"),
            ("aria-label", "Find results"),
        ],
        vec![
            el(
                "div",
                &[("class", "drawer-head")],
                vec![
                    el(
                        "span",
                        &[("id", "findQuery"), ("class", "find-query")],
                        vec![t("Find")],
                    ),
                    el(
                        "span",
                        &[("id", "findMeta"), ("class", "find-meta")],
                        vec![],
                    ),
                    el(
                        "button",
                        &[
                            ("id", "findClose"),
                            ("class", "btn btn-ghost"),
                            ("type", "button"),
                            ("title", "Leave Find mode (Esc)"),
                        ],
                        vec![t("\u{2715}")],
                    ),
                ],
            ),
            el(
                "div",
                &[
                    ("id", "findNotice"),
                    ("class", "find-notice hidden"),
                ],
                vec![],
            ),
            el(
                "div",
                &[("id", "findResults"), ("class", "find-results")],
                vec![],
            ),
            el(
                "div",
                &[("class", "find-foot")],
                vec![el(
                    "span",
                    &[("class", "find-hint")],
                    vec![
                        t("fully-qualified names supported, e.g. "),
                        el(
                            "code",
                            &[],
                            vec![t("NZK.Core.MA.NormaliseName")],
                        ),
                    ],
                )],
            ),
        ],
    )
}

/// `<aside id="blocksPanel" class="blocks-panel hidden">` — the read-only
/// block canvas, its edges and the hover peek.
fn blocks_panel() -> Node {
    el(
        "aside",
        &[
            ("id", "blocksPanel"),
            ("class", "blocks-panel hidden"),
            ("aria-label", "Blocks \u{2014} declarations and reference edges"),
        ],
        vec![
            el(
                "div",
                &[("class", "blocks-head")],
                vec![
                    el(
                        "span",
                        &[("id", "blocksTitle"), ("class", "blocks-title")],
                        vec![t("Blocks")],
                    ),
                    el(
                        "span",
                        &[("id", "blocksMeta"), ("class", "blocks-meta")],
                        vec![],
                    ),
                    el(
                        "span",
                        &[("class", "blocks-hint")],
                        vec![t("click a block to navigate")],
                    ),
                    el(
                        "button",
                        &[
                            ("id", "blocksClose"),
                            ("class", "btn btn-ghost"),
                            ("type", "button"),
                            ("title", "Close the Blocks panel (b)"),
                        ],
                        vec![t("\u{2715}")],
                    ),
                ],
            ),
            el(
                "div",
                &[("id", "blocksScroll"), ("class", "blocks-scroll")],
                vec![el(
                    "div",
                    &[("id", "blocksStage"), ("class", "blocks-stage")],
                    vec![
                        svg_el(
                            "svg",
                            &[
                                ("id", "blocksEdges"),
                                ("class", "blocks-edges"),
                                ("aria-hidden", "true"),
                            ],
                            vec![],
                        ),
                        el(
                            "div",
                            &[("id", "blocksCards"), ("class", "blocks-cards")],
                            vec![],
                        ),
                    ],
                )],
            ),
            el(
                "div",
                &[
                    ("id", "blocksPeek"),
                    ("class", "blocks-peek hidden"),
                    ("role", "tooltip"),
                ],
                vec![
                    el(
                        "div",
                        &[("class", "bp-head")],
                        vec![
                            el(
                                "span",
                                &[("id", "blocksPeekKind"), ("class", "bp-kind")],
                                vec![],
                            ),
                            el(
                                "span",
                                &[("id", "blocksPeekName"), ("class", "bp-name")],
                                vec![],
                            ),
                        ],
                    ),
                    el(
                        "div",
                        &[("id", "blocksPeekFile"), ("class", "bp-file")],
                        vec![],
                    ),
                    el(
                        "div",
                        &[("id", "blocksPeekSrc"), ("class", "bp-src")],
                        vec![t("Loading source\u{2026}")],
                    ),
                    el(
                        "div",
                        &[("id", "blocksPeekRefs"), ("class", "bp-refs")],
                        vec![],
                    ),
                ],
            ),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::super::testsupport;
    use super::*;

    /// `#nodeDrawer`, `#findPanel`, `#blocksPanel` match the reference parse.
    #[test]
    fn drawers_matches_reference() {
        testsupport::assert_children_match(&nodes(), "#canvasWrap", 6);
    }
}
