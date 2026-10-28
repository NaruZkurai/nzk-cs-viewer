//! reshape — the MCP-only `reformat` and `functionize` tools.
//!
//! These are the second half of the owner's priority-(2) "dedupe/functionize +
//! reformat" work. Slice 1 (`csgraph dedupe` / MCP `find_duplicates`) only
//! *reports* duplication; the tools here act on it:
//!
//! * [`reformat`] normalizes one file (or one declaration) to the toolkit
//!   dialect documented in the repo README: `/* */` comments only, no `using`
//!   directives, fully-qualified type names, the
//!   `namespace NS{  public static partial class X{` opener with 2-space
//!   indentation, and trivial one-expression bodies collapsed onto the
//!   signature line. Dry-run by default; `apply` writes through the same
//!   temp+fsync+atomic-rename path the rest of `edit` uses and is gated by the
//!   real Roslyn parse driver.
//! * [`functionize`] takes a dedupe cluster (or explicit node ids), proves the
//!   members' bodies are token-identical, extracts the common body into one new
//!   shared static helper, and rewrites every duplicate site to call it.
//!
//! Both **refuse rather than guess**: a `//` comment whose body contains `*/`,
//! a raw/multi-line string literal, an unsupported `using` directive, members
//! whose bodies differ, a free identifier that cannot be uniquely resolved.
//! Everything else is deterministic (file then line, `BTreeMap` ordering) and
//! byte-for-byte reproducible.
//!
//! Apply-path validity policy: a missing toolchain is NEVER a silent pass. The
//! payload carries `unverified:true`, and `require_validated:true` refuses the
//! write instead (`compile_check::validity_policy`, the same helper
//! `move_decls` uses).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Value};

use crate::edit::{
    allow_outside_root, bget, block_comment_starts, canonical_file, code_tokens, commit_move_files,
    decl_slices, declared_names, disk_text, infer_class_from_path, move_target_path,
    parse_gate_candidates, qualify_map, resolve_declaration, resolve_from_declaration,
    resolve_move_target, sget, str_of, transform_line, write_atomic_sync, MoveFile, DECL_KEYWORDS,
    TYPE_KINDS,
};
use crate::index::Index;

/// C# predefined type keywords. `model::is_keyword` covers the language
/// keywords; these are the contextual/built-in type names a body may mention
/// without a graph declaration.
const PREDEFINED: [&str; 20] = [
    "bool", "byte", "sbyte", "char", "decimal", "double", "float", "int", "uint", "long",
    "ulong", "short", "ushort", "object", "string", "void", "var", "dynamic", "this", "base",
];

fn is_builtin_word(tok: &str) -> bool {
    PREDEFINED.contains(&tok) || crate::model::is_keyword(tok)
}

fn not_indexed() -> Value {
    json!({"ok": false, "error": "nothing indexed"})
}

fn need_str(args: &Value, key: &str) -> Result<String, String> {
    match args.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(format!("argument {key:?} must be a non-empty string")),
        Some(_) => Err(format!("argument {key:?} must be a string")),
        None => Err(format!("argument {key:?} is required")),
    }
}

// ---------------------------------------------------------------------------
// a mode-aware code scanner (strings, chars, comments)
// ---------------------------------------------------------------------------

/// Carry state for [`walk_code`] across lines: inside a `/* … */` block comment,
/// inside a plain multi-line `@"…"` verbatim string, and inside a multi-line
/// verbatim *interpolated* string (`$@"…"` / `@$"…"`), whose continuation lines
/// still need interpolation-hole tracking.
#[derive(Clone, Copy, Default)]
struct ScanCarry {
    block: bool,
    verbatim: bool,
    verbatim_interp: bool,
}

/// One frame of the per-line scanner. `on_code` receives a character only while
/// the top frame is `Code`; an interpolation hole is `Code { hole: true }`, so a
/// nested `"…"` (or `$"…"`, `@"…"`) inside a hole is scanned as a literal — a
/// `//` inside that nested literal is never mistaken for a line comment.
#[derive(Clone, Copy)]
enum Frame {
    Code { hole: bool, depth: u32 },
    Str { verbatim: bool, interp: bool },
    Chr,
    Block,
    Line,
}

