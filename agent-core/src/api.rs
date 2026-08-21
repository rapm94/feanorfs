use anyhow::{bail, ensure, Context, Result};
use axum::body::HttpBody as _;
use feanorfs_common::{RelayConfig, SyncRequest, SyncResponse};
use futures_util::StreamExt as _;
use reqwest::{Certificate, Client};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::hub::LocalHub;
use crate::local::{load_config, Config};

enum Backend {
    Http {
        client: Client,
        server_url: String,
        _tunnel: Option<crate::tunnel::ClientTunnel>,
    },
    Local(Arc<LocalHub>),
}

pub struct ApiClient {
    backend: Backend,
    server_password: Option<String>,
    migration_token: Option<String>,
}

#[derive(Deserialize, Serialize, Debug)]
struct WorkspaceFormatResponse {
    format_version: u32,
}

#[derive(Deserialize)]
struct VersionResponse {
    version: String,
}

/// Oldest hub version whose advertised protocol this client accepts.
pub const MIN_SUPPORTED_SERVER_VERSION: &str = "0.7.0";

const MAX_VERSION_RESPONSE_BYTES: usize = 1024;
const MAX_API_RESPONSE_BYTES: usize = 100 * 1024 * 1024;
const MAX_API_ERROR_BYTES: usize = 64 * 1024;
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// Upper bound for establishing one hub connection (DNS + TCP + TLS).
/// An unreachable or silently dropped hub must fail a sync instead of
/// blocking on the OS connect timeout.
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Idle bound for reading one hub response. The timer resets on every byte
/// the hub sends, so slow-but-live transfers are never aborted; a peer that
/// stops answering (blackholed network, paused hub process) fails within the
/// bound instead of wedging sync, the watcher, and CLI commands forever.
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct RequestStatusError {
    method: &'static str,
    path: String,
    status: http::StatusCode,
    body: String,
}

impl std::fmt::Display for RequestStatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} {} failed with status {}: {}",
            self.method, self.path, self.status, self.body
        )
    }
}

impl std::error::Error for RequestStatusError {}

fn bounded_error_body(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut output = String::new();
    for character in text.chars().take(4096) {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    output
}

pub(crate) fn request_error_status(error: &anyhow::Error) -> Option<http::StatusCode> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<RequestStatusError>()
            .map(|error| error.status)
    })
}

/// Endpoint-aware classification of one failed API response.
///
/// Bounded by construction: it carries only the request endpoint and status,
/// never the response body, so server text cannot leak into control flow or
/// spoof a classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiFailureKind {
    /// The reachability-manifest endpoint (`POST /api/manifest`) rejected the
    /// manifest with 412 because a referenced blob is missing (fresh/restored
    /// hub data or a GC race). Recovery re-uploads all objects.
    ManifestReferencesMissingBlob,
    /// Any other failed API response with a typed status error.
    Other,
}

/// Classifies a failed API response by endpoint and status alone.
///
/// Returns `None` when the error chain contains no typed status error (for
/// example a transport failure). The body is never consulted, so unrelated or
/// malicious response text cannot change the classification.
pub fn api_failure_kind(error: &anyhow::Error) -> Option<ApiFailureKind> {
    error.chain().find_map(|cause| {
        let status_error = cause.downcast_ref::<RequestStatusError>()?;
        let references_missing_blob = status_error.path == "/api/manifest"
            && status_error.status == http::StatusCode::PRECONDITION_FAILED;
        Some(if references_missing_blob {
            ApiFailureKind::ManifestReferencesMissingBlob
        } else {
            ApiFailureKind::Other
        })
    })
}

/// Returns whether a failed HTTP operation may be retried as unavailable
/// transport. Authentication, malformed responses, local I/O, and ordinary
/// client errors remain fail-closed.
pub fn is_retryable_transport_error(error: &anyhow::Error) -> bool {
    request_error_status(error).is_some_and(retryable_server_status)
        || error.chain().any(|cause| {
            cause.downcast_ref::<reqwest::Error>().is_some_and(|error| {
                error.is_request() || error.is_connect() || error.is_timeout() || error.is_body()
            })
        })
}

const fn retryable_server_status(status: http::StatusCode) -> bool {
    matches!(
        status,
        http::StatusCode::INTERNAL_SERVER_ERROR
            | http::StatusCode::BAD_GATEWAY
            | http::StatusCode::SERVICE_UNAVAILABLE
            | http::StatusCode::GATEWAY_TIMEOUT
    )
}

pub(crate) fn request_status_error(
    method: &'static str,
    path: &str,
    status: http::StatusCode,
    body: &[u8],
) -> anyhow::Error {
    RequestStatusError {
        method,
        path: path.to_string(),
        status,
        body: bounded_error_body(body),
    }
    .into()
}

/// Expected-status policy for one bounded response read. The policy is the
/// single place that decides which byte limit applies (the exact success
/// limit or the error-body bound) and whether a non-success status is a
/// typed [`ResponseReadError::Status`] instead of a returned value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedStatus {
    /// Accept any status; the caller inspects the returned status itself.
    Any,
    /// Reject non-success statuses with a typed [`ResponseReadError::Status`].
    Success,
}

/// Typed failure of one bounded hub response read.
///
/// Every variant carries only bounded data (a limit, a status, a bounded
/// error body, or a fixed-size hash), so server-controlled text cannot drive
/// unbounded allocation or control flow. Callers convert these into their
/// existing error chains; the variants exist so response-reading failure
/// modes are inspectable instead of being inferred from rendered text.
#[derive(Debug)]
pub enum ResponseReadError {
    /// The response body exceeded the exact byte limit.
    OverLimit { limit: usize },
    /// The response stalled or failed to complete within the transport
    /// bounds (HTTP read/connect timeout, or a truncated body).
    Timeout { source: reqwest::Error },
    /// A transport-level failure while reading the response body (for
    /// example a truncated connection). Carries the original error so
    /// retryability classification keeps working.
    Transport { source: reqwest::Error },
    /// The in-process hub failed to read its (already materialized) response
    /// body, for example when it exceeds the byte limit.
    LocalBody { message: String },
    /// A non-success status was rejected by the expected-status policy.
    Status {
        status: http::StatusCode,
        body: Vec<u8>,
    },
    /// A JSON response did not decode into the expected type.
    Decode { path: String, message: String },
    /// A downloaded blob failed its ciphertext hash verification.
    HashMismatch { expected: String },
}

