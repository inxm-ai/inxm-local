//! Deterministic plan executor.
//!
//! Steps are dispatched in topological order.  For every step the run state
//! is persisted to disk before and after execution, giving a full audit trail
//! even when the process crashes mid-run.
//!
//! # FAN_OUT ownership
//!
//! Steps listed in a `FAN_OUT` step's `spawn_steps` are **owned** by that
//! runner — they are skipped in the main loop and executed inside
//! `fan::run_fan_out` (once per list item).  This prevents double-execution.

pub mod dag;
pub mod step_runners;

pub use crate::storage::runs::{Run, RunStatus, StepRun, StepRunIteration, StepRunStatus};

use crate::error::ExecutorError;
use crate::plan::types::{Plan, StepConfig};
use crate::storage::StorageRoot;
use crate::tools::catalog::ToolCatalog;
use indexmap::IndexMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tracing::Instrument;

/// Error recorded on a step (and streamed to UIs) when the operator rejects
/// a HUMAN_INTERACTION approval.
const REJECTED_BY_OPERATOR_MESSAGE: &str = "rejected by the operator";
const MAX_BACKOFF_SHIFT: u32 = 31;
const PROCESS_REAP_GRACE_SECS: u64 = 1;

// ─── UI hooks ─────────────────────────────────────────────────────────────────

/// A step status transition, emitted while a run is in flight so a UI can
/// render live progress. Purely observational — the executor never waits on
/// the receiver.
#[derive(Debug, Clone)]
pub struct ProgressEvent {
    pub run_id: String,
    pub step_id: String,
    pub status: StepRunStatus,
    pub error: Option<String>,
    /// Completed per-item execution when this event came from a FAN_OUT body.
    pub iteration: Option<StepRunIteration>,
    /// Live position of a running FAN_OUT owner, zero-based.
    pub fan_out_progress: Option<FanOutProgress>,
    pub transcript: Option<AgentTranscriptEvent>,
}

/// One unmodified line emitted by an agent CLI.  Consumers should retain the
/// ordering in which events arrive; the complete byte streams are also stored
/// on the finished [`StepRun`] for audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTranscriptEvent {
    pub run_id: String,
    pub step_id: String,
    pub stream: AgentTranscriptStream,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTranscriptStream {
    Stdin,
    Stdout,
    Stderr,
}

/// Progress reported by a FAN_OUT owner while it processes its input list.
#[derive(Debug, Clone, Copy)]
pub struct FanOutProgress {
    pub iteration: usize,
    pub total_iterations: usize,
}

/// The operator's answer to a [`HumanRequest`].
#[derive(Debug, Clone)]
pub enum HumanDecision {
    Approve,
    Reject,
    Text(String),
}

/// A HUMAN_INTERACTION step waiting for an answer.
///
/// Sent over `ExecutorConfig::human` when present; the step runner awaits the
/// `respond` oneshot. When no channel is configured the runner falls back to
/// stdin (headless/test mode).
#[derive(Debug)]
pub struct HumanRequest {
    pub step_id: String,
    pub prompt: String,
    pub approval_required: bool,
    pub response_field: String,
    pub respond: tokio::sync::oneshot::Sender<HumanDecision>,
}

/// API keys for PROMPT_CALL steps. Each slot falls back to its environment
/// variable (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY`) when `None`.
#[derive(Debug, Clone, Default)]
pub struct LlmKeys {
    pub anthropic: Option<String>,
    pub openai: Option<String>,
    /// Explicit connection selected by the application. When absent, retain
    /// legacy model-prefix routing for library callers.
    pub profile: Option<crate::llm::LlmProfile>,
    /// Configuration error captured while constructing the application
    /// profile. Prompt steps surface it instead of silently using legacy
    /// provider inference.
    pub profile_error: Option<String>,
}

// ─── Configuration ────────────────────────────────────────────────────────────

pub struct ExecutorConfig {
    /// Values supplied for this invocation. They are validated against `plan.inputs`.
    pub inputs: IndexMap<String, serde_json::Value>,
    /// Default per-step timeout in seconds; individual steps may override this.
    pub timeout_secs: Option<u64>,
    pub storage: Arc<StorageRoot>,
    pub catalog: ToolCatalog,
    /// Optional live step-status stream for UIs.
    pub progress: Option<tokio::sync::mpsc::UnboundedSender<ProgressEvent>>,
    /// Optional channel for HUMAN_INTERACTION prompts; stdin fallback when `None`.
    pub human: Option<tokio::sync::mpsc::UnboundedSender<HumanRequest>>,
    /// Credentials for PROMPT_CALL steps (env-var fallback when unset).
    pub llm_keys: LlmKeys,
    /// Which surface asked for this execution; stamped on the run record so
    /// the Runs view can show where a run came from. Resume paths
    /// keep the run's existing value.
    pub source: Option<crate::storage::runs::RunSource>,
}

// ─── Entry point ─────────────────────────────────────────────────────────────

/// Execute a plan and return the final run state.
///
/// The run is persisted to disk after every step so it can be inspected or
/// resumed after a failure.
pub async fn execute(mut plan: Plan, config: ExecutorConfig) -> Result<Run, ExecutorError> {
    let inputs = plan.resolve_inputs(&config.inputs)?;
    inject_runtime_inputs(&mut plan, &inputs);

    // ── 1. Create run, initialise step entries, save immediately ─────────────
    let mut run = Run::new(plan.metadata.id.clone(), plan.metadata.version);
    run.inputs = inputs;
    run.source = config.source;
    for step in &plan.steps {
        run.step_runs
            .insert(step.id.clone(), StepRun::new(&step.id));
    }
    save_run(&run, &config)?;
    execute_run(plan, config, run).await
}

/// Resume a run paused at a `HUMAN_INTERACTION` step without repeating
/// already-completed steps or changing the run identity.
pub async fn resume(
    mut plan: Plan,
    config: ExecutorConfig,
    run: Run,
) -> Result<Run, ExecutorError> {
    let invalid_resume = |reason: String| ExecutorError::InvalidResume {
        run_id: run.id.clone(),
        reason,
    };
    let supplied = plan.resolve_inputs(&config.inputs)?;
    if supplied != run.inputs {
        return Err(invalid_resume(
            "must resume with its original inputs".to_owned(),
        ));
    }

    let waiting_step_id = match &run.status {
        RunStatus::WaitingForHuman { step_id } => step_id,
        status => {
            return Err(invalid_resume(format!(
                "cannot be resumed from status {status}"
            )));
        }
    };
    if run.plan_id != plan.metadata.id || run.plan_version != plan.metadata.version {
        return Err(invalid_resume(format!(
            "belongs to plan {} v{}, not {} v{}",
            run.plan_id, run.plan_version, plan.metadata.id, plan.metadata.version
        )));
    }

    let waiting_step = plan
        .step(waiting_step_id)
        .ok_or_else(|| invalid_resume(format!("waits for missing step '{waiting_step_id}'")))?;
    if !matches!(waiting_step.config, StepConfig::HumanInteraction(_))
        || run
            .step_runs
            .get(waiting_step_id)
            .is_none_or(|step_run| step_run.status != StepRunStatus::WaitingForHuman)
    {
        return Err(invalid_resume(format!(
            "has an invalid human-interaction checkpoint at '{waiting_step_id}'"
        )));
    }
    inject_runtime_inputs(&mut plan, &run.inputs);

    execute_run(plan, config, run).await
}

