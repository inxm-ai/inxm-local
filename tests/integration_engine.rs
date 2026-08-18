//! End-to-end tests for the app engine bridge: commands in, events out,
//! including live step progress and the chat-routed human-interaction path.
//! Runs headless — an `egui::Context` works without a window.

use std::time::{Duration, Instant};

use inxm_local::app::engine::{
    self, AppSettings, BackendChoice, DataPaths, EngineCommand, EngineEvent, RoutedEngineEvent,
};
use inxm_local::executor::{HumanDecision, RunStatus, StepRunStatus};
use inxm_local::plan::types::{
    HumanInteractionConfig, Plan, PlanMetadata, PlanStep, StepConfig, ToolCallConfig,
};
use inxm_local::storage::StorageRoot;

const EVENT_TIMEOUT: Duration = Duration::from_secs(15);
const DOSSIER_INTENT: &str = "Build a reusable executive research dossier: accept an article-listing URL (the page containing the article links, not a general site homepage), and optional path prefix to filter articles under that path, a plain-language research topic (not another URL), maximum article count, output path, and root directory; fetch the listing; deterministically resolve, filter, deduplicate, and cap same-origin HTML article links under the listing URL's path prefix while rejecting site navigation, images, stylesheets, scripts, feeds, and other static assets; fan out to fetch each article and produce a compact evidence-backed summary with its source URL and risk signals; synthesize the summaries into a cross-source brief without sending raw pages to the final model call; ask for approval; then branch so approval writes the brief to disk while rejection emits a cancellation receipt";

fn echo_step(id: &str, message: &str, deps: &[&str]) -> PlanStep {
    let mut arguments = indexmap::IndexMap::new();
    arguments.insert(
        "message".to_owned(),
        serde_json::Value::String(message.to_owned()),
    );
    PlanStep {
        id: id.to_owned(),
        name: format!("step {id}"),
        description: None,
        config: StepConfig::ToolCall(ToolCallConfig {
            tool: "echo".to_owned(),
            arguments,
        }),
        depends_on: deps.iter().map(|s| (*s).to_owned()).collect(),
        outputs: vec![],
        timeout_secs: None,
        retry: None,
    }
}

fn plan_with(steps: Vec<PlanStep>) -> Plan {
    Plan {
        metadata: PlanMetadata::new(Some("test intent".to_owned())),
        name: "engine-test-plan".to_owned(),
        description: None,
        inputs: vec![],
        config: Default::default(),
        steps,
        outputs: vec![],
    }
}

struct Harness {
    handle: engine::EngineHandle,
    events: std::sync::mpsc::Receiver<RoutedEngineEvent>,
    paths: DataPaths,
    _tmp: tempfile::TempDir,
}

fn start_engine() -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = DataPaths::at(tmp.path().to_path_buf());
    let (handle, events) = engine::spawn(egui::Context::default(), paths.clone());
    handle.send(EngineCommand::Bootstrap);
    Harness {
        handle,
        events,
        paths,
        _tmp: tmp,
    }
}

fn start_engine_with_agent_calls_enabled() -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = DataPaths::at(tmp.path().to_path_buf());
    AppSettings {
        backend: BackendChoice::Codex,
        experimental_agent_calls: true,
        ..AppSettings::default()
    }
    .save(&paths.settings_path)
    .expect("save engine settings");
    let (handle, events) = engine::spawn(egui::Context::default(), paths.clone());
    handle.send(EngineCommand::Bootstrap);
    Harness {
        handle,
        events,
        paths,
        _tmp: tmp,
    }
}

