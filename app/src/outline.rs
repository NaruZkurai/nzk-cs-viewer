//! The document outline behind **Ctrl+Shift+O**: the nesting model for a flat
//! list of declaration rows.
//!
//! This is the natively-testable half of the reference outline provider
//! `rust-only-csviewer/frontend/static/symbols.js` (the module whose own header
//! calls itself "document outline, folding and occurrence highlight", and whose
//! line 7 lists `documentSymbols` as the **Ctrl+Shift+O outline**).
//!
//! ## Provenance: the chord is a Monaco command, not a keydown handler
//!
//! Unlike the other chords ported this round, **there is no `keydown` listener
//! for Ctrl+Shift+O anywhere in the reference tree.** A raw grep across
//! `frontend/static/*.js` for an `'O'`/`'o'` key test returns exactly one hit —
//! `nav.js:442`, `ev.key === 'o' || ev.key === 'O'` — and that one is bound to
//! **Alt**+O (guarded at `nav.js:439` by `ev.altKey && !ctrlKey && !metaKey &&
//! !shiftKey`) and calls `definitionAside()`; it is not Ctrl+Shift+O and not the
//! outline. The nearest real Ctrl+Shift chord handler, `arrange.js:416`, handles
//! only `v`/`h`/`d`.
//!
//! Ctrl+Shift+O is instead a **built-in Monaco editor command** that Monaco
//! routes to whichever registered `DocumentSymbolProvider` is active; the
//! reference registers one in `symbols.js`. This tree has no Monaco runtime
//! (`grep -ri monaco app/src/*.rs` matches doc comments only), so there is no
//! chord handler to port and none is invented here. What *is* portable — and
//! what this module provides — is the model behind that provider.
//!
//! Consequently [`decide_chord`] exists for the *inverse* reason to the
//! `#editArgs` handler: it documents that Ctrl+Shift+O must map to the outline
//! and, crucially, that the **Alt+O** gesture belongs to `definitionAside`
//! (`nav.js:442`) and must NOT be folded into the outline. Two chords that both
//! involve the letter O are exactly the pair a careless port merges.
//!
//! ## The model
//!
//! [`Outline::build`] reproduces `buildSymbols` (`symbols.js:235-330`):
//!
//! 1. **Nesting from `owner`/`parent`.** `symbols.js:248-262` builds an index
//!    `byFull` (by `full`) and `byId` (by `id`), then for each row picks
//!    `owner` looked up in `byFull` first; only if that fails does it try
//!    `parent` in `byId`, then `parent` in `byFull`. A row never becomes its own
//!    parent (`!== i` in all three comparisons).
//! 2. **Cycle cutting.** `symbols.js:259-271` walks each parent chain with a
//!    `seen` set and a `guard` of `n` steps, and detaches the row to the root
//!    when a cycle is found. The reference's own comment: "Cut cycles (malformed
//!    owner chains must not hang the outline)." This port keeps that — the walk
//!    is bounded, so a malformed chain cannot hang or overflow here either.
//! 3. **Roots and children in document order.** `symbols.js:273-280`: a row whose
//!    parent is `-1` is a root; children keep ascending row order.
//! 4. **Depth cap of 12.** `buildList` (`symbols.js:296`) returns `[]` once
//!    `depth > 12`, so nesting stops at 12 levels rather than recursing forever.
//! 5. **Display name.** `displayName` (`symbols.js:229-231`) is
//!    `label || full || name || id || 'symbol'` — the literal fallback is
//!    `'symbol'`, not an empty string.
//! 6. **Kind mapping.** `symbolKind` (`symbols.js:209-227`) maps the CS kind to a
//!    Monaco `SymbolKind`, whose *numeric* values are stable and public. The
//!    numbers are mirrored here (see [`SymbolKind`]) because they are what a
//!    consumer compares against; the unknown fallback is `Variable` (5), exactly
//!    as the reference's `default:` arm.
//! 7. **`MAX_SYMBOLS = 5000`.** `symbols.js:25` and the `slice` at `:236`: only
//!    the first 5000 rows participate.
//!
//! ## What is deliberately NOT modelled
//!
//! `buildSymbols` also computes Monaco `Range`s by translating each row's real
//! `(file, line)` into the constructed document's line numbers via
//! `lineIndexFor(model)` / `endLineFor` (`symbols.js:180-205`), which needs the
//! live model and its line records. Those are document geometry, not outline
//! structure; [`OutlineRow`] carries the row's own `line`/`end_line` and leaves
//! the translation to whatever document a consumer builds. The `range`,
//! `selectionRange` and `tags` fields of the reference object are therefore not
//! reproduced, and neither are the sibling `foldingRanges` /
//! `documentHighlights` providers (different features sharing one query).
//!
//! ## Reachability (stated plainly)
//!
//! There is **no consumer of this module yet**: see the handoff. The model is
//! pure and fully tested; nothing in `app/src` calls it. It reads real rows via
//! the existing [`crate::codelens::decls_for`] / [`crate::model::Node`] shapes
//! rather than a parallel engine access, so wiring it later does not re-derive
//! the data source.

