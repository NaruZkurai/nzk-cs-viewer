/*! Rust-built region: the remaining `aside.side` sections (`#pubSection`,
`#strSection`, `#chainSection`) and the `#sourcePane` sibling.
//!
//! Transcribed from `frontend/index.html`; parity is asserted against the
//! reference parse by `super::testsupport` (test-only).
//!
//! NOTE: in the reference document `section#sourcePane` is a direct child of
//! `main.layout` (child index 2), **not** a child of `aside.side`;
//! `aside.side` has exactly four element children (`#filtersSection`, owned by
//! `filters.rs`, plus the three sections here). [`nodes`] therefore returns the
//! three sections owned by this region, and [`source_pane`] exposes the
//! `#sourcePane` sibling for `main`'s child list. */

use super::{el, t, Node};

/// Children 1-3 of `aside.side`: `#pubSection`, `#strSection`, `#chainSection`.
pub fn nodes() -> Vec<Node> {
    vec![pub_section(), str_section(), chain_section()]
}

/// Child 2 of `main.layout`: `section#sourcePane`.
pub fn source_pane() -> Node {
    el(
        "section",
        &[
            ("id", "sourcePane"),
            ("class", "source-pane hidden"),
            ("aria-label", "Source viewer"),
        ],
        vec![
            el(
                "div",
                &[("class", "source-head")],
                vec![
                    el(
                        "span",
                        &[("id", "sourceTitle"), ("class", "source-title")],
                        vec![t("\u{2014}")],
                    ),
                    button(
                        "srcCopyBtn",
                        "Copy the shown code (no line numbers)",
                        "Copy code",
                    ),
                    button("srcRefBtn", "Copy file:line", "Copy ref"),
                    el("span", &[("class", "source-spacer")], vec![]),
                    el(
                        "span",
                        &[("id", "sourceChipWrap"), ("class", "source-chipwrap")],
                        vec![],
                    ),
                    el(
                        "span",
                        &[
                            ("id", "sourceMatchTag"),
                            ("class", "source-matchtag hidden"),
                        ],
                        vec![],
                    ),
                    el(
                        "select",
                        &[
                            ("id", "sourceTheme"),
                            ("class", "select"),
                            ("title", "Syntax theme"),
                        ],
                        vec![
                            option("darkplus", "Dark+", false),
                            option("darkmodern", "Dark Modern", false),
                            option("monokai", "Monokai", false),
                            option("solarized", "Solarized Dark", false),
                        ],
                    ),
                    button(
                        "srcModeBtn",
                        "Doc = every declaration site gathered into one synthesized file; Raw = the file as written",
                        "Doc",
                    ),
                    button(
                        "srcPopBtn",
                        "Open this document in its own window (its own thread and its own editor worker)",
                        "\u{29C9} New window",
                    ),
                    button("srcMethodBtn", "Show the whole declaration", "Whole method"),
                    button("srcWindowBtn", "Show \u{00B1}30 lines around the hit", "Window"),
                    button("srcPrevBtn", "Previous hit in this file", "Prev"),
                    button("srcNextBtn", "Next hit in this file", "Next"),
                    button("srcCloseBtn", "Close (Esc)", "Close"),
                ],
            ),
            el(
                "div",
                &[("id", "sourceBody"), ("class", "source-body")],
                vec![],
            ),
            el(
                "div",
                &[("id", "sourceNote"), ("class", "source-note")],
                vec![],
            ),
        ],
    )
}

fn section_head(
    id: &'static str,
    title: &'static str,
    label: &'static str,
    count: &'static str,
) -> Node {
    el(
        "button",
        &[
            ("class", "section-head"),
            ("id", id),
            ("type", "button"),
            ("title", title),
        ],
        vec![
            el("span", &[("class", "caret")], vec![t("\u{25B8}")]),
            t(label),
            el("span", &[("class", "scount"), ("id", count)], vec![]),
        ],
    )
}

