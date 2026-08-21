//! Frozen JSON contract fixtures for the agent SDK (SDK-1).
//! Snapshot tests compare serialized output against these strings.

use crate::{
    AgentCheckResult, AgentLandResult, AgentRefreshResult, ConcurrentEdit, ConflictKind, FileState,
    LandedPath,
};
use serde::{Deserialize, Serialize};

/// `feanorfs --json agent spawn` result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnResult {
    pub agent: String,
    pub files_copied: usize,
}

/// One row in `feanorfs --json agent status` (list mode, online).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentListEntry {
    pub name: String,
    pub state: String,
}

/// `feanorfs --json agent status` (list mode, online).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentListResult {
    pub agents: Vec<AgentListEntry>,
}

/// `feanorfs --json agent status` (list mode, offline).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentListOfflineResult {
    pub agents: Vec<String>,
}

/// Live continuous-reconciliation phase for one active agent.
///
/// The same transition and error classification drives interactive
/// `agent run` owners, configured runner workers, CLI status, events, and
/// tests. The controller never gains semantic merge authority: every mutation
/// still flows through the existing land/refresh/conflict machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuousPhase {
    /// Controller started; startup reconciliation has not finished.
    Starting,
    /// Settled and waiting for filesystem or head events.
    Idle,
    /// A coalesced local burst is waiting for the quiet-period debounce.
    LocalDirty,
    /// Outbound land is running.
    ReconcilingLocal,
    /// Inbound refresh of the agent worktree is running.
    RefreshingRemote,
    /// Retryable transport failures; bounded retry with backoff.
    Offline,
    /// Automatic mutation paused for an explicit human/consumer action.
    NeedsAttention,
    /// Shutting down; one bounded final reconciliation attempt remains.
    Stopping,
}

impl ContinuousPhase {
    /// Stable human/event spelling matching the serialized wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Idle => "idle",
            Self::LocalDirty => "local_dirty",
            Self::ReconcilingLocal => "reconciling_local",
            Self::RefreshingRemote => "refreshing_remote",
            Self::Offline => "offline",
            Self::NeedsAttention => "needs_attention",
            Self::Stopping => "stopping",
        }
    }
}

/// Fail-closed reason that pauses automatic mutation until explicit action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuousAttention {
    /// `pending_conflicts` | `unsafe_path` | `corrupt_state` |
    /// `unsupported_schema` | `ownership_lost`
    pub reason: String,
    /// Bounded human-readable detail. Never contains file contents, message
    /// bodies, credentials, or unbounded errors.
    pub detail: String,
}

/// Bounded, secret-free live status projection for one active agent.
///
/// Persisted by the continuous controller and read by `agent status`, the
/// events surface, the tray, and `doctor`. Never contains message bodies, file
/// contents, credentials, endpoints, or process arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuousAgentStatus {
    pub schema_version: u32,
    pub agent: String,
    pub active: bool,
    pub phase: ContinuousPhase,
    /// Last opaque workspace head this controller observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_head: Option<String>,
    /// Tree root of `observed_head`; signal-only heads change the head id but
    /// keep this value, which is how controllers skip file work.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_tree: Option<String>,
    /// Latest reachable snapshot carrying the settled tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_snapshot: Option<String>,
    pub pending_local: bool,
    pub deferred_count: u32,
    pub attention: Option<ContinuousAttention>,
    /// Owner process identity captured for bounded diagnostics. Readers use
    /// the OS-backed ownership lease—not this advisory value—to reject stale
    /// status left by a dead owner.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_start_id: Option<String>,
    pub updated_at_ms: i64,
}

/// Current schema version persisted by continuous controllers.
pub const CONTINUOUS_STATUS_SCHEMA_VERSION: u32 = 1;

/// `feanorfs --json agent clean` result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCleanResult {
    pub cleaned: String,
}

