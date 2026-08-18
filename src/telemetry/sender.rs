//! The exact code that sends a telemetry event: one HTTP POST, on a detached
//! thread, with a short timeout, and every failure swallowed. Nothing in
//! here can block or fail the caller.

use super::schema::Event;

/// Default sink: a Cloudflare Worker (see `telemetry-worker/` in the repo
/// root) that validates the schema and writes one row to Workers Analytics
/// Engine. Overridable for inspection/self-hosting via
/// `INXM_TELEMETRY_ENDPOINT`.
pub const DEFAULT_ENDPOINT: &str = "https://telemetry.inxm.ai/v1/event";

const ENDPOINT_ENV: &str = "INXM_TELEMETRY_ENDPOINT";
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

pub fn endpoint() -> String {
    std::env::var(ENDPOINT_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned())
}

/// Fire-and-forget: spawn a detached thread, POST the event, ignore the
/// result. The thread is never joined — if the process exits first the send
/// is simply lost, which is the correct trade-off for telemetry.
pub fn send_detached(event: Event) {
    let endpoint = endpoint();
    std::thread::Builder::new()
        .name("inxm-telemetry".to_owned())
        .spawn(move || send_blocking(&endpoint, &event))
        .map(drop)
        .unwrap_or_else(|error| {
            tracing::debug!(
                operation = "telemetry.spawn",
                app_version = env!("CARGO_PKG_VERSION"),
                triggered_by = "application",
                outcome = "failure",
                error = %error,
                "telemetry thread could not be spawned; event dropped"
            );
        });
}

fn send_blocking(endpoint: &str, event: &Event) {
    let outcome = reqwest::blocking::Client::builder()
        .timeout(SEND_TIMEOUT)
        .build()
        .and_then(|client| client.post(endpoint).json(event).send())
        .and_then(|response| response.error_for_status());
    if let Err(error) = outcome {
        tracing::debug!(
            operation = "telemetry.send",
            app_version = env!("CARGO_PKG_VERSION"),
            triggered_by = "application",
            outcome = "failure",
            error = %error,
            "telemetry send failed; event dropped"
        );
    }
}
