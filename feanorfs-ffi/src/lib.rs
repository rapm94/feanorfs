//! C ABI: JSON strings in/out. See `feanorfs.h` and `docs/agent-api.md`.
#[cfg(test)]
feanorfs_test_support::isolate_test_process!();

use feanorfs_agent_core::{LandOptions, ResolveKeep, Runtime, SpawnOptions, Workspace};
use feanorfs_common::agent_contract::AgentListOfflineResult;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::c_char;
use std::panic;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, LazyLock, Mutex};

thread_local! {
    static LAST_ERROR: RefCell<Option<String>> = const { RefCell::new(None) };
}

static RUNTIME: Mutex<Option<Arc<Runtime>>> = Mutex::new(None);
static RETURNED_STRINGS: LazyLock<Mutex<HashMap<usize, Box<[u8]>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const MAX_FFI_INPUT_BYTES: usize = 1024 * 1024;
const MAX_FFI_OUTPUT_BYTES: usize = 1024 * 1024;

fn set_error(msg: impl Into<String>) {
    let msg = msg.into();
    let mut safe = String::new();
    for character in msg.chars().take(4096) {
        if character.is_control() {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    LAST_ERROR.with(|error| *error.borrow_mut() = Some(safe));
}

fn clear_error() {
    LAST_ERROR.with(|e| *e.borrow_mut() = None);
}

fn return_string(value: impl Into<Vec<u8>>) -> *const c_char {
    let cstring = match CString::new(value) {
        Ok(value) => value,
        Err(error) => {
            set_error(error.to_string());
            return ptr::null();
        }
    };
    let mut allocation = cstring.into_bytes_with_nul().into_boxed_slice();
    let pointer = allocation.as_mut_ptr().cast::<c_char>() as *const c_char;
    RETURNED_STRINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(pointer as usize, allocation);
    pointer
}

/// `serde_json` writes incrementally, so an over-bound result fails closed
/// with a typed overflow error instead of building an unbounded buffer.
struct BoundedJsonWriter {
    buffer: Vec<u8>,
    limit: usize,
    overflowed: bool,
}

impl std::io::Write for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.buffer.len().saturating_add(bytes.len()) > self.limit {
            self.overflowed = true;
            return Err(std::io::Error::other(
                "serialized output exceeds the FFI bound",
            ));
        }
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn ok_json<T: serde::Serialize>(value: &T) -> *const c_char {
    let mut writer = BoundedJsonWriter {
        buffer: Vec::with_capacity(MAX_FFI_OUTPUT_BYTES.min(64 * 1024)),
        limit: MAX_FFI_OUTPUT_BYTES,
        overflowed: false,
    };
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) if !writer.overflowed => return_string(writer.buffer),
        Ok(()) => {
            set_error(format!(
                "serialized output exceeds {MAX_FFI_OUTPUT_BYTES} bytes"
            ));
            ptr::null()
        }
        Err(error) => {
            set_error(error.to_string());
            ptr::null()
        }
    }
}

fn ok_string(value: impl Into<Vec<u8>>) -> *const c_char {
    return_string(value)
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
    Workspace::open(&rt, Path::new(&root)).map_err(|e| e.to_string())
}

fn bounded_cstr(ptr: *const c_char) -> Result<String, String> {
    if ptr.is_null() {
        return Err("required string argument is null".to_string());
    }
    for length in 0..=MAX_FFI_INPUT_BYTES {
        let byte = unsafe { ptr.add(length).read() };
        if byte == 0 {
            let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), length) };
            return std::str::from_utf8(bytes)
                .map(str::to_string)
                .map_err(|error| error.to_string());
        }
    }
    Err(format!(
        "string argument exceeds {MAX_FFI_INPUT_BYTES} bytes"
    ))
}

fn cstr_opt(ptr: *const c_char) -> Result<Option<String>, String> {
    if ptr.is_null() {
        return Ok(None);
    }
    bounded_cstr(ptr).map(Some)
}

/// Read a required bounded UTF-8 C string argument. Exported callers declare
/// the pointer validity requirement in their `# Safety` contract.
fn cstr_req(ptr: *const c_char) -> Result<String, String> {
    bounded_cstr(ptr)
}

/// Initialize the shared Tokio runtime. Call once before any other `ffs_*` function.
/// Returns `0` on success, `-1` on error (see `ffs_last_error`).
#[no_mangle]
pub extern "C" fn ffs_runtime_init() -> i32 {
    catch_i32(|| {
        let mut guard = match RUNTIME.lock() {
            Ok(guard) => guard,
            Err(error) => {
                set_error(error.to_string());
                return -1;
            }
        };
        if guard.is_some() {
            clear_error();
            return 0;
        }
        match Runtime::new() {
            Ok(runtime) => {
                *guard = Some(runtime);
                clear_error();
                0
            }
            Err(error) => {
                set_error(error.to_string());
                -1
            }
        }
    })
}

/// Free a string previously returned by any `ffs_*` function (including `ffs_last_error`).
/// The allocation is tracked independently of its contents, so callers may
/// treat returned strings as immutable and deallocation never scans them.
///
/// # Safety
/// `s` must be NULL or an outstanding pointer returned by this library. Each
/// non-NULL pointer must be freed at most once.
#[no_mangle]
pub unsafe extern "C" fn ffs_string_free(s: *const c_char) {
    if s.is_null() {
        return;
    }
    let allocation = RETURNED_STRINGS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&(s as usize));
    drop(allocation);
}

