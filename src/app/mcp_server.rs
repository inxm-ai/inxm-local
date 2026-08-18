//! Local Streamable-HTTP MCP server for controlling INXM Local.
//!
//! The server starts with the desktop client and exposes the same workflow
//! primitives as the chat slash commands over JSON-RPC at `POST /mcp`.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::app::activity::{ActivityKind, ActivityOrigin, ActivityRegistry};
use crate::app::engine::{self, AppSettings, DataPaths};
use crate::app::schedule_store;
use crate::executor::{self, ExecutorConfig, HumanDecision, HumanRequest, ProgressEvent};
use crate::hostenv::EnvProbe;
use crate::plan::bundle::PlanBundle;
use crate::repair;
use crate::storage::StorageRoot;
use crate::tools::catalog::ToolCatalog;

#[derive(Debug, Clone)]
pub enum ServerStatus {
    Starting {
        port: u16,
    },
    /// `fallback_from` is set when the configured port could not be bound
    /// (e.g. transiently reserved by Windows/WSL2 networking) and the
    /// server fell back to an OS-assigned ephemeral port instead.
    Running {
        port: u16,
        fallback_from: Option<u16>,
    },
    Failed {
        port: u16,
        error: String,
    },
}

impl ServerStatus {
    pub fn label(&self) -> String {
        match self {
            Self::Starting { port } => format!("MCP starting on 127.0.0.1:{port}"),
            Self::Running {
                port,
                fallback_from: Some(requested),
            } => format!("MCP 127.0.0.1:{port} (configured port {requested} was unavailable)"),
            Self::Running { port, .. } => format!("MCP 127.0.0.1:{port}"),
            Self::Failed { port, .. } => format!("MCP failed on 127.0.0.1:{port}"),
        }
    }

    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Failed { error, .. } => Some(error.as_str()),
            _ => None,
        }
    }
}

/// Outcome of a detached compile/edit, shared with any retry that arrives
/// while the original is still running. `anyhow::Error` isn't `Clone`, so
/// Dedup-key namespaces for [`await_detached`] — `compile_plan` and
/// `edit_plan` share one in-flight registry, so each keys its fingerprint
/// under its own prefix to avoid an (unrealistic but free to rule out)
/// collision between an intent string and a plan_ref+instruction pair.
///
/// The trailing `\u{1f}` (unit separator) cannot occur in user-supplied text,
/// so a key can never be forged across an argument boundary either.
const COMPILE_PLAN_DEDUP_PREFIX: &str = "compile_plan\u{1f}";
const EDIT_PLAN_DEDUP_PREFIX: &str = "edit_plan\u{1f}";
/// Separator between the fields of a multi-argument dedup key.
const DEDUP_FIELD_SEPARATOR: char = '\u{1f}';

#[derive(Clone)]
struct McpState {
    paths: DataPaths,
    activities: ActivityRegistry,
    /// Compile/edit work currently running detached from any HTTP request,
    /// keyed by a normalized request fingerprint (see [`await_detached`]).
    /// A client retry — typically fired after its own tool-call timeout
    /// while the original compile is still running — joins the detached task
    /// instead of paying for a second compile that saves a duplicate plan.
    compiles: InFlightCompiles,
}

/// Latest published outcome of a detached compile: `None` while in flight,
/// then exactly one `Some` when the work finishes. Errors travel as strings
/// so joined waiters can share one clonable value.
type CompileOutcome = Option<Result<Value, String>>;

/// Registry of in-flight detached compiles. Holding a receiver clone here
/// keeps the watch channel alive even after every requesting client has
/// disconnected, and lets an identical retry join the running work.
type InFlightCompiles = Arc<Mutex<HashMap<String, tokio::sync::watch::Receiver<CompileOutcome>>>>;

// JSON-RPC 2.0 error codes (rather than collapsing everything onto the
// implementation-defined `-32000`).
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Marker for argument-shape failures inside `call_tool`, so they can be
/// mapped to JSON-RPC `-32602 Invalid params` while genuine tool-execution
/// failures become normal results with `isError: true`.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct InvalidParams(String);

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: Option<String>,
    /// Presence, not value: `None` means the `id` member was **absent**, i.e. a
    /// notification (§4.1); `Some(Value::Null)` an explicit `"id": null`.
    ///
    /// §4 discourages a null id but permits it, and §5 then requires an
    /// ordinary response echoing `"id": null`. A plain `Option<Value>`
    /// deserialises both an absent member and an explicit null to `None`, which
    /// would silently demote such a call to a notification: HTTP 202, method
    /// never executed, caller waiting for a response that never comes.
    #[serde(default, deserialize_with = "id_member")]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

/// Keep an explicit `"id": null` distinguishable from an absent `id`.
///
/// serde only invokes `deserialize_with` when the member is present, so every
/// value — `null` included — arrives here and becomes `Some`, while
/// `#[serde(default)]` supplies `None` for an absent member. This is the usual
/// `Option<Option<T>>` double-option trick with one level saved: `Value`
/// already models JSON null.
fn id_member<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    /// Always serialized: JSON-RPC 2.0 §5 requires `id` on every response, and
    /// mandates `null` when the request's id could not be determined. Omitting
    /// the member makes strict clients reject the object.
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Deserialize)]
struct CompileArgs {
    intent: String,
}

#[derive(Debug, Deserialize)]
struct PlanRefArgs {
    plan_ref: String,
}

#[derive(Debug, Deserialize)]
struct ExportPlanArgs {
    plan_ref: String,
    output_path: PathBuf,
}

/// Arguments for `import_plan`: the bundle travels inline as JSON, never as a
/// filesystem path — an unauthenticated caller must not gain an arbitrary-file
/// -read primitive (the mirror of the export-path confinement).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportPlanArgs {
    bundle: Value,
    /// Refuse a same-name plan unless a caller explicitly requests a copy or
    /// a new version of the one local lineage.
    #[serde(default)]
    on_conflict: engine::ImportConflictPolicy,
}

#[derive(Debug, Deserialize)]
struct EditArgs {
    plan_ref: String,
    instruction: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecuteArgs {
    plan_ref: String,
    /// Resume a run returned with `elicitation_required` without repeating
    /// already-completed steps.
    #[serde(default)]
    run_id: Option<String>,
    /// Invocation values matching the plan's declared inputs.
    #[serde(default)]
    inputs: Option<IndexMap<String, Value>>,
    /// Human answers keyed by step id. Values can be strings, true/false for
    /// approval prompts, or {"decision":"approve|reject|text","text":"..."}.
    #[serde(default)]
    human_responses: HashMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct RunRefArgs {
    run_id: String,
}

/// Arguments for `apply_patch`.
#[derive(Debug, Deserialize)]
struct PatchRefArgs {
    patch_id: String,
}

/// Arguments for `reject_patch`: the reason is optional but recorded when
/// given, so a later reader knows why a proposal was turned down.
#[derive(Debug, Deserialize)]
struct RejectPatchArgs {
    patch_id: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeRunArgs {
    run_id: String,
    /// Replacement invocation values to apply while continuing a repaired
    /// failed run. The executor validates which inputs may be changed.
    #[serde(default)]
    inputs: IndexMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct ScheduleArgs {
    plan_ref: String,
    cron: String,
    /// Invocation values captured on this schedule.
    #[serde(default)]
    inputs: IndexMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct ScheduleRefArgs {
    schedule_id: String,
}

#[derive(Debug, Deserialize)]
struct ScheduleEnableArgs {
    schedule_id: String,
    enabled: bool,
}

/// Paging for the list tools.
#[derive(Debug, Default, Deserialize)]
struct ListArgs {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

impl ListArgs {
    /// Apply `offset`/`limit` to an already-ordered listing. `default_limit`
    /// caps the result when the caller does not pass an explicit `limit`.
    fn paginate<T>(&self, items: Vec<T>, default_limit: Option<usize>) -> Vec<T> {
        items
            .into_iter()
            .skip(self.offset.unwrap_or(0))
            .take(self.limit.or(default_limit).unwrap_or(usize::MAX))
            .collect()
    }
}

/// Runs accumulate forever (no retention yet), so `list_runs` without an
/// explicit `limit` returns at most this many newest-first entries.
const DEFAULT_LIST_RUNS_LIMIT: usize = 100;

pub fn spawn(paths: DataPaths, port: u16) -> std::sync::mpsc::Receiver<ServerStatus> {
    spawn_with_activities(paths, port, ActivityRegistry::default())
}

/// Start the server with a registry shared with the desktop engine. The
/// two-argument [`spawn`] remains available for headless callers and tests.
pub fn spawn_with_activities(
    paths: DataPaths,
    port: u16,
    activities: ActivityRegistry,
) -> std::sync::mpsc::Receiver<ServerStatus> {
    let (status_tx, status_rx) = std::sync::mpsc::channel();
    let _ = status_tx.send(ServerStatus::Starting { port });

    std::thread::Builder::new()
        .name("inxm-mcp-http".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = status_tx.send(ServerStatus::Failed {
                        port,
                        error: format!("failed to build MCP runtime: {error}"),
                    });
                    return;
                }
            };

            runtime.block_on(async move {
                let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
                let (listener, fallback_from) = match TcpListener::bind(addr).await {
                    Ok(listener) => (listener, None),
                    Err(primary_error) => {
                        // The configured port may be transiently unavailable for
                        // reasons that have nothing to do with another process
                        // actually using it — e.g. Windows/WSL2's Hyper-V
                        // networking stack (HNS/WinNAT) can reserve port ranges
                        // that never show up in `netstat` or `netsh`'s exclusion
                        // list. Rather than failing outright, fall back to an
                        // OS-assigned ephemeral port, which is always free.
                        let fallback_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
                        match TcpListener::bind(fallback_addr).await {
                            Ok(listener) => (listener, Some(port)),
                            Err(fallback_error) => {
                                let _ = status_tx.send(ServerStatus::Failed {
                                    port,
                                    error: format!(
                                        "{primary_error} (fallback to an ephemeral port \
                                         also failed: {fallback_error})"
                                    ),
                                });
                                return;
                            }
                        }
                    }
                };

                let bound_port = listener
                    .local_addr()
                    .map(|addr| addr.port())
                    .unwrap_or(port);

                let app = Router::new()
                    .route("/health", get(health))
                    .route("/mcp", post(handle_mcp))
                    .route("/", post(handle_mcp))
                    .layer(axum::middleware::from_fn(require_loopback_caller))
                    .with_state(Arc::new(McpState {
                        paths,
                        activities,
                        compiles: Arc::default(),
                    }));

                let _ = status_tx.send(ServerStatus::Running {
                    port: bound_port,
                    fallback_from,
                });
                if let Err(error) = axum::serve(listener, app).await {
                    let _ = status_tx.send(ServerStatus::Failed {
                        port: bound_port,
                        error: error.to_string(),
                    });
                }
            });
        })
        .expect("failed to spawn MCP server thread");

    status_rx
}

async fn health() -> Json<Value> {
    // Deliberately terse: the endpoint is unauthenticated, and echoing
    // `data_dir` here would leak the OS username and on-disk storage
    // location.
    Json(json!({
        "status": "ok",
        "endpoint": "/mcp"
    }))
}

/// Anti-DNS-rebinding guard. The server only listens on
/// loopback, but a web page the user merely visits can rebind its own
/// hostname to 127.0.0.1 and then drive this unauthenticated endpoint as if
/// it were same-origin. The rebound requests still carry the attacker's
/// hostname in `Host` (and `Origin`), so rejecting anything non-loopback in
/// those headers restores the "loopback == trusted" assumption.
async fn require_loopback_caller(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !headers_are_loopback(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            "forbidden: Host/Origin must be loopback",
        )
            .into_response();
    }
    next.run(request).await
}

/// `Host` must name loopback when present (HTTP/1.1 always sends it; treat
/// absence as a non-browser client). `Origin` is absent for non-browser
/// clients and same-origin non-CORS requests; when a browser does send it,
/// it must be a loopback origin too.
fn headers_are_loopback(headers: &HeaderMap) -> bool {
    let host_ok = headers
        .get(header::HOST)
        .map(|value| value.to_str().is_ok_and(host_is_loopback))
        .unwrap_or(true);
    let origin_ok = headers
        .get(header::ORIGIN)
        .map(|value| value.to_str().is_ok_and(origin_is_loopback))
        .unwrap_or(true);
    host_ok && origin_ok
}

/// Whether a `Host` header value (`host` or `host:port`) names loopback.
fn host_is_loopback(value: &str) -> bool {
    let host = value.trim();
    // Bracketed IPv6 (`[::1]:39387` / `[::1]`) carries its own delimiter.
    let host = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((inner, _port)) => inner,
            None => return false,
        }
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

/// Whether an `Origin` header value is a loopback origin. `null` (sandboxed
/// contexts) and non-http(s) schemes are rejected.
fn origin_is_loopback(value: &str) -> bool {
    let origin = value.trim();
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .is_some_and(host_is_loopback)
}

async fn handle_mcp(State(state): State<Arc<McpState>>, body: axum::body::Bytes) -> Response {
    // Parse the body ourselves rather than through the `Json` extractor:
    // malformed payloads must produce spec-compliant JSON-RPC error objects,
    // not axum's raw HTTP 400/422 text responses.
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return rpc_error(None, PARSE_ERROR, &format!("parse error: {error}")).into_response();
        }
    };
    if value.is_array() {
        // MCP 2025-06-18 dropped JSON-RPC batching; say so instead of
        // surfacing an opaque serde type-mismatch.
        return rpc_error(None, INVALID_REQUEST, "batch requests are not supported")
            .into_response();
    }
    // Salvage the id for the error echo even when the envelope is malformed.
    // An absent id and an explicit null both end up as `"id": null` on the
    // wire here, which §5 mandates for an id that cannot be determined.
    let id = value.get("id").cloned();
    let request: JsonRpcRequest = match serde_json::from_value(value) {
        Ok(request) => request,
        Err(error) => {
            return rpc_error(id, INVALID_REQUEST, &format!("invalid request: {error}"))
                .into_response();
        }
    };

