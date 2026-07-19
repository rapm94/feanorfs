use anyhow::Context as _;
use feanorfs_client::{
    conflicts, do_sync, encode_invite, hub::LocalHub, load_global_config, save_config_secure,
    save_global_config_secure, validate_e2ee_key, Config, GlobalConfig, WorkspaceInvite,
    LOCAL_HUB_URL,
};
use std::fs::OpenOptions;
use std::io::{IsTerminal as _, Write as _};
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

    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
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
        .with(stderr_layer)
        .with(file_layer)
        .try_init();

    Ok(())
}

fn discover_server_mdns(timeout: Duration) -> anyhow::Result<String> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};

    let daemon =
        ServiceDaemon::new().map_err(|e| anyhow::anyhow!("Failed to start mDNS daemon: {e}"))?;
    let receiver = daemon
        .browse("_feanorfs._tcp.local.")
        .map_err(|e| anyhow::anyhow!("Failed to browse mDNS: {e}"))?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or_default();
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(ip) = info.addresses.iter().next() {
                    let scheme = info.get_property_val_str("scheme").unwrap_or("http");
                    if scheme == "https" && info.get_property_val_str("ca").is_some() {
                        let fingerprint = info.get_property_val_str("ca").unwrap_or("unknown");
                        let _ = daemon.shutdown();
                        anyhow::bail!(
                            "Found a secure FeanorFS hub (CA {fingerprint}), but mDNS cannot authenticate it. \
                             Paste the fnh1-… hub invite printed by `feanorfs serve`."
                        );
                    }
                    let url = format!("{}://{}:{}", scheme, ip, info.port);
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
        Some(u) if u.starts_with("http://") || u.starts_with("https://") => Ok(u),
        Some(u) => Ok(format!("https://{u}")),
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

fn resolve_connection_token(explicit: Option<String>, local_hub: bool) -> Option<String> {
    if local_hub {
        explicit
    } else {
        resolve_server_password(explicit)
    }
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
        format!("{head}...{tail}")
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
    let stdout = std::io::stdout();
    output_json_to(stdout.lock(), value)
}

fn output_json_to<T: serde::Serialize>(
    mut writer: impl std::io::Write,
    value: &T,
) -> anyhow::Result<()> {
    if let Err(error) = serde_json::to_writer_pretty(&mut writer, value) {
        if error.io_error_kind() == Some(std::io::ErrorKind::BrokenPipe) {
            return Ok(());
        }
        return Err(error.into());
    }
    match writeln!(writer) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn service_identity(programs: &[&Path]) -> anyhow::Result<String> {
    let identities = programs
        .iter()
        .map(|program| {
            let canonical = program
                .canonicalize()
                .with_context(|| format!("resolve service executable {}", program.display()))?;
            let bytes = std::fs::read(&canonical)
                .with_context(|| format!("read service executable {}", canonical.display()))?;
            Ok((
                canonical.to_string_lossy().into_owned(),
                blake3::hash(&bytes).to_hex().to_string(),
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    serde_json::to_string(&identities).context("encode service executable identity")
}

pub(crate) fn service_identity_matches(marker: &Path, programs: &[&Path]) -> bool {
    let Ok(installed) = std::fs::read_to_string(marker) else {
        return false;
    };
    service_identity(programs).is_ok_and(|current| installed == current)
}

pub(crate) fn record_service_identity(marker: &Path, programs: &[&Path]) -> anyhow::Result<()> {
    let identity = service_identity(programs)?;
    let mut file = atomic_write_file::AtomicWriteFile::open(marker)
        .with_context(|| format!("create service identity marker {}", marker.display()))?;
    file.write_all(identity.as_bytes())
        .with_context(|| format!("write service identity marker {}", marker.display()))?;
    file.commit()
        .with_context(|| format!("commit service identity marker {}", marker.display()))
}

#[cfg(target_os = "windows")]
pub(crate) fn windows_task_running(
    task_path: &str,
    task_name: &str,
    full_task_name: &str,
) -> anyhow::Result<Option<bool>> {
    let query = std::process::Command::new("schtasks.exe")
        .args(["/Query", "/TN", full_task_name])
        .output()
        .context("query Windows scheduled task")?;
    if !query.status.success() {
        return Ok(None);
    }

    // `schtasks /Query` localizes its human-readable state. PowerShell exposes
    // the Task Scheduler enum as a stable integer, where Running is 4.
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$task = Get-ScheduledTask -TaskPath $env:FEANORFS_TASK_PATH -TaskName $env:FEANORFS_TASK_NAME -ErrorAction Stop; [Console]::Out.Write([int]$task.State)",
        ])
        .env("FEANORFS_TASK_PATH", task_path)
        .env("FEANORFS_TASK_NAME", task_name)
        .output()
        .context("read Windows scheduled task state")?;
    if !output.status.success() {
        anyhow::bail!(
            "read Windows scheduled task state: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(Some(output.stdout == b"4"))
}

#[cfg(target_os = "windows")]
pub(crate) fn windows_register_task(
    task_path: &str,
    task_name: &str,
    program: &str,
    arguments: &str,
    interactive: bool,
) -> anyhow::Result<()> {
    // schtasks.exe flattens the executable and arguments into /TR, which has
    // a 261-character limit even though Task Scheduler stores them as separate
    // fields. Register-ScheduledTask keeps those fields separate so ordinary
    // long install/workspace paths remain valid.
    const SCRIPT: &str = concat!(
        "$ErrorActionPreference='Stop';",
        "$taskPath=$env:FEANORFS_TASK_PATH;$taskName=$env:FEANORFS_TASK_NAME;",
        "$program=$env:FEANORFS_TASK_PROGRAM;$arguments=$env:FEANORFS_TASK_ARGUMENTS;",
        "$interactive=($env:FEANORFS_TASK_INTERACTIVE -eq 'true');",
        "$scheduler=New-Object -ComObject Schedule.Service;$scheduler.Connect();",
        "$folderName=$taskPath.Trim('\\');",
        "try{$null=$scheduler.GetFolder('\\'+$folderName)}catch{",
        "$null=$scheduler.GetFolder('\\').CreateFolder($folderName)};",
        "$user=[Security.Principal.WindowsIdentity]::GetCurrent().Name;",
        "if([string]::IsNullOrEmpty($arguments)){",
        "$action=New-ScheduledTaskAction -Execute $program}else{",
        "$action=New-ScheduledTaskAction -Execute $program -Argument $arguments};",
        "$trigger=New-ScheduledTaskTrigger -AtLogOn -User $user;",
        "$settings=New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries ",
        "-DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero);",
        "$params=@{TaskPath=$taskPath;TaskName=$taskName;Action=$action;",
        "Trigger=$trigger;Settings=$settings;Force=$true};",
        "if($interactive){$params.Principal=New-ScheduledTaskPrincipal ",
        "-UserId $user -LogonType Interactive -RunLevel Limited};",
        "Register-ScheduledTask @params | Out-Null"
    );
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .env("FEANORFS_TASK_PATH", task_path)
        .env("FEANORFS_TASK_NAME", task_name)
        .env("FEANORFS_TASK_PROGRAM", program)
        .env("FEANORFS_TASK_ARGUMENTS", arguments)
        .env(
            "FEANORFS_TASK_INTERACTIVE",
            if interactive { "true" } else { "false" },
        )
        .output()
        .context("register Windows scheduled task")?;
    if !output.status.success() {
        anyhow::bail!(
            "register Windows scheduled task: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub fn invite_from_config(config: &Config) -> Option<WorkspaceInvite> {
    Some(WorkspaceInvite {
        server_url: config.server_url.clone(),
        workspace_id: config.workspace_id.clone(),
        server_token: config.server_password.clone(),
        encryption_key: config.encryption_password.clone()?,
        tls_ca_pem: config.tls_ca_pem.clone(),
        hub_local: config.is_local_hub(),
        relay: config.relay.clone(),
        ignore_policy: None,
    })
}

#[derive(Debug, Clone)]
pub struct HubConnection {
    pub url: String,
    pub token: Option<String>,
    pub tls_ca_pem: Option<String>,
    pub relay: Option<feanorfs_common::RelayConfig>,
}

pub fn print_invite(invite: &WorkspaceInvite) -> anyhow::Result<()> {
    let portable = super::hub_service::portable_invite(invite.clone());
    let encoded = encode_invite(&portable)?;
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
        workspace,
        encryption_key,
        HubConnection {
            url: LOCAL_HUB_URL.to_string(),
            token: None,
            tls_ca_pem: None,
            relay: None,
        },
        false,
        true,
    )
    .await
}

pub async fn initialize_new_mirror(
    current_dir: &Path,
    workspace: String,
    encryption_key: Option<String>,
    hub: HubConnection,
    save_global: bool,
    local_hub: bool,
) -> anyhow::Result<()> {
    let HubConnection {
        url,
        token: server_token,
        tls_ca_pem,
        relay,
    } = hub;
    let hub_local = local_hub || url == LOCAL_HUB_URL;
    let srv_pass = resolve_connection_token(server_token, hub_local);
    let server_url = if hub_local {
        LOCAL_HUB_URL.to_string()
    } else {
        url.clone()
    };

    let (e2ee_key, was_generated) = match encryption_key {
        Some(key) => (key, false),
        None => (feanorfs_common::generate_password()?, true),
    };
    validate_e2ee_key(&e2ee_key, 3)?;

    if save_global && !hub_local {
        let global = GlobalConfig {
            server_url: server_url.clone(),
            server_password: srv_pass.clone(),
            tls_ca_pem: tls_ca_pem.clone(),
            relay: relay.clone(),
        };
        save_global_config_secure(&global)?;
    }

    let config = Config {
        server_url: server_url.clone(),
        workspace_id: workspace.clone(),
        encryption_password: Some(e2ee_key.clone()),
        server_password: srv_pass.clone(),
        tls_ca_pem: tls_ca_pem.clone(),
        format_version: 3,
        hub_local,
        relay: relay.clone(),
    };
    save_config_secure(current_dir, &config)?;

    let _db = crate::open_client_db(current_dir).await?;

    if hub_local {
        let hub_dir = config.hub_data_dir(current_dir);
        LocalHub::open(hub_dir, srv_pass.clone()).await?;
    }

    println!("This folder is now mirrored to FeanorFS.");
    if hub_local {
        println!("  Hub:          embedded (local, in-process)");
    } else {
        println!("  Server:       {server_url}");
    }
    println!("  Workspace:    {workspace}");
    println!("  Encryption:   enabled (zero-knowledge)");
    if srv_pass.is_some() {
        println!("  Server auth:  enabled");
    }

    let invite = WorkspaceInvite {
        server_url: server_url.clone(),
        workspace_id: workspace.clone(),
        server_token: srv_pass.clone(),
        encryption_key: e2ee_key.clone(),
        tls_ca_pem,
        hub_local,
        relay,
        ignore_policy: feanorfs_client::join_preflight::read_ignore_policy(current_dir).ok(),
    };

    let reveal_secrets = std::io::stdout().is_terminal();
    if was_generated && reveal_secrets {
        println!("\nWorkspace encryption key: {e2ee_key}");
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
    } else if was_generated {
        println!(
            "Recovery key and invite hidden because output is redirected. \
             Run `feanorfs config --key` from this workspace to export them."
        );
    } else if hub_local {
        println!(
            "\nThis workspace uses an embedded local hub. Invites are not portable — \
             run `feanorfs serve --data-dir .feanorfs/hub-data` to share it on the network."
        );
    } else if reveal_secrets {
        print_invite(&invite)?;
    } else {
        println!(
            "Workspace invite hidden because output is redirected. \
             Run `feanorfs config --key` to export it."
        );
    }

    Ok(())
}

pub async fn link_existing_mirror(
    current_dir: &Path,
    workspace: String,
    encryption_key: String,
    hub: HubConnection,
    hub_local: bool,
    run_initial_sync: bool,
) -> anyhow::Result<()> {
    validate_e2ee_key(&encryption_key, 3)?;
    let HubConnection {
        url,
        token: server_token,
        tls_ca_pem,
        relay,
    } = hub;
    let hub_local = hub_local || url == LOCAL_HUB_URL;
    let srv_pass = resolve_connection_token(server_token, hub_local);
    let server_url = if hub_local {
        LOCAL_HUB_URL.to_string()
    } else {
        url.clone()
    };

    if !hub_local {
        let global = GlobalConfig {
            server_url: server_url.clone(),
            server_password: srv_pass.clone(),
            tls_ca_pem: tls_ca_pem.clone(),
            relay: relay.clone(),
        };
        save_global_config_secure(&global)?;
    }

    let config = Config {
        server_url: server_url.clone(),
        workspace_id: workspace.clone(),
        encryption_password: Some(encryption_key.clone()),
        server_password: srv_pass.clone(),
        tls_ca_pem,
        format_version: 3,
        hub_local,
        relay,
    };
    save_config_secure(current_dir, &config)?;

    let db = crate::open_client_db(current_dir).await?;

    if hub_local {
        LocalHub::open(config.hub_data_dir(current_dir), srv_pass.clone()).await?;
    }

    println!("Linked this folder to mirrored workspace '{workspace}'.");
    if hub_local {
        println!("  Hub:        embedded (local, in-process)");
    } else {
        println!("  Server:     {server_url}");
    }
    println!("  Encryption: enabled");
    if srv_pass.is_some() {
        println!("  Server auth: enabled");
    }

    let api = crate::open_api_client(current_dir, &config).await?;
    let local_files =
        feanorfs_client::local::scan_local_directory(current_dir, &db, Some(&encryption_key))
            .await?;
    let ctx = feanorfs_client::SyncCtx::from_config(&api, &db, current_dir, &config)?;
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
    accept_join: bool,
) -> anyhow::Result<()> {
    let invite = feanorfs_client::decode_invite(token)?;
    join_from_workspace_invite(current_dir, invite, run_initial_sync, accept_join).await
}

pub async fn join_from_workspace_invite(
    current_dir: &Path,
    invite: WorkspaceInvite,
    run_initial_sync: bool,
    accept_join: bool,
) -> anyhow::Result<()> {
    if invite.hub_local {
        anyhow::bail!(
            "This invite is for an embedded local hub and cannot be used on another machine. \
             Run `feanorfs serve` on the host and join with a remote invite, or copy the folder."
        );
    }
    let preview = feanorfs_client::preview_join(current_dir, &invite)
        .await
        .context("preview this folder against the encrypted mirror")?;
    if preview.destination_has_files()
        || preview.ignore_policy_differs
        || preview.large_files.count > 0
    {
        print_join_preflight(&preview);
    }
    if preview.needs_confirmation() && !accept_join {
        confirm_join_preflight()?;
    }

    feanorfs_client::join_preflight::apply_invited_ignore_policy(
        current_dir,
        invite.ignore_policy.as_deref(),
    )
    .await?;

    link_existing_mirror(
        current_dir,
        invite.workspace_id,
        invite.encryption_key,
        HubConnection {
            url: invite.server_url,
            token: invite.server_token,
            tls_ca_pem: invite.tls_ca_pem,
            relay: invite.relay,
        },
        invite.hub_local,
        run_initial_sync,
    )
    .await
}

fn print_join_preflight(preview: &feanorfs_client::JoinPreflight) {
    println!("\nBefore this folder is joined:");
    print_preflight_group("Local only (will upload)", &preview.local_only);
    print_preflight_group("Mirror only (will download)", &preview.remote_only);
    print_preflight_group("Already identical", &preview.same);
    print_preflight_group(
        "Same path, different content (needs attention)",
        &preview.conflicts,
    );
    if preview.ignore_policy_differs {
        println!(
            "  Ignore policy: differs; the encrypted mirror policy will replace this folder's .feanorfsignore"
        );
    } else if preview.ignore_policy_known {
        println!("  Ignore policy: matches the encrypted mirror");
    } else {
        println!("  Ignore policy: older invite; keeping this folder's current policy");
    }
    if preview.large_files.count > 0 {
        print_preflight_group(
            "Large files (authenticated encrypted chunks)",
            &preview.large_files,
        );
    }
    println!("No files or FeanorFS setup have been changed by this preview.");
}

fn print_preflight_group(label: &str, group: &feanorfs_client::JoinPathGroup) {
    println!("  {label}: {}", group.count);
    for path in &group.examples {
        println!("    {path}");
    }
    if group.count > group.examples.len() {
        println!("    … and {} more", group.count - group.examples.len());
    }
}

fn confirm_join_preflight() -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "Join needs confirmation and input is not an interactive terminal. Review the preview, then rerun with --accept-join."
        );
    }
    print!("Type JOIN to upload local-only files, download mirror-only files, and keep conflicts for review: ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if answer.trim() != "JOIN" {
        anyhow::bail!("Join canceled; no FeanorFS setup or workspace files were changed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        output_json_to, record_service_identity, resolve_connection_token,
        service_identity_matches, truncate_password_for_display,
    };

    struct ClosedPipe;

    impl std::io::Write for ClosedPipe {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn json_output_exits_cleanly_when_its_reader_closes() {
        output_json_to(ClosedPipe, &vec!["path"; 10_000]).unwrap();
    }

    #[test]
    fn service_identity_detects_same_path_binary_upgrades() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("feanorfs");
        let marker = dir.path().join("service-program");
        std::fs::write(&program, b"version one").unwrap();
        record_service_identity(&marker, &[&program]).unwrap();
        assert!(service_identity_matches(&marker, &[&program]));

        std::fs::write(&program, b"version two").unwrap();
        assert!(!service_identity_matches(&marker, &[&program]));
    }

    #[test]
    fn embedded_hub_does_not_inherit_an_unrelated_global_token() {
        assert_eq!(resolve_connection_token(None, true), None);
        assert_eq!(
            resolve_connection_token(Some("explicit-local-token".into()), true).as_deref(),
            Some("explicit-local-token")
        );
    }

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
