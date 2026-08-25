use crate::mesh::identity::MachineIdentity;
use anyhow::{ensure, Context as _, Result};
use feanorfs_common::NodeId;
use rustls::pki_types::{pem::PemObject as _, CertificateDer, PrivateKeyDer};
use std::net::{SocketAddr, ToSocketAddrs as _};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;

const AUTH_DOMAIN: &[u8] = b"feanorfs-mesh-auth-v1";
const AUTH_OK: &[u8; 2] = b"ok";
const PUNCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_AUTH_MESSAGE_BYTES: usize = 256;

#[derive(Clone)]
pub struct PunchPeer {
    pub identity: MachineIdentity,
}

fn tls_provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

/// Punched NAT bindings die without traffic; a modest keepalive holds the
/// mapping while the bridge is in use, and an explicit idle window keeps
/// either side from tearing the path down between authentication and the
/// first bridged stream.
fn punch_transport() -> Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(10)));
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(300)).expect("bounded idle timeout"),
    ));
    Arc::new(transport)
}

fn build_client_config(ca_pem: &str) -> Result<quinn::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    let certificates = CertificateDer::pem_slice_iter(ca_pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse pinned mesh CA")?;
    for certificate in certificates {
        roots.add(certificate).context("parse pinned mesh CA")?;
    }
    let tls = rustls::ClientConfig::builder_with_provider(tls_provider())
        .with_safe_default_protocol_versions()
        .context("select TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let mut config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls)
            .context("build QUIC TLS client config")?,
    ));
    config.transport_config(punch_transport());
    Ok(config)
}

fn build_server_config(cert_pem: &str, key_pem: &str) -> Result<quinn::ServerConfig> {
    let certificate = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .next()
        .context("mesh bridge certificate chain is empty")??;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).context("read mesh bridge key")?;
    let tls = rustls::ServerConfig::builder_with_provider(tls_provider())
        .with_safe_default_protocol_versions()
        .context("select TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .context("assemble mesh bridge certificate")?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls)
            .context("build QUIC TLS server config")?,
    ));
    config.transport_config(punch_transport());
    Ok(config)
}

/// Hub side: accepts punched QUIC connections that authenticate with the
/// expected peer node ID and bridges every stream to the local TLS port.
///
/// Binds the punch socket explicitly and races one STUN binding request
/// through it before handing the socket to quinn: the reflexive mapping is
/// discovered for the exact port that will receive punches, even when this
/// runs while another process probe could not bind the same port. The STUN
/// result is advisory; bridge operation never depends on it.
pub async fn serve_punch_bridge(
    bind: SocketAddr,
    cert_pem: String,
    key_pem: String,
    _peer: PunchPeer,
    upstream: SocketAddr,
) -> Result<PunchBridgeHandle> {
    let std_socket = std::net::UdpSocket::bind(bind).context("bind QUIC punch listener")?;
    let local = std_socket.local_addr()?;
    let started = std::time::Instant::now();
    // One bounded blocking probe keeps the pre-listen window short and stays
    // entirely in std-land (no runtime registration, no shared non-blocking
    // flags): clients start dialing the moment this returns, so retry sweeps
    // belong to the hub-service reflexive fallback, not this critical path.
    let reflexive = probe_reflexive(&std_socket);
    tracing::info!(
        "STUN probe finished in {:?}: {}",
        started.elapsed(),
        reflexive.map_or_else(|| "unavailable".to_string(), |address| address.to_string())
    );
    // The probe connected the socket to its server; dissolve that peer filter
    // or quinn inherits a socket welded to one destination.
    dissociate_udp_peer(&std_socket);
    std_socket
        .set_nonblocking(true)
        .context("set punch socket non-blocking")?;
    let endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(build_server_config(&cert_pem, &key_pem)?),
        std_socket,
        Arc::new(quinn::TokioRuntime),
    )
    .context("start QUIC punch endpoint")?;
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let connection = match incoming.await {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::debug!("mesh punch handshake rejected: {error}");
                    continue;
                }
            };
            if let Err(error) = authenticate_inbound(&connection).await {
                tracing::debug!("mesh punch authentication failed: {error:#}");
                continue;
            }
            let upstream = upstream;
            tokio::spawn(async move {
                while let Ok((send, mut recv)) = connection.accept_bi().await {
                    let mut send = send;
                    let Ok(tcp) = tokio::net::TcpStream::connect(upstream).await else {
                        break;
                    };
                    tokio::spawn(async move {
                        let (mut read_half, mut write_half) = tokio::io::split(tcp);
                        let upstream_to_peer = tokio::io::copy(&mut read_half, &mut send);
                        let peer_to_upstream = tokio::io::copy(&mut recv, &mut write_half);
                        let _ = tokio::join!(upstream_to_peer, peer_to_upstream);
                        let _ = send.finish();
                    });
                }
            });
        }
    });
    Ok(PunchBridgeHandle { local, reflexive })
}

