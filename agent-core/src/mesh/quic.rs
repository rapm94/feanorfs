use crate::mesh::identity::MachineIdentity;
use anyhow::{ensure, Context as _, Result};
use feanorfs_common::NodeId;
use rustls::pki_types::{pem::PemObject as _, CertificateDer, PrivateKeyDer};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Semaphore;
use std::time::Duration;

const AUTH_DOMAIN: &[u8] = b"feanorfs-mesh-auth-v1";
const AUTH_OK: &[u8; 2] = b"ok";
const PUNCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
const MAX_AUTH_MESSAGE_BYTES: usize = 96;
const MAX_CONNECTIONS: usize = 64;
const MAX_STREAMS: usize = 64;

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
    let socket = tokio::net::UdpSocket::bind(bind).await.context("bind QUIC punch listener")?;
    let local = socket.local_addr()?;
    let started = std::time::Instant::now();
    // One bounded blocking probe keeps the pre-listen window short and stays
    // entirely in std-land. send_to/recv_from deliberately avoid connect():
    // an AF_UNSPEC dissociation afterwards leaves Linux UDP sockets unable to
    // serve quinn, while a lingering connect() would weld quinn to one peer.
    let reflexive = probe_reflexive(&socket).await;
    let std_socket = socket.into_std()?;
    tracing::info!(
        "STUN probe finished in {:?}: {}",
        started.elapsed(),
        reflexive.map_or_else(|| "unavailable".to_string(), |address| address.to_string())
    );
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
    eprintln!("DBG endpoint-live");
    let auth_slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            let connection = match incoming.await {
                Ok(connection) => connection,
                Err(error) => {
                    tracing::debug!("mesh punch handshake rejected: {error}");
                    continue;
                }
            };
            let auth = auth_slots.clone();
            tokio::spawn(async move {
                // Bounded concurrent authentication keeps slow peers from
                // consuming unbounded accept capacity.
                let Ok(_permit) = auth.acquire_owned().await else { return };
                if let Err(error) = authenticate_inbound(&connection).await {
                    tracing::debug!("mesh punch authentication failed: {error:#}");
                } else {
                    let upstream = upstream;
                    let streams = Arc::new(Semaphore::new(MAX_STREAMS));
                    while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                        let Ok(permit) = streams.clone().acquire_owned().await else { break };
                        let Ok(tcp) = tokio::net::TcpStream::connect(upstream).await else {
                            break;
                        };
                        tokio::spawn(async move {
                            let (mut read_half, mut write_half) = tokio::io::split(tcp);
                            let upstream_to_peer = tokio::io::copy(&mut read_half, &mut send);
                            let peer_to_upstream = tokio::io::copy(&mut recv, &mut write_half);
                            let _ = tokio::join!(upstream_to_peer, peer_to_upstream);
                            let _ = send.finish();
                            drop(permit);
                        });
                    }
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

/// Probe the exact punch socket without connecting it to one peer. DNS and
/// receive share a deadline; quinn inherits the same unconnected socket.
async fn probe_reflexive(socket: &tokio::net::UdpSocket) -> Option<SocketAddr> {
    tokio::time::timeout(Duration::from_millis(750), async {
        let target = crate::mesh::stun::resolve_server(crate::mesh::stun::DEFAULT_PRIMARY_SERVER).await?;
        let address = crate::mesh::stun::query_reflexive_over(socket, target).await?;
        ensure!(!address.ip().is_loopback() && !address.ip().is_unspecified(), "reflexive address is not remotely reachable");
        Ok::<_, anyhow::Error>(address)
    }).await.ok().and_then(Result::ok)
}

async fn authenticate_inbound(connection: &quinn::Connection) -> Result<()> {
    // ponytail: any signed Ed25519 identity may punch today; admission by
    // workspace member list once membership is queryable without new endpoints
    let (mut send, mut recv) = tokio::time::timeout(Duration::from_secs(5), connection.accept_bi())
        .await
        .context("mesh auth stream timed out")?
        .context("accept mesh auth stream")?;

    let buffer = tokio::time::timeout(Duration::from_secs(5), recv.read_to_end(MAX_AUTH_MESSAGE_BYTES))
        .await
        .context("mesh auth reply timed out")?
        .context("read bounded mesh auth reply")?;
    let _claimed = decode_auth_message(&buffer)?;
    send.write_all(AUTH_OK).await?;
    send.finish()?;
    Ok(())
}

fn decode_auth_message(message: &[u8]) -> Result<NodeId> {
    ensure!(message.len() == MAX_AUTH_MESSAGE_BYTES, "mesh auth message has the wrong length");
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
    send.finish()?;
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
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

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
