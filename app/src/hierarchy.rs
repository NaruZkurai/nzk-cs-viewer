//! Call hierarchy, type hierarchy and go-to-implementation — the pure core.
//!
//! Mirrors `rust-only-csviewer/frontend/static/hierarchy.js` (736 lines), which
//! builds the *user-visible* hierarchy from the engine's own graph:
//!
//! * the **call tree** comes from `/api/chain` (`{id, depth, dir, max}`) and is
//!   normalized by `normChain` (`hierarchy.js:113-131`) into
//!   `{id,name,label,full,kind,file,line,edge,cycle,depth,children}`, then
//!   flattened pre-order by `flattenTree` (`hierarchy.js:133-139`);
//! * the **type tree** reads `/api/node` relations — `inherits:out` is "this
//!   type inherits from" (bases), `inherits:in` is "types that inherit from
//!   this" (derived) — plus a forward chain for the transitive base chain
//!   (`relationsOf` 193-203, `typeTreeFor` 205-215);
//! * `derivedTypes` (`hierarchy.js:163-191`) is a breadth-first walk over
//!   `inherits:in` guarded by a `seen` set, capped at `MAX_DERIVED_DEPTH`;
//! * the render layer names sections (`renderCall` 529-546, `renderType`
//!   597-...): "Outgoing calls" / "Incoming callers", `N hops` / `1 hop`,
//!   "truncated at N hops", "Nothing linked in this direction.",
//!   "No base type in this index."
//!
//! # Why this is a *core* and not a DOM builder
//!
//! There is no JS engine in this application: `nzk_ui/src/dom.rs` builds the
//! document in Rust and the styling stays in the real stylesheet. The traversal
//! and naming rules above are the part that must behave *exactly* like the
//! reference, so they live here as pure functions over plain data — no `egui`,
//! no I/O, no DOM. The wire layer ([`crate::hierarchy_wire`]) projects this to
//! the JSON the DOM layer deserializes, and `dom.rs` renders it.
//!
//! # Unfaithfulness, stated plainly
//!
//! `hierarchy.js` also drives three Monaco affordances that **cannot** be
//! reproduced here, because this application has no Monaco (it has zero
//! `monaco` references; the source pane is pure egui):
//!
//! * `registerProvider` (666) / `installActions` (677) wire the hierarchy into
//!   Monaco's context menu and a Shift+Alt+H keybinding;
//! * `provideImplementation`/`implementationsAt` (308-341) are a Monaco
//!   definition provider, using `editor`/`model` positions.
//!
//! What *is* portable — and what this module implements — is the graph
//! reasoning those affordances are built on: the candidacy rules
//! ([`implementation_targets`]) and the traversal/naming above.

use serde_json::{json, Value};

/// Edge kinds `hierarchy.js:36-39` labels inline on each hop.
pub const EDGE_LABELS: &[(&str, &str)] = &[
    ("calls", "calls"),
    ("constructs", "constructs"),
    ("uses", "uses"),
    ("assigns", "assigns"),
    ("inherits", "inherits"),
    ("contains", "contains"),
];

/// Node kinds that count as a *type* (`hierarchy.js:28-31`).
pub const TYPE_KINDS: &[&str] = &[
    "class",
    "struct",
    "interface",
    "enum",
    "record",
    "delegate",
    "type",
];

/// Node kinds that count as a *member* (`hierarchy.js:32-35`).
pub const MEMBER_KINDS: &[&str] = &[
    "method",
    "constructor",
    "operator",
    "property",
    "field",
    "event",
    "indexer",
];

/// `hierarchy.js:40` — the derived-type walk stops here.
pub const MAX_DERIVED_DEPTH: u32 = 3;

/// The label for an edge kind, verbatim; an unknown edge falls back to the raw
/// kind and a missing edge to `""` — this is `EDGE_LABELS[edge] || edge || ''`
/// (`hierarchy.js:476`).
pub fn edge_label(edge: Option<&str>) -> String {
    match edge {
        None => String::new(),
        Some(e) => EDGE_LABELS
            .iter()
            .find(|(k, _)| *k == e)
            .map(|(_, v)| (*v).to_string())
            .unwrap_or_else(|| e.to_string()),
    }
}

/// Is `kind` one of [`TYPE_KINDS`]?
pub fn is_type_kind(kind: &str) -> bool {
    TYPE_KINDS.contains(&kind)
}

