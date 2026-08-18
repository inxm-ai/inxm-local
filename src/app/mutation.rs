//! Shared mutation boundary for app-owned persistent state.
//!
//! The desktop engine and local MCP server run on separate Tokio runtimes but
//! share one [`MutationBoundary`] through [`crate::app::engine::DataPaths`].
//! Short filesystem mutations execute under this gate; network calls and plan
//! execution deliberately remain outside it.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

/// Serializes read-modify-write operations across every app adapter.
#[derive(Clone, Debug, Default)]
pub struct MutationBoundary {
    gate: Arc<Mutex<()>>,
}

impl MutationBoundary {
    /// Execute one synchronous persistent-state mutation exclusively.
    pub fn run_named<T>(
        &self,
        mutation_kind: &'static str,
        triggered_by: &'static str,
        operation: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let started = Instant::now();
        let result = self
            .gate
            .lock()
            .map_err(|_| anyhow::anyhow!("app mutation boundary was poisoned"))
            .and_then(|_guard| operation());
        let duration_ms = started.elapsed().as_millis() as u64;

        if result.is_ok() {
            tracing::info!(
                mutation_kind,
                triggered_by,
                app_version = env!("CARGO_PKG_VERSION"),
                duration_ms,
                outcome = "success",
                "app state mutation completed"
            );
        } else {
            tracing::error!(
                mutation_kind,
                triggered_by,
                app_version = env!("CARGO_PKG_VERSION"),
                duration_ms,
                outcome = "failure",
                "app state mutation failed"
            );
        }

        result
    }
}
