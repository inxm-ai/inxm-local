//! CONDITION step runner.
//!
//! Evaluates the configured expression against resolved placeholders and
//! stores the verdict in a single boolean output named `result`.  Routing —
//! marking the untaken branch (`true_steps` / `false_steps`) as skipped — is
//! done by the executor main loop, which owns run state; the runner only
//! decides true or false.
//!
//! # Expression grammar (v1)
//!
//! - `<lhs> == <rhs>` — loose equality
//! - `<lhs> != <rhs>` — loose inequality
//! - `<value>`        — bare value, evaluated for truthiness
//!
//! Each side may be a `${…}` placeholder or a literal.  Comparison is *loose*:
//! both sides are canonicalised to their string form first, so the boolean
//! `true` equals the string `"true"` and the number `42` equals `"42"`.  This
//! matches what LLM-compiled plans actually emit, where an output's JSON type
//! is not reliably known at compile time.

use crate::error::ExecutorError;
pub use crate::plan::steps::{Comparison, Expression, parse_expression};
use crate::plan::types::StepConfig;
use indexmap::IndexMap;

use super::{APPROVED_RESPONSE, StepContext, StepResult, resolve_placeholders};

/// Name of the single boolean output a CONDITION step produces. The executor
/// main loop reads it to decide which branch's steps to skip.
pub(crate) const RESULT_OUTPUT: &str = "result";

pub async fn run(ctx: &StepContext) -> Result<StepResult, ExecutorError> {
    let cfg = match &ctx.step.config {
        StepConfig::Condition(c) => c,
        _ => {
            return Err(ExecutorError::StepFailed {
                step_id: ctx.step.id.clone(),
                message: "expected CONDITION config".to_owned(),
            });
        }
    };

    let (result, trace) = evaluate_expression(&cfg.expression, &ctx.plan.config, &ctx.step_outputs)
        .map_err(|message| ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message,
        })?;

    let mut outputs = IndexMap::new();
    outputs.insert(RESULT_OUTPUT.to_owned(), serde_json::Value::Bool(result));

    Ok(StepResult {
        outputs,
        stdout: Some(trace),
        stderr: None,
        usage: None,
        child_runs: IndexMap::new(),
    })
}

/// Evaluate the shared CONDITION/FAN_OUT-until expression grammar against
/// normalized plan configuration and available step outputs.
pub fn evaluate_expression<L: super::StepOutputLookup + ?Sized>(
    expression: &str,
    plan_config: &IndexMap<String, serde_json::Value>,
    step_outputs: &L,
) -> Result<(bool, String), String> {
    let expr = parse_expression(expression)?;
    Ok(match &expr {
        Expression::Compare { lhs, op, rhs } => {
            let lhs_val = resolve_side(lhs, plan_config, step_outputs);
            let rhs_val = resolve_side(rhs, plan_config, step_outputs);
            let equal = loosely_equal(&lhs_val, &rhs_val);
            let result = match op {
                Comparison::Eq => equal,
                Comparison::Ne => !equal,
            };
            (
                result,
                format!(
                    "{} {} {} → {}",
                    canonical(&lhs_val),
                    op,
                    canonical(&rhs_val),
                    result
                ),
            )
        }
        Expression::Truthy(value) => {
            let resolved = resolve_side(value, plan_config, step_outputs);
            let result = is_truthy(&resolved);
            (result, format!("{} → {}", canonical(&resolved), result))
        }
    })
}

// ─── Evaluation helpers ───────────────────────────────────────────────────────

/// Resolve one side of an expression: placeholders resolve through the usual
/// whole-string rule (native JSON types come back intact); plain literals are
/// parsed as JSON where possible (`true`, `42`, `"quoted"`) and fall back to
/// the raw string.
fn resolve_side<L: super::StepOutputLookup + ?Sized>(
    side: &str,
    plan_config: &IndexMap<String, serde_json::Value>,
    step_outputs: &L,
) -> serde_json::Value {
    let resolved = resolve_placeholders(
        &serde_json::Value::String(side.to_owned()),
        plan_config,
        step_outputs,
    );
    match resolved {
        serde_json::Value::String(s) => parse_condition_literal(s),
        other => other,
    }
}

/// Parse a plain comparison operand (one that did not resolve to a native JSON
/// value). JSON is tried first, so `true` / `42` / `"quoted"` compare by value.
///
/// A single-quoted literal (`'high'`) is then treated as the string it wraps:
/// JSON only recognises double quotes, but compiled plans — especially those
/// written by an LLM or a Python-minded author — routinely quote a CONDITION
/// value with single quotes. Without this, `${step.x.level} == 'high'` compares
/// the bare output `high` against the literal `'high'` (quotes included) and can
/// never match, so the branch silently always takes the false path. Everything
/// else falls back to the original string.
fn parse_condition_literal(raw: String) -> serde_json::Value {
    let trimmed = raw.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return value;
    }
    if let Some(inner) = single_quoted_inner(trimmed) {
        return serde_json::Value::String(inner.to_owned());
    }
    serde_json::Value::String(raw)
}

