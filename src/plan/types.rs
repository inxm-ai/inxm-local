//! Plan IR — the compiled workflow artifact.
//!
//! A `Plan` is the output of the compiler and the input to the executor.
//! It is serialised to JSON and stored versioned in `.inxm/plans/`.

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// The well-known plan input name that carries the working directory for
/// every command-executing step (CODE_CALL and AGENT_CALL). Plans that run
/// commands must declare this input — see [`Plan::requires_root_directory`]
/// and the root-directory check in `validator`. CODE_CALL plans may declare it
/// as `required: false` with no/null default (the executor then falls back to
/// an app-managed per-run scratch workspace). AGENT_CALL plans require an
/// explicit caller-supplied directory because agents can make arbitrary
/// workspace changes. The plan IR itself treats both shapes as ordinary
/// optional/required inputs; the validator enforces the step-specific rule.
pub const ROOT_DIRECTORY_INPUT: &str = "root_directory";

// ─── Step types ───────────────────────────────────────────────────────────────

/// The set of step types legal in the v1 runtime.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StepType {
    ToolCall,
    CodeCall,
    HumanInteraction,
    FanOut,
    FanIn,
    PromptCall,
    Condition,
    AgentCall,
}

impl std::fmt::Display for StepType {
    // Must stay in sync with the SCREAMING_SNAKE_CASE serde tags above —
    // display output is used in logs and error messages that refer to the
    // serialized plan format. Guarded by a test in `step_config_serde_tests`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            StepType::ToolCall => "TOOL_CALL",
            StepType::CodeCall => "CODE_CALL",
            StepType::HumanInteraction => "HUMAN_INTERACTION",
            StepType::FanOut => "FAN_OUT",
            StepType::FanIn => "FAN_IN",
            StepType::PromptCall => "PROMPT_CALL",
            StepType::Condition => "CONDITION",
            StepType::AgentCall => "AGENT_CALL",
        };
        write!(f, "{s}")
    }
}

// ─── Step configs ─────────────────────────────────────────────────────────────

/// Configuration for a TOOL_CALL step.
///
/// `tool` must match a name in the active tool catalog.
/// `arguments` values may contain `${conf.*}` or `${step.<id>.<output>}` placeholders.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCallConfig {
    pub tool: String,
    #[serde(default)]
    pub arguments: IndexMap<String, serde_json::Value>,
}

/// Configuration for a CODE_CALL step.
///
/// Either `inline` or `file` must be set, not both.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CodeCallConfig {
    pub language: String,
    /// Inline script body.
    pub inline: Option<String>,
    /// Path to a script file (relative to the working dir).
    pub file: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional text streamed to the child process on standard input.
    /// Use this for dependency outputs that may be large.
    #[serde(default)]
    pub stdin: Option<String>,
    #[serde(default)]
    pub env: IndexMap<String, String>,
    pub working_dir: Option<String>,
    pub timeout_secs: Option<u64>,
}

/// Configuration for a PROMPT_CALL step.
///
/// Must be tool-free: no tool access, no agent loop, single model call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PromptCallConfig {
    /// Fully-qualified model identifier, e.g. `"claude-sonnet-4-6"`.
    pub model: String,
    pub system_prompt: Option<String>,
    pub user_prompt: String,
    /// The field name in `step.outputs` where the model response will be stored.
    pub output_field: String,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

/// Configuration for an AGENT_CALL step.
///
/// Unlike a bounded prompt completion or a fixed script, an agent call runs a
/// tool-using coding agent in a real workspace. `working_dir` is deliberately
/// required in the serialized form: it must resolve from the plan's required
/// [`ROOT_DIRECTORY_INPUT`] so an agent is never launched in an implicit
/// process directory. The validator enforces that reference relationship.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentCallConfig {
    /// The outcome the coding agent should accomplish.
    pub objective: String,
    /// Workspace path, normally exactly `"${input.root_directory}"` or a path
    /// derived from it.
    pub working_dir: String,
    /// Maximum wall-clock time for the agent process.
    pub timeout_secs: Option<u64>,
}

/// Configuration for a HUMAN_INTERACTION step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HumanInteractionConfig {
    /// The prompt text to display to the human operator.
    pub prompt: String,
    /// Field name in `step.outputs` where the response will be stored.
    pub response_field: String,
    /// If true, execution pauses until the human explicitly approves (not just responds).
    #[serde(default)]
    pub approval_required: bool,
}

