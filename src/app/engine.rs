//! Async engine — bridges the sync egui thread and the tokio workflow core.
//!
//! The UI sends [`EngineCommand`]s over an unbounded channel; a dedicated
//! thread running a tokio runtime executes them (compile, run, repair, …)
//! and streams [`EngineEvent`]s back over a `std::sync::mpsc` channel,
//! nudging egui with `request_repaint` after every event. The UI thread
//! never blocks on I/O or the network.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::compiler::{
    self, BackendKind, CompileRequest, DEFAULT_MAX_TOKENS, ToolSynthesisRequest,
};
use crate::executor::{self, ExecutorConfig, HumanRequest, ProgressEvent, Run};
use crate::hostenv::EnvProbe;
use crate::llm::{LlmAuth, LlmProfile, LlmProtocol};
use crate::plan::bundle::{PlanBundle, ToolReference};
use crate::plan::normalization::normalize;
use crate::plan::types::{Plan, PlanMetadata, PlanStatus, StepType};
use crate::repair;
use crate::storage::StorageRoot;
use crate::storage::patches::{Patch, PatchStatus};
use crate::storage::plan_edits::PlanEdit;
use crate::storage::world_fixes::WorldFix;
use crate::tools::catalog::{
    HttpConfig, McpDiscoveredTool, McpTransport, SubprocessConfig, ToolCatalog, ToolConfig,
    ToolEntry,
};
use crate::tools::oauth::{McpOAuthFacade, OAuthConnectionStatus};
use crate::validator;

use super::activity::{ActivityKind, ActivityOrigin, ActivityRegistry};
use super::commands;
use super::console::CompileConsole;
use super::schedule_store;
use super::scheduler_lock::{LockAcquisition, SchedulerLock};
use super::support;

/// A UI-nudge hook the engine calls after emitting an event. The desktop wires
/// in `egui::Context::request_repaint`; headless mode passes a no-op so the
/// exact same engine code runs without an egui context.
pub type RepaintHook = Arc<dyn Fn() + Send + Sync>;

/// A repaint hook that does nothing — used by headless runs.
pub fn no_repaint() -> RepaintHook {
    Arc::new(|| {})
}

// ─── Configuration ────────────────────────────────────────────────────────────

pub const DATA_DIR_ENV: &str = "INXM_LOCAL_DATA_DIR";
const CATALOG_FILE_NAME: &str = "tools.yaml";
const SETTINGS_FILE_NAME: &str = "settings.json";
const SCHEDULES_FILE_NAME: &str = "schedules.json";
const SCHEDULER_LOCK_FILE_NAME: &str = "scheduler.lock";
/// How often the scheduler checks for due cron slots.
const SCHEDULER_TICK: std::time::Duration = std::time::Duration::from_secs(20);
/// Scheduled runs are unattended by default. A human prompt or other single
/// step that receives no response is failed after this bound.
const SCHEDULED_STEP_TIMEOUT_SECS: u64 = 60 * 60;
const ENGINE_THREAD_NAME: &str = "inxm-engine";
const ANTHROPIC_KEY_ENV: &str = "ANTHROPIC_API_KEY";
const OPENAI_KEY_ENV: &str = "OPENAI_API_KEY";
// Complex repository workflows produce larger typed plans than ordinary user
// automations. Account-backed CLIs — and slower local models (e.g. a reasoning
// model served through Ollama) that spend minutes thinking before emitting the
// plan JSON — need enough time to finish one bounded compiler call; higher-level
// validation retries remain separately bounded.
const COMPILER_REQUEST_TIMEOUT_SECS: u64 = 1800;
/// Keep edit prompts useful without allowing an unbounded local run archive to
/// consume the compiler context window. Run summaries are newest-first.
const EDIT_RUN_HISTORY_LIMIT: usize = 5;
const MCP_OAUTH_CALLBACK_PATH: &str = "/callback";
const MCP_OAUTH_CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// The starter catalog, with an `echo` tool that works on the current OS.
/// Dynamic arguments reach subprocess tools as `INXM_ARG_*` env vars, so the
/// command reads the message from its environment rather than command text.
pub(crate) fn default_catalog_yaml() -> String {
    default_catalog_yaml_for(cfg!(windows))
}

fn default_catalog_yaml_for(windows: bool) -> String {
    let echo_config = match windows {
        true => format!(
            "kind: subprocess\n      command: powershell\n      args: [\"-NoProfile\", \"-NonInteractive\", \"-ExecutionPolicy\", \"Bypass\", \"-Command\", '{WINDOWS_UTF8_ECHO_SCRIPT}']"
        ),
        false => {
            "kind: subprocess\n      command: sh\n      args: [\"-c\", \"printf '%s\\\\n' \\\"$INXM_ARG_MESSAGE\\\"\"]".to_owned()
        }
    };
    DEFAULT_CATALOG_YAML.replace(ECHO_CONFIG_PLACEHOLDER, &echo_config)
}

const ECHO_CONFIG_PLACEHOLDER: &str = "__ECHO_CONFIG__";
const WINDOWS_UTF8_ECHO_SCRIPT: &str = "$inxmUtf8 = New-Object System.Text.UTF8Encoding $false; [Console]::InputEncoding = $inxmUtf8; [Console]::OutputEncoding = $inxmUtf8; $OutputEncoding = $inxmUtf8; [Console]::Out.WriteLine($env:INXM_ARG_MESSAGE)";

/// Seeded on first launch so the app is usable before any tool is added.
///
/// Besides a plain `echo`, this wires up the reference MCP servers
/// (time, fetch, filesystem — they need `uvx`/`npx` on PATH and are
/// downloaded on first use) and one public HTTP API for a network demo.
const DEFAULT_CATALOG_YAML: &str = r#"tools:
  - name: echo
    description: Echoes its input to stdout
    config:
      __ECHO_CONFIG__
    input_schema:
      type: object
      properties:
        message:
          type: string
      required: [message]
    output_schema:
      type: object
      properties:
        stdout:
          type: string
    allowlisted: true

  - name: http-get
    description: Fetch an arbitrary URL with the built-in HTTP client; use this instead of shelling out to curl/wget/Invoke-WebRequest
    config:
      kind: http
      base_url: ""
      method: GET
      path_template: "{url}"
      timeout_secs: 60
    input_schema:
      type: object
      properties:
        url:
          type: string
      required: [url]
    output_schema:
      type: object
      properties:
        body:
          type: string
    allowlisted: true

  - name: current-time
    description: Current date and time for a timezone (reference MCP time server, needs uvx)
    config:
      kind: mcp
      server_command: uvx
      server_args: ["--with", "mcp<2", mcp-server-time]
      tool_name: get_current_time
    input_schema:
      type: object
      properties:
        timezone:
          type: string
          description: IANA timezone name, e.g. Europe/Vienna
      required: [timezone]
    allowlisted: true

  - name: web-fetch
    description: Fetch a URL and return its content as markdown (reference MCP fetch server, needs uvx)
    config:
      kind: mcp
      server_command: uvx
      server_args: ["--with", "mcp<2", mcp-server-fetch]
      tool_name: fetch
    input_schema:
      type: object
      properties:
        url:
          type: string
        max_length:
          type: integer
      required: [url]
    allowlisted: true

  - name: read-file
    description: Read a text file below the working directory (reference MCP filesystem server, needs npx)
    config:
      kind: mcp
      server_command: npx
      server_args: ["-y", "@modelcontextprotocol/server-filesystem", "."]
      tool_name: read_text_file
    input_schema:
      type: object
      properties:
        path:
          type: string
          x-inxm-input-kind: file_path
      required: [path]
    allowlisted: true

  - name: write-file
    description: Write a text file below the working directory (reference MCP filesystem server, needs npx)
    config:
      kind: mcp
      server_command: npx
      server_args: ["-y", "@modelcontextprotocol/server-filesystem", "."]
      tool_name: write_file
    input_schema:
      type: object
      properties:
        path:
          type: string
          x-inxm-input-kind: output_file_path
        content:
          type: string
      required: [path, content]
    allowlisted: true

  - name: btc-price
    description: Current Bitcoin price in USD (CoinGecko public API, no key needed)
    config:
      kind: http
      base_url: https://api.coingecko.com
      method: GET
      path_template: /api/v3/simple/price?ids=bitcoin&vs_currencies=usd
    input_schema:
      type: object
    allowlisted: true
"#;

/// Where the engine keeps plans, runs, patches, and the tool catalog.
#[derive(Debug, Clone)]
pub struct DataPaths {
    pub data_dir: PathBuf,
    pub catalog_path: PathBuf,
    pub settings_path: PathBuf,
    pub schedules_path: PathBuf,
    /// Single-writer lock for the scheduler loop, next to the schedule store.
    pub scheduler_lock_path: PathBuf,
    /// Shared by every adapter created from these paths so persistent
    /// read-modify-write operations have one owner inside this process.
    pub mutations: super::mutation::MutationBoundary,
}

/// Explicit policy for an imported plan whose name already exists locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportConflictPolicy {
    #[default]
    Reject,
    NewVersion,
    Duplicate,
}

/// What the collision resolver did. Returned to MCP callers so a client never
/// has to infer a destructive choice from a plan id alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ImportOutcome {
    Imported,
    Rejected,
    NewVersion,
    Duplicate,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ImportResolution {
    pub outcome: ImportOutcome,
    pub plan: Plan,
    pub same_name_plan_ids: Vec<String>,
}

impl DataPaths {
    /// `$INXM_LOCAL_DATA_DIR` if set, otherwise the platform data dir
    /// (XDG / AppData / Application Support), otherwise `.inxm-local/`
    /// in the working directory.
    pub fn resolve() -> Self {
        let data_dir = std::env::var(DATA_DIR_ENV)
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                directories::ProjectDirs::from("ai", "inxm", "inxm-local")
                    .map(|d| d.data_dir().to_path_buf())
            })
            .unwrap_or_else(|| PathBuf::from(".inxm-local"));
        Self::at(data_dir)
    }

    /// Paths rooted at an explicit data dir (used by tests).
    pub fn at(data_dir: PathBuf) -> Self {
        let catalog_path = data_dir.join(CATALOG_FILE_NAME);
        let settings_path = data_dir.join(SETTINGS_FILE_NAME);
        let schedules_path = data_dir.join(SCHEDULES_FILE_NAME);
        let scheduler_lock_path = data_dir.join(SCHEDULER_LOCK_FILE_NAME);
        Self {
            data_dir,
            catalog_path,
            settings_path,
            schedules_path,
            scheduler_lock_path,
            mutations: super::mutation::MutationBoundary::default(),
        }
    }
}

// ─── Compiler settings ────────────────────────────────────────────────────────

/// Which LLM compiles plans. `Auto` picks from available env keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendChoice {
    #[default]
    Auto,
    Claude,
    OpenAi,
    Codex,
    ClaudeCode,
    GoogleVertex,
    OpenAiCompatible,
    AnthropicCompatible,
    CustomCli,
}

/// Which visual theme to use. `System` follows the OS light/dark setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

/// Resolve a theme preference to an actual dark/light choice, given what the
/// OS reports (`None` when the backend can't tell, e.g. some Linux setups).
pub fn resolve_dark_mode(preference: ThemePreference, system: Option<egui::Theme>) -> bool {
    match preference {
        ThemePreference::Dark => true,
        ThemePreference::Light => false,
        ThemePreference::System => !matches!(system, Some(egui::Theme::Light)),
    }
}

/// User-configurable compiler settings, persisted as `settings.json` in the
/// data dir. Empty strings mean "use the default / environment".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AppSettings {
    /// Visual theme preference. Defaults to following the OS setting.
    #[serde(default)]
    pub theme_preference: ThemePreference,
    #[serde(default)]
    pub backend: BackendChoice,
    /// Model id override; empty → the backend's built-in default.
    #[serde(default)]
    pub model: String,
    /// API key; empty → the backend's environment variable.
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub api_base: String,
    #[serde(default)]
    pub executable: String,
    /// Full command line for [`BackendChoice::CustomCli`], e.g.
    /// `opencode run --print "{{PROMPT}}"`. Empty pipes the prompt to stdin
    /// instead — see `crate::llm::CUSTOM_CLI_PROMPT_PLACEHOLDER`.
    #[serde(default)]
    pub command_template: String,
    /// Explicitly declares that a custom CLI is a real agent runtime (tools,
    /// workspace access, and multi-turn operation), rather than a bare text
    /// completion command.
    #[serde(default)]
    pub custom_cli_agentic: bool,
    /// Enables the experimental AGENT_CALL step. It is intentionally off by
    /// default because agent steps can run arbitrary commands and modify the
    /// selected workspace.
    #[serde(default)]
    pub experimental_agent_calls: bool,
    /// Skips exactly one gate of the guided create-a-plan flow: the DESIGN
    /// phase's "✓ Approve design" click. With it on, a design is compiled
    /// into a plan the moment it arrives instead of waiting for approval.
    /// Nothing else changes — the design still renders in the panel, the
    /// REFINE phase still asks its clarifying questions, and running a
    /// compiled plan still requires the usual actions and HUMAN_INTERACTION
    /// approvals. Off by default.
    #[serde(default)]
    pub auto_mode: bool,
    /// Max output tokens per compiler request; 0 → backend default
    /// ([`DEFAULT_MAX_TOKENS`]). Extended-thinking models spend part of this
    /// budget on reasoning before producing visible output, so a value
    /// that's too small can exhaust itself mid-thought.
    #[serde(default)]
    pub max_tokens: u32,
    /// Local Streamable-HTTP MCP port. Change this if startup reports a bind
    /// collision.
    #[serde(default = "default_mcp_port")]
    pub mcp_port: u16,
    /// Whether to silently check GitHub releases for a newer version on
    /// startup. Purely informational — never auto-downloads or installs.
    #[serde(default = "default_check_updates_on_startup")]
    pub check_updates_on_startup: bool,
    /// Close-to-tray preference. `None` (the default) behaves as enabled —
    /// closing the window hides it rather than quitting, so a schedule can
    /// always keep running whether or not one has been created yet. Only an
    /// explicit `Some(false)` (the Settings checkbox unchecked) makes the
    /// window close quit the app outright.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_running_in_background: Option<bool>,
    /// Global scheduler pause controlled from the tray. Individual schedules
    /// retain their own enabled state while this is set.
    #[serde(default)]
    pub schedules_paused: bool,
    /// Whether the first-run setup assistant has already been shown (and
    /// dismissed, however it was dismissed). Deliberately asymmetric from
    /// every other one-shot flag here: a `settings.json` written before this
    /// field existed must deserialize to `true` — an *existing* install —
    /// so upgrading users are never shown a "first run" experience they
    /// never asked for. Only [`AppSettings::default`] (used when there is no
    /// settings file at all, i.e. a genuine new install) produces `false`.
    #[serde(default = "default_onboarding_completed")]
    pub onboarding_completed: bool,
    /// Anonymous usage telemetry (see `docs/telemetry.md`).
    /// On by default *at the setup assistant*: its checkbox starts checked
    /// and dismissing the card persists the checkbox as shown, so opting
    /// out means unchecking it. `None` means the user was never shown the
    /// disclosure — settings files that predate the field, or installs
    /// that never rendered the assistant (headless/agent) — and is treated
    /// exactly like `Some(false)`: collection before disclosure is never
    /// allowed. Runtime kill switches (`INXM_TELEMETRY=off`,
    /// `--no-telemetry`) override this in `crate::telemetry`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry_enabled: Option<bool>,
    /// Only consulted when `backend == BackendChoice::Codex`. See
    /// [`crate::llm::CodexSandboxMode`].
    #[serde(default)]
    pub codex_sandbox_mode: crate::llm::CodexSandboxMode,
}

pub const DEFAULT_MCP_PORT: u16 = 39387;

fn default_mcp_port() -> u16 {
    DEFAULT_MCP_PORT
}

fn default_check_updates_on_startup() -> bool {
    true
}

fn default_onboarding_completed() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme_preference: ThemePreference::default(),
            backend: BackendChoice::default(),
            model: String::new(),
            api_key: String::new(),
            api_base: String::new(),
            executable: String::new(),
            command_template: String::new(),
            custom_cli_agentic: false,
            experimental_agent_calls: false,
            auto_mode: false,
            max_tokens: 0,
            mcp_port: DEFAULT_MCP_PORT,
            check_updates_on_startup: true,
            keep_running_in_background: None,
            schedules_paused: false,
            // A brand-new install (no settings.json at all) is the one case
            // that should see the first-run assistant.
            onboarding_completed: false,
            telemetry_enabled: None,
            codex_sandbox_mode: crate::llm::CodexSandboxMode::default(),
        }
    }
}

impl AppSettings {
    /// Select a compiler backend without carrying an executable override from
    /// a different CLI-backed connection into the new profile.
    pub fn select_backend(&mut self, backend: BackendChoice) {
        if self.backend == backend {
            return;
        }
        if uses_executable_override(self.backend) || uses_executable_override(backend) {
            self.executable.clear();
        }
        self.backend = backend;
    }

    /// Whether plans may contain AGENT_CALL steps under this configuration.
    /// API backends remain completion-only even when the experimental toggle
    /// is enabled; only account CLIs and an explicitly agent-shaped custom CLI
    /// provide the real tool-using execution loop that the step promises.
    pub fn supports_agent_call(&self) -> bool {
        self.experimental_agent_calls
            && match self.backend {
                BackendChoice::Codex | BackendChoice::ClaudeCode => true,
                BackendChoice::CustomCli => self.custom_cli_agentic,
                _ => false,
            }
    }

    /// Load settings, falling back to defaults when the file is missing or
    /// unreadable (a broken settings file must never brick the app).
    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(std::fs::write(path, serde_json::to_string_pretty(self)?)?)
    }

    /// The concrete backend kind, resolving `Auto` from available env keys.
    pub fn resolved_kind(&self) -> Option<BackendKind> {
        match self.backend {
            BackendChoice::Claude => Some(BackendKind::Claude),
            BackendChoice::OpenAi => Some(BackendKind::OpenAI),
            BackendChoice::Codex
            | BackendChoice::ClaudeCode
            | BackendChoice::GoogleVertex
            | BackendChoice::OpenAiCompatible
            | BackendChoice::AnthropicCompatible
            | BackendChoice::CustomCli => None,
            BackendChoice::Auto => [
                (ANTHROPIC_KEY_ENV, BackendKind::Claude),
                (OPENAI_KEY_ENV, BackendKind::OpenAI),
            ]
            .into_iter()
            .find(|(var, _)| std::env::var(var).is_ok())
            .map(|(_, kind)| kind),
        }
    }

    /// The configured max-tokens override, or `None` to use the backend
    /// default.
    pub fn resolved_max_tokens(&self) -> Option<u32> {
        Some(self.max_tokens).filter(|&v| v > 0)
    }

    /// The max-tokens value that will actually be used, resolving to the
    /// backend default when unset. Handy for UI hints.
    pub fn effective_max_tokens(&self) -> u32 {
        self.resolved_max_tokens().unwrap_or(DEFAULT_MAX_TOKENS)
    }

    /// Whether the selected connection has enough configuration to be tried.
    pub fn has_key(&self) -> bool {
        match self.backend {
            BackendChoice::Codex | BackendChoice::ClaudeCode => true,
            // Identity auth plus a default model — only the endpoint is needed.
            BackendChoice::GoogleVertex => !self.api_base.trim().is_empty(),
            BackendChoice::OpenAiCompatible | BackendChoice::AnthropicCompatible => {
                !self.api_base.trim().is_empty() && !self.model.trim().is_empty()
            }
            BackendChoice::CustomCli => !self.command_template.trim().is_empty(),
            _ => {
                !self.api_key.trim().is_empty()
                    || self
                        .resolved_kind()
                        .is_some_and(|kind| std::env::var(key_env_for(&kind)).is_ok())
            }
        }
    }

    /// Short status label for the sidebar, e.g. `"claude · claude-sonnet-4-6"`.
    /// `None` when no backend can be resolved or no key is available.
    pub fn status_label(&self) -> Option<String> {
        if !self.has_key() {
            return None;
        }
        let label = match self.backend {
            BackendChoice::Auto => self.resolved_kind().as_ref().map(backend_label)?,
            BackendChoice::Claude => "claude",
            BackendChoice::OpenAi => "openai",
            BackendChoice::Codex => "codex account",
            BackendChoice::ClaudeCode => "claude account",
            BackendChoice::GoogleVertex => "vertex",
            BackendChoice::OpenAiCompatible => "custom openai",
            BackendChoice::AnthropicCompatible => "custom anthropic",
            BackendChoice::CustomCli => "custom cli",
        };
        let model = Some(self.model.trim())
            .filter(|m| !m.is_empty())
            .unwrap_or("default model");
        Some(format!("{label} · {model}"))
    }

    pub fn llm_profile(&self) -> anyhow::Result<LlmProfile> {
        let backend = match self.backend {
            BackendChoice::Auto => match self.resolved_kind() {
                Some(BackendKind::Claude) => BackendChoice::Claude,
                Some(BackendKind::OpenAI) => BackendChoice::OpenAi,
                None => anyhow::bail!(
                    "no compiler configured — choose a connection under Settings or set an API-key environment variable"
                ),
            },
            other => other,
        };
        let value_or = |value: &str, fallback: &str| {
            if value.trim().is_empty() {
                fallback.to_owned()
            } else {
                value.trim().to_owned()
            }
        };
        let (protocol, model, base_url, auth, name) = match backend {
            BackendChoice::Claude => (
                LlmProtocol::AnthropicMessages,
                value_or(&self.model, "claude-sonnet-4-6"),
                value_or(&self.api_base, "https://api.anthropic.com/v1"),
                LlmAuth::Auto,
                "Anthropic API",
            ),
            BackendChoice::OpenAi => (
                LlmProtocol::OpenAiChat,
                value_or(&self.model, "gpt-4o"),
                value_or(&self.api_base, "https://api.openai.com/v1"),
                LlmAuth::Auto,
                "OpenAI API",
            ),
            BackendChoice::Codex => (
                LlmProtocol::CodexCli,
                // Empty means "let the codex CLI's own config pick the
                // model" — forcing a guessed default here can select a
                // model the user's account/plan doesn't support.
                self.model.trim().to_owned(),
                String::new(),
                LlmAuth::None,
                "Codex account",
            ),
            BackendChoice::ClaudeCode => (
                LlmProtocol::ClaudeCli,
                // Same reasoning as the Codex branch above.
                self.model.trim().to_owned(),
                String::new(),
                LlmAuth::None,
                "Claude account",
            ),
            BackendChoice::GoogleVertex => (
                LlmProtocol::GoogleVertex,
                value_or(&self.model, "gemini-2.0-flash"),
                self.api_base.trim().to_owned(),
                // Identity-based: an explicit key is used as the bearer
                // token, otherwise the gcloud CLI / GCE metadata server.
                LlmAuth::GcloudIdentity,
                "Google Vertex AI",
            ),
            BackendChoice::OpenAiCompatible => (
                LlmProtocol::OpenAiChat,
                value_or(&self.model, "local-model"),
                self.api_base.trim().to_owned(),
                if self.api_key.trim().is_empty() {
                    LlmAuth::None
                } else {
                    LlmAuth::Bearer
                },
                "OpenAI-compatible endpoint",
            ),
            BackendChoice::AnthropicCompatible => (
                LlmProtocol::AnthropicMessages,
                value_or(&self.model, "claude"),
                self.api_base.trim().to_owned(),
                if self.api_key.trim().is_empty() {
                    LlmAuth::None
                } else {
                    LlmAuth::AnthropicKey
                },
                "Anthropic-compatible endpoint",
            ),
            BackendChoice::CustomCli => (
                LlmProtocol::CustomCli,
                self.model.trim().to_owned(),
                String::new(),
                LlmAuth::None,
                "Custom CLI",
            ),
            BackendChoice::Auto => unreachable!(),
        };
        let executable = compatible_executable(backend, &self.executable);
        let profile = LlmProfile {
            id: "active".to_owned(),
            name: name.to_owned(),
            protocol,
            model,
            base_url,
            api_key: self.api_key.trim().to_owned(),
            auth,
            headers: Default::default(),
            executable,
            command_template: self.command_template.trim().to_owned(),
            max_tokens: Some(self.effective_max_tokens()),
            temperature: Some(0.0),
            timeout_secs: COMPILER_REQUEST_TIMEOUT_SECS,
            codex_sandbox_mode: self.codex_sandbox_mode,
        };
        profile.validate().map_err(anyhow::Error::msg)?;
        Ok(profile)
    }
}

fn uses_executable_override(backend: BackendChoice) -> bool {
    matches!(
        backend,
        BackendChoice::Codex | BackendChoice::ClaudeCode | BackendChoice::GoogleVertex
    )
}

/// Protect settings written by older versions, where the single executable
/// field survived switching between the Codex and Claude account backends.
/// Only reject an unambiguous cross-provider binary name; arbitrary wrapper
/// names remain supported.
fn compatible_executable(backend: BackendChoice, configured: &str) -> String {
    let configured = configured.trim();
    let file_name = std::path::Path::new(configured)
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mismatched = matches!(backend, BackendChoice::Codex) && file_name == "claude"
        || matches!(backend, BackendChoice::ClaudeCode) && file_name == "codex";
    if mismatched {
        String::new()
    } else {
        configured.to_owned()
    }
}

pub(crate) fn ensure_agent_call_allowed(plan: &Plan, settings: &AppSettings) -> anyhow::Result<()> {
    if plan
        .steps
        .iter()
        .any(|step| step.step_type() == StepType::AgentCall)
        && !settings.supports_agent_call()
    {
        anyhow::bail!(
            "this plan contains experimental AGENT_CALL steps, but this system is not configured to run them — enable Experimental agent steps in Settings and select OpenAI account, Claude account, or an explicitly agent-shaped Custom CLI"
        );
    }
    Ok(())
}

fn key_env_for(kind: &BackendKind) -> &'static str {
    match kind {
        BackendKind::Claude => ANTHROPIC_KEY_ENV,
        BackendKind::OpenAI => OPENAI_KEY_ENV,
    }
}

/// Resolve the persisted connection once for all `PROMPT_CALL` steps. Keep a
/// configuration error so execution cannot silently fall back to another
/// provider when a custom profile is incomplete.
pub(crate) fn llm_keys_from(settings: &AppSettings) -> executor::LlmKeys {
    match settings.llm_profile() {
        Ok(profile) => executor::LlmKeys {
            profile: Some(profile),
            ..executor::LlmKeys::default()
        },
        Err(error) => executor::LlmKeys {
            profile_error: Some(error.to_string()),
            ..executor::LlmKeys::default()
        },
    }
}

