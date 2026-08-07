//! Encrypted agent signals: message-only snapshots and reachability-delta inbox.
//!
//! A signal is an ordinary encrypted format-v3 snapshot with no file-tree
//! changes: it reuses the latest head's tree root, keeps the latest head as
//! its parent, stores the sender in `Snapshot.author`, and stores the
//! `ffmsg1:` envelope in `Snapshot.message`. Publication uses the existing
//! workspace-head compare-and-swap operation; every CAS retry reloads both the
//! latest head and its tree root so a retry can never roll back files.

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
    let from = resolve_sender(input.from.as_deref())?;
    validate_recipient(&input.to)?;
    let body = input.body.trim();
    ensure!(
        !body.is_empty() && body.len() <= AGENT_MESSAGE_MAX_BODY_BYTES,
        "signal body must be non-empty UTF-8 of at most 8 KiB"
    );

    let engine = SnapshotEngine::new(ctx);
    let initial_head = ctx.api.get_head(ctx.workspace_id()).await?;
    let about = match input.about_snapshot {
        Some(id) => {
            ensure!(
                is_valid_hash(&id),
                "about_snapshot must be a full snapshot id"
            );
            ensure_reachable(ctx, initial_head.as_deref(), &id).await?;
            id
        }
        None => initial_head
            .clone()
            .context("workspace has no snapshot to attach a signal to")?,
    };
    if let Some(reply_to) = &input.reply_to {
        ensure!(
            is_valid_hash(reply_to),
            "reply_to must be a full snapshot id"
        );
        ensure_signal(ctx, initial_head.as_deref(), reply_to).await?;
    }

    let envelope = encode_agent_message(&AgentMessagePayload {
        to: input.to,
        kind: input.kind,
        body: body.to_string(),
        about_snapshot: about.clone(),
        reply_to: input.reply_to,
    })?;

    let mut expected = initial_head;
    for _ in 0..MAX_SEND_RETRIES {
        let Some(parent) = &expected else {
            bail!("workspace head disappeared while sending signal");
        };
        let parent_snapshot = engine.load_snapshot(parent).await?;
        let candidate =
            write_message_snapshot(ctx, &parent_snapshot, parent, &from, &envelope).await?;
        match ctx
            .api
            .swap_head(ctx.workspace_id(), expected.as_deref(), &candidate)
            .await?
        {
            SwapHeadResult::Swapped => {
                return Ok(AgentSendResult {
                    message_id: candidate,
                    about_snapshot: about,
                })
            }
            SwapHeadResult::Conflict(current) => expected = current,
        }
    }
    bail!("workspace head changed too many times while sending signal")
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
    // to the graph delta instead of rescanning all history. If a multi-parent
    // snapshot occurs before the cursor, an alternate parent can re-enter the
    // cursor's older ancestry; a second bounded pass then paints the complete
    // cursor ancestry so those snapshots are subtracted as required by
    // reachable(head) - reachable(cursor).
    let mut pending = VecDeque::from([head.clone()]);
    let mut seen_from_head = HashSet::new();
    let mut loaded_snapshots: HashMap<String, Snapshot> = HashMap::new();
    let mut candidate_ids = Vec::new();
    let mut after_found = after.is_none();
    let mut scan_exhausted = false;
    let mut saw_merge_before_cursor = false;
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
            loaded_snapshots.insert(id.clone(), engine.load_snapshot(&id).await?);
            continue;
        }

        let snapshot = engine.load_snapshot(&id).await?;
        saw_merge_before_cursor |= snapshot.parents.len() > 1;
        pending.extend(snapshot.parents.iter().cloned());
        loaded_snapshots.insert(id.clone(), snapshot);
        candidate_ids.push(id);
    }

    let mut cursor_ancestry = HashSet::new();
    let mut cursor_reset = scan_requires_cursor_reset(after, scan_exhausted, after_found);
    if let Some(after) = after.filter(|_| !cursor_reset && saw_merge_before_cursor) {
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
                let snapshot = engine.load_snapshot(&id).await?;
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
    ensure!(
        ctx.format_version() >= 3,
        "agent signals require format v3; run `feanorfs migrate` first"
    );
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
    let mut pending = vec![head.to_string()];
    let mut seen = HashSet::new();
    while let Some(current) = pending.pop() {
        if seen.len() >= MAX_SIGNAL_SCAN {
            bail!("snapshot {id} is not reachable within the scan bound");
        }
        if !seen.insert(current.clone()) {
            continue;
        }
        if current == id {
            return Ok(());
        }
        pending.extend(engine.load_snapshot(&current).await?.parents);
    }
    bail!("snapshot {id} is not reachable from the workspace head")
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
        SwapHeadResult::Conflict(_) => {
            bail!("workspace head changed while appending raw snapshot")
        }
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
