//! Shared LLM transports used by both the compiler and `PROMPT_CALL` steps.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Applied when neither the request nor the profile sets a token budget.
const DEFAULT_MAX_TOKENS: u32 = 1024;
/// Wide enough to keep long CLI/agent turns alive; overridable per profile.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Pinned `anthropic-version` header sent with every Messages API request.
const ANTHROPIC_API_VERSION: &str = "2023-06-01";
/// Upper bound on error-body/stderr text carried into an error message.
const ERROR_BODY_EXCERPT_CHARS: usize = 4_000;
/// Upper bound on raw-output snippets embedded in parse-error messages.
const RAW_SNIPPET_EXCERPT_CHARS: usize = 1_000;
/// `gcloud auth print-access-token` can be slow on a cold credential cache.
const GCLOUD_TOKEN_TIMEOUT_SECS: u64 = 30;
/// The GCE metadata server is link-local; anything slower means we're not on GCP.
const METADATA_TOKEN_TIMEOUT_SECS: u64 = 3;
/// Token endpoint of the GCE/Cloud Run metadata server.
const METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmProtocol {
    OpenAiChat,
    AnthropicMessages,
    GoogleVertex,
    CodexCli,
    ClaudeCli,
    CustomCli,
}

/// How a [`LlmProtocol::CodexCli`] connection handles Codex's own OS-level
/// sandbox (Landlock/bwrap on Linux, Seatbelt on macOS, restricted tokens on
/// Windows). That sandbox is separate from — and additional to — the process
/// isolation this app already applies (workspace confinement, process-group
/// kill on timeout); on some hosts (notably Ubuntu 24.04+, where AppArmor
/// blocks unprivileged user-namespace creation by default) it fails to
/// initialize at all, which fails every command the CLI tries to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CodexSandboxMode {
    /// Try Codex's normal sandbox first; if it fails to initialize (as
    /// opposed to an ordinary command failure inside a working sandbox),
    /// retry once with `--sandbox danger-full-access` and note the
    /// degraded isolation in the run's audit transcript.
    #[default]
    Auto,
    /// Never fall back — a sandbox-init failure is a hard error with a
    /// remediation hint for this OS.
    Strict,
    /// Always use `--sandbox danger-full-access`, skipping Codex's own OS
    /// sandbox entirely. For hosts where it's known to be broken.
    Unsandboxed,
}

/// Substituted with the rendered prompt inside a [`LlmProfile::command_template`].
/// When the template doesn't contain this placeholder, the prompt is piped to
/// the process's stdin instead (matching how the Codex/Claude Code CLIs work).
pub const CUSTOM_CLI_PROMPT_PLACEHOLDER: &str = "{{PROMPT}}";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LlmAuth {
    /// Use the protocol's conventional environment variable when no explicit
    /// key is configured.
    #[default]
    Auto,
    None,
    Bearer,
    AnthropicKey,
    /// Resolve a Google Cloud access token from the caller's identity:
    /// `gcloud auth print-access-token`, falling back to the GCE metadata
    /// server when running on Google Cloud.
    GcloudIdentity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmProfile {
    pub id: String,
    pub name: String,
    pub protocol: LlmProtocol,
    pub model: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub auth: LlmAuth,
    #[serde(default)]
    pub headers: IndexMap<String, String>,
    #[serde(default)]
    pub executable: String,
    /// Full command line for [`LlmProtocol::CustomCli`], e.g.
    /// `opencode run --print "{{PROMPT}}"`. See [`CUSTOM_CLI_PROMPT_PLACEHOLDER`].
    #[serde(default)]
    pub command_template: String,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Only consulted for [`LlmProtocol::CodexCli`].
    #[serde(default)]
    pub codex_sandbox_mode: CodexSandboxMode,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

impl LlmProfile {
    pub fn label(&self) -> &'static str {
        match self.protocol {
            LlmProtocol::OpenAiChat => "openai",
            LlmProtocol::AnthropicMessages => "claude",
            LlmProtocol::GoogleVertex => "vertex",
            LlmProtocol::CodexCli => "codex",
            LlmProtocol::ClaudeCli => "claude-code",
            LlmProtocol::CustomCli => "custom-cli",
        }
    }

    pub fn validate(&self) -> Result<(), LlmError> {
        if matches!(
            self.protocol,
            LlmProtocol::OpenAiChat | LlmProtocol::AnthropicMessages | LlmProtocol::GoogleVertex
        ) {
            if self.model.trim().is_empty() {
                return Err(LlmError::Config("model must not be empty".to_owned()));
            }
            let url = reqwest::Url::parse(self.base_url.trim())
                .map_err(|e| LlmError::Config(format!("invalid base URL: {e}")))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(LlmError::Config(
                    "base URL must use http or https".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct CompletionRequest<'a> {
    pub system: Option<&'a str>,
    pub user: &'a str,
    pub model: Option<&'a str>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionResponse {
    pub text: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("request failed: {0}")]
    Request(String),
    #[error("provider returned HTTP {status}: {message}")]
    Http {
        status: reqwest::StatusCode,
        message: String,
    },
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    #[error("{provider} CLI is not installed or could not be started: {message}")]
    CliStart {
        provider: &'static str,
        message: String,
    },
    #[error("{provider} CLI exited with status {status}: {message}")]
    CliExit {
        provider: &'static str,
        status: String,
        message: String,
    },
    #[error("{provider} request timed out after {secs}s")]
    Timeout { provider: &'static str, secs: u64 },
}

pub async fn complete(
    profile: &LlmProfile,
    request: CompletionRequest<'_>,
) -> Result<CompletionResponse, LlmError> {
    profile.validate()?;
    match profile.protocol {
        LlmProtocol::OpenAiChat => complete_openai(profile, request).await,
        LlmProtocol::AnthropicMessages => complete_anthropic(profile, request).await,
        LlmProtocol::GoogleVertex => complete_vertex(profile, request).await,
        LlmProtocol::CodexCli => complete_codex(profile, request).await,
        LlmProtocol::ClaudeCli => complete_claude_cli(profile, request).await,
        LlmProtocol::CustomCli => complete_custom_cli(profile, request).await,
    }
}

fn resolved_key(profile: &LlmProfile) -> Option<String> {
    if !profile.api_key.trim().is_empty() {
        return Some(profile.api_key.trim().to_owned());
    }
    match (profile.auth, profile.protocol) {
        (LlmAuth::Auto, LlmProtocol::OpenAiChat) => std::env::var("OPENAI_API_KEY").ok(),
        (LlmAuth::Auto, LlmProtocol::AnthropicMessages) => std::env::var("ANTHROPIC_API_KEY").ok(),
        _ => None,
    }
}

fn endpoint(base: &str, suffix: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with(suffix) {
        base.to_owned()
    } else {
        format!("{base}/{suffix}")
    }
}

fn apply_headers(
    mut builder: reqwest::RequestBuilder,
    profile: &LlmProfile,
) -> Result<reqwest::RequestBuilder, LlmError> {
    let key = resolved_key(profile);
    let auth = match profile.auth {
        LlmAuth::Auto => match profile.protocol {
            LlmProtocol::OpenAiChat => LlmAuth::Bearer,
            LlmProtocol::AnthropicMessages => LlmAuth::AnthropicKey,
            _ => LlmAuth::None,
        },
        other => other,
    };
    match (auth, key) {
        (LlmAuth::Bearer, Some(key)) => builder = builder.bearer_auth(key),
        (LlmAuth::AnthropicKey, Some(key)) => builder = builder.header("x-api-key", key),
        (LlmAuth::Bearer | LlmAuth::AnthropicKey, None) => {
            return Err(LlmError::Config(
                "this connection requires an API key".to_owned(),
            ));
        }
        _ => {}
    }
    for (name, value) in &profile.headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| LlmError::Config(format!("invalid header name: {e}")))?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|e| LlmError::Config(format!("invalid header value: {e}")))?;
        builder = builder.header(name, value);
    }
    Ok(builder)
}

async fn send_json(
    profile: &LlmProfile,
    url: String,
    extra_headers: &[(&'static str, &'static str)],
    body: serde_json::Value,
) -> Result<serde_json::Value, LlmError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(profile.timeout_secs))
        .build()
        .map_err(|e| LlmError::Request(e.to_string()))?;
    let mut builder = client.post(url).json(&body);
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
    }
    let request = apply_headers(builder, profile)?;
    let response = request
        .send()
        .await
        .map_err(|e| LlmError::Request(e.to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| LlmError::Request(e.to_string()))?;
    if !status.is_success() {
        return Err(LlmError::Http {
            status,
            message: truncate(&text, ERROR_BODY_EXCERPT_CHARS),
        });
    }
    serde_json::from_str(&text).map_err(|e| {
        LlmError::InvalidResponse(format!(
            "invalid JSON ({e}): {}",
            truncate(&text, RAW_SNIPPET_EXCERPT_CHARS)
        ))
    })
}