use crate::model::Graph;

/// `symbols.js:25` — `var MAX_SYMBOLS = 5000;`, applied by the `slice` at
/// `:236`. Rows past this are dropped, not an error.
pub const MAX_SYMBOLS: usize = 5000;

/// `symbols.js:296` — `if (depth > 12) return out;`. Nesting stops here; a row
/// at depth 13 and below is omitted entirely (the reference returns an empty
/// list, it does not flatten).
pub const MAX_DEPTH: usize = 12;

/// `symbols.js:230` — the literal last resort in `displayName`.
pub const FALLBACK_NAME: &str = "symbol";

/// Map a CS declaration kind to its Monaco `SymbolKind` numeric value, exactly
/// as `symbolKind` (`symbols.js:209-227`) does — including the `Variable`
/// default.
///
/// The numbers come from Monaco's public, long-stable `SymbolKind` enumeration,
/// which is what `monacoRef.languages.SymbolKind` resolves to in the reference.
/// They are returned as `i64` rather than modelled as an enum because the
/// reference's own contract is "a number a consumer compares", and the
/// unrecognised case is a legitimate value (`Variable`), not an error — an enum
/// here would invite a `Result` the reference does not have.
///
/// Note `method`, `constructor` and `operator` all collapse to `Method` (6), as
/// the reference's three separate `case` labels returning one value.
pub fn symbol_kind(kind: &str) -> i64 {
    match kind {
        "namespace" => 3,
        "class" | "record" => 4,
        "struct" => 22,
        "interface" => 10,
        "enum" => 9,
        // Monaco: Method = 6, Constructor = 9, but the reference maps
        // method/constructor/operator ALL to `K.Method` (= 6).
        "method" | "constructor" | "operator" => 6,
        "property" => 7,
        "field" => 8,
        "event" => 13,
        "enumMember" => 17,
        // `default: return K.Variable;`
        _ => 5,
    }
}

/// `displayName(row)` (`symbols.js:229-231`):
/// `label || full || name || id || 'symbol'`.
///
/// "Falsy" in the reference means empty string *and* absent, so each candidate is
/// tried in order and the first non-empty wins. The literal [`FALLBACK_NAME`] is
/// last, so a row with nothing at all still yields a non-empty name.
pub fn display_name(label: &str, full: &str, name: &str, id: &str) -> String {
    for c in [label, full, name, id] {
        if !c.is_empty() {
            return c.to_string();
        }
    }
    FALLBACK_NAME.to_string()
}

/// One flat declaration row — the `rows[i]` objects `buildSymbols` nests.
///
/// Mirrors the subset of `/api/decls` lite node fields the reference reads:
/// `id`, `full`, `owner`, `parent`, `label`, `name`, `kind`, `line`, `endLine`
/// (`symbols.js:248-262`, `:229`, `:216`, `:196-198`). Built from
/// [`crate::model::Node`] by [`OutlineRow::from_node`], so the app's existing
/// graph is the source and no second engine access is introduced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutlineRow {
    /// `row.id`.
    pub id: String,
    /// `row.full` — the key `byFull` indexes.
    pub full: String,
    /// `row.owner` — a fully-qualified owner name, looked up in `byFull`.
    pub owner: String,
    /// `row.parent` — a node id *or* a full name (`symbols.js:256-260`).
    pub parent: String,
    /// `row.label`, first candidate in [`display_name`].
    pub label: String,
    /// `row.name`.
    pub name: String,
    /// `row.kind` — the CS kind driving [`symbol_kind`].
    pub kind: String,
    /// `row.line`.
    pub line: i64,
    /// `row.endLine`.
    pub end_line: i64,
}

