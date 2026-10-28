//! The two display-only id families from the task-23 sweep, wired to the real
//! reference behaviour instead of being JS-orphaned.
//!
//! Both families ship as static markup in the shared app nzk_ui; until now no
//! code ever read them, so they were inert. This module supplies the live
//! consumer for every id in both families, modelled as pure state so the exact
//! rules are testable without a browser.
//!
//! ## PART A — the `blocksPeek*` hover tooltip (`scratch.js:418-503`, handles `:564-569`)
//!
//! `#blocksPeek` is `role="tooltip"` and ships `class="blocks-peek hidden"`.
//! `scratch.js:424-436` (`showPeek`) fills it when the pointer enters a block
//! card, and `:499-503` (`hidePeek`) hides it again. Reproduced here:
//!
//! * `#blocksPeekKind` <- `d.kind` (tinted by `kindColor`)
//! * `#blocksPeekName` <- `d.name || d.id`, with `title = d.full || d.id`
//! * `#blocksPeekFile` <- `file` plus `:line` when the line is non-zero
//! * `#blocksPeekSrc`  <- the literal `Loading source…`, then the first
//!   `PEEK_LINES` (= 12, `scratch.js:32`) lines from `GET /api/source`, or
//!   `(no source lines)` for an empty result, or `source unavailable: <msg>`
//!   when the fetch rejects
//! * `#blocksPeekRefs` <- a degree line `in N · out M` from the card itself,
//!   which the later `GET /api/node` may refine
//!
//! `peekSeq` (`:424`, `:441`, `:449`) is the staleness guard: a hover that
//! moves on before a fetch lands must discard the in-flight result rather than
//! paint the wrong card. [`Peek::seq`] carries it.
//!
//! ## PART B — the `reorg*Row` visibility toggles (`reorg.js:286-290`)
//!
//! `syncRows` is the whole rule, driven by `#reorgMode`:
//!
//! ```text
//! split = els.mode.value === 'split'
//! granRow hidden unless split
//! groupRow hidden unless split
//! sizeRow hidden unless split AND group.value === 'size'
//! ```
//!
//! `reorg.js:334-336` re-runs it on the `change` of `#reorgMode`, `#reorgGroup`
//! and `#reorgScope`, and `:278` runs it when the modal opens.

use crate::graphviz::{kind_color, kind_color_nzk_darkplus};

/// `scratch.js:32` — `var PEEK_LINES = 12`.
pub const PEEK_LINES: usize = 12;

/// The literal the markup ships in `#blocksPeekSrc` (`index.html:191`) and the
/// value `showPeek` resets it to on every hover.
pub const PEEK_LOADING: &str = "Loading source…";

/// `scratch.js:442` — an answered-but-empty `GET /api/source`.
pub const PEEK_NO_LINES: &str = "(no source lines)";

// ------------------------------------------------------------------ PART A

/// What the pointer is currently over — the fields `showPeek` reads off
/// `card.__decl` (`scratch.js:425-432`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PeekDecl {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub full: String,
    pub file: String,
    pub line: u32,
    pub in_deg: u32,
    pub out_deg: u32,
}

/// The five `blocksPeek*` spans, as the DOM would show them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PeekView {
    /// `#blocksPeek` — whether the `hidden` class is *absent*.
    pub visible: bool,
    pub kind: String,
    pub name: String,
    /// `peekNameEl.title` (`scratch.js:431`).
    pub name_title: String,
    pub file: String,
    pub src: String,
    pub refs: String,
}

/// The hover-peek state machine — `peekCard`/`peekSeq` from `scratch.js`.
#[derive(Debug, Clone, Default)]
pub struct Peek {
    card: Option<String>,
    seq: u64,
    view: PeekView,
}

impl Peek {
    pub fn new() -> Self {
        // `#blocksPeekSrc` ships with the literal in the markup, and the panel
        // starts hidden (`class="blocks-peek hidden"`).
        Self { card: None, seq: 0, view: PeekView { src: PEEK_LOADING.to_string(), ..Default::default() } }
    }

    /// The current span contents.
    pub fn view(&self) -> &PeekView {
        &self.view
    }

