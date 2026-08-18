//! OAuth lifecycle and secure credential storage for outbound MCP connections.
//!
//! UI code only needs [`McpOAuthFacade`]. Tests can replace both the credential
//! store and OAuth HTTP client through [`McpOAuthFacade::with_boundaries`].

use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, OAuthState, StoredCredentials,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

pub use rmcp::transport::auth::{
    CredentialStore, InMemoryCredentialStore, OAuthHttpClient, OAuthHttpClientFuture,
    OAuthHttpRedirectPolicy, OAuthHttpRequest,
};

const VAULT_SERVICE: &str = "inxm-local-mcp-oauth";
const VAULT_DOCUMENT_VERSION: u8 = 1;
const RECONNECT_REMEDIATION: &str = "Reconnect under MCP Tools";
const CLIENT_NAME: &str = "inxm-local";

/// Windows' Credential Manager caps a single generic credential's secret at
/// 2560 bytes once encoded as UTF-16 (`CRED_MAX_CREDENTIAL_BLOB_SIZE`).
/// A `StoredCredentials` document holding real access/refresh tokens
/// routinely exceeds that (JWT-style tokens alone are often 1-2 KB each),
/// so it is split across multiple entries below. macOS Keychain and Linux
/// Secret Service have no comparable limit, but the same scheme works
/// everywhere, so every platform uses it rather than special-casing Windows.
const VAULT_CHUNK_BYTES: usize = 1000;

/// Coarse connection status suitable for rendering in the MCP Tools UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthConnectionStatus {
    Disconnected,
    AuthorizationPending,
    Connected,
}

/// Values returned when an authorization-code flow begins.
///
/// `state` is opaque and must be passed back unchanged to
/// [`McpOAuthFacade::complete_authorization`].
#[derive(Clone, PartialEq, Eq)]
pub struct AuthorizationStart {
    pub authorization_url: String,
    pub state: String,
}

/// Errors from the tools-owned OAuth boundary. Messages deliberately exclude
/// authorization codes, PKCE verifiers, and token values.
#[derive(Debug, thiserror::Error)]
pub enum McpOAuthError {
    #[error("invalid MCP OAuth endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("MCP OAuth authorization failed; {RECONNECT_REMEDIATION}")]
    AuthorizationRequired,
    #[error("MCP OAuth credential vault is unavailable: {0}")]
    Vault(String),
    #[error("MCP OAuth operation failed: {0}")]
    Operation(String),
}

/// OAuth lifecycle facade owned by the tools module.
///
/// It performs discovery, DCR (only when no configured client ID exists),
/// authorization-code exchange with S256 PKCE, refresh, and disconnect. It
/// never opens a browser; callers decide how to present `authorization_url`.
pub struct McpOAuthFacade {
    endpoint: String,
    client_id: Option<String>,
    store: Arc<dyn CredentialStore>,
    http: Option<Arc<dyn OAuthHttpClient>>,
    state: Mutex<OAuthState>,
}

impl McpOAuthFacade {
    /// Construct the production facade backed exclusively by the OS keyring.
    pub async fn production(
        endpoint: &str,
        client_id: Option<String>,
    ) -> Result<Self, McpOAuthError> {
        let endpoint = canonical_endpoint(endpoint)?;
        let store: Arc<dyn CredentialStore> =
            Arc::new(KeyringCredentialStore::new(endpoint.clone()));
        let manager = build_manager(&endpoint, client_id.as_deref(), &store, None).await?;
        Ok(Self {
            endpoint,
            client_id,
            store,
            http: None,
            state: Mutex::new(OAuthState::Unauthorized(manager)),
        })
    }

    /// Construct a facade with deterministic credential and HTTP boundaries.
    pub async fn with_boundaries(
        endpoint: &str,
        client_id: Option<String>,
        store: Arc<dyn CredentialStore>,
        http: Arc<dyn OAuthHttpClient>,
    ) -> Result<Self, McpOAuthError> {
        let endpoint = canonical_endpoint(endpoint)?;
        let manager = build_manager(
            &endpoint,
            client_id.as_deref(),
            &store,
            Some(Arc::clone(&http)),
        )
        .await?;
        Ok(Self {
            endpoint,
            client_id,
            store,
            http: Some(http),
            state: Mutex::new(OAuthState::Unauthorized(manager)),
        })
    }

    /// Return current status without initiating network or browser activity.
    pub async fn connection_status(&self) -> Result<OAuthConnectionStatus, McpOAuthError> {
        if matches!(*self.state.lock().await, OAuthState::Session(_)) {
            return Ok(OAuthConnectionStatus::AuthorizationPending);
        }
        let stored = self.store.load().await.map_err(map_auth_error)?;
        Ok(
            if stored.is_some_and(|value| value.token_response.is_some()) {
                OAuthConnectionStatus::Connected
            } else {
                OAuthConnectionStatus::Disconnected
            },
        )
    }

