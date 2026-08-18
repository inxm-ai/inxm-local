//! FAN_OUT and FAN_IN step runners.
//!
//! # v1 simplifications
//!
//! **FAN_OUT**: spawn steps are executed *sequentially* for each item in the
//! list (no goroutines / task spawning).  Each iteration injects the current
//! item into a synthetic `__item__` output map, which makes `${item.VAR}`
//! placeholders work inside spawn steps.  The final spawn step is the map
//! result: only its outputs are collected into the `results` JSON array. This
//! keeps intermediate payloads (for example raw HTML before summarization) out
//! of downstream steps.
//!
//! **FAN_IN**: collects whatever outputs are currently available in
//! `step_outputs` for the listed `from_steps` and stores them as an array.
//! Steps that have not yet completed (or were skipped) contribute `null`.

use crate::error::ExecutorError;
use crate::executor::{FanOutProgress, ProgressEvent, StepRunIteration, StepRunStatus};
use crate::plan::types::StepConfig;
use indexmap::IndexMap;
use std::collections::HashSet;
use std::sync::Arc;

use super::{ITEM_SCOPE_KEY, NamedOutputs, StepContext, StepOutputs, StepResult, run_step};

/// Name of the FAN_OUT step's single output: the JSON array of per-item map
/// results. Must stay in sync with the default output declared by the plan
/// compiler (`crate::plan::steps`).
const RESULTS_OUTPUT: &str = "results";

// ─── FAN_OUT ──────────────────────────────────────────────────────────────────

