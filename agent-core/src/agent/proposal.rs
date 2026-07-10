use anyhow::{Context, Result};
use feanorfs_common::ConcurrentEdit;
use std::path::Path;

use crate::conflict_artifacts::{
    is_binary_content, is_sentinel_content, resolve_artifact, ArtifactRole,
};

pub(super) fn write_proposal_if_clean(
    conflict_dir: &Path,
    edit: &mut ConcurrentEdit,
) -> Result<()> {
    let original = resolve_artifact(conflict_dir, &edit.path, ArtifactRole::Original);
    let local = resolve_artifact(conflict_dir, &edit.path, ArtifactRole::Local);
    let cloud = resolve_artifact(conflict_dir, &edit.path, ArtifactRole::Cloud);
    let local_bytes = std::fs::read(&local).context("missing local artifact for proposal")?;
    let cloud_bytes = std::fs::read(&cloud).context("missing cloud artifact for proposal")?;
    if is_binary_content(&local_bytes) || is_binary_content(&cloud_bytes) {
        return Ok(());
    }
    let original_bytes = if original.is_file() {
        let bytes = std::fs::read(original).context("read original artifact for proposal")?;
        if is_sentinel_content(&bytes) {
            return Ok(());
        }
        bytes
    } else {
        Vec::new()
    };
    let proposed_path = conflict_dir.join(format!("{}.proposed", edit.path));
    if let Some(parent) = proposed_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match diffy::merge(
        String::from_utf8_lossy(&original_bytes).as_ref(),
        String::from_utf8_lossy(&local_bytes).as_ref(),
        String::from_utf8_lossy(&cloud_bytes).as_ref(),
    ) {
        Ok(merged) => {
            std::fs::write(&proposed_path, merged)?;
            edit.proposal_clean = Some(true);
        }
        Err(merged) => {
            std::fs::write(&proposed_path, merged)?;
            edit.proposal_clean = Some(false);
        }
    }
    edit.proposed_file = Some(proposed_path.to_string_lossy().into_owned());
    Ok(())
}
