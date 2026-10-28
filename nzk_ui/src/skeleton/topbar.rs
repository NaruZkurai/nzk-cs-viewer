/*! Rust-built region: `<header class="topbar">` (the whole top bar).
//!
//! Transcribed from `frontend/index.html` lines 54-105; parity is asserted
//! against the reference parse by `super::testsupport` (test-only). */

use super::{el, svg_el, t, Node};

pub fn nodes() -> Vec<Node> {
    vec![el(
        "header",
        &[("class", "topbar")],
        vec![
            el(
                "div",
                &[
                    ("class", "brand"),
                    ("title", "csgraph \u{2014} C# function relation graph"),
                ],
                vec![el("span", &[("class", "brand-mark")], vec![]), t("csgraph")],
            ),
            el(
                "div",
                &[("class", "rootpick")],
                vec![
                    el(
                        "label",
                        &[("class", "sr-only"), ("for", "rootInput")],
                        vec![t("Root folder")],
                    ),
                    el(
                        "input",
                        &[
                            ("id", "rootInput"),
                            ("class", "text-input"),
                            ("type", "text"),
                            ("spellcheck", "false"),
                            ("autocomplete", "off"),
                            (
                                "placeholder",
                                "/absolute/path or repo-relative folder\u{2026}",
                            ),
                        ],
                        vec![],
                    ),
                    el(
                        "button",
                        &[
                            ("id", "browseBtn"),
                            ("class", "btn"),
                            ("type", "button"),
                            (
                                "title",
                                "Pick the folder to index \u{2014} opens the folder browser",
                            ),
                        ],
                        vec![t("Browse")],
                    ),
                    el(
                        "button",
                        &[
                            ("id", "indexBtn"),
                            ("class", "btn btn-primary"),
                            ("type", "button"),
                            (
                                "title",
                                "Build the graph from the chosen folder (re-runs parsing and indexing)",
                            ),
                            ("disabled", ""),
                        ],
                        vec![t("Index")],
                    ),
                ],
            ),
            el(
                "div",
                &[("class", "searchwrap")],
                vec![
                    el(
                        "div",
                        &[
                            ("class", "mode-switch"),
                            ("role", "tablist"),
                            ("aria-label", "Search mode"),
                        ],
                        vec![
                            el(
                                "button",
                                &[
                                    ("id", "modeGraph"),
                                    ("class", "mode active"),
                                    ("type", "button"),
                                    ("role", "tab"),
                                    ("aria-selected", "true"),
                                    ("title", "Graph search (/)"),
                                ],
                                vec![t("Graph")],
                            ),
                            el(
                                "button",
                                &[
                                    ("id", "modeFind"),
                                    ("class", "mode"),
                                    ("type", "button"),
                                    ("role", "tab"),
                                    ("aria-selected", "false"),
                                    (
                                        "title",
                                        "Find definitions, implementations and usages (f)",
                                    ),
                                ],
                                vec![t("Find")],
                            ),
                        ],
                    ),
                    el(
                        "label",
                        &[("class", "sr-only"), ("for", "searchInput")],
                        vec![t("Search symbols")],
                    ),
                    el(
                        "input",
                        &[
                            ("id", "searchInput"),
                            ("class", "text-input"),
                            ("type", "text"),
                            ("spellcheck", "false"),
                            ("autocomplete", "off"),
                            ("placeholder", "Search symbols\u{2026}  ( / )"),
                            ("role", "combobox"),
                            ("aria-expanded", "false"),
                            ("aria-controls", "suggestions"),
                        ],
                        vec![],
                    ),
                    el(
                        "div",
                        &[
                            ("id", "suggestions"),
                            ("class", "suggestions hidden"),
                            ("role", "listbox"),
                            ("aria-label", "Search suggestions"),
                        ],
                        vec![],
                    ),
                ],
            ),
            el(
                "div",
                &[
                    ("id", "statusLine"),
                    ("class", "status"),
                    ("title", "Index status"),
                ],
                vec![t("not indexed")],
            ),
            el(
                "button",
                &[
                    ("id", "exportBtn"),
                    ("class", "btn btn-ghost"),
                    ("type", "button"),
                    ("title", "Merge or split the indexed C# and download it"),
                    ("aria-haspopup", "dialog"),
                    ("aria-controls", "reorgModal"),
                ],
                vec![t("Export")],
            ),
            el(
                "button",
                &[
                    ("id", "editBtn"),
                    ("class", "btn btn-ghost"),
                    ("type", "button"),
                    (
                        "title",
                        "Run the shared edit tools: rename, qualify, select, create and patch source",
                    ),
                    ("aria-haspopup", "dialog"),
                    ("aria-controls", "editModal"),
                ],
                vec![t("Edit")],
            ),
            el(
                "button",
                &[
                    ("id", "blocksBtn"),
                    ("class", "btn btn-ghost"),
                    ("type", "button"),
                    (
                        "title",
                        "Blocks \u{2014} a read-only Scratch-like view of the current document: declarations as typed-port cards, reference edges between them (b)",
                    ),
                    ("aria-pressed", "false"),
                ],
                vec![t("Blocks")],
            ),
            el(
                "button",
                &[
                    ("id", "previewBtn"),
                    ("class", "btn btn-ghost"),
                    ("type", "button"),
                    (
                        "title",
                        "Preview the signatures and call sites a rename or retype would change \u{2014} read-only, writes nothing",
                    ),
                    ("aria-haspopup", "dialog"),
                    ("aria-controls", "previewModal"),
                ],
                vec![t("Preview")],
            ),
            el(
                "button",
                &[
                    ("id", "shortcutsBtn"),
                    ("class", "btn btn-ghost shortcuts-btn"),
                    ("type", "button"),
                    ("title", "Shortcuts & settings (?)"),
                    ("aria-label", "Shortcuts and settings"),
                    ("aria-haspopup", "dialog"),
                    ("aria-controls", "shortcutsModal"),
                ],
                vec![svg_el(
                    "svg",
                    &[
                        ("class", "shortcuts-gear"),
                        ("viewBox", "0 0 24 24"),
                        ("width", "15"),
                        ("height", "15"),
                        ("aria-hidden", "true"),
                        ("focusable", "false"),
                    ],
                    vec![svg_el(
                        "path",
                        &[
                            ("fill", "currentColor"),
                            (
                                "d",
                                "M19.14 12.94a7.07 7.07 0 0 0 0-1.88l2.03-1.58a.5.5 0 0 0 .12-.64l-1.92-3.32a.5.5 0 0 0-.61-.22l-2.39.96a7.03 7.03 0 0 0-1.62-.94l-.36-2.54a.5.5 0 0 0-.5-.42H9.61a.5.5 0 0 0-.5.42l-.36 2.54a7.03 7.03 0 0 0-1.62.94l-2.39-.96a.5.5 0 0 0-.61.22L2.21 8.84a.5.5 0 0 0 .12.64l2.03 1.58a7.07 7.07 0 0 0 0 1.88L2.33 14.52a.5.5 0 0 0-.12.64l1.92 3.32a.5.5 0 0 0 .61.22l2.39-.96a7.03 7.03 0 0 0 1.62.94l.36 2.54a.5.5 0 0 0 .5.42h3.84a.5.5 0 0 0 .5-.42l.36-2.54a7.03 7.03 0 0 0 1.62-.94l2.39.96a.5.5 0 0 0 .61-.22l1.92-3.32a.5.5 0 0 0-.12-.64Zm-7.14 2.66A3.6 3.6 0 1 1 15.6 12 3.6 3.6 0 0 1 12 15.6Z",
                            ),
                        ],
                        vec![],
                    )],
                )],
            ),
        ],
    )]
}

#[cfg(test)]
mod tests {
    use super::super::testsupport;
    use super::*;

    #[test]
    fn topbar_matches_reference() {
        testsupport::assert_subtree_match(&nodes()[0], "header.topbar");
    }
}
