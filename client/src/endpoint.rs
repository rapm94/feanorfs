use anyhow::Context as _;
use feanorfs_agent_core::mesh::{
    DirectPeerDialer, MeshFailureKind, MeshPath, MeshStateStore, PeerDialTarget, PeerDialer as _,
};
use feanorfs_agent_core::ApiClient;
use feanorfs_common::{
    hub_ca_fingerprint, hub_mdns_hostname, MeshCandidate, MeshCandidateKind, MeshConfig,
    MeshTransport, HUB_MDNS_SERVICE, MAX_MESH_CANDIDATES,
};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs as _};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::local::{load_global_config, save_config, save_global_config, Config};

const PROBE_TIMEOUT: Duration = Duration::from_millis(900);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
const MESH_DIRECT_DIAL_TIMEOUT: Duration = Duration::from_millis(1_500);

struct StableEndpoint {
    url: String,
    hostname: String,
    fingerprint: String,
    port: u16,
}

pub(crate) async fn open(workspace: &Path, config: &Config) -> anyhow::Result<ApiClient> {
    if config.is_local_hub() {
        return ApiClient::from_config(workspace, config).await;
    }

    // Capability candidates are raced before anything else, including the
    // CA-bound stable-name gate: only an authenticated TLS probe wins, and
    // slower discovery or DNS must not preempt a signed direct candidate.
    if let Some(client) = try_mesh_direct(config).await {
        return Ok(client);
    }

    let Some(stable) = stable_endpoint(config) else {
        return ApiClient::from_config_direct(workspace, config).await;
    };

    // Prefer an exactly identity-matched managed hub on this machine before
    // asking the system resolver for its `.local` name. Some resolvers keep a
    // blocking mDNS lookup alive after the async probe timeout, delaying every
    // short-lived CLI command until the runtime can shut down. The resolved
    // client retains the CA-bound SNI and certificate verification; only its
    // socket address is loopback.
    if let Some(pinned_ca) = config.tls_ca_pem.as_deref() {
        if let Some(address) = same_machine_address(&stable, pinned_ca) {
            let resolved = ApiClient::new_with_tls_resolved(
                &stable.url,
                config.server_password.as_deref(),
                config.tls_ca_pem.as_deref(),
                &stable.hostname,
                &[address],
            )?;
            if probe(&resolved).await {
                // This is a local transport fallback, not proof that the
                // CA-bound name resolves on the network. Preserve the
                // configured endpoint until a direct or mDNS probe succeeds.
                return Ok(resolved);
            }
        }
    }

    let original = ApiClient::from_config_direct(workspace, config).await?;
    let direct = ApiClient::new_with_tls(
        &stable.url,
        config.server_password.as_deref(),
        config.tls_ca_pem.as_deref(),
    )?;
    if probe(&direct).await {
        persist_stable_url(workspace, config, &stable.url);
        return Ok(direct);
    }

    if let Some(client) = try_mesh_quic(config, &stable).await {
        persist_stable_url(workspace, config, &stable.url);
        return Ok(client);
    }

    let fingerprint = stable.fingerprint.clone();
    let hostname = stable.hostname.clone();
    let port = stable.port;
    let addresses = tokio::task::spawn_blocking(move || {
        discover_addresses(&fingerprint, &hostname, port, DISCOVERY_TIMEOUT)
    })
    .await
    .context("join FeanorFS mDNS discovery")?;
    if !addresses.is_empty() {
        let resolved = ApiClient::new_with_tls_resolved(
            &stable.url,
            config.server_password.as_deref(),
            config.tls_ca_pem.as_deref(),
            &stable.hostname,
            &addresses,
        )?;
        if probe(&resolved).await {
            persist_authenticated_mdns(workspace, config, &stable.url, &addresses);
            return Ok(resolved);
        }
    }

    // Old hubs may not yet advertise the stable name or include it in the leaf
    // SAN. Keep their configured endpoint until the host upgrades.
    if config.relay.is_some() {
        ApiClient::from_config(workspace, config).await
    } else {
        Ok(original)
    }
}

