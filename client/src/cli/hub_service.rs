use anyhow::Context as _;
use feanorfs_client::{
    load_global_config, save_config_secure, save_global_config_secure, ApiClient, Config,
    GlobalConfig, WorkspaceInvite,
};
use feanorfs_server::{
    acquire_hub_runtime, prepare_tls, resolve_or_create_auth_token, run_http_server, ServeOptions,
};
#[cfg(not(target_os = "windows"))]
use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLevel, ServiceManager, ServiceStartCtx, ServiceStatus,
    ServiceStatusCtx,
};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::util::{record_service_identity, service_identity_matches, HubConnection};

const LABEL: &str = "com.feanorfs.hub";
const DEFAULT_PORT: u16 = 3030;
const FALLBACK_PORT_SPAN: u16 = 100;
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const RELAY_CONFIG_FILE: &str = "relay.json";
const LISTEN_PORT_FILE: &str = "listen-port";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HubStatus {
    NotInstalled,
    Running,
    Stopped,
}

#[derive(Debug, Clone)]
struct HubServiceSpec {
    data_dir: PathBuf,
    program: PathBuf,
}

impl HubServiceSpec {
    fn load_default() -> anyhow::Result<Self> {
        Self::load(default_data_dir()?)
    }

    fn load(data_dir: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("create private hub directory {}", data_dir.display()))?;
        let data_dir = data_dir
            .canonicalize()
            .with_context(|| format!("resolve private hub directory {}", data_dir.display()))?;
        Ok(Self {
            data_dir,
            program: std::env::current_exe().context("locate the feanorfs executable")?,
        })
    }

    fn worker_args(&self) -> Vec<OsString> {
        vec![
            OsString::from("service"),
            OsString::from("hub-run"),
            self.data_dir.as_os_str().to_owned(),
        ]
    }

    fn marker_path(&self) -> PathBuf {
        self.data_dir.join("service-program")
    }

    fn installed_program_matches(&self) -> bool {
        service_identity_matches(&self.marker_path(), &[&self.program])
    }

    fn record_installed_program(&self) -> anyhow::Result<()> {
        record_service_identity(&self.marker_path(), &[&self.program])
            .context("record private hub service executable")
    }
}

fn default_data_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("HOME or USERPROFILE is not set")?;
    Ok(PathBuf::from(home).join(".feanorfs").join("hub-data"))
}

fn listen_port_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LISTEN_PORT_FILE)
}

fn load_listen_port(data_dir: &Path) -> anyhow::Result<Option<u16>> {
    let content = match std::fs::read_to_string(listen_port_path(data_dir)) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read private-hub listen port"),
    };
    let port = content
        .trim()
        .parse::<u16>()
        .context("parse private-hub listen port")?;
    if port == 0 {
        anyhow::bail!("private-hub listen port must be between 1 and 65535");
    }
    Ok(Some(port))
}

fn save_listen_port(data_dir: &Path, port: u16) -> anyhow::Result<()> {
    if port == 0 {
        anyhow::bail!("private-hub listen port must be between 1 and 65535");
    }
    std::fs::create_dir_all(data_dir).context("create private-hub data directory")?;
    let path = listen_port_path(data_dir);
    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(&path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(&path)?;
    writeln!(file, "{port}")?;
    file.commit()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(unix)]
    std::fs::File::open(data_dir)?.sync_all()?;
    Ok(())
}

fn hub_state_already_exists(data_dir: &Path) -> bool {
    [
        "auth-token",
        "db.sqlite",
        "hub_state.json",
        "service-program",
        "tls/ca-cert.pem",
    ]
    .iter()
    .any(|relative| data_dir.join(relative).exists())
}

fn select_available_port(preferred: u16) -> anyhow::Result<u16> {
    if preferred != 0 {
        for candidate in preferred..=preferred.saturating_add(FALLBACK_PORT_SPAN) {
            if std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, candidate)).is_ok() {
                return Ok(candidate);
            }
        }
    }
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))
        .context("select an available private-hub port")?;
    Ok(listener.local_addr()?.port())
}

fn resolve_or_create_listen_port(data_dir: &Path) -> anyhow::Result<u16> {
    if let Some(port) = load_listen_port(data_dir)? {
        return Ok(port);
    }
    // Preserve the historical endpoint for every existing hub. Only a fresh
    // profile may move away from 3030 when another application already owns it.
    let port = if hub_state_already_exists(data_dir) {
        DEFAULT_PORT
    } else {
        select_available_port(DEFAULT_PORT)?
    };
    save_listen_port(data_dir, port)?;
    Ok(port)
}

