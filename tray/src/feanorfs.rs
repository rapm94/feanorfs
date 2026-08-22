//! Spawn `feanorfs` subprocesses — the tray never duplicates sync logic.

use crate::ui::dialog_text;
use feanorfs_common::tray_contract::{RecentWorkspacesResult, SetupResult, TrayStatusResult};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::borrow::Cow;
use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const PAIR_EXPIRES_SECONDS: &str = "300";

/// Default bound for captured stdout of ordinary JSON commands.
const DEFAULT_STDOUT_LIMIT: usize = 256 * 1024;
/// Bound for captured stderr of every subprocess.
const DEFAULT_STDERR_LIMIT: usize = 64 * 1024;
/// Bound for the typed setup-result line (`start`, `tray join`).
const SETUP_RESULT_STDOUT_LIMIT: usize = 64 * 1024;
/// The recent-workspace registry can legitimately list many folders.
const RECENT_STDOUT_LIMIT: usize = 1024 * 1024;
/// Bound for the one-line update-check JSON result.
const UPDATE_STDOUT_LIMIT: usize = 16 * 1024;
/// Bound for the interactive join preview line.
const JOIN_PREVIEW_LIMIT: usize = 65_536;
/// Bound for the pairing ready event line.
const PAIR_LINE_LIMIT: usize = 1024;

/// Bounded child stdout/stderr capture: `bytes` holds the first `limit` bytes
/// written and `truncated` reports that the child wrote more.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoundedBytes {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

impl BoundedBytes {
    fn read(mut reader: impl std::io::Read, limit: usize) -> Self {
        let mut bytes = Vec::with_capacity(limit.min(65_536));
        let mut truncated = false;
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                // Keep draining after the bound so a chatty child never blocks
                // on a full pipe; only the bounded head is retained.
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let remaining = limit.saturating_sub(bytes.len());
                    if read > remaining {
                        truncated = true;
                    }
                    bytes.extend_from_slice(&buffer[..read.min(remaining)]);
                }
            }
        }
        Self { bytes, truncated }
    }

    /// Lossy UTF-8 view; callers only use this for human presentation.
    pub fn as_str_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }
}

/// Captured outcome of one bounded subprocess run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedOutput {
    pub status: std::process::ExitStatus,
    pub stdout: BoundedBytes,
    pub stderr: BoundedBytes,
}

impl CapturedOutput {
    /// Decode the bounded stdout as a typed JSON document.
    ///
    /// Fails with a typed error when the output exceeded the bound, was empty
    /// (early exit), or was not valid JSON for the requested type.
    pub fn decode_json<T: DeserializeOwned>(&self) -> Result<T, CapturedError> {
        if self.stdout.truncated {
            return Err(CapturedError::OutputOverLimit { stream: "stdout" });
        }
        if self.stdout.bytes.is_empty() {
            return Err(CapturedError::NoOutput);
        }
        serde_json::from_slice(&self.stdout.bytes)
            .map_err(|error| CapturedError::MalformedJson(format!("{error}")))
    }
}

/// Typed failure of a bounded subprocess run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapturedError {
    /// The program could not be spawned.
    Spawn(String),
    /// Secret stdin was invalid (line breaks/NUL) or could not be written.
    InvalidSecret,
    Stdin(String),
    /// The child was stopped because the cancel channel fired.
    Canceled,
    /// The child exceeded the timeout; its process tree was stopped.
    Timeout {
        stdout: BoundedBytes,
        stderr: BoundedBytes,
    },
    /// The child could not be waited on.
    Wait(String),
    /// The child exited without producing decodable output.
    NoOutput,
    /// stdout exceeded the capture bound.
    OutputOverLimit {
        stream: &'static str,
    },
    /// stdout was not valid JSON for the requested type.
    MalformedJson(String),
}

impl std::fmt::Display for CapturedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapturedError::Spawn(error) => write!(f, "could not start the command: {error}"),
            CapturedError::InvalidSecret => {
                write!(f, "the secret input contains line breaks or NUL characters")
            }
            CapturedError::Stdin(error) => write!(f, "could not write command input: {error}"),
            CapturedError::Canceled => write!(f, "the command was canceled"),
            CapturedError::Timeout { .. } => {
                write!(f, "the command did not finish in time and was stopped")
            }
            CapturedError::Wait(error) => write!(f, "could not wait for the command: {error}"),
            CapturedError::NoOutput => write!(f, "the command exited without producing output"),
            CapturedError::OutputOverLimit { stream } => {
                write!(f, "{stream} output exceeded the capture limit")
            }
            CapturedError::MalformedJson(error) => {
                write!(f, "the command returned unreadable data: {error}")
            }
        }
    }
}

/// Bounded, typed CLI subprocess adapter used by every non-interactive tray
/// command: exact `OsString` args, working directory, bounded stdout/stderr
/// with typed over-limit reporting, timeout, cancellation, typed JSON decode,
/// and optional one-line secret stdin that never enters argv, env, or logs.
#[derive(Clone)]
pub struct CapturedCommand {
    program: OsString,
    args: Vec<OsString>,
    current_dir: Option<PathBuf>,
    stdin: Option<Zeroizing<String>>,
    stdout_limit: usize,
    stderr_limit: usize,
    timeout: Option<Duration>,
}