/// Walk `text` char by char. `on_code(i, c)` is called for every character that
/// is real code (not inside a string, a character literal, or a comment) and
/// returns `true` to stop. `on_line_comment(i, slashes)` is called at the first
/// `/` of a `//`/`///` comment outside a literal and returns `true` to stop.
///
/// Interpolated strings are fully tracked: a `{…}` hole is scanned as code (with
/// nested strings/comments), so `$"{(s ?? "//")}tail"` is understood. Raw string
/// literals (`"""…"""`), unterminated single-line literals, and an interpolation
/// hole that spans lines are refused: the scanner would have to guess, and this
/// module never guesses.
fn walk_code(
    text: &str,
    carry: &mut ScanCarry,
    on_code: &mut dyn FnMut(usize, char) -> bool,
    on_line_comment: &mut dyn FnMut(usize, usize) -> bool,
) -> Result<(), String> {
    let ch: Vec<char> = text.chars().collect();
    let n = ch.len();
    let mut stack: Vec<Frame> = Vec::new();
    // The base `Code` frame is always present, so popping a carried construct
    // (a block comment, a verbatim string) can never exhaust the stack.
    stack.push(Frame::Code {
        hole: false,
        depth: 0,
    });
    if carry.block {
        stack.push(Frame::Block);
    } else if carry.verbatim_interp {
        stack.push(Frame::Str {
            verbatim: true,
            interp: true,
        });
    } else if carry.verbatim {
        stack.push(Frame::Str {
            verbatim: true,
            interp: false,
        });
    }
    *carry = ScanCarry::default();
    let mut i = 0usize;
    while i < n {
        let c = ch[i];
        // Unreachable by construction (the base frame never pops); returning an
        // error beats panicking the worker thread and hanging the caller.
        let Some(top) = stack.last().copied() else {
            return Err("internal scanner state underflow".into());
        };
        match top {
            Frame::Code { hole, depth } => {
                if c == '/' && i + 1 < n && ch[i + 1] == '/' {
                    let slashes = if i + 2 < n && ch[i + 2] == '/' { 3 } else { 2 };
                    if on_line_comment(i, slashes) {
                        return Ok(());
                    }
                    stack.push(Frame::Line);
                    i += slashes;
                    continue;
                }
                if c == '/' && i + 1 < n && ch[i + 1] == '*' {
                    stack.push(Frame::Block);
                    i += 2;
                    continue;
                }
                if c == '"' {
                    if i + 2 < n && ch[i + 1] == '"' && ch[i + 2] == '"' {
                        return Err("raw string literal (\"\"\") is not supported".into());
                    }
                    stack.push(Frame::Str {
                        verbatim: false,
                        interp: false,
                    });
                    i += 1;
                    continue;
                }
                if c == '@' && i + 1 < n && ch[i + 1] == '"' {
                    stack.push(Frame::Str {
                        verbatim: true,
                        interp: false,
                    });
                    i += 2;
                    continue;
                }
                if c == '$' && i + 1 < n {
                    if ch[i + 1] == '@' && i + 2 < n && ch[i + 2] == '"' {
                        stack.push(Frame::Str {
                            verbatim: true,
                            interp: true,
                        });
                        i += 3;
                        continue;
                    }
                    if ch[i + 1] == '"' {
                        if i + 3 < n && ch[i + 2] == '"' && ch[i + 3] == '"' {
                            return Err("raw string literal (\"\"\") is not supported".into());
                        }
                        stack.push(Frame::Str {
                            verbatim: false,
                            interp: true,
                        });
                        i += 2;
                        continue;
                    }
                }
                if c == '@' && i + 1 < n && ch[i + 1] == '$' && i + 2 < n && ch[i + 2] == '"' {
                    stack.push(Frame::Str {
                        verbatim: true,
                        interp: true,
                    });
                    i += 3;
                    continue;
                }
                if c == '\'' {
                    stack.push(Frame::Chr);
                    i += 1;
                    continue;
                }
                if hole {
                    if c == '{' {
                        stack.pop();
                        stack.push(Frame::Code {
                            hole: true,
                            depth: depth + 1,
                        });
                        i += 1;
                        continue;
                    }
                    if c == '}' {
                        stack.pop();
                        if depth > 0 {
                            stack.push(Frame::Code {
                                hole: true,
                                depth: depth - 1,
                            });
                        }
                        i += 1;
                        continue;
                    }
                }
                if on_code(i, c) {
                    return Ok(());
                }
                i += 1;
            }
            Frame::Str { verbatim, interp } => {
                if interp {
                    if c == '{' {
                        if i + 1 < n && ch[i + 1] == '{' {
                            i += 2;
                            continue;
                        }
                        stack.push(Frame::Code {
                            hole: true,
                            depth: 0,
                        });
                        i += 1;
                        continue;
                    }
                    if c == '}' {
                        if i + 1 < n && ch[i + 1] == '}' {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        continue;
                    }
                    if !verbatim && c == '\\' {
                        i += 2;
                        continue;
                    }
                    if c == '"' {
                        if verbatim && i + 1 < n && ch[i + 1] == '"' {
                            i += 2;
                            continue;
                        }
                        stack.pop();
                        i += 1;
                        continue;
                    }
                    i += 1;
                } else {
                    if !verbatim && c == '\\' {
                        i += 2;
                        continue;
                    }
                    if c == '"' {
                        if verbatim {
                            if i + 1 < n && ch[i + 1] == '"' {
                                i += 2;
                                continue;
                            }
                        }
                        stack.pop();
                    }
                    i += 1;
                }
            }
            Frame::Chr => {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == '\'' {
                    stack.pop();
                }
                i += 1;
            }
            Frame::Block => {
                if c == '*' && i + 1 < n && ch[i + 1] == '/' {
                    stack.pop();
                    i += 2;
                    continue;
                }
                i += 1;
            }
            Frame::Line => {
                if c == '\n' {
                    stack.pop();
                }
                i += 1;
            }
        }
    }
    match stack.last() {
        Some(Frame::Block) => {
            carry.block = true;
            Ok(())
        }
        Some(Frame::Str {
            verbatim: true,
            interp: false,
        }) => {
            carry.verbatim = true;
            Ok(())
        }
        Some(Frame::Str {
            verbatim: true,
            interp: true,
        }) => {
            carry.verbatim_interp = true;
            Ok(())
        }
        Some(Frame::Code { hole: true, .. }) => {
            Err("an interpolation hole that spans lines is not supported".into())
        }
        Some(Frame::Str { .. }) => Err("unterminated string literal is not supported".into()),
        Some(Frame::Chr) => Err("unterminated character literal is not supported".into()),
        Some(Frame::Code { .. }) | Some(Frame::Line) | None => Ok(()),
    }
}

/// The first `//`/`///` comment on one line: `(char index of the slashes,
/// slashes)`. `*carry` is updated to the state at the end of the line.
fn scan_line_comment(
    line: &str,
    carry: &mut ScanCarry,
) -> Result<Option<(usize, usize)>, String> {
    let mut hit: Option<(usize, usize)> = None;
    walk_code(
        line,
        carry,
        &mut |_, _| false,
        &mut |i, n| {
            hit = Some((i, n));
            true
        },
    )?;
    Ok(hit)
}

/// For each line, the scanner state at the *start* of that line, plus one
/// trailing sentinel for the state after the last line.
fn scan_states(lines: &[String]) -> Result<Vec<ScanCarry>, String> {
    let mut out = Vec::with_capacity(lines.len() + 1);
    let mut carry = ScanCarry::default();
    for l in lines {
        out.push(carry);
        scan_line_comment(l, &mut carry)?;
    }
    out.push(carry);
    Ok(out)
}

/// True when a line is inside a `/* */` block or a verbatim string, i.e. its
/// text must not be treated as code.
fn inert(c: ScanCarry) -> bool {
    c.block || c.verbatim || c.verbatim_interp
}

// ---------------------------------------------------------------------------
// pass 1 — comments: `//` and `///` -> `/* … */`
// ---------------------------------------------------------------------------

/// Convert every `//`/`///` line comment in `range` to a `/* … */` block
/// comment, preserving the comment text verbatim. Refuses when the converted
/// body would contain a literal `*/` (there is no way to escape it), and — via
/// [`scan_line_comment`] — when the line has a raw or unterminated literal.
fn convert_comments(
    lines: &mut [String],
    states: &[ScanCarry],
    range: (usize, usize),
) -> Result<(usize, Vec<usize>), String> {
    let mut count = 0usize;
    let mut rows: Vec<usize> = Vec::new();
    for i in range.0..range.1.min(lines.len()) {
        let mut carry = states.get(i).copied().unwrap_or_default();
        let Some((at, slashes)) = scan_line_comment(&lines[i], &mut carry)? else {
            continue;
        };
        let ch: Vec<char> = lines[i].chars().collect();
        let body: String = ch[(at + slashes).min(ch.len())..].iter().collect();
        if body.contains("*/") {
            return Err(format!(
                "line {}: the `//` comment body would contain `*/` and cannot be represented as a block comment; refusing",
                i + 1
            ));
        }
        let code: String = ch[..at].iter().collect();
        let close = if body.is_empty() || body.ends_with(char::is_whitespace) {
            "*/"
        } else {
            " */"
        };
        let new = format!("{code}/*{body}{close}");
        if new != lines[i] {
            lines[i] = new;
            count += 1;
            rows.push(i + 1);
        }
    }
    Ok((count, rows))
}

// ---------------------------------------------------------------------------
// pass 2 — no `using` directives
// ---------------------------------------------------------------------------

/// True for a `using` **statement** (`using (…)`, `using var x = …`), which must
/// be kept, as opposed to a `using` directive.
fn using_statement(line: &str) -> bool {
    let t = line.trim();
    let t = t.strip_prefix("global ").map(str::trim_start).unwrap_or(t);
    let rest = match t.strip_prefix("using") {
        Some(r) => r.trim_start(),
        None => return false,
    };
    rest.starts_with('(') || rest.starts_with("var ") || rest.starts_with("var\t")
}

/// Remove every `using`/`global using` directive line. A `using` directive that
/// is not a single well-formed statement (a line continuation, an exotic alias)
/// is refused rather than half-removed.
fn strip_usings(lines: &mut Vec<String>) -> Result<Vec<usize>, String> {
    let mut keep: Vec<String> = Vec::with_capacity(lines.len());
    let mut rows: Vec<usize> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim();
        let t = t.strip_prefix("global ").map(str::trim_start).unwrap_or(t);
        if !(t.starts_with("using ") || t.starts_with("using\t")) {
            keep.push(l.clone());
            continue;
        }
        if using_statement(l) {
            keep.push(l.clone());
            continue;
        }
        let rest = t["using".len()..].trim_start();
        if t.ends_with(';') && !rest.contains('(') {
            rows.push(i + 1);
            continue;
        }
        return Err(format!(
            "line {}: unsupported `using` directive {:?}; refusing rather than half-removing it",
            i + 1,
            l.trim()
        ));
    }
    *lines = keep;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// shared small helpers
// ---------------------------------------------------------------------------

fn line_tokens(line: &str) -> Vec<crate::edit::Tok> {
    let mut ib = false;
    code_tokens(line, &mut ib)
}

fn brace_deltas(lines: &[String]) -> Vec<i32> {
    let mut carry = ScanCarry::default();
    let mut out: Vec<i32> = Vec::with_capacity(lines.len());
    for l in lines {
        let mut d = 0i32;
        walk_code(
            l,
            &mut carry,
            &mut |_, c| {
                if c == '{' {
                    d += 1;
                } else if c == '}' {
                    d -= 1;
                }
                false
            },
            &mut |_, _| false,
        )
        .ok();
        out.push(d);
    }
    out
}

fn depth_prefix(deltas: &[i32]) -> Vec<i32> {
    let mut out = vec![0i32; deltas.len() + 1];
    for i in 0..deltas.len() {
        out[i + 1] = out[i] + deltas[i];
    }
    out
}

// ---------------------------------------------------------------------------
// pass 3 — fully-qualified type names (graph-backed, conservative)
// ---------------------------------------------------------------------------

/// Every declaration name in `file`, so the declaration that opens on a line is
/// never qualified in its own header.
fn file_decl_names(index: &Index, file: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for id in index.node_ids_of_file(file) {
        if !index.is_declaration_id(&id) {
            continue;
        }
        if let Some(n) = index.node_value(&id) {
            let name = str_of(n, "name");
            if !name.is_empty() {
                out.insert(name.to_string());
            }
        }
    }
    out
}

/// Identifier words that directly continue an ASCII digit on this line. Inside
/// a numeric literal (`1.0E-08f`, `0xE1`, `1_000`) the letters are not type
/// references; `transform_line` still offers them as identifiers, so they are
/// banned rather than rewritten into `1.0NZK.E-08f`. Banning is line-local and
/// only ever under-qualifies.
fn numeric_adjacent_idents(line: &str) -> BTreeSet<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = BTreeSet::new();
    let mut i = 0usize;
    while i + 1 < chars.len() {
        if chars[i].is_ascii_digit()
            && (chars[i + 1] == '_' || chars[i + 1].is_alphabetic())
        {
            let s = i + 1;
            let mut j = s;
            while j < chars.len() && (chars[j] == '_' || chars[j].is_alphanumeric()) {
                j += 1;
            }
            out.insert(chars[s..j].iter().collect());
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Declaration names `edit::declared_names` misses, recovered from the token
/// stream: array declarators (`Type[] name = …`) and the names after the first
/// in a multi-declarator list (`int SV, VC, SS;`). Both are declarations, and
/// qualifying them rewrites a definition into a member access (invalid C#).
fn declarator_list_names(toks: &[crate::edit::Tok]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for w in toks.windows(3) {
        let name_after_bracket = !w[0].ident
            && w[0].text == "]"
            && w[1].ident
            && !w[1].after_dot
            && matches!(w[2].text.as_str(), "=" | ";" | "," | ")" | "]");
        let name_in_list = w[0].text == ","
            && w[1].ident
            && !w[1].after_dot
            && matches!(w[2].text.as_str(), "," | ";" | "=" | ")");
        if name_after_bracket || name_in_list {
            out.insert(w[1].text.clone());
        }
    }
    out
}

/// Lines that belong to an `enum` body (including the declaring line when the
/// whole enum is on it). Enum member names are declarations, not type
/// references, so qualification must leave the whole block alone.
fn enum_line_mask(lines: &[String]) -> Vec<bool> {
    let deltas = brace_deltas(lines);
    let depth = depth_prefix(&deltas);
    let mut mask = vec![false; lines.len()];
    let mut open_depth: Option<i32> = None;
    for i in 0..lines.len() {
        let mut inside = false;
        if let Some(d) = open_depth {
            if depth[i] < d {
                open_depth = None;
            } else {
                inside = true;
            }
        }
        if inside {
            mask[i] = true;
            continue;
        }
        let toks = line_tokens(&lines[i]);
        let has_enum = toks.iter().any(|t| t.ident && t.text == "enum");
        let has_brace = toks.iter().any(|t| t.text == "{");
        if has_enum && has_brace {
            mask[i] = true;
            if deltas[i] > 0 {
                open_depth = Some(depth[i] + 1);
            }
        }
    }
    mask
}

/// Rewrite bare identifiers to their unique fully-qualified name where the
/// graph proves one exists. Identifiers DECLARED in the file, member accesses
/// (`a.B`), built-in words, keywords, and numeric-literal continuations are left
/// alone; nothing is guessed.
fn qualify_lines(
    lines: &mut [String],
    range: (usize, usize),
    map: &BTreeMap<String, String>,
    protected: &BTreeSet<String>,
) -> Result<(usize, Vec<usize>), String> {
    let states = scan_states(lines)?;
    let enums = enum_line_mask(lines);
    let mut count = 0usize;
    let mut rows: Vec<usize> = Vec::new();
    for i in range.0..range.1.min(lines.len()) {
        // Enum member names are declarations; never qualify inside an enum body.
        if enums.get(i).copied().unwrap_or(false) {
            continue;
        }
        // `transform_line` carries only block-comment state, so a line that
        // starts or ends inside a multi-line verbatim string (or a block
        // comment) must not be rewritten at all (under-qualifying is safe;
        // guessing is not).
        if inert(states.get(i).copied().unwrap_or_default())
            || inert(states.get(i + 1).copied().unwrap_or_default())
        {
            continue;
        }
        if lines[i].trim_start().starts_with('#') {
            continue; // never rewrite a preprocessor directive
        }
        let numeric = numeric_adjacent_idents(&lines[i]);
        let mut ib = states.get(i).copied().unwrap_or_default().block;
        let new_text = transform_line(&lines[i], &mut ib, &mut |tok, dot, _follow, prev| {
            if dot || numeric.contains(tok) || protected.contains(tok) || is_builtin_word(tok) {
                return None;
            }
            if prev.map(|p| DECL_KEYWORDS.contains(&p)).unwrap_or(false) {
                return None;
            }
            map.get(tok).cloned()
        });
        if new_text != lines[i] {
            lines[i] = new_text;
            count += 1;
            rows.push(i + 1);
        }
    }
    Ok((count, rows))
}

// ---------------------------------------------------------------------------
// pass 4 — collapse trivial one-expression bodies
// ---------------------------------------------------------------------------

const CONTROL_WORDS: [&str; 9] = [
    "if", "else", "for", "foreach", "while", "switch", "do", "try", "using",
];

fn normalize_open(line: &str) -> String {
    let t = line.trim_end();
    if let Some(stripped) = t.strip_suffix(" {") {
        format!("{stripped}{{")
    } else {
        t.to_string()
    }
}

/// Collapse a member whose body is exactly one brace-free, comment-free line
/// onto its signature: `sig {\n  stmt\n}` -> `sig{stmt}`. Bodies with more than
/// one statement/line, nested braces, control-flow openers, or comments stay
/// expanded. Returns the number collapsed and the 1-based lines.
fn collapse_bodies(lines: &mut Vec<String>, range: (usize, usize)) -> (usize, Vec<usize>) {
    if lines.is_empty() {
        return (0, Vec::new());
    }
    let deltas = brace_deltas(lines);
    let depth = depth_prefix(&deltas);
    let entering: Vec<bool> = scan_states(lines)
        .unwrap_or_else(|_| vec![ScanCarry::default(); lines.len() + 1])
        .iter()
        .map(|c| inert(*c))
        .collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut count = 0usize;
    let mut rows: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        if i >= range.0 && i < range.1 {
            let t = lines[i].trim_end();
            let toks = line_tokens(&lines[i]);
            let first_control = toks
                .iter()
                .find(|tok| tok.ident)
                .map(|tok| CONTROL_WORDS.contains(&tok.text.as_str()))
                .unwrap_or(false);
            let has_paren = toks.iter().any(|tok| tok.text == "(");
            if deltas[i] == 1 && t.ends_with('{') && !entering[i] && has_paren && !first_control {
                let target = depth[i];
                let mut j = i + 1;
                let mut close = None;
                while j < lines.len() {
                    if depth[j + 1] == target && deltas[j] == -1 && lines[j].trim() == "}" {
                        close = Some(j);
                        break;
                    }
                    if depth[j + 1] <= target {
                        break;
                    }
                    j += 1;
                }
                if let Some(j) = close {
                    if j < range.1 {
                        let nonblank: Vec<usize> = (i + 1..j)
                            .filter(|k| !lines[*k].trim().is_empty())
                            .collect();
                        if nonblank.len() == 1 {
                            let b = &lines[nonblank[0]];
                            if deltas[nonblank[0]] == 0
                                && !b.contains("//")
                                && !b.contains("/*")
                                && !b.contains('{')
                                && !b.contains('}')
                            {
                                out.push(format!("{}{}}}", normalize_open(&lines[i]), b.trim()));
                                count += 1;
                                rows.push(i + 1);
                                i = j + 1;
                                continue;
                            }
                        }
                    }
                }
            }
        }
        out.push(lines[i].clone());
        i += 1;
    }
    if count > 0 {
        *lines = out;
    }
    (count, rows)
}

// ---------------------------------------------------------------------------
// pass 5 — namespace/class shape + 2-space indent
// ---------------------------------------------------------------------------

/// Char offsets (into `line`) of every `{` that is real code, in order. Used to
/// locate the class-body brace in the raw text without guessing through
/// strings/comments.
fn code_brace_offsets(line: &str) -> Vec<usize> {
    let mut carry = ScanCarry::default();
    let mut out = Vec::new();
    walk_code(
        line,
        &mut carry,
        &mut |i, c| {
            if c == '{' {
                out.push(i);
            }
            false
        },
        &mut |_, _| false,
    )
    .ok();
    out
}

/// The raw text that trails the `{` at token index `brace_pos` on `line`.
/// `code_tokens` and [`code_brace_offsets`] agree on brace order, so the token
/// index maps to the same code brace in the raw line.
fn tail_after_brace(line: &str, ctoks: &[crate::edit::Tok], brace_pos: usize) -> String {
    let order = ctoks[..=brace_pos]
        .iter()
        .filter(|t| t.text == "{")
        .count();
    let offsets = code_brace_offsets(line);
    match order.checked_sub(1).and_then(|k| offsets.get(k)) {
        Some(&ci) => {
            let cut = line
                .char_indices()
                .nth(ci + 1)
                .map(|(b, _)| b)
                .unwrap_or(line.len());
            line[cut..].trim().to_string()
        }
        None => String::new(),
    }
}

/// True when `tail` contains a `}` that is real code: the class would then close
/// on its opener line, a shape this pass does not model, so the caller skips.
fn tail_closes(tail: &str) -> bool {
    let mut carry = ScanCarry::default();
    let mut hit = false;
    walk_code(
        tail,
        &mut carry,
        &mut |_, c| {
            if c == '}' {
                hit = true;
            }
            false
        },
        &mut |_, _| false,
    )
    .ok();
    hit
}

/// True when the line holds nothing but `}` braces (the namespace closer).
fn only_close_braces(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| c == '}')
}

/// Normalize the file to `namespace NS{  public static partial class X{` with
/// the class body indented two spaces and a `}}` close, but only when the file
/// is exactly one top-level namespace holding exactly one `static` class. Any
/// other shape is reported as `skipped:<reason>` (never guessed); the caller
/// can require the shape with `require_shape:true`.
fn shape_file(lines: &mut Vec<String>) -> (String, Option<String>, Option<String>) {
    if lines.is_empty() {
        return ("not-applicable:empty file".into(), None, None);
    }
    let deltas = brace_deltas(lines);
    let depth = depth_prefix(&deltas);
    let states = scan_states(lines).unwrap_or_else(|_| vec![ScanCarry::default(); lines.len() + 1]);
    let entering: Vec<bool> = states.iter().map(|s| inert(*s)).collect();
    let mut ns_openers: Vec<usize> = Vec::new();
    for i in 0..lines.len() {
        if entering[i] || depth[i] != 0 {
            continue;
        }
        let toks = line_tokens(&lines[i]);
        if toks.len() >= 3 && toks[0].text == "namespace" && toks[1].ident && toks[2].text == "{" {
            ns_openers.push(i);
        }
    }
    if ns_openers.is_empty() {
        return ("not-applicable:no namespace declaration".into(), None, None);
    }
    if ns_openers.len() > 1 {
        return ("skipped:more than one top-level namespace".into(), None, None);
    }
    let i = ns_openers[0];
    let ns_toks = line_tokens(&lines[i]);
    let ns = ns_toks[1].text.clone();
    let target = depth[i];
    let mut j = i + 1;
    let mut ns_close = None;
    while j < lines.len() {
        if depth[j + 1] == target && lines[j].trim_start().starts_with('}') {
            ns_close = Some(j);
            break;
        }
        j += 1;
    }
    let Some(j) = ns_close else {
        return ("skipped:unbalanced namespace block".into(), Some(ns), None);
    };
    // The class may open on the namespace line itself (the canonical
    // `namespace NS{  public static partial class X{`) or on the next line.
    let same_line_kind = ns_toks
        .iter()
        .position(|t| t.ident && TYPE_KINDS.contains(&t.text.as_str()));
    let (c, kind_idx) = match same_line_kind {
        Some(k) => (i, k),
        None => {
            let inner = depth[i] + 1;
            let mut types: Vec<usize> = Vec::new();
            for k in i + 1..j {
                if entering[k] || depth[k] != inner {
                    continue;
                }
                if line_tokens(&lines[k])
                    .iter()
                    .any(|t| t.ident && TYPE_KINDS.contains(&t.text.as_str()))
                {
                    types.push(k);
                }
            }
            if types.is_empty() {
                return (
                    "skipped:no top-level type inside the namespace".into(),
                    Some(ns),
                    None,
                );
            }
            if types.len() > 1 {
                return (
                    "skipped:multiple top-level type declarations".into(),
                    Some(ns),
                    None,
                );
            }
            let c = types[0];
            let k = line_tokens(&lines[c])
                .iter()
                .position(|t| t.ident && TYPE_KINDS.contains(&t.text.as_str()))
                .unwrap_or(0);
            (c, k)
        }
    };
    let ctoks = line_tokens(&lines[c]);
    let kind = ctoks[kind_idx].text.clone();
    let name = match ctoks.get(kind_idx + 1) {
        Some(t) => t.text.clone(),
        None => return ("skipped:cannot parse the type name".into(), Some(ns), None),
    };
    if kind != "class" {
        return (
            format!("skipped:top-level type is a {kind}, not a class"),
            Some(ns),
            Some(name),
        );
    }
    // The class-body brace is the first `{` after the type name (never a
    // `rposition`, which used to pick the namespace brace and then panic on
    // `ctoks[kind_idx..brace_pos]` when the class body opened on the next line).
    let name_idx = kind_idx + 1;
    let brace_pos = ctoks
        .iter()
        .enumerate()
        .skip(name_idx + 1)
        .find(|(_, t)| t.text == "{")
        .map(|(p, _)| p);
    if let Some(bp) = brace_pos {
        if ctoks[kind_idx..bp].iter().any(|t| t.text == ":") {
            return ("skipped:class has a base list".into(), Some(ns), Some(name));
        }
    }
    let mods: Vec<String> = ctoks[..kind_idx]
        .iter()
        .filter(|t| t.ident && !t.after_dot)
        .map(|t| t.text.clone())
        .collect();
    if !mods.iter().any(|m| m == "static") {
        return ("skipped:class is not static".into(), Some(ns), Some(name));
    }
    if mods.iter().any(|m| m == "sealed" || m == "abstract") {
        return (
            "skipped:unsupported class modifiers".into(),
            Some(ns),
            Some(name),
        );
    }
    // `Some(text)` when the namespace closer line must be preserved verbatim
    // (same-line-class shape); `None` re-emits the canonical `}}`.
    let mut closer_verbatim: Option<String> = None;
    let body: Vec<String> = if c == i {
        // The class opens on the namespace line (`namespace N{ class X{`) or on
        // the line right after it (`namespace N{ class X` newline `{`). Either
        // way the class's `{` is removed here and re-emitted by the canonical
        // opener, so every original closer must be preserved.
        let (brace_line, tail) = match brace_pos {
            Some(bp) => (i, tail_after_brace(&lines[i], &ctoks, bp)),
            None => {
                if i + 1 < j && lines[i + 1].trim_start().starts_with('{') {
                    let line = lines[i + 1].clone();
                    let cut = line.find('{').map(|p| p + 1).unwrap_or(line.len());
                    (i + 1, line[cut..].trim().to_string())
                } else {
                    return (
                        "skipped:class body does not open on the namespace line".into(),
                        Some(ns),
                        Some(name),
                    );
                }
            }
        };
        if !only_close_braces(&lines[j]) {
            return (
                "skipped:class and namespace do not close together".into(),
                Some(ns),
                Some(name),
            );
        }
        if tail_closes(&tail) {
            return (
                "skipped:class body closes on the opener line".into(),
                Some(ns),
                Some(name),
            );
        }
        let mut b: Vec<String> = Vec::new();
        if !tail.is_empty() {
            b.push(tail);
        }
        let start = if brace_line == i { i + 1 } else { i + 2 };
        b.extend_from_slice(&lines[start..j]);
        closer_verbatim = Some(lines[j].trim_end().to_string());
        b
    } else {
        if !lines[c].trim_end().ends_with('{') {
            return (
                "skipped:class body does not start on a new line".into(),
                Some(ns),
                Some(name),
            );
        }
        let ctarget = depth[c];
        let mut m = c + 1;
        let mut class_close = None;
        while m < j {
            if depth[m + 1] == ctarget && deltas[m] == -1 && lines[m].trim() == "}" {
                class_close = Some(m);
                break;
            }
            if depth[m + 1] <= ctarget {
                break;
            }
            m += 1;
        }
        let Some(m) = class_close else {
            return ("skipped:unbalanced class block".into(), Some(ns), Some(name));
        };
        if lines[m + 1..j].iter().any(|l| !l.trim().is_empty()) {
            return (
                "skipped:namespace holds more than the class".into(),
                Some(ns),
                Some(name),
            );
        }
        lines[c + 1..m].to_vec()
    };
    let indent = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let new_body: Vec<String> = body
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("  {}", l.get(indent..).unwrap_or("").trim_end())
            }
        })
        .collect();
    let mut out: Vec<String> = Vec::new();
    out.extend_from_slice(&lines[..i]);
    out.push(format!("namespace {ns}{{  public static partial class {name}{{"));
    out.extend(new_body);
    out.push(closer_verbatim.unwrap_or_else(|| "}}".to_string()));
    out.extend_from_slice(&lines[j + 1..]);
    if out != *lines {
        *lines = out;
    }
    ("applied".into(), Some(ns), Some(name))
}

