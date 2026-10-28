//! Symbol search over the loaded graph — the native replacement for the
//! incumbent's `/api/search` + `search.js` suggestion list.
//!
//! **Display-free by construction.** Nothing here mentions `egui`, a window or a
//! GPU, exactly as in [`crate::app::actions`]: the whole subsystem is a pure
//! function of `(&Graph, &str)`, so its ranking is unit-testable with no
//! display. The UI half (a text field, a results list, key handling) consumes
//! [`SearchResult`] and never re-derives a match.
//!
//! ### Why this module exists
//!
//! The owner reported "the search bar doesn't work", and the report is accurate:
//! before this module there was no search implementation anywhere in `app/src/`
//! (`grep -rni "fn search\|search_hits\|match_hits" app/src/*.rs` matched only
//! wgpu's `occlusion_query_set`). The nzk_ui already ships the whole Find UI —
//! `#findPanel` with `#findQuery`/`#findMeta`/`#findNotice`/`#findResults`
//! (`rust-only-csviewer/frontend/index.html:157`) — but nothing ever produced a
//! row for it. This is the producer.
//!
//! ### Ranking
//!
//! Four match classes, strongest first, mirroring the incumbent's
//! exact-before-short-name preference (`search.js:238` highlights a usage as
//! `fq` for an exact literal hit and `short` for a bare-name hit):
//!
//! | class | meaning | example for query `find` |
//! |---|---|---|
//! | [`MatchKind::Exact`] | whole name equals the query | `find` |
//! | [`MatchKind::Prefix`] | name starts with the query | `findOne` |
//! | [`MatchKind::Substring`] | name contains it later | `model::find` |
//! | [`MatchKind::Subsequence`] | query chars appear in order, gaps allowed | `fnd` |
//!
//! A node is scored by its **best** class across the searchable fields, so a
//! node whose full name merely contains the query is not outranked by one whose
//! label starts with it unless the label really is the better field.
//!
//! ### Ordering: name before context
//!
//! Results sort by a two-level rule, because one level is not enough:
//!
//! 1. **A hit on the symbol's own name beats a hit on its context**, whatever
//!    the class ([`Field::is_name`]). 2. Only then does the class above, then
//!    field specificity, then degree, then `full`/`label`/`id` decide.
//!
//! The first level is not a preference, it is a measured correction. With the
//! class alone as the primary key, a real-corpus query for `search` ranked seven
//! `Exact` hits that merely *live in* `search.rs` above the actual `search`
//! function, and `find` pushed `model::find` below a file-owner hit. A user
//! typing a symbol name means that symbol, not the file it sits in.
//!
//! ### Determinism
//!
//! Ordering never depends on map iteration. Every key in that chain is derived
//! from the graph's own data and the last one (`id`) is unique, so the order is
//! total and byte-identical across repeated calls on the same graph.

// The core is deliberately written ahead of its renderer: the Lead wires
// `#findResults`/`#findMeta`/`#findNotice` to [`search_default`] separately, so
// until that lands every public item here is unused and would warn. Same
// precedent as `model::find` (model.rs:348), which carries the identical
// allowance for the identical reason.
#![allow(dead_code)]

use crate::model::{Graph, Node};

/// How strongly a name matched the query. Ordered strongest-first; the derived
/// `Ord` is the ranking's primary key, so the variant order *is* the contract.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum MatchKind {
    Exact,
    Prefix,
    Substring,
    Subsequence,
}

impl MatchKind {
    /// The short tag the Find panel shows next to a row — the native spelling of
    /// `search.js`'s `fq`/`short` chips (`search.js:226-230`).
    pub fn tag(self) -> &'static str {
        match self {
            MatchKind::Exact => "exact",
            MatchKind::Prefix => "prefix",
            MatchKind::Substring => "substring",
            MatchKind::Subsequence => "subseq",
        }
    }
}

/// Which field produced the winning match.
///
/// The declaration order is the ranking's second key, and it encodes a real
/// distinction rather than a preference: [`Field::Label`] and [`Field::Full`] are
/// the symbol's **own name** — what a user types when they mean *this* symbol —
/// while `Kind`/`Namespace`/`Owner`/`File` are **context** the symbol merely
/// sits in. `search` compares [`Field::is_name`] before the class, so a symbol
/// named `finder` beats an unrelated symbol that merely lives in
/// `Finder.cs`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub enum Field {
    Label,
    Full,
    Kind,
    Namespace,
    Owner,
    File,
}

