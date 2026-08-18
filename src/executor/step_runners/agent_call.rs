//! `AGENT_CALL` runner.
//!
//! This deliberately owns a subprocess protocol instead of going through
//! `llm::complete`: the child receives a writable workspace and its normal
//! tool set, and every event it emits is retained as an audit transcript.

use super::{StepContext, StepResult, resolve_placeholders};
use crate::error::ExecutorError;
use crate::executor::{AgentTranscriptEvent, AgentTranscriptStream};
use crate::llm::{CUSTOM_CLI_PROMPT_PLACEHOLDER, LlmProtocol};
use crate::plan::types::StepConfig;
use crate::storage::runs::TokenUsage;
use indexmap::IndexMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const DEFAULT_AGENT_TIMEOUT_SECS: u64 = 900;

pub async fn run(ctx: &StepContext) -> Result<StepResult, ExecutorError> {
    let cfg = match &ctx.step.config {
        StepConfig::AgentCall(cfg) => cfg,
        _ => unreachable!("agent_call runner called for non-AGENT_CALL step"),
    };
    let objective = resolve_placeholders(
        &serde_json::Value::String(cfg.objective.clone()),
        &ctx.plan.config,
        &ctx.step_outputs,
    )
    .as_str()
    .unwrap_or_default()
    .to_owned();
    let working_dir = resolve_placeholders(
        &serde_json::Value::String(cfg.working_dir.clone()),
        &ctx.plan.config,
        &ctx.step_outputs,
    )
    .as_str()
    .unwrap_or_default()
    .to_owned();
    let workspace = validate_workspace(ctx, &working_dir)?;

    let profile = ctx.llm_keys.profile.as_ref().ok_or_else(|| failure(
        ctx,
        ctx.llm_keys.profile_error.as_deref().unwrap_or(
            "AGENT_CALL requires an active Codex, Claude Code, or agent-shaped custom CLI connection",
        ),
    ))?;
    let objective = objective_with_workflow_context(&ctx.plan, &ctx.step.id, &objective);
    let (program, mut args, stdin, interactive) =
        command(profile, &objective).map_err(|message| failure(ctx, message))?;
    let timeout_secs = cfg
        .timeout_secs
        .or(ctx.global_timeout_secs)
        .unwrap_or(DEFAULT_AGENT_TIMEOUT_SECS);

    tracing::info!(
        name: "inxm.executor.external.started",
        run_id = %ctx.run_id,
        plan_id = %ctx.plan.metadata.id,
        step_id = %ctx.step.id,
        runner_kind = "agent_call",
        runner_protocol = profile.label(),
        runner_workspace = %workspace.display(),
        "external runner started"
    );
    let started = std::time::Instant::now();
    let mut output = run_process(
        ctx,
        profile.protocol,
        ProcessSpawn {
            program: &program,
            args: &args,
            stdin: stdin.clone(),
            interactive,
        },
        &workspace,
        timeout_secs,
    )
    .await?;
    // Codex added `--ephemeral` to `exec` in a recent release; an older CLI
    // install rejects it outright rather than ignoring it. Drop the flag and
    // retry once instead of hard-failing on that version skew.
    if !output.success
        && matches!(profile.protocol, LlmProtocol::CodexCli)
        && crate::llm::rejects_flag(&output.stdout, &output.stderr, "--ephemeral")
    {
        args.retain(|arg| arg != "--ephemeral");
        output = run_process(
            ctx,
            profile.protocol,
            ProcessSpawn {
                program: &program,
                args: &args,
                stdin: stdin.clone(),
                interactive,
            },
            &workspace,
            timeout_secs,
        )
        .await?;
    }
    // Codex's own OS sandbox (Landlock/bwrap on Linux, Seatbelt on macOS) is
    // separate from — and additional to — the containment this runner
    // already applies (workspace confinement, process-group kill on
    // timeout). On some hosts it fails to come up at all (e.g. Ubuntu
    // 24.04+ blocking unprivileged user namespaces by default), which fails
    // every command the CLI tries to run. Auto mode retries unsandboxed
    // once rather than hard-failing on a host misconfiguration unrelated to
    // the objective; Strict mode surfaces a remediation hint instead.
    if !output.success
        && matches!(profile.protocol, LlmProtocol::CodexCli)
        && crate::llm::sandbox_init_failed(&output.stdout, &output.stderr)
    {
        if matches!(
            profile.codex_sandbox_mode,
            crate::llm::CodexSandboxMode::Auto
        ) {
            crate::llm::replace_sandbox_arg(&mut args, "danger-full-access");
            let note = "Codex's own sandbox failed to initialize on this host; retrying with \
                        --sandbox danger-full-access (this app's workspace and process \
                        containment still apply).";
            append_audit(ctx, AgentTranscriptStream::Stderr, note);
            emit_progress(ctx, AgentTranscriptStream::Stderr, note.to_owned());
            output = run_process(
                ctx,
                profile.protocol,
                ProcessSpawn {
                    program: &program,
                    args: &args,
                    stdin,
                    interactive,
                },
                &workspace,
                timeout_secs,
            )
            .await?;
        } else if matches!(
            profile.codex_sandbox_mode,
            crate::llm::CodexSandboxMode::Strict
        ) {
            return Err(failure(
                ctx,
                format!(
                    "Codex's sandbox failed to initialize.{}",
                    crate::llm::sandbox_remediation_hint()
                ),
            ));
        }
    }
    tracing::info!(
        name: "inxm.executor.external.completed",
        run_id = %ctx.run_id,
        plan_id = %ctx.plan.metadata.id,
        step_id = %ctx.step.id,
        runner_kind = "agent_call",
        runner_protocol = profile.label(),
        runner_duration_ms = started.elapsed().as_millis() as u64,
        runner_outcome = if output.success { "succeeded" } else { "failed" },
        "external runner completed"
    );
    if !output.success {
        let detail = if output.stderr.trim().is_empty() {
            output.stdout.trim()
        } else {
            output.stderr.trim()
        };
        return Err(failure(
            ctx,
            format!("agent CLI exited unsuccessfully: {detail}"),
        ));
    }
    let answer = final_answer(profile.protocol, &output.stdout)
        .unwrap_or_else(|| output.stdout.trim().to_owned());
    let usage = token_usage(profile.protocol, &output.stdout);
    let audit = ctx
        .agent_audit
        .lock()
        .expect("agent audit mutex poisoned")
        .clone();
    Ok(StepResult {
        outputs: declared_outputs(ctx, &answer),
        stdout: Some(audit.0),
        stderr: (!audit.1.is_empty()).then_some(audit.1),
        usage,
        child_runs: IndexMap::new(),
    })
}

