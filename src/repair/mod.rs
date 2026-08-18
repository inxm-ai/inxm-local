//! Repair loop — propose and apply patches for failed runs.
//!
//! Design contract:
//! - `propose_repair` is the only repair-module entry point that invokes the
//!   compiler backend. The backend may use multiple bounded model calls (for
//!   example strategy then patch implementation) but still returns one typed
//!   `Patch` for review.
//! - `apply_patch` is purely deterministic: it applies the approved operation,
//!   re-validates, and persists the new plan version.
//! - Human approval lives in a separate CLI command; this module only proposes.

pub mod classifier;
pub mod failure_packet;
pub mod patch;

pub use classifier::ErrorKind;
pub use failure_packet::FailurePacket;

pub use crate::compiler::backend::RepairProposal;

use crate::compiler::backend::{Backend, RepairRequest};
use crate::error::RepairError;
use crate::plan::normalization::normalize;
use crate::plan::types::Plan;
use crate::storage::StorageRoot;
use crate::storage::patches::{Patch, PatchStatus};
use crate::storage::runs::{Run, RunStatus, StepRun, StepRunStatus};
use crate::tools::catalog::ToolCatalog;
use crate::validator;
use std::time::{Duration, Instant};
use tracing::Instrument as _;

const OUTCOME_SUCCESS: &str = "success";
const OUTCOME_INVALID_PATCH: &str = "invalid_patch";
const OUTCOME_WORLD_FIX: &str = "world_fix";

#[derive(Default)]
struct RepairTelemetry {
    correction_attempt_count: u32,
    validation_error_count: usize,
    outcome: Option<&'static str>,
}

fn elapsed_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn error_kind_name(error_kind: &ErrorKind) -> &'static str {
    match error_kind {
        ErrorKind::ToolNotFound => "tool_not_found",
        ErrorKind::ToolExecutionFailed => "tool_execution_failed",
        ErrorKind::CodeExecutionFailed => "code_execution_failed",
        ErrorKind::MissingInterpreter => "missing_interpreter",
        ErrorKind::TimeoutExceeded => "timeout_exceeded",
        ErrorKind::NetworkError => "network_error",
        ErrorKind::ExternalEndpointDown => "external_endpoint_down",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::OutputSchemaViolation => "output_schema_violation",
        ErrorKind::PromptCallFailed => "prompt_call_failed",
        ErrorKind::Unknown => "unknown",
    }
}

fn default_error_outcome(error: &RepairError) -> &'static str {
    match error {
        RepairError::RunNotFailed => "conflict_run_not_failed",
        RepairError::PatchNotApproved { .. } => "conflict_patch_not_approved",
        RepairError::PatchInvalid(_) => OUTCOME_INVALID_PATCH,
        RepairError::Compiler(_) => "compiler_error",
        RepairError::Io(_) => "io_error",
        RepairError::Storage(message) if message.contains("stale repair") => "conflict_stale_plan",
        RepairError::Storage(_) => "storage_error",
        RepairError::RunNotFound { .. } => "run_not_found",
    }
}

fn finish_repair_span(
    span: &tracing::Span,
    started_at: Instant,
    result: &Result<impl Sized, RepairError>,
    telemetry: &RepairTelemetry,
) {
    let outcome = telemetry.outcome.unwrap_or_else(|| match result {
        Ok(_) => OUTCOME_SUCCESS,
        Err(error) => default_error_outcome(error),
    });
    let duration_ms = elapsed_millis(started_at.elapsed());
    span.record(
        "correction_attempt_count",
        telemetry.correction_attempt_count,
    );
    span.record(
        "validation_error_count",
        u64::try_from(telemetry.validation_error_count).unwrap_or(u64::MAX),
    );
    span.record("duration_ms", duration_ms);
    span.record("outcome", outcome);
    tracing::info!(
        parent: span,
        outcome,
        duration_ms,
        correction_attempt_count = telemetry.correction_attempt_count,
        validation_error_count = telemetry.validation_error_count,
        "repair operation completed"
    );
}

// ─── propose_repair ───────────────────────────────────────────────────────────

