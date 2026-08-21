use super::*;

#[cfg(unix)]
#[test]
fn process_elapsed_parser_is_bounded_and_exact() {
    assert_eq!(parse_process_elapsed("00:07"), Some(7));
    assert_eq!(parse_process_elapsed("01:02:03"), Some(3_723));
    assert_eq!(parse_process_elapsed("2-01:02:03"), Some(176_523));
    assert_eq!(parse_process_elapsed("bogus"), None);
    assert_eq!(parse_process_elapsed("1:2:3:4"), None);
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn process_identity_reads_kernel_executable_path() {
    let actual = process_executable(std::process::id()).expect("read current executable");
    let expected = std::env::current_exe().expect("resolve current executable");
    assert_eq!(
        std::fs::canonicalize(actual).unwrap(),
        std::fs::canonicalize(expected).unwrap()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn verified_residual_runner_group_is_terminated() {
    use std::os::unix::process::CommandExt as _;

    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    let process_start_id = process_tree::process_start_identifier(pid, "residual-test");
    let metadata = feanorfs_agent_core::RunnerProcessMetadata {
        pid,
        process_start_id,
    };
    assert!(runner_process_start_matches(&metadata));

    let cleanup = std::thread::spawn(move || terminate_verified_runner_group(&metadata));
    let status = child.wait().unwrap();
    assert!(!status.success());
    assert!(cleanup.join().unwrap());
    assert!(!runner_process_group_exists(pid));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn mismatched_runner_identity_never_signals_group() {
    use std::os::unix::process::CommandExt as _;

    let mut child = std::process::Command::new("/bin/sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    let valid = process_tree::process_start_identifier(pid, "mismatch-test");
    let mismatch = if valid.ends_with(":0") {
        format!("{valid}1")
    } else {
        format!("{valid}0")
    };
    let metadata = feanorfs_agent_core::RunnerProcessMetadata {
        pid,
        process_start_id: mismatch,
    };
    assert!(!runner_process_start_matches(&metadata));
    assert!(!terminate_verified_runner_group(&metadata));
    assert!(feanorfs_agent_core::lock::pid_alive(pid));
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(target_os = "windows")]
#[test]
#[ignore]
fn supervisor_job_descendant_helper() {
    let descendant_path = std::env::var_os("FEANORFS_SUPERVISOR_DESCENDANT")
        .map(PathBuf::from)
        .expect("descendant pid path");
    let executable = std::env::current_exe().expect("test executable");
    let descendant = std::process::Command::new(executable)
        .args([
            "--ignored",
            "--exact",
            "cli::supervisor::tests::platform::supervisor_job_descendant_sleep_helper",
            "--nocapture",
        ])
        .spawn()
        .expect("spawn descendant");
    std::fs::write(descendant_path, descendant.id().to_string()).expect("record descendant pid");
    std::thread::sleep(Duration::from_secs(30));
}

#[cfg(target_os = "windows")]
#[test]
#[ignore]
fn supervisor_job_descendant_sleep_helper() {
    std::thread::sleep(Duration::from_secs(30));
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unverified_runner_process_start_is_never_signaled() {
    let metadata = feanorfs_agent_core::RunnerProcessMetadata {
        pid: std::process::id(),
        process_start_id: format!("spawn:{}:session", std::process::id()),
    };
    assert!(!runner_process_start_matches(&metadata));
    #[cfg(not(unix))]
    assert!(!terminate_verified_runner_group(&metadata));
}