async fn complete_openai(
    profile: &LlmProfile,
    request: CompletionRequest<'_>,
) -> Result<CompletionResponse, LlmError> {
    let mut messages = Vec::new();
    if let Some(system) = request.system {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    messages.push(serde_json::json!({"role": "user", "content": request.user}));
    let mut body = serde_json::json!({
        "model": request.model.unwrap_or(&profile.model),
        "messages": messages,
        "max_tokens": request.max_tokens.or(profile.max_tokens).unwrap_or(DEFAULT_MAX_TOKENS),
    });
    if let Some(temperature) = request.temperature.or(profile.temperature) {
        body["temperature"] = serde_json::json!(temperature);
    }
    let json = send_json(
        profile,
        endpoint(&profile.base_url, "chat/completions"),
        &[],
        body,
    )
    .await?;
    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| {
            LlmError::InvalidResponse("missing choices[0].message.content".to_owned())
        })?;
    Ok(CompletionResponse {
        text: text.to_owned(),
        input_tokens: json["usage"]["prompt_tokens"].as_u64(),
        output_tokens: json["usage"]["completion_tokens"].as_u64(),
    })
}

async fn complete_anthropic(
    profile: &LlmProfile,
    request: CompletionRequest<'_>,
) -> Result<CompletionResponse, LlmError> {
    let max_tokens = request
        .max_tokens
        .or(profile.max_tokens)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    let mut body = serde_json::json!({
        "model": request.model.unwrap_or(&profile.model),
        "messages": [{"role": "user", "content": request.user}],
        "max_tokens": max_tokens,
    });
    if let Some(system) = request.system {
        body["system"] = serde_json::Value::String(system.to_owned());
    }
    let json = send_json(
        profile,
        endpoint(&profile.base_url, "messages"),
        &[("anthropic-version", ANTHROPIC_API_VERSION)],
        body,
    )
    .await?;
    parse_anthropic_response(&json, max_tokens)
}

