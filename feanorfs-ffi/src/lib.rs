//! C ABI: JSON strings in/out. See `feanorfs.h` and `docs/agent-api.md`.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use feanorfs_agent_core::{LandOptions, ResolveKeep, Runtime, SpawnOptions, Workspace};
use feanorfs_common::agent_contract::AgentListOfflineResult;
use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::panic;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Mutex};

thread_local! {
    static LAST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

static RUNTIME: Mutex<Option<Arc<Runtime>>> = Mutex::new(None);

fn set_error(msg: impl Into<String>) {
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(msg.into()));
}

fn clear_error() {
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
}

fn ok_json<T: serde::Serialize>(value: &T) -> *mut c_char {
    match serde_json::to_string(value) {
        Ok(s) => CString::new(s)
            .map(CString::into_raw)
            .unwrap_or(ptr::null_mut()),
        Err(e) => {
            set_error(e.to_string());
            ptr::null_mut()
        }
    }
}

fn ok_string(value: impl Into<Vec<u8>>) -> *mut c_char {
    CString::new(value)
        .map(CString::into_raw)
        .unwrap_or(ptr::null_mut())
}

fn runtime() -> Result<Arc<Runtime>, String> {
    RUNTIME
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
        .ok_or_else(|| "call ffs_runtime_init first".to_string())
}

/// Each agent call opens the workspace fresh (new cache DB pool + transport).
/// This is intentional: callers pass `root` every time; no handle API yet.
fn workspace(root: *const c_char) -> Result<Workspace, String> {
    let root = cstr_req(root)?;
    let rt = runtime()?;
    Workspace::open(&rt, Path::new(root)).map_err(|e| e.to_string())
}

