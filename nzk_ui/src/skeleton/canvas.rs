/*! Rust-built region: The canvas overlays: `#graph`, `#tooltip`, `#emptyHint`, `#notice`, `.canvas-tools`, `#loading` (children of `#canvasWrap`).
//!
//! Transcribed from `frontend/index.html`; parity is asserted against the
//! reference parse by `super::testsupport` (test-only). */

use super::{el, t, Node};

pub fn nodes() -> Vec<Node> {
    vec![
        el(
            "canvas",
            &[
                ("id", "graph"),
                ("aria-label", "Function relation graph"),
            ],
            vec![],
        ),
        el(
            "div",
            &[("id", "tooltip"), ("class", "tooltip hidden"), ("role", "tooltip")],
            vec![],
        ),
        el(
            "div",
            &[("id", "emptyHint"), ("class", "empty-hint hidden")],
            vec![],
        ),
        el(
            "div",
            &[("id", "notice"), ("class", "notice hidden")],
            vec![],
        ),
        el(
            "div",
            &[("class", "canvas-tools")],
            vec![
                el(
                    "button",
                    &[
                        ("id", "backBtn"),
                        ("class", "btn btn-ghost"),
                        ("type", "button"),
                        ("title", "Back to the previous node (Backspace)"),
                    ],
                    vec![t("\u{2190} Back")],
                ),
                el(
                    "button",
                    &[
                        ("id", "upBtn"),
                        ("class", "btn btn-ghost"),
                        ("type", "button"),
                        ("title", "Go up to the parent (namespace / type)"),
                    ],
                    vec![t("\u{2191} Up")],
                ),
                el(
                    "button",
                    &[
                        ("id", "zoomInBtn"),
                        ("class", "btn btn-ghost"),
                        ("type", "button"),
                        ("title", "Zoom in (+)"),
                    ],
                    vec![t("+")],
                ),
                el(
                    "button",
                    &[
                        ("id", "zoomOutBtn"),
                        ("class", "btn btn-ghost"),
                        ("type", "button"),
                        ("title", "Zoom out (\u{2212})"),
                    ],
                    vec![t("\u{2212}")],
                ),
                el(
                    "button",
                    &[
                        ("id", "fitBtn"),
                        ("class", "btn btn-ghost"),
                        ("type", "button"),
                        ("title", "Fit graph to view"),
                    ],
                    vec![t("Fit")],
                ),
                el(
                    "button",
                    &[
                        ("id", "wholeGraphBtn"),
                        ("class", "btn btn-ghost"),
                        ("type", "button"),
                        ("title", "Load every namespace and its types"),
                    ],
                    vec![t("Full graph")],
                ),
                el(
                    "button",
                    &[
                        ("id", "resettleBtn"),
                        ("class", "btn btn-ghost"),
                        ("type", "button"),
                        ("title", "Re-run the FA2 + Noverlap layout (R)"),
                    ],
                    vec![t("Re-settle")],
                ),
                el(
                    "span",
                    &[("id", "layoutState"), ("class", "layout-state")],
                    vec![],
                ),
            ],
        ),
        el(
            "div",
            &[("id", "loading"), ("class", "loading hidden")],
            vec![
                el("div", &[("class", "spinner"), ("aria-hidden", "true")], vec![]),
                el(
                    "div",
                    &[("id", "loadingText"), ("class", "loading-text")],
                    vec![t("Working\u{2026}")],
                ),
                el(
                    "button",
                    &[
                        ("id", "stopIndexBtn"),
                        ("class", "btn btn-ghost"),
                        ("type", "button"),
                        ("style", "display:none"),
                        (
                            "title",
                            "Stop the running index. The previous graph stays loaded and usable.",
                        ),
                    ],
                    vec![t("Stop")],
                ),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::super::testsupport;
    use super::*;

    #[test]
    fn canvas_matches_reference() {
        testsupport::assert_children_match(&nodes(), "#canvasWrap", 0);
    }
}
