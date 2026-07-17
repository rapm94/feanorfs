use anyhow::Result;
use std::fs;
use std::path::Path;

pub(super) fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(super) fn write_private_json(path: &Path, content: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;

        let mut options = atomic_write_file::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        atomic_write_file::unix::OpenOptionsExt::preserve_mode(&mut options, false);
        let mut file = options.open(path)?;
        file.write_all(content.as_bytes())?;
        file.commit()?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
    }
    #[cfg(not(unix))]
    crate::durable::atomic_overwrite(path, content.as_bytes())?;
    Ok(())
}