fn parse_anthropic_response(
    json: &serde_json::Value,
    max_tokens: u32,
) -> Result<CompletionResponse, LlmError> {
    if let Some(text) = json["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|block| block["type"] == "text"))
        .and_then(|block| block["text"].as_str())
    {
        return Ok(CompletionResponse {
            text: text.to_owned(),
            input_tokens: json["usage"]["input_tokens"].as_u64(),
            output_tokens: json["usage"]["output_tokens"].as_u64(),
        });
    }

    let stop_reason = json["stop_reason"].as_str().unwrap_or("unknown");
    let thinking = json["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|block| block["type"] == "thinking"))
        .and_then(|block| block["thinking"].as_str())
        .filter(|thinking| !thinking.trim().is_empty());
    let hint = if stop_reason == "max_tokens" {
        format!(
            " — the model exhausted max_tokens ({max_tokens}) before producing output; try raising \"Max tokens\" in Settings"
        )
    } else {
        String::new()
    };
    let message = match thinking {
        Some(thinking) => format!(
            "no text block in response content (stop_reason: {stop_reason}){hint}\n\nmodel's thinking so far:\n\n{thinking}"
        ),
        None => format!("no text block in response content (stop_reason: {stop_reason}){hint}"),
    };
    Err(LlmError::InvalidResponse(message))
}

async fn complete_vertex(
    profile: &LlmProfile,
    request: CompletionRequest<'_>,
) -> Result<CompletionResponse, LlmError> {
    let model = request.model.unwrap_or(&profile.model).trim();
    let mut body = serde_json::json!({
        "contents": [{"role": "user", "parts": [{"text": request.user}]}],
        "generationConfig": {
            "maxOutputTokens": request.max_tokens.or(profile.max_tokens).unwrap_or(DEFAULT_MAX_TOKENS),
        },
    });
    if let Some(temperature) = request.temperature.or(profile.temperature) {
        body["generationConfig"]["temperature"] = serde_json::json!(temperature);
    }
    if let Some(system) = request.system {
        body["systemInstruction"] = serde_json::json!({"parts": [{"text": system}]});
    }
    // Token resolution is async, but header application is not, so hand
    // `send_json` a profile that already carries the resolved bearer token.
    let mut authed = profile.clone();
    if !matches!(profile.auth, LlmAuth::None) {
        authed.api_key = match resolved_key(profile) {
            Some(token) => token,
            None => gcloud_access_token(profile).await?,
        };
        authed.auth = LlmAuth::Bearer;
    }
    let json = send_json(
        &authed,
        endpoint(&profile.base_url, &format!("{model}:generateContent")),
        &[],
        body,
    )
    .await?;
    parse_vertex_response(&json)
}

/// Resolve a Google Cloud access token from the ambient identity: the
/// `gcloud` CLI when available, otherwise the GCE metadata server (covers
/// Compute Engine, Cloud Run, and GKE workloads without gcloud installed).
async fn gcloud_access_token(profile: &LlmProfile) -> Result<String, LlmError> {
    let executable = if profile.executable.trim().is_empty() {
        "gcloud"
    } else {
        profile.executable.trim()
    };
    let cli_error = match run_cli(
        "gcloud",
        executable,
        &["auth".to_owned(), "print-access-token".to_owned()],
        String::new(),
        GCLOUD_TOKEN_TIMEOUT_SECS,
    )
    .await
    {
        Ok(output) if output.status.success() => {
            let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if !token.is_empty() {
                return Ok(token);
            }
            "gcloud printed an empty access token".to_owned()
        }
        Ok(output) => cli_exit("gcloud", &output).to_string(),
        Err(error) => error.to_string(),
    };
    metadata_access_token().await.map_err(|metadata_error| {
        LlmError::Config(format!(
            "could not obtain a Google Cloud access token — gcloud CLI: {cli_error}; metadata server: {metadata_error}"
        ))
    })
}

async fn metadata_access_token() -> Result<String, LlmError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(METADATA_TOKEN_TIMEOUT_SECS))
        .build()
        .map_err(|e| LlmError::Request(e.to_string()))?;
    let response = client
        .get(METADATA_TOKEN_URL)
        .header("Metadata-Flavor", "Google")
        .send()
        .await
        .map_err(|e| LlmError::Request(e.to_string()))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| LlmError::Request(e.to_string()))?;
    if !status.is_success() {
        return Err(LlmError::Http {
            status,
            message: truncate(&text, ERROR_BODY_EXCERPT_CHARS),
        });
    }
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| LlmError::InvalidResponse(format!("invalid metadata token JSON: {e}")))?;
    json["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            LlmError::InvalidResponse("metadata server returned no access_token".to_owned())
        })
}

pub fn parse_vertex_response(json: &serde_json::Value) -> Result<CompletionResponse, LlmError> {
    let candidate = &json["candidates"][0];
    let text: String = candidate["content"]["parts"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect()
        })
        .unwrap_or_default();
    if text.is_empty() {
        let finish_reason = candidate["finishReason"].as_str().unwrap_or("unknown");
        return Err(LlmError::InvalidResponse(format!(
            "no text parts in candidate content (finishReason: {finish_reason})"
        )));
    }
    Ok(CompletionResponse {
        text,
        input_tokens: json["usageMetadata"]["promptTokenCount"].as_u64(),
        output_tokens: json["usageMetadata"]["candidatesTokenCount"].as_u64(),
    })
}

fn combined_prompt(request: &CompletionRequest<'_>) -> String {
    match request.system {
        Some(system) => format!(
            "Follow the system instructions below and return only the requested answer.\n\n<system>\n{system}\n</system>\n\n<user>\n{}\n</user>",
            request.user
        ),
        None => request.user.to_owned(),
    }
}

/// Receives live CLI output lines while a `run_cli` subprocess runs.
/// Implemented by the app layer's compile console; `run_cli`
/// looks the sink up through a task-local so the many call layers between
/// the engine and the subprocess stay untouched.
pub trait CliLineSink: Send + Sync {
    fn cli_line(&self, stream: CliLineStream, text: &str);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliLineStream {
    Stdout,
    Stderr,
}

tokio::task_local! {
    /// The sink for the current async task tree, set via [`with_cli_line_sink`].
    static CLI_LINE_SINK: std::sync::Arc<dyn CliLineSink>;
}

/// Run `future` with `sink` receiving every CLI output line produced by
/// `run_cli` calls inside it (including nested retries).
pub async fn with_cli_line_sink<F: std::future::Future>(
    sink: std::sync::Arc<dyn CliLineSink>,
    future: F,
) -> F::Output {
    CLI_LINE_SINK.scope(sink, future).await
}

fn active_cli_line_sink() -> Option<std::sync::Arc<dyn CliLineSink>> {
    CLI_LINE_SINK.try_with(|sink| sink.clone()).ok()
}

async fn run_cli(
    provider: &'static str,
    executable: &str,
    args: &[String],
    stdin: String,
    timeout_secs: u64,
) -> Result<std::process::Output, LlmError> {
    // Resolve through `hostenv` so a CLI installed in a shell-rc location
    // (`~/.local/bin`, nvm/volta bins, Homebrew) is still found when the app
    // was launched from a desktop/tray with a minimal `PATH`. Falls back to
    // the bare name so the OS `ENOENT` stays meaningful when truly missing.
    let resolved = crate::hostenv::resolve_program(executable);
    let mut child = tokio::process::Command::new(&resolved)
        .args(args)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| LlmError::CliStart {
            provider,
            message: e.to_string(),
        })?;
    if let Some(mut input) = child.stdin.take() {
        input
            .write_all(stdin.as_bytes())
            .await
            .map_err(|e| LlmError::Request(e.to_string()))?;
    }
    // Drain stdout/stderr incrementally so a task-local sink can
    // show live progress during minutes-long CLI calls; the collected bytes
    // still return as one `Output`, so parsing callers are unaffected.
    let sink = active_cli_line_sink();
    let stdout_task = tokio::spawn(drain_cli_pipe(
        child.stdout.take(),
        CliLineStream::Stdout,
        sink.clone(),
    ));
    let stderr_task = tokio::spawn(drain_cli_pipe(
        child.stderr.take(),
        CliLineStream::Stderr,
        sink,
    ));
    let status = tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait())
        .await
        .map_err(|_| LlmError::Timeout {
            provider,
            secs: timeout_secs,
        })?
        .map_err(|e| LlmError::Request(e.to_string()))?;
    // The child has exited, so both pipes hit EOF promptly.
    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Read one std pipe to EOF, echoing each line to the sink as it arrives and
