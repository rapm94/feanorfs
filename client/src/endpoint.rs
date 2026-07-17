use anyhow::Context as _;
use feanorfs_agent_core::ApiClient;
use feanorfs_common::{hub_ca_fingerprint, hub_mdns_hostname, HUB_MDNS_SERVICE};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::local::{load_global_config, save_config, save_global_config, Config};

const PROBE_TIMEOUT: Duration = Duration::from_millis(900);
const DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1200);

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
                    && info
                        .get_hostname()
                        .trim_end_matches('.')
                        .eq_ignore_ascii_case(hostname)
                    && info.get_port() == port =>
            {
                addresses.extend(
                    info.get_addresses_v4()
                        .into_iter()
                        .map(|address| SocketAddr::new(IpAddr::V4(address), port)),
                );
                break;
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
}
