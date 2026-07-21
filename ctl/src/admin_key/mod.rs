//! `botctl admin-key` — genesis admin credential management.
//!
//! Manages the pre-shared key that identifies the genesis admin to
//! `botwork-auth-broker`.  When `BOTWORK_ADMIN_API_KEY` is set in
//! auth-broker's environment, any request carrying an
//! `Authorization: ****** header whose token matches that key
//! passes the admin check and receives
//! `x-botwork-admin: admin` in the forwarded headers — satisfying
//! `botwork-api`'s `admin_required` gate on `/api/tenants`,
//! `/api/plugins`, etc.
//!
//! The key is stored in a file (not an env var) so it is never exposed
//! via `docker inspect` or process-env dumps.  Systemd reads the file
//! via `EnvironmentFile=` and injects the env var into auth-broker's
//! process only.
//!
//! # Key file
//!
//! Default path: [`DEFAULT_KEY_FILE`] (`/var/lib/botwork/admin.env`).
//! Override with `--file <path>` or [`KEY_FILE_ENV`]
//! (`BOTWORK_ADMIN_KEY_FILE`).
//!
//! Format (shell-parseable; matches `/var/lib/botwork-db/secret.env`):
//!
//! ```text
//! BOTWORK_ADMIN_API_KEY=<key>
//! ```
//!
//! # Commands
//!
//! ```text
//! botctl admin-key get        Print the current admin key to stdout.
//! botctl admin-key set <key>  Write <key> to the key file (rotation).
//! botctl admin-key generate   Generate a new random key and write it
//!                             (no-op if the file already exists, unless
//!                             --force is given).
//! ```
//!
//! # Exit codes
//!
//! | Code | Meaning                                         |
//! |------|-------------------------------------------------|
//! | 0    | success                                         |
//! | 2    | invalid CLI usage                               |
//! | 4    | key-file I/O error (read or write)              |
//!
//! # Genesis provisioning
//!
//! On a clean boot the sysadmin (or the systemd service that wraps
//! `botctl bootstrap`) should run:
//!
//! ```bash
//! botctl admin-key generate   # idempotent: no-op if key already exists
//! ```
//!
//! The generated key is written to [`DEFAULT_KEY_FILE`].  Systemd then
//! picks it up via `EnvironmentFile=` and passes it into auth-broker as
//! `BOTWORK_ADMIN_API_KEY`.  Rotation:
//!
//! ```bash
//! botctl admin-key generate --force   # rotate: overwrite existing key
//! # or:
//! botctl admin-key set <new-key>
//! ```

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::info;

/// Default path for the admin key file.
///
/// Written with `0600` permissions by [`run`] so only the owning
/// user (root in production) can read it.
pub const DEFAULT_KEY_FILE: &str = "/var/lib/botwork/admin.env";

/// Env var that overrides [`DEFAULT_KEY_FILE`].
pub const KEY_FILE_ENV: &str = "BOTWORK_ADMIN_KEY_FILE";

/// Name of the env var written into the key file.
pub const KEY_ENV_VAR: &str = "BOTWORK_ADMIN_API_KEY";

/// Entry point dispatched from `cli::dispatch`.
pub fn run(argv: &[String]) -> Result<i32, AdminKeyError> {
    let args = Args::from_argv(argv)?;
    match args.command {
        Command::Get => cmd_get(&args.key_file),
        Command::Set(key) => cmd_set(&args.key_file, &key),
        Command::Generate { force } => cmd_generate(&args.key_file, force),
    }
}

fn cmd_get(key_file: &Path) -> Result<i32, AdminKeyError> {
    let key = read_key(key_file)?;
    println!("{key}");
    Ok(0)
}

fn cmd_set(key_file: &Path, key: &str) -> Result<i32, AdminKeyError> {
    validate_key(key)?;
    write_key(key_file, key)?;
    info!("[admin-key] key written to {}", key_file.display());
    Ok(0)
}

fn cmd_generate(key_file: &Path, force: bool) -> Result<i32, AdminKeyError> {
    if !force && key_file.exists() {
        // Idempotent: already provisioned — treat as success.
        info!(
            "[admin-key] key file already exists at {}; skipping (use --force to overwrite)",
            key_file.display()
        );
        return Ok(0);
    }
    let key = generate_key();
    write_key(key_file, &key)?;
    info!(
        "[admin-key] generated new admin key; written to {}",
        key_file.display()
    );
    Ok(0)
}

// ── key generation ──────────────────────────────────────────────────