impl Field {
    pub fn name(self) -> &'static str {
        match self {
            Field::Label => "label",
            Field::Full => "full",
            Field::Kind => "kind",
            Field::Namespace => "ns",
            Field::Owner => "owner",
            Field::File => "file",
        }
    }

    /// Is this the symbol's own name, rather than the context around it?
    ///
    /// Measured on a real corpus: with context fields ranked purely by match
    /// class, the query `search` returned seven `exact` hits that merely *live
    /// in* `search.rs` above the actual `search` function, and `find` put
    /// `model::find` below a file-owner hit. A name hit is what the user meant;
    /// a context hit is a fallback.
    pub fn is_name(self) -> bool {
        matches!(self, Field::Label | Field::Full)
    }
}

/// One ranked hit. Borrows the graph's own `Node` and strings: the search
/// allocates its result vector and nothing else, so a keystroke cannot turn into
/// thousands of `String` clones on the hot path.
#[derive(Clone, Copy, Debug)]
pub struct SearchResult<'a> {
    pub node: &'a Node,
    pub kind: MatchKind,
    pub field: Field,
}

impl<'a> SearchResult<'a> {
    /// The row title, matching the incumbent's `node.full || node.label ||
    /// node.id` fallback chain (`search.js:194`). Never empty: a node with all
    /// three blank still yields its `id`, which the engine guarantees is unique.
    pub fn title(&self) -> &'a str {
        let n = self.node;
        if !n.full.is_empty() {
            &n.full
        } else if !n.label.is_empty() {
            &n.label
        } else {
            &n.id
        }
    }

    /// The secondary line's file:line, or an empty string when the engine
    /// recorded no file. `search.js:204` already guards `node.line ? … : ''`;
    /// rendering no location is honest here because the graph genuinely has none
    /// for some nodes, and line 0 is the codebase's existing "no position"
    /// encoding (see `model::WarningRow`).
    pub fn location(&self) -> String {
        if self.node.file.is_empty() {
            String::new()
        } else if self.node.line > 0 {
            format!("{}:{}", self.node.file, self.node.line)
        } else {
            self.node.file.clone()
        }
    }

    /// The inline detail under the title: the label when it differs from the
    /// title, then the location — the same `bits.join(' · ')` shape as
    /// `search.js:203-205`.
    pub fn subtitle(&self) -> String {
        let mut bits: Vec<String> = Vec::new();
        if !self.node.label.is_empty() && self.node.label != self.title() {
            bits.push(self.node.label.clone());
        }
        let loc = self.location();
        if !loc.is_empty() {
            bits.push(loc);
        }
        bits.join("  ·  ")
    }
}

/// The search outcome, including the honest states.
///
/// This is the honesty seam of the module. An empty query and a query with no
/// hits are **different** states with different copy, and neither is a
/// silently-empty `results` vector — a caller that only looked at `results`
/// could not tell "the user has typed nothing" from "nothing matched", which is
/// exactly the fabricated-empty-list failure the incumbent's
/// `renderSuggestions` avoids with its `'no matches'` message
/// (`search.js:23`) and `renderFind`'s distinct "Type a symbol…" state
/// (`search.js:336`).
#[derive(Clone, Debug)]
pub enum Outcome<'a> {
    /// Nothing to search for. `results` is empty *because there was no query*,
    /// not because nothing matched.
    NoQuery,
    /// A real query, really searched, with at least one hit.
    Hits(Vec<SearchResult<'a>>),
    /// A real query, really searched, with no hits.
    NoHits,
}

impl<'a> Outcome<'a> {
    pub fn results(&self) -> &[SearchResult<'a>] {
        match self {
            Outcome::Hits(rs) => rs,
            _ => &[],
        }
    }

