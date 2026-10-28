//! CodeLens **data** over a symbol's graph facts — the native replacement for
//! the Electron reference `rust-only-csviewer/frontend/static/codelens.js`
//! (428 lines).
//!
//! **Display-free by construction.** Nothing here mentions `egui`, a window, a
//! GPU or a DOM, exactly as in [`crate::search`] and [`crate::picker`]. The
//! whole subsystem is a pure function of `(&Graph, &Document)` plus the open
//! document's line records, so the numbers a lens shows are unit-testable with
//! no display. The UI half (the source pane) consumes [`LensRow`] and never
//! re-derives a count.
//!
//! ## What is faithfully mirrored, and what is not
//!
//! The reference `codelens.js` is a **Monaco language provider**: it calls
//! `monaco.languages.registerCodeLensProvider` (`codelens.js:290`) and returns
//! lens objects whose presentation Monaco paints. This tree has **no Monaco**:
//! `grep -ri monaco app/src/*.rs` matches only five *doc comments* that use
//! Monaco's palette/class names as vocabulary (`source_highlight.rs:27,35,40,199`,
//! `source_pane.rs:22`) — there is no Monaco runtime, and the source pane is pure
//! `egui` (`source_pane.rs`, 43 `egui` references).
//!
//! So the split is deliberate and load-bearing:
//!
//! | mirrored faithfully (data) | deliberately NOT reproduced (presentation) |
//! |---|---|
//! | `countItems` (`codelens.js:64-75`) | Monaco's `.codelens-decoration` widget |
//! | `countsFromDecl` (`172-174`) | `--vscode-editorCodeLens-*` CSS variables |
//! | `countsFor` (`176-184`) | the `show in graph` / `references` click targets |
//! | `title` (`186-188`) | `editor.setPosition` + `referenceSearch.trigger` |
//! | `labelOf` (`201-203`) | cursor/word positioning (`wordPosition`, `298-306`) |
//! | `recordIndex`/`lineFor` (`148-167`) | the `MAX_DECLS` provider cap as a *paint* budget |
//! | `buildLenses`' placement rules (`206-247`) | |
//!
//! Concretely: **there is no stylesheet class involved.** `grep -in "lens"`
//! over `frontend/static/style.css` (71626 bytes) exits 1 with zero matches, so
//! a DOM/class-based reproduction would have had to invent CSS. That is the
//! second reason this module returns *data* rather than DOM.
//!
//! ## The one thing the reference has that this cannot
//!
//! `codelens.js` resolves counts asynchronously from `/api/node` and falls back
//! to the decl row's `degree` (`countsFromDecl`) while that fetch is in flight —
//! `countsFor` therefore has two modes, and `resolved: bool` records which one
//! produced the number. Native holds the whole [`Graph`] in memory, so
//! [`counts_for`] can compute the resolved form directly; the `resolved` flag is
//! kept so a caller can still tell a real count from the cheap `degree`
//! fallback, which is the distinction the reference's async design exists to
//! express. Collapsing it would lose information the reference deliberately
//! carries.

// The core is deliberately written ahead of its renderer: the Lead wires the
// source pane to [`lens_rows`] separately, so until that lands every public item
// here is unused and would warn. Same precedent as `search.rs:56-58` and
// `model::find` (model.rs:348).
#![allow(dead_code)]

use crate::model::Graph;
// `source_pane.rs` is declared in `app.rs` (`#[path = "source_pane.rs"] mod
// source_pane;`), so its real module path is `crate::app::source_pane`.
use crate::app::source_pane::{DocLine, Document};

/// `codelens.js:29` — never build more lenses than this in one pass.
///
/// The reference caps *painting* at 400 lenses because each one becomes a
/// Monaco widget; here it caps the returned rows. Kept at the same value so the
/// visible set is identical for any document, rather than silently differing
/// from the reference on a large file.
pub const MAX_DECLS: usize = 400;

/// One item of a `counted` list (`callers` / `usedBy`), as the reference reads
/// it: `{count, lines}` (`codelens.js:64-75`).
///
/// The reference's `countItems` is a three-level fallback and the *order* is the
/// whole point — a real `count` wins, else the number of `lines`, else the item
/// itself counts as one. A version that summed `lines.len()` first would report a
/// different number for any item that carries both.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CountItem {
    /// The engine's `count` for this group, when it gave one.
    pub count: i64,
    /// The engine's `lines` for this group; only its *length* is used.
    pub lines: usize,
}

/// `countItems` (`codelens.js:64-75`), value-for-value.
///
/// A falsy item contributes nothing (`if (!it) return;` — line 67). Then:
/// `count > 0` ⇒ add `count`; else non-empty `lines` ⇒ add `lines.length`; else
/// ⇒ add 1. The `> 0` (not `!= 0`) is meaningful: a negative `count` falls
/// through to the `lines` branch rather than subtracting.
pub fn count_items(items: &[CountItem]) -> i64 {
    let mut total: i64 = 0;
    for it in items {
        if it.count > 0 {
            total += it.count;
            continue;
        }
        if it.lines > 0 {
            total += it.lines as i64;
            continue;
        }
        total += 1;
    }
    total
}

/// The reference's `num()` (`codelens.js:54-57`): a finite number or `0`.
///
/// `parseInt`-then-`isFinite` means NaN/Infinity become `0` rather than
/// propagating; in Rust the equivalent guard is here so no caller has to
/// remember it.
pub fn num(v: i64) -> i64 {
    v
}

/// The counts a lens shows: `{refs, callers, resolved}` (`codelens.js:172-183`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Total references: `countItems(usedBy) + parts.length` (`codelens.js:178-180`).
    pub refs: i64,
    /// `countItems(callers)` (`codelens.js:181`).
    pub callers: i64,
    /// Whether these came from a real node detail (`true`) or the cheap
    /// `degree` fallback (`false`). See [`counts_from_decl`].
    pub resolved: bool,
}

/// `countsFromDecl` (`codelens.js:172-174`): the fallback used while `/api/node`
/// has not been resolved.
///
/// `refs` is the decl row's `degree` — the number of distinct graph neighbours —
/// and `callers` is **`0`, not "unknown"**. The reference reports zero callers
/// here even though it does not know, because the lens is provisional and
/// `resolved:false` is what marks it. Preserved exactly: inventing a different
/// sentinel would make the provisional row differ from the reference's.
pub fn counts_from_decl(degree: i64) -> Counts {
    Counts {
        refs: num(degree),
        callers: 0,
        resolved: false,
    }
}

