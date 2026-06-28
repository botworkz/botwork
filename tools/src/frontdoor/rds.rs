use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

pub const HOLDING_MARKER: &str = "frontdoor: hello world";

pub const HOLDING_RDS: &str = r#"resources:
- "@type": type.googleapis.com/envoy.config.route.v3.RouteConfiguration
  name: frontdoor_routes
  virtual_hosts:
  - name: frontdoor
    domains: ["*"]
    routes:
    - match:
        prefix: "/"
      response_headers_to_add:
      - header:
          key: content-type
          value: "text/html; charset=utf-8"
        keep_empty_value: false
      direct_response:
        status: 200
        body:
          inline_string: |
            <!doctype html><meta charset=utf-8>
            <title>botwork frontdoor</title>
            <h1>frontdoor: hello world</h1>
            <p>Base holding content. Override by swapping rds/active.yaml.</p>
"#;

pub const INGRESS_RDS: &str = r#"resources:
- "@type": type.googleapis.com/envoy.config.route.v3.RouteConfiguration
  name: frontdoor_routes
  virtual_hosts:
  - name: frontdoor
    domains: ["*"]
    routes:
    - match:
        prefix: "/"
      route:
        cluster: ingress
"#;

/// Write a frontdoor RDS payload via same-filesystem atomic rename.
///
/// This contract is load-bearing for vm 0.6.0 frontdoor FS-xDS: Envoy's
/// file watch subscribes to `IN_MOVED_TO` only, so in-place writes
/// (`sed -i`, `cp`) or symlink swaps (`ln -sf`) do not trigger a reload.
/// The only supported update path is:
///
/// 1. write `<rds-dir>/active.yaml.new`
/// 2. `fsync` + close it
/// 3. `rename(2)` onto `<rds-dir>/active.yaml`
///
/// The temp file must live in the same directory/filesystem. Writing in
/// `/tmp` and copying across mount boundaries degrades into copy+unlink
/// (`IN_CREATE` + `IN_DELETE`), which frontdoor does not consume.
pub fn write_rds(rds_dir: &Path, payload: &str) -> Result<(), RdsError> {
    let new_path = rds_dir.join("active.yaml.new");
    let active_path = rds_dir.join("active.yaml");

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&new_path)
        .map_err(|source| RdsError::WriteOpen {
            path: new_path.clone(),
            source,
        })?;
    file.write_all(payload.as_bytes())
        .map_err(|source| RdsError::Write {
            path: new_path.clone(),
            source,
        })?;
    file.sync_all().map_err(|source| RdsError::Fsync {
        path: new_path.clone(),
        source,
    })?;
    drop(file);

    std::fs::rename(&new_path, &active_path).map_err(|source| RdsError::Rename {
        source_path: new_path,
        destination: active_path,
        source_error: source,
    })?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum RdsError {
    #[error("failed to open '{path}' for write: {source}")]
    WriteOpen {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write '{path}': {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to fsync '{path}': {source}")]
    Fsync {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to rename '{source_path}' -> '{destination}': {source_error}")]
    Rename {
        source_path: PathBuf,
        destination: PathBuf,
        source_error: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{write_rds, HOLDING_MARKER, HOLDING_RDS, INGRESS_RDS};

    #[test]
    fn holding_rds_contains_pinned_route_name() {
        assert!(HOLDING_RDS.contains("name: frontdoor_routes"));
    }

    #[test]
    fn ingress_rds_contains_pinned_route_name() {
        assert!(INGRESS_RDS.contains("name: frontdoor_routes"));
    }

    #[test]
    fn holding_rds_contains_holding_marker() {
        assert!(HOLDING_RDS.contains(HOLDING_MARKER));
    }

    #[test]
    fn write_rds_renames_atomically() {
        let dir = tempdir().expect("tempdir");
        write_rds(dir.path(), HOLDING_RDS).expect("write");

        let active = dir.path().join("active.yaml");
        let new = dir.path().join("active.yaml.new");
        assert!(active.exists(), "active.yaml must exist");
        assert_eq!(fs::read_to_string(active).expect("read"), HOLDING_RDS);
        assert!(!new.exists(), "active.yaml.new must be gone after rename");
    }

    #[test]
    fn write_rds_overwrites_existing_active_yaml() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join("active.yaml"), "garbage").expect("seed");
        write_rds(dir.path(), INGRESS_RDS).expect("write");

        assert_eq!(
            fs::read_to_string(dir.path().join("active.yaml")).expect("read"),
            INGRESS_RDS
        );
    }
}
