#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use indexmap::IndexMap;
use inxm_local::executor::{
    ExecutorConfig, HumanDecision, LlmKeys, RepairResumeMode, execute, resume, resume_from_repair,
};
use inxm_local::llm::{LlmAuth, LlmProfile, LlmProtocol};
use inxm_local::plan::types::*;
use inxm_local::storage::StorageRoot;
use inxm_local::storage::runs::{Run, RunStatus, StepRun, StepRunIteration, StepRunStatus};
use inxm_local::tools::catalog::{
    McpConfig, McpTransport, SubprocessConfig, ToolCatalog, ToolConfig, ToolEntry,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MCP_READER: &str = r#"
allowed=$(cd "$1" && pwd -P) || exit 1
IFS= read -r initialize
printf '%s\n' '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"workspace-reader","version":"1"}}}'
IFS= read -r initialized
IFS= read -r request
if [ "$allowed" != "$(pwd -P)" ]; then
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"workspace is outside allowed directory"}]}}'
else
    result=$(cat artifact.txt) || exit 1
    printf '{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$result"
fi
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    storage: Arc<StorageRoot>,
    plan: Plan,
    catalog: ToolCatalog,
    profile: LlmProfile,
}

fn step(id: &str, config: StepConfig, depends_on: &[&str], output: &str) -> PlanStep {
    PlanStep {
        id: id.to_owned(),
        name: id.to_owned(),
        description: None,
        config,
        depends_on: depends_on.iter().map(|id| (*id).to_owned()).collect(),
        outputs: vec![PlanOutput {
            name: output.to_owned(),
            description: None,
            value_type: "string".to_owned(),
        }],
        timeout_secs: Some(5),
        retry: None,
    }
}

fn seed_tool(working_dir: Option<String>) -> ToolEntry {
    ToolEntry {
        name: "seed".to_owned(),
        description: "Write an intermediate workflow file".to_owned(),
        config: ToolConfig::Subprocess(SubprocessConfig {
            command: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "printf '%s' \"$INXM_ARG_BODY\" > source.txt; printf '{\"path\":\"source.txt\"}'"
                    .to_owned(),
            ],
            env: Default::default(),
            working_dir,
        }),
        input_schema: json!({"type":"object","properties":{"body":{"type":"string"}},"required":["body"]}),
        output_schema: json!({"type":"object"}),
        allowlisted: true,
        timeout_secs: Some(5),
    }
}

