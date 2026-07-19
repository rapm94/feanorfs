use anyhow::{bail, Context as _};
use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use feanorfs_client::{Config, WorkspaceInvite};
use futures_util::{SinkExt as _, StreamExt as _};
use mdns_sd::{DaemonEvent, ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use zeroize::{Zeroize as _, ZeroizeOnDrop, Zeroizing};

use super::util::{copy_to_clipboard, invite_from_config};

pub const PAIR_PREFIX: &str = "fnp1-";
pub const RELAY_PAIR_PREFIX: &str = "fnp2-";
const PAIR_SERVICE: &str = "_feanorfs-pair._tcp.local.";
const CODE_ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
const PROTOCOL_ID: &[u8] = b"feanorfs-lan-pair-v1";
const RELAY_PROTOCOL_ID: &[u8] = b"feanorfs-relay-pair-v1";
const CLIENT_ID: &[u8] = b"feanorfs-pair-client-v1";
const INVITE_AAD: &[u8] = b"feanorfs-pair-invite-v1";
const ACK_AAD: &[u8] = b"feanorfs-pair-ack-v1";
const ACK: &[u8] = b"paired";
const MAX_FRAME: usize = 16 * 1024;
const MAX_ATTEMPTS: usize = 3;
const MAX_RELAY_URL_BYTES: usize = 256;
const MAX_RELAY_CODE_BYTES: usize = 900;
const MDNS_ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairPresentation {
    Human,
    Tray,
}

#[derive(Serialize)]
struct PairReadyEvent<'a> {
    event: &'static str,
    code: &'a str,
    expires_in_seconds: u64,
}

#[derive(Serialize)]
struct PairCompleteEvent {
    event: &'static str,
}

#[derive(Clone, PartialEq, Eq, zeroize::Zeroize, ZeroizeOnDrop)]
pub struct PairCode {
    canonical: String,
    tag: String,
    relay: Option<RelayPair>,
}

#[derive(Clone, PartialEq, Eq, zeroize::Zeroize, ZeroizeOnDrop)]
struct RelayPair {
    url: String,
    session: String,
}

#[derive(Serialize, Deserialize, zeroize::Zeroize, ZeroizeOnDrop)]
struct RelayPairPayload {
    relay_url: String,
    session: String,
    secret: String,
}

impl PairCode {
    pub fn generate() -> anyhow::Result<Self> {
        let characters = generate_pair_secret()?;
        let code = Zeroizing::new(format_pair_secret(PAIR_PREFIX, &characters));
        Self::parse(&code)
    }

    pub fn generate_relay(relay_url: &str) -> anyhow::Result<Self> {
        let relay_url = normalize_relay_url(relay_url)?;
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|error| anyhow::anyhow!("generate relay session ID: {error}"))?;
        let session = hex_encode(&random);
        random.zeroize();
        let payload = RelayPairPayload {
            relay_url,
            session,
            secret: generate_pair_secret()?.to_string(),
        };
        let encoded = Zeroizing::new(
            serde_json::to_vec(&payload).context("encode relay pairing capability")?,
        );
        let encoded_hex = Zeroizing::new(hex_encode(&encoded));
        let code = Zeroizing::new(format!("{RELAY_PAIR_PREFIX}{}", encoded_hex.as_str()));
        Self::parse(&code)
    }

    pub fn parse(input: &str) -> anyhow::Result<Self> {
        let trimmed = input.trim();
        if trimmed
            .get(..RELAY_PAIR_PREFIX.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(RELAY_PAIR_PREFIX))
        {
            return Self::parse_relay(input.trim());
        }
        let uppercase = Zeroizing::new(trimmed.to_ascii_uppercase());
        let body = uppercase
            .strip_prefix("FNP1-")
            .context("pairing code must start with fnp1-")?;
        let groups: Vec<&str> = body.split('-').collect();
        if groups.len() != 4 || groups.iter().any(|group| group.len() != 4) {
            bail!("pairing code must have four groups of four characters");
        }
        if !groups
            .iter()
            .all(|group| group.bytes().all(|byte| CODE_ALPHABET.contains(&byte)))
        {
            bail!("pairing code contains an invalid or ambiguous character");
        }
        Ok(Self {
            canonical: format!("{PAIR_PREFIX}{}", groups.join("-")),
            tag: groups[0].to_string(),
            relay: None,
        })
    }

    fn parse_relay(input: &str) -> anyhow::Result<Self> {
        if input.len() > MAX_RELAY_CODE_BYTES {
            bail!("relay pairing capability is too long");
        }
        let encoded = input
            .get(RELAY_PAIR_PREFIX.len()..)
            .context("relay pairing capability must start with fnp2-")?;
        let decoded = Zeroizing::new(hex_decode(encoded)?);
        let payload: RelayPairPayload =
            serde_json::from_slice(&decoded).context("invalid relay pairing capability")?;
        let relay_url = normalize_relay_url(&payload.relay_url)?;
        validate_relay_session(&payload.session)?;
        validate_pair_secret(&payload.secret)?;
        let normalized = RelayPairPayload {
            relay_url: relay_url.clone(),
            session: payload.session.clone(),
            secret: payload.secret.to_ascii_uppercase(),
        };
        let encoded = Zeroizing::new(
            serde_json::to_vec(&normalized).context("normalize relay pairing code")?,
        );
        let encoded_hex = Zeroizing::new(hex_encode(&encoded));
        Ok(Self {
            canonical: format!("{RELAY_PAIR_PREFIX}{}", encoded_hex.as_str()),
            tag: payload.session.clone(),
            relay: Some(RelayPair {
                url: relay_url,
                session: payload.session.clone(),
            }),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.canonical
    }

    fn host_identity(&self) -> Identity {
        let protocol = if self.relay.is_some() {
            RELAY_PROTOCOL_ID
        } else {
            PROTOCOL_ID
        };
        Identity::new(format!("{}:{}", String::from_utf8_lossy(protocol), self.tag).as_bytes())
    }

    fn relay_endpoint(&self, role: &str) -> anyhow::Result<Option<String>> {
        let Some(relay) = &self.relay else {
            return Ok(None);
        };
        let mut url = reqwest::Url::parse(&relay.url).context("parse pairing relay URL")?;
        let base = url.path().trim_end_matches('/');
        url.set_path(&format!("{base}/api/pair-relay/{}/{role}", relay.session));
        Ok(Some(url.to_string()))
    }
}

pub fn looks_like_pair_code(value: &str) -> bool {
    [PAIR_PREFIX, RELAY_PAIR_PREFIX].iter().any(|expected| {
        value
            .get(..expected.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(expected))
    })
}

fn generate_pair_secret() -> anyhow::Result<Zeroizing<String>> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("generate secure pairing code: {error}"))?;
    let secret = random
        .iter()
        .map(|byte| char::from(CODE_ALPHABET[usize::from(byte & 31)]))
        .collect();
    random.zeroize();
    Ok(Zeroizing::new(secret))
}

