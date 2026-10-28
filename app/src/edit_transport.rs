//! The in-process transport for the engine's MCP-only edit/aux tools.
//!
//! [`crate::edit_wire`] builds the request body and interprets the response —
//! it deliberately performs no I/O. This module is the other half of that
//! seam: it routes a `(name, args)` pair straight into the engine, in-process,
//! with no HTTP client and no async runtime.
//!
//! Routing is by the engine's own predicates, never a hand-copied list:
//!
//! 1. [`csgraph_core::edit::is_edit_tool`] -> [`csgraph_core::edit::invoke`]
//!    (`core/src/edit.rs:110`), which returns the standard MCP text envelope.
//! 2. [`csgraph_core::edit::is_aux_tool`] -> [`csgraph_core::edit::invoke_aux`]
//!    (`core/src/edit.rs:6388`), same envelope. The engine keeps `AUX_TOOLS`
//!    out of `TOOLS` so the `/api/edit` panel never offers them.
//! 3. Anything else -> an error-shaped envelope built here. It is never a
//!    fabricated success: `isError` is `true` and the text names the tool.
//!
//! [`tool_names`] is the op list the panel offers: the engine's `TOOLS`
//! (`edit.rs:53`, 14 names) followed by `AUX_TOOLS` (`edit.rs:79`, 5 names),
//! read from those constants so the list cannot drift.

use csgraph_core::edit;
use csgraph_core::index::Index;
use serde_json::{json, Value};

/// The engine's MCP text envelope, mirroring the private `edit::envelope`:
/// one text block carrying the JSON body plus `isError` when the body reports
/// `"ok": false`. Kept here so an unknown tool is refused in exactly the shape
/// a known tool would use.
fn envelope(body: Value) -> Value {
    let ok = body.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let text = serde_json::to_string(&body).unwrap_or_else(|_| "{\"ok\":false}".to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": !ok,
    })
}

/// The tool names this transport accepts: engine `TOOLS` then `AUX_TOOLS`, in
/// that order. Read from the engine constants, never a hand-copied literal.
pub fn tool_names() -> Vec<String> {
    edit::TOOLS
        .iter()
        .chain(edit::AUX_TOOLS.iter())
        .map(|name| (*name).to_string())
        .collect()
}

/// True when the engine owns `name` as an edit tool or an aux tool.
pub fn is_known(name: &str) -> bool {
    edit::is_edit_tool(name) || edit::is_aux_tool(name)
}

/// Run one MCP-only tool in-process and return its envelope.
///
/// An unknown `name` never reaches the engine: it gets an honest error
/// envelope instead. This is the only branch that can fabricate anything, and
/// it fabricates a refusal.
pub fn invoke(index: &mut Index, name: &str, args: &Value) -> Value {
    if edit::is_edit_tool(name) {
        edit::invoke(index, name, args)
    } else if edit::is_aux_tool(name) {
        edit::invoke_aux(index, name, args)
    } else {
        envelope(json!({"ok": false, "error": format!("unknown tool: {name}")}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cheap empty index: the routing tests below must not need a build.
    fn empty_index() -> Index {
        Index::new(std::env::temp_dir())
    }

    /// The envelope text of a value, or the empty string when the shape is off.
    fn text_of(value: &Value) -> String {
        value
            .get("content")
            .and_then(Value::as_array)
            .and_then(|blocks| blocks.first())
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn tool_names_equals_engine_tools_then_aux_tools() {
        let expected: Vec<String> = edit::TOOLS
            .iter()
            .chain(edit::AUX_TOOLS.iter())
            .map(|name| (*name).to_string())
            .collect();
        assert_eq!(tool_names(), expected);
    }

    #[test]
    fn tool_names_has_nineteen_unique_names() {
        let names = tool_names();
        assert_eq!(names.len(), 19, "14 edit + 5 aux: {names:?}");
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "duplicate op name in {names:?}");
        assert_eq!(names.first().map(String::as_str), Some("fuzzy_search"));
        assert_eq!(names.last().map(String::as_str), Some("workspace"));
    }

    #[test]
    fn is_known_is_true_for_every_engine_tool_name() {
        for name in tool_names() {
            assert!(is_known(&name), "engine owns {name:?}");
        }
    }

    #[test]
    fn is_known_is_false_for_empty_and_bogus_names() {
        assert!(!is_known(""));
        assert!(!is_known("nope"));
        assert!(!is_known("grep "), "trailing space is not the tool name");
    }

    #[test]
    fn unknown_tool_is_error_shaped_and_never_claims_success() {
        let mut index = empty_index();
        let out = invoke(&mut index, "nope", &json!({}));
        assert_eq!(out.get("isError").and_then(Value::as_bool), Some(true));
        let text = text_of(&out);
        assert!(text.contains("unknown tool: nope"), "text was {text:?}");
        assert!(
            !text.contains("\"ok\":true"),
            "refusal must not claim success: {text:?}"
        );
    }

    #[test]
    fn empty_tool_name_is_error_shaped_too() {
        let mut index = empty_index();
        let out = invoke(&mut index, "", &json!({}));
        assert_eq!(out.get("isError").and_then(Value::as_bool), Some(true));
        assert!(text_of(&out).contains("unknown tool"));
    }

    #[test]
    fn an_aux_tool_reaches_invoke_aux_not_the_unknown_branch() {
        let mut index = empty_index();
        // `workspace` is read-only and answers from a fresh Index.
        let out = invoke(&mut index, "workspace", &json!({}));
        assert_eq!(out.get("isError").and_then(Value::as_bool), Some(false));
        assert!(!text_of(&out).contains("unknown tool"));
    }

    #[test]
    fn an_edit_tool_is_not_refused_as_unknown() {
        let mut index = empty_index();
        // `grep` on an unbuilt index refuses for its own reason, not routing.
        let out = invoke(&mut index, "grep", &json!({}));
        let text = text_of(&out);
        assert!(
            !text.contains("unknown tool: grep"),
            "edit tool must route to edit::invoke: {text:?}"
        );
    }
}
