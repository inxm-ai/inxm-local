use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─── Validation errors ────────────────────────────────────────────────────────

/// A single validation error, pinned to a step and field where possible.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidationError {
    /// The step ID this error relates to, if applicable.
    pub step_id: Option<String>,
    /// The field path inside the step config (e.g. "config.tool").
    pub field: Option<String>,
    pub kind: ValidationErrorKind,
    pub message: String,
}

impl ValidationError {
    pub fn plan(kind: ValidationErrorKind, message: impl Into<String>) -> Self {
        Self {
            step_id: None,
            field: None,
            kind,
            message: message.into(),
        }
    }

    pub fn step(
        step_id: impl Into<String>,
        kind: ValidationErrorKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            step_id: Some(step_id.into()),
            field: None,
            kind,
            message: message.into(),
        }
    }

    pub fn field(
        step_id: impl Into<String>,
        field: impl Into<String>,
        kind: ValidationErrorKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            step_id: Some(step_id.into()),
            field: Some(field.into()),
            kind,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.step_id, &self.field) {
            (Some(s), Some(fld)) => write!(f, "[step:{s} field:{fld}] {}", self.message),
            (Some(s), None) => write!(f, "[step:{s}] {}", self.message),
            _ => write!(f, "{}", self.message),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ValidationErrorKind {
    MissingDependency,
    CyclicDependency,
    UnreachableStep,
    UnknownPlaceholder,
    MalformedPlaceholder,
    UnknownTool,
    MissingRequiredArgument,
    TypeMismatch,
    ForbiddenStepType,
    PromptCallConstraint,
    DuplicateStepId,
    EmptyPlan,
    InvalidStepConfig,
    MissingRootDirectoryInput,
    Other,
}

// ─── Plan errors ─────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("plan not found: {id}")]
    NotFound { id: String },

    #[error("validation failed with {count} error(s):\n{errors}")]
    ValidationFailed { count: usize, errors: String },

    #[error("serialisation error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid plan structure: {0}")]
    Invalid(String),

    #[error("unknown input '{name}' for plan '{plan}'; expected: {expected}")]
    UnknownInput {
        name: String,
        plan: String,
        /// Comma-separated list of the input names the plan declares.
        expected: String,
    },

    #[error("missing required input '{name}' for plan '{plan}'")]
    MissingRequiredInput { name: String, plan: String },

    #[error("input '{name}' must be {expected}, got {actual}")]
    InputTypeMismatch {
        name: String,
        expected: String,
        actual: String,
    },
}

impl PlanError {
    pub fn validation(errors: Vec<ValidationError>) -> Self {
        let count = errors.len();
        let formatted = errors
            .iter()
            .map(|e| format!("  • {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        Self::ValidationFailed {
            count,
            errors: formatted,
        }
    }
}

// ─── Tool errors ─────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("tool not found: {name}")]
    NotFound { name: String },

    #[error("tool catalog error: {0}")]
    Catalog(String),

    #[error("tool execution failed ({tool}): {message}")]
    Execution { tool: String, message: String },