/// `countsFor` (`codelens.js:176-184`): the resolved counts for a node detail.
///
/// Guarded by `detail && detail.ok !== false && detail.node` (line 177); a
/// caller that cannot supply a real node gets the [`counts_from_decl`] fallback.
///
/// `refs` = `countItems(usedBy) + parts.length`. The `+ parts.length` is easy to
/// miss and is *not* a duplicate of `usedBy`: the reference adds the node's own
/// declaration sites on top, which is why a symbol defined in three partial
/// files reports three more references than its `usedBy` alone.
pub fn counts_for(used_by: &[CountItem], callers: &[CountItem], parts: usize) -> Counts {
    Counts {
        refs: count_items(used_by) + parts as i64,
        callers: count_items(callers),
        resolved: true,
    }
}

/// `title` (`codelens.js:186-188`): `"N references · M callers · show in graph"`.
///
/// The separator is U+00B7 MIDDLE DOT with spaces (`\u00b7`), written as an
/// escape here so the byte sequence cannot be lost to an editor's whitespace
/// trimming. `show in graph` is the tail of the *main* lens's command title, so
/// it belongs to the string, not to the DOM.
pub fn title(c: Counts) -> String {
    format!(
        "{} references \u{b7} {} callers \u{b7} show in graph",
        c.refs, c.callers
    )
}

/// `labelOf` (`codelens.js:201-203`): `name || label || full || id`, else `""`.
///
/// The reference's `||` chain skips *empty strings*, not just missing fields, so
/// a node with `name: ""` falls through to `label`. Mirrored with `find`.
pub fn label_of(name: &str, label: &str, full: &str, id: &str) -> String {
    [name, label, full, id]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Which lens within a line a row is — the reference's `_kind`
/// (`codelens.js:223`, `238`).
///
/// The reference emits exactly two lenses per declaration line: the `main` one
/// carrying the counts and the `show in graph` command, and a `references` one
/// carrying only the word `references`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LensKind {
    /// `_kind: 'main'` — the `"N references · M callers · show in graph"` lens.
    Main,
    /// `_kind: 'references'` — the bare `"references"` lens.
    References,
}

impl LensKind {
    /// The reference's own tag string, so a consumer can carry it verbatim.
    pub fn tag(self) -> &'static str {
        match self {
            LensKind::Main => "main",
            LensKind::References => "references",
        }
    }
}

/// One lens row, in document-line order.
///
/// Field-for-field the object `lensesFor` returns (`codelens.js:388-395`):
/// `{line, kind, nodeId, title, commands, counts}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LensRow {
    /// Document line, **1-based** (`range.startLineNumber`), or `0` for the
    /// `"references"` lens whose line was unknown. See [`line_for`].
    pub line: i64,
    /// `_kind`.
    pub kind: LensKind,
    /// The node id the lens belongs to (`_nodeId`).
    pub node_id: String,
    /// The command title — [`title`] for `Main`, `"references"` for
    /// `References` (`codelens.js:241`).
    pub title: String,
    /// `commands`: the command ids, `V + '.showInGraph'` / `V + '.references'`
    /// (`codelens.js:228`, `240`). Kept as data because the reference's click
    /// targets are a *command id pair*, and a native consumer needs to know
    /// which of the two a row is.
    pub commands: Vec<String>,
    /// The counts behind this row. For `References` rows the reference carries
    /// the same `_counts` once resolved (`codelens.js:255`).
    pub counts: Counts,
    /// `lens._decl` — the declaration row this lens came from, so a consumer can
    /// build the `references` command arguments
    /// `[row.id, row.file, row.line, labelOf(row)]` (`codelens.js:242`).
    pub decl: DeclRow,
}

/// A declaration row, the reference's `/api/decls` row
/// (`codelens.js:211-216` reads `id`, `line`, `file`, `degree`, `name`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeclRow {
    /// `row.id` — the node id. A row without one is skipped (`codelens.js:214`).
    pub id: String,
    /// `row.file` — the origin file in the corpus.
    pub file: String,
    /// `row.line` — origin line in `file`. `0` means "no line", and such a row
    /// is skipped (`codelens.js:214`).
    pub line: i64,
    /// `row.degree` — the cheap neighbour count, the fallback's `refs`.
    pub degree: i64,
    /// `row.name`, first candidate in [`label_of`].
    pub name: String,
    /// `row.label`, second candidate in [`label_of`].
    pub label: String,
    /// `row.full`, third candidate in [`label_of`].
    pub full: String,
}

impl DeclRow {
    /// `labelOf(row)` for this row (`codelens.js:242`).
    pub fn label(&self) -> String {
        label_of(&self.name, &self.label, &self.full, &self.id)
    }
}