impl std::fmt::Debug for CapturedCommand {
    /// `Debug` is a potential log surface, so the secret stdin is redacted.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedCommand")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("current_dir", &self.current_dir)
            .field("stdin", &self.stdin.as_ref().map(|_| "[secret redacted]"))
            .field("stdout_limit", &self.stdout_limit)
            .field("stderr_limit", &self.stderr_limit)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl CapturedCommand {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            stdin: None,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
            timeout: None,
        }
    }

    pub fn args<I, A>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = A>,
        A: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    pub fn stdout_limit(mut self, limit: usize) -> Self {
        self.stdout_limit = limit;
        self
    }

    pub fn stderr_limit(mut self, limit: usize) -> Self {
        self.stderr_limit = limit;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Feed exactly one line (`secret` + `\n`) to the child's stdin and close
    /// it. The secret never appears in argv, environment, or any log surface
    /// (`Debug` redacts it).
    pub fn secret_stdin(mut self, secret: Zeroizing<String>) -> Self {
        self.stdin = Some(secret);
        self
    }

    pub fn capture(self) -> Result<CapturedOutput, CapturedError> {
        self.capture_with_cancel(None)
    }

    /// Capture with an explicit cancellation channel. A fired receiver stops
    /// the child's process tree and returns [`CapturedError::Canceled`].
    pub fn capture_with_cancel(
        &self,
        cancel: Option<&Receiver<()>>,
    ) -> Result<CapturedOutput, CapturedError> {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        if let Some(dir) = &self.current_dir {
            command.current_dir(dir);
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        if self.stdin.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        let mut child =
            spawn_child(&mut command).map_err(|error| CapturedError::Spawn(format!("{error}")))?;

        if let Some(secret) = &self.stdin {
            if secret.contains(['\r', '\n', '\0']) {
                stop_child(&mut child);
                return Err(CapturedError::InvalidSecret);
            }
            let write = child
                .stdin
                .take()
                .ok_or_else(|| CapturedError::Stdin("child stdin was not piped".into()))
                .and_then(|mut stdin| {
                    stdin
                        .write_all(secret.as_bytes())
                        .and_then(|()| stdin.write_all(b"\n"))
                        .and_then(|()| stdin.flush())
                        .map_err(|error| CapturedError::Stdin(format!("{error}")))
                });
            if let Err(error) = write {
                stop_child(&mut child);
                return Err(error);
            }
        }
        // The piped stdin handle was taken out of `child` above and dropped at
        // the end of the write closure, so the child sees EOF on its input.
        // `self.stdin` (the zeroized secret) drops when this builder ends.

        let stdout = child.stdout.take().expect("captured stdout is piped");
        let stderr = child.stderr.take().expect("captured stderr is piped");
        let stdout_limit = self.stdout_limit;
        let stderr_limit = self.stderr_limit;
        let stdout_thread = std::thread::spawn(move || BoundedBytes::read(stdout, stdout_limit));
        let stderr_thread = std::thread::spawn(move || BoundedBytes::read(stderr, stderr_limit));

        let deadline = self.timeout.map(|timeout| Instant::now() + timeout);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let stdout = stdout_thread.join().unwrap_or_default();
                    let stderr = stderr_thread.join().unwrap_or_default();
                    return Ok(CapturedOutput {
                        status,
                        stdout,
                        stderr,
                    });
                }
                Ok(None) => {}
                Err(error) => {
                    stop_child(&mut child);
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(CapturedError::Wait(format!("{error}")));
                }
            }
            if let Some(cancel) = cancel {
                if cancel.try_recv().is_ok() {
                    stop_child(&mut child);
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(CapturedError::Canceled);
                }
            }
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    stop_child(&mut child);
                    let stdout = stdout_thread.join().unwrap_or_default();
                    let stderr = stderr_thread.join().unwrap_or_default();
                    return Err(CapturedError::Timeout { stdout, stderr });
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Spawn a child as a new process-group leader on Unix so cancellation and
/// timeout can stop its whole process tree, never just the direct child.
fn spawn_child(command: &mut Command) -> std::io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command.spawn()
}

/// Stop a child and its process tree: SIGTERM to the group, a short grace
/// period, then SIGKILL to the group — even when the direct child already
/// exited — so grandchildren that ignored TERM cannot survive holding pipes.
/// On non-Unix, falls back to `kill()`.
pub fn stop_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HealthReport {
    pub ok: bool,
    pub checks: Vec<HealthCheck>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct HealthCheck {
    pub name: String,
    pub status: HealthStatus,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Ok,
    Info,
    Warning,
    Failure,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
pub struct UpdateCheckResult {
    pub status: UpdateStatus,
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStatus {
    UpToDate,
    UpdateAvailable,
    DevelopmentBuild,
}

/// JSON mirror of the CLI's `update --apply` result. `applied_version` must
/// equal the version the check advertised before install was offered.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct UpdateApplyOutcome {
    pub applied_version: String,
    pub previous_version: String,
    pub replaced_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<String>,
}

pub fn feanorfs_bin() -> String {
    select_feanorfs_bin(
        std::env::var("FEANORFS_BIN").ok(),
        std::env::current_exe().ok(),
        &packaged_cli_candidates(),
    )
    .unwrap_or_else(|| format!("feanorfs{}", std::env::consts::EXE_SUFFIX))
}

fn select_feanorfs_bin(
    configured: Option<String>,
    current_exe: Option<PathBuf>,
    packaged: &[PathBuf],
) -> Option<String> {
    if configured.is_some() {
        return configured;
    }
    if let Some(current) = current_exe {
        let sibling = current.with_file_name(format!("feanorfs{}", std::env::consts::EXE_SUFFIX));
        if sibling.is_file() {
            return Some(sibling.display().to_string());
        }
    }
    packaged
        .iter()
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.display().to_string())
}

fn packaged_cli_candidates() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        vec![PathBuf::from("/usr/local/bin/feanorfs")]
    }
    #[cfg(target_os = "linux")]
    {
        vec![PathBuf::from("/usr/bin/feanorfs")]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Vec::new()
    }
}