fn search_body(
    input_id: &'static str,
    placeholder: &'static str,
    list_id: &'static str,
) -> Node {
    el(
        "div",
        &[("class", "section-body")],
        vec![
            el(
                "input",
                &[
                    ("id", input_id),
                    ("class", "mini-input"),
                    ("type", "text"),
                    ("spellcheck", "false"),
                    ("placeholder", placeholder),
                ],
                vec![],
            ),
            el(
                "div",
                &[("id", list_id), ("class", "listbox")],
                vec![el(
                    "div",
                    &[("class", "list-empty")],
                    vec![t("open to load")],
                )],
            ),
        ],
    )
}

fn pub_section() -> Node {
    el(
        "section",
        &[("class", "section collapsed"), ("id", "pubSection")],
        vec![
            section_head(
                "pubHead",
                "Collapse or expand the list of public definitions",
                "Public definitions",
                "pubCount",
            ),
            search_body("pubSearch", "filter public definitions\u{2026}", "pubList"),
        ],
    )
}

fn str_section() -> Node {
    el(
        "section",
        &[("class", "section collapsed"), ("id", "strSection")],
        vec![
            section_head(
                "strHead",
                "Collapse or expand the string-literal search",
                "Strings / error codes",
                "strCount",
            ),
            search_body("strSearch", "filter strings\u{2026}", "strList"),
        ],
    )
}

fn chain_section() -> Node {
    el(
        "section",
        &[("class", "section collapsed"), ("id", "chainSection")],
        vec![
            section_head(
                "chainHead",
                "Collapse or expand the resolve-chain walker",
                "Resolve chain",
                "chainCount",
            ),
            el(
                "div",
                &[("class", "section-body")],
                vec![
                    el(
                        "div",
                        &[("class", "chain-controls")],
                        vec![
                            el(
                                "select",
                                &[("id", "chainDepth"), ("class", "select")],
                                vec![
                                    option("", "1", false),
                                    option("", "2", false),
                                    option("", "3", true),
                                    option("", "4", false),
                                    option("", "5", false),
                                    option("", "6", false),
                                ],
                            ),
                            el(
                                "select",
                                &[("id", "chainDir"), ("class", "select")],
                                vec![
                                    option("forward", "forward", true),
                                    option("backward", "backward", false),
                                    option("both", "both", false),
                                ],
                            ),
                            button(
                                "chainRun",
                                "Walk the resolve chain from the selected node",
                                "Run",
                            ),
                            button("chainFlatBtn", "Toggle flat list", "Flat"),
                        ],
                    ),
                    el(
                        "div",
                        &[("id", "chainTree"), ("class", "chain-tree")],
                        vec![el(
                            "div",
                            &[("class", "list-empty")],
                            vec![t("Select a node, then Run.")],
                        )],
                    ),
                ],
            ),
        ],
    )
}

fn button(id: &'static str, title: &'static str, label: &'static str) -> Node {
    el(
        "button",
        &[
            ("id", id),
            ("class", "btn btn-ghost"),
            ("type", "button"),
            ("title", title),
        ],
        vec![t(label)],
    )
}

fn option(value: &'static str, label: &'static str, selected: bool) -> Node {
    if value.is_empty() {
        if selected {
            el("option", &[("selected", "")], vec![t(label)])
        } else {
            el("option", &[], vec![t(label)])
        }
    } else if selected {
        el(
            "option",
            &[("value", value), ("selected", "")],
            vec![t(label)],
        )
    } else {
        el("option", &[("value", value)], vec![t(label)])
    }
}

#[cfg(test)]
mod tests {
    use super::super::testsupport;
    use super::*;

    #[test]
    fn side_rest_matches_reference() {
        testsupport::assert_children_match(&nodes(), "aside.side", 1);
    }

    /// `#sourcePane` is `main.layout`'s third element child in the reference.
    #[test]
    fn source_pane_matches_reference() {
        testsupport::assert_subtree_match(&source_pane(), "#sourcePane");
    }
}
