use anyhow::{bail, Context as _, Result};
use feanorfs_common::RelayConfig;
use futures_util::{SinkExt as _, StreamExt as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use url::Url;

const ROUTE_BYTES: usize = 32;
const ROUTE_HEX_LEN: usize = ROUTE_BYTES * 2;
const MAX_RELAY_URL_BYTES: usize = 512;
const FRAME_BYTES: usize = 64 * 1024;
const HOST_OFFER_WORKERS: usize = 4;
const RETRY_DELAY: Duration = Duration::from_secs(1);
const CLIENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READY_TIMEOUT: Duration = Duration::from_secs(2);
const READY_PING: &[u8] = b"feanorfs-tunnel-ready-v1";

type RelaySocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct ClientTunnel {
    server_url: String,
    hostname: String,
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl ClientTunnel {
    pub async fn start(config: &RelayConfig, server_url: &str) -> Result<Self> {
        validate_config(config)?;
        let mut local_url = Url::parse(server_url).context("parse tunneled hub URL")?;
        if local_url.scheme() != "https" {
            bail!("opaque relay tunnels require an HTTPS hub");
        }
        let hostname = local_url
            .host_str()
            .context("tunneled hub URL has no hostname")?
            .to_string();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .context("bind local opaque-tunnel bridge")?;
        let address = listener
            .local_addr()
            .context("read local opaque-tunnel bridge address")?;
        local_url
            .set_port(Some(address.port()))
            .map_err(|_| anyhow::anyhow!("set local opaque-tunnel bridge port"))?;
        let config = config.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let config = config.clone();
                tokio::spawn(async move {
                    if let Err(error) = client_connection(stream, &config).await {
                        tracing::debug!("opaque relay client connection ended: {error:#}");
                    }
                });
            }
        });
        Ok(Self {
            server_url: local_url.to_string().trim_end_matches('/').to_string(),
            hostname,
            address,
            task,
        })
    }

    #[must_use]
    pub fn server_url(&self) -> &str {
        &self.server_url
    }

    #[must_use]
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for ClientTunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn generate_config(relay_url: &str) -> Result<RelayConfig> {
    let mut route = [0_u8; ROUTE_BYTES];
    getrandom::fill(&mut route)
        .map_err(|error| anyhow::anyhow!("generate relay route: {error}"))?;
    let config = RelayConfig {
        url: normalized_base_url(relay_url)?.to_string(),
        route: hex(&route),
    };
    route.fill(0);
    Ok(config)
}

pub fn validate_config(config: &RelayConfig) -> Result<()> {
    normalized_base_url(&config.url)?;
    if config.route.len() != ROUTE_HEX_LEN
        || !config
            .route
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("opaque relay route must be 256-bit lowercase hex");
    }
    Ok(())
}

pub async fn run_host(config: RelayConfig, local_hub: SocketAddr) -> Result<()> {
    validate_config(&config)?;
    let mut workers = JoinSet::new();
    for _ in 0..HOST_OFFER_WORKERS {
        workers.spawn(host_worker(config.clone(), local_hub));
    }
    loop {
        match workers.join_next().await {
            Some(Ok(())) => {}
            Some(Err(error)) => tracing::warn!("opaque relay host worker stopped: {error}"),
            None => bail!("opaque relay host workers stopped unexpectedly"),
        }
        workers.spawn(host_worker(config.clone(), local_hub));
    }
}

async fn host_worker(config: RelayConfig, local_hub: SocketAddr) {
    loop {
        if let Err(error) = host_connection(&config, local_hub).await {
            tracing::debug!("opaque relay host offer ended: {error:#}");
            tokio::time::sleep(RETRY_DELAY).await;
        }
    }
}

async fn host_connection(config: &RelayConfig, local_hub: SocketAddr) -> Result<()> {
    let mut socket = connect(config, "host").await?;
    let first = loop {
        match socket.next().await {
            Some(Ok(Message::Binary(bytes))) if bytes.len() <= FRAME_BYTES => break bytes,
            Some(Ok(Message::Ping(bytes))) => socket
                .send(Message::Pong(bytes))
                .await
                .context("answer opaque relay ping")?,
            Some(Ok(Message::Pong(_))) => {}
            Some(Ok(_)) => bail!("opaque relay sent an invalid host frame"),
            Some(Err(error)) => return Err(error).context("read opaque relay host offer"),
            None => bail!("opaque relay host offer closed before a client connected"),
        }
    };
    let mut stream = TcpStream::connect(local_hub)
        .await
        .context("connect opaque relay to local private hub")?;
    stream
        .write_all(&first)
        .await
        .context("forward initial inner-TLS frame to private hub")?;
    bridge(stream, socket).await
}