/// How a failed run earned the right to be resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairResumeMode {
    /// A repair patch produced a NEWER plan version; the resume must run
    /// against it.
    PatchedPlan,
    /// The plan was diagnosed as correct — the world was wrong and the human
    /// has repaired it. The resume runs against the SAME plan version the run
    /// originally executed.
    WorldFixed,
}

/// Resume a run after a repair: reset the originally failed step and
/// everything transitively downstream of it (in the resumed plan's dependency
/// graph — recomputed fresh, since a patch may have inserted, replaced, or
/// rewired steps) to `Pending`, then continue the SAME run id through
/// `execute_run`'s checkpoint-based scheduler. Every other step's persisted
/// `StepRun` is left untouched, so already-succeeded work is never repeated.
///
/// `mode` states why retrying is not futile: [`RepairResumeMode::PatchedPlan`]
/// requires a plan version newer than the run's (a patch changed the plan);
/// [`RepairResumeMode::WorldFixed`] requires the SAME version (the plan was
/// fine — the environment was repaired instead, so re-running the identical
/// step is exactly the point).
///
/// Unlike [`resume`], this does not require `run.status` to be
/// `WaitingForHuman` — it is meant to run right after `repair::apply_patch`
/// has bumped the plan version (or a world fix has been carried out) for a
/// run whose status is `Failed`.
#[tracing::instrument(
    name = "inxm.executor.repair_resume",
    skip_all,
    fields(
        run.id = %run.id,
        plan.id = %plan.metadata.id,
        plan.version = plan.metadata.version,
        run.plan_version = run.plan_version
    )
)]
pub async fn resume_from_repair(
    mut plan: Plan,
    config: ExecutorConfig,
    mut run: Run,
    input_overrides: IndexMap<String, serde_json::Value>,
    mode: RepairResumeMode,
) -> Result<Run, ExecutorError> {
    let invalid_resume = |reason: String| ExecutorError::InvalidResume {
        run_id: run.id.clone(),
        reason,
    };
    let failed_step_id = match &run.status {
        RunStatus::Failed { failed_step_id, .. } => failed_step_id.clone(),
        status => {
            return Err(invalid_resume(format!(
                "cannot resume from repair with status {status}"
            )));
        }
    };
    if plan.metadata.id != run.plan_id {
        return Err(invalid_resume(format!(
            "belongs to plan {}, not {}",
            run.plan_id, plan.metadata.id
        )));
    }
    match mode {
        RepairResumeMode::PatchedPlan => {
            if plan.metadata.version <= run.plan_version {
                return Err(invalid_resume(format!(
                    "repair plan version {} must be newer than run version {}",
                    plan.metadata.version, run.plan_version
                )));
            }
        }
        RepairResumeMode::WorldFixed => {
            // A world fix leaves the plan untouched by definition. A version
            // drift here means a patch landed in between — that resume must
            // go through `PatchedPlan` semantics instead.
            if plan.metadata.version != run.plan_version {
                return Err(invalid_resume(format!(
                    "world-fixed resume requires the unchanged plan version {}, got {}",
                    run.plan_version, plan.metadata.version
                )));
            }
        }
    }

    if run
        .step_runs
        .get(&failed_step_id)
        .is_none_or(|step_run| step_run.status != StepRunStatus::Failed)
    {
        return Err(invalid_resume(format!(
            "has no failed step checkpoint at '{failed_step_id}'"
        )));
    }

    // Rebase the invocation onto the repaired plan's input contract before
    // modifying checkpoints. This retains values still declared by the new
    // plan, applies newly introduced defaults, drops removed inputs, and
    // validates any submitted repair overrides.
    let reset_step_ids = repair_reset_step_ids(&plan, &run, &failed_step_id);
    let inputs = rebase_repair_inputs(&plan, &run, &input_overrides, &reset_step_ids)?;
    run.inputs = inputs;
    inject_runtime_inputs(&mut plan, &run.inputs);

    // A step whose ID no longer exists in the new plan (renamed/replaced by
    // the patch) carries no useful state forward for that plan; drop it
    // rather than leak a stale entry. `execute_run` treats a missing entry
    // exactly like a fresh `Pending` one, so this is harmless.
    run.step_runs
        .retain(|step_id, _| plan.step(step_id).is_some());

    for step_id in reset_step_ids {
        run.step_runs
            .insert(step_id.clone(), StepRun::new(&step_id));
    }
    for step in &plan.steps {
        run.step_runs
            .entry(step.id.clone())
            .or_insert_with(|| StepRun::new(&step.id));
    }

    run.plan_id = plan.metadata.id.clone();
    run.plan_version = plan.metadata.version;
    tracing::info!(
        name: "inxm.executor.repair_resume.accepted",
        run_id = %run.id,
        plan_id = %run.plan_id,
        plan_version = run.plan_version,
        failed_step_id = %failed_step_id,
        lifecycle_step_count = run.step_runs.len(),
        "repair resume accepted"
    );

    // Persist the accepted, revalidated input contract before any reset step
    // can execute. This preserves the audit trail even if the process exits
    // between accepting the repair and the first checkpoint transition.
    save_run(&run, &config)?;

    execute_run(plan, config, run).await
}

/// Steps whose checkpoints cannot be reused after a repair.
///
/// Newly introduced steps are reset roots as well as the recorded failed step.
/// This covers `ReplaceStep` patches that rename the failed owner: the old ID
/// is absent from the repaired graph, while the replacement, its dependency
/// downstream, and any FAN_OUT-owned bodies must all start fresh.
fn repair_reset_step_ids(plan: &Plan, run: &Run, failed_step_id: &str) -> HashSet<String> {
    let mut roots = vec![failed_step_id.to_owned()];
    roots.extend(
        plan.steps
            .iter()
            .filter(|step| !run.step_runs.contains_key(&step.id))
            .map(|step| step.id.clone()),
    );

    let mut reset_step_ids = HashSet::new();
    for root in roots {
        reset_step_ids.extend(dag::downstream_closure(plan, &root));
    }
    let fan_out_owners: Vec<String> = reset_step_ids.iter().cloned().collect();
    for owner_id in fan_out_owners {
        reset_step_ids.extend(crate::plan::steps::fan_out_body_closure(plan, &owner_id));
    }

    reset_step_ids
}