pub async fn run_fan_out(ctx: &StepContext) -> Result<StepResult, ExecutorError> {
    let cfg = match &ctx.step.config {
        StepConfig::FanOut(c) => c,
        _ => {
            return Err(ExecutorError::StepFailed {
                step_id: ctx.step.id.clone(),
                message: "expected FAN_OUT config".to_owned(),
            });
        }
    };

    // Resolve `over` = "step-id.output-name".
    let (over_step, over_output) =
        parse_over_ref(&cfg.over).ok_or_else(|| ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: format!(
                "FAN_OUT 'over' must be 'step-id.output-name', got: '{}'",
                cfg.over
            ),
        })?;

    let list = resolve_over_value(ctx, over_step, over_output)?;
    let items = coerce_fan_out_items(list);
    let item_count = items.len();

    let item_var = cfg.item_var.clone();
    let spawn_step_ids: Vec<String> = cfg.spawn_steps.clone();

    let mut per_item_results: Vec<serde_json::Value> = Vec::with_capacity(items.len());
    let mut child_runs: IndexMap<String, Vec<StepRunIteration>> = spawn_step_ids
        .iter()
        .map(|step_id| (step_id.clone(), Vec::new()))
        .collect();
    // `until` turns a fan-out into a bounded retry loop: exhausting `over`
    // without a match is a failed postcondition, rather than a successful map
    // whose caller happens to receive only unsuccessful results.
    let mut until_matched = false;

    for (iteration, item) in items.into_iter().enumerate() {
        let iteration_started = std::time::Instant::now();
        tracing::info!(
            name: "inxm.executor.fan_out_iteration.started",
            run_id = %ctx.run_id,
            plan_id = %ctx.plan.metadata.id,
            step_id = %ctx.step.id,
            fan_out_iteration = iteration,
            fan_out_total_iterations = item_count,
            "fan-out iteration started"
        );
        emit_fan_out_progress(ctx, iteration, item_count);

        // Inject the current item under the synthetic item scope so
        // `${item.*}` placeholders resolve inside this iteration's spawn steps.
        let item_map: IndexMap<String, serde_json::Value> =
            IndexMap::from([(item_var.clone(), item)]);
        let iter_base = ctx
            .step_outputs
            .with_output(ITEM_SCOPE_KEY.to_owned(), Arc::new(item_map));

        // Run each spawn step in sequence, accumulating outputs so later
        // spawn steps can reference earlier ones within the same iteration.
        let mut iter_accumulated: IndexMap<String, Arc<NamedOutputs>> = IndexMap::new();
        let mut skipped: HashSet<String> = HashSet::new();

        for spawn_id in &spawn_step_ids {
            let spawn_step = ctx
                .plan
                .step(spawn_id)
                .ok_or_else(|| ExecutorError::StepFailed {
                    step_id: ctx.step.id.clone(),
                    message: format!("spawn step '{spawn_id}' not found in plan"),
                })?;
            if skipped.contains(spawn_id)
                || spawn_step
                    .depends_on
                    .iter()
                    .any(|dependency| skipped.contains(dependency))
            {
                skipped.insert(spawn_id.clone());
                record_skipped_spawn(
                    ctx,
                    spawn_id,
                    iteration,
                    iteration + 1 == item_count,
                    &mut child_runs,
                );
                continue;
            }
            let spawn_outputs = iter_base.with_outputs(&iter_accumulated);

            let outputs = match run_spawn_step(
                ctx,
                spawn_id,
                iteration,
                iteration + 1 == item_count,
                spawn_outputs,
                &mut child_runs,
            )
            .await
            {
                Ok(outputs) => outputs,
                Err(error) => {
                    tracing::warn!(
                        name: "inxm.executor.fan_out_iteration.completed",
                        run_id = %ctx.run_id,
                        plan_id = %ctx.plan.metadata.id,
                        step_id = %ctx.step.id,
                        fan_out_iteration = iteration,
                        fan_out_total_iterations = item_count,
                        fan_out_duration_ms = iteration_started.elapsed().as_millis() as u64,
                        fan_out_outcome = "failed",
                        failure_class = "spawn_step",
                        "fan-out iteration completed"
                    );
                    return Err(error);
                }
            };
            iter_accumulated.insert(spawn_id.clone(), outputs);

            // CONDITION routing must be scoped to this iteration just like
            // its accumulated outputs. The executor's main loop never sees
            // fan-owned steps, so it cannot perform this routing for us.
            if let StepConfig::Condition(condition) = &spawn_step.config {
                let took_true_branch = iter_accumulated
                    .get(spawn_id)
                    .and_then(|outputs| outputs.get(super::condition::RESULT_OUTPUT))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                let untaken = if took_true_branch {
                    &condition.false_steps
                } else {
                    &condition.true_steps
                };
                skipped.extend(untaken.iter().cloned());
            }
        }

        // The last spawn step is the per-item map result. Intermediate outputs
        // remain available within this iteration but are deliberately omitted
        // from FAN_OUT.results so raw payloads do not leak into reduce steps.
        per_item_results.push(collect_final_spawn_outputs(
            &spawn_step_ids,
            &iter_accumulated,
        ));

        let iteration_outputs = iter_base.with_outputs(&iter_accumulated);
        let stop_early = until_satisfied(ctx, cfg.until.as_deref(), &iteration_outputs)?;
        tracing::info!(
            name: "inxm.executor.fan_out_iteration.completed",
            run_id = %ctx.run_id,
            plan_id = %ctx.plan.metadata.id,
            step_id = %ctx.step.id,
            fan_out_iteration = iteration,
            fan_out_total_iterations = item_count,
            fan_out_duration_ms = iteration_started.elapsed().as_millis() as u64,
            fan_out_outcome = "succeeded",
            fan_out_stop_early = stop_early,
            "fan-out iteration completed"
        );
        if stop_early {
            until_matched = true;
            if iteration + 1 < item_count {
                let terminal_status = StepRunStatus::Succeeded;
                for spawn_id in &spawn_step_ids {
                    let child_status = child_runs
                        .get(spawn_id)
                        .filter(|runs| !runs.is_empty())
                        .filter(|runs| runs.iter().all(|run| run.status == StepRunStatus::Skipped))
                        .map_or_else(|| terminal_status.clone(), |_| StepRunStatus::Skipped);
                    emit_child_progress(ctx, spawn_id, child_status, None, None);
                }
            }
            break;
        }
    }

    if let Some(until) = cfg.until.as_deref()
        && !until_matched
    {
        return Err(ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: format!(
                "FAN_OUT exhausted {} iteration(s) without satisfying until condition: {until}",
                item_count
            ),
        });
    }

    let mut outputs = IndexMap::new();
    outputs.insert(
        RESULTS_OUTPUT.to_owned(),
        serde_json::Value::Array(per_item_results),
    );

    Ok(StepResult {
        outputs,
        stdout: None,
        stderr: None,
        usage: None,
        child_runs,
    })
}

// ─── FAN_IN ───────────────────────────────────────────────────────────────────

