//! Bridge from the viewer's loaded graph to the `def2mcpfnnumber` addon.
//!
//! The addon owns the numbering rules (static system types first, project
//! tokens from 200, sticky dump/load, `thismodule` sub-dictionaries). This
//! module only translates [`crate::model::Node`] into the addon's [`Def`].
//!
//! It is compiled into the shipped binary on purpose: `--tokens` is the
//! packaged smoke test for the addon (see `main.rs`), so a missing dependency
//! or a broken mapping fails the AppImage run rather than a unit test only.
//!
//! ## Overloads
//!
//! csgraph nodes are unique (`id`), but `full` is not: constructors and
//! overloads repeat it (e.g. `X.Entry`). The addon keys a definition by its
//! `full` string, so a repeated `full` is made unique as `<full>#<node id>`.
//! That keeps the token 1:1 with the node without inventing a second numbering
//! space; `name` still carries the readable label for search.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::PathBuf;

use def2mcpfnnumber::shorthand::{self, Feature};
use def2mcpfnnumber::{DefaultKind, Def, DefKind, TokenIndex, defaults};

use crate::model::{Graph, Node};

/// The result of numbering one loaded graph.
pub struct TokenReport {
    pub index: TokenIndex,
    /// Entries whose declared type did not resolve (foreign type, or a parser
    /// value the table does not know). Reported, never silently turned into 0.
    pub unresolved_types: usize,
    /// Definitions whose `full` collided and were keyed by `<full>#<id>`.
    pub overloads: usize,
}

/// Number every declaration in the graph, system types excluded (they are the
/// addon's static table).
pub fn index_graph(graph: &Graph) -> TokenReport {
    let mut index = TokenIndex::new();
    let mut defs: Vec<(String, Def)> = Vec::with_capacity(graph.nodes.len());

    for node in &graph.nodes {
        if let Some(def) = def_of(node) {
            defs.push((node.id.clone(), def));
        }
    }

    // Namespaces first, then parents before children, then by full name. This
    // is the order the owner's sketch assumes (`NamespaceRwby` = 200 before its
    // members) and it makes a fresh index deterministic. Stability across a
    // rebuild comes from the persisted map, not from this sort.
    defs.sort_by(|(_, a), (_, b)| {
        kind_rank(a.kind)
            .cmp(&kind_rank(b.kind))
            .then_with(|| depth(&a.full).cmp(&depth(&b.full)))
            .then_with(|| a.full.cmp(&b.full))
    });

    let mut seen: HashSet<String> = HashSet::new();
    let mut overloads = 0usize;
    let mut ordered: Vec<Def> = Vec::with_capacity(defs.len());
    for (id, mut def) in defs {
        if !seen.insert(def.full.clone()) {
            overloads += 1;
            def.full = format!("{}#{}", def.full, id);
        }
        ordered.push(def);
    }

    index.index_defs(&ordered);
    let unresolved_types = index.globals.resolve_types();
    TokenReport {
        index,
        unresolved_types,
        overloads,
    }
}

/// Human-readable, deterministic report for `--tokens`.
pub fn report(graph: &Graph) -> String {
    let report = index_graph(graph);
    let mut out = String::new();
    out.push_str(&format!(
        "tokens: definitions={} overloads={} unresolvedTypes={} next=g({})\n",
        report.index.globals.len(),
        report.overloads,
        report.unresolved_types,
        report.index.globals.next_token().0
    ));

    let mut unresolved: Vec<&str> = report
        .index
        .globals
        .iter()
        .filter(|e| !e.type_name.is_empty() && e.type_token.is_none())
        .map(|e| e.type_name.as_str())
        .collect();
    unresolved.sort_unstable();
    unresolved.dedup();
    if !unresolved.is_empty() {
        unresolved.truncate(6);
        out.push_str(&format!("unresolved-types: {}\n", unresolved.join(", ")));
    }

    out.push_str("system-types:");
    for entry in defaults::all()
        .iter()
        .filter(|e| e.kind == DefaultKind::Type)
        .take(6)
    {
        out.push_str(&format!(" g({})={}", entry.token, entry.name));
    }
    out.push('\n');

    for entry in report.index.globals.iter().take(12) {
        out.push_str(&format!(
            "token g({}) {} {}\n",
            entry.token.0,
            entry.kind.as_str(),
            entry.full
        ));
    }

    out.push_str(&shorthand_summary(graph));
    out.push('\n');
    out
}

/// Scan every file the graph references and count the C# shorthand techniques
/// on its lines. This is the "detect ternary / shorthand" half of the report:
/// the tokens are pre-assigned (g(77) = `?:`, g(78) = `??`, …), the detector
/// only decides which lines carry them.
fn shorthand_summary(graph: &Graph) -> String {
    let files: BTreeSet<&str> = graph
        .nodes
        .iter()
        .map(|n| n.file.as_str())
        .filter(|f| !f.is_empty())
        .collect();

    let mut read = 0usize;
    let mut lines = 0usize;
    let mut counts: BTreeMap<Feature, usize> = BTreeMap::new();
    let mut unread = 0usize;

    for file in &files {
        let path = if std::path::Path::new(file).is_absolute() {
            PathBuf::from(file)
        } else {
            graph.repo.join(file)
        };
        let Ok(text) = fs::read_to_string(&path) else {
            unread += 1;
            continue;
        };
        read += 1;
        lines += text.lines().count();
        for line in shorthand::detect_lines(&text) {
            for feature in line.features {
                *counts.entry(feature).or_default() += 1;
            }
        }
    }

    let mut out = format!("shorthand: files={read} lines={lines}");
    if unread > 0 {
        out.push_str(&format!(" unread={unread}"));
    }
    if counts.is_empty() {
        out.push_str(" none");
    } else {
        for (feature, n) in counts {
            out.push_str(&format!(" {}={}", feature.as_str(), n));
        }
    }
    out
}

