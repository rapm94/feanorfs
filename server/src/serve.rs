use anyhow::{Context as _, Result};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub data_dir: PathBuf,
    pub bind_ip: IpAddr,
    pub port: u16,
    pub token: Option<String>,
    pub allow_open: bool,
    pub allow_http: bool,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub tls_ca: Option<PathBuf>,
    pub mdns: bool,
    pub relay: bool,
    pub gc_interval_secs: u64,
    pub gc_grace_minutes: u64,
    pub tombstone_retention_days: u64,
    pub snapshot_retention_days: u64,
    pub snapshot_keep_last: usize,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("server-data"),
            bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 3030,
            token: None,
            allow_open: false,
            allow_http: false,
            tls_cert: None,
            tls_key: None,
            tls_ca: None,
            mdns: false,
            relay: false,
            gc_interval_secs: 0,
            gc_grace_minutes: 10,
            tombstone_retention_days: 30,
            snapshot_retention_days: 30,
            snapshot_keep_last: 50,
        }
    }
}

pub fn resolve_or_create_auth_token(
    data_dir: &Path,
    token: Option<String>,
    allow_open: bool,
) -> Result<Option<String>> {
    crate::ensure_recovery_complete(data_dir)?;
    if allow_open {
        if token.is_some() {
            anyhow::bail!("--allow-open conflicts with --token");
        }
        tracing::warn!("--allow-open: server accepts unauthenticated requests (development only)");
        return Ok(None);
    }

    crate::private_file::create_private_dir(data_dir)?;
    let lock = crate::private_file::open_private_lock(&data_dir.join("auth-token.lock"))?;
    fs2::FileExt::lock_exclusive(&lock).context("lock hub authentication token")?;
    let path = data_dir.join("auth-token");
    let resolved = match token {
        Some(token) => {
            validate_auth_token(&token)?;
            crate::private_file::atomic_private_write(&path, token.as_bytes())?;
            token
        }
        None if path.exists() => {
            let token = fs::read_to_string(&path)
                .with_context(|| format!("read hub authentication token {}", path.display()))?;
            validate_auth_token(&token)?;
            token
        }
        None => {
            let token = feanorfs_common::generate_password()?;
            crate::private_file::atomic_private_write(&path, token.as_bytes())?;
            token
        }
    };
    Ok(Some(resolved))
}

pub(crate) fn validate_auth_token(token: &str) -> Result<()> {
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        anyhow::bail!("hub authentication token must be non-empty and contain no whitespace");
    }
    Ok(())
}

fn mdns_service_info(port: u16, tls: Option<&crate::TlsIdentity>) -> Result<mdns_sd::ServiceInfo> {
    use mdns_sd::ServiceInfo;

    let scheme = if tls.is_some() { "https" } else { "http" };
    let mut props = vec![("v", "1"), ("scheme", scheme)];
    if let Some(fingerprint) = tls.and_then(|identity| identity.fingerprint.as_deref()) {
        props.push(("ca", fingerprint));
    }
    let hostname = tls
        .and_then(|identity| identity.mdns_hostname.as_deref())
        .unwrap_or("feanorfs-server.local");
    let instance = hostname.strip_suffix(".local").unwrap_or(hostname);
    ServiceInfo::new(
        feanorfs_common::HUB_MDNS_SERVICE,
        instance,
        &format!("{hostname}."),
        "",
        port,
        &props[..],
    )
    .map(mdns_sd::ServiceInfo::enable_addr_auto)
    .map_err(Into::into)
}

fn register_mdns(port: u16, tls: Option<&crate::TlsIdentity>) -> Result<mdns_sd::ServiceDaemon> {
    use mdns_sd::ServiceDaemon;

    let daemon = ServiceDaemon::new()?;
    daemon.register(mdns_service_info(port, tls)?)?;
    Ok(daemon)
}

pub async fn run_http_server(opts: ServeOptions) -> Result<()> {
    let guard = crate::acquire_hub_runtime(&opts.data_dir)?;
    run_http_server_guarded(opts, guard).await
}

