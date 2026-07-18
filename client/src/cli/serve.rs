use clap::{Parser, Subcommand};
use feanorfs_common::{encode_hub_invite, HubInvite};
use feanorfs_server::{
    acquire_hub_runtime, export_recovery_bundle, import_recovery_bundle, prepare_tls,
    resolve_or_create_auth_token, rotate_hub_identity, run_gc, run_http_server_guarded,
    ServeOptions,
};
use std::io::IsTerminal as _;
use std::path::PathBuf;
use zeroize::Zeroizing;

use super::util::copy_to_clipboard;

#[derive(Parser)]
#[command(args_conflicts_with_subcommands = true)]
pub struct ServeCli {
    #[command(subcommand)]
    pub command: Option<ServeCommand>,
    /// Bearer token (default: generate and persist one in the hub data directory)
    #[arg(long, env = "FEANORFS_TOKEN", visible_alias = "password")]
    pub token: Option<String>,
    #[arg(long)]
    pub allow_open: bool,
    /// Disable native TLS (development or an external TLS reverse proxy only)
    #[arg(long, conflicts_with_all = ["tls_cert", "tls_key", "tls_ca"])]
    pub allow_http: bool,
    /// PEM certificate chain (default: generate a private hub CA and leaf certificate)
    #[arg(long, requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,
    /// PEM private key matching --tls-cert
    #[arg(long, requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
    /// Optional PEM private CA certificate to embed in secure hub invites
    #[arg(long, requires = "tls_cert")]
    pub tls_ca: Option<PathBuf>,
    /// Public URL placed in the secure hub invite
    #[arg(long)]
    pub public_url: Option<String>,
    /// Print the secret-bearing hub invite even when stdout is not a terminal
    #[arg(long)]
    pub show_invite: bool,
    #[arg(long)]
    pub mdns: bool,
    /// Enable public, opaque pairing and inner-TLS tunnel relay routes
    #[arg(long, visible_alias = "pair-relay")]
    pub relay: bool,
    #[arg(long, default_value = "3030", env = "FEANORFS_PORT")]
    pub port: u16,
    /// Address to listen on (use 127.0.0.1 for local maintenance)
    #[arg(
        long,
        default_value = "0.0.0.0",
        env = "FEANORFS_BIND",
        value_parser = parse_bind_ip
    )]
    pub bind: Box<std::net::IpAddr>,
    #[arg(long, default_value = "server-data", env = "FEANORFS_DATA_DIR")]
    pub data_dir: PathBuf,
    #[arg(long, env = "FEANORFS_GC_INTERVAL", default_value = "0")]
    pub gc_interval: u64,
    #[arg(long, default_value = "10")]
    pub gc_grace_minutes: u64,
    #[arg(long, default_value = "30")]
    pub tombstone_retention_days: u64,
    #[arg(long, default_value = "30")]
    pub snapshot_retention_days: u64,
    #[arg(long, default_value = "50")]
    pub snapshot_keep_last: usize,
    /// Run blob GC once and exit (no HTTP server)
    #[arg(long)]
    pub gc_only: bool,
}

fn parse_bind_ip(value: &str) -> Result<Box<std::net::IpAddr>, std::net::AddrParseError> {
    value.parse().map(Box::new)
}

#[derive(Subcommand)]
pub enum ServeCommand {
    /// Export or restore the private hub identity while the hub is stopped
    Recovery {
        #[command(subcommand)]
        action: RecoveryAction,
    },
}

#[derive(Subcommand)]
pub enum RecoveryAction {
    /// Write an encrypted backup of the hub CA and access token
    Export {
        /// Destination recovery bundle
        path: PathBuf,
        #[arg(long, default_value = "server-data", env = "FEANORFS_DATA_DIR")]
        data_dir: PathBuf,
        /// Atomically replace an existing bundle at PATH
        #[arg(long)]
        replace: bool,
    },
    /// Restore the hub CA and access token from an encrypted bundle
    Import {
        /// Recovery bundle to restore
        path: PathBuf,
        #[arg(long, default_value = "server-data", env = "FEANORFS_DATA_DIR")]
        data_dir: PathBuf,
        /// Replace a different existing hub identity
        #[arg(long)]
        replace: bool,
    },
    /// Replace the private hub CA and access token while preserving encrypted storage
    Rotate {
        /// Destination for the mandatory encrypted backup of the new identity
        path: PathBuf,
        #[arg(long, default_value = "server-data", env = "FEANORFS_DATA_DIR")]
        data_dir: PathBuf,
        /// Atomically replace an existing recovery bundle at PATH
        #[arg(long)]
        replace: bool,
    },
}

impl From<ServeCli> for ServeOptions {
    fn from(c: ServeCli) -> Self {
        ServeOptions {
            data_dir: c.data_dir,
            bind_ip: *c.bind,
            port: c.port,
            token: c.token,
            allow_open: c.allow_open,
            allow_http: c.allow_http,
            tls_cert: c.tls_cert,
            tls_key: c.tls_key,
            tls_ca: c.tls_ca,
            mdns: c.mdns,
            relay: c.relay,
            gc_interval_secs: c.gc_interval,
            gc_grace_minutes: c.gc_grace_minutes,
            tombstone_retention_days: c.tombstone_retention_days,
            snapshot_retention_days: c.snapshot_retention_days,
            snapshot_keep_last: c.snapshot_keep_last,
        }
    }
}

