use anyhow::{ensure, Context as _};
use reqwest::Url;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::util::output_json;

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/rapm94/feanorfs/releases/latest";
const OFFICIAL_RELEASE_PATH_PREFIX: &str = "/rapm94/feanorfs/releases/tag/";
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UpdateStatus {
    UpToDate,
    UpdateAvailable,
    DevelopmentBuild,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UpdateCheckResult {
    status: UpdateStatus,
    current_version: String,
    latest_version: String,
    release_url: String,
}

#[derive(Debug, Deserialize)]
struct ReleaseMetadata {
    tag_name: String,
    html_url: String,
    draft: bool,
    prerelease: bool,
}

pub(crate) async fn run(json: bool) -> anyhow::Result<()> {
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
    if let Some(length) = response.content_length() {
        ensure!(
            length <= MAX_RESPONSE_BYTES as u64,
            "official FeanorFS release response is unexpectedly large"
        );
    }
    let body = response
        .bytes()
        .await
        .context("read official FeanorFS release response")?;
    ensure!(
        body.len() <= MAX_RESPONSE_BYTES,
        "official FeanorFS release response is unexpectedly large"
    );
    let metadata: ReleaseMetadata =
        serde_json::from_slice(&body).context("parse official FeanorFS release response")?;
    let result = evaluate_release(env!("CARGO_PKG_VERSION"), metadata)?;
    if json {
        return output_json(&result);
    }
    render(&result);
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
}