// ---------------------------------------------------------------------------
// plan diff + apply
// ---------------------------------------------------------------------------

fn diff_hunks(before: &[String], after: &[String]) -> Vec<Value> {
    if before == after {
        return Vec::new();
    }
    let mut p = 0usize;
    while p < before.len() && p < after.len() && before[p] == after[p] {
        p += 1;
    }
    let mut s = 0usize;
    while s < before.len() - p
        && s < after.len() - p
        && before[before.len() - 1 - s] == after[after.len() - 1 - s]
    {
        s += 1;
    }
    let b = &before[p..before.len() - s];
    let a = &after[p..after.len() - s];
    let clip = |v: &[String]| -> (Vec<String>, bool) {
        if v.len() > 200 {
            (v[..200].to_vec(), true)
        } else {
            (v.to_vec(), false)
        }
    };
    let (bm, bt) = clip(b);
    let (am, at) = clip(a);
    vec![json!({
        "fromLine": p + 1,
        "before": bm,
        "after": am,
        "beforeTruncated": bt,
        "afterTruncated": at,
    })]
}

struct ApplyResult {
    validated: String,
    unverified: bool,
    diagnostics: Vec<Value>,
    applied: bool,
    /// True when there was nothing to write (the text already matches the
    /// target), so `applied:false` is not a failure.
    noop: bool,
    written: Vec<String>,
    created: Vec<String>,
    error: Option<Value>,
}

