//! FailurePacket — a self-contained snapshot of everything the repair loop
//! needs to understand why a step failed.
//!
//! The packet is built synchronously from run state and the plan definition.
//! It is passed to the compiler backend as part of a `RepairRequest`.

use crate::plan::types::PlanStep;
use crate::repair::classifier::{self, ErrorKind};
use crate::storage::runs::{Run, StepRun};
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Guidance appended to `error_message` when the failure classifies as
/// `ErrorKind::ExternalEndpointDown`.
///
/// The compiler backend only sees `FailurePacket::error_message` (via
/// `RepairRequest`), so this text must say, explicitly and up front, that the
/// remote endpoint is unreachable and that repair should substitute the
/// endpoint or add a fallback step — never resume against the same URL.
/// Without this, the raw transport error reads like any other failure and the
/// compiler proposes an unrelated patch (e.g. switching interpreters) while
/// leaving the same dead URL in place, so resume fails identically.
/// Marker prefixing repair guidance inside an enriched `error_message`.
/// `app::engine` also looks for it when deciding whether to hint at
/// `/repair` after a failed resume — keep the guidance text below starting
/// with it.
pub(crate) const REPAIR_GUIDANCE_MARKER: &str = "[repair-guidance]";

const EXTERNAL_ENDPOINT_DOWN_GUIDANCE: &str = "\n\n[repair-guidance] This is an EXTERNAL ENDPOINT DOWN failure: the remote \
host is unreachable at the transport level (DNS failure, connection \
refused/reset, timeout, or TLS error) — it is not an executor or code bug. \
Resuming the step against the same URL will fail again identically. Propose \
substituting the endpoint with a reliable alternative, or adding a fallback \
step (e.g. an offline/local computation, or a different provider), instead \
of retrying the same call.";

/// Guidance appended to `error_message` when the failure classifies as
/// `ErrorKind::MissingInterpreter`.
///
/// The interpreter was located on `PATH` (so the env probe reported it as
/// available) but the OS rejected the `exec` call — common causes: a broken
/// shebang script, a macOS CLT stub that never executes, or a Python wrapper
/// whose real runtime is absent. Retrying with the same interpreter will
/// always fail. The repair must switch to an interpreter that is confirmed
/// available (see the `## Execution environment` section), or use a
/// TOOL_CALL step instead.
/// Guidance appended to `error_message` when the failure classifies as
/// `ErrorKind::OutputSchemaViolation` — the script ran successfully but its
/// stdout could not be mapped onto the declared outputs (invalid JSON, e.g.
/// Python `json.dumps` emitting lone UTF-16 surrogate escapes after decoding
/// stdin with the wrong codec, or plain text where an object is required).
/// Without this, the stdout in the packet *looks* like perfect JSON to the
/// backend and it keeps rewriting the step's (working) parsing logic.
const OUTPUT_MAPPING_GUIDANCE: &str = "\n\n[repair-guidance] This is an OUTPUT MAPPING failure: the script itself ran \
successfully, but its stdout could not be parsed as a single RFC 8259 JSON \
object, so the declared outputs stayed empty. Do NOT rewrite the script's \
input-parsing logic — it is not the problem. Fix how the output is emitted: \
the script must print exactly one JSON object whose keys are the declared \
output names, with no invalid escape sequences. A frequent cause on Windows \
is Python decoding stdin with the ANSI code page and json.dumps then emitting \
lone UTF-16 surrogate escapes; ensure the step reads and writes UTF-8 (for \
Python, set the step's env to PYTHONUTF8=1 / PYTHONIOENCODING=utf-8, or \
decode input bytes explicitly with errors='replace').";

/// Guidance appended by the repair loop (see `propose_repair`) when earlier
/// applied patches already targeted this exact step + error and the failure
/// recurred unchanged — proof the previously edited field is not the root
/// cause, so the backend must not propose another variation of the same edit.
pub(crate) fn repeated_failure_guidance(prior_repair_count: usize) -> String {
    format!(
        "\n\n[repair-guidance] REPEATED FAILURE AFTER REPAIR: {prior_repair_count} earlier \
         patch(es) for this plan were already applied for this exact failing step and \
         error message, and the failure recurred unchanged. Whatever those patches \
         edited is NOT the root cause — do not propose another variation of the same \
         edit. Re-diagnose from first principles: inspect the upstream steps' actual \
         outputs, the execution environment (interpreter, encoding, working directory, \
         external state), and consider whether the fix belongs in a different step, in \
         a step's env, or in the world rather than in the previously edited field."
    )
}

const MISSING_INTERPRETER_GUIDANCE: &str = "\n\n[repair-guidance] This is a MISSING INTERPRETER failure: \
the code interpreter was found on PATH but the OS could not execute it \
(e.g. broken shebang, macOS CLT stub, or a wrapper pointing to an absent \
runtime). The spawn itself fails with ENOENT — retrying the same interpreter \
will fail identically. \
Do NOT propose another CODE_CALL step with the same interpreter. \
Consult the '## Execution environment' section to see which interpreters are \
confirmed available on this machine. If none are suitable, use a TOOL_CALL \
step instead.";

