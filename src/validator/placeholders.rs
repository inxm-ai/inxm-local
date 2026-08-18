//! Placeholder validation.
//!
//! Plans may reference values via placeholders:
//! - `${input.<name>}` — resolved from values supplied for this run
//! - `${conf.<key>}` — resolved from static `plan.config`
//! - `${step.<step-id>.<output-name>}` — resolved at runtime from a prior step's outputs
//! - `${env.<VAR>}` — resolved from environment variables at runtime
//! - `${item.<item_var>}` — resolved inside a FAN_OUT-owned step
//!
//! Compile-time checks:
//! - Malformed placeholders (no closing `}`, invalid chars)
//! - Unknown `${input.*}` and `${conf.*}` keys
//! - Unknown steps or outputs in `${step.*}` references
//! - `${item.*}` references match the owning FAN_OUT's configured `item_var`
//!
//! Environment references are checked at runtime.

use crate::error::{ValidationError, ValidationErrorKind};
use crate::plan::types::{Plan, PlanStep, StepConfig};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// The fixed output name under which a FAN_OUT step collects its per-item
/// results (see fan.rs in the executor).
pub(super) const FAN_OUT_RESULTS_OUTPUT: &str = "results";

// Matches a well-formed `${...}` placeholder: `${ns.path.parts}`
static PLACEHOLDER_RE: OnceLock<Regex> = OnceLock::new();
// Matches any `${` that starts a placeholder (to find malformed ones)
static OPEN_RE: OnceLock<Regex> = OnceLock::new();

fn placeholder_re() -> &'static Regex {
    PLACEHOLDER_RE.get_or_init(|| {
        Regex::new(r"\$\{([a-zA-Z_][a-zA-Z0-9_.\-]*)\}").expect("placeholder regex is valid")
    })
}

fn open_re() -> &'static Regex {
    OPEN_RE.get_or_init(|| Regex::new(r"\$\{").expect("open regex is valid"))
}

/// Extract all `${...}` placeholder names from a string.
fn extract_placeholders(s: &str) -> Vec<&str> {
    placeholder_re()
        .captures_iter(s)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str())
        .collect()
}

/// Return the placeholder name when the JSON value is exactly one
/// well-formed placeholder. Whole-value placeholders preserve their JSON type
/// at runtime; interpolated strings do not.
pub(super) fn exact_placeholder_name(value: &serde_json::Value) -> Option<&str> {
    let value = value.as_str()?;
    let captures = placeholder_re().captures(value)?;
    (captures.get(0)?.as_str() == value).then(|| captures.get(1).expect("capture exists").as_str())
}

/// Return true if a string contains `${` that is NOT followed by a valid placeholder.
fn has_malformed_placeholder(s: &str) -> bool {
    let open_count = open_re().find_iter(s).count();
    let valid_count = placeholder_re().find_iter(s).count();
    open_count > valid_count
}

/// The namespaces reserved for plan placeholders. In code-bearing strings
/// (CODE_CALL source, args, stdin, env) any `${...}` outside these namespaces
/// is the script's own syntax — bash parameter expansion, JavaScript template
/// literals — and is passed through verbatim by the executor rather than
/// treated as a plan placeholder.
fn is_reserved_namespace(ph: &str) -> bool {
    matches!(
        ph.split('.').next(),
        Some("input" | "conf" | "step" | "env" | "item")
    )
}

// Matches `${...}` the way the executor's resolver does (any non-empty
// content), so the CODE_CALL static-source check predicts exactly what
// `reject_executable_placeholders` in the CODE_CALL runner will refuse.
static EXECUTOR_PLACEHOLDER_RE: OnceLock<Regex> = OnceLock::new();

fn executor_placeholder_re() -> &'static Regex {
    EXECUTOR_PLACEHOLDER_RE
        .get_or_init(|| Regex::new(r"\$\{([^}]+)\}").expect("executor placeholder regex is valid"))
}

/// Plan placeholders found in an executable source string, using the
/// executor's matching rules: any `${...}` whose content starts with a
/// reserved namespace followed by a dot (see `contains_plan_placeholder` in
/// the executor). CODE_CALL `inline`/`file` must stay static — the runner
/// refuses to substitute into them — so every match here is a guaranteed
/// run-time failure. Returns the full `${...}` texts, in order of appearance.
pub(super) fn plan_placeholders_in_source(source: &str) -> Vec<&str> {
    executor_placeholder_re()
        .captures_iter(source)
        .filter_map(|caps| {
            let key = caps.get(1).expect("capture exists").as_str();
            ["input.", "conf.", "step.", "env.", "item."]
                .iter()
                .any(|ns| key.starts_with(ns))
                .then(|| caps.get(0).expect("match exists").as_str())
        })
        .collect()
}

