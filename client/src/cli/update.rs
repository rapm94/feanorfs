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

/// Private atomic visibility: the periodic update-throttle
/// state is replaced via a 0o600 temp file and atomic rename, without a
/// parent-directory sync. Losing the rename to a crash only forces an earlier
/// re-check; the payload is bounded by `MAX_UPDATE_STATE_BYTES` before write.
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
            assets: Vec::new(),
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
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const MIN_BINARY_BYTES: u64 = 1024 * 1024;
const MAX_CHECKSUM_BYTES: usize = 4 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const UPDATE_STAGING_DIR: &str = "update-staging";
const EXECUTABLE_NAME: &str = "feanorfs";

/// Target triple of the running binary, matching cargo-dist artifact names.
fn current_target_triple() -> anyhow::Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        (os, arch) => anyhow::bail!("self-update has no release archive for {os}/{arch}"),
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct UpdateApplyResult {
    pub(crate) applied_version: String,
    pub(crate) previous_version: String,
    pub(crate) replaced_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backup_path: Option<String>,
}

fn is_root_context() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::getuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Homebrew owns binaries under its prefix; replacing them behind brew
/// corrupts its linkage records.
fn is_homebrew_managed(exe: &Path) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    exe.starts_with("/opt/homebrew/")
        || exe.starts_with("/usr/local/Cellar/")
        || exe.starts_with("/usr/local/Caskroom/")
}

/// Cargo-installed binaries belong to `cargo install feanorfs`.
fn is_cargo_managed(exe: &Path) -> bool {
    // The home directory is literally named `.cargo`; match the separator
    // spelling per platform instead of component names.
    let text = exe.to_string_lossy();
    text.contains("/.cargo/bin/") || text.contains("\\\\.cargo\\bin\\")
}

fn apply_guards(exe: &Path) -> anyhow::Result<()> {
    if is_root_context() {
        anyhow::bail!(
            "refusing to self-update as root; package-manager contexts must not mutate user installs"
        );
    }
    if is_homebrew_managed(exe) {
        anyhow::bail!("this copy is managed by Homebrew; run `brew upgrade feanorfs` instead");
    }
    if is_cargo_managed(exe) {
        anyhow::bail!("this copy was installed by cargo; run `cargo install feanorfs` instead");
    }
    Ok(())
}

fn select_assets(
    assets: &[ReleaseAsset],
    triple: &str,
    version: &str,
) -> anyhow::Result<(String, String)> {
    let archive = format!("feanorfs-client-{triple}.tar.xz");
    let checksum = format!("{archive}.sha256");
    let mut found: Vec<&ReleaseAsset> = assets.iter().filter(|a| a.name == archive).collect();
    let archive_asset = found
        .pop()
        .context("official release has no archive for this platform")?;
    let checksum_asset = assets
        .iter()
        .find(|a| a.name == checksum)
        .context("official release has no checksum for this platform")?;
    let expected_prefix =
        format!("https://github.com/rapm94/feanorfs/releases/download/v{version}/");
    for asset in [archive_asset, checksum_asset] {
        ensure!(
            asset.browser_download_url == format!("{expected_prefix}{}", asset.name),
            "release asset URL does not match the official download location"
        );
    }
    Ok((
        archive_asset.browser_download_url.clone(),
        checksum_asset.browser_download_url.clone(),
    ))
}

/// Accepts `<hex>` or `<hex>  <filename>`; rejects anything else so a
/// malformed or hostile checksum file cannot silently pass verification.
fn parse_checksum_file(bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?;
    let token = text.split_whitespace().next()?;
    let hex: String = token.to_ascii_lowercase();
    if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return None;
    }
    Some(hex)
}

fn verify_sha256(path: &Path, expected_hex: &str) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};
    let file = std::fs::File::open(path).context("open downloaded archive")?;
    let mut reader = std::io::BufReader::new(file).take(MAX_ARCHIVE_BYTES);
    let mut hasher = Sha256::new();
    std::io::copy(&mut reader, &mut hasher).context("hash downloaded archive")?;
    let digest = format!("{:x}", hasher.finalize());
    ensure!(
        digest == expected_hex,
        "downloaded archive failed SHA-256 verification"
    );
    Ok(())
}

