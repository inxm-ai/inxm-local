//! TOOL_CALL step runner.
//!
//! Resolves argument placeholders, looks up the tool in the catalog,
//! calls the tool adapter, and maps the result to a `StepResult`.

use crate::error::ExecutorError;
use crate::plan::types::StepConfig;
use crate::tools;
use indexmap::IndexMap;

use super::{StepContext, StepResult, non_empty, resolve_placeholders};

pub async fn run(ctx: &StepContext) -> Result<StepResult, ExecutorError> {
    let cfg = match &ctx.step.config {
        StepConfig::ToolCall(c) => c,
        _ => {
            return Err(ExecutorError::StepFailed {
                step_id: ctx.step.id.clone(),
                message: "expected TOOL_CALL config".to_owned(),
            });
        }
    };

    // Resolve placeholders in all argument values.
    let resolved_args: IndexMap<String, serde_json::Value> = cfg
        .arguments
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                resolve_placeholders(v, &ctx.plan.config, &ctx.step_outputs),
            )
        })
        .collect();

    // Look up the tool entry.
    let entry = ctx
        .catalog
        .get(&cfg.tool)
        .ok_or_else(|| ExecutorError::StepFailed {
            step_id: ctx.step.id.clone(),
            message: format!("tool '{}' not found in catalog", cfg.tool),
        })?;

    // Delegate to the tool adapter for normal execution.
    let started = std::time::Instant::now();
    tracing::info!(
        name: "inxm.executor.external.started",
        run_id = %ctx.run_id,
        plan_id = %ctx.plan.metadata.id,
        step_id = %ctx.step.id,
        runner_kind = "tool_call",
        runner_tool = %cfg.tool,
        "external runner started"
    );
    let output = match tools::execute_tool(entry, &resolved_args, ctx.global_timeout_secs).await {
        Ok(output) => output,
        Err(error) => {
            tracing::warn!(
                name: "inxm.executor.external.completed",
                run_id = %ctx.run_id,
                plan_id = %ctx.plan.metadata.id,
                step_id = %ctx.step.id,
                runner_kind = "tool_call",
                runner_tool = %cfg.tool,
                runner_duration_ms = started.elapsed().as_millis() as u64,
                runner_outcome = "failed",
                failure_class = tool_failure_classification(&error),
                "external runner completed"
            );
            return Err(error.into());
        }
    };
    tracing::info!(
        name: "inxm.executor.external.completed",
        run_id = %ctx.run_id,
        plan_id = %ctx.plan.metadata.id,
        step_id = %ctx.step.id,
        runner_kind = "tool_call",
        runner_tool = %cfg.tool,
        runner_duration_ms = started.elapsed().as_millis() as u64,
        runner_outcome = "succeeded",
        "external runner completed"
    );

    // Build named outputs: prefer parsed data, fall back to JSON-parsed stdout.
    let mut named_outputs: IndexMap<String, serde_json::Value> = IndexMap::new();

    if let serde_json::Value::Object(map) = &output.data {
        for (k, v) in map {
            named_outputs.insert(k.clone(), v.clone());
        }
    }

    if named_outputs.is_empty()
        && !output.stdout.is_empty()
        && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&output.stdout)
        && let serde_json::Value::Object(map) = parsed
    {
        for (k, v) in map {
            named_outputs.insert(k, v);
        }
    }

    // Contract: the step's single declared-but-unfilled output receives the
    // tool's primary payload. Without this, tools that return plain text
    // (fetched HTML, command output) produce NO named outputs and every
    // `${step.<id>.<output>}` reference to them is unresolvable.
    fill_declared_output(
        &mut named_outputs,
        &ctx.step.outputs,
        &output.data,
        &output.stdout,
    );

    Ok(StepResult {
        outputs: named_outputs,
        stdout: non_empty(output.stdout),
        stderr: non_empty(output.stderr),
        usage: None,
        child_runs: IndexMap::new(),
    })
}

fn tool_failure_classification(error: &crate::error::ToolError) -> &'static str {
    use crate::error::ToolError;
    match error {
        ToolError::NotFound { .. } => "not_found",
        ToolError::Catalog(_) => "catalog",
        ToolError::Execution { .. } => "execution",
        ToolError::Timeout { .. } => "timeout",
        ToolError::Yaml(_) => "yaml",
        ToolError::Io(_) => "io",
    }
}

