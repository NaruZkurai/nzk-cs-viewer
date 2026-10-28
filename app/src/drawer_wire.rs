//! Node-drawer content — v2 `frontend/static/drawer.js`.
//!
//! v2 fills the docked `#nodeDrawer` from `GET /api/node` + `/api/relation`:
//! the selected declaration's identity and location, its property chips, and
//! one section per relation bucket (callers / used by / children / …) with
//! clickable neighbour rows (`drawer.js:74-360`). This fork already owns the
//! loaded [`Graph`] — nodes and attributed edges — so the content is derived
//! **in process** from that graph: no HTTP, no async, and no second relation
//! model.
//!
//! What this module produces is the honest subset the v3 model carries:
//! * identity: `label` (fallback `full`/`id`), `kind`, `full` as the title
//!   tooltip;
//! * location: root-relative `file:line(-endLine)` plus the absolute path for
//!   the tooltip;
//! * property rows for the fields [`Node`] really has (`ns`, `owner`, `file`,
//!   `line`/`end_line`, `degree`) and flags for `external`;
//! * relation sections from the graph's own edges, split by direction and kind
//!   (`calls` in/out, `uses`/`assigns`/`constructs` in/out, `inherits`,
//!   `contains`), each with a real count and clickable row ids.
//!
//! Fields v2 reads from `/api/node` that this model does not carry (the raw
//! `signature`, `access`, `static`, `literal`) are deliberately **absent**
//! rather than fabricated. Nothing selected (or a stale selection index) is
//! `None`, which is the honest empty state: the drawer keeps its shipped
//! `hidden` class and the default render stays byte-identical.

use crate::model::{Graph, Node};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Cap on rows carried per relation bucket (the count stays the real one).
pub const ROWS_PER_BUCKET: usize = 200;

/// Ancestry walk bound — v2 `renderCrumbs`'s `guard++ < 20`
/// (`frontend/static/interaction.js:381`).
pub const MAX_CRUMB_HOPS: usize = 20;

/// The drawer's own interactive state that the graph does not carry.
///
/// v2 keeps two Sets outside `/api/node` and re-renders the drawer from them:
/// `state.relations.listOpenByNode` (which relation groups are expanded) and
/// `state.hiddenChildren` (the "Hide children" scope, `drawer.js:120-145`).
/// This is the pure, transportable model of both; the app owns one instance and
/// the facts are a function of it. Additive: [`facts`] is the default-state
/// builder, so an existing consumer keeps its exact output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrawerUi {
    /// Node ids whose contains-children are hidden (`state.hiddenChildren`).
    pub hidden_children: BTreeSet<String>,
    /// `node id -> expanded relation bucket keys` (`listOpenByNode`).
    pub open_lists: BTreeMap<String, BTreeSet<String>>,
}

impl DrawerUi {
    /// v2 `relationListOpen`: a group is collapsed unless it was opened.
    pub fn group_open(&self, node: &str, key: &str) -> bool {
        self.open_lists.get(node).is_some_and(|s| s.contains(key))
    }

    /// v2 `toggleRelationList`: flip one group's expanded state.
    pub fn toggle_group(&mut self, node: &str, key: &str) -> bool {
        let set = self.open_lists.entry(node.to_string()).or_default();
        if set.contains(key) {
            set.remove(key);
            false
        } else {
            set.insert(key.to_string());
            true
        }
    }

    /// v2 `openRelationList`: open one group (no-op when already open).
    pub fn open_group(&mut self, node: &str, key: &str) {
        self.open_lists
            .entry(node.to_string())
            .or_default()
            .insert(key.to_string());
    }

    /// The `Hide children` / `Show children` toggle
    /// (`drawer.js:99`, `state.hiddenChildren` add/delete).
    pub fn toggle_hidden_children(&mut self, node: &str) -> bool {
        if self.hidden_children.contains(node) {
            self.hidden_children.remove(node);
            false
        } else {
            self.hidden_children.insert(node.to_string());
            true
        }
    }

    /// v2 `Expand`: every relation group of `node` becomes expanded. Returns
    /// the number of groups opened.
    ///
    /// This is the drawer's half only. The canvas half — actually pulling every
    /// relation bucket onto the graph — belongs to the host, which calls
    /// `GraphView::expand_node_all` with the same node. Keeping the two in one
    /// press is what makes the button "Add every relation bucket" rather than
    /// merely "show the lists".
    pub fn expand_all<'a>(&mut self, node: &str, keys: impl Iterator<Item = &'a str>) -> usize {
        let set = self.open_lists.entry(node.to_string()).or_default();
        let mut n = 0;
        for key in keys {
            if set.insert(key.to_string()) {
                n += 1;
            }
        }
        n
    }
}

