//! The keyboard quick-open palette's model — display-free by construction.
//!
//! This is the engine behind `frontend/static/quickopen.js` (534 lines), the
//! Ctrl/Cmd+P "files & symbols" and Ctrl/Cmd+T "symbols" overlay. Like
//! [`crate::search`] and [`crate::picker`], nothing here mentions `egui`, a
//! window or a GPU: the model answers *"what rows does this query produce, which
//! one is selected, and what does Enter do?"* and the host paints the answer into
//! the `#cs-quickopen` overlay. Every operation is therefore unit-testable with
//! no display.
//!
//! ### What is mirrored, and what is deliberately not
//!
//! `quickopen.js` is a *client* of `/api/search` and `/api/find`; it owns no
//! matching of its own (its `composeSearch`, lines 206-259, is a merge/dedupe
//! over two HTTP responses). The native fork has no HTTP server, so the two
//! halves are separated rather than conflated:
//!
//! 1. **[`compose`]** is `composeSearch` verbatim — the same dedupe key, the
//!    same usage cap, the same `MAX_ROWS`, the same three-way file-hit
//!    preference, and the same *order of concatenation*. It consumes candidate
//!    rows that the caller already fetched, so it is a pure function of its
//!    inputs and can be tested without a graph or a socket.
//! 2. **[`rank`]** is the piece the reference never had in the palette: the
//!    `/api/search` ordering lived server-side in `core/src/api.rs`. The palette
//!    is the viewer's own implementation (quickopen.js:17-18), and the native
//!    fork must order its rows locally. [`rank`] supplies that, reusing
//!    [`crate::search`]'s **measured** rule (a hit on a symbol's own name beats a
//!    hit on the context it merely lives in) rather than inventing a second,
//!    differently-tuned fuzzy scorer.
//!
//! Keeping (1) exact is what makes the palette *faithful*; adding (2) locally is
//! what makes it *work* without the incumbent's server. They are separate
//! functions so a future reader can tell which is which.
//!
//! ### Fuzzy path matching
//!
//! Both layers are needed, because the reference judged two different things:
//!
//! * [`subsequence`] is the strict *in-order, gaps allowed* test, scoring
//!   consecutive runs and word-boundary starts (the `/` and `.` in a path, the
//!   humps in a CamelCase symbol). This is what makes `mfin` find
//!   `model/find_wire.rs` and `fnd` find `find`, i.e. the behaviour a quick-open
//!   user expects from a subsequence box.
//! * [`file_hit`] is `composeSearch`'s own test — a plain case-insensitive
//!   `indexOf` of the raw query against `row.file` (quickopen.js:245, 252) — kept
//!   separate and *character-for-character the same* because it drives the
//!   file-rows-first reordering, and that reordering is observable.
//!
//! A row can be a file hit without being a subsequence hit and vice versa, and
//! the reference reorders on the former only. Conflating them would change which
//! rows appear in which half of the list.
//!
//! ### Determinism
//!
//! Ordering never depends on map iteration or on hash order. [`dedupe`] walks the
//! input in order and keeps first occurrences; the sort in [`rank`] ends on the
//! row's `id` (unique within a composed list), so the order is total and
//! byte-identical across repeated calls on the same input. The tie-break chain is
//! spelled out on [`rank`].

#![allow(dead_code)]

use crate::search::{self, MatchKind};
use crate::model::Graph;

/// `MAX_ROWS = 60` — quickopen.js:23. The list is capped *after* the merge, so
/// the cap is observable and belongs in the core rather than in the renderer.
pub const MAX_ROWS: usize = 60;

/// `DEBOUNCE = 140` — quickopen.js:24. The host owns the timer; the value is
/// named here so the host cannot invent a different one, and so a test can
/// assert the constant the reference shipped.
pub const DEBOUNCE_MS: u64 = 140;

/// `RECENT_MAX = 8` — quickopen.js:25. The number of `CsNav` history entries
/// [`recent`] turns into seed rows.
pub const RECENT_MAX: usize = 8;

/// The `limit` the palette asks `/api/search` for, per mode — quickopen.js:208.
pub fn search_limit(want_symbols: bool) -> usize {
    if want_symbols {
        50
    } else {
        80
    }
}

/// The `limit` the palette asks `/api/find` for — quickopen.js:212. Only ever
/// used in symbols mode.
pub const FIND_LIMIT: usize = 50;

/// The usage rows `composeSearch` keeps — quickopen.js:230, `.slice(0, 20)`.
pub const USAGE_MAX: usize = 20;

/// Which palette is showing: `Ctrl+P` files & symbols, or `Ctrl+T` symbols.
///
/// `mode` is a two-valued string in the reference (quickopen.js:32) and every
/// consumer branches on `mode === 'symbols'`. Modelling it as an enum makes the
/// two `MODE_LABEL[]`/`MODE_PLACEHOLDER[]` lookups (quickopen.js:39-43)
/// exhaustive, so a third mode cannot be added without the compiler demanding
/// its label and placeholder.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Hash)]
pub enum Mode {
    #[default]
    Files,
    Symbols,
}

impl Mode {
    /// Parse `'symbols'`; everything else is `files`.
    ///
    /// This is the reference's own coercion — `(m === 'symbols') ? 'symbols' :
    /// 'files'` (quickopen.js:429, 453) — so an unknown mode string cannot
    /// produce a third state.
    pub fn parse(m: &str) -> Mode {
        if m == "symbols" {
            Mode::Symbols
        } else {
            Mode::Files
        }
    }

    /// `MODE_LABEL` — quickopen.js:39. Rendered into `.cs-qo-mode`.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Files => "Files & symbols",
            Mode::Symbols => "Symbols",
        }
    }

    /// `MODE_PLACEHOLDER` — quickopen.js:40-43. Rendered as the input's
    /// `placeholder`.
    pub fn placeholder(self) -> &'static str {
        match self {
            Mode::Files => "Type a file or symbol name\u{2026}",
            Mode::Symbols => "Type a symbol name\u{2026}",
        }
    }

    /// `mode === 'symbols'` — the flag that switches on the `/api/find` merge
    /// (quickopen.js:211) and the file-hit reordering (quickopen.js:242/248).
    pub fn want_symbols(self) -> bool {
        self == Mode::Symbols
    }
}

/// Where a row came from — the reference's `source` field (quickopen.js:190).
///
/// The variant order is the ranking's dominant key in [`rank`]: rows the
/// incumbent put *first* (`/api/find`'s definitions and implementations, which
/// `composeSearch` prepends at quickopen.js:224-229) must stay ahead of generic
/// `/api/search` hits, exactly as the concatenation does.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash, Default)]
pub enum Source {
    /// `find.definitions[]` — quickopen.js:224-226.
    Definition,
    /// `find.implementations[]` — quickopen.js:227-229.
    Implementation,
    /// `find.usages[]` — quickopen.js:230-238. Capped by [`USAGE_MAX`].
    Usage,
    /// `/api/search` results — quickopen.js:219. The default.
    #[default]
    Search,
    /// `CsNav` history seeds — quickopen.js:394-411.
    Recent,
}

impl Source {
    /// The lowercase tag the reference puts in `source` (quickopen.js:190).
    pub fn tag(self) -> &'static str {
        match self {
            Source::Definition => "definition",
            Source::Implementation => "implementation",
            Source::Usage => "usage",
            Source::Search => "search",
            Source::Recent => "recent",
        }
    }
}

/// One palette row — the reference's row object (quickopen.js:185-191).
///
/// Owns its strings (unlike [`crate::search::SearchResult`], which borrows the
/// graph) because `composeSearch` genuinely *builds new objects*: it copies
/// fields out of two different API payloads into one shape. The allocation is
/// therefore the reference's own, not an artefact of this port.
///
/// `label`/`name`/`full` replicate the reference's fallback chains exactly,
/// because they are what the row *displays*: `fromNode` computes
/// `label: n.label || n.full || n.name || id` (quickopen.js:188) and
/// `name: n.name || n.label || n.full || id` (quickopen.js:187) — note the
/// **different order** of the same three fields, which is preserved here.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Row {
    /// `n.id || ''`. The click target; empty is legal (a file-only row).
    pub id: String,
    /// `n.kind || n.label || ''` — the uppercase chip.
    pub kind: String,
    /// `n.name || n.label || n.full || id`.
    pub name: String,
    /// `n.full || ''`.
    pub full: String,
    /// `n.label || n.full || n.name || id`.
    pub label: String,
    /// `n.file || ''`.
    pub file: String,
    /// `n.line || 0`.
    pub line: i64,
    pub source: Source,
}