fn format_pair_secret(prefix: &str, secret: &str) -> String {
    format!(
        "{prefix}{}-{}-{}-{}",
        &secret[0..4],
        &secret[4..8],
        &secret[8..12],
        &secret[12..16]
    )
}

fn validate_pair_secret(secret: &str) -> anyhow::Result<()> {
    if secret.len() != 16
        || !secret
            .bytes()
            .all(|byte| CODE_ALPHABET.contains(&byte.to_ascii_uppercase()))
    {
        bail!("relay pairing secret must contain 16 unambiguous characters");
    }
    Ok(())
}

fn validate_relay_session(session: &str) -> anyhow::Result<()> {
    if session.len() != 32
        || !session
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("relay pairing session must be a 128-bit lowercase hexadecimal ID");
    }
    Ok(())
}

fn normalize_relay_url(value: &str) -> anyhow::Result<String> {
    if value.len() > MAX_RELAY_URL_BYTES {
        bail!("pairing relay URL exceeds {MAX_RELAY_URL_BYTES} bytes");
    }
    let mut url = reqwest::Url::parse(value).context("parse pairing relay URL")?;
    let host = url.host_str().context("pairing relay URL has no host")?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    match url.scheme() {
        "https" => url.set_scheme("wss").expect("wss is a valid URL scheme"),
        "http" if loopback => url.set_scheme("ws").expect("ws is a valid URL scheme"),
        "wss" => {}
        "ws" if loopback => {}
        _ => bail!("pairing relay must use wss:// (ws:// is allowed only on loopback for tests)"),
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("pairing relay URL cannot contain credentials, a query, or a fragment");
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_decode(encoded: &str) -> anyhow::Result<Zeroizing<Vec<u8>>> {
    if !encoded.len().is_multiple_of(2) {
        bail!("relay pairing capability has an invalid hexadecimal length");
    }
    let decoded = (0..encoded.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&encoded[index..index + 2], 16)
                .context("relay pairing capability is not valid hexadecimal")
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Zeroizing::new(decoded))
}

struct MdnsRegistration {
    daemon: ServiceDaemon,
    fullname: String,
}

type RelayWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

enum PairChannel {
    Tcp(TcpStream),
    Relay(Box<RelayWebSocket>),
}

impl PairChannel {
    async fn send(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        if bytes.len() > MAX_FRAME {
            bail!("pairing frame exceeds {MAX_FRAME} bytes");
        }
        match self {
            Self::Tcp(stream) => {
                stream.write_u32(bytes.len() as u32).await?;
                stream.write_all(bytes).await?;
                stream.flush().await?;
            }
            Self::Relay(socket) => {
                socket
                    .send(WebSocketMessage::Binary(bytes.to_vec().into()))
                    .await
                    .context("send pairing relay frame")?;
            }
        }
        Ok(())
    }

    async fn receive(&mut self) -> anyhow::Result<Vec<u8>> {
        match self {
            Self::Tcp(stream) => {
                let length = stream.read_u32().await? as usize;
                if length > MAX_FRAME {
                    bail!("pairing frame exceeds {MAX_FRAME} bytes");
                }
                let mut bytes = vec![0_u8; length];
                stream.read_exact(&mut bytes).await?;
                Ok(bytes)
            }
            Self::Relay(socket) => loop {
                match socket.next().await {
                    Some(Ok(WebSocketMessage::Binary(bytes))) if bytes.len() <= MAX_FRAME => {
                        return Ok(bytes.to_vec());
                    }
                    Some(Ok(WebSocketMessage::Binary(_))) => {
                        bail!("pairing frame exceeds {MAX_FRAME} bytes");
                    }
                    Some(Ok(WebSocketMessage::Ping(bytes))) => {
                        socket.send(WebSocketMessage::Pong(bytes)).await?;
                    }
                    Some(Ok(WebSocketMessage::Pong(_))) => {}
                    Some(Ok(WebSocketMessage::Close(_))) | None => {
                        bail!("pairing relay closed before the exchange completed");
                    }
                    Some(Ok(WebSocketMessage::Text(_) | WebSocketMessage::Frame(_))) => {
                        bail!("pairing relay sent a non-binary protocol frame");
                    }
                    Some(Err(error)) => return Err(error).context("read pairing relay frame"),
                }
            },
        }
    }
}

impl Drop for MdnsRegistration {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

pub async fn offer(
    workspace: &Path,
    timeout: Duration,
    presentation: PairPresentation,
    relay_url: Option<&str>,
) -> anyhow::Result<()> {
    let mut config = feanorfs_client::load_config(workspace)?;
    if config.is_local_hub() {
        let hub_dir = config.hub_data_dir(workspace)?;
        bail!(
            "This workspace uses an embedded local hub. Run `feanorfs serve --data-dir {}` \
             first, then relink it to that LAN hub before pairing.",
            hub_dir.display()
        );
    }
    if let Some(relay_url) = relay_url {
        config =
            super::hub_service::configure_relay_for_pairing(workspace, &config, relay_url).await?;
    } else {
        super::hub_service::refresh_for_pairing(&config).await?;
    }
    ensure_hub_reachable(workspace, &config).await?;
    let mut invite = invite_from_config(&config).context("workspace has no encryption key")?;
    invite.ignore_policy = Some(
        feanorfs_client::join_preflight::read_ignore_policy(workspace)
            .context("read mirror ignore policy before pairing")?,
    );
    let effective_relay = relay_url
        .map(str::to_owned)
        .or_else(|| invite.relay.as_ref().map(|relay| relay.url.clone()));
    if let Some(relay_url) = effective_relay {
        return offer_via_relay(&invite, &relay_url, timeout, presentation).await;
    }
    let code = PairCode::generate()?;
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))
        .await
        .context("open LAN pairing listener")?;
    let port = listener.local_addr()?.port();
    let _registration = advertise(&code, port).await?;

    present_code(&code, timeout, presentation)?;
    copy_to_clipboard(code.as_str());
    if presentation == PairPresentation::Human {
        println!("Copied pairing code to clipboard. Waiting…");
    }

    let deadline = tokio::time::Instant::now() + timeout;
    for attempt in 1..=MAX_ATTEMPTS {
        let (stream, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .context("Pairing code expired before another computer connected")??;
        let local_address = stream.local_addr()?.ip();
        match tokio::time::timeout(
            Duration::from_secs(15),
            host_exchange(
                PairChannel::Tcp(stream),
                &code,
                &invite,
                Some(local_address),
            ),
        )
        .await
        {
            Ok(Ok(())) => {
                match presentation {
                    PairPresentation::Human => println!(
                        "Pairing credentials delivered. The other computer is connecting now."
                    ),
                    PairPresentation::Tray => {
                        write_tray_event(&PairCompleteEvent { event: "paired" })?
                    }
                }
                return Ok(());
            }
            Ok(Err(error)) => {
                tracing::warn!("Rejected pairing attempt: {error:#}");
                if attempt < MAX_ATTEMPTS {
                    eprintln!(
                        "Pairing attempt rejected; still waiting ({attempt}/{MAX_ATTEMPTS})."
                    );
                }
            }
            Err(_) if attempt < MAX_ATTEMPTS => {
                eprintln!("Pairing attempt timed out; still waiting ({attempt}/{MAX_ATTEMPTS}).");
            }
            Err(_) => {}
        }
    }
    bail!("Pairing stopped after {MAX_ATTEMPTS} rejected or incomplete attempts")
}

async fn offer_via_relay(
    invite: &WorkspaceInvite,
    relay_url: &str,
    timeout: Duration,
    presentation: PairPresentation,
) -> anyhow::Result<()> {
    ensure_internet_hub(invite)?;
    let code = PairCode::generate_relay(relay_url)?;
    let mut first = Some(connect_relay(&code, "offer").await?);
    present_code(&code, timeout, presentation)?;
    copy_to_clipboard(code.as_str());
    if presentation == PairPresentation::Human {
        println!("Copied pairing capability to clipboard. Waiting through the relay…");
    }

    let deadline = tokio::time::Instant::now() + timeout;
    for attempt in 1..=MAX_ATTEMPTS {
        let channel = match first.take() {
            Some(channel) => channel,
            None => tokio::time::timeout_at(deadline, connect_relay(&code, "offer"))
                .await
                .context("Pairing capability expired before another computer connected")??,
        };
        match tokio::time::timeout_at(
            deadline.min(tokio::time::Instant::now() + Duration::from_secs(30)),
            host_exchange(channel, &code, invite, None),
        )
        .await
        {
            Ok(Ok(())) => {
                match presentation {
                    PairPresentation::Human => println!(
                        "Pairing credentials delivered. The other computer is connecting now."
                    ),
                    PairPresentation::Tray => {
                        write_tray_event(&PairCompleteEvent { event: "paired" })?
                    }
                }
                return Ok(());
            }
            Ok(Err(error)) => {
                tracing::warn!("Rejected relayed pairing attempt: {error:#}");
                if attempt < MAX_ATTEMPTS {
                    eprintln!(
                        "Pairing attempt rejected; still waiting ({attempt}/{MAX_ATTEMPTS})."
                    );
                }
            }
            Err(_) if attempt < MAX_ATTEMPTS => {
                eprintln!("Pairing attempt timed out; still waiting ({attempt}/{MAX_ATTEMPTS}).");
            }
            Err(_) => {}
        }
    }
    bail!("Pairing stopped after {MAX_ATTEMPTS} rejected or incomplete attempts")
}

fn present_code(
    code: &PairCode,
    timeout: Duration,
    presentation: PairPresentation,
) -> anyhow::Result<()> {
    match presentation {
        PairPresentation::Human => {
            println!("Pair another computer");
            println!("\n  {}", code.as_str());
            println!("\nOn the other computer:");
            println!("  feanorfs start {} /path/to/project", code.as_str());
            println!(
                "\nThe code expires in {} and works once.",
                format_ttl(timeout)
            );
            Ok(())
        }
        PairPresentation::Tray => write_tray_event(&PairReadyEvent {
            event: "ready",
            code: code.as_str(),
            expires_in_seconds: timeout.as_secs(),
        }),
    }
}

fn write_tray_event(event: &impl Serialize) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, event).context("encode tray pairing event")?;
    writeln!(stdout).context("write tray pairing event")?;
    stdout.flush().context("flush tray pairing event")
}

