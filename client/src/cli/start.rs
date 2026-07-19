use anyhow::Context as _;
use feanorfs_client::{
    do_sync, load_config, load_global_config, register_workspace, save_config_secure,
    save_global_config_secure, watch, ApiClient, Config, GlobalConfig,
};
use feanorfs_common::{
    decode_hub_invite, looks_like_hub_invite, looks_like_invite, HubInvite, WorkspaceInvite,
};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

use super::pair::{looks_like_pair_code, receive, PairCode};
use super::util::{
    acquire_token, initialize_local_mirror, initialize_new_mirror, join_from_invite,
    join_from_workspace_invite, link_existing_mirror, resolve_server_url, HubConnection,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    Background,
    Foreground,
    OneShot,
}

#[derive(Clone)]
pub struct StartOptions {
    pub target: Option<String>,
    pub folder: Option<PathBuf>,
    pub workspace: Option<String>,
    pub encryption_key: Option<String>,
    pub server_token: Option<String>,
    pub lan: bool,
    pub local: bool,
    pub host: bool,
    pub relay: Option<String>,
    pub no_watch: bool,
    pub foreground: bool,
    pub accept_join: bool,
    /// Decrypted in-process recovery input. This never comes from argv and is
    /// intentionally excluded from `Debug` output.
    pub recovery_invite: Option<WorkspaceInvite>,
    /// Parsed in-process pairing input supplied by the bundled tray through
    /// bounded stdin. The value zeroizes itself and never enters argv/env.
    pub pair_code: Option<PairCode>,
}

enum ParsedTarget {
    Invite(Zeroizing<String>),
    PairCode(PairCode),
    HubInvite(HubInvite),
    ServerUrl(String),
    Folder(PathBuf),
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Unambiguous bare `host:port` setup URLs without a scheme.
///
/// A Windows drive path also contains a colon, and relative folders may do so
/// on Unix. Keep those as folders. Custom single-label hostnames remain
/// available through an explicit `https://` URL.
fn looks_like_server_host(s: &str) -> bool {
    if s.chars()
        .any(|character| matches!(character, '/' | '\\' | ' '))
        || looks_like_invite(s)
        || looks_like_hub_invite(s)
    {
        return false;
    }

    let Ok(url) = reqwest::Url::parse(&format!("https://{s}")) else {
        return false;
    };
    if url.port().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }

    url.host_str().is_some_and(|host| {
        let ip_host = host.trim_start_matches('[').trim_end_matches(']');
        host == "localhost" || ip_host.parse::<std::net::IpAddr>().is_ok() || host.contains('.')
    })
}

fn normalize_server_url(s: &str) -> String {
    if looks_like_url(s) {
        s.to_string()
    } else {
        format!("https://{s}")
    }
}