fn validate_workspace(ctx: &StepContext, raw: &str) -> Result<PathBuf, ExecutorError> {
    if raw.trim().is_empty() {
        return Err(failure(
            ctx,
            "agent working directory resolved to an empty path",
        ));
    }
    let path = Path::new(raw).canonicalize().map_err(|error| {
        failure(
            ctx,
            format!("cannot resolve agent working directory '{raw}': {error}"),
        )
    })?;
    if !path.is_dir() {
        return Err(failure(
            ctx,
            format!(
                "agent working directory '{}' is not a directory",
                path.display()
            ),
        ));
    }
    Ok(path)
}

fn objective_with_workflow_context(
    plan: &crate::plan::types::Plan,
    step_id: &str,
    objective: &str,
) -> String {
    let mut reachable = std::collections::HashSet::from([step_id]);
    let mut downstream = Vec::new();
    loop {
        let mut changed = false;
        for step in &plan.steps {
            if reachable.contains(step.id.as_str())
                || !step
                    .depends_on
                    .iter()
                    .any(|dependency| reachable.contains(dependency.as_str()))
            {
                continue;
            }
            reachable.insert(step.id.as_str());
            downstream.push(step);
            changed = true;
        }
        if !changed {
            break;
        }
    }
    if downstream.is_empty() {
        return objective.to_owned();
    }

    let follow_up = downstream
        .iter()
        .take(8)
        .map(|step| format!("- {} ({})", step.name, step.step_type()))
        .collect::<Vec<_>>()
        .join("\n");
    let remaining = downstream.len().saturating_sub(8);
    let more = if remaining > 0 {
        format!("\n- …and {remaining} more workflow step(s)")
    } else {
        String::new()
    };
    format!(
        "{objective}\n\nWorkflow context: after you return, the plan owns these possible follow-up steps (branching may skip some):\n{follow_up}{more}\nDo not run or retry commands owned by those follow-up steps, and do not ask the operator to run them. Complete only your editing task, then return a concise summary; the downstream steps determine verification success."
    )
}