impl Row {
    /// `fromNode(n, source)` — quickopen.js:181-192.
    ///
    /// Returns `None` for a row the reference would drop: a null node, or one
    /// with **neither** an id nor a file (quickopen.js:183: `if (!id && !n.file)
    /// return null`). That guard is the reason the palette never renders a
    /// nameless blank row.
    pub fn from_node(
        id: &str,
        kind: &str,
        name: &str,
        full: &str,
        label: &str,
        file: &str,
        line: i64,
        source: Source,
    ) -> Option<Row> {
        if id.is_empty() && file.is_empty() {
            return None;
        }
        Some(Row {
            id: id.to_string(),
            // `n.kind || n.label || ''` (quickopen.js:186).
            kind: first_of(&[kind, label]).to_string(),
            // `n.name || n.label || n.full || id` (quickopen.js:187).
            name: first_of(&[name, label, full, id]).to_string(),
            full: full.to_string(),
            // `n.label || n.full || n.name || id` (quickopen.js:188).
            label: first_of(&[label, full, name, id]).to_string(),
            file: file.to_string(),
            line,
            source,
        })
    }

    /// A `/api/search` row — `fromNode(n, 'search')` (quickopen.js:219).
    pub fn from_search_node(n: &crate::model::Node) -> Option<Row> {
        Row::from_node(
            &n.id,
            &n.kind,
            &n.label,
            &n.full,
            &n.label,
            &n.file,
            n.line,
            Source::Search,
        )
    }

    /// A usage row — quickopen.js:232-237.
    ///
    /// The reference reads the *nested* `u.node` for id/name/full/label but the
    /// *outer* `u.file`/`u.line` for the location, and falls back to `u.text`
    /// when the node has no name. Both are preserved; the outer/inner split is
    /// real (a usage's location is the site, not the definition).
    pub fn from_usage(u_node_id: &str, u_node_name: &str, u_node_label: &str, u_node_full: &str, u_text: &str, u_file: &str, u_line: i64) -> Row {
        Row {
            id: u_node_id.to_string(),
            // Forced to `usage` regardless of the node's own kind
            // (quickopen.js:233).
            kind: "usage".to_string(),
            name: first_of(&[u_node_name, u_node_label, u_text]).to_string(),
            full: u_node_full.to_string(),
            label: first_of(&[u_node_label, u_node_full, u_text]).to_string(),
            file: u_file.to_string(),
            line: u_line,
            source: Source::Usage,
        }
    }

    /// `kindOf(r)` — quickopen.js:299-303. Falls back to `usage` for a usage row
    /// whose `kind` is empty, then to `""` (the chip renders blank rather than
    /// inventing the word "node").
    pub fn kind_of(&self) -> &str {
        if !self.kind.is_empty() {
            return &self.kind;
        }
        if self.source == Source::Usage {
            return "usage";
        }
        ""
    }

    /// `name.textContent = r.label || r.name || r.id || r.file || ''`
    /// (quickopen.js:331) — the row's main text.
    pub fn display_label(&self) -> &str {
        first_of(&[&self.label, &self.name, &self.id, &self.file])
    }

    /// `loc.textContent = r.file ? (r.file + (r.line ? ':' + r.line : '')) :
    /// (r.id ? r.id : '')` (quickopen.js:341).
    ///
    /// Note both conditions are truthiness tests on the *original* values: a
    /// `line` of `0` renders as no suffix at all, it does not render `:0`.
    pub fn loc(&self) -> String {
        if !self.file.is_empty() {
            if self.line != 0 {
                return format!("{}:{}", self.file, self.line);
            }
            return self.file.clone();
        }
        self.id.clone()
    }

    /// `row.title` — quickopen.js:343-344: the full name, then `  —  ` and the
    /// location when there is a file. The spacing is the reference's (two
    /// spaces, an em dash, two spaces).
    pub fn title(&self) -> String {
        let head = first_of(&[&self.full, &self.label, &self.name]);
        if self.file.is_empty() {
            return head.to_string();
        }
        let mut s = String::with_capacity(head.len() + self.file.len() + 8);
        s.push_str(head);
        s.push_str("  \u{2014}  ");
        s.push_str(&self.file);
        if self.line != 0 {
            s.push(':');
            s.push_str(&self.line.to_string());
        }
        s
    }

    /// Does the row's *own name* contain the query? The `name` half of
    /// [`subsequence`]'s input.
    fn name_field(&self) -> &str {
        self.display_label()
    }

    /// The dedupe key — quickopen.js:198:
    /// `r.id ? ('id:' + r.id) : ('loc:' + r.file + ':' + r.line)`.
    pub fn dedupe_key(&self) -> String {
        if !self.id.is_empty() {
            return format!("id:{}", self.id);
        }
        format!("loc:{}:{}", self.file, self.line)
    }
}

/// First non-empty string in `cands`, else `""` — the `a || b || c` chain the
/// reference uses for its display fields.
fn first_of<'a>(cands: &[&'a str]) -> &'a str {
    for c in cands {
        if !c.is_empty() {
            return c;
        }
    }
    ""
}

/// `dedupe(list)` — quickopen.js:194-204.
///
/// Keeps the **first** occurrence of each key, in input order. The reference
/// uses a plain object for `seen` (insertion-ordered for string keys), so first
/// -wins and input order are both part of the observable behaviour: the merged
/// list puts `/api/find` rows before `/api/search` rows, and dedupe is what
/// decides which copy of a duplicated symbol survives.
pub fn dedupe(rows: Vec<Row>) -> Vec<Row> {
    let mut seen: Vec<String> = Vec::with_capacity(rows.len());
    let mut out: Vec<Row> = Vec::with_capacity(rows.len());
    for r in rows {
        let key = r.dedupe_key();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(r);
    }
    out
}

/// `q.indexOf('/') >= 0` — quickopen.js:242. A query with a slash is treated as
/// a path, which switches the composition from "prefer, don't hide" to
/// "filter, unless that empties the list".
pub fn looks_like_path(q: &str) -> bool {
    q.contains('/')
}

/// The reference's file test — quickopen.js:245/252:
/// `r.file && r.file.toLowerCase().indexOf(q.toLowerCase()) >= 0`.
///
/// A plain case-insensitive substring test, deliberately *not* the fuzzy
/// subsequence: this is the predicate that decides the file-rows-first
/// reordering, so it must stay character-for-character the reference's.
pub fn file_hit(row: &Row, q: &str) -> bool {
    if row.file.is_empty() {
        return false;
    }
    row.file
        .to_lowercase()
        .contains(&q.to_lowercase())
}
// NOTE: the predicate above reads `row.file` and nothing else. The earlier shape
// OR-ed in a hit on the row's *name*, which put a symbol whose name happens to
// contain the query into the file-rows half of `compose` — not what
// quickopen.js:245 does, and observable as a reordering.

/// The rows `composeSearch` returns for already-fetched candidates.
///
/// `search` is the `/api/search` payload (already unwrapped from its
/// `{ok, results:[…]}` envelope, as quickopen.js:217-218 does); `find` is the
/// optional `/api/find` payload, present only in symbols mode.
///
/// Mirrors quickopen.js:214-258 step for step:
///
/// 1. `/api/find`'s `definitions`, then `implementations`, then the first
///    [`USAGE_MAX`] `usages` are pushed **before** the search rows (224-229,
///    230-238).
/// 2. `dedupe(out.concat(searchRows))` (241) — find rows win ties.
/// 3. In files mode only: a slash query keeps just the file hits, *but only if
///    that set is non-empty* (242-247); otherwise file hits are moved to the
///    front with the symbol hits kept behind them (248-256).
/// 4. `merged.slice(0, MAX_ROWS)` (257).
///
/// Step 3's "unless it empties the list" is the subtle one: the reference will
/// happily show symbol rows for a slash query when no path matches, rather than
/// showing nothing. Both branches are preserved.
pub fn compose(search: Vec<Row>, find: Option<FindRows>, q: &str, want_symbols: bool) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::new();
    if want_symbols {
        if let Some(f) = find {
            for r in f.definitions {
                out.push(r);
            }
            for r in f.implementations {
                out.push(r);
            }
            for r in f.usages.into_iter().take(USAGE_MAX) {
                out.push(r);
            }
        }
    }
    let mut merged = dedupe([out, search].concat());

    if !want_symbols && looks_like_path(q) {
        // quickopen.js:244-247 — a path query keeps only rows whose file
        // matches, *unless* that leaves nothing.
        let file_hits: Vec<Row> = merged.iter().filter(|r| file_hit(r, q)).cloned().collect();
        if !file_hits.is_empty() {
            merged = file_hits;
        }
    } else if !want_symbols {
        // quickopen.js:249-255 — prefer file hits without hiding symbol hits.
        let mut out_rows: Vec<Row> = Vec::new();
        let mut file_rows: Vec<Row> = Vec::new();
        for r in merged {
            if file_hit(&r, q) {
                file_rows.push(r);
            } else {
                out_rows.push(r);
            }
        }
        merged = [file_rows, out_rows].concat();
    }
    merged.truncate(MAX_ROWS);
    merged
}

