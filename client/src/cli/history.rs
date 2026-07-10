use super::util::output_json;
use feanorfs_client::{load_config, SyncCtx};
use std::path::Path;

pub async fn log(current_dir: &Path, limit: usize, json: bool) -> anyhow::Result<()> {
    let config = load_config(current_dir)?;
    let db = crate::open_client_db(current_dir).await?;
    let api = crate::open_api_client(current_dir, &config).await?;
    let ctx = SyncCtx::from_config(&api, &db, current_dir, &config)?;
    let result = feanorfs_agent_core::history::log(&ctx, limit).await?;
    if json {
        output_json(&result)?;
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp_millis();
    for entry in result.entries {
        let id = &entry.snapshot_id[..8];
        let age = relative_age(now.saturating_sub(entry.created_at_ms));
        let message = entry.message.unwrap_or_default();
        println!(
            "{id} {age} {} {} path(s){}",
            entry.author,
            entry.changed_paths.len(),
            if message.is_empty() {
                String::new()
            } else {
                format!(" — {message}")
            }
        );
    }
    Ok(())
}

pub async fn undo(current_dir: &Path, snapshot_id: &str, json: bool) -> anyhow::Result<()> {
    let config = load_config(current_dir)?;
    let db = crate::open_client_db(current_dir).await?;
    let api = crate::open_api_client(current_dir, &config).await?;
    let ctx = SyncCtx::from_config(&api, &db, current_dir, &config)?;
    let result = feanorfs_agent_core::history::undo(&ctx, snapshot_id).await?;
    if json {
        output_json(&result)?;
    } else {
        println!(
            "Restored snapshot {} as {} ({} path(s) changed).",
            &result.restored_snapshot_id[..8],
            &result.snapshot_id[..8],
            result.changed_paths.len()
        );
    }
    Ok(())
}

fn relative_age(milliseconds: i64) -> String {
    let seconds = milliseconds.max(0) / 1_000;
    if seconds < 60 {
        return format!("{seconds}s ago");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    format!("{}d ago", hours / 24)
}