pub async fn run_fan_in(ctx: &StepContext) -> Result<StepResult, ExecutorError> {
    let cfg = match &ctx.step.config {
        StepConfig::FanIn(c) => c,
        _ => {
            return Err(ExecutorError::StepFailed {
                step_id: ctx.step.id.clone(),
                message: "expected FAN_IN config".to_owned(),
            });
        }
    };

    let collected: Vec<serde_json::Value> = cfg
        .from_steps
        .iter()
        .map(|step_id| {
            ctx.step_outputs
                .get(step_id)
                .map(|outs| serde_json::to_value(outs).unwrap_or(serde_json::Value::Null))
                .unwrap_or(serde_json::Value::Null)
        })
        .collect();

    let mut outputs = IndexMap::new();
    outputs.insert(
        cfg.collect_field.clone(),
        serde_json::Value::Array(collected),
    );

    Ok(StepResult {
        outputs,
        stdout: None,
        stderr: None,
        usage: None,
        child_runs: IndexMap::new(),
    })
}

// ─── Private helpers ──────────────────────────────────────────────────────────

/// Execute one spawn step for one fan-out item.
///
/// Records the completed (or failed) execution in `child_runs`, streams it to
/// the owning executor via `emit_child_progress`, and returns the step's
/// outputs so later spawn steps in the same iteration can reference them.
///
/// The aggregate status reported for the child stays `Running` until the
/// final item, so UIs show the child as in-flight while iterations remain.
async fn run_spawn_step(
    ctx: &StepContext,
    spawn_id: &str,
    iteration: usize,
    is_final_item: bool,
    step_outputs: StepOutputs,
    child_runs: &mut IndexMap<String, Vec<StepRunIteration>>,
) -> Result<Arc<NamedOutputs>, ExecutorError> {
    let spawn_step = ctx
        .plan
        .step(spawn_id)
        .ok_or_else(|| ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: format!("spawn step '{spawn_id}' not found in plan"),
        })?
        .clone();

    let spawn_ctx = StepContext {
        step: spawn_step,
        step_outputs,
        // Each body invocation is a distinct recorded child run. Do not let
        // an AGENT_CALL inherit transcripts from an earlier fan iteration.
        agent_audit: Default::default(),
        ..ctx.clone()
    };

    emit_child_progress(ctx, spawn_id, StepRunStatus::Running, None, None);
    let started_at = chrono::Utc::now();
    let timer = std::time::Instant::now();

    let result = match run_step(spawn_ctx).await {
        Ok(result) => result,
        Err(error) => {
            let error_text = error.to_string();
            let failed_run = StepRunIteration {
                iteration,
                status: StepRunStatus::Failed,
                started_at,
                finished_at: chrono::Utc::now(),
                duration_ms: timer.elapsed().as_millis() as u64,
                outputs: IndexMap::new(),
                stdout: None,
                stderr: None,
                error: Some(error_text.clone()),
                token_usage: None,
            };
            emit_child_progress(
                ctx,
                spawn_id,
                StepRunStatus::Failed,
                Some(error_text),
                Some(failed_run),
            );
            return Err(error);
        }
    };

    let finished_at = chrono::Utc::now();
    let invocation_status = StepRunStatus::Succeeded;
    let aggregate_status = if is_final_item {
        invocation_status.clone()
    } else {
        StepRunStatus::Running
    };
    let super::StepResult {
        outputs,
        stdout,
        stderr,
        usage,
        child_runs: nested_runs,
    } = result;
    let completed_run = StepRunIteration {
        iteration,
        status: invocation_status,
        started_at,
        finished_at,
        duration_ms: timer.elapsed().as_millis() as u64,
        outputs: outputs.clone(),
        stdout,
        stderr,
        error: None,
        token_usage: usage,
    };
    emit_child_progress(
        ctx,
        spawn_id,
        aggregate_status,
        None,
        Some(completed_run.clone()),
    );
    child_runs
        .entry(spawn_id.to_owned())
        .or_default()
        .push(completed_run);
    for (nested_id, mut runs) in nested_runs {
        child_runs.entry(nested_id).or_default().append(&mut runs);
    }

    Ok(Arc::new(outputs))
}

fn record_skipped_spawn(
    ctx: &StepContext,
    spawn_id: &str,
    iteration: usize,
    is_final_item: bool,
    child_runs: &mut IndexMap<String, Vec<StepRunIteration>>,
) {
    let now = chrono::Utc::now();
    let skipped_run = StepRunIteration {
        iteration,
        status: StepRunStatus::Skipped,
        started_at: now,
        finished_at: now,
        duration_ms: 0,
        outputs: IndexMap::new(),
        stdout: None,
        stderr: None,
        error: None,
        token_usage: None,
    };
    let aggregate_status = if is_final_item {
        StepRunStatus::Skipped
    } else {
        StepRunStatus::Running
    };
    emit_child_progress(
        ctx,
        spawn_id,
        aggregate_status,
        None,
        Some(skipped_run.clone()),
    );
    child_runs
        .entry(spawn_id.to_owned())
        .or_default()
        .push(skipped_run);
}