// ─── Commands (UI → engine) ───────────────────────────────────────────────────

#[derive(Debug)]
pub enum EngineCommand {
    /// Initial load: seed catalog, report backend, list plans + tools.
    Bootstrap,
    Compile {
        intent: String,
    },
    /// REFINE phase: assess how complete an intent is before compiling.
    /// `conversation` is the whole clarification history (the intent is its
    /// first user turn) — the engine stays stateless across turns.
    AssessIntent {
        intent: String,
        conversation: Vec<compiler::SpecTurn>,
    },
    /// DESIGN phase: produce (or, with `previous_design` + `feedback`,
    /// revise) a reviewable solution design for a refined spec.
    GenerateDesign {
        spec: compiler::SpecDraft,
        conversation: Vec<compiler::SpecTurn>,
        previous_design: Option<Box<compiler::SolutionDesign>>,
        feedback: Option<String>,
    },
    /// Compile a plan from a refined spec (and optionally an approved
    /// design). The result is immediately usable.
    CompileFromSpec {
        intent: String,
        spec: compiler::SpecDraft,
        design: Option<Box<compiler::SolutionDesign>>,
        conversation: Vec<compiler::SpecTurn>,
    },
    EditPlan {
        plan_ref: String,
        instruction: String,
    },
    RunPlan {
        plan_ref: String,
        inputs: indexmap::IndexMap<String, serde_json::Value>,
    },
    ShowPlan {
        plan_ref: String,
    },
    ListPlans,
    ListRuns,
    InspectRun {
        run_id: String,
    },
    /// Load a run for agent-mode inspection without opening a chat session.
    InspectRunReadOnly {
        run_id: String,
    },
    Repair {
        run_id: String,
    },
    /// Approve and apply a pending patch in one step.
    ApplyPatch {
        patch_id: String,
    },
    /// Re-execute a failed run against the (possibly just-patched) current
    /// plan version: only the originally failed step and its true
    /// dependents run again, everything already succeeded is left alone.
    /// `plan_id` is normally `run.plan_id` — callers that already have both
    /// in hand (e.g. a "Resume" action on an inspected run) can pass it
    /// straight through without an extra lookup.
    ResumeRun {
        plan_id: String,
        run_id: String,
        /// Replacement values submitted while resuming a repaired failed
        /// run. The executor validates which values are safe to change.
        inputs: indexmap::IndexMap<String, serde_json::Value>,
    },
    RejectPatch {
        patch_id: String,
        reason: Option<String>,
    },
    /// Abort an in-progress run: drops its execution and persists it as
    /// `RunStatus::Cancelled`. No-op if the run has already finished.
    AbortRun {
        run_id: String,
    },
    ListTools,
    ListPatches,
    /// Insert or update a catalog entry and persist the catalog.
    SaveTool {
        entry: Box<ToolEntry>,
    },
    /// Atomically replace an existing catalog entry under a new name.
    RenameTool {
        old_name: String,
        entry: Box<ToolEntry>,
    },
    DeleteTool {
        name: String,
    },
    /// "Describe what you need" — ask the compiler backend to invent a
    /// starting `ToolEntry` from a free-text description, so the user can
    /// review/adjust it in the manual editor rather than filling in raw
    /// fields from scratch.
    SynthesizeTool {
        description: String,
    },
    /// Persist compiler settings and re-announce the backend status.
    SaveSettings {
        settings: Box<AppSettings>,
    },
    ListSchedules,
    /// Create a schedule. `expression` is 5-field crontab syntax OR natural
    /// language ("every morning at 8") — the compiler converts the latter.
    SaveSchedule {
        plan_ref: String,
        expression: String,
        inputs: indexmap::IndexMap<String, serde_json::Value>,
    },
    DeleteSchedule {
        id: String,
    },
    /// Flip a schedule between enabled and paused.
    ToggleSchedule {
        id: String,
    },
    /// Delete a plan (all versions) and any schedules pointing at it.
    DeletePlan {
        plan_id: String,
    },
    /// Export a plan and its tool references to a bundle file on disk.
    ExportPlan {
        plan_ref: String,
        dest_path: PathBuf,
    },
    /// Import a plan bundle: missing tools are synthesized by the compiler,
    /// existing local tools of the same name are left untouched.
    ImportPlan {
        path: PathBuf,
    },
    /// Import with an explicit same-name collision decision from the desktop
    /// dialog. The plain command above remains safe-by-default for callers
    /// that do not present a choice.
    ImportPlanWithPolicy {
        path: PathBuf,
        conflict_policy: ImportConflictPolicy,
    },
    /// Collect an anonymized support report (host facts, plan, and — when
    /// given — a run's timeline), save it under the data dir, and build a
    /// prefilled GitHub issue URL.
    CreateSupportTicket {
        run_id: Option<String>,
        plan_ref: Option<String>,
    },
    /// Check GitHub releases for a newer version. Fails (and stays) silent on
    /// any network/HTTP error — this is a best-effort, purely informational
    /// check, never surfaced as a chat error.
    CheckForUpdates,
    /// Probe whether Codex's OS sandbox can initialize on this host, using
    /// the draft executable path (which may not be saved yet).
    TestCodexSandbox {
        executable: String,
    },
    /// Query the secure-vault connection state for a remote MCP endpoint.
    CheckMcpOAuthStatus {
        tool_name: String,
        endpoint: String,
        client_id: Option<String>,
    },
    /// Start an OAuth flow on an ephemeral loopback callback listener.
    BeginMcpOAuth {
        tool_name: String,
        endpoint: String,
        client_id: Option<String>,
    },
    /// Stop an in-flight authorization and close its callback listener.
    CancelMcpOAuth {
        tool_name: String,
    },
    /// Clear the endpoint's credentials from the OS vault.
    DisconnectMcpOAuth {
        tool_name: String,
        endpoint: String,
        client_id: Option<String>,
    },
    /// Connect to an MCP server (local stdio or remote HTTP) and enumerate
    /// the tools it currently advertises, for a bulk-import checklist.
    ListMcpServerTools {
        transport: McpTransport,
    },
    /// Add or update many catalog entries in a single mutation, emitting one
    /// `Catalog` event instead of one per entry. Used by MCP bulk import.
    BulkSaveTools {
        entries: Vec<ToolEntry>,
    },
    /// Answer a plain (non-slash) chat message from context instead of
    /// acting on it. `plan_id` scopes the answer to the chat's attached
    /// plan, when there is one. See `EngineEvent::InsightAnswer`.
    AnswerInsight {
        question: String,
        plan_id: Option<String>,
    },
}

/// Commands backed by an agent/compiler call that must be visible in the
/// shared activity inspector. Keeping this classification beside the command
/// boundary prevents individual chat widgets from inventing lifecycle state.
fn activity_kind(command: &EngineCommand) -> Option<ActivityKind> {
    match command {
        EngineCommand::Compile { .. } | EngineCommand::CompileFromSpec { .. } => {
            Some(ActivityKind::Compile)
        }
        EngineCommand::EditPlan { .. } => Some(ActivityKind::Edit),
        EngineCommand::Repair { .. } => Some(ActivityKind::Repair),
        _ => None,
    }
}

#[derive(Debug)]
struct EngineCommandTrace {
    command_kind: &'static str,
    run_id: Option<String>,
    plan_id: Option<String>,
    schedule_id: Option<String>,
}

impl EngineCommandTrace {
    fn from_command(command: &EngineCommand) -> Self {
        let command_kind = match command {
            EngineCommand::Bootstrap => "bootstrap",
            EngineCommand::Compile { .. } => "compile",
            EngineCommand::AssessIntent { .. } => "assess_intent",
            EngineCommand::GenerateDesign { .. } => "generate_design",
            EngineCommand::CompileFromSpec { .. } => "compile_from_spec",
            EngineCommand::EditPlan { .. } => "edit_plan",
            EngineCommand::RunPlan { .. } => "run_plan",
            EngineCommand::ShowPlan { .. } => "show_plan",
            EngineCommand::ListPlans => "list_plans",
            EngineCommand::ListRuns => "list_runs",
            EngineCommand::InspectRun { .. } => "inspect_run",
            EngineCommand::InspectRunReadOnly { .. } => "inspect_run_read_only",
            EngineCommand::Repair { .. } => "repair_run",
            EngineCommand::ApplyPatch { .. } => "apply_patch",
            EngineCommand::ResumeRun { .. } => "resume_run",
            EngineCommand::RejectPatch { .. } => "reject_patch",
            EngineCommand::AbortRun { .. } => "abort_run",
            EngineCommand::ListTools => "list_tools",
            EngineCommand::ListPatches => "list_patches",
            EngineCommand::SaveTool { .. } => "save_tool",
            EngineCommand::RenameTool { .. } => "rename_tool",
            EngineCommand::DeleteTool { .. } => "delete_tool",
            EngineCommand::SynthesizeTool { .. } => "synthesize_tool",
            EngineCommand::SaveSettings { .. } => "save_settings",
            EngineCommand::ListSchedules => "list_schedules",
            EngineCommand::SaveSchedule { .. } => "save_schedule",
            EngineCommand::DeleteSchedule { .. } => "delete_schedule",
            EngineCommand::ToggleSchedule { .. } => "toggle_schedule",
            EngineCommand::DeletePlan { .. } => "delete_plan",
            EngineCommand::ExportPlan { .. } => "export_plan",
            EngineCommand::ImportPlan { .. } | EngineCommand::ImportPlanWithPolicy { .. } => {
                "import_plan"
            }
            EngineCommand::CreateSupportTicket { .. } => "create_support_ticket",
            EngineCommand::CheckForUpdates => "check_for_updates",
            EngineCommand::TestCodexSandbox { .. } => "test_codex_sandbox",
            EngineCommand::CheckMcpOAuthStatus { .. } => "mcp_oauth_status",
            EngineCommand::BeginMcpOAuth { .. } => "mcp_oauth_begin",
            EngineCommand::CancelMcpOAuth { .. } => "mcp_oauth_cancel",
            EngineCommand::DisconnectMcpOAuth { .. } => "mcp_oauth_disconnect",
            EngineCommand::ListMcpServerTools { .. } => "list_mcp_server_tools",
            EngineCommand::BulkSaveTools { .. } => "bulk_save_tools",
            EngineCommand::AnswerInsight { .. } => "answer_insight",
        };
        let run_id = match command {
            EngineCommand::InspectRun { run_id }
            | EngineCommand::InspectRunReadOnly { run_id }
            | EngineCommand::Repair { run_id }
            | EngineCommand::AbortRun { run_id }
            | EngineCommand::ResumeRun { run_id, .. } => Some(run_id.clone()),
            _ => None,
        };
        let plan_id = match command {
            EngineCommand::ResumeRun { plan_id, .. } | EngineCommand::DeletePlan { plan_id } => {
                Some(plan_id.clone())
            }
            EngineCommand::AnswerInsight { plan_id, .. } => plan_id.clone(),
            _ => None,
        };
        let schedule_id = match command {
            EngineCommand::DeleteSchedule { id } | EngineCommand::ToggleSchedule { id } => {
                Some(id.clone())
            }
            _ => None,
        };
        Self {
            command_kind,
            run_id,
            plan_id,
            schedule_id,
        }
    }
}

// ─── Events (engine → UI) ─────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct PlanListItem {
    pub id: String,
    pub name: String,
    pub version: u32,
    pub intent: Option<String>,
    pub inputs: Vec<crate::plan::types::PlanInput>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub status: PlanStatus,
}

/// Which surface started a run — interactively from a plan chat, by an
/// external agent through the local MCP server, or by the cron scheduler.
/// Surfaced as a badge in the Runs view so agent-triggered work (the runs
/// the user did *not* watch happen) is recognizable at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunSource {
    Chat,
    Mcp,
    Schedule,
}

impl RunSource {
    /// Badge label — deliberately short, these render as pill tags.
    pub fn label(self) -> &'static str {
        match self {
            RunSource::Chat => "Chat",
            RunSource::Mcp => "MCP",
            RunSource::Schedule => "Schedule",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RunListItem {
    pub id: String,
    pub plan_id: String,
    pub plan_name: String,
    pub status: crate::executor::RunStatus,
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// When the run finished, if it has — lets the Runs view show wall time
    /// for completed runs. `None` for still-running and legacy records.
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Origin of the run, once known. Older records written before the source
    /// field existed carry `None` — the UI renders those as "—".
    pub source: Option<RunSource>,
}

impl From<crate::storage::runs::RunSource> for RunSource {
    fn from(source: crate::storage::runs::RunSource) -> Self {
        match source {
            crate::storage::runs::RunSource::Chat => RunSource::Chat,
            crate::storage::runs::RunSource::Mcp => RunSource::Mcp,
            crate::storage::runs::RunSource::Schedule => RunSource::Schedule,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PatchListItem {
    pub id: String,
    pub plan_name: String,
    pub failing_step_id: String,
    pub status: crate::storage::patches::PatchStatus,
    pub proposed_at: chrono::DateTime<chrono::Utc>,
}

/// A concrete follow-up action the insight assistant may propose after
/// answering a plain-text question — rendered as a single button in chat.
/// Confirming it re-parses `command` through the ordinary slash-command
/// parser, so a click can never do anything a typed command could not.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SuggestedAction {
    pub label: String,
    pub command: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScheduleItem {
    pub id: String,
    pub plan_id: String,
    pub plan_name: String,
    /// The normalised cron expression (6-field).
    pub cron: String,
    pub enabled: bool,
    pub inputs: indexmap::IndexMap<String, serde_json::Value>,
    /// Next local-time occurrence, pre-formatted for display.
    pub next_run_display: Option<String>,
}

#[derive(Debug)]
pub enum EngineEvent {
    Ready {
        data_dir: String,
        backend: Option<String>,
        /// One-line host summary (OS, interpreters, runners).
        environment: String,
    },
    /// Plain assistant chat text.
    Assistant(String),
    /// An operation failed — rendered as an error bubble.
    Failure(String),
    CompileStarted {
        intent: String,
    },
    /// The assessment backend has been invoked for the REFINE phase.
    AssessStarted {
        intent: String,
    },
    /// The compiler's judgement of how design-ready the spec is.
    AssessmentReady {
        assessment: Box<compiler::IntentAssessment>,
    },
    /// The design backend has been invoked for the DESIGN phase.
    DesignStarted,
    /// A (re)generated solution design for review.
    DesignReady {
        design: Box<compiler::SolutionDesign>,
    },
    EditStarted {
        plan_name: String,
        instruction: String,
    },
    /// The insight backend has been invoked for a plain-text question.
    InsightStarted,
    /// An answer to a plain-text question, with an optional one-click
    /// follow-up action.
    InsightAnswer {
        answer: String,
        suggested_action: Option<SuggestedAction>,
    },
    /// A live console for the compile/edit announced by the immediately
    /// preceding `CompileStarted`/`EditStarted` event. The UI
    /// reads the shared buffer each frame while lines keep arriving; the
    /// console stays readable after the operation closes it, and its log
    /// file survives on disk either way.
    CompileConsole {
        console: CompileConsole,
    },
    /// A freshly compiled (validated, normalized, saved) plan.
    PlanCompiled {
        plan: Box<Plan>,
    },
    /// A pending, LLM-compiled edit to an existing plan, awaiting approval.
    EditProposed {
        edit: Box<PlanEdit>,
    },
    PlanDeleted {
        plan_id: String,
        message: String,
    },
    /// The repair backend has been invoked for a failed run; a `PatchProposed`
    /// or `Failure` event follows once the (potentially slow) LLM call
    /// returns.
    RepairStarted {
        run_id: String,
        failing_step_id: String,
    },
    /// An existing plan loaded for display.
    PlanLoaded {
        plan: Box<Plan>,
    },
    PlanList(Vec<PlanListItem>),
    RunStarted {
        run_id: String,
        plan: Box<Plan>,
        inputs: indexmap::IndexMap<String, serde_json::Value>,
    },
    StepProgress(Box<ProgressEvent>),
    /// Terminal state of a run (success or failure).
    RunFinished {
        run: Box<Run>,
    },
    RunList(Vec<RunListItem>),
    RunInspected {
        run: Box<Run>,
        plan: Box<Plan>,
    },
    /// Read-only detail for agent mode; does not imply chat navigation.
    RunReadOnlyInspected {
        run: Box<Run>,
    },
    /// A HUMAN_INTERACTION step is waiting; answer via `request.respond`.
    HumanNeeded {
        run_id: String,
        request: HumanRequest,
    },
    PatchProposed {
        patch: Box<Patch>,
    },
    /// Repair diagnosed the failure as a world-state problem: the plan is
    /// fine, the environment violated its assumptions. Carries the human
    /// remediation actions; the run resumes at the same plan version once the
    /// world is fixed.
    WorldFixProposed {
        fix: Box<WorldFix>,
    },
    /// A patch was applied or rejected.
    PatchResolved {
        patch_id: String,
        message: String,
    },
    /// A plan-edit proposal was applied or rejected.
    EditResolved {
        edit_id: String,
        message: String,
    },
    Catalog(Vec<ToolEntry>),
    /// A tool definition generated from a free-text "describe what you
    /// need" request, ready to be reviewed in the manual editor.
    ToolSynthesized {
        entry: Box<ToolEntry>,
    },
    /// The "describe what you need" request failed (e.g. no compiler
    /// configured, or the model's response was unusable).
    ToolSynthesisFailed {
        message: String,
    },
    PatchList(Vec<PatchListItem>),
    /// A schedule was persisted successfully. The message is also shown in chat.
    ScheduleSaved {
        plan_id: String,
        message: String,
    },
    /// Schedule creation failed after the user submitted the form.
    ScheduleSaveFailed {
        plan_id: String,
        message: String,
    },
    ScheduleList(Vec<ScheduleItem>),
    /// Current persisted settings (sent at bootstrap and after every save).
    Settings(AppSettings),
    /// An anonymized support report was collected and saved; `issue_url`
    /// opens GitHub's new-issue form prefilled with it.
    SupportTicketReady {
        issue_url: String,
        report_path: String,
        message: String,
    },
    /// A newer release than the running build is available on GitHub.
    UpdateAvailable {
        /// e.g. `"0.2.0"` (no leading `v`).
        version: String,
        /// Release page to open in the browser.
        url: String,
    },
    /// Result of an `EngineCommand::TestCodexSandbox` probe: `Ok` when
    /// Codex's OS sandbox initialized cleanly, `Err` with a remediation hint
    /// otherwise.
    CodexSandboxTestResult(Result<(), String>),
    /// OAuth connection state for one outbound remote MCP tool.
    McpOAuthStatus {
        tool_name: String,
        status: OAuthConnectionStatus,
    },
    /// A user must complete authorization at this URL. It contains no tokens.
    McpAuthorizationStarted {
        tool_name: String,
        authorization_url: String,
    },
    /// Authorization was completed, cancelled, or failed. Error messages are
    /// deliberately sanitised by the OAuth boundary and callback parser.
    McpAuthorizationFinished {
        tool_name: String,
        result: Result<OAuthConnectionStatus, String>,
    },
    /// Tools discovered from a server's `tools/list` for bulk import, or the
    /// reason discovery failed.
    McpServerToolsListed {
        result: Result<Vec<McpDiscoveredTool>, String>,
    },
    /// The cron scheduler is being run by another live instance (another
    /// desktop window or a `--headless` process holds the lock), so this
    /// instance did not start its own loop. Informational only.
    SchedulerUnavailable {
        /// PID of the live holder, if the lock file could be parsed.
        holder_pid: Option<u32>,
    },
}

/// Every event remembers the chat that initiated its command. Global and
/// externally triggered work (scheduler/MCP) has no origin and is routed by
/// plan/run identity in the app shell.
#[derive(Debug)]
pub struct RoutedEngineEvent {
    pub session_id: Option<String>,
    pub event: EngineEvent,
}

#[derive(Debug)]
struct EngineRequest {
    session_id: Option<String>,
    command: EngineCommand,
}

// ─── Handle ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct EngineHandle {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<EngineRequest>,
    session_id: Option<String>,
}

impl EngineHandle {
    pub fn send(&self, command: EngineCommand) {
        // A closed engine means the app is shutting down; nothing to do.
        let _ = self.cmd_tx.send(EngineRequest {
            session_id: self.session_id.clone(),
            command,
        });
    }

    pub fn scoped(&self, session_id: impl Into<String>) -> Self {
        Self {
            cmd_tx: self.cmd_tx.clone(),
            session_id: Some(session_id.into()),
        }
    }

    pub fn send_from(&self, session_id: impl Into<String>, command: EngineCommand) {
        let _ = self.cmd_tx.send(EngineRequest {
            session_id: Some(session_id.into()),
            command,
        });
    }
}

/// Spawn the engine thread. Returns the command handle and the event stream.
pub fn spawn(
    egui_ctx: egui::Context,
    paths: DataPaths,
) -> (EngineHandle, std::sync::mpsc::Receiver<RoutedEngineEvent>) {
    spawn_with_activities(egui_ctx, paths, ActivityRegistry::default())
}

/// Desktop-engine constructor that shares live activity state with the local
/// MCP server. The two-argument [`spawn`] remains the headless/test-friendly
/// compatibility boundary.
pub fn spawn_with_activities(
    egui_ctx: egui::Context,
    paths: DataPaths,
    activities: ActivityRegistry,
) -> (EngineHandle, std::sync::mpsc::Receiver<RoutedEngineEvent>) {
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<EngineRequest>();
    let (evt_tx, evt_rx) = std::sync::mpsc::channel::<RoutedEngineEvent>();
    let oauth_cancellations = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let run_cancellations = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    std::thread::Builder::new()
        .name(ENGINE_THREAD_NAME.to_owned())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("failed to build tokio runtime for engine");

            let repaint: RepaintHook = {
                let ctx = egui_ctx.clone();
                Arc::new(move || ctx.request_repaint())
            };

            runtime.block_on(async move {
                // Cron scheduler runs for the app's lifetime alongside the
                // command loop — but only if this process wins the scheduler
                // lock. A second instance leaves the loop to the holder so
                // schedules never double-fire. The guard lives for the whole
                // block, releasing the lock on clean shutdown.
                let _scheduler_lock = match SchedulerLock::acquire(&paths.scheduler_lock_path) {
                    Ok(LockAcquisition::Acquired(lock)) => {
                        tokio::spawn(scheduler_loop(EngineEnv {
                            paths: paths.clone(),
                            evt_tx: evt_tx.clone(),
                            repaint: repaint.clone(),
                            session_id: None,
                            oauth_cancellations: Arc::clone(&oauth_cancellations),
                            run_cancellations: Arc::clone(&run_cancellations),
                            activities: activities.clone(),
                            activity_id: None,
                        }));
                        Some(lock)
                    }
                    Ok(LockAcquisition::Held { holder_pid }) => {
                        tracing::warn!(
                            ?holder_pid,
                            "another instance holds the scheduler lock; \
                             this window will not run schedules"
                        );
                        let _ = evt_tx.send(RoutedEngineEvent {
                            session_id: None,
                            event: EngineEvent::SchedulerUnavailable { holder_pid },
                        });
                        (repaint)();
                        None
                    }
                    Err(_) => {
                        tracing::error!(
                            operation = "scheduler_lock.acquire",
                            app_version = env!("CARGO_PKG_VERSION"),
                            triggered_by = "application",
                            outcome = "failure",
                            "scheduler lock acquisition failed; schedules disabled"
                        );
                        None
                    }
                };

                while let Some(request) = cmd_rx.recv().await {
                    let trace = EngineCommandTrace::from_command(&request.command);
                    let triggered_by = match &request.command {
                        EngineCommand::Bootstrap | EngineCommand::CheckForUpdates => "application",
                        _ if request.session_id.is_some() => "human_chat",
                        _ => "human_ui",
                    };
                    let env = EngineEnv {
                        paths: paths.clone(),
                        evt_tx: evt_tx.clone(),
                        repaint: repaint.clone(),
                        session_id: request.session_id,
                        oauth_cancellations: Arc::clone(&oauth_cancellations),
                        run_cancellations: Arc::clone(&run_cancellations),
                        activities: activities.clone(),
                        activity_id: None,
                    };
                    // Each command runs as its own task so a long run never
                    // blocks catalog edits or further chat commands.
                    tokio::spawn(async move {
                        let activity = activity_kind(&request.command)
                            .map(|kind| env.activities.start(ActivityOrigin::Chat, kind));
                        let mut env = env;
                        env.activity_id = activity.as_ref().map(|activity| activity.id());
                        let started = std::time::Instant::now();
                        let result = handle_command(request.command, &env).await;
                        let duration_ms = started.elapsed().as_millis() as u64;
                        if let Some(activity) = activity {
                            match &result {
                                Ok(()) => activity.succeeded(),
                                Err(error) => activity.failed(format!("{error:#}")),
                            }
                        }
                        if let Err(error) = result {
                            tracing::error!(
                                command_kind = trace.command_kind,
                                session_id = ?env.session_id,
                                run_id = ?trace.run_id,
                                plan_id = ?trace.plan_id,
                                schedule_id = ?trace.schedule_id,
                                app_version = env!("CARGO_PKG_VERSION"),
                                triggered_by,
                                duration_ms,
                                outcome = "failure",
                                "engine command completed"
                            );
                            env.emit(EngineEvent::Failure(format!("{error:#}")));
                        } else {
                            tracing::info!(
                                command_kind = trace.command_kind,
                                session_id = ?env.session_id,
                                run_id = ?trace.run_id,
                                plan_id = ?trace.plan_id,
                                schedule_id = ?trace.schedule_id,
                                app_version = env!("CARGO_PKG_VERSION"),
                                triggered_by,
                                duration_ms,
                                outcome = "success",
                                "engine command completed"
                            );
                        }
                    });
                }
            });
        })
        .expect("failed to spawn engine thread");

    (
        EngineHandle {
            cmd_tx,
            session_id: None,
        },
        evt_rx,
    )
}

// ─── Headless scheduler ─────────────────────────────────────────────────────────

const SCHEDULER_THREAD_NAME: &str = "inxm-scheduler";

/// Result of trying to start the scheduler loop headlessly.
#[derive(Debug)]
pub enum SchedulerOutcome {
    /// This process took the lock and is running the loop. `pid` is our PID
    /// (now recorded in the lock file).
    Running { pid: u32 },
    /// A live instance already holds the lock; the loop was not started here.
    Blocked { holder_pid: Option<u32> },
    /// The lock could not be taken due to an I/O error.
    Failed { error: String },
}

/// Start the cron scheduler without an egui context.
///
/// Acquires the scheduler lock and, on success, spawns a dedicated tokio
/// runtime thread that runs [`scheduler_loop`] for the process's lifetime
/// (the returned lock guard is held by that thread and released on clean
/// shutdown). Emitted engine events are returned over the receiver so a caller
/// can persist scheduled-run chat messages; when the lock is not taken the
/// receiver is `None`.
///
/// This never blocks: the outcome is decided synchronously (lock acquisition)
/// and the loop runs on its own thread.
pub fn start_scheduler_headless(
    paths: DataPaths,
) -> (
    SchedulerOutcome,
    Option<std::sync::mpsc::Receiver<RoutedEngineEvent>>,
) {
    let lock = match SchedulerLock::acquire(&paths.scheduler_lock_path) {
        Ok(LockAcquisition::Acquired(lock)) => lock,
        Ok(LockAcquisition::Held { holder_pid }) => {
            tracing::warn!(
                ?holder_pid,
                "another instance holds the scheduler lock; headless scheduler not started"
            );
            return (SchedulerOutcome::Blocked { holder_pid }, None);
        }
        Err(error) => {
            tracing::error!(
                operation = "scheduler_lock.acquire",
                app_version = env!("CARGO_PKG_VERSION"),
                triggered_by = "application",
                outcome = "failure",
                "scheduler lock acquisition failed; scheduler not started"
            );
            return (
                SchedulerOutcome::Failed {
                    error: error.to_string(),
                },
                None,
            );
        }
    };

    let (evt_tx, evt_rx) = std::sync::mpsc::channel::<RoutedEngineEvent>();
    let pid = std::process::id();

    std::thread::Builder::new()
        .name(SCHEDULER_THREAD_NAME.to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => {
                    tracing::error!(
                        operation = "scheduler_runtime.initialize",
                        app_version = env!("CARGO_PKG_VERSION"),
                        triggered_by = "application",
                        outcome = "failure",
                        "scheduler runtime initialization failed"
                    );
                    return;
                }
            };
            runtime.block_on(async move {
                // Hold the lock for the loop's lifetime; drop releases it.
                let _lock = lock;
                scheduler_loop(EngineEnv {
                    paths,
                    evt_tx,
                    repaint: no_repaint(),
                    session_id: None,
                    oauth_cancellations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                    run_cancellations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                    activities: ActivityRegistry::default(),
                    activity_id: None,
                })
                .await;
            });
        })
        .expect("failed to spawn scheduler thread");

