use anyhow::Result;
use std::path::{Path, PathBuf};
use tokio::fs;

/// Write `content` to `base_path/rel` atomically via a temp file under
/// `.feanorfs/tmp/` and `rename` (same-filesystem rename is atomic).
pub async fn atomic_write(base_path: &Path, rel: &str, content: &[u8]) -> Result<()> {
    let dest = base_path.join(rel);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).await?;
    }

    let tmp_dir = base_path.join(".feanorfs/tmp");
    fs::create_dir_all(&tmp_dir).await?;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_path: PathBuf = tmp_dir.join(format!("{stamp}-{}", std::process::id()));

    fs::write(&tmp_path, content).await?;
    match fs::rename(&tmp_path, &dest).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp_path).await;
            Err(e.into())
        }
    }
}