pub fn workspace_has_config(path: &Path) -> bool {
    quick_capture(path, &["--json", "config"]).is_ok_and(|output| output.status.success())
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

/// Run a quick local command with the default bounds and a 30s timeout.
fn quick_capture(workspace: &Path, args: &[&str]) -> Result<CapturedOutput, CapturedError> {
    CapturedCommand::new(feanorfs_bin())
        .args(args)
        .cwd(workspace)
        .stdout_limit(DEFAULT_STDOUT_LIMIT)
        .stderr_limit(DEFAULT_STDERR_LIMIT)
        .timeout(Duration::from_secs(30))
        .capture()
}

fn run_checked(workspace: &Path, args: &[&str], timeout: Option<Duration>) -> Result<(), String> {
    let mut command = CapturedCommand::new(feanorfs_bin())
        .args(args)
        .cwd(workspace)
        .stdout_limit(DEFAULT_STDOUT_LIMIT)
        .stderr_limit(DEFAULT_STDERR_LIMIT);
    if let Some(timeout) = timeout {
        command = command.timeout(timeout);
    }
    let out = command
        .capture()
        .map_err(|error| {
            truncate_error(&format!(
                "FeanorFS could not start its sync command. No files were changed. Reinstall FeanorFS and try again. Details: {error}"
            ))
        })?;
    if !out.status.success() {
        let stderr = out.stderr.as_str_lossy().trim().to_string();
        let msg = if stderr.is_empty() {
            format!("feanorfs exited with {}", out.status)
        } else {
            stderr
        };
        return Err(truncate_error(&msg));
    }
    Ok(())
}

/// Trimmed stderr text with a leading `Error:` prefix removed, when any.
fn stderr_detail(stderr: &BoundedBytes) -> Option<String> {
    let text = stderr.as_str_lossy();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(
        trimmed
            .strip_prefix("Error:")
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
            .unwrap_or(trimmed)
            .to_string(),
    )
}

fn truncate_error(msg: &str) -> String {
    const MAX_CHARS: usize = 300;
    if msg.chars().count() <= MAX_CHARS {
        return msg.to_string();
    }
    let limited: String = msg.chars().take(MAX_CHARS).collect();
    let first_line: String = limited.lines().next().unwrap_or(&limited).to_string();
    if first_line.chars().count() < limited.chars().count() {
        format!("{first_line}…")
    } else {
        format!("{limited}…")
    }
}

pub fn tray_status(workspace: &Path) -> Result<TrayStatusResult, String> {
    let out = quick_capture(workspace, &["--json", "tray", "status"]).map_err(|error| {
        truncate_error(&format!(
            "Sync status is unavailable because the FeanorFS command could not start. Your files were not changed. Reinstall FeanorFS and try again. Details: {error}"
        ))
    })?;
    if !out.status.success() {
        return Err(status_failure_message(
            stderr_detail(&out.stderr).as_deref(),
        ));
    }
    out.decode_json::<TrayStatusResult>().map_err(|_| {
        status_failure_message(Some(
            "the installed CLI returned unreadable status data; reinstall FeanorFS",
        ))
    })
}

fn status_failure_message(detail: Option<&str>) -> String {
    let recovery = "Sync status is unavailable. Your files were not changed. Quit and reopen FeanorFS; if this continues, choose Check System Health… from the tray.";
    match detail {
        Some(detail) => truncate_error(&format!("{recovery} Details: {detail}")),
        None => recovery.to_string(),
    }
}

pub fn system_health(workspace: &Path) -> Result<HealthReport, String> {
    let out = CapturedCommand::new(feanorfs_bin())
        .args(health_args())
        .cwd(workspace)
        .stdout_limit(DEFAULT_STDOUT_LIMIT)
        .stderr_limit(DEFAULT_STDERR_LIMIT)
        .timeout(Duration::from_secs(90))
        .capture()
        .map_err(|_| {
            "System health could not be checked because the FeanorFS command is unavailable. Your files were not changed. Reinstall FeanorFS and try again."
                .to_string()
        })?;
    if !out.status.success() {
        return Err(
            "System health could not be checked. Your files were not changed. Reopen FeanorFS and try again."
                .into(),
        );
    }
    out.decode_json::<HealthReport>().map_err(|_| {
        "System health could not be read from the installed FeanorFS command. Your files were not changed. Reinstall FeanorFS and try again."
            .into()
    })
}

fn health_args() -> [&'static str; 2] {
    ["--json", "doctor"]
}

pub fn check_for_updates() -> Result<UpdateCheckResult, String> {
    let out = CapturedCommand::new(feanorfs_bin())
        .args(update_args())
        .cwd(home_dir())
        .stdout_limit(UPDATE_STDOUT_LIMIT)
        .stderr_limit(DEFAULT_STDERR_LIMIT)
        .timeout(Duration::from_secs(90))
        .capture()
        .map_err(|_| {
            "Updates could not be checked because the FeanorFS command is unavailable. The installed app was not changed. Reinstall FeanorFS and try again."
                .to_string()
        })?;
    if !out.status.success() {
        return Err(
            "Updates could not be checked. The installed app was not changed. Check your internet connection and try again."
                .into(),
        );
    }
    let result: UpdateCheckResult = out.decode_json().map_err(|_| {
        "The installed FeanorFS command returned an unreadable update result. The installed app was not changed. Reinstall FeanorFS and try again."
            .to_string()
    })?;
    if !official_release_result(&result) {
        return Err(
            "The update result did not point to the official FeanorFS release page. The installed app was not changed."
                .into(),
        );
    }
    Ok(result)
}

fn update_args() -> [&'static str; 2] {
    ["--json", "update"]
}

/// Installs the advertised update by delegating to the CLI's checksum-verified
/// apply path. `expected_latest` must match the version the last check offered;
/// any other applied version is refused as a confused-deputy outcome.
pub fn install_update(expected_latest: &str) -> Result<UpdateApplyOutcome, String> {
    let out = CapturedCommand::new(feanorfs_bin())
        .args(["--json", "update", "--apply"])
        .cwd(home_dir())
        .stdout_limit(UPDATE_STDOUT_LIMIT)
        .stderr_limit(DEFAULT_STDERR_LIMIT)
        .timeout(Duration::from_secs(600))
        .capture()
        .map_err(|_| {
            "The update could not be installed because the FeanorFS command is unavailable. The installed app was not changed."
                .to_string()
        })?;
    if !out.status.success() {
        let detail = out.stderr.as_str_lossy();
        let detail = detail
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("unknown error");
        return Err(format!(
            "The update could not be installed. The previous FeanorFS remains in place. Detail: {detail}"
        ));
    }
    let outcome: UpdateApplyOutcome = out.decode_json().map_err(|error| {
        format!(
            "The installed FeanorFS command returned an unreadable install result ({error}). Check `feanorfs update --apply` output before retrying."
        )
    })?;
    if outcome.applied_version != expected_latest {
        return Err(format!(
            "The installer reported version {} but {} was verified. Refusing to treat this as the expected update.",
            outcome.applied_version, expected_latest
        ));
    }
    Ok(outcome)
}

fn official_release_result(result: &UpdateCheckResult) -> bool {
    if result.current_version.is_empty()
        || result.current_version.len() > 64
        || result.latest_version.is_empty()
        || result.latest_version.len() > 64
    {
        return false;
    }
    let safe_version = |value: &str| {
        value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".-+".contains(character))
    };
    safe_version(&result.current_version)
        && safe_version(&result.latest_version)
        && result.release_url
            == format!(
                "https://github.com/rapm94/feanorfs/releases/tag/v{}",
                result.latest_version
            )
}

pub fn tray_pause(workspace: &Path, pause: bool) -> Result<(), String> {
    let sub = if pause { "pause" } else { "resume" };
    run_checked(
        workspace,
        &["--json", "tray", sub],
        Some(Duration::from_secs(30)),
    )
}

pub fn tray_recent() -> Option<RecentWorkspacesResult> {
    let out = CapturedCommand::new(feanorfs_bin())
        .args(["--json", "tray", "recent"])
        .cwd(home_dir())
        .stdout_limit(RECENT_STDOUT_LIMIT)
        .stderr_limit(DEFAULT_STDERR_LIMIT)
        .timeout(Duration::from_secs(30))
        .capture()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    out.decode_json::<RecentWorkspacesResult>().ok()
}

pub fn forget_unavailable_workspaces() -> Result<RecentWorkspacesResult, String> {
    let out = CapturedCommand::new(feanorfs_bin())
        .args(["--json", "tray", "forget-unavailable"])
        .cwd(home_dir())
        .stdout_limit(RECENT_STDOUT_LIMIT)
        .stderr_limit(DEFAULT_STDERR_LIMIT)
        .timeout(Duration::from_secs(60))
        .capture()
        .map_err(|error| {
            truncate_error(&format!(
                "The unavailable workspace entries could not be removed. No files or workspace data were changed. Reopen FeanorFS and try again. Details: {error}"
            ))
        })?;
    if !out.status.success() {
        let detail = out.stderr.as_str_lossy().trim().to_string();
        return Err(truncate_error(&format!(
            "The unavailable workspace entries could not be removed. No files or workspace data were changed. Reopen FeanorFS and try again. Details: {detail}"
        )));
    }
    out.decode_json::<RecentWorkspacesResult>().map_err(|_| {
        "The workspace list could not be refreshed. No files or workspace data were changed. Reinstall FeanorFS and try again."
            .into()
    })
}

pub fn tray_activate(path: &Path) -> Result<(), String> {
    let path_str = path
        .to_str()
        .ok_or_else(|| "workspace path is not valid UTF-8".to_string())?;
    run_checked(
        &home_dir(),
        &["tray", "activate", "--", path_str],
        Some(Duration::from_secs(30)),
    )
}

