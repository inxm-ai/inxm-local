//! Optional, opt-in usage telemetry.
//!
//! Nothing is ever sent unless the user explicitly said yes — the persisted
//! setting is `Option<bool>` and only `Some(true)` counts, so upgraded
//! installs that were never asked stay silent. On top of the setting there
//! are two runtime kill switches that always win and can only *disable*:
//!
//! - environment: `INXM_TELEMETRY=off` (or `0`, `false`, `no`)
//! - CLI: `--no-telemetry`
//!
//! What is collected, where it goes, and for how long it is kept is
//! documented in `docs/telemetry.md`; the schema in [`schema`] is exhaustive
//! and the sending code in [`sender`] is the only place a request is made.
//! Sends are fire-and-forget: a failure can never affect normal operation.

pub mod schema;
pub mod sender;
pub mod usage;

pub use schema::{Channel, Event};

const TELEMETRY_ENV: &str = "INXM_TELEMETRY";
const NO_TELEMETRY_FLAG: &str = "--no-telemetry";

/// Record a process start; `setting` is `AppSettings::telemetry_enabled`
/// as persisted. The batched counters go through [`usage::flush`] instead.
pub fn record_app_started(setting: Option<bool>, channel: Channel) {
    if !enabled(setting) {
        return;
    }
    sender::send_detached(Event::app_started(channel));
}

/// The effective on/off decision for this process.
pub fn enabled(setting: Option<bool>) -> bool {
    resolve(
        setting,
        std::env::var(TELEMETRY_ENV).ok().as_deref(),
        std::env::args(),
    )
}

/// Whether a runtime kill switch (env var or CLI flag) is forcing telemetry
/// off in this process, regardless of the persisted setting. The Settings
/// view uses this to explain why the checkbox has no effect right now.
pub fn runtime_disabled() -> bool {
    !resolve(
        Some(true),
        std::env::var(TELEMETRY_ENV).ok().as_deref(),
        std::env::args(),
    )
}

/// Pure resolution, testable without touching the process environment.
/// Precedence: CLI flag and env var can each only disable; the persisted
/// setting must be an explicit `Some(true)` for anything to be sent.
fn resolve(
    setting: Option<bool>,
    env_value: Option<&str>,
    mut args: impl Iterator<Item = String>,
) -> bool {
    if setting != Some(true) {
        return false;
    }
    if args.any(|arg| arg == NO_TELEMETRY_FLAG) {
        return false;
    }
    !env_value
        .map(|value| value.trim().to_ascii_lowercase())
        .is_some_and(|value| matches!(value.as_str(), "0" | "off" | "false" | "no"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_args() -> std::iter::Empty<String> {
        std::iter::empty()
    }

    #[test]
    fn disabled_unless_explicitly_opted_in() {
        assert!(!resolve(None, None, no_args()));
        assert!(!resolve(Some(false), None, no_args()));
        assert!(resolve(Some(true), None, no_args()));
    }

    #[test]
    fn env_var_disables_but_never_enables() {
        for off in ["0", "off", "OFF", "false", "no", " off "] {
            assert!(!resolve(Some(true), Some(off), no_args()), "{off:?}");
        }
        // A truthy env value cannot override a missing or negative consent.
        assert!(!resolve(None, Some("1"), no_args()));
        assert!(!resolve(Some(false), Some("on"), no_args()));
        assert!(resolve(Some(true), Some("1"), no_args()));
    }

    #[test]
    fn cli_flag_disables() {
        let args = || ["--headless".to_owned(), "--no-telemetry".to_owned()].into_iter();
        assert!(!resolve(Some(true), None, args()));
        assert!(resolve(
            Some(true),
            None,
            ["--headless".to_owned()].into_iter()
        ));
    }
}
