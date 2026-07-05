use anyhow::Context as _;
use feanorfs_client::{
    conflicts, do_sync, encode_invite, hub::LocalHub, load_global_config, save_config,
    save_global_config, validate_e2ee_key, ApiClient, ClientDb, Config, GlobalConfig,
    WorkspaceInvite, LOCAL_HUB_URL,
};
use std::fs::OpenOptions;
use std::path::Path;
use std::time::Duration;
use tracing_subscriber::{fmt, prelude::*, EnvFilter, Registry};

pub fn setup_logging(current_dir: &Path) -> anyhow::Result<()> {
    let log_dir = current_dir.join(".feanorfs");
    let _ = std::fs::create_dir_all(&log_dir)
        .map_err(|e| eprintln!("Warning: could not create log directory: {e:?}"));

    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("feanorfs.log"))?;

    let log_file_clone = log_file.try_clone()?;

    let stdout_layer = fmt::layer()
        .with_writer(std::io::stdout)
        .with_target(false)
        .without_time()
        .with_filter(EnvFilter::new("warn"));

    let file_layer = fmt::layer()
        .with_writer(move || -> Box<dyn std::io::Write + Send> {
            match log_file_clone.try_clone() {
                Ok(f) => Box::new(f),
                Err(_) => Box::new(std::io::sink()),
            }
        })
        .with_target(true)
        .with_ansi(false)
        .with_filter(EnvFilter::new("debug"));

    let _ = Registry::default()
        .with(stdout_layer)
        .with(file_layer)
        .try_init();

    Ok(())
}

fn discover_server_mdns(timeout: Duration) -> anyhow::Result<String> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};

    let daemon =
        ServiceDaemon::new().map_err(|e| anyhow::anyhow!("Failed to start mDNS daemon: {}", e))?;
    let receiver = daemon
        .browse("_feanorfs._tcp.local.")
        .map_err(|e| anyhow::anyhow!("Failed to browse mDNS: {}", e))?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or_default();
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(ip) = info.addresses.iter().next() {
                    let url = format!("http://{}:{}", ip, info.port);
                    let _ = daemon.shutdown();
                    return Ok(url);
                }
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    let _ = daemon.shutdown();
    anyhow::bail!(
        "No FeanorFS server found on local network within {} seconds. \
         Specify URL explicitly: feanorfs start https://your-server:3030",
        timeout.as_secs()
    )
}

pub fn resolve_server_url(explicit: Option<String>, allow_lan: bool) -> anyhow::Result<String> {
    match explicit {
        Some(u) => Ok(u),
        None => match load_global_config() {
            Ok(g) => Ok(g.server_url),
            Err(_) => {
                if allow_lan {
                    println!("Searching for FeanorFS server on local network...");
                    discover_server_mdns(Duration::from_secs(3))
                } else {
                    anyhow::bail!(
                        "No server URL specified and no cached connection found.\n\
                         \n\
                         Examples:\n  \
                         feanorfs start https://your-server.com:3030\n  \
                         feanorfs start --lan\n  \
                         feanorfs start --local"
                    )
                }
            }
        },
    }
}

pub fn resolve_server_password(explicit: Option<String>) -> Option<String> {
    explicit.or_else(|| load_global_config().ok().and_then(|g| g.server_password))
}

fn try_clipboard_cmd(cmd: &str, args: &[&str], text: &str) -> Option<std::process::ExitStatus> {
    std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(text.as_bytes())?;
            }
            child.wait()
        })
        .ok()
}

pub fn copy_to_clipboard(text: &str) {
    let result = if cfg!(target_os = "macos") {
        try_clipboard_cmd("pbcopy", &[], text)
    } else if cfg!(target_os = "linux") {
        try_clipboard_cmd("xclip", &["-selection", "clipboard"], text)
            .or_else(|| try_clipboard_cmd("wl-copy", &[], text))
            .or_else(|| try_clipboard_cmd("xsel", &["--clipboard", "--input"], text))
    } else {
        None
    };
    let _ = result;
}

pub fn read_password_hidden(prompt: &str) -> anyhow::Result<String> {
    Ok(rpassword::prompt_password(prompt)?)
}

pub fn truncate_password_for_display(p: &str) -> String {
    let chars: Vec<char> = p.chars().collect();
    if chars.len() > 12 {
        let head: String = chars.iter().take(6).collect();
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{}...{}", head, tail)
    } else {
        p.to_string()
    }
}

