use feanorfs_agent_core::{LandOptions, ResolveKeep, Runtime, SpawnOptions, Workspace};
use feanorfs_common::agent_contract::AgentListOfflineResult;
use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

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