/// The `/api/find` payload's three lists, already converted to [`Row`]s.
///
/// A struct rather than a tuple so the three fields cannot be transposed at a
/// call site — `definitions`, `implementations` and `usages` are merged in that
/// order and a swap would be a silent reordering.
#[derive(Clone, Debug, Default)]
pub struct FindRows {
    pub definitions: Vec<Row>,
    pub implementations: Vec<Row>,
    pub usages: Vec<Row>,
}

/// `recentRows()` — quickopen.js:394-411, the seed rows an empty palette shows.
///
/// `hist` is `CsNav.history()` in the reference; the host passes whatever it
/// really has. Two details are preserved: the walk is **backwards** from the
/// newest entry (`for (i = hist.length - 1; i >= 0; i--)`), and a null entry is
/// skipped **without** consuming a slot (`if (!e) continue` precedes the
/// `out.length < RECENT_MAX` test in the loop condition, so the cap counts rows
/// produced, not entries visited).
///
/// `kind` is `'node'` for a document-mode entry and `'file'` otherwise
/// (quickopen.js:403). `full` is always empty; `file` is the remembered path.
pub fn recent(hist: &[NavEntry]) -> Vec<Row> {
    let mut out = Vec::with_capacity(RECENT_MAX);
    for e in hist.iter().rev() {
        if out.len() >= RECENT_MAX {
            break;
        }
        out.push(Row {
            id: e.node_id.clone(),
            kind: if e.mode == "doc" { "node" } else { "file" }.to_string(),
            name: first_of(&[&e.label, &e.node_id, &e.file]).to_string(),
            label: first_of(&[&e.label, &e.node_id, &e.file]).to_string(),
            full: String::new(),
            file: e.file.clone(),
            line: e.line,
            source: Source::Recent,
        });
    }
    out
}

/// One `CsNav` history entry (nav.js), as [`recent`] needs it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct NavEntry {
    pub node_id: String,
    pub file: String,
    pub line: i64,
    pub label: String,
    /// `'doc'` selects the `node` kind; anything else is a `file`.
    pub mode: String,
}

/// Seed the palette — `seed(q)` (quickopen.js:413-421).
pub fn seed(q: &str, hist: &[NavEntry]) -> Seed {
    if !q.is_empty() {
        return Seed::Query(q.to_string());
    }
    let recents = recent(hist);
    if recents.is_empty() {
        Seed::EmptyRecents
    } else {
        Seed::Recents(recents)
    }
}

/// What [`seed`] decided the initial list is.
///
/// The reference distinguishes three outcomes that all look like "an empty
/// list" if flattened into `Vec<Row>`: a query to run, recents to show, and
/// *no* query and *no* recents. The third is why `seedNote` exists — the
/// reference sets an empty note there (quickopen.js:420) while the other two get
/// real text, so the note bar's visibility must not be derivable from "rows are
/// empty".
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Seed {
    Query(String),
    Recents(Vec<Row>),
    EmptyRecents,
}

impl Seed {
    /// The `#cs-qo-note` text for this seed, or `None` to hide the bar.
    ///
    /// `seed()`'s own strings (quickopen.js:419) plus `setNote('')` (420) for
    /// the empty case. Expressed as a value so the renderer cannot show "Recent
    /// documents." above an empty list.
    pub fn note(&self) -> Option<&'static str> {
        match self {
            Seed::Query(_) => None,
            Seed::Recents(_) => Some("Recent documents. Type to search files and symbols."),
            Seed::EmptyRecents => None,
        }
    }
}

// ------------------------------------------------------------------ ranking

/// Why a row matched — the two-level key [`rank`] sorts on.
///
/// Variant order **is** the contract (the derived `Ord` is the primary key), and
/// it is copied from [`crate::search`]'s measured rule: a hit on the symbol's own
/// name beats a hit on the context it merely lives in, whatever the match class.
/// `search.rs:37-49` documents the measurement that forced this ordering; this is
/// the same rule, so the palette and the Find panel cannot disagree about which
/// of two similar symbols a user meant.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Why {
    /// The query is exactly the row's own display name (`find` -> `find`).
    NameExact,
    /// The row's own display name contains the query as a substring.
    NameSubstring,
    /// The query is a subsequence of the name (`fnd` -> `find`).
    NameSubsequence,
    /// Only the row's file path contains it — context, not the name, and a
    /// *weaker* signal than any name hit, even a gapped one. This is what keeps
    /// a symbol merely living in `find.rs` behind one actually named `f_i_n_d`.
    FileSubstring,
    /// The query is a subsequence of the file path only.
    FileSubsequence,
    /// Alphabetical filler: kept so [`rank`] is total.
    Fallback,
}

/// Is `q` an in-order subsequence of `hay`, case-insensitively?
///
/// The quick-open expectation: `mfin` matches `model/find.rs`. Empty `q` matches
/// (the reference's no-query path shows recents, not matches, so this only
/// matters for a caller that ranks without a query).
pub fn subsequence(hay: &str, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    let mut it = hay.chars().flat_map(|c| c.to_lowercase());
    let mut want = q.chars().flat_map(|c| c.to_lowercase());
    let mut cur = match want.next() {
        Some(c) => c,
        None => return true,
    };
    for c in it.by_ref() {
        if c == cur {
            match want.next() {
                Some(n) => cur = n,
                None => return true,
            }
        }
    }
    false
}

/// Score a subsequence match: how *good* the match is, higher first.
///
/// Gaps are penalised and a match starting on a word boundary or a run of
/// consecutive characters is rewarded, which is what separates `find_wire.rs`
/// from `filters.rs` for the query `fwr`. Returns `None` when `q` is not a
/// subsequence of `hay` at all.
///
/// The score is only ever compared within one [`Why`] class, so its absolute
/// scale is not part of any contract — only its ordering is, and that ordering
/// is total (ties fall through to [`rank`]'s later keys).
pub fn subseq_score(hay: &str, q: &str) -> Option<i32> {
    if q.is_empty() {
        return Some(0);
    }
    let hay_l: Vec<char> = hay.chars().flat_map(|c| c.to_lowercase()).collect();
    let q_l: Vec<char> = q.chars().flat_map(|c| c.to_lowercase()).collect();
    let mut qi = 0usize;
    let mut score = 0i32;
    let mut last: Option<usize> = None;
    for (i, c) in hay_l.iter().enumerate() {
        if qi >= q_l.len() {
            break;
        }
        if *c != q_l[qi] {
            continue;
        }
        // Word-boundary bonus: the first char, or one following a separator.
        let boundary = i == 0
            || matches!(
                hay_l[i - 1],
                '/' | '\\' | '.' | '_' | '-' | ' ' | ':' | '(' | '['
            );
        if boundary {
            score += 12;
        }
        match last {
            // Consecutive run. The bonus must exceed the largest possible gap
            // penalty (10 * 2, below) or a contiguous match can be beaten by a
            // scattered one that merely collected boundary bonuses.
            Some(prev) if prev + 1 == i => score += 24,
            Some(prev) => score -= (i - prev - 1).min(10) as i32 * 2, // gap penalty
            None => score -= (i as i32).min(20),       // late start
        }
        last = Some(i);
        qi += 1;
    }
    if qi == q_l.len() {
        Some(score)
    } else {
        None
    }
}

/// The best [`Why`] for one row against `q`, or `None` when it does not match.
///
/// The classes are tested strongest-first, and the precedence is deliberately
/// **two-level** (`search.rs`'s measured rule): every hit on the row's own name
/// beats every hit on the file it merely lives in. So the name is resolved
/// through its whole ladder — exact, then substring, then subsequence — before
/// the file is consulted at all.
///
/// Testing `FileSubstring` between `NameSubstring` and `NameSubsequence` (the
/// earlier shape) let a symbol that merely *lives in* `find.rs` outrank one
/// genuinely named `f_i_n_d`, which is exactly the inversion `search.rs:37-49`
/// documents as a bug.
pub fn why(row: &Row, q: &str) -> Option<Why> {
    if q.is_empty() {
        return Some(Why::Fallback);
    }
    let ql = q.to_lowercase();
    let name = row.name_field().to_lowercase();
    if name == ql {
        return Some(Why::NameExact);
    }
    if name.contains(&ql) {
        return Some(Why::NameSubstring);
    }
    if subsequence(row.name_field(), q) {
        return Some(Why::NameSubsequence);
    }
    let file = row.file.to_lowercase();
    if !file.is_empty() && file.contains(&ql) {
        return Some(Why::FileSubstring);
    }
    if !row.file.is_empty() && subsequence(&row.file, q) {
        return Some(Why::FileSubsequence);
    }
    None
}

