//! Spawn `feanorfs` subprocesses — the tray never duplicates sync logic.

use feanorfs_common::tray_contract::{RecentWorkspacesResult, TrayStatusResult};
use serde::Deserialize;
use std::ffi::OsString;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

const PAIR_EXPIRES_SECONDS: &str = "300";

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

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct UpdateCheckResult {
    pub status: UpdateStatus,
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStatus {
    UpToDate,
    UpdateAvailable,
    DevelopmentBuild,
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
    path.join(".feanorfs").join("config.json").is_file()
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"))
}

fn run_in(workspace: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new(feanorfs_bin())
        .args(args)
        .current_dir(workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

fn run_checked(workspace: &Path, args: &[&str]) -> Result<(), String> {
    let out = run_in(workspace, args).map_err(|error| {
        truncate_error(&format!(
            "FeanorFS could not start its sync command. No files were changed. Reinstall FeanorFS and try again. Details: {error}"
        ))
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = if stderr.is_empty() {
            format!("feanorfs exited with {}", out.status)
        } else {
            stderr
        };
        return Err(truncate_error(&msg));
    }
    Ok(())
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
    let out = run_in(workspace, &["--json", "tray", "status"]).map_err(|error| {
        truncate_error(&format!(
            "Sync status is unavailable because the FeanorFS command could not start. Your files were not changed. Reinstall FeanorFS and try again. Details: {error}"
        ))
    })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let detail = stderr
            .trim()
            .strip_prefix("Error:")
            .map(str::trim)
            .filter(|detail| !detail.is_empty());
        return Err(status_failure_message(detail));
    }
    serde_json::from_slice(&out.stdout).map_err(|_| {
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
    let out = run_in(workspace, &health_args()).map_err(|_| {
        "System health could not be checked because the FeanorFS command is unavailable. Your files were not changed. Reinstall FeanorFS and try again."
            .to_string()
    })?;
    if !out.status.success() {
        return Err(
            "System health could not be checked. Your files were not changed. Reopen FeanorFS and try again."
                .into(),
        );
    }
    serde_json::from_slice(&out.stdout).map_err(|_| {
        "System health could not be read from the installed FeanorFS command. Your files were not changed. Reinstall FeanorFS and try again."
            .into()
    })
}

fn health_args() -> [&'static str; 2] {
    ["--json", "doctor"]
}

pub fn check_for_updates() -> Result<UpdateCheckResult, String> {
    let out = run_in(&home_dir(), &update_args()).map_err(|_| {
        "Updates could not be checked because the FeanorFS command is unavailable. The installed app was not changed. Reinstall FeanorFS and try again."
            .to_string()
    })?;
    if !out.status.success() {
        return Err(
            "Updates could not be checked. The installed app was not changed. Check your internet connection and try again."
                .into(),
        );
    }
    let result: UpdateCheckResult = serde_json::from_slice(&out.stdout).map_err(|_| {
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
    run_checked(workspace, &["--json", "tray", sub])
}

pub fn tray_recent() -> Option<RecentWorkspacesResult> {
    let out = run_in(&home_dir(), &["--json", "tray", "recent"]).ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

pub fn forget_unavailable_workspaces() -> Result<RecentWorkspacesResult, String> {
    let out = run_in(
        &home_dir(),
        &["--json", "tray", "forget-unavailable"],
    )
    .map_err(|error| {
        truncate_error(&format!(
            "The unavailable workspace entries could not be removed. No files or workspace data were changed. Reopen FeanorFS and try again. Details: {error}"
        ))
    })?;
    if !out.status.success() {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(truncate_error(&format!(
            "The unavailable workspace entries could not be removed. No files or workspace data were changed. Reopen FeanorFS and try again. Details: {detail}"
        )));
    }
    serde_json::from_slice(&out.stdout).map_err(|_| {
        "The workspace list could not be refreshed. No files or workspace data were changed. Reinstall FeanorFS and try again."
            .into()
    })
}

pub fn tray_activate(path: &Path) -> Result<(), String> {
    let path_str = path
        .to_str()
        .ok_or_else(|| "workspace path is not valid UTF-8".to_string())?;
    run_checked(&home_dir(), &["tray", "activate", "--", path_str])
}

pub fn start_workspace(path: &Path) -> Result<(), String> {
    let path = path
        .to_str()
        .ok_or_else(|| "workspace path is not valid UTF-8".to_string())?;
    run_checked(&home_dir(), &start_args(path))
}

fn start_args(path: &str) -> [&str; 3] {
    ["start", "--", path]
}

pub fn join_workspace(path: &Path, pairing_code: Zeroizing<String>) -> Result<(), String> {
    join_workspace_interactive(path, pairing_code).and_then(|outcome| match outcome {
        JoinOutcome::Joined => Ok(()),
        JoinOutcome::Canceled => Err("__FEANORFS_JOIN_CANCELED__".into()),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JoinOutcome {
    Joined,
    Canceled,
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

fn join_workspace_interactive(
    path: &Path,
    pairing_code: Zeroizing<String>,
) -> Result<JoinOutcome, String> {
    if pairing_code.contains(['\r', '\n', '\0']) {
        return Err("The pairing capability cannot contain line breaks or NUL characters.".into());
    }
    let mut child = Command::new(feanorfs_bin())
        .args(tray_join_args(path))
        .current_dir(home_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start secure workspace join: {error}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open secure pairing input".to_string())?;
    stdin
        .write_all(pairing_code.as_bytes())
        .and_then(|()| stdin.write_all(b"\n"))
        .and_then(|()| stdin.flush())
        .map_err(|error| format!("send pairing capability: {error}"))?;
    drop(pairing_code);

    let stdout = child.stdout.take().expect("join stdout is piped");
    let mut stdout = BufReader::new(stdout);
    let mut event_line = String::new();
    let bytes = std::io::Read::by_ref(&mut stdout)
        .take(65_537)
        .read_line(&mut event_line)
        .map_err(|error| format!("read secure join preview: {error}"))?;
    if bytes == 0 || bytes > 65_536 || !event_line.ends_with('\n') {
        graceful_stop_child(&mut child);
        return Err("FeanorFS ended before the safe join preview was ready.".into());
    }
    let event: JoinPreviewEvent = serde_json::from_str(event_line.trim_end())
        .map_err(|_| "FeanorFS returned an invalid safe join preview.".to_string())?;
    if event.event != "join_preview" {
        graceful_stop_child(&mut child);
        return Err("FeanorFS returned an unexpected secure join stage.".into());
    }

    let needs_confirmation = event.preview.local_only.count > 0
        || event.preview.conflicts.count > 0
        || event.preview.ignore_policy_differs;
    if needs_confirmation {
        let confirmed = rfd::MessageDialog::new()
            .set_title("Join this existing folder?")
            .set_description(join_confirmation_copy(&event.preview))
            .set_level(rfd::MessageLevel::Warning)
            .set_buttons(rfd::MessageButtons::OkCancel)
            .show();
        if !matches!(confirmed, rfd::MessageDialogResult::Ok) {
            let _ = stdin.write_all(b"CANCEL\n");
            drop(stdin);
            graceful_stop_child(&mut child);
            return Ok(JoinOutcome::Canceled);
        }
    }
    stdin
        .write_all(b"CONFIRM\n")
        .and_then(|()| stdin.flush())
        .map_err(|error| format!("confirm secure workspace join: {error}"))?;
    drop(stdin);

    let stdout_thread = std::thread::spawn(move || drain_bounded(stdout, 8192));
    let stderr = child.stderr.take().expect("join stderr is piped");
    let stderr_thread = std::thread::spawn(move || drain_bounded(stderr, 8192));
    let status = child
        .wait()
        .map_err(|error| format!("wait for secure workspace join: {error}"))?;
    let _ = stdout_thread.join();
    let stderr = stderr_thread.join().unwrap_or_default();
    if status.success() {
        Ok(JoinOutcome::Joined)
    } else {
        let stderr = String::from_utf8_lossy(&stderr).trim().to_string();
        Err(truncate_error(if stderr.is_empty() {
            "secure workspace join failed"
        } else {
            &stderr
        }))
    }
}

fn drain_bounded(mut reader: impl std::io::Read, limit: usize) -> Vec<u8> {
    let mut captured = Vec::with_capacity(limit);
    let mut buffer = [0_u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                let remaining = limit.saturating_sub(captured.len());
                captured.extend_from_slice(&buffer[..read.min(remaining)]);
            }
        }
    }
    captured
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
            "\n\nThe mirror uses different ignore rules. Its encrypted .feanorfsignore policy will replace this folder's policy before the first sync.",
        );
    } else if !preview.ignore_policy_known {
        message
            .push_str("\n\nThis is an older invite, so this folder's ignore rules will be kept.");
    }
    if preview.large_files.count > 0 {
        message.push_str(&format!(
            "\n\nLarge files: {}. They will use authenticated encrypted chunks; add disposable files to .feanorfsignore to avoid transferring them.",
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
    run_checked(&home_dir(), &stop_args(path))
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
    let mut child = Command::new(feanorfs_bin())
        .args(&args)
        .current_dir(current_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start {operation}: {error}"))?;
    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| format!("failed to open secure {input_name} input"))
        .and_then(|mut stdin| {
            stdin
                .write_all(secret.as_bytes())
                .and_then(|()| stdin.write_all(b"\n"))
                .map_err(|error| format!("send {input_name}: {error}"))
        });
    drop(secret);
    if let Err(error) = write_result {
        graceful_stop_child(&mut child);
        return Err(error);
    }
    let out = child
        .wait_with_output()
        .map_err(|error| format!("wait for {operation}: {error}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
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
    let mut child = match Command::new(binary)
        .args(pair_args())
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
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
            .take(1025)
            .read_line(&mut line)
            .map_err(|error| format!("read pairing code: {error}"))
            .and_then(|read| {
                if read == 0 {
                    Err("pairing ended before a code was ready".to_string())
                } else if read > 1024 || !line.ends_with('\n') {
                    Err("secure pairing event exceeded 1024 bytes".to_string())
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
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let message = stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("secure pairing stopped unexpectedly");
    truncate_error(message)
}

pub fn graceful_stop_child(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
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
    )
}

pub fn agent_land(workspace: &Path, name: &str) -> Result<(), String> {
    run_checked(workspace, &["--json", "agent", "land", "--", name])
}

pub fn sync_once(workspace: &Path) -> Result<(), String> {
    run_checked(workspace, &["--json", "sync", "--no-watch"])
}

#[derive(Deserialize)]
struct BackgroundServiceResult {
    status: String,
}

pub fn background_service_managed(workspace: &Path) -> bool {
    let Ok(out) = run_in(workspace, &["--json", "service", "status"]) else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    serde_json::from_slice::<BackgroundServiceResult>(&out.stdout)
        .is_ok_and(|result| result.status != "not_installed")
}

pub fn background_service_stop(workspace: &Path) -> Result<(), String> {
    run_checked(workspace, &["--json", "service", "stop"])
}

pub fn background_service_start(workspace: &Path) -> Result<(), String> {
    run_checked(workspace, &["--json", "service", "start"])
}

#[cfg(test)]
mod tests {
    use super::*;

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
            start_args("--folder-that-looks-like-a-flag"),
            ["start", "--", "--folder-that-looks-like-a-flag"]
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
}
