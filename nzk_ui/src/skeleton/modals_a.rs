/*! Rust-built region: `#modal`, `#shortcutsModal`, `#toasts` (body children).
//!
//! Transcribed from `frontend/index.html`; parity is asserted against the
//! reference parse by `super::testsupport` (test-only). */

use super::{el, t, Node};

pub fn nodes() -> Vec<Node> {
    vec![folder_modal(), shortcuts_modal(), toasts()]
}

/// `div#modal` — the folder-browser dialog.
fn folder_modal() -> Node {
    el(
        "div",
        &[
            ("id", "modal"),
            ("class", "modal-backdrop hidden"),
            ("aria-hidden", "true"),
        ],
        vec![el(
            "div",
            &[
                ("class", "modal"),
                ("role", "dialog"),
                ("aria-modal", "true"),
                ("aria-labelledby", "modalTitle"),
            ],
            vec![
                el(
                    "div",
                    &[("class", "modal-head")],
                    vec![
                        el(
                            "h2",
                            &[("id", "modalTitle")],
                            vec![t("Choose a folder to index")],
                        ),
                        el(
                            "button",
                            &[
                                ("id", "modalClose"),
                                ("class", "btn btn-ghost"),
                                ("type", "button"),
                                ("title", "Close the folder browser"),
                                ("aria-label", "Close"),
                            ],
                            vec![t("\u{2715}")],
                        ),
                    ],
                ),
                el(
                    "div",
                    &[("class", "modal-path")],
                    vec![
                        el(
                            "label",
                            &[("class", "sr-only"), ("for", "modalPathInput")],
                            vec![t("Folder path")],
                        ),
                        el(
                            "input",
                            &[
                                ("id", "modalPathInput"),
                                ("class", "text-input modal-path-input"),
                                ("type", "text"),
                                ("spellcheck", "false"),
                                ("autocomplete", "off"),
                                ("placeholder", "/absolute/path or repo-relative folder\u{2026}"),
                            ],
                            vec![],
                        ),
                        el(
                            "button",
                            &[
                                ("id", "modalGo"),
                                ("class", "btn"),
                                ("type", "button"),
                                ("title", "Open this folder"),
                            ],
                            vec![t("Go")],
                        ),
                    ],
                ),
                el(
                    "div",
                    &[("class", "modal-path")],
                    vec![
                        el(
                            "button",
                            &[
                                ("id", "modalUp"),
                                ("class", "btn btn-ghost"),
                                ("type", "button"),
                                ("title", "Parent folder"),
                            ],
                            vec![t("\u{2191} Up")],
                        ),
                        el(
                            "button",
                            &[
                                ("id", "modalHome"),
                                ("class", "btn btn-ghost"),
                                ("type", "button"),
                                ("title", "Home folder"),
                            ],
                            vec![t("Home")],
                        ),
                        el(
                            "button",
                            &[
                                ("id", "modalRoot"),
                                ("class", "btn btn-ghost"),
                                ("type", "button"),
                                ("title", "Filesystem root"),
                            ],
                            vec![t("Root /")],
                        ),
                        el(
                            "button",
                            &[
                                ("id", "modalRepo"),
                                ("class", "btn btn-ghost"),
                                ("type", "button"),
                                ("title", "Repository root"),
                            ],
                            vec![t("Repo")],
                        ),
                        el(
                            "code",
                            &[("id", "modalPath"), ("class", "modal-path-text")],
                            vec![t("/")],
                        ),
                    ],
                ),
                el(
                    "div",
                    &[("id", "modalBody"), ("class", "modal-body")],
                    vec![],
                ),
                el(
                    "div",
                    &[("class", "modal-foot")],
                    vec![
                        el(
                            "span",
                            &[("id", "modalNote"), ("class", "modal-note")],
                            vec![],
                        ),
                        el(
                            "button",
                            &[
                                ("id", "modalCancel"),
                                ("class", "btn"),
                                ("type", "button"),
                                ("title", "Close without changing the indexed folder"),
                            ],
                            vec![t("Cancel")],
                        ),
                        el(
                            "button",
                            &[
                                ("id", "modalSelect"),
                                ("class", "btn btn-primary"),
                                ("type", "button"),
                                ("title", "Index the folder shown above"),
                                ("disabled", ""),
                            ],
                            vec![t("Select this folder")],
                        ),
                    ],
                ),
            ],
        )],
    )
}

/// `div#shortcutsModal` — the shortcuts and theme dialog.
fn shortcuts_modal() -> Node {
    el(
        "div",
        &[
            ("id", "shortcutsModal"),
            ("class", "modal-backdrop hidden"),
            ("aria-hidden", "true"),
        ],
        vec![el(
            "div",
            &[
                ("class", "modal modal-shortcuts"),
                ("role", "dialog"),
                ("aria-modal", "true"),
                ("aria-labelledby", "shortcutsTitle"),
            ],
            vec![
                el(
                    "div",
                    &[("class", "modal-head")],
                    vec![
                        el(
                            "h2",
                            &[("id", "shortcutsTitle")],
                            vec![t("Keyboard shortcuts")],
                        ),
                        el(
                            "button",
                            &[
                                ("id", "shortcutsClose"),
                                ("class", "btn btn-ghost"),
                                ("type", "button"),
                                ("title", "Close the shortcuts and settings dialog"),
                                ("aria-label", "Close"),
                            ],
                            vec![t("\u{2715}")],
                        ),
                    ],
                ),
                el(
                    "div",
                    &[("class", "settings-row")],
                    vec![
                        el(
                            "label",
                            &[("class", "settings-label"), ("for", "uiTheme")],
                            vec![t("UI theme")],
                        ),
                        el(
                            "select",
                            &[
                                ("id", "uiTheme"),
                                ("class", "select"),
                                // UI copy pinned to the reference document
                                // (`frontend/index.html`); a parity test compares it.
                                ("title", "Whole-viewer theme: chrome and node kinds"),
                            ],
                            vec![
                                el(
                                    "option",
                                    &[("value", "nzk-dark+")],
                                    vec![t("nzk-dark+ \u{b7} VS Code Dark+")],
                                ),
                                el(
                                    "option",
                                    &[("value", "dark-v1")],
                                    vec![t("dark v1")],
                                ),
                            ],
                        ),
                    ],
                ),
                el(
                    "div",
                    &[
                        ("id", "shortcutsBody"),
                        ("class", "modal-body shortcuts-body"),
                    ],
                    vec![],
                ),
                el(
                    "div",
                    &[("class", "modal-foot")],
                    vec![
                        el(
                            "span",
                            &[("class", "modal-note")],
                            vec![
                                t("Shortcuts are global; press "),
                                el("kbd", &[], vec![t("?")]),
                                t(" to reopen this."),
                            ],
                        ),
                        el(
                            "button",
                            &[
                                ("id", "shortcutsDone"),
                                ("class", "btn btn-primary"),
                                ("type", "button"),
                                ("title", "Close the shortcuts and settings dialog"),
                            ],
                            vec![t("Done")],
                        ),
                    ],
                ),
            ],
        )],
    )
}

/// `div#toasts` — the toast container (filled at runtime).
fn toasts() -> Node {
    el(
        "div",
        &[
            ("id", "toasts"),
            ("class", "toasts"),
            ("aria-live", "polite"),
        ],
        vec![],
    )
}

#[cfg(test)]
mod tests {
    use super::super::testsupport;
    use super::*;

    /// `#modal`, `#shortcutsModal` and `#toasts` match the reference parse.
    #[test]
    fn modals_a_matches_reference() {
        testsupport::assert_children_match(&nodes(), "body", 2);
    }
}