fn same_machine_address(stable: &StableEndpoint, pinned_ca: &str) -> Option<SocketAddr> {
    same_machine_address_in(
        stable,
        pinned_ca,
        &feanorfs_agent_core::global_state_root()
            .ok()?
            .join("hub-data"),
    )
}

fn same_machine_address_in(
    stable: &StableEndpoint,
    pinned_ca: &str,
    data_dir: &Path,
) -> Option<SocketAddr> {
    let managed_ca = std::fs::read_to_string(data_dir.join("tls").join("ca-cert.pem")).ok()?;
    if managed_ca != pinned_ca || stable.hostname != hub_mdns_hostname(&managed_ca) {
        return None;
    }
    let port = std::fs::read_to_string(data_dir.join("listen-port"))
        .ok()?
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0 && *port == stable.port)?;
    Some(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
}

async fn probe(client: &ApiClient) -> bool {
    tokio::time::timeout(PROBE_TIMEOUT, client.get_workspaces())
        .await
        .is_ok_and(|result| result.is_ok())
}

fn mesh_state_store() -> Option<MeshStateStore> {
    let root = feanorfs_agent_core::global_state_root().ok()?;
    MeshStateStore::open(&root).ok()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

async fn try_mesh_direct(config: &Config) -> Option<ApiClient> {
    let mesh = config.mesh.clone()?;
    let node_id = mesh.node_id();
    let target = PeerDialTarget {
        server_url: config.server_url.clone(),
        server_password: config.server_password.clone(),
        tls_ca_pem: config.tls_ca_pem.clone(),
        mesh,
    };
    let store = mesh_state_store();
    let started = Instant::now();
    match DirectPeerDialer::default().dial(target).await {
        Ok(outcome) => {
            if let Some(store) = store {
                let candidate = outcome.candidate().clone();
                if let Ok(path) = MeshPath::new(node_id, candidate, now_ms()) {
                    let _ = store.record_success(path);
                }
            }
            Some(outcome.into_client())
        }
        Err(_) => {
            if let Some(store) = store {
                let kind = if started.elapsed() >= MESH_DIRECT_DIAL_TIMEOUT {
                    MeshFailureKind::Timeout
                } else {
                    MeshFailureKind::Unreachable
                };
                let _ = store.record_failure(MeshTransport::Tcp, kind, now_ms());
            }
            None
        }
    }
}

const MESH_QUIC_CANDIDATE_BUDGET: usize = 4;

async fn try_mesh_quic(config: &Config, stable: &StableEndpoint) -> Option<ApiClient> {
    let mesh = config.mesh.as_ref()?;
    let ca_pem = config.tls_ca_pem.as_deref()?;
    let identity = feanorfs_agent_core::mesh::MachineIdentity::load_or_create().ok()?;
    let quic_candidates = mesh
        .candidates()
        .iter()
        .filter(|candidate| candidate.transport() == MeshTransport::Quic)
        .take(MESH_QUIC_CANDIDATE_BUDGET)
        .cloned()
        .collect::<Vec<_>>();
    if quic_candidates.is_empty() {
        return None;
    }

    let store = mesh_state_store();
    let configured_port = reqwest::Url::parse(&config.server_url)
        .ok()
        .and_then(|url| url.port_or_known_default())
        .unwrap_or(443);
    for candidate in quic_candidates {
        // URL host must be the candidate IP: hub leaves SAN-cover their
        // interface addresses, and Linux reqwest does not honor DNS
        // overrides for `.local` names (macOS does). The CA pin still
        // verifies the certificate for this exact IP.
        let ip_host = candidate.address().ip().to_string();
        let url = format!("https://{ip_host}:{configured_port}");
        match feanorfs_agent_core::mesh::dial_punch_bridge(
            candidate.address(),
            ca_pem,
            &stable.hostname,
            feanorfs_agent_core::mesh::PunchPeer {
                identity: identity.clone(),
            },
        )
        .await
        {
            Ok(bridge) => {
                let client = ApiClient::new_with_tls_resolved(
                    &url,
                    config.server_password.as_deref(),
                    Some(ca_pem),
                    &ip_host,
                    &[bridge],
                )
                .ok();
                if let Some(client) = client {
                    if probe(&client).await {
                        if let Some(store) = store.as_ref() {
                            if let Ok(path) = MeshPath::new(mesh.node_id(), candidate, now_ms()) {
                                let _ = store.record_success(path);
                            }
                        }
                        return Some(client);
                    }
                    if let Some(store) = store.as_ref() {
                        let _ = store.record_failure(
                            MeshTransport::Quic,
                            MeshFailureKind::Authentication,
                            now_ms(),
                        );
                    }
                }
            }
            Err(_) => {
                if let Some(store) = store.as_ref() {
                    let _ = store.record_failure(
                        MeshTransport::Quic,
                        MeshFailureKind::Unreachable,
                        now_ms(),
                    );
                }
            }
        }
    }
    None
}

fn stable_endpoint(config: &Config) -> Option<StableEndpoint> {
    let ca = config.tls_ca_pem.as_deref()?;
    let mut url = reqwest::Url::parse(&config.server_url).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    let expected = hub_mdns_hostname(ca);
    let current = url.host_str()?;
    let eligible = current.eq_ignore_ascii_case(&expected)
        || current.eq_ignore_ascii_case("localhost")
        || current.parse::<IpAddr>().is_ok();
    if !eligible || url.set_host(Some(&expected)).is_err() {
        return None;
    }
    let port = url.port_or_known_default()?;
    Some(StableEndpoint {
        url: url.to_string().trim_end_matches('/').to_string(),
        hostname: expected,
        fingerprint: hub_ca_fingerprint(ca),
        port,
    })
}

fn discover_addresses(
    fingerprint: &str,
    hostname: &str,
    port: u16,
    timeout: Duration,
) -> Vec<SocketAddr> {
    #[cfg(target_os = "linux")]
    {
        let native = discover_addresses_avahi(fingerprint, hostname, port);
        if !native.is_empty() {
            return native;
        }
    }

    let Ok(daemon) = ServiceDaemon::new() else {
        return Vec::new();
    };
    let Ok(receiver) = daemon.browse(HUB_MDNS_SERVICE) else {
        let _ = daemon.shutdown();
        return Vec::new();
    };
    let deadline = Instant::now() + timeout;
    let mut addresses = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info))
                if info.get_property_val_str("v") == Some("1")
                    && info.get_property_val_str("scheme") == Some("https")
                    && info.get_property_val_str("ca") == Some(fingerprint)
                    && service_identity_matches(&info, hostname)
                    && info.get_port() == port =>
            {
                let resolved = info.get_addresses_v4();
                addresses.extend(
                    resolved
                        .into_iter()
                        .map(|address| SocketAddr::new(IpAddr::V4(address), port)),
                );
                if addresses.is_empty() {
                    let service_host = info.get_hostname().trim_end_matches('.');
                    if let Ok(system_addresses) = (service_host, port).to_socket_addrs() {
                        addresses.extend(system_addresses.filter(SocketAddr::is_ipv4));
                    }
                }
                if !addresses.is_empty() {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(HUB_MDNS_SERVICE);
    let _ = daemon.shutdown();
    addresses.sort_unstable();
    addresses.dedup();
    addresses
}

#[cfg(target_os = "linux")]
fn discover_addresses_avahi(fingerprint: &str, hostname: &str, port: u16) -> Vec<SocketAddr> {
    type ResolvedService = (
        i32,
        i32,
        String,
        String,
        String,
        String,
        i32,
        String,
        u16,
        Vec<Vec<u8>>,
        u32,
    );

    let Ok(connection) = zbus::blocking::Connection::system() else {
        return Vec::new();
    };
    let Ok(proxy) = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.Avahi",
        "/",
        "org.freedesktop.Avahi.Server",
    ) else {
        return Vec::new();
    };
    let instance = hostname.strip_suffix(".local").unwrap_or(hostname);
    let service_type = HUB_MDNS_SERVICE
        .strip_suffix(".local.")
        .unwrap_or(HUB_MDNS_SERVICE);
    let request = (
        -1_i32,
        -1_i32,
        instance,
        service_type,
        "local",
        0_i32,
        0_u32,
    );
    let Ok((
        _,
        _,
        resolved_instance,
        resolved_type,
        resolved_domain,
        _,
        _,
        address,
        resolved_port,
        txt,
        _,
    )) = proxy.call::<_, _, ResolvedService>("ResolveService", &request)
    else {
        return Vec::new();
    };
    let identity_matches = resolved_instance.eq_ignore_ascii_case(instance)
        && resolved_type.eq_ignore_ascii_case(service_type)
        && resolved_domain
            .trim_end_matches('.')
            .eq_ignore_ascii_case("local")
        && resolved_port == port
        && avahi_txt_value(&txt, "v") == Some("1")
        && avahi_txt_value(&txt, "scheme") == Some("https")
        && avahi_txt_value(&txt, "ca") == Some(fingerprint);
    if !identity_matches {
        return Vec::new();
    }
    address
        .parse::<Ipv4Addr>()
        .ok()
        .map(|address| vec![SocketAddr::new(IpAddr::V4(address), port)])
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn avahi_txt_value<'a>(records: &'a [Vec<u8>], key: &str) -> Option<&'a str> {
    records.iter().find_map(|record| {
        let separator = record.iter().position(|byte| *byte == b'=')?;
        let (record_key, value) = record.split_at(separator);
        let value = value.get(1..)?;
        (record_key == key.as_bytes())
            .then(|| std::str::from_utf8(value).ok())
            .flatten()
    })
}

