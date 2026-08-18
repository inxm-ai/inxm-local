//! Opt-in live compiler checks. These require an authenticated Codex CLI and
//! are ignored by the normal test suite.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use inxm_local::app::engine::{
    self, AppSettings, BackendChoice, DataPaths, EngineCommand, EngineEvent, RoutedEngineEvent,
};
use inxm_local::compiler::SpecTurn;
use inxm_local::plan::types::{InputKind, Plan, StepConfig};

// One compile command may include a deterministic validation correction, so
// the harness must outlive two bounded 600-second compiler calls.
const LIVE_EVENT_TIMEOUT: Duration = Duration::from_secs(1_300);
const DOSSIER_INTENT: &str = "Build a reusable executive research dossier: accept an article-listing URL (the page containing the article links, not a general site homepage), and optional path prefix to filter articles under that path, a plain-language research topic (not another URL), maximum article count, output path, and root directory; fetch the listing; deterministically resolve, filter, deduplicate, and cap same-origin HTML article links under the listing URL's path prefix while rejecting site navigation, images, stylesheets, scripts, feeds, and other static assets; fan out to fetch each article and produce a compact evidence-backed summary with its source URL and risk signals; synthesize the summaries into a cross-source brief without sending raw pages to the final model call; ask for approval; then branch so approval writes the brief to disk while rejection emits a cancellation receipt";
const EXPECTED_INPUT_COUNT: usize = 6;
const PROMPT_TO_PLAN_INTENT: &str = include_str!("../examples/dogfooding/prompt-to-plan.md");
const FEATURE_DEVELOPMENT_INTENT: &str =
    include_str!("../examples/dogfooding/feature-development.md");
const BUGFIX_INTENT: &str = include_str!("../examples/dogfooding/bugfix.md");

fn wait_for<T>(
    events: &std::sync::mpsc::Receiver<RoutedEngineEvent>,
    what: &str,
    mut pick: impl FnMut(EngineEvent) -> Option<T>,
) -> T {
    let deadline = Instant::now() + LIVE_EVENT_TIMEOUT;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for {what}"));
        match events.recv_timeout(remaining) {
            Ok(routed) => {
                if let EngineEvent::Failure(message) = &routed.event {
                    panic!("live planning failure while waiting for {what}: {message}");
                }
                if let Some(value) = pick(routed.event) {
                    return value;
                }
            }
            Err(_) => panic!("timed out waiting for {what}"),
        }
    }
}

fn compile_live(intent: &str) -> Plan {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = DataPaths::at(tmp.path().to_path_buf());
    AppSettings {
        backend: BackendChoice::Codex,
        // The dogfooding intents ask Codex/Claude Code to do real
        // implementation and repair work, so AGENT_CALL must be part of
        // this compile request's capability allowlist — see
        // `AppSettings::supports_agent_call`.
        experimental_agent_calls: true,
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
        content: intent.to_owned(),
    }];
    handle.send(EngineCommand::AssessIntent {
        intent: intent.to_owned(),
        conversation: conversation.clone(),
    });
    let assessment = wait_for(&events, "AssessmentReady", |event| match event {
        EngineEvent::AssessmentReady { assessment } => Some(*assessment),
        _ => None,
    });
    assert!(!assessment.needs_clarification, "{assessment:?}");

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

    handle.send(EngineCommand::CompileFromSpec {
        intent: intent.to_owned(),
        spec: assessment.spec,
        design: Some(Box::new(design)),
        conversation,
    });
    wait_for(&events, "PlanCompiled", |event| match event {
        EngineEvent::PlanCompiled { plan } => Some(*plan),
        _ => None,
    })
}

fn assert_declares_inputs(plan: &Plan, expected: &[&str]) {
    let names = plan
        .inputs
        .iter()
        .map(|input| input.name.as_str())
        .collect::<BTreeSet<_>>();
    for name in expected {
        assert!(names.contains(name), "missing input '{name}': {names:?}");
    }
}

