//! CODE_CALL step runner.
//!
//! Executes inline or file-based scripts via a local interpreter.
//! Supported languages: Python, Bash/sh, JavaScript (node), PowerShell,
//! and Windows batch (`cmd`). Interpreters are resolved through
//! [`crate::hostenv`], so `.cmd`/`.exe` shims work on Windows and aliases
//! like `python3`/`python` are handled per platform.
//!
//! Inline scripts are written to a temporary file (deleted on drop) before
//! execution.  Stdout is parsed as JSON to populate named outputs; if stdout
//! is not valid JSON the raw text is still captured in `StepResult::stdout`.
//! A step that declares more than one output *must* print a single JSON
//! object — when that parse fails the step fails with the parse error instead
//! of silently reporting empty outputs (which reads as "produced no named
//! outputs" downstream and sends repair chasing the wrong step).

use crate::error::ExecutorError;
use crate::hostenv;
use crate::plan::types::StepConfig;
use indexmap::IndexMap;
use std::path::PathBuf;
use std::process::Output;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use super::{StepContext, StepResult, non_empty, resolve_to_optional_string, resolve_to_string};

const POWERSHELL_UTF8_WRAPPER: &str = r#"param(
    [Parameter(Mandatory = $true)][string]$InxmScriptPath,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$InxmScriptArgs
)
$InxmUtf8 = New-Object System.Text.UTF8Encoding $false
[Console]::InputEncoding = $InxmUtf8
[Console]::OutputEncoding = $InxmUtf8
$OutputEncoding = $InxmUtf8
& $InxmScriptPath @InxmScriptArgs
"#;

/// How to invoke one script language on this machine.
struct Interpreter {
    /// Resolved program path.
    program: PathBuf,
    /// Arguments placed before the script path (e.g. `-File` for PowerShell).
    prefix_args: &'static [&'static str],
    extension: &'static str,
}

/// Candidate program names per language, in preference order, plus the
/// invocation shape. Returns a user-actionable error when nothing is found.
fn interpreter_for(language: &str) -> Result<Interpreter, String> {
    let (candidates, prefix_args, extension): (&[&str], &[&str], &str) = match language {
        "python" | "python3" | "py" => (&["python3", "python"], &[], ".py"),
        "bash" | "sh" | "shell" => (&["bash", "sh"], &[], ".sh"),
        "javascript" | "js" | "node" => (&["node"], &[], ".js"),
        "powershell" | "pwsh" | "ps1" => (
            &["pwsh", "powershell"],
            &["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"],
            ".ps1",
        ),
        "cmd" | "bat" | "batch" => (&["cmd"], &["/C"], ".bat"),
        other => return Err(format!("unsupported script language: '{other}'")),
    };

    candidates
        .iter()
        .find_map(|name| hostenv::find_on_path(name))
        .map(|program| Interpreter {
            program,
            prefix_args,
            extension,
        })
        .ok_or_else(|| {
            format!(
                "no interpreter for '{language}' on this machine (looked for: {}) — \
                 recompile the plan; the compiler is told which interpreters exist here",
                candidates.join(", ")
            )
        })
}

