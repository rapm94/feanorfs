//! Unified coordination contract: one lifecycle over the three signal
//! protocols, engine-computed next actions, the edit guard, and typed
//! integrator replies.
//!
//! `ffwork1` (work intent), `ffint1` (integrator assignment), and `ffres1`
//! (conflict resolution) keep their versioned wire profiles and reducers.
//! This contract only projects them into one state machine so agents read
//! one status, follow one prefilled next action, and never hand-write
//! protocol JSON.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrator_contract::{IntegratorOutcomeState, VerificationSummary};

/// Schema version of [`CoordinationStatus`] and [`GuardResult`].
pub const COORDINATION_SCHEMA_VERSION: u32 = 1;
/// Maximum lifecycle items in one projection.
pub const COORDINATION_MAX_ITEMS: usize = 64;
/// Maximum next actions in one projection.
pub const COORDINATION_MAX_ACTIONS: usize = 8;
/// Maximum warnings in one projection.
pub const COORDINATION_MAX_WARNINGS: usize = 16;
/// Maximum paths one guard call evaluates.
pub const GUARD_MAX_PATHS: usize = 256;
/// Byte bound for lifecycle details, reasons, and warnings.
pub const COORDINATION_TEXT_BYTES: usize = 512;

/// Which protocol object a lifecycle item projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleKind {
    /// One `ffwork1` proposal (task scope).
    Task,
    /// One pending file conflict and its `ffres1` resolution job, if any.
    Conflict,
    /// One `ffint1` integrator assignment.
    Integration,
}

/// The single lifecycle every coordination object moves through:
/// `proposed → accepted → settled → done` for scoped work, and
/// `conflicted → assigned → resolving → (awaiting_human) → done` when
/// prevention failed. `blocked` and `stopped` are terminal exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStage {
    /// Waiting for a human answer; ordered first because it blocks progress.
    AwaitingHuman,
    Conflicted,
    Proposed,
    Assigned,
    Resolving,
    Accepted,
    Settled,
    Blocked,
    Stopped,
    Done,
}

impl LifecycleStage {
    /// Whether the item can still change.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Blocked | Self::Stopped | Self::Done)
    }
}

/// One projected coordination object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleItem {
    pub kind: LifecycleKind,
    /// Task id, conflict path, or assignment id.
    pub id: String,
    pub stage: LifecycleStage,
    /// Who must act next on this item, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Bounded one-line explanation.
    pub detail: String,
}

/// One engine-computed next step with prefilled arguments. `tool` and
/// `args` are a ready MCP `tools/call`; `cli` is the terminal equivalent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextAction {
    /// Identity expected to act (`human` for escalations).
    pub actor: String,
    pub tool: String,
    pub args: Map<String, Value>,
    pub cli: String,
    pub reason: String,
}

