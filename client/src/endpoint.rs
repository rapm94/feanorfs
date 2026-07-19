use anyhow::Context as _;
use feanorfs_agent_core::ApiClient;
use feanorfs_common::{hub_ca_fingerprint, hub_mdns_hostname, HUB_MDNS_SERVICE};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs as _};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::local::{load_global_config, save_config, save_global_config, Config};

const PROBE_TIMEOUT: Duration = Duration::from_millis(900);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

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

    let original = ApiClient::from_config_direct(workspace, config).await?;
    let Some(stable) = stable_endpoint(config) else {
        return Ok(original);
    };

    let direct = ApiClient::new_with_tls(
        &stable.url,
        config.server_password.as_deref(),
        config.tls_ca_pem.as_deref(),
    )?;
    if probe(&direct).await {
        persist_stable_url(workspace, config, &stable.url);
        return Ok(direct);
    }

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
            persist_stable_url(workspace, config, &stable.url);
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
    if config.server_url == url {
        return;
    }
    let mut updated = config.clone();
    updated.server_url = url.to_string();
    if let Err(error) = save_config(workspace, &updated) {
        tracing::warn!("could not persist stable private-hub endpoint: {error}");
        return;
    }
    if let Ok(mut global) = load_global_config() {
        if global.server_url == config.server_url && global.tls_ca_pem == config.tls_ca_pem {
            global.server_url = url.to_string();
            if let Err(error) = save_global_config(&global) {
                tracing::warn!("could not persist stable global hub endpoint: {error}");
            }
        }
    }
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
}