fn format_ttl(timeout: Duration) -> String {
    if timeout.as_secs() < 60 {
        format!("{} seconds", timeout.as_secs())
    } else {
        let minutes = timeout.as_secs() / 60;
        format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
    }
}

pub async fn receive(code: &PairCode, timeout: Duration) -> anyhow::Result<Zeroizing<String>> {
    if code.relay.is_some() {
        let channel = tokio::time::timeout(timeout, connect_relay(code, "join"))
            .await
            .context("Pairing relay did not respond before the capability expired")??;
        return tokio::time::timeout(Duration::from_secs(30), client_exchange(channel, code))
            .await
            .context("Relayed pairing exchange timed out")?
            .context("Pairing capability was rejected or expired");
    }
    let endpoints = discover(code, timeout)?;
    for endpoint in endpoints {
        let stream = match tokio::time::timeout(
            Duration::from_secs(5),
            TcpStream::connect(endpoint),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            _ => continue,
        };
        return tokio::time::timeout(
            Duration::from_secs(15),
            client_exchange(PairChannel::Tcp(stream), code),
        )
        .await
        .context("Pairing exchange timed out")?
        .context("Pairing code was rejected or expired");
    }
    bail!("Found the pairing session, but could not connect to it")
}

async fn connect_relay(code: &PairCode, role: &str) -> anyhow::Result<PairChannel> {
    let endpoint = code
        .relay_endpoint(role)?
        .context("pairing code does not contain a relay endpoint")?;
    let (socket, response) = tokio_tungstenite::connect_async(&endpoint)
        .await
        .with_context(|| format!("connect to pairing relay at {endpoint}"))?;
    if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
        bail!("pairing relay refused the WebSocket upgrade");
    }
    Ok(PairChannel::Relay(Box::new(socket)))
}