impl Fixture {
    fn new(allowed_directory: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let storage = Arc::new(StorageRoot::open(dir.path()).unwrap());
        let executable = dir.path().join("mock-claude");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
IFS= read -r request
case "$request" in *prepared.txt*cringe*) ;; *) exit 9;; esac
cat prepared.txt > artifact.txt || exit 1
printf '%s\n' '{"type":"result","result":"artifact created","usage":{"input_tokens":10,"output_tokens":5}}'
"#,
        ).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let catalog = ToolCatalog::new(vec![
            seed_tool(None),
            ToolEntry {
                name: "read-artifact".to_owned(),
                description: "Read an artifact through a workspace-scoped MCP server".to_owned(),
                config: ToolConfig::Mcp(McpConfig {
                    tool_name: "read_text_file".to_owned(),
                    transport: McpTransport::Stdio {
                        server_command: "sh".to_owned(),
                        server_args: vec![
                            "-c".to_owned(),
                            MCP_READER.to_owned(),
                            "workspace-reader".to_owned(),
                            allowed_directory.to_owned(),
                        ],
                        server_env: Default::default(),
                    },
                }),
                input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
                output_schema: json!({"type":"string"}),
                allowlisted: true,
                timeout_secs: Some(5),
            },
        ]);
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "Shared workspace".to_owned(),
            description: None,
            inputs: vec![
                PlanInput {
                    name: "root_directory".to_owned(),
                    description: None,
                    value_type: "string".to_owned(),
                    required: false,
                    default: None,
                    input_kind: InputKind::DirectoryPath,
                },
                PlanInput {
                    name: "style".to_owned(),
                    description: None,
                    value_type: "string".to_owned(),
                    required: false,
                    default: Some(json!("cringe dad joke")),
                    input_kind: InputKind::Value,
                },
            ],
            config: Default::default(),
            steps: vec![
                step("seed", StepConfig::ToolCall(ToolCallConfig {
                    tool: "seed".to_owned(),
                    arguments: IndexMap::from([("body".to_owned(), json!("orchestrator-data:"))]),
                }), &[], "path"),
                step("prepare", StepConfig::CodeCall(CodeCallConfig {
                    language: "sh".to_owned(),
                    inline: Some("cat source.txt > prepared.txt; printf '%s' \"$STYLE\" >> prepared.txt; printf '{\"path\":\"prepared.txt\"}'".to_owned()),
                    file: None,
                    args: vec![],
                    stdin: None,
                    env: IndexMap::from([("STYLE".to_owned(), "${input.style}".to_owned())]),
                    working_dir: Some("${input.root_directory}".to_owned()),
                    timeout_secs: Some(5),
                }), &["seed"], "path"),
                step("agent", StepConfig::AgentCall(AgentCallConfig {
                    objective: "Read ${step.prepare.path}, use style ${input.style}, and write artifact.txt.".to_owned(),
                    working_dir: "${input.root_directory}".to_owned(),
                    timeout_secs: Some(5),
                }), &["prepare"], "result"),
                step("read", StepConfig::ToolCall(ToolCallConfig {
                    tool: "read-artifact".to_owned(),
                    arguments: IndexMap::from([("path".to_owned(), json!("artifact.txt"))]),
                }), &["agent"], "content"),
            ],
            outputs: vec![PlanOutputRef {
                name: "artifact".to_owned(),
                description: None,
                source: "${step.read.content}".to_owned(),
            }],
        };
        let profile = LlmProfile {
            id: "test".to_owned(),
            name: "Claude API".to_owned(),
            protocol: LlmProtocol::AnthropicMessages,
            model: "test-model".to_owned(),
            base_url: "https://api.anthropic.com/v1".to_owned(),
            api_key: "test-key-not-a-real-secret".to_owned(),
            auth: LlmAuth::Auto,
            headers: Default::default(),
            executable: executable.display().to_string(),
            command_template: String::new(),
            max_tokens: None,
            temperature: None,
            timeout_secs: 5,
            codex_sandbox_mode: Default::default(),
        };
        Self {
            _dir: dir,
            storage,
            plan,
            catalog,
            profile,
        }
    }

    fn config(&self, root: Option<&Path>) -> ExecutorConfig {
        ExecutorConfig {
            inputs: root
                .map(|path| IndexMap::from([("root_directory".to_owned(), json!(path))]))
                .unwrap_or_default(),
            timeout_secs: Some(5),
            storage: self.storage.clone(),
            catalog: self.catalog.clone(),
            progress: None,
            human: None,
            llm_keys: LlmKeys {
                profile: Some(self.profile.clone()),
                ..Default::default()
            },
            claude_usage_enabled: false,
            claude_prompts_enabled: false,
            source: None,
        }
    }
}

#[tokio::test]
async fn subprocess_code_agent_and_mcp_share_default_and_selected_workspaces() {
    for selected in [false, true] {
        let fixture = Fixture::new(".");
        let user_directory = tempfile::tempdir().unwrap();
        let root = selected.then_some(user_directory.path());
        let errors = inxm_local::validator::validate(&fixture.plan, &fixture.catalog);
        assert!(errors.is_empty(), "{errors:?}");
        let run = execute(fixture.plan.clone(), fixture.config(root))
            .await
            .unwrap();
        assert_eq!(run.status, RunStatus::Succeeded);
        assert_eq!(run.outputs["artifact"], "orchestrator-data:cringe dad joke");
        let workspace = root.map(Path::to_path_buf).unwrap_or_else(|| {
            fixture
                .storage
                .root_path()
                .join("runs")
                .join(&run.id)
                .join("workspace")
        });
        for name in ["source.txt", "prepared.txt", "artifact.txt"] {
            assert!(
                workspace.join(name).is_file(),
                "missing {name} in {}",
                workspace.display()
            );
        }
        if !selected {
            assert!(
                run.inputs["root_directory"].is_null(),
                "resolved workspace must not change the persisted invocation contract"
            );
        }
    }
}

