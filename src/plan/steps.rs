//! Typed, phase-aware operations over persisted plan steps.
//!
//! `PlanStep` and `StepConfig` remain the serialization boundary. Runtime
//! callers use this module when behavior depends on a step kind, keeping raw
//! config branching out of executor orchestration.

use crate::plan::types::{PlanStep, StepConfig};
use std::collections::BTreeSet;

/// Comparison operators shared by CONDITION and FAN_OUT `until` expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    Eq,
    Ne,
}

impl std::fmt::Display for Comparison {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Comparison::Eq => write!(f, "=="),
            Comparison::Ne => write!(f, "!="),
        }
    }
}

/// Parsed shared expression form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expression {
    Compare {
        lhs: String,
        op: Comparison,
        rhs: String,
    },
    Truthy(String),
}

/// Parse the expression grammar used by CONDITION and FAN_OUT `until`.
pub fn parse_expression(expression: &str) -> Result<Expression, String> {
    let expression = expression.trim();
    if expression.is_empty() {
        return Err("CONDITION expression is empty".to_owned());
    }

    let scan = scan_operators(expression)?;

    // A logical operator is refused even when a supported comparison is also
    // present: `${a} == x && ${b} == y` would otherwise split at the first
    // `==` and compare `${a}` against the literal `x && ${b} == y`, which
    // resolves to a non-truthy string and takes the false branch on a run that
    // reports success. That compound form is the one an LLM reaches for first.
    if let Some(token) = scan.logical {
        return Err(unsupported_operator_error(expression, token));
    }

    match scan.supported {
        Some((index, token_len, comparison)) => {
            let lhs = expression[..index].trim();
            let rhs = expression[index + token_len..].trim();
            if lhs.is_empty() || rhs.is_empty() {
                return Err(format!(
                    "CONDITION expression '{expression}' must have a value on both sides of '{comparison}'"
                ));
            }
            Ok(Expression::Compare {
                lhs: lhs.to_owned(),
                op: comparison,
                rhs: rhs.to_owned(),
            })
        }
        // An expression with no supported operator is a truthiness check — unless
        // it carries an operator this grammar does not implement. Falling through
        // to `Truthy` there would resolve the whole text (`10 > 5`) to a string,
        // find it non-truthy, and silently take the false branch on a run that
        // reports success. Refuse it instead, at compile time via the validator
        // and at run time via the runner, both of which parse through here.
        None => match scan.comparison {
            Some(token) => Err(unsupported_operator_error(expression, token)),
            None => Ok(Expression::Truthy(expression.to_owned())),
        },
    }
}

fn unsupported_operator_error(expression: &str, token: &str) -> String {
    format!(
        "CONDITION expression '{expression}' uses the unsupported operator '{token}' — \
         this grammar has only '==', '!=' and a bare truthiness check. Compute the \
         comparison in a preceding CODE_CALL step and compare its output for equality."
    )
}

/// Multi-character logical operators the grammar does not implement. They
/// cannot be part of a legitimate scalar operand, so they are refused wherever
/// they appear.
const UNSUPPORTED_LOGICAL_OPERATORS: &[&str] = &["&&", "||"];

/// Return whether `!` at `index` is a prefix negation operator. A bang inside
/// an unquoted scalar (for example `ready!`) is data and must remain legal.
fn is_negation_operator(expression: &str, index: usize) -> bool {
    let before = expression[..index].trim_end();
    let after = expression[index + '!'.len_utf8()..].trim_start();
    let starts_operand = before.is_empty()
        || before.ends_with("==")
        || before.ends_with("!=")
        || before.ends_with("&&")
        || before.ends_with("||");

    starts_operand && !after.is_empty()
}

/// Comparison operators the grammar does not implement. Longest-first so `>=`
/// is reported rather than a bare `>`.
///
/// These are refused only when no supported operator was found. With `==`
/// present, a `<` or `>` sits inside an operand where it is plausibly part of a
/// value — `${step.a.v} != <none>` compares against the literal `<none>` today
/// and must keep working.
const UNSUPPORTED_COMPARISON_OPERATORS: &[&str] = &[">=", "<=", "<>", ">", "<"];

/// What an operator scan found outside quoted literals.
struct OperatorScan {
    /// The first supported operator: byte offset, token length, kind.
    supported: Option<(usize, usize, Comparison)>,
    /// The first logical operator, refused unconditionally.
    logical: Option<&'static str>,
    /// The first comparison operator, refused only in the absence of a
    /// supported one.
    comparison: Option<&'static str>,
}