/// Evaluate the optional FAN_OUT `until` expression against this iteration's
/// outputs (base outputs plus everything the iteration accumulated).
/// `Ok(true)` stops the fan-out before the remaining items are processed.
fn until_satisfied(
    ctx: &StepContext,
    until: Option<&str>,
    iteration_outputs: &StepOutputs,
) -> Result<bool, ExecutorError> {
    let Some(until) = until else {
        return Ok(false);
    };

    let missing = super::missing_placeholders(
        &serde_json::Value::String(until.to_owned()),
        &ctx.plan.config,
        iteration_outputs,
    );
    if !missing.is_empty() {
        return Err(ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: format!(
                "FAN_OUT until expression references unavailable iteration output(s): {}",
                missing.join(", ")
            ),
        });
    }

    let (matched, _) =
        super::condition::evaluate_expression(until, &ctx.plan.config, iteration_outputs).map_err(
            |message| ExecutorError::StepFailed {
                step_id: ctx.step.id.clone(),
                message: format!("invalid FAN_OUT until expression: {message}"),
            },
        )?;
    Ok(matched)
}

fn emit_child_progress(
    ctx: &StepContext,
    step_id: &str,
    status: StepRunStatus,
    error: Option<String>,
    iteration: Option<StepRunIteration>,
) {
    if let (Some(child_progress), Some(run)) = (&ctx.child_progress, iteration.as_ref()) {
        let _ = child_progress.send(super::ChildRunEvent {
            step_id: step_id.to_owned(),
            status: status.clone(),
            run: run.clone(),
        });
    }
    if let Some(progress) = &ctx.progress {
        let _ = progress.send(ProgressEvent {
            run_id: ctx.run_id.clone(),
            step_id: step_id.to_owned(),
            status,
            error,
            iteration,
            fan_out_progress: None,
            transcript: None,
        });
    }
}

fn emit_fan_out_progress(ctx: &StepContext, iteration: usize, total_iterations: usize) {
    if let Some(progress) = &ctx.progress {
        let _ = progress.send(ProgressEvent {
            run_id: ctx.run_id.clone(),
            step_id: ctx.step.id.clone(),
            status: StepRunStatus::Running,
            error: None,
            iteration: None,
            fan_out_progress: Some(FanOutProgress {
                iteration,
                total_iterations,
            }),
            transcript: None,
        });
    }
}

/// Parse `"step-id.output-name"` into `(&str, &str)`.
fn parse_over_ref(over: &str) -> Option<(&str, &str)> {
    let dot = over.find('.')?;
    Some((&over[..dot], &over[dot + 1..]))
}

fn resolve_over_value(
    ctx: &StepContext,
    over_step: &str,
    over_output: &str,
) -> Result<serde_json::Value, ExecutorError> {
    let Some(outputs) = ctx.step_outputs.get(over_step) else {
        return Err(ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: format!(
                "FAN_OUT input '{}.{}' is unavailable because step '{}' has not produced outputs yet. Available steps: {}",
                over_step,
                over_output,
                over_step,
                available_step_outputs(&ctx.step_outputs),
            ),
        });
    };

    outputs
        .get(over_output)
        .cloned()
        .ok_or_else(|| ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: format!(
                "FAN_OUT input '{}.{}' does not exist. Step '{}' produced: {}",
                over_step,
                over_output,
                over_step,
                available_outputs(outputs),
            ),
        })
}

fn coerce_fan_out_items(value: serde_json::Value) -> Vec<serde_json::Value> {
    match value {
        serde_json::Value::Array(arr) => arr,
        serde_json::Value::String(s) => parse_json_array_from_text(&s)
            // Treat non-array strings as a single-element list, preserving the
            // v1 runner behaviour for scalar fan-out inputs.
            .unwrap_or_else(|| vec![serde_json::Value::String(s)]),
        // Treat a non-array as a single-element list.
        single => vec![single],
    }
}

