//! Subprocess adapter — runs an external binary as a tool.
//!
//! Arguments are injected as environment variables so the child process can
//! consume them in whatever language/framework it chooses. An input named
//! `args` is also appended to the child command line when its value is an array:
//! `capture_status: true` instead returns the completed command status as data,
//! including for non-zero exits.
//!
//! * `INXM_ARG_<KEY>` — individual argument values (strings are passed as-is,
//!   other JSON types are serialised with `to_string`).
//! * `INXM_ARGS`      — the full argument map as a compact JSON object.

use crate::error::ToolError;
use crate::tools::ToolOutput;
use crate::tools::adapters::process::{ProcessGroupGuard, isolate_process_group, kill_and_reap};
use crate::tools::catalog::SubprocessConfig;
use indexmap::IndexMap;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::{ChildStderr, ChildStdout, Command};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout_at};

/// Prefix for per-argument env vars: `INXM_ARG_<KEY>=<value>`.
const ARG_ENV_PREFIX: &str = "INXM_ARG_";
/// Env var carrying the full argument map as a compact JSON object.
const ARGS_JSON_ENV_VAR: &str = "INXM_ARGS";
/// Exit code reported when the child was terminated by a signal and has no
/// real exit code.
const EXIT_CODE_SIGNAL_TERMINATED: i32 = -1;
/// Maximum stdout retained from a subprocess tool.
const MAX_SUBPROCESS_STDOUT_BYTES: usize = 1_048_576;
/// Maximum stderr retained from a subprocess tool.
const MAX_SUBPROCESS_STDERR_BYTES: usize = 262_144;
const OUTPUT_LIMIT_MARKER: &str = "[truncated: subprocess output limit exceeded]";

// ─── Public entry point ───────────────────────────────────────────────────────

pub async fn run(
    config: &SubprocessConfig,
    arguments: &IndexMap<String, serde_json::Value>,
    timeout_secs: Option<u64>,
) -> Result<ToolOutput, ToolError> {
    let raw = execute(config, arguments, timeout_secs).await?;

    let stdout = String::from_utf8_lossy(&raw.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&raw.stderr).into_owned();
    let exit_code = raw.status.code().unwrap_or(EXIT_CODE_SIGNAL_TERMINATED);

    let capture_status = matches!(
        arguments.get("capture_status"),
        Some(serde_json::Value::Bool(true))
    );
    interpret_output(&config.command, stdout, stderr, exit_code, capture_status)
}

// ─── Internals ────────────────────────────────────────────────────────────────