#[tokio::test]
async fn concurrent_runs_use_separate_managed_workspaces() {
    let fixture = Fixture::new(".");
    let (first, second) = tokio::join!(
        execute(fixture.plan.clone(), fixture.config(None)),
        execute(fixture.plan.clone(), fixture.config(None)),
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_ne!(first.id, second.id);
    for run in [first, second] {
        assert_eq!(run.status, RunStatus::Succeeded);
        assert!(
            fixture
                .storage
                .root_path()
                .join("runs")
                .join(run.id)
                .join("workspace/artifact.txt")
                .is_file()
        );
    }
}

#[tokio::test]
async fn explicit_subprocess_working_directory_is_preserved() {
    let mut fixture = Fixture::new(".");
    let explicit = tempfile::tempdir().unwrap();
    fixture.catalog =
        ToolCatalog::new(vec![seed_tool(Some(explicit.path().display().to_string()))]);
    fixture.plan.steps.truncate(1);
    fixture.plan.outputs.clear();
    let run = execute(fixture.plan.clone(), fixture.config(None))
        .await
        .unwrap();
    assert!(explicit.path().join("source.txt").is_file());
    assert!(
        !fixture
            .storage
            .root_path()
            .join("runs")
            .join(run.id)
            .join("workspace/source.txt")
            .exists()
    );
}

#[tokio::test]
async fn relative_subprocess_working_directory_resolves_inside_the_selected_workspace() {
    let mut fixture = Fixture::new(".");
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    fixture.catalog = ToolCatalog::new(vec![seed_tool(Some("nested".to_owned()))]);
    fixture.plan.steps.truncate(1);
    fixture.plan.outputs.clear();
    let run = execute(fixture.plan.clone(), fixture.config(Some(root.path())))
        .await
        .unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);
    assert!(nested.join("source.txt").is_file());
    assert!(!root.path().join("source.txt").exists());
}

#[tokio::test]
async fn code_working_directory_can_derive_a_subdirectory_from_the_managed_root() {
    let mut fixture = Fixture::new(".");
    fixture.plan.steps.truncate(2);
    fixture.plan.outputs.clear();
    let StepConfig::CodeCall(config) = &mut fixture.plan.steps[1].config else {
        panic!("code step")
    };
    config.inline = Some("mkdir nested; printf '{\"path\":\"nested\"}'".to_owned());
    fixture.plan.steps.push(step(
        "nested",
        StepConfig::CodeCall(CodeCallConfig {
            language: "sh".to_owned(),
            inline: Some("pwd".to_owned()),
            file: None,
            args: vec![],
            stdin: None,
            env: Default::default(),
            working_dir: Some("${input.root_directory}/nested".to_owned()),
            timeout_secs: Some(5),
        }),
        &["prepare"],
        "path",
    ));
    let run = execute(fixture.plan.clone(), fixture.config(None))
        .await
        .unwrap();
    let nested = fixture
        .storage
        .root_path()
        .join("runs")
        .join(run.id)
        .join("workspace/nested")
        .canonicalize()
        .unwrap();
    assert_eq!(
        run.step_runs["nested"].outputs["path"]
            .as_str()
            .unwrap()
            .trim(),
        nested.to_str().unwrap()
    );
}

#[tokio::test]
async fn http_tools_do_not_require_a_local_workspace() {
    let mut fixture = Fixture::new(".");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 8192];
        let received = stream.read(&mut request).await.unwrap();
        assert!(received > 0);
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"path\":\"done\"}").await.unwrap();
    });
    fixture.catalog = ToolCatalog::new(vec![ToolEntry {
        name: "seed".to_owned(),
        description: "Remote operation".to_owned(),
        config: ToolConfig::Http(inxm_local::tools::catalog::HttpConfig {
            base_url: url,
            method: "GET".to_owned(),
            path_template: String::new(),
            headers: Default::default(),
            timeout_secs: Some(5),
        }),
        input_schema: json!({"type":"object"}),
        output_schema: json!({"type":"object"}),
        allowlisted: true,
        timeout_secs: Some(5),
    }]);
    fixture.plan.steps.truncate(1);
    fixture.plan.outputs.clear();
    let missing = fixture.storage.root_path().join("not-a-local-workspace");
    let run = execute(fixture.plan.clone(), fixture.config(Some(&missing)))
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(run.step_runs["seed"].outputs["path"], "done");
    assert!(!missing.exists());
}

