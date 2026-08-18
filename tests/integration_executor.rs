//! Integration tests: executor end-to-end on simple plans.

use inxm_local::executor::{
    ExecutorConfig, HumanDecision, HumanRequest, RepairResumeMode, Run, StepRun, StepRunIteration,
    execute, resume, resume_from_repair,
};

use chrono::Utc;
use inxm_local::plan::types::*;
use inxm_local::storage::{StorageRoot, runs::RunStatus, runs::StepRunStatus};
use inxm_local::tools::catalog::{SubprocessConfig, ToolCatalog, ToolConfig, ToolEntry};
use std::sync::Arc;

fn echo_catalog() -> ToolCatalog {
    // Dynamic arguments are env-only (INXM_ARG_*); the command expands them.
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
        description: "echo".to_owned(),
        config: ToolConfig::Subprocess(SubprocessConfig {
            command,
            args,
            env: Default::default(),
            working_dir: None,
        }),
        input_schema: serde_json::json!({"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}),
        output_schema: serde_json::json!({"type":"object"}),
        allowlisted: true,
        timeout_secs: None,
    }])
}

fn make_echo_step(id: &str, message: &str, depends_on: Vec<&str>) -> PlanStep {
    PlanStep {
        id: id.to_owned(),
        name: id.to_owned(),
        description: None,
        config: StepConfig::ToolCall(ToolCallConfig {
            tool: "echo".to_owned(),
            arguments: {
                let mut m = indexmap::IndexMap::new();
                m.insert("message".to_owned(), serde_json::json!(message));
                m
            },
        }),
        depends_on: depends_on.into_iter().map(str::to_owned).collect(),
        outputs: vec![PlanOutput {
            name: "stdout".to_owned(),
            description: None,
            value_type: "string".to_owned(),
        }],
        timeout_secs: None,
        retry: None,
    }
}

#[tokio::test]
async fn single_step_plan_succeeds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));

    let plan = Plan {
        metadata: PlanMetadata::new(Some("test single step".to_owned())),
        name: "single step".to_owned(),
        description: None,
        inputs: vec![],
        config: Default::default(),
        steps: vec![make_echo_step("greet", "hello from test", vec![])],
        outputs: vec![],
    };

    let config = ExecutorConfig {
        inputs: Default::default(),
        timeout_secs: Some(30),
        storage,
        catalog: echo_catalog(),
        progress: None,
        human: None,
        llm_keys: Default::default(),
        source: None,
    };

    let run = execute(plan, config)
        .await
        .expect("execution should succeed");
    assert!(matches!(run.status, RunStatus::Succeeded));
    assert_eq!(run.step_runs.len(), 1);

    let step_run = &run.step_runs["greet"];
    assert!(
        matches!(
            step_run.status,
            inxm_local::storage::runs::StepRunStatus::Succeeded
        ),
        "step should have succeeded, got {:?}",
        step_run.status
    );
    // stdout should be populated (echo produces output)
    assert!(
        step_run.stdout.is_some(),
        "step should have captured stdout"
    );
}

#[tokio::test]
async fn plan_level_output_is_resolved_from_step_result() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));

    let plan = Plan {
        metadata: PlanMetadata::new(Some("test plan output".to_owned())),
        name: "plan output".to_owned(),
        description: None,
        inputs: vec![],
        config: Default::default(),
        steps: vec![make_echo_step("greet", "hello from test", vec![])],
        outputs: vec![PlanOutputRef {
            name: "greeting".to_owned(),
            description: Some("The final greeting".to_owned()),
            source: "${step.greet.stdout}".to_owned(),
        }],
    };

    let config = ExecutorConfig {
        inputs: Default::default(),
        timeout_secs: Some(30),
        storage,
        catalog: echo_catalog(),
        progress: None,
        human: None,
        llm_keys: Default::default(),
        source: None,
    };

    let run = execute(plan, config)
        .await
        .expect("execution should succeed");
    assert!(matches!(run.status, RunStatus::Succeeded));

    let expected = run.step_runs["greet"].outputs["stdout"].clone();
    assert_eq!(run.outputs.get("greeting"), Some(&expected));
}

#[tokio::test]
async fn declared_input_is_resolved_and_persisted_for_the_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
    let plan = Plan {
        metadata: PlanMetadata::new(None),
        name: "runtime input".to_owned(),
        description: None,
        inputs: vec![PlanInput {
            name: "message".to_owned(),
            description: Some("Message for this invocation".to_owned()),
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: inxm_local::plan::types::InputKind::Value,
        }],
        config: Default::default(),
        steps: vec![make_echo_step("greet", "${input.message}", vec![])],
        outputs: vec![],
    };
    let supplied = [("message".to_owned(), serde_json::json!("hello input"))]
        .into_iter()
        .collect();
    let config = ExecutorConfig {
        inputs: supplied,
        timeout_secs: Some(30),
        storage: storage.clone(),
        catalog: echo_catalog(),
        progress: None,
        human: None,
        llm_keys: Default::default(),
        source: None,
    };

    let run = execute(plan, config)
        .await
        .expect("input run should succeed");
    assert_eq!(run.inputs["message"], serde_json::json!("hello input"));
    assert!(
        run.step_runs["greet"]
            .stdout
            .as_deref()
            .is_some_and(|stdout| stdout.contains("hello input"))
    );
    assert_eq!(storage.runs().load(&run.id).unwrap().inputs, run.inputs);
}

#[tokio::test]
async fn missing_required_input_is_rejected_before_a_run_is_created() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
    let plan = Plan {
        metadata: PlanMetadata::new(None),
        name: "required input".to_owned(),
        description: None,
        inputs: vec![PlanInput {
            name: "message".to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: inxm_local::plan::types::InputKind::Value,
        }],
        config: Default::default(),
        steps: vec![make_echo_step("greet", "${input.message}", vec![])],
        outputs: vec![],
    };
    let config = ExecutorConfig {
        inputs: Default::default(),
        timeout_secs: Some(30),
        storage: storage.clone(),
        catalog: echo_catalog(),
        progress: None,
        human: None,
        llm_keys: Default::default(),
        source: None,
    };

    let error = execute(plan, config).await.unwrap_err().to_string();
    assert!(
        error.contains("missing required input 'message'"),
        "{error}"
    );
    assert!(storage.runs().list().unwrap().is_empty());
}

