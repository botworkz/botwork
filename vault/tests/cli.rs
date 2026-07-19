//! `botwork-vault` CLI smoke test.
//!
//! The CLI routes state-mutating subcommands through the auth-broker's
//! `GET /auth/lease/wrapped-export-key` endpoint to resolve the wrapped
//! export_key. End-to-end CLI coverage that exercises a live broker
//! lives in `auth-broker`'s
//! docker-gated `opaque_e2e` suite — see
//! `auth-broker/tests/opaque_e2e.rs::vault_v4_round_trip_via_cli`.
//!
//! What we pin here without a live broker:
//!
//! - `botwork-vault --help` parses and exits cleanly.
//! - Subcommands that need a bearer fail fast with the
//!   "missing bearer" diagnostic when `BOTWORK_BEARER` is not set.

use assert_cmd::Command;
use predicates::prelude::*;

fn vault_cmd() -> Command {
    let mut cmd = Command::cargo_bin("botwork-vault").unwrap();
    cmd.env_remove("BOTWORK_BEARER");
    cmd
}

#[test]
fn help_exits_cleanly() {
    vault_cmd()
        .args(["--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Manage botwork secret vaults"));
}

#[test]
fn init_help_matches_current_surface() {
    vault_cmd()
        .args(["init", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--force"))
        .stdout(predicate::str::contains("--yes-really-overwrite"))
        .stdout(predicate::str::contains("--from-lease").not());
}

#[test]
fn list_without_bearer_exits_with_diagnostic() {
    vault_cmd()
        .args(["list", "--root", "/tmp/nonexistent-root-for-cli-smoke"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("missing bearer"));
}

#[test]
fn get_without_bearer_exits_with_diagnostic() {
    vault_cmd()
        .args([
            "get",
            "--root",
            "/tmp/nonexistent-root-for-cli-smoke",
            "--service",
            "svc",
            "--name",
            "n",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("missing bearer"));
}