    (SchedulerOutcome::Running { pid }, Some(evt_rx))
}

// ─── Engine environment ───────────────────────────────────────────────────────

struct EngineEnv {
    paths: DataPaths,
    evt_tx: std::sync::mpsc::Sender<RoutedEngineEvent>,
    repaint: RepaintHook,
    session_id: Option<String>,
    oauth_cancellations: Arc<tokio::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>,
    /// Per-run abort signals, keyed by run id once it is known (see
    /// `run_plan_with_timeout`). Firing the sender drops the run's
    /// execution future; the run is then persisted as `RunStatus::Cancelled`.
    run_cancellations: Arc<tokio::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>,
    activities: ActivityRegistry,
    activity_id: Option<u64>,
}

impl EngineEnv {
    fn triggered_by(&self) -> &'static str {
        if self.session_id.is_some() {
            "human_chat"
        } else {
            "human_ui"
        }
    }

    fn emit(&self, event: EngineEvent) {
        self.count_usage(&event);
        let _ = self.evt_tx.send(RoutedEngineEvent {
            session_id: self.session_id.clone(),
            event,
        });
        (self.repaint)();
    }

    /// Consent-gated usage tallies (see `crate::telemetry::usage`), taken
    /// at the one choke point every app-side lifecycle event passes
    /// through. Chat, plan cards, and the scheduler all end up here, so
    /// everything counted from this process is `Source::App`; the MCP
    /// server counts its own. `RunFinished` with a non-terminal status
    /// (waiting for a human) is deliberately not counted — the run will
    /// come through again once it actually finishes.
    fn count_usage(&self, event: &EngineEvent) {
        use crate::storage::runs::RunStatus;
        use crate::telemetry::usage::{self, Action, Source};
        let action = match event {
            EngineEvent::PlanCompiled { .. } => Some(Action::PlanCreated),
            // Only the successful application of a reviewed edit counts —
            // a rejected proposal was never actually applied to the plan.
            EngineEvent::EditResolved { message, .. } if message.starts_with("Edit applied") => {
                Some(Action::PlanEdited)
            }
            EngineEvent::RunFinished { run } => match run.status {
                RunStatus::Succeeded => Some(Action::RunSucceeded),
                RunStatus::Failed { .. } => Some(Action::RunFailed),
                _ => None,
            },
            _ => None,
        };
        if let Some(action) = action {
            usage::count(
                &self.paths.data_dir,
                &self.paths.settings_path,
                Source::App,
                action,
            );
        }
    }

    fn storage(&self) -> anyhow::Result<StorageRoot> {
        Ok(StorageRoot::open(&self.paths.data_dir)?)
    }

    /// Open a live console for a slow compiler operation and announce it to
    /// the UI. The repaint hook keeps the chat's console view
    /// current as lines stream in.
    fn attach_compile_console(&self, label: &str) -> CompileConsole {
        let console = CompileConsole::new(
            label,
            Some(&super::console::default_log_dir(&self.paths.data_dir)),
            Some(self.repaint.clone()),
        );
        if let Some(activity_id) = self.activity_id {
            self.activities.attach_console(activity_id, console.clone());
        }
        self.emit(EngineEvent::CompileConsole {
            console: console.clone(),
        });
        console
    }

    /// Give a non-compile LLM operation the same activity console without
    /// changing chat's historical compiler-console event semantics.
    fn attach_activity_console(&self, label: &str) -> CompileConsole {
        let console = CompileConsole::new(
            label,
            Some(&super::console::default_log_dir(&self.paths.data_dir)),
            Some(self.repaint.clone()),
        );
        if let Some(activity_id) = self.activity_id {
            self.activities.attach_console(activity_id, console.clone());
        }
        console
    }

    fn catalog(&self) -> anyhow::Result<ToolCatalog> {
        if self.paths.catalog_path.exists() {
            Ok(ToolCatalog::load_from_file(&self.paths.catalog_path)?)
        } else {
            Ok(ToolCatalog::default())
        }
    }
}

fn backend_label(kind: &BackendKind) -> &'static str {
    match kind {
        BackendKind::Claude => "claude",
        BackendKind::OpenAI => "openai",
    }
}

/// Build a compiler backend from the persisted LLM connection settings.
pub(crate) fn create_configured_backend(
    settings: &AppSettings,
) -> anyhow::Result<compiler::Backend> {
    let profile = settings
        .llm_profile()
        .map_err(|e| anyhow::anyhow!("invalid LLM settings: {e}"))?;
    compiler::create_profile_backend(profile)
        .map_err(|e| anyhow::anyhow!("failed to create compiler backend: {e} — check Settings"))
}

// ─── Command handlers ─────────────────────────────────────────────────────────

async fn handle_command(command: EngineCommand, env: &EngineEnv) -> anyhow::Result<()> {
    match command {
        EngineCommand::Bootstrap => bootstrap(env),
        EngineCommand::Compile { intent } => compile(env, intent).await,
        EngineCommand::AssessIntent {
            intent,
            conversation,
        } => assess_intent(env, intent, conversation).await,
        EngineCommand::GenerateDesign {
            spec,
            conversation,
            previous_design,
            feedback,
        } => {
            generate_design(
                env,
                spec,
                conversation,
                previous_design.map(|design| *design),
                feedback,
            )
            .await
        }
        EngineCommand::CompileFromSpec {
            intent,
            spec,
            design,
            conversation,
        } => {
            compile_from_spec(
                env,
                intent,
                spec,
                design.map(|design| *design),
                conversation,
            )
            .await
        }
        EngineCommand::EditPlan {
            plan_ref,
            instruction,
        } => edit_plan(env, &plan_ref, instruction).await,
        EngineCommand::RunPlan { plan_ref, inputs } => run_plan(env, &plan_ref, inputs).await,
        EngineCommand::ShowPlan { plan_ref } => {
            let storage = env.storage()?;
            let plan = resolve_plan(&storage, &plan_ref)?;
            env.emit(EngineEvent::PlanLoaded {
                plan: Box::new(plan),
            });
            Ok(())
        }
        EngineCommand::ListPlans => {
            env.emit(EngineEvent::PlanList(list_plans(env)?));
            Ok(())
        }
        EngineCommand::ListRuns => {
            env.emit(EngineEvent::RunList(list_runs(env)?));
            Ok(())
        }
        EngineCommand::InspectRun { run_id } => {
            let storage = env.storage()?;
            let run = load_run_by_prefix(&storage, &run_id)?;
            let plan = storage
                .plans()
                .load_version(&run.plan_id, run.plan_version)?;
            env.emit(EngineEvent::RunInspected {
                run: Box::new(run),
                plan: Box::new(plan),
            });
            Ok(())
        }
        EngineCommand::InspectRunReadOnly { run_id } => {
            let run = load_run_by_prefix(&env.storage()?, &run_id)?;
            env.emit(EngineEvent::RunReadOnlyInspected { run: Box::new(run) });
            Ok(())
        }
        EngineCommand::Repair { run_id } => propose_repair(env, &run_id).await,
        EngineCommand::ApplyPatch { patch_id } => apply_patch(env, &patch_id),
        EngineCommand::ResumeRun {
            plan_id,
            run_id,
            inputs,
        } => resume_run(env, &plan_id, &run_id, inputs).await,
        EngineCommand::RejectPatch { patch_id, reason } => reject_patch(env, &patch_id, reason),
        EngineCommand::AbortRun { run_id } => abort_run(env, &run_id).await,
        EngineCommand::ListTools => {
            let catalog = env.catalog()?;
            env.emit(EngineEvent::Catalog(catalog.all().cloned().collect()));
            Ok(())
        }
        EngineCommand::SaveTool { entry } => save_tool(env, *entry),
        EngineCommand::RenameTool { old_name, entry } => rename_tool(env, &old_name, *entry),
        EngineCommand::DeleteTool { name } => delete_tool(env, &name),
        EngineCommand::SynthesizeTool { description } => synthesize_tool(env, description).await,
        EngineCommand::SaveSettings { settings } => save_settings(env, *settings),
        EngineCommand::ListPatches => {
            env.emit(EngineEvent::PatchList(list_patches(env)?));
            Ok(())
        }
        EngineCommand::ListSchedules => {
            env.emit(EngineEvent::ScheduleList(list_schedules(env)?));
            Ok(())
        }
        EngineCommand::SaveSchedule {
            plan_ref,
            expression,
            inputs,
        } => {
            if let Err(error) = save_schedule(env, &plan_ref, &expression, inputs).await {
                env.emit(EngineEvent::ScheduleSaveFailed {
                    plan_id: plan_ref,
                    message: format_error_chain(&error),
                });
            }
            Ok(())
        }
        EngineCommand::DeletePlan { plan_id } => delete_plan(env, &plan_id),
        EngineCommand::ExportPlan {
            plan_ref,
            dest_path,
        } => export_plan(env, &plan_ref, &dest_path),
        EngineCommand::ImportPlan { path } => {
            import_plan(env, &path, ImportConflictPolicy::Reject).await
        }
        EngineCommand::ImportPlanWithPolicy {
            path,
            conflict_policy,
        } => import_plan(env, &path, conflict_policy).await,
        EngineCommand::DeleteSchedule { id } => {
            env.paths
                .mutations
                .run_named("schedule.delete", env.triggered_by(), || {
                    let remaining: Vec<schedule_store::Schedule> =
                        schedule_store::load(&env.paths.schedules_path)?
                            .into_iter()
                            .filter(|s| s.id != id)
                            .collect();
                    schedule_store::save(&env.paths.schedules_path, &remaining)?;
                    Ok(())
                })?;
            env.emit(EngineEvent::ScheduleList(list_schedules(env)?));
            Ok(())
        }
        EngineCommand::ToggleSchedule { id } => {
            env.paths
                .mutations
                .run_named("schedule.toggle", env.triggered_by(), || {
                    let toggled: Vec<schedule_store::Schedule> =
                        schedule_store::load(&env.paths.schedules_path)?
                            .into_iter()
                            .map(|s| match s.id == id {
                                true => schedule_store::Schedule {
                                    enabled: !s.enabled,
                                    ..s
                                },
                                false => s,
                            })
                            .collect();
                    schedule_store::save(&env.paths.schedules_path, &toggled)?;
                    Ok(())
                })?;
            env.emit(EngineEvent::ScheduleList(list_schedules(env)?));
            Ok(())
        }
        EngineCommand::CreateSupportTicket { run_id, plan_ref } => {
            create_support_ticket(env, run_id.as_deref(), plan_ref.as_deref())
        }
        EngineCommand::CheckForUpdates => {
            check_for_updates(env).await;
            Ok(())
        }
        EngineCommand::TestCodexSandbox { executable } => {
            let result = crate::llm::test_codex_sandbox(&executable).await;
            env.emit(EngineEvent::CodexSandboxTestResult(result));
            Ok(())
        }
        EngineCommand::CheckMcpOAuthStatus {
            tool_name,
            endpoint,
            client_id,
        } => check_mcp_oauth_status(env, tool_name, endpoint, client_id).await,
        EngineCommand::BeginMcpOAuth {
            tool_name,
            endpoint,
            client_id,
        } => begin_mcp_oauth(env, tool_name, endpoint, client_id).await,
        EngineCommand::CancelMcpOAuth { tool_name } => {
            if let Some(cancel) = env.oauth_cancellations.lock().await.remove(&tool_name) {
                let _ = cancel.send(());
            }
            Ok(())
        }
        EngineCommand::DisconnectMcpOAuth {
            tool_name,
            endpoint,
            client_id,
        } => disconnect_mcp_oauth(env, tool_name, endpoint, client_id).await,
        EngineCommand::ListMcpServerTools { transport } => {
            list_mcp_server_tools(env, transport).await
        }
        EngineCommand::BulkSaveTools { entries } => bulk_save_tools(env, entries),
        EngineCommand::AnswerInsight { question, plan_id } => {
            answer_insight(env, question, plan_id).await
        }
    }
}

async fn check_mcp_oauth_status(
    env: &EngineEnv,
    tool_name: String,
    endpoint: String,
    client_id: Option<String>,
) -> anyhow::Result<()> {
    let facade = McpOAuthFacade::production(&endpoint, client_id)
        .await
        .map_err(sanitized_oauth_error)?;
    let status = facade
        .connection_status()
        .await
        .map_err(sanitized_oauth_error)?;
    env.emit(EngineEvent::McpOAuthStatus { tool_name, status });
    Ok(())
}

async fn begin_mcp_oauth(
    env: &EngineEnv,
    tool_name: String,
    endpoint: String,
    client_id: Option<String>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|_| anyhow::anyhow!("could not start the local authorization callback"))?;
    let port = listener
        .local_addr()
        .map_err(|_| anyhow::anyhow!("could not start the local authorization callback"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}{MCP_OAUTH_CALLBACK_PATH}");
    let facade = McpOAuthFacade::production(&endpoint, client_id)
        .await
        .map_err(sanitized_oauth_error)?;
    let authorization = facade
        .begin_authorization(&redirect_uri)
        .await
        .map_err(sanitized_oauth_error)?;
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    env.oauth_cancellations
        .lock()
        .await
        .insert(tool_name.clone(), cancel_tx);
    env.emit(EngineEvent::McpAuthorizationStarted {
        tool_name: tool_name.clone(),
        authorization_url: authorization.authorization_url,
    });
    let outcome = tokio::select! {
        _ = cancel_rx => Err("authorization was cancelled".to_owned()),
        callback = wait_for_oauth_callback(listener) => match callback {
            Ok(callback) => complete_callback(&facade, &authorization.state, callback).await,
            Err(message) => Err(message),
        },
        _ = tokio::time::sleep(MCP_OAUTH_CALLBACK_TIMEOUT) => Err("authorization timed out; try connecting again".to_owned()),
    };
    env.oauth_cancellations.lock().await.remove(&tool_name);
    let result = match outcome {
        Ok(()) => facade
            .connection_status()
            .await
            .map_err(sanitized_oauth_error)
            .map_err(|error| error.to_string()),
        Err(message) => Err(message),
    };
    env.emit(EngineEvent::McpAuthorizationFinished { tool_name, result });
    Ok(())
}

async fn disconnect_mcp_oauth(
    env: &EngineEnv,
    tool_name: String,
    endpoint: String,
    client_id: Option<String>,
) -> anyhow::Result<()> {
    let facade = McpOAuthFacade::production(&endpoint, client_id)
        .await
        .map_err(sanitized_oauth_error)?;
    facade.disconnect().await.map_err(sanitized_oauth_error)?;
    env.emit(EngineEvent::McpOAuthStatus {
        tool_name,
        status: OAuthConnectionStatus::Disconnected,
    });
    Ok(())
}

fn sanitized_oauth_error(error: crate::tools::oauth::McpOAuthError) -> anyhow::Error {
    anyhow::anyhow!(error.to_string())
}

/// Connect to an MCP server and enumerate its tools for bulk import. Always
/// succeeds at the command level — connection failures are reported through
/// the `result` field of the emitted event so the UI can show them inline,
/// the same way `McpAuthorizationFinished` reports OAuth failures.
async fn list_mcp_server_tools(env: &EngineEnv, transport: McpTransport) -> anyhow::Result<()> {
    let result = crate::tools::adapters::mcp::list_tools(&transport, None)
        .await
        .map_err(|error| error.to_string());
    env.emit(EngineEvent::McpServerToolsListed { result });
    Ok(())
}

fn bulk_save_tools(env: &EngineEnv, entries: Vec<ToolEntry>) -> anyhow::Result<()> {
    let updated =
        env.paths
            .mutations
            .run_named("catalog.bulk_save_tools", env.triggered_by(), || {
                let catalog = env.catalog()?;
                let mut merged: Vec<ToolEntry> = catalog.all().cloned().collect();
                for entry in entries {
                    if entry.name.trim().is_empty() {
                        anyhow::bail!("tool name must not be empty");
                    }
                    match merged.iter_mut().find(|tool| tool.name == entry.name) {
                        Some(existing) => *existing = entry,
                        None => merged.push(entry),
                    }
                }
                let updated = ToolCatalog::new(merged);
                updated.save_to_file(&env.paths.catalog_path)?;
                Ok(updated)
            })?;
    env.emit(EngineEvent::Catalog(updated.all().cloned().collect()));
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum OAuthCallback {
    Code { code: String, state: String },
    Denied,
}

async fn wait_for_oauth_callback(listener: TcpListener) -> Result<OAuthCallback, String> {
    let (mut stream, _) = listener
        .accept()
        .await
        .map_err(|_| "authorization callback could not be received".to_owned())?;
    let mut request = vec![0_u8; 8_192];
    let count = stream
        .read(&mut request)
        .await
        .map_err(|_| "authorization callback could not be read".to_owned())?;
    let callback = parse_oauth_callback(&request[..count]);
    let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 46\r\nConnection: close\r\n\r\nAuthorization received. You may return to INXM.";
    let _ = stream.write_all(response).await;
    callback
}

fn parse_oauth_callback(request: &[u8]) -> Result<OAuthCallback, String> {
    let request = std::str::from_utf8(request)
        .map_err(|_| "authorization callback was malformed".to_owned())?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|line| line.split_whitespace().next())
        .ok_or_else(|| "authorization callback was malformed".to_owned())?;
    let url = reqwest::Url::parse(&format!("http://localhost{target}"))
        .map_err(|_| "authorization callback was malformed".to_owned())?;
    if url.path() != MCP_OAUTH_CALLBACK_PATH {
        return Err("authorization callback was malformed".to_owned());
    }
    let values: HashMap<_, _> = url.query_pairs().into_owned().collect();
    if values.contains_key("error") {
        return Ok(OAuthCallback::Denied);
    }
    let code = values
        .get("code")
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| "authorization callback did not include a code".to_owned())?;
    let state = values
        .get("state")
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| "authorization callback did not include state".to_owned())?;
    Ok(OAuthCallback::Code { code, state })
}

async fn complete_callback(
    facade: &McpOAuthFacade,
    expected_state: &str,
    callback: OAuthCallback,
) -> Result<(), String> {
    callback_state_result(expected_state, &callback)?;
    match callback {
        OAuthCallback::Denied => Err("authorization was denied".to_owned()),
        OAuthCallback::Code { code, state } => facade
            .complete_authorization(&code, &state)
            .await
            .map_err(sanitized_oauth_error)
            .map_err(|error| error.to_string()),
    }
}

fn callback_state_result(expected_state: &str, callback: &OAuthCallback) -> Result<(), String> {
    match callback {
        OAuthCallback::Denied => Err("authorization was denied".to_owned()),
        OAuthCallback::Code { state, .. } if state != expected_state => {
            Err("authorization callback state did not match".to_owned())
        }
        OAuthCallback::Code { .. } => Ok(()),
    }
}

// ─── Schedules ────────────────────────────────────────────────────────────────

const NL_TO_CRON_SYSTEM_PROMPT: &str = "You convert natural-language schedules into standard \
5-field cron expressions (minute hour day-of-month month day-of-week), interpreted in the \
user's LOCAL timezone. Respond with ONLY the cron expression — no prose, no code fence.";

/// Interpret a schedule expression: valid crontab syntax is used as-is;
/// anything else is treated as natural language and converted by the
/// compiler backend.
async fn interpret_schedule_expression(
    env: &EngineEnv,
    expression: &str,
) -> anyhow::Result<String> {
    if let Ok(normalized) = schedule_store::normalize_cron(expression) {
        return Ok(normalized);
    }

    let settings = AppSettings::load(&env.paths.settings_path);
    let backend = create_configured_backend(&settings).map_err(|_| {
        anyhow::anyhow!(
            "'{expression}' is not valid cron syntax, and no compiler is configured to \
             interpret it as natural language — use e.g. `*/15 * * * *`, or set a \
             compiler under Settings"
        )
    })?;

    let raw = backend
        .complete(NL_TO_CRON_SYSTEM_PROMPT, &format!("Schedule: {expression}"))
        .await
        .map_err(|e| anyhow::anyhow!("could not interpret the schedule: {e}"))?;
    let candidate = extract_cron_candidate(&raw).unwrap_or_else(|| raw.trim().to_owned());
    schedule_store::normalize_cron(&candidate).map_err(|e| {
        anyhow::anyhow!(
            "the compiler interpreted '{expression}' as '{candidate}', which is not a \
             valid cron expression ({e}) — try rephrasing or use cron syntax directly"
        )
    })
}

/// Pick a valid cron expression out of an otherwise chatty compiler response.
/// Scanning from the end prefers an explicit correction over an earlier draft.
fn extract_cron_candidate(raw: &str) -> Option<String> {
    std::iter::once(raw.trim())
        .chain(raw.lines().rev().map(str::trim))
        .filter_map(|line| {
            let candidate =
                line.trim_matches(|c: char| c.is_whitespace() || matches!(c, '`' | '\'' | '"'));
            schedule_store::normalize_cron(candidate)
                .ok()
                .map(|_| candidate.to_owned())
        })
        .next()
}

async fn save_schedule(
    env: &EngineEnv,
    plan_ref: &str,
    expression: &str,
    inputs: indexmap::IndexMap<String, serde_json::Value>,
) -> anyhow::Result<()> {
    let storage = env.storage()?;
    let plan = resolve_plan(&storage, plan_ref)?;
    let inputs = plan.resolve_inputs(&drop_inputs_deferring_to_defaults(&plan, inputs))?;
    let normalized = interpret_schedule_expression(env, expression).await?;

    let schedule = schedule_store::Schedule {
        id: uuid::Uuid::new_v4().to_string(),
        plan_id: plan.metadata.id.clone(),
        cron: normalized.clone(),
        enabled: true,
        inputs,
        created_at: chrono::Utc::now(),
        last_run: None,
    };
    env.paths
        .mutations
        .run_named("schedule.create", env.triggered_by(), || {
            let all = schedule_store::load(&env.paths.schedules_path)?
                .into_iter()
                .chain(std::iter::once(schedule))
                .collect::<Vec<_>>();
            schedule_store::save(&env.paths.schedules_path, &all)?;
            Ok(())
        })?;

    let next = schedule_store::next_occurrence(&normalized, chrono::Local::now())
        .map(|t| t.format("%b %d %H:%M").to_string())
        .unwrap_or_else(|| "never".to_owned());
    let interpretation = match normalized
        .split_whitespace()
        .skip(1)
        .collect::<Vec<_>>()
        .join(" ")
        == expression.trim()
    {
        true => String::new(),
        false => format!(" (interpreted as `{normalized}`)"),
    };
    env.emit(EngineEvent::ScheduleSaved {
        plan_id: plan.metadata.id.clone(),
        message: format!(
            "Scheduled “{}” for “{}”{} — local time, next run {}.",
            plan.name, expression, interpretation, next
        ),
    });
    env.emit(EngineEvent::ScheduleList(list_schedules(env)?));
    Ok(())
}

/// Delete a plan (all versions) plus any schedules pointing at it. Runs and
/// patches are historical records and stay.
fn delete_plan(env: &EngineEnv, plan_id: &str) -> anyhow::Result<()> {
    let plan_name = env
        .paths
        .mutations
        .run_named("plan.delete", env.triggered_by(), || {
            let storage = env.storage()?;
            let plan_name = storage
                .plans()
                .load_current(plan_id)
                .map(|plan| plan.name)
                .unwrap_or_else(|_| plan_id.to_owned());
            storage.plans().delete(plan_id)?;
            let remaining = schedule_store::load(&env.paths.schedules_path)?
                .into_iter()
                .filter(|schedule| schedule.plan_id != plan_id)
                .collect::<Vec<_>>();
            schedule_store::save(&env.paths.schedules_path, &remaining)?;
            Ok(plan_name)
        })?;

    env.emit(EngineEvent::PlanDeleted {
        plan_id: plan_id.to_owned(),
        message: format!(
            "Deleted plan “{plan_name}” and its schedules. Past runs stay in history."
        ),
    });
    env.emit(EngineEvent::PlanList(list_plans(env)?));
    env.emit(EngineEvent::RunList(list_runs(env)?));
    env.emit(EngineEvent::ScheduleList(list_schedules(env)?));
    Ok(())
}