/// One immutable workspace snapshot exposed through history APIs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogEntry {
    pub snapshot_id: String,
    pub parents: Vec<String>,
    pub author: String,
    pub created_at_ms: i64,
    pub message: Option<String>,
    pub changed_paths: Vec<String>,
}

/// Structured workspace history result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogResult {
    pub entries: Vec<LogEntry>,
}

/// Result of restoring a historical snapshot as a new commit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UndoResult {
    pub snapshot_id: String,
    pub restored_snapshot_id: String,
    pub changed_paths: Vec<String>,
}

/// Versioned discriminator prefix for encrypted agent signal envelopes.
///
/// A signal lives in `Snapshot.message` as `ffmsg1:` followed by canonical
/// compact JSON. Unknown future versions remain ordinary history messages and
/// are ignored by typed inbox readers.
pub const AGENT_MESSAGE_DISCRIMINATOR: &str = "ffmsg1";

/// Maximum encoded UTF-8 body length for one agent signal.
pub const AGENT_MESSAGE_MAX_BODY_BYTES: usize = 8 * 1024;

/// Maximum canonical encoded envelope size, including JSON escaping.
pub const AGENT_MESSAGE_MAX_ENCODED_BYTES: usize = 64 * 1024;

/// Maximum UTF-8 byte length for one portable agent name.
pub const AGENT_NAME_MAX_BYTES: usize = 255;

/// Default result count for one inbox read.
pub const AGENT_INBOX_DEFAULT_LIMIT: usize = 50;

/// Maximum result count for one inbox read.
pub const AGENT_INBOX_MAX_LIMIT: usize = 1000;

/// Kind of one encrypted agent signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageKind {
    /// Ask another agent to perform bounded work against a snapshot.
    Request,
    /// Short progress update; no acknowledgement required.
    Status,
    /// Final bounded outcome; the requester consumes it.
    Result,
    /// Final explanation of why a request cannot complete.
    Blocked,
}

impl AgentMessageKind {
    /// Returns the stable wire string for this kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Status => "status",
            Self::Result => "result",
            Self::Blocked => "blocked",
        }
    }
}

/// Canonical `ffmsg1` envelope payload stored in `Snapshot.message`.
///
/// Fields derived from the enclosing snapshot (`message_id`, `from`,
/// `created_at_ms`) are intentionally not duplicated in the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessagePayload {
    pub to: String,
    pub kind: AgentMessageKind,
    pub body: String,
    pub about_snapshot: String,
    pub reply_to: Option<String>,
}

/// Sender-side input for `Workspace::send_message`.
///
/// `from` is optional: CLI callers derive it from `FEANORFS_AGENT` (falling
/// back to `human`); embeddings may supply an explicit validated sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMessageInput {
    pub to: String,
    pub kind: AgentMessageKind,
    pub body: String,
    pub about_snapshot: Option<String>,
    pub reply_to: Option<String>,
    pub from: Option<String>,
}

/// Result of publishing one agent signal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSendResult {
    pub message_id: String,
    pub about_snapshot: String,
}

/// One typed agent signal returned by inbox reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessage {
    pub message_id: String,
    pub from: String,
    pub to: String,
    pub kind: AgentMessageKind,
    pub body: String,
    pub about_snapshot: String,
    pub reply_to: Option<String>,
    pub created_at_ms: i64,
}

/// Inbox read query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInboxQuery {
    pub recipient: String,
    /// Previous workspace-head cursor; reads the graph delta when present.
    pub after: Option<String>,
    pub limit: usize,
}

/// Result of one inbox read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInboxResult {
    /// Workspace head observed by this read; pass back as the next `after`.
    pub cursor: String,
    /// True when the supplied cursor was unreachable or a traversal/result
    /// bound omitted history: older signals may have been missed and only a
    /// bounded recent view is returned.
    pub cursor_reset: bool,
    pub messages: Vec<AgentMessage>,
}

