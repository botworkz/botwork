//! Tenant, workspace, and plugin name validation.
//!
//! # Canonical source
//!
//! **DO NOT EDIT the grammar constants or validation logic independently.**
//! This file is vendored from `botwork-extra/auth-broker/src/grammar.rs`.
//! When `botwork-extra`'s auth-broker crate is available as a Cargo
//! dependency (private repo), replace the body of this module with:
//!
//! ```rust,ignore
//! pub use botwork_auth_broker::grammar::{
//!     RESERVED_TENANT_NAMES,
//!     NAME_REGEX,
//!     validate_tenant_name,
//!     validate_workspace_name,
//!     validate_plugin_name,
//!     normalise_name,
//!     NameError,
//! };
//! ```
//!
//! Until then, keep any edits in lockstep with the authoritative copy in
//! `botwork-extra`. Single source of truth is auth-broker; divergence
//! causes the silent wrong-tenant bug this reshape was designed to prevent.
//!
//! # Grammar
//!
//! * **Regex:** `^[A-Za-z0-9_-]{1,63}$`
//! * **Case-sensitive** storage (`Phlax` ≠ `phlax`).
//! * **Normalised-unique** — `normalise_name` lowercases for the
//!   uniqueness check so `Phlax` blocks `phlax` / `PHLAX` from being
//!   created by a different operator.
//! * **Same regex** for tenants, workspaces, and plugins.
//!   Distinct reserved-name lists per scope (future).
//!
//! # Reserved names (tenant scope, v1)
//!
//! `["admin", "api", "auth", "static", "stats", "logs"]`
//!
//! Anything that does not match the regex above is implicitly in
//! system-URL-space (e.g. `.well-known`, `@@foo`, names containing `.`
//! or `@`). No explicit carve-out needed — the regex IS the rule.

use std::sync::OnceLock;

use regex::Regex;
use thiserror::Error;

/// The regex every valid name must satisfy.
///
/// Maximum 63 characters, ASCII alphanumeric plus `_` and `-`. No leading-
/// character constraint (a digit or `_` or `-` at position 0 is permitted).
pub const NAME_REGEX_STR: &str = r"^[A-Za-z0-9_-]{1,63}$";

/// Compiled form of [`NAME_REGEX_STR`]; initialised once per process.
pub fn name_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(NAME_REGEX_STR).expect("NAME_REGEX_STR is a valid regex"))
}

/// Names that are unconditionally reserved at **tenant** scope.
///
/// Creating a tenant whose `normalise_name(name)` appears in this slice
/// returns [`NameError::Reserved`].
///
/// Workspaces and plugins share the regex but currently have no entries in
/// their reserved lists; distinct lists may grow in a future version.
pub const RESERVED_TENANT_NAMES: &[&str] =
    &["admin", "api", "auth", "static", "stats", "logs"];

/// Errors returned by the name-validation functions.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NameError {
    /// The name does not match `^[A-Za-z0-9_-]{1,63}$`.
    #[error(
        "invalid name {name:?}: must match ^[A-Za-z0-9_-]{{1,63}}$ \
         (max 63 chars, ASCII alphanumeric / underscore / hyphen)"
    )]
    InvalidFormat { name: String },

    /// The normalised form of the name appears in the reserved list for
    /// this scope.
    #[error("name {name:?} is reserved and cannot be used for this resource")]
    Reserved { name: String },
}

/// Validate a proposed **tenant** name.
///
/// Checks the regex then the reserved-name list (case-insensitive via
/// `normalise_name`). Returns the name unchanged on success; the caller
/// stores whatever capitalisation the operator provided.
pub fn validate_tenant_name(name: &str) -> Result<(), NameError> {
    validate_format(name)?;
    let normalised = normalise_name(name);
    if RESERVED_TENANT_NAMES.iter().any(|&r| r == normalised) {
        return Err(NameError::Reserved {
            name: name.to_string(),
        });
    }
    Ok(())
}

/// Validate a proposed **workspace** name.
///
/// Checks the regex only. Workspace reserved-name lists may grow in future
/// versions; the function signature is already at parity with the
/// tenant variant so call-sites don't need to change.
pub fn validate_workspace_name(name: &str) -> Result<(), NameError> {
    validate_format(name)
}

/// Validate a proposed **plugin** name.
///
/// Checks the regex only. Plugin reserved-name lists may grow in future
/// versions; the function signature is already at parity with the
/// tenant variant so call-sites don't need to change.
pub fn validate_plugin_name(name: &str) -> Result<(), NameError> {
    validate_format(name)
}