async fn execute(
    config: &SubprocessConfig,
    arguments: &IndexMap<String, serde_json::Value>,
    timeout_secs: Option<u64>,
) -> Result<std::process::Output, ToolError> {
    // PATH/PATHEXT-aware resolution: finds `.cmd`/`.exe` shims on Windows.
    let mut cmd = Command::new(crate::hostenv::resolve_program(&config.command));

    // Fixed prefix args from the tool definition.
    cmd.args(&config.args);

    // `args` is the subprocess convention for dynamic positional arguments.
    // Keep exporting it below as an environment variable too, for backwards
    // compatibility with tools that consumed it that way.
    if let Some(dynamic_args) = arguments.get("args").and_then(serde_json::Value::as_array) {
        cmd.args(dynamic_args.iter().map(json_value_to_str));
    }

    // Static env vars from the tool definition.
    for (k, v) in &config.env {
        cmd.env(k, v);
    }

    // Per-argument env vars: INXM_ARG_<KEY>=<value>.
    for (key, value) in arguments {
        cmd.env(arg_env_key(key), json_value_to_str(value));
    }

    // Full argument map as JSON for callers that prefer structured input.
    let args_json = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_owned());
    cmd.env(ARGS_JSON_ENV_VAR, &args_json);

    if let Some(dir) = &config.working_dir {
        cmd.current_dir(dir);
    }

    // No interactive input is ever expected from a tool subprocess. Without
    // this, the child inherits this process's own stdin, so any program in
    // the chain that blocks on an interactive prompt (an SSH host-key/
    // passphrase prompt is the classic case — it is not covered by Git's or
    // gh's own "disable prompts" env vars) hangs until the step times out,
    // instead of seeing EOF and failing immediately.
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    isolate_process_group(&mut cmd);

    let mut child = cmd.spawn().map_err(|e| ToolError::Execution {
        tool: config.command.clone(),
        message: format!(
            "failed to run '{}': {e} — is it installed and on PATH?",
            config.command
        ),
    })?;
    let mut process_group = ProcessGroupGuard::for_child(&child);
    let stdout = child.stdout.take().ok_or_else(|| ToolError::Execution {
        tool: config.command.clone(),
        message: "failed to open child stdout".to_owned(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ToolError::Execution {
        tool: config.command.clone(),
        message: "failed to open child stderr".to_owned(),
    })?;
    let mut stdout_task = read_stdout(stdout);
    let mut stderr_task = read_stderr(stderr);
    let deadline = timeout_secs.map(|secs| Instant::now() + Duration::from_secs(secs));

    let status = if let Some(deadline) = deadline {
        match timeout_at(deadline, child.wait()).await {
            Ok(result) => result.map_err(ToolError::Io)?,
            Err(_) => {
                let secs = timeout_secs.expect("deadline exists only when timeout_secs is set");
                kill_and_reap(&mut child, &mut process_group)
                    .await
                    .map_err(|error| ToolError::Execution {
                        tool: config.command.clone(),
                        message: format!(
                            "timed out after {secs}s and failed to terminate child: {error}"
                        ),
                    })?;
                // Killing the child closes its stdout/stderr pipes, so
                // draining them now (instead of discarding the result) picks
                // up whatever it had already printed — e.g. a blocked
                // interactive prompt's own message — without waiting any
                // longer than it takes those pipes to hit EOF.
                let (stdout, stderr) =
                    collect_reader_tasks(&mut stdout_task, &mut stderr_task, &config.command)
                        .await
                        .unwrap_or_default();
                return Err(ToolError::timeout_with_output(
                    config.command.clone(),
                    secs,
                    &stdout.text(),
                    &stderr.text(),
                ));
            }
        }
    } else {
        child.wait().await.map_err(ToolError::Io)?
    };

    let streams = if let Some(deadline) = deadline {
        match timeout_at(
            deadline,
            collect_reader_tasks(&mut stdout_task, &mut stderr_task, &config.command),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                let secs = timeout_secs.expect("deadline exists only when timeout_secs is set");
                kill_and_reap(&mut child, &mut process_group)
                    .await
                    .map_err(|error| ToolError::Execution {
                        tool: config.command.clone(),
                        message: format!(
                            "timed out after {secs}s and failed to terminate descendants: {error}"
                        ),
                    })?;
                let (stdout, stderr) =
                    collect_reader_tasks(&mut stdout_task, &mut stderr_task, &config.command)
                        .await
                        .unwrap_or_default();
                return Err(ToolError::timeout_with_output(
                    config.command.clone(),
                    secs,
                    &stdout.text(),
                    &stderr.text(),
                ));
            }
        }
    } else {
        collect_reader_tasks(&mut stdout_task, &mut stderr_task, &config.command).await?
    };
    let (stdout, stderr) = streams;
    if stdout.overflow || stderr.overflow {
        tracing::Span::current().record("tool.output_limit_violation", true);
        kill_and_reap(&mut child, &mut process_group)
            .await
            .map_err(|error| ToolError::Execution {
                tool: config.command.clone(),
                message: format!("output limit exceeded and cleanup failed: {error}"),
            })?;
        let stream = if stdout.overflow { "stdout" } else { "stderr" };
        let limit = if stdout.overflow {
            MAX_SUBPROCESS_STDOUT_BYTES
        } else {
            MAX_SUBPROCESS_STDERR_BYTES
        };
        return Err(ToolError::Execution {
            tool: config.command.clone(),
            message: format!("{OUTPUT_LIMIT_MARKER}; {stream} maximum is {limit} bytes"),
        });
    }
    process_group.disarm();
    Ok(std::process::Output {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
    })
}

