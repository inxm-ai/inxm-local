//! Cross-module coverage for outbound Streamable HTTP MCP tools.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use indexmap::IndexMap;
use inxm_local::tools::catalog::{McpAuth, McpConfig, McpTransport, ToolConfig, ToolEntry};
use inxm_local::tools::execute_tool;

#[tokio::test]
async fn remote_mcp_catalog_executes_without_persisting_credentials() {
    let app = axum::Router::new().route("/mcp", post(mcp_server));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake remote MCP server");
    let address = listener.local_addr().expect("fake server address");
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve fake remote MCP server")
    });
    let endpoint = format!("http://{address}/mcp");
    let entry = ToolEntry {
        name: "remote-increment".to_owned(),
        description: "Increment through remote MCP".to_owned(),
        config: ToolConfig::Mcp(McpConfig {
            tool_name: "increment".to_owned(),
            transport: McpTransport::StreamableHttp {
                endpoint: endpoint.clone(),
                auth: McpAuth::None,
            },
        }),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "value": { "type": "integer" } },
            "required": ["value"]
        }),
        output_schema: serde_json::json!({
            "type": "object",
            "properties": { "answer": { "type": "integer" } },
            "required": ["answer"]
        }),
        allowlisted: true,
        timeout_secs: Some(5),
    };

    let yaml = serde_yaml::to_string(&entry).expect("serialize remote MCP entry");
    assert!(yaml.contains(&endpoint));
    for forbidden in [
        "access_token",
        "refresh_token",
        "client_secret",
        "authorization_code",
        "pkce_verifier",
    ] {
        assert!(!yaml.contains(forbidden), "catalog leaked {forbidden}");
    }

    let arguments = [("value".to_owned(), serde_json::json!(41))]
        .into_iter()
        .collect::<IndexMap<_, _>>();
    let output = execute_tool(&entry, &arguments, None)
        .await
        .expect("execute remote MCP tool");
    assert_eq!(output.stdout, "42");
    assert_eq!(output.data, serde_json::json!({"answer": 42}));

    server.abort();
}

async fn mcp_server(Json(request): Json<serde_json::Value>) -> Response {
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
                "serverInfo": {"name": "integration-fake", "version": "1"}
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