/// Configuration for a FAN_OUT step.
///
/// Iterates over a list and spawns one instance of `spawn_steps` per item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FanOutConfig {
    /// Reference to a dependency output list: `"<step-id>.<output-name>"`.
    pub over: String,
    /// Variable name injected into each spawned step's context as `${item.<var>}`.
    pub item_var: String,
    /// IDs of steps in the plan that act as the fan-out body.
    #[serde(default)]
    pub spawn_steps: Vec<String>,
    /// Optional condition evaluated after each iteration. The fan-out stops
    /// after the first truthy match, while `over` remains the hard iteration
    /// bound. Expressions use the same grammar as CONDITION steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

/// Configuration for a FAN_IN step.
///
/// Waits for a set of steps to complete and aggregates their outputs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FanInConfig {
    /// IDs of steps whose outputs to collect.
    pub from_steps: Vec<String>,
    /// Field name in `step.outputs` where the aggregated list is stored.
    pub collect_field: String,
}

/// Configuration for a CONDITION step.
///
/// Evaluates a simple equality/comparison expression and routes execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConditionConfig {
    /// Expression to evaluate. Supports `${ref} == value`, `${ref} != value`.
    pub expression: String,
    /// Step IDs to execute when the condition is true.
    #[serde(default)]
    pub true_steps: Vec<String>,
    /// Step IDs to execute when the condition is false (may be empty).
    #[serde(default)]
    pub false_steps: Vec<String>,
}

/// Discriminated union of step configurations.
///
/// The `"type"` tag drives deserialization so the JSON representation is:
/// `{ "type": "TOOL_CALL", "tool": "...", "arguments": { ... } }`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StepConfig {
    ToolCall(ToolCallConfig),
    CodeCall(CodeCallConfig),
    HumanInteraction(HumanInteractionConfig),
    FanOut(FanOutConfig),
    FanIn(FanInConfig),
    PromptCall(PromptCallConfig),
    Condition(ConditionConfig),
    AgentCall(AgentCallConfig),
}

impl StepConfig {
    pub fn step_type(&self) -> StepType {
        match self {
            StepConfig::ToolCall(_) => StepType::ToolCall,
            StepConfig::CodeCall(_) => StepType::CodeCall,
            StepConfig::HumanInteraction(_) => StepType::HumanInteraction,
            StepConfig::FanOut(_) => StepType::FanOut,
            StepConfig::FanIn(_) => StepType::FanIn,
            StepConfig::PromptCall(_) => StepType::PromptCall,
            StepConfig::Condition(_) => StepType::Condition,
            StepConfig::AgentCall(_) => StepType::AgentCall,
        }
    }
}

// ─── Plan step ────────────────────────────────────────────────────────────────

/// A bounded retry policy for a step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub delay_secs: u64,
    #[serde(default)]
    pub backoff: bool,
}

/// A value supplied whenever a plan is run or scheduled.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanInput {
    pub name: String,
    pub description: Option<String>,
    /// JSON Schema type: `string`, `number`, `integer`, `boolean`, `object`, `array`, or `any`.
    #[serde(default = "default_output_type")]
    pub value_type: String,
    /// Required inputs must be supplied unless they define a default.
    #[serde(default = "default_true")]
    pub required: bool,
    /// A value used when the caller does not supply this input.
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    /// Interaction and semantic hint for values that name filesystem paths.
    ///
    /// Runtime values remain ordinary JSON strings. The hint lets callers
    /// select a suitable path picker and lets validation compare an input to
    /// a tool argument's path contract.
    #[serde(default)]
    pub input_kind: InputKind,
}

/// The semantic role of a plan input.
///
/// Path kinds intentionally do not change the JSON runtime representation:
/// all three are still string values. They only describe how callers collect
/// the value and how a plan binds it to a tool contract.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    /// An ordinary JSON value. This is the backwards-compatible default.
    #[default]
    Value,
    /// A path to an existing file, collected with an open-file picker.
    FilePath,
    /// A destination file path, collected with a save-file picker.
    OutputFilePath,
    /// A directory path, collected with a folder picker.
    DirectoryPath,
}

impl InputKind {
    /// Whether this semantic kind requires a string-valued input.
    pub fn requires_string(self) -> bool {
        !matches!(self, Self::Value)
    }
}

