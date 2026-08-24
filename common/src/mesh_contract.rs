use anyhow::{ensure, Context as _, Result};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

pub const MESH_CAPABILITY_VERSION: u8 = 2;
pub const MAX_MESH_CANDIDATES: usize = 16;

/// Public Ed25519 machine identity carried by authenticated capabilities.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId([u8; 32]);

impl NodeId {
    #[must_use]
    pub const fn from_public_key(public_key: [u8; 32]) -> Self {
        Self(public_key)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for NodeId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&hex_encode(&self.0))
    }
}

impl Serialize for NodeId {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&hex_encode(&self.0))
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        let bytes = hex_decode(&encoded).map_err(D::Error::custom)?;
        let public_key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| D::Error::custom("mesh node ID must be 256-bit hex"))?;
        Ok(Self(public_key))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MeshTransport {
    Tcp,
    Quic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MeshCandidateKind {
    Lan,
    Direct,
    Mapped,
    Reflexive,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MeshCandidate {
    transport: MeshTransport,
    kind: MeshCandidateKind,
    address: SocketAddr,
}

impl MeshCandidate {
    pub fn new(
        transport: MeshTransport,
        kind: MeshCandidateKind,
        address: SocketAddr,
    ) -> Result<Self> {
        ensure!(address.port() != 0, "mesh candidate port must be nonzero");
        ensure!(
            !address.ip().is_unspecified()
                && !address.ip().is_loopback()
                && !address.ip().is_multicast(),
            "mesh candidate address must be remotely reachable"
        );
        ensure!(
            transport == MeshTransport::Quic || kind != MeshCandidateKind::Reflexive,
            "reflexive mesh candidates require QUIC"
        );
        Ok(Self {
            transport,
            kind,
            address,
        })
    }

    #[must_use]
    pub const fn transport(&self) -> MeshTransport {
        self.transport
    }

    #[must_use]
    pub const fn kind(&self) -> MeshCandidateKind {
        self.kind
    }

    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }
}

#[derive(Deserialize)]
struct MeshCandidateWire {
    transport: MeshTransport,
    kind: MeshCandidateKind,
    address: SocketAddr,
}

impl<'de> Deserialize<'de> for MeshCandidate {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = MeshCandidateWire::deserialize(deserializer)?;
        Self::new(wire.transport, wire.kind, wire.address).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MeshConfig {
    version: u8,
    node_id: NodeId,
    candidates: Vec<MeshCandidate>,
}

impl MeshConfig {
    pub fn new(node_id: NodeId, mut candidates: Vec<MeshCandidate>) -> Result<Self> {
        ensure!(
            candidates.len() <= MAX_MESH_CANDIDATES,
            "mesh capability has too many candidates"
        );
        candidates.sort_unstable();
        ensure!(
            !candidates.windows(2).any(|pair| pair[0] == pair[1]),
            "mesh capability contains duplicate candidates"
        );
        Ok(Self {
            version: MESH_CAPABILITY_VERSION,
            node_id,
            candidates,
        })
    }

    #[must_use]
    pub const fn node_id(&self) -> NodeId {
        self.node_id
    }

    #[must_use]
    pub fn candidates(&self) -> &[MeshCandidate] {
        &self.candidates
    }
}

#[derive(Deserialize)]
struct MeshConfigWire {
    version: u8,
    node_id: NodeId,
    candidates: Vec<MeshCandidate>,
}

impl<'de> Deserialize<'de> for MeshConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = MeshConfigWire::deserialize(deserializer)?;
        if wire.version != MESH_CAPABILITY_VERSION {
            return Err(D::Error::custom("unsupported mesh capability version"));
        }
        Self::new(wire.node_id, wire.candidates).map_err(D::Error::custom)
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn hex_decode(encoded: &str) -> Result<Vec<u8>> {
    ensure!(encoded.len().is_multiple_of(2), "hex length must be even");
    encoded
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_nibble(pair[0]).context("invalid mesh identity hex")?;
            let low = hex_nibble(pair[1]).context("invalid mesh identity hex")?;
            Ok((high << 4) | low)
        })
        .collect()
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_config_rejects_duplicate_and_excess_candidates() {
        let candidate = MeshCandidate::new(
            MeshTransport::Tcp,
            MeshCandidateKind::Mapped,
            "198.51.100.8:3030".parse().unwrap(),
        )
        .unwrap();
        assert!(MeshConfig::new(
            NodeId::from_public_key([8_u8; 32]),
            vec![candidate.clone(), candidate]
        )
        .is_err());

        let candidates = (1_u16..=17)
            .map(|port| {
                MeshCandidate::new(
                    MeshTransport::Quic,
                    MeshCandidateKind::Reflexive,
                    format!("198.51.100.9:{port}").parse().unwrap(),
                )
                .unwrap()
            })
            .collect();
        assert!(MeshConfig::new(NodeId::from_public_key([9_u8; 32]), candidates).is_err());
    }
}