    /// Begin interactive authorization and return a URL plus opaque CSRF state.
    pub async fn begin_authorization(
        &self,
        loopback_redirect_uri: &str,
    ) -> Result<AuthorizationStart, McpOAuthError> {
        self.begin_authorization_with_challenge(loopback_redirect_uri, None)
            .await
    }

    /// Begin authorization using a `WWW-Authenticate` challenge captured from
    /// the MCP endpoint. This preserves challenged scopes and a protected
    /// resource metadata pointer while keeping browser interaction with the UI.
    pub async fn begin_authorization_with_challenge(
        &self,
        loopback_redirect_uri: &str,
        www_authenticate: Option<&str>,
    ) -> Result<AuthorizationStart, McpOAuthError> {
        validate_loopback_redirect(loopback_redirect_uri)?;
        let mut state = self.state.lock().await;
        if !matches!(*state, OAuthState::Unauthorized(_)) {
            let manager = build_manager(
                &self.endpoint,
                self.client_id.as_deref(),
                &self.store,
                self.http.as_ref().map(Arc::clone),
            )
            .await?;
            *state = OAuthState::Unauthorized(manager);
        }
        let mut request =
            AuthorizationRequest::new(loopback_redirect_uri).with_client_name(CLIENT_NAME);
        if let Some(client_id) = &self.client_id {
            request = request.with_preregistered_client(client_id);
        }
        if let Some(challenge) = www_authenticate {
            request = request.with_challenge(challenge);
        }
        state
            .start_authorization(request)
            .await
            .map_err(map_auth_error)?;
        let authorization_url = state
            .get_authorization_url()
            .await
            .map_err(map_auth_error)?;
        let state_value = reqwest::Url::parse(&authorization_url)
            .ok()
            .and_then(|url| {
                url.query_pairs()
                    .find(|(name, _)| name == "state")
                    .map(|(_, value)| value.into_owned())
            })
            .ok_or_else(|| {
                McpOAuthError::Operation("authorization response omitted state".into())
            })?;
        Ok(AuthorizationStart {
            authorization_url,
            state: state_value,
        })
    }

    /// Complete the pending flow. The code and state are never logged or
    /// included in returned errors.
    pub async fn complete_authorization(
        &self,
        code: &str,
        state_value: &str,
    ) -> Result<(), McpOAuthError> {
        self.state
            .lock()
            .await
            .handle_callback(code, state_value)
            .await
            .map_err(map_auth_error)
    }

    /// Delete credentials from the OS vault and reset the local lifecycle.
    pub async fn disconnect(&self) -> Result<(), McpOAuthError> {
        self.store.clear().await.map_err(map_auth_error)?;
        let manager = build_manager(
            &self.endpoint,
            self.client_id.as_deref(),
            &self.store,
            self.http.as_ref().map(Arc::clone),
        )
        .await?;
        *self.state.lock().await = OAuthState::Unauthorized(manager);
        Ok(())
    }

    /// Obtain an access token, proactively refreshing near-expiry credentials.
    /// Execution callers use this method but never begin authorization.
    pub(crate) async fn access_token(&self) -> Result<String, McpOAuthError> {
        let state = self.state.lock().await;
        let result = match &*state {
            OAuthState::Unauthorized(manager) | OAuthState::Authorized(manager) => {
                manager.get_access_token().await
            }
            _ => Err(AuthError::AuthorizationRequired),
        };
        match result {
            Ok(token) => Ok(token),
            Err(AuthError::AuthorizationRequired | AuthError::TokenRefreshRejected(_)) => {
                self.store.clear().await.map_err(map_auth_error)?;
                Err(McpOAuthError::AuthorizationRequired)
            }
            Err(error) => Err(map_auth_error(error)),
        }
    }

    /// Force a refresh after a server rejects a token. Refresh-token rejection
    /// clears the unusable vault entry.
    pub(crate) async fn refresh_after_unauthorized(&self) -> Result<String, McpOAuthError> {
        let state = self.state.lock().await;
        let result = match &*state {
            OAuthState::Unauthorized(manager) | OAuthState::Authorized(manager) => {
                match manager.refresh_token().await {
                    Ok(_) => manager.get_access_token().await,
                    Err(error) => Err(error),
                }
            }
            _ => Err(AuthError::AuthorizationRequired),
        };
        match result {
            Ok(token) => Ok(token),
            Err(AuthError::TokenRefreshRejected(_)) | Err(AuthError::AuthorizationRequired) => {
                self.store.clear().await.map_err(map_auth_error)?;
                Err(McpOAuthError::AuthorizationRequired)
            }
            Err(error) => Err(map_auth_error(error)),
        }
    }
}