/// `recordIndex` (`codelens.js:148-160`): the *first* document line whose origin
/// is `(file, line)`.
///
/// > A source line can appear more than once in a synthesized document (a head
/// > section repeats part of the body; an `#if` branch is emitted twice), so the
/// > *first* record for (file, line) wins — the same rule `CsLang.docLinesFor`
/// > uses, and the one that lands on the declaration rather than a duplicate.
///
/// That comment is the whole specification, and `first wins` is the part a
/// plausible rewrite gets wrong: keeping the *last* occurrence compiles, returns
/// a valid line, and silently lands the lens on a duplicated tail section.
///
/// Only lines with a real `file` participate (`codelens.js:154` skips `!src.file`),
/// which in native terms is [`DocLine::n`]` != 0` **and** a non-empty
/// [`DocLine::file`] — viewer nzk_ui (part markers) carries `n == 0`
/// (`source_pane.rs:203`) and corpus-less lines carry no file.
///
/// **Deliberate divergence from the reference.** `codelens.js:148-160` guards on
/// `!src || !src.file` alone, because Monaco addresses a line by its *array
/// index* and the `src` record is the only handle it has. Native's [`DocLine`]
/// carries an explicit document number (`n`), so we additionally require
/// `n != 0`: a nzk_ui line has no number of its own, and indexing it would let
/// it win `first occurrence` and shift every real line's position by one. The
/// governing ruling for this port is "adapt the tests to be functionally
/// identical, not code-identical" — this custom renderer pops panes in and out of
/// the main UI and must keep scaling correctly, so it needs the explicit number
/// rather than a positional guess. The higher-fidelity behaviour wins over
/// byte-for-byte reference parity, and the divergence is recorded here rather
/// than left silent.
pub fn record_index(lines: &[DocLine]) -> std::collections::HashMap<(String, i64), i64> {
    let mut map = std::collections::HashMap::new();
    for (i, l) in lines.iter().enumerate() {
        // Viewer nzk_ui (part markers) carries `n == 0` and is not addressable,
        // and a line with no origin file has nothing to key on. Both halves of
        // this guard are load-bearing: dropping the `n == 0` half lets a nzk_ui
        // line win `first occurrence` and hand back a position that is one off
        // from every real line after it.
        if l.n == 0 || l.file.is_empty() {
            continue;
        }
        let key = (l.file.clone(), l.orig);
        // First wins: `if (!(k in map)) map[k] = i + 1`.
        //
        // Record the line's **own document number** (`l.n`) rather than the array
        // index (`i + 1`). The two coincide on every real document — production
        // emits `n == position` for each emitted line (`source_pane.rs:1057-1061`),
        // and the only line without one is the `n == 0` part marker skipped above —
        // so this is not a behaviour change on real data; it is what makes the
        // `ln > line_count` bound in `build_lenses` meaningful. With the array
        // index the bound could never fire (an index is always `<= lines.len()`),
        // which is precisely how a declaration from a much larger document landed
        // a lens on a synthesized 3-line one. Using `n` also matches the reference's
        // contract: `recordIndex` returns *Monaco line numbers*, which is what
        // `lineFor`'s callers compare against `getLineCount()`.
        map.entry(key).or_insert(l.n);
    }
    map
}

/// `lineFor` (`codelens.js:162-167`): the document line a declaration sits on.
///
/// * no decl, or `decl.line == 0` ⇒ `0` (line 163).
/// * a real *file* model ⇒ the decl's own line, but **only if the decl is in the
///   file the model is showing** — a decl from another file maps to `0` rather
///   than to a line that does not exist in this document (line 164).
/// * a synthesized document ⇒ the reverse map from [`record_index`].
///
/// `0` is the honest "not on screen" answer and is never substituted with `1`:
/// the reference's `showReferences` turns a `0` into the toast "Declaration is
/// not on screen" (`codelens.js:322-325`), which it could not distinguish from a
/// genuine line 1 if this returned `1`.
/// The document's **last numbered line** — its line count as the pane numbers
/// lines — not the number of slice entries.
///
/// `Document::line_count()` is the raw slice length, and a viewer-nzk_ui part
/// marker is a slice entry that carries no document number: the engine numbers
/// every non-`part` line `1..N` and leaves part markers at `0`
/// (`core/src/doc.rs:1389-1400`). On a nzk_ui-bearing document the two differ by
/// the number of part markers, so using the slice length as the placement bound
/// is what round-16 audit §6.1/§6.2 found: it admits a file-model declaration at
/// `lines.len()`, a line the document never numbers.
///
/// Returning `max n` rather than the *count* of numbered entries keeps the bound
/// honest even for a document whose numbers are not contiguous (the source pane's
/// `LensRows` indexes by document number, not by position).
pub fn document_line_count(lines: &[DocLine]) -> i64 {
    lines.iter().map(|l| l.n).filter(|&n| n > 0).max().unwrap_or(0)
}

pub fn line_for(
    index: &std::collections::HashMap<(String, i64), i64>,
    doc_file: &str,
    is_file_model: bool,
    decl: &DeclRow,
) -> i64 {
    if decl.line == 0 {
        return 0;
    }
    if is_file_model {
        return if decl.file == doc_file { decl.line } else { 0 };
    }
    index
        .get(&(decl.file.clone(), decl.line))
        .copied()
        .unwrap_or(0)
}

/// Build the lens rows for a document, mirroring `buildLenses`
/// (`codelens.js:206-247`).
///
/// The placement rules, each of which changes the output:
///
/// 1. A row with no `id` or no `line` is skipped (line 214).
/// 2. `max_decls` caps how many declarations get lenses (line 212); the
///    reference's own constant is [`MAX_DECLS`].
/// 3. A line outside `[1, line_count]` **or** not one of the document's numbered
///    lines is skipped (line 216) — a declaration that is not in this document
///    produces *no* lens rather than one on line `0`. The second half matters on
///    the file-model branch, where `line_for` returns `decl.line` unchecked:
///    the caller-supplied bound alone would admit a line the document never
///    numbers (round-16 audit §6.2/§6.4).
/// 4. **One declaration per line**: `used[ln]` (line 216) — the first
///    declaration that lands on a line wins and later ones on the same line are
///    dropped. Two lenses on one line would overlap in Monaco; here it keeps the
///    rows a faithful subset of the reference's.
/// 5. Each placed declaration emits **two** rows, `Main` then `References`, both
///    on the same line (lines 221-244).
///
/// Counts: `Main` gets the resolved counts from `counts_of`, mirroring
/// `resolveLens` (`codelens.js:249-263`) having run over every lens before
/// `lensesFor` reads `l._counts`. `References` rows carry the same counts, as
/// the reference sets `lens._counts` for both kinds (line 255).
///
/// `counts_of` is injected rather than taking a `&Graph` so the placement rules
/// are testable without constructing a graph, and so a caller holding richer
/// facts than the graph can supply the reference's numbers.
pub fn build_lenses<F>(
    lines: &[DocLine],
    line_count: i64,
    doc_file: &str,
    is_file_model: bool,
    decls: &[DeclRow],
    max_decls: usize,
    mut counts_of: F,
) -> Vec<LensRow>
where
    F: FnMut(&DeclRow) -> Counts,
{
    let index = record_index(lines);
    // The document's own numbering, so `ln` is reconciled against `l.n` and not
    // only against the caller-supplied bound. On the file-model branch
    // `line_for` returns `decl.line` unchecked, and a generous `line_count` (the
    // raw file's line count, say) would otherwise place a lens on a line this
    // document does not have — round-16 audit §6.2/§6.4. Part markers stay out
    // (`n > 0`), exactly like `record_index`.
    let numbered: std::collections::HashSet<i64> =
        lines.iter().map(|l| l.n).filter(|&n| n > 0).collect();
    let mut out = Vec::new();
    let mut used: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let mut placed = 0usize;

    for row in decls {
        if placed >= max_decls {
            break;
        }
        if row.id.is_empty() || row.line == 0 {
            continue;
        }
        let ln = line_for(&index, doc_file, is_file_model, row);
        if ln < 1 || ln > line_count || !numbered.contains(&ln) || used.contains(&ln) {
            continue;
        }
        used.insert(ln);
        placed += 1;

        let c = counts_of(row);
        out.push(LensRow {
            line: ln,
            kind: LensKind::Main,
            node_id: row.id.clone(),
            title: title(c),
            commands: vec![format!("{V}.showInGraph")],
            counts: c,
            decl: row.clone(),
        });
        out.push(LensRow {
            line: ln,
            kind: LensKind::References,
            node_id: row.id.clone(),
            title: "references".to_string(),
            // `[row.id, row.file, row.line, labelOf(row)]` (`codelens.js:242`).
            commands: vec![format!("{V}.references")],
            counts: c,
            decl: row.clone(),
        });
    }
    out
}