/// Last error on **this thread** from the most recent failing `ffs_*` call.
/// Caller must free with `ffs_string_free`. Never NULL (empty string if no error).
#[no_mangle]
pub extern "C" fn ffs_last_error() -> *const c_char {
    catch_ptr(|| {
        let msg = LAST_ERROR.with(|e| e.borrow().clone()).unwrap_or_default();
        return_string(msg.into_bytes())
    })
}

/// List agent workspace names. JSON: `AgentListOfflineResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_agent_list(root: *const c_char) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        match workspace(root) {
            Ok(ws) => match ws.list() {
                Ok(names) => ok_json(&AgentListOfflineResult { agents: names }),
                Err(e) => {
                    set_error(e.to_string());
                    ptr::null()
                }
            },
            Err(e) => {
                set_error(e.to_string());
                ptr::null()
            }
        }
    })
}

/// Spawn an isolated agent workspace. JSON: `SpawnResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_agent_spawn(
    root: *const c_char,
    name: *const c_char,
    no_sync: i32,
    replace: i32,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let name = match cstr_req(name) {
            Ok(s) => s,
            Err(e) => {
                set_error(e);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.spawn(
                &name,
                SpawnOptions {
                    no_sync: no_sync != 0,
                    replace: replace != 0,
                },
            ) {
                Ok(r) => ok_json(&r),
                Err(e) => {
                    set_error(e.to_string());
                    ptr::null()
                }
            },
            Err(e) => {
                set_error(e.to_string());
                ptr::null()
            }
        }
    })
}

fn exact_path_bytes(path: &std::path::Path) -> Result<&[u8], String> {
    path.to_str()
        .map(str::as_bytes)
        .ok_or_else(|| "agent path cannot be represented exactly as UTF-8".to_string())
}

/// Return the absolute worktree path for an existing agent. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_agent_path(root: *const c_char, name: *const c_char) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let name = match cstr_req(name) {
            Ok(name) => name,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root).and_then(|ws| ws.agent_path(&name).map_err(|error| error.to_string()))
        {
            Ok(path) => match exact_path_bytes(&path) {
                Ok(bytes) => ok_string(bytes),
                Err(error) => {
                    set_error(error);
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Preview one agent's changes. JSON: `AgentCheckResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_agent_status(
    root: *const c_char,
    name: *const c_char,
) -> *const c_char {
    catch_ptr(|| agent_by_name(root, name, |ws, name| ws.status(name)))
}

/// Pull cloud changes into the agent. JSON: `AgentRefreshResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_agent_refresh(
    root: *const c_char,
    name: *const c_char,
) -> *const c_char {
    catch_ptr(|| agent_by_name(root, name, |ws, name| ws.refresh(name)))
}

/// Integrate agent work into the main workspace. JSON: `AgentLandResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_agent_land(
    root: *const c_char,
    name: *const c_char,
    clean: i32,
    propose: i32,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let name = match cstr_req(name) {
            Ok(s) => s,
            Err(e) => {
                set_error(e);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.land(
                &name,
                LandOptions {
                    clean: clean != 0,
                    propose: propose != 0,
                },
            ) {
                Ok(r) => ok_json(&r),
                Err(e) => {
                    set_error(e.to_string());
                    ptr::null()
                }
            },
            Err(e) => {
                set_error(e.to_string());
                ptr::null()
            }
        }
    })
}

/// Remove an agent workspace. JSON: `AgentCleanResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_agent_clean(
    root: *const c_char,
    name: *const c_char,
) -> *const c_char {
    catch_ptr(|| agent_by_name(root, name, |ws, name| ws.clean(name)))
}