fn command(
    profile: &crate::llm::LlmProfile,
    objective: &str,
) -> Result<(String, Vec<String>, String, bool), &'static str> {
    let executable = |fallback: &str| {
        if profile.executable.trim().is_empty() {
            fallback.to_owned()
        } else {
            profile.executable.trim().to_owned()
        }
    };
    match profile.protocol {
        LlmProtocol::CodexCli => {
            let sandbox = crate::llm::initial_codex_sandbox_arg(
                profile.codex_sandbox_mode,
                "workspace-write",
            );
            let mut args = vec![
                "exec".into(),
                "--ephemeral".into(),
                "--sandbox".into(),
                sandbox.into(),
                "--skip-git-repo-check".into(),
                "--json".into(),
            ];
            if !profile.model.trim().is_empty() {
                args.extend(["--model".into(), profile.model.trim().into()]);
            }
            args.push("-".into());
            Ok((executable("codex"), args, objective.to_owned(), false))
        }
        LlmProtocol::ClaudeCli => {
            let mut args = vec![
                "-p".into(),
                "--verbose".into(),
                "--input-format".into(),
                "stream-json".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--brief".into(),
                // AGENT_CALL is an explicit opt-in to arbitrary workspace
                // edits and commands. Print-mode sessions have no operator
                // attached to approve tool calls, so pass that authorization
                // through instead of silently declining build/test commands.
                "--dangerously-skip-permissions".into(),
            ];
            if !profile.model.trim().is_empty() {
                args.extend(["--model".into(), profile.model.trim().into()]);
            }
            let message =
                serde_json::json!({"type":"user","message":{"role":"user","content":objective}});
            Ok((executable("claude"), args, format!("{message}\n"), true))
        }
        LlmProtocol::CustomCli => custom_command(&profile.command_template, objective),
        _ => Err("AGENT_CALL is unavailable for completion-only API connections"),
    }
}

fn custom_command(
    template: &str,
    objective: &str,
) -> Result<(String, Vec<String>, String, bool), &'static str> {
    let template = template.trim();
    if template.is_empty() {
        return Err("custom agent CLI command must not be empty");
    }
    let has_placeholder = template.contains(CUSTOM_CLI_PROMPT_PLACEHOLDER);
    let rendered = template.replace(CUSTOM_CLI_PROMPT_PLACEHOLDER, objective);
    let mut parts = shlex::split(&rendered).ok_or("could not parse custom agent CLI command")?;
    if parts.is_empty() {
        return Err("custom agent CLI command must not be empty");
    }
    let program = parts.remove(0);
    Ok((
        program,
        parts,
        if has_placeholder {
            String::new()
        } else {
            objective.to_owned()
        },
        // A custom CLI has no standardized mid-turn elicitation protocol.
        // Close stdin after the objective so ordinary `read-to-EOF` tools can
        // start and finish instead of hanging until the agent timeout.
        false,
    ))
}

struct ProcessOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

struct ProcessSpawn<'a> {
    program: &'a str,
    args: &'a [String],
    stdin: String,
    interactive: bool,
}

async fn run_process(
    ctx: &StepContext,
    protocol: LlmProtocol,
    spawn: ProcessSpawn<'_>,
    workspace: &Path,
    timeout_secs: u64,
) -> Result<ProcessOutput, ExecutorError> {
    let resolved = crate::hostenv::resolve_program(spawn.program);
    let mut command = tokio::process::Command::new(&resolved);
    command
        .args(spawn.args)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| {
        failure(
            ctx,
            format!(
                "failed to start agent CLI '{}': {error}",
                resolved.display()
            ),
        )
    })?;
    let mut process_stdin = child.stdin.take();
    if let Some(input) = process_stdin.as_mut() {
        input
            .write_all(spawn.stdin.as_bytes())
            .await
            .map_err(|error| failure(ctx, format!("failed to send agent objective: {error}")))?;
    }
    if !spawn.interactive {
        drop(process_stdin.take());
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_task = tokio::spawn(capture_stdout(ctx.clone(), protocol, stdout, process_stdin));
    let err_task = tokio::spawn(capture(ctx.clone(), stderr, AgentTranscriptStream::Stderr));
    let status = match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(result) => {
            result.map_err(|error| failure(ctx, format!("agent process error: {error}")))?
        }
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                unsafe { unix_process::kill_process_group(pid) };
            }
            let _ = child.start_kill();
            let _ = child.wait().await;
            // Killing closes both pipes. Drain their readers before returning
            // so the executor's audit buffer contains the final lines.
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                let _ = out_task.await;
                let _ = err_task.await;
            })
            .await;
            return Err(failure(
                ctx,
                format!("agent timed out after {timeout_secs}s"),
            ));
        }
    };
    let stdout = out_task
        .await
        .map_err(|error| failure(ctx, format!("stdout capture failed: {error}")))??;
    let stderr = err_task
        .await
        .map_err(|error| failure(ctx, format!("stderr capture failed: {error}")))??;
    Ok(ProcessOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

#[cfg(unix)]
mod unix_process {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    pub unsafe fn kill_process_group(pid: u32) {
        if let Ok(pid) = i32::try_from(pid) {
            // SAFETY: the subprocess was created as the leader of this group.
            let _ = unsafe { kill(-pid, 9) };
        }
    }
}

async fn capture<R: tokio::io::AsyncRead + Unpin>(
    ctx: StepContext,
    reader: R,
    stream: AgentTranscriptStream,
) -> Result<String, ExecutorError> {
    let mut lines = BufReader::new(reader).lines();
    let mut raw = String::new();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|error| failure(&ctx, format!("agent transcript read failed: {error}")))?
    {
        raw.push_str(&line);
        raw.push('\n');
        append_audit(&ctx, stream, &line);
        emit_progress(&ctx, stream, line);
    }
    Ok(raw)
}