impl std::fmt::Display for ResponseReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseReadError::OverLimit { limit } => {
                write!(formatter, "HTTP response exceeds {limit} byte limit")
            }
            ResponseReadError::Timeout { source } => {
                write!(
                    formatter,
                    "timed out or stalled while reading hub response: {source}"
                )
            }
            ResponseReadError::Transport { source } => {
                write!(formatter, "read HTTP response body: {source}")
            }
            ResponseReadError::LocalBody { message } => {
                write!(formatter, "read bounded response body: {message}")
            }
            ResponseReadError::Status { status, .. } => {
                write!(formatter, "HTTP response has unexpected status {status}")
            }
            ResponseReadError::Decode { path, message } => {
                write!(formatter, "Failed to parse {path} response: {message}")
            }
            ResponseReadError::HashMismatch { expected } => {
                write!(formatter, "downloaded object hash mismatch for {expected}")
            }
        }
    }
}

impl std::error::Error for ResponseReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ResponseReadError::Timeout { source } | ResponseReadError::Transport { source } => {
                Some(source)
            }
            _ => None,
        }
    }
}

/// Returns a typed hash-mismatch error for a downloaded blob whose bytes do
/// not verify against the expected ciphertext hash.
///
/// The message keeps the endpoint download context (`downloaded object hash
/// mismatch for {expected}`) so callers that historically matched on rendered
/// text keep working, while the typed variant stays inspectable.
pub fn hash_mismatch_error(expected: &str) -> anyhow::Error {
    ResponseReadError::HashMismatch {
        expected: expected.to_string(),
    }
    .into()
}

impl ApiClient {
    pub fn new(server_url: &str, server_password: Option<&str>) -> Self {
        let client = Self::build_http_client(None, None)
            .expect("build FeanorFS HTTP client with default transport timeouts");
        Self {
            backend: Backend::Http {
                client,
                server_url: server_url.trim_end_matches('/').to_string(),
                _tunnel: None,
            },
            server_password: server_password.map(str::to_string),
            migration_token: None,
        }
    }

    pub fn new_with_tls(
        server_url: &str,
        server_password: Option<&str>,
        tls_ca_pem: Option<&str>,
    ) -> Result<Self> {
        Self::new_with_tls_resolution(server_url, server_password, tls_ca_pem, None)
    }

    /// Builds a normally verified TLS client while overriding address lookup
    /// for the URL hostname. The URL hostname remains the TLS SNI/name check;
    /// this is safe for mDNS-discovered addresses only when the CA is pinned.
    pub fn new_with_tls_resolved(
        server_url: &str,
        server_password: Option<&str>,
        tls_ca_pem: Option<&str>,
        hostname: &str,
        addresses: &[SocketAddr],
    ) -> Result<Self> {
        Self::new_with_tls_resolution(
            server_url,
            server_password,
            tls_ca_pem,
            Some((hostname, addresses)),
        )
    }

    fn new_with_tls_resolution(
        server_url: &str,
        server_password: Option<&str>,
        tls_ca_pem: Option<&str>,
        resolution: Option<(&str, &[SocketAddr])>,
    ) -> Result<Self> {
        let client = Self::build_http_client(tls_ca_pem, resolution)
            .context("build FeanorFS HTTP client")?;
        Ok(Self {
            backend: Backend::Http {
                client,
                server_url: server_url.trim_end_matches('/').to_string(),
                _tunnel: None,
            },
            server_password: server_password.map(str::to_string),
            migration_token: None,
        })
    }

    /// Builds the shared hub HTTP client with bounded connect/read timeouts
    /// so a silent or unreachable hub fails requests instead of hanging every
    /// sync, watcher, and CLI command indefinitely.
    fn build_http_client(
        tls_ca_pem: Option<&str>,
        resolution: Option<(&str, &[SocketAddr])>,
    ) -> Result<Client> {
        let mut builder = Client::builder();
        builder = builder
            .connect_timeout(HTTP_CONNECT_TIMEOUT)
            .read_timeout(HTTP_READ_TIMEOUT);
        if let Some(pem) = tls_ca_pem {
            let certificate = Certificate::from_pem(pem.as_bytes())
                .context("parse FeanorFS hub CA certificate")?;
            builder = builder.add_root_certificate(certificate);
        }
        if let Some((hostname, addresses)) = resolution {
            builder = builder.resolve_to_addrs(hostname, addresses);
        }
        builder.build().context("build FeanorFS HTTP client")
    }

    /// Test-only constructor with explicit transport timeouts.
    #[cfg(test)]
    pub(crate) fn new_with_timeouts(
        server_url: &str,
        server_password: Option<&str>,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self> {
        let mut builder = Client::builder();
        builder = builder
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout);
        let client = builder.build().context("build test FeanorFS HTTP client")?;
        Ok(Self {
            backend: Backend::Http {
                client,
                server_url: server_url.trim_end_matches('/').to_string(),
                _tunnel: None,
            },
            server_password: server_password.map(str::to_string),
            migration_token: None,
        })
    }

    pub fn local(hub: Arc<LocalHub>, server_password: Option<String>) -> Self {
        Self {
            backend: Backend::Local(hub),
            server_password,
            migration_token: None,
        }
    }

    pub async fn open_for_workspace(workspace: &Path) -> Result<Self> {
        let config = load_config(workspace)?;
        Self::from_config(workspace, &config).await
    }

    pub async fn from_config(workspace: &Path, config: &Config) -> Result<Self> {
        if config.is_local_hub() {
            let hub_dir = config.hub_data_dir(workspace)?;
            let hub = LocalHub::open(hub_dir, config.server_password.clone()).await?;
            Ok(Self::local(hub, config.server_password.clone()))
        } else if let Some(relay) = config
            .relay
            .as_ref()
            .filter(|_| !url_is_loopback(&config.server_url))
        {
            Self::new_with_relay(
                &config.server_url,
                config.server_password.as_deref(),
                config.tls_ca_pem.as_deref(),
                relay,
            )
            .await
        } else {
            Self::from_config_direct(workspace, config).await
        }
    }