/// Namespace < type < member, so a parent namespace is never numbered after
/// the types it contains.
fn kind_rank(kind: DefKind) -> u8 {
    match kind {
        DefKind::Namespace => 0,
        DefKind::Type => 1,
        DefKind::Delegate => 2,
        DefKind::EnumMember => 3,
        DefKind::Constructor => 4,
        DefKind::Method => 5,
        DefKind::Property => 6,
        DefKind::Field => 7,
        DefKind::Event => 8,
        DefKind::Local => 9,
    }
}

fn depth(full: &str) -> usize {
    full.matches('.').count()
}

fn def_of(node: &Node) -> Option<Def> {
    let kind = def_kind(&node.kind)?;
    let full = full_name(node);
    if full.is_empty() {
        return None;
    }

    let mut def = Def::new(full, kind).at(
        node.file.clone(),
        node.line.max(0) as u32,
        node.end_line.max(0) as u32,
    );

    // A type's own identity is its token; it has no declared `ret`. Namespaces
    // and delegates are the same. Everything else carries the engine's `ret`.
    if !node.ret.is_empty()
        && !matches!(
            kind,
            DefKind::Namespace | DefKind::Type | DefKind::Delegate
        )
    {
        def = def.type_ref(node.ret.clone());
    }

    let owner = owner_name(node);
    if !owner.is_empty() {
        def = def.owner(owner);
    }

    Some(def)
}

fn def_kind(kind: &str) -> Option<DefKind> {
    match kind {
        "namespace" => Some(DefKind::Namespace),
        "class" | "struct" | "interface" | "enum" | "record" => Some(DefKind::Type),
        "delegate" => Some(DefKind::Delegate),
        "constructor" => Some(DefKind::Constructor),
        "method" => Some(DefKind::Method),
        "property" => Some(DefKind::Property),
        "field" => Some(DefKind::Field),
        "event" => Some(DefKind::Event),
        "enum-member" | "enumMember" => Some(DefKind::EnumMember),
        // `string` literals and `external` stubs are already dropped by
        // `model::convert`; anything else stays unlabelled rather than guessed.
        _ => None,
    }
}

/// A namespace's `full` is often empty in the engine payload; its id carries
/// the name as `#ns:...`. Members always have a `full`.
fn full_name(node: &Node) -> String {
    if !node.full.is_empty() {
        return node.full.clone();
    }
    if let Some(rest) = node.id.strip_prefix("#ns:") {
        return rest.to_string();
    }
    node.label.clone()
}

/// Namespaces point at their parent namespace; members point at the declaring
/// type (or namespace). Both spellings are normalised to a bare full name.
fn owner_name(node: &Node) -> String {
    let raw = if !node.owner.is_empty() {
        node.owner.as_str()
    } else if node.kind == "namespace" {
        node.parent.as_str()
    } else {
        ""
    };
    raw.strip_prefix("#ns:").unwrap_or(raw).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, kind: &str, full: &str, owner: &str, ret: &str) -> Node {
        Node {
            id: id.to_string(),
            label: full.rsplit('.').next().unwrap_or(full).to_string(),
            full: full.to_string(),
            kind: kind.to_string(),
            owner: owner.to_string(),
            ret: ret.to_string(),
            line: 3,
            end_line: 9,
            ..Node::default()
        }
    }

    #[test]
    fn maps_namespace_type_and_member_to_the_addon_kinds() {
        assert_eq!(def_kind("namespace"), Some(DefKind::Namespace));
        assert_eq!(def_kind("class"), Some(DefKind::Type));
        assert_eq!(def_kind("method"), Some(DefKind::Method));
        assert_eq!(def_kind("string"), None);
        assert_eq!(def_kind("external"), None);
    }

    #[test]
    fn namespace_name_comes_from_the_id_and_members_keep_full() {
        let ns = Node {
            id: "#ns:NZK.B".to_string(),
            kind: "namespace".to_string(),
            label: "B".to_string(),
            ..Node::default()
        };
        assert_eq!(full_name(&ns), "NZK.B");

        let m = node("NZK.B.A", "method", "NZK.B.A", "NZK.B", "int");
        assert_eq!(full_name(&m), "NZK.B.A");
        assert_eq!(owner_name(&m), "NZK.B");
        let def = def_of(&m).expect("method maps");
        assert_eq!(def.type_name, "int");
    }

    #[test]
    fn aliases_resolve_to_system_type_tokens() {
        let mut index = TokenIndex::new();
        let def = def_of(&node("A.M", "method", "A.M", "A", "int")).unwrap();
        index.index_defs(&[def, Def::new("A", DefKind::Type)]);
        assert_eq!(index.globals.resolve_types(), 0);
        let entry = index.globals.get(index.globals.token_of("A.M").unwrap()).unwrap();
        assert_eq!(entry.type_token.map(|t| t.0), Some(102), "int == g(102)");
    }
}