/// Normalise a name for uniqueness checks.
///
/// Returns the ASCII-lowercased form. The normalised value is used to check
/// for case-insensitive collisions: if `normalise_name(existing) ==
/// normalise_name(proposed)` then the two names collide and the proposed
/// name must be rejected.
///
/// The stored value is always the operator-supplied original (case preserved).
pub fn normalise_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

// ── internal ──────────────────────────────────────────────────────────────

fn validate_format(name: &str) -> Result<(), NameError> {
    if name_regex().is_match(name) {
        Ok(())
    } else {
        Err(NameError::InvalidFormat {
            name: name.to_string(),
        })
    }
}

// ── tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── format validation ─────────────────────────────────────────────────

    #[test]
    fn accepts_simple_lowercase() {
        assert!(validate_tenant_name("phlax").is_ok());
        assert!(validate_workspace_name("mcp").is_ok());
        assert!(validate_plugin_name("mcp-bash").is_ok());
    }

    #[test]
    fn accepts_mixed_case_and_digits_and_symbols() {
        assert!(validate_tenant_name("Phlax").is_ok());
        assert!(validate_tenant_name("tenant-1").is_ok());
        assert!(validate_tenant_name("tenant_2").is_ok());
        assert!(validate_tenant_name("ABC123").is_ok());
    }

    #[test]
    fn accepts_exactly_63_chars() {
        let name = "a".repeat(63);
        assert!(validate_tenant_name(&name).is_ok());
    }

    #[test]
    fn rejects_64_chars() {
        let name = "a".repeat(64);
        assert_eq!(
            validate_tenant_name(&name),
            Err(NameError::InvalidFormat { name: name.clone() })
        );
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(
            validate_tenant_name(""),
            Err(NameError::InvalidFormat {
                name: "".to_string()
            })
        );
    }

    #[test]
    fn rejects_dot() {
        assert_eq!(
            validate_tenant_name("foo.bar"),
            Err(NameError::InvalidFormat {
                name: "foo.bar".to_string()
            })
        );
    }

    #[test]
    fn rejects_at() {
        assert_eq!(
            validate_tenant_name("@@foo"),
            Err(NameError::InvalidFormat {
                name: "@@foo".to_string()
            })
        );
    }

    #[test]
    fn rejects_space() {
        assert_eq!(
            validate_tenant_name("foo bar"),
            Err(NameError::InvalidFormat {
                name: "foo bar".to_string()
            })
        );
    }

    #[test]
    fn rejects_slash() {
        assert_eq!(
            validate_tenant_name("foo/bar"),
            Err(NameError::InvalidFormat {
                name: "foo/bar".to_string()
            })
        );
    }

    // ── reserved names (tenant scope) ──────────────────────────────────────

    #[test]
    fn rejects_all_reserved_lowercase() {
        for &r in RESERVED_TENANT_NAMES {
            assert_eq!(
                validate_tenant_name(r),
                Err(NameError::Reserved {
                    name: r.to_string()
                }),
                "expected '{r}' to be reserved"
            );
        }
    }

    #[test]
    fn rejects_reserved_in_any_case() {
        assert_eq!(
            validate_tenant_name("Admin"),
            Err(NameError::Reserved {
                name: "Admin".to_string()
            })
        );
        assert_eq!(
            validate_tenant_name("ADMIN"),
            Err(NameError::Reserved {
                name: "ADMIN".to_string()
            })
        );
        assert_eq!(
            validate_tenant_name("API"),
            Err(NameError::Reserved {
                name: "API".to_string()
            })
        );
        assert_eq!(
            validate_tenant_name("Auth"),
            Err(NameError::Reserved {
                name: "Auth".to_string()
            })
        );
        assert_eq!(
            validate_tenant_name("STATIC"),
            Err(NameError::Reserved {
                name: "STATIC".to_string()
            })
        );
    }

    #[test]
    fn workspace_and_plugin_not_blocked_by_tenant_reserved() {
        // "api" is reserved for tenants but fine for workspaces/plugins.
        assert!(validate_workspace_name("api").is_ok());
        assert!(validate_plugin_name("api").is_ok());
        assert!(validate_workspace_name("admin").is_ok());
        assert!(validate_plugin_name("static").is_ok());
    }

    // ── normalise_name ─────────────────────────────────────────────────────

    #[test]
    fn normalise_lowercases_ascii() {
        assert_eq!(normalise_name("Phlax"), "phlax");
        assert_eq!(normalise_name("ADMIN"), "admin");
        assert_eq!(normalise_name("mcp-Bash"), "mcp-bash");
        assert_eq!(normalise_name("mcp"), "mcp");
    }
}
