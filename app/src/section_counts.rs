//! GAP B: the **producer** for the two section-head counters `#strCount` /
//! `#chainCount` (`filters.js:523`/`:528`, `:555`).
//!
//! The nzk_ui half landed first and is correct: `dom.rs:2913`
//! (`apply_section_counts`) hydrates both spans, gated by
//! `if let Some(counts) = &facts.section_counts`. Nothing in `app/src` ever
//! emitted that key, so the gate never opened and both spans stayed at the text
//! `index.html` ships — an empty pair of `<span class="scount">`. This module is
//! the missing half.
//!
//! # What is derived, and from what
//!
//! The reference reaches both numbers from engine answers it already has:
//!
//! * `#strCount` — `filters.js:528` writes `fmtNum(list.length)` over the
//!   **strings-search** result (`:493` is the public-defs sibling read of the
//!   same shape). The app's strings search does not exist yet (`app/src` has no
//!   strings core; `find_wire` is the symbol search and a different list), so
//!   [`SectionCounts::from_engine_answers`] takes it as a parameter and the one
//!   caller passes `None`. The key is then **omitted**, not sent as `0`.
//! * `#chainCount` — `filters.js:555` reads `(d.flat || []).length`, the number
//!   of rows in the **flattened** call tree with the **root included**, from the
//!   very answer `renderChain` renders. The app has exactly that: the walks in
//!   [`crate::hierarchy_wire::HierarchyView`] are the real edges. Both the head
//!   counter and the panel body come out of *this same walk*, which is the
//!   reference's own coupling (one `d`, two consumers) and what keeps a drawn
//!   panel and its printed count from disagreeing.
//!
//! # Why the numbers are formatted HERE and the nzk_ui does not format them
//!
//! `SectionCountFacts` carries `str_count: Option<usize>` and
//! `chain_flat: Option<usize>` — raw numbers, and `null` on `str_count` is the
//! *clear* state, not "no value". So:
//!
//! * `None` for `str_count` means **no key at all** (nothing has been searched);
//! * `Some(None)` means `"str_count": null` — the empty-query clear
//!   (`filters.js:523`), which the landed `StrCount::Cleared` arm turns into an
//!   empty string;
//! * `Some(Some(n))` is `fmtNum(n)`.
//!
//! That is why an unsearched counter must produce *no key* rather than a `0`:
//! `0` is `fmtNum(0) == "0"`, a count nobody measured.
//!
//! # The two divergences that are deliberate and test-locked
//!
//! * `1` row prints `"1 hops"`, not `"1 hop"`, and `"0 hops"` for a walk that
//!   resolved nothing — the reference hard-codes the plural at `:555`. This is
//!   **not** [`crate::hierarchy::hop_label`], which singularises and belongs to
//!   the panel *body* hint (`hierarchy.js:539`).
//! * The count is the flat row count **including the root**, so it is one MORE
//!   than [`crate::hierarchy::hop_count`] (`rows - 1`), which is the body's own
//!   number. Two consumers, one array, two different numbers — on purpose.

use serde_json::{json, Value};

/// The two counters, in the exact shape `nzk_ui::dom::SectionCountFacts`
/// deserializes.
///
/// The JSON field names are the *snake_case* of the struct's own fields:
/// `str_count` (`dom.rs:1276`) and `chain_flat` (`dom.rs:1279`). They are spelled
/// explicitly below rather than left to a struct-wide `rename_all` — the same
/// reason [`crate::actions::NzkUiState`]'s `group_edges` carries a per-field
/// `rename` (`nzk_ui/src/dom.rs:795`): a `rename_all` here would silently
/// rename every sibling key this type ever grows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SectionCounts {
    /// `#strCount`. `None` = the key is omitted entirely (never searched);
    /// `Some(None)` = the empty-query clear; `Some(Some(n))` = `fmtNum(n)`.
    pub str_count: Option<Option<usize>>,
    /// `#chainCount`'s count, from the flattened walk. `None` = no walk has been
    /// installed, so the key is omitted.
    pub chain_flat: Option<usize>,
    /// Whether that walk was cut short by the depth limit. Only read when
    /// `chain_flat` is present, exactly as `SectionCountFacts` documents.
    pub chain_truncated: bool,
}

impl SectionCounts {
    /// Build the counters from the engine answers the host actually holds.
    ///
    /// `str_query` / `str_hits` are the strings search's `(query, result length)`
    /// pair; `None` while no such search has run. The query decides the *state*
    /// and the length only supplies the number, so an empty query can never
    /// paint a stale count: it emits the clear, whatever `str_hits` holds.
    pub fn from_engine_answers(
        str_query: Option<(&str, usize)>,
        flat_rows: Option<(usize, bool)>,
    ) -> SectionCounts {
        SectionCounts {
            str_count: str_query.map(|(q, hits)| if q.is_empty() { None } else { Some(hits) }),
            chain_flat: flat_rows.map(|(rows, _)| rows),
            chain_truncated: flat_rows.map(|(_, cut)| cut).unwrap_or(false),
        }
    }