/// Generate a fresh random admin key.
///
/// Format: two UUID v4 values joined with `-` giving ~244 bits of
/// entropy in a URL-safe, header-safe string.
fn generate_key() -> String {
    format!("{}-{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4())
}

// ── key file I/O ────────────────────────────────────────────────────

fn read_key(key_file: &Path) -> Result<String, AdminKeyError> {
    let content = fs::read_to_string(key_file).map_err(|err| AdminKeyError::FileRead {
        path: key_file.to_path_buf(),
        reason: err.to_string(),
    })?;
    parse_key_from_content(&content, key_file)
}

fn parse_key_from_content(content: &str, key_file: &Path) -> Result<String, AdminKeyError> {
    for line in content.lines() {
        let line = line.trim();
        if let Some(val) = line
            .strip_prefix(KEY_ENV_VAR)
            .and_then(|s| s.strip_prefix('='))
        {
            let val = val.trim();
            if !val.is_empty() {
                return Ok(val.to_string());
            }
        }
    }
    Err(AdminKeyError::KeyNotFound(key_file.to_path_buf()))
}

fn write_key(key_file: &Path, key: &str) -> Result<(), AdminKeyError> {
    // Ensure parent directory exists.
    if let Some(parent) = key_file.parent() {
        fs::create_dir_all(parent).map_err(|err| AdminKeyError::FileWrite {
            path: key_file.to_path_buf(),
            reason: format!("failed to create parent directory: {err}"),
        })?;
    }
    let content = format!("{KEY_ENV_VAR}={key}\n");
    // Write atomically via a temporary file in the same directory.
    let tmp_path = key_file.with_extension("env.tmp");
    let mut file = fs::File::create(&tmp_path).map_err(|err| AdminKeyError::FileWrite {
        path: key_file.to_path_buf(),
        reason: err.to_string(),
    })?;
    // Restrict permissions before writing the secret (best-effort; unix only).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    }
    file.write_all(content.as_bytes())
        .map_err(|err| AdminKeyError::FileWrite {
            path: key_file.to_path_buf(),
            reason: err.to_string(),
        })?;
    drop(file);
    fs::rename(&tmp_path, key_file).map_err(|err| AdminKeyError::FileWrite {
        path: key_file.to_path_buf(),
        reason: err.to_string(),
    })?;
    Ok(())
}

fn validate_key(key: &str) -> Result<(), AdminKeyError> {
    if key.is_empty() {
        return Err(AdminKeyError::InvalidKey("key must not be empty".into()));
    }
    // Reject whitespace and non-ASCII-printable chars that would break
    // the env-file format or header encoding.
    if key
        .chars()
        .any(|c| c.is_whitespace() || !c.is_ascii() || c == '=')
    {
        return Err(AdminKeyError::InvalidKey(
            "key must contain only printable ASCII characters (no whitespace, no '=')".into(),
        ));
    }
    Ok(())
}

// ── argument parsing ────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Command {
    Get,
    Set(String),
    Generate { force: bool },
}

#[derive(Debug, Clone)]
struct Args {
    command: Command,
    key_file: PathBuf,
}

impl Args {
    /// Parse `argv` (everything after `botctl admin-key`).
    ///
    /// `key_file_env` is the value of [`KEY_FILE_ENV`], or `None` when
    /// the variable is unset (injected by tests).
    pub fn resolve(argv: &[String], key_file_env: Option<String>) -> Result<Self, AdminKeyError> {
        let default_key_file = key_file_env
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_KEY_FILE));

        let mut iter = argv.iter().peekable();
        let subcommand = match iter.next().map(String::as_str) {
            None | Some("-h") | Some("--help") => return Err(AdminKeyError::Usage(help_text())),
            Some("get") => {
                let key_file = parse_common_flags(&mut iter, default_key_file)?;
                return Ok(Args {
                    command: Command::Get,
                    key_file,
                });
            }
            Some("set") => {
                // `set <key> [--file <path>]`
                let key = iter
                    .next()
                    .ok_or_else(|| {
                        AdminKeyError::InvalidUsage("set requires a <key> argument".into())
                    })?
                    .clone();
                let key_file = parse_common_flags(&mut iter, default_key_file)?;
                return Ok(Args {
                    command: Command::Set(key),
                    key_file,
                });
            }
            Some("generate") => "generate",
            Some(other) => {
                return Err(AdminKeyError::InvalidUsage(format!(
                    "unknown admin-key command '{other}'"
                )));
            }
        };

        // `generate [--force] [--file <path>]`
        assert_eq!(subcommand, "generate");
        let mut force = false;
        let mut key_file = default_key_file;

        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--force" => force = true,
                "--file" => {
                    let v = iter.next().ok_or_else(|| {
                        AdminKeyError::InvalidUsage("--file requires a value".into())
                    })?;
                    key_file = PathBuf::from(v);
                }
                other => {
                    return Err(AdminKeyError::InvalidUsage(format!(
                        "unknown flag '{other}'"
                    )));
                }
            }
        }

        Ok(Args {
            command: Command::Generate { force },
            key_file,
        })
    }

    pub fn from_argv(argv: &[String]) -> Result<Self, AdminKeyError> {
        Self::resolve(argv, std::env::var(KEY_FILE_ENV).ok())
    }
}

