/*! Rust-built region: `#reorgModal`, `#editModal`, `#previewModal` (body children).
//!
//! Transcribed from `frontend/index.html`; parity is asserted against the
//! reference parse by `super::testsupport` (test-only). */

use super::{el, t, Node};

pub fn nodes() -> Vec<Node> {
    vec![reorg_modal(), edit_modal(), preview_modal()]
}

/// A plain `.settings-row`: a label followed by its control.
fn row(label_for: &'static str, label: &'static str, control: Node) -> Node {
    el(
        "div",
        &[("class", "settings-row")],
        vec![
            el(
                "label",
                &[("class", "settings-label"), ("for", label_for)],
                vec![t(label)],
            ),
            control,
        ],
    )
}

/// A `.settings-row` carrying extra attributes on the row itself.
fn row_with(
    row_attrs: &[(&'static str, &'static str)],
    label_for: &'static str,
    label: &'static str,
    control: Node,
) -> Node {
    el(
        "div",
        row_attrs,
        vec![
            el(
                "label",
                &[("class", "settings-label"), ("for", label_for)],
                vec![t(label)],
            ),
            control,
        ],
    )
}

/// A `select.select` with `(value, text)` options.
fn select(id: &'static str, options: &[(&'static str, &'static str)]) -> Node {
    let children: Vec<Node> = options
        .iter()
        .map(|(value, text)| el("option", &[("value", *value)], vec![t(*text)]))
        .collect();
    el("select", &[("id", id), ("class", "select")], children)
}

fn input(attrs: &[(&'static str, &'static str)]) -> Node {
    el("input", attrs, vec![])
}

/// A `button.btn` whose text is its label.
fn button(
    id: &'static str,
    class: &'static str,
    title: &'static str,
    text: &'static str,
) -> Node {
    el(
        "button",
        &[
            ("id", id),
            ("class", class),
            ("type", "button"),
            ("title", title),
        ],
        vec![t(text)],
    )
}

fn reorg_modal() -> Node {
    el(
        "div",
        &[
            ("id", "reorgModal"),
            ("class", "modal-backdrop hidden"),
            ("aria-hidden", "true"),
        ],
        vec![el(
            "div",
            &[
                ("class", "modal modal-reorg"),
                ("role", "dialog"),
                ("aria-modal", "true"),
                ("aria-labelledby", "reorgTitle"),
            ],
            vec![
                el(
                    "div",
                    &[("class", "modal-head")],
                    vec![
                        el("h2", &[("id", "reorgTitle")], vec![t("Reorganise & export")]),
                        el(
                            "button",
                            &[
                                ("id", "reorgClose"),
                                ("class", "btn btn-ghost"),
                                ("type", "button"),
                                ("title", "Close the export dialog"),
                                ("aria-label", "Close"),
                            ],
                            vec![t("✕")],
                        ),
                    ],
                ),
                el(
                    "div",
                    &[("class", "modal-body")],
                    vec![
                        el(
                            "p",
                            &[("class", "reorg-lede")],
                            vec![t("Rebuild the indexed C# as one file, or as chunks you choose.\n        Every declaration and every byte of code is kept: only the file layout changes.")],
                        ),
                        row(
                            "reorgMode",
                            "Output",
                            select(
                                "reorgMode",
                                &[("single", "One merged file"), ("split", "Split into chunks")],
                            ),
                        ),
                        row(
                            "reorgScope",
                            "What to move",
                            select(
                                "reorgScope",
                                &[
                                    ("selection", "The graph selection"),
                                    ("all", "The whole corpus"),
                                ],
                            ),
                        ),
                        row_with(
                            &[("class", "settings-row"), ("id", "reorgGranRow")],
                            "reorgGran",
                            "Chunk unit",
                            select(
                                "reorgGran",
                                &[
                                    ("leaf", "One declaration per chunk"),
                                    ("top", "Whole top-level types"),
                                ],
                            ),
                        ),
                        row_with(
                            &[("class", "settings-row"), ("id", "reorgGroupRow")],
                            "reorgGroup",
                            "Grouping",
                            select(
                                "reorgGroup",
                                &[
                                    ("size", "By size budget"),
                                    ("namespace", "By namespace"),
                                    ("owner", "By owning type"),
                                    ("single", "Everything in one file"),
                                ],
                            ),
                        ),
                        row_with(
                            &[("class", "settings-row"), ("id", "reorgSizeRow")],
                            "reorgSize",
                            "Lines per file",
                            input(&[
                                ("id", "reorgSize"),
                                ("class", "text-input"),
                                ("type", "number"),
                                ("min", "50"),
                                ("step", "50"),
                                ("value", "3000"),
                            ]),
                        ),
                        row(
                            "reorgDest",
                            "Write to",
                            input(&[
                                ("id", "reorgDest"),
                                ("class", "text-input"),
                                ("type", "text"),
                                ("spellcheck", "false"),
                                (
                                    "placeholder",
                                    "absolute folder — leave empty to only download",
                                ),
                            ]),
                        ),
                        row(
                            "reorgHeader",
                            "Banner comment",
                            input(&[
                                ("id", "reorgHeader"),
                                ("type", "checkbox"),
                                ("checked", ""),
                                (
                                    "title",
                                    "Prefix each file with a short auto-generated comment",
                                ),
                            ]),
                        ),
                        el(
                            "div",
                            &[("id", "reorgReport"), ("class", "reorg-report")],
                            vec![],
                        ),
                    ],
                ),
                el(
                    "div",
                    &[("class", "modal-foot")],
                    vec![
                        el(
                            "span",
                            &[("id", "reorgNote"), ("class", "modal-note")],
                            vec![],
                        ),
                        button(
                            "reorgCompile",
                            "btn",
                            "Compile the source and the result with Unity's Roslyn and compare the diagnostics",
                            "Compile",
                        ),
                        button(
                            "reorgVerify",
                            "btn",
                            "Build the output and prove every declaration and byte survives",
                            "Verify",
                        ),
                        button(
                            "reorgWrite",
                            "btn",
                            "Write the reorganised files to the folder above",
                            "Write",
                        ),
                        button(
                            "reorgZip",
                            "btn btn-primary",
                            "Download the reorganised source as a .zip",
                            "Download .zip",
                        ),
                    ],
                ),
            ],
        )],
    )
}

