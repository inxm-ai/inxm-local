//! The exhaustive telemetry event schema.
//!
//! Every field that can ever leave the machine is declared in this file —
//! `docs/telemetry.md` promises users that this list is complete, so adding
//! a field here is a documentation change too. Rules for every event:
//!
//! - no stable identifiers: no machine id, no install id, no user name,
//!   no hostname, no IP handling on the client (the Worker discards it)
//! - no timestamps on the client; the sink assigns a coarse server-side one
//! - no free-form strings from user data (plan names, prompts, paths)

/// Which entry point started the process. Coarse by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Desktop,
    Headless,
    McpOnly,
}

/// The two events that exist. `AppStarted` is one ping per process start;
/// `UsageSummary` is the batched counter flush sent on the next app start
/// (never live) — see [`super::usage`] for how the counters accumulate.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    AppStarted {
        /// Cargo package version, e.g. `0.1.0`.
        app_version: &'static str,
        /// `std::env::consts::OS`: `linux`, `macos`, or `windows`.
        os: &'static str,
        channel: Channel,
    },
    UsageSummary {
        app_version: &'static str,
        os: &'static str,
        /// The configured compiler backend kind, e.g. `claude`, `codex`,
        /// `custom_cli`. Never the executable or command line.
        backend: String,
        /// The configured model *name* only (trimmed, max 64 chars); empty
        /// when the backend default is used. For custom CLIs this is still
        /// just the model field — commands and executables are never read.
        model: String,
        experimental_agent_calls: bool,
        /// The plain tallies from `telemetry-usage.json`, flattened.
        #[serde(flatten)]
        counters: super::usage::UsageCounters,
    },
}

impl Event {
    pub fn app_started(channel: Channel) -> Self {
        Event::AppStarted {
            app_version: env!("CARGO_PKG_VERSION"),
            os: std::env::consts::OS,
            channel,
        }
    }

    pub fn usage_summary(
        context: super::usage::CompilerContext,
        counters: super::usage::UsageCounters,
    ) -> Self {
        Event::UsageSummary {
            app_version: env!("CARGO_PKG_VERSION"),
            os: std::env::consts::OS,
            backend: context.backend,
            model: context.model,
            experimental_agent_calls: context.experimental_agent_calls,
            counters,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_started_serializes_to_exactly_the_documented_fields() {
        let json = serde_json::to_value(Event::AppStarted {
            app_version: "0.1.0",
            os: "linux",
            channel: Channel::Desktop,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "event": "app_started",
                "app_version": "0.1.0",
                "os": "linux",
                "channel": "desktop",
            })
        );
        // Exactly four keys — a new field must show up in this test, the
        // docs, and the Worker validator together.
        assert_eq!(json.as_object().unwrap().len(), 4);
    }

    #[test]
    fn usage_summary_serializes_to_exactly_the_documented_fields() {
        let counters = crate::telemetry::usage::UsageCounters {
            plans_created_app: 2,
            runs_succeeded_mcp: 3,
            seconds_in_chat: 120,
            ..Default::default()
        };
        let context = crate::telemetry::usage::CompilerContext {
            backend: "claude".to_owned(),
            model: "claude-sonnet-5".to_owned(),
            experimental_agent_calls: true,
        };
        let json = serde_json::to_value(Event::usage_summary(context, counters)).unwrap();
        assert_eq!(json["event"], "usage_summary");
        assert_eq!(json["backend"], "claude");
        assert_eq!(json["model"], "claude-sonnet-5");
        assert_eq!(json["experimental_agent_calls"], true);
        assert_eq!(json["plans_created_app"], 2);
        assert_eq!(json["runs_succeeded_mcp"], 3);
        assert_eq!(json["seconds_in_chat"], 120);
        // 6 envelope/context fields + 15 flattened counters, nothing else.
        assert_eq!(json.as_object().unwrap().len(), 21);
    }
}
