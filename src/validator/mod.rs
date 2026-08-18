//! Deterministic plan validation.
//!
//! Validation is purely functional: it takes a plan + catalog and returns a list
//! of errors. It never modifies the plan. All AI assistance belongs in the compiler
//! or repair phases — not here.

pub mod graph;
pub mod placeholders;
pub mod tool_binding;

use crate::error::{ValidationError, ValidationErrorKind};
use crate::plan::types::{
    AgentCallConfig, CodeCallConfig, FanInConfig, FanOutConfig, HumanInteractionConfig, InputKind,
    Plan, PlanStep, PromptCallConfig, StepConfig,
};
use crate::tools::catalog::ToolCatalog;
use std::collections::{HashMap, HashSet};

/// Value types a plan input may declare.
const ALLOWED_INPUT_TYPES: &[&str] = &[
    "string", "number", "integer", "boolean", "object", "array", "any",
];

/// Run all validation passes and return the collected errors.
///
/// Passes run in this order:
/// 1. Structural (empty plan, duplicate IDs)
/// 2. Step-kind contracts
/// 3. Graph (missing dependencies, cycles)
/// 5. Placeholder (unknown `${conf.*}` refs, malformed placeholders)
/// 6. Plan-level output references (`plan.outputs[].source` must point at a
///    real step output)
/// 7. Tool binding (unknown tools, missing required args, basic type checks)
pub fn validate(plan: &Plan, catalog: &ToolCatalog) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    errors.extend(validate_structure(plan));
    errors.extend(validate_step_contracts(plan));
    errors.extend(graph::validate_graph(plan));
    errors.extend(placeholders::validate_placeholders(plan));
    errors.extend(placeholders::validate_plan_output_sources(plan));
    errors.extend(tool_binding::validate_tool_bindings(plan, catalog));

    errors
}

// ─── Structural validation ────────────────────────────────────────────────────

fn validate_structure(plan: &Plan) -> Vec<ValidationError> {
    if plan.steps.is_empty() {
        // Further checks are meaningless on an empty plan.
        return vec![ValidationError::plan(
            ValidationErrorKind::EmptyPlan,
            "plan has no steps",
        )];
    }

    let mut errors = validate_plan_inputs(plan);
    errors.extend(validate_step_ids(plan));
    errors
}

/// A plan input name must start with a letter or underscore and contain only
/// letters, numbers, underscores, or hyphens.
pub(super) fn is_addressable_identifier(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn validate_plan_inputs(plan: &Plan) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let mut input_names = HashSet::new();
    for input in &plan.inputs {
        if !is_addressable_identifier(&input.name) {
            errors.push(ValidationError::plan(
                ValidationErrorKind::InvalidStepConfig,
                format!(
                    "plan input '{}' must start with a letter or underscore and contain only letters, numbers, underscores, or hyphens",
                    input.name
                ),
            ));
        } else if !input_names.insert(&input.name) {
            errors.push(ValidationError::plan(
                ValidationErrorKind::InvalidStepConfig,
                format!("duplicate plan input name: {}", input.name),
            ));
        }
        if !ALLOWED_INPUT_TYPES.contains(&input.value_type.as_str()) {
            errors.push(ValidationError::plan(
                ValidationErrorKind::TypeMismatch,
                format!(
                    "plan input '{}' has unsupported type '{}'",
                    input.name, input.value_type
                ),
            ));
        }
        if input.input_kind.requires_string() && input.value_type != "string" {
            errors.push(ValidationError::plan(
                ValidationErrorKind::TypeMismatch,
                format!(
                    "plan input '{}' has path input kind '{}' but value_type is '{}'; path inputs must use value_type 'string'",
                    input.name,
                    input_kind_name(input.input_kind),
                    input.value_type,
                ),
            ));
        }
        if let Some(default) = &input.default
            && !crate::plan::types::input_value_matches_type(default, &input.value_type)
        {
            errors.push(ValidationError::plan(
                ValidationErrorKind::TypeMismatch,
                format!(
                    "default for plan input '{}' must be {}, got {}",
                    input.name,
                    input.value_type,
                    crate::plan::types::json_type_name(default)
                ),
            ));
        }
    }
    errors
}

fn input_kind_name(input_kind: InputKind) -> &'static str {
    match input_kind {
        InputKind::Value => "value",
        InputKind::FilePath => "file_path",
        InputKind::OutputFilePath => "output_file_path",
        InputKind::DirectoryPath => "directory_path",
    }
}

/// Every step needs a non-empty, plan-unique ID.
fn validate_step_ids(plan: &Plan) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let mut seen = HashSet::new();
    for step in &plan.steps {
        if !is_addressable_identifier(&step.id) {
            errors.push(ValidationError::step(
                &step.id,
                ValidationErrorKind::InvalidStepConfig,
                format!(
                    "step ID '{}' must start with a letter or underscore and contain only letters, numbers, underscores, or hyphens",
                    step.id
                ),
            ));
        }
        if !seen.insert(&step.id) {
            errors.push(ValidationError::step(
                &step.id,
                ValidationErrorKind::DuplicateStepId,
                format!("duplicate step ID: {}", step.id),
            ));
        }
    }

    for step in &plan.steps {
        let mut output_names = HashSet::new();
        for output in &step.outputs {
            if !is_addressable_identifier(&output.name) {
                errors.push(ValidationError::field(
                    &step.id,
                    "outputs",
                    ValidationErrorKind::InvalidStepConfig,
                    format!(
                        "step output '{}' must be a placeholder-addressable identifier",
                        output.name
                    ),
                ));
            } else if !output_names.insert(&output.name) {
                errors.push(ValidationError::field(
                    &step.id,
                    "outputs",
                    ValidationErrorKind::InvalidStepConfig,
                    format!("duplicate step output name: {}", output.name),
                ));
            }
        }
    }

    errors
}

// ─── Typed step contracts ─────────────────────────────────────────────────────

const SUPPORTED_CODE_LANGUAGES: &[&str] = &[
    "python",
    "python3",
    "py",
    "bash",
    "sh",
    "shell",
    "javascript",
    "js",
    "node",
    "powershell",
    "pwsh",
    "ps1",
    "cmd",
    "bat",
    "batch",
];

fn validate_step_contracts(plan: &Plan) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    for step in &plan.steps {
        if step.timeout_secs == Some(0) {
            errors.push(ValidationError::field(
                &step.id,
                "timeout_secs",
                ValidationErrorKind::InvalidStepConfig,
                "step timeout_secs must be greater than zero",
            ));
        }

        match &step.config {
            StepConfig::ToolCall(_) => {}
            StepConfig::CodeCall(config) => {
                validate_code_call_contract(step, config, &mut errors);
            }
            StepConfig::HumanInteraction(config) => {
                validate_human_interaction_contract(step, config, &mut errors);
            }
            StepConfig::FanOut(_) => {
                validate_configured_output(step, FAN_OUT_RESULTS_OUTPUT, &mut errors);
            }
            StepConfig::FanIn(config) => {
                validate_fan_in_contract(plan, step, config, &mut errors);
            }
            StepConfig::PromptCall(config) => {
                validate_prompt_call_contract(step, config, &mut errors);
            }
            StepConfig::Condition(_) => {
                validate_configured_output(step, CONDITION_RESULT_OUTPUT, &mut errors);
            }
            StepConfig::AgentCall(config) => {
                validate_agent_call_contract(step, config, &mut errors);
            }
        }
    }

    errors.extend(validate_root_directory_input(plan));
    errors.extend(validate_fan_out_constraints(plan));
    errors.extend(validate_condition_constraints(plan));
    errors
}

const FAN_OUT_RESULTS_OUTPUT: &str = "results";
const CONDITION_RESULT_OUTPUT: &str = "result";

fn validate_configured_output(
    step: &PlanStep,
    output_name: &str,
    errors: &mut Vec<ValidationError>,
) {
    if !is_addressable_identifier(output_name) {
        errors.push(ValidationError::field(
            &step.id,
            "config",
            ValidationErrorKind::InvalidStepConfig,
            format!("configured output field '{output_name}' is not placeholder-addressable"),
        ));
        return;
    }
    if !step.outputs.is_empty() && !step.outputs.iter().any(|output| output.name == output_name) {
        errors.push(ValidationError::field(
            &step.id,
            "outputs",
            ValidationErrorKind::InvalidStepConfig,
            format!(
                "step '{}' declares outputs but omits its configured output '{output_name}'",
                step.id
            ),
        ));
    }
}

// ─── Root directory input ─────────────────────────────────────────────────────

