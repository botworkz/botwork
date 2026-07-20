use std::fs;
use std::path::{Path, PathBuf};

use crate::atomic::atomic_write_public;
use crate::error::VaultError;

// ---------------------------------------------------------------------------
// Label validation
// ---------------------------------------------------------------------------

/// Validates a public-key label: `^[a-z0-9][a-z0-9._-]{0,62}$`.
fn validate_label(label: &str) -> Result<(), VaultError> {
    if label.is_empty() || label.len() > 63 {
        return Err(VaultError::PublicStore(format!("invalid label: {label}")));
    }
    let mut chars = label.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(VaultError::PublicStore(format!("invalid label: {label}")));
    }
    for c in chars {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '.' && c != '_' && c != '-' {
            return Err(VaultError::PublicStore(format!("invalid label: {label}")));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SSH public-key type validation
// ---------------------------------------------------------------------------

fn is_valid_ssh_key_type(token: &str) -> bool {
    matches!(token, "ssh-rsa" | "ssh-ed25519" | "ssh-ecdsa")
        || token.starts_with("ecdsa-sha2-")
        || token.starts_with("sk-")
}

/// Parses the key type and comment from a single OpenSSH public-key line.
///
/// Returns `(key_type, comment)` or a `PublicStore` error if the line does
/// not start with a recognised OpenSSH key-type token.
fn parse_ssh_key_line(line: &str) -> Result<(String, String), VaultError> {
    let mut parts = line.split_whitespace();
    let key_type = parts
        .next()
        .ok_or_else(|| VaultError::PublicStore("not an OpenSSH public key".to_string()))?;
    if !is_valid_ssh_key_type(key_type) {
        return Err(VaultError::PublicStore(
            "not an OpenSSH public key".to_string(),
        ));
    }
    // Skip the base64 key material; collect the rest as comment.
    let _ = parts.next();
    let comment = parts.collect::<Vec<_>>().join(" ");
    Ok((key_type.to_string(), comment))
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Metadata for a single SSH public key stored in the sidecar tree.
#[derive(Debug)]
pub struct SshEntry {
    pub label: String,
    pub key_type: String,
    pub comment: String,
}

/// A plaintext sidecar store for public material that does not need to be
/// inside the sealed `vault.botwork`.
///
/// On-disk layout (relative to the vault root):
/// ```text
/// public/
/// └── ssh/
///     └── <label>.pub   # 0644; one OpenSSH authorized_keys line
/// ```
pub struct PublicStore {
    root: PathBuf,
}

impl PublicStore {
    /// Construct a handle pointing at an existing vault root.
    ///
    /// This call is cheap and does **not** touch the filesystem.
    pub fn new(vault_root: &Path) -> Self {
        Self {
            root: vault_root.to_path_buf(),
        }
    }

    fn public_dir(&self) -> PathBuf {
        self.root.join("public")
    }

    fn ssh_dir(&self) -> PathBuf {
        self.root.join("public").join("ssh")
    }

    fn check_root(&self) -> Result<(), VaultError> {
        if !self.root.exists() {
            return Err(VaultError::PublicStore(
                "vault root not initialized".to_string(),
            ));
        }
        Ok(())
    }

    /// Ensure `<root>/public/` and `<root>/public/ssh/` exist with `0755`.
    ///
    /// Returns `true` if `<root>/public/ssh/` had to be created (i.e. this
    /// is the first call), `false` if it already existed.
    #[cfg(unix)]
    fn ensure_ssh_dir(&self) -> Result<bool, VaultError> {
        use std::os::unix::fs::PermissionsExt;

        let ssh_dir = self.ssh_dir();
        if ssh_dir.exists() {
            return Ok(false);
        }

        let public_dir = self.public_dir();
        if !public_dir.exists() {
            fs::create_dir(&public_dir)?;
            fs::set_permissions(&public_dir, fs::Permissions::from_mode(0o755))?;
        }
        fs::create_dir(&ssh_dir)?;
        fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o755))?;
        Ok(true)
    }

    /// Store a single OpenSSH public key under the given label.
    ///
    /// `key_line` must have a recognised OpenSSH key-type token as its first
    /// whitespace-delimited field on the first line.
    ///
    /// On the first call ever (i.e. when `<root>/public/ssh/` is created),
    /// `<root>` is chmod'd to `0o701` so that the peer bastion process can
    /// traverse into `<root>/public/` even though it cannot read `<root>`
    /// itself.
    #[cfg(unix)]
    pub fn add_ssh(&self, label: &str, key_line: &str, force: bool) -> Result<(), VaultError> {
        use std::os::unix::fs::PermissionsExt;

        self.check_root()?;
        validate_label(label)?;

        // Validate the first line of the supplied key material.
        let first_line = key_line.lines().next().unwrap_or("").trim();
        parse_ssh_key_line(first_line)?;

        let dest = self.ssh_dir().join(format!("{label}.pub"));

        // Check for an existing label before touching directories.
        if dest.exists() && !force {
            return Err(VaultError::PublicStore(format!(
                "label already exists: {label}"
            )));
        }

        // Create <root>/public/ssh/ if this is the first call.
        let first_use = self.ensure_ssh_dir()?;

        // Normalise: ensure the stored line is newline-terminated.
        let key_data = if key_line.ends_with('\n') {
            key_line.to_string()
        } else {
            format!("{key_line}\n")
        };

        atomic_write_public(&dest, key_data.as_bytes())?;

        // Bump <root> to 0o701 on first use so peers can traverse into
        // <root>/public/ without being able to list <root> itself.
        if first_use {
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o701))?;
        }

        Ok(())
    }

    /// List all stored SSH public keys, sorted lexicographically by label.
    ///
    /// Files that are empty or that do not start with a recognised key-type
    /// token are silently skipped.
    pub fn list_ssh(&self) -> Result<Vec<SshEntry>, VaultError> {
        self.check_root()?;
        let ssh_dir = self.ssh_dir();
        if !ssh_dir.exists() {
            return Ok(vec![]);
        }

        let mut files: Vec<_> = fs::read_dir(&ssh_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "pub").unwrap_or(false))
            .collect();
        files.sort_by_key(|e| e.file_name());

        let mut entries = Vec::new();
        for entry in files {
            let path = entry.path();
            let label = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let content = fs::read_to_string(&path)?;
            let first = content.lines().next().unwrap_or("").trim();
            if first.is_empty() {
                continue;
            }
            if let Ok((key_type, comment)) = parse_ssh_key_line(first) {
                entries.push(SshEntry {
                    label,
                    key_type,
                    comment,
                });
            }
        }

        Ok(entries)
    }

    /// Remove the `.pub` file for the given label.
    pub fn delete_ssh(&self, label: &str) -> Result<(), VaultError> {
        self.check_root()?;
        validate_label(label)?;
        let dest = self.ssh_dir().join(format!("{label}.pub"));
        if !dest.exists() {
            return Err(VaultError::PublicStore(format!("no such label: {label}")));
        }
        fs::remove_file(&dest)?;
        Ok(())
    }

    /// Concatenate all stored SSH public-key lines to a single `String`,
    /// one key per line, sorted lexicographically by label.
    ///
    /// Empty files and files with an unrecognised key type are silently
    /// skipped.  Returns an empty string when no keys are stored.
    pub fn cat_ssh(&self) -> Result<String, VaultError> {
        self.check_root()?;
        let ssh_dir = self.ssh_dir();
        if !ssh_dir.exists() {
            return Ok(String::new());
        }

        let mut files: Vec<_> = fs::read_dir(&ssh_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "pub").unwrap_or(false))
            .collect();
        files.sort_by_key(|e| e.file_name());

        let mut result = String::new();
        for entry in files {
            let path = entry.path();
            let content = fs::read_to_string(&path)?;
            if content.is_empty() {
                continue;
            }
            let first = content.lines().next().unwrap_or("").trim();
            if first.is_empty() {
                continue;
            }
            let mut parts = first.split_whitespace();
            if let Some(token) = parts.next() {
                if is_valid_ssh_key_type(token) {
                    result.push_str(first);
                    result.push('\n');
                }
            }
        }

        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_root() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("vault");
        fs::create_dir_all(&root).unwrap();
        (dir, root)
    }

    const ED25519_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI test-comment";
    const RSA_KEY: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAB rsa-comment";
    const ECDSA_KEY: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYA ecdsa-comment";

    // -----------------------------------------------------------------------
    // Label validation
    // -----------------------------------------------------------------------

    #[test]
    fn valid_labels_accepted() {
        for label in &["a", "abc", "a1-b_2.c", &"a".repeat(63)] {
            validate_label(label).unwrap();
        }
    }

    #[test]
    fn invalid_labels_rejected() {
        for label in &[
            "",
            "-starts-with-dash",
            ".starts-with-dot",
            "_starts-with-underscore",
            "UPPERCASE",
            "has space",
            &"a".repeat(64),
        ] {
            assert!(
                validate_label(label).is_err(),
                "expected label {label:?} to be rejected"
            );
        }
    }

    // -----------------------------------------------------------------------
    // SSH key-type validation
    // -----------------------------------------------------------------------

    #[test]
    fn valid_ssh_key_lines_accepted() {
        for line in &[
            ED25519_KEY,
            RSA_KEY,
            ECDSA_KEY,
            "ssh-ecdsa AAAAB foo",
            "ecdsa-sha2-nistp384 AAAAB foo",
            "sk-ssh-ed25519@openssh.com AAAAB foo",
        ] {
            parse_ssh_key_line(line).unwrap();
        }
    }

    #[test]
    fn invalid_ssh_key_lines_rejected() {
        for line in &[
            "",
            "not-a-key AAAAB foo",
            "# comment line",
            "command=\"foo\" ssh-rsa AAAA",
        ] {
            assert!(
                parse_ssh_key_line(line).is_err(),
                "expected key line {line:?} to be rejected"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Root-not-initialized guard
    // -----------------------------------------------------------------------

    #[test]
    #[cfg(unix)]
    fn add_fails_when_root_missing() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("nonexistent");
        let store = PublicStore::new(&root);
        let err = store.add_ssh("mykey", ED25519_KEY, false).unwrap_err();
        assert!(err.to_string().contains("vault root not initialized"));
    }

    #[test]
    fn list_fails_when_root_missing() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("nonexistent");
        let store = PublicStore::new(&root);
        let err = store.list_ssh().unwrap_err();
        assert!(err.to_string().contains("vault root not initialized"));
    }

    #[test]
    fn delete_fails_when_root_missing() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("nonexistent");
        let store = PublicStore::new(&root);
        let err = store.delete_ssh("mykey").unwrap_err();
        assert!(err.to_string().contains("vault root not initialized"));
    }

    #[test]
    fn cat_fails_when_root_missing() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("nonexistent");
        let store = PublicStore::new(&root);
        let err = store.cat_ssh().unwrap_err();
        assert!(err.to_string().contains("vault root not initialized"));
    }

    // -----------------------------------------------------------------------
    // add_ssh / list_ssh / delete_ssh / cat_ssh happy-path
    // -----------------------------------------------------------------------

    #[test]
    #[cfg(unix)]
    fn add_and_list() {
        let (_dir, root) = make_root();
        let store = PublicStore::new(&root);
        store.add_ssh("mykey", ED25519_KEY, false).unwrap();
        let entries = store.list_ssh().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, "mykey");
        assert_eq!(entries[0].key_type, "ssh-ed25519");
        assert_eq!(entries[0].comment, "test-comment");
    }

    #[test]
    #[cfg(unix)]
    fn add_duplicate_without_force_fails() {
        let (_dir, root) = make_root();
        let store = PublicStore::new(&root);
        store.add_ssh("mykey", ED25519_KEY, false).unwrap();
        let err = store.add_ssh("mykey", ED25519_KEY, false).unwrap_err();
        assert!(err.to_string().contains("label already exists: mykey"));
    }

    #[test]
    #[cfg(unix)]
    fn add_duplicate_with_force_succeeds() {
        let (_dir, root) = make_root();
        let store = PublicStore::new(&root);
        store.add_ssh("mykey", ED25519_KEY, false).unwrap();
        store.add_ssh("mykey", RSA_KEY, true).unwrap();
        let entries = store.list_ssh().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key_type, "ssh-rsa");
    }

    #[test]
    #[cfg(unix)]
    fn add_invalid_key_line_fails() {
        let (_dir, root) = make_root();
        let store = PublicStore::new(&root);
        let err = store
            .add_ssh("mykey", "not-a-key AAAA comment", false)
            .unwrap_err();
        assert!(err.to_string().contains("not an OpenSSH public key"));
    }

    #[test]
    #[cfg(unix)]
    fn list_empty_when_no_keys() {
        let (_dir, root) = make_root();
        let store = PublicStore::new(&root);
        let entries = store.list_ssh().unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn list_sorted_by_label() {
        let (_dir, root) = make_root();
        let store = PublicStore::new(&root);
        store.add_ssh("beta", ED25519_KEY, false).unwrap();
        store.add_ssh("alpha", RSA_KEY, false).unwrap();
        store.add_ssh("gamma", ECDSA_KEY, false).unwrap();
        let entries = store.list_ssh().unwrap();
        let labels: Vec<_> = entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    #[cfg(unix)]
    fn delete_existing_label() {
        let (_dir, root) = make_root();
        let store = PublicStore::new(&root);
        store.add_ssh("mykey", ED25519_KEY, false).unwrap();
        store.delete_ssh("mykey").unwrap();
        let entries = store.list_ssh().unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn delete_missing_label_fails() {
        let (_dir, root) = make_root();
        let store = PublicStore::new(&root);
        let err = store.delete_ssh("missing").unwrap_err();
        assert!(err.to_string().contains("no such label: missing"));
    }

    #[test]
    #[cfg(unix)]
    fn cat_empty_when_no_keys() {
        let (_dir, root) = make_root();
        let store = PublicStore::new(&root);
        let out = store.cat_ssh().unwrap();
        assert!(out.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn cat_returns_sorted_newline_terminated_lines() {
        let (_dir, root) = make_root();
        let store = PublicStore::new(&root);
        store.add_ssh("beta", ED25519_KEY, false).unwrap();
        store.add_ssh("alpha", RSA_KEY, false).unwrap();
        let out = store.cat_ssh().unwrap();
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("ssh-rsa"), "first should be alpha/rsa");
        assert!(
            lines[1].starts_with("ssh-ed25519"),
            "second should be beta/ed25519"
        );
        // Must end with a newline
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn list_skips_empty_public_key_files() {
        let (_dir, root) = make_root();
        let ssh_dir = root.join("public").join("ssh");
        fs::create_dir_all(&ssh_dir).unwrap();
        fs::write(ssh_dir.join("empty.pub"), "").unwrap();
        fs::write(ssh_dir.join("blank.pub"), "\n").unwrap();

        let store = PublicStore::new(&root);
        assert!(store.list_ssh().unwrap().is_empty());
    }

    #[test]
    fn cat_skips_empty_public_key_files() {
        let (_dir, root) = make_root();
        let ssh_dir = root.join("public").join("ssh");
        fs::create_dir_all(&ssh_dir).unwrap();
        fs::write(ssh_dir.join("empty.pub"), "").unwrap();
        fs::write(ssh_dir.join("blank.pub"), "\n").unwrap();

        let store = PublicStore::new(&root);
        assert!(store.cat_ssh().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------
    // Permission bump
    // -----------------------------------------------------------------------

    #[test]
    #[cfg(unix)]
    fn root_permissions_bumped_to_0o701_on_first_add() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, root) = make_root();
        // Root starts at 0700 (or whatever make_root gives; assert below only
        // about post-add state).
        let store = PublicStore::new(&root);
        store.add_ssh("mykey", ED25519_KEY, false).unwrap();

        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o701, "root should be 0o701 after first pubkey add");
    }

    #[test]
    #[cfg(unix)]
    fn root_permissions_not_bumped_on_subsequent_add() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, root) = make_root();
        let store = PublicStore::new(&root);
        store.add_ssh("key1", ED25519_KEY, false).unwrap();
        // Manually set root back to 0700 to simulate a scenario where another
        // process reset it.
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        // Second add does NOT re-bump since the dir already exists.
        store.add_ssh("key2", RSA_KEY, false).unwrap();
        let mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        // Root is whatever it was; this test just ensures no panic and that
        // ensure_ssh_dir returns false.
        assert_eq!(mode, 0o700);
    }

    // -----------------------------------------------------------------------
    // File permissions
    // -----------------------------------------------------------------------

    #[test]
    #[cfg(unix)]
    fn key_file_is_0o644() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, root) = make_root();
        let store = PublicStore::new(&root);
        store.add_ssh("mykey", ED25519_KEY, false).unwrap();
        let path = root.join("public").join("ssh").join("mykey.pub");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }
}