/// Rank rows best-first, dropping the ones that do not match.
///
/// The chain, in order, is:
///
/// 1. [`Why`] — name before context, then match strength.
/// 2. [`Source`] — a definition outranks a usage, preserving `composeSearch`'s
///    concatenation order (quickopen.js:224-241) *within* one match class.
/// 3. the best [`subseq_score`] (higher first) — this is where `find_wire.rs`
///    beats `filters.rs` for `fwr`.
/// 4. `id`, then `label`, then `file` — the total tie-break, so the result never
///    depends on input order once everything else is equal. `id` is unique
///    within a deduped list, so the order is total.
///
/// `truncate` to [`MAX_ROWS`] happens last, exactly as `composeSearch` does
/// (quickopen.js:257), so ranking cannot change the cap.
pub fn rank(mut rows: Vec<Row>, q: &str) -> Vec<Row> {
    rows.retain(|r| why(r, q).is_some());
    rows.sort_by(|a, b| {
        let wa = why(a, q).unwrap_or(Why::Fallback);
        let wb = why(b, q).unwrap_or(Why::Fallback);
        wa.cmp(&wb)
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| {
                let sa = subseq_score(a.name_field(), q)
                    .or_else(|| subseq_score(&a.file, q))
                    .unwrap_or(i32::MIN);
                let sb = subseq_score(b.name_field(), q)
                    .or_else(|| subseq_score(&b.file, q))
                    .unwrap_or(i32::MIN);
                sb.cmp(&sa)
            })
            .then_with(|| a.id.cmp(&b.id))
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.file.cmp(&b.file))
    });
    rows.truncate(MAX_ROWS);
    rows
}

/// Rank rows out of a real [`Graph`] — the native stand-in for `/api/search`.
///
/// This is the one function here that touches the graph, and it is deliberately
/// a thin adapter: it converts nodes to [`Row`]s (via
/// [`Row::from_search_node`], i.e. `fromNode(n, 'search')`) and hands them to
/// [`rank`]. The incumbent's `/api/search` already answered in ranked order; the
/// native fork has to produce that order itself, and [`rank`] is where the rule
/// lives so the palette and the Find panel share one implementation.
pub fn rank_graph(graph: &Graph, q: &str, want_symbols: bool) -> Vec<Row> {
    let limit = search_limit(want_symbols);
    let mut cands: Vec<Row> = graph
        .nodes
        .iter()
        .filter_map(Row::from_search_node)
        .collect();
    // `/api/search`'s own limit is applied to the *candidate* set before
    // ranking, mirroring a server that returns at most `limit` rows; the
    // palette's MAX_ROWS cap is then applied inside `rank`.
    let ranked = rank(cands.split_off(0), q);
    let _ = cands;
    ranked.into_iter().take(limit.max(MAX_ROWS)).collect()
}

/// Convenience: rank a graph and compose it as the palette would, with no
/// `/api/find` payload. Files mode's whole pipeline in one call.
///
/// This is the one place where [`compose`]'s input is **already ranked**, and
/// that changes what its files-mode reorder means. The reference runs
/// `composeSearch` on a raw `/api/search` payload, where "does the path contain
/// the query" is the only signal it has, so floating file hits to the front is a
/// pure improvement. Applied to a ranked list it is a *regression*: for the query
/// `find`, [`rank`] correctly puts the symbol NAMED `find` first
/// (`NameExact`), and the file-first pass would then bury it behind every row
/// that merely lives in `find.rs` (`FileSubstring`) — the precise inversion
/// `search.rs:37-49` documents as a measured bug, and the thing `rank` exists to
/// prevent.
///
/// So the file-first preference is applied only *within* the rows that are not
/// name matches: a row whose own name matches keeps the position [`rank`] gave
/// it, and everything else still floats path-matches above non-matches.
pub fn query_graph(graph: &Graph, q: &str, mode: Mode) -> Vec<Row> {
    let ranked = rank_graph(graph, q, mode.want_symbols());
    if mode.want_symbols() || looks_like_path(q) {
        // `compose` is already exactly right for these two branches: symbols
        // mode does not reorder at all, and a slash query *filters* rather than
        // reorders, which is meaningful on ranked input too.
        return compose(ranked, None, q, mode.want_symbols());
    }
    // Split off the name matches, let `compose` do the reference's file-first
    // reorder on the remainder, then splice the two back together. The name
    // matches are already in best-first order, so concatenation preserves it.
    let (named, rest): (Vec<Row>, Vec<Row>) =
        ranked.into_iter().partition(|r| is_name_hit(r, q));
    let mut out = named;
    out.extend(compose(rest, None, q, false));
    out.truncate(MAX_ROWS);
    out
}

/// Is the row a *name* hit for `q` — i.e. one of the three `Why` classes that
/// describe the row's own name rather than the file it lives in?
fn is_name_hit(row: &Row, q: &str) -> bool {
    matches!(
        why(row, q),
        Some(Why::NameExact) | Some(Why::NameSubstring) | Some(Why::NameSubsequence)
    )
}

/// The honest reason the list is empty, or `None` when it is not.
///
/// `runSearch`'s two notices (quickopen.js:271, 277) and `seed`'s recents note
/// (419) are all *sentences about state*. Making them values keeps the renderer
/// from having to guess, and keeps "nothing matched" distinguishable from
/// "nothing typed" — the distinction `search.rs` already makes for `#findNotice`.
pub fn no_match_note(q: &str) -> String {
    format!("No files or symbols match \u{201c}{}\u{201d}.", q)
}

/// The `Empty`-list text — `renderList` (quickopen.js:311-313).
///
/// `'No matches.'` once something was typed, `'Type to search files and
/// symbols.'` otherwise. `seed_query` is the remembered query
/// (quickopen.js:36), not the raw input, which is why it is a parameter here.
pub fn empty_text(seed_query: &str) -> &'static str {
    if seed_query.is_empty() {
        "Type to search files and symbols."
    } else {
        "No matches."
    }
}

// ------------------------------------------------------------------ selection

/// The palette's selection state: which row is highlighted.
///
/// `sel` is `-1` exactly when there is nothing to select (quickopen.js:289:
/// `sel = rows.length ? 0 : -1`). Modelling "no selection" as `None` rather than
/// the sentinel keeps [`Selection::enter`] total: the reference's
/// `activate(sel)` with `sel === -1` returns `null` because `rows[-1]` is
/// `undefined`, which is the same fact stated with a sentinel.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Selection {
    sel: Option<usize>,
}

impl Selection {
    /// `setRows(list)` — quickopen.js:289. Selects row 0 when there are rows,
    /// nothing otherwise. Note the reference *resets* to 0 on every new result
    /// set, so a fresh search never keeps an old index.
    pub fn set_rows(len: usize) -> Selection {
        Selection {
            sel: if len == 0 { None } else { Some(0) },
        }
    }

    pub fn index(self) -> Option<usize> {
        self.sel
    }

    /// Is this row the selected one? Drives the `sel` class (quickopen.js:320).
    pub fn is_selected(self, i: usize) -> bool {
        self.sel == Some(i)
    }

    /// `move(delta)` — quickopen.js:355-366.
    ///
    /// Wraps at both ends (`sel < 0` -> last, `sel >= len` -> first). A no-op
    /// when there are no rows, which is why `len` is passed in rather than
    /// stored: the caller owns the row list.
    pub fn moved(self, delta: i64, len: usize) -> Selection {
        if len == 0 {
            return self;
        }
        let cur = self.sel.unwrap_or(0) as i64;
        let mut next = cur + delta;
        let n = len as i64;
        if next < 0 {
            next = n - 1;
        }
        if next >= n {
            next = 0;
        }
        Selection {
            sel: Some(next as usize),
        }
    }

    /// `activate(i)` — quickopen.js:384-389: the row Enter opens, if any.
    ///
    /// Returns the index; the reference returns the *opened row* (`openRow(r)`)
    /// or `null`. The host performs the open, so the index is what it needs.
    pub fn enter(self) -> Option<usize> {
        self.sel
    }
}

/// One key the palette handles while it is open — `onWindowKeyDown`
/// (quickopen.js:463-491).
///
/// The reference also opens the palette on Ctrl/Cmd+P and Ctrl/Cmd+T (465-476);
/// those are *global* bindings and are modelled here too, because the whole
/// point of the module is that the palette owns its keys. `Ctrl+P`/`Ctrl+T`
/// carry no modifier beyond the platform accelerator: the reference requires
/// `!shiftKey && !altKey` for both, so `Ctrl+Shift+P` is deliberately **not** a
/// palette chord and must fall through to whatever else wants it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// `Ctrl/Cmd+P`, no shift/alt — `open('files')`.
    OpenFiles,
    /// `Ctrl/Cmd+T`, no shift/alt — `open('symbols')`.
    OpenSymbols,
    /// `Escape` — `close()`.
    Escape,
    Up,
    Down,
    PageUp,
    PageDown,
    /// `Tab` moves down, `Shift+Tab` up (quickopen.js:490) — the palette reuses
    /// the key as navigation rather than as focus traversal.
    Tab { shift: bool },
    /// `Enter` — `activate(sel)`.
    Enter,
}