/// Parse `--file <path>` flags, returning the resolved path.
fn parse_common_flags(
    iter: &mut std::iter::Peekable<std::slice::Iter<'_, String>>,
    mut key_file: PathBuf,
) -> Result<PathBuf, AdminKeyError> {
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--file" => {
                let v = iter
                    .next()
                    .ok_or_else(|| AdminKeyError::InvalidUsage("--file requires a value".into()))?;
                key_file = PathBuf::from(v);
            }
            other => {
                return Err(AdminKeyError::InvalidUsage(format!(
                    "unknown flag '{other}'"
                )));
            }
        }
    }
    Ok(key_file)
}

pub fn help_text() -> &'static str {
    "Usage: botctl admin-key <COMMAND> [OPTIONS]\n\
     \n\
     Manage the genesis admin credential for botwork-auth-broker.\n\
     \n\
     Commands:\n\
     \x20  get                Print the current admin key to stdout.\n\
     \x20  set <key>          Write <key> to the key file (rotation).\n\
     \x20  generate           Generate a random key; no-op if file exists.\n\
     \x20                     Use --force to overwrite an existing key.\n\
     \n\
     Options (all commands):\n\
     \x20  --file <path>      Key file path.\n\
     \x20                     Default: BOTWORK_ADMIN_KEY_FILE or\n\
     \x20                     /var/lib/botwork/admin.env\n\
     \n\
     Options (generate only):\n\
     \x20  --force            Overwrite existing key (rotation).\n\
     \n\
     Key file format:\n\
     \x20  BOTWORK_ADMIN_API_KEY=<key>\n\
     \n\
     Systemd reads the key file via EnvironmentFile= and injects\n\
     BOTWORK_ADMIN_API_KEY into botwork-auth-broker. The bearer value\n\
     is then usable as Authorization: ****** on admin-gated\n\
     API routes (/api/tenants, /api/plugins, etc.).\n\
     \n\
     Exit codes: 0=ok, 2=usage, 4=file-io"
}

// ── error type ──────────────────────────────────────────────────────

/// Errors emitted by the admin-key subcommand.
#[derive(Debug, Error)]
pub enum AdminKeyError {
    /// Help / usage (exit 0).
    #[error("{0}")]
    Usage(&'static str),
    /// Invalid CLI usage (exit 2).
    #[error("usage: {0}\n\n{help}", help = help_text())]
    InvalidUsage(String),
    /// Key file could not be read (exit 4).
    #[error("failed to read key file {path}: {reason}", path = path.display())]
    FileRead { path: PathBuf, reason: String },
    /// Key file could not be written (exit 4).
    #[error("failed to write key file {path}: {reason}", path = path.display())]
    FileWrite { path: PathBuf, reason: String },
    /// Key file exists but does not contain a valid `BOTWORK_ADMIN_API_KEY=`
    /// entry (exit 4).
    #[error("key file {0} does not contain a BOTWORK_ADMIN_API_KEY= entry")]
    KeyNotFound(PathBuf),
    /// Supplied key value is invalid (exit 2).
    #[error("invalid key: {0}")]
    InvalidKey(String),
}

impl AdminKeyError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 0,
            Self::InvalidUsage(_) | Self::InvalidKey(_) => 2,
            Self::FileRead { .. } | Self::FileWrite { .. } | Self::KeyNotFound(_) => 4,
        }
    }
}

// ── tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    // ── help / usage ────────────────────────────────────────────────

    #[test]
    fn no_args_returns_usage_exit_zero() {
        let err = Args::resolve(&argv(&[]), None).unwrap_err();
        assert!(matches!(err, AdminKeyError::Usage(_)));
        assert_eq!(err.exit_code(), 0);
    }

    #[test]
    fn help_flag_returns_usage_exit_zero() {
        for flag in ["-h", "--help"] {
            let err = Args::resolve(&argv(&[flag]), None).unwrap_err();
            assert!(matches!(err, AdminKeyError::Usage(_)));
            assert_eq!(err.exit_code(), 0, "flag {flag}");
        }
    }

    #[test]
    fn unknown_command_returns_invalid_usage() {
        let err = Args::resolve(&argv(&["frob"]), None).unwrap_err();
        assert!(matches!(err, AdminKeyError::InvalidUsage(_)));
        assert_eq!(err.exit_code(), 2);
    }

    // ── get ─────────────────────────────────────────────────────────

    #[test]
    fn get_parses_correctly() {
        let args = Args::resolve(&argv(&["get"]), None).expect("parse");
        assert!(matches!(args.command, Command::Get));
        assert_eq!(args.key_file, PathBuf::from(DEFAULT_KEY_FILE));
    }

    #[test]
    fn get_with_file_override() {
        let args = Args::resolve(&argv(&["get", "--file", "/tmp/key.env"]), None).expect("parse");
        assert!(matches!(args.command, Command::Get));
        assert_eq!(args.key_file, PathBuf::from("/tmp/key.env"));
    }

    #[test]
    fn get_with_unknown_flag_returns_invalid_usage() {
        let err = Args::resolve(&argv(&["get", "--frobnicate"]), None).unwrap_err();
        assert!(matches!(err, AdminKeyError::InvalidUsage(_)));
    }

    // ── set ─────────────────────────────────────────────────────────

    #[test]
    fn set_parses_key_arg() {
        let args = Args::resolve(&argv(&["set", "my-api-key"]), None).expect("parse");
        assert!(matches!(args.command, Command::Set(ref k) if k == "my-api-key"));
    }

    #[test]
    fn set_missing_key_returns_invalid_usage() {
        let err = Args::resolve(&argv(&["set"]), None).unwrap_err();
        assert!(matches!(err, AdminKeyError::InvalidUsage(_)));
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn set_with_file_override() {
        let args =
            Args::resolve(&argv(&["set", "my-key", "--file", "/tmp/a.env"]), None).expect("parse");
        assert!(matches!(args.command, Command::Set(ref k) if k == "my-key"));
        assert_eq!(args.key_file, PathBuf::from("/tmp/a.env"));
    }

    // ── generate ────────────────────────────────────────────────────

    #[test]
    fn generate_parses_correctly() {
        let args = Args::resolve(&argv(&["generate"]), None).expect("parse");
        assert!(matches!(args.command, Command::Generate { force: false }));
    }

    #[test]
    fn generate_force_flag() {
        let args = Args::resolve(&argv(&["generate", "--force"]), None).expect("parse");
        assert!(matches!(args.command, Command::Generate { force: true }));
    }

    #[test]
    fn generate_unknown_flag_returns_invalid_usage() {
        let err = Args::resolve(&argv(&["generate", "--bad"]), None).unwrap_err();
        assert!(matches!(err, AdminKeyError::InvalidUsage(_)));
    }

    // ── env-var fallback ────────────────────────────────────────────

    #[test]
    fn env_var_sets_default_file() {
        let args =
            Args::resolve(&argv(&["get"]), Some("/env/path/admin.env".to_string())).expect("parse");
        assert_eq!(args.key_file, PathBuf::from("/env/path/admin.env"));
    }

    #[test]
    fn explicit_file_flag_takes_priority_over_env() {
        let args = Args::resolve(
            &argv(&["get", "--file", "/explicit/admin.env"]),
            Some("/env/path/admin.env".to_string()),
        )
        .expect("parse");
        assert_eq!(args.key_file, PathBuf::from("/explicit/admin.env"));
    }

    // ── generate_key ────────────────────────────────────────────────

    #[test]
    fn generate_key_is_unique_and_nonempty() {
        let k1 = generate_key();
        let k2 = generate_key();
        assert!(!k1.is_empty());
        assert_ne!(k1, k2, "two generated keys should differ");
    }

    #[test]
    fn generated_key_is_valid_ascii_no_whitespace() {
        let k = generate_key();
        assert!(
            k.chars()
                .all(|c| c.is_ascii() && !c.is_whitespace() && c != '='),
            "key contains invalid chars: {k}"
        );
    }

    // ── validate_key ────────────────────────────────────────────────

    #[test]
    fn validate_key_accepts_alphanumeric_and_hyphens() {
        assert!(validate_key("abc-123-def").is_ok());
        assert!(validate_key("abc123").is_ok());
    }

    #[test]
    fn validate_key_rejects_empty() {
        let err = validate_key("").unwrap_err();
        assert!(matches!(err, AdminKeyError::InvalidKey(_)));
    }

    #[test]
    fn validate_key_rejects_whitespace() {
        let err = validate_key("key with space").unwrap_err();
        assert!(matches!(err, AdminKeyError::InvalidKey(_)));
    }

    #[test]
    fn validate_key_rejects_equals_sign() {
        let err = validate_key("key=value").unwrap_err();
        assert!(matches!(err, AdminKeyError::InvalidKey(_)));
    }

    // ── parse_key_from_content ──────────────────────────────────────

    #[test]
    fn parse_key_reads_standard_format() {
        let content = "BOTWORK_ADMIN_API_KEY=my-secret-key\n";
        let key = parse_key_from_content(content, Path::new("test.env")).expect("parse");
        assert_eq!(key, "my-secret-key");
    }

    #[test]
    fn parse_key_ignores_comments_and_blank_lines() {
        let content = "# comment\n\nBOTWORK_ADMIN_API_KEY=abc123\n";
        let key = parse_key_from_content(content, Path::new("test.env")).expect("parse");
        assert_eq!(key, "abc123");
    }

    #[test]
    fn parse_key_returns_error_on_missing_entry() {
        let content = "OTHER_VAR=other\n";
        let err = parse_key_from_content(content, Path::new("test.env")).unwrap_err();
        assert!(matches!(err, AdminKeyError::KeyNotFound(_)));
        assert_eq!(err.exit_code(), 4);
    }

    // ── file round-trip ─────────────────────────────────────────────

    #[test]
    fn write_then_read_round_trips_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("admin.env");
        write_key(&path, "test-key-value").expect("write");
        let key = read_key(&path).expect("read");
        assert_eq!(key, "test-key-value");
    }

    #[test]
    fn generate_command_creates_file_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("admin.env");
        let result = cmd_generate(&path, false).expect("generate");
        assert_eq!(result, 0);
        assert!(path.exists());
        // Key should be parseable
        read_key(&path).expect("key readable after generate");
    }

    #[test]
    fn generate_command_is_idempotent_without_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("admin.env");
        write_key(&path, "original-key").expect("seed");
        let result = cmd_generate(&path, false).expect("generate");
        assert_eq!(result, 0);
        // Key should be unchanged
        let key = read_key(&path).expect("read");
        assert_eq!(key, "original-key");
    }

    #[test]
    fn generate_command_with_force_replaces_existing_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("admin.env");
        write_key(&path, "old-key").expect("seed");
        let result = cmd_generate(&path, true).expect("generate");
        assert_eq!(result, 0);
        let key = read_key(&path).expect("read");
        assert_ne!(key, "old-key", "key should have been rotated");
    }

    #[test]
    fn set_command_writes_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("admin.env");
        let result = cmd_set(&path, "explicit-key-123").expect("set");
        assert_eq!(result, 0);
        let key = read_key(&path).expect("read");
        assert_eq!(key, "explicit-key-123");
    }

    #[test]
    fn set_command_rotates_existing_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("admin.env");
        write_key(&path, "old-key").expect("seed");
        cmd_set(&path, "new-key").expect("set");
        let key = read_key(&path).expect("read");
        assert_eq!(key, "new-key");
    }

    // ── exit codes ──────────────────────────────────────────────────

    #[test]
    fn admin_key_error_exit_codes() {
        assert_eq!(AdminKeyError::Usage("").exit_code(), 0);
        assert_eq!(AdminKeyError::InvalidUsage("".into()).exit_code(), 2);
        assert_eq!(AdminKeyError::InvalidKey("".into()).exit_code(), 2);
        assert_eq!(
            AdminKeyError::FileRead {
                path: PathBuf::from("/x"),
                reason: "err".into()
            }
            .exit_code(),
            4
        );
        assert_eq!(
            AdminKeyError::KeyNotFound(PathBuf::from("/x")).exit_code(),
            4
        );
    }

    // ── help text ───────────────────────────────────────────────────

    #[test]
    fn help_text_mentions_all_commands() {
        let text = help_text();
        assert!(text.contains("get"), "{text}");
        assert!(text.contains("set"), "{text}");
        assert!(text.contains("generate"), "{text}");
    }

    #[test]
    fn help_text_mentions_key_file_and_exit_codes() {
        let text = help_text();
        assert!(text.contains(DEFAULT_KEY_FILE), "{text}");
        assert!(text.contains("Exit codes"), "{text}");
    }
}
