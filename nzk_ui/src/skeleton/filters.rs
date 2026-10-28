/*! Rust-built region: `section#filtersSection` (inside `aside.side`).
//!
//! Transcribed from `frontend/index.html`; parity is asserted against the
//! reference parse by `super::testsupport` (test-only). */

use super::{el, t, Node};

pub fn nodes() -> Vec<Node> {
    vec![el(
        "section",
        &[("class", "section"), ("id", "filtersSection")],
        vec![
            el(
                "button",
                &[
                    ("class", "section-head"),
                    ("id", "filtersHead"),
                    ("type", "button"),
                    ("title", "Collapse or expand the filter controls"),
                ],
                vec![
                    el("span", &[("class", "caret")], vec![t("▾")]),
                    t("Filters"),
                    el(
                        "span",
                        &[("class", "scount"), ("id", "filtersCount")],
                        vec![],
                    ),
                ],
            ),
            el(
                "div",
                &[("class", "section-body")],
                vec![
                    el(
                        "div",
                        &[("class", "filter-label")],
                        vec![
                            t("Namespace"),
                            el(
                                "span",
                                &[("class", "filter-actions")],
                                vec![el(
                                    "button",
                                    &[
                                        ("class", "linkbtn"),
                                        ("type", "button"),
                                        ("id", "nsAllBtn"),
                                        ("title", "Show every namespace"),
                                    ],
                                    vec![t("all")],
                                )],
                            ),
                        ],
                    ),
                    el(
                        "select",
                        &[
                            ("id", "nsSelect"),
                            ("class", "select filter-ns"),
                            ("title", "Show only one namespace at a time"),
                        ],
                        vec![el(
                            "option",
                            &[("value", "")],
                            vec![t("All namespaces")],
                        )],
                    ),
                    el(
                        "label",
                        &[("class", "check"), ("style", "margin-top:6px")],
                        vec![
                            el(
                                "input",
                                &[("type", "checkbox"), ("id", "nsDeep"), ("checked", "")],
                                vec![],
                            ),
                            el(
                                "span",
                                &[(
                                    "title",
                                    "On: the chosen namespace also keeps everything nested inside it\n                       (ns.subns.subsubns). Off: only what is declared directly in that\n                       one namespace — no sub-namespaces.",
                                )],
                                vec![t("Include sub-namespaces")],
                            ),
                        ],
                    ),
                    el(
                        "div",
                        &[
                            ("class", "filter-label"),
                            ("style", "margin-top:9px"),
                        ],
                        vec![
                            t("Node kinds"),
                            el(
                                "span",
                                &[("class", "filter-actions")],
                                vec![
                                    el(
                                        "button",
                                        &[
                                            ("class", "linkbtn"),
                                            ("type", "button"),
                                            ("data-kinds", "all"),
                                            ("title", "Show every node kind"),
                                        ],
                                        vec![t("all")],
                                    ),
                                    el(
                                        "button",
                                        &[
                                            ("class", "linkbtn"),
                                            ("type", "button"),
                                            ("data-kinds", "none"),
                                            ("title", "Hide every node kind"),
                                        ],
                                        vec![t("none")],
                                    ),
                                ],
                            ),
                        ],
                    ),
                    el("div", &[("id", "kindFilters")], vec![]),
                    el(
                        "div",
                        &[
                            ("class", "filter-label"),
                            ("style", "margin-top:9px"),
                        ],
                        vec![
                            t("Edge kinds"),
                            el(
                                "span",
                                &[("class", "filter-actions")],
                                vec![
                                    el(
                                        "button",
                                        &[
                                            ("class", "linkbtn"),
                                            ("type", "button"),
                                            ("data-edges", "all"),
                                            ("title", "Show every edge kind"),
                                        ],
                                        vec![t("all")],
                                    ),
                                    el(
                                        "button",
                                        &[
                                            ("class", "linkbtn"),
                                            ("type", "button"),
                                            ("data-edges", "none"),
                                            ("title", "Hide every edge kind"),
                                        ],
                                        vec![t("none")],
                                    ),
                                ],
                            ),
                        ],
                    ),
                    el("div", &[("id", "edgeFilters"), ("class", "chips")], vec![]),
                    el(
                        "label",
                        &[("class", "check"), ("style", "margin-top:9px")],
                        vec![
                            el(
                                "input",
                                &[
                                    ("type", "checkbox"),
                                    ("id", "hideEdgeLabels"),
                                ],
                                vec![],
                            ),
                            el(
                                "span",
                                &[(
                                    "title",
                                    "On: VT mode — every relation is a small node (dot + word) on a\n                       direct line between its declarations. Off: the relation is a\n                       word box beside the curved arrow.",
                                )],
                                vec![t("VT mode — direct-line relations")],
                            ),
                        ],
                    ),
                    el(
                        "div",
                        &[
                            ("class", "filter-label"),
                            ("style", "margin-top:9px"),
                        ],
                        vec![t("Groups")],
                    ),
                    el(
                        "div",
                        &[("class", "chips")],
                        vec![
                            el(
                                "label",
                                &[("class", "edgekey"), ("id", "kindChildWrap")],
                                vec![
                                    el(
                                        "input",
                                        &[
                                            ("type", "checkbox"),
                                            ("id", "groupChildren"),
                                            ("checked", ""),
                                            ("hidden", ""),
                                        ],
                                        vec![],
                                    ),
                                    el(
                                        "span",
                                        &[
                                            ("class", "bars"),
                                            ("style", "border-top-color:#6b7897"),
                                        ],
                                        vec![],
                                    ),
                                    t("child nodes (contains)"),
                                ],
                            ),
                            el(
                                "label",
                                &[("class", "edgekey"), ("id", "kindEdgeWrap")],
                                vec![
                                    el(
                                        "input",
                                        &[
                                            ("type", "checkbox"),
                                            ("id", "groupEdges"),
                                            ("checked", ""),
                                            ("hidden", ""),
                                        ],
                                        vec![],
                                    ),
                                    el(
                                        "span",
                                        &[
                                            ("class", "bars"),
                                            ("style", "border-top-color:#6b7897"),
                                        ],
                                        vec![],
                                    ),
                                    t("edge nodes (calls/uses/…)"),
                                ],
                            ),
                        ],
                    ),
                    el(
                        "div",
                        &[
                            ("class", "filter-row"),
                            ("style", "margin-top:9px"),
                        ],
                        vec![
                            el(
                                "label",
                                &[("class", "check")],
                                vec![
                                    el(
                                        "input",
                                        &[
                                            ("type", "checkbox"),
                                            ("id", "hideIsolated"),
                                        ],
                                        vec![],
                                    ),
                                    el("span", &[], vec![t("Hide isolated")]),
                                ],
                            ),
                            el(
                                "label",
                                &[("class", "check")],
                                vec![
                                    el("span", &[], vec![t("Depth")]),
                                    el(
                                        "input",
                                        &[
                                            ("type", "number"),
                                            ("id", "depthSelect"),
                                            ("class", "select depth-input"),
                                            ("min", "0"),
                                            ("max", "9999999"),
                                            ("step", "1"),
                                            ("value", "1"),
                                            ("inputmode", "numeric"),
                                            ("autocomplete", "off"),
                                            ("spellcheck", "false"),
                                            ("title", "Neighbourhood depth: 0 shows only the node. Type a value, click the up/down arrows, or scroll the box (Shift = ×10). Range 0–9999999."),
                                        ],
                                        vec![],
                                    ),
                                ],
                            ),
                        ],
                    ),
                    el(
                        "label",
                        &[("class", "check"), ("style", "margin-top:9px")],
                        vec![
                            el(
                                "input",
                                &[
                                    ("type", "checkbox"),
                                    ("id", "hideChildren"),
                                ],
                                vec![],
                            ),
                            el("span", &[], vec![t("Hide relations")]),
                        ],
                    ),
                    el(
                        "div",
                        &[
                            ("class", "filter-row"),
                            ("style", "margin-top:9px"),
                        ],
                        vec![
                            el(
                                "button",
                                &[
                                    ("id", "resetFilters"),
                                    ("class", "btn btn-ghost"),
                                    ("type", "button"),
                                    (
                                        "title",
                                        "Restore the default filters, layout and hidden-node state",
                                    ),
                                ],
                                vec![t("Reset filters")],
                            ),
                            el(
                                "button",
                                &[
                                    ("id", "pruneHidden"),
                                    ("class", "btn btn-ghost"),
                                    ("type", "button"),
                                    (
                                        "title",
                                        "Delete nodes hidden by the filters from the graph",
                                    ),
                                ],
                                vec![t("Remove hidden")],
                            ),
                        ],
                    ),
                    el(
                        "div",
                        &[
                            ("class", "filter-group"),
                            ("style", "margin-top:12px"),
                        ],
                        vec![
                            el(
                                "div",
                                &[("class", "filter-label")],
                                vec![t("Layout")],
                            ),
                            el(
                                "label",
                                &[("class", "range"), ("for", "spacingRange")],
                                vec![
                                    el(
                                        "span",
                                        &[("class", "range-name")],
                                        vec![t("Spacing")],
                                    ),
                                    el(
                                        "input",
                                        &[
                                            ("id", "spacingRange"),
                                            ("type", "range"),
                                            ("min", "8"),
                                            ("max", "240"),
                                            ("step", "1"),
                                            ("value", "120"),
                                        ],
                                        vec![],
                                    ),
                                    el(
                                        "output",
                                        &[
                                            ("id", "spacingOut"),
                                            ("class", "range-val"),
                                        ],
                                        vec![t("120")],
                                    ),
                                ],
                            ),
                            el(
                                "label",
                                &[("class", "range"), ("for", "nodeSizeRange")],
                                vec![
                                    el(
                                        "span",
                                        &[("class", "range-name")],
                                        vec![t("Node size")],
                                    ),
                                    el(
                                        "input",
                                        &[
                                            ("id", "nodeSizeRange"),
                                            ("type", "range"),
                                            ("min", "2"),
                                            ("max", "24"),
                                            ("step", "1"),
                                            ("value", "8"),
                                        ],
                                        vec![],
                                    ),
                                    el(
                                        "output",
                                        &[
                                            ("id", "nodeSizeOut"),
                                            ("class", "range-val"),
                                        ],
                                        vec![t("8")],
                                    ),
                                ],
                            ),
                            el(
                                "label",
                                &[("class", "range"), ("for", "iterRange")],
                                vec![
                                    el(
                                        "span",
                                        &[("class", "range-name")],
                                        vec![t("Iterations")],
                                    ),
                                    el(
                                        "input",
                                        &[
                                            ("id", "iterRange"),
                                            ("type", "range"),
                                            ("min", "300"),
                                            ("max", "1500"),
                                            ("step", "50"),
                                            ("value", "800"),
                                        ],
                                        vec![],
                                    ),
                                    el(
                                        "output",
                                        &[("id", "iterOut"), ("class", "range-val")],
                                        vec![t("800")],
                                    ),
                                ],
                            ),
                            el(
                                "label",
                                &[("class", "range"), ("for", "curveRange")],
                                vec![
                                    el(
                                        "span",
                                        &[("class", "range-name")],
                                        vec![t("Edge curve")],
                                    ),
                                    el(
                                        "input",
                                        &[
                                            ("id", "curveRange"),
                                            ("type", "range"),
                                            ("min", "0"),
                                            ("max", "0.2"),
                                            ("step", "0.01"),
                                            ("value", "0.1"),
                                        ],
                                        vec![],
                                    ),
                                    el(
                                        "output",
                                        &[("id", "curveOut"), ("class", "range-val")],
                                        vec![t("0.10")],
                                    ),
                                ],
                            ),
                            el(
                                "label",
                                &[("class", "range"), ("for", "textSizeRange")],
                                vec![
                                    el(
                                        "span",
                                        &[("class", "range-name")],
                                        vec![t("Text size")],
                                    ),
                                    el(
                                        "input",
                                        &[
                                            ("id", "textSizeRange"),
                                            ("type", "range"),
                                            ("min", "6"),
                                            ("max", "28"),
                                            ("step", "1"),
                                            ("value", "11"),
                                        ],
                                        vec![],
                                    ),
                                    el(
                                        "output",
                                        &[
                                            ("id", "textSizeOut"),
                                            ("class", "range-val"),
                                        ],
                                        vec![t("11")],
                                    ),
                                ],
                            ),
                        ],
                    ),
                    el(
                        "p",
                        &[("class", "hint"), ("style", "margin-top:8px")],
                        vec![t("Double-click a node to expand it · drag pins a node ·\n        hover shows relations · right-click a node to pin its card open · click selects.")],
                    ),
                ],
            ),
        ],
    )]
}

#[cfg(test)]
mod tests {
    use super::super::testsupport;
    use super::*;

    #[test]
    fn filters_matches_reference() {
        testsupport::assert_children_match(&nodes(), "aside.side", 0);
    }
}
