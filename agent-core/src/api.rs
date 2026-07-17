use anyhow::{bail, Context, Result};
use feanorfs_common::{RelayConfig, SyncRequest, SyncResponse};
use reqwest::{Certificate, Client};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

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
            let hub_dir = config.hub_data_dir(workspace);
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
            let hub_dir = config.hub_data_dir(workspace);
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
