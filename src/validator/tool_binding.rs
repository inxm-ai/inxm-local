//! Tool binding validation.
//!
//! Checks TOOL_CALL steps against the tool catalog:
//! - Tool exists and is allowlisted.
//! - All required inputs are provided.
//! - Provided arguments match the declared type where possible.

use crate::error::{ValidationError, ValidationErrorKind};
use crate::plan::types::{InputKind, Plan, PlanInput, PlanStep, StepConfig, ToolCallConfig};
use crate::tools::catalog::{ToolCatalog, ToolEntry};
use std::collections::BTreeSet;

/// JSON Schema extension shared with tool catalog validation. It remains local
/// because the schema module intentionally exposes no public validation API.
const INPUT_KIND_ANNOTATION: &str = "x-inxm-input-kind";

/// Validate tool bindings for all TOOL_CALL steps in the plan.
pub fn validate_tool_bindings(plan: &Plan, catalog: &ToolCatalog) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let condition_routed_steps = condition_routed_step_ids(plan);

    for step in &plan.steps {
        let StepConfig::ToolCall(cfg) = &step.config else {
            continue;
        };
        let Some(tool) = catalog.get(&cfg.tool) else {
            errors.push(ValidationError::field(
                &step.id,
                "config.tool",
                ValidationErrorKind::UnknownTool,
                format!(
                    "step '{}' references tool '{}' which is not in the catalog",
                    step.id, cfg.tool
                ),
            ));
            // Stop checking this step — no schema to check against.
            continue;
        };

        check_allowlisted(step, cfg, tool, &mut errors);
        check_required_arguments(step, cfg, tool, &mut errors);
        check_argument_types(
            plan,
            step,
            cfg,
            tool,
            condition_routed_steps.contains(step.id.as_str()),
            &mut errors,
        );
    }

    errors
}

fn check_allowlisted(
    step: &PlanStep,
    cfg: &ToolCallConfig,
    tool: &ToolEntry,
    errors: &mut Vec<ValidationError>,
) {
    if !tool.allowlisted {
        errors.push(ValidationError::field(
            &step.id,
            "config.tool",
            ValidationErrorKind::UnknownTool,
            format!(
                "step '{}' uses tool '{}' which is not on the allowlist",
                step.id, cfg.tool
            ),
        ));
    }
}

fn check_required_arguments(
    step: &PlanStep,
    cfg: &ToolCallConfig,
    tool: &ToolEntry,
    errors: &mut Vec<ValidationError>,
) {
    for req in &tool.required_inputs() {
        if !cfg.arguments.contains_key(req.as_str()) {
            errors.push(ValidationError::field(
                &step.id,
                format!("config.arguments.{req}"),
                ValidationErrorKind::MissingRequiredArgument,
                format!(
                    "step '{}' calls tool '{}' but is missing required argument '{req}'",
                    step.id, cfg.tool
                ),
            ));
        }
    }
}

/// Where the tool schema declares a type for a property, verify the provided
/// value is at least the right JSON kind. Exact plan-input placeholders are
/// checked against both the input's declared JSON type and its path semantics.
/// Other placeholder strings are deferred to runtime resolution.
fn check_argument_types(
    plan: &Plan,
    step: &PlanStep,
    cfg: &ToolCallConfig,
    tool: &ToolEntry,
    is_condition_routed: bool,
    errors: &mut Vec<ValidationError>,
) {
    let Some(props) = tool
        .input_schema
        .get("properties")
        .and_then(|p| p.as_object())
    else {
        return;
    };

    for (arg_name, arg_value) in &cfg.arguments {
        let Some(property_schema) = props.get(arg_name) else {
            continue;
        };
        if let Some(placeholder) = crate::validator::placeholders::exact_placeholder_name(arg_value)
        {
            if let Some(input_name) = placeholder.strip_prefix("input.")
                && let Some(input) = plan.inputs.iter().find(|input| input.name == input_name)
            {
                check_input_binding(
                    step,
                    tool,
                    arg_name,
                    input,
                    property_schema,
                    is_condition_routed,
                    &mut *errors,
                );
            }
            continue;
        }
        if !json_value_matches_schema(arg_value, property_schema) {
            errors.push(ValidationError::field(
                &step.id,
                format!("config.arguments.{arg_name}"),
                ValidationErrorKind::TypeMismatch,
                format!(
                    "step '{}' argument '{arg_name}' is wrong type: expected {}",
                    step.id,
                    schema_type_description(property_schema)
                ),
            ));
        }
    }
}