    pub async fn from_config_direct(workspace: &Path, config: &Config) -> Result<Self> {
        if config.is_local_hub() {
            let hub_dir = config.hub_data_dir(workspace)?;
            let hub = LocalHub::open(hub_dir, config.server_password.clone()).await?;
            Ok(Self::local(hub, config.server_password.clone()))
        } else {
            Self::new_with_tls(
                &config.server_url,
                config.server_password.as_deref(),
                config.tls_ca_pem.as_deref(),
            )
        }
    }

    async fn new_with_relay(
        server_url: &str,
        server_password: Option<&str>,
        tls_ca_pem: Option<&str>,
        relay: &RelayConfig,
    ) -> Result<Self> {
        let tunnel = crate::tunnel::ClientTunnel::start(relay, server_url).await?;
        let address = tunnel.address();
        let mut api = Self::new_with_tls_resolved(
            tunnel.server_url(),
            server_password,
            tls_ca_pem,
            tunnel.hostname(),
            &[address],
        )?;
        if let Backend::Http { _tunnel, .. } = &mut api.backend {
            *_tunnel = Some(tunnel);
        }
        Ok(api)
    }

    pub fn is_local(&self) -> bool {
        matches!(self.backend, Backend::Local(_))
    }

    #[must_use]
    pub fn with_migration_token(mut self, token: String) -> Self {
        self.migration_token = Some(token);
        self
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str, query: &str) -> Result<T> {
        self.get_json_bounded(path, query, MAX_API_RESPONSE_BYTES)
            .await
    }