#[tokio::test]
async fn chain_plan_executes_in_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));

    let plan = Plan {
        metadata: PlanMetadata::new(None),
        name: "chain".to_owned(),
        description: None,
        inputs: vec![],
        config: Default::default(),
        steps: vec![
            make_echo_step("first", "step 1", vec![]),
            make_echo_step("second", "step 2", vec!["first"]),
            make_echo_step("third", "step 3", vec!["second"]),
        ],
        outputs: vec![],
    };

    let config = ExecutorConfig {
        inputs: Default::default(),
        timeout_secs: Some(30),
        storage,
        catalog: echo_catalog(),
        progress: None,
        human: None,
        llm_keys: Default::default(),
        source: None,
    };

    let run = execute(plan, config).await.expect("should succeed");
    assert!(matches!(run.status, RunStatus::Succeeded));
    assert_eq!(run.step_runs.len(), 3);
    for id in ["first", "second", "third"] {
        assert!(
            matches!(
                run.step_runs[id].status,
                inxm_local::storage::runs::StepRunStatus::Succeeded
            ),
            "step {id} should have succeeded"
        );
    }
}

#[tokio::test]
async fn code_call_passes_step_output_through_static_script_stdin() {
    // Executable source remains static. Runtime values from prior steps cross
    // the process boundary through structured stdin, where placeholders are
    // resolved without turning data into executable code.
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));

    // Step 1: emit a known JSON value so step 2 can reference it.
    let produce = PlanStep {
        id: "produce".to_owned(),
        name: "Produce".to_owned(),
        description: None,
        config: StepConfig::CodeCall(CodeCallConfig {
            language: "python".to_owned(),
            inline: Some(
                "import json; print(json.dumps({\"greeting\": \"hello_world\"}))".to_owned(),
            ),
            file: None,
            args: vec![],
            stdin: None,
            env: Default::default(),
            working_dir: None,
            timeout_secs: None,
        }),
        depends_on: vec![],
        outputs: vec![PlanOutput {
            name: "greeting".to_owned(),
            description: None,
            value_type: "string".to_owned(),
        }],
        timeout_secs: None,
        retry: None,
    };

    // Step 2: stream the prior step's output into a static script.
    let consume = PlanStep {
        id: "consume".to_owned(),
        name: "Consume".to_owned(),
        description: None,
        config: StepConfig::CodeCall(CodeCallConfig {
            language: "python".to_owned(),
            inline: Some(
                "import json, sys\nval = sys.stdin.read()\nprint(json.dumps({\"received\": val}))"
                    .to_owned(),
            ),
            file: None,
            args: vec![],
            stdin: Some("${step.produce.greeting}".to_owned()),
            env: Default::default(),
            working_dir: None,
            timeout_secs: None,
        }),
        depends_on: vec!["produce".to_owned()],
        outputs: vec![PlanOutput {
            name: "received".to_owned(),
            description: None,
            value_type: "string".to_owned(),
        }],
        timeout_secs: None,
        retry: None,
    };

    let plan = Plan {
        metadata: PlanMetadata::new(None),
        name: "placeholder resolution".to_owned(),
        description: None,
        inputs: vec![],
        config: Default::default(),
        steps: vec![produce, consume],
        outputs: vec![],
    };

    let config = ExecutorConfig {
        inputs: Default::default(),
        timeout_secs: Some(30),
        storage,
        catalog: ToolCatalog::new(vec![]),
        progress: None,
        human: None,
        llm_keys: Default::default(),
        source: None,
    };

    let run = execute(plan, config)
        .await
        .expect("execution should succeed");
    assert!(matches!(run.status, RunStatus::Succeeded));

    let received = run.step_runs["consume"]
        .outputs
        .get("received")
        .expect("consume step should have 'received' output");
    assert_eq!(
        received,
        &serde_json::json!("hello_world"),
        "structured stdin did not receive the resolved prior-step output"
    );
}

#[tokio::test]
async fn code_call_streams_large_placeholder_through_stdin() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
    // Large enough to exceed Windows' CreateProcess command-line limit if it
    // were expanded into argv or the inline script source.
    let payload = "x".repeat(100_000);

    let step = PlanStep {
        id: "measure".to_owned(),
        name: "Measure stdin".to_owned(),
        description: None,
        config: StepConfig::CodeCall(CodeCallConfig {
            language: "python".to_owned(),
            inline: Some(
                "import json, sys; print(json.dumps({'length': len(sys.stdin.read())}))".to_owned(),
            ),
            file: None,
            args: vec![],
            stdin: Some("${input.payload}".to_owned()),
            env: Default::default(),
            working_dir: None,
            timeout_secs: None,
        }),
        depends_on: vec![],
        outputs: vec![PlanOutput {
            name: "length".to_owned(),
            description: None,
            value_type: "integer".to_owned(),
        }],
        timeout_secs: None,
        retry: None,
    };

    let plan = Plan {
        metadata: PlanMetadata::new(None),
        name: "large stdin payload".to_owned(),
        description: None,
        inputs: vec![PlanInput {
            name: "payload".to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: inxm_local::plan::types::InputKind::Value,
        }],
        config: Default::default(),
        steps: vec![step],
        outputs: vec![],
    };
    let mut inputs = indexmap::IndexMap::new();
    inputs.insert("payload".to_owned(), serde_json::json!(payload));
    let config = ExecutorConfig {
        inputs,
        timeout_secs: Some(30),
        storage,
        catalog: ToolCatalog::new(vec![]),
        progress: None,
        human: None,
        llm_keys: Default::default(),
        source: None,
    };

    let run = execute(plan, config)
        .await
        .expect("large stdin execution should succeed");
    assert_eq!(
        run.step_runs["measure"].outputs.get("length"),
        Some(&serde_json::json!(100_000))
    );
}