fn service_identity_matches(info: &mdns_sd::ResolvedService, hostname: &str) -> bool {
    if info
        .get_hostname()
        .trim_end_matches('.')
        .eq_ignore_ascii_case(hostname)
    {
        return true;
    }
    let expected_instance = hostname.strip_suffix(".local").unwrap_or(hostname);
    info.get_fullname()
        .split('.')
        .next()
        .is_some_and(|instance| instance.eq_ignore_ascii_case(expected_instance))
}

fn persist_stable_url(workspace: &Path, config: &Config, url: &str) {
    persist_connection_update(workspace, config, url, None);
}

fn persist_authenticated_mdns(
    workspace: &Path,
    config: &Config,
    url: &str,
    addresses: &[SocketAddr],
) {
    let refreshed = config
        .mesh
        .as_ref()
        .and_then(|mesh| refresh_lan_candidates(mesh, addresses).ok());
    persist_connection_update(workspace, config, url, refreshed);
}

fn persist_connection_update(
    workspace: &Path,
    config: &Config,
    url: &str,
    refreshed_mesh: Option<MeshConfig>,
) {
    let mut updated = config.clone();
    updated.server_url = url.to_string();
    if let Some(mesh) = refreshed_mesh {
        updated.mesh = Some(mesh);
    }
    if updated.server_url == config.server_url && updated.mesh == config.mesh {
        return;
    }
    if let Err(error) = save_config(workspace, &updated) {
        tracing::warn!("could not persist stable private-hub endpoint: {error}");
        return;
    }
    if let Ok(mut global) = load_global_config() {
        if global.server_url == config.server_url && global.tls_ca_pem == config.tls_ca_pem {
            global.server_url = url.to_string();
            if global.mesh == config.mesh {
                global.mesh = updated.mesh.clone();
            }
            if let Err(error) = save_global_config(&global) {
                tracing::warn!("could not persist stable global hub endpoint: {error}");
            }
        }
    }
}