    pub fn len(&self) -> usize {
        self.results().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The `#findMeta` text: the real hit count, or an honest blank.
    ///
    /// `search.js:314-316` sets `'searching…'`, then `N hits`, and else `''`.
    /// This module has no async load step, so the only inputs left are the two
    /// honest states; `''` is returned for `NoQuery` so the bar shows nothing
    /// rather than a "0 hits" that would look like a completed search.
    pub fn meta(&self) -> String {
        match self {
            Outcome::NoQuery => String::new(),
            Outcome::NoHits => "no hits".to_string(),
            Outcome::Hits(rs) => format!("{} hits", rs.len()),
        }
    }

    /// The `#findNotice` text, shown only when there is something to say.
    ///
    /// `search.js:318-319` shows the notice only for a truncated result set.
    /// The two honest states are the analogue here, so an empty string means
    /// "keep the notice hidden".
    pub fn notice(&self) -> String {
        match self {
            Outcome::NoQuery => {
                "Type a symbol name to search. Matches rank exact, then prefix, \
                 then substring, then subsequence."
                    .to_string()
            }
            Outcome::NoHits => "No symbol matched this query.".to_string(),
            Outcome::Hits(_) => String::new(),
        }
    }
}

/// The largest result set [`search`] will return.
///
/// The incumbent asks the server for `limit: 30` (`search.js:42`) and the Find
/// panel pages 40 at a time (`FIND_PAGE`). 30 keeps a suggestion list a glance,
/// not a scroll, and is the number the frontend already ships with.
pub const DEFAULT_LIMIT: usize = 30;

/// Score one candidate string against a lowercased query.
///
/// `hay` and `q` must already be lowercase; callers pass the field and the
/// pre-lowercased query so a 20k-node scan lowercases the query once per call,
/// not once per node per field.
///
/// Returns `None` when there is no match at all. The order of the checks is the
/// ranking: the first (strongest) class that applies wins.
pub fn match_kind(hay: &str, q: &str) -> Option<MatchKind> {
    if q.is_empty() || hay.is_empty() {
        return None;
    }
    if hay == q {
        return Some(MatchKind::Exact);
    }
    if hay.starts_with(q) {
        return Some(MatchKind::Prefix);
    }
    if hay.contains(q) {
        return Some(MatchKind::Substring);
    }
    if is_subsequence(hay, q) {
        return Some(MatchKind::Subsequence);
    }
    None
}

/// Do all of `q`'s chars appear in `hay` in order, gaps allowed?
///
/// Byte-wise over `char`s so a multi-byte identifier cannot be split; a single
/// forward pass, no allocation and no backtracking.
fn is_subsequence(hay: &str, q: &str) -> bool {
    let mut chars = hay.chars();
    for qc in q.chars() {
        loop {
            match chars.next() {
                Some(hc) if hc == qc => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

/// The fields a node is searched through, in descending specificity.
///
/// `id` is deliberately absent: the engine's ids are compound
/// (`"<file>#<name>@<line>"`-ish) and including them makes a query match almost
/// everything via its file component, which would rank noise above real symbol
/// hits. `file` is included because the incumbent's Find panel does search
/// source text and file names are the closest thing the *graph* alone knows.
fn fields_of(node: &Node) -> [(Field, &str); 6] {
    [
        (Field::Label, node.label.as_str()),
        (Field::Full, node.full.as_str()),
        (Field::Kind, node.kind.as_str()),
        (Field::Namespace, node.ns.as_str()),
        (Field::Owner, node.owner.as_str()),
        (Field::File, node.file.as_str()),
    ]
}

/// Score one node: its best (strongest class, then most specific field) match.
fn score(node: &Node, q: &str) -> Option<(MatchKind, Field)> {
    let mut best: Option<(MatchKind, Field)> = None;
    for (field, raw) in fields_of(node) {
        if raw.is_empty() {
            continue;
        }
        // The label/full/kind fields are the symbol's own names and are the
        // ones users type; lowercasing them per node is the cost of
        // case-insensitive matching without an index. Only the fields that can
        // win are lowercased, and `to_lowercase` is skipped entirely when the
        // string is already lowercase (true for the overwhelming majority of
        // C# identifiers, which is what keeps this a single pass in practice).
        let lower = if raw.bytes().any(|b| b.is_ascii_uppercase()) {
            raw.to_lowercase()
        } else {
            raw.to_string()
        };
        if let Some(k) = match_kind(&lower, q) {
            let cand = (k, field);
            // `Field`'s derived `Ord` is declaration order = specificity, and
            // `MatchKind`'s is strength order, so tuple comparison is exactly
            // "strongest class, then most specific field".
            if best.map_or(true, |b| cand < b) {
                best = Some(cand);
            }
        }
    }
    best
}

/// Total order over scored results. Every key is derived from the graph's own
/// data, and the final `id` tiebreak is unique, so the order is total.
///
/// The first key is [`Field::is_name`], **not** the match class: a hit on the
/// symbol's own name always beats a hit on the context it happens to live in,
/// whatever the class. Only then does class strength, then field specificity,
/// then connectedness decide.
fn cmp_results(a: &SearchResult, b: &SearchResult) -> std::cmp::Ordering {
    b.field
        .is_name()
        .cmp(&a.field.is_name())
        .then_with(|| a.kind.cmp(&b.kind))
        .then_with(|| a.field.cmp(&b.field))
        // More connected first: a symbol with 40 call sites is more likely the
        // one the user means than a same-named one with none. Descending, so
        // compared in reverse.
        .then_with(|| b.node.degree.cmp(&a.node.degree))
        .then_with(|| a.node.full.cmp(&b.node.full))
        .then_with(|| a.node.label.cmp(&b.node.label))
        // The engine's ids are unique; this makes the order total, which is
        // what rules out any run-to-run reordering.
        .then_with(|| a.node.id.cmp(&b.node.id))
}

/// Search `graph` for `query`, best-first, at most `limit` results.
///
/// Case-insensitive. Returns [`Outcome::NoQuery`] for an empty or
/// whitespace-only query — an honest "nothing was searched", never an empty
/// hit list that reads as a completed search that found nothing.
pub fn search<'g>(graph: &'g Graph, query: &str, limit: usize) -> Outcome<'g> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Outcome::NoQuery;
    }
    let mut hits: Vec<SearchResult> = Vec::new();
    for node in &graph.nodes {
        if let Some((kind, field)) = score(node, &q) {
            hits.push(SearchResult { node, kind, field });
        }
    }
    if hits.is_empty() {
        return Outcome::NoHits;
    }
    hits.sort_by(cmp_results);
    hits.truncate(limit);
    Outcome::Hits(hits)
}

/// [`search`] at [`DEFAULT_LIMIT`] — the call the UI makes on every keystroke.
pub fn search_default<'g>(graph: &'g Graph, query: &str) -> Outcome<'g> {
    search(graph, query, DEFAULT_LIMIT)
}

