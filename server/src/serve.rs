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
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("8.8.8.8:80")?;
    Ok(socket.local_addr()?.ip().to_string())
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
        let grace = Duration::from_secs(opts.gc_grace_minutes * 60);
        let retention = Duration::from_secs(opts.tombstone_retention_days * 86400);
        let interval = Duration::from_secs(opts.gc_interval_secs);
        tokio::spawn(async move {
            let db = match crate::db::Db::new(data_dir.join("db.sqlite")).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::error!("GC task failed to open db: {e}");
                    return;
                }
            };
            loop {
                tokio::time::sleep(interval).await;
                match crate::gc::run_gc(&db, &data_dir, grace, retention).await {
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
    crate::gc::run_gc(
        &db,
        &opts.data_dir,
        Duration::from_secs(opts.gc_grace_minutes * 60),
        Duration::from_secs(opts.tombstone_retention_days * 86400),
    )
    .await
}