#[tokio::test]
async fn code_call_argument_placeholder_resolves_plan_input() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
    let plan = Plan {
        metadata: PlanMetadata::new(None),
        name: "code argument input".to_owned(),
        description: None,
        inputs: vec![PlanInput {
            name: "test_command".to_owned(),
            description: Some("Shell command used to run the test suite".to_owned()),
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: inxm_local::plan::types::InputKind::Value,
        }],
        config: Default::default(),
        steps: vec![PlanStep {
            id: "run_tests".to_owned(),
            name: "Run test suite".to_owned(),
            description: None,
            config: StepConfig::CodeCall(CodeCallConfig {
                language: "python".to_owned(),
                inline: Some(
                    "import json, sys; print(json.dumps({'received': sys.argv[1]}))".to_owned(),
                ),
                file: None,
                args: vec!["${input.test_command}".to_owned()],
                stdin: None,
                env: Default::default(),
                working_dir: None,
                timeout_secs: None,
            }),
            depends_on: vec![],
            outputs: vec![PlanOutput {
                name: "received".to_owned(),
                description: None,
                value_type: "string".to_owned(),
            }],
            timeout_secs: None,
            retry: None,
        }],
        outputs: vec![],
    };
    let config = ExecutorConfig {
        inputs: [("test_command".to_owned(), serde_json::json!("cargo test"))]
            .into_iter()
            .collect(),
        timeout_secs: Some(30),
        storage,
        catalog: ToolCatalog::new(vec![]),
        progress: None,
        human: None,
        llm_keys: Default::default(),
        source: None,
    };

    let run = execute(plan, config)
        .await
        .expect("CODE_CALL input argument should resolve");
    assert_eq!(
        run.step_runs["run_tests"].outputs["received"],
        serde_json::json!("cargo test")
    );
}

#[tokio::test]
async fn fan_out_templates_record_iterations_before_human_pause() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));

    let produce_urls = PlanStep {
        id: "produce_urls".to_owned(),
        name: "Produce URLs".to_owned(),
        description: None,
        config: StepConfig::CodeCall(CodeCallConfig {
            language: "python".to_owned(),
            inline: Some(
                "print('[\"https://example.com/one\", \"https://example.com/two\"]')".to_owned(),
            ),
            file: None,
            args: vec![],
            stdin: None,
            env: Default::default(),
            working_dir: None,
            timeout_secs: None,
        }),
        depends_on: vec![],
        outputs: vec![PlanOutput {
            name: "urls".to_owned(),
            description: None,
            value_type: "array".to_owned(),
        }],
        timeout_secs: None,
        retry: None,
    };
    let fetch_post = make_echo_step("fetch_post", "${item.item}", vec![]);
    // This ID sorts after `ask_approval`, reproducing the case where a pause
    // previously left a later FAN_OUT template incorrectly pending.
    let summarize_post = make_echo_step(
        "zz_summarize_post",
        "${step.fetch_post.stdout}",
        vec!["fetch_post"],
    );
    let fan_out = PlanStep {
        id: "fetch_posts".to_owned(),
        name: "Fetch posts".to_owned(),
        description: None,
        config: StepConfig::FanOut(FanOutConfig {
            over: "produce_urls.urls".to_owned(),
            item_var: "item".to_owned(),
            spawn_steps: vec!["fetch_post".to_owned(), "zz_summarize_post".to_owned()],
            until: None,
        }),
        depends_on: vec!["produce_urls".to_owned()],
        outputs: vec![],
        timeout_secs: None,
        retry: None,
    };
    let approve = PlanStep {
        id: "ask_approval".to_owned(),
        name: "Approve".to_owned(),
        description: None,
        config: StepConfig::HumanInteraction(HumanInteractionConfig {
            prompt: "Approve?".to_owned(),
            response_field: "approval".to_owned(),
            approval_required: true,
        }),
        depends_on: vec!["fetch_posts".to_owned()],
        outputs: vec![],
        timeout_secs: None,
        retry: None,
    };
    let plan = Plan {
        metadata: PlanMetadata::new(None),
        name: "fan-out pause statuses".to_owned(),
        description: None,
        inputs: vec![],
        config: Default::default(),
        steps: vec![produce_urls, fetch_post, summarize_post, fan_out, approve],
        outputs: vec![],
    };

    let (human_tx, mut human_rx) = tokio::sync::mpsc::unbounded_channel::<HumanRequest>();
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let config = ExecutorConfig {
        inputs: Default::default(),
        timeout_secs: Some(30),
        storage,
        catalog: echo_catalog(),
        progress: Some(progress_tx),
        human: Some(human_tx),
        llm_keys: Default::default(),
        source: None,
    };
    let execution = tokio::spawn(async move { execute(plan, config).await });
    let request = human_rx.recv().await.expect("human request");
    drop(request.respond);
    let waiting = execution.await.unwrap().unwrap();

    assert!(matches!(waiting.status, RunStatus::WaitingForHuman { .. }));
    for step_id in ["fetch_post", "zz_summarize_post"] {
        let step_run = &waiting.step_runs[step_id];
        assert!(matches!(
            step_run.status,
            inxm_local::storage::runs::StepRunStatus::Succeeded
        ));
        assert_eq!(step_run.iterations.len(), 2);
        assert_eq!(step_run.iterations[0].iteration, 0);
        assert_eq!(step_run.iterations[1].iteration, 1);
        assert!(
            step_run
                .iterations
                .iter()
                .all(|iteration| !iteration.outputs.is_empty())
        );
    }

    let mut progress = Vec::new();
    while let Ok(event) = progress_rx.try_recv() {
        progress.push(event);
    }
    for iteration in 0..2 {
        assert!(progress.iter().any(|event| {
            event.step_id == "fetch_posts"
                && event.status == inxm_local::storage::runs::StepRunStatus::Running
                && event.fan_out_progress.is_some_and(|progress| {
                    progress.iteration == iteration && progress.total_iterations == 2
                })
        }));
    }
    for step_id in ["fetch_post", "zz_summarize_post"] {
        assert!(progress.iter().any(|event| {
            event.step_id == step_id
                && event.status == inxm_local::storage::runs::StepRunStatus::Running
        }));
        assert!(progress.iter().any(|event| {
            event.step_id == step_id
                && event.status == inxm_local::storage::runs::StepRunStatus::Running
                && event
                    .iteration
                    .as_ref()
                    .is_some_and(|iteration| iteration.iteration == 0)
        }));
        assert!(progress.iter().any(|event| {
            event.step_id == step_id
                && event.status == inxm_local::storage::runs::StepRunStatus::Succeeded
                && event
                    .iteration
                    .as_ref()
                    .is_some_and(|iteration| iteration.iteration == 1)
        }));
    }
}

