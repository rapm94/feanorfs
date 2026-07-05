use clap::Parser;
use feanorfs_server::{run_gc, run_http_server, ServeOptions};
use std::path::PathBuf;

#[derive(Parser)]
pub struct ServeCli {
    #[arg(long, env = "FEANORFS_TOKEN", visible_alias = "password")]
    pub token: Option<String>,
    #[arg(long)]
    pub allow_open: bool,
    #[arg(long)]
    pub mdns: bool,
    #[arg(long, default_value = "3030", env = "FEANORFS_PORT")]
    pub port: u16,
    #[arg(long, default_value = "server-data", env = "FEANORFS_DATA_DIR")]
    pub data_dir: PathBuf,
    #[arg(long, env = "FEANORFS_GC_INTERVAL", default_value = "0")]
    pub gc_interval: u64,
    #[arg(long, default_value = "10")]
    pub gc_grace_minutes: u64,
    #[arg(long, default_value = "30")]
    pub tombstone_retention_days: u64,
    /// Run blob GC once and exit (no HTTP server)
    #[arg(long)]
    pub gc_only: bool,
}

impl From<ServeCli> for ServeOptions {
    fn from(c: ServeCli) -> Self {
        ServeOptions {
            data_dir: c.data_dir,
            port: c.port,
            token: c.token,
            allow_open: c.allow_open,
            mdns: c.mdns,
            gc_interval_secs: c.gc_interval,
            gc_grace_minutes: c.gc_grace_minutes,
            tombstone_retention_days: c.tombstone_retention_days,
        }
    }
}

pub async fn run_serve(args: ServeCli) -> anyhow::Result<()> {
    if args.gc_only {
        return run_gc_cli(args).await;
    }
    run_http_server(args.into()).await
}

pub async fn run_gc_cli(args: ServeCli) -> anyhow::Result<()> {
    let stats = run_gc(&ServeOptions {
        data_dir: args.data_dir,
        gc_grace_minutes: args.gc_grace_minutes,
        tombstone_retention_days: args.tombstone_retention_days,
        ..ServeOptions::default()
    })
    .await?;
    println!(
        "GC complete: {} blobs deleted ({} bytes), {} tombstones purged",
        stats.blobs_deleted, stats.bytes_freed, stats.tombstones_purged
    );
    Ok(())
}