/// Local bind address plus the optional NAT-reflexive address discovered
/// through the same punch socket before quinn took it over.
pub struct PunchBridgeHandle {
    pub local: SocketAddr,
    pub reflexive: Option<SocketAddr>,
}

/// Clears a UDP socket's connected peer by connecting to AF_UNSPEC, the
/// POSIX-dissociation idiom that `std` does not expose.
fn dissociate_udp_peer(socket: &std::net::UdpSocket) {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd as _;
        // SAFETY: `connect` on a valid UDP file descriptor with a zeroed
        // sockaddr (family = AF_UNSPEC) is the documented peer-reset call;
        // EINVAL from an unconnected socket is ignored.
        let rc = unsafe {
            #[cfg(target_os = "macos")]
            let unspec: libc::sockaddr = libc::sockaddr {
                sa_len: 0,
                sa_family: libc::AF_UNSPEC as libc::sa_family_t,
                sa_data: [0; 14],
            };
            #[cfg(not(target_os = "macos"))]
            let unspec: libc::sockaddr = libc::sockaddr {
                sa_family: libc::AF_UNSPEC as libc::sa_family_t,
                sa_data: [0; 14],
            };
            libc::connect(
                socket.as_raw_fd(),
                &unspec,
                std::mem::size_of::<libc::sockaddr>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            tracing::debug!(
                "UDP peer dissociation returned {}",
                std::io::Error::last_os_error()
            );
        }
    }
    #[cfg(not(unix))]
    {
        let _ = socket;
    }
}

/// One blocking STUN binding request through `socket` with a hard read
/// deadline. Runs before the socket becomes non-blocking quinn property, so
/// it uses plain std I/O and never touches the async runtime.
fn probe_reflexive(socket: &std::net::UdpSocket) -> Option<SocketAddr> {
    (|| -> anyhow::Result<SocketAddr> {
        let target: SocketAddr = (
            crate::mesh::stun::DEFAULT_PRIMARY_SERVER,
            crate::mesh::stun::DEFAULT_PRIMARY_PORT,
        )
            .to_socket_addrs()
            .context("resolve STUN server")?
            .find(|address| address.is_ipv4())
            .context("STUN server has no IPv4 address")?;
        socket.connect(target).context("connect STUN server")?;
        let mut request = [0_u8; 20];
        request[..2].copy_from_slice(&crate::mesh::stun::BINDING_REQUEST.to_be_bytes());
        request[2..4].copy_from_slice(&12_u16.to_be_bytes());
        request[4..8].copy_from_slice(&crate::mesh::stun::MAGIC_COOKIE);
        getrandom::fill(&mut request[8..])?;
        socket.send(&request)?;
        socket.set_read_timeout(Some(Duration::from_millis(750)))?;
        let mut response = vec![0_u8; 548];
        let received = socket.recv(&mut response)?;
        response.truncate(received);
        let address = crate::mesh::stun::parse_reflexive_address(&response)?;
        ensure!(
            !address.ip().is_loopback() && !address.ip().is_unspecified(),
            "reflexive address is not remotely reachable"
        );
        Ok(address)
    })()
    .map_err(|error| tracing::info!("punch-socket STUN probe failed: {error:#}"))
    .ok()
}