    /// JSON response read through the shared bounded reader with an exact
    /// byte limit. `get_json` uses the default API bound; tests use smaller
    /// limits to exercise the reader's boundary behavior.
    pub(crate) async fn get_json_bounded<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &str,
        limit: usize,
    ) -> Result<T> {
        let (status, body) = self
            .raw_request(http::Method::GET, path, query, Vec::new(), None)
            .await?;
        if status == http::StatusCode::UNAUTHORIZED {
            bail!(
                "Server requires a valid access token. Paste its fnh1/fnr1 invite into `feanorfs start`, or set one with `feanorfs connect <URL> --token <TOKEN>`"
            );
        }
        if !status.is_success() {
            return Err(request_status_error("GET", path, status, &body));
        }
        if body.len() > limit {
            return Err(ResponseReadError::OverLimit { limit }.into());
        }
        match serde_json::from_slice(&body) {
            Ok(value) => Ok(value),
            Err(error) => Err(anyhow::Error::from(ResponseReadError::Decode {
                path: path.to_string(),
                message: error.to_string(),
            }))
            .with_context(|| format!("Failed to parse GET {path} response")),
        }
    }

    async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl serde::Serialize,
    ) -> Result<T> {
        let payload = serde_json::to_vec(body).context("serialize request body")?;
        let (status, bytes) = self
            .raw_request(
                http::Method::POST,
                path,
                "",
                payload,
                Some("application/json"),
            )
            .await?;
        if status == http::StatusCode::UNAUTHORIZED {
            bail!(
                "Server requires a valid access token. Paste its fnh1/fnr1 invite into `feanorfs start`, or set one with `feanorfs connect <URL> --token <TOKEN>`"
            );
        }
        if !status.is_success() {
            return Err(request_status_error("POST", path, status, &bytes));
        }
        serde_json::from_slice(&bytes)
            .with_context(|| format!("Failed to parse POST {path} response"))
    }

    async fn post_bytes(&self, path: &str, query: &str, body: Vec<u8>) -> Result<()> {
        let (status, bytes) = self
            .raw_request(http::Method::POST, path, query, body, None)
            .await?;
        if status == http::StatusCode::UNAUTHORIZED {
            bail!(
                "Server requires a valid access token. Paste its fnh1/fnr1 invite into `feanorfs start`, or set one with `feanorfs connect <URL> --token <TOKEN>`"
            );
        }
        if !status.is_success() {
            return Err(request_status_error("POST", path, status, &bytes));
        }
        Ok(())
    }

    pub(crate) async fn raw_request(
        &self,
        method: http::Method,
        path: &str,
        query: &str,
        body: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<(http::StatusCode, Vec<u8>)> {
        match &self.backend {
            Backend::Http {
                client, server_url, ..
            } => {
                let url = if query.is_empty() {
                    format!("{server_url}{path}")
                } else {
                    format!("{server_url}{path}?{query}")
                };
                let mut req = client.request(method, &url);
                req = req.header("X-FeanorFS-Format", "3");
                if let Some(pass) = &self.server_password {
                    req = req.bearer_auth(pass);
                }
                if let Some(token) = &self.migration_token {
                    req = req.header("X-FeanorFS-Migration", token);
                }
                if let Some(ct) = content_type {
                    req = req.header("Content-Type", ct);
                }
                if !body.is_empty() {
                    req = req.body(body);
                }
                let resp = req
                    .send()
                    .await
                    .with_context(|| format!("Failed to send request to {url}"))?;
                self.read_response_http(resp, ExpectedStatus::Any, MAX_API_RESPONSE_BYTES)
                    .await
                    .map_err(Into::into)
            }
            Backend::Local(hub) => {
                let resp = hub
                    .request(
                        method,
                        path,
                        query,
                        body,
                        (
                            self.server_password.as_deref(),
                            self.migration_token.as_deref(),
                        ),
                        content_type,
                    )
                    .await?;
                self.read_response_local(resp, ExpectedStatus::Any, MAX_API_RESPONSE_BYTES)
                    .await
                    .map_err(Into::into)
            }
        }
    }

    /// One bounded hub response reader shared by every API call path.
    ///
    /// Reads the full body as a stream so large responses never buffer
    /// beyond their exact byte limit, and applies the expected-status policy:
    /// non-success statuses are read with the smaller error-body bound and,
    /// when the policy rejects them, returned as a typed
    /// [`ResponseReadError::Status`] instead of a value. The caller keeps
    /// interpreting the returned status (authorization handling, status
    /// errors, conflict outcomes) exactly as before.
    async fn read_response_http(
        &self,
        response: reqwest::Response,
        expected: ExpectedStatus,
        success_limit: usize,
    ) -> Result<(http::StatusCode, Vec<u8>), ResponseReadError> {
        let status = response.status();
        let limit = if status.is_success() {
            success_limit
        } else {
            MAX_API_ERROR_BYTES
        };
        let body = read_http_body_bounded(response, limit).await?;
        apply_expected_status(expected, status, body)
    }

    /// LocalHub twin of [`Self::read_response_http`]. The in-process backend
    /// materializes its response body in memory, so the bounded read is the
    /// hub's own bounded body reader. Over-limit responses are classified
    /// into the same typed [`ResponseReadError::OverLimit`] as the HTTP
    /// path: the hub's exact-size body advertises its length up front, and
    /// the bounded reader's length-limit failure is recognized as well.
    async fn read_response_local(
        &self,
        response: axum::response::Response<axum::body::Body>,
        expected: ExpectedStatus,
        success_limit: usize,
    ) -> Result<(http::StatusCode, Vec<u8>), ResponseReadError> {
        let status = response.status();
        let limit = if status.is_success() {
            success_limit
        } else {
            MAX_API_ERROR_BYTES
        };
        if response
            .body()
            .size_hint()
            .exact()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(ResponseReadError::OverLimit { limit });
        }
        let (_, body) = LocalHub::read_body_bounded(response, limit)
            .await
            .map_err(|error| {
                let message = error.to_string();
                if message.contains("length limit exceeded") {
                    ResponseReadError::OverLimit { limit }
                } else {
                    ResponseReadError::LocalBody { message }
                }
            })?;
        apply_expected_status(expected, status, body)
    }

    async fn post_sync_endpoint(
        &self,
        endpoint: &str,
        request: &SyncRequest,
    ) -> Result<SyncResponse> {
        self.post_json(&format!("/api/sync/{endpoint}"), request)
            .await
    }

    pub async fn peek_sync(&self, request: &SyncRequest) -> Result<SyncResponse> {
        self.post_sync_endpoint("peek", request).await
    }

    pub async fn upload_file(
        &self,
        workspace_id: &str,
        file: &feanorfs_common::FileState,
        content: Vec<u8>,
    ) -> Result<()> {
        let query = format!(
            "workspace_id={}&path={}&hash={}&size={}&mtime={}&mode={}&deleted=false",
            urlencoding_path(workspace_id),
            urlencoding_path(&file.path),
            urlencoding_path(&file.hash),
            file.size,
            file.mtime,
            file.mode
        );
        self.post_bytes("/api/upload", &query, content).await
    }

    pub async fn upload_tombstone(
        &self,
        workspace_id: &str,
        path: &str,
        hash: &str,
        mtime: i64,
    ) -> Result<()> {
        let query = format!(
            "workspace_id={}&path={}&hash={}&size=0&mtime={}&deleted=true",
            urlencoding_path(workspace_id),
            urlencoding_path(path),
            urlencoding_path(hash),
            mtime
        );
        self.post_bytes("/api/upload", &query, Vec::new()).await
    }

    pub async fn upload_object(
        &self,
        workspace_id: &str,
        hash: &str,
        content: Vec<u8>,
    ) -> Result<()> {
        let query = format!(
            "workspace_id={}&path=object&hash={}&size={}&mtime=0&deleted=false&object=true",
            urlencoding_path(workspace_id),
            urlencoding_path(hash),
            content.len()
        );
        self.post_bytes("/api/upload", &query, content).await
    }

    pub async fn upload_manifest(
        &self,
        workspace_id: &str,
        snapshot_id: &str,
        hashes: &[String],
    ) -> Result<()> {
        let query = format!(
            "workspace_id={}&snapshot_id={}",
            urlencoding_path(workspace_id),
            urlencoding_path(snapshot_id)
        );
        let canonical = feanorfs_common::canonical_manifest_hash_list(snapshot_id, hashes)?;
        let mut manifest = canonical.join("\n").into_bytes();
        manifest.push(b'\n');
        self.post_bytes("/api/manifest", &query, manifest).await
    }

    pub async fn set_workspace_format(&self, workspace_id: &str, version: u32) -> Result<()> {
        let query = format!(
            "workspace_id={}&format_version={version}",
            urlencoding_path(workspace_id)
        );
        self.post_bytes("/api/workspace/format", &query, Vec::new())
            .await
    }

    pub async fn workspace_format(&self, workspace_id: &str) -> Result<u32> {
        let query = format!("workspace_id={}", urlencoding_path(workspace_id));
        let response: WorkspaceFormatResponse =
            self.get_json("/api/workspace/format", &query).await?;
        Ok(response.format_version)
    }

    /// Returns the hub's advertised version, or `None` when the hub predates
    /// the version endpoint. Embedded local hubs always report this build.
    pub async fn server_version(&self) -> Result<Option<String>> {
        match &self.backend {
            Backend::Local(_) => Ok(Some(env!("CARGO_PKG_VERSION").to_string())),
            Backend::Http {
                client, server_url, ..
            } => {
                let mut request = client
                    .get(format!("{server_url}/api/version"))
                    .timeout(VERSION_PROBE_TIMEOUT)
                    .header("X-FeanorFS-Format", "3");
                if let Some(password) = &self.server_password {
                    request = request.bearer_auth(password);
                }
                let mut response = request.send().await.context("probe hub version")?;
                if response.status() == http::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                let status = response.status();
                if status == http::StatusCode::UNAUTHORIZED {
                    bail!(
                        "Server requires a valid access token. Paste its fnh1/fnr1 invite into `feanorfs start`, or set one with `feanorfs connect <URL> --token <TOKEN>`"
                    );
                }
                if !status.is_success() {
                    return Err(request_status_error("GET", "/api/version", status, &[]));
                }
                if let Some(length) = response.content_length() {
                    ensure!(
                        length <= MAX_VERSION_RESPONSE_BYTES as u64,
                        "hub version response is unexpectedly large"
                    );
                }
                let mut body = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .context("read hub version response")?
                {
                    ensure!(
                        chunk.len() <= MAX_VERSION_RESPONSE_BYTES.saturating_sub(body.len()),
                        "hub version response is unexpectedly large"
                    );
                    body.extend_from_slice(&chunk);
                }
                let response: VersionResponse =
                    serde_json::from_slice(&body).context("parse hub version response")?;
                Ok(Some(response.version))
            }
        }
    }

    /// Fails closed when an authenticated hub advertises an unsupported
    /// protocol version. A 404 remains compatible with the previous release,
    /// which predates the version endpoint.
    pub async fn ensure_server_compatible(&self) -> Result<()> {
        let Some(advertised) = self.server_version().await? else {
            return Ok(());
        };
        check_server_version(&advertised)
    }

    pub async fn begin_migration(&self, workspace_id: &str) -> Result<()> {
        let query = format!("workspace_id={}", urlencoding_path(workspace_id));
        self.post_bytes("/api/workspace/migration", &query, Vec::new())
            .await
    }

    pub async fn download_file(&self, hash: &str) -> Result<Vec<u8>> {
        self.download_file_bounded(hash, 100 * 1024 * 1024).await
    }

    pub async fn download_file_bounded(&self, hash: &str, max_bytes: usize) -> Result<Vec<u8>> {
        let path = format!("/api/download/{hash}");
        let (status, body) = match &self.backend {
            Backend::Http {
                client, server_url, ..
            } => {
                let url = format!("{server_url}{path}");
                let mut request = client.get(&url).header("X-FeanorFS-Format", "3");
                if let Some(password) = &self.server_password {
                    request = request.bearer_auth(password);
                }
                if let Some(token) = &self.migration_token {
                    request = request.header("X-FeanorFS-Migration", token);
                }
                let response = request
                    .send()
                    .await
                    .with_context(|| format!("Failed to send request to {url}"))?;
                self.read_response_http(response, ExpectedStatus::Success, max_bytes)
                    .await
                    .map_err(download_status_error("GET", &path))?
            }
            Backend::Local(hub) => {
                let response = hub
                    .request(
                        http::Method::GET,
                        &path,
                        "",
                        Vec::new(),
                        (
                            self.server_password.as_deref(),
                            self.migration_token.as_deref(),
                        ),
                        None,
                    )
                    .await?;
                self.read_response_local(response, ExpectedStatus::Success, max_bytes)
                    .await
                    .map_err(download_status_error("GET", &path))?
            }
        };
        if status == http::StatusCode::UNAUTHORIZED {
            bail!(
                "Server requires a valid access token. Paste its fnh1/fnr1 invite into `feanorfs start`, or set one with `feanorfs connect <URL> --token <TOKEN>`"
            );
        }
        Ok(body)
    }

    pub async fn get_workspaces(&self) -> Result<Vec<String>> {
        self.get_json("/api/workspaces", "").await
    }
}