    if request.jsonrpc.as_deref().is_some_and(|v| v != "2.0") {
        return rpc_error(request.id, INVALID_REQUEST, "expected JSON-RPC 2.0").into_response();
    }

    // JSON-RPC 2.0 §4.1: a request without `id` is a notification, and the
    // server MUST NOT reply to one. The defining property is the *absent* id,
    // not the method name — keying on a `notifications/` prefix would execute
    // an id-less `tools/call` and answer it — and not a null id either: an
    // explicit `"id": null` is a normal request that must run and be answered
    // with `"id": null` (see `JsonRpcRequest::id`).
    if request.id.is_none() {
        return StatusCode::ACCEPTED.into_response();
    }

    let result = match request.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {
                "tools": { "listChanged": false },
                "elicitation": {}
            },
            "serverInfo": {
                "name": "inxm-local",
                "version": env!("CARGO_PKG_VERSION")
            }
        })),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => match serde_json::from_value::<ToolCallParams>(request.params) {
            Ok(params) => call_tool(&state, params).await,
            Err(error) => Err(JsonRpcError {
                code: INVALID_PARAMS,
                message: format!("invalid tools/call params: {error}"),
            }),
        },
        "ping" => Ok(json!({})),
        other => Err(JsonRpcError {
            code: METHOD_NOT_FOUND,
            message: format!("method '{other}' not found"),
        }),
    };

    match result {
        Ok(result) => Json(JsonRpcResponse {
            jsonrpc: "2.0",
            // The fallback is unreachable: a request with an absent id
            // returned above as a notification, so what is left is the
            // caller's id — possibly the explicit `Value::Null` of §4.
            id: request.id.clone().unwrap_or(Value::Null),
            result: Some(result),
            error: None,
        })
        .into_response(),
        Err(error) => rpc_error(request.id, error.code, &error.message).into_response(),
    }
}

/// Build an error response. `id` is `None` only when the request's id could
/// not be determined, which the spec requires to be reported as `null`.
fn rpc_error(id: Option<Value>, code: i64, message: &str) -> Json<JsonRpcResponse> {
    Json(JsonRpcResponse {
        jsonrpc: "2.0",
        id: id.unwrap_or(Value::Null),
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_owned(),
        }),
    })
}

