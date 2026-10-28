/*! End-to-end wire test: the **app's frozen facts JSON** -> the nzk_ui
/// renderer, for the two round-2 channels: the `#cs-tabs` document strip and the
/// `#searchInput` typed query.
///
/// The unit suites prove each side in isolation (app: `nzk_ui::...` facts tests;
/// nzk_ui: `dom::tabs_tests` / `dom::search_input_tests`). This file pins the
/// seam between them: it feeds the exact JSON shapes `app/src/nzk_ui.rs`
/// publishes into `nzk_ui::Facts`, builds the runtime (Rust-skeleton) shell,
/// and measures the DOM. If a key is renamed on either side, these tests fail.
///
/// The shipped document never carries either channel (`#cs-tabs` is created by
/// `shell.js`; `#searchInput` has no `value` attribute), so "absent facts" must
/// render exactly the shipped document.
*/

use std::path::PathBuf;

use nzk_ui::{Facts, NzkUi};

/// The runtime path a real host uses: round-trip `Facts` through JSON with `ui`
/// spliced in, then build the Rust-skeleton shell against the real stylesheet.
fn build(ui: Option<serde_json::Value>) -> NzkUi {
    let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
        .expect("Facts must serialize");
    if let Some(ui) = ui {
        value["ui"] = ui;
    }
    let facts: Facts = serde_json::from_value(value).expect("Facts must round-trip");
    let dir = NzkUi::frontend_dir();
    let css_path: PathBuf = dir.join("static/style.css");
    let css = std::fs::read_to_string(&css_path).expect("read style.css");
    NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
        .expect("build skeleton shell")
}

fn count_matching(shell: &NzkUi, selector: &str) -> usize {
    shell
        .document()
        .query_selector_all(selector)
        .map(|v| v.len())
        .unwrap_or(0)
}

fn text_of(shell: &NzkUi, selector: &str) -> String {
    shell
        .text_of(selector)
        .unwrap_or_else(|| panic!("{selector} must exist"))
}

/// The exact `ui.tabs` object `app/src/nzk_ui.rs::ui_state_json` publishes for
/// two open documents: A active and the split, B inactive.
fn tabs_ui(pane_open: bool) -> serde_json::Value {
    serde_json::json!({
        "pane_open": pane_open,
        "tabs": {
            "tabs": [
                {"key": "node:A", "label": "A", "active": true, "split": true, "title": "A"},
                {"key": "file:/tmp/B.cs", "label": "B.cs", "active": false, "split": false,
                 "title": "/tmp/B.cs"}
            ],
            "split_on": true
        }
    })
}

/// ABSENT is the shipped state: no `#cs-tabs` node (it is a runtime-created
/// region) and no `value` attribute on `#searchInput`. This is the contract that
/// keeps the default render — and the pinned `@frame` token — byte-identical.
#[test]
fn absent_round_two_facts_render_the_shipped_document() {
    let shell = build(None);
    assert_eq!(
        count_matching(&shell, "#cs-tabs"),
        0,
        "the shipped document has no #cs-tabs; absent facts must not create it"
    );
    assert_eq!(count_matching(&shell, ".cs-tab"), 0);
    assert_eq!(
        shell
            .attr_of("#searchInput", "value")
            .expect("#searchInput must exist"),
        None,
        "#searchInput ships without a value attribute"
    );
    assert_eq!(
        shell
            .attr_of("#searchInput", "placeholder")
            .expect("#searchInput must exist"),
        Some("Search symbols…  ( / )".to_string()),
        "the default paint keeps index.html's placeholder"
    );
}

