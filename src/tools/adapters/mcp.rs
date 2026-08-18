//! MCP stdio adapter — speaks JSON-RPC 2.0 over a child process's stdio.
//!
//! Protocol sketch:
//!
//! 1. Spawn `config.server_command config.server_args…` with piped stdio.
//! 2. Perform the MCP lifecycle handshake: send `initialize`, read its
//!    response, then send the `notifications/initialized` notification.
//!    Compliant servers (including the reference `server-filesystem`)
//!    refuse or ignore other requests until this has happened.
//! 3. Write a single `tools/call` JSON-RPC request on stdin (newline-delimited).
//! 4. Read the first complete JSON line from stdout as the response.
//! 5. Extract `result.content[0].text` (MCP standard text content format).
//!
//! stdin is only closed after the whole exchange completes, and stderr is
//! drained concurrently so it can be attached to any error we report.
//! The MCP server is given a bounded grace period to exit after EOF; timeout
//! paths explicitly kill and reap the isolated child process group.

use crate::error::ToolError;
use crate::tools::ToolOutput;
use crate::tools::adapters::process::{ProcessGroupGuard, isolate_process_group, kill_and_reap};
use crate::tools::catalog::{McpConfig, McpDiscoveredTool, McpTransport};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::time::{Instant, timeout, timeout_at};

// ─── Protocol constants ───────────────────────────────────────────────────────

const JSONRPC_VERSION: &str = "2.0";
/// MCP protocol revision this client implements.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const METHOD_INITIALIZE: &str = "initialize";
const NOTIFICATION_INITIALIZED: &str = "notifications/initialized";
const METHOD_TOOLS_CALL: &str = "tools/call";
const METHOD_TOOLS_LIST: &str = "tools/list";
/// Label used in place of a tool name for errors raised while discovering a
/// server's tools rather than calling one of them.
const LIST_TOOLS_LABEL: &str = "tools/list";
const LIST_TOOLS_REQUEST_ID: u64 = 1;
/// Whole-session deadline for a discovery ("tools/list") call.
const DEFAULT_LIST_TOOLS_TIMEOUT_SECS: u64 = 30;
/// MCP content block type whose `text` field carries the tool result.
const CONTENT_TYPE_TEXT: &str = "text";
/// Request ids for the two requests we send in a session; any distinct
/// values work since we read responses in order.
const INITIALIZE_REQUEST_ID: u64 = 0;
const TOOL_CALL_REQUEST_ID: u64 = 1;
/// Whole-session deadline when the catalog does not specify one.
const DEFAULT_MCP_TIMEOUT_SECS: u64 = 60;
/// Time allowed for a server to exit cleanly after stdin is closed.
const MCP_SHUTDOWN_GRACE_SECS: u64 = 1;
/// Maximum size of one newline-delimited MCP JSON-RPC message.
const MAX_MCP_STDOUT_LINE_BYTES: usize = 1_048_576;
/// Maximum stderr retained from an MCP server.
const MAX_MCP_STDERR_BYTES: usize = 262_144;
/// Maximum text or serialized structured content returned by an MCP tool.
const MAX_MCP_CONTENT_BYTES: usize = 524_288;
const OUTPUT_LIMIT_MARKER: &str = "[truncated: MCP output limit exceeded]";

// ─── JSON-RPC wire types ──────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: ToolCallParams<'a>,
}

#[derive(Debug, Serialize)]
struct ToolCallParams<'a> {
    name: &'a str,
    arguments: &'a IndexMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcEnvelope {
    jsonrpc: String,
    id: Option<serde_json::Value>,
    method: Option<String>,
    result: Option<serde_json::Value>,
    error: Option<McpRpcError>,
}

#[derive(Debug, Deserialize)]
struct McpCallResult {
    #[serde(default)]
    content: Vec<McpContent>,
    #[serde(rename = "structuredContent")]
    structured_content: Option<serde_json::Value>,
    #[serde(rename = "isError", default)]
    is_error: bool,
}

#[derive(Debug, Deserialize)]
struct McpContent {
    #[serde(rename = "type")]
    content_type: Option<String>,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpRpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    protocol_version: String,
    capabilities: serde_json::Map<String, serde_json::Value>,
    #[serde(rename = "serverInfo")]
    server_info: McpImplementation,
}

#[derive(Debug, Deserialize)]
struct McpImplementation {
    name: String,
    version: String,
}

#[derive(Debug)]
enum EnvelopeDisposition {
    Notification,
    Response {
        result: Option<serde_json::Value>,
        error: Option<McpRpcError>,
    },
}

// ─── Public entry point ───────────────────────────────────────────────────────