fn apply_candidates(
    index: &Index,
    candidates: &[(String, String, bool)],
    apply: bool,
    allow_invalid: bool,
    require_validated: bool,
    toolchain: Option<&str>,
    allow_outside_root: bool,
) -> ApplyResult {
    let mut r = ApplyResult {
        validated: "not-run".to_string(),
        unverified: false,
        diagnostics: Vec::new(),
        applied: false,
        noop: false,
        written: Vec::new(),
        created: Vec::new(),
        error: None,
    };
    if candidates.is_empty() {
        // Nothing to write: report a no-op rather than a misleading
        // `applied:true, validated:"not-run"` on a toolchain-less box.
        r.noop = true;
        return r;
    }
    if !apply {
        return r;
    }
    let pairs: Vec<(String, String)> = candidates
        .iter()
        .map(|(f, c, _)| (f.clone(), c.clone()))
        .collect();
    let gate = match parse_gate_candidates(&pairs, toolchain) {
        Ok(v) => v,
        Err(e) => {
            r.error = Some(json!({
                "ok": false,
                "error": format!("candidate validation failed: {e}"),
            }));
            return r;
        }
    };
    // The validity policy: a missing toolchain is never a silent pass.
    let policy = crate::compile_check::validity_policy(&gate, require_validated);
    r.validated = policy.validated.clone();
    r.unverified = policy.unverified;
    if let Some(refusal) = policy.refusal {
        r.error = Some(refusal);
        return r;
    }
    r.diagnostics = gate["diagnostics"].as_array().cloned().unwrap_or_default();
    if !r.diagnostics.is_empty() && !allow_invalid {
        let n = r.diagnostics.len();
        let diags = std::mem::take(&mut r.diagnostics);
        r.error = Some(json!({
            "ok": false,
            "validated": r.validated,
            "unverified": r.unverified,
            "error": format!(
                "the commit would leave {n} Roslyn parse diagnostic(s); nothing written \
                 (pass `allow_invalid:true` to override)"
            ),
            "diagnostics": diags,
        }));
        return r;
    }
    let mut writes: Vec<MoveFile> = Vec::new();
    for (display, content, _existed) in candidates {
        let (abs, ddisp, dexisted) = match resolve_move_target(index, display, allow_outside_root) {
            Ok(x) => x,
            Err(e) => {
                r.error = Some(json!({"ok": false, "error": e}));
                return r;
            }
        };
        let original = if dexisted { disk_text(index, display) } else { None };
        writes.push(MoveFile {
            display: ddisp,
            abs,
            existed: dexisted,
            content: content.clone(),
            original,
        });
    }
    writes.sort_by(|a, b| a.display.cmp(&b.display));
    // A single file (the reformat case) goes straight through
    // `write_atomic_sync`; a batch (functionize: every duplicate file plus the
    // new helper) uses the all-or-nothing staged commit.
    if writes.len() == 1 {
        let w = &writes[0];
        if !w.existed {
            let dir = w.abs.parent().unwrap_or_else(|| std::path::Path::new("."));
            if !dir.exists() {
                if let Err(e) = std::fs::create_dir_all(dir) {
                    r.error = Some(json!({
                        "ok": false,
                        "error": format!("create {}: {e}", dir.display()),
                    }));
                    return r;
                }
            }
        }
        match write_atomic_sync(&w.abs, w.content.as_bytes()) {
            Ok(()) => {
                r.applied = true;
                r.written = vec![w.display.clone()];
                if !w.existed {
                    r.created = vec![w.display.clone()];
                }
            }
            Err(e) => {
                r.error = Some(json!({"ok": false, "error": e}));
            }
        }
        return r;
    }
    match commit_move_files(&writes) {
        Ok((w, c)) => {
            r.applied = true;
            r.written = w;
            r.created = c;
        }
        Err(e) => {
            r.error = Some(json!({"ok": false, "error": e}));
        }
    }
    r
}

// ---------------------------------------------------------------------------
// reformat
// ---------------------------------------------------------------------------

/// `reformat {file, decl?, apply?, allow_invalid?, require_validated?,
/// require_shape?, qualify?, _dotnet?}` — normalize one file (or one declaration
/// inside it) to the toolkit dialect.
///
/// `qualify` defaults to `true` (the full dialect). `qualify:false` runs every
/// other pass but leaves names alone, which is the only mode whose result is
/// byte-identical in `dump-parser` terms to the untouched source: qualification
/// is the one pass that makes namespace/type prefixes explicit and therefore
/// adds `uses` edges.
///
/// Dry-run by default. `apply` writes through the atomic path and is gated by
/// the Roslyn parse driver, reporting `validated:"roslyn"` or
/// `"skipped:<reason>"` plus `unverified:true` — never a false pass.
pub fn reformat(index: &Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let apply = bget(args, "apply", false);
    let allow_invalid = bget(args, "allow_invalid", false);
    let require_validated = bget(args, "require_validated", false);
    let require_shape = bget(args, "require_shape", false);
    let qualify = bget(args, "qualify", true);
    let raw_file = match need_str(args, "file") {
        Ok(f) => f,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let Some(file) = canonical_file(index, &raw_file) else {
        return json!({"ok": false, "error": format!("{raw_file:?} is not an indexed file")});
    };
    let Some(before) = disk_text(index, &file) else {
        return json!({"ok": false, "error": format!("cannot read {file}")});
    };
    let before_lines: Vec<String> = before.split('\n').map(str::to_string).collect();
    let mut lines = before_lines.clone();
    let n = lines.len();

    let decl_arg = sget(args, "decl").map(str::to_string);
    let mut declaration = Value::Null;
    let scope;
    let mut range = (0usize, n);
    if let Some(q) = &decl_arg {
        let (id, candidates) = resolve_declaration(index, q);
        let Some(id) = id else {
            return json!({
                "ok": false,
                "error": format!("{q:?} does not name exactly one declaration"),
                "candidates": candidates,
            });
        };
        let Some(node) = index.node_value(&id) else {
            return json!({"ok": false, "error": format!("{id} is not indexed")});
        };
        let canon = canonical_file(index, str_of(node, "file")).unwrap_or_default();
        if canon != file {
            return json!({
                "ok": false,
                "error": format!("declaration {id} lives in {canon}, not {file}"),
            });
        }
        let line = node.get("line").and_then(Value::as_i64).unwrap_or(1).max(1);
        let end = node
            .get("endLine")
            .and_then(Value::as_i64)
            .unwrap_or(line)
            .max(line);
        if end as usize > n || line as usize > n {
            return json!({"ok": false, "error": format!("declaration {id} runs past {file}")});
        }
        range = ((line - 1) as usize, end as usize);
        scope = "decl";
        declaration = json!({
            "id": id, "name": str_of(node, "name"), "kind": str_of(node, "kind"),
            "line": line, "endLine": end,
        });
    } else {
        scope = "file";
    }

    // pass 1 — comments
    let states = match scan_states(&lines) {
        Ok(f) => f,
        Err(e) => return json!({"ok": false, "file": file, "error": format!("{file}: {e}")}),
    };
    let (comment_count, comment_rows) = match convert_comments(&mut lines, &states, range) {
        Ok(x) => x,
        Err(e) => return json!({"ok": false, "file": file, "error": format!("{file}: {e}")}),
    };

    // pass 2 — usings (whole file only; a declaration never holds directives)
    let mut using_rows: Vec<usize> = Vec::new();
    if scope == "file" {
        match strip_usings(&mut lines) {
            Ok(rows) => using_rows = rows,
            Err(e) => return json!({"ok": false, "file": file, "error": format!("{file}: {e}")}),
        }
        range = (0, lines.len());
    }

    // pass 3 — full qualification (skippable: the only pass that can move the
    // parser's reference graph, because it makes namespace/type prefixes
    // explicit; `qualify:false` keeps the copy's dump-parser document
    // byte-identical to the untouched source while the other four passes run)
    let (map, ambiguous, qualified_count, qualified_rows) = if qualify {
        // `file_decl_names` covers the graph's declaration nodes in this file;
        // the textual binding sites the graph does not model — most importantly
        // method parameters and locals — must be protected too, or a bare
        // parameter `a` gets rewritten to an unrelated static member `NZK.S.P.a`
        // and the file no longer parses. Under-qualifying is safe; rewriting a
        // local is not.
        let (map, ambiguous) = qualify_map(index);
        let mut protected = file_decl_names(index, &file);
        {
            // A fresh block-comment flag per line: `edit::code_tokens` used to
            // panic (its own stack underflow) when a line was entered with a
            // carried block comment that closes mid-line. Tokenising each line
            // independently over-collects names that sit inside comments, which
            // only under-qualifies — safe.
            let mut toks = Vec::new();
            for l in before_lines.iter() {
                let mut ib = false;
                toks.extend(code_tokens(l, &mut ib));
            }
            protected.extend(declared_names(&toks, ""));
            protected.extend(declarator_list_names(&toks));
        }
        match qualify_lines(&mut lines, range, &map, &protected) {
            Ok((c, rows)) => (map, ambiguous, c, rows),
            Err(e) => return json!({"ok": false, "file": file, "error": format!("{file}: {e}")}),
        }
    } else {
        (BTreeMap::new(), Vec::new(), 0usize, Vec::new())
    };

    // pass 4 — collapse trivial bodies
    let (collapse_count, collapse_rows) = collapse_bodies(&mut lines, range);

    // pass 5 — shape
    let (shape_status, shape_ns, shape_class) = if scope == "file" {
        shape_file(&mut lines)
    } else {
        ("not-applicable:declaration scope".to_string(), None, None)
    };
    if require_shape && !shape_status.starts_with("applied") {
        return json!({
            "ok": false,
            "file": file,
            "error": format!("require_shape:true but the shape could not be normalized: {shape_status}"),
            "passes": {"shape": {"status": shape_status}},
        });
    }

    let after = lines.join("\n");
    let changed = after != before;
    let hunks = diff_hunks(&before_lines, &lines);
    let cap = 300_000usize;
    let truncated = before.len() > cap || after.len() > cap;
    let before_out = if before.len() > cap {
        before[..cap].to_string()
    } else {
        before.clone()
    };
    let after_out = if after.len() > cap {
        after[..cap].to_string()
    } else {
        after.clone()
    };

    let candidates: Vec<(String, String, bool)> = if changed {
        vec![(file.clone(), after.clone(), true)]
    } else {
        Vec::new()
    };
    let outcome = apply_candidates(
        index,
        &candidates,
        apply,
        allow_invalid,
        require_validated,
        sget(args, "_dotnet"),
        allow_outside_root(args),
    );
    if let Some(e) = outcome.error {
        return e;
    }
    let mut body = json!({
        "ok": true,
        "tool": "reformat",
        "file": file,
        "scope": scope,
        "declaration": declaration,
        "dryRun": !apply,
        "changed": changed,
        "truncated": truncated,
        "before": before_out,
        "after": after_out,
        "hunks": hunks,
        "passes": {
            "comments": {"converted": comment_count, "lines": comment_rows},
            "usings": {"removed": using_rows.len(), "lines": using_rows},
            "qualified": {"rewritten": qualified_count, "lines": qualified_rows},
            "collapsed": {"bodies": collapse_count, "lines": collapse_rows},
            "shape": {"status": shape_status, "namespace": shape_ns, "class": shape_class},
        },
        "skipped_ambiguous": ambiguous,
        "validated": outcome.validated,
        "unverified": outcome.unverified,
        "noop": outcome.noop,
        "applied": outcome.applied,
        "written": outcome.written,
        "created": outcome.created,
        "reindex_required": true,
        "note": "after `apply`, call `index` to re-sync the graph with the files on disk",
    });
    if !outcome.diagnostics.is_empty() {
        body["diagnostics"] = json!(outcome.diagnostics);
    }
    body
}

// ---------------------------------------------------------------------------
// functionize
// ---------------------------------------------------------------------------

/// One duplicate member selected for functionize.
struct Site {
    id: String,
    file: String,
    line: i64,
    end: i64,
    name: String,
    kind: String,
    ret: String,
    sig_prefix: String,
    body: String,
    params_text: String,
    params: Vec<String>,
}

/// Resolve the `cluster`/`members`/`ids` argument to declaration nodes.
fn cluster_refs(index: &Index, args: &Value) -> Result<Vec<Value>, Value> {
    let mut raw: Vec<Value> = Vec::new();
    if let Some(c) = args.get("cluster") {
        if let Some(a) = c.as_array() {
            raw.extend(a.iter().cloned());
        } else if let Some(m) = c.get("members").and_then(Value::as_array) {
            raw.extend(m.iter().cloned());
        } else {
            return Err(json!({
                "ok": false,
                "error": "`cluster` must be an array of members or an object with a `members` array",
            }));
        }
    }
    for key in ["members", "ids"] {
        match args.get(key) {
            Some(Value::Array(a)) => raw.extend(a.iter().cloned()),
            Some(Value::String(s)) => {
                for p in s.split(',') {
                    if !p.trim().is_empty() {
                        raw.push(json!(p.trim()));
                    }
                }
            }
            _ => {}
        }
    }
    if raw.is_empty() {
        return Err(json!({
            "ok": false,
            "error": "functionize needs `cluster` (a find_duplicates cluster) or `members`/`ids` \
                      (node ids, or {file,line} references)",
        }));
    }
    let mut out: Vec<Value> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for r in raw {
        let node = if let Some(id) = r.as_str() {
            match resolve_from_declaration(index, id) {
                Ok(n) => n,
                Err(e) => return Err(e),
            }
        } else if let Some(id) = r.get("id").and_then(Value::as_str) {
            match index.node_value(id) {
                Some(n) => n.clone(),
                None => match resolve_from_declaration(index, id) {
                    Ok(n) => n,
                    Err(e) => return Err(e),
                },
            }
        } else {
            let f = r.get("file").and_then(Value::as_str).unwrap_or("");
            let line = r.get("line").and_then(Value::as_i64).unwrap_or(0);
            if f.is_empty() || line < 1 {
                return Err(json!({
                    "ok": false,
                    "error": "a member reference must be a node id string or a {file,line} object",
                }));
            }
            let Some(cf) = canonical_file(index, f) else {
                return Err(json!({"ok": false, "error": format!("{f:?} is not an indexed file")}));
            };
            let found = index.nodes().iter().find(|n| {
                index.is_declaration_id(str_of(n, "id"))
                    && canonical_file(index, str_of(n, "file")).as_deref() == Some(cf.as_str())
                    && n.get("line").and_then(Value::as_i64) == Some(line)
            });
            match found {
                Some(n) => n.clone(),
                None => {
                    return Err(json!({
                        "ok": false,
                        "error": format!("no declaration at {cf}:{line}"),
                    }))
                }
            }
        };
        let id = str_of(&node, "id").to_string();
        if seen.insert(id) {
            out.push(node);
        }
    }
    Ok(out)
}

/// The first `{ … }` body of a member slice: `(text through the opening brace,
/// body text)`. Refuses a slice with stray text after the body.
fn extract_member_body(slice: &str) -> Result<(String, String), String> {
    let ch: Vec<char> = slice.chars().collect();
    let mut open: Option<usize> = None;
    let mut close: Option<usize> = None;
    let mut depth = 0i32;
    let mut carry = ScanCarry::default();
    {
        let mut no_comment = |_: usize, _: usize| false;
        let mut on_code = |i: usize, c: char| -> bool {
            if c == '{' {
                if depth == 0 && open.is_none() {
                    open = Some(i);
                }
                depth += 1;
            } else if c == '}' {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    return true;
                }
            }
            false
        };
        walk_code(slice, &mut carry, &mut on_code, &mut no_comment)?;
    }
    let Some(open) = open else {
        return Err("no body braces found".into());
    };
    let Some(close) = close else {
        return Err("unbalanced body braces".into());
    };
    let tail: String = ch[(close + 1).min(ch.len())..].iter().collect();
    if !tail.trim().is_empty() {
        return Err("unexpected text after the member body".into());
    }
    let sig: String = ch[..=open].iter().collect();
    let body: String = ch[open + 1..close].iter().collect();
    Ok((sig, body))
}

