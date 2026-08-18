//! HTTP-level MCP integration test for declared plan inputs.

use std::time::Duration;

use inxm_local::app::{engine::DataPaths, mcp_server};
use inxm_local::plan::bundle::{CURRENT_FORMAT_VERSION, PlanBundle};
use inxm_local::plan::types::{
    Plan, PlanInput, PlanMetadata, PlanOutput, PlanStep, StepConfig, ToolCallConfig,
};
use inxm_local::storage::StorageRoot;
use inxm_local::tools::catalog::{SubprocessConfig, ToolCatalog, ToolConfig, ToolEntry};
use serde_json::{Value, json};

fn available_port() -> u16 {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind ephemeral port")
        .local_addr()
        .expect("ephemeral local address")
        .port()
}

fn input_plan() -> Plan {
    let mut metadata = PlanMetadata::new(Some(
        "Echo a message supplied whenever the plan is triggered or scheduled".to_owned(),
    ));
    metadata.id = "mcp-input-plan".to_owned();
    Plan {
        metadata,
        name: "mcp-input-plan".to_owned(),
        description: Some("Exercises MCP invocation and schedule inputs".to_owned()),
        inputs: vec![PlanInput {
            name: "message".to_owned(),
            description: Some("Message for this invocation".to_owned()),
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: inxm_local::plan::types::InputKind::Value,
        }],
        config: Default::default(),
        steps: vec![PlanStep {
            id: "echo_input".to_owned(),
            name: "Echo input".to_owned(),
            description: None,
            config: StepConfig::ToolCall(ToolCallConfig {
                tool: "echo".to_owned(),
                arguments: [("message".to_owned(), json!("${input.message}"))]
                    .into_iter()
                    .collect(),
            }),
            depends_on: vec![],
            outputs: vec![PlanOutput {
                name: "stdout".to_owned(),
                description: None,
                value_type: "string".to_owned(),
            }],
            timeout_secs: None,
            retry: None,
        }],
        outputs: vec![],
    }
}

fn env_echo_catalog() -> ToolCatalog {
    let (command, args) = if cfg!(windows) {
        (
            "cmd".to_owned(),
            vec![
                "/C".to_owned(),
                "echo".to_owned(),
                "%INXM_ARG_MESSAGE%".to_owned(),
            ],
        )
    } else {
        (
            "sh".to_owned(),
            vec![
                "-c".to_owned(),
                "printf '%s\\n' \"$INXM_ARG_MESSAGE\"".to_owned(),
            ],
        )
    };
    ToolCatalog::new(vec![ToolEntry {
        name: "echo".to_owned(),
        description: "Echo the message exposed through the tool environment".to_owned(),
        config: ToolConfig::Subprocess(SubprocessConfig {
            command,
            args,
            env: Default::default(),
            working_dir: None,
        }),
        input_schema: json!({
            "type": "object",
            "properties": { "message": { "type": "string" } },
            "required": ["message"]
        }),
        output_schema: json!({ "type": "object" }),
        allowlisted: true,
        timeout_secs: Some(10),
    }])
}

async fn rpc(
    client: &reqwest::Client,
    endpoint: &str,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let response = client
        .post(endpoint)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .send()
        .await
        .expect("send MCP request")
        .error_for_status()
        .expect("MCP HTTP status")
        .json::<Value>()
        .await
        .expect("MCP JSON response");
    assert!(response.get("error").is_none(), "MCP error: {response}");
    response
}

