use anyhow::{bail, ensure, Context, Result};
use feanorfs_common::{RelayConfig, SyncRequest, SyncResponse};
use futures_util::StreamExt as _;
use reqwest::{Certificate, Client};
use serde::Deserialize;
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

#[derive(Deserialize)]
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
        serde_json::from_slice(&body)
            .with_context(|| format!("Failed to parse GET {path} response"))
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
                let status = resp.status();
                let limit = if status.is_success() {
                    MAX_API_RESPONSE_BYTES
                } else {
                    MAX_API_ERROR_BYTES
                };
                if resp
                    .content_length()
                    .is_some_and(|length| length > limit as u64)
                {
                    bail!("HTTP response exceeds {limit} byte limit");
                }
                let mut bytes = Vec::with_capacity(
                    resp.content_length()
                        .and_then(|length| usize::try_from(length).ok())
                        .unwrap_or(0)
                        .min(limit),
                );
                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.context("read HTTP response body")?;
                    if bytes.len().saturating_add(chunk.len()) > limit {
                        bail!("HTTP response exceeds {limit} byte limit");
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok((status, bytes))
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
                let limit = if resp.status().is_success() {
                    MAX_API_RESPONSE_BYTES
                } else {
                    MAX_API_ERROR_BYTES
                };
                LocalHub::read_body_bounded(resp, limit).await
            }
        }
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
                let status = response.status();
                let limit = if status.is_success() {
                    max_bytes
                } else {
                    MAX_API_ERROR_BYTES
                };
                if response
                    .content_length()
                    .is_some_and(|length| length > limit as u64)
                {
                    bail!("download response exceeds {limit} byte limit");
                }
                let mut body = Vec::with_capacity(
                    response
                        .content_length()
                        .and_then(|length| usize::try_from(length).ok())
                        .unwrap_or(0)
                        .min(limit),
                );
                let mut stream = response.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.context("read HTTP response body")?;
                    if body.len().saturating_add(chunk.len()) > limit {
                        bail!("download response exceeds {limit} byte limit");
                    }
                    body.extend_from_slice(&chunk);
                }
                (status, body)
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
                let status = response.status();
                let limit = if status.is_success() {
                    max_bytes
                } else {
                    MAX_API_ERROR_BYTES
                };
                LocalHub::read_body_bounded(response, limit).await?
            }
        };
        if status == http::StatusCode::UNAUTHORIZED {
            bail!(
                "Server requires a valid access token. Paste its fnh1/fnr1 invite into `feanorfs start`, or set one with `feanorfs connect <URL> --token <TOKEN>`"
            );
        }
        if !status.is_success() {
            return Err(request_status_error("GET", &path, status, &body));
        }
        Ok(body)
    }

    pub async fn get_workspaces(&self) -> Result<Vec<String>> {
        self.get_json("/api/workspaces", "").await
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
        check_server_version, is_retryable_transport_error, request_status_error, ApiClient,
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
}
