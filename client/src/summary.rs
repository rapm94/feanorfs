use crate::local::ClientDb;
use anyhow::Result;
use feanorfs_common::FileState;
use std::path::Path;

#[derive(Debug, Default, PartialEq, serde::Serialize)]
pub struct SummaryResult {
    pub files_added: Vec<String>,
    pub files_modified: Vec<String>,
    pub files_deleted: Vec<String>,
}

/// Compares the on-disk workspace against the last recorded session state
/// (stored in `last_session` table). Returns paths grouped by change kind.
/// FeanorFS itself does not produce human-readable summaries — the caller
/// (or `--summarize` flag shelling out to `FEANORFS_SUMMARY_CMD`) decides
/// how to render this into prose.
pub async fn diff_since_last_session(
    base: &Path,
    db: &ClientDb,
    password: Option<&str>,
) -> Result<SummaryResult> {
    let current = crate::local::scan_local_directory(base, db, password).await?;
    let previous_str = db.get_session_key("last_scan").await?;
    let previous: std::collections::HashMap<String, FileState> = match previous_str {
        Some(s) => match serde_json::from_str(&s) {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(
                    "Failed to parse last_scan session state: {}. Treating as empty.",
                    e
                );
                std::collections::HashMap::new()
            }
        },
        None => std::collections::HashMap::new(),
    };

    let mut result = SummaryResult::default();
    for (path, file) in &current {
        match previous.get(path) {
            None => {
                if !file.deleted {
                    result.files_added.push(path.clone());
                }
            }
            Some(prev) => {
                if file.deleted && !prev.deleted {
                    result.files_deleted.push(path.clone());
                } else if !file.deleted && file.hash != prev.hash {
                    result.files_modified.push(path.clone());
                }
            }
        }
    }
    for (path, prev) in &previous {
        if prev.deleted {
            continue;
        }
        if !current.contains_key(path) {
            result.files_deleted.push(path.clone());
        }
    }

    Ok(result)
}

/// Persist the current state as the next session's "previous" snapshot.
pub async fn commit_session_marker(
    base: &Path,
    db: &ClientDb,
    password: Option<&str>,
) -> Result<()> {
    let current = crate::local::scan_local_directory(base, db, password).await?;
    let serialized = serde_json::to_string(&current)?;
    db.set_session_key("last_scan", &serialized).await?;
    Ok(())
}

/// Shell out to `FEANORFS_SUMMARY_CMD` (default `feanorfs-llm`), feeding it
/// the SummaryResult as JSON on stdin and returning whatever prose the
/// command prints on stdout. If the binary is not on PATH, falls back to
/// listing the changed paths — preserving zero-knowledge guarantees by
/// never shipping file contents to a remote LLM unmodified.
pub fn render_via_summary_tool(summary: &SummaryResult) -> Result<String> {
    let cmd = std::env::var("FEANORFS_SUMMARY_CMD").unwrap_or_else(|_| "feanorfs-llm".to_string());
    if which::which(&cmd).is_err() {
        return Ok(render_plain(summary));
    }
    let json = serde_json::to_string(summary)?;
    let output = std::process::Command::new(&cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(json.as_bytes())?;
            }
            child.wait_with_output()
        })?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn render_plain(s: &SummaryResult) -> String {
    let mut lines = Vec::new();
    if !s.files_added.is_empty() {
        lines.push("Added:".to_string());
        for p in &s.files_added {
            lines.push(format!("  + {p}"));
        }
    }
    if !s.files_modified.is_empty() {
        lines.push("Modified:".to_string());
        for p in &s.files_modified {
            lines.push(format!("  ~ {p}"));
        }
    }
    if !s.files_deleted.is_empty() {
        lines.push("Deleted:".to_string());
        for p in &s.files_deleted {
            lines.push(format!("  - {p}"));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{commit_session_marker, diff_since_last_session, SummaryResult};
    use crate::local::ClientDb;
    use std::path::Path;
    use tempfile::TempDir;

    const PASSWORD: &str = "summary-test-password";

    async fn workspace_with_db() -> (TempDir, ClientDb) {
        let dir = TempDir::new().unwrap();
        let db = ClientDb::new(dir.path().join(".feanorfs")).await.unwrap();
        (dir, db)
    }

    async fn write_file(base: &Path, rel: &str, content: &str) {
        let path = base.join(rel);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.unwrap();
        }
        tokio::fs::write(path, content).await.unwrap();
    }

    #[tokio::test]
    async fn diff_detects_added_modified_and_deleted_files() {
        let (dir, db) = workspace_with_db().await;
        let base = dir.path();

        write_file(base, "alpha.txt", "v1").await;
        write_file(base, "beta.txt", "stable").await;
        commit_session_marker(base, &db, Some(PASSWORD))
            .await
            .unwrap();

        write_file(base, "alpha.txt", "v2").await;
        write_file(base, "gamma.txt", "new").await;
        tokio::fs::remove_file(base.join("beta.txt")).await.unwrap();

        let diff = diff_since_last_session(base, &db, Some(PASSWORD))
            .await
            .unwrap();

        assert_eq!(diff.files_added, vec!["gamma.txt".to_string()]);
        assert_eq!(diff.files_modified, vec!["alpha.txt".to_string()]);
        assert_eq!(diff.files_deleted, vec!["beta.txt".to_string()]);
    }

    #[tokio::test]
    async fn diff_with_no_prior_session_treats_all_files_as_added() {
        let (dir, db) = workspace_with_db().await;
        write_file(dir.path(), "only.txt", "hello").await;

        let diff = diff_since_last_session(dir.path(), &db, Some(PASSWORD))
            .await
            .unwrap();

        assert_eq!(
            diff,
            SummaryResult {
                files_added: vec!["only.txt".to_string()],
                files_modified: vec![],
                files_deleted: vec![],
            }
        );
    }
}