/// Parameter list text between the first `(` and its matching `)`, plus the
/// parameter names in order. Empty for a zero-parameter member.
fn extract_params(sig_prefix: &str) -> Result<(String, Vec<String>), String> {
    let ch: Vec<char> = sig_prefix.chars().collect();
    let Some(open) = ch.iter().position(|c| *c == '(') else {
        return Ok((String::new(), Vec::new()));
    };
    let mut depth = 0i32;
    let mut close = None;
    for (i, c) in ch.iter().enumerate().skip(open) {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        return Err("unbalanced parameter list".into());
    };
    let inner: String = ch[open + 1..close].iter().collect();
    if inner.trim().is_empty() {
        return Ok((inner, Vec::new()));
    }
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut angle = 0i32;
    let mut paren = 0i32;
    let mut bracket = 0i32;
    for c in inner.chars() {
        match c {
            '<' => angle += 1,
            '>' => angle -= 1,
            '(' => paren += 1,
            ')' => paren -= 1,
            '[' => bracket += 1,
            ']' => bracket -= 1,
            ',' if angle == 0 && paren == 0 && bracket == 0 => {
                parts.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        parts.push(cur);
    }
    let mut names: Vec<String> = Vec::new();
    for p in &parts {
        let toks = line_tokens(p);
        let name = toks
            .iter()
            .rev()
            .find(|t| t.ident && !is_builtin_word(&t.text))
            .map(|t| t.text.clone());
        match name {
            Some(n) => names.push(n),
            None => return Err(format!("cannot parse the parameter name in {p:?}")),
        }
    }
    Ok((inner, names))
}

/// Whitespace-collapsed form, for comparing parameter lists.
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn reindent(body: &str, indent: usize) -> Vec<String> {
    let lines: Vec<&str> = body.split('\n').collect();
    let min = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let pad = " ".repeat(indent);
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{pad}{}", l.get(min..).unwrap_or("").trim_end())
            }
        })
        .collect()
}

