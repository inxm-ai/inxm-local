//! Deterministic error classification for repair.
//!
//! Classification is purely based on string patterns — no I/O, no AI calls.
//! The goal is a cheap first-pass triage so the repair loop can route failures
//! appropriately and give the compiler backend useful context.

use serde::{Deserialize, Serialize};

// ─── ErrorKind ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    ToolNotFound,
    ToolExecutionFailed,
    CodeExecutionFailed,
    /// A CODE_CALL step failed because its interpreter could not be spawned.
    ///
    /// Distinct from `ToolNotFound`: the interpreter file was located on
    /// `PATH` (so it looks "available") but the OS rejected the `exec` call —
    /// a broken shebang, a macOS CLT stub pointing to a missing runtime, or a
    /// wrapper script whose target interpreter is absent are all common causes.
    ///
    /// The fix is **never** to retry the same interpreter. See
    /// `failure_packet.rs` for how this gets surfaced to the compiler.
    MissingInterpreter,
    TimeoutExceeded,
    NetworkError,
    /// A transport-level HTTP failure (DNS failure, connection
    /// refused/reset, timeout, TLS) reaching an external endpoint.
    ///
    /// Distinct from the generic `NetworkError`: this is specifically an
    /// *external service is unreachable* situation, so the fix is never
    /// "resume the same call" — it is "substitute the endpoint" or "add a
    /// fallback step". See `failure_packet.rs` for how this gets surfaced to
    /// the compiler.
    ExternalEndpointDown,
    PermissionDenied,
    OutputSchemaViolation,
    PromptCallFailed,
    Unknown,
}

// ─── Pattern tables ───────────────────────────────────────────────────────────
//
// All patterns are matched as lowercase substrings against the combined
// error message + stderr. Each table is one classification bucket; the
// priority order between buckets lives in `classify`.

/// Markers that the failure involves an HTTP call to a remote endpoint.
const HTTP_MARKER_PATTERNS: &[&str] = &["http", "reqwest"];

/// Transport-level failure signals: DNS failure, connection refused/reset,
/// timeout, or TLS/certificate error. Covers both the pre-hardening reqwest
/// error text (e.g. "error trying to connect: dns error: ...", "tcp connect
/// error: Connection refused", "operation timed out", "invalid peer
/// certificate") and the hardened adapter's explicit phrasing ("DNS lookup
/// failed", "connection refused", "timed out", "TLS").
/// Shared with `app::engine`, which uses the same signals (lowercased) to
/// decide whether a failed resume deserves a `/repair` hint — one table so
/// the hint and the classification can never drift apart.
pub(crate) const TRANSPORT_FAILURE_PATTERNS: &[&str] = &[
    "dns",
    "connection refused",
    "connection reset",
    "error trying to connect",
    "timed out",
    "timeout",
    "tls",
    "ssl",
    "certificate",
];

const TIMEOUT_PATTERNS: &[&str] = &["timed out", "timeout"];

const PERMISSION_PATTERNS: &[&str] = &["permission denied", "access denied"];

const NETWORK_PATTERNS: &[&str] = &["network", "connection", "dns", "socket"];

const PROMPT_CALL_PATTERNS: &[&str] = &["prompt_call", "api error"];

const TOOL_EXECUTION_PATTERNS: &[&str] = &["exit code", "exited with"];

/// The step ran fine but its stdout could not be mapped onto the declared
/// outputs (not a JSON object, or invalid JSON such as lone UTF-16 surrogate
/// escapes). Emitted verbatim by `executor::step_runners::code_call`; matched
/// first because the embedded parse error is free-form text that could
/// otherwise stray into the network/timeout buckets.
const OUTPUT_MAPPING_PATTERNS: &[&str] = &["cannot satisfy the declared outputs"];