#[tokio::test]
async fn fan_out_until_persists_only_iterations_through_the_first_match() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
    let attempts = PlanStep {
        id: "attempts".to_owned(),
        name: "Bound attempts".to_owned(),
        description: None,
        config: StepConfig::CodeCall(CodeCallConfig {
            language: "python".to_owned(),
            inline: Some("print('[1, 2, 3, 4, 5]')".to_owned()),
            file: None,
            args: vec![],
            stdin: None,
            env: Default::default(),
            working_dir: None,
            timeout_secs: None,
        }),
        depends_on: vec![],
        outputs: vec![PlanOutput {
            name: "values".to_owned(),
            description: None,
            value_type: "array".to_owned(),
        }],
        timeout_secs: None,
        retry: None,
    };
    let verify = PlanStep {
        id: "verify".to_owned(),
        name: "Verify".to_owned(),
        description: None,
        config: StepConfig::Condition(ConditionConfig {
            expression: "${item.attempt} == 3".to_owned(),
            true_steps: vec![],
            false_steps: vec![],
        }),
        depends_on: vec![],
        outputs: vec![],
        timeout_secs: None,
        retry: None,
    };
    let retry = PlanStep {
        id: "retry".to_owned(),
        name: "Retry".to_owned(),
        description: None,
        config: StepConfig::FanOut(FanOutConfig {
            over: "attempts.values".to_owned(),
            item_var: "attempt".to_owned(),
            spawn_steps: vec!["verify".to_owned()],
            until: Some("${step.verify.result} == true".to_owned()),
        }),
        depends_on: vec!["attempts".to_owned()],
        outputs: vec![],
        timeout_secs: None,
        retry: None,
    };
    let plan = Plan {
        metadata: PlanMetadata::new(None),
        name: "bounded retry".to_owned(),
        description: None,
        inputs: vec![],
        config: Default::default(),
        steps: vec![attempts, verify, retry],
        outputs: vec![],
    };
    let config = ExecutorConfig {
        inputs: Default::default(),
        timeout_secs: Some(30),
        storage,
        catalog: ToolCatalog::new(vec![]),
        progress: None,
        human: None,
        llm_keys: Default::default(),
        source: None,
    };

    let run = execute(plan, config)
        .await
        .expect("execution should succeed");

    assert_eq!(run.step_runs["verify"].iterations.len(), 3);
    assert_eq!(
        run.step_runs["retry"].outputs["results"],
        serde_json::json!([false, false, true])
    );
}

#[tokio::test]
async fn fan_out_persists_failed_child_iteration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
    let produce_urls = PlanStep {
        id: "produce_urls".to_owned(),
        name: "Produce URLs".to_owned(),
        description: None,
        config: StepConfig::CodeCall(CodeCallConfig {
            language: "python".to_owned(),
            inline: Some(
                "print('[\"https://example.com/one\", \"https://example.com/two\"]')".to_owned(),
            ),
            file: None,
            args: vec![],
            stdin: None,
            env: Default::default(),
            working_dir: None,
            timeout_secs: None,
        }),
        depends_on: vec![],
        outputs: vec![PlanOutput {
            name: "urls".to_owned(),
            description: None,
            value_type: "array".to_owned(),
        }],
        timeout_secs: None,
        retry: None,
    };
    let succeed_child = make_echo_step("succeed_child", "${item.item}", vec![]);
    let fail_child = PlanStep {
        id: "fail_child".to_owned(),
        name: "Fail child".to_owned(),
        description: None,
        config: StepConfig::CodeCall(CodeCallConfig {
            language: "python".to_owned(),
            inline: Some("import sys; sys.exit(7)".to_owned()),
            file: None,
            args: vec![],
            stdin: None,
            env: Default::default(),
            working_dir: None,
            timeout_secs: None,
        }),
        depends_on: vec![],
        outputs: vec![],
        timeout_secs: None,
        retry: None,
    };
    let fan_out = PlanStep {
        id: "process_urls".to_owned(),
        name: "Process URLs".to_owned(),
        description: None,
        config: StepConfig::FanOut(FanOutConfig {
            over: "produce_urls.urls".to_owned(),
            item_var: "item".to_owned(),
            spawn_steps: vec!["succeed_child".to_owned(), "fail_child".to_owned()],
            until: None,
        }),
        depends_on: vec!["produce_urls".to_owned()],
        outputs: vec![],
        timeout_secs: None,
        retry: None,
    };
    let plan = Plan {
        metadata: PlanMetadata::new(None),
        name: "failed fan-out child".to_owned(),
        description: None,
        inputs: vec![],
        config: Default::default(),
        steps: vec![produce_urls, succeed_child, fail_child, fan_out],
        outputs: vec![],
    };
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let config = ExecutorConfig {
        inputs: Default::default(),
        timeout_secs: Some(30),
        storage: storage.clone(),
        catalog: echo_catalog(),
        progress: Some(progress_tx),
        human: None,
        llm_keys: Default::default(),
        source: None,
    };

    assert!(execute(plan, config).await.is_err());
    let first_event = progress_rx.try_recv().expect("progress event");
    let stored = storage
        .runs()
        .load(&first_event.run_id)
        .expect("persisted failed run");
    let child = &stored.step_runs["fail_child"];
    assert_eq!(
        child.status,
        inxm_local::storage::runs::StepRunStatus::Failed
    );
    assert_eq!(child.iterations.len(), 1);
    assert_eq!(child.iterations[0].iteration, 0);
    assert!(child.iterations[0].error.is_some());
    assert!(child.error.is_some());

    let completed_sibling = &stored.step_runs["succeed_child"];
    assert_eq!(
        completed_sibling.status,
        inxm_local::storage::runs::StepRunStatus::Cancelled
    );
    assert_eq!(completed_sibling.iterations.len(), 1);
}