/// `functionize {cluster|members|ids, newfile, member/name, class?, dir?,
/// namespace?/prefix?, modifiers?, apply?, allow_unresolved?, allow_invalid?,
/// require_validated?, _dotnet?}` — extract a dedupe cluster's common body into
/// one static helper and rewrite every duplicate site to call it.
///
/// Dry-run by default; the apply path is the same all-or-nothing atomic commit
/// `move_decls` uses, gated by the real Roslyn parse driver.
pub fn functionize(index: &Index, args: &Value) -> Value {
    if !index.indexed {
        return not_indexed();
    }
    let apply = bget(args, "apply", false);
    let allow_invalid = bget(args, "allow_invalid", false);
    let allow_unresolved = bget(args, "allow_unresolved", false);
    let require_validated = bget(args, "require_validated", false);
    let members = match cluster_refs(index, args) {
        Ok(m) => m,
        Err(e) => return e,
    };
    if members.len() < 2 {
        return json!({
            "ok": false,
            "error": format!("functionize needs at least two cluster members; {} given", members.len()),
        });
    }
    let Some(raw_dest) = move_target_path(args) else {
        return json!({
            "ok": false,
            "error": "functionize needs a destination: `newfile` (+ optional `dir`)",
        });
    };
    let class_arg = sget(args, "class").map(str::to_string);
    let member_name = match sget(args, "member").or_else(|| sget(args, "name")) {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ => {
            return json!({
                "ok": false,
                "error": "functionize needs `member` (or `name`): the shared helper's member name",
            })
        }
    };
    if !crate::edit::valid_cs_identifier(&member_name) || crate::model::is_keyword(&member_name) {
        return json!({"ok": false, "error": format!("{member_name:?} is not a legal C# member name")});
    }
    let mods = sget(args, "modifiers")
        .map(str::trim)
        .unwrap_or("public static")
        .to_string();

    // ---- gather the member sites ------------------------------------------
    let mut lines_by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut sites: Vec<Site> = Vec::new();
    for node in &members {
        let full = str_of(node, "full").to_string();
        let kind = str_of(node, "kind").to_string();
        if !matches!(kind.as_str(), "method" | "constructor") {
            return json!({
                "ok": false,
                "error": format!("{full}: functionize extracts method/constructor bodies only (got {kind})"),
            });
        }
        let node_file = str_of(node, "file");
        let Some(cfile) = canonical_file(index, node_file) else {
            return json!({"ok": false, "error": format!("{full}: {} is not an indexed file", node_file)});
        };
        if !lines_by_file.contains_key(&cfile) {
            let Some(text) = disk_text(index, &cfile) else {
                return json!({"ok": false, "error": format!("cannot read {cfile}")});
            };
            lines_by_file.insert(cfile.clone(), text.split('\n').map(str::to_string).collect());
        }
        let parts = decl_slices(node);
        let (line, end) = parts
            .iter()
            .find(|(f, _, _)| canonical_file(index, f).as_deref() == Some(cfile.as_str()))
            .cloned()
            .or_else(|| parts.first().cloned())
            .map(|(_, l, e)| (l, e))
            .unwrap_or((
                node.get("line").and_then(Value::as_i64).unwrap_or(1).max(1),
                node.get("endLine").and_then(Value::as_i64).unwrap_or(1).max(1),
            ));
        let file_lines = &lines_by_file[&cfile];
        if line < 1 || end < line || end as usize > file_lines.len() {
            return json!({
                "ok": false,
                "error": format!("{full}: slice {line}..{end} runs past {cfile}"),
            });
        }
        let slice = file_lines[(line - 1) as usize..end as usize].join("\n");
        let (sig_prefix, body) = match extract_member_body(&slice) {
            Ok(x) => x,
            Err(e) => return json!({"ok": false, "error": format!("{full}: {e}")}),
        };
        let (params_text, params) = match extract_params(&sig_prefix) {
            Ok(x) => x,
            Err(e) => return json!({"ok": false, "error": format!("{full}: {e}")}),
        };
        if sig_prefix
            .split('(')
            .next()
            .map(|head| head.contains('<'))
            .unwrap_or(false)
        {
            return json!({
                "ok": false,
                "error": format!("{full}: generic members are not supported by functionize"),
            });
        }
        let ret = {
            let r = str_of(node, "ret").to_string();
            if r.is_empty() { "void".to_string() } else { r }
        };
        let site_name = str_of(node, "name").to_string();
        // The graph may be older than the file (an `apply` without a following
        // `index` shifts line numbers). Never extract a slice that no longer
        // declares its own name: that would silently turn a sibling into the
        // helper.
        if !line_tokens(&sig_prefix)
            .iter()
            .any(|t| t.ident && !t.after_dot && t.text == site_name)
        {
            return json!({
                "ok": false,
                "error": format!(
                    "{cfile}:{line}-{end} no longer declares `{site_name}`; re-run `index` before functionize"
                ),
            });
        }
        sites.push(Site {
            id: str_of(node, "id").to_string(),
            file: cfile,
            line,
            end,
            name: site_name,
            kind,
            ret,
            sig_prefix,
            body,
            params_text,
            params,
        });
    }
    sites.sort_by(|a, b| (a.file.as_str(), a.line).cmp(&(b.file.as_str(), b.line)));

    // ---- prove the bodies are the same code --------------------------------
    let first_fp = crate::dedupe::body_fingerprint(&sites[0].body);
    let mut differs: Vec<Value> = Vec::new();
    for s in &sites[1..] {
        if crate::dedupe::body_fingerprint(&s.body) != first_fp {
            differs.push(json!({"id": s.id, "file": s.file, "line": s.line}));
        }
    }
    if !differs.is_empty() {
        return json!({
            "ok": false,
            "error": format!(
                "functionize requires an exact duplicate cluster: {} member body/bodies differ from {}",
                differs.len(),
                sites[0].id
            ),
            "differs": differs,
        });
    }
    let params0 = squash(&sites[0].params_text);
    let ret0 = sites[0].ret.clone();
    for s in &sites[1..] {
        if squash(&s.params_text) != params0 || s.ret != ret0 {
            return json!({
                "ok": false,
                "error": format!(
                    "functionize requires identical parameter lists and return types; {} differs",
                    s.id
                ),
                "first": {"id": sites[0].id, "params": params0, "ret": ret0},
                "member": {"id": s.id, "params": squash(&s.params_text), "ret": s.ret},
            });
        }
    }

    // ---- target ------------------------------------------------------------
    let class_name = match class_arg {
        Some(c) => c,
        None => match infer_class_from_path(&raw_dest) {
            Some(c) => c,
            None => {
                return json!({
                    "ok": false,
                    "error": "functionize needs `class`, or a qualified `newfile` stem (e.g. \
                              newfile NZK.H.cs -> class H)",
                })
            }
        },
    };
    if !crate::edit::valid_cs_identifier(&class_name) || crate::model::is_keyword(&class_name) {
        return json!({"ok": false, "error": format!("{class_name:?} is not a legal C# class name")});
    }
    let common_ns = {
        let first = str_of(&members[0], "ns").to_string();
        if members.iter().all(|n| str_of(n, "ns") == first) {
            first
        } else {
            String::new()
        }
    };
    let ns = sget(args, "prefix")
        .or_else(|| sget(args, "namespace"))
        .map(|s| s.trim().to_string())
        .unwrap_or(common_ns);
    if !ns.is_empty() && !crate::edit::valid_qualified_name(&ns) {
        return json!({"ok": false, "error": format!("{ns:?} is not a legal namespace")});
    }
    let class_full = if ns.is_empty() {
        class_name.clone()
    } else {
        format!("{ns}.{class_name}")
    };

    // ---- free-identifier check + qualification of the shared body ----------
    let (map, ambiguous) = qualify_map(index);
    let ambiguous_names: BTreeSet<String> = ambiguous
        .iter()
        .filter_map(|v| v["name"].as_str().map(str::to_string))
        .collect();
    let mut declared: BTreeSet<String> = BTreeSet::new();
    for s in &sites {
        let slice = lines_by_file[&s.file][(s.line - 1) as usize..s.end as usize].join("\n");
        let mut toks: Vec<crate::edit::Tok> = Vec::new();
        for l in slice.split('\n') {
            // Fresh block-comment state per line: `code_tokens` underflows its
            // stack if a line is entered inside `/* … */` and closes it
            // mid-line, and `edit.rs` is not ours to change. Tokenising comment
            // text as code only over-collects protected names — safe.
            let mut ib = false;
            toks.extend(code_tokens(l, &mut ib));
        }
        declared.extend(declared_names(&toks, &s.name));
        declared.extend(s.params.iter().cloned());
        declared.insert(s.name.clone());
    }
    let member_names: BTreeSet<String> = sites.iter().map(|s| s.name.clone()).collect();
    let mut unresolved: Vec<Value> = Vec::new();
    let mut qualified_body: Vec<String> = Vec::new();
    {
        // `transform_line` carries block-comment state; a line that *begins*
        // inside a block comment and closes it mid-line would underflow its own
        // scanner (edit.rs). Such lines are passed through verbatim instead —
        // under-qualifying is safe, a worker panic is not.
        let comment_lines = block_comment_starts(&sites[0].body);
        for (k, raw) in sites[0].body.split('\n').enumerate() {
            if comment_lines.get(k).copied().unwrap_or(false) {
                qualified_body.push(raw.to_string());
                continue;
            }
            let mut ib = false;
            let mut hits: Vec<Value> = Vec::new();
            let after = transform_line(raw, &mut ib, &mut |tok, dot, follow, prev| {
                if dot || is_builtin_word(tok) || declared.contains(tok) {
                    return None;
                }
                if prev.map(|p| DECL_KEYWORDS.contains(&p)).unwrap_or(false) {
                    return None;
                }
                if member_names.contains(tok) {
                    hits.push(json!({"name": tok, "reason": "the cluster's own member name"}));
                    return None;
                }
                if let Some(full) = map.get(tok) {
                    return Some(full.clone());
                }
                if follow && !ambiguous_names.contains(tok) {
                    // `UnityEngine.Transform`, `System.String`: a qualifier
                    // prefix (namespace or external type) the graph does not
                    // model. It is already usable as written, so flagging it as
                    // "unresolved" was a false refusal. An *ambiguous*
                    // declaration name still refuses: it may be a local or a
                    // sibling that needs a real qualification.
                    return None;
                }
                hits.push(json!({"name": tok, "reason": "no unique declaration for this identifier"}));
                None
            });
            unresolved.extend(hits);
            qualified_body.push(after);
        }
    }
    unresolved.dedup_by(|a, b| a["name"] == b["name"]);
    if !unresolved.is_empty() && !allow_unresolved {
        let names: Vec<String> = unresolved
            .iter()
            .map(|u| u["name"].as_str().unwrap_or("").to_string())
            .collect();
        return json!({
            "ok": false,
            "error": format!(
                "{} body identifier(s) cannot be uniquely resolved in a static helper ({}); \
                 qualify them, or pass `allow_unresolved:true`",
                unresolved.len(),
                names.join(", ")
            ),
            "unresolved": unresolved,
        });
    }

    // ---- build the helper --------------------------------------------------
    let ret_qualified = if ret0 == "void" || ret0.is_empty() {
        "void".to_string()
    } else {
        map.get(&ret0).cloned().unwrap_or_else(|| ret0.clone())
    };
    let params_qualified = {
        let mut ib = false;
        transform_line(
            &sites[0].params_text,
            &mut ib,
            &mut |tok, dot, _follow, prev| {
                if dot || is_builtin_word(tok) || sites[0].params.iter().any(|p| p == tok) {
                    return None;
                }
                if prev.map(|p| DECL_KEYWORDS.contains(&p)).unwrap_or(false) {
                    return None;
                }
                map.get(tok).cloned()
            },
        )
    };
    let body_pad = if ns.is_empty() { 2 } else { 4 };
    let mut helper_lines: Vec<String> = Vec::new();
    if ns.is_empty() {
        helper_lines.push(format!(
            "{mods} {ret_qualified} {member_name}({params_qualified}){{"
        ));
    } else {
        helper_lines.push(format!(
            "namespace {ns}{{  public static partial class {class_name}{{"
        ));
        helper_lines.push(format!(
            "  {mods} {ret_qualified} {member_name}({params_qualified}){{"
        ));
    }
    for l in reindent(&qualified_body.join("\n"), body_pad) {
        helper_lines.push(l);
    }
    if ns.is_empty() {
        helper_lines.push("}".to_string());
    } else {
        helper_lines.push("  }".to_string());
        helper_lines.push("}}".to_string());
    }
    let helper_content = format!("{}\n", helper_lines.join("\n"));
    let call = format!(
        "{class_full}.{member_name}({})",
        sites[0].params.join(", ")
    );
    let is_void = ret_qualified == "void" || sites[0].kind == "constructor";
    let stmt = if is_void {
        format!("{call};")
    } else {
        format!("return {call};")
    };

    // ---- plan: rewrite every duplicate site --------------------------------
    let mut replacements: BTreeMap<String, Vec<(i64, i64, String)>> = BTreeMap::new();
    let mut edits: Vec<Value> = Vec::new();
    let mut selected: Vec<Value> = Vec::new();
    for s in &sites {
        let sig = normalize_open(s.sig_prefix.trim_end());
        let replacement = format!("{sig}{stmt}}}");
        replacements
            .entry(s.file.clone())
            .or_default()
            .push((s.line, s.end, replacement.clone()));
        edits.push(json!({
            "kind": "replace-body",
            "file": s.file,
            "from": s.line,
            "to": s.end,
            "after": replacement,
        }));
        selected.push(json!({
            "id": s.id, "file": s.file, "line": s.line, "endLine": s.end,
            "name": s.name, "kind": s.kind, "ret": s.ret,
        }));
    }

    // ---- assemble every changed file before staging ------------------------
    let mut candidates: Vec<(String, String, bool)> = Vec::new();
    for (file, mut reps) in replacements {
        reps.sort_by(|a, b| a.0.cmp(&b.0));
        let mut cur = 1i64;
        for (from, _, _) in &reps {
            if *from < cur {
                return json!({
                    "ok": false,
                    "error": format!("{file}: overlapping member slices at line {from}; refusing"),
                });
            }
            cur = *from + 1;
        }
        let file_lines = &lines_by_file[&file];
        let mut out: Vec<String> = Vec::new();
        let mut at = 1i64;
        for (from, to, text) in &reps {
            for l in &file_lines[(at - 1) as usize..(from - 1) as usize] {
                out.push(l.clone());
            }
            for part in text.split('\n') {
                out.push(part.to_string());
            }
            at = to + 1;
        }
        for l in &file_lines[(at - 1) as usize..] {
            out.push(l.clone());
        }
        candidates.push((file.clone(), out.join("\n"), true));
    }

    let (dest_abs, dest_display, dest_existed) =
        match resolve_move_target(index, &raw_dest, allow_outside_root(args)) {
        Ok(x) => x,
        Err(e) => return json!({"ok": false, "error": e}),
    };
    let _ = dest_abs;
    let mut destination_edits: Vec<Value> = Vec::new();
    if dest_existed {
        let Some(existing) = disk_text(index, &dest_display) else {
            return json!({"ok": false, "error": format!("cannot read existing destination {dest_display}")});
        };
        let mut content = existing;
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&helper_content);
        candidates.push((dest_display.clone(), content, true));
        destination_edits.push(json!({"kind": "append", "file": dest_display}));
    } else {
        candidates.push((dest_display.clone(), helper_content.clone(), false));
        destination_edits.push(json!({
            "kind": "create", "file": dest_display, "bytes": helper_content.len(),
        }));
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0));

    let outcome = apply_candidates(
        index,
        &candidates,
        apply,
        allow_invalid,
        require_validated,
        sget(args, "_dotnet"),
        allow_outside_root(args),
    );
    if let Some(e) = outcome.error {
        return e;
    }
    let warnings: Vec<String> = unresolved
        .iter()
        .map(|u| {
            format!(
                "allow_unresolved: `{}` left unqualified ({})",
                u["name"].as_str().unwrap_or(""),
                u["reason"].as_str().unwrap_or("")
            )
        })
        .collect();

    let mut body = json!({
        "ok": true,
        "tool": "functionize",
        "mode": "extract-helper",
        "helper": {
            "file": dest_display,
            "created": !dest_existed,
            "namespace": ns,
            "class": class_name,
            "classFull": class_full,
            "member": member_name,
            "returnType": ret_qualified,
            "params": sites[0].params,
            "signature": format!("{mods} {ret_qualified} {member_name}({params_qualified})"),
            "call": format!("{call};"),
            "content": helper_content,
        },
        "selected": selected,
        "files": candidates.iter().map(|(f, _, _)| f.clone()).collect::<Vec<_>>(),
        "edits": edits,
        "destinationEdits": destination_edits,
        "unresolved": unresolved,
        "ambiguous": ambiguous,
        "warnings": warnings,
        "validated": outcome.validated,
        "unverified": outcome.unverified,
        "noop": outcome.noop,
        "applied": outcome.applied,
        "written": outcome.written,
        "created": outcome.created,
        "reindex_required": true,
        "note": "after `apply`, call `index` to re-sync the graph with the files on disk",
    });
    if !outcome.diagnostics.is_empty() {
        body["diagnostics"] = json!(outcome.diagnostics);
    }
    body
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
fn reshape_fixture_dir() -> Option<std::path::PathBuf> {
    let src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../tools/fixtures/reshape");
    src.is_dir().then_some(src)
}

