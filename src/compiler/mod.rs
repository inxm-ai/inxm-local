//! Compiler module — the ONLY place in the system where an LLM is invoked.
//!
//! The compiler transforms natural-language intent into a typed [`Plan`] IR.
//! The executor never calls back into the compiler at runtime.
//!
//! # Usage
//!
//! ```no_run
//! # async fn example() -> Result<(), inxm_local::error::CompilerError> {
//! use inxm_local::compiler::{self, BackendConfig, BackendKind, CompileRequest};
//! use inxm_local::plan::types::StepType;
//!
//! let config = BackendConfig {
//!     kind: BackendKind::Claude,
//!     model: None,   // uses default
//!     api_key: None, // falls back to ANTHROPIC_API_KEY env var
//!     api_base: None,
//!     max_tokens: None,
//!     temperature: None,
//! };
//!
//! let backend = compiler::create_backend(&config)?;
//! let plan = backend.compile(CompileRequest {
//!     intent: "fetch the top 10 GitHub repos for a user and save them to disk".into(),
//!     allowed_step_types: vec![StepType::ToolCall, StepType::CodeCall],
//!     tool_catalog: vec![],
//!     existing_plan: None,
//!     run_history: vec![],
//!     extra_context: None,
//! }).await?;
//! # Ok(())
//! # }
//! ```

pub mod backend;
pub mod config;
mod diagnostics;
pub mod extractor;
pub mod prompt;

pub use backend::{
    AssessRequest, Backend, CompileRequest, CompileRunHistoryEntry, CompileRunIteration,
    CompileRunStep, CompilerBackend, CompletionPort, DesignRequest, IntentAssessment, OutlineStep,
    OutlineStepKind, RecommendedTool, RepairRequest, SolutionDesign, SpecDraft, SpecInput,
    SpecTurn, ToolSynthesisRequest,
};
pub use config::{BackendConfig, BackendKind, DEFAULT_MAX_TOKENS};

use crate::error::CompilerError;
use crate::llm::{LlmAuth, LlmProfile, LlmProtocol};

const ANTHROPIC_API_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";
const DEFAULT_CLAUDE_MODEL: &str = "claude-sonnet-4-6";
const DEFAULT_OPENAI_MODEL: &str = "gpt-4o";
const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com/v1";
const DEFAULT_OPENAI_API_BASE: &str = "https://api.openai.com/v1";
const DEFAULT_TEMPERATURE: f32 = 0.0;
const LEGACY_PROFILE_TIMEOUT_SECS: u64 = 300;

/// Create a compiler backend from configuration.
///
/// API keys are resolved in priority order:
/// 1. `config.api_key` (if `Some`)
/// 2. The environment variable for the chosen backend
///    (`ANTHROPIC_API_KEY` for Claude, `OPENAI_API_KEY` for OpenAI)
///
/// Returns `Err(CompilerError::Config)` if no key is available.
pub fn create_backend(config: &BackendConfig) -> Result<Backend, CompilerError> {
    let (id, name, protocol, auth, key_env, default_model, base_url) = match &config.kind {
        BackendKind::Claude => (
            "legacy-claude",
            "Claude API",
            LlmProtocol::AnthropicMessages,
            LlmAuth::AnthropicKey,
            ANTHROPIC_API_KEY_ENV,
            DEFAULT_CLAUDE_MODEL,
            ANTHROPIC_API_BASE.to_owned(),
        ),
        BackendKind::OpenAI => (
            "legacy-openai",
            "OpenAI API",
            LlmProtocol::OpenAiChat,
            LlmAuth::Bearer,
            OPENAI_API_KEY_ENV,
            DEFAULT_OPENAI_MODEL,
            config
                .api_base
                .clone()
                .unwrap_or_else(|| DEFAULT_OPENAI_API_BASE.to_owned()),
        ),
    };
    let api_key = config
        .api_key
        .clone()
        .or_else(|| std::env::var(key_env).ok())
        .ok_or_else(|| {
            CompilerError::Config(format!("no api_key in config and {key_env} is not set"))
        })?;
    Backend::from_profile(LlmProfile {
        id: id.to_owned(),
        name: name.to_owned(),
        protocol,
        model: config
            .model
            .clone()
            .unwrap_or_else(|| default_model.to_owned()),
        api_key,
        base_url,
        auth,
        headers: Default::default(),
        executable: String::new(),
        command_template: String::new(),
        max_tokens: Some(config.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        temperature: Some(config.temperature.unwrap_or(DEFAULT_TEMPERATURE)),
        timeout_secs: LEGACY_PROFILE_TIMEOUT_SECS,
        codex_sandbox_mode: crate::llm::CodexSandboxMode::default(),
    })
}

pub fn create_profile_backend(profile: LlmProfile) -> Result<Backend, CompilerError> {
    Backend::from_profile(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_key(kind: BackendKind) -> BackendConfig {
        BackendConfig {
            kind,
            model: None,
            api_key: Some("test-key".to_owned()),
            api_base: None,
            max_tokens: None,
            temperature: None,
        }
    }

    #[test]
    fn claude_config_maps_to_an_anthropic_profile_with_defaults() {
        let backend = create_backend(&config_with_key(BackendKind::Claude)).unwrap();
        let profile = backend.profile();

        assert!(matches!(profile.protocol, LlmProtocol::AnthropicMessages));
        assert!(matches!(profile.auth, LlmAuth::AnthropicKey));
        assert_eq!(profile.model, DEFAULT_CLAUDE_MODEL);
        assert_eq!(profile.base_url, ANTHROPIC_API_BASE);
        assert_eq!(profile.api_key, "test-key");
        assert_eq!(profile.max_tokens, Some(DEFAULT_MAX_TOKENS));
        assert_eq!(profile.temperature, Some(DEFAULT_TEMPERATURE));
    }

    #[test]
    fn openai_config_maps_to_a_chat_profile_and_honours_overrides() {
        let mut config = config_with_key(BackendKind::OpenAI);
        config.model = Some("gpt-5".to_owned());
        config.api_base = Some("https://llm.example.com/v1".to_owned());
        config.max_tokens = Some(4096);
        config.temperature = Some(0.7);

        let backend = create_backend(&config).unwrap();
        let profile = backend.profile();

        assert!(matches!(profile.protocol, LlmProtocol::OpenAiChat));
        assert!(matches!(profile.auth, LlmAuth::Bearer));
        assert_eq!(profile.model, "gpt-5");
        assert_eq!(profile.base_url, "https://llm.example.com/v1");
        assert_eq!(profile.max_tokens, Some(4096));
        assert_eq!(profile.temperature, Some(0.7));
    }

    #[test]
    fn openai_config_without_api_base_uses_the_official_endpoint() {
        let backend = create_backend(&config_with_key(BackendKind::OpenAI)).unwrap();
        let profile = backend.profile();

        assert_eq!(profile.model, DEFAULT_OPENAI_MODEL);
        assert_eq!(profile.base_url, DEFAULT_OPENAI_API_BASE);
    }
}