async fn client_connection(stream: TcpStream, config: &RelayConfig) -> Result<()> {
    let deadline = tokio::time::Instant::now() + CLIENT_CONNECT_TIMEOUT;
    loop {
        if let Ok(mut socket) = connect(config, "client").await {
            if client_ready(&mut socket).await.is_ok() {
                return bridge(stream, socket).await;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("opaque relay has no available private-hub tunnel");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn client_ready(socket: &mut RelaySocket) -> Result<()> {
    socket
        .send(Message::Ping(READY_PING.to_vec().into()))
        .await
        .context("send opaque relay readiness probe")?;
    tokio::time::timeout(READY_TIMEOUT, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Pong(bytes))) if bytes.as_ref() == READY_PING => return Ok(()),
                Some(Ok(Message::Ping(bytes))) => socket
                    .send(Message::Pong(bytes))
                    .await
                    .context("answer opaque relay readiness ping")?,
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(_)) => bail!("opaque relay sent data before readiness confirmation"),
                Some(Err(error)) => return Err(error).context("read opaque relay readiness"),
                None => bail!("opaque relay closed before readiness confirmation"),
            }
        }
    })
    .await
    .context("opaque relay readiness timed out")?
}

async fn connect(config: &RelayConfig, role: &str) -> Result<RelaySocket> {
    let endpoint = endpoint(config, role)?;
    let (socket, response) = tokio_tungstenite::connect_async(endpoint.as_str())
        .await
        .context("connect to opaque relay")?;
    if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
        bail!("opaque relay refused the WebSocket upgrade");
    }
    Ok(socket)
}

async fn bridge(mut stream: TcpStream, mut socket: RelaySocket) -> Result<()> {
    let mut buffer = vec![0_u8; FRAME_BYTES];
    loop {
        tokio::select! {
            read = stream.read(&mut buffer) => {
                let count = read.context("read inner-TLS stream")?;
                if count == 0 {
                    let _ = socket.close(None).await;
                    return Ok(());
                }
                socket
                    .send(Message::Binary(buffer[..count].to_vec().into()))
                    .await
                    .context("send inner-TLS relay frame")?;
            }
            message = socket.next() => match message {
                Some(Ok(Message::Binary(bytes))) if bytes.len() <= FRAME_BYTES => {
                    stream
                        .write_all(&bytes)
                        .await
                        .context("write inner-TLS stream")?;
                }
                Some(Ok(Message::Ping(bytes))) => {
                    socket
                        .send(Message::Pong(bytes))
                        .await
                        .context("answer opaque relay ping")?;
                }
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Ok(_)) => bail!("opaque relay sent an invalid tunnel frame"),
                Some(Err(error)) => return Err(error).context("read opaque relay frame"),
            }
        }
    }
}

fn endpoint(config: &RelayConfig, role: &str) -> Result<Url> {
    if !matches!(role, "host" | "client") {
        bail!("invalid opaque relay role");
    }
    validate_config(config)?;
    let mut url = normalized_base_url(&config.url)?;
    let base = url.path().trim_end_matches('/');
    url.set_path(&format!(
        "{base}/api/tunnel-relay/{}/{}",
        config.route, role
    ));
    Ok(url)
}

fn normalized_base_url(value: &str) -> Result<Url> {
    if value.len() > MAX_RELAY_URL_BYTES {
        bail!("opaque relay URL is too long");
    }
    let mut url = Url::parse(value).context("parse opaque relay URL")?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("opaque relay URL must not contain credentials, a query, or a fragment");
    }
    let host = url.host_str().context("opaque relay URL has no host")?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    match url.scheme() {
        "https" => url.set_scheme("wss").expect("known scheme"),
        "wss" => {}
        "http" if loopback => url.set_scheme("ws").expect("known scheme"),
        "ws" if loopback => {}
        _ => bail!("opaque relay requires HTTPS/WSS outside loopback tests"),
    }
    Ok(url)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("write to string");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_routes_are_random_256_bit_lowercase_hex() {
        let first = generate_config("https://relay.example/base").unwrap();
        let second = generate_config("https://relay.example/base").unwrap();
        assert_ne!(first.route, second.route);
        assert_eq!(first.route.len(), 64);
        assert!(first
            .route
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert_eq!(first.url, "wss://relay.example/base");
        assert!(endpoint(&first, "client")
            .unwrap()
            .as_str()
            .ends_with(&format!("/api/tunnel-relay/{}/client", first.route)));
    }

    #[test]
    fn public_plaintext_and_malformed_routes_are_rejected() {
        assert!(generate_config("ws://relay.example").is_err());
        assert!(generate_config("http://relay.example").is_err());
        assert!(generate_config("ws://127.0.0.1:3030").is_ok());
        let invalid = RelayConfig {
            url: "wss://relay.example".into(),
            route: "A".repeat(64),
        };
        assert!(validate_config(&invalid).is_err());
    }
}