async fn build_manager(
    endpoint: &str,
    configured_client_id: Option<&str>,
    store: &Arc<dyn CredentialStore>,
    http: Option<Arc<dyn OAuthHttpClient>>,
) -> Result<AuthorizationManager, McpOAuthError> {
    crate::tools::ensure_tls_crypto_provider_installed();
    if let (Some(configured), Some(stored)) = (
        configured_client_id,
        store.load().await.map_err(map_auth_error)?,
    ) && stored.client_id != configured
    {
        store.clear().await.map_err(map_auth_error)?;
    }
    let mut manager = match http {
        Some(http) => AuthorizationManager::new_with_oauth_http_client(endpoint, http).await,
        None => AuthorizationManager::new(endpoint).await,
    }
    .map_err(map_auth_error)?;
    manager.set_credential_store(SharedCredentialStore(Arc::clone(store)));
    manager
        .initialize_from_store()
        .await
        .map_err(map_auth_error)?;
    Ok(manager)
}

fn map_auth_error(error: AuthError) -> McpOAuthError {
    match error {
        AuthError::AuthorizationRequired | AuthError::TokenRefreshRejected(_) => {
            McpOAuthError::AuthorizationRequired
        }
        AuthError::InternalError(message) if message.contains("credential") => {
            McpOAuthError::Vault("OS credential service operation failed".to_owned())
        }
        // The variants enriched below carry only server-side metadata/registration
        // detail (issuers, scopes, HTTP status) that predates any code/token
        // exchange, so it is safe to surface verbatim. Anything that could echo an
        // authorization code, PKCE verifier, or token (TokenExchangeFailed,
        // TokenRefreshFailed, OAuthError, ...) stays on the generic message below.
        AuthError::RegistrationFailed(detail) => McpOAuthError::Operation(format!(
            "the server rejected dynamic client registration ({detail}); it likely \
             requires a pre-registered client ID rather than auto-registration — set \
             one in the tool's OAuth settings"
        )),
        AuthError::NoAuthorizationSupport => McpOAuthError::Operation(
            "the server does not advertise OAuth support at this endpoint".to_owned(),
        ),
        AuthError::MetadataError(detail) => McpOAuthError::Operation(format!(
            "could not read the server's OAuth discovery metadata ({detail})"
        )),
        AuthError::PkceUnsupported => McpOAuthError::Operation(
            "the server does not support the required PKCE code challenge method (S256)".to_owned(),
        ),
        AuthError::AuthorizationServerMismatch {
            expected_issuer,
            received_issuer,
        } => McpOAuthError::Operation(format!(
            "the authorization server's issuer ({received_issuer}) did not match the \
             expected issuer ({expected_issuer})"
        )),
        AuthError::AuthorizationServerMissingIssuer { expected_issuer } => {
            McpOAuthError::Operation(format!(
                "the authorization server's metadata is missing the expected issuer \
                 ({expected_issuer})"
            ))
        }
        AuthError::InsufficientScope { required_scope, .. } => McpOAuthError::Operation(format!(
            "the server requires additional scope: {required_scope}"
        )),
        AuthError::InvalidScope(detail) => {
            McpOAuthError::Operation(format!("the requested scope was rejected ({detail})"))
        }
        AuthError::AuthorizationFailed(detail) => McpOAuthError::Operation(format!(
            "the authorization server rejected the authorization request ({detail})"
        )),
        AuthError::HttpError(source) => McpOAuthError::Operation(format!(
            "a network error occurred while contacting the authorization server ({source})"
        )),
        _ => McpOAuthError::Operation("authorization server request was rejected".to_owned()),
    }
}

fn canonical_endpoint(endpoint: &str) -> Result<String, McpOAuthError> {
    let url = reqwest::Url::parse(endpoint)
        .map_err(|_| McpOAuthError::InvalidEndpoint("URL is malformed".to_owned()))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(McpOAuthError::InvalidEndpoint(
            "URL must be absolute HTTP(S)".to_owned(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(McpOAuthError::InvalidEndpoint(
            "userinfo and fragments are forbidden".to_owned(),
        ));
    }
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if url.scheme() != "https" && !loopback {
        return Err(McpOAuthError::InvalidEndpoint(
            "OAuth requires HTTPS except on loopback hosts".to_owned(),
        ));
    }
    Ok(url.to_string())
}

fn validate_loopback_redirect(value: &str) -> Result<(), McpOAuthError> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| McpOAuthError::InvalidEndpoint("redirect URI is malformed".to_owned()))?;
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if url.scheme() != "http" || !loopback || !url.username().is_empty() || url.fragment().is_some()
    {
        return Err(McpOAuthError::InvalidEndpoint(
            "redirect URI must be an HTTP loopback URL".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Clone)]
struct SharedCredentialStore(Arc<dyn CredentialStore>);

impl CredentialStore for SharedCredentialStore {
    fn load<'life0, 'async_trait>(
        &'life0 self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<StoredCredentials>, AuthError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { self.0.load().await })
    }

    fn save<'life0, 'async_trait>(
        &'life0 self,
        credentials: StoredCredentials,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), AuthError>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { self.0.save(credentials).await })
    }

    fn clear<'life0, 'async_trait>(
        &'life0 self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), AuthError>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { self.0.clear().await })
    }
}

