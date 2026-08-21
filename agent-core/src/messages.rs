//! Encrypted agent signals: message-only snapshots and reachability-delta inbox.
//!
//! A signal is an ordinary encrypted format-v3 snapshot with no file-tree
//! changes: it reuses the latest head's tree root, keeps the latest head as
//! its parent, stores the sender in `Snapshot.author`, and stores the
//! `ffmsg1:` envelope in `Snapshot.message`. Publication uses the existing
//! workspace-head compare-and-swap operation; every CAS retry reloads both the
//! latest head and its tree root so a retry can never roll back files.

use crate::history::traversal;
use crate::paths::validate_name;
use crate::snapshot::SnapshotEngine;
use crate::{SwapHeadResult, SyncCtx};
use anyhow::{bail, ensure, Context, Result};
use feanorfs_common::{
    encode_agent_message, is_valid_hash, parse_agent_message, AgentInboxQuery, AgentInboxResult,
    AgentMessage, AgentMessageInput, AgentMessagePayload, AgentSendResult, Snapshot,
    AGENT_INBOX_MAX_LIMIT, AGENT_MESSAGE_MAX_BODY_BYTES,
};
use std::collections::{HashMap, HashSet, VecDeque};

const MAX_SEND_RETRIES: usize = 8;
const MAX_SIGNAL_SCAN: usize = 10_000;

/// Result of attempting to append one signal against an exact observed head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadConditionalSendResult {
    /// The signal became the workspace head.
    Sent(AgentSendResult),
    /// The head changed before publication. No retry was attempted.
    Conflict(Option<String>),
}

/// Publishes one encrypted agent signal and returns its immutable snapshot id.
///
/// The signal's `about_snapshot` defaults to the head observed when sending
/// starts and stays fixed even if concurrent changes advance the head while
/// the signal is being appended. A successful send returns only after the
/// encrypted snapshot object, reachability manifest, and head swap succeed.
///
/// # Errors
/// Returns an error for invalid names/ids/bodies, unreachable snapshot
/// references, offline transport, or repeated concurrent head changes.
pub async fn send_message(ctx: &SyncCtx<'_>, input: AgentMessageInput) -> Result<AgentSendResult> {
    ensure_signal_format(ctx)?;
    let engine = SnapshotEngine::new(ctx);
    let initial_head = ctx.api.get_head(ctx.workspace_id()).await?;
    let prepared = prepare_message(ctx, input, initial_head.as_deref()).await?;

    let mut expected = initial_head;
    for _ in 0..MAX_SEND_RETRIES {
        let Some(parent) = &expected else {
            bail!("workspace head disappeared while sending signal");
        };
        let parent_snapshot = engine.load_snapshot(parent).await?;
        let candidate = write_message_snapshot(
            ctx,
            &parent_snapshot,
            parent,
            &prepared.from,
            &prepared.envelope,
        )
        .await?;
        match ctx
            .api
            .swap_head(ctx.workspace_id(), expected.as_deref(), &candidate)
            .await?
        {
            SwapHeadResult::Swapped => {
                return Ok(AgentSendResult {
                    message_id: candidate,
                    about_snapshot: prepared.about_snapshot,
                })
            }
            SwapHeadResult::Conflict(current) => expected = current,
        }
    }
    Err(crate::agent::continuous::retryable_volatility_failure(
        "workspace head changed too many times while sending signal",
    ))
}

/// Attempts one signal append against `expected_head` without CAS retries.
///
/// A known head conflict is returned separately from transport errors so a
/// caller can reread history before deciding whether another append is safe.
/// The candidate object may already be uploaded when a conflict is returned,
/// but it is never made reachable by this operation.
///
/// # Errors
/// Returns an error for invalid input, unreadable references, or transport
/// failures whose publication outcome cannot be established.
pub async fn send_message_if_head(
    ctx: &SyncCtx<'_>,
    expected_head: &str,
    input: AgentMessageInput,
) -> Result<HeadConditionalSendResult> {
    ensure_signal_format(ctx)?;
    ensure!(
        is_valid_hash(expected_head),
        "expected_head must be a full snapshot id"
    );
    let prepared = prepare_message(ctx, input, Some(expected_head)).await?;
    let engine = SnapshotEngine::new(ctx);
    let parent_snapshot = engine.load_snapshot(expected_head).await?;
    let candidate = write_message_snapshot(
        ctx,
        &parent_snapshot,
        expected_head,
        &prepared.from,
        &prepared.envelope,
    )
    .await?;
    match ctx
        .api
        .swap_head(ctx.workspace_id(), Some(expected_head), &candidate)
        .await?
    {
        SwapHeadResult::Swapped => Ok(HeadConditionalSendResult::Sent(AgentSendResult {
            message_id: candidate,
            about_snapshot: prepared.about_snapshot,
        })),
        SwapHeadResult::Conflict(current) => Ok(HeadConditionalSendResult::Conflict(current)),
    }
}

