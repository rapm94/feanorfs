use anyhow::{ensure, Context as _};
use reqwest::Url;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::util::output_json;

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/rapm94/feanorfs/releases/latest";
const OFFICIAL_RELEASE_PATH_PREFIX: &str = "/rapm94/feanorfs/releases/tag/";
/// Bounded response cap: large enough for the official release metadata and
/// its asset list, small enough to keep the HTTPS-only probe bounded.
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

/// Per-machine throttle for periodic update checks.
const PERIODIC_CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const UPDATE_STATE_FILE: &str = "update-state.json";
const MAX_UPDATE_STATE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UpdateStatus {
    UpToDate,
    UpdateAvailable,
    DevelopmentBuild,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UpdateCheckResult {
    pub(crate) status: UpdateStatus,
    pub(crate) current_version: String,
    pub(crate) latest_version: String,
    pub(crate) release_url: String,
}

/// Secret-free per-machine throttle state in global state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct UpdateState {
    last_check_ms: i64,
    status: UpdateStatus,
    current_version: String,
    latest_version: String,
    release_url: String,
}

fn update_state_path() -> anyhow::Result<PathBuf> {
    Ok(feanorfs_agent_core::global_state_root()?.join(UPDATE_STATE_FILE))
}

fn load_update_state_at(path: &Path) -> Option<UpdateState> {
    let file = std::fs::File::open(path).ok()?;
    let mut content = Vec::new();
    file.take((MAX_UPDATE_STATE_BYTES + 1) as u64)
        .read_to_end(&mut content)
        .ok()?;
    if content.len() > MAX_UPDATE_STATE_BYTES {
        return None;
    }
    serde_json::from_slice(&content).ok()
}

fn save_update_state_at(path: &Path, state: &UpdateState) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(state)?;
    ensure!(
        bytes.len() <= MAX_UPDATE_STATE_BYTES,
        "periodic update state is unexpectedly large"
    );
    let parent = path
        .parent()
        .context("periodic update state path has no parent directory")?;
    std::fs::create_dir_all(parent).context("create periodic update state directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .context("protect periodic update state directory")?;
    }
    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;
    file.write_all(&bytes)?;
    file.commit()?;
    Ok(())
}

fn save_update_state(state: &UpdateState) {
    let Ok(path) = update_state_path() else {
        return;
    };
    if let Err(error) = save_update_state_at(&path, state) {
        tracing::warn!(%error, "could not persist periodic update state");
    }
}

/// Returns the cached periodic result when it is still inside the throttle
/// window; `None` means a fresh network check is due.
fn cached_within_throttle_at(path: &Path, current_version: &str) -> Option<UpdateCheckResult> {
    let state = load_update_state_at(path)?;
    if state.current_version != current_version {
        return None;
    }
    let age_ms = chrono::Utc::now()
        .timestamp_millis()
        .checked_sub(state.last_check_ms)?;
    if age_ms < 0
        || age_ms >= i64::try_from(PERIODIC_CHECK_INTERVAL.as_millis()).unwrap_or(i64::MAX)
    {
        return None;
    }
    validated_cached_result(state)
}

fn validated_cached_result(state: UpdateState) -> Option<UpdateCheckResult> {
    let expected = evaluate_release(
        &state.current_version,
        ReleaseMetadata {
            tag_name: format!("v{}", state.latest_version),
            html_url: state.release_url.clone(),
            draft: false,
            prerelease: false,
        },
    )
    .ok()?;
    (expected.status == state.status).then_some(expected)
}

/// Returns the cached periodic result when it is still inside the throttle
/// window; `None` means a fresh network check is due.
fn cached_within_throttle() -> Option<UpdateCheckResult> {
    cached_within_throttle_at(&update_state_path().ok()?, env!("CARGO_PKG_VERSION"))
}

/// Records the last periodic result so every machine surface reports the same
/// throttled answer.
fn record_periodic_state(result: &UpdateCheckResult) {
    save_update_state(&UpdateState {
        last_check_ms: chrono::Utc::now().timestamp_millis(),
        status: result.status,
        current_version: result.current_version.clone(),
        latest_version: result.latest_version.clone(),
        release_url: result.release_url.clone(),
    });
}

/// Read the last periodic check result from global state without any network.
pub(crate) fn last_periodic_result() -> Option<UpdateCheckResult> {
    cached_within_throttle()
}

#[derive(Debug, Deserialize)]
struct ReleaseMetadata {
    tag_name: String,
    html_url: String,
    draft: bool,
    prerelease: bool,
}