pub async fn run_serve(mut args: ServeCli, json: bool) -> anyhow::Result<()> {
    if json {
        anyhow::bail!("`feanorfs serve` is interactive and does not support --json");
    }
    if let Some(command) = args.command.take() {
        return run_serve_command(command);
    }
    if args.gc_only {
        return run_gc_cli(args).await;
    }
    let public_url = args.public_url.clone();
    let show_invite = args.show_invite;
    let mut opts: ServeOptions = args.into();
    let guard = acquire_hub_runtime(&opts.data_dir)?;
    opts.token = resolve_or_create_auth_token(&opts.data_dir, opts.token.take(), opts.allow_open)?;
    let tls = prepare_tls(&mut opts)?;
    let scheme = if tls.is_some() { "https" } else { "http" };
    let url = public_url
        .map(|url| normalize_public_url(&url, scheme))
        .transpose()?
        .unwrap_or_else(|| default_public_url(scheme, opts.port));
    let hub_invite = HubInvite {
        server_url: url.clone(),
        server_token: opts.token.clone(),
        tls_ca_pem: tls.and_then(|identity| identity.public_ca_pem),
        relay: None,
    };
    let encoded = encode_hub_invite(&hub_invite)?;

    println!("FeanorFS hub ready at {url}");
    if std::io::stdout().is_terminal() || show_invite {
        println!("\nOn the first computer:");
        println!("  feanorfs start {encoded} /path/to/project");
        copy_to_clipboard(&encoded);
        println!("\nSecure hub invite copied to clipboard. Treat it like a password.");
    } else {
        println!(
            "Hub invite hidden because stdout is redirected; rerun with --show-invite to expose it."
        );
    }
    run_http_server_guarded(opts, guard).await
}

fn run_serve_command(command: ServeCommand) -> anyhow::Result<()> {
    match command {
        ServeCommand::Recovery { action } => match action {
            RecoveryAction::Export {
                path,
                data_dir,
                replace,
            } => {
                let passphrase = read_new_recovery_passphrase()?;
                let result = export_recovery_bundle(&data_dir, &path, &passphrase, replace)?;
                println!("Encrypted hub recovery bundle saved to {}", path.display());
                println!("  Hub CA fingerprint: {}", result.public_ca_fingerprint);
                println!("Keep this bundle and its passphrase in separate secure locations.");
                Ok(())
            }
            RecoveryAction::Import {
                path,
                data_dir,
                replace,
            } => {
                let passphrase =
                    Zeroizing::new(rpassword::prompt_password("Recovery bundle passphrase: ")?);
                let result = import_recovery_bundle(&data_dir, &path, &passphrase, replace)?;
                println!("Hub identity restored in {}", data_dir.display());
                println!("  Hub CA fingerprint: {}", result.public_ca_fingerprint);
                if result.resumed {
                    println!("  Completed a previously interrupted import.");
                }
                if result.replaced_existing_identity {
                    println!("  Replaced the existing hub identity with the recovered identity.");
                }
                println!(
                    "Restart the hub at its previous URL; existing clients retain its trust and credentials."
                );
                Ok(())
            }
            RecoveryAction::Rotate {
                path,
                data_dir,
                replace,
            } => {
                let passphrase = read_new_recovery_passphrase()?;
                let result = rotate_hub_identity(&data_dir, &path, &passphrase, replace)?;
                println!("Hub identity rotated in {}", data_dir.display());
                if let Some(previous) = result.previous_public_ca_fingerprint {
                    println!("  Previous CA fingerprint: {previous}");
                }
                println!(
                    "  Replacement CA fingerprint: {}",
                    result.public_ca_fingerprint
                );
                println!(
                    "  Encrypted recovery bundle: {}",
                    result.recovery_bundle.display()
                );
                if result.resumed {
                    println!("  Completed a previously interrupted rotation.");
                }
                println!("Restart the hub to emit its replacement fnh1 invite.");
                println!(
                    "On every existing client, run `feanorfs start fnh1-… <folder>` to authenticate the replacement identity without changing its E2EE key."
                );
                Ok(())
            }
        },
    }
}

fn read_new_recovery_passphrase() -> anyhow::Result<Zeroizing<String>> {
    let passphrase = Zeroizing::new(rpassword::prompt_password(
        "New recovery passphrase (12+ characters): ",
    )?);
    let confirmation = Zeroizing::new(rpassword::prompt_password("Confirm passphrase: ")?);
    if passphrase.as_str() != confirmation.as_str() {
        anyhow::bail!("recovery passphrases do not match");
    }
    Ok(passphrase)
}

fn normalize_public_url(url: &str, scheme: &str) -> anyhow::Result<String> {
    let normalized = if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else {
        format!("{scheme}://{url}")
    };
    let parsed = reqwest::Url::parse(&normalized)?;
    if parsed.scheme() != scheme {
        anyhow::bail!("--public-url must use {scheme}:// for this server mode");
    }
    Ok(normalized.trim_end_matches('/').to_string())
}

fn default_public_url(scheme: &str, port: u16) -> String {
    let host = if_addrs::get_if_addrs()
        .ok()
        .and_then(|interfaces| {
            interfaces
                .into_iter()
                .find_map(|interface| match interface.ip() {
                    std::net::IpAddr::V4(ip) if !ip.is_loopback() => Some(ip.to_string()),
                    _ => None,
                })
        })
        .unwrap_or_else(|| "127.0.0.1".into());
    format!("{scheme}://{host}:{port}")
}

pub async fn run_gc_cli(args: ServeCli) -> anyhow::Result<()> {
    let stats = run_gc(&ServeOptions {
        data_dir: args.data_dir,
        gc_grace_minutes: args.gc_grace_minutes,
        tombstone_retention_days: args.tombstone_retention_days,
        snapshot_retention_days: args.snapshot_retention_days,
        snapshot_keep_last: args.snapshot_keep_last,
        ..ServeOptions::default()
    })
    .await?;
    println!(
        "GC complete: {} blobs deleted ({} bytes), {} tombstones purged",
        stats.blobs_deleted, stats.bytes_freed, stats.tombstones_purged
    );
    Ok(())
}