    /// `captured_output` is pre-formatted (empty when nothing was captured)
    /// so it can be interpolated directly; see [`ToolError::timeout`] and
    /// [`ToolError::timeout_with_output`].
    #[error("tool timed out after {secs}s: {tool}{captured_output}")]
    Timeout {
        tool: String,
        secs: u64,
        captured_output: String,
    },

    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl ToolError {
    /// A timeout with no captured child output, e.g. an HTTP request deadline
    /// or a case where output genuinely couldn't be captured.
    pub fn timeout(tool: impl Into<String>, secs: u64) -> Self {
        Self::Timeout {
            tool: tool.into(),
            secs,
            captured_output: String::new(),
        }
    }

    /// A timeout where the child process was killed after producing some
    /// stdout/stderr. Attaching it turns a bare "timed out" into a
    /// diagnosable failure — this is the difference between "it hung, no
    /// idea why" and seeing the interactive prompt (or other cause) that
    /// blocked it.
    pub fn timeout_with_output(
        tool: impl Into<String>,
        secs: u64,
        stdout: &str,
        stderr: &str,
    ) -> Self {
        Self::Timeout {
            tool: tool.into(),
            secs,
            captured_output: format_captured_output(stdout, stderr),
        }
    }
}

const MAX_TIMEOUT_OUTPUT_EXCERPT_CHARS: usize = 4_000;

fn format_captured_output(stdout: &str, stderr: &str) -> String {
    let stdout = truncate_excerpt(stdout.trim());
    let stderr = truncate_excerpt(stderr.trim());
    if stdout.is_empty() && stderr.is_empty() {
        return String::new();
    }
    let mut parts = Vec::new();
    if !stdout.is_empty() {
        parts.push(format!("stdout: {stdout}"));
    }
    if !stderr.is_empty() {
        parts.push(format!("stderr: {stderr}"));
    }
    format!(
        " — captured before the process was killed ({})",
        parts.join("; ")
    )
}

fn truncate_excerpt(s: &str) -> String {
    if s.chars().count() <= MAX_TIMEOUT_OUTPUT_EXCERPT_CHARS {
        return s.to_owned();
    }
    let mut excerpt: String = s.chars().take(MAX_TIMEOUT_OUTPUT_EXCERPT_CHARS).collect();
    excerpt.push_str("...(truncated)");
    excerpt
}

// ─── Compiler errors ─────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum CompilerError {
    #[error("API error from {backend}: {message}")]
    Api { backend: String, message: String },

    #[error("invalid response from {backend}: {message}\nraw: {raw}")]
    InvalidResponse {
        backend: String,
        message: String,
        raw: String,
    },

    /// The model returned parseable plan JSON, but the plan failed the
    /// deterministic validator. Carries the parsed plan so callers can hand
    /// the model its own artifact back for a targeted correction instead of
    /// regenerating blind.
    #[error("compiled plan from {backend} failed deterministic validation: {}", errors.join("; "))]
    PlanValidationFailed {
        backend: String,
        plan: Box<crate::plan::types::Plan>,
        errors: Vec<String>,
    },

    #[error("compiler config error: {0}")]
    Config(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

// ─── Executor errors ─────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("run not found: {id}")]
    RunNotFound { id: String },

    #[error("step execution failed (step: {step_id}): {message}")]
    StepFailed { step_id: String, message: String },

    /// The operator explicitly declined an approval step. This ends the run
    /// as *cancelled*, not failed — there is nothing to repair.
    #[error("run rejected by the operator at step {step_id}")]
    RejectedByHuman { step_id: String },

    /// Internal control signal used to persist a resumable MCP elicitation.
    #[error("human response pending at step {step_id}")]
    HumanResponsePending { step_id: String },

    /// A resume request that contradicts the persisted run state: different
    /// inputs, the wrong plan or version, or a checkpoint that no longer lines
    /// up with the plan.
    #[error("cannot resume run '{run_id}': {reason}")]
    InvalidResume { run_id: String, reason: String },

    #[error("tool error: {0}")]
    Tool(#[from] ToolError),

    #[error("plan error: {0}")]
    Plan(#[from] PlanError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("storage error: {0}")]
    Storage(String),
}

// ─── Repair errors ───────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum RepairError {
    #[error("no run found to repair: {run_id}")]
    RunNotFound { run_id: String },

    #[error("run did not fail — nothing to repair")]
    RunNotFailed,

    #[error("patch '{patch_id}' cannot be applied: expected status Approved, got {status}")]
    PatchNotApproved { patch_id: String, status: String },

    #[error("compiler error: {0}")]
    Compiler(#[from] CompilerError),

    #[error("validation failed after proposed patch: {0}")]
    PatchInvalid(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("storage error: {0}")]
    Storage(String),
}

// ─── Storage errors ───────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("not found: {kind} {id}")]
    NotFound { kind: &'static str, id: String },
}
