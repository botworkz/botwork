//! Compile-time pin of the repo's VERSION file. Single source of truth
//! for every binary's `--version` output, every startup log line, and
//! every protocol `clientInfo.version` slot.
//!
//! The release version lives in /VERSION at the workspace root. This
//! crate is the only consumer of that file via include_str!, so a stale
//! per-crate Cargo.toml version (everything in the workspace ships as
//! `version = "0.0.0"`) cannot drift from the release.
//!
//! GIT_SHA is sourced from BOTWORK_GIT_SHA at compile time. It's empty
//! in local-dev builds; CI populates it from $GITHUB_SHA via the
//! container builds + release-binaries job. On a dev/pre-release
//! VERSION (anything containing a `-`, e.g. `0.4.0-dev`) `full()`
//! emits `<VERSION>+<short-sha>`; on a clean release it emits bare
//! `<VERSION>`.

/// Raw contents of /VERSION at compile time. May contain a trailing
/// newline; consumers should use [`VERSION`] (the trimmed form).
const VERSION_RAW: &str = include_str!("../../VERSION");

/// Release version (trimmed contents of /VERSION). E.g. "0.3.15" or
/// "0.4.0-dev".
pub const VERSION: &str = VERSION_RAW.trim_ascii();

/// Git sha baked in at build time via BOTWORK_GIT_SHA. Empty in local
/// builds where the env var is unset at compile time.
pub const GIT_SHA: &str = match option_env!("BOTWORK_GIT_SHA") {
    Some(s) => s,
    None => "",
};

/// Canonical one-liner for `--version` output and startup log lines.
/// On a dev/pre-release VERSION (contains `-`), returns
/// `"<VERSION>+<short-sha>"` when GIT_SHA is non-empty. Otherwise
/// returns bare `"<VERSION>"`.
pub fn full() -> String {
    format_full(VERSION, GIT_SHA)
}

fn format_full(version: &str, git_sha: &str) -> String {
    let is_dev = version.contains('-');
    if is_dev && !git_sha.is_empty() {
        let short_sha: String = git_sha.chars().take(7).collect();
        format!("{version}+{short_sha}")
    } else {
        // Clean release: NEVER append sha. The release VERSION is the
        // identity; appending a sha would make the same VERSION render
        // differently across rebuilds, which defeats the point.
        //
        // Local-dev (BOTWORK_GIT_SHA unset): also bare. We don't have a
        // sha to append, and `0.4.0-dev` on a developer laptop is fine
        // as-is.
        version.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_trimmed_and_non_empty() {
        assert!(!VERSION.is_empty(), "VERSION must not be empty");
        assert_eq!(VERSION, VERSION.trim(), "VERSION must be trimmed");
        assert!(
            !VERSION.contains('\n'),
            "VERSION must not contain a newline"
        );
    }

    #[test]
    fn full_without_git_sha_is_just_version() {
        if GIT_SHA.is_empty() {
            assert_eq!(full(), VERSION);
        }
    }

    #[test]
    fn full_with_git_sha_uses_short_form() {
        assert_eq!(format_full("0.4.0-dev", "abcdef0123"), "0.4.0-dev+abcdef0");
        assert_eq!(format_full("0.3.15", "abcdef0123"), "0.3.15");
        assert_eq!(format_full("0.4.0-dev", ""), "0.4.0-dev");
        assert_eq!(format_full("0.3.15", ""), "0.3.15");
        assert_eq!(format_full("0.4.0-dev", "abc"), "0.4.0-dev+abc");
    }

    #[test]
    fn dev_version_predicate_uses_hyphen() {
        // Pin the same predicate the ci.yml publish gate uses:
        // pre-release/dev versions contain a hyphen.
        assert!("0.4.0-dev".contains('-'));
        assert!("0.4.0-rc1".contains('-'));
        assert!("0.4.0-beta2".contains('-'));
        assert!(!"0.4.0".contains('-'));
        assert!(!"0.3.15".contains('-'));
    }
}