/// Encodes one signal envelope as `ffmsg1:` followed by canonical compact JSON.
///
/// # Errors
/// Returns an error when the recipient is not a valid agent name or broadcast,
/// the body is empty or exceeds 8 KiB, or a snapshot reference is not a full
/// 64-hex id.
pub fn encode_agent_message(payload: &AgentMessagePayload) -> anyhow::Result<String> {
    let body = payload.body.trim();
    anyhow::ensure!(
        !body.is_empty() && payload.body.len() <= AGENT_MESSAGE_MAX_BODY_BYTES,
        "signal body must be non-empty UTF-8 of at most 8 KiB"
    );
    let valid_recipient = payload.to == "*" || crate::is_valid_agent_name(&payload.to);
    anyhow::ensure!(
        valid_recipient,
        "signal recipient must be a valid agent name or '*'"
    );
    anyhow::ensure!(
        crate::is_valid_hash(&payload.about_snapshot),
        "signal about_snapshot must be a full snapshot id"
    );
    if let Some(reply_to) = &payload.reply_to {
        anyhow::ensure!(
            crate::is_valid_hash(reply_to),
            "signal reply_to must be a full snapshot id"
        );
    }
    let encoded = format!(
        "{AGENT_MESSAGE_DISCRIMINATOR}:{}",
        serde_json::to_string(payload)?
    );
    anyhow::ensure!(
        encoded.len() <= AGENT_MESSAGE_MAX_ENCODED_BYTES,
        "encoded signal envelope exceeds 64 KiB"
    );
    Ok(encoded)
}

/// Parses an `ffmsg1` signal envelope.
///
/// Returns `None` for unknown message versions, malformed payloads, or
/// payloads outside the canonical bounds. Unknown/malformed messages remain
/// visible in raw history but are ignored by typed inbox reads.
pub fn parse_agent_message(message: &str) -> Option<AgentMessagePayload> {
    if message.len() > AGENT_MESSAGE_MAX_ENCODED_BYTES {
        return None;
    }
    let json = message.strip_prefix(AGENT_MESSAGE_DISCRIMINATOR)?;
    let json = json.strip_prefix(':')?;
    let payload: AgentMessagePayload = serde_json::from_str(json).ok()?;
    // Canonical compact JSON: a payload that does not re-encode to the exact
    // same bytes (whitespace, reordered or unknown fields) is not canonical.
    if serde_json::to_string(&payload).ok()? != json {
        return None;
    }
    if encode_agent_message(&payload).is_err() {
        return None;
    }
    Some(payload)
}

fn sample_file_state(path: &str) -> FileState {
    FileState {
        path: path.to_string(),
        hash: "a1b2c3d4e5f6789012345678901234567890abcdef1234567890abcdef123456".to_string(),
        size: 42,
        mtime: 1_719_500_000_000,
        deleted: false,
        mode: 0,
    }
}

fn sample_concurrent_edit() -> ConcurrentEdit {
    ConcurrentEdit {
        path: "src/main.rs".to_string(),
        base: Some(sample_file_state("src/main.rs")),
        ours: Some(sample_file_state("src/main.rs")),
        theirs: Some(sample_file_state("src/main.rs")),
        original_file: Some(
            "~/.feanorfs/workspaces/opaque/conflicts/1719500000000/src/main.rs.original"
                .to_string(),
        ),
        local_file: Some(
            "~/.feanorfs/workspaces/opaque/conflicts/1719500000000/src/main.rs.local".to_string(),
        ),
        cloud_file: Some(
            "~/.feanorfs/workspaces/opaque/conflicts/1719500000000/src/main.rs.cloud".to_string(),
        ),
        kind: Some(ConflictKind::EditEdit),
        local_available: true,
        cloud_available: true,
        is_binary: false,
        hint: Some("both sides edited since spawn".to_string()),
        proposed_file: None,
        proposal_clean: None,
    }
}

/// Canonical JSON fixtures — update only with a semver-major contract bump.
pub mod fixtures {
    use super::*;

    pub fn spawn_result() -> SpawnResult {
        SpawnResult {
            agent: "ci1".to_string(),
            files_copied: 12,
        }
    }