async fn ensure_hub_reachable(workspace: &Path, config: &Config) -> anyhow::Result<()> {
    let api = crate::open_api_client(workspace, config).await?;
    if config.format_version >= 3 {
        let head = api
            .get_head(&config.workspace_id)
            .await
            .context("The workspace hub is not reachable; start it before pairing")?;
        if head.is_none() {
            bail!(
                "Workspace has no encrypted snapshot head yet; run `feanorfs sync --no-watch` first"
            );
        }
    } else {
        let workspaces = api
            .get_workspaces()
            .await
            .context("The workspace hub is not reachable; start it before pairing")?;
        if !workspaces.contains(&config.workspace_id) {
            bail!("Workspace is not present on the hub yet; run `feanorfs sync --no-watch` first");
        }
    }
    Ok(())
}

fn ensure_internet_hub(invite: &WorkspaceInvite) -> anyhow::Result<()> {
    let url = reqwest::Url::parse(&invite.server_url).context("parse workspace hub URL")?;
    if url.scheme() != "https" {
        bail!("Off-LAN pairing requires an HTTPS hub reachable from the other computer");
    }
    if let Some(relay) = &invite.relay {
        feanorfs_agent_core::tunnel::validate_config(relay)?;
        return Ok(());
    }
    let host = url.host_str().context("workspace hub URL has no host")?;
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") {
        bail!(
            "This private hub is LAN-only. Off-LAN pairing needs a publicly reachable managed or self-hosted HTTPS hub; the relay transfers credentials but never tunnels file traffic."
        );
    }
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        let private = match address {
            std::net::IpAddr::V4(address) => {
                address.is_private()
                    || address.is_loopback()
                    || address.is_link_local()
                    || address.is_unspecified()
            }
            std::net::IpAddr::V6(address) => {
                address.is_loopback()
                    || address.is_unspecified()
                    || address.is_unique_local()
                    || address.is_unicast_link_local()
            }
        };
        if private {
            bail!(
                "This hub address is private/LAN-only. Off-LAN pairing needs a publicly reachable HTTPS hub; the relay does not tunnel file traffic."
            );
        }
    }
    Ok(())
}