fn edit_modal() -> Node {
    el(
        "div",
        &[
            ("id", "editModal"),
            ("class", "modal-backdrop hidden"),
            ("aria-hidden", "true"),
        ],
        vec![el(
            "div",
            &[
                ("class", "modal modal-edit"),
                ("role", "dialog"),
                ("aria-modal", "true"),
                ("aria-labelledby", "editTitle"),
            ],
            vec![
                el(
                    "div",
                    &[("class", "modal-head")],
                    vec![
                        el("h2", &[("id", "editTitle")], vec![t("Edit tools")]),
                        el(
                            "button",
                            &[
                                ("id", "editClose"),
                                ("class", "btn btn-ghost"),
                                ("type", "button"),
                                ("title", "Close the edit dialog"),
                                ("aria-label", "Close"),
                            ],
                            vec![t("✕")],
                        ),
                    ],
                ),
                el(
                    "div",
                    &[("class", "modal-body")],
                    vec![
                        el(
                            "p",
                            &[("class", "reorg-lede")],
                            vec![
                                t("The same named operations an agent has over MCP, dispatched by the host's shared edit layer.\n        Every write tool defaults to a dry run;"),
                                el("b", &[], vec![t("Apply")]),
                                t("sends"),
                                el("code", &[], vec![t("apply: true")]),
                                t("."),
                                el("b", &[], vec![t("Validate")]),
                                t("runs the Roslyn parse gate over the corpus (a missing toolchain is reported\n        as skipped, never as a pass) and"),
                                el("b", &[], vec![t("Duplicates")]),
                                t("reports repeated code."),
                            ],
                        ),
                        row("editTool", "Operation", select("editTool", &[])),
                        row_with(
                            &[
                                ("class", "settings-row"),
                                ("style", "align-items:flex-start"),
                            ],
                            "editArgs",
                            "Args (JSON)",
                            el(
                                "textarea",
                                &[
                                    ("id", "editArgs"),
                                    ("class", "text-input"),
                                    ("spellcheck", "false"),
                                    ("rows", "7"),
                                    (
                                        "style",
                                        "flex:1 1 auto; min-width:0; font-family:var(--mono); font-size:11.5px; resize:vertical",
                                    ),
                                    (
                                        "placeholder",
                                        "{\"like\": [\"sps\", \"pcs\"], \"limit\": 50}",
                                    ),
                                ],
                                vec![t("{}")],
                            ),
                        ),
                        el(
                            "pre",
                            &[
                                ("id", "editOut"),
                                ("class", "reorg-report"),
                                ("aria-live", "polite"),
                            ],
                            vec![],
                        ),
                    ],
                ),
                el(
                    "div",
                    &[("class", "modal-foot")],
                    vec![
                        el("span", &[("id", "editNote"), ("class", "modal-note")], vec![]),
                        button(
                            "editValidate",
                            "btn",
                            "Run the Roslyn parse gate over the corpus (reports a skipped reason honestly)",
                            "Validate",
                        ),
                        button(
                            "editDedupe",
                            "btn",
                            "Report exact/near duplicate bodies and repeated blocks",
                            "Duplicates",
                        ),
                        button(
                            "editDry",
                            "btn",
                            "Preview the plan without writing anything",
                            "Dry-run",
                        ),
                        button(
                            "editApply",
                            "btn btn-primary",
                            "Run the tool with apply:true (write tools only)",
                            "Apply",
                        ),
                    ],
                ),
            ],
        )],
    )
}