fn cstr_opt(ptr: *const c_char) -> Result<Option<String>, String> {
    if ptr.is_null() {
        return Ok(None);
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map(|s| Some(s.to_string()))
        .map_err(|e| e.to_string())
}

/// Read a required C string argument, rejecting NULL (which would be
/// undefined behavior in `CStr::from_ptr`) before dereferencing. The returned
/// slice borrows the caller's C string, which must outlive the call.
fn cstr_req<'a>(ptr: *const c_char) -> Result<&'a str, String> {
    if ptr.is_null() {
        return Err("required string argument is null".to_string());
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|e| e.to_string())
}

/// Initialize the shared Tokio runtime. Call once before any other `ffs_*` function.
/// Returns `0` on success, `-1` on error (see `ffs_last_error`).
#[no_mangle]
pub extern "C" fn ffs_runtime_init() -> i32 {
    catch_i32(|| match Runtime::new() {
        Ok(rt) => match RUNTIME.lock() {
            Ok(mut guard) => {
                *guard = Some(rt);
                clear_error();
                0
            }
            Err(e) => {
                set_error(e.to_string());
                -1
            }
        },
        Err(e) => {
            set_error(e.to_string());
            -1
        }
    })
}

/// Free a string previously returned by any `ffs_*` function (including `ffs_last_error`).
#[no_mangle]
pub extern "C" fn ffs_string_free(s: *mut c_char) {
    if !s.is_null() {
        unsafe {
            drop(CString::from_raw(s));
        }
    }
}

/// Last error on **this thread** from the most recent failing `ffs_*` call.
/// Caller must free with `ffs_string_free`. Never NULL (empty string if no error).
#[no_mangle]
pub extern "C" fn ffs_last_error() -> *mut c_char {
    catch_ptr(|| {
        let msg = LAST_ERROR.with(|e| e.borrow().clone()).unwrap_or_default();
        CString::new(msg)
            .map(CString::into_raw)
            .unwrap_or(ptr::null_mut())
    })
}

/// List agent workspace names. JSON: `AgentListOfflineResult`. NULL on error.
#[no_mangle]
pub extern "C" fn ffs_agent_list(root: *const c_char) -> *mut c_char {
    catch_ptr(|| {
        clear_error();
        match workspace(root) {
            Ok(ws) => match ws.list() {
                Ok(names) => ok_json(&AgentListOfflineResult { agents: names }),
                Err(e) => {
                    set_error(e.to_string());
                    ptr::null_mut()
                }
            },
            Err(e) => {
                set_error(e.to_string());
                ptr::null_mut()
            }
        }
    })
}

/// Spawn an isolated agent workspace. JSON: `SpawnResult`. NULL on error.
#[no_mangle]
pub extern "C" fn ffs_agent_spawn(
    root: *const c_char,
    name: *const c_char,
    no_sync: i32,
    replace: i32,
) -> *mut c_char {
    catch_ptr(|| {
        clear_error();
        let name = match cstr_req(name) {
            Ok(s) => s,
            Err(e) => {
                set_error(e);
                return ptr::null_mut();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.spawn(
                name,
                SpawnOptions {
                    no_sync: no_sync != 0,
                    replace: replace != 0,
                },
            ) {
                Ok(r) => ok_json(&r),
                Err(e) => {
                    set_error(e.to_string());
                    ptr::null_mut()
                }
            },
            Err(e) => {
                set_error(e.to_string());
                ptr::null_mut()
            }
        }
    })
}

/// Return the absolute worktree path for an existing agent. NULL on error.
#[no_mangle]
pub extern "C" fn ffs_agent_path(root: *const c_char, name: *const c_char) -> *mut c_char {
    catch_ptr(|| {
        clear_error();
        let name = match cstr_req(name) {
            Ok(name) => name,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        match workspace(root).and_then(|ws| ws.agent_path(name).map_err(|error| error.to_string()))
        {
            Ok(path) => ok_string(path.to_string_lossy().as_bytes()),
            Err(error) => {
                set_error(error.to_string());
                ptr::null_mut()
            }
        }
    })
}

/// Preview one agent's changes. JSON: `AgentCheckResult`. NULL on error.
#[no_mangle]
pub extern "C" fn ffs_agent_status(root: *const c_char, name: *const c_char) -> *mut c_char {
    catch_ptr(|| agent_by_name(root, name, |ws, name| ws.status(name)))
}

/// Pull cloud changes into the agent. JSON: `AgentRefreshResult`. NULL on error.
#[no_mangle]
pub extern "C" fn ffs_agent_refresh(root: *const c_char, name: *const c_char) -> *mut c_char {
    catch_ptr(|| agent_by_name(root, name, |ws, name| ws.refresh(name)))
}

/// Integrate agent work into the main workspace. JSON: `AgentLandResult`. NULL on error.
#[no_mangle]
pub extern "C" fn ffs_agent_land(
    root: *const c_char,
    name: *const c_char,
    clean: i32,
    propose: i32,
) -> *mut c_char {
    catch_ptr(|| {
        clear_error();
        let name = match cstr_req(name) {
            Ok(s) => s,
            Err(e) => {
                set_error(e);
                return ptr::null_mut();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.land(
                name,
                LandOptions {
                    clean: clean != 0,
                    propose: propose != 0,
                },
            ) {
                Ok(r) => ok_json(&r),
                Err(e) => {
                    set_error(e.to_string());
                    ptr::null_mut()
                }
            },
            Err(e) => {
                set_error(e.to_string());
                ptr::null_mut()
            }
        }
    })
}

/// Remove an agent workspace. JSON: `AgentCleanResult`. NULL on error.
#[no_mangle]
pub extern "C" fn ffs_agent_clean(root: *const c_char, name: *const c_char) -> *mut c_char {
    catch_ptr(|| agent_by_name(root, name, |ws, name| ws.clean(name)))
}

/// List reachable workspace history. JSON: `LogResult`. NULL on error.
#[no_mangle]
pub extern "C" fn ffs_log(root: *const c_char, limit: u32) -> *mut c_char {
    catch_ptr(|| {
        clear_error();
        match workspace(root) {
            Ok(ws) => match ws.log(usize::try_from(limit).unwrap_or(usize::MAX)) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null_mut()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null_mut()
            }
        }
    })
}

/// Restore a reachable snapshot as a new snapshot. JSON: `UndoResult`. NULL on error.
#[no_mangle]
pub extern "C" fn ffs_undo(root: *const c_char, snapshot_id: *const c_char) -> *mut c_char {
    catch_ptr(|| {
        clear_error();
        let snapshot_id = match cstr_req(snapshot_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.undo(snapshot_id) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null_mut()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null_mut()
            }
        }
    })
}

/// Send an encrypted agent signal. JSON in: `AgentMessageInput`; JSON out:
/// `AgentSendResult`. NULL on error.
#[no_mangle]
pub extern "C" fn ffs_agent_send(root: *const c_char, input_json: *const c_char) -> *mut c_char {
    catch_ptr(|| {
        clear_error();
        let input_json = match cstr_req(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        let input: feanorfs_common::AgentMessageInput = match serde_json::from_str(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid agent_send input: {error}"));
                return ptr::null_mut();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.send_message(input) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null_mut()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null_mut()
            }
        }
    })
}

/// Read agent signals. JSON in: `AgentInboxQuery`; JSON out: `AgentInboxResult`.
/// NULL on error.
#[no_mangle]
pub extern "C" fn ffs_agent_inbox(root: *const c_char, query_json: *const c_char) -> *mut c_char {
    catch_ptr(|| {
        clear_error();
        let query_json = match cstr_req(query_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        let query: feanorfs_common::AgentInboxQuery = match serde_json::from_str(query_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid agent_inbox input: {error}"));
                return ptr::null_mut();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.inbox(query) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null_mut()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null_mut()
            }
        }
    })
}

/// Resolve a pending conflict. Returns `0` on success, `-1` on error.
/// `keep`: 0=local, 1=cloud, 2=both, 3=file (requires non-null `file_path`).
#[no_mangle]
pub extern "C" fn ffs_conflicts_keep(
    root: *const c_char,
    path: *const c_char,
    keep: i32,
    file_path: *const c_char,
) -> i32 {
    catch_i32(|| {
        clear_error();
        let path = match cstr_req(path) {
            Ok(s) => s,
            Err(e) => {
                set_error(e);
                return -1;
            }
        };
        let file_source = match (keep, cstr_opt(file_path)) {
            (_, Err(e)) => {
                set_error(e.to_string());
                return -1;
            }
            (3, Ok(None)) => {
                set_error("keep=3 (file) requires non-null file_path");
                return -1;
            }
            (3, Ok(Some(p))) => Some(PathBuf::from(p)),
            (_, Ok(_)) => None,
        };
        let keep = match keep {
            0 => ResolveKeep::Local,
            1 => ResolveKeep::Cloud,
            2 => ResolveKeep::Both,
            3 => ResolveKeep::File,
            _ => {
                set_error("keep must be 0=local, 1=cloud, 2=both, 3=file");
                return -1;
            }
        };
        let file_ref = file_source.as_deref();
        match workspace(root) {
            Ok(ws) => match ws.resolve(path, keep, file_ref) {
                Ok(()) => 0,
                Err(e) => {
                    set_error(e.to_string());
                    -1
                }
            },
            Err(e) => {
                set_error(e.to_string());
                -1
            }
        }
    })
}

fn agent_by_name<T: serde::Serialize>(
    root: *const c_char,
    name: *const c_char,
    f: impl FnOnce(&Workspace, &str) -> Result<T, anyhow::Error>,
) -> *mut c_char {
    clear_error();
    let name = match cstr_req(name) {
        Ok(s) => s,
        Err(e) => {
            set_error(e.to_string());
            return ptr::null_mut();
        }
    };
    match workspace(root) {
        Ok(ws) => match f(&ws, name) {
            Ok(r) => ok_json(&r),
            Err(e) => {
                set_error(e.to_string());
                ptr::null_mut()
            }
        },
        Err(e) => {
            set_error(e.to_string());
            ptr::null_mut()
        }
    }
}

fn catch_ptr(f: impl FnOnce() -> *mut c_char + panic::UnwindSafe) -> *mut c_char {
    match panic::catch_unwind(f) {
        Ok(ptr) => ptr,
        Err(_) => {
            set_error("internal panic");
            ptr::null_mut()
        }
    }
}

fn catch_i32(f: impl FnOnce() -> i32 + panic::UnwindSafe) -> i32 {
    match panic::catch_unwind(f) {
        Ok(code) => code,
        Err(_) => {
            set_error("internal panic");
            -1
        }
    }
}

/// Assign one batch to a randomly ranked integrator. JSON in:
/// `IntegratorAssignInput`; JSON out: `IntegratorAssignResult`. NULL on error.
#[no_mangle]
pub extern "C" fn ffs_integrator_assign(
    root: *const c_char,
    input_json: *const c_char,
) -> *mut c_char {
    catch_ptr(|| {
        clear_error();
        let input_json = match cstr_req(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        let input: feanorfs_common::IntegratorAssignInput = match serde_json::from_str(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid integrator_assign input: {error}"));
                return ptr::null_mut();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.integrator_assign(input) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null_mut()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null_mut()
            }
        }
    })
}

/// Read the active integrator assignment (or one by id). JSON out:
/// `IntegratorStatusResult`. NULL on error; pass NULL `assignment_id` for the
/// active assignment.
#[no_mangle]
pub extern "C" fn ffs_integrator_status(
    root: *const c_char,
    assignment_id: *const c_char,
) -> *mut c_char {
    catch_ptr(|| {
        clear_error();
        let assignment_id = match cstr_opt(assignment_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.integrator_status(assignment_id.as_deref()) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null_mut()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null_mut()
            }
        }
    })
}

/// Explicitly revoke the active integrator assignment. JSON out:
/// `IntegratorStatusResult`. NULL on error.
#[no_mangle]
pub extern "C" fn ffs_integrator_revoke(
    root: *const c_char,
    assignment_id: *const c_char,
    reason: *const c_char,
) -> *mut c_char {
    catch_ptr(|| {
        clear_error();
        let assignment_id = match cstr_req(assignment_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        let reason = match cstr_req(reason) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.integrator_revoke(assignment_id, reason) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null_mut()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null_mut()
            }
        }
    })
}

