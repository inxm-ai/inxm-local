//! HUMAN_INTERACTION step runner.
//!
//! Pauses execution and reads a response from stdin.
//!
//! - If `approval_required` is set, the operator must type `y` / `yes` to
//!   continue.  Any other input returns `ExecutorError::StepFailed`.
//! - Otherwise a free-form response line is read and stored.

use crate::error::ExecutorError;
use crate::plan::types::StepConfig;
use crate::support::Presence;
use indexmap::IndexMap;
use tokio::io::AsyncBufReadExt;

use super::{APPROVED_RESPONSE, StepContext, StepResult, resolve_to_string};

pub async fn run(ctx: &StepContext) -> Result<StepResult, ExecutorError> {
    let cfg = match &ctx.step.config {
        StepConfig::HumanInteraction(c) => c,
        _ => {
            return Err(ExecutorError::StepFailed {
                step_id: ctx.step.id.clone(),
                message: "expected HUMAN_INTERACTION config".to_owned(),
            });
        }
    };

    let resolved_prompt = resolve_to_string(&cfg.prompt, ctx);

    // ── UI channel (when configured) ─────────────────────────────────────────
    if let Some(channel) = &ctx.human {
        let wait_started = std::time::Instant::now();
        tracing::info!(
            name: "inxm.executor.human_wait.started",
            run_id = %ctx.run_id,
            plan_id = %ctx.plan.metadata.id,
            step_id = %ctx.step.id,
            human_approval_required = cfg.approval_required,
            human_channel = "ui",
            "human wait started"
        );
        let (respond, receiver) = tokio::sync::oneshot::channel();
        channel
            .send(crate::executor::HumanRequest {
                step_id: ctx.step.id.clone(),
                prompt: resolved_prompt.clone(),
                approval_required: cfg.approval_required,
                response_field: cfg.response_field.clone(),
                respond,
            })
            .map_err(|_| ExecutorError::StepFailed {
                step_id: ctx.step.id.clone(),
                message: "human interaction channel closed".to_owned(),
            })?;

        let decision = receiver.await.map_err(|_| {
            tracing::info!(
                name: "inxm.executor.human_wait.completed",
                run_id = %ctx.run_id,
                plan_id = %ctx.plan.metadata.id,
                step_id = %ctx.step.id,
                human_wait_duration_ms = wait_started.elapsed().as_millis() as u64,
                human_outcome = "pending",
                "human wait completed"
            );
            ExecutorError::HumanResponsePending {
                step_id: ctx.step.id.clone(),
            }
        })?;

        let response = match decision {
            crate::executor::HumanDecision::Approve => {
                tracing::info!(
                    name: "inxm.executor.human_wait.completed",
                    run_id = %ctx.run_id,
                    plan_id = %ctx.plan.metadata.id,
                    step_id = %ctx.step.id,
                    human_wait_duration_ms = wait_started.elapsed().as_millis() as u64,
                    human_outcome = "approved",
                    "human wait completed"
                );
                APPROVED_RESPONSE.to_owned()
            }
            crate::executor::HumanDecision::Reject => {
                tracing::info!(
                    name: "inxm.executor.human_wait.completed",
                    run_id = %ctx.run_id,
                    plan_id = %ctx.plan.metadata.id,
                    step_id = %ctx.step.id,
                    human_wait_duration_ms = wait_started.elapsed().as_millis() as u64,
                    human_outcome = "rejected",
                    "human wait completed"
                );
                return Err(ExecutorError::RejectedByHuman {
                    step_id: ctx.step.id.clone(),
                });
            }
            crate::executor::HumanDecision::Text(text) => {
                tracing::info!(
                    name: "inxm.executor.human_wait.completed",
                    run_id = %ctx.run_id,
                    plan_id = %ctx.plan.metadata.id,
                    step_id = %ctx.step.id,
                    human_wait_duration_ms = wait_started.elapsed().as_millis() as u64,
                    human_outcome = "answered",
                    "human wait completed"
                );
                text
            }
        };

        let mut outputs = IndexMap::new();
        outputs.insert(
            cfg.response_field.clone(),
            serde_json::Value::String(response),
        );
        return Ok(StepResult {
            outputs,
            stdout: None,
            stderr: None,
            usage: None,
            child_runs: IndexMap::new(),
        });
    }

    // ── Live path: stdin fallback ─────────────────────────────────────────────
    let wait_started = std::time::Instant::now();
    tracing::info!(
        name: "inxm.executor.human_wait.started",
        run_id = %ctx.run_id,
        plan_id = %ctx.plan.metadata.id,
        step_id = %ctx.step.id,
        human_approval_required = cfg.approval_required,
        human_channel = "stdin",
        "human wait started"
    );
    println!("\n[HUMAN INTERACTION] {}", resolved_prompt);

    // One reader for the whole interaction: a fresh BufReader per prompt
    // would discard any bytes buffered beyond the first line, losing input
    // typed ahead of the retry loop.
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());

    let response: String = if cfg.approval_required {
        // Loop until we get a clear y/n.
        loop {
            print!("[HUMAN INTERACTION] Approve? [y/n]: ");
            flush_stdout()?;
            let line = match read_line(&mut stdin).await {
                Presence::Found(line) => line,
                Presence::Absent => return Err(stdin_closed_error(&ctx.step.id)),
                Presence::Broken(err) => return Err(err),
            };
            match line.trim().to_lowercase().as_str() {
                "y" | "yes" => break APPROVED_RESPONSE.to_owned(),
                "n" | "no" => {
                    tracing::info!(
                        name: "inxm.executor.human_wait.completed",
                        run_id = %ctx.run_id,
                        plan_id = %ctx.plan.metadata.id,
                        step_id = %ctx.step.id,
                        human_wait_duration_ms = wait_started.elapsed().as_millis() as u64,
                        human_outcome = "rejected",
                        "human wait completed"
                    );
                    return Err(ExecutorError::RejectedByHuman {
                        step_id: ctx.step.id.clone(),
                    });
                }
                _ => println!("[HUMAN INTERACTION] Please enter 'y' or 'n'."),
            }
        }
    } else {
        print!("[HUMAN INTERACTION] Response: ");
        flush_stdout()?;
        match read_line(&mut stdin).await {
            Presence::Found(line) => line,
            Presence::Absent => return Err(stdin_closed_error(&ctx.step.id)),
            Presence::Broken(err) => return Err(err),
        }
    };
    tracing::info!(
        name: "inxm.executor.human_wait.completed",
        run_id = %ctx.run_id,
        plan_id = %ctx.plan.metadata.id,
        step_id = %ctx.step.id,
        human_wait_duration_ms = wait_started.elapsed().as_millis() as u64,
        human_outcome = if cfg.approval_required { "approved" } else { "answered" },
        "human wait completed"
    );

    let mut outputs = IndexMap::new();
    outputs.insert(
        cfg.response_field.clone(),
        serde_json::Value::String(response),
    );

    Ok(StepResult {
        outputs,
        stdout: None,
        stderr: None,
        usage: None,
        child_runs: IndexMap::new(),
    })
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Read one line, distinguishing a closed stream from an empty one: `Ok(0)`
/// from `read_line` means EOF, not "the user typed nothing" — conflating the
/// two used to spin the approval loop forever against a closed stdin.
async fn read_line<R>(reader: &mut R) -> Presence<String, ExecutorError>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => Presence::Absent,
        Ok(_) => Presence::Found(line.trim_end_matches(['\n', '\r']).to_owned()),
        Err(err) => Presence::Broken(ExecutorError::Io(err)),
    }
}