/// Ancestry crumbs for `index`: walk `contains` parents upward (bounded) and
/// return them root-first, **omitting the selected node** — the drawer header
/// already names it (`interaction.js:375-409`).
pub fn crumbs(graph: &Graph, index: usize) -> Vec<Value> {
    let mut ids: Vec<usize> = Vec::new();
    let mut cur = index;
    for _ in 0..MAX_CRUMB_HOPS {
        let Some(parent) = graph
            .edges
            .iter()
            .find(|e| e.to == cur && e.kind == "contains")
            .map(|e| e.from)
        else {
            break;
        };
        let Some(pn) = graph.nodes.get(parent) else { break };
        let _ = pn;
        ids.push(parent);
        cur = parent;
    }
    ids.reverse();
    ids.iter()
        .filter_map(|&i| graph.nodes.get(i))
        .map(|n| {
            json!({
                "id": n.id,
                "label": title_of(n),
                "title": if n.full.is_empty() { n.id.clone() } else { n.full.clone() },
            })
        })
        .collect()
}

/// The `.prop-actions` row (`drawer.js:96-102`), in shipping order. `Expand`
/// always; `Hide children`/`Show children` reflects the scope; `Resolve chain`
/// only when the node has a source position. `View code` is the shipped header
/// button (`#nodeCodeBtn`), not a row action in the reference.
fn actions_of(
    node: &Node,
    ui: &DrawerUi,
    focus_only: bool,
    focus_parents: bool,
) -> Vec<Value> {
    let hidden = ui.hidden_children.contains(&node.id);
    let mut out = vec![
        json!({"act": "expand", "label": "Expand",
               "title": "Expand every relation group"}),
        json!({"act": "hidech",
               "label": if hidden { "Show children" } else { "Hide children" },
               "title": "Hide or show the contains children of this node"}),
        // Contextual focus. These are one press away from the node they act on,
        // which is what makes decluttering a 9k-node view practical. They write
        // the one filter model (`FilterState::focus_only` / `focus_parents`) and
        // live only here: the sidebar duplicates were removed because the
        // filter is anchored on the selection right beside them.
        json!({"act": "focus",
               "label": if focus_only { "Show all nodes" } else { "Only related nodes" },
               "title": "Draw only this node and the nodes it relates to (press again to show everything)"}),
        json!({"act": "parents",
               "label": if focus_parents { "Show parents" } else { "Hide parents" },
               "title": "Drop the containment parents (the Parent chain) from the canvas"}),
    ];
    if !node.file.is_empty() && node.line > 0 {
        out.push(json!({"act": "chain", "label": "Resolve chain",
                        "title": "Walk the resolve chain from this declaration"}));
    }
    out
}