fn assert_declares_input_concept(plan: &Plan, concept: &str) {
    let names = plan
        .inputs
        .iter()
        .map(|input| input.name.as_str())
        .collect::<Vec<_>>();
    assert!(
        names.iter().any(|name| name.contains(concept)),
        "missing input concept '{concept}': {names:?}"
    );
}

fn plan_text(plan: &Plan) -> String {
    serde_json::to_string(plan)
        .expect("plan should serialize")
        .to_ascii_lowercase()
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

fn assert_repository_safety_and_quality_gates(plan: &Plan) {
    let text = plan_text(plan);
    for required in [
        "cargo test",
        "live_spec_planning",
        "cargo fmt",
        "cargo clippy",
        "principal-developer",
    ] {
        assert!(text.contains(required), "plan missing '{required}': {text}");
    }
    let approvals = plan
        .steps
        .iter()
        .filter_map(|step| match &step.config {
            StepConfig::HumanInteraction(config) if config.approval_required => {
                Some(step.id.as_str())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert!(!approvals.is_empty(), "plan must require explicit approval");
    assert!(
        plan.steps.iter().any(|step| match &step.config {
            StepConfig::CodeCall(config) => config.inline.as_ref().is_some_and(|script| {
                let script = script.to_ascii_lowercase();
                script.contains("gh")
                    && script.contains("pr")
                    && script.contains("create")
                    && transitively_depends_on(plan, &step.id, &approvals)
            }),
            _ => false,
        }),
        "plan must invoke gh pr create after approval"
    );
    assert!(
        text.contains("status --porcelain")
            || text.contains("diff --quiet")
            || text.contains("status -s"),
        "plan must inspect tracked and untracked changes: {text}"
    );
    assert!(
        text.contains("git switch") || text.contains("git checkout"),
        "plan must switch branches explicitly: {text}"
    );
    assert!(
        text.contains("pull --ff-only") || text.contains("merge --ff-only"),
        "plan must update main with fast-forward-only semantics: {text}"
    );
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

fn assert_agent_call_is_used_and_approval_gated(plan: &Plan) {
    for step in &plan.steps {
        if let StepConfig::CodeCall(config) = &step.config {
            let script = config
                .inline
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            assert!(
                !invokes_coding_agent_cli(&script),
                "step '{}' invokes a coding-agent CLI from CODE_CALL instead of AGENT_CALL: {}",
                step.id,
                script
            );
        }
    }
    let agent_steps = plan
        .steps
        .iter()
        .filter(|step| matches!(step.config, StepConfig::AgentCall(_)))
        .map(|step| step.id.as_str())
        .collect::<Vec<_>>();
    assert!(
        !agent_steps.is_empty(),
        "plan does not use an AGENT_CALL step for the coding-agent work its intent describes"
    );
    let approvals = plan
        .steps
        .iter()
        .filter_map(|step| match &step.config {
            StepConfig::HumanInteraction(config) if config.approval_required => {
                Some(step.id.as_str())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert!(
        agent_steps
            .iter()
            .any(|agent_step| transitively_depends_on(plan, agent_step, &approvals)),
        "no AGENT_CALL step is transitively gated by human approval before it edits the workspace"
    );
}

fn assert_has_bounded_retry_loop(plan: &Plan) {
    assert!(
        plan.steps.iter().any(|step| matches!(
            &step.config,
            StepConfig::FanOut(config)
                if !config.over.trim().is_empty()
                    && config.until.as_ref().is_some_and(|until| !until.trim().is_empty())
        )),
        "plan must use a bounded FAN_OUT until loop: {:#?}",
        plan.steps
    );
}

#[test]
#[ignore = "requires an authenticated Codex CLI account"]
fn dossier_compiles_live_with_invocation_inputs_and_approval_only_hitl() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let paths = DataPaths::at(tmp.path().to_path_buf());
    AppSettings {
        backend: BackendChoice::Codex,
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
    assert!(!assessment.needs_clarification, "{assessment:?}");
    let spec_input_names = assessment
        .spec
        .inputs
        .iter()
        .map(|input| input.name.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(spec_input_names.len(), EXPECTED_INPUT_COUNT);
    let input_contract = assessment
        .spec
        .inputs
        .iter()
        .map(|input| format!("{} {}", input.name, input.description).to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    for concept in ["listing", "path", "topic", "count", "output", "root"] {
        assert!(
            input_contract.contains(concept),
            "spec input contract missing {concept}: {input_contract}"
        );
    }

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
            .filter(|step| step.step_kind.eq_ignore_ascii_case("human"))
            .count(),
        1,
        "{design:?}"
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

    let plan_input_names = plan
        .inputs
        .iter()
        .map(|input| input.name.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(plan_input_names, spec_input_names);
    let output_path = plan
        .inputs
        .iter()
        .find(|input| input.name == "output_path")
        .expect("dossier plan must declare output_path");
    assert!(output_path.required, "output_path must not resolve to null");
    assert_eq!(output_path.value_type, "string");
    assert_eq!(output_path.input_kind, InputKind::OutputFilePath);
    let human_steps = plan
        .steps
        .iter()
        .filter_map(|step| match &step.config {
            StepConfig::HumanInteraction(config) => Some(config),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(human_steps.len(), 1, "{:#?}", plan.steps);
    let approval_prompt = human_steps[0].prompt.to_ascii_lowercase();
    assert!(
        approval_prompt.contains("approv") || approval_prompt.contains("reject"),
        "unexpected human prompt: {}",
        human_steps[0].prompt
    );
}

#[test]
#[ignore = "requires an authenticated Codex CLI account"]
fn prompt_to_plan_dogfooding_workflow_compiles_with_regression_first_feedback_loop() {
    let plan = compile_live(PROMPT_TO_PLAN_INTENT);
    assert_declares_inputs(&plan, &["plan_prompt", "branch_name", "root_directory"]);
    assert_declares_input_concept(&plan, "expectation");
    assert_repository_safety_and_quality_gates(&plan);
    assert_has_bounded_retry_loop(&plan);
    assert_agent_call_is_used_and_approval_gated(&plan);

    let text = plan_text(&plan);
    for required in [
        "mcp",
        "compile_plan",
        "show_plan",
        "expectation",
        "feedback",
        "diagnos",
        "regression",
        "codex",
    ] {
        assert!(text.contains(required), "plan missing '{required}': {text}");
    }
}

#[test]
#[ignore = "requires an authenticated Codex CLI account"]
fn feature_development_dogfooding_workflow_compiles_with_module_owned_packages() {
    let plan = compile_live(FEATURE_DEVELOPMENT_INTENT);
    assert_declares_inputs(&plan, &["feature_request", "branch_name", "root_directory"]);
    assert_declares_input_concept(&plan, "expectation");
    assert_repository_safety_and_quality_gates(&plan);
    assert_agent_call_is_used_and_approval_gated(&plan);

    let text = plan_text(&plan);
    for required in ["module", "package", "codex", "approv"] {
        assert!(text.contains(required), "plan missing '{required}': {text}");
    }
    assert!(
        text.contains("parallel") || text.contains("concurrent"),
        "plan must distribute independent packages concurrently: {text}"
    );
}

#[test]
#[ignore = "requires an authenticated Codex CLI account"]
fn bugfix_dogfooding_workflow_compiles_with_failing_test_before_fix() {
    let plan = compile_live(BUGFIX_INTENT);
    assert_declares_inputs(&plan, &["bug_report", "branch_name", "root_directory"]);
    assert_declares_input_concept(&plan, "expectation");
    assert_repository_safety_and_quality_gates(&plan);
    assert_agent_call_is_used_and_approval_gated(&plan);

    let text = plan_text(&plan);
    for required in ["reproduce", "failing test", "before", "fix", "regression"] {
        assert!(text.contains(required), "plan missing '{required}': {text}");
    }
}