/// Analyse a failed run, ask the compiler backend for a repair, and persist
/// the proposal for human review.
///
/// The diagnosis distinguishes two failure causes:
/// - the plan is wrong → a `Patch` with status `Pending`; use `apply_patch`
///   after the human has approved it via the CLI, then resume against the
///   new plan version.
/// - the world is wrong (the plan was reasonable, but runtime state violated
///   its assumptions — e.g. a commit step with nothing to commit) → a
///   `WorldFix` listing environment remediation actions for the human. The
///   plan is never modified; once the world is fixed, the persisted record
///   authorises resuming the run against the SAME plan version.
pub async fn propose_repair(
    run: &Run,
    plan: &Plan,
    backend: &Backend,
    catalog: &ToolCatalog,
    storage: &StorageRoot,
    extra_context: Option<String>,
) -> Result<RepairProposal, RepairError> {
    let next_version = u64::from(plan.metadata.version) + 1;
    let span = tracing::info_span!(
        "inxm.repair.propose",
        run_id = %run.id,
        plan_id = %plan.metadata.id,
        base_version = plan.metadata.version,
        next_version,
        patch_id = tracing::field::Empty,
        failing_step_id = tracing::field::Empty,
        error_kind = tracing::field::Empty,
        correction_attempt_count = tracing::field::Empty,
        validation_error_count = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
    );
    let started_at = Instant::now();
    let mut telemetry = RepairTelemetry::default();
    let result = propose_repair_inner(
        run,
        plan,
        backend,
        catalog,
        storage,
        extra_context,
        &span,
        &mut telemetry,
    )
    .instrument(span.clone())
    .await;
    if let Ok(RepairProposal::Patch(patch)) = &result {
        span.record("patch_id", patch.id.as_str());
    }
    finish_repair_span(&span, started_at, &result, &telemetry);
    result
}

#[allow(clippy::too_many_arguments)]
async fn propose_repair_inner(
    run: &Run,
    plan: &Plan,
    backend: &Backend,
    catalog: &ToolCatalog,
    storage: &StorageRoot,
    extra_context: Option<String>,
    span: &tracing::Span,
    telemetry: &mut RepairTelemetry,
) -> Result<RepairProposal, RepairError> {
    // 1. Resolve the exact failure recorded by RunStatus. Insertion order is
    // not an execution contract, and a recovered run may contain more than one
    // failed StepRun.
    let step_run = failed_step_for_repair(run)?;
    span.record("failing_step_id", step_run.step_id.as_str());

    // 3. Find the matching plan step definition.
    let failing_step = plan.step(&step_run.step_id).ok_or_else(|| {
        RepairError::Storage(format!(
            "failed step '{}' not found in plan '{}'",
            step_run.step_id, plan.metadata.id
        ))
    })?;

    // 4. Build the failure packet (full runtime context snapshot). The
    //    compiler backend receives the error classification via the enriched
    //    `error_message` text: when `packet.error_kind` is
    //    `ExternalEndpointDown`, `FailurePacket::build` has already appended
    //    explicit "substitute the endpoint, don't resume" guidance, so the
    //    message flows into the `RepairRequest` below unmodified.
    let packet = FailurePacket::build(run, failing_step, step_run);
    let classified_error_kind = error_kind_name(&packet.error_kind);
    span.record("error_kind", classified_error_kind);
    tracing::info!(
        parent: span,
        error_kind = classified_error_kind,
        failing_step_id = %step_run.step_id,
        "repair failure classified"
    );

    // 4b. Convergence guard: when earlier applied patches already targeted
    //     this exact failing step + error message and the failure recurred
    //     unchanged, the previously edited field is proven not to be the root
    //     cause. Say so explicitly — otherwise the backend keeps proposing
    //     variations of the same edit forever (observed on plan 423d8933:
    //     four applied patches, identical failure every run).
    let prior_repair_count = applied_repairs_for_recurring_failure(storage, run);
    let error_message = if prior_repair_count == 0 {
        packet.error_message.clone()
    } else {
        tracing::info!(
            parent: span,
            prior_repair_count,
            failing_step_id = %step_run.step_id,
            "failure recurred unchanged after applied repair(s); escalating guidance"
        );
        format!(
            "{}{}",
            packet.error_message,
            failure_packet::repeated_failure_guidance(prior_repair_count)
        )
    };

    // 5. Build the repair request for the compiler backend.
    let req = RepairRequest {
        plan: plan.clone(),
        run_id: run.id.clone(),
        failing_step_id: step_run.step_id.clone(),
        error_message,
        stdout: packet.stdout.clone(),
        stderr: packet.stderr.clone(),
        runtime_inputs: packet.runtime_inputs.clone(),
        dependency_outputs: packet.dependency_outputs.clone(),
        tool_catalog: catalog
            .all()
            .filter(|tool| tool.allowlisted)
            .cloned()
            .collect(),
        extra_context,
    };

    // 6. Ask for a candidate. A world-locus diagnosis carries no plan
    //    mutation, so there is nothing to preflight against the plan
    //    validator — persist it directly as the human-facing remediation
    //    record that also authorises a same-version resume.
    let proposed = match backend.propose_repair(&req).await? {
        RepairProposal::WorldFix(world_fix) => {
            telemetry.outcome = Some(OUTCOME_WORLD_FIX);
            tracing::info!(
                parent: span,
                world_fix_id = %world_fix.id,
                remediation_action_count = world_fix.remediation.len(),
                "repair diagnosed a world-state failure; plan left unchanged"
            );
            storage
                .world_fixes()
                .save(&world_fix)
                .map_err(|error| RepairError::Storage(error.to_string()))?;
            return Ok(RepairProposal::WorldFix(world_fix));
        }
        RepairProposal::Patch(patch) => patch,
    };

    // 7. Deterministically prove that a patch candidate can be applied and
    //    leaves a valid plan before exposing it for human review.
    span.record("patch_id", proposed.id.as_str());
    match persist_validated_candidate_observed(&proposed, plan, catalog, storage, span, telemetry) {
        Ok(()) => Ok(RepairProposal::Patch(proposed)),
        Err(RepairError::PatchInvalid(initial_errors)) => {
            // One bounded correction call gives the compiler the exact
            // applicator/validator feedback. Never persist either invalid draft.
            telemetry.correction_attempt_count = 1;
            let corrected = backend
                .correct_patch(&req, &proposed, &initial_errors)
                .await?;
            span.record("patch_id", corrected.id.as_str());
            persist_validated_candidate_observed(
                &corrected,
                plan,
                catalog,
                storage,
                span,
                telemetry,
            )
            .map_err(|error| match error {
                RepairError::PatchInvalid(corrected_errors) => RepairError::PatchInvalid(format!(
                    "corrected patch remained invalid: {corrected_errors}; initial candidate: {initial_errors}"
                )),
                other => other,
            })?;
            Ok(RepairProposal::Patch(Box::new(corrected)))
        }
        Err(other) => Err(other),
    }
}