/// Rebase an invocation from a failed run onto a repaired plan and ensure an
/// operator cannot change a value that has already contributed to persisted
/// successful work.
///
/// Input reference discovery belongs to the typed plan-step subsystem. The
/// executor only combines that plan fact with checkpoint state.
fn rebase_repair_inputs(
    plan: &Plan,
    run: &Run,
    overrides: &IndexMap<String, serde_json::Value>,
    reset_step_ids: &HashSet<String>,
) -> Result<IndexMap<String, serde_json::Value>, ExecutorError> {
    let mut supplied = IndexMap::new();
    for input in &plan.inputs {
        if let Some(value) = run.inputs.get(&input.name) {
            supplied.insert(input.name.clone(), value.clone());
        }
    }
    for (name, value) in overrides {
        supplied.insert(name.clone(), value.clone());
    }

    let resolved = plan.resolve_inputs(&supplied)?;
    for (name, value) in overrides {
        if run.inputs.get(name) == Some(value) {
            continue;
        }

        let protected_step = plan.steps.iter().find(|step| {
            crate::plan::steps::input_references(step).contains(name)
                && step_has_persisted_success(run, &step.id, reset_step_ids)
        });
        if let Some(step) = protected_step {
            return Err(ExecutorError::InvalidResume {
                run_id: run.id.clone(),
                reason: format!(
                    "input '{name}' cannot change because succeeded step '{}' used its original value",
                    step.id
                ),
            });
        }
    }

    Ok(resolved)
}

/// True when the named step's successful result prevents an input override.
/// A reset ordinary step is safe to re-run with a replacement input, but a
/// completed fan-out iteration always protects its original input: it may
/// have produced durable per-item output even when its template is reset.
fn step_has_persisted_success(run: &Run, step_id: &str, reset_step_ids: &HashSet<String>) -> bool {
    let Some(step_run) = run.step_runs.get(step_id) else {
        return false;
    };

    let succeeded_iteration_survives = step_run
        .iterations
        .iter()
        .any(|iteration| iteration.status == StepRunStatus::Succeeded);
    let succeeded_step_survives =
        step_run.status == StepRunStatus::Succeeded && !reset_step_ids.contains(step_id);

    succeeded_iteration_survives || succeeded_step_survives
}

#[tracing::instrument(
    name = "inxm.executor.run",
    skip_all,
    fields(
        run.id = %run.id,
        plan.id = %plan.metadata.id,
        plan.version = plan.metadata.version
    )
)]
async fn execute_run(
    plan: Plan,
    config: ExecutorConfig,
    mut run: Run,
) -> Result<Run, ExecutorError> {
    // Shared read-only from here on; step contexts clone the Arc, not the plan.
    let plan = std::sync::Arc::new(plan);
    run.status = RunStatus::Running;
    run.finished_at = None;
    save_run(&run, &config)?;
    tracing::info!(
        name: "inxm.executor.run.started",
        run_id = %run.id,
        plan_id = %plan.metadata.id,
        plan_version = plan.metadata.version,
        run_step_count = plan.steps.len(),
        "executor run started"
    );

    // ── 2. Build execution order ──────────────────────────────────────────────
    let step_order: Vec<String> = dag::topological_order(&plan)
        .into_iter()
        .map(|s| s.id.clone())
        .collect();

    // Collect step IDs owned by FAN_OUT steps; the main loop skips these.
    let fan_out_owned: HashSet<String> = plan
        .steps
        .iter()
        .filter_map(|s| {
            if let StepConfig::FanOut(cfg) = &s.config {
                Some(cfg.spawn_steps.iter().cloned())
            } else {
                None
            }
        })
        .flatten()
        .collect();

    // ── 3. Rebuild execution context from persisted checkpoints ──────────────
    let mut completed: HashSet<String> = HashSet::new();
    let mut failed_or_skipped: HashSet<String> = HashSet::new();
    let mut step_outputs = step_runners::StepOutputs::default();
    for (step_id, step_run) in &run.step_runs {
        match step_run.status {
            StepRunStatus::Succeeded => {
                completed.insert(step_id.clone());
                step_outputs.insert_base(step_id.clone(), step_run.outputs.clone());
            }
            StepRunStatus::Failed | StepRunStatus::Skipped | StepRunStatus::Cancelled => {
                failed_or_skipped.insert(step_id.clone());
            }
            // `Unknown` is a status this build does not recognize (e.g. one
            // added by a newer release). Treat it as not-yet-completed so a
            // resume re-runs the step rather than trusting an opaque status
            // it cannot interpret.
            StepRunStatus::Pending
            | StepRunStatus::Running
            | StepRunStatus::WaitingForHuman
            | StepRunStatus::Unknown => {}
        }
    }

    // ── 4. Execute pending steps sequentially ─────────────────────────────────
    for step_id in &step_order {
        if completed.contains(step_id)
            || failed_or_skipped.contains(step_id)
            || fan_out_owned.contains(step_id)
        {
            continue;
        }

        let step = match plan.step(step_id) {
            Some(s) => s.clone(),
            None => {
                return Err(ExecutorError::StepFailed {
                    step_id: step_id.clone(),
                    message: "step id disappeared from plan after ordering".to_owned(),
                });
            }
        };

        // Skip if any declared dependency failed or was skipped.
        if step
            .depends_on
            .iter()
            .any(|dep| failed_or_skipped.contains(dep))
        {
            if let Some(sr) = run.step_runs.get_mut(step_id) {
                sr.status = StepRunStatus::Skipped;
            }
            failed_or_skipped.insert(step_id.clone());
            emit_progress(&config, &run.id, step_id, StepRunStatus::Skipped, None);
            save_run(&run, &config)?;
            continue;
        }

        let timer = std::time::Instant::now();

        let effective_timeout_secs = step.timeout_secs.or(config.timeout_secs);
        let ctx = step_runners::StepContext {
            plan: plan.clone(),
            step,
            step_outputs: step_outputs.clone(),
            catalog: config.catalog.clone(),
            global_timeout_secs: effective_timeout_secs,
            human: config.human.clone(),
            run_id: run.id.clone(),
            progress: config.progress.clone(),
            child_progress: None,
            llm_keys: config.llm_keys.clone(),
            storage_root: config.storage.root_path().to_path_buf(),
            agent_audit: Default::default(),
        };

        match run_step_with_policy(ctx, &mut run, &config).await {
            Ok(result) => {
                let finished_at = chrono::Utc::now();
                let duration_ms = timer.elapsed().as_millis() as u64;

                let final_status = StepRunStatus::Succeeded;
                let step_runners::StepResult {
                    outputs,
                    stdout,
                    stderr,
                    usage,
                    child_runs,
                } = result;

                finalize_child_runs(&mut run, child_runs);
                if let Some(sr) = run.step_runs.get_mut(step_id) {
                    sr.status = final_status.clone();
                    sr.finished_at = Some(finished_at);
                    sr.outputs = outputs.clone();
                    sr.stdout = stdout;
                    sr.stderr = stderr;
                    sr.duration_ms = Some(duration_ms);
                    sr.token_usage = usage;
                }

                step_outputs.insert_base(step_id.clone(), outputs);
                completed.insert(step_id.clone());
                emit_progress(&config, &run.id, step_id, final_status, None);
                save_run(&run, &config)?;

                // CONDITION steps route execution: mark every step of the
                // untaken branch as skipped so it — and anything that depends
                // on it — never runs.
                if let Some(StepConfig::Condition(cond)) = plan.step(step_id).map(|s| &s.config) {
                    let took_true_branch = step_outputs
                        .get(step_id)
                        .and_then(|outs| outs.get(step_runners::condition::RESULT_OUTPUT))
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let untaken = if took_true_branch {
                        &cond.false_steps
                    } else {
                        &cond.true_steps
                    };
                    let mut any_skipped = false;
                    for skip_id in untaken {
                        if completed.contains(skip_id) || failed_or_skipped.contains(skip_id) {
                            continue;
                        }
                        if let Some(sr) = run.step_runs.get_mut(skip_id) {
                            sr.status = StepRunStatus::Skipped;
                        }
                        failed_or_skipped.insert(skip_id.clone());
                        emit_progress(&config, &run.id, skip_id, StepRunStatus::Skipped, None);
                        any_skipped = true;
                    }
                    if any_skipped {
                        save_run(&run, &config)?;
                    }
                }
            }
            Err(ExecutorError::HumanResponsePending {
                step_id: pending_step_id,
            }) if pending_step_id == *step_id => {
                if let Some(sr) = run.step_runs.get_mut(step_id) {
                    sr.status = StepRunStatus::WaitingForHuman;
                }
                run.status = RunStatus::WaitingForHuman {
                    step_id: step_id.clone(),
                };
                emit_progress(
                    &config,
                    &run.id,
                    step_id,
                    StepRunStatus::WaitingForHuman,
                    None,
                );
                save_run(&run, &config)?;
                tracing::info!(
                    name: "inxm.executor.run.waiting_for_human",
                    run_id = %run.id,
                    plan_id = %plan.metadata.id,
                    step_id = %step_id,
                    "executor run waiting for human"
                );
                return Ok(run);
            }
            // A rejected approval ends the run as cancelled — an intentional
            // outcome, not a failure to diagnose or repair.
            Err(ExecutorError::RejectedByHuman { .. }) => {
                let finished_at = chrono::Utc::now();
                if let Some(sr) = run.step_runs.get_mut(step_id) {
                    sr.status = StepRunStatus::Failed;
                    sr.finished_at = Some(finished_at);
                    sr.error = Some(REJECTED_BY_OPERATOR_MESSAGE.to_owned());
                    sr.duration_ms = Some(timer.elapsed().as_millis() as u64);
                }
                emit_progress(
                    &config,
                    &run.id,
                    step_id,
                    StepRunStatus::Failed,
                    Some(REJECTED_BY_OPERATOR_MESSAGE.to_owned()),
                );
                run.status = RunStatus::Cancelled;
                run.finished_at = Some(finished_at);
                save_run(&run, &config)?;
                tracing::info!(
                    name: "inxm.executor.run.completed",
                    run_id = %run.id,
                    plan_id = %plan.metadata.id,
                    run_status = "cancelled",
                    failed_step_id = %step_id,
                    "executor run completed"
                );
                return Ok(run);
            }
            Err(e) => {
                let finished_at = chrono::Utc::now();
                let duration_ms = timer.elapsed().as_millis() as u64;
                let err_msg = e.to_string();

                if let Some(sr) = run.step_runs.get_mut(step_id) {
                    sr.status = StepRunStatus::Failed;
                    sr.finished_at = Some(finished_at);
                    sr.error = Some(err_msg.clone());
                    sr.duration_ms = Some(duration_ms);
                }

                terminalize_failed_fan_out_children(&mut run, &plan, step_id, finished_at, &config);
                run.status = RunStatus::Failed {
                    failed_step_id: step_id.clone(),
                    message: err_msg.clone(),
                };
                run.finished_at = Some(finished_at);
                failed_or_skipped.insert(step_id.clone());
                emit_progress(
                    &config,
                    &run.id,
                    step_id,
                    StepRunStatus::Failed,
                    Some(err_msg),
                );
                save_run(&run, &config)?;
                tracing::warn!(
                    name: "inxm.executor.run.completed",
                    run_id = %run.id,
                    plan_id = %plan.metadata.id,
                    run_status = "failed",
                    failed_step_id = %step_id,
                    failure_class = failure_classification(&e),
                    "executor run completed"
                );

                return Err(e);
            }
        }
    }

    // ── 5. All steps completed ────────────────────────────────────────────────
    run.outputs = resolve_plan_outputs(&plan, &step_outputs);
    run.status = RunStatus::Succeeded;
    run.finished_at = Some(chrono::Utc::now());
    save_run(&run, &config)?;
    tracing::info!(
        name: "inxm.executor.run.completed",
        run_id = %run.id,
        plan_id = %plan.metadata.id,
        run_status = "succeeded",
        run_step_count = run.step_runs.len(),
        "executor run completed"
    );

    Ok(run)
}