/// The app's `tabs` payload renders the bar the reference creates: first child
/// of `#sourcePane`, tablist role/label, one `.cs-tab` per document with the
/// active/split marks, and the two strip actions with their reference texts.
#[test]
fn the_tabs_wire_payload_renders_the_strip() {
    let shell = build(Some(tabs_ui(true)));
    let bar = shell
        .box_of("#cs-tabs")
        .expect("the open pane + tabs must create #cs-tabs");
    assert!(bar.w > 0.0 && bar.h > 0.0, "an unhidden bar has a real box");

    assert_eq!(count_matching(&shell, "#cs-tabs .cs-tab"), 2);
    assert_eq!(
        text_of(&shell, "#csSplitBtn"),
        "▤ Unsplit",
        "split_on renders the Unsplit label (shell.js:1233)"
    );
    assert_eq!(text_of(&shell, "#csTabCloseOthers"), "Close others");
    // Two tabs means the split action is enabled; the reference's disabled rule
    // is `tabs.length < 2 && !isSplitOn()`.
    assert_eq!(
        shell
            .attr_of("#csSplitBtn", "disabled")
            .expect("#csSplitBtn"),
        None
    );
    // The bar sits inside the (open) source pane.
    let pane = shell.box_of("#sourcePane").expect("#sourcePane");
    assert!(
        bar.y >= pane.y && bar.y <= pane.y + pane.h + 1.0,
        "the strip is in the source pane's vertical band (bar {}..{}, pane {}..{})",
        bar.y,
        bar.y + bar.h,
        pane.y,
        pane.y + pane.h
    );
}

/// The app's `search_query` payload paints the typed query onto the box.
#[test]
fn the_search_query_wire_payload_reaches_the_input() {
    let shell = build(Some(serde_json::json!({"search_query": "Needle"})));
    assert_eq!(
        shell
            .attr_of("#searchInput", "value")
            .expect("#searchInput must exist"),
        Some("Needle".to_string())
    );
}

/// Both channels survive a `Facts` JSON round-trip unchanged — the seam that
/// would otherwise let a typo'd key deserialize to `None` and look wired while
/// being dead.
#[test]
fn the_round_two_keys_survive_a_facts_round_trip() {
    let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture")).expect("serialize");
    value["ui"] = serde_json::json!({
        "search_query": "abc",
        "tabs": {
            "tabs": [{"key": "node:A", "label": "A", "active": true, "split": false, "title": "A"}],
            "split_on": false
        }
    });
    let facts: Facts = serde_json::from_value(value.clone()).expect("deserialize");
    let back = serde_json::to_value(&facts).expect("re-serialize");
    assert_eq!(
        back["ui"]["search_query"],
        serde_json::json!("abc"),
        "search_query survives the wire"
    );
    assert_eq!(
        back["ui"]["tabs"]["tabs"][0]["key"],
        serde_json::json!("node:A"),
        "tabs[0].key survives the wire"
    );
    assert_eq!(
        back["ui"]["tabs"]["split_on"],
        serde_json::json!(false),
        "split_on survives the wire"
    );
}

/// Round-7: the runtime-created bar's controls carry their routing payload in
/// the DOM itself — `data-key` on each `.cs-tab` row and `data-close` on its ✕ —
/// and the shell's hit test resolves a press on a **child** (the label span) up
/// to that payload via the ancestor walk (the reference's `closest()`,
/// `shell.js:1250-1266`). This is exactly the fact the app's `@hit` rows carry,
/// so a wiring that drops the payload or the walk fails here.
#[test]
fn tab_bar_controls_carry_the_routing_payload() {
    let shell = build(Some(tabs_ui(true)));
    let row = shell
        .box_of("#cs-tabs .cs-tab")
        .expect("the first tab row has a box");
    let close = shell
        .box_of("#cs-tabs .cs-tab .cs-tab-x")
        .expect("the close button has a box");
    let label = shell
        .box_of("#cs-tabs .cs-tab .cs-tab-label")
        .expect("the label span has a box");
    let centre = |b: &nzk_ui::Box2| (b.x + b.w / 2.0, b.y + b.h / 2.0);

    let (x, y) = centre(&row);
    let h = shell.hit(x, y).expect("the row is hit-testable");
    assert_eq!(
        h.data_key.as_deref(),
        Some("node:A"),
        "the row carries its data-key"
    );

    let (x, y) = centre(&close);
    let h = shell.hit(x, y).expect("the close button is hit-testable");
    assert_eq!(
        h.data_close.as_deref(),
        Some("node:A"),
        "the ✕ carries data-close"
    );

    let (x, y) = centre(&label);
    let h = shell.hit(x, y).expect("the label span is hit-testable");
    assert_eq!(h.tag, "span", "the label child is the raw hit");
    // The span has no `data-key` of its own: the value comes from the ANCESTOR
    // walk, which is what lets a press on a row's label still name the row.
    assert_eq!(
        h.data_key.as_deref(),
        Some("node:A"),
        "the ancestor walk must resolve the row's data-key for a label press"
    );
    assert_eq!(
        h.data_close, None,
        "the span must not inherit the row's close payload"
    );

    // The split button is hit directly (its text is a text node, not an
    // element), and the bar's own background has neither payload.
    let split = shell.box_of("#csSplitBtn").expect("#csSplitBtn box");
    let (x, y) = centre(&split);
    let h = shell.hit(x, y).expect("#csSplitBtn is hit-testable");
    assert_eq!(h.id.as_deref(), Some("csSplitBtn"));
    let bar = shell.box_of("#cs-tabs").expect("#cs-tabs box");
    let (x, y) = centre(&bar);
    let h = shell.hit(x, y).expect("the bar background is hit-testable");
    assert_eq!(h.id.as_deref(), Some("cs-tabs"));
    assert_eq!(h.data_key, None);
    assert_eq!(h.data_close, None);
}