pub async fn run(
    config: &McpConfig,
    arguments: &IndexMap<String, serde_json::Value>,
    timeout_secs: Option<u64>,
) -> Result<ToolOutput, ToolError> {
    let effective_timeout_secs = timeout_secs.unwrap_or(DEFAULT_MCP_TIMEOUT_SECS);
    match &config.transport {
        McpTransport::Stdio { .. } => call_tool(config, arguments, effective_timeout_secs).await,
        McpTransport::StreamableHttp { endpoint, auth } => {
            super::mcp_http::run(
                endpoint,
                auth,
                &config.tool_name,
                arguments,
                effective_timeout_secs,
            )
            .await
        }
    }
}

/// Connect to a server and enumerate the tools it currently advertises
/// (`tools/list`), so the caller can offer them for bulk import into the
/// catalog. Independent of any already-registered catalog entry.
pub async fn list_tools(
    transport: &McpTransport,
    timeout_secs: Option<u64>,
) -> Result<Vec<McpDiscoveredTool>, ToolError> {
    let effective_timeout_secs = timeout_secs.unwrap_or(DEFAULT_LIST_TOOLS_TIMEOUT_SECS);
    match transport {
        McpTransport::Stdio {
            server_command,
            server_args,
            server_env,
        } => {
            run_stdio_session(
                server_command,
                server_args,
                server_env,
                effective_timeout_secs,
                LIST_TOOLS_LABEL,
                |stdin, stdout| Box::pin(list_session(stdin, stdout)),
            )
            .await
        }
        McpTransport::StreamableHttp { endpoint, auth } => {
            super::mcp_http::list_tools(endpoint, auth, effective_timeout_secs).await
        }
    }
}

// ─── Internals ────────────────────────────────────────────────────────────────

async fn call_tool(
    config: &McpConfig,
    arguments: &IndexMap<String, serde_json::Value>,
    timeout_secs: u64,
) -> Result<ToolOutput, ToolError> {
    let McpTransport::Stdio {
        server_command,
        server_args,
        server_env,
    } = &config.transport
    else {
        return Err(ToolError::Execution {
            tool: config.tool_name.clone(),
            message: "internal MCP transport dispatch mismatch".to_owned(),
        });
    };
    // Cloned so the boxed session future is self-contained (owns everything
    // but the stdin/stdout it borrows for the HRTB lifetime), avoiding a tie
    // between that lifetime and this function's `config`/`arguments` borrows.
    let tool_label = config.tool_name.clone();
    let owned_config = config.clone();
    let owned_arguments = arguments.clone();
    run_stdio_session(
        server_command,
        server_args,
        server_env,
        timeout_secs,
        &tool_label,
        move |stdin, stdout| {
            Box::pin(
                async move { run_session(&owned_config, &owned_arguments, stdin, stdout).await },
            )
        },
    )
    .await
}