/// returning the exact collected bytes. `read_until` keeps byte fidelity for
/// JSONL parsers; only the sink copy is lossily decoded for display.
async fn drain_cli_pipe<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    pipe: Option<R>,
    stream: CliLineStream,
    sink: Option<std::sync::Arc<dyn CliLineSink>>,
) -> Vec<u8> {
    use tokio::io::AsyncBufReadExt;
    let Some(pipe) = pipe else {
        return Vec::new();
    };
    let mut reader = tokio::io::BufReader::new(pipe);
    let mut collected = Vec::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if let Some(sink) = &sink {
            let text = String::from_utf8_lossy(&buf);
            sink.cli_line(stream, text.trim_end_matches(['\r', '\n']));
        }
        collected.extend_from_slice(&buf);
    }
    collected
}

async fn complete_codex(
    profile: &LlmProfile,
    request: CompletionRequest<'_>,
) -> Result<CompletionResponse, LlmError> {
    let executable = if profile.executable.trim().is_empty() {
        "codex"
    } else {
        profile.executable.trim()
    };
    let model = request.model.unwrap_or(&profile.model).trim();
    let sandbox = initial_codex_sandbox_arg(profile.codex_sandbox_mode, "read-only");
    let mut args = vec![
        "exec".to_owned(),
        "--ephemeral".to_owned(),
        "--sandbox".to_owned(),
        sandbox.to_owned(),
        "--skip-git-repo-check".to_owned(),
        "--json".to_owned(),
    ];
    if !model.is_empty() {
        args.push("--model".to_owned());
        args.push(model.to_owned());
    }
    args.push("-".to_owned());
    let stdin = combined_prompt(&request);
    let mut output = run_cli(
        "Codex",
        executable,
        &args,
        stdin.clone(),
        profile.timeout_secs,
    )
    .await?;
    if !output.status.success()
        && rejects_flag(
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
            "--ephemeral",
        )
    {
        args.retain(|arg| arg != "--ephemeral");
        output = run_cli(
            "Codex",
            executable,
            &args,
            stdin.clone(),
            profile.timeout_secs,
        )
        .await?;
    }
    if !output.status.success()
        && matches!(profile.codex_sandbox_mode, CodexSandboxMode::Auto)
        && sandbox_init_failed(
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        )
    {
        replace_sandbox_arg(&mut args, "danger-full-access");
        output = run_cli("Codex", executable, &args, stdin, profile.timeout_secs).await?;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        if matches!(profile.codex_sandbox_mode, CodexSandboxMode::Strict)
            && sandbox_init_failed(&stdout, &String::from_utf8_lossy(&output.stderr))
        {
            return Err(LlmError::CliExit {
                provider: "Codex",
                status: output.status.to_string(),
                message: format!(
                    "Codex's sandbox failed to initialize.{}",
                    sandbox_remediation_hint()
                ),
            });
        }
        return Err(codex_error(&stdout).unwrap_or_else(|| cli_exit("Codex", &output)));
    }
    parse_codex_jsonl(&stdout)
}

/// The `--sandbox` value Codex's own retry ladder starts from: the caller's
/// usual choice (`read-only` for completions, `workspace-write` for
/// AGENT_CALL), unless the profile has opted straight into no OS sandbox.
pub(crate) fn initial_codex_sandbox_arg(
    mode: CodexSandboxMode,
    default_sandboxed_mode: &'static str,
) -> &'static str {
    match mode {
        CodexSandboxMode::Unsandboxed => "danger-full-access",
        CodexSandboxMode::Auto | CodexSandboxMode::Strict => default_sandboxed_mode,
    }
}

/// Overwrite the value following a `--sandbox` flag in an argv, in place.
pub(crate) fn replace_sandbox_arg(args: &mut [String], value: &'static str) {
    if let Some(position) = args.iter().position(|arg| arg == "--sandbox")
        && let Some(slot) = args.get_mut(position + 1)
    {
        *slot = value.to_owned();
    }
}

/// Recognises a Codex sandbox-*initialization* failure — the OS-level
/// isolation itself (Landlock/bwrap on Linux, Seatbelt on macOS, restricted
/// tokens on Windows) never came up — as opposed to an ordinary command or
/// tool failure running normally inside a working sandbox. Heuristic, in the
/// same spirit as [`rejects_flag`]: Codex's own escalation logic
/// (`is_likely_sandbox_denied`) is stderr/exit-code heuristic too.
pub(crate) fn sandbox_init_failed(stdout: &str, stderr: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "failed to create sandbox",
        "failed to initialize sandbox",
        "sandbox setup failed",
        "could not create sandbox",
        "user namespace",
        "unshare(",
        "landlock",
        "bwrap",
        "seatbelt",
        "sandbox-exec",
        "apparmor",
        "restrict_unprivileged_userns",
    ];
    let haystack = format!("{stdout}\n{stderr}").to_lowercase();
    NEEDLES.iter().any(|needle| haystack.contains(needle))
}