    pub fn agent_list_result() -> AgentListResult {
        AgentListResult {
            agents: vec![
                AgentListEntry {
                    name: "ci1".to_string(),
                    state: "2 change(s)".to_string(),
                },
                AgentListEntry {
                    name: "ci2".to_string(),
                    state: "clean".to_string(),
                },
            ],
        }
    }

    pub fn agent_list_offline_result() -> AgentListOfflineResult {
        AgentListOfflineResult {
            agents: vec!["ci1".to_string(), "ci2".to_string()],
        }
    }

    pub fn agent_check_result() -> AgentCheckResult {
        AgentCheckResult {
            agent_name: "ci1".to_string(),
            our_changes: vec![sample_file_state("doc.txt")],
            their_changes: vec![],
            conflicts: vec![],
            conflict_risk: vec!["notes.md".to_string()],
            live: None,
        }
    }

    pub fn agent_land_result() -> AgentLandResult {
        AgentLandResult {
            agent_name: "ci1".to_string(),
            our_changes: vec![sample_file_state("doc.txt")],
            their_changes: vec![],
            conflicts: vec![sample_concurrent_edit()],
            landed: vec![LandedPath {
                path: "doc.txt".to_string(),
                action: "applied".to_string(),
            }],
            message: "Landed 1 path; 1 needs attention.".to_string(),
            snapshot_id: None,
        }
    }

    pub fn agent_refresh_result() -> AgentRefreshResult {
        AgentRefreshResult {
            agent_name: "ci1".to_string(),
            refreshed: vec!["README.md".to_string()],
            deferred: vec!["doc.txt".to_string()],
        }
    }

    pub fn agent_clean_result() -> AgentCleanResult {
        AgentCleanResult {
            cleaned: "ci1".to_string(),
        }
    }

    pub fn log_result() -> LogResult {
        LogResult {
            entries: vec![LogEntry {
                snapshot_id: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_string(),
                parents: vec![
                    "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210".to_string(),
                ],
                author: "ci1".to_string(),
                created_at_ms: 1_719_500_000_000,
                message: Some("land".to_string()),
                changed_paths: vec!["src/main.rs".to_string()],
            }],
        }
    }

    pub fn undo_result() -> UndoResult {
        UndoResult {
            snapshot_id: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .to_string(),
            restored_snapshot_id:
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
            changed_paths: vec!["src/main.rs".to_string()],
        }
    }

    pub fn spawn_json() -> String {
        serde_json::to_string(&spawn_result()).unwrap()
    }

    pub fn agent_list_json() -> String {
        serde_json::to_string(&agent_list_result()).unwrap()
    }

    pub fn agent_list_offline_json() -> String {
        serde_json::to_string(&agent_list_offline_result()).unwrap()
    }

    pub fn agent_check_json() -> String {
        serde_json::to_string(&agent_check_result()).unwrap()
    }

    pub fn agent_land_json() -> String {
        serde_json::to_string(&agent_land_result()).unwrap()
    }

    pub fn agent_refresh_json() -> String {
        serde_json::to_string(&agent_refresh_result()).unwrap()
    }

    pub fn agent_clean_json() -> String {
        serde_json::to_string(&agent_clean_result()).unwrap()
    }

    pub fn log_json() -> String {
        serde_json::to_string(&log_result()).unwrap()
    }

    pub fn undo_json() -> String {
        serde_json::to_string(&undo_result()).unwrap()
    }

    pub fn agent_send_result() -> AgentSendResult {
        AgentSendResult {
            message_id: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            about_snapshot: "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
                .to_string(),
        }
    }

    pub fn agent_message() -> AgentMessage {
        AgentMessage {
            message_id: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                .to_string(),
            from: "linux-dev".to_string(),
            to: "mac-test".to_string(),
            kind: AgentMessageKind::Request,
            body: "Run iOS simulator tests".to_string(),
            about_snapshot: "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
                .to_string(),
            reply_to: None,
            created_at_ms: 1_785_852_000_000,
        }
    }