impl PlanInput {
    /// Return the input kind used by callers and validators.
    ///
    /// Plans persisted before `input_kind` was introduced deserialize as
    /// [`InputKind::Value`]. Their conventional `root_directory` input still
    /// needs a directory picker, so it retains that legacy behavior here.
    pub fn effective_input_kind(&self) -> InputKind {
        if self.input_kind == InputKind::Value && self.name == ROOT_DIRECTORY_INPUT {
            InputKind::DirectoryPath
        } else {
            self.input_kind
        }
    }
}

fn default_true() -> bool {
    true
}

/// A named output produced by a step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanOutput {
    pub name: String,
    pub description: Option<String>,
    /// JSON Schema type hint: `"string"`, `"number"`, `"boolean"`, `"object"`, `"array"`, `"any"`.
    #[serde(default = "default_output_type")]
    pub value_type: String,
}

fn default_output_type() -> String {
    "any".to_owned()
}

/// A published, top-level output of the whole plan.
///
/// Unlike [`PlanOutput`] (which just declares that a step produces a named
/// value), a `PlanOutputRef` is an explicit *reference* to one specific
/// step's result — e.g. `${step.summarize.summary}`. Once a run finishes
/// successfully, these references are resolved against the completed run's
/// step outputs and surfaced to the user as the plan's "final result".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanOutputRef {
    pub name: String,
    pub description: Option<String>,
    /// A single `${step.<step_id>.<output_name>}` placeholder naming the
    /// step output this value comes from.
    pub source: String,
}

/// A single step in the plan graph.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanStep {
    /// Stable identifier — used as a dependency reference and in run logs.
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub config: StepConfig,
    /// IDs of steps that must succeed before this step can run.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Named outputs this step is expected to produce.
    #[serde(default)]
    pub outputs: Vec<PlanOutput>,
    /// Per-step timeout, overrides any plan-level default.
    pub timeout_secs: Option<u64>,
    pub retry: Option<RetryPolicy>,
}

impl PlanStep {
    pub fn step_type(&self) -> StepType {
        self.config.step_type()
    }
}

// ─── Plan ─────────────────────────────────────────────────────────────────────

/// Lifecycle state of a plan.
///
/// Plans created through the design flow start as `Draft` and are not "live"
/// until the user publishes them.
/// Defaults to `Published` so plan JSON stored before this field existed
/// deserialises as published and keeps behaving exactly as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    #[default]
    Published,
}

/// Provenance and versioning metadata for a plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanMetadata {
    /// UUID v4 — stable across versions.
    pub id: String,
    /// Monotonically increasing version counter.
    pub version: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Backend that compiled this version, e.g. `"claude:claude-sonnet-4-6"`.
    pub compiled_by: Option<String>,
    /// The natural-language intent that produced this plan.
    pub intent: Option<String>,
    /// ID of the plan this was repaired from (if applicable).
    pub parent_plan_id: Option<String>,
    /// Version of the parent plan this was repaired from.
    pub parent_version: Option<u32>,
    /// Lifecycle state — draft plans are testable but not live.
    #[serde(default)]
    pub status: PlanStatus,
    /// Markdown text of the approved solution design that produced this plan.
    /// Provenance only — never interpreted by the engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solution_design: Option<String>,
}

impl PlanMetadata {
    pub fn new(intent: Option<String>) -> Self {
        let now = Utc::now();
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            version: 1,
            created_at: now,
            updated_at: now,
            compiled_by: None,
            intent,
            parent_plan_id: None,
            parent_version: None,
            status: PlanStatus::Published,
            solution_design: None,
        }
    }

    /// Note: preserves `status` and `solution_design` — a new version of a
    /// draft stays a draft.
    pub fn next_version(&self) -> Self {
        Self {
            version: self.version + 1,
            updated_at: Utc::now(),
            ..self.clone()
        }
    }

    /// True when the plan is a draft — testable but not live.
    pub fn is_draft(&self) -> bool {
        self.status == PlanStatus::Draft
    }
}

/// The compiled workflow artifact.
///
/// A plan is the single source of truth for what a workflow does and how it does it.
/// It is produced by the compiler, validated, stored, approved, and then executed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    pub metadata: PlanMetadata,
    pub name: String,
    pub description: Option<String>,
    /// Values a caller supplies for each invocation, available as `${input.<name>}`.
    #[serde(default)]
    pub inputs: Vec<PlanInput>,
    /// Static workflow configuration available as `${conf.<key>}` placeholders.
    #[serde(default)]
    pub config: IndexMap<String, serde_json::Value>,
    pub steps: Vec<PlanStep>,
    /// Published outputs for the whole plan — explicit references to step
    /// results, resolved and shown to the user as the run's "final result"
    /// once the plan finishes successfully.
    #[serde(default)]
    pub outputs: Vec<PlanOutputRef>,
}

