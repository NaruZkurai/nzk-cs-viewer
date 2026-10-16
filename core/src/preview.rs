//! M3 slice 2 — the read-only change preview.
//!
//! `preview_change` answers one question before any write happens: *which
//! signatures and call sites would a rename or a retype of this declaration
//! change?* It is the dry-run companion to [`crate::edit`]'s write tools:
//!
//! * `rename` reuses the **real** planner — [`crate::edit::invoke`] with
//!   `rename_symbol` and `apply` forced to `false` — so the preview can never
//!   drift from what an apply would do, and it can never write.
//! * `retype` previews replacing the declaration's declared type (return type
//!   of a method, element type of a field/property/event) with `to`, locating
//!   the exact type span on the declaration line.
//!
//! Both ops also report every **call site** the graph attributes to the
//! declaration (incoming `calls`/`constructs`/`uses`/`assigns`/`inherits`
//! edges) with file and line numbers — those are what a human must review even
//! when no line text changes. Output is deterministic (everything sorted, all
//! maps are `BTreeMap`s) and refuses loudly on an unknown or ambiguous symbol,
//! a bad `to`, or a declaration whose type cannot be located. `writes` is
//! always an empty array and the tool holds no write path at all.

use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::edit;
use crate::index::Index;

/// Incoming edge kinds that record a usage site of the declaration (the same
/// set `rename_symbol` replans over).
const USAGE_KINDS: [&str; 5] = ["calls", "constructs", "uses", "assigns", "inherits"];

/// C# member modifiers that are never part of a declared type. Used when the
/// retype preview walks left from the declaration name to find the type span.
const MODIFIERS: [&str; 22] = [
    "public", "private", "protected", "internal", "static", "readonly", "const", "virtual",
    "override", "abstract", "sealed", "extern", "unsafe", "partial", "async", "new", "volatile",
    "required", "fixed", "event", "ref", "in",
];

/// Hard cap on reported sites, mirroring `rename_symbol`'s own cap.
const CAP: usize = 20_000;

/// A refusal body: `ok:false` makes the MCP wrapper set `isError`.
fn fail(error: impl Into<String>) -> Value {
    json!({"ok": false, "tool": "preview_change", "error": error.into()})
}

fn valid_type_name(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    let mut base = s;
    while let Some(b) = base.strip_suffix("[]") {
        base = b.trim_end();
    }
    if base.is_empty() {
        return false;
    }
    match base.find('<') {
        Some(i) => {
            if !base.ends_with('>') || base.len() <= i + 2 {
                return false;
            }
            edit::valid_qualified_name(base[..i].trim())
        }
        None => edit::valid_qualified_name(base),
    }
}

/// One lexical token on a source line, as a byte span.
struct Span {
    text: String,
    start: usize,
    ident: bool,
}

