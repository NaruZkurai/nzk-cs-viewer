//! typography — the faces the reference ACTUALLY resolved, pinned for the fork.
//!
//! # Why this file exists
//!
//! Matching the requested CSS stack (`--mono`, `--sans`) is not the same as
//! matching the face Chromium actually used. `tools/fontprobe.mjs` interrogates
//! the running original over CDP with `CSS.getPlatformFontsForNode`; measured
//! 2026-10-16 at 1600x1000 on the ab-corpus, the resolved faces are:
//!
//! | surface | requested | RESOLVED (postscript) | evidence |
//! |---|---|---|---|
//! | DOM nzk_ui mono (top bar, status, sidebar labels, buttons, notice) | `ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace` | **Liberation Mono** (`LiberationMono`) | 17 surfaces, 329 glyphs |
//! | sans body / `.hint` | `system-ui, -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif` | **Noto Sans** (`NotoSans-Regular`) | `p.hint`, body |
//! | canvas chip + edge labels (JS `ctx.font`) | `ui-monospace, Menlo, Consolas, monospace` | **Noto Sans Mono** (`NotoSansMono-Regular`) | DOM proxy with the byte-identical `font` shorthand |
//!
//! The canvas face is *different* from the nzk_ui face even though both stacks
//! name a monospace: `"Liberation Mono"` is explicit in `--mono`, while the
//! canvas stack falls through `ui-monospace`/`monospace` to Noto Sans Mono.
//!
//! egui's own defaults are Ubuntu-Light (proportional) and Hack (monospace) —
//! neither is what the reference uses, so every glyph reached the diff with the
//! wrong shape. This module installs the resolved faces and exposes helpers so
//! the canvas gets the canvas face and nzk_ui gets the nzk_ui face; the mapping
//! is measured, not guessed.
//!
//! Faces are loaded from their system paths at runtime. A missing file is never
//! fatal: the face is skipped and egui's bundled default stays as the fallback,
//! with the miss reported by [`report`] so a wrong-face run cannot pass silently.
//!
//! # The one call
//!
//! egui applies `Context::set_fonts` on the NEXT pass, and the offscreen A/B
//! path runs exactly one pass, so `install` must be called on a fresh `Context`
//! before its first `run_ui`:
//!
//! ```ignore
//! let ctx = egui::Context::default();
//! crate::typography::install(&ctx);   // before ctx.run_ui(...)
//! ```
//!
//! Set `NCV_TYPOGRAPHY=off` to skip the install. That is the A/B control: the
//! SAME binary then renders with egui's bundled faces (Ubuntu-Light/Hack), so
//! the effect of the measured faces can be isolated from every other change on
//! the tree instead of being confounded with them.

use std::sync::Arc;

use egui::{Context, FontData, FontDefinitions, FontFamily, FontId};
use serde_json::{json, Value};

/// Family key for the canvas face (Noto Sans Mono, `render.js` chip labels).
pub const CANVAS_FAMILY: &str = "ncv-noto-sans-mono";
/// Family key for the DOM nzk_ui mono face (Liberation Mono).
pub const NZK_UI_MONO_FAMILY: &str = "ncv-liberation-mono";
/// Family key for the sans face (Noto Sans, body / `.hint`).
pub const SANS_FAMILY: &str = "ncv-noto-sans";

/// One measured face: `key` names it inside egui, `file` is where it came from.
#[derive(Clone, Copy, Debug)]
pub struct Face {
    pub key: &'static str,
    pub family: &'static str,
    pub postscript: &'static str,
    pub path: &'static str,
}