/// Shared child-process lifecycle for stdio MCP sessions: spawn, drain
/// stderr concurrently, run `session` under `timeout_secs`, then shut the
/// server down (EOF, bounded grace period, force-kill if needed) and attach
/// any captured stderr to a failing outcome. `tool_label` is used only for
/// error messages — it names the operation, not a catalog tool.
async fn run_stdio_session<T>(
    server_command: &str,
    server_args: &[String],
    server_env: &IndexMap<String, String>,
    timeout_secs: u64,
    tool_label: &str,
    session: impl for<'a> FnOnce(
        &'a mut ChildStdin,
        &'a mut BufReader<ChildStdout>,
    ) -> Pin<Box<dyn Future<Output = Result<T, ToolError>> + Send + 'a>>,
) -> Result<T, ToolError> {
    // PATH/PATHEXT-aware resolution: finds `npx.cmd`/`uvx.exe` on Windows.
    let mut command = Command::new(crate::hostenv::resolve_program(server_command));
    command
        .args(server_args)
        .envs(server_env.iter())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    isolate_process_group(&mut command);

    let mut child = command.spawn().map_err(|e| ToolError::Execution {
        tool: tool_label.to_owned(),
        message: format!(
            "failed to start MCP server '{}': {e} — is it installed and on PATH?",
            server_command
        ),
    })?;
    let mut process_group = ProcessGroupGuard::for_child(&child);

    let mut stdin = child.stdin.take().ok_or_else(|| ToolError::Execution {
        tool: tool_label.to_owned(),
        message: "failed to open child stdin".to_owned(),
    })?;

    let stdout = child.stdout.take().ok_or_else(|| ToolError::Execution {
        tool: tool_label.to_owned(),
        message: "failed to open child stdout".to_owned(),
    })?;

    let stderr = child.stderr.take().ok_or_else(|| ToolError::Execution {
        tool: tool_label.to_owned(),
        message: "failed to open child stderr".to_owned(),
    })?;

    // Drain stderr concurrently so a chatty server can't deadlock on a full
    // pipe, and so we can attach whatever it printed to any error we report.
    let mut stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).take((MAX_MCP_STDERR_BYTES + 1) as u64);
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        let overflow = bytes.len() > MAX_MCP_STDERR_BYTES;
        bytes.truncate(MAX_MCP_STDERR_BYTES);
        Ok::<_, std::io::Error>(BoundedText {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            overflow,
        })
    });

    let mut stdout_reader = BufReader::new(stdout);
    let outcome = match timeout(
        Duration::from_secs(timeout_secs),
        session(&mut stdin, &mut stdout_reader),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => {
            drop(stdin);
            drop(stdout_reader);
            kill_and_reap(&mut child, &mut process_group)
                .await
                .map_err(|error| ToolError::Execution {
                    tool: tool_label.to_owned(),
                    message: format!(
                        "timed out after {timeout_secs}s and failed to terminate MCP server: {error}"
                    ),
                })?;
            // Killing the server closes its stderr pipe, so this now picks
            // up whatever it had already printed instead of discarding it,
            // turning a bare timeout into a diagnosable failure.
            let stderr_text = stderr_task
                .await
                .ok()
                .and_then(Result::ok)
                .map(|bounded| bounded.text)
                .unwrap_or_default();
            return Err(ToolError::timeout_with_output(
                tool_label.to_owned(),
                timeout_secs,
                "",
                &stderr_text,
            ));
        }
    };

    // Close both protocol handles so the server sees EOF, then give it a
    // bounded grace period before forcing cleanup.
    drop(stdin);
    drop(stdout_reader);
    let shutdown_deadline = Instant::now() + Duration::from_secs(MCP_SHUTDOWN_GRACE_SECS);
    match timeout_at(shutdown_deadline, child.wait()).await {
        Ok(result) => {
            result.map_err(ToolError::Io)?;
        }
        Err(_) => {
            kill_and_reap(&mut child, &mut process_group)
                .await
                .map_err(|error| ToolError::Execution {
                    tool: tool_label.to_owned(),
                    message: format!(
                        "failed to terminate MCP server after shutdown grace: {error}"
                    ),
                })?;
        }
    }
    let stderr_output = match timeout_at(shutdown_deadline, &mut stderr_task).await {
        Ok(result) => {
            process_group.disarm();
            collect_stderr(result, tool_label)?
        }
        Err(_) => {
            kill_and_reap(&mut child, &mut process_group)
                .await
                .map_err(|error| ToolError::Execution {
                    tool: tool_label.to_owned(),
                    message: format!(
                        "failed to terminate MCP descendants after shutdown grace: {error}"
                    ),
                })?;
            collect_stderr(stderr_task.await, tool_label)?
        }
    };
    if stderr_output.overflow {
        tracing::Span::current().record("tool.output_limit_violation", true);
        return Err(ToolError::Execution {
            tool: tool_label.to_owned(),
            message: format!(
                "{OUTPUT_LIMIT_MARKER}; stderr maximum is {MAX_MCP_STDERR_BYTES} bytes"
            ),
        });
    }

    outcome.map_err(|err| attach_stderr(err, &stderr_output.text))
}

/// Runs the MCP lifecycle over an already-spawned child's stdio:
/// `initialize` → `notifications/initialized` → `tools/call`.
async fn run_session(
    config: &McpConfig,
    arguments: &IndexMap<String, serde_json::Value>,
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
) -> Result<ToolOutput, ToolError> {
    // 1. `initialize` — required before a compliant server will honour any
    //    other request.
    let init_request = serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": INITIALIZE_REQUEST_ID,
        "method": METHOD_INITIALIZE,
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": env!("CARGO_PKG_NAME"),
                "version": env!("CARGO_PKG_VERSION"),
            },
        }
    });
    write_json_line(stdin, &init_request, &config.tool_name).await?;

    let init_response = read_matching_response(
        stdout,
        &config.tool_name,
        "initialize",
        INITIALIZE_REQUEST_ID,
    )
    .await?;
    if let Some(err) = init_response.error {
        return Err(ToolError::Execution {
            tool: config.tool_name.clone(),
            message: format!("MCP initialize error (code {}): {}", err.code, err.message),
        });
    }
    let init_result = init_response.result.ok_or_else(|| ToolError::Execution {
        tool: config.tool_name.clone(),
        message: "MCP initialize response contained no result".to_owned(),
    })?;
    validate_initialize_result(&init_result).map_err(|message| ToolError::Execution {
        tool: config.tool_name.clone(),
        message,
    })?;

    // 2. `notifications/initialized` — a notification, so no response is sent.
    let initialized_notification = serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "method": NOTIFICATION_INITIALIZED,
        "params": {}
    });
    write_json_line(stdin, &initialized_notification, &config.tool_name).await?;

    // 3. `tools/call` — the actual request we care about.
    let call_request = JsonRpcRequest {
        jsonrpc: JSONRPC_VERSION,
        id: TOOL_CALL_REQUEST_ID,
        method: METHOD_TOOLS_CALL,
        params: ToolCallParams {
            name: &config.tool_name,
            arguments,
        },
    };
    write_json_line(stdin, &call_request, &config.tool_name).await?;

    let response = read_matching_response(
        stdout,
        &config.tool_name,
        "tools/call",
        TOOL_CALL_REQUEST_ID,
    )
    .await?;

    // Check for a protocol-level error.
    if let Some(rpc_err) = response.error {
        return Err(ToolError::Execution {
            tool: config.tool_name.clone(),
            message: format!("MCP error (code {}): {}", rpc_err.code, rpc_err.message),
        });
    }

    let result_value = response.result.ok_or_else(|| ToolError::Execution {
        tool: config.tool_name.clone(),
        message: "MCP response contained neither result nor error".to_owned(),
    })?;
    let result: McpCallResult =
        serde_json::from_value(result_value).map_err(|error| ToolError::Execution {
            tool: config.tool_name.clone(),
            message: format!("invalid MCP tools/call result: {error}"),
        })?;

    let text = primary_content(&result);
    if text.len() > MAX_MCP_CONTENT_BYTES {
        tracing::Span::current().record("tool.output_limit_violation", true);
        return Err(ToolError::Execution {
            tool: config.tool_name.clone(),
            message: format!(
                "{OUTPUT_LIMIT_MARKER}; content maximum is {MAX_MCP_CONTENT_BYTES} bytes"
            ),
        });
    }
    if result.is_error {
        let message = if text.is_empty() {
            "MCP tool reported an error without content".to_owned()
        } else {
            format!("MCP tool reported an error: {text}")
        };
        return Err(ToolError::Execution {
            tool: config.tool_name.clone(),
            message,
        });
    }
    let data = result
        .structured_content
        .unwrap_or_else(|| serde_json::from_str(&text).unwrap_or(serde_json::Value::Null));

    Ok(ToolOutput {
        stdout: text,
        stderr: String::new(),
        exit_code: 0,
        data,
    })
}