pub async fn probe_server_auth(url: &str) -> anyhow::Result<bool> {
    if url == LOCAL_HUB_URL {
        return Ok(false);
    }
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{}/api/workspaces", url.trim_end_matches('/')))
        .send()
        .await
        .context("Failed to reach server")?;
    Ok(resp.status() == reqwest::StatusCode::UNAUTHORIZED)
}

pub fn output_json<T: serde::Serialize>(value: &T) -> anyhow::Result<()> {
    let s = serde_json::to_string_pretty(value)?;
    println!("{}", s);
    Ok(())
}

pub fn invite_from_config(config: &Config) -> Option<WorkspaceInvite> {
    Some(WorkspaceInvite {
        server_url: config.server_url.clone(),
        workspace_id: config.workspace_id.clone(),
        server_token: config.server_password.clone(),
        encryption_key: config.encryption_password.clone()?,
        hub_local: config.is_local_hub(),
    })
}

pub fn print_invite(invite: &WorkspaceInvite) -> anyhow::Result<()> {
    let encoded = encode_invite(invite)?;
    println!("\nInvite (one-line join on another machine):");
    println!("  feanorfs start {encoded}");
    copy_to_clipboard(&encoded);
    println!("Copied invite to clipboard.");
    Ok(())
}

pub async fn initialize_local_mirror(
    current_dir: &Path,
    workspace: String,
    encryption_key: Option<String>,
) -> anyhow::Result<()> {
    initialize_new_mirror(
        current_dir,
        LOCAL_HUB_URL.to_string(),
        workspace,
        encryption_key,
        None,
        false,
        true,
    )
    .await
}

pub async fn initialize_new_mirror(
    current_dir: &Path,
    url: String,
    workspace: String,
    encryption_key: Option<String>,
    server_token: Option<String>,
    save_global: bool,
    local_hub: bool,
) -> anyhow::Result<()> {
    let srv_pass = resolve_server_password(server_token);
    let hub_local = local_hub || url == LOCAL_HUB_URL;
    let server_url = if hub_local {
        LOCAL_HUB_URL.to_string()
    } else {
        url.clone()
    };

    if save_global && !hub_local {
        let global = GlobalConfig {
            server_url: server_url.clone(),
            server_password: srv_pass.clone(),
        };
        save_global_config(&global)?;
    }

    let (e2ee_key, was_generated) = match encryption_key {
        Some(k) => (k, false),
        None => (feanorfs_common::generate_password()?, true),
    };

    let config = Config {
        server_url: server_url.clone(),
        workspace_id: workspace.clone(),
        encryption_password: Some(e2ee_key.clone()),
        server_password: srv_pass.clone(),
        format_version: 2,
        hub_local,
    };
    if let Err(e) = validate_e2ee_key(&e2ee_key, 2) {
        if !was_generated {
            return Err(e);
        }
    }
    save_config(current_dir, &config)?;

    if hub_local {
        let hub_dir = config.hub_data_dir(current_dir);
        LocalHub::open(hub_dir, srv_pass.clone()).await?;
    }

    let _db = ClientDb::new(current_dir.join(".feanorfs")).await?;

    println!("This folder is now mirrored to FeanorFS.");
    if hub_local {
        println!("  Hub:          embedded (local, in-process)");
    } else {
        println!("  Server:       {}", server_url);
    }
    println!("  Workspace:    {}", workspace);
    println!("  Encryption:   enabled (zero-knowledge)");
    if srv_pass.is_some() {
        println!("  Server auth:  enabled");
    }

    let invite = WorkspaceInvite {
        server_url: server_url.clone(),
        workspace_id: workspace.clone(),
        server_token: srv_pass.clone(),
        encryption_key: e2ee_key.clone(),
        hub_local,
    };

    if was_generated {
        println!("\nWorkspace encryption key: {}", e2ee_key);
        copy_to_clipboard(&e2ee_key);
        println!("Copied encryption key to clipboard.");
        if hub_local {
            println!(
                "\nThis workspace uses an embedded local hub. Invites are not portable — \
                 run `feanorfs serve --data-dir .feanorfs/hub-data` to share it on the network."
            );
        } else {
            print_invite(&invite)?;
        }
        println!(
            "This key encrypts your files. The server can never read them. \
             Store it — without it your files are unrecoverable."
        );
    } else if hub_local {
        println!(
            "\nThis workspace uses an embedded local hub. Invites are not portable — \
             run `feanorfs serve --data-dir .feanorfs/hub-data` to share it on the network."
        );
    } else {
        print_invite(&invite)?;
    }

    Ok(())
}