/// The regular-weight faces, in fallback order per family.
pub const REGULAR: &[Face] = &[
    Face {
        key: "ncv-noto-sans-mono-regular",
        family: CANVAS_FAMILY,
        postscript: "NotoSansMono-Regular",
        path: "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    },
    Face {
        key: "ncv-liberation-mono-regular",
        family: NZK_UI_MONO_FAMILY,
        postscript: "LiberationMono",
        path: "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
    },
    Face {
        key: "ncv-noto-sans-regular",
        family: SANS_FAMILY,
        postscript: "NotoSans-Regular",
        path: "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    },
];

/// The bold faces (the reference measured weight 400 everywhere, but egui asks
/// for bold for `strong`/headings; a real bold face beats a synthesised one).
pub const BOLD: &[Face] = &[
    Face {
        key: "ncv-noto-sans-mono-bold",
        family: CANVAS_FAMILY,
        postscript: "NotoSansMono-Bold",
        path: "/usr/share/fonts/noto/NotoSansMono-Bold.ttf",
    },
    Face {
        key: "ncv-liberation-mono-bold",
        family: NZK_UI_MONO_FAMILY,
        postscript: "LiberationMono-Bold",
        path: "/usr/share/fonts/liberation/LiberationMono-Bold.ttf",
    },
    Face {
        key: "ncv-noto-sans-bold",
        family: SANS_FAMILY,
        postscript: "NotoSans-Bold",
        path: "/usr/share/fonts/noto/NotoSans-Bold.ttf",
    },
];

fn read_face(face: &Face) -> Option<Vec<u8>> {
    std::fs::read(face.path).ok()
}

/// Build the definitions `install` applies. Pure except for reading the font
/// files, so a unit test can assert the mapping without an egui context.
pub fn build_definitions() -> (FontDefinitions, Vec<Value>) {
    let mut defs = FontDefinitions::default();
    let mut loaded = Vec::new();

    for face in REGULAR.iter().chain(BOLD.iter()) {
        match read_face(face) {
            Some(bytes) => {
                let len = bytes.len();
                defs.font_data
                    .insert(face.key.to_string(), Arc::new(FontData::from_owned(bytes)));
                loaded.push(json!({
                    "key": face.key, "family": face.family, "postscript": face.postscript,
                    "path": face.path, "bytes": len, "loaded": true,
                }));
            }
            None => loaded.push(json!({
                "key": face.key, "family": face.family, "postscript": face.postscript,
                "path": face.path, "loaded": false,
                "note": "file missing; egui default remains as fallback",
            })),
        }
    }

    // Named families: exact faces, no generic fallback in front of them.
    let family_of = |family: &str| -> Vec<String> {
        REGULAR
            .iter()
            .chain(BOLD.iter())
            .filter(|f| f.family == family)
            .map(|f| f.key.to_string())
            .collect()
    };

    defs.families
        .insert(FontFamily::Name(CANVAS_FAMILY.into()), family_of(CANVAS_FAMILY));
    defs.families.insert(
        FontFamily::Name(NZK_UI_MONO_FAMILY.into()),
        family_of(NZK_UI_MONO_FAMILY),
    );
    defs.families
        .insert(FontFamily::Name(SANS_FAMILY.into()), family_of(SANS_FAMILY));

    // egui's built-in families. The canvas draws with `FontId::monospace`, so
    // Monospace must resolve to the CANVAS face (Noto Sans Mono) — that is the
    // measured face of the chip/edge labels. Proportional is the sans face.
    // The built-in Hack / Ubuntu-Light entries stay at the end as fallbacks for
    // glyphs the measured faces lack (egui also appends its emoji fonts).
    for (family, key) in [
        (FontFamily::Monospace, "ncv-noto-sans-mono-regular"),
        (FontFamily::Proportional, "ncv-noto-sans-regular"),
    ] {
        let entry = defs.families.entry(family).or_default();
        if !entry.iter().any(|k| k == key) {
            entry.insert(0, key.to_string());
        }
    }

    (defs, loaded)
}

/// Install the measured faces on `ctx`, before its first pass (see the module
/// docs). Idempotent and non-fatal on a missing font file. `NCV_TYPOGRAPHY=off`
/// skips the install entirely (the A/B control).
pub fn install(ctx: &Context) {
    if matches!(
        std::env::var("NCV_TYPOGRAPHY").as_deref(),
        Ok("off") | Ok("0") | Ok("false") | Ok("no")
    ) {
        eprintln!("[typography] NCV_TYPOGRAPHY=off: egui default faces (A/B control)");
        return;
    }
    let (defs, loaded) = build_definitions();
    let missing: Vec<String> = loaded
        .iter()
        .filter(|v| v["loaded"] == json!(false))
        .map(|v| v["path"].as_str().unwrap_or("?").to_string())
        .collect();
    if !missing.is_empty() {
        eprintln!("[typography] missing face files, egui default remains: {missing:?}");
    }
    ctx.set_fonts(defs);
}

// Consumed by the nzk_ui/canvas call sites as the shell work lands; kept
// public (with the measured mapping) so those owners can adopt it without
// re-deriving the faces. `report` is the evidence/diagnostic entry point.
/// The canvas face at `size` px (chip/edge labels: measured Noto Sans Mono 11px
/// chips, 10px edge labels).
///
/// Wired at the node-label paint site and the chip-measurement callback
/// (`view.rs`), so both the painted glyphs and the boxes the layout reserves use
/// the measured face — the round-20 audit's 5,478 Δpx `.node-label` bucket.
pub fn canvas_mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(CANVAS_FAMILY.into()))
}

