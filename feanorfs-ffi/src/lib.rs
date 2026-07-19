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
    let root = unsafe { CStr::from_ptr(root) }
        .to_str()
        .map_err(|e| e.to_string())?;
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
                set_error(e);
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
        let name = match unsafe { CStr::from_ptr(name) }.to_str() {
            Ok(s) => s,
            Err(e) => {
                set_error(e.to_string());
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
                set_error(e);
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
        let name = match unsafe { CStr::from_ptr(name) }.to_str() {
            Ok(name) => name,
            Err(error) => {
                set_error(error.to_string());
                return ptr::null_mut();
            }
        };
        match workspace(root).and_then(|ws| ws.agent_path(name).map_err(|error| error.to_string()))
        {
            Ok(path) => ok_string(path.to_string_lossy().as_bytes()),
            Err(error) => {
                set_error(error);
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
        let name = match unsafe { CStr::from_ptr(name) }.to_str() {
            Ok(s) => s,
            Err(e) => {
                set_error(e.to_string());
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
                set_error(e);
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
                set_error(error);
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
        let snapshot_id = match unsafe { CStr::from_ptr(snapshot_id) }.to_str() {
            Ok(value) => value,
            Err(error) => {
                set_error(error.to_string());
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
                set_error(error);
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
        let path = match unsafe { CStr::from_ptr(path) }.to_str() {
            Ok(s) => s,
            Err(e) => {
                set_error(e.to_string());
                return -1;
            }
        };
        let file_source = match (keep, cstr_opt(file_path)) {
            (_, Err(e)) => {
                set_error(e);
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
                set_error(e);
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
    let name = match unsafe { CStr::from_ptr(name) }.to_str() {
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
            set_error(e);
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

#[cfg(test)]
mod smoke {
    use feanorfs_agent_core::{save_config, Config, LOCAL_HUB_URL};
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
}