/// Run `feanorfs --json start` and return its typed setup outcome. The CLI
/// prints human progress first and the `SetupResult` as its final stdout
/// line, so the tray extracts that line and never classifies from CLI wording.
pub fn tray_setup(path: &Path) -> SetupResult {
    let out = match CapturedCommand::new(feanorfs_bin())
        .args(setup_json_args(path))
        .cwd(home_dir())
        .stdout_limit(SETUP_RESULT_STDOUT_LIMIT)
        .stderr_limit(DEFAULT_STDERR_LIMIT)
        .capture()
    {
        Ok(out) => out,
        Err(error) => {
            return SetupResult::generic(&format!(
                "FeanorFS could not start its setup command. No files were changed. Reinstall FeanorFS and try again. Details: {error}"
            ));
        }
    };
    last_setup_result_line(&out.stdout).unwrap_or_else(|| {
        SetupResult::generic(&stderr_detail(&out.stderr).unwrap_or_else(|| {
            "the installed CLI returned an unreadable setup result; reinstall FeanorFS".to_string()
        }))
    })
}

fn setup_json_args(path: &Path) -> Vec<OsString> {
    vec![
        "--json".into(),
        "start".into(),
        "--".into(),
        path.as_os_str().to_owned(),
    ]
}

/// The final stdout line of a setup run is the typed result; human progress
/// lines before it never parse as `SetupResult`.
fn last_setup_result_line(stdout: &BoundedBytes) -> Option<SetupResult> {
    stdout
        .as_str_lossy()
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<SetupResult>(line.trim()).ok())
}

/// Outcome of one tray join: whether the user canceled, plus the typed setup
/// result (generic when the CLI never produced a typed tail).
pub struct JoinOutcome {
    pub canceled: bool,
    pub result: SetupResult,
}

pub fn join_workspace(path: &Path, pairing_code: Zeroizing<String>) -> JoinOutcome {
    join_workspace_interactive(path, pairing_code)
}

#[derive(Debug, Deserialize)]
struct JoinPreviewEvent {
    event: String,
    preview: JoinPreview,
}

#[derive(Debug, Deserialize)]
struct JoinPreview {
    local_only: JoinPathGroup,
    remote_only: JoinPathGroup,
    same: JoinPathGroup,
    conflicts: JoinPathGroup,
    large_files: JoinPathGroup,
    ignore_policy_known: bool,
    ignore_policy_differs: bool,
}

#[derive(Debug, Deserialize)]
struct JoinPathGroup {
    count: usize,
    examples: Vec<String>,
}

fn join_workspace_interactive(path: &Path, pairing_code: Zeroizing<String>) -> JoinOutcome {
    let canceled_failure = |canceled: bool, detail: &str| JoinOutcome {
        canceled,
        result: SetupResult::generic(detail),
    };
    if pairing_code.contains(['\r', '\n', '\0']) {
        return canceled_failure(
            false,
            "The pairing capability cannot contain line breaks or NUL characters.",
        );
    }
    let mut command = Command::new(feanorfs_bin());
    command
        .args(tray_join_args(path))
        .current_dir(home_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match spawn_child(&mut command) {
        Ok(child) => child,
        Err(error) => {
            return canceled_failure(
                false,
                &format!("failed to start secure workspace join: {error}"),
            );
        }
    };
    let mut stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            stop_child(&mut child);
            return canceled_failure(false, "failed to open secure pairing input");
        }
    };
    if let Err(error) = stdin
        .write_all(pairing_code.as_bytes())
        .and_then(|()| stdin.write_all(b"\n"))
        .and_then(|()| stdin.flush())
    {
        stop_child(&mut child);
        return canceled_failure(false, &format!("send pairing capability: {error}"));
    }
    drop(pairing_code);

    let stdout = child.stdout.take().expect("join stdout is piped");
    let mut stdout = BufReader::new(stdout);
    let mut event_line = String::new();
    let bytes = match std::io::Read::by_ref(&mut stdout)
        .take(JOIN_PREVIEW_LIMIT as u64 + 1)
        .read_line(&mut event_line)
    {
        Ok(bytes) => bytes,
        Err(error) => {
            stop_child(&mut child);
            return canceled_failure(false, &format!("read secure join preview: {error}"));
        }
    };
    if bytes == 0 || bytes as usize > JOIN_PREVIEW_LIMIT || !event_line.ends_with('\n') {
        stop_child(&mut child);
        return canceled_failure(
            false,
            "FeanorFS ended before the safe join preview was ready.",
        );
    }
    let event: JoinPreviewEvent = match serde_json::from_str(event_line.trim_end()) {
        Ok(event) => event,
        Err(_) => {
            stop_child(&mut child);
            return canceled_failure(false, "FeanorFS returned an invalid safe join preview.");
        }
    };
    if event.event != "join_preview" {
        stop_child(&mut child);
        return canceled_failure(false, "FeanorFS returned an unexpected secure join stage.");
    }

    let needs_confirmation = event.preview.local_only.count > 0
        || event.preview.conflicts.count > 0
        || event.preview.ignore_policy_differs;
    if needs_confirmation {
        let confirmed = rfd::MessageDialog::new()
            .set_title("Join this existing folder?")
            .set_description(dialog_text(join_confirmation_copy(&event.preview)))
            .set_level(rfd::MessageLevel::Warning)
            .set_buttons(rfd::MessageButtons::OkCancel)
            .show();
        if !matches!(confirmed, rfd::MessageDialogResult::Ok) {
            let _ = stdin.write_all(b"CANCEL\n");
            drop(stdin);
            stop_child(&mut child);
            return canceled_failure(
                true,
                "Join canceled. No FeanorFS setup or workspace files were changed.",
            );
        }
    }
    if let Err(error) = stdin.write_all(b"CONFIRM\n").and_then(|()| stdin.flush()) {
        stop_child(&mut child);
        return canceled_failure(false, &format!("confirm secure workspace join: {error}"));
    }
    drop(stdin);

    // Drain stderr concurrently so the child never blocks on a full pipe.
    let stderr = child.stderr.take().expect("join stderr is piped");
    let stderr_thread =
        std::thread::spawn(move || BoundedBytes::read(stderr, DEFAULT_STDERR_LIMIT));

    // The typed setup result is the final stdout line of the join protocol;
    // the preview line and any human progress precede it. Read to EOF so the
    // child can never block, then classify from the typed tail.
    let tail = BoundedBytes::read(
        std::io::Read::by_ref(&mut stdout),
        SETUP_RESULT_STDOUT_LIMIT,
    );
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            let _ = stderr_thread.join();
            return canceled_failure(false, &format!("wait for secure workspace join: {error}"));
        }
    };
    let stderr = stderr_thread.join().unwrap_or_default();
    if let Some(result) = last_setup_result_line(&tail) {
        return JoinOutcome {
            canceled: false,
            result,
        };
    }
    let detail = stderr_detail(&stderr).unwrap_or_else(|| {
        if status.success() {
            "secure workspace join returned no typed setup result".to_string()
        } else {
            "secure workspace join failed".to_string()
        }
    });
    canceled_failure(false, &truncate_error(&detail))
}

