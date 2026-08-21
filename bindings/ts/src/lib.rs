use feanorfs_agent_core::{LandOptions, ResolveKeep, Runtime, SpawnOptions, Workspace};
use feanorfs_common::agent_contract::AgentListOfflineResult;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

const MAX_ADAPTER_INPUT_BYTES: usize = 1024 * 1024;
const MAX_ADAPTER_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_ROOT_BYTES: usize = 32 * 1024;
const MAX_NAME_BYTES: usize = 512;
const MAX_HEX_ID_BYTES: usize = 128;

fn ensure_bounded(value: &str, maximum: usize, label: &str) -> Result<()> {
    if value.len() > maximum {
        return Err(Error::from_reason(format!(
            "{label} exceeds {maximum} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn parse_bounded_json<T: serde::de::DeserializeOwned>(json: &str, label: &str) -> Result<T> {
    ensure_bounded(json, MAX_ADAPTER_INPUT_BYTES, label)?;
    serde_json::from_str(json)
        .map_err(|error| Error::from_reason(format!("invalid {label}: {error}")))
}

fn runtime() -> Result<Arc<Runtime>> {
    static RT: OnceLock<Mutex<Option<Arc<Runtime>>>> = OnceLock::new();
    let cell = RT.get_or_init(|| Mutex::new(None));
    let rt = {
        let mut guard = cell
            .lock()
            .map_err(|_| Error::from_reason("runtime lock"))?;
        if guard.is_none() {
            *guard = Some(Runtime::new().map_err(|e| Error::from_reason(e.to_string()))?);
        }
        guard.as_ref().unwrap().clone()
    };
    Ok(rt)
}

fn open(root: &str) -> Result<Workspace> {
    ensure_bounded(root, MAX_ROOT_BYTES, "workspace root")?;
    Workspace::open(&runtime()?, Path::new(root)).map_err(|e| Error::from_reason(e.to_string()))
}

/// Convert an engine `agent_path` result to an exact UTF-8 string.
///
/// The Node adapter cannot represent a lossy filesystem path: an agent
/// directory that is not exactly UTF-8 is a typed error, never a replacement
/// character.
fn agent_path_to_string(path: PathBuf, name: &str) -> anyhow::Result<String> {
    path.into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("agent path for '{name}' is not valid UTF-8"))
}

/// Decodes bounded base64 candidate bytes; anything over the engine's
/// 64 MiB plaintext bound fails closed before the engine call.
fn decode_candidate_base64(encoded: &str) -> Result<Vec<u8>> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .map_err(|error| Error::from_reason(format!("invalid candidate base64: {error}")))?;
    if bytes.len() as u64 > feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES {
        return Err(Error::from_reason(
            "candidate base64 exceeds the 64 MiB bound".to_string(),
        ));
    }
    Ok(bytes)
}

/// Maps one materialized-leg list to the documented `[{"role", "path"}]`
/// JSON shape (the engine returns `(role, absolute path)` tuples).
fn resolution_legs_json(
    legs: Vec<(feanorfs_common::ArtifactRoleName, PathBuf)>,
) -> anyhow::Result<Vec<serde_json::Value>> {
    legs.into_iter()
        .map(|(role, path)| {
            serde_json::to_value(&path)
                .map(|path| serde_json::json!({ "role": role.as_str(), "path": path }))
                .map_err(anyhow::Error::from)
        })
        .collect()
}

/// One bounded operation executor for the Node adapter.
///
/// It centralizes the repeated workspace-open/bounds/error-map/serialize
/// boilerplate: the exact UTF-8 `root` is bounded in [`open`], the engine
/// call runs on a blocking thread (never blocking napi's async runtime), and
/// engine errors map to napi errors carrying the operation name.
///
/// Per-operation input schemas remain in each public method via
/// [`parse_bounded_json`]; there is deliberately no untyped catch-all
/// operation.
async fn execute<T, F>(root: String, op: &'static str, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Workspace) -> anyhow::Result<T> + Send + 'static,
{
    napi::tokio::task::spawn_blocking(move || {
        let workspace =
            open(&root).map_err(|error| Error::from_reason(format!("{op}: {error}")))?;
        f(&workspace).map_err(|error| Error::from_reason(format!("{op}: {error}")))
    })
    .await
    .map_err(|error| Error::from_reason(format!("task join: {error}")))?
}

/// Write sink that refuses to grow past a hard byte bound. `serde_json`
/// serializes incrementally, so an over-bound result fails closed without an
/// unbounded intermediate string.
struct BoundedWriter {
    buffer: Vec<u8>,
    limit: usize,
    overflowed: bool,
}

impl BoundedWriter {
    fn new(limit: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(limit.min(64 * 1024)),
            limit,
            overflowed: false,
        }
    }
}

impl std::io::Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.buffer.len().saturating_add(bytes.len()) > self.limit {
            self.overflowed = true;
            return Err(std::io::Error::other(
                "serialized output exceeds the adapter bound",
            ));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serializes one result with a hard size bound and a typed overflow
/// outcome. Never builds the full output before the bound is enforced.
fn serialize_bounded_json<T: serde::Serialize>(value: &T, op: &'static str) -> Result<String> {
    let mut writer = BoundedWriter::new(MAX_ADAPTER_OUTPUT_BYTES);
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| Error::from_reason(format!("{op} output serialization: {error}")))?;
    if writer.overflowed {
        return Err(Error::from_reason(format!(
            "{op} output exceeds {MAX_ADAPTER_OUTPUT_BYTES} UTF-8 bytes"
        )));
    }
    String::from_utf8(writer.buffer)
        .map_err(|error| Error::from_reason(format!("{op} output encoding: {error}")))
}

/// [`execute`] plus bounded exact JSON output serialization for JSON-string
/// results.
async fn execute_json<T, F>(root: String, op: &'static str, f: F) -> Result<String>
where
    T: serde::Serialize + Send + 'static,
    F: FnOnce(&Workspace) -> anyhow::Result<T> + Send + 'static,
{
    let output = execute(root, op, f).await?;
    serialize_bounded_json(&output, op)
}

#[napi(object)]
pub struct SpawnOptionsJs {
    pub no_sync: Option<bool>,
    pub replace: Option<bool>,
}

#[napi(object)]
pub struct LandOptionsJs {
    pub clean: Option<bool>,
    pub propose: Option<bool>,
}

#[napi]
pub async fn agent_list(root: String) -> Result<String> {
    execute_json(root, "agent_list", |workspace| {
        workspace
            .list()
            .map(|agents| AgentListOfflineResult { agents })
    })
    .await
}

#[napi]
pub async fn agent_spawn(
    root: String,
    name: String,
    opts: Option<SpawnOptionsJs>,
) -> Result<String> {
    ensure_bounded(&name, MAX_NAME_BYTES, "agent name")?;
    let opts = opts.unwrap_or(SpawnOptionsJs {
        no_sync: None,
        replace: None,
    });
    execute_json(root, "agent_spawn", move |workspace| {
        workspace.spawn(
            &name,
            SpawnOptions {
                no_sync: opts.no_sync.unwrap_or(false),
                replace: opts.replace.unwrap_or(false),
            },
        )
    })
    .await
}

#[napi]
pub async fn agent_path(root: String, name: String) -> Result<String> {
    ensure_bounded(&name, MAX_NAME_BYTES, "agent name")?;
    execute(root, "agent_path", move |workspace| {
        workspace
            .agent_path(&name)
            .and_then(|path| agent_path_to_string(path, &name))
    })
    .await
}

#[napi]
pub async fn agent_status(root: String, name: String) -> Result<String> {
    ensure_bounded(&name, MAX_NAME_BYTES, "agent name")?;
    execute_json(root, "agent_status", move |workspace| {
        workspace.status(&name)
    })
    .await
}

#[napi]
pub async fn agent_refresh(root: String, name: String) -> Result<String> {
    ensure_bounded(&name, MAX_NAME_BYTES, "agent name")?;
    execute_json(root, "agent_refresh", move |workspace| {
        workspace.refresh(&name)
    })
    .await
}

#[napi]
pub async fn agent_land(root: String, name: String, opts: Option<LandOptionsJs>) -> Result<String> {
    ensure_bounded(&name, MAX_NAME_BYTES, "agent name")?;
    let opts = opts.unwrap_or(LandOptionsJs {
        clean: None,
        propose: None,
    });
    execute_json(root, "agent_land", move |workspace| {
        workspace.land(
            &name,
            LandOptions {
                clean: opts.clean.unwrap_or(false),
                propose: opts.propose.unwrap_or(false),
            },
        )
    })
    .await
}

#[napi]
pub async fn agent_clean(root: String, name: String) -> Result<String> {
    ensure_bounded(&name, MAX_NAME_BYTES, "agent name")?;
    execute_json(root, "agent_clean", move |workspace| workspace.clean(&name)).await
}

#[napi]
pub async fn history_log(root: String, limit: Option<u32>) -> Result<String> {
    execute_json(root, "history_log", move |workspace| {
        workspace.log(
            limit
                .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
                .unwrap_or(20),
        )
    })
    .await
}

#[napi]
pub async fn undo(root: String, snapshot_id: String) -> Result<String> {
    ensure_bounded(&snapshot_id, MAX_HEX_ID_BYTES, "snapshot id")?;
    execute_json(root, "undo", move |workspace| workspace.undo(&snapshot_id)).await
}

/// Send an encrypted agent signal. JSON in: `AgentMessageInput`; JSON out: `AgentSendResult`.
#[napi]
pub async fn agent_send(root: String, input_json: String) -> Result<String> {
    let input: feanorfs_common::AgentMessageInput =
        parse_bounded_json(&input_json, "agent_send input")?;
    execute_json(root, "agent_send", move |workspace| {
        workspace.send_message(input)
    })
    .await
}

/// Read agent signals. JSON in: `AgentInboxQuery`; JSON out: `AgentInboxResult`.
#[napi]
pub async fn agent_inbox(root: String, query_json: String) -> Result<String> {
    let query: feanorfs_common::AgentInboxQuery =
        parse_bounded_json(&query_json, "agent_inbox input")?;
    execute_json(root, "agent_inbox", move |workspace| workspace.inbox(query)).await
}

/// keep: 0=local, 1=cloud, 2=both, 3=file (requires filePath)
#[napi]
pub async fn conflicts_keep(
    root: String,
    path: String,
    keep: i32,
    file_path: Option<String>,
) -> Result<()> {
    ensure_bounded(&path, MAX_ROOT_BYTES, "conflict path")?;
    if let Some(file_path) = file_path.as_deref() {
        ensure_bounded(file_path, MAX_ROOT_BYTES, "file path")?;
    }
    let keep = match keep {
        0 => ResolveKeep::Local,
        1 => ResolveKeep::Cloud,
        2 => ResolveKeep::Both,
        3 => ResolveKeep::File,
        _ => {
            return Err(Error::from_reason(
                "keep must be 0=local, 1=cloud, 2=both, 3=file",
            ))
        }
    };
    if matches!(keep, ResolveKeep::File) && file_path.is_none() {
        return Err(Error::from_reason("keep=3 (file) requires filePath"));
    }
    let file_ref = file_path.as_deref().map(PathBuf::from);
    execute(root, "conflicts_keep", move |workspace| {
        workspace.resolve(&path, keep, file_ref.as_deref())
    })
    .await
}
/// Assign one batch to a randomly ranked integrator.
/// JSON in: `IntegratorAssignInput`; JSON out: `IntegratorAssignResult`.
#[napi]
pub async fn integrator_assign(root: String, input_json: String) -> Result<String> {
    let input: feanorfs_common::IntegratorAssignInput =
        parse_bounded_json(&input_json, "integrator_assign input")?;
    execute_json(root, "integrator_assign", move |workspace| {
        workspace.integrator_assign(input)
    })
    .await
}

/// Read the active integrator assignment (or one by id).
/// JSON out: `IntegratorStatusResult`.
#[napi]
pub async fn integrator_status(root: String, assignment_id: Option<String>) -> Result<String> {
    if let Some(assignment_id) = assignment_id.as_deref() {
        ensure_bounded(assignment_id, MAX_HEX_ID_BYTES, "assignment id")?;
    }
    execute_json(root, "integrator_status", move |workspace| {
        workspace.integrator_status(assignment_id.as_deref())
    })
    .await
}

/// Explicitly revoke the active integrator assignment.
/// JSON out: `IntegratorStatusResult`.
#[napi]
pub async fn integrator_revoke(
    root: String,
    assignment_id: String,
    reason: String,
) -> Result<String> {
    ensure_bounded(&assignment_id, MAX_HEX_ID_BYTES, "assignment id")?;
    ensure_bounded(&reason, 1024, "revoke reason")?;
    execute_json(root, "integrator_revoke", move |workspace| {
        workspace.integrator_revoke(&assignment_id, &reason)
    })
    .await
}

/// Resume dispatcher observation after a restart.
/// Options JSON: object with optional `ack_timeout_ms` and `fallback_on_blocked`;
/// JSON out: `IntegratorObserveResult`.
#[napi]
pub async fn integrator_resume(root: String, options_json: Option<String>) -> Result<String> {
    let options = match options_json {
        None => feanorfs_agent_core::IntegratorObserveOptions {
            ack_timeout_ms: None,
            fallback_on_blocked: false,
        },
        Some(json) => {
            let input: feanorfs_common::IntegratorObserveInput =
                parse_bounded_json(&json, "integrator_resume options")?;
            feanorfs_agent_core::IntegratorObserveOptions {
                ack_timeout_ms: input.ack_timeout_ms,
                fallback_on_blocked: input.fallback_on_blocked,
            }
        }
    };
    execute_json(root, "integrator_resume", move |workspace| {
        workspace.integrator_resume(options)
    })
    .await
}

/// Materialize the encrypted conflict triple for a snapshot.
/// JSON in: object with `about_snapshot` and exactly one of non-empty `paths` or `all: true`;
/// JSON out: `ConflictMaterializeResult`.
#[napi]
pub async fn conflict_materialize(root: String, input_json: String) -> Result<String> {
    let input: feanorfs_common::ConflictMaterializeInput =
        parse_bounded_json(&input_json, "conflict_materialize input")?;
    let (about_snapshot, paths) = input.validate().map_err(|error| {
        Error::from_reason(format!("invalid conflict_materialize input: {error}"))
    })?;
    execute_json(root, "conflict_materialize", move |workspace| {
        workspace.materialize_conflicts(&about_snapshot, &paths)
    })
    .await
}

/// Propose one encrypted work intent.
/// JSON in: `WorkProposeInput`; JSON out: `WorkSendResult`.
#[napi]
pub async fn work_propose(root: String, input_json: String) -> Result<String> {
    let input: feanorfs_common::WorkProposeInput =
        parse_bounded_json(&input_json, "work_propose input")?;
    execute_json(root, "work_propose", move |workspace| {
        workspace.work_propose(input)
    })
    .await
}

/// Send one coordinator decision.
/// JSON in: `WorkDecideInput`; JSON out: `WorkSendResult`.
#[napi]
pub async fn work_decide(root: String, input_json: String) -> Result<String> {
    let input: feanorfs_common::WorkDecideInput =
        parse_bounded_json(&input_json, "work_decide input")?;
    execute_json(root, "work_decide", move |workspace| {
        workspace.work_decide(input)
    })
    .await
}

/// Amend an accepted intent's scope.
/// JSON in: `WorkAmendInput`; JSON out: `WorkSendResult`.
#[napi]
pub async fn work_amend(root: String, input_json: String) -> Result<String> {
    let input: feanorfs_common::WorkAmendInput =
        parse_bounded_json(&input_json, "work_amend input")?;
    execute_json(root, "work_amend", move |workspace| {
        workspace.work_amend(input)
    })
    .await
}

/// Send an explicit yield.
/// JSON in: `WorkYieldInput`; JSON out: `WorkSendResult`.
#[napi]
pub async fn work_yield(root: String, input_json: String) -> Result<String> {
    let input: feanorfs_common::WorkYieldInput =
        parse_bounded_json(&input_json, "work_yield input")?;
    execute_json(root, "work_yield", move |workspace| {
        workspace.work_yield(input)
    })
    .await
}

/// Send a settled profile with verification evidence.
/// JSON in: `WorkSettleInput`; JSON out: `WorkSendResult`.
#[napi]
pub async fn work_settle(root: String, input_json: String) -> Result<String> {
    let input: feanorfs_common::WorkSettleInput =
        parse_bounded_json(&input_json, "work_settle input")?;
    execute_json(root, "work_settle", move |workspace| {
        workspace.work_settle(input)
    })
    .await
}

/// Send a terminal completion.
/// JSON in: `WorkCompleteInput`; JSON out: `WorkSendResult`.
#[napi]
pub async fn work_complete(root: String, input_json: String) -> Result<String> {
    let input: feanorfs_common::WorkCompleteInput =
        parse_bounded_json(&input_json, "work_complete input")?;
    execute_json(root, "work_complete", move |workspace| {
        workspace.work_complete(input)
    })
    .await
}

/// Send a terminal blocker.
/// JSON in: `WorkBlockInput`; JSON out: `WorkSendResult`.
#[napi]
pub async fn work_block(root: String, input_json: String) -> Result<String> {
    let input: feanorfs_common::WorkBlockInput =
        parse_bounded_json(&input_json, "work_block input")?;
    execute_json(root, "work_block", move |workspace| {
        workspace.work_block(input)
    })
    .await
}

/// Observe signals through the `ffwork1` reducer and report the bounded
/// projection. JSON in: `WorkStatusInput` (optional); JSON out:
/// `WorkStatusResult`.
#[napi]
pub async fn work_status(root: String, input_json: Option<String>) -> Result<String> {
    let input = match input_json {
        Some(json) => parse_bounded_json(&json, "work_status input")?,
        None => feanorfs_common::WorkStatusInput::default(),
    };
    execute_json(root, "work_status", move |workspace| {
        workspace.work_status(input)
    })
    .await
}

/// Prepare one automatic resolution job for the exact current conflict.
/// JSON in: `PreventionReason` (`{"type":"exhausted"|"violated","detail":…}`);
/// JSON out: `ResolutionJob`.
#[napi]
pub async fn resolution_prepare(
    root: String,
    path: String,
    prevention_json: String,
) -> Result<String> {
    ensure_bounded(&path, MAX_ROOT_BYTES, "conflict path")?;
    let prevention: feanorfs_common::PreventionReason =
        parse_bounded_json(&prevention_json, "resolution_prepare prevention")?;
    execute_json(root, "resolution_prepare", move |workspace| {
        workspace.resolution_prepare(&path, prevention)
    })
    .await
}

/// Read the bounded resolution status projection (ids/state/counts only).
/// JSON out: `ResolutionStatusProjection`; pass null `job_id` for the whole
/// store.
#[napi]
pub async fn resolution_status(root: String, job_id: Option<String>) -> Result<String> {
    if let Some(job_id) = job_id.as_deref() {
        ensure_bounded(job_id, MAX_HEX_ID_BYTES, "job id")?;
    }
    execute_json(root, "resolution_status", move |workspace| {
        workspace.resolution_status(job_id.as_deref())
    })
    .await
}

/// Submit one resolution result. JSON in: `ResolutionResult`; JSON out:
/// `ResolutionResult`. Submission NEVER applies: it validates and records
/// the result without mutating the worktree, registry, artifacts, or head.
#[napi]
pub async fn resolution_submit(
    root: String,
    job_id: String,
    result_json: String,
) -> Result<String> {
    ensure_bounded(&job_id, MAX_HEX_ID_BYTES, "job id")?;
    let result: feanorfs_common::ResolutionResult =
        parse_bounded_json(&result_json, "resolution_submit result")?;
    execute_json(root, "resolution_submit", move |workspace| {
        workspace.resolution_submit(&job_id, result)
    })
    .await
}

/// Apply a submitted resolution result with guarded publication.
/// JSON out: `ResolutionApplyOutcome`.
#[napi]
pub async fn resolution_apply(root: String, job_id: String) -> Result<String> {
    ensure_bounded(&job_id, MAX_HEX_ID_BYTES, "job id")?;
    execute_json(root, "resolution_apply", move |workspace| {
        workspace.resolution_apply(&job_id)
    })
    .await
}

/// Materialize the authenticated base/ours/theirs legs of one resolution job
/// into the engine-owned job directory (create-new, no-follow, fsync'd).
/// JSON out: array of `{"role", "path"}`. Read-only: never changes the
/// worktree, conflict registry, artifacts, or head.
#[napi]
pub async fn resolution_materialize(root: String, job_id: String) -> Result<String> {
    ensure_bounded(&job_id, MAX_HEX_ID_BYTES, "job id")?;
    execute_json(root, "resolution_materialize", move |workspace| {
        workspace
            .resolution_materialize_legs(&job_id)
            .and_then(resolution_legs_json)
    })
    .await
}

/// Write the immutable engine-owned candidate file for one job from bounded
/// base64 bytes (create-new, no-follow, fsync'd). The base64 parameter is
/// adapter-bound (1 MiB); JSON out: `CandidateDescriptor`.
#[napi]
pub async fn resolution_put(root: String, job_id: String, base64: String) -> Result<String> {
    ensure_bounded(&job_id, MAX_HEX_ID_BYTES, "job id")?;
    ensure_bounded(&base64, MAX_ADAPTER_INPUT_BYTES, "resolution_put base64")?;
    let bytes = decode_candidate_base64(&base64)?;
    execute_json(root, "resolution_put", move |workspace| {
        workspace.resolution_put_candidate(&job_id, &bytes)
    })
    .await
}

/// Record one typed human answer bound to one exact escalation. JSON in:
/// `HumanResolutionAnswer`; JSON out: the recorded answer. The local engine
/// validates the full binding (job/assignment/attempt/fingerprint/question
/// generation) and, for `submit_candidate`, runs the inline verification.
#[napi]
pub async fn resolution_answer(root: String, answer_json: String) -> Result<String> {
    let answer: feanorfs_common::HumanResolutionAnswer =
        parse_bounded_json(&answer_json, "resolution_answer input")?;
    execute_json(root, "resolution_answer", move |workspace| {
        workspace.resolution_answer(answer)
    })
    .await
}

/// Record the terminal `Deferred` state for one assignment without any
/// publication. JSON out: `null`.
#[napi]
pub async fn resolution_defer(root: String, job_id: String) -> Result<String> {
    ensure_bounded(&job_id, MAX_HEX_ID_BYTES, "job id")?;
    execute_json(root, "resolution_defer", move |workspace| {
        workspace.resolution_defer(&job_id)
    })
    .await
}

/// Observe the encrypted signal stream through the `ffres1` reducer and
/// report the bounded metadata-only projection (ids/state/counts only).
/// `rebuild` resets the cursor and re-observes the bounded window.
#[napi]
pub async fn resolution_protocol_status(root: String, rebuild: Option<bool>) -> Result<String> {
    execute_json(root, "resolution_protocol_status", move |workspace| {
        workspace.resolution_protocol_status(rebuild.unwrap_or(false))
    })
    .await
}

/// Publish the `ffres1` assignment profile (with the complete immutable job)
/// for one locally prepared job. JSON out: `{"message_id": "..."}`.
#[napi]
pub async fn resolution_assign(root: String, job_id: String) -> Result<String> {
    ensure_bounded(&job_id, MAX_HEX_ID_BYTES, "job id")?;
    execute_json(root, "resolution_assign", move |workspace| {
        workspace
            .resolution_assign(&job_id)
            .map(|message_id| serde_json::json!({ "message_id": message_id }))
    })
    .await
}

/// Publish the `ffres1` result profile for one locally submitted job.
/// JSON out: `{"message_id": "..."}`.
#[napi]
pub async fn resolution_reply(root: String, job_id: String) -> Result<String> {
    ensure_bounded(&job_id, MAX_HEX_ID_BYTES, "job id")?;
    execute_json(root, "resolution_reply", move |workspace| {
        workspace
            .resolution_reply(&job_id)
            .map(|message_id| serde_json::json!({ "message_id": message_id }))
    })
    .await
}

/// Publish the `ffres1` revoke/supersede profile for one local job.
/// JSON out: `{"message_id": "..."}`.
#[napi]
pub async fn resolution_revoke(
    root: String,
    job_id: String,
    superseded: Option<bool>,
) -> Result<String> {
    ensure_bounded(&job_id, MAX_HEX_ID_BYTES, "job id")?;
    execute_json(root, "resolution_revoke", move |workspace| {
        workspace
            .resolution_revoke(&job_id, superseded.unwrap_or(false))
            .map(|message_id| serde_json::json!({ "message_id": message_id }))
    })
    .await
}

/// Publish one typed human answer as an `ffres1` profile. JSON in:
/// `HumanResolutionAnswer`; JSON out: `{"message_id": "..."}`. The engine
/// validates the full answer (including `submit_candidate` verification
/// evidence) before sending.
#[napi]
pub async fn resolution_publish_answer(root: String, answer_json: String) -> Result<String> {
    let answer: feanorfs_common::HumanResolutionAnswer =
        parse_bounded_json(&answer_json, "resolution_publish_answer input")?;
    execute_json(root, "resolution_publish_answer", move |workspace| {
        workspace
            .resolution_publish_answer(&answer)
            .map(|message_id| serde_json::json!({ "message_id": message_id }))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_length_is_bounded() {
        let oversized = "x".repeat(MAX_ROOT_BYTES + 1);
        let error = ensure_bounded(&oversized, MAX_ROOT_BYTES, "workspace root").unwrap_err();
        assert!(error.to_string().contains("workspace root exceeds"));
        assert!(error.to_string().contains(&MAX_ROOT_BYTES.to_string()));
        ensure_bounded(
            &oversized[..MAX_ROOT_BYTES],
            MAX_ROOT_BYTES,
            "workspace root",
        )
        .expect("root at the exact bound is accepted");
    }

    #[test]
    fn input_json_is_bounded_and_parsed() {
        // A JSON string literal one byte over the adapter bound.
        let oversized = format!("\"{}\"", "x".repeat(MAX_ADAPTER_INPUT_BYTES));
        let error =
            parse_bounded_json::<serde_json::Value>(&oversized, "agent_send input").unwrap_err();
        assert!(error.to_string().contains("agent_send input exceeds"));

        let invalid =
            parse_bounded_json::<serde_json::Value>("{not json", "agent_inbox input").unwrap_err();
        assert!(error_invalid_json_message(invalid));
    }

    /// `parse_bounded_json` reports schema errors through the napi error
    /// message with the operation label.
    fn error_invalid_json_message(error: napi::Error) -> bool {
        error.to_string().contains("invalid agent_inbox input")
    }

    #[test]
    fn agent_path_accepts_exact_utf8() {
        let path = PathBuf::from("/tmp/ws/agents/worker");
        assert_eq!(
            agent_path_to_string(path, "worker").expect("UTF-8 path converts"),
            "/tmp/ws/agents/worker"
        );
    }

    #[cfg(unix)]
    #[test]
    fn agent_path_rejects_non_utf8_without_lossy_aliasing() {
        use std::os::unix::ffi::OsStringExt as _;

        let path = PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0xff]));
        let error = agent_path_to_string(path, "worker").expect_err("non-UTF-8 path must fail");
        assert!(error.to_string().contains("not valid UTF-8"));
        assert!(error.to_string().contains("worker"));
    }

    #[tokio::test]
    async fn executor_maps_open_failure_with_operation_name() {
        // The engine closure never runs: open fails first, and the mapped
        // napi error still carries the operation name.
        let result = execute(
            "/definitely/not/a/feanorfs/workspace".to_string(),
            "agent_list",
            |_workspace| anyhow::Ok(AgentListOfflineResult { agents: Vec::new() }),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("open must fail for a non-workspace path"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("agent_list"));
    }

    #[tokio::test]
    async fn executor_json_propagates_mapped_engine_errors() {
        let result = execute_json(
            "/definitely/not/a/feanorfs/workspace".to_string(),
            "agent_status",
            |_workspace| anyhow::Ok(()),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("open must fail for a non-workspace path"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("agent_status"));
    }

    #[tokio::test]
    async fn executor_never_runs_engine_when_open_fails() {
        // Contract: the engine closure is only invoked after the bounded
        // workspace open succeeds; an invalid root must not reach the engine.
        let result = execute(
            "x".repeat(MAX_ROOT_BYTES + 1),
            "agent_refresh",
            |_workspace: &Workspace| -> anyhow::Result<()> { anyhow::bail!("engine must not run") },
        )
        .await;
        let error = match result {
            Ok(_) => panic!("oversized root must fail before the engine runs"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("workspace root exceeds"));
    }
}
