//! Official RMCP Streamable HTTP client adapter.

use crate::error::ToolError;
use crate::tools::ToolOutput;
use crate::tools::catalog::{McpAuth, McpDiscoveredTool};
use crate::tools::oauth::McpOAuthFacade;
use indexmap::IndexMap;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{ClientInitializeError, ClientLifecycleMode, ClientServiceExt, ServiceError};
use rmcp::transport::DynamicTransportError;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, InsufficientScopeError, StreamableHttpClientTransportConfig,
};
use std::error::Error;
use std::time::Duration;

const MAX_MCP_CONTENT_BYTES: usize = 524_288;
const OUTPUT_LIMIT_MARKER: &str = "[truncated: MCP output limit exceeded]";
const RECONNECT_REMEDIATION: &str = "Reconnect under MCP Tools";
/// Label used in place of a tool name for errors raised while discovering a
/// server's tools rather than calling one of them.
const LIST_TOOLS_LABEL: &str = "tools/list";

pub async fn run(
    endpoint: &str,
    auth: &McpAuth,
    tool_name: &str,
    arguments: &IndexMap<String, serde_json::Value>,
    timeout_secs: u64,
) -> Result<ToolOutput, ToolError> {
    let execution = async {
        match auth {
            McpAuth::None => call_once(endpoint, None, tool_name, arguments).await,
            McpAuth::OAuth { client_id } => {
                let oauth = McpOAuthFacade::production(endpoint, client_id.clone())
                    .await
                    .map_err(|_| RemoteMcpError::AuthorizationRequired)?;
                call_tool_via_oauth(&oauth, endpoint, tool_name, arguments).await
            }
        }
    };

    match tokio::time::timeout(Duration::from_secs(timeout_secs), execution).await {
        Err(_) => Err(ToolError::timeout(tool_name.to_owned(), timeout_secs)),
        Ok(Ok(output)) => Ok(output),
        Ok(Err(RemoteMcpError::AuthorizationRequired)) => Err(ToolError::Execution {
            tool: tool_name.to_owned(),
            message: format!("authorization required; {RECONNECT_REMEDIATION}"),
        }),
        Ok(Err(RemoteMcpError::InsufficientScope)) => Err(ToolError::Execution {
            tool: tool_name.to_owned(),
            message: format!("OAuth token has insufficient scope; {RECONNECT_REMEDIATION}"),
        }),
        Ok(Err(RemoteMcpError::OutputLimit)) => Err(ToolError::Execution {
            tool: tool_name.to_owned(),
            message: format!("{OUTPUT_LIMIT_MARKER}; maximum is {MAX_MCP_CONTENT_BYTES} bytes"),
        }),
        Ok(Err(RemoteMcpError::Protocol)) => Err(ToolError::Execution {
            tool: tool_name.to_owned(),
            message: "remote MCP protocol request failed".to_owned(),
        }),
    }
}

/// Connect to a remote Streamable HTTP MCP server and enumerate the tools it
/// currently advertises (`tools/list`), for bulk import into the catalog.
pub async fn list_tools(
    endpoint: &str,
    auth: &McpAuth,
    timeout_secs: u64,
) -> Result<Vec<McpDiscoveredTool>, ToolError> {
    let execution = async {
        match auth {
            McpAuth::None => list_once(endpoint, None).await,
            McpAuth::OAuth { client_id } => {
                let oauth = McpOAuthFacade::production(endpoint, client_id.clone())
                    .await
                    .map_err(|_| RemoteMcpError::AuthorizationRequired)?;
                list_tools_via_oauth(&oauth, endpoint).await
            }
        }
    };

    match tokio::time::timeout(Duration::from_secs(timeout_secs), execution).await {
        Err(_) => Err(ToolError::timeout(LIST_TOOLS_LABEL, timeout_secs)),
        Ok(Ok(tools)) => Ok(tools),
        Ok(Err(RemoteMcpError::AuthorizationRequired)) => Err(ToolError::Execution {
            tool: LIST_TOOLS_LABEL.to_owned(),
            message: format!("authorization required; {RECONNECT_REMEDIATION}"),
        }),
        Ok(Err(RemoteMcpError::InsufficientScope)) => Err(ToolError::Execution {
            tool: LIST_TOOLS_LABEL.to_owned(),
            message: format!("OAuth token has insufficient scope; {RECONNECT_REMEDIATION}"),
        }),
        Ok(Err(RemoteMcpError::OutputLimit | RemoteMcpError::Protocol)) => {
            Err(ToolError::Execution {
                tool: LIST_TOOLS_LABEL.to_owned(),
                message: "remote MCP protocol request failed".to_owned(),
            })
        }
    }
}