/// Applies the expected-status policy to a fully read response: a rejected
/// non-success status becomes the typed [`ResponseReadError::Status`] (with
/// its already-bounded error body) instead of a returned value.
fn apply_expected_status(
    expected: ExpectedStatus,
    status: http::StatusCode,
    body: Vec<u8>,
) -> Result<(http::StatusCode, Vec<u8>), ResponseReadError> {
    if expected == ExpectedStatus::Success && !status.is_success() {
        return Err(ResponseReadError::Status { status, body });
    }
    Ok((status, body))
}

/// Reads one HTTP response body as a stream, bounded by an exact byte limit.
///
/// The limit is enforced both against the declared `Content-Length` and
/// against the bytes actually received, so a hub that lies about its length
/// or streams without bound still fails within the cap. Streamed reads keep
/// large downloads from being buffered beyond the limit.
async fn read_http_body_bounded(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ResponseReadError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(ResponseReadError::OverLimit { limit });
    }
    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(limit),
    );
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) if error.is_timeout() => {
                return Err(ResponseReadError::Timeout { source: error });
            }
            Err(error) => return Err(ResponseReadError::Transport { source: error }),
        };
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(ResponseReadError::OverLimit { limit });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Maps a typed [`ResponseReadError`] from the shared reader back into the
/// caller-visible error chain for a download. Status rejections become the
/// existing typed [`RequestStatusError`] so status classification and
/// retryability keep working; all other variants pass through unchanged.
fn download_status_error<'a>(
    method: &'static str,
    path: &'a str,
) -> impl Fn(ResponseReadError) -> anyhow::Error + 'a {
    move |error| match error {
        ResponseReadError::Status { status, body } => {
            request_status_error(method, path, status, &body)
        }
        other => anyhow::Error::from(other),
    }
}

