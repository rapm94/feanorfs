pub(super) fn os_store_allowed() -> bool {
    match std::env::var("FEANORFS_CREDENTIAL_STORE").as_deref() {
        Ok("file") => false,
        Ok("os") => true,
        _ => platform_store_allowed(),
    }
}

#[cfg(target_os = "macos")]
fn platform_store_allowed() -> bool {
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;

    static SIGNED: OnceLock<bool> = OnceLock::new();
    *SIGNED.get_or_init(|| {
        let Ok(executable) = std::env::current_exe() else {
            return false;
        };
        let verified = Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(&executable)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !verified {
            return false;
        }
        let Ok(details) = Command::new("/usr/bin/codesign")
            .args(["--display", "--verbose=4"])
            .arg(executable)
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&details.stderr)
            .lines()
            .any(|line| line.starts_with("Authority=Developer ID Application:"))
    })
}

#[cfg(not(target_os = "macos"))]
fn platform_store_allowed() -> bool {
    true
}
