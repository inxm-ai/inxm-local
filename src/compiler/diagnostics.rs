//! Safe projections for data crossing the compiler/LLM boundary.
//!
//! Runtime failure artifacts can be both sensitive and arbitrarily large. This
//! module applies one compiler-owned policy before any such value is rendered
//! into a prompt or retained in an error.

use indexmap::IndexMap;

use crate::compiler::backend::CompileRunHistoryEntry;

const APPROX_BYTES_PER_TOKEN: usize = 4;
const DIAGNOSTIC_FIELD_MAX_BYTES: usize = 4_096;
const DIAGNOSTIC_TOTAL_MAX_TOKENS: usize = 4_096;
const DIAGNOSTIC_TOTAL_MAX_BYTES: usize = DIAGNOSTIC_TOTAL_MAX_TOKENS * APPROX_BYTES_PER_TOKEN;
const MODEL_RESPONSE_MAX_BYTES: usize = 2_048;
const REDACTED_VALUE: &str = "[REDACTED]";
const REDACTED_LINE: &str = "[REDACTED SENSITIVE LINE]";

const SENSITIVE_KEY_PARTS: &[&str] = &[
    "access_key",
    "api_key",
    "authorization",
    "client_secret",
    "cookie",
    "credential",
    "password",
    "private_key",
    "secret",
    "session",
    "token",
];

/// A bounded, redacted view of repair-time runtime diagnostics.
#[derive(Debug, Clone)]
pub(crate) struct RepairDiagnosticProjection {
    pub error_message: String,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub runtime_inputs: Option<String>,
    pub dependency_outputs: IndexMap<String, String>,
}

impl RepairDiagnosticProjection {
    pub fn new(
        error_message: &str,
        stdout: Option<&str>,
        stderr: Option<&str>,
        runtime_inputs: &serde_json::Value,
        dependency_outputs: &IndexMap<String, serde_json::Value>,
    ) -> Self {
        let mut budget = DiagnosticBudget::default();
        let error_message = budget.project_text("error_message", error_message);
        let stdout = stdout
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| budget.project_optional_text("stdout", value));
        let stderr = stderr
            .filter(|value| !value.trim().is_empty())
            .and_then(|value| budget.project_optional_text("stderr", value));
        let runtime_inputs = (!runtime_inputs.is_null())
            .then(|| budget.project_optional_json("runtime_inputs", runtime_inputs))
            .flatten();
        let dependency_outputs = dependency_outputs
            .iter()
            .filter_map(|(step_id, value)| {
                let field_name = format!("dependency_outputs.{step_id}");
                budget
                    .project_optional_json(&field_name, value)
                    .map(|projection| (step_id.clone(), projection))
            })
            .collect();

        Self {
            error_message,
            stdout,
            stderr,
            runtime_inputs,
            dependency_outputs,
        }
    }
}

/// A bounded, redacted projection of recent execution history supplied to a
/// plan edit. This deliberately reuses the same policy and total budget as
/// repair diagnostics so runtime evidence has one safety boundary.
#[derive(Debug, Clone)]
pub(crate) struct RunHistoryDiagnosticProjection {
    pub runs: Vec<String>,
}

impl RunHistoryDiagnosticProjection {
    pub fn new(history: &[CompileRunHistoryEntry]) -> Self {
        let mut budget = DiagnosticBudget::default();
        let runs = history
            .iter()
            .enumerate()
            .filter_map(|(index, run)| {
                let value = serde_json::to_value(run).ok()?;
                budget.project_optional_json(&format!("run_history[{index}]"), &value)
            })
            .collect();
        Self { runs }
    }
}

/// Replace a raw model response with a bounded, redacted, correlatable excerpt.
pub(crate) fn safe_model_response(raw: &str) -> String {
    let redacted = redact_sensitive_text(raw);
    render_projection_with_hash(
        "model_response",
        &redacted,
        raw.len(),
        MODEL_RESPONSE_MAX_BYTES,
        stable_hash(raw.as_bytes()),
    )
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace(['-', ' '], "_");
    SENSITIVE_KEY_PARTS
        .iter()
        .any(|part| normalized == *part || normalized.ends_with(&format!("_{part}")))
}

fn redact_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let projected = if is_sensitive_key(key) {
                        serde_json::Value::String(REDACTED_VALUE.to_owned())
                    } else {
                        redact_json(value)
                    };
                    (key.clone(), projected)
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().map(redact_json).collect())
        }
        serde_json::Value::String(value) => serde_json::Value::String(redact_sensitive_text(value)),
        _ => value.clone(),
    }
}