async fn download_bounded(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    cap: u64,
) -> anyhow::Result<()> {
    let mut response = client
        .get(url)
        .send()
        .await
        .context("download official release asset")?
        .error_for_status()
        .context("official release asset returned an error")?;
    if let Some(length) = response.content_length() {
        ensure!(
            length <= cap,
            "official release asset is unexpectedly large"
        );
    }
    let mut file = std::fs::File::create(dest).context("create staged asset file")?;
    let mut written: u64 = 0;
    while let Some(chunk) = response
        .chunk()
        .await
        .context("stream official release asset")?
    {
        written += chunk.len() as u64;
        ensure!(
            written <= cap,
            "official release asset exceeded its size bound"
        );
        file.write_all(&chunk).context("write staged asset file")?;
    }
    file.sync_all().ok();
    Ok(())
}

fn extract_archive(archive: &Path, stage_dir: &Path) -> anyhow::Result<PathBuf> {
    let status = std::process::Command::new("tar")
        .arg("-xJf")
        .arg(archive)
        .arg("-C")
        .arg(stage_dir)
        .status()
        .context("run system tar to unpack the verified archive")?;
    ensure!(status.success(), "system tar failed to unpack the archive");
    let exe_name = if cfg!(windows) {
        "feanorfs.exe"
    } else {
        EXECUTABLE_NAME
    };
    let extracted = stage_dir.join(exe_name);
    let metadata =
        std::fs::metadata(&extracted).context("unpacked archive lacks the FeanorFS executable")?;
    ensure!(
        metadata.is_file(),
        "unpacked FeanorFS executable is not a regular file"
    );
    let size = metadata.len();
    ensure!(
        (MIN_BINARY_BYTES..=MAX_ARCHIVE_BYTES).contains(&size),
        "unpacked FeanorFS executable has an implausible size"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&extracted, std::fs::Permissions::from_mode(0o755))
            .context("restore executable permissions")?;
    }
    Ok(extracted)
}

/// Replaces the running executable with the staged one. Unix renames over a
/// running image freely; Windows refuses deletion but allows renaming the
/// live image aside first, keeping a `.old-<pid>` backup for inspection.
fn replace_current_exe(staged: &Path) -> anyhow::Result<(PathBuf, Option<PathBuf>)> {
    let current = std::env::current_exe().context("locate the running executable")?;
    let parent = current
        .parent()
        .context("running executable has no parent directory")?
        .to_path_buf();

    {
        let file = std::fs::File::open(staged).context("reopen staged executable")?;
        file.sync_all().context("sync staged executable bytes")?;
    }

    #[cfg(windows)]
    let backup_path = {
        let file_name = current
            .file_name()
            .context("running executable has no file name")?
            .to_string_lossy()
            .into_owned();
        parent.join(format!("{file_name}.old-{}", std::process::id()))
    };
    #[cfg(windows)]
    {
        let _ = std::fs::remove_file(&backup_path);
        std::fs::rename(&current, &backup_path).context("move the running executable aside")?;
        if let Err(error) = std::fs::rename(staged, &current) {
            let _ = std::fs::rename(&backup_path, &current);
            return Err(anyhow::anyhow!(error)).context("install the new executable");
        }
        return Ok((current, Some(backup_path)));
    }
    #[cfg(unix)]
    {
        std::fs::rename(staged, &current).context("replace the running executable")?;
        if let Ok(dir) = std::fs::File::open(&parent) {
            dir.sync_all().ok();
        }
        Ok((current, None))
    }
}

fn sync_staging_parent(stage_dir: &Path) {
    if let Some(parent) = stage_dir.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            dir.sync_all().ok();
        }
    }
}

fn cleanup_staging(root: &Path) {
    let _ = std::fs::remove_dir_all(root);
}

