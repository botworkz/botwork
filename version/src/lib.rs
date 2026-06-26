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
//! in local-dev builds; the CI wiring to populate it lives in a
//! follow-up PR (Dockerfiles + _crate.yml), at which point `full()`
//! starts emitting "0.3.15 (sha 1a2b3c4)" without any source edits.

/// Raw contents of /VERSION at compile time. May contain a trailing
/// newline; consumers should use [`VERSION`] (the trimmed form).
const VERSION_RAW: &str = include_str!("../../VERSION");

/// Release version (trimmed contents of /VERSION). E.g. "0.3.15" or
/// "0.4.0-dev".
pub const VERSION: &str = VERSION_RAW.trim_ascii();

/// Git sha baked in at build time via BOTWORK_GIT_SHA. Empty when
/// the env var was unset at compile time.
pub const GIT_SHA: &str = match option_env!("BOTWORK_GIT_SHA") {
    Some(s) => s,
    None => "",
};

/// Canonical one-liner for `--version` output and startup log lines.
/// Returns `"<VERSION>"` when GIT_SHA is empty, otherwise
/// `"<VERSION> (sha <short>)"` where `<short>` is the first 7 chars
/// of GIT_SHA (or the whole thing if shorter).
pub fn full() -> String {
    if GIT_SHA.is_empty() {
        VERSION.to_string()
    } else {
        let short_len = GIT_SHA.len().min(7);
        format!("{VERSION} (sha {})", &GIT_SHA[..short_len])
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
        let short_len = "abcdef0123".len().min(7);
        let formatted = format!("{VERSION} (sha {})", &"abcdef0123"[..short_len]);
        assert_eq!(formatted, format!("{VERSION} (sha abcdef0)"));
    }
}