/// Tokenise one line into byte spans, skipping `//` comments, string and char
/// literals. Whitespace is not emitted. This is deliberately tiny: it only has
/// to find the declared type on a declaration line.
fn line_spans(line: &str) -> Vec<Span> {
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let n = chars.len();
    let mut out: Vec<Span> = Vec::new();
    let mut i = 0usize;
    while i < n {
        let (b, c) = chars[i];
        if c == '/' && i + 1 < n && chars[i + 1].1 == '/' {
            break;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            i += 1;
            while i < n {
                let ch = chars[i].1;
                if ch == '\\' {
                    i += 2;
                    continue;
                }
                i += 1;
                if ch == quote {
                    break;
                }
            }
            continue;
        }
        if c == '_' || c.is_alphabetic() {
            let start = b;
            let mut j = i;
            while j < n && (chars[j].1 == '_' || chars[j].1.is_alphanumeric()) {
                j += 1;
            }
            let text: String = chars[i..j].iter().map(|(_, ch)| *ch).collect();
            out.push(Span { text, start, ident: true });
            i = j;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        out.push(Span { text: c.to_string(), start: b, ident: false });
        i += 1;
    }
    out
}

/// The byte span of the declared type on `line`, for the declaration named
/// `name`: the maximal type expression immediately before the name token.
fn declared_type_span(line: &str, name: &str) -> Option<(usize, usize, String)> {
    let toks = line_spans(line);
    // The declaration name: an identifier followed by `(` (method-like) or by
    // `=`/`;`/`{`/nothing (field/property/event). First match wins.
    let mut k: Option<usize> = None;
    for (i, t) in toks.iter().enumerate() {
        if !t.ident || t.text != name {
            continue;
        }
        match toks.get(i + 1).map(|n| n.text.as_str()) {
            Some("(") | Some("=") | Some(";") | Some("{") => {
                k = Some(i);
                break;
            }
            None => {
                k = Some(i);
                break;
            }
            _ => {}
        }
    }
    let k = k?;
    if k == 0 {
        return None;
    }
    // Walk left over the type expression; stop at a modifier, `(`, `=`, `;`,
    // `{`, `}`, `:` or a comma outside generic brackets (none of which are in
    // the type grammar here). `[]` (an array suffix) is type; a `]` that closes
    // an attribute is not.
    let mut j = k as i64 - 1;
    while j >= 0 {
        let t = &toks[j as usize];
        if t.ident {
            if MODIFIERS.contains(&t.text.as_str()) {
                break;
            }
            j -= 1;
            continue;
        }
        match t.text.as_str() {
            "." | "<" | ">" | "," | "?" | "*" => {
                j -= 1;
                continue;
            }
            "]" => {
                if j >= 1 && toks[(j - 1) as usize].text == "[" {
                    j -= 2;
                    continue;
                }
                break;
            }
            _ => break,
        }
    }
    let first = (j + 1) as usize;
    let start = toks[first].start;
    let end_raw = toks[k].start;
    let old = line[start..end_raw].trim_end();
    if old.is_empty() || old == name {
        return None;
    }
    Some((start, start + old.len(), old.to_string()))
}

/// The first line of the declaration slice that mentions `name` as a bare
/// identifier, plus the file it lives in. Read straight from disk so the
/// preview reflects the bytes an apply would rewrite.
fn signature_line(index: &Index, node: &Value) -> Option<(String, i64, String)> {
    let name = edit::str_of(node, "name");
    if name.is_empty() {
        return None;
    }
    for (file, a, b) in edit::decl_slices(node) {
        let Some(text) = edit::disk_text(index, &file) else {
            continue;
        };
        let lines: Vec<&str> = text.split('\n').collect();
        for line in a..=b {
            if line < 1 || line as usize > lines.len() {
                continue;
            }
            let raw = lines[line as usize - 1];
            let mut hit = false;
            let mut ib = false;
            let _ = edit::transform_line(raw, &mut ib, &mut |tok, dot, _f, _p| {
                if !dot && tok == name {
                    hit = true;
                }
                None
            });
            if hit {
                return Some((file, line, raw.to_string()));
            }
        }
    }
    None
}

/// Every incoming usage site of `id`: `(file, line, source id, edge kind)`.
/// Deterministic by construction (BTreeMap key).
fn call_sites(index: &Index, id: &str) -> Vec<(String, i64, String, String)> {
    let mut out: BTreeMap<(String, i64, String, String), ()> = BTreeMap::new();
    for e in index.edges_of(id) {
        let target = e.get("target").and_then(Value::as_str).unwrap_or("");
        let source = e.get("source").and_then(Value::as_str).unwrap_or("");
        if target != id || source == id || source.is_empty() {
            continue;
        }
        let kind = e.get("kind").and_then(Value::as_str).unwrap_or("");
        if !USAGE_KINDS.contains(&kind) {
            continue;
        }
        let Some(src) = index.node_value(source) else {
            continue;
        };
        let file = edit::str_of(src, "file");
        if file.is_empty() {
            continue;
        }
        let lines: Vec<i64> = e
            .get("lines")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        if lines.is_empty() {
            let l = src.get("line").and_then(Value::as_i64).unwrap_or(1).max(1);
            out.insert((file.to_string(), l, source.to_string(), kind.to_string()), ());
        } else {
            for l in lines {
                out.insert((file.to_string(), l, source.to_string(), kind.to_string()), ());
            }
        }
    }
    out.into_keys().collect()
}

/// The `[{file,line,before,after}]` edits a rename would apply, via the real
/// planner (`rename_symbol` with `apply:false`).
fn rename_plan(
    index: &mut Index,
    selector: &str,
    to: &str,
    include_declarations: bool,
) -> Result<Value, Value> {
    let env = edit::invoke(
        index,
        "rename_symbol",
        &json!({
            "from": selector,
            "to": to,
            "apply": false,
            "include_declarations": include_declarations,
        }),
    );
    let text = env
        .get("content")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let body: Value = serde_json::from_str(text)
        .map_err(|_| fail("rename planner returned a malformed envelope"))?;
    if body.get("ok").and_then(Value::as_bool) != Some(true) {
        let mut refusal = fail(
            body.get("error")
                .and_then(Value::as_str)
                .unwrap_or("the rename planner refused the change"),
        );
        if let Some(amb) = body.get("ambiguity") {
            refusal["ambiguity"] = amb.clone();
        }
        return Err(refusal);
    }
    Ok(body)
}

/// Build one preview body. `index` is never mutated in a way that writes: the
/// only planner call forces `apply:false`.
pub fn invoke(index: &mut Index, args: &Value) -> Value {
    if !index.indexed {
        return fail("index a folder first");
    }
    let selector = ["node", "id", "symbol", "from"]
        .iter()
        .find_map(|k| edit::sget(args, k))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let Some(selector) = selector else {
        return fail("one of `node`/`id`/`symbol` is required");
    };
    let op = edit::sget(args, "op").unwrap_or("").trim().to_string();
    if op != "rename" && op != "retype" {
        return fail(format!("`op` must be \"rename\" or \"retype\", not {op:?}"));
    }
    let to = edit::sget(args, "to")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let Some(to) = to else {
        return fail("`to` is required");
    };
    if op == "rename" && !edit::valid_cs_identifier(&to) {
        return fail(format!("{to:?} is not a legal C# identifier"));
    }
    if op == "retype" && !valid_type_name(&to) {
        return fail(format!("{to:?} is not a legal C# type name"));
    }

    let (unique, candidates) = edit::resolve_declaration(index, &selector);
    let Some(id) = unique else {
        let mut body = if candidates.is_empty() {
            fail(format!("unknown symbol: {selector:?}"))
        } else {
            fail(format!("{selector:?} does not resolve to one declaration"))
        };
        body["ambiguity"] = json!(candidates);
        return body;
    };
    let Some(node) = index.node_value(&id).cloned() else {
        return fail(format!("no such node: {id}"));
    };
    let kind = edit::str_of(&node, "kind").to_string();
    if matches!(kind.as_str(), "namespace" | "external" | "string") {
        return fail(format!("{id} is a {kind} node; only code declarations can be previewed"));
    }
    let name = edit::str_of(&node, "name").to_string();
    if name.is_empty() {
        return fail(format!("{id} has no simple name"));
    }

    // Current/proposed signature, read from the declaration line on disk.
    let sig = signature_line(index, &node);
    let current_signature = sig
        .as_ref()
        .map(|(_, _, text)| text.trim().to_string())
        .unwrap_or_else(|| edit::str_of(&node, "signature").to_string());
    let mut proposed_signature = current_signature.clone();
    let mut edits: Vec<Value> = Vec::new();
    let mut files_planned: BTreeSet<String> = BTreeSet::new();
    let mut truncated = false;
    let mut current_type: Option<String> = None;

    if op == "rename" {
        let include_declarations = edit::bget(args, "include_declarations", true);
        let plan = match rename_plan(index, &id, &to, include_declarations) {
            Ok(p) => p,
            Err(refusal) => return refusal,
        };
        truncated = plan.get("truncated").and_then(Value::as_bool).unwrap_or(false);
        for f in plan.get("files_planned").and_then(Value::as_array).into_iter().flatten() {
            if let Some(s) = f.as_str() {
                files_planned.insert(s.to_string());
            }
        }
        for e in plan.get("edits").and_then(Value::as_array).into_iter().flatten() {
            edits.push(json!({
                "file": e.get("file").cloned().unwrap_or(Value::Null),
                "line": e.get("line").cloned().unwrap_or(Value::Null),
                "before": e.get("before").cloned().unwrap_or(Value::Null),
                "after": e.get("after").cloned().unwrap_or(Value::Null),
                "kind": "declaration-or-usage",
            }));
        }
        // Proposed signature: the same identifier rewrite the planner applies,
        // rendered on the declaration's signature line.
        if let Some((_, _, text)) = &sig {
            let mut ib = false;
            proposed_signature = edit::transform_line(text, &mut ib, &mut |tok, dot, _f, _p| {
                if !dot && tok == name {
                    Some(to.clone())
                } else {
                    None
                }
            })
            .trim()
            .to_string();
        }
    } else {
        // retype: replace the declared type on the signature line. The type is
        // read from the source line (authoritative for methods AND fields: the
        // graph only records `ret` for method-like members).
        let Some((file, line, text)) = sig.clone() else {
            return fail(format!(
                "could not read the declaration line for {id}; retype preview is unsupported"
            ));
        };
        let Some((start, end, old_type)) = declared_type_span(&text, &name) else {
            return fail(format!(
                "could not locate the declared type of {id} ({kind}) on {file}:{line}; retype preview is unsupported"
            ));
        };
        current_type = Some(old_type.clone());
        proposed_signature = format!("{}{}{}", &text[..start], to, &text[end..])
            .trim()
            .to_string();
        if proposed_signature != text.trim() {
            files_planned.insert(file.clone());
            edits.push(json!({
                "file": file,
                "line": line,
                "before": text.trim(),
                "after": proposed_signature,
                "kind": "declaration-type",
            }));
        }
    }

    // Call sites (incoming usage edges) with their line numbers.
    let sites = call_sites(index, &id);
    let edit_keys: BTreeSet<(String, i64)> = edits
        .iter()
        .map(|e| {
            (
                e.get("file").and_then(Value::as_str).unwrap_or("").to_string(),
                e.get("line").and_then(Value::as_i64).unwrap_or(0),
            )
        })
        .collect();
    let mut call_site_rows: Vec<Value> = Vec::new();
    let mut affected: BTreeMap<(String, i64, String), Value> = BTreeMap::new();
    for (file, line, source, edge) in &sites {
        let changed = edit_keys.contains(&(file.clone(), *line));
        call_site_rows.push(json!({
            "file": file,
            "line": line,
            "node": source,
            "edge": edge,
            "changed": changed,
        }));
        if affected.len() < CAP {
            let (src_full, src_kind) = index
                .node_value(source)
                .map(|n| {
                    (
                        edit::str_of(n, "full").to_string(),
                        edit::str_of(n, "kind").to_string(),
                    )
                })
                .unwrap_or_default();
            affected.insert(
                (file.clone(), *line, source.clone()),
                json!({
                    "id": source,
                    "full": src_full,
                    "kind": src_kind,
                    "file": file,
                    "line": line,
                    "edge": edge,
                    "reason": if op == "rename" {
                        "references the renamed symbol"
                    } else {
                        "references a declaration whose declared type changes"
                    },
                }),
            );
        }
    }
    edits.sort_by(|a, b| {
        let fa = a.get("file").and_then(Value::as_str).unwrap_or("");
        let fb = b.get("file").and_then(Value::as_str).unwrap_or("");
        let la = a.get("line").and_then(Value::as_i64).unwrap_or(0);
        let lb = b.get("line").and_then(Value::as_i64).unwrap_or(0);
        (fa, la).cmp(&(fb, lb))
    });
    let affected_rows: Vec<Value> = affected.into_values().collect();
    if affected_rows.len() >= CAP {
        truncated = true;
    }
    let mut files_planned: Vec<String> = files_planned.into_iter().collect();
    files_planned.sort();
    files_planned.dedup();

    let mut body = json!({
        "ok": true,
        "tool": "preview_change",
        "readOnly": true,
        "dryRun": true,
        "applied": false,
        "writes": [],
        "op": op,
        "to": to,
        "declaration": {
            "id": edit::str_of(&node, "id"),
            "full": edit::str_of(&node, "full"),
            "label": edit::str_of(&node, "label"),
            "name": name,
            "kind": kind,
            "file": edit::str_of(&node, "file"),
            "line": node.get("line").cloned().unwrap_or(Value::Null),
            "endLine": node.get("endLine").cloned().unwrap_or(Value::Null),
            "ret": edit::str_of(&node, "ret"),
        },
        "currentSignature": current_signature,
        "proposedSignature": proposed_signature,
        "affectedDeclarations": affected_rows,
        "callSites": call_site_rows,
        "edits": edits,
        "filesPlanned": files_planned,
        "counts": {},
        "truncated": truncated,
    });
    // counts are filled after the fact so every number is derived from the
    // arrays actually returned (never recomputed twice).
    let n_edits = body["edits"].as_array().map(Vec::len).unwrap_or(0);
    let n_sites = body["callSites"].as_array().map(Vec::len).unwrap_or(0);
    let n_decls = body["affectedDeclarations"].as_array().map(Vec::len).unwrap_or(0);
    let n_files = body["filesPlanned"].as_array().map(Vec::len).unwrap_or(0);
    body["counts"] = json!({
        "declarations": n_decls,
        "files": n_files,
        "edits": n_edits,
        "callSites": n_sites,
        "changedLines": n_edits,
    });
    if let Some(t) = current_type {
        body["currentType"] = json!(t);
    }
    body
}
