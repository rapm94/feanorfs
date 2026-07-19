//! Global, project-litter-free workspace state layout.

use crate::state::LocalStateV1;
use anyhow::{bail, Context as _, Result};
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const LEGACY_STATE_DIR: &str = ".feanorfs";
const LEGACY_IGNORE_FILE: &str = ".feanorfsignore";
const DEFAULT_RETENTION_DAYS: u64 = 30;
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(60 * 60);
const TEMP_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub fn global_state_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("FEANORFS_HOME") {
        return Ok(PathBuf::from(root));
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .context("HOME or USERPROFILE environment variable not set")?;
    Ok(PathBuf::from(home).join(".feanorfs"))
}

fn canonical_workspace(workspace: &Path) -> Result<PathBuf> {
    match fs::canonicalize(workspace) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if workspace.is_absolute() {
                Ok(workspace.to_path_buf())
            } else {
                Ok(std::env::current_dir()?.join(workspace))
            }
        }
        Err(error) => Err(error).context("resolve workspace path"),
    }
}

pub fn workspace_state_id(workspace: &Path) -> Result<String> {
    let canonical = canonical_workspace(workspace)?;
    let mut hasher = blake3::Hasher::new_derive_key("feanorfs global workspace state v1");
    hasher.update(canonical.to_string_lossy().as_bytes());
    Ok(hasher.finalize().to_hex().to_string())
}

pub fn workspace_state_path(workspace: &Path) -> Result<PathBuf> {
    workspace_state_path_in(workspace, &global_state_root()?)
}

fn workspace_state_path_in(workspace: &Path, root: &Path) -> Result<PathBuf> {
    let workspaces = root.join("workspaces");
    let preferred = workspaces.join(workspace_state_id(workspace)?);
    if preferred.exists() {
        return Ok(preferred);
    }
    let Some(identity) = workspace_identity(workspace)? else {
        return Ok(preferred);
    };
    let entries = match fs::read_dir(&workspaces) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(preferred),
        Err(error) => return Err(error).context("search global workspace registry"),
    };
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let candidate = entry.path();
        if fs::read_to_string(candidate.join("identity"))
            .ok()
            .is_some_and(|stored| stored.trim() == identity.as_str())
        {
            return Ok(candidate);
        }
    }
    Ok(preferred)
}