#[tokio::test]
async fn failed_step_leaves_unstarted_downstream_pending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));

    // Catalog with a tool that always fails (non-zero exit)
    let (fail_command, fail_args) = if cfg!(windows) {
        (
            "cmd".to_owned(),
            vec![
                "/C".to_owned(),
                "exit".to_owned(),
                "/B".to_owned(),
                "1".to_owned(),
            ],
        )
    } else {
        ("false".to_owned(), vec![])
    };
    let failing_catalog = ToolCatalog::new(vec![ToolEntry {
        name: "fail".to_owned(),
        description: "always fails".to_owned(),
        config: ToolConfig::Subprocess(SubprocessConfig {
            command: fail_command,
            args: fail_args,
            env: Default::default(),
            working_dir: None,
        }),
        input_schema: serde_json::json!({"type":"object"}),
        output_schema: serde_json::json!({"type":"object"}),
        allowlisted: true,
        timeout_secs: None,
    }]);

    let plan = Plan {
        metadata: PlanMetadata::new(None),
        name: "fail chain".to_owned(),
        description: None,
        inputs: vec![],
        config: Default::default(),
        steps: vec![
            PlanStep {
                id: "should-fail".to_owned(),
                name: "Should Fail".to_owned(),
                description: None,
                config: StepConfig::ToolCall(ToolCallConfig {
                    tool: "fail".to_owned(),
                    arguments: Default::default(),
                }),
                depends_on: vec![],
                outputs: vec![],
                timeout_secs: None,
                retry: None,
            },
            PlanStep {
                id: "should-skip".to_owned(),
                name: "Should Skip".to_owned(),
                description: None,
                config: StepConfig::ToolCall(ToolCallConfig {
                    tool: "fail".to_owned(),
                    arguments: Default::default(),
                }),
                depends_on: vec!["should-fail".to_owned()],
                outputs: vec![],
                timeout_secs: None,
                retry: None,
            },
        ],
        outputs: vec![],
    };

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let config = ExecutorConfig {
        inputs: Default::default(),
        timeout_secs: Some(30),
        storage: storage.clone(),
        catalog: failing_catalog,
        progress: Some(progress_tx),
        human: None,
        llm_keys: Default::default(),
        source: None,
    };

    let result = execute(plan, config).await;
    assert!(result.is_err(), "expected failure");
    let first_event = progress_rx.try_recv().expect("progress event");
    let stored = storage
        .runs()
        .load(&first_event.run_id)
        .expect("persisted failed run");
    assert_eq!(
        stored.step_runs["should-fail"].status,
        StepRunStatus::Failed
    );
    assert_eq!(
        stored.step_runs["should-skip"].status,
        StepRunStatus::Pending,
        "fail-fast must not misreport a dependent that never started as skipped"
    );
}

#[tokio::test]
async fn condition_skip_survives_human_resume() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));

    let produce = PlanStep {
        id: "produce".to_owned(),
        name: "Produce summary".to_owned(),
        description: None,
        config: StepConfig::CodeCall(CodeCallConfig {
            language: "python".to_owned(),
            inline: Some("import json; print(json.dumps({'summary': 'ready'}))".to_owned()),
            file: None,
            args: vec![],
            stdin: None,
            env: Default::default(),
            working_dir: None,
            timeout_secs: None,
        }),
        depends_on: vec![],
        outputs: vec![PlanOutput {
            name: "summary".to_owned(),
            description: None,
            value_type: "string".to_owned(),
        }],
        timeout_secs: None,
        retry: None,
    };
    let approve = PlanStep {
        id: "approve".to_owned(),
        name: "Approve summary".to_owned(),
        description: None,
        config: StepConfig::HumanInteraction(HumanInteractionConfig {
            prompt: "Summary: ${step.produce.summary}. Send it?".to_owned(),
            response_field: "approval".to_owned(),
            approval_required: true,
        }),
        depends_on: vec!["route".to_owned()],
        outputs: vec![],
        timeout_secs: None,
        retry: None,
    };
    let route = PlanStep {
        id: "route".to_owned(),
        name: "Route summary".to_owned(),
        description: None,
        config: StepConfig::Condition(ConditionConfig {
            expression: "true == true".to_owned(),
            true_steps: vec!["approve".to_owned()],
            false_steps: vec!["do-not-send".to_owned()],
        }),
        depends_on: vec!["produce".to_owned()],
        outputs: vec![],
        timeout_secs: None,
        retry: None,
    };
    let do_not_send = make_echo_step("do-not-send", "not sent", vec!["route"]);
    let plan = Plan {
        metadata: PlanMetadata::new(None),
        name: "resumable human".to_owned(),
        description: None,
        inputs: vec![],
        config: Default::default(),
        steps: vec![produce, route, approve, do_not_send],
        outputs: vec![],
    };

    let (human_tx, mut human_rx) = tokio::sync::mpsc::unbounded_channel::<HumanRequest>();
    let first_config = ExecutorConfig {
        inputs: Default::default(),
        timeout_secs: Some(30),
        storage: storage.clone(),
        catalog: ToolCatalog::default(),
        progress: None,
        human: Some(human_tx),
        llm_keys: Default::default(),
        source: None,
    };
    let first_plan = plan.clone();
    let first = tokio::spawn(async move { execute(first_plan, first_config).await });
    let request = human_rx.recv().await.expect("human request");
    assert_eq!(request.prompt, "Summary: ready. Send it?");
    assert_eq!(request.response_field, "approval");
    drop(request.respond);

    let waiting = first.await.unwrap().unwrap();
    let run_id = waiting.id.clone();
    let producer_started_at = waiting.step_runs["produce"].started_at;
    assert_eq!(
        waiting.status,
        RunStatus::WaitingForHuman {
            step_id: "approve".to_owned()
        }
    );
    assert_eq!(
        waiting.step_runs["approve"].status,
        inxm_local::storage::runs::StepRunStatus::WaitingForHuman
    );
    assert_eq!(
        waiting.step_runs["do-not-send"].status,
        StepRunStatus::Skipped,
        "the CONDITION runner must mark the untaken branch skipped"
    );
    assert_eq!(storage.runs().load(&run_id).unwrap().status, waiting.status);

    let (human_tx, mut human_rx) = tokio::sync::mpsc::unbounded_channel::<HumanRequest>();
    let resume_config = ExecutorConfig {
        inputs: Default::default(),
        timeout_secs: Some(30),
        storage,
        catalog: ToolCatalog::default(),
        progress: None,
        human: Some(human_tx),
        llm_keys: Default::default(),
        source: None,
    };
    let resumed = tokio::spawn(async move { resume(plan, resume_config, waiting).await });
    let request = human_rx.recv().await.expect("resumed human request");
    request.respond.send(HumanDecision::Approve).unwrap();

    let completed = resumed.await.unwrap().unwrap();
    assert_eq!(completed.id, run_id);
    assert_eq!(completed.status, RunStatus::Succeeded);
    assert_eq!(
        completed.step_runs["produce"].started_at, producer_started_at,
        "completed dependencies must not run again"
    );
    assert_eq!(
        completed.step_runs["approve"].outputs["approval"],
        serde_json::json!("approved")
    );
    assert_eq!(
        completed.step_runs["do-not-send"].status,
        StepRunStatus::Skipped,
        "resume must preserve the CONDITION checkpoint for the untaken branch"
    );
}

