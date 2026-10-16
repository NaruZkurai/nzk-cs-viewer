//! graph — `GraphBuilder` and `build_graph` from `tools/csgraph/cs_parser.py`.
//!
//! Byte-identical nodes and edges: the same ids, labels, signatures, caps,
//! insertion order (which fixes every `e%d` edge id and the `_parts` array) and
//! the same `depth > 8` inheritance guard.
//!
//! `self.nodes` is a Python `dict` in the original and its *insertion order* is
//! observable (it drives `finish()`'s edge numbering), so it is kept as a `Vec`
//! plus an id -> index map rather than a `HashMap`.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

use crate::hash::md5_hex;
use crate::model::{is_keyword, strip_ws};
use crate::parser::qualified_re;

type EdgeKey = (String, String, String);

/// `class GraphBuilder`.
pub struct GraphBuilder {
    pub nodes: Vec<Value>,
    pub node_ix: HashMap<String, usize>,
    pub edges: Vec<Value>,
    edge_ix: HashMap<EdgeKey, usize>,
    types_by_name: HashMap<String, Vec<String>>,
    types_by_full: HashMap<String, String>,
    members_by_name: HashMap<String, Vec<String>>,
    by_owner_name: HashMap<(String, String), Vec<String>>,
    fields_by_owner: HashMap<String, HashMap<String, Vec<String>>>,
    var_types: HashMap<String, Value>,
    bases: HashMap<String, Vec<String>>,
    ns_types: HashMap<String, Vec<String>>,
    pub warnings: Vec<String>,
    deg: HashMap<String, i64>,
    external_count: usize,
    string_count: usize,
}

impl Default for GraphBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphBuilder {
    /// `GraphBuilder.EXTERNAL_CAP`.
    pub const EXTERNAL_CAP: usize = 4000;
    /// `GraphBuilder.STRING_CAP`.
    pub const STRING_CAP: usize = 8000;

    /// `GraphBuilder.__init__`.
    pub fn new() -> GraphBuilder {
        GraphBuilder {
            nodes: Vec::new(),
            node_ix: HashMap::new(),
            edges: Vec::new(),
            edge_ix: HashMap::new(),
            types_by_name: HashMap::new(),
            types_by_full: HashMap::new(),
            members_by_name: HashMap::new(),
            by_owner_name: HashMap::new(),
            fields_by_owner: HashMap::new(),
            var_types: HashMap::new(),
            bases: HashMap::new(),
            ns_types: HashMap::new(),
            warnings: Vec::new(),
            deg: HashMap::new(),
            external_count: 0,
            string_count: 0,
        }
    }

