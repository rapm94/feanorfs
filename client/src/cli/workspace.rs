use anyhow::Context as _;
use clap::Subcommand;
use feanorfs_client::{
    list_recent_workspaces, load_config, load_global_config, save_global_config_secure, summary,
    unregister_workspace, ApiClient, GlobalConfig,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::serve::{run_serve, ServeCli};
use super::start::{run_start, StartOptions};
use super::util::{
    acquire_token, copy_to_clipboard, initialize_local_mirror, initialize_new_mirror,
    invite_from_config, join_from_invite, link_existing_mirror, output_json, print_invite,
    probe_server_auth, read_password_hidden, resolve_server_url, truncate_password_for_display,
    HubConnection,
};

#[derive(Subcommand)]
pub enum WorkspaceAction {
    /// Begin or resume: create, join, sync, and run automatically at login
    Start {
        /// Server URL, fnh1-… hub invite, fnp1/fnp2 pair capability, fnr1 invite, or folder
        target: Option<String>,
        /// Folder to mirror when TARGET is a server, pair code, or invite
        folder: Option<std::path::PathBuf>,
        /// Explicit workspace ID (advanced; generated securely when omitted)
        #[arg(short, long)]
        workspace: Option<String>,
        /// Workspace encryption key (manual link — requires --workspace)
        #[arg(long, requires = "workspace")]
        encryption_key: Option<String>,
        /// Server access token
        #[arg(long, visible_alias = "token")]
        server_token: Option<String>,
        /// Discover server on local network via mDNS
        #[arg(long)]
        lan: bool,
        /// Embedded local hub (no remote server)
        #[arg(long)]
        local: bool,
        /// Host a private encrypted hub on this computer
        #[arg(long, conflicts_with_all = ["local", "lan", "server_token"])]
        host: bool,
        /// Reach this private hub through an opaque HTTPS/WSS relay
        #[arg(long, env = "FEANORFS_RELAY")]
        relay: Option<String>,
        /// Sync once and exit without entering watch mode
        #[arg(long, conflicts_with = "foreground")]
        no_watch: bool,
        /// Keep sync attached to this terminal instead of installing a background service
        #[arg(long)]
        foreground: bool,
    },
    /// Stop mirroring a folder while keeping its files and encrypted setup
    Stop {
        /// Workspace folder (default: current directory)
        folder: Option<std::path::PathBuf>,
    },
    /// Run the secure sync hub
    Serve(ServeCli),
    /// Manage automatic background sync for this workspace
    Service {
        #[command(subcommand)]
        action: super::service::ServiceAction,
    },
    /// Pair another computer with a single-use capability
    Pair {
        /// Seconds before the pairing code expires (30–900)
        #[arg(long, default_value_t = 300)]
        expires: u64,
        /// Public HTTPS/WSS rendezvous for pairing across networks (advanced)
        #[arg(long, env = "FEANORFS_RELAY")]
        relay: Option<String>,
        /// Emit an ephemeral ready event for the bundled tray UI.
        #[arg(long, hide = true)]
        tray: bool,
    },
    /// Create or restore a passphrase-encrypted workspace recovery kit
    Recovery {
        #[command(subcommand)]
        action: super::recovery::RecoveryAction,
    },
    /// Show connection and workspace configuration
    Config {
        /// Show full encryption key and invite (copies to clipboard)
        #[arg(long)]
        key: bool,
    },
    /// Diagnose connection and configuration issues
    Doctor {
        /// Report aggregate local format adoption without workspace details or credentials
        #[arg(long)]
        migration_report: bool,
    },
    /// Summarize files that changed since you last opened this workspace
    Summary {
        /// Shell out to FEANORFS_SUMMARY_CMD to produce prose instead of listing paths
        #[arg(long)]
        summarize: bool,
        /// Do not save the current snapshot as the baseline for the next catch-up diff
        #[arg(long)]
        no_remember: bool,
    },
    /// Re-seal legacy blobs and upgrade to format v3 encrypted snapshots
    Migrate {
        #[arg(long)]
        rekey: bool,
    },
    /// Mirror this folder (legacy — prefer `feanorfs start`)
    #[command(alias = "init", hide = true)]
    Setup {
        #[arg(short, long, default_value = "default")]
        workspace: String,
        server_url: Option<String>,
        #[arg(long)]
        encryption_key: Option<String>,
        #[arg(long, visible_alias = "token")]
        server_token: Option<String>,
        #[arg(long)]
        lan: bool,
        #[arg(long)]
        local: bool,
    },
    /// Join via invite or manual flags (legacy — prefer `feanorfs start`)
    #[command(hide = true)]
    Join {
        invite: Option<String>,
        #[arg(long)]
        workspace: Option<String>,
        #[arg(long)]
        encryption_key: Option<String>,
        #[arg(long)]
        server_url: Option<String>,
        #[arg(long, visible_alias = "token")]
        server_token: Option<String>,
        #[arg(long)]
        lan: bool,
    },
    /// Link with explicit flags (legacy — prefer `feanorfs start --encryption-key …`)
    #[command(hide = true)]
    Attach {
        workspace: String,
        #[arg(long)]
        encryption_key: String,
        #[arg(long)]
        server_url: Option<String>,
        #[arg(long, visible_alias = "token")]
        server_token: Option<String>,
        #[arg(long)]
        lan: bool,
    },
    /// Cache server URL in ~/.feanorfs/global.json (legacy — prefer `feanorfs start <URL>`)
    #[command(hide = true)]
    Connect {
        url: Option<String>,
        #[arg(long = "token")]
        token: Option<String>,
        #[arg(long)]
        lan: bool,
    },
    /// Show E2EE key (legacy — prefer `feanorfs config --key`)
    #[command(hide = true)]
    ShowKey,
    /// List workspaces on the server (legacy)
    #[command(hide = true, aliases = ["list", "ls"])]
    Workspaces { server_url: Option<String> },
    /// NDJSON event stream for orchestrators
    #[command(hide = true)]
    Events,
    /// Stdio MCP tool server for orchestrators
    #[command(hide = true)]
    Mcp,
    /// Tray companion commands (status, pause, recent workspaces).
    #[command(hide = true)]
    Tray {
        #[command(subcommand)]
        action: super::tray::TrayAction,
    },
}

pub async fn run(current_dir: &Path, action: WorkspaceAction, json: bool) -> anyhow::Result<()> {
    match action {
        WorkspaceAction::Start {
            target,
            folder,
            workspace,
            encryption_key,
            server_token,
            lan,
            local,
            host,
            relay,
            no_watch,
            foreground,
        } => {
            run_start(
                current_dir,
                StartOptions {
                    target,
                    folder,
                    workspace,
                    encryption_key,
                    server_token,
                    lan,
                    local,
                    host,
                    relay,
                    no_watch,
                    foreground,
                    recovery_invite: None,
                    pair_code: None,
                },
            )
            .await
        }
        WorkspaceAction::Setup {
            workspace,
            server_url,
            encryption_key,
            server_token,
            lan,
            local,
        } => {
            run_setup(
                current_dir,
                workspace,
                server_url,
                encryption_key,
                server_token,
                lan,
                local,
            )
            .await
        }
        WorkspaceAction::Stop { folder } => run_stop(current_dir, folder.as_deref(), json),
        WorkspaceAction::Join {
            invite,
            workspace,
            encryption_key,
            server_token,
            server_url,
            lan,
        } => {
            if let Some(ref key) = encryption_key {
                let ws = workspace.ok_or_else(|| {
                    anyhow::anyhow!("--workspace is required with --encryption-key")
                })?;
                return run_attach(current_dir, ws, key.clone(), server_url, server_token, lan)
                    .await;
            }
            if let Some(token) = invite {
                return join_from_invite(current_dir, &token, true).await;
            }
            run_join_interactive(current_dir, lan).await
        }
        WorkspaceAction::Attach {
            workspace,
            encryption_key,
            server_url,
            server_token,
            lan,
        } => {
            run_attach(
                current_dir,
                workspace,
                encryption_key,
                server_url,
                server_token,
                lan,
            )
            .await
        }
        WorkspaceAction::Connect { url, token, lan } => run_connect(url, token, lan).await,
        WorkspaceAction::Serve(args) => run_serve(args, json).await,
        WorkspaceAction::Service { action } => super::service::run(current_dir, action, json).await,
        WorkspaceAction::Pair {
            expires,
            mut relay,
            tray,
        } => {
            if json {
                anyhow::bail!("`feanorfs pair` is interactive and does not support --json");
            }
            if !(30..=900).contains(&expires) {
                anyhow::bail!("--expires must be between 30 and 900 seconds");
            }
            let presentation = if tray {
                super::pair::PairPresentation::Tray
            } else {
                super::pair::PairPresentation::Human
            };
            if relay.is_none() {
                relay = std::env::var("FEANORFS_PAIR_RELAY").ok();
            }
            super::pair::offer(
                current_dir,
                std::time::Duration::from_secs(expires),
                presentation,
                relay.as_deref(),
            )
            .await
        }
        WorkspaceAction::Recovery { action } => {
            super::recovery::run(current_dir, action, json).await
        }
        WorkspaceAction::Config { key } => run_config(current_dir, key),
        WorkspaceAction::ShowKey => run_config(current_dir, true),
        WorkspaceAction::Doctor { migration_report } => {
            if migration_report {
                run_migration_report(current_dir, json)
            } else {
                run_doctor(current_dir, json).await
            }
        }
        WorkspaceAction::Workspaces { server_url } => {
            run_workspaces(current_dir, json, server_url).await
        }
        WorkspaceAction::Summary {
            summarize,
            no_remember,
        } => run_summary(current_dir, json, summarize, no_remember).await,
        WorkspaceAction::Migrate { rekey } => {
            feanorfs_client::migrate_workspace(current_dir, rekey).await
        }
        WorkspaceAction::Events => super::events::run_events(current_dir).await,
        WorkspaceAction::Mcp => super::mcp::run_mcp(current_dir).await,
        WorkspaceAction::Tray { action } => super::tray::run(current_dir, action, json).await,
    }
}

#[derive(Debug, Serialize)]
struct StopResult {
    workspace: String,
    mirroring: bool,
    tray_registered: bool,
    files_preserved: bool,
    setup_preserved: bool,
    hub_preserved: bool,
}

fn run_stop(current_dir: &Path, folder: Option<&Path>, json: bool) -> anyhow::Result<()> {
    let workspace = folder
        .unwrap_or(current_dir)
        .canonicalize()
        .with_context(|| {
            format!(
                "Workspace folder does not exist: {}",
                folder.unwrap_or(current_dir).display()
            )
        })?;
    load_config(&workspace).with_context(|| {
        format!(
            "{} is not a FeanorFS workspace; choose a mirrored folder",
            workspace.display()
        )
    })?;
    super::service::uninstall_for_workspace_stop(&workspace)?;
    unregister_workspace(&workspace)?;

    let result = StopResult {
        workspace: workspace.display().to_string(),
        mirroring: false,
        tray_registered: false,
        files_preserved: true,
        setup_preserved: true,
        hub_preserved: true,
    };
    if json {
        return output_json(&result);
    }

    println!("Stopped mirroring {}.", workspace.display());
    println!("Your files and encrypted FeanorFS setup were kept.");
    println!("Open this folder and run `feanorfs start` to resume.");
    println!("Remote encrypted snapshots and any private hub were left unchanged.");
    Ok(())
}

async fn run_setup(
    current_dir: &Path,
    workspace: String,
    server_url: Option<String>,
    encryption_key: Option<String>,
    server_token: Option<String>,
    lan: bool,
    local: bool,
) -> anyhow::Result<()> {
    if local {
        return initialize_local_mirror(current_dir, workspace, encryption_key).await;
    }
    let url = match server_url {
        Some(u) if u.starts_with("http://") || u.starts_with("https://") => u,
        Some(u) => format!("https://{u}"),
        None => resolve_server_url(None, lan)?,
    };
    let final_token = acquire_token(&url, server_token).await?;
    initialize_new_mirror(
        current_dir,
        workspace,
        encryption_key,
        HubConnection {
            url,
            token: final_token,
            tls_ca_pem: None,
            relay: None,
        },
        true,
        false,
    )
    .await
}

async fn run_attach(
    current_dir: &Path,
    workspace: String,
    encryption_key: String,
    server_url: Option<String>,
    server_token: Option<String>,
    lan: bool,
) -> anyhow::Result<()> {
    let url = match server_url {
        Some(u) if u.starts_with("http://") || u.starts_with("https://") => u,
        Some(u) => format!("https://{u}"),
        None => resolve_server_url(None, lan)?,
    };
    link_existing_mirror(
        current_dir,
        workspace,
        encryption_key,
        HubConnection {
            url,
            token: server_token,
            tls_ca_pem: None,
            relay: None,
        },
        false,
        false,
    )
    .await
}

async fn run_connect(url: Option<String>, token: Option<String>, lan: bool) -> anyhow::Result<()> {
    let server_url = resolve_server_url(url, lan)?;
    let final_token = match token {
        Some(t) => Some(t),
        None => match probe_server_auth(&server_url).await {
            Ok(true) => Some(read_password_hidden("Server requires a token: ")?),
            Ok(false) => None,
            Err(e) => {
                tracing::warn!(
                    "Server auth probe failed for {}: {:?}. Saving without token.",
                    server_url,
                    e
                );
                None
            }
        },
    };
    let global = GlobalConfig {
        server_url: server_url.clone(),
        server_password: final_token.clone(),
        tls_ca_pem: None,
        relay: None,
    };
    save_global_config_secure(&global)?;
    println!("Connected to FeanorFS server at {server_url}");
    if final_token.is_some() {
        println!("  Server token: saved");
    }
    println!("\nNow run: feanorfs start");
    Ok(())
}

async fn run_join_interactive(current_dir: &Path, lan: bool) -> anyhow::Result<()> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("No invite provided. Pass fnr1-… or run: feanorfs start fnr1-…");
    }
    if lan {
        let url = resolve_server_url(None, true)?;
        println!("Discovered server at {url}. Paste an fnr1-… invite from the host machine.");
    } else {
        println!("Paste the fnr1-… invite from the host machine.");
        println!("For LAN discovery, re-run with: feanorfs start --lan");
    }
    print!("Invite: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let token = line.trim();
    if token.is_empty() {
        anyhow::bail!("No invite provided.");
    }
    join_from_invite(current_dir, token, true).await
}

