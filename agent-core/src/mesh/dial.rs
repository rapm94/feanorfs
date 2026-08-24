use crate::api::ApiClient;
use anyhow::{bail, Context as _, Result};
use feanorfs_common::{MeshCandidate, MeshConfig, MeshTransport};
use std::future::Future;
use std::time::Duration;
use tokio::task::JoinSet;

const DEFAULT_DIRECT_DIAL_TIMEOUT: Duration = Duration::from_millis(1_500);

pub struct PeerDialTarget {
    pub server_url: String,
    pub server_password: Option<String>,
    pub tls_ca_pem: Option<String>,
    pub mesh: MeshConfig,
}

pub struct DirectDialOutcome {
    client: ApiClient,
    candidate: MeshCandidate,
}

impl DirectDialOutcome {
    pub fn into_client(self) -> ApiClient {
        self.client
    }

    #[must_use]
    pub const fn candidate(&self) -> &MeshCandidate {
        &self.candidate
    }
}

pub trait PeerDialer {
    fn dial(
        &self,
        target: PeerDialTarget,
    ) -> impl Future<Output = Result<DirectDialOutcome>> + Send;
}

#[derive(Debug, Clone, Copy)]
pub struct DirectPeerDialer {
    attempt_timeout: Duration,
}

impl Default for DirectPeerDialer {
    fn default() -> Self {
        Self {
            attempt_timeout: DEFAULT_DIRECT_DIAL_TIMEOUT,
        }
    }
}

impl DirectPeerDialer {
    #[must_use]
    pub const fn with_timeout(attempt_timeout: Duration) -> Self {
        Self { attempt_timeout }
    }

    fn tcp_candidates(mesh: &MeshConfig) -> Vec<MeshCandidate> {
        mesh.candidates()
            .iter()
            .filter(|candidate| candidate.transport() == MeshTransport::Tcp)
            .cloned()
            .collect()
    }

    async fn dial_target(&self, target: PeerDialTarget) -> Result<DirectDialOutcome> {
        let url = reqwest::Url::parse(&target.server_url).context("parse mesh hub URL")?;
        let hostname = url
            .host_str()
            .context("mesh hub URL has no hostname")?
            .to_string();
        let candidates = Self::tcp_candidates(&target.mesh);
        if candidates.is_empty() {
            bail!("mesh capability has no direct TCP candidate");
        }

        let mut attempts = JoinSet::new();
        for candidate in candidates {
            let server_url = target.server_url.clone();
            let server_password = target.server_password.clone();
            let tls_ca_pem = target.tls_ca_pem.clone();
            let hostname = hostname.clone();
            let timeout = self.attempt_timeout;
            attempts.spawn(async move {
                let client = ApiClient::new_with_tls_resolved(
                    &server_url,
                    server_password.as_deref(),
                    tls_ca_pem.as_deref(),
                    &hostname,
                    &[candidate.address()],
                )?;
                tokio::time::timeout(timeout, client.get_workspaces())
                    .await
                    .context("direct mesh probe timed out")??;
                Ok::<_, anyhow::Error>(DirectDialOutcome { client, candidate })
            });
        }

        let attempted = attempts.len();
        while let Some(result) = attempts.join_next().await {
            if let Ok(Ok(outcome)) = result {
                attempts.abort_all();
                return Ok(outcome);
            }
        }
        bail!("none of {attempted} authenticated direct mesh candidates were reachable")
    }
}

impl PeerDialer for DirectPeerDialer {
    fn dial(
        &self,
        target: PeerDialTarget,
    ) -> impl Future<Output = Result<DirectDialOutcome>> + Send {
        self.dial_target(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::{MeshCandidate, MeshCandidateKind, MeshConfig, MeshTransport, NodeId};

    #[test]
    fn direct_dialer_selects_only_tcp_candidates() {
        let tcp = MeshCandidate::new(
            MeshTransport::Tcp,
            MeshCandidateKind::Mapped,
            "198.51.100.10:3030".parse().unwrap(),
        )
        .unwrap();
        let quic = MeshCandidate::new(
            MeshTransport::Quic,
            MeshCandidateKind::Reflexive,
            "198.51.100.11:3030".parse().unwrap(),
        )
        .unwrap();
        let mesh = MeshConfig::new(
            NodeId::from_public_key([10_u8; 32]),
            vec![quic, tcp.clone()],
        )
        .unwrap();

        assert_eq!(DirectPeerDialer::tcp_candidates(&mesh), vec![tcp]);
    }
}