fn join_confirmation_copy(preview: &JoinPreview) -> String {
    let mut message = format!(
        "FeanorFS compared this folder with the encrypted mirror before changing anything.\n\nLocal only — upload: {}\nMirror only — download: {}\nAlready identical: {}\nDifferent at the same path — keep for review: {}",
        preview.local_only.count,
        preview.remote_only.count,
        preview.same.count,
        preview.conflicts.count,
    );
    if preview.ignore_policy_differs {
        message.push_str(
            "\n\nThe mirror uses different ignore rules. Its encrypted rules will replace this folder's global rules before the first sync.",
        );
    } else if !preview.ignore_policy_known {
        message
            .push_str("\n\nThis is an older invite, so this folder's ignore rules will be kept.");
    }
    if preview.large_files.count > 0 {
        message.push_str(&format!(
            "\n\nLarge files: {}. They will use authenticated encrypted chunks; use `feanorfs ignore <pattern>` for disposable files.",
            preview.large_files.count
        ));
    }
    let examples = preview
        .conflicts
        .examples
        .iter()
        .chain(preview.local_only.examples.iter())
        .take(5)
        .cloned()
        .collect::<Vec<_>>();
    if !examples.is_empty() {
        message.push_str("\n\nExamples:\n");
        message.push_str(&examples.join("\n"));
    }
    message.push_str(
        "\n\nChoose OK only if you want to continue. FeanorFS will not auto-merge different content; those paths remain visible for your choice.",
    );
    message
}

pub fn stop_workspace(path: &Path) -> Result<(), String> {
    let path = path
        .to_str()
        .ok_or_else(|| "workspace path is not valid UTF-8".to_string())?;
    run_checked(&home_dir(), &stop_args(path), Some(Duration::from_secs(60)))
}

pub fn export_recovery_kit(
    workspace: &Path,
    destination: &Path,
    passphrase: Zeroizing<String>,
) -> Result<(), String> {
    run_with_stdin_secret(
        workspace,
        recovery_export_args(destination),
        passphrase,
        "recovery passphrase",
        "workspace recovery",
    )
}

pub fn import_recovery_kit(
    source: &Path,
    destination: &Path,
    passphrase: Zeroizing<String>,
) -> Result<(), String> {
    run_with_stdin_secret(
        &home_dir(),
        recovery_import_args(source, destination),
        passphrase,
        "recovery passphrase",
        "workspace recovery",
    )
}

fn tray_join_args(destination: &Path) -> Vec<OsString> {
    vec![
        "tray".into(),
        "join".into(),
        "--".into(),
        destination.as_os_str().to_owned(),
    ]
}

fn recovery_export_args(destination: &Path) -> Vec<OsString> {
    vec![
        "recovery".into(),
        "export".into(),
        "--replace".into(),
        "--passphrase-stdin".into(),
        "--".into(),
        destination.as_os_str().to_owned(),
    ]
}

fn recovery_import_args(source: &Path, destination: &Path) -> Vec<OsString> {
    vec![
        "recovery".into(),
        "import".into(),
        "--passphrase-stdin".into(),
        "--".into(),
        source.as_os_str().to_owned(),
        destination.as_os_str().to_owned(),
    ]
}

fn run_with_stdin_secret(
    current_dir: &Path,
    args: Vec<OsString>,
    secret: Zeroizing<String>,
    input_name: &str,
    operation: &str,
) -> Result<(), String> {
    if secret.contains(['\r', '\n', '\0']) {
        return Err(format!(
            "The {input_name} cannot contain line breaks or NUL characters."
        ));
    }
    let out = match CapturedCommand::new(feanorfs_bin())
        .args(args)
        .cwd(current_dir)
        .stdout_limit(DEFAULT_STDOUT_LIMIT)
        .stderr_limit(DEFAULT_STDERR_LIMIT)
        .secret_stdin(secret)
        .capture()
    {
        Ok(out) => out,
        Err(CapturedError::InvalidSecret) => {
            return Err(format!(
                "The {input_name} cannot contain line breaks or NUL characters."
            ));
        }
        Err(CapturedError::Stdin(error)) => {
            return Err(format!("send {input_name}: {error}"));
        }
        Err(error) => {
            return Err(format!("failed to start {operation}: {error}"));
        }
    };
    if out.status.success() {
        return Ok(());
    }
    let stderr = out.stderr.as_str_lossy().trim().to_string();
    let fallback = format!("{operation} failed");
    Err(truncate_error(if stderr.is_empty() {
        &fallback
    } else {
        &stderr
    }))
}

fn stop_args(path: &str) -> [&str; 4] {
    ["--json", "stop", "--", path]
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct PairReady {
    event: String,
    pub code: String,
    pub expires_in_seconds: u64,
}

pub enum PairSessionEvent {
    Ready(PairReady),
    Done {
        paired: bool,
        canceled: bool,
        error: Option<String>,
    },
}

fn pair_args() -> [&'static str; 4] {
    ["pair", "--tray", "--expires", PAIR_EXPIRES_SECONDS]
}

pub fn run_pairing_session(
    workspace: &Path,
    cancel: Receiver<()>,
    emit: impl FnMut(PairSessionEvent),
) {
    run_pairing_session_with_bin(workspace, &feanorfs_bin(), cancel, emit);
}

pub fn copy_pairing_clipboard(code: &str) {
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        let _ = clipboard.set_text(code);
    }
}

pub fn clear_pairing_clipboard(code: &str) {
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        if clipboard.get_text().is_ok_and(|current| current == code) {
            let _ = clipboard.set_text("");
        }
    }
}

fn run_pairing_session_with_bin(
    workspace: &Path,
    binary: &str,
    cancel: Receiver<()>,
    mut emit: impl FnMut(PairSessionEvent),
) {
    let mut command = Command::new(binary);
    command
        .args(pair_args())
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match spawn_child(&mut command) {
        Ok(child) => child,
        Err(error) => {
            emit(PairSessionEvent::Done {
                paired: false,
                canceled: false,
                error: Some(format!("failed to start secure pairing: {error}")),
            });
            return;
        }
    };

    let stdout = child.stdout.take().expect("pairing stdout is piped");
    let (ready_tx, ready_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let ready = std::io::Read::by_ref(&mut reader)
            .take(PAIR_LINE_LIMIT as u64 + 1)
            .read_line(&mut line)
            .map_err(|error| format!("read pairing code: {error}"))
            .and_then(|read| {
                if read == 0 {
                    Err("pairing ended before a code was ready".to_string())
                } else if read > PAIR_LINE_LIMIT || !line.ends_with('\n') {
                    Err("secure pairing event exceeded the 1024-byte limit".to_string())
                } else {
                    parse_pair_ready(line.trim_end())
                }
            });
        let _ = ready_tx.send(ready);
        for line in reader.lines() {
            if line.is_err() {
                break;
            }
        }
    });

    let mut ready_emitted = false;
    loop {
        match ready_rx.try_recv() {
            Ok(Ok(ready)) => {
                ready_emitted = true;
                emit(PairSessionEvent::Ready(ready));
            }
            Ok(Err(error)) => {
                graceful_stop_child(&mut child);
                let _ = reader.join();
                emit(PairSessionEvent::Done {
                    paired: false,
                    canceled: false,
                    error: Some(error),
                });
                return;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) if !ready_emitted => {
                graceful_stop_child(&mut child);
                let _ = reader.join();
                emit(PairSessionEvent::Done {
                    paired: false,
                    canceled: false,
                    error: Some("pairing code channel closed unexpectedly".into()),
                });
                return;
            }
            Err(TryRecvError::Disconnected) => {}
        }

        match cancel.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => {
                graceful_stop_child(&mut child);
                let _ = reader.join();
                emit(PairSessionEvent::Done {
                    paired: false,
                    canceled: true,
                    error: None,
                });
                return;
            }
            Err(TryRecvError::Empty) => {}
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = reader.join();
                if !ready_emitted {
                    if let Ok(Ok(ready)) = ready_rx.try_recv() {
                        emit(PairSessionEvent::Ready(ready));
                    }
                }
                let error = (!status.success()).then(|| pairing_stderr(&mut child));
                emit(PairSessionEvent::Done {
                    paired: status.success(),
                    canceled: false,
                    error,
                });
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                graceful_stop_child(&mut child);
                let _ = reader.join();
                emit(PairSessionEvent::Done {
                    paired: false,
                    canceled: false,
                    error: Some(format!("check secure pairing process: {error}")),
                });
                return;
            }
        }
    }
}