fn staging_dir(version: &str) -> anyhow::Result<PathBuf> {
    let base = feanorfs_agent_core::global_state_root()?.join(UPDATE_STAGING_DIR);
    let dir = base.join(format!("v{version}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).context("create update staging directory")?;
    Ok(dir)
}

/// Applies the available stable release: verifies then replaces this
/// executable in place. Never runs implicitly — the caller must pass
/// `--apply`, which documents user consent on both surfaces.
pub(crate) async fn run_apply(json: bool) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locate the running executable")?;
    apply_guards(&exe)?;
    let triple = current_target_triple()?;

    // Fresh authoritative check: never apply from the throttle cache.
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .user_agent(format!("feanorfs/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build secure release client")?;
    let response = client
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .context("check the official FeanorFS release")?
        .error_for_status()
        .context("official FeanorFS release service returned an error")?;
    let body = read_bounded_body(response).await?;
    let metadata: ReleaseMetadata =
        serde_json::from_slice(&body).context("parse official FeanorFS release")?;
    let result = evaluate_release(env!("CARGO_PKG_VERSION"), metadata.clone_for_check())?;
    record_periodic_state(&result);
    ensure!(
        result.status == UpdateStatus::UpdateAvailable,
        "no update to apply: {}",
        match result.status {
            UpdateStatus::UpToDate => "this build already matches the latest stable release",
            UpdateStatus::DevelopmentBuild => {
                "this build is newer than the latest stable release"
            }
            UpdateStatus::UpdateAvailable => unreachable!(),
        }
    );

    let (archive_url, checksum_url) =
        select_assets(&metadata.assets, triple, &result.latest_version)?;

    let stage_dir = staging_dir(&result.latest_version)?;
    let apply_result = async {
        let download_client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::limited(5))
            .timeout(DOWNLOAD_TIMEOUT)
            .user_agent(format!("feanorfs/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build secure download client")?;

        let archive_path = stage_dir.join(format!("feanorfs-client-{triple}.tar.xz"));
        let checksum_path = stage_dir.join(format!("feanorfs-client-{triple}.tar.xz.sha256"));
        download_bounded(
            &download_client,
            &checksum_url,
            &checksum_path,
            MAX_CHECKSUM_BYTES as u64,
        )
        .await
        .context("download the release checksum")?;
        let checksum_bytes = std::fs::read(&checksum_path).context("read the release checksum")?;
        let expected_hex = parse_checksum_file(&checksum_bytes)
            .context("the release checksum file is not in the expected format")?;
        ensure!(
            checksum_bytes.len() <= MAX_CHECKSUM_BYTES,
            "release checksum file exceeds its size bound"
        );
        download_bounded(
            &download_client,
            &archive_url,
            &archive_path,
            MAX_ARCHIVE_BYTES,
        )
        .await
        .context("download the official release archive")?;
        verify_sha256(&archive_path, &expected_hex)?;

        let extracted = extract_archive(&archive_path, &stage_dir)?;
        let (replaced, backup) = replace_current_exe(&extracted)?;
        sync_staging_parent(&stage_dir);

        // Refresh the throttle state so every surface reports the new build
        // instead of re-offering the version that was just installed.
        let refreshed = evaluate_release(
            &result.latest_version,
            ReleaseMetadata {
                tag_name: format!("v{}", result.latest_version),
                html_url: result.release_url.clone(),
                draft: false,
                prerelease: false,
                assets: Vec::new(),
            },
        )?;
        save_update_state(&UpdateState {
            last_check_ms: chrono::Utc::now().timestamp_millis(),
            status: refreshed.status,
            current_version: refreshed.current_version.clone(),
            latest_version: refreshed.latest_version.clone(),
            release_url: refreshed.release_url.clone(),
        });

        Ok::<UpdateApplyResult, anyhow::Error>(UpdateApplyResult {
            applied_version: result.latest_version.clone(),
            previous_version: result.current_version.clone(),
            replaced_path: replaced.to_string_lossy().into_owned(),
            backup_path: backup.map(|p| p.to_string_lossy().into_owned()),
        })
    }
    .await;

    let outcome = match apply_result {
        Ok(outcome) => outcome,
        Err(error) => {
            cleanup_staging(&stage_dir);
            return Err(error);
        }
    };
    cleanup_staging(&stage_dir);

    render_apply_result(&outcome, json);
    Ok(())
}

// Small shim so the apply path can reuse evaluate_release without cloning
// asset lists into the check result.
impl ReleaseMetadata {
    fn clone_for_check(&self) -> ReleaseMetadata {
        ReleaseMetadata {
            tag_name: self.tag_name.clone(),
            html_url: self.html_url.clone(),
            draft: self.draft,
            prerelease: self.prerelease,
            assets: Vec::new(),
        }
    }
}

async fn read_bounded_body(response: reqwest::Response) -> anyhow::Result<Vec<u8>> {
    let length = response.content_length();
    if let Some(length) = length {
        ensure!(
            length <= MAX_RESPONSE_BYTES as u64,
            "official FeanorFS release response is unexpectedly large"
        );
    }
    let mut body = Vec::new();
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .context("read official FeanorFS release response")?
    {
        ensure!(
            chunk.len() <= MAX_RESPONSE_BYTES.saturating_sub(body.len()),
            "official FeanorFS release response is unexpectedly large"
        );
        body.extend_from_slice(chunk.as_ref());
    }
    Ok(body)
}

fn render_apply_result(result: &UpdateApplyResult, json: bool) {
    if json {
        let _ = output_json(result);
        return;
    }
    println!(
        "FeanorFS {} installed (previously {}).",
        result.applied_version, result.previous_version
    );
    println!("Updated file: {}", result.replaced_path);
    if let Some(backup) = &result.backup_path {
        println!("Previous binary kept at: {backup}");
    }
    println!(
        "Run `feanorfs service refresh-installation` so supervised services pick up the new build."
    );
    println!("Quit and reopen the tray if it does not restart by itself.");
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
            assets: Vec::new(),
        }
    }

    #[test]
    fn apply_selects_platform_assets_and_validates_official_urls() {
        let triple = current_target_triple().unwrap();
        let assets = vec![
            ReleaseAsset {
                name: format!("feanorfs-client-{triple}.tar.xz"),
                browser_download_url: format!(
                    "https://github.com/rapm94/feanorfs/releases/download/v1.2.3/feanorfs-client-{triple}.tar.xz"
                ),
            },
            ReleaseAsset {
                name: format!("feanorfs-client-{triple}.tar.xz.sha256"),
                browser_download_url: format!(
                    "https://github.com/rapm94/feanorfs/releases/download/v1.2.3/feanorfs-client-{triple}.tar.xz.sha256"
                ),
            },
            ReleaseAsset {
                name: "feanorfs-client-other.tar.xz".into(),
                browser_download_url:
                    "https://github.com/rapm94/feanorfs/releases/download/v1.2.3/feanorfs-client-other.tar.xz"
                        .into(),
            },
        ];
        let (archive_url, checksum_url) = select_assets(&assets, triple, "1.2.3").unwrap();
        assert!(archive_url.ends_with(&format!("feanorfs-client-{triple}.tar.xz")));
        assert!(checksum_url.ends_with(".sha256"));

        // A tampered download host must be refused.
        let hostile = vec![ReleaseAsset {
            name: format!("feanorfs-client-{triple}.tar.xz"),
            browser_download_url: format!(
                "https://mirror.example/rapm94/feanorfs/releases/download/v1.2.3/feanorfs-client-{triple}.tar.xz"
            ),
        }];
        assert!(select_assets(&hostile, triple, "1.2.3").is_err());
        // A release without this platform's archive must be refused.
        assert!(select_assets(&[], triple, "1.2.3").is_err());
    }

    #[test]
    fn checksum_parser_accepts_only_strict_hex_digests() {
        let digest = "a".repeat(64);
        assert_eq!(
            parse_checksum_file(digest.as_bytes()).as_deref(),
            Some(digest.as_str())
        );
        let with_name = format!("{digest}  feanorfs-client-x.tar.xz\n");
        assert_eq!(
            parse_checksum_file(with_name.as_bytes()).as_deref(),
            Some(digest.as_str())
        );
        assert!(parse_checksum_file(b"nothex").is_none());
        assert!(parse_checksum_file(b"").is_none());
        let short = "a".repeat(63);
        assert!(parse_checksum_file(short.as_bytes()).is_none());
    }

    #[test]
    fn package_manager_guards_detect_managed_locations() {
        let cargo_managed = Path::new("/Users/dev/.cargo/bin/feanorfs");
        assert!(is_cargo_managed(cargo_managed));
        assert!(!is_cargo_managed(Path::new(
            "/Applications/FeanorFS/feanorfs"
        )));
        #[cfg(target_os = "macos")]
        {
            assert!(is_homebrew_managed(Path::new("/opt/homebrew/bin/feanorfs")));
            assert!(is_homebrew_managed(Path::new(
                "/usr/local/Cellar/feanorfs/0.9.0/bin/feanorfs"
            )));
            assert!(!is_homebrew_managed(Path::new(
                "/Applications/FeanorFS.app/Contents/MacOS/feanorfs"
            )));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(!is_homebrew_managed(Path::new(
                "/opt/homebrew/bin/feanorfs"
            )));
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