/// Scan an expression for comparison operators outside quoted literals,
/// recording both the first supported one and the first unsupported one.
fn scan_operators(expression: &str) -> Result<OperatorScan, String> {
    let mut quoted = false;
    let mut escaped = false;
    let mut supported = None;
    let mut logical = None;
    let mut comparison = None;

    for (index, character) in expression.char_indices() {
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
            continue;
        }

        let rest = &expression[index..];
        if character == '"' {
            quoted = true;
            continue;
        }
        if rest.starts_with("==") {
            supported.get_or_insert((index, "==".len(), Comparison::Eq));
            continue;
        }
        if rest.starts_with("!=") {
            supported.get_or_insert((index, "!=".len(), Comparison::Ne));
            continue;
        }
        if logical.is_none() {
            logical = UNSUPPORTED_LOGICAL_OPERATORS
                .iter()
                .find(|token| rest.starts_with(**token))
                .copied()
                // Prefix `!` is logical negation, which this grammar cannot
                // express. A bang elsewhere in an unquoted scalar is data.
                .or_else(|| {
                    (character == '!' && is_negation_operator(expression, index)).then_some("!")
                });
        }
        if comparison.is_none() {
            comparison = UNSUPPORTED_COMPARISON_OPERATORS
                .iter()
                .find(|token| rest.starts_with(**token))
                .copied()
                // A lone `=` is an assignment-style comparison (`${a} = 5`);
                // the two-character forms are matched above and skipped here.
                .or_else(|| (character == '=').then_some("="));
        }
    }

    if quoted {
        return Err(format!(
            "CONDITION expression '{expression}' contains an unterminated quoted literal"
        ));
    }

    Ok(OperatorScan {
        supported,
        logical,
        comparison,
    })
}

/// Serialized field name of `FanOutConfig::until` — must match the serde
/// representation in [`crate::plan::types::FanOutConfig`].
const FAN_OUT_UNTIL_FIELD: &str = "until";

/// Every step owned by `step_id` as a FAN_OUT body, transitively.
///
/// FAN_OUT ownership is a typed plan contract rather than a dependency edge:
/// body steps execute inside their owner and may themselves own nested bodies.
/// Returns an empty set when `step_id` is absent or is not a FAN_OUT.
pub fn fan_out_body_closure(plan: &crate::plan::types::Plan, step_id: &str) -> BTreeSet<String> {
    let mut owned = BTreeSet::new();
    let mut pending = vec![step_id.to_owned()];

    while let Some(current) = pending.pop() {
        let Some(step) = plan.step(&current) else {
            continue;
        };
        let StepConfig::FanOut(config) = &step.config else {
            continue;
        };
        for spawn_id in &config.spawn_steps {
            if owned.insert(spawn_id.clone()) {
                pending.push(spawn_id.clone());
            }
        }
    }

    owned
}

/// Return the part of a step configuration whose placeholders must exist
/// before the step starts.
///
/// A FAN_OUT `until` expression is intentionally excluded: it references
/// outputs produced by the current body iteration and is evaluated only after
/// that iteration completes. All other configuration remains preflighted.
pub fn runtime_preflight_config(step: &PlanStep) -> serde_json::Value {
    // Serialising a StepConfig is infallible in practice (plain data, string
    // keys); the fallback keeps this function total rather than pushing an
    // impossible error onto every caller.
    let mut config = serde_json::to_value(&step.config).unwrap_or(serde_json::Value::Null);
    if matches!(step.config, StepConfig::FanOut(_))
        && let Some(object) = config.as_object_mut()
    {
        object.remove(FAN_OUT_UNTIL_FIELD);
    }
    config
}

/// Return every declared plan input referenced by a step configuration.
///
/// This is the plan IR's single source of truth for the question "which run
/// inputs influenced this step?". Repair-resume uses it to protect values
/// that already produced persisted successful outputs. The scan intentionally
/// covers every string-bearing config field, including nested TOOL_CALL JSON
/// arguments and expression fields on CONDITION and FAN_OUT steps.
///
/// Only well-formed `${input.<name>}` placeholders are returned. Malformed or
/// unknown placeholders remain the validator's responsibility.
pub fn input_references(step: &PlanStep) -> BTreeSet<String> {
    let mut references = BTreeSet::new();
    match &step.config {
        StepConfig::ToolCall(config) => {
            for value in config.arguments.values() {
                collect_input_references_from_value(value, &mut references);
            }
        }
        StepConfig::CodeCall(config) => {
            for value in config
                .inline
                .iter()
                .chain(config.file.iter())
                .chain(config.args.iter())
                .chain(config.stdin.iter())
                .chain(config.env.values())
                .chain(config.working_dir.iter())
            {
                collect_input_references_from_string(value, &mut references);
            }
        }
        StepConfig::HumanInteraction(config) => {
            collect_input_references_from_string(&config.prompt, &mut references);
        }
        StepConfig::FanOut(config) => {
            collect_input_references_from_string(&config.over, &mut references);
            if let Some(until) = &config.until {
                collect_input_references_from_string(until, &mut references);
            }
        }
        StepConfig::FanIn(_) => {}
        StepConfig::PromptCall(config) => {
            collect_input_references_from_string(&config.user_prompt, &mut references);
            if let Some(system_prompt) = &config.system_prompt {
                collect_input_references_from_string(system_prompt, &mut references);
            }
        }
        StepConfig::Condition(config) => {
            collect_input_references_from_string(&config.expression, &mut references);
        }
        StepConfig::AgentCall(config) => {
            collect_input_references_from_string(&config.objective, &mut references);
            collect_input_references_from_string(&config.working_dir, &mut references);
        }
    }
    references
}