fn urlencoding_path(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

fn url_is_loopback(value: &str) -> bool {
    reqwest::Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
}

fn check_server_version(advertised: &str) -> Result<()> {
    let server = semver::Version::parse(advertised)
        .map_err(|_| anyhow::anyhow!("hub advertised an unparsable version"))?;
    let floor = semver::Version::parse(MIN_SUPPORTED_SERVER_VERSION)?;
    if server < floor {
        bail!(
            "hub version {advertised} is below the minimum supported protocol \
             version {MIN_SUPPORTED_SERVER_VERSION}. Upgrade the hub to the current release \
             (run `feanorfs serve` or `feanorfs start --host` with the new binary) before syncing."
        );
    }
    Ok(())
}

#[cfg(test)]
mod version_tests {
    use super::{
        api_failure_kind, check_server_version, is_retryable_transport_error, request_status_error,
        ApiClient, ApiFailureKind,
    };
    use std::time::Duration;

    async fn serve(router: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{address}"), task)
    }

    #[test]
    fn server_version_check_fails_closed_below_minimum() {
        let error = check_server_version("0.6.9").unwrap_err();
        assert!(error.to_string().contains("minimum supported"));
        assert!(error.to_string().contains("0.7.0"));
    }

    #[test]
    fn server_version_check_accepts_minimum_and_newer() {
        check_server_version("0.7.0").unwrap();
        check_server_version("0.7.11").unwrap();
        check_server_version("0.8.0").unwrap();
        check_server_version("1.0.0").unwrap();
    }

    #[test]
    fn server_version_check_rejects_unparsable_advertisements() {
        let error = check_server_version("not-a-version").unwrap_err();
        assert!(error.to_string().contains("unparsable"));
    }

    #[tokio::test]
    async fn incompatible_advertised_hub_fails_over_real_http() {
        let router = axum::Router::new().route(
            "/api/version",
            axum::routing::get(|| async { axum::Json(serde_json::json!({ "version": "0.6.9" })) }),
        );
        let (url, task) = serve(router).await;
        let error = ApiClient::new(&url, Some("test-token"))
            .ensure_server_compatible()
            .await
            .unwrap_err();
        task.abort();
        assert!(error.to_string().contains("hub version 0.6.9"));
        assert!(error.to_string().contains("Upgrade the hub"));
    }

    #[tokio::test]
    async fn silent_hub_fails_bounded_instead_of_hanging_forever() {
        // A hub that accepts the connection but never sends a response must
        // fail the request within the read timeout instead of wedging sync,
        // the watcher, and every CLI command on a blackholed connection.
        let router = axum::Router::new().route(
            "/api/head",
            axum::routing::get(|| async {
                std::future::pending::<axum::response::Response>().await
            }),
        );
        let (url, task) = serve(router).await;
        let client = ApiClient::new_with_timeouts(
            &url,
            Some("test-token"),
            Duration::from_secs(2),
            Duration::from_millis(400),
        )
        .unwrap();
        let start = std::time::Instant::now();
        let error = client.get_head("ws-1").await.unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "request hung on a silent hub"
        );
        assert!(is_retryable_transport_error(&error), "{error:#}");
        task.abort();
    }

    #[tokio::test]
    async fn object_download_is_bounded_before_body_growth() {
        let router = axum::Router::new().route(
            "/api/download/{hash}",
            axum::routing::get(|| async { vec![0_u8; 5] }),
        );
        let (url, task) = serve(router).await;
        let error = ApiClient::new(&url, Some("test-token"))
            .download_file_bounded(&"a".repeat(64), 4)
            .await
            .unwrap_err();
        task.abort();
        assert!(error.to_string().contains("exceeds 4 byte limit"));
    }

    #[tokio::test]
    async fn endpointless_hub_404_remains_explicitly_compatible() {
        let (url, task) = serve(axum::Router::new()).await;
        ApiClient::new(&url, Some("test-token"))
            .ensure_server_compatible()
            .await
            .unwrap();
        task.abort();
    }

    #[tokio::test]
    async fn typed_head_status_errors_classify_only_retryable_server_failures() {
        for status in [
            http::StatusCode::INTERNAL_SERVER_ERROR,
            http::StatusCode::BAD_GATEWAY,
            http::StatusCode::SERVICE_UNAVAILABLE,
            http::StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert!(is_retryable_transport_error(&request_status_error(
                "GET",
                "/api/head",
                status,
                &[]
            )));
        }
        assert!(!is_retryable_transport_error(&request_status_error(
            "GET",
            "/api/head",
            http::StatusCode::NOT_IMPLEMENTED,
            &[]
        )));

        let retryable = axum::Router::new().route(
            "/api/head",
            axum::routing::get(|| async {
                (http::StatusCode::SERVICE_UNAVAILABLE, "temporary outage")
            }),
        );
        let (url, task) = serve(retryable).await;
        let error = ApiClient::new(&url, None)
            .get_head("runner-test")
            .await
            .unwrap_err();
        task.abort();
        assert!(error
            .to_string()
            .contains("GET /api/head failed with status 503"));
        assert!(is_retryable_transport_error(&error));

        let unauthorized = axum::Router::new().route(
            "/api/head",
            axum::routing::get(|| async { http::StatusCode::UNAUTHORIZED }),
        );
        let (url, task) = serve(unauthorized).await;
        let error = ApiClient::new(&url, None)
            .get_head("runner-test")
            .await
            .unwrap_err();
        task.abort();
        assert!(error.to_string().contains("valid access token"));
        assert!(!is_retryable_transport_error(&error));
    }

    #[tokio::test]
    async fn typed_api_statuses_and_local_io_are_not_broadly_retryable() {
        let router = axum::Router::new().route(
            "/api/workspaces",
            axum::routing::get(|| async { (http::StatusCode::BAD_REQUEST, "bad request") }),
        );
        let (url, task) = serve(router).await;
        let error = ApiClient::new(&url, None)
            .get_workspaces()
            .await
            .unwrap_err();
        task.abort();
        assert!(error
            .to_string()
            .contains("GET /api/workspaces failed with status 400"));
        assert!(!is_retryable_transport_error(&error));
        assert!(!is_retryable_transport_error(&anyhow::Error::from(
            std::io::Error::other("local runner state failure")
        )));
    }

    #[test]
    fn manifest_missing_blob_classifies_by_endpoint_and_status_only() {
        // Exact manifest-endpoint 412 is the missing-blob signal.
        let missing = request_status_error(
            "POST",
            "/api/manifest",
            http::StatusCode::PRECONDITION_FAILED,
            b"412 precondition body",
        );
        assert_eq!(
            api_failure_kind(&missing),
            Some(ApiFailureKind::ManifestReferencesMissingBlob)
        );

        // Malicious or unrelated body text on the same endpoint cannot spoof
        // or negate the classification: the body is never read.
        let malicious = request_status_error(
            "POST",
            "/api/manifest",
            http::StatusCode::PRECONDITION_FAILED,
            b"SECRET precondition body text",
        );
        assert_eq!(
            api_failure_kind(&malicious),
            Some(ApiFailureKind::ManifestReferencesMissingBlob)
        );
        let empty_body = request_status_error(
            "POST",
            "/api/manifest",
            http::StatusCode::PRECONDITION_FAILED,
            &[],
        );
        assert_eq!(
            api_failure_kind(&empty_body),
            Some(ApiFailureKind::ManifestReferencesMissingBlob)
        );

        // The same status on another endpoint is a different precondition
        // (for example a missing head manifest on swap) and must not retry
        // object re-uploads.
        let swap_head = request_status_error(
            "PUT",
            "/api/head",
            http::StatusCode::PRECONDITION_FAILED,
            b"412 precondition body",
        );
        assert_eq!(api_failure_kind(&swap_head), Some(ApiFailureKind::Other));

        // A 500 with missing-blob text on the manifest endpoint is not the
        // typed precondition either.
        let five_hundred = request_status_error(
            "POST",
            "/api/manifest",
            http::StatusCode::INTERNAL_SERVER_ERROR,
            b"412 precondition body",
        );
        assert_eq!(api_failure_kind(&five_hundred), Some(ApiFailureKind::Other));

        // Transport and other non-status errors carry no typed kind.
        assert_eq!(
            api_failure_kind(&anyhow::anyhow!("connection refused")),
            None
        );

        // Context layers around the typed error preserve the classification.
        let wrapped = missing.context("upload reachability manifest");
        assert_eq!(
            api_failure_kind(&wrapped),
            Some(ApiFailureKind::ManifestReferencesMissingBlob)
        );
    }
}