/// Validate all placeholders in a plan.
pub fn validate_placeholders(plan: &Plan) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    let input_names: HashSet<&str> = plan
        .inputs
        .iter()
        .map(|input| input.name.as_str())
        .collect();
    let conf_keys: HashSet<&str> = plan.config.keys().map(String::as_str).collect();
    let step_ids: HashSet<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();
    let outputs_by_step: HashMap<&str, Vec<String>> = plan
        .steps
        .iter()
        .map(|s| (s.id.as_str(), producible_outputs(s)))
        .collect();
    let mut fan_out_owners: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
    for owner in &plan.steps {
        if let StepConfig::FanOut(config) = &owner.config {
            for spawn_id in &config.spawn_steps {
                fan_out_owners
                    .entry(spawn_id.as_str())
                    .or_default()
                    .push((owner.id.as_str(), config.item_var.as_str()));
            }
        }
    }

    for step in &plan.steps {
        let strings = collect_strings_from_config(&step.config);
        for (s, is_code) in &strings {
            // Check for malformed placeholders first. Code-bearing strings
            // are exempt: their `${...}` may be the script language's own
            // syntax (e.g. bash `${VAR%%pattern}`), which the executor
            // passes through verbatim.
            if !is_code && has_malformed_placeholder(s) {
                errors.push(ValidationError::step(
                    &step.id,
                    ValidationErrorKind::MalformedPlaceholder,
                    format!("malformed placeholder (missing '}}') in step '{}'", step.id),
                ));
            }

            for ph in extract_placeholders(s) {
                let parts: Vec<&str> = ph.splitn(3, '.').collect();
                match parts.as_slice() {
                    ["input", name] => {
                        if !input_names.contains(name) {
                            errors.push(ValidationError::step(
                                &step.id,
                                ValidationErrorKind::UnknownPlaceholder,
                                format!(
                                    "step '{}' references '${{{ph}}}' but '{name}' is not declared in plan.inputs",
                                    step.id
                                ),
                            ));
                        }
                    }
                    ["conf", key] => {
                        if !conf_keys.contains(key) {
                            errors.push(ValidationError::step(
                                &step.id,
                                ValidationErrorKind::UnknownPlaceholder,
                                format!(
                                    "step '{}' references '${{{ph}}}' but '{key}' is not defined in plan.config",
                                    step.id
                                ),
                            ));
                        }
                    }
                    ["step", step_ref, output] => {
                        if !step_ids.contains(step_ref) {
                            errors.push(ValidationError::step(
                                &step.id,
                                ValidationErrorKind::UnknownPlaceholder,
                                format!(
                                    "step '{}' references '${{{ph}}}' but step '{step_ref}' does not exist",
                                    step.id
                                ),
                            ));
                        } else if let Some(known) = outputs_by_step.get(step_ref)
                            && !known.iter().any(|name| name == output)
                        {
                            // The output contract: references must name a
                            // declared (or type-implicit) output of the
                            // producing step. This is what lets a repair be
                            // verified before it is accepted.
                            let available = describe_available_outputs(known);
                            errors.push(ValidationError::step(
                                &step.id,
                                ValidationErrorKind::UnknownPlaceholder,
                                format!(
                                    "step '{}' references '${{{ph}}}' but {available}",
                                    step.id
                                ),
                            ));
                        }
                    }
                    ["env", _var] => {
                        // env refs are checked at runtime — no compile-time validation
                    }
                    ["item", var] => match fan_out_owners.get(step.id.as_str()) {
                        None => errors.push(ValidationError::step(
                            &step.id,
                            ValidationErrorKind::UnknownPlaceholder,
                            format!(
                                "step '{}' references '${{{ph}}}', but item placeholders are only available in FAN_OUT-owned spawn steps",
                                step.id
                            ),
                        )),
                        Some(owners) => {
                            // Invariant: owner lists are only created via
                            // `entry(..).or_default().push(..)`, so `owners`
                            // is never empty here.
                            let expected_var = owners[0].1;
                            let conflicting_owner = owners
                                .iter()
                                .skip(1)
                                .find(|(_, item_var)| *item_var != expected_var);

                            if let Some((other_owner_id, other_var)) = conflicting_owner {
                                let (owner_id, _) = owners[0];
                                errors.push(ValidationError::step(
                                    &step.id,
                                    ValidationErrorKind::InvalidStepConfig,
                                    format!(
                                        "step '{}' references '${{{ph}}}', but it is owned by FAN_OUT '{}' injecting '${{item.{expected_var}}}' and FAN_OUT '{}' injecting '${{item.{other_var}}}'",
                                        step.id, owner_id, other_owner_id
                                    ),
                                ));
                            } else if *var != expected_var {
                                let (owner_id, _) = owners[0];
                                errors.push(ValidationError::step(
                                    &step.id,
                                    ValidationErrorKind::UnknownPlaceholder,
                                    format!(
                                        "step '{}' references '${{{ph}}}', but FAN_OUT '{}' injects '${{item.{expected_var}}}'",
                                        step.id, owner_id
                                    ),
                                ));
                            }
                        }
                    }
                    _ => {
                        // In code-bearing strings, `${...}` outside the
                        // reserved namespaces is script syntax, not a plan
                        // placeholder — leave it alone.
                        if !is_code || is_reserved_namespace(ph) {
                            errors.push(ValidationError::step(
                                &step.id,
                                ValidationErrorKind::MalformedPlaceholder,
                                format!(
                                    "step '{}' contains unrecognised placeholder namespace in '${{{ph}}}' — expected input, conf, step, item, or env",
                                    step.id
                                ),
                            ));
                        }
                    }
                }
            }
        }
    }

    errors.extend(validate_step_reference_order(plan));
    errors
}

