//! CLI attention and error projection for the runner worker.

use anyhow::{ensure, Context as _};
use feanorfs_agent_core::{RunnerAttention, RunnerPhase, RunnerStore};
use std::path::{Path, PathBuf};

pub(super) fn require_canonical_workspace(path: &Path) -> anyhow::Result<PathBuf> {
    ensure!(path.is_absolute(), "runner workspace path must be absolute");
    let canonical = path
        .canonicalize()
        .with_context(|| format!("canonicalize runner workspace {}", path.display()))?;
    ensure!(
        canonical == path,
        "runner workspace path must already be canonical: {}",
        path.display()
    );
    ensure!(canonical.is_dir(), "runner workspace must be a directory");
    Ok(canonical)
}

pub(super) fn report_attention(store: &RunnerStore) -> anyhow::Result<bool> {
    let status = store.status()?;
    if status.phase != RunnerPhase::NeedsAttention {
        return Ok(false);
    }
    match status.attention {
        Some(RunnerAttention::CursorReset) => {
            tracing::error!("agent runner stopped: inbox cursor needs attention")
        }
        Some(RunnerAttention::PendingOverflow) => {
            tracing::error!("agent runner stopped: pending queue needs attention")
        }
        Some(RunnerAttention::AmbiguousExecution) => {
            tracing::error!("agent runner stopped: a prior execution is ambiguous")
        }
        Some(RunnerAttention::DeliveryUnknown) => {
            tracing::error!("agent runner stopped: terminal delivery is unknown")
        }
        Some(RunnerAttention::PreparationFailed) => {
            tracing::error!("agent runner stopped: refresh preparation failed")
        }
        None => tracing::error!("agent runner stopped: runner state needs attention"),
    }
    Ok(true)
}