    // -- registration -------------------------------------------------------
    /// `register(pr)`.
    pub fn register(&mut self, pr: &Value) {
        if let Some(ws) = pr.get("warnings").and_then(|v| v.as_array()) {
            for w in ws {
                self.warnings.push(w.as_str().unwrap_or("").to_string());
            }
        }
        if let Some(types) = pr.get("types").and_then(|v| v.as_array()) {
            for t in types {
                let id = vs(t, "id");
                let name = vs(t, "name");
                let full = vs(t, "full");
                let kind = vs(t, "kind");
                let ns = vs(t, "ns");
                let owner = vs(t, "owner");
                let label = full.rsplit('.').next().unwrap_or("").to_string();
                let parent = if !owner.is_empty() {
                    owner.clone()
                } else if !ns.is_empty() {
                    format!("#ns:{ns}")
                } else {
                    String::new()
                };
                let mut n = Map::new();
                n.insert("id".into(), json!(id));
                n.insert("name".into(), json!(name));
                n.insert("label".into(), json!(label));
                n.insert("full".into(), json!(full));
                n.insert("kind".into(), json!(kind));
                n.insert("ns".into(), json!(ns));
                n.insert("owner".into(), json!(owner));
                n.insert("ownerKind".into(), json!(""));
                n.insert("file".into(), json!(vs(t, "file")));
                n.insert("line".into(), json!(vi(t, "line")));
                n.insert("endLine".into(), json!(vi(t, "endLine")));
                n.insert("signature".into(), json!(self.type_sig(t)));
                n.insert("ret".into(), json!(""));
                n.insert("params".into(), json!([]));
                n.insert("access".into(), json!(""));
                n.insert("static".into(), json!(false));
                n.insert("deg".into(), json!(0));
                n.insert("parent".into(), json!(parent));
                n.insert("head".into(), list_or_empty(t, "head"));
                n.insert("tail".into(), list_or_empty(t, "tail"));
                self.put_node(n);

                let tid = vs(t, "id");
                self.types_by_name.entry(name.clone()).or_default().push(tid.clone());
                self.types_by_full.insert(full.to_lowercase(), tid.clone());
                self.bases.insert(
                    tid.clone(),
                    t.get("bases")
                        .and_then(|v| v.as_array())
                        .map(|a| a.iter().map(|b| b.as_str().unwrap_or("").to_string()).collect())
                        .unwrap_or_default(),
                );
                self.ns_types.entry(ns).or_default().push(tid);
            }
        }
        if let Some(members) = pr.get("members").and_then(|v| v.as_array()) {
            for m in members {
                let owner = vs(m, "owner");
                let name = vs(m, "name");
                let label = if owner.is_empty() {
                    name.clone()
                } else {
                    format!("{}.{}", owner.rsplit('.').next().unwrap_or(""), name)
                };
                let full = if owner.is_empty() {
                    name.clone()
                } else {
                    format!("{owner}.{name}")
                };
                let mid = vs(m, "id");
                let mut n = Map::new();
                n.insert("id".into(), json!(mid));
                n.insert("name".into(), json!(name));
                n.insert("label".into(), json!(label));
                n.insert("full".into(), json!(full));
                n.insert("kind".into(), json!(vs(m, "kind")));
                n.insert("ns".into(), json!(vs(m, "ns")));
                n.insert("owner".into(), json!(owner));
                n.insert("ownerKind".into(), json!(""));
                n.insert("file".into(), json!(vs(m, "file")));
                n.insert("line".into(), json!(vi(m, "line")));
                n.insert("endLine".into(), json!(vi(m, "endLine")));
                n.insert("signature".into(), json!(vs(m, "signature")));
                n.insert("ret".into(), json!(vs(m, "ret")));
                n.insert(
                    "params".into(),
                    m.get("params").cloned().unwrap_or_else(|| json!([])),
                );
                n.insert("access".into(), json!(vs(m, "access")));
                n.insert("static".into(), json!(vb(m, "static")));
                n.insert("deg".into(), json!(0));
                n.insert("parent".into(), json!(vs(m, "owner")));
                n.insert("head".into(), list_or_empty(m, "head"));
                n.insert("tail".into(), list_or_empty(m, "tail"));
                n.insert(
                    "startOff".into(),
                    m.get("startOff").cloned().unwrap_or(Value::Null),
                );
                n.insert("endOff".into(), m.get("endOff").cloned().unwrap_or(Value::Null));
                self.put_node(n);

                self.members_by_name
                    .entry(name.clone())
                    .or_default()
                    .push(mid.clone());
                self.by_owner_name
                    .entry((owner.clone(), name.clone()))
                    .or_default()
                    .push(mid.clone());
                let kind = vs(m, "kind");
                if kind == "field" || kind == "property" || kind == "event" {
                    self.fields_by_owner
                        .entry(owner.clone())
                        .or_default()
                        .entry(name.clone())
                        .or_default()
                        .push(mid.clone());
                }
                self.var_types.insert(
                    mid.clone(),
                    m.get("vartypes")
                        .cloned()
                        .unwrap_or_else(|| Value::Object(Map::new())),
                );
                if let Some(strings) = m.get("strings").and_then(|v| v.as_array()) {
                    for s in strings {
                        let value = s.get("value").and_then(|v| v.as_str()).unwrap_or("");
                        let line = s.get("line").and_then(|v| v.as_i64());
                        if let Some(sid) = self.string_node(value, &vs(m, "file"), line) {
                            let assigned = s.get("assigned").and_then(|v| v.as_bool()).unwrap_or(false);
                            let k = if assigned { "assigns" } else { "uses" };
                            self.edge(&mid, &sid, k, line, false);
                        }
                    }
                }
            }
        }
    }

    /// `resolve(pr)`.
    pub fn resolve(&mut self, pr: &Value) {
        if let Some(members) = pr.get("members").and_then(|v| v.as_array()) {
            for m in members {
                if let Some(refs) = m.get("refs").and_then(|v| v.as_array()) {
                    for r in refs {
                        self.resolve_ref(r, m);
                    }
                }
            }
        }
    }

    /// `add_parse(pr)` — register + resolve in one call.
    pub fn add_parse(&mut self, pr: &Value) {
        self.register(pr);
        self.resolve(pr);
    }