struct PreparedMessage {
    from: String,
    envelope: String,
    about_snapshot: String,
}

async fn prepare_message(
    ctx: &SyncCtx<'_>,
    input: AgentMessageInput,
    observed_head: Option<&str>,
) -> Result<PreparedMessage> {
    let from = resolve_sender(input.from.as_deref())?;
    validate_recipient(&input.to)?;
    let body = input.body.trim();
    ensure!(
        !body.is_empty() && body.len() <= AGENT_MESSAGE_MAX_BODY_BYTES,
        "signal body must be non-empty UTF-8 of at most 8 KiB"
    );
    let about_snapshot = match input.about_snapshot {
        Some(id) => {
            ensure!(
                is_valid_hash(&id),
                "about_snapshot must be a full snapshot id"
            );
            ensure_reachable(ctx, observed_head, &id).await?;
            id
        }
        None => observed_head
            .map(str::to_string)
            .context("workspace has no snapshot to attach a signal to")?,
    };
    if let Some(reply_to) = &input.reply_to {
        ensure!(
            is_valid_hash(reply_to),
            "reply_to must be a full snapshot id"
        );
        ensure_signal(ctx, observed_head, reply_to).await?;
    }
    let envelope = encode_agent_message(&AgentMessagePayload {
        to: input.to,
        kind: input.kind,
        body: body.to_string(),
        about_snapshot: about_snapshot.clone(),
        reply_to: input.reply_to,
    })?;
    Ok(PreparedMessage {
        from,
        envelope,
        about_snapshot,
    })
}

/// Reads signals addressed to `query.recipient` (or broadcast `*`).
///
/// With `after` the read searches the graph delta: snapshots reachable from
/// the current head but not from the prior cursor. Cursor loss or an
/// unreachable cursor sets `cursor_reset` and returns only a bounded recent
/// view. Within the scan and result bounds, reusing cursors provides
/// repeatable delivery; a reset explicitly means older signals may have been
/// missed. Reads never publish acknowledgements or mutate history.
///
/// # Errors
/// Returns an error for invalid recipients or corrupt head objects.
pub async fn inbox(ctx: &SyncCtx<'_>, query: AgentInboxQuery) -> Result<AgentInboxResult> {
    validate_recipient(&query.recipient)?;
    ensure!(
        query.limit > 0,
        "inbox limit must be greater than zero; a zero limit would always report a cursor reset"
    );
    let limit = query.limit.min(AGENT_INBOX_MAX_LIMIT);
    collect_signals(ctx, query.after.as_deref(), Some(&query.recipient), limit).await
}

/// Reads every new signal in the graph delta regardless of recipient.
///
/// Orchestrator wakeup helper for NDJSON event streams: returns the same
/// bounded cursor semantics as [`inbox`] but skips the recipient filter.
///
/// # Errors
/// Returns an error for corrupt head objects.
pub async fn signals_since(
    ctx: &SyncCtx<'_>,
    after: Option<&str>,
    limit: usize,
) -> Result<AgentInboxResult> {
    collect_signals(ctx, after, None, limit.min(AGENT_INBOX_MAX_LIMIT)).await
}