/// Runs the MCP lifecycle for discovery: `initialize` → `notifications/initialized`
/// → one or more `tools/list` requests, following `nextCursor` until the
/// server reports no further page.
async fn list_session(
    stdin: &mut ChildStdin,
    stdout: &mut BufReader<ChildStdout>,
) -> Result<Vec<McpDiscoveredTool>, ToolError> {
    let init_request = serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": INITIALIZE_REQUEST_ID,
        "method": METHOD_INITIALIZE,
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": env!("CARGO_PKG_NAME"),
                "version": env!("CARGO_PKG_VERSION"),
            },
        }
    });
    write_json_line(stdin, &init_request, LIST_TOOLS_LABEL).await?;

    let init_response = read_matching_response(
        stdout,
        LIST_TOOLS_LABEL,
        "initialize",
        INITIALIZE_REQUEST_ID,
    )
    .await?;
    if let Some(err) = init_response.error {
        return Err(ToolError::Execution {
            tool: LIST_TOOLS_LABEL.to_owned(),
            message: format!("MCP initialize error (code {}): {}", err.code, err.message),
        });
    }
    let init_result = init_response.result.ok_or_else(|| ToolError::Execution {
        tool: LIST_TOOLS_LABEL.to_owned(),
        message: "MCP initialize response contained no result".to_owned(),
    })?;
    validate_initialize_result(&init_result).map_err(|message| ToolError::Execution {
        tool: LIST_TOOLS_LABEL.to_owned(),
        message,
    })?;

    let initialized_notification = serde_json::json!({
        "jsonrpc": JSONRPC_VERSION,
        "method": NOTIFICATION_INITIALIZED,
        "params": {}
    });
    write_json_line(stdin, &initialized_notification, LIST_TOOLS_LABEL).await?;

    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut params = serde_json::Map::new();
        if let Some(cursor) = &cursor {
            params.insert(
                "cursor".to_owned(),
                serde_json::Value::String(cursor.clone()),
            );
        }
        let request = serde_json::json!({
            "jsonrpc": JSONRPC_VERSION,
            "id": LIST_TOOLS_REQUEST_ID,
            "method": METHOD_TOOLS_LIST,
            "params": params,
        });
        write_json_line(stdin, &request, LIST_TOOLS_LABEL).await?;

        let response = read_matching_response(
            stdout,
            LIST_TOOLS_LABEL,
            METHOD_TOOLS_LIST,
            LIST_TOOLS_REQUEST_ID,
        )
        .await?;
        if let Some(rpc_err) = response.error {
            return Err(ToolError::Execution {
                tool: LIST_TOOLS_LABEL.to_owned(),
                message: format!("MCP error (code {}): {}", rpc_err.code, rpc_err.message),
            });
        }
        let result_value = response.result.ok_or_else(|| ToolError::Execution {
            tool: LIST_TOOLS_LABEL.to_owned(),
            message: "MCP response contained neither result nor error".to_owned(),
        })?;
        let result: ListToolsWireResult =
            serde_json::from_value(result_value).map_err(|error| ToolError::Execution {
                tool: LIST_TOOLS_LABEL.to_owned(),
                message: format!("invalid MCP tools/list result: {error}"),
            })?;

        tools.extend(result.tools.into_iter().map(|tool| McpDiscoveredTool {
            name: tool.name,
            description: tool.description.unwrap_or_default(),
            input_schema: tool.input_schema,
        }));

        match result.next_cursor {
            Some(next) if !next.is_empty() => cursor = Some(next),
            _ => break,
        }
    }
    Ok(tools)
}

