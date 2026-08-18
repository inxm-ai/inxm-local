//! Tool execution entry point.
//!
//! Provides [`execute_tool`], which dispatches to the appropriate adapter
//! based on the tool's [`ToolConfig`] variant.

pub mod adapters;
pub mod catalog;
pub mod oauth;
mod schema;

use crate::error::ToolError;
use crate::tools::catalog::{McpTransport, ToolConfig, ToolEntry};
use crate::tools::schema::validate_instance;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::sync::Once;
use std::time::Instant;
use tracing::Instrument;

/// rmcp vendors its own reqwest (a different major version from the one this
/// crate depends on directly, so Cargo feature unification does not apply)
/// built with the `rustls-tls-no-provider` feature — no bundled crypto
/// provider crate, to avoid a second one (aws-lc-rs) alongside the `ring`
/// already used elsewhere in the dependency tree. reqwest 0.13 panics on
/// client construction if no provider is installed process-wide, so every
/// entry point that builds an rmcp HTTP(S) client (outbound OAuth and
/// Streamable HTTP) must call this first. `Once` makes it safe to call
/// repeatedly and from any process entry point (desktop, headless,
/// self-test, or tests) without depending on `main`.
pub(crate) fn ensure_tls_crypto_provider_installed() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

// ─── Output type ─────────────────────────────────────────────────────────────

/// The normalised output from any tool adapter.
///
/// The derived `Default` is a successful empty output: empty streams,
/// `exit_code` 0, and `data` of `serde_json::Value::Null`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    /// Parsed JSON if the tool emitted valid JSON on stdout, otherwise `null`.
    pub data: serde_json::Value,
}

// ─── Dispatch ────────────────────────────────────────────────────────────────

