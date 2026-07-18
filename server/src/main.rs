use clap::{Parser, Subcommand};
use feanorfs_server::{run_gc, run_http_server, ServeOptions};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "feanorfs-server")]
#[command(about = "Content-addressed blob storage and sync metadata server for FeanorFS")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the secure sync server (default).
    Serve(ServeArgs),
    /// Garbage-collect unreferenced blobs and old tombstones.
    Gc(GcArgs),
}

#[derive(Parser, Clone)]
struct ServeArgs {
    /// Bearer token (default: generate and persist one in the hub data directory)
    #[arg(long, env = "FEANORFS_TOKEN", visible_alias = "password")]
    token: Option<String>,
    #[arg(long)]
    allow_open: bool,
    #[arg(long, conflicts_with_all = ["tls_cert", "tls_key", "tls_ca"])]
    allow_http: bool,
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    #[arg(long, requires = "tls_cert")]
    tls_ca: Option<PathBuf>,
    #[arg(long)]
    mdns: bool,
    #[arg(long, visible_alias = "pair-relay")]
    relay: bool,
    #[arg(long, default_value = "3030", env = "FEANORFS_PORT")]
    port: u16,
    #[arg(
        long,
        default_value = "0.0.0.0",
        env = "FEANORFS_BIND",
        value_parser = parse_bind_ip
    )]
    bind: Box<std::net::IpAddr>,
    #[arg(long, default_value = "server-data", env = "FEANORFS_DATA_DIR")]
    data_dir: PathBuf,
    #[arg(long, env = "FEANORFS_GC_INTERVAL", default_value = "0")]
    gc_interval: u64,
    #[arg(long, default_value = "10")]
    gc_grace_minutes: u64,
    #[arg(long, default_value = "30")]
    tombstone_retention_days: u64,
    #[arg(long, default_value = "30")]
    snapshot_retention_days: u64,
    #[arg(long, default_value = "50")]
    snapshot_keep_last: usize,
}

fn parse_bind_ip(value: &str) -> Result<Box<std::net::IpAddr>, std::net::AddrParseError> {
    value.parse().map(Box::new)
}

#[derive(Parser)]
struct GcArgs {
    #[arg(long, default_value = "server-data", env = "FEANORFS_DATA_DIR")]
    data_dir: PathBuf,
    #[arg(long, default_value = "10")]
    gc_grace_minutes: u64,
    #[arg(long, default_value = "30")]
    tombstone_retention_days: u64,
    #[arg(long, default_value = "30")]
    snapshot_retention_days: u64,
    #[arg(long, default_value = "50")]
    snapshot_keep_last: usize,
}

impl From<ServeArgs> for ServeOptions {
    fn from(a: ServeArgs) -> Self {
        ServeOptions {
            data_dir: a.data_dir,
            bind_ip: *a.bind,
            port: a.port,
            token: a.token,
            allow_open: a.allow_open,
            allow_http: a.allow_http,
            tls_cert: a.tls_cert,
            tls_key: a.tls_key,
            tls_ca: a.tls_ca,
            mdns: a.mdns,
            relay: a.relay,
            gc_interval_secs: a.gc_interval,
            gc_grace_minutes: a.gc_grace_minutes,
            tombstone_retention_days: a.tombstone_retention_days,
            snapshot_retention_days: a.snapshot_retention_days,
            snapshot_keep_last: a.snapshot_keep_last,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "feanorfs_server=info,tower_http=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Gc(args)) => run_gc_once(&args).await,
        Some(Commands::Serve(args)) => run_http_server(args.into()).await,
        None => run_http_server(cli.serve.into()).await,
    }
}

async fn run_gc_once(args: &GcArgs) -> anyhow::Result<()> {
    let opts = ServeOptions {
        data_dir: args.data_dir.clone(),
        gc_grace_minutes: args.gc_grace_minutes,
        tombstone_retention_days: args.tombstone_retention_days,
        snapshot_retention_days: args.snapshot_retention_days,
        snapshot_keep_last: args.snapshot_keep_last,
        ..ServeOptions::default()
    };
    let stats = run_gc(&opts).await?;
    println!(
        "GC complete: {} blobs deleted ({} bytes), {} tombstones purged",
        stats.blobs_deleted, stats.bytes_freed, stats.tombstones_purged
    );
    Ok(())
}
