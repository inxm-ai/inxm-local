//! Local usage counters — the "a tad more than a ping" half of telemetry.
//!
//! Nothing here is real-time: counters accumulate in a plain JSON file in
//! the data dir (`telemetry-usage.json`, inspectable at any time) and are
//! sent as ONE `usage_summary` event on the next app start, then reset.
//! Counting itself is consent-gated — with telemetry off (or force-disabled
//! via env/CLI) the file is never even written, so opting in later can
//! never ship activity from before the opt-in.
//!
//! Every counter is a plain tally; the file contains no run ids, plan
//! names, timestamps, or anything else that could sequence or identify
//! activity.

use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::schema::Event;

pub const USAGE_FILE_NAME: &str = "telemetry-usage.json";

/// Which surface drove the action — the app (desktop UI, chat, scheduler)
/// or the local MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    App,
    Mcp,
}

/// The countable plan-lifecycle actions. `RunHealed` counts a successful
/// resume after a repair and is *in addition to* the `RunSucceeded` the same
/// run also gets — healed is a subset of succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    PlanCreated,
    PlanEdited,
    RunSucceeded,
    RunFailed,
    RunHealed,
}

/// The five top-level views, for the where-time-is-spent buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    Chat,
    Plans,
    Schedules,
    McpTools,
    Settings,
}

/// The accumulated counters, exactly as persisted and exactly as sent
/// (flattened into the `usage_summary` event). All plain tallies.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCounters {
    #[serde(default)]
    pub plans_created_app: u64,
    #[serde(default)]
    pub plans_created_mcp: u64,
    #[serde(default)]
    pub plans_edited_app: u64,
    #[serde(default)]
    pub plans_edited_mcp: u64,
    #[serde(default)]
    pub runs_succeeded_app: u64,
    #[serde(default)]
    pub runs_succeeded_mcp: u64,
    #[serde(default)]
    pub runs_failed_app: u64,
    #[serde(default)]
    pub runs_failed_mcp: u64,
    #[serde(default)]
    pub runs_healed_app: u64,
    #[serde(default)]
    pub runs_healed_mcp: u64,
    #[serde(default)]
    pub seconds_in_chat: u64,
    #[serde(default)]
    pub seconds_in_plans: u64,
    #[serde(default)]
    pub seconds_in_schedules: u64,
    #[serde(default)]
    pub seconds_in_mcp_tools: u64,
    #[serde(default)]
    pub seconds_in_settings: u64,
}

impl UsageCounters {
    pub fn is_empty(&self) -> bool {
        *self == UsageCounters::default()
    }

    fn bump(&mut self, source: Source, action: Action) {
        let slot = match (action, source) {
            (Action::PlanCreated, Source::App) => &mut self.plans_created_app,
            (Action::PlanCreated, Source::Mcp) => &mut self.plans_created_mcp,
            (Action::PlanEdited, Source::App) => &mut self.plans_edited_app,
            (Action::PlanEdited, Source::Mcp) => &mut self.plans_edited_mcp,
            (Action::RunSucceeded, Source::App) => &mut self.runs_succeeded_app,
            (Action::RunSucceeded, Source::Mcp) => &mut self.runs_succeeded_mcp,
            (Action::RunFailed, Source::App) => &mut self.runs_failed_app,
            (Action::RunFailed, Source::Mcp) => &mut self.runs_failed_mcp,
            (Action::RunHealed, Source::App) => &mut self.runs_healed_app,
            (Action::RunHealed, Source::Mcp) => &mut self.runs_healed_mcp,
        };
        *slot = slot.saturating_add(1);
    }

    fn add_view_seconds(&mut self, view: ViewKind, seconds: u64) {
        let slot = match view {
            ViewKind::Chat => &mut self.seconds_in_chat,
            ViewKind::Plans => &mut self.seconds_in_plans,
            ViewKind::Schedules => &mut self.seconds_in_schedules,
            ViewKind::McpTools => &mut self.seconds_in_mcp_tools,
            ViewKind::Settings => &mut self.seconds_in_settings,
        };
        *slot = slot.saturating_add(seconds);
    }
}

/// The compiler context sent alongside the counters. Read from
/// `settings.json` by key — this function touches exactly three keys
/// (`backend`, `model`, `experimental_agent_calls`) so a custom CLI's
/// `executable` and `command_template` can never leak into telemetry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilerContext {
    pub backend: String,
    pub model: String,
    pub experimental_agent_calls: bool,
}

const MODEL_NAME_MAX_LEN: usize = 64;