#[derive(Clone)]
struct PlanningServerState {
    call_index: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

async fn planning_server_handler(
    axum::extract::State(state): axum::extract::State<PlanningServerState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    use std::sync::atomic::Ordering;

    let call_index = state.call_index.fetch_add(1, Ordering::SeqCst);
    let system = body["messages"][0]["content"].as_str().unwrap_or_default();
    let user = body["messages"][1]["content"].as_str().unwrap_or_default();
    let content = match call_index {
        0 => {
            assert!(system.contains("define it in `spec.inputs`"));
            assert!(user.contains(DOSSIER_INTENT));
            serde_json::json!({
                "confidence": 0.96,
                "needs_clarification": false,
                "question": null,
                "spec": {
                    "desired_outcome": "Create a reusable, schedulable executive research dossier with one approval decision.",
                    "acceptance_criteria": [
                        "Listing and article processing is deterministic and bounded.",
                        "Only the final approve or reject decision pauses for a human."
                    ],
                    "inputs": [
                        {"name":"listing_url","description":"Article-listing page URL","value_type":"string","required":true,"default":null},
                        {"name":"path_prefix","description":"Optional article path filter","value_type":"string","required":false,"default":null},
                        {"name":"research_topic","description":"Plain-language research topic","value_type":"string","required":true,"default":null},
                        {"name":"maximum_article_count","description":"Maximum articles to process","value_type":"integer","required":true,"default":null},
                        {"name":"output_path","description":"Brief output path","value_type":"string","required":true,"default":null},
                        {"name":"root_directory","description":"Working root directory","value_type":"string","required":true,"default":null}
                    ]
                }
            })
            .to_string()
        }
        1 => {
            assert!(system.contains("never add a \"human\" step to collect"));
            for input_name in [
                "listing_url",
                "path_prefix",
                "research_topic",
                "maximum_article_count",
                "output_path",
                "root_directory",
            ] {
                assert!(
                    user.contains(input_name),
                    "design prompt missing {input_name}"
                );
            }
            serde_json::json!({
                "title": "Executive research dossier",
                "summary": "Consume invocation inputs, process bounded article evidence, request one approval, and emit the selected receipt.",
                "recommended_tools": [{"name":"http-get","reason":"Fetch listing and article HTML"}],
                "execution_outline": [
                    {"name":"Fetch and filter listing","step_kind":"code_call","description":"Use the declared inputs to resolve a bounded article list."},
                    {"name":"Summarize articles","step_kind":"fan_out","description":"Fetch and summarize each article into compact evidence."},
                    {"name":"Synthesize brief","step_kind":"prompt_call","description":"Combine compact summaries without raw pages."},
                    {"name":"Approve or reject","step_kind":"human","description":"Ask for the sole runtime human decision."},
                    {"name":"Emit result","step_kind":"condition","description":"Write the approved brief or emit a cancellation receipt."}
                ]
            })
            .to_string()
        }
        2 => {
            assert!(system.contains("Do not use HUMAN_INTERACTION for"));
            assert!(user.contains("Invocation inputs (available before execution"));
            for input_name in [
                "listing_url",
                "path_prefix",
                "research_topic",
                "maximum_article_count",
                "output_path",
                "root_directory",
            ] {
                assert!(
                    user.contains(input_name),
                    "compile prompt missing {input_name}"
                );
            }
            serde_json::json!({
                "name": "Executive research dossier",
                "description": "Input-aware dossier regression artifact",
                "inputs": [
                    {"name":"listing_url","description":"Article-listing page URL","value_type":"string","required":true,"default":null},
                    {"name":"path_prefix","description":"Optional article path filter","value_type":"string","required":false,"default":null},
                    {"name":"research_topic","description":"Plain-language research topic","value_type":"string","required":true,"default":null},
                    {"name":"maximum_article_count","description":"Maximum articles to process","value_type":"integer","required":true,"default":null},
                    {"name":"output_path","description":"Brief output path","value_type":"string","required":true,"default":null},
                    {"name":"root_directory","description":"Working root directory","value_type":"string","required":true,"default":null}
                ],
                "config": {},
                "steps": [{
                    "id": "approve_dossier",
                    "name": "Approve or reject dossier",
                    "description": "The only human interaction in the plan",
                    "depends_on": [],
                    "outputs": [{"name":"decision","description":"Approval decision","value_type":"string"}],
                    "timeout_secs": null,
                    "retry": null,
                    "config": {
                        "type": "HUMAN_INTERACTION",
                        "prompt": "Approve the dossier? Reply approve or reject.",
                        "response_field": "decision",
                        "approval_required": false
                    }
                }],
                "outputs": [{"name":"decision","description":"Final approval decision","source":"${step.approve_dossier.decision}"}]
            })
            .to_string()
        }
        _ => panic!("unexpected compiler request {call_index}"),
    };

    axum::Json(serde_json::json!({
        "choices": [{"message": {"content": content}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 100}
    }))
}

/// Receive events until `pick` returns `Some`, panicking on timeout.
fn wait_for<T>(
    events: &std::sync::mpsc::Receiver<RoutedEngineEvent>,
    what: &str,
    mut pick: impl FnMut(EngineEvent) -> Option<T>,
) -> T {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for {what}"));
        match events.recv_timeout(remaining) {
            Ok(routed) => {
                let event = routed.event;
                if let EngineEvent::Failure(message) = &event {
                    panic!("engine failure while waiting for {what}: {message}");
                }
                if let Some(value) = pick(event) {
                    return value;
                }
            }
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

#[test]
fn bootstrap_seeds_catalog_and_reports_ready() {
    let harness = start_engine();

    let data_dir = wait_for(&harness.events, "Ready", |e| match e {
        EngineEvent::Ready { data_dir, .. } => Some(data_dir),
        _ => None,
    });
    assert_eq!(data_dir, harness.paths.data_dir.display().to_string());

    let tools = wait_for(&harness.events, "Catalog", |e| match e {
        EngineEvent::Catalog(tools) => Some(tools),
        _ => None,
    });
    assert!(
        tools.iter().any(|t| t.name == "echo"),
        "default catalog should contain the echo tool"
    );
    assert!(harness.paths.catalog_path.exists());
}

#[test]
fn checked_in_dogfooding_bundles_import_through_the_app_engine() {
    // These bundles contain AGENT_CALL steps (Codex/Claude Code does the actual
    // implementation and repair work), so importing them requires the
    // experimental agent-steps setting to already be enabled — see
    // `dogfooding_bundles_are_refused_without_experimental_agent_calls` below
    // for the disabled-by-default case a fresh install starts from.
    let harness = start_engine_with_agent_calls_enabled();
    wait_for(&harness.events, "Ready", |event| match event {
        EngineEvent::Ready { .. } => Some(()),
        _ => None,
    });

    for slug in ["prompt-to-plan", "feature-development", "bugfix"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("examples/dogfooding")
            .join(format!("{slug}.plan.json"));
        harness.handle.send(EngineCommand::ImportPlan { path });
        let message = wait_for(
            &harness.events,
            "successful plan import",
            |event| match event {
                EngineEvent::Assistant(message) if message.starts_with("Imported") => Some(message),
                _ => None,
            },
        );
        assert!(message.contains('“') && message.contains('”'));
    }

    let imported = StorageRoot::open(&harness.paths.data_dir)
        .expect("open imported plan storage")
        .plans()
        .list()
        .expect("list imported plans");
    assert_eq!(imported.len(), 3);
}

#[test]
fn dogfooding_bundles_are_refused_without_experimental_agent_calls() {
    let harness = start_engine();
    wait_for(&harness.events, "Ready", |event| match event {
        EngineEvent::Ready { .. } => Some(()),
        _ => None,
    });

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/dogfooding")
        .join("bugfix.plan.json");
    harness.handle.send(EngineCommand::ImportPlan { path });
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for import refusal"));
        let routed = harness
            .events
            .recv_timeout(remaining)
            .unwrap_or_else(|_| panic!("timed out waiting for import refusal"));
        if let EngineEvent::Failure(message) = routed.event {
            assert!(message.contains("Experimental agent steps"));
            assert!(message.contains("AGENT_CALL"));
            break;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dossier_guided_flow_carries_inputs_and_compiles_with_approval_only_hitl() {
    use inxm_local::compiler::SpecTurn;
    use std::sync::atomic::Ordering;

    let state = PlanningServerState {
        call_index: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/v1/chat/completions",
            axum::routing::post(planning_server_handler),
        )
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = DataPaths::at(tmp.path().to_path_buf());
    AppSettings {
        backend: BackendChoice::OpenAiCompatible,
        model: "planning-regression".to_owned(),
        api_base: format!("http://{address}/v1"),
        ..AppSettings::default()
    }
    .save(&paths.settings_path)
    .unwrap();
    let (handle, events) = engine::spawn(egui::Context::default(), paths);
    handle.send(EngineCommand::Bootstrap);
    wait_for(&events, "Ready", |event| match event {
        EngineEvent::Ready { .. } => Some(()),
        _ => None,
    });

    let conversation = vec![SpecTurn {
        role: "user".to_owned(),
        content: DOSSIER_INTENT.to_owned(),
    }];
    handle.send(EngineCommand::AssessIntent {
        intent: DOSSIER_INTENT.to_owned(),
        conversation: conversation.clone(),
    });
    let assessment = wait_for(&events, "AssessmentReady", |event| match event {
        EngineEvent::AssessmentReady { assessment } => Some(*assessment),
        _ => None,
    });
    assert!(!assessment.needs_clarification);
    assert_eq!(assessment.spec.inputs.len(), 6);

    handle.send(EngineCommand::GenerateDesign {
        spec: assessment.spec.clone(),
        conversation: conversation.clone(),
        previous_design: None,
        feedback: None,
    });
    let design = wait_for(&events, "DesignReady", |event| match event {
        EngineEvent::DesignReady { design } => Some(*design),
        _ => None,
    });
    assert_eq!(
        design
            .execution_outline
            .iter()
            .filter(|step| step.step_kind == "human")
            .count(),
        1
    );

    handle.send(EngineCommand::CompileFromSpec {
        intent: DOSSIER_INTENT.to_owned(),
        spec: assessment.spec,
        design: Some(Box::new(design)),
        conversation,
    });
    let plan = wait_for(&events, "PlanCompiled", |event| match event {
        EngineEvent::PlanCompiled { plan } => Some(*plan),
        _ => None,
    });

    let input_names = plan
        .inputs
        .iter()
        .map(|input| input.name.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        input_names,
        std::collections::BTreeSet::from([
            "listing_url",
            "path_prefix",
            "research_topic",
            "maximum_article_count",
            "output_path",
            "root_directory",
        ])
    );
    let human_steps = plan
        .steps
        .iter()
        .filter_map(|step| match &step.config {
            StepConfig::HumanInteraction(config) => Some(config),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(human_steps.len(), 1);
    assert!(human_steps[0].prompt.contains("Approve"));
    assert_eq!(state.call_index.load(Ordering::SeqCst), 3);

    server.abort();
}

#[test]
fn scoped_commands_keep_their_originating_chat() {
    let harness = start_engine();
    wait_for(&harness.events, "Ready", |e| match e {
        EngineEvent::Ready { .. } => Some(()),
        _ => None,
    });

    harness.handle.send_from("chat-a", EngineCommand::ListPlans);
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let routed = harness
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("scoped PlanList event");
        if matches!(routed.event, EngineEvent::PlanList(_))
            && routed.session_id.as_deref() == Some("chat-a")
        {
            break;
        }
    }
}

#[test]
fn run_plan_streams_progress_and_finishes() {
    let harness = start_engine();
    wait_for(&harness.events, "Ready", |e| match e {
        EngineEvent::Ready { .. } => Some(()),
        _ => None,
    });

    // Store a two-step plan directly, as the compiler would.
    let plan = plan_with(vec![
        echo_step("first", "hello", &[]),
        echo_step("second", "world", &["first"]),
    ]);
    let plan_id = plan.metadata.id.clone();
    StorageRoot::open(&harness.paths.data_dir)
        .unwrap()
        .plans()
        .save(&plan)
        .unwrap();

    harness.handle.send(EngineCommand::RunPlan {
        plan_ref: plan_id.clone(),
        inputs: Default::default(),
    });

    let started_plan_id = wait_for(&harness.events, "RunStarted", |e| match e {
        EngineEvent::RunStarted { plan, .. } => Some(plan.metadata.id.clone()),
        _ => None,
    });
    assert_eq!(started_plan_id, plan_id);

    let saw_running = wait_for(&harness.events, "step Running progress", |e| match e {
        EngineEvent::StepProgress(p) if p.status == StepRunStatus::Running => Some(p.step_id),
        _ => None,
    });
    assert_eq!(saw_running, "first");

    let run = wait_for(&harness.events, "RunFinished", |e| match e {
        EngineEvent::RunFinished { run } => Some(run),
        _ => None,
    });
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(run.step_runs.len(), 2);
    assert!(
        run.step_runs
            .values()
            .all(|sr| sr.status == StepRunStatus::Succeeded)
    );
}

#[test]
fn human_interaction_routes_through_channel() {
    let harness = start_engine();
    wait_for(&harness.events, "Ready", |e| match e {
        EngineEvent::Ready { .. } => Some(()),
        _ => None,
    });

    let plan = plan_with(vec![
        PlanStep {
            id: "ask".to_owned(),
            name: "ask the operator".to_owned(),
            description: None,
            config: StepConfig::HumanInteraction(HumanInteractionConfig {
                prompt: "Proceed?".to_owned(),
                response_field: "answer".to_owned(),
                approval_required: true,
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        },
        echo_step("after", "done", &["ask"]),
    ]);
    let plan_id = plan.metadata.id.clone();
    StorageRoot::open(&harness.paths.data_dir)
        .unwrap()
        .plans()
        .save(&plan)
        .unwrap();

    harness.handle.send(EngineCommand::RunPlan {
        plan_ref: plan_id,
        inputs: Default::default(),
    });

    let request = wait_for(&harness.events, "HumanNeeded", |e| match e {
        EngineEvent::HumanNeeded { request, .. } => Some(request),
        _ => None,
    });
    assert_eq!(request.prompt, "Proceed?");
    assert!(request.approval_required);
    request
        .respond
        .send(HumanDecision::Approve)
        .expect("executor should be waiting for the decision");

    let run = wait_for(&harness.events, "RunFinished", |e| match e {
        EngineEvent::RunFinished { run } => Some(run),
        _ => None,
    });
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(
        run.step_runs["ask"].outputs["answer"],
        serde_json::Value::String("approved".to_owned())
    );
}

#[test]
fn save_and_delete_tool_persist_catalog() {
    let harness = start_engine();
    // Skip past bootstrap events.
    wait_for(&harness.events, "Catalog", |e| match e {
        EngineEvent::Catalog(_) => Some(()),
        _ => None,
    });

    let entry = inxm_local::tools::catalog::ToolEntry {
        name: "date".to_owned(),
        description: "Prints the date".to_owned(),
        config: inxm_local::tools::catalog::ToolConfig::Subprocess(
            inxm_local::tools::catalog::SubprocessConfig {
                command: "date".to_owned(),
                args: vec![],
                env: Default::default(),
                working_dir: None,
            },
        ),
        input_schema: serde_json::json!({ "type": "object" }),
        output_schema: serde_json::json!({ "type": "object" }),
        allowlisted: true,
        timeout_secs: None,
    };
    harness.handle.send(EngineCommand::SaveTool {
        entry: Box::new(entry),
    });

    let tools = wait_for(&harness.events, "Catalog after save", |e| match e {
        EngineEvent::Catalog(tools) if tools.iter().any(|t| t.name == "date") => Some(tools),
        _ => None,
    });
    assert!(tools.iter().any(|t| t.name == "echo"), "echo kept");

    // The file on disk reflects the change.
    let yaml = std::fs::read_to_string(&harness.paths.catalog_path).unwrap();
    assert!(yaml.contains("name: date"));

    harness.handle.send(EngineCommand::DeleteTool {
        name: "date".to_owned(),
    });
    wait_for(&harness.events, "Catalog after delete", |e| match e {
        EngineEvent::Catalog(tools) if !tools.iter().any(|t| t.name == "date") => Some(()),
        _ => None,
    });
    let yaml = std::fs::read_to_string(&harness.paths.catalog_path).unwrap();
    assert!(!yaml.contains("name: date"));
}