/// Validate that every step output used during preflight is available in the
/// execution phase where the consumer runs.
fn validate_step_reference_order(plan: &Plan) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let owners = fan_out_owners(plan);

    for step in &plan.steps {
        let preflight = crate::plan::steps::runtime_preflight_config(step);
        let mut strings = Vec::new();
        collect_from_value(&preflight, &mut strings);
        for reference in strings
            .iter()
            .flat_map(|value| extract_placeholders(value))
            .filter_map(parse_step_reference)
        {
            if !reference_is_available(plan, step, reference.0, &owners) {
                errors.push(ValidationError::field(
                    &step.id,
                    "config",
                    ValidationErrorKind::MissingDependency,
                    format!(
                        "step '{}' references '${{step.{}.{}}}' before step '{}' is available — add an execution-order dependency",
                        step.id, reference.0, reference.1, reference.0
                    ),
                ));
            }
        }

        let StepConfig::FanOut(config) = &step.config else {
            continue;
        };
        if let Some(until) = &config.until {
            for (source_id, output_name) in extract_placeholders(until)
                .into_iter()
                .filter_map(parse_step_reference)
            {
                let produced_by_body = config.spawn_steps.iter().any(|id| id == source_id);
                let available_before_owner = super::depends_transitively(plan, &step.id, source_id);
                if !produced_by_body && !available_before_owner {
                    errors.push(ValidationError::field(
                        &step.id,
                        "config.until",
                        ValidationErrorKind::MissingDependency,
                        format!(
                            "FAN_OUT step '{}' until expression references '${{step.{source_id}.{output_name}}}', but '{source_id}' is neither an owned body step nor an owner dependency",
                            step.id
                        ),
                    ));
                }
            }
        }
    }

    errors
}

fn parse_step_reference(placeholder: &str) -> Option<(&str, &str)> {
    let rest = placeholder.strip_prefix("step.")?;
    rest.split_once('.')
}

fn fan_out_owners(plan: &Plan) -> HashMap<&str, Vec<(&PlanStep, usize)>> {
    let mut owners: HashMap<&str, Vec<(&PlanStep, usize)>> = HashMap::new();
    for owner in &plan.steps {
        if let StepConfig::FanOut(config) = &owner.config {
            for (position, spawn_id) in config.spawn_steps.iter().enumerate() {
                owners
                    .entry(spawn_id.as_str())
                    .or_default()
                    .push((owner, position));
            }
        }
    }
    owners
}

fn reference_is_available(
    plan: &Plan,
    consumer: &PlanStep,
    source_id: &str,
    owners: &HashMap<&str, Vec<(&PlanStep, usize)>>,
) -> bool {
    if source_id == consumer.id {
        return false;
    }

    if let Some(consumer_owners) = owners.get(consumer.id.as_str()) {
        let [(owner, consumer_position)] = consumer_owners.as_slice() else {
            // Multiple ownership is reported by the FAN_OUT contract pass.
            return true;
        };
        let source_is_earlier_body_step =
            owners.get(source_id).is_some_and(|source_owners| {
                source_owners.iter().any(|(source_owner, source_position)| {
                    source_owner.id == owner.id && source_position < consumer_position
                })
            }) && super::depends_transitively(plan, &consumer.id, source_id);
        return source_is_earlier_body_step
            || super::depends_transitively(plan, &owner.id, source_id);
    }

    !owners.contains_key(source_id) && super::depends_transitively(plan, &consumer.id, source_id)
}