// ─── FailurePacket ────────────────────────────────────────────────────────────

/// A self-contained failure context for one step run.
///
/// Fields intentionally mirror the keys that the compiler prompt template
/// expects, so the backend can render them directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailurePacket {
    /// Unique packet ID (UUID v4).
    pub id: String,
    pub run_id: String,
    pub plan_id: String,
    pub plan_version: u32,
    /// The full plan step definition that failed.
    pub failing_step: PlanStep,
    /// Resolved input values that were passed to the step at runtime.
    ///
    /// Populated from call-site context if available; defaults to an empty
    /// object when the executor did not record runtime inputs.
    pub runtime_inputs: serde_json::Value,
    /// Outputs from each upstream dependency, keyed by step ID.
    ///
    /// These are the actual runtime values the failing step could see, which
    /// is essential context for diagnosing output-schema mismatches.
    pub dependency_outputs: IndexMap<String, serde_json::Value>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    /// Human-readable error message recorded by the executor.
    ///
    /// When `error_kind` is `ExternalEndpointDown`, this is enriched with an
    /// explicit `[repair-guidance]` note (see
    /// `EXTERNAL_ENDPOINT_DOWN_GUIDANCE`) so the compiler backend — which only
    /// receives this string, not the structured `error_kind` — cannot miss
    /// that the fix is "substitute the endpoint", not "resume".
    pub error_message: String,
    /// Deterministic classification of `error_message`/`stderr`, computed at
    /// build time via `classifier::classify`.
    pub error_kind: ErrorKind,
    pub created_at: DateTime<Utc>,
}

impl FailurePacket {
    /// Build a `FailurePacket` from run state and the plan step definition.
    ///
    /// `dependency_outputs` is collected from whichever upstream steps have
    /// already completed (i.e., their outputs are present in `run.step_runs`).
    pub fn build(run: &Run, step: &PlanStep, step_run: &StepRun) -> Self {
        // Collect outputs from all declared upstream dependencies that ran.
        let dependency_outputs: IndexMap<String, serde_json::Value> = step
            .depends_on
            .iter()
            .filter_map(|dep_id| {
                run.step_runs.get(dep_id).map(|dep_run| {
                    let outputs =
                        serde_json::to_value(&dep_run.outputs).unwrap_or(serde_json::Value::Null);
                    (dep_id.clone(), outputs)
                })
            })
            .collect();

        let raw_error_message = step_run
            .error
            .clone()
            .unwrap_or_else(|| "unknown error".to_owned());
        let error_kind = classifier::classify(&raw_error_message, step_run.stderr.as_deref());
        let error_message = match error_kind {
            ErrorKind::ExternalEndpointDown => {
                format!("{raw_error_message}{EXTERNAL_ENDPOINT_DOWN_GUIDANCE}")
            }
            ErrorKind::MissingInterpreter => {
                format!("{raw_error_message}{MISSING_INTERPRETER_GUIDANCE}")
            }
            ErrorKind::OutputSchemaViolation => {
                format!("{raw_error_message}{OUTPUT_MAPPING_GUIDANCE}")
            }
            _ => raw_error_message,
        };

        Self {
            id: uuid::Uuid::new_v4().to_string(),
            run_id: run.id.clone(),
            plan_id: run.plan_id.clone(),
            plan_version: run.plan_version,
            failing_step: step.clone(),
            // StepRun does not yet retain a separately resolved input snapshot,
            // so use the run's validated invocation values (including defaults)
            // rather than hiding the available runtime context.
            runtime_inputs: serde_json::Value::Object(
                run.inputs
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone()))
                    .collect(),
            ),
            dependency_outputs,
            stdout: step_run.stdout.clone(),
            stderr: step_run.stderr.clone(),
            error_message,
            error_kind,
            created_at: Utc::now(),
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{PlanStep, StepConfig, ToolCallConfig};
    use crate::storage::runs::{Run, RunStatus, StepRun, StepRunStatus};
    use indexmap::IndexMap;

    /// Drift guard: `app::engine` detects enriched messages by this marker,
    /// so the guidance text must keep starting with it.
    #[test]
    fn guidance_starts_with_the_shared_marker() {
        assert!(
            EXTERNAL_ENDPOINT_DOWN_GUIDANCE
                .trim_start()
                .starts_with(REPAIR_GUIDANCE_MARKER)
        );
        assert!(
            MISSING_INTERPRETER_GUIDANCE
                .trim_start()
                .starts_with(REPAIR_GUIDANCE_MARKER)
        );
        assert!(
            OUTPUT_MAPPING_GUIDANCE
                .trim_start()
                .starts_with(REPAIR_GUIDANCE_MARKER)
        );
        assert!(
            repeated_failure_guidance(2)
                .trim_start()
                .starts_with(REPAIR_GUIDANCE_MARKER)
        );
    }

