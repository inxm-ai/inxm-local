//! Core compiler backend types and the profile-backed `Backend`.
//!
//! `async fn` in traits is not object-safe without the `async_trait` crate, so
//! compile/repair methods live as inherent methods on the concrete `Backend`
//! struct rather than on a trait.  The `CompilerBackend` trait is kept for
//! lightweight introspection (e.g. logging which backend is active) and can be
//! used as a trait object where only `name()` is needed.

use crate::compiler::{diagnostics, extractor, prompt};
use crate::error::CompilerError;
use crate::llm::{CompletionRequest, CompletionResponse, LlmError, LlmProfile};
use crate::plan::types::{InputKind, Plan, PlanMetadata, StepType};
use crate::storage::patches::{Patch, PatchOperation};
use crate::storage::world_fixes::{RemediationAction, WorldFix};
use crate::tools::catalog::{ToolCatalog, ToolEntry, ToolKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;
use tracing::Instrument;

/// Boxed future returned by [`CompletionPort`].
pub type CompletionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CompletionResponse, LlmError>> + Send + 'a>>;

/// Compiler-owned port for the external completion transport.
///
/// Production uses the shared LLM transport. Tests and alternate hosts can
/// inject a deterministic nullable implementation without replacing compiler
/// domain logic.
pub trait CompletionPort: Send + Sync {
    fn complete<'a>(
        &'a self,
        profile: &'a LlmProfile,
        request: CompletionRequest<'a>,
    ) -> CompletionFuture<'a>;
}

struct LlmCompletionPort;

impl CompletionPort for LlmCompletionPort {
    fn complete<'a>(
        &'a self,
        profile: &'a LlmProfile,
        request: CompletionRequest<'a>,
    ) -> CompletionFuture<'a> {
        Box::pin(crate::llm::complete(profile, request))
    }
}

#[derive(Debug, Clone, Copy)]
enum CompilerOperation {
    Assess,
    Compile,
    Design,
    DesignCorrection,
    RepairCorrection,
    RepairImplementation,
    RepairStrategy,
    SynthesizeTool,
    Utility,
}

impl CompilerOperation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Assess => "assess",
            Self::Compile => "compile",
            Self::Design => "design",
            Self::DesignCorrection => "design_correction",
            Self::RepairCorrection => "repair_correction",
            Self::RepairImplementation => "repair_implementation",
            Self::RepairStrategy => "repair_strategy",
            Self::SynthesizeTool => "synthesize_tool",
            Self::Utility => "utility",
        }
    }
}

// ─── Request types ────────────────────────────────────────────────────────────

/// Runtime evidence from one iteration of a fan-out step.
#[derive(Debug, Clone, Serialize)]
pub struct CompileRunIteration {
    pub iteration: usize,
    pub status: String,
    pub duration_ms: u64,
    pub outputs: serde_json::Value,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub error: Option<String>,
}

/// Runtime evidence from one step in a prior plan execution.
#[derive(Debug, Clone, Serialize)]
pub struct CompileRunStep {
    pub step_id: String,
    pub status: String,
    pub attempt: u32,
    pub duration_ms: Option<u64>,
    pub outputs: serde_json::Value,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub error: Option<String>,
    pub iterations: Vec<CompileRunIteration>,
}

/// A compiler-facing snapshot of one prior execution of the plan being edited.
/// Runtime values are redacted and bounded when rendered into the model prompt.
#[derive(Debug, Clone, Serialize)]
pub struct CompileRunHistoryEntry {
    pub run_id: String,
    pub plan_version: u32,
    pub status: String,
    pub status_message: Option<String>,
    pub started_at: String,
    pub inputs: serde_json::Value,
    pub outputs: serde_json::Value,
    pub steps: Vec<CompileRunStep>,
}

/// A compile-time request: turn natural-language intent into a typed `Plan`.
#[derive(Clone)]
pub struct CompileRequest {
    /// The natural-language description of what the workflow should do.
    pub intent: String,
    /// Step types the compiler is allowed to emit. The compiler prompt and the
    /// post-compile capability check both enforce this list. In particular,
    /// `AGENT_CALL` is present only when the selected execution backend is a
    /// real agent-shaped CLI and the experimental setting is enabled.
    pub allowed_step_types: Vec<StepType>,
    /// Tools visible to this plan. Passed to the prompt so the LLM knows which
    /// tool names and argument schemas are available.
    pub tool_catalog: Vec<ToolEntry>,
    /// If set, the compiler is updating an existing plan rather than creating a
    /// new one. The LLM receives the current plan for reference, and the
    /// resulting plan keeps the same plan ID with an incremented version.
    pub existing_plan: Option<Plan>,
    /// Newest-first execution evidence for the plan being edited. Empty for new
    /// plans. Prompt rendering treats every runtime value as untrusted data and
    /// applies compiler-owned redaction and size limits.
    pub run_history: Vec<CompileRunHistoryEntry>,
    /// Optional free-form context appended to the user prompt (constraints,
    /// environment hints, prior conversation summary, etc.).
    pub extra_context: Option<String>,
}

/// A tool-synthesis request: given a bare name/description reference (no
/// runnable config), ask the model to invent a plausible `ToolEntry`.
///
/// Used when importing a plan bundle that references a tool not present in
/// the local catalog — see [`crate::plan::bundle::PlanBundle`].
pub struct ToolSynthesisRequest {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    /// Best-effort hint for which `ToolConfig` shape to synthesize.
    pub kind_hint: Option<ToolKind>,
    /// Optional free-form context (e.g. host environment description).
    pub extra_context: Option<String>,
}

/// A repair request: a step has failed, ask the model for a targeted patch.
pub struct RepairRequest {
    /// The plan that was executing when the step failed.
    pub plan: Plan,
    /// ID of the run in which the failure occurred. Required to construct the
    /// `Patch` record that will be stored and presented for approval.
    pub run_id: String,
    /// ID of the step that failed.
    pub failing_step_id: String,
    /// Human-readable error message from the executor.
    pub error_message: String,
    /// Captured stdout from the failing step (if any).
    pub stdout: Option<String>,
    /// Captured stderr from the failing step (if any).
    pub stderr: Option<String>,
    /// The resolved input values that were passed to the step at runtime.
    pub runtime_inputs: serde_json::Value,
    /// Actual runtime outputs of each upstream dependency, keyed by step ID.
    ///
    /// This is the ground truth for what a dependency step *actually*
    /// produced (as opposed to what it was declared to produce). Combined
    /// with the dependency's `outputs` list in `plan`, it lets the model
    /// distinguish "the declared output name is wrong" from "the step ran
    /// but produced nothing" — without it, patches tend to guess generic
    /// output names instead of reusing the ones the plan already declares.
    pub dependency_outputs: indexmap::IndexMap<String, serde_json::Value>,
    /// Allowlisted tools available to the repaired plan. Repair prompts use the
    /// concrete adapter kind and schema to prefer native tools over shell
    /// commands that may not exist on the host.
    pub tool_catalog: Vec<ToolEntry>,
    /// Optional free-form context appended to the repair prompt
    /// (e.g. the host environment description).
    pub extra_context: Option<String>,
}

/// The outcome of a repair diagnosis: either the plan is wrong and a
/// constrained patch fixes it, or the world is wrong and a set of human
/// remediation actions fixes the environment while the plan stays untouched.
#[derive(Debug, Clone, PartialEq)]
pub enum RepairProposal {
    /// The plan is defective — a constrained plan edit, pending approval.
    Patch(Box<Patch>),
    /// The plan is fine — the runtime environment violated its assumptions;
    /// the human repairs the world, then resumes the run unchanged.
    WorldFix(Box<WorldFix>),
}

// ─── Spec refinement (REFINE phase) ──────────────────────────────────────────