/// The exact `ui.suggestions` object `app/src/nzk_ui.rs::ui_state_json` publishes
/// for the graph-mode popup — the literal is the contract (not a struct
/// round-trip), so a renamed key on either side fails this seam.
fn suggestions_ui() -> serde_json::Value {
    serde_json::json!({
        "search_query": "hit",
        "suggestions": {
            "rows": [
                {"id": "n:1", "kind": "method", "label": "hit", "full": "App"},
                {"id": "n:2", "kind": "field", "label": "hitsAll", "full": "App"}
            ],
            "active": 0,
            "empty": null
        }
    })
}

/// Round-7 ABSENT contract: the shipped document's `#suggestions` box stays
/// hidden with no children and `aria-expanded` stays `false` when the app omits
/// the key (the default nzk_ui, and therefore the pinned `@frame`).
///
/// ADVERSARIAL: this fails if the paint is called unconditionally (the box would
/// lose `hidden`) or if the app ever publishes the key while the graph query is
/// empty (the seam test feeds `build(None)`; the app-side proof is
/// `nzk_ui::suggestions_tests::suggestions_facts_are_absent_by_default`).
#[test]
fn absent_suggestions_ui_renders_the_shipped_document() {
    let shell = build(None);
    let classes = shell
        .classes_of("#suggestions")
        .expect("the shipped document contains #suggestions");
    assert!(
        classes.iter().any(|c| c == "hidden"),
        "absent facts must keep the shipped hidden class: {classes:?}"
    );
    assert_eq!(count_matching(&shell, ".sug"), 0);
    assert_eq!(count_matching(&shell, ".sug-empty"), 0);
    assert_eq!(
        shell
            .attr_of("#searchInput", "aria-expanded")
            .expect("#searchInput"),
        Some("false".to_string()),
        "the shipped combobox is collapsed"
    );
    // The shipped hidden box has no box at all (`.hidden { display:none }`).
    let b = shell.box_of("#suggestions");
    assert!(
        b.map(|b| b.w == 0.0 && b.h == 0.0).unwrap_or(true),
        "a hidden popup must not measure"
    );
}