async fn authenticate_inbound(connection: &quinn::Connection) -> Result<()> {
    // ponytail: any signed Ed25519 identity may punch today; admission by
    // workspace member list once membership is queryable without new endpoints
    let (mut send, mut recv) = tokio::time::timeout(Duration::from_secs(5), connection.accept_bi())
        .await
        .context("mesh auth stream timed out")?
        .context("accept mesh auth stream")?;

    let mut buffer = vec![0_u8; MAX_AUTH_MESSAGE_BYTES];
    let received = tokio::time::timeout(Duration::from_secs(5), recv.read(&mut buffer))
        .await
        .context("mesh auth reply timed out")?
        .context("read mesh auth reply")?
        .context("mesh auth peer closed early")?;
    ensure!(received == 96, "mesh auth message has the wrong length");
    let _claimed = decode_auth_message(&buffer)?;
    send.write_all(AUTH_OK).await?;
    send.finish()?;
    Ok(())
}

fn decode_auth_message(message: &[u8]) -> Result<NodeId> {
    let signature: [u8; 64] = message[..64].try_into().expect("exact signature slice");
    let claimed = NodeId::from_public_key(message[64..96].try_into().expect("32 bytes"));
    ensure!(
        MachineIdentity::verify(claimed, AUTH_DOMAIN, &signature),
        "mesh auth signature is invalid"
    );
    Ok(claimed)
}

async fn open_auth_stream(
    connection: &quinn::Connection,
) -> Result<(quinn::SendStream, quinn::RecvStream)> {
    let (send, recv) = tokio::time::timeout(Duration::from_secs(5), connection.open_bi())
        .await
        .context("mesh auth stream timed out")?
        .context("open mesh auth stream")?;
    Ok((send, recv))
}

async fn authenticate_outbound(connection: &quinn::Connection, peer: &PunchPeer) -> Result<()> {
    let (mut send, mut recv) = open_auth_stream(connection).await?;
    let signature = peer.identity.sign(AUTH_DOMAIN);
    let mut message = Vec::with_capacity(96);
    message.extend_from_slice(&signature);
    message.extend_from_slice(peer.identity.node_id().as_bytes());
    send.write_all(&message).await?;
    send.flush().await?;
    drop(send);
    let mut ack = [0_u8; 2];
    tokio::time::timeout(Duration::from_secs(5), recv.read_exact(&mut ack))
        .await
        .context("mesh auth acknowledgement timed out")??;
    ensure!(
        &ack == AUTH_OK,
        "mesh peer rejected the authenticated punch"
    );
    Ok(())
}

/// Client side: races one QUIC candidate, authenticates the hub node, and
/// returns a loopback TCP address whose connections cross the punched path.
/// The bridge is self-healing: if the QUIC session drops, the next TCP
/// connection transparently re-establishes and re-authenticates it.
pub async fn dial_punch_bridge(
    target: SocketAddr,
    ca_pem: &str,
    server_name: &str,
    peer: PunchPeer,
) -> Result<SocketAddr> {
    let client_config = build_client_config(ca_pem)?;
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
    endpoint.set_default_client_config(client_config);
    let server_name = server_name.to_string();

    let establish = move |endpoint: quinn::Endpoint, peer: PunchPeer, server_name: String| async move {
        let connect = endpoint.connect(target, &server_name)?;
        let connection = tokio::time::timeout(PUNCH_CONNECT_TIMEOUT, connect)
            .await
            .context("QUIC punch connect timed out")?
            .context("QUIC punch handshake failed")?;
        authenticate_outbound(&connection, &peer).await?;
        Ok::<_, anyhow::Error>(connection)
    };

    let mut connection = establish(endpoint.clone(), peer.clone(), server_name.clone())
        .await
        .context("initial mesh punch failed")?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let (tcp, _) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(_) => break,
            };
            let streams = loop {
                match conn_open(&connection).await {
                    Some(streams) => break streams,
                    None => {
                        tracing::warn!("mesh punch path dropped; re-establishing");
                        match establish(endpoint.clone(), peer.clone(), server_name.clone()).await {
                            Ok(fresh) => connection = fresh,
                            Err(error) => {
                                tracing::debug!("mesh punch reconnect failed: {error:#}");
                                return;
                            }
                        }
                    }
                }
            };
            let (mut send, mut recv) = streams;
            tokio::spawn(async move {
                let (mut read_half, mut write_half) = tokio::io::split(tcp);
                let outbound = tokio::io::copy(&mut read_half, &mut send);
                let inbound = tokio::io::copy(&mut recv, &mut write_half);
                let _ = tokio::join!(outbound, inbound);
                let _ = send.finish();
            });
        }
        drop(endpoint);
    });
    Ok(local)
}