#[derive(Default)]
struct BoundedBytes {
    bytes: Vec<u8>,
    overflow: bool,
}

impl BoundedBytes {
    fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }
}

fn read_stdout(stdout: ChildStdout) -> JoinHandle<std::io::Result<BoundedBytes>> {
    tokio::spawn(async move {
        let mut stdout = stdout.take((MAX_SUBPROCESS_STDOUT_BYTES + 1) as u64);
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await?;
        let overflow = bytes.len() > MAX_SUBPROCESS_STDOUT_BYTES;
        bytes.truncate(MAX_SUBPROCESS_STDOUT_BYTES);
        Ok(BoundedBytes { bytes, overflow })
    })
}

fn read_stderr(stderr: ChildStderr) -> JoinHandle<std::io::Result<BoundedBytes>> {
    tokio::spawn(async move {
        let mut stderr = stderr.take((MAX_SUBPROCESS_STDERR_BYTES + 1) as u64);
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await?;
        let overflow = bytes.len() > MAX_SUBPROCESS_STDERR_BYTES;
        bytes.truncate(MAX_SUBPROCESS_STDERR_BYTES);
        Ok(BoundedBytes { bytes, overflow })
    })
}

async fn collect_reader_tasks(
    stdout_task: &mut JoinHandle<std::io::Result<BoundedBytes>>,
    stderr_task: &mut JoinHandle<std::io::Result<BoundedBytes>>,
    command: &str,
) -> Result<(BoundedBytes, BoundedBytes), ToolError> {
    let stdout = collect_reader_task(stdout_task, command, "stdout").await?;
    let stderr = collect_reader_task(stderr_task, command, "stderr").await?;
    Ok((stdout, stderr))
}

async fn collect_reader_task(
    task: &mut JoinHandle<std::io::Result<BoundedBytes>>,
    command: &str,
    stream: &str,
) -> Result<BoundedBytes, ToolError> {
    task.await
        .map_err(|error| ToolError::Execution {
            tool: command.to_owned(),
            message: format!("failed to join child {stream} reader: {error}"),
        })?
        .map_err(ToolError::Io)
}

/// Map a finished child's decoded streams and exit code to the adapter
/// result. Non-zero exit is an [`ToolError::Execution`] unless status capture
/// is enabled; successful output otherwise parses stdout as JSON when possible.
fn interpret_output(
    command: &str,
    stdout: String,
    stderr: String,
    exit_code: i32,
    capture_status: bool,
) -> Result<ToolOutput, ToolError> {
    if !capture_status && exit_code != 0 {
        return Err(ToolError::Execution {
            tool: command.to_owned(),
            message: format!("exited with code {exit_code}: {stderr}"),
        });
    }

    let data = if capture_status {
        serde_json::json!({
            "success": exit_code == 0,
            "exit_code": exit_code,
            "stdout": stdout,
            "stderr": stderr,
        })
    } else {
        serde_json::from_str(&stdout).unwrap_or(serde_json::Value::Null)
    };

    Ok(ToolOutput {
        stdout,
        stderr,
        exit_code,
        data,
    })
}