/// The reshape tests use external fixture files. When the optional testing
/// repo is not linked in, skip them instead of failing the public test run.
macro_rules! require_reshape_fixture {
    () => {
        if reshape_fixture_dir().is_none() {
            eprintln!("[reshape] tools/fixtures/reshape absent; skipping");
            return;
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::fs;
    use std::path::PathBuf;

    /// Copy the read-only fixture tree into a scratch repo so `apply` tests
    /// never touch `tools/fixtures/`.
    fn fixture(tag: &str) -> PathBuf {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tools/fixtures/reshape");
        let dir = std::env::temp_dir().join(format!(
            "csgraph-reshape-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for entry in fs::read_dir(&src).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_file() {
                fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
            }
        }
        dir
    }

    fn indexed(tag: &str) -> (PathBuf, Index) {
        let dir = fixture(tag);
        let mut ix = Index::new(dir.clone());
        ix.build(&[".".to_string()], &[], &Config::default());
        assert!(ix.indexed, "fixture must index");
        (dir, ix)
    }

    fn node_id(ix: &Index, name: &str) -> String {
        ix.nodes()
            .iter()
            .find(|n| {
                str_of(n, "name") == name && ix.is_declaration_id(str_of(n, "id"))
            })
            .map(|n| str_of(n, "id").to_string())
            .unwrap_or_else(|| panic!("no declaration named {name}"))
    }

    #[test]
    fn reformat_dry_run_plans_every_dialect_pass_and_writes_nothing() {
        require_reshape_fixture!();
        let (dir, ix) = indexed("dry");
        let before = fs::read_to_string(dir.join("box.cs")).unwrap();
        let out = reformat(&ix, &json!({"file": "box.cs"}));
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["changed"], json!(true));
        assert_eq!(out["applied"], json!(false));
        assert_eq!(out["dryRun"], json!(true));
        assert_eq!(out["passes"]["comments"]["converted"], json!(2), "{out}");
        assert_eq!(out["passes"]["usings"]["removed"], json!(1), "{out}");
        assert!(out["passes"]["qualified"]["rewritten"].as_u64().unwrap() >= 1, "{out}");
        assert_eq!(out["passes"]["collapsed"]["bodies"], json!(1), "{out}");
        assert_eq!(out["passes"]["shape"]["status"], json!("applied"), "{out}");
        let after = out["after"].as_str().unwrap();
        assert!(after.contains("namespace Fixture{  public static partial class Box{"), "{after}");
        assert!(after.contains("/* a box */"), "{after}");
        assert!(after.contains("/* Adds two numbers. */"), "{after}");
        assert!(!after.contains("using System;"), "{after}");
        assert!(after.contains("return Fixture.Counter.Sum(a, b);"), "{after}");
        assert!(
            after.contains("public static int Add(int a, int b){return Fixture.Counter.Sum(a, b);}"),
            "{after}"
        );
        // the dry run must not have touched the file
        assert_eq!(fs::read_to_string(dir.join("box.cs")).unwrap(), before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reformat_apply_writes_atomically_and_reports_the_gate() {
        require_reshape_fixture!();
        let (dir, ix) = indexed("apply");
        let out = reformat(&ix, &json!({"file": "box.cs", "apply": true}));
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["applied"], json!(true), "{out}");
        assert_eq!(out["written"], json!(["box.cs"]), "{out}");
        let v = out["validated"].as_str().unwrap();
        assert!(
            v == "roslyn" || v.starts_with("skipped:"),
            "validated must be roslyn or skipped:<reason>, got {v}"
        );
        let after = fs::read_to_string(dir.join("box.cs")).unwrap();
        assert!(after.contains("namespace Fixture{  public static partial class Box{"), "{after}");
        assert!(!after.contains("// a box"), "{after}");
        assert!(!after.contains("using System;"), "{after}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reformat_refuses_a_comment_body_containing_the_close_marker() {
        require_reshape_fixture!();
        let (dir, ix) = indexed("unsafe");
        let before = fs::read_to_string(dir.join("unsafe_comment.cs")).unwrap();
        let out = reformat(&ix, &json!({"file": "unsafe_comment.cs"}));
        assert_eq!(out["ok"], json!(false), "{out}");
        assert!(out["error"].as_str().unwrap().contains("*/"), "{out}");
        assert_eq!(fs::read_to_string(dir.join("unsafe_comment.cs")).unwrap(), before);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reformat_skipped_gate_is_unverified_and_require_validated_refuses() {
        require_reshape_fixture!();
        let (dir, ix) = indexed("validity");
        // A missing toolchain: the gate reports skipped, and the payload must say
        // unverified rather than silently passing.
        let out = reformat(
            &ix,
            &json!({"file": "box.cs", "apply": true, "_dotnet": "/nonexistent/csgraph-no-dotnet"}),
        );
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["unverified"], json!(true), "{out}");
        assert!(
            out["validated"].as_str().unwrap().starts_with("skipped:"),
            "{out}"
        );
        // reset the file and require validity: the write must refuse.
        let _ = fs::write(
            dir.join("box.cs"),
            fs::read_to_string(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tools/fixtures/reshape/box.cs"),
            )
            .unwrap(),
        );
        let out = reformat(
            &ix,
            &json!({"file": "box.cs", "apply": true, "require_validated": true,
                    "_dotnet": "/nonexistent/csgraph-no-dotnet"}),
        );
        assert_eq!(out["ok"], json!(false), "{out}");
        assert_eq!(out["unverified"], json!(true), "{out}");
        assert!(out["error"].as_str().unwrap().contains("unverified"), "{out}");
        // the refusal wrote nothing
        assert!(fs::read_to_string(dir.join("box.cs")).unwrap().contains("// a box"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn functionize_dry_run_extracts_a_helper_and_rewrites_every_site() {
        require_reshape_fixture!();
        let (dir, ix) = indexed("fn-dry");
        let a = node_id(&ix, "Twice");
        let b = node_id(&ix, "Double");
        let before_a = fs::read_to_string(dir.join("dup_a.cs")).unwrap();
        let out = functionize(
            &ix,
            &json!({"members": [a, b], "newfile": "Fixture.Shared.cs", "member": "TwiceImpl",
                    "namespace": "Fixture"}),
        );
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["applied"], json!(false));
        assert_eq!(out["helper"]["class"], json!("Shared"));
        assert_eq!(out["helper"]["classFull"], json!("Fixture.Shared"));
        let content = out["helper"]["content"].as_str().unwrap();
        assert!(content.contains("namespace Fixture{  public static partial class Shared{"), "{content}");
        assert!(content.contains("public static int TwiceImpl(int x){"), "{content}");
        assert!(content.contains("int y = x + 1;"), "{content}");
        assert!(content.contains("return y * 2;"), "{content}");
        assert_eq!(out["selected"].as_array().unwrap().len(), 2);
        // dry run: nothing on disk changed
        assert_eq!(fs::read_to_string(dir.join("dup_a.cs")).unwrap(), before_a);
        assert!(!dir.join("Fixture.Shared.cs").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn functionize_apply_writes_the_helper_and_the_calls() {
        require_reshape_fixture!();
        let (dir, ix) = indexed("fn-apply");
        let a = node_id(&ix, "Twice");
        let b = node_id(&ix, "Double");
        let out = functionize(
            &ix,
            &json!({"members": [a, b], "newfile": "Fixture.Shared.cs", "member": "TwiceImpl",
                    "namespace": "Fixture", "apply": true}),
        );
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["applied"], json!(true), "{out}");
        let helper = fs::read_to_string(dir.join("Fixture.Shared.cs")).unwrap();
        assert!(helper.contains("public static int TwiceImpl(int x){"), "{helper}");
        let a_text = fs::read_to_string(dir.join("dup_a.cs")).unwrap();
        assert!(
            a_text.contains("public static int Twice(int x){return Fixture.Shared.TwiceImpl(x);}"),
            "{a_text}"
        );
        let b_text = fs::read_to_string(dir.join("dup_b.cs")).unwrap();
        assert!(
            b_text.contains("public static int Double(int x){return Fixture.Shared.TwiceImpl(x);}"),
            "{b_text}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn functionize_refuses_members_whose_bodies_differ() {
        require_reshape_fixture!();
        let (dir, ix) = indexed("fn-differ");
        let a = node_id(&ix, "Twice");
        let sum = node_id(&ix, "Sum");
        let out = functionize(
            &ix,
            &json!({"members": [a, sum], "newfile": "Fixture.Shared.cs", "member": "TwiceImpl",
                    "namespace": "Fixture"}),
        );
        assert_eq!(out["ok"], json!(false), "{out}");
        assert!(out["error"].as_str().unwrap().contains("differ"), "{out}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn functionize_refuses_a_free_identifier_it_cannot_resolve() {
        require_reshape_fixture!();
        let (dir, _ix) = indexed("fn-unresolved");
        // `Counter.Sum` is uniquely resolvable through the graph, so this body
        // extracts. A body calling an unknown bare method must refuse instead.
        let extra = dir.join("unknown.cs");
        fs::write(
            &extra,
            "namespace Fixture {\n    public static partial class U {\n\
             \x20       public static int One(int z) {\n\
             \x20           return Mystery(z);\n        }\n\
             \x20       public static int Two(int z) {\n\
             \x20           return Mystery(z);\n        }\n    }\n}\n",
        )
        .unwrap();
        let mut ix2 = Index::new(dir.clone());
        ix2.build(&[".".to_string()], &[], &Config::default());
        let one = node_id(&ix2, "One");
        let two = node_id(&ix2, "Two");
        let out = functionize(
            &ix2,
            &json!({"members": [one, two], "newfile": "Fixture.Shared2.cs", "member": "OneImpl",
                    "namespace": "Fixture"}),
        );
        assert_eq!(out["ok"], json!(false), "{out}");
        assert!(
            out["unresolved"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
            "{out}"
        );
        // allow_unresolved makes it a loud warning instead
        let out = functionize(
            &ix2,
            &json!({"members": [one, two], "newfile": "Fixture.Shared2.cs", "member": "OneImpl",
                    "namespace": "Fixture", "allow_unresolved": true}),
        );
        assert_eq!(out["ok"], json!(true), "{out}");
        assert!(!out["warnings"].as_array().unwrap().is_empty(), "{out}");
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod decl_tests {
    use super::*;
    use crate::config::Config;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn reformat_of_a_single_declaration_leaves_the_wrapper_alone() {
        require_reshape_fixture!();
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tools/fixtures/reshape");
        let dir = std::env::temp_dir().join(format!("csgraph-reshape-decl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for entry in fs::read_dir(&src).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_file() {
                fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
            }
        }
        let mut ix = Index::new(dir.clone());
        ix.build(&[".".to_string()], &[], &Config::default());
        assert!(ix.indexed);
        let before = fs::read_to_string(dir.join("box.cs")).unwrap();
        let out = reformat(&ix, &json!({"file": "box.cs", "decl": "Add"}));
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["scope"], json!("decl"), "{out}");
        assert_eq!(out["passes"]["shape"]["status"], json!("not-applicable:declaration scope"));
        assert_eq!(out["passes"]["collapsed"]["bodies"], json!(1), "{out}");
        let after = out["after"].as_str().unwrap();
        // The declaration was reshaped ...
        assert!(after.contains("public static int Add(int a, int b){return Fixture.Counter.Sum(a, b);}"), "{after}");
        // ... but the wrapper and the comment above it were not touched.
        assert!(after.contains("    // a box"), "{after}");
        assert!(after.contains("namespace Fixture {"), "{after}");
        assert_eq!(fs::read_to_string(dir.join("box.cs")).unwrap(), before);
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod idempotence_tests {
    use super::*;
    use crate::config::Config;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn reformat_is_idempotent() {
        require_reshape_fixture!();
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tools/fixtures/reshape");
        let dir = std::env::temp_dir().join(format!("csgraph-reshape-idem-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for entry in fs::read_dir(&src).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_file() {
                fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
            }
        }
        let mut ix = Index::new(dir.clone());
        ix.build(&[".".to_string()], &[], &Config::default());
        let first = reformat(&ix, &json!({"file": "box.cs", "apply": true}));
        assert_eq!(first["applied"], json!(true), "{first}");
        // A second pass over the dialect text must find nothing left to do.
        let second = reformat(&ix, &json!({"file": "box.cs"}));
        assert_eq!(second["ok"], json!(true), "{second}");
        assert_eq!(second["changed"], json!(false), "{second}");
        assert_eq!(second["passes"]["comments"]["converted"], json!(0), "{second}");
        assert_eq!(second["passes"]["usings"]["removed"], json!(0), "{second}");
        assert_eq!(second["passes"]["collapsed"]["bodies"], json!(0), "{second}");
        assert_eq!(second["passes"]["shape"]["status"], json!("applied"), "{second}");
        assert_eq!(second["noop"], json!(true), "{second}");
        // An `apply` with nothing to write is a reported no-op, not a
        // misleading `applied:true` with `validated:"not-run"`.
        let noop = reformat(&ix, &json!({"file": "box.cs", "apply": true}));
        assert_eq!(noop["applied"], json!(false), "{noop}");
        assert_eq!(noop["noop"], json!(true), "{noop}");
        assert_eq!(noop["written"], json!([]), "{noop}");
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod interpolation_tests {
    use super::*;
    use crate::config::Config;
    use std::fs;
    use std::path::PathBuf;

    fn copy_fixture(tag: &str) -> PathBuf {
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tools/fixtures/reshape");
        let dir = std::env::temp_dir().join(format!("csgraph-reshape-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for entry in fs::read_dir(&src).unwrap() {
            let entry = entry.unwrap();
            if entry.path().is_file() {
                fs::copy(entry.path(), dir.join(entry.file_name())).unwrap();
            }
        }
        dir
    }

    /// Regression: `//` inside a nested string literal inside an interpolation
    /// hole must NOT be treated as a line comment (verifier's target-#4 attack;
    /// the old scanner produced `$"{(s ?? "/*")}tail"; */`, corrupting valid C#).
    /// A verbatim string containing `//` and one containing `*/` must also
    /// survive byte-for-byte.
    #[test]
    fn reformat_keeps_interpolated_strings_with_nested_literals() {
        require_reshape_fixture!();
        let dir = copy_fixture("interp");
        let mut ix = Index::new(dir.clone());
        ix.build(&[".".to_string()], &[], &Config::default());
        let before = fs::read_to_string(dir.join("interp.cs")).unwrap();
        let out = reformat(&ix, &json!({"file": "interp.cs"}));
        assert_eq!(out["ok"], json!(true), "{out}");
        let after = out["after"].as_str().unwrap();
        for literal in [
            r#"string x = $"{(s ?? "//")}tail";"#,
            r#"string a = @"a // b";"#,
            r#"string b = @"a */ b";"#,
            r#"string c = @"a /* b";"#,
        ] {
            assert!(
                after.contains(literal),
                "{literal:?} must survive verbatim:\n{after}"
            );
        }
        assert_eq!(out["passes"]["comments"]["converted"], json!(0), "{out}");
        // And the apply path must land a file that still parses (real gate when
        // the toolchain is present, honest skipped otherwise).
        let applied = reformat(&ix, &json!({"file": "interp.cs", "apply": true}));
        assert_eq!(applied["ok"], json!(true), "{applied}");
        let v = applied["validated"].as_str().unwrap();
        assert!(
            v == "roslyn" || v.starts_with("skipped:"),
            "validated must be roslyn or skipped:<reason>, got {v}"
        );
        if v == "roslyn" {
            let empty = applied
                .get("diagnostics")
                .map(|d| d.as_array().map(|a| a.is_empty()).unwrap_or(true))
                .unwrap_or(true);
            assert!(empty, "{applied}");
        }
        let disk = fs::read_to_string(dir.join("interp.cs")).unwrap();
        for literal in [
            r#"string x = $"{(s ?? "//")}tail";"#,
            r#"string a = @"a // b";"#,
            r#"string b = @"a */ b";"#,
            r#"string c = @"a /* b";"#,
        ] {
            assert!(disk.contains(literal), "applied file corrupted:\n{disk}");
        }
        assert!(disk != before, "shape pass should still normalize the wrapper");
        let _ = fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod corpus_regressions {
    use super::*;
    use crate::config::Config;
    use std::fs;
    use std::path::PathBuf;

    /// A scratch repo with exactly the given files, indexed.
    fn repo(tag: &str, files: &[(&str, &str)]) -> (PathBuf, Index) {
        let dir = std::env::temp_dir().join(format!(
            "csgraph-corpus-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for (name, text) in files {
            fs::write(dir.join(name), text).unwrap();
        }
        let mut ix = Index::new(dir.clone());
        ix.build(&[".".to_string()], &[], &Config::default());
        assert!(ix.indexed, "scratch repo must index");
        (dir, ix)
    }

    fn balanced(s: &str) -> bool {
        s.matches('{').count() == s.matches('}').count()
    }

    /// Corpus `ClipFixer.MI.cs.nzk`: a nested class declaration shares the
    /// namespace/class opener line. The old shape pass dropped the trailing
    /// declaration, emitting `class Core{` followed by a bare `{`.
    #[test]
    fn shape_keeps_same_line_nested_class_declaration() {
        let (dir, ix) = repo(
            "nested-same-line",
            &[(
                "nested.cs",
                "namespace NZK { public static partial class Core { public static partial class MenuItems\n{\n  public static int Twice(int x){ return x * 2; }\n}}}\n",
            )],
        );
        let out = reformat(&ix, &json!({"file": "nested.cs"}));
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["passes"]["shape"]["status"], json!("applied"), "{out}");
        let after = out["after"].as_str().unwrap();
        assert!(
            after.contains("namespace NZK{  public static partial class Core{"),
            "{after}"
        );
        assert!(
            after.contains("public static partial class MenuItems"),
            "the nested declaration must survive: {after}"
        );
        assert!(!after.contains("class Core{\n  {\n"), "{after}");
        assert!(balanced(after), "{after}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Corpus `MI.NaN.cs.nzk`: the class body brace opens on the line after the
    /// namespace line. The old shape pass took `rposition` of `{` (the namespace
    /// brace) and panicked slicing `ctoks[kind_idx..brace_pos]`.
    #[test]
    fn shape_handles_class_brace_on_next_line() {
        let (dir, ix) = repo(
            "nested-next-line",
            &[(
                "nested2.cs",
                "#if UNITY_EDITOR\nnamespace NZK{ public static partial class Core\n{ public static class Nested\n{\n  public static int Twice(int x){ return x * 2; }\n}}}\n#endif\n",
            )],
        );
        let out = reformat(&ix, &json!({"file": "nested2.cs"}));
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["passes"]["shape"]["status"], json!("applied"), "{out}");
        let after = out["after"].as_str().unwrap();
        assert!(after.contains("namespace NZK{  public static partial class Core{"), "{after}");
        assert!(after.contains("public static class Nested"), "{after}");
        assert!(after.trim_end().ends_with("#endif"), "{after}");
        assert!(balanced(after), "{after}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Corpus `NZK.Toolkit.cs.nzk`: a line entered inside a `/* … */` block
    /// comment that closes mid-line used to empty the scanner stack and panic
    /// at the next char.
    #[test]
    fn walk_code_closes_carried_block_comment_without_underflow() {
        let mut carry = ScanCarry {
            block: true,
            ..Default::default()
        };
        let mut braces = 0i32;
        let r = walk_code(
            "*/ if (x) { }",
            &mut carry,
            &mut |_, c| {
                if c == '{' {
                    braces += 1;
                }
                false
            },
            &mut |_, _| false,
        );
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(braces, 1);
        assert!(!carry.block, "comment must be closed");
    }

    /// Corpus `Shorthand.cs.nzk`: `1.0E-08f` was rewritten to `1.0NZK.E-08f`.
    #[test]
    fn numeric_literals_are_not_qualified() {
        assert!(numeric_adjacent_idents("public const float W = 1.0E-08f;").contains("E"));
        assert!(numeric_adjacent_idents("public const float W = 1.0E-08f;").contains("f"));
        assert!(!numeric_adjacent_idents("E x = 5;").contains("E"));

        let (dir, ix) = repo(
            "numeric",
            &[
                (
                    "num.cs",
                    "namespace NZK{  public static partial class S{\n  public const float W = 1.0E-08f;\n}}\n",
                ),
                ("e.cs", "namespace NZK{ public class E { } }\n"),
            ],
        );
        let out = reformat(&ix, &json!({"file": "num.cs"}));
        assert_eq!(out["ok"], json!(true), "{out}");
        let after = out["after"].as_str().unwrap();
        assert!(after.contains("1.0E-08f"), "{after}");
        assert!(!after.contains("NZK.E-08"), "{after}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Corpus monoliths: enum member names were rewritten to unrelated static
    /// members (`Head = 5` -> `NZK.S.Head = 5`).
    #[test]
    fn enum_members_are_not_qualified() {
        let (dir, ix) = repo(
            "enum",
            &[
                (
                    "en.cs",
                    "namespace NZK{  public static partial class S{ public enum Slot { None = -1, Head = 5, Extra = 100 } } }\n",
                ),
                (
                    "head.cs",
                    "namespace NZK{ public static partial class T{ public static int Head(){ return 1; } } }\n",
                ),
            ],
        );
        let out = reformat(&ix, &json!({"file": "en.cs"}));
        assert_eq!(out["ok"], json!(true), "{out}");
        let after = out["after"].as_str().unwrap();
        assert!(after.contains("None = -1"), "{after}");
        assert!(after.contains("Head = 5"), "{after}");
        assert!(!after.contains("NZK.T.Head"), "{after}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// `declared_names` misses array declarators and the tail of a
    /// multi-declarator list; both are definitions and must never be rewritten.
    #[test]
    fn declarator_lists_are_protected() {
        let mut ib = false;
        let toks = code_tokens(
            "public static readonly System.String[] S_KnownBoolFlags = { \"x\" };",
            &mut ib,
        );
        let names = declarator_list_names(&toks);
        assert!(names.contains("S_KnownBoolFlags"), "{names:?}");

        let mut ib = false;
        let toks = code_tokens("public int SV, VC, SS;", &mut ib);
        let names = declarator_list_names(&toks);
        assert!(names.contains("VC"), "{names:?}");
        assert!(names.contains("SS"), "{names:?}");
    }

    /// Corpus `Barcodekiller.cs.nzk`: a method parameter `a` was rewritten to
    /// the unrelated static field `NZK.P.a`.
    #[test]
    fn parameters_are_not_qualified() {
        let (dir, ix) = repo(
            "params",
            &[
                (
                    "p.cs",
                    "namespace NZK{  public static partial class S{\n  public static void OK(string a, string b){ Show(a, b); }\n}}\n",
                ),
                ("a.cs", "namespace NZK{ public static class P { public static int a; } }\n"),
            ],
        );
        let out = reformat(&ix, &json!({"file": "p.cs"}));
        assert_eq!(out["ok"], json!(true), "{out}");
        let after = out["after"].as_str().unwrap();
        assert!(after.contains("string a, string b"), "{after}");
        assert!(after.contains("Show(a, b)"), "{after}");
        assert!(!after.contains("NZK.P.a"), "{after}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Corpus monoliths: functionize refused a duplicate body whose only
    /// "unresolved" identifier was a namespace prefix (`UnityEngine` in
    /// `UnityEngine.Debug.Log`). A qualifier the graph does not model is not a
    /// free identifier.
    #[test]
    fn functionize_treats_namespace_prefixes_as_qualified() {
        let (dir, ix) = repo(
            "fnz-prefix",
            &[
                (
                    "dup_a.cs",
                    "namespace NZK{  public static partial class A{\n  public static void Twice(int x){ UnityEngine.Debug.Log(x); }\n}}\n",
                ),
                (
                    "dup_b.cs",
                    "namespace NZK{  public static partial class B{\n  public static void Twice(int x){ UnityEngine.Debug.Log(x); }\n}}\n",
                ),
            ],
        );
        let ids: Vec<String> = ix
            .nodes()
            .iter()
            .filter(|n| str_of(n, "name") == "Twice" && ix.is_declaration_id(str_of(n, "id")))
            .map(|n| str_of(n, "id").to_string())
            .collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        let out = functionize(
            &ix,
            &json!({
                "members": ids,
                "newfile": "NZK.Shared.cs",
                "class": "Shared",
                "member": "TwiceImpl",
                "namespace": "NZK",
            }),
        );
        assert_eq!(out["ok"], json!(true), "{out}");
        assert!(
            out.get("unresolved").and_then(|u| u.as_array()).map(|a| a.is_empty()).unwrap_or(true),
            "a namespace prefix must not be reported unresolved: {out}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// `qualify:false` runs the other passes but leaves names alone — the mode
    /// whose result is graph-identical to the untouched source.
    #[test]
    fn qualify_false_leaves_names_alone() {
        let (dir, ix) = repo(
            "noqual",
            &[
                (
                    "p.cs",
                    "namespace NZK{  public static partial class S{\n  // a comment\n  public static void OK(string a, string b){ Show(a, b); }\n}}\n",
                ),
                ("a.cs", "namespace NZK{ public static class P { public static int a; } }\n"),
            ],
        );
        let out = reformat(&ix, &json!({"file": "p.cs", "qualify": false}));
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["passes"]["qualified"]["rewritten"], json!(0), "{out}");
        assert_eq!(out["passes"]["comments"]["converted"], json!(1), "{out}");
        assert!(
            out["skipped_ambiguous"].as_array().map(|a| a.is_empty()).unwrap_or(false),
            "{out}"
        );
        let after = out["after"].as_str().unwrap();
        assert!(after.contains("Show(a, b)"), "{after}");
        assert!(!after.contains("NZK.P.a"), "{after}");
        let _ = fs::remove_dir_all(&dir);
    }
}