/// Resume dispatcher observation after a restart. JSON in: object with
/// optional `ack_timeout_ms` (u64) and `fallback_on_blocked` (bool); JSON out:
/// `IntegratorObserveResult`. NULL on error; pass NULL `options_json` for
/// conservative defaults.
#[no_mangle]
pub extern "C" fn ffs_integrator_resume(
    root: *const c_char,
    options_json: *const c_char,
) -> *mut c_char {
    catch_ptr(|| {
        clear_error();
        let (ack_timeout_ms, fallback_on_blocked) = match cstr_opt(options_json) {
            Ok(None) => (None, false),
            Ok(Some(json)) => {
                let value: serde_json::Value = match serde_json::from_str(&json) {
                    Ok(value) => value,
                    Err(error) => {
                        set_error(format!("invalid integrator_resume options: {error}"));
                        return ptr::null_mut();
                    }
                };
                (
                    value.get("ack_timeout_ms").and_then(|v| v.as_u64()),
                    value
                        .get("fallback_on_blocked")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                )
            }
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.integrator_resume(feanorfs_agent_core::IntegratorObserveOptions {
                ack_timeout_ms,
                fallback_on_blocked,
            }) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null_mut()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null_mut()
            }
        }
    })
}

/// Materialize the encrypted conflict triple for a snapshot. JSON in: object
/// with `about_snapshot` (string, optional: defaults to the head) and `paths`
/// (array of strings, optional); JSON out: `ConflictMaterializeResult`.
/// NULL on error.
#[no_mangle]
pub extern "C" fn ffs_conflict_materialize(
    root: *const c_char,
    input_json: *const c_char,
) -> *mut c_char {
    catch_ptr(|| {
        clear_error();
        let input_json = match cstr_req(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null_mut();
            }
        };
        let value: serde_json::Value = match serde_json::from_str(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid conflict_materialize input: {error}"));
                return ptr::null_mut();
            }
        };
        let about = match value.get("about_snapshot").and_then(|v| v.as_str()) {
            Some(snapshot) => snapshot.to_string(),
            None => {
                set_error("conflict_materialize requires about_snapshot");
                return ptr::null_mut();
            }
        };
        let paths: Vec<String> = value
            .get("paths")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        match workspace(root) {
            Ok(ws) => match ws.materialize_conflicts(&about, &paths) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null_mut()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null_mut()
            }
        }
    })
}