    /// `peekVisible()` (`scratch.js:615`).
    pub fn visible(&self) -> bool {
        self.view.visible
    }

    /// The id of the card the peek is currently pinned to, if any.
    pub fn card(&self) -> Option<&str> {
        self.card.as_deref()
    }

    /// The staleness guard `peekSeq`. A fetch carries the seq it was issued
    /// under and is dropped unless it still matches.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// `showPeek(card)` — `scratch.js:424-436`, the synchronous half.
    ///
    /// Returns the seq the caller must present when the async halves land.
    pub fn show(&mut self, d: &PeekDecl) -> u64 {
        self.card = Some(d.id.clone());
        self.seq += 1;
        let v = &mut self.view;
        v.visible = true;
        v.kind = d.kind.clone();
        v.name = if !d.name.is_empty() { d.name.clone() } else { d.id.clone() };
        v.name_title = if !d.full.is_empty() { d.full.clone() } else { d.id.clone() };
        // `(d.file || '') + (d.line ? ':' + d.line : '')`
        v.file = if d.line != 0 { format!("{}:{}", d.file, d.line) } else { d.file.clone() };
        v.src = PEEK_LOADING.to_string();
        v.refs = degree_line(d.in_deg, d.out_deg);
        self.seq
    }

    /// `hidePeek()` — `scratch.js:499-503`. Bumps `peekSeq` so any in-flight
    /// fetch for the previous card is discarded.
    pub fn hide(&mut self) {
        self.seq += 1;
        self.card = None;
        self.view.visible = false;
    }

    /// The `mouseout` half of `wireCards` (`scratch.js:522-531`): leaving a card
    /// hides the peek, but only when the pointer actually left that card.
    pub fn on_mouse_out(&mut self, from: &str, related_inside: bool) {
        if self.card.as_deref() != Some(from) {
            return;
        }
        if related_inside {
            return;
        }
        self.hide();
    }

    /// `GET /api/source` resolving (`scratch.js:439-446`), truncated to
    /// `PEEK_LINES`.
    pub fn apply_source(&mut self, seq: u64, lines: &[String]) {
        if seq != self.seq {
            return;
        }
        if lines.is_empty() {
            self.view.src = PEEK_NO_LINES.to_string();
            return;
        }
        self.view.src = lines.iter().take(PEEK_LINES).cloned().collect::<Vec<_>>().join("\n");
    }

    /// `GET /api/source` rejecting (`scratch.js:447-450`).
    pub fn apply_source_error(&mut self, seq: u64, message: &str) {
        if seq != self.seq {
            return;
        }
        self.view.src = format!("source unavailable: {message}");
    }

    /// `GET /api/node` resolving — `refsHtml` replaces the degree line with the
    /// relation breakdown (`scratch.js:451-454`).
    pub fn apply_node_refs(&mut self, seq: u64, rows: &[(String, u32)]) {
        if seq != self.seq {
            return;
        }
        if rows.is_empty() {
            return;
        }
        let mut out = String::new();
        for (rel, n) in rows {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{rel} {n}"));
        }
        self.view.refs = out;
    }

    /// The five ids this view drives, paired with their current contents — the
    /// consumer seam a DOM writer reads.
    pub fn spans(&self) -> [(&'static str, String); 5] {
        [
            ("#blocksPeekKind", self.view.kind.clone()),
            ("#blocksPeekName", self.view.name.clone()),
            ("#blocksPeekFile", self.view.file.clone()),
            ("#blocksPeekSrc", self.view.src.clone()),
            ("#blocksPeekRefs", self.view.refs.clone()),
        ]
    }
}

/// `scratch.js:433-435` — the degree line shown before `/api/node` answers.
pub fn degree_line(in_deg: u32, out_deg: u32) -> String {
    format!("in {in_deg} · out {out_deg}")
}

/// The `#blocksPeek` root id, so the panel consumer is explicit.
pub const PEEK_ROOT: &str = "#blocksPeek";

// ------------------------------------------------------------------ PART B

/// `#reorgMode` values that matter to `syncRows` (`index.html:456-459`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReorgMode {
    Single,
    Split,
}

