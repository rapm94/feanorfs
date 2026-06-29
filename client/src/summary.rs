use crate::local::ClientDb;
use anyhow::Result;
use feanorfs_common::FileState;
use std::path::Path;

#[derive(Debug, Default, serde::Serialize)]
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
            lines.push(format!("  + {}", p));
        }
    }
    if !s.files_modified.is_empty() {
        lines.push("Modified:".to_string());
        for p in &s.files_modified {
            lines.push(format!("  ~ {}", p));
        }
    }
    if !s.files_deleted.is_empty() {
        lines.push("Deleted:".to_string());
        for p in &s.files_deleted {
            lines.push(format!("  - {}", p));
        }
    }
    lines.join("\n")
}