fn parse_pair_ready(line: &str) -> Result<PairReady, String> {
    let ready: PairReady = serde_json::from_str(line)
        .map_err(|error| format!("read secure pairing event: {error}"))?;
    let valid_lan_code = ready.code.len() == 24
        && ready.code.starts_with("fnp1-")
        && ready
            .code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    let relay_payload = ready.code.strip_prefix("fnp2-");
    let valid_relay_code = ready.code.len() <= 900
        && relay_payload.is_some_and(|payload| {
            !payload.is_empty()
                && payload.len().is_multiple_of(2)
                && payload
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        });
    if ready.event != "ready"
        || !(valid_lan_code || valid_relay_code)
        || !(30..=900).contains(&ready.expires_in_seconds)
    {
        return Err("feanorfs returned an invalid secure pairing event".into());
    }
    Ok(ready)
}

fn pairing_stderr(child: &mut Child) -> String {
    let captured = child
        .stderr
        .take()
        .map(|pipe| BoundedBytes::read(pipe, DEFAULT_STDERR_LIMIT))
        .unwrap_or_default();
    let stderr = captured.as_str_lossy();
    let message = stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("secure pairing stopped unexpectedly");
    truncate_error(message)
}

/// Gracefully stop a child and its whole process tree (see [`stop_child`]).
pub fn graceful_stop_child(child: &mut Child) {
    stop_child(child);
}

pub fn conflicts_keep(workspace: &Path, path: &str, choice: &str) -> Result<(), String> {
    let flag = match choice {
        "local" => "--local",
        "cloud" => "--cloud",
        "both" => "--both",
        _ => return Err(format!("unknown keep choice: {choice}")),
    };
    run_checked(
        workspace,
        &["--json", "conflicts", "keep", flag, "--", path],
        Some(Duration::from_secs(60)),
    )
}

pub fn conflicts_keep_all(workspace: &Path, choice: &str) -> Result<(), String> {
    let flag = match choice {
        "local" => "--local",
        "cloud" => "--cloud",
        _ => return Err(format!("unknown bulk keep choice: {choice}")),
    };
    run_checked(
        workspace,
        &["--json", "conflicts", "keep", "--all", flag],
        Some(Duration::from_secs(60)),
    )
}

pub fn agent_land(workspace: &Path, name: &str) -> Result<(), String> {
    run_checked(
        workspace,
        &["--json", "agent", "land", "--", name],
        Some(Duration::from_secs(60)),
    )
}

pub fn sync_once(workspace: &Path) -> Result<(), String> {
    // A user-triggered sync pass can legitimately take a long time.
    run_checked(workspace, &["--json", "sync", "--no-watch"], None)
}

#[derive(Deserialize)]
struct BackgroundServiceResult {
    status: String,
}

pub fn background_service_managed(workspace: &Path) -> bool {
    let Ok(out) = quick_capture(workspace, &["--json", "service", "status"]) else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    out.decode_json::<BackgroundServiceResult>()
        .is_ok_and(|result| result.status != "not_installed")
}

pub fn background_service_stop(workspace: &Path) -> Result<(), String> {
    run_checked(
        workspace,
        &["--json", "service", "stop"],
        Some(Duration::from_secs(60)),
    )
}