async fn call_tool(state: &McpState, params: ToolCallParams) -> Result<Value, JsonRpcError> {
    let tool_kind = match params.name.as_str() {
        "compile_plan" => "compile_plan",
        "list_plans" => "list_plans",
        "show_plan" => "show_plan",
        "export_plan" => "export_plan",
        "import_plan" => "import_plan",
        "edit_plan" => "edit_plan",
        "execute_plan" => "execute_plan",
        "list_runs" => "list_runs",
        "inspect_run" => "inspect_run",
        "repair_run" => "repair_run",
        "resume_run" => "resume_run",
        "list_patches" => "list_patches",
        "apply_patch" => "apply_patch",
        "reject_patch" => "reject_patch",
        "schedule_plan" => "schedule_plan",
        "list_schedules" => "list_schedules",
        "delete_schedule" => "delete_schedule",
        "set_schedule_enabled" => "set_schedule_enabled",
        _ => "unknown",
    };
    let requested_run_id = matches!(
        tool_kind,
        "execute_plan" | "inspect_run" | "repair_run" | "resume_run"
    )
    .then(|| params.arguments.get("run_id")?.as_str().map(str::to_owned))
    .flatten();
    let request_id = uuid::Uuid::new_v4().to_string();
    let started = std::time::Instant::now();
    let result: anyhow::Result<Value> = async {
        Ok(match params.name.as_str() {
            "compile_plan" => compile_plan(state, args(params.arguments)?).await?,
            "list_plans" => list_plans(state)?,
            "show_plan" => show_plan(state, args(params.arguments)?)?,
            "export_plan" => export_plan(state, args(params.arguments)?)?,
            "import_plan" => import_plan(state, args(params.arguments)?)?,
            "edit_plan" => edit_plan(state, args(params.arguments)?).await?,
            "execute_plan" => execute_plan(state, args(params.arguments)?).await?,
            "list_runs" => list_runs(state, list_args(params.arguments)?)?,
            "inspect_run" => inspect_run(state, args(params.arguments)?)?,
            "repair_run" => repair_run(state, args(params.arguments)?).await?,
            "resume_run" => resume_run(state, args(params.arguments)?).await?,
            "list_patches" => list_patches(state, list_args(params.arguments)?)?,
            "apply_patch" => apply_patch(state, args(params.arguments)?)?,
            "reject_patch" => reject_patch(state, args(params.arguments)?)?,
            "schedule_plan" => schedule_plan(state, args(params.arguments)?)?,
            "list_schedules" => list_schedules(state, list_args(params.arguments)?)?,
            "delete_schedule" => delete_schedule(state, args(params.arguments)?)?,
            "set_schedule_enabled" => set_schedule_enabled(state, args(params.arguments)?)?,
            other => {
                return Err(anyhow::Error::new(InvalidParams(format!(
                    "unknown tool '{other}'"
                ))));
            }
        })
    }
    .await;
    let duration_ms = started.elapsed().as_millis() as u64;
    let run_id = result
        .as_ref()
        .ok()
        .and_then(|value| {
            value
                .pointer("/run/id")
                .or_else(|| value.get("run_id"))
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
        .or(requested_run_id);
    let plan_id = result.as_ref().ok().and_then(|value| {
        value
            .pointer("/plan/metadata/id")
            .or_else(|| value.get("plan_id"))
            .or_else(|| value.pointer("/run/plan_id"))
            .and_then(Value::as_str)
    });
    let schedule_id = result
        .as_ref()
        .ok()
        .and_then(|value| value.pointer("/schedule/id"))
        .and_then(Value::as_str);

    if result.is_ok() {
        tracing::info!(
            request_id,
            tool_kind,
            run_id = ?run_id,
            plan_id = ?plan_id,
            schedule_id = ?schedule_id,
            app_version = env!("CARGO_PKG_VERSION"),
            triggered_by = "mcp_client",
            duration_ms,
            outcome = "success",
            "MCP tool request completed"
        );
    } else {
        tracing::error!(
            request_id,
            tool_kind,
            run_id = ?run_id,
            plan_id = ?plan_id,
            schedule_id = ?schedule_id,
            app_version = env!("CARGO_PKG_VERSION"),
            triggered_by = "mcp_client",
            duration_ms,
            outcome = "failure",
            "MCP tool request completed"
        );
    }
    let structured = match result {
        Ok(structured) => structured,
        Err(error) => {
            // Malformed arguments and unknown tool names are protocol-level
            // per MCP: JSON-RPC `-32602 Invalid params`.
            if let Some(invalid) = error.downcast_ref::<InvalidParams>() {
                return Err(JsonRpcError {
                    code: INVALID_PARAMS,
                    message: invalid.to_string(),
                });
            }
            // Genuine tool-execution failures are a normal result with
            // `isError: true`, not a protocol error.
            return Ok(json!({
                "content": [{
                    "type": "text",
                    "text": engine::format_error_chain(&error)
                }],
                "isError": true
            }));
        }
    };

    let text = serde_json::to_string_pretty(&structured).map_err(|error| JsonRpcError {
        code: -32000,
        message: format!("failed to serialize tool result: {error}"),
    })?;
    Ok(json!({
        "content": [{
            "type": "text",
            "text": text
        }],
        "structuredContent": structured,
        "isError": false
    }))
}

fn args<T: for<'de> Deserialize<'de>>(value: Value) -> anyhow::Result<T> {
    serde_json::from_value(value)
        .map_err(|e| anyhow::Error::new(InvalidParams(format!("invalid tool arguments: {e}"))))
}

/// Like [`args`], but tolerates the `arguments` field being omitted entirely
/// (it defaults to JSON `null`) — the list tools take only optional paging.
fn list_args(value: Value) -> anyhow::Result<ListArgs> {
    if value.is_null() {
        return Ok(ListArgs::default());
    }
    args(value)
}

fn storage(state: &McpState) -> anyhow::Result<StorageRoot> {
    Ok(StorageRoot::open(&state.paths.data_dir)?)
}

/// Console for an MCP-triggered compile: no UI notify hook, but the same
/// per-compile log file under the data dir as desktop compiles.
fn compile_console(state: &McpState, label: &str) -> super::console::CompileConsole {
    super::console::CompileConsole::new(
        label,
        Some(&super::console::default_log_dir(&state.paths.data_dir)),
        state.activities.console_notify(),
    )
}

fn catalog(state: &McpState) -> anyhow::Result<ToolCatalog> {
    state
        .paths
        .mutations
        .run_named("catalog.seed", "mcp_client", || {
            if !state.paths.catalog_path.exists() {
                if let Some(parent) = state.paths.catalog_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&state.paths.catalog_path, engine::default_catalog_yaml())?;
            }
            Ok(ToolCatalog::load_from_file(&state.paths.catalog_path)?)
        })
}

/// Compile in a task detached from the HTTP request lifetime.
///
/// Awaiting the compiler inside the axum handler ties the subprocess to the
/// connection: axum cancels the handler when the client disconnects (e.g. a
/// tool-call timeout shorter than the compile), the dropped future kills the
/// `kill_on_drop` compiler CLI, and all work vanishes without a trace. The
/// work therefore runs in a spawned task that always finishes and persists
/// the plan — a client that timed out finds it via `list_plans`, and an
/// identical retry joins the in-flight compile instead of forking a second
/// subprocess.
async fn compile_plan(state: &McpState, args: CompileArgs) -> anyhow::Result<Value> {
    let key = format!("{COMPILE_PLAN_DEDUP_PREFIX}{}", args.intent.trim());
    let task_state = state.clone();
    await_detached(&state.compiles, "compile_plan", key, async move {
        compile_plan_detached(&task_state, args).await
    })
    .await
}

async fn compile_plan_detached(state: &McpState, args: CompileArgs) -> anyhow::Result<Value> {
    let activity = state
        .activities
        .start(ActivityOrigin::Mcp, ActivityKind::Compile);
    let result = compile_plan_work(state, args, activity.id()).await;
    match &result {
        Ok(_) => activity.succeeded(),
        Err(error) => activity.failed(format!("{error:#}")),
    }
    result
}

async fn compile_plan_work(
    state: &McpState,
    args: CompileArgs,
    activity_id: u64,
) -> anyhow::Result<Value> {
    // No UI watches an MCP compile, but the console still persists a
    // per-compile log file for a post-mortem trail.
    let console = compile_console(state, "mcp compile");
    state
        .activities
        .attach_console(activity_id, console.clone());
    console.info(format!("intent: {}", args.intent));
    let catalog = catalog(state)?;
    let settings = AppSettings::load(&state.paths.settings_path);
    let backend = engine::create_configured_backend(&settings)?;
    let request = engine::compile_request(&catalog, &settings, args.intent, None);
    let plan = crate::llm::with_cli_line_sink(
        std::sync::Arc::new(console.clone()),
        engine::compile_validate_normalize(
            &backend,
            request,
            &catalog,
            "MCP compilation failed",
            Some(&console),
        ),
    )
    .await
    .inspect_err(|error| console.close(format!("✗ {error:#}")))?;
    console.close_after_persisting(
        || {
            state
                .paths
                .mutations
                .run_named("plan.compile_save", "mcp_client", || {
                    Ok(storage(state)?.plans().save(&plan)?)
                })
        },
        "compiled, but saving the plan failed",
        || {
            format!(
                "✓ compiled “{}” — {} steps, validated",
                plan.name,
                plan.steps.len()
            )
        },
    )?;
    count_usage(state, crate::telemetry::usage::Action::PlanCreated);
    Ok(json!({ "plan": plan }))
}

/// Warns when the awaiting MCP request future is dropped — axum cancels the
/// handler as soon as the client disconnects, typically because its own
/// tool-call timeout is shorter than the compile — while the detached work is
/// still running. Without this the abort would be completely silent: the task
/// keeps going and eventually logs its own outcome, but nothing would ever
/// record that the caller stopped listening.
struct DetachedCompileGuard {
    operation: &'static str,
    completed: bool,
}

impl DetachedCompileGuard {
    fn new(operation: &'static str) -> Self {
        Self {
            operation,
            completed: false,
        }
    }
}

impl Drop for DetachedCompileGuard {
    fn drop(&mut self) {
        if !self.completed {
            tracing::warn!(
                operation = self.operation,
                triggered_by = "mcp_client",
                "MCP client disconnected during {}; compilation continues detached \
                 and the plan will appear in list_plans when it finishes",
                self.operation
            );
        }
    }
}

/// Lock the in-flight registry, recovering a poisoned lock instead of
/// propagating the panic. A poisoned lock only means some other holder
/// panicked while the map was open; the map itself is still structurally
/// sound to read and mutate, and dedup bookkeeping is not worth failing a
/// compile — or a whole request — over.
fn lock_registry(
    registry: &InFlightCompiles,
) -> std::sync::MutexGuard<'_, HashMap<String, tokio::sync::watch::Receiver<CompileOutcome>>> {
    registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run `work` detached from the caller and await its published outcome.
///
/// The work is spawned as its own tokio task and always runs to completion,
/// even when the awaiting request handler is dropped mid-flight. A second
/// call with the same `key` while the work is still running joins the same
/// outcome instead of starting the work again — the natural client behavior
/// after a tool-call timeout is an identical retry, which would otherwise
/// fork one compiler subprocess per attempt.
async fn await_detached(
    registry: &InFlightCompiles,
    operation: &'static str,
    key: String,
    work: impl std::future::Future<Output = anyhow::Result<Value>> + Send + 'static,
) -> anyhow::Result<Value> {
    let mut receiver = {
        let mut in_flight = lock_registry(registry);
        match in_flight.get(&key) {
            Some(receiver) => {
                tracing::info!(
                    operation,
                    triggered_by = "mcp_client",
                    "identical request while a compile is in flight; joining it instead of \
                     starting a second compiler subprocess"
                );
                receiver.clone()
            }
            None => {
                let (sender, receiver) = tokio::sync::watch::channel(None);
                in_flight.insert(key.clone(), receiver.clone());
                let compile_id = uuid::Uuid::new_v4().to_string();
                tracing::info!(
                    operation,
                    compile_id,
                    triggered_by = "mcp_client",
                    "detached compile started"
                );
                let registry = registry.clone();
                // Keep the publisher outside the worker task: a panic in the
                // compiler/repair future then becomes a terminal result and
                // removes the dedup key instead of stranding retries forever.
                let worker = tokio::spawn(work);
                tokio::spawn(async move {
                    let started = std::time::Instant::now();
                    let outcome = match worker.await {
                        Ok(result) => result.map_err(|error| format!("{error:#}")),
                        Err(error) => Err(format!(
                            "detached {operation} task ended before reporting an outcome: {error}"
                        )),
                    };
                    let duration_ms = started.elapsed().as_millis() as u64;
                    match &outcome {
                        Ok(_) => tracing::info!(
                            operation,
                            compile_id,
                            duration_ms,
                            outcome = "success",
                            "detached compile finished"
                        ),
                        Err(error) => tracing::error!(
                            operation,
                            compile_id,
                            duration_ms,
                            outcome = "failure",
                            error,
                            "detached compile failed"
                        ),
                    }
                    // Publish before deregistering: the registry's receiver
                    // clone keeps the channel alive, so the send reaches any
                    // still-connected waiters even after every client hung up.
                    let _ = sender.send(Some(outcome));
                    lock_registry(&registry).remove(&key);
                });
                receiver
            }
        }
    };
    let mut guard = DetachedCompileGuard::new(operation);
    let outcome = receiver
        .wait_for(|outcome| outcome.is_some())
        .await
        .map_err(|_| {
            anyhow::anyhow!("the detached compile task ended without publishing a result")
        })?
        .clone()
        .expect("wait_for only returns once an outcome is published");
    guard.completed = true;
    outcome.map_err(|message| anyhow::anyhow!(message))
}

/// Consent-gated usage tally attributed to the MCP surface (see
/// `crate::telemetry::usage`); the desktop engine tallies its own side.
fn count_usage(state: &McpState, action: crate::telemetry::usage::Action) {
    crate::telemetry::usage::count(
        &state.paths.data_dir,
        &state.paths.settings_path,
        crate::telemetry::usage::Source::Mcp,
        action,
    );
}

/// Tally a terminal run outcome; non-terminal statuses (waiting for a
/// human) are skipped — the run comes back through once it finishes.
/// `healed` marks a successful post-repair resume, counted on top of
/// `RunSucceeded`.
fn count_run_outcome(state: &McpState, status: &executor::RunStatus, healed: bool) {
    use crate::telemetry::usage::Action;
    match status {
        executor::RunStatus::Succeeded => {
            count_usage(state, Action::RunSucceeded);
            if healed {
                count_usage(state, Action::RunHealed);
            }
        }
        executor::RunStatus::Failed { .. } => count_usage(state, Action::RunFailed),
        _ => {}
    }
}

fn list_plans(state: &McpState) -> anyhow::Result<Value> {
    let plans: Vec<engine::PlanListItem> = engine::list_plan_summaries(&storage(state)?)?;
    Ok(json!({ "plans": plans }))
}

fn show_plan(state: &McpState, args: PlanRefArgs) -> anyhow::Result<Value> {
    Ok(json!({ "plan": engine::resolve_plan(&storage(state)?, &args.plan_ref)? }))
}

fn export_plan(state: &McpState, args: ExportPlanArgs) -> anyhow::Result<Value> {
    // Unauthenticated callers must not gain an arbitrary-file-write
    // primitive: exports are confined to the app's export directory, and
    // the requested path may not escape it.
    let output_path =
        sanitized_export_path(&state.paths.data_dir.join("exports"), &args.output_path)?;
    let plan = engine::resolve_plan(&storage(state)?, &args.plan_ref)?;
    let (bundle, missing_tools) = PlanBundle::from_plan(&plan, &catalog(state)?);
    bundle.save_to_file(&output_path)?;
    // Report the path relative to the export root, not the absolute path: the
    // absolute form discloses `data_dir` (and usually the OS username), which
    // is the same leak #64 removed from /health.
    let relative_path = output_path
        .strip_prefix(&state.paths.data_dir)
        .unwrap_or(&output_path);
    Ok(json!({
        "format_version": bundle.format_version,
        "missing_tools": missing_tools,
        "plan_id": plan.metadata.id,
        "output_path": relative_path,
        "tool_references": bundle.tools.len(),
    }))
}

/// Import a plan bundle supplied inline as JSON, the counterpart to
/// `export_plan` so an agent can move a plan between machines over MCP alone.
///
/// Two deliberate boundaries versus the desktop importer: the bundle is passed
/// inline (no filesystem path — no arbitrary-read), and tools the bundle needs
/// that are absent from the local catalog are reported and refused rather than
/// silently synthesised by an LLM. A caller adds the tools first (they own that
/// trust decision), then imports.
fn import_plan(state: &McpState, args: ImportPlanArgs) -> anyhow::Result<Value> {
    // Route through `PlanBundle::from_json` (not a bare `from_value`) so the MCP
    // path enforces the same format-version ceiling and tools⟷TOOL_CALL
    // bijection as the desktop file importer.
    let bundle_json = serde_json::to_string(&args.bundle)
        .map_err(|e| anyhow::anyhow!("bundle is not serialisable JSON: {e}"))?;
    let bundle = PlanBundle::from_json(&bundle_json)
        .map_err(|e| anyhow::anyhow!("bundle is not a valid plan bundle: {e}"))?;

    let catalog = catalog(state)?;
    let missing: Vec<&str> = bundle
        .tools
        .iter()
        .map(|t| t.name.as_str())
        .filter(|name| !catalog.contains(name))
        .collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "this plan references {} tool(s) not in the local catalog ({}); add them first, \
             then import",
            missing.len(),
            missing.join(", ")
        );
    }

    let plan = bundle.plan;

    let errors = crate::validator::validate(&plan, &catalog);
    if !errors.is_empty() {
        let bullets: String = errors
            .iter()
            .map(|e| format!("\u{2022} {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!("the imported plan failed validation against the local catalog:\n{bullets}");
    }

    let resolution = engine::resolve_import_conflict(
        &state.paths,
        "mcp_client",
        crate::plan::normalization::normalize(plan),
        args.on_conflict,
    )?;
    Ok(json!({
        "outcome": resolution.outcome,
        "plan": resolution.plan,
        "same_name_plan_ids": resolution.same_name_plan_ids,
    }))
}

/// Resolve a caller-supplied export destination against the export root,
/// rejecting anything that could land outside it: absolute paths, `..`
/// traversal, and (on Windows) drive/prefix components. With only `Normal`
/// and `.` components left, the joined path stays under `root` lexically.
fn sanitized_export_path(
    root: &std::path::Path,
    requested: &std::path::Path,
) -> anyhow::Result<PathBuf> {
    if requested.as_os_str().is_empty() {
        anyhow::bail!("output_path must not be empty");
    }
    if requested.is_absolute() {
        anyhow::bail!(
            "output_path must be a relative path (exports are written under the app's \
             exports directory)"
        );
    }
    for component in requested.components() {
        match component {
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
            _ => anyhow::bail!("output_path must not contain '..' or root/prefix components"),
        }
    }
    Ok(root.join(requested))
}

/// Edit through the same detached-task path as [`compile_plan`]: plan edits
/// invoke the identical slow compiler pipeline and die the same way when the
/// client disconnects.
async fn edit_plan(state: &McpState, args: EditArgs) -> anyhow::Result<Value> {
    let key = format!(
        "{EDIT_PLAN_DEDUP_PREFIX}{}{DEDUP_FIELD_SEPARATOR}{}",
        args.plan_ref.trim(),
        args.instruction.trim()
    );
    let task_state = state.clone();
    await_detached(&state.compiles, "edit_plan", key, async move {
        edit_plan_detached(&task_state, args).await
    })
    .await
}

async fn edit_plan_detached(state: &McpState, args: EditArgs) -> anyhow::Result<Value> {
    let activity = state
        .activities
        .start(ActivityOrigin::Mcp, ActivityKind::Edit);
    let result = edit_plan_work(state, args, activity.id()).await;
    match &result {
        Ok(_) => activity.succeeded(),
        Err(error) => activity.failed(format!("{error:#}")),
    }
    result
}

async fn edit_plan_work(
    state: &McpState,
    args: EditArgs,
    activity_id: u64,
) -> anyhow::Result<Value> {
    let console = compile_console(state, "mcp edit");
    state
        .activities
        .attach_console(activity_id, console.clone());
    console.info(format!(
        "plan: {} — instruction: {}",
        args.plan_ref, args.instruction
    ));
    let storage = storage(state)?;
    let existing = engine::resolve_plan(&storage, &args.plan_ref)?;
    let catalog = catalog(state)?;
    let settings = AppSettings::load(&state.paths.settings_path);
    let backend = engine::create_configured_backend(&settings)?;
    let request =
        engine::edit_compile_request(&storage, &catalog, &settings, args.instruction, existing)?;
    let plan = crate::llm::with_cli_line_sink(
        std::sync::Arc::new(console.clone()),
        engine::compile_validate_normalize(
            &backend,
            request,
            &catalog,
            "MCP plan edit failed",
            Some(&console),
        ),
    )
    .await
    .inspect_err(|error| console.close(format!("✗ {error:#}")))?;
    console.close_after_persisting(
        || {
            state
                .paths
                .mutations
                .run_named("plan.edit_save", "mcp_client", || {
                    Ok(storage.plans().save(&plan)?)
                })
        },
        "edited, but saving the plan failed",
        || {
            format!(
                "✓ updated “{}” to v{} — {} steps, validated",
                plan.name,
                plan.metadata.version,
                plan.steps.len()
            )
        },
    )?;
    count_usage(state, crate::telemetry::usage::Action::PlanEdited);
    Ok(json!({ "plan": plan }))
}

async fn execute_plan(state: &McpState, args: ExecuteArgs) -> anyhow::Result<Value> {
    let storage = Arc::new(storage(state)?);
    let requested_plan = engine::resolve_plan(&storage, &args.plan_ref)?;
    let (plan, checkpoint, inputs) = match args.run_id.as_deref() {
        Some(run_id) => {
            let run = engine::load_run_by_prefix(&storage, run_id)?;
            if run.plan_id != requested_plan.metadata.id {
                anyhow::bail!(
                    "run '{}' belongs to plan '{}', not '{}'",
                    run.id,
                    run.plan_id,
                    requested_plan.metadata.id
                );
            }
            let plan = storage
                .plans()
                .load_version(&run.plan_id, run.plan_version)?;
            let inputs = args.inputs.clone().unwrap_or_else(|| run.inputs.clone());
            (plan, Some(run), inputs)
        }
        None => (
            requested_plan,
            None,
            args.inputs.clone().unwrap_or_default(),
        ),
    };
    // Explicit null / "" means "use the declared default".
    let inputs = engine::drop_inputs_deferring_to_defaults(&plan, inputs);

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();
    let (human_tx, mut human_rx) = tokio::sync::mpsc::unbounded_channel::<HumanRequest>();
    let settings = AppSettings::load(&state.paths.settings_path);
    engine::ensure_agent_call_allowed(&plan, &settings)?;
    let config = ExecutorConfig {
        inputs,
        timeout_secs: None,
        storage: storage.clone(),
        catalog: catalog(state)?,
        progress: Some(progress_tx),
        human: Some(human_tx),
        llm_keys: engine::llm_keys_from(&settings),
        source: Some(crate::storage::runs::RunSource::Mcp),
    };

    let mut progress = Vec::new();
    let mut elicitation = None;
    let mut response_error = None;
    let execution = async move {
        match checkpoint {
            Some(run) => executor::resume(plan, config, run).await,
            None => executor::execute(plan, config).await,
        }
    };
    tokio::pin!(execution);

    let run = loop {
        tokio::select! {
            result = &mut execution => break result?,
            Some(event) = progress_rx.recv() => progress.push(progress_json(&event)),
            Some(request) = human_rx.recv() => {
                if let Some(value) = args.human_responses.get(&request.step_id) {
                    match decision_for_value(&request, value) {
                        Ok(decision) => {
                            let _ = request.respond.send(decision);
                        }
                        Err(error) => {
                            response_error = Some(error);
                            elicitation = Some(elicitation_for_request(&request));
                            drop(request.respond);
                        }
                    }
                } else {
                    elicitation = Some(elicitation_for_request(&request));
                    drop(request.respond);
                }
            }
        }
    };

    if let Some(error) = response_error {
        return Err(error);
    }

    while let Ok(event) = progress_rx.try_recv() {
        progress.push(progress_json(&event));
    }

    if matches!(run.status, executor::RunStatus::WaitingForHuman { .. }) {
        return Ok(json!({
            "status": "elicitation_required",
            "message": "Provide an answer in execute_plan.human_responses keyed by step_id and call execute_plan again with this run_id.",
            "run_id": run.id,
            "run": run,
            "elicitation": elicitation.ok_or_else(|| anyhow::anyhow!(
                "executor paused without returning an elicitation"
            ))?,
            "progress": progress
        }));
    }

    count_run_outcome(state, &run.status, false);
    Ok(json!({ "run": run, "progress": progress }))
}

/// The wire shape of one progress event in `execute_plan`/`resume_run`
/// responses.
fn progress_json(event: &ProgressEvent) -> Value {
    json!({
        "run_id": event.run_id,
        "step_id": event.step_id,
        "status": event.status,
        "error": event.error,
        "iteration": event.iteration,
    })
}

fn elicitation_for_request(request: &HumanRequest) -> Value {
    json!({
        "step_id": request.step_id,
        "prompt": request.prompt,
        "approval_required": request.approval_required,
        "response_field": request.response_field,
        "schema": if request.approval_required {
            json!({"type":"boolean", "title":"Approve?"})
        } else {
            json!({"type":"string", "title":"Response"})
        }
    })
}

fn decision_for_value(request: &HumanRequest, value: &Value) -> anyhow::Result<HumanDecision> {
    if request.approval_required {
        return match value {
            Value::Bool(true) => Ok(HumanDecision::Approve),
            Value::Bool(false) => Ok(HumanDecision::Reject),
            Value::String(s)
                if matches!(
                    s.to_ascii_lowercase().as_str(),
                    "approve" | "approved" | "yes" | "y"
                ) =>
            {
                Ok(HumanDecision::Approve)
            }
            Value::String(s)
                if matches!(
                    s.to_ascii_lowercase().as_str(),
                    "reject" | "rejected" | "no" | "n"
                ) =>
            {
                Ok(HumanDecision::Reject)
            }
            Value::Object(map) => match map.get("decision").and_then(Value::as_str) {
                Some("approve") => Ok(HumanDecision::Approve),
                Some("reject") => Ok(HumanDecision::Reject),
                _ => anyhow::bail!(
                    "approval response for '{}' must approve or reject",
                    request.step_id
                ),
            },
            _ => anyhow::bail!(
                "approval response for '{}' must be a boolean or decision object",
                request.step_id
            ),
        };
    }

    match value {
        Value::String(text) => Ok(HumanDecision::Text(text.clone())),
        Value::Object(map) => {
            let text = map.get("text").and_then(Value::as_str).unwrap_or_default();
            Ok(HumanDecision::Text(text.to_owned()))
        }
        other => Ok(HumanDecision::Text(other.to_string())),
    }
}

fn list_runs(state: &McpState, args: ListArgs) -> anyhow::Result<Value> {
    let runs = engine::list_run_summaries(&storage(state)?)?;
    let total = runs.len();
    Ok(json!({
        "runs": args.paginate(runs, Some(DEFAULT_LIST_RUNS_LIMIT)),
        "total": total
    }))
}

fn inspect_run(state: &McpState, args: RunRefArgs) -> anyhow::Result<Value> {
    let storage = storage(state)?;
    let run = engine::load_run_by_prefix(&storage, &args.run_id)?;
    let plan = storage
        .plans()
        .load_version(&run.plan_id, run.plan_version)?;
    Ok(json!({ "run": run, "plan": plan }))
}

async fn repair_run(state: &McpState, args: RunRefArgs) -> anyhow::Result<Value> {
    let key = format!("repair_run{DEDUP_FIELD_SEPARATOR}{}", args.run_id.trim());
    let task_state = state.clone();
    await_detached(&state.compiles, "repair_run", key, async move {
        repair_run_detached(&task_state, args).await
    })
    .await
}

async fn repair_run_detached(state: &McpState, args: RunRefArgs) -> anyhow::Result<Value> {
    let activity = state
        .activities
        .start(ActivityOrigin::Mcp, ActivityKind::Repair);
    let console = compile_console(state, "mcp repair");
    state
        .activities
        .attach_console(activity.id(), console.clone());
    console.info(format!("repair requested for run {}", args.run_id));
    let result = repair_run_work(state, args, &console).await;
    match &result {
        Ok(_) => activity.succeeded(),
        Err(error) => activity.failed(format!("{error:#}")),
    }
    result
}

async fn repair_run_work(
    state: &McpState,
    args: RunRefArgs,
    console: &super::console::CompileConsole,
) -> anyhow::Result<Value> {
    let storage = storage(state)?;
    let run = engine::load_run_by_prefix(&storage, &args.run_id)?;
    if !run.status.is_failed() {
        anyhow::bail!("run '{}' has not failed (status: {})", run.id, run.status);
    }
    let plan = storage
        .plans()
        .load_version(&run.plan_id, run.plan_version)?;
    let catalog = catalog(state)?;
    let settings = AppSettings::load(&state.paths.settings_path);
    let backend = engine::create_configured_backend(&settings)?;
    console.info("analyzing failed run and proposing a repair");
    let proposal = repair::propose_repair(
        &run,
        &plan,
        &backend,
        &catalog,
        &storage,
        Some(EnvProbe::detect().compiler_context()),
    )
    .await?;
    Ok(match proposal {
        repair::RepairProposal::Patch(patch) => {
            console.info("repair patch proposed");
            json!({ "patch": patch })
        }
        repair::RepairProposal::WorldFix(fix) => json!({
            "world_fix": fix,
            "message": "The plan is fine — the runtime environment caused the failure. \
                        Perform the remediation actions, then call resume_run; the plan \
                        version stays unchanged.",
        }),
    })
}

/// Apply a pending repair patch, producing a new plan version.
///
/// Without this an agent could ask for a repair but never install it: applying
/// used to be reachable only from the desktop UI, which left the
/// `execute_plan → repair_run → resume_run` loop unclosable over MCP.
fn apply_patch(state: &McpState, args: PatchRefArgs) -> anyhow::Result<Value> {
    let plan = engine::apply_patch_in_storage(&state.paths, "mcp_client", args.patch_id.trim())?;
    Ok(json!({
        "plan": plan,
        "message": format!(
            "Patch applied — plan “{}” is now v{}. Call resume_run to retry the failed step.",
            plan.name, plan.metadata.version
        ),
    }))
}

/// Reject a pending repair patch, recording the reason for later readers.
fn reject_patch(state: &McpState, args: RejectPatchArgs) -> anyhow::Result<Value> {
    engine::reject_patch_in_storage(
        &state.paths,
        "mcp_client",
        args.patch_id.trim(),
        args.reason.clone(),
    )?;
    Ok(json!({
        "patch_id": args.patch_id,
        "status": "rejected",
        "reason": args.reason,
    }))
}

/// Re-execute a failed run against its plan's current version: only the
/// originally failed step and its true dependents run again. Mirrors
/// `execute_plan`'s elicitation handling so a HUMAN_INTERACTION step reached
/// while resuming surfaces the same `elicitation_required` shape.
async fn resume_run(state: &McpState, args: ResumeRunArgs) -> anyhow::Result<Value> {
    let storage = Arc::new(storage(state)?);
    let run = engine::load_run_by_prefix(&storage, &args.run_id)?;
    if !run.status.is_failed() {
        anyhow::bail!("run '{}' has not failed (status: {})", run.id, run.status);
    }
    // Deliberately `load_current`, not `load_version(run.plan_version)`:
    // resuming is meant to continue against whatever version a repair patch
    // most recently produced for this plan.
    let plan = storage.plans().load_current(&run.plan_id)?;
    let resume_mode = engine::repair_resume_mode(&storage, &run, &plan)?;
    let catalog = catalog(state)?;

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();
    let (human_tx, mut human_rx) = tokio::sync::mpsc::unbounded_channel::<HumanRequest>();
    let settings = AppSettings::load(&state.paths.settings_path);
    engine::ensure_agent_call_allowed(&plan, &settings)?;
    let config = ExecutorConfig {
        inputs: run.inputs.clone(),
        timeout_secs: None,
        storage: storage.clone(),
        catalog,
        progress: Some(progress_tx),
        human: Some(human_tx),
        llm_keys: engine::llm_keys_from(&settings),
        // Resume keeps the run's existing source.
        source: None,
    };

    let mut progress = Vec::new();
    let mut elicitation = None;
    let execution = executor::resume_from_repair(plan, config, run, args.inputs, resume_mode);
    tokio::pin!(execution);

    let run = loop {
        tokio::select! {
            result = &mut execution => break result?,
            Some(event) = progress_rx.recv() => progress.push(progress_json(&event)),
            Some(request) = human_rx.recv() => {
                elicitation = Some(elicitation_for_request(&request));
                drop(request.respond);
            }
        }
    };

    while let Ok(event) = progress_rx.try_recv() {
        progress.push(progress_json(&event));
    }

    if matches!(run.status, executor::RunStatus::WaitingForHuman { .. }) {
        return Ok(json!({
            "status": "elicitation_required",
            "message": "This run now needs a human answer; use execute_plan with this run_id and human_responses to continue it.",
            "run_id": run.id,
            "run": run,
            "elicitation": elicitation.ok_or_else(|| anyhow::anyhow!(
                "executor paused without returning an elicitation"
            ))?,
            "progress": progress
        }));
    }

    // `resume_run` only exists for repaired runs, so success here is healed.
    count_run_outcome(state, &run.status, true);
    Ok(json!({ "run": run, "progress": progress }))
}

fn list_patches(state: &McpState, args: ListArgs) -> anyhow::Result<Value> {
    let storage = storage(state)?;
    let patches = storage.patches().list()?;
    let total = patches.len();
    Ok(json!({ "patches": args.paginate(patches, None), "total": total }))
}

fn schedule_plan(state: &McpState, args: ScheduleArgs) -> anyhow::Result<Value> {
    let storage = storage(state)?;
    let plan = engine::resolve_plan(&storage, &args.plan_ref)?;
    let cron = schedule_store::normalize_cron(&args.cron).map_err(|e| anyhow::anyhow!(e))?;
    // Explicit null / "" means "use the declared default".
    let inputs = plan.resolve_inputs(&engine::drop_inputs_deferring_to_defaults(
        &plan,
        args.inputs,
    ))?;
    let schedule = schedule_store::Schedule {
        id: uuid::Uuid::new_v4().to_string(),
        plan_id: plan.metadata.id.clone(),
        cron: cron.clone(),
        enabled: true,
        inputs,
        created_at: chrono::Utc::now(),
        last_run: None,
    };
    state
        .paths
        .mutations
        .run_named("schedule.create", "mcp_client", || {
            let mut schedules = schedule_store::load(&state.paths.schedules_path)?;
            schedules.push(schedule.clone());
            schedule_store::save(&state.paths.schedules_path, &schedules)?;
            Ok(())
        })?;
    Ok(json!({
        "schedule": schedule,
        "next_run_display": schedule_store::next_occurrence(&cron, chrono::Local::now())
            .map(|t| t.format("%b %d %H:%M").to_string())
    }))
}

fn list_schedules(state: &McpState, args: ListArgs) -> anyhow::Result<Value> {
    let schedules = engine::list_schedule_summaries(&storage(state)?, &state.paths.schedules_path)?;
    let total = schedules.len();
    Ok(json!({ "schedules": args.paginate(schedules, None), "total": total }))
}

/// Remove a schedule by id. Mirrors the desktop engine's
/// `EngineCommand::DeleteSchedule`, which was never wired into the MCP
/// surface.
fn delete_schedule(state: &McpState, args: ScheduleRefArgs) -> anyhow::Result<Value> {
    state
        .paths
        .mutations
        .run_named("schedule.delete", "mcp_client", || {
            let schedules = schedule_store::load(&state.paths.schedules_path)?;
            let before = schedules.len();
            let remaining: Vec<schedule_store::Schedule> = schedules
                .into_iter()
                .filter(|s| s.id != args.schedule_id)
                .collect();
            // Unlike the desktop engine (which deletes from a live list and
            // can't miss), an API caller can hold a stale id — report it.
            if remaining.len() == before {
                anyhow::bail!("no schedule with id '{}'", args.schedule_id);
            }
            schedule_store::save(&state.paths.schedules_path, &remaining)?;
            Ok(())
        })?;
    Ok(json!({ "deleted": args.schedule_id }))
}

/// Enable or disable a schedule by id. The set-based shape (rather than the
/// desktop engine's toggle) keeps repeated agent calls idempotent.
fn set_schedule_enabled(state: &McpState, args: ScheduleEnableArgs) -> anyhow::Result<Value> {
    let updated = state
        .paths
        .mutations
        .run_named("schedule.toggle", "mcp_client", || {
            let mut schedules = schedule_store::load(&state.paths.schedules_path)?;
            let Some(schedule) = schedules.iter_mut().find(|s| s.id == args.schedule_id) else {
                anyhow::bail!("no schedule with id '{}'", args.schedule_id);
            };
            schedule.enabled = args.enabled;
            let updated = schedule.clone();
            schedule_store::save(&state.paths.schedules_path, &schedules)?;
            Ok(updated)
        })?;
    Ok(json!({ "schedule": updated }))
}

fn tools() -> Vec<Value> {
    vec![
        tool(
            "compile_plan",
            "Compile natural language into a validated, saved plan. Compilation keeps running \
             even if this call times out or disconnects — retry with the identical intent to \
             join the in-flight compile, or find the finished plan via list_plans.",
            json!({
                "type":"object", "properties": { "intent": {"type":"string"} }, "required":["intent"]
            }),
        ),
        tool(
            "list_plans",
            "Find/list stored plans",
            json!({"type":"object", "properties": {}}),
        ),
        tool(
            "show_plan",
            "Show a stored plan by id, id prefix, or exact name",
            json!({
                "type":"object", "properties": { "plan_ref": {"type":"string"} }, "required":["plan_ref"]
            }),
        ),
        tool(
            "export_plan",
            "Export a plan and its tool references as an importable plan bundle",
            json!({
                "type":"object",
                "properties": {
                    "plan_ref": {"type":"string"},
                    "output_path": {"type":"string", "description":"JSON destination path, relative to the app data dir's exports/ directory (no absolute paths or '..')"}
                },
                "required":["plan_ref", "output_path"]
            }),
        ),
        tool(
            "edit_plan",
            "Edit an existing plan using the configured compiler. Like compile_plan, the edit \
             keeps running if this call times out — retry with identical arguments to join it, \
             or check the plan version via show_plan.",
            json!({
                "type":"object", "properties": { "plan_ref": {"type":"string"}, "instruction": {"type":"string"} }, "required":["plan_ref", "instruction"]
            }),
        ),
        tool(
            "execute_plan",
            "Execute a plan. Human steps return elicitation_required unless human_responses supplies answers.",
            json!({
                "type":"object",
                "properties": {
                    "plan_ref": {"type":"string"},
                    "run_id": {"type":"string", "description":"Run id returned by elicitation_required; resumes without repeating completed steps"},
                    "inputs": {"type":"object", "description":"Values matching the plan's declared inputs", "additionalProperties": true},
                    "human_responses": {"type":"object", "additionalProperties": true}
                },
                "required":["plan_ref"],
                "additionalProperties": false
            }),
        ),
        tool(
            "list_runs",
            "List recent runs, newest first (at most `limit`, default 100; response includes `total`)",
            json!({"type":"object", "properties": {
                "limit": {"type":"integer", "minimum": 1, "description":"Maximum runs to return (default 100)"},
                "offset": {"type":"integer", "minimum": 0, "description":"Runs to skip, for paging"}
            }}),
        ),
        tool(
            "inspect_run",
            "Inspect a run with step status, timing, and outputs",
            json!({
                "type":"object", "properties": { "run_id": {"type":"string"} }, "required":["run_id"]
            }),
        ),
        tool(
            "repair_run",
            "Ask the compiler to propose a repair patch for a failed run",
            json!({
                "type":"object", "properties": { "run_id": {"type":"string"} }, "required":["run_id"]
            }),
        ),
        tool(
            "resume_run",
            "Re-run a failed run's failing step and everything downstream of it, against the plan's current version (normally called right after a repair patch is applied). inputs may correct failed/downstream values, but values used by completed steps are rejected.",
            json!({
                "type":"object", "properties": {
                    "run_id": {"type":"string"},
                    "inputs": {"type":"object", "description":"Optional replacement values matching current plan inputs", "additionalProperties": true}
                }, "required":["run_id"], "additionalProperties": false
            }),
        ),
        tool(
            "import_plan",
            "Import a plan bundle (as returned by export_plan) supplied inline as JSON. Tools the bundle needs but the local catalog lacks are refused, not auto-created — add them first.",
            json!({
                "type":"object", "properties": {
                "bundle": {"type":"object", "description":"A plan bundle object with format_version, plan, and tools"}
                    ,"on_conflict": {"type":"string", "enum":["reject", "new_version", "duplicate"], "default":"reject", "description":"Same-name behavior; reject is the default."}
                }, "required":["bundle"], "additionalProperties": false
            }),
        ),
        tool(
            "apply_patch",
            "Apply a pending repair patch, producing a new plan version. Follow with resume_run to retry the failed step.",
            json!({
                "type":"object", "properties": { "patch_id": {"type":"string"} }, "required":["patch_id"]
            }),
        ),
        tool(
            "reject_patch",
            "Reject a pending repair patch, optionally recording why",
            json!({
                "type":"object", "properties": {
                    "patch_id": {"type":"string"},
                    "reason": {"type":"string", "description":"Why the proposal was turned down"}
                }, "required":["patch_id"]
            }),
        ),
        tool(
            "list_patches",
            "List repair patches, newest first (all unless `limit` is given; response includes `total`)",
            json!({"type":"object", "properties": {
                "limit": {"type":"integer", "minimum": 1, "description":"Maximum patches to return"},
                "offset": {"type":"integer", "minimum": 0, "description":"Patches to skip, for paging"}
            }}),
        ),
        tool(
            "schedule_plan",
            "Schedule a plan with 5-, 6-, or 7-field cron syntax and captured inputs",
            json!({
                "type":"object", "properties": {
                    "plan_ref": {"type":"string"},
                    "cron": {"type":"string"},
                    "inputs": {"type":"object", "description":"Values matching the plan's declared inputs", "additionalProperties": true}
                }, "required":["plan_ref", "cron"]
            }),
        ),
        tool(
            "list_schedules",
            "List configured schedules by next run time (all unless `limit` is given; response includes `total`)",
            json!({"type":"object", "properties": {
                "limit": {"type":"integer", "minimum": 1, "description":"Maximum schedules to return"},
                "offset": {"type":"integer", "minimum": 0, "description":"Schedules to skip, for paging"}
            }}),
        ),
        tool(
            "delete_schedule",
            "Delete a schedule by id (see list_schedules)",
            json!({
                "type":"object", "properties": { "schedule_id": {"type":"string"} }, "required":["schedule_id"]
            }),
        ),
        tool(
            "set_schedule_enabled",
            "Enable or disable a schedule by id without deleting it",
            json!({
                "type":"object", "properties": {
                    "schedule_id": {"type":"string"},
                    "enabled": {"type":"boolean"}
                }, "required":["schedule_id", "enabled"]
            }),
        ),
    ]
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval_request() -> HumanRequest {
        request(true)
    }

    fn text_request() -> HumanRequest {
        request(false)
    }

    fn request(approval_required: bool) -> HumanRequest {
        let (respond, _rx) = tokio::sync::oneshot::channel();
        HumanRequest {
            step_id: "confirm".to_owned(),
            prompt: "Save it?".to_owned(),
            approval_required,
            response_field: "answer".to_owned(),
            respond,
        }
    }

    #[test]
    fn approval_accepts_booleans() {
        let request = approval_request();
        assert!(matches!(
            decision_for_value(&request, &json!(true)),
            Ok(HumanDecision::Approve)
        ));
        assert!(matches!(
            decision_for_value(&request, &json!(false)),
            Ok(HumanDecision::Reject)
        ));
    }

    #[test]
    fn approval_accepts_common_yes_no_strings_case_insensitively() {
        let request = approval_request();
        for yes in ["approve", "Approved", "YES", "y"] {
            assert!(
                matches!(
                    decision_for_value(&request, &json!(yes)),
                    Ok(HumanDecision::Approve)
                ),
                "'{yes}' should approve"
            );
        }
        for no in ["reject", "Rejected", "NO", "n"] {
            assert!(
                matches!(
                    decision_for_value(&request, &json!(no)),
                    Ok(HumanDecision::Reject)
                ),
                "'{no}' should reject"
            );
        }
    }

    #[test]
    fn approval_accepts_decision_objects_and_rejects_other_shapes() {
        let request = approval_request();
        assert!(matches!(
            decision_for_value(&request, &json!({"decision": "approve"})),
            Ok(HumanDecision::Approve)
        ));
        assert!(matches!(
            decision_for_value(&request, &json!({"decision": "reject"})),
            Ok(HumanDecision::Reject)
        ));
        assert!(decision_for_value(&request, &json!({"decision": "maybe"})).is_err());
        assert!(decision_for_value(&request, &json!("maybe")).is_err());
        assert!(decision_for_value(&request, &json!(42)).is_err());
    }

    #[test]
    fn text_prompts_take_strings_objects_and_stringified_values() {
        let request = text_request();
        assert!(matches!(
            decision_for_value(&request, &json!("blue")),
            Ok(HumanDecision::Text(text)) if text == "blue"
        ));
        assert!(matches!(
            decision_for_value(&request, &json!({"text": "blue"})),
            Ok(HumanDecision::Text(text)) if text == "blue"
        ));
        // Non-string scalars are stringified rather than rejected.
        assert!(matches!(
            decision_for_value(&request, &json!(42)),
            Ok(HumanDecision::Text(text)) if text == "42"
        ));
    }

    // ── JSON-RPC conformance ─────────────────────────────────────────────────

    fn test_state() -> (tempfile::TempDir, Arc<McpState>) {
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(McpState {
            paths: DataPaths::at(tmp.path().to_owned()),
            activities: ActivityRegistry::default(),
            compiles: Arc::default(),
        });
        (tmp, state)
    }

    /// Drive `handle_mcp` with a raw body and decode the JSON-RPC response.
    async fn rpc(state: Arc<McpState>, body: &str) -> (StatusCode, Value) {
        let response = handle_mcp(
            State(state),
            axum::body::Bytes::copy_from_slice(body.as_bytes()),
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    #[tokio::test]
    async fn malformed_json_returns_a_parse_error_object() {
        let (_tmp, state) = test_state();
        let (status, body) = rpc(state, "this is not json").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["error"]["code"], json!(PARSE_ERROR));
    }

    #[tokio::test]
    async fn batch_requests_are_rejected_with_a_clear_message() {
        let (_tmp, state) = test_state();
        let (status, body) = rpc(state, r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["error"]["code"], json!(INVALID_REQUEST));
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("batch requests are not supported"),
            "got: {body}"
        );
    }

    #[tokio::test]
    async fn invalid_envelope_returns_invalid_request_and_echoes_the_id() {
        let (_tmp, state) = test_state();
        // `method` missing entirely.
        let (status, body) = rpc(state, r#"{"jsonrpc":"2.0","id":3}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["error"]["code"], json!(INVALID_REQUEST));
        assert_eq!(body["id"], json!(3));
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let (_tmp, state) = test_state();
        let (status, body) = rpc(
            state,
            r#"{"jsonrpc":"2.0","id":1,"method":"no/such_method"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["error"]["code"], json!(METHOD_NOT_FOUND));
    }

    /// `import_plan` is advertised, and a malformed bundle is a tool-level
    /// failure (not a protocol error) so a caller can recover.
    #[tokio::test]
    async fn import_plan_is_advertised_and_rejects_a_malformed_bundle() {
        let (_tmp, state) = test_state();
        let (_, list) = rpc(
            state.clone(),
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
        )
        .await;
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"import_plan"), "missing from {names:?}");

        let (status, body) = rpc(
            state,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"import_plan","arguments":{"bundle":{"nope":1}}}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.get("error").is_none(), "not a protocol error: {body}");
        assert_eq!(body["result"]["isError"], json!(true), "got: {body}");
    }

    #[tokio::test]
    async fn unknown_tool_and_bad_arguments_are_invalid_params() {
        let (_tmp, state) = test_state();
        let (_, body) = rpc(
            state.clone(),
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"no_such_tool"}}"#,
        )
        .await;
        assert_eq!(body["error"]["code"], json!(INVALID_PARAMS), "got: {body}");

        let (_, body) = rpc(
            state,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_runs","arguments":{"limit":"many"}}}"#,
        )
        .await;
        assert_eq!(body["error"]["code"], json!(INVALID_PARAMS), "got: {body}");
    }

    /// JSON-RPC 2.0 §4.1: a request without `id` is a notification and must
    /// not be answered — and, critically, must not be *executed* and then
    /// answered. The defining property is the absent id, not the method name.
    #[tokio::test]
    async fn an_id_less_request_is_a_notification_whatever_its_method() {
        let (_tmp, state) = test_state();
        for body in [
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"list_plans","arguments":{}}}"#,
        ] {
            let (status, body_value) = rpc(state.clone(), body).await;
            assert_eq!(status, StatusCode::ACCEPTED, "for {body}");
            assert!(
                body_value.is_null(),
                "a notification must not be answered: {body_value}"
            );
        }
    }

    /// §5: every response carries `id`, and it is `null` when the request's id
    /// could not be determined. Omitting the member breaks strict clients.
    #[tokio::test]
    async fn every_response_echoes_jsonrpc_and_an_explicit_id() {
        let (_tmp, state) = test_state();

        let (_, malformed) = rpc(state.clone(), "{not json").await;
        assert_eq!(malformed["jsonrpc"], json!("2.0"));
        assert!(
            malformed.get("id").is_some() && malformed["id"].is_null(),
            "an undeterminable id must be explicit null: {malformed}"
        );

        let (_, ok) = rpc(
            state,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}}"#,
        )
        .await;
        assert_eq!(ok["jsonrpc"], json!("2.0"));
        assert_eq!(ok["id"], json!(7), "the caller's id must be echoed");
    }

    /// A plan whose single step calls the seeded `echo` tool, so
    /// `import_plan` is a real, observable side effect: it either lands a plan
    /// in storage or it does not.
    fn echo_plan() -> crate::plan::types::Plan {
        use crate::plan::types::{Plan, PlanMetadata, PlanStep, StepConfig, ToolCallConfig};
        let mut metadata = PlanMetadata::new(Some("Echo a fixed message".to_owned()));
        metadata.id = "id-semantics-plan".to_owned();
        Plan {
            metadata,
            name: "id-semantics-plan".to_owned(),
            description: None,
            inputs: vec![],
            config: Default::default(),
            steps: vec![PlanStep {
                id: "echo".to_owned(),
                name: "Echo".to_owned(),
                description: None,
                config: StepConfig::ToolCall(ToolCallConfig {
                    tool: "echo".to_owned(),
                    arguments: [("message".to_owned(), json!("hello"))]
                        .into_iter()
                        .collect(),
                }),
                depends_on: vec![],
                outputs: vec![],
                timeout_secs: None,
                retry: None,
            }],
            outputs: vec![],
        }
    }

    /// A `tools/call import_plan` body for [`echo_plan`], with `id_member`
    /// spliced in verbatim so the same call can be sent with an explicit null
    /// id, an absent id, or a normal one.
    fn import_echo_plan_body(state: &McpState, id_member: &str) -> String {
        let catalog = catalog(state).expect("seeded catalog");
        let (bundle, missing) = PlanBundle::from_plan(&echo_plan(), &catalog);
        assert!(
            missing.is_empty(),
            "echo must come from the default catalog"
        );
        let bundle = serde_json::to_value(&bundle).expect("bundle serialises");
        format!(
            r#"{{"jsonrpc":"2.0",{id_member}"method":"tools/call","params":{{"name":"import_plan","arguments":{{"bundle":{bundle}}}}}}}"#
        )
    }

    fn echo_bundle(state: &McpState) -> Value {
        let catalog = catalog(state).expect("seeded catalog");
        let (bundle, missing) = PlanBundle::from_plan(&echo_plan(), &catalog);
        assert!(
            missing.is_empty(),
            "echo must come from the default catalog"
        );
        serde_json::to_value(bundle).expect("bundle serialises")
    }

    #[test]
    fn import_conflicts_default_to_reject_and_report_the_local_ids() {
        let (_tmp, state) = test_state();
        let first = import_plan(
            &state,
            ImportPlanArgs {
                bundle: echo_bundle(&state),
                on_conflict: engine::ImportConflictPolicy::Reject,
            },
        )
        .expect("first import succeeds");
        let first_id = first["plan"]["metadata"]["id"]
            .as_str()
            .expect("plan id")
            .to_owned();

        let rejected = import_plan(
            &state,
            ImportPlanArgs {
                bundle: echo_bundle(&state),
                on_conflict: Default::default(),
            },
        )
        .expect("collision is a result, not a write failure");
        assert_eq!(rejected["outcome"], json!("rejected"));
        assert_eq!(rejected["same_name_plan_ids"], json!([first_id]));
        assert_eq!(storage(&state).unwrap().plans().list().unwrap().len(), 1);
    }

    #[test]
    fn import_duplicate_requires_explicit_policy() {
        let (_tmp, state) = test_state();
        import_plan(
            &state,
            ImportPlanArgs {
                bundle: echo_bundle(&state),
                on_conflict: Default::default(),
            },
        )
        .unwrap();
        let duplicate = import_plan(
            &state,
            ImportPlanArgs {
                bundle: echo_bundle(&state),
                on_conflict: engine::ImportConflictPolicy::Duplicate,
            },
        )
        .unwrap();
        assert_eq!(duplicate["outcome"], json!("duplicate"));
        assert_eq!(storage(&state).unwrap().plans().list().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn import_schema_exposes_an_explicit_conflict_policy() {
        let (_tmp, state) = test_state();
        let (_, body) = rpc(
            state,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
        )
        .await;
        let import = body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "import_plan")
            .unwrap();
        assert_eq!(
            import["inputSchema"]["properties"]["on_conflict"]["default"],
            json!("reject")
        );
        assert_eq!(
            import["inputSchema"]["properties"]["on_conflict"]["enum"],
            json!(["reject", "new_version", "duplicate"])
        );
    }

    async fn stored_plan_names(state: Arc<McpState>) -> Vec<String> {
        let (_, body) = rpc(
            state,
            r#"{"jsonrpc":"2.0","id":99,"method":"tools/call","params":{"name":"list_plans","arguments":{}}}"#,
        )
        .await;
        let text = body["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let listed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        listed["plans"]
            .as_array()
            .map(|plans| {
                plans
                    .iter()
                    .filter_map(|p| p["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// §4: an `id` of null is discouraged but legal, and §5 then requires an
    /// ordinary response carrying `"id": null`. Treating it as a notification
    /// (the double-`Option` bug this guards) would drop the call on the floor:
    /// 202, nothing executed, caller blocked on a response that never arrives.
    #[tokio::test]
    async fn an_explicit_null_id_is_a_request_and_is_answered_with_a_null_id() {
        let (_tmp, state) = test_state();
        let body_text = import_echo_plan_body(&state, r#""id":null,"#);
        let (status, body) = rpc(state.clone(), &body_text).await;

        assert_eq!(status, StatusCode::OK, "got: {body}");
        assert_eq!(body["jsonrpc"], json!("2.0"));
        assert!(
            body.get("id").is_some() && body["id"].is_null(),
            "a null id must be echoed as an explicit null: {body}"
        );
        assert!(body.get("error").is_none(), "not a protocol error: {body}");
        assert_eq!(body["result"]["isError"], json!(false), "got: {body}");
        assert_eq!(
            stored_plan_names(state).await,
            vec!["id-semantics-plan".to_owned()],
            "the method must actually have executed"
        );
    }

    /// §4.1: only a genuinely *absent* `id` makes a request a notification —
    /// no response, and the method must not run at all.
    #[tokio::test]
    async fn an_absent_id_is_a_notification_whose_method_never_executes() {
        let (_tmp, state) = test_state();
        let body_text = import_echo_plan_body(&state, "");
        let (status, body) = rpc(state.clone(), &body_text).await;

        assert_eq!(status, StatusCode::ACCEPTED, "got: {body}");
        assert!(
            body.is_null(),
            "a notification must not be answered: {body}"
        );
        assert!(
            stored_plan_names(state).await.is_empty(),
            "a notification must not have executed the tool"
        );
    }

    /// The error paths keep their id behaviour: an undeterminable id stays
    /// `null`, and a null id on a failing-but-parsable request is echoed as
    /// null rather than turning the call into a notification.
    #[tokio::test]
    async fn error_responses_keep_their_id_behaviour_around_null_ids() {
        let (_tmp, state) = test_state();

        // Parse error — no id to determine.
        let (status, body) = rpc(state.clone(), "{\"jsonrpc\":").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["error"]["code"], json!(PARSE_ERROR), "got: {body}");
        assert!(body.get("id").is_some() && body["id"].is_null(), "{body}");

        // Invalid params, explicit null id: answered, code preserved, id null.
        let (status, body) = rpc(
            state.clone(),
            r#"{"jsonrpc":"2.0","id":null,"method":"tools/call","params":{"name":"no_such_tool"}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["error"]["code"], json!(INVALID_PARAMS), "got: {body}");
        assert!(body.get("id").is_some() && body["id"].is_null(), "{body}");

        // Invalid envelope (no `method`) with a null id: still an error echo.
        let (status, body) = rpc(state, r#"{"jsonrpc":"2.0","id":null}"#).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["error"]["code"], json!(INVALID_REQUEST), "got: {body}");
        assert!(body.get("id").is_some() && body["id"].is_null(), "{body}");
    }

    /// Applying and rejecting a repair patch must be reachable
    /// over MCP, so an agent can close the
    /// `execute_plan → repair_run → apply_patch → resume_run` loop without
    /// the desktop UI.
    #[tokio::test]
    async fn patch_verbs_are_advertised_over_mcp() {
        let (_tmp, state) = test_state();
        let (_, body) = rpc(
            state,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#,
        )
        .await;
        let names: Vec<&str> = body["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        for expected in ["apply_patch", "reject_patch"] {
            assert!(
                names.contains(&expected),
                "{expected} missing from {names:?}"
            );
        }
    }

    /// An unknown patch id is a tool-level failure (`isError`), not a
    /// protocol error — same shape as every other id-driven tool.
    #[tokio::test]
    async fn applying_an_unknown_patch_reports_a_tool_error() {
        let (_tmp, state) = test_state();
        let (status, body) = rpc(
            state,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"apply_patch","arguments":{"patch_id":"missing"}}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.get("error").is_none(), "not a protocol error: {body}");
        assert_eq!(body["result"]["isError"], json!(true), "got: {body}");
    }

    /// `patch_id` is required; omitting it is an argument-shape failure.
    #[tokio::test]
    async fn patch_verbs_require_a_patch_id() {
        let (_tmp, state) = test_state();
        let (_, body) = rpc(
            state,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"reject_patch","arguments":{"reason":"no id given"}}}"#,
        )
        .await;
        assert_eq!(body["error"]["code"], json!(INVALID_PARAMS), "got: {body}");
    }

    #[tokio::test]
    async fn tool_execution_failure_is_a_result_with_is_error() {
        let (_tmp, state) = test_state();
        let (status, body) = rpc(
            state,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"show_plan","arguments":{"plan_ref":"does-not-exist"}}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.get("error").is_none(),
            "must not be a protocol error: {body}"
        );
        assert_eq!(body["result"]["isError"], json!(true));
        assert!(
            body["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("does-not-exist"),
            "got: {body}"
        );
    }

    #[tokio::test]
    async fn list_tools_honor_limit_and_offset() {
        let (_tmp, state) = test_state();
        // Three schedules, listed with limit 1 / offset 1.
        let schedules: Vec<schedule_store::Schedule> = (0..3)
            .map(|i| schedule_store::Schedule {
                id: format!("s{i}"),
                plan_id: "p1".to_owned(),
                cron: "0 0 8 * * *".to_owned(),
                enabled: true,
                inputs: IndexMap::new(),
                created_at: chrono::Utc::now(),
                last_run: None,
            })
            .collect();
        schedule_store::save(&state.paths.schedules_path, &schedules).unwrap();

        let value = list_schedules(
            &state,
            ListArgs {
                limit: Some(1),
                offset: Some(1),
            },
        )
        .unwrap();
        assert_eq!(value["schedules"].as_array().unwrap().len(), 1);
        assert_eq!(value["total"], json!(3));
    }

    // ── Schedule management tools ────────────────────────────────────────────

    #[test]
    fn schedule_can_be_disabled_and_deleted_by_id() {
        let (_tmp, state) = test_state();
        let schedule = schedule_store::Schedule {
            id: "s1".to_owned(),
            plan_id: "p1".to_owned(),
            cron: "0 0 8 * * *".to_owned(),
            enabled: true,
            inputs: IndexMap::new(),
            created_at: chrono::Utc::now(),
            last_run: None,
        };
        schedule_store::save(&state.paths.schedules_path, &[schedule]).unwrap();

        let value = set_schedule_enabled(
            &state,
            ScheduleEnableArgs {
                schedule_id: "s1".to_owned(),
                enabled: false,
            },
        )
        .unwrap();
        assert_eq!(value["schedule"]["enabled"], json!(false));
        assert!(
            set_schedule_enabled(
                &state,
                ScheduleEnableArgs {
                    schedule_id: "nope".to_owned(),
                    enabled: true,
                },
            )
            .is_err()
        );

        delete_schedule(
            &state,
            ScheduleRefArgs {
                schedule_id: "s1".to_owned(),
            },
        )
        .unwrap();
        assert!(
            schedule_store::load(&state.paths.schedules_path)
                .unwrap()
                .is_empty()
        );
        // Stale ids are reported instead of silently succeeding.
        assert!(
            delete_schedule(
                &state,
                ScheduleRefArgs {
                    schedule_id: "s1".to_owned(),
                },
            )
            .is_err()
        );
    }

    // ── Loopback Host/Origin guard ───────────────────────────────────────────

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(name.clone(), value.parse().unwrap());
        }
        map
    }

    #[test]
    fn loopback_hosts_are_accepted() {
        for value in [
            "127.0.0.1:39387",
            "127.0.0.1",
            "localhost:39387",
            "localhost",
            "LOCALHOST:1234",
            "[::1]:39387",
            "[::1]",
        ] {
            assert!(host_is_loopback(value), "'{value}' should be loopback");
        }
    }

    #[test]
    fn foreign_and_lookalike_hosts_are_rejected() {
        for value in [
            "evil.example",
            "evil.example:39387",
            "127.0.0.1.evil.example",
            "localhost.evil.example",
            "[::1",
            "",
        ] {
            assert!(!host_is_loopback(value), "'{value}' must be rejected");
        }
    }

    #[test]
    fn rebound_host_header_is_rejected_even_with_absent_origin() {
        assert!(!headers_are_loopback(&headers(&[(
            header::HOST,
            "evil.example:39387"
        )])));
    }

    #[test]
    fn non_loopback_and_null_origins_are_rejected() {
        for origin in ["https://evil.example", "null", "file://x"] {
            assert!(
                !headers_are_loopback(&headers(&[
                    (header::HOST, "127.0.0.1:39387"),
                    (header::ORIGIN, origin),
                ])),
                "origin '{origin}' must be rejected"
            );
        }
    }

    #[test]
    fn local_clients_and_loopback_origins_pass_the_guard() {
        // Non-browser MCP client: Host only.
        assert!(headers_are_loopback(&headers(&[(
            header::HOST,
            "127.0.0.1:39387"
        )])));
        // No headers at all (e.g. HTTP/1.0 tooling).
        assert!(headers_are_loopback(&HeaderMap::new()));
        // Browser on a genuinely local page.
        assert!(headers_are_loopback(&headers(&[
            (header::HOST, "localhost:39387"),
            (header::ORIGIN, "http://localhost:39387"),
        ])));
    }

    // ── Export path confinement ─────────────────────────────────────────────

    #[test]
    fn relative_export_paths_resolve_under_the_export_root() {
        let root = std::path::Path::new("/data/exports");
        assert_eq!(
            sanitized_export_path(root, std::path::Path::new("bundle.json")).unwrap(),
            root.join("bundle.json")
        );
        assert_eq!(
            sanitized_export_path(root, std::path::Path::new("nested/dir/bundle.json")).unwrap(),
            root.join("nested/dir/bundle.json")
        );
    }

    #[test]
    fn escaping_export_paths_are_rejected() {
        let root = std::path::Path::new("/data/exports");
        for path in ["/etc/passwd", "../outside.json", "a/../../outside.json", ""] {
            assert!(
                sanitized_export_path(root, std::path::Path::new(path)).is_err(),
                "'{path}' must be rejected"
            );
        }
    }

    /// The compile must survive the axum handler being cancelled
    /// on client disconnect — dropping the awaiting future must not drop the
    /// detached work.
    #[tokio::test]
    async fn detached_compile_outlives_a_dropped_client_request() {
        let registry: InFlightCompiles = Arc::default();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let waiter = await_detached(&registry, "compile_plan", "key".to_owned(), async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = done_tx.send(());
            Ok(json!({ "plan": "compiled" }))
        });
        // Simulate the client timing out: poll long enough to spawn the
        // detached task, then drop the request future.
        let timed_out = tokio::time::timeout(std::time::Duration::from_millis(5), waiter).await;
        assert!(timed_out.is_err(), "the simulated client should time out");

        done_rx
            .await
            .expect("detached work should run to completion after the caller dropped");
        // The registry entry is cleared once the outcome is published.
        for _ in 0..50 {
            if registry.lock().unwrap().is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("finished compile should be removed from the in-flight registry");
    }

    /// An identical retry while a compile is in flight joins the
    /// running work instead of forking a second compiler subprocess.
    #[tokio::test]
    async fn identical_in_flight_compiles_share_one_execution() {
        let registry: InFlightCompiles = Arc::default();
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let work = |executions: Arc<std::sync::atomic::AtomicUsize>| async move {
            executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(json!({ "plan": "compiled" }))
        };
        // `join!` on the current-thread test runtime polls the first future
        // (which registers the key synchronously) before the second, so the
        // second deterministically observes the in-flight entry.
        let (first, second) = tokio::join!(
            await_detached(
                &registry,
                "compile_plan",
                "key".to_owned(),
                work(executions.clone())
            ),
            await_detached(
                &registry,
                "compile_plan",
                "key".to_owned(),
                work(executions.clone())
            )
        );
        assert_eq!(first.unwrap(), json!({ "plan": "compiled" }));
        assert_eq!(second.unwrap(), json!({ "plan": "compiled" }));
        assert_eq!(
            executions.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the duplicate request must join the in-flight compile"
        );
    }

    /// A failed detached compile publishes its error to every waiter and
    /// clears the registry so the next attempt starts fresh.
    #[tokio::test]
    async fn failed_detached_compiles_report_the_error_and_clear_the_registry() {
        let registry: InFlightCompiles = Arc::default();
        let result = await_detached(&registry, "compile_plan", "key".to_owned(), async {
            anyhow::bail!("backend exploded")
        })
        .await;
        assert!(result.unwrap_err().to_string().contains("backend exploded"));
        for _ in 0..50 {
            if registry.lock().unwrap().is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("failed compile should be removed from the in-flight registry");
    }

    #[test]
    fn running_status_reports_a_fallback_port_when_used() {
        let status = ServerStatus::Running {
            port: 40001,
            fallback_from: Some(39387),
        };
        assert_eq!(
            status.label(),
            "MCP 127.0.0.1:40001 (configured port 39387 was unavailable)"
        );
        let normal = ServerStatus::Running {
            port: 39387,
            fallback_from: None,
        };
        assert_eq!(normal.label(), "MCP 127.0.0.1:39387");
    }

    #[test]
    fn resume_tool_accepts_optional_input_overrides() {
        let resume = tools()
            .into_iter()
            .find(|tool| tool["name"] == "resume_run")
            .expect("resume_run is advertised");
        let schema = &resume["inputSchema"];
        assert_eq!(schema["required"], json!(["run_id"]));
        assert_eq!(schema["properties"]["inputs"]["type"], json!("object"));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn resume_args_default_to_an_empty_override_map() {
        let args: ResumeRunArgs = serde_json::from_value(json!({ "run_id": "run-1" })).unwrap();
        assert_eq!(args.run_id, "run-1");
        assert!(args.inputs.is_empty());
    }

    /// A client retry that arrives while the original detached compile is
    /// still running must join it rather than starting a second compile —
    /// the bug a retry-storm hits without this (see PR #61 review). Ported
    /// from main's `run_deduped_compile` onto `await_detached`, which keeps
    /// the stronger assertion: the retry's own work returns a *different*
    /// value, so joining is proven by the value, not only by the counter.
    #[tokio::test]
    async fn dedup_joins_a_concurrent_identical_request_instead_of_recompiling() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let registry: InFlightCompiles = Arc::default();
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let key = "dup-key".to_owned();

        let first = {
            let registry = registry.clone();
            let key = key.clone();
            let started = started.clone();
            let release = release.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                await_detached(&registry, "test_tool", key, async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    started.notify_one();
                    release.notified().await;
                    anyhow::Ok(json!({"v": 1}))
                })
                .await
            })
        };

        // Wait for the first compile to actually start (so its key is
        // registered) before firing the "retry".
        started.notified().await;

        let second = {
            let registry = registry.clone();
            let key = key.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                await_detached(&registry, "test_tool", key, async move {
                    // Must never run: the retry joins the first compile, and
                    // an unpolled future never executes its body.
                    calls.fetch_add(1, Ordering::SeqCst);
                    anyhow::Ok(json!({"v": 2}))
                })
                .await
            })
        };

        // This test runs on the current-thread flavor, so yielding lets the
        // just-spawned retry run up to the point where it subscribes to the
        // in-flight entry, deterministically, before we let the first
        // compile finish.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        release.notify_one();

        let first_value = first.await.unwrap().unwrap();
        let second_value = second.await.unwrap().unwrap();

        assert_eq!(first_value, json!({"v": 1}));
        assert_eq!(second_value, json!({"v": 1}));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        for _ in 0..50 {
            if lock_registry(&registry).is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("completed compile must be removed from the in-flight registry");
    }

    /// A distinct request (different key) is never blocked or merged by an
    /// unrelated in-flight compile.
    #[tokio::test]
    async fn distinct_keys_run_independently() {
        let registry: InFlightCompiles = Arc::default();
        let (a, b) = tokio::join!(
            await_detached(&registry, "test_tool", "a".to_owned(), async {
                anyhow::Ok(json!({"v": "a"}))
            }),
            await_detached(&registry, "test_tool", "b".to_owned(), async {
                anyhow::Ok(json!({"v": "b"}))
            }),
        );
        assert_eq!(a.unwrap(), json!({"v": "a"}));
        assert_eq!(b.unwrap(), json!({"v": "b"}));
    }
}