fn local_ipv4_addresses() -> anyhow::Result<Vec<Ipv4Addr>> {
    let mut addresses: Vec<Ipv4Addr> = if_addrs::get_if_addrs()?
        .into_iter()
        .filter_map(|interface| match interface.ip() {
            std::net::IpAddr::V4(address) if !address.is_loopback() => Some(address),
            _ => None,
        })
        .collect();
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        bail!("No non-loopback IPv4 network interface is available for LAN pairing");
    }
    Ok(addresses)
}

async fn advertise(code: &PairCode, port: u16) -> anyhow::Result<MdnsRegistration> {
    let addresses = local_ipv4_addresses()?
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let daemon = ServiceDaemon::new().context("start mDNS pairing discovery")?;
    let monitor = daemon
        .monitor()
        .context("monitor mDNS pairing advertisement")?;
    let info = pairing_service_info(code, port, addresses)?;
    let fullname = info.get_fullname().to_string();
    daemon.register(info)?;
    tokio::time::timeout(MDNS_ANNOUNCE_TIMEOUT, async {
        loop {
            match monitor.recv_async().await {
                Ok(DaemonEvent::Announce(announced, _))
                    if announced.eq_ignore_ascii_case(&fullname) =>
                {
                    return Ok(());
                }
                Ok(DaemonEvent::Error(error)) => {
                    return Err(anyhow::anyhow!(error).context("announce LAN pairing session"));
                }
                Ok(_) => {}
                Err(error) => {
                    return Err(anyhow::anyhow!(error)
                        .context("mDNS pairing monitor stopped before announcement"));
                }
            }
        }
    })
    .await
    .context("mDNS pairing session was not announced within 3 seconds")??;
    Ok(MdnsRegistration { daemon, fullname })
}

fn pairing_service_info(
    code: &PairCode,
    port: u16,
    addresses: String,
) -> anyhow::Result<ServiceInfo> {
    let instance = format!("feanorfs-pair-{}", code.tag);
    let properties = [("v", "1"), ("id", code.tag.as_str())];
    Ok(ServiceInfo::new(
        PAIR_SERVICE,
        &instance,
        &format!("{instance}.local."),
        addresses,
        port,
        &properties[..],
    )?)
}

fn discover(code: &PairCode, timeout: Duration) -> anyhow::Result<Vec<SocketAddr>> {
    let daemon = ServiceDaemon::new().context("start mDNS pairing discovery")?;
    let receiver = daemon.browse(PAIR_SERVICE)?;
    let deadline = Instant::now() + timeout;
    let result = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info))
                if info.get_property_val_str("v") == Some("1")
                    && info.get_property_val_str("id") == Some(code.tag.as_str()) =>
            {
                let mut endpoints: Vec<SocketAddr> = info
                    .get_addresses_v4()
                    .into_iter()
                    .map(|address| SocketAddr::from((address, info.get_port())))
                    .collect();
                endpoints.sort_unstable();
                break Ok(endpoints);
            }
            Ok(_) => continue,
            Err(_) => {
                break Err(anyhow::anyhow!(
                    "No matching FeanorFS pairing session found on this LAN within {} seconds",
                    timeout.as_secs()
                ))
            }
        }
    };
    let _ = daemon.stop_browse(PAIR_SERVICE);
    let _ = daemon.shutdown();
    let endpoints = result?;
    if endpoints.is_empty() {
        bail!("Pairing session advertised no reachable IPv4 address");
    }
    Ok(endpoints)
}

async fn host_exchange(
    mut channel: PairChannel,
    code: &PairCode,
    invite: &WorkspaceInvite,
    local_address: Option<std::net::IpAddr>,
) -> anyhow::Result<()> {
    let encoded_invite = invite_for_connection(invite, local_address)?;
    let (state, outbound) = Spake2::<Ed25519Group>::start_b(
        &Password::new(code.as_str().as_bytes()),
        &Identity::new(CLIENT_ID),
        &code.host_identity(),
    );
    let inbound = channel.receive().await?;
    channel.send(&outbound).await?;
    let shared = Zeroizing::new(
        state
            .finish(&inbound)
            .map_err(|error| anyhow::anyhow!("invalid PAKE message: {error:?}"))?,
    );
    let sealed_invite = seal(&shared, INVITE_AAD, encoded_invite.as_bytes())?;
    channel.send(&sealed_invite).await?;
    let sealed_ack = channel.receive().await?;
    let ack = open(&shared, ACK_AAD, &sealed_ack)?;
    if ack != ACK {
        bail!("pairing key confirmation failed");
    }
    Ok(())
}