/// Round-7 OPEN contract: feeding the app's literal `ui.suggestions` JSON into
/// the real stylesheet must LAY OUT the popup (non-zero measured box) and paint
/// the reference's row markup. This is the same discipline the `#cs-tabs` seam
/// uses: DOM shape plus real geometry, so "painted but never laid out" fails.
#[test]
fn suggestions_ui_from_the_app_facts_json_paints_into_the_real_stylesheet() {
    let shell = build(Some(suggestions_ui()));

    // The popup has real geometry from `style.css`: it is open, not a hidden box.
    let b = shell
        .box_of("#suggestions")
        .expect("#suggestions must exist and be visible");
    assert!(
        b.w > 0.0 && b.h > 0.0,
        "an open #suggestions must have a real box, got {}x{}",
        b.w,
        b.h
    );
    assert_eq!(
        shell
            .attr_of("#searchInput", "aria-expanded")
            .expect("#searchInput"),
        Some("true".to_string()),
        "showSuggestions flips aria-expanded (search.js:13)"
    );
    assert_eq!(count_matching(&shell, "#suggestions .sug"), 2);
    let active = shell
        .box_of("#suggestions .sug.active")
        .expect("the active row is painted and laid out");
    assert!(active.w > 0.0 && active.h > 0.0);

    // Per-row routing payload + the four ordered children, measured on the first
    // row — exactly what a future 7B click/Enter commit will read.
    assert_eq!(
        shell.attr_of("#suggestions .sug:nth-child(1)", "data-id"),
        Ok(Some("n:1".to_string()))
    );
    assert_eq!(
        shell.attr_of("#suggestions .sug:nth-child(1)", "data-i"),
        Ok(Some("0".to_string()))
    );
    assert_eq!(
        shell.attr_of("#suggestions .sug:nth-child(1)", "title"),
        Ok(Some("Open this symbol in the graph".to_string()))
    );
    assert_eq!(count_matching(&shell, ".sug > .dot:nth-child(1)"), 2);
    assert_eq!(count_matching(&shell, ".sug > .sug-kind:nth-child(2)"), 2);
    assert_eq!(count_matching(&shell, ".sug > .sug-label:nth-child(3)"), 2);
    assert_eq!(count_matching(&shell, ".sug > .sug-full:nth-child(4)"), 2);
    // The typed graph query reaches the shared box in the same pass.
    assert_eq!(
        shell.attr_of("#searchInput", "value"),
        Ok(Some("hit".to_string()))
    );
}

/// Round-7 split-pane scaffold: feeding the app's literal `ui.tabs` JSON with
/// `split_on: true` must build the reference wrap and LAY IT OUT from the
/// shipped stylesheet — the wrap and the empty split body measure, the divider
/// is displayed, and `#sourceBody` is the CSS-default half of the wrap.
///
/// ADVERSARIAL: fails if `apply_split` is not reached from `apply_ui_state`, if
/// the wrap is created without `cs-split-on` (every split rule is keyed off that
/// class, so the body would not shrink and the divider/split body would stay
/// `display:none`), or if `#sourceBody` was copied instead of reparented.
#[test]
fn split_on_from_the_app_tabs_json_builds_and_lays_out_the_split_scaffold() {
    let shell = build(Some(tabs_ui(true)));

    assert_eq!(count_matching(&shell, "#csSplitWrap"), 1);
    assert_eq!(count_matching(&shell, "#sourceBody"), 1, "moved, not cloned");
    // Child order is the reference's: source body, divider, split body.
    assert_eq!(
        count_matching(&shell, "#csSplitWrap > #sourceBody:nth-child(1)"),
        1
    );
    assert_eq!(
        count_matching(&shell, "#csSplitWrap > #csSplitDivider:nth-child(2)"),
        1
    );
    assert_eq!(
        count_matching(&shell, "#csSplitWrap > #csSplitBody:nth-child(3)"),
        1
    );
    assert_eq!(count_matching(&shell, ".cs-split-label"), 1);
    // The split row is A in `tabs_ui`, so the head shows A's label.
    assert_eq!(shell.text_of(".cs-split-label"), Some("A".to_string()));
    assert_eq!(
        shell
            .attr_of("#csSplitDivider", "title")
            .expect("#csSplitDivider"),
        Some("Drag to resize the split".to_string())
    );

    let wrap = shell.box_of("#csSplitWrap").expect("#csSplitWrap must be laid out");
    assert!(wrap.w > 0.0 && wrap.h > 0.0, "wrap box {}x{}", wrap.w, wrap.h);
    let split_body = shell
        .box_of("#csSplitBody")
        .expect("#csSplitBody must be laid out when split is on");
    assert!(
        split_body.w > 0.0 && split_body.h > 0.0,
        "the split body must flex beside the source body, got {}x{}",
        split_body.w,
        split_body.h
    );
    let divider = shell
        .box_of("#csSplitDivider")
        .expect("the divider is displayed when split is on");
    assert!(divider.w > 0.0, "divider width {}", divider.w);
    // Round 8: `#csSplitHost` is the second document body's box. It must be laid
    // out here, because the host paints into exactly this rect; a MISSING row
    // would mean the second body silently never paints.
    let host = shell
        .box_of("#csSplitHost")
        .expect("#csSplitHost must be laid out when split is on");
    assert!(
        host.w > 0.0 && host.h > 0.0,
        "second body box {}x{}",
        host.w,
        host.h
    );
    let source = shell.box_of("#sourceBody").expect("#sourceBody box");
    let half = wrap.w * 0.5;
    assert!(
        (source.w - half).abs() <= wrap.w * 0.06,
        "default --cs-split-w is 50%: source {} vs wrap {}",
        source.w,
        wrap.w
    );
}

