//! HTTP adapter — calls a remote endpoint as a tool.
//!
//! URL construction:
//!
//! 1. Replace `{key}` placeholders in `config.path_template` with matching
//!    argument values.
//! 2. For GET / DELETE / HEAD / OPTIONS: attach remaining arguments as query
//!    parameters.
//! 3. For POST / PUT / PATCH: send remaining arguments as a JSON body.

use crate::error::ToolError;
use crate::tools::ToolOutput;
use crate::tools::catalog::HttpConfig;
use indexmap::IndexMap;
use std::time::Duration;

// ─── Defaults ─────────────────────────────────────────────────────────────────
//
// `HttpConfig` currently only exposes a single `timeout_secs` (total request
// budget) that is shared across other modules (`app/engine.rs`,
// `app/views/mcp.rs`, `compiler/prompt.rs`) via struct-literal construction.
// Adding further fields to `HttpConfig` would require touching those
// call sites, which are outside this module's ownership — so connect
// timeout and retry policy are kept as internal constants for now rather
// than new per-tool config fields.
/// Upper bound on how long the TCP connect phase may take, independent of
/// the overall request timeout.
const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
/// Total request budget (connect + send + receive) when the tool/caller
/// doesn't specify one.
const DEFAULT_TOTAL_TIMEOUT_SECS: u64 = 30;
/// Initial attempt + this many retries on transport errors and 5xx responses.
const MAX_RETRIES: u32 = 2;
const MAX_ATTEMPTS: u32 = MAX_RETRIES + 1;
/// Base delay for exponential backoff between retries.
const RETRY_BASE_DELAY_MS: u64 = 200;
/// How much of a non-success response body to include in the error message.
const ERROR_BODY_EXCERPT_MAX_CHARS: usize = 1_000;
/// Maximum successful or error response body retained from an HTTP tool.
const MAX_HTTP_BODY_BYTES: usize = 1_048_576;
const OUTPUT_LIMIT_MARKER: &str = "[truncated: HTTP body limit exceeded]";

fn backoff_delay(attempt: u32) -> Duration {
    // attempt is 1-based (the attempt that just failed); backoff before the
    // next one grows as 200ms, 400ms, 800ms, ...
    Duration::from_millis(RETRY_BASE_DELAY_MS * 2u64.saturating_pow(attempt.saturating_sub(1)))
}

// ─── Public entry point ───────────────────────────────────────────────────────

pub async fn run(
    config: &HttpConfig,
    arguments: &IndexMap<String, serde_json::Value>,
    timeout_secs: Option<u64>,
) -> Result<ToolOutput, ToolError> {
    // Effective timeout: caller-supplied takes precedence over tool config,
    // falling back to a sensible default so requests never hang forever.
    let effective_timeout = timeout_secs
        .or(config.timeout_secs)
        .unwrap_or(DEFAULT_TOTAL_TIMEOUT_SECS);
    let connect_timeout = effective_timeout.min(DEFAULT_CONNECT_TIMEOUT_SECS);

    let (url, remaining) =
        build_url(&config.base_url, &config.path_template, arguments).map_err(|message| {
            ToolError::Execution {
                tool: "http".to_owned(),
                message,
            }
        })?;

    let method = parse_method(&config.method, &url)?;

    let client = reqwest::Client::builder()
        .user_agent(format!(
            "{}/{}",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION")
        ))
        .connect_timeout(Duration::from_secs(connect_timeout))
        .build()
        .map_err(|e| ToolError::Execution {
            tool: url.clone(),
            message: format!("failed to build HTTP client: {e}"),
        })?;

    tokio::time::timeout(
        Duration::from_secs(effective_timeout),
        execute_with_retries(&client, config, &method, &url, &remaining),
    )
    .await
    .map_err(|_| ToolError::timeout(url, effective_timeout))?
}

