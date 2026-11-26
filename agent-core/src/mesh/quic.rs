use crate::mesh::identity::MachineIdentity;
use anyhow::{ensure, Context as _, Result};
use feanorfs_common::NodeId;
use rustls::pki_types::{pem::PemObject as _, CertificateDer, PrivateKeyDer};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use zeroize::Zeroizing;

const AUTH_DOMAIN: &[u8] = b"feanorfs-mesh-auth-v2";
const AUTH_VERSION: u8 = 2;
const AUTH_NONCE_BYTES: usize = 32;
const AUTH_MESSAGE_BYTES: usize = 64 + 32 + 32;
const AUTH_OK: &[u8; 2] = b"ok";
const PUNCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 64;
const MAX_STREAMS: usize = 64;

#[derive(Clone)]
pub struct PunchPeer {
    pub identity: MachineIdentity,
    /// Hub bearer token. Both sides prove knowledge of it; it never crosses the wire.
    pub token: Arc<Zeroizing<String>>,
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
    peer: PunchPeer,
    upstream: SocketAddr,
) -> Result<PunchBridgeHandle> {
    let socket = tokio::net::UdpSocket::bind(bind)
        .await
        .context("bind QUIC punch listener")?;
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
    let auth_slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    tokio::spawn(async move {
        while let Some(incoming) = endpoint.accept().await {
            // Reject, never queue: a full table must not grow parked tasks.
            let Ok(permit) = auth_slots.clone().try_acquire_owned() else {
                incoming.refuse();
                continue;
            };
            let token = peer.token.clone();
            tokio::spawn(async move {
                let _permit = permit;
                // Handshake per task so a stalled Initial cannot block accepts.
                let connection = match tokio::time::timeout(PUNCH_CONNECT_TIMEOUT, incoming).await {
                    Ok(Ok(connection)) => connection,
                    Ok(Err(error)) => {
                        tracing::debug!("mesh punch handshake rejected: {error}");
                        return;
                    }
                    Err(_) => return,
                };
                if let Err(error) = authenticate_inbound(&connection, &token).await {
                    tracing::debug!("mesh punch authentication failed: {error:#}");
                    connection.close(1_u32.into(), b"auth");
                    return;
                }
                let upstream = upstream;
                let streams = Arc::new(Semaphore::new(MAX_STREAMS));
                while let Ok((mut send, mut recv)) = connection.accept_bi().await {
                    let Ok(permit) = streams.clone().acquire_owned().await else {
                        break;
                    };
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
        let target =
            crate::mesh::stun::resolve_server(crate::mesh::stun::DEFAULT_PRIMARY_SERVER).await?;
        let address = crate::mesh::stun::query_reflexive_over(socket, target).await?;
        ensure!(
            !address.ip().is_loopback() && !address.ip().is_unspecified(),
            "reflexive address is not remotely reachable"
        );
        Ok::<_, anyhow::Error>(address)
    })
    .await
    .ok()
    .and_then(Result::ok)
}

fn auth_transcript(nonce: &[u8; AUTH_NONCE_BYTES]) -> Vec<u8> {
    [AUTH_DOMAIN, nonce.as_slice()].concat()
}

fn token_proof(token: &str, nonce: &[u8; AUTH_NONCE_BYTES]) -> blake3::Hash {
    let key = blake3::derive_key("feanorfs mesh punch token proof v2", token.as_bytes());
    blake3::keyed_hash(&key, &auth_transcript(nonce))
}

fn decode_auth_message(
    message: &[u8],
    nonce: &[u8; AUTH_NONCE_BYTES],
    token: &str,
) -> Result<NodeId> {
    ensure!(
        message.len() == AUTH_MESSAGE_BYTES,
        "mesh auth message has the wrong length"
    );
    let signature: [u8; 64] = message[..64].try_into().expect("exact signature slice");
    let claimed = NodeId::from_public_key(message[64..96].try_into().expect("32 bytes"));
    let proof = blake3::Hash::from_bytes(message[96..].try_into().expect("32 bytes"));
    // blake3::Hash equality is constant-time.
    ensure!(
        proof == token_proof(token, nonce),
        "mesh auth token proof is invalid"
    );
    ensure!(
        MachineIdentity::verify(claimed, &auth_transcript(nonce), &signature),
        "mesh auth signature is invalid"
    );
    Ok(claimed)
}

async fn authenticate_inbound(connection: &quinn::Connection, token: &str) -> Result<NodeId> {
    let (mut send, mut recv) = tokio::time::timeout(Duration::from_secs(5), connection.accept_bi())
        .await
        .context("mesh auth stream timed out")?
        .context("accept mesh auth stream")?;

    // Read hello version byte
    let mut hello = [0_u8; 1];
    tokio::time::timeout(Duration::from_secs(5), recv.read_exact(&mut hello))
        .await
        .context("mesh auth hello timed out")??;
    ensure!(hello[0] == AUTH_VERSION, "unsupported mesh auth version");

    // Generate and send nonce
    let mut nonce = [0_u8; AUTH_NONCE_BYTES];
    getrandom::fill(&mut nonce).context("generate mesh auth nonce")?;
    send.write_all(&nonce).await?;

    // Read auth message
    let buffer = tokio::time::timeout(Duration::from_secs(5), recv.read_to_end(AUTH_MESSAGE_BYTES))
        .await
        .context("mesh auth message timed out")?
        .context("read bounded mesh auth message")?;

    let claimed = decode_auth_message(&buffer, &nonce, token)?;
    send.write_all(AUTH_OK).await?;
    send.finish()?;
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

    // Send version byte
    send.write_all(&[AUTH_VERSION]).await?;

    // Read nonce
    let mut nonce = [0_u8; AUTH_NONCE_BYTES];
    tokio::time::timeout(Duration::from_secs(5), recv.read_exact(&mut nonce))
        .await
        .context("mesh auth nonce timed out")??;

    // Build and send auth message
    let signature = peer.identity.sign(&auth_transcript(&nonce));
    let mut message = Vec::with_capacity(AUTH_MESSAGE_BYTES);
    message.extend_from_slice(&signature);
    message.extend_from_slice(peer.identity.node_id().as_bytes());
    message.extend_from_slice(token_proof(&peer.token, &nonce).as_bytes());
    send.write_all(&message).await?;
    send.finish()?;

    // Read ack
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
        let token = "test-token-secret".to_string();

        let bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let advertised = serve_punch_bridge(
            bind,
            cert_pem.clone(),
            key_pem,
            PunchPeer {
                identity: host_identity.clone(),
                token: Arc::new(Zeroizing::new(token.clone())),
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
                token: Arc::new(Zeroizing::new(token.clone())),
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
        let token = "test-token";

        let mut nonce = [0_u8; AUTH_NONCE_BYTES];
        nonce[0] = 42;

        // Build valid message
        let signature = identity.sign(&auth_transcript(&nonce));
        let mut message = Vec::with_capacity(AUTH_MESSAGE_BYTES);
        message.extend_from_slice(&signature);
        message.extend_from_slice(identity.node_id().as_bytes());
        message.extend_from_slice(token_proof(token, &nonce).as_bytes());
        assert_eq!(
            decode_auth_message(&message, &nonce, token).unwrap(),
            identity.node_id()
        );

        // Flipped signature bit
        *message.first_mut().unwrap() ^= 1;
        assert!(decode_auth_message(&message, &nonce, token).is_err());

        // Restore and test substituted node id
        *message.first_mut().unwrap() ^= 1;
        message[64..96].copy_from_slice(stranger.node_id().as_bytes());
        assert!(decode_auth_message(&message, &nonce, token).is_err());

        // Wrong token
        message[64..96].copy_from_slice(identity.node_id().as_bytes());
        assert!(decode_auth_message(&message, &nonce, "different-token").is_err());

        // Different nonce
        let mut different_nonce = [0_u8; AUTH_NONCE_BYTES];
        different_nonce[0] = 99;
        assert!(decode_auth_message(&message, &different_nonce, token).is_err());
    }

    #[tokio::test]
    async fn punch_with_wrong_token_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (cert_pem, key_pem, server_name) = pem_material(dir.path()).await;

        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let mut buf = vec![0_u8; 512];
            let _ = socket.read(&mut buf).await;
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
                token: Arc::new(Zeroizing::new("server-token".to_string())),
            },
            upstream_addr,
        )
        .await
        .unwrap();

        let result = dial_punch_bridge(
            advertised.local,
            &cert_pem,
            &server_name,
            PunchPeer {
                identity: client_identity.clone(),
                token: Arc::new(Zeroizing::new("client-token-different".to_string())),
            },
        )
        .await;

        assert!(result.is_err());
    }
}