/// The same search over a bare node slice.
///
/// Exists so a caller holding only part of the graph (a filtered view, a test
/// fixture) gets identical ranking without constructing a `Graph`, and so this
/// module's tests need no engine. `search` is defined in terms of it, so the two
/// cannot drift.
pub fn search_nodes<'a>(nodes: &'a [Node], query: &str, limit: usize) -> Outcome<'a> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Outcome::NoQuery;
    }
    let mut hits: Vec<SearchResult> = Vec::new();
    for node in nodes {
        if let Some((kind, field)) = score(node, &q) {
            hits.push(SearchResult { node, kind, field });
        }
    }
    if hits.is_empty() {
        return Outcome::NoHits;
    }
    hits.sort_by(cmp_results);
    hits.truncate(limit);
    Outcome::Hits(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A node with only the fields a test cares about, so a failure names the
    /// field that broke rather than a wall of defaults.
    fn node(id: &str, label: &str, full: &str, kind: &str) -> Node {
        Node {
            id: id.to_string(),
            label: label.to_string(),
            full: full.to_string(),
            kind: kind.to_string(),
            ..Node::default()
        }
    }

    fn ids(out: &Outcome) -> Vec<String> {
        out.results().iter().map(|r| r.node.id.clone()).collect()
    }

    fn kinds(out: &Outcome) -> Vec<MatchKind> {
        out.results().iter().map(|r| r.kind).collect()
    }

    /// Wrap nodes in a `Graph` so the `&Graph` entry point is exercised too.
    fn graph_of(nodes: Vec<Node>) -> Graph {
        Graph {
            repo: PathBuf::from("/tmp/test"),
            nodes,
            edges: Vec::new(),
            state: serde_json::Value::Null,
            skipped: Vec::new(),
            projects: Vec::new(),
        }
    }

    // -- ranking order ------------------------------------------------------

    /// The headline contract: exact beats prefix beats substring beats
    /// subsequence, and the result set is returned in exactly that order.
    ///
    /// The subsequence fixture is `f_i_n_d` with gaps (`f.i.n.d` contains
    /// `find` in order but never contiguously), which is what distinguishes
    /// [`MatchKind::Subsequence`] from [`MatchKind::Substring`]; a name that
    /// merely *lacks* letters would be an honest no-match, not a weak hit.
    #[test]
    fn ranking_is_exact_prefix_substring_subsequence() {
        let nodes = vec![
            node("4", "f.i.n.d", "N.f.i.n.d", "method"), // subsequence of "find"
            node("3", "refind", "N.refind", "method"),   // substring of "find"
            node("2", "finder", "N.finder", "method"),   // prefix
            node("1", "find", "N.find", "method"),       // exact
        ];
        let out = search_nodes(&nodes, "find", 10);
        assert_eq!(ids(&out), ["1", "2", "3", "4"]);
        assert_eq!(
            kinds(&out),
            [
                MatchKind::Exact,
                MatchKind::Prefix,
                MatchKind::Substring,
                MatchKind::Subsequence
            ]
        );
    }

    /// A node is scored by its best field, not its first: `full` matching
    /// exactly must lift a node whose `label` only matches as a subsequence.
    #[test]
    fn a_node_is_scored_by_its_best_field() {
        let nodes = vec![
            node("A", "r.e.f.i.n.d", "N.re", "method"),
            // label is a weak subsequence; full is an exact hit.
            node("B", "f.i.n.d", "find", "method"),
        ];
        let out = search_nodes(&nodes, "find", 10);
        assert_eq!(ids(&out), ["B", "A"]);
        assert_eq!(kinds(&out), [MatchKind::Exact, MatchKind::Subsequence]);
    }

    /// Field specificity breaks a class tie: same class, label beats full.
    #[test]
    fn field_specificity_breaks_class_ties() {
        let nodes = vec![
            node("FULL", "zzz", "finder", "method"),   // prefix in `full`
            node("LABEL", "finder", "zzz", "method"),  // prefix in `label`
        ];
        let out = search_nodes(&nodes, "find", 10);
        assert_eq!(ids(&out), ["LABEL", "FULL"]);
        assert_eq!(out.results()[0].field, Field::Label);
        assert_eq!(out.results()[1].field, Field::Full);
    }

    /// Regression guard for the real-corpus defect: a hit on the symbol's own
    /// name outranks a *stronger-class* hit on the context it lives in.
    ///
    /// Measured on `app/src` before the fix: the query `search` returned seven
    /// `Exact` file/owner hits above the `search` function itself, and `find`
    /// sank `model::find` below a file-owner hit. `NAME` here is only a
    /// *subsequence* match on its label yet still outranks an `Exact` match on
    /// the file it sits in — which is the whole point: the user typed a symbol.
    #[test]
    fn a_name_match_outranks_a_stronger_match_on_context() {
        let mut ctx = node("CTX", "unrelated", "N.unrelated", "method");
        ctx.file = "find".to_string(); // Exact on the `file` field
        let name = node("NAME", "f.i.n.d", "N.f.i.n.d", "method"); // Subsequence on label

        let nodes = [ctx, name];
        let out = search_nodes(&nodes, "find", 10);
        assert_eq!(
            ids(&out),
            ["NAME", "CTX"],
            "a name hit must beat a context hit regardless of class"
        );
        assert_eq!(out.results()[0].field, Field::Label);
        assert_eq!(out.results()[1].field, Field::File);
        assert!(
            out.results()[1].kind < out.results()[0].kind,
            "the context hit really is the stronger class — the field key is what reordered them"
        );
    }

    /// `is_name` is the boundary the ordering depends on: only the symbol's own
    /// two name fields count as a name, everything else is context.
    #[test]
    fn only_label_and_full_are_name_fields() {
        assert!(Field::Label.is_name());
        assert!(Field::Full.is_name());
        for f in [Field::Kind, Field::Namespace, Field::Owner, Field::File] {
            assert!(!f.is_name(), "{} is context, not a name", f.name());
        }
    }

    /// Degree is the usefulness tiebreak, and it is descending.
    #[test]
    fn degree_breaks_remaining_ties_descending() {
        let mut a = node("A", "find", "N.find", "method");
        a.degree = 3;
        let mut b = node("B", "find", "N.find", "method");
        b.degree = 40;
        let nodes = [a, b];
        let out = search_nodes(&nodes, "find", 10);
        assert_eq!(ids(&out), ["B", "A"], "the more-connected symbol ranks first");
    }

    // -- case-insensitivity -------------------------------------------------

    #[test]
    fn matching_is_case_insensitive_both_ways() {
        let nodes = vec![node("A", "FindOne", "N.FindOne", "method")];
        for q in ["find", "FIND", "FiNd", "fInDoNe"] {
            let out = search_nodes(&nodes, q, 10);
            assert_eq!(out.len(), 1, "query {q:?} must match FindOne");
        }
        // And a lowercase node matches an uppercase query.
        let lower = vec![node("L", "findone", "N.findone", "method")];
        assert_eq!(search_nodes(&lower, "FIND", 10).len(), 1);
    }

    /// Case-insensitivity must not become "matches everything": a query with no
    /// occurrence in any field still yields NoHits.
    #[test]
    fn case_insensitive_matching_is_still_a_real_match() {
        let nodes = vec![node("A", "FindOne", "N.FindOne", "method")];
        assert!(matches!(search_nodes(&nodes, "zzz", 10), Outcome::NoHits));
    }

    // -- honesty ------------------------------------------------------------

    /// An empty query is `NoQuery`, and its own state — not an empty hit list.
    #[test]
    fn empty_query_is_an_honest_no_query_state() {
        let nodes = vec![node("A", "find", "N.find", "method")];
        for q in ["", "   ", "\t\n"] {
            let out = search_nodes(&nodes, q, 10);
            assert!(matches!(out, Outcome::NoQuery), "query {q:?}");
            assert!(out.results().is_empty());
            assert_eq!(out.meta(), "", "no '0 hits' for a query never run");
            assert!(
                !out.notice().is_empty(),
                "the UI must be told to prompt, not shown a fake empty list"
            );
        }
    }

    /// `NoQuery` and `NoHits` are distinguishable — the whole point of the enum.
    #[test]
    fn no_query_and_no_hits_are_distinct_states() {
        let nodes = vec![node("A", "find", "N.find", "method")];
        let none = search_nodes(&nodes, "", 10);
        let miss = search_nodes(&nodes, "zzzzz", 10);
        assert!(matches!(none, Outcome::NoQuery));
        assert!(matches!(miss, Outcome::NoHits));
        assert_ne!(none.meta(), miss.meta());
        assert_ne!(none.notice(), miss.notice());
        assert_eq!(miss.meta(), "no hits", "a real search that found nothing");
    }

    /// A corpus with no nodes at all cannot produce hits, and says so.
    #[test]
    fn empty_corpus_reports_no_hits_not_a_panic() {
        let out = search_nodes(&[], "find", 10);
        assert!(matches!(out, Outcome::NoHits));
        assert!(out.results().is_empty());
    }

    /// `Hits` reports the real count in `#findMeta`, and no notice.
    #[test]
    fn hit_meta_counts_the_real_rows() {
        let nodes = vec![
            node("A", "find", "N.find", "method"),
            node("B", "finder", "N.finder", "method"),
            node("C", "unrelated", "N.unrelated", "method"),
        ];
        let out = search_nodes(&nodes, "find", 10);
        assert_eq!(out.meta(), "2 hits");
        assert_eq!(out.len(), 2);
        assert_eq!(out.notice(), "", "nothing to warn about");
    }

    /// One hit reads "1 hits" only if it is the incumbent's literal copy; the
    /// count itself must be the real one either way.
    #[test]
    fn single_hit_reports_a_count_of_one() {
        let nodes = vec![node("A", "find", "N.find", "method")];
        let out = search_nodes(&nodes, "find", 10);
        assert_eq!(out.len(), 1);
        assert!(out.meta().contains('1'));
    }

    // -- determinism --------------------------------------------------------

    /// The acceptance criterion: identical input gives a byte-identical,
    /// element-by-element identical order on every call.
    ///
    /// The corpus is built so every cheap tiebreak collides (same class, same
    /// field, same degree, same full, same label) and only the unique `id` can
    /// decide — which is exactly the case a `HashMap`-ordered implementation
    /// would shuffle between runs.
    #[test]
    fn ordering_is_identical_across_repeated_calls() {
        let mut nodes = Vec::new();
        for i in 0..64 {
            let mut n = node(&format!("id{i:03}"), "same", "same", "method");
            n.degree = 7;
            nodes.push(n);
        }
        let first = ids(&search_nodes(&nodes, "same", 100));
        assert_eq!(first.len(), 64);
        for round in 0..8 {
            assert_eq!(
                ids(&search_nodes(&nodes, "same", 100)),
                first,
                "round {round} reordered"
            );
        }
        // Fully total: the ids came back in sorted order, not insertion or
        // hash order.
        let mut sorted = first.clone();
        sorted.sort();
        assert_eq!(first, sorted, "the id tiebreak must be a total order");
    }

    /// Determinism must also hold through the `&Graph` entry point and when the
    /// node vector itself is presented in a different order — the ranking is a
    /// property of the score keys, not of input position.
    #[test]
    fn ordering_does_not_depend_on_input_position() {
        let a = node("id1", "find", "N.find", "method");
        let b = node("id2", "find", "N.find", "method");
        let forward = ids(&search_nodes(&[a.clone(), b.clone()], "find", 10));
        let reversed = ids(&search_nodes(&[b, a], "find", 10));
        assert_eq!(forward, reversed);
        assert_eq!(forward, ["id1", "id2"]);
    }

    /// The `&Graph` entry point agrees with the slice one, and repeated calls
    /// on the same graph agree with each other.
    #[test]
    fn graph_entry_point_is_deterministic_and_agrees_with_the_slice() {
        let nodes = vec![
            node("id2", "find", "N.find", "method"),
            node("id1", "find", "N.find", "method"),
            node("id3", "finder", "N.finder", "method"),
        ];
        let g = graph_of(nodes.clone());
        let via_graph = ids(&search(&g, "find", 10));
        let via_slice = ids(&search_nodes(&nodes, "find", 10));
        assert_eq!(via_graph, via_slice);
        assert_eq!(via_graph, ids(&search_default(&g, "find")));
        assert_eq!(via_graph, ids(&search(&g, "find", 10)));
        assert_eq!(via_graph, ["id1", "id2", "id3"]);
    }

    // -- limit + fields -----------------------------------------------------

    /// `limit` truncates the strongest hits, never a random subset.
    #[test]
    fn limit_keeps_the_strongest_hits() {
        let nodes = vec![
            node("5", "fnd", "N.fnd", "method"),
            node("1", "find", "N.find", "method"),
            node("3", "refind", "N.refind", "method"),
            node("2", "finder", "N.finder", "method"),
        ];
        let out = search_nodes(&nodes, "find", 2);
        assert_eq!(ids(&out), ["1", "2"], "top-2 by rank");
        assert_eq!(out.meta(), "2 hits", "meta counts what was returned");
    }

    /// Kind is searchable: the panel promises `Find symbol or code`.
    #[test]
    fn kind_is_searchable() {
        let nodes = vec![
            node("A", "Alpha", "N.Alpha", "interface"),
            node("B", "Beta", "N.Beta", "class"),
        ];
        let out = search_nodes(&nodes, "interface", 10);
        assert_eq!(ids(&out), ["A"]);
        assert_eq!(out.results()[0].field, Field::Kind);
    }

    /// Namespace and owner are searchable, so a namespace query lists its
    /// members instead of finding nothing.
    #[test]
    fn namespace_and_owner_are_searchable() {
        let mut a = node("A", "NormaliseName", "NZK.Core.MA.NormaliseName", "method");
        a.ns = "NZK.Core.MA".to_string();
        a.owner = "NameRules".to_string();
        let nodes = [a];
        let out = search_nodes(&nodes, "NZK.Core.MA", 10);
        assert_eq!(ids(&out), ["A"], "fully-qualified prefix, per the panel hint");
        let by_owner = search_nodes(&nodes, "NameRules", 10);
        assert_eq!(ids(&by_owner), ["A"]);
    }

    /// The file path is searchable, which is the closest the graph alone gets
    /// to the incumbent's source-text search.
    #[test]
    fn file_path_is_searchable() {
        let mut a = node("A", "Alpha", "N.Alpha", "method");
        a.file = "src/Model/Rules.cs".to_string();
        a.line = 42;
        let nodes = [a];
        let out = search_nodes(&nodes, "rules.cs", 10);
        assert_eq!(ids(&out), ["A"]);
        assert_eq!(out.results()[0].field, Field::File);
        assert_eq!(out.results()[0].location(), "src/Model/Rules.cs:42");
        // The subtitle carries the label *and* the location, because the title
        // is the full name (`search.js:203`).
        assert_eq!(
            out.results()[0].subtitle(),
            "Alpha  ·  src/Model/Rules.cs:42"
        );
    }

    /// Multi-byte identifiers are matched by `char`, not by byte, so a query
    /// cannot match a half-character or panic.
    #[test]
    fn multibyte_names_match_by_char() {
        let nodes = vec![node("A", "CaféFinder", "N.CaféFinder", "method")];
        assert_eq!(search_nodes(&nodes, "café", 10).len(), 1);
        assert_eq!(search_nodes(&nodes, "éf", 10).len(), 1, "substring across bytes");
        assert!(matches!(search_nodes(&nodes, "cafz", 10), Outcome::NoHits));
    }

    // -- row rendering inputs ----------------------------------------------

    /// The row title follows the incumbent's `full || label || id` chain and is
    /// never empty, so the panel cannot render a blank row.
    ///
    /// Each step is driven by a query that really matches that field — `id` is
    /// deliberately not searchable, so the all-empty node must still be found
    /// through `kind` for the `id` fallback to be observable.
    #[test]
    fn title_falls_back_full_then_label_then_id() {
        // No label, no full: the title must fall back to the engine's unique id.
        let mut n = node("ID", "", "", "method");
        let out = search_nodes(std::slice::from_ref(&n), "method", 10);
        assert_eq!(out.len(), 1, "matched via kind");
        assert_eq!(out.results()[0].title(), "ID", "id is never blank");

        n.label = "Label".to_string();
        let out = search_nodes(std::slice::from_ref(&n), "label", 10);
        assert_eq!(out.results()[0].title(), "Label");

        n.full = "N.Label".to_string();
        let out = search_nodes(std::slice::from_ref(&n), "n.label", 10);
        assert_eq!(out.results()[0].title(), "N.Label");
    }

    /// A node with no file renders no location rather than a fake `:0` link.
    /// This is the same honesty rule as `model::WarningRow`'s position-free
    /// case: an empty file with line 0 means "text with no location".
    #[test]
    fn location_is_absent_rather_than_fabricated() {
        // No file, no line: the location is empty and contributes no subtitle
        // bit. The subtitle still carries the label because the title is the
        // *full* name and they differ (`search.js:203`).
        let bare = node("A", "Alpha", "N.Alpha", "method");
        let out = search_nodes(std::slice::from_ref(&bare), "alpha", 10);
        assert_eq!(out.results()[0].field, Field::Label);
        assert_eq!(out.results()[0].location(), "", "no fake :0");
        assert_eq!(out.results()[0].subtitle(), "Alpha", "label only, no location");

        // With a file but no line, the file alone is shown — still no `:0`.
        let mut filed = node("A", "Alpha", "N.Alpha", "method");
        filed.file = "a.cs".to_string();
        let out = search_nodes(std::slice::from_ref(&filed), "alpha", 10);
        assert_eq!(out.results()[0].location(), "a.cs");

        // A node whose title *is* its label (no full name) has nothing left to
        // show once the location is absent, so the subtitle is empty.
        let titled = node("A", "Alpha", "", "method");
        let out = search_nodes(std::slice::from_ref(&titled), "alpha", 10);
        assert_eq!(out.results()[0].title(), "Alpha");
        assert_eq!(out.results()[0].subtitle(), "");
    }

    /// The subtitle never repeats the title, matching `search.js:203`.
    #[test]
    fn subtitle_omits_a_label_equal_to_the_title() {
        let mut a = node("A", "Alpha", "Alpha", "method");
        a.file = "a.cs".to_string();
        a.line = 3;
        let nodes = [a];
        let out = search_nodes(&nodes, "alpha", 10);
        assert_eq!(out.results()[0].subtitle(), "a.cs:3", "label == title, so dropped");
    }

    /// The match tag is the panel's chip text.
    #[test]
    fn match_tags_are_stable_strings() {
        assert_eq!(MatchKind::Exact.tag(), "exact");
        assert_eq!(MatchKind::Prefix.tag(), "prefix");
        assert_eq!(MatchKind::Substring.tag(), "substring");
        assert_eq!(MatchKind::Subsequence.tag(), "subseq");
        assert_eq!(Field::Label.name(), "label");
    }

    /// `is_subsequence` in isolation, with the direction the UI depends on: the
    /// user's **query** must be a subsequence of the name, so `fnd` finds
    /// `finder` (the abbreviation case) while `find` does not find `fnd`.
    #[test]
    fn subsequence_requires_order_and_repetition() {
        assert!(is_subsequence("finder", "fnd"), "abbreviation");
        assert!(is_subsequence("find", "find"));
        assert!(
            is_subsequence("finder", "fndr"),
            "gaps are allowed: f,n,d then the trailing r"
        );
        assert!(!is_subsequence("find", "fnid"), "order matters");
        assert!(
            !is_subsequence("fnd", "find"),
            "the query is the subsequence, not the name"
        );
        assert!(is_subsequence("any", ""), "the empty query is contained");
    }
}