fn new_workspace_id(explicit: Option<&str>) -> anyhow::Result<String> {
    if let Some(explicit) = explicit {
        if explicit.trim().is_empty() {
            anyhow::bail!("--workspace cannot be empty");
        }
        return Ok(explicit.to_string());
    }

    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).context("generate private workspace ID")?;
    let mut id = String::with_capacity(37);
    id.push_str("fsw1-");
    for byte in random {
        use std::fmt::Write as _;
        write!(id, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(id)
}

fn parse_target(raw: &str) -> anyhow::Result<ParsedTarget> {
    if looks_like_invite(raw) {
        return Ok(ParsedTarget::Invite(Zeroizing::new(raw.to_string())));
    }
    if looks_like_pair_code(raw) {
        return Ok(ParsedTarget::PairCode(PairCode::parse(raw)?));
    }
    if looks_like_hub_invite(raw) {
        return Ok(ParsedTarget::HubInvite(decode_hub_invite(raw)?));
    }
    if looks_like_url(raw) || looks_like_server_host(raw) {
        return Ok(ParsedTarget::ServerUrl(normalize_server_url(raw)));
    }
    let expanded = if raw.starts_with('~') {
        let home = std::env::var("HOME").unwrap_or_else(|_| "~".into());
        raw.replacen('~', &home, 1)
    } else {
        raw.to_string()
    };
    Ok(ParsedTarget::Folder(PathBuf::from(expanded)))
}

pub(crate) async fn finish_sync_watch(
    work_dir: &Path,
    watch_mode: WatchMode,
) -> anyhow::Result<()> {
    let managed_was_running = if watch_mode == WatchMode::Background {
        super::service::stop_for_start(work_dir)?
    } else {
        false
    };
    let config = load_config(work_dir)?;
    if config.format_version < 3 {
        eprintln!("Note: run `feanorfs migrate` to upgrade this workspace to format v3.");
    }
    let db = crate::open_client_db(work_dir).await?;
    let api = crate::open_api_client(work_dir, &config).await?;

    println!("Running sync...");
    let sync_result = do_sync(
        &api,
        &db,
        work_dir,
        &config.workspace_id,
        config.encryption_password.as_deref(),
        false,
    )
    .await;
    let sync_result = match sync_result {
        Ok(result) => result,
        Err(error) => {
            if managed_was_running {
                if let Err(restart_error) = super::service::restore_after_failed_start(work_dir) {
                    eprintln!(
                        "Warning: automatic sync could not be restored after the failed start: {restart_error}"
                    );
                }
            }
            return Err(error);
        }
    };
    if sync_result.large_file_count > 0 {
        println!(
            "Large-file transport: {} file(s) used authenticated encrypted chunks.",
            sync_result.large_file_count
        );
        for path in &sync_result.large_file_examples {
            println!("  {path}");
        }
        println!(
            "Keep legitimate large files normally; add disposable artifacts to .feanorfsignore."
        );
    }

    // Migrate legacy protected-file credentials only after sync succeeds and before the
    // unattended service starts. Headless systems keep the private-file fallback.
    // Endpoint selection may have safely replaced a legacy numeric LAN URL
    // after its authenticated probe. Reload so credential protection does not
    // overwrite that migration with the stale pre-probe config.
    let config = load_config(work_dir)?;
    save_config_secure(work_dir, &config)?;
    if let Ok(global) = load_global_config() {
        if let Err(error) = save_global_config_secure(&global) {
            eprintln!("Warning: could not migrate cached server credentials: {error}");
        }
    }

    if let Err(e) = register_workspace(work_dir) {
        eprintln!("Warning: could not register workspace for tray: {e}");
    }

    match watch_mode {
        WatchMode::OneShot => {}
        WatchMode::Foreground => {
            watch::run_watch(
                &api,
                &db,
                work_dir,
                &config.workspace_id,
                config.encryption_password.as_deref(),
            )
            .await?;
        }
        WatchMode::Background => {
            // The supervised worker opens the same workspace state. Release
            // this process's handles before launchd/systemd starts it so the
            // first launch cannot lose a lock race and enter restart backoff.
            drop(api);
            drop(db);
            super::service::install_and_start(work_dir)?;
        }
    }
    Ok(())
}

fn watch_mode(opts: &StartOptions) -> WatchMode {
    if opts.no_watch {
        WatchMode::OneShot
    } else if opts.foreground {
        WatchMode::Foreground
    } else {
        WatchMode::Background
    }
}

async fn ensure_owned_private_hub_for_resume(
    work_dir: &Path,
    mut config: feanorfs_client::Config,
) -> anyhow::Result<bool> {
    let legacy_http = config.tls_ca_pem.is_none() && config.server_url.starts_with("http://");
    let hub =
        super::hub_service::ensure_private_hub(config.server_password.clone(), legacy_http).await?;
    let ca_changed = config.tls_ca_pem != hub.tls_ca_pem;
    let upgraded_transport = legacy_http || ca_changed;
    if upgraded_transport {
        config.server_url = hub.url;
    }
    config.server_password = hub.token.clone();
    config.tls_ca_pem = hub.tls_ca_pem.clone();
    save_config_secure(work_dir, &config)?;
    save_global_config_secure(&GlobalConfig {
        server_url: config.server_url,
        server_password: hub.token,
        tls_ca_pem: hub.tls_ca_pem,
        relay: config.relay,
    })?;
    Ok(upgraded_transport)
}

fn config_with_refreshed_hub(current: &Config, invite: HubInvite) -> anyhow::Result<Config> {
    if !invite.server_url.starts_with("https://") {
        anyhow::bail!(
            "Refreshing hub trust requires HTTPS so the replacement access token is never sent in plaintext."
        );
    }
    if invite.server_token.is_none() {
        anyhow::bail!(
            "The replacement hub invite has no access token. Use the authenticated fnh1 invite emitted by the hub after identity rotation."
        );
    }
    if current.encryption_password.is_none() {
        anyhow::bail!(
            "This workspace has no recoverable E2EE key. Restore its local credentials before refreshing hub trust."
        );
    }
    if current.format_version < 3 {
        anyhow::bail!(
            "Hub trust refresh requires format-v3 encrypted snapshots. Run `feanorfs migrate` before rotating the hub identity."
        );
    }

    let mut refreshed = current.clone();
    refreshed.server_url = invite.server_url;
    refreshed.server_password = invite.server_token;
    refreshed.tls_ca_pem = invite.tls_ca_pem;
    refreshed.relay = invite.relay;
    refreshed.hub_local = false;
    Ok(refreshed)
}

async fn refresh_hub_trust(work_dir: &Path, invite: HubInvite) -> anyhow::Result<()> {
    let current = load_config(work_dir)?;
    let refreshed = config_with_refreshed_hub(&current, invite)?;
    authenticate_refreshed_hub(&refreshed).await?;

    save_config_secure(work_dir, &refreshed)?;
    if let Err(error) = save_global_config_secure(&GlobalConfig {
        server_url: refreshed.server_url.clone(),
        server_password: refreshed.server_password.clone(),
        tls_ca_pem: refreshed.tls_ca_pem.clone(),
        relay: refreshed.relay.clone(),
    }) {
        eprintln!("Warning: could not update the default hub connection: {error}");
    }

    println!("Authenticated the replacement hub identity for this folder.");
    println!("  Workspace and E2EE key: preserved");
    println!("  Encrypted snapshots and local files: preserved");
    Ok(())
}

async fn authenticate_refreshed_hub(refreshed: &Config) -> anyhow::Result<()> {
    let candidate = ApiClient::new_with_tls(
        &refreshed.server_url,
        refreshed.server_password.as_deref(),
        refreshed.tls_ca_pem.as_deref(),
    )?;

    let replacement_head = candidate
        .get_head(&refreshed.workspace_id)
        .await
        .context(
            "Replacement hub identity could not be authenticated; existing connection settings were preserved",
        )?;
    if replacement_head.is_none() {
        anyhow::bail!(
            "The authenticated replacement hub does not contain this workspace; existing connection settings were preserved"
        );
    }
    Ok(())
}

pub async fn run_start(current_dir: &Path, mut opts: StartOptions) -> anyhow::Result<()> {
    let watch_mode = watch_mode(&opts);
    let mut folder = opts.folder.clone();
    let mut invite: Option<Zeroizing<String>> = None;
    let mut recovery_invite = opts.recovery_invite.take();
    let mut pair_code = opts.pair_code.take();
    let mut hub_invite: Option<HubInvite> = None;
    let mut server_url: Option<String> = None;

    if pair_code.is_some() && opts.target.is_some() {
        anyhow::bail!("pairing input cannot be combined with another start target");
    }
    if let Some(raw) = opts.target.take() {
        let raw = Zeroizing::new(raw);
        match parse_target(raw.as_str())? {
            ParsedTarget::Invite(s) => invite = Some(s),
            ParsedTarget::PairCode(s) => pair_code = Some(s),
            ParsedTarget::HubInvite(invite) => hub_invite = Some(invite),
            ParsedTarget::ServerUrl(u) => server_url = Some(u),
            ParsedTarget::Folder(p) => {
                if folder.is_some() {
                    anyhow::bail!(
                        "Specify one folder path. The second positional is only for a server, \
                         pairing code, or invite target."
                    );
                }
                folder = Some(p);
            }
        }
    }

    if opts.host
        && (invite.is_some()
            || recovery_invite.is_some()
            || pair_code.is_some()
            || hub_invite.is_some()
            || server_url.is_some())
    {
        anyhow::bail!("--host cannot be combined with a server, pairing code, or invite");
    }
    if opts.relay.is_some()
        && (invite.is_some()
            || recovery_invite.is_some()
            || pair_code.is_some()
            || hub_invite.is_some()
            || server_url.is_some()
            || opts.local
            || opts.lan)
    {
        anyhow::bail!(
            "--relay configures this computer's private hub and cannot be combined with another hub or invite"
        );
    }

    let work_dir = folder.clone().unwrap_or_else(|| current_dir.to_path_buf());
    if work_dir != current_dir {
        std::fs::create_dir_all(&work_dir)
            .with_context(|| format!("create workspace folder {}", work_dir.display()))?;
        std::env::set_current_dir(&work_dir)?;
    }
    let work_dir = std::env::current_dir()?;

    let has_config = load_config(&work_dir).is_ok();
    if has_config && opts.host && opts.encryption_key.is_none() {
        let config = load_config(&work_dir)?;
        if !super::hub_service::owns_workspace(&config) {
            anyhow::bail!(
                "This workspace belongs to a different hub. Omit --host to resume it, or run --host in an unconfigured folder to create a new private workspace."
            );
        }
        let upgraded_transport = ensure_owned_private_hub_for_resume(&work_dir, config).await?;
        println!("Private encrypted hub is running automatically on this computer.");
        if upgraded_transport {
            println!("Upgraded this workspace from local HTTP to authenticated native TLS.");
        }
        if let Some(relay) = opts.relay.as_deref() {
            let config = load_config(&work_dir)?;
            super::hub_service::configure_relay_for_pairing(&work_dir, &config, relay).await?;
            println!("Private encrypted hub is available through the opaque relay.");
        }
        return finish_sync_watch(&work_dir, watch_mode).await;
    }
    if has_config && opts.relay.is_some() && opts.encryption_key.is_none() {
        let config = load_config(&work_dir)?;
        if !super::hub_service::owns_workspace(&config) {
            anyhow::bail!(
                "This workspace belongs to a different hub. --relay can only expose the private hub owned by this computer."
            );
        }
        super::hub_service::ensure_private_hub(config.server_password.clone(), false).await?;
        super::hub_service::configure_relay_for_pairing(
            &work_dir,
            &config,
            opts.relay.as_deref().expect("checked relay"),
        )
        .await?;
        println!("Private encrypted hub is available through the opaque relay.");
        return finish_sync_watch(&work_dir, watch_mode).await;
    }
    let is_relink = opts.encryption_key.is_some()
        || invite.is_some()
        || recovery_invite.is_some()
        || pair_code.is_some()
        || hub_invite.is_some();
    let wants_setup = server_url.is_some()
        || opts.local
        || opts.host
        || opts.encryption_key.is_some()
        || opts.lan
        || invite.is_some()
        || recovery_invite.is_some()
        || pair_code.is_some()
        || hub_invite.is_some()
        || opts.workspace.is_some()
        || (!has_config && load_global_config().is_ok());

    if has_config && wants_setup && !is_relink {
        anyhow::bail!(
            "Workspace already configured in this folder. Use `feanorfs sync` to resume, \
             or pass an invite / `--encryption-key` to re-link."
        );
    }

    if let Some(invite) = recovery_invite.take() {
        join_from_workspace_invite(&work_dir, invite, false, opts.accept_join).await?;
        return finish_sync_watch(&work_dir, watch_mode).await;
    }

    if let Some(token) = invite {
        join_from_invite(&work_dir, &token, false, opts.accept_join).await?;
        return finish_sync_watch(&work_dir, watch_mode).await;
    }

    if let Some(code) = pair_code {
        println!("Finding the other computer…");
        let token = receive(&code, std::time::Duration::from_secs(20)).await?;
        join_from_invite(&work_dir, &token, false, opts.accept_join).await?;
        return finish_sync_watch(&work_dir, watch_mode).await;
    }

    if let Some(invite) = hub_invite {
        if has_config {
            refresh_hub_trust(&work_dir, invite).await?;
            return finish_sync_watch(&work_dir, watch_mode).await;
        }
        initialize_new_mirror(
            &work_dir,
            new_workspace_id(opts.workspace.as_deref())?,
            opts.encryption_key.clone(),
            HubConnection {
                url: invite.server_url,
                token: invite.server_token,
                tls_ca_pem: invite.tls_ca_pem,
                relay: invite.relay,
            },
            true,
            false,
        )
        .await?;
        return finish_sync_watch(&work_dir, watch_mode).await;
    }

    if opts.local {
        initialize_local_mirror(
            &work_dir,
            new_workspace_id(opts.workspace.as_deref())?,
            opts.encryption_key.clone(),
        )
        .await?;
        return finish_sync_watch(&work_dir, watch_mode).await;
    }

    if opts.host || opts.relay.is_some() {
        let hub = super::hub_service::ensure_private_hub(None, false).await?;
        println!("Private encrypted hub is running automatically on this computer.");
        initialize_new_mirror(
            &work_dir,
            new_workspace_id(opts.workspace.as_deref())?,
            opts.encryption_key.clone(),
            hub,
            true,
            false,
        )
        .await?;
        if let Some(relay) = opts.relay.as_deref() {
            let config = load_config(&work_dir)?;
            super::hub_service::configure_relay_for_pairing(&work_dir, &config, relay).await?;
            println!("Private encrypted hub is available through the opaque relay.");
        }
        return finish_sync_watch(&work_dir, watch_mode).await;
    }

    if let Some(url) = server_url {
        let token = acquire_token(&url, opts.server_token.clone()).await?;
        initialize_new_mirror(
            &work_dir,
            new_workspace_id(opts.workspace.as_deref())?,
            opts.encryption_key.clone(),
            HubConnection {
                url,
                token,
                tls_ca_pem: None,
                relay: None,
            },
            true,
            false,
        )
        .await?;
        return finish_sync_watch(&work_dir, watch_mode).await;
    }

    if let Some(ref key) = opts.encryption_key {
        let workspace = opts
            .workspace
            .clone()
            .context("--workspace is required with --encryption-key")?;
        let url = resolve_server_url(None, opts.lan)?;
        link_existing_mirror(
            &work_dir,
            workspace,
            key.clone(),
            HubConnection {
                url,
                token: opts.server_token.clone(),
                tls_ca_pem: None,
                relay: None,
            },
            false,
            false,
        )
        .await?;
        return finish_sync_watch(&work_dir, watch_mode).await;
    }

    if has_config {
        let config = load_config(&work_dir)?;
        if super::hub_service::owns_workspace(&config) {
            let upgraded_transport = ensure_owned_private_hub_for_resume(&work_dir, config).await?;
            if upgraded_transport {
                println!("Updated this private hub to authenticated native TLS.");
            }
        }
        return finish_sync_watch(&work_dir, watch_mode).await;
    }

    if opts.lan {
        let url = resolve_server_url(None, true)?;
        let token = acquire_token(&url, opts.server_token.clone()).await?;
        initialize_new_mirror(
            &work_dir,
            new_workspace_id(opts.workspace.as_deref())?,
            None,
            HubConnection {
                url,
                token,
                tls_ca_pem: None,
                relay: None,
            },
            true,
            false,
        )
        .await?;
        return finish_sync_watch(&work_dir, watch_mode).await;
    }

    if let Ok(global) = load_global_config() {
        let url = global.server_url;
        let token = acquire_token(&url, opts.server_token.or(global.server_password)).await?;
        initialize_new_mirror(
            &work_dir,
            new_workspace_id(opts.workspace.as_deref())?,
            None,
            HubConnection {
                url,
                token,
                tls_ca_pem: global.tls_ca_pem,
                relay: global.relay,
            },
            false,
            false,
        )
        .await?;
        return finish_sync_watch(&work_dir, watch_mode).await;
    }

    let hub = super::hub_service::ensure_private_hub(None, false).await?;
    println!("Private encrypted hub is running automatically on this computer.");
    initialize_new_mirror(
        &work_dir,
        new_workspace_id(opts.workspace.as_deref())?,
        opts.encryption_key,
        hub,
        true,
        false,
    )
    .await?;
    finish_sync_watch(&work_dir, watch_mode).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_invite_target() {
        let t = "fnr1-deadbeef";
        assert!(matches!(parse_target(t).unwrap(), ParsedTarget::Invite(_)));
    }

    #[test]
    fn parse_url_target() {
        assert!(matches!(
            parse_target("https://x:3030").unwrap(),
            ParsedTarget::ServerUrl(_)
        ));
    }

    #[test]
    fn parse_pair_code_target() {
        assert!(matches!(
            parse_target("fnp1-2345-6789-ABCD-EFGH").unwrap(),
            ParsedTarget::PairCode(_)
        ));
    }

    #[test]
    fn parse_legacy_host_port() {
        match parse_target("127.0.0.1:3030").unwrap() {
            ParsedTarget::ServerUrl(u) => assert_eq!(u, "https://127.0.0.1:3030"),
            _ => panic!("expected server url"),
        }
        assert!(matches!(
            parse_target("localhost:3030").unwrap(),
            ParsedTarget::ServerUrl(url) if url == "https://localhost:3030"
        ));
        assert!(matches!(
            parse_target("hub.example:3030").unwrap(),
            ParsedTarget::ServerUrl(url) if url == "https://hub.example:3030"
        ));
        assert!(matches!(
            parse_target("[::1]:3030").unwrap(),
            ParsedTarget::ServerUrl(url) if url == "https://[::1]:3030"
        ));
    }

    #[test]
    fn parse_secure_hub_invite_target() {
        let invite = feanorfs_common::HubInvite {
            server_url: "https://127.0.0.1:3030".into(),
            server_token: Some("token".into()),
            tls_ca_pem: Some("public-ca".into()),
            relay: None,
        };
        let encoded = feanorfs_common::encode_hub_invite(&invite).unwrap();
        assert!(matches!(
            parse_target(&encoded).unwrap(),
            ParsedTarget::HubInvite(decoded) if decoded == invite
        ));
    }

    #[test]
    fn parse_folder_target() {
        assert!(matches!(
            parse_target("/tmp/ws").unwrap(),
            ParsedTarget::Folder(_)
        ));
    }

    #[test]
    fn parse_new_relative_folder_target() {
        assert!(matches!(
            parse_target("new-workspace").unwrap(),
            ParsedTarget::Folder(path) if path == Path::new("new-workspace")
        ));
    }

    #[test]
    fn windows_and_colon_folder_targets_are_not_misclassified_as_servers() {
        for folder in [
            r"C:\Users\Raul\project",
            "C:/Users/Raul/project",
            "localhost-project",
            "project:3030",
            "/tmp/project:3030",
        ] {
            assert!(matches!(
                parse_target(folder).unwrap(),
                ParsedTarget::Folder(path) if path == Path::new(folder)
            ));
        }
    }

    #[test]
    fn generated_workspace_ids_are_opaque_and_unique() {
        let first = new_workspace_id(None).unwrap();
        let second = new_workspace_id(None).unwrap();
        assert!(first.starts_with("fsw1-"));
        assert_eq!(first.len(), 37);
        assert_ne!(first, second);
        assert!(first[5..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn explicit_workspace_id_is_preserved() {
        assert_eq!(new_workspace_id(Some("team-app")).unwrap(), "team-app");
        assert!(new_workspace_id(Some(" ")).is_err());
    }

    #[test]
    fn refreshed_hub_preserves_workspace_identity_and_e2ee_key() {
        let current = Config {
            server_url: "https://old.example".into(),
            workspace_id: "fsw1-existing".into(),
            encryption_password: Some("a".repeat(64)),
            server_password: Some("old-token".into()),
            tls_ca_pem: Some("old-public-ca".into()),
            format_version: 3,
            hub_local: false,
            relay: None,
        };
        let refreshed = config_with_refreshed_hub(
            &current,
            HubInvite {
                server_url: "https://new.example".into(),
                server_token: Some("new-token".into()),
                tls_ca_pem: Some("new-public-ca".into()),
                relay: None,
            },
        )
        .unwrap();

        assert_eq!(refreshed.workspace_id, current.workspace_id);
        assert_eq!(refreshed.encryption_password, current.encryption_password);
        assert_eq!(refreshed.format_version, current.format_version);
        assert_eq!(refreshed.server_url, "https://new.example");
        assert_eq!(refreshed.server_password.as_deref(), Some("new-token"));
        assert_eq!(refreshed.tls_ca_pem.as_deref(), Some("new-public-ca"));
    }

    #[test]
    fn refreshed_hub_rejects_plaintext_or_unauthenticated_invites() {
        let current = Config {
            server_url: "https://old.example".into(),
            workspace_id: "fsw1-existing".into(),
            encryption_password: Some("a".repeat(64)),
            server_password: Some("old-token".into()),
            tls_ca_pem: Some("old-public-ca".into()),
            format_version: 3,
            hub_local: false,
            relay: None,
        };

        assert!(config_with_refreshed_hub(
            &current,
            HubInvite {
                server_url: "http://new.example".into(),
                server_token: Some("new-token".into()),
                tls_ca_pem: None,
                relay: None,
            },
        )
        .is_err());
        assert!(config_with_refreshed_hub(
            &current,
            HubInvite {
                server_url: "https://new.example".into(),
                server_token: None,
                tls_ca_pem: None,
                relay: None,
            },
        )
        .is_err());
    }

    #[tokio::test]
    async fn refreshed_hub_authenticates_the_existing_opaque_head() {
        let data = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let mut options = feanorfs_server::ServeOptions {
            data_dir: data.path().to_path_buf(),
            port,
            token: Some("replacement-token".into()),
            ..feanorfs_server::ServeOptions::default()
        };
        let identity = feanorfs_server::prepare_tls(&mut options).unwrap().unwrap();
        let ca = identity.public_ca_pem.unwrap();
        let server = tokio::spawn(feanorfs_server::run_http_server(options));
        let url = format!("https://127.0.0.1:{port}");
        let api = ApiClient::new_with_tls(&url, Some("replacement-token"), Some(&ca)).unwrap();

        let mut ready = false;
        for _ in 0..100 {
            if api.get_workspaces().await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(ready, "replacement TLS hub did not become ready");

        let workspace = "fsw1-existing";
        let snapshot = b"opaque snapshot";
        let snapshot_id = feanorfs_common::hash_bytes(snapshot);
        api.upload_object(workspace, &snapshot_id, snapshot.to_vec())
            .await
            .unwrap();
        api.upload_manifest(workspace, &snapshot_id, std::slice::from_ref(&snapshot_id))
            .await
            .unwrap();
        api.swap_head(workspace, None, &snapshot_id).await.unwrap();

        let refreshed = Config {
            server_url: url,
            workspace_id: workspace.into(),
            encryption_password: Some("a".repeat(64)),
            server_password: Some("replacement-token".into()),
            tls_ca_pem: Some(ca),
            format_version: 3,
            hub_local: false,
            relay: None,
        };
        authenticate_refreshed_hub(&refreshed).await.unwrap();

        let mut wrong_token = refreshed.clone();
        wrong_token.server_password = Some("wrong-token".into());
        assert!(authenticate_refreshed_hub(&wrong_token).await.is_err());

        let mut missing_workspace = refreshed;
        missing_workspace.workspace_id = "fsw1-missing".into();
        assert!(authenticate_refreshed_hub(&missing_workspace)
            .await
            .is_err());

        server.abort();
    }
}