fn stdin_closed_error(step_id: &str) -> ExecutorError {
    ExecutorError::StepFailed {
        step_id: step_id.to_owned(),
        message: "human input closed before an approval or response was given".to_owned(),
    }
}

fn flush_stdout() -> Result<(), ExecutorError> {
    use std::io::Write;
    std::io::stdout().flush().map_err(ExecutorError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_found(presence: Presence<String, ExecutorError>) -> String {
        match presence {
            Presence::Found(line) => line,
            Presence::Absent => panic!("expected a line, got EOF"),
            Presence::Broken(err) => panic!("expected a line, got error: {err}"),
        }
    }

    /// Regression test for the retry loop: input typed ahead ("maybe\ny\n")
    /// arrives in one buffered read, so the second prompt must see the "y"
    /// instead of losing it with the reader that buffered it.
    #[tokio::test]
    async fn one_reader_keeps_type_ahead_input_across_prompts() {
        let mut reader = tokio::io::BufReader::new(&b"maybe\ny\n"[..]);
        assert_eq!(expect_found(read_line(&mut reader).await), "maybe");
        assert_eq!(expect_found(read_line(&mut reader).await), "y");
    }

    #[tokio::test]
    async fn read_line_strips_crlf_line_endings() {
        let mut reader = tokio::io::BufReader::new(&b"yes\r\n"[..]);
        assert_eq!(expect_found(read_line(&mut reader).await), "yes");
    }

    /// Regression test for #108: a closed stdin (EOF on the very first read)
    /// must be reported, not silently treated as an empty line to retry on.
    #[tokio::test]
    async fn read_line_reports_eof_as_absent() {
        let mut reader = tokio::io::BufReader::new(&b""[..]);
        assert!(matches!(read_line(&mut reader).await, Presence::Absent));
    }
}
