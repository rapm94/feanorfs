use crate::durable::DurableJson;
use anyhow::{ensure, Result};
use feanorfs_common::{MeshCandidate, MeshTransport, NodeId};
use serde::{Deserialize, Serialize};
use std::path::Path;

const MESH_STATE_SCHEMA_VERSION: u32 = 1;
const MESH_STATE_FILE: &str = "mesh-state.json";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MeshFailureKind {
    Timeout,
    Unreachable,
    Authentication,
    Stun,
    Nat,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct MeshAttemptCounters {
    attempts: u64,
    successes: u64,
    consecutive_failures: u64,
    last_attempt_ms: i64,
    last_failure: Option<MeshFailureKind>,
}

impl MeshAttemptCounters {
    #[must_use]
    pub const fn attempts(&self) -> u64 {
        self.attempts
    }

    #[must_use]
    pub const fn successes(&self) -> u64 {
        self.successes
    }

    #[must_use]
    pub const fn consecutive_failures(&self) -> u64 {
        self.consecutive_failures
    }

    fn record_failure(&mut self, failure: MeshFailureKind, now_ms: i64) {
        self.attempts = self.attempts.saturating_add(1);
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_attempt_ms = now_ms;
        self.last_failure = Some(failure);
    }

    fn record_success(&mut self, now_ms: i64) {
        self.attempts = self.attempts.saturating_add(1);
        self.successes = self.successes.saturating_add(1);
        self.consecutive_failures = 0;
        self.last_attempt_ms = now_ms;
        self.last_failure = None;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeshPath {
    node_id: NodeId,
    candidate: MeshCandidate,
    established_at_ms: i64,
}

impl MeshPath {
    pub fn new(node_id: NodeId, candidate: MeshCandidate, established_at_ms: i64) -> Result<Self> {
        ensure!(
            established_at_ms >= 0,
            "mesh path timestamp must be nonnegative"
        );
        Ok(Self {
            node_id,
            candidate,
            established_at_ms,
        })
    }

    #[must_use]
    pub const fn node_id(&self) -> NodeId {
        self.node_id
    }

    #[must_use]
    pub const fn candidate(&self) -> &MeshCandidate {
        &self.candidate
    }

    #[must_use]
    pub const fn established_at_ms(&self) -> i64 {
        self.established_at_ms
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeshState {
    schema_version: u32,
    #[serde(default)]
    tcp: MeshAttemptCounters,
    #[serde(default)]
    quic: MeshAttemptCounters,
    #[serde(default)]
    last_path: Option<MeshPath>,
}

impl Default for MeshState {
    fn default() -> Self {
        Self {
            schema_version: MESH_STATE_SCHEMA_VERSION,
            tcp: MeshAttemptCounters::default(),
            quic: MeshAttemptCounters::default(),
            last_path: None,
        }
    }
}

impl MeshState {
    #[must_use]
    pub const fn tcp(&self) -> &MeshAttemptCounters {
        &self.tcp
    }

    #[must_use]
    pub const fn quic(&self) -> &MeshAttemptCounters {
        &self.quic
    }

    #[must_use]
    pub const fn last_path(&self) -> Option<&MeshPath> {
        self.last_path.as_ref()
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == MESH_STATE_SCHEMA_VERSION,
            "unsupported mesh state schema version {}",
            self.schema_version
        );
        for counters in [&self.tcp, &self.quic] {
            ensure!(
                counters.successes <= counters.attempts
                    && counters.consecutive_failures <= counters.attempts,
                "mesh attempt counters are inconsistent"
            );
        }
        Ok(())
    }

    fn counters_mut(&mut self, transport: MeshTransport) -> &mut MeshAttemptCounters {
        match transport {
            MeshTransport::Tcp => &mut self.tcp,
            MeshTransport::Quic => &mut self.quic,
        }
    }
}

#[derive(Debug)]
pub struct MeshStateStore {
    state: DurableJson<MeshState>,
}

impl MeshStateStore {
    pub fn open(state_dir: &Path) -> Result<Self> {
        let store = Self {
            state: DurableJson::open(state_dir, MESH_STATE_FILE, MeshState::default())?,
        };
        store.state.with_read(MeshState::validate)?;
        Ok(store)
    }

    pub fn snapshot(&self) -> Result<MeshState> {
        self.state.with_read(|state| {
            state.validate()?;
            Ok(state.clone())
        })
    }

    pub fn record_failure(
        &self,
        transport: MeshTransport,
        failure: MeshFailureKind,
        now_ms: i64,
    ) -> Result<()> {
        ensure!(now_ms >= 0, "mesh attempt timestamp must be nonnegative");
        self.state.with_write(|state| {
            state.validate()?;
            state
                .counters_mut(transport)
                .record_failure(failure, now_ms);
            Ok(())
        })
    }

    pub fn record_success(&self, path: MeshPath) -> Result<()> {
        self.state.with_write(|state| {
            state.validate()?;
            state
                .counters_mut(path.candidate.transport())
                .record_success(path.established_at_ms);
            state.last_path = Some(path);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::{MeshCandidate, MeshCandidateKind, MeshTransport, NodeId};

    #[test]
    fn mesh_state_records_bounded_attempts_and_winning_path() {
        let root = tempfile::tempdir().unwrap();
        let store = MeshStateStore::open(root.path()).unwrap();
        store
            .record_failure(MeshTransport::Tcp, MeshFailureKind::Timeout, 10)
            .unwrap();
        let candidate = MeshCandidate::new(
            MeshTransport::Tcp,
            MeshCandidateKind::Mapped,
            "198.51.100.12:3030".parse().unwrap(),
        )
        .unwrap();
        store
            .record_success(
                MeshPath::new(NodeId::from_public_key([12_u8; 32]), candidate.clone(), 20).unwrap(),
            )
            .unwrap();

        let state = store.snapshot().unwrap();
        assert_eq!(state.tcp().attempts(), 2);
        assert_eq!(state.tcp().successes(), 1);
        assert_eq!(state.tcp().consecutive_failures(), 0);
        assert_eq!(state.last_path().unwrap().candidate(), &candidate);
    }
}