/// One turn of the refinement conversation. `role` is `"user"` or
/// `"assistant"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecTurn {
    pub role: String,
    pub content: String,
}

/// An intent-assessment request: given the clarification conversation so far,
/// ask the model how complete the spec is and what to ask next.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessRequest {
    /// The original first message from the user.
    pub intent: String,
    /// Full clarification history, including the intent as the first user turn.
    pub conversation: Vec<SpecTurn>,
    /// Tools visible to the eventual plan, so the model knows what's feasible.
    pub tool_catalog: Vec<ToolEntry>,
    /// Optional free-form context (host environment etc.).
    pub extra_context: Option<String>,
}

/// The best-effort spec draft the assessment maintains each turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecDraft {
    pub desired_outcome: String,
    pub acceptance_criteria: Vec<String>,
    /// Values supplied when a plan is triggered or scheduled. Keeping these
    /// explicit during refinement prevents the design phase from turning
    /// invocation parameters into mid-run human-interaction steps.
    #[serde(default)]
    pub inputs: Vec<SpecInput>,
}

/// One invocation-time input identified while refining a workflow spec.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpecInput {
    pub name: String,
    pub description: String,
    pub value_type: String,
    /// Interaction and semantic hint for an invocation-time value. This must
    /// survive refinement so final compilation can bind path inputs to the
    /// right tool argument and UI control.
    #[serde(default)]
    pub input_kind: InputKind,
    pub required: bool,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
}

impl SpecDraft {
    /// Render the compiler-owned specification context supplied to final plan
    /// compilation. App orchestration treats this as opaque prompt context.
    pub fn to_compile_context(&self) -> String {
        let mut out = String::from("Desired outcome: ");
        out.push_str(&self.desired_outcome);
        out.push('\n');
        if !self.acceptance_criteria.is_empty() {
            out.push_str("\nAcceptance criteria:\n");
            for criterion in &self.acceptance_criteria {
                out.push_str(&format!("- {criterion}\n"));
            }
        }
        if !self.inputs.is_empty() {
            out.push_str("\nInvocation inputs (available before execution; preserve these as plan inputs and never collect them with HUMAN_INTERACTION):\n");
            for input in &self.inputs {
                let requirement = if input.required {
                    "required"
                } else {
                    "optional"
                };
                let default = input
                    .default
                    .as_ref()
                    .map_or_else(|| "null".to_owned(), serde_json::Value::to_string);
                out.push_str(&format!(
                    "- `{}` ({}, {}, input_kind {}, default {}): {}\n",
                    input.name,
                    input.value_type,
                    requirement,
                    input_kind_name(&input.input_kind),
                    default,
                    input.description
                ));
            }
        }
        out
    }
}

/// The model's judgement of how ready the spec is for solution design.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentAssessment {
    /// 0.0–1.0: how certain the compiler is that the spec is complete enough
    /// to design a solution.
    pub confidence: f32,
    pub needs_clarification: bool,
    /// The next clarifying question to ask the user (`None` when
    /// `needs_clarification` is false).
    pub question: Option<String>,
    pub spec: SpecDraft,
}

// ─── Solution design (DESIGN phase) ──────────────────────────────────────────

/// A solution-design request: turn an approved spec into a reviewable design.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesignRequest {
    pub spec: SpecDraft,
    /// The refinement conversation, for background on user preferences.
    pub conversation: Vec<SpecTurn>,
    pub tool_catalog: Vec<ToolEntry>,
    /// Set when regenerating: the design the user gave feedback on.
    pub previous_design: Option<SolutionDesign>,
    /// User feedback on `previous_design`.
    pub feedback: Option<String>,
    /// Optional free-form context (host environment etc.).
    pub extra_context: Option<String>,
}

/// A tool recommendation inside a [`SolutionDesign`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecommendedTool {
    pub name: String,
    pub reason: String,
}

/// One step of the execution outline inside a [`SolutionDesign`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutlineStep {
    pub name: String,
    /// Hint like "tool_call", "prompt_call", "code_call", "condition",
    /// "fan_out", "human".
    pub step_kind: String,
    pub description: String,
}

/// Known high-level execution kinds accepted from the solution-design model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlineStepKind {
    ToolCall,
    PromptCall,
    CodeCall,
    AgentCall,
    Condition,
    FanOut,
    Human,
}

impl TryFrom<&str> for OutlineStepKind {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "tool_call" => Ok(Self::ToolCall),
            "prompt_call" => Ok(Self::PromptCall),
            "code_call" => Ok(Self::CodeCall),
            "agent_call" => Ok(Self::AgentCall),
            "condition" => Ok(Self::Condition),
            "fan_out" => Ok(Self::FanOut),
            "human" => Ok(Self::Human),
            _ => Err("unknown outline step kind"),
        }
    }
}

impl OutlineStep {
    /// Parse the model-facing string into the compiler's closed kind set.
    pub fn kind(&self) -> Result<OutlineStepKind, &'static str> {
        OutlineStepKind::try_from(self.step_kind.as_str())
    }
}

/// The compiler's proposed solution design for an approved spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolutionDesign {
    pub title: String,
    pub summary: String,
    pub recommended_tools: Vec<RecommendedTool>,
    pub execution_outline: Vec<OutlineStep>,
}

impl SolutionDesign {
    /// Render the design as nicely formatted markdown for storing on the plan.
    pub fn to_markdown(&self) -> String {
        let mut out = String::with_capacity(512);
        out.push_str(&format!("# {}\n\n", self.title));
        out.push_str(&self.summary);
        out.push('\n');

        if !self.recommended_tools.is_empty() {
            out.push_str("\n## Recommended tools\n\n");
            for tool in &self.recommended_tools {
                out.push_str(&format!("- **{}** — {}\n", tool.name, tool.reason));
            }
        }

        if !self.execution_outline.is_empty() {
            out.push_str("\n## Execution outline\n\n");
            for (i, step) in self.execution_outline.iter().enumerate() {
                out.push_str(&format!(
                    "{}. **{}** (`{}`): {}\n",
                    i + 1,
                    step.name,
                    step.step_kind,
                    step.description
                ));
            }
        }

        out
    }
}

// ─── Trait ────────────────────────────────────────────────────────────────────

/// Lightweight introspection trait for compiler backends.
///
/// This trait is intentionally minimal so it remains object-safe.  Async
/// compile/repair operations are exposed as inherent methods on [`Backend`].
pub trait CompilerBackend {
    fn name(&self) -> &str;
}

// ─── Backend ─────────────────────────────────────────────────────────────────

/// Compiler backend backed by one shared LLM profile.
pub struct Backend {
    profile: LlmProfile,
    completion: Arc<dyn CompletionPort>,
}

impl Backend {
    pub fn from_profile(profile: LlmProfile) -> Result<Self, CompilerError> {
        Self::from_profile_with_port(profile, Arc::new(LlmCompletionPort))
    }

    /// Construct a backend with an explicit completion port.
    pub fn from_profile_with_port(
        profile: LlmProfile,
        completion: Arc<dyn CompletionPort>,
    ) -> Result<Self, CompilerError> {
        profile
            .validate()
            .map_err(|e| CompilerError::Config(e.to_string()))?;
        Ok(Self {
            profile,
            completion,
        })
    }

    pub fn profile(&self) -> &LlmProfile {
        &self.profile
    }

    /// The backend's short identifier, e.g. `"claude"` or `"openai"`.
    pub fn name(&self) -> &str {
        self.profile.label()
    }