pub(crate) fn list_schedule_summaries(
    storage: &StorageRoot,
    schedules_path: &std::path::Path,
) -> anyhow::Result<Vec<ScheduleItem>> {
    let plan_names: std::collections::HashMap<String, String> = storage
        .plans()
        .list()?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    let now = chrono::Local::now();
    let mut schedules: Vec<_> = schedule_store::load(schedules_path)?
        .into_iter()
        .map(|schedule| {
            let next_run = schedule
                .enabled
                .then(|| schedule_store::next_occurrence(&schedule.cron, now))
                .flatten();
            (schedule, next_run)
        })
        .collect();
    sort_by_next_run(&mut schedules);

    Ok(schedules
        .into_iter()
        .map(|(s, next_run)| ScheduleItem {
            plan_name: plan_names
                .get(&s.plan_id)
                .cloned()
                .unwrap_or_else(|| s.plan_id.clone()),
            next_run_display: next_run.map(|t| t.format("%b %d %H:%M").to_string()),
            id: s.id,
            plan_id: s.plan_id,
            cron: s.cron,
            enabled: s.enabled,
            inputs: s.inputs,
        })
        .collect())
}

/// Sort active schedules chronologically and keep paused/unschedulable items
/// at the bottom. The generic value type keeps this rule straightforward to test.
fn sort_by_next_run<T, D: Ord>(items: &mut [(T, Option<D>)]) {
    items.sort_by(|(_, left), (_, right)| match (left, right) {
        (Some(left), Some(right)) => left.cmp(right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
}

fn list_schedules(env: &EngineEnv) -> anyhow::Result<Vec<ScheduleItem>> {
    let storage = env.storage()?;
    list_schedule_summaries(&storage, &env.paths.schedules_path)
}

/// Background task: fire enabled schedules whose next occurrence since the
/// previous tick has passed. Missed slots while the app was closed are NOT
/// caught up — schedules describe the future, not a backlog.
async fn scheduler_loop(env: EngineEnv) {
    let mut last_tick = chrono::Local::now();
    let active = Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    loop {
        tokio::time::sleep(SCHEDULER_TICK).await;
        let now = chrono::Local::now();

        let schedules_paused = AppSettings::load(&env.paths.settings_path).schedules_paused;
        let active_ids = match active.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => {
                tracing::error!(
                    operation = "schedule.active_state",
                    app_version = env!("CARGO_PKG_VERSION"),
                    triggered_by = "scheduler",
                    outcome = "failure",
                    "scheduler active-state read failed"
                );
                // Do not advance `last_tick` — nothing was claimed this
                // iteration, so the window must be retried, not skipped.
                continue;
            }
        };
        let claim_started = std::time::Instant::now();
        let due = match env
            .paths
            .mutations
            .run_named("schedule.claim", "scheduler", || {
                claim_due_schedules(
                    &env.paths.schedules_path,
                    schedules_paused,
                    &active_ids,
                    last_tick,
                    now,
                )
            }) {
            Ok(due) => due,
            Err(_) => {
                tracing::error!(
                    operation = "schedule.claim",
                    app_version = env!("CARGO_PKG_VERSION"),
                    triggered_by = "scheduler",
                    duration_ms = claim_started.elapsed().as_millis() as u64,
                    outcome = "failure",
                    "scheduled occurrence claim failed"
                );
                // Same reasoning as the active-state failure above: keep the
                // window so a later successful tick still picks it up.
                continue;
            }
        };
        let claim_duration_ms = claim_started.elapsed().as_millis() as u64;
        last_tick = now;

        for schedule in due {
            tracing::info!(
                schedule_id = schedule.id,
                plan_id = schedule.plan_id,
                app_version = env!("CARGO_PKG_VERSION"),
                triggered_by = "scheduler",
                duration_ms = claim_duration_ms,
                outcome = "success",
                "scheduled occurrence claimed"
            );
            match active.lock() {
                Ok(mut guard) => {
                    guard.insert(schedule.id.clone());
                }
                Err(_) => {
                    tracing::error!(
                        schedule_id = schedule.id,
                        plan_id = schedule.plan_id,
                        operation = "schedule.dispatch",
                        app_version = env!("CARGO_PKG_VERSION"),
                        triggered_by = "scheduler",
                        outcome = "failure",
                        "scheduled occurrence dispatch failed"
                    );
                    continue;
                }
            }
            let scheduled_env = EngineEnv {
                paths: env.paths.clone(),
                evt_tx: env.evt_tx.clone(),
                repaint: env.repaint.clone(),
                session_id: crate::app::chat_store::find_by_plan(
                    &env.paths.data_dir,
                    &schedule.plan_id,
                )
                .map(|session| session.id),
                oauth_cancellations: Arc::clone(&env.oauth_cancellations),
                run_cancellations: Arc::clone(&env.run_cancellations),
                activities: env.activities.clone(),
                activity_id: None,
            };
            let plan_name = env
                .storage()
                .ok()
                .and_then(|st| st.plans().load_current(&schedule.plan_id).ok())
                .map(|p| p.name)
                .unwrap_or_else(|| schedule.plan_id.clone());
            scheduled_env.emit(EngineEvent::Assistant(format!(
                "⏱ Scheduled run of “{plan_name}” ({}).",
                schedule.cron
            )));
            tracing::info!(
                schedule_id = schedule.id,
                plan_id = schedule.plan_id,
                app_version = env!("CARGO_PKG_VERSION"),
                triggered_by = "scheduler",
                outcome = "dispatched",
                "scheduled occurrence dispatched"
            );
            let active = active.clone();
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                let result = run_plan_with_timeout(
                    &scheduled_env,
                    &schedule.plan_id,
                    schedule.inputs,
                    Some(SCHEDULED_STEP_TIMEOUT_SECS),
                    crate::storage::runs::RunSource::Schedule,
                )
                .await;
                let duration_ms = started.elapsed().as_millis() as u64;
                match result {
                    Ok(()) => tracing::info!(
                        schedule_id = schedule.id,
                        plan_id = schedule.plan_id,
                        app_version = env!("CARGO_PKG_VERSION"),
                        triggered_by = "scheduler",
                        duration_ms,
                        outcome = "success",
                        "scheduled occurrence completed"
                    ),
                    Err(error) => {
                        tracing::error!(
                            schedule_id = schedule.id,
                            plan_id = schedule.plan_id,
                            app_version = env!("CARGO_PKG_VERSION"),
                            triggered_by = "scheduler",
                            duration_ms,
                            outcome = "failure",
                            "scheduled occurrence completed"
                        );
                        scheduled_env.emit(EngineEvent::Failure(format!(
                            "scheduled run of “{plan_name}” failed: {error:#}"
                        )));
                    }
                }
                match active.lock() {
                    Ok(mut guard) => {
                        guard.remove(&schedule.id);
                    }
                    Err(_) => tracing::error!(
                        schedule_id = schedule.id,
                        plan_id = schedule.plan_id,
                        operation = "schedule.active_state",
                        app_version = env!("CARGO_PKG_VERSION"),
                        triggered_by = "scheduler",
                        outcome = "failure",
                        "scheduler active-state cleanup failed"
                    ),
                }
            });
        }
    }
}

/// Atomically record every occurrence selected for dispatch. `last_run` is a
/// claim timestamp, not a completion timestamp: a crash after this write may
/// lose that one occurrence, but it can never execute it twice.
fn claim_due_schedules(
    schedules_path: &Path,
    paused: bool,
    active_ids: &std::collections::HashSet<String>,
    last_tick: chrono::DateTime<chrono::Local>,
    now: chrono::DateTime<chrono::Local>,
) -> anyhow::Result<Vec<schedule_store::Schedule>> {
    let schedules = schedule_store::load(schedules_path)?;
    let due = due_schedules(schedules.clone(), paused, last_tick, now)
        .into_iter()
        .filter(|schedule| !active_ids.contains(&schedule.id))
        .collect::<Vec<_>>();
    if due.is_empty() {
        return Ok(due);
    }
    let claimed_ids = due
        .iter()
        .map(|schedule| schedule.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let claimed_at = now.with_timezone(&chrono::Utc);
    let updated = schedules
        .into_iter()
        .map(|schedule| {
            if claimed_ids.contains(schedule.id.as_str()) {
                schedule_store::Schedule {
                    last_run: Some(claimed_at),
                    ..schedule
                }
            } else {
                schedule
            }
        })
        .collect::<Vec<_>>();
    schedule_store::save(schedules_path, &updated)?;
    Ok(due)
}

fn due_schedules(
    schedules: Vec<schedule_store::Schedule>,
    paused: bool,
    last_tick: chrono::DateTime<chrono::Local>,
    now: chrono::DateTime<chrono::Local>,
) -> Vec<schedule_store::Schedule> {
    if paused {
        return Vec::new();
    }
    schedules
        .into_iter()
        .filter(|schedule| schedule.enabled)
        .filter(|schedule| {
            schedule_store::next_occurrence(&schedule.cron, last_tick)
                .is_some_and(|next| next <= now)
        })
        .collect()
}

fn list_patches(env: &EngineEnv) -> anyhow::Result<Vec<PatchListItem>> {
    let storage = env.storage()?;
    let plan_names: std::collections::HashMap<String, String> = storage
        .plans()
        .list()?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    let items: Vec<PatchListItem> = storage
        .patches()
        .list()?
        .into_iter()
        .map(|p| PatchListItem {
            plan_name: plan_names
                .get(&p.plan_id)
                .cloned()
                .unwrap_or_else(|| p.plan_id.clone()),
            id: p.id,
            failing_step_id: p.failing_step_id,
            status: p.status,
            proposed_at: p.proposed_at,
        })
        .collect();
    Ok(sorted_by_recency(items))
}

fn sorted_by_recency(items: Vec<PatchListItem>) -> Vec<PatchListItem> {
    let mut items = items;
    items.sort_by_key(|p| std::cmp::Reverse(p.proposed_at));
    items
}

fn save_settings(env: &EngineEnv, settings: AppSettings) -> anyhow::Result<()> {
    env.paths
        .mutations
        .run_named("settings.save", env.triggered_by(), || {
            settings.save(&env.paths.settings_path)
        })?;
    env.emit(EngineEvent::Ready {
        data_dir: env.paths.data_dir.display().to_string(),
        backend: settings.status_label(),
        environment: EnvProbe::detect().summary(),
    });
    env.emit(EngineEvent::Settings(settings));
    Ok(())
}

fn bootstrap(env: &EngineEnv) -> anyhow::Result<()> {
    env.paths
        .mutations
        .run_named("bootstrap.seed", "application", || {
            // Ensure storage exists and seed a starter catalog on first launch.
            let _ = env.storage()?;
            if !env.paths.catalog_path.exists() {
                std::fs::write(&env.paths.catalog_path, default_catalog_yaml())?;
            }

            // Older Windows catalogs route echo through cmd.exe, whose redirected
            // output uses a legacy code page. Migrate known seeded configurations
            // to a PowerShell command that emits UTF-8 without interpolating input.
            if cfg!(windows)
                && let Some(migrated) = legacy_echo_to_utf8_migration(&env.catalog()?)
            {
                migrated.save_to_file(&env.paths.catalog_path)?;
            }

            // Older catalogs also predate the native HTTP adapter. Add a generic
            // GET tool so planners do not need to guess whether curl exists.
            if let Some(migrated) = add_native_http_get_migration(&env.catalog()?) {
                migrated.save_to_file(&env.paths.catalog_path)?;
            }

            // Catalogs seeded before the `mcp<2` pin launch their uvx servers
            // against the 1.x-era APIs of a 2.x client and crash on import.
            if let Some(migrated) = add_mcp_v1_constraint_migration(&env.catalog()?) {
                migrated.save_to_file(&env.paths.catalog_path)?;
            }
            Ok(())
        })?;

    let settings = AppSettings::load(&env.paths.settings_path);
    env.emit(EngineEvent::Ready {
        data_dir: env.paths.data_dir.display().to_string(),
        backend: settings.status_label(),
        environment: EnvProbe::detect().summary(),
    });
    env.emit(EngineEvent::Settings(settings));
    env.emit(EngineEvent::PlanList(list_plans(env)?));
    env.emit(EngineEvent::RunList(list_runs(env)?));
    let catalog = env.catalog()?;
    env.emit(EngineEvent::Catalog(catalog.all().cloned().collect()));
    env.emit(EngineEvent::ScheduleList(list_schedules(env)?));
    Ok(())
}

async fn compile(env: &EngineEnv, intent: String) -> anyhow::Result<()> {
    let catalog = env.catalog()?;
    let settings = AppSettings::load(&env.paths.settings_path);
    let backend = create_configured_backend(&settings)?;

    env.emit(EngineEvent::CompileStarted {
        intent: intent.clone(),
    });
    let console = env.attach_compile_console("compile");

    let request = compile_request(&catalog, &settings, intent, None);
    let plan = crate::llm::with_cli_line_sink(
        std::sync::Arc::new(console.clone()),
        compile_validate_normalize(
            &backend,
            request,
            &catalog,
            "compilation failed",
            Some(&console),
        ),
    )
    .await
    .inspect_err(|error| console.close(format!("✗ {error:#}")))?;
    console.close_after_persisting(
        || {
            env.paths
                .mutations
                .run_named("plan.compile_save", env.triggered_by(), || {
                    Ok(env.storage()?.plans().save(&plan)?)
                })
        },
        "compiled, but saving the plan failed",
        || {
            format!(
                "✓ compiled “{}” — {} steps, validated",
                plan.name,
                plan.steps.len()
            )
        },
    )?;
    env.emit(EngineEvent::PlanCompiled {
        plan: Box::new(plan),
    });
    Ok(())
}

// ─── Guided plan creation (refine → design → compile) ──────────────────────────

/// Belt-and-braces rewrite of known phantom-surface phrases the compiler
/// backend sometimes invents in its free-form prose (e.g. "open the Plan
/// View") even though the prompt templates instruct it to name only real UI
/// surfaces. The real surface is the plan card pinned above the
/// chat transcript, not a standalone "view".
///
/// Cheap, literal string replacement — not a substitute for the prompt fix,
/// just a safety net so a stale phrase never reaches the user. Logs a
/// warning whenever it fires so prompt regressions stay visible.
fn rewrite_phantom_surfaces(text: &str) -> String {
    const PHANTOM_PHRASES: &[(&str, &str)] = &[
        ("Plan View", "Plan card"),
        ("plan view", "plan card"),
        ("PLAN VIEW", "PLAN CARD"),
    ];

    let mut out = text.to_owned();
    let mut rewrote = false;
    for (phantom, real) in PHANTOM_PHRASES {
        if out.contains(phantom) {
            out = out.replace(phantom, real);
            rewrote = true;
        }
    }
    if rewrote {
        tracing::warn!(
            "assistant text referenced a phantom UI surface (\"Plan View\"); rewrote to the \
             real name — this usually means the prompt vocabulary instruction regressed"
        );
    }
    out
}

async fn assess_intent(
    env: &EngineEnv,
    intent: String,
    conversation: Vec<compiler::SpecTurn>,
) -> anyhow::Result<()> {
    let catalog = env.catalog()?;
    let settings = AppSettings::load(&env.paths.settings_path);
    let backend = create_configured_backend(&settings)?;

    env.emit(EngineEvent::AssessStarted {
        intent: intent.clone(),
    });

    let request = compiler::AssessRequest {
        intent,
        conversation,
        tool_catalog: runnable_tool_catalog(&catalog),
        extra_context: Some(planning_context(&settings)),
    };
    let mut assessment = backend
        .assess(&request)
        .await
        .map_err(|e| anyhow::anyhow!("intent assessment failed: {e}"))?;
    if let Some(question) = assessment.question.take() {
        if is_internal_execution_choice_question(&question) {
            tracing::warn!(
                "assessment asked the user to choose an internal execution primitive; accepting the best-effort spec instead"
            );
            assessment.needs_clarification = false;
            assessment.confidence = assessment.confidence.max(0.85);
        } else {
            assessment.question = Some(rewrite_phantom_surfaces(&question));
        }
    }
    env.emit(EngineEvent::AssessmentReady {
        assessment: Box::new(assessment),
    });
    Ok(())
}

const INSIGHT_SYSTEM_PROMPT: &str = "\
You are the assistant inside inxm, a local plan-automation app. The user \
just asked a question in chat instead of typing a slash command, so this is \
an INSIGHT question, not a request to change or run anything. Answer \
briefly and factually using ONLY the JSON context supplied below — never \
invent plans, runs, timestamps, or outcomes that are not present in it. If \
the context does not contain the answer, say so plainly instead of \
guessing.\n\n\
If, and only if, the question clearly implies the user wants something \
changed or executed (an edit, running/repairing/resuming a run, a new \
schedule, a brand-new plan, etc.) rather than explained, suggest ONE \
concrete follow-up action as the exact slash command the user would type. \
Never suggest an action for a question that is purely asking for \
information.\n\n\
Reply with ONLY this JSON object, no prose outside it, no code fence:\n\
{\"answer\": \"<plain-text answer>\", \"suggested_action\": {\"label\": \
\"<short button label, e.g. Run this plan>\", \"command\": \"<the exact \
slash command, e.g. /run>\"}}\n\
Omit suggested_action (use null) when nothing needs to change.\n\n\
Slash commands available: /compile <intent>, /edit <change>, /run, \
/repair [run-id], /resume <run-id>, /schedule <plan> <cron>, /show [plan].";

#[derive(serde::Deserialize)]
struct InsightResponsePayload {
    answer: String,
    #[serde(default)]
    suggested_action: Option<SuggestedAction>,
}

/// Parse the insight backend's reply. A JSON envelope (optionally wrapped in
/// a code fence) is expected, but any parse failure just falls back to
/// treating the whole reply as the plain-text answer with no suggested
/// action — an unhelpful reply is better than a broken one.
fn parse_insight_response(raw: &str) -> (String, Option<SuggestedAction>) {
    let candidate = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    match serde_json::from_str::<InsightResponsePayload>(candidate) {
        Ok(payload) => (payload.answer, payload.suggested_action),
        Err(_) => (raw.trim().to_owned(), None),
    }
}

/// Read-only context handed to the insight backend — the same data already
/// exposed as MCP tools (`show_plan`, `list_runs`, `list_schedules`),
/// fetched directly rather than through an HTTP round trip since this call
/// already runs inside the engine.
fn insight_context(env: &EngineEnv, plan_id: Option<&str>) -> String {
    let storage = match env.storage() {
        Ok(storage) => storage,
        Err(error) => return format!("(local storage is unavailable: {error})"),
    };
    let context = match plan_id.map(|plan_ref| resolve_plan(&storage, plan_ref)) {
        Some(Ok(plan)) => {
            let plan_id = plan.metadata.id.clone();
            let recent_runs: Vec<RunListItem> = list_run_summaries(&storage)
                .unwrap_or_default()
                .into_iter()
                .filter(|run| run.plan_id == plan_id)
                .take(10)
                .collect();
            let schedules: Vec<ScheduleItem> =
                list_schedule_summaries(&storage, &env.paths.schedules_path)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|schedule| schedule.plan_id == plan_id)
                    .collect();
            serde_json::json!({
                "plan": plan,
                "recent_runs_for_this_plan": recent_runs,
                "schedules_for_this_plan": schedules,
            })
        }
        Some(Err(error)) => serde_json::json!({ "plan_lookup_error": error.to_string() }),
        None => {
            let plans = list_plan_summaries(&storage).unwrap_or_default();
            let recent_runs: Vec<RunListItem> = list_run_summaries(&storage)
                .unwrap_or_default()
                .into_iter()
                .take(15)
                .collect();
            let schedules =
                list_schedule_summaries(&storage, &env.paths.schedules_path).unwrap_or_default();
            serde_json::json!({
                "plans": plans,
                "recent_runs": recent_runs,
                "schedules": schedules,
            })
        }
    };
    context.to_string()
}

/// Answer a plain-text chat message from context (`/plans`, `/runs`,
/// `/schedules` data — the same read-only facts already exposed over the
/// local MCP server) instead of treating it as an instruction. Never edits,
/// runs, or compiles anything; it may only *suggest* a follow-up slash
/// command, which the composer validates again before it can run.
async fn answer_insight(
    env: &EngineEnv,
    question: String,
    plan_id: Option<String>,
) -> anyhow::Result<()> {
    let settings = AppSettings::load(&env.paths.settings_path);
    let backend = create_configured_backend(&settings)?;
    let context = insight_context(env, plan_id.as_deref());

    env.emit(EngineEvent::InsightStarted);

    let user = format!("Context:\n{context}\n\nQuestion: {question}");
    let raw = backend
        .complete(INSIGHT_SYSTEM_PROMPT, &user)
        .await
        .map_err(|e| anyhow::anyhow!("could not answer that: {e}"))?;

    let (answer, suggested_action) = parse_insight_response(&raw);
    // Only forward an action the composer could actually run — a hallucinated
    // or malformed command is silently dropped rather than shown as dead.
    let suggested_action = suggested_action.filter(|action| {
        matches!(
            commands::parse(&action.command),
            Ok(commands::ChatInput::Command(_))
        )
    });
    env.emit(EngineEvent::InsightAnswer {
        answer,
        suggested_action,
    });
    Ok(())
}

async fn generate_design(
    env: &EngineEnv,
    spec: compiler::SpecDraft,
    conversation: Vec<compiler::SpecTurn>,
    previous_design: Option<compiler::SolutionDesign>,
    feedback: Option<String>,
) -> anyhow::Result<()> {
    let catalog = env.catalog()?;
    let settings = AppSettings::load(&env.paths.settings_path);
    let backend = create_configured_backend(&settings)?;

    env.emit(EngineEvent::DesignStarted);

    let request = compiler::DesignRequest {
        spec,
        conversation,
        tool_catalog: runnable_tool_catalog(&catalog),
        previous_design,
        feedback,
        extra_context: Some(planning_context(&settings)),
    };
    let mut design = backend
        .design(&request)
        .await
        .map_err(|e| anyhow::anyhow!("solution design failed: {e}"))?;
    normalize_agent_outline(&mut design, &settings);
    design.title = rewrite_phantom_surfaces(&design.title);
    design.summary = rewrite_phantom_surfaces(&design.summary);
    for tool in &mut design.recommended_tools {
        tool.reason = rewrite_phantom_surfaces(&tool.reason);
    }
    for step in &mut design.execution_outline {
        step.description = rewrite_phantom_surfaces(&step.description);
    }
    env.emit(EngineEvent::DesignReady {
        design: Box::new(design),
    });
    Ok(())
}

fn planning_context(settings: &AppSettings) -> String {
    let mut context = EnvProbe::detect().compiler_context();
    if settings.supports_agent_call() {
        context.push_str(
            "\n\n## Agent execution capability\n\
             Experimental AGENT_CALL is enabled and has a configured tool-using agent backend. \
             Treat it as naturally available for work that must inspect a workspace, choose \
             commands, edit files, and iterate. This is an internal execution choice: do not ask \
             the user whether to use Claude, Codex, a custom CLI, AGENT_CALL, CODE_CALL, or \
             PROMPT_CALL. Use AGENT_CALL when its semantics fit; use deterministic typed steps \
             for fixed commands and checks. Any outline step that invokes a coding agent to \
             inspect or edit the workspace MUST have step_kind `agent_call`, never `code_call`; \
             provider CLIs are selected by AGENT_CALL at runtime and must not be embedded in \
             the design.\n",
        );
    } else {
        context.push_str(
            "\n\n## Agent execution capability\n\
             Experimental AGENT_CALL is not enabled for this request. Do not propose it and do \
             not ask the user whether to enable it or choose an agent CLI. Use the available \
             deterministic capabilities.\n",
        );
    }
    context
}

fn normalize_agent_outline(design: &mut compiler::SolutionDesign, settings: &AppSettings) {
    if !settings.supports_agent_call() {
        return;
    }
    for step in &mut design.execution_outline {
        if step.step_kind != "code_call" || !is_agent_shaped_outline_step(step) {
            continue;
        }
        tracing::warn!(
            step_name = %step.name,
            "solution design wrapped agent-shaped workspace work in code_call; normalized it to agent_call"
        );
        step.step_kind = "agent_call".to_owned();
        step.description = step
            .description
            .replace(
                "coding-agent CLI (codex/claude)",
                "configured AGENT_CALL backend",
            )
            .replace(
                "coding agent CLI (codex/claude)",
                "configured AGENT_CALL backend",
            );
    }
}

fn is_agent_shaped_outline_step(step: &compiler::OutlineStep) -> bool {
    let text = format!("{} {}", step.name, step.description).to_ascii_lowercase();
    let names_agent = ["agent", "claude", "codex", "coding cli", "tool-using cli"]
        .iter()
        .any(|term| text.contains(term));
    let changes_workspace = [
        "implement",
        "edit",
        "modify",
        "fix",
        "workspace",
        "source file",
    ]
    .iter()
    .any(|term| text.contains(term));
    names_agent && changes_workspace
}

fn is_internal_execution_choice_question(question: &str) -> bool {
    let normalized = question.to_ascii_lowercase();
    [
        "agent_call",
        "prompt_call",
        "code_call",
        "tool_call",
        "claude",
        "codex",
        "coding agent cli",
        "coding-agent cli",
        "compiler's built-in llm",
    ]
    .iter()
    .any(|term| normalized.contains(term))
}