    pub fn agent_inbox_result() -> AgentInboxResult {
        AgentInboxResult {
            cursor: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
            cursor_reset: false,
            messages: vec![agent_message()],
        }
    }

    pub fn agent_send_json() -> String {
        serde_json::to_string(&agent_send_result()).unwrap()
    }

    pub fn agent_message_json() -> String {
        serde_json::to_string(&agent_message()).unwrap()
    }

    pub fn agent_inbox_json() -> String {
        serde_json::to_string(&agent_inbox_result()).unwrap()
    }

    /// Canonical live-status projection for a settled, active controller.
    pub fn continuous_agent_status() -> ContinuousAgentStatus {
        ContinuousAgentStatus {
            schema_version: CONTINUOUS_STATUS_SCHEMA_VERSION,
            agent: "worker".to_string(),
            active: true,
            phase: ContinuousPhase::Idle,
            observed_head: Some(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
            ),
            observed_tree: Some(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string(),
            ),
            settled_snapshot: Some(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
            ),
            pending_local: false,
            deferred_count: 0,
            attention: None,
            owner_pid: None,
            owner_start_id: None,
            updated_at_ms: 1_719_500_000_000,
        }
    }

    pub fn continuous_agent_status_json() -> String {
        serde_json::to_string(&continuous_agent_status()).unwrap()
    }
}

/// Canonical integrator-assignment fixture types (SDK-1 additive).
pub mod integrator_fixtures {
    use crate::{
        IntegratorAssignResult, IntegratorAssignmentState, IntegratorAttemptState,
        IntegratorAttemptStatus, IntegratorDigest, IntegratorOutcomeState, IntegratorStatusResult,
        VerificationStatus, VerificationSummary,
    };

    fn hex64(byte: u8) -> String {
        std::iter::repeat_n(byte as char, 64).collect()
    }

    fn hex32(byte: u8) -> String {
        std::iter::repeat_n(byte as char, 32).collect()
    }

    pub fn integrator_assign_result() -> IntegratorAssignResult {
        IntegratorAssignResult {
            assignment_id: hex32(b'a'),
            about_snapshot: hex64(b'b'),
            selected: "agent-b".to_string(),
            fallback_order: vec!["agent-a".to_string()],
            neutral_integrator: true,
            roster_fingerprint: hex64(b'c'),
            attempt: 0,
            request_message_id: hex64(b'd'),
            state: IntegratorAssignmentState::Offered,
            task_summary: "Integrate parser implementation and tests".to_string(),
        }
    }

    pub fn integrator_digest() -> IntegratorDigest {
        IntegratorDigest {
            assignment_id: hex32(b'a'),
            integrator: "agent-b".to_string(),
            about_snapshot: hex64(b'b'),
            inspected_snapshot: hex64(b'b'),
            state: IntegratorOutcomeState::Completed,
            landed_paths: 12,
            resolved_conflicts: 3,
            remaining_conflicts: 0,
            verification: VerificationSummary {
                status: VerificationStatus::Passed,
                summary: "84 tests passed".to_string(),
                ..VerificationSummary::default()
            },
            outcome: "Integrated parser implementation and tests.".to_string(),
            risks: vec![],
            decision_required: None,
        }
    }

    pub fn integrator_status_result() -> IntegratorStatusResult {
        IntegratorStatusResult {
            assignment_id: hex32(b'a'),
            about_snapshot: hex64(b'b'),
            state: IntegratorAssignmentState::Accepted,
            selected: Some("agent-b".to_string()),
            attempt: 0,
            neutral_integrator: true,
            roster_fingerprint: hex64(b'c'),
            fallback_order: vec!["agent-a".to_string()],
            task_summary: "Integrate parser implementation and tests".to_string(),
            created_at_ms: 1_785_852_000_000,
            updated_at_ms: 1_785_852_000_000,
            attempts: vec![IntegratorAttemptStatus {
                attempt: 0,
                selected: "agent-b".to_string(),
                state: IntegratorAttemptState::Accepted,
                offered_at_ms: 1_785_852_000_000,
                request_message_id: Some(hex64(b'd')),
                terminal_message_id: None,
                reason: None,
            }],
            digest: None,
            inbox_cursor: Some(hex64(b'e')),
        }
    }

