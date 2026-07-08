use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const INVITE_PREFIX: &str = "fnr1-";

/// Opaque join payload (CONN-4): server + workspace + tokens + E2EE key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceInvite {
    pub server_url: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_token: Option<String>,
    pub encryption_key: String,
    #[serde(default)]
    pub hub_local: bool,
}

pub fn encode_invite(invite: &WorkspaceInvite) -> Result<String> {
    let json = serde_json::to_vec(invite).context("serialize invite")?;
    Ok(format!("{INVITE_PREFIX}{}", hex_encode(&json)))
}

pub fn decode_invite(token: &str) -> Result<WorkspaceInvite> {
    if token.len() > 8192 {
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

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        bail!("hex length must be even");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("invalid hex in invite"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_roundtrip() {
        let inv = WorkspaceInvite {
            server_url: "http://127.0.0.1:3030".into(),
            workspace_id: "demo".into(),
            server_token: None,
            encryption_key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
            hub_local: false,
        };
        let enc = encode_invite(&inv).unwrap();
        assert!(enc.starts_with(INVITE_PREFIX));
        assert_eq!(decode_invite(&enc).unwrap(), inv);
    }

    #[test]
    fn decode_invite_rejects_oversized() {
        let giant = format!("fnr1-{}", "aa".repeat(5000));
        assert!(decode_invite(&giant).is_err());
    }

    #[test]
    fn invite_preserves_hub_local_flag() {
        let inv = WorkspaceInvite {
            server_url: "feanorfs+local://hub".into(),
            workspace_id: "local".into(),
            server_token: None,
            encryption_key: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .into(),
            hub_local: true,
        };
        let enc = encode_invite(&inv).unwrap();
        assert!(decode_invite(&enc).unwrap().hub_local);
    }
}