    /// `_part(n)`.
    fn part(n: &Map<String, Value>) -> Value {
        let file = n
            .get("file")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        let line = n.get("line").cloned().unwrap_or_else(|| json!(0));
        let end_line = n.get("endLine").cloned().unwrap_or_else(|| json!(0));
        let start_off = n.get("startOff").cloned().unwrap_or(Value::Null);
        let end_off = n.get("endOff").cloned().unwrap_or(Value::Null);
        let head = n
            .get("head")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let tail = n
            .get("tail")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        json!({
            "file": file, "line": line, "endLine": end_line,
            "startOff": start_off, "endOff": end_off,
            "head": head, "tail": tail,
        })
    }

    /// `_put_node(n)` — insert, or fold a repeat declaration into `_parts`.
    fn put_node(&mut self, mut n: Map<String, Value>) {
        let id = n.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if let Some(&ix) = self.node_ix.get(&id) {
            let new_end = n.get("endLine").and_then(|v| v.as_i64()).unwrap_or(0);
            let old = self.nodes[ix].as_object_mut().unwrap();
            let old_end = old.get("endLine").and_then(|v| v.as_i64()).unwrap_or(0);
            if new_end > old_end {
                old.insert("endLine".into(), json!(new_end));
            }
            let segs = old.get("_segs").and_then(|v| v.as_i64()).unwrap_or(1) + 1;
            old.insert("_segs".into(), json!(segs));
            if !old.contains_key("_parts") {
                let p = Self::part(old);
                old.insert("_parts".into(), Value::Array(vec![p]));
            }
            let p = Self::part(&n);
            old.get_mut("_parts").unwrap().as_array_mut().unwrap().push(p);
            return;
        }
        n.insert("_segs".into(), json!(1));
        let p = Self::part(&n);
        n.insert("_parts".into(), Value::Array(vec![p]));
        self.node_ix.insert(id, self.nodes.len());
        self.nodes.push(Value::Object(n));
    }