/// ABSENT contract for the split channel: the shipped document (no `ui.tabs`)
/// carries no `#csSplitWrap`/`#csSplitDivider`/`#csSplitBody`, and `#sourceBody`
/// stays a direct child of `#sourcePane` — the geometry the pinned
/// `@frame 1600 1000 8de7a5141270f758` was measured on.
///
/// ADVERSARIAL: fails if the split scaffold is created unconditionally (a
/// display:none wrap still moves the parent's flow) or if the body is
/// reparented before any split fact arrives.
#[test]
fn absent_split_facts_leave_the_shipped_document_without_a_wrap() {
    let shell = build(None);
    assert_eq!(count_matching(&shell, "#csSplitWrap"), 0);
    assert_eq!(count_matching(&shell, "#csSplitDivider"), 0);
    assert_eq!(count_matching(&shell, "#csSplitBody"), 0);
    // Round 8: the newly probed second-body box answers None under the shipped
    // facts, exactly like before the selector was added, so its probe row cannot
    // move the painted frame (or the pinned @frame token).
    assert!(
        shell.box_of("#csSplitHost").is_none(),
        "shipped facts must not lay out a #csSplitHost"
    );
    assert_eq!(
        count_matching(&shell, "#sourcePane > #sourceBody"),
        1,
        "the shipped #sourceBody must stay a direct child of #sourcePane"
    );
    assert!(shell.box_of("#csSplitWrap").is_none());
}

/// The build path the Blocks canvas actually uses: the app's `ui.blocks` fact
/// (a member of `ui`, exactly as `nzk_ui.rs::ui_state_json` emits it).
fn build_with_blocks(cards: serde_json::Value) -> NzkUi {
    let mut value = serde_json::to_value(Facts::for_corpus("/tmp/fixture"))
        .expect("Facts must serialize");
    value["ui"] = serde_json::json!({
        "blocks": {
            "title": "Blocks · Widget",
            "meta": "3 blocks · 2 edges",
            "empty": "",
            "cards": cards,
            "edges": [],
        }
    });
    let facts: Facts = serde_json::from_value(value).expect("Facts must round-trip");
    let dir = NzkUi::frontend_dir();
    let css_path: PathBuf = dir.join("static/style.css");
    let css = std::fs::read_to_string(&css_path).expect("read style.css");
    NzkUi::from_skeleton_with_facts(&css_path, &css, (1600, 1000), &facts)
        .expect("build skeleton shell")
}

/// The `RECTS=` selectors `app/src/blocks_wire.rs::PORT_RECT_SELECTORS` sends.
/// They are duplicated here deliberately: this is the seam test, so a rename on
/// either side must fail rather than silently measuring nothing.
const PORT_RECT_SELECTORS: [(&str, &str); 5] = [
    ("origin", "#blocksCards"),
    ("card", "#blocksCards>.block-card"),
    ("ports", "#blocksCards>.block-card>.bc-ports"),
    ("out", "#blocksCards>.block-card>.bc-ports>.bc-out>.port.port-out>.port-dot"),
    ("in", "#blocksCards>.block-card>.bc-ports>.bc-in>.port.port-in>.port-dot"),
];