fn invite_for_connection(
    invite: &WorkspaceInvite,
    local_address: Option<std::net::IpAddr>,
) -> anyhow::Result<Zeroizing<String>> {
    // Managed private hubs use their durable CA-bound mDNS hostname. Custom
    // loopback hubs retain the accepted-interface fallback for compatibility.
    let mut invite = super::hub_service::portable_invite(invite.clone());
    let mut server_url = reqwest::Url::parse(&invite.server_url).context("parse invite hub URL")?;
    let host = server_url.host_str().unwrap_or_default();
    let is_local_only = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback() || address.is_unspecified());
    if is_local_only {
        if let Some(local_address) = local_address {
            server_url
                .set_host(Some(&local_address.to_string()))
                .map_err(|_| anyhow::anyhow!("replace loopback hub address for paired computer"))?;
            invite.server_url = server_url.to_string();
        }
    }
    Ok(Zeroizing::new(feanorfs_client::encode_invite(&invite)?))
}

async fn client_exchange(
    mut channel: PairChannel,
    code: &PairCode,
) -> anyhow::Result<Zeroizing<String>> {
    let (state, outbound) = Spake2::<Ed25519Group>::start_a(
        &Password::new(code.as_str().as_bytes()),
        &Identity::new(CLIENT_ID),
        &code.host_identity(),
    );
    channel.send(&outbound).await?;
    let inbound = channel.receive().await?;
    let shared = Zeroizing::new(
        state
            .finish(&inbound)
            .map_err(|error| anyhow::anyhow!("invalid PAKE message: {error:?}"))?,
    );
    let sealed_invite = channel.receive().await?;
    let invite = open(&shared, INVITE_AAD, &sealed_invite)?;
    let sealed_ack = seal(&shared, ACK_AAD, ACK)?;
    channel.send(&sealed_ack).await?;
    String::from_utf8(invite)
        .map(Zeroizing::new)
        .context("pairing response was not a valid invite")
}

fn seal(shared: &[u8], aad: &[u8], plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let key_bytes = Zeroizing::new(blake3::derive_key("feanorfs lan pairing aead v1", shared));
    let key: &Key = key_bytes.as_ref().try_into().expect("32-byte key");
    let cipher = ChaCha20Poly1305::new(key);
    let mut nonce_bytes = [0_u8; 12];
    getrandom::fill(&mut nonce_bytes)
        .map_err(|error| anyhow::anyhow!("generate pairing nonce: {error}"))?;
    let mut output = nonce_bytes.to_vec();
    let nonce: &Nonce = (&nonce_bytes).into();
    output.extend(
        cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("encrypt pairing payload"))?,
    );
    nonce_bytes.zeroize();
    Ok(output)
}

