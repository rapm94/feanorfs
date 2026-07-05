use clap::Subcommand;
use feanorfs_client::{
    conflict_artifacts::{is_binary_content, resolve_artifact, ArtifactRole},
    conflicts, load_config, ApiClient, ClientDb, ResolveKeep, SyncCtx,
};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::util::output_json;

#[derive(Subcommand)]
pub enum ConflictsAction {
    /// List pending conflicts (paths blocked from sync).
    List,
    /// Keep a version to resolve a conflict.
    Keep {
        path: String,
        /// Keep the local version present in this folder.
        #[arg(long, group = "keep_choice")]
        local: bool,
        /// Keep the cloud version downloaded from the mirror.
        #[arg(long, group = "keep_choice")]
        cloud: bool,
        /// Keep both versions (other side renamed to a `conflicted copy` file).
        #[arg(long, group = "keep_choice")]
        both: bool,
        /// Path to a reconciled candidate file (keep with `--file`).
        #[arg(long, group = "keep_choice")]
        file: Option<PathBuf>,
    },
    /// Show unified diff of local vs cloud versions.
    Show {
        path: String,
        /// Open compare view in your editor.
        #[arg(long)]
        open: bool,
    },
    /// Show resolution history
    #[command(hide = true)]
    History,
    /// Open compare view (legacy — prefer `conflicts show --open`)
    #[command(hide = true)]
    Open { path: String },
}

fn parse_keep_flags(
    local: bool,
    cloud: bool,
    both: bool,
    file: Option<PathBuf>,
) -> anyhow::Result<(ResolveKeep, Option<PathBuf>)> {
    let choices = [local, cloud, both, file.is_some()]
        .into_iter()
        .filter(|&c| c)
        .count();
    if choices != 1 {
        anyhow::bail!("specify exactly one of --local, --cloud, --both, or --file");
    }
    if let Some(path) = file {
        return Ok((ResolveKeep::File, Some(path)));
    }
    if local {
        Ok((ResolveKeep::Local, None))
    } else if cloud {
        Ok((ResolveKeep::Cloud, None))
    } else {
        Ok((ResolveKeep::Both, None))
    }
}

pub async fn run(current_dir: &Path, action: ConflictsAction, json: bool) -> anyhow::Result<()> {
    let config = load_config(current_dir)?;
    let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
    let api = ApiClient::from_config(current_dir, &config).await?;
    let ctx = SyncCtx::from_config(&api, &db, current_dir, &config)?;
    match action {
        ConflictsAction::List => {
            let records = db.list_conflict_records().await?;
            if json {
                output_json(&records)?;
            } else if records.is_empty() {
                println!("No paths need attention.");
            } else {
                println!("Paths needing attention:");
                for r in &records {
                    println!("  {} ({:?}) — {}", r.path, r.kind, r.conflict_dir);
                }
            }
        }
        ConflictsAction::Keep {
            path,
            local,
            cloud,
            both,
            file,
        } => {
            let (keep, file_path) = parse_keep_flags(local, cloud, both, file)?;
            conflicts::resolve_conflict(&ctx, &path, keep, file_path.as_deref()).await?;
            if json {
                output_json(&serde_json::json!({ "resolved": path }))?;
            } else {
                println!("Resolved '{path}'. Run 'feanorfs sync' to continue.");
            }
        }
        ConflictsAction::Show { path, open } => {
            if open {
                open_conflict_compare(&db, &path).await?;
            } else {
                show_conflict_diff(&db, &path).await?;
            }
        }
        ConflictsAction::Open { path } => {
            open_conflict_compare(&db, &path).await?;
        }
        ConflictsAction::History => {
            let history = db.list_conflict_resolutions().await?;
            if json {
                output_json(&history)?;
            } else if history.is_empty() {
                println!("No conflict resolutions recorded.");
            } else {
                println!("Conflict resolution history:");
                for r in &history {
                    println!(
                        "  {} — {} via {} ({})",
                        r.path, r.method, r.resolver, r.resolved_at
                    );
                }
            }
        }
    }
    Ok(())
}

async fn show_conflict_diff(db: &ClientDb, path: &str) -> anyhow::Result<()> {
    let record = db
        .get_conflict_record(path)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no pending conflict for {path}"))?;
    let dir = Path::new(&record.conflict_dir);
    let local = resolve_artifact(dir, path, ArtifactRole::Local);
    let cloud = resolve_artifact(dir, path, ArtifactRole::Cloud);
    let local_bytes = std::fs::read(&local).unwrap_or_default();
    let cloud_bytes = std::fs::read(&cloud).unwrap_or_default();
    if is_binary_content(&local_bytes) || is_binary_content(&cloud_bytes) {
        println!(
            "Binary file — local {} bytes vs cloud {} bytes",
            local_bytes.len(),
            cloud_bytes.len()
        );
    } else {
        let local_s = String::from_utf8_lossy(&local_bytes);
        let cloud_s = String::from_utf8_lossy(&cloud_bytes);
        let diff = diffy::create_patch(local_s.as_ref(), cloud_s.as_ref());
        println!("{diff}");
    }
    Ok(())
}

async fn open_conflict_compare(db: &ClientDb, path: &str) -> anyhow::Result<()> {
    let record = db
        .get_conflict_record(path)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no pending conflict for {path}"))?;
    let dir = Path::new(&record.conflict_dir);
    let local = resolve_artifact(dir, path, ArtifactRole::Local);
    let cloud = resolve_artifact(dir, path, ArtifactRole::Cloud);
    if which::which("code").is_ok() {
        let status = Command::new("code")
            .args(["--diff", &local.to_string_lossy(), &cloud.to_string_lossy()])
            .status()?;
        if !status.success() {
            anyhow::bail!("editor exited with {:?}", status.code());
        }
    } else if let Ok(editor) = std::env::var("EDITOR") {
        let status = Command::new(&editor).arg(&local).arg(&cloud).status()?;
        if !status.success() {
            anyhow::bail!("editor exited with {:?}", status.code());
        }
    } else {
        anyhow::bail!("Set EDITOR or install VS Code (`code`) for compare view");
    }
    Ok(())
}