/// Execute a tool using its configured adapter.
///
/// `effective_step_timeout_secs` is the *caller's* deadline for this call —
/// normally the resolved `PlanStep.timeout_secs` (or plan-level default) that
/// the executor will enforce around the whole step regardless of what this
/// function does. It is combined with the tool's own `timeout_secs` (the
/// shorter of the two wins, same precedence `code_call` already uses for its
/// script timeout vs. the step timeout) so the adapter's *internal* timeout
/// path — which kills the child process/session and still returns whatever
/// output it managed to capture — has a chance to fire before the outer,
/// adapter-agnostic step timeout does. The outer timeout only drops the
/// in-flight future; it cannot kill a subprocess (or a WSL-hosted session
/// inside it) or recover any diagnostic output, so relying on it alone turns
/// every hang into an opaque timeout with no stdout/stderr to diagnose.
pub async fn execute_tool(
    entry: &ToolEntry,
    arguments: &IndexMap<String, serde_json::Value>,
    effective_step_timeout_secs: Option<u64>,
) -> Result<ToolOutput, ToolError> {
    let arguments_value =
        serde_json::to_value(arguments).map_err(|error| ToolError::Execution {
            tool: entry.name.clone(),
            message: format!("failed to serialise tool arguments: {error}"),
        })?;
    validate_instance(&entry.input_schema, &arguments_value).map_err(|message| {
        ToolError::Execution {
            tool: entry.name.clone(),
            message: format!("tool input does not match input_schema: {message}"),
        }
    })?;

    let timeout_secs = match (entry.timeout_secs, effective_step_timeout_secs) {
        (Some(tool_timeout), Some(step_timeout)) => Some(tool_timeout.min(step_timeout)),
        (Some(tool_timeout), None) => Some(tool_timeout),
        (None, Some(step_timeout)) => Some(step_timeout),
        (None, None) => None,
    };

    let span = execution_span(entry);
    async {
        let started = Instant::now();
        let result = match &entry.config {
            ToolConfig::Subprocess(config) => {
                adapters::subprocess::run(config, arguments, timeout_secs).await
            }
            ToolConfig::Http(config) => adapters::http::run(config, arguments, timeout_secs).await,
            ToolConfig::Mcp(config) => adapters::mcp::run(config, arguments, timeout_secs).await,
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        tracing::Span::current().record("tool.duration_ms", elapsed_ms);

        match result {
            Ok(output) => {
                if let Err(error) = validate_tool_output(entry, &output) {
                    tracing::Span::current().record("tool.outcome", "error");
                    tracing::Span::current()
                        .record("tool.exit_classification", "output_schema_error");
                    return Err(error);
                }
                tracing::Span::current().record("tool.outcome", "success");
                tracing::Span::current().record(
                    "tool.exit_classification",
                    if output.exit_code == 0 {
                        "success"
                    } else {
                        "nonzero"
                    },
                );
                Ok(output)
            }
            Err(error) => {
                tracing::Span::current().record("tool.outcome", "error");
                tracing::Span::current().record(
                    "tool.exit_classification",
                    match error {
                        ToolError::Timeout { .. } => "timeout",
                        _ => "execution_error",
                    },
                );
                Err(error)
            }
        }
    }
    .instrument(span)
    .await
}

fn execution_span(entry: &ToolEntry) -> tracing::Span {
    let adapter_kind = adapter_kind(&entry.config);
    let identity = sanitized_identity(&entry.config);
    let timeout_secs = entry
        .timeout_secs
        .map(|value| value.to_string())
        .unwrap_or_else(|| "default".to_owned());
    tracing::info_span!(
        "tools.execute",
        tool.name = %entry.name,
        tool.adapter.kind = adapter_kind,
        tool.identity = %identity,
        tool.timeout_secs = %timeout_secs,
        tool.attempt_count = tracing::field::Empty,
        tool.retry_count = tracing::field::Empty,
        tool.duration_ms = tracing::field::Empty,
        tool.outcome = tracing::field::Empty,
        tool.exit_classification = tracing::field::Empty,
        tool.output_limit_violation = false,
    )
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn validate_tool_output(entry: &ToolEntry, output: &ToolOutput) -> Result<(), ToolError> {
    let schema_type = entry
        .output_schema
        .get("type")
        .and_then(serde_json::Value::as_str);
    let value = match schema_type {
        Some("object") if output.data.is_object() => output.data.clone(),
        Some("object") => serde_json::json!({
            "stdout": output.stdout,
            "stderr": output.stderr,
            "exit_code": output.exit_code,
        }),
        Some("string") => serde_json::Value::String(output.stdout.clone()),
        _ if !output.data.is_null() => output.data.clone(),
        _ => serde_json::Value::String(output.stdout.clone()),
    };
    validate_instance(&entry.output_schema, &value).map_err(|message| ToolError::Execution {
        tool: entry.name.clone(),
        message: format!("tool output does not match output_schema: {message}"),
    })
}

fn adapter_kind(config: &ToolConfig) -> &'static str {
    match config {
        ToolConfig::Subprocess(_) => "subprocess",
        ToolConfig::Http(_) => "http",
        ToolConfig::Mcp(_) => "mcp",
    }
}

fn sanitized_identity(config: &ToolConfig) -> String {
    match config {
        ToolConfig::Subprocess(config) => command_identity(&config.command),
        ToolConfig::Mcp(config) => {
            let transport = match &config.transport {
                McpTransport::Stdio { server_command, .. } => command_identity(server_command),
                McpTransport::StreamableHttp { endpoint, .. } => reqwest::Url::parse(endpoint)
                    .ok()
                    .and_then(|url| url.host_str().map(str::to_owned))
                    .unwrap_or_else(|| "remote-mcp".to_owned()),
            };
            format!("{transport}:{}", config.tool_name)
        }
        ToolConfig::Http(config) => {
            let target = if config.base_url.is_empty() {
                config.path_template.as_str()
            } else {
                config.base_url.as_str()
            };
            reqwest::Url::parse(target)
                .ok()
                .and_then(|url| {
                    url.host_str().map(|host| {
                        let port = url
                            .port()
                            .map(|value| format!(":{value}"))
                            .unwrap_or_default();
                        format!("{}://{host}{port}", url.scheme())
                    })
                })
                .unwrap_or_else(|| "dynamic-http-target".to_owned())
        }
    }
}

fn command_identity(command: &str) -> String {
    std::path::Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("configured-command")
        .to_owned()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::catalog::{SubprocessConfig, ToolConfig, ToolEntry};

    fn make_subprocess_entry() -> ToolEntry {
        ToolEntry {
            name: "test-tool".to_owned(),
            description: "a test tool".to_owned(),
            config: ToolConfig::Subprocess(SubprocessConfig {
                command: "echo".to_owned(),
                args: vec![],
                env: IndexMap::new(),
                working_dir: None,
            }),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: serde_json::json!({"type": "object"}),
            allowlisted: true,
            timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn resolved_inputs_are_validated_before_execution() {
        let mut entry = make_subprocess_entry();
        entry.input_schema = serde_json::json!({
            "type": "object",
            "properties": { "count": { "type": "integer" } },
            "required": ["count"]
        });
        let arguments: IndexMap<String, serde_json::Value> =
            [("count".to_owned(), serde_json::json!("not-an-integer"))]
                .into_iter()
                .collect();

        let error = execute_tool(&entry, &arguments, None).await.unwrap_err();
        assert!(matches!(error, ToolError::Execution { .. }));
        assert!(error.to_string().contains("$.count: expected integer"));
    }

    #[test]
    fn successful_output_is_validated_against_output_schema() {
        let mut entry = make_subprocess_entry();
        entry.output_schema = serde_json::json!({"type": "boolean"});
        let output = ToolOutput {
            stdout: "42".to_owned(),
            data: serde_json::json!(42),
            ..ToolOutput::default()
        };

        let error = validate_tool_output(&entry, &output).unwrap_err();
        assert!(matches!(error, ToolError::Execution { .. }));
        assert!(error.to_string().contains("expected boolean"));
    }
}