/// Count earlier *applied* patches for the same plan whose motivating run
/// failed on the same step with the exact same error message as `run`.
///
/// A non-zero count means at least one repair was accepted, applied, and made
/// no difference — the strongest signal available that the edited field is
/// not the root cause. Message matching is deliberately exact: two failures
/// that differ at all may legitimately deserve similar patches, and a false
/// positive here would wrongly steer repair away from a correct fix.
/// Storage errors count as zero — the guard only ever adds guidance.
fn applied_repairs_for_recurring_failure(storage: &StorageRoot, run: &Run) -> usize {
    let RunStatus::Failed {
        failed_step_id,
        message,
    } = &run.status
    else {
        return 0;
    };
    let Ok(patches) = storage.patches().list() else {
        return 0;
    };
    patches
        .iter()
        .filter(|patch| {
            patch.plan_id == run.plan_id
                && patch.status == PatchStatus::Applied
                && patch.failing_step_id == *failed_step_id
                && patch.run_id != run.id
                && storage.runs().load(&patch.run_id).is_ok_and(|prior| {
                    matches!(
                        &prior.status,
                        RunStatus::Failed {
                            failed_step_id: prior_step_id,
                            message: prior_message,
                        } if prior_step_id == failed_step_id && prior_message == message
                    )
                })
        })
        .count()
}

fn failed_step_for_repair(run: &Run) -> Result<&StepRun, RepairError> {
    let failed_step_id = match &run.status {
        RunStatus::Failed { failed_step_id, .. } => failed_step_id,
        _ => return Err(RepairError::RunNotFailed),
    };
    let step_run = run.step_runs.get(failed_step_id).ok_or_else(|| {
        RepairError::Storage(format!(
            "run status names failed step '{failed_step_id}', but no matching step run exists"
        ))
    })?;
    if step_run.status != StepRunStatus::Failed {
        return Err(RepairError::Storage(format!(
            "run status names failed step '{failed_step_id}', but its step run status is {}",
            step_run.status
        )));
    }
    Ok(step_run)
}