/// D5: the one bounded response-body reader, exercised through both call
/// sites (JSON response read and blob download) over both transports (real
/// HTTP against an axum test server, and the in-process LocalHub).
#[cfg(test)]
mod response_reader_tests {
    use super::{
        hash_mismatch_error, request_error_status, ApiClient, ResponseReadError,
        MAX_API_ERROR_BYTES,
    };
    use std::time::Duration;

    const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    async fn serve(router: axum::Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{address}"), task)
    }

    /// The JSON array `["<name>"]` serializes to exactly `name.len() + 4`
    /// bytes, giving the boundary tests a response whose exact size is known.
    fn json_array_body(name: &str) -> (Vec<u8>, usize) {
        let body = serde_json::to_vec(&vec![name]).unwrap();
        let len = body.len();
        assert_eq!(len, name.len() + 4);
        (body, len)
    }

    async fn expect_over_limit(error: anyhow::Error, limit: usize) {
        assert!(
            error.chain().any(|cause| matches!(
                cause.downcast_ref::<ResponseReadError>(),
                Some(ResponseReadError::OverLimit { limit: found }) if *found == limit
            )),
            "expected over-limit error with limit {limit}, got: {error:#}"
        );
    }

    #[tokio::test]
    async fn http_json_limit_minus_one_is_rejected() {
        let (body, len) = json_array_body("abcdefghijklm");
        let router = axum::Router::new().route(
            "/api/workspaces",
            axum::routing::get(move || {
                let body = body.clone();
                async move {
                    let mut response = axum::response::Response::new(axum::body::Body::from(body));
                    response.headers_mut().insert(
                        http::header::CONTENT_TYPE,
                        http::header::HeaderValue::from_static("application/json"),
                    );
                    response
                }
            }),
        );
        let (url, task) = serve(router).await;
        let error = ApiClient::new(&url, None)
            .get_json_bounded::<Vec<String>>("/api/workspaces", "", len - 1)
            .await
            .unwrap_err();
        task.abort();
        expect_over_limit(error, len - 1).await;
    }

    #[tokio::test]
    async fn http_json_exact_limit_is_accepted() {
        let (body, len) = json_array_body("abcdefghijklm");
        let router = axum::Router::new().route(
            "/api/workspaces",
            axum::routing::get(move || {
                let body = body.clone();
                async move {
                    let mut response = axum::response::Response::new(axum::body::Body::from(body));
                    response.headers_mut().insert(
                        http::header::CONTENT_TYPE,
                        http::header::HeaderValue::from_static("application/json"),
                    );
                    response
                }
            }),
        );
        let (url, task) = serve(router).await;
        let workspaces: Vec<String> = ApiClient::new(&url, None)
            .get_json_bounded("/api/workspaces", "", len)
            .await
            .unwrap();
        task.abort();
        assert_eq!(workspaces, vec!["abcdefghijklm".to_string()]);
    }

    #[tokio::test]
    async fn http_json_plus_one_is_accepted() {
        // A response one byte under the limit is accepted and decoded.
        let (body, len) = json_array_body("abcdefghijklm");
        let router = axum::Router::new().route(
            "/api/workspaces",
            axum::routing::get(move || {
                let body = body.clone();
                async move { axum::body::Body::from(body) }
            }),
        );
        let (url, task) = serve(router).await;
        let workspaces: Vec<String> = ApiClient::new(&url, None)
            .get_json_bounded("/api/workspaces", "", len + 1)
            .await
            .unwrap();
        task.abort();
        assert_eq!(workspaces, vec!["abcdefghijklm".to_string()]);
    }

    #[tokio::test]
    async fn http_download_limit_minus_one_is_rejected() {
        let blob = vec![7_u8; 13];
        let blob_len = blob.len();
        let route_blob = blob.clone();
        let router = axum::Router::new().route(
            "/api/download/{hash}",
            axum::routing::get(move || {
                let blob = route_blob.clone();
                async move { blob }
            }),
        );
        let (url, task) = serve(router).await;
        let error = ApiClient::new(&url, None)
            .download_file_bounded(HASH, blob_len - 1)
            .await
            .unwrap_err();
        task.abort();
        expect_over_limit(error, blob_len - 1).await;
    }

    #[tokio::test]
    async fn http_download_exact_limit_is_accepted() {
        let blob = vec![7_u8; 13];
        let blob_len = blob.len();
        let route_blob = blob.clone();
        let router = axum::Router::new().route(
            "/api/download/{hash}",
            axum::routing::get(move || {
                let blob = route_blob.clone();
                async move { blob }
            }),
        );
        let (url, task) = serve(router).await;
        let bytes = ApiClient::new(&url, None)
            .download_file_bounded(HASH, blob_len)
            .await
            .unwrap();
        task.abort();
        assert_eq!(bytes, blob);
    }

    #[tokio::test]
    async fn http_download_plus_one_is_accepted() {
        let blob = vec![7_u8; 13];
        let blob_len = blob.len();
        let route_blob = blob.clone();
        let router = axum::Router::new().route(
            "/api/download/{hash}",
            axum::routing::get(move || {
                let blob = route_blob.clone();
                async move { blob }
            }),
        );
        let (url, task) = serve(router).await;
        let bytes = ApiClient::new(&url, None)
            .download_file_bounded(HASH, blob_len + 1)
            .await
            .unwrap();
        task.abort();
        assert_eq!(bytes, blob);
    }