impl OutlineRow {
    /// Adapt a real graph node. `owner`/`parent` come from the node's `owner`
    /// (the raw payload's `parent` is not carried by [`crate::model::Node`], so
    /// it is left empty and the `owner` lookup — the reference's *first*
    /// branch — does the work).
    pub fn from_node(n: &crate::model::Node) -> OutlineRow {
        OutlineRow {
            id: n.id.clone(),
            full: n.full.clone(),
            owner: n.owner.clone(),
            parent: String::new(),
            label: n.label.clone(),
            name: n.label.clone(),
            kind: n.kind.clone(),
            line: n.line,
            end_line: n.end_line,
        }
    }

    /// `displayName(row)` for this row.
    pub fn display_name(&self) -> String {
        display_name(&self.label, &self.full, &self.name, &self.id)
    }

    /// `symbolKind(row.kind)` for this row.
    pub fn symbol_kind(&self) -> i64 {
        symbol_kind(&self.kind)
    }
}

/// One node of the nested outline: a row plus its children, in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineNode {
    /// Index into the original (unsliced) row list, so a consumer can map back.
    pub row: usize,
    /// `displayName(row)`.
    pub name: String,
    /// `row.kind` — the reference's `detail` (`symbols.js:288`).
    pub detail: String,
    /// `symbolKind(row.kind)`.
    pub kind: i64,
    /// The row's own origin line (not a document line — see the module docs).
    pub line: i64,
    /// The row's own origin end line.
    pub end_line: i64,
    /// Nested children, ascending row order.
    pub children: Vec<OutlineNode>,
}

/// The built outline: roots in document order, plus the honest counts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Outline {
    /// Root rows in document order.
    pub roots: Vec<OutlineNode>,
    /// How many rows participated (`min(rows.len(), MAX_SYMBOLS)`).
    pub considered: usize,
    /// How many rows were dropped by the [`MAX_SYMBOLS`] cap.
    pub dropped: usize,
    /// How many rows were detached to the root by cycle cutting.
    pub cycles_cut: usize,
    /// How many rows were omitted because they fell past [`MAX_DEPTH`].
    pub truncated_by_depth: usize,
}

impl Outline {
    /// Nest flat rows exactly as `buildSymbols` (`symbols.js:235-330`) does.
    pub fn build(rows: &[OutlineRow]) -> Outline {
        let dropped = rows.len().saturating_sub(MAX_SYMBOLS);
        let rows = &rows[..rows.len().min(MAX_SYMBOLS)];
        let n = rows.len();

        // `byFull` / `byId` — first occurrence wins, as the reference's plain
        // object assignment does (a later duplicate overwrites, but so does the
        // reference's; both are "one slot per key").
        let mut by_full: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        let mut by_id: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (i, r) in rows.iter().enumerate() {
            if !r.full.is_empty() {
                by_full.insert(r.full.as_str(), i);
            }
            if !r.id.is_empty() {
                by_id.insert(r.id.as_str(), i);
            }
        }

        // Nesting, exactly the reference's precedence: owner-by-full, else
        // parent-by-id, else parent-by-full. Never self (`!== i`).
        let mut parent: Vec<Option<usize>> = Vec::with_capacity(n);
        for (i, r) in rows.iter().enumerate() {
            let mut p: Option<usize> = None;
            if !r.owner.is_empty() {
                if let Some(&j) = by_full.get(r.owner.as_str()) {
                    if j != i {
                        p = Some(j);
                    }
                }
            }
            if p.is_none() && !r.parent.is_empty() {
                if let Some(&j) = by_id.get(r.parent.as_str()) {
                    if j != i {
                        p = Some(j);
                    }
                } else if let Some(&j) = by_full.get(r.parent.as_str()) {
                    if j != i {
                        p = Some(j);
                    }
                }
            }
            parent.push(p);
        }

        // Cycle cutting (`symbols.js:259-271`). Each row's own ancestor chain is
        // walked with a `seen` set and a `guard` of `n` steps; a repeat detaches
        // **that row** to the root. The walk is bounded, so a malformed chain
        // cannot hang or blow the stack — the reference's own stated reason.
        //
        // Faithfulness note: like the reference, this detaches only the row that
        // *observes* the cycle, not every member of it. After the pass every row
        // that can reach a root does; a row still attached inside a pure cycle
        // (a cycle where no member was detached, because each member's walk
        // terminated on the `guard` before seeing a repeat) is unreachable and
        // is therefore promoted to a root below, so no row is ever silently
        // lost from the outline.
        let mut cycles_cut = 0usize;
        for i in 0..n {
            let mut seen = std::collections::HashSet::new();
            let mut q = parent[i];
            let mut guard = 0usize;
            while let Some(cur) = q {
                if guard > n {
                    break;
                }
                guard += 1;
                if !seen.insert(cur) {
                    if parent[i].is_some() {
                        parent[i] = None;
                        cycles_cut += 1;
                    }
                    break;
                }
                q = parent[cur];
            }
        }

        // Reachability repair: promote any row that cannot reach a root. Without
        // this a leftover cycle would swallow its members and `Outline::len()`
        // would silently under-report the corpus. This is the `Cut cycles`
        // comment's *intent* in `symbols.js:259` made unconditional; the
        // reference relies on its `guard` breaking in a way that happens to
        // detach, which does not cover every cycle shape.
        for i in 0..n {
            let mut seen = std::collections::HashSet::new();
            let mut q = parent[i];
            let mut reached_root = false;
            let mut guard = 0usize;
            while let Some(cur) = q {
                if guard > n {
                    break;
                }
                guard += 1;
                if !seen.insert(cur) {
                    break;
                }
                match parent[cur] {
                    Some(next) => q = Some(next),
                    None => {
                        reached_root = true;
                        break;
                    }
                }
            }
            if !reached_root && parent[i].is_some() {
                parent[i] = None;
                cycles_cut += 1;
            }
        }

        let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut roots: Vec<usize> = Vec::new();
        for i in 0..n {
            match parent[i] {
                Some(p) => children[p].push(i),
                None => roots.push(i),
            }
        }

        let mut truncated_by_depth = 0usize;
        let roots = roots
            .into_iter()
            .map(|i| build_node(i, 1, rows, &children, &mut truncated_by_depth))
            .collect();

        Outline { roots, considered: n, dropped, cycles_cut, truncated_by_depth }
    }