impl Plan {
    /// Convenience: find a step by ID.
    pub fn step(&self, id: &str) -> Option<&PlanStep> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// True when the plan contains at least one step that runs a command on
    /// the host (CODE_CALL or AGENT_CALL). Such plans must declare a
    /// [`ROOT_DIRECTORY_INPUT`] input so the working directory is always
    /// explicit rather than defaulting to wherever the app process happens
    /// to be running from.
    pub fn requires_root_directory(&self) -> bool {
        self.steps.iter().any(|step| {
            matches!(
                step.config,
                StepConfig::CodeCall(_) | StepConfig::AgentCall(_)
            )
        })
    }

    /// The plan's declared `root_directory` input, if any.
    pub fn root_directory_input(&self) -> Option<&PlanInput> {
        self.inputs
            .iter()
            .find(|input| input.name == ROOT_DIRECTORY_INPUT)
    }

    /// Validate caller-provided values and apply defaults for one invocation.
    pub fn resolve_inputs(
        &self,
        provided: &IndexMap<String, serde_json::Value>,
    ) -> Result<IndexMap<String, serde_json::Value>, crate::error::PlanError> {
        use crate::error::PlanError;

        for name in provided.keys() {
            if !self.inputs.iter().any(|input| input.name == *name) {
                return Err(PlanError::UnknownInput {
                    name: name.clone(),
                    plan: self.name.clone(),
                    expected: self
                        .inputs
                        .iter()
                        .map(|input| input.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                });
            }
        }

        let mut resolved = IndexMap::new();
        for input in &self.inputs {
            let value = provided
                .get(&input.name)
                .cloned()
                // Callers that build `inputs` programmatically commonly send JSON
                // `null` (or an empty string) to mean "unset" rather than omitting
                // the key. When the input declares a default, treat those the same
                // as an absent key so the default applies instead of injecting a
                // value that only fails the tool's schema at run time.
                .filter(|value| !(input.default.is_some() && value_defers_to_default(value)))
                .or_else(|| input.default.clone());
            let value = match value {
                Some(value) => value,
                None if input.required => {
                    return Err(PlanError::MissingRequiredInput {
                        name: input.name.clone(),
                        plan: self.name.clone(),
                    });
                }
                None => serde_json::Value::Null,
            };
            if input.required && input.value_type != "any" && value.is_null() {
                return Err(PlanError::InputTypeMismatch {
                    name: input.name.clone(),
                    expected: input.value_type.clone(),
                    actual: json_type_name(&value).to_owned(),
                });
            }
            if !input_value_matches_type(&value, &input.value_type) {
                return Err(PlanError::InputTypeMismatch {
                    name: input.name.clone(),
                    expected: input.value_type.clone(),
                    actual: json_type_name(&value).to_owned(),
                });
            }
            resolved.insert(input.name.clone(), value);
        }
        Ok(resolved)
    }
}

/// Whether a caller-provided value should defer to the input's declared
/// default: explicit `null` always does, and an empty string does so that
/// `""` and omitting the key behave identically. Deliberate falsy values
/// (`false`, `0`, `[]`, `{}`) still win over the default.
fn value_defers_to_default(value: &serde_json::Value) -> bool {
    value.is_null() || value.as_str().is_some_and(str::is_empty)
}

pub fn input_value_matches_type(value: &serde_json::Value, value_type: &str) -> bool {
    if value.is_null() {
        return true;
    }
    match value_type {
        "any" => true,
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => crate::support::is_json_integer(value),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        _ => false,
    }
}

pub fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod step_config_serde_tests {
    use super::*;

    /// One representative value per `StepConfig` variant, with optional
    /// fields populated so their serde attributes are exercised.
    fn all_step_config_variants() -> Vec<StepConfig> {
        vec![
            StepConfig::ToolCall(ToolCallConfig {
                tool: "search".to_owned(),
                arguments: [("query".to_owned(), serde_json::json!("${input.q}"))]
                    .into_iter()
                    .collect(),
            }),
            StepConfig::CodeCall(CodeCallConfig {
                language: "bash".to_owned(),
                inline: Some("echo hi".to_owned()),
                file: None,
                args: vec!["-x".to_owned()],
                stdin: Some("payload".to_owned()),
                env: [("KEY".to_owned(), "value".to_owned())]
                    .into_iter()
                    .collect(),
                working_dir: Some("/work".to_owned()),
                timeout_secs: Some(30),
            }),
            StepConfig::HumanInteraction(HumanInteractionConfig {
                prompt: "Approve?".to_owned(),
                response_field: "answer".to_owned(),
                approval_required: true,
            }),
            StepConfig::FanOut(FanOutConfig {
                over: "list.items".to_owned(),
                item_var: "item".to_owned(),
                spawn_steps: vec!["body".to_owned()],
                until: Some("${step.body.done} == true".to_owned()),
            }),
            // `until: None` is skipped during serialization — round-trip
            // must still reproduce the same value.
            StepConfig::FanOut(FanOutConfig {
                over: "list.items".to_owned(),
                item_var: "item".to_owned(),
                spawn_steps: vec![],
                until: None,
            }),
            StepConfig::FanIn(FanInConfig {
                from_steps: vec!["a".to_owned(), "b".to_owned()],
                collect_field: "results".to_owned(),
            }),
            StepConfig::PromptCall(PromptCallConfig {
                model: "claude-sonnet-4-6".to_owned(),
                system_prompt: Some("You are terse.".to_owned()),
                user_prompt: "Summarise ${step.fetch.body}".to_owned(),
                output_field: "summary".to_owned(),
                max_tokens: Some(1024),
                temperature: Some(0.2),
            }),
            StepConfig::Condition(ConditionConfig {
                expression: "${step.check.ok} == true".to_owned(),
                true_steps: vec!["yes".to_owned()],
                false_steps: vec!["no".to_owned()],
            }),
            StepConfig::AgentCall(AgentCallConfig {
                objective: "Implement and test the requested change".to_owned(),
                working_dir: "${input.root_directory}".to_owned(),
                timeout_secs: Some(900),
            }),
        ]
    }

    #[test]
    fn every_step_config_variant_round_trips_through_json() {
        for config in all_step_config_variants() {
            let json = serde_json::to_value(&config).unwrap();
            let parsed: StepConfig = serde_json::from_value(json).unwrap();
            assert_eq!(parsed, config);
        }
    }

    /// The serde tag is the persistence contract; `StepType::to_string` is
    /// used in logs/errors and must never drift from it.
    #[test]
    fn serde_tag_matches_step_type_display() {
        for config in all_step_config_variants() {
            let json = serde_json::to_value(&config).unwrap();
            assert_eq!(
                json["type"],
                serde_json::json!(config.step_type().to_string()),
                "serde tag and Display disagree for {:?}",
                config.step_type()
            );
        }
    }

    #[test]
    fn step_type_round_trips() {
        for config in all_step_config_variants() {
            let step_type = config.step_type();
            let json = serde_json::to_value(step_type).unwrap();
            let parsed: StepType = serde_json::from_value(json).unwrap();
            assert_eq!(parsed, step_type);
        }
    }

    #[test]
    fn agent_call_requires_working_dir_in_serialized_config() {
        let missing_working_dir = serde_json::json!({
            "type": "AGENT_CALL",
            "objective": "Fix the failing tests",
            "timeout_secs": 600
        });

        assert!(serde_json::from_value::<StepConfig>(missing_working_dir).is_err());
    }

    #[test]
    fn plan_step_with_retry_outputs_and_timeout_round_trips() {
        let step = PlanStep {
            id: "s1".to_owned(),
            name: "Step".to_owned(),
            description: Some("desc".to_owned()),
            config: StepConfig::ToolCall(ToolCallConfig {
                tool: "echo".to_owned(),
                arguments: IndexMap::new(),
            }),
            depends_on: vec!["s0".to_owned()],
            outputs: vec![PlanOutput {
                name: "out".to_owned(),
                description: None,
                value_type: "string".to_owned(),
            }],
            timeout_secs: Some(60),
            retry: Some(RetryPolicy {
                max_attempts: 3,
                delay_secs: 5,
                backoff: true,
            }),
        };
        let json = serde_json::to_value(&step).unwrap();
        let parsed: PlanStep = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, step);
    }