#[tokio::test]
async fn explicit_mcp_allowed_directory_is_not_rewritten() {
    let allowed = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(&allowed.path().display().to_string());
    let error = execute(fixture.plan.clone(), fixture.config(None))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("workspace is outside allowed directory"),
        "{error}"
    );
    let ToolConfig::Mcp(config) = &fixture.catalog.get("read-artifact").unwrap().config else {
        panic!("MCP reader")
    };
    let McpTransport::Stdio { server_args, .. } = &config.transport else {
        panic!("stdio reader")
    };
    assert_eq!(
        server_args.last().unwrap(),
        &allowed.path().display().to_string()
    );
}

#[tokio::test]
async fn human_resume_reuses_intermediate_files_in_the_same_workspace() {
    let mut fixture = Fixture::new(".");
    fixture.plan.steps.insert(
        2,
        step(
            "pause",
            StepConfig::HumanInteraction(HumanInteractionConfig {
                prompt: "Continue?".to_owned(),
                response_field: "approved".to_owned(),
                approval_required: true,
            }),
            &["prepare"],
            "approved",
        ),
    );
    fixture.plan.steps[3].depends_on = vec!["pause".to_owned()];
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<inxm_local::executor::HumanRequest>();
    let pause = tokio::spawn(async move {
        drop(rx.recv().await.unwrap().respond);
    });
    let mut config = fixture.config(None);
    config.human = Some(tx);
    let paused = execute(fixture.plan.clone(), config).await.unwrap();
    pause.await.unwrap();
    assert!(matches!(paused.status, RunStatus::WaitingForHuman { .. }));
    let original_started = paused.step_runs["prepare"].started_at;
    let run_id = paused.id.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<inxm_local::executor::HumanRequest>();
    let approve = tokio::spawn(async move {
        rx.recv()
            .await
            .unwrap()
            .respond
            .send(HumanDecision::Approve)
            .unwrap();
    });
    let mut config = fixture.config(None);
    config.human = Some(tx);
    let resumed = resume(fixture.plan.clone(), config, paused).await.unwrap();
    approve.await.unwrap();
    assert_eq!(resumed.id, run_id);
    assert_eq!(resumed.status, RunStatus::Succeeded);
    assert_eq!(resumed.step_runs["prepare"].started_at, original_started);
    assert_eq!(
        resumed.outputs["artifact"],
        "orchestrator-data:cringe dad joke"
    );
}

#[tokio::test]
async fn repair_resume_rejects_workspace_change_after_an_implicit_file_producer_succeeds() {
    for managed in [false, true] {
        let mut fixture = Fixture::new(".");
        let original = tempfile::tempdir().unwrap();
        let replacement = tempfile::tempdir().unwrap();
        fixture.plan.steps.truncate(2);
        fixture.plan.outputs.clear();
        let StepConfig::CodeCall(config) = &mut fixture.plan.steps[1].config else {
            panic!("code step")
        };
        config.inline = Some("test -f ready && cat source.txt".to_owned());
        let root = (!managed).then_some(original.path());

        assert!(
            execute(fixture.plan.clone(), fixture.config(root))
                .await
                .is_err()
        );
        let run_id = fixture.storage.runs().list().unwrap()[0].id.clone();
        let failed = fixture.storage.runs().load(&run_id).unwrap();
        let workspace = root.map(Path::to_path_buf).unwrap_or_else(|| {
            fixture
                .storage
                .root_path()
                .join("runs")
                .join(&run_id)
                .join("workspace")
        });
        std::fs::write(replacement.path().join("source.txt"), "unrelated artifact").unwrap();
        std::fs::write(replacement.path().join("ready"), "").unwrap();
        let overrides = IndexMap::from([("root_directory".to_owned(), json!(replacement.path()))]);

        let error = resume_from_repair(
            fixture.plan.clone(),
            fixture.config(root),
            failed.clone(),
            overrides,
            RepairResumeMode::WorldFixed,
        )
        .await
        .unwrap_err();

        assert!(
            error.to_string().contains("root_directory") && error.to_string().contains("seed"),
            "{error}"
        );
        assert_eq!(fixture.storage.runs().load(&run_id).unwrap(), failed);

        std::fs::write(workspace.join("ready"), "").unwrap();
        let resumed = resume_from_repair(
            fixture.plan.clone(),
            fixture.config(root),
            failed.clone(),
            IndexMap::new(),
            RepairResumeMode::WorldFixed,
        )
        .await
        .unwrap();
        assert_eq!(resumed.status, RunStatus::Succeeded);
        assert_eq!(
            resumed.step_runs["seed"].started_at,
            failed.step_runs["seed"].started_at
        );
        assert_eq!(
            resumed.step_runs["prepare"].stdout.as_deref(),
            Some("orchestrator-data:")
        );
    }
}