async fn compile_from_spec(
    env: &EngineEnv,
    intent: String,
    spec: compiler::SpecDraft,
    mut design: Option<compiler::SolutionDesign>,
    conversation: Vec<compiler::SpecTurn>,
) -> anyhow::Result<()> {
    let catalog = env.catalog()?;
    let settings = AppSettings::load(&env.paths.settings_path);
    let backend = create_configured_backend(&settings)?;
    if let Some(design) = design.as_mut() {
        // Guided-flow state can outlive an app rebuild. Re-apply the
        // capability-aware boundary when an older design is approved so a
        // stale CODE_CALL hint cannot wrap the configured coding agent.
        normalize_agent_outline(design, &settings);
    }

    env.emit(EngineEvent::CompileStarted {
        intent: intent.clone(),
    });
    let console = env.attach_compile_console("compile from spec");

    let mut request = compile_request(&catalog, &settings, intent, None);
    request.extra_context = Some(spec_compile_context(
        request.extra_context.as_deref().unwrap_or_default(),
        &spec,
        design.as_ref(),
        &conversation,
    ));
    let mut plan = crate::llm::with_cli_line_sink(
        std::sync::Arc::new(console.clone()),
        compile_validate_normalize(
            &backend,
            request,
            &catalog,
            "compilation failed",
            Some(&console),
        ),
    )
    .await
    .inspect_err(|error| console.close(format!("✗ {error:#}")))?;
    plan.metadata.status = PlanStatus::Published;
    plan.metadata.solution_design = design.as_ref().map(compiler::SolutionDesign::to_markdown);
    console.close_after_persisting(
        || {
            env.paths
                .mutations
                .run_named("plan.compile_spec_save", env.triggered_by(), || {
                    Ok(env.storage()?.plans().save(&plan)?)
                })
        },
        "compiled, but saving the plan failed",
        || {
            format!(
                "✓ compiled “{}” — {} steps, validated",
                plan.name,
                plan.steps.len()
            )
        },
    )?;
    env.emit(EngineEvent::PlanCompiled {
        plan: Box::new(plan),
    });
    Ok(())
}

/// Compose the compile `extra_context` for the guided flow: the usual host
/// environment description, then the refined spec, the clarification
/// conversation (when it went beyond the opening message), and the approved
/// solution design.
fn spec_compile_context(
    host_context: &str,
    spec: &compiler::SpecDraft,
    design: Option<&compiler::SolutionDesign>,
    conversation: &[compiler::SpecTurn],
) -> String {
    let mut out = host_context.to_owned();
    out.push_str("\n\n## Refined specification\n\n");
    out.push_str(&spec.to_compile_context());
    if conversation.len() > 1 {
        out.push_str("\n## Clarification conversation\n\n");
        for turn in conversation {
            out.push_str(&format!("{}: {}\n", turn.role, turn.content));
        }
    }
    if let Some(design) = design {
        out.push_str("\n## Approved solution design\n\n");
        out.push_str(&design.to_markdown());
    }
    out
}

async fn edit_plan(env: &EngineEnv, plan_ref: &str, instruction: String) -> anyhow::Result<()> {
    let storage = env.storage()?;
    let existing = resolve_plan(&storage, plan_ref)?;
    let catalog = env.catalog()?;
    let settings = AppSettings::load(&env.paths.settings_path);
    let backend = create_configured_backend(&settings)?;

    env.emit(EngineEvent::EditStarted {
        plan_name: existing.name.clone(),
        instruction: instruction.clone(),
    });
    let console = env.attach_compile_console("edit plan");

    let request = edit_compile_request(
        &storage,
        &catalog,
        &settings,
        instruction.clone(),
        existing.clone(),
    )?;
    let proposed_plan = crate::llm::with_cli_line_sink(
        std::sync::Arc::new(console.clone()),
        compile_validate_normalize(
            &backend,
            request,
            &catalog,
            "plan edit failed",
            Some(&console),
        ),
    )
    .await
    .inspect_err(|error| console.close(format!("✗ {error:#}")))?;

    // Not saved to the plan store yet — this is a reviewable proposal. It
    // only becomes the plan's current version once the user applies it via
    // `/apply` (or rejects it via `/reject`), mirroring the repair flow.
    let edit = PlanEdit::new(
        existing.metadata.id.clone(),
        existing.metadata.version,
        instruction,
        existing,
        proposed_plan.clone(),
    );
    console.close_after_persisting(
        || {
            env.paths
                .mutations
                .run_named("plan_edit.propose", env.triggered_by(), || {
                    Ok(storage.plan_edits().save(&edit)?)
                })
        },
        "edited, but saving the proposed edit failed",
        || {
            format!(
                "✓ proposed an update to “{}” — {} steps, awaiting your review",
                proposed_plan.name,
                proposed_plan.steps.len()
            )
        },
    )?;
    env.emit(EngineEvent::EditProposed {
        edit: Box::new(edit),
    });
    Ok(())
}

pub(crate) fn edit_compile_request(
    storage: &StorageRoot,
    catalog: &ToolCatalog,
    settings: &AppSettings,
    instruction: String,
    existing_plan: Plan,
) -> anyhow::Result<CompileRequest> {
    let run_history = recent_edit_run_history(storage, &existing_plan.metadata.id)?;
    let mut request = compile_request(catalog, settings, instruction, Some(existing_plan));
    request.run_history = run_history;
    Ok(request)
}

fn recent_edit_run_history(
    storage: &StorageRoot,
    plan_id: &str,
) -> anyhow::Result<Vec<compiler::CompileRunHistoryEntry>> {
    let run_store = storage.runs();
    let summaries = run_store.list()?;
    let mut history = Vec::new();

    for summary in summaries
        .into_iter()
        .filter(|summary| summary.plan_id == plan_id)
        .take(EDIT_RUN_HISTORY_LIMIT)
    {
        match run_store.load(&summary.id) {
            Ok(run) => history.push(compile_run_history_entry(run)),
            Err(error) => tracing::warn!(
                "storage.event" = "edit_run_history_skipped",
                "run.id" = %summary.id,
                %error,
                "run disappeared or became unreadable while preparing plan edit context"
            ),
        }
    }

    Ok(history)
}

fn compile_run_history_entry(run: Run) -> compiler::CompileRunHistoryEntry {
    let status = run.status.to_string();
    let status_message = match &run.status {
        executor::RunStatus::Failed { message, .. } => Some(message.clone()),
        _ => None,
    };
    let steps = run
        .step_runs
        .into_values()
        .map(|step| compiler::CompileRunStep {
            step_id: step.step_id,
            status: step.status.to_string(),
            attempt: step.attempt,
            duration_ms: step.duration_ms,
            outputs: serde_json::to_value(step.outputs).unwrap_or(serde_json::Value::Null),
            stdout: step.stdout,
            stderr: step.stderr,
            error: step.error,
            iterations: step
                .iterations
                .into_iter()
                .map(|iteration| compiler::CompileRunIteration {
                    iteration: iteration.iteration,
                    status: iteration.status.to_string(),
                    duration_ms: iteration.duration_ms,
                    outputs: serde_json::to_value(iteration.outputs)
                        .unwrap_or(serde_json::Value::Null),
                    stdout: iteration.stdout,
                    stderr: iteration.stderr,
                    error: iteration.error,
                })
                .collect(),
        })
        .collect();

    compiler::CompileRunHistoryEntry {
        run_id: run.id,
        plan_version: run.plan_version,
        status,
        status_message,
        started_at: run.started_at.to_rfc3339(),
        inputs: serde_json::to_value(run.inputs).unwrap_or(serde_json::Value::Null),
        outputs: serde_json::to_value(run.outputs).unwrap_or(serde_json::Value::Null),
        steps,
    }
}

pub(crate) fn compile_request(
    catalog: &ToolCatalog,
    settings: &AppSettings,
    intent: String,
    existing_plan: Option<Plan>,
) -> CompileRequest {
    let probe = EnvProbe::detect();
    let mut allowed_step_types = vec![
        StepType::ToolCall,
        StepType::CodeCall,
        StepType::HumanInteraction,
        StepType::FanOut,
        StepType::FanIn,
        StepType::PromptCall,
        StepType::Condition,
    ];
    if settings.supports_agent_call() {
        allowed_step_types.push(StepType::AgentCall);
    }
    CompileRequest {
        intent,
        allowed_step_types,
        tool_catalog: runnable_tool_catalog(catalog),
        existing_plan,
        run_history: vec![],
        // The compiler must know the host so it never emits bash steps on a
        // bash-less Windows (or PowerShell steps on Linux), nor shells out to
        // CLI helpers that are not installed here.
        extra_context: Some(probe.compiler_context()),
    }
}

/// `console` (when given) receives lifecycle notes — attempt, retry, and
/// validation milestones — so a minutes-long compiler call is observable
/// while it runs and leaves a persisted trace afterwards.
pub(crate) async fn compile_validate_normalize(
    backend: &compiler::Backend,
    request: CompileRequest,
    catalog: &ToolCatalog,
    error_context: &str,
    console: Option<&CompileConsole>,
) -> anyhow::Result<Plan> {
    let note = |text: String| {
        if let Some(console) = console {
            console.info(text);
        }
    };
    let original_request = request.clone();
    let mut regenerated = false;
    note("calling the compiler backend — this can take a few minutes…".to_owned());
    let attempt = match backend.compile(request).await {
        Err(crate::error::CompilerError::InvalidResponse { message, .. }) => {
            // Nested scripts and prompts make large plans unusually prone to
            // one malformed JSON escape. Give unparseable model output one
            // clean regeneration attempt, while leaving API/config/transport
            // errors alone because retrying those cannot correct the response.
            regenerated = true;
            note(
                "the response could not be parsed as plan JSON — asking the backend to \
                 regenerate once…"
                    .to_owned(),
            );
            let mut retry = original_request.clone();
            retry.intent = format!(
                "{}\n\nYour previous response could not be parsed as plan JSON: {}. Regenerate the complete plan. Return strict valid JSON, correctly escape every inline script and multiline string, and keep descriptions and scripts concise.",
                retry.intent, message
            );
            backend.compile(retry).await
        }
        other => other,
    };

    // A plan that parsed but failed the deterministic validator is not a
    // dead end: keep the artifact and its errors so the correction retry can
    // show the model its own plan alongside the exact violations.
    let (mut plan, mut errors) = match attempt {
        Ok(plan) => (plan, Vec::new()),
        Err(crate::error::CompilerError::PlanValidationFailed { plan, errors, .. }) => {
            (*plan, errors)
        }
        Err(error) if regenerated => {
            return Err(anyhow::anyhow!(
                "{error_context} during JSON regeneration: {error}"
            ));
        }
        Err(error) => return Err(anyhow::anyhow!("{error_context}: {error}")),
    };

    if errors.is_empty() {
        // The compiler validated against the runnable tool subset; re-check
        // against the engine's full catalog.
        errors = validator::validate(&plan, catalog)
            .iter()
            .map(ToString::to_string)
            .collect();
    }

    if !errors.is_empty() {
        note(format!(
            "the plan failed deterministic validation ({} issue{}) — asking the backend for \
             one correction…",
            errors.len(),
            if errors.len() == 1 { "" } else { "s" }
        ));
        let initial_metadata = plan.metadata.clone();
        let feedback = errors
            .iter()
            .map(|error| format!("• {error}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut retry = original_request;
        retry.intent = format!(
            "{}\n\nThe previous plan failed deterministic validation. Correct only the issues below and return the complete corrected plan:\n{}",
            retry.intent, feedback
        );
        retry.existing_plan = Some(plan);
        plan = match backend.compile(retry).await {
            Ok(plan) => plan,
            Err(crate::error::CompilerError::PlanValidationFailed { errors, .. }) => {
                let bullet_list = errors
                    .iter()
                    .map(|error| format!("• {error}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                anyhow::bail!(
                    "the compiled plan failed validation after one correction attempt:\n{bullet_list}"
                );
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "{error_context} during validation retry: {e}"
                ));
            }
        };
        // The rejected artifact was never saved, so correction is still the
        // same logical version rather than a user-visible edit.
        plan.metadata = initial_metadata;
        let remaining = validator::validate(&plan, catalog);
        if !remaining.is_empty() {
            let bullet_list = remaining
                .iter()
                .map(|error| format!("• {error}"))
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(
                "the compiled plan failed validation after one correction attempt:\n{bullet_list}"
            );
        }
    }

    note("plan validated — normalizing and saving…".to_owned());
    Ok(normalize(plan))
}

async fn run_plan(
    env: &EngineEnv,
    plan_ref: &str,
    inputs: indexmap::IndexMap<String, serde_json::Value>,
) -> anyhow::Result<()> {
    run_plan_with_timeout(
        env,
        plan_ref,
        inputs,
        None,
        crate::storage::runs::RunSource::Chat,
    )
    .await
}

async fn run_plan_with_timeout(
    env: &EngineEnv,
    plan_ref: &str,
    inputs: indexmap::IndexMap<String, serde_json::Value>,
    timeout_secs: Option<u64>,
    source: crate::storage::runs::RunSource,
) -> anyhow::Result<()> {
    let storage = Arc::new(env.storage()?);
    let catalog = env.catalog()?;
    let plan = resolve_plan(&storage, plan_ref)?;
    let inputs = plan.resolve_inputs(&drop_inputs_deferring_to_defaults(&plan, inputs))?;

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();
    let (human_tx, mut human_rx) = tokio::sync::mpsc::unbounded_channel::<HumanRequest>();

    let settings = AppSettings::load(&env.paths.settings_path);
    ensure_agent_call_allowed(&plan, &settings)?;
    let config = ExecutorConfig {
        inputs: inputs.clone(),
        timeout_secs,
        storage: storage.clone(),
        catalog,
        progress: Some(progress_tx),
        human: Some(human_tx),
        llm_keys: llm_keys_from(&settings),
        source: Some(source),
    };

    let plan_for_events = plan.clone();
    let execution = executor::execute(plan, config);
    tokio::pin!(execution);

    // Fires when the user clicks "Abort" on this run (see `abort_run`); the
    // sender is only registered in `env.run_cancellations` once `run_id` is
    // known, so the UI cannot request an abort before it has a run id to
    // send.
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
    let mut cancel_tx = Some(cancel_tx);
    tokio::pin!(cancel_rx);

    let mut run_id: Option<String> = None;
    let mut announced = false;

    enum RunLoopOutcome {
        Finished(Box<Result<Run, crate::error::ExecutorError>>),
        Aborted,
    }

    let outcome = loop {
        tokio::select! {
            result = &mut execution => break RunLoopOutcome::Finished(Box::new(result)),
            Some(progress) = progress_rx.recv() => {
                if !announced {
                    announced = true;
                    run_id = Some(progress.run_id.clone());
                    if let Some(tx) = cancel_tx.take() {
                        env.run_cancellations
                            .lock()
                            .await
                            .insert(progress.run_id.clone(), tx);
                    }
                    env.emit(EngineEvent::RunStarted {
                        run_id: progress.run_id.clone(),
                        plan: Box::new(plan_for_events.clone()),
                        inputs: inputs.clone(),
                    });
                }
                env.emit(EngineEvent::StepProgress(Box::new(progress)));
            }
            Some(request) = human_rx.recv() => {
                let for_run = run_id.clone().unwrap_or_default();
                env.emit(EngineEvent::HumanNeeded { run_id: for_run, request });
            }
            _ = &mut cancel_rx => break RunLoopOutcome::Aborted,
        }
    };

    if let Some(id) = &run_id {
        env.run_cancellations.lock().await.remove(id);
    }

    // Drain any progress that raced with completion.
    while let Ok(progress) = progress_rx.try_recv() {
        env.emit(EngineEvent::StepProgress(Box::new(progress)));
    }

    match outcome {
        RunLoopOutcome::Finished(outcome) => match *outcome {
            Ok(run) => {
                env.emit(EngineEvent::RunFinished { run: Box::new(run) });
            }
            Err(error) => {
                // The executor persists run state before returning an error;
                // surface the recorded run so the UI can show what happened.
                if let Some(id) = run_id
                    && let Ok(run) = storage.runs().load(&id)
                {
                    env.emit(EngineEvent::RunFinished { run: Box::new(run) });
                }
                anyhow::bail!("run failed: {error}");
            }
        },
        RunLoopOutcome::Aborted => {
            // `execution` is simply never polled again from here on — it is
            // abandoned in place and drops when this function returns. Only
            // stdout/stderr capture tasks the executor may have spawned can
            // outlive that point, and those exit on their own once the
            // underlying child process's pipes close.
            let Some(id) = run_id else {
                // Aborted before a run id was ever announced — nothing was
                // persisted, so there is nothing to mark cancelled.
                return Ok(());
            };
            let mut run = storage.runs().load(&id)?;
            let finished_at = chrono::Utc::now();
            for step_run in run.step_runs.values_mut() {
                if matches!(
                    step_run.status,
                    crate::executor::StepRunStatus::Pending
                        | crate::executor::StepRunStatus::Running
                ) {
                    step_run.status = crate::executor::StepRunStatus::Cancelled;
                    step_run.finished_at = Some(finished_at);
                }
            }
            run.status = executor::RunStatus::Cancelled;
            run.finished_at = Some(finished_at);
            storage.runs().save(&run)?;
            env.emit(EngineEvent::RunFinished { run: Box::new(run) });
        }
    }
    Ok(())
}

/// Abort a running run: fires its cancellation signal if one is registered
/// (i.e. the run has announced its id and is still in flight). A no-op if
/// the run already finished or was never found — abort is best-effort and
/// should never surface an error to the user for a run that simply beat it
/// to completion.
async fn abort_run(env: &EngineEnv, run_id: &str) -> anyhow::Result<()> {
    if let Some(cancel) = env.run_cancellations.lock().await.remove(run_id) {
        let _ = cancel.send(());
    }
    Ok(())
}

/// Re-execute a failed run against the current plan version: only the
/// originally failed step and its true dependents run again. Mirrors
/// `run_plan`'s event sequence (`RunStarted` → `StepProgress`* →
/// `RunFinished`, with `HumanNeeded` interleaved if a HUMAN_INTERACTION step
/// is reached) so the UI's run view works unmodified.
///
/// `plan_id` is expected to be `run.plan_id` — it exists on the command so a
/// caller that already has both in hand (e.g. a "Resume" action on an
/// inspected run, which carries both `run` and `plan`) can pass it straight
/// through. An empty string is accepted as "derive it from the run" (the
/// `/resume <run-id>` slash command only has a run id to work with); any
/// other mismatching value is rejected rather than silently ignored.
async fn resume_run(
    env: &EngineEnv,
    plan_id: &str,
    run_id: &str,
    input_overrides: indexmap::IndexMap<String, serde_json::Value>,
) -> anyhow::Result<()> {
    let storage = Arc::new(env.storage()?);
    let catalog = env.catalog()?;

    let run = load_run_by_prefix(&storage, run_id)?;
    if !run.status.is_failed() {
        anyhow::bail!(
            "run '{}' has not failed (status: {}); nothing to resume",
            run.id,
            run.status
        );
    }
    if !plan_id.is_empty() && plan_id != run.plan_id {
        anyhow::bail!(
            "run '{}' belongs to plan '{}', not '{plan_id}'",
            run.id,
            run.plan_id
        );
    }
    // Deliberately `load_current`, not `load_version(run.plan_version)`: the
    // whole point of resuming is to continue against the version a repair
    // patch just produced, which is newer than what the run originally ran.
    // When no patch landed, a persisted world fix instead authorises a
    // same-version resume — the plan was right; the world has been repaired.
    let plan = storage.plans().load_current(&run.plan_id)?;
    let resume_mode = repair_resume_mode(&storage, &run, &plan)?;

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();
    let (human_tx, mut human_rx) = tokio::sync::mpsc::unbounded_channel::<HumanRequest>();

    let settings = AppSettings::load(&env.paths.settings_path);
    ensure_agent_call_allowed(&plan, &settings)?;
    let inputs = run.inputs.clone();
    let run_id = run.id.clone();
    let config = ExecutorConfig {
        inputs: inputs.clone(),
        timeout_secs: None,
        storage: storage.clone(),
        catalog,
        progress: Some(progress_tx),
        human: Some(human_tx),
        llm_keys: llm_keys_from(&settings),
        // Resume keeps the run's existing source; the executor's resume path
        // does not read this field.
        source: None,
    };

    // The run id is already known (this continues an existing run rather
    // than minting one), so — unlike `run_plan` — there is no need to wait
    // for the first progress event before announcing it.
    env.emit(EngineEvent::RunStarted {
        run_id: run_id.clone(),
        plan: Box::new(plan.clone()),
        inputs,
    });

    let execution = executor::resume_from_repair(plan, config, run, input_overrides, resume_mode);
    tokio::pin!(execution);

    let outcome = loop {
        tokio::select! {
            result = &mut execution => break result,
            Some(progress) = progress_rx.recv() => {
                env.emit(EngineEvent::StepProgress(Box::new(progress)));
            }
            Some(request) = human_rx.recv() => {
                env.emit(EngineEvent::HumanNeeded { run_id: run_id.clone(), request });
            }
        }
    };

    // Drain any progress that raced with completion.
    while let Ok(progress) = progress_rx.try_recv() {
        env.emit(EngineEvent::StepProgress(Box::new(progress)));
    }

    match outcome {
        Ok(run) => {
            // A resume only exists because a repair (patch or world fix)
            // was applied first, so a successful one is a healed run — in
            // addition to the `RunSucceeded` the emit below tallies.
            if run.status == crate::storage::runs::RunStatus::Succeeded {
                crate::telemetry::usage::count(
                    &env.paths.data_dir,
                    &env.paths.settings_path,
                    crate::telemetry::usage::Source::App,
                    crate::telemetry::usage::Action::RunHealed,
                );
            }
            env.emit(EngineEvent::RunFinished { run: Box::new(run) });
        }
        Err(error) => {
            // The executor persists run state before returning an error;
            // surface the recorded run so the UI can show what happened.
            if let Ok(run) = storage.runs().load(&run_id) {
                env.emit(EngineEvent::RunFinished { run: Box::new(run) });
            }
            let error_str = format!("{error}");
            let message = enhance_resume_error_message(&error_str);
            anyhow::bail!("{}", message);
        }
    }
    Ok(())
}

async fn propose_repair(env: &EngineEnv, run_ref: &str) -> anyhow::Result<()> {
    let console = env.attach_activity_console("repair");
    console.info(format!("repair requested for run {run_ref}"));
    let storage = env.storage()?;
    let run = load_run_by_prefix(&storage, run_ref)?;

    if !run.status.is_failed() {
        anyhow::bail!(
            "run '{}' has not failed (status: {}); nothing to repair",
            run.id,
            run.status
        );
    }

    let plan = storage
        .plans()
        .load_version(&run.plan_id, run.plan_version)?;
    let catalog = env.catalog()?;
    let settings = AppSettings::load(&env.paths.settings_path);
    let backend = create_configured_backend(&settings)?;

    let failing_step_id = run
        .failed_step()
        .map(|s| s.step_id.clone())
        .unwrap_or_default();
    env.emit(EngineEvent::RepairStarted {
        run_id: run.id.clone(),
        failing_step_id,
    });
    console.info("analyzing failed run and proposing a repair");

    let proposal = repair::propose_repair(
        &run,
        &plan,
        &backend,
        &catalog,
        &storage,
        Some(EnvProbe::detect().compiler_context()),
    )
    .await
    .map_err(|e| anyhow::anyhow!("repair proposal failed: {e}"))?;

    match proposal {
        repair::RepairProposal::Patch(patch) => {
            console.info("repair patch proposed");
            env.emit(EngineEvent::PatchProposed { patch })
        }
        repair::RepairProposal::WorldFix(fix) => {
            console.info("world-state remediation proposed");
            env.emit(EngineEvent::WorldFixProposed { fix });
        }
    }
    Ok(())
}

/// Approve (when still pending) and apply a stored patch, returning the
/// updated plan.
///
/// Shared by the desktop engine and the MCP server: both perform
/// the identical storage mutation and differ only in the eventing around it,
/// so an agent can close the repair loop headlessly instead of needing the UI.
pub(crate) fn apply_patch_in_storage(
    paths: &DataPaths,
    triggered_by: &'static str,
    patch_id: &str,
) -> anyhow::Result<Plan> {
    paths.mutations.run_named("patch.apply", triggered_by, || {
        let storage = StorageRoot::open(&paths.data_dir)?;
        let patch = storage.patches().load(patch_id)?;

        // Cloned before the match takes ownership of the patch.
        let previous_status = patch.status.clone();
        let approved = match previous_status {
            PatchStatus::Pending | PatchStatus::Approved => {
                let mut approved = patch;
                if approved.status == PatchStatus::Pending {
                    approved.status = PatchStatus::Approved;
                    approved.approved_at = Some(chrono::Utc::now());
                    // `repair::apply_patch` re-reads the patch and requires the
                    // approval to be persisted, so this write cannot wait for
                    // the outcome — it is rolled back below if the apply fails.
                    storage.patches().save(&approved)?;
                }
                approved
            }
            PatchStatus::Applied => {
                anyhow::bail!("patch '{patch_id}' has already been applied")
            }
            PatchStatus::Rejected => {
                anyhow::bail!("patch '{patch_id}' cannot be applied: it was rejected")
            }
        };

        // Read the catalog inside the mutation boundary: validation during the
        // apply must see the same catalog the commit is checked against, and a
        // value captured outside could already be stale.
        let catalog = match paths.catalog_path.exists() {
            true => ToolCatalog::load_from_file(&paths.catalog_path)?,
            false => ToolCatalog::default(),
        };
        let plan = storage
            .plans()
            .load_version(&approved.plan_id, approved.plan_version)?;
        let applied = repair::apply_patch(&approved, plan, &catalog, &storage)
            .map_err(|e| anyhow::anyhow!("failed to apply patch: {e}"));

        if applied.is_err() && previous_status == PatchStatus::Pending {
            // Restore the proposal so it stays actionable. Left `Approved`, a
            // patch that cannot be applied also cannot be rejected — rejection
            // requires `Pending` — and would sit unreachable for ever.
            let mut restored = approved;
            restored.status = PatchStatus::Pending;
            restored.approved_at = None;
            storage.patches().save(&restored)?;
        }
        applied
    })
}

/// Reject a pending patch, recording an optional reason. Counterpart to
/// [`apply_patch_in_storage`], shared with the MCP server.
pub(crate) fn reject_patch_in_storage(
    paths: &DataPaths,
    triggered_by: &'static str,
    patch_id: &str,
    reason: Option<String>,
) -> anyhow::Result<()> {
    paths.mutations.run_named("patch.reject", triggered_by, || {
        let storage = StorageRoot::open(&paths.data_dir)?;
        let patch = storage.patches().load(patch_id)?;
        if patch.status != PatchStatus::Pending {
            anyhow::bail!(
                "patch '{patch_id}' cannot be rejected: expected status pending, got {}",
                patch.status
            );
        }
        let mut rejected = patch;
        rejected.status = PatchStatus::Rejected;
        rejected.rejected_at = Some(chrono::Utc::now());
        rejected.rejection_reason = reason;
        storage.patches().save(&rejected)?;
        Ok(())
    })
}

/// True when `error` is the "no such patch" case from `apply_patch_in_storage`
/// / `reject_patch_in_storage` — the signal that `id` refers to a plan-edit
/// proposal instead, which the caller falls back to.
fn is_patch_not_found(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<crate::error::StorageError>(),
        Some(crate::error::StorageError::NotFound { kind: "patch", .. })
    )
}

