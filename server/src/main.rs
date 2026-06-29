use clap::Parser;
use feanorfs_server::{build_router, init_app_state};
use std::net::SocketAddr;

#[derive(Parser)]
#[command(name = "feanorfs-server")]
#[command(about = "Content-addressed blob storage and sync metadata server for FeanorFS")]
struct Cli {
    /// Authentication token. Clients must send this as a Bearer token. In SaaS mode, this becomes a per-user API key.
    #[arg(long, env = "FEANORFS_TOKEN", visible_alias = "password")]
    token: Option<String>,

    /// Enable mDNS service advertisement for LAN discovery (off by default for internet deployments)
    #[arg(long)]
    mdns: bool,

    /// Port to listen on (default: 3030). Use different ports when running multiple instances behind a reverse proxy.
    #[arg(long, default_value = "3030", env = "FEANORFS_PORT")]
    port: u16,

    /// Data directory for SQLite DB and blob storage (default: ./server-data). Each instance should have its own.
    #[arg(long, default_value = "server-data", env = "FEANORFS_DATA_DIR")]
    data_dir: std::path::PathBuf,
}

fn local_ip() -> anyhow::Result<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
    socket.connect("8.8.8.8:80")?;
    Ok(socket.local_addr()?.ip().to_string())
}

fn register_mdns(port: u16) -> anyhow::Result<mdns_sd::ServiceDaemon> {
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "feanorfs_server=info,tower_http=info".into()),
        )
        .init();

    let state = init_app_state(cli.data_dir.clone(), cli.token.clone()).await?;
    let app = build_router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], cli.port));
    tracing::info!(
        "FeanorFS Sync Server starting on http://{} (data: {})",
        addr,
        cli.data_dir.display()
    );

    let _mdns_daemon = if cli.mdns {
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

    if cli.token.is_some() {
        tracing::info!("Authentication enabled (token required)");
    } else {
        tracing::warn!("No auth token set. Run with --token <TOKEN> for authenticated access.");
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