async fn capture_stdout<R: tokio::io::AsyncRead + Unpin>(
    ctx: StepContext,
    protocol: LlmProtocol,
    reader: R,
    mut stdin: Option<tokio::process::ChildStdin>,
) -> Result<String, ExecutorError> {
    let mut lines = BufReader::new(reader).lines();
    let mut raw = String::new();
    while let Some(line) = lines
        .next_line()
        .await
        .map_err(|e| failure(&ctx, format!("agent transcript read failed: {e}")))?
    {
        raw.push_str(&line);
        raw.push('\n');
        emit_agent_line(&ctx, protocol, AgentTranscriptStream::Stdout, line.clone());
        // Claude's streaming input mode keeps listening for another user turn.
        // A terminal result ends this AGENT_CALL, so close stdin and let the
        // CLI exit instead of leaving an otherwise-successful step to time out.
        if is_terminal_result(protocol, &line) {
            drop(stdin.take());
        }
        let Some((request_id, prompt)) = elicitation(&line) else {
            continue;
        };
        let human = ctx.human.as_ref().ok_or_else(|| {
            failure(
                &ctx,
                "agent requested input but no interactive UI is connected",
            )
        })?;
        let (respond, receive) = tokio::sync::oneshot::channel();
        human
            .send(crate::executor::HumanRequest {
                step_id: ctx.step.id.clone(),
                prompt,
                approval_required: false,
                response_field: "agent_reply".into(),
                respond,
            })
            .map_err(|_| failure(&ctx, "interactive UI disconnected"))?;
        let reply = match receive
            .await
            .map_err(|_| failure(&ctx, "agent elicitation was dismissed"))?
        {
            crate::executor::HumanDecision::Text(v) => v,
            crate::executor::HumanDecision::Approve => "approved".into(),
            crate::executor::HumanDecision::Reject => "rejected".into(),
        };
        append_audit(&ctx, AgentTranscriptStream::Stdin, &reply);
        emit_progress(&ctx, AgentTranscriptStream::Stdin, reply.clone());
        let input = stdin
            .as_mut()
            .ok_or_else(|| failure(&ctx, "agent CLI does not support mid-turn input"))?;
        let message = serde_json::json!({"type":"user","request_id":request_id,"message":{"role":"user","content":reply}});
        input
            .write_all(format!("{message}\n").as_bytes())
            .await
            .map_err(|e| failure(&ctx, format!("failed to reply to agent: {e}")))?;
        input
            .flush()
            .await
            .map_err(|e| failure(&ctx, format!("failed to flush agent reply: {e}")))?;
    }
    Ok(raw)
}

fn emit_agent_line(
    ctx: &StepContext,
    protocol: LlmProtocol,
    stream: AgentTranscriptStream,
    content: String,
) {
    append_audit(ctx, stream, &content);
    let Some(content) = display_line(protocol, stream, &content) else {
        return;
    };
    emit_progress(ctx, stream, content);
}