#[tokio::test]
async fn repair_resume_protects_implicit_code_agent_and_stdio_mcp_workspace_use() {
    for producer in [
        StepConfig::CodeCall(CodeCallConfig {
            language: "sh".to_owned(),
            inline: Some("printf data > artifact.txt".to_owned()),
            file: None,
            args: vec![],
            stdin: None,
            env: Default::default(),
            working_dir: None,
            timeout_secs: None,
        }),
        StepConfig::AgentCall(AgentCallConfig {
            objective: "Write artifact.txt".to_owned(),
            working_dir: "${input.root_directory}".to_owned(),
            timeout_secs: None,
        }),
        StepConfig::ToolCall(ToolCallConfig {
            tool: "read-artifact".to_owned(),
            arguments: IndexMap::from([("path".to_owned(), json!("artifact.txt"))]),
        }),
    ] {
        let mut fixture = Fixture::new(".");
        fixture.plan.steps.truncate(2);
        fixture.plan.outputs.clear();
        fixture.plan.steps[0].config = producer;
        let mut run = Run::new(fixture.plan.metadata.id.clone(), 1);
        run.inputs = fixture.plan.resolve_inputs(&IndexMap::new()).unwrap();
        run.status = RunStatus::Failed {
            failed_step_id: "prepare".to_owned(),
            message: "fixture failure".to_owned(),
        };
        let mut seed = StepRun::new("seed");
        seed.status = StepRunStatus::Succeeded;
        let mut prepare = StepRun::new("prepare");
        prepare.status = StepRunStatus::Failed;
        run.step_runs.insert("seed".to_owned(), seed);
        run.step_runs.insert("prepare".to_owned(), prepare);
        fixture.storage.runs().save(&run).unwrap();
        let replacement = tempfile::tempdir().unwrap();

        let error = resume_from_repair(
            fixture.plan.clone(),
            fixture.config(None),
            run.clone(),
            IndexMap::from([("root_directory".to_owned(), json!(replacement.path()))]),
            RepairResumeMode::WorldFixed,
        )
        .await
        .unwrap_err();

        assert!(
            error.to_string().contains("root_directory") && error.to_string().contains("seed"),
            "{error}"
        );
        assert_eq!(fixture.storage.runs().load(&run.id).unwrap(), run);
    }
}

#[tokio::test]
async fn repair_resume_protects_successful_fan_out_files_even_when_the_owner_is_reset() {
    let mut fixture = Fixture::new(".");
    fixture.plan.steps.truncate(1);
    fixture.plan.outputs.clear();
    fixture.plan.steps.insert(
        0,
        step(
            "fan",
            StepConfig::FanOut(FanOutConfig {
                over: "source.items".to_owned(),
                item_var: "item".to_owned(),
                spawn_steps: vec!["seed".to_owned()],
                until: None,
            }),
            &[],
            "results",
        ),
    );
    let mut run = Run::new(fixture.plan.metadata.id.clone(), 1);
    run.inputs = fixture.plan.resolve_inputs(&IndexMap::new()).unwrap();
    run.status = RunStatus::Failed {
        failed_step_id: "fan".to_owned(),
        message: "second iteration failed".to_owned(),
    };
    let mut owner = StepRun::new("fan");
    owner.status = StepRunStatus::Failed;
    let mut child = StepRun::new("seed");
    child.status = StepRunStatus::Failed;
    child.iterations.push(StepRunIteration {
        iteration: 0,
        status: StepRunStatus::Succeeded,
        started_at: chrono::Utc::now(),
        finished_at: chrono::Utc::now(),
        duration_ms: 1,
        outputs: Default::default(),
        stdout: None,
        stderr: None,
        error: None,
        token_usage: None,
    });
    run.step_runs.insert("fan".to_owned(), owner);
    run.step_runs.insert("seed".to_owned(), child);
    fixture.storage.runs().save(&run).unwrap();
    let replacement = tempfile::tempdir().unwrap();

    let error = resume_from_repair(
        fixture.plan.clone(),
        fixture.config(None),
        run.clone(),
        IndexMap::from([("root_directory".to_owned(), json!(replacement.path()))]),
        RepairResumeMode::WorldFixed,
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("seed"), "{error}");
    assert_eq!(fixture.storage.runs().load(&run.id).unwrap(), run);
}