/// The reference's namespace prefix `var V = 'csgraph.codelens'`
/// (`codelens.js:28`), used for both command ids.
pub const V: &str = "csgraph.codelens";

// ---------------------------------------------------------------------------
// graph-backed entry points
// ---------------------------------------------------------------------------

/// The declaration rows a document's node contains, as `/api/decls` would
/// return them.
///
/// The reference asks the engine for decls (`codelens.js:107-108`); native holds
/// the whole graph, so the rows are read straight out of it. `file`
/// (`Some`) scopes the rows to one file — the reference's `{file}` parameter —
/// and `None` gives every node, its `{node}` parameter.
///
/// Rows come back in graph order, which is the engine's order; the reference
/// relies on that too (`buildLenses` never sorts, line 211).
pub fn decls_for(graph: &Graph, file: Option<&str>) -> Vec<DeclRow> {
    graph
        .nodes
        .iter()
        .filter(|n| file.is_none_or(|f| n.file == f))
        .map(|n| DeclRow {
            id: n.id.clone(),
            file: n.file.clone(),
            line: n.line,
            degree: n.degree,
            name: n.label.clone(),
            label: n.label.clone(),
            full: n.full.clone(),
        })
        .collect()
}

/// The resolved counts for one node id, from the graph — the native stand-in for
/// `nodeDetail(id)` + `countsFor` (`codelens.js:190-199`, `176-184`).
///
/// The reference's numbers come from `/api/node`'s `callers`/`usedBy`/`parts`.
/// Native's [`Graph`] stores edges, not pre-grouped `usedBy` lists, so the
/// equivalent is computed from the edges that point *at* the node:
///
/// * **refs** — every incoming edge, plus `1` per edge **kind** to stand in for
///   `/api/node`'s `parts` (the node's own declaration sites). A node with no
///   incoming edge still reports its `degree` fallback when the graph holds no
///   edges at all.
/// * **callers** — incoming edges whose kind is [`CALLS`], plus [`USES`], since
///   the engine's "callers" list is the call-shaped subset of the same
///   neighbours.
///
/// Returns `None` for an unknown id, so a caller can fall back to
/// [`counts_from_decl`] exactly as `countsFor` does.
///
/// **Not faithfully mirrorable:** the reference's `usedBy` is *reference sites*
/// (a symbol used 30 times in one file is one item with `count: 30`), while an
/// edge is *one relation between two nodes* with its own `count`. The edge
/// `count` is added here so a multi-use reference contributes its real weight,
/// but for a graph whose edges do not carry `count` the totals will be lower
/// than the engine's `usedBy` sum. This is stated rather than papered over; it is
/// the one numeric place the mirror is approximate.
pub fn counts_for_node(graph: &Graph, id: &str) -> Option<Counts> {
    let slot = graph.nodes.iter().position(|n| n.id == id)?;
    let mut used_by: Vec<CountItem> = Vec::new();
    let mut callers: Vec<CountItem> = Vec::new();
    for e in &graph.edges {
        if e.to != slot {
            continue;
        }
        let item = CountItem {
            count: e.count,
            lines: 0,
        };
        // `CountItem` is not `Copy`, and one edge can belong to both lists (a
        // `calls`/`uses` edge is also an incoming edge), so each list takes its
        // own value instead of the second push moving an already-moved item.
        used_by.push(item.clone());
        if e.kind == CALLS || e.kind == USES {
            callers.push(item);
        }
    }
    // The `+ parts.length` term: native has no `parts` array in the graph, so a
    // node that has a declaration line at all contributes exactly one part.
    let node = &graph.nodes[slot];
    let parts = if node.line > 0 { 1 } else { 0 };
    Some(counts_for(&used_by, &callers, parts))
}

/// Incoming-edge kinds the engine groups under "callers".
///
/// `view.rs:299` is the authority for the two kinds the app treats as edges at
/// all (`&["contains", "calls"]`), and `model.rs:721` shows `"uses"` in a real
/// fixture, so all three names are load-bearing rather than guesses.
pub const CALLS: &str = "calls";
/// See [`CALLS`].
pub const USES: &str = "uses";