    /// `_type_sig(t)`.
    fn type_sig(&self, t: &Value) -> String {
        let kind = vs(t, "kind");
        let name = vs(t, "name");
        let bases: Vec<String> = t
            .get("bases")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(|b| b.as_str().unwrap_or("").to_string()).collect())
            .unwrap_or_default();
        let b = if bases.is_empty() {
            String::new()
        } else {
            format!(" : {}", bases.join(", "))
        };
        format!("{kind} {name}{b}")
    }

    // -- literal + external nodes -------------------------------------------
    /// `string_node(value, file, line)`.
    pub fn string_node(&mut self, value: &str, file: &str, line: Option<i64>) -> Option<String> {
        let v = value;
        let stripped: Vec<char> = strip_ws(&v.chars().collect::<Vec<char>>()).to_vec();
        if stripped.is_empty() || v.chars().count() > 400 {
            return None;
        }
        let nid = format!("#str:{}", &md5_hex(v.as_bytes())[..16]);
        if self.node_ix.contains_key(&nid) {
            return Some(nid);
        }
        if self.string_count >= Self::STRING_CAP {
            return None;
        }
        let count = v.chars().count();
        let label = if count <= 90 {
            v.to_string()
        } else {
            let prefix: String = v.chars().take(87).collect();
            format!("{prefix}...")
        };
        self.string_count += 1;
        let name: String = v.chars().take(200).collect();
        let mut n = Map::new();
        n.insert("id".into(), json!(nid));
        n.insert("name".into(), json!(name));
        n.insert("label".into(), json!(label));
        n.insert("full".into(), json!(v));
        n.insert("kind".into(), json!("string"));
        n.insert("ns".into(), json!(""));
        n.insert("owner".into(), json!(""));
        n.insert("ownerKind".into(), json!(""));
        n.insert("file".into(), json!(file));
        n.insert("line".into(), json!(line.unwrap_or(0)));
        n.insert("endLine".into(), json!(line.unwrap_or(0)));
        n.insert("signature".into(), json!(format!("\"{label}\"")));
        n.insert("ret".into(), json!(""));
        n.insert("params".into(), json!([]));
        n.insert("access".into(), json!(""));
        n.insert("static".into(), json!(false));
        n.insert("deg".into(), json!(0));
        n.insert("parent".into(), json!(""));
        n.insert("literal".into(), json!(true));
        self.put_node(n);
        Some(nid)
    }

    /// `external(dotted)`.
    pub fn external(&mut self, dotted: &str) -> Option<String> {
        if dotted.is_empty() {
            return None;
        }
        let d: String = dotted
            .trim_matches(|c| c == '.' || c == ' ')
            .to_string();
        if d.is_empty() || is_keyword(&d) {
            return None;
        }
        let low = d.to_lowercase();
        if let Some(id) = self.types_by_full.get(&low) {
            return Some(id.clone());
        }
        if let Some(ids) = self.types_by_name.get(&d) {
            return ids.first().cloned();
        }
        let parts: Vec<&str> = d.split('.').collect();
        let last = parts.last().copied().unwrap_or("");
        if !last.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            return None;
        }
        if self.external_count >= Self::EXTERNAL_CAP {
            return None;
        }
        self.external_count += 1;
        let name = last.to_string();
        let ns = parts[..parts.len() - 1].join(".");
        let mut n = Map::new();
        n.insert("id".into(), json!(d));
        n.insert("name".into(), json!(name));
        n.insert("label".into(), json!(d));
        n.insert("full".into(), json!(d));
        n.insert("kind".into(), json!("external"));
        n.insert("ns".into(), json!(ns));
        n.insert("owner".into(), json!(""));
        n.insert("ownerKind".into(), json!(""));
        n.insert("file".into(), json!(""));
        n.insert("line".into(), json!(0));
        n.insert("endLine".into(), json!(0));
        n.insert("signature".into(), json!(format!("external type {d}")));
        n.insert("ret".into(), json!(""));
        n.insert("params".into(), json!([]));
        n.insert("access".into(), json!(""));
        n.insert("static".into(), json!(false));
        n.insert("deg".into(), json!(0));
        n.insert("parent".into(), json!(""));
        n.insert("external".into(), json!(true));
        self.put_node(n);
        Some(d)
    }

    // -- edges --------------------------------------------------------------
    /// `_edge(source, target, kind, line, ambiguous=False)`.
    pub fn edge(
        &mut self,
        source: &str,
        target: &str,
        kind: &str,
        line: Option<i64>,
        ambiguous: bool,
    ) {
        if source == target
            || !self.node_ix.contains_key(source)
            || !self.node_ix.contains_key(target)
        {
            return;
        }
        let key = (source.to_string(), target.to_string(), kind.to_string());
        let ix = match self.edge_ix.get(&key) {
            Some(&ix) => ix,
            None => {
                let ix = self.edges.len();
                self.edges.push(json!({
                    "id": format!("e{ix}"),
                    "source": source,
                    "target": target,
                    "kind": kind,
                    "count": 0,
                    "lines": [],
                    "ambiguous": ambiguous,
                }));
                self.edge_ix.insert(key, ix);
                *self.deg.entry(source.to_string()).or_insert(0) += 1;
                *self.deg.entry(target.to_string()).or_insert(0) += 1;
                ix
            }
        };
        let e = self.edges[ix].as_object_mut().unwrap();
        let count = e.get("count").and_then(|v| v.as_i64()).unwrap_or(0) + 1;
        e.insert("count".into(), json!(count));
        let truthy = line.map(|l| l != 0).unwrap_or(false);
        if truthy {
            let lines = e.get_mut("lines").unwrap().as_array_mut().unwrap();
            if lines.len() < 40 {
                let l = line.unwrap();
                if !lines.iter().any(|v| v.as_i64() == Some(l)) {
                    lines.push(json!(l));
                }
            }
        }
        if ambiguous {
            e.insert("ambiguous".into(), json!(true));
        }
    }

    // -- resolution helpers -------------------------------------------------
    /// `_bases_of(type_id, depth=0, seen=None)`.
    pub fn bases_of(&self, type_id: &str) -> Vec<String> {
        let mut seen = HashSet::new();
        self.bases_of_rec(type_id, 0, &mut seen)
    }

    fn bases_of_rec(&self, type_id: &str, depth: usize, seen: &mut HashSet<String>) -> Vec<String> {
        if seen.contains(type_id) || depth > 8 {
            return Vec::new();
        }
        seen.insert(type_id.to_string());
        let mut out = Vec::new();
        if let Some(bs) = self.bases.get(type_id) {
            for b in bs {
                if let Some(tids) = self.types_by_name.get(b) {
                    for tid in tids.clone() {
                        if !seen.contains(&tid) {
                            out.push(tid.clone());
                            out.extend(self.bases_of_rec(&tid, depth + 1, seen));
                        }
                    }
                }
            }
        }
        out
    }

    /// `_type_ids_for_text(text)`.
    fn type_ids_for_text(&self, text: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if text.is_empty() {
            return out;
        }
        let chars: Vec<char> = text.chars().collect();
        for m in crate::parser::ident_re().findall1(&chars) {
            if is_keyword(&m) {
                continue;
            }
            if let Some(tids) = self.types_by_name.get(&m) {
                for tid in tids {
                    if !out.contains(tid) {
                        out.push(tid.clone());
                    }
                }
            }
        }
        out
    }

    /// `_q_types(q, m)`.
    fn q_types(&self, q: &str, m: &Value) -> Vec<String> {
        let owner = vs(m, "owner");
        if q == "this" {
            return if owner.is_empty() { Vec::new() } else { vec![owner] };
        }
        if q == "base" {
            return if owner.is_empty() {
                Vec::new()
            } else {
                self.bases_of(&owner)
            };
        }
        let mid = vs(m, "id");
        if let Some(vt) = self.var_types.get(&mid).and_then(|v| v.as_object()) {
            if let Some(ty) = vt.get(q).and_then(|v| v.as_str()) {
                if !ty.is_empty() {
                    let ids = self.type_ids_for_text(ty);
                    if !ids.is_empty() {
                        return ids;
                    }
                }
            }
        }
        let mut owners: Vec<String> = vec![owner.clone()];
        if !owner.is_empty() {
            owners.extend(self.bases_of(&owner));
        }
        for o in &owners {
            if let Some(flds) = self.fields_by_owner.get(o) {
                if let Some(fids) = flds.get(q) {
                    for fid in fids {
                        let ret = self
                            .nodes
                            .get(*self.node_ix.get(fid).unwrap_or(&usize::MAX))
                            .and_then(|n| n.get("ret"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let ids = self.type_ids_for_text(&ret);
                        if !ids.is_empty() {
                            return ids;
                        }
                    }
                }
            }
        }
        let parts: Vec<&str> = q.split('.').collect();
        for i in 0..parts.len() {
            let cand = parts[i..].join(".");
            let low = cand.to_lowercase();
            if let Some(id) = self.types_by_full.get(&low) {
                return vec![id.clone()];
            }
            if let Some(ids) = self.types_by_name.get(&cand) {
                return ids.clone();
            }
        }
        Vec::new()
    }

    fn node_kind(&self, id: &str) -> String {
        match self.node_ix.get(id) {
            Some(&ix) => self.nodes[ix]
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            None => String::new(),
        }
    }

    /// `_resolve_ref(r, m)`.
    fn resolve_ref(&mut self, r: &Value, m: &Value) {
        let mid = vs(m, "id");
        let name = vs(r, "name");
        let q = r.get("qual").and_then(|v| v.as_str()).map(|s| s.to_string());
        let line = r.get("line").and_then(|v| v.as_i64());
        let kind = vs(r, "kind");
        if kind == "construct" {
            let mut targets = self.types_by_name.get(&name).cloned().unwrap_or_default();
            if targets.is_empty() {
                let dotted = r
                    .get("dotted")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&name)
                    .to_lowercase();
                if let Some(id) = self.types_by_full.get(&dotted) {
                    targets = vec![id.clone()];
                }
            }
            if targets.is_empty() {
                let dotted = r.get("dotted").and_then(|v| v.as_str()).unwrap_or(&name);
                if let Some(eid) = self.external(dotted) {
                    self.edge(&mid, &eid, "constructs", line, false);
                }
                return;
            }
            for t in targets {
                self.edge(&mid, &t, "constructs", line, false);
            }
            return;
        }
        if kind == "call" {
            let owners: Option<Vec<String>> = match &q {
                Some(q) => Some(self.q_types(q, m)),
                None => None,
            };
            let m_owner = vs(m, "owner");
            let m_ns = vs(m, "ns");
            let mut tiers: Vec<Vec<String>> = Vec::new();
            if q.is_none() || owners.is_some() {
                match &q {
                    None => {
                        tiers.push(if m_owner.is_empty() {
                            Vec::new()
                        } else {
                            vec![m_owner.clone()]
                        });
                        tiers.push(if m_owner.is_empty() {
                            Vec::new()
                        } else {
                            self.bases_of(&m_owner)
                        });
                        tiers.push(self.ns_types.get(&m_ns).cloned().unwrap_or_default());
                    }
                    Some(_) => {
                        let owners = owners.clone().unwrap_or_default();
                        tiers.push(owners.clone());
                        let mut second: Vec<String> = Vec::new();
                        for o in &owners {
                            second.extend(self.bases_of(o));
                        }
                        tiers.push(second);
                    }
                }
            }
            for tier in tiers {
                let mut found: Vec<String> = Vec::new();
                for o in &tier {
                    if let Some(ids) = self.by_owner_name.get(&(o.clone(), name.clone())) {
                        found.extend(ids.iter().cloned());
                    }
                }
                if !found.is_empty() {
                    for t in found {
                        let k = if matches!(self.node_kind(&t).as_str(), "property" | "field" | "event")
                        {
                            "uses"
                        } else {
                            "calls"
                        };
                        self.edge(&mid, &t, k, line, false);
                    }
                    return;
                }
            }
            if q.is_some() && owners.as_ref().map(|o| !o.is_empty()).unwrap_or(false) {
                return;
            }
            let mut glob = self.members_by_name.get(&name).cloned().unwrap_or_default();
            glob.retain(|g| {
                matches!(
                    self.node_kind(g).as_str(),
                    "method" | "constructor" | "operator" | "property"
                )
            });
            let amb = glob.len() > 1;
            for t in glob.iter().take(12) {
                let k = if self.node_kind(t) == "property" {
                    "uses"
                } else {
                    "calls"
                };
                self.edge(&mid, t, k, line, amb);
            }
            if glob.is_empty() {
                if let Some(q) = &q {
                    if let Some(eid) = self.external(q) {
                        self.edge(&mid, &eid, "uses", line, false);
                    }
                }
            }
            return;
        }
        // 'use'
        if q.is_none() {
            let m_owner = vs(m, "owner");
            let mut owners: Vec<String> = vec![m_owner.clone()];
            if !m_owner.is_empty() {
                owners.extend(self.bases_of(&m_owner));
            }
            for o in owners {
                let hits: Vec<String> = self
                    .fields_by_owner
                    .get(&o)
                    .and_then(|f| f.get(&name))
                    .cloned()
                    .unwrap_or_default();
                if let Some(fid) = hits.first() {
                    self.edge(&mid, fid, "uses", line, false);
                    return;
                }
            }
            let tids = self.types_by_name.get(&name).cloned().unwrap_or_default();
            for tid in tids.iter().take(4) {
                self.edge(&mid, tid, "uses", line, false);
            }
            return;
        }
        let q = q.unwrap();
        let owners = self.q_types(&q, m);
        if !owners.is_empty() {
            let mut all: Vec<String> = owners.clone();
            for x in &owners {
                all.extend(self.bases_of(x));
            }
            for o in all {
                let hits: Vec<String> = self
                    .fields_by_owner
                    .get(&o)
                    .and_then(|f| f.get(&name))
                    .cloned()
                    .unwrap_or_default();
                if !hits.is_empty() {
                    for t in hits {
                        self.edge(&mid, &t, "uses", line, false);
                    }
                    return;
                }
            }
        }
        let tids = self.types_by_name.get(&name).cloned().unwrap_or_default();
        for tid in tids.iter().take(4) {
            self.edge(&mid, tid, "uses", line, false);
        }
        let dotted = r.get("dotted").and_then(|v| v.as_str()).unwrap_or("");
        if dotted.contains('.')
            && dotted
                .split('.')
                .next()
                .and_then(|s| s.chars().next())
                .map(|c| c.is_uppercase())
                .unwrap_or(false)
        {
            let prefix = dotted.split('.').collect::<Vec<_>>();
            let parent = prefix[..prefix.len() - 1].join(".");
            if let Some(eid) = self.external(&parent) {
                self.edge(&mid, &eid, "uses", line, false);
            }
        }
    }

    // -- finalise -----------------------------------------------------------
    /// `finish()`.
    pub fn finish(&mut self) {
        // 1. namespace nodes (every namespace in play, plus its parent chain)
        let mut want: HashSet<String> = HashSet::new();
        for n in &self.nodes {
            let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if kind == "string" || kind == "external" {
                continue;
            }
            let ns = n.get("ns").and_then(|v| v.as_str()).unwrap_or("");
            if ns.is_empty() {
                continue;
            }
            let parts: Vec<&str> = ns.split('.').collect();
            for i in 1..=parts.len() {
                want.insert(parts[..i].join("."));
            }
        }
        let mut sorted: Vec<String> = want.into_iter().collect();
        sorted.sort();
        let mut parent_ns: Vec<(String, String)> = Vec::new();
        for ns in sorted {
            let nid = format!("#ns:{ns}");
            let parent = if ns.contains('.') {
                let parts: Vec<&str> = ns.split('.').collect();
                format!("#ns:{}", parts[..parts.len() - 1].join("."))
            } else {
                String::new()
            };
            parent_ns.push((ns.clone(), parent.clone()));
            let name = ns.rsplit('.').next().unwrap_or("").to_string();
            let mut n = Map::new();
            n.insert("id".into(), json!(nid));
            n.insert("name".into(), json!(name));
            n.insert("label".into(), json!(ns));
            n.insert("full".into(), json!(ns));
            n.insert("kind".into(), json!("namespace"));
            n.insert("ns".into(), json!(ns));
            n.insert("owner".into(), json!(""));
            n.insert("ownerKind".into(), json!(""));
            n.insert("file".into(), json!(""));
            n.insert("line".into(), json!(0));
            n.insert("endLine".into(), json!(0));
            n.insert("signature".into(), json!(format!("namespace {ns}")));
            n.insert("ret".into(), json!(""));
            n.insert("params".into(), json!([]));
            n.insert("access".into(), json!(""));
            n.insert("static".into(), json!(false));
            n.insert("deg".into(), json!(0));
            n.insert("parent".into(), json!(parent));
            self.put_node(n);
        }
        // 2. parent links (types keep owner-or-namespace, members keep owner)
        for n in self.nodes.iter_mut() {
            let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if kind == "namespace" {
                continue;
            }
            let obj = n.as_object_mut().unwrap();
            if kind == "string" || kind == "external" {
                obj.insert("parent".into(), json!(""));
            } else {
                let empty = obj
                    .get("parent")
                    .map(|v| v.as_str().map(|s| s.is_empty()).unwrap_or(true))
                    .unwrap_or(true);
                if empty {
                    let ns = obj.get("ns").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let p = if ns.is_empty() {
                        String::new()
                    } else {
                        format!("#ns:{ns}")
                    };
                    obj.insert("parent".into(), json!(p));
                }
            }
        }
        // 3. ownerKind
        let owner_kinds: Vec<(String, String)> = self
            .nodes
            .iter()
            .map(|n| {
                let owner = n.get("owner").and_then(|v| v.as_str()).unwrap_or("");
                let ok = if !owner.is_empty() {
                    self.node_kind(owner)
                } else {
                    String::new()
                };
                (owner.to_string(), ok)
            })
            .collect();
        for (i, (owner, ok)) in owner_kinds.into_iter().enumerate() {
            if !owner.is_empty() {
                self.nodes[i]
                    .as_object_mut()
                    .unwrap()
                    .insert("ownerKind".into(), json!(ok));
            }
        }
        // 4. contains: namespace -> type, type -> nested type/member
        let mut contains: Vec<(String, String, i64)> = Vec::new();
        for n in &self.nodes {
            let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if kind == "external" || kind == "string" || kind == "namespace" {
                continue;
            }
            let nid = n.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let parent = n
                .get("parent")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let line = n.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
            if !parent.is_empty() && self.node_ix.contains_key(&parent) {
                contains.push((parent, nid, line));
            }
        }
        for (parent, nid, line) in contains {
            self.edge(&parent, &nid, "contains", Some(line), false);
        }
        for (ns, parent) in &parent_ns {
            if !parent.is_empty() {
                self.edge(parent, &format!("#ns:{ns}"), "contains", Some(0), false);
            }
        }
        // 5. external types referenced by return / parameter / field types
        let snapshot = self.nodes.len();
        let mut ext_edges: Vec<(String, String, i64)> = Vec::new();
        for i in 0..snapshot {
            let n = &self.nodes[i];
            let kind = n.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if kind == "string" || kind == "external" || kind == "namespace" {
                continue;
            }
            let nid = n.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let line = n.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
            let mut texts: Vec<String> = vec![
                n.get("ret")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            ];
            if let Some(params) = n.get("params").and_then(|v| v.as_array()) {
                for p in params {
                    texts.push(p.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string());
                }
            }
            for text in texts {
                let chars: Vec<char> = text.chars().collect();
                for name in qualified_re().findall1(&chars) {
                    if !name.contains('.') {
                        continue;
                    }
                    let first_upper = name
                        .split('.')
                        .next()
                        .and_then(|s| s.chars().next())
                        .map(|c| c.is_uppercase())
                        .unwrap_or(false);
                    if first_upper && !self.types_by_full.contains_key(&name.to_lowercase()) {
                        ext_edges.push((nid.clone(), name, line));
                    }
                }
            }
        }
        for (nid, name, line) in ext_edges {
            if let Some(eid) = self.external(&name) {
                self.edge(&nid, &eid, "uses", Some(line), false);
            }
        }
        // 6. inherits edges
        let snapshot2 = self.nodes.len();
        let mut inh_edges: Vec<(String, String, i64)> = Vec::new();
        for i in 0..snapshot2 {
            let n = &self.nodes[i];
            let nid = n.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let line = n.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
            if let Some(bs) = self.bases.get(&nid) {
                for b in bs {
                    if let Some(tids) = self.types_by_name.get(b) {
                        for tid in tids {
                            inh_edges.push((nid.clone(), tid.clone(), line));
                        }
                    }
                }
            }
        }
        for (nid, tid, line) in inh_edges {
            self.edge(&nid, &tid, "inherits", Some(line), false);
        }
        // 7. degrees
        for i in 0..self.nodes.len() {
            let id = self.nodes[i]
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let d = *self.deg.get(&id).unwrap_or(&0);
            self.nodes[i].as_object_mut().unwrap().insert("degree".into(), json!(d));
        }
        // line lists are only needed on calls/constructs/uses
        for e in self.edges.iter_mut() {
            let kind = e.get("kind").and_then(|v| v.as_str()).unwrap_or("");
            if kind == "contains" || kind == "inherits" {
                e.as_object_mut().unwrap().insert("lines".into(), json!([]));
            }
        }
    }
}