#[derive(Clone)]
struct KeyringCredentialStore {
    account: String,
}

#[derive(Serialize, Deserialize)]
struct VaultDocument {
    version: u8,
    credentials: StoredCredentials,
}

/// Small pointer entry, stored under the bare account, that records how
/// many chunk entries the actual document was split across.
#[derive(Serialize, Deserialize)]
struct VaultManifest {
    version: u8,
    chunk_count: usize,
}

/// Split `raw` into UTF-8-boundary-safe pieces no larger than `max_bytes`.
/// Bounding by UTF-8 byte length is always at least as strict as bounding by
/// UTF-16 code units (the unit that Windows' blob limit is measured in),
/// since every UTF-16 unit costs at least one UTF-8 byte.
fn chunk_str(raw: &str, max_bytes: usize) -> Vec<&str> {
    if raw.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < raw.len() {
        let mut end = (start + max_bytes).min(raw.len());
        while end < raw.len() && !raw.is_char_boundary(end) {
            end -= 1;
        }
        chunks.push(&raw[start..end]);
        start = end;
    }
    chunks
}

impl KeyringCredentialStore {
    fn new(account: String) -> Self {
        Self { account }
    }

    /// The manifest entry, keyed on the bare account. A pre-chunking install
    /// would have stored the full document directly under this same key, so
    /// `load` falls back to parsing it as a legacy `VaultDocument`.
    fn manifest_entry(&self) -> Result<keyring::v1::Entry, AuthError> {
        self.entry_for(&self.account)
    }

    fn chunk_entry(&self, index: usize) -> Result<keyring::v1::Entry, AuthError> {
        self.entry_for(&format!("{}#chunk#{index}", self.account))
    }

    fn entry_for(&self, account: &str) -> Result<keyring::v1::Entry, AuthError> {
        keyring::v1::Entry::new(VAULT_SERVICE, account).map_err(|error| {
            tracing::warn!(%error, "keyring entry creation failed");
            AuthError::InternalError("credential vault unavailable".to_owned())
        })
    }

    /// Best-effort read of the current manifest's chunk count. Returns 0 if
    /// there is no manifest, or if it can't be parsed as one (e.g. a legacy
    /// unchunked document), since there are no indexed chunk entries to
    /// account for in either case.
    fn existing_chunk_count(&self) -> usize {
        self.manifest_entry()
            .ok()
            .and_then(|entry| entry.get_password().ok())
            .and_then(|raw| serde_json::from_str::<VaultManifest>(&raw).ok())
            .map(|manifest| manifest.chunk_count)
            .unwrap_or(0)
    }
}

impl CredentialStore for KeyringCredentialStore {
    fn load<'life0, 'async_trait>(
        &'life0 self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<StoredCredentials>, AuthError>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let manifest_raw = match self.manifest_entry()?.get_password() {
                Ok(value) => value,
                Err(keyring::v1::Error::NoEntry) => return Ok(None),
                Err(error) => {
                    tracing::warn!(%error, "keyring manifest read failed");
                    return Err(AuthError::InternalError(
                        "credential vault read failed".to_owned(),
                    ));
                }
            };

            // Legacy fallback: a pre-chunking install may have stored the
            // full document directly under the manifest's key.
            if let Ok(document) = serde_json::from_str::<VaultDocument>(&manifest_raw)
                && document.version == VAULT_DOCUMENT_VERSION
            {
                return Ok(Some(document.credentials));
            }

            let manifest: VaultManifest = serde_json::from_str(&manifest_raw).map_err(|_| {
                AuthError::InternalError("credential vault data is invalid".to_owned())
            })?;
            if manifest.version != VAULT_DOCUMENT_VERSION {
                return Err(AuthError::InternalError(
                    "credential vault version is unsupported".to_owned(),
                ));
            }

            let mut raw = String::new();
            for index in 0..manifest.chunk_count {
                let chunk = self.chunk_entry(index)?.get_password().map_err(|error| {
                    tracing::warn!(%error, chunk = index, "keyring chunk read failed");
                    AuthError::InternalError("credential vault read failed".to_owned())
                })?;
                raw.push_str(&chunk);
            }