fn redact_sensitive_text(text: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
        return serde_json::to_string_pretty(&redact_json(&value))
            .unwrap_or_else(|_| REDACTED_VALUE.to_owned());
    }

    text.lines()
        .map(|line| {
            let normalized = line.to_ascii_lowercase().replace(['-', ' '], "_");
            if SENSITIVE_KEY_PARTS.iter().any(|part| {
                normalized.contains(&format!("{part}="))
                    || normalized.contains(&format!("{part}:"))
                    || normalized.contains(&format!("\"{part}\""))
            }) {
                REDACTED_LINE
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn value_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[derive(Debug)]
struct DiagnosticBudget {
    remaining_bytes: usize,
}

impl Default for DiagnosticBudget {
    fn default() -> Self {
        Self {
            remaining_bytes: DIAGNOSTIC_TOTAL_MAX_BYTES,
        }
    }
}

impl DiagnosticBudget {
    fn project_text(&mut self, name: &str, value: &str) -> String {
        self.project_optional_text(name, value).unwrap_or_else(|| {
            format!("[diagnostic field={name} omitted=true reason=total_budget_exhausted]")
        })
    }

    fn project_optional_text(&mut self, name: &str, value: &str) -> Option<String> {
        if self.remaining_bytes == 0 {
            return None;
        }
        let redacted = redact_sensitive_text(value);
        self.project_pre_redacted(name, &redacted, value.len())
    }

    fn project_pre_redacted(
        &mut self,
        name: &str,
        value: &str,
        original_bytes: usize,
    ) -> Option<String> {
        if self.remaining_bytes == 0 {
            return None;
        }
        let field_budget = DIAGNOSTIC_FIELD_MAX_BYTES.min(self.remaining_bytes);
        let projection = render_projection(name, value, original_bytes, field_budget);
        self.remaining_bytes = self.remaining_bytes.saturating_sub(projection.len());
        Some(projection)
    }

    fn project_optional_json(&mut self, name: &str, value: &serde_json::Value) -> Option<String> {
        if self.remaining_bytes == 0 {
            return None;
        }
        let redacted = redact_json(value);
        let serialized =
            serde_json::to_string_pretty(&redacted).unwrap_or_else(|_| "null".to_owned());
        let named = format!("kind={}\n{serialized}", value_kind(value));
        self.project_pre_redacted(name, &named, serialized.len())
    }
}

fn render_projection(name: &str, value: &str, original_bytes: usize, max_bytes: usize) -> String {
    render_projection_with_hash(
        name,
        value,
        original_bytes,
        max_bytes,
        stable_hash(value.as_bytes()),
    )
}

fn render_projection_with_hash(
    name: &str,
    value: &str,
    original_bytes: usize,
    max_bytes: usize,
    hash: u64,
) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let header_with_truncation = format!(
        "[diagnostic field={name} original_bytes={original_bytes} approx_tokens={} hash=fnv1a64:{hash:016x} truncated=true]\n",
        original_bytes.div_ceil(APPROX_BYTES_PER_TOKEN)
    );
    if header_with_truncation.len() >= max_bytes {
        return truncate_utf8(&header_with_truncation, max_bytes).to_owned();
    }
    let initial_excerpt = truncate_utf8(
        value,
        max_bytes.saturating_sub(header_with_truncation.len()),
    );
    let truncated = initial_excerpt.len() < value.len();
    let header = format!(
        "[diagnostic field={name} original_bytes={original_bytes} approx_tokens={} hash=fnv1a64:{hash:016x} truncated={truncated}]\n",
        original_bytes.div_ceil(APPROX_BYTES_PER_TOKEN)
    );
    if header.len() >= max_bytes {
        return truncate_utf8(&header, max_bytes).to_owned();
    }
    let excerpt = truncate_utf8(value, max_bytes - header.len());
    format!("{header}{excerpt}")
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn stable_hash(bytes: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    bytes.iter().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursively_redacts_sensitive_json_keys() {
        let value = serde_json::json!({
            "user": {
                "api_key": "sk-live",
                "nested": [{ "client_secret": "hidden", "safe": "visible" }]
            }
        });
        let mut budget = DiagnosticBudget::default();

        let projection = budget
            .project_optional_json("runtime_inputs", &value)
            .unwrap();

        assert!(!projection.contains("sk-live"));
        assert!(!projection.contains("hidden"));
        assert!(projection.contains(REDACTED_VALUE));
        assert!(projection.contains("visible"));
    }

    #[test]
    fn truncates_each_field_and_the_total_projection() {
        let large = "x".repeat(DIAGNOSTIC_FIELD_MAX_BYTES * 4);
        let dependencies = (0..10)
            .map(|index| {
                (
                    format!("step_{index}"),
                    serde_json::json!({ "payload": large }),
                )
            })
            .collect();

        let projection = RepairDiagnosticProjection::new(
            &large,
            Some(&large),
            Some(&large),
            &serde_json::json!({ "payload": large }),
            &dependencies,
        );
        let total = projection.error_message.len()
            + projection.stdout.as_ref().map_or(0, String::len)
            + projection.stderr.as_ref().map_or(0, String::len)
            + projection.runtime_inputs.as_ref().map_or(0, String::len)
            + projection
                .dependency_outputs
                .values()
                .map(String::len)
                .sum::<usize>();

        assert!(projection.error_message.contains("truncated=true"));
        assert!(total <= DIAGNOSTIC_TOTAL_MAX_BYTES);
    }

    #[test]
    fn model_response_projection_is_bounded_redacted_and_correlatable() {
        let raw = format!(
            "api_key=sk-secret\n{}",
            "z".repeat(MODEL_RESPONSE_MAX_BYTES * 2)
        );

        let safe = safe_model_response(&raw);

        assert!(!safe.contains("sk-secret"));
        assert!(safe.contains("hash=fnv1a64:"));
        assert!(safe.contains("truncated=true"));
        assert!(safe.len() <= MODEL_RESPONSE_MAX_BYTES);
    }
}