    /// Compile a natural-language intent into a validated `Plan`.
    ///
    /// This is the **only** place in the system where an LLM is called at
    /// compile time. The returned plan is a typed artifact; only explicitly
    /// represented runtime model steps may invoke a model during execution.
    pub async fn compile(&self, req: CompileRequest) -> Result<Plan, CompilerError> {
        let system = prompt::build_compile_system_prompt(
            req.allowed_step_types.contains(&StepType::AgentCall),
        );
        let user = prompt::build_compile_user_prompt(&req);
        let raw = self
            .complete_operation(CompilerOperation::Compile, &system, &user, 0)
            .await?;
        let mut plan_json = extractor::extract_plan_json(&raw)?;
        let metadata = build_metadata(&req, &format!("{}:{}", self.name(), self.profile.model));
        plan_json["metadata"] = serde_json::to_value(&metadata)?;
        let plan = serde_json::from_value(plan_json).map_err(|e| {
            invalid_response(
                self.name(),
                format!("plan deserialisation failed: {e}"),
                &raw,
            )
        })?;
        let violations = validate_compiled_plan(&plan, &req);
        if !violations.is_empty() {
            return Err(CompilerError::PlanValidationFailed {
                backend: self.name().to_owned(),
                plan: Box::new(plan),
                errors: violations,
            });
        }
        Ok(plan)
    }

    /// Propose a repair for a failing step.
    ///
    /// The diagnosis stage first decides the failure locus: a plan defect
    /// yields a `Pending` `Patch` (reviewed and approved before it is
    /// applied); a world defect — the plan was reasonable but the runtime
    /// environment violated its assumptions — yields a `WorldFix` describing
    /// how the human should repair the environment before resuming the run
    /// against the unchanged plan. See the repair module for the full loop.
    pub async fn propose_repair(
        &self,
        req: &RepairRequest,
    ) -> Result<RepairProposal, CompilerError> {
        // Stage 1: ask for a compact diagnosis/change plan. This keeps the
        // expensive reasoning separate from the mechanical JSON patch emission.
        let strategy_raw = self
            .complete_operation(
                CompilerOperation::RepairStrategy,
                &prompt::build_repair_strategy_system_prompt(),
                &prompt::build_repair_strategy_user_prompt(req),
                0,
            )
            .await?;
        let strategy_json = extractor::extract_patch_json(&strategy_raw)?;

        // A world-locus diagnosis needs no patch: the plan stays untouched and
        // the remediation goes to the human, so stage 2 is skipped entirely.
        if let Some(world_fix) = parse_world_fix_strategy(&strategy_json, req) {
            return Ok(RepairProposal::WorldFix(Box::new(world_fix)));
        }

        // Stage 2: turn the strategy into constrained patch operations. The
        // implementer is encouraged to use per-step JSON-pointer edits/batches
        // instead of regenerating full steps or plans.
        let patch_raw = self
            .complete_operation(
                CompilerOperation::RepairImplementation,
                &prompt::build_repair_system_prompt(),
                &prompt::build_repair_implementation_user_prompt(req, &strategy_json),
                0,
            )
            .await?;
        let patch_json = extractor::extract_patch_json(&patch_raw)?;
        parse_patch_json(patch_json, req, self.name(), &patch_raw)
            .map(|patch| RepairProposal::Patch(Box::new(patch)))
    }

    /// Make one bounded attempt to correct a proposed patch rejected by the
    /// deterministic patch applicator or plan validator.
    pub async fn correct_patch(
        &self,
        req: &RepairRequest,
        rejected_patch: &Patch,
        validation_errors: &str,
    ) -> Result<Patch, CompilerError> {
        let patch_raw = self
            .complete_operation(
                CompilerOperation::RepairCorrection,
                &prompt::build_repair_system_prompt(),
                &prompt::build_repair_correction_user_prompt(
                    req,
                    rejected_patch,
                    validation_errors,
                ),
                1,
            )
            .await?;
        let patch_json = extractor::extract_patch_json(&patch_raw)?;
        parse_patch_json(patch_json, req, self.name(), &patch_raw)
    }

    /// A single, plain text completion — used for small utility conversions
    /// (e.g. natural language → cron), not for plan compilation.
    pub async fn complete(&self, system: &str, user: &str) -> Result<String, CompilerError> {
        self.complete_operation(CompilerOperation::Utility, system, user, 0)
            .await
    }

    async fn complete_operation(
        &self,
        operation: CompilerOperation,
        system: &str,
        user: &str,
        correction_count: u64,
    ) -> Result<String, CompilerError> {
        let operation_name = operation.as_str();
        let span = tracing::info_span!(
            "compiler.llm.complete",
            compiler.operation = operation_name,
            compiler.backend = self.name(),
            compiler.profile.id = self.profile.id.as_str(),
            compiler.model = self.profile.model.as_str(),
            compiler.correction_count = correction_count,
        );
        let started = Instant::now();
        let result = self
            .completion
            .complete(
                &self.profile,
                CompletionRequest {
                    system: Some(system),
                    user,
                    model: None,
                    max_tokens: self.profile.max_tokens,
                    temperature: self.profile.temperature,
                },
            )
            .instrument(span.clone())
            .await;
        let latency_ms = started.elapsed().as_millis() as u64;

        match result {
            Ok(response) => {
                span.in_scope(|| {
                    tracing::info!(
                        compiler.operation = operation_name,
                        compiler.outcome = "success",
                        compiler.latency_ms = latency_ms,
                        compiler.input_tokens = ?response.input_tokens,
                        compiler.output_tokens = ?response.output_tokens,
                        compiler.correction_count = correction_count,
                        "compiler LLM operation completed"
                    );
                });
                Ok(response.text)
            }
            Err(error) => {
                span.in_scope(|| {
                    tracing::warn!(
                        compiler.operation = operation_name,
                        compiler.outcome = "error",
                        compiler.latency_ms = latency_ms,
                        compiler.correction_count = correction_count,
                        error.kind = llm_error_kind(&error),
                        "compiler LLM operation failed"
                    );
                });
                Err(CompilerError::Api {
                    backend: self.name().to_owned(),
                    message: error.to_string(),
                })
            }
        }
    }

    /// Synthesize a runnable `ToolEntry` from a bare name/description
    /// reference.
    ///
    /// The result is always forced to `allowlisted: false` and its `name`
    /// forced to match the request, regardless of what the model returns —
    /// a freshly invented tool must be reviewed and explicitly enabled
    /// before any plan can call it.
    pub async fn synthesize_tool(
        &self,
        req: ToolSynthesisRequest,
    ) -> Result<ToolEntry, CompilerError> {
        let system = prompt::build_tool_synthesis_system_prompt();
        let user = prompt::build_tool_synthesis_user_prompt(&req);
        let raw = self
            .complete_operation(CompilerOperation::SynthesizeTool, &system, &user, 0)
            .await?;

        let tool_json = extractor::extract_tool_json(&raw)?;
        let mut entry: ToolEntry = serde_json::from_value(tool_json).map_err(|e| {
            invalid_response(
                self.name(),
                format!("tool deserialisation failed: {e}"),
                &raw,
            )
        })?;
        entry.name = req.name.clone();
        entry.allowlisted = false;
        Ok(entry)
    }

    /// Assess how complete a natural-language intent is (REFINE phase).
    ///
    /// Returns an updated spec draft, a confidence score in `[0.0, 1.0]`, and
    /// — when clarification is still needed — the single next question to ask
    /// the user. Confidence is clamped after parsing, and `question` is forced
    /// to `None` whenever `needs_clarification` is false.
    pub async fn assess(&self, req: &AssessRequest) -> Result<IntentAssessment, CompilerError> {
        let system = prompt::build_assess_system_prompt();
        let user = prompt::build_assess_user_prompt(req);
        let raw = self
            .complete_operation(CompilerOperation::Assess, &system, &user, 0)
            .await?;
        let json = extractor::extract_assessment_json(&raw)?;
        let mut assessment: IntentAssessment = serde_json::from_value(json).map_err(|e| {
            invalid_response(
                self.name(),
                format!("assessment deserialisation failed: {e}"),
                &raw,
            )
        })?;
        assessment.confidence = assessment.confidence.clamp(0.0, 1.0);
        if !assessment.needs_clarification {
            assessment.question = None;
        }
        validate_assessment(&assessment, self.name(), &raw)?;
        Ok(assessment)
    }