            let document: VaultDocument = serde_json::from_str(&raw).map_err(|_| {
                AuthError::InternalError("credential vault data is invalid".to_owned())
            })?;
            if document.version != VAULT_DOCUMENT_VERSION {
                return Err(AuthError::InternalError(
                    "credential vault version is unsupported".to_owned(),
                ));
            }
            Ok(Some(document.credentials))
        })
    }

    fn save<'life0, 'async_trait>(
        &'life0 self,
        credentials: StoredCredentials,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), AuthError>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let document = VaultDocument {
                version: VAULT_DOCUMENT_VERSION,
                credentials,
            };
            let raw = serde_json::to_string(&document).map_err(|_| {
                AuthError::InternalError("credential vault encoding failed".to_owned())
            })?;
            let chunks = chunk_str(&raw, VAULT_CHUNK_BYTES);
            let manifest = VaultManifest {
                version: VAULT_DOCUMENT_VERSION,
                chunk_count: chunks.len(),
            };
            let manifest_raw = serde_json::to_string(&manifest).map_err(|_| {
                AuthError::InternalError("credential vault encoding failed".to_owned())
            })?;
            let previous_chunk_count = self.existing_chunk_count();

            for (index, chunk) in chunks.iter().enumerate() {
                self.chunk_entry(index)?
                    .set_password(chunk)
                    .map_err(|error| {
                        tracing::warn!(%error, chunk = index, "keyring chunk write failed");
                        AuthError::InternalError("credential vault write failed".to_owned())
                    })?;
            }
            self.manifest_entry()?
                .set_password(&manifest_raw)
                .map_err(|error| {
                    tracing::warn!(%error, "keyring manifest write failed");
                    AuthError::InternalError("credential vault write failed".to_owned())
                })?;

            // Best-effort: drop any chunks left over from a previous, larger
            // save (e.g. a shorter refresh token). The manifest above is
            // already authoritative, so a failure here can't corrupt reads.
            for index in chunks.len()..previous_chunk_count {
                if let Ok(entry) = self.chunk_entry(index) {
                    let _ = entry.delete_credential();
                }
            }
            Ok(())
        })
    }

    fn clear<'life0, 'async_trait>(
        &'life0 self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), AuthError>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            for index in 0..self.existing_chunk_count() {
                if let Ok(entry) = self.chunk_entry(index) {
                    match entry.delete_credential() {
                        Ok(()) | Err(keyring::v1::Error::NoEntry) => {}
                        Err(error) => {
                            tracing::warn!(%error, chunk = index, "keyring chunk delete failed");
                            return Err(AuthError::InternalError(
                                "credential vault delete failed".to_owned(),
                            ));
                        }
                    }
                }
            }
            match self.manifest_entry()?.delete_credential() {
                Ok(()) | Err(keyring::v1::Error::NoEntry) => Ok(()),
                Err(error) => {
                    tracing::warn!(%error, "keyring manifest delete failed");
                    Err(AuthError::InternalError(
                        "credential vault delete failed".to_owned(),
                    ))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex as StdMutex;

    const ENDPOINT: &str = "https://mcp.example.com/mcp";
    const RESOURCE_METADATA: &str = "https://mcp.example.com/.well-known/oauth-protected-resource";
    const AUTHORIZATION_SERVER_METADATA: &str =
        "https://auth.example.com/.well-known/oauth-authorization-server";
    const CHALLENGE: &str = "Bearer resource_metadata=\"https://mcp.example.com/.well-known/oauth-protected-resource\", scope=\"challenge:read\"";

    #[derive(Clone, PartialEq, Eq)]
    struct RecordedRequest {
        method: String,
        uri: String,
        redirect_policy: OAuthHttpRedirectPolicy,
        body: Vec<u8>,
    }

    struct ScriptedResponse {
        status: u16,
        headers: BTreeMap<String, String>,
        body: Vec<u8>,
    }

    #[derive(Clone, Default)]
    struct ScriptedOAuthHttpClient {
        requests: Arc<StdMutex<Vec<RecordedRequest>>>,
        responses: Arc<StdMutex<VecDeque<ScriptedResponse>>>,
    }

    impl ScriptedOAuthHttpClient {
        fn new(responses: Vec<ScriptedResponse>) -> Self {
            Self {
                requests: Arc::new(StdMutex::new(Vec::new())),
                responses: Arc::new(StdMutex::new(responses.into())),
            }
        }

        fn requests(&self) -> Vec<RecordedRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl OAuthHttpClient for ScriptedOAuthHttpClient {
        fn execute(&self, operation: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
            self.requests.lock().unwrap().push(RecordedRequest {
                method: operation.request.method().to_string(),
                uri: operation.request.uri().to_string(),
                redirect_policy: operation.redirect_policy,
                body: operation.request.body().clone(),
            });
            let scripted = self.responses.lock().unwrap().pop_front();
            Box::pin(async move {
                let scripted = scripted.ok_or_else(|| {
                    Box::new(std::io::Error::other("missing scripted OAuth response"))
                        as rmcp::transport::auth::OAuthHttpClientError
                })?;
                let mut response: axum::http::Response<Vec<u8>> = Default::default();
                *response.status_mut() = scripted.status.try_into().unwrap();
                for (name, value) in scripted.headers {
                    response.headers_mut().insert(
                        name.parse::<axum::http::HeaderName>().unwrap(),
                        value.parse::<axum::http::HeaderValue>().unwrap(),
                    );
                }
                *response.body_mut() = scripted.body;
                Ok::<_, rmcp::transport::auth::OAuthHttpClientError>(response)
            })
        }
    }

    fn json_response(value: serde_json::Value) -> ScriptedResponse {
        ScriptedResponse {
            status: 200,
            headers: [("content-type".to_owned(), "application/json".to_owned())]
                .into_iter()
                .collect(),
            body: serde_json::to_vec(&value).unwrap(),
        }
    }

    fn discovery_responses() -> Vec<ScriptedResponse> {
        vec![
            json_response(serde_json::json!({
                "resource": "https://mcp.example.com",
                "authorization_servers": ["https://auth.example.com"],
                "scopes_supported": ["resource:read"]
            })),
            json_response(serde_json::json!({
                "issuer": "https://auth.example.com",
                "authorization_endpoint": "https://auth.example.com/authorize",
                "token_endpoint": "https://auth.example.com/token",
                "registration_endpoint": "https://auth.example.com/register",
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"],
                "scopes_supported": ["resource:read", "challenge:read", "offline_access"]
            })),
        ]
    }

    fn query_parameters(url: &str) -> BTreeMap<String, String> {
        reqwest::Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    fn form_parameters(request: &RecordedRequest) -> BTreeMap<String, String> {
        let raw = String::from_utf8(request.body.clone()).unwrap();
        reqwest::Url::parse(&format!("https://form.invalid/?{raw}"))
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    struct NullOAuthHttpClient;

    impl OAuthHttpClient for NullOAuthHttpClient {
        fn execute(&self, _operation: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
            Box::pin(async {
                Err(Box::new(std::io::Error::other("offline"))
                    as rmcp::transport::auth::OAuthHttpClientError)
            })
        }
    }

    #[test]
    fn canonical_account_rejects_secrets_and_fragments() {
        assert!(canonical_endpoint("https://user:pass@example.com/mcp").is_err());
        assert!(canonical_endpoint("https://example.com/mcp#token").is_err());
        assert!(canonical_endpoint("http://example.com/mcp").is_err());
        assert!(canonical_endpoint("http://localhost:3000/mcp").is_ok());
        assert_eq!(
            canonical_endpoint("HTTPS://EXAMPLE.COM:443/mcp").unwrap(),
            "https://example.com/mcp"
        );
    }

    #[test]
    fn redirect_uri_is_restricted_to_loopback() {
        assert!(validate_loopback_redirect("http://127.0.0.1:4567/callback").is_ok());
        assert!(validate_loopback_redirect("http://[::1]:4567/callback").is_ok());
        assert!(validate_loopback_redirect("https://example.com/callback").is_err());
    }

    #[tokio::test]
    async fn status_and_disconnect_use_the_injected_credential_store() {
        let store = Arc::new(InMemoryCredentialStore::new());
        let facade = McpOAuthFacade::with_boundaries(
            "https://mcp.example.com/rpc",
            Some("public-client".to_owned()),
            store.clone(),
            Arc::new(NullOAuthHttpClient),
        )
        .await
        .unwrap();
        assert_eq!(
            facade.connection_status().await.unwrap(),
            OAuthConnectionStatus::Disconnected
        );

        let credentials: StoredCredentials = serde_json::from_value(serde_json::json!({
            "client_id": "public-client",
            "token_response": {
                "access_token": "do-not-print",
                "token_type": "bearer"
            },
            "granted_scopes": ["tools:read"],
            "token_received_at": 1,
            "issuer": "https://auth.example.com"
        }))
        .unwrap();
        store.save(credentials).await.unwrap();
        assert_eq!(
            facade.connection_status().await.unwrap(),
            OAuthConnectionStatus::Connected
        );
        facade.disconnect().await.unwrap();
        assert_eq!(
            facade.connection_status().await.unwrap(),
            OAuthConnectionStatus::Disconnected
        );
    }

    #[test]
    fn oauth_errors_do_not_echo_provider_secrets() {
        let error = map_auth_error(AuthError::TokenExchangeFailed(
            "code=secret-code&access_token=secret-token".to_owned(),
        ));
        let rendered = error.to_string();
        assert!(!rendered.contains("secret-code"));
        assert!(!rendered.contains("secret-token"));
    }

    #[test]
    fn registration_failure_names_the_pre_registered_client_id_remediation() {
        let error = map_auth_error(AuthError::RegistrationFailed(
            "HTTP status client error (405 Method Not Allowed)".to_owned(),
        ));
        let rendered = error.to_string();
        assert!(rendered.contains("pre-registered client ID"));
        assert!(rendered.contains("405 Method Not Allowed"));
    }

    #[tokio::test]
    async fn discovery_pkce_state_resource_scopes_and_configured_client_are_preserved() {
        let mut responses = discovery_responses();
        responses.push(json_response(serde_json::json!({
            "access_token": "initial-access-token",
            "token_type": "bearer",
            "refresh_token": "initial-refresh-token",
            "expires_in": 3600,
            "scope": "challenge:read resource:read offline_access"
        })));
        responses.push(json_response(serde_json::json!({
            "access_token": "refreshed-access-token",
            "token_type": "bearer",
            "refresh_token": "rotated-refresh-token",
            "expires_in": 3600,
            "scope": "challenge:read resource:read offline_access"
        })));
        let http = Arc::new(ScriptedOAuthHttpClient::new(responses));
        let store = Arc::new(InMemoryCredentialStore::new());
        let facade = McpOAuthFacade::with_boundaries(
            ENDPOINT,
            Some("configured-public-client".to_owned()),
            store.clone(),
            http.clone(),
        )
        .await
        .unwrap();

        let start = facade
            .begin_authorization_with_challenge(
                "http://127.0.0.1:4567/oauth/callback",
                Some(CHALLENGE),
            )
            .await
            .unwrap();
        let authorization = query_parameters(&start.authorization_url);
        assert_eq!(
            authorization.get("client_id").map(String::as_str),
            Some("configured-public-client")
        );
        assert_eq!(
            authorization
                .get("code_challenge_method")
                .map(String::as_str),
            Some("S256")
        );
        assert!(
            authorization
                .get("code_challenge")
                .is_some_and(|v| !v.is_empty())
        );
        assert_eq!(authorization.get("state"), Some(&start.state));
        assert_eq!(
            authorization.get("resource").map(String::as_str),
            Some("https://mcp.example.com")
        );
        let requested_scopes = authorization.get("scope").unwrap();
        assert!(requested_scopes.contains("challenge:read"));
        assert!(requested_scopes.contains("resource:read"));
        assert!(requested_scopes.contains("offline_access"));
        assert_eq!(
            http.requests()
                .iter()
                .map(|request| request.uri.as_str())
                .collect::<Vec<_>>(),
            [RESOURCE_METADATA, AUTHORIZATION_SERVER_METADATA]
        );
        assert!(
            !http
                .requests()
                .iter()
                .any(|request| request.uri.ends_with("/register"))
        );

        facade
            .complete_authorization("one-time-authorization-code", &start.state)
            .await
            .unwrap();
        let token_request = http
            .requests()
            .into_iter()
            .find(|request| {
                request.uri == "https://auth.example.com/token"
                    && form_parameters(request)
                        .get("grant_type")
                        .map(String::as_str)
                        == Some("authorization_code")
            })
            .unwrap();
        let token_form = form_parameters(&token_request);
        assert_eq!(
            token_form.get("resource").map(String::as_str),
            Some("https://mcp.example.com")
        );
        assert_eq!(
            token_form.get("code").map(String::as_str),
            Some("one-time-authorization-code")
        );
        assert!(
            token_form
                .get("code_verifier")
                .is_some_and(|v| !v.is_empty())
        );
        assert!(token_form.get("code_verifier") != authorization.get("code_challenge"));

        assert_eq!(
            facade.refresh_after_unauthorized().await.unwrap(),
            "refreshed-access-token"
        );
        let refresh_request = http.requests().into_iter().last().unwrap();
        let refresh_form = form_parameters(&refresh_request);
        assert_eq!(
            refresh_form.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(
            refresh_form.get("refresh_token").map(String::as_str),
            Some("initial-refresh-token")
        );
        assert_eq!(
            refresh_form.get("resource").map(String::as_str),
            Some("https://mcp.example.com")
        );
        let stored = serde_json::to_value(store.load().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            stored["token_response"]["refresh_token"],
            "rotated-refresh-token"
        );
        assert_eq!(stored["issuer"], "https://auth.example.com");
    }

    #[tokio::test]
    async fn dynamic_registration_is_used_only_without_a_configured_client_id() {
        let mut responses = discovery_responses();
        responses.push(json_response(serde_json::json!({
            "client_id": "dynamically-registered-client",
            "redirect_uris": ["http://127.0.0.1:4567/oauth/callback"]
        })));
        let http = Arc::new(ScriptedOAuthHttpClient::new(responses));
        let facade = McpOAuthFacade::with_boundaries(
            ENDPOINT,
            None,
            Arc::new(InMemoryCredentialStore::new()),
            http.clone(),
        )
        .await
        .unwrap();

        let start = facade
            .begin_authorization_with_challenge(
                "http://127.0.0.1:4567/oauth/callback",
                Some(CHALLENGE),
            )
            .await
            .unwrap();
        assert_eq!(
            query_parameters(&start.authorization_url)
                .get("client_id")
                .map(String::as_str),
            Some("dynamically-registered-client")
        );
        let registration = http
            .requests()
            .into_iter()
            .find(|request| request.uri == "https://auth.example.com/register")
            .unwrap();
        assert_eq!(registration.method, "POST");
        let body: serde_json::Value = serde_json::from_slice(&registration.body).unwrap();
        assert_eq!(body["token_endpoint_auth_method"], "none");
        assert_eq!(body["response_types"], serde_json::json!(["code"]));
    }

    #[tokio::test]
    async fn rejected_refresh_clears_credentials_and_returns_reconnect_remediation() {
        let mut responses = discovery_responses();
        responses.push(json_response(serde_json::json!({
            "access_token": "initial-access-token",
            "token_type": "bearer",
            "refresh_token": "rejected-refresh-token",
            "expires_in": 3600
        })));
        responses.push(ScriptedResponse {
            status: 400,
            headers: [("content-type".to_owned(), "application/json".to_owned())]
                .into_iter()
                .collect(),
            body: serde_json::to_vec(&serde_json::json!({
                "error": "invalid_grant",
                "error_description": "refresh rejected"
            }))
            .unwrap(),
        });
        let http = Arc::new(ScriptedOAuthHttpClient::new(responses));
        let store = Arc::new(InMemoryCredentialStore::new());
        let facade = McpOAuthFacade::with_boundaries(
            ENDPOINT,
            Some("configured-public-client".to_owned()),
            store.clone(),
            http,
        )
        .await
        .unwrap();
        let start = facade
            .begin_authorization_with_challenge(
                "http://127.0.0.1:4567/oauth/callback",
                Some(CHALLENGE),
            )
            .await
            .unwrap();
        facade
            .complete_authorization("authorization-code", &start.state)
            .await
            .unwrap();

        let error = facade.refresh_after_unauthorized().await.unwrap_err();
        assert!(error.to_string().contains(RECONNECT_REMEDIATION));
        assert!(store.load().await.unwrap().is_none());
    }

    struct UnavailableCredentialStore;

    impl CredentialStore for UnavailableCredentialStore {
        fn load<'life0, 'async_trait>(
            &'life0 self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Option<StoredCredentials>, AuthError>>
                    + Send
                    + 'async_trait,
            >,
        >
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async {
                Err(AuthError::InternalError(
                    "credential vault leaked-secret-marker".to_owned(),
                ))
            })
        }

        fn save<'life0, 'async_trait>(
            &'life0 self,
            _credentials: StoredCredentials,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), AuthError>> + Send + 'async_trait>,
        >
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async { Err(AuthError::InternalError("credential vault failed".into())) })
        }

        fn clear<'life0, 'async_trait>(
            &'life0 self,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<(), AuthError>> + Send + 'async_trait>,
        >
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async { Err(AuthError::InternalError("credential vault failed".into())) })
        }
    }

    #[tokio::test]
    async fn vault_failures_are_actionable_and_redacted() {
        let error = McpOAuthFacade::with_boundaries(
            ENDPOINT,
            Some("public-client".to_owned()),
            Arc::new(UnavailableCredentialStore),
            Arc::new(NullOAuthHttpClient),
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(error, McpOAuthError::Vault(_)));
        assert!(!error.to_string().contains("leaked-secret-marker"));
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn keyring_store_round_trips_a_realistic_oauth_payload_over_the_windows_blob_limit() {
        let account = "https://mcp.oauth-repro-test.invalid/mcp".to_owned();
        let store = KeyringCredentialStore::new(account);

        let large_client_id = "a".repeat(4000);
        let credentials = StoredCredentials::new(
            large_client_id.clone(),
            None,
            vec!["tools:call".to_owned()],
            Some(1_700_000_000),
        );

        store.clear().await.unwrap();
        store.save(credentials).await.unwrap();
        let loaded = store
            .load()
            .await
            .unwrap()
            .expect("credentials should round trip");
        assert_eq!(loaded.client_id, large_client_id);
        assert_eq!(loaded.granted_scopes, vec!["tools:call".to_owned()]);

        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
    }
}