async fn tool(
    client: &reqwest::Client,
    endpoint: &str,
    id: u64,
    name: &str,
    arguments: Value,
) -> Value {
    rpc(
        client,
        endpoint,
        id,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
    .await
}

#[tokio::test]
async fn mcp_executes_and_schedules_with_declared_inputs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = DataPaths::at(temp.path().to_path_buf());
    let stored_plan = input_plan();
    StorageRoot::open(&paths.data_dir)
        .expect("storage")
        .plans()
        .save(&stored_plan)
        .expect("save input plan");
    env_echo_catalog()
        .save_to_file(&paths.catalog_path)
        .expect("save explicit env-driven tool catalog");

    let port = available_port();
    let status = mcp_server::spawn(paths, port);
    loop {
        match status.recv_timeout(Duration::from_secs(10)) {
            Ok(mcp_server::ServerStatus::Running { .. }) => break,
            Ok(mcp_server::ServerStatus::Starting { .. }) => {}
            Ok(mcp_server::ServerStatus::Failed { error, .. }) => {
                panic!("MCP server failed: {error}")
            }
            Err(error) => panic!("MCP server startup timed out: {error}"),
        }
    }

    let endpoint = format!("http://127.0.0.1:{port}/mcp");
    let client = reqwest::Client::new();
    rpc(&client, &endpoint, 1, "initialize", json!({})).await;

    let listed = rpc(&client, &endpoint, 2, "tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().expect("tools array");
    for name in ["execute_plan", "schedule_plan", "resume_run", "export_plan"] {
        let schema = tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing MCP tool {name}"));
        let expected_property = if name == "export_plan" {
            "output_path"
        } else {
            "inputs"
        };
        assert!(
            schema["inputSchema"]["properties"]
                .get(expected_property)
                .is_some(),
            "{name} does not advertise {expected_property}: {schema}"
        );
    }

    let shown = tool(
        &client,
        &endpoint,
        3,
        "show_plan",
        json!({ "plan_ref": "mcp-input-plan" }),
    )
    .await;
    assert_eq!(
        shown["result"]["structuredContent"]["plan"]["inputs"][0]["name"],
        "message"
    );

    // Exports are confined to the app's `<data_dir>/exports/` directory:
    // the MCP caller passes a relative name, never an absolute
    // path or one escaping the export root.
    let export_path = temp.path().join("exports/mcp-input-plan.json");
    let exported = tool(
        &client,
        &endpoint,
        4,
        "export_plan",
        json!({
            "plan_ref": "mcp-input-plan",
            "output_path": "mcp-input-plan.json",
        }),
    )
    .await;
    assert_eq!(
        exported["result"]["structuredContent"]["plan_id"],
        "mcp-input-plan"
    );
    assert_eq!(
        exported["result"]["structuredContent"]["format_version"],
        CURRENT_FORMAT_VERSION
    );
    let exported_bundle =
        PlanBundle::load_from_file(&export_path).expect("load MCP-exported plan bundle");
    assert_eq!(exported_bundle.format_version, CURRENT_FORMAT_VERSION);
    assert_eq!(exported_bundle.plan, stored_plan);
    assert_eq!(exported_bundle.tools.len(), 1);
    assert_eq!(exported_bundle.tools[0].name, "echo");

    // An absolute output_path (arbitrary-file-write vector) is
    // rejected rather than honored.
    let escape = temp.path().join("escape.json");
    let rejected = tool(
        &client,
        &endpoint,
        41,
        "export_plan",
        json!({
            "plan_ref": "mcp-input-plan",
            "output_path": escape,
        }),
    )
    .await;
    assert_eq!(
        rejected["result"]["isError"], true,
        "absolute export path must be rejected: {rejected}"
    );
    assert!(
        !escape.exists(),
        "rejected export must not write the file outside the exports dir"
    );

    let executed = tool(
        &client,
        &endpoint,
        5,
        "execute_plan",
        json!({
            "plan_ref": "mcp-input-plan",
            "inputs": { "message": "executed through MCP" }
        }),
    )
    .await;
    let run = &executed["result"]["structuredContent"]["run"];
    assert_eq!(run["inputs"]["message"], "executed through MCP");
    assert_eq!(run["status"], "succeeded");
    assert!(
        run["step_runs"]["echo_input"]["stdout"]
            .as_str()
            .is_some_and(|stdout| stdout.contains("executed through MCP")),
        "input was not resolved into the executed tool call: {run}"
    );

    let inspected = tool(
        &client,
        &endpoint,
        6,
        "inspect_run",
        json!({ "run_id": run["id"] }),
    )
    .await;
    assert_eq!(
        inspected["result"]["structuredContent"]["run"]["inputs"]["message"],
        "executed through MCP"
    );

    let scheduled = tool(
        &client,
        &endpoint,
        7,
        "schedule_plan",
        json!({
            "plan_ref": "mcp-input-plan",
            "cron": "*/5 * * * *",
            "inputs": { "message": "scheduled through MCP" }
        }),
    )
    .await;
    assert_eq!(
        scheduled["result"]["structuredContent"]["schedule"]["inputs"]["message"],
        "scheduled through MCP"
    );

    let schedules = tool(&client, &endpoint, 8, "list_schedules", json!({})).await;
    assert_eq!(
        schedules["result"]["structuredContent"]["schedules"][0]["inputs"]["message"],
        "scheduled through MCP"
    );
}
