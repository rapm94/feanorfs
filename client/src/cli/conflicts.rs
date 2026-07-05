use feanorfs_client::{conflicts, load_config, ApiClient, ClientDb, ResolveKeep};
use std::path::Path;

use super::util::output_json;
use super::ConflictsAction;

pub async fn run(current_dir: &Path, action: ConflictsAction, json: bool) -> anyhow::Result<()> {
    let config = load_config(current_dir)?;
    let db = ClientDb::new(current_dir.join(".feanorfs")).await?;
    let api = ApiClient::new(&config.server_url, config.server_password.as_deref());
    match action {
        ConflictsAction::List => {
            let records = db.list_conflict_records().await?;
            if json {
                output_json(&records)?;
            } else if records.is_empty() {
                println!("No pending conflicts.");
            } else {
                println!("Pending conflicts:");
                for r in &records {
                    println!("  {} ({:?}) — {}", r.path, r.kind, r.conflict_dir);
                }
            }
        }
        ConflictsAction::Resolve { path, keep } => {
            let keep = match keep.as_str() {
                "ours" => ResolveKeep::Ours,
                "theirs" => ResolveKeep::Theirs,
                _ => ResolveKeep::Both,
            };
            conflicts::resolve_conflict(
                current_dir,
                &api,
                &db,
                &config.workspace_id,
                &path,
                keep,
                config.encryption_password.as_deref(),
            )
            .await?;
            if json {
                output_json(&serde_json::json!({ "resolved": path }))?;
            } else {
                println!(
                    "Resolved conflict for '{}'. Run 'feanorfs sync' to apply.",
                    path
                );
            }
        }
    }
    Ok(())
}