    /// Convenience: build straight from real graph nodes, via
    /// [`OutlineRow::from_node`].
    pub fn from_nodes(nodes: &[crate::model::Node]) -> Outline {
        let rows: Vec<OutlineRow> = nodes.iter().map(OutlineRow::from_node).collect();
        Outline::build(&rows)
    }

    /// Convenience: build from a graph's nodes.
    pub fn from_graph(graph: &Graph) -> Outline {
        Outline::from_nodes(&graph.nodes)
    }

    /// Total rows in the built tree (all depths).
    pub fn len(&self) -> usize {
        fn count(n: &OutlineNode) -> usize {
            1 + n.children.iter().map(count).sum::<usize>()
        }
        self.roots.iter().map(count).sum()
    }

    /// True when the outline has no roots — `documentSymbols` returning `[]`.
    ///
    /// A closed/empty outline is a real state and is distinct from "not built":
    /// [`Outline::build`] on an empty slice yields this, whereas a consumer that
    /// has not asked yet should hold an `Option`/absent value instead. The
    /// distinction is what stops an unbuilt outline rendering as "no symbols".
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }
}

fn build_node(
    i: usize,
    depth: usize,
    rows: &[OutlineRow],
    children: &[Vec<usize>],
    truncated: &mut usize,
) -> OutlineNode {
    let r = &rows[i];
    // `buildList` (`symbols.js:296`) returns `[]` when `depth > 12`, and the
    // reference calls it with `depth + 1` per level starting from 1. The check
    // therefore fires when the *child* depth would be 13, so emission stops at
    // depth 12 — mirrored exactly rather than shifted by one.
    let kids = if depth >= MAX_DEPTH {
        // This node IS still emitted; only its descendants are dropped (the
        // reference returns an empty child list rather than flattening). So the
        // omitted count is exactly the descendant subtree, never `1 + subtree`.
        *truncated += count_subtree(i, children);
        Vec::new()
    } else {
        children[i]
            .iter()
            .map(|&c| build_node(c, depth + 1, rows, children, truncated))
            .collect()
    };
    OutlineNode {
        row: i,
        name: r.display_name(),
        detail: r.kind.clone(),
        kind: r.symbol_kind(),
        line: r.line,
        end_line: r.end_line,
        children: kids,
    }
}

fn count_subtree(i: usize, children: &[Vec<usize>]) -> usize {
    children[i].iter().map(|&c| 1 + count_subtree(c, children)).sum()
}

/// Which outline-relevant gesture a key event is — the Ctrl+Shift+O side of the
/// key routing, and the reason the letter O is ambiguous on this project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chord {
    /// Ctrl/Cmd+Shift+O — the document outline (`symbols.js:7`).
    Outline,
    /// **Alt**+O — `definitionAside` (`nav.js:442`, guarded `nav.js:439`).
    /// Deliberately a distinct variant so it can never be merged with
    /// [`Chord::Outline`].
    DefinitionAside,
}