/// Remediation text for a sandbox-init failure, specific to the OS this
/// binary is running on — the fix (and the debug subcommand to confirm it)
/// differs per platform.
pub(crate) fn sandbox_remediation_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "\n\nCodex's macOS Seatbelt sandbox couldn't start — this is often an MDM/sandbox-exec \
         restriction. Diagnose with `codex debug seatbelt -- echo ok` in a terminal. You can \
         also switch this connection's sandbox mode to Unsandboxed in Settings."
    } else if cfg!(target_os = "windows") {
        "\n\nCodex's Windows sandbox couldn't start — it needs rights to create a restricted \
         sandbox user and firewall rules, which usually means running as Administrator or \
         asking your IT team. You can also switch this connection's sandbox mode to \
         Unsandboxed in Settings."
    } else {
        "\n\nCodex's Linux sandbox (Landlock/bwrap) couldn't start — commonly because unprivileged \
         user namespaces are blocked (the default on Ubuntu 24.04+). Try \
         `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`, or diagnose directly \
         with `codex debug landlock -- echo ok`. You can also switch this connection's sandbox \
         mode to Unsandboxed in Settings."
    }
}

/// Probe whether Codex's OS sandbox can actually initialize on this host,
/// independent of the model backend: runs Codex's own sandbox debug helper
/// around a no-op command, so this needs no auth and spends no tokens.
/// Windows has no equivalent debug subcommand, so it isn't probed — the
/// remediation hint there covers the common failure modes instead.
pub async fn test_codex_sandbox(executable: &str) -> Result<(), String> {
    if cfg!(target_os = "windows") {
        return Err(
            "Codex has no sandbox self-test on Windows. If AGENT_CALL steps fail with a \
             sandbox error, see the remediation hint in that failure, or switch this \
             connection's sandbox mode to Unsandboxed."
                .to_owned(),
        );
    }
    let executable = if executable.trim().is_empty() {
        "codex"
    } else {
        executable.trim()
    };
    let debug_subcommand = if cfg!(target_os = "macos") {
        "seatbelt"
    } else {
        "landlock"
    };
    let resolved = crate::hostenv::resolve_program(executable);
    let output = tokio::process::Command::new(&resolved)
        .args(["debug", debug_subcommand, "--", "true"])
        .output()
        .await
        .map_err(|error| format!("failed to run '{}': {error}", resolved.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|part| !part.is_empty())
        .unwrap_or("(no output on stderr or stdout)");
    Err(format!(
        "{}{}",
        truncate(detail, ERROR_BODY_EXCERPT_CHARS),
        sandbox_remediation_hint()
    ))
}

/// Whether `stdout`/`stderr` carries a clap-style "unknown option" rejection
/// of `flag`. Codex added `--ephemeral` to `exec` in a recent release; an
/// older CLI install rejects it outright rather than ignoring it, so this
/// lets the caller drop the flag and retry once instead of hard-failing on
/// that version skew.
pub(crate) fn rejects_flag(stdout: &str, stderr: &str, flag: &str) -> bool {
    (stdout.contains("unknown option") || stderr.contains("unknown option"))
        && (stdout.contains(flag) || stderr.contains(flag))
}

/// Codex CLI writes its actual failure detail (`turn.failed`/`error` items)
/// to stdout as JSONL, not to stderr, so a plain exit-status check surfaces
/// nothing useful. Scan stdout for that detail before falling back to the
/// generic stderr-based error.
fn codex_error(stdout: &str) -> Option<LlmError> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            codex_failure(line, &value)
        })
}

/// The error carried by a Codex JSONL failure item, if `value` is one.
fn codex_failure(line: &str, value: &serde_json::Value) -> Option<LlmError> {
    (value["type"] == "turn.failed" || value["type"] == "error").then(|| {
        LlmError::InvalidResponse(format!(
            "Codex reported an error: {}",
            truncate(line, RAW_SNIPPET_EXCERPT_CHARS)
        ))
    })
}

async fn complete_claude_cli(
    profile: &LlmProfile,
    request: CompletionRequest<'_>,
) -> Result<CompletionResponse, LlmError> {
    let executable = if profile.executable.trim().is_empty() {
        "claude"
    } else {
        profile.executable.trim()
    };
    let model = request.model.unwrap_or(&profile.model).trim();
    let mut args = vec![
        "-p".to_owned(),
        "--output-format".to_owned(),
        "json".to_owned(),
        "--no-session-persistence".to_owned(),
        "--tools".to_owned(),
        String::new(),
    ];
    if !model.is_empty() {
        args.push("--model".to_owned());
        args.push(model.to_owned());
    }
    let output = run_cli(
        "Claude Code",
        executable,
        &args,
        combined_prompt(&request),
        profile.timeout_secs,
    )
    .await?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        // Prefer the structured error Claude Code writes to stdout as JSON
        // (e.g. an expired-login `is_error` result); fall back to the combined
        // stdout/stderr + auth hint when the output is not parseable JSON.
        return Err(claude_error(&stdout).unwrap_or_else(|| cli_exit("Claude Code", &output)));
    }
    parse_claude_json(&stdout)
}

/// The structured error Claude Code emits to stdout on failure, if `stdout`
/// parses as its result JSON with `is_error: true`. Returns `None` when the
/// output is not that shape, so the caller can fall back to a stream dump.
fn claude_error(stdout: &str) -> Option<LlmError> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    value["is_error"].as_bool().unwrap_or(false).then(|| {
        let detail = value["result"].as_str().unwrap_or("(no detail)");
        let mut message = format!("Claude Code reported an error: {detail}");
        if let Some(hint) = auth_hint("Claude Code", detail) {
            message.push_str(hint);
        }
        LlmError::InvalidResponse(message)
    })
}

/// Renders a [`LlmProfile::command_template`] against a prompt into an argv
/// (executable + args) and the stdin payload to feed the child process.
/// Split out from [`complete_custom_cli`] so the substitution/parsing logic
/// is unit-testable without spawning a real process.
fn render_custom_cli_command(
    template: &str,
    prompt: String,
) -> Result<(String, Vec<String>, String), LlmError> {
    let template = template.trim();
    if template.is_empty() {
        return Err(LlmError::Config(
            "custom CLI command must not be empty".to_owned(),
        ));
    }
    let has_placeholder = template.contains(CUSTOM_CLI_PROMPT_PLACEHOLDER);
    let rendered = if has_placeholder {
        template.replace(CUSTOM_CLI_PROMPT_PLACEHOLDER, &prompt)
    } else {
        template.to_owned()
    };
    let mut parts = shlex::split(&rendered).ok_or_else(|| {
        LlmError::Config("could not parse custom CLI command (unmatched quotes?)".to_owned())
    })?;
    if parts.is_empty() {
        return Err(LlmError::Config(
            "custom CLI command must not be empty".to_owned(),
        ));
    }
    let executable = parts.remove(0);
    let stdin = if has_placeholder {
        String::new()
    } else {
        prompt
    };
    Ok((executable, parts, stdin))
}