/// Preflight a candidate against the exact same patch and plan validation used
/// at approval time. Only a candidate that passes is written to the patch store.
#[cfg(test)]
fn persist_validated_candidate(
    candidate: &Patch,
    plan: &Plan,
    catalog: &ToolCatalog,
    storage: &StorageRoot,
) -> Result<(), RepairError> {
    validate_patch_candidate(candidate, plan, catalog)?;
    storage
        .patches()
        .save(candidate)
        .map_err(|e| RepairError::Storage(e.to_string()))
}

fn persist_validated_candidate_observed(
    candidate: &Patch,
    plan: &Plan,
    catalog: &ToolCatalog,
    storage: &StorageRoot,
    span: &tracing::Span,
    telemetry: &mut RepairTelemetry,
) -> Result<(), RepairError> {
    let (validation_result, validation_error_count) =
        validate_patch_candidate_with_count(candidate, plan, catalog);
    telemetry.validation_error_count += validation_error_count;
    let validation_outcome = if validation_result.is_ok() {
        "valid"
    } else {
        "invalid"
    };
    tracing::info!(
        parent: span,
        candidate_patch_id = %candidate.id,
        validation_error_count,
        outcome = validation_outcome,
        "repair candidate validation completed"
    );
    validation_result?;

    let persistence_result = storage
        .patches()
        .save(candidate)
        .map_err(|error| RepairError::Storage(error.to_string()));
    let persistence_outcome = if persistence_result.is_ok() {
        "persisted"
    } else {
        "persistence_error"
    };
    tracing::info!(
        parent: span,
        candidate_patch_id = %candidate.id,
        outcome = persistence_outcome,
        "repair candidate persistence completed"
    );
    persistence_result
}

#[cfg(test)]
fn validate_patch_candidate(
    candidate: &Patch,
    plan: &Plan,
    catalog: &ToolCatalog,
) -> Result<(), RepairError> {
    validate_patch_candidate_with_count(candidate, plan, catalog).0
}

fn validate_patch_candidate_with_count(
    candidate: &Patch,
    plan: &Plan,
    catalog: &ToolCatalog,
) -> (Result<(), RepairError>, usize) {
    let updated_plan = match patch::apply_operation(plan.clone(), candidate).map(normalize) {
        Ok(updated_plan) => updated_plan,
        Err(error) => {
            return (Err(RepairError::PatchInvalid(error.to_string())), 1);
        }
    };
    let errors = validator::validate(&updated_plan, catalog);
    if errors.is_empty() {
        return (Ok(()), 0);
    }

    let error_count = errors.len();
    (
        Err(RepairError::PatchInvalid(format_validation_errors(&errors))),
        error_count,
    )
}