/// Convert a JSON value to a plain string for use in an environment variable.
fn json_value_to_str(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Map an argument key to its env-var name: `"my_key"` → `"INXM_ARG_MY_KEY"`.
fn arg_env_key(key: &str) -> String {
    format!("{ARG_ENV_PREFIX}{}", key.to_uppercase())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    const DELAYED_SIDE_EFFECT_SCRIPT: &str =
        "echo $$ > \"$PID_PATH\"; (sleep 2; printf late > \"$MARKER_PATH\") & wait";

    #[test]
    fn arg_env_key_uppercases_key() {
        assert_eq!(arg_env_key("message"), "INXM_ARG_MESSAGE");
        assert_eq!(arg_env_key("my_key"), "INXM_ARG_MY_KEY");
        assert_eq!(arg_env_key("CamelCase"), "INXM_ARG_CAMELCASE");
    }

    #[test]
    fn json_value_to_str_string_passthrough() {
        let v = serde_json::json!("hello world");
        assert_eq!(json_value_to_str(&v), "hello world");
    }

    #[test]
    fn json_value_to_str_number_serialised() {
        let v = serde_json::json!(42);
        assert_eq!(json_value_to_str(&v), "42");
    }

    #[test]
    fn json_value_to_str_bool_serialised() {
        assert_eq!(json_value_to_str(&serde_json::json!(true)), "true");
        assert_eq!(json_value_to_str(&serde_json::json!(false)), "false");
    }

    #[test]
    fn json_value_to_str_null_serialised() {
        assert_eq!(json_value_to_str(&serde_json::Value::Null), "null");
    }

    // ── interpret_output ─────────────────────────────────────────────────────
    //
    // The spawn-and-capture I/O shell itself is covered end-to-end by
    // `tests/integration_executor.rs`; unit tests stick to the pure mapping.

    #[test]
    fn success_with_plain_stdout_has_null_data() {
        let output =
            interpret_output("mytool", "hello\n".to_owned(), String::new(), 0, false).unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, "hello\n");
        assert_eq!(output.data, serde_json::Value::Null);
    }

    #[test]
    fn success_with_json_stdout_parses_data() {
        let output = interpret_output(
            "mytool",
            r#"{"ok":true}"#.to_owned(),
            String::new(),
            0,
            false,
        )
        .unwrap();
        assert_eq!(output.data, serde_json::json!({"ok": true}));
    }

    #[test]
    fn nonzero_exit_is_an_execution_error_carrying_stderr() {
        let err =
            interpret_output("mytool", String::new(), "boom".to_owned(), 3, false).unwrap_err();
        match err {
            ToolError::Execution { tool, message } => {
                assert_eq!(tool, "mytool");
                assert!(message.contains("exited with code 3"));
                assert!(message.contains("boom"));
            }
            other => panic!("expected Execution error, got: {other}"),
        }
    }

    #[test]
    fn captured_status_returns_data_for_zero_and_nonzero_exits() {
        for (exit_code, expected_success) in [(0, true), (3, false)] {
            let output = interpret_output(
                "mytool",
                "standard output".to_owned(),
                "standard error".to_owned(),
                exit_code,
                true,
            )
            .unwrap();
            assert_eq!(output.exit_code, exit_code);
            assert_eq!(
                output.data,
                serde_json::json!({
                    "success": expected_success,
                    "exit_code": exit_code,
                    "stdout": "standard output",
                    "stderr": "standard error",
                })
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_child_and_descendants_before_they_can_act() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("late-side-effect");
        let pid_file = temp.path().join("child.pid");
        let config = SubprocessConfig {
            command: "sh".to_owned(),
            args: vec!["-c".to_owned(), DELAYED_SIDE_EFFECT_SCRIPT.to_owned()],
            env: [
                (
                    "MARKER_PATH".to_owned(),
                    marker.to_string_lossy().into_owned(),
                ),
                (
                    "PID_PATH".to_owned(),
                    pid_file.to_string_lossy().into_owned(),
                ),
            ]
            .into_iter()
            .collect(),
            working_dir: None,
        };

        let error = run(&config, &IndexMap::new(), Some(1)).await.unwrap_err();
        assert!(matches!(error, ToolError::Timeout { secs: 1, .. }));

        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_owned();
        tokio::time::sleep(Duration::from_millis(1_500)).await;

        assert!(
            !marker.exists(),
            "a descendant performed its delayed side effect after timeout"
        );
        assert!(
            !process_is_alive(&pid),
            "timed-out child process {pid} is still alive"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn named_dynamic_arguments_remain_env_only_regardless_of_map_order() {
        let config = SubprocessConfig {
            command: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "printf '%s\\n%s' \"$#\" \"$INXM_ARGS\"".to_owned(),
            ],
            env: IndexMap::new(),
            working_dir: None,
        };
        let first: IndexMap<String, serde_json::Value> = [
            ("alpha".to_owned(), serde_json::json!(1)),
            ("beta".to_owned(), serde_json::json!(2)),
        ]
        .into_iter()
        .collect();
        let second: IndexMap<String, serde_json::Value> = [
            ("beta".to_owned(), serde_json::json!(2)),
            ("alpha".to_owned(), serde_json::json!(1)),
        ]
        .into_iter()
        .collect();

        for arguments in [&first, &second] {
            let output = run(&config, arguments, Some(5)).await.unwrap();
            let mut lines = output.stdout.lines();
            assert_eq!(
                lines.next(),
                Some("0"),
                "named dynamic values became CLI args"
            );
            let env_json: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
            assert_eq!(env_json, serde_json::json!({"alpha": 1, "beta": 2}));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn args_array_is_forwarded_to_child_argv_and_preserved_in_environment() {
        let config = SubprocessConfig {
            command: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "printf '%s|%s|%s|%s' \"$#\" \"$1\" \"$2\" \"$INXM_ARG_LABEL\"".to_owned(),
                "subprocess".to_owned(),
            ],
            env: IndexMap::new(),
            working_dir: None,
        };
        let arguments: IndexMap<String, serde_json::Value> = [
            ("args".to_owned(), serde_json::json!(["check", "--all"])),
            ("label".to_owned(), serde_json::json!("still-an-env-input")),
        ]
        .into_iter()
        .collect();

        let output = run(&config, &arguments, Some(5)).await.unwrap();
        assert_eq!(output.stdout, "2|check|--all|still-an-env-input");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn args_array_forwards_nonzero_exit_status() {
        let config = SubprocessConfig {
            command: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "test \"$1\" = fail || exit 99; exit 7".to_owned(),
                "subprocess".to_owned(),
            ],
            env: IndexMap::new(),
            working_dir: None,
        };
        let arguments = [("args".to_owned(), serde_json::json!(["fail"]))]
            .into_iter()
            .collect();

        let error = run(&config, &arguments, Some(5)).await.unwrap_err();
        assert!(error.to_string().contains("exited with code 7"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn capture_status_keeps_nonzero_exit_as_data_without_adding_argv() {
        let config = SubprocessConfig {
            command: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "test \"$#\" = 0 || exit 99; printf output; printf error >&2; exit 7".to_owned(),
            ],
            env: IndexMap::new(),
            working_dir: None,
        };
        let arguments = [("capture_status".to_owned(), serde_json::json!(true))]
            .into_iter()
            .collect();

        let output = run(&config, &arguments, Some(5)).await.unwrap();
        assert_eq!(output.exit_code, 7);
        assert_eq!(output.stdout, "output");
        assert_eq!(output.stderr, "error");
        assert_eq!(
            output.data,
            serde_json::json!({
                "success": false,
                "exit_code": 7,
                "stdout": "output",
                "stderr": "error",
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_stdout_and_stderr_are_rejected() {
        for (stream, byte_count) in [
            ("stdout", MAX_SUBPROCESS_STDOUT_BYTES + 1),
            ("stderr", MAX_SUBPROCESS_STDERR_BYTES + 1),
        ] {
            let redirect = if stream == "stderr" { " >&2" } else { "" };
            let config = SubprocessConfig {
                command: "sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    format!("head -c {byte_count} /dev/zero{redirect}"),
                ],
                env: IndexMap::new(),
                working_dir: None,
            };

            let error = run(&config, &IndexMap::new(), Some(5)).await.unwrap_err();
            let message = error.to_string();
            assert!(message.contains(OUTPUT_LIMIT_MARKER));
            assert!(message.contains(stream));
            assert!(message.len() < 256, "overflow error included raw output");
        }
    }

    #[cfg(unix)]
    fn process_is_alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