#[derive(Debug, Deserialize)]
struct ListToolsWireResult {
    #[serde(default)]
    tools: Vec<McpToolWire>,
    #[serde(rename = "nextCursor")]
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct McpToolWire {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "inputSchema", default = "default_input_schema")]
    input_schema: serde_json::Value,
}

fn default_input_schema() -> serde_json::Value {
    serde_json::json!({ "type": "object" })
}

/// Extracts the first text-type content block from a `tools/call` result,
/// or an empty string when the result carries no text content.
fn first_text_content(result: &McpCallResult) -> String {
    result
        .content
        .iter()
        .find(|c| c.content_type.as_deref() == Some(CONTENT_TYPE_TEXT))
        .and_then(|c| c.text.clone())
        .unwrap_or_default()
}

fn primary_content(result: &McpCallResult) -> String {
    let text = first_text_content(result);
    if !text.is_empty() {
        return text;
    }
    result
        .structured_content
        .as_ref()
        .and_then(|value| serde_json::to_string(value).ok())
        .unwrap_or_default()
}

struct MatchedResponse {
    result: Option<serde_json::Value>,
    error: Option<McpRpcError>,
}

async fn read_matching_response(
    stdout: &mut BufReader<ChildStdout>,
    tool: &str,
    context: &str,
    expected_id: u64,
) -> Result<MatchedResponse, ToolError> {
    loop {
        let raw_line = read_json_line(stdout, tool, context).await?;
        match classify_envelope(raw_line.trim(), expected_id).map_err(|message| {
            ToolError::Execution {
                tool: tool.to_owned(),
                message: format!("invalid JSON-RPC {context} response: {message}"),
            }
        })? {
            EnvelopeDisposition::Notification => continue,
            EnvelopeDisposition::Response { result, error } => {
                return Ok(MatchedResponse { result, error });
            }
        }
    }
}

fn classify_envelope(line: &str, expected_id: u64) -> Result<EnvelopeDisposition, String> {
    let envelope: JsonRpcEnvelope =
        serde_json::from_str(line).map_err(|error| error.to_string())?;
    if envelope.jsonrpc != JSONRPC_VERSION {
        return Err(format!(
            "expected jsonrpc '{JSONRPC_VERSION}', got '{}'",
            envelope.jsonrpc
        ));
    }

    if let Some(method) = envelope.method {
        if method.trim().is_empty() {
            return Err("notification method is empty".to_owned());
        }
        if envelope.id.is_none() && envelope.result.is_none() && envelope.error.is_none() {
            return Ok(EnvelopeDisposition::Notification);
        }
        return Err(format!("server request '{method}' is not a notification"));
    }

    let id = envelope
        .id
        .ok_or_else(|| "response is missing an id".to_owned())?;
    if id.as_u64() != Some(expected_id) {
        return Err(format!(
            "response id {id} does not match request id {expected_id}"
        ));
    }
    if envelope.result.is_some() == envelope.error.is_some() {
        return Err("response must contain exactly one of result or error".to_owned());
    }
    Ok(EnvelopeDisposition::Response {
        result: envelope.result,
        error: envelope.error,
    })
}

fn validate_initialize_result(value: &serde_json::Value) -> Result<(), String> {
    let result: InitializeResult = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid MCP initialize result: {error}"))?;
    if result.protocol_version.trim().is_empty()
        || result.server_info.name.trim().is_empty()
        || result.server_info.version.trim().is_empty()
    {
        return Err(
            "invalid MCP initialize result: protocol and server identity must be non-empty"
                .to_owned(),
        );
    }
    let _ = result.capabilities;
    Ok(())
}

/// Serialises `value` and writes it as a single newline-delimited JSON-RPC
/// message on the child's stdin.
async fn write_json_line<T: Serialize>(
    stdin: &mut ChildStdin,
    value: &T,
    tool: &str,
) -> Result<(), ToolError> {
    let json = serde_json::to_string(value).map_err(|e| ToolError::Execution {
        tool: tool.to_owned(),
        message: format!("failed to serialise JSON-RPC message: {e}"),
    })?;
    stdin
        .write_all(json.as_bytes())
        .await
        .map_err(ToolError::Io)?;
    stdin.write_all(b"\n").await.map_err(ToolError::Io)?;
    stdin.flush().await.map_err(ToolError::Io)
}

