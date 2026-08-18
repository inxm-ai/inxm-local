//! Extract structured JSON from raw LLM output.
//!
//! LLMs routinely wrap JSON in markdown fences and add surrounding prose.
//! This module strips both before deserialisation.

use crate::compiler::diagnostics::safe_model_response;
use crate::error::CompilerError;

/// Try to extract a JSON object from a raw compile response.
///
/// Tries `\`\`\`json … \`\`\`` fence first, then falls back to the first `{`
/// through the last `}`.
pub fn extract_plan_json(raw: &str) -> Result<serde_json::Value, CompilerError> {
    extract_json(raw, "plan")
}

/// Try to extract a JSON object from a raw repair response.
pub fn extract_patch_json(raw: &str) -> Result<serde_json::Value, CompilerError> {
    extract_json(raw, "patch")
}

/// Try to extract a JSON object from a raw tool-synthesis response.
pub fn extract_tool_json(raw: &str) -> Result<serde_json::Value, CompilerError> {
    extract_json(raw, "tool")
}

/// Try to extract a JSON object from a raw intent-assessment response.
pub fn extract_assessment_json(raw: &str) -> Result<serde_json::Value, CompilerError> {
    extract_json(raw, "assessment")
}

/// Try to extract a JSON object from a raw solution-design response.
pub fn extract_design_json(raw: &str) -> Result<serde_json::Value, CompilerError> {
    extract_json(raw, "design")
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

fn extract_json(raw: &str, kind: &str) -> Result<serde_json::Value, CompilerError> {
    // 1. Prefer an explicit ```json … ``` fence.
    // Embedded prompts and scripts can themselves contain Markdown fences.
    // If the first apparent closing fence truncates the JSON, retry against
    // the complete outer object before reporting the model response invalid.
    if let Some(s) = from_fence(raw) {
        match to_value(s, kind, raw) {
            Ok(value) => return Ok(value),
            Err(fence_error) => {
                if let Some(s) = from_braces(raw) {
                    return to_value(s, kind, raw);
                }
                return Err(fence_error);
            }
        }
    }
    // 2. Fall back: grab from the first `{` to the last `}`.
    if let Some(s) = from_braces(raw) {
        return to_value(s, kind, raw);
    }
    Err(CompilerError::InvalidResponse {
        backend: "extractor".to_owned(),
        message: format!("no JSON object found in {kind} response"),
        raw: safe_model_response(raw),
    })
}

fn from_fence(raw: &str) -> Option<&str> {
    const OPEN: &str = "```json";
    const CLOSE: &str = "```";

    let start = raw.find(OPEN)?;
    let after_open = start + OPEN.len();

    // Skip the single newline immediately after the opening fence marker.
    let body_start = if raw[after_open..].starts_with("\r\n") {
        after_open + 2
    } else if raw[after_open..].starts_with('\n') {
        after_open + 1
    } else {
        after_open
    };

    let end = raw[body_start..].find(CLOSE)?;
    Some(raw[body_start..body_start + end].trim())
}

fn from_braces(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end < start {
        return None;
    }
    Some(&raw[start..=end])
}

fn to_value(s: &str, kind: &str, raw: &str) -> Result<serde_json::Value, CompilerError> {
    serde_json::from_str(s).map_err(|e| CompilerError::InvalidResponse {
        backend: "extractor".to_owned(),
        message: format!("JSON parse error ({kind}): {e}"),
        raw: safe_model_response(raw),
    })
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_from_json_fence() {
        let raw = "Here is the plan:\n```json\n{\"name\": \"test\", \"steps\": []}\n```\nTrailing.";
        let val = extract_plan_json(raw).unwrap();
        assert_eq!(val["name"], "test");
    }

    #[test]
    fn extracts_from_bare_braces_when_no_fence() {
        let raw = r#"Some text before {"key": "value"} some text after"#;
        let val = extract_plan_json(raw).unwrap();
        assert_eq!(val["key"], "value");
    }

    #[test]
    fn prefers_fence_over_bare_braces() {
        let raw = "outer { not_this: 1 }\n```json\n{\"correct\": true}\n```";
        let val = extract_plan_json(raw).unwrap();
        assert_eq!(val["correct"], true);
    }

    #[test]
    fn returns_error_on_no_json() {
        let raw = "No JSON here at all.";
        assert!(extract_plan_json(raw).is_err());
    }

    #[test]
    fn returns_error_on_invalid_json_in_fence() {
        let raw = "```json\n{ invalid json }\n```";
        assert!(extract_plan_json(raw).is_err());
    }

    #[test]
    fn extracts_patch_json() {
        let raw = "```json\n{\"operation\": {\"op\": \"update_step_config\"}, \"rationale\": \"fix it\"}\n```";
        let val = extract_patch_json(raw).unwrap();
        assert_eq!(val["rationale"], "fix it");
    }

    #[test]
    fn extracts_tool_json() {
        let raw = "```json\n{\"name\": \"curl\", \"allowlisted\": false}\n```";
        let val = extract_tool_json(raw).unwrap();
        assert_eq!(val["name"], "curl");
    }

    #[test]
    fn handles_crlf_after_fence_marker() {
        let raw = "```json\r\n{\"ok\": true}\r\n```";
        let val = extract_plan_json(raw).unwrap();
        assert_eq!(val["ok"], true);
    }

    #[test]
    fn extracts_assessment_json_from_fence() {
        let raw = "Here you go:\n```json\n{\"confidence\": 0.9, \"needs_clarification\": false, \"question\": null, \"spec\": {\"desired_outcome\": \"BTC price on disk\", \"acceptance_criteria\": [\"file exists\"]}}\n```";
        let val = extract_assessment_json(raw).unwrap();
        assert_eq!(val["confidence"], 0.9);
        assert_eq!(val["needs_clarification"], false);
        assert_eq!(val["spec"]["desired_outcome"], "BTC price on disk");
    }

    #[test]
    fn extracts_assessment_json_from_bare_braces() {
        let raw = r#"{"confidence": 0.4, "needs_clarification": true, "question": "Which file format?", "spec": {"desired_outcome": "x", "acceptance_criteria": []}}"#;
        let val = extract_assessment_json(raw).unwrap();
        assert_eq!(val["question"], "Which file format?");
    }

    #[test]
    fn assessment_extraction_errors_carry_kind() {
        let err = extract_assessment_json("no json at all").unwrap_err();
        assert!(err.to_string().contains("assessment"));
    }

    #[test]
    fn extracts_design_json_from_fence() {
        let raw = "```json\n{\"title\": \"BTC price logger\", \"summary\": \"Fetch and store.\", \"recommended_tools\": [{\"name\": \"http_get\", \"reason\": \"fetch the price\"}], \"execution_outline\": [{\"name\": \"Fetch\", \"step_kind\": \"tool_call\", \"description\": \"GET the price\"}]}\n```";
        let val = extract_design_json(raw).unwrap();
        assert_eq!(val["title"], "BTC price logger");
        assert_eq!(val["recommended_tools"][0]["name"], "http_get");
        assert_eq!(val["execution_outline"][0]["step_kind"], "tool_call");
    }

    #[test]
    fn extracts_design_json_from_bare_braces() {
        let raw = r#"Design: {"title": "t", "summary": "s", "recommended_tools": [], "execution_outline": []} done"#;
        let val = extract_design_json(raw).unwrap();
        assert_eq!(val["title"], "t");
    }

    #[test]
    fn handles_nested_braces_in_bare_extraction() {
        let raw = r#"prefix {"outer": {"inner": 42}} suffix"#;
        let val = extract_plan_json(raw).unwrap();
        assert_eq!(val["outer"]["inner"], 42);
    }

    #[test]
    fn extracts_complete_object_when_a_json_string_contains_a_markdown_fence() {
        let raw = r#"```json
{"name":"research","steps":[{"prompt":"Return this shape: ```json {\"summary\":\"string\"} ```"}]}
```"#;

        let val = extract_plan_json(raw).unwrap();

        assert_eq!(val["name"], "research");
        assert!(
            val["steps"][0]["prompt"]
                .as_str()
                .unwrap()
                .contains("```json")
        );
    }
}