/// Result of `agent next`: the unified coordination projection for one
/// agent identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinationStatus {
    pub schema_version: u32,
    pub agent: String,
    /// Non-terminal items first, ordered by stage urgency.
    pub items: Vec<LifecycleItem>,
    /// The caller's own actions first, then actions owed by others.
    pub next_actions: Vec<NextAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Who advertises which capabilities; route with `cap:<capability>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roster: Vec<RosterEntry>,
    /// True when any source projection was truncated or unavailable; an
    /// absent item is then not proof that nothing is pending.
    pub projection_incomplete: bool,
}

/// Input of the edit guard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardInput {
    /// Agent identity; defaults to `FEANORFS_AGENT`, then `human`.
    #[serde(default)]
    pub agent: Option<String>,
    /// Canonical workspace-relative paths about to be written.
    pub paths: Vec<String>,
    /// Deny paths outside the agent's own accepted scope whenever the
    /// agent holds accepted scope.
    #[serde(default)]
    pub require_scope: bool,
}

/// Guard verdict, ordered by severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardVerdict {
    Allow,
    Warn,
    Deny,
}

/// Verdict for one path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardFinding {
    pub path: String,
    pub verdict: GuardVerdict,
    pub reason: String,
}

/// Result of the edit guard. Advisory coordination, not access control:
/// harness hooks turn `deny` into a blocked tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardResult {
    pub schema_version: u32,
    pub agent: String,
    /// The most severe finding.
    pub verdict: GuardVerdict,
    /// Only paths that are not plainly allowed.
    pub findings: Vec<GuardFinding>,
}

/// Which `ffint1` reply the selected candidate sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegratorReplyKind {
    Accept,
    Result,
    Blocked,
}

/// Candidate-side reply input. The engine binds assignment id, attempt,
/// snapshot, dispatcher, and request id from the observed offer, so callers
/// never hand-write `ffint1` JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntegratorReplyInput {
    /// Replying candidate; defaults to `FEANORFS_AGENT`, then `human`.
    #[serde(default)]
    pub agent: Option<String>,
    /// Defaults to the newest open offer to this agent.
    #[serde(default)]
    pub assignment_id: Option<String>,
    pub kind: IntegratorReplyKind,
    /// Required for `blocked`.
    #[serde(default)]
    pub reason: Option<String>,
    /// `result` outcome state; defaults to `completed`.
    #[serde(default)]
    pub state: Option<IntegratorOutcomeState>,
    /// Required for `result`: bounded human outcome summary.
    #[serde(default)]
    pub outcome: Option<String>,
    /// Required for `result`.
    #[serde(default)]
    pub verification: Option<VerificationSummary>,
    /// Snapshot actually inspected; defaults to the current head.
    #[serde(default)]
    pub inspected_snapshot: Option<String>,
    #[serde(default)]
    pub landed_paths: u64,
    #[serde(default)]
    pub resolved_conflicts: u64,
    #[serde(default)]
    pub remaining_conflicts: u64,
    #[serde(default)]
    pub risks: Vec<String>,
    #[serde(default)]
    pub decision_required: Option<String>,
}

/// One integrator offer as seen by the offered candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegratorOffer {
    pub assignment_id: String,
    pub attempt: u32,
    pub dispatcher: String,
    pub about_snapshot: String,
    pub request_message_id: String,
    pub task: String,
    pub accepted: bool,
    /// A terminal result or blocker was already sent for this attempt.
    pub terminal: bool,
    /// Newer attempt and its selected candidate, when superseded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<(u32, String)>,
}

impl IntegratorOffer {
    /// Whether the candidate still owns this attempt.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        !self.terminal && self.superseded_by.is_none()
    }
}

/// Result of a typed integrator reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegratorReplyResult {
    pub message_id: String,
    pub assignment_id: String,
    pub attempt: u32,
    pub dispatcher: String,
    pub kind: IntegratorReplyKind,
}

/// Discriminator of the capability announcement profile carried in a
/// broadcast `status` signal body.
pub const CAPABILITY_PROFILE_DISCRIMINATOR: &str = "ffcap1";
/// Recipient prefix that routes a signal to the agent advertising a
/// capability (`cap:ios-build`).
pub const CAPABILITY_RECIPIENT_PREFIX: &str = "cap:";

/// `ffcap1` body: the sender's complete current capability set. The newest
/// announcement per sender replaces older ones; advisory like every route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityAnnouncement {
    pub capabilities: Vec<String>,
}

/// Encodes a canonical `ffcap1` announcement (normalized, sorted, unique).
///
/// # Errors
/// Returns an error for invalid or too many capabilities.
pub fn encode_capability_announcement(capabilities: &[String]) -> anyhow::Result<String> {
    let capabilities = crate::normalize_capabilities(capabilities)?;
    Ok(format!(
        "{CAPABILITY_PROFILE_DISCRIMINATOR}:{}",
        serde_json::to_string(&CapabilityAnnouncement { capabilities })?
    ))
}

/// Parses a canonical `ffcap1` announcement; anything else is `None`.
#[must_use]
pub fn parse_capability_announcement(body: &str) -> Option<Vec<String>> {
    let json = body
        .strip_prefix(CAPABILITY_PROFILE_DISCRIMINATOR)?
        .strip_prefix(':')?;
    let announcement: CapabilityAnnouncement = serde_json::from_str(json).ok()?;
    if serde_json::to_string(&announcement).ok()? != json {
        return None;
    }
    let canonical = crate::normalize_capabilities(&announcement.capabilities).ok()?;
    (canonical == announcement.capabilities).then_some(canonical)
}

/// One agent and the capabilities it advertised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterEntry {
    pub agent: String,
    pub capabilities: Vec<String>,
}

/// Input of the capability operation: announce (when `announce` is set),
/// then return the roster.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesInput {
    /// Announcing agent; defaults to `FEANORFS_AGENT`, then `human`.
    #[serde(default)]
    pub agent: Option<String>,
    /// Complete capability set to announce for `agent`.
    #[serde(default)]
    pub announce: Option<Vec<String>>,
}

/// Capability roster built from announcements and work-intent capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityRoster {
    /// Signal id of the announcement this call published, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub announced: Option<String>,
    pub agents: Vec<RosterEntry>,
    /// True when the bounded signal scan was truncated.
    pub projection_incomplete: bool,
}

/// Truncates text to the coordination byte bound on a char boundary.
#[must_use]
pub fn bounded_text(text: &str) -> String {
    if text.len() <= COORDINATION_TEXT_BYTES {
        return text.to_string();
    }
    let mut end = COORDINATION_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_order_puts_blocking_states_first_and_terminal_last() {
        let mut stages = vec![
            LifecycleStage::Done,
            LifecycleStage::Accepted,
            LifecycleStage::AwaitingHuman,
            LifecycleStage::Conflicted,
        ];
        stages.sort();
        assert_eq!(
            stages,
            vec![
                LifecycleStage::AwaitingHuman,
                LifecycleStage::Conflicted,
                LifecycleStage::Accepted,
                LifecycleStage::Done,
            ]
        );
        assert!(LifecycleStage::Done.is_terminal());
        assert!(!LifecycleStage::Resolving.is_terminal());
    }

    #[test]
    fn guard_verdict_severity_orders_deny_highest() {
        assert!(GuardVerdict::Deny > GuardVerdict::Warn);
        assert!(GuardVerdict::Warn > GuardVerdict::Allow);
    }

    #[test]
    fn reply_input_rejects_unknown_fields_and_defaults_counts() {
        let input: IntegratorReplyInput = serde_json::from_str(r#"{"kind":"accept"}"#).unwrap();
        assert_eq!(input.kind, IntegratorReplyKind::Accept);
        assert_eq!(input.landed_paths, 0);
        assert!(
            serde_json::from_str::<IntegratorReplyInput>(r#"{"kind":"accept","x":1}"#).is_err()
        );
    }

    #[test]
    fn capability_announcements_are_canonical() {
        let body = encode_capability_announcement(&["xcode".into(), "ios-build".into()]).unwrap();
        assert_eq!(body, r#"ffcap1:{"capabilities":["ios-build","xcode"]}"#);
        assert_eq!(
            parse_capability_announcement(&body),
            Some(vec!["ios-build".to_string(), "xcode".to_string()])
        );
        assert!(
            parse_capability_announcement(r#"ffcap1:{"capabilities":["xcode","ios-build"]}"#)
                .is_none()
        );
        assert!(parse_capability_announcement("plain status text").is_none());
        assert!(parse_capability_announcement(&format!("{body} ")).is_none());
        assert!(encode_capability_announcement(&["Not Valid".into()]).is_err());
    }

    #[test]
    fn bounded_text_respects_char_boundaries() {
        let text = "é".repeat(COORDINATION_TEXT_BYTES);
        let bounded = bounded_text(&text);
        assert!(bounded.len() <= COORDINATION_TEXT_BYTES);
        assert!(bounded.chars().all(|c| c == 'é'));
    }
}
