use feanorfs_agent_core::{LandOptions, ResolveKeep, Runtime, SpawnOptions, Workspace};
use feanorfs_common::agent_contract::AgentListOfflineResult;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

const MAX_ADAPTER_INPUT_BYTES: usize = 1024 * 1024;
const MAX_ROOT_BYTES: usize = 32 * 1024;

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

async fn run<F, T>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    napi::tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::from_reason(format!("task join: {e}")))?
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
    run(move || {
        let names = open(&root)?
            .list()
            .map_err(|e| Error::from_reason(e.to_string()))?;
        serde_json::to_string(&AgentListOfflineResult { agents: names })
            .map_err(|e| Error::from_reason(e.to_string()))
    })
    .await
}

#[napi]
pub async fn agent_spawn(
    root: String,
    name: String,
    opts: Option<SpawnOptionsJs>,
) -> Result<String> {
    let opts = opts.unwrap_or(SpawnOptionsJs {
        no_sync: None,
        replace: None,
    });
    run(move || {
        let result = open(&root)?
            .spawn(
                &name,
                SpawnOptions {
                    no_sync: opts.no_sync.unwrap_or(false),
                    replace: opts.replace.unwrap_or(false),
                },
            )
            .map_err(|e| Error::from_reason(e.to_string()))?;
        serde_json::to_string(&result).map_err(|e| Error::from_reason(e.to_string()))
    })
    .await
}

#[napi]
pub async fn agent_path(root: String, name: String) -> Result<String> {
    run(move || {
        let path = open(&root)?
            .agent_path(&name)
            .map_err(|error| Error::from_reason(error.to_string()))?;
        path.into_os_string()
            .into_string()
            .map_err(|_| Error::from_reason("agent path is not valid UTF-8"))
    })
    .await
}

#[napi]
pub async fn agent_status(root: String, name: String) -> Result<String> {
    run(move || {
        let result = open(&root)?
            .status(&name)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        serde_json::to_string(&result).map_err(|e| Error::from_reason(e.to_string()))
    })
    .await
}

#[napi]
pub async fn agent_refresh(root: String, name: String) -> Result<String> {
    run(move || {
        let result = open(&root)?
            .refresh(&name)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        serde_json::to_string(&result).map_err(|e| Error::from_reason(e.to_string()))
    })
    .await
}

#[napi]
pub async fn agent_land(root: String, name: String, opts: Option<LandOptionsJs>) -> Result<String> {
    let opts = opts.unwrap_or(LandOptionsJs {
        clean: None,
        propose: None,
    });
    run(move || {
        let result = open(&root)?
            .land(
                &name,
                LandOptions {
                    clean: opts.clean.unwrap_or(false),
                    propose: opts.propose.unwrap_or(false),
                },
            )
            .map_err(|e| Error::from_reason(e.to_string()))?;
        serde_json::to_string(&result).map_err(|e| Error::from_reason(e.to_string()))
    })
    .await
}

#[napi]
pub async fn agent_clean(root: String, name: String) -> Result<String> {
    run(move || {
        let result = open(&root)?
            .clean(&name)
            .map_err(|e| Error::from_reason(e.to_string()))?;
        serde_json::to_string(&result).map_err(|e| Error::from_reason(e.to_string()))
    })
    .await
}

#[napi]
pub async fn history_log(root: String, limit: Option<u32>) -> Result<String> {
    run(move || {
        let result = open(&root)?
            .log(
                limit
                    .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
                    .unwrap_or(20),
            )
            .map_err(|error| Error::from_reason(error.to_string()))?;
        serde_json::to_string(&result).map_err(|error| Error::from_reason(error.to_string()))
    })
    .await
}

#[napi]
pub async fn undo(root: String, snapshot_id: String) -> Result<String> {
    run(move || {
        let result = open(&root)?
            .undo(&snapshot_id)
            .map_err(|error| Error::from_reason(error.to_string()))?;
        serde_json::to_string(&result).map_err(|error| Error::from_reason(error.to_string()))
    })
    .await
}

/// Send an encrypted agent signal. JSON in: `AgentMessageInput`; JSON out: `AgentSendResult`.
#[napi]
pub async fn agent_send(root: String, input_json: String) -> Result<String> {
    run(move || {
        let input: feanorfs_common::AgentMessageInput =
            parse_bounded_json(&input_json, "agent_send input")?;
        let result = open(&root)?
            .send_message(input)
            .map_err(|error| Error::from_reason(error.to_string()))?;
        serde_json::to_string(&result).map_err(|error| Error::from_reason(error.to_string()))
    })
    .await
}