/// Plans that run commands (CODE_CALL and AGENT_CALL steps) must declare a
/// `root_directory` input so the working directory stays user-overridable.
/// The input may now be `required: false` (optionally with a default): when
/// left unset at runtime, the executor runs the step in a managed per-run
/// scratch workspace instead of an undefined cwd. What is not allowed is
/// omitting the input entirely — without it there is no way to override the
/// working directory at all.
fn validate_root_directory_input(plan: &Plan) -> Vec<ValidationError> {
    if !plan.requires_root_directory() {
        return Vec::new();
    }

    let Some(input) = plan.root_directory_input() else {
        return vec![ValidationError::plan(
            ValidationErrorKind::MissingRootDirectoryInput,
            format!(
                "plan runs a command (CODE_CALL or AGENT_CALL) but does not declare a '{}' input for its working directory",
                crate::plan::types::ROOT_DIRECTORY_INPUT
            ),
        )];
    };

    let mut errors = Vec::new();
    if input.value_type != "string" {
        errors.push(ValidationError::plan(
            ValidationErrorKind::MissingRootDirectoryInput,
            format!(
                "plan input '{}' must have value_type 'string', got '{}'",
                crate::plan::types::ROOT_DIRECTORY_INPUT,
                input.value_type
            ),
        ));
    }
    let has_agent_call = plan
        .steps
        .iter()
        .any(|step| matches!(step.config, StepConfig::AgentCall(_)));
    if has_agent_call && !input.required {
        errors.push(ValidationError::plan(
            ValidationErrorKind::MissingRootDirectoryInput,
            format!(
                "plan input '{}' must be required when the plan contains an AGENT_CALL step",
                crate::plan::types::ROOT_DIRECTORY_INPUT
            ),
        ));
    }
    for step in &plan.steps {
        let (kind, working_dir, permits_implicit_workspace) = match &step.config {
            StepConfig::CodeCall(config) => ("CODE_CALL", config.working_dir.as_deref(), true),
            StepConfig::AgentCall(config) => {
                ("AGENT_CALL", Some(config.working_dir.as_str()), false)
            }
            _ => continue,
        };
        match working_dir {
            None if permits_implicit_workspace && !input.required => {}
            None => errors.push(ValidationError::field(
                &step.id,
                "config.working_dir",
                ValidationErrorKind::MissingRootDirectoryInput,
                format!(
                    "{kind} step '{}' must derive working_dir from '${{input.{}}}' because the input is required",
                    step.id,
                    crate::plan::types::ROOT_DIRECTORY_INPUT
                ),
            )),
            Some(working_dir) if is_root_directory_path(working_dir) => {}
            Some(_) => errors.push(ValidationError::field(
                &step.id,
                "config.working_dir",
                ValidationErrorKind::MissingRootDirectoryInput,
                format!(
                    "{kind} step '{}' working_dir must be '${{input.{}}}' or a path derived from it",
                    step.id,
                    crate::plan::types::ROOT_DIRECTORY_INPUT
                ),
            )),
        }
    }
    errors
}

fn validate_agent_call_contract(
    step: &PlanStep,
    config: &AgentCallConfig,
    errors: &mut Vec<ValidationError>,
) {
    if config.objective.trim().is_empty() {
        errors.push(ValidationError::field(
            &step.id,
            "config.objective",
            ValidationErrorKind::InvalidStepConfig,
            "AGENT_CALL requires a non-empty objective",
        ));
    }
    if config.timeout_secs == Some(0) {
        errors.push(ValidationError::field(
            &step.id,
            "config.timeout_secs",
            ValidationErrorKind::InvalidStepConfig,
            "AGENT_CALL timeout_secs must be greater than zero",
        ));
    }
}

fn is_root_directory_path(working_dir: &str) -> bool {
    let root = format!("${{input.{}}}", crate::plan::types::ROOT_DIRECTORY_INPUT);
    working_dir == root
        || working_dir
            .strip_prefix(&root)
            .is_some_and(|suffix| suffix.starts_with('/') || suffix.starts_with('\\'))
}

fn validate_code_call_contract(
    step: &PlanStep,
    config: &CodeCallConfig,
    errors: &mut Vec<ValidationError>,
) {
    let inline = config
        .inline
        .as_deref()
        .filter(|source| !source.trim().is_empty());
    let file = config
        .file
        .as_deref()
        .filter(|source| !source.trim().is_empty());
    if inline.is_some() == file.is_some() {
        errors.push(ValidationError::field(
            &step.id,
            "config",
            ValidationErrorKind::InvalidStepConfig,
            "CODE_CALL requires exactly one non-empty source: inline or file",
        ));
    }

    // `inline`/`file` must stay static: the CODE_CALL runner refuses to
    // substitute plan placeholders into executable sources
    // (`reject_executable_placeholders`), so a reserved-namespace `${...}`
    // here is a guaranteed run-time failure. Catch it now — the compile
    // correction loop can then fix the plan before it is ever saved.
    for (field, source) in [("inline", inline), ("file", file)] {
        let Some(source) = source else { continue };
        let mut seen = HashSet::new();
        for placeholder in placeholders::plan_placeholders_in_source(source) {
            if seen.insert(placeholder) {
                errors.push(ValidationError::field(
                    &step.id,
                    format!("config.{field}"),
                    ValidationErrorKind::InvalidStepConfig,
                    format!(
                        "CODE_CALL {field} in step '{}' must be static but contains plan placeholder '{placeholder}' — pass runtime values through args, env, or stdin",
                        step.id
                    ),
                ));
            }
        }
    }

    let language = config.language.trim().to_ascii_lowercase();
    if !SUPPORTED_CODE_LANGUAGES.contains(&language.as_str()) {
        errors.push(ValidationError::field(
            &step.id,
            "config.language",
            ValidationErrorKind::InvalidStepConfig,
            format!("CODE_CALL has unsupported language '{}'", config.language),
        ));
    }
    if config.timeout_secs == Some(0) {
        errors.push(ValidationError::field(
            &step.id,
            "config.timeout_secs",
            ValidationErrorKind::InvalidStepConfig,
            "CODE_CALL timeout_secs must be greater than zero",
        ));
    }
}

fn validate_human_interaction_contract(
    step: &PlanStep,
    config: &HumanInteractionConfig,
    errors: &mut Vec<ValidationError>,
) {
    if config.prompt.trim().is_empty() {
        errors.push(ValidationError::field(
            &step.id,
            "config.prompt",
            ValidationErrorKind::InvalidStepConfig,
            "HUMAN_INTERACTION requires a non-empty prompt",
        ));
    }
    validate_configured_output(step, &config.response_field, errors);
}

fn validate_fan_in_contract(
    plan: &Plan,
    step: &PlanStep,
    config: &FanInConfig,
    errors: &mut Vec<ValidationError>,
) {
    validate_configured_output(step, &config.collect_field, errors);
    if config.from_steps.is_empty() {
        errors.push(ValidationError::field(
            &step.id,
            "config.from_steps",
            ValidationErrorKind::InvalidStepConfig,
            "FAN_IN requires at least one source step",
        ));
    }

    let mut seen = HashSet::new();
    for source_id in &config.from_steps {
        if !seen.insert(source_id) {
            errors.push(ValidationError::field(
                &step.id,
                "config.from_steps",
                ValidationErrorKind::InvalidStepConfig,
                format!("FAN_IN contains duplicate source step '{source_id}'"),
            ));
            continue;
        }
        if source_id == &step.id {
            errors.push(ValidationError::field(
                &step.id,
                "config.from_steps",
                ValidationErrorKind::InvalidStepConfig,
                "FAN_IN cannot collect from itself",
            ));
        } else if plan.step(source_id).is_none() {
            errors.push(ValidationError::field(
                &step.id,
                "config.from_steps",
                ValidationErrorKind::MissingDependency,
                format!("FAN_IN source step '{source_id}' does not exist"),
            ));
        } else if !depends_transitively(plan, &step.id, source_id) {
            errors.push(ValidationError::field(
                &step.id,
                "depends_on",
                ValidationErrorKind::MissingDependency,
                format!(
                    "FAN_IN step '{}' must depend on source step '{source_id}'",
                    step.id
                ),
            ));
        }
    }
}

fn validate_prompt_call_contract(
    step: &PlanStep,
    config: &PromptCallConfig,
    errors: &mut Vec<ValidationError>,
) {
    if config.model.trim().is_empty() {
        errors.push(ValidationError::field(
            &step.id,
            "config.model",
            ValidationErrorKind::PromptCallConstraint,
            "PROMPT_CALL requires an explicit model identifier",
        ));
    }
    if config.user_prompt.trim().is_empty() {
        errors.push(ValidationError::field(
            &step.id,
            "config.user_prompt",
            ValidationErrorKind::PromptCallConstraint,
            "PROMPT_CALL requires a non-empty user_prompt",
        ));
    }
    if config.output_field.trim().is_empty() {
        errors.push(ValidationError::field(
            &step.id,
            "config.output_field",
            ValidationErrorKind::PromptCallConstraint,
            "PROMPT_CALL requires an output_field to store the result",
        ));
    } else {
        validate_configured_output(step, &config.output_field, errors);
    }
    if config.max_tokens == Some(0) {
        errors.push(ValidationError::field(
            &step.id,
            "config.max_tokens",
            ValidationErrorKind::PromptCallConstraint,
            "PROMPT_CALL max_tokens must be greater than zero",
        ));
    }
}