fn apply_patch(env: &EngineEnv, id: &str) -> anyhow::Result<()> {
    match apply_patch_in_storage(&env.paths, env.triggered_by(), id) {
        Ok(updated_plan) => {
            env.emit(EngineEvent::PatchResolved {
                patch_id: id.to_owned(),
                message: format!(
                    "Patch applied — plan “{}” updated to v{}.",
                    updated_plan.name, updated_plan.metadata.version
                ),
            });
            env.emit(EngineEvent::PlanLoaded {
                plan: Box::new(updated_plan),
            });
            Ok(())
        }
        Err(error) if is_patch_not_found(&error) => apply_plan_edit(env, id),
        Err(error) => Err(error),
    }
}

fn reject_patch(env: &EngineEnv, patch_id: &str, reason: Option<String>) -> anyhow::Result<()> {
    match reject_patch_in_storage(&env.paths, env.triggered_by(), patch_id, reason.clone()) {
        Ok(()) => {
            env.emit(EngineEvent::PatchResolved {
                patch_id: patch_id.to_owned(),
                message: match reason {
                    Some(r) => format!("Patch rejected — {r}."),
                    None => "Patch rejected.".to_owned(),
                },
            });
            Ok(())
        }
        Err(error) if is_patch_not_found(&error) => reject_plan_edit(env, patch_id, reason),
        Err(error) => Err(error),
    }
}

/// Approve (when still pending) and apply a stored plan-edit proposal,
/// returning the updated plan. Mirrors `apply_patch_in_storage`.
pub(crate) fn apply_plan_edit_in_storage(
    paths: &DataPaths,
    triggered_by: &'static str,
    edit_id: &str,
) -> anyhow::Result<Plan> {
    paths
        .mutations
        .run_named("plan_edit.apply", triggered_by, || {
            let storage = StorageRoot::open(&paths.data_dir)?;
            let edit = storage.plan_edits().load(edit_id)?;

            let previous_status = edit.status.clone();
            let mut approved = match previous_status {
                PatchStatus::Pending | PatchStatus::Approved => {
                    let mut approved = edit;
                    if approved.status == PatchStatus::Pending {
                        approved.status = PatchStatus::Approved;
                        approved.approved_at = Some(chrono::Utc::now());
                        storage.plan_edits().save(&approved)?;
                    }
                    approved
                }
                PatchStatus::Applied => {
                    anyhow::bail!("edit '{edit_id}' has already been applied")
                }
                PatchStatus::Rejected => {
                    anyhow::bail!("edit '{edit_id}' cannot be applied: it was rejected")
                }
            };

            let plan = approved.proposed_plan.clone();
            match storage.plans().save(&plan) {
                Ok(()) => {
                    approved.status = PatchStatus::Applied;
                    storage.plan_edits().save(&approved)?;
                    Ok(plan)
                }
                Err(error) => {
                    // Restore the proposal so it stays actionable, mirroring
                    // `apply_patch_in_storage`'s rollback on a failed apply.
                    if previous_status == PatchStatus::Pending {
                        approved.status = PatchStatus::Pending;
                        approved.approved_at = None;
                        storage.plan_edits().save(&approved)?;
                    }
                    Err(anyhow::anyhow!("failed to save the edited plan: {error}"))
                }
            }
        })
}

/// Reject a pending plan-edit proposal, recording an optional reason.
/// Counterpart to `apply_plan_edit_in_storage`, mirrors `reject_patch_in_storage`.
pub(crate) fn reject_plan_edit_in_storage(
    paths: &DataPaths,
    triggered_by: &'static str,
    edit_id: &str,
    reason: Option<String>,
) -> anyhow::Result<()> {
    paths
        .mutations
        .run_named("plan_edit.reject", triggered_by, || {
            let storage = StorageRoot::open(&paths.data_dir)?;
            let edit = storage.plan_edits().load(edit_id)?;
            if edit.status != PatchStatus::Pending {
                anyhow::bail!(
                    "edit '{edit_id}' cannot be rejected: expected status pending, got {}",
                    edit.status
                );
            }
            let mut rejected = edit;
            rejected.status = PatchStatus::Rejected;
            rejected.rejected_at = Some(chrono::Utc::now());
            rejected.rejection_reason = reason;
            storage.plan_edits().save(&rejected)?;
            Ok(())
        })
}

fn apply_plan_edit(env: &EngineEnv, edit_id: &str) -> anyhow::Result<()> {
    let updated_plan = apply_plan_edit_in_storage(&env.paths, env.triggered_by(), edit_id)?;

    env.emit(EngineEvent::EditResolved {
        edit_id: edit_id.to_owned(),
        message: format!(
            "Edit applied — plan “{}” updated to v{}.",
            updated_plan.name, updated_plan.metadata.version
        ),
    });
    env.emit(EngineEvent::PlanLoaded {
        plan: Box::new(updated_plan),
    });
    Ok(())
}

fn reject_plan_edit(env: &EngineEnv, edit_id: &str, reason: Option<String>) -> anyhow::Result<()> {
    reject_plan_edit_in_storage(&env.paths, env.triggered_by(), edit_id, reason.clone())?;

    env.emit(EngineEvent::EditResolved {
        edit_id: edit_id.to_owned(),
        message: match reason {
            Some(r) => format!("Edit rejected — {r}."),
            None => "Edit rejected.".to_owned(),
        },
    });
    Ok(())
}

fn save_tool(env: &EngineEnv, entry: ToolEntry) -> anyhow::Result<()> {
    if entry.name.trim().is_empty() {
        anyhow::bail!("tool name must not be empty");
    }
    let updated = env
        .paths
        .mutations
        .run_named("catalog.save_tool", env.triggered_by(), || {
            let catalog = env.catalog()?;
            let mut entries: Vec<ToolEntry> = catalog.all().cloned().collect();
            match entries.iter_mut().find(|tool| tool.name == entry.name) {
                Some(existing) => *existing = entry,
                None => entries.push(entry),
            }
            let updated = ToolCatalog::new(entries);
            updated.save_to_file(&env.paths.catalog_path)?;
            Ok(updated)
        })?;
    env.emit(EngineEvent::Catalog(updated.all().cloned().collect()));
    Ok(())
}

fn rename_tool(env: &EngineEnv, old_name: &str, entry: ToolEntry) -> anyhow::Result<()> {
    if entry.name.trim().is_empty() {
        anyhow::bail!("tool name must not be empty");
    }
    let updated =
        env.paths
            .mutations
            .run_named("catalog.rename_tool", env.triggered_by(), || {
                let catalog = env.catalog()?;
                if !catalog.contains(old_name) {
                    anyhow::bail!("tool '{old_name}' no longer exists");
                }
                if old_name != entry.name && catalog.contains(&entry.name) {
                    anyhow::bail!("tool '{}' already exists", entry.name);
                }
                let entries = catalog
                    .all()
                    .filter(|tool| tool.name != old_name)
                    .cloned()
                    .chain(std::iter::once(entry))
                    .collect();
                let updated = ToolCatalog::new(entries);
                updated.save_to_file(&env.paths.catalog_path)?;
                Ok(updated)
            })?;
    env.emit(EngineEvent::Catalog(updated.all().cloned().collect()));
    Ok(())
}

fn delete_tool(env: &EngineEnv, name: &str) -> anyhow::Result<()> {
    let updated =
        env.paths
            .mutations
            .run_named("catalog.delete_tool", env.triggered_by(), || {
                let catalog = env.catalog()?;
                let entries = catalog
                    .all()
                    .filter(|tool| tool.name != name)
                    .cloned()
                    .collect();
                let updated = ToolCatalog::new(entries);
                updated.save_to_file(&env.paths.catalog_path)?;
                Ok(updated)
            })?;
    env.emit(EngineEvent::Catalog(updated.all().cloned().collect()));
    Ok(())
}

/// Handle a "describe what you need" request: ask the compiler backend to
/// invent a starting `ToolEntry` from free text, and emit the outcome as a
/// dedicated event rather than the generic `Failure` bubble, so the MCP view
/// can show the result (or error) inline instead of routing it through chat.
async fn synthesize_tool(env: &EngineEnv, description: String) -> anyhow::Result<()> {
    match synthesize_tool_from_description(env, &description).await {
        Ok(entry) => env.emit(EngineEvent::ToolSynthesized {
            entry: Box::new(entry),
        }),
        Err(error) => env.emit(EngineEvent::ToolSynthesisFailed {
            message: format_error_chain(&error),
        }),
    }
    Ok(())
}

async fn synthesize_tool_from_description(
    env: &EngineEnv,
    description: &str,
) -> anyhow::Result<ToolEntry> {
    if description.trim().is_empty() {
        anyhow::bail!("describe what you need first");
    }

    let settings = AppSettings::load(&env.paths.settings_path);
    let backend = create_configured_backend(&settings).map_err(|_| {
        anyhow::anyhow!(
            "no compiler is configured to generate tools \u{2014} pick one under Settings, or \
             fill in the fields manually instead"
        )
    })?;

    let request = ToolSynthesisRequest {
        name: slugify_tool_name(description),
        description: description.trim().to_owned(),
        input_schema: serde_json::json!({ "type": "object", "properties": {} }),
        output_schema: serde_json::json!({ "type": "object" }),
        kind_hint: None,
        extra_context: Some(EnvProbe::detect().compiler_context()),
    };
    backend
        .synthesize_tool(request)
        .await
        .map_err(|e| anyhow::anyhow!("could not generate a tool from that description: {e}"))
}

