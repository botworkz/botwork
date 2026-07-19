pub mod contents;
pub mod error;
pub mod kdf;
pub mod path;
pub mod public;

mod atomic;
mod lock;
mod vault;

pub(crate) const PREFIX: &str = "[vault]";

pub use contents::{
    DecryptedSecret, SecretEntry, SecretKey, SecretKind, SecretMeta, VaultContents,
};
pub use error::VaultError;
pub use path::{validate_name, validate_service};
pub use public::{PublicStore, SshEntry};
pub use vault::{UnlockedMasterKey, Vault};