    #[tokio::test]
    async fn local_json_limit_boundaries() {
        // GET /api/workspace/format returns a fixed-size JSON object
        // (`{"format_version":2}`) with no workspace seeded, giving the
        // boundary tests a response whose exact size is known.
        let directory = tempfile::tempdir().unwrap();
        let hub = crate::LocalHub::open(directory.path().join("hub"), None)
            .await
            .unwrap();
        let api = ApiClient::local(hub, None);
        let response: super::WorkspaceFormatResponse = api
            .get_json("/api/workspace/format", "workspace_id=ws")
            .await
            .unwrap();
        assert_eq!(response.format_version, 2);
        let len = serde_json::to_vec(&response).unwrap().len();
        assert_eq!(len, "{\"format_version\":2}".len());

        // Limit at minus one: rejected as over-limit.
        let error = api
            .get_json_bounded::<super::WorkspaceFormatResponse>(
                "/api/workspace/format",
                "workspace_id=ws",
                len - 1,
            )
            .await
            .unwrap_err();
        expect_over_limit(error, len - 1).await;

        // Exact limit: accepted.
        let response: super::WorkspaceFormatResponse = api
            .get_json_bounded("/api/workspace/format", "workspace_id=ws", len)
            .await
            .unwrap();
        assert_eq!(response.format_version, 2);

        // Plus one: also accepted (the exact limit is a minimum bound).
        let response: super::WorkspaceFormatResponse = api
            .get_json_bounded("/api/workspace/format", "workspace_id=ws", len + 1)
            .await
            .unwrap();
        assert_eq!(response.format_version, 2);
    }

    #[tokio::test]
    async fn local_download_limit_boundaries() {
        let blob = vec![9_u8; 17];
        let directory = tempfile::tempdir().unwrap();
        let hub = crate::LocalHub::open(directory.path().join("hub"), None)
            .await
            .unwrap();
        let hash = feanorfs_common::hash_bytes(&blob);
        hub.migration_db().store_blob(&hash, &blob).unwrap();
        let api = ApiClient::local(hub, None);

        let error = api
            .download_file_bounded(&hash, blob.len() - 1)
            .await
            .unwrap_err();
        expect_over_limit(error, blob.len() - 1).await;

        let bytes = api.download_file_bounded(&hash, blob.len()).await.unwrap();
        assert_eq!(bytes, blob);

        let bytes = api
            .download_file_bounded(&hash, blob.len() + 1)
            .await
            .unwrap();
        assert_eq!(bytes, blob);
    }

    #[tokio::test]
    async fn local_json_decode_failure_is_typed() {
        // A hub route returning non-JSON on success (simulated by requesting
        // an unknown blob) must surface as a status error, not a hang.
        let directory = tempfile::tempdir().unwrap();
        let hub = crate::LocalHub::open(directory.path().join("hub"), None)
            .await
            .unwrap();
        let api = ApiClient::local(hub, None);
        let error = api
            .get_json_bounded::<Vec<String>>("/api/download/not-a-blob", "", MAX_API_ERROR_BYTES)
            .await
            .unwrap_err();
        assert!(request_error_status(&error).is_some(), "{error:#}");
    }

    #[tokio::test]
    async fn stalled_http_body_fails_bounded_with_typed_timeout() {
        // The server sends headers and a first chunk, then stops answering.
        // The shared reader must fail within the read bound with the typed
        // timeout instead of buffering forever.
        let router = axum::Router::new().route(
            "/api/download/{hash}",
            axum::routing::get(|| async {
                let stream = futures_util::stream::iter(vec![
                    Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(b"start")),
                    Box::pin(async {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(b"end"))
                    })
                    .await,
                ]);
                axum::body::Body::from_stream(stream)
            }),
        );
        let (url, task) = serve(router).await;
        let client = ApiClient::new_with_timeouts(
            &url,
            None,
            Duration::from_secs(2),
            Duration::from_millis(400),
        )
        .unwrap();
        let start = std::time::Instant::now();
        let error = client.download_file_bounded(HASH, 1024).await.unwrap_err();
        task.abort();
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "request hung on a stalled body"
        );
        // The transport read bound fires at the request level (reqwest's own
        // timeout) or at the body-stream level (typed Timeout variant);
        // either way the stalled body must fail bounded and be classified as
        // retryable transport, never hang or buffer unbounded.
        assert!(super::is_retryable_transport_error(&error), "{error:#}");
        assert!(
            error.chain().any(|cause| {
                cause
                    .downcast_ref::<ResponseReadError>()
                    .is_some_and(|read_error| {
                        matches!(read_error, ResponseReadError::Timeout { .. })
                    })
            }) || error.chain().any(|cause| {
                cause
                    .downcast_ref::<reqwest::Error>()
                    .is_some_and(|transport| transport.is_timeout())
            }),
            "expected a bounded timeout failure, got: {error:#}"
        );
    }

    #[tokio::test]
    async fn premature_http_eof_fails_with_typed_transport_error() {
        // A raw HTTP server declares a 100-byte body but sends 5 bytes and
        // closes the connection. The client reads the first chunk, then the
        // transport ends early; the shared reader must surface the truncated
        // body as a typed transport error instead of returning a short body.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nshort";
            use tokio::io::AsyncWriteExt as _;
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let url = format!("http://{address}");
        let error = ApiClient::new(&url, None)
            .download_file_bounded(HASH, 4096)
            .await
            .unwrap_err();
        let _ = server.await;
        // Windows surfaces the abort at the hyper send-request layer
        // (os error 10053) instead of as a mid-body read failure; both are
        // bounded transport failures. The invariant under test is the
        // retryable-transport classification, not which layer reports it.
        assert!(
            error.chain().any(|cause| {
                cause
                    .downcast_ref::<ResponseReadError>()
                    .is_some_and(|read_error| {
                        matches!(
                            read_error,
                            ResponseReadError::Transport { .. } | ResponseReadError::Timeout { .. }
                        )
                    })
            }) || error.chain().any(|cause| {
                cause.downcast_ref::<reqwest::Error>().is_some_and(|transport| {
                    transport.is_request() || transport.is_connect() || transport.is_body()
                })
            }),
            "expected typed transport error for truncated body, got: {error:#}"
        );
        assert!(
            super::is_retryable_transport_error(&error),
            "truncated body must classify as retryable transport: {error:#}"
        );
    }

    #[test]
    fn corrupt_blob_hash_is_a_typed_error() {
        let error = hash_mismatch_error(HASH);
        assert!(error.to_string().contains("hash mismatch"));
        assert!(error.to_string().contains(HASH));
        assert!(error.chain().any(|cause| matches!(
            cause.downcast_ref::<ResponseReadError>(),
            Some(ResponseReadError::HashMismatch { expected }) if expected == HASH
        )));
    }
}