/// The contents of a `'…'` single-quoted literal, when `s` is exactly one such
/// literal with no interior single quotes. `None` otherwise.
fn single_quoted_inner(s: &str) -> Option<&str> {
    const SINGLE_QUOTE: char = '\'';
    let inner = s.strip_prefix(SINGLE_QUOTE)?.strip_suffix(SINGLE_QUOTE)?;
    match inner.contains(SINGLE_QUOTE) {
        true => None,
        false => Some(inner),
    }
}

/// Compare values after string canonicalisation, while treating the executor's
/// approval response as equivalent to boolean `true`.
fn loosely_equal(lhs: &serde_json::Value, rhs: &serde_json::Value) -> bool {
    let lhs = canonical(lhs);
    let rhs = canonical(rhs);

    lhs == rhs
        || matches!(
            (
                lhs.to_ascii_lowercase().as_str(),
                rhs.to_ascii_lowercase().as_str()
            ),
            (APPROVED_RESPONSE, "true") | ("true", APPROVED_RESPONSE)
        )
}

/// Canonical string form used for loose comparison.
fn canonical(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.trim().to_owned(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn is_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        serde_json::Value::String(s) => matches!(
            s.trim().to_lowercase().as_str(),
            "true" | "yes" | "y" | APPROVED_RESPONSE | "1"
        ),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::types::{ConditionConfig, Plan, PlanMetadata, PlanStep, StepConfig};
    use crate::tools::catalog::ToolCatalog;
    use indexmap::IndexMap;
    use serde_json::json;

    fn make_ctx(
        expression: &str,
        step_outputs: IndexMap<String, IndexMap<String, serde_json::Value>>,
    ) -> StepContext {
        let step = PlanStep {
            id: "cond".to_owned(),
            name: "cond".to_owned(),
            description: None,
            config: StepConfig::Condition(ConditionConfig {
                expression: expression.to_owned(),
                true_steps: vec![],
                false_steps: vec![],
            }),
            depends_on: vec![],
            outputs: vec![],
            timeout_secs: None,
            retry: None,
        };
        StepContext {
            plan: std::sync::Arc::new(Plan {
                metadata: PlanMetadata::new(None),
                name: "test".to_owned(),
                description: None,
                inputs: vec![],
                config: IndexMap::new(),
                steps: vec![step.clone()],
                outputs: vec![],
            }),
            step,
            step_outputs: step_outputs.into(),
            catalog: ToolCatalog::default(),
            global_timeout_secs: None,
            human: None,
            run_id: "test-run".to_owned(),
            progress: None,
            child_progress: None,
            llm_keys: Default::default(),
            storage_root: std::env::temp_dir(),
            agent_audit: Default::default(),
        }
    }

    fn outputs_with(
        step: &str,
        field: &str,
        value: serde_json::Value,
    ) -> IndexMap<String, IndexMap<String, serde_json::Value>> {
        let mut inner = IndexMap::new();
        inner.insert(field.to_owned(), value);
        let mut outer = IndexMap::new();
        outer.insert(step.to_owned(), inner);
        outer
    }

    async fn eval(
        expression: &str,
        step_outputs: IndexMap<String, IndexMap<String, serde_json::Value>>,
    ) -> bool {
        let ctx = make_ctx(expression, step_outputs);
        let result = run(&ctx).await.expect("condition should evaluate");
        result.outputs["result"].as_bool().expect("bool result")
    }

    // ── parse_expression ──────────────────────────────────────────────────────

    #[test]
    fn parses_equality() {
        assert_eq!(
            parse_expression("${step.a.b} == ok"),
            Ok(Expression::Compare {
                lhs: "${step.a.b}".to_owned(),
                op: Comparison::Eq,
                rhs: "ok".to_owned(),
            })
        );
    }

    #[test]
    fn parses_inequality() {
        assert_eq!(
            parse_expression("${conf.mode} != strict"),
            Ok(Expression::Compare {
                lhs: "${conf.mode}".to_owned(),
                op: Comparison::Ne,
                rhs: "strict".to_owned(),
            })
        );
    }

    #[test]
    fn parses_bare_value_as_truthy() {
        assert_eq!(
            parse_expression("${step.ask.response}"),
            Ok(Expression::Truthy("${step.ask.response}".to_owned()))
        );
    }

    #[test]
    fn empty_expression_is_rejected() {
        assert!(parse_expression("   ").is_err());
    }

    #[test]
    fn one_sided_expression_is_rejected() {
        assert!(parse_expression("${step.a.b} ==").is_err());
        assert!(parse_expression("== ok").is_err());
    }

    // ── evaluation ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn string_equality_matches() {
        let outs = outputs_with("check", "status", json!("success"));
        assert!(eval("${step.check.status} == success", outs).await);
    }

    #[tokio::test]
    async fn string_equality_mismatch_is_false() {
        let outs = outputs_with("check", "status", json!("failure"));
        assert!(!eval("${step.check.status} == success", outs).await);
    }

    #[tokio::test]
    async fn inequality_inverts() {
        let outs = outputs_with("check", "status", json!("failure"));
        assert!(eval("${step.check.status} != success", outs).await);
    }

    #[tokio::test]
    async fn boolean_output_equals_true_literal() {
        // Loose comparison: JSON boolean true == literal "true".
        let outs = outputs_with("ask", "response", json!(true));
        assert!(eval("${step.ask.response} == true", outs).await);
    }

    #[tokio::test]
    async fn string_true_equals_true_literal() {
        let outs = outputs_with("ask", "response", json!("true"));
        assert!(eval("${step.ask.response} == true", outs).await);
    }

    #[tokio::test]
    async fn approved_response_is_truthy() {
        // HUMAN_INTERACTION approvals produce the literal "approved".
        let outs = outputs_with("ask", "response", json!("approved"));
        assert!(eval("${step.ask.response}", outs).await);
    }

    #[tokio::test]
    async fn approved_response_equals_true_literal() {
        let outs = outputs_with("ask", "response", json!("approved"));
        assert!(eval("${step.ask.response} == true", outs).await);
    }

    #[tokio::test]
    async fn number_equality_is_loose() {
        let outs = outputs_with("count", "value", json!(42));
        assert!(eval("${step.count.value} == 42", outs).await);
    }

    /// The boundaries of quote handling, pinned deliberately: the tokenizer
    /// (`scan_operators`) understands double quotes only, while an operand may
    /// be single-quoted. A bare apostrophe therefore stays harmless, but a
    /// single-quoted literal that itself contains an operator is split at that
    /// operator when it sits on the left-hand side. Use double quotes when a
    /// literal must contain `==` or `!=`.
    #[tokio::test]
    async fn quote_handling_boundaries_are_documented() {
        // An apostrophe inside a value does not open a quoted literal.
        let apostrophe = outputs_with("x", "v", json!("don't"));
        assert!(eval("${step.x.v} == don't", apostrophe).await);

        // To the right of the operator a single-quoted literal is fine: the
        // first `==` encountered is the real operator.
        let with_operator = outputs_with("x", "v", json!("a == b"));
        assert!(eval("${step.x.v} == 'a == b'", with_operator.clone()).await);

        // To its left, the tokenizer splits at the operator *inside* the
        // literal. Pre-existing limitation; the unmodified base behaves the
        // same way.
        assert!(
            !eval("'a == b' == ${step.x.v}", with_operator).await,
            "known boundary: use double quotes when a literal contains an operator"
        );
    }

    #[tokio::test]
    async fn single_quoted_literal_matches_bare_output() {
        // A compiler (or a Python-minded author) writing `== 'high'` must still
        // match a step output of `high`; single quotes are not JSON quotes.
        let outs = outputs_with("decide", "level", json!("high"));
        assert!(eval("${step.decide.level} == 'high'", outs).await);
    }

    #[tokio::test]
    async fn single_quoted_literal_still_distinguishes_values() {
        // The fix must not make everything equal: a real mismatch stays false.
        let outs = outputs_with("decide", "level", json!("low"));
        assert!(!eval("${step.decide.level} == 'high'", outs).await);
    }

    #[tokio::test]
    async fn single_quoted_literal_works_on_both_sides() {
        let outs = outputs_with("decide", "level", json!("warm"));
        assert!(eval("'warm' == ${step.decide.level}", outs).await);
    }

    #[tokio::test]
    async fn missing_reference_resolves_to_null_and_is_falsy() {
        assert!(!eval("${step.nope.value}", IndexMap::new()).await);
    }

    #[tokio::test]
    async fn result_output_is_declared() {
        let ctx = make_ctx("1 == 1", IndexMap::new());
        let result = run(&ctx).await.expect("evaluates");
        assert_eq!(result.outputs["result"], json!(true));
        assert!(result.stdout.is_some(), "trace should be recorded");
    }
}
