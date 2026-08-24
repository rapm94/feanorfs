use anyhow::Context as _;
use feanorfs_client::{
    load_global_config, save_config_secure, save_global_config_secure, ApiClient, Config,
    GlobalConfig, WorkspaceInvite,
};
use feanorfs_common::{
    MeshCandidate, MeshCandidateKind, MeshConfig, MeshTransport, MAX_MESH_CANDIDATES,
};
use feanorfs_server::{
    acquire_hub_runtime, prepare_tls, resolve_or_create_auth_token, run_http_server, ServeOptions,
};
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::util::HubConnection;

const DEFAULT_PORT: u16 = 3030;
const FALLBACK_PORT_SPAN: u16 = 100;
const READY_TIMEOUT: Duration = Duration::from_secs(20);
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const RELAY_CONFIG_FILE: &str = "relay.json";
const LISTEN_PORT_FILE: &str = "listen-port";

/// Private-hub state as seen through the single supervisor job.
pub(crate) use crate::cli::supervisor::ServiceState as HubStatus;

#[derive(Debug, Clone)]
struct HubServiceSpec {
    data_dir: PathBuf,
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
        Ok(Self { data_dir })
    }
}

pub(crate) fn default_data_dir() -> anyhow::Result<PathBuf> {
    Ok(feanorfs_agent_core::global_state_root()?.join("hub-data"))
}