/// Get a token from `oauth`, try the call, and on `AuthorizationRequired`
/// refresh once and retry — the logic `run`/`list_tools` need for the OAuth
/// case. Factored out (rather than inlined in `run`) so tests can exercise
/// it against an `McpOAuthFacade::with_boundaries` facade — real credential
/// store, real HTTP calls to the resource server, without touching the OS
/// keyring `McpOAuthFacade::production` requires.
async fn call_tool_via_oauth(
    oauth: &McpOAuthFacade,
    endpoint: &str,
    tool_name: &str,
    arguments: &IndexMap<String, serde_json::Value>,
) -> Result<ToolOutput, RemoteMcpError> {
    let first_token = oauth
        .access_token()
        .await
        .map_err(|_| RemoteMcpError::AuthorizationRequired)?;
    match call_once(endpoint, Some(first_token), tool_name, arguments).await {
        Err(RemoteMcpError::AuthorizationRequired) => {
            tracing::Span::current().record("tool.retry_count", 1_u64);
            let refreshed = oauth
                .refresh_after_unauthorized()
                .await
                .map_err(|_| RemoteMcpError::AuthorizationRequired)?;
            call_once(endpoint, Some(refreshed), tool_name, arguments).await
        }
        result => result,
    }
}

/// Same retry shape as [`call_tool_via_oauth`], for `tools/list`.
async fn list_tools_via_oauth(
    oauth: &McpOAuthFacade,
    endpoint: &str,
) -> Result<Vec<McpDiscoveredTool>, RemoteMcpError> {
    let first_token = oauth
        .access_token()
        .await
        .map_err(|_| RemoteMcpError::AuthorizationRequired)?;
    match list_once(endpoint, Some(first_token)).await {
        Err(RemoteMcpError::AuthorizationRequired) => {
            let refreshed = oauth
                .refresh_after_unauthorized()
                .await
                .map_err(|_| RemoteMcpError::AuthorizationRequired)?;
            list_once(endpoint, Some(refreshed)).await
        }
        result => result,
    }
}

async fn list_once(
    endpoint: &str,
    access_token: Option<String>,
) -> Result<Vec<McpDiscoveredTool>, RemoteMcpError> {
    crate::tools::ensure_tls_crypto_provider_installed();
    let mut config = StreamableHttpClientTransportConfig::with_uri(endpoint.to_owned());
    if let Some(access_token) = access_token {
        config = config.auth_header(access_token);
    }
    let transport = StreamableHttpClientTransport::from_config(config);
    let client =
        ().serve_with_lifecycle(transport, client_lifecycle())
            .await
            .map_err(|error| classify_error(&error))?;
    let tools = client
        .list_all_tools()
        .await
        .map_err(|error| classify_error(&error));
    let _ = client.cancel().await;
    Ok(tools?
        .into_iter()
        .map(|tool| McpDiscoveredTool {
            name: tool.name.into_owned(),
            description: tool
                .description
                .map(std::borrow::Cow::into_owned)
                .unwrap_or_default(),
            input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
        })
        .collect())
}

/// The lifecycle handshake to use when connecting to a remote server.
///
/// This intentionally skips `ClientLifecycleMode::Auto`/`Discover`, which
/// probe with the `server/discover` method (an rmcp-specific extension for
/// an upcoming, not-yet-ratified protocol draft) before ever attempting the
/// classic `initialize` handshake every real-world MCP server actually
/// implements. Against Granola's MCP server that probing didn't degrade
/// gracefully: sending a candidate version it doesn't recognize (rmcp's
/// newest known version, ahead of even `ProtocolVersion::LATEST`) came back
/// as a generic JSON-RPC error rather than the standard
/// `UNSUPPORTED_PROTOCOL_VERSION` code `Auto`'s retry logic looks for, and
/// retrying with a version Granola explicitly does support (`LATEST`) then
/// closed the connection outright instead of answering — `server/discover`
/// itself isn't implemented there, just enough of a version check in front
/// of it to reject what it doesn't recognize. Since `server/discover`
/// currently buys nothing against real deployed servers, use the plain
/// legacy handshake unconditionally rather than depend on newer servers
/// implementing the probe/fallback contract correctly.
fn client_lifecycle() -> ClientLifecycleMode {
    ClientLifecycleMode::Initialize
}