// ─── FAN_OUT constraints ──────────────────────────────────────────────────────

fn validate_fan_out_constraints(plan: &Plan) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let mut owner_lists: HashMap<&str, Vec<&str>> = HashMap::new();
    for step in &plan.steps {
        if let StepConfig::FanOut(config) = &step.config {
            let mut local = HashSet::new();
            for spawn_id in &config.spawn_steps {
                if !local.insert(spawn_id.as_str()) {
                    errors.push(ValidationError::field(
                        &step.id,
                        "config.spawn_steps",
                        ValidationErrorKind::InvalidStepConfig,
                        format!(
                            "FAN_OUT step '{}' lists spawn step '{}' more than once",
                            step.id, spawn_id
                        ),
                    ));
                }
                owner_lists
                    .entry(spawn_id.as_str())
                    .or_default()
                    .push(step.id.as_str());
            }
        }
    }
    for (spawn_id, owners) in &owner_lists {
        let unique: HashSet<&str> = owners.iter().copied().collect();
        if unique.len() > 1 {
            errors.push(ValidationError::field(
                *spawn_id,
                "config.spawn_steps",
                ValidationErrorKind::InvalidStepConfig,
                format!(
                    "FAN_OUT-owned step '{}' has multiple owners: {}",
                    spawn_id,
                    unique.into_iter().collect::<Vec<_>>().join(", ")
                ),
            ));
        }
    }
    let fan_out_owners: HashMap<&str, &str> = owner_lists
        .iter()
        .filter_map(|(spawn_id, owners)| {
            let unique: HashSet<&str> = owners.iter().copied().collect();
            (unique.len() == 1).then(|| (*spawn_id, owners[0]))
        })
        .collect();

    for step in &plan.steps {
        let StepConfig::FanOut(cfg) = &step.config else {
            continue;
        };

        check_fan_out_until(step, cfg, &mut errors);
        check_fan_out_spawn_steps(plan, step, cfg, &mut errors);
        check_raw_results_consumers(plan, step, cfg, &mut errors);
        // `over` must be well-formed before source checks make sense.
        if let Some((source_id, output_name)) = split_over_reference(step, cfg, &mut errors) {
            check_fan_out_source(plan, step, cfg, source_id, output_name, &mut errors);
        }
    }

    check_main_flow_dependencies(plan, &fan_out_owners, &mut errors);

    errors
}

fn check_fan_out_until(step: &PlanStep, cfg: &FanOutConfig, errors: &mut Vec<ValidationError>) {
    if let Some(until) = cfg.until.as_deref()
        && let Err(message) = crate::plan::steps::parse_expression(until)
    {
        errors.push(ValidationError::field(
            &step.id,
            "config.until",
            ValidationErrorKind::InvalidStepConfig,
            format!(
                "FAN_OUT step '{}' has invalid until expression: {message}",
                step.id
            ),
        ));
    }
}

/// Split `over` into `(source_step_id, output_name)`, reporting an error and
/// returning `None` when it is not of the form `<step-id>.<output-name>`.
fn split_over_reference<'cfg>(
    step: &PlanStep,
    cfg: &'cfg FanOutConfig,
    errors: &mut Vec<ValidationError>,
) -> Option<(&'cfg str, &'cfg str)> {
    match cfg.over.split_once('.') {
        Some((source_id, output_name)) if !source_id.is_empty() && !output_name.is_empty() => {
            Some((source_id, output_name))
        }
        _ => {
            errors.push(ValidationError::field(
                &step.id,
                "config.over",
                ValidationErrorKind::InvalidStepConfig,
                format!(
                    "FAN_OUT step '{}' has invalid over reference '{}' — expected '<step-id>.<output-name>'",
                    step.id, cfg.over
                ),
            ));
            None
        }
    }
}

/// The `over` source must exist, run before the FAN_OUT, and produce the
/// referenced output.
fn check_fan_out_source(
    plan: &Plan,
    step: &PlanStep,
    cfg: &FanOutConfig,
    source_id: &str,
    output_name: &str,
    errors: &mut Vec<ValidationError>,
) {
    let Some(source_step) = plan.step(source_id) else {
        errors.push(ValidationError::field(
            &step.id,
            "config.over",
            ValidationErrorKind::MissingDependency,
            format!(
                "FAN_OUT step '{}' iterates over '{}' but step '{}' does not exist",
                step.id, cfg.over, source_id
            ),
        ));
        return;
    };

    if !depends_transitively(plan, &step.id, source_id) {
        errors.push(ValidationError::field(
            &step.id,
            "depends_on",
            ValidationErrorKind::InvalidStepConfig,
            format!(
                "FAN_OUT step '{}' iterates over '{}' but does not depend on '{}' — add '{}' to depends_on so the list exists before fan-out runs",
                step.id, cfg.over, source_id, source_id
            ),
        ));
    }

    let known = placeholders::producible_outputs(source_step);
    if !known.iter().any(|name| name == output_name) {
        errors.push(ValidationError::field(
            &step.id,
            "config.over",
            ValidationErrorKind::UnknownPlaceholder,
            format!(
                "FAN_OUT step '{}' iterates over '{}' but {}",
                step.id,
                cfg.over,
                placeholders::describe_available_outputs(&known)
            ),
        ));
    }
}

/// Spawn steps must exist, not be the owner itself, not depend on the owner,
/// and not be HUMAN_INTERACTION steps (per-item human input cannot resume).
fn check_fan_out_spawn_steps(
    plan: &Plan,
    step: &PlanStep,
    cfg: &FanOutConfig,
    errors: &mut Vec<ValidationError>,
) {
    if cfg.spawn_steps.is_empty() {
        errors.push(ValidationError::field(
            &step.id,
            "config.spawn_steps",
            ValidationErrorKind::InvalidStepConfig,
            format!(
                "FAN_OUT step '{}' must list at least one spawn step",
                step.id
            ),
        ));
    }

    for (position, spawn_id) in cfg.spawn_steps.iter().enumerate() {
        if spawn_id == &step.id {
            errors.push(ValidationError::field(
                &step.id,
                "config.spawn_steps",
                ValidationErrorKind::InvalidStepConfig,
                format!("FAN_OUT step '{}' cannot spawn itself", step.id),
            ));
            continue;
        }
        let Some(spawn_step) = plan.step(spawn_id) else {
            errors.push(ValidationError::field(
                &step.id,
                "config.spawn_steps",
                ValidationErrorKind::MissingDependency,
                format!(
                    "FAN_OUT step '{}' spawns '{}' which does not exist",
                    step.id, spawn_id
                ),
            ));
            continue;
        };
        if spawn_step.depends_on.contains(&step.id) {
            errors.push(ValidationError::field(
                spawn_id,
                "depends_on",
                ValidationErrorKind::InvalidStepConfig,
                format!(
                    "FAN_OUT-owned step '{}' must not depend on its owner '{}' — it runs inside that step, not after it",
                    spawn_id, step.id
                ),
            ));
        }
        if matches!(spawn_step.config, StepConfig::HumanInteraction(_)) {
            let kind = spawn_step.step_type();
            errors.push(ValidationError::field(
                &step.id,
                "config.spawn_steps",
                ValidationErrorKind::InvalidStepConfig,
                format!(
                    "FAN_OUT step '{}' cannot own {kind} step '{}' because per-item human input cannot be resumed",
                    step.id, spawn_id
                ),
            ));
        }
        for dependency in &spawn_step.depends_on {
            if dependency == &step.id {
                continue;
            }
            if let Some(dependency_position) = cfg
                .spawn_steps
                .iter()
                .position(|candidate| candidate == dependency)
            {
                if dependency_position >= position {
                    errors.push(ValidationError::field(
                        spawn_id,
                        "depends_on",
                        ValidationErrorKind::InvalidStepConfig,
                        format!(
                            "FAN_OUT-owned step '{}' depends on body step '{}' which is not earlier in owner '{}' spawn_steps",
                            spawn_id, dependency, step.id
                        ),
                    ));
                }
            } else if !depends_transitively(plan, &step.id, dependency) {
                errors.push(ValidationError::field(
                    spawn_id,
                    "depends_on",
                    ValidationErrorKind::InvalidStepConfig,
                    format!(
                        "FAN_OUT-owned step '{}' depends on '{}' which is neither an earlier body step nor available to owner '{}'",
                        spawn_id, dependency, step.id
                    ),
                ));
            }
        }
    }
}

