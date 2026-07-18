//! Shared version-string formatter for the botwork ecosystem.
//!
//! This crate does NOT pin the release version itself. Each consumer
//! supplies its own version string (typically via `include_str!` on
//! the consumer's own `/VERSION` file at the workspace root). The
//! crate provides:
//!
//! * [`VERSION`] — compile-time pin of the repository root `/VERSION`.
//! * [`GIT_SHA`] — compile-time pin of `BOTWORK_GIT_SHA`. Common to
//!   every consumer in the ecosystem because the env var name is the
//!   shared contract CI sets on every build.
//! * [`format_full`] — the canonical `<VERSION>[+<short-sha>]` formatter.
//! * [`version_string`] — canonical formatted version for this repository.
//!
//! ## Usage from a consumer's `main.rs`
//!
//! ```ignore
//! const VERSION: &str = include_str!("../../VERSION").trim_ascii();
//!
//! fn version_string() -> String {
//!     botwork_version::format_full(VERSION, botwork_version::GIT_SHA)
//! }
//! ```
//!
//! ## Formatter contract
//!
//! Dev/pre-release versions (anything containing a `-`, e.g.
//! `0.4.0-dev`, `1.0.0-rc1`) get `+<short-sha>` appended when
//! `GIT_SHA` is non-empty. Clean releases (e.g. `0.3.15`) and any
//! version with an empty `GIT_SHA` return the bare version. The
//! short sha is the first 7 characters of `GIT_SHA`.
//!
//! This matches the [semver build metadata](https://semver.org/#spec-item-10)
//! shape, and the dev-vs-clean predicate is identical to the one
//! `.github/workflows/ci.yml`'s publish gate uses (`*-*`).

/// Repository version baked in at compile time from `/VERSION`.
pub const VERSION: &str = include_str!("../../VERSION").trim_ascii();

/// Git sha baked in at build time via `BOTWORK_GIT_SHA`. Empty when
/// the env var was unset at compile time (local-dev builds).
pub const GIT_SHA: &str = match option_env!("BOTWORK_GIT_SHA") {
    Some(s) => s,
    None => "",
};

/// Canonical `<VERSION>[+<short-sha>]` formatter — see module docs.
///
/// `version` is the consumer-supplied release string (typically from
/// `include_str!` on the consumer's `/VERSION`). `git_sha` is
/// typically [`GIT_SHA`] but is parameterised so callers and tests
/// can drive it explicitly.
pub fn format_full(version: &str, git_sha: &str) -> String {
    let is_dev = version.contains('-');
    if is_dev && !git_sha.is_empty() {
        let short_sha: String = git_sha.chars().take(7).collect();
        format!("{version}+{short_sha}")
    } else {
        // Clean release OR no sha: bare version. We never append a
        // sha to a clean release — the release VERSION is the
        // identity, and the same VERSION rendering differently
        // across rebuilds defeats the point.
        version.to_string()
    }
}

/// Canonical version string for botwork binaries.
pub fn version_string() -> String {
    format_full(VERSION, GIT_SHA)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_with_sha_appends_short_sha() {
        assert_eq!(format_full("0.4.0-dev", "abcdef0123"), "0.4.0-dev+abcdef0");
        assert_eq!(
            format_full("1.0.0-rc1", "1234567abcdef"),
            "1.0.0-rc1+1234567"
        );
    }

    #[test]
    fn clean_release_never_appends_sha() {
        assert_eq!(format_full("0.3.15", "abcdef0123"), "0.3.15");
        assert_eq!(format_full("1.0.0", "deadbeef"), "1.0.0");
    }

    #[test]
    fn dev_without_sha_is_bare() {
        assert_eq!(format_full("0.4.0-dev", ""), "0.4.0-dev");
    }

    #[test]
    fn clean_release_without_sha_is_bare() {
        assert_eq!(format_full("0.3.15", ""), "0.3.15");
    }

    #[test]
    fn short_sha_uses_all_of_it_when_under_seven_chars() {
        assert_eq!(format_full("0.4.0-dev", "abc"), "0.4.0-dev+abc");
    }

    #[test]
    fn version_const_is_non_empty() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn dev_predicate_matches_publish_gate() {
        // Pin against ci.yml's `[[ "${raw}" == *-* ]]` predicate.
        // The crate's dev detection is `version.contains('-')`; same
        // semantics, both must stay in lockstep.
        for dev in ["0.4.0-dev", "0.4.0-rc1", "0.4.0-beta2", "1.0.0-alpha.1"] {
            assert_eq!(
                format_full(dev, "abcdef0"),
                format!("{dev}+abcdef0"),
                "dev predicate failed for {dev}"
            );
        }
        for clean in ["0.4.0", "1.2.3", "10.20.30"] {
            assert_eq!(
                format_full(clean, "abcdef0"),
                clean.to_string(),
                "clean-release predicate failed for {clean}"
            );
        }
    }
}