#[tokio::test]
async fn repair_resume_allows_workspace_change_before_local_work_has_succeeded() {
    let mut fixture = Fixture::new(".");
    fixture.plan.steps.truncate(1);
    fixture.plan.outputs.clear();
    let mut run = Run::new(fixture.plan.metadata.id.clone(), 1);
    run.inputs = fixture.plan.resolve_inputs(&IndexMap::new()).unwrap();
    run.status = RunStatus::Failed {
        failed_step_id: "seed".to_owned(),
        message: "failed before writing".to_owned(),
    };
    let mut failed = StepRun::new("seed");
    failed.status = StepRunStatus::Failed;
    run.step_runs.insert("seed".to_owned(), failed);
    let replacement = tempfile::tempdir().unwrap();

    let resumed = resume_from_repair(
        fixture.plan.clone(),
        fixture.config(None),
        run,
        IndexMap::from([("root_directory".to_owned(), json!(replacement.path()))]),
        RepairResumeMode::WorldFixed,
    )
    .await
    .unwrap();

    assert_eq!(resumed.status, RunStatus::Succeeded);
    assert!(replacement.path().join("source.txt").is_file());
}

#[tokio::test]
async fn repair_resume_allows_workspace_change_after_only_remote_work_succeeded() {
    use inxm_local::tools::catalog::{HttpConfig, McpAuth};

    for remote in [
        ToolConfig::Http(HttpConfig {
            base_url: "https://example.com".to_owned(),
            method: "GET".to_owned(),
            path_template: String::new(),
            headers: Default::default(),
            timeout_secs: None,
        }),
        ToolConfig::Mcp(McpConfig {
            tool_name: "remote".to_owned(),
            transport: McpTransport::StreamableHttp {
                endpoint: "https://example.com/mcp".to_owned(),
                auth: McpAuth::None,
            },
        }),
    ] {
        let mut fixture = Fixture::new(".");
        fixture.plan.steps.truncate(2);
        fixture.plan.outputs.clear();
        let mut tool = seed_tool(None);
        tool.config = remote;
        fixture.catalog = ToolCatalog::new(vec![tool]);
        let StepConfig::CodeCall(code) = &mut fixture.plan.steps[1].config else {
            panic!("code step")
        };
        code.inline = Some("printf resumed > result.txt".to_owned());
        let mut run = Run::new(fixture.plan.metadata.id.clone(), 1);
        run.inputs = fixture.plan.resolve_inputs(&IndexMap::new()).unwrap();
        run.status = RunStatus::Failed {
            failed_step_id: "prepare".to_owned(),
            message: "local step failed before writing".to_owned(),
        };
        let mut remote = StepRun::new("seed");
        remote.status = StepRunStatus::Succeeded;
        remote.outputs.insert("path".to_owned(), json!("remote"));
        let mut failed = StepRun::new("prepare");
        failed.status = StepRunStatus::Failed;
        run.step_runs.insert("seed".to_owned(), remote);
        run.step_runs.insert("prepare".to_owned(), failed);
        let replacement = tempfile::tempdir().unwrap();

        let resumed = resume_from_repair(
            fixture.plan.clone(),
            fixture.config(None),
            run,
            IndexMap::from([("root_directory".to_owned(), json!(replacement.path()))]),
            RepairResumeMode::WorldFixed,
        )
        .await
        .unwrap();

        assert_eq!(resumed.status, RunStatus::Succeeded);
        assert_eq!(
            std::fs::read_to_string(replacement.path().join("result.txt")).unwrap(),
            "resumed"
        );
    }
}