// ─── resume_from_repair ────────────────────────────────────────────────────────
//
// Scenario shared by most of these tests: `aaa_parallel` and `setup` have no
// dependency on the failing step and complete before it; `flaky` fails
// because its tool always exits non-zero; `after` depends on `flaky` and
// therefore never gets a chance to run in the original failed execution
// (the executor returns as soon as a step errors). A "repair" is simulated
// by handing `resume_from_repair` a bumped plan version whose `flaky` step
// now points at a tool that succeeds — exactly what `repair::apply_patch`
// would produce for an `UpdateStepConfig`/`SetStepField` patch.

fn make_tool_step(id: &str, tool: &str, message: &str, depends_on: Vec<&str>) -> PlanStep {
    PlanStep {
        id: id.to_owned(),
        name: id.to_owned(),
        description: None,
        config: StepConfig::ToolCall(ToolCallConfig {
            tool: tool.to_owned(),
            arguments: {
                let mut m = indexmap::IndexMap::new();
                m.insert("message".to_owned(), serde_json::json!(message));
                m
            },
        }),
        depends_on: depends_on.into_iter().map(str::to_owned).collect(),
        outputs: vec![PlanOutput {
            name: "stdout".to_owned(),
            description: None,
            value_type: "string".to_owned(),
        }],
        timeout_secs: None,
        retry: None,
    }
}

/// Catalog with a working `echo` plus a `fail` tool that always exits
/// non-zero, so a step's tool can be swapped from failing to succeeding
/// between plan versions without changing anything else about the step.
fn echo_and_fail_catalog() -> ToolCatalog {
    let (echo_command, echo_args) = if cfg!(windows) {
        ("cmd".to_owned(), vec!["/C".to_owned(), "echo".to_owned()])
    } else {
        ("echo".to_owned(), vec![])
    };
    let (fail_command, fail_args) = if cfg!(windows) {
        (
            "cmd".to_owned(),
            vec![
                "/C".to_owned(),
                "exit".to_owned(),
                "/B".to_owned(),
                "1".to_owned(),
            ],
        )
    } else {
        ("false".to_owned(), vec![])
    };
    ToolCatalog::new(vec![
        ToolEntry {
            name: "echo".to_owned(),
            description: "echo".to_owned(),
            config: ToolConfig::Subprocess(SubprocessConfig {
                command: echo_command,
                args: echo_args,
                env: Default::default(),
                working_dir: None,
            }),
            input_schema: serde_json::json!({"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}),
            output_schema: serde_json::json!({"type":"object"}),
            allowlisted: true,
            timeout_secs: None,
        },
        ToolEntry {
            name: "fail".to_owned(),
            description: "always fails".to_owned(),
            config: ToolConfig::Subprocess(SubprocessConfig {
                command: fail_command,
                args: fail_args,
                env: Default::default(),
                working_dir: None,
            }),
            input_schema: serde_json::json!({"type":"object"}),
            output_schema: serde_json::json!({"type":"object"}),
            allowlisted: true,
            timeout_secs: None,
        },
    ])
}

fn base_repair_plan(flaky_tool: &str) -> Plan {
    Plan {
        metadata: PlanMetadata::new(None),
        name: "repairable".to_owned(),
        description: None,
        inputs: vec![],
        config: Default::default(),
        steps: vec![
            make_tool_step("aaa_parallel", "echo", "parallel", vec![]),
            make_tool_step("setup", "echo", "setup", vec![]),
            make_tool_step("flaky", flaky_tool, "flaky", vec!["setup"]),
            make_tool_step("after", "echo", "after", vec!["flaky"]),
        ],
        outputs: vec![],
    }
}

/// Run `plan` (whose `flaky` step always fails) to completion and return the
/// persisted `Run` in its `Failed` state, fetched via the progress channel
/// the same way `fan_out_persists_failed_child_iteration` does.
async fn run_until_flaky_fails(storage: Arc<StorageRoot>, plan: Plan) -> inxm_local::executor::Run {
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let config = ExecutorConfig {
        inputs: Default::default(),
        timeout_secs: Some(30),
        storage: storage.clone(),
        catalog: echo_and_fail_catalog(),
        progress: Some(progress_tx),
        human: None,
        llm_keys: Default::default(),
        source: None,
    };
    assert!(execute(plan, config).await.is_err(), "flaky must fail");
    let first_event = progress_rx.try_recv().expect("progress event");
    storage
        .runs()
        .load(&first_event.run_id)
        .expect("persisted failed run")
}

/// Stamp `plan` with the same plan id as `failed` and the next version — the
/// identity a real `repair::apply_patch` output would carry.
fn as_next_version_of(mut plan: Plan, failed: &inxm_local::executor::Run) -> Plan {
    plan.metadata.id = failed.plan_id.clone();
    plan.metadata.version = failed.plan_version + 1;
    plan
}

fn resume_config(storage: Arc<StorageRoot>) -> ExecutorConfig {
    ExecutorConfig {
        inputs: Default::default(),
        timeout_secs: Some(30),
        storage,
        catalog: echo_and_fail_catalog(),
        progress: None,
        human: None,
        llm_keys: Default::default(),
        source: None,
    }
}

#[tokio::test]
async fn resume_from_repair_skips_already_succeeded_upstream_steps() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));

    let failed = run_until_flaky_fails(storage.clone(), base_repair_plan("fail")).await;
    let parallel_started_at = failed.step_runs["aaa_parallel"].started_at;
    let setup_started_at = failed.step_runs["setup"].started_at;

    let patched_plan = as_next_version_of(base_repair_plan("echo"), &failed);

    let resumed = resume_from_repair(
        patched_plan,
        resume_config(storage.clone()),
        failed,
        Default::default(),
        RepairResumeMode::PatchedPlan,
    )
    .await
    .expect("resume should succeed once flaky is fixed");

    assert_eq!(resumed.status, RunStatus::Succeeded);
    assert_eq!(
        resumed.step_runs["aaa_parallel"].started_at, parallel_started_at,
        "unrelated already-succeeded step must not be re-run"
    );
    assert_eq!(
        resumed.step_runs["setup"].started_at, setup_started_at,
        "upstream already-succeeded step must not be re-run"
    );
}