/// Resolve a real key event to a palette [`Key`], or `None` to let it through.
///
/// `shown` is the reference's `shown` flag: the two open chords are handled
/// whether or not the palette is up, and **every other** key is ignored while it
/// is down (quickopen.js:477: `if (!shown) return`). That ordering is the
/// contract — it is what stops the palette from swallowing Escape or the arrow
/// keys when it is closed.
pub fn resolve_key(key: &str, ctrl: bool, meta: bool, shift: bool, alt: bool, shown: bool) -> Option<Key> {
    let mod_ = ctrl || meta;
    // quickopen.js:465-476. Case-insensitive on the letter: the reference tests
    // both `'p'` and `'P'`.
    if mod_ && !shift && !alt {
        if key.eq_ignore_ascii_case("p") {
            return Some(Key::OpenFiles);
        }
        if key.eq_ignore_ascii_case("t") {
            return Some(Key::OpenSymbols);
        }
    }
    if !shown {
        return None;
    }
    // quickopen.js:478-490, in the reference's own order.
    match key {
        "Escape" => Some(Key::Escape),
        "ArrowDown" => Some(Key::Down),
        "ArrowUp" => Some(Key::Up),
        "PageDown" => Some(Key::PageDown),
        "PageUp" => Some(Key::PageUp),
        "Enter" => Some(Key::Enter),
        "Tab" => Some(Key::Tab { shift }),
        _ => None,
    }
}

/// `PageDown`/`PageUp` move by 10 (quickopen.js:487-488), everything else by 1.
///
/// `Tab` is the reference's `move(ev.shiftKey ? -1 : 1)` (quickopen.js:490): a
/// plain `Tab` moves **forward**, exactly like `ArrowDown`, and only
/// `Shift+Tab` reverses. The sign is carried by `shift`, so an unshifted `Tab`
/// must not fall into the `_ => 1` default by accident of the pattern order.
pub fn step_for(key: Key) -> i64 {
    match key {
        Key::PageDown => 10,
        Key::PageUp => -10,
        Key::Down => 1,
        Key::Up => -1,
        Key::Tab { shift: true } => -1,
        Key::Tab { shift: false } => 1,
        _ => 1,
    }
}

/// What `openRow` decided to do — quickopen.js:369-382.
///
/// The reference calls `ctx.openSource`/`ctx.goToNode` and swallows exceptions
/// (`try { … } catch (e) { /* fall through */ }`). The decision of *which*
/// payload to send is the model's; performing it is the host's.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Open {
    /// `openSource({ node: r.id })` plus `goToNode(r.id)` — quickopen.js:372-375.
    Node { id: String },
    /// `openSource({ file, line: r.line || 1, mode: 'raw' })`, and `goToNode`
    /// when the row also had an id — quickopen.js:375-377.
    ///
    /// Note `r.line || 1`: a zero line becomes **1**, not 0, on this path — the
    /// opposite of the display rule in [`Row::loc`]. Both are the reference's.
    File { file: String, line: i64, id: String },
    /// The editor is not ready: `toast('The editor is not ready yet', 'warn')`
    /// — quickopen.js:379.
    NotReady,
}

/// Decide what activating `row` opens — `openRow(r)` (quickopen.js:369-382).
///
/// The branch order is the reference's: an **id** wins even when a file is
/// present (372), a file-only row opens the file directly (375), and a row with
/// neither reports that the editor is not ready rather than opening nothing
/// silently.
pub fn open_row(row: &Row) -> Open {
    if !row.id.is_empty() {
        return Open::Node {
            id: row.id.clone(),
        };
    }
    if !row.file.is_empty() {
        return Open::File {
            file: row.file.clone(),
            line: if row.line == 0 { 1 } else { row.line },
            id: row.id.clone(),
        };
    }
    Open::NotReady
}

/// The `Esc`/`Enter`/arrow hint in the header — quickopen.js:142.
pub const HINT: &str = "\u{2191}\u{2193} \u{00b7} Enter \u{00b7} Esc";

/// The overlay's accessible name — `aria-label` (quickopen.js:121).
pub const ARIA_LABEL: &str = "Quick open";

/// The input's id and the list's id (quickopen.js:132, 148).
pub const INPUT_ID: &str = "cs-qo-input";
pub const LIST_ID: &str = "cs-qo-list";
pub const NOTE_ID: &str = "cs-qo-note";
pub const OVERLAY_ID: &str = "cs-quickopen";

/// Is this the query text the palette considers *unset*?
///
/// `runSearch` trims before testing for emptiness (quickopen.js:265), but
/// `seedQuery` — which decides the empty-list copy at line 311 — keeps the
/// **untrimmed** input. Both are preserved: trimming is done at the call site
/// that mirrors `runSearch`, and this predicate is the `seedQuery` one.
pub fn seed_query_is_empty(seed_query: &str) -> bool {
    seed_query.is_empty()
}

