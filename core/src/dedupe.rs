//! `dedupe` — deterministic, memory-bounded duplicate-code report.
//!
//! The owner's vision 2/4 is "dedupe/functionize": find repeated code and
//! extract it. This is the first bounded slice — a *report*, not an edit. It
//! answers three questions over a corpus:
//!
//! * **exact** — method/constructor bodies that are byte-different but
//!   token-identical (so whitespace/indentation/comments are ignored);
//! * **near** — bodies whose normalised 4-gram shingle sets have Jaccard
//!   similarity `>= threshold`, with the differing structure summarised;
//! * **blocks** — runs of `>= min_lines` consecutive non-trivial normalised
//!   lines that recur in more than one place.
//!
//! Design constraints, each enforced below:
//!
//! * **No quadratic blow-up.** Units are grouped by a stable digest; near
//!   matches come from an inverted shingle index with a posting cap and a hard
//!   pair-comparison budget. Nothing iterates the unit count squared.
//! * **Deterministic.** Every map that reaches the output is a `BTreeMap`; ties
//!   are broken by hash, then file/line. Two runs of the same corpus emit
//!   byte-identical JSON.
//! * **Capped.** Cluster/member/occurrence counts are bounded and the report
//!   says `truncated: true` when something was dropped.
//! * **No new dependencies.** Digests come from [`crate::hash`] (MD5) plus a
//!   local FNV-1a, exactly like the rest of the crate.
//!
//! The module is read-only: it never writes to the repo it scans.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::{json, Value};

/// Default minimum body/block length in non-blank lines.
pub const DEFAULT_MIN_LINES: usize = 4;
/// Default Jaccard similarity for the near-duplicate pass.
pub const DEFAULT_THRESHOLD: f64 = 0.80;
/// Default cap on exact clusters reported.
pub const DEFAULT_MAX_CLUSTERS: usize = 50;
/// Default cap on members listed per cluster.
pub const DEFAULT_MAX_MEMBERS: usize = 20;
/// Default cap on repeated blocks reported.
pub const DEFAULT_MAX_BLOCKS: usize = 50;

/// Members shorter than this many normalised tokens are "trivial" (accessors,
/// forwarding stubs) and never open an exact/near cluster.
const MIN_UNIT_TOKENS: usize = 6;
/// A near-duplicate candidate needs enough structure to shingle.
const MIN_NEAR_TOKENS: usize = 12;
/// Shingle width (normalised tokens).
const SHINGLE: usize = 4;
/// A shingle that occurs in more than this many units is too common to be
/// evidence of duplication; such postings are skipped.
const MAX_POSTINGS: usize = 64;
/// Hard cap on candidate pair comparisons in the near pass.
const MAX_PAIR_COMPARE: usize = 40_000;
/// Cap on occurrences listed per repeated block.
const MAX_OCCURRENCES: usize = 20;

/// Knobs for [`dedupe`].
#[derive(Clone, Debug)]
pub struct DedupeOptions {
    pub min_lines: usize,
    pub threshold: f64,
    pub max_clusters: usize,
    pub max_members: usize,
    pub max_blocks: usize,
}

impl Default for DedupeOptions {
    fn default() -> Self {
        DedupeOptions {
            min_lines: DEFAULT_MIN_LINES,
            threshold: DEFAULT_THRESHOLD,
            max_clusters: DEFAULT_MAX_CLUSTERS,
            max_members: DEFAULT_MAX_MEMBERS,
            max_blocks: DEFAULT_MAX_BLOCKS,
        }
    }
}

// ---------------------------------------------------------------------------
// small stable digests
// ---------------------------------------------------------------------------

