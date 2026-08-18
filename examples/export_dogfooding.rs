use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, ensure};
use inxm_local::app::{
    engine::{AppSettings, BackendChoice, DataPaths},
    mcp_server,
};
use inxm_local::plan::bundle::PlanBundle;
use inxm_local::plan::types::{Plan, StepConfig};
use serde_json::{Value, json};

const MCP_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const DOGFOODING_DIRECTORY: &str = "examples/dogfooding";

struct Workflow {
    slug: &'static str,
    intent: &'static str,
    required_inputs: &'static [&'static str],
    required_terms: &'static [&'static str],
    requires_retry_loop: bool,
}

const WORKFLOWS: &[Workflow] = &[
    Workflow {
        slug: "prompt-to-plan",
        intent: include_str!("dogfooding/prompt-to-plan.md"),
        required_inputs: &["plan_prompt", "branch_name", "root_directory"],
        required_terms: &[
            "mcp",
            "compile_plan",
            "show_plan",
            "expectation",
            "feedback",
            "diagnos",
            "regression",
            "codex",
        ],
        requires_retry_loop: true,
    },
    Workflow {
        slug: "feature-development",
        intent: include_str!("dogfooding/feature-development.md"),
        required_inputs: &["feature_request", "branch_name", "root_directory"],
        required_terms: &["module", "package", "codex", "approv"],
        requires_retry_loop: false,
    },
    Workflow {
        slug: "bugfix",
        intent: include_str!("dogfooding/bugfix.md"),
        required_inputs: &["bug_report", "branch_name", "root_directory"],
        required_terms: &["reproduce", "failing test", "before", "fix", "regression"],
        requires_retry_loop: false,
    },
];

fn main() -> Result<()> {
    tokio::runtime::Runtime::new()
        .context("create exporter runtime")?
        .block_on(export_workflows())
}

async fn export_workflows() -> Result<()> {
    let output_directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DOGFOODING_DIRECTORY);
    let requested = std::env::args().skip(1).collect::<BTreeSet<_>>();
    for slug in &requested {
        ensure!(
            WORKFLOWS.iter().any(|workflow| workflow.slug == slug),
            "unknown dogfooding workflow '{slug}'"
        );
    }
    let workflows = WORKFLOWS
        .iter()
        .filter(|workflow| requested.is_empty() || requested.contains(workflow.slug))
        .collect::<Vec<_>>();
    let temporary_directory = tempfile::tempdir().context("create temporary MCP data directory")?;
    let exports_directory = temporary_directory.path().join("exports");
    let paths = DataPaths::at(temporary_directory.path().to_path_buf());
    AppSettings {
        backend: BackendChoice::Codex,
        experimental_agent_calls: true,
        ..AppSettings::default()
    }
    .save(&paths.settings_path)
    .context("save MCP compiler settings")?;

    let port = available_port()?;
    let statuses = mcp_server::spawn(paths, port);
    wait_for_server(&statuses)?;
    let endpoint = format!("http://127.0.0.1:{port}/mcp");
    let client = reqwest::Client::new();
    rpc(&client, &endpoint, 1, "initialize", json!({})).await?;
    let listed = rpc(&client, &endpoint, 2, "tools/list", json!({})).await?;
    ensure!(
        listed["result"]["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "export_plan")),
        "local MCP server does not advertise export_plan"
    );

    for (index, workflow) in workflows.iter().enumerate() {
        println!("Compiling {}...", workflow.slug);
        let request_id = 3 + (index as u64 * 3);
        let compiled = call_tool(
            &client,
            &endpoint,
            request_id,
            "compile_plan",
            json!({ "intent": workflow.intent }),
        )
        .await
        .with_context(|| format!("MCP compile_plan failed for {}", workflow.slug))?;
        let plan: Plan =
            serde_json::from_value(compiled["result"]["structuredContent"]["plan"].clone())
                .with_context(|| {
                    format!("compile_plan returned an invalid {} plan", workflow.slug)
                })?;
        verify_plan(workflow, &plan)
            .with_context(|| format!("{} did not meet its export contract", workflow.slug))?;

        let shown = call_tool(
            &client,
            &endpoint,
            request_id + 1,
            "show_plan",
            json!({ "plan_ref": plan.metadata.id }),
        )
        .await
        .with_context(|| format!("MCP show_plan failed for {}", workflow.slug))?;
        let shown_plan: Plan =
            serde_json::from_value(shown["result"]["structuredContent"]["plan"].clone())
                .with_context(|| format!("show_plan returned an invalid {} plan", workflow.slug))?;
        ensure!(
            shown_plan == plan,
            "show_plan did not return the compiled plan"
        );

        let export_file_name = format!("{}.plan.json", workflow.slug);
        call_tool(
            &client,
            &endpoint,
            request_id + 2,
            "export_plan",
            json!({
                "plan_ref": plan.metadata.id,
                "output_path": &export_file_name,
            }),
        )
        .await
        .with_context(|| format!("MCP export_plan failed for {}", workflow.slug))?;
        let exported_at = exports_directory.join(&export_file_name);
        let output_path = output_directory.join(&export_file_name);
        let exported = PlanBundle::load_from_file(&exported_at)
            .with_context(|| format!("load exported plan bundle from {}", exported_at.display()))?;
        ensure!(
            exported.plan == plan,
            "exported plan bundle differs from MCP plan"
        );
        std::fs::copy(&exported_at, &output_path).with_context(|| {
            format!(
                "copy exported bundle from {} to {}",
                exported_at.display(),
                output_path.display()
            )
        })?;
        println!("Exported {}", output_path.display());
    }
    Ok(())
}