fn compiler_context(settings: &serde_json::Value) -> CompilerContext {
    let backend = settings["backend"].as_str().unwrap_or("auto").to_owned();
    let model = settings["model"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(MODEL_NAME_MAX_LEN)
        .collect();
    CompilerContext {
        backend,
        model,
        experimental_agent_calls: settings["experimental_agent_calls"]
            .as_bool()
            .unwrap_or(false),
    }
}

/// Count one action. A no-op unless telemetry is enabled right now — both
/// the persisted consent and the runtime kill switches gate the *write*,
/// not just the send. Failures (unreadable file, full disk) are swallowed:
/// losing a count must never affect the operation being counted.
pub fn count(data_dir: &Path, settings_path: &Path, source: Source, action: Action) {
    mutate(data_dir, settings_path, |counters| {
        counters.bump(source, action)
    });
}

/// Add foreground seconds spent in a view. Same gating as [`count`].
pub fn count_view_seconds(data_dir: &Path, settings_path: &Path, view: ViewKind, seconds: u64) {
    if seconds == 0 {
        return;
    }
    mutate(data_dir, settings_path, |counters| {
        counters.add_view_seconds(view, seconds)
    });
}

/// Send whatever accumulated since the last flush as one `usage_summary`
/// event and reset the file. Called once per app start; a no-op when
/// telemetry is disabled or nothing was counted.
pub fn flush(data_dir: &Path, settings_path: &Path) {
    let Some(settings) = consented_settings(settings_path) else {
        return;
    };
    let counters = {
        let _guard = usage_file_lock();
        let counters = load(data_dir);
        if counters.is_empty() {
            return;
        }
        let _ = std::fs::remove_file(data_dir.join(USAGE_FILE_NAME));
        counters
    };
    super::sender::send_detached(Event::usage_summary(compiler_context(&settings), counters));
}

/// One process-wide lock for read-modify-write on the usage file. Telemetry
/// is best-effort by design, so a concurrent *process* (desktop + headless)
/// racing a count away is accepted rather than file-locked.
fn usage_file_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn mutate(data_dir: &Path, settings_path: &Path, apply: impl FnOnce(&mut UsageCounters)) {
    if consented_settings(settings_path).is_none() {
        return;
    }
    let _guard = usage_file_lock();
    let mut counters = load(data_dir);
    apply(&mut counters);
    let path = data_dir.join(USAGE_FILE_NAME);
    let write = std::fs::create_dir_all(data_dir).and_then(|()| {
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&counters).unwrap_or_default(),
        )
    });
    if let Err(error) = write {
        tracing::debug!(
            operation = "telemetry.usage_write",
            app_version = env!("CARGO_PKG_VERSION"),
            triggered_by = "application",
            outcome = "failure",
            error = %error,
            "usage counter write failed; count dropped"
        );
    }
}

fn load(data_dir: &Path) -> UsageCounters {
    std::fs::read_to_string(data_dir.join(USAGE_FILE_NAME))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// The parsed settings file, but only when telemetry is enabled (explicit
/// consent recorded AND no runtime kill switch). `None` means "do nothing".
fn consented_settings(settings_path: &Path) -> Option<serde_json::Value> {
    let settings: serde_json::Value = std::fs::read_to_string(settings_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())?;
    super::enabled(settings["telemetry_enabled"].as_bool()).then_some(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consented_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"telemetry_enabled":true,"backend":"claude","model":"claude-sonnet-5","experimental_agent_calls":true}"#,
        )
        .unwrap();
        dir
    }

    #[test]
    fn counts_accumulate_and_persist() {
        let dir = consented_dir();
        let settings = dir.path().join("settings.json");
        count(dir.path(), &settings, Source::App, Action::PlanCreated);
        count(dir.path(), &settings, Source::Mcp, Action::RunSucceeded);
        count(dir.path(), &settings, Source::Mcp, Action::RunSucceeded);
        count_view_seconds(dir.path(), &settings, ViewKind::Plans, 42);
        let counters = load(dir.path());
        assert_eq!(counters.plans_created_app, 1);
        assert_eq!(counters.runs_succeeded_mcp, 2);
        assert_eq!(counters.seconds_in_plans, 42);
        assert_eq!(counters.runs_failed_app, 0);
    }

    #[test]
    fn nothing_is_written_without_consent() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, r#"{"telemetry_enabled":false}"#).unwrap();
        count(dir.path(), &settings, Source::App, Action::PlanCreated);
        count_view_seconds(dir.path(), &settings, ViewKind::Chat, 10);
        assert!(!dir.path().join(USAGE_FILE_NAME).exists());

        // No settings file at all (never asked) must behave the same.
        let dir = tempfile::tempdir().unwrap();
        count(
            dir.path(),
            &dir.path().join("settings.json"),
            Source::App,
            Action::PlanCreated,
        );
        assert!(!dir.path().join(USAGE_FILE_NAME).exists());
    }

    #[test]
    fn compiler_context_never_reads_custom_cli_commands() {
        let settings: serde_json::Value = serde_json::from_str(
            r#"{
                "backend": "custom_cli",
                "model": "my-local-model",
                "executable": "/home/user/secret-tool",
                "command_template": "secret-tool --token=hunter2 {{PROMPT}}",
                "experimental_agent_calls": false
            }"#,
        )
        .unwrap();
        let context = compiler_context(&settings);
        assert_eq!(context.backend, "custom_cli");
        assert_eq!(context.model, "my-local-model");
        let serialized =
            serde_json::to_string(&Event::usage_summary(context, UsageCounters::default()))
                .unwrap();
        assert!(!serialized.contains("secret-tool"));
        assert!(!serialized.contains("hunter2"));
    }

    #[test]
    fn model_name_is_trimmed_and_capped() {
        let settings = serde_json::json!({ "model": format!("  {}  ", "x".repeat(200)) });
        assert_eq!(compiler_context(&settings).model.len(), MODEL_NAME_MAX_LEN);
    }

    #[test]
    fn flush_resets_the_file() {
        let dir = consented_dir();
        let settings = dir.path().join("settings.json");
        // Point the sender at a closed local port so the detached send can
        // never reach a real endpoint from a test.
        count(dir.path(), &settings, Source::App, Action::RunFailed);
        assert!(dir.path().join(USAGE_FILE_NAME).exists());
        temp_env(|| flush(dir.path(), &settings));
        assert!(!dir.path().join(USAGE_FILE_NAME).exists());
    }

    fn temp_env(run: impl FnOnce()) {
        // SAFETY: test-only; serialized by the usage-file mutex not being
        // relevant here and cargo running each test binary in one process.
        unsafe {
            std::env::set_var("INXM_TELEMETRY_ENDPOINT", "http://127.0.0.1:9/v1/event");
        }
        run();
        unsafe {
            std::env::remove_var("INXM_TELEMETRY_ENDPOINT");
        }
    }
}
