//! Rail: `testcontainers` only appears under `[dev-dependencies]`.
//!
//! Per RFE 97, `testcontainers` and `testcontainers-modules` MUST never
//! appear as a regular `[dependencies]` of any workspace crate — doing so
//! would link the docker-runner into a production binary. The crate
//! contracts state this in their `Cargo.toml` comments; this test enforces
//! it.
//!
//! Implementation: parse each member's `Cargo.toml` directly (string-level
//! section detection, no `toml` crate dependency on the rail itself) and
//! assert the offending crate names appear only under `[dev-dependencies]`.

use std::path::{Path, PathBuf};

const PROHIBITED_RUNTIME_CRATES: &[&str] = &["testcontainers", "testcontainers-modules"];

#[test]
fn testcontainers_is_dev_only() {
    let workspace_root = workspace_root();
    let workspace_toml = workspace_root.join("Cargo.toml");
    let members = workspace_members(&workspace_toml);
    assert!(
        !members.is_empty(),
        "workspace has no members — Cargo.toml parse went wrong"
    );

    let mut offenders: Vec<String> = Vec::new();
    for member in members {
        let manifest = workspace_root.join(&member).join("Cargo.toml");
        let raw = match std::fs::read_to_string(&manifest) {
            Ok(c) => c,
            Err(err) => {
                panic!(
                    "could not read {} for testcontainers rail: {err}",
                    manifest.display()
                );
            }
        };
        for section in regular_dep_sections(&raw) {
            for crate_name in PROHIBITED_RUNTIME_CRATES {
                if section_mentions_crate(&section.body, crate_name) {
                    offenders.push(format!(
                        "{} [{}] mentions `{}`",
                        manifest.display(),
                        section.header,
                        crate_name
                    ));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "\n\
         RFE 97 rail violation: testcontainers must only appear under\n\
         [dev-dependencies], never [dependencies] or any target-specific\n\
         dependency table:\n\n  {}\n",
        offenders.join("\n  ")
    );
}

fn workspace_root() -> PathBuf {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(Path::to_path_buf)
        .expect("workspace root is two levels above db/migration/")
}

fn workspace_members(workspace_toml: &Path) -> Vec<String> {
    let raw = std::fs::read_to_string(workspace_toml)
        .unwrap_or_else(|err| panic!("read {}: {err}", workspace_toml.display()));
    // Cheap parse: find the `[workspace]` section, scan for `members = [...]`
    // and split out the quoted strings inside. We avoid a TOML dep here so
    // the rail stays insulated from upstream version churn.
    let mut in_workspace = false;
    let mut collected = String::new();
    let mut capturing_members = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_workspace = trimmed == "[workspace]";
            capturing_members = false;
            continue;
        }
        if !in_workspace {
            continue;
        }
        if trimmed.starts_with("members") {
            capturing_members = true;
        }
        if capturing_members {
            collected.push_str(line);
            collected.push('\n');
            if trimmed.ends_with(']') {
                break;
            }
        }
    }

    let mut members = Vec::new();
    let mut in_quote = false;
    let mut current = String::new();
    for c in collected.chars() {
        match c {
            '"' if !in_quote => {
                in_quote = true;
                current.clear();
            }
            '"' if in_quote => {
                in_quote = false;
                if !current.is_empty() {
                    members.push(std::mem::take(&mut current));
                }
            }
            other if in_quote => current.push(other),
            _ => {}
        }
    }
    members
}

struct Section<'a> {
    header: &'a str,
    body: String,
}

/// Iterate every `[dependencies]` / `[target.<cfg>.dependencies]` section in
/// the manifest. `[dev-dependencies]` and `[build-dependencies]` are
/// **excluded** — those are the legitimate homes for testcontainers.
fn regular_dep_sections(raw: &str) -> Vec<Section<'_>> {
    let mut out: Vec<Section<'_>> = Vec::new();
    let mut current: Option<(&str, String)> = None;
    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(header) = trimmed
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            if let Some((h, body)) = current.take() {
                out.push(Section { header: h, body });
            }
            if is_runtime_dep_header(header) {
                current = Some((header, String::new()));
            }
            continue;
        }
        if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some((h, body)) = current {
        out.push(Section { header: h, body });
    }
    out
}

fn is_runtime_dep_header(header: &str) -> bool {
    // `dependencies` and `target.<cfg>.dependencies` are runtime.
    // `dev-dependencies` / `build-dependencies` / their target.* variants
    // are not.
    let last = header.split('.').next_back().unwrap_or(header);
    last == "dependencies"
}

fn section_mentions_crate(body: &str, crate_name: &str) -> bool {
    // Two shapes:
    //   testcontainers = "0.27"
    //   testcontainers = { version = "0.27", ... }
    // Both start with `<name> = ` at line-start (modulo indent).
    body.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed
            .strip_prefix(crate_name)
            .map(|rest| rest.trim_start().starts_with('='))
            .unwrap_or(false)
    })
}