pub(crate) fn portable_invite(invite: WorkspaceInvite) -> WorkspaceInvite {
    let Some(managed_ca) = managed_hub_ca() else {
        return invite;
    };
    portable_invite_for_managed_ca(invite, &managed_ca)
}

fn portable_invite_for_managed_ca(
    mut invite: WorkspaceInvite,
    managed_ca: &str,
) -> WorkspaceInvite {
    if invite.hub_local
        || !invite_points_to_loopback(&invite)
        || invite.tls_ca_pem.as_deref() != Some(managed_ca)
    {
        return invite;
    }
    let Ok(mut url) = reqwest::Url::parse(&invite.server_url) else {
        return invite;
    };
    let hostname = feanorfs_common::hub_mdns_hostname(managed_ca);
    if url.set_host(Some(&hostname)).is_ok() {
        invite.server_url = url.to_string().trim_end_matches('/').to_string();
    }
    invite
}

fn invite_points_to_loopback(invite: &WorkspaceInvite) -> bool {
    reqwest::Url::parse(&invite.server_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

fn managed_hub_ca() -> Option<String> {
    default_data_dir()
        .ok()
        .and_then(|data_dir| std::fs::read_to_string(data_dir.join("tls/ca-cert.pem")).ok())
}

pub(crate) fn owns_workspace(config: &Config) -> bool {
    if config.is_local_hub() {
        return false;
    }
    let Ok(data_dir) = default_data_dir() else {
        return false;
    };
    let legacy_local_http = config.server_password.is_some()
        && config.tls_ca_pem.is_none()
        && config.server_url.starts_with("http://")
        && url_targets_this_machine(&config.server_url)
        && data_dir.join("db.sqlite").is_file();
    let token_matches = std::fs::read_to_string(data_dir.join("auth-token"))
        .ok()
        .is_some_and(|token| config.server_password.as_deref() == Some(token.as_str()));
    let ca_matches = std::fs::read_to_string(data_dir.join("tls/ca-cert.pem"))
        .ok()
        .is_some_and(|ca| config.tls_ca_pem.as_deref() == Some(ca.as_str()));
    (token_matches && (ca_matches || legacy_local_http))
        || (legacy_local_http && !data_dir.join("auth-token").exists())
}

pub(crate) fn status_for_workspace(config: &Config) -> anyhow::Result<Option<HubStatus>> {
    if !owns_workspace(config) {
        return Ok(None);
    }
    platform_status().map(Some)
}

fn url_targets_this_machine(url: &str) -> bool {
    let Some(host) = reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
    else {
        return false;
    };
    if host == "localhost" {
        return true;
    }
    let Ok(host_ip) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    host_ip.is_loopback()
        || if_addrs::get_if_addrs()
            .is_ok_and(|interfaces| interfaces.into_iter().any(|iface| iface.ip() == host_ip))
}

pub(crate) async fn ensure_private_hub(
    bootstrap_token: Option<String>,
    probe_legacy_http: bool,
) -> anyhow::Result<HubConnection> {
    ensure_private_hub_inner(bootstrap_token, probe_legacy_http, false).await
}

pub(crate) async fn refresh_for_pairing(config: &Config) -> anyhow::Result<()> {
    if !owns_workspace(config) {
        return Ok(());
    }
    if config.tls_ca_pem.is_none() || config.server_url.starts_with("http://") {
        anyhow::bail!(
            "This local hub still uses legacy HTTP. Run `feanorfs start --host` once to upgrade it before pairing."
        );
    }
    ensure_private_hub_inner(config.server_password.clone(), false, true).await?;
    Ok(())
}

pub(crate) async fn configure_relay_for_pairing(
    workspace: &Path,
    config: &Config,
    relay_url: &str,
) -> anyhow::Result<Config> {
    if !owns_workspace(config) {
        return Ok(config.clone());
    }
    if config.tls_ca_pem.is_none() || !config.server_url.starts_with("https://") {
        anyhow::bail!(
            "This local hub must use native HTTPS before an opaque relay can be enabled. Run `feanorfs start --host` first."
        );
    }

    let generated = feanorfs_agent_core::tunnel::generate_config(relay_url)?;
    let previous_global = load_global_config().context("load the automatic hub connection")?;
    let data_dir = default_data_dir()?;
    let previous_hub_relay = load_hub_relay(&data_dir)?;
    let relay = config
        .relay
        .as_ref()
        .or(previous_global.relay.as_ref())
        .or(previous_hub_relay.as_ref())
        .filter(|existing| {
            existing.url == generated.url
                && feanorfs_agent_core::tunnel::validate_config(existing).is_ok()
        })
        .cloned()
        .unwrap_or(generated);
    let updated_global = GlobalConfig {
        server_url: previous_global.server_url.clone(),
        server_password: previous_global.server_password.clone(),
        tls_ca_pem: previous_global.tls_ca_pem.clone(),
        relay: Some(relay.clone()),
    };
    save_hub_relay(&data_dir, &relay).context("save private-hub relay configuration")?;
    if let Err(error) = save_global_config_secure(&updated_global) {
        let _ = restore_hub_relay(&data_dir, previous_hub_relay.as_ref());
        return Err(error).context("save opaque relay configuration");
    }
    let mut updated = config.clone();
    updated.relay = Some(relay);
    if let Err(error) = save_config_secure(workspace, &updated) {
        let _ = save_global_config_secure(&previous_global);
        let _ = restore_hub_relay(&data_dir, previous_hub_relay.as_ref());
        return Err(error).context("save workspace relay configuration");
    }
    if let Err(error) = ensure_private_hub_inner(config.server_password.clone(), false, true).await
    {
        let _ = save_config_secure(workspace, config);
        let _ = save_global_config_secure(&previous_global);
        let _ = restore_hub_relay(&data_dir, previous_hub_relay.as_ref());
        let _ = ensure_private_hub_inner(config.server_password.clone(), false, true).await;
        return Err(error).context("restart the private hub with its opaque relay tunnel");
    }
    Ok(updated)
}

fn relay_config_path(data_dir: &Path) -> PathBuf {
    data_dir.join(RELAY_CONFIG_FILE)
}

fn load_hub_relay(data_dir: &Path) -> anyhow::Result<Option<feanorfs_common::RelayConfig>> {
    let content = match std::fs::read_to_string(relay_config_path(data_dir)) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read private-hub relay configuration"),
    };
    let relay: feanorfs_common::RelayConfig =
        serde_json::from_str(&content).context("parse private-hub relay configuration")?;
    feanorfs_agent_core::tunnel::validate_config(&relay)?;
    Ok(Some(relay))
}

fn save_hub_relay(data_dir: &Path, relay: &feanorfs_common::RelayConfig) -> anyhow::Result<()> {
    feanorfs_agent_core::tunnel::validate_config(relay)?;
    std::fs::create_dir_all(data_dir).context("create private-hub data directory")?;
    let content = serde_json::to_vec_pretty(relay).context("encode private-hub relay config")?;
    let path = relay_config_path(data_dir);
    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(&path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(&path)?;
    file.write_all(&content)?;
    file.commit()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(unix)]
    std::fs::File::open(data_dir)?.sync_all()?;
    Ok(())
}

fn restore_hub_relay(
    data_dir: &Path,
    previous: Option<&feanorfs_common::RelayConfig>,
) -> anyhow::Result<()> {
    if let Some(previous) = previous {
        return save_hub_relay(data_dir, previous);
    }
    match std::fs::remove_file(relay_config_path(data_dir)) {
        Ok(()) => {
            #[cfg(unix)]
            std::fs::File::open(data_dir)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("restore private-hub relay configuration"),
    }
    Ok(())
}

async fn ensure_private_hub_inner(
    bootstrap_token: Option<String>,
    probe_legacy_http: bool,
    refresh_tls: bool,
) -> anyhow::Result<HubConnection> {
    let spec = HubServiceSpec::load_default()?;
    let port = resolve_or_create_listen_port(&spec.data_dir)?;
    let mut opts = automatic_options(spec.data_dir.clone(), port);
    let requested_token = (!spec.data_dir.join("auth-token").exists())
        .then_some(bootstrap_token)
        .flatten();
    let token = resolve_or_create_auth_token(&spec.data_dir, requested_token, false)?
        .context("private hub authentication unexpectedly disabled")?;
    let tls = prepare_tls(&mut opts)?.context("private hub TLS unexpectedly disabled")?;
    let connection = HubConnection {
        url: format!("https://127.0.0.1:{port}"),
        token: Some(token),
        tls_ca_pem: tls.public_ca_pem,
        relay: load_hub_relay(&spec.data_dir)?,
    };

    let status = platform_status()?;
    let managed_endpoint_ready = endpoint_ready(&connection).await;
    if managed_endpoint_ready {
        if status == HubStatus::NotInstalled {
            anyhow::bail!(
                "A manually started FeanorFS hub is already using {}. Stop that `feanorfs serve` process and rerun `feanorfs start --host` so the hub can restart automatically at login.",
                spec.data_dir.display()
            );
        }
        if spec.installed_program_matches() && !refresh_tls {
            return Ok(connection);
        }
    }
    if probe_legacy_http && legacy_http_endpoint_ready(&connection).await {
        anyhow::bail!(
            "A manually started legacy HTTP hub is using port {port}. Stop that `feanorfs serve` process and rerun `feanorfs start --host`; FeanorFS will preserve its token, upgrade the workspace to native TLS, and run the hub automatically at login."
        );
    }

    if status != HubStatus::Running && !managed_endpoint_ready {
        match acquire_hub_runtime(&spec.data_dir) {
            Ok(guard) => drop(guard),
            Err(error) => {
                anyhow::bail!(
                    "The private hub data directory is already active but its managed service is not ready: {error}"
                )
            }
        }
    }

    let install_status = if refresh_tls || (status == HubStatus::Running && !managed_endpoint_ready)
    {
        HubStatus::NotInstalled
    } else {
        status
    };
    platform_install_and_start(&spec, install_status)?;
    wait_until_ready(&connection).await.with_context(|| {
        format!(
            "automatic private hub did not become ready on port {port}; another application may be using that port"
        )
    })?;
    Ok(connection)
}

pub(crate) async fn run_supervised(data_dir: PathBuf) -> anyhow::Result<()> {
    let spec = HubServiceSpec::load(data_dir)?;
    let port = resolve_or_create_listen_port(&spec.data_dir)?;
    let relay = match load_hub_relay(&spec.data_dir) {
        Ok(relay) => relay,
        Err(error) => {
            tracing::warn!("private-hub relay configuration is invalid; LAN service remains available: {error:#}");
            None
        }
    };
    let server = run_http_server(automatic_options(spec.data_dir, port));
    tokio::pin!(server);
    let Some(relay) = relay else {
        return server.await;
    };
    let tunnel = feanorfs_agent_core::tunnel::run_host(
        relay,
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
    );
    tokio::pin!(tunnel);
    tokio::select! {
        result = &mut server => result,
        result = &mut tunnel => {
            if let Err(error) = result {
                tracing::warn!("opaque relay tunnel stopped; private hub remains available on LAN: {error:#}");
            }
            server.await
        }
    }
}

fn automatic_options(data_dir: PathBuf, port: u16) -> ServeOptions {
    ServeOptions {
        data_dir,
        port,
        mdns: true,
        gc_interval_secs: 60 * 60,
        ..ServeOptions::default()
    }
}

async fn endpoint_ready(connection: &HubConnection) -> bool {
    let Ok(api) = ApiClient::new_with_tls(
        &connection.url,
        connection.token.as_deref(),
        connection.tls_ca_pem.as_deref(),
    ) else {
        return false;
    };
    matches!(
        tokio::time::timeout(READY_PROBE_TIMEOUT, api.get_workspaces()).await,
        Ok(Ok(_))
    )
}

async fn legacy_http_endpoint_ready(connection: &HubConnection) -> bool {
    let port = reqwest::Url::parse(&connection.url)
        .ok()
        .and_then(|url| url.port_or_known_default())
        .unwrap_or(DEFAULT_PORT);
    let api = ApiClient::new(
        &format!("http://127.0.0.1:{port}"),
        connection.token.as_deref(),
    );
    api.get_workspaces().await.is_ok()
}

async fn wait_until_ready(connection: &HubConnection) -> anyhow::Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    while Instant::now() < deadline {
        if endpoint_ready(connection).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("timed out waiting for the private hub")
}

#[cfg(not(target_os = "windows"))]
fn manager() -> anyhow::Result<Box<dyn ServiceManager>> {
    let mut manager = <dyn ServiceManager>::native().context("detect service manager")?;
    manager
        .set_level(ServiceLevel::User)
        .context("select per-user service management")?;
    Ok(manager)
}

#[cfg(not(target_os = "windows"))]
fn native_label() -> anyhow::Result<service_manager::ServiceLabel> {
    LABEL.parse().context("build private hub service label")
}

#[cfg(not(target_os = "windows"))]
fn platform_status() -> anyhow::Result<HubStatus> {
    let status = manager()?
        .status(ServiceStatusCtx {
            label: native_label()?,
        })
        .context("read private hub service status")?;
    Ok(match status {
        ServiceStatus::NotInstalled => HubStatus::NotInstalled,
        ServiceStatus::Running => HubStatus::Running,
        ServiceStatus::Stopped(_) => HubStatus::Stopped,
    })
}

#[cfg(not(target_os = "windows"))]
fn platform_install_and_start(spec: &HubServiceSpec, status: HubStatus) -> anyhow::Result<()> {
    let manager = manager()?;
    let label = native_label()?;
    let install_required = status == HubStatus::NotInstalled || !spec.installed_program_matches();
    if install_required {
        manager
            .install(ServiceInstallCtx {
                label: label.clone(),
                program: spec.program.clone(),
                args: spec.worker_args(),
                contents: None,
                username: None,
                working_directory: spec
                    .data_dir
                    .parent()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf),
                environment: None,
                autostart: true,
                restart_policy: RestartPolicy::OnFailure {
                    delay_secs: None,
                    max_retries: None,
                    reset_after_secs: None,
                },
            })
            .context("install automatic private hub")?;
        spec.record_installed_program()?;
    }
    if install_required || status != HubStatus::Running {
        manager
            .start(ServiceStartCtx { label })
            .context("start automatic private hub")?;
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn task_name() -> String {
    format!("FeanorFS\\{LABEL}")
}

#[cfg(any(target_os = "windows", test))]
fn windows_task_action(spec: &HubServiceSpec) -> anyhow::Result<(String, String)> {
    let program = spec.program.display().to_string();
    let data_dir = spec.data_dir.display().to_string();
    if program.contains('"') || data_dir.contains('"') {
        anyhow::bail!("Windows paths containing double quotes cannot be installed as tasks");
    }
    Ok((program, format!("service hub-run \"{data_dir}\"")))
}

#[cfg(target_os = "windows")]
fn schtasks(args: &[&str]) -> anyhow::Result<std::process::Output> {
    std::process::Command::new("schtasks.exe")
        .args(args)
        .output()
        .context("run Windows Task Scheduler")
}

#[cfg(target_os = "windows")]
fn platform_status() -> anyhow::Result<HubStatus> {
    Ok(hub_status_from_windows_task(
        super::util::windows_task_running("\\FeanorFS\\", LABEL, &task_name())?,
    ))
}

#[cfg(any(target_os = "windows", test))]
fn hub_status_from_windows_task(running: Option<bool>) -> HubStatus {
    match running {
        None => HubStatus::NotInstalled,
        Some(true) => HubStatus::Running,
        Some(false) => HubStatus::Stopped,
    }
}

#[cfg(target_os = "windows")]
fn platform_install_and_start(spec: &HubServiceSpec, status: HubStatus) -> anyhow::Result<()> {
    let name = task_name();
    if status == HubStatus::NotInstalled || !spec.installed_program_matches() {
        let (program, arguments) = windows_task_action(spec)?;
        super::util::windows_register_task("\\FeanorFS\\", LABEL, &program, &arguments, false)
            .context("install automatic private hub")?;
        spec.record_installed_program()?;
    }
    let output = schtasks(&["/Run", "/TN", &name])?;
    if !output.status.success() {
        anyhow::bail!(
            "start automatic private hub: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_command_contains_only_internal_command_and_data_directory() {
        let spec = HubServiceSpec {
            data_dir: PathBuf::from("/tmp/private hub"),
            program: PathBuf::from("/usr/local/bin/feanorfs"),
        };
        assert_eq!(
            spec.worker_args(),
            vec!["service", "hub-run", "/tmp/private hub"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        let (program, arguments) = windows_task_action(&spec).unwrap();
        assert_eq!(program, "/usr/local/bin/feanorfs");
        assert_eq!(arguments, "service hub-run \"/tmp/private hub\"");
        let action = format!("{program} {arguments}");
        assert!(!action.contains("token"));
        assert!(!action.contains("key"));
        assert!(!action.contains("invite"));
    }

    #[test]
    fn automatic_hub_is_secure_by_default() {
        let options = automatic_options(PathBuf::from("/tmp/hub"), DEFAULT_PORT);
        assert_eq!(options.port, 3030);
        assert!(!options.allow_http);
        assert!(!options.allow_open);
        assert!(options.token.is_none());
        assert!(options.mdns);
        assert!(!options.relay);
        assert!(options.gc_interval_secs > 0);
    }

    #[test]
    fn fresh_hub_selects_and_persists_an_available_port() {
        let data = tempfile::tempdir().unwrap();
        let occupied = std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let preferred = occupied.local_addr().unwrap().port();
        let selected = select_available_port(preferred).unwrap();
        assert_ne!(selected, preferred);
        assert_ne!(selected, 0);

        save_listen_port(data.path(), selected).unwrap();
        assert_eq!(load_listen_port(data.path()).unwrap(), Some(selected));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(listen_port_path(data.path()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn existing_hub_without_port_record_keeps_legacy_default() {
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("auth-token"), "existing").unwrap();
        assert_eq!(
            resolve_or_create_listen_port(data.path()).unwrap(),
            DEFAULT_PORT
        );
        assert_eq!(load_listen_port(data.path()).unwrap(), Some(DEFAULT_PORT));
    }

    #[test]
    fn invalid_persisted_listen_port_fails_closed() {
        let data = tempfile::tempdir().unwrap();
        std::fs::write(listen_port_path(data.path()), "0\n").unwrap();
        assert!(load_listen_port(data.path()).is_err());
        std::fs::write(listen_port_path(data.path()), "not-a-port\n").unwrap();
        assert!(load_listen_port(data.path()).is_err());
    }

    #[tokio::test]
    async fn supervised_hub_uses_persisted_port_and_requires_authentication() {
        let data = tempfile::tempdir().unwrap();
        let port = select_available_port(0).unwrap();
        save_listen_port(data.path(), port).unwrap();
        let data_dir = data.path().to_path_buf();
        let server = tokio::spawn(run_supervised(data_dir.clone()));
        let url = format!("https://127.0.0.1:{port}");
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut ready = false;
        while Instant::now() < deadline {
            if server.is_finished() {
                let outcome = server.await;
                panic!("supervised private hub stopped before readiness: {outcome:?}");
            }
            let token = std::fs::read_to_string(data_dir.join("auth-token"));
            let ca = std::fs::read_to_string(data_dir.join("tls/ca-cert.pem"));
            if let (Ok(token), Ok(ca)) = (token, ca) {
                let authenticated = ApiClient::new_with_tls(&url, Some(&token), Some(&ca)).unwrap();
                if matches!(
                    tokio::time::timeout(READY_PROBE_TIMEOUT, authenticated.get_workspaces()).await,
                    Ok(Ok(_))
                ) {
                    let unauthenticated = ApiClient::new_with_tls(&url, None, Some(&ca)).unwrap();
                    assert!(unauthenticated.get_workspaces().await.is_err());
                    ready = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        server.abort();
        let _ = server.await;
        assert!(ready, "supervised private hub did not become ready");
    }

    #[test]
    fn windows_hub_task_state_distinguishes_running_stopped_and_missing() {
        assert_eq!(hub_status_from_windows_task(Some(true)), HubStatus::Running);
        assert_eq!(
            hub_status_from_windows_task(Some(false)),
            HubStatus::Stopped
        );
        assert_eq!(hub_status_from_windows_task(None), HubStatus::NotInstalled);
    }

    #[test]
    fn hub_relay_config_is_private_atomic_and_roundtrips() {
        let data = tempfile::tempdir().unwrap();
        let relay = feanorfs_agent_core::tunnel::generate_config("http://127.0.0.1:3040").unwrap();
        save_hub_relay(data.path(), &relay).unwrap();
        assert_eq!(load_hub_relay(data.path()).unwrap(), Some(relay));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(relay_config_path(data.path()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        restore_hub_relay(data.path(), None).unwrap();
        assert!(load_hub_relay(data.path()).unwrap().is_none());
    }

    #[test]
    fn portable_invite_rewrites_only_the_matching_managed_hub() {
        let invite = WorkspaceInvite {
            server_url: "https://127.0.0.1:43123".into(),
            workspace_id: "workspace".into(),
            server_token: Some("secret".into()),
            encryption_key: "a".repeat(64),
            tls_ca_pem: Some("managed-public-ca".into()),
            hub_local: false,
            relay: None,
        };
        let portable = portable_invite_for_managed_ca(invite.clone(), "managed-public-ca");
        assert_eq!(
            portable.server_url,
            format!(
                "https://{}:43123",
                feanorfs_common::hub_mdns_hostname("managed-public-ca")
            )
        );
        assert_eq!(portable.server_token, invite.server_token);
        assert_eq!(portable.encryption_key, invite.encryption_key);

        let unrelated = portable_invite_for_managed_ca(invite.clone(), "different-public-ca");
        assert_eq!(unrelated, invite);
    }
}