    /// Produce a solution design for an approved spec (DESIGN phase).
    ///
    /// When `req.previous_design` and `req.feedback` are set, the model is
    /// instructed to revise the previous design rather than start over.
    pub async fn design(&self, req: &DesignRequest) -> Result<SolutionDesign, CompilerError> {
        let system = prompt::build_design_system_prompt();
        let user = prompt::build_design_user_prompt(req);
        let raw = self
            .complete_operation(CompilerOperation::Design, &system, &user, 0)
            .await?;
        let design = self.parse_design(&raw, req)?;

        let Some(previous) = req.previous_design.as_ref() else {
            return Ok(design);
        };
        let missing_kinds = missing_outline_kinds(previous, &design);
        if missing_kinds.is_empty() {
            return Ok(design);
        }

        // A revision is allowed to change topology when the user requested
        // it, but silent structural loss is common in full JSON rewrites.
        // Give the model one focused chance to distinguish those cases and
        // restore untouched outline kinds without inventing application-side
        // step semantics.
        let correction = build_design_continuity_correction(&user, &missing_kinds);
        let corrected_raw = self
            .complete_operation(CompilerOperation::DesignCorrection, &system, &correction, 1)
            .await?;
        self.parse_design(&corrected_raw, req)
    }

    fn parse_design(
        &self,
        raw: &str,
        req: &DesignRequest,
    ) -> Result<SolutionDesign, CompilerError> {
        let json = extractor::extract_design_json(raw)?;
        let design = serde_json::from_value(json).map_err(|e| {
            invalid_response(
                self.name(),
                format!("design deserialisation failed: {e}"),
                raw,
            )
        })?;
        validate_design(&design, req, self.name(), raw)?;
        Ok(design)
    }
}

fn invalid_response(backend: &str, message: impl Into<String>, raw: &str) -> CompilerError {
    CompilerError::InvalidResponse {
        backend: backend.to_owned(),
        message: message.into(),
        raw: diagnostics::safe_model_response(raw),
    }
}

fn validate_compiled_plan(plan: &Plan, req: &CompileRequest) -> Vec<String> {
    let allowed: HashSet<StepType> = req.allowed_step_types.iter().copied().collect();
    let capability_violations = plan
        .steps
        .iter()
        .filter(|step| !allowed.contains(&step.step_type()))
        .map(|step| {
            format!(
                "step '{}' emitted disallowed type {}",
                step.id,
                step.step_type()
            )
        })
        .collect::<Vec<_>>();

    let catalog = ToolCatalog::new(req.tool_catalog.clone());
    let validation_errors = crate::validator::validate(plan, &catalog)
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    let error_count = capability_violations.len() + validation_errors.len();
    tracing::info!(
        compiler.operation = CompilerOperation::Compile.as_str(),
        compiler.validation_error_count = error_count,
        compiler.outcome = if error_count == 0 {
            "validated"
        } else {
            "validation_failed"
        },
        "compiler plan validation completed"
    );

    capability_violations
        .into_iter()
        .chain(validation_errors)
        .collect()
}

fn input_kind_name(input_kind: &InputKind) -> &'static str {
    match input_kind {
        InputKind::Value => "value",
        InputKind::FilePath => "file_path",
        InputKind::OutputFilePath => "output_file_path",
        InputKind::DirectoryPath => "directory_path",
    }
}

fn validate_assessment(
    assessment: &IntentAssessment,
    backend: &str,
    raw: &str,
) -> Result<(), CompilerError> {
    const ALLOWED_INPUT_TYPES: &[&str] = &[
        "string", "number", "integer", "boolean", "object", "array", "any",
    ];
    let mut errors = Vec::new();
    if assessment.spec.desired_outcome.trim().is_empty() {
        errors.push("spec.desired_outcome must not be empty".to_owned());
    }
    if assessment.spec.acceptance_criteria.is_empty() {
        errors.push("spec.acceptance_criteria must contain at least one criterion".to_owned());
    }
    for (index, criterion) in assessment.spec.acceptance_criteria.iter().enumerate() {
        if criterion.trim().is_empty() {
            errors.push(format!(
                "spec.acceptance_criteria[{index}] must not be empty"
            ));
        }
    }
    for (index, input) in assessment.spec.inputs.iter().enumerate() {
        if input.name.trim().is_empty() {
            errors.push(format!("spec.inputs[{index}].name must not be empty"));
        }
        if input.description.trim().is_empty() {
            errors.push(format!(
                "spec.inputs[{index}].description must not be empty"
            ));
        }
        if !ALLOWED_INPUT_TYPES.contains(&input.value_type.as_str()) {
            errors.push(format!(
                "spec.inputs[{index}].value_type '{}' is not supported",
                input.value_type
            ));
        }
        if input.input_kind != InputKind::Value && input.value_type != "string" {
            errors.push(format!(
                "spec.inputs[{index}] input_kind '{}' requires value_type 'string', got '{}'",
                input_kind_name(&input.input_kind),
                input.value_type
            ));
        }
    }
    if assessment.needs_clarification
        && assessment
            .question
            .as_deref()
            .is_none_or(|question| question.trim().is_empty())
    {
        errors.push(
            "question must contain one focused question when clarification is needed".to_owned(),
        );
    }

    tracing::info!(
        compiler.operation = CompilerOperation::Assess.as_str(),
        compiler.validation_error_count = errors.len(),
        compiler.outcome = if errors.is_empty() {
            "validated"
        } else {
            "validation_failed"
        },
        "compiler assessment validation completed"
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(invalid_response(
            backend,
            format!("assessment domain validation failed: {}", errors.join("; ")),
            raw,
        ))
    }
}

fn validate_design(
    design: &SolutionDesign,
    req: &DesignRequest,
    backend: &str,
    raw: &str,
) -> Result<(), CompilerError> {
    const MIN_OUTLINE_STEPS: usize = 2;
    const MAX_OUTLINE_STEPS: usize = 7;

    let mut errors = Vec::new();
    if design.title.trim().is_empty() {
        errors.push("title must not be empty".to_owned());
    }
    if design.summary.trim().is_empty() {
        errors.push("summary must not be empty".to_owned());
    }
    if !(MIN_OUTLINE_STEPS..=MAX_OUTLINE_STEPS).contains(&design.execution_outline.len()) {
        errors.push(format!(
            "execution_outline must contain between {MIN_OUTLINE_STEPS} and {MAX_OUTLINE_STEPS} steps"
        ));
    }

    let known_tools: HashSet<&str> = req
        .tool_catalog
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    for (index, tool) in design.recommended_tools.iter().enumerate() {
        if tool.name.trim().is_empty() {
            errors.push(format!("recommended_tools[{index}].name must not be empty"));
        } else if !known_tools.contains(tool.name.as_str()) {
            errors.push(format!(
                "recommended_tools[{index}] references unknown tool '{}'",
                tool.name
            ));
        }
        if tool.reason.trim().is_empty() {
            errors.push(format!(
                "recommended_tools[{index}].reason must not be empty"
            ));
        }
    }
    for (index, step) in design.execution_outline.iter().enumerate() {
        if step.name.trim().is_empty() {
            errors.push(format!("execution_outline[{index}].name must not be empty"));
        }
        if step.description.trim().is_empty() {
            errors.push(format!(
                "execution_outline[{index}].description must not be empty"
            ));
        }
        if step.kind().is_err() {
            errors.push(format!(
                "execution_outline[{index}].step_kind '{}' is not supported",
                step.step_kind
            ));
        }
    }

    tracing::info!(
        compiler.operation = CompilerOperation::Design.as_str(),
        compiler.validation_error_count = errors.len(),
        compiler.outcome = if errors.is_empty() {
            "validated"
        } else {
            "validation_failed"
        },
        "compiler design validation completed"
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(invalid_response(
            backend,
            format!("design domain validation failed: {}", errors.join("; ")),
            raw,
        ))
    }
}