pub async fn run(ctx: &StepContext) -> Result<StepResult, ExecutorError> {
    let cfg = match &ctx.step.config {
        StepConfig::CodeCall(c) => c,
        _ => {
            return Err(ExecutorError::StepFailed {
                step_id: ctx.step.id.clone(),
                message: "expected CODE_CALL config".to_owned(),
            });
        }
    };
    if let Some(source) = &cfg.inline {
        reject_executable_placeholders(ctx, "inline", source)?;
    }
    if let Some(file) = &cfg.file {
        reject_executable_placeholders(ctx, "file", file)?;
    }

    let language = cfg.language.to_lowercase();

    let interpreter = interpreter_for(&language).map_err(|message| ExecutorError::StepFailed {
        step_id: ctx.step.id.clone(),
        message,
    })?;
    let extension = interpreter.extension;

    // Resolve every runtime-configurable string before constructing the
    // process. Validation scans these same fields, so none may reach the
    // interpreter as a literal `${...}` placeholder.
    let mut resolved_env: IndexMap<String, String> = cfg
        .env
        .iter()
        .map(|(key, value)| (key.clone(), resolve_to_string(value, ctx)))
        .collect();
    // Force UTF-8 I/O for Python unless the step overrides it: Windows
    // Python otherwise decodes pipes with the ANSI code page, turning UTF-8
    // stdin into mojibake plus surrogateescape'd bytes that `json.dumps`
    // then emits as lone `\udXXX` escapes — invalid JSON that breaks the
    // named-output mapping below.
    if matches!(language.as_str(), "python" | "python3" | "py") {
        for (key, value) in [("PYTHONUTF8", "1"), ("PYTHONIOENCODING", "utf-8")] {
            if !resolved_env.contains_key(key) {
                resolved_env.insert(key.to_owned(), value.to_owned());
            }
        }
    }
    let resolved_args: Vec<String> = cfg
        .args
        .iter()
        .map(|value| resolve_to_string(value, ctx))
        .collect();
    let resolved_stdin = cfg
        .stdin
        .as_deref()
        .map(|value| resolve_to_string(value, ctx));
    // A step's own `working_dir` wins when set; otherwise fall back to the
    // plan-wide `root_directory` input. That input may now be `required:
    // false` with no value supplied at runtime — in that case there is no
    // user-provided path to fall back to, so the step runs in a managed
    // per-run scratch workspace instead of the app's cwd (never leave the
    // working directory undefined).
    let root_directory_value = ctx
        .plan
        .config
        .get(&format!(
            "input.{}",
            crate::plan::types::ROOT_DIRECTORY_INPUT
        ))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);

    let resolved_working_dir = match cfg
        .working_dir
        .as_deref()
        .and_then(|value| resolve_to_optional_string(value, ctx))
        .filter(|value| !value.is_empty())
        .or(root_directory_value)
    {
        Some(dir) => dir,
        None => scratch_workspace_dir(ctx)
            .map_err(|e| ExecutorError::StepFailed {
                step_id: ctx.step.id.clone(),
                message: format!("failed to prepare scratch workspace: {e}"),
            })?
            .to_string_lossy()
            .into_owned(),
    };
    // Spawning with a missing cwd yields the same ENOENT as a missing
    // interpreter, which reads as "bash not found" — catch it here instead.
    if !std::path::Path::new(&resolved_working_dir).is_dir() {
        return Err(ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: format!(
                "working directory '{resolved_working_dir}' does not exist — \
                 supply a valid `root_directory` input or fix the step's `working_dir`"
            ),
        });
    }

    let timeout_secs = match (cfg.timeout_secs, ctx.global_timeout_secs) {
        (Some(code_call_timeout), Some(step_timeout)) => Some(code_call_timeout.min(step_timeout)),
        (Some(code_call_timeout), None) => Some(code_call_timeout),
        (None, step_timeout) => step_timeout,
    };

    // Prepare the script path; hold `_guard` so the temp file lives until
    // after the process exits.
    let (script_path, _guard): (PathBuf, Option<TempScript>) = if let Some(src) = &cfg.inline {
        let script = TempScript::write(extension, src).map_err(ExecutorError::Io)?;
        (script.0.clone(), Some(script))
    } else if let Some(file) = &cfg.file {
        (PathBuf::from(file), None)
    } else {
        return Err(ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: "CODE_CALL has neither 'inline' nor 'file' set".to_owned(),
        });
    };

    let mut cmd = Command::new(&interpreter.program);
    cmd.args(interpreter.prefix_args);
    // Windows PowerShell writes redirected output using a legacy code page by
    // default. Run PowerShell scripts through an ASCII wrapper that selects
    // UTF-8 before loading the real script. The script still receives its
    // environment through Windows' native UTF-16 environment block.
    let _powershell_wrapper = if matches!(language.as_str(), "powershell" | "pwsh" | "ps1") {
        let wrapper =
            TempScript::write(".ps1", POWERSHELL_UTF8_WRAPPER).map_err(ExecutorError::Io)?;
        cmd.arg(&wrapper.0);
        Some(wrapper)
    } else {
        None
    };
    cmd.arg(&script_path);
    cmd.args(&resolved_args);
    if resolved_stdin.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    } else {
        cmd.stdin(std::process::Stdio::null());
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    cmd.creation_flags(windows_process::CREATE_SUSPENDED);

    cmd.current_dir(&resolved_working_dir);
    for (k, v) in &resolved_env {
        cmd.env(k, v);
    }

    let started = std::time::Instant::now();
    tracing::info!(
        name: "inxm.executor.external.started",
        run_id = %ctx.run_id,
        plan_id = %ctx.plan.metadata.id,
        step_id = %ctx.step.id,
        runner_kind = "code_call",
        runner_language = %language,
        "external runner started"
    );
    let mut child = cmd.spawn().map_err(|e| {
        tracing::warn!(
            name: "inxm.executor.external.completed",
            run_id = %ctx.run_id,
            plan_id = %ctx.plan.metadata.id,
            step_id = %ctx.step.id,
            runner_kind = "code_call",
            runner_language = %language,
            runner_duration_ms = started.elapsed().as_millis() as u64,
            runner_outcome = "failed",
            failure_class = "spawn",
            "external runner completed"
        );
        ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: format!(
                "failed to spawn interpreter '{}': {e}",
                interpreter.program.display()
            ),
        }
    })?;
    // Create containment before any I/O with the child. On Windows this
    // assigns the process to a kill-on-close Job Object immediately after
    // spawn, so children it creates inherit the job from the outset.
    let mut containment = match ProcessContainment::for_child(&child) {
        Ok(containment) => containment,
        Err(error) => {
            // The process exists even though setup failed, so synchronously
            // reap it before exposing the setup error to the caller.
            terminate_uncontained_child(&mut child).await;
            return Err(ExecutorError::StepFailed {
                step_id: ctx.step.id.clone(),
                message: format!("failed to contain script process tree: {error}"),
            });
        }
    };
    if let Err(error) = containment.resume_child(&child) {
        terminate_process_tree(&mut child, &mut containment).await;
        return Err(ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: format!("failed to start contained script process: {error}"),
        });
    }

    if let Some(input) = resolved_stdin {
        let mut stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                terminate_process_tree(&mut child, &mut containment).await;
                return Err(ExecutorError::StepFailed {
                    step_id: ctx.step.id.clone(),
                    message: "failed to open script stdin".to_owned(),
                });
            }
        };
        if let Err(error) = stdin.write_all(input.as_bytes()).await {
            terminate_process_tree(&mut child, &mut containment).await;
            return Err(ExecutorError::StepFailed {
                step_id: ctx.step.id.clone(),
                message: format!("failed to write script stdin: {error}"),
            });
        }
        drop(stdin);
    }

    let output = wait_for_output(&mut child, &mut containment, timeout_secs)
        .await
        .map_err(|failure| {
            let failure_class = match &failure {
                ChildFailure::TimedOut { .. } => "timeout",
                ChildFailure::OutputLimit { .. } => "output_limit",
                ChildFailure::Io(_) => "io",
            };
            tracing::warn!(
                name: "inxm.executor.external.completed",
                run_id = %ctx.run_id,
                plan_id = %ctx.plan.metadata.id,
                step_id = %ctx.step.id,
                runner_kind = "code_call",
                runner_language = %language,
                runner_duration_ms = started.elapsed().as_millis() as u64,
                runner_outcome = "failed",
                failure_class,
                "external runner completed"
            );
            ExecutorError::StepFailed {
                step_id: ctx.step.id.clone(),
                message: match failure {
                    ChildFailure::TimedOut { secs } => format!("script timed out after {secs}s"),
                    ChildFailure::OutputLimit { stream, limit } => {
                        format!("{OUTPUT_LIMIT_MARKER}; {stream} maximum is {limit} bytes")
                    }
                    ChildFailure::Io(error) => format!("script I/O error: {error}"),
                },
            }
        })?;
    tracing::info!(
        name: "inxm.executor.external.completed",
        run_id = %ctx.run_id,
        plan_id = %ctx.plan.metadata.id,
        step_id = %ctx.step.id,
        runner_kind = "code_call",
        runner_language = %language,
        runner_duration_ms = started.elapsed().as_millis() as u64,
        runner_outcome = if output.status.success() { "succeeded" } else { "failed" },
        failure_class = if output.status.success() { "none" } else { "exit_status" },
        "external runner completed"
    );

    let stdout_str = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr_str = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        let detail = if stderr_str.trim().is_empty() {
            String::new()
        } else {
            format!(": {}", stderr_str.trim())
        };
        return Err(ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: format!("script exited with code {:?}{detail}", output.status.code()),
        });
    }

    // Parse stdout as JSON for named outputs.
    let mut stderr_str = stderr_str;
    let mut outputs: IndexMap<String, serde_json::Value> = IndexMap::new();
    let mut parse_error: Option<String> = None;
    match serde_json::from_str::<serde_json::Value>(&stdout_str) {
        Ok(serde_json::Value::Object(map)) => outputs.extend(map),
        Ok(_) => {}
        Err(error) => {
            // Python's `json.dumps` may emit lone UTF-16 surrogate escapes
            // (surrogateescape'd undecodable bytes) — legal for Python's own
            // parser, invalid per RFC 8259. Recover the outputs instead of
            // discarding an otherwise well-formed object.
            let recovered = sanitize_lone_surrogate_escapes(&stdout_str)
                .and_then(|sanitized| serde_json::from_str(&sanitized).ok());
            if let Some(serde_json::Value::Object(map)) = recovered {
                outputs.extend(map);
                stderr_str.push_str(
                    "\n[inxm] warning: stdout contained lone UTF-16 surrogate escapes \
                     (invalid JSON, typically Python json.dumps of bytes decoded with \
                     surrogateescape); they were replaced with U+FFFD to recover the \
                     step outputs. Make the script read and write UTF-8 to avoid this.",
                );
            } else {
                parse_error = Some(error.to_string());
            }
        }
    }

    // Same contract as TOOL_CALL: a single declared-but-unfilled output
    // receives the script's stdout, so plain-text scripts still satisfy
    // `${step.<id>.<output>}` references.
    super::tool_call::fill_declared_output(
        &mut outputs,
        &ctx.step.outputs,
        &serde_json::Value::Null,
        &stdout_str,
    );

    // With two or more declared outputs there is no plain-text fallback:
    // only a JSON object on stdout can satisfy them. Reporting success with
    // empty outputs here surfaces later as "step produced no named outputs"
    // on whichever step references them — a misleading message that repair
    // then chases into this step's (working) logic. Fail here, with the real
    // parse error, instead.
    if outputs.is_empty() && ctx.step.outputs.len() > 1 && !stdout_str.trim().is_empty() {
        let declared = ctx
            .step
            .outputs
            .iter()
            .map(|output| output.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let detail = match parse_error {
            Some(error) => format!("stdout is not a valid JSON object ({error})"),
            None => "stdout is valid JSON but not an object".to_owned(),
        };
        return Err(ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: format!(
                "script succeeded but its stdout cannot satisfy the declared outputs \
                 ({declared}): {detail}. A CODE_CALL step with more than one declared \
                 output must print exactly one JSON object whose keys are the declared \
                 output names."
            ),
        });
    }

    Ok(StepResult {
        outputs,
        stdout: non_empty(stdout_str),
        stderr: non_empty(stderr_str),
        usage: None,
        child_runs: IndexMap::new(),
    })
}

