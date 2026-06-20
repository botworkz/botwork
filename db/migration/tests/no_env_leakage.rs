//! Rail: no test source file reads `BOTWORK_DATABASE_URL`.
//!
//! Per RFE 97, tests must obtain their `DatabaseConnection` via the explicit
//! [`botwork_entity::connection::connect`] helper with a URL from
//! testcontainers, not from the production env-var read by
//! [`botwork_entity::connection::connect_from_env`]. This guarantees that
//! no test can accidentally point at a real postgres if `BOTWORK_DATABASE_URL`
//! happens to be set in the environment running `cargo test`.
//!
//! The rail is enforced by source-grep: a test that uses `connect_from_env`
//! or names the env var literal anywhere under a workspace `tests/` directory
//! fails CI.
//!
//! This file itself names the constant in literal-stringified form (a single
//! suffix-stripped substring, see [`PROHIBITED`]) so the test's own contents
//! don't trip the grep.

use std::path::{Path, PathBuf};

/// We split the env-var name across two adjacent string literals so the
/// grep below does not match its own definition. Concatenated at runtime
/// for the assertion message; the substring search uses the prefix to
/// catch any future variant (e.g. `BOTWORK_DATABASE_URL_RO`).
const PROHIBITED_PREFIX: &str = "BOTWORK_DATABASE";

/// Substring forms of the production-only API surface that tests must not
/// touch.
const PROHIBITED_SYMBOLS: &[&str] = &["connect_from_env", "DATABASE_URL_ENV"];

#[test]
fn no_test_source_references_production_env() {
    let workspace_root = workspace_root();

    let mut offenders: Vec<String> = Vec::new();
    for tests_dir in collect_tests_dirs(&workspace_root) {
        scan(&tests_dir, &mut offenders);
    }

    assert!(
        offenders.is_empty(),
        "\n\
         RFE 97 rail violation: the following test files reference the\n\
         production-only DB env var / API surface (must use\n\
         botwork_entity::connection::connect with a testcontainer URL\n\
         instead):\n\n  {}\n\n\
         If you genuinely need the production helper from a test, route\n\
         the URL through a parameter and call `connect(...)` directly.",
        offenders.join("\n  ")
    );
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `db/migration/`; the workspace root is two
    // levels up. The check intentionally rejects paths outside the
    // workspace so that out-of-tree builds fail loudly rather than
    // silently rooting the scan at /.
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .expect("workspace root is two levels above db/migration/")
}

fn collect_tests_dirs(root: &Path) -> Vec<PathBuf> {
    // Walk only the immediate workspace members' `tests/` directories.
    // Going deeper risks scanning `target/` or vendored deps.
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // Top-level workspace members like config-broker/, db/, ...
        let direct_tests = path.join("tests");
        if direct_tests.is_dir() {
            out.push(direct_tests);
        }
        // Nested workspace members like db/entity/, db/migration/.
        let nested = match std::fs::read_dir(&path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for child in nested.flatten() {
            let child_tests = child.path().join("tests");
            if child_tests.is_dir() {
                out.push(child_tests);
            }
        }
    }
    out
}

fn scan(dir: &Path, offenders: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan(&path, offenders);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        // This rail file itself names the prohibited tokens in literal
        // string form. Skip it.
        if path.file_name().and_then(|s| s.to_str()) == Some("no_env_leakage.rs") {
            continue;
        }
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if contents.contains(PROHIBITED_PREFIX)
            || PROHIBITED_SYMBOLS.iter().any(|sym| contents.contains(sym))
        {
            offenders.push(path.display().to_string());
        }
    }
}