/// Is `kind` one of [`MEMBER_KINDS`]?
pub fn is_member_kind(kind: &str) -> bool {
    MEMBER_KINDS.contains(&kind)
}

/// `sameMemberKind` (`hierarchy.js:218-222`).
///
/// Note the reference's exact semantics: a missing kind on *either* side is
/// treated as compatible (`true`), and two kinds are compatible when they are
/// both member kinds or both type kinds. It is deliberately **not** "equal
/// kind".
pub fn same_member_kind(a: Option<&str>, b: Option<&str>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return true;
    };
    if a == b {
        return true;
    }
    (is_member_kind(a) && is_member_kind(b)) || (is_type_kind(a) && is_type_kind(b))
}

/// One normalized hop, the Rust shape of `normChain`'s return
/// (`hierarchy.js:118-130`).
#[derive(Debug, Clone, PartialEq)]
pub struct Hop {
    pub id: String,
    pub name: String,
    pub label: String,
    pub full: String,
    pub kind: String,
    pub file: String,
    pub line: i64,
    /// The edge kind that led *to* this hop; `None` on the root.
    pub edge: Option<String>,
    /// The engine flagged this hop as already-seen (a cycle).
    pub cycle: bool,
    pub depth: u32,
    pub children: Vec<Hop>,
}

impl Hop {
    /// An empty hop — used to synthesize the base/derived rows `renderType`
    /// builds out of bare id lists (`hierarchy.js:619-622`).
    pub fn default_hop() -> Hop {
        Hop {
            id: String::new(),
            name: String::new(),
            label: String::new(),
            full: String::new(),
            kind: String::new(),
            file: String::new(),
            line: 0,
            edge: None,
            cycle: false,
            depth: 0,
            children: Vec::new(),
        }
    }
}

/// `labelOf` (`hierarchy.js:90-93`): label, else name, else id, else `?`.
pub fn label_of(node: &Value) -> String {
    for key in ["label", "name", "id"] {
        if let Some(s) = node.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    "?".to_string()
}

/// `normChain` (`hierarchy.js:113-131`).
///
/// A hop with no `node` object normalizes to `None` (`if (!t) return null`),
/// and children are filtered by `Boolean` — i.e. a null child is dropped rather
/// than becoming an empty hop. `line` prefers the hop's own `line` and only
/// falls back to the node's when it is absent (note the reference uses
/// `t.line != null ? t.line : n.line`, so a hop line of `0` wins over a node
/// line of `7`).
pub fn norm_chain(t: Option<&Value>, depth: u32) -> Option<Hop> {
    let t = t?;
    let n = t.get("node").unwrap_or(&Value::Null);
    if n.is_null() {
        return None;
    }
    let line = match t.get("line") {
        Some(v) if !v.is_null() => v.as_i64().unwrap_or(0),
        _ => n.get("line").and_then(|v| v.as_i64()).unwrap_or(0),
    };
    let children = t
        .get("children")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| norm_chain(Some(c), depth + 1))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(Hop {
        id: str_field(n, "id"),
        name: str_field(n, "name"),
        label: label_of(n),
        full: str_field(n, "full"),
        kind: str_field(n, "kind"),
        file: str_field(n, "file"),
        line,
        edge: t.get("edge").and_then(|v| v.as_str()).map(|s| s.to_string()),
        cycle: t.get("cycle").and_then(|v| v.as_bool()).unwrap_or(false),
        depth,
        children,
    })
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

/// `flattenTree` (`hierarchy.js:133-139`): pre-order, root first.
pub fn flatten_tree(root: &Hop) -> Vec<&Hop> {
    let mut out = Vec::new();
    fn walk<'a>(n: &'a Hop, out: &mut Vec<&'a Hop>) {
        out.push(n);
        for c in &n.children {
            walk(c, out);
        }
    }
    walk(root, &mut out);
    out
}

/// The hop count `renderCall` prints (`hierarchy.js:538`): rows minus the root.
pub fn hop_count(rows: usize) -> usize {
    rows.saturating_sub(1)
}

/// `count + (count === 1 ? ' hop' : ' hops')` (`hierarchy.js:539`).
pub fn hop_label(count: usize) -> String {
    if count == 1 {
        "1 hop".to_string()
    } else {
        format!("{count} hops")
    }
}

/// `truncated at N hops` (`hierarchy.js:542`).
pub fn truncated_label(count: usize) -> String {
    format!("truncated at {count} hops")
}