fn llm_error_kind(error: &LlmError) -> &'static str {
    match error {
        LlmError::Config(_) => "config",
        LlmError::Request(_) => "request",
        LlmError::Http { .. } => "http",
        LlmError::InvalidResponse(_) => "invalid_response",
        LlmError::CliStart { .. } => "cli_start",
        LlmError::CliExit { .. } => "cli_exit",
        LlmError::Timeout { .. } => "timeout",
    }
}

fn outline_kind_counts(design: &SolutionDesign) -> BTreeMap<String, usize> {
    design
        .execution_outline
        .iter()
        .fold(BTreeMap::<String, usize>::new(), |mut counts, step| {
            *counts
                .entry(step.step_kind.to_ascii_lowercase())
                .or_default() += 1;
            counts
        })
}

fn missing_outline_kinds(previous: &SolutionDesign, revised: &SolutionDesign) -> Vec<String> {
    let revised_counts = outline_kind_counts(revised);
    outline_kind_counts(previous)
        .into_iter()
        .filter_map(|(kind, previous_count)| {
            let revised_count = revised_counts.get(&kind).copied().unwrap_or_default();
            (previous_count > revised_count)
                .then(|| format!("{kind} (previously {previous_count}, now {revised_count})"))
        })
        .collect()
}

/// Build `PlanMetadata` for a freshly compiled plan, handling both new plans
/// and re-compilations of an existing plan.
///
/// The LLM is instructed to omit the `metadata` field; this fills it in
/// authoritatively so it is always valid.
fn build_metadata(req: &CompileRequest, compiled_by: &str) -> PlanMetadata {
    let now = chrono::Utc::now();
    match &req.existing_plan {
        None => PlanMetadata {
            id: uuid::Uuid::new_v4().to_string(),
            version: 1,
            created_at: now,
            updated_at: now,
            compiled_by: Some(compiled_by.to_owned()),
            intent: Some(req.intent.clone()),
            parent_plan_id: None,
            parent_version: None,
            status: Default::default(),
            solution_design: None,
        },
        Some(existing) => PlanMetadata {
            id: existing.metadata.id.clone(),
            version: existing.metadata.version + 1,
            created_at: existing.metadata.created_at,
            updated_at: now,
            compiled_by: Some(compiled_by.to_owned()),
            intent: Some(req.intent.clone()),
            parent_plan_id: Some(existing.metadata.id.clone()),
            parent_version: Some(existing.metadata.version),
            status: existing.metadata.status,
            solution_design: existing.metadata.solution_design.clone(),
        },
    }
}

/// Interpret a repair strategy whose failure locus is the world, not the plan.
///
/// Returns `Some(WorldFix)` only when the strategy explicitly declares
/// `"failure_locus": "world"`, proposes no plan changes, and carries at least
/// one usable remediation action. Anything else — including a malformed or
/// contradictory strategy — falls through to the normal patch path, which has
/// its own deterministic preflight and bounded correction.
fn parse_world_fix_strategy(
    strategy_json: &serde_json::Value,
    req: &RepairRequest,
) -> Option<WorldFix> {
    if strategy_json["failure_locus"].as_str() != Some("world") {
        return None;
    }
    let has_plan_changes = strategy_json["changes"]
        .as_array()
        .is_some_and(|changes| !changes.is_empty());
    if has_plan_changes {
        // A strategy that edits the plan is a plan repair regardless of the
        // declared locus; the patch path handles it.
        return None;
    }
    let remediation: Vec<RemediationAction> = strategy_json["world_remediation"]
        .as_array()?
        .iter()
        .filter_map(|action| {
            let description = action["description"].as_str()?.trim().to_owned();
            if description.is_empty() {
                return None;
            }
            let command = action["command"]
                .as_str()
                .map(str::trim)
                .filter(|command| !command.is_empty())
                .map(str::to_owned);
            Some(RemediationAction {
                description,
                command,
            })
        })
        .collect();
    if remediation.is_empty() {
        return None;
    }
    let diagnosis = strategy_json["diagnosis"]
        .as_str()
        .unwrap_or("(no diagnosis provided)")
        .to_owned();
    Some(WorldFix::new(
        &req.plan.metadata.id,
        req.plan.metadata.version,
        &req.run_id,
        &req.failing_step_id,
        diagnosis,
        remediation,
    ))
}

/// Deserialise the `{ "operation": {...}, "rationale": "..." }` value produced
/// by the repair prompt into a `Patch`.
fn parse_patch_json(
    patch_json: serde_json::Value,
    req: &RepairRequest,
    backend: &str,
    raw: &str,
) -> Result<Patch, CompilerError> {
    let operation: PatchOperation = serde_json::from_value(patch_json["operation"].clone())
        .map_err(|e| {
            invalid_response(
                backend,
                format!("patch operation deserialisation failed: {e}"),
                raw,
            )
        })?;

    let rationale = patch_json["rationale"]
        .as_str()
        .unwrap_or("(no rationale provided)")
        .to_owned();

    Ok(Patch::new(
        &req.plan.metadata.id,
        req.plan.metadata.version,
        &req.run_id,
        &req.failing_step_id,
        operation,
        rationale,
    ))
}

fn build_design_continuity_correction(user_prompt: &str, missing_kinds: &[String]) -> String {
    format!(
        "{user_prompt}\n\n## Revision continuity check\n\
         The proposed revision reduced or removed these outline step kinds: {}. \
         Re-read the user's feedback. If it did not explicitly request each structural \
         change, restore the affected steps and preserve their topology, including \
         fan-out boundaries. If the feedback did explicitly request the change, keep it. \
         Return the complete revised design JSON only.",
        missing_kinds.join(", ")
    )
}

impl CompilerBackend for Backend {
    fn name(&self) -> &str {
        Backend::name(self)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{ConditionConfig, PlanStep, StepConfig, ToolCallConfig};
    use indexmap::IndexMap;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct ScriptedCompletionPort {
        responses: Mutex<VecDeque<Result<CompletionResponse, LlmError>>>,
    }

    impl ScriptedCompletionPort {
        fn new(responses: Vec<Result<CompletionResponse, LlmError>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
            }
        }
    }