/// List reachable workspace history. JSON: `LogResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_log(root: *const c_char, limit: u32) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        match workspace(root) {
            Ok(ws) => match ws.log(usize::try_from(limit).unwrap_or(usize::MAX)) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Restore a reachable snapshot as a new snapshot. JSON: `UndoResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_undo(
    root: *const c_char,
    snapshot_id: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let snapshot_id = match cstr_req(snapshot_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.undo(&snapshot_id) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Send an encrypted agent signal. JSON in: `AgentMessageInput`; JSON out:
/// `AgentSendResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_agent_send(
    root: *const c_char,
    input_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let input_json = match cstr_req(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let input: feanorfs_common::AgentMessageInput = match serde_json::from_str(&input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid agent_send input: {error}"));
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.send_message(input) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Read agent signals. JSON in: `AgentInboxQuery`; JSON out: `AgentInboxResult`.
/// NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_agent_inbox(
    root: *const c_char,
    query_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let query_json = match cstr_req(query_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let query: feanorfs_common::AgentInboxQuery = match serde_json::from_str(&query_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid agent_inbox input: {error}"));
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.inbox(query) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Resolve a pending conflict. Returns `0` on success, `-1` on error.
/// `keep`: 0=local, 1=cloud, 2=both, 3=file (requires non-null `file_path`).
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_conflicts_keep(
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
            Ok(ws) => match ws.resolve(&path, keep, file_ref) {
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
) -> *const c_char {
    clear_error();
    let name = match cstr_req(name) {
        Ok(s) => s,
        Err(e) => {
            set_error(e.to_string());
            return ptr::null();
        }
    };
    match workspace(root) {
        Ok(ws) => match f(&ws, &name) {
            Ok(r) => ok_json(&r),
            Err(e) => {
                set_error(e.to_string());
                ptr::null()
            }
        },
        Err(e) => {
            set_error(e.to_string());
            ptr::null()
        }
    }
}

fn catch_ptr(f: impl FnOnce() -> *const c_char + panic::UnwindSafe) -> *const c_char {
    match panic::catch_unwind(f) {
        Ok(ptr) => ptr,
        Err(_) => {
            set_error("internal panic");
            ptr::null()
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

/// Decodes bounded base64 candidate bytes. Accepts either a JSON string
/// document (`"<base64>"`) or a bare base64 payload; anything over the
/// engine's 64 MiB plaintext bound fails closed.
fn decode_base64_json(input: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let encoded = match serde_json::from_str::<serde_json::Value>(input) {
        Ok(serde_json::Value::String(encoded)) => encoded,
        _ => input.to_string(),
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.as_bytes())
        .map_err(|error| format!("invalid candidate base64: {error}"))?;
    if bytes.len() as u64 > feanorfs_common::RESOLUTION_MAX_CANDIDATE_BYTES {
        return Err("candidate base64 exceeds the 64 MiB bound".to_string());
    }
    Ok(bytes)
}

/// Parses the optional `ffres1` flags JSON (`null`/absent for defaults, or
/// `{"superseded": bool}`); unknown keys and wrong types are typed errors.
fn parse_flags_json(ptr: *const c_char, op: &str) -> Result<bool, String> {
    let Some(json) = cstr_opt(ptr)? else {
        return Ok(false);
    };
    let value: serde_json::Value =
        serde_json::from_str(&json).map_err(|error| format!("invalid {op} flags: {error}"))?;
    match value {
        serde_json::Value::Null => Ok(false),
        serde_json::Value::Object(map) => {
            let mut superseded = false;
            for (key, value) in map {
                match key.as_str() {
                    "superseded" => {
                        superseded = value
                            .as_bool()
                            .ok_or_else(|| format!("{op} flags `superseded` must be a boolean"))?;
                    }
                    _ => return Err(format!("{op} flags carry an unknown key `{key}`")),
                }
            }
            Ok(superseded)
        }
        _ => Err(format!("{op} flags must be null or an object")),
    }
}

/// Maps one materialized-leg list to the documented `[{"role", "path"}]`
/// JSON shape (the engine returns `(role, absolute path)` tuples).
fn resolution_legs_json(
    legs: Vec<(feanorfs_common::ArtifactRoleName, PathBuf)>,
) -> Result<Vec<serde_json::Value>, String> {
    legs.into_iter()
        .map(|(role, path)| {
            serde_json::to_value(&path)
                .map(|path| serde_json::json!({ "role": role.as_str(), "path": path }))
                .map_err(|error| error.to_string())
        })
        .collect()
}

/// Assign one batch to a randomly ranked integrator. JSON in:
/// `IntegratorAssignInput`; JSON out: `IntegratorAssignResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_integrator_assign(
    root: *const c_char,
    input_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let input_json = match cstr_req(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let input: feanorfs_common::IntegratorAssignInput = match serde_json::from_str(&input_json)
        {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid integrator_assign input: {error}"));
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.integrator_assign(input) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Read the active integrator assignment (or one by id). JSON out:
/// `IntegratorStatusResult`. NULL on error; pass NULL `assignment_id` for the
/// active assignment.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_integrator_status(
    root: *const c_char,
    assignment_id: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let assignment_id = match cstr_opt(assignment_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.integrator_status(assignment_id.as_deref()) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Explicitly revoke the active integrator assignment. JSON out:
/// `IntegratorStatusResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_integrator_revoke(
    root: *const c_char,
    assignment_id: *const c_char,
    reason: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let assignment_id = match cstr_req(assignment_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let reason = match cstr_req(reason) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.integrator_revoke(&assignment_id, &reason) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Resume dispatcher observation after a restart. JSON in: object with
/// optional `ack_timeout_ms` (u64) and `fallback_on_blocked` (bool); JSON out:
/// `IntegratorObserveResult`. NULL on error; pass NULL `options_json` for
/// conservative defaults.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_integrator_resume(
    root: *const c_char,
    options_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let (ack_timeout_ms, fallback_on_blocked) = match cstr_opt(options_json) {
            Ok(None) => (None, false),
            Ok(Some(json)) => {
                let input: feanorfs_common::IntegratorObserveInput =
                    match serde_json::from_str(&json) {
                        Ok(value) => value,
                        Err(error) => {
                            set_error(format!("invalid integrator_resume options: {error}"));
                            return ptr::null();
                        }
                    };
                (input.ack_timeout_ms, input.fallback_on_blocked)
            }
            Err(error) => {
                set_error(error);
                return ptr::null();
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
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Materialize the encrypted conflict triple for a snapshot. JSON in: object
/// with required `about_snapshot` and exactly one of non-empty `paths` or
/// `all: true`; JSON out: `ConflictMaterializeResult`.
/// NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_conflict_materialize(
    root: *const c_char,
    input_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let input_json = match cstr_req(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let input: feanorfs_common::ConflictMaterializeInput =
            match serde_json::from_str(&input_json) {
                Ok(value) => value,
                Err(error) => {
                    set_error(format!("invalid conflict_materialize input: {error}"));
                    return ptr::null();
                }
            };
        let (about_snapshot, paths) = match input.validate() {
            Ok(selection) => selection,
            Err(error) => {
                set_error(format!("invalid conflict_materialize input: {error}"));
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.materialize_conflicts(&about_snapshot, &paths) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Propose one encrypted work intent. JSON in: `WorkProposeInput`; JSON out:
/// `WorkSendResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_work_propose(
    root: *const c_char,
    input_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let input_json = match cstr_req(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let input: feanorfs_common::WorkProposeInput = match serde_json::from_str(&input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid work_propose input: {error}"));
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.work_propose(input) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Send one coordinator decision. JSON in: `WorkDecideInput`; JSON out:
/// `WorkSendResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_work_decide(
    root: *const c_char,
    input_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let input_json = match cstr_req(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let input: feanorfs_common::WorkDecideInput = match serde_json::from_str(&input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid work_decide input: {error}"));
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.work_decide(input) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Amend an accepted intent's scope. JSON in: `WorkAmendInput`; JSON out:
/// `WorkSendResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_work_amend(
    root: *const c_char,
    input_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let input_json = match cstr_req(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let input: feanorfs_common::WorkAmendInput = match serde_json::from_str(&input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid work_amend input: {error}"));
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.work_amend(input) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Send an explicit yield. JSON in: `WorkYieldInput`; JSON out:
/// `WorkSendResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_work_yield(
    root: *const c_char,
    input_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let input_json = match cstr_req(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let input: feanorfs_common::WorkYieldInput = match serde_json::from_str(&input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid work_yield input: {error}"));
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.work_yield(input) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Send a settled profile with verification evidence. JSON in:
/// `WorkSettleInput`; JSON out: `WorkSendResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_work_settle(
    root: *const c_char,
    input_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let input_json = match cstr_req(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let input: feanorfs_common::WorkSettleInput = match serde_json::from_str(&input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid work_settle input: {error}"));
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.work_settle(input) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Send a terminal completion. JSON in: `WorkCompleteInput`; JSON out:
/// `WorkSendResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_work_complete(
    root: *const c_char,
    input_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let input_json = match cstr_req(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let input: feanorfs_common::WorkCompleteInput = match serde_json::from_str(&input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid work_complete input: {error}"));
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.work_complete(input) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Send a terminal blocker. JSON in: `WorkBlockInput`; JSON out:
/// `WorkSendResult`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_work_block(
    root: *const c_char,
    input_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let input_json = match cstr_req(input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let input: feanorfs_common::WorkBlockInput = match serde_json::from_str(&input_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(format!("invalid work_block input: {error}"));
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.work_block(input) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Observe signals through the `ffwork1` reducer and report the bounded
/// projection. JSON in: `WorkStatusInput`; JSON out: `WorkStatusResult`.
/// NULL on error; pass NULL `input_json` for defaults.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_work_status(
    root: *const c_char,
    input_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let input = match cstr_opt(input_json) {
            Ok(None) => feanorfs_common::WorkStatusInput::default(),
            Ok(Some(json)) => match serde_json::from_str(&json) {
                Ok(value) => value,
                Err(error) => {
                    set_error(format!("invalid work_status input: {error}"));
                    return ptr::null();
                }
            },
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.work_status(input) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Prepare one automatic resolution job for the exact current conflict.
/// JSON in: `PreventionReason` (`{"type":"exhausted"|"violated","detail":…}`);
/// JSON out: `ResolutionJob`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_resolution_prepare(
    root: *const c_char,
    path: *const c_char,
    prevention_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let path = match cstr_req(path) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let prevention = match cstr_req(prevention_json) {
            Ok(json) => match serde_json::from_str(&json) {
                Ok(value) => value,
                Err(error) => {
                    set_error(format!("invalid resolution_prepare prevention: {error}"));
                    return ptr::null();
                }
            },
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.resolution_prepare(&path, prevention) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Read the bounded resolution status projection (ids/state/counts only).
/// JSON out: `ResolutionStatusProjection`. Pass NULL `job_id` for the whole
/// store. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_resolution_status(
    root: *const c_char,
    job_id: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let job_id = match cstr_opt(job_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.resolution_status(job_id.as_deref()) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Submit one resolution result. JSON in: `ResolutionResult`; JSON out:
/// `ResolutionResult`. Submission NEVER applies: it validates and records the
/// result without mutating the worktree, registry, artifacts, or head. NULL
/// on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_resolution_submit(
    root: *const c_char,
    job_id: *const c_char,
    result_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let job_id = match cstr_req(job_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let result = match cstr_req(result_json) {
            Ok(json) => match serde_json::from_str(&json) {
                Ok(value) => value,
                Err(error) => {
                    set_error(format!("invalid resolution_submit result: {error}"));
                    return ptr::null();
                }
            },
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.resolution_submit(&job_id, result) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Apply one submitted resolution result with guarded publication. JSON out:
/// `ResolutionApplyOutcome`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_resolution_apply(
    root: *const c_char,
    job_id: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let job_id = match cstr_req(job_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.resolution_apply(&job_id) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Materialize the authenticated base/ours/theirs legs of one resolution job
/// into the engine-owned job directory. JSON out: array of
/// `{"role": "original"|"local"|"cloud", "path": "<absolute-path>"}`. NULL
/// on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_resolution_materialize(
    root: *const c_char,
    job_id: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let job_id = match cstr_req(job_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.resolution_materialize_legs(&job_id) {
                Ok(legs) => match resolution_legs_json(legs) {
                    Ok(mapped) => ok_json(&mapped),
                    Err(error) => {
                        set_error(error);
                        ptr::null()
                    }
                },
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Write the immutable engine-owned candidate file for one job from bounded
/// base64 bytes (create-new, no-follow, fsync'd). JSON in: base64 string
/// (or a JSON string document); JSON out: `CandidateDescriptor`. NULL on
/// error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_resolution_put(
    root: *const c_char,
    job_id: *const c_char,
    base64_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let job_id = match cstr_req(job_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let base64 = match cstr_req(base64_json) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let bytes = match decode_base64_json(&base64) {
            Ok(bytes) => bytes,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.resolution_put_candidate(&job_id, &bytes) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Record one typed human answer bound to one exact escalation. JSON in:
/// `HumanResolutionAnswer`; JSON out: the recorded answer. The local engine
/// validates the full binding (job/assignment/attempt/fingerprint/question
/// generation) and, for `submit_candidate`, runs the inline verification.
/// NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_resolution_answer(
    root: *const c_char,
    answer_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let answer = match cstr_req(answer_json) {
            Ok(json) => match serde_json::from_str::<feanorfs_common::HumanResolutionAnswer>(&json)
            {
                Ok(value) => value,
                Err(error) => {
                    set_error(format!("invalid resolution_answer input: {error}"));
                    return ptr::null();
                }
            },
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.resolution_answer(answer) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Record the terminal `Deferred` state for one assignment without any
/// publication. JSON out: `null`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_resolution_defer(
    root: *const c_char,
    job_id: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let job_id = match cstr_req(job_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.resolution_defer(&job_id) {
                Ok(()) => ok_json(&serde_json::Value::Null),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Observe the encrypted signal stream through the `ffres1` reducer and
/// report the bounded metadata-only projection. JSON in: optional `rebuild`
/// boolean (or `{"rebuild": bool}`); JSON out: `ResolutionProtocolStatus`.
/// NULL on error; pass NULL `rebuild_json` for defaults.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_resolution_protocol_status(
    root: *const c_char,
    rebuild_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let rebuild = match cstr_opt(rebuild_json) {
            Ok(None) => false,
            Ok(Some(json)) => match serde_json::from_str::<serde_json::Value>(&json) {
                Ok(serde_json::Value::Null) => false,
                Ok(serde_json::Value::Bool(value)) => value,
                Ok(serde_json::Value::Object(map)) => map
                    .get("rebuild")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                _ => {
                    set_error(
                        "invalid resolution_protocol_status rebuild_json: expected a boolean or \
                         {\"rebuild\": bool}"
                            .to_string(),
                    );
                    return ptr::null();
                }
            },
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.resolution_protocol_status(rebuild) {
                Ok(result) => ok_json(&result),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Publish the `ffres1` assignment profile (with the complete immutable job)
/// for one locally prepared job. JSON in: optional flags (pass NULL or `{}`);
/// JSON out: `{"message_id": "..."}`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_resolution_assign(
    root: *const c_char,
    job_id: *const c_char,
    flags_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let job_id = match cstr_req(job_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let _flags = match parse_flags_json(flags_json, "resolution_assign") {
            Ok(flags) => flags,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.resolution_assign(&job_id) {
                Ok(message_id) => ok_json(&serde_json::json!({ "message_id": message_id })),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Publish the `ffres1` result profile for one locally submitted job.
/// JSON in: optional flags (pass NULL or `{}`); JSON out:
/// `{"message_id": "..."}`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_resolution_reply(
    root: *const c_char,
    job_id: *const c_char,
    flags_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let job_id = match cstr_req(job_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let _flags = match parse_flags_json(flags_json, "resolution_reply") {
            Ok(flags) => flags,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.resolution_reply(&job_id) {
                Ok(message_id) => ok_json(&serde_json::json!({ "message_id": message_id })),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Publish the `ffres1` revoke/supersede profile for one local job.
/// JSON in: optional flags `{"superseded": bool}` (default false); JSON out:
/// `{"message_id": "..."}`. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_resolution_revoke(
    root: *const c_char,
    job_id: *const c_char,
    flags_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let job_id = match cstr_req(job_id) {
            Ok(value) => value,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        let superseded = match parse_flags_json(flags_json, "resolution_revoke") {
            Ok(flags) => flags,
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.resolution_revoke(&job_id, superseded) {
                Ok(message_id) => ok_json(&serde_json::json!({ "message_id": message_id })),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
            }
        }
    })
}

/// Publish one typed human answer as an `ffres1` profile. JSON in:
/// `HumanResolutionAnswer`; JSON out: `{"message_id": "..."}`. The engine
/// validates the full answer (including `submit_candidate` verification
/// evidence) before sending. NULL on error.
///
/// Returns an owned NUL-terminated UTF-8 string. The caller owns the
/// allocation and must release it with `ffs_string_free` (never with libc
/// `free`). NULL means an error; read `ffs_last_error` for the
/// thread-local diagnostic.
///
/// # Safety
/// Every non-NULL string input must point to valid UTF-8 readable through its terminating NUL for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn ffs_resolution_publish_answer(
    root: *const c_char,
    answer_json: *const c_char,
) -> *const c_char {
    catch_ptr(|| {
        clear_error();
        let answer = match cstr_req(answer_json) {
            Ok(json) => {
                match serde_json::from_str::<feanorfs_common::HumanResolutionAnswer>(&json) {
                    Ok(value) => value,
                    Err(error) => {
                        set_error(format!("invalid resolution_publish_answer input: {error}"));
                        return ptr::null();
                    }
                }
            }
            Err(error) => {
                set_error(error);
                return ptr::null();
            }
        };
        match workspace(root) {
            Ok(ws) => match ws.resolution_publish_answer(&answer) {
                Ok(message_id) => ok_json(&serde_json::json!({ "message_id": message_id })),
                Err(error) => {
                    set_error(error.to_string());
                    ptr::null()
                }
            },
            Err(error) => {
                set_error(error.to_string());
                ptr::null()
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
    use serde_json::json;
    use std::ffi::{CStr, CString};
    use std::fs;

    use super::*;

    fn cstr(ptr: *const c_char) -> String {
        unsafe {
            // Returned strings are valid UTF-8 by construction; a lossy
            // decode here would mask an ABI bug.
            let s = CStr::from_ptr(ptr).to_str().unwrap().to_string();
            ffs_string_free(ptr);
            s
        }
    }

    fn last_err() -> String {
        cstr(ffs_last_error())
    }

    #[cfg(unix)]
    #[test]
    fn agent_path_rejects_non_utf8_without_lossy_aliasing() {
        use std::os::unix::ffi::OsStringExt as _;

        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0xff]));
        assert_eq!(
            exact_path_bytes(&path).unwrap_err(),
            "agent path cannot be represented exactly as UTF-8"
        );
    }

    #[test]
    fn null_required_strings_fail_cleanly_instead_of_crashing() {
        // NULL root / name must never reach CStr::from_ptr (undefined
        // behavior); the ABI returns NULL plus a thread-local error instead.
        assert_eq!(ffs_runtime_init(), 0);
        let result = unsafe { ffs_agent_list(std::ptr::null()) };
        assert!(result.is_null());
        assert!(!last_err().is_empty());
        unsafe { ffs_string_free(result) };
    }

    #[test]
    fn returned_strings_free_by_registered_allocation_even_after_mutation() {
        let pointer = ok_string("secret");
        assert!(!pointer.is_null());
        unsafe {
            pointer.cast_mut().write(b'x' as c_char);
            pointer.cast_mut().add(1).write(0);
            ffs_string_free(pointer);
        }
        assert!(!RETURNED_STRINGS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&(pointer as usize)));
    }

    #[test]
    fn last_error_is_non_null_and_sanitizes_interior_nul() {
        set_error("before\0after");
        let pointer = ffs_last_error();
        assert!(!pointer.is_null());
        assert_eq!(cstr(pointer), r"before\u{0}after");
    }

    #[test]
    fn oversized_c_input_is_rejected_before_workspace_work() {
        let mut bytes = vec![b'a'; MAX_FFI_INPUT_BYTES + 1];
        bytes.push(0);
        let result = unsafe { ffs_agent_list(bytes.as_ptr().cast::<c_char>()) };
        assert!(result.is_null());
        assert!(last_err().contains("exceeds"));
    }

    #[test]
    fn invalid_utf8_c_input_is_rejected_cleanly() {
        // Invalid UTF-8 must fail in `bounded_cstr` before any workspace work,
        // in every required-string slot (root and name).
        assert_eq!(ffs_runtime_init(), 0);
        let bad_root = c"bad\xff\xfe".as_ptr().cast::<c_char>();
        let result = unsafe { ffs_agent_list(bad_root) };
        assert!(result.is_null());
        let error = last_err();
        assert!(
            error.to_ascii_lowercase().contains("utf"),
            "expected a UTF-8 diagnostic, got: {error}"
        );
        let name = CString::new("x").unwrap();
        let result = unsafe { ffs_agent_spawn(bad_root, name.as_ptr(), 0, 0) };
        assert!(result.is_null());
        let bad_name = c"na\xf0\x28\x8c\x28".as_ptr().cast::<c_char>();
        let root = CString::new("/tmp/nope").unwrap();
        let result = unsafe { ffs_agent_spawn(root.as_ptr(), bad_name, 0, 0) };
        assert!(result.is_null());
        assert!(last_err().to_ascii_lowercase().contains("utf"));
    }

    #[test]
    fn panic_inside_export_is_contained() {
        // A panic inside an exported entry point must be caught, translated to
        // NULL/-1, and surfaced as a thread-local "internal panic" error —
        // never propagated across the C ABI.
        let pointer = catch_ptr(|| -> *const c_char { panic!("boom") });
        assert!(pointer.is_null());
        assert_eq!(last_err(), "internal panic");

        let code = catch_i32(|| -> i32 { panic!("boom") });
        assert_eq!(code, -1);
        assert_eq!(last_err(), "internal panic");

        // Non-panicking closures still work after a contained panic.
        let ok = catch_ptr(|| ok_string("still works"));
        assert!(!ok.is_null());
        unsafe { ffs_string_free(ok) };
    }

    #[test]
    fn concurrent_last_error_is_thread_local() {
        // `ffs_last_error` must be per-thread: worker failures never clobber
        // the calling thread's diagnostic and vice versa.
        set_error("main-thread error");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                std::thread::spawn(move || {
                    set_error(format!("worker-{i} error"));
                    let seen = last_err();
                    assert!(
                        seen.contains(&format!("worker-{i}")),
                        "worker-{i} saw a foreign error: {seen}"
                    );
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(last_err(), "main-thread error");
    }

    #[test]
    fn free_null_pointer_is_a_noop() {
        // NULL is always an acceptable `ffs_string_free` argument; it must not
        // panic, dereference, or consume an outstanding allocation.
        let pointer = ok_string("keep");
        assert!(!pointer.is_null());
        unsafe { ffs_string_free(ptr::null()) };
        assert!(RETURNED_STRINGS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&(pointer as usize)));
        unsafe { ffs_string_free(pointer) };
    }

    #[test]
    fn interior_nul_output_fails_closed() {
        // Returned C strings must never expose an interior NUL. serde_json
        // escapes control characters, so a literal NUL can only reach the ABI
        // allocator through a raw payload; it must fail closed with NULL plus
        // a thread-local diagnostic instead of returning a truncated string.
        let pointer = ok_string(vec![b'a', 0, b'b']);
        assert!(pointer.is_null());
        assert!(!last_err().is_empty());
    }

    #[test]
    fn malformed_conflict_subset_does_not_broaden_to_all_paths() {
        let malformed = CString::new(r#"{"about_snapshot":"head","paths":["one",42]}"#).unwrap();
        let result = unsafe { ffs_conflict_materialize(ptr::null(), malformed.as_ptr()) };
        assert!(result.is_null());
        assert!(last_err().contains("invalid conflict_materialize input"));
    }

    #[test]
    fn resolution_null_and_malformed_inputs_fail_cleanly() {
        assert_eq!(ffs_runtime_init(), 0);
        // NULL required args never reach CStr::from_ptr.
        let result = unsafe { ffs_resolution_prepare(ptr::null(), ptr::null(), ptr::null()) };
        assert!(result.is_null());
        assert!(!last_err().is_empty());
        // Malformed prevention JSON is rejected before workspace work.
        let malformed = CString::new(r#"{"type":"exhausted"}"#).unwrap();
        let root = CString::new("/tmp/nope").unwrap();
        let path = CString::new("src/main.rs").unwrap();
        let result =
            unsafe { ffs_resolution_prepare(root.as_ptr(), path.as_ptr(), malformed.as_ptr()) };
        assert!(result.is_null());
        assert!(last_err().contains("invalid resolution_prepare prevention"));
        // Malformed result JSON for submit is rejected the same way.
        let result = unsafe {
            ffs_resolution_submit(
                root.as_ptr(),
                CString::new("j").unwrap().as_ptr(),
                CString::new("{}").unwrap().as_ptr(),
            )
        };
        assert!(result.is_null());
        assert!(last_err().contains("invalid resolution_submit result"));
    }

    #[test]
    fn resolution_status_roundtrips_frozen_fixture_json() {
        let (_tmp, ws) = setup_ws();
        prepare_format_v3_workspace(&ws);
        assert_eq!(ffs_runtime_init(), 0);
        let root = CString::new(ws.to_str().unwrap()).unwrap();

        // Empty store: bounded projection with the frozen schema version.
        let status = unsafe { ffs_resolution_status(root.as_ptr(), ptr::null()) };
        assert!(
            !status.is_null(),
            "resolution status failed: {}",
            last_err()
        );
        let json = cstr(status);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["jobs"], json!([]));

        // Unknown job id filters to an empty projection (read-only).
        let unknown = CString::new("0123456789abcdef0123456789abcdef").unwrap();
        let status = unsafe { ffs_resolution_status(root.as_ptr(), unknown.as_ptr()) };
        assert!(!status.is_null(), "status failed: {}", last_err());
        let value: serde_json::Value = serde_json::from_str(&cstr(status)).unwrap();
        assert_eq!(value["jobs"], json!([]));

        // Apply with an unknown job fails closed with a bounded error.
        let apply = unsafe { ffs_resolution_apply(root.as_ptr(), unknown.as_ptr()) };
        assert!(apply.is_null());
        assert!(!last_err().is_empty());
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
                mesh: None,
            },
        )
        .unwrap();
        fs::write(ws.join("seed.txt"), b"seed").unwrap();
        (tmp, ws)
    }

    #[test]
    fn agent_path_rejects_absolute_and_parent_names() {
        let (_temp, workspace) = setup_ws();
        assert_eq!(ffs_runtime_init(), 0);
        let root = CString::new(workspace.to_str().unwrap()).unwrap();
        for name in ["../outside", "/tmp/outside", "nested/name"] {
            let name = CString::new(name).unwrap();
            let result = unsafe { ffs_agent_path(root.as_ptr(), name.as_ptr()) };
            assert!(result.is_null());
            assert!(last_err().to_ascii_lowercase().contains("agent name"));
        }
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
    fn ffi_calls_succeed_from_current_and_multithread_tokio_contexts() {
        let (_temp, workspace) = setup_ws();
        let root = CString::new(workspace.to_str().unwrap()).unwrap();
        assert_eq!(ffs_runtime_init(), 0);
        for multithread in [false, true] {
            let mut builder = if multithread {
                tokio::runtime::Builder::new_multi_thread()
            } else {
                tokio::runtime::Builder::new_current_thread()
            };
            let outer = builder.enable_all().build().unwrap();
            outer.block_on(async {
                assert_eq!(ffs_runtime_init(), 0);
                let result = unsafe { ffs_agent_list(root.as_ptr()) };
                assert!(!result.is_null(), "agent list failed: {}", last_err());
                let _: feanorfs_common::AgentListOfflineResult =
                    serde_json::from_str(&cstr(result)).unwrap();
            });
        }
    }

    #[test]
    fn spawn_land_local_hub() {
        let (_tmp, ws) = setup_ws();
        assert_eq!(ffs_runtime_init(), 0);

        let root = CString::new(ws.to_str().unwrap()).unwrap();
        let name = CString::new("ffi1").unwrap();

        let spawn_json = unsafe { ffs_agent_spawn(root.as_ptr(), name.as_ptr(), 0, 0) };
        assert!(!spawn_json.is_null(), "spawn failed: {}", last_err());
        assert!(cstr(spawn_json).contains("files_copied"));

        let agent_path = unsafe { ffs_agent_path(root.as_ptr(), name.as_ptr()) };
        assert!(!agent_path.is_null(), "agent path failed: {}", last_err());
        let agent_dir = PathBuf::from(cstr(agent_path));
        assert!(agent_dir.is_dir());
        assert!(!agent_dir.starts_with(&ws));
        assert!(!ws.join(".feanorfs").exists());
        fs::write(agent_dir.join("note.txt"), b"ffi edit").unwrap();

        let land_json = unsafe { ffs_agent_land(root.as_ptr(), name.as_ptr(), 0, 0) };
        assert!(!land_json.is_null(), "land failed: {}", last_err());
        let _ = cstr(land_json);

        let log_json = unsafe { ffs_log(root.as_ptr(), 10) };
        assert!(!log_json.is_null(), "log failed: {}", last_err());
        let log: feanorfs_common::LogResult = serde_json::from_str(&cstr(log_json)).unwrap();
        let target = log.entries[0].parents.last().unwrap();
        let target = CString::new(target.as_str()).unwrap();
        let undo_json = unsafe { ffs_undo(root.as_ptr(), target.as_ptr()) };
        assert!(!undo_json.is_null(), "undo failed: {}", last_err());
        let undo: feanorfs_common::UndoResult = serde_json::from_str(&cstr(undo_json)).unwrap();
        assert_eq!(undo.restored_snapshot_id, target.to_str().unwrap());

        let clean_json = unsafe { ffs_agent_clean(root.as_ptr(), name.as_ptr()) };
        assert!(!clean_json.is_null());
        let _ = cstr(clean_json);
    }

    #[test]
    fn spawn_twice_without_replace_fails() {
        let (_tmp, ws) = setup_ws();
        assert_eq!(ffs_runtime_init(), 0);

        let root = CString::new(ws.to_str().unwrap()).unwrap();
        let name = CString::new("dup").unwrap();

        assert!(!unsafe { ffs_agent_spawn(root.as_ptr(), name.as_ptr(), 0, 0) }.is_null());
        let second = unsafe { ffs_agent_spawn(root.as_ptr(), name.as_ptr(), 0, 0) };
        assert!(second.is_null());
        assert!(!last_err().is_empty());
    }

    #[test]
    fn conflicts_keep_invalid_keep() {
        let (_tmp, ws) = setup_ws();
        assert_eq!(ffs_runtime_init(), 0);

        let root = CString::new(ws.to_str().unwrap()).unwrap();
        let path = CString::new("missing.txt").unwrap();
        assert_eq!(
            unsafe { ffs_conflicts_keep(root.as_ptr(), path.as_ptr(), 99, ptr::null()) },
            -1
        );
        assert!(!last_err().is_empty());
    }

    #[test]
    fn ops_before_init_fail() {
        let root = CString::new("/tmp/nope").unwrap();
        let name = CString::new("x").unwrap();
        assert!(unsafe { ffs_agent_list(root.as_ptr()) }.is_null());
        assert!(!last_err().is_empty());
        clear_error();
        assert!(unsafe { ffs_agent_spawn(root.as_ptr(), name.as_ptr(), 0, 0) }.is_null());
        assert!(!last_err().is_empty());
    }

    #[test]
    fn agent_send_inbox_roundtrip_local_hub() {
        let (_tmp, ws) = setup_ws();
        prepare_format_v3_workspace(&ws);
        assert_eq!(ffs_runtime_init(), 0);

        let root = CString::new(ws.to_str().unwrap()).unwrap();
        let name = CString::new("ffi2").unwrap();
        assert!(!unsafe { ffs_agent_spawn(root.as_ptr(), name.as_ptr(), 0, 0) }.is_null());
        let land_json = unsafe { ffs_agent_land(root.as_ptr(), name.as_ptr(), 0, 0) };
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
        let send_json = unsafe { ffs_agent_send(root.as_ptr(), input.as_ptr()) };
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
        let inbox_json = unsafe { ffs_agent_inbox(root.as_ptr(), query.as_ptr()) };
        assert!(!inbox_json.is_null(), "inbox failed: {}", last_err());
        let inbox: feanorfs_common::AgentInboxResult =
            serde_json::from_str(&cstr(inbox_json)).unwrap();
        assert_eq!(inbox.messages.len(), 1);
        assert_eq!(inbox.messages[0].message_id, send.message_id);
        assert_eq!(inbox.messages[0].from, "ffi-sender");
        assert_eq!(inbox.messages[0].body, "Run iOS simulator tests");
        assert!(!inbox.cursor_reset);

        let bad = CString::new("not json").unwrap();
        assert!(unsafe { ffs_agent_send(root.as_ptr(), bad.as_ptr()) }.is_null());
        assert!(!last_err().is_empty());
    }

    #[test]
    fn integrator_assign_status_revoke_local_hub() {
        let (_tmp, ws) = setup_ws();
        prepare_format_v3_workspace(&ws);
        assert_eq!(ffs_runtime_init(), 0);

        let root = CString::new(ws.to_str().unwrap()).unwrap();
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
        let assign_json = unsafe { ffs_integrator_assign(root.as_ptr(), input.as_ptr()) };
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
        let status_json = unsafe { ffs_integrator_status(root.as_ptr(), id.as_ptr()) };
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
        assert!(unsafe { ffs_integrator_assign(root.as_ptr(), again.as_ptr()) }.is_null());

        let reason = CString::new("integrator went quiet").unwrap();
        let revoke_json =
            unsafe { ffs_integrator_revoke(root.as_ptr(), id.as_ptr(), reason.as_ptr()) };
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
        let resume_json = unsafe { ffs_integrator_resume(root.as_ptr(), ptr::null()) };
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