/// The DOM nzk_ui mono face at `size` px (measured Liberation Mono).
#[allow(dead_code)]
pub fn nzk_ui_mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(NZK_UI_MONO_FAMILY.into()))
}

/// The sans face at `size` px (measured Noto Sans; `.hint` is 11.5px/18.4px).
#[allow(dead_code)]
pub fn sans(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(SANS_FAMILY.into()))
}

/// The measured mappings, as data, for the evidence report and for other
/// renderers (nzk_ui pins the same faces in Parley).
#[allow(dead_code)]
pub fn report() -> Value {
    let (defs, loaded) = build_definitions();
    let families: Vec<Value> = [
        ("Monospace (egui canvas)", FontFamily::Monospace),
        ("Proportional (egui sans)", FontFamily::Proportional),
        ("canvas", FontFamily::Name(CANVAS_FAMILY.into())),
        ("nzk_ui-mono", FontFamily::Name(NZK_UI_MONO_FAMILY.into())),
        ("sans", FontFamily::Name(SANS_FAMILY.into())),
    ]
    .iter()
    .map(|(label, fam)| {
        json!({
            "label": label,
            "order": defs.families.get(fam).cloned().unwrap_or_default(),
        })
    })
    .collect();
    json!({
        "source": "tools/fontprobe.mjs (CSS.getPlatformFontsForNode) on the live original, 1600x1000",
        "faces": loaded,
        "families": families,
        "measured": [
            {"surface": "DOM nzk_ui mono", "resolved": "Liberation Mono", "postscript": "LiberationMono", "weight": 400, "stack": "ui-monospace, SFMono-Regular, \"SF Mono\", Menlo, Consolas, \"Liberation Mono\", monospace"},
            {"surface": "canvas chip + edge labels", "resolved": "Noto Sans Mono", "postscript": "NotoSansMono-Regular", "weight": 400, "stack": "ui-monospace, Menlo, Consolas, monospace"},
            {"surface": "body / .hint sans", "resolved": "Noto Sans", "postscript": "NotoSans-Regular", "weight": 400, "stack": "system-ui, -apple-system, \"Segoe UI\", Roboto, Helvetica, Arial, sans-serif"},
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_faces_are_present_and_mapped() {
        let (defs, loaded) = build_definitions();
        let all_present = loaded.iter().all(|v| v["loaded"] == json!(true));
        assert!(
            all_present,
            "fontprobe measured these faces; a missing file would silently fall back: {loaded:?}"
        );
        assert_eq!(loaded.len(), REGULAR.len() + BOLD.len());

        for family in [CANVAS_FAMILY, NZK_UI_MONO_FAMILY, SANS_FAMILY] {
            let entry = defs
                .families
                .get(&FontFamily::Name(family.into()))
                .unwrap_or_else(|| panic!("family {family} not installed"));
            assert!(!entry.is_empty(), "family {family} has no faces");
        }

        // The canvas draws through `FontId::monospace`: it must lead with the
        // canvas face, not Hack.
        let mono = defs.families.get(&FontFamily::Monospace).unwrap();
        assert_eq!(mono.first().map(String::as_str), Some("ncv-noto-sans-mono-regular"));
        let prop = defs.families.get(&FontFamily::Proportional).unwrap();
        assert_eq!(prop.first().map(String::as_str), Some("ncv-noto-sans-regular"));
    }

    #[test]
    fn report_names_the_measured_postscripts() {
        let r = report();
        let faces = r["faces"].as_array().unwrap();
        assert!(faces.iter().any(|f| f["postscript"] == json!("NotoSansMono-Regular")));
        assert!(faces.iter().any(|f| f["postscript"] == json!("LiberationMono")));
        assert!(faces.iter().any(|f| f["postscript"] == json!("NotoSans-Regular")));
    }

    #[test]
    fn font_id_helpers_use_the_named_families() {
        assert_eq!(canvas_mono(11.0).family, FontFamily::Name(CANVAS_FAMILY.into()));
        assert_eq!(nzk_ui_mono(11.5).family, FontFamily::Name(NZK_UI_MONO_FAMILY.into()));
        assert_eq!(sans(11.5).family, FontFamily::Name(SANS_FAMILY.into()));
    }
}