/// Read agent signals. JSON in: `AgentInboxQuery`; JSON out: `AgentInboxResult`.
#[napi]
pub async fn agent_inbox(root: String, query_json: String) -> Result<String> {
    run(move || {
        let query: feanorfs_common::AgentInboxQuery =
            parse_bounded_json(&query_json, "agent_inbox input")?;
        let result = open(&root)?
            .inbox(query)
            .map_err(|error| Error::from_reason(error.to_string()))?;
        serde_json::to_string(&result).map_err(|error| Error::from_reason(error.to_string()))
    })
    .await
}

/// keep: 0=local, 1=cloud, 2=both, 3=file (requires filePath)
#[napi]
pub async fn conflicts_keep(
    root: String,
    path: String,
    keep: i32,
    file_path: Option<String>,
) -> Result<()> {
    run(move || {
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
        open(&root)?
            .resolve(&path, keep, file_ref.as_deref())
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(())
    })
    .await
}
/// Assign one batch to a randomly ranked integrator.
/// JSON in: `IntegratorAssignInput`; JSON out: `IntegratorAssignResult`.
#[napi]
pub async fn integrator_assign(root: String, input_json: String) -> Result<String> {
    run(move || {
        let input: feanorfs_common::IntegratorAssignInput =
            parse_bounded_json(&input_json, "integrator_assign input")?;
        let result = open(&root)?
            .integrator_assign(input)
            .map_err(|error| Error::from_reason(error.to_string()))?;
        serde_json::to_string(&result).map_err(|error| Error::from_reason(error.to_string()))
    })
    .await
}

/// Read the active integrator assignment (or one by id).
/// JSON out: `IntegratorStatusResult`.
#[napi]
pub async fn integrator_status(root: String, assignment_id: Option<String>) -> Result<String> {
    run(move || {
        let result = open(&root)?
            .integrator_status(assignment_id.as_deref())
            .map_err(|error| Error::from_reason(error.to_string()))?;
        serde_json::to_string(&result).map_err(|error| Error::from_reason(error.to_string()))
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
    run(move || {
        let result = open(&root)?
            .integrator_revoke(&assignment_id, &reason)
            .map_err(|error| Error::from_reason(error.to_string()))?;
        serde_json::to_string(&result).map_err(|error| Error::from_reason(error.to_string()))
    })
    .await
}

/// Resume dispatcher observation after a restart.
/// Options JSON: object with optional `ack_timeout_ms` and `fallback_on_blocked`;
/// JSON out: `IntegratorObserveResult`.
#[napi]
pub async fn integrator_resume(root: String, options_json: Option<String>) -> Result<String> {
    run(move || {
        let (ack_timeout_ms, fallback_on_blocked) = match options_json {
            None => (None, false),
            Some(json) => {
                let input: feanorfs_common::IntegratorObserveInput =
                    parse_bounded_json(&json, "integrator_resume options")?;
                (input.ack_timeout_ms, input.fallback_on_blocked)
            }
        };
        let result = open(&root)?
            .integrator_resume(feanorfs_agent_core::IntegratorObserveOptions {
                ack_timeout_ms,
                fallback_on_blocked,
            })
            .map_err(|error| Error::from_reason(error.to_string()))?;
        serde_json::to_string(&result).map_err(|error| Error::from_reason(error.to_string()))
    })
    .await
}

/// Materialize the encrypted conflict triple for a snapshot.
/// JSON in: object with `about_snapshot` and exactly one of non-empty `paths` or `all: true`;
/// JSON out: `ConflictMaterializeResult`.
#[napi]
pub async fn conflict_materialize(root: String, input_json: String) -> Result<String> {
    run(move || {
        let input: feanorfs_common::ConflictMaterializeInput =
            parse_bounded_json(&input_json, "conflict_materialize input")?;
        let (about_snapshot, paths) = input.validate().map_err(|error| {
            Error::from_reason(format!("invalid conflict_materialize input: {error}"))
        })?;
        let result = open(&root)?
            .materialize_conflicts(&about_snapshot, &paths)
            .map_err(|error| Error::from_reason(error.to_string()))?;
        serde_json::to_string(&result).map_err(|error| Error::from_reason(error.to_string()))
    })
    .await
}