fn collect_final_spawn_outputs(
    spawn_step_ids: &[String],
    accumulated: &IndexMap<String, Arc<NamedOutputs>>,
) -> serde_json::Value {
    let Some(outputs) = spawn_step_ids
        .last()
        .and_then(|final_step_id| accumulated.get(final_step_id))
    else {
        return serde_json::Value::Null;
    };

    // FAN_OUT behaves like a conventional map: a body with one final output
    // yields a list of those values. Multi-output bodies retain an object,
    // using the declared output names directly. Prefixing every field with
    // the body step id made deterministic reducers silently miss their data.
    if outputs.len() == 1 {
        return outputs
            .values()
            .next()
            .cloned()
            .unwrap_or(serde_json::Value::Null);
    }

    serde_json::Value::Object(
        outputs
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    )
}

fn parse_json_array_from_text(text: &str) -> Option<Vec<serde_json::Value>> {
    let trimmed = text.trim();
    parse_json_array(trimmed)
        .or_else(|| first_fenced_block(trimmed).and_then(parse_json_array))
        .or_else(|| bracketed_array_slice(trimmed).and_then(parse_json_array))
}

fn parse_json_array(candidate: &str) -> Option<Vec<serde_json::Value>> {
    match serde_json::from_str::<serde_json::Value>(candidate).ok()? {
        serde_json::Value::Array(arr) => Some(arr),
        _ => None,
    }
}

fn first_fenced_block(text: &str) -> Option<&str> {
    let fence_start = text.find("```")?;
    let after_open = &text[fence_start + 3..];
    let content_start = after_open.find('\n').map(|i| i + 1).unwrap_or(0);
    let rest = &after_open[content_start..];
    let fence_end = rest.find("```")?;
    Some(rest[..fence_end].trim())
}

fn bracketed_array_slice(text: &str) -> Option<&str> {
    let start = text.find('[')?;
    let end = text.rfind(']')?;
    (start < end).then_some(text[start..=end].trim())
}