/// FNV-1a 64 — stable for the life of the format, unlike `DefaultHasher`.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Order-independent(ish) fold of a sequence of hashes into one.
fn fold_hashes(hs: &[u64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in hs {
        for b in x.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// 16 hex chars of an MD5, matching the `#str:` node-id convention.
fn md5_hex16(s: &str) -> String {
    let d = crate::hash::md5_bytes(s.as_bytes());
    let mut out = String::with_capacity(16);
    for b in &d[..8] {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// token normalisation
// ---------------------------------------------------------------------------

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '@' || c == '$'
}

fn is_ident_cont(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Tokenise `text` into a whitespace-insensitive normal form.
///
/// Comments are dropped, runs of whitespace collapse, and every punctuation
/// character is its own token, so `a + b`, `a+b` and `a  +\n b` all normalise
/// to `a + b`. String and character literals are kept verbatim (a changed
/// literal is a changed body).
fn tokenize(text: &[char]) -> Vec<String> {
    let n = text.len();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < n {
        let c = text[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < n && text[i + 1] == '/' {
            while i < n && text[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && i + 1 < n && text[i + 1] == '*' {
            i += 2;
            while i + 1 < n && !(text[i] == '*' && text[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            let mut tok = String::new();
            tok.push(c);
            i += 1;
            while i < n {
                let d = text[i];
                tok.push(d);
                i += 1;
                if d == '\\' && i < n {
                    tok.push(text[i]);
                    i += 1;
                    continue;
                }
                if d == quote {
                    break;
                }
            }
            out.push(tok);
            continue;
        }
        if is_ident_start(c) {
            let start = i;
            i += 1;
            while i < n && is_ident_cont(text[i]) {
                i += 1;
            }
            out.push(text[start..i].iter().collect());
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            i += 1;
            while i < n
                && (text[i].is_ascii_alphanumeric()
                    || text[i] == '_'
                    || (text[i] == '.' && i + 1 < n && text[i + 1].is_ascii_digit()))
            {
                i += 1;
            }
            out.push(text[start..i].iter().collect());
            continue;
        }
        out.push(c.to_string());
        i += 1;
    }
    out
}

/// Normalised token fingerprint of a method/constructor body: comments dropped,
/// whitespace collapsed to single spaces, string/char literals kept verbatim,
/// every punctuation character its own token. This is the same normal form the
/// dedupe "exact" cluster hashes, exposed for `reshape::functionize`, which must
/// prove two bodies are the same code before it makes one helper out of them.
pub(crate) fn body_fingerprint(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    tokenize(&chars).join(" ")
}

// ---------------------------------------------------------------------------
// per-file extraction
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct LineRec {
    line: usize,
    hash: u64,
    tokens: usize,
}

#[derive(Clone, Debug)]
struct Run {
    /// index into the global file list
    file: usize,
    lines: Vec<LineRec>,
}

#[derive(Clone, Debug)]
struct Unit {
    file: String,
    id: String,
    name: String,
    kind: String,
    owner: String,
    line: usize,
    end_line: usize,
    token_count: usize,
    lines: usize,
    toks: Vec<String>,
    hash: String,
    shingles: Vec<u64>,
}

impl Unit {
    fn member_json(&self) -> Value {
        json!({
            "file": self.file,
            "id": self.id,
            "name": self.name,
            "kind": self.kind,
            "owner": self.owner,
            "line": self.line,
            "endLine": self.end_line,
            "tokens": self.token_count,
            "lines": self.lines,
            "hash": self.hash,
        })
    }
}

fn member_str(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Body kinds worth deduplicating. Properties/fields are excluded (their
/// "bodies" are one-liners and would drown the report).
fn is_body_kind(kind: &str) -> bool {
    matches!(kind, "method" | "constructor" | "destructor")
}

/// Split one file into runs of consecutive non-blank normalised lines.
fn line_runs(text: &[char]) -> Vec<Vec<LineRec>> {
    let mut runs: Vec<Vec<LineRec>> = Vec::new();
    let mut cur: Vec<LineRec> = Vec::new();
    let mut line_no = 1usize;
    let mut start = 0usize;
    let mut idx = 0usize;
    while idx <= text.len() {
        let at_end = idx == text.len();
        if at_end || text[idx] == '\n' {
            let slice = &text[start..idx];
            let toks = tokenize(slice);
            if toks.is_empty() {
                if !cur.is_empty() {
                    runs.push(std::mem::take(&mut cur));
                }
            } else {
                cur.push(LineRec {
                    line: line_no,
                    hash: fnv1a(toks.join(" ").as_bytes()),
                    tokens: toks.len(),
                });
            }
            line_no += 1;
            start = idx + 1;
        }
        idx += 1;
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    runs
}

/// Parse every file once and collect method/ctor bodies plus line runs.
fn collect(
    files: &[String],
    opts: &DedupeOptions,
) -> (Vec<Unit>, Vec<Unit>, Vec<Run>) {
    let mut all_units: Vec<Unit> = Vec::new();
    let mut units: Vec<Unit> = Vec::new();
    let mut runs: Vec<Run> = Vec::new();
    for (fi, path_str) in files.iter().enumerate() {
        let path = Path::new(path_str);
        let display = path_str.clone();
        let text = crate::parser::read_python_text(path).unwrap_or_default();
        let src = crate::parser::Source::new(
            path.to_string_lossy().into_owned(),
            display.clone(),
            text,
            None,
        );
        let mut parser = crate::parser::Parser::new(&src);
        parser.parse();
        let members: Vec<Value> = parser.members.clone();
        let raw = &src.text;

        for m in &members {
            let kind = member_str(m, "kind");
            if !is_body_kind(&kind) {
                continue;
            }
            let (Some(bs), Some(be)) = (
                m.get("_bstart").and_then(Value::as_u64).map(|v| v as usize),
                m.get("_bend").and_then(Value::as_u64).map(|v| v as usize),
            ) else {
                continue;
            };
            if bs >= be || be > raw.len() {
                continue;
            }
            let body = &raw[bs..be];
            let toks = tokenize(body);
            if toks.is_empty() {
                continue;
            }
            let lines = body
                .split(|c| *c == '\n')
                .filter(|l| l.iter().any(|c| !c.is_whitespace()))
                .count()
                .max(1);
            let norm = toks.join(" ");
            let token_count = toks.len();
            let mut shingles: Vec<u64> = Vec::new();
            if token_count >= SHINGLE {
                let mut set: BTreeSet<u64> = BTreeSet::new();
                for w in toks.windows(SHINGLE) {
                    let hs: Vec<u64> = w.iter().map(|t| fnv1a(t.as_bytes())).collect();
                    set.insert(fold_hashes(&hs));
                }
                shingles = set.into_iter().collect();
            }
            let unit = Unit {
                file: display.clone(),
                id: member_str(m, "id"),
                name: member_str(m, "name"),
                kind,
                owner: member_str(m, "owner"),
                line: m
                    .get("line")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| src.line_at(bs) as u64) as usize,
                end_line: m
                    .get("endLine")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| src.line_at(be) as u64) as usize,
                token_count,
                lines,
                toks,
                hash: md5_hex16(&norm),
                shingles,
            };
            let qualifies = unit.lines >= opts.min_lines && unit.token_count >= MIN_UNIT_TOKENS;
            if qualifies {
                units.push(unit.clone());
            }
            all_units.push(unit);
        }

        for run in line_runs(raw) {
            runs.push(Run { file: fi, lines: run });
        }
    }
    (units, all_units, runs)
}

// ---------------------------------------------------------------------------
// exact duplicates
// ---------------------------------------------------------------------------

fn exact_clusters(units: &[Unit], opts: &DedupeOptions) -> (Vec<Value>, BTreeSet<usize>) {
    let mut by_hash: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, u) in units.iter().enumerate() {
        by_hash.entry(u.hash.clone()).or_default().push(i);
    }
    let mut groups: Vec<(String, Vec<usize>)> = by_hash
        .into_iter()
        .filter(|(_, v)| v.len() >= 2)
        .collect();
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));

    // Every member of an exact cluster is excluded from the near pass, even
    // when the cluster itself is past the reporting cap — otherwise identical
    // bodies reappear there as "near" matches.
    let mut in_cluster: BTreeSet<usize> = BTreeSet::new();
    for (_, idxs) in &groups {
        in_cluster.extend(idxs.iter().copied());
    }

    let mut out: Vec<Value> = Vec::with_capacity(groups.len().min(opts.max_clusters));
    for (hash, idxs) in groups {
        if out.len() >= opts.max_clusters {
            break;
        }
        let first = &units[idxs[0]];
        let members: Vec<Value> = idxs
            .iter()
            .take(opts.max_members)
            .map(|&i| units[i].member_json())
            .collect();
        out.push(json!({
            "hash": hash,
            "count": idxs.len(),
            "tokens": first.token_count,
            "lines": first.lines,
            "members": members,
        }));
    }
    (out, in_cluster)
}

fn exact_total(units: &[Unit]) -> usize {
    let mut by_hash: BTreeMap<&str, usize> = BTreeMap::new();
    for u in units {
        *by_hash.entry(u.hash.as_str()).or_insert(0) += 1;
    }
    by_hash.values().filter(|v| **v >= 2).count()
}

// ---------------------------------------------------------------------------
// near duplicates
// ---------------------------------------------------------------------------

fn jaccard(a: &[u64], b: &[u64]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let (mut i, mut j, mut inter) = (0usize, 0usize, 0usize);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            inter += 1;
            i += 1;
            j += 1;
        } else if a[i] < b[j] {
            i += 1;
        } else {
            j += 1;
        }
    }
    let union = a.len() + b.len() - inter;
    if union == 0 {
        1.0
    } else {
        inter as f64 / union as f64
    }
}

struct Uf {
    parent: Vec<usize>,
}

impl Uf {
    fn new(n: usize) -> Self {
        Uf {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, x: usize) -> usize {
        let mut r = x;
        while self.parent[r] != r {
            r = self.parent[r];
        }
        let mut c = x;
        while self.parent[c] != c {
            let nxt = self.parent[c];
            self.parent[c] = r;
            c = nxt;
        }
        r
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
            self.parent[hi] = lo;
        }
    }
}

/// Multi-set union minus the intersection: tokens present in some members but
/// not all, ordered by how many members carry them (desc) then name.
fn distinct_tokens(members: &[&Unit]) -> Vec<String> {
    if members.len() < 2 {
        return Vec::new();
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut present: BTreeMap<&str, usize> = BTreeMap::new();
    for u in members {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for t in &u.toks {
            *counts.entry(t.as_str()).or_insert(0) += 1;
            seen.insert(t.as_str());
        }
        for t in seen {
            *present.entry(t).or_insert(0) += 1;
        }
    }
    let total = members.len();
    let mut out: Vec<(&str, usize)> = present
        .into_iter()
        .filter(|(_, c)| *c < total)
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    out.into_iter().take(20).map(|(t, _)| t.to_string()).collect()
}

fn near_clusters(
    units: &[Unit],
    in_exact: &BTreeSet<usize>,
    opts: &DedupeOptions,
) -> (Vec<Value>, usize, usize, bool) {
    // candidate units: qualifying, not already an exact duplicate, shingled
    let cand: Vec<usize> = (0..units.len())
        .filter(|i| !in_exact.contains(i) && units[*i].token_count >= MIN_NEAR_TOKENS)
        .collect();
    if cand.len() < 2 {
        return (Vec::new(), 0, 0, false);
    }

    // inverted index shingle -> candidate positions
    let mut postings: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (ci, &ui) in cand.iter().enumerate() {
        for &s in &units[ui].shingles {
            postings.entry(s).or_default().push(ci);
        }
    }

    let mut pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
    let mut pairs_truncated = false;
    'outer: for (_sh, list) in postings.iter() {
        if list.len() < 2 || list.len() > MAX_POSTINGS {
            continue;
        }
        for a in 0..list.len() {
            for b in (a + 1)..list.len() {
                if pairs.len() >= MAX_PAIR_COMPARE {
                    pairs_truncated = true;
                    break 'outer;
                }
                pairs.insert((list[a], list[b]));
            }
        }
    }

    let mut uf = Uf::new(cand.len());
    let mut edges: Vec<(usize, usize, f64)> = Vec::new();
    let mut compared = 0usize;
    for &(a, b) in pairs.iter() {
        compared += 1;
        let sim = jaccard(&units[cand[a]].shingles, &units[cand[b]].shingles);
        if sim >= opts.threshold {
            uf.union(a, b);
            edges.push((a, b, sim));
        }
    }

    // group by final root
    let mut comps: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..cand.len() {
        let r = uf.find(i);
        comps.entry(r).or_default().push(i);
    }
    let mut groups: Vec<(usize, Vec<usize>)> = comps
        .into_iter()
        .filter(|(_, v)| v.len() >= 2)
        .collect();
    // deterministic: largest first, then by the sorted candidate index list
    for (_, v) in groups.iter_mut() {
        v.sort();
    }
    groups.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.1.cmp(&b.1)));

    let total = groups.len();
    let mut out: Vec<Value> = Vec::with_capacity(total.min(25));
    let mut truncated = pairs_truncated;
    for (_root, comp) in groups {
        if out.len() >= 25 {
            truncated = true;
            break;
        }
        let refs: Vec<&Unit> = comp.iter().map(|&ci| &units[cand[ci]]).collect();
        let set: BTreeSet<usize> = comp.iter().copied().collect();
        let (mut min_sim, mut max_sim) = (1.0f64, 0.0f64);
        for (a, b, sim) in &edges {
            if set.contains(a) && set.contains(b) {
                min_sim = min_sim.min(*sim);
                max_sim = max_sim.max(*sim);
            }
        }
        if max_sim == 0.0 {
            min_sim = opts.threshold;
            max_sim = opts.threshold;
        }
        let tokens_min = refs.iter().map(|u| u.token_count).min().unwrap_or(0);
        let tokens_max = refs.iter().map(|u| u.token_count).max().unwrap_or(0);
        let members: Vec<Value> = refs
            .iter()
            .take(opts.max_members)
            .map(|u| u.member_json())
            .collect();
        out.push(json!({
            "root": refs[0].hash,
            "count": refs.len(),
            "similarity": {
                "min": (min_sim * 1000.0).round() / 1000.0,
                "max": (max_sim * 1000.0).round() / 1000.0,
            },
            "tokenDelta": [tokens_min, tokens_max],
            "distinctTokens": distinct_tokens(&refs),
            "members": members,
        }));
    }
    (out, total, compared, truncated)
}

// ---------------------------------------------------------------------------
// repeated blocks
// ---------------------------------------------------------------------------

fn block_groups(
    runs: &[Run],
    files: &[String],
    min_lines: usize,
    max_blocks: usize,
) -> (Vec<Value>, usize, bool) {
    if min_lines == 0 {
        return (Vec::new(), 0, false);
    }
    // window hash -> (run index, start offset)
    let mut windows: BTreeMap<u64, Vec<(usize, usize)>> = BTreeMap::new();
    for (ri, run) in runs.iter().enumerate() {
        if run.lines.len() < min_lines {
            continue;
        }
        for s in 0..=(run.lines.len() - min_lines) {
            let win = &run.lines[s..s + min_lines];
            let tokens: usize = win.iter().map(|l| l.tokens).sum();
            let nontrivial = win.iter().filter(|l| l.tokens >= 2).count();
            if tokens < 10.max(2 * min_lines) || nontrivial < 2 {
                continue;
            }
            let hs: Vec<u64> = win.iter().map(|l| l.hash).collect();
            windows
                .entry(fold_hashes(&hs))
                .or_default()
                .push((ri, s));
        }
    }

    // extended block key -> occurrences (file index, first line, last line)
    let mut blocks: BTreeMap<(u64, usize), BTreeSet<(usize, usize, usize)>> = BTreeMap::new();
    let mut dropped = false;
    for (_key, occ) in windows {
        if occ.len() < 2 {
            continue;
        }
        let mut starts: Vec<(usize, usize)> = occ;
        starts.sort();
        starts.dedup();
        if starts.len() > MAX_OCCURRENCES {
            starts.truncate(MAX_OCCURRENCES);
            dropped = true;
        }
        let mut len = min_lines;
        // extend backward while every occurrence agrees on the previous line
        loop {
            let (r0, s0) = starts[0];
            if s0 == 0 {
                break;
            }
            let target = runs[r0].lines[s0 - 1].hash;
            let ok = starts.iter().all(|(r, s)| {
                *s > 0 && runs[*r].lines[*s - 1].hash == target
            });
            if !ok {
                break;
            }
            for st in starts.iter_mut() {
                st.1 -= 1;
            }
            len += 1;
        }
        // extend forward while every occurrence agrees on the next line
        loop {
            let (r0, s0) = starts[0];
            let next = s0 + len;
            if next >= runs[r0].lines.len() {
                break;
            }
            let target = runs[r0].lines[next].hash;
            let ok = starts.iter().all(|(r, s)| {
                let p = s + len;
                p < runs[*r].lines.len() && runs[*r].lines[p].hash == target
            });
            if !ok {
                break;
            }
            len += 1;
        }
        let (r0, s0) = starts[0];
        let hs: Vec<u64> = runs[r0].lines[s0..s0 + len].iter().map(|l| l.hash).collect();
        let key = (fold_hashes(&hs), len);
        let entry = blocks.entry(key).or_default();
        for (r, s) in &starts {
            let first = runs[*r].lines[*s].line;
            let last = runs[*r].lines[*s + len - 1].line;
            entry.insert((runs[*r].file, first, last));
        }
    }

    let total = blocks.len();
    let mut items: Vec<((u64, usize), BTreeSet<(usize, usize, usize)>)> =
        blocks.into_iter().collect();
    items.sort_by(|a, b| {
        b.0 .1
            .cmp(&a.0 .1)
            .then_with(|| a.0 .0.cmp(&b.0 .0))
            .then_with(|| a.1.iter().next().cmp(&b.1.iter().next()))
    });
    let mut out: Vec<Value> = Vec::with_capacity(total.min(max_blocks));
    for ((hash, len), occ) in items {
        if out.len() >= max_blocks {
            dropped = true;
            break;
        }
        let occurrences: Vec<Value> = occ
            .iter()
            .take(MAX_OCCURRENCES)
            .map(|(f, a, b)| {
                json!({
                    "file": files.get(*f).cloned().unwrap_or_default(),
                    "line": a,
                    "endLine": b,
                })
            })
            .collect();
        out.push(json!({
            "hash": format!("{hash:016x}"),
            "lines": len,
            "count": occ.len(),
            "occurrences": occurrences,
        }));
    }
    (out, total, dropped)
}

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

/// Build the duplicate-code report for `repo` under `roots`.
///
/// Read-only, deterministic, and bounded by [`DedupeOptions`]. Always returns a
/// JSON object with `ok: true` (there is no toolchain to be missing here).
pub fn dedupe(
    repo: &Path,
    roots: &[String],
    excludes: &[String],
    opts: &DedupeOptions,
) -> Value {
    let files = crate::parser::discover_files(repo, roots, excludes);
    let (units, all_units, runs) = collect(&files, opts);

    let (exact, in_exact) = exact_clusters(&units, opts);
    let exact_total = exact_total(&units);
    let (near, near_total, compared, near_truncated) = near_clusters(&units, &in_exact, opts);
    let (blocks, block_total, block_truncated) = block_groups(&runs, &files, opts.min_lines, opts.max_blocks);

    let exact_truncated = exact_total > opts.max_clusters;

    json!({
        "ok": true,
        "repo": repo.to_string_lossy(),
        "roots": roots,
        "files": files.len(),
        "methodBodies": all_units.len(),
        "units": units.len(),
        "minLines": opts.min_lines,
        "threshold": opts.threshold,
        "exact": exact,
        "near": near,
        "blocks": blocks,
        "stats": {
            "exactClusters": exact_total,
            "nearClusters": near_total,
            "blockGroups": block_total,
            "pairComparisons": compared,
        },
        "truncated": exact_truncated || near_truncated || block_truncated,
    })
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn now_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    fn fixture(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dedupe-test-{}-{}-{}",
            std::process::id(),
            now_nanos(),
            name
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        for (rel, body) in files {
            let p = dir.join("src").join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, body).unwrap();
        }
        dir
    }

    fn run(dir: &Path, opts: &DedupeOptions) -> Value {
        dedupe(dir, &["src".to_string()], &[], opts)
    }

    #[test]
    fn detects_identical_bodies_and_whitespace_only_differences() {
        let dir = fixture(
            "exact",
            &[(
                "A.cs",
                "namespace N {\n\
                 class A {\n\
                   public int One(int a, int b) {\n\
                     int s = a + b;\n\
                     int t = s * 2;\n\
                     return t + s;\n\
                   }\n\
                   public int Two(int a, int b) {\n\
                     int  s = a+b;\n\
                     int t = s * 2; // same code, re-spaced\n\
                     return t + s;\n\
                   }\n\
                   public int Three(int a, int b) {\n\
                     int s = a - b;\n\
                     int t = s / 2;\n\
                     return t - s;\n\
                   }\n\
                 }\n}\n",
            )],
        );
        let opts = DedupeOptions {
            min_lines: 3,
            ..Default::default()
        };
        let rep = run(&dir, &opts);
        assert_eq!(rep["ok"], json!(true));
        assert_eq!(rep["exact"].as_array().unwrap().len(), 1, "{rep}");
        let cluster = &rep["exact"][0];
        assert_eq!(cluster["count"], json!(2), "{rep}");
        assert_eq!(cluster["members"].as_array().unwrap().len(), 2);
        assert_eq!(cluster["members"][0]["name"], json!("One"));
        assert_eq!(cluster["members"][1]["name"], json!("Two"));
        // Three differs by operators, so it is not in this cluster
        assert_ne!(cluster["members"][0]["hash"], Value::Null);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn trivial_bodies_are_not_reported() {
        let dir = fixture(
            "trivial",
            &[(
                "T.cs",
                "namespace N {\n\
                 class T {\n\
                   public int Get() { return 1; }\n\
                   public int Pair() { return 1; }\n\
                   public void Noop() { }\n\
                   public void Noop2() { }\n\
                 }\n}\n",
            )],
        );
        let rep = run(&dir, &DedupeOptions::default());
        assert_eq!(rep["exact"], json!([]), "{rep}");
        assert_eq!(rep["near"], json!([]), "{rep}");
        assert_eq!(rep["blocks"], json!([]), "{rep}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_near_duplicates_above_threshold() {
        let dir = fixture(
            "near",
            &[(
                "N.cs",
                "namespace N {\n\
                 class N1 {\n\
                   public int Alpha(int a, int b) {\n\
                     int s = a + b;\n\
                     int t = s * 2;\n\
                     int u = t + s;\n\
                     return u - 1;\n\
                   }\n\
                   public int Beta(int a, int b) {\n\
                     int s = a + b;\n\
                     int t = s * 2;\n\
                     int u = t + s;\n\
                     return u - 2;\n\
                   }\n\
                 }\n}\n",
            )],
        );
        let opts = DedupeOptions {
            min_lines: 3,
            threshold: 0.6,
            ..Default::default()
        };
        let rep = run(&dir, &opts);
        // The two bodies differ in one token, so they are not exact
        assert_eq!(rep["exact"].as_array().unwrap().len(), 0, "{rep}");
        let near = rep["near"].as_array().unwrap();
        assert_eq!(near.len(), 1, "{rep}");
        assert_eq!(near[0]["count"], json!(2), "{rep}");
        assert!(near[0]["similarity"]["min"].as_f64().unwrap() >= 0.6);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reports_repeated_line_blocks() {
        let dir = fixture(
            "blocks",
            &[
                (
                    "B.cs",
                    "namespace N {\n\
                     class B1 {\n\
                       void F() {\n\
                         int alpha = 1;\n\
                         int beta = alpha + 2;\n\
                         int gamma = beta * 3;\n\
                         int delta = gamma - 4;\n\
                         Use(delta);\n\
                       }\n\
                     }\n}\n",
                ),
                (
                    "C.cs",
                    "namespace N {\n\
                     class C1 {\n\
                       void G() {\n\
                         int alpha = 1;\n\
                         int beta = alpha + 2;\n\
                         int gamma = beta * 3;\n\
                         int delta = gamma - 4;\n\
                         Use(delta);\n\
                       }\n\
                     }\n}\n",
                ),
            ],
        );
        let rep = run(&dir, &DedupeOptions::default());
        let blocks = rep["blocks"].as_array().unwrap();
        assert!(!blocks.is_empty(), "no repeated block found: {rep}");
        assert!(blocks[0]["lines"].as_u64().unwrap() >= 4, "{rep}");
        assert_eq!(blocks[0]["occurrences"].as_array().unwrap().len(), 2, "{rep}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn output_is_deterministic() {
        let dir = fixture(
            "det",
            &[(
                "D.cs",
                "namespace N {\n\
                 class D {\n\
                   int A(int x) {\n\
                     int p = x + 1;\n\
                     int q = p * 2;\n\
                     return q + p;\n\
                   }\n\
                   int B(int x) {\n\
                     int p = x + 1;\n\
                     int q = p * 2;\n\
                     return q + p;\n\
                   }\n\
                   int C(int x) {\n\
                     int p = x + 3;\n\
                     int q = p * 5;\n\
                     return q + p;\n\
                   }\n\
                 }\n}\n",
            )],
        );
        let opts = DedupeOptions {
            min_lines: 3,
            threshold: 0.5,
            ..Default::default()
        };
        let a = run(&dir, &opts);
        let b = run(&dir, &opts);
        assert_eq!(a, b, "report must be byte-deterministic");
        assert!(!a["exact"].as_array().unwrap().is_empty(), "{a}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn exact_members_never_reappear_as_near_duplicates() {
        // Four identical pairs; only one exact cluster is reported. Every pair
        // must still be excluded from the near pass, or identical bodies leak
        // in with similarity 1.0.
        let mut body = String::from("namespace N {\n  class E {\n");
        for k in 0..4 {
            for name in ["P", "Q"] {
                body.push_str(&format!(
                    "    int {name}{k}(int x) {{\n\
                     \x20     int a = x + {};\n\
                     \x20     int b = a * 2;\n\
                     \x20     int c = b + a;\n\
                     \x20     return c - {};\n\
                     \x20   }}\n",
                    k + 1,
                    k + 1
                ));
            }
        }
        body.push_str("  }\n}\n");
        let dir = fixture("exact-leak", &[("E.cs", body.as_str())]);
        let opts = DedupeOptions {
            min_lines: 3,
            max_clusters: 1,
            ..Default::default()
        };
        let rep = run(&dir, &opts);
        assert_eq!(rep["stats"]["exactClusters"], json!(4), "{rep}");
        assert_eq!(rep["exact"].as_array().unwrap().len(), 1, "{rep}");
        for c in rep["near"].as_array().unwrap() {
            let ms = c["members"].as_array().unwrap();
            for i in 0..ms.len() {
                for j in (i + 1)..ms.len() {
                    assert_ne!(
                        ms[i]["hash"], ms[j]["hash"],
                        "identical bodies leaked into near: {c}"
                    );
                }
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_repo_is_an_empty_report_not_an_error() {
        let rep = dedupe(
            Path::new("/nonexistent-dedupe-repo"),
            &["src".to_string()],
            &[],
            &DedupeOptions::default(),
        );
        assert_eq!(rep["ok"], json!(true));
        assert_eq!(rep["files"], json!(0));
        assert_eq!(rep["exact"], json!([]));
    }

    #[test]
    fn normalize_ignores_whitespace_and_comments() {
        let a: Vec<char> = "int s = a + b; // note".chars().collect();
        let b: Vec<char> = "int   s=a+b;\n/* other */".chars().collect();
        assert_eq!(tokenize(&a), tokenize(&b));
        let c: Vec<char> = "int s = a - b;".chars().collect();
        assert_ne!(tokenize(&a), tokenize(&c));
    }
}