/// Replace `\uD800`–`\uDFFF` escapes that do not form a valid surrogate pair
/// with `�` (the replacement character). Returns `None` when the input
/// contains no lone surrogate escapes, so callers can skip the re-parse.
///
/// Only escape sequences are touched — a literal `\\u` (escaped backslash
/// followed by `u`) is copied through untouched, and valid high+low pairs
/// (how JSON encodes astral-plane characters such as emoji) are preserved.
fn sanitize_lone_surrogate_escapes(input: &str) -> Option<String> {
    const ESCAPE_LEN: usize = 6; // \uXXXX

    fn unicode_escape_at(bytes: &[u8], at: usize) -> Option<u16> {
        if bytes.len() < at + ESCAPE_LEN || bytes[at] != b'\\' || bytes[at + 1] != b'u' {
            return None;
        }
        let hex = std::str::from_utf8(&bytes[at + 2..at + ESCAPE_LEN]).ok()?;
        u16::from_str_radix(hex, 16).ok()
    }

    let bytes = input.as_bytes();
    let mut sanitized = Vec::with_capacity(bytes.len());
    let mut changed = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            sanitized.extend_from_slice(b"\\\\");
            i += 2;
            continue;
        }
        if let Some(unit) = unicode_escape_at(bytes, i) {
            if (0xD800..=0xDBFF).contains(&unit) {
                if unicode_escape_at(bytes, i + ESCAPE_LEN)
                    .is_some_and(|low| (0xDC00..=0xDFFF).contains(&low))
                {
                    sanitized.extend_from_slice(&bytes[i..i + 2 * ESCAPE_LEN]);
                    i += 2 * ESCAPE_LEN;
                    continue;
                }
                sanitized.extend_from_slice(b"\\uFFFD");
                changed = true;
                i += ESCAPE_LEN;
                continue;
            }
            if (0xDC00..=0xDFFF).contains(&unit) {
                sanitized.extend_from_slice(b"\\uFFFD");
                changed = true;
                i += ESCAPE_LEN;
                continue;
            }
        }
        sanitized.push(bytes[i]);
        i += 1;
    }
    if !changed {
        return None;
    }
    // Replacements are pure ASCII and everything else is copied verbatim from
    // a valid UTF-8 `&str`, so the buffer is valid UTF-8 by construction.
    Some(String::from_utf8(sanitized).expect("sanitized JSON stays valid UTF-8"))
}

fn reject_executable_placeholders(
    ctx: &StepContext,
    field: &str,
    source: &str,
) -> Result<(), ExecutorError> {
    if super::contains_plan_placeholder(source) {
        return Err(ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: format!(
                "CODE_CALL config.{field} must be static; pass runtime values through args or stdin"
            ),
        });
    }
    Ok(())
}

/// Byte caps for a script's captured output. A CODE_CALL runs model-written
/// code, so the same limits the tool adapters enforce apply here — otherwise a
/// runaway script buffers without bound into the app process (the step timeout
/// does not help: the readers run in their own tasks).
const MAX_SCRIPT_STDOUT_BYTES: usize = 1_048_576;
const MAX_SCRIPT_STDERR_BYTES: usize = 262_144;
/// Prefix shared with the tool adapters so the repair classifier sees a
/// familiar shape.
const OUTPUT_LIMIT_MARKER: &str = "[truncated: script output limit exceeded]";