fn check_input_binding(
    step: &PlanStep,
    tool: &ToolEntry,
    argument_name: &str,
    input: &PlanInput,
    property_schema: &serde_json::Value,
    is_condition_routed: bool,
    errors: &mut Vec<ValidationError>,
) {
    let field = format!("config.arguments.{argument_name}");

    if !declared_type_matches_schema(&input.value_type, property_schema) {
        errors.push(ValidationError::field(
            &step.id,
            field.clone(),
            ValidationErrorKind::TypeMismatch,
            format!(
                "step '{}' argument '{argument_name}' expects {}, but input '{}' is declared as {}",
                step.id,
                schema_type_description(property_schema),
                input.name,
                input.value_type,
            ),
        ));
    }

    if let Some(expected_kind) = property_schema
        .get(INPUT_KIND_ANNOTATION)
        .and_then(|value| value.as_str())
        && !input_kind_matches_annotation(input.effective_input_kind(), expected_kind)
    {
        errors.push(ValidationError::field(
            &step.id,
            field.clone(),
            ValidationErrorKind::TypeMismatch,
            format!(
                "step '{}' argument '{argument_name}' requires input kind '{expected_kind}', but input '{}' has kind '{}'",
                step.id,
                input.name,
                input_kind_name(input.effective_input_kind()),
            ),
        ));
    }

    if !is_condition_routed
        && required_non_nullable_property(tool, argument_name, property_schema)
        && !input.required
        && !has_concrete_compatible_default(input, property_schema)
    {
        errors.push(ValidationError::field(
            &step.id,
            field,
            ValidationErrorKind::MissingRequiredArgument,
            format!(
                "step '{}' argument '{argument_name}' is required by tool '{}', but input '{}' is optional without a concrete compatible default",
                step.id, tool.name, input.name
            ),
        ));
    }
}

fn input_kind_matches_annotation(input_kind: InputKind, annotation: &str) -> bool {
    matches!(
        (input_kind, annotation),
        (InputKind::Value, "value")
            | (InputKind::FilePath, "file_path")
            | (InputKind::OutputFilePath, "output_file_path")
            | (InputKind::DirectoryPath, "directory_path")
    )
}

fn input_kind_name(input_kind: InputKind) -> &'static str {
    match input_kind {
        InputKind::Value => "value",
        InputKind::FilePath => "file_path",
        InputKind::OutputFilePath => "output_file_path",
        InputKind::DirectoryPath => "directory_path",
    }
}

fn required_non_nullable_property(
    tool: &ToolEntry,
    argument_name: &str,
    property_schema: &serde_json::Value,
) -> bool {
    tool.required_inputs()
        .iter()
        .any(|required| required == argument_name)
        && !schema_accepts_null(property_schema)
}

fn has_concrete_compatible_default(input: &PlanInput, property_schema: &serde_json::Value) -> bool {
    let Some(default) = input.default.as_ref() else {
        return false;
    };
    !default.is_null()
        && crate::plan::types::input_value_matches_type(default, &input.value_type)
        && json_value_matches_schema(default, property_schema)
}

fn declared_type_matches_schema(declared: &str, property_schema: &serde_json::Value) -> bool {
    declared == "any"
        || schema_types(property_schema).is_none_or(|types| {
            types.iter().any(|expected| {
                declared == *expected || (declared == "integer" && *expected == "number")
            })
        })
}

/// Check whether a JSON value matches any primitive type permitted by a property schema.
fn json_value_matches_schema(
    value: &serde_json::Value,
    property_schema: &serde_json::Value,
) -> bool {
    if value.is_null() && schema_accepts_null(property_schema) {
        return true;
    }
    schema_types(property_schema).is_none_or(|types| {
        types
            .iter()
            .any(|expected| json_value_matches_type(value, expected))
    })
}

fn json_value_matches_type(value: &serde_json::Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => crate::support::is_json_integer(value),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true, // unknown type — pass
    }
}