/// True when a private hub data directory exists with provisioned state, so
/// the supervisor should run its hub worker.
pub(crate) fn hub_data_present() -> bool {
    default_data_dir().is_ok_and(|dir| hub_state_already_exists(&dir))
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

/// Private durable replacement: the private-hub listen
/// port is persisted via a 0o600 temp file, atomic rename, post-commit mode
/// fix, and a parent-directory sync on Unix so the hub can bind the same port
/// after a crash instead of drifting to a fallback.
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
    super::supervisor::hub_status().map(Some)
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
        mesh: previous_global.mesh.clone(),
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

/// Private durable replacement: the private-hub relay
/// configuration is persisted like the listen port — 0o600 temp file, atomic
/// rename, post-commit mode fix, parent sync on Unix — so a crash cannot drop
/// the configured opaque relay tunnel.
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
        mesh: Some(automatic_mesh_config(port).await?),
    };

    let status = super::supervisor::supervisor_job_state()?;
    let managed_endpoint_ready = endpoint_ready(&connection).await;
    if managed_endpoint_ready {
        if status == HubStatus::NotInstalled {
            anyhow::bail!(
                "A manually started FeanorFS hub is already using {}. Stop that `feanorfs serve` process and rerun `feanorfs start --host` so the hub can restart automatically at login.",
                spec.data_dir.display()
            );
        }
        if super::supervisor::installed_program_matches() && !refresh_tls {
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

    // The single supervisor job owns the hub worker. It detects the fresh hub
    // data directory and every relay/port change within its reconcile cycle.
    super::supervisor::ensure_supervisor_running()?;
    if refresh_tls {
        wait_for_hub_restart(&connection).await.with_context(|| {
            format!(
                "automatic private hub did not restart on port {port}; another application may be using that port"
            )
        })?;
    } else {
        wait_until_ready(&connection).await.with_context(|| {
            format!(
                "automatic private hub did not become ready on port {port}; another application may be using that port"
            )
        })?;
    }
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
    start_punch_bridge(&spec.data_dir, port);
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

fn start_punch_bridge(data_dir: &Path, port: u16) {
    let read = |name: &str| -> anyhow::Result<String> {
        Ok(std::fs::read_to_string(data_dir.join("tls").join(name))?)
    };
    let (cert, key) = match (read("server-cert.pem"), read("server-key.pem")) {
        (Ok(cert), Ok(key)) => (cert, key),
        _ => {
            tracing::debug!("private-hub TLS material is unavailable; mesh punch disabled");
            return;
        }
    };
    let identity = match feanorfs_agent_core::mesh::MachineIdentity::load_or_create() {
        Ok(identity) => identity,
        Err(error) => {
            tracing::warn!("mesh punch disabled; node identity unavailable: {error:#}");
            return;
        }
    };
    let bind: SocketAddr = ([0, 0, 0, 0], port).into();
    let upstream: SocketAddr = ([127, 0, 0, 1], port).into();
    let data_dir = data_dir.to_path_buf();
    tokio::spawn(async move {
        match feanorfs_agent_core::mesh::serve_punch_bridge(
            bind,
            cert,
            key,
            feanorfs_agent_core::mesh::PunchPeer { identity },
            upstream,
        )
        .await
        {
            Ok(handle) => {
                tracing::info!("mesh punch bridge listening on UDP {}", handle.local);
                if let Some(reflexive) = handle.reflexive {
                    match persist_reflexive(&data_dir, reflexive).await {
                        Ok(()) => tracing::info!("mesh punch reflexive address recorded"),
                        Err(error) => {
                            tracing::debug!("could not record punch reflexive address: {error:#}")
                        }
                    }
                }
            }
            Err(error) => tracing::debug!("mesh punch bridge disabled: {error:#}"),
        }
    });
}

const REFLEXIVE_FILE: &str = "mesh-reflexive.json";
const REFLEXIVE_SCHEMA_VERSION: u32 = 1;

/// Persists the punch socket's STUN-reflexive mapping so capability
/// generation can advertise it even while the worker holds the port.
async fn persist_reflexive(data_dir: &Path, address: SocketAddr) -> anyhow::Result<()> {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct ReflexiveRecord {
        #[allow(dead_code)]
        schema_version: u32,
        address: SocketAddr,
    }
    let content = serde_json::to_vec_pretty(&ReflexiveRecord {
        schema_version: REFLEXIVE_SCHEMA_VERSION,
        address,
    })?;
    feanorfs_agent_core::fs_util::atomic_write_durable(data_dir, REFLEXIVE_FILE, &content)
        .await
        .map_err(|error| anyhow::anyhow!("persist punch reflexive record: {error:#}"))
}

/// Reads the reflexive mapping the punch-bridge worker discovered through its
/// own bound socket. Absent, corrupt, loopback, or unspecified records are
/// ignored — this is a hint file, never authority.
pub(crate) fn load_reflexive(data_dir: &Path) -> Option<SocketAddr> {
    #[derive(serde::Deserialize)]
    struct ReflexiveRecord {
        #[allow(dead_code)]
        schema_version: u32,
        address: SocketAddr,
    }
    let content = std::fs::read_to_string(data_dir.join(REFLEXIVE_FILE)).ok()?;
    let record: ReflexiveRecord = serde_json::from_str(&content).ok()?;
    let address = record.address;
    (!address.ip().is_loopback() && !address.ip().is_unspecified()).then_some(address)
}

async fn automatic_mesh_config(port: u16) -> anyhow::Result<MeshConfig> {
    let identity = feanorfs_agent_core::mesh::MachineIdentity::load_or_create()?;
    let addresses = if_addrs::get_if_addrs()?
        .into_iter()
        .map(|interface| interface.ip());
    let mut candidates = interface_candidates(port, addresses);
    let internal = candidates
        .iter()
        .find(|candidate| {
            candidate.kind() == MeshCandidateKind::Lan && candidate.address().is_ipv4()
        })
        .map(|candidate| candidate.address().ip());
    if let Some(internal) = internal {
        match feanorfs_agent_core::mesh::map_tcp_port(internal, port).await {
            Ok(mapped) => candidates.push(mapped),
            Err(error) => {
                tracing::debug!("private-hub NAT port mapping unavailable: {error:#}");
            }
        }
    }
    match feanorfs_agent_core::mesh::discover_reflexive(Some(port)).await {
        Ok(reflexive) => {
            if let Ok(candidate) =
                MeshCandidate::new(MeshTransport::Quic, MeshCandidateKind::Reflexive, reflexive)
            {
                candidates.push(candidate);
            }
        }
        Err(error) => {
            // The punch-bridge worker usually owns the UDP port by now; fall
            // back to the mapping it discovered through that exact socket.
            tracing::debug!("private-hub STUN discovery unavailable: {error:#}");
            let spec = HubServiceSpec::load_default().ok();
            if let Some(address) = spec.and_then(|spec| load_reflexive(&spec.data_dir)) {
                if let Ok(candidate) =
                    MeshCandidate::new(MeshTransport::Quic, MeshCandidateKind::Reflexive, address)
                {
                    candidates.push(candidate);
                }
            }
        }
    }
    MeshConfig::new(identity.node_id(), prioritized_candidates(candidates))
}

fn interface_candidates(
    port: u16,
    addresses: impl IntoIterator<Item = IpAddr>,
) -> Vec<MeshCandidate> {
    addresses
        .into_iter()
        .filter_map(|address| {
            MeshCandidate::new(
                MeshTransport::Tcp,
                mesh_candidate_kind(address)?,
                SocketAddr::new(address, port),
            )
            .ok()
        })
        .collect()
}

const fn candidate_rank(kind: MeshCandidateKind) -> u8 {
    match kind {
        MeshCandidateKind::Direct => 0,
        MeshCandidateKind::Mapped => 1,
        _ => 2,
    }
}

fn prioritized_candidates(mut candidates: Vec<MeshCandidate>) -> Vec<MeshCandidate> {
    candidates.sort_unstable();
    candidates.dedup();
    candidates
        .sort_unstable_by_key(|candidate| (candidate_rank(candidate.kind()), candidate.address()));
    candidates.truncate(MAX_MESH_CANDIDATES);
    candidates
}

#[cfg(test)]
fn mesh_config_for_addresses(
    node_id: feanorfs_common::NodeId,
    port: u16,
    addresses: impl IntoIterator<Item = IpAddr>,
) -> anyhow::Result<MeshConfig> {
    MeshConfig::new(
        node_id,
        prioritized_candidates(interface_candidates(port, addresses)),
    )
}

fn mesh_candidate_kind(address: IpAddr) -> Option<MeshCandidateKind> {
    match address {
        IpAddr::V4(address) if address.is_loopback() || address.is_unspecified() => None,
        IpAddr::V4(address)
            if address.is_private()
                || address.is_link_local()
                || (address.octets()[0] == 100 && (64..=127).contains(&address.octets()[1])) =>
        {
            Some(MeshCandidateKind::Lan)
        }
        IpAddr::V4(_) => Some(MeshCandidateKind::Direct),
        IpAddr::V6(address)
            if address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || address.is_unicast_link_local() =>
        {
            None
        }
        IpAddr::V6(address) if address.segments()[0] & 0xfe00 == 0xfc00 => {
            Some(MeshCandidateKind::Lan)
        }
        IpAddr::V6(_) => Some(MeshCandidateKind::Direct),
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

/// After a relay or port change the supervisor restarts the hub worker: wait
/// a bounded moment for the old process to stop serving, then for the
/// replacement to become ready again.
///
/// When nothing actually changed (plain LAN pairing, unchanged relay), no
/// restart happens and the still-serving hub is exactly what the caller
/// needs; waiting out the full grace period would stall every `pair` by
/// several seconds for no effect.
async fn wait_for_hub_restart(connection: &HubConnection) -> anyhow::Result<()> {
    if endpoint_ready(connection).await {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut restarted = false;
        while Instant::now() < deadline {
            if !endpoint_ready(connection).await {
                restarted = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if !restarted {
            return Ok(());
        }
    }
    wait_until_ready(connection).await
}

#[cfg(not(target_os = "windows"))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_worker_command_contains_only_internal_command_and_data_directory() {
        let mut command = tokio::process::Command::new("/usr/local/bin/feanorfs");
        command.args(["service", "hub-run", "/tmp/private hub"]);
        let action = format!(
            "{} {}",
            command.as_std().get_program().to_string_lossy(),
            command
                .as_std()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert_eq!(
            action,
            "/usr/local/bin/feanorfs service hub-run /tmp/private hub"
        );
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
    fn hub_status_aliases_the_supervisor_state() {
        assert_eq!(
            HubStatus::Running,
            crate::cli::supervisor::ServiceState::Running
        );
        assert_eq!(
            HubStatus::Stopped,
            crate::cli::supervisor::ServiceState::Stopped
        );
        assert_eq!(
            HubStatus::NotInstalled,
            crate::cli::supervisor::ServiceState::NotInstalled
        );
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
            mesh: None,
            ignore_policy: None,
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

    #[test]
    fn automatic_mesh_candidates_are_bounded_and_prioritize_direct_addresses() {
        let node_id = feanorfs_common::NodeId::from_public_key([7_u8; 32]);
        let mut addresses = (1_u8..=20)
            .map(|suffix| IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, suffix)))
            .collect::<Vec<_>>();
        addresses.push("2001:4860:4860::8888".parse().unwrap());
        addresses.push(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        addresses.push("fe80::1".parse().unwrap());

        let mesh = mesh_config_for_addresses(node_id, 3030, addresses).unwrap();

        assert_eq!(mesh.candidates().len(), MAX_MESH_CANDIDATES);
        assert!(mesh.candidates().iter().any(|candidate| {
            candidate.kind() == MeshCandidateKind::Direct
                && candidate.address() == "[2001:4860:4860::8888]:3030".parse().unwrap()
        }));
        assert!(mesh
            .candidates()
            .iter()
            .all(|candidate| !candidate.address().ip().is_loopback()));
    }

    #[test]
    fn mapped_candidates_rank_between_direct_and_lan_within_the_cap() {
        let lan = MeshCandidate::new(
            MeshTransport::Tcp,
            MeshCandidateKind::Lan,
            "10.0.0.1:3030".parse().unwrap(),
        )
        .unwrap();
        let direct = MeshCandidate::new(
            MeshTransport::Tcp,
            MeshCandidateKind::Direct,
            "203.0.113.9:3030".parse().unwrap(),
        )
        .unwrap();
        let mapped = MeshCandidate::new(
            MeshTransport::Tcp,
            MeshCandidateKind::Mapped,
            "198.51.100.4:54321".parse().unwrap(),
        )
        .unwrap();

        let ranked = prioritized_candidates(vec![lan, mapped, direct.clone()]);

        assert_eq!(
            ranked
                .iter()
                .map(|candidate| candidate.kind())
                .collect::<Vec<_>>(),
            vec![
                MeshCandidateKind::Direct,
                MeshCandidateKind::Mapped,
                MeshCandidateKind::Lan
            ]
        );
        assert_eq!(
            prioritized_candidates(vec![direct; MAX_MESH_CANDIDATES + 3]).len(),
            1
        );
        let many = (0..MAX_MESH_CANDIDATES + 3)
            .map(|index| {
                MeshCandidate::new(
                    MeshTransport::Tcp,
                    MeshCandidateKind::Lan,
                    SocketAddr::from((
                        std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, index as u8 + 1)),
                        3030,
                    )),
                )
                .unwrap()
            })
            .collect();
        assert_eq!(prioritized_candidates(many).len(), MAX_MESH_CANDIDATES);
    }
}
