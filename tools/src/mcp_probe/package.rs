//! Thin loader wrapping [`botwork_api_core::package::validate_package`]
//! with the io-error layer the CLI needs.
//!
//! The shape here mirrors [`botwork_api_core::config::LoadError`]
//! intentionally — the operator-facing failure modes (file missing,
//! file unreadable, yaml parse error, validation rule failed) are
//! the same set whether they're producing a `bootstrap.yaml` or an
//! `mcp-package.yaml`, and the CLI maps every variant in this enum
//! to exit code 3 ("package load failure") so the operator gets one
//! umbrella code for "fix the file before re-trying".

use std::path::{Path, PathBuf};

use botwork_api_core::package::{validate_package, PackageFileEntry, ValidatedPackage};
use botwork_api_core::ValidationError;
use thiserror::Error;

/// Read + parse + validate an `mcp-package.yaml`.
pub fn load(path: &Path) -> Result<ValidatedPackage, PackageLoadError> {
    if !path.exists() {
        return Err(PackageLoadError::NotFound(path.to_path_buf()));
    }
    let bytes = std::fs::read_to_string(path).map_err(|err| PackageLoadError::Read {
        path: path.to_path_buf(),
        err,
    })?;
    let raw: PackageFileEntry = serde_yaml::from_str(&bytes)?;
    Ok(validate_package(&raw)?)
}

/// Errors emitted while loading + validating a package file.
///
/// All four variants map to exit code 3 ("package load failure")
/// from [`crate::mcp_probe::McpProbeError::exit_code`] — same
/// posture [`botwork_api_core::config::LoadError`] uses for
/// bootstrap.yaml load failures.
#[derive(Debug, Error)]
pub enum PackageLoadError {
    #[error("mcp-package.yaml not found: {0}")]
    NotFound(PathBuf),

    #[error("failed to read mcp-package.yaml {path}: {err}", path = path.display())]
    Read {
        path: PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("failed to parse mcp-package.yaml: {0}")]
    Parse(#[from] serde_yaml::Error),

    #[error(transparent)]
    Validation(#[from] ValidationError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_yaml(yaml: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tmp");
        f.write_all(yaml.as_bytes()).expect("write");
        f
    }

    #[test]
    fn loads_minimal_well_formed_file() {
        let yaml = "name: echo\nisolation: shared\negress: none\nspill:\n  mode: never\n";
        let f = write_yaml(yaml);
        let pkg = load(f.path()).expect("load");
        assert_eq!(pkg.name, "echo");
    }

    #[test]
    fn missing_file_surfaces_not_found() {
        let err = load(Path::new("/nonexistent/mcp-package.yaml")).unwrap_err();
        assert!(matches!(err, PackageLoadError::NotFound(_)));
    }

    #[test]
    fn parse_error_surfaces_parse_variant() {
        let f = write_yaml("not: [valid yaml]: : :");
        let err = load(f.path()).unwrap_err();
        assert!(matches!(err, PackageLoadError::Parse(_)));
    }

    #[test]
    fn validation_error_surfaces_validation_variant() {
        let yaml = "name: NOT-VALID\nisolation: shared\negress: none\nspill:\n  mode: never\n";
        let f = write_yaml(yaml);
        let err = load(f.path()).unwrap_err();
        assert!(matches!(err, PackageLoadError::Validation(_)));
    }
}