/// An HTTP error *status* in the adapter's `HTTP {status}: {body}` format.
/// The endpoint answered — this is an application-level failure, so it must
/// classify as tool execution, not as a missing tool when the body happens
/// to say "not found" (a 404 page) or similar.
fn has_http_error_status(combined: &str) -> bool {
    static HTTP_ERROR_STATUS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    HTTP_ERROR_STATUS
        .get_or_init(|| {
            regex::Regex::new(r"http [45]\d\d\b").expect("HTTP status pattern is a valid regex")
        })
        .is_match(combined)
}

const NOT_FOUND_PATTERNS: &[&str] = &["not found", "no such file"];

/// Signals that a CODE_CALL interpreter was found on PATH but could not be
/// exec'd by the OS — a broken shebang, a macOS CLT stub, or a wrapper
/// script whose target interpreter is absent.
///
/// Matched against the error message only (the executor writes it there).
/// Checked *before* the generic `NOT_FOUND_PATTERNS` bucket so this more
/// specific classification takes priority.
const MISSING_INTERPRETER_PATTERNS: &[&str] = &["failed to spawn interpreter"];

/// Code-execution signals (Python tracebacks, compile errors, etc.).
/// Matched against stderr only — an executor error *message* mentioning
/// "error: " is too generic to imply the step's own code failed.
const CODE_EXECUTION_STDERR_PATTERNS: &[&str] = &[
    "traceback",
    "syntaxerror",
    "nameerror",
    "typeerror",
    "exception",
    "error[e", // Rust compiler
    "error: ", // generic compiler/interpreter prefix
];

// ─── Classifier ───────────────────────────────────────────────────────────────

fn contains_any(haystack: &str, patterns: &[&str]) -> bool {
    patterns.iter().any(|pattern| haystack.contains(pattern))
}

/// Detect a transport-level HTTP failure: DNS failure, connection
/// refused/reset, timeout, or TLS error, while talking to a remote endpoint.
///
/// Gated on the presence of an HTTP/URL marker (`http`) so that unrelated
/// generic network or timeout strings (e.g. a subprocess timing out, a raw
/// socket error with no URL in sight) keep classifying as `NetworkError` /
/// `TimeoutExceeded` as before.
fn is_external_endpoint_down(combined: &str) -> bool {
    contains_any(combined, HTTP_MARKER_PATTERNS)
        && contains_any(combined, TRANSPORT_FAILURE_PATTERNS)
}