fn preview_modal() -> Node {
    el(
        "div",
        &[
            ("id", "previewModal"),
            ("class", "modal-backdrop hidden"),
            ("aria-hidden", "true"),
        ],
        vec![el(
            "div",
            &[
                ("class", "modal modal-preview"),
                ("role", "dialog"),
                ("aria-modal", "true"),
                ("aria-labelledby", "previewTitle"),
            ],
            vec![
                el(
                    "div",
                    &[("class", "modal-head")],
                    vec![
                        el("h2", &[("id", "previewTitle")], vec![t("Change preview")]),
                        el(
                            "button",
                            &[
                                ("id", "previewClose"),
                                ("class", "btn btn-ghost"),
                                ("type", "button"),
                                ("title", "Close the preview dialog"),
                                ("aria-label", "Close"),
                            ],
                            vec![t("✕")],
                        ),
                    ],
                ),
                el(
                    "div",
                    &[("class", "modal-body")],
                    vec![
                        el(
                            "p",
                            &[("class", "reorg-lede")],
                            vec![
                                t("What would change"),
                                el("b", &[], vec![t("before")]),
                                t("anything is written: the current and proposed\n        signature, every affected declaration and call site with its line number, and the\n        exact edits an apply would make. This panel is read-only — it has no apply path\n        and the engine always reports"),
                                el("code", &[], vec![t("writes: []")]),
                                t("."),
                            ],
                        ),
                        row(
                            "previewSelector",
                            "Declaration",
                            input(&[
                                ("id", "previewSelector"),
                                ("class", "text-input"),
                                ("type", "text"),
                                ("spellcheck", "false"),
                                (
                                    "placeholder",
                                    "node id, full name, or a unique simple name (defaults to the selection)",
                                ),
                            ]),
                        ),
                        row("previewOp", "Change", select("previewOp", &[])),
                        row(
                            "previewTo",
                            "New name / type",
                            input(&[
                                ("id", "previewTo"),
                                ("class", "text-input"),
                                ("type", "text"),
                                ("spellcheck", "false"),
                                ("placeholder", "e.g. Plus  —  or  long"),
                            ]),
                        ),
                        el(
                            "div",
                            &[("id", "previewOut"), ("class", "reorg-report preview-out")],
                            vec![],
                        ),
                    ],
                ),
                el(
                    "div",
                    &[("class", "modal-foot")],
                    vec![
                        el(
                            "span",
                            &[("id", "previewNote"), ("class", "modal-note")],
                            vec![],
                        ),
                        button(
                            "previewRun",
                            "btn btn-primary",
                            "Plan the change — read-only, writes nothing",
                            "Preview",
                        ),
                    ],
                ),
            ],
        )],
    )
}

#[cfg(test)]
mod tests {
    use super::super::testsupport;
    use super::*;

    #[test]
    fn modals_b_matches_reference() {
        testsupport::assert_children_match(&nodes(), "body", 5);
    }
}