/// Validate the plan's top-level `outputs` — each entry's `source` must be a
/// single, well-formed `${step.<step_id>.<output_name>}` reference to an
/// existing step and one of its declared/implicit outputs.
pub fn validate_plan_output_sources(plan: &Plan) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    let step_ids: HashSet<&str> = plan.steps.iter().map(|s| s.id.as_str()).collect();
    let outputs_by_step: HashMap<&str, Vec<String>> = plan
        .steps
        .iter()
        .map(|s| (s.id.as_str(), producible_outputs(s)))
        .collect();

    for output in &plan.outputs {
        let whole_match = placeholder_re()
            .captures(&output.source)
            .filter(|cap| cap.get(0).map(|m| m.as_str()) == Some(output.source.as_str()));
        let Some(cap) = whole_match else {
            errors.push(ValidationError::plan(
                ValidationErrorKind::MalformedPlaceholder,
                format!(
                    "plan output '{}' has source '{}' — expected a single '${{step.<step_id>.<output_name>}}' reference",
                    output.name, output.source
                ),
            ));
            continue;
        };
        let ph = cap[1].to_owned();
        let parts: Vec<&str> = ph.splitn(3, '.').collect();
        match parts.as_slice() {
            ["step", step_ref, out_name] => {
                if !step_ids.contains(step_ref) {
                    errors.push(ValidationError::plan(
                        ValidationErrorKind::UnknownPlaceholder,
                        format!(
                            "plan output '{}' references '${{{ph}}}' but step '{step_ref}' does not exist",
                            output.name
                        ),
                    ));
                } else if let Some(known) = outputs_by_step.get(step_ref)
                    && !known.iter().any(|name| name == out_name)
                {
                    let available = describe_available_outputs(known);
                    errors.push(ValidationError::plan(
                        ValidationErrorKind::UnknownPlaceholder,
                        format!(
                            "plan output '{}' references '${{{ph}}}' but {available}",
                            output.name
                        ),
                    ));
                }
            }
            _ => {
                errors.push(ValidationError::plan(
                    ValidationErrorKind::UnknownPlaceholder,
                    format!(
                        "plan output '{}' has source '${{{ph}}}' — plan outputs must reference a step result, e.g. '${{step.<step_id>.<output_name>}}'",
                        output.name
                    ),
                ));
            }
        }
    }

    errors
}

/// Output names a step can produce at runtime: its declared `outputs` plus
/// the field its config type implicitly writes to. The executor guarantees a
/// single declared output is filled with the step's primary payload.
pub(super) fn producible_outputs(step: &PlanStep) -> Vec<String> {
    let implicit: Vec<String> = match &step.config {
        StepConfig::HumanInteraction(c) => vec![c.response_field.clone()],
        StepConfig::PromptCall(c) => vec![c.output_field.clone()],
        StepConfig::FanIn(c) => vec![c.collect_field.clone()],
        StepConfig::FanOut(_) => vec![FAN_OUT_RESULTS_OUTPUT.to_owned()],
        StepConfig::Condition(_) => vec!["result".to_owned()],
        _ => Vec::new(),
    };
    let mut names: Vec<String> = step
        .outputs
        .iter()
        .map(|o| o.name.clone())
        .chain(implicit)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Human-readable description of what a step can produce, for "unknown
/// output" error messages.
pub(super) fn describe_available_outputs(known: &[String]) -> String {
    match known.is_empty() {
        true => "it declares no outputs — add one to its `outputs` list".to_owned(),
        false => format!("it produces: {}", known.join(", ")),
    }
}

/// Walk a StepConfig and collect all string values for placeholder scanning.
/// The `bool` marks code-bearing strings (CODE_CALL source, args, stdin, env),
/// where non-reserved `${...}` is script syntax and must not be validated as
/// a plan placeholder.
fn collect_strings_from_config(config: &StepConfig) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    match config {
        StepConfig::ToolCall(c) => {
            let mut strings = Vec::new();
            for v in c.arguments.values() {
                collect_from_value(v, &mut strings);
            }
            out.extend(strings.into_iter().map(|s| (s, false)));
        }
        StepConfig::CodeCall(c) => {
            if let Some(s) = &c.inline {
                out.push((s.clone(), true));
            }
            if let Some(s) = &c.file {
                out.push((s.clone(), false));
            }
            if let Some(s) = &c.working_dir {
                out.push((s.clone(), false));
            }
            if let Some(s) = &c.stdin {
                out.push((s.clone(), true));
            }
            for v in c.env.values() {
                out.push((v.clone(), true));
            }
            out.extend(c.args.iter().map(|s| (s.clone(), true)));
        }
        StepConfig::PromptCall(c) => {
            out.push((c.user_prompt.clone(), false));
            if let Some(s) = &c.system_prompt {
                out.push((s.clone(), false));
            }
        }
        StepConfig::HumanInteraction(c) => {
            out.push((c.prompt.clone(), false));
        }
        StepConfig::AgentCall(c) => {
            out.push((c.objective.clone(), false));
            out.push((c.working_dir.clone(), false));
        }
        StepConfig::FanOut(c) => {
            out.push((c.over.clone(), false));
            if let Some(until) = &c.until {
                out.push((until.clone(), false));
            }
        }
        StepConfig::FanIn(_) => {}
        StepConfig::Condition(c) => {
            out.push((c.expression.clone(), false));
        }
    }
    out
}