fn workspace_identity(workspace: &Path) -> Result<Option<String>> {
    let metadata = match fs::metadata(workspace) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read workspace identity"),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let created = metadata
            .created()
            .ok()
            .and_then(|created| created.duration_since(UNIX_EPOCH).ok());
        Ok(created.map(|created| {
            format!(
                "unix:{}:{}:{}:{}",
                metadata.dev(),
                metadata.ino(),
                created.as_secs(),
                created.subsec_nanos()
            )
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok(None)
    }
}

pub fn workspace_is_configured(workspace: &Path) -> bool {
    workspace_state_path(workspace).is_ok_and(|state| state.join("config.json").is_file())
        || workspace
            .join(LEGACY_STATE_DIR)
            .join("config.json")
            .is_file()
}

/// Return the global workspace state directory, migrating the legacy in-project
/// directory and ignore policy on first access.
pub fn ensure_workspace_state(workspace: &Path) -> Result<PathBuf> {
    ensure_workspace_state_in(workspace, &global_state_root()?)
}

fn ensure_workspace_state_in(workspace: &Path, root: &Path) -> Result<PathBuf> {
    let state = workspace_state_path_in(workspace, root)?;
    let workspaces = state
        .parent()
        .context("global workspace state has no parent")?;
    private_dir(workspaces)?;
    let id = workspace_state_id(workspace)?;
    let lock = workspaces.join(format!(".{id}.migration.lock"));
    let _guard = MigrationLock::acquire(&lock)?;

    let legacy = workspace.join(LEGACY_STATE_DIR);
    recover_overlapping_state(root, &id, &legacy, &state)?;
    if legacy.exists() {
        migrate_legacy_state(&legacy, &state, workspaces, &id)?;
        relocate_conflict_paths(&state, &legacy, &state)?;
    } else {
        private_dir(&state)?;
    }
    migrate_agent_layouts(&state)?;

    import_legacy_ignore(workspace, &state)?;
    write_location(&state, workspace)?;
    if let Some(identity) = workspace_identity(workspace)? {
        write_private(&state.join("identity"), identity.as_bytes())?;
    }
    maintain_workspace(workspace, &state)?;
    Ok(state)
}

fn recover_overlapping_state(root: &Path, id: &str, legacy: &Path, state: &Path) -> Result<()> {
    if !legacy.exists() || !state.exists() {
        return Ok(());
    }
    if trees_equal(legacy, state)? {
        fs::remove_dir_all(legacy).context("finish interrupted workspace-state migration")?;
        return Ok(());
    }

    // The project-local directory is the state an older installed client could
    // still be updating. Preserve the partial/stale global copy, then migrate
    // that active legacy state. No bytes are discarded.
    let quarantine = root.join("quarantine");
    private_dir(&quarantine)?;
    let destination = unique_child(&quarantine, &format!("workspace-{id}-global"));
    fs::rename(state, &destination).with_context(|| {
        format!(
            "quarantine conflicting global workspace state at {}",
            destination.display()
        )
    })?;
    tracing::warn!(
        "Preserved conflicting global workspace state at {} before migrating {}",
        destination.display(),
        legacy.display()
    );
    Ok(())
}

fn migrate_legacy_state(legacy: &Path, state: &Path, workspaces: &Path, id: &str) -> Result<()> {
    if !legacy.is_dir() {
        bail!(
            "legacy FeanorFS state is not a directory: {}",
            legacy.display()
        );
    }
    match fs::rename(legacy, state) {
        Ok(()) => return Ok(()),
        Err(rename_error) => tracing::debug!(
            "Direct workspace-state move failed ({}); using verified copy fallback",
            rename_error
        ),
    }

    let staging = unique_child(workspaces, &format!(".{id}.migrating"));
    let result = (|| -> Result<()> {
        copy_tree(legacy, &staging)?;
        if !trees_equal(legacy, &staging)? {
            bail!("workspace-state copy verification failed");
        }
        fs::rename(&staging, state).context("publish copied global workspace state")?;
        fs::remove_dir_all(legacy).context("remove verified legacy workspace state")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result.with_context(|| {
        format!(
            "move workspace state out of the project from {} to {}",
            legacy.display(),
            state.display()
        )
    })
}

fn unique_child(parent: &Path, stem: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    parent.join(format!("{stem}-{stamp}-{}", std::process::id()))
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    private_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)?;
            fs::set_permissions(&target, entry.metadata()?.permissions())?;
        } else {
            bail!(
                "workspace state contains unsupported symlink or special file: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn trees_equal(left: &Path, right: &Path) -> Result<bool> {
    let left_entries = sorted_entries(left)?;
    let right_entries = sorted_entries(right)?;
    if left_entries.len() != right_entries.len() {
        return Ok(false);
    }
    for ((left_name, left_kind), (right_name, right_kind)) in
        left_entries.into_iter().zip(right_entries)
    {
        if left_name != right_name || left_kind != right_kind {
            return Ok(false);
        }
        let left_path = left.join(&left_name);
        let right_path = right.join(&right_name);
        match left_kind {
            EntryKind::Directory => {
                if !trees_equal(&left_path, &right_path)? {
                    return Ok(false);
                }
            }
            EntryKind::File => {
                if fs::metadata(&left_path)?.len() != fs::metadata(&right_path)?.len()
                    || !files_equal(&left_path, &right_path)?
                {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Directory,
    File,
}

fn sorted_entries(directory: &Path) -> Result<Vec<(std::ffi::OsString, EntryKind)>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let kind = if kind.is_dir() {
            EntryKind::Directory
        } else if kind.is_file() {
            EntryKind::File
        } else {
            bail!(
                "unsupported workspace-state entry: {}",
                entry.path().display()
            );
        };
        entries.push((entry.file_name(), kind));
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(entries)
}

fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    let mut left = fs::File::open(left)?;
    let mut right = fs::File::open(right)?;
    let mut left_buffer = [0_u8; 64 * 1024];
    let mut right_buffer = [0_u8; 64 * 1024];
    loop {
        let left_read = left.read(&mut left_buffer)?;
        let right_read = right.read(&mut right_buffer)?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn relocate_conflict_paths(state: &Path, old_root: &Path, new_root: &Path) -> Result<()> {
    let path = state.join("local_state.json");
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("read migrated local conflict state"),
    };
    let mut local = LocalStateV1::from_json(&content)?;
    let mut changed = false;
    for record in local.conflict_registry.values_mut() {
        let current = Path::new(&record.conflict_dir);
        if let Ok(relative) = current.strip_prefix(old_root) {
            record.conflict_dir = new_root.join(relative).to_string_lossy().into_owned();
            changed = true;
        }
    }
    if changed {
        write_private(&path, local.to_json()?.as_bytes())?;
    }
    Ok(())
}

fn migrate_agent_layouts(state: &Path) -> Result<()> {
    let agents = state.join("agents");
    let entries = match fs::read_dir(&agents) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("read legacy agent workspaces"),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let root = entry.path();
        let worktree = root.join("worktree");
        if worktree.is_dir() {
            continue;
        }
        let staging = root.join("worktree.migrating");
        private_dir(&staging)?;
        let children = fs::read_dir(&root)?
            .filter_map(std::result::Result::ok)
            .map(|child| child.path())
            .collect::<Vec<_>>();
        for child in children {
            let name = child.file_name().and_then(|name| name.to_str());
            if matches!(
                name,
                Some("state" | "worktree" | "worktree.migrating" | "legacy-state")
            ) {
                continue;
            }
            if name == Some(LEGACY_STATE_DIR) {
                let base = child.join("base-snapshot");
                let destination = root.join("state/base-snapshot");
                if base.is_file() && !destination.exists() {
                    if let Some(parent) = destination.parent() {
                        private_dir(parent)?;
                    }
                    fs::copy(&base, &destination)?;
                }
                fs::rename(&child, root.join("legacy-state"))
                    .context("preserve legacy agent cache outside its worktree")?;
                continue;
            }
            let destination = staging.join(
                child
                    .file_name()
                    .context("legacy agent entry has no file name")?,
            );
            fs::rename(&child, destination).context("move legacy agent content into worktree")?;
        }
        fs::rename(&staging, &worktree).context("publish migrated agent worktree")?;
    }
    Ok(())
}

fn import_legacy_ignore(workspace: &Path, state: &Path) -> Result<()> {
    let legacy = workspace.join(LEGACY_IGNORE_FILE);
    if !legacy.exists() {
        return Ok(());
    }
    if !fs::symlink_metadata(&legacy)?.file_type().is_file() {
        bail!(
            "legacy project-local ignore policy is not a regular file: {}",
            legacy.display()
        );
    }
    let content = fs::read(&legacy).context("read legacy project-local ignore policy")?;
    let destination = state.join("ignore");
    if destination.exists() && fs::read(&destination)? != content {
        let previous = unique_child(state, "ignore.previous");
        fs::rename(&destination, &previous)
            .context("preserve previous global workspace ignore policy")?;
        tracing::warn!(
            "Preserved a differing global ignore policy at {} before importing the active legacy policy",
            previous.display()
        );
    }
    write_private(&destination, &content)?;
    fs::remove_file(&legacy).context("remove migrated project-local ignore policy")?;
    Ok(())
}

fn write_location(state: &Path, workspace: &Path) -> Result<()> {
    let canonical = canonical_workspace(workspace)?;
    write_private(
        &state.join("location"),
        canonical.to_string_lossy().as_bytes(),
    )
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("private state file has no parent")?;
    private_dir(parent)?;
    #[cfg(unix)]
    let mut file = {
        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        options.open(path)?
    };
    #[cfg(not(unix))]
    let mut file = atomic_write_file::AtomicWriteFile::open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    file.commit()?;
    Ok(())
}

struct MigrationLock(PathBuf);

impl MigrationLock {
    fn acquire(path: &Path) -> Result<Self> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            match options.open(path) {
                Ok(mut file) => {
                    writeln!(file, "{}", std::process::id())?;
                    return Ok(Self(path.to_path_buf()));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = fs::read_to_string(path)
                        .ok()
                        .and_then(|pid| pid.trim().parse::<u32>().ok())
                        .is_none_or(|pid| !crate::lock::pid_alive(pid));
                    if stale {
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        bail!("workspace state migration is already running");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(crate) fn retention_age() -> Duration {
    let days = std::env::var("FEANORFS_RETENTION_DAYS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|days| *days > 0)
        .unwrap_or(DEFAULT_RETENTION_DAYS);
    Duration::from_secs(days.saturating_mul(24 * 60 * 60))
}

pub fn maintain_workspace_state(state: &Path) -> Result<()> {
    let retention = retention_age();
    purge_old_children(&state.join("tmp"), TEMP_RETENTION)?;
    purge_old_children(&state.join("recovery"), retention)?;
    rotate_log(&state.join("feanorfs.log"), retention)?;
    Ok(())
}

fn maintain_workspace(workspace: &Path, state: &Path) -> Result<()> {
    let stamp = state.join("maintenance.stamp");
    if fs::metadata(&stamp)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age < MAINTENANCE_INTERVAL)
    {
        return Ok(());
    }
    maintain_workspace_state(state)?;
    purge_stale_worktree_temps(workspace, TEMP_RETENTION)?;
    write_private(&stamp, b"workspace maintenance v1\n")
}

fn purge_stale_worktree_temps(workspace: &Path, max_age: Duration) -> Result<()> {
    fn visit(directory: &Path, now: SystemTime, max_age: Duration) -> Result<()> {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            let kind = entry.file_type()?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() {
                if matches!(name.as_ref(), ".git" | ".jj" | ".feanorfs") {
                    continue;
                }
                visit(&entry.path(), now, max_age)?;
            } else if kind.is_file()
                && name.starts_with(".feanorfs-tmp-")
                && entry
                    .metadata()?
                    .modified()
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok())
                    .is_some_and(|age| age > max_age)
            {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }
    visit(workspace, SystemTime::now(), max_age)
}

fn purge_old_children(directory: &Path, max_age: Duration) -> Result<()> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() <= max_age {
            continue;
        }
        let path = entry.path();
        if metadata.is_dir() {
            let _ = fs::remove_dir_all(path);
        } else {
            let _ = fs::remove_file(path);
        }
    }
    Ok(())
}

fn rotate_log(log: &Path, retention: Duration) -> Result<()> {
    let Ok(metadata) = fs::metadata(log) else {
        return Ok(());
    };
    let expired = metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > retention);
    if metadata.len() <= MAX_LOG_BYTES && !expired {
        return Ok(());
    }
    let rotated = log.with_extension("log.old");
    let _ = fs::remove_file(&rotated);
    fs::rename(log, rotated)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ConflictRecordV1;
    use feanorfs_common::ConflictKind;

    #[test]
    fn fresh_state_creates_no_project_metadata() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project_parent.path().join("project");
        fs::create_dir(&project).unwrap();

        let state = ensure_workspace_state_in(&project, global.path()).unwrap();

        assert!(state.starts_with(global.path().join("workspaces")));
        assert!(state.join("location").is_file());
        assert!(!project.join(LEGACY_STATE_DIR).exists());
        assert!(!project.join(LEGACY_IGNORE_FILE).exists());
    }

    #[test]
    fn legacy_state_and_ignore_rules_move_out_of_project() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project_parent.path().join("project");
        let legacy = project.join(LEGACY_STATE_DIR);
        fs::create_dir_all(legacy.join("conflicts/1")).unwrap();
        fs::write(legacy.join("config.json"), b"legacy config").unwrap();
        fs::write(project.join(LEGACY_IGNORE_FILE), b"server-data/\n").unwrap();
        let mut local = LocalStateV1::default();
        local.conflict_registry.insert(
            "src/lib.rs".into(),
            ConflictRecordV1 {
                path: "src/lib.rs".into(),
                kind: ConflictKind::EditEdit,
                conflict_dir: legacy.join("conflicts/1").to_string_lossy().into_owned(),
                opened_at: 1,
                status: "pending".into(),
            },
        );
        fs::write(legacy.join("local_state.json"), local.to_json().unwrap()).unwrap();

        let state = ensure_workspace_state_in(&project, global.path()).unwrap();

        assert_eq!(
            fs::read(state.join("config.json")).unwrap(),
            b"legacy config"
        );
        assert_eq!(fs::read(state.join("ignore")).unwrap(), b"server-data/\n");
        assert!(!legacy.exists());
        assert!(!project.join(LEGACY_IGNORE_FILE).exists());
        let migrated =
            LocalStateV1::from_json(&fs::read_to_string(state.join("local_state.json")).unwrap())
                .unwrap();
        assert!(migrated.conflict_registry["src/lib.rs"]
            .conflict_dir
            .starts_with(&state.to_string_lossy().into_owned()));
    }

    #[test]
    fn conflicting_partial_global_state_is_quarantined_and_legacy_wins() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let project = project_parent.path().join("project");
        let legacy = project.join(LEGACY_STATE_DIR);
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("config.json"), b"active legacy").unwrap();
        let state = workspace_state_path_in(&project, global.path()).unwrap();
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("config.json"), b"partial global").unwrap();

        let state = ensure_workspace_state_in(&project, global.path()).unwrap();

        assert_eq!(
            fs::read(state.join("config.json")).unwrap(),
            b"active legacy"
        );
        assert!(!legacy.exists());
        let quarantine = global.path().join("quarantine");
        let preserved = fs::read_dir(quarantine)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(preserved.len(), 1);
        assert_eq!(
            fs::read(preserved[0].join("config.json")).unwrap(),
            b"partial global"
        );
    }

    #[cfg(unix)]
    #[test]
    fn renamed_folder_keeps_its_existing_global_state() {
        let project_parent = tempfile::tempdir().unwrap();
        let global = tempfile::tempdir().unwrap();
        let original = project_parent.path().join("before");
        let renamed = project_parent.path().join("after");
        fs::create_dir(&original).unwrap();
        let state = ensure_workspace_state_in(&original, global.path()).unwrap();
        fs::write(state.join("config.json"), b"configured").unwrap();

        fs::rename(&original, &renamed).unwrap();
        let relocated = ensure_workspace_state_in(&renamed, global.path()).unwrap();

        assert_eq!(relocated, state);
        assert_eq!(
            fs::read(relocated.join("config.json")).unwrap(),
            b"configured"
        );
        assert_eq!(
            fs::read_to_string(relocated.join("location")).unwrap(),
            fs::canonicalize(renamed).unwrap().to_string_lossy()
        );
    }

    #[test]
    fn verified_copy_preserves_nested_bytes() {
        let source = tempfile::tempdir().unwrap();
        let destination_parent = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("objects/nested")).unwrap();
        fs::write(source.path().join("objects/nested/blob"), b"ciphertext").unwrap();
        let destination = destination_parent.path().join("copy");

        copy_tree(source.path(), &destination).unwrap();

        assert!(trees_equal(source.path(), &destination).unwrap());
    }

    #[test]
    fn legacy_agent_content_is_separated_from_agent_metadata() {
        let state = tempfile::tempdir().unwrap();
        let agent = state.path().join("agents/coder");
        fs::create_dir_all(agent.join("src")).unwrap();
        fs::create_dir_all(agent.join(".feanorfs")).unwrap();
        fs::write(agent.join("src/lib.rs"), b"agent work").unwrap();
        fs::write(agent.join(".feanorfs/base-snapshot"), b"snapshot-id").unwrap();
        fs::write(agent.join(".feanorfs/local_state.json"), b"cache").unwrap();

        migrate_agent_layouts(state.path()).unwrap();

        assert_eq!(
            fs::read(agent.join("worktree/src/lib.rs")).unwrap(),
            b"agent work"
        );
        assert_eq!(
            fs::read(agent.join("state/base-snapshot")).unwrap(),
            b"snapshot-id"
        );
        assert!(agent.join("legacy-state/local_state.json").is_file());
        assert!(!agent.join("worktree/.feanorfs").exists());
    }

    #[test]
    fn retention_removes_expired_children_and_rotates_oversized_log() {
        let state = tempfile::tempdir().unwrap();
        fs::create_dir(state.path().join("tmp")).unwrap();
        fs::write(state.path().join("tmp/old"), b"temporary").unwrap();
        std::thread::sleep(Duration::from_millis(2));
        purge_old_children(&state.path().join("tmp"), Duration::ZERO).unwrap();
        assert!(!state.path().join("tmp/old").exists());

        let log = state.path().join("feanorfs.log");
        let file = fs::File::create(&log).unwrap();
        file.set_len(MAX_LOG_BYTES + 1).unwrap();
        rotate_log(&log, Duration::from_secs(u64::MAX)).unwrap();
        assert!(!log.exists());
        assert!(state.path().join("feanorfs.log.old").is_file());
    }
}