#[tokio::test]
async fn resume_from_repair_reexecutes_the_failed_step() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));

    let failed = run_until_flaky_fails(storage.clone(), base_repair_plan("fail")).await;
    assert_eq!(failed.step_runs["flaky"].status, StepRunStatus::Failed);
    assert!(failed.step_runs["flaky"].error.is_some());
    let original_plan_version = failed.plan_version;

    let patched_plan = as_next_version_of(base_repair_plan("echo"), &failed);
    let resumed = resume_from_repair(
        patched_plan,
        resume_config(storage.clone()),
        failed,
        Default::default(),
        RepairResumeMode::PatchedPlan,
    )
    .await
    .expect("resume should succeed once flaky is fixed");

    let flaky = &resumed.step_runs["flaky"];
    assert_eq!(flaky.status, StepRunStatus::Succeeded);
    assert!(
        flaky.error.is_none(),
        "the stale failure message must be cleared on reset"
    );
    assert_eq!(
        resumed.plan_version,
        original_plan_version + 1,
        "the run should now be recorded against the patched plan version"
    );
}

#[tokio::test]
async fn resume_from_repair_reexecutes_downstream_steps() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));

    let failed = run_until_flaky_fails(storage.clone(), base_repair_plan("fail")).await;
    // `after` depends on the failed step, so the original run never reached it.
    assert_eq!(failed.step_runs["after"].status, StepRunStatus::Pending);

    let patched_plan = as_next_version_of(base_repair_plan("echo"), &failed);
    let run_id = failed.id.clone();
    let resumed = resume_from_repair(
        patched_plan,
        resume_config(storage.clone()),
        failed,
        Default::default(),
        RepairResumeMode::PatchedPlan,
    )
    .await
    .expect("resume should succeed once flaky is fixed");

    assert_eq!(resumed.id, run_id, "resume continues the same run");
    assert_eq!(resumed.status, RunStatus::Succeeded);
    assert_eq!(resumed.step_runs["after"].status, StepRunStatus::Succeeded);
}

#[tokio::test]
async fn resume_from_repair_picks_up_a_step_inserted_between_existing_steps() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));

    let failed = run_until_flaky_fails(storage.clone(), base_repair_plan("fail")).await;

    // Simulate an `InsertAfter` patch: a brand new "middle" step is spliced
    // in between the (now fixed) `flaky` step and `after`, and `after` is
    // rewired to depend on it instead of depending on `flaky` directly.
    let mut patched_plan = Plan {
        metadata: PlanMetadata::new(None),
        name: "repairable".to_owned(),
        description: None,
        inputs: vec![],
        config: Default::default(),
        steps: vec![
            make_tool_step("aaa_parallel", "echo", "parallel", vec![]),
            make_tool_step("setup", "echo", "setup", vec![]),
            make_tool_step("flaky", "echo", "flaky", vec!["setup"]),
            make_tool_step("middle", "echo", "middle", vec!["flaky"]),
            make_tool_step("after", "echo", "after", vec!["middle"]),
        ],
        outputs: vec![],
    };
    patched_plan = as_next_version_of(patched_plan, &failed);

    let resumed = resume_from_repair(
        patched_plan,
        resume_config(storage.clone()),
        failed,
        Default::default(),
        RepairResumeMode::PatchedPlan,
    )
    .await
    .expect("resume should succeed with the inserted step");

    assert_eq!(resumed.status, RunStatus::Succeeded);
    assert_eq!(resumed.step_runs["middle"].status, StepRunStatus::Succeeded);
    assert_eq!(resumed.step_runs["after"].status, StepRunStatus::Succeeded);
}

#[tokio::test]
async fn resume_from_repair_resets_only_the_failed_step_and_its_true_dependents() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));

    let failed = run_until_flaky_fails(storage.clone(), base_repair_plan("fail")).await;
    let parallel_started_at = failed.step_runs["aaa_parallel"].started_at;
    let setup_started_at = failed.step_runs["setup"].started_at;
    assert_eq!(failed.step_runs.len(), 4);

    // The patch only swaps `flaky`'s tool — it does not touch `after`'s
    // `depends_on`, so the DAG shape downstream of `flaky` is unchanged.
    let patched_plan = as_next_version_of(base_repair_plan("echo"), &failed);
    let resumed = resume_from_repair(
        patched_plan,
        resume_config(storage.clone()),
        failed,
        Default::default(),
        RepairResumeMode::PatchedPlan,
    )
    .await
    .expect("resume should succeed once flaky is fixed");

    assert_eq!(
        resumed.step_runs.len(),
        4,
        "no step entries should appear or disappear"
    );
    assert_eq!(
        resumed.step_runs["aaa_parallel"].started_at, parallel_started_at,
        "sibling step outside the failed step's dependent chain must be untouched"
    );
    assert_eq!(
        resumed.step_runs["setup"].started_at, setup_started_at,
        "upstream dependency must be untouched"
    );
    assert_eq!(resumed.step_runs["flaky"].status, StepRunStatus::Succeeded);
    assert_eq!(resumed.step_runs["after"].status, StepRunStatus::Succeeded);
}

fn input_repair_plan(failed_tool: &str, completed_uses_input: bool, input_name: &str) -> Plan {
    let mut completed = make_tool_step("completed", "echo", "completed", vec![]);
    if completed_uses_input && let StepConfig::ToolCall(config) = &mut completed.config {
        config.arguments.insert(
            "message".to_owned(),
            serde_json::json!(format!("${{input.{input_name}}}")),
        );
    }

    Plan {
        metadata: PlanMetadata::new(None),
        name: "input repair plan".to_owned(),
        description: None,
        inputs: vec![PlanInput {
            name: input_name.to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: InputKind::Value,
        }],
        config: Default::default(),
        steps: vec![
            completed,
            make_input_tool_step("flaky", failed_tool, input_name, vec!["completed"]),
            make_input_tool_step("after", "echo", input_name, vec!["flaky"]),
        ],
        outputs: vec![],
    }
}

fn make_input_tool_step(id: &str, tool: &str, input_name: &str, depends_on: Vec<&str>) -> PlanStep {
    let mut step = make_tool_step(id, tool, "input", depends_on);
    if let StepConfig::ToolCall(config) = &mut step.config {
        config.arguments.insert(
            "message".to_owned(),
            serde_json::json!(format!("${{input.{input_name}}}")),
        );
    }
    step
}

async fn run_until_flaky_fails_with_inputs(
    storage: Arc<StorageRoot>,
    plan: Plan,
    inputs: indexmap::IndexMap<String, serde_json::Value>,
) -> inxm_local::executor::Run {
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let config = ExecutorConfig {
        inputs,
        timeout_secs: Some(30),
        storage: storage.clone(),
        catalog: echo_and_fail_catalog(),
        progress: Some(progress_tx),
        human: None,
        llm_keys: Default::default(),
        source: None,
    };
    assert!(execute(plan, config).await.is_err(), "flaky must fail");
    let first_event = progress_rx.try_recv().expect("progress event");
    storage
        .runs()
        .load(&first_event.run_id)
        .expect("persisted failed run")
}