fn append_audit(ctx: &StepContext, stream: AgentTranscriptStream, content: &str) {
    if let Ok(mut audit) = ctx.agent_audit.lock() {
        let target = match stream {
            AgentTranscriptStream::Stderr => &mut audit.1,
            _ => &mut audit.0,
        };
        if stream == AgentTranscriptStream::Stdin {
            target.push_str("[user] ");
        }
        target.push_str(content);
        target.push('\n');
    }
}

fn emit_progress(ctx: &StepContext, stream: AgentTranscriptStream, content: String) {
    if let Some(tx) = &ctx.progress {
        let transcript = AgentTranscriptEvent {
            run_id: ctx.run_id.clone(),
            step_id: ctx.step.id.clone(),
            stream,
            content,
        };
        let _ = tx.send(crate::executor::ProgressEvent {
            run_id: ctx.run_id.clone(),
            step_id: ctx.step.id.clone(),
            status: crate::executor::StepRunStatus::Running,
            error: None,
            iteration: None,
            fan_out_progress: None,
            transcript: Some(transcript),
        });
    }
}

fn is_terminal_result(protocol: LlmProtocol, line: &str) -> bool {
    protocol == LlmProtocol::ClaudeCli
        && serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .is_some_and(|value| value["type"] == "result")
}

/// Convert provider JSONL into compact, readable progress while the untouched
/// bytes remain in `agent_audit`. Unknown custom-CLI JSON is pretty-printed so
/// there is always a useful fallback without provider-specific configuration.
fn display_line(
    protocol: LlmProtocol,
    stream: AgentTranscriptStream,
    line: &str,
) -> Option<String> {
    if stream != AgentTranscriptStream::Stdout {
        return Some(line.to_owned());
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Some(line.to_owned());
    };
    match protocol {
        LlmProtocol::ClaudeCli => display_claude_event(&value),
        LlmProtocol::CodexCli => display_codex_event(&value),
        LlmProtocol::CustomCli => display_generic_json(&value),
        _ => display_generic_json(&value),
    }
}

fn display_claude_event(value: &serde_json::Value) -> Option<String> {
    match value["type"].as_str()? {
        "assistant" | "user" => {
            let content = value.pointer("/message/content")?.as_array()?;
            let rendered = content
                .iter()
                .filter_map(|item| match item["type"].as_str()? {
                    "text" => item["text"].as_str().map(str::to_owned),
                    "tool_use" => {
                        let name = item["name"].as_str().unwrap_or("tool");
                        let input = item.get("input").map(pretty_json).unwrap_or_default();
                        Some(format!("Using {name}\n{input}"))
                    }
                    "tool_result" => {
                        let marker = if item["is_error"].as_bool().unwrap_or(false) {
                            "Tool error"
                        } else {
                            "Tool result"
                        };
                        let content = json_text(item.get("content")?)
                            .unwrap_or_else(|| pretty_json(&item["content"]));
                        Some(format!("{marker}\n{content}"))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            (!rendered.is_empty()).then(|| rendered.join("\n\n"))
        }
        "result" => value["result"]
            .as_str()
            .map(|text| format!("Completed\n{text}")),
        // Thinking-token counters, signatures, usage, and other protocol
        // bookkeeping remain in the audit transcript but add no UI value.
        "system" => None,
        _ => display_generic_json(value),
    }
}

fn display_codex_event(value: &serde_json::Value) -> Option<String> {
    let kind = value["type"].as_str()?;
    let item = value.get("item").unwrap_or(value);
    match (kind, item["type"].as_str().unwrap_or_default()) {
        ("item.completed", "agent_message") => item["text"].as_str().map(str::to_owned),
        ("item.started" | "item.completed", "command_execution") => {
            let command =
                json_text(item.get("command")?).unwrap_or_else(|| pretty_json(&item["command"]));
            let output = item["aggregated_output"].as_str().unwrap_or_default();
            Some(if output.trim().is_empty() {
                format!("Command\n{command}")
            } else {
                format!("Command\n{command}\n\n{output}")
            })
        }
        ("turn.started" | "thread.started", _) => None,
        ("turn.completed", _) => Some("Completed".to_owned()),
        _ => display_generic_json(value),
    }
}

fn display_generic_json(value: &serde_json::Value) -> Option<String> {
    generic_result(value).or_else(|| Some(pretty_json(value)))
}

fn generic_result(value: &serde_json::Value) -> Option<String> {
    for pointer in [
        "/result",
        "/final",
        "/output",
        "/text",
        "/message/content",
        "/message",
        "/choices/0/message/content",
        "/choices/0/text",
        "/data/result",
    ] {
        if let Some(text) = value.pointer(pointer).and_then(json_text) {
            return Some(text);
        }
    }
    None
}

fn json_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(items) => {
            let parts = items
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .or_else(|| item["text"].as_str().map(str::to_owned))
                })
                .collect::<Vec<_>>();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn elicitation(line: &str) -> Option<(Option<String>, String)> {
    fn visit(value: &serde_json::Value) -> Option<(Option<String>, String)> {
        if let Some(array) = value.as_array() {
            return array.iter().find_map(visit);
        }
        let object = value.as_object()?;
        let kind = object
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let name = object
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if matches!(kind, "elicitation" | "input_required") || name == "SendUserMessage" {
            let input = object.get("input").unwrap_or(value);
            let prompt = ["prompt", "question", "message"]
                .into_iter()
                .find_map(|k| input.get(k).and_then(|v| v.as_str()))?;
            let id = object
                .get("id")
                .or_else(|| object.get("request_id"))
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            return Some((id, prompt.to_owned()));
        }
        object.values().find_map(visit)
    }
    visit(&serde_json::from_str(line).ok()?)
}

fn final_answer(protocol: LlmProtocol, raw: &str) -> Option<String> {
    if protocol == LlmProtocol::CustomCli
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(raw.trim())
    {
        return generic_result(&value).or_else(|| Some(pretty_json(&value)));
    }
    let mut answer = None;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match protocol {
            LlmProtocol::CodexCli
                if value["type"] == "item.completed"
                    && value["item"]["type"] == "agent_message" =>
            {
                answer = value["item"]["text"].as_str().map(str::to_owned);
            }
            LlmProtocol::ClaudeCli if value["type"] == "result" => {
                answer = value["result"].as_str().map(str::to_owned);
            }
            LlmProtocol::CustomCli => {
                answer = generic_result(&value).or(answer);
            }
            _ => {}
        }
    }
    answer
}

/// Extract the aggregate usage record emitted by an agent CLI. Provider CLIs
/// put this bookkeeping in their terminal event, while custom CLIs commonly
/// use either provider-style or camelCase field names.
fn token_usage(protocol: LlmProtocol, raw: &str) -> Option<TokenUsage> {
    let mut usage = serde_json::from_str::<serde_json::Value>(raw.trim())
        .ok()
        .and_then(|value| token_usage_from_value(protocol, &value));
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(event_usage) = token_usage_from_value(protocol, &value) {
            // Terminal usage events are cumulative, so the last one is the
            // authoritative total rather than another amount to add.
            usage = Some(event_usage);
        }
    }
    usage
}

