use anyhow::{bail, ensure, Context, Result};
use feanorfs_common::{RelayConfig, SyncRequest, SyncResponse};
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
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(8);

impl ApiClient {
    pub fn new(server_url: &str, server_password: Option<&str>) -> Self {
        Self {
            backend: Backend::Http {
                client: Client::new(),
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
        let mut builder = Client::builder();
        if let Some(pem) = tls_ca_pem {
            let certificate = Certificate::from_pem(pem.as_bytes())
                .context("parse FeanorFS hub CA certificate")?;
            builder = builder.add_root_certificate(certificate);
        }
        if let Some((hostname, addresses)) = resolution {
            builder = builder.resolve_to_addrs(hostname, addresses);
        }
        let client = builder.build().context("build FeanorFS HTTP client")?;
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
            bail!("Server requires a valid access token. Paste its fnh1/fnr1 invite into `feanorfs start`, or set one with `feanorfs connect <URL> --token <TOKEN>`");
        }
        if !status.is_success() {
            bail!(
                "GET {path} failed with status {status}: {}",
                String::from_utf8_lossy(&body)
            );
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
            bail!("Server requires a valid access token. Paste its fnh1/fnr1 invite into `feanorfs start`, or set one with `feanorfs connect <URL> --token <TOKEN>`");
        }
        if !status.is_success() {
            bail!(
                "POST {path} failed with status {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        serde_json::from_slice(&bytes)
            .with_context(|| format!("Failed to parse POST {path} response"))
    }

    async fn post_bytes(&self, path: &str, query: &str, body: Vec<u8>) -> Result<()> {
        let (status, bytes) = self
            .raw_request(http::Method::POST, path, query, body, None)
            .await?;
        if status == http::StatusCode::UNAUTHORIZED {
            bail!("Server requires a valid access token. Paste its fnh1/fnr1 invite into `feanorfs start`, or set one with `feanorfs connect <URL> --token <TOKEN>`");
        }
        if !status.is_success() {
            bail!(
                "POST {path} failed with status {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
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
                let bytes = resp.bytes().await.context("read HTTP response body")?;
                Ok((status, bytes.to_vec()))
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
                LocalHub::read_body(resp).await
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
        let mut manifest = hashes.join("\n").into_bytes();
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
                    bail!("Server requires a valid access token. Paste its fnh1/fnr1 invite into `feanorfs start`, or set one with `feanorfs connect <URL> --token <TOKEN>`");
                }
                if !status.is_success() {
                    bail!("GET /api/version failed with status {status}");
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
        let (status, body) = self
            .raw_request(
                http::Method::GET,
                &format!("/api/download/{hash}"),
                "",
                Vec::new(),
                None,
            )
            .await?;
        if status == http::StatusCode::UNAUTHORIZED {
            bail!("Server requires a valid access token. Paste its fnh1/fnr1 invite into `feanorfs start`, or set one with `feanorfs connect <URL> --token <TOKEN>`");
        }
        if !status.is_success() {
            bail!(
                "Download failed with status {status}: {}",
                String::from_utf8_lossy(&body)
            );
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
    use super::{check_server_version, ApiClient};

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
    async fn endpointless_hub_404_remains_explicitly_compatible() {
        let (url, task) = serve(axum::Router::new()).await;
        ApiClient::new(&url, Some("test-token"))
            .ensure_server_compatible()
            .await
            .unwrap();
        task.abort();
    }
}