fn available_port() -> Result<u16> {
    Ok(
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .context("bind temporary MCP port")?
            .local_addr()
            .context("read temporary MCP address")?
            .port(),
    )
}

fn wait_for_server(statuses: &std::sync::mpsc::Receiver<mcp_server::ServerStatus>) -> Result<()> {
    loop {
        match statuses.recv_timeout(MCP_STARTUP_TIMEOUT) {
            Ok(mcp_server::ServerStatus::Starting { .. }) => {}
            Ok(mcp_server::ServerStatus::Running { fallback_from, .. }) => {
                ensure!(
                    fallback_from.is_none(),
                    "requested MCP port was unavailable"
                );
                return Ok(());
            }
            Ok(mcp_server::ServerStatus::Failed { error, .. }) => {
                return Err(anyhow!("MCP server failed to start: {error}"));
            }
            Err(error) => return Err(anyhow!("MCP server startup timed out: {error}")),
        }
    }
}

async fn rpc(
    client: &reqwest::Client,
    endpoint: &str,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
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
        .with_context(|| format!("send MCP {method} request"))?
        .error_for_status()
        .with_context(|| format!("MCP {method} HTTP status"))?
        .json::<Value>()
        .await
        .with_context(|| format!("decode MCP {method} response"))?;
    if let Some(error) = response.get("error") {
        return Err(anyhow!("MCP {method} error: {error}"));
    }
    Ok(response)
}

