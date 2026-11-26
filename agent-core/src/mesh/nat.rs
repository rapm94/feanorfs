use anyhow::{ensure, Context as _, Result};
use feanorfs_common::{MeshCandidate, MeshCandidateKind, MeshTransport};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

const GATEWAY_SEARCH_TIMEOUT: Duration = Duration::from_secs(2);
pub const MAPPING_LEASE_SECS: u32 = 30 * 60;
pub const MAPPING_RENEW_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(MAPPING_LEASE_SECS as u64 / 2);
const MAPPING_DESCRIPTION: &str = "FeanorFS mesh";

pub async fn map_tcp_port(
    internal: IpAddr,
    internal_port: u16,
    external_port: Option<u16>,
) -> Result<MeshCandidate> {
    ensure!(internal_port != 0, "mesh NAT mapping requires a real port");
    ensure!(
        !internal.is_loopback() && !internal.is_unspecified(),
        "mesh NAT mapping requires a routable internal address"
    );
    tokio::task::spawn_blocking(move || map_blocking(internal, internal_port, external_port))
        .await
        .context("join NAT gateway search")?
}

fn map_blocking(
    internal: IpAddr,
    internal_port: u16,
    external_port: Option<u16>,
) -> Result<MeshCandidate> {
    let options = igd_next::SearchOptions {
        timeout: Some(GATEWAY_SEARCH_TIMEOUT),
        ..igd_next::SearchOptions::default()
    };
    let gateway = igd_next::search_gateway(options)?;
    let external_ip = gateway
        .get_external_ip()
        .context("read NAT gateway external address")?;
    let external_port = match external_port {
        Some(external) => {
            ensure!(external != 0, "external port must be non-zero");
            gateway.add_port(
                igd_next::PortMappingProtocol::TCP,
                external,
                SocketAddr::new(internal, internal_port),
                MAPPING_LEASE_SECS,
                MAPPING_DESCRIPTION,
            )?;
            external
        }
        None => gateway.add_any_port(
            igd_next::PortMappingProtocol::TCP,
            SocketAddr::new(internal, internal_port),
            MAPPING_LEASE_SECS,
            MAPPING_DESCRIPTION,
        )?,
    };
    MeshCandidate::new(
        MeshTransport::Tcp,
        MeshCandidateKind::Mapped,
        SocketAddr::new(external_ip, external_port),
    )
}

pub async fn release_tcp_port(external_port: u16) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let options = igd_next::SearchOptions {
            timeout: Some(GATEWAY_SEARCH_TIMEOUT),
            ..igd_next::SearchOptions::default()
        };
        let gateway = igd_next::search_gateway(options)?;
        gateway.remove_port(igd_next::PortMappingProtocol::TCP, external_port)?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("join NAT gateway search")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_ports_and_internal_addresses_without_touching_the_network() {
        assert!(map_tcp_port("192.168.1.13".parse().unwrap(), 0, None)
            .await
            .is_err());
        assert!(map_tcp_port("127.0.0.1".parse().unwrap(), 3030, None)
            .await
            .is_err());
        assert!(map_tcp_port("0.0.0.0".parse().unwrap(), 3030, None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn rejects_zero_external_port_without_touching_the_network() {
        assert!(map_tcp_port("192.168.1.13".parse().unwrap(), 3030, Some(0))
            .await
            .is_err());
    }

    #[tokio::test]
    #[ignore = "requires a NAT gateway with UPnP or NAT-PMP enabled"]
    async fn maps_a_real_gateway_port() {
        let candidate = map_tcp_port("192.168.1.13".parse().unwrap(), 3030, None)
            .await
            .unwrap();
        assert_eq!(candidate.kind(), MeshCandidateKind::Mapped);
        assert_eq!(candidate.transport(), MeshTransport::Tcp);
    }
}