/// Resolve the plan's published `outputs` (each a `${step.*}` reference)
/// against the run's final step outputs, producing the values shown to the
/// user as the run's "final result".
fn resolve_plan_outputs<L: step_runners::StepOutputLookup + ?Sized>(
    plan: &Plan,
    step_outputs: &L,
) -> IndexMap<String, serde_json::Value> {
    plan.outputs
        .iter()
        .map(|output| {
            let value = step_runners::resolve_placeholders(
                &serde_json::Value::String(output.source.clone()),
                &plan.config,
                step_outputs,
            );
            (output.name.clone(), value)
        })
        .collect()
}

// ─── Private helpers ──────────────────────────────────────────────────────────

fn inject_runtime_inputs(plan: &mut Plan, inputs: &IndexMap<String, serde_json::Value>) {
    for (name, value) in inputs {
        plan.config.insert(format!("input.{name}"), value.clone());
    }
}

/// Best-effort progress notification — a closed receiver never fails a run.
fn emit_progress(
    config: &ExecutorConfig,
    run_id: &str,
    step_id: &str,
    status: StepRunStatus,
    error: Option<String>,
) {
    if let Some(tx) = &config.progress {
        let _ = tx.send(ProgressEvent {
            run_id: run_id.to_owned(),
            step_id: step_id.to_owned(),
            status,
            error,
            iteration: None,
            fan_out_progress: None,
            transcript: None,
        });
    }
}

async fn run_step_with_child_progress(
    mut ctx: step_runners::StepContext,
    run: &mut Run,
    config: &ExecutorConfig,
) -> Result<step_runners::StepResult, ExecutorError> {
    let (child_tx, mut child_rx) = tokio::sync::mpsc::unbounded_channel();
    ctx.child_progress = Some(child_tx);
    let execution = step_runners::run_step(ctx);
    tokio::pin!(execution);

    let outcome = loop {
        tokio::select! {
            result = &mut execution => break result,
            Some(event) = child_rx.recv() => {
                record_child_run(run, event);
                save_run(run, config)?;
            }
        }
    };
    while let Ok(event) = child_rx.try_recv() {
        record_child_run(run, event);
        save_run(run, config)?;
    }
    outcome
}