pub(crate) async fn run(json: bool, periodic: bool) -> anyhow::Result<()> {
    if periodic {
        if let Some(cached) = cached_within_throttle() {
            return render_result(&cached, json, true);
        }
    }
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .user_agent(format!("feanorfs/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build secure release client")?;
    let mut response = client
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .context("check the official FeanorFS release")?
        .error_for_status()
        .context("official FeanorFS release service returned an error")?;
    if let Some(length) = response.content_length() {
        ensure!(
            length <= MAX_RESPONSE_BYTES as u64,
            "official FeanorFS release response is unexpectedly large"
        );
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("read official FeanorFS release response")?
    {
        ensure!(
            chunk.len() <= MAX_RESPONSE_BYTES.saturating_sub(body.len()),
            "official FeanorFS release response is unexpectedly large"
        );
        body.extend_from_slice(&chunk);
    }
    let metadata: ReleaseMetadata =
        serde_json::from_slice(&body).context("parse official FeanorFS release response")?;
    let result = evaluate_release(env!("CARGO_PKG_VERSION"), metadata)?;
    if periodic {
        record_periodic_state(&result);
    }
    render_result(&result, json, periodic)
}

fn render_result(result: &UpdateCheckResult, json: bool, periodic: bool) -> anyhow::Result<()> {
    if json {
        return output_json(result);
    }
    if periodic {
        if result.status == UpdateStatus::UpdateAvailable {
            println!(
                "FeanorFS {} is available; this computer has {}.",
                result.latest_version, result.current_version
            );
            println!(
                "Run `feanorfs update` or open the verified release page: {}",
                result.release_url
            );
            println!("FeanorFS does not download or execute updates automatically.");
        } else if result.status == UpdateStatus::UpToDate {
            println!("FeanorFS is up to date with the latest stable release.");
        }
        return Ok(());
    }
    render(result);
    Ok(())
}

fn evaluate_release(
    current_version: &str,
    metadata: ReleaseMetadata,
) -> anyhow::Result<UpdateCheckResult> {
    ensure!(
        !metadata.draft && !metadata.prerelease,
        "official latest release is not stable"
    );
    let tag_version = metadata
        .tag_name
        .strip_prefix('v')
        .context("official release tag must start with v")?;
    let current = Version::parse(current_version).context("parse installed FeanorFS version")?;
    let latest = Version::parse(tag_version).context("parse official FeanorFS release version")?;
    ensure!(
        latest.pre.is_empty() && latest.build.is_empty(),
        "official latest release must use a stable version"
    );
    let release_url = validate_release_url(&metadata.html_url, &metadata.tag_name)?;
    let status = match current.cmp(&latest) {
        std::cmp::Ordering::Less => UpdateStatus::UpdateAvailable,
        std::cmp::Ordering::Equal => UpdateStatus::UpToDate,
        std::cmp::Ordering::Greater => UpdateStatus::DevelopmentBuild,
    };
    Ok(UpdateCheckResult {
        status,
        current_version: current.to_string(),
        latest_version: latest.to_string(),
        release_url,
    })
}

fn validate_release_url(value: &str, tag: &str) -> anyhow::Result<String> {
    let url = Url::parse(value).context("parse official release URL")?;
    ensure!(
        url.scheme() == "https",
        "official release URL must use HTTPS"
    );
    ensure!(
        url.host_str() == Some("github.com"),
        "official release URL must use github.com"
    );
    ensure!(
        url.port().is_none()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "official release URL contains unexpected authority or suffix data"
    );
    ensure!(
        url.path() == format!("{OFFICIAL_RELEASE_PATH_PREFIX}{tag}"),
        "official release URL path does not match its tag"
    );
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn render(result: &UpdateCheckResult) {
    match result.status {
        UpdateStatus::UpToDate => println!(
            "FeanorFS {} is up to date with the latest stable release.",
            result.current_version
        ),
        UpdateStatus::UpdateAvailable => {
            println!(
                "FeanorFS {} is available; this computer has {}.",
                result.latest_version, result.current_version
            );
            println!("Open the verified release page: {}", result.release_url);
            println!("FeanorFS does not download or execute updates automatically.");
        }
        UpdateStatus::DevelopmentBuild => println!(
            "This FeanorFS build ({}) is newer than the latest stable release ({}).",
            result.current_version, result.latest_version
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str) -> ReleaseMetadata {
        ReleaseMetadata {
            tag_name: tag.into(),
            html_url: format!("https://github.com/rapm94/feanorfs/releases/tag/{tag}"),
            draft: false,
            prerelease: false,
        }
    }

    #[test]
    fn semantic_versions_distinguish_current_update_and_development_builds() {
        assert_eq!(
            evaluate_release("0.4.0", release("v0.4.0")).unwrap().status,
            UpdateStatus::UpToDate
        );
        assert_eq!(
            evaluate_release("0.4.0", release("v0.4.1")).unwrap().status,
            UpdateStatus::UpdateAvailable
        );
        assert_eq!(
            evaluate_release("0.5.0", release("v0.4.1")).unwrap().status,
            UpdateStatus::DevelopmentBuild
        );
        assert_eq!(
            evaluate_release("0.9.0", release("v0.10.0"))
                .unwrap()
                .status,
            UpdateStatus::UpdateAvailable
        );
    }

    #[test]
    fn release_metadata_fails_closed_on_unstable_or_malformed_tags() {
        let mut draft = release("v0.4.0");
        draft.draft = true;
        assert!(evaluate_release("0.4.0", draft).is_err());
        let mut prerelease = release("v0.4.0");
        prerelease.prerelease = true;
        assert!(evaluate_release("0.4.0", prerelease).is_err());
        assert!(evaluate_release("0.4.0", release("0.4.0")).is_err());
        assert!(evaluate_release("0.4.0", release("v0.4.1-beta.1")).is_err());
    }

    #[test]
    fn release_url_is_restricted_to_the_matching_official_https_tag() {
        assert!(validate_release_url(
            "https://github.com/rapm94/feanorfs/releases/tag/v0.4.0",
            "v0.4.0"
        )
        .is_ok());
        for invalid in [
            "http://github.com/rapm94/feanorfs/releases/tag/v0.4.0",
            "https://github.example/rapm94/feanorfs/releases/tag/v0.4.0",
            "https://github.com.evil.example/rapm94/feanorfs/releases/tag/v0.4.0",
            "https://user@github.com/rapm94/feanorfs/releases/tag/v0.4.0",
            "https://github.com/rapm94/feanorfs/releases/tag/v0.4.1",
            "https://github.com/rapm94/feanorfs/releases/tag/v0.4.0?download=1",
            "https://github.com/rapm94/feanorfs/releases/tag/v0.4.0#files",
        ] {
            assert!(validate_release_url(invalid, "v0.4.0").is_err());
        }
    }

    #[test]
    fn periodic_state_roundtrips_and_throttles() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("update-state.json");
        {
            let result = UpdateCheckResult {
                status: UpdateStatus::UpdateAvailable,
                current_version: "0.7.10".into(),
                latest_version: "0.8.0".into(),
                release_url: "https://github.com/rapm94/feanorfs/releases/tag/v0.8.0".into(),
            };
            save_update_state_at(
                &path,
                &UpdateState {
                    last_check_ms: chrono::Utc::now().timestamp_millis(),
                    status: result.status,
                    current_version: result.current_version.clone(),
                    latest_version: result.latest_version.clone(),
                    release_url: result.release_url.clone(),
                },
            )
            .unwrap();
            let cached = cached_within_throttle_at(&path, "0.7.10")
                .expect("fresh result must come from cache");
            assert_eq!(cached, result);
            assert!(
                cached_within_throttle_at(&path, "0.7.11").is_none(),
                "a binary upgrade must force a release-metadata refresh"
            );

            let state = load_update_state_at(&path).unwrap();
            let mut stale = state.clone();
            stale.last_check_ms -= (PERIODIC_CHECK_INTERVAL.as_millis() as i64) + 1;
            save_update_state_at(&path, &stale).unwrap();
            assert!(cached_within_throttle_at(&path, "0.7.10").is_none());
        }
    }

    #[test]
    fn periodic_state_replaces_existing_file_atomically() {
        let home = tempfile::tempdir().unwrap();
        let state_dir = home.path().join("new-state-directory");
        let path = state_dir.join("update-state.json");
        let mut state = UpdateState {
            last_check_ms: chrono::Utc::now().timestamp_millis(),
            status: UpdateStatus::UpToDate,
            current_version: "0.7.11".into(),
            latest_version: "0.7.11".into(),
            release_url: "https://github.com/rapm94/feanorfs/releases/tag/v0.7.11".into(),
        };
        save_update_state_at(&path, &state).unwrap();
        state.status = UpdateStatus::UpdateAvailable;
        state.latest_version = "0.8.0".into();
        state.release_url = "https://github.com/rapm94/feanorfs/releases/tag/v0.8.0".into();
        save_update_state_at(&path, &state).unwrap();
        assert_eq!(load_update_state_at(&path).unwrap(), state);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn oversized_periodic_state_is_not_written() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("update-state.json");
        let state = UpdateState {
            last_check_ms: chrono::Utc::now().timestamp_millis(),
            status: UpdateStatus::DevelopmentBuild,
            current_version: "x".repeat(MAX_UPDATE_STATE_BYTES),
            latest_version: "0.7.11".into(),
            release_url: "https://github.com/rapm94/feanorfs/releases/tag/v0.7.11".into(),
        };
        assert!(save_update_state_at(&path, &state).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn malformed_periodic_state_is_ignored() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("update-state.json");
        std::fs::write(&path, b"not json").unwrap();
        assert!(cached_within_throttle_at(&path, "0.7.11").is_none());
        assert!(cached_within_throttle_at(&home.path().join("missing.json"), "0.7.11").is_none());

        let invalid = UpdateState {
            last_check_ms: chrono::Utc::now().timestamp_millis(),
            status: UpdateStatus::UpToDate,
            current_version: "0.7.11".into(),
            latest_version: "0.7.11".into(),
            release_url: "https://example.com/not-the-release".into(),
        };
        save_update_state_at(&path, &invalid).unwrap();
        assert!(cached_within_throttle_at(&path, "0.7.11").is_none());

        let mut future = invalid;
        future.release_url = "https://github.com/rapm94/feanorfs/releases/tag/v0.7.11".into();
        future.last_check_ms += 60_000;
        save_update_state_at(&path, &future).unwrap();
        assert!(cached_within_throttle_at(&path, "0.7.11").is_none());
    }
}