/// The modifiers a chord decision reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChordEvent {
    /// `ev.key`, already lower-cased by the caller.
    pub key: char,
    /// `ev.ctrlKey`.
    pub ctrl: bool,
    /// `ev.metaKey`.
    pub meta: bool,
    /// `ev.shiftKey`.
    pub shift: bool,
    /// `ev.altKey`.
    pub alt: bool,
}

/// Decide the outline chord from a key event: **Ctrl/Cmd+Shift+O** -> outline.
///
/// This mirrors the *modifier* shape the reference's Ctrl+Shift handlers use
/// (`arrange.js:416` `(ev.ctrlKey || ev.metaKey) && ev.shiftKey`), applied to the
/// `O` key that `symbols.js:7` documents as the outline chord. It also encodes
/// the one distinction that matters: **Alt+O is `definitionAside`, not the
/// outline** (`nav.js:439-442`), and any bare `O` is neither.
///
/// Returns `None` for everything else, so a caller cannot accidentally treat an
/// unmatched key as the outline.
pub fn decide_chord(ev: ChordEvent) -> Option<Chord> {
    if ev.key == 'o' {
        // Alt+O: `ev.altKey && !ctrl && !meta && !shift` (`nav.js:439`).
        if ev.alt && !ev.ctrl && !ev.meta && !ev.shift {
            return Some(Chord::DefinitionAside);
        }
        if (ev.ctrl || ev.meta) && ev.shift && !ev.alt {
            return Some(Chord::Outline);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, full: &str, owner: &str, kind: &str, line: i64) -> OutlineRow {
        OutlineRow {
            id: id.to_string(),
            full: full.to_string(),
            owner: owner.to_string(),
            parent: String::new(),
            // `label` is left empty on purpose: `displayName` then falls through
            // to `full`, so a test asserting on a node's `name` is asserting the
            // row's FULL name and stays readable.
            label: String::new(),
            name: String::new(),
            kind: kind.to_string(),
            line,
            end_line: line + 1,
        }
    }

    // ------------------------------------------------------------ chord policy

    /// Ctrl+Shift+O is the outline.
    #[test]
    fn ctrl_shift_o_is_the_outline() {
        let ev = ChordEvent { key: 'o', ctrl: true, meta: false, shift: true, alt: false };
        assert_eq!(decide_chord(ev), Some(Chord::Outline));
    }

    /// Cmd+Shift+O is the same gesture — the reference's Ctrl/Alt handlers always
    /// accept `metaKey` alongside `ctrlKey` (`arrange.js:416`).
    #[test]
    fn meta_shift_o_is_the_outline() {
        let ev = ChordEvent { key: 'o', ctrl: false, meta: true, shift: true, alt: false };
        assert_eq!(decide_chord(ev), Some(Chord::Outline));
    }

    /// **The two-directional half that matters:** Alt+O is `definitionAside` and
    /// must never be the outline.
    #[test]
    fn alt_o_is_definition_aside_and_not_the_outline() {
        let ev = ChordEvent { key: 'o', ctrl: false, meta: false, shift: false, alt: true };
        assert_eq!(decide_chord(ev), Some(Chord::DefinitionAside));
        assert_ne!(decide_chord(ev), Some(Chord::Outline), "Alt+O must not open the outline");
    }

    /// A bare `O`, and O with only Ctrl (no Shift), are neither chord.
    #[test]
    fn bare_and_ctrl_only_o_are_neither_chord() {
        for ev in [
            ChordEvent { key: 'o', ctrl: false, meta: false, shift: false, alt: false },
            ChordEvent { key: 'o', ctrl: true, meta: false, shift: false, alt: false },
            ChordEvent { key: 'o', ctrl: false, meta: true, shift: false, alt: false },
            ChordEvent { key: 'o', ctrl: true, meta: false, shift: true, alt: true },
        ] {
            assert_eq!(decide_chord(ev), None, "{ev:?}");
        }
    }

    /// Only the `o` key is routed; other letters with the same modifiers are not.
    #[test]
    fn other_keys_are_not_the_outline() {
        for key in ['v', 'h', 'd', 'c', 'w', 's', 'p'] {
            let ev = ChordEvent { key, ctrl: true, meta: false, shift: true, alt: false };
            assert_eq!(decide_chord(ev), None, "{key} must not open the outline");
        }
    }

    // ------------------------------------------------------------- displayName

    /// `displayName`'s precedence, each step, plus the literal fallback.
    #[test]
    fn display_name_prefers_label_then_full_then_name_then_id() {
        assert_eq!(display_name("L", "F", "N", "I"), "L");
        assert_eq!(display_name("", "F", "N", "I"), "F");
        assert_eq!(display_name("", "", "N", "I"), "N");
        assert_eq!(display_name("", "", "", "I"), "I");
        assert_eq!(display_name("", "", "", ""), "symbol");
        assert_eq!(display_name("", "", "", ""), FALLBACK_NAME);
    }

    /// A name is never empty, even for a row with no fields at all.
    #[test]
    fn a_name_is_never_empty() {
        let r = OutlineRow::default();
        assert_eq!(r.display_name(), "symbol");
        assert!(!r.display_name().is_empty());
    }

    // -------------------------------------------------------------- kind map

    /// Every case arm in `symbolKind`, plus the `Variable` default, asserted in
    /// both directions (mapped kinds differ from the default; unknown kinds all
    /// collapse to it).
    #[test]
    fn symbol_kinds_match_the_reference_map() {
        assert_eq!(symbol_kind("namespace"), 3);
        assert_eq!(symbol_kind("class"), 4);
        assert_eq!(symbol_kind("record"), 4);
        assert_eq!(symbol_kind("struct"), 22);
        assert_eq!(symbol_kind("interface"), 10);
        assert_eq!(symbol_kind("enum"), 9);
        assert_eq!(symbol_kind("method"), 6);
        assert_eq!(symbol_kind("constructor"), 6);
        assert_eq!(symbol_kind("operator"), 6);
        assert_eq!(symbol_kind("property"), 7);
        assert_eq!(symbol_kind("field"), 8);
        assert_eq!(symbol_kind("event"), 13);
        assert_eq!(symbol_kind("enumMember"), 17);
        // The `default:` arm.
        assert_eq!(symbol_kind("delegate"), 5);
        assert_eq!(symbol_kind(""), 5);
        assert_eq!(symbol_kind("Class"), 5, "the reference is case-sensitive");
        assert_ne!(symbol_kind("class"), symbol_kind("nonsense"));
    }

    // ---------------------------------------------------------------- nesting

    /// The core derivation: children nest under their `owner` by full name.
    #[test]
    fn rows_nest_under_their_owner() {
        let rows = vec![
            row("N1", "NZK.VF", "", "namespace", 1),
            row("C1", "NZK.VF.Folder", "NZK.VF", "class", 10),
            row("M1", "NZK.VF.Folder.Open", "NZK.VF.Folder", "method", 12),
        ];
        let o = Outline::build(&rows);
        assert_eq!(o.roots.len(), 1, "one root: the namespace");
        assert_eq!(o.roots[0].name, "NZK.VF");
        assert_eq!(o.roots[0].children.len(), 1);
        assert_eq!(o.roots[0].children[0].name, "NZK.VF.Folder");
        assert_eq!(o.roots[0].children[0].children.len(), 1);
        assert_eq!(o.roots[0].children[0].children[0].name, "NZK.VF.Folder.Open");
        assert_eq!(o.len(), 3);
        assert!(!o.is_empty());
    }

    /// `parent` is looked up **by id first**, then by full name
    /// (`symbols.js:256-260`) — and only when `owner` did not resolve.
    #[test]
    fn parent_resolves_by_id_and_then_by_full_name() {
        let rows = vec![
            row("C1", "NS.Klass", "", "class", 5),
            // parent is an ID
            OutlineRow { parent: "C1".into(), ..row("M1", "NS.Klass.ById", "", "method", 6) },
            // parent is a full name
            OutlineRow { parent: "NS.Klass".into(), ..row("M2", "NS.Klass.ByFull", "", "method", 7) },
        ];
        let o = Outline::build(&rows);
        assert_eq!(o.roots.len(), 1);
        assert_eq!(o.roots[0].name, "NS.Klass");
        assert_eq!(o.roots[0].children.len(), 2);
        assert_eq!(o.roots[0].children[0].name, "NS.Klass.ById");
        assert_eq!(o.roots[0].children[1].name, "NS.Klass.ByFull");
    }

    /// `owner` wins over `parent` when both resolve — the reference's `if/else
    /// if` precedence.
    #[test]
    fn owner_takes_precedence_over_parent() {
        let rows = vec![
            row("A", "NS.A", "", "class", 1),
            row("B", "NS.B", "", "class", 2),
            OutlineRow {
                owner: "NS.A".into(),
                parent: "B".into(),
                ..row("M", "NS.A.M", "", "method", 3)
            },
        ];
        let o = Outline::build(&rows);
        let a = o.roots.iter().find(|r| r.name == "NS.A").expect("NS.A is a root");
        let b = o.roots.iter().find(|r| r.name == "NS.B").expect("NS.B is a root");
        assert_eq!(a.children.len(), 1, "owner (NS.A) won");
        assert!(b.children.is_empty(), "parent (B) must not also claim it");
    }

    /// A row never becomes its own parent, in each of the three comparisons.
    #[test]
    fn a_row_is_never_its_own_parent() {
        let rows = vec![
            OutlineRow { owner: "SELF".into(), ..row("S", "SELF", "", "class", 1) },
            OutlineRow { parent: "S2".into(), ..row("S2", "OTHER", "", "class", 2) },
        ];
        let o = Outline::build(&rows);
        assert_eq!(o.roots.len(), 2, "both rows must be roots, not self-nested");
        assert!(o.roots.iter().all(|r| r.children.is_empty()));
    }

    /// Document order is preserved: roots ascend, and so do siblings.
    #[test]
    fn document_order_is_preserved() {
        let rows = vec![
            row("B", "NS.B", "", "class", 20),
            row("A", "NS.A", "", "class", 10),
            row("A2", "NS.A.Second", "NS.A", "method", 12),
            row("A1", "NS.A.First", "NS.A", "method", 11),
        ];
        let o = Outline::build(&rows);
        assert_eq!(
            o.roots.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["NS.B", "NS.A"]
        );
        let a = &o.roots[1];
        assert_eq!(
            a.children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["NS.A.Second", "NS.A.First"],
            "siblings keep row order, not line order"
        );
    }

    // ------------------------------------------------------------ cycle cutting

    /// A direct 2-cycle is cut: neither row may vanish, and the walk terminates.
    #[test]
    fn a_two_cycle_is_cut_rather_than_hanging() {
        let rows = vec![
            OutlineRow { owner: "B".into(), ..row("A", "A", "", "class", 1) },
            OutlineRow { owner: "A".into(), ..row("B", "B", "", "class", 2) },
        ];
        let o = Outline::build(&rows);
        assert_eq!(o.len(), 2, "both rows survive");
        assert!(o.cycles_cut >= 1, "the cycle must be reported, not silent");
        assert!(o.roots.len() >= 1, "something must surface at the root");
    }

    /// A self-owner is not a cycle to cut — it is excluded by the `!== i` rule,
    /// so the row is simply a root and nothing is counted as cut.
    #[test]
    fn a_self_owner_is_a_root_and_not_a_cycle() {
        let rows = vec![OutlineRow { owner: "SELF".into(), ..row("S", "SELF", "", "class", 1) }];
        let o = Outline::build(&rows);
        assert_eq!(o.cycles_cut, 0);
        assert_eq!(o.roots.len(), 1);
    }

    /// A long malformed chain still terminates and loses no rows *silently* —
    /// the property the reference's `guard` exists for.
    ///
    /// A row may legitimately be absent from the built tree for two **reported**
    /// reasons: cycle cutting ([`Outline::cycles_cut`]) and the depth cap
    /// ([`Outline::truncated_by_depth`]). The invariant is therefore that the
    /// tree plus the depth-truncated count accounts for every considered row, so
    /// an under-report is always visible rather than silent.
    #[test]
    fn a_long_malformed_cycle_still_terminates() {
        let rows: Vec<OutlineRow> = (0..40)
            .map(|i| {
                let owner = format!("N{}", (i + 1) % 40);
                OutlineRow { owner, ..row(&format!("N{i}"), &format!("N{i}"), "", "class", i as i64) }
            })
            .collect();
        let o = Outline::build(&rows);
        assert_eq!(o.considered, 40);
        assert_eq!(
            o.len() + o.truncated_by_depth,
            40,
            "every row must be emitted or reported as depth-truncated \
             (len={} truncated={} cut={})",
            o.len(),
            o.truncated_by_depth,
            o.cycles_cut
        );
        assert!(o.cycles_cut > 0, "this input is a cycle and must be reported as one");
    }

    // ------------------------------------------------------------- depth cap

    /// Nesting is capped at [`MAX_DEPTH`]; deeper rows are counted as truncated
    /// rather than silently dropped or flattened.
    #[test]
    fn nesting_stops_at_the_depth_cap() {
        let n = MAX_DEPTH + 4;
        let rows: Vec<OutlineRow> = (0..n)
            .map(|i| {
                let owner = if i == 0 { String::new() } else { format!("N{}", i - 1) };
                OutlineRow { owner, ..row(&format!("N{i}"), &format!("N{i}"), "", "class", i as i64) }
            })
            .collect();
        let o = Outline::build(&rows);
        assert_eq!(o.roots.len(), 1);
        // Walk to the deepest emitted node.
        let mut depth = 1;
        let mut cur = &o.roots[0];
        while let Some(c) = cur.children.first() {
            depth += 1;
            cur = c;
        }
        assert_eq!(depth, MAX_DEPTH, "emitted depth must stop at MAX_DEPTH");
        assert!(o.truncated_by_depth > 0, "the omitted rows must be reported");
        assert!(o.len() < n, "the cap must actually omit rows");
    }

    // ------------------------------------------------- MAX_SYMBOLS cap + empty

    /// The empty outline is a real, distinguishable state.
    #[test]
    fn an_empty_outline_is_distinguishable_from_a_populated_one() {
        let empty = Outline::build(&[]);
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.considered, 0);
        assert_eq!(empty.dropped, 0);

        let one = Outline::build(&[row("A", "A", "", "class", 1)]);
        assert!(!one.is_empty());
        assert_eq!(one.len(), 1);
        assert_ne!(empty, one, "empty and populated must not compare equal");
    }

    /// Rows past [`MAX_SYMBOLS`] are dropped **and counted** — the cap is not
    /// silent.
    #[test]
    fn the_symbol_cap_drops_and_reports_the_excess() {
        let n = MAX_SYMBOLS + 7;
        let rows: Vec<OutlineRow> = (0..n)
            .map(|i| row(&format!("N{i}"), &format!("Root{i}"), "", "class", i as i64))
            .collect();
        let o = Outline::build(&rows);
        assert_eq!(o.considered, MAX_SYMBOLS);
        assert_eq!(o.dropped, 7);
        assert_eq!(o.len(), MAX_SYMBOLS);
        assert!(o.considered + o.dropped == n);
    }

    /// An unattached row is a root, and `row` is the ORIGINAL index so a consumer
    /// can map a node back to its input row.
    #[test]
    fn roots_keep_their_original_row_index() {
        let rows = vec![
            row("A", "A", "", "class", 1),
            row("B", "B", "", "class", 2),
            row("C", "C", "", "class", 3),
        ];
        let o = Outline::build(&rows);
        assert_eq!(o.roots.iter().map(|r| r.row).collect::<Vec<_>>(), [0, 1, 2]);
        for (k, r) in o.roots.iter().enumerate() {
            assert_eq!(r.name, rows[r.row].display_name(), "row index must round-trip");
            assert_eq!(r.line, rows[k].line);
        }
    }

    /// `detail` and `kind` are carried from the row's CS kind.
    #[test]
    fn detail_and_kind_come_from_the_row_kind() {
        let o = Outline::build(&[row("I", "NS.I", "", "interface", 4)]);
        assert_eq!(o.roots[0].detail, "interface");
        assert_eq!(o.roots[0].kind, 10);
        assert_ne!(o.roots[0].kind, symbol_kind("class"));
    }

    // ------------------------------------------------------------- from nodes

    /// A real [`crate::model::Node`] adapts through `from_node`, so the outline
    /// consumes the app's existing graph shape.
    #[test]
    fn outline_builds_from_real_graph_nodes() {
        let node = crate::model::Node {
            id: "C1".into(),
            label: "Folder".into(),
            full: "NZK.VF.Folder".into(),
            kind: "class".into(),
            owner: "NZK.VF".into(),
            file: "a.cs".into(),
            line: 10,
            end_line: 20,
            ..Default::default()
        };
        let r = OutlineRow::from_node(&node);
        assert_eq!(r.id, "C1");
        assert_eq!(r.full, "NZK.VF.Folder");
        assert_eq!(r.owner, "NZK.VF");
        assert_eq!(r.display_name(), "Folder");
        assert_eq!(r.symbol_kind(), 4);
        assert_eq!(r.line, 10);
        assert_eq!(r.end_line, 20);

        let o = Outline::from_nodes(&[node]);
        assert_eq!(o.roots.len(), 1);
        assert_eq!(o.roots[0].name, "Folder");
        assert_eq!(o.roots[0].end_line, 20);
    }
}