    #[test]
    fn plan_output_ref_round_trips() {
        let output = PlanOutputRef {
            name: "final".to_owned(),
            description: Some("the result".to_owned()),
            source: "${step.summarize.summary}".to_owned(),
        };
        let json = serde_json::to_value(&output).unwrap();
        let parsed: PlanOutputRef = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, output);
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn new_metadata_is_published_with_no_solution_design() {
        let metadata = PlanMetadata::new(None);
        assert_eq!(metadata.status, PlanStatus::Published);
        assert!(metadata.solution_design.is_none());
        assert!(!metadata.is_draft());
    }

    #[test]
    fn metadata_without_status_field_deserialises_as_published() {
        // Plan JSON stored before the status field existed must keep working.
        let json = serde_json::json!({
            "id": "plan-1",
            "version": 1,
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "compiled_by": null,
            "intent": null,
            "parent_plan_id": null,
            "parent_version": null,
        });
        let metadata: PlanMetadata = serde_json::from_value(json).unwrap();
        assert_eq!(metadata.status, PlanStatus::Published);
        assert!(metadata.solution_design.is_none());
    }

    #[test]
    fn status_serialises_as_snake_case_and_solution_design_is_omitted_when_none() {
        let mut metadata = PlanMetadata::new(None);
        metadata.status = PlanStatus::Draft;
        let value = serde_json::to_value(&metadata).unwrap();
        assert_eq!(value["status"], serde_json::json!("draft"));
        assert!(value.get("solution_design").is_none());

        metadata.solution_design = Some("# Design".to_owned());
        let value = serde_json::to_value(&metadata).unwrap();
        assert_eq!(value["solution_design"], serde_json::json!("# Design"));
    }