/// If exactly one declared output is still unfilled (absent **or** null), assign
/// it the primary payload: parsed JSON data when present, otherwise raw stdout.
pub(super) fn fill_declared_output(
    outputs: &mut IndexMap<String, serde_json::Value>,
    declared: &[crate::plan::types::PlanOutput],
    data: &serde_json::Value,
    stdout: &str,
) {
    let missing: Vec<&str> = declared
        .iter()
        .map(|o| o.name.as_str())
        .filter(|name| outputs.get(*name).is_none_or(serde_json::Value::is_null))
        .collect();
    let [only] = missing.as_slice() else {
        return;
    };
    let primary = match data {
        serde_json::Value::Null => (!stdout.trim().is_empty())
            .then(|| serde_json::Value::String(stdout.trim_end().to_owned())),
        other => Some(other.clone()),
    };
    if let Some(value) = primary {
        outputs.insert((*only).to_owned(), value);
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod fill_declared_output_tests {
    use super::*;
    use crate::plan::types::PlanOutput;
    use serde_json::json;

    fn declared(names: &[&str]) -> Vec<PlanOutput> {
        names
            .iter()
            .map(|name| PlanOutput {
                name: (*name).to_owned(),
                description: None,
                value_type: "any".to_owned(),
            })
            .collect()
    }

    #[test]
    fn single_missing_output_receives_parsed_data() {
        let mut outputs = IndexMap::new();
        fill_declared_output(
            &mut outputs,
            &declared(&["body"]),
            &json!({"status": 200}),
            "ignored stdout",
        );
        assert_eq!(outputs["body"], json!({"status": 200}));
    }

    #[test]
    fn single_missing_output_falls_back_to_trimmed_stdout() {
        let mut outputs = IndexMap::new();
        fill_declared_output(
            &mut outputs,
            &declared(&["text"]),
            &serde_json::Value::Null,
            "plain text result\n",
        );
        assert_eq!(outputs["text"], json!("plain text result"));
    }

    #[test]
    fn nothing_filled_when_stdout_is_blank_and_data_is_null() {
        let mut outputs = IndexMap::new();
        fill_declared_output(
            &mut outputs,
            &declared(&["text"]),
            &serde_json::Value::Null,
            "   \n",
        );
        assert!(
            outputs.is_empty(),
            "blank payload must not fabricate an output"
        );
    }

    #[test]
    fn already_filled_output_is_not_overwritten() {
        let mut outputs: IndexMap<String, serde_json::Value> =
            [("body".to_owned(), json!("from tool"))]
                .into_iter()
                .collect();
        fill_declared_output(
            &mut outputs,
            &declared(&["body"]),
            &serde_json::Value::Null,
            "stdout payload",
        );
        assert_eq!(outputs["body"], json!("from tool"));
    }

    #[test]
    fn null_valued_output_is_backfilled_from_stdout() {
        let mut outputs: IndexMap<String, serde_json::Value> =
            [("url".to_owned(), serde_json::Value::Null)]
                .into_iter()
                .collect();
        fill_declared_output(
            &mut outputs,
            &declared(&["url"]),
            &serde_json::Value::Null,
            "https://example.com\n",
        );
        assert_eq!(outputs["url"], json!("https://example.com"));
    }

    #[test]
    fn null_valued_output_is_backfilled_from_data() {
        let mut outputs: IndexMap<String, serde_json::Value> =
            [("result".to_owned(), serde_json::Value::Null)]
                .into_iter()
                .collect();
        fill_declared_output(
            &mut outputs,
            &declared(&["result"]),
            &json!({"status": 200}),
            "",
        );
        assert_eq!(outputs["result"], json!({"status": 200}));
    }

    #[test]
    fn null_valued_output_not_filled_when_both_null_and_stdout_blank() {
        let mut outputs: IndexMap<String, serde_json::Value> =
            [("url".to_owned(), serde_json::Value::Null)]
                .into_iter()
                .collect();
        fill_declared_output(
            &mut outputs,
            &declared(&["url"]),
            &serde_json::Value::Null,
            "   \n",
        );
        assert_eq!(
            outputs["url"],
            serde_json::Value::Null,
            "null output must stay null when there is no payload to backfill with"
        );
    }

    #[test]
    fn ambiguous_multiple_missing_outputs_are_left_alone() {
        let mut outputs = IndexMap::new();
        fill_declared_output(
            &mut outputs,
            &declared(&["first", "second"]),
            &serde_json::Value::Null,
            "stdout payload",
        );
        assert!(
            outputs.is_empty(),
            "with two candidates the payload's destination is ambiguous"
        );
    }
}