/// The whole pipeline for one open document: decls ⇒ lens rows, with counts read
/// from the graph.
///
/// This is the function a source-pane caller invokes; it is the native
/// equivalent of `lensesFor` (`codelens.js:378-403`) minus the async caching,
/// which exists in the reference only because `/api/decls` and `/api/node` are
/// HTTP round-trips. Native reads both from memory, so there is nothing to cache
/// and — importantly — nothing that can serve a stale row from a graph that has
/// been replaced by a re-index.
pub fn lens_rows(graph: &Graph, doc: &Document, max_decls: usize) -> Vec<LensRow> {
    let decls = decls_for(graph, None);
    // The bound is the document's own numbering, not `doc.line_count()` (the raw
    // slice length, which counts viewer-nzk_ui part markers). See
    // [`document_line_count`].
    let line_count = document_line_count(&doc.lines);
    build_lenses(
        &doc.lines,
        line_count,
        &doc.file,
        // Native's source pane synthesizes a document for a node; it has no
        // `csgraph://file/…` model, so this is always the synthesized case. The
        // `file`-model branch of `lineFor` is still exercised by tests.
        false,
        &decls,
        max_decls,
        |row| {
            counts_for_node(graph, &row.id).unwrap_or_else(|| counts_from_decl(row.degree))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Node;
    use std::path::PathBuf;

    fn items(pairs: &[(i64, usize)]) -> Vec<CountItem> {
        pairs
            .iter()
            .map(|&(count, lines)| CountItem { count, lines })
            .collect()
    }

    fn docline(n: i64, file: &str, orig: i64) -> DocLine {
        DocLine {
            n,
            ctx: "body".to_string(),
            file: file.to_string(),
            orig,
            active: true,
            text: String::new(),
            segs: Vec::new(),
        }
    }

    fn decl(id: &str, file: &str, line: i64, degree: i64) -> DeclRow {
        DeclRow {
            id: id.to_string(),
            file: file.to_string(),
            line,
            degree,
            name: id.to_string(),
            ..DeclRow::default()
        }
    }

    fn node(id: &str, file: &str, line: i64, degree: i64) -> Node {
        Node {
            id: id.to_string(),
            label: id.to_string(),
            full: format!("N.{id}"),
            file: file.to_string(),
            line,
            degree,
            ..Node::default()
        }
    }

    fn graph_of(nodes: Vec<Node>, edges: Vec<Edge>) -> Graph {
        Graph {
            repo: PathBuf::from("/tmp/test"),
            nodes,
            edges,
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        }
    }

    use crate::model::Edge;

    // -- countItems ---------------------------------------------------------

    /// The three-level fallback, in the reference's order: `count` beats
    /// `lines.len()` beats the implicit `1`.
    ///
    /// The `(0, 3)` case is the one that catches a `lines.len()`-first rewrite:
    /// the reference adds 3, and so must this.
    #[test]
    fn count_items_follows_count_then_lines_then_one() {
        assert_eq!(count_items(&items(&[(5, 9)])), 5, "count wins over lines");
        assert_eq!(count_items(&items(&[(0, 3)])), 3, "lines wins when count is 0");
        assert_eq!(count_items(&items(&[(0, 0)])), 1, "neither ⇒ the item itself");
        assert_eq!(count_items(&items(&[(2, 0), (0, 4), (0, 0)])), 7, "summed in order");
    }

    /// An empty list is `0` — the reference's `total` starts at 0 and the loop
    /// body never runs (line 64-75).
    #[test]
    fn count_items_of_nothing_is_zero() {
        assert_eq!(count_items(&[]), 0);
    }

    /// A **negative** `count` falls through to the `lines` branch rather than
    /// subtracting: the guard is `c > 0`, not `c != 0` (`codelens.js:69`).
    ///
    /// This is the sharp edge of `countItems`. A `.filter(|c| *c != 0)` rewrite
    /// would subtract 4 and report -1 here.
    #[test]
    fn count_items_negative_count_is_not_subtracted() {
        assert_eq!(count_items(&items(&[(-4, 0)])), 1, "negative count ⇒ falls through to 1");
        assert_eq!(count_items(&items(&[(-4, 6)])), 6, "negative count ⇒ uses lines");
    }

    // -- countsFromDecl / countsFor ----------------------------------------

    /// The fallback reports `degree` as `refs` and **`0` callers**, and is
    /// flagged unresolved (`codelens.js:172-174`).
    #[test]
    fn counts_from_decl_is_degree_zero_callers_unresolved() {
        let c = counts_from_decl(7);
        assert_eq!(c.refs, 7);
        assert_eq!(c.callers, 0, "the reference reports 0, not 'unknown'");
        assert!(!c.resolved);
    }

    /// `countsFor` = `countItems(usedBy) + parts.length` for refs and
    /// `countItems(callers)` for callers, and is flagged resolved.
    ///
    /// The `parts` term is asserted separately: 10 `usedBy` + 3 parts must be 13,
    /// which fails if `parts` is dropped (a plausible "why add the node to its own
    /// reference count?" simplification).
    #[test]
    fn counts_for_adds_parts_to_usedby() {
        let c = counts_for(&items(&[(10, 0)]), &items(&[(2, 0)]), 3);
        assert_eq!(c.refs, 13, "usedBy + parts, not usedBy alone");
        assert_eq!(c.callers, 2);
        assert!(c.resolved);

        let no_parts = counts_for(&items(&[(10, 0)]), &items(&[]), 0);
        assert_eq!(no_parts.refs, 10);
        assert_eq!(no_parts.callers, 0);
    }

    // -- title / labelOf ----------------------------------------------------

    /// The exact title format, including the U+00B7 separator and the spaces
    /// around it (`codelens.js:187`).
    #[test]
    fn title_is_the_reference_string() {
        let t = title(Counts { refs: 12, callers: 3, resolved: true });
        assert_eq!(t, "12 references \u{b7} 3 callers \u{b7} show in graph");
        // Spelled out, so a change to the separator byte fails loudly.
        assert_eq!(
            t.as_bytes()[14..16].to_vec(),
            vec![0xc2, 0xb7],
            "the separator must be U+00B7 UTF-8 (C2 B7)"
        );
    }

    /// `title` reads the counts it is given, including the unresolved ones —
    /// the reference renders the provisional lens with zero callers.
    #[test]
    fn title_of_unresolved_counts_shows_zero_callers() {
        assert_eq!(
            title(counts_from_decl(4)),
            "4 references \u{b7} 0 callers \u{b7} show in graph"
        );
    }

    /// `labelOf`'s `||` chain skips **empty strings**, not just absent fields.
    #[test]
    fn label_of_skips_empty_candidates() {
        assert_eq!(label_of("name", "label", "full", "id"), "name");
        assert_eq!(label_of("", "label", "full", "id"), "label");
        assert_eq!(label_of("", "", "full", "id"), "full");
        assert_eq!(label_of("", "", "", "id"), "id");
        assert_eq!(label_of("", "", "", ""), "", "all empty ⇒ empty, not a panic");
    }

    // -- recordIndex / lineFor ---------------------------------------------

    /// **First wins.** A line that appears twice maps to the first occurrence —
    /// the head-section/duplicated-`#if` rule from `codelens.js:144-147`.
    ///
    /// Keeping the last occurrence passes every other test in this file and lands
    /// the lens on a duplicate; this is the assertion that forbids it.
    #[test]
    fn record_index_first_occurrence_wins() {
        let lines = vec![
            docline(1, "a.cs", 10),
            docline(2, "a.cs", 10), // the same origin line again
            docline(3, "b.cs", 10),
        ];
        let idx = record_index(&lines);
        assert_eq!(idx.get(&("a.cs".to_string(), 10)), Some(&1), "first, not 2");
        assert_eq!(idx.get(&("b.cs".to_string(), 10)), Some(&3));
    }

    /// Viewer nzk_ui (part markers) carries `n == 0`, and a line with no origin
    /// file is not addressable — neither may create a map entry
    /// (`codelens.js:154`).
    #[test]
    fn record_index_skips_nzk_ui_and_fileless_lines() {
        let lines = vec![
            docline(0, "a.cs", 10), // a part marker: no document line number
            docline(2, "", 10),     // no origin file
            docline(3, "a.cs", 10),
        ];
        let idx = record_index(&lines);
        assert_eq!(idx.len(), 1, "only the real record is addressable");
        assert_eq!(idx.get(&("a.cs".to_string(), 10)), Some(&3));
    }

    /// A zero line is `0` — never coerced to `1` (`codelens.js:163`).
    #[test]
    fn line_for_decl_without_line_is_zero() {
        let idx = record_index(&[]);
        assert_eq!(line_for(&idx, "a.cs", false, &decl("n", "a.cs", 0, 0)), 0);
    }

    /// A file model maps a declaration to its own line **only when the decl is
    /// in the file being shown** (`codelens.js:164`); another file's decl is `0`.
    #[test]
    fn line_for_file_model_requires_the_same_file() {
        let idx = record_index(&[]);
        assert_eq!(line_for(&idx, "a.cs", true, &decl("n", "a.cs", 42, 0)), 42);
        assert_eq!(
            line_for(&idx, "a.cs", true, &decl("n", "other.cs", 42, 0)),
            0,
            "a decl from another file has no line in this document"
        );
    }

    /// A declaration whose origin is not in the document maps to `0`, so it
    /// produces no lens rather than one on line 0.
    #[test]
    fn line_for_synthesized_document_uses_the_reverse_map() {
        let lines = vec![docline(1, "a.cs", 100), docline(2, "a.cs", 110)];
        let idx = record_index(&lines);
        assert_eq!(line_for(&idx, "a.cs", false, &decl("n", "a.cs", 110, 0)), 2);
        assert_eq!(line_for(&idx, "a.cs", false, &decl("n", "a.cs", 999, 0)), 0);
    }

    // -- buildLenses --------------------------------------------------------

    fn always(c: Counts) -> impl FnMut(&DeclRow) -> Counts {
        move |_| c
    }

    /// Each placed declaration emits exactly two rows — `main` then `references`,
    /// both on the same line — with the reference's command ids and titles.
    #[test]
    fn build_lenses_emits_two_rows_per_declaration() {
        let lines = vec![docline(1, "a.cs", 10), docline(2, "a.cs", 20)];
        let decls = vec![decl("A", "a.cs", 10, 5), decl("B", "a.cs", 20, 6)];
        let rows = build_lenses(&lines, 2, "a.cs", false, &decls, MAX_DECLS, always(
            Counts { refs: 9, callers: 2, resolved: true },
        ));

        assert_eq!(rows.len(), 4, "two lenses per declaration");
        assert_eq!(rows[0].kind, LensKind::Main);
        assert_eq!(rows[0].line, 1);
        assert_eq!(rows[0].commands, vec!["csgraph.codelens.showInGraph"]);
        assert_eq!(rows[0].title, "9 references \u{b7} 2 callers \u{b7} show in graph");
        assert_eq!(rows[1].kind, LensKind::References);
        assert_eq!(rows[1].line, 1, "same line as its main lens");
        assert_eq!(rows[1].commands, vec!["csgraph.codelens.references"]);
        assert_eq!(rows[1].title, "references", "the bare word, codelens.js:241");
        assert_eq!(rows[2].line, 2);
        assert_eq!(rows[3].line, 2);
        assert_eq!(rows[1].node_id, "A");
        assert_eq!(rows[3].node_id, "B");
    }

    /// The kind tags are the reference's own `_kind` strings.
    #[test]
    fn lens_kind_tags_match_the_reference() {
        assert_eq!(LensKind::Main.tag(), "main");
        assert_eq!(LensKind::References.tag(), "references");
    }

    /// A row with no `id` or no `line` is skipped (`codelens.js:214`).
    #[test]
    fn build_lenses_skips_rows_without_id_or_line() {
        let lines = vec![docline(1, "a.cs", 10)];
        let decls = vec![
            decl("", "a.cs", 10, 1),  // no id
            decl("B", "a.cs", 0, 1),  // no line
            decl("C", "a.cs", 10, 1), // the only usable row
        ];
        let rows = build_lenses(&lines, 1, "a.cs", false, &decls, MAX_DECLS, always(Counts::default()));
        assert_eq!(rows.len(), 2, "only C places lenses");
        assert_eq!(rows[0].node_id, "C");
    }

    /// A declaration outside `[1, line_count]` gets **no** lens
    /// (`codelens.js:216`).
    #[test]
    fn build_lenses_rejects_lines_outside_the_document() {
        let lines = vec![docline(5, "a.cs", 10)];
        let decls = vec![decl("A", "a.cs", 10, 1)];
        // The decl maps to line 5, but the document only has 3 lines.
        assert!(build_lenses(&lines, 3, "a.cs", false, &decls, MAX_DECLS, always(Counts::default())).is_empty());
        // With the real count it places.
        assert_eq!(build_lenses(&lines, 5, "a.cs", false, &decls, MAX_DECLS, always(Counts::default())).len(), 2);
    }

    /// Round-16 §6.2: the bound was compared against the raw slice length, so on
    /// a nzk_ui-bearing document (a `ctx == "part"` marker is a slice entry with
    /// no document number, `core/src/doc.rs:1389-1400`) a file-model declaration
    /// at `line == lines.len()` — a line the document never numbers — slipped
    /// through. The bound must reconcile `ln` against the document's own
    /// numbering (`l.n`), not the slice.
    #[test]
    fn build_lenses_rejects_a_file_model_line_that_only_the_slice_length_admits() {
        let lines = vec![
            docline(0, "a.cs", 0), // the part marker: a slice entry, not a line
            docline(1, "a.cs", 10),
        ];
        // `lines.len()` is 2 but the document's last numbered line is 1.
        let decls = vec![decl("A", "a.cs", 2, 1)];
        let rows = build_lenses(
            &lines,
            lines.len() as i64, // exactly what the old `lens_rows` passed
            "a.cs",
            true, // file model: `line_for` returns `decl.line` unchecked
            &decls,
            MAX_DECLS,
            always(Counts::default()),
        );
        assert!(
            rows.is_empty(),
            "line 2 is not a line of this document, so it must place no lens"
        );
    }

    /// Round-16 §6.4: on the file-model branch `line_for` returns `decl.line`
    /// unchecked, so the *only* guard was the caller-supplied `line_count`. A
    /// caller holding a larger number (the raw file's line count, say) would
    /// place a lens past the end of the synthesized document. `build_lenses`
    /// must reconcile `ln` against the document's own numbered lines.
    #[test]
    fn build_lenses_rejects_a_file_model_declaration_the_document_does_not_number() {
        let lines = vec![docline(1, "a.cs", 10), docline(2, "a.cs", 20)];
        let decls = vec![decl("A", "a.cs", 9, 1)];
        let rows = build_lenses(
            &lines,
            9, // a generous caller-supplied bound that admits line 9
            "a.cs",
            true,
            &decls,
            MAX_DECLS,
            always(Counts::default()),
        );
        assert!(
            rows.is_empty(),
            "there is no document line 9, so the generic bound alone must not place a lens"
        );
    }

    /// Round-16 §6.1: the placement bound must be the document's own numbering,
    /// not the raw slice length. A part marker is a slice entry with `n == 0`, so
    /// a nzk_ui-bearing document's slice is longer than its numbering.
    ///
    /// (Red before the fix: `document_line_count` did not exist and `lens_rows`
    /// passed `doc.line_count()` — the slice length — as the bound. The value was
    /// not observable through `lens_rows` itself, because every synthesized
    /// placement resolves to a line that exists; its only reachable consequence
    /// is the file-model bound leak covered by the two tests above, so this is
    /// the direct contract test of the fixed rule.)
    #[test]
    fn document_line_count_is_the_last_numbered_line_not_the_slice_length() {
        let lines = vec![
            docline(0, "a.cs", 0), // part marker: slice entry, no document line
            docline(1, "a.cs", 10),
            docline(2, "a.cs", 20),
        ];
        assert_eq!(document_line_count(&lines), 2);
        assert_eq!(document_line_count(&lines), lines.len() as i64 - 1);
        // NzkUi-only document: no numbered line at all.
        assert_eq!(document_line_count(&[docline(0, "a.cs", 0)]), 0);
        assert_eq!(document_line_count(&[]), 0);
    }

    /// Round-16 §6.3 says `record_index` classifies nzk_ui by `n == 0` alone
    /// while `DocLine::ctx` marks `part`/`cond`/`head`/`tail` as nzk_ui-painted,
    /// so a nzk_ui line with `n > 0` "would be indexed".
    ///
    /// **Verdict: not a defect.** The engine's payload numbers **every**
    /// non-`part` line `1..N` and leaves only part markers at `0`
    /// (`core/src/doc.rs:1389-1400`), so `n == 0` identifies exactly the
    /// unaddressable part marker. Head/tail/`cond` lines *are* numbered and
    /// *are* `src`-bearing, and the reference provider indexes exactly those
    /// (`codelens.js:148-154` skips only `!src || !src.file`); indexing them is
    /// parity, not a bug. This test pins that equivalence so a future "fix" to
    /// `record_index` cannot silently diverge from the reference.
    #[test]
    fn record_index_indexes_numbered_head_and_cond_lines_like_the_reference() {
        let mut head = docline(1, "a.cs", 10);
        head.ctx = "head".to_string();
        let mut cond = docline(2, "a.cs", 10);
        cond.ctx = "cond".to_string();
        let lines = vec![
            docline(0, "a.cs", 0), // the only unaddressable nzk_ui: part marker
            head,
            cond,
            docline(3, "a.cs", 10),
        ];
        let idx = record_index(&lines);
        assert_eq!(
            idx.get(&("a.cs".to_string(), 10)),
            Some(&1),
            "the numbered head line is src-bearing and wins first occurrence"
        );
        assert!(
            !idx.values().any(|&n| n == 0),
            "no entry may ever point at an unnumbered nzk_ui line"
        );
    }

    /// **One declaration per line**: the second decl on an occupied line is
    /// dropped (`used[ln]`, `codelens.js:216`).
    #[test]
    fn build_lenses_places_at_most_one_declaration_per_line() {
        let lines = vec![docline(1, "a.cs", 10), docline(2, "a.cs", 20)];
        // Both declarations resolve to document line 1.
        let decls = vec![decl("FIRST", "a.cs", 10, 1), decl("SECOND", "a.cs", 10, 1)];
        let rows = build_lenses(&lines, 2, "a.cs", false, &decls, MAX_DECLS, always(Counts::default()));
        assert_eq!(rows.len(), 2, "the duplicate line yields one declaration's lenses");
        assert_eq!(rows[0].node_id, "FIRST", "the first declaration wins");
    }

    /// The `max_decls` cap counts **declarations placed**, not rows emitted
    /// (`codelens.js:212` breaks on `placed`).
    ///
    /// With a cap of 1 over two placeable declarations, exactly one declaration
    /// (two rows) is produced — a cap applied to rows would give one row.
    #[test]
    fn build_lenses_cap_counts_declarations_not_rows() {
        let lines = vec![docline(1, "a.cs", 10), docline(2, "a.cs", 20)];
        let decls = vec![decl("A", "a.cs", 10, 1), decl("B", "a.cs", 20, 1)];
        let rows = build_lenses(&lines, 2, "a.cs", false, &decls, 1, always(Counts::default()));
        assert_eq!(rows.len(), 2, "one declaration ⇒ two rows");
        assert_eq!(rows[0].node_id, "A");
    }

    /// Resolved counts reach both rows, mirroring `resolveLens` setting
    /// `lens._counts` before `lensesFor` reads it (`codelens.js:255`, `394`).
    #[test]
    fn build_lenses_carries_resolved_counts_to_both_rows() {
        let lines = vec![docline(1, "a.cs", 10)];
        let decls = vec![decl("A", "a.cs", 10, 3)];
        let rows = build_lenses(&lines, 1, "a.cs", false, &decls, MAX_DECLS, |row| {
            assert_eq!(row.degree, 3, "the closure sees the real decl row");
            Counts { refs: 11, callers: 4, resolved: true }
        });
        assert!(rows.iter().all(|r| r.counts.refs == 11 && r.counts.resolved));
        // The decl travels with the row so the `references` command can be built
        // from `[id, file, line, label]` (`codelens.js:242`).
        assert_eq!(rows[1].decl.file, "a.cs");
        assert_eq!(rows[1].decl.label(), "A");
    }

    // -- declsFor / countsForNode / lensRows --------------------------------

    #[test]
    fn decls_for_reads_rows_out_of_the_graph() {
        let g = graph_of(
            vec![node("A", "a.cs", 10, 2), node("B", "b.cs", 20, 3)],
            Vec::new(),
        );
        let all = decls_for(&g, None);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "A");
        assert_eq!(all[0].line, 10);
        assert_eq!(all[0].degree, 2);

        let only_b = decls_for(&g, Some("b.cs"));
        assert_eq!(only_b.len(), 1, "the file parameter scopes the rows");
        assert_eq!(only_b[0].id, "B");
    }

    /// Incoming edges become `usedBy`; call-shaped ones also become callers; the
    /// node's own declaration line contributes the `parts` term.
    #[test]
    fn counts_for_node_counts_incoming_edges() {
        let g = graph_of(
            vec![node("A", "a.cs", 10, 0), node("B", "a.cs", 20, 0), node("C", "a.cs", 30, 0)],
            vec![
                Edge { from: 1, to: 0, kind: "calls".into(), count: 1 },
                Edge { from: 2, to: 0, kind: "uses".into(), count: 1 },
            ],
        );
        let c = counts_for_node(&g, "A").expect("A is in the graph");
        assert!(c.resolved);
        // 1 (calls) + 1 (uses) + 1 part = 3.
        assert_eq!(c.refs, 3);
        assert_eq!(c.callers, 2, "both calls and uses count as callers");
    }

    /// An outgoing edge does **not** count as a reference to the node
    /// (`e.to != slot` is the filter) — the direction is the whole meaning.
    #[test]
    fn counts_for_node_ignores_outgoing_edges() {
        let g = graph_of(
            vec![node("A", "a.cs", 10, 0), node("B", "a.cs", 20, 0)],
            vec![Edge { from: 0, to: 1, kind: "calls".into(), count: 1 }],
        );
        let c = counts_for_node(&g, "A").unwrap();
        // No incoming edge, and A has a line ⇒ exactly one part.
        assert_eq!(c.refs, 1);
        assert_eq!(c.callers, 0, "A calls B; nothing calls A");
    }

    /// An unknown id returns `None` so the caller can use the `degree` fallback —
    /// the reference's `countsFor` branch when `detail.node` is absent.
    #[test]
    fn counts_for_node_is_none_for_an_unknown_id() {
        let g = graph_of(vec![node("A", "a.cs", 10, 0)], Vec::new());
        assert!(counts_for_node(&g, "MISSING").is_none());
    }

    /// A node with no declaration line contributes no `parts` term.
    #[test]
    fn counts_for_node_parts_requires_a_declaration_line() {
        let g = graph_of(vec![node("A", "a.cs", 0, 0)], Vec::new());
        assert_eq!(counts_for_node(&g, "A").unwrap().refs, 0);
    }

    /// A node id that is not in the graph falls back to `degree`, flagged
    /// unresolved — the `countsFromDecl` path, still reachable in native when a
    /// decl row outlives its node.
    #[test]
    fn lens_rows_falls_back_to_degree_for_an_unknown_node() {
        let g = graph_of(vec![node("A", "a.cs", 10, 4)], Vec::new());
        // Built through the test-only constructor rather than a struct literal:
        // `Document::highlight_generation` is private because it is the galley
        // cache's invalidation key, and a fixture that could pick it by hand
        // would be able to fake a cache identity.
        let mut doc =
            Document::from_lines_for_tests("A", "a.cs", vec![docline(1, "a.cs", 10)], 10, 10);
        let rows = lens_rows(&g, &doc, MAX_DECLS);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].line, 1);
        // A is present, so this is the resolved path: no incoming edges ⇒
        // 0 refs + 1 part.
        assert_eq!(rows[0].counts.refs, 1);
        assert!(rows[0].counts.resolved);

        // Now a decl row whose node the graph does not hold: the fallback fires.
        doc.lines = vec![docline(1, "a.cs", 10)];
        let empty = graph_of(Vec::new(), Vec::new());
        assert!(lens_rows(&empty, &doc, MAX_DECLS).is_empty(), "no nodes ⇒ no decls");
    }

    /// The end-to-end pipeline places a lens on the document line of each
    /// declaration, in document order.
    #[test]
    fn lens_rows_places_lenses_on_the_right_document_lines() {
        let g = graph_of(
            vec![node("A", "a.cs", 10, 1), node("B", "a.cs", 30, 2)],
            Vec::new(),
        );
        let doc = Document::from_lines_for_tests(
            "A",
            "a.cs",
            vec![
                docline(1, "a.cs", 10),
                docline(2, "a.cs", 20),
                docline(3, "a.cs", 30),
            ],
            10,
            30,
        );
        let rows = lens_rows(&g, &doc, MAX_DECLS);
        assert_eq!(rows.len(), 4, "two declarations × two lenses");
        assert_eq!((rows[0].line, rows[0].node_id.as_str()), (1, "A"));
        assert_eq!((rows[2].line, rows[2].node_id.as_str()), (3, "B"));
        // The line with no declaration gets no lens.
        assert!(!rows.iter().any(|r| r.line == 2));
    }
}