    #[test]
    fn next_version_preserves_status_and_solution_design() {
        let mut metadata = PlanMetadata::new(Some("intent".to_owned()));
        metadata.status = PlanStatus::Draft;
        metadata.solution_design = Some("# Design".to_owned());

        let next = metadata.next_version();
        assert_eq!(next.version, metadata.version + 1);
        assert_eq!(next.status, PlanStatus::Draft);
        assert!(next.is_draft());
        assert_eq!(next.solution_design.as_deref(), Some("# Design"));
    }
}

#[cfg(test)]
mod input_tests {
    use super::*;

    fn plan_with_inputs(inputs: Vec<PlanInput>) -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: "input-test".to_owned(),
            description: None,
            inputs,
            config: IndexMap::new(),
            steps: vec![],
            outputs: vec![],
        }
    }

    #[test]
    fn agent_call_marks_plan_as_requiring_a_root_directory() {
        let mut plan = plan_with_inputs(vec![]);
        plan.steps.push(PlanStep {
            id: "agent".to_owned(),
            name: "Agent".to_owned(),
            description: None,
            config: StepConfig::AgentCall(AgentCallConfig {
                objective: "Make the requested repository change".to_owned(),
                working_dir: "${input.root_directory}".to_owned(),
                timeout_secs: None,
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        });

        assert!(plan.requires_root_directory());
    }

    #[test]
    fn resolve_inputs_applies_defaults_and_preserves_native_types() {
        let plan = plan_with_inputs(vec![
            PlanInput {
                name: "query".to_owned(),
                description: None,
                value_type: "string".to_owned(),
                required: true,
                default: None,
                input_kind: InputKind::Value,
            },
            PlanInput {
                name: "limit".to_owned(),
                description: None,
                value_type: "integer".to_owned(),
                required: false,
                default: Some(serde_json::json!(5)),
                input_kind: InputKind::Value,
            },
        ]);
        let provided = [("query".to_owned(), serde_json::json!("rust"))]
            .into_iter()
            .collect();

        let resolved = plan.resolve_inputs(&provided).unwrap();
        assert_eq!(resolved["query"], serde_json::json!("rust"));
        assert_eq!(resolved["limit"], serde_json::json!(5));
    }

    /// `root_directory` may be declared
    /// `required: false` with no default, meaning "use the app-managed
    /// scratch workspace". The plan IR must not force it to be required,
    /// invent a default, or otherwise reject this shape — resolving inputs
    /// without a caller-supplied value should yield `null`, letting the
    /// executor apply its own scratch-dir fallback.
    #[test]
    fn optional_root_directory_input_without_default_resolves_to_null() {
        let plan = plan_with_inputs(vec![PlanInput {
            name: ROOT_DIRECTORY_INPUT.to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: false,
            default: None,
            input_kind: InputKind::Value,
        }]);

        let resolved = plan.resolve_inputs(&IndexMap::new()).unwrap();
        assert_eq!(resolved[ROOT_DIRECTORY_INPUT], serde_json::Value::Null);

        // Round-trips through JSON unchanged: still optional, still no default.
        let json = serde_json::to_value(&plan.inputs[0]).unwrap();
        assert_eq!(json["required"], serde_json::json!(false));
        assert!(
            json.get("default").is_none_or(|value| value.is_null()),
            "default must not be invented for optional root_directory"
        );
        let round_tripped: PlanInput = serde_json::from_value(json).unwrap();
        assert_eq!(round_tripped, plan.inputs[0]);
    }

    #[test]
    fn input_kind_round_trips_as_a_stable_snake_case_contract() {
        let cases = [
            (InputKind::Value, "value"),
            (InputKind::FilePath, "file_path"),
            (InputKind::OutputFilePath, "output_file_path"),
            (InputKind::DirectoryPath, "directory_path"),
        ];

        for (kind, serialized) in cases {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::json!(serialized)
            );
            assert_eq!(
                serde_json::from_value::<InputKind>(serde_json::json!(serialized)).unwrap(),
                kind
            );
        }
        assert_eq!(InputKind::default(), InputKind::Value);
        assert!(!InputKind::Value.requires_string());
        assert!(InputKind::FilePath.requires_string());
        assert!(InputKind::OutputFilePath.requires_string());
        assert!(InputKind::DirectoryPath.requires_string());
    }

    #[test]
    fn legacy_inputs_without_kind_deserialize_as_value_and_keep_root_directory_picker() {
        let legacy_root: PlanInput = serde_json::from_value(serde_json::json!({
            "name": ROOT_DIRECTORY_INPUT,
            "description": "Workspace",
            "value_type": "string",
            "required": true,
            "default": null
        }))
        .unwrap();
        assert_eq!(legacy_root.input_kind, InputKind::Value);
        assert_eq!(legacy_root.effective_input_kind(), InputKind::DirectoryPath);

        let legacy_value: PlanInput = serde_json::from_value(serde_json::json!({
            "name": "query",
            "description": null,
            "value_type": "string",
            "required": true,
            "default": null
        }))
        .unwrap();
        assert_eq!(legacy_value.input_kind, InputKind::Value);
        assert_eq!(legacy_value.effective_input_kind(), InputKind::Value);
    }

    #[test]
    fn explicit_path_kinds_are_preserved_on_serialization() {
        let input = PlanInput {
            name: "output_path".to_owned(),
            description: Some("Where to save the report".to_owned()),
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: InputKind::OutputFilePath,
        };

        let serialized = serde_json::to_value(&input).unwrap();
        assert_eq!(
            serialized["input_kind"],
            serde_json::json!("output_file_path")
        );
        assert_eq!(
            serde_json::from_value::<PlanInput>(serialized).unwrap(),
            input
        );
    }

    #[test]
    fn resolve_inputs_rejects_missing_unknown_and_wrong_typed_values() {
        let plan = plan_with_inputs(vec![PlanInput {
            name: "count".to_owned(),
            description: None,
            value_type: "integer".to_owned(),
            required: true,
            default: None,
            input_kind: InputKind::Value,
        }]);
        let missing = plan.resolve_inputs(&IndexMap::new()).unwrap_err();
        assert!(matches!(
            missing,
            crate::error::PlanError::MissingRequiredInput { ref name, .. } if name == "count"
        ));
        assert!(missing.to_string().contains("missing required"));

        let unknown = plan
            .resolve_inputs(
                &[("other".to_owned(), serde_json::json!(1))]
                    .into_iter()
                    .collect(),
            )
            .unwrap_err();
        assert!(matches!(
            unknown,
            crate::error::PlanError::UnknownInput { ref name, .. } if name == "other"
        ));
        assert!(unknown.to_string().contains("unknown input"));

        let mismatch = plan
            .resolve_inputs(
                &[("count".to_owned(), serde_json::json!(1.5))]
                    .into_iter()
                    .collect(),
            )
            .unwrap_err();
        assert!(matches!(
            mismatch,
            crate::error::PlanError::InputTypeMismatch { ref expected, .. } if expected == "integer"
        ));
        assert!(mismatch.to_string().contains("must be integer"));
    }

    #[test]
    fn required_concrete_inputs_reject_explicit_and_default_null() {
        const CONCRETE_TYPES: &[&str] =
            &["string", "number", "integer", "boolean", "object", "array"];

        for value_type in CONCRETE_TYPES {
            let input = PlanInput {
                name: "value".to_owned(),
                description: None,
                value_type: (*value_type).to_owned(),
                required: true,
                default: None,
                input_kind: InputKind::Value,
            };
            let plan = plan_with_inputs(vec![input.clone()]);
            let explicit_null = plan
                .resolve_inputs(
                    &[("value".to_owned(), serde_json::Value::Null)]
                        .into_iter()
                        .collect(),
                )
                .unwrap_err();
            assert!(matches!(
                explicit_null,
                crate::error::PlanError::InputTypeMismatch {
                    ref expected,
                    ref actual,
                    ..
                } if expected == value_type && actual == "null"
            ));

            let plan = plan_with_inputs(vec![PlanInput {
                default: Some(serde_json::Value::Null),
                ..input
            }]);
            let default_null = plan.resolve_inputs(&IndexMap::new()).unwrap_err();
            assert!(matches!(
                default_null,
                crate::error::PlanError::InputTypeMismatch {
                    ref expected,
                    ref actual,
                    ..
                } if expected == value_type && actual == "null"
            ));
        }
    }

    #[test]
    fn optional_omitted_concrete_input_still_resolves_to_null() {
        let plan = plan_with_inputs(vec![PlanInput {
            name: "optional_path".to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: false,
            default: None,
            input_kind: InputKind::Value,
        }]);

        let resolved = plan.resolve_inputs(&IndexMap::new()).unwrap();

        assert_eq!(resolved["optional_path"], serde_json::Value::Null);
    }

    /// Callers that send explicit `null` or `""`
    /// for an input with a declared default should get the default, exactly
    /// as if they had omitted the key.
    #[test]
    fn explicit_null_and_empty_string_defer_to_declared_default() {
        let plan = plan_with_inputs(vec![PlanInput {
            name: "price".to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: true,
            default: Some(serde_json::json!("52.50")),
            input_kind: InputKind::Value,
        }]);

        for unset in [serde_json::Value::Null, serde_json::json!("")] {
            let resolved = plan
                .resolve_inputs(&[("price".to_owned(), unset)].into_iter().collect())
                .unwrap();
            assert_eq!(resolved["price"], serde_json::json!("52.50"));
        }
    }

    #[test]
    fn real_values_including_falsy_ones_still_override_defaults() {
        let cases: &[(&str, serde_json::Value, serde_json::Value)] = &[
            (
                "string",
                serde_json::json!("default"),
                serde_json::json!("provided"),
            ),
            ("boolean", serde_json::json!(true), serde_json::json!(false)),
            ("integer", serde_json::json!(7), serde_json::json!(0)),
            ("array", serde_json::json!([1]), serde_json::json!([])),
            ("object", serde_json::json!({"a": 1}), serde_json::json!({})),
        ];

        for (value_type, default, provided) in cases {
            let plan = plan_with_inputs(vec![PlanInput {
                name: "value".to_owned(),
                description: None,
                value_type: (*value_type).to_owned(),
                required: true,
                default: Some(default.clone()),
                input_kind: InputKind::Value,
            }]);
            let resolved = plan
                .resolve_inputs(
                    &[("value".to_owned(), provided.clone())]
                        .into_iter()
                        .collect(),
                )
                .unwrap();
            assert_eq!(
                &resolved["value"], provided,
                "{value_type}: provided value must win over default"
            );
        }
    }

    /// Without a declared default, explicit `null`/`""` keep their current
    /// behavior: `null` fails a required concrete input's type check, and
    /// `""` is passed through as a legitimate string value.
    #[test]
    fn explicit_null_and_empty_string_without_default_keep_current_behavior() {
        let plan = plan_with_inputs(vec![PlanInput {
            name: "value".to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: InputKind::Value,
        }]);

        let explicit_null = plan
            .resolve_inputs(
                &[("value".to_owned(), serde_json::Value::Null)]
                    .into_iter()
                    .collect(),
            )
            .unwrap_err();
        assert!(matches!(
            explicit_null,
            crate::error::PlanError::InputTypeMismatch { ref actual, .. } if actual == "null"
        ));

        let resolved = plan
            .resolve_inputs(
                &[("value".to_owned(), serde_json::json!(""))]
                    .into_iter()
                    .collect(),
            )
            .unwrap();
        assert_eq!(resolved["value"], serde_json::json!(""));
    }
}