/// When the final spawn step yields raw tool/code output, a PROMPT_CALL that
/// consumes the FAN_OUT's collected `results` may blow the model's context —
/// require a per-item summarising PROMPT_CALL instead.
fn check_raw_results_consumers(
    plan: &Plan,
    step: &PlanStep,
    cfg: &FanOutConfig,
    errors: &mut Vec<ValidationError>,
) {
    let raw_result_step = cfg
        .spawn_steps
        .last()
        .and_then(|spawn_id| plan.step(spawn_id))
        .filter(|spawn_step| {
            !spawn_step.outputs.is_empty()
                && matches!(
                    spawn_step.config,
                    StepConfig::ToolCall(_) | StepConfig::CodeCall(_) | StepConfig::AgentCall(_)
                )
        });
    let Some(raw_result_step) = raw_result_step else {
        return;
    };

    let results_placeholder = format!(
        "${{step.{}.{}}}",
        step.id,
        placeholders::FAN_OUT_RESULTS_OUTPUT
    );
    for consumer in &plan.steps {
        let StepConfig::PromptCall(prompt) = &consumer.config else {
            continue;
        };
        if prompt.user_prompt.contains(&results_placeholder)
            || prompt
                .system_prompt
                .as_deref()
                .is_some_and(|value| value.contains(&results_placeholder))
        {
            errors.push(ValidationError::field(
                &consumer.id,
                "config.user_prompt",
                ValidationErrorKind::PromptCallConstraint,
                format!(
                    "PROMPT_CALL step '{}' sends potentially large FAN_OUT '{}' results from '{}' directly to the model — add a per-item PROMPT_CALL as the final spawn step so FAN_OUT collects compact summaries before aggregation",
                    consumer.id, step.id, raw_result_step.id
                ),
            ));
        }
    }
}

/// Spawn steps are templates executed inside their FAN_OUT owner. Main-flow
/// steps see the owner's `results`, while the template records are skipped.
fn check_main_flow_dependencies(
    plan: &Plan,
    fan_out_owners: &HashMap<&str, &str>,
    errors: &mut Vec<ValidationError>,
) {
    for step in &plan.steps {
        if fan_out_owners.contains_key(step.id.as_str()) {
            continue;
        }
        for dependency in &step.depends_on {
            if let Some(owner) = fan_out_owners.get(dependency.as_str()) {
                errors.push(ValidationError::field(
                    &step.id,
                    "depends_on",
                    ValidationErrorKind::InvalidStepConfig,
                    format!(
                        "main-flow step '{}' depends on FAN_OUT-owned step '{}' — depend on '{}' and consume its 'results' output instead",
                        step.id, dependency, owner
                    ),
                ));
            }
        }
    }
}

// ─── CONDITION constraints ────────────────────────────────────────────────────

/// CONDITION routing only works when the executor reaches the condition step
/// *before* any of its branch steps.  That ordering is only guaranteed when
/// each branch step depends (directly or transitively) on the condition step,
/// so we enforce it here rather than let a plan silently run both branches.
fn validate_condition_constraints(plan: &Plan) -> Vec<ValidationError> {
    use crate::plan::steps::parse_expression;

    let mut errors = Vec::new();
    let step_ids: HashSet<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();

    for step in &plan.steps {
        let StepConfig::Condition(cfg) = &step.config else {
            continue;
        };

        if let Err(msg) = parse_expression(&cfg.expression) {
            errors.push(ValidationError::field(
                &step.id,
                "config.expression",
                ValidationErrorKind::InvalidStepConfig,
                msg,
            ));
        }

        for human_step in &plan.steps {
            let StepConfig::HumanInteraction(human) = &human_step.config else {
                continue;
            };
            if human.approval_required {
                let approval_ref = format!("${{step.{}.{}}}", human_step.id, human.response_field);
                if cfg.expression.contains(&approval_ref) {
                    errors.push(ValidationError::field(
                        &step.id,
                        "config.expression",
                        ValidationErrorKind::InvalidStepConfig,
                        format!(
                            "CONDITION step '{}' branches on approval step '{}', but approval_required=true terminates the run on rejection, so this condition can never observe a rejected value; remove the redundant condition when rejection should cancel, or set approval_required=false when both choices must continue into explicit branches",
                            step.id, human_step.id
                        ),
                    ));
                }
            }
        }

        for (field, branch) in [
            ("config.true_steps", &cfg.true_steps),
            ("config.false_steps", &cfg.false_steps),
        ] {
            for branch_id in branch {
                if !step_ids.contains(branch_id.as_str()) {
                    errors.push(ValidationError::field(
                        &step.id,
                        field,
                        ValidationErrorKind::MissingDependency,
                        format!(
                            "CONDITION step '{}' routes to '{}' which does not exist",
                            step.id, branch_id
                        ),
                    ));
                } else if !depends_transitively(plan, branch_id, &step.id) {
                    errors.push(ValidationError::field(
                        &step.id,
                        field,
                        ValidationErrorKind::InvalidStepConfig,
                        format!(
                            "step '{}' is routed by CONDITION step '{}' but does not depend \
                             on it — add '{}' to its depends_on so it runs after the branch \
                             decision",
                            branch_id, step.id, step.id
                        ),
                    ));
                }
            }
        }

        if let Some(dup) = cfg
            .true_steps
            .iter()
            .find(|id| cfg.false_steps.contains(id))
        {
            errors.push(ValidationError::field(
                &step.id,
                "config.true_steps",
                ValidationErrorKind::InvalidStepConfig,
                format!(
                    "step '{}' appears in both true_steps and false_steps of CONDITION '{}'",
                    dup, step.id
                ),
            ));
        }
    }

    errors
}