async fn run_step_with_policy(
    ctx: step_runners::StepContext,
    run: &mut Run,
    config: &ExecutorConfig,
) -> Result<step_runners::StepResult, ExecutorError> {
    let retry = ctx.step.retry.clone();
    let max_attempts = retry
        .as_ref()
        .map_or(1, |policy| policy.max_attempts.max(1));
    let timeout_secs = ctx.step.timeout_secs.or(config.timeout_secs);
    // TOOL_CALL adapters (subprocess/http/mcp) get the same reap grace as
    // CodeCall/AgentCall: `execute_tool` is given the step's own timeout as
    // its internal deadline (see `tools::execute_tool`), so its adapter-level
    // timeout path — which kills the child process/session and still
    // surfaces whatever stdout/stderr it captured — needs a moment to run
    // *before* this outer, adapter-agnostic wrapper gives up and drops the
    // future. Without this grace period the two deadlines fire in the same
    // instant and this outer timeout reliably wins the race, abandoning the
    // child process without killing it and discarding any diagnostic output.
    let timeout_grace_secs = if matches!(
        ctx.step.step_type(),
        crate::plan::types::StepType::CodeCall
            | crate::plan::types::StepType::AgentCall
            | crate::plan::types::StepType::ToolCall
    ) {
        PROCESS_REAP_GRACE_SECS
    } else {
        0
    };
    let step_id = ctx.step.id.clone();

    for attempt in 1..=max_attempts {
        let attempt_started = std::time::Instant::now();
        if let Some(step_run) = run.step_runs.get_mut(&step_id) {
            step_run.status = StepRunStatus::Running;
            step_run.started_at.get_or_insert_with(chrono::Utc::now);
            step_run.attempt = attempt;
            step_run.error = None;
        }
        emit_progress(config, &run.id, &step_id, StepRunStatus::Running, None);
        save_run(run, config)?;

        let step_span = tracing::info_span!(
            "inxm.executor.step",
            run_id = %run.id,
            plan_id = %ctx.plan.metadata.id,
            plan_version = ctx.plan.metadata.version,
            step_id = %step_id,
            step_kind = step_kind(&ctx.step),
            step_attempt = attempt,
            step_max_attempts = max_attempts,
            step_timeout_secs = timeout_secs.unwrap_or_default(),
            step_timeout_configured = timeout_secs.is_some()
        );
        let execution =
            run_step_with_child_progress(ctx.clone(), run, config).instrument(step_span);
        let (outcome, timed_out) = match timeout_secs {
            Some(secs) => match tokio::time::timeout(
                Duration::from_secs(secs.saturating_add(timeout_grace_secs)),
                execution,
            )
            .await
            {
                Ok(outcome) => (outcome, false),
                Err(_) => (
                    Err(ExecutorError::StepFailed {
                        step_id: step_id.clone(),
                        message: format!("step timed out after {secs}s"),
                    }),
                    true,
                ),
            },
            None => (execution.await, false),
        };
        let attempt_duration_ms = attempt_started.elapsed().as_millis() as u64;

        match outcome {
            Ok(result) => {
                tracing::info!(
                    name: "inxm.executor.step.attempt_completed",
                    run_id = %run.id,
                    plan_id = %ctx.plan.metadata.id,
                    step_id = %step_id,
                    step_kind = step_kind(&ctx.step),
                    step_attempt = attempt,
                    step_duration_ms = attempt_duration_ms,
                    step_outcome = "succeeded",
                    "executor step attempt completed"
                );
                return Ok(result);
            }
            Err(error @ ExecutorError::HumanResponsePending { .. })
            | Err(error @ ExecutorError::RejectedByHuman { .. }) => {
                tracing::info!(
                    name: "inxm.executor.step.attempt_completed",
                    run_id = %run.id,
                    plan_id = %ctx.plan.metadata.id,
                    step_id = %step_id,
                    step_kind = step_kind(&ctx.step),
                    step_attempt = attempt,
                    step_duration_ms = attempt_duration_ms,
                    step_outcome = "human_control",
                    failure_class = failure_classification(&error),
                    "executor step attempt completed"
                );
                return Err(error);
            }
            Err(error) if attempt < max_attempts => {
                tracing::warn!(
                    name: "inxm.executor.step.retry_scheduled",
                    run_id = %run.id,
                    plan_id = %ctx.plan.metadata.id,
                    step_id = %step_id,
                    step_kind = step_kind(&ctx.step),
                    step_attempt = attempt,
                    step_max_attempts = max_attempts,
                    step_duration_ms = attempt_duration_ms,
                    step_timed_out = timed_out,
                    failure_class = failure_classification(&error),
                    retry_delay_ms = retry.as_ref().map_or(0, |policy| retry_delay(policy, attempt).as_millis() as u64),
                    "executor step retry scheduled"
                );
                let error_message =
                    format!("attempt {attempt}/{max_attempts} failed: {error}; retrying");
                if let Some(step_run) = run.step_runs.get_mut(&step_id) {
                    step_run.error = Some(error_message.clone());
                }
                emit_progress(
                    config,
                    &run.id,
                    &step_id,
                    StepRunStatus::Running,
                    Some(error_message),
                );
                save_run(run, config)?;
                if let Some(policy) = &retry {
                    tokio::time::sleep(retry_delay(policy, attempt)).await;
                }
            }
            Err(error) => {
                let (stdout, stderr) = ctx
                    .agent_audit
                    .lock()
                    .expect("agent audit mutex poisoned")
                    .clone();
                if let Some(step_run) = run.step_runs.get_mut(&step_id) {
                    if !stdout.is_empty() {
                        step_run.stdout = Some(stdout);
                    }
                    if !stderr.is_empty() {
                        step_run.stderr = Some(stderr);
                    }
                }
                tracing::warn!(
                    name: "inxm.executor.step.attempt_completed",
                    run_id = %run.id,
                    plan_id = %ctx.plan.metadata.id,
                    step_id = %step_id,
                    step_kind = step_kind(&ctx.step),
                    step_attempt = attempt,
                    step_duration_ms = attempt_duration_ms,
                    step_timed_out = timed_out,
                    step_outcome = "failed",
                    failure_class = failure_classification(&error),
                    "executor step attempt completed"
                );
                return Err(error);
            }
        }
    }

    unreachable!("max_attempts is always at least one")
}

fn step_kind(step: &crate::plan::types::PlanStep) -> &'static str {
    use crate::plan::types::StepType;
    match step.step_type() {
        StepType::ToolCall => "tool_call",
        StepType::CodeCall => "code_call",
        StepType::HumanInteraction => "human_interaction",
        StepType::FanOut => "fan_out",
        StepType::FanIn => "fan_in",
        StepType::PromptCall => "prompt_call",
        StepType::Condition => "condition",
        StepType::AgentCall => "agent_call",
    }
}

fn failure_classification(error: &ExecutorError) -> &'static str {
    match error {
        ExecutorError::RunNotFound { .. } => "run_not_found",
        ExecutorError::StepFailed { .. } => "step_failed",
        ExecutorError::RejectedByHuman { .. } => "human_rejected",
        ExecutorError::HumanResponsePending { .. } => "human_response_pending",
        ExecutorError::InvalidResume { .. } => "invalid_resume",
        ExecutorError::Tool(_) => "tool",
        ExecutorError::Plan(_) => "plan",
        ExecutorError::Io(_) => "io",
        ExecutorError::Storage(_) => "storage",
    }
}