/// `build_graph(parses, roots=None)`.
pub fn build_graph(parses: &[Value]) -> GraphBuilder {
    let mut gb = GraphBuilder::new();
    for pr in parses {
        gb.register(pr);
    }
    for pr in parses {
        gb.resolve(pr);
    }
    gb.finish();
    gb
}

// ---------------------------------------------------------------------------
// small `Value` accessors
// ---------------------------------------------------------------------------
fn vs(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn vi(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

fn vb(v: &Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

/// `list(x.get(key) or [])`.
fn list_or_empty(v: &Value, key: &str) -> Value {
    Value::Array(
        v.get(key)
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::Path;

    #[test]
    fn string_node_id_is_md5_prefix() {
        let mut gb = GraphBuilder::new();
        let id = gb.string_node("hello", "f.cs", Some(1)).unwrap();
        assert_eq!(id, format!("#str:{}", &crate::hash::md5_hex(b"hello")[..16]));
        assert!(gb.string_node("hello", "f.cs", Some(2)).is_some());
        assert_eq!(gb.nodes.len(), 1);
    }

    #[test]
    fn build_graph_smoke() {
        let dir = std::env::temp_dir().join("csgraph_port_smoke");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("a.cs");
        std::fs::write(
            &file,
            "namespace N {\n class C : B {\n int F() { return new D().G(); }\n }\n}\n",
        )
        .unwrap();
        let src = crate::parser::Source::new(
            file.to_string_lossy().to_string(),
            "a.cs".to_string(),
            crate::parser::read_python_text(&file).unwrap(),
            None::<&HashSet<String>>,
        );
        let mut p = crate::parser::Parser::new(&src);
        p.parse();
        let members = crate::parser::finalize_members(&p, &src);
        let pr = json!({
            "file": "a.cs", "types": p.types, "members": members,
            "inherits": [], "warnings": [],
        });
        let gb = build_graph(&[pr]);
        // Ground truth from the frozen Python on this exact fixture:
        //   ['#ns:N', 'D', 'N.C', 'N.C.F()']
        // (there is no `N.C.F().G` node — the callee of a chained call is not
        // a declaration of C, so nothing declares that id).
        assert!(gb.node_ix.contains_key("#ns:N"));
        assert!(gb.node_ix.contains_key("N.C"));
        assert!(gb.node_ix.contains_key("N.C.F()"));
        assert!(gb.node_ix.contains_key("D"));
        let _ = Path::new(".");
    }
}