pub async fn run_http_server_guarded(
    mut opts: ServeOptions,
    _guard: crate::HubRuntimeGuard,
) -> Result<()> {
    crate::ensure_recovery_complete(&opts.data_dir)?;
    let tls = crate::prepare_tls(&mut opts)?;
    let token = resolve_or_create_auth_token(&opts.data_dir, opts.token.take(), opts.allow_open)?;
    let state = crate::init_app_state(opts.data_dir.clone(), token.clone()).await?;
    let gc_db = state.db.clone();
    let publication_lock = state.publication_lock.clone();
    let app = if opts.relay {
        crate::app::build_router_with_relay(state)
    } else {
        crate::build_router(state)
    };

    let addr = SocketAddr::new(opts.bind_ip, opts.port);
    let scheme = if tls.is_some() { "https" } else { "http" };
    tracing::info!(
        "FeanorFS Sync Server starting on {}://{} (data: {})",
        scheme,
        addr,
        opts.data_dir.display()
    );

    let _mdns_daemon = if opts.mdns {
        match register_mdns(addr.port(), tls.as_ref()) {
            Ok(d) => {
                tracing::info!("mDNS service registered (discoverable on local network)");
                Some(d)
            }
            Err(e) => {
                tracing::warn!("Failed to register mDNS service: {}", e);
                None
            }
        }
    } else {
        tracing::info!("mDNS disabled (default). Use --mdns to enable LAN discovery.");
        None
    };

    if token.is_some() {
        tracing::info!("Authentication enabled (token required)");
    }
    if opts.relay {
        tracing::info!("Opaque pairing and inner-TLS tunnel relay enabled");
    }

    if opts.gc_interval_secs > 0 {
        let data_dir = opts.data_dir.clone();
        let db = gc_db.clone();
        let publication_lock = publication_lock.clone();
        let grace = Duration::from_secs(opts.gc_grace_minutes * 60);
        let retention = Duration::from_secs(opts.tombstone_retention_days * 86400);
        let snapshot_retention = Duration::from_secs(opts.snapshot_retention_days * 86400);
        let snapshot_keep_last = opts.snapshot_keep_last;
        let interval = Duration::from_secs(opts.gc_interval_secs);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                match crate::gc::run_gc(
                    &db,
                    &data_dir,
                    grace,
                    retention,
                    snapshot_retention,
                    snapshot_keep_last,
                    &publication_lock,
                )
                .await
                {
                    Ok(s) => tracing::info!(
                        "GC: deleted {} blobs ({} bytes), purged {} tombstones",
                        s.blobs_deleted,
                        s.bytes_freed,
                        s.tombstones_purged
                    ),
                    Err(e) => tracing::error!("GC failed: {e}"),
                }
            }
        });
    }

    if let Some(identity) = tls {
        let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            identity.cert_path,
            identity.key_path,
        )
        .await
        .context("load TLS server identity")?;
        axum_server::bind_rustls(addr, config)
            .serve(app.into_make_service())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
    }
    Ok(())
}

pub async fn run_gc(opts: &ServeOptions) -> Result<crate::gc::GcStats> {
    let _guard = crate::acquire_hub_runtime(&opts.data_dir)?;
    crate::ensure_recovery_complete(&opts.data_dir)?;
    let db = crate::db::Db::new(opts.data_dir.join("db.sqlite")).await?;
    let publication_lock = tokio::sync::RwLock::new(());
    crate::gc::run_gc(
        &db,
        &opts.data_dir,
        Duration::from_secs(opts.gc_grace_minutes * 60),
        Duration::from_secs(opts.tombstone_retention_days * 86400),
        Duration::from_secs(opts.snapshot_retention_days * 86400),
        opts.snapshot_keep_last,
        &publication_lock,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_auth_token_is_durable_and_rotatable() {
        let data = tempfile::tempdir().unwrap();
        let first = resolve_or_create_auth_token(data.path(), None, false)
            .unwrap()
            .unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(
            resolve_or_create_auth_token(data.path(), None, false)
                .unwrap()
                .as_deref(),
            Some(first.as_str())
        );
        assert_eq!(
            resolve_or_create_auth_token(data.path(), Some("rotated-token".into()), false)
                .unwrap()
                .as_deref(),
            Some("rotated-token")
        );
    }

    #[cfg(unix)]
    #[test]
    fn generated_auth_token_is_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let data = tempfile::tempdir().unwrap();
        resolve_or_create_auth_token(data.path(), None, false).unwrap();
        assert_eq!(
            fs::metadata(data.path().join("auth-token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn allow_open_rejects_ambiguous_token() {
        let data = tempfile::tempdir().unwrap();
        assert!(resolve_or_create_auth_token(data.path(), Some("token".into()), true).is_err());
        assert_eq!(
            resolve_or_create_auth_token(data.path(), None, true).unwrap(),
            None
        );
    }

    #[test]
    fn managed_mdns_tracks_interfaces_without_advertising_secrets() {
        let identity = crate::TlsIdentity {
            cert_path: PathBuf::from("server-cert.pem"),
            key_path: PathBuf::from("server-key.pem"),
            public_ca_pem: Some("public-ca".into()),
            fingerprint: Some("0123456789abcdef".into()),
            mdns_hostname: Some("feanorfs-0123456789abcdef.local".into()),
        };
        let info = mdns_service_info(3030, Some(&identity)).unwrap();

        assert!(info.is_addr_auto());
        assert_eq!(info.get_hostname(), "feanorfs-0123456789abcdef.local.");
        assert_eq!(info.get_property_val_str("v"), Some("1"));
        assert_eq!(info.get_property_val_str("scheme"), Some("https"));
        assert_eq!(info.get_property_val_str("ca"), Some("0123456789abcdef"));
        let rendered = format!("{info:?}");
        assert!(!rendered.contains("token"));
        assert!(!rendered.contains("PRIVATE KEY"));
    }
}