async fn execute_with_retries(
    client: &reqwest::Client,
    config: &HttpConfig,
    method: &reqwest::Method,
    url: &str,
    remaining: &IndexMap<String, serde_json::Value>,
) -> Result<ToolOutput, ToolError> {
    let retry_allowed = method_is_idempotent(method) || has_idempotency_key(&config.headers);
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        tracing::Span::current().record("tool.attempt_count", attempt);
        tracing::Span::current().record("tool.retry_count", attempt.saturating_sub(1));

        let mut request = client.request(method.clone(), url);

        // Attach arguments: query params for read-only methods, JSON body for
        // mutating methods.
        if sends_arguments_as_query(method) {
            for (key, value) in remaining {
                request = request.query(&[(key.as_str(), json_value_to_str(value))]);
            }
        } else {
            request = request.json(&remaining);
        }

        // Custom headers from the tool definition.
        for (name, value) in &config.headers {
            request = request.header(name.as_str(), value.as_str());
        }

        match request.send().await {
            Ok(response) => {
                let status = response.status();

                if status.is_server_error() && retry_allowed && attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(backoff_delay(attempt)).await;
                    continue;
                }

                let body = read_body_limited(response, url).await?;

                if !status.is_success() {
                    let excerpt: String = body.chars().take(ERROR_BODY_EXCERPT_MAX_CHARS).collect();
                    let message = if attempt > 1 {
                        format!("HTTP {status} after {attempt} attempt(s): {excerpt}")
                    } else {
                        format!("HTTP {status}: {excerpt}")
                    };
                    return Err(ToolError::Execution {
                        tool: url.to_owned(),
                        message,
                    });
                }

                let data = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);

                return Ok(ToolOutput {
                    stdout: body,
                    stderr: String::new(),
                    exit_code: 0,
                    data,
                });
            }
            Err(error) => {
                if is_retryable(&error) && retry_allowed && attempt < MAX_ATTEMPTS {
                    tokio::time::sleep(backoff_delay(attempt)).await;
                    continue;
                }

                let description = describe_transport_error(&error);
                return Err(ToolError::Execution {
                    tool: url.to_owned(),
                    message: format!(
                        "HTTP request failed after {attempt} attempt(s): {description}"
                    ),
                });
            }
        }
    }
}