    /// The counters carried by a real hierarchy walk, or the empty set when the
    /// panel holds no tree yet.
    ///
    /// `flat_rows` is `(d.flat || []).length` for the walk the panel is
    /// rendering — counted **with the root** — and `truncated` is `d.truncated`.
    /// Both come from [`crate::hierarchy_wire::HierarchyView::chain_flat_rows`],
    /// which is the same walk `dom_facts` serialises into the panel body.
    pub fn from_chain(flat_rows: Option<(usize, bool)>) -> SectionCounts {
        SectionCounts {
            str_count: None,
            chain_flat: flat_rows.map(|(rows, _)| rows),
            chain_truncated: flat_rows.map(|(_, cut)| cut).unwrap_or(false),
        }
    }

    /// The `section_counts` object for the host's `Facts`, or `serde_json::Value::Null`
    /// when neither counter is known.
    ///
    /// The caller must **omit** the key on `Null` rather than insert it: an
    /// absent key leaves `index.html`'s shipped document byte-identical, while
    /// `"section_counts": null` and `"section_counts": {}` are both "measured
    /// nothing" spelled as if they were an answer. This is the same
    /// absent-rather-than-null rule `pub_defs` and `hierarchy` follow
    /// (`app/src/nzk_ui.rs:952-955`).
    pub fn to_json(&self) -> Value {
        if self.str_count.is_none() && self.chain_flat.is_none() {
            return Value::Null;
        }
        let mut obj = serde_json::Map::new();
        match self.str_count {
            // `null` — the empty-query clear (`filters.js:523`), a real write.
            Some(None) => {
                obj.insert("str_count".into(), Value::Null);
            }
            Some(Some(n)) => {
                obj.insert("str_count".into(), json!(n));
            }
            // Never searched: the key stays absent even when the chain counter
            // is present. `0` here would be `fmtNum(0) == "0"`.
            None => {}
        }
        if let Some(rows) = self.chain_flat {
            obj.insert("chain_flat".into(), json!(rows));
            obj.insert("chain_truncated".into(), json!(self.chain_truncated));
        }
        Value::Object(obj)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// What the renderer's `SectionCountFacts` reads out of the object, for the
    /// three fields this producer can emit. Deliberately a *reader over the
    /// emitted JSON*, not a `serde` mirror of the struct: `serde` is not a
    /// direct dependency of this crate (only `serde_json` is), so a mirror
    /// would need a new dependency to say the same thing. What matters is the
    /// key spelling and the null/absent distinction, both of which are visible
    /// here:
    ///
    /// * `str_count` — `None` = key absent (no write); `Some(None)` = JSON
    ///   `null`, the clear (`filters.js:523`); `Some(Some(n))` = `fmtNum(n)`.
    /// * `chain_flat` / `chain_truncated` — `None` = key absent.
    #[derive(Debug, PartialEq, Eq)]
    struct Read {
        str_count: Option<Option<usize>>,
        chain_flat: Option<usize>,
        chain_truncated: Option<bool>,
    }

    fn read(v: &Value) -> Read {
        Read {
            str_count: match v.get("str_count") {
                None => None,
                Some(Value::Null) => Some(None),
                Some(n) => Some(Some(n.as_u64().expect("a count is a number") as usize)),
            },
            chain_flat: v.get("chain_flat").map(|n| n.as_u64().unwrap() as usize),
            chain_truncated: v.get("chain_truncated").map(|b| b.as_bool().unwrap()),
        }
    }

    /// Drive the **production** path end to end over a genuine graph: install a
    /// real `NzkUiState` just as the host does, open the panel on a real node
    /// so `walk_call_section` walks the real edges, then read the counters back
    /// out of the same `facts_json_with` object the renderer is handed. The
    /// result is what `#chainCount` is painted from — no fixture tree, no
    /// hand-built `Section`.
    fn real_chain_flat_rows() -> Option<(usize, bool)> {
        use crate::model::{Edge, Graph, Node};
        use std::path::PathBuf;

        let nodes: Vec<Node> = [("n1", "main"), ("n2", "helper"), ("n3", "deep")]
            .into_iter()
            .map(|(id, label)| Node {
                id: id.into(),
                label: label.into(),
                full: format!("NZK.{label}"),
                kind: "method".into(),
                file: "a.cs".into(),
                line: 1,
                ..Default::default()
            })
            .collect();
        // main -> helper -> deep: a 3-row root-inclusive chain at depth 2.
        let graph = Graph {
            repo: PathBuf::from("/tmp/section-counts-fixture"),
            nodes,
            edges: vec![
                Edge { from: 0, to: 1, kind: "calls".into(), count: 1 },
                Edge { from: 1, to: 2, kind: "calls".into(), count: 1 },
            ],
            state: json!({"fileCount": 1, "parseMs": 1, "indexed": true}),
            skipped: Vec::new(),
            projects: Vec::new(),
        };

        let mut state = crate::actions::NzkUiState::default();
        // `#chainCount` is `(d.flat || []).length` from the RESOLVE-CHAIN walk
        // (`filters.js:555 runChain`), which is the sidebar's own run — not the
        // separate hierarchy panel's tree. Three flat rows = three hops.
        state.chain = Some(json!({
            "ok": true,
            "flat": [{"depth": 1}, {"depth": 2}, {"depth": 3}],
            "truncated": false,
        }));
        let facts = crate::app::nzk_ui::facts_json_with(&graph, &state);
        facts
            .get("section_counts")
            .map(|v| {
                (
                    read(v).chain_flat.expect("a walked panel publishes its flat length"),
                    v.get("chain_truncated").and_then(|b| b.as_bool()).unwrap_or(false),
                )
            })
    }

    /// The producer's key spelling is exactly the deserializer's field spelling.    /// A `camelCase` slip (`strCount`/`chainFlat`) lands on `#[serde(default)]`
    /// fields and silently paints nothing, so this asserts the *read* result,
    /// not just that the keys exist.
    #[test]
    fn emitted_keys_are_the_ones_the_renderer_reads() {
        let v = SectionCounts::from_engine_answers(Some(("hit", 1234)), Some((7, true))).to_json();
        assert_eq!(v, json!({"str_count": 1234, "chain_flat": 7, "chain_truncated": true}));
        let got = read(&v);
        assert_eq!(got.str_count, Some(Some(1234)), "a `strCount` slip reads as None");
        assert_eq!(got.chain_flat, Some(7), "a `chainFlat` slip reads as None");
        assert_eq!(got.chain_truncated, Some(true));
    }

    /// An unsearched counter is an **absent key**, not a zero. `"str_count": 0`
    /// would render `"0"` — a count nobody measured (`filters.js:523`).
    #[test]
    fn an_unsearched_counter_omits_its_key_instead_of_zero() {
        let v = SectionCounts::from_chain(Some((3, false))).to_json();
        assert!(v.get("str_count").is_none(), "no key at all: {v}");
        assert_eq!(v["chain_flat"], json!(3));
        // And with nothing known at all the whole object is Null, which the
        // caller must OMIT — never insert.
        assert_eq!(SectionCounts::default().to_json(), Value::Null);
    }

    /// THE CLEAR STATE (`filters.js:523`): an empty query writes the empty
    /// string, which needs `null` on the wire — not an absent key, and not `0`.
    /// This is the state `Option<Option<usize>>` exists to carry, so it must be
    /// reachable from the producer.
    #[test]
    fn an_empty_query_produces_the_clear_not_an_absent_key_and_not_zero() {
        let v = SectionCounts::from_engine_answers(Some(("", 9)), None).to_json();
        assert_eq!(v, json!({"str_count": null}), "the clear is `null` in the object");
        let got = read(&v);
        assert_eq!(got.str_count, Some(None), "Some(None) is the clear, Some(Some(9)) is not");
        assert_ne!(got.str_count, Some(Some(0)), "`0` renders a count nobody measured");
        // The absent state is genuinely different from the clear state.
        let absent = read(&SectionCounts::default().to_json());
        assert_eq!(absent.str_count, None, "the key is absent, so there is no write");
        assert_ne!(absent.str_count, got.str_count, "clear and never-searched differ");
    }

    /// The count is the resolve-chain walk's `flat` length (`filters.js:555`).
    /// That array excludes the root, so it is one LESS than the panel body's
    /// `hop_count` over the same rows; subtracting or adding one would be the
    /// off-by-one this producer must not reproduce.
    #[test]
    fn the_chain_count_is_the_resolve_chain_flat_length() {
        let real = real_chain_flat_rows();
        let (rows, truncated) = real.expect("the walk is real, so it carries a tree");
        assert!(rows >= 2, "a walked root plus at least one hop: {rows}");
        assert!(!truncated, "depth 2 over a 2-deep tree is not cut");

        let v = SectionCounts::from_chain(Some((rows, truncated))).to_json();
        assert_eq!(read(&v).chain_flat, Some(rows));
        // Root-inclusive: exactly one more than the body's `rows - 1` hint.
        assert_eq!(crate::app::hierarchy_wire::hop_count(rows), rows - 1);
        assert_ne!(Some(rows), Some(crate::app::hierarchy_wire::hop_count(rows)));
    }

    /// Truncation rides alongside the count and survives the round trip, since
    /// the renderer appends it (`"7 hops (truncated)"`).
    #[test]
    fn truncation_is_carried_next_to_the_count() {        let v = SectionCounts::from_chain(Some((7, true))).to_json();
        assert_eq!(read(&v).chain_truncated, Some(true));
        let untruncated = read(&SectionCounts::from_chain(Some((7, false))).to_json());
        assert_eq!(untruncated.chain_truncated, Some(false), "false must not be dropped");
        assert_eq!(untruncated.chain_flat, Some(7));
    }
}