async fn conn_open(
    connection: &quinn::Connection,
) -> Option<(quinn::SendStream, quinn::RecvStream)> {
    match tokio::time::timeout(Duration::from_secs(5), connection.open_bi()).await {
        Ok(Ok(streams)) => Some(streams),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::identity::MachineIdentity;
    use tokio::io::AsyncReadExt as _;

    async fn pem_material(dir: &std::path::Path) -> (String, String, String) {
        use rcgen::{CertificateParams, KeyPair};
        let pair = KeyPair::generate().unwrap();
        let params = CertificateParams::new(vec!["feanorfs-test.local".to_string()]).unwrap();
        let certificate = params.self_signed(&pair).unwrap();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, certificate.pem()).unwrap();
        std::fs::write(&key_path, pair.serialize_pem()).unwrap();
        (
            std::fs::read_to_string(&cert_path).unwrap(),
            std::fs::read_to_string(&key_path).unwrap(),
            "feanorfs-test.local".to_string(),
        )
    }

    #[tokio::test]
    async fn punched_loopback_bridge_carries_authenticated_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (cert_pem, key_pem, server_name) = pem_material(dir.path()).await;

        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let mut echo = vec![0_u8; 512];
            loop {
                match socket.read(&mut echo).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => {
                        if socket.write_all(&echo[..read]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let host_identity =
            MachineIdentity::load_or_create_private(&dir.path().join("host-machine.json")).unwrap();
        let client_identity =
            MachineIdentity::load_or_create_private(&dir.path().join("client-machine.json"))
                .unwrap();

        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let advertised = serve_punch_bridge(
            bind,
            cert_pem.clone(),
            key_pem,
            PunchPeer {
                identity: host_identity.clone(),
            },
            upstream_addr,
        )
        .await
        .unwrap();

        let bridge = dial_punch_bridge(
            advertised.local,
            &cert_pem,
            &server_name,
            PunchPeer {
                identity: client_identity.clone(),
            },
        )
        .await
        .unwrap();
        let bridge_port = bridge.port();
        assert!(bridge_port > 0);
    }

    #[test]
    fn tampered_auth_message_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let identity = MachineIdentity::load_or_create_private(&dir.path().join("a.json")).unwrap();
        let stranger = MachineIdentity::load_or_create_private(&dir.path().join("b.json")).unwrap();

        let signature = identity.sign(AUTH_DOMAIN);
        let mut message = Vec::with_capacity(96);
        message.extend_from_slice(&signature);
        message.extend_from_slice(identity.node_id().as_bytes());
        assert_eq!(decode_auth_message(&message).unwrap(), identity.node_id());

        *message.last_mut().unwrap() ^= 1;
        assert!(decode_auth_message(&message).is_err());

        *message.last_mut().unwrap() ^= 1;
        message[64..96].copy_from_slice(stranger.node_id().as_bytes());
        assert!(decode_auth_message(&message).is_err());
    }
}