/// Root-relative form of `file` when it sits under the indexed repo root.
pub fn short_file(repo: &std::path::Path, file: &str) -> String {
    if file.is_empty() {
        return String::new();
    }
    let root = repo.to_string_lossy();
    if !root.is_empty() {
        if let Some(rest) = file.strip_prefix(root.as_ref()) {
            let rest = rest.trim_start_matches(['/', '\\']);
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    file.to_string()
}

/// `file:line` / `file:line–endLine`, or `""` when the node has no file.
fn location(node: &Node, file: &str) -> String {
    if file.is_empty() {
        return String::new();
    }
    if node.line <= 0 {
        return file.to_string();
    }
    let mut loc = format!("{file}:{}", node.line);
    if node.end_line > node.line {
        loc.push('\u{2013}');
        loc.push_str(&node.end_line.to_string());
    }
    loc
}

/// The identity line v2 puts in `#nodeTitle` (`name || label || id`).
fn title_of(node: &Node) -> String {
    if !node.label.is_empty() {
        return node.label.clone();
    }
    if !node.full.is_empty() {
        return node.full.clone();
    }
    node.id.clone()
}

/// The relation bucket an edge kind maps to, or `None` for an unmodelled kind.
///
/// Keys/labels follow the reference's `/api/relation` vocabulary where the
/// graph carries the same relation (`drawer.js:216-360`); an unknown edge kind
/// is still shown, under the generic "Referenced by"/"References" buckets,
/// rather than dropped silently.
fn bucket_of(kind: &str, incoming: bool) -> Option<(&'static str, &'static str)> {
    match (kind, incoming) {
        ("calls", true) => Some(("callers", "Callers")),
        ("calls", false) => Some(("calls", "Calls")),
        ("uses", true) | ("assigns", true) | ("constructs", true) => {
            Some(("usedby", "Used by"))
        }
        ("uses", false) => Some(("uses", "Uses")),
        ("assigns", false) => Some(("assigns", "Assigns")),
        ("constructs", false) => Some(("constructs", "Constructs")),
        ("inherits", true) => Some(("inheritedby", "Inherited by")),
        ("inherits", false) => Some(("inherits", "Inherits")),
        ("contains", true) => Some(("parent", "Parent")),
        ("contains", false) => Some(("children", "Children")),
        (_, true) => Some(("refs_in", "Referenced by")),
        (_, false) => Some(("refs_out", "References")),
    }
}

/// The canonical section order: the reference lists callers first, then the
/// usage buckets, then containment (`drawer.js:216-360`). Deterministic, so a
/// repaint cannot reorder rows.
const ORDER: [(&str, &str); 12] = [
    ("callers", "Callers"),
    ("calls", "Calls"),
    ("usedby", "Used by"),
    ("uses", "Uses"),
    ("assigns", "Assigns"),
    ("constructs", "Constructs"),
    ("inheritedby", "Inherited by"),
    ("inherits", "Inherits"),
    ("parent", "Parent"),
    ("children", "Children"),
    ("refs_in", "Referenced by"),
    ("refs_out", "References"),
];

/// The relation bucket keys [`facts`] would produce for `index`, in shipping
/// order. Used by the drawer's `Expand` action to open every group without
/// re-deriving the buckets a second way.
pub fn relation_keys(graph: &Graph, index: usize) -> Vec<&'static str> {
    let mut keys: Vec<&'static str> = Vec::new();
    for e in &graph.edges {
        let incoming = if e.from == index {
            false
        } else if e.to == index {
            true
        } else {
            continue;
        };
        let Some((key, _)) = bucket_of(&e.kind, incoming) else {
            continue;
        };
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    keys
}

/// The drawer content for the selected node, in the `ui.drawer` wire shape.
///
/// Pure: the graph supplies both the identity and every relation row. This is
/// the *default-state* builder (no group expanded, no children hidden), so
/// existing consumers keep their exact facts; the app uses [`facts_with`] with
/// its [`DrawerUi`].
pub fn facts(graph: &Graph, index: usize) -> Option<Value> {
    facts_with(graph, index, &DrawerUi::default())
}

/// [`facts`] with the drawer's own [`DrawerUi`] applied to the additive fields
/// (`crumbs`, `actions`, per-relation `open`/`hidden`). Additive by design: an
/// absent `DrawerUi` is the default and produces the same pre-existing keys.
pub fn facts_with(graph: &Graph, index: usize, ui: &DrawerUi) -> Option<Value> {
    facts_with_focus(graph, index, ui, false, false)
}

fn facts_with_focus(
    graph: &Graph,
    index: usize,
    ui: &DrawerUi,
    focus_only: bool,
    focus_parents: bool,
) -> Option<Value> {
    let node = graph.nodes.get(index)?;
    let repo = graph.repo.as_path();
    let file = short_file(repo, &node.file);
    let location_short = location(node, &file);
    let location_full = location(node, &node.file);

    let mut flags: Vec<String> = Vec::new();
    if node.external {
        flags.push("external".to_string());
    }
    flags.push(format!("deg {}", node.degree));

    let mut props: Vec<Value> = Vec::new();
    if !node.ns.is_empty() {
        props.push(json!({"k": "Namespace", "v": node.ns}));
    }
    if !node.owner.is_empty() {
        props.push(json!({"k": "Owner", "v": node.owner}));
    }
    if !file.is_empty() {
        props.push(json!({"k": "File", "v": file}));
    }
    if node.line > 0 {
        let lines = if node.end_line > node.line {
            format!("{}–{}", node.line, node.end_line)
        } else {
            node.line.to_string()
        };
        props.push(json!({"k": "Lines", "v": lines}));
    }
    props.push(json!({"k": "Degree", "v": node.degree.to_string()}));

    // One bucket per canonical relation, filled from the graph's attributed
    // edges. Rows are the neighbour nodes; the count is the true edge count for
    // the section, even when the row list is capped.
    let mut rows: Vec<(&'static str, &'static str, Vec<&Node>, usize)> = Vec::new();
    for e in &graph.edges {
        let (other, incoming) = if e.from == index {
            (e.to, false)
        } else if e.to == index {
            (e.from, true)
        } else {
            continue;
        };
        let Some((key, label)) = bucket_of(&e.kind, incoming) else {
            continue;
        };
        let Some(neighbour) = graph.nodes.get(other) else { continue };
        match rows.iter_mut().find(|(k, _, _, _)| *k == key) {
            Some((_, _, nodes, count)) => {
                *count += 1;
                if nodes.len() < ROWS_PER_BUCKET {
                    nodes.push(neighbour);
                }
            }
            None => rows.push((key, label, vec![neighbour], 1)),
        }
    }
    rows.sort_by_key(|(key, _, _, _)| ORDER.iter().position(|(k, _)| k == key).unwrap_or(usize::MAX));
    let relation_total: usize = rows.iter().map(|(_, _, _, c)| c).sum();
    let relations: Vec<Value> = rows
        .iter()
        .map(|(key, label, nodes, count)| {
            let mut seen = std::collections::BTreeSet::new();
            let listed: Vec<Value> = nodes
                .iter()
                .filter(|n| seen.insert(n.id.clone()))
                .map(|n| json!({"id": n.id, "label": n.label, "kind": n.kind}))
                .collect();
            // v2 `renderRelations`: a group is open only while it was toggled on
            // (`relationListOpen`), and the contains-children group is forced
            // shut by `state.hiddenChildren` (`.hidden-children`).
            let hidden = *key == "children" && ui.hidden_children.contains(&node.id);
            let open = ui.group_open(&node.id, key) && !hidden;
            json!({"key": key, "label": label, "count": count, "rows": listed,
                   "open": open, "hidden": hidden})
        })
        .collect();

    Some(json!({
        "id": node.id,
        "title": title_of(node),
        "title_attr": if node.full.is_empty() { node.id.clone() } else { node.full.clone() },
        "kind": node.kind,
        "location": location_short,
        "location_full": location_full,
        "flags": flags,
        "props": props,
        "crumbs": crumbs(graph, index),
        "actions": actions_of(node, ui, focus_only, focus_parents),
        "relations": relations,
        "relation_total": relation_total,
    }))
}

/// The drawer content for a selection index, `None` when nothing is selected or
/// the index is stale. The honest empty state: no facts = no drawer.
pub fn facts_for(graph: &Graph, selected: Option<usize>) -> Option<Value> {
    selected.and_then(|i| facts(graph, i))
}

/// [`facts_for`] with an explicit [`DrawerUi`] — what the nzk_ui publishes.
pub fn facts_for_with(
    graph: &Graph,
    selected: Option<usize>,
    ui: &DrawerUi,
) -> Option<Value> {
    facts_for_focus(graph, selected, ui, false, false)
}

/// [`facts_for_with`] plus the two view focus flags, so the drawer's `Only
/// related nodes` / `Hide parents` actions can show their own pressed state.
pub fn facts_for_focus(
    graph: &Graph,
    selected: Option<usize>,
    ui: &DrawerUi,
    focus_only: bool,
    focus_parents: bool,
) -> Option<Value> {
    selected.and_then(|i| facts_with_focus(graph, i, ui, focus_only, focus_parents))
}

#[cfg(test)]
mod drawer_tests {
    use super::*;
    use crate::model::{Edge, Graph, Node};
    use std::path::PathBuf;

    fn node(id: &str, label: &str, kind: &str) -> Node {
        Node {
            id: id.to_string(),
            label: label.to_string(),
            full: format!("App.{label}"),
            kind: kind.to_string(),
            ns: "App".to_string(),
            owner: "App".to_string(),
            file: "/repo/src/App.cs".to_string(),
            line: 10,
            end_line: 20,
            degree: 3,
            external: false,
            ..Default::default()
        }
    }

    fn graph() -> Graph {
        Graph {
            repo: PathBuf::from("/repo"),
            nodes: vec![
                node("n:1", "Run", "method"),
                node("n:2", "Helper", "method"),
                node("n:3", "Box", "class"),
            ],
            edges: vec![
                // n:2 calls n:1 -> an incoming caller of Run.
                Edge { from: 1, to: 0, kind: "calls".to_string(), count: 1 },
                // Run contains nothing, but Box's edge makes a children bucket.
                Edge { from: 2, to: 0, kind: "contains".to_string(), count: 1 },
                // Run uses Helper -> an outgoing "Uses" bucket.
                Edge { from: 0, to: 1, kind: "uses".to_string(), count: 1 },
            ],
            state: json!({}),
            skipped: Vec::new(),
            projects: Vec::new(),
        }
    }

    #[test]
    fn nothing_selected_is_the_honest_empty_state() {
        let g = graph();
        assert!(facts_for(&g, None).is_none());
        assert!(facts_for(&g, Some(99)).is_none(), "a stale index is not a node");
    }

    #[test]
    fn identity_location_and_properties_come_from_the_real_node() {
        let g = graph();
        let f = facts_for(&g, Some(0)).expect("the selected node has facts");
        assert_eq!(f["title"], json!("Run"));
        assert_eq!(f["title_attr"], json!("App.Run"));
        assert_eq!(f["kind"], json!("method"));
        assert_eq!(f["location"], json!("src/App.cs:10–20"));
        assert_eq!(f["location_full"], json!("/repo/src/App.cs:10–20"));
        assert_eq!(f["flags"], json!(["deg 3"]));
        let props = f["props"].as_array().expect("prop rows");
        assert_eq!(props.len(), 5, "namespace, owner, file, lines, degree: {props:?}");
        assert_eq!(props[0], json!({"k": "Namespace", "v": "App"}));
        assert_eq!(props[3], json!({"k": "Lines", "v": "10–20"}));
    }

    #[test]
    fn relations_are_bucketed_by_direction_and_kind_with_real_counts() {
        let g = graph();
        let f = facts_for(&g, Some(0)).expect("facts");
        let rels = f["relations"].as_array().expect("relation sections");
        let keys: Vec<&str> = rels.iter().filter_map(|r| r["key"].as_str()).collect();
        // Direction matters: n:2->n:1 calls/contains are INCOMING (callers,
        // parent); n:1->n:2 uses is OUTGOING (uses).
        assert_eq!(keys, ["callers", "uses", "parent"], "canonical order");
        assert_eq!(rels[0]["label"], json!("Callers"));
        assert_eq!(rels[0]["count"], json!(1));
        assert_eq!(rels[0]["rows"][0]["label"], json!("Helper"));
        assert_eq!(rels[1]["label"], json!("Uses"));
        assert_eq!(rels[2]["label"], json!("Parent"));
        assert_eq!(f["relation_total"], json!(3));
    }

    #[test]
    fn a_node_with_no_edges_has_no_relation_sections_and_a_zero_total() {
        let mut g = graph();
        g.edges.clear();
        let f = facts_for(&g, Some(0)).expect("facts");
        assert!(f["relations"].as_array().unwrap().is_empty());
        assert_eq!(f["relation_total"], json!(0));
    }

    #[test]
    fn the_external_flag_is_reported_and_caps_never_lie_about_the_count() {
        let mut g = graph();
        g.nodes[0].external = true;
        for i in 0..(ROWS_PER_BUCKET + 5) {
            let id = format!("n:x{i}");
            g.nodes.push(Node { id: id.clone(), ..node(&id, "X", "method") });
            let from = g.nodes.len() - 1;
            g.edges.push(Edge { from, to: 0, kind: "calls".to_string(), count: 1 });
        }
        let f = facts_for(&g, Some(0)).expect("facts");
        assert_eq!(f["flags"][0], json!("external"));
        let callers = &f["relations"][0];
        assert_eq!(
            callers["count"],
            json!((ROWS_PER_BUCKET + 5 + 1) as i64), // + the fixture's own callers edge
            "the count is the real number of edges"
        );
        assert_eq!(
            callers["rows"].as_array().unwrap().len(),
            ROWS_PER_BUCKET,
            "the row list is capped"
        );
    }

    #[test]
    fn crumbs_walk_contains_parents_root_first_and_omit_the_selected_node() {
        let g = graph();
        // Fixture edges: Box (n:3) --contains--> Run (n:1), so Box is Run's parent.
        let c = crumbs(&g, 0);
        assert_eq!(c.len(), 1, "one parent: {c:?}");
        assert_eq!(c[0]["id"], json!("n:3"));
        assert_eq!(c[0]["label"], json!("Box"));
        assert_eq!(c[0]["title"], json!("App.Box"));
        // The root itself has no crumb.
        assert!(crumbs(&g, 2).is_empty());
        // A cycle is bounded, never a hang (v2 MAX 20 hops).
        let mut g2 = graph();
        g2.edges.push(Edge { from: 0, to: 2, kind: "contains".to_string(), count: 1 });
        assert!(crumbs(&g2, 0).len() <= MAX_CRUMB_HOPS);
    }

    #[test]
    fn prop_actions_row_follows_the_hide_children_scope() {
        let g = graph();
        let f = facts_for(&g, Some(0)).expect("facts");
        let acts: Vec<&str> = f["actions"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|a| a["act"].as_str())
            .collect();
        assert_eq!(
            acts,
            ["expand", "hidech", "focus", "parents", "chain"],
            "shipping order"
        );
        assert_eq!(f["actions"][1]["label"], json!("Hide children"));
        // The two contextual focus actions ship unpressed and sit between the
        // drawer scope and the chain walk.
        assert_eq!(f["actions"][2]["act"], json!("focus"));
        assert_eq!(f["actions"][2]["label"], json!("Only related nodes"));
        assert_eq!(f["actions"][3]["act"], json!("parents"));
        assert_eq!(f["actions"][3]["label"], json!("Hide parents"));

        // `facts_for_focus` is the pressed state the drawer paints.
        let focused = facts_for_focus(&g, Some(0), &DrawerUi::default(), true, true)
            .expect("facts");
        assert_eq!(focused["actions"][2]["label"], json!("Show all nodes"));
        assert_eq!(focused["actions"][3]["label"], json!("Show parents"));

        let mut ui = DrawerUi::default();
        assert!(ui.toggle_hidden_children("n:1"), "now hidden");
        let f2 = facts_for_with(&g, Some(0), &ui).expect("facts");
        assert_eq!(f2["actions"][1]["label"], json!("Show children"));
        // Second toggle restores the scope.
        assert!(!ui.toggle_hidden_children("n:1"));
        assert!(!ui.hidden_children.contains("n:1"));
    }

    #[test]
    fn relation_groups_are_collapsed_by_default_and_toggle_per_node() {
        let g = graph();
        let f = facts_for(&g, Some(0)).expect("facts");
        assert!(f["relations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["open"] == json!(false)), "all groups ship collapsed");

        let mut ui = DrawerUi::default();
        assert!(!ui.group_open("n:1", "callers"));
        assert!(ui.toggle_group("n:1", "callers"), "opened");
        assert!(ui.group_open("n:1", "callers"));
        assert!(!ui.group_open("n:2", "callers"), "per-node scope");
        let f2 = facts_for_with(&g, Some(0), &ui).expect("facts");
        let callers = &f2["relations"][0];
        assert_eq!(callers["key"], json!("callers"));
        assert_eq!(callers["open"], json!(true));
        // Toggling again collapses it.
        assert!(!ui.toggle_group("n:1", "callers"));
        assert!(!ui.group_open("n:1", "callers"));
    }

    #[test]
    fn hide_children_forces_the_children_group_shut_and_marks_it_hidden() {
        let mut g = graph();
        // Add a real children bucket for Run: Run --contains--> Helper.
        g.edges.push(Edge { from: 0, to: 1, kind: "contains".to_string(), count: 1 });
        let mut ui = DrawerUi::default();
        ui.open_group("n:1", "children");
        let f = facts_for_with(&g, Some(0), &ui).expect("facts");
        let children = f["relations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == json!("children"))
            .expect("children bucket");
        assert_eq!(children["open"], json!(true));

        ui.toggle_hidden_children("n:1");
        let f = facts_for_with(&g, Some(0), &ui).expect("facts");
        let children = f["relations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["key"] == json!("children"))
            .expect("children bucket");
        assert_eq!(children["hidden"], json!(true));
        assert_eq!(children["open"], json!(false), "hidden scope forces it shut");
    }

    #[test]
    fn expand_all_opens_every_group_and_counts_only_the_new_ones() {
        let mut ui = DrawerUi::default();
        assert_eq!(ui.expand_all("n:1", ["callers", "uses"].into_iter()), 2);
        assert!(ui.group_open("n:1", "callers"));
        assert!(ui.group_open("n:1", "uses"));
        assert_eq!(ui.expand_all("n:1", ["callers", "uses"].into_iter()), 0);
    }

    #[test]
    fn short_file_is_root_relative_only_under_the_root() {
        let repo = std::path::Path::new("/repo");
        assert_eq!(short_file(repo, "/repo/src/A.cs"), "src/A.cs");
        assert_eq!(short_file(repo, "/other/A.cs"), "/other/A.cs");
        assert_eq!(short_file(repo, ""), "");
    }
}