async fn collect_signals(
    ctx: &SyncCtx<'_>,
    after: Option<&str>,
    recipient: Option<&str>,
    limit: usize,
) -> Result<AgentInboxResult> {
    ensure_signal_format(ctx)?;
    let engine = SnapshotEngine::new(ctx);
    let Some(head) = ctx.api.get_head(ctx.workspace_id()).await? else {
        return Ok(AgentInboxResult {
            cursor: String::new(),
            cursor_reset: after.is_some(),
            messages: Vec::new(),
        });
    };
    if after == Some(head.as_str()) {
        // Validate that the advertised head object is readable, but avoid
        // walking old history when the caller is already at the current head.
        engine.load_snapshot(&head).await?;
        return Ok(AgentInboxResult {
            cursor: head,
            cursor_reset: false,
            messages: Vec::new(),
        });
    }

    // First walk the current head, stopping only the path that reaches the
    // supplied cursor. This keeps the common one-new-signal case proportional
    // to the graph delta instead of rescanning all history. If any multi-
    // parent snapshot occurs in the head walk, an alternate parent can re-enter
    // the cursor's older ancestry without passing through the cursor itself;
    // a second bounded pass then paints the complete cursor ancestry so those
    // snapshots are subtracted as required by
    // reachable(head) - reachable(cursor).
    let mut signal_index = match ctx.state_dir() {
        Ok(state_dir) => crate::signal_index::SignalIndexSession::load(&state_dir),
        Err(_) => crate::signal_index::SignalIndexSession::disabled(),
    };
    async fn walked_snapshot(
        index: &mut crate::signal_index::SignalIndexSession,
        engine: &SnapshotEngine<'_, '_>,
        id: &str,
    ) -> Result<Snapshot> {
        if let Some(snapshot) = index.get(id) {
            return Ok(snapshot);
        }
        let snapshot = engine.load_snapshot(id).await?;
        index.put(id, &snapshot);
        Ok(snapshot)
    }
    let mut pending = VecDeque::from([head.clone()]);
    let mut seen_from_head = HashSet::new();
    let mut loaded_snapshots: HashMap<String, Snapshot> = HashMap::new();
    let mut candidate_ids = Vec::new();
    let mut after_found = after.is_none();
    let mut scan_exhausted = false;
    let mut saw_merge_in_head_walk = false;
    while let Some(id) = pending.pop_front() {
        if !seen_from_head.insert(id.clone()) {
            continue;
        }
        if loaded_snapshots.len() >= MAX_SIGNAL_SCAN {
            scan_exhausted = true;
            break;
        }

        if after == Some(id.as_str()) {
            after_found = true;
            let snapshot = walked_snapshot(&mut signal_index, &engine, &id).await?;
            loaded_snapshots.insert(id, snapshot);
            continue;
        }

        let snapshot = walked_snapshot(&mut signal_index, &engine, &id).await?;
        saw_merge_in_head_walk |= snapshot.parents.len() > 1;
        pending.extend(snapshot.parents.iter().cloned());
        loaded_snapshots.insert(id.clone(), snapshot);
        candidate_ids.push(id);
    }

    let mut cursor_ancestry = HashSet::new();
    let mut cursor_reset = scan_requires_cursor_reset(after, scan_exhausted, after_found);
    if let Some(after) = after.filter(|_| !cursor_reset && saw_merge_in_head_walk) {
        let mut cursor_pending = VecDeque::from([after.to_string()]);
        while let Some(id) = cursor_pending.pop_front() {
            if !cursor_ancestry.insert(id.clone()) {
                continue;
            }

            let parents = if let Some(snapshot) = loaded_snapshots.get(&id) {
                snapshot.parents.clone()
            } else {
                if loaded_snapshots.len() >= MAX_SIGNAL_SCAN {
                    scan_exhausted = true;
                    break;
                }
                let snapshot = walked_snapshot(&mut signal_index, &engine, &id).await?;
                let parents = snapshot.parents.clone();
                loaded_snapshots.insert(id, snapshot);
                parents
            };
            cursor_pending.extend(parents);
        }
        if scan_exhausted {
            // We cannot prove a complete set difference. Surface the reset and
            // return a bounded recent head view rather than a partial delta.
            cursor_reset = true;
            cursor_ancestry.clear();
        }
    }

    let mut messages = Vec::new();
    for id in candidate_ids {
        if cursor_ancestry.contains(&id) {
            continue;
        }
        let snapshot = &loaded_snapshots[&id];
        if let Some(message) = &snapshot.message {
            if let Some(payload) = parse_agent_message(message) {
                if snapshot.author == "*" || validate_name(&snapshot.author).is_err() {
                    continue;
                }
                let matches = recipient.is_none_or(|r| payload.to == r || payload.to == "*");
                if matches {
                    messages.push(AgentMessage {
                        message_id: id,
                        from: snapshot.author.clone(),
                        to: payload.to,
                        kind: payload.kind,
                        body: payload.body,
                        about_snapshot: payload.about_snapshot,
                        reply_to: payload.reply_to,
                        created_at_ms: snapshot.created_at_ms,
                    });
                }
            }
        }
    }
    messages.sort_by(|left, right| {
        right
            .created_at_ms
            .cmp(&left.created_at_ms)
            .then_with(|| right.message_id.cmp(&left.message_id))
    });
    cursor_reset |= messages.len() > limit;
    messages.truncate(limit);
    signal_index.flush().await;
    Ok(AgentInboxResult {
        cursor: head,
        cursor_reset,
        messages,
    })
}

fn scan_requires_cursor_reset(
    after: Option<&str>,
    scan_exhausted: bool,
    after_found: bool,
) -> bool {
    scan_exhausted || (after.is_some() && !after_found)
}