fn refresh_lan_candidates(
    mesh: &MeshConfig,
    addresses: &[SocketAddr],
) -> anyhow::Result<MeshConfig> {
    let mut candidates = mesh
        .candidates()
        .iter()
        .filter(|candidate| candidate.kind() != MeshCandidateKind::Lan)
        .cloned()
        .collect::<Vec<_>>();
    for address in addresses {
        if let Ok(candidate) =
            MeshCandidate::new(MeshTransport::Tcp, MeshCandidateKind::Lan, *address)
        {
            candidates.push(candidate);
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    candidates.sort_unstable_by_key(|candidate| {
        let rank = match candidate.kind() {
            MeshCandidateKind::Direct => 0,
            MeshCandidateKind::Mapped => 1,
            _ => 2,
        };
        (rank, candidate.address())
    });
    candidates.truncate(MAX_MESH_CANDIDATES);
    MeshConfig::new(mesh.node_id(), candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(server_url: &str, ca: Option<&str>) -> Config {
        Config {
            server_url: server_url.into(),
            workspace_id: "workspace".into(),
            encryption_password: Some("a".repeat(64)),
            server_password: Some("token".into()),
            tls_ca_pem: ca.map(str::to_string),
            format_version: 3,
            hub_local: false,
            relay: None,
            mesh: None,
        }
    }

    #[test]
    fn numeric_private_hub_gets_ca_bound_stable_candidate() {
        let candidate = stable_endpoint(&config("https://192.168.1.13:3030", Some("ca")))
            .expect("stable endpoint");
        assert_eq!(
            candidate.url,
            format!("https://{}:3030", hub_mdns_hostname("ca"))
        );
        assert_eq!(candidate.fingerprint, hub_ca_fingerprint("ca"));
    }

    #[test]
    fn public_and_custom_named_endpoints_are_not_rewritten() {
        assert!(stable_endpoint(&config("https://hub.example:3030", Some("ca"))).is_none());
        assert!(stable_endpoint(&config("https://192.168.1.13:3030", None)).is_none());
        assert!(stable_endpoint(&config("http://192.168.1.13:3030", Some("ca"))).is_none());
    }

    #[test]
    fn same_machine_fallback_requires_exact_ca_and_port() {
        let data = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(data.path().join("tls")).unwrap();
        std::fs::write(data.path().join("tls/ca-cert.pem"), "managed-ca").unwrap();
        std::fs::write(data.path().join("listen-port"), "3030\n").unwrap();
        let stable =
            stable_endpoint(&config("https://127.0.0.1:3030", Some("managed-ca"))).unwrap();

        assert_eq!(
            same_machine_address_in(&stable, "managed-ca", data.path()),
            Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 3030)))
        );
        assert_eq!(
            same_machine_address_in(&stable, "other-ca", data.path()),
            None
        );

        std::fs::write(data.path().join("listen-port"), "3031\n").unwrap();
        assert_eq!(
            same_machine_address_in(&stable, "managed-ca", data.path()),
            None
        );
    }

    #[test]
    fn native_service_hostname_is_accepted_by_ca_bound_instance() {
        let expected = hub_mdns_hostname("managed-ca");
        let instance = expected.strip_suffix(".local").unwrap();
        let info = mdns_sd::ServiceInfo::new(
            HUB_MDNS_SERVICE,
            instance,
            "ordinary-mac-host.local.",
            "192.0.2.10",
            3030,
            &[("v", "1")][..],
        )
        .unwrap()
        .as_resolved_service();

        assert!(service_identity_matches(&info, &expected));
        assert!(!service_identity_matches(
            &info,
            "feanorfs-ffffffffffffffff.local"
        ));
    }

    #[test]
    fn authenticated_discovery_replaces_only_stale_lan_candidates() {
        let node = feanorfs_common::NodeId::from_public_key([9_u8; 32]);
        let stale = MeshCandidate::new(
            MeshTransport::Tcp,
            MeshCandidateKind::Lan,
            "192.168.50.30:3031".parse().unwrap(),
        )
        .unwrap();
        let mapped = MeshCandidate::new(
            MeshTransport::Tcp,
            MeshCandidateKind::Mapped,
            "198.51.100.30:3031".parse().unwrap(),
        )
        .unwrap();
        let reflexive = MeshCandidate::new(
            MeshTransport::Quic,
            MeshCandidateKind::Reflexive,
            "203.0.113.30:3031".parse().unwrap(),
        )
        .unwrap();
        let mesh =
            MeshConfig::new(node, vec![stale.clone(), mapped.clone(), reflexive.clone()]).unwrap();
        let current: SocketAddr = "192.168.1.16:3031".parse().unwrap();

        let refreshed = refresh_lan_candidates(&mesh, &[current, current]).unwrap();
        assert!(!refreshed.candidates().contains(&stale));
        assert!(refreshed.candidates().contains(&mapped));
        assert!(refreshed.candidates().contains(&reflexive));
        assert!(refreshed.candidates().iter().any(|candidate| {
            candidate.kind() == MeshCandidateKind::Lan && candidate.address() == current
        }));
        assert!(refreshed.candidates().len() <= MAX_MESH_CANDIDATES);
    }
}