impl ReorgMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ReorgMode::Single => "single",
            ReorgMode::Split => "split",
        }
    }
}

/// `#reorgGroup` values (`index.html:470-474`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReorgGroup {
    Size,
    Namespace,
    Owner,
    Single,
}

impl ReorgGroup {
    pub fn as_str(self) -> &'static str {
        match self {
            ReorgGroup::Size => "size",
            ReorgGroup::Namespace => "namespace",
            ReorgGroup::Owner => "owner",
            ReorgGroup::Single => "single",
        }
    }
}

/// The three rows `syncRows` toggles, as `hidden`-class membership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReorgRows {
    pub gran_hidden: bool,
    pub group_hidden: bool,
    pub size_hidden: bool,
}

/// `syncRows` — `reorg.js:286-290`, verbatim:
///
/// ```text
/// var split = els.mode.value === 'split';
/// granRow.classList.toggle('hidden',  !split);
/// groupRow.classList.toggle('hidden', !split);
/// sizeRow.classList.toggle('hidden', !(split && els.group.value === 'size'));
/// ```
pub fn reorg_rows(mode: ReorgMode, group: ReorgGroup) -> ReorgRows {
    let split = mode == ReorgMode::Split;
    ReorgRows {
        gran_hidden: !split,
        group_hidden: !split,
        size_hidden: !(split && group == ReorgGroup::Size),
    }
}