enum ChildFailure {
    TimedOut { secs: u64 },
    OutputLimit { stream: &'static str, limit: usize },
    Io(std::io::Error),
}

/// Bytes captured from one stream, plus whether the cap was hit.
struct BoundedBytes {
    bytes: Vec<u8>,
    overflow: bool,
}

type ReaderJoinResult = Result<std::io::Result<BoundedBytes>, tokio::task::JoinError>;

enum ChildEvent {
    Exited(std::io::Result<std::process::ExitStatus>),
    Stdout(ReaderJoinResult),
    Stderr(ReaderJoinResult),
    TimedOut(u64),
}

async fn wait_for_output(
    child: &mut tokio::process::Child,
    containment: &mut ProcessContainment,
    timeout_secs: Option<u64>,
) -> Result<Output, ChildFailure> {
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_process_tree(child, containment).await;
            return Err(ChildFailure::Io(std::io::Error::other(
                "script stdout was not captured",
            )));
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_process_tree(child, containment).await;
            return Err(ChildFailure::Io(std::io::Error::other(
                "script stderr was not captured",
            )));
        }
    };
    let mut stdout_task = tokio::spawn(read_stream(stdout, MAX_SCRIPT_STDOUT_BYTES));
    let mut stderr_task = tokio::spawn(read_stream(stderr, MAX_SCRIPT_STDERR_BYTES));
    let timeout = async move {
        match timeout_secs {
            Some(secs) => {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                secs
            }
            None => std::future::pending::<u64>().await,
        }
    };
    tokio::pin!(timeout);

    let mut stdout_result = None;
    let mut stderr_result = None;
    let status = loop {
        let event = tokio::select! {
            result = child.wait() => ChildEvent::Exited(result),
            result = &mut stdout_task, if stdout_result.is_none() => ChildEvent::Stdout(result),
            result = &mut stderr_task, if stderr_result.is_none() => ChildEvent::Stderr(result),
            secs = &mut timeout => ChildEvent::TimedOut(secs),
        };

        match event {
            ChildEvent::Exited(result) => {
                let status = result.map_err(ChildFailure::Io)?;
                // Background children inherit the leader's pipes. Close
                // containment before joining those readers so a successful
                // leader can never leave this future blocked on an orphaned
                // descendant that keeps stdout or stderr open.
                containment.finish_after_leader_exit();
                break status;
            }
            ChildEvent::Stdout(result) => match reader_result(result) {
                Ok(stdout) if stdout.overflow => {
                    terminate_process_tree(child, containment).await;
                    stderr_task.abort();
                    let _ = stderr_task.await;
                    return Err(ChildFailure::OutputLimit {
                        stream: "stdout",
                        limit: MAX_SCRIPT_STDOUT_BYTES,
                    });
                }
                Ok(stdout) => stdout_result = Some(stdout),
                Err(failure) => {
                    terminate_process_tree(child, containment).await;
                    stderr_task.abort();
                    let _ = stderr_task.await;
                    return Err(failure);
                }
            },
            ChildEvent::Stderr(result) => match reader_result(result) {
                Ok(stderr) if stderr.overflow => {
                    terminate_process_tree(child, containment).await;
                    stdout_task.abort();
                    let _ = stdout_task.await;
                    return Err(ChildFailure::OutputLimit {
                        stream: "stderr",
                        limit: MAX_SCRIPT_STDERR_BYTES,
                    });
                }
                Ok(stderr) => stderr_result = Some(stderr),
                Err(failure) => {
                    terminate_process_tree(child, containment).await;
                    stdout_task.abort();
                    let _ = stdout_task.await;
                    return Err(failure);
                }
            },
            ChildEvent::TimedOut(secs) => {
                terminate_process_tree(child, containment).await;
                stdout_task.abort();
                stderr_task.abort();
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(ChildFailure::TimedOut { secs });
            }
        }
    };
    let stdout = match stdout_result {
        Some(stdout) => stdout,
        None => join_reader(stdout_task).await?,
    };
    let stderr = match stderr_result {
        Some(stderr) => stderr,
        None => join_reader(stderr_task).await?,
    };
    if stdout.overflow || stderr.overflow {
        // The leader has already been reaped, but a descendant may still be
        // holding a pipe. Containment is closed before reporting the cap.
        containment.finish_after_output();
        let (stream, limit) = match stdout.overflow {
            true => ("stdout", MAX_SCRIPT_STDOUT_BYTES),
            false => ("stderr", MAX_SCRIPT_STDERR_BYTES),
        };
        return Err(ChildFailure::OutputLimit { stream, limit });
    }
    // A successful leader can leave background descendants. Readers have
    // drained at this point, so closing containment cannot truncate captured
    // output and guarantees no process tree escapes this CODE_CALL.
    containment.finish_after_output();
    Ok(Output {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

async fn read_stream<R>(stream: R, limit: usize) -> std::io::Result<BoundedBytes>
where
    R: tokio::io::AsyncRead + Unpin,
{
    // Read one byte past the cap so overflow is detectable, then trim back.
    let mut stream = stream.take((limit + 1) as u64);
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await?;
    let overflow = bytes.len() > limit;
    bytes.truncate(limit);
    Ok(BoundedBytes { bytes, overflow })
}

async fn join_reader(
    task: tokio::task::JoinHandle<std::io::Result<BoundedBytes>>,
) -> Result<BoundedBytes, ChildFailure> {
    reader_result(task.await)
}

fn reader_result(result: ReaderJoinResult) -> Result<BoundedBytes, ChildFailure> {
    result
        .map_err(|error| ChildFailure::Io(std::io::Error::other(error.to_string())))?
        .map_err(ChildFailure::Io)
}

async fn terminate_process_tree(
    child: &mut tokio::process::Child,
    containment: &mut ProcessContainment,
) {
    containment.terminate_before_leader_reap();
    match child.start_kill() {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
        Err(_) => {}
    }
    // Re-sweep the dedicated group before reaping the leader. This catches a
    // descendant that was forked while the first group signal was delivered.
    containment.resweep_descendants_before_leader_reap();
    let _ = child.wait().await;
    containment.finish_after_output();
}

async fn terminate_uncontained_child(child: &mut tokio::process::Child) {
    match child.start_kill() {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
        Err(_) => {}
    }
    let _ = child.wait().await;
}

/// Owns every process created by a CODE_CALL until its pipes are drained and
/// cleanup is complete. Its destructor is deliberately destructive: a future
/// cancelled between `spawn` and `wait_for_output` still cannot orphan the
/// process tree.
struct ProcessContainment {
    #[cfg(unix)]
    unix: unix_process::ProcessTree,
    #[cfg(windows)]
    windows: windows_process::Job,
}

impl ProcessContainment {
    fn for_child(child: &tokio::process::Child) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            unix_process::ProcessTree::for_child(child).map(|unix| Self { unix })
        }
        #[cfg(windows)]
        {
            return windows_process::Job::for_child(child).map(|windows| Self { windows });
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Ok(Self {})
        }
    }

    fn terminate_before_leader_reap(&mut self) {
        #[cfg(unix)]
        self.unix.terminate_before_leader_reap();
        #[cfg(windows)]
        self.windows.terminate();
    }

    fn resweep_descendants_before_leader_reap(&mut self) {
        #[cfg(unix)]
        self.unix.resweep_descendants_before_leader_reap();
    }

    fn resume_child(&mut self, child: &tokio::process::Child) -> std::io::Result<()> {
        #[cfg(windows)]
        return self.windows.resume_suspended_child(child);
        #[cfg(not(windows))]
        {
            let _ = child;
            Ok(())
        }
    }

    fn finish_after_output(&mut self) {
        #[cfg(windows)]
        self.windows.close();
    }

    fn finish_after_leader_exit(&mut self) {
        #[cfg(unix)]
        self.unix.kill_group();
        #[cfg(windows)]
        self.windows.close();
    }

    #[cfg(all(test, windows))]
    fn contains_child(&self, child: &tokio::process::Child) -> std::io::Result<bool> {
        self.windows.contains(child)
    }
}

#[cfg(unix)]
impl Drop for ProcessContainment {
    fn drop(&mut self) {
        self.unix.kill_group();
    }
}

#[cfg(unix)]
mod unix_process {
    const SIGKILL: i32 = 9;

    /// Unix process-group containment for one CODE_CALL.
    pub struct ProcessTree {
        process_group_id: i32,
    }

    impl ProcessTree {
        pub fn for_child(child: &tokio::process::Child) -> std::io::Result<Self> {
            let pid = child
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .ok_or_else(|| std::io::Error::other("spawned child has no usable process ID"))?;
            Ok(Self {
                // `Command::process_group(0)` makes the child the leader.
                process_group_id: pid,
            })
        }

        pub fn terminate_before_leader_reap(&mut self) {
            self.kill_group();
        }

        pub fn resweep_descendants_before_leader_reap(&mut self) {
            self.kill_group();
        }

        pub fn kill_group(&self) {
            // SAFETY: `process_group_id` came from the just-spawned child
            // configured with `process_group(0)`. `kill` receives only an
            // integer PID and a constant signal; it dereferences no pointers.
            unsafe { kill(-self.process_group_id, SIGKILL) };
        }
    }

    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    #[cfg(test)]
    pub unsafe fn process_is_running(pid: i32) -> bool {
        const NO_SIGNAL: i32 = 0;
        // SAFETY: signal 0 performs existence/permission checking only.
        if unsafe { kill(pid, NO_SIGNAL) } != 0 {
            return false;
        }
        true
    }
}