fn token_usage_from_value(protocol: LlmProtocol, value: &serde_json::Value) -> Option<TokenUsage> {
    let usage = value
        .get("usage")
        .or_else(|| value.pointer("/response/usage"))
        .or_else(|| value.pointer("/data/usage"));
    if let Some(usage) = usage
        && let Some(parsed) = parse_usage_object(protocol, usage)
    {
        return Some(parsed);
    }

    // Newer Claude Code result events can expose per-model totals instead of
    // a single `usage` object. Sum those entries once, as a fallback only.
    if protocol == LlmProtocol::ClaudeCli {
        let models = value
            .get("modelUsage")
            .or_else(|| value.get("model_usage"))?
            .as_object()?;
        let mut total = TokenUsage::default();
        let mut found = false;
        for model in models.values() {
            if let Some(model_usage) = parse_usage_object(protocol, model) {
                total.add(model_usage);
                found = true;
            }
        }
        return found.then_some(total);
    }
    None
}

fn parse_usage_object(protocol: LlmProtocol, usage: &serde_json::Value) -> Option<TokenUsage> {
    fn number(value: &serde_json::Value, names: &[&str]) -> Option<u64> {
        names.iter().find_map(|name| value.get(*name)?.as_u64())
    }

    let direct_input = number(
        usage,
        &[
            "input_tokens",
            "prompt_tokens",
            "inputTokens",
            "promptTokenCount",
        ],
    );
    let output = number(
        usage,
        &[
            "output_tokens",
            "completion_tokens",
            "outputTokens",
            "candidatesTokenCount",
        ],
    );
    if direct_input.is_none() && output.is_none() {
        return None;
    }

    let mut input_tokens = direct_input.unwrap_or(0);
    // Anthropic reports uncached input, cache writes, and cache reads as
    // separate, non-overlapping counters. All three consumed agent context.
    if protocol == LlmProtocol::ClaudeCli {
        input_tokens = input_tokens
            .saturating_add(
                number(
                    usage,
                    &["cache_creation_input_tokens", "cacheCreationInputTokens"],
                )
                .unwrap_or(0),
            )
            .saturating_add(
                number(usage, &["cache_read_input_tokens", "cacheReadInputTokens"]).unwrap_or(0),
            );
    }
    Some(TokenUsage {
        input_tokens,
        output_tokens: output.unwrap_or(0),
    })
}