// ---------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, label: &str, full: &str, file: &str, line: i64) -> Row {
        Row {
            id: id.to_string(),
            kind: "method".to_string(),
            name: label.to_string(),
            full: full.to_string(),
            label: label.to_string(),
            file: file.to_string(),
            line,
            source: Source::Search,
        }
    }

    // ------------------------------------------------------ constants (ref)

    /// The four constants are the reference's own literals; a change here is a
    /// change to behaviour, not a tuning knob.
    #[test]
    fn constants_match_the_reference() {
        assert_eq!(MAX_ROWS, 60, "quickopen.js:23");
        assert_eq!(DEBOUNCE_MS, 140, "quickopen.js:24");
        assert_eq!(RECENT_MAX, 8, "quickopen.js:25");
        assert_eq!(search_limit(false), 80, "quickopen.js:208");
        assert_eq!(search_limit(true), 50, "quickopen.js:208");
        assert_eq!(FIND_LIMIT, 50, "quickopen.js:212");
        assert_eq!(USAGE_MAX, 20, "quickopen.js:230");
        assert_eq!(HINT, "\u{2191}\u{2193} \u{00b7} Enter \u{00b7} Esc", "quickopen.js:142");
        assert_eq!(ARIA_LABEL, "Quick open", "quickopen.js:121");
    }

    #[test]
    fn mode_labels_and_placeholders_are_the_reference_strings() {
        assert_eq!(Mode::Files.label(), "Files & symbols"); // quickopen.js:39
        assert_eq!(Mode::Symbols.label(), "Symbols");
        assert_eq!(Mode::Files.placeholder(), "Type a file or symbol name\u{2026}");
        assert_eq!(Mode::Symbols.placeholder(), "Type a symbol name\u{2026}");
        assert!(Mode::Symbols.want_symbols());
        assert!(!Mode::Files.want_symbols());
    }

    /// The reference coerces an unknown mode to `files`; there is no third mode.
    #[test]
    fn unknown_mode_string_falls_back_to_files() {
        assert_eq!(Mode::parse("symbols"), Mode::Symbols);
        assert_eq!(Mode::parse("files"), Mode::Files);
        assert_eq!(Mode::parse("garbage"), Mode::Files);
        assert_eq!(Mode::parse(""), Mode::Files);
    }

    // ------------------------------------------------------------- fromNode

    /// The three fallback chains of `fromNode`, including the **different**
    /// orders for `name` and `label`.
    #[test]
    fn from_node_replicates_the_fallback_chains() {
        // Nothing set but the id: every chain terminates on the id.
        let r = Row::from_node("n1", "", "", "", "", "", 0, Source::Search).unwrap();
        assert_eq!(r.name, "n1");
        assert_eq!(r.label, "n1");
        assert_eq!(r.kind, "", "kind is `n.kind || n.label`, not `|| id`");

        // label set, name empty: name falls back to label; label prefers label.
        let r = Row::from_node("n1", "", "", "", "Widget", "", 0, Source::Search).unwrap();
        assert_eq!(r.name, "Widget", "n.name || n.label");
        assert_eq!(r.label, "Widget");
        assert_eq!(r.kind, "Widget", "n.kind || n.label");

        // full set only: name -> full, label -> full.
        let r = Row::from_node("n1", "", "", "N.Widget", "", "", 0, Source::Search).unwrap();
        assert_eq!(r.name, "N.Widget");
        assert_eq!(r.label, "N.Widget");

        // name set only: label falls back to name (3rd in its chain).
        let r = Row::from_node("n1", "", "Bare", "", "", "", 0, Source::Search).unwrap();
        assert_eq!(r.name, "Bare");
        assert_eq!(r.label, "Bare");
    }

    /// The drop guard: no id **and** no file means the row never renders.
    #[test]
    fn from_node_drops_a_row_with_neither_id_nor_file() {
        assert!(Row::from_node("", "", "", "", "", "", 0, Source::Search).is_none());
        // A file alone is enough to keep the row.
        assert!(Row::from_node("", "", "", "", "", "a/b.rs", 0, Source::Search).is_some());
        // An id alone is enough.
        assert!(Row::from_node("n1", "", "", "", "", "", 0, Source::Search).is_some());
    }

    /// The usage row reads the nested node for names and the *outer* object for
    /// the location.
    #[test]
    fn usage_row_uses_nested_node_for_names_and_outer_for_location() {
        let r = Row::from_usage("n9", "callee", "Callee", "N.Callee", "callee(...)", "src/a.rs", 42);
        assert_eq!(r.kind, "usage", "forced regardless of the node kind");
        assert_eq!(r.name, "callee");
        assert_eq!(r.full, "N.Callee");
        assert_eq!(r.file, "src/a.rs");
        assert_eq!(r.line, 42);
        assert_eq!(r.source, Source::Usage);

        // No node name: `u.text` is the last resort.
        let r = Row::from_usage("", "", "", "", "some text", "src/a.rs", 7);
        assert_eq!(r.name, "some text");
        assert_eq!(r.label, "some text");
    }

    // --------------------------------------------------------------- dedupe

    #[test]
    fn dedupe_keeps_the_first_occurrence_in_input_order() {
        let rows = vec![
            row("a", "first", "", "1.rs", 1),
            row("b", "second", "", "2.rs", 2),
            row("a", "duplicate-of-a", "", "9.rs", 9),
        ];
        let out = dedupe(rows);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label, "first", "first wins, later copy dropped");
        assert_eq!(out[1].label, "second");
    }

    /// The key is `id:` when there is an id, else `loc:file:line` — two rows
    /// with the same file+line but no id collide.
    #[test]
    fn dedupe_key_prefers_id_then_location() {
        let a = row("n1", "x", "", "f.rs", 3);
        assert_eq!(a.dedupe_key(), "id:n1");
        let b = row("", "x", "", "f.rs", 3);
        assert_eq!(b.dedupe_key(), "loc:f.rs:3");
        // Same location, no ids: collapsed.
        let out = dedupe(vec![b.clone(), row("", "y", "", "f.rs", 3)]);
        assert_eq!(out.len(), 1);
        // Same location but one HAS an id: distinct keys, both survive.
        let out = dedupe(vec![row("", "y", "", "f.rs", 3), row("n2", "z", "", "f.rs", 3)]);
        assert_eq!(out.len(), 2);
    }

    // ---------------------------------------------------------------- file_hit

    #[test]
    fn file_hit_is_a_case_insensitive_substring_of_the_file_only() {
        // FIXTURE TYPO (corrected): this row originally read
        // `row("n1", "Widget", "N.Widget", "Src/Model/Widget.rs", 1)`, whose file
        // *textually contains* its own name — `"Src/Model/Widget.rs"
        // .to_lowercase().contains("widget")` is `true`. Under the reference's
        // predicate (`r.file && r.file.toLowerCase().indexOf(q.toLowerCase()) >= 0`,
        // quickopen.js:245/252) the final assertion could therefore never hold:
        // it asserted that a path does not contain the very token it ends with.
        // The path is changed to one that does not contain the name, so the
        // assertions test the *predicate* rather than contradicting the fixture.
        let r = row("n1", "Widget", "N.Widget", "Src/Model/wdg_impl.rs", 1);
        assert!(file_hit(&r, "wdg"));
        assert!(file_hit(&r, "MODEL"), "case-insensitive");
        assert!(file_hit(&r, "src/model"));
        // The row's *name* is `Widget`, which the file does not contain: reading
        // the name here would wrongly report a hit.
        assert!(!file_hit(&r, "Widget"), "the *name* is not the file");
        // A row with no file is never a file hit, even for an empty query.
        let nofile = row("n1", "W", "", "", 0);
        assert!(!file_hit(&nofile, ""));
        assert!(!file_hit(&nofile, "anything"));
    }

    #[test]
    fn looks_like_path_is_the_slash_test() {
        assert!(looks_like_path("src/model"));
        assert!(looks_like_path("/abs"));
        assert!(looks_like_path("trailing/"));
        assert!(!looks_like_path("Widget"));
        assert!(!looks_like_path(""));
    }

    // ---------------------------------------------------------------- compose

    /// Files mode with no slash: file hits move to the FRONT, symbol hits stay
    /// behind them (quickopen.js:249-255). Nothing is dropped.
    #[test]
    fn compose_files_mode_prefers_file_hits_without_hiding_symbol_hits() {
        let search = vec![
            row("n1", "Widget", "N.Widget", "other/place.rs", 1), // symbol hit
            row("n2", "helper", "N.helper", "src/widget.rs", 2),  // file hit
        ];
        let out = compose(search, None, "widget", false);
        assert_eq!(out.len(), 2, "no row is hidden");
        assert_eq!(out[0].id, "n2", "file hit first");
        assert_eq!(out[1].id, "n1", "symbol hit kept, just after");
    }

    /// A slash query keeps ONLY file hits — unless that would empty the list.
    #[test]
    fn compose_slash_query_filters_but_never_empties_the_list() {
        let search = vec![
            row("n1", "Widget", "N.Widget", "other/place.rs", 1),
            row("n2", "helper", "N.helper", "src/widget.rs", 2),
        ];
        let out = compose(search.clone(), None, "src/", false);
        assert_eq!(out.len(), 1, "only the file hit survives");
        assert_eq!(out[0].id, "n2");

        // No file matches `zz/`: the filter is abandoned rather than emptying.
        let out = compose(search, None, "zz/", false);
        assert_eq!(out.len(), 2, "symbol rows are kept when the filter matches nothing");
    }

    /// Symbols mode: `/api/find` rows are merged BEFORE the search rows, and
    /// dedupe therefore keeps the find copy of a duplicated symbol.
    #[test]
    fn compose_symbols_mode_prepends_find_rows_and_wins_dedupe() {
        let search = vec![row("dup", "from-search", "S", "s.rs", 1)];
        let find = FindRows {
            definitions: vec![row("dup", "from-find", "F", "f.rs", 9)],
            implementations: vec![],
            usages: vec![],
        };
        let out = compose(search, Some(find), "dup", true);
        assert_eq!(out.len(), 1, "one id, deduped");
        assert_eq!(out[0].label, "from-find", "find rows are concatenated first");
    }

    /// The three `/api/find` lists merge in the reference's order:
    /// definitions, implementations, usages.
    #[test]
    fn compose_merges_find_lists_in_reference_order() {
        let mut d = row("d", "def", "", "d.rs", 1);
        d.source = Source::Definition;
        let mut i = row("i", "impl", "", "i.rs", 1);
        i.source = Source::Implementation;
        let mut u = row("u", "use", "", "u.rs", 1);
        u.source = Source::Usage;
        let find = FindRows {
            definitions: vec![d],
            implementations: vec![i],
            usages: vec![u],
        };
        let out = compose(vec![], Some(find), "x", true);
        let ids: Vec<&str> = out.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["d", "i", "u"]);
    }

    /// Only the first 20 usage rows survive (quickopen.js:230).
    #[test]
    fn compose_caps_usages_at_twenty() {
        let usages: Vec<Row> = (0..30)
            .map(|n| {
                let mut r = row(&format!("u{n}"), "u", "", "u.rs", n as i64);
                r.source = Source::Usage;
                r
            })
            .collect();
        let find = FindRows {
            definitions: vec![],
            implementations: vec![],
            usages,
        };
        let out = compose(vec![], Some(find), "x", true);
        assert_eq!(out.len(), USAGE_MAX);
        assert_eq!(out[19].id, "u19", "the cap is a head-slice, not a sample");
    }

    /// The find payload is ignored in files mode (quickopen.js:211).
    #[test]
    fn compose_ignores_find_rows_in_files_mode() {
        let find = FindRows {
            definitions: vec![row("d", "def", "", "d.rs", 1)],
            implementations: vec![],
            usages: vec![],
        };
        let out = compose(vec![row("s", "search", "", "s.rs", 1)], Some(find), "x", false);
        let ids: Vec<&str> = out.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["s"], "no find rows leak into files mode");
    }

    /// The cap is applied after the merge, at exactly MAX_ROWS.
    #[test]
    fn compose_truncates_to_max_rows_after_merging() {
        let search: Vec<Row> = (0..(MAX_ROWS + 25))
            .map(|n| row(&format!("n{n}"), "n", "", "a.rs", n as i64))
            .collect();
        let out = compose(search, None, "n", false);
        assert_eq!(out.len(), MAX_ROWS);
        assert_eq!(out[0].id, "n0");
    }

    // --------------------------------------------------------------- recents

    fn nav(node_id: &str, file: &str, line: i64, label: &str, mode: &str) -> NavEntry {
        NavEntry {
            node_id: node_id.to_string(),
            file: file.to_string(),
            line,
            label: label.to_string(),
            mode: mode.to_string(),
        }
    }

    /// `recent()` walks history BACKWARDS, so the newest entry is row 0.
    #[test]
    fn recent_is_newest_first() {
        let hist = vec![nav("old", "a.rs", 1, "Old", "doc"), nav("new", "b.rs", 2, "New", "doc")];
        let out = recent(&hist);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "new", "the last history entry comes first");
        assert_eq!(out[1].id, "old");
    }

    #[test]
    fn recent_caps_at_eight() {
        let hist: Vec<NavEntry> = (0..20)
            .map(|n| nav(&format!("n{n}"), "f.rs", n, "l", "doc"))
            .collect();
        let out = recent(&hist);
        assert_eq!(out.len(), RECENT_MAX);
        assert_eq!(out[0].id, "n19", "newest first, capped at 8");
    }

    /// `kind` is `node` for a doc entry and `file` otherwise, and `full` is
    /// always empty (quickopen.js:402-407).
    #[test]
    fn recent_kind_depends_on_doc_mode_and_full_is_empty() {
        let hist = vec![nav("d", "a.rs", 1, "Doc", "doc"), nav("f", "b.rs", 2, "Fil", "raw")];
        let out = recent(&hist);
        assert_eq!(out[0].kind, "file", "non-doc -> file");
        assert_eq!(out[1].kind, "node", "doc -> node");
        assert!(out.iter().all(|r| r.full.is_empty()));
        assert!(out.iter().all(|r| r.source == Source::Recent));
    }

    /// `seed` has three distinguishable outcomes.
    #[test]
    fn seed_distinguishes_query_recents_and_neither() {
        let empty: Vec<NavEntry> = vec![];
        assert_eq!(seed("q", &empty), Seed::Query("q".to_string()));
        assert_eq!(seed("", &empty), Seed::EmptyRecents);

        let hist = vec![nav("n", "a.rs", 1, "L", "doc")];
        match seed("", &hist) {
            Seed::Recents(rows) => assert_eq!(rows.len(), 1),
            other => panic!("expected Recents, got {other:?}"),
        }
    }

    /// The recents note is shown ONLY when there are recents — the empty case
    /// must not claim "Recent documents." above an empty list.
    #[test]
    fn seed_note_is_only_set_for_real_recents() {
        let empty: Vec<NavEntry> = vec![];
        assert_eq!(seed("", &empty).note(), None);
        assert_eq!(seed("q", &empty).note(), None);
        let hist = vec![nav("n", "a.rs", 1, "L", "doc")];
        assert!(seed("", &hist).note().is_some());
    }

    // ------------------------------------------------------------- subsequence

    #[test]
    fn subsequence_matches_in_order_with_gaps() {
        assert!(subsequence("model/find_wire.rs", "mfin"));
        assert!(subsequence("find", "fnd"));
        assert!(subsequence("Widget", "wgt"));
        assert!(subsequence("find", "find"));
        assert!(subsequence("FIND_WIRE", "fwr"), "case-insensitive");
        assert!(subsequence("anything", ""), "an empty query matches");
    }

    #[test]
    fn subsequence_rejects_out_of_order_and_missing_chars() {
        assert!(!subsequence("find", "dnif"), "order matters");
        assert!(!subsequence("find", "findx"), "a missing char fails");
        assert!(!subsequence("abc", "abcd"));
    }

    /// Word-boundary starts and consecutive runs score above a scattered match.
    #[test]
    fn subseq_score_prefers_boundaries_and_runs() {
        let tight = subseq_score("find_wire.rs", "find").unwrap();
        let loose = subseq_score("f_i_n_d_wire.rs", "find").unwrap();
        assert!(tight > loose, "consecutive run {tight} must beat scattered {loose}");
        // A boundary start beats a late start.
        let at_boundary = subseq_score("a/b/find", "find").unwrap();
        let mid_word = subseq_score("abcfindx", "find").unwrap();
        assert!(at_boundary > mid_word, "{at_boundary} vs {mid_word}");
        assert!(subseq_score("abc", "xyz").is_none(), "no match -> None");
    }

    // ------------------------------------------------------------------- why

    /// The two-level rule: a NAME hit beats a FILE hit at every strength.
    #[test]
    fn why_ranks_name_hits_above_file_hits() {
        let name_hit = row("n1", "finder", "N.finder", "unrelated.rs", 1);
        let file_hit_row = row("n2", "other", "N.other", "src/finder.rs", 1);
        assert_eq!(why(&name_hit, "find"), Some(Why::NameSubstring));
        assert_eq!(why(&file_hit_row, "find"), Some(Why::FileSubstring));
        assert!(Why::NameSubstring < Why::FileSubstring, "name before context");

        // And a name *subsequence* still beats a file *substring*.
        let name_sub = row("n3", "f_i_n_d", "", "unrelated.rs", 1);
        assert_eq!(why(&name_sub, "find"), Some(Why::NameSubsequence));
        assert!(Why::NameSubsequence < Why::FileSubstring);
    }

    #[test]
    fn why_is_none_when_nothing_matches() {
        let r = row("n1", "alpha", "N.alpha", "src/alpha.rs", 1);
        assert_eq!(why(&r, "zzzz"), None);
    }

    // ------------------------------------------------------------------ rank

    /// The measured `search.rs` correction: the query `find` must put the symbol
    /// NAMED find above a symbol that merely LIVES in find.rs.
    #[test]
    fn rank_puts_a_name_hit_above_a_file_owner() {
        let in_file = row("a", "something", "N.something", "src/find.rs", 1);
        let named = row("b", "find", "N.find", "src/other.rs", 1);
        let out = rank(vec![in_file, named], "find");
        assert_eq!(out[0].id, "b", "the symbol named `find` wins");
        assert_eq!(out[1].id, "a");
    }

    /// Within one match class, a definition outranks a usage.
    #[test]
    fn rank_prefers_definitions_over_usages() {
        let mut usage = row("u", "find", "N.find", "u.rs", 1);
        usage.source = Source::Usage;
        let mut def = row("d", "find", "N.find", "d.rs", 1);
        def.source = Source::Definition;
        // Same Why class (NameSubstring), different Source.
        let out = rank(vec![usage.clone(), def.clone()], "find");
        assert_eq!(out[0].id, "d", "definition first");
        assert_eq!(out[1].id, "u");
    }

    /// The score breaks a tie inside one class and one source.
    #[test]
    fn rank_breaks_ties_by_subsequence_quality() {
        let scattered = row("s", "f_i_n_d", "", "a.rs", 1);
        let tight = row("t", "findWire", "", "b.rs", 1);
        let out = rank(vec![scattered, tight], "find");
        assert_eq!(out[0].id, "t", "the consecutive run ranks first");
    }

    /// Determinism: the same input always yields the same byte order, and the
    /// order does not depend on the input order once keys tie.
    #[test]
    fn rank_is_deterministic_and_total() {
        let a = row("id-a", "same", "N.same", "same.rs", 1);
        let b = row("id-b", "same", "N.same", "same.rs", 1);
        let out1 = rank(vec![a.clone(), b.clone()], "same");
        let out2 = rank(vec![b, a], "same");
        let ids1: Vec<&str> = out1.iter().map(|r| r.id.as_str()).collect();
        let ids2: Vec<&str> = out2.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids1, ids2, "input order must not change the output order");
        assert_eq!(ids1, vec!["id-a", "id-b"], "the id is the total tie-break");
    }

    #[test]
    fn rank_drops_non_matching_rows() {
        let rows = vec![
            row("keep", "alpha", "", "a.rs", 1),
            row("drop", "zzz", "", "z.rs", 1),
        ];
        let out = rank(rows, "alpha");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "keep");
    }

    #[test]
    fn rank_truncates_to_max_rows() {
        let rows: Vec<Row> = (0..(MAX_ROWS + 10))
            .map(|n| row(&format!("n{n:03}"), "aa", "", "a.rs", n as i64))
            .collect();
        assert_eq!(rank(rows, "aa").len(), MAX_ROWS);
    }

    // ------------------------------------------------------------- selection

    /// `setRows` selects row 0 when there are rows and NOTHING otherwise
    /// (quickopen.js:289) — the `sel = -1` sentinel.
    #[test]
    fn selection_resets_to_first_row_or_none() {
        assert_eq!(Selection::set_rows(0).index(), None);
        assert_eq!(Selection::set_rows(1).index(), Some(0));
        assert_eq!(Selection::set_rows(9).index(), Some(0));
        assert!(Selection::set_rows(3).is_selected(0));
        assert!(!Selection::set_rows(3).is_selected(1));
    }

    /// `move` wraps at BOTH ends and is a no-op with no rows.
    #[test]
    fn selection_move_wraps_both_ends() {
        let s = Selection::set_rows(3);
        assert_eq!(s.moved(1, 3).index(), Some(1));
        assert_eq!(s.moved(2, 3).index(), Some(2));
        assert_eq!(s.moved(3, 3).index(), Some(0), "past the end wraps to 0");
        assert_eq!(s.moved(0, 3).moved(-1, 3).index(), Some(2), "before 0 wraps to last");
        assert_eq!(s.moved(-1, 3).index(), Some(2));
        assert_eq!(s.moved(1, 0).index(), Some(0), "no rows is a no-op");
        assert_eq!(Selection::set_rows(0).moved(1, 0).index(), None);
        // PageDown/PageUp are move(+-10).
        assert_eq!(Selection::set_rows(9).moved(step_for(Key::PageDown), 9).index(), Some(0));
    }

    /// `activate(sel)` with the `-1` sentinel opens nothing.
    #[test]
    fn enter_with_no_selection_opens_nothing() {
        assert_eq!(Selection::set_rows(0).enter(), None);
        assert_eq!(Selection::set_rows(4).enter(), Some(0));
        assert_eq!(Selection::set_rows(4).moved(1, 4).enter(), Some(1));
    }

    // ------------------------------------------------------------ key routing

    /// The global open chords work whether or not the palette is shown, and
    /// require no shift/alt.
    #[test]
    fn open_chords_are_global_and_modifier_strict() {
        assert_eq!(resolve_key("p", true, false, false, false, false), Some(Key::OpenFiles));
        assert_eq!(resolve_key("P", true, false, false, false, false), Some(Key::OpenFiles));
        assert_eq!(resolve_key("t", true, false, false, false, true), Some(Key::OpenSymbols));
        assert_eq!(resolve_key("t", false, true, false, false, false), Some(Key::OpenSymbols), "Cmd on mac");
        // Shift or Alt disqualifies the chord — it must fall through.
        assert_eq!(resolve_key("p", true, false, true, false, false), None);
        assert_eq!(resolve_key("p", true, false, false, true, false), None);
        // No modifier at all is not a chord.
        assert_eq!(resolve_key("p", false, false, false, false, false), None);
    }

    /// `if (!shown) return` (quickopen.js:477) — navigation keys are ignored
    /// while the palette is closed, so it cannot swallow them.
    #[test]
    fn navigation_keys_are_ignored_while_closed() {
        for k in ["Escape", "ArrowDown", "ArrowUp", "PageDown", "PageUp", "Enter", "Tab"] {
            assert_eq!(resolve_key(k, false, false, false, false, false), None, "{k} while closed");
        }
        assert_eq!(resolve_key("Escape", false, false, false, false, true), Some(Key::Escape));
        assert_eq!(resolve_key("ArrowDown", false, false, false, false, true), Some(Key::Down));
        assert_eq!(resolve_key("ArrowUp", false, false, false, false, true), Some(Key::Up));
        assert_eq!(resolve_key("PageDown", false, false, false, false, true), Some(Key::PageDown));
        assert_eq!(resolve_key("PageUp", false, false, false, false, true), Some(Key::PageUp));
        assert_eq!(resolve_key("Enter", false, false, false, false, true), Some(Key::Enter));
    }

    /// `Tab` is navigation, and `Shift+Tab` goes the other way.
    #[test]
    fn tab_is_navigation_and_shift_reverses_it() {
        assert_eq!(resolve_key("Tab", false, false, false, false, true), Some(Key::Tab { shift: false }));
        assert_eq!(resolve_key("Tab", false, false, false, false, true), Some(Key::Tab { shift: false }));
        assert_eq!(
            resolve_key("Tab", false, false, true, false, true),
            Some(Key::Tab { shift: true })
        );
        assert_eq!(step_for(Key::Tab { shift: false }), 1);
        assert_eq!(step_for(Key::Tab { shift: true }), -1);
        assert_eq!(step_for(Key::PageDown), 10);
        assert_eq!(step_for(Key::PageUp), -10);
        assert_eq!(step_for(Key::Down), 1);
        assert_eq!(step_for(Key::Up), -1);
    }

    /// An unhandled key is let through, not swallowed.
    #[test]
    fn unrelated_keys_pass_through() {
        assert_eq!(resolve_key("a", false, false, false, false, true), None);
        assert_eq!(resolve_key("F5", false, false, false, false, true), None);
        assert_eq!(resolve_key("/", false, false, false, false, true), None, "shell.js owns `/`");
        assert_eq!(resolve_key("f", false, false, false, false, true), None, "shell.js owns `f`");
    }

    // ------------------------------------------------------------ open_row

    #[test]
    fn open_row_prefers_the_node_and_falls_back_to_the_file() {
        let with_id = row("n1", "x", "", "a.rs", 5);
        assert_eq!(open_row(&with_id), Open::Node { id: "n1".to_string() });

        let file_only = row("", "x", "", "a.rs", 5);
        assert_eq!(
            open_row(&file_only),
            Open::File { file: "a.rs".to_string(), line: 5, id: String::new() }
        );
    }

    /// `mode: 'raw'` opens at `r.line || 1`, so line 0 becomes 1 — the opposite
    /// of the display rule.
    #[test]
    fn open_row_turns_a_zero_line_into_one() {
        let file_only = row("", "x", "", "a.rs", 0);
        match open_row(&file_only) {
            Open::File { line, .. } => assert_eq!(line, 1, "`r.line || 1`"),
            other => panic!("expected File, got {other:?}"),
        }
        // But the *displayed* location omits a zero line entirely.
        assert_eq!(row("", "x", "", "a.rs", 0).loc(), "a.rs");
        assert_eq!(row("", "x", "", "a.rs", 7).loc(), "a.rs:7");
    }

    #[test]
    fn open_row_reports_not_ready_with_neither_id_nor_file() {
        let r = Row::default();
        assert_eq!(open_row(&r), Open::NotReady);
    }

    // -------------------------------------------------------------- display

    /// The row's rendered text fields, including the `full` span's condition.
    #[test]
    fn display_fields_match_the_render_rules() {
        let r = row("n1", "Widget", "N.Widget", "src/widget.rs", 3);
        assert_eq!(r.display_label(), "Widget", "label || name || id || file");
        assert_eq!(r.loc(), "src/widget.rs:3");
        assert_eq!(r.title(), "N.Widget  \u{2014}  src/widget.rs:3");

        // `full` equal to label suppresses the extra span (quickopen.js:332).
        let same = Row {
            full: "Widget".to_string(),
            ..r.clone()
        };
        assert_eq!(same.title(), "Widget  \u{2014}  src/widget.rs:3");

        // No file: the title is just the name, and loc falls back to the id.
        let nofile = Row {
            file: String::new(),
            full: String::new(),
            ..r.clone()
        };
        assert_eq!(nofile.title(), "Widget", "no file -> no dash suffix");
        assert_eq!(nofile.loc(), "n1", "no file -> the id");
    }

    #[test]
    fn kind_of_falls_back_to_usage_then_empty() {
        let mut r = row("n1", "x", "", "a.rs", 1);
        assert_eq!(r.kind_of(), "method");
        r.kind = String::new();
        assert_eq!(r.kind_of(), "", "non-usage with no kind renders blank");
        r.source = Source::Usage;
        assert_eq!(r.kind_of(), "usage");
    }

    /// The empty-list copy depends on `seedQuery`, not on the live input.
    #[test]
    fn empty_text_depends_on_the_seed_query() {
        assert_eq!(empty_text(""), "Type to search files and symbols.");
        assert_eq!(empty_text("abc"), "No matches.");
        assert!(seed_query_is_empty(""));
        assert!(!seed_query_is_empty("x"));
    }

    #[test]
    fn no_match_note_is_the_reference_sentence() {
        assert_eq!(
            no_match_note("widget"),
            "No files or symbols match \u{201c}widget\u{201d}."
        );
    }

    /// The id constants are the reference's own ids (quickopen.js:117,132,148,153).
    #[test]
    fn element_ids_are_the_reference_ids() {
        assert_eq!(OVERLAY_ID, "cs-quickopen");
        assert_eq!(INPUT_ID, "cs-qo-input");
        assert_eq!(LIST_ID, "cs-qo-list");
        assert_eq!(NOTE_ID, "cs-qo-note");
    }
}