/// The primitive types declared by a property schema. An absent or malformed
/// `type` leaves the property unconstrained for this lightweight validator.
fn schema_types(property_schema: &serde_json::Value) -> Option<Vec<&str>> {
    match property_schema.get("type")? {
        serde_json::Value::String(value) => Some(vec![value]),
        serde_json::Value::Array(values) => Some(
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect(),
        ),
        _ => None,
    }
}

/// OpenAPI's `nullable` extension and JSON Schema's `null` type both permit a
/// required tool property to receive an absent optional plan input.
fn schema_accepts_null(property_schema: &serde_json::Value) -> bool {
    property_schema
        .get("nullable")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || schema_types(property_schema).is_none_or(|types| types.contains(&"null"))
}

fn schema_type_description(property_schema: &serde_json::Value) -> String {
    schema_types(property_schema)
        .map(|types| types.join(" or "))
        .unwrap_or_else(|| "any type".to_owned())
}

/// Every step whose execution is conditional on any CONDITION branch.
///
/// Routing starts with every configured branch target and propagates to
/// downstream dependents. A routed FAN_OUT also routes its template body;
/// nested bodies and dependents are discovered by the same fixed-point walk.
/// This remains local to binding validation because it only affects the
/// preflight policy for missing tool input values.
fn condition_routed_step_ids(plan: &Plan) -> BTreeSet<&str> {
    let mut routed = BTreeSet::new();
    let mut pending = plan
        .steps
        .iter()
        .filter_map(|step| match &step.config {
            StepConfig::Condition(config) => Some(
                config
                    .true_steps
                    .iter()
                    .chain(config.false_steps.iter())
                    .map(String::as_str),
            ),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();

    while let Some(step_id) = pending.pop() {
        if !routed.insert(step_id) {
            continue;
        }

        if let Some(step) = plan.step(step_id)
            && let StepConfig::FanOut(config) = &step.config
        {
            pending.extend(config.spawn_steps.iter().map(String::as_str));
        }

        pending.extend(
            plan.steps
                .iter()
                .filter(|step| {
                    step.depends_on
                        .iter()
                        .any(|dependency| dependency == step_id)
                })
                .map(|step| step.id.as_str()),
        );
    }

    routed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{
        ConditionConfig, FanOutConfig, InputKind, PlanInput, PlanMetadata, PlanStep, ToolCallConfig,
    };
    use crate::tools::catalog::{SubprocessConfig, ToolConfig, ToolEntry};
    use indexmap::IndexMap;

    fn make_catalog(name: &str, required: Vec<&str>) -> ToolCatalog {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "msg": { "type": "string" },
                "count": { "type": "number" }
            },
            "required": required
        });
        ToolCatalog::new(vec![ToolEntry {
            name: name.to_owned(),
            description: String::new(),
            config: ToolConfig::Subprocess(SubprocessConfig {
                command: "true".to_owned(),
                args: vec![],
                env: Default::default(),
                working_dir: None,
            }),
            input_schema: schema,
            output_schema: serde_json::json!({"type": "object"}),
            allowlisted: true,
            timeout_secs: None,
        }])
    }

    fn make_step(id: &str, tool: &str, args: IndexMap<String, serde_json::Value>) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            config: StepConfig::ToolCall(ToolCallConfig {
                tool: tool.to_owned(),
                arguments: args,
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        }
    }

    fn make_plan(steps: Vec<PlanStep>) -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: "t".to_owned(),
            description: None,
            inputs: vec![],
            config: Default::default(),
            steps,
            outputs: vec![],
        }
    }

    fn input(
        name: &str,
        value_type: &str,
        required: bool,
        default: Option<serde_json::Value>,
        input_kind: InputKind,
    ) -> PlanInput {
        PlanInput {
            name: name.to_owned(),
            description: None,
            value_type: value_type.to_owned(),
            required,
            default,
            input_kind,
        }
    }

    fn condition_step(id: &str, true_steps: Vec<&str>, false_steps: Vec<&str>) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            config: StepConfig::Condition(ConditionConfig {
                expression: "true".to_owned(),
                true_steps: true_steps.into_iter().map(str::to_owned).collect(),
                false_steps: false_steps.into_iter().map(str::to_owned).collect(),
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        }
    }

    fn fan_out_step(id: &str, spawn_steps: Vec<&str>) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            name: id.to_owned(),
            description: None,
            config: StepConfig::FanOut(FanOutConfig {
                over: "producer.items".to_owned(),
                item_var: "item".to_owned(),
                spawn_steps: spawn_steps.into_iter().map(str::to_owned).collect(),
                until: None,
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        }
    }

    fn with_dependencies(mut step: PlanStep, dependencies: Vec<&str>) -> PlanStep {
        step.depends_on = dependencies.into_iter().map(str::to_owned).collect();
        step
    }

    #[test]
    fn missing_required_arg_fails() {
        let catalog = make_catalog("greet", vec!["msg"]);
        let plan = make_plan(vec![make_step("s1", "greet", IndexMap::new())]);
        let errors = validate_tool_bindings(&plan, &catalog);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::MissingRequiredArgument),
            "expected MissingRequiredArgument, got {errors:?}"
        );
    }

    #[test]
    fn all_required_args_present_passes() {
        let catalog = make_catalog("greet", vec!["msg"]);
        let mut args = IndexMap::new();
        args.insert("msg".to_owned(), serde_json::json!("hello"));
        let plan = make_plan(vec![make_step("s1", "greet", args)]);
        let errors = validate_tool_bindings(&plan, &catalog);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn unknown_tool_fails() {
        let catalog = ToolCatalog::default();
        let plan = make_plan(vec![make_step("s1", "nonexistent", IndexMap::new())]);
        let errors = validate_tool_bindings(&plan, &catalog);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::UnknownTool)
        );
    }

    #[test]
    fn non_allowlisted_tool_fails() {
        let catalog = make_catalog("greet", vec![]);
        let mut entry = catalog.get("greet").expect("tool was just added").clone();
        entry.allowlisted = false;
        let catalog = ToolCatalog::new(vec![entry]);

        let plan = make_plan(vec![make_step("s1", "greet", IndexMap::new())]);
        let errors = validate_tool_bindings(&plan, &catalog);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::UnknownTool
                    && e.message.contains("not on the allowlist")),
            "expected allowlist error, got {errors:?}"
        );
    }

    #[test]
    fn type_mismatch_fails() {
        let catalog = make_catalog("greet", vec![]);
        let mut args = IndexMap::new();
        args.insert("count".to_owned(), serde_json::json!("not-a-number"));
        let plan = make_plan(vec![make_step("s1", "greet", args)]);
        let errors = validate_tool_bindings(&plan, &catalog);
        assert!(
            errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::TypeMismatch),
            "{errors:?}"
        );
    }

    #[test]
    fn placeholder_strings_skip_type_check() {
        let catalog = make_catalog("greet", vec![]);
        let mut args = IndexMap::new();
        // This is a string placeholder for a number field — should pass since it's runtime-resolved
        args.insert("count".to_owned(), serde_json::json!("${conf.count}"));
        let plan = make_plan(vec![make_step("s1", "greet", args)]);
        let errors = validate_tool_bindings(&plan, &catalog);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn interpolated_string_does_not_defer_numeric_type_check() {
        let catalog = make_catalog("greet", vec![]);
        let mut args = IndexMap::new();
        args.insert(
            "count".to_owned(),
            serde_json::json!("prefix-${input.count}"),
        );
        let plan = make_plan(vec![make_step("s1", "greet", args)]);

        let errors = validate_tool_bindings(&plan, &catalog);

        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::TypeMismatch)
        );
    }

    #[test]
    fn fractional_number_does_not_match_integer_schema() {
        let mut catalog = make_catalog("greet", vec![]);
        let mut entry = catalog.get("greet").expect("tool exists").clone();
        entry.input_schema["properties"]["count"]["type"] = serde_json::json!("integer");
        catalog = ToolCatalog::new(vec![entry]);
        let mut args = IndexMap::new();
        args.insert("count".to_owned(), serde_json::json!(1.5));

        let errors =
            validate_tool_bindings(&make_plan(vec![make_step("s1", "greet", args)]), &catalog);

        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::TypeMismatch)
        );
    }

    /// Regression test for #108 (formerly #125): JSON Schema's `integer` cares
    /// about the mathematical value, so a whole-number float like `1.0` must
    /// be accepted — only a genuine fraction is a type mismatch.
    #[test]
    fn whole_number_float_matches_integer_schema() {
        let mut catalog = make_catalog("greet", vec![]);
        let mut entry = catalog.get("greet").expect("tool exists").clone();
        entry.input_schema["properties"]["count"]["type"] = serde_json::json!("integer");
        catalog = ToolCatalog::new(vec![entry]);
        let mut args = IndexMap::new();
        args.insert("count".to_owned(), serde_json::json!(1.0));

        let errors =
            validate_tool_bindings(&make_plan(vec![make_step("s1", "greet", args)]), &catalog);

        assert!(
            errors
                .iter()
                .all(|error| error.kind != ValidationErrorKind::TypeMismatch),
            "expected no type mismatch, got {errors:?}"
        );
    }

    #[test]
    fn exact_input_placeholder_uses_declared_input_type() {
        let catalog = make_catalog("greet", vec![]);
        let mut args = IndexMap::new();
        args.insert("count".to_owned(), serde_json::json!("${input.count}"));
        let mut plan = make_plan(vec![make_step("s1", "greet", args)]);
        plan.inputs
            .push(input("count", "string", true, None, InputKind::Value));

        let errors = validate_tool_bindings(&plan, &catalog);

        assert!(errors.iter().any(|error| {
            error.kind == ValidationErrorKind::TypeMismatch
                && error.message.contains("declared as string")
        }));
    }

    #[test]
    fn required_output_path_rejects_optional_or_null_input() {
        let mut catalog = make_catalog("write-file", vec!["path"]);
        let mut entry = catalog.get("write-file").expect("tool exists").clone();
        entry.input_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "x-inxm-input-kind": "output_file_path"
                }
            },
            "required": ["path"]
        });
        catalog = ToolCatalog::new(vec![entry]);
        let mut args = IndexMap::new();
        args.insert("path".to_owned(), serde_json::json!("${input.output_path}"));
        let mut plan = make_plan(vec![make_step("write", "write-file", args)]);
        plan.inputs.push(input(
            "output_path",
            "string",
            false,
            Some(serde_json::Value::Null),
            InputKind::OutputFilePath,
        ));

        let errors = validate_tool_bindings(&plan, &catalog);

        assert!(errors.iter().any(|error| {
            error.kind == ValidationErrorKind::MissingRequiredArgument
                && error
                    .message
                    .contains("optional without a concrete compatible default")
        }));
    }

    #[test]
    fn exact_input_placeholder_rejects_path_semantic_mismatch() {
        let mut catalog = make_catalog("write-file", vec![]);
        let mut entry = catalog.get("write-file").expect("tool exists").clone();
        entry.input_schema["properties"]["msg"] = serde_json::json!({
            "type": "string",
            "x-inxm-input-kind": "output_file_path"
        });
        catalog = ToolCatalog::new(vec![entry]);
        let mut args = IndexMap::new();
        args.insert("msg".to_owned(), serde_json::json!("${input.source_path}"));
        let mut plan = make_plan(vec![make_step("write", "write-file", args)]);
        plan.inputs.push(input(
            "source_path",
            "string",
            true,
            None,
            InputKind::FilePath,
        ));

        let errors = validate_tool_bindings(&plan, &catalog);

        assert!(errors.iter().any(|error| {
            error.kind == ValidationErrorKind::TypeMismatch
                && error
                    .message
                    .contains("requires input kind 'output_file_path'")
        }));
    }

    #[test]
    fn ordinary_string_input_is_compatible_without_semantic_annotation() {
        let catalog = make_catalog("greet", vec!["msg"]);
        let mut args = IndexMap::new();
        args.insert("msg".to_owned(), serde_json::json!("${input.output_path}"));
        let mut plan = make_plan(vec![make_step("greet", "greet", args)]);
        plan.inputs.push(input(
            "output_path",
            "string",
            true,
            None,
            InputKind::OutputFilePath,
        ));

        let errors = validate_tool_bindings(&plan, &catalog);

        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[test]
    fn concrete_compatible_default_satisfies_required_tool_argument() {
        let catalog = make_catalog("greet", vec!["msg"]);
        let mut args = IndexMap::new();
        args.insert("msg".to_owned(), serde_json::json!("${input.message}"));
        let mut plan = make_plan(vec![make_step("greet", "greet", args)]);
        plan.inputs.push(input(
            "message",
            "string",
            false,
            Some(serde_json::json!("default message")),
            InputKind::Value,
        ));

        assert!(validate_tool_bindings(&plan, &catalog).is_empty());
    }

    #[test]
    fn condition_routed_step_suppresses_only_optional_input_requirement_errors() {
        let catalog = make_catalog("greet", vec!["msg"]);
        let mut arguments = IndexMap::new();
        arguments.insert("msg".to_owned(), serde_json::json!("${input.message}"));
        let routed = with_dependencies(make_step("routed", "greet", arguments), vec!["condition"]);
        let mut plan = make_plan(vec![
            condition_step("condition", vec!["routed"], vec![]),
            routed,
        ]);
        plan.inputs
            .push(input("message", "number", false, None, InputKind::Value));

        let errors = validate_tool_bindings(&plan, &catalog);

        assert!(
            errors
                .iter()
                .any(|error| error.kind == ValidationErrorKind::TypeMismatch)
        );
        assert!(
            errors
                .iter()
                .all(|error| error.kind != ValidationErrorKind::MissingRequiredArgument),
            "only the optional-input requirement should be suppressed: {errors:?}"
        );
    }

    #[test]
    fn condition_routed_step_still_requires_declared_arguments() {
        let catalog = make_catalog("greet", vec!["msg"]);
        let routed = with_dependencies(
            make_step("routed", "greet", IndexMap::new()),
            vec!["condition"],
        );
        let plan = make_plan(vec![
            condition_step("condition", vec!["routed"], vec![]),
            routed,
        ]);

        let errors = validate_tool_bindings(&plan, &catalog);

        assert!(errors.iter().any(|error| {
            error.kind == ValidationErrorKind::MissingRequiredArgument
                && error.message.contains("missing required argument 'msg'")
        }));
    }

    #[test]
    fn conditionality_propagates_transitively_to_downstream_dependents() {
        let catalog = make_catalog("greet", vec!["msg"]);
        let mut branch_arguments = IndexMap::new();
        branch_arguments.insert("msg".to_owned(), serde_json::json!("branch"));
        let branch = with_dependencies(
            make_step("branch", "greet", branch_arguments),
            vec!["condition"],
        );
        let mut downstream_arguments = IndexMap::new();
        downstream_arguments.insert("msg".to_owned(), serde_json::json!("${input.message}"));
        let downstream = with_dependencies(
            make_step("downstream", "greet", downstream_arguments),
            vec!["branch"],
        );
        let mut terminal_arguments = IndexMap::new();
        terminal_arguments.insert("msg".to_owned(), serde_json::json!("${input.message}"));
        let terminal = with_dependencies(
            make_step("terminal", "greet", terminal_arguments),
            vec!["downstream"],
        );
        let mut plan = make_plan(vec![
            condition_step("condition", vec!["branch"], vec![]),
            branch,
            downstream,
            terminal,
        ]);
        plan.inputs
            .push(input("message", "string", false, None, InputKind::Value));

        assert!(validate_tool_bindings(&plan, &catalog).is_empty());
    }

    #[test]
    fn condition_routed_fan_out_body_is_conditional() {
        let catalog = make_catalog("greet", vec!["msg"]);
        let fan_out = with_dependencies(fan_out_step("fan", vec!["body"]), vec!["condition"]);
        let mut body_arguments = IndexMap::new();
        body_arguments.insert("msg".to_owned(), serde_json::json!("${input.message}"));
        let body = make_step("body", "greet", body_arguments);
        let mut plan = make_plan(vec![
            condition_step("condition", vec!["fan"], vec![]),
            fan_out,
            body,
        ]);
        plan.inputs
            .push(input("message", "string", false, None, InputKind::Value));

        assert!(validate_tool_bindings(&plan, &catalog).is_empty());
    }

    #[test]
    fn unconditional_steps_keep_optional_input_requirement_errors() {
        let catalog = make_catalog("greet", vec!["msg"]);
        let mut arguments = IndexMap::new();
        arguments.insert("msg".to_owned(), serde_json::json!("${input.message}"));
        let mut plan = make_plan(vec![make_step("always", "greet", arguments)]);
        plan.inputs
            .push(input("message", "string", false, None, InputKind::Value));

        assert!(
            validate_tool_bindings(&plan, &catalog)
                .iter()
                .any(|error| error.kind == ValidationErrorKind::MissingRequiredArgument)
        );
    }

    #[test]
    fn nullable_true_property_accepts_an_optional_input() {
        let mut catalog = make_catalog("greet", vec!["msg"]);
        let mut entry = catalog.get("greet").expect("tool exists").clone();
        entry.input_schema["properties"]["msg"]["nullable"] = serde_json::json!(true);
        catalog = ToolCatalog::new(vec![entry]);
        let mut arguments = IndexMap::new();
        arguments.insert("msg".to_owned(), serde_json::json!("${input.message}"));
        let mut plan = make_plan(vec![make_step("greet", "greet", arguments)]);
        plan.inputs
            .push(input("message", "string", false, None, InputKind::Value));

        assert!(validate_tool_bindings(&plan, &catalog).is_empty());
    }

    #[test]
    fn null_union_property_accepts_an_optional_defaultless_input() {
        let mut catalog = make_catalog("greet", vec!["msg"]);
        let mut entry = catalog.get("greet").expect("tool exists").clone();
        entry.input_schema["properties"]["msg"]["type"] = serde_json::json!(["string", "null"]);
        catalog = ToolCatalog::new(vec![entry]);
        let mut arguments = IndexMap::new();
        arguments.insert("msg".to_owned(), serde_json::json!("${input.message}"));
        let mut plan = make_plan(vec![make_step("greet", "greet", arguments)]);
        plan.inputs
            .push(input("message", "string", false, None, InputKind::Value));

        assert!(validate_tool_bindings(&plan, &catalog).is_empty());
    }

    #[test]
    fn condition_routed_step_keeps_input_kind_mismatch_diagnostic() {
        let mut catalog = make_catalog("greet", vec!["msg"]);
        let mut entry = catalog.get("greet").expect("tool exists").clone();
        entry.input_schema["properties"]["msg"]["x-inxm-input-kind"] =
            serde_json::json!("output_file_path");
        catalog = ToolCatalog::new(vec![entry]);
        let mut arguments = IndexMap::new();
        arguments.insert("msg".to_owned(), serde_json::json!("${input.message}"));
        let routed = with_dependencies(make_step("routed", "greet", arguments), vec!["condition"]);
        let mut plan = make_plan(vec![
            condition_step("condition", vec!["routed"], vec![]),
            routed,
        ]);
        plan.inputs
            .push(input("message", "string", false, None, InputKind::FilePath));

        let errors = validate_tool_bindings(&plan, &catalog);

        assert!(errors.iter().any(|error| {
            error.kind == ValidationErrorKind::TypeMismatch
                && error
                    .message
                    .contains("requires input kind 'output_file_path'")
        }));
        assert!(
            errors
                .iter()
                .all(|error| error.kind != ValidationErrorKind::MissingRequiredArgument),
            "the routed optional input should not produce a missing-argument error: {errors:?}"
        );
    }

    #[test]
    fn union_schema_accepts_compatible_default_and_rejects_incompatible_default() {
        let mut catalog = make_catalog("greet", vec!["msg"]);
        let mut entry = catalog.get("greet").expect("tool exists").clone();
        entry.input_schema["properties"]["msg"]["type"] = serde_json::json!(["string", "number"]);
        catalog = ToolCatalog::new(vec![entry]);
        let mut compatible_arguments = IndexMap::new();
        compatible_arguments.insert("msg".to_owned(), serde_json::json!("${input.compatible}"));
        let mut incompatible_arguments = IndexMap::new();
        incompatible_arguments.insert("msg".to_owned(), serde_json::json!("${input.incompatible}"));
        let mut plan = make_plan(vec![
            make_step("compatible", "greet", compatible_arguments),
            make_step("incompatible", "greet", incompatible_arguments),
        ]);
        plan.inputs.push(input(
            "compatible",
            "string",
            false,
            Some(serde_json::json!("fallback")),
            InputKind::Value,
        ));
        plan.inputs.push(input(
            "incompatible",
            "any",
            false,
            Some(serde_json::json!(false)),
            InputKind::Value,
        ));

        let errors = validate_tool_bindings(&plan, &catalog);

        assert!(errors.iter().any(|error| {
            error.step_id.as_deref() == Some("incompatible")
                && error.kind == ValidationErrorKind::MissingRequiredArgument
        }));
        assert!(
            errors
                .iter()
                .all(|error| error.step_id.as_deref() != Some("compatible"))
        );
    }
}