fn failed_run(plan: &Plan, inputs: indexmap::IndexMap<String, serde_json::Value>) -> Run {
    let mut run = Run::new(plan.metadata.id.clone(), plan.metadata.version - 1);
    run.status = RunStatus::Failed {
        failed_step_id: "flaky".to_owned(),
        message: "flaky failed".to_owned(),
    };
    run.inputs = inputs;
    let mut flaky = StepRun::new("flaky");
    flaky.status = StepRunStatus::Failed;
    run.step_runs.insert("flaky".to_owned(), flaky);
    run
}

#[tokio::test]
async fn repair_resume_allows_downstream_input_override_and_persists_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
    let plan = input_repair_plan("fail", false, "value");
    let failed = run_until_flaky_fails_with_inputs(
        storage.clone(),
        plan.clone(),
        indexmap::indexmap! { "value".to_owned() => serde_json::json!("old") },
    )
    .await;
    let patched_plan = as_next_version_of(input_repair_plan("echo", false, "value"), &failed);

    let resumed = resume_from_repair(
        patched_plan,
        resume_config(storage.clone()),
        failed,
        indexmap::indexmap! { "value".to_owned() => serde_json::json!("new") },
        RepairResumeMode::PatchedPlan,
    )
    .await
    .expect("failed-step input may be replaced");

    assert_eq!(resumed.status, RunStatus::Succeeded);
    assert_eq!(resumed.inputs["value"], serde_json::json!("new"));
    assert_eq!(resumed.step_runs["after"].status, StepRunStatus::Succeeded);
    let persisted = storage.runs().load(&resumed.id).expect("persisted run");
    assert_eq!(persisted.inputs["value"], serde_json::json!("new"));
}

#[tokio::test]
async fn repair_resume_rejects_override_used_by_a_succeeded_step() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
    let plan = input_repair_plan("fail", true, "value");
    let failed = run_until_flaky_fails_with_inputs(
        storage.clone(),
        plan.clone(),
        indexmap::indexmap! { "value".to_owned() => serde_json::json!("old") },
    )
    .await;
    assert_eq!(
        failed.step_runs["completed"].status,
        StepRunStatus::Succeeded
    );
    let patched_plan = as_next_version_of(input_repair_plan("echo", true, "value"), &failed);

    let error = resume_from_repair(
        patched_plan,
        resume_config(storage),
        failed,
        indexmap::indexmap! { "value".to_owned() => serde_json::json!("new") },
        RepairResumeMode::PatchedPlan,
    )
    .await
    .expect_err("completed-step input must be protected");
    assert!(error.to_string().contains("succeeded step 'completed'"));
}

#[tokio::test]
async fn repair_resume_rejects_override_used_by_a_succeeded_fan_out_iteration() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
    let mut plan = input_repair_plan("echo", false, "value");
    plan.steps.insert(
        0,
        PlanStep {
            id: "fan_child".to_owned(),
            name: "fan child".to_owned(),
            description: None,
            config: StepConfig::ToolCall(ToolCallConfig {
                tool: "echo".to_owned(),
                arguments: [("message".to_owned(), serde_json::json!("${input.value}"))]
                    .into_iter()
                    .collect(),
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        },
    );
    let mut run = failed_run(
        &plan,
        indexmap::indexmap! { "value".to_owned() => serde_json::json!("old") },
    );
    let now = Utc::now();
    run.step_runs.insert(
        "fan_child".to_owned(),
        StepRun {
            iterations: vec![StepRunIteration {
                iteration: 0,
                status: StepRunStatus::Succeeded,
                started_at: now,
                finished_at: now,
                duration_ms: 0,
                outputs: Default::default(),
                stdout: None,
                stderr: None,
                error: None,
                token_usage: None,
            }],
            ..StepRun::new("fan_child")
        },
    );
    let patched_plan = as_next_version_of(plan, &run);

    let error = resume_from_repair(
        patched_plan,
        resume_config(storage),
        run,
        indexmap::indexmap! { "value".to_owned() => serde_json::json!("new") },
        RepairResumeMode::PatchedPlan,
    )
    .await
    .expect_err("completed fan-out iteration input must be protected");
    assert!(error.to_string().contains("succeeded step 'fan_child'"));
}

#[tokio::test]
async fn repair_resume_rebases_added_removed_and_invalid_inputs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));

    let mut repaired = input_repair_plan("echo", false, "introduced");
    repaired.inputs[0].required = false;
    repaired.inputs[0].default = Some(serde_json::json!("default"));
    let failed = failed_run(
        &repaired,
        indexmap::indexmap! { "removed".to_owned() => serde_json::json!("stale") },
    );
    let patched = as_next_version_of(repaired.clone(), &failed);
    let resumed = resume_from_repair(
        patched,
        resume_config(storage.clone()),
        failed.clone(),
        Default::default(),
        RepairResumeMode::PatchedPlan,
    )
    .await
    .expect("new defaults and removed values should rebase");
    assert_eq!(resumed.inputs["introduced"], serde_json::json!("default"));
    assert!(!resumed.inputs.contains_key("removed"));

    let unknown = resume_from_repair(
        repaired.clone(),
        resume_config(storage.clone()),
        failed.clone(),
        indexmap::indexmap! { "unknown".to_owned() => serde_json::json!("x") },
        RepairResumeMode::PatchedPlan,
    )
    .await
    .expect_err("unknown override must be rejected");
    assert!(unknown.to_string().contains("unknown input"));

    let mut required = repaired.clone();
    required.inputs[0].required = true;
    required.inputs[0].default = None;
    let missing = resume_from_repair(
        required.clone(),
        resume_config(storage.clone()),
        failed.clone(),
        Default::default(),
        RepairResumeMode::PatchedPlan,
    )
    .await
    .expect_err("new required input must be supplied");
    assert!(missing.to_string().contains("missing required input"));

    let mut integer = required;
    integer.inputs[0].value_type = "integer".to_owned();
    let invalid = resume_from_repair(
        integer,
        resume_config(storage),
        failed,
        indexmap::indexmap! { "introduced".to_owned() => serde_json::json!("not an integer") },
        RepairResumeMode::PatchedPlan,
    )
    .await
    .expect_err("invalid override type must be rejected");
    assert!(invalid.to_string().contains("must be integer"));
}