fn format_validation_errors(errors: &[crate::error::ValidationError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

// ─── apply_patch ─────────────────────────────────────────────────────────────

/// Apply an approved patch to the plan, validate the result, and save the new
/// plan version.
///
/// Steps:
/// 1. Verify the patch is `Approved`.
/// 2. Apply the `PatchOperation` to the step list.
/// 3. Bump the plan version.
/// 4. Normalise (stable topological sort, alphabetical dependency lists).
/// 5. Validate — return `PatchInvalid` if any errors remain.
/// 6. Save the new plan version.
/// 7. Mark the patch `Applied` and save it.
/// 8. Return the updated plan.
pub fn apply_patch(
    patch: &Patch,
    plan: Plan,
    catalog: &ToolCatalog,
    storage: &StorageRoot,
) -> Result<Plan, RepairError> {
    let next_version = u64::from(plan.metadata.version) + 1;
    let span = tracing::info_span!(
        "inxm.repair.apply",
        run_id = %patch.run_id,
        plan_id = %plan.metadata.id,
        base_version = plan.metadata.version,
        next_version,
        patch_id = %patch.id,
        failing_step_id = %patch.failing_step_id,
        error_kind = "not_available",
        correction_attempt_count = 0_u32,
        validation_error_count = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
        outcome = tracing::field::Empty,
    );
    let started_at = Instant::now();
    let mut telemetry = RepairTelemetry::default();
    let result =
        span.in_scope(|| apply_patch_inner(patch, plan, catalog, storage, &span, &mut telemetry));
    finish_repair_span(&span, started_at, &result, &telemetry);
    result
}

fn apply_patch_inner(
    patch: &Patch,
    plan: Plan,
    catalog: &ToolCatalog,
    storage: &StorageRoot,
    span: &tracing::Span,
    telemetry: &mut RepairTelemetry,
) -> Result<Plan, RepairError> {
    // 1. Bind the operation to the exact approved record reviewed by the
    // human. A caller-held snapshot is not authoritative once persisted.
    let persisted_patch = storage
        .patches()
        .load(&patch.id)
        .map_err(|error| RepairError::Storage(error.to_string()))?;
    if persisted_patch != *patch {
        telemetry.outcome = Some("conflict_patch_snapshot_mismatch");
        return Err(RepairError::Storage(format!(
            "persisted patch '{}' does not exactly match the supplied patch",
            patch.id
        )));
    }
    if persisted_patch.status != PatchStatus::Approved {
        telemetry.outcome = Some("conflict_patch_not_approved");
        return Err(RepairError::PatchNotApproved {
            patch_id: persisted_patch.id,
            status: persisted_patch.status.to_string(),
        });
    }

    // 2. The supplied plan must be the exact current immutable base targeted by
    // the persisted patch.
    if patch.plan_id != plan.metadata.id {
        telemetry.outcome = Some("conflict_plan_identity");
        return Err(RepairError::Storage(format!(
            "repair plan identity mismatch for patch '{}': patch targets '{}', supplied plan is '{}'",
            patch.id, patch.plan_id, plan.metadata.id
        )));
    }
    if patch.plan_version != plan.metadata.version {
        telemetry.outcome = Some("conflict_plan_version");
        return Err(RepairError::Storage(format!(
            "repair base version mismatch for patch '{}': patch targets v{}, supplied plan is v{}",
            patch.id, patch.plan_version, plan.metadata.version
        )));
    }
    let current_plan = storage
        .plans()
        .load_current(&patch.plan_id)
        .map_err(|error| RepairError::Storage(error.to_string()))?;
    if current_plan != plan {
        telemetry.outcome = Some("conflict_stale_plan");
        return Err(RepairError::Storage(format!(
            "stale repair base for patch '{}': supplied plan v{} is not the current persisted plan v{}",
            patch.id, plan.metadata.version, current_plan.metadata.version
        )));
    }

    let base_plan_id = plan.metadata.id.clone();
    let base_version = plan.metadata.version;
    base_version.checked_add(1).ok_or_else(|| {
        telemetry.outcome = Some("conflict_version_overflow");
        RepairError::Storage(format!(
            "cannot apply patch '{}': plan version overflow at v{base_version}",
            patch.id
        ))
    })?;

    // 3. Apply the patch operation to the step list.
    let mut updated_plan = match patch::apply_operation(plan, patch) {
        Ok(updated_plan) => updated_plan,
        Err(error) => {
            telemetry.validation_error_count = 1;
            telemetry.outcome = Some(OUTCOME_INVALID_PATCH);
            tracing::info!(
                parent: span,
                validation_error_count = 1,
                outcome = "invalid_operation",
                "repair application validation completed"
            );
            return Err(RepairError::PatchInvalid(error.to_string()));
        }
    };

    // 4. Bump the version and record the actual repair parent.
    updated_plan.metadata = updated_plan.metadata.next_version();
    updated_plan.metadata.parent_plan_id = Some(base_plan_id);
    updated_plan.metadata.parent_version = Some(base_version);

    // 5. Normalise for stable topological order and consistent field ordering.
    updated_plan = normalize(updated_plan);

    // 6. Validate — the patched plan must pass all checks.
    let errors = validator::validate(&updated_plan, catalog);
    telemetry.validation_error_count = errors.len();
    let validation_outcome = if errors.is_empty() {
        "valid"
    } else {
        "invalid"
    };
    tracing::info!(
        parent: span,
        validation_error_count = errors.len(),
        outcome = validation_outcome,
        "repair application validation completed"
    );
    if !errors.is_empty() {
        telemetry.outcome = Some(OUTCOME_INVALID_PATCH);
        return Err(RepairError::PatchInvalid(format_validation_errors(&errors)));
    }

    // 7. Atomically publish the new plan and mark the exact persisted patch as
    // applied. Storage rechecks current-version ownership under its commit lock.
    let commit_result = storage
        .commit_repair(&patch.id, &updated_plan)
        .map_err(|error| RepairError::Storage(error.to_string()));
    let commit_outcome = match &commit_result {
        Ok(()) => "committed",
        Err(RepairError::Storage(message)) if message.contains("stale repair") => {
            telemetry.outcome = Some("conflict_stale_plan");
            "conflict_stale_plan"
        }
        Err(_) => {
            telemetry.outcome = Some("commit_error");
            "commit_error"
        }
    };
    tracing::info!(
        parent: span,
        patch_id = %patch.id,
        plan_id = %updated_plan.metadata.id,
        next_version = updated_plan.metadata.version,
        outcome = commit_outcome,
        "repair atomic commit completed"
    );
    commit_result?;

    // 8. Return the validated, atomically persisted plan.
    Ok(updated_plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{
        FanOutConfig, PlanMetadata, PlanOutput, PlanStep, StepConfig, ToolCallConfig,
    };
    use crate::storage::patches::PatchOperation;
    use crate::tools::catalog::{SubprocessConfig, ToolConfig, ToolEntry};
    use indexmap::IndexMap;

    fn tool(name: &str, required: &[&str]) -> ToolEntry {
        ToolEntry {
            name: name.to_owned(),
            description: String::new(),
            config: ToolConfig::Subprocess(SubprocessConfig {
                command: "true".to_owned(),
                args: vec![],
                env: IndexMap::new(),
                working_dir: None,
            }),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "url": { "type": "string" } },
                "required": required,
            }),
            output_schema: serde_json::json!({ "type": "string" }),
            allowlisted: true,
            timeout_secs: None,
        }
    }

    fn fan_out_plan() -> Plan {
        let output = |name: &str, value_type: &str| PlanOutput {
            name: name.to_owned(),
            description: None,
            value_type: value_type.to_owned(),
        };
        let mut body_arguments = IndexMap::new();
        body_arguments.insert("url".to_owned(), serde_json::json!("${item.url}"));

        Plan {
            metadata: PlanMetadata::new(None),
            name: "fetch-posts".to_owned(),
            description: None,
            inputs: vec![],
            config: IndexMap::new(),
            steps: vec![
                PlanStep {
                    id: "extract_post_links".to_owned(),
                    name: "Extract post links".to_owned(),
                    description: None,
                    config: StepConfig::ToolCall(ToolCallConfig {
                        tool: "produce-links".to_owned(),
                        arguments: IndexMap::new(),
                    }),
                    depends_on: vec![],
                    outputs: vec![output("post_urls", "array")],
                    timeout_secs: None,
                    retry: None,
                },
                PlanStep {
                    id: "fetch_posts".to_owned(),
                    name: "Fetch posts".to_owned(),
                    description: None,
                    config: StepConfig::FanOut(FanOutConfig {
                        over: "extract_post_links.post_urls".to_owned(),
                        item_var: "item".to_owned(),
                        spawn_steps: vec!["fetch_post_body".to_owned()],
                        until: None,
                    }),
                    depends_on: vec!["extract_post_links".to_owned()],
                    outputs: vec![],
                    timeout_secs: None,
                    retry: None,
                },
                PlanStep {
                    id: "fetch_post_body".to_owned(),
                    name: "Fetch post body".to_owned(),
                    description: None,
                    config: StepConfig::ToolCall(ToolCallConfig {
                        tool: "http-get".to_owned(),
                        arguments: body_arguments,
                    }),
                    depends_on: vec![],
                    outputs: vec![output("body", "string")],
                    timeout_secs: None,
                    retry: None,
                },
            ],
            outputs: vec![],
        }
    }

    fn catalog() -> ToolCatalog {
        ToolCatalog::new(vec![tool("produce-links", &[]), tool("http-get", &["url"])])
    }

    fn item_patch(plan: &Plan, value: &str) -> Patch {
        Patch::new(
            &plan.metadata.id,
            plan.metadata.version,
            "run-1",
            "fetch_posts",
            PatchOperation::SetStepField {
                step_id: "fetch_post_body".to_owned(),
                pointer: "/config/arguments/url".to_owned(),
                value: serde_json::json!(value),
            },
            "Bind the current fan-out item",
        )
    }

    fn approved_item_patch(plan: &Plan, value: &str) -> Patch {
        let mut patch = item_patch(plan, value);
        patch.status = PatchStatus::Approved;
        patch.approved_at = Some(chrono::Utc::now());
        patch
    }

    fn persist_base_and_patch(storage: &StorageRoot, plan: &Plan, patch: &Patch) {
        storage.plans().save(plan).unwrap();
        storage.patches().save(patch).unwrap();
    }

    #[test]
    fn preflight_rejects_bare_fan_out_item_placeholder() {
        let plan = fan_out_plan();
        let error = validate_patch_candidate(&item_patch(&plan, "${item}"), &plan, &catalog())
            .expect_err("bare item placeholder must fail preflight");

        assert!(
            error
                .to_string()
                .contains("unrecognised placeholder namespace")
        );
    }

    #[test]
    fn preflight_accepts_exact_fan_out_item_variable() {
        let plan = fan_out_plan();
        validate_patch_candidate(&item_patch(&plan, "${item.item}"), &plan, &catalog())
            .expect("exact fan-out item variable should pass preflight");
    }

    #[test]
    fn invalid_candidate_is_not_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(temp.path()).unwrap();
        let plan = fan_out_plan();
        let invalid = item_patch(&plan, "${item}");

        persist_validated_candidate(&invalid, &plan, &catalog(), &storage)
            .expect_err("invalid candidate must not be saved");

        assert!(storage.patches().list().unwrap().is_empty());
    }

    #[test]
    fn apply_patch_rejects_unapproved_patches_with_a_typed_error() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(temp.path()).unwrap();
        let plan = fan_out_plan();
        let pending = item_patch(&plan, "${__item__}");
        persist_base_and_patch(&storage, &plan, &pending);

        let error = apply_patch(&pending, plan.clone(), &catalog(), &storage)
            .expect_err("a pending patch must not apply");

        assert!(matches!(
            error,
            RepairError::PatchNotApproved { ref patch_id, .. } if *patch_id == pending.id
        ));
        assert_eq!(
            storage.patches().load(&pending.id).unwrap().status,
            PatchStatus::Pending
        );
        assert_eq!(
            storage.plans().load_current(&plan.metadata.id).unwrap(),
            plan
        );
    }

    #[test]
    fn apply_patch_atomically_publishes_plan_and_patch_with_parent_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(temp.path()).unwrap();
        let plan = fan_out_plan();
        let patch = approved_item_patch(&plan, "${item.item}");
        persist_base_and_patch(&storage, &plan, &patch);

        let updated = apply_patch(&patch, plan.clone(), &catalog(), &storage).unwrap();

        assert_eq!(updated.metadata.version, plan.metadata.version + 1);
        assert_eq!(
            updated.metadata.parent_plan_id.as_deref(),
            Some(plan.metadata.id.as_str())
        );
        assert_eq!(updated.metadata.parent_version, Some(plan.metadata.version));
        assert_eq!(
            storage.plans().load_current(&plan.metadata.id).unwrap(),
            updated
        );
        assert_eq!(
            storage.patches().load(&patch.id).unwrap().status,
            PatchStatus::Applied
        );
    }

    #[test]
    fn recurring_failure_guard_counts_only_applied_patches_with_identical_failures() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(temp.path()).unwrap();
        let plan = fan_out_plan();
        let failure = "unresolved placeholder(s): ${step.parse.branch_name}";

        let save_failed_run = |run_id: &str, step_id: &str, message: &str| {
            let mut run = Run::new(&plan.metadata.id, plan.metadata.version);
            run.id = run_id.to_owned();
            run.status = RunStatus::Failed {
                failed_step_id: step_id.to_owned(),
                message: message.to_owned(),
            };
            storage.runs().save(&run).unwrap();
            run
        };
        let save_patch = |run_id: &str, step_id: &str, status: PatchStatus| {
            let mut patch = Patch::new(
                &plan.metadata.id,
                plan.metadata.version,
                run_id,
                step_id,
                PatchOperation::RemovePlanField {
                    pointer: "/config/obsolete".to_owned(),
                },
                "test repair",
            );
            patch.status = status;
            storage.patches().save(&patch).unwrap();
        };

        // Two applied repairs whose motivating runs failed exactly like the
        // current run — these must count.
        save_failed_run("run-prior-1", "fetch_posts", failure);
        save_patch("run-prior-1", "fetch_posts", PatchStatus::Applied);
        save_failed_run("run-prior-2", "fetch_posts", failure);
        save_patch("run-prior-2", "fetch_posts", PatchStatus::Applied);
        // Same step but a different message — must not count.
        save_failed_run("run-other-msg", "fetch_posts", "timed out after 30s");
        save_patch("run-other-msg", "fetch_posts", PatchStatus::Applied);
        // Identical failure but the patch was never applied — must not count.
        save_failed_run("run-pending", "fetch_posts", failure);
        save_patch("run-pending", "fetch_posts", PatchStatus::Pending);
        // A different failing step — must not count.
        save_failed_run("run-other-step", "extract_post_links", failure);
        save_patch("run-other-step", "extract_post_links", PatchStatus::Applied);

        let current = save_failed_run("run-current", "fetch_posts", failure);
        // A patch already recorded for the current run itself is not a *prior*
        // repair and must be excluded.
        save_patch("run-current", "fetch_posts", PatchStatus::Applied);

        assert_eq!(applied_repairs_for_recurring_failure(&storage, &current), 2);

        let unrelated = save_failed_run("run-unrelated", "fetch_posts", "some new failure");
        assert_eq!(
            applied_repairs_for_recurring_failure(&storage, &unrelated),
            0,
            "a new failure message must start with a clean slate"
        );
    }

    #[test]
    fn apply_patch_rejects_a_patch_for_another_plan() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(temp.path()).unwrap();
        let patch_plan = fan_out_plan();
        let supplied_plan = fan_out_plan();
        let patch = approved_item_patch(&patch_plan, "${item.item}");
        storage.patches().save(&patch).unwrap();
        storage.plans().save(&supplied_plan).unwrap();

        let error = apply_patch(&patch, supplied_plan, &catalog(), &storage).unwrap_err();

        assert!(
            error.to_string().contains("plan identity mismatch"),
            "got: {error}"
        );
    }

    #[test]
    fn apply_patch_rejects_a_stale_base_plan() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(temp.path()).unwrap();
        let plan = fan_out_plan();
        let patch = approved_item_patch(&plan, "${item.item}");
        persist_base_and_patch(&storage, &plan, &patch);
        let mut current = plan.clone();
        current.metadata = current.metadata.next_version();
        current.name = "newer-plan".to_owned();
        storage.plans().save(&current).unwrap();

        let error = apply_patch(&patch, plan, &catalog(), &storage).unwrap_err();

        assert!(
            error.to_string().contains("stale repair base"),
            "got: {error}"
        );
        assert_eq!(
            storage.patches().load(&patch.id).unwrap().status,
            PatchStatus::Approved
        );
    }

    #[test]
    fn apply_patch_rejects_a_caller_patch_that_differs_from_storage() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(temp.path()).unwrap();
        let plan = fan_out_plan();
        let persisted = approved_item_patch(&plan, "${item.item}");
        persist_base_and_patch(&storage, &plan, &persisted);
        let mut supplied = persisted.clone();
        supplied.rationale = "caller-mutated rationale".to_owned();

        let error = apply_patch(&supplied, plan, &catalog(), &storage).unwrap_err();

        assert!(
            error.to_string().contains("does not exactly match"),
            "got: {error}"
        );
    }

    #[test]
    fn failed_step_selection_uses_the_id_named_by_run_status() {
        let mut first = StepRun::new("first");
        first.status = StepRunStatus::Failed;
        let mut named = StepRun::new("named");
        named.status = StepRunStatus::Failed;
        let mut run = Run::new("plan-1", 1);
        run.step_runs.insert(first.step_id.clone(), first);
        run.step_runs.insert(named.step_id.clone(), named);
        run.status = RunStatus::Failed {
            failed_step_id: "named".to_owned(),
            message: "named failed".to_owned(),
        };

        let selected = failed_step_for_repair(&run).unwrap();

        assert_eq!(selected.step_id, "named");
    }

    #[test]
    fn failed_step_selection_rejects_inconsistent_run_state() {
        let mut run = Run::new("plan-1", 1);
        run.status = RunStatus::Failed {
            failed_step_id: "missing".to_owned(),
            message: "missing failed".to_owned(),
        };

        let error = failed_step_for_repair(&run).unwrap_err();

        assert!(
            error.to_string().contains("no matching step run"),
            "got: {error}"
        );
    }
}
