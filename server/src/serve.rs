use anyhow::Result;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub data_dir: PathBuf,
    pub port: u16,
    pub token: Option<String>,
    pub allow_open: bool,
    pub mdns: bool,
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
            port: 3030,
            token: None,
            allow_open: false,
            mdns: false,
            gc_interval_secs: 0,
            gc_grace_minutes: 10,
            tombstone_retention_days: 30,
            snapshot_retention_days: 30,
            snapshot_keep_last: 50,
        }
    }
}

pub fn resolve_auth_token(token: Option<String>, allow_open: bool) -> Result<Option<String>> {
    if token.is_some() {
        Ok(token)
    } else if allow_open {
        tracing::warn!("--allow-open: server accepts unauthenticated requests (development only)");
        Ok(None)
    } else {
        anyhow::bail!(
            "Authentication token required. Set FEANORFS_TOKEN / --token, or pass --allow-open for local dev."
        )
    }
}

fn local_ip() -> Result<String> {
    for iface in if_addrs::get_if_addrs()? {
        if !iface.is_loopback() {
            if let std::net::IpAddr::V4(ip) = iface.ip() {
                return Ok(ip.to_string());
            }
        }
    }
    anyhow::bail!("no non-loopback IPv4 address found")
}

fn register_mdns(port: u16) -> Result<mdns_sd::ServiceDaemon> {
    use mdns_sd::{ServiceDaemon, ServiceInfo};

    let daemon = ServiceDaemon::new()?;
    let ip = local_ip()?;
    let props: &[(&str, &str)] = &[("v", "1")];
    let service_info = ServiceInfo::new(
        "_feanorfs._tcp.local.",
        "feanorfs-server",
        "feanorfs-server",
        &ip,
        port,
        props,
    )?;
    daemon.register(service_info)?;
    Ok(daemon)
}

pub async fn run_http_server(opts: ServeOptions) -> Result<()> {
    let token = resolve_auth_token(opts.token, opts.allow_open)?;
    let state = crate::init_app_state(opts.data_dir.clone(), token.clone()).await?;
    let gc_db = state.db.clone();
    let publication_lock = state.publication_lock.clone();
    let app = crate::build_router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], opts.port));
    tracing::info!(
        "FeanorFS Sync Server starting on http://{} (data: {})",
        addr,
        opts.data_dir.display()
    );

    let _mdns_daemon = if opts.mdns {
        match register_mdns(addr.port()) {
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

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

pub async fn run_gc(opts: &ServeOptions) -> Result<crate::gc::GcStats> {
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
