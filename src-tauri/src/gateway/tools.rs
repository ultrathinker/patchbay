//! Namespacing: merge upstream tools under `<jack>__<tool>` and split on the
//! first `__` to route calls back (MASTER_PLAN D4).
//!
//! Jack names are validated elsewhere (`config::validate`: `[A-Za-z0-9_-]+`, no
//! `__`, <= 40, unique). Here we only build/split merged names and guard against
//! an over-long merged name (> 64 chars) by logging a loud warning — truncation
//! with a deterministic hash is a documented follow-up; we never silently emit
//! an invalid name.

use serde_json::{json, Value};

use crate::utils::log::log;

/// Separator between jack name and upstream tool name.
const SEP: &str = "__";

/// Maximum length of a merged tool name (MASTER_PLAN: warn past 64).
const MAX_MERGED_LEN: usize = 64;

/// Build the namespaced tool name: `<jack>__<tool>`.
pub fn namespace(jack: &str, tool: &str) -> String {
    format!("{}{}{}", jack, SEP, tool)
}

/// Split a namespaced name on the FIRST `__` into `(jack, tool)`. Returns `None`
/// if there is no `__` (the name is not namespaced / unknown).
///
/// Splitting on the first separator is required because an upstream tool name is
/// not itself restricted from containing `__`.
pub fn split_namespaced(name: &str) -> Option<(&str, &str)> {
    name.split_once(SEP)
}

/// Rewrite one upstream tool definition's `name` to the namespaced form, leaving
/// all other fields (description, inputSchema, …) untouched. Logs a warning if
/// the merged name would exceed [`MAX_MERGED_LEN`].
pub fn namespaced_tool(jack: &str, tool: &Value) -> Value {
    let upstream_name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let merged = namespace(jack, upstream_name);
    if merged.chars().count() > MAX_MERGED_LEN {
        log(&format!(
            "tools: WARNING merged tool name '{}' exceeds {} chars (truncation-with-hash is a follow-up)",
            merged, MAX_MERGED_LEN
        ));
    }
    let mut out = tool.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert("name".to_string(), Value::String(merged));
    } else {
        // Not an object (unexpected for a tool def): emit a minimal valid entry.
        return json!({ "name": merged });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn namespace_joins_with_double_underscore() {
        assert_eq!(namespace("everything", "echo"), "everything__echo");
    }

    #[test]
    fn split_on_first_separator() {
        // A tool whose upstream name itself contains `__` splits on the FIRST.
        assert_eq!(
            split_namespaced("prod__do__thing"),
            Some(("prod", "do__thing"))
        );
    }

    #[test]
    fn split_none_without_separator() {
        assert_eq!(split_namespaced("echo"), None);
        // A single underscore is NOT the separator.
        assert_eq!(split_namespaced("prod_x"), None);
        assert_eq!(split_namespaced("prod-x"), None);
    }

    #[test]
    fn split_empty_parts_are_well_defined() {
        // Leading/trailing separators still split (caller decides validity).
        assert_eq!(split_namespaced("__echo"), Some(("", "echo")));
        assert_eq!(split_namespaced("prod__"), Some(("prod", "")));
    }

    #[test]
    fn namespaced_tool_rewrites_name_only() {
        let tool = json!({
            "name": "echo",
            "description": "Echo back",
            "inputSchema": { "type": "object" }
        });
        let out = namespaced_tool("everything", &tool);
        assert_eq!(out["name"], "everything__echo");
        // Other fields preserved verbatim.
        assert_eq!(out["description"], "Echo back");
        assert_eq!(out["inputSchema"]["type"], "object");
    }

    #[test]
    fn namespaced_tool_handles_non_object_input() {
        // Defensive: a malformed upstream tool entry still yields a valid tool.
        let out = namespaced_tool("prod", &json!("not-an-object"));
        assert_eq!(out["name"], "prod__");
    }

    #[test]
    fn merged_name_over_64_chars_warns_without_truncating() {
        // 60-char jack + 2-char sep + 10-char tool == 72 (> 64). S4 warns only.
        let long_jack = "j".repeat(60);
        let merged = namespace(&long_jack, "tool123456"); // 60 + 2 + 10
        assert_eq!(merged.chars().count(), 72);
        // Exercise the warn path (asserting no panic + name unchanged).
        let tool = json!({ "name": "tool123456" });
        let out = namespaced_tool(&long_jack, &tool);
        assert_eq!(out["name"], merged, "S4 must NOT truncate, only warn");
    }
}