/// `N bases` / empty when there are none (`hierarchy.js:613`).
pub fn bases_label(count: usize) -> String {
    if count == 0 {
        String::new()
    } else {
        format!("{count} bases")
    }
}

/// The section title for a call-tree direction (`hierarchy.js:552`).
pub fn direction_label(dir: &str) -> &'static str {
    match dir {
        "backward" => "Incoming callers",
        _ => "Outgoing calls",
    }
}

/// The directions a call load expands to (`hierarchy.js:551`): `both` becomes
/// forward + backward, in that order.
pub fn expand_dirs(dir: &str) -> Vec<&'static str> {
    match dir {
        "both" => vec!["forward", "backward"],
        "backward" => vec!["backward"],
        _ => vec!["forward"],
    }
}

/// `state.dir` is one of these (`hierarchy.js:423`).
pub fn is_valid_dir(dir: &str) -> bool {
    matches!(dir, "both" | "forward" | "backward")
}

/// `state.depth` — the select offers 1..4 and the default is 2
/// (`hierarchy.js:433-439`).
pub fn clamp_depth(depth: u32) -> u32 {
    depth.clamp(1, 4)
}

/// `typeTreeFor` asks for depth 4 (`hierarchy.js:208`) regardless of the
/// user's call-tree depth.
pub const TYPE_CHAIN_DEPTH: u32 = 4;

/// `max` in the `/api/chain` call (`hierarchy.js:146`).
pub const CHAIN_MAX: u32 = 300;

/// The derived-type walk, `derivedTypes` (`hierarchy.js:163-191`).
///
/// Breadth-first over `inherits:in`, one level per round, skipping anything
/// already `seen` — including the seed itself, which is inserted before the
/// walk starts. `max_depth` is the `f.d >= maxDepth` cut. The result keeps
/// discovery order and has no duplicates.
///
/// `children_of` returns the `inherits:in` neighbours of a node id, and is the
/// injected I/O seam: the reference calls `/api/node` here, the caller here
/// passes whatever it has.
pub fn derived_types<F>(seed: &str, max_depth: u32, mut children_of: F) -> Vec<String>
where
    F: FnMut(&str) -> Vec<String>,
{
    let mut seen: Vec<String> = vec![seed.to_string()];
    let mut out: Vec<String> = Vec::new();
    let mut frontier: Vec<(String, u32)> = vec![(seed.to_string(), 0)];

    while !frontier.is_empty() {
        let mut next: Vec<(String, u32)> = Vec::new();
        for (id, d) in &frontier {
            if *d >= max_depth {
                continue;
            }
            for child in children_of(id) {
                if child.is_empty() || seen.iter().any(|s| s == &child) {
                    continue;
                }
                seen.push(child.clone());
                out.push(child.clone());
                next.push((child, d + 1));
            }
        }
        frontier = next;
    }
    out
}

/// How the type panel resolves one node's relations, the Rust shape of
/// `relationsOf` (`hierarchy.js:193-203`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Relations {
    /// `inherits:out` — "this type inherits from".
    pub bases: Vec<String>,
    /// `inherits:in` — "types that inherit from this".
    pub derived: Vec<String>,
}