/// The three ids `syncRows` owns, paired with their `hidden` state — the
/// consumer seam, so each id has a live reader.
pub fn reorg_row_states(mode: ReorgMode, group: ReorgGroup) -> [(&'static str, bool); 3] {
    let r = reorg_rows(mode, group);
    [
        ("#reorgGranRow", r.gran_hidden),
        ("#reorgGroupRow", r.group_hidden),
        ("#reorgSizeRow", r.size_hidden),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl() -> PeekDecl {
        PeekDecl {
            id: "T:Foo.Bar".into(),
            kind: "method".into(),
            name: "Bar".into(),
            full: "Foo.Bar".into(),
            file: "src/Foo.cs".into(),
            line: 42,
            in_deg: 3,
            out_deg: 5,
        }
    }

    // ---- PART A ---------------------------------------------------------

    #[test]
    fn peek_starts_hidden_with_the_loading_literal_the_markup_ships() {
        let p = Peek::new();
        assert!(!p.visible(), "`class=\"blocks-peek hidden\"` ships closed");
        assert_eq!(p.view().src, "Loading source…", "index.html:191 literal");
        assert_eq!(p.view().src, PEEK_LOADING);
    }

    #[test]
    fn show_fills_every_span_and_reveals_the_panel() {
        let mut p = Peek::new();
        let seq = p.show(&decl());
        assert!(p.visible(), "showPeek does classList.remove('hidden')");
        assert_eq!(p.view().kind, "method");
        assert_eq!(p.view().name, "Bar");
        assert_eq!(p.view().name_title, "Foo.Bar");
        assert_eq!(p.view().file, "src/Foo.cs:42");
        assert_eq!(p.view().src, PEEK_LOADING, "reset to the literal on every hover");
        assert_eq!(p.view().refs, "in 3 · out 5");
        assert_eq!(p.seq(), seq);
    }

    #[test]
    fn peek_file_omits_a_zero_line_suffix() {
        let mut p = Peek::new();
        let mut d = decl();
        d.line = 0;
        p.show(&d);
        assert_eq!(p.view().file, "src/Foo.cs", "`d.line ? ':' + d.line : ''`");
    }

    #[test]
    fn peek_name_falls_back_to_the_id_like_the_reference() {
        let mut p = Peek::new();
        let mut d = decl();
        d.name = String::new();
        d.full = String::new();
        p.show(&d);
        assert_eq!(p.view().name, "T:Foo.Bar");
        assert_eq!(p.view().name_title, "T:Foo.Bar");
    }

    #[test]
    fn source_replaces_the_loading_literal_with_at_most_twelve_lines() {
        let mut p = Peek::new();
        let seq = p.show(&decl());
        assert_eq!(p.view().src, PEEK_LOADING);
        let lines: Vec<String> = (1..=30).map(|i| format!("line {i}")).collect();
        p.apply_source(seq, &lines);
        let shown: Vec<&str> = p.view().src.split('\n').collect();
        assert_eq!(shown.len(), 12, "PEEK_LINES truncation (scratch.js:443)");
        assert_eq!(shown[0], "line 1");
        assert_eq!(shown[11], "line 12");
        assert!(!p.view().src.contains(PEEK_LOADING), "the literal is replaced, not appended");
    }

    #[test]
    fn empty_source_and_a_failed_fetch_have_distinct_text() {
        let mut p = Peek::new();
        let seq = p.show(&decl());
        p.apply_source(seq, &[]);
        assert_eq!(p.view().src, "(no source lines)");

        let mut q = Peek::new();
        let seq = q.show(&decl());
        q.apply_source_error(seq, "boom");
        assert_eq!(q.view().src, "source unavailable: boom");
    }

    #[test]
    fn a_stale_fetch_is_discarded_rather_than_painting_the_wrong_card() {
        let mut p = Peek::new();
        let stale = p.show(&decl());
        let mut other = decl();
        other.id = "T:Other".into();
        other.name = "Other".into();
        other.file = "src/Other.cs".into();
        let fresh = p.show(&other);
        assert_ne!(stale, fresh, "each hover bumps peekSeq");

        // The first card's source lands late: it must NOT overwrite the peek.
        p.apply_source(stale, &["WRONG".to_string()]);
        assert_eq!(p.view().src, PEEK_LOADING, "stale seq was dropped");
        p.apply_source(fresh, &["RIGHT".to_string()]);
        assert_eq!(p.view().src, "RIGHT");
    }

    #[test]
    fn mouse_out_hides_only_when_the_pointer_really_left_the_card() {
        let mut p = Peek::new();
        p.show(&decl());
        p.on_mouse_out("T:Foo.Bar", true);
        assert!(p.visible(), "moving onto a child stays on the same card");

        p.show(&decl());
        p.on_mouse_out("T:SomethingElse", false);
        assert!(p.visible(), "not the peeked card — ignore");

        p.on_mouse_out("T:Foo.Bar", false);
        assert!(!p.visible(), "hidePeek adds the hidden class back");
    }

    #[test]
    fn hiding_invalidates_an_in_flight_fetch() {
        let mut p = Peek::new();
        let seq = p.show(&decl());
        p.hide();
        assert!(!p.visible());
        p.apply_source(seq, &["WRONG".to_string()]);
        assert!(p.view().src.contains(PEEK_LOADING) || p.view().src == PEEK_LOADING);
        assert_ne!(p.view().src, "WRONG", "hidePeek bumps peekSeq");
    }

    #[test]
    fn node_relations_refine_the_degree_line() {
        let mut p = Peek::new();
        let seq = p.show(&decl());
        assert_eq!(p.view().refs, "in 3 · out 5");
        p.apply_node_refs(seq, &[("calls".into(), 2), ("uses".into(), 1)]);
        assert_eq!(p.view().refs, "calls 2\nuses 1");
    }

    #[test]
    fn every_peek_id_has_a_live_span_consumer() {
        let mut p = Peek::new();
        p.show(&decl());
        let spans = p.spans();
        let ids: Vec<&str> = spans.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            vec![
                "#blocksPeekKind",
                "#blocksPeekName",
                "#blocksPeekFile",
                "#blocksPeekSrc",
                "#blocksPeekRefs"
            ]
        );
        assert_eq!(PEEK_ROOT, "#blocksPeek");
        // Non-vacuous: each span carries real content, not an empty default.
        assert_eq!(spans[0].1, "method");
        assert_eq!(spans[1].1, "Bar");
        assert_eq!(spans[2].1, "src/Foo.cs:42");
        assert_eq!(spans[3].1, PEEK_LOADING);
        assert_eq!(spans[4].1, "in 3 · out 5");
    }

    #[test]
    fn the_kind_tint_comes_from_the_shared_kind_colour_table() {
        // `peekKindEl.style.color = kindColor(d.kind)` (scratch.js:429). The
        // peek panel is part of the nzk-dark+ document, so the tint must come
        // from that theme's table and must actually discriminate by kind.
        let method = kind_color(crate::graphviz::UiTheme::NzkDarkPlus, "method");
        let namespace = kind_color(crate::graphviz::UiTheme::NzkDarkPlus, "namespace");
        assert_ne!(method, namespace, "kind tint must vary with the kind");
        assert_eq!(kind_color_nzk_darkplus("method"), method);
        assert_eq!(kind_color_nzk_darkplus("constructor"), method);
        assert_ne!(kind_color_nzk_darkplus("struct"), namespace);
    }

    // ---- PART B ---------------------------------------------------------

    #[test]
    fn single_mode_hides_all_three_rows() {
        for group in [ReorgGroup::Size, ReorgGroup::Namespace, ReorgGroup::Owner, ReorgGroup::Single] {
            let r = reorg_rows(ReorgMode::Single, group);
            assert!(r.gran_hidden, "gran row needs split (group {})", group.as_str());
            assert!(r.group_hidden, "group row needs split (group {})", group.as_str());
            assert!(r.size_hidden, "size row needs split (group {})", group.as_str());
        }
    }

    #[test]
    fn split_shows_gran_and_group_but_the_size_row_follows_the_group() {
        let size = reorg_rows(ReorgMode::Split, ReorgGroup::Size);
        assert!(!size.gran_hidden);
        assert!(!size.group_hidden);
        assert!(!size.size_hidden, "split + group=size is the ONLY visible case");

        for group in [ReorgGroup::Namespace, ReorgGroup::Owner, ReorgGroup::Single] {
            let r = reorg_rows(ReorgMode::Split, group);
            assert!(!r.gran_hidden, "split still shows the gran row");
            assert!(!r.group_hidden, "split still shows the group row");
            assert!(r.size_hidden, "split + group={} hides the size row", group.as_str());
        }
    }

    #[test]
    fn the_size_row_is_the_only_row_that_reads_the_group_select() {
        // Toggling the group select must never move the gran/group rows.
        for group in [ReorgGroup::Size, ReorgGroup::Namespace, ReorgGroup::Owner, ReorgGroup::Single] {
            let base = reorg_rows(ReorgMode::Split, ReorgGroup::Size);
            let other = reorg_rows(ReorgMode::Split, group);
            assert_eq!(base.gran_hidden, other.gran_hidden);
            assert_eq!(base.group_hidden, other.group_hidden);
        }
    }

    #[test]
    fn every_reorg_row_id_has_a_live_state_consumer() {
        let rows = reorg_row_states(ReorgMode::Split, ReorgGroup::Size);
        assert_eq!(
            rows,
            [
                ("#reorgGranRow", false),
                ("#reorgGroupRow", false),
                ("#reorgSizeRow", false)
            ]
        );
        let hidden = reorg_row_states(ReorgMode::Single, ReorgGroup::Size);
        assert_eq!(
            hidden,
            [
                ("#reorgGranRow", true),
                ("#reorgGroupRow", true),
                ("#reorgSizeRow", true)
            ]
        );
        // The discriminating case: split with a non-size group.
        let ns = reorg_row_states(ReorgMode::Split, ReorgGroup::Namespace);
        assert_eq!(
            ns,
            [
                ("#reorgGranRow", false),
                ("#reorgGroupRow", false),
                ("#reorgSizeRow", true)
            ],
            "size row is the ONLY row hidden here"
        );
    }

    #[test]
    fn mode_and_group_round_trip_through_their_dom_values() {
        // `els.mode.value === 'split'` / `els.group.value === 'size'` are string
        // comparisons against the markup, so the mapping must be exact.
        assert_eq!(ReorgMode::Split.as_str(), "split");
        assert_eq!(ReorgMode::Single.as_str(), "single");
        assert_eq!(ReorgGroup::Size.as_str(), "size");
        assert_eq!(ReorgGroup::Namespace.as_str(), "namespace");
        assert_eq!(ReorgGroup::Owner.as_str(), "owner");
        assert_eq!(ReorgGroup::Single.as_str(), "single");
    }
}