#[cfg(windows)]
mod windows_process {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    #[cfg(test)]
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED as WINDOWS_CREATE_SUSPENDED, OpenThread, ResumeThread,
        THREAD_SUSPEND_RESUME,
    };

    pub const CREATE_SUSPENDED: u32 = WINDOWS_CREATE_SUSPENDED;

    /// A single owned Windows kernel handle. It is used for the ToolHelp
    /// snapshot and primary thread so every early error path closes handles.
    struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        fn new(handle: HANDLE) -> std::io::Result<Self> {
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self(handle))
        }

        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: this wrapper owns the kernel HANDLE exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Owned Job Object handle. Closing the last handle synchronously asks
    /// Windows to terminate every assigned process, including descendants.
    pub struct Job {
        handle: Option<HANDLE>,
    }

    // SAFETY: a Windows HANDLE is a process-wide kernel reference with no
    // thread affinity. This guard serializes ownership through `&mut self`,
    // and closes the handle exactly once in `close`/Drop.
    unsafe impl Send for Job {}

    impl Job {
        pub fn for_child(child: &tokio::process::Child) -> std::io::Result<Self> {
            let process = child
                .raw_handle()
                .map(|handle| handle as HANDLE)
                .ok_or_else(|| std::io::Error::other("spawned child has no process handle"))?;
            // SAFETY: null security attributes and name request a private Job
            // Object owned exclusively by this guard. Windows returns either
            // a valid owned HANDLE or null, which is checked immediately.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let mut job = Self {
                handle: Some(handle),
            };
            // SAFETY: zero is the documented initialization for this C
            // structure; we set only the supported limit flag and pass its
            // exact address and byte size to the Windows API.
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `job.handle` is valid while `job` owns it, and `limits`
            // remains alive for this synchronous call with its exact layout.
            let configured = unsafe {
                SetInformationJobObject(
                    job.handle.expect("new Job always has a handle"),
                    JobObjectExtendedLimitInformation,
                    &limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if configured == 0 {
                let error = std::io::Error::last_os_error();
                job.close();
                return Err(error);
            }
            // SAFETY: `process` is borrowed from the live Tokio child and the
            // private Job remains owned by `job`. Assignment happens directly
            // after spawn, before stdin or output handling can run.
            let assigned = unsafe {
                AssignProcessToJobObject(job.handle.expect("configured Job has a handle"), process)
            };
            if assigned == 0 {
                let error = std::io::Error::last_os_error();
                job.close();
                return Err(error);
            }
            Ok(job)
        }

        pub fn terminate(&mut self) {
            self.close();
        }

        /// Resumes the primary thread only after its process is assigned to
        /// this Job Object. The child was created with CREATE_SUSPENDED, so it
        /// could not create descendants during the assignment window.
        pub fn resume_suspended_child(&self, child: &tokio::process::Child) -> std::io::Result<()> {
            let process_id = child
                .id()
                .ok_or_else(|| std::io::Error::other("suspended child has no process ID"))?;
            // SAFETY: snapshot flags are a documented constant and zero
            // process id requests a system-wide thread snapshot.
            let snapshot =
                OwnedHandle::new(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) })?;
            // SAFETY: zeroed is the documented initializer; dwSize is set to
            // its exact ABI size before passing writable storage to ToolHelp.
            let mut entry: THREADENTRY32 = unsafe { zeroed() };
            entry.dwSize = size_of::<THREADENTRY32>() as u32;
            // SAFETY: `snapshot` is valid and `entry` points to initialized,
            // writable storage with the required `dwSize` field populated.
            let mut has_entry = unsafe { Thread32First(snapshot.raw(), &mut entry) } != 0;
            while has_entry && entry.th32OwnerProcessID != process_id {
                entry.dwSize = size_of::<THREADENTRY32>() as u32;
                // SAFETY: same valid snapshot and writable entry invariant as
                // Thread32First above.
                has_entry = unsafe { Thread32Next(snapshot.raw(), &mut entry) } != 0;
            }
            if !has_entry {
                return Err(std::io::Error::other(
                    "could not locate suspended child primary thread",
                ));
            }
            // SAFETY: this thread id came from the suspended child process
            // snapshot. The returned handle is owned by this scope.
            let thread = OwnedHandle::new(unsafe {
                OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID)
            })?;
            // SAFETY: `thread` is a valid owned handle with suspend/resume
            // rights. A u32::MAX result is the documented failure sentinel.
            if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }

        pub fn close(&mut self) {
            if let Some(handle) = self.handle.take() {
                // SAFETY: this guard owns the HANDLE exactly once. Closing it
                // releases the final Job handle and activates KILL_ON_JOB_CLOSE.
                unsafe { CloseHandle(handle) };
            }
        }

        #[cfg(test)]
        pub fn contains(&self, child: &tokio::process::Child) -> std::io::Result<bool> {
            let process = child
                .raw_handle()
                .map(|handle| handle as HANDLE)
                .ok_or_else(|| std::io::Error::other("child handle unavailable"))?;
            let mut contained = 0;
            // SAFETY: both handles are valid borrowed kernel handles for the
            // duration of this call, and `contained` is writable BOOL storage.
            let result = unsafe {
                IsProcessInJob(
                    process,
                    self.handle
                        .expect("open Job required for containment query"),
                    &mut contained,
                )
            };
            if result == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(contained != 0)
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            self.close();
        }
    }
}

// ─── Temp-file guard ──────────────────────────────────────────────────────────

/// Deletes the wrapped path when dropped.
struct TempScript(PathBuf);

impl TempScript {
    fn write(extension: &str, contents: &str) -> std::io::Result<Self> {
        let name = format!("inxm_{}{extension}", uuid::Uuid::new_v4().as_simple());
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, contents)?;
        Ok(Self(path))
    }
}