#[cfg(test)]
mod smoke {
    use feanorfs_agent_core::sync_pass::{run_sync_pass, SyncMode};
    use feanorfs_agent_core::{
        ensure_workspace_state, load_config, save_config, ApiClient, ClientDb, Config, Runtime,
        SyncCtx, LOCAL_HUB_URL,
    };
    use std::ffi::{CStr, CString};
    use std::fs;

    use super::*;

    fn cstr(ptr: *mut c_char) -> String {
        unsafe {
            let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
            ffs_string_free(ptr);
            s
        }
    }

    fn last_err() -> String {
        cstr(ffs_last_error())
    }

    #[test]
    fn null_required_strings_fail_cleanly_instead_of_crashing() {
        // NULL root / name must never reach CStr::from_ptr (undefined
        // behavior); the ABI returns NULL plus a thread-local error instead.
        assert_eq!(ffs_runtime_init(), 0);
        let result = ffs_agent_list(std::ptr::null());
        assert!(result.is_null());
        assert!(!last_err().is_empty());
        ffs_string_free(result);
    }

    fn setup_ws() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let key = feanorfs_common::generate_password().unwrap();
        save_config(
            &ws,
            &Config {
                server_url: LOCAL_HUB_URL.to_string(),
                workspace_id: "ffi-test".into(),
                encryption_password: Some(key),
                server_password: None,
                tls_ca_pem: None,
                format_version: 2,
                hub_local: true,
                relay: None,
            },
        )
        .unwrap();
        fs::write(ws.join("seed.txt"), b"seed").unwrap();
        (tmp, ws)
    }

    fn prepare_format_v3_workspace(ws: &Path) {
        let runtime = Runtime::new().unwrap();
        let mut config = load_config(ws).unwrap();
        config.format_version = 3;
        save_config(ws, &config).unwrap();
        let db = runtime
            .block_on(ClientDb::new(ensure_workspace_state(ws).unwrap()))
            .unwrap();
        let api = runtime
            .block_on(ApiClient::from_config(ws, &config))
            .unwrap();
        let ctx = SyncCtx::from_config(&api, &db, ws, &config).unwrap();
        runtime
            .block_on(run_sync_pass(&ctx, SyncMode::Full, false))
            .unwrap();
    }

    #[test]
    fn spawn_land_local_hub() {
        let (_tmp, ws) = setup_ws();
        assert_eq!(ffs_runtime_init(), 0);

        let root = CString::new(ws.to_string_lossy().as_ref()).unwrap();
        let name = CString::new("ffi1").unwrap();

        let spawn_json = ffs_agent_spawn(root.as_ptr(), name.as_ptr(), 0, 0);
        assert!(!spawn_json.is_null(), "spawn failed: {}", last_err());
        assert!(cstr(spawn_json).contains("files_copied"));

        let agent_path = ffs_agent_path(root.as_ptr(), name.as_ptr());
        assert!(!agent_path.is_null(), "agent path failed: {}", last_err());
        let agent_dir = PathBuf::from(cstr(agent_path));
        assert!(agent_dir.is_dir());
        assert!(!agent_dir.starts_with(&ws));
        assert!(!ws.join(".feanorfs").exists());
        fs::write(agent_dir.join("note.txt"), b"ffi edit").unwrap();

        let land_json = ffs_agent_land(root.as_ptr(), name.as_ptr(), 0, 0);
        assert!(!land_json.is_null(), "land failed: {}", last_err());
        let _ = cstr(land_json);

        let log_json = ffs_log(root.as_ptr(), 10);
        assert!(!log_json.is_null(), "log failed: {}", last_err());
        let log: feanorfs_common::LogResult = serde_json::from_str(&cstr(log_json)).unwrap();
        let target = log.entries[0].parents.last().unwrap();
        let target = CString::new(target.as_str()).unwrap();
        let undo_json = ffs_undo(root.as_ptr(), target.as_ptr());
        assert!(!undo_json.is_null(), "undo failed: {}", last_err());
        let undo: feanorfs_common::UndoResult = serde_json::from_str(&cstr(undo_json)).unwrap();
        assert_eq!(undo.restored_snapshot_id, target.to_str().unwrap());

        let clean_json = ffs_agent_clean(root.as_ptr(), name.as_ptr());
        assert!(!clean_json.is_null());
        let _ = cstr(clean_json);
    }

    #[test]
    fn spawn_twice_without_replace_fails() {
        let (_tmp, ws) = setup_ws();
        assert_eq!(ffs_runtime_init(), 0);

        let root = CString::new(ws.to_string_lossy().as_ref()).unwrap();
        let name = CString::new("dup").unwrap();

        assert!(!ffs_agent_spawn(root.as_ptr(), name.as_ptr(), 0, 0).is_null());
        let second = ffs_agent_spawn(root.as_ptr(), name.as_ptr(), 0, 0);
        assert!(second.is_null());
        assert!(!last_err().is_empty());
    }

    #[test]
    fn conflicts_keep_invalid_keep() {
        let (_tmp, ws) = setup_ws();
        assert_eq!(ffs_runtime_init(), 0);

        let root = CString::new(ws.to_string_lossy().as_ref()).unwrap();
        let path = CString::new("missing.txt").unwrap();
        assert_eq!(
            ffs_conflicts_keep(root.as_ptr(), path.as_ptr(), 99, ptr::null()),
            -1
        );
        assert!(!last_err().is_empty());
    }

    #[test]
    fn ops_before_init_fail() {
        let root = CString::new("/tmp/nope").unwrap();
        let name = CString::new("x").unwrap();
        assert!(ffs_agent_list(root.as_ptr()).is_null());
        assert!(!last_err().is_empty());
        clear_error();
        assert!(ffs_agent_spawn(root.as_ptr(), name.as_ptr(), 0, 0).is_null());
        assert!(!last_err().is_empty());
    }

    #[test]
    fn agent_send_inbox_roundtrip_local_hub() {
        let (_tmp, ws) = setup_ws();
        prepare_format_v3_workspace(&ws);
        assert_eq!(ffs_runtime_init(), 0);

        let root = CString::new(ws.to_string_lossy().as_ref()).unwrap();
        let name = CString::new("ffi2").unwrap();
        assert!(!ffs_agent_spawn(root.as_ptr(), name.as_ptr(), 0, 0).is_null());
        let land_json = ffs_agent_land(root.as_ptr(), name.as_ptr(), 0, 0);
        assert!(!land_json.is_null(), "land failed: {}", last_err());
        let _ = cstr(land_json);

        let input = serde_json::to_string(&feanorfs_common::AgentMessageInput {
            to: "mac-test".to_string(),
            kind: feanorfs_common::AgentMessageKind::Request,
            body: "Run iOS simulator tests".to_string(),
            about_snapshot: None,
            reply_to: None,
            from: Some("ffi-sender".to_string()),
        })
        .unwrap();
        let input = CString::new(input).unwrap();
        let send_json = ffs_agent_send(root.as_ptr(), input.as_ptr());
        assert!(!send_json.is_null(), "send failed: {}", last_err());
        let send: feanorfs_common::AgentSendResult =
            serde_json::from_str(&cstr(send_json)).unwrap();

        let query = serde_json::to_string(&feanorfs_common::AgentInboxQuery {
            recipient: "mac-test".to_string(),
            after: None,
            limit: 50,
        })
        .unwrap();
        let query = CString::new(query).unwrap();
        let inbox_json = ffs_agent_inbox(root.as_ptr(), query.as_ptr());
        assert!(!inbox_json.is_null(), "inbox failed: {}", last_err());
        let inbox: feanorfs_common::AgentInboxResult =
            serde_json::from_str(&cstr(inbox_json)).unwrap();
        assert_eq!(inbox.messages.len(), 1);
        assert_eq!(inbox.messages[0].message_id, send.message_id);
        assert_eq!(inbox.messages[0].from, "ffi-sender");
        assert_eq!(inbox.messages[0].body, "Run iOS simulator tests");
        assert!(!inbox.cursor_reset);

        let bad = CString::new("not json").unwrap();
        assert!(ffs_agent_send(root.as_ptr(), bad.as_ptr()).is_null());
        assert!(!last_err().is_empty());
    }

    #[test]
    fn integrator_assign_status_revoke_local_hub() {
        let (_tmp, ws) = setup_ws();
        prepare_format_v3_workspace(&ws);
        assert_eq!(ffs_runtime_init(), 0);

        let root = CString::new(ws.to_string_lossy().as_ref()).unwrap();
        let runtime = Runtime::new().unwrap();
        let config = load_config(&ws).unwrap();
        let db = runtime
            .block_on(ClientDb::new(ensure_workspace_state(&ws).unwrap()))
            .unwrap();
        let api = runtime
            .block_on(ApiClient::from_config(&ws, &config))
            .unwrap();
        let ctx = SyncCtx::from_config(&api, &db, &ws, &config).unwrap();
        let head = runtime
            .block_on(ctx.api.get_head(ctx.workspace_id()))
            .unwrap()
            .unwrap();

        let input = serde_json::json!({
            "about_snapshot": head,
            "candidates": [
                { "name": "agent-a", "capabilities": ["rust"] },
                { "name": "agent-b", "capabilities": ["rust", "ios"] }
            ],
            "required_capabilities": ["rust"],
            "task_summary": "Integrate parser implementation and tests",
            "ack_timeout_ms": 300000
        });
        let input = CString::new(serde_json::to_string(&input).unwrap()).unwrap();
        let assign_json = ffs_integrator_assign(root.as_ptr(), input.as_ptr());
        assert!(
            !assign_json.is_null(),
            "integrator assign failed: {}",
            last_err()
        );
        let assign: feanorfs_common::IntegratorAssignResult =
            serde_json::from_str(&cstr(assign_json)).unwrap();
        assert_eq!(assign.attempt, 0);
        assert!(assign.fallback_order.len() == 1);
        assert_eq!(
            assign.state,
            feanorfs_common::IntegratorAssignmentState::Offered
        );

        let id = CString::new(assign.assignment_id.clone()).unwrap();
        let status_json = ffs_integrator_status(root.as_ptr(), id.as_ptr());
        assert!(
            !status_json.is_null(),
            "integrator status failed: {}",
            last_err()
        );
        let status: feanorfs_common::IntegratorStatusResult =
            serde_json::from_str(&cstr(status_json)).unwrap();
        assert_eq!(status.assignment_id, assign.assignment_id);
        assert_eq!(
            status.state,
            feanorfs_common::IntegratorAssignmentState::Offered
        );

        // A second dispatcher must fail closed on the orchestration lock while
        // the first holds it only during operations, so a second assign fails
        // because an assignment is already active.
        let again = CString::new(
            serde_json::to_string(&serde_json::json!({
                "about_snapshot": head,
                "candidates": [{ "name": "agent-c", "capabilities": [] }],
                "task_summary": "second batch"
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(ffs_integrator_assign(root.as_ptr(), again.as_ptr()).is_null());

        let reason = CString::new("integrator went quiet").unwrap();
        let revoke_json = ffs_integrator_revoke(root.as_ptr(), id.as_ptr(), reason.as_ptr());
        assert!(
            !revoke_json.is_null(),
            "integrator revoke failed: {}",
            last_err()
        );
        let revoked: feanorfs_common::IntegratorStatusResult =
            serde_json::from_str(&cstr(revoke_json)).unwrap();
        assert_eq!(
            revoked.state,
            feanorfs_common::IntegratorAssignmentState::Cancelled,
            "revoking an offered attempt cancels the assignment"
        );

        // Resume with no active assignment is a no-op.
        let resume_json = ffs_integrator_resume(root.as_ptr(), ptr::null());
        assert!(
            !resume_json.is_null(),
            "integrator resume failed: {}",
            last_err()
        );
        let resume: feanorfs_common::IntegratorObserveResult =
            serde_json::from_str(&cstr(resume_json)).unwrap();
        assert_eq!(resume.action, "none");
    }
}
