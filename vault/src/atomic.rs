use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::VaultError;

#[cfg(unix)]
pub fn atomic_write_private(dest: &Path, data: &[u8]) -> Result<(), VaultError> {
    use std::os::unix::fs::PermissionsExt;
    use tempfile::NamedTempFile;

    let parent = dest.parent().ok_or_else(|| {
        VaultError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination has no parent dir",
        ))
    })?;

    // Defense-in-depth: re-assert private root-dir perms on every write.
    // This silently undoes operator chmod drift during live operations.
    // If the public sidecar dir already exists, preserve 0o701 so the peer
    // process (bastion sshd) can still traverse into public/.
    let root_mode = if parent.join("public").exists() {
        0o701
    } else {
        0o700
    };
    fs::set_permissions(parent, fs::Permissions::from_mode(root_mode))?;

    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    tmp.as_file().sync_all()?;
    tmp.persist(dest).map_err(|e| e.error)?;

    let dir = fs::File::open(parent)?;
    dir.sync_all()?;

    Ok(())
}

/// Atomic write for public (world-readable) files in the sidecar tree.
/// Sets the written file to 0o644. Does NOT reassert root-dir permissions.
#[cfg(unix)]
pub fn atomic_write_public(dest: &Path, data: &[u8]) -> Result<(), VaultError> {
    use std::os::unix::fs::PermissionsExt;
    use tempfile::NamedTempFile;

    let parent = dest.parent().ok_or_else(|| {
        VaultError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "destination has no parent dir",
        ))
    })?;

    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))?;
    tmp.as_file().sync_all()?;
    tmp.persist(dest).map_err(|e| e.error)?;

    let dir = fs::File::open(parent)?;
    dir.sync_all()?;

    Ok(())
}

#[cfg(unix)]
pub fn ensure_vault_root(root: &Path) -> Result<(), VaultError> {
    use std::os::unix::fs::PermissionsExt;

    fs::create_dir_all(root)?;
    fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    #[test]
    fn ensure_vault_root_rejects_existing_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("not-a-dir");
        fs::write(&file, b"x").unwrap();
        let err = ensure_vault_root(&file).unwrap_err();
        assert!(matches!(err, VaultError::Io(_)), "got {err:?}");
    }

    #[test]
    fn atomic_write_private_sets_root_mode_without_public_sidecar() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

        let dest = root.join("vault.botwork");
        atomic_write_private(&dest, b"secret").unwrap();

        assert_eq!(fs::read(&dest).unwrap(), b"secret");
        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn atomic_write_private_preserves_public_traversal_mode() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        fs::create_dir_all(root.join("public")).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();

        let dest = root.join("vault.botwork");
        atomic_write_private(&dest, b"secret").unwrap();

        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o701);
    }

    #[test]
    fn atomic_write_private_rejects_path_without_parent() {
        let err = atomic_write_private(Path::new(""), b"secret").unwrap_err();
        assert!(matches!(err, VaultError::Io(_)), "got {err:?}");
    }

    #[test]
    fn atomic_write_public_rejects_path_without_parent() {
        let err = atomic_write_public(Path::new(""), b"public").unwrap_err();
        assert!(matches!(err, VaultError::Io(_)), "got {err:?}");
    }

    #[test]
    fn atomic_write_private_surfaces_tempfile_creation_errors() {
        let dir = TempDir::new().unwrap();
        let root_file = dir.path().join("root-file");
        fs::write(&root_file, b"file").unwrap();
        let err = atomic_write_private(&root_file.join("vault.botwork"), b"secret").unwrap_err();
        assert!(matches!(err, VaultError::Io(_)), "got {err:?}");
    }

    #[test]
    fn atomic_write_private_surfaces_persist_errors() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        fs::create_dir_all(&root).unwrap();
        let dest = root.join("vault.botwork");
        fs::create_dir(&dest).unwrap();
        let err = atomic_write_private(&dest, b"secret").unwrap_err();
        assert!(matches!(err, VaultError::Io(_)), "got {err:?}");
    }
}