fn collect_input_references_from_value(value: &serde_json::Value, out: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::String(value) => collect_input_references_from_string(value, out),
        serde_json::Value::Array(values) => values
            .iter()
            .for_each(|value| collect_input_references_from_value(value, out)),
        serde_json::Value::Object(values) => values
            .values()
            .for_each(|value| collect_input_references_from_value(value, out)),
        _ => {}
    }
}

fn collect_input_references_from_string(value: &str, out: &mut BTreeSet<String>) {
    let mut remainder = value;
    while let Some(open_index) = remainder.find("${") {
        let after_open = &remainder[open_index + "${".len()..];
        let Some(close_index) = after_open.find('}') else {
            return;
        };
        let placeholder = &after_open[..close_index];
        if let Some(input_name) = placeholder.strip_prefix("input.")
            && is_input_name(input_name)
        {
            out.insert(input_name.to_owned());
        }
        remainder = &after_open[close_index + '}'.len_utf8()..];
    }
}

fn is_input_name(name: &str) -> bool {
    let mut characters = name.chars();
    matches!(characters.next(), Some(character) if character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{
        AgentCallConfig, CodeCallConfig, ConditionConfig, FanInConfig, FanOutConfig,
        HumanInteractionConfig, Plan, PlanMetadata, PlanOutput, PlanStep, PromptCallConfig,
        ToolCallConfig,
    };
    use indexmap::IndexMap;

    fn step_with_config(config: StepConfig) -> PlanStep {
        PlanStep {
            id: "step".to_owned(),
            name: "Step".to_owned(),
            description: None,
            config,
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        }
    }

    #[test]
    fn fan_out_until_is_deferred_but_source_is_preflighted() {
        let step = PlanStep {
            id: "retry".to_owned(),
            name: "Retry".to_owned(),
            description: None,
            config: StepConfig::FanOut(FanOutConfig {
                over: "attempts.values".to_owned(),
                item_var: "attempt".to_owned(),
                spawn_steps: vec!["verify".to_owned()],
                until: Some("${step.verify.matches} == true".to_owned()),
            }),
            depends_on: vec!["attempts".to_owned()],
            outputs: vec![PlanOutput {
                name: "results".to_owned(),
                description: None,
                value_type: "array".to_owned(),
            }],
            timeout_secs: None,
            retry: None,
        };

        let config = runtime_preflight_config(&step);

        assert_eq!(config["over"], "attempts.values");
        assert!(config.get("until").is_none());
    }

    #[test]
    fn fan_out_body_closure_collects_nested_bodies() {
        let mut outer = step_with_config(StepConfig::FanOut(FanOutConfig {
            over: "source.items".to_owned(),
            item_var: "item".to_owned(),
            spawn_steps: vec!["inner".to_owned(), "tail".to_owned()],
            until: None,
        }));
        outer.id = "outer".to_owned();
        let mut inner = step_with_config(StepConfig::FanOut(FanOutConfig {
            over: "nested.items".to_owned(),
            item_var: "nested".to_owned(),
            spawn_steps: vec!["leaf".to_owned()],
            until: None,
        }));
        inner.id = "inner".to_owned();
        let mut tail = step_with_config(StepConfig::ToolCall(ToolCallConfig {
            tool: "noop".to_owned(),
            arguments: IndexMap::new(),
        }));
        tail.id = "tail".to_owned();
        let mut leaf = tail.clone();
        leaf.id = "leaf".to_owned();
        let plan = Plan {
            metadata: PlanMetadata::new(None),
            name: "nested fan-out".to_owned(),
            description: None,
            inputs: vec![],
            config: IndexMap::new(),
            steps: vec![outer, inner, tail, leaf],
            outputs: vec![],
        };

        assert_eq!(
            fan_out_body_closure(&plan, "outer"),
            ["inner", "leaf", "tail"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
        assert!(fan_out_body_closure(&plan, "tail").is_empty());
        assert!(fan_out_body_closure(&plan, "missing").is_empty());
    }

    /// An operator this grammar does not implement must be refused, not folded
    /// into a truthiness check that silently reports false while the run
    /// succeeds and the trace reads like a real comparison.
    #[test]
    fn unsupported_operators_are_refused_instead_of_silently_false() {
        for (expression, token) in [
            ("${step.n.value} > 5", ">"),
            ("${step.n.value} < 5", "<"),
            ("${step.n.value} >= 5", ">="),
            ("${step.n.value} <= 5", "<="),
            ("${step.n.value} <> 5", "<>"),
            ("${step.a.ok} && ${step.b.ok}", "&&"),
            ("${step.a.ok} || ${step.b.ok}", "||"),
        ] {
            let error = parse_expression(expression).expect_err(&format!(
                "'{expression}' must not parse as a truthiness check"
            ));
            assert!(
                error.contains(token),
                "error for '{expression}' should name '{token}': {error}"
            );
            assert!(
                error.contains("=="),
                "error for '{expression}' should point at the supported grammar: {error}"
            );
        }
    }

    /// The compound forms an LLM writes first: a logical operator alongside a
    /// supported comparison must still be refused, or the expression splits at
    /// the first `==` and compares against the rest as a literal.
    #[test]
    fn logical_operators_are_refused_even_beside_a_supported_comparison() {
        for expression in [
            r#"${step.a.v} == "x" && ${step.b.v} == "y""#,
            "${step.a.v} == x || ${step.b.v} == y",
            "!${step.a.done}",
        ] {
            let error =
                parse_expression(expression).expect_err(&format!("'{expression}' must be refused"));
            assert!(
                error.contains("unsupported operator"),
                "error for '{expression}' should name the problem: {error}"
            );
        }
    }

    /// A comparison operator inside an operand is left alone: with `==`
    /// present it is plausibly part of a value, and plans relying on that
    /// must keep working.
    #[test]
    fn comparison_characters_inside_an_operand_are_not_refused() {
        assert_eq!(
            parse_expression("${step.a.v} != <none>").unwrap(),
            Expression::Compare {
                lhs: "${step.a.v}".to_owned(),
                op: Comparison::Ne,
                rhs: "<none>".to_owned(),
            }
        );
    }

    /// A bang at the end of an unquoted scalar is punctuation, not logical
    /// negation, and remains compatible with the existing scalar grammar.
    #[test]
    fn exclamation_mark_inside_an_operand_is_not_refused() {
        assert_eq!(
            parse_expression("${step.check.status} == ready!").unwrap(),
            Expression::Compare {
                lhs: "${step.check.status}".to_owned(),
                op: Comparison::Eq,
                rhs: "ready!".to_owned(),
            }
        );
    }

    /// An assignment-style comparison is a comparison the grammar lacks, not
    /// a truthiness check.
    #[test]
    fn a_lone_equals_is_refused() {
        let error = parse_expression("${step.a.v} = 5").expect_err("must be refused");
        assert!(error.contains("'='"), "should name the operator: {error}");
    }

    /// The refusal must not swallow the forms the grammar does support.
    #[test]
    fn supported_expressions_still_parse_after_the_operator_check() {
        assert_eq!(
            parse_expression("${step.a.v} == high").unwrap(),
            Expression::Compare {
                lhs: "${step.a.v}".to_owned(),
                op: Comparison::Eq,
                rhs: "high".to_owned(),
            }
        );
        assert_eq!(
            parse_expression("${step.a.v} != high").unwrap(),
            Expression::Compare {
                lhs: "${step.a.v}".to_owned(),
                op: Comparison::Ne,
                rhs: "high".to_owned(),
            }
        );
        // A bare truthiness check carries no operator at all.
        assert_eq!(
            parse_expression("${step.a.done}").unwrap(),
            Expression::Truthy("${step.a.done}".to_owned())
        );
        // An unsupported operator inside a quoted literal is text, not syntax.
        assert_eq!(
            parse_expression(r#"${step.a.v} == "5 > 3""#).unwrap(),
            Expression::Compare {
                lhs: "${step.a.v}".to_owned(),
                op: Comparison::Eq,
                rhs: r#""5 > 3""#.to_owned(),
            }
        );
    }

    #[test]
    fn comparison_operators_inside_quoted_literals_are_not_parsed_as_operators() {
        assert_eq!(
            parse_expression(r#""left == right""#).unwrap(),
            Expression::Truthy(r#""left == right""#.to_owned())
        );
        assert_eq!(
            parse_expression(r#""left != right""#).unwrap(),
            Expression::Truthy(r#""left != right""#.to_owned())
        );
    }

    #[test]
    fn escaped_quotes_do_not_expose_literal_operators() {
        assert_eq!(
            parse_expression(r#""left \"quoted == value\"" != expected"#).unwrap(),
            Expression::Compare {
                lhs: r#""left \"quoted == value\"""#.to_owned(),
                op: Comparison::Ne,
                rhs: "expected".to_owned(),
            }
        );
    }

    #[test]
    fn malformed_quoted_literals_are_rejected_deterministically() {
        for expression in [r#""unterminated == value"#, r#"value == "unterminated"#] {
            let first = parse_expression(expression).unwrap_err();
            let second = parse_expression(expression).unwrap_err();

            assert_eq!(first, second);
            assert!(first.contains("unterminated quoted literal"));
        }
    }

    #[test]
    fn input_references_cover_every_string_bearing_step_config_field() {
        let mut nested_tool_arguments = IndexMap::new();
        nested_tool_arguments.insert(
            "nested".to_owned(),
            serde_json::json!({
                "array": ["${input.tool_arg}", {"again": "prefix-${input.tool_arg}"}]
            }),
        );

        let cases = [
            (
                StepConfig::ToolCall(ToolCallConfig {
                    tool: "tool".to_owned(),
                    arguments: nested_tool_arguments,
                }),
                ["tool_arg"].as_slice(),
            ),
            (
                StepConfig::CodeCall(CodeCallConfig {
                    language: "bash".to_owned(),
                    inline: Some("${input.inline}".to_owned()),
                    file: Some("${input.file}".to_owned()),
                    args: vec!["${input.arg}".to_owned()],
                    stdin: Some("${input.stdin}".to_owned()),
                    env: [("KEY".to_owned(), "${input.env}".to_owned())]
                        .into_iter()
                        .collect(),
                    working_dir: Some("${input.working_dir}".to_owned()),
                    timeout_secs: None,
                }),
                ["arg", "env", "file", "inline", "stdin", "working_dir"].as_slice(),
            ),
            (
                StepConfig::HumanInteraction(HumanInteractionConfig {
                    prompt: "${input.prompt}".to_owned(),
                    response_field: "response".to_owned(),
                    approval_required: false,
                }),
                ["prompt"].as_slice(),
            ),
            (
                StepConfig::FanOut(FanOutConfig {
                    over: "${input.over}".to_owned(),
                    item_var: "item".to_owned(),
                    spawn_steps: vec!["body".to_owned()],
                    until: Some("${input.until} == true".to_owned()),
                }),
                ["over", "until"].as_slice(),
            ),
            (
                StepConfig::FanIn(FanInConfig {
                    from_steps: vec!["${input.not_a_reference}".to_owned()],
                    collect_field: "${input.not_a_reference}".to_owned(),
                }),
                [].as_slice(),
            ),
            (
                StepConfig::PromptCall(PromptCallConfig {
                    model: "model".to_owned(),
                    system_prompt: Some("${input.system}".to_owned()),
                    user_prompt: "${input.user}".to_owned(),
                    output_field: "answer".to_owned(),
                    max_tokens: None,
                    temperature: None,
                }),
                ["system", "user"].as_slice(),
            ),
            (
                StepConfig::Condition(ConditionConfig {
                    expression: "${input.condition} == true".to_owned(),
                    true_steps: vec![],
                    false_steps: vec![],
                }),
                ["condition"].as_slice(),
            ),
            (
                StepConfig::AgentCall(AgentCallConfig {
                    objective: "${input.objective}".to_owned(),
                    working_dir: "${input.agent_directory}".to_owned(),
                    timeout_secs: None,
                }),
                ["agent_directory", "objective"].as_slice(),
            ),
        ];

        for (config, expected) in cases {
            let expected: BTreeSet<String> =
                expected.iter().map(|name| (*name).to_owned()).collect();
            assert_eq!(input_references(&step_with_config(config)), expected);
        }
    }

    #[test]
    fn input_references_ignore_malformed_or_non_input_placeholders() {
        let step = step_with_config(StepConfig::Condition(ConditionConfig {
            expression: "${input.valid} ${input.invalid.name} ${input.1bad} ${step.other.output} ${input.unclosed".to_owned(),
            true_steps: vec![],
            false_steps: vec![],
        }));

        assert_eq!(
            input_references(&step),
            ["valid".to_owned()].into_iter().collect()
        );
    }
}
