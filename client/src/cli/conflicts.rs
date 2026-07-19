use anyhow::Context as _;
use clap::Subcommand;
use feanorfs_client::{
    build_conflict_show,
    conflict_artifacts::{is_binary_content, resolve_artifact, ArtifactRole},
    conflicts, invalidate_agent_cache, load_config, ClientDb, ConflictKeepResult, ConflictRecord,
    ConflictResolution, ResolveKeep, SyncCtx,
};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fmt::Write as _, io::Write as _};

use super::util::output_json;

const MAX_CONFLICT_OUTPUT: usize = 20;
const MAX_DIFF_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Subcommand)]
pub enum ConflictsAction {
    /// List pending conflicts (paths blocked from sync).
    List,
    /// Keep a version to resolve a conflict.
    Keep {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        path: Option<String>,
        /// Apply the selected choice to every pending conflict.
        #[arg(long, conflicts_with = "path")]
        all: bool,
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
    let db = crate::open_client_db(current_dir).await?;
    let api = crate::open_api_client(current_dir, &config).await?;
    let ctx = SyncCtx::from_config(&api, &db, current_dir, &config)?;
    match action {
        ConflictsAction::List => {
            let records = db.list_conflict_records().await?;
            if json {
                output_json(&records)?;
            } else if records.is_empty() {
                println!("No paths need attention.");
            } else {
                write_stdout(&render_conflict_list(&records)?)?;
            }
        }
        ConflictsAction::Keep {
            path,
            all,
            local,
            cloud,
            both,
            file,
        } => {
            let (keep, file_path) = parse_keep_flags(local, cloud, both, file)?;
            if all {
                let paths = match keep {
                    ResolveKeep::Local => conflicts::resolve_all_local_conflicts(&ctx).await?,
                    ResolveKeep::Cloud => conflicts::resolve_all_cloud_conflicts(&ctx).await?,
                    _ => anyhow::bail!("--all requires --local or --cloud"),
                };
                invalidate_agent_cache(current_dir);
                if json {
                    let results: Vec<ConflictKeepResult> = paths
                        .into_iter()
                        .map(|resolved| ConflictKeepResult { resolved })
                        .collect();
                    output_json(&results)?;
                } else {
                    println!(
                        "Resolved {} conflict(s) with the {} versions. Run 'feanorfs sync' to continue.",
                        paths.len(),
                        if keep == ResolveKeep::Local { "local" } else { "mirror" }
                    );
                }
            } else {
                let path = path.context("conflict path is required unless --all is used")?;
                conflicts::resolve_conflict(&ctx, &path, keep, file_path.as_deref()).await?;
                invalidate_agent_cache(current_dir);
                if json {
                    output_json(&ConflictKeepResult { resolved: path })?;
                } else {
                    println!("Resolved '{path}'. Run 'feanorfs sync' to continue.");
                }
            }
        }
        ConflictsAction::Show { path, open } => {
            if open {
                open_conflict_compare(&db, &path).await?;
            } else if json {
                let result = build_conflict_show(&db, &path).await?;
                output_json(&result)?;
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
                write_stdout(&render_conflict_history(&history)?)?;
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
        write_stdout(&format!(
            "Binary file — local {} bytes vs cloud {} bytes\n",
            local_bytes.len(),
            cloud_bytes.len()
        ))?;
    } else {
        write_stdout(&render_text_diff(&local_bytes, &cloud_bytes))?;
    }
    Ok(())
}

fn render_conflict_list(records: &[ConflictRecord]) -> anyhow::Result<String> {
    let mut output = String::from("Paths needing attention:\n");
    for record in records.iter().take(MAX_CONFLICT_OUTPUT) {
        writeln!(
            output,
            "  {} ({:?}) — {}",
            record.path, record.kind, record.conflict_dir
        )?;
    }
    if records.len() > MAX_CONFLICT_OUTPUT {
        writeln!(
            output,
            "  … and {} more (use `feanorfs --json conflicts` for automation)",
            records.len() - MAX_CONFLICT_OUTPUT
        )?;
    }
    Ok(output)
}

fn render_conflict_history(records: &[ConflictResolution]) -> anyhow::Result<String> {
    let mut output = String::from("Conflict resolution history:\n");
    for record in records.iter().take(MAX_CONFLICT_OUTPUT) {
        writeln!(
            output,
            "  {} — {} via {} ({})",
            record.path, record.method, record.resolver, record.resolved_at
        )?;
    }
    if records.len() > MAX_CONFLICT_OUTPUT {
        writeln!(
            output,
            "  … and {} more",
            records.len() - MAX_CONFLICT_OUTPUT
        )?;
    }
    Ok(output)
}

fn render_text_diff(local_bytes: &[u8], cloud_bytes: &[u8]) -> String {
    let local = String::from_utf8_lossy(local_bytes);
    let cloud = String::from_utf8_lossy(cloud_bytes);
    let rendered = diffy::create_patch(local.as_ref(), cloud.as_ref()).to_string();
    if rendered.len() <= MAX_DIFF_OUTPUT_BYTES {
        return format!("{rendered}\n");
    }
    let boundary = rendered
        .char_indices()
        .take_while(|(index, _)| *index <= MAX_DIFF_OUTPUT_BYTES)
        .map(|(index, _)| index)
        .last()
        .unwrap_or(0);
    format!(
        "{}\n… diff truncated; use `feanorfs conflicts show --open` for the full comparison\n",
        &rendered[..boundary]
    )
}

fn write_stdout(output: &str) -> anyhow::Result<()> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    match stdout.write_all(output.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.into()),
    }
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
        let mut parts = editor.split_whitespace();
        let bin = parts.next().unwrap_or("vi");
        let bin_args: Vec<&str> = parts.collect();
        let status = Command::new(bin)
            .args(&bin_args)
            .arg(&local)
            .arg(&cloud)
            .status()?;
        if !status.success() {
            anyhow::bail!("editor exited with {:?}", status.code());
        }
    } else {
        anyhow::bail!("Set EDITOR or install VS Code (`code`) for compare view");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use feanorfs_client::ConflictKind;

    #[test]
    fn human_conflict_list_and_history_are_bounded() {
        let conflicts: Vec<_> = (0..25)
            .map(|index| ConflictRecord {
                path: format!("path-{index:02}.txt"),
                kind: ConflictKind::EditEdit,
                conflict_dir: format!("/conflicts/{index}"),
                opened_at: index,
                status: "open".into(),
            })
            .collect();
        let list = render_conflict_list(&conflicts).unwrap();
        assert!(list.contains("path-19.txt"));
        assert!(!list.contains("path-20.txt"));
        assert!(list.contains("… and 5 more"));

        let history: Vec<_> = (0..25)
            .map(|index| ConflictResolution {
                path: format!("resolved-{index:02}.txt"),
                method: "local".into(),
                source_file_hash: None,
                resolved_at: index,
                resolver: "test".into(),
            })
            .collect();
        let history = render_conflict_history(&history).unwrap();
        assert!(history.contains("resolved-19.txt"));
        assert!(!history.contains("resolved-20.txt"));
        assert!(history.contains("… and 5 more"));
    }

    #[test]
    fn human_diff_is_utf8_safe_and_bounded() {
        let local = format!("{}\n", "é".repeat(40_000));
        let cloud = format!("{}\n", "ø".repeat(40_000));
        let rendered = render_text_diff(local.as_bytes(), cloud.as_bytes());
        assert!(rendered.is_char_boundary(rendered.len()));
        assert!(rendered.contains("diff truncated"));
        assert!(rendered.len() <= MAX_DIFF_OUTPUT_BYTES + 128);
    }
}