fn available_step_outputs(step_outputs: &StepOutputs) -> String {
    if step_outputs.is_empty() {
        return "none".to_owned();
    }

    step_outputs
        .entries()
        .map(|(step_id, outputs)| format!("{} ({})", step_id, available_outputs(outputs)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn available_outputs(outputs: &IndexMap<String, serde_json::Value>) -> String {
    if outputs.is_empty() {
        "no named outputs".to_owned()
    } else {
        outputs.keys().cloned().collect::<Vec<_>>().join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmAuth, LlmProfile, LlmProtocol};
    use crate::plan::types::{
        AgentCallConfig, ConditionConfig, FanOutConfig, Plan, PlanMetadata, PlanOutput, PlanStep,
        StepConfig,
    };
    use crate::tools::catalog::ToolCatalog;
    use serde_json::json;

    #[test]
    fn fan_out_items_accept_native_arrays() {
        assert_eq!(
            coerce_fan_out_items(json!(["a", "b"])),
            vec![json!("a"), json!("b")]
        );
    }

    #[test]
    fn fan_out_items_parse_fenced_json_arrays() {
        let text = "```json\n[\"https://example.com/a\", \"https://example.com/b\"]\n```";
        assert_eq!(
            coerce_fan_out_items(json!(text)),
            vec![
                json!("https://example.com/a"),
                json!("https://example.com/b")
            ]
        );
    }

    #[test]
    fn fan_out_items_parse_arrays_with_surrounding_text() {
        let text = "Here are the URLs:\n[\"/a\", \"/b\"]";
        assert_eq!(
            coerce_fan_out_items(json!(text)),
            vec![json!("/a"), json!("/b")]
        );
    }

    #[test]
    fn fan_out_items_keep_scalar_strings_as_single_item() {
        assert_eq!(coerce_fan_out_items(json!("one")), vec![json!("one")]);
    }

    #[test]
    fn fan_out_collects_single_final_output_as_map_value() {
        let spawn_steps = vec!["fetch_post".to_owned(), "summarize_post".to_owned()];
        let accumulated = IndexMap::from([
            (
                "fetch_post".to_owned(),
                Arc::new(IndexMap::from([(
                    "body".to_owned(),
                    json!("<html>large</html>"),
                )])),
            ),
            (
                "summarize_post".to_owned(),
                Arc::new(IndexMap::from([(
                    "summary".to_owned(),
                    json!("A compact summary"),
                )])),
            ),
        ]);

        assert_eq!(
            collect_final_spawn_outputs(&spawn_steps, &accumulated),
            json!("A compact summary")
        );
    }

    #[test]
    fn fan_out_collects_multiple_final_outputs_by_declared_name() {
        let spawn_steps = vec!["review".to_owned()];
        let accumulated = [(
            "review".to_owned(),
            Arc::new(
                [
                    ("verdict".to_owned(), json!("pass")),
                    ("score".to_owned(), json!(97)),
                ]
                .into_iter()
                .collect(),
            ),
        )]
        .into_iter()
        .collect();

        assert_eq!(
            collect_final_spawn_outputs(&spawn_steps, &accumulated),
            json!({"verdict": "pass", "score": 97})
        );
    }

    #[tokio::test]
    async fn fan_out_until_stops_after_first_matching_iteration() {
        let owner = PlanStep {
            id: "retry".to_owned(),
            name: "Retry until matched".to_owned(),
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
        let verifier = PlanStep {
            id: "verify".to_owned(),
            name: "Verify attempt".to_owned(),
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
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "bounded retry".to_owned(),
            description: None,
            inputs: vec![],
            config: IndexMap::new(),
            steps: vec![owner.clone(), verifier],
            outputs: vec![],
        };
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = StepContext {
            plan: std::sync::Arc::new(plan),
            step: owner,
            step_outputs: IndexMap::from([(
                "attempts".to_owned(),
                IndexMap::from([("values".to_owned(), json!([1, 2, 3, 4, 5]))]),
            )])
            .into(),
            catalog: ToolCatalog::default(),
            global_timeout_secs: None,
            human: None,
            run_id: "test-run".to_owned(),
            progress: Some(progress_tx),
            child_progress: None,
            llm_keys: Default::default(),
            storage_root: std::env::temp_dir(),
            agent_audit: Default::default(),
        };

        let result = run_fan_out(&ctx).await.expect("fan-out should succeed");

        assert_eq!(result.outputs["results"], json!([false, false, true]));
        assert_eq!(result.child_runs["verify"].len(), 3);
        let verify_events: Vec<ProgressEvent> = std::iter::from_fn(|| progress_rx.try_recv().ok())
            .filter(|event| event.step_id == "verify")
            .collect();
        assert_eq!(
            verify_events.last().map(|event| &event.status),
            Some(&StepRunStatus::Succeeded),
            "early termination must publish a terminal aggregate child status"
        );

        let mut exhausted_ctx = ctx.clone();
        let StepConfig::FanOut(config) = &mut exhausted_ctx.step.config else {
            unreachable!("test owner must remain a FAN_OUT")
        };
        config.until = Some("${step.verify.result} == \"never\"".to_owned());
        let error = match run_fan_out(&exhausted_ctx).await {
            Err(error) => error,
            Ok(_) => panic!("an unmatched until condition must fail the fan-out"),
        };
        assert!(
            error
                .to_string()
                .contains("FAN_OUT exhausted 5 iteration(s) without satisfying until condition"),
            "the failure should make the exhausted retry postcondition clear"
        );

        let mut missing_output_ctx = ctx.clone();
        let StepConfig::FanOut(config) = &mut missing_output_ctx.step.config else {
            unreachable!("test owner must remain a FAN_OUT")
        };
        config.until = Some("${step.verify.missing} == true".to_owned());
        let error = match run_fan_out(&missing_output_ctx).await {
            Err(error) => error,
            Ok(_) => panic!("missing until output should fail"),
        };
        assert!(
            error
                .to_string()
                .contains("until expression references unavailable iteration output")
        );
    }

    #[tokio::test]
    async fn fan_out_executes_and_records_agent_call_for_each_item() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let owner = PlanStep {
            id: "spread".to_owned(),
            name: "spread".to_owned(),
            description: None,
            config: StepConfig::FanOut(FanOutConfig {
                over: "source.items".to_owned(),
                item_var: "task".to_owned(),
                spawn_steps: vec!["agent".to_owned()],
                until: None,
            }),
            depends_on: vec!["source".to_owned()],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let agent = PlanStep {
            id: "agent".to_owned(),
            name: "agent".to_owned(),
            description: None,
            config: StepConfig::AgentCall(AgentCallConfig {
                objective: "work ${item.task}".to_owned(),
                working_dir: workspace.path().display().to_string(),
                timeout_secs: Some(5),
            }),
            depends_on: vec![],
            outputs: vec![PlanOutput {
                name: "result".to_owned(),
                description: None,
                value_type: "string".to_owned(),
            }],
            timeout_secs: None,
            retry: None,
        };
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "agent fan-out".to_owned(),
            description: None,
            inputs: vec![],
            config: IndexMap::new(),
            steps: vec![owner.clone(), agent],
            outputs: vec![],
        };
        let mut ctx = bare_ctx(plan, owner);
        ctx.step_outputs = IndexMap::from([(
            "source".to_owned(),
            IndexMap::from([("items".to_owned(), json!(["first", "second"]))]),
        )])
        .into();
        ctx.llm_keys.profile = Some(LlmProfile {
            id: "test-agent".to_owned(),
            name: "test-agent".to_owned(),
            protocol: LlmProtocol::CustomCli,
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            auth: LlmAuth::None,
            headers: IndexMap::new(),
            executable: String::new(),
            command_template: "cat".to_owned(),
            max_tokens: None,
            temperature: None,
            timeout_secs: 5,
            codex_sandbox_mode: crate::llm::CodexSandboxMode::default(),
        });

        let result = run_fan_out(&ctx).await.expect("fan-out should succeed");

        assert_eq!(
            result.outputs["results"],
            json!(["work first", "work second"])
        );
        let runs = &result.child_runs["agent"];
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].iteration, 0);
        assert_eq!(runs[0].stdout.as_deref(), Some("work first\n"));
        assert_eq!(runs[1].iteration, 1);
        assert_eq!(
            runs[1].stdout.as_deref(),
            Some("work second\n"),
            "each iteration must retain only its own agent transcript"
        );
    }

    #[tokio::test]
    async fn fan_out_condition_skips_untaken_agent_branch_and_its_dependents() {
        let workspace = tempfile::tempdir().expect("tempdir");
        let owner = PlanStep {
            id: "spread".to_owned(),
            name: "spread".to_owned(),
            description: None,
            config: StepConfig::FanOut(FanOutConfig {
                over: "source.items".to_owned(),
                item_var: "item".to_owned(),
                spawn_steps: vec![
                    "check".to_owned(),
                    "repair".to_owned(),
                    "after_repair".to_owned(),
                ],
                until: None,
            }),
            depends_on: vec!["source".to_owned()],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let check = PlanStep {
            id: "check".to_owned(),
            name: "check".to_owned(),
            description: None,
            config: StepConfig::Condition(ConditionConfig {
                expression: "true".to_owned(),
                true_steps: vec![],
                false_steps: vec!["repair".to_owned()],
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let repair = PlanStep {
            id: "repair".to_owned(),
            name: "repair".to_owned(),
            description: None,
            config: StepConfig::AgentCall(AgentCallConfig {
                objective: "must not run".to_owned(),
                working_dir: workspace.path().display().to_string(),
                timeout_secs: Some(1),
            }),
            depends_on: vec!["check".to_owned()],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let after_repair = PlanStep {
            id: "after_repair".to_owned(),
            name: "after repair".to_owned(),
            description: None,
            config: StepConfig::ToolCall(crate::plan::types::ToolCallConfig {
                tool: "must-not-run".to_owned(),
                arguments: IndexMap::new(),
            }),
            depends_on: vec!["repair".to_owned()],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "conditional agent fan-out".to_owned(),
            description: None,
            inputs: vec![],
            config: IndexMap::new(),
            steps: vec![owner.clone(), check, repair, after_repair],
            outputs: vec![],
        };
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = bare_ctx(plan, owner);
        ctx.step_outputs = IndexMap::from([(
            "source".to_owned(),
            IndexMap::from([("items".to_owned(), json!(["one"]))]),
        )])
        .into();
        ctx.progress = Some(progress_tx);

        let result = run_fan_out(&ctx)
            .await
            .expect("untaken agent branch must not execute");

        assert_eq!(
            result.child_runs["check"][0].status,
            StepRunStatus::Succeeded
        );
        assert_eq!(
            result.child_runs["repair"][0].status,
            StepRunStatus::Skipped
        );
        assert_eq!(
            result.child_runs["after_repair"][0].status,
            StepRunStatus::Skipped,
            "a step depending on a skipped branch must also be skipped"
        );
        let skipped_events = std::iter::from_fn(|| progress_rx.try_recv().ok())
            .filter(|event| event.status == StepRunStatus::Skipped)
            .collect::<Vec<_>>();
        assert!(skipped_events.iter().any(|event| {
            event.step_id == "repair"
                && event
                    .iteration
                    .as_ref()
                    .is_some_and(|run| run.iteration == 0 && run.status == StepRunStatus::Skipped)
        }));
        assert!(
            skipped_events
                .iter()
                .any(|event| event.step_id == "after_repair")
        );
    }

    fn bare_ctx(plan: Plan, step: PlanStep) -> StepContext {
        StepContext {
            plan: std::sync::Arc::new(plan),
            step,
            step_outputs: IndexMap::new().into(),
            catalog: ToolCatalog::default(),
            global_timeout_secs: None,
            human: None,
            run_id: "test-run".to_owned(),
            progress: None,
            child_progress: None,
            llm_keys: Default::default(),
            storage_root: std::env::temp_dir(),
            agent_audit: Default::default(),
        }
    }

    fn single_step_plan(step: PlanStep) -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![],
            config: IndexMap::new(),
            steps: vec![step],
            outputs: vec![],
        }
    }

    #[tokio::test]
    async fn fan_in_collects_outputs_and_null_for_missing_steps() {
        let step = PlanStep {
            id: "gather".to_owned(),
            name: "gather".to_owned(),
            description: None,
            config: StepConfig::FanIn(crate::plan::types::FanInConfig {
                from_steps: vec!["done".to_owned(), "never_ran".to_owned()],
                collect_field: "gathered".to_owned(),
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let mut ctx = bare_ctx(single_step_plan(step.clone()), step);
        ctx.step_outputs = IndexMap::from([(
            "done".to_owned(),
            IndexMap::from([("value".to_owned(), json!(7))]),
        )])
        .into();

        let result = run_fan_in(&ctx).await.expect("fan-in should succeed");

        assert_eq!(
            result.outputs["gathered"],
            json!([{ "value": 7 }, null]),
            "completed steps contribute their outputs; missing steps contribute null"
        );
    }

    #[tokio::test]
    async fn fan_out_rejects_over_reference_without_output_name() {
        let step = PlanStep {
            id: "spread".to_owned(),
            name: "spread".to_owned(),
            description: None,
            config: StepConfig::FanOut(FanOutConfig {
                over: "no-dot-here".to_owned(),
                item_var: "item".to_owned(),
                spawn_steps: vec![],
                until: None,
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let ctx = bare_ctx(single_step_plan(step.clone()), step);

        let Err(error) = run_fan_out(&ctx).await else {
            panic!("bad 'over' must fail");
        };

        assert!(
            error
                .to_string()
                .contains("'over' must be 'step-id.output-name'")
        );
    }

    #[tokio::test]
    async fn fan_out_spawn_failure_propagates_and_reports_failed_child_run() {
        let owner = PlanStep {
            id: "spread".to_owned(),
            name: "spread".to_owned(),
            description: None,
            config: StepConfig::FanOut(FanOutConfig {
                over: "source.items".to_owned(),
                item_var: "item".to_owned(),
                spawn_steps: vec!["broken".to_owned()],
                until: None,
            }),
            depends_on: vec!["source".to_owned()],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        // A TOOL_CALL naming a tool absent from the (empty) catalog fails on
        // its first iteration.
        let broken = PlanStep {
            id: "broken".to_owned(),
            name: "broken".to_owned(),
            description: None,
            config: StepConfig::ToolCall(crate::plan::types::ToolCallConfig {
                tool: "no-such-tool".to_owned(),
                arguments: IndexMap::new(),
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![],
            config: IndexMap::new(),
            steps: vec![owner.clone(), broken],
            outputs: vec![],
        };
        let (child_tx, mut child_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ctx = bare_ctx(plan, owner);
        ctx.step_outputs = IndexMap::from([(
            "source".to_owned(),
            IndexMap::from([("items".to_owned(), json!(["only-item"]))]),
        )])
        .into();
        ctx.child_progress = Some(child_tx);

        let Err(error) = run_fan_out(&ctx).await else {
            panic!("spawn failure must fail the fan-out");
        };
        assert!(error.to_string().contains("no-such-tool"));

        // Skip the initial Running event (sent without an iteration record)
        // and find the terminal Failed record for the spawn step.
        let mut failed_event = None;
        while let Ok(event) = child_rx.try_recv() {
            if matches!(event.status, StepRunStatus::Failed) {
                failed_event = Some(event);
            }
        }
        let failed_event = failed_event.expect("a Failed child-run event must be reported");
        assert_eq!(failed_event.step_id, "broken");
        assert_eq!(failed_event.run.iteration, 0);
        assert!(
            failed_event
                .run
                .error
                .as_deref()
                .is_some_and(|message| message.contains("no-such-tool"))
        );
    }
}
