//! Compiler backend configuration.

use serde::{Deserialize, Serialize};

/// Default max output tokens for a compiler request when not explicitly
/// configured. Generous enough to leave headroom for extended-thinking
/// models, which spend part of this budget on reasoning before they emit
/// any visible output — too small a budget can exhaust itself mid-thought
/// and leave no text block at all.
pub const DEFAULT_MAX_TOKENS: u32 = 32_000;

/// Which AI provider to use for compilation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    Claude,
    #[serde(rename = "openai")]
    OpenAI,
}

/// Configuration for constructing a compiler backend.
///
/// API keys can be supplied directly or left as `None` — in that case
/// `create_backend` will read `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` from
/// the process environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    pub kind: BackendKind,
    /// Model identifier, e.g. `"claude-sonnet-4-6"` or `"gpt-4o"`.
    /// Falls back to a sensible default when `None`.
    pub model: Option<String>,
    /// API key. When `None`, resolved from the environment at backend creation time.
    pub api_key: Option<String>,
    /// Base URL for OpenAI-compatible endpoints.
    /// Defaults to `"https://api.openai.com/v1"`.
    pub api_base: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}
