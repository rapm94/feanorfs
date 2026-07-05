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
    /// Run the HTTP sync server (default).
    Serve(ServeArgs),
    /// Garbage-collect unreferenced blobs and old tombstones.
    Gc(GcArgs),
}

#[derive(Parser, Clone)]
struct ServeArgs {
    #[arg(long, env = "FEANORFS_TOKEN", visible_alias = "password")]
    token: Option<String>,
    #[arg(long)]
    allow_open: bool,
    #[arg(long)]
    mdns: bool,
    #[arg(long, default_value = "3030", env = "FEANORFS_PORT")]
    port: u16,
    #[arg(long, default_value = "server-data", env = "FEANORFS_DATA_DIR")]
    data_dir: PathBuf,
    #[arg(long, env = "FEANORFS_GC_INTERVAL", default_value = "0")]
    gc_interval: u64,
    #[arg(long, default_value = "10")]
    gc_grace_minutes: u64,
    #[arg(long, default_value = "30")]
    tombstone_retention_days: u64,
}

#[derive(Parser)]
struct GcArgs {
    #[arg(long, default_value = "server-data", env = "FEANORFS_DATA_DIR")]
    data_dir: PathBuf,
    #[arg(long, default_value = "10")]
    gc_grace_minutes: u64,
    #[arg(long, default_value = "30")]
    tombstone_retention_days: u64,
}

impl From<ServeArgs> for ServeOptions {
    fn from(a: ServeArgs) -> Self {
        ServeOptions {
            data_dir: a.data_dir,
            port: a.port,
            token: a.token,
            allow_open: a.allow_open,
            mdns: a.mdns,
            gc_interval_secs: a.gc_interval,
            gc_grace_minutes: a.gc_grace_minutes,
            tombstone_retention_days: a.tombstone_retention_days,
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
        ..ServeOptions::default()
    };
    let stats = run_gc(&opts).await?;
    println!(
        "GC complete: {} blobs deleted ({} bytes), {} tombstones purged",
        stats.blobs_deleted, stats.bytes_freed, stats.tombstones_purged
    );
    Ok(())
}