fn run_config(current_dir: &Path, show_key: bool) -> anyhow::Result<()> {
    if show_key {
        return run_show_key(current_dir);
    }
    match load_global_config() {
        Ok(g) => {
            println!("Global connection (~/.feanorfs/global.json):");
            println!("  Server:        {}", g.server_url);
            println!(
                "  Transport:     {}",
                transport_label(&g.server_url, g.tls_ca_pem.is_some())
            );
            println!(
                "  Server auth:   {}",
                if g.server_password.is_some() {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            if g.relay.is_some() {
                println!("  Reachability:  opaque relay (inner TLS)");
            }
        }
        Err(_) => {
            println!("Global connection: not configured");
        }
    }
    println!();
    match load_config(current_dir) {
        Ok(c) => {
            println!("Workspace (.feanorfs/config.json):");
            println!("  Server:        {}", c.server_url);
            println!(
                "  Transport:     {}",
                transport_label(&c.server_url, c.tls_ca_pem.is_some())
            );
            println!("  Workspace ID:  {}", c.workspace_id);
            let e2ee_status = if c.encryption_password.is_some() {
                "enabled"
            } else {
                "disabled"
            };
            println!("  E2EE:          {e2ee_status}");
            if let Some(ref p) = c.encryption_password {
                println!("  E2EE key:      {}", truncate_password_for_display(p));
            }
            println!(
                "  Server auth:   {}",
                if c.server_password.is_some() {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            if c.relay.is_some() {
                println!("  Reachability:  opaque relay (inner TLS)");
            }
            println!("\nFull key + invite: feanorfs config --key");
        }
        Err(_) => {
            println!("Workspace: not mirrored yet (run `feanorfs start` in this directory)");
        }
    }
    Ok(())
}

fn transport_label(server_url: &str, private_ca: bool) -> &'static str {
    if server_url.starts_with("https://") && private_ca {
        "HTTPS (private hub CA verified)"
    } else if server_url.starts_with("https://") {
        "HTTPS (system CA verified)"
    } else if server_url == feanorfs_client::LOCAL_HUB_URL {
        "in-process local hub"
    } else {
        "HTTP (not encrypted; use only behind TLS/VPN)"
    }
}

fn run_show_key(current_dir: &Path) -> anyhow::Result<()> {
    let config = load_config(current_dir)?;
    match config.encryption_password {
        Some(ref key) => {
            println!("{key}");
            copy_to_clipboard(key);
            eprintln!("\nCopied to clipboard.");
            if config.is_local_hub() {
                eprintln!(
                    "\nEmbedded local hub — invites are not portable. \
                     Run `feanorfs serve --data-dir .feanorfs/hub-data` to share on the network."
                );
            } else if let Some(invite) = invite_from_config(&config) {
                print_invite(&invite)?;
            } else {
                eprintln!("\nOn another machine:");
                eprintln!(
                    "  feanorfs start --workspace {} --encryption-key {}",
                    config.workspace_id, key
                );
            }
        }
        None => {
            println!("No encryption key set for this workspace.");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DoctorCheckStatus {
    Ok,
    Info,
    Warning,
    Failure,
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: &'static str,
    status: DoctorCheckStatus,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    action: Option<String>,
}

#[derive(Debug, Serialize)]
struct DoctorResult {
    ok: bool,
    checks: Vec<DoctorCheck>,
}

impl DoctorResult {
    fn new() -> Self {
        Self {
            ok: true,
            checks: Vec::new(),
        }
    }

    fn add(
        &mut self,
        name: &'static str,
        status: DoctorCheckStatus,
        message: impl Into<String>,
        action: Option<&str>,
    ) {
        if status == DoctorCheckStatus::Failure {
            self.ok = false;
        }
        self.checks.push(DoctorCheck {
            name,
            status,
            message: message.into(),
            action: action.map(str::to_owned),
        });
    }
}

fn doctor_label(name: &str) -> &str {
    match name {
        "global_config" => "Global connection",
        "workspace_config" => "Workspace config",
        "e2ee" => "Encryption",
        "workspace_format" => "Workspace format",
        "automatic_sync" => "Automatic sync",
        "tray_registration" => "System tray",
        "private_hub" => "Private hub",
        "relay" => "Opaque relay",
        "server" => "Mirror connection",
        "remote_workspace" => "Remote workspace",
        "local_state" => "Local sync state",
        _ => name,
    }
}

fn render_doctor(result: &DoctorResult) {
    println!("Running diagnostics...\n");
    for check in &result.checks {
        let prefix = match check.status {
            DoctorCheckStatus::Ok => "[OK]  ",
            DoctorCheckStatus::Info => "[INFO]",
            DoctorCheckStatus::Warning => "[WARN]",
            DoctorCheckStatus::Failure => "[FAIL]",
        };
        println!("{prefix} {}: {}", doctor_label(check.name), check.message);
        if let Some(action) = &check.action {
            println!("       Next: {action}");
        }
    }
    println!();
    if result.ok {
        println!("All essential checks passed.");
    } else {
        println!("FeanorFS needs attention. These checks did not change your working files.");
    }
}

fn global_config_is_present() -> bool {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .is_some_and(|home| home.join(".feanorfs/global.json").is_file())
}

fn legacy_format_version() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
struct WorkspaceFormatProbe {
    #[serde(default = "legacy_format_version")]
    format_version: u32,
}

#[derive(Debug, Serialize)]
struct MigrationReport {
    report_version: u32,
    workspaces_checked: usize,
    format_v1: usize,
    format_v2: usize,
    format_v3: usize,
    unsupported_format: usize,
    unreadable_or_missing: usize,
    recent_registry_readable: bool,
    legacy_xor_retirement_ready: bool,
}

impl Default for MigrationReport {
    fn default() -> Self {
        Self {
            report_version: 1,
            workspaces_checked: 0,
            format_v1: 0,
            format_v2: 0,
            format_v3: 0,
            unsupported_format: 0,
            unreadable_or_missing: 0,
            recent_registry_readable: false,
            legacy_xor_retirement_ready: false,
        }
    }
}

impl MigrationReport {
    fn observe(&mut self, workspace: &Path) {
        self.workspaces_checked += 1;
        let config_path = workspace.join(".feanorfs").join("config.json");
        let Ok(content) = std::fs::read_to_string(config_path) else {
            self.unreadable_or_missing += 1;
            return;
        };
        let Ok(probe) = serde_json::from_str::<WorkspaceFormatProbe>(&content) else {
            self.unreadable_or_missing += 1;
            return;
        };
        match probe.format_version {
            1 => self.format_v1 += 1,
            2 => self.format_v2 += 1,
            3 => self.format_v3 += 1,
            _ => self.unsupported_format += 1,
        }
    }

    fn finish(&mut self) {
        self.legacy_xor_retirement_ready = self.recent_registry_readable
            && self.workspaces_checked > 0
            && self.format_v1 == 0
            && self.unsupported_format == 0
            && self.unreadable_or_missing == 0;
    }
}

fn normalized_workspace_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn collect_migration_report_from(
    current_dir: &Path,
    recent: Option<&feanorfs_client::RecentWorkspacesResult>,
    recent_registry_readable: bool,
) -> MigrationReport {
    let mut workspaces = BTreeSet::new();
    if let Some(recent) = recent {
        workspaces.extend(
            recent
                .workspaces
                .iter()
                .map(|entry| normalized_workspace_path(Path::new(&entry.path))),
        );
    }
    if current_dir.join(".feanorfs/config.json").is_file() {
        workspaces.insert(normalized_workspace_path(current_dir));
    }

    let mut report = MigrationReport {
        recent_registry_readable,
        ..MigrationReport::default()
    };
    for workspace in workspaces {
        report.observe(&workspace);
    }
    report.finish();
    report
}

fn collect_migration_report(current_dir: &Path) -> MigrationReport {
    match list_recent_workspaces() {
        Ok(recent) => collect_migration_report_from(current_dir, Some(&recent), true),
        Err(_) => collect_migration_report_from(current_dir, None, false),
    }
}

fn render_migration_report(report: &MigrationReport) {
    println!("Migration report (aggregate only; no workspace details):\n");
    println!("Workspaces checked:          {}", report.workspaces_checked);
    println!("Format v1:                  {}", report.format_v1);
    println!("Format v2:                  {}", report.format_v2);
    println!("Format v3:                  {}", report.format_v3);
    println!("Unsupported format:         {}", report.unsupported_format);
    println!(
        "Unreadable or missing:      {}",
        report.unreadable_or_missing
    );
    println!(
        "Recent registry readable:  {}",
        if report.recent_registry_readable {
            "yes"
        } else {
            "no"
        }
    );
    println!();
    if report.legacy_xor_retirement_ready {
        println!("This local profile is ready to contribute legacy-XOR retirement evidence.");
    } else if report.workspaces_checked == 0 {
        println!("No configured workspace evidence was found on this profile.");
    } else {
        println!("This local profile is not ready for legacy-XOR retirement.");
        println!(
            "Run `feanorfs migrate` in legacy workspaces. Reconnect offline folders, or use Remove Unavailable Folders… in the tray for entries you intentionally removed, then rerun this report."
        );
    }
    println!(
        "Removing legacy decryption still requires aggregate field evidence across deployments."
    );
}

fn run_migration_report(current_dir: &Path, json: bool) -> anyhow::Result<()> {
    let report = collect_migration_report(current_dir);
    if json {
        output_json(&report)
    } else {
        render_migration_report(&report);
        Ok(())
    }
}

async fn run_doctor(current_dir: &Path, json: bool) -> anyhow::Result<()> {
    use super::service::BackgroundStatus;

    let mut result = DoctorResult::new();
    if global_config_is_present() {
        match load_global_config() {
            Ok(global) => result.add(
                "global_config",
                DoctorCheckStatus::Ok,
                format!("default mirror is {}", global.server_url),
                None,
            ),
            Err(error) => result.add(
                "global_config",
                DoctorCheckStatus::Failure,
                format!("saved connection settings could not be read ({error})"),
                Some("Run `feanorfs start <server-or-invite> <folder>` to repair the saved connection."),
            ),
        }
    } else {
        result.add(
            "global_config",
            DoctorCheckStatus::Info,
            "no default mirror is saved; workspace-specific settings can still work",
            None,
        );
    }

    let config = match load_config(current_dir) {
        Ok(config) => config,
        Err(error) => {
            let config_exists = current_dir.join(".feanorfs/config.json").is_file();
            result.add(
                "workspace_config",
                DoctorCheckStatus::Failure,
                if config_exists {
                    format!("saved workspace settings could not be read ({error})")
                } else {
                    "this folder is not being mirrored".into()
                },
                Some("Run `feanorfs start` in the folder. Existing files will be kept."),
            );
            if json {
                return output_json(&result);
            }
            render_doctor(&result);
            return Ok(());
        }
    };

    result.add(
        "workspace_config",
        DoctorCheckStatus::Ok,
        format!(
            "workspace {} is configured for {}",
            config.workspace_id, config.server_url
        ),
        None,
    );
    if config.encryption_password.is_some() {
        result.add(
            "e2ee",
            DoctorCheckStatus::Ok,
            "end-to-end encryption is enabled",
            None,
        );
    } else {
        result.add(
            "e2ee",
            DoctorCheckStatus::Failure,
            "no workspace encryption key is available; do not sync this folder",
            Some("Re-link the folder with its invite or explicit encryption key."),
        );
    }
    if config.format_version >= 3 {
        result.add(
            "workspace_format",
            DoctorCheckStatus::Ok,
            "encrypted Merkle snapshots are enabled",
            None,
        );
    } else {
        result.add(
            "workspace_format",
            DoctorCheckStatus::Warning,
            format!("legacy format {} is still in use", config.format_version),
            Some("Run `feanorfs migrate` before using an untrusted mirror."),
        );
    }

    match super::service::status_for_workspace(current_dir) {
        Ok(BackgroundStatus::Running) => result.add(
            "automatic_sync",
            DoctorCheckStatus::Ok,
            "running and configured to restart at login",
            None,
        ),
        Ok(BackgroundStatus::Stopped) => result.add(
            "automatic_sync",
            DoctorCheckStatus::Failure,
            "stopped; local files and encrypted setup are unchanged",
            Some("Run `feanorfs start` to resume automatic mirroring."),
        ),
        Ok(BackgroundStatus::NotInstalled) => result.add(
            "automatic_sync",
            DoctorCheckStatus::Failure,
            "not installed; one-shot syncs work, but changes will not follow you automatically",
            Some("Run `feanorfs start` to enable automatic mirroring."),
        ),
        Err(error) => result.add(
            "automatic_sync",
            DoctorCheckStatus::Failure,
            format!("status could not be read ({error})"),
            Some("Run `feanorfs start`; if this persists, check user service permissions."),
        ),
    }

    match list_recent_workspaces() {
        Ok(recent) => {
            let canonical = current_dir
                .canonicalize()
                .unwrap_or_else(|_| current_dir.to_path_buf())
                .to_string_lossy()
                .into_owned();
            if recent
                .workspaces
                .iter()
                .any(|workspace| workspace.path == canonical)
            {
                result.add(
                    "tray_registration",
                    DoctorCheckStatus::Ok,
                    "this folder is available in the workspace switcher",
                    None,
                );
            } else {
                result.add(
                    "tray_registration",
                    DoctorCheckStatus::Warning,
                    "this folder is not listed in the system tray",
                    Some("Run `feanorfs start` to register it again."),
                );
            }
        }
        Err(error) => result.add(
            "tray_registration",
            DoctorCheckStatus::Warning,
            format!("recent workspace state could not be read ({error})"),
            Some("Back up and repair `~/.feanorfs/recent.json`, then run `feanorfs start`."),
        ),
    }

    match super::hub_service::status_for_workspace(&config) {
        Ok(Some(super::hub_service::HubStatus::Running)) => result.add(
            "private_hub",
            DoctorCheckStatus::Ok,
            "running and configured to restart at login",
            None,
        ),
        Ok(Some(super::hub_service::HubStatus::Stopped)) => result.add(
            "private_hub",
            DoctorCheckStatus::Failure,
            "stopped; local files and encrypted hub data are unchanged",
            Some("Run `feanorfs start --host` to restart the private hub."),
        ),
        Ok(Some(super::hub_service::HubStatus::NotInstalled)) => result.add(
            "private_hub",
            DoctorCheckStatus::Failure,
            "not installed at login; other computers cannot rely on this host",
            Some("Run `feanorfs start --host` to restore the private hub service."),
        ),
        Ok(None) if config.is_local_hub() => result.add(
            "private_hub",
            DoctorCheckStatus::Info,
            "this workspace uses an embedded local-only hub",
            None,
        ),
        Ok(None) => result.add(
            "private_hub",
            DoctorCheckStatus::Info,
            "this computer does not own the workspace hub",
            None,
        ),
        Err(error) => result.add(
            "private_hub",
            DoctorCheckStatus::Failure,
            format!("lifecycle status could not be read ({error})"),
            Some("Run `feanorfs start --host`; existing encrypted hub data will be reused."),
        ),
    }

    if config.relay.is_some() {
        match probe_opaque_relay(current_dir, &config).await {
            Ok(()) => result.add(
                "relay",
                DoctorCheckStatus::Ok,
                "reachable with hub authentication protected inside inner TLS",
                None,
            ),
            Err(error) => result.add(
                "relay",
                DoctorCheckStatus::Failure,
                format!("the inner-TLS tunnel could not reach the hub ({error})"),
                Some("Check the relay service, then run `feanorfs start --relay <URL>` on the hub computer."),
            ),
        }
    }

    match crate::open_api_client(current_dir, &config).await {
        Ok(api) => match api.get_workspaces().await {
            Ok(workspaces) => {
                result.add(
                    "server",
                    DoctorCheckStatus::Ok,
                    "the encrypted mirror is reachable and authenticated",
                    None,
                );
                if workspaces.contains(&config.workspace_id) {
                    result.add(
                        "remote_workspace",
                        DoctorCheckStatus::Ok,
                        "the workspace is present on the mirror",
                        None,
                    );
                } else {
                    result.add(
                        "remote_workspace",
                        DoctorCheckStatus::Info,
                        "the workspace has not published legacy file rows; format-v3 heads may still be healthy",
                        Some("Run `feanorfs status` for the authoritative encrypted-tree comparison."),
                    );
                }
            }
            Err(error) => result.add(
                "server",
                DoctorCheckStatus::Failure,
                format!("the encrypted mirror could not be reached ({error}); local files were not changed"),
                Some("Check the network, then run `feanorfs start` to retry and repair lifecycle services."),
            ),
        },
        Err(error) => result.add(
            "server",
            DoctorCheckStatus::Failure,
            format!("secure connection settings could not be opened ({error}); local files were not changed"),
            Some("Re-link with the original invite so TLS trust and credentials can be restored."),
        ),
    }

    match crate::open_client_db(current_dir).await {
        Ok(_) => result.add(
            "local_state",
            DoctorCheckStatus::Ok,
            "local sync state is readable",
            None,
        ),
        Err(error) => result.add(
            "local_state",
            DoctorCheckStatus::Failure,
            format!(
                "local sync state could not be opened ({error}); working files were not changed"
            ),
            Some("Back up the folder and repair `.feanorfs` permissions before syncing."),
        ),
    }

    if json {
        output_json(&result)
    } else {
        render_doctor(&result);
        Ok(())
    }
}

async fn probe_opaque_relay(
    current_dir: &Path,
    config: &feanorfs_client::Config,
) -> anyhow::Result<()> {
    let relay = config
        .relay
        .as_ref()
        .context("workspace relay configuration is missing")?;
    feanorfs_agent_core::tunnel::validate_config(relay)?;
    let mut probe = config.clone();
    if super::hub_service::owns_workspace(config) {
        let invite = invite_from_config(config).context("workspace has no E2EE key")?;
        probe.server_url = super::hub_service::portable_invite(invite).server_url;
    }
    let api = feanorfs_client::open_relay_api_client(current_dir, &probe).await?;
    tokio::time::timeout(std::time::Duration::from_secs(6), api.get_workspaces())
        .await
        .context("opaque relay probe timed out")??;
    Ok(())
}

async fn run_workspaces(
    current_dir: &Path,
    json: bool,
    server_url: Option<String>,
) -> anyhow::Result<()> {
    let (url, srv_pass) = if let Some(u) = server_url {
        (u, None)
    } else {
        let config = load_config(current_dir)?;
        (config.server_url, config.server_password)
    };
    let api = ApiClient::new(&url, srv_pass.as_deref());
    if !json {
        println!("Querying workspaces from server at {url}...");
    }
    let workspaces = api.get_workspaces().await?;
    if json {
        output_json(&workspaces)?;
    } else if workspaces.is_empty() {
        println!("No active workspaces found on the server.");
    } else {
        println!("\nActive Workspaces:");
        for w in workspaces {
            println!("  * {w}");
        }
    }
    Ok(())
}

async fn run_summary(
    current_dir: &Path,
    json: bool,
    summarize: bool,
    no_remember: bool,
) -> anyhow::Result<()> {
    let password = load_config(current_dir)
        .ok()
        .and_then(|c| c.encryption_password);
    let db = crate::open_client_db(current_dir).await?;
    let result = summary::diff_since_last_session(current_dir, &db, password.as_deref()).await?;
    if !no_remember {
        summary::commit_session_marker(current_dir, &db, password.as_deref()).await?;
    }
    if json {
        output_json(&result)?;
    } else if summarize {
        let rendered = summary::render_via_summary_tool(&result)?;
        println!("{rendered}");
    } else {
        let rendered = summary::render_via_summary_tool(&result);
        match rendered {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("Summary tool error: {e}");
                output_json(&result)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod doctor_tests {
    use super::*;

    fn recent_entry(path: &Path, workspace_id: &str) -> feanorfs_client::RecentWorkspaceEntry {
        feanorfs_client::RecentWorkspaceEntry {
            path: path.to_string_lossy().into_owned(),
            workspace_id: workspace_id.into(),
            label: "private-label".into(),
        }
    }

    fn write_probe_config(workspace: &Path, json: &str) {
        let config_dir = workspace.join(".feanorfs");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.json"), json).unwrap();
    }

    #[test]
    fn failure_check_marks_doctor_result_unhealthy() {
        let mut result = DoctorResult::new();
        result.add(
            "automatic_sync",
            DoctorCheckStatus::Failure,
            "stopped; files are unchanged",
            Some("Run `feanorfs start`."),
        );

        assert!(!result.ok);
        assert_eq!(result.checks.len(), 1);
        assert_eq!(result.checks[0].status, DoctorCheckStatus::Failure);
    }

    #[test]
    fn doctor_json_uses_stable_machine_readable_statuses() {
        let mut result = DoctorResult::new();
        result.add(
            "e2ee",
            DoctorCheckStatus::Ok,
            "end-to-end encryption is enabled",
            None,
        );
        result.add(
            "workspace_format",
            DoctorCheckStatus::Warning,
            "legacy format",
            Some("Run `feanorfs migrate`."),
        );

        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["checks"][0]["status"], "ok");
        assert_eq!(json["checks"][1]["status"], "warning");
        assert_eq!(json["checks"][1]["action"], "Run `feanorfs migrate`.");
    }

    #[test]
    fn migration_report_deduplicates_workspaces_and_defaults_legacy_configs_to_v1() {
        let directory = tempfile::tempdir().unwrap();
        let current = directory.path().join("current");
        let v2 = directory.path().join("v2");
        let missing = directory.path().join("missing");
        write_probe_config(
            &current,
            r#"{"workspace_id":"secret-id","credential_ref":"secret-ref"}"#,
        );
        write_probe_config(&v2, r#"{"format_version":2,"server_password":"secret"}"#);
        let recent = feanorfs_client::RecentWorkspacesResult {
            active: Some(current.to_string_lossy().into_owned()),
            workspaces: vec![
                recent_entry(&current, "secret-id"),
                recent_entry(&current, "duplicate-secret-id"),
                recent_entry(&v2, "v2-secret-id"),
                recent_entry(&missing, "missing-secret-id"),
            ],
        };

        let report = collect_migration_report_from(&current, Some(&recent), true);

        assert_eq!(report.workspaces_checked, 3);
        assert_eq!(report.format_v1, 1);
        assert_eq!(report.format_v2, 1);
        assert_eq!(report.format_v3, 0);
        assert_eq!(report.unreadable_or_missing, 1);
        assert!(!report.legacy_xor_retirement_ready);
    }

    #[test]
    fn migration_report_json_contains_aggregate_evidence_only() {
        let directory = tempfile::tempdir().unwrap();
        let current = directory.path().join("private-workspace-name");
        write_probe_config(
            &current,
            r#"{
                "format_version": 3,
                "workspace_id": "private-workspace-id",
                "server_url": "https://private-host.example",
                "credential_ref": "fsc1-private-reference",
                "encryption_password": "private-encryption-key",
                "server_password": "private-server-token"
            }"#,
        );
        let recent = feanorfs_client::RecentWorkspacesResult {
            active: None,
            workspaces: vec![recent_entry(&current, "private-workspace-id")],
        };

        let report = collect_migration_report_from(&current, Some(&recent), true);
        let value = serde_json::to_value(&report).unwrap();
        let json = serde_json::to_string(&value).unwrap();

        assert!(report.legacy_xor_retirement_ready);
        assert_eq!(
            value
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            [
                "format_v1",
                "format_v2",
                "format_v3",
                "legacy_xor_retirement_ready",
                "recent_registry_readable",
                "report_version",
                "unreadable_or_missing",
                "unsupported_format",
                "workspaces_checked",
            ]
        );
        assert_eq!(value["report_version"], 1);
        for private_value in [
            "private-workspace-name",
            "private-workspace-id",
            "private-host.example",
            "fsc1-private-reference",
            "private-encryption-key",
            "private-server-token",
        ] {
            assert!(!json.contains(private_value));
        }
    }

    #[test]
    fn unreadable_recent_registry_never_claims_retirement_readiness() {
        let directory = tempfile::tempdir().unwrap();
        let current = directory.path().join("current");
        write_probe_config(&current, r#"{"format_version":3}"#);

        let report = collect_migration_report_from(&current, None, false);

        assert_eq!(report.format_v3, 1);
        assert!(!report.recent_registry_readable);
        assert!(!report.legacy_xor_retirement_ready);
    }
}