/// Runs an arbitrary user-configured CLI agent (e.g. `cline`, `opencode`) as
/// the compiler backend. Unlike the Codex/Claude Code integrations above,
/// this CLI's argument shape and output format are unknown ahead of time, so
/// the prompt is substituted into the user's own command template rather than
/// assembled from fixed flags, and the raw trimmed stdout is returned as-is
/// rather than parsed as a known JSON shape.
async fn complete_custom_cli(
    profile: &LlmProfile,
    request: CompletionRequest<'_>,
) -> Result<CompletionResponse, LlmError> {
    let prompt = combined_prompt(&request);
    let (executable, args, stdin) = render_custom_cli_command(&profile.command_template, prompt)?;
    let output = run_cli(
        "Custom CLI",
        &executable,
        &args,
        stdin,
        profile.timeout_secs,
    )
    .await?;
    if !output.status.success() {
        return Err(cli_exit("Custom CLI", &output));
    }
    Ok(CompletionResponse {
        text: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        input_tokens: None,
        output_tokens: None,
    })
}

fn cli_exit(provider: &'static str, output: &std::process::Output) -> LlmError {
    // These CLIs split their failure detail across streams: `claude -p
    // --output-format json` writes an expired-login / auth error to *stdout*
    // (as JSON), while spawn/runtime failures land on stderr. Reading stderr
    // alone surfaced a bare "exited with status 1" with no explanation, so
    // combine both and prefer whichever carries text.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|part| !part.is_empty())
        .unwrap_or("(no output on stderr or stdout)");
    let mut message = truncate(detail, ERROR_BODY_EXCERPT_CHARS);
    if let Some(hint) = auth_hint(provider, &format!("{stderr}\n{stdout}")) {
        message.push_str(hint);
    }
    LlmError::CliExit {
        provider,
        status: output.status.to_string(),
        message,
    }
}

/// Recognise an expired/absent CLI login in the combined output and return a
/// remediation hint. The `claude`/`codex` CLIs authenticate out-of-band (their
/// own `login` flow, not an API key this app holds), so the only fix is for
/// the user to re-authenticate in a terminal — say so explicitly instead of
/// leaving them with a raw error.
fn auth_hint(provider: &'static str, combined: &str) -> Option<&'static str> {
    const NEEDLES: &[&str] = &[
        "login",
        "log in",
        "logged in",
        "authenticat",
        "unauthor",
        "not authenticated",
        "invalid api key",
        "expired",
        "session has expired",
        "credential",
        "please run",
        "oauth",
        "token has expired",
        "re-authenticate",
    ];
    let haystack = combined.to_lowercase();
    NEEDLES
        .iter()
        .any(|n| haystack.contains(n))
        .then_some(match provider {
            "Claude Code" => {
                "\n\nThe Claude Code CLI session looks expired or logged out. Run `claude` in a terminal and complete /login, then try again."
            }
            "Codex" => {
                "\n\nThe Codex CLI session looks expired or logged out. Run `codex login` in a terminal, then try again."
            }
            _ => "\n\nThe CLI session looks expired or logged out. Re-authenticate in a terminal, then try again.",
        })
}

pub fn parse_codex_jsonl(raw: &str) -> Result<CompletionResponse, LlmError> {
    let mut text = None;
    let mut input_tokens = None;
    let mut output_tokens = None;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| LlmError::InvalidResponse(format!("invalid Codex JSONL: {e}")))?;
        if value["type"] == "item.completed" && value["item"]["type"] == "agent_message" {
            text = value["item"]["text"].as_str().map(str::to_owned);
        }
        if value["type"] == "turn.completed" {
            input_tokens = value["usage"]["input_tokens"].as_u64();
            output_tokens = value["usage"]["output_tokens"].as_u64();
        }
        if let Some(error) = codex_failure(line, &value) {
            return Err(error);
        }
    }
    Ok(CompletionResponse {
        text: text.ok_or_else(|| {
            LlmError::InvalidResponse("Codex returned no final agent message".to_owned())
        })?,
        input_tokens,
        output_tokens,
    })
}

pub fn parse_claude_json(raw: &str) -> Result<CompletionResponse, LlmError> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| LlmError::InvalidResponse(format!("invalid Claude Code JSON: {e}")))?;
    if value["is_error"].as_bool().unwrap_or(false) {
        return Err(LlmError::InvalidResponse(
            value["result"]
                .as_str()
                .unwrap_or("Claude Code reported an error")
                .to_owned(),
        ));
    }
    Ok(CompletionResponse {
        text: value["result"]
            .as_str()
            .ok_or_else(|| LlmError::InvalidResponse("missing Claude Code result".to_owned()))?
            .to_owned(),
        input_tokens: value["usage"]["input_tokens"].as_u64(),
        output_tokens: value["usage"]["output_tokens"].as_u64(),
    })
}