#[derive(Debug)]
enum RemoteMcpError {
    AuthorizationRequired,
    InsufficientScope,
    OutputLimit,
    Protocol,
}

async fn call_once(
    endpoint: &str,
    access_token: Option<String>,
    tool_name: &str,
    arguments: &IndexMap<String, serde_json::Value>,
) -> Result<ToolOutput, RemoteMcpError> {
    crate::tools::ensure_tls_crypto_provider_installed();
    let mut config = StreamableHttpClientTransportConfig::with_uri(endpoint.to_owned());
    if let Some(access_token) = access_token {
        config = config.auth_header(access_token);
    }
    let transport = StreamableHttpClientTransport::from_config(config);
    let client =
        ().serve_with_lifecycle(transport, client_lifecycle())
            .await
            .map_err(|error| classify_error(&error))?;
    let object: serde_json::Map<String, serde_json::Value> = arguments
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let response = client
        .call_tool(CallToolRequestParams::new(tool_name.to_owned()).with_arguments(object))
        .await
        .map_err(|error| classify_error(&error));
    let _ = client.cancel().await;
    normalize_result(response?)
}

/// Classifies a failure from `serve_with_lifecycle`/`call_tool`/`list_all_tools`.
///
/// `AuthRequiredError`/`InsufficientScopeError` never surface through the
/// ordinary `Error::source()` chain from these entry points: rmcp 3.1.2's
/// `ClientInitializeError::TransportError { error, .. }` and
/// `ServiceError::TransportSend(error)` variants both carry the transport
/// failure as a `DynamicTransportError` field *without* a `#[source]`/`#[from]`
/// attribute, so their own `.source()` returns `None` — walking `.source()`
/// from either enum stops immediately and never reaches the
/// `DynamicTransportError`, let alone the `AuthRequiredError`/
/// `InsufficientScopeError` nested another level inside it. That left every
/// 401/403 — even the textbook case with a `WWW-Authenticate` header —
/// falling through to the generic `Protocol` bucket below, which is why a
/// stale/rejected token never triggered the refresh-and-retry path in `run`/
/// `list_tools` above: it looked identical to any other protocol failure.
///
/// `DynamicTransportError`'s own `error` field (the actual per-transport
/// error) *is* public, so we reach into it directly instead of relying on
/// `.source()` to bridge the gap.
fn classify_error(error: &(dyn Error + 'static)) -> RemoteMcpError {
    if let Some(dynamic) = dynamic_transport_error(error) {
        return classify_dynamic_transport_error(dynamic);
    }
    // Fallback for anything not shaped like the two cases above (e.g. a
    // future rmcp release that threads these through `.source()` directly).
    let mut source = Some(error);
    while let Some(current) = source {
        if current.is::<AuthRequiredError>() {
            return RemoteMcpError::AuthorizationRequired;
        }
        if current.is::<InsufficientScopeError>() {
            return RemoteMcpError::InsufficientScope;
        }
        source = current.source();
    }
    tracing::warn!(
        error = %error,
        chain = %error_chain(error),
        "remote MCP request failed with an unclassified error; treating as a generic protocol failure"
    );
    RemoteMcpError::Protocol
}

/// Renders `error` and every `.source()` beneath it as a single `" -> "`
/// separated string, so a failure that would otherwise collapse into the
/// generic `Protocol` bucket still leaves a diagnosable trail in the logs
/// (enable with e.g. `RUST_LOG=inxm_local=warn`).
fn error_chain(error: &(dyn Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(current) = source {
        parts.push(current.to_string());
        source = current.source();
    }
    parts.join(" -> ")
}

fn dynamic_transport_error<'a>(
    error: &'a (dyn Error + 'static),
) -> Option<&'a DynamicTransportError> {
    if let Some(ClientInitializeError::TransportError { error, .. }) =
        error.downcast_ref::<ClientInitializeError>()
    {
        return Some(error);
    }
    if let Some(ServiceError::TransportSend(error)) = error.downcast_ref::<ServiceError>() {
        return Some(error);
    }
    None
}