async fn read_body_limited(
    mut response: reqwest::Response,
    url: &str,
) -> Result<String, ToolError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| ToolError::Execution {
            tool: url.to_owned(),
            message: format!("failed to read response body: {error}"),
        })?
    {
        if body.len().saturating_add(chunk.len()) > MAX_HTTP_BODY_BYTES {
            tracing::Span::current().record("tool.output_limit_violation", true);
            return Err(ToolError::Execution {
                tool: url.to_owned(),
                message: format!("{OUTPUT_LIMIT_MARKER}; maximum is {MAX_HTTP_BODY_BYTES} bytes"),
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn method_is_idempotent(method: &reqwest::Method) -> bool {
    matches!(
        *method,
        reqwest::Method::GET
            | reqwest::Method::HEAD
            | reqwest::Method::OPTIONS
            | reqwest::Method::PUT
            | reqwest::Method::DELETE
    )
}

fn has_idempotency_key(headers: &IndexMap<String, String>) -> bool {
    headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("idempotency-key") && !value.trim().is_empty()
    })
}

// ─── Transport error classification ──────────────────────────────────────────

/// Whether a transport-level send error is worth retrying: connection
/// establishment failures and timeouts, but not e.g. body-encoding errors.
fn is_retryable(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

/// Turn a reqwest transport error into a diagnosable message that separates
/// DNS failures, refused connections, timeouts, and TLS errors — instead of
/// the raw (and largely useless) `error sending request` display text.
fn describe_transport_error(error: &reqwest::Error) -> String {
    // Walk the full source chain: the useful detail (DNS, refused, TLS) is
    // usually a few levels down inside the hyper/hyper-util/rustls errors.
    let mut chain = String::new();
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    let mut first = true;
    while let Some(err) = source {
        if !first {
            chain.push_str(" -> ");
        }
        chain.push_str(&err.to_string());
        first = false;
        source = err.source();
    }
    let lower = chain.to_lowercase();

    if error.is_timeout() {
        return format!("timed out waiting for a response ({chain})");
    }

    let looks_like_dns = lower.contains("dns")
        || lower.contains("lookup")
        || lower.contains("resolve")
        || lower.contains("name or service not known")
        || lower.contains("nodename nor servname");
    let looks_like_tls = lower.contains("tls")
        || lower.contains("ssl")
        || lower.contains("certificate")
        || lower.contains("handshake");
    let looks_like_refused = lower.contains("connection refused") || lower.contains("econnrefused");

    if error.is_connect() {
        if looks_like_dns {
            return format!("DNS resolution failed ({chain})");
        }
        if looks_like_tls {
            return format!("TLS handshake failed ({chain})");
        }
        if looks_like_refused {
            return format!("connection refused ({chain})");
        }
        return format!("connection failed ({chain})");
    }

    if looks_like_tls {
        return format!("TLS handshake failed ({chain})");
    }
    if looks_like_dns {
        return format!("DNS resolution failed ({chain})");
    }

    format!("request failed ({chain})")
}

// ─── Pure helpers (also tested below) ────────────────────────────────────────

/// Whether arguments should be attached as query parameters (`true`) or sent
/// as a JSON body (`false`) for this method.
fn sends_arguments_as_query(method: &reqwest::Method) -> bool {
    [
        reqwest::Method::GET,
        reqwest::Method::DELETE,
        reqwest::Method::HEAD,
        reqwest::Method::OPTIONS,
    ]
    .contains(method)
}

/// Substitute `{key}` placeholders in `path_template` with values from
/// `arguments`.  Returns the final URL and the arguments that were **not**
/// consumed by a placeholder (to be used as query params or body).
fn build_url(
    base_url: &str,
    path_template: &str,
    arguments: &IndexMap<String, serde_json::Value>,
) -> Result<(String, IndexMap<String, serde_json::Value>), String> {
    if base_url.is_empty() && path_template == "{url}" {
        let target = arguments
            .get("url")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "whole-target HTTP mode requires string argument 'url'".to_owned())?;
        let parsed = reqwest::Url::parse(target)
            .map_err(|error| format!("invalid whole-target URL: {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err("whole-target URL must use http or https".to_owned());
        }
        let remaining = arguments
            .iter()
            .filter(|(key, _)| key.as_str() != "url")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        return Ok((target.to_owned(), remaining));
    }

    let mut path = path_template.to_owned();
    let mut remaining: IndexMap<String, serde_json::Value> = IndexMap::new();

    for (key, value) in arguments {
        let placeholder = format!("{{{key}}}");
        if path.contains(placeholder.as_str()) {
            path = path.replace(
                placeholder.as_str(),
                &percent_encode_segment(&json_value_to_str(value)),
            );
        } else {
            remaining.insert(key.clone(), value.clone());
        }
    }

    if path.contains('{') || path.contains('}') {
        return Err("HTTP path_template contains an unresolved placeholder".to_owned());
    }
    let url = format!("{}{}", base_url.trim_end_matches('/'), path);
    Ok((url, remaining))
}

fn percent_encode_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn parse_method(method: &str, url: &str) -> Result<reqwest::Method, ToolError> {
    match method.to_uppercase().as_str() {
        "GET" => Ok(reqwest::Method::GET),
        "POST" => Ok(reqwest::Method::POST),
        "PUT" => Ok(reqwest::Method::PUT),
        "DELETE" => Ok(reqwest::Method::DELETE),
        "PATCH" => Ok(reqwest::Method::PATCH),
        "HEAD" => Ok(reqwest::Method::HEAD),
        "OPTIONS" => Ok(reqwest::Method::OPTIONS),
        other => Err(ToolError::Execution {
            tool: url.to_owned(),
            message: format!("unsupported HTTP method: {other}"),
        }),
    }
}

fn json_value_to_str(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, serde_json::Value)]) -> IndexMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    async fn one_shot_server(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8_192];
            let size = stream.read(&mut request).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&request[..size]).into_owned()
        });
        (format!("http://{address}"), task)
    }

    fn config(base_url: String) -> HttpConfig {
        HttpConfig {
            base_url,
            method: "GET".to_owned(),
            path_template: String::new(),
            headers: IndexMap::new(),
            timeout_secs: Some(5),
        }
    }

    #[tokio::test]
    async fn sends_default_user_agent() {
        let (url, request) =
            one_shot_server("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await;
        run(&config(url), &IndexMap::new(), None).await.unwrap();
        let request = request.await.unwrap().to_ascii_lowercase();
        assert!(request.contains("user-agent: inxm-local/"));
    }

    #[tokio::test]
    async fn non_success_status_is_an_error() {
        let (url, _) = one_shot_server(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 7\r\nConnection: close\r\n\r\nmissing",
        )
        .await;
        let error = run(&config(url), &IndexMap::new(), None).await.unwrap_err();
        assert!(error.to_string().contains("HTTP 404 Not Found: missing"));
    }

    /// Accepts connections one at a time, replying with each response in
    /// order — used to simulate a flaky server that fails then recovers.
    async fn sequential_server(responses: &'static [&'static str]) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 8_192];
                let _ = stream.read(&mut request).await.unwrap();
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        format!("http://{address}")
    }

    async fn counting_server(
        response: Option<&'static str>,
    ) -> (String, tokio::task::JoinHandle<usize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut count = 0;
            while let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(Duration::from_millis(500), listener.accept()).await
            {
                count += 1;
                let mut request = vec![0; 8_192];
                let _ = stream.read(&mut request).await.unwrap();
                if let Some(response) = response {
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            }
            count
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn retries_on_5xx_then_succeeds() {
        let url = sequential_server(&[
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\noops!",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        ])
        .await;

        let output = run(&config(url), &IndexMap::new(), None).await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.stdout, "{}");
    }

    #[tokio::test]
    async fn persistent_5xx_reports_attempts_exhausted() {
        let responses: &'static [&'static str] = &[
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\noops!",
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\noops!",
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\noops!",
        ];
        let url = sequential_server(responses).await;

        let error = run(&config(url), &IndexMap::new(), None).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("HTTP 500"));
        assert!(message.contains("after 3 attempt(s)"));
    }

    #[tokio::test]
    async fn post_does_not_retry_5xx_without_idempotency_key() {
        let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\noops!";
        let (url, requests) = counting_server(Some(response)).await;
        let mut cfg = config(url);
        cfg.method = "POST".to_owned();

        let error = run(&cfg, &IndexMap::new(), None).await.unwrap_err();
        assert!(error.to_string().contains("HTTP 500"));
        assert_eq!(requests.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn post_does_not_retry_when_response_is_lost() {
        let (url, requests) = counting_server(None).await;
        let mut cfg = config(url);
        cfg.method = "POST".to_owned();

        let error = run(&cfg, &IndexMap::new(), None).await.unwrap_err();
        assert!(error.to_string().contains("after 1 attempt(s)"));
        assert_eq!(requests.await.unwrap(), 1);
    }

    #[tokio::test]
    async fn post_with_idempotency_key_can_retry() {
        let url = sequential_server(&[
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\noops!",
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        ])
        .await;
        let mut cfg = config(url);
        cfg.method = "POST".to_owned();
        cfg.headers
            .insert("Idempotency-Key".to_owned(), "request-1".to_owned());

        let output = run(&cfg, &IndexMap::new(), None).await.unwrap();
        assert_eq!(output.stdout, "{}");
    }

    #[tokio::test]
    async fn connection_never_responding_times_out_with_clear_message() {
        // A listener that is bound but never accepted — the TCP handshake
        // completes (the connection sits in the OS accept backlog) but no
        // HTTP response is ever sent, so the client's overall request
        // timeout should fire.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        let mut cfg = config(format!("http://{address}"));
        cfg.timeout_secs = Some(1); // keep the test fast

        let started = tokio::time::Instant::now();
        let error = run(&cfg, &IndexMap::new(), None).await.unwrap_err();
        assert!(matches!(error, ToolError::Timeout { secs: 1, .. }));
        assert!(
            started.elapsed() < Duration::from_millis(1_500),
            "configured total deadline was exceeded"
        );

        drop(listener);
    }

    #[tokio::test]
    async fn oversized_http_body_is_rejected_without_raw_excess() {
        let body = "sensitive-body".repeat((MAX_HTTP_BODY_BYTES / 14) + 2);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let response: &'static str = Box::leak(response.into_boxed_str());
        let (url, _) = one_shot_server(response).await;

        let error = run(&config(url), &IndexMap::new(), None).await.unwrap_err();
        let message = error.to_string();
        assert!(message.contains(OUTPUT_LIMIT_MARKER));
        assert!(!message.contains("sensitive-body"));
    }

    #[test]
    fn backoff_delay_grows_with_each_attempt() {
        assert!(backoff_delay(1) < backoff_delay(2));
        assert!(backoff_delay(2) < backoff_delay(3));
    }

    // ── sends_arguments_as_query ─────────────────────────────────────────────

    #[test]
    fn read_methods_send_arguments_as_query() {
        assert!(sends_arguments_as_query(&reqwest::Method::GET));
        assert!(sends_arguments_as_query(&reqwest::Method::DELETE));
        assert!(sends_arguments_as_query(&reqwest::Method::HEAD));
        assert!(sends_arguments_as_query(&reqwest::Method::OPTIONS));
    }

    #[test]
    fn mutating_methods_send_arguments_as_body() {
        assert!(!sends_arguments_as_query(&reqwest::Method::POST));
        assert!(!sends_arguments_as_query(&reqwest::Method::PUT));
        assert!(!sends_arguments_as_query(&reqwest::Method::PATCH));
    }

    // ── build_url ────────────────────────────────────────────────────────────

    #[test]
    fn url_no_template_no_args() {
        let (url, remaining) = build_url("https://example.com", "/ping", &IndexMap::new()).unwrap();
        assert_eq!(url, "https://example.com/ping");
        assert!(remaining.is_empty());
    }

    #[test]
    fn url_template_single_substitution() {
        let arguments = args(&[("id", serde_json::json!(42))]);
        let (url, remaining) =
            build_url("https://api.example.com", "/users/{id}", &arguments).unwrap();
        assert_eq!(url, "https://api.example.com/users/42");
        assert!(remaining.is_empty());
    }

    #[test]
    fn url_template_string_substitution() {
        let arguments = args(&[("slug", serde_json::json!("hello-world"))]);
        let (url, remaining) =
            build_url("https://api.example.com", "/posts/{slug}", &arguments).unwrap();
        assert_eq!(url, "https://api.example.com/posts/hello-world");
        assert!(remaining.is_empty());
    }

    #[test]
    fn ordinary_path_placeholder_is_percent_encoded() {
        let arguments = args(&[("slug", serde_json::json!("folder/a b?token=x"))]);
        let (url, remaining) =
            build_url("https://api.example.com", "/files/{slug}", &arguments).unwrap();
        assert_eq!(
            url,
            "https://api.example.com/files/folder%2Fa%20b%3Ftoken%3Dx"
        );
        assert!(remaining.is_empty());
    }

    #[test]
    fn whole_target_url_mode_preserves_valid_url() {
        let arguments = args(&[(
            "url",
            serde_json::json!("https://example.com/a/b?query=value"),
        )]);
        let (url, remaining) = build_url("", "{url}", &arguments).unwrap();
        assert_eq!(url, "https://example.com/a/b?query=value");
        assert!(remaining.is_empty());
    }

    #[test]
    fn url_template_multiple_placeholders() {
        let arguments = args(&[
            ("org", serde_json::json!("acme")),
            ("repo", serde_json::json!("widget")),
        ]);
        let (url, remaining) = build_url(
            "https://api.example.com",
            "/orgs/{org}/repos/{repo}",
            &arguments,
        )
        .unwrap();
        assert_eq!(url, "https://api.example.com/orgs/acme/repos/widget");
        assert!(remaining.is_empty());
    }

    #[test]
    fn url_non_template_args_go_to_remaining() {
        let arguments = args(&[
            ("id", serde_json::json!(7)),
            ("format", serde_json::json!("json")),
        ]);
        let (url, remaining) =
            build_url("https://api.example.com", "/items/{id}", &arguments).unwrap();
        assert_eq!(url, "https://api.example.com/items/7");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining["format"], serde_json::json!("json"));
    }

    #[test]
    fn url_base_trailing_slash_normalised() {
        let arguments = args(&[("id", serde_json::json!(1))]);
        let (url, _) = build_url("https://api.example.com/", "/items/{id}", &arguments).unwrap();
        // Should not produce double slash.
        assert_eq!(url, "https://api.example.com/items/1");
    }

    #[test]
    fn url_empty_path_template() {
        let (url, _) = build_url("https://example.com", "", &IndexMap::new()).unwrap();
        assert_eq!(url, "https://example.com");
    }

    // ── parse_method ─────────────────────────────────────────────────────────

    #[test]
    fn parse_method_case_insensitive() {
        assert_eq!(parse_method("get", "url").unwrap(), reqwest::Method::GET);
        assert_eq!(parse_method("POST", "url").unwrap(), reqwest::Method::POST);
        assert_eq!(
            parse_method("Patch", "url").unwrap(),
            reqwest::Method::PATCH
        );
    }

    #[test]
    fn parse_method_unknown_returns_error() {
        assert!(parse_method("BREW", "url").is_err());
    }

    // ── json_value_to_str ────────────────────────────────────────────────────

    #[test]
    fn string_value_is_unwrapped() {
        assert_eq!(json_value_to_str(&serde_json::json!("foo")), "foo");
    }

    #[test]
    fn number_value_is_serialised() {
        assert_eq!(json_value_to_str(&serde_json::json!(2.75)), "2.75");
    }

    #[test]
    fn bool_value_is_serialised() {
        assert_eq!(json_value_to_str(&serde_json::json!(true)), "true");
    }
}
