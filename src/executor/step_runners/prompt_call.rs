//! `PROMPT_CALL` runner using the same LLM transport as the compiler.

use crate::error::ExecutorError;
use crate::llm::{CompletionRequest, LlmAuth, LlmProfile, LlmProtocol};
use crate::plan::types::StepConfig;
use crate::storage::runs::TokenUsage;
use indexmap::IndexMap;

use super::{StepContext, StepResult, resolve_to_string};

/// Request timeout for the legacy (profile-less) provider fallback.
const LEGACY_PROFILE_TIMEOUT_SECS: u64 = 300;

/// Models whose name starts with this prefix route to the Anthropic API in
/// legacy model-prefix routing; everything else routes to the OpenAI API.
const ANTHROPIC_MODEL_PREFIX: &str = "claude";

pub async fn run(ctx: &StepContext) -> Result<StepResult, ExecutorError> {
    let cfg = match &ctx.step.config {
        StepConfig::PromptCall(config) => config,
        _ => return Err(failed(ctx, "expected PROMPT_CALL config")),
    };

    if let Some(error) = &ctx.llm_keys.profile_error {
        return Err(failed(ctx, format!("invalid LLM settings: {error}")));
    }

    let plan_model = resolve_to_string(&cfg.model, ctx);
    let profile = ctx
        .llm_keys
        .profile
        .clone()
        .unwrap_or_else(|| legacy_profile(ctx, &plan_model));
    // The active application connection owns provider and model selection.
    // This keeps older plans (which often contain a Claude example model)
    // portable across account CLIs and compatible endpoints. Library callers
    // without a profile retain the plan's legacy per-step model behavior.
    let model = if ctx.llm_keys.profile.is_some() {
        profile.model.clone()
    } else {
        plan_model
    };
    let user = resolve_to_string(&cfg.user_prompt, ctx);
    let system = cfg
        .system_prompt
        .as_deref()
        .map(|value| resolve_to_string(value, ctx));

    let started = std::time::Instant::now();
    tracing::info!(
        name: "inxm.executor.external.started",
        run_id = %ctx.run_id,
        plan_id = %ctx.plan.metadata.id,
        step_id = %ctx.step.id,
        runner_kind = "prompt_call",
        runner_profile_id = %profile.id,
        "external runner started"
    );
    let response = crate::llm::complete(
        &profile,
        CompletionRequest {
            system: system.as_deref(),
            user: &user,
            model: Some(&model),
            max_tokens: cfg.max_tokens,
            temperature: cfg.temperature,
        },
    )
    .await
    .map_err(|error| {
        tracing::warn!(
            name: "inxm.executor.external.completed",
            run_id = %ctx.run_id,
            plan_id = %ctx.plan.metadata.id,
            step_id = %ctx.step.id,
            runner_kind = "prompt_call",
            runner_profile_id = %profile.id,
            runner_duration_ms = started.elapsed().as_millis() as u64,
            runner_outcome = "failed",
            failure_class = llm_failure_classification(&error),
            "external runner completed"
        );
        failed(ctx, format!("{} completion failed: {error}", profile.name))
    })?;
    tracing::info!(
        name: "inxm.executor.external.completed",
        run_id = %ctx.run_id,
        plan_id = %ctx.plan.metadata.id,
        step_id = %ctx.step.id,
        runner_kind = "prompt_call",
        runner_profile_id = %profile.id,
        runner_duration_ms = started.elapsed().as_millis() as u64,
        runner_outcome = "succeeded",
        "external runner completed"
    );

    let usage = response.input_tokens.map(|input_tokens| TokenUsage {
        input_tokens,
        output_tokens: response.output_tokens.unwrap_or(0),
    });
    let mut outputs = IndexMap::new();
    outputs.insert(
        cfg.output_field.clone(),
        serde_json::Value::String(response.text.clone()),
    );
    Ok(StepResult {
        outputs,
        stdout: Some(response.text),
        stderr: None,
        usage,
        child_runs: IndexMap::new(),
    })
}

fn llm_failure_classification(error: &crate::llm::LlmError) -> &'static str {
    use crate::llm::LlmError;
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

fn legacy_profile(ctx: &StepContext, model: &str) -> LlmProfile {
    let claude = model.starts_with(ANTHROPIC_MODEL_PREFIX);
    LlmProfile {
        id: "legacy-runtime".to_owned(),
        name: if claude {
            "Anthropic API".to_owned()
        } else {
            "OpenAI API".to_owned()
        },
        protocol: if claude {
            LlmProtocol::AnthropicMessages
        } else {
            LlmProtocol::OpenAiChat
        },
        model: model.to_owned(),
        base_url: if claude {
            "https://api.anthropic.com/v1".to_owned()
        } else {
            "https://api.openai.com/v1".to_owned()
        },
        api_key: if claude {
            ctx.llm_keys.anthropic.clone().unwrap_or_default()
        } else {
            ctx.llm_keys.openai.clone().unwrap_or_default()
        },
        auth: LlmAuth::Auto,
        headers: Default::default(),
        executable: String::new(),
        command_template: String::new(),
        max_tokens: None,
        temperature: None,
        timeout_secs: LEGACY_PROFILE_TIMEOUT_SECS,
        codex_sandbox_mode: crate::llm::CodexSandboxMode::default(),
    }
}

fn failed(ctx: &StepContext, message: impl Into<String>) -> ExecutorError {
    ExecutorError::StepFailed {
        step_id: ctx.step.id.clone(),
        message: message.into(),
    }
}
