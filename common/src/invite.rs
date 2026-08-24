use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};

use crate::mesh_contract::MeshConfig;

pub const INVITE_PREFIX: &str = "fnr1-";
pub const HUB_INVITE_PREFIX: &str = "fnh1-";
pub const HUB_MDNS_SERVICE: &str = "_feanorfs._tcp.local.";
const MAX_WORKSPACE_INVITE_BYTES: usize = 8_192;
const MAX_HUB_INVITE_BYTES: usize = 16_384;

/// Public relay location plus an unguessable route for an opaque inner-TLS tunnel.
///
/// The route is reachability capability material, not a hub bearer token. Relays
/// can observe it and traffic metadata, but the tunneled TLS stream still hides
/// hub credentials, workspace identifiers, object names, and ciphertext.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayConfig {
    pub url: String,
    pub route: String,
}

/// Short public identity used only to correlate a discovered hub with an
/// invite-pinned CA. Possession of this value never establishes trust.
#[must_use]
pub fn hub_ca_fingerprint(public_ca_pem: &str) -> String {
    crate::hash_bytes(public_ca_pem.as_bytes())[..16].to_string()
}

/// Stable local hostname derived from the durable public hub CA.
///
/// The matching CA still authenticates TLS; mDNS only makes this name
/// reachable as interfaces and DHCP leases change.
#[must_use]
pub fn hub_mdns_hostname(public_ca_pem: &str) -> String {
    format!("feanorfs-{}.local", hub_ca_fingerprint(public_ca_pem))
}

/// Opaque join payload (CONN-4): server + workspace + tokens + E2EE key.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceInvite {
    pub server_url: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_token: Option<String>,
    pub encryption_key: String,
    /// Optional private-CA trust anchor for a native-TLS hub. Public certificate only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    #[serde(default)]
    pub hub_local: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh: Option<MeshConfig>,
    /// Exact legacy ignore-file contents selected by the sharing workspace.
    ///
    /// Pairing and recovery encrypt this field with the rest of the capability.
    /// `None` identifies an older capability whose policy is unknown; `Some("")`
    /// explicitly means that the mirror has no custom ignore rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_policy: Option<String>,
}

/// Secure hub introduction used before a workspace and E2EE key exist.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HubInvite {
    pub server_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_token: Option<String>,
    /// Optional private-CA trust anchor. It is public data, not a private key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh: Option<MeshConfig>,
}

impl std::fmt::Debug for RelayConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RelayConfig")
            .field("url", &self.url)
            .field("route", &"<redacted>")
            .finish()
    }
}

impl std::fmt::Debug for WorkspaceInvite {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceInvite")
            .field("server_url", &self.server_url)
            .field("workspace_id", &self.workspace_id)
            .field(
                "server_token",
                &self.server_token.as_ref().map(|_| "<redacted>"),
            )
            .field("encryption_key", &"<redacted>")
            .field("tls_ca_pem_present", &self.tls_ca_pem.is_some())
            .field("hub_local", &self.hub_local)
            .field("relay", &self.relay)
            .field("mesh", &self.mesh)
            .field("ignore_policy_present", &self.ignore_policy.is_some())
            .finish()
    }
}

impl std::fmt::Debug for HubInvite {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HubInvite")
            .field("server_url", &self.server_url)
            .field(
                "server_token",
                &self.server_token.as_ref().map(|_| "<redacted>"),
            )
            .field("tls_ca_pem_present", &self.tls_ca_pem.is_some())
            .field("relay", &self.relay)
            .field("mesh", &self.mesh)
            .finish()
    }
}

pub fn encode_invite(invite: &WorkspaceInvite) -> Result<String> {
    let json = serde_json::to_vec(invite).context("serialize invite")?;
    ensure!(
        INVITE_PREFIX.len() + json.len().saturating_mul(2) <= MAX_WORKSPACE_INVITE_BYTES,
        "invite payload exceeds encodable size"
    );
    Ok(format!("{INVITE_PREFIX}{}", hex_encode(&json)))
}

pub fn decode_invite(token: &str) -> Result<WorkspaceInvite> {
    if token.len() > MAX_WORKSPACE_INVITE_BYTES {
        bail!("invite too long ({})", token.len());
    }
    let hex_part = token
        .strip_prefix(INVITE_PREFIX)
        .with_context(|| format!("invite must start with {INVITE_PREFIX}"))?;
    let bytes = hex_decode(hex_part).context("invalid invite encoding")?;
    serde_json::from_slice(&bytes).context("invalid invite payload")
}

pub fn looks_like_invite(s: &str) -> bool {
    s.starts_with(INVITE_PREFIX)
}

pub fn encode_hub_invite(invite: &HubInvite) -> Result<String> {
    let json = serde_json::to_vec(invite).context("serialize hub invite")?;
    ensure!(
        HUB_INVITE_PREFIX.len() + json.len().saturating_mul(2) <= MAX_HUB_INVITE_BYTES,
        "hub invite payload exceeds encodable size"
    );
    Ok(format!("{HUB_INVITE_PREFIX}{}", hex_encode(&json)))
}

pub fn decode_hub_invite(token: &str) -> Result<HubInvite> {
    if token.len() > MAX_HUB_INVITE_BYTES {
        bail!("hub invite too long ({})", token.len());
    }
    let hex_part = token
        .strip_prefix(HUB_INVITE_PREFIX)
        .with_context(|| format!("hub invite must start with {HUB_INVITE_PREFIX}"))?;
    let bytes = hex_decode(hex_part).context("invalid hub invite encoding")?;
    serde_json::from_slice(&bytes).context("invalid hub invite payload")
}

pub fn looks_like_hub_invite(s: &str) -> bool {
    s.starts_with(HUB_INVITE_PREFIX)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        bail!("hex length must be even");
    }
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = nibble(pair[0]).context("invalid hex in invite")?;
            let low = nibble(pair[1]).context("invalid hex in invite")?;
            Ok((high << 4) | low)
        })
        .collect()
}

#[cfg(test)]
#[path = "invite_tests.rs"]
mod tests;