fn ensure_signal_format(ctx: &SyncCtx<'_>) -> Result<()> {
    if ctx.format_version() < 3 {
        return Err(crate::agent::continuous::unsupported_schema_failure(
            "agent signals require format v3; run `feanorfs migrate` first",
        ));
    }
    Ok(())
}

fn resolve_sender(explicit: Option<&str>) -> Result<String> {
    let sender = explicit.unwrap_or("human").trim().to_string();
    validate_name(&sender)?;
    ensure!(sender != "*", "sender must not be the broadcast form");
    Ok(sender)
}

fn validate_recipient(to: &str) -> Result<()> {
    if to == "*" {
        return Ok(());
    }
    validate_name(to)
}

async fn write_message_snapshot(
    ctx: &SyncCtx<'_>,
    parent: &Snapshot,
    parent_id: &str,
    author: &str,
    message: &str,
) -> Result<String> {
    let engine = SnapshotEngine::new(ctx);
    let id = engine
        .objects
        .put_snapshot(&Snapshot {
            root: parent.root.clone(),
            parents: vec![parent_id.to_string()],
            author: author.to_string(),
            created_at_ms: chrono::Utc::now().timestamp_millis(),
            message: Some(message.to_string()),
        })
        .await?;
    let hashes = engine.objects.snapshot_reachability(&id, true).await?;
    ctx.api
        .upload_manifest(ctx.workspace_id(), &id, &hashes)
        .await?;
    if let Ok(state_dir) = ctx.state_dir() {
        let _ = crate::upload_registry::record_many(&state_dir, &hashes).await;
    }
    engine.objects.cache_manifest(&id, &hashes).await?;
    Ok(id)
}

async fn ensure_reachable(ctx: &SyncCtx<'_>, head: Option<&str>, id: &str) -> Result<()> {
    let Some(head) = head else {
        bail!("snapshot {id} is not reachable from the workspace head");
    };
    if head == id {
        return Ok(());
    }
    let engine = SnapshotEngine::new(ctx);
    let outcome = traversal::walk(
        head,
        traversal::TraversalBudgets {
            node_budget: MAX_SIGNAL_SCAN,
            ..traversal::TraversalBudgets::unlimited()
        },
        traversal::ParentOrder::LastFirst,
        &mut traversal::EngineLoader(&engine),
        &mut traversal::TargetFinder::new(id),
    )
    .await?;
    match outcome {
        traversal::TraversalOutcome::Stopped { .. } => Ok(()),
        traversal::TraversalOutcome::Exhausted { reason, .. } => {
            bail!("snapshot {id} is not reachable within the scan bound ({reason})")
        }
        traversal::TraversalOutcome::Complete { .. } => {
            bail!("snapshot {id} is not reachable from the workspace head")
        }
    }
}

async fn ensure_signal(ctx: &SyncCtx<'_>, head: Option<&str>, id: &str) -> Result<()> {
    ensure_reachable(ctx, head, id).await?;
    let snapshot = SnapshotEngine::new(ctx).load_snapshot(id).await?;
    ensure!(
        snapshot
            .message
            .as_deref()
            .and_then(parse_agent_message)
            .is_some(),
        "reply_to must reference an ffmsg1 signal snapshot"
    );
    Ok(())
}

/// Appends one raw message-only snapshot onto `parent` and swaps the head.
///
/// Test-support and diagnostics helper: writes arbitrary `Snapshot.message`
/// text without `ffmsg1` validation, so malformed history can be built and
/// proven harmless to typed inbox reads.
#[doc(hidden)]
pub async fn append_raw_snapshot(
    ctx: &SyncCtx<'_>,
    parent: &str,
    author: &str,
    message: &str,
) -> Result<String> {
    let engine = SnapshotEngine::new(ctx);
    let parent_snapshot = engine.load_snapshot(parent).await?;
    let id = write_message_snapshot(ctx, &parent_snapshot, parent, author, message).await?;
    match ctx
        .api
        .swap_head(ctx.workspace_id(), Some(parent), &id)
        .await?
    {
        SwapHeadResult::Swapped => Ok(id),
        SwapHeadResult::Conflict(_) => Err(crate::agent::continuous::retryable_volatility_failure(
            "workspace head changed while appending raw snapshot",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::scan_requires_cursor_reset;

    #[test]
    fn exhausted_scan_resets_even_without_a_prior_cursor() {
        assert!(scan_requires_cursor_reset(None, true, true));
        assert!(!scan_requires_cursor_reset(None, false, true));
        assert!(scan_requires_cursor_reset(Some("missing"), false, false));
        assert!(!scan_requires_cursor_reset(Some("found"), false, true));
    }
}