/// Reads a single line from the child's stdout, treating an immediate EOF
/// (empty line) as a distinct, more actionable error than a JSON parse failure.
async fn read_json_line(
    stdout: &mut BufReader<ChildStdout>,
    tool: &str,
    context: &str,
) -> Result<String, ToolError> {
    let mut bytes = Vec::new();
    stdout
        .take((MAX_MCP_STDOUT_LINE_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .await
        .map_err(ToolError::Io)?;
    if bytes.len() > MAX_MCP_STDOUT_LINE_BYTES {
        tracing::Span::current().record("tool.output_limit_violation", true);
        return Err(ToolError::Execution {
            tool: tool.to_owned(),
            message: format!(
                "{OUTPUT_LIMIT_MARKER}; stdout line maximum is {MAX_MCP_STDOUT_LINE_BYTES} bytes"
            ),
        });
    }
    let raw_line = String::from_utf8(bytes).map_err(|error| ToolError::Execution {
        tool: tool.to_owned(),
        message: format!("MCP server emitted non-UTF-8 {context} response: {error}"),
    })?;
    if raw_line.trim().is_empty() {
        return Err(ToolError::Execution {
            tool: tool.to_owned(),
            message: format!("MCP server closed its output before sending a {context} response"),
        });
    }
    Ok(raw_line)
}

struct BoundedText {
    text: String,
    overflow: bool,
}

fn collect_stderr(
    result: Result<std::io::Result<BoundedText>, tokio::task::JoinError>,
    tool: &str,
) -> Result<BoundedText, ToolError> {
    result
        .map_err(|error| ToolError::Execution {
            tool: tool.to_owned(),
            message: format!("failed to join MCP stderr reader: {error}"),
        })?
        .map_err(ToolError::Io)
}

/// Appends captured stderr to an `Execution` error's message, if any was captured.
fn attach_stderr(err: ToolError, stderr: &str) -> ToolError {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        return err;
    }
    match err {
        ToolError::Execution { tool, message } => ToolError::Execution {
            tool,
            message: format!("{message} — stderr: {stderr}"),
        },
        other => other,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    const UNRESPONSIVE_SERVER_SCRIPT: &str =
        "echo $$ > \"$PID_PATH\"; (sleep 2; printf late > \"$MARKER_PATH\") & wait";
    #[cfg(unix)]
    const SLOW_SHUTDOWN_SERVER_SCRIPT: &str = r#"
        IFS= read -r init
        printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}'
        IFS= read -r initialized
        IFS= read -r call
        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"done"}]}}'
        echo $$ > "$PID_PATH"
        (sleep 3; printf late > "$MARKER_PATH") &
        wait
    "#;
    #[cfg(unix)]
    const NOTIFYING_SERVER_SCRIPT: &str = r#"
        IFS= read -r init
        printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1}}'
        printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}'
        IFS= read -r initialized
        IFS= read -r call
        printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info"}}'
        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"done"}]}}'
    "#;
    #[cfg(unix)]
    const STRUCTURED_ERROR_SERVER_SCRIPT: &str = r#"
        IFS= read -r init
        printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}'
        IFS= read -r initialized
        IFS= read -r call
        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[],"structuredContent":{"reason":"denied"}}}'
    "#;
    #[cfg(unix)]
    const WRONG_ID_SERVER_SCRIPT: &str = r#"
        IFS= read -r init
        printf '%s\n' '{"jsonrpc":"2.0","id":99,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}'
    "#;
    #[cfg(unix)]
    const INVALID_INITIALIZE_SERVER_SCRIPT: &str = r#"
        IFS= read -r init
        printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{}}'
    "#;
    #[cfg(unix)]
    const PAGINATED_TOOLS_LIST_SERVER_SCRIPT: &str = r#"
        IFS= read -r init
        printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}'
        IFS= read -r initialized
        IFS= read -r list1
        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"alpha","description":"first tool","inputSchema":{"type":"object"}}],"nextCursor":"page2"}}'
        IFS= read -r list2
        printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"beta","inputSchema":{"type":"object","properties":{"x":{"type":"string"}}}}]}}'
    "#;

    // Test round-trip serialisation of the JSON-RPC request shape.
    #[test]
    fn jsonrpc_request_serialises_correctly() {
        let mut args: IndexMap<String, serde_json::Value> = IndexMap::new();
        args.insert("query".to_owned(), serde_json::json!("hello"));

        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "tools/call",
            params: ToolCallParams {
                name: "my-tool",
                arguments: &args,
            },
        };

        let json = serde_json::to_string(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["method"], "tools/call");
        assert_eq!(v["params"]["name"], "my-tool");
        assert_eq!(v["params"]["arguments"]["query"], "hello");
    }

    // Test that the response deserialiser handles both result and error shapes.
    #[test]
    fn jsonrpc_response_parses_result() {
        let raw = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [
                    {"type": "text", "text": "42"}
                ]
            }
        }"#;
        let EnvelopeDisposition::Response { result, error } =
            classify_envelope(raw, TOOL_CALL_REQUEST_ID).unwrap()
        else {
            panic!("expected response")
        };
        assert!(error.is_none());
        let result: McpCallResult = serde_json::from_value(result.unwrap()).unwrap();
        assert_eq!(first_text_content(&result), "42");
    }

    #[test]
    fn jsonrpc_response_parses_error() {
        let raw = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32600, "message": "Invalid Request"}
        }"#;
        let EnvelopeDisposition::Response { result, error } =
            classify_envelope(raw, TOOL_CALL_REQUEST_ID).unwrap()
        else {
            panic!("expected response")
        };
        assert!(result.is_none());
        let err = error.unwrap();
        assert_eq!(err.code, -32600);
        assert_eq!(err.message, "Invalid Request");
    }

    #[test]
    fn jsonrpc_response_empty_content_gives_empty_text() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[]}}"#;
        let EnvelopeDisposition::Response { result, .. } =
            classify_envelope(raw, TOOL_CALL_REQUEST_ID).unwrap()
        else {
            panic!("expected response")
        };
        let result: McpCallResult = serde_json::from_value(result.unwrap()).unwrap();
        assert_eq!(first_text_content(&result), "");
    }

    #[test]
    fn first_text_content_skips_non_text_blocks() {
        let raw = r#"{
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "content": [
                    {"type": "image", "text": "not this"},
                    {"type": "text", "text": "this one"}
                ]
            }
        }"#;
        let EnvelopeDisposition::Response { result, .. } =
            classify_envelope(raw, TOOL_CALL_REQUEST_ID).unwrap()
        else {
            panic!("expected response")
        };
        let result: McpCallResult = serde_json::from_value(result.unwrap()).unwrap();
        assert_eq!(first_text_content(&result), "this one");
    }

    #[test]
    fn legal_notification_is_classified_separately() {
        let raw = r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{}}"#;
        assert!(matches!(
            classify_envelope(raw, INITIALIZE_REQUEST_ID).unwrap(),
            EnvelopeDisposition::Notification
        ));
    }

    #[test]
    fn wrong_response_id_is_rejected() {
        let raw = r#"{"jsonrpc":"2.0","id":99,"result":{}}"#;
        let error = classify_envelope(raw, INITIALIZE_REQUEST_ID).unwrap_err();
        assert!(error.contains("does not match request id 0"));
    }

    #[test]
    fn attach_stderr_appends_to_execution_error() {
        let err = ToolError::Execution {
            tool: "write-file".to_owned(),
            message: "invalid JSON-RPC response: EOF — raw: ".to_owned(),
        };
        let with_stderr = attach_stderr(err, "Error: ENOENT: no such file or directory\n");
        match with_stderr {
            ToolError::Execution { message, .. } => {
                assert!(message.contains("stderr: Error: ENOENT"));
            }
            _ => panic!("expected Execution variant"),
        }
    }

    #[test]
    fn attach_stderr_is_noop_when_empty() {
        let err = ToolError::timeout("write-file", 30);
        let unchanged = attach_stderr(err, "   \n");
        assert!(matches!(unchanged, ToolError::Timeout { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_server_and_descendants_before_they_can_act() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("late-side-effect");
        let pid_file = temp.path().join("server.pid");
        let config = shell_server_config(UNRESPONSIVE_SERVER_SCRIPT, &marker, &pid_file);

        let error = run(&config, &IndexMap::new(), Some(1)).await.unwrap_err();
        assert!(matches!(error, ToolError::Timeout { secs: 1, .. }));

        assert_process_is_gone_and_side_effect_absent(&pid_file, &marker, 1_500).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn server_that_ignores_eof_is_killed_after_shutdown_grace() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("late-side-effect");
        let pid_file = temp.path().join("server.pid");
        let config = shell_server_config(SLOW_SHUTDOWN_SERVER_SCRIPT, &marker, &pid_file);

        let output = run(&config, &IndexMap::new(), Some(5)).await.unwrap();
        assert_eq!(output.stdout, "done");

        assert_process_is_gone_and_side_effect_absent(&pid_file, &marker, 2_200).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notifications_before_matching_responses_are_skipped() {
        let output = run(
            &protocol_server_config(NOTIFYING_SERVER_SCRIPT),
            &IndexMap::new(),
            Some(5),
        )
        .await
        .unwrap();
        assert_eq!(output.stdout, "done");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_tools_follows_pagination_and_defaults_missing_description() {
        let config = protocol_server_config(PAGINATED_TOOLS_LIST_SERVER_SCRIPT);
        let tools = list_tools(&config.transport, Some(5)).await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "alpha");
        assert_eq!(tools[0].description, "first tool");
        assert_eq!(tools[1].name, "beta");
        assert_eq!(tools[1].description, "");
        assert_eq!(
            tools[1].input_schema["properties"]["x"]["type"],
            serde_json::json!("string")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_tools_reports_protocol_errors() {
        let config = protocol_server_config(WRONG_ID_SERVER_SCRIPT);
        let error = list_tools(&config.transport, Some(5)).await.unwrap_err();
        assert!(error.to_string().contains("does not match request id 0"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn structured_only_tool_error_is_an_execution_error() {
        let error = run(
            &protocol_server_config(STRUCTURED_ERROR_SERVER_SCRIPT),
            &IndexMap::new(),
            Some(5),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ToolError::Execution { .. }));
        assert!(error.to_string().contains(r#""reason":"denied""#));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wrong_response_id_fails_the_session() {
        let error = run(
            &protocol_server_config(WRONG_ID_SERVER_SCRIPT),
            &IndexMap::new(),
            Some(5),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("does not match request id 0"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_initialize_result_fails_the_session() {
        let error = run(
            &protocol_server_config(INVALID_INITIALIZE_SERVER_SCRIPT),
            &IndexMap::new(),
            Some(5),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("invalid MCP initialize result"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_stdout_line_is_rejected() {
        let script = format!(
            "IFS= read -r init; head -c {} /dev/zero | tr '\\0' x",
            MAX_MCP_STDOUT_LINE_BYTES + 1
        );
        let error = run(&protocol_server_config(&script), &IndexMap::new(), Some(5))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(OUTPUT_LIMIT_MARKER));
        assert!(error.to_string().len() < 256);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_tool_content_is_rejected() {
        let script = format!(
            r#"
            IFS= read -r init
            printf '%s\n' '{{"jsonrpc":"2.0","id":0,"result":{{"protocolVersion":"2024-11-05","capabilities":{{}},"serverInfo":{{"name":"test","version":"1"}}}}}}'
            IFS= read -r initialized
            IFS= read -r call
            printf '%s' '{{"jsonrpc":"2.0","id":1,"result":{{"content":[{{"type":"text","text":"'
            head -c {} /dev/zero | tr '\0' x
            printf '%s\n' '"}}]}}}}'
            "#,
            MAX_MCP_CONTENT_BYTES + 1
        );
        let error = run(&protocol_server_config(&script), &IndexMap::new(), Some(5))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(OUTPUT_LIMIT_MARKER));
        assert!(error.to_string().len() < 256);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_stderr_is_rejected() {
        let script = format!(
            r#"
            IFS= read -r init
            printf '%s\n' '{{"jsonrpc":"2.0","id":0,"result":{{"protocolVersion":"2024-11-05","capabilities":{{}},"serverInfo":{{"name":"test","version":"1"}}}}}}'
            IFS= read -r initialized
            IFS= read -r call
            printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"content":[{{"type":"text","text":"done"}}]}}}}'
            head -c {} /dev/zero >&2
            "#,
            MAX_MCP_STDERR_BYTES + 1
        );
        let error = run(&protocol_server_config(&script), &IndexMap::new(), Some(5))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(OUTPUT_LIMIT_MARKER));
        assert!(error.to_string().len() < 256);
    }

    #[cfg(unix)]
    fn shell_server_config(
        script: &str,
        marker: &std::path::Path,
        pid_file: &std::path::Path,
    ) -> McpConfig {
        McpConfig {
            tool_name: "test-tool".to_owned(),
            transport: McpTransport::Stdio {
                server_command: "sh".to_owned(),
                server_args: vec!["-c".to_owned(), script.to_owned()],
                server_env: [
                    (
                        "MARKER_PATH".to_owned(),
                        marker.to_string_lossy().into_owned(),
                    ),
                    (
                        "PID_PATH".to_owned(),
                        pid_file.to_string_lossy().into_owned(),
                    ),
                ]
                .into_iter()
                .collect(),
            },
        }
    }

    #[cfg(unix)]
    fn protocol_server_config(script: &str) -> McpConfig {
        McpConfig {
            tool_name: "test-tool".to_owned(),
            transport: McpTransport::Stdio {
                server_command: "sh".to_owned(),
                server_args: vec!["-c".to_owned(), script.to_owned()],
                server_env: IndexMap::new(),
            },
        }
    }

    #[cfg(unix)]
    async fn assert_process_is_gone_and_side_effect_absent(
        pid_file: &std::path::Path,
        marker: &std::path::Path,
        wait_millis: u64,
    ) {
        let pid = std::fs::read_to_string(pid_file).unwrap().trim().to_owned();
        tokio::time::sleep(Duration::from_millis(wait_millis)).await;

        assert!(
            !marker.exists(),
            "an MCP descendant performed its delayed side effect after cleanup"
        );
        assert!(
            !process_is_alive(&pid),
            "cleaned-up MCP server process {pid} is still alive"
        );
    }

    #[cfg(unix)]
    fn process_is_alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