async fn call_tool(
    client: &reqwest::Client,
    endpoint: &str,
    id: u64,
    name: &str,
    arguments: Value,
) -> Result<Value> {
    let response = rpc(
        client,
        endpoint,
        id,
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
    .await?;
    if response["result"]["isError"].as_bool().unwrap_or(false) {
        return Err(anyhow!(
            "MCP tool '{name}' reported failure: {}",
            response["result"]["content"]
        ));
    }
    Ok(response)
}

/// A CODE_CALL step may check whether `codex`/`claude` is installed and
/// authenticated (a preflight, e.g. `codex login status`) but must never run
/// it to do coding-agent work — that must go through AGENT_CALL, gated by
/// human approval before it can edit the workspace (see
/// `AGENT_CALL_CONFIG_SHAPE` in `src/compiler/prompt.rs`).
fn invokes_coding_agent_cli(script: &str) -> bool {
    const FORBIDDEN: &[&str] = &[
        "codex exec",
        "claude -p",
        "spawnsync('codex'",
        "spawnsync(\"codex\"",
        "spawnsync('claude'",
        "spawnsync(\"claude\"",
    ];
    FORBIDDEN.iter().any(|pattern| script.contains(pattern))
}

fn verify_plan(workflow: &Workflow, plan: &Plan) -> Result<()> {
    let input_names = plan
        .inputs
        .iter()
        .map(|input| input.name.as_str())
        .collect::<BTreeSet<_>>();
    for input in workflow.required_inputs {
        ensure!(input_names.contains(input), "missing input '{input}'");
    }
    ensure!(
        input_names.iter().any(|name| name.contains("expectation")),
        "missing expectations input"
    );

    let text = serde_json::to_string(plan)?.to_ascii_lowercase();
    for required in workflow.required_terms.iter().chain(
        [
            "cargo test --all-targets --all-features",
            "live_spec_planning",
            "cargo fmt --check",
            "cargo clippy --all-targets --all-features",
            "skills/principal-developer/skill.md",
            "credentials",
        ]
        .iter(),
    ) {
        ensure!(text.contains(required), "plan missing '{required}'");
    }
    ensure!(
        text.contains("status --porcelain")
            || text.contains("diff --quiet")
            || text.contains("status -s"),
        "plan does not check for a clean checkout"
    );
    ensure!(
        text.contains("pull --ff-only") || text.contains("merge --ff-only"),
        "plan does not require a fast-forward-only main update"
    );
    ensure!(
        text.contains("git switch") || text.contains("git checkout"),
        "plan does not switch branches explicitly"
    );
    if workflow.slug == "feature-development" {
        ensure!(
            text.contains("parallel") || text.contains("concurrent"),
            "feature workflow does not distribute independent work concurrently"
        );
    }

    let approval_steps = plan
        .steps
        .iter()
        .filter_map(|step| match &step.config {
            StepConfig::HumanInteraction(config) if config.approval_required => {
                Some(step.id.as_str())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    ensure!(!approval_steps.is_empty(), "plan has no approval gate");
    ensure!(
        plan.steps.iter().any(|step| match &step.config {
            StepConfig::CodeCall(config) => config.inline.as_ref().is_some_and(|script| {
                let script = script.to_ascii_lowercase();
                script.contains("gh")
                    && script.contains("pr")
                    && script.contains("create")
                    && transitively_depends_on(plan, &step.id, &approval_steps)
            }),
            _ => false,
        }),
        "gh pr create is not transitively gated by human approval"
    );

    for step in &plan.steps {
        if let StepConfig::CodeCall(config) = &step.config {
            let script = config
                .inline
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            ensure!(
                !invokes_coding_agent_cli(&script),
                "step '{}' invokes a coding-agent CLI from CODE_CALL instead of AGENT_CALL",
                step.id
            );
        }
    }
    let agent_steps = plan
        .steps
        .iter()
        .filter(|step| matches!(step.config, StepConfig::AgentCall(_)))
        .map(|step| step.id.as_str())
        .collect::<Vec<_>>();
    ensure!(
        !agent_steps.is_empty(),
        "plan does not use an AGENT_CALL step for the coding-agent work its intent describes"
    );
    ensure!(
        agent_steps.iter().any(|agent_step| transitively_depends_on(
            plan,
            agent_step,
            &approval_steps
        )),
        "no AGENT_CALL step is transitively gated by human approval before it edits the workspace"
    );

    if workflow.requires_retry_loop {
        ensure!(
            plan.steps.iter().any(|step| matches!(
                &step.config,
                StepConfig::FanOut(config)
                    if !config.over.trim().is_empty()
                        && config.until.as_ref().is_some_and(|until| !until.trim().is_empty())
            )),
            "plan does not contain a bounded FAN_OUT.until loop"
        );
    }
    Ok(())
}

fn transitively_depends_on(plan: &Plan, step_id: &str, targets: &BTreeSet<&str>) -> bool {
    let mut pending = vec![step_id];
    let mut visited = BTreeSet::new();
    while let Some(current) = pending.pop() {
        if !visited.insert(current) {
            continue;
        }
        let Some(step) = plan.step(current) else {
            continue;
        };
        for dependency in &step.depends_on {
            if targets.contains(dependency.as_str()) {
                return true;
            }
            pending.push(dependency);
        }
    }
    false
}
