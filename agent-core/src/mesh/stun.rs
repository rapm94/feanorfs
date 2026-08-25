use anyhow::{ensure, Context as _, Result};
use std::net::SocketAddr;
use std::time::Duration;

pub(crate) const MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
pub(crate) const BINDING_REQUEST: u16 = 0x0001;
pub(crate) const DEFAULT_PRIMARY_PORT: u16 = 19302;
const BINDING_SUCCESS: u16 = 0x0101;
const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const HEADER_BYTES: usize = 20;
const TRANSACTION_ID_BYTES: usize = 12;
const MAX_RESPONSE_BYTES: usize = 548;
const SERVER_TIMEOUT: Duration = Duration::from_millis(1_200);

pub(crate) const DEFAULT_PRIMARY_SERVER: &str = "stun.l.google.com:19302";

const DEFAULT_SERVERS: [&str; 3] = [
    DEFAULT_PRIMARY_SERVER,
    "stun.cloudflare.com:3478",
    "stun.nextcloud.com:443",
];

/// Discovers the NAT-reflexive UDP address seen by the public internet.
/// All well-known servers are raced concurrently; the first verified reply
/// wins and the rest are abandoned, so the total wait never exceeds one
/// server timeout even when every server is unreachable.
pub async fn discover_reflexive(bind_port: Option<u16>) -> Result<SocketAddr> {
    let bind = match bind_port {
        Some(port) => format!("0.0.0.0:{port}"),
        None => "0.0.0.0:0".to_string(),
    };
    let mut attempts = tokio::task::JoinSet::new();
    for server in DEFAULT_SERVERS {
        let bind = bind.clone();
        attempts.spawn(async move { query_server(&bind, server).await });
    }
    let attempted = attempts.len();
    while let Some(result) = attempts.join_next().await {
        if let Ok(Ok(address)) = result {
            attempts.abort_all();
            return Ok(address);
        }
    }
    anyhow::bail!("none of {attempted} STUN servers reported a reflexive address")
}

async fn query_server(bind: &str, server: &str) -> Result<SocketAddr> {
    let target = resolve_server(server).await?;
    let socket = tokio::net::UdpSocket::bind(bind).await?;
    query_reflexive_over(&socket, target).await
}

pub(crate) async fn resolve_server(server: &str) -> Result<SocketAddr> {
    let server = server.to_string();
    let resolved = tokio::task::spawn_blocking(move || to_socket_addrs_vec(&server))
        .await
        .context("join STUN address resolution")??;
    resolved
        .into_iter()
        .find(|address| address.is_ipv4())
        .context("STUN server has no IPv4 address")
}

/// Sends one binding request through an already-bound socket — required when
/// the punch port is owned by another task and the reflexive mapping must be
/// discovered for that exact local endpoint.
pub(crate) async fn query_reflexive_over(
    socket: &tokio::net::UdpSocket,
    target: SocketAddr,
) -> Result<SocketAddr> {
    socket.connect(target).await?;

    let mut request = [0_u8; HEADER_BYTES];
    request[..2].copy_from_slice(&BINDING_REQUEST.to_be_bytes());
    request[2..4].copy_from_slice(&(TRANSACTION_ID_BYTES as u16).to_be_bytes());
    request[4..8].copy_from_slice(&MAGIC_COOKIE);
    getrandom::fill(&mut request[8..])?;

    socket.send(&request).await?;
    let mut response = vec![0_u8; MAX_RESPONSE_BYTES];
    let received = tokio::time::timeout(SERVER_TIMEOUT, socket.recv(&mut response)).await??;
    response.truncate(received);
    ensure!(
        response.len() >= HEADER_BYTES && response[8..].starts_with(&request[8..]),
        "STUN reply transaction does not match the request"
    );
    parse_reflexive_address(&response)
}

fn to_socket_addrs_vec(server: &str) -> std::io::Result<Vec<SocketAddr>> {
    use std::net::ToSocketAddrs as _;
    server.to_socket_addrs().map(|iter| iter.collect())
}

pub(crate) fn parse_reflexive_address(response: &[u8]) -> Result<SocketAddr> {
    ensure!(
        u16::from_be_bytes([response[0], response[1]]) == BINDING_SUCCESS,
        "STUN server rejected the binding request"
    );
    let length = usize::from(u16::from_be_bytes([response[2], response[3]]));
    ensure!(
        HEADER_BYTES + length <= response.len(),
        "STUN reply header exceeds the datagram"
    );
    let mut cursor = HEADER_BYTES;
    while cursor + 4 <= HEADER_BYTES + length {
        let attribute = u16::from_be_bytes([response[cursor], response[cursor + 1]]);
        let size = usize::from(u16::from_be_bytes([
            response[cursor + 2],
            response[cursor + 3],
        ]));
        let value = response
            .get(cursor + 4..cursor + 4 + size)
            .context("STUN attribute exceeds the datagram")?;
        match attribute {
            ATTR_XOR_MAPPED_ADDRESS => return decode_xor_mapped(value),
            ATTR_MAPPED_ADDRESS => return decode_mapped(value),
            _ => {}
        }
        cursor += 4 + size.next_multiple_of(4);
    }
    anyhow::bail!("STUN reply carries no mapped-address attribute")
}