    fn step() -> PlanStep {
        PlanStep {
            id: "fetch_time".to_owned(),
            name: "Fetch Tokyo time".to_owned(),
            description: None,
            config: StepConfig::ToolCall(ToolCallConfig {
                tool: "http-get".to_owned(),
                arguments: IndexMap::new(),
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        }
    }

    fn run_with_step(step_run: StepRun) -> Run {
        let mut run = Run::new("plan-1", 1);
        run.status = RunStatus::Failed {
            failed_step_id: step_run.step_id.clone(),
            message: step_run.error.clone().unwrap_or_default(),
        };
        run.step_runs.insert(step_run.step_id.clone(), step_run);
        run
    }

    fn failed_step_run(step_id: &str, error: &str) -> StepRun {
        let mut step_run = StepRun::new(step_id);
        step_run.status = StepRunStatus::Failed;
        step_run.error = Some(error.to_owned());
        step_run
    }

    #[test]
    fn external_endpoint_down_error_message_is_enriched_with_guidance() {
        let step_run = failed_step_run(
            "fetch_time",
            "HTTP request failed: error sending request for url (http://worldtimeapi.org/api/timezone/Asia/Tokyo): error trying to connect: dns error: failed to lookup address information: Temporary failure in name resolution",
        );
        let run = run_with_step(step_run.clone());
        let packet = FailurePacket::build(&run, &step(), &step_run);

        assert_eq!(packet.error_kind, ErrorKind::ExternalEndpointDown);
        assert!(packet.error_message.contains("[repair-guidance]"));
        assert!(packet.error_message.contains("EXTERNAL ENDPOINT DOWN"));
        assert!(packet.error_message.contains("substituting the endpoint"));
        assert!(
            !packet
                .error_message
                .to_lowercase()
                .contains("resuming the step against the same url will succeed"),
        );
    }

    #[test]
    fn non_transport_failure_leaves_error_message_untouched() {
        let step_run = failed_step_run("fetch_time", "tool exited with code 1");
        let run = run_with_step(step_run.clone());
        let packet = FailurePacket::build(&run, &step(), &step_run);

        assert_eq!(packet.error_kind, ErrorKind::ToolExecutionFailed);
        assert_eq!(packet.error_message, "tool exited with code 1");
        assert!(!packet.error_message.contains("[repair-guidance]"));
    }

    #[test]
    fn missing_interpreter_error_message_is_enriched_with_guidance() {
        let step_run = failed_step_run(
            "fetch_time",
            "step execution failed (step: fetch_current_time): failed to spawn interpreter '/usr/bin/python3': No such file or directory (os error 2)",
        );
        let run = run_with_step(step_run.clone());
        let packet = FailurePacket::build(&run, &step(), &step_run);

        assert_eq!(packet.error_kind, ErrorKind::MissingInterpreter);
        assert!(packet.error_message.contains("[repair-guidance]"));
        assert!(packet.error_message.contains("MISSING INTERPRETER"));
        assert!(
            packet
                .error_message
                .contains("Do NOT propose another CODE_CALL step with the same interpreter"),
            "guidance must tell the repair compiler not to retry the same interpreter"
        );
    }

    #[test]
    fn output_mapping_error_message_is_enriched_with_guidance() {
        let step_run = failed_step_run(
            "parse_branch_and_diff",
            "step execution failed (step: parse_branch_and_diff): script succeeded but its stdout cannot satisfy the declared outputs (branch_name, staged_diff): stdout is not a valid JSON object (lone leading surrogate in hex escape at line 1 column 54792). A CODE_CALL step with more than one declared output must print exactly one JSON object whose keys are the declared output names.",
        );
        let run = run_with_step(step_run.clone());
        let packet = FailurePacket::build(&run, &step(), &step_run);

        assert_eq!(packet.error_kind, ErrorKind::OutputSchemaViolation);
        assert!(packet.error_message.contains("[repair-guidance]"));
        assert!(packet.error_message.contains("OUTPUT MAPPING"));
        assert!(
            packet
                .error_message
                .contains("Do NOT rewrite the script's input-parsing logic"),
            "guidance must steer repair away from rewriting the working parsing code"
        );
        assert!(
            packet.error_message.contains("PYTHONUTF8=1"),
            "guidance must name the concrete encoding fix"
        );
    }

    #[test]
    fn runtime_inputs_include_the_runs_validated_invocation_values() {
        let step_run = failed_step_run("fetch_time", "tool exited with code 1");
        let mut run = run_with_step(step_run.clone());
        run.inputs
            .insert("timezone".to_owned(), serde_json::json!("Asia/Tokyo"));
        run.inputs
            .insert("retry_limit".to_owned(), serde_json::json!(2));

        let packet = FailurePacket::build(&run, &step(), &step_run);

        assert_eq!(
            packet.runtime_inputs,
            serde_json::json!({
                "timezone": "Asia/Tokyo",
                "retry_limit": 2
            })
        );
    }
}