/// The measured-geometry channel against the REAL stylesheet: every selector the
/// host sends matches painted elements, carries the card's `data-id`, and each
/// `.port-dot` lies inside its card's `.bc-ports` row. This is what makes the
/// Blocks edge endpoints measured rather than stylesheet-assumed.
#[test]
fn blocks_rect_selectors_measure_the_painted_cards_and_ports() {
    let cards = serde_json::json!([
        {
            "id": "M:Demo.Widget.Run(System.String,System.Int32)",
            "index": 0, "kind": "method", "name": "Run", "full": "Demo.Widget.Run",
            "file": "widget.cs", "line": 5, "params": ["System.String", "System.Int32"],
            "out_type": "System.Void", "degree": 2, "x": 0, "y": 0,
            "selected": false, "kind_color": "#42c3d0"
        },
        {
            "id": "T:Demo.Widget",
            "index": 1, "kind": "class", "name": "Widget", "full": "Demo.Widget",
            "file": "widget.cs", "line": 1, "params": [],
            "out_type": "class", "degree": 1, "x": 262, "y": 0,
            "selected": true, "kind_color": "#86aaf9"
        }
    ]);
    let shell = build_with_blocks(cards);

    let origin = shell.rects_of(PORT_RECT_SELECTORS[0].1);
    assert_eq!(origin.len(), 1, "#blocksCards must be painted when blocks facts arrive");
    assert!(origin[0].rect.w > 0.0 && origin[0].rect.h > 0.0);

    let cards = shell.rects_of(PORT_RECT_SELECTORS[1].1);
    assert_eq!(cards.len(), 2, "one .block-card per fact");
    let ids: Vec<&str> = cards.iter().filter_map(|r| r.data_id.as_deref()).collect();
    assert!(ids.contains(&"M:Demo.Widget.Run(System.String,System.Int32)"));
    assert!(ids.contains(&"T:Demo.Widget"));

    let ports = shell.rects_of(PORT_RECT_SELECTORS[2].1);
    assert_eq!(ports.len(), 2, "one .bc-ports row per card");
    let outs = shell.rects_of(PORT_RECT_SELECTORS[3].1);
    assert_eq!(outs.len(), 2, "one .port-out dot per card");
    let ins = shell.rects_of(PORT_RECT_SELECTORS[4].1);
    assert_eq!(ins.len(), 2, "two parameter ports on the method, none on the class");

    // Every measured dot names its card and sits inside that card's box — the
    // property the host relies on when it subtracts `#blocksCards`'s origin.
    let card_for = |id: &str| {
        cards
            .iter()
            .find(|c| c.data_id.as_deref() == Some(id))
            .map(|c| c.rect)
            .expect("card row")
    };
    for (rows, id) in [
        (&outs, "M:Demo.Widget.Run(System.String,System.Int32)"),
        (&outs, "T:Demo.Widget"),
    ] {
        let card = card_for(id);
        let dot = rows
            .iter()
            .find(|r| r.data_id.as_deref() == Some(id))
            .map(|r| r.rect)
            .expect("out dot row");
        assert!(
            dot.x >= card.x && dot.x + dot.w <= card.x + card.w + 0.5,
            "out dot {dot:?} must lie within card {id} {card:?}"
        );
    }
    for row in &ins {
        let id = row.data_id.as_deref().expect("in dot data-id");
        let card = card_for(id);
        let ports = ports
            .iter()
            .find(|r| r.data_id.as_deref() == Some(id))
            .map(|r| r.rect)
            .expect("ports row");
        assert!(
            row.rect.y >= ports.y - 1.0 && row.rect.y + row.rect.h <= ports.y + ports.h + 1.0,
            "in dot {:?} must lie in the ports row {ports:?}",
            row.rect
        );
        assert!(row.rect.x >= card.x - 0.5 && row.rect.x <= card.x + card.w);
    }
}