fn open(shared: &[u8], aad: &[u8], sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
    if sealed.len() < 12 {
        bail!("encrypted pairing frame is truncated");
    }
    let key_bytes = Zeroizing::new(blake3::derive_key("feanorfs lan pairing aead v1", shared));
    let key: &Key = key_bytes.as_ref().try_into().expect("32-byte key");
    let cipher = ChaCha20Poly1305::new(key);
    let nonce: &Nonce = sealed[..12].try_into().expect("12-byte nonce");
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &sealed[12..],
                aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("pairing authentication failed"))?;
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_code_is_unambiguous_and_roundtrips() {
        let code = PairCode::generate().unwrap();
        assert!(code.as_str().starts_with(PAIR_PREFIX));
        assert!(PairCode::parse(&code.as_str().to_ascii_lowercase()).unwrap() == code);
        let body = code.as_str().strip_prefix(PAIR_PREFIX).unwrap();
        assert!(!body.contains(['0', '1', 'I', 'O']));
    }

    #[test]
    fn relay_pair_capability_roundtrips_without_putting_secret_in_endpoint() {
        let code = PairCode::generate_relay("https://relay.example/base").unwrap();
        assert!(code.as_str().starts_with(RELAY_PAIR_PREFIX));
        let parsed = PairCode::parse(&code.as_str().to_ascii_uppercase()).unwrap();
        assert!(parsed == code);
        let endpoint = parsed.relay_endpoint("join").unwrap().unwrap();
        let relay = parsed.relay.as_ref().unwrap();
        assert_eq!(relay.session.len(), 32);
        assert!(endpoint.starts_with("wss://relay.example/base/api/pair-relay/"));
        assert!(endpoint.ends_with("/join"));
        assert!(!endpoint.contains(parsed.as_str()));
    }

    #[test]
    fn relay_pairing_requires_encrypted_transport_except_on_loopback() {
        assert!(PairCode::generate_relay("ws://relay.example").is_err());
        assert!(PairCode::generate_relay("http://relay.example").is_err());
        assert!(PairCode::generate_relay("ws://127.0.0.1:3030").is_ok());
        assert!(PairCode::generate_relay("https://relay.example").is_ok());
    }

    #[test]
    fn longest_relay_capability_fits_the_tray_event_bound() {
        let relay = format!("https://relay.example/{}", "a".repeat(220));
        let code = PairCode::generate_relay(&relay).unwrap();
        let event = serde_json::to_vec(&PairReadyEvent {
            event: "ready",
            code: code.as_str(),
            expires_in_seconds: 900,
        })
        .unwrap();
        assert!(code.as_str().len() <= MAX_RELAY_CODE_BYTES);
        assert!(event.len() < 1024);
    }

    #[test]
    fn off_lan_pairing_refuses_lan_only_hubs() {
        let invite = |server_url: &str| WorkspaceInvite {
            server_url: server_url.into(),
            workspace_id: "demo".into(),
            server_token: Some("token".into()),
            encryption_key: "a".repeat(64),
            tls_ca_pem: None,
            hub_local: false,
            relay: None,
            ignore_policy: None,
        };
        assert!(ensure_internet_hub(&invite("https://sync.example")).is_ok());
        assert!(ensure_internet_hub(&invite("http://sync.example")).is_err());
        assert!(ensure_internet_hub(&invite("https://feanorfs-abcd.local")).is_err());
        assert!(ensure_internet_hub(&invite("https://192.168.1.13:3030")).is_err());
        assert!(ensure_internet_hub(&invite("https://127.0.0.1:3030")).is_err());
        let mut relayed = invite("https://feanorfs-abcd.local:3030");
        relayed.relay =
            Some(feanorfs_agent_core::tunnel::generate_config("http://127.0.0.1:4040").unwrap());
        assert!(ensure_internet_hub(&relayed).is_ok());
    }

    #[test]
    fn tray_ready_event_exposes_only_the_ephemeral_code_and_ttl() {
        let code = PairCode::parse("fnp1-2345-6789-ABCD-EFGH").unwrap();
        let event = serde_json::to_value(PairReadyEvent {
            event: "ready",
            code: code.as_str(),
            expires_in_seconds: 300,
        })
        .unwrap();
        assert_eq!(
            event,
            serde_json::json!({
                "event": "ready",
                "code": "fnp1-2345-6789-ABCD-EFGH",
                "expires_in_seconds": 300
            })
        );
        let serialized = event.to_string();
        assert!(!serialized.contains("invite"));
        assert!(!serialized.contains("token"));
        assert!(!serialized.contains("encryption"));
    }

    #[test]
    fn pair_code_rejects_ambiguous_characters() {
        assert!(PairCode::parse("fnp1-0000-AAAA-BBBB-CCCC").is_err());
    }

    #[test]
    fn expiry_copy_uses_seconds_for_short_smoke_windows() {
        assert_eq!(format_ttl(Duration::from_secs(30)), "30 seconds");
        assert_eq!(format_ttl(Duration::from_secs(300)), "5 minutes");
    }

    #[test]
    fn mdns_metadata_contains_only_public_rendezvous_fields() {
        let code = PairCode::parse("fnp1-2345-6789-ABCD-EFGH").unwrap();
        let info = pairing_service_info(&code, 41234, "192.168.1.13".into()).unwrap();
        assert_eq!(info.get_property_val_str("v"), Some("1"));
        assert_eq!(info.get_property_val_str("id"), Some("2345"));
        assert_eq!(info.get_properties().iter().count(), 2);
        let rendered = format!("{info:?}");
        assert!(!rendered.contains("6789-ABCD-EFGH"));
        assert!(!rendered.contains("fnr1-"));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "requires a multicast-capable LAN host"]
    async fn announced_mdns_pairing_is_discoverable() {
        let code = PairCode::generate().unwrap();
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let _registration = advertise(&code, port).await.unwrap();
        let discovered_code = code.clone();
        let endpoints =
            tokio::task::spawn_blocking(move || discover(&discovered_code, Duration::from_secs(3)))
                .await
                .unwrap()
                .unwrap();
        assert!(endpoints.iter().any(|endpoint| endpoint.port() == port));
    }

    #[tokio::test]
    async fn loopback_pairing_encrypts_and_confirms_invite() {
        let code = PairCode::parse("fnp1-2345-6789-ABCD-EFGH").unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        let invite = feanorfs_client::WorkspaceInvite {
            server_url: "https://hub.example".into(),
            workspace_id: "demo".into(),
            server_token: Some("server-secret".into()),
            encryption_key: "a".repeat(64),
            tls_ca_pem: None,
            hub_local: false,
            relay: None,
            ignore_policy: Some("target/\n".into()),
        };
        let encoded = feanorfs_client::encode_invite(&invite).unwrap();
        let host_code = code.clone();
        let host_invite = invite.clone();
        let host = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            host_exchange(
                PairChannel::Tcp(stream),
                &host_code,
                &host_invite,
                Some(Ipv4Addr::LOCALHOST.into()),
            )
            .await
        });
        let client = client_exchange(
            PairChannel::Tcp(TcpStream::connect(endpoint).await.unwrap()),
            &code,
        )
        .await
        .unwrap();
        assert_eq!(client.as_str(), encoded);
        host.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn relayed_pairing_encrypts_and_confirms_invite_end_to_end() {
        let data = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let relay = tokio::spawn(feanorfs_server::run_http_server(
            feanorfs_server::ServeOptions {
                data_dir: data.path().to_path_buf(),
                port,
                allow_http: true,
                relay: true,
                ..feanorfs_server::ServeOptions::default()
            },
        ));
        let mut ready = false;
        for _ in 0..100 {
            if TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .is_ok()
            {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ready, "pair relay did not become ready");

        let code = PairCode::generate_relay(&format!("http://127.0.0.1:{port}")).unwrap();
        let invite = feanorfs_client::WorkspaceInvite {
            server_url: "https://hub.example".into(),
            workspace_id: "demo".into(),
            server_token: Some("server-secret".into()),
            encryption_key: "a".repeat(64),
            tls_ca_pem: None,
            hub_local: false,
            relay: None,
            ignore_policy: None,
        };
        let encoded = feanorfs_client::encode_invite(&invite).unwrap();
        let offer = connect_relay(&code, "offer").await.unwrap();
        let host_code = code.clone();
        let host_invite = invite.clone();
        let host =
            tokio::spawn(async move { host_exchange(offer, &host_code, &host_invite, None).await });
        let join = connect_relay(&code, "join").await.unwrap();
        let delivered = client_exchange(join, &code).await.unwrap();

        assert_eq!(delivered.as_str(), encoded);
        host.await.unwrap().unwrap();

        let correct = PairCode::generate_relay(&format!("http://127.0.0.1:{port}")).unwrap();
        let relay_details = correct.relay.as_ref().unwrap();
        let wrong_payload = RelayPairPayload {
            relay_url: relay_details.url.clone(),
            session: relay_details.session.clone(),
            secret: "BBBBBBBBBBBBBBBB".into(),
        };
        let wrong = PairCode::parse(&format!(
            "{RELAY_PAIR_PREFIX}{}",
            hex_encode(&serde_json::to_vec(&wrong_payload).unwrap())
        ))
        .unwrap();
        let offer = connect_relay(&correct, "offer").await.unwrap();
        let host_invite = invite.clone();
        let host_code = correct.clone();
        let host =
            tokio::spawn(async move { host_exchange(offer, &host_code, &host_invite, None).await });
        let join = connect_relay(&wrong, "join").await.unwrap();
        assert!(client_exchange(join, &wrong).await.is_err());
        assert!(host.await.unwrap().is_err());

        relay.abort();
    }

    #[tokio::test]
    async fn wrong_code_cannot_decrypt_invite() {
        let host_code = PairCode::parse("fnp1-2345-6789-ABCD-EFGH").unwrap();
        let client_code = PairCode::parse("fnp1-2345-6789-ABCD-EFGJ").unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        let host = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let invite = feanorfs_client::WorkspaceInvite {
                server_url: "https://hub.example".into(),
                workspace_id: "demo".into(),
                server_token: None,
                encryption_key: "b".repeat(64),
                tls_ca_pem: None,
                hub_local: false,
                relay: None,
                ignore_policy: None,
            };
            host_exchange(
                PairChannel::Tcp(stream),
                &host_code,
                &invite,
                Some(Ipv4Addr::LOCALHOST.into()),
            )
            .await
        });
        assert!(client_exchange(
            PairChannel::Tcp(TcpStream::connect(endpoint).await.unwrap()),
            &client_code,
        )
        .await
        .is_err());
        assert!(host.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_allocation() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let endpoint = listener.local_addr().unwrap();
        let reader = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            PairChannel::Tcp(stream).receive().await
        });
        let mut writer = TcpStream::connect(endpoint).await.unwrap();
        writer.write_u32((MAX_FRAME + 1) as u32).await.unwrap();
        assert!(reader.await.unwrap().is_err());
    }

    #[test]
    fn tampered_pairing_payload_fails_authentication() {
        let shared = [7_u8; 32];
        let mut sealed = seal(&shared, INVITE_AAD, b"fnr1-secret").unwrap();
        *sealed.last_mut().unwrap() ^= 1;
        assert!(open(&shared, INVITE_AAD, &sealed).is_err());
        assert!(open(&shared, ACK_AAD, &seal(&shared, INVITE_AAD, b"x").unwrap()).is_err());
    }

    #[test]
    fn pairing_rewrites_loopback_hub_for_the_working_interface() {
        let invite = feanorfs_client::WorkspaceInvite {
            server_url: "http://127.0.0.1:3030".into(),
            workspace_id: "demo".into(),
            server_token: None,
            encryption_key: "c".repeat(64),
            tls_ca_pem: None,
            hub_local: false,
            relay: None,
            ignore_policy: None,
        };
        let encoded =
            invite_for_connection(&invite, Some("192.168.1.13".parse().unwrap())).unwrap();
        let decoded = feanorfs_client::decode_invite(encoded.as_str()).unwrap();
        assert_eq!(decoded.server_url, "http://192.168.1.13:3030/");
    }
}