/// Classify an error message into an `ErrorKind`.
///
/// Both `error_message` and `stderr` are searched (case-insensitive).
/// Patterns are checked in priority order: more specific conditions first.
pub fn classify(error_message: &str, stderr: Option<&str>) -> ErrorKind {
    let msg = error_message.to_lowercase();
    let err = stderr.unwrap_or("").to_lowercase();
    let combined = format!("{msg} {err}");

    // Output mapping — the step succeeded but stdout could not satisfy the
    // declared outputs. Checked first: the message embeds a free-form JSON
    // parse error whose wording must not leak into other buckets.
    if contains_any(&msg, OUTPUT_MAPPING_PATTERNS) {
        return ErrorKind::OutputSchemaViolation;
    }

    // Prompt / API — check before generic "not found" so API 404s don't
    // misclassify as ToolNotFound
    if contains_any(&combined, PROMPT_CALL_PATTERNS) {
        return ErrorKind::PromptCallFailed;
    }

    // HTTP error status — the endpoint answered with 4xx/5xx. Checked
    // before the not-found bucket so a 404 body ("HTTP 404 Not Found: …")
    // reads as a failed call, not a missing tool.
    if has_http_error_status(&combined) {
        return ErrorKind::ToolExecutionFailed;
    }

    // External endpoint down — after explicit HTTP statuses (which prove that
    // an endpoint answered), but before generic timeout/network buckets.
    if is_external_endpoint_down(&combined) {
        return ErrorKind::ExternalEndpointDown;
    }

    // Timeout — check before exit codes: a timed-out process may also emit
    // "exit code"
    if contains_any(&combined, TIMEOUT_PATTERNS) {
        return ErrorKind::TimeoutExceeded;
    }

    // Permission
    if contains_any(&combined, PERMISSION_PATTERNS) {
        return ErrorKind::PermissionDenied;
    }

    // Network
    if contains_any(&combined, NETWORK_PATTERNS) {
        return ErrorKind::NetworkError;
    }

    // Tool exit-code failure
    if contains_any(&combined, TOOL_EXECUTION_PATTERNS) {
        return ErrorKind::ToolExecutionFailed;
    }

    // Missing interpreter — a CODE_CALL interpreter was found on PATH but the
    // OS rejected the exec (broken shebang, macOS CLT stub, etc.). Check
    // before the generic "not found" bucket: the error message contains both
    // "failed to spawn interpreter" and "no such file", and this more specific
    // classification carries targeted repair guidance.
    if contains_any(&msg, MISSING_INTERPRETER_PATTERNS)
        && contains_any(&combined, NOT_FOUND_PATTERNS)
    {
        return ErrorKind::MissingInterpreter;
    }

    // Missing tool / file
    if contains_any(&combined, NOT_FOUND_PATTERNS) {
        return ErrorKind::ToolNotFound;
    }

    // Code execution signals — stderr only, see the pattern table.
    if contains_any(&err, CODE_EXECUTION_STDERR_PATTERNS) {
        return ErrorKind::CodeExecutionFailed;
    }

    ErrorKind::Unknown
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn c(msg: &str) -> ErrorKind {
        classify(msg, None)
    }

    fn cs(msg: &str, stderr: &str) -> ErrorKind {
        classify(msg, Some(stderr))
    }

    #[test]
    fn timeout_from_message() {
        assert_eq!(c("step timed out after 30s"), ErrorKind::TimeoutExceeded);
        assert_eq!(c("request timeout"), ErrorKind::TimeoutExceeded);
    }

    #[test]
    fn timeout_takes_priority_over_exit_code() {
        // A process killed by timeout may still log "exit code 124"
        assert_eq!(
            c("exited with exit code 124 — timed out"),
            ErrorKind::TimeoutExceeded
        );
    }

    #[test]
    fn permission_denied() {
        assert_eq!(
            c("permission denied: /etc/passwd"),
            ErrorKind::PermissionDenied
        );
        assert_eq!(
            c("access denied for user root"),
            ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn network_errors() {
        assert_eq!(c("network unreachable"), ErrorKind::NetworkError);
        assert_eq!(c("connection refused"), ErrorKind::NetworkError);
        assert_eq!(c("dns resolution failed"), ErrorKind::NetworkError);
        assert_eq!(c("socket error: broken pipe"), ErrorKind::NetworkError);
    }

    #[test]
    fn network_pure() {
        assert_eq!(c("connection reset by peer"), ErrorKind::NetworkError);
    }

    #[test]
    fn prompt_call_and_api_error() {
        assert_eq!(
            c("prompt_call failed: model unavailable"),
            ErrorKind::PromptCallFailed
        );
        assert_eq!(
            c("api error: 429 rate limited"),
            ErrorKind::PromptCallFailed
        );
    }

    #[test]
    fn tool_execution_via_exit_code() {
        assert_eq!(c("tool exited with code 1"), ErrorKind::ToolExecutionFailed);
        assert_eq!(c("process exit code 127"), ErrorKind::ToolExecutionFailed);
    }

    #[test]
    fn http_error_status_is_tool_execution_not_tool_not_found() {
        // A 404 means the endpoint answered; the tool itself exists and ran.
        assert_eq!(
            c("tool execution failed (fetch): HTTP 404 Not Found: missing"),
            ErrorKind::ToolExecutionFailed
        );
        assert_eq!(
            c("tool execution failed (fetch): HTTP 429 Too Many Requests: slow down"),
            ErrorKind::ToolExecutionFailed
        );
    }

    #[test]
    fn http_status_takes_priority_over_transport_signals() {
        // An explicit status proves an endpoint answered, even when its reason
        // phrase or body contains a transport-like word.
        assert_eq!(
            c("HTTP 504 Gateway Timeout: upstream timed out"),
            ErrorKind::ToolExecutionFailed
        );
    }

    #[test]
    fn prompt_failure_takes_priority_over_its_http_status() {
        assert_eq!(
            c("prompt_call failed: provider returned HTTP 404 Not Found"),
            ErrorKind::PromptCallFailed
        );
    }

    #[test]
    fn tool_not_found() {
        assert_eq!(c("tool not found: jq"), ErrorKind::ToolNotFound);
        assert_eq!(
            c("no such file or directory: /usr/bin/mytool"),
            ErrorKind::ToolNotFound
        );
    }

    // ── MissingInterpreter ───────────────────────────────────────────────────

    #[test]
    fn missing_interpreter_spawn_failure() {
        // The exact error format emitted by code_call.rs when spawn returns ENOENT.
        assert_eq!(
            c(
                "step execution failed (step: fetch_current_time): failed to spawn interpreter '/usr/bin/python3': No such file or directory (os error 2)"
            ),
            ErrorKind::MissingInterpreter
        );
    }

    #[test]
    fn missing_interpreter_takes_priority_over_tool_not_found() {
        // The error contains "no such file" which would normally match ToolNotFound,
        // but the more specific "failed to spawn interpreter" pattern wins.
        assert_eq!(
            c("failed to spawn interpreter '/usr/local/bin/python3': No such file or directory"),
            ErrorKind::MissingInterpreter
        );
    }

    #[test]
    fn tool_not_found_without_interpreter_prefix_stays_tool_not_found() {
        // "no such file" without the interpreter spawn prefix stays ToolNotFound.
        assert_eq!(
            c("no such file or directory: /usr/bin/mytool"),
            ErrorKind::ToolNotFound
        );
    }

    #[test]
    fn missing_interpreter_requires_both_patterns() {
        // "failed to spawn interpreter" alone (no "not found"/"no such file") does
        // not match MissingInterpreter — could be a permission error or other cause.
        assert_eq!(
            c("failed to spawn interpreter '/usr/bin/bash': permission denied"),
            ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn code_execution_from_stderr() {
        assert_eq!(
            cs("script failed", "Traceback (most recent call last):\n  ..."),
            ErrorKind::CodeExecutionFailed
        );
        assert_eq!(
            cs("script failed", "SyntaxError: invalid syntax"),
            ErrorKind::CodeExecutionFailed
        );
        assert_eq!(
            cs("script failed", "NameError: name 'foo' is not defined"),
            ErrorKind::CodeExecutionFailed
        );
    }

    // ── ExternalEndpointDown ─────────────────────────────────────────────────
    //
    // Representative transport error strings, covering both the pre-hardening
    // reqwest `Display` text and the hardened adapter's explicit phrasing.

    #[test]
    fn external_endpoint_down_dns_failure_old_format() {
        assert_eq!(
            c(
                "HTTP request failed: error sending request for url (http://worldtimeapi.org/api/timezone/Asia/Tokyo): error trying to connect: dns error: failed to lookup address information: Temporary failure in name resolution"
            ),
            ErrorKind::ExternalEndpointDown
        );
    }

    #[test]
    fn external_endpoint_down_dns_failure_new_format() {
        assert_eq!(
            c("HTTP request failed: DNS lookup failed for host worldtimeapi.org"),
            ErrorKind::ExternalEndpointDown
        );
    }

    #[test]
    fn external_endpoint_down_connection_refused_old_format() {
        assert_eq!(
            c(
                "HTTP request failed: error sending request for url (http://localhost:9999/): error trying to connect: tcp connect error: Connection refused (os error 111)"
            ),
            ErrorKind::ExternalEndpointDown
        );
    }

    #[test]
    fn external_endpoint_down_connection_refused_new_format() {
        assert_eq!(
            c("HTTP request failed: connection refused connecting to 127.0.0.1:9999"),
            ErrorKind::ExternalEndpointDown
        );
    }

    #[test]
    fn external_endpoint_down_connection_reset() {
        assert_eq!(
            c(
                "HTTP request failed: error sending request for url (http://example.com/): connection reset by peer"
            ),
            ErrorKind::ExternalEndpointDown
        );
    }

    #[test]
    fn external_endpoint_down_timeout_old_format() {
        assert_eq!(
            c(
                "HTTP request failed: error sending request for url (http://slow.example.com/): operation timed out"
            ),
            ErrorKind::ExternalEndpointDown
        );
    }

    #[test]
    fn external_endpoint_down_timeout_new_format() {
        assert_eq!(
            c("HTTP request failed: request to http://slow.example.com/ timed out after 30s"),
            ErrorKind::ExternalEndpointDown
        );
    }

    #[test]
    fn external_endpoint_down_tls_old_format() {
        assert_eq!(
            c(
                "HTTP request failed: error sending request for url (https://expired.badssl.com/): error trying to connect: invalid peer certificate: UnknownIssuer"
            ),
            ErrorKind::ExternalEndpointDown
        );
    }

    #[test]
    fn external_endpoint_down_tls_new_format() {
        assert_eq!(
            c(
                "HTTP request failed: TLS handshake failed for https://expired.badssl.com/: certificate verify failed"
            ),
            ErrorKind::ExternalEndpointDown
        );
    }

    #[test]
    fn http_failure_without_transport_signal_is_not_endpoint_down() {
        // An HTTP-level application error (e.g. a 5xx body) is not a
        // transport failure — the endpoint answered, so "substitute the
        // endpoint" guidance would be wrong.
        assert_eq!(
            c("HTTP request failed: 500 Internal Server Error"),
            ErrorKind::Unknown
        );
    }

    #[test]
    fn code_execution_signals_in_message_only_do_not_classify() {
        // Code-execution patterns are deliberately matched against stderr
        // only; the same words in the executor's error message are too
        // generic to imply the step's own code failed.
        assert_eq!(c("Exception occurred somewhere"), ErrorKind::Unknown);
        assert_eq!(cs("script failed", ""), ErrorKind::Unknown);
    }

    #[test]
    fn generic_network_errors_without_http_marker_stay_network_error() {
        // Bare/non-HTTP transport strings should not be swept into
        // ExternalEndpointDown; that classification is specifically for
        // "an external endpoint we called over HTTP is down."
        assert_eq!(c("connection refused"), ErrorKind::NetworkError);
        assert_eq!(c("dns resolution failed"), ErrorKind::NetworkError);
        assert_eq!(c("connection reset by peer"), ErrorKind::NetworkError);
    }

    #[test]
    fn output_mapping_failure_classifies_as_output_schema_violation() {
        // The exact message format emitted by code_call.rs when stdout cannot
        // be mapped onto multiple declared outputs.
        assert_eq!(
            c(
                "script succeeded but its stdout cannot satisfy the declared outputs (branch_name, staged_diff): stdout is not a valid JSON object (lone leading surrogate in hex escape at line 1 column 54792). A CODE_CALL step with more than one declared output must print exactly one JSON object whose keys are the declared output names."
            ),
            ErrorKind::OutputSchemaViolation
        );
    }

    #[test]
    fn output_mapping_wins_over_words_inside_the_embedded_parse_error() {
        // The embedded serde error is free-form; even when it mentions e.g.
        // a timeout-like word, the mapping classification must win.
        assert_eq!(
            c(
                "script succeeded but its stdout cannot satisfy the declared outputs (a, b): stdout is not a valid JSON object (expected value at line 1 column 1; raw text was 'connection timed out')."
            ),
            ErrorKind::OutputSchemaViolation
        );
    }

    #[test]
    fn unknown_fallback() {
        assert_eq!(c("something went wrong"), ErrorKind::Unknown);
        assert_eq!(c(""), ErrorKind::Unknown);
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(c("TIMED OUT"), ErrorKind::TimeoutExceeded);
        assert_eq!(c("Permission Denied"), ErrorKind::PermissionDenied);
    }
}