fn classify_dynamic_transport_error(dynamic: &DynamicTransportError) -> RemoteMcpError {
    let mut source: Option<&(dyn Error + 'static)> = Some(dynamic.error.as_ref());
    while let Some(current) = source {
        if current.is::<AuthRequiredError>() {
            return RemoteMcpError::AuthorizationRequired;
        }
        if current.is::<InsufficientScopeError>() {
            return RemoteMcpError::InsufficientScope;
        }
        // rmcp's own 401/403 detection (above) requires the response to
        // carry a WWW-Authenticate header; lacking one, it falls back to
        // `StreamableHttpError::UnexpectedServerResponse(format!("HTTP
        // {status}: {body}"))` instead — a bare status line with no
        // structured code, and no distinct type to downcast to (the enum's
        // type parameter is the transport's reqwest version, which isn't
        // one we can name without depending on that exact reqwest release).
        // Many real MCP OAuth resource servers omit that header on a plain
        // 401/403, so treat this text shape as authoritative too rather than
        // silently downgrading a real auth failure to an unretryable
        // generic protocol error.
        if let Some(status) = unexpected_response_status(&current.to_string()) {
            if status == 401 {
                return RemoteMcpError::AuthorizationRequired;
            }
            if status == 403 {
                return RemoteMcpError::InsufficientScope;
            }
        }
        source = current.source();
    }
    tracing::warn!(
        error = %dynamic.error,
        chain = %error_chain(dynamic.error.as_ref()),
        "remote MCP transport error was not a recognized 401/403; treating as a generic protocol failure"
    );
    RemoteMcpError::Protocol
}

/// Extracts the numeric status code from rmcp's
/// `"unexpected server response: HTTP <code> <reason>: <body>"` message, or
/// `None` if the text isn't in that shape (a different failure entirely, and
/// not something we should misclassify).
fn unexpected_response_status(message: &str) -> Option<u16> {
    let rest = message.split_once("HTTP ")?.1;
    let code = rest.split(|c: char| !c.is_ascii_digit()).next()?;
    code.parse().ok()
}

fn normalize_result(result: CallToolResult) -> Result<ToolOutput, RemoteMcpError> {
    let stdout = result
        .content
        .iter()
        .filter_map(|content| content.as_text())
        .map(|text| text.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let data = result
        .structured_content
        .unwrap_or_else(|| serde_json::from_str(&stdout).unwrap_or(serde_json::Value::Null));
    let size = stdout.len().saturating_add(if data.is_null() {
        0
    } else {
        serde_json::to_vec(&data).map_or(MAX_MCP_CONTENT_BYTES + 1, |value| value.len())
    });
    if size > MAX_MCP_CONTENT_BYTES {
        tracing::Span::current().record("tool.output_limit_violation", true);
        return Err(RemoteMcpError::OutputLimit);
    }
    if result.is_error.unwrap_or(false) {
        return Err(RemoteMcpError::Protocol);
    }
    Ok(ToolOutput {
        stdout,
        stderr: String::new(),
        exit_code: 0,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use rmcp::model::ContentBlock;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use crate::tools::oauth::{
        InMemoryCredentialStore, OAuthHttpClient, OAuthHttpClientFuture, OAuthHttpRequest,
    };

    #[test]
    fn normalizes_text_and_structured_results() {
        let text =
            normalize_result(CallToolResult::success(vec![ContentBlock::text("42")])).unwrap();
        assert_eq!(text.stdout, "42");
        assert_eq!(text.data, serde_json::json!(42));

        let structured = normalize_result(CallToolResult::structured(serde_json::json!({
            "answer": 42
        })))
        .unwrap();
        assert_eq!(structured.data, serde_json::json!({"answer": 42}));
    }

    /// A real HTTP server (no mocking of our own code) that requires
    /// `Authorization: Bearer good-token` and otherwise rejects with 401 —
    /// with or without a `WWW-Authenticate` challenge header, matching what
    /// real-world MCP OAuth resource servers do inconsistently in practice.
    fn auth_gated_server(with_www_authenticate: bool) -> axum::Router {
        axum::Router::new().route(
            "/mcp",
            post(
                move |headers: axum::http::HeaderMap, body: axum::body::Bytes| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default();
                    if auth == "Bearer good-token"
                        && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body)
                    {
                        return legacy_json_server(Json(json)).await;
                    }
                    if with_www_authenticate {
                        (
                            StatusCode::UNAUTHORIZED,
                            [("www-authenticate", "Bearer realm=\"test\"")],
                        )
                            .into_response()
                    } else {
                        StatusCode::UNAUTHORIZED.into_response()
                    }
                },
            ),
        )
    }

    async fn spawn(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}/mcp")
    }

    // Regression test for the bug reported 2026-08-14: OAuth connects
    // (a token is obtained and stored) but every subsequent tool call then
    // fails with a generic "remote MCP protocol request failed" instead of
    // refreshing the token and retrying — because classify_error could never
    // actually detect the 401, on any real server, with or without a
    // WWW-Authenticate header (see the doc comment on `classify_error`).
    #[tokio::test]
    async fn bare_401_without_www_authenticate_is_classified_as_authorization_required() {
        let endpoint = spawn(auth_gated_server(false)).await;
        let error = list_once(&endpoint, Some("bad-token".to_owned()))
            .await
            .unwrap_err();
        assert!(matches!(error, RemoteMcpError::AuthorizationRequired));
    }

    #[tokio::test]
    async fn textbook_401_with_www_authenticate_is_classified_as_authorization_required() {
        let endpoint = spawn(auth_gated_server(true)).await;
        let error = list_once(&endpoint, Some("bad-token".to_owned()))
            .await
            .unwrap_err();
        assert!(matches!(error, RemoteMcpError::AuthorizationRequired));
    }

    // Same defect, but hit via ServiceError::TransportSend instead of
    // ClientInitializeError::TransportError: the token is accepted at
    // connect time (discovery/initialize succeed) and only rejected once an
    // actual tools/call goes out — e.g. a token that dies mid-session.
    #[tokio::test]
    async fn mid_session_401_on_call_tool_is_classified_as_authorization_required() {
        let app = axum::Router::new().route(
            "/mcp",
            post(
                |headers: axum::http::HeaderMap, body: axum::body::Bytes| async move {
                    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) else {
                        return StatusCode::BAD_REQUEST.into_response();
                    };
                    let method = json["method"].as_str().unwrap_or_default();
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default();
                    // Accept the connect-time handshake unconditionally, but
                    // reject the actual tool call — simulating a token that
                    // was valid when OAuth completed but is rejected by the
                    // resource server by the time a tool actually runs.
                    if method == "tools/call" && auth != "Bearer good-token" {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    legacy_json_server(Json(json)).await
                },
            ),
        );
        let endpoint = spawn(app).await;
        let arguments = [("value".to_owned(), serde_json::json!(41))]
            .into_iter()
            .collect();
        let error = call_once(
            &endpoint,
            Some("bad-token".to_owned()),
            "increment",
            &arguments,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, RemoteMcpError::AuthorizationRequired));
    }

    #[tokio::test]
    async fn remote_json_round_trip_uses_legacy_lifecycle() {
        let app = axum::Router::new().route("/mcp", post(legacy_json_server));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let arguments = [("value".to_owned(), serde_json::json!(41))]
            .into_iter()
            .collect();

        let output = call_once(
            &format!("http://{address}/mcp"),
            None,
            "increment",
            &arguments,
        )
        .await
        .unwrap();
        assert_eq!(output.stdout, "42");
        assert_eq!(output.data, serde_json::json!({"answer": 42}));
        server.abort();
    }

    async fn legacy_json_server(Json(request): Json<serde_json::Value>) -> Response {
        let method = request["method"].as_str().unwrap_or_default();
        let id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        match method {
            "server/discover" => Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "Method not found"}
            }))
            .into_response(),
            "initialize" => Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "test", "version": "1"}
                }
            }))
            .into_response(),
            "notifications/initialized" => StatusCode::ACCEPTED.into_response(),
            "tools/call" => Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{"type": "text", "text": "42"}],
                    "structuredContent": {"answer": 42},
                    "isError": false
                }
            }))
            .into_response(),
            _ => StatusCode::BAD_REQUEST.into_response(),
        }
    }

    // ─── Full round-trip: production retry logic + a real resource server ──
    //
    // Everything below reuses the exact OAuth-boundary substitution pattern
    // from tools::oauth's own tests (ScriptedOAuthHttpClient answering
    // discovery + token requests) so the credential store and authorization
    // server are the only things not hitting real network/OS services — the
    // MCP resource server is real (axum, real TCP), and the retry logic
    // under test (`call_tool_via_oauth`) is the exact function `run` calls
    // in production, not a re-implementation of it.

    struct ScriptedResponse {
        status: u16,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    #[derive(Clone, Default)]
    struct ScriptedOAuthHttpClient {
        responses: Arc<StdMutex<VecDeque<ScriptedResponse>>>,
    }

    impl ScriptedOAuthHttpClient {
        fn new(responses: Vec<ScriptedResponse>) -> Self {
            Self {
                responses: Arc::new(StdMutex::new(responses.into())),
            }
        }
    }

    impl OAuthHttpClient for ScriptedOAuthHttpClient {
        fn execute(&self, _operation: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
            let scripted = self.responses.lock().unwrap().pop_front();
            Box::pin(async move {
                let scripted = scripted.ok_or_else(|| {
                    Box::new(std::io::Error::other("missing scripted OAuth response"))
                        as rmcp::transport::auth::OAuthHttpClientError
                })?;
                let mut response: axum::http::Response<Vec<u8>> = Default::default();
                *response.status_mut() = scripted.status.try_into().unwrap();
                for (name, value) in scripted.headers {
                    response.headers_mut().insert(
                        name.parse::<axum::http::HeaderName>().unwrap(),
                        value.parse::<axum::http::HeaderValue>().unwrap(),
                    );
                }
                *response.body_mut() = scripted.body;
                Ok::<_, rmcp::transport::auth::OAuthHttpClientError>(response)
            })
        }
    }

    fn json_response(value: serde_json::Value) -> ScriptedResponse {
        ScriptedResponse {
            status: 200,
            headers: [("content-type".to_owned(), "application/json".to_owned())]
                .into_iter()
                .collect(),
            body: serde_json::to_vec(&value).unwrap(),
        }
    }

    /// Protected-resource metadata + authorization-server metadata — the two
    /// discovery requests `AuthorizationManager` always makes before it can
    /// reach a token endpoint, for whatever `endpoint` the facade was built
    /// against.
    /// `resource` must be the bare origin (scheme + authority), not the full
    /// endpoint URL with its path — matching what tools::oauth's own tests
    /// assert AuthorizationManager actually requires.
    fn discovery_responses(resource_origin: &str) -> Vec<ScriptedResponse> {
        vec![
            json_response(serde_json::json!({
                "resource": resource_origin,
                "authorization_servers": ["https://auth.example.com"],
                "scopes_supported": ["resource:read"]
            })),
            json_response(serde_json::json!({
                "issuer": "https://auth.example.com",
                "authorization_endpoint": "https://auth.example.com/authorize",
                "token_endpoint": "https://auth.example.com/token",
                "registration_endpoint": "https://auth.example.com/register",
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"],
                "scopes_supported": ["resource:read", "offline_access"]
            })),
        ]
    }

    // Regression test for the bug reported 2026-08-14, end to end: OAuth
    // completes (a token is obtained and stored) but the MCP resource server
    // rejects it — matching what the real Granola MCP endpoint apparently
    // does — so this exercises the *entire* production path: get token,
    // real HTTP call to a real server, get a real 401, correctly classify it
    // (the actual bug), refresh, retry, succeed. Before the classify_error
    // fix, this hung/failed at the "retry" step because the initial 401 was
    // misclassified as a generic Protocol error and never triggered a
    // refresh.
    #[tokio::test]
    async fn oauth_call_recovers_from_a_resource_server_rejecting_the_initial_token() {
        let endpoint = spawn(auth_gated_server(false)).await;
        let origin = endpoint.trim_end_matches("/mcp");

        let mut responses = discovery_responses(origin);
        responses.push(json_response(serde_json::json!({
            "access_token": "stale-token",
            "token_type": "bearer",
            "refresh_token": "valid-refresh-token",
            "expires_in": 3600
        })));
        responses.push(json_response(serde_json::json!({
            "access_token": "good-token",
            "token_type": "bearer",
            "refresh_token": "valid-refresh-token",
            "expires_in": 3600
        })));
        let http = Arc::new(ScriptedOAuthHttpClient::new(responses));
        let store = Arc::new(InMemoryCredentialStore::new());
        let facade = McpOAuthFacade::with_boundaries(
            &endpoint,
            Some("test-client".to_owned()),
            store.clone(),
            http,
        )
        .await
        .unwrap();

        // The 401/403 challenge tells AuthorizationManager where to find the
        // protected-resource metadata (RFC 9728) without having to guess a
        // well-known path against the real local server — same shape as
        // tools::oauth's own tests use.
        let challenge =
            format!("Bearer resource_metadata=\"{origin}/.well-known/oauth-protected-resource\"");
        let start = facade
            .begin_authorization_with_challenge(
                "http://127.0.0.1:4567/oauth/callback",
                Some(&challenge),
            )
            .await
            .unwrap();
        facade
            .complete_authorization("authorization-code", &start.state)
            .await
            .unwrap();

        let arguments = [("value".to_owned(), serde_json::json!(41))]
            .into_iter()
            .collect();
        let output = call_tool_via_oauth(&facade, &endpoint, "increment", &arguments)
            .await
            .expect("token refresh should recover from the resource server's initial 401");
        assert_eq!(output.stdout, "42");
    }
}