impl Drop for TempScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Per-run scratch workspace for a CODE_CALL step whose `root_directory`
/// input is optional and was not supplied at runtime: `.inxm/runs/<run-id>/workspace`
/// under the app's managed data directory (`ctx.storage_root`). Created
/// (idempotently) before the step runs, and isolated per run id so
/// concurrent/repeated runs never share a cwd.
fn scratch_workspace_dir(ctx: &StepContext) -> std::io::Result<PathBuf> {
    let dir = ctx
        .storage_root
        .join("runs")
        .join(&ctx.run_id)
        .join("workspace");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod scratch_workspace_tests {
    use super::*;
    use crate::plan::types::{CodeCallConfig, Plan, PlanMetadata, PlanStep, StepConfig};
    use crate::tools::catalog::ToolCatalog;
    use indexmap::IndexMap;

    fn make_ctx(
        cfg: CodeCallConfig,
        run_id: &str,
        plan_config: IndexMap<String, serde_json::Value>,
        storage_root: PathBuf,
    ) -> StepContext {
        let step = PlanStep {
            id: "script".to_owned(),
            name: "script".to_owned(),
            description: None,
            config: StepConfig::CodeCall(cfg),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        StepContext {
            plan: std::sync::Arc::new(Plan {
                metadata: PlanMetadata::new(None),
                name: "test".to_owned(),
                description: None,
                inputs: vec![],
                config: plan_config,
                steps: vec![step.clone()],
                outputs: vec![],
            }),
            step,
            step_outputs: IndexMap::new().into(),
            catalog: ToolCatalog::default(),
            global_timeout_secs: None,
            human: None,
            run_id: run_id.to_owned(),
            progress: None,
            child_progress: None,
            llm_keys: Default::default(),
            storage_root,
            agent_audit: Default::default(),
        }
    }

    fn bash_config(script: &str) -> CodeCallConfig {
        CodeCallConfig {
            language: "bash".to_owned(),
            inline: Some(script.to_owned()),
            file: None,
            args: vec![],
            stdin: None,
            env: IndexMap::new(),
            working_dir: None,
            timeout_secs: None,
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}_{}", uuid::Uuid::new_v4().as_simple()))
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_preserves_unicode_environment_values() {
        const UNICODE: &str = "Umlaute äöüßÄÖÜ € Euro — Gedankenstrich 日本語 🚀 Emoji";

        let storage_root = unique_temp_dir("inxm_test_storage");
        let mut cfg = CodeCallConfig {
            language: "powershell".to_owned(),
            inline: Some("[Console]::Out.Write($env:INXM_UNICODE)".to_owned()),
            file: None,
            args: vec![],
            stdin: None,
            env: IndexMap::new(),
            working_dir: None,
            timeout_secs: None,
        };
        cfg.env
            .insert("INXM_UNICODE".to_owned(), UNICODE.to_owned());
        let ctx = make_ctx(
            cfg,
            "run-unicode-env",
            IndexMap::new(),
            storage_root.clone(),
        );

        let result = run(&ctx).await.expect("PowerShell script should run");

        assert_eq!(result.stdout.as_deref(), Some(UNICODE));
        let _ = std::fs::remove_dir_all(storage_root);
    }

    /// A CODE_CALL plan may declare `root_directory` as `required:
    /// false` with no value supplied at runtime — resolved as `Value::Null`
    /// by `Plan::resolve_inputs` (see `plan::types`). The executor must not
    /// fail and must not run in the app's cwd; it must use an isolated,
    /// pre-created per-run scratch workspace instead.
    #[tokio::test]
    async fn unset_optional_root_directory_runs_in_scratch_workspace() {
        let storage_root = unique_temp_dir("inxm_test_storage");
        let cfg = bash_config(r#"echo "{\"cwd\": \"$(pwd)\"}""#);
        let mut plan_config = IndexMap::new();
        plan_config.insert("input.root_directory".to_owned(), serde_json::Value::Null);
        let ctx = make_ctx(cfg, "run-unset", plan_config, storage_root.clone());

        let result = run(&ctx).await.expect("script should run");

        let expected = storage_root
            .join("runs")
            .join("run-unset")
            .join("workspace");
        assert!(
            expected.is_dir(),
            "scratch workspace should have been created at {expected:?}"
        );
        let cwd = result.outputs["cwd"].as_str().expect("cwd output present");
        assert_eq!(
            std::fs::canonicalize(cwd).unwrap(),
            std::fs::canonicalize(&expected).unwrap(),
            "script should have run inside the managed scratch workspace"
        );

        let _ = std::fs::remove_dir_all(&storage_root);
    }

    /// Regression guard: when `root_directory` *is* supplied, behavior is
    /// unchanged — the step runs in the given directory, and no scratch
    /// workspace is created.
    #[tokio::test]
    async fn set_root_directory_input_is_used_unchanged() {
        let provided_root = unique_temp_dir("inxm_test_root");
        std::fs::create_dir_all(&provided_root).unwrap();
        let storage_root = unique_temp_dir("inxm_test_storage");

        let cfg = bash_config(r#"echo "{\"cwd\": \"$(pwd)\"}""#);
        let mut plan_config = IndexMap::new();
        plan_config.insert(
            "input.root_directory".to_owned(),
            serde_json::Value::String(provided_root.to_string_lossy().into_owned()),
        );
        let ctx = make_ctx(cfg, "run-set", plan_config, storage_root.clone());

        let result = run(&ctx).await.expect("script should run");

        let scratch = storage_root.join("runs").join("run-set").join("workspace");
        assert!(
            !scratch.exists(),
            "scratch workspace must not be created when root_directory is set"
        );

        let cwd = result.outputs["cwd"].as_str().expect("cwd output present");
        assert_eq!(
            std::fs::canonicalize(cwd).unwrap(),
            std::fs::canonicalize(&provided_root).unwrap(),
            "script should have run in the supplied root_directory, unchanged"
        );

        let _ = std::fs::remove_dir_all(&provided_root);
        let _ = std::fs::remove_dir_all(&storage_root);
    }

    /// The compiler emits `working_dir: "${input.root_directory}"` on every
    /// CODE_CALL step. With the input unset (`Value::Null`), the placeholder
    /// used to resolve to the literal string `"null"`, and spawning with that
    /// missing cwd failed with an ENOENT blamed on the interpreter. It must
    /// fall back to the scratch workspace, same as `working_dir: None`.
    #[tokio::test]
    async fn working_dir_placeholder_resolving_to_null_uses_scratch_workspace() {
        let storage_root = unique_temp_dir("inxm_test_storage");
        let mut cfg = bash_config(r#"echo "{\"cwd\": \"$(pwd)\"}""#);
        cfg.working_dir = Some("${input.root_directory}".to_owned());
        let mut plan_config = IndexMap::new();
        plan_config.insert("input.root_directory".to_owned(), serde_json::Value::Null);
        let ctx = make_ctx(cfg, "run-null-wd", plan_config, storage_root.clone());

        let result = run(&ctx).await.expect("script should run");

        let expected = storage_root
            .join("runs")
            .join("run-null-wd")
            .join("workspace");
        let cwd = result.outputs["cwd"].as_str().expect("cwd output present");
        assert_eq!(
            std::fs::canonicalize(cwd).unwrap(),
            std::fs::canonicalize(&expected).unwrap(),
            "null working_dir placeholder must fall back to the scratch workspace"
        );

        let _ = std::fs::remove_dir_all(&storage_root);
    }

    /// A working directory that does not exist must fail with an error naming
    /// the directory — not the misleading OS-level "failed to spawn
    /// interpreter ... No such file or directory".
    #[tokio::test]
    async fn missing_working_dir_fails_with_clear_error() {
        let storage_root = unique_temp_dir("inxm_test_storage");
        let mut cfg = bash_config("echo hi");
        cfg.working_dir = Some("/definitely/not/a/real/dir".to_owned());
        let ctx = make_ctx(cfg, "run-bad-wd", IndexMap::new(), storage_root.clone());

        let error = match run(&ctx).await {
            Err(error) => error,
            Ok(_) => panic!("missing working dir must fail"),
        };

        let message = error.to_string();
        assert!(
            message.contains("working directory '/definitely/not/a/real/dir' does not exist"),
            "error must name the missing working directory, got: {message}"
        );
        assert!(
            !message.contains("spawn interpreter"),
            "error must not blame the interpreter, got: {message}"
        );
        let _ = std::fs::remove_dir_all(&storage_root);
    }

    #[tokio::test]
    async fn executable_source_rejects_placeholder_injection_without_running_it() {
        let storage_root = unique_temp_dir("inxm_test_storage");
        let marker = unique_temp_dir("inxm_injection_marker");
        let payload = format!("\"; touch '{}'; #", marker.display());
        let mut plan_config = IndexMap::new();
        plan_config.insert(
            "input.payload".to_owned(),
            serde_json::Value::String(payload),
        );
        let cfg = bash_config("printf '%s' \"${input.payload}\"");
        let ctx = make_ctx(cfg, "run-injection", plan_config, storage_root.clone());

        let error = match run(&ctx).await {
            Err(error) => error,
            Ok(_) => panic!("dynamic executable source must be rejected"),
        };

        assert!(error.to_string().contains("config.inline must be static"));
        assert!(
            !marker.exists(),
            "the injected command must never be executed"
        );
        let _ = std::fs::remove_dir_all(&storage_root);
    }

    #[tokio::test]
    async fn runtime_values_remain_supported_through_args_and_stdin() {
        let storage_root = unique_temp_dir("inxm_test_storage");
        let payload = "safe runtime payload";
        let mut plan_config = IndexMap::new();
        plan_config.insert(
            "input.payload".to_owned(),
            serde_json::Value::String(payload.to_owned()),
        );
        let mut cfg = bash_config("read -r line; printf '%s|%s' \"$1\" \"$line\"");
        cfg.args = vec!["${input.payload}".to_owned()];
        cfg.stdin = Some("${input.payload}".to_owned());
        let ctx = make_ctx(
            cfg,
            "run-structured-input",
            plan_config,
            storage_root.clone(),
        );

        let result = run(&ctx)
            .await
            .expect("args and stdin placeholders should resolve");

        assert_eq!(
            result.stdout.as_deref(),
            Some("safe runtime payload|safe runtime payload")
        );
        let _ = std::fs::remove_dir_all(&storage_root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_and_reaps_descendants_before_they_write() {
        let storage_root = unique_temp_dir("inxm_test_storage");
        let marker = unique_temp_dir("inxm_timeout_marker");
        let pid_file = unique_temp_dir("inxm_timeout_pid");
        let script = format!(
            "(sleep 2; printf late > '{}') & child=$!; printf '%s' \"$child\" > '{}'; wait",
            marker.display(),
            pid_file.display()
        );
        let mut cfg = bash_config(&script);
        cfg.timeout_secs = Some(30);
        let mut ctx = make_ctx(cfg, "run-timeout", IndexMap::new(), storage_root.clone());
        ctx.global_timeout_secs = Some(1);

        let error = match run(&ctx).await {
            Err(error) => error,
            Ok(_) => panic!("script should time out"),
        };
        assert!(error.to_string().contains("timed out after 1s"));

        let child_pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("child PID should be recorded before timeout")
            .parse()
            .expect("recorded child PID should be numeric");
        tokio::time::sleep(Duration::from_millis(1_250)).await;
        assert!(
            !marker.exists(),
            "a timed-out descendant must not perform delayed side effects"
        );
        // SAFETY: helper only inspects the recorded PID's liveness/state.
        assert!(
            !unsafe { unix_process::process_is_running(child_pid) },
            "the descendant process should be gone after timeout cleanup"
        );

        let _ = std::fs::remove_file(&pid_file);
        let _ = std::fs::remove_dir_all(&storage_root);
    }

    /// The script leader can exit while a background child still owns stdout.
    /// Normal completion must close containment before waiting for that pipe,
    /// otherwise this runner would hang until the child exits naturally.
    #[cfg(unix)]
    #[tokio::test]
    async fn completed_leader_does_not_wait_for_background_child_pipe() {
        let storage_root = unique_temp_dir("inxm_test_storage");
        let pid_file = unique_temp_dir("inxm_background_pipe_pid");
        let script = format!(
            "sleep 30 & child=$!; disown; printf '%s' \"$child\" > '{}'; printf done; sleep 0.1; exit 0",
            pid_file.display()
        );
        let ctx = make_ctx(
            bash_config(&script),
            "run-background-pipe",
            IndexMap::new(),
            storage_root.clone(),
        );

        let result = tokio::time::timeout(Duration::from_secs(5), run(&ctx))
            .await
            .expect("background pipe must not hold CODE_CALL completion")
            .expect("leader success should remain successful");
        assert_eq!(result.stdout.as_deref(), Some("done"));

        let child_pid: i32 = std::fs::read_to_string(&pid_file)
            .expect("background child PID should be recorded")
            .parse()
            .expect("recorded child PID should be numeric");
        tokio::time::sleep(Duration::from_millis(100)).await;
        // SAFETY: helper only inspects the recorded PID's liveness/state.
        assert!(
            !unsafe { unix_process::process_is_running(child_pid) },
            "normal completion must clean up the background child too"
        );

        let _ = std::fs::remove_file(&pid_file);
        let _ = std::fs::remove_dir_all(&storage_root);
    }
}

#[cfg(test)]
mod output_mapping_tests {
    use super::*;
    use crate::plan::types::{
        CodeCallConfig, Plan, PlanMetadata, PlanOutput, PlanStep, StepConfig,
    };
    use crate::tools::catalog::ToolCatalog;
    use indexmap::IndexMap;

    const OUTPUT_LIMIT_TEST_DEADLINE_SECS: u64 = 5;
    const LONG_RUNNING_SCRIPT_SECS: u64 = 300;

    fn make_ctx(cfg: CodeCallConfig, declared_outputs: &[&str], run_id: &str) -> StepContext {
        let step = PlanStep {
            id: "script".to_owned(),
            name: "script".to_owned(),
            description: None,
            config: StepConfig::CodeCall(cfg),
            depends_on: vec![],
            outputs: declared_outputs
                .iter()
                .map(|name| PlanOutput {
                    name: (*name).to_owned(),
                    description: None,
                    value_type: "string".to_owned(),
                })
                .collect(),
            timeout_secs: None,
            retry: None,
        };
        StepContext {
            plan: std::sync::Arc::new(Plan {
                metadata: PlanMetadata::new(None),
                name: "test".to_owned(),
                description: None,
                inputs: vec![],
                config: IndexMap::new(),
                steps: vec![step.clone()],
                outputs: vec![],
            }),
            step,
            step_outputs: IndexMap::new().into(),
            catalog: ToolCatalog::default(),
            global_timeout_secs: None,
            human: None,
            run_id: run_id.to_owned(),
            progress: None,
            child_progress: None,
            llm_keys: Default::default(),
            storage_root: std::env::temp_dir().join(format!(
                "inxm_test_storage_{}",
                uuid::Uuid::new_v4().as_simple()
            )),
            agent_audit: Default::default(),
        }
    }

    fn config(language: &str, inline: &str) -> CodeCallConfig {
        CodeCallConfig {
            language: language.to_owned(),
            inline: Some(inline.to_owned()),
            file: None,
            args: vec![],
            stdin: None,
            env: IndexMap::new(),
            working_dir: None,
            timeout_secs: None,
        }
    }

    /// `hostenv::tests::find_on_path_falls_back_to_well_known_dir` blanks the
    /// process-wide `PATH` for a moment, and tests run in parallel — an
    /// interpreter lookup that races into that window fails spuriously.
    /// Retry only that specific error; everything else propagates unchanged.
    async fn run_tolerating_path_races(
        ctx: &StepContext,
    ) -> Result<StepResult, crate::error::ExecutorError> {
        let mut last_error = None;
        for _ in 0..20 {
            match run(ctx).await {
                Err(error) if error.to_string().contains("no interpreter for") => {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                other => return other,
            }
        }
        Err(last_error.expect("loop only exits early or with an error recorded"))
    }

    // ── sanitize_lone_surrogate_escapes ──────────────────────────────────────

    #[test]
    fn sanitize_returns_none_when_nothing_to_fix() {
        assert_eq!(sanitize_lone_surrogate_escapes(r#"{"a": "plain"}"#), None);
        // A valid surrogate pair (emoji) is not "lone" and must be preserved.
        assert_eq!(sanitize_lone_surrogate_escapes(r#"{"a": "🎤"}"#), None);
        // Escaped backslash followed by 'u' is literal text, not an escape.
        assert_eq!(sanitize_lone_surrogate_escapes(r#"{"a": "\\uDC9D"}"#), None);
    }

    #[test]
    fn sanitize_replaces_lone_surrogates_and_keeps_pairs() {
        // The exact shape produced by run 71d3008b: cp1252-decoded UTF-8 with
        // a surrogateescape'd 0x9D byte, i.e. `â€` + lone `\udc9d`.
        let broken = r#"{"a": "â€\udc9d", "b": "🎤", "c": "\ud800"}"#;
        let fixed = sanitize_lone_surrogate_escapes(broken).expect("must sanitize");
        let value: serde_json::Value =
            serde_json::from_str(&fixed).expect("sanitized JSON must parse");
        assert_eq!(value["a"], format!("â€{}", '\u{FFFD}'));
        assert_eq!(value["b"], "🎤");
        assert_eq!(value["c"], "\u{FFFD}");
    }

    // ── run(): output mapping behavior ───────────────────────────────────────

    /// Regression test for plan 423d8933: Python `json.dumps` emitted a lone
    /// `\udc9d` escape, serde_json rejected the whole document, and the step
    /// "succeeded" with empty outputs. The outputs must now be recovered, with
    /// a warning on stderr.
    #[tokio::test]
    async fn lone_surrogate_json_stdout_still_yields_outputs() {
        let script =
            r#"printf '%s' '{"branch_name": "sync-1", "staged_diff": "quote â€\udc9d end"}'"#;
        let ctx = make_ctx(
            config("bash", script),
            &["branch_name", "staged_diff"],
            "run-surrogate",
        );

        let result = run_tolerating_path_races(&ctx)
            .await
            .expect("outputs should be recovered");

        assert_eq!(result.outputs["branch_name"], "sync-1");
        assert_eq!(
            result.outputs["staged_diff"],
            format!("quote â€{} end", '\u{FFFD}')
        );
        let stderr = result.stderr.expect("recovery warning expected on stderr");
        assert!(
            stderr.contains("lone UTF-16 surrogate escapes"),
            "stderr must explain the recovery, got: {stderr}"
        );
    }

    /// With multiple declared outputs and undecipherable stdout, the step must
    /// fail with the real parse diagnosis instead of succeeding with empty
    /// outputs (which downstream reports as the misleading "produced no named
    /// outputs" and sends repair rewriting the wrong step).
    #[tokio::test]
    async fn non_json_stdout_with_multiple_declared_outputs_fails_with_diagnosis() {
        let ctx = make_ctx(
            config("bash", "echo BRANCH_NAME=sync-1"),
            &["branch_name", "staged_diff"],
            "run-not-json",
        );

        let error = match run_tolerating_path_races(&ctx).await {
            Err(error) => error,
            Ok(result) => panic!("expected failure, got outputs {:?}", result.outputs),
        };

        let message = error.to_string();
        assert!(
            message.contains("cannot satisfy the declared outputs (branch_name, staged_diff)"),
            "message must name the declared outputs, got: {message}"
        );
        assert!(
            message.contains("must print exactly one JSON object"),
            "message must state the output contract, got: {message}"
        );
    }

    /// The single-output plain-text fallback is unchanged: stdout fills the
    /// one declared output even when it is not JSON.
    #[tokio::test]
    async fn single_declared_output_still_receives_plain_text_stdout() {
        let ctx = make_ctx(
            config("bash", "echo BRANCH_NAME=sync-1"),
            &["raw"],
            "run-plain-single",
        );

        let result = run_tolerating_path_races(&ctx)
            .await
            .expect("plain-text fallback should apply");

        assert_eq!(result.outputs["raw"], "BRANCH_NAME=sync-1");
    }

    /// Python steps default to UTF-8 mode so Windows pipes stop producing
    /// mojibake and lone surrogates; an explicit step env still wins.
    #[tokio::test]
    async fn python_gets_utf8_env_defaults_and_step_env_overrides() {
        let script = "import os, json\nprint(json.dumps({'utf8': os.environ.get('PYTHONUTF8', ''), 'io': os.environ.get('PYTHONIOENCODING', '')}))";
        let ctx = make_ctx(config("python", script), &["utf8", "io"], "run-py-env");
        let result = run_tolerating_path_races(&ctx)
            .await
            .expect("python script should run");
        assert_eq!(result.outputs["utf8"], "1");
        assert_eq!(result.outputs["io"], "utf-8");

        let mut cfg = config("python", script);
        cfg.env
            .insert("PYTHONIOENCODING".to_owned(), "latin-1".to_owned());
        let ctx = make_ctx(cfg, &["utf8", "io"], "run-py-env-override");
        let result = run_tolerating_path_races(&ctx)
            .await
            .expect("python script should run");
        assert_eq!(
            result.outputs["io"], "latin-1",
            "explicit step env must override the UTF-8 default"
        );
    }

    /// A CODE_CALL runs model-written code, so its output must be bounded like
    /// every tool adapter's — otherwise a runaway script buffers into the app
    /// process without limit.
    #[tokio::test]
    async fn oversized_stdout_fails_with_the_output_limit_marker() {
        // Emit comfortably past the 1 MiB cap without writing a huge script.
        let script = format!(
            "head -c {} /dev/zero | tr '\\0' 'x'",
            MAX_SCRIPT_STDOUT_BYTES + 4096
        );
        let ctx = make_ctx(config("bash", &script), &["out"], "run-oversized-stdout");

        let message = match run_tolerating_path_races(&ctx).await {
            Err(error) => error.to_string(),
            Ok(_) => panic!("output past the cap must fail the step"),
        };
        assert!(
            message.contains(OUTPUT_LIMIT_MARKER),
            "should carry the shared limit marker: {message}"
        );
        assert!(
            message.contains("stdout") && message.contains(&MAX_SCRIPT_STDOUT_BYTES.to_string()),
            "should name the stream and its limit: {message}"
        );
    }

    /// Reaching the cap must win the race against a child that keeps running;
    /// otherwise a CODE_CALL without a configured timeout can hang forever.
    #[tokio::test]
    async fn oversized_output_terminates_a_child_that_keeps_running() {
        let script = format!(
            "head -c {} /dev/zero; sleep {LONG_RUNNING_SCRIPT_SECS}",
            MAX_SCRIPT_STDOUT_BYTES + 1
        );
        let ctx = make_ctx(config("bash", &script), &["out"], "run-output-cap-hang");

        let result = tokio::time::timeout(
            Duration::from_secs(OUTPUT_LIMIT_TEST_DEADLINE_SECS),
            run_tolerating_path_races(&ctx),
        )
        .await
        .expect("output overflow must terminate the long-running child");
        let message = match result {
            Err(error) => error.to_string(),
            Ok(_) => panic!("output past the cap must fail"),
        };

        assert!(
            message.contains(OUTPUT_LIMIT_MARKER),
            "should report the output limit: {message}"
        );
    }

    /// The cap must not disturb ordinary output — including output close to,
    /// but under, the limit.
    #[tokio::test]
    async fn output_just_under_the_cap_still_succeeds() {
        let size = MAX_SCRIPT_STDOUT_BYTES - 1024;
        let script = format!("head -c {size} /dev/zero | tr '\\0' 'y'");
        let ctx = make_ctx(config("bash", &script), &["out"], "run-under-cap");

        let result = run_tolerating_path_races(&ctx)
            .await
            .expect("output under the cap must succeed");

        let out = result.outputs["out"].as_str().expect("string output");
        assert_eq!(out.trim_end().len(), size, "payload must arrive intact");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_job_contains_child_and_kills_descendants_when_closed() {
        let marker = std::env::temp_dir().join(format!(
            "inxm_job_descendant_marker_{}",
            uuid::Uuid::new_v4().as_simple()
        ));
        let ready = std::env::temp_dir().join(format!(
            "inxm_job_descendant_ready_{}",
            uuid::Uuid::new_v4().as_simple()
        ));
        let child_script = std::env::temp_dir().join(format!(
            "inxm_job_descendant_child_{}.cmd",
            uuid::Uuid::new_v4().as_simple()
        ));
        let parent_script = std::env::temp_dir().join(format!(
            "inxm_job_descendant_parent_{}.cmd",
            uuid::Uuid::new_v4().as_simple()
        ));
        std::fs::write(
            &child_script,
            "@echo off\r\ntype nul > \"%INXM_READY%\"\r\nping -n 3 127.0.0.1 >NUL\r\ntype nul > \"%INXM_MARKER%\"\r\n",
        )
        .expect("descendant batch script should be written");
        std::fs::write(
            &parent_script,
            "@echo off\r\nstart \"\" /B cmd /C call \"%INXM_CHILD_SCRIPT%\"\r\nping -n 30 127.0.0.1 >NUL\r\n",
        )
        .expect("parent batch script should be written");
        let mut command = tokio::process::Command::new("cmd");
        command
            .args(["/C"])
            .arg(&parent_script)
            .env("INXM_MARKER", &marker)
            .env("INXM_READY", &ready)
            .env("INXM_CHILD_SCRIPT", &child_script)
            .creation_flags(windows_process::CREATE_SUSPENDED)
            .kill_on_drop(true);
        let mut child = command.spawn().expect("cmd should start");
        let mut containment = ProcessContainment::for_child(&child)
            .expect("new child must be assigned to a kill-on-close Job Object");

        assert!(
            containment
                .contains_child(&child)
                .expect("Job containment query should succeed"),
            "the spawned child must be in the owned Job Object"
        );
        containment
            .resume_child(&child)
            .expect("contained parent must resume after Job assignment");
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("descendant must signal readiness before Job close");
        containment.finish_after_output();
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("closing the Job Object should promptly end its child")
            .expect("child wait should succeed");
        assert!(
            !status.success(),
            "closing a kill-on-close Job Object must terminate its child"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "a descendant of the Job must not perform its delayed write after close"
        );

        let _ = std::fs::remove_file(&parent_script);
        let _ = std::fs::remove_file(&child_script);
        let _ = std::fs::remove_file(&ready);
        let _ = std::fs::remove_file(&marker);
    }
}