    pub fn integrator_assign_json() -> String {
        serde_json::to_string(&integrator_assign_result()).unwrap()
    }

    pub fn integrator_digest_json() -> String {
        serde_json::to_string(&integrator_digest()).unwrap()
    }

    pub fn integrator_status_json() -> String {
        serde_json::to_string(&integrator_status_result()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(body: &str) -> AgentMessagePayload {
        AgentMessagePayload {
            to: "mac-test".to_string(),
            kind: AgentMessageKind::Request,
            body: body.to_string(),
            about_snapshot: "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
                .to_string(),
            reply_to: None,
        }
    }

    #[test]
    fn envelope_roundtrips_through_canonical_json() {
        let encoded = encode_agent_message(&payload("Run iOS simulator tests")).unwrap();
        assert!(encoded.starts_with("ffmsg1:"));
        let parsed = parse_agent_message(&encoded).unwrap();
        assert_eq!(parsed, payload("Run iOS simulator tests"));
        assert_eq!(parsed.kind.as_str(), "request");
    }

    #[test]
    fn envelope_is_exact_canonical_compact_json() {
        let encoded = encode_agent_message(&payload("hello")).unwrap();
        assert_eq!(
            encoded,
            concat!(
                "ffmsg1:{\"to\":\"mac-test\",\"kind\":\"request\",\"body\":\"hello\",",
                "\"about_snapshot\":\"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210\",",
                "\"reply_to\":null}"
            )
        );
    }

    #[test]
    fn unknown_discriminators_and_malformed_payloads_parse_to_none() {
        assert!(parse_agent_message("ffmsg2:{\"to\":\"x\"}").is_none());
        assert!(parse_agent_message("ffmsg1:not-json").is_none());
        assert!(parse_agent_message("ffmsg1:{\"to\":\"x\"}").is_none());
        assert!(
            parse_agent_message("ffmsg1:{\"to\":\"x\",\"kind\":\"chat\",\"body\":\"b\"}").is_none()
        );
        assert!(parse_agent_message("plain history message").is_none());
        // Whitespace or reordered fields are not canonical compact JSON.
        let encoded = encode_agent_message(&payload("hello")).unwrap();
        assert!(parse_agent_message(&format!("ffmsg1:{}", " {",)).is_none());
        let spaced = encoded.replacen('"', " ", 1);
        assert!(parse_agent_message(&spaced).is_none());
        assert!(parse_agent_message("").is_none());
    }

    #[test]
    fn missing_about_snapshot_is_malformed() {
        let mut p = payload("hello");
        p.about_snapshot = "short".to_string();
        assert!(encode_agent_message(&p).is_err());
        let mut p = payload("hello");
        p.about_snapshot.clear();
        let json = serde_json::to_string(&p).unwrap();
        assert!(parse_agent_message(&format!("ffmsg1:{json}")).is_none());
    }

    #[test]
    fn invalid_reply_to_is_rejected() {
        let mut p = payload("hello");
        p.reply_to = Some("not-a-hash".to_string());
        assert!(encode_agent_message(&p).is_err());
        let mut p = payload("hello");
        p.reply_to =
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into());
        assert!(encode_agent_message(&p).is_ok());
    }

    #[test]
    fn body_bounds_are_enforced() {
        assert!(encode_agent_message(&payload("")).is_err());
        assert!(encode_agent_message(&payload("   ")).is_err());
        let big = "x".repeat(AGENT_MESSAGE_MAX_BODY_BYTES + 1);
        assert!(encode_agent_message(&payload(&big)).is_err());
        let ok = "x".repeat(AGENT_MESSAGE_MAX_BODY_BYTES);
        assert!(encode_agent_message(&payload(&ok)).is_ok());
        let whitespace_wrapped = format!(" {ok} ");
        assert!(
            encode_agent_message(&payload(&whitespace_wrapped)).is_err(),
            "trimming must not let an oversized encoded body bypass the byte bound"
        );
        assert!(encode_agent_message(&payload(&"é".repeat(4096))).is_ok());
        assert!(encode_agent_message(&payload(&"é".repeat(4097))).is_err());
        // Whitespace-only bodies are rejected both at encode and at parse.
        let whitespace_only = format!("ffmsg1:{}", serde_json::to_string(&payload("   ")).unwrap());
        assert!(parse_agent_message(&whitespace_only).is_none());
    }

    #[test]
    fn encoded_envelope_is_bounded_before_json_parsing() {
        let oversized = format!(
            "{AGENT_MESSAGE_DISCRIMINATOR}:{}",
            "x".repeat(AGENT_MESSAGE_MAX_ENCODED_BYTES)
        );
        assert!(oversized.len() > AGENT_MESSAGE_MAX_ENCODED_BYTES);
        assert!(parse_agent_message(&oversized).is_none());
    }

    #[test]
    fn invalid_recipients_are_rejected() {
        for recipient in ["", ".", "..", "nested/name", "nested\\name", "bad\nname"] {
            let mut p = payload("hello");
            p.to = recipient.to_string();
            assert!(
                encode_agent_message(&p).is_err(),
                "recipient {recipient:?} must be rejected"
            );
            let json = serde_json::to_string(&p).unwrap();
            assert!(parse_agent_message(&format!("ffmsg1:{json}")).is_none());
        }

        let mut too_long = payload("hello");
        too_long.to = "a".repeat(AGENT_NAME_MAX_BYTES + 1);
        assert!(encode_agent_message(&too_long).is_err());

        let mut broadcast = payload("hello");
        broadcast.to = "*".into();
        assert!(encode_agent_message(&broadcast).is_ok());
    }

    #[test]
    fn enum_wire_names_are_stable() {
        assert_eq!(AgentMessageKind::Request.as_str(), "request");
        assert_eq!(AgentMessageKind::Status.as_str(), "status");
        assert_eq!(AgentMessageKind::Result.as_str(), "result");
        assert_eq!(AgentMessageKind::Blocked.as_str(), "blocked");
        for (kind, name) in [
            (AgentMessageKind::Request, "request"),
            (AgentMessageKind::Status, "status"),
            (AgentMessageKind::Result, "result"),
            (AgentMessageKind::Blocked, "blocked"),
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), serde_json::json!(name));
        }
        for (phase, name) in [
            (ContinuousPhase::Starting, "starting"),
            (ContinuousPhase::Idle, "idle"),
            (ContinuousPhase::LocalDirty, "local_dirty"),
            (ContinuousPhase::ReconcilingLocal, "reconciling_local"),
            (ContinuousPhase::RefreshingRemote, "refreshing_remote"),
            (ContinuousPhase::Offline, "offline"),
            (ContinuousPhase::NeedsAttention, "needs_attention"),
            (ContinuousPhase::Stopping, "stopping"),
        ] {
            assert_eq!(phase.as_str(), name);
            assert_eq!(
                serde_json::to_value(phase).unwrap(),
                serde_json::json!(name)
            );
        }
    }

    #[test]
    fn adapter_inputs_reject_unknown_fields() {
        assert!(serde_json::from_str::<AgentMessageInput>(
            r#"{"to":"a","kind":"request","body":"x","about_snapshot":null,"reply_to":null,"from":null,"replyto":"typo"}"#,
        )
        .is_err());
        assert!(serde_json::from_str::<AgentInboxQuery>(
            r#"{"recipient":"a","after":null,"limit":1,"cursor":"typo"}"#,
        )
        .is_err());
    }
}
