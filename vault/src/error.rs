use std::path::PathBuf;

#[derive(thiserror::Error, Debug)]
pub enum VaultError {
    #[error("vault is locked")]
    Locked,
    #[error("invalid vault key material")]
    Auth,
    #[error("vault metadata missing under {0}")]
    NotInitialized(PathBuf),
    #[error("vault root already exists and is non-empty: {0}")]
    AlreadyInitialized(PathBuf),
    #[error("secret not found: {0}/{1}")]
    SecretNotFound(String, String),
    #[error("integrity check failed: {0}")]
    Integrity(String),
    /// Loaded a vault file whose on-disk versioning bytes are not
    /// supported by this build.
    #[error("vault at {path} is in an unsupported format")]
    UnsupportedVersion {
        /// The vault file (typically `<root>/vault.botwork`) that
        /// triggered the rejection.
        path: PathBuf,
    },
    #[error("unsafe path component: {0}")]
    InvalidComponent(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Codec(String),
    #[error("vault write conflict: generation mismatch (expected {expected}, on-disk {found}); reload and retry")]
    Conflict { expected: u64, found: u64 },
    #[error("{0}")]
    PublicStore(String),
}