/// Turn free text into a `kebab-case` starting name — the synthesis prompt
/// requires *some* name, and the model is instructed to echo it back
/// verbatim. The user reviews and can rename it before saving either way.
fn slugify_tool_name(text: &str) -> String {
    let slug = text
        .split_whitespace()
        .take(6)
        .map(|word| {
            word.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "new-tool".to_owned()
    } else {
        slug
    }
}

// ─── Support tickets ──────────────────────────────────────────────────────────

/// Directory (under the data dir) where collected support reports are kept.
const SUPPORT_TICKET_DIR: &str = "support-tickets";

/// Collect an anonymized support report and a prefilled GitHub issue URL.
///
/// The run (when given) pins the plan version that actually executed;
/// otherwise the plan reference alone is used. The full report is written to
/// disk so the user can review exactly what the issue will contain — the
/// issue URL carries a (possibly truncated) copy of the same text.
fn create_support_ticket(
    env: &EngineEnv,
    run_id: Option<&str>,
    plan_ref: Option<&str>,
) -> anyhow::Result<()> {
    let storage = env.storage()?;
    let run = run_id
        .map(|id| load_run_by_prefix(&storage, id))
        .transpose()?;
    let plan = match (&run, plan_ref) {
        (Some(run), _) => Some(
            storage
                .plans()
                .load_version(&run.plan_id, run.plan_version)?,
        ),
        (None, Some(plan_ref)) => Some(resolve_plan(&storage, plan_ref)?),
        (None, None) => None,
    };

    let settings = AppSettings::load(&env.paths.settings_path);
    let environment = EnvProbe::detect().summary();
    let backend = settings.status_label();
    let info = support::SupportInfo {
        app_version: env!("CARGO_PKG_VERSION"),
        environment: &environment,
        backend: backend.as_deref(),
        plan: plan.as_ref(),
        run: run.as_ref(),
    };
    let report = support::build_report(&info);
    let title = support::issue_title(&info);

    let dir = env.paths.data_dir.join(SUPPORT_TICKET_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("could not create {}: {e}", dir.display()))?;
    let report_path = dir.join(format!(
        "support-{}.md",
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    ));
    std::fs::write(&report_path, &report)
        .map_err(|e| anyhow::anyhow!("could not write {}: {e}", report_path.display()))?;
    let report_path = report_path.display().to_string();

    let issue_url = support::issue_url(&title, &report, &report_path);
    let message = format!(
        "Support report collected — plan structure and run timeline included, all input/output \
         values anonymized and credentials masked. Saved to `{report_path}` for review."
    );
    env.emit(EngineEvent::SupportTicketReady {
        issue_url,
        report_path,
        message,
    });
    Ok(())
}

// ─── Plan export / import ─────────────────────────────────────────────────────

fn export_plan(env: &EngineEnv, plan_ref: &str, dest_path: &Path) -> anyhow::Result<()> {
    let storage = env.storage()?;
    let plan = resolve_plan(&storage, plan_ref)?;
    let catalog = env.catalog()?;

    let (bundle, missing) = PlanBundle::from_plan(&plan, &catalog);
    bundle
        .save_to_file(dest_path)
        .map_err(|e| anyhow::anyhow!("could not write plan bundle: {e}"))?;

    let mut message = format!(
        "Exported \u{201c}{}\u{201d} to `{}` ({} tool reference(s), no credentials or local \
         config included).",
        plan.name,
        dest_path.display(),
        bundle.tools.len()
    );
    if !missing.is_empty() {
        message.push_str(&format!(
            "\n\nNote: {} tool(s) this plan calls aren't in your local catalog either, so \
             they were exported as bare name-only references and may be harder to \
             reconstruct on import: {}",
            missing.len(),
            missing.join(", ")
        ));
    }
    env.emit(EngineEvent::Assistant(message));
    Ok(())
}

async fn import_plan(
    env: &EngineEnv,
    path: &Path,
    conflict_policy: ImportConflictPolicy,
) -> anyhow::Result<()> {
    let bundle = PlanBundle::load_from_file(path)
        .map_err(|e| anyhow::anyhow!("could not read plan bundle: {e}"))?;

    let settings = AppSettings::load(&env.paths.settings_path);
    ensure_agent_call_allowed(&bundle.plan, &settings)?;

    let catalog = env.catalog()?;
    let missing: Vec<&ToolReference> = bundle
        .tools
        .iter()
        .filter(|t| !catalog.contains(&t.name))
        .collect();

    let mut synthesized: Vec<ToolEntry> = Vec::new();
    if !missing.is_empty() {
        let backend = create_configured_backend(&settings).map_err(|_| {
            anyhow::anyhow!(
                "this plan references {} tool(s) not in your local catalog ({}), and no \
                 compiler is configured to generate them \u{2014} pick one under Settings, or add \
                 them manually in MCP Tools first",
                missing.len(),
                missing
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

        for reference in &missing {
            let request = ToolSynthesisRequest {
                name: reference.name.clone(),
                description: reference.description.clone(),
                input_schema: reference.input_schema.clone(),
                output_schema: reference.output_schema.clone(),
                kind_hint: reference.kind_hint.clone(),
                extra_context: Some(EnvProbe::detect().compiler_context()),
            };
            let entry = backend.synthesize_tool(request).await.map_err(|e| {
                anyhow::anyhow!("could not generate tool '{}': {e}", reference.name)
            })?;
            synthesized.push(entry);
        }
    }

    let updated_catalog = if synthesized.is_empty() {
        catalog
    } else {
        env.paths
            .mutations
            .run_named("catalog.import_merge", env.triggered_by(), || {
                let latest = env.catalog()?;
                let mut entries = latest.all().cloned().collect::<Vec<_>>();
                entries.extend(
                    synthesized
                        .iter()
                        .filter(|entry| !latest.contains(&entry.name))
                        .cloned(),
                );
                let merged = ToolCatalog::new(entries);
                merged.save_to_file(&env.paths.catalog_path)?;
                Ok(merged)
            })?
    };

    let plan = bundle.plan;

    let errors = validator::validate(&plan, &updated_catalog);
    if !errors.is_empty() {
        let bullet_list: String = errors
            .iter()
            .map(|e| format!("\u{2022} {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!(
            "the imported plan failed validation against the local tool catalog:\n{bullet_list}"
        );
    }

    let resolution = resolve_import_conflict(
        &env.paths,
        env.triggered_by(),
        normalize(plan),
        conflict_policy,
    )?;
    if resolution.outcome == ImportOutcome::Rejected {
        env.emit(EngineEvent::Assistant(format!(
            "Import of \u{201c}{}\u{201d} was not saved because that name already exists. Choose New version or Copy to continue.",
            resolution.plan.name,
        )));
        return Ok(());
    }
    let plan = resolution.plan;

    let mut message = match resolution.outcome {
        ImportOutcome::Imported => format!("Imported \u{201c}{}\u{201d}.", plan.name),
        ImportOutcome::NewVersion => {
            format!("Imported \u{201c}{}\u{201d} as a new version.", plan.name)
        }
        ImportOutcome::Duplicate => format!("Imported \u{201c}{}\u{201d} as a copy.", plan.name),
        ImportOutcome::Rejected => unreachable!("handled above"),
    };
    if !synthesized.is_empty() {
        message.push_str(&format!(
            "\n\n{} new tool(s) were generated and saved as disabled \u{2014} review and \
             allowlist them under MCP Tools before running this plan: {}",
            synthesized.len(),
            synthesized
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    env.emit(EngineEvent::Assistant(message));
    env.emit(EngineEvent::PlanLoaded {
        plan: Box::new(plan),
    });
    env.emit(EngineEvent::Catalog(
        updated_catalog.all().cloned().collect(),
    ));
    env.emit(EngineEvent::PlanList(list_plans(env)?));
    Ok(())
}

/// Resolve a same-name import and persist it while holding the one app-wide
/// mutation gate. The lookup intentionally lives in this closure: a desktop
/// preflight is only a prompt aid and must never decide persistence.
pub(crate) fn resolve_import_conflict(
    paths: &DataPaths,
    triggered_by: &'static str,
    imported: Plan,
    policy: ImportConflictPolicy,
) -> anyhow::Result<ImportResolution> {
    paths
        .mutations
        .run_named("plan.import_resolve_save", triggered_by, || {
            let storage = StorageRoot::open(&paths.data_dir)?;
            let mut same_name_plan_ids: Vec<String> = storage
                .plans()
                .list()?
                .into_iter()
                .filter(|summary| summary.name.eq_ignore_ascii_case(&imported.name))
                .map(|summary| summary.id)
                .collect();
            same_name_plan_ids.sort();

            if same_name_plan_ids.is_empty() {
                let mut plan = imported;
                plan.metadata = PlanMetadata::new(plan.metadata.intent.clone());
                storage.plans().save(&plan)?;
                return Ok(ImportResolution {
                    outcome: ImportOutcome::Imported,
                    plan,
                    same_name_plan_ids,
                });
            }

            if policy == ImportConflictPolicy::Reject {
                return Ok(ImportResolution {
                    outcome: ImportOutcome::Rejected,
                    plan: imported,
                    same_name_plan_ids,
                });
            }

            if policy == ImportConflictPolicy::Duplicate {
                let mut plan = imported;
                plan.metadata = PlanMetadata::new(plan.metadata.intent.clone());
                storage.plans().save(&plan)?;
                return Ok(ImportResolution {
                    outcome: ImportOutcome::Duplicate,
                    plan,
                    same_name_plan_ids,
                });
            }

            let [plan_id] = same_name_plan_ids.as_slice() else {
                anyhow::bail!(
                    "cannot import '{}' as a new version: its local lineage is ambiguous ({})",
                    imported.name,
                    same_name_plan_ids.join(", ")
                );
            };
            let current = storage.plans().load_current(plan_id)?;
            let version = current.metadata.version.checked_add(1).ok_or_else(|| {
                anyhow::anyhow!(
                    "cannot import '{}' as a new version: local version counter overflowed",
                    imported.name
                )
            })?;
            let mut plan = imported;
            // Only local metadata determines lineage and lifecycle state. In
            // particular, imported parent/status/provenance fields are untrusted.
            plan.metadata = current.metadata;
            plan.metadata.version = version;
            plan.metadata.updated_at = Utc::now();
            storage.plans().save(&plan)?;
            Ok(ImportResolution {
                outcome: ImportOutcome::NewVersion,
                plan,
                same_name_plan_ids,
            })
        })
}

// ─── Update check ──────────────────────────────────────────────────────────────

/// GitHub API endpoint for the newest published release.
const RELEASES_API_URL: &str =
    "https://api.github.com/repos/inxm-ai/matthias-hackathon-inxm/releases/latest";
/// Human-facing releases page, used as a fallback if the API response is
/// missing an `html_url` for some reason.
pub const RELEASES_PAGE_URL: &str = "https://github.com/inxm-ai/matthias-hackathon-inxm/releases";

/// Best-effort GitHub release check. Never surfaces an error to the UI:
/// network hiccups, rate limiting, or a malformed response all just mean
/// "no update found this time".
async fn check_for_updates(env: &EngineEnv) {
    let outcome: anyhow::Result<Option<(String, String)>> = async {
        let client = reqwest::Client::builder()
            .user_agent(format!("inxm-local/{}", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let response = client
            .get(RELEASES_API_URL)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?;
        let body: serde_json::Value = response.json().await?;
        let tag = body
            .get("tag_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("release response missing tag_name"))?;
        let url = body
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or(RELEASES_PAGE_URL)
            .to_owned();
        Ok(is_newer_version(tag, env!("CARGO_PKG_VERSION"))
            .then(|| (tag.trim_start_matches('v').to_owned(), url)))
    }
    .await;

    if let Ok(Some((version, url))) = outcome {
        env.emit(EngineEvent::UpdateAvailable { version, url });
    }
    // Errors (network down, rate-limited, unparsable) are intentionally
    // dropped here — this check must never surface as a chat error.
}

/// Parse a `major.minor.patch` version, tolerating a leading `v` and any
/// non-numeric suffix on the patch component (e.g. `"1.2.3-beta"`).
fn parse_semver(raw: &str) -> Option<(u64, u64, u64)> {
    let raw = raw.trim().trim_start_matches('v');
    let mut parts = raw.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch_field = parts.next()?;
    let patch_digits: String = patch_field
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let patch = patch_digits.parse().ok()?;
    Some((major, minor, patch))
}

/// Whether `latest` (e.g. `"v0.2.0"`) is a newer semver than `current` (e.g.
/// `"0.1.0"`). Unparsable input on either side is treated as "not newer" so a
/// malformed tag can never falsely trigger the update badge.
pub(crate) fn is_newer_version(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// Rewrite known echo configurations seeded by older releases. `cmd.exe`
/// writes redirected output through a legacy code page, and expanding the
/// environment value into command text also lets shell metacharacters execute.
fn legacy_echo_to_utf8_migration(catalog: &ToolCatalog) -> Option<ToolCatalog> {
    let is_legacy_echo = |entry: &ToolEntry| {
        if entry.name != "echo" {
            return false;
        }
        matches!(&entry.config, ToolConfig::Subprocess(c)
            if (c.command == "echo" && c.args.is_empty())
                || (c.command.eq_ignore_ascii_case("cmd")
                    && matches!(c.args.as_slice(), [shell, echo] | [shell, echo, _]
                        if shell.eq_ignore_ascii_case("/C") && echo.eq_ignore_ascii_case("echo"))))
    };
    catalog.all().any(is_legacy_echo).then(|| {
        let entries = catalog
            .all()
            .map(|entry| match &entry.config {
                ToolConfig::Subprocess(c) if is_legacy_echo(entry) => ToolEntry {
                    config: ToolConfig::Subprocess(SubprocessConfig {
                        command: "powershell".to_owned(),
                        args: [
                            "-NoProfile",
                            "-NonInteractive",
                            "-ExecutionPolicy",
                            "Bypass",
                            "-Command",
                            WINDOWS_UTF8_ECHO_SCRIPT,
                        ]
                        .map(str::to_owned)
                        .to_vec(),
                        env: c.env.clone(),
                        working_dir: c.working_dir.clone(),
                    }),
                    ..entry.clone()
                },
                _ => entry.clone(),
            })
            .collect();
        ToolCatalog::new(entries)
    })
}

fn runnable_tool_catalog(catalog: &ToolCatalog) -> Vec<ToolEntry> {
    catalog
        .all()
        .filter(|entry| tool_is_runnable(entry))
        .cloned()
        .collect()
}

fn tool_is_runnable(entry: &ToolEntry) -> bool {
    match &entry.config {
        ToolConfig::Http(_) => true,
        ToolConfig::Mcp(c) => match &c.transport {
            McpTransport::Stdio { server_command, .. } => {
                crate::hostenv::find_on_path(server_command).is_some()
            }
            // Remote endpoints have no local executable dependency.
            McpTransport::StreamableHttp { .. } => true,
        },
        ToolConfig::Subprocess(c) => crate::hostenv::find_on_path(&c.command).is_some(),
    }
}

fn add_native_http_get_migration(catalog: &ToolCatalog) -> Option<ToolCatalog> {
    (!catalog.contains("http-get")).then(|| {
        let mut entries: Vec<ToolEntry> = catalog.all().cloned().collect();
        entries.push(native_http_get_tool());
        ToolCatalog::new(entries)
    })
}

/// Launcher for the reference MCP servers, and the version constraint they need.
const UVX_COMMAND: &str = "uvx";
const UVX_WITH_FLAG: &str = "--with";
const MCP_V1_CONSTRAINT: &str = "mcp<2";

/// Pin `uvx`-launched reference MCP servers to the 1.x client library.
///
/// The seeded catalog carries `--with mcp<2`, but it is only written on first
/// launch — an existing `tools.yaml` is never rewritten. Catalogs seeded before
/// that pin resolve the newest `mcp`, and the 1.x-era servers then die on import
/// with `ImportError: cannot import name 'McpError'`, which reaches the user as
/// a tool that simply stopped working. Rewriting the arguments in place repairs
/// those installs and is a no-op once the constraint is present.
fn add_mcp_v1_constraint_migration(catalog: &ToolCatalog) -> Option<ToolCatalog> {
    let mut changed = false;
    let entries: Vec<ToolEntry> = catalog
        .all()
        .cloned()
        .map(|mut entry| {
            // Only stdio servers are launched by us; a Streamable-HTTP MCP has
            // no command line to pin (see `McpTransport`).
            if let ToolConfig::Mcp(config) = &mut entry.config
                && let McpTransport::Stdio {
                    server_command,
                    server_args,
                    ..
                } = &mut config.transport
                && server_command.as_str() == UVX_COMMAND
                && needs_mcp_v1_constraint(server_args)
            {
                let mut args = vec![UVX_WITH_FLAG.to_owned(), MCP_V1_CONSTRAINT.to_owned()];
                args.extend(server_args.iter().cloned());
                *server_args = args;
                changed = true;
            }
            entry
        })
        .collect();
    changed.then(|| ToolCatalog::new(entries))
}

/// Whether `uvx` arguments still lack any `--with mcp…` constraint. A catalog
/// that pins a different bound (a hand-edited `mcp<3`, say) is left alone: the
/// user's explicit choice outranks the migration.
fn needs_mcp_v1_constraint(server_args: &[String]) -> bool {
    !server_args
        .windows(2)
        .any(|pair| pair[0] == UVX_WITH_FLAG && pair[1].starts_with("mcp"))
}

fn native_http_get_tool() -> ToolEntry {
    ToolEntry {
        name: "http-get".to_owned(),
        description: "Fetch an arbitrary URL with the built-in HTTP client; use this instead of shelling out to curl/wget/Invoke-WebRequest".to_owned(),
        config: ToolConfig::Http(HttpConfig {
            base_url: String::new(),
            method: "GET".to_owned(),
            path_template: "{url}".to_owned(),
            headers: Default::default(),
            timeout_secs: Some(60),
        }),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string" }
            },
            "required": ["url"]
        }),
        output_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "body": { "type": "string" }
            }
        }),
        allowlisted: true,
        timeout_secs: Some(60),
    }
}

// ─── Lookup helpers ───────────────────────────────────────────────────────────

pub(crate) fn list_plan_summaries(storage: &StorageRoot) -> anyhow::Result<Vec<PlanListItem>> {
    storage
        .plans()
        .list()?
        .into_iter()
        .map(|summary| {
            let inputs = storage.plans().load_current(&summary.id)?.inputs;
            Ok(PlanListItem {
                id: summary.id,
                name: summary.name,
                version: summary.version,
                intent: summary.intent,
                inputs,
                updated_at: summary.updated_at,
                status: summary.status,
            })
        })
        .collect()
}

fn list_plans(env: &EngineEnv) -> anyhow::Result<Vec<PlanListItem>> {
    let storage = env.storage()?;
    list_plan_summaries(&storage)
}

pub(crate) fn list_run_summaries(storage: &StorageRoot) -> anyhow::Result<Vec<RunListItem>> {
    let plan_names: std::collections::HashMap<String, String> = storage
        .plans()
        .list()?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    Ok(storage
        .runs()
        .list()?
        .into_iter()
        .map(|r| RunListItem {
            plan_name: plan_names
                .get(&r.plan_id)
                .cloned()
                .unwrap_or_else(|| r.plan_id.clone()),
            id: r.id,
            plan_id: r.plan_id,
            status: r.status,
            started_at: r.started_at,
            finished_at: r.finished_at,
            source: r.source.map(RunSource::from),
        })
        .collect())
}

fn list_runs(env: &EngineEnv) -> anyhow::Result<Vec<RunListItem>> {
    let storage = env.storage()?;
    list_run_summaries(&storage)
}

/// Render an error with its cause chain while skipping causes whose text is
/// already embedded in the message so far. `anyhow`'s alternate (`:#`)
/// formatter appends every `source()` unconditionally, which duplicates
/// errors that already include their source in their own `Display` (e.g.
/// `#[error("... {source}")]` wrappers), yielding "msg: msg".
pub(crate) fn format_error_chain(error: &anyhow::Error) -> String {
    let mut message = error.to_string();
    for cause in error.chain().skip(1) {
        let cause = cause.to_string();
        if !message.contains(&cause) {
            message.push_str(": ");
            message.push_str(&cause);
        }
    }
    message
}

/// App-surface shim for `Plan::resolve_inputs`: callers that build `inputs`
/// programmatically commonly send JSON `null` (or an empty string) to mean
/// "unset" rather than omitting the key. Drop such values for inputs that
/// declare a default, so `resolve_inputs` falls back to the default instead
/// of injecting a value that only fails the tool's own schema at run time.
/// Deliberate falsy values (`false`, `0`, `[]`, `{}`) are kept.
pub(crate) fn drop_inputs_deferring_to_defaults(
    plan: &Plan,
    inputs: indexmap::IndexMap<String, serde_json::Value>,
) -> indexmap::IndexMap<String, serde_json::Value> {
    inputs
        .into_iter()
        .filter(|(name, value)| {
            let has_default = plan
                .inputs
                .iter()
                .any(|input| input.name == *name && input.default.is_some());
            let means_unset = value.is_null() || value.as_str().is_some_and(str::is_empty);
            !(has_default && means_unset)
        })
        .collect()
}

/// Resolve a plan by exact id, id prefix, or exact name (case-insensitive).
pub(crate) fn resolve_plan(storage: &StorageRoot, plan_ref: &str) -> anyhow::Result<Plan> {
    if let Ok(plan) = storage.plans().load_current(plan_ref) {
        return Ok(plan);
    }
    let summaries = storage.plans().list()?;
    let matches: Vec<_> = summaries
        .iter()
        .filter(|p| p.id.starts_with(plan_ref) || p.name.eq_ignore_ascii_case(plan_ref))
        .collect();
    match matches.as_slice() {
        [only] => Ok(storage.plans().load_current(&only.id)?),
        [] => anyhow::bail!("no plan matches '{plan_ref}' — try /plans"),
        several => anyhow::bail!(
            "'{plan_ref}' is ambiguous — matches {}",
            several
                .iter()
                .map(|p| format!("{} ({})", &p.id[..p.id.len().min(8)], p.name))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Load a run by exact id or unique id prefix.
pub(crate) fn load_run_by_prefix(storage: &StorageRoot, run_ref: &str) -> anyhow::Result<Run> {
    if let Ok(run) = storage.runs().load(run_ref) {
        return Ok(run);
    }
    let summaries = storage.runs().list()?;
    let matches: Vec<_> = summaries
        .iter()
        .filter(|r| r.id.starts_with(run_ref))
        .collect();
    match matches.as_slice() {
        [only] => Ok(storage.runs().load(&only.id)?),
        [] => anyhow::bail!("no run matches '{run_ref}' — try /runs"),
        _ => anyhow::bail!("'{run_ref}' is ambiguous — give more characters of the run id"),
    }
}

/// Decide how a failed run may be resumed.
///
/// A plan version newer than the run's means a repair patch was applied —
/// resume against the patched plan. An unchanged version is only resumable
/// when repair diagnosed the failure as a world-state problem and persisted a
/// `WorldFix` for this run: the plan was right, the human has (presumably)
/// repaired the environment, and re-running the identical step is the point.
pub(crate) fn repair_resume_mode(
    storage: &StorageRoot,
    run: &Run,
    plan: &Plan,
) -> anyhow::Result<executor::RepairResumeMode> {
    if plan.metadata.version > run.plan_version {
        return Ok(executor::RepairResumeMode::PatchedPlan);
    }
    let authorising_fix = storage
        .world_fixes()
        .latest_for_run(&run.id)?
        .filter(|fix| fix.plan_version == run.plan_version);
    match authorising_fix {
        Some(_) => Ok(executor::RepairResumeMode::WorldFixed),
        None => anyhow::bail!(
            "run '{}' cannot be resumed: its plan is still v{} (no repair patch applied) \
             and no world fix was proposed for it — run `/repair {}` first",
            run.id,
            run.plan_version,
            run.id
        ),
    }
}

/// Detect if an error message indicates a transport-level or external endpoint
/// failure (DNS, connection refused, timeout, TLS, or repair guidance marker).
///
/// Reuses the repair classifier's transport pattern table so this hint and
/// the classifier's `ExternalEndpointDown` verdict cannot drift apart.
fn is_transport_error(error_text: &str) -> bool {
    let lowered = error_text.to_lowercase();
    error_text.contains(crate::repair::failure_packet::REPAIR_GUIDANCE_MARKER)
        || crate::repair::classifier::TRANSPORT_FAILURE_PATTERNS
            .iter()
            .any(|pattern| lowered.contains(pattern))
}

/// Enhance a resume failure error message with a hint to use `/repair` if the
/// failure is transport/external level.
fn enhance_resume_error_message(error_text: &str) -> String {
    if is_transport_error(error_text) {
        format!(
            "resume failed: {}\n\n\
             The error appears to be from an external endpoint — try `/repair` \
             to swap the endpoint or add a fallback step.",
            error_text
        )
    } else {
        format!("resume failed: {}", error_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::catalog::McpConfig;

    fn test_env(paths: DataPaths) -> EngineEnv {
        let (evt_tx, _events) = std::sync::mpsc::channel();
        EngineEnv {
            paths,
            evt_tx,
            repaint: no_repaint(),
            session_id: None,
            oauth_cancellations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            run_cancellations: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            activities: ActivityRegistry::default(),
            activity_id: None,
        }
    }

    fn named_http_tool(name: &str) -> ToolEntry {
        ToolEntry {
            name: name.to_owned(),
            ..native_http_get_tool()
        }
    }

    fn empty_plan_v1() -> Plan {
        Plan {
            metadata: PlanMetadata::new(None),
            name: "noop".to_owned(),
            description: None,
            inputs: vec![],
            config: indexmap::IndexMap::new(),
            steps: vec![],
            outputs: vec![],
        }
    }

    #[test]
    fn plan_edit_is_saved_only_after_apply_and_can_be_rejected_instead() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = DataPaths::at(tmp.path().to_owned());
        let storage = StorageRoot::open(&paths.data_dir).unwrap();

        let previous = empty_plan_v1();
        storage.plans().save(&previous).unwrap();
        let mut proposed = previous.clone();
        proposed.metadata.version += 1;
        proposed.name = "renamed".to_owned();

        let edit = PlanEdit::new(
            previous.metadata.id.clone(),
            previous.metadata.version,
            "rename the plan",
            previous.clone(),
            proposed.clone(),
        );
        storage.plan_edits().save(&edit).unwrap();

        // Proposing an edit must not touch the plan store.
        assert_eq!(
            storage
                .plans()
                .load_current(&previous.metadata.id)
                .unwrap()
                .name,
            "noop"
        );

        let applied = apply_plan_edit_in_storage(&paths, "test", &edit.id).unwrap();
        assert_eq!(applied.name, "renamed");
        assert_eq!(
            storage
                .plans()
                .load_current(&previous.metadata.id)
                .unwrap()
                .name,
            "renamed"
        );
        assert_eq!(
            storage.plan_edits().load(&edit.id).unwrap().status,
            PatchStatus::Applied
        );

        // A second apply of an already-applied edit is rejected.
        assert!(apply_plan_edit_in_storage(&paths, "test", &edit.id).is_err());

        // A pending edit can be rejected instead of applied.
        let other = PlanEdit::new(
            previous.metadata.id.clone(),
            previous.metadata.version,
            "another change",
            previous.clone(),
            proposed,
        );
        storage.plan_edits().save(&other).unwrap();
        reject_plan_edit_in_storage(&paths, "test", &other.id, Some("not needed".to_owned()))
            .unwrap();
        let rejected = storage.plan_edits().load(&other.id).unwrap();
        assert_eq!(rejected.status, PatchStatus::Rejected);
        assert_eq!(rejected.rejection_reason, Some("not needed".to_owned()));
        assert!(apply_plan_edit_in_storage(&paths, "test", &other.id).is_err());
    }

    #[test]
    fn concurrent_new_version_imports_share_one_lookup_and_save_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = DataPaths::at(tmp.path().to_owned());
        let initial = resolve_import_conflict(
            &paths,
            "test",
            empty_plan_v1(),
            ImportConflictPolicy::Reject,
        )
        .unwrap();
        assert_eq!(initial.outcome, ImportOutcome::Imported);

        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles = (0..2)
            .map(|_| {
                let paths = paths.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    resolve_import_conflict(
                        &paths,
                        "test",
                        empty_plan_v1(),
                        ImportConflictPolicy::NewVersion,
                    )
                    .unwrap()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let mut versions = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().plan.metadata.version)
            .collect::<Vec<_>>();
        versions.sort();
        assert_eq!(versions, vec![2, 3]);
        let stored = StorageRoot::open(&paths.data_dir).unwrap();
        assert_eq!(stored.plans().list().unwrap()[0].version, 3);
    }

    #[test]
    fn new_version_uses_only_local_lineage_and_lifecycle_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = DataPaths::at(tmp.path().to_owned());
        let mut local = empty_plan_v1();
        local.metadata.status = PlanStatus::Draft;
        local.metadata.parent_plan_id = Some("local-parent".to_owned());
        local.metadata.parent_version = Some(7);
        let initial_id = local.metadata.id.clone();
        StorageRoot::open(&paths.data_dir)
            .unwrap()
            .plans()
            .save(&local)
            .unwrap();
        let mut imported = empty_plan_v1();
        imported.metadata.status = PlanStatus::Published;
        imported.metadata.parent_plan_id = Some("foreign-parent".to_owned());
        let resolution =
            resolve_import_conflict(&paths, "test", imported, ImportConflictPolicy::NewVersion)
                .unwrap();
        assert_eq!(resolution.plan.metadata.id, initial_id);
        assert_eq!(resolution.plan.metadata.version, 2);
        assert_eq!(resolution.plan.metadata.status, PlanStatus::Draft);
        assert_eq!(
            resolution.plan.metadata.parent_plan_id.as_deref(),
            Some("local-parent")
        );
        assert_eq!(resolution.plan.metadata.parent_version, Some(7));
    }

    #[test]
    fn case_insensitive_ambiguous_lineage_fails_without_a_new_record() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = DataPaths::at(tmp.path().to_owned());
        resolve_import_conflict(
            &paths,
            "test",
            empty_plan_v1(),
            ImportConflictPolicy::Reject,
        )
        .unwrap();
        let mut second = empty_plan_v1();
        second.name = "NOOP".to_owned();
        resolve_import_conflict(&paths, "test", second, ImportConflictPolicy::Duplicate).unwrap();

        let mut imported = empty_plan_v1();
        imported.name = "NoOp".to_owned();
        let error =
            resolve_import_conflict(&paths, "test", imported, ImportConflictPolicy::NewVersion)
                .unwrap_err();
        assert!(error.to_string().contains("ambiguous"));
        assert_eq!(
            StorageRoot::open(&paths.data_dir)
                .unwrap()
                .plans()
                .list()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn version_counter_overflow_fails_before_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = DataPaths::at(tmp.path().to_owned());
        let mut current = empty_plan_v1();
        current.metadata.id = "overflow-plan".to_owned();
        current.metadata.version = u32::MAX;
        let plan_dir = paths.data_dir.join("plans").join(&current.metadata.id);
        std::fs::create_dir_all(&plan_dir).unwrap();
        std::fs::write(
            plan_dir.join("current.json"),
            serde_json::to_string(&current).unwrap(),
        )
        .unwrap();

        let error = resolve_import_conflict(
            &paths,
            "test",
            empty_plan_v1(),
            ImportConflictPolicy::NewVersion,
        )
        .unwrap_err();
        assert!(error.to_string().contains("overflowed"));
        assert_eq!(
            StorageRoot::open(&paths.data_dir)
                .unwrap()
                .plans()
                .list()
                .unwrap()
                .len(),
            1
        );
    }

    /// Errors whose `Display` already embeds their source must not
    /// be rendered as "msg: msg", while genuine context chains still expand.
    #[test]
    fn format_error_chain_skips_causes_already_in_the_message() {
        #[derive(Debug, thiserror::Error)]
        #[error("schedule save failed: {source}")]
        struct EmbedsSource {
            #[source]
            source: std::io::Error,
        }

        let embedding = anyhow::Error::new(EmbedsSource {
            source: std::io::Error::other("disk full"),
        });
        assert_eq!(
            format_error_chain(&embedding),
            "schedule save failed: disk full"
        );

        use anyhow::Context as _;
        let contextual = anyhow::Result::<()>::Err(anyhow::anyhow!("boom"))
            .context("running step")
            .unwrap_err();
        assert_eq!(format_error_chain(&contextual), "running step: boom");
    }

    /// Explicit `null` / `""` must defer to a declared default,
    /// while deliberately provided values — including falsy ones — still win.
    #[test]
    fn null_and_empty_inputs_defer_to_defaults_but_real_values_win() {
        let mut plan = empty_plan_v1();
        plan.inputs = vec![
            crate::plan::types::PlanInput {
                name: "latitude".to_owned(),
                description: None,
                value_type: "string".to_owned(),
                required: false,
                default: Some(serde_json::json!("52.50")),
                input_kind: crate::plan::types::InputKind::Value,
            },
            crate::plan::types::PlanInput {
                name: "verbose".to_owned(),
                description: None,
                value_type: "boolean".to_owned(),
                required: false,
                default: Some(serde_json::json!(true)),
                input_kind: crate::plan::types::InputKind::Value,
            },
            crate::plan::types::PlanInput {
                name: "note".to_owned(),
                description: None,
                value_type: "string".to_owned(),
                required: false,
                default: None,
                input_kind: crate::plan::types::InputKind::Value,
            },
        ];

        // null and "" for a defaulted input are dropped, so resolve_inputs
        // falls back to the default instead of injecting them.
        for unset in [serde_json::Value::Null, serde_json::json!("")] {
            let provided = [("latitude".to_owned(), unset)].into_iter().collect();
            let resolved = plan
                .resolve_inputs(&drop_inputs_deferring_to_defaults(&plan, provided))
                .unwrap();
            assert_eq!(resolved["latitude"], serde_json::json!("52.50"));
        }

        // Real values — including falsy `false` — are kept verbatim.
        let provided = [
            ("latitude".to_owned(), serde_json::json!("48.14")),
            ("verbose".to_owned(), serde_json::json!(false)),
        ]
        .into_iter()
        .collect();
        let resolved = plan
            .resolve_inputs(&drop_inputs_deferring_to_defaults(&plan, provided))
            .unwrap();
        assert_eq!(resolved["latitude"], serde_json::json!("48.14"));
        assert_eq!(resolved["verbose"], serde_json::json!(false));

        // Without a default there is nothing to fall back to: the value
        // passes through unchanged (pre-#63 behavior preserved).
        let provided = [("note".to_owned(), serde_json::json!(""))]
            .into_iter()
            .collect();
        let filtered = drop_inputs_deferring_to_defaults(&plan, provided);
        assert_eq!(filtered["note"], serde_json::json!(""));
    }

    #[test]
    fn resume_mode_requires_a_newer_plan_or_a_matching_world_fix() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(tmp.path()).unwrap();
        let mut plan = empty_plan_v1();
        let run = Run::new(plan.metadata.id.clone(), 1);

        // Unchanged plan, no world fix: resuming would just fail identically.
        let error = repair_resume_mode(&storage, &run, &plan).unwrap_err();
        assert!(error.to_string().contains("run `/repair"), "got: {error}");

        // A world fix for another run does not authorise this one.
        storage
            .world_fixes()
            .save(&WorldFix::new(
                &plan.metadata.id,
                1,
                "other-run",
                "step",
                "diagnosis",
                vec![],
            ))
            .unwrap();
        assert!(repair_resume_mode(&storage, &run, &plan).is_err());

        // A world fix for this run at the run's version authorises a
        // same-version resume.
        storage
            .world_fixes()
            .save(&WorldFix::new(
                &plan.metadata.id,
                1,
                &run.id,
                "step",
                "the world was broken",
                vec![],
            ))
            .unwrap();
        assert_eq!(
            repair_resume_mode(&storage, &run, &plan).unwrap(),
            executor::RepairResumeMode::WorldFixed
        );

        // A newer plan version always means patched-plan semantics.
        plan.metadata.version = 2;
        assert_eq!(
            repair_resume_mode(&storage, &run, &plan).unwrap(),
            executor::RepairResumeMode::PatchedPlan
        );
    }

    #[test]
    fn rewrite_phantom_surfaces_replaces_known_phrases_preserving_casing() {
        assert_eq!(
            rewrite_phantom_surfaces("see the Plan View above the chat"),
            "see the Plan card above the chat"
        );
        assert_eq!(
            rewrite_phantom_surfaces("open the plan view to check progress"),
            "open the plan card to check progress"
        );
        assert_eq!(
            rewrite_phantom_surfaces("check the PLAN VIEW section"),
            "check the PLAN CARD section"
        );
    }

    #[test]
    fn rewrite_phantom_surfaces_is_a_no_op_when_no_phantom_phrase_present() {
        let text = "the plan card above this chat now shows your compiled plan";
        assert_eq!(rewrite_phantom_surfaces(text), text);
    }

    #[test]
    fn planning_context_exposes_agent_capability_only_when_enabled_and_supported() {
        let mut settings = AppSettings {
            backend: BackendChoice::ClaudeCode,
            ..AppSettings::default()
        };
        let disabled = planning_context(&settings);
        assert!(disabled.contains("AGENT_CALL is not enabled"));

        settings.experimental_agent_calls = true;
        let enabled = planning_context(&settings);
        assert!(enabled.contains("AGENT_CALL is enabled"));
        assert!(enabled.contains("Treat it as naturally available"));
        assert!(enabled.contains("do not ask the user whether to use Claude"));
        assert!(enabled.contains("MUST have step_kind `agent_call`, never `code_call`"));
    }

    #[test]
    fn enabled_agent_capability_normalizes_agent_cli_code_outline() {
        let mut design = compiler::SolutionDesign {
            title: "Rust feature".into(),
            summary: "Implement and verify".into(),
            recommended_tools: vec![],
            execution_outline: vec![compiler::OutlineStep {
                name: "Implement Feature".into(),
                step_kind: "code_call".into(),
                description: "Invoke the available coding-agent CLI (codex/claude) to add or modify source files under project_path.".into(),
            }],
        };
        let settings = AppSettings {
            backend: BackendChoice::ClaudeCode,
            experimental_agent_calls: true,
            ..AppSettings::default()
        };

        normalize_agent_outline(&mut design, &settings);

        assert_eq!(design.execution_outline[0].step_kind, "agent_call");
        assert!(
            design.execution_outline[0]
                .description
                .contains("configured AGENT_CALL backend")
        );
        assert!(!design.execution_outline[0].description.contains("claude"));
    }

    #[test]
    fn disabled_agent_capability_leaves_code_outline_unchanged() {
        let mut design = compiler::SolutionDesign {
            title: "Rust feature".into(),
            summary: "Implement it".into(),
            recommended_tools: vec![],
            execution_outline: vec![compiler::OutlineStep {
                name: "Implement with agent".into(),
                step_kind: "code_call".into(),
                description: "Invoke Codex to edit source files.".into(),
            }],
        };

        normalize_agent_outline(&mut design, &AppSettings::default());

        assert_eq!(design.execution_outline[0].step_kind, "code_call");
    }

    #[test]
    fn internal_execution_choices_are_not_user_clarifications() {
        assert!(is_internal_execution_choice_question(
            "Should the plan shell out to Claude or Codex, or use PROMPT_CALL?"
        ));
        assert!(is_internal_execution_choice_question(
            "Should this use AGENT_CALL or CODE_CALL?"
        ));
        assert!(!is_internal_execution_choice_question(
            "Should failed checks block publication or only produce a warning?"
        ));
    }

    #[test]
    fn is_newer_version_detects_a_newer_release() {
        assert!(is_newer_version("v0.2.0", "0.1.0"));
        assert!(is_newer_version("v1.0.0", "0.9.9"));
        assert!(is_newer_version("v0.1.10", "0.1.9"));
    }

    #[test]
    fn is_newer_version_rejects_equal_or_older() {
        assert!(!is_newer_version("v0.1.0", "0.1.0"));
        assert!(!is_newer_version("v0.1.0", "0.2.0"));
        assert!(!is_newer_version("v0.9.0", "0.9.1"));
    }

    #[test]
    fn is_newer_version_tolerates_prerelease_suffixes_and_missing_v_prefix() {
        assert!(is_newer_version("0.2.0", "0.1.0"));
        assert!(is_newer_version("v0.2.0-beta", "0.1.0"));
    }

    #[test]
    fn is_newer_version_treats_unparsable_input_as_not_newer() {
        assert!(!is_newer_version("not-a-version", "0.1.0"));
        assert!(!is_newer_version("v1.2", "0.1.0"));
        assert!(!is_newer_version("v1.2.3", "garbage"));
    }

    #[test]
    fn cron_candidate_prefers_a_valid_correction_in_chatty_output() {
        let raw = "0 14:55 * * *\n\nWait, let me correct:\n\n55 14 * * *";
        assert_eq!(extract_cron_candidate(raw).as_deref(), Some("55 14 * * *"));
    }

    #[test]
    fn cron_candidate_accepts_code_fenced_single_line_output() {
        assert_eq!(
            extract_cron_candidate("```\n55 14 * * *\n```").as_deref(),
            Some("55 14 * * *")
        );
    }

    #[test]
    fn insight_response_parses_an_answer_with_a_suggested_action() {
        let raw = r#"{"answer": "It last ran yesterday.", "suggested_action": {"label": "Run it again", "command": "/run"}}"#;
        let (answer, action) = parse_insight_response(raw);
        assert_eq!(answer, "It last ran yesterday.");
        let action = action.expect("suggested_action was present");
        assert_eq!(action.label, "Run it again");
        assert_eq!(action.command, "/run");
    }

    #[test]
    fn insight_response_accepts_a_code_fenced_envelope_and_a_null_action() {
        let raw =
            "```json\n{\"answer\": \"Three plans are saved.\", \"suggested_action\": null}\n```";
        let (answer, action) = parse_insight_response(raw);
        assert_eq!(answer, "Three plans are saved.");
        assert!(action.is_none());
    }

    #[test]
    fn insight_response_falls_back_to_raw_text_when_not_json() {
        let (answer, action) = parse_insight_response("Sorry, I don't have that information.");
        assert_eq!(answer, "Sorry, I don't have that information.");
        assert!(action.is_none());
    }

    #[test]
    fn schedules_are_sorted_by_next_run_with_paused_items_last() {
        let mut schedules = vec![
            ("paused", None),
            ("later", Some(30)),
            ("next", Some(10)),
            ("last", Some(50)),
        ];
        sort_by_next_run(&mut schedules);
        assert_eq!(
            schedules
                .into_iter()
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            vec!["next", "later", "last", "paused"]
        );
    }

    #[test]
    fn spec_compile_context_layers_host_spec_conversation_and_design() {
        let spec = compiler::SpecDraft {
            desired_outcome: "log the BTC price hourly".to_owned(),
            acceptance_criteria: vec!["price is in USD".to_owned()],
            inputs: vec![compiler::SpecInput {
                name: "output_path".to_owned(),
                description: "Destination for the price log".to_owned(),
                value_type: "string".to_owned(),
                required: true,
                default: None,
                input_kind: crate::plan::types::InputKind::OutputFilePath,
            }],
        };
        let design = compiler::SolutionDesign {
            title: "BTC logger".to_owned(),
            summary: "fetch and append".to_owned(),
            recommended_tools: vec![],
            execution_outline: vec![],
        };
        let conversation = vec![
            compiler::SpecTurn {
                role: "user".to_owned(),
                content: "log btc".to_owned(),
            },
            compiler::SpecTurn {
                role: "assistant".to_owned(),
                content: "which currency?".to_owned(),
            },
        ];

        let context = spec_compile_context("HOST", &spec, Some(&design), &conversation);
        let host_at = context.find("HOST").unwrap();
        let spec_at = context.find("## Refined specification").unwrap();
        let conversation_at = context.find("## Clarification conversation").unwrap();
        let design_at = context.find("## Approved solution design").unwrap();
        assert!(host_at < spec_at && spec_at < conversation_at && conversation_at < design_at);
        assert!(context.contains("- price is in USD"));
        assert!(context.contains(
            "`output_path` (string, required, input_kind output_file_path, default null)"
        ));
        assert!(context.contains("never collect them with HUMAN_INTERACTION"));
        assert!(context.contains("assistant: which currency?"));
        assert!(context.contains("# BTC logger"));
    }

    #[test]
    fn spec_compile_context_omits_conversation_and_design_for_simple_prompts() {
        let spec = compiler::SpecDraft {
            desired_outcome: "fetch the BTC price".to_owned(),
            acceptance_criteria: vec![],
            inputs: vec![],
        };
        let single_turn = vec![compiler::SpecTurn {
            role: "user".to_owned(),
            content: "fetch the BTC price".to_owned(),
        }];
        let context = spec_compile_context("HOST", &spec, None, &single_turn);
        assert!(!context.contains("## Clarification conversation"));
        assert!(!context.contains("## Approved solution design"));
        assert!(context.contains("Desired outcome: fetch the BTC price"));
    }

    #[test]
    fn tray_pause_preserves_individual_schedule_state() {
        let now = chrono::Local::now();
        let schedules = vec![schedule_store::Schedule {
            id: "schedule-1".to_owned(),
            plan_id: "plan-1".to_owned(),
            cron: "* * * * * *".to_owned(),
            enabled: true,
            inputs: Default::default(),
            created_at: chrono::Utc::now(),
            last_run: None,
        }];

        assert!(
            due_schedules(
                schedules.clone(),
                true,
                now,
                now + chrono::Duration::seconds(2)
            )
            .is_empty()
        );
        assert!(schedules[0].enabled);
        assert_eq!(
            due_schedules(schedules, false, now, now + chrono::Duration::seconds(2)).len(),
            1
        );
    }

    #[test]
    fn claiming_due_schedule_is_durable_and_active_run_does_not_overlap() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("schedules.json");
        let now = chrono::Local::now();
        let schedule = schedule_store::Schedule {
            id: "schedule-1".to_owned(),
            plan_id: "plan-1".to_owned(),
            cron: "* * * * * *".to_owned(),
            enabled: true,
            inputs: Default::default(),
            created_at: chrono::Utc::now(),
            last_run: None,
        };
        schedule_store::save(&path, std::slice::from_ref(&schedule)).unwrap();

        let claimed = claim_due_schedules(
            &path,
            false,
            &Default::default(),
            now,
            now + chrono::Duration::seconds(2),
        )
        .unwrap();
        assert_eq!(claimed.len(), 1);
        assert!(schedule_store::load(&path).unwrap()[0].last_run.is_some());

        let active = std::collections::HashSet::from(["schedule-1".to_owned()]);
        let skipped = claim_due_schedules(
            &path,
            false,
            &active,
            now + chrono::Duration::seconds(2),
            now + chrono::Duration::seconds(4),
        )
        .unwrap();
        assert!(skipped.is_empty());
    }

    #[test]
    fn legacy_settings_deserialize_with_new_connection_fields_defaulted() {
        let settings: AppSettings = serde_json::from_str(
            r#"{"backend":"open_ai","model":"gpt-4o","api_key":"secret","max_tokens":2048,"mcp_port":39387}"#,
        )
        .unwrap();
        assert_eq!(settings.backend, BackendChoice::OpenAi);
        assert!(settings.api_base.is_empty());
        assert!(settings.executable.is_empty());
        assert_eq!(settings.keep_running_in_background, None);
        assert!(!settings.schedules_paused);
        assert!(!settings.custom_cli_agentic);
        assert!(!settings.experimental_agent_calls);
        let explicit_opt_out: AppSettings =
            serde_json::from_str(r#"{"keep_running_in_background":false}"#).unwrap();
        assert_eq!(explicit_opt_out.keep_running_in_background, Some(false));
        let profile = settings.llm_profile().unwrap();
        assert_eq!(profile.protocol, LlmProtocol::OpenAiChat);
        assert_eq!(profile.max_tokens, Some(2048));
    }

    #[test]
    fn auto_mode_is_off_for_settings_written_before_it_existed() {
        // Skipping the design approval is never inherited from an upgrade.
        let legacy: AppSettings = serde_json::from_str(r#"{"backend":"claude"}"#).unwrap();
        assert!(!legacy.auto_mode);
        assert!(!AppSettings::default().auto_mode);

        let opted_in = AppSettings {
            auto_mode: true,
            ..AppSettings::default()
        };
        let round_tripped: AppSettings =
            serde_json::from_str(&serde_json::to_string(&opted_in).unwrap()).unwrap();
        assert!(round_tripped.auto_mode);
    }

    #[test]
    fn agent_call_requires_opt_in_and_an_agent_backend() {
        for backend in [BackendChoice::Codex, BackendChoice::ClaudeCode] {
            let mut settings = AppSettings {
                backend,
                ..AppSettings::default()
            };
            assert!(!settings.supports_agent_call());
            settings.experimental_agent_calls = true;
            assert!(settings.supports_agent_call());
        }

        for backend in [
            BackendChoice::Auto,
            BackendChoice::Claude,
            BackendChoice::OpenAi,
            BackendChoice::GoogleVertex,
            BackendChoice::OpenAiCompatible,
            BackendChoice::AnthropicCompatible,
        ] {
            let settings = AppSettings {
                backend,
                experimental_agent_calls: true,
                ..AppSettings::default()
            };
            assert!(
                !settings.supports_agent_call(),
                "{backend:?} is completion-only"
            );
        }

        let mut custom = AppSettings {
            backend: BackendChoice::CustomCli,
            experimental_agent_calls: true,
            ..AppSettings::default()
        };
        assert!(!custom.supports_agent_call());
        custom.custom_cli_agentic = true;
        assert!(custom.supports_agent_call());
    }

    #[test]
    fn edit_history_uses_newest_matching_runs_across_plan_versions() {
        let temp = tempfile::tempdir().unwrap();
        let storage = StorageRoot::open(temp.path()).unwrap();
        let base = chrono::Utc::now();

        for version in 1..=7 {
            let mut run = Run::new("plan-1", version);
            run.started_at = base + chrono::Duration::seconds(i64::from(version));
            run.status = executor::RunStatus::Succeeded;
            run.outputs.insert(
                "result".to_owned(),
                serde_json::json!(format!("version-{version}")),
            );
            storage.runs().save(&run).unwrap();
        }

        let mut unrelated = Run::new("other-plan", 99);
        unrelated.started_at = base + chrono::Duration::hours(1);
        unrelated.status = executor::RunStatus::Succeeded;
        unrelated
            .outputs
            .insert("result".to_owned(), serde_json::json!("unrelated"));
        storage.runs().save(&unrelated).unwrap();

        let history = recent_edit_run_history(&storage, "plan-1").unwrap();

        assert_eq!(history.len(), EDIT_RUN_HISTORY_LIMIT);
        assert_eq!(
            history
                .iter()
                .map(|run| run.plan_version)
                .collect::<Vec<_>>(),
            vec![7, 6, 5, 4, 3]
        );
        assert!(history.iter().all(|run| run.run_id != unrelated.id));
        assert_eq!(history[0].outputs["result"], "version-7");
    }

    #[test]
    fn compile_request_only_allows_agent_call_when_supported() {
        let catalog = ToolCatalog::default();
        let disabled = compile_request(&catalog, &AppSettings::default(), "x".into(), None);
        assert!(!disabled.allowed_step_types.contains(&StepType::AgentCall));

        let enabled_settings = AppSettings {
            backend: BackendChoice::Codex,
            experimental_agent_calls: true,
            ..AppSettings::default()
        };
        let enabled = compile_request(&catalog, &enabled_settings, "x".into(), None);
        assert!(enabled.allowed_step_types.contains(&StepType::AgentCall));
    }

    /// An existing user's `settings.json` predates the onboarding flag, so
    /// the field is simply absent from the JSON. That must deserialize to
    /// `true` ("already onboarded") — never `false`, which would make the
    /// first-run assistant pop up for someone who has used the app for
    /// months. Only a fresh `AppSettings::default()` (no file at all) should
    /// produce `false`.
    #[test]
    fn legacy_settings_without_onboarding_field_are_treated_as_already_onboarded() {
        let settings: AppSettings =
            serde_json::from_str(r#"{"backend":"open_ai","model":"gpt-4o"}"#).unwrap();
        assert!(settings.onboarding_completed);
    }

    #[test]
    fn fresh_default_settings_have_not_completed_onboarding() {
        assert!(!AppSettings::default().onboarding_completed);
    }

    /// Telemetry must never turn itself on for an install that was never
    /// asked: a settings file that predates the field, and a fresh default,
    /// both resolve to `None` — which `crate::telemetry` treats as off.
    #[test]
    fn telemetry_is_unset_for_legacy_settings_and_fresh_defaults() {
        let legacy: AppSettings =
            serde_json::from_str(r#"{"backend":"open_ai","model":"gpt-4o"}"#).unwrap();
        assert_eq!(legacy.telemetry_enabled, None);
        assert_eq!(AppSettings::default().telemetry_enabled, None);
        assert!(!crate::telemetry::enabled(legacy.telemetry_enabled));
    }

    #[test]
    fn default_profile_uses_compiler_max_token_budget() {
        let settings = AppSettings {
            backend: BackendChoice::OpenAiCompatible,
            model: "test-model".to_owned(),
            api_base: "http://localhost:11434/v1".to_owned(),
            ..AppSettings::default()
        };

        assert_eq!(
            settings.llm_profile().unwrap().max_tokens,
            Some(DEFAULT_MAX_TOKENS)
        );
    }

    #[test]
    fn custom_openai_profile_allows_local_endpoint_without_key() {
        let settings = AppSettings {
            backend: BackendChoice::OpenAiCompatible,
            model: "qwen2.5:7b".to_owned(),
            api_base: "http://localhost:11434/v1".to_owned(),
            ..AppSettings::default()
        };
        let profile = settings.llm_profile().unwrap();
        assert_eq!(profile.protocol, LlmProtocol::OpenAiChat);
        assert_eq!(profile.auth, LlmAuth::None);
        assert_eq!(profile.model, "qwen2.5:7b");
    }

    #[test]
    fn account_connection_uses_configured_cli() {
        let settings = AppSettings {
            backend: BackendChoice::ClaudeCode,
            model: "opus".to_owned(),
            executable: "/opt/bin/claude".to_owned(),
            ..AppSettings::default()
        };
        let profile = settings.llm_profile().unwrap();
        assert_eq!(profile.protocol, LlmProtocol::ClaudeCli);
        assert_eq!(profile.auth, LlmAuth::None);
        assert_eq!(profile.executable, "/opt/bin/claude");
    }

    #[test]
    fn changing_account_backend_clears_the_previous_cli_executable() {
        let mut settings = AppSettings {
            backend: BackendChoice::ClaudeCode,
            executable: "/opt/bin/claude".to_owned(),
            ..AppSettings::default()
        };

        settings.select_backend(BackendChoice::Codex);

        assert_eq!(settings.backend, BackendChoice::Codex);
        assert!(settings.executable.is_empty());
    }

    #[test]
    fn codex_profile_ignores_stale_claude_executable_from_older_settings() {
        let settings = AppSettings {
            backend: BackendChoice::Codex,
            executable: "/home/user/.local/bin/claude".to_owned(),
            ..AppSettings::default()
        };

        let profile = settings.llm_profile().unwrap();

        assert_eq!(profile.protocol, LlmProtocol::CodexCli);
        assert!(profile.executable.is_empty());
    }

    #[test]
    fn account_profile_keeps_custom_named_cli_wrapper() {
        let settings = AppSettings {
            backend: BackendChoice::Codex,
            executable: "/opt/bin/company-agent".to_owned(),
            ..AppSettings::default()
        };

        assert_eq!(
            settings.llm_profile().unwrap().executable,
            "/opt/bin/company-agent"
        );
    }

    #[test]
    fn vertex_connection_uses_identity_auth_without_key() {
        let settings = AppSettings {
            backend: BackendChoice::GoogleVertex,
            api_base: "https://europe-west1-aiplatform.googleapis.com/v1/projects/p/locations/europe-west1/publishers/google/models".to_owned(),
            ..AppSettings::default()
        };
        assert!(settings.has_key(), "identity auth needs no API key");
        let profile = settings.llm_profile().unwrap();
        assert_eq!(profile.protocol, LlmProtocol::GoogleVertex);
        assert_eq!(profile.auth, LlmAuth::GcloudIdentity);
        assert_eq!(profile.model, "gemini-2.0-flash");
        assert!(profile.api_key.is_empty());
    }

    #[test]
    fn vertex_connection_requires_base_url() {
        let settings = AppSettings {
            backend: BackendChoice::GoogleVertex,
            ..AppSettings::default()
        };
        assert!(!settings.has_key());
        assert!(settings.llm_profile().is_err());
    }

    #[test]
    fn default_catalog_parses_and_contains_expected_tools() {
        let yaml = default_catalog_yaml();
        assert!(!yaml.contains(ECHO_CONFIG_PLACEHOLDER));
        let catalog = ToolCatalog::load_from_yaml(&yaml).expect("seed catalog must parse");
        for tool in [
            "echo",
            "http-get",
            "current-time",
            "web-fetch",
            "read-file",
            "write-file",
            "btc-price",
        ] {
            assert!(catalog.contains(tool), "missing seed tool: {tool}");
        }
        for tool in ["current-time", "web-fetch"] {
            match &catalog.get(tool).unwrap().config {
                ToolConfig::Mcp(McpConfig {
                    transport: McpTransport::Stdio { server_args, .. },
                    ..
                }) => assert_eq!(
                    server_args.get(..2),
                    Some(["--with".to_owned(), "mcp<2".to_owned()].as_slice()),
                    "{tool} must constrain the incompatible MCP SDK 2.x"
                ),
                other => panic!("expected MCP config for {tool}, got {other:?}"),
            }
        }
    }

    #[test]
    fn oauth_callback_parser_handles_success_and_sanitized_failures() {
        assert_eq!(
            parse_oauth_callback(b"GET /callback?code=one-time&state=csrf HTTP/1.1\r\n\r\n"),
            Ok(OAuthCallback::Code {
                code: "one-time".to_owned(),
                state: "csrf".to_owned(),
            })
        );
        assert_eq!(
            parse_oauth_callback(b"GET /callback?error=access_denied HTTP/1.1\r\n\r\n"),
            Ok(OAuthCallback::Denied)
        );
        assert_eq!(
            parse_oauth_callback(b"GET /callback?state=csrf HTTP/1.1\r\n\r\n"),
            Err("authorization callback did not include a code".to_owned())
        );
        assert_eq!(
            parse_oauth_callback(b"GET /callback?code=one-time HTTP/1.1\r\n\r\n"),
            Err("authorization callback did not include state".to_owned())
        );
    }

    #[tokio::test]
    async fn oauth_callback_state_mismatch_is_rejected_before_token_exchange() {
        let callback = OAuthCallback::Code {
            code: "one-time".to_owned(),
            state: "wrong".to_owned(),
        };
        // The facade is never touched when CSRF state does not match.
        assert_eq!(
            callback_state_result("expected", &callback),
            Err("authorization callback state did not match".to_owned())
        );
    }

    #[test]
    fn windows_default_echo_uses_utf8_powershell() {
        let yaml = default_catalog_yaml_for(true);
        let catalog = ToolCatalog::load_from_yaml(&yaml).expect("Windows seed catalog must parse");
        match &catalog.get("echo").unwrap().config {
            ToolConfig::Subprocess(c) => {
                assert_eq!(c.command, "powershell");
                assert!(c.args.contains(&WINDOWS_UTF8_ECHO_SCRIPT.to_owned()));
                assert!(!c.args.iter().any(|arg| arg.contains("%INXM_ARG_MESSAGE%")));
            }
            other => panic!("expected subprocess config, got {other:?}"),
        }
    }

    const UNIX_ECHO_YAML: &str = r#"tools:
  - name: echo
    description: Echoes its input to stdout
    config:
      kind: subprocess
      command: echo
      args: []
"#;

    #[test]
    fn legacy_echo_is_migrated_to_utf8_powershell() {
        for yaml in [
            UNIX_ECHO_YAML.to_owned(),
            UNIX_ECHO_YAML.replace(
                "command: echo\n      args: []",
                "command: cmd\n      args: [\"/C\", \"echo\", \"%INXM_ARG_MESSAGE%\"]",
            ),
        ] {
            let catalog = ToolCatalog::load_from_yaml(&yaml).unwrap();
            let migrated = legacy_echo_to_utf8_migration(&catalog).expect("should migrate");
            match &migrated.get("echo").unwrap().config {
                ToolConfig::Subprocess(c) => {
                    assert_eq!(c.command, "powershell");
                    assert!(c.args.contains(&WINDOWS_UTF8_ECHO_SCRIPT.to_owned()));
                    assert!(!c.args.iter().any(|arg| arg.contains("%INXM_ARG_MESSAGE%")));
                }
                other => panic!("expected subprocess config, got {other:?}"),
            }
            assert!(legacy_echo_to_utf8_migration(&migrated).is_none());
        }
    }

    #[test]
    fn native_http_get_is_added_to_older_catalogs() {
        let catalog = ToolCatalog::load_from_yaml(UNIX_ECHO_YAML).unwrap();
        let migrated = add_native_http_get_migration(&catalog).expect("should add http-get");
        match &migrated.get("http-get").unwrap().config {
            ToolConfig::Http(c) => {
                assert_eq!(c.method, "GET");
                assert_eq!(c.path_template, "{url}");
            }
            other => panic!("expected http config, got {other:?}"),
        }
        assert!(add_native_http_get_migration(&migrated).is_none());
    }

    /// A catalog seeded before the pin launches `mcp-server-time` against
    /// whatever `mcp` release uvx resolves; under 2.x the server dies on import
    /// and the tool looks broken to the user.
    const UNPINNED_UVX_YAML: &str = r#"
tools:
  - name: current-time
    description: Current date and time for a timezone
    config:
      kind: mcp
      server_command: uvx
      server_args: [mcp-server-time]
      tool_name: get_current_time
    input_schema:
      type: object
      properties:
        timezone:
          type: string
      required: [timezone]
    allowlisted: true
"#;

    #[test]
    fn unpinned_uvx_mcp_tools_are_constrained_to_the_v1_client() {
        let catalog = ToolCatalog::load_from_yaml(UNPINNED_UVX_YAML).unwrap();
        let migrated =
            add_mcp_v1_constraint_migration(&catalog).expect("unpinned catalog should migrate");
        match &migrated.get("current-time").unwrap().config {
            ToolConfig::Mcp(c) => match &c.transport {
                McpTransport::Stdio { server_args, .. } => assert_eq!(
                    server_args,
                    &vec![
                        UVX_WITH_FLAG.to_owned(),
                        MCP_V1_CONSTRAINT.to_owned(),
                        "mcp-server-time".to_owned()
                    ]
                ),
                other => panic!("expected a stdio transport, got {other:?}"),
            },
            other => panic!("expected mcp config, got {other:?}"),
        }
        // Idempotent: a second pass finds nothing left to do.
        assert!(add_mcp_v1_constraint_migration(&migrated).is_none());
    }

    #[test]
    fn the_seeded_catalog_needs_no_mcp_constraint_migration() {
        let catalog = ToolCatalog::load_from_yaml(&default_catalog_yaml()).unwrap();
        assert!(add_mcp_v1_constraint_migration(&catalog).is_none());
    }

    #[test]
    fn a_hand_pinned_mcp_constraint_is_left_alone() {
        let yaml = UNPINNED_UVX_YAML.replace(
            "server_args: [mcp-server-time]",
            r#"server_args: ["--with", "mcp<3", mcp-server-time]"#,
        );
        let catalog = ToolCatalog::load_from_yaml(&yaml).unwrap();
        assert!(
            add_mcp_v1_constraint_migration(&catalog).is_none(),
            "an explicit user constraint must outrank the migration"
        );
    }

    #[test]
    fn non_uvx_mcp_tools_are_untouched_by_the_constraint_migration() {
        let yaml = UNPINNED_UVX_YAML
            .replace("server_command: uvx", "server_command: npx")
            .replace(
                "server_args: [mcp-server-time]",
                "server_args: [some-server]",
            );
        let catalog = ToolCatalog::load_from_yaml(&yaml).unwrap();
        assert!(add_mcp_v1_constraint_migration(&catalog).is_none());
    }

    #[test]
    fn runnable_tool_catalog_keeps_native_http_and_drops_missing_commands() {
        let catalog = ToolCatalog::new(vec![
            native_http_get_tool(),
            ToolEntry {
                name: "missing-subprocess".to_owned(),
                description: "not installed".to_owned(),
                config: ToolConfig::Subprocess(SubprocessConfig {
                    command: "definitely-not-installed-inxm-test".to_owned(),
                    args: vec![],
                    env: Default::default(),
                    working_dir: None,
                }),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: serde_json::json!({"type": "object"}),
                allowlisted: true,
                timeout_secs: None,
            },
        ]);
        let names: Vec<String> = runnable_tool_catalog(&catalog)
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(names, vec!["http-get".to_owned()]);
    }

    #[test]
    fn is_transport_error_detects_dns_failures() {
        assert!(is_transport_error("DNS lookup failed for example.com"));
        assert!(is_transport_error("error: DNS lookup failed"));
    }

    #[test]
    fn is_transport_error_detects_connection_refused() {
        assert!(is_transport_error("connection refused"));
        assert!(is_transport_error("error: connection refused by host"));
    }

    #[test]
    fn is_transport_error_detects_timeouts() {
        assert!(is_transport_error("timed out"));
        assert!(is_transport_error("request timed out after 30s"));
    }

    #[test]
    fn is_transport_error_detects_tls_errors() {
        assert!(is_transport_error("TLS error"));
        assert!(is_transport_error("TLS certificate validation failed"));
    }

    #[test]
    fn is_transport_error_detects_repair_guidance_marker() {
        assert!(is_transport_error(
            "some error\n\n[repair-guidance] this is external"
        ));
        assert!(is_transport_error("[repair-guidance]"));
    }

    #[test]
    fn is_transport_error_covers_the_full_classifier_table_case_insensitively() {
        // Signals from the shared classifier table that the old hand-rolled
        // list missed; matching is now case-insensitive as well.
        assert!(is_transport_error("certificate verify failed"));
        assert!(is_transport_error("connection reset by peer"));
        assert!(is_transport_error("tls handshake failure"));
    }

    #[test]
    fn is_transport_error_returns_false_for_local_errors() {
        assert!(!is_transport_error("Python not found"));
        assert!(!is_transport_error("step validation failed"));
        assert!(!is_transport_error("output schema mismatch"));
    }

    #[test]
    fn enhance_resume_error_message_adds_repair_hint_for_transport_errors() {
        let error = "HTTP request failed: DNS lookup failed";
        let enhanced = enhance_resume_error_message(error);
        assert!(enhanced.contains("resume failed:"));
        assert!(enhanced.contains("DNS lookup failed"));
        assert!(enhanced.contains("/repair"));
        assert!(enhanced.contains("external endpoint"));
        assert!(enhanced.contains("fallback step"));
    }

    #[test]
    fn enhance_resume_error_message_preserves_error_for_local_failures() {
        let error = "Python not found on system";
        let enhanced = enhance_resume_error_message(error);
        assert_eq!(enhanced, "resume failed: Python not found on system");
        assert!(!enhanced.contains("/repair"));
    }

    #[test]
    fn concurrent_catalog_mutations_do_not_lose_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = DataPaths::at(tmp.path().to_owned());
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        ToolCatalog::new(Vec::new())
            .save_to_file(&paths.catalog_path)
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles = ["alpha", "beta"]
            .into_iter()
            .map(|name| {
                let barrier = barrier.clone();
                let env = test_env(paths.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    save_tool(&env, named_http_tool(name)).unwrap();
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }

        let catalog = ToolCatalog::load_from_file(&paths.catalog_path).unwrap();
        assert!(catalog.contains("alpha"));
        assert!(catalog.contains("beta"));
    }

    #[test]
    fn rename_tool_is_one_deterministic_catalog_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = DataPaths::at(tmp.path().to_owned());
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        ToolCatalog::new(vec![named_http_tool("old"), named_http_tool("other")])
            .save_to_file(&paths.catalog_path)
            .unwrap();
        let env = test_env(paths.clone());

        rename_tool(&env, "old", named_http_tool("new")).unwrap();

        let catalog = ToolCatalog::load_from_file(&paths.catalog_path).unwrap();
        assert!(!catalog.contains("old"));
        assert!(catalog.contains("new"));
        assert!(catalog.contains("other"));
    }

    #[test]
    fn slugify_tool_name_makes_a_kebab_case_starting_point() {
        assert_eq!(
            slugify_tool_name("Search GitHub repositories by keyword"),
            "search-github-repositories-by-keyword"
        );
        assert_eq!(slugify_tool_name("!!! ??? "), "new-tool");
        assert_eq!(slugify_tool_name(""), "new-tool");
    }
}