    impl CompletionPort for ScriptedCompletionPort {
        fn complete<'a>(
            &'a self,
            _profile: &'a LlmProfile,
            _request: CompletionRequest<'a>,
        ) -> CompletionFuture<'a> {
            Box::pin(async move {
                self.responses
                    .lock()
                    .expect("scripted completion lock poisoned")
                    .pop_front()
                    .expect("scripted completion response missing")
            })
        }
    }

    fn response(text: impl Into<String>) -> Result<CompletionResponse, LlmError> {
        Ok(CompletionResponse {
            text: text.into(),
            input_tokens: Some(12),
            output_tokens: Some(8),
        })
    }

    fn backend_with_responses(responses: Vec<Result<CompletionResponse, LlmError>>) -> Backend {
        Backend::from_profile_with_port(
            LlmProfile {
                id: "test-profile".to_owned(),
                name: "Test profile".to_owned(),
                protocol: crate::llm::LlmProtocol::OpenAiChat,
                model: "test-model".to_owned(),
                base_url: "https://llm.example.test/v1".to_owned(),
                api_key: "not-used".to_owned(),
                auth: crate::llm::LlmAuth::Bearer,
                headers: Default::default(),
                executable: String::new(),
                command_template: String::new(),
                max_tokens: Some(1_024),
                temperature: Some(0.0),
                timeout_secs: 30,
                codex_sandbox_mode: crate::llm::CodexSandboxMode::default(),
            },
            Arc::new(ScriptedCompletionPort::new(responses)),
        )
        .unwrap()
    }

    fn minimal_plan() -> Plan {
        Plan {
            metadata: PlanMetadata {
                id: "plan-1".to_owned(),
                version: 3,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                compiled_by: None,
                intent: None,
                parent_plan_id: None,
                parent_version: None,
                status: Default::default(),
                solution_design: None,
            },
            name: "test-plan".to_owned(),
            description: None,
            inputs: vec![],
            config: IndexMap::new(),
            steps: vec![PlanStep {
                id: "call_api".to_owned(),
                name: "Call the API".to_owned(),
                description: None,
                config: StepConfig::ToolCall(ToolCallConfig {
                    tool: "http_get".to_owned(),
                    arguments: IndexMap::new(),
                }),
                depends_on: vec![],
                outputs: vec![],
                timeout_secs: None,
                retry: None,
            }],
            outputs: vec![],
        }
    }

    fn compile_request(existing_plan: Option<Plan>) -> CompileRequest {
        CompileRequest {
            intent: "fetch the BTC price".to_owned(),
            allowed_step_types: vec![StepType::ToolCall],
            tool_catalog: vec![],
            existing_plan,
            run_history: vec![],
            extra_context: None,
        }
    }

    fn repair_request() -> RepairRequest {
        RepairRequest {
            plan: minimal_plan(),
            run_id: "run-9".to_owned(),
            failing_step_id: "call_api".to_owned(),
            error_message: "boom".to_owned(),
            stdout: None,
            stderr: None,
            runtime_inputs: serde_json::Value::Null,
            dependency_outputs: Default::default(),
            tool_catalog: vec![],
            extra_context: None,
        }
    }

    #[test]
    fn metadata_for_a_new_plan_starts_at_version_one_with_no_parent() {
        let metadata = build_metadata(&compile_request(None), "claude:claude-sonnet-4-6");

        assert_eq!(metadata.version, 1);
        assert_eq!(
            metadata.compiled_by.as_deref(),
            Some("claude:claude-sonnet-4-6")
        );
        assert_eq!(metadata.intent.as_deref(), Some("fetch the BTC price"));
        assert!(metadata.parent_plan_id.is_none());
        assert!(metadata.parent_version.is_none());
    }

    #[test]
    fn metadata_for_a_recompiled_plan_keeps_the_id_and_increments_the_version() {
        let existing = minimal_plan();
        let created_at = existing.metadata.created_at;

        let metadata = build_metadata(&compile_request(Some(existing)), "openai:gpt-5");

        assert_eq!(metadata.id, "plan-1");
        assert_eq!(metadata.version, 4);
        assert_eq!(metadata.created_at, created_at);
        assert_eq!(metadata.parent_plan_id.as_deref(), Some("plan-1"));
        assert_eq!(metadata.parent_version, Some(3));
    }

    #[test]
    fn world_locus_strategy_without_plan_changes_becomes_a_world_fix() {
        let strategy = serde_json::json!({
            "diagnosis": "The branch has no staged changes; the commit step's precondition is unmet.",
            "failure_locus": "world",
            "changes": [],
            "world_remediation": [
                { "description": "Stage the intended files", "command": "git add -A" },
                { "description": "Verify the working tree actually contains the edits", "command": null }
            ],
            "risks": []
        });

        let fix = parse_world_fix_strategy(&strategy, &repair_request())
            .expect("world locus with remediation must yield a world fix");

        assert_eq!(fix.plan_id, "plan-1");
        assert_eq!(fix.plan_version, 3);
        assert_eq!(fix.run_id, "run-9");
        assert_eq!(fix.failing_step_id, "call_api");
        assert_eq!(fix.remediation.len(), 2);
        assert_eq!(fix.remediation[0].command.as_deref(), Some("git add -A"));
        assert_eq!(fix.remediation[1].command, None);
    }

    #[test]
    fn world_locus_strategy_with_plan_changes_falls_through_to_the_patch_path() {
        let strategy = serde_json::json!({
            "diagnosis": "contradictory strategy",
            "failure_locus": "world",
            "changes": [ { "step_id": "call_api", "operation_hint": "set_step_field" } ],
            "world_remediation": [ { "description": "also fix the world" } ],
        });

        assert!(parse_world_fix_strategy(&strategy, &repair_request()).is_none());
    }

    #[test]
    fn plan_locus_and_empty_remediation_never_yield_a_world_fix() {
        let plan_locus = serde_json::json!({
            "diagnosis": "bad pointer",
            "failure_locus": "plan",
            "changes": [],
            "world_remediation": [ { "description": "irrelevant" } ],
        });
        assert!(parse_world_fix_strategy(&plan_locus, &repair_request()).is_none());

        let empty_remediation = serde_json::json!({
            "diagnosis": "world broke",
            "failure_locus": "world",
            "changes": [],
            "world_remediation": [ { "description": "   " } ],
        });
        assert!(parse_world_fix_strategy(&empty_remediation, &repair_request()).is_none());
    }

    #[test]
    fn patch_json_with_operation_and_rationale_becomes_a_pending_patch() {
        let patch_json = serde_json::json!({
            "operation": {
                "op": "set_step_field",
                "step_id": "call_api",
                "pointer": "/config/tool",
                "value": "http_post"
            },
            "rationale": "the endpoint requires POST"
        });

        let patch = parse_patch_json(patch_json, &repair_request(), "claude", "raw").unwrap();

        assert_eq!(patch.plan_id, "plan-1");
        assert_eq!(patch.plan_version, 3);
        assert_eq!(patch.run_id, "run-9");
        assert_eq!(patch.failing_step_id, "call_api");
        assert_eq!(patch.rationale, "the endpoint requires POST");
        assert!(matches!(
            patch.operation,
            PatchOperation::SetStepField { ref step_id, .. } if step_id == "call_api"
        ));
    }

    #[test]
    fn patch_json_without_a_rationale_gets_a_placeholder() {
        let patch_json = serde_json::json!({
            "operation": { "op": "remove_step_field", "step_id": "call_api", "pointer": "/config/obsolete" }
        });

        let patch = parse_patch_json(patch_json, &repair_request(), "claude", "raw").unwrap();

        assert_eq!(patch.rationale, "(no rationale provided)");
    }

    #[test]
    fn malformed_patch_operation_maps_to_invalid_response_with_the_raw_output() {
        let patch_json = serde_json::json!({
            "operation": { "op": "not_a_real_op" },
            "rationale": "r"
        });

        let err = parse_patch_json(patch_json, &repair_request(), "claude", "the raw output")
            .unwrap_err();

        match err {
            CompilerError::InvalidResponse { backend, raw, .. } => {
                assert_eq!(backend, "claude");
                assert!(raw.contains("hash=fnv1a64:"));
                assert!(raw.contains("the raw output"));
            }
            other => panic!("expected InvalidResponse, got: {other}"),
        }
    }

    #[tokio::test]
    async fn compile_rejects_a_step_type_outside_the_request_capability_set() {
        let raw = serde_json::to_string(&minimal_plan()).unwrap();
        let backend = backend_with_responses(vec![response(raw)]);
        let request = CompileRequest {
            intent: "do not call tools".to_owned(),
            allowed_step_types: vec![StepType::HumanInteraction],
            tool_catalog: vec![],
            existing_plan: None,
            run_history: vec![],
            extra_context: None,
        };

        let error = backend.compile(request).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("emitted disallowed type TOOL_CALL")
        );
    }

    #[tokio::test]
    async fn compile_enforces_full_deterministic_plan_validation() {
        let raw = serde_json::to_string(&minimal_plan()).unwrap();
        let backend = backend_with_responses(vec![response(raw)]);
        let request = CompileRequest {
            intent: "call a configured tool".to_owned(),
            allowed_step_types: vec![StepType::ToolCall],
            tool_catalog: vec![],
            existing_plan: None,
            run_history: vec![],
            extra_context: None,
        };

        let error = backend.compile(request).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("references tool 'http_get' which is not in the catalog")
        );
    }

    #[tokio::test]
    async fn assess_rejects_inconsistent_clarification_contracts() {
        let backend = backend_with_responses(vec![response(
            serde_json::json!({
                "confidence": 0.4,
                "needs_clarification": true,
                "question": null,
                "spec": {
                    "desired_outcome": "Create a report",
                    "acceptance_criteria": ["The report exists"],
                    "inputs": []
                }
            })
            .to_string(),
        )]);
        let request = AssessRequest {
            intent: "create a report".to_owned(),
            conversation: vec![],
            tool_catalog: vec![],
            extra_context: None,
        };

        let error = backend.assess(&request).await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("question must contain one focused question")
        );
    }

    #[tokio::test]
    async fn design_rejects_unknown_tools_kinds_and_invalid_cardinality() {
        let backend = backend_with_responses(vec![response(
            serde_json::json!({
                "title": "Report",
                "summary": "Create it.",
                "recommended_tools": [{"name": "invented", "reason": "write"}],
                "execution_outline": [{
                    "name": "Do it",
                    "step_kind": "agent",
                    "description": "Run an agent."
                }]
            })
            .to_string(),
        )]);
        let request = DesignRequest {
            spec: SpecDraft {
                desired_outcome: "Create a report".to_owned(),
                acceptance_criteria: vec!["The report exists".to_owned()],
                inputs: vec![],
            },
            conversation: vec![],
            tool_catalog: vec![],
            previous_design: None,
            feedback: None,
            extra_context: None,
        };

        let error = backend.design(&request).await.unwrap_err();
        let message = error.to_string();

        assert!(message.contains("between 2 and 7 steps"));
        assert!(message.contains("unknown tool 'invented'"));
        assert!(message.contains("step_kind 'agent' is not supported"));
    }

    #[test]
    fn outline_kind_accepts_agent_call() {
        let step = OutlineStep {
            name: "Implement change".to_owned(),
            step_kind: "agent_call".to_owned(),
            description: "Inspect, edit, and verify the workspace.".to_owned(),
        };

        assert_eq!(step.kind(), Ok(OutlineStepKind::AgentCall));
    }

    #[tokio::test]
    async fn completion_transport_errors_map_without_exposing_payloads() {
        let backend = backend_with_responses(vec![Err(LlmError::Request(
            "transport unavailable".to_owned(),
        ))]);

        let error = backend
            .complete("system-secret", "user-secret")
            .await
            .unwrap_err();

        assert!(matches!(error, CompilerError::Api { .. }));
        assert!(!error.to_string().contains("system-secret"));
        assert!(!error.to_string().contains("user-secret"));
    }

    #[tokio::test]
    async fn synthesize_tool_uses_the_injected_port_and_forces_safety_fields() {
        let backend = backend_with_responses(vec![response(
            serde_json::json!({
                "name": "model-chosen-name",
                "description": "Fetch a resource",
                "config": {
                    "kind": "http",
                    "base_url": "https://REPLACE_ME.example.com",
                    "method": "GET",
                    "path_template": "",
                    "headers": {},
                    "timeout_secs": null
                },
                "input_schema": {"type": "object"},
                "output_schema": {"type": "object"},
                "allowlisted": true,
                "timeout_secs": null
            })
            .to_string(),
        )]);

        let tool = backend
            .synthesize_tool(ToolSynthesisRequest {
                name: "requested-name".to_owned(),
                description: "Fetch a resource".to_owned(),
                input_schema: serde_json::json!({"type":"object"}),
                output_schema: serde_json::json!({"type":"object"}),
                kind_hint: Some(ToolKind::Http),
                extra_context: None,
            })
            .await
            .unwrap();

        assert_eq!(tool.name, "requested-name");
        assert!(!tool.allowlisted);
    }

    fn design_with_kinds(kinds: &[&str]) -> SolutionDesign {
        SolutionDesign {
            title: "Research dossier".to_owned(),
            summary: "Build the dossier.".to_owned(),
            recommended_tools: Vec::new(),
            execution_outline: kinds
                .iter()
                .enumerate()
                .map(|(index, kind)| OutlineStep {
                    name: format!("Step {index}"),
                    step_kind: (*kind).to_owned(),
                    description: "Execute this stage.".to_owned(),
                })
                .collect(),
        }
    }

    #[test]
    fn design_continuity_check_reports_removed_fan_out_structure() {
        let previous = design_with_kinds(&["code_call", "fan_out", "fan_out", "prompt_call"]);
        let revised = design_with_kinds(&["code_call", "prompt_call"]);

        let missing = missing_outline_kinds(&previous, &revised);

        assert_eq!(missing, vec!["fan_out (previously 2, now 0)"]);
        let correction = build_design_continuity_correction("original prompt", &missing);
        assert!(correction.contains("fan_out (previously 2, now 0)"));
        assert!(correction.contains("restore the affected steps"));
    }

    #[test]
    fn design_continuity_check_allows_preserved_structure() {
        let previous = design_with_kinds(&["code_call", "fan_out", "prompt_call"]);
        let revised = design_with_kinds(&["code_call", "fan_out", "prompt_call"]);

        assert!(missing_outline_kinds(&previous, &revised).is_empty());
    }

    #[test]
    fn solution_design_to_markdown_renders_all_sections() {
        let design = SolutionDesign {
            title: "BTC price logger".to_owned(),
            summary: "Fetches the current BTC price and appends it to a file.".to_owned(),
            recommended_tools: vec![
                RecommendedTool {
                    name: "http_get".to_owned(),
                    reason: "fetch the ticker endpoint".to_owned(),
                },
                RecommendedTool {
                    name: "fs_write".to_owned(),
                    reason: "append the price to the output file".to_owned(),
                },
            ],
            execution_outline: vec![
                OutlineStep {
                    name: "Fetch price".to_owned(),
                    step_kind: "tool_call".to_owned(),
                    description: "GET the BTC/USD ticker".to_owned(),
                },
                OutlineStep {
                    name: "Append to file".to_owned(),
                    step_kind: "tool_call".to_owned(),
                    description: "write the price with a timestamp".to_owned(),
                },
            ],
        };
        let md = design.to_markdown();
        assert!(md.starts_with("# BTC price logger\n"));
        assert!(md.contains("Fetches the current BTC price"));
        assert!(md.contains("## Recommended tools"));
        assert!(md.contains("- **http_get** — fetch the ticker endpoint"));
        assert!(md.contains("- **fs_write** — append the price to the output file"));
        assert!(md.contains("## Execution outline"));
        assert!(md.contains("1. **Fetch price** (`tool_call`): GET the BTC/USD ticker"));
        assert!(
            md.contains("2. **Append to file** (`tool_call`): write the price with a timestamp")
        );
    }

    #[test]
    fn solution_design_to_markdown_omits_empty_sections() {
        let design = SolutionDesign {
            title: "Minimal".to_owned(),
            summary: "Nothing else.".to_owned(),
            recommended_tools: vec![],
            execution_outline: vec![],
        };
        let md = design.to_markdown();
        assert!(md.contains("# Minimal"));
        assert!(md.contains("Nothing else."));
        assert!(!md.contains("## Recommended tools"));
        assert!(!md.contains("## Execution outline"));
    }

    #[test]
    fn solution_design_round_trips_through_serde() {
        let design = SolutionDesign {
            title: "t".to_owned(),
            summary: "s".to_owned(),
            recommended_tools: vec![RecommendedTool {
                name: "http_get".to_owned(),
                reason: "r".to_owned(),
            }],
            execution_outline: vec![OutlineStep {
                name: "n".to_owned(),
                step_kind: "code_call".to_owned(),
                description: "d".to_owned(),
            }],
        };
        let json = serde_json::to_value(&design).unwrap();
        assert_eq!(json["recommended_tools"][0]["name"], "http_get");
        assert_eq!(json["execution_outline"][0]["step_kind"], "code_call");
        let back: SolutionDesign = serde_json::from_value(json).unwrap();
        assert_eq!(back.title, design.title);
    }

    #[test]
    fn legacy_spec_draft_without_inputs_deserializes_with_empty_contract() {
        let spec: SpecDraft = serde_json::from_value(serde_json::json!({
            "desired_outcome": "Summarize articles",
            "acceptance_criteria": ["summary is evidence-backed"]
        }))
        .unwrap();

        assert!(spec.inputs.is_empty());
    }

    #[test]
    fn legacy_spec_input_without_kind_defaults_to_value() {
        let input: SpecInput = serde_json::from_value(serde_json::json!({
            "name": "query",
            "description": "Search phrase",
            "value_type": "string",
            "required": true,
            "default": null
        }))
        .unwrap();

        assert_eq!(input.input_kind, InputKind::Value);
    }

    #[test]
    fn spec_compile_context_marks_inputs_as_pre_execution_values() {
        let spec = SpecDraft {
            desired_outcome: "Create a dossier".to_owned(),
            acceptance_criteria: vec!["Evidence is cited".to_owned()],
            inputs: vec![SpecInput {
                name: "listing_url".to_owned(),
                description: "Article listing".to_owned(),
                value_type: "string".to_owned(),
                input_kind: InputKind::Value,
                required: true,
                default: None,
            }],
        };

        let context = spec.to_compile_context();
        assert!(context.contains("Desired outcome: Create a dossier"));
        assert!(context.contains("- Evidence is cited"));
        assert!(
            context.contains("`listing_url` (string, required, input_kind value, default null)")
        );
        assert!(context.contains("never collect them with HUMAN_INTERACTION"));
    }

    #[test]
    fn assessment_rejects_path_kind_for_non_string_input() {
        let assessment = IntentAssessment {
            confidence: 1.0,
            needs_clarification: false,
            question: None,
            spec: SpecDraft {
                desired_outcome: "Write a report".to_owned(),
                acceptance_criteria: vec!["A report exists".to_owned()],
                inputs: vec![SpecInput {
                    name: "output_path".to_owned(),
                    description: "Destination report path".to_owned(),
                    value_type: "integer".to_owned(),
                    input_kind: InputKind::OutputFilePath,
                    required: true,
                    default: None,
                }],
            },
        };

        let error = validate_assessment(&assessment, "test", "raw").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("input_kind 'output_file_path' requires value_type 'string'")
        );
    }

    fn write_file_tool() -> ToolEntry {
        ToolEntry {
            name: "write_file".to_owned(),
            description: "Write a file".to_owned(),
            config: crate::tools::catalog::ToolConfig::Http(crate::tools::catalog::HttpConfig {
                base_url: "https://example.invalid".to_owned(),
                method: "POST".to_owned(),
                path_template: "/write".to_owned(),
                headers: IndexMap::new(),
                timeout_secs: None,
            }),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
            output_schema: serde_json::json!({ "type": "object" }),
            allowlisted: true,
            timeout_secs: None,
        }
    }

    fn write_file_plan(input_required: bool, default: Option<serde_json::Value>) -> Plan {
        let mut plan = minimal_plan();
        plan.inputs.push(crate::plan::types::PlanInput {
            name: "output_path".to_owned(),
            description: Some("Destination file".to_owned()),
            value_type: "string".to_owned(),
            required: input_required,
            default,
            input_kind: InputKind::OutputFilePath,
        });
        let StepConfig::ToolCall(config) = &mut plan.steps[0].config else {
            panic!("minimal plan must use a tool call");
        };
        config.tool = "write_file".to_owned();
        config
            .arguments
            .insert("path".to_owned(), serde_json::json!("${input.output_path}"));
        plan
    }

    fn condition_routed_write_file_plan() -> Plan {
        let mut plan = write_file_plan(false, None);
        plan.steps[0].id = "write".to_owned();
        plan.steps[0].depends_on = vec!["has_output_path".to_owned()];
        plan.steps.insert(
            0,
            PlanStep {
                id: "has_output_path".to_owned(),
                name: "Check whether an output path was supplied".to_owned(),
                description: None,
                config: StepConfig::Condition(ConditionConfig {
                    expression: "${input.output_path}".to_owned(),
                    true_steps: vec!["write".to_owned()],
                    false_steps: vec![],
                }),
                depends_on: vec![],
                outputs: vec![],
                timeout_secs: None,
                retry: None,
            },
        );
        plan
    }

    #[tokio::test]
    async fn compiler_validation_acceptance_matrix() {
        struct AcceptanceCase {
            name: &'static str,
            plan: Plan,
            tool_catalog: Vec<ToolEntry>,
            allowed_step_types: Vec<StepType>,
            accepted: bool,
            expected_error: Option<&'static str>,
        }

        let cases = vec![
            AcceptanceCase {
                name: "condition-routed optional input may bind a required non-nullable tool argument",
                plan: condition_routed_write_file_plan(),
                tool_catalog: vec![write_file_tool()],
                allowed_step_types: vec![StepType::ToolCall, StepType::Condition],
                accepted: true,
                expected_error: None,
            },
            AcceptanceCase {
                name: "optional input with a concrete default safely satisfies a required non-nullable tool argument",
                plan: write_file_plan(false, Some(serde_json::json!("/tmp/report.txt"))),
                tool_catalog: vec![write_file_tool()],
                allowed_step_types: vec![StepType::ToolCall],
                accepted: true,
                expected_error: None,
            },
            AcceptanceCase {
                name: "optional input without a default cannot satisfy a required non-nullable tool argument",
                plan: write_file_plan(false, None),
                tool_catalog: vec![write_file_tool()],
                allowed_step_types: vec![StepType::ToolCall],
                accepted: false,
                expected_error: Some("optional without a concrete compatible default"),
            },
            AcceptanceCase {
                name: "required tool argument cannot be omitted",
                plan: {
                    let mut plan = write_file_plan(true, None);
                    let StepConfig::ToolCall(config) = &mut plan.steps[0].config else {
                        panic!("write file plan must use a tool call");
                    };
                    config.arguments.clear();
                    plan
                },
                tool_catalog: vec![write_file_tool()],
                allowed_step_types: vec![StepType::ToolCall],
                accepted: false,
                expected_error: Some("missing required argument 'path'"),
            },
        ];

        for case in cases {
            let backend = backend_with_responses(vec![response(
                serde_json::to_string(&case.plan).expect("test plan serialises"),
            )]);
            let result = backend
                .compile(CompileRequest {
                    intent: case.name.to_owned(),
                    allowed_step_types: case.allowed_step_types,
                    tool_catalog: case.tool_catalog,
                    existing_plan: None,
                    run_history: vec![],
                    extra_context: None,
                })
                .await;

            match (case.accepted, result) {
                (true, Ok(_)) => {}
                (false, Err(error)) => assert!(
                    error.to_string().contains(
                        case.expected_error
                            .expect("rejected case has an error expectation")
                    ),
                    "{}: unexpected compiler error: {error}",
                    case.name
                ),
                (true, Err(error)) => panic!("{}: expected acceptance, got {error}", case.name),
                (false, Ok(_)) => panic!("{}: expected rejection", case.name),
            }
        }
    }
}