fn retry_delay(policy: &crate::plan::types::RetryPolicy, failed_attempt: u32) -> Duration {
    let multiplier = if policy.backoff {
        1_u64 << failed_attempt.saturating_sub(1).min(MAX_BACKOFF_SHIFT)
    } else {
        1
    };
    Duration::from_secs(policy.delay_secs.saturating_mul(multiplier))
}

fn record_child_run(run: &mut Run, event: step_runners::ChildRunEvent) {
    let Some(step_run) = run.step_runs.get_mut(&event.step_id) else {
        return;
    };
    step_run.status = event.status;
    if let Some(error) = &event.run.error {
        step_run.error = Some(error.clone());
    }
    step_run.started_at.get_or_insert(event.run.started_at);
    step_run.finished_at = Some(event.run.finished_at);
    step_run.duration_ms = Some(
        step_run
            .duration_ms
            .unwrap_or_default()
            .saturating_add(event.run.duration_ms),
    );
    step_run.iterations.push(event.run);
    step_run.attempt = step_run.iterations.len() as u32;
}

fn terminalize_failed_fan_out_children(
    run: &mut Run,
    plan: &Plan,
    owner_id: &str,
    finished_at: chrono::DateTime<chrono::Utc>,
    config: &ExecutorConfig,
) {
    let mut pending = vec![owner_id.to_owned()];
    let mut seen = HashSet::new();
    while let Some(step_id) = pending.pop() {
        if !seen.insert(step_id.clone()) {
            continue;
        }
        let Some(StepConfig::FanOut(fan_out)) = plan.step(&step_id).map(|step| &step.config) else {
            continue;
        };
        for child_id in &fan_out.spawn_steps {
            pending.push(child_id.clone());
            let Some(step_run) = run.step_runs.get_mut(child_id) else {
                continue;
            };
            if matches!(
                step_run.status,
                StepRunStatus::Pending | StepRunStatus::Running
            ) {
                step_run.status = StepRunStatus::Cancelled;
                step_run.finished_at = Some(finished_at);
                emit_progress(config, &run.id, child_id, StepRunStatus::Cancelled, None);
            }
        }
    }
}

fn finalize_child_runs(run: &mut Run, child_runs: IndexMap<String, Vec<StepRunIteration>>) {
    for (step_id, returned_runs) in child_runs {
        let Some(step_run) = run.step_runs.get_mut(&step_id) else {
            continue;
        };
        let recorded_runs = if step_run.iterations.is_empty() {
            &returned_runs
        } else {
            &step_run.iterations
        };
        if recorded_runs.is_empty()
            || recorded_runs
                .iter()
                .all(|run| run.status == StepRunStatus::Skipped)
        {
            step_run.status = StepRunStatus::Skipped;
            step_run.duration_ms = Some(0);
        } else {
            step_run.status = StepRunStatus::Succeeded;
        }
    }
}

fn save_run(run: &Run, config: &ExecutorConfig) -> Result<(), ExecutorError> {
    config
        .storage
        .runs()
        .save(run)
        .map_err(|e| ExecutorError::Storage(e.to_string()))
}

#[cfg(test)]
mod execution_policy_tests {
    use super::*;
    use crate::plan::types::{
        CodeCallConfig, ConditionConfig, FanOutConfig, HumanInteractionConfig, PlanInput,
        PlanMetadata, PlanStep, RetryPolicy,
    };