fn collect_from_value(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(arr) => arr.iter().for_each(|v| collect_from_value(v, out)),
        serde_json::Value::Object(obj) => obj.values().for_each(|v| collect_from_value(v, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{
        AgentCallConfig, FanOutConfig, PlanMetadata, PlanStep, StepConfig, ToolCallConfig,
    };
    use indexmap::IndexMap;

    fn make_plan_with_config(
        config: IndexMap<String, serde_json::Value>,
        steps: Vec<PlanStep>,
    ) -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: "test".to_owned(),
            description: None,
            inputs: vec![],
            config,
            steps,
            outputs: vec![],
        }
    }

    fn step_with_arg(id: &str, arg_value: &str) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            config: StepConfig::ToolCall(ToolCallConfig {
                tool: "t".to_owned(),
                arguments: {
                    let mut m = IndexMap::new();
                    m.insert(
                        "x".to_owned(),
                        serde_json::Value::String(arg_value.to_owned()),
                    );
                    m
                },
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        }
    }

    #[test]
    fn valid_conf_ref_passes() {
        let mut config = IndexMap::new();
        config.insert(
            "api_url".to_owned(),
            serde_json::json!("https://example.com"),
        );
        let plan = make_plan_with_config(config, vec![step_with_arg("s1", "${conf.api_url}")]);
        assert!(validate_placeholders(&plan).is_empty());
    }

    #[test]
    fn declared_input_ref_passes_and_unknown_input_fails() {
        let mut plan =
            make_plan_with_config(IndexMap::new(), vec![step_with_arg("s1", "${input.query}")]);
        plan.inputs.push(crate::plan::types::PlanInput {
            name: "query".to_owned(),
            description: None,
            value_type: "string".to_owned(),
            required: true,
            default: None,
            input_kind: crate::plan::types::InputKind::Value,
        });
        assert!(validate_placeholders(&plan).is_empty());

        plan.inputs.clear();
        let errors = validate_placeholders(&plan);
        assert!(errors.iter().any(|error| {
            error.kind == ValidationErrorKind::UnknownPlaceholder
                && error.message.contains("plan.inputs")
        }));
    }

    #[test]
    fn agent_call_objective_and_working_directory_placeholders_are_validated() {
        let mut plan = make_plan_with_config(
            IndexMap::new(),
            vec![PlanStep {
                id: "agent".to_owned(),
                name: "agent".to_owned(),
                description: None,
                config: StepConfig::AgentCall(AgentCallConfig {
                    objective: "Implement ${input.request}".to_owned(),
                    working_dir: "${input.root_directory}".to_owned(),
                    timeout_secs: None,
                }),
                depends_on: vec![],
                outputs: vec![],
                timeout_secs: None,
                retry: None,
            }],
        );
        plan.inputs = vec![
            crate::plan::types::PlanInput {
                name: "request".to_owned(),
                description: None,
                value_type: "string".to_owned(),
                required: true,
                default: None,
                input_kind: crate::plan::types::InputKind::Value,
            },
            crate::plan::types::PlanInput {
                name: "root_directory".to_owned(),
                description: None,
                value_type: "string".to_owned(),
                required: true,
                default: None,
                input_kind: crate::plan::types::InputKind::DirectoryPath,
            },
        ];
        assert!(validate_placeholders(&plan).is_empty());

        plan.inputs.retain(|input| input.name != "request");
        let errors = validate_placeholders(&plan);
        assert!(errors.iter().any(|error| {
            error.step_id.as_deref() == Some("agent") && error.message.contains("input.request")
        }));
    }

    #[test]
    fn unknown_conf_ref_fails() {
        let plan = make_plan_with_config(
            IndexMap::new(),
            vec![step_with_arg("s1", "${conf.missing}")],
        );
        let errors = validate_placeholders(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::UnknownPlaceholder)
        );
    }

    #[test]
    fn env_ref_always_passes() {
        let plan = make_plan_with_config(IndexMap::new(), vec![step_with_arg("s1", "${env.HOME}")]);
        assert!(validate_placeholders(&plan).is_empty());
    }

    fn producer_with_outputs(id: &str, outputs: &[&str]) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            config: StepConfig::ToolCall(ToolCallConfig {
                tool: "t".to_owned(),
                arguments: Default::default(),
            }),
            depends_on: vec![],
            outputs: outputs
                .iter()
                .map(|name| crate::plan::types::PlanOutput {
                    name: (*name).to_owned(),
                    description: None,
                    value_type: "any".to_owned(),
                })
                .collect(),
            timeout_secs: None,
            retry: None,
        }
    }

    fn two_step_plan(producer: PlanStep, reference: &str) -> Plan {
        let producer_id = producer.id.clone();
        let mut consumer = step_with_arg("step-b", reference);
        consumer.depends_on = vec![producer_id];
        Plan {
            metadata: PlanMetadata::new(None),
            name: "t".to_owned(),
            description: None,
            inputs: vec![],
            config: IndexMap::new(),
            steps: vec![producer, consumer],
            outputs: vec![],
        }
    }

    #[test]
    fn step_ref_to_declared_output_passes() {
        let plan = two_step_plan(
            producer_with_outputs("step-a", &["result"]),
            "${step.step-a.result}",
        );
        assert!(validate_placeholders(&plan).is_empty());
    }

    #[test]
    fn step_ref_without_execution_dependency_is_rejected() {
        let producer = producer_with_outputs("step-a", &["result"]);
        let mut plan = two_step_plan(producer, "${step.step-a.result}");
        plan.steps[1].depends_on.clear();

        let errors = validate_placeholders(&plan);

        assert!(errors.iter().any(|error| {
            error.kind == ValidationErrorKind::MissingDependency
                && error.message.contains("before step 'step-a' is available")
        }));
    }

    #[test]
    fn transitive_step_ref_dependency_passes() {
        let producer = producer_with_outputs("step-a", &["result"]);
        let middle = PlanStep {
            depends_on: vec!["step-a".to_owned()],
            ..step_with_arg("middle", "constant")
        };
        let consumer = PlanStep {
            depends_on: vec!["middle".to_owned()],
            ..step_with_arg("consumer", "${step.step-a.result}")
        };
        let plan = make_plan_with_config(IndexMap::new(), vec![producer, middle, consumer]);

        assert!(validate_placeholders(&plan).is_empty());
    }

    #[test]
    fn step_ref_to_undeclared_output_is_rejected_with_available_names() {
        let plan = two_step_plan(
            producer_with_outputs("step-a", &["body"]),
            "${step.step-a.content}",
        );
        let errors = validate_placeholders(&plan);
        assert_eq!(errors.len(), 1);
        let message = errors[0].to_string();
        assert!(message.contains("${step.step-a.content}"), "got: {message}");
        assert!(message.contains("it produces: body"), "got: {message}");
    }

    #[test]
    fn step_ref_to_step_without_outputs_asks_for_a_declaration() {
        let plan = two_step_plan(
            producer_with_outputs("step-a", &[]),
            "${step.step-a.result}",
        );
        let errors = validate_placeholders(&plan);
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].to_string().contains("declares no outputs"),
            "got: {}",
            errors[0]
        );
    }

    #[test]
    fn implicit_type_outputs_are_recognised() {
        let producer = PlanStep {
            id: "ask".to_owned(),
            name: "ask".to_owned(),
            description: None,
            config: StepConfig::HumanInteraction(crate::plan::types::HumanInteractionConfig {
                prompt: "ok?".to_owned(),
                response_field: "answer".to_owned(),
                approval_required: false,
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let plan = two_step_plan(producer, "${step.ask.answer}");
        assert!(validate_placeholders(&plan).is_empty());
    }

    fn fan_out_plan(item_var: &str, body_placeholder: &str) -> Plan {
        let producer = producer_with_outputs("extract", &["urls"]);
        let fan_out = PlanStep {
            id: "fetch_posts".to_owned(),
            name: "fetch_posts".to_owned(),
            description: None,
            config: StepConfig::FanOut(FanOutConfig {
                over: "extract.urls".to_owned(),
                item_var: item_var.to_owned(),
                spawn_steps: vec!["fetch_post_body".to_owned()],
                until: None,
            }),
            depends_on: vec!["extract".to_owned()],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        let body = step_with_arg("fetch_post_body", body_placeholder);
        make_plan_with_config(IndexMap::new(), vec![producer, fan_out, body])
    }

    #[test]
    fn matching_fan_out_item_placeholder_passes() {
        let plan = fan_out_plan("item", "${item.item}");
        assert!(validate_placeholders(&plan).is_empty());
    }

    #[test]
    fn later_fan_out_body_step_is_not_available_to_earlier_body_step() {
        let mut plan = fan_out_plan("item", "${step.later.result}");
        plan.steps[2].depends_on = vec!["later".to_owned()];
        plan.steps.push(producer_with_outputs("later", &["result"]));
        let StepConfig::FanOut(config) = &mut plan.steps[1].config else {
            unreachable!("helper creates FAN_OUT")
        };
        config.spawn_steps = vec!["fetch_post_body".to_owned(), "later".to_owned()];

        let errors = validate_placeholders(&plan);

        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::MissingDependency)
        );
    }

    #[test]
    fn fan_out_until_may_reference_body_output() {
        let mut plan = fan_out_plan("item", "${item.item}");
        plan.steps[2].outputs = vec![crate::plan::types::PlanOutput {
            name: "done".to_owned(),
            description: None,
            value_type: "boolean".to_owned(),
        }];
        let StepConfig::FanOut(config) = &mut plan.steps[1].config else {
            unreachable!("helper creates FAN_OUT")
        };
        config.until = Some("${step.fetch_post_body.done} == true".to_owned());

        assert!(validate_placeholders(&plan).is_empty());
    }

    #[test]
    fn mismatched_fan_out_item_placeholder_is_rejected_with_expected_name() {
        let plan = fan_out_plan("item", "${item.url}");
        let errors = validate_placeholders(&plan);
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].message,
            "step 'fetch_post_body' references '${item.url}', but FAN_OUT 'fetch_posts' injects '${item.item}'"
        );
    }

    #[test]
    fn item_placeholder_outside_fan_out_body_is_rejected() {
        let plan = make_plan_with_config(
            IndexMap::new(),
            vec![step_with_arg("ordinary_step", "${item.entry}")],
        );
        let errors = validate_placeholders(&plan);
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0]
                .message
                .contains("only available in FAN_OUT-owned spawn steps")
        );
    }

    #[test]
    fn conflicting_fan_out_item_variables_are_rejected() {
        let mut plan = fan_out_plan("item", "${item.item}");
        plan.steps.insert(
            2,
            PlanStep {
                id: "other_fan".to_owned(),
                name: "other_fan".to_owned(),
                description: None,
                config: StepConfig::FanOut(FanOutConfig {
                    over: "extract.urls".to_owned(),
                    item_var: "url".to_owned(),
                    spawn_steps: vec!["fetch_post_body".to_owned()],
                    until: None,
                }),
                depends_on: vec!["extract".to_owned()],
                outputs: vec![],
                timeout_secs: None,
                retry: None,
            },
        );

        let errors = validate_placeholders(&plan);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ValidationErrorKind::InvalidStepConfig);
        assert!(errors[0].message.contains("FAN_OUT 'fetch_posts'"));
        assert!(errors[0].message.contains("FAN_OUT 'other_fan'"));
    }

    #[test]
    fn step_ref_to_missing_step_fails() {
        let plan = make_plan_with_config(
            IndexMap::new(),
            vec![step_with_arg("s1", "${step.ghost.output}")],
        );
        let errors = validate_placeholders(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::UnknownPlaceholder)
        );
    }

    #[test]
    fn unclosed_placeholder_is_reported_as_malformed() {
        let plan = make_plan_with_config(
            IndexMap::new(),
            vec![step_with_arg("s1", "prefix ${input.query")],
        );
        let errors = validate_placeholders(&plan);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ValidationErrorKind::MalformedPlaceholder);
        assert!(errors[0].message.contains("missing '}'"), "{}", errors[0]);
    }

    #[test]
    fn nested_placeholder_is_reported_as_malformed() {
        // `${a ${conf.x}}` — the outer `${` never closes into a valid
        // placeholder, only the inner one does.
        let mut config = IndexMap::new();
        config.insert("x".to_owned(), serde_json::json!("v"));
        let plan = make_plan_with_config(config, vec![step_with_arg("s1", "${a ${conf.x}}")]);
        let errors = validate_placeholders(&plan);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ValidationErrorKind::MalformedPlaceholder);
    }

    #[test]
    fn step_ref_without_output_part_is_rejected_as_unrecognised() {
        // `${step.a}` lacks the `<output-name>` part, so it does not match the
        // step namespace and falls through to the unrecognised-namespace error.
        let plan = two_step_plan(
            producer_with_outputs("step-a", &["result"]),
            "${step.step-a}",
        );
        let errors = validate_placeholders(&plan);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ValidationErrorKind::MalformedPlaceholder);
        assert!(
            errors[0]
                .message
                .contains("unrecognised placeholder namespace"),
            "{}",
            errors[0]
        );
    }

    fn code_call_step(id: &str, inline: &str, stdin: Option<&str>) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            config: StepConfig::CodeCall(crate::plan::types::CodeCallConfig {
                language: "bash".to_owned(),
                inline: Some(inline.to_owned()),
                file: None,
                args: vec![],
                stdin: stdin.map(str::to_owned),
                env: IndexMap::new(),
                working_dir: None,
                timeout_secs: None,
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        }
    }

    #[test]
    fn bash_parameter_expansion_in_code_call_source_passes() {
        let plan = make_plan_with_config(
            IndexMap::new(),
            vec![code_call_step(
                "extract",
                r#"datetime=${BODY#*\"datetime\":\"}; datetime=${datetime%%\"*}; echo "${datetime}""#,
                None,
            )],
        );
        assert!(validate_placeholders(&plan).is_empty());
    }

    #[test]
    fn reserved_namespace_in_code_call_stdin_is_still_validated() {
        let plan = make_plan_with_config(
            IndexMap::new(),
            vec![code_call_step("extract", "cat -", Some("${input.missing}"))],
        );
        let errors = validate_placeholders(&plan);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ValidationErrorKind::UnknownPlaceholder);
    }

    #[test]
    fn non_reserved_placeholder_in_code_call_stdin_passes() {
        let plan = make_plan_with_config(
            IndexMap::new(),
            vec![code_call_step(
                "extract",
                "cat -",
                Some("literal ${not_a_plan_ref}"),
            )],
        );
        assert!(validate_placeholders(&plan).is_empty());
    }

    #[test]
    fn unknown_namespace_fails() {
        let plan =
            make_plan_with_config(IndexMap::new(), vec![step_with_arg("s1", "${secret.key}")]);
        let errors = validate_placeholders(&plan);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::MalformedPlaceholder)
        );
    }

    fn plan_with_output(producer: PlanStep, output_name: &str, source: &str) -> Plan {
        let mut plan = make_plan_with_config(IndexMap::new(), vec![producer]);
        plan.outputs = vec![crate::plan::types::PlanOutputRef {
            name: output_name.to_owned(),
            description: None,
            source: source.to_owned(),
        }];
        plan
    }

    #[test]
    fn plan_output_referencing_declared_step_output_passes() {
        let plan = plan_with_output(
            producer_with_outputs("step-a", &["result"]),
            "final",
            "${step.step-a.result}",
        );
        assert!(validate_plan_output_sources(&plan).is_empty());
    }

    #[test]
    fn plan_output_referencing_missing_step_fails() {
        let plan = plan_with_output(
            producer_with_outputs("step-a", &["result"]),
            "final",
            "${step.ghost.result}",
        );
        let errors = validate_plan_output_sources(&plan);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ValidationErrorKind::UnknownPlaceholder);
        assert!(errors[0].message.contains("step 'ghost' does not exist"));
    }

    #[test]
    fn plan_output_referencing_undeclared_output_fails() {
        let plan = plan_with_output(
            producer_with_outputs("step-a", &["body"]),
            "final",
            "${step.step-a.missing}",
        );
        let errors = validate_plan_output_sources(&plan);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ValidationErrorKind::UnknownPlaceholder);
        assert!(errors[0].message.contains("it produces: body"));
    }

    #[test]
    fn plan_output_with_non_step_source_fails() {
        let plan = plan_with_output(
            producer_with_outputs("step-a", &["result"]),
            "final",
            "${conf.some_key}",
        );
        let errors = validate_plan_output_sources(&plan);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ValidationErrorKind::UnknownPlaceholder);
        assert!(errors[0].message.contains("must reference a step result"));
    }

    #[test]
    fn plan_output_with_malformed_source_fails() {
        let plan = plan_with_output(
            producer_with_outputs("step-a", &["result"]),
            "final",
            "prefix ${step.step-a.result}",
        );
        let errors = validate_plan_output_sources(&plan);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].kind, ValidationErrorKind::MalformedPlaceholder);
    }
}