pub fn background_service_start(workspace: &Path) -> Result<(), String> {
    run_checked(
        workspace,
        &["--json", "service", "start"],
        Some(Duration::from_secs(60)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_common::tray_contract::{SetupRecovery, SetupStage};

    #[test]
    fn cli_discovery_prefers_override_then_colocated_then_packaged_binary() {
        let root = std::env::temp_dir().join(format!(
            "feanorfs-tray-cli-discovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let app = root.join("app");
        std::fs::create_dir_all(&app).unwrap();
        let current = app.join(format!("feanorfs-tray{}", std::env::consts::EXE_SUFFIX));
        let sibling = app.join(format!("feanorfs{}", std::env::consts::EXE_SUFFIX));
        let packaged = root.join("packaged-feanorfs");
        std::fs::write(&sibling, b"sibling").unwrap();
        std::fs::write(&packaged, b"packaged").unwrap();

        assert_eq!(
            select_feanorfs_bin(
                Some("/explicit/feanorfs".into()),
                Some(current.clone()),
                std::slice::from_ref(&packaged)
            )
            .as_deref(),
            Some("/explicit/feanorfs")
        );
        assert_eq!(
            select_feanorfs_bin(None, Some(current.clone()), std::slice::from_ref(&packaged)),
            Some(sibling.display().to_string())
        );
        std::fs::remove_file(&sibling).unwrap();
        assert_eq!(
            select_feanorfs_bin(None, Some(current), std::slice::from_ref(&packaged)),
            Some(packaged.display().to_string())
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn packaged_cli_candidate_matches_native_install_location() {
        let candidates = packaged_cli_candidates();
        #[cfg(target_os = "macos")]
        assert_eq!(candidates, [PathBuf::from("/usr/local/bin/feanorfs")]);
        #[cfg(target_os = "linux")]
        assert_eq!(candidates, [PathBuf::from("/usr/bin/feanorfs")]);
        #[cfg(target_os = "windows")]
        assert!(candidates.is_empty());
    }

    #[test]
    fn truncate_error_respects_char_boundary() {
        let msg = "é".repeat(400);
        let out = truncate_error(&msg);
        assert!(out.chars().count() <= 301);
    }

    #[test]
    fn status_failure_reassures_and_gives_recovery_before_details() {
        let message = status_failure_message(Some("connection refused"));
        assert!(message.starts_with("Sync status is unavailable."));
        assert!(message.contains("Your files were not changed."));
        assert!(message.contains("Check System Health"));
        assert!(message.ends_with("Details: connection refused"));
    }

    #[test]
    fn health_report_reads_only_named_statuses_from_doctor_json() {
        let report: HealthReport = serde_json::from_str(
            r#"{
                "ok": false,
                "checks": [
                    {
                        "name": "server",
                        "status": "failure",
                        "message": "detail intentionally ignored",
                        "action": "action intentionally ignored"
                    },
                    {
                        "name": "e2ee",
                        "status": "ok",
                        "message": "detail intentionally ignored"
                    }
                ]
            }"#,
        )
        .unwrap();
        assert!(!report.ok);
        assert_eq!(report.checks.len(), 2);
        assert_eq!(report.checks[0].name, "server");
        assert_eq!(report.checks[0].status, HealthStatus::Failure);
        assert_eq!(report.checks[1].status, HealthStatus::Ok);
    }

    #[test]
    fn pairing_subprocess_arguments_contain_no_generated_secret() {
        assert_eq!(
            pair_args(),
            ["pair", "--tray", "--expires", PAIR_EXPIRES_SECONDS]
        );
    }

    #[test]
    fn stop_subprocess_places_untrusted_path_after_separator() {
        assert_eq!(
            stop_args("--workspace-that-looks-like-a-flag"),
            ["--json", "stop", "--", "--workspace-that-looks-like-a-flag"]
        );
    }

    #[test]
    fn health_and_repair_subprocess_arguments_are_public_and_flag_safe() {
        assert_eq!(health_args(), ["--json", "doctor"]);
        assert_eq!(
            setup_json_args(Path::new("--folder-that-looks-like-a-flag")),
            vec![
                OsString::from("--json"),
                OsString::from("start"),
                OsString::from("--"),
                OsString::from("--folder-that-looks-like-a-flag"),
            ]
        );
        assert_eq!(update_args(), ["--json", "update"]);
    }

    #[test]
    fn tray_accepts_only_bounded_official_release_results() {
        let result = UpdateCheckResult {
            status: UpdateStatus::UpdateAvailable,
            current_version: "0.4.0".into(),
            latest_version: "0.5.0".into(),
            release_url: "https://github.com/rapm94/feanorfs/releases/tag/v0.5.0".into(),
        };
        assert!(official_release_result(&result));
        for invalid in [
            "https://github.example/rapm94/feanorfs/releases/tag/v0.5.0",
            "https://github.com.evil.example/rapm94/feanorfs/releases/tag/v0.5.0",
            "https://github.com/rapm94/feanorfs/releases/tag/v0.4.0",
            "https://github.com/rapm94/feanorfs/releases/tag/v0.5.0?download=1",
        ] {
            let mut tampered = result.clone();
            tampered.release_url = invalid.into();
            assert!(!official_release_result(&tampered));
        }
    }

    #[test]
    fn recovery_subprocess_arguments_contain_paths_but_no_secrets() {
        assert_eq!(
            recovery_export_args(Path::new("--kit.fnrk")),
            vec![
                OsString::from("recovery"),
                OsString::from("export"),
                OsString::from("--replace"),
                OsString::from("--passphrase-stdin"),
                OsString::from("--"),
                OsString::from("--kit.fnrk"),
            ]
        );
        assert_eq!(
            recovery_import_args(Path::new("kit.fnrk"), Path::new("--restored")),
            vec![
                OsString::from("recovery"),
                OsString::from("import"),
                OsString::from("--passphrase-stdin"),
                OsString::from("--"),
                OsString::from("kit.fnrk"),
                OsString::from("--restored"),
            ]
        );
        assert_eq!(
            tray_join_args(Path::new("--joined")),
            vec![
                OsString::from("tray"),
                OsString::from("join"),
                OsString::from("--"),
                OsString::from("--joined"),
            ]
        );
    }

    #[test]
    fn pairing_ready_event_is_strictly_validated() {
        let ready = parse_pair_ready(
            r#"{"event":"ready","code":"fnp1-2345-6789-ABCD-EFGH","expires_in_seconds":300}"#,
        )
        .unwrap();
        assert_eq!(ready.code, "fnp1-2345-6789-ABCD-EFGH");
        let relay =
            parse_pair_ready(r#"{"event":"ready","code":"fnp2-7b7d","expires_in_seconds":300}"#)
                .unwrap();
        assert_eq!(relay.code, "fnp2-7b7d");
        assert!(parse_pair_ready(
            r#"{"event":"ready","code":"fnr1-secret","expires_in_seconds":300}"#
        )
        .is_err());
        assert!(parse_pair_ready(
            r#"{"event":"ready","code":"fnp1-2345-6789-ABCD-EFGH","expires_in_seconds":999}"#
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn canceling_pairing_stops_the_child_without_an_error() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "feanorfs-tray-pair-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("fake-feanorfs");
        std::fs::write(
            &script,
            b"#!/bin/sh\nprintf '%s\\n' '{\"event\":\"ready\",\"code\":\"fnp1-2345-6789-ABCD-EFGH\",\"expires_in_seconds\":300}'\ntrap 'exit 0' TERM\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

        let (cancel, cancel_rx) = mpsc::channel();
        let mut saw_ready = false;
        let mut done = None;
        run_pairing_session_with_bin(
            &root,
            script.to_str().unwrap(),
            cancel_rx,
            |event| match event {
                PairSessionEvent::Ready(_) => {
                    saw_ready = true;
                    cancel.send(()).unwrap();
                }
                PairSessionEvent::Done {
                    paired,
                    canceled,
                    error,
                } => done = Some((paired, canceled, error)),
            },
        );

        assert!(saw_ready);
        assert_eq!(done, Some((false, true, None)));
        std::fs::remove_dir_all(root).unwrap();
    }

    // All current callers are unix-gated child-process tests; Windows keeps
    // the helper compiled for future cross-platform coverage.
    #[cfg_attr(windows, allow(dead_code))]
    fn fake_cli_dir(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "feanorfs-tray-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[cfg(unix)]
    fn write_executable_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    fn process_alive(pid: i32) -> bool {
        // `kill(pid, 0)` returns 0 while the process exists (same user).
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[cfg(unix)]
    #[test]
    fn oversized_child_output_is_bounded_and_decode_fails_typed() {
        let root = fake_cli_dir("overflow");
        let script = root.join("fake-feanorfs");
        write_executable_script(
            &script,
            "#!/bin/sh\nhead -c 200000 /dev/zero | tr '\\0' 'x'\n",
        );
        let output = CapturedCommand::new(script.as_os_str().to_owned())
            .stdout_limit(1024)
            .stderr_limit(1024)
            .capture()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.truncated);
        assert!(output.stdout.bytes.len() <= 1024);
        assert!(output.stderr.bytes.is_empty());
        let decoded: Result<TrayStatusResult, _> = output.decode_json();
        assert!(matches!(
            decoded,
            Err(CapturedError::OutputOverLimit { stream: "stdout" })
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn hung_child_times_out_and_its_process_tree_is_cleaned_up() {
        let root = fake_cli_dir("hung");
        let marker = root.join("orphan.pid");
        let script = root.join("fake-feanorfs");
        // The grandchild ignores TERM, so only the group SIGKILL can reach it.
        write_executable_script(
            &script,
            &format!(
                "#!/bin/sh\nsh -c 'echo $$ > \"{marker}\"; trap \"\" TERM; while :; do sleep 1; done' &\nwhile :; do sleep 1; done\n",
                marker = marker.display()
            ),
        );
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = CapturedCommand::new(script.as_os_str().to_owned())
                .stdout_limit(1024)
                .stderr_limit(1024)
                .timeout(Duration::from_millis(1500))
                .capture();
            let _ = result_tx.send(result);
        });
        let result = result_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("capture must return");
        assert!(matches!(result, Err(CapturedError::Timeout { .. })));

        // The grandchild may spawn a moment before the timeout fires; if it
        // did, it must be dead after the process-tree cleanup.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if marker.is_file() {
            let pid: i32 = std::fs::read_to_string(&marker)
                .expect("orphan marker")
                .trim()
                .parse()
                .expect("orphan pid");
            let deadline = Instant::now() + Duration::from_secs(2);
            while process_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(!process_alive(pid), "orphaned grandchild must be reaped");
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn early_exit_without_output_is_a_typed_no_output_error() {
        let root = fake_cli_dir("early");
        let script = root.join("fake-feanorfs");
        write_executable_script(&script, "#!/bin/sh\nexit 0\n");
        let output = CapturedCommand::new(script.as_os_str().to_owned())
            .stdout_limit(1024)
            .stderr_limit(1024)
            .capture()
            .unwrap();
        assert!(output.status.success());
        assert!(matches!(
            output.decode_json::<TrayStatusResult>(),
            Err(CapturedError::NoOutput)
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn malformed_json_is_a_typed_decode_error() {
        let root = fake_cli_dir("bad-json");
        let script = root.join("fake-feanorfs");
        write_executable_script(&script, "#!/bin/sh\nprintf 'this is not json'\n");
        let output = CapturedCommand::new(script.as_os_str().to_owned())
            .stdout_limit(1024)
            .stderr_limit(1024)
            .capture()
            .unwrap();
        assert!(matches!(
            output.decode_json::<TrayStatusResult>(),
            Err(CapturedError::MalformedJson(_))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn secret_stdin_is_one_line_and_never_exposed_to_argv_env_or_errors() {
        let root = fake_cli_dir("secret");
        let argv_file = root.join("argv.txt");
        let env_file = root.join("env.txt");
        let stdin_file = root.join("stdin.txt");
        let script = root.join("fake-feanorfs");
        write_executable_script(
            &script,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{argv_file}\"\nenv > \"{env_file}\"\ncat > \"{stdin_file}\"\n",
                argv_file = argv_file.display(),
                env_file = env_file.display(),
                stdin_file = stdin_file.display(),
            ),
        );

        let secret = Zeroizing::new("hunter2-secret".to_string());
        let command = CapturedCommand::new(script.as_os_str().to_owned())
            .args([OsString::from("--flag"), OsString::from("--value")])
            .stdout_limit(1024)
            .stderr_limit(1024)
            .secret_stdin(secret.clone());
        // Debug is a log surface: it must never expose the secret.
        let debug_render = format!("{command:?}");
        assert!(!debug_render.contains("hunter2-secret"));

        let output = command.capture().unwrap();
        assert!(output.status.success());

        let argv = std::fs::read_to_string(&argv_file).unwrap();
        let env = std::fs::read_to_string(&env_file).unwrap();
        let stdin = std::fs::read_to_string(&stdin_file).unwrap();
        assert!(!argv.contains("hunter2-secret"));
        assert!(!env.contains("hunter2-secret"));
        assert_eq!(stdin, "hunter2-secret\n");

        // Errors never echo the secret either.
        let error = CapturedCommand::new(root.join("does-not-exist").as_os_str().to_owned())
            .secret_stdin(secret)
            .capture()
            .expect_err("spawn must fail");
        assert!(!format!("{error:?}").contains("hunter2-secret"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn secret_stdin_rejects_line_breaks_without_spawning() {
        let root = fake_cli_dir("bad-secret");
        let script = root.join("fake-feanorfs");
        write_executable_script(&script, "#!/bin/sh\ncat\n");
        // Linux containers can answer exec of a freshly written file with
        // ETXTBSY; that is transport noise around the setup, not the
        // contract under test. Only the invalid-secret rejection must hold.
        let mut attempts = 0;
        let error = loop {
            match CapturedCommand::new(script.as_os_str().to_owned())
                .secret_stdin(Zeroizing::new("line\nbreak".to_string()))
                .capture()
            {
                Err(error @ CapturedError::InvalidSecret) => break error,
                Err(CapturedError::Spawn(message))
                    if message.contains("Text file busy") && attempts < 5 =>
                {
                    attempts += 1;
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                other => panic!("invalid secret must be rejected without spawning: {other:?}"),
            }
        };
        assert_eq!(error, CapturedError::InvalidSecret);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn canceling_a_capture_stops_the_child_process_tree() {
        let root = fake_cli_dir("cancel");
        let marker = root.join("started.pid");
        let script = root.join("fake-feanorfs");
        write_executable_script(
            &script,
            &format!(
                "#!/bin/sh\necho $$ > \"{marker}\"\nwhile :; do sleep 1; done\n",
                marker = marker.display(),
            ),
        );
        let (cancel_tx, cancel_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = CapturedCommand::new(script.as_os_str().to_owned())
                .stdout_limit(1024)
                .stderr_limit(1024)
                .capture_with_cancel(Some(&cancel_rx));
            let _ = result_tx.send(result);
        });
        // Wait for the child to be running, then cancel it. Cold CI runners
        // can take seconds to schedule the script process.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !marker.is_file() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(marker.is_file(), "child must start before cancellation");
        cancel_tx.send(()).unwrap();
        let result = result_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("capture must return");
        assert!(matches!(result, Err(CapturedError::Canceled)));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn setup_result_line_extraction_skips_human_progress_and_preview() {
        let mut stdout = BoundedBytes {
            bytes: br#"Running sync...
Initial sync completed.
{"event":"join_preview","preview":{}}
Some human wording that changed completely
{"stage":"initial_sync","committed":{"workspace_configured":true,"initial_sync_completed":true,"background_service_installed":false,"tray_registered":false},"retryable":true,"recovery":"retry_start","detail":"reworded CLI message"}
"#
            .to_vec(),
            truncated: false,
        };
        let result = last_setup_result_line(&stdout).expect("typed line must be found");
        assert_eq!(result.stage, SetupStage::InitialSync);
        assert_eq!(result.detail.as_deref(), Some("reworded CLI message"));

        // No typed line at all (crashed/very old CLI) → None, so the caller
        // falls back to a generic outcome.
        stdout.bytes = b"just human text\n".to_vec();
        assert!(last_setup_result_line(&stdout).is_none());
    }

    #[test]
    fn setup_result_json_fixture_decodes_stable_stage_names() {
        let fixture = r#"{"stage":"service_installed","committed":{"workspace_configured":true,"initial_sync_completed":true,"background_service_installed":true,"tray_registered":false},"retryable":true,"recovery":"retry_tray","detail":"whatever the CLI says"}"#;
        let result: SetupResult = serde_json::from_str(fixture).unwrap();
        assert_eq!(result.stage, SetupStage::ServiceInstalled);
        assert_eq!(result.recovery, Some(SetupRecovery::RetryTray));
        assert!(result.committed.background_service_installed);
    }
}