    fn plan_with_step(step: PlanStep) -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: "executor policy test".to_owned(),
            description: None,
            inputs: vec![],
            config: IndexMap::new(),
            steps: vec![step],
            outputs: vec![],
        }
    }

    fn config(
        storage: Arc<StorageRoot>,
        progress: Option<tokio::sync::mpsc::UnboundedSender<ProgressEvent>>,
        human: Option<tokio::sync::mpsc::UnboundedSender<HumanRequest>>,
    ) -> ExecutorConfig {
        ExecutorConfig {
            inputs: IndexMap::new(),
            timeout_secs: None,
            storage,
            catalog: ToolCatalog::default(),
            progress,
            human,
            llm_keys: LlmKeys::default(),
            source: None,
        }
    }

    fn retry_policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            delay_secs: 0,
            backoff: false,
        }
    }

    fn assert_exact_step_lifecycle(run: &Run, expected_step_ids: &[&str]) {
        let actual: HashSet<&str> = run.step_runs.keys().map(String::as_str).collect();
        let expected: HashSet<&str> = expected_step_ids.iter().copied().collect();
        assert_eq!(actual, expected);
        assert_eq!(run.step_runs.len(), expected_step_ids.len());
    }

    fn input_condition_step(id: &str, expression: &str, depends_on: Vec<&str>) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            config: StepConfig::Condition(ConditionConfig {
                expression: expression.to_owned(),
                true_steps: vec![],
                false_steps: vec![],
            }),
            depends_on: depends_on.into_iter().map(str::to_owned).collect(),
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        }
    }

    fn repair_input_plan(steps: Vec<PlanStep>) -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: "repair input test".to_owned(),
            description: None,
            inputs: vec![PlanInput {
                name: "value".to_owned(),
                description: None,
                value_type: "string".to_owned(),
                required: true,
                default: None,
                input_kind: Default::default(),
            }],
            config: IndexMap::new(),
            steps,
            outputs: vec![],
        }
    }

    #[test]
    fn fan_child_skipped_in_every_iteration_stays_skipped_after_finalization() {
        let mut run = Run::new("plan", 1);
        run.step_runs
            .insert("repair".to_owned(), StepRun::new("repair"));
        let now = chrono::Utc::now();
        let skipped = StepRunIteration {
            iteration: 0,
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

        finalize_child_runs(
            &mut run,
            IndexMap::from([("repair".to_owned(), vec![skipped])]),
        );

        assert_eq!(run.step_runs["repair"].status, StepRunStatus::Skipped);
    }

    #[tokio::test]
    async fn configured_retry_persists_each_attempt_until_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
        let counter = dir.path().join("attempts");
        let script = format!(
            "count=0; test ! -f '{}' || count=$(cat '{}'); \
             count=$((count + 1)); printf '%s' \"$count\" > '{}'; \
             test \"$count\" -ge 3 || exit 1; printf '{{\"ok\":true}}'",
            counter.display(),
            counter.display(),
            counter.display()
        );
        let step = PlanStep {
            id: "retrying".to_owned(),
            name: "retrying".to_owned(),
            description: None,
            config: StepConfig::CodeCall(CodeCallConfig {
                language: "bash".to_owned(),
                inline: Some(script),
                file: None,
                args: vec![],
                stdin: None,
                env: IndexMap::new(),
                working_dir: None,
                timeout_secs: None,
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: Some(retry_policy(3)),
        };

        let run = execute(plan_with_step(step), config(storage.clone(), None, None))
            .await
            .expect("third attempt should succeed");

        assert_eq!(run.step_runs["retrying"].attempt, 3);
        assert_eq!(run.step_runs["retrying"].status, StepRunStatus::Succeeded);
        assert_exact_step_lifecycle(&run, &["retrying"]);
        assert_eq!(
            std::fs::read_to_string(counter).expect("attempt count"),
            "3"
        );
    }

    #[tokio::test]
    async fn whole_step_timeout_is_retried_and_stops_at_max_attempts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let (human_tx, _human_rx) = tokio::sync::mpsc::unbounded_channel();
        let step = PlanStep {
            id: "wait".to_owned(),
            name: "wait".to_owned(),
            description: None,
            config: StepConfig::HumanInteraction(HumanInteractionConfig {
                prompt: "wait forever".to_owned(),
                response_field: "response".to_owned(),
                approval_required: false,
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: Some(1),
            retry: Some(retry_policy(2)),
        };

        let result = execute(
            plan_with_step(step),
            config(storage.clone(), Some(progress_tx), Some(human_tx)),
        )
        .await;

        assert!(result.is_err(), "timeout should be terminal after retry");
        let run_id = progress_rx
            .try_recv()
            .expect("running progress event")
            .run_id;
        let persisted = storage.runs().load(&run_id).expect("persisted failed run");
        assert_eq!(persisted.step_runs["wait"].attempt, 2);
        assert!(
            persisted.step_runs["wait"]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("timed out after 1s"))
        );
    }

    #[tokio::test]
    async fn human_rejection_is_never_retried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
        let (human_tx, mut human_rx) = tokio::sync::mpsc::unbounded_channel();
        let step = PlanStep {
            id: "approve".to_owned(),
            name: "approve".to_owned(),
            description: None,
            config: StepConfig::HumanInteraction(HumanInteractionConfig {
                prompt: "approve?".to_owned(),
                response_field: "response".to_owned(),
                approval_required: true,
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: Some(retry_policy(3)),
        };
        let plan = plan_with_step(step);
        let execution = tokio::spawn(execute(
            plan.clone(),
            config(storage.clone(), None, Some(human_tx)),
        ));
        let request = human_rx.recv().await.expect("human request");
        request
            .respond
            .send(HumanDecision::Reject)
            .expect("executor should await response");

        let run = execution
            .await
            .expect("execution task")
            .expect("rejection is a cancelled run");
        assert_eq!(run.status, RunStatus::Cancelled);
        assert_eq!(run.step_runs["approve"].attempt, 1);
    }

    #[tokio::test]
    async fn pending_human_response_is_never_retried() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
        let (human_tx, mut human_rx) = tokio::sync::mpsc::unbounded_channel();
        let step = PlanStep {
            id: "answer".to_owned(),
            name: "answer".to_owned(),
            description: None,
            config: StepConfig::HumanInteraction(HumanInteractionConfig {
                prompt: "answer?".to_owned(),
                response_field: "response".to_owned(),
                approval_required: false,
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: Some(retry_policy(3)),
        };
        let plan = plan_with_step(step);
        let execution = tokio::spawn(execute(
            plan.clone(),
            config(storage.clone(), None, Some(human_tx)),
        ));
        let request = human_rx.recv().await.expect("human request");
        drop(request);

        let run = execution
            .await
            .expect("execution task")
            .expect("pending response should pause the run");
        assert!(matches!(run.status, RunStatus::WaitingForHuman { .. }));
        assert_eq!(run.step_runs["answer"].attempt, 1);
        assert_exact_step_lifecycle(&run, &["answer"]);

        let (resume_human_tx, mut resume_human_rx) = tokio::sync::mpsc::unbounded_channel();
        let resumed_execution = tokio::spawn(resume(
            plan,
            config(storage, None, Some(resume_human_tx)),
            run,
        ));
        let request = resume_human_rx.recv().await.expect("resumed human request");
        request
            .respond
            .send(HumanDecision::Text("done".to_owned()))
            .expect("resumed executor should await response");
        let resumed = resumed_execution
            .await
            .expect("resume task")
            .expect("resume should complete");
        assert_exact_step_lifecycle(&resumed, &["answer"]);
    }

    #[tokio::test]
    async fn repair_resume_initializes_a_renamed_step_and_rejects_wrong_plan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
        let mut run = Run::new("plan-a", 1);
        run.status = RunStatus::Failed {
            failed_step_id: "old".to_owned(),
            message: "failed".to_owned(),
        };
        let mut failed = StepRun::new("old");
        failed.status = StepRunStatus::Failed;
        run.step_runs.insert("old".to_owned(), failed);

        let replacement = PlanStep {
            id: "new".to_owned(),
            name: "new".to_owned(),
            description: None,
            config: StepConfig::Condition(ConditionConfig {
                expression: "true".to_owned(),
                true_steps: vec![],
                false_steps: vec![],
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let mut repaired_plan = plan_with_step(replacement);
        repaired_plan.metadata.id = "plan-a".to_owned();
        repaired_plan.metadata.version = 2;

        let resumed = resume_from_repair(
            repaired_plan.clone(),
            config(storage.clone(), None, None),
            run.clone(),
            IndexMap::new(),
            RepairResumeMode::PatchedPlan,
        )
        .await
        .expect("renamed replacement should execute");

        assert_eq!(resumed.step_runs.len(), 1);
        assert_eq!(resumed.step_runs["new"].status, StepRunStatus::Succeeded);
        assert_exact_step_lifecycle(&resumed, &["new"]);

        repaired_plan.metadata.id = "plan-b".to_owned();
        let error = resume_from_repair(
            repaired_plan,
            config(storage, None, None),
            run,
            IndexMap::new(),
            RepairResumeMode::PatchedPlan,
        )
        .await
        .expect_err("repair must retain plan identity");
        assert!(matches!(error, ExecutorError::InvalidResume { .. }));
    }

    #[test]
    fn repair_reset_includes_bodies_and_downstream_of_a_renamed_fan_out() {
        let replacement = PlanStep {
            id: "new-fan-out".to_owned(),
            name: "new fan-out".to_owned(),
            description: None,
            config: StepConfig::FanOut(FanOutConfig {
                over: "source.items".to_owned(),
                item_var: "item".to_owned(),
                spawn_steps: vec!["body".to_owned()],
                until: None,
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let body = input_condition_step("body", "true", vec![]);
        let downstream = input_condition_step("downstream", "true", vec!["new-fan-out"]);
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "renamed fan-out".to_owned(),
            description: None,
            inputs: vec![],
            config: IndexMap::new(),
            steps: vec![replacement, body, downstream],
            outputs: vec![],
        };
        let mut run = Run::new(plan.metadata.id.clone(), plan.metadata.version);
        let mut failed = StepRun::new("old-fan-out");
        failed.status = StepRunStatus::Failed;
        run.step_runs.insert("old-fan-out".to_owned(), failed);
        for step_id in ["body", "downstream"] {
            let mut completed = StepRun::new(step_id);
            completed.status = StepRunStatus::Succeeded;
            run.step_runs.insert(step_id.to_owned(), completed);
        }

        assert_eq!(
            repair_reset_step_ids(&plan, &run, "old-fan-out"),
            ["new-fan-out", "body", "downstream"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
    }

    #[tokio::test]
    async fn world_fixed_resume_reexecutes_the_failed_step_at_the_same_plan_version() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
        let step = PlanStep {
            id: "check".to_owned(),
            name: "check".to_owned(),
            description: None,
            config: StepConfig::Condition(ConditionConfig {
                expression: "true".to_owned(),
                true_steps: vec![],
                false_steps: vec![],
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let mut plan = plan_with_step(step);
        plan.metadata.id = "plan-a".to_owned();
        plan.metadata.version = 1;
        let mut run = Run::new("plan-a", 1);
        run.status = RunStatus::Failed {
            failed_step_id: "check".to_owned(),
            message: "world was broken".to_owned(),
        };
        let mut failed = StepRun::new("check");
        failed.status = StepRunStatus::Failed;
        run.step_runs.insert("check".to_owned(), failed);

        // Same plan version: only the world-fixed mode may resume it.
        let error = resume_from_repair(
            plan.clone(),
            config(storage.clone(), None, None),
            run.clone(),
            IndexMap::new(),
            RepairResumeMode::PatchedPlan,
        )
        .await
        .expect_err("same-version resume must be rejected without a world fix");
        assert!(matches!(error, ExecutorError::InvalidResume { .. }));

        let resumed = resume_from_repair(
            plan.clone(),
            config(storage.clone(), None, None),
            run.clone(),
            IndexMap::new(),
            RepairResumeMode::WorldFixed,
        )
        .await
        .expect("world-fixed resume should re-run the identical step");
        assert_eq!(resumed.step_runs["check"].status, StepRunStatus::Succeeded);

        // Conversely, a world-fixed resume must reject a drifted plan version.
        plan.metadata.version = 2;
        let error = resume_from_repair(
            plan,
            config(storage, None, None),
            run,
            IndexMap::new(),
            RepairResumeMode::WorldFixed,
        )
        .await
        .expect_err("world-fixed resume must run against the unchanged plan version");
        assert!(matches!(error, ExecutorError::InvalidResume { .. }));
    }

    #[tokio::test]
    async fn repair_resume_rebases_a_changed_input_used_only_by_the_failed_step() {
        let dir = tempfile::tempdir().expect("tempdir");
        let storage = Arc::new(StorageRoot::open(dir.path()).expect("storage"));
        let mut plan = repair_input_plan(vec![input_condition_step(
            "retry",
            "${input.value} == new",
            vec![],
        )]);
        plan.metadata.id = "plan-a".to_owned();
        plan.metadata.version = 2;

        let mut run = Run::new("plan-a", 1);
        run.status = RunStatus::Failed {
            failed_step_id: "retry".to_owned(),
            message: "old value failed".to_owned(),
        };
        run.inputs
            .insert("value".to_owned(), serde_json::json!("old"));
        let mut failed = StepRun::new("retry");
        failed.status = StepRunStatus::Failed;
        run.step_runs.insert("retry".to_owned(), failed);

        let resumed = resume_from_repair(
            plan,
            config(storage.clone(), None, None),
            run,
            IndexMap::from([("value".to_owned(), serde_json::json!("new"))]),
            RepairResumeMode::PatchedPlan,
        )
        .await
        .expect("failed-only input may be replaced");

        assert_eq!(resumed.status, RunStatus::Succeeded);
        assert_eq!(resumed.inputs["value"], serde_json::json!("new"));
        let persisted = storage.runs().load(&resumed.id).expect("persisted run");
        assert_eq!(persisted.inputs["value"], serde_json::json!("new"));
    }

    #[test]
    fn repair_resume_rejects_an_override_used_by_a_succeeded_step_or_iteration() {
        let plan = repair_input_plan(vec![
            input_condition_step("completed", "${input.value} == old", vec![]),
            input_condition_step("fan_child", "${input.value} == old", vec![]),
            input_condition_step("retry", "${input.value} == new", vec![]),
        ]);
        let mut run = Run::new(plan.metadata.id.clone(), plan.metadata.version);
        run.status = RunStatus::Failed {
            failed_step_id: "retry".to_owned(),
            message: "failed".to_owned(),
        };
        run.inputs
            .insert("value".to_owned(), serde_json::json!("old"));

        let mut completed = StepRun::new("completed");
        completed.status = StepRunStatus::Succeeded;
        run.step_runs.insert("completed".to_owned(), completed);
        let mut retry = StepRun::new("retry");
        retry.status = StepRunStatus::Failed;
        run.step_runs.insert("retry".to_owned(), retry);

        let error = rebase_repair_inputs(
            &plan,
            &run,
            &IndexMap::from([("value".to_owned(), serde_json::json!("new"))]),
            &dag::downstream_closure(&plan, "retry"),
        )
        .expect_err("succeeded checkpoint must protect its input");
        assert!(matches!(error, ExecutorError::InvalidResume { .. }));

        run.step_runs["completed"].status = StepRunStatus::Skipped;
        let now = chrono::Utc::now();
        run.step_runs.insert(
            "fan_child".to_owned(),
            StepRun {
                iterations: vec![StepRunIteration {
                    iteration: 0,
                    status: StepRunStatus::Succeeded,
                    started_at: now,
                    finished_at: now,
                    duration_ms: 0,
                    outputs: IndexMap::new(),
                    stdout: None,
                    stderr: None,
                    error: None,
                    token_usage: None,
                }],
                ..StepRun::new("fan_child")
            },
        );
        let error = rebase_repair_inputs(
            &plan,
            &run,
            &IndexMap::from([("value".to_owned(), serde_json::json!("new"))]),
            &HashSet::from(["fan_child".to_owned()]),
        )
        .expect_err("succeeded fan-out iteration must protect its input even when reset");
        assert!(matches!(error, ExecutorError::InvalidResume { .. }));
    }

    #[test]
    fn repair_input_rebase_discards_removed_values_and_validates_the_repaired_contract() {
        let mut plan = repair_input_plan(vec![]);
        plan.inputs = vec![PlanInput {
            name: "introduced".to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: true,
            default: Some(serde_json::json!("default")),
            input_kind: Default::default(),
        }];
        let mut run = Run::new(plan.metadata.id.clone(), plan.metadata.version);
        run.inputs
            .insert("removed".to_owned(), serde_json::json!("discard me"));

        let inputs = rebase_repair_inputs(&plan, &run, &IndexMap::new(), &HashSet::new())
            .expect("introduced default should satisfy repaired contract");
        assert_eq!(
            inputs,
            IndexMap::from([("introduced".to_owned(), serde_json::json!("default"))])
        );

        let error = rebase_repair_inputs(
            &plan,
            &run,
            &IndexMap::from([("unknown".to_owned(), serde_json::json!("value"))]),
            &HashSet::new(),
        )
        .expect_err("unknown overrides must be rejected");
        assert!(matches!(error, ExecutorError::Plan(_)));

        let error = rebase_repair_inputs(
            &plan,
            &run,
            &IndexMap::from([("introduced".to_owned(), serde_json::json!(7))]),
            &HashSet::new(),
        )
        .expect_err("wrongly typed overrides must be rejected");
        assert!(matches!(error, ExecutorError::Plan(_)));
    }

    #[test]
    fn exponential_retry_delay_is_bounded_and_saturating() {
        let policy = RetryPolicy {
            max_attempts: u32::MAX,
            delay_secs: u64::MAX,
            backoff: true,
        };
        assert_eq!(retry_delay(&policy, u32::MAX).as_secs(), u64::MAX);
    }
}