/// `relationsOf`'s split of `/api/node` relation groups by `key`
/// (`hierarchy.js:197-200`).
pub fn relations_from_groups(groups: &Value) -> Relations {
    let mut rel = Relations::default();
    let Some(arr) = groups.as_array() else {
        return rel;
    };
    for g in arr {
        let key = g.get("key").and_then(|v| v.as_str()).unwrap_or_default();
        let ids = g
            .get("nodes")
            .and_then(|v| v.as_array())
            .map(|ns| {
                ns.iter()
                    .filter_map(|n| n.get("id").and_then(|v| v.as_str()))
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        match key {
            "inherits:out" => rel.bases = ids,
            "inherits:in" => rel.derived = ids,
            _ => {}
        }
    }
    rel
}

/// The `chainKids` filter `renderType` applies (`hierarchy.js:610-612`): the
/// chain walk also reports `uses`/`contains`, so only `inherits` children are
/// shown as bases.
pub fn chain_base_children(chain_root: &Hop) -> Vec<&Hop> {
    chain_root
        .children
        .iter()
        .filter(|k| k.edge.as_deref() == Some("inherits"))
        .collect()
}

/// `implementationTargets` membership test (`hierarchy.js:242-...`).
///
/// The reference derives implementation candidates from the node's own
/// relations and maps them through the language's `docLocFor`/`locForFile`,
/// preferring a location already on screen, and returns `[]` rather than a
/// guess. This is the part of that rule that is pure: a candidate is admitted
/// only when the id is non-empty, it is not the node itself, and its kind is
/// compatible under [`same_member_kind`].
pub fn implementation_candidates(
    node_id: &str,
    node_kind: Option<&str>,
    candidates: &[(String, Option<String>)],
) -> Vec<String> {
    candidates
        .iter()
        .filter(|(id, _)| !id.is_empty() && id != node_id)
        .filter(|(_, kind)| same_member_kind(node_kind, kind.as_deref()))
        .map(|(id, _)| id.clone())
        .collect()
}

/// The panel body's "nothing here" strings, kept in one place so the wire layer
/// and its tests agree on them (`hierarchy.js:533,545,601,624`).
pub mod empty {
    pub const NO_HIERARCHY: &str = "No hierarchy loaded.";
    pub const NOTHING_LINKED: &str = "Nothing linked in this direction.";
    pub const NO_TYPE: &str = "No type selected.";
    pub const NO_BASE: &str = "No base type in this index.";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn edge_label_falls_back_to_kind_then_empty() {
        assert_eq!(edge_label(Some("calls")), "calls");
        assert_eq!(edge_label(Some("inherits")), "inherits");
        // unknown edge -> the raw kind
        assert_eq!(edge_label(Some("weird")), "weird");
        // missing edge -> ""
        assert_eq!(edge_label(None), "");
    }

    #[test]
    fn kind_sets_match_the_reference_tables() {
        for k in TYPE_KINDS {
            assert!(is_type_kind(k), "{k} should be a type kind");
            assert!(!is_member_kind(k));
        }
        for k in MEMBER_KINDS {
            assert!(is_member_kind(k), "{k} should be a member kind");
            assert!(!is_type_kind(k));
        }
        assert!(!is_type_kind("method"));
        assert!(!is_member_kind("class"));
    }

    #[test]
    fn same_member_kind_treats_missing_as_compatible() {
        // A missing kind on either side is compatible — the reference returns
        // true there, and this is the case a naive equality check gets wrong.
        assert!(same_member_kind(None, Some("method")));
        assert!(same_member_kind(Some("method"), None));
        assert!(same_member_kind(None, None));
        // same kind is trivially compatible
        assert!(same_member_kind(Some("method"), Some("method")));
        // two member kinds are compatible even when different
        assert!(same_member_kind(Some("method"), Some("field")));
        // two type kinds likewise
        assert!(same_member_kind(Some("class"), Some("interface")));
        // a member and a type are NOT
        assert!(!same_member_kind(Some("method"), Some("class")));
        // an unknown kind against a member kind is NOT
        assert!(!same_member_kind(Some("banana"), Some("method")));
    }

    #[test]
    fn norm_chain_none_for_missing_node() {
        assert!(norm_chain(None, 0).is_none());
        // no `node` object at all -> null -> None
        assert!(norm_chain(Some(&json!({"line": 3})), 0).is_none());
    }

    #[test]
    fn norm_chain_maps_fields_and_depth() {
        let t = json!({
            "node": {"id": "A", "name": "alpha", "full": "N.A", "kind": "method",
                     "file": "a.cs", "line": 12},
            "edge": "calls", "cycle": true, "line": 99
        });
        let h = norm_chain(Some(&t), 2).expect("hop");
        assert_eq!(h.id, "A");
        assert_eq!(h.name, "alpha");
        assert_eq!(h.label, "alpha", "labelOf prefers label, then name");
        assert_eq!(h.full, "N.A");
        assert_eq!(h.kind, "method");
        assert_eq!(h.file, "a.cs");
        assert_eq!(h.line, 99, "the hop's own line wins over the node's");
        assert_eq!(h.edge.as_deref(), Some("calls"));
        assert!(h.cycle);
        assert_eq!(h.depth, 2);
        assert!(h.children.is_empty());
    }

    #[test]
    fn norm_chain_hop_line_zero_beats_node_line() {
        // `t.line != null ? t.line : n.line` — 0 is not null, so 0 wins.
        let t = json!({"node": {"id": "A", "line": 7}, "line": 0});
        assert_eq!(norm_chain(Some(&t), 0).unwrap().line, 0);
    }

    #[test]
    fn norm_chain_falls_back_to_node_line_when_hop_line_absent() {
        let t = json!({"node": {"id": "A", "line": 7}});
        assert_eq!(norm_chain(Some(&t), 0).unwrap().line, 7);
    }

    #[test]
    fn label_of_chain_is_label_then_name_then_id_then_question() {
        assert_eq!(label_of(&json!({"label": "L", "name": "N", "id": "I"})), "L");
        assert_eq!(label_of(&json!({"name": "N", "id": "I"})), "N");
        assert_eq!(label_of(&json!({"id": "I"})), "I");
        assert_eq!(label_of(&json!({})), "?");
        // empty strings do not count as present
        assert_eq!(label_of(&json!({"label": "", "name": "N"})), "N");
    }

    #[test]
    fn norm_chain_drops_null_children_but_keeps_depth() {
        let t = json!({
            "node": {"id": "root"},
            "children": [
                {"node": {"id": "kid"}},
                {"line": 5},                       // no node -> dropped
                {"node": {"id": "kid2"}, "children": [{"node": {"id": "grand"}}]}
            ]
        });
        let h = norm_chain(Some(&t), 0).unwrap();
        assert_eq!(h.children.len(), 2, "the node-less child is dropped");
        assert_eq!(h.children[0].id, "kid");
        assert_eq!(h.children[0].depth, 1);
        assert_eq!(h.children[1].id, "kid2");
        assert_eq!(h.children[1].children[0].id, "grand");
        assert_eq!(h.children[1].children[0].depth, 2);
    }

    #[test]
    fn flatten_tree_is_preorder_root_first() {
        let t = json!({
            "node": {"id": "r"},
            "children": [
                {"node": {"id": "a"}, "children": [{"node": {"id": "a1"}}]},
                {"node": {"id": "b"}}
            ]
        });
        let root = norm_chain(Some(&t), 0).unwrap();
        let ids: Vec<&str> = flatten_tree(&root).iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["r", "a", "a1", "b"]);
    }

    #[test]
    fn hop_count_and_labels() {
        assert_eq!(hop_count(0), 0, "saturating");
        assert_eq!(hop_count(1), 0, "a root alone is zero hops");
        assert_eq!(hop_count(5), 4);
        assert_eq!(hop_label(0), "0 hops");
        assert_eq!(hop_label(1), "1 hop", "singular");
        assert_eq!(hop_label(2), "2 hops");
        assert_eq!(truncated_label(7), "truncated at 7 hops");
        assert_eq!(bases_label(0), "", "no hint when there are no bases");
        assert_eq!(bases_label(1), "1 bases", "the reference does not singularise");
    }

    #[test]
    fn expand_dirs_order_is_forward_then_backward() {
        assert_eq!(expand_dirs("both"), vec!["forward", "backward"]);
        assert_eq!(expand_dirs("forward"), vec!["forward"]);
        assert_eq!(expand_dirs("backward"), vec!["backward"]);
        assert_eq!(expand_dirs("nonsense"), vec!["forward"], "default");
        assert_eq!(direction_label("forward"), "Outgoing calls");
        assert_eq!(direction_label("backward"), "Incoming callers");
        assert!(is_valid_dir("both"));
        assert!(!is_valid_dir("sideways"));
    }

    #[test]
    fn clamp_depth_bounds() {
        assert_eq!(clamp_depth(0), 1);
        assert_eq!(clamp_depth(1), 1);
        assert_eq!(clamp_depth(2), 2);
        assert_eq!(clamp_depth(4), 4);
        assert_eq!(clamp_depth(9), 4);
    }

    #[test]
    fn derived_types_walks_breadth_first_and_dedupes() {
        // a -> b, c ; b -> d ; c -> d (d seen via b first); d -> a (cycle back)
        let graph = |id: &str| -> Vec<String> {
            match id {
                "a" => vec!["b".into(), "c".into()],
                "b" => vec!["d".into()],
                "c" => vec!["d".into()],
                "d" => vec!["a".into()],
                _ => vec![],
            }
        };
        let got = derived_types("a", MAX_DERIVED_DEPTH, graph);
        assert_eq!(got, vec!["b", "c", "d"], "level order, d once, a never");
    }

    #[test]
    fn derived_types_excludes_the_seed_even_on_a_cycle() {
        let graph = |id: &str| -> Vec<String> {
            match id {
                "x" => vec!["x".into(), "y".into()],
                "y" => vec!["x".into()],
                _ => vec![],
            }
        };
        let got = derived_types("x", MAX_DERIVED_DEPTH, graph);
        assert_eq!(got, vec!["y"], "the seed is pre-seeded so it never reappears");
    }

    #[test]
    fn derived_types_respects_max_depth() {
        // a straight chain a->b->c->d->e
        let graph = |id: &str| -> Vec<String> {
            match id {
                "a" => vec!["b".into()],
                "b" => vec!["c".into()],
                "c" => vec!["d".into()],
                "d" => vec!["e".into()],
                _ => vec![],
            }
        };
        // depth 0: the frontier starts at d=0 and is immediately cut, so even
        // the first level is not walked.
        assert_eq!(derived_types("a", 0, graph), Vec::<String>::new());
        // depth 1: only b (the walk from the seed happens at d=0 < 1).
        assert_eq!(derived_types("a", 1, graph), vec!["b"]);
        // depth 2: b, c
        assert_eq!(derived_types("a", 2, graph), vec!["b", "c"]);
        // depth 3: b, c, d
        assert_eq!(derived_types("a", 3, graph), vec!["b", "c", "d"]);
    }

    #[test]
    fn derived_types_skips_empty_ids() {
        let graph = |_: &str| -> Vec<String> { vec![String::new(), "real".into()] };
        assert_eq!(derived_types("s", 2, graph), vec!["real"]);
    }

    #[test]
    fn relations_split_by_key() {
        let groups = json!([
            {"key": "inherits:out", "nodes": [{"id": "Base"}]},
            {"key": "inherits:in", "nodes": [{"id": "Sub1"}, {"id": "Sub2"}]},
            {"key": "calls:out", "nodes": [{"id": "Ignored"}]}
        ]);
        let rel = relations_from_groups(&groups);
        assert_eq!(rel.bases, vec!["Base"]);
        assert_eq!(rel.derived, vec!["Sub1", "Sub2"]);
        // a non-array yields the empty split rather than panicking
        assert_eq!(relations_from_groups(&json!(null)), Relations::default());
        assert_eq!(relations_from_groups(&json!({})), Relations::default());
    }

    #[test]
    fn relations_tolerate_missing_or_untyped_node_entries() {
        let groups = json!([
            {"key": "inherits:out", "nodes": [{"id": "A"}, {"nope": 1}, {"id": ""}, "junk"]},
            {"key": "inherits:in"}
        ]);
        let rel = relations_from_groups(&groups);
        assert_eq!(rel.bases, vec!["A"], "only non-empty string ids survive");
        assert!(rel.derived.is_empty());
    }

    #[test]
    fn chain_base_children_filters_to_inherits_only() {
        // The chain walk also reports uses/contains; renderType keeps inherits.
        let t = json!({
            "node": {"id": "r"},
            "children": [
                {"node": {"id": "b1"}, "edge": "inherits"},
                {"node": {"id": "u1"}, "edge": "uses"},
                {"node": {"id": "c1"}, "edge": "contains"},
                {"node": {"id": "b2"}, "edge": "inherits"}
            ]
        });
        let root = norm_chain(Some(&t), 0).unwrap();
        let kids = chain_base_children(&root);
        let ids: Vec<&str> = kids.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["b1", "b2"]);
        assert_eq!(bases_label(kids.len()), "2 bases");
    }

    #[test]
    fn implementation_candidates_excludes_self_empty_and_incompatible() {
        let cands = vec![
            ("self".to_string(), Some("method".to_string())),
            (String::new(), Some("method".to_string())),
            ("ok".to_string(), Some("method".to_string())),
            ("typed".to_string(), Some("class".to_string())),
        ];
        let got = implementation_candidates("self", Some("method"), &cands);
        // self excluded, empty excluded, another member kind admitted,
        // a type kind against a member kind rejected
        assert_eq!(got, vec!["ok"]);
    }

    #[test]
    fn implementation_candidates_missing_kind_admits() {
        let cands = vec![("a".to_string(), None), ("b".to_string(), Some("method".into()))];
        let got = implementation_candidates("self", Some("method"), &cands);
        assert_eq!(got, vec!["a", "b"], "a missing kind is compatible");
    }

    #[test]
    fn empty_strings_are_the_references_verbatim() {
        assert_eq!(empty::NO_HIERARCHY, "No hierarchy loaded.");
        assert_eq!(empty::NOTHING_LINKED, "Nothing linked in this direction.");
        assert_eq!(empty::NO_TYPE, "No type selected.");
        assert_eq!(empty::NO_BASE, "No base type in this index.");
    }
}