fn truncate(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_profile(protocol: LlmProtocol, base_url: String) -> LlmProfile {
        LlmProfile {
            id: "test".to_owned(),
            name: "test".to_owned(),
            protocol,
            model: "test-model".to_owned(),
            base_url,
            api_key: String::new(),
            auth: LlmAuth::None,
            headers: Default::default(),
            executable: String::new(),
            command_template: String::new(),
            max_tokens: None,
            temperature: None,
            timeout_secs: 5,
            codex_sandbox_mode: CodexSandboxMode::default(),
        }
    }

    #[test]
    fn appends_protocol_paths_once() {
        assert_eq!(
            endpoint("http://localhost:11434/v1", "chat/completions"),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            endpoint("http://localhost/v1/chat/completions", "chat/completions"),
            "http://localhost/v1/chat/completions"
        );
    }

    #[test]
    fn parses_codex_final_message_and_usage() {
        let raw = r#"{"type":"item.completed","item":{"type":"agent_message","text":"hello"}}
{"type":"turn.completed","usage":{"input_tokens":12,"output_tokens":3}}"#;
        let result = parse_codex_jsonl(raw).unwrap();
        assert_eq!(result.text, "hello");
        assert_eq!(result.input_tokens, Some(12));
        assert_eq!(result.output_tokens, Some(3));
    }

    #[test]
    fn extracts_codex_error_from_stdout_jsonl() {
        let raw = r#"{"type":"thread.started","thread_id":"abc"}
{"type":"turn.started"}
{"type":"error","message":"the 'gpt-5' model is not supported when using Codex with a ChatGPT account"}
{"type":"turn.failed","error":{"message":"the 'gpt-5' model is not supported when using Codex with a ChatGPT account"}}"#;
        let error = codex_error(raw).expect("should find an error line");
        assert!(error.to_string().contains("not supported"));
    }

    #[test]
    fn codex_error_is_none_for_clean_stdout() {
        let raw = r#"{"type":"item.completed","item":{"type":"agent_message","text":"hello"}}
{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#;
        assert!(codex_error(raw).is_none());
    }

    #[test]
    fn rejects_flag_detects_clap_unknown_option() {
        assert!(rejects_flag(
            "",
            "error: unknown option '--ephemeral'\n\nUsage: codex exec [OPTIONS] [PROMPT]",
            "--ephemeral"
        ));
    }

    #[test]
    fn sandbox_init_failed_detects_linux_userns_denial() {
        assert!(sandbox_init_failed(
            "",
            "error: failed to initialize sandbox: unshare(CLONE_NEWUSER): Operation not permitted"
        ));
        assert!(sandbox_init_failed(
            "",
            "bwrap: setting up uid map: Permission denied"
        ));
    }

    #[test]
    fn sandbox_init_failed_detects_macos_seatbelt_denial() {
        assert!(sandbox_init_failed(
            "",
            "sandbox-exec: could not create sandbox: Seatbelt initialization failed"
        ));
    }

    #[test]
    fn sandbox_init_failed_ignores_ordinary_command_failures() {
        assert!(!sandbox_init_failed(
            "",
            "error: unknown option '--model'\n\nUsage: codex exec [OPTIONS] [PROMPT]"
        ));
        assert!(!sandbox_init_failed(
            "",
            "npm ERR! command failed with exit code 1"
        ));
    }

    #[test]
    fn initial_codex_sandbox_arg_honors_unsandboxed_override() {
        assert_eq!(
            initial_codex_sandbox_arg(CodexSandboxMode::Auto, "workspace-write"),
            "workspace-write"
        );
        assert_eq!(
            initial_codex_sandbox_arg(CodexSandboxMode::Strict, "read-only"),
            "read-only"
        );
        assert_eq!(
            initial_codex_sandbox_arg(CodexSandboxMode::Unsandboxed, "workspace-write"),
            "danger-full-access"
        );
    }

    #[test]
    fn replace_sandbox_arg_rewrites_the_value_in_place() {
        let mut args = vec![
            "exec".to_owned(),
            "--sandbox".to_owned(),
            "workspace-write".to_owned(),
            "--json".to_owned(),
        ];
        replace_sandbox_arg(&mut args, "danger-full-access");
        assert_eq!(args[2], "danger-full-access");
        assert_eq!(args.len(), 4, "must not insert or remove args");
    }

    #[test]
    fn replace_sandbox_arg_is_a_noop_without_the_flag() {
        let mut args = vec!["exec".to_owned(), "--json".to_owned()];
        replace_sandbox_arg(&mut args, "danger-full-access");
        assert_eq!(args, vec!["exec".to_owned(), "--json".to_owned()]);
    }

    #[test]
    fn rejects_flag_ignores_unrelated_failures() {
        assert!(!rejects_flag("", "error: model not found", "--ephemeral"));
        assert!(!rejects_flag(
            "",
            "error: unknown option '--model'",
            "--ephemeral"
        ));
    }

    #[test]
    fn parses_claude_result() {
        let result = parse_claude_json(r#"{"is_error":false,"result":"hello"}"#).unwrap();
        assert_eq!(result.text, "hello");
    }

    #[test]
    fn custom_cli_placeholder_is_substituted_into_argv_not_stdin() {
        let (executable, args, stdin) = render_custom_cli_command(
            r#"opencode run --print "{{PROMPT}}""#,
            "hello world".to_owned(),
        )
        .unwrap();
        assert_eq!(executable, "opencode");
        assert_eq!(args, vec!["run", "--print", "hello world"]);
        assert_eq!(stdin, "");
    }

    #[test]
    fn custom_cli_without_placeholder_pipes_prompt_to_stdin() {
        let (executable, args, stdin) =
            render_custom_cli_command("claude -p", "hello world".to_owned()).unwrap();
        assert_eq!(executable, "claude");
        assert_eq!(args, vec!["-p"]);
        assert_eq!(stdin, "hello world");
    }

    #[test]
    fn custom_cli_empty_command_is_rejected() {
        assert!(render_custom_cli_command("   ", "hi".to_owned()).is_err());
    }

    #[test]
    fn custom_cli_unmatched_quotes_are_rejected() {
        assert!(render_custom_cli_command(r#"opencode "unterminated"#, "hi".to_owned()).is_err());
    }

    #[test]
    fn auth_hint_flags_expired_login_per_provider() {
        assert!(
            auth_hint("Claude Code", "session has expired, please run /login")
                .unwrap()
                .contains("/login")
        );
        assert!(
            auth_hint("Codex", "Not authenticated")
                .unwrap()
                .contains("codex login")
        );
        assert!(auth_hint("Claude Code", "here is your normal answer").is_none());
    }

    #[test]
    fn claude_error_surfaces_structured_error_with_auth_hint() {
        let error = claude_error(r#"{"is_error":true,"result":"Invalid API key · run /login"}"#)
            .expect("is_error:true should yield an error");
        let message = error.to_string();
        assert!(message.contains("Invalid API key"));
        assert!(
            message.contains("/login"),
            "expected an auth remediation hint"
        );
    }

    #[test]
    fn claude_error_is_none_for_success_and_non_json() {
        assert!(claude_error(r#"{"is_error":false,"result":"ok"}"#).is_none());
        assert!(claude_error("not json at all").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn cli_exit_falls_back_to_stdout_when_stderr_empty() {
        use std::os::unix::process::ExitStatusExt;
        // Claude Code writes its expired-login detail to stdout, not stderr.
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: b"login expired, please re-authenticate".to_vec(),
            stderr: Vec::new(),
        };
        let message = cli_exit("Claude Code", &output).to_string();
        assert!(
            message.contains("login expired"),
            "stdout detail must survive"
        );
        assert!(message.contains("/login"), "auth hint must be appended");
    }

    #[test]
    fn parses_anthropic_text_after_thinking() {
        let response = serde_json::json!({
            "content": [
                {"type": "thinking", "thinking": "reasoning", "signature": "sig"},
                {"type": "text", "text": "final answer"}
            ],
            "usage": {"input_tokens": 5, "output_tokens": 8}
        });

        let result = parse_anthropic_response(&response, 32_000).unwrap();
        assert_eq!(result.text, "final answer");
        assert_eq!(result.output_tokens, Some(8));
    }

    #[test]
    fn explains_anthropic_thinking_only_max_token_response() {
        let response = serde_json::json!({
            "content": [
                {"type": "thinking", "thinking": "unfinished reasoning", "signature": "sig"}
            ],
            "stop_reason": "max_tokens"
        });

        let error = parse_anthropic_response(&response, 32_000).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("exhausted max_tokens (32000)"));
        assert!(message.contains("unfinished reasoning"));
    }

    #[tokio::test]
    async fn calls_openai_compatible_endpoint_without_auth() {
        async fn handler(
            headers: axum::http::HeaderMap,
            axum::Json(body): axum::Json<serde_json::Value>,
        ) -> axum::Json<serde_json::Value> {
            assert!(headers.get(axum::http::header::AUTHORIZATION).is_none());
            assert_eq!(body["model"], "test-model");
            axum::Json(serde_json::json!({
                "choices": [{"message": {"content": "local reply"}}],
                "usage": {"prompt_tokens": 4, "completion_tokens": 2}
            }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(handler));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let response = complete(
            &http_profile(LlmProtocol::OpenAiChat, format!("http://{address}/v1")),
            CompletionRequest {
                system: Some("system"),
                user: "hello",
                model: None,
                max_tokens: None,
                temperature: None,
            },
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(response.text, "local reply");
        assert_eq!(response.input_tokens, Some(4));
    }

    #[test]
    fn parses_vertex_candidate_and_usage() {
        let response = serde_json::json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "part one "}, {"text": "part two"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 4}
        });

        let result = parse_vertex_response(&response).unwrap();
        assert_eq!(result.text, "part one part two");
        assert_eq!(result.input_tokens, Some(7));
        assert_eq!(result.output_tokens, Some(4));
    }

    #[test]
    fn explains_vertex_response_without_text() {
        let response = serde_json::json!({
            "candidates": [{"content": {"role": "model"}, "finishReason": "SAFETY"}]
        });

        let error = parse_vertex_response(&response).unwrap_err();
        assert!(error.to_string().contains("finishReason: SAFETY"));
    }

    #[tokio::test]
    async fn calls_vertex_endpoint_with_explicit_token() {
        async fn handler(
            headers: axum::http::HeaderMap,
            axum::Json(body): axum::Json<serde_json::Value>,
        ) -> axum::Json<serde_json::Value> {
            assert_eq!(
                headers[axum::http::header::AUTHORIZATION],
                "Bearer test-token"
            );
            assert_eq!(body["contents"][0]["parts"][0]["text"], "hello");
            assert_eq!(body["systemInstruction"]["parts"][0]["text"], "system");
            assert_eq!(body["generationConfig"]["maxOutputTokens"], 1024);
            axum::Json(serde_json::json!({
                "candidates": [{"content": {"parts": [{"text": "vertex reply"}]}}],
                "usageMetadata": {"promptTokenCount": 6, "candidatesTokenCount": 2}
            }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/v1/projects/p/locations/l/publishers/google/models/test-model:generateContent",
            axum::routing::post(handler),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut profile = http_profile(
            LlmProtocol::GoogleVertex,
            format!("http://{address}/v1/projects/p/locations/l/publishers/google/models"),
        );
        profile.api_key = "test-token".to_owned();
        profile.auth = LlmAuth::GcloudIdentity;
        let response = complete(
            &profile,
            CompletionRequest {
                system: Some("system"),
                user: "hello",
                model: None,
                max_tokens: None,
                temperature: None,
            },
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(response.text, "vertex reply");
        assert_eq!(response.input_tokens, Some(6));
        assert_eq!(response.output_tokens, Some(2));
    }

    #[tokio::test]
    async fn calls_anthropic_compatible_endpoint_with_key() {
        async fn handler(
            headers: axum::http::HeaderMap,
            axum::Json(body): axum::Json<serde_json::Value>,
        ) -> axum::Json<serde_json::Value> {
            assert_eq!(headers["x-api-key"], "test-key");
            assert_eq!(headers["anthropic-version"], "2023-06-01");
            assert_eq!(body["model"], "test-model");
            assert_eq!(body["max_tokens"], 32_000);
            assert!(body.get("temperature").is_none());
            axum::Json(serde_json::json!({
                "content": [{"type": "text", "text": "gateway reply"}],
                "usage": {"input_tokens": 5, "output_tokens": 3}
            }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/api/messages", axum::routing::post(handler));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut profile = http_profile(
            LlmProtocol::AnthropicMessages,
            format!("http://{address}/api"),
        );
        profile.api_key = "test-key".to_owned();
        profile.auth = LlmAuth::AnthropicKey;
        profile.max_tokens = Some(32_000);
        profile.temperature = Some(0.0);
        let response = complete(
            &profile,
            CompletionRequest {
                system: None,
                user: "hello",
                model: None,
                max_tokens: None,
                temperature: Some(0.5),
            },
        )
        .await
        .unwrap();
        server.abort();
        assert_eq!(response.text, "gateway reply");
        assert_eq!(response.output_tokens, Some(3));
    }
}