fn decode_xor_mapped(value: &[u8]) -> Result<SocketAddr> {
    ensure!(value.len() >= 8, "truncated XOR-MAPPED-ADDRESS");
    let port = u16::from_be_bytes([value[2], value[3]])
        ^ u16::from_be_bytes([MAGIC_COOKIE[2], MAGIC_COOKIE[3]]);
    let address = match value[1] {
        0x01 => {
            ensure!(value.len() >= 8, "truncated XOR-MAPPED-ADDRESS");
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                value[4] ^ MAGIC_COOKIE[0],
                value[5] ^ MAGIC_COOKIE[1],
                value[6] ^ MAGIC_COOKIE[2],
                value[7] ^ MAGIC_COOKIE[3],
            ))
        }
        0x02 => {
            ensure!(value.len() >= 20, "truncated XOR-MAPPED-ADDRESS");
            let mut octets = [0_u8; 16];
            for (slot, byte) in octets.iter_mut().zip(value[4..20].iter()) {
                *slot = *byte;
            }
            for (slot, mask) in octets.iter_mut().zip(MAGIC_COOKIE.iter().cycle()) {
                *slot ^= mask;
            }
            std::net::IpAddr::V6(std::net::Ipv6Addr::from(octets))
        }
        family => anyhow::bail!("unknown STUN address family {family:#x}"),
    };
    Ok(SocketAddr::new(address, port))
}

fn decode_mapped(value: &[u8]) -> Result<SocketAddr> {
    ensure!(value.len() >= 8, "truncated MAPPED-ADDRESS");
    let port = u16::from_be_bytes([value[2], value[3]]);
    let address = match value[1] {
        0x01 => std::net::IpAddr::V4(std::net::Ipv4Addr::new(
            value[4], value[5], value[6], value[7],
        )),
        family => anyhow::bail!("unknown STUN address family {family:#x}"),
    };
    Ok(SocketAddr::new(address, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::{MeshCandidate, MeshCandidateKind};

    fn xor_mapped_response(port: u16, ip: [u8; 4]) -> Vec<u8> {
        let mut response = vec![0_u8; HEADER_BYTES];
        response[..2].copy_from_slice(&BINDING_SUCCESS.to_be_bytes());
        response[2..4].copy_from_slice(&12_u16.to_be_bytes());
        response[4..8].copy_from_slice(&MAGIC_COOKIE);
        let mut attribute = vec![0_u8; 8];
        attribute[1] = 0x01;
        attribute[2..4].copy_from_slice(&(port ^ 0xA442).to_be_bytes());
        attribute[4] = ip[0] ^ MAGIC_COOKIE[0];
        attribute[5] = ip[1] ^ MAGIC_COOKIE[1];
        attribute[6] = ip[2] ^ MAGIC_COOKIE[2];
        attribute[7] = ip[3] ^ MAGIC_COOKIE[3];
        response.extend_from_slice(&(ATTR_XOR_MAPPED_ADDRESS).to_be_bytes());
        response.extend_from_slice(&(attribute.len() as u16).to_be_bytes());
        response.extend_from_slice(&attribute);
        response
    }

    #[test]
    fn parses_xor_mapped_ipv4_reply() {
        let response = xor_mapped_response(40000, [198, 51, 100, 7]);
        let address = parse_reflexive_address(&response).unwrap();
        assert_eq!(address, "198.51.100.7:40000".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn rejects_wrong_message_type_and_truncated_attributes() {
        let mut response = xor_mapped_response(40000, [198, 51, 100, 7]);
        response[1] = 0x11;
        assert!(parse_reflexive_address(&response).is_err());

        let mut truncated = xor_mapped_response(40000, [198, 51, 100, 7]);
        truncated.truncate(truncated.len() - 2);
        assert!(parse_reflexive_address(&truncated).is_err());
    }

    #[test]
    fn reflexive_candidates_require_quic_transport() {
        assert!(MeshCandidate::new(
            feanorfs_common::MeshTransport::Tcp,
            MeshCandidateKind::Reflexive,
            "198.51.100.7:40000".parse().unwrap(),
        )
        .is_err());
    }
}