fn declared_outputs(ctx: &StepContext, answer: &str) -> IndexMap<String, serde_json::Value> {
    ctx.step
        .outputs
        .iter()
        .map(|name| {
            (
                name.name.clone(),
                serde_json::Value::String(answer.to_owned()),
            )
        })
        .collect()
}

fn failure(ctx: &StepContext, message: impl Into<String>) -> ExecutorError {
    ExecutorError::StepFailed {
        step_id: ctx.step.id.clone(),
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmAuth, LlmProfile};
    use crate::plan::types::{
        AgentCallConfig, Plan, PlanMetadata, PlanStep, StepConfig, ToolCallConfig,
    };

    fn profile(protocol: LlmProtocol) -> LlmProfile {
        LlmProfile {
            id: "test".into(),
            name: "test".into(),
            protocol,
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            auth: LlmAuth::None,
            headers: IndexMap::new(),
            executable: String::new(),
            command_template: String::new(),
            max_tokens: None,
            temperature: None,
            timeout_secs: 1,
            codex_sandbox_mode: crate::llm::CodexSandboxMode::default(),
        }
    }

    #[test]
    fn codex_uses_workspace_write_agent_mode() {
        let (_, args, stdin, interactive) =
            command(&profile(LlmProtocol::CodexCli), "fix it").unwrap();
        assert!(
            args.windows(2)
                .any(|v| v == ["--sandbox", "workspace-write"])
        );
        assert!(args.contains(&"--json".to_owned()));
        assert_eq!(stdin, "fix it");
        assert!(
            !interactive,
            "codex exec exposes no multi-turn stdin protocol"
        );
    }

    #[test]
    fn codex_unsandboxed_mode_skips_own_sandbox_from_the_start() {
        let mut unsandboxed = profile(LlmProtocol::CodexCli);
        unsandboxed.codex_sandbox_mode = crate::llm::CodexSandboxMode::Unsandboxed;
        let (_, args, ..) = command(&unsandboxed, "fix it").unwrap();
        assert!(
            args.windows(2)
                .any(|v| v == ["--sandbox", "danger-full-access"])
        );
        assert!(
            !args.contains(&"workspace-write".to_owned()),
            "Unsandboxed must not attempt Codex's own sandbox at all"
        );
    }

    #[test]
    fn api_protocols_are_rejected() {
        assert!(command(&profile(LlmProtocol::OpenAiChat), "fix it").is_err());
    }

    #[test]
    fn custom_cli_receives_the_objective_then_eof() {
        let (program, args, stdin, interactive) =
            custom_command("my-agent --json", "fix it").unwrap();
        assert_eq!(program, "my-agent");
        assert_eq!(args, ["--json"]);
        assert_eq!(stdin, "fix it");
        assert!(!interactive);
    }

    #[test]
    fn objective_tells_agent_that_downstream_validation_is_not_its_job() {
        let step = |id: &str, name: &str, config: StepConfig, depends_on: Vec<&str>| PlanStep {
            id: id.to_owned(),
            name: name.to_owned(),
            description: None,
            config,
            depends_on: depends_on.into_iter().map(str::to_owned).collect(),
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "feature".into(),
            description: None,
            inputs: vec![],
            config: Default::default(),
            steps: vec![
                step(
                    "implement",
                    "Implement feature",
                    StepConfig::AgentCall(AgentCallConfig {
                        objective: "Implement it".into(),
                        working_dir: ".".into(),
                        timeout_secs: None,
                    }),
                    vec![],
                ),
                step(
                    "clippy",
                    "Run cargo clippy",
                    StepConfig::ToolCall(ToolCallConfig {
                        tool: "cargo".into(),
                        arguments: Default::default(),
                    }),
                    vec!["implement"],
                ),
            ],
            outputs: vec![],
        };

        let objective = objective_with_workflow_context(&plan, "implement", "Implement it");
        assert!(objective.contains("Run cargo clippy (TOOL_CALL)"));
        assert!(objective.contains("Do not run or retry commands owned by those follow-up steps"));
        assert!(objective.contains("downstream steps determine verification success"));
    }

    #[test]
    fn extracts_final_codex_message_without_losing_raw_transcript() {
        let raw = "{\"type\":\"item.completed\",\"item\":{\"type\":\"command_execution\"}}\n{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"done\"}}\n";
        assert_eq!(
            final_answer(LlmProtocol::CodexCli, raw).as_deref(),
            Some("done")
        );
    }

    #[test]
    fn claude_uses_bidirectional_stream_json() {
        let (_, args, stdin, interactive) =
            command(&profile(LlmProtocol::ClaudeCli), "fix it").unwrap();
        assert!(
            args.windows(2)
                .any(|v| v == ["--input-format", "stream-json"])
        );
        assert!(args.contains(&"--brief".to_owned()));
        assert!(args.contains(&"--dangerously-skip-permissions".to_owned()));
        assert!(interactive);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(stdin.trim()).unwrap()["message"]["content"],
            "fix it"
        );
    }

    #[test]
    fn detects_nested_send_user_message() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"ask-1","name":"SendUserMessage","input":{"question":"Which crate?"}}]}}"#;
        assert_eq!(
            elicitation(line),
            Some((Some("ask-1".to_owned()), "Which crate?".to_owned()))
        );
    }

    #[test]
    fn claude_jsonl_is_rendered_without_protocol_noise() {
        let assistant = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Changed settings.rs"},{"type":"tool_use","name":"Bash","input":{"command":"cargo check"}}]}}"#;
        let system = r#"{"type":"system","subtype":"thinking_tokens","signature":"large-secret-protocol-blob"}"#;
        assert_eq!(
            display_line(
                LlmProtocol::ClaudeCli,
                AgentTranscriptStream::Stdout,
                assistant
            ),
            Some(
                "Changed settings.rs\n\nUsing Bash\n{\n  \"command\": \"cargo check\"\n}"
                    .to_owned()
            )
        );
        assert_eq!(
            display_line(
                LlmProtocol::ClaudeCli,
                AgentTranscriptStream::Stdout,
                system
            ),
            None
        );
    }

    #[test]
    fn claude_result_is_terminal_and_supplies_the_final_answer() {
        let result = r#"{"type":"result","subtype":"success","result":"done"}"#;
        assert!(is_terminal_result(LlmProtocol::ClaudeCli, result));
        assert_eq!(
            final_answer(LlmProtocol::ClaudeCli, result).as_deref(),
            Some("done")
        );
    }

    #[test]
    fn custom_cli_extracts_common_json_and_jsonl_result_shapes() {
        assert_eq!(
            final_answer(
                LlmProtocol::CustomCli,
                r#"{"choices":[{"message":{"content":"finished"}}]}"#
            )
            .as_deref(),
            Some("finished")
        );
        assert_eq!(
            final_answer(
                LlmProtocol::CustomCli,
                "{\"event\":\"start\"}\n{\"result\":\"finished\"}\n{\"usage\":1}\n"
            )
            .as_deref(),
            Some("finished")
        );
    }

    #[test]
    fn extracts_codex_agent_token_usage() {
        let raw = r#"{"type":"thread.started","thread_id":"test"}
{"type":"turn.completed","usage":{"input_tokens":120,"cached_input_tokens":20,"output_tokens":30}}"#;
        assert_eq!(
            token_usage(LlmProtocol::CodexCli, raw),
            Some(TokenUsage {
                input_tokens: 120,
                output_tokens: 30,
            })
        );
    }

    #[test]
    fn extracts_claude_agent_usage_including_cached_input() {
        let raw = r#"{"type":"result","result":"done","usage":{"input_tokens":10,"cache_creation_input_tokens":40,"cache_read_input_tokens":50,"output_tokens":8}}"#;
        assert_eq!(
            token_usage(LlmProtocol::ClaudeCli, raw),
            Some(TokenUsage {
                input_tokens: 100,
                output_tokens: 8,
            })
        );
    }

    #[test]
    fn extracts_custom_agent_provider_style_usage() {
        let raw = r#"{"result":"done","usage":{"prompt_tokens":7,"completion_tokens":3}}"#;
        assert_eq!(
            token_usage(LlmProtocol::CustomCli, raw),
            Some(TokenUsage {
                input_tokens: 7,
                output_tokens: 3,
            })
        );
    }
}