/// True when `from` lists `target` in its `depends_on`, directly or through
/// intermediate steps.
fn depends_transitively(plan: &Plan, from: &str, target: &str) -> bool {
    let mut queue: Vec<&str> = vec![from];
    let mut seen: HashSet<&str> = HashSet::new();
    while let Some(id) = queue.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(step) = plan.step(id) else { continue };
        for dep in &step.depends_on {
            if dep == target {
                return true;
            }
            queue.push(dep.as_str());
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{
        AgentCallConfig, FanOutConfig, PlanInput, PlanMetadata, PlanOutput, PlanStep,
        PromptCallConfig, StepConfig, ToolCallConfig,
    };
    use crate::tools::catalog::ToolCatalog;

    fn make_step(id: &str, config: StepConfig) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            config,
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        }
    }

    fn tool_call(tool: &str) -> StepConfig {
        StepConfig::ToolCall(ToolCallConfig {
            tool: tool.to_owned(),
            arguments: Default::default(),
        })
    }

    fn valid_plan_one_step(tool_name: &str) -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![],
            config: Default::default(),
            steps: vec![make_step("step1", tool_call(tool_name))],
            outputs: vec![],
        }
    }

    fn catalog_with(names: &[&str]) -> ToolCatalog {
        use crate::tools::catalog::{SubprocessConfig, ToolConfig, ToolEntry};
        let tools = names
            .iter()
            .map(|name| ToolEntry {
                name: name.to_string(),
                description: String::new(),
                config: ToolConfig::Subprocess(SubprocessConfig {
                    command: "true".to_owned(),
                    args: vec![],
                    env: Default::default(),
                    working_dir: None,
                }),
                input_schema: serde_json::json!({"type":"object"}),
                output_schema: serde_json::json!({"type":"object"}),
                allowlisted: true,
                timeout_secs: None,
            })
            .collect();
        ToolCatalog::new(tools)
    }

    #[test]
    fn empty_plan_fails() {
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "empty".to_owned(),
            description: None,
            inputs: vec![],
            config: Default::default(),
            steps: vec![],
            outputs: vec![],
        };
        let errors = validate(&plan, &ToolCatalog::default());
        assert!(!errors.is_empty());
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::EmptyPlan)
        );
    }

    #[test]
    fn valid_plan_passes() {
        let plan = valid_plan_one_step("echo");
        let catalog = catalog_with(&["echo"]);
        let errors = validate(&plan, &catalog);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    fn agent_call(objective: &str, working_dir: &str, timeout_secs: Option<u64>) -> StepConfig {
        StepConfig::AgentCall(AgentCallConfig {
            objective: objective.to_owned(),
            working_dir: working_dir.to_owned(),
            timeout_secs,
        })
    }

    fn required_root_directory_input() -> PlanInput {
        PlanInput {
            name: crate::plan::types::ROOT_DIRECTORY_INPUT.to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: InputKind::DirectoryPath,
        }
    }

    #[test]
    fn agent_call_with_required_derived_working_directory_is_valid() {
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![required_root_directory_input()],
            config: Default::default(),
            steps: vec![make_step(
                "agent",
                agent_call(
                    "Implement the requested change",
                    "${input.root_directory}/crate",
                    Some(300),
                ),
            )],
            outputs: vec![],
        };
        let errors = validate(&plan, &ToolCatalog::default());
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[test]
    fn agent_call_rejects_empty_objective_and_zero_timeout() {
        let step = make_step(
            "agent",
            agent_call("  ", "${input.root_directory}", Some(0)),
        );
        let errors = validate_step_contracts(&Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![required_root_directory_input()],
            config: Default::default(),
            steps: vec![step],
            outputs: vec![],
        });
        assert!(errors.iter().any(|error| {
            error.field.as_deref() == Some("config.objective")
                && error.message.contains("non-empty objective")
        }));
        assert!(errors.iter().any(|error| {
            error.field.as_deref() == Some("config.timeout_secs")
                && error.message.contains("greater than zero")
        }));
    }

    #[test]
    fn duplicate_step_id_fails() {
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![],
            config: Default::default(),
            steps: vec![
                make_step("s1", tool_call("echo")),
                make_step("s1", tool_call("echo")),
            ],
            outputs: vec![],
        };
        let catalog = catalog_with(&["echo"]);
        let errors = validate(&plan, &catalog);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::DuplicateStepId)
        );
    }

    // ── Root directory input ─────────────────────────────────────────────────

    fn code_call_step(id: &str, working_dir: Option<&str>) -> PlanStep {
        make_step(
            id,
            StepConfig::CodeCall(crate::plan::types::CodeCallConfig {
                language: "bash".to_owned(),
                inline: Some("echo hi".to_owned()),
                file: None,
                args: vec![],
                stdin: None,
                env: Default::default(),
                working_dir: working_dir.map(str::to_owned),
                timeout_secs: None,
            }),
        )
    }

    #[test]
    fn code_call_without_root_directory_input_fails() {
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![],
            config: Default::default(),
            steps: vec![code_call_step("run", Some("${input.root_directory}"))],
            outputs: vec![],
        };
        let errors = validate(&plan, &ToolCatalog::default());
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::MissingRootDirectoryInput),
            "expected a MissingRootDirectoryInput error: {errors:?}"
        );
    }

    #[test]
    fn code_call_with_optional_root_directory_input_is_valid() {
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![crate::plan::types::PlanInput {
                name: "root_directory".to_owned(),
                description: None,
                value_type: "string".to_owned(),
                required: false,
                default: Some(serde_json::json!(".")),
                input_kind: InputKind::DirectoryPath,
            }],
            config: Default::default(),
            steps: vec![code_call_step("run", Some("${input.root_directory}"))],
            outputs: vec![],
        };
        let errors = validate(&plan, &ToolCatalog::default());
        assert!(
            errors
                .iter()
                .all(|e| e.kind != ValidationErrorKind::MissingRootDirectoryInput),
            "an optional root_directory input (with or without a default) is valid — \
             unset at runtime falls back to the managed scratch workspace: {errors:?}"
        );
    }

    #[test]
    fn code_call_with_required_root_directory_input_passes() {
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![crate::plan::types::PlanInput {
                name: "root_directory".to_owned(),
                description: None,
                value_type: "string".to_owned(),
                required: true,
                default: None,
                input_kind: InputKind::DirectoryPath,
            }],
            config: Default::default(),
            steps: vec![code_call_step("run", Some("${input.root_directory}"))],
            outputs: vec![],
        };
        let errors = validate(&plan, &ToolCatalog::default());
        assert!(
            errors
                .iter()
                .all(|e| e.kind != ValidationErrorKind::MissingRootDirectoryInput),
            "unexpected root-directory error: {errors:?}"
        );
    }

    #[test]
    fn agent_call_requires_root_directory_input_to_be_required() {
        let mut root = required_root_directory_input();
        root.required = false;
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![root],
            config: Default::default(),
            steps: vec![make_step(
                "agent",
                agent_call("Implement it", "${input.root_directory}", None),
            )],
            outputs: vec![],
        };
        let errors = validate_root_directory_input(&plan);
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("must be required"))
        );
    }

    #[test]
    fn agent_call_rejects_fixed_working_directory() {
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![required_root_directory_input()],
            config: Default::default(),
            steps: vec![make_step(
                "agent",
                agent_call("Implement it", "/tmp/project", None),
            )],
            outputs: vec![],
        };
        let errors = validate_root_directory_input(&plan);
        assert!(errors.iter().any(|error| {
            error.field.as_deref() == Some("config.working_dir")
                && error.message.contains("AGENT_CALL")
                && error.message.contains("path derived from it")
        }));
    }

    #[test]
    fn plan_without_commands_does_not_require_root_directory() {
        let plan = valid_plan_one_step("echo");
        let catalog = catalog_with(&["echo"]);
        let errors = validate(&plan, &catalog);
        assert!(
            errors
                .iter()
                .all(|e| e.kind != ValidationErrorKind::MissingRootDirectoryInput),
            "TOOL_CALL-only plans should not require root_directory: {errors:?}"
        );
    }

    // ── CONDITION constraints ─────────────────────────────────────────────────

    fn condition_step(
        id: &str,
        expression: &str,
        true_steps: Vec<&str>,
        false_steps: Vec<&str>,
    ) -> PlanStep {
        make_step(
            id,
            StepConfig::Condition(crate::plan::types::ConditionConfig {
                expression: expression.to_owned(),
                true_steps: true_steps.into_iter().map(str::to_owned).collect(),
                false_steps: false_steps.into_iter().map(str::to_owned).collect(),
            }),
        )
    }

    fn with_deps(mut step: PlanStep, deps: Vec<&str>) -> PlanStep {
        step.depends_on = deps.into_iter().map(str::to_owned).collect();
        step
    }

    fn plan_of(steps: Vec<PlanStep>) -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![],
            config: Default::default(),
            steps,
            outputs: vec![],
        }
    }

    // ── FAN_OUT constraints ────────────────────────────────────────────────────

    fn producer_with_output(id: &str, output: &str) -> PlanStep {
        let mut step = make_step(id, tool_call("echo"));
        step.outputs = vec![PlanOutput {
            name: output.to_owned(),
            description: None,
            value_type: "array".to_owned(),
        }];
        step
    }

    fn fan_out_step(id: &str, over: &str, spawn_steps: Vec<&str>) -> PlanStep {
        make_step(
            id,
            StepConfig::FanOut(FanOutConfig {
                over: over.to_owned(),
                item_var: "url".to_owned(),
                spawn_steps: spawn_steps.into_iter().map(str::to_owned).collect(),
                until: None,
            }),
        )
    }

    #[test]
    fn fan_out_over_known_output_passes() {
        let plan = plan_of(vec![
            producer_with_output("extract", "urls"),
            with_deps(
                fan_out_step("fan", "extract.urls", vec!["body"]),
                vec!["extract"],
            ),
            make_step("body", tool_call("echo")),
        ]);
        let errors = validate_fan_out_constraints(&plan);
        assert!(errors.is_empty(), "unexpected: {errors:?}");
    }

    #[test]
    fn fan_out_until_rejects_an_empty_expression() {
        let producer = producer_with_output("extract", "urls");
        let mut fan = with_deps(
            fan_out_step("fan", "extract.urls", vec!["body"]),
            vec!["extract"],
        );
        let StepConfig::FanOut(config) = &mut fan.config else {
            unreachable!("helper must create FAN_OUT")
        };
        config.until = Some(String::new());
        let body = make_step("body", tool_call("echo"));
        let errors = validate_fan_out_constraints(&plan_of(vec![producer, fan, body]));

        assert!(errors.iter().any(|error| {
            error.field.as_deref() == Some("config.until")
                && error.message.contains("expression is empty")
        }));
    }

    #[test]
    fn fan_out_over_unknown_output_is_reported() {
        let plan = plan_of(vec![
            producer_with_output("extract", "links"),
            with_deps(
                fan_out_step("fan", "extract.urls", vec!["body"]),
                vec!["extract"],
            ),
            make_step("body", tool_call("echo")),
        ]);
        let errors = validate_fan_out_constraints(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::UnknownPlaceholder
                    && e.message.contains("it produces: links")),
            "expected unknown output error, got {errors:?}"
        );
    }

    #[test]
    fn fan_out_over_source_must_be_a_dependency() {
        let plan = plan_of(vec![
            producer_with_output("extract", "urls"),
            fan_out_step("fan", "extract.urls", vec!["body"]),
            make_step("body", tool_call("echo")),
        ]);
        let errors = validate_fan_out_constraints(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::InvalidStepConfig
                    && e.field.as_deref() == Some("depends_on")),
            "expected dependency error, got {errors:?}"
        );
    }

    #[test]
    fn fan_out_missing_spawn_step_is_reported() {
        let plan = plan_of(vec![
            producer_with_output("extract", "urls"),
            with_deps(
                fan_out_step("fan", "extract.urls", vec!["missing"]),
                vec!["extract"],
            ),
        ]);
        let errors = validate_fan_out_constraints(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::MissingDependency
                    && e.field.as_deref() == Some("config.spawn_steps")),
            "expected missing spawn error, got {errors:?}"
        );
    }

    #[test]
    fn main_flow_must_depend_on_fan_out_not_owned_body() {
        let plan = plan_of(vec![
            producer_with_output("extract", "urls"),
            with_deps(
                fan_out_step("fan", "extract.urls", vec!["fetch", "summarize"]),
                vec!["extract"],
            ),
            make_step("fetch", tool_call("echo")),
            with_deps(make_step("summarize", tool_call("echo")), vec!["fetch"]),
            with_deps(make_step("compose", tool_call("echo")), vec!["summarize"]),
        ]);
        let errors = validate_fan_out_constraints(&plan);
        assert!(
            errors.iter().any(|error| {
                error.step_id.as_deref() == Some("compose")
                    && error.message.contains("depend on 'fan'")
            }),
            "expected main-flow dependency error, got {errors:?}"
        );
    }

    #[test]
    fn fan_out_body_must_not_depend_on_owner() {
        let plan = plan_of(vec![
            producer_with_output("extract", "urls"),
            with_deps(
                fan_out_step("fan", "extract.urls", vec!["body"]),
                vec!["extract"],
            ),
            with_deps(make_step("body", tool_call("echo")), vec!["fan"]),
        ]);
        let errors = validate_fan_out_constraints(&plan);
        assert!(
            errors.iter().any(|error| {
                error.step_id.as_deref() == Some("body")
                    && error.message.contains("must not depend on its owner")
            }),
            "expected owner dependency error, got {errors:?}"
        );
    }

    fn prompt_step(id: &str, user_prompt: &str) -> PlanStep {
        make_step(
            id,
            StepConfig::PromptCall(PromptCallConfig {
                model: "test-model".to_owned(),
                system_prompt: None,
                user_prompt: user_prompt.to_owned(),
                output_field: "summary".to_owned(),
                max_tokens: Some(100),
                temperature: Some(0.0),
            }),
        )
    }

    #[test]
    fn prompt_cannot_consume_raw_fan_out_results_directly() {
        let mut body = make_step("fetch_body", tool_call("http-get"));
        body.outputs = vec![PlanOutput {
            name: "body".to_owned(),
            description: None,
            value_type: "string".to_owned(),
        }];
        let plan = plan_of(vec![
            producer_with_output("extract", "urls"),
            with_deps(
                fan_out_step("fetch_posts", "extract.urls", vec!["fetch_body"]),
                vec!["extract"],
            ),
            body,
            with_deps(
                prompt_step("summarize", "Summarize: ${step.fetch_posts.results}"),
                vec!["fetch_posts"],
            ),
        ]);

        let errors = validate_fan_out_constraints(&plan);
        assert!(errors.iter().any(|error| {
            error.step_id.as_deref() == Some("summarize")
                && error.kind == ValidationErrorKind::PromptCallConstraint
                && error
                    .message
                    .contains("per-item PROMPT_CALL as the final spawn step")
        }));
    }

    #[test]
    fn prompt_can_consume_fan_out_with_per_item_summary_result() {
        let mut body = make_step("fetch_body", tool_call("http-get"));
        body.outputs = vec![PlanOutput {
            name: "body".to_owned(),
            description: None,
            value_type: "string".to_owned(),
        }];
        let summary = with_deps(
            prompt_step("summarize_item", "Summarize: ${step.fetch_body.body}"),
            vec!["fetch_body"],
        );
        let plan = plan_of(vec![
            producer_with_output("extract", "urls"),
            with_deps(
                fan_out_step(
                    "fetch_posts",
                    "extract.urls",
                    vec!["fetch_body", "summarize_item"],
                ),
                vec!["extract"],
            ),
            body,
            summary,
            with_deps(
                prompt_step("aggregate", "Combine: ${step.fetch_posts.results}"),
                vec!["fetch_posts"],
            ),
        ]);

        let errors = validate_fan_out_constraints(&plan);
        assert!(errors.is_empty(), "unexpected: {errors:?}");
    }

    #[test]
    fn condition_with_dependent_branches_passes() {
        let plan = plan_of(vec![
            make_step("fetch", tool_call("echo")),
            with_deps(
                condition_step("gate", "${step.fetch.stdout} == ok", vec!["write"], vec![]),
                vec!["fetch"],
            ),
            with_deps(make_step("write", tool_call("echo")), vec!["gate"]),
        ]);
        let errors = validate_condition_constraints(&plan);
        assert!(errors.is_empty(), "unexpected: {errors:?}");
    }

    #[test]
    fn condition_branch_missing_step_reported() {
        let plan = plan_of(vec![condition_step(
            "gate",
            "1 == 1",
            vec!["ghost"],
            vec![],
        )]);
        let errors = validate_condition_constraints(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::MissingDependency)
        );
    }

    #[test]
    fn condition_branch_without_dependency_reported() {
        let plan = plan_of(vec![
            condition_step("gate", "1 == 1", vec!["write"], vec![]),
            make_step("write", tool_call("echo")), // no depends_on ["gate"]
        ]);
        let errors = validate_condition_constraints(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::InvalidStepConfig),
            "expected an InvalidStepConfig error, got {errors:?}"
        );
    }

    #[test]
    fn condition_branch_transitive_dependency_passes() {
        let plan = plan_of(vec![
            condition_step("gate", "1 == 1", vec!["late"], vec![]),
            with_deps(make_step("mid", tool_call("echo")), vec!["gate"]),
            with_deps(make_step("late", tool_call("echo")), vec!["mid"]),
        ]);
        let errors = validate_condition_constraints(&plan);
        assert!(errors.is_empty(), "unexpected: {errors:?}");
    }

    #[test]
    fn condition_empty_expression_reported() {
        let plan = plan_of(vec![condition_step("gate", "  ", vec![], vec![])]);
        let errors = validate_condition_constraints(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::InvalidStepConfig)
        );
    }

    #[test]
    fn condition_step_in_both_branches_reported() {
        let plan = plan_of(vec![
            condition_step("gate", "1 == 1", vec!["write"], vec!["write"]),
            with_deps(make_step("write", tool_call("echo")), vec!["gate"]),
        ]);
        let errors = validate_condition_constraints(&plan);
        assert!(
            errors.iter().any(|e| e.message.contains("both true_steps")),
            "expected a both-branches error, got {errors:?}"
        );
    }

    // ── Plan input constraints ────────────────────────────────────────────────

    fn plan_with_input(input: crate::plan::types::PlanInput) -> Plan {
        let mut plan = valid_plan_one_step("echo");
        plan.inputs = vec![input];
        plan
    }

    fn string_input(name: &str) -> crate::plan::types::PlanInput {
        crate::plan::types::PlanInput {
            name: name.to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: InputKind::Value,
        }
    }

    #[test]
    fn well_named_string_input_passes() {
        let errors = validate_structure(&plan_with_input(string_input("query_1-a")));
        assert!(errors.is_empty(), "unexpected: {errors:?}");
    }

    #[test]
    fn input_name_starting_with_digit_is_rejected() {
        let errors = validate_structure(&plan_with_input(string_input("1query")));
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::InvalidStepConfig
                    && e.message.contains("must start with a letter or underscore")),
            "expected invalid-name error, got {errors:?}"
        );
    }

    #[test]
    fn duplicate_input_name_is_rejected() {
        let mut plan = valid_plan_one_step("echo");
        plan.inputs = vec![string_input("query"), string_input("query")];
        let errors = validate_structure(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("duplicate plan input name: query")),
            "expected duplicate-input error, got {errors:?}"
        );
    }

    #[test]
    fn unsupported_input_type_is_rejected() {
        let mut input = string_input("query");
        input.value_type = "text".to_owned();
        let errors = validate_structure(&plan_with_input(input));
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::TypeMismatch
                    && e.message.contains("unsupported type 'text'")),
            "expected unsupported-type error, got {errors:?}"
        );
    }

    #[test]
    fn path_input_kind_requires_string_value_type() {
        let mut input = string_input("output_path");
        input.value_type = "integer".to_owned();
        input.input_kind = InputKind::OutputFilePath;

        let errors = validate_structure(&plan_with_input(input));

        assert!(errors.iter().any(|error| {
            error.kind == ValidationErrorKind::TypeMismatch
                && error.message.contains("path input kind 'output_file_path'")
        }));
    }

    #[test]
    fn input_default_must_match_declared_type() {
        let mut input = string_input("query");
        input.default = Some(serde_json::json!(42));
        let errors = validate_structure(&plan_with_input(input));
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::TypeMismatch
                    && e.message.contains("default for plan input 'query'")),
            "expected default-type error, got {errors:?}"
        );
    }

    // ── PROMPT_CALL constraints ───────────────────────────────────────────────

    #[test]
    fn complete_prompt_call_passes() {
        let plan = plan_of(vec![prompt_step("summarize", "Summarize this")]);
        let errors = validate_step_contracts(&plan);
        assert!(errors.is_empty(), "unexpected: {errors:?}");
    }

    #[test]
    fn prompt_call_missing_required_fields_is_reported_per_field() {
        let mut step = prompt_step("summarize", "");
        let StepConfig::PromptCall(cfg) = &mut step.config else {
            unreachable!("helper must create PROMPT_CALL")
        };
        cfg.model = String::new();
        cfg.output_field = String::new();
        let errors = validate_step_contracts(&plan_of(vec![step]));

        let flagged_fields: Vec<&str> = errors
            .iter()
            .filter(|e| e.kind == ValidationErrorKind::PromptCallConstraint)
            .filter_map(|e| e.field.as_deref())
            .collect();
        assert_eq!(
            flagged_fields,
            vec!["config.model", "config.user_prompt", "config.output_field"],
            "expected one error per empty field, got {errors:?}"
        );
    }

    // ── FAN_OUT edge cases ────────────────────────────────────────────────────

    #[test]
    fn fan_out_can_own_agent_call() {
        let mut plan = plan_of(vec![
            producer_with_output("extract", "items"),
            with_deps(
                fan_out_step("fan", "extract.items", vec!["agent"]),
                vec!["extract"],
            ),
            make_step(
                "agent",
                agent_call("Process ${item.url}", "${input.root_directory}", None),
            ),
        ]);
        plan.inputs = vec![required_root_directory_input()];

        let errors = validate(&plan, &catalog_with(&["echo"]));
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[test]
    fn fan_out_over_without_dot_is_rejected() {
        let plan = plan_of(vec![
            producer_with_output("extract", "urls"),
            with_deps(fan_out_step("fan", "urls", vec!["body"]), vec!["extract"]),
            make_step("body", tool_call("echo")),
        ]);
        let errors = validate_fan_out_constraints(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.field.as_deref() == Some("config.over")
                    && e.message.contains("expected '<step-id>.<output-name>'")),
            "expected malformed-over error, got {errors:?}"
        );
    }

    #[test]
    fn fan_out_over_missing_source_step_is_rejected() {
        let plan = plan_of(vec![
            fan_out_step("fan", "ghost.urls", vec!["body"]),
            make_step("body", tool_call("echo")),
        ]);
        let errors = validate_fan_out_constraints(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::MissingDependency
                    && e.message.contains("step 'ghost' does not exist")),
            "expected missing-source error, got {errors:?}"
        );
    }

    #[test]
    fn fan_out_with_empty_spawn_list_is_rejected() {
        let plan = plan_of(vec![
            producer_with_output("extract", "urls"),
            with_deps(fan_out_step("fan", "extract.urls", vec![]), vec!["extract"]),
        ]);
        let errors = validate_fan_out_constraints(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("must list at least one spawn step")),
            "expected empty-spawn error, got {errors:?}"
        );
    }

    #[test]
    fn fan_out_cannot_spawn_itself() {
        let plan = plan_of(vec![
            producer_with_output("extract", "urls"),
            with_deps(
                fan_out_step("fan", "extract.urls", vec!["fan"]),
                vec!["extract"],
            ),
        ]);
        let errors = validate_fan_out_constraints(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("cannot spawn itself")),
            "expected self-spawn error, got {errors:?}"
        );
    }

    #[test]
    fn fan_out_cannot_own_human_interaction_step() {
        let ask = make_step(
            "ask",
            StepConfig::HumanInteraction(crate::plan::types::HumanInteractionConfig {
                prompt: "ok?".to_owned(),
                response_field: "answer".to_owned(),
                approval_required: false,
            }),
        );
        let plan = plan_of(vec![
            producer_with_output("extract", "urls"),
            with_deps(
                fan_out_step("fan", "extract.urls", vec!["ask"]),
                vec!["extract"],
            ),
            ask,
        ]);
        let errors = validate_fan_out_constraints(&plan);
        assert!(
            errors.iter().any(|e| e
                .message
                .contains("cannot own HUMAN_INTERACTION step 'ask'")),
            "expected human-interaction error, got {errors:?}"
        );
    }

    #[test]
    fn condition_cannot_branch_on_terminating_approval() {
        let approval = make_step(
            "approve",
            StepConfig::HumanInteraction(crate::plan::types::HumanInteractionConfig {
                prompt: "Approve or reject?".to_owned(),
                response_field: "decision".to_owned(),
                approval_required: true,
            }),
        );
        let plan = plan_of(vec![
            approval,
            with_deps(
                condition_step(
                    "gate",
                    "${step.approve.decision} == true",
                    vec!["accepted"],
                    vec!["rejected"],
                ),
                vec!["approve"],
            ),
            with_deps(make_step("accepted", tool_call("echo")), vec!["gate"]),
            with_deps(make_step("rejected", tool_call("echo")), vec!["gate"]),
        ]);

        let errors = validate_condition_constraints(&plan);
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("can never observe a rejected value")),
            "expected approval-branch error, got {errors:?}"
        );
    }

    #[test]
    fn code_call_requires_exactly_one_non_empty_source() {
        let mut step = code_call_step("run", None);
        let StepConfig::CodeCall(config) = &mut step.config else {
            unreachable!("helper creates CODE_CALL")
        };
        config.inline = None;
        config.file = None;

        let errors = validate_step_contracts(&plan_of(vec![step]));

        assert!(errors.iter().any(|error| {
            error.field.as_deref() == Some("config")
                && error.message.contains("exactly one non-empty source")
        }));
    }

    #[test]
    fn code_call_rejects_unsupported_language_and_zero_timeouts() {
        let mut step = code_call_step("run", None);
        step.timeout_secs = Some(0);
        let StepConfig::CodeCall(config) = &mut step.config else {
            unreachable!("helper creates CODE_CALL")
        };
        config.language = "ruby".to_owned();
        config.timeout_secs = Some(0);

        let errors = validate_step_contracts(&plan_of(vec![step]));

        assert!(errors.iter().any(|error| {
            error.field.as_deref() == Some("config.language")
                && error.message.contains("unsupported language")
        }));
        assert_eq!(
            errors
                .iter()
                .filter(|error| error.message.contains("greater than zero"))
                .count(),
            2
        );
    }

    fn code_call_with_inline(inline: &str) -> PlanStep {
        let mut step = code_call_step("run", None);
        let StepConfig::CodeCall(config) = &mut step.config else {
            unreachable!("helper creates CODE_CALL")
        };
        config.inline = Some(inline.to_owned());
        step
    }

    /// Errors from the CODE_CALL static-source check only — the shared step
    /// helpers omit the root_directory input, whose (unrelated) error would
    /// otherwise pollute count assertions.
    fn static_source_errors(step: PlanStep) -> Vec<ValidationError> {
        validate_step_contracts(&plan_of(vec![step]))
            .into_iter()
            .filter(|error| error.message.contains("must be static"))
            .collect()
    }

    #[test]
    fn code_call_inline_with_plan_placeholder_is_rejected() {
        let step = code_call_with_inline(
            "import json\ncompiled = json.loads('${step.step_bench_compiled.stdout}')",
        );

        let errors = static_source_errors(step);

        assert_eq!(errors.len(), 1, "got {errors:?}");
        assert_eq!(errors[0].kind, ValidationErrorKind::InvalidStepConfig);
        assert_eq!(errors[0].field.as_deref(), Some("config.inline"));
        assert!(errors[0].message.contains("step 'run'"), "{}", errors[0]);
        assert!(
            errors[0]
                .message
                .contains("'${step.step_bench_compiled.stdout}'"),
            "{}",
            errors[0]
        );
        assert!(
            errors[0].message.contains("args, env, or stdin"),
            "{}",
            errors[0]
        );
    }

    #[test]
    fn code_call_file_with_plan_placeholder_is_rejected() {
        let mut step = code_call_step("run", None);
        let StepConfig::CodeCall(config) = &mut step.config else {
            unreachable!("helper creates CODE_CALL")
        };
        config.inline = None;
        config.file = Some("${input.script_path}".to_owned());

        let errors = validate_step_contracts(&plan_of(vec![step]));

        assert!(errors.iter().any(|error| {
            error.field.as_deref() == Some("config.file")
                && error.message.contains("'${input.script_path}'")
        }));
    }

    #[test]
    fn code_call_static_inline_with_placeholders_in_args_passes() {
        let mut step = code_call_with_inline("import sys, json\nprint(json.loads(sys.argv[1]))");
        let StepConfig::CodeCall(config) = &mut step.config else {
            unreachable!("helper creates CODE_CALL")
        };
        config.args = vec!["${step.fetch.stdout}".to_owned()];
        config.stdin = Some("${input.payload}".to_owned());

        assert!(static_source_errors(step).is_empty());
    }

    #[test]
    fn code_call_inline_script_own_dollar_brace_syntax_passes() {
        // Bash parameter expansion and JS template literals are the script's
        // own syntax — the executor passes them through verbatim.
        let step = code_call_with_inline(
            r#"datetime=${BODY#*x}; echo "${datetime%%y}"; node -e 'console.log(`${value}`)'"#,
        );

        assert!(static_source_errors(step).is_empty());
    }

    #[test]
    fn code_call_inline_has_no_dollar_dollar_escape() {
        // There is no `$$` escaping concept: the executor's resolver still
        // matches the inner `${step...}`, so the validator must flag it too.
        let step = code_call_with_inline("echo $${step.fetch.stdout}");

        let errors = validate_step_contracts(&plan_of(vec![step]));

        assert!(errors.iter().any(|error| {
            error.field.as_deref() == Some("config.inline")
                && error.message.contains("'${step.fetch.stdout}'")
        }));
    }

    #[test]
    fn code_call_inline_placeholder_with_shell_default_is_rejected() {
        // The executor matches any `${...}` content, not just well-formed
        // placeholder names — mirror that so validation predicts run time.
        let step = code_call_with_inline("echo ${step.fetch.stdout:-fallback}");

        let errors = validate_step_contracts(&plan_of(vec![step]));

        assert!(errors.iter().any(|error| {
            error.field.as_deref() == Some("config.inline")
                && error.message.contains("'${step.fetch.stdout:-fallback}'")
        }));
    }

    #[test]
    fn code_call_inline_repeated_placeholder_is_reported_once() {
        let step = code_call_with_inline("echo ${step.a.out}; echo ${step.a.out}; echo ${input.x}");

        let errors = static_source_errors(step);

        assert_eq!(errors.len(), 2, "one per distinct placeholder: {errors:?}");
    }

    #[test]
    fn required_root_directory_rejects_fixed_working_directory() {
        let mut plan = plan_of(vec![code_call_step("run", Some("/tmp/fixed"))]);
        plan.inputs = vec![crate::plan::types::PlanInput {
            name: "root_directory".to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: InputKind::DirectoryPath,
        }];

        let errors = validate_root_directory_input(&plan);

        assert!(errors.iter().any(|error| {
            error.field.as_deref() == Some("config.working_dir")
                && error.message.contains("must be '${input.root_directory}'")
        }));
    }

    #[test]
    fn optional_root_directory_allows_managed_scratch_workspace() {
        let mut plan = plan_of(vec![code_call_step("run", None)]);
        plan.inputs = vec![crate::plan::types::PlanInput {
            name: "root_directory".to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: false,
            default: None,
            input_kind: InputKind::DirectoryPath,
        }];

        assert!(validate_root_directory_input(&plan).is_empty());
    }

    #[test]
    fn derived_root_directory_path_is_valid() {
        let mut plan = plan_of(vec![code_call_step(
            "run",
            Some("${input.root_directory}/subdir"),
        )]);
        plan.inputs = vec![crate::plan::types::PlanInput {
            name: "root_directory".to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: InputKind::DirectoryPath,
        }];

        assert!(validate_root_directory_input(&plan).is_empty());
    }

    #[test]
    fn fan_in_requires_existing_ordered_sources() {
        let source = make_step("source", tool_call("echo"));
        let fan_in = make_step(
            "collect",
            StepConfig::FanIn(crate::plan::types::FanInConfig {
                from_steps: vec!["source".to_owned(), "missing".to_owned()],
                collect_field: "results".to_owned(),
            }),
        );

        let errors = validate_step_contracts(&plan_of(vec![source, fan_in]));

        assert!(errors.iter().any(|error| {
            error.field.as_deref() == Some("depends_on")
                && error
                    .message
                    .contains("must depend on source step 'source'")
        }));
        assert!(errors.iter().any(|error| {
            error.kind == ValidationErrorKind::MissingDependency
                && error.message.contains("'missing' does not exist")
        }));
    }

    #[test]
    fn fan_in_with_transitive_sources_is_valid() {
        let source = make_step("source", tool_call("echo"));
        let middle = with_deps(make_step("middle", tool_call("echo")), vec!["source"]);
        let fan_in = with_deps(
            make_step(
                "collect",
                StepConfig::FanIn(crate::plan::types::FanInConfig {
                    from_steps: vec!["source".to_owned(), "middle".to_owned()],
                    collect_field: "results".to_owned(),
                }),
            ),
            vec!["middle"],
        );

        let errors = validate_step_contracts(&plan_of(vec![source, middle, fan_in]));

        assert!(errors.is_empty(), "unexpected: {errors:?}");
    }

    #[test]
    fn fan_out_rejects_multiple_owners_and_reversed_body_order() {
        let producer = producer_with_output("extract", "urls");
        let first_owner = with_deps(
            fan_out_step("fan-a", "extract.urls", vec!["later", "early"]),
            vec!["extract"],
        );
        let second_owner = with_deps(
            fan_out_step("fan-b", "extract.urls", vec!["early"]),
            vec!["extract"],
        );
        let early = make_step("early", tool_call("echo"));
        let later = with_deps(make_step("later", tool_call("echo")), vec!["early"]);

        let errors = validate_fan_out_constraints(&plan_of(vec![
            producer,
            first_owner,
            second_owner,
            early,
            later,
        ]));

        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("multiple owners"))
        );
        assert!(errors.iter().any(|error| {
            error
                .message
                .contains("which is not earlier in owner 'fan-a'")
        }));
    }

    #[test]
    fn present_output_declarations_must_include_configured_output() {
        let mut prompt = prompt_step("summarize", "Summarize");
        prompt.outputs = vec![PlanOutput {
            name: "other".to_owned(),
            description: None,
            value_type: "string".to_owned(),
        }];

        let errors = validate_step_contracts(&plan_of(vec![prompt]));

        assert!(errors.iter().any(|error| {
            error.field.as_deref() == Some("outputs")
                && error
                    .message
                    .contains("omits its configured output 'summary'")
        }));
    }

    #[test]
    fn every_step_config_variant_has_a_contract_failure_path() {
        let variants = vec![
            make_step("tool", tool_call("missing")),
            make_step(
                "code",
                StepConfig::CodeCall(crate::plan::types::CodeCallConfig {
                    language: String::new(),
                    inline: None,
                    file: None,
                    args: vec![],
                    stdin: None,
                    env: Default::default(),
                    working_dir: None,
                    timeout_secs: None,
                }),
            ),
            make_step(
                "human",
                StepConfig::HumanInteraction(crate::plan::types::HumanInteractionConfig {
                    prompt: String::new(),
                    response_field: String::new(),
                    approval_required: false,
                }),
            ),
            fan_out_step("fan-out", "bad", vec![]),
            make_step(
                "fan-in",
                StepConfig::FanIn(crate::plan::types::FanInConfig {
                    from_steps: vec![],
                    collect_field: String::new(),
                }),
            ),
            make_step(
                "prompt",
                StepConfig::PromptCall(PromptCallConfig {
                    model: String::new(),
                    system_prompt: None,
                    user_prompt: String::new(),
                    output_field: String::new(),
                    max_tokens: None,
                    temperature: None,
                }),
            ),
            condition_step("condition", "", vec![], vec![]),
            make_step("agent", agent_call("", "/fixed", Some(0))),
        ];
        let plan = plan_of(variants);

        let errors = validate(&plan, &ToolCatalog::default());

        for step_id in [
            "tool",
            "code",
            "human",
            "fan-out",
            "fan-in",
            "prompt",
            "condition",
            "agent",
        ] {
            assert!(
                errors
                    .iter()
                    .any(|error| error.step_id.as_deref() == Some(step_id)),
                "missing contract failure for {step_id}: {errors:?}"
            );
        }
    }

    #[test]
    fn accepted_step_identifier_round_trips_through_placeholder_validation() {
        let producer = producer_with_output("fetch-page_1", "body");
        let mut consumer = make_step(
            "consume",
            StepConfig::ToolCall(ToolCallConfig {
                tool: "echo".to_owned(),
                arguments: indexmap::IndexMap::from([(
                    "value".to_owned(),
                    serde_json::json!("${step.fetch-page_1.body}"),
                )]),
            }),
        );
        consumer.depends_on = vec!["fetch-page_1".to_owned()];
        let plan = plan_of(vec![producer, consumer]);

        assert!(validate_structure(&plan).is_empty());
        assert!(placeholders::validate_placeholders(&plan).is_empty());
    }

    #[test]
    fn dotted_step_identifier_is_rejected_as_unaddressable() {
        let plan = plan_of(vec![make_step("fetch.page", tool_call("echo"))]);

        let errors = validate_structure(&plan);

        assert!(errors.iter().any(|error| {
            error.kind == ValidationErrorKind::InvalidStepConfig
                && error.message.contains("step ID 'fetch.page'")
        }));
    }
}