pub async fn link_existing_mirror(
    current_dir: &Path,
    url: String,
    workspace: String,
    encryption_key: String,
    server_token: Option<String>,
    hub_local: bool,
    run_initial_sync: bool,
) -> anyhow::Result<()> {
    let srv_pass = resolve_server_password(server_token);
    let hub_local = hub_local || url == LOCAL_HUB_URL;
    let server_url = if hub_local {
        LOCAL_HUB_URL.to_string()
    } else {
        url.clone()
    };

    if !hub_local {
        let global = GlobalConfig {
            server_url: server_url.clone(),
            server_password: srv_pass.clone(),
        };
        save_global_config(&global)?;
    }

    let config = Config {
        server_url: server_url.clone(),
        workspace_id: workspace.clone(),
        encryption_password: Some(encryption_key.clone()),
        server_password: srv_pass.clone(),
        format_version: 2,
        hub_local,
    };
    validate_e2ee_key(&encryption_key, 2)?;
    save_config(current_dir, &config)?;

    if hub_local {
        LocalHub::open(config.hub_data_dir(current_dir), srv_pass.clone()).await?;
    }

    let db = ClientDb::new(current_dir.join(".feanorfs")).await?;

    println!("Linked this folder to mirrored workspace '{}'.", workspace);
    if hub_local {
        println!("  Hub:        embedded (local, in-process)");
    } else {
        println!("  Server:     {}", server_url);
    }
    println!("  Encryption: enabled");
    if srv_pass.is_some() {
        println!("  Server auth: enabled");
    }

    let api = ApiClient::from_config(current_dir, &config).await?;
    let local_files =
        feanorfs_client::local::scan_local_directory(current_dir, &db, Some(&encryption_key))
            .await?;
    let ctx = feanorfs_client::SyncCtx::new(
        &api,
        &db,
        current_dir,
        &workspace,
        Some(&encryption_key),
        feanorfs_common::LegacyPolicy::Reject,
    );
    conflicts::seed_last_synced_from_server(&ctx, &local_files).await?;

    if run_initial_sync {
        println!("Syncing union of local files and workspace mirror...");
        do_sync(
            &api,
            &db,
            current_dir,
            &workspace,
            Some(&encryption_key),
            false,
        )
        .await?;
    } else {
        println!("\nRun 'feanorfs sync --no-watch' to download files from the mirror.");
    }

    Ok(())
}

pub async fn acquire_token(
    server_url: &str,
    arg: Option<String>,
) -> anyhow::Result<Option<String>> {
    if let Some(t) = arg {
        return Ok(Some(t));
    }
    match probe_server_auth(server_url).await {
        Ok(true) => Ok(Some(read_password_hidden("Server requires a token: ")?)),
        Ok(false) => Ok(None),
        Err(e) => {
            tracing::warn!(
                "Server auth probe failed for {server_url}: {e:?}. Continuing without token."
            );
            Ok(None)
        }
    }
}

pub async fn join_from_invite(
    current_dir: &Path,
    token: &str,
    run_initial_sync: bool,
) -> anyhow::Result<()> {
    let invite = feanorfs_client::decode_invite(token)?;
    if invite.hub_local {
        anyhow::bail!(
            "This invite is for an embedded local hub and cannot be used on another machine. \
             Run `feanorfs serve` on the host and join with a remote invite, or copy the folder."
        );
    }
    link_existing_mirror(
        current_dir,
        invite.server_url,
        invite.workspace_id,
        invite.encryption_key,
        invite.server_token,
        invite.hub_local,
        run_initial_sync,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::truncate_password_for_display;

    #[test]
    fn display_short_password_returns_unchanged() {
        assert_eq!(truncate_password_for_display("short"), "short");
    }

    #[test]
    fn display_long_ascii_password_is_truncated_with_ellipsis() {
        let pw = "0123456789abcdef0123456789abcdef";
        let display = truncate_password_for_display(pw);
        assert!(display.contains("..."));
        assert!(display.starts_with("012345"));
        assert!(display.ends_with("cdef"));
    }

    #[test]
    fn display_multibyte_password_does_not_panic() {
        let pw = "日本語のパスワード1234567890";
        let display = truncate_password_for_display(pw);
        assert!(!display.is_empty());
        assert!(display.contains("..."));
    }

    #[test]
    fn display_exactly_twelve_chars_returns_unchanged() {
        let pw = "012345678901";
        assert_eq!(truncate_password_for_display(pw), pw);
    }

    #[test]
    fn display_thirteen_chars_is_truncated() {
        let pw = "0123456789012";
        let display = truncate_password_for_display(pw);
        assert!(display.contains("..."));
    }
}
