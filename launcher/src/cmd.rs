use std::process::Command;

use tracing::{info, warn};

use crate::config::PREFIX;

#[derive(Debug)]
pub struct CommandOutput {
    pub returncode: i32,
    #[allow(dead_code)]
    pub stdout: String,
    pub stderr: String,
}

pub fn log_info(message: &str) {
    info!("{PREFIX} {message}");
}

pub fn log_warn(message: &str) {
    warn!("{PREFIX} [warn] {message}");
}

/// Returns `true` if `name` (the left-hand side of a `NAME=VALUE` argv element)
/// looks like it might carry a secret and its value should be redacted in logs.
fn is_secret_name(name: &str) -> bool {
    if name.starts_with("BOTWORK_SECRET_") {
        return true;
    }
    // Suffix-based match catches `_PAT` itself (and other common shapes).
    // The numbered-variant branch below handles `_PAT1`, `_PAT2`, etc.
    for suffix in [
        "_TOKEN",
        "_PAT",
        "_SECRET",
        "_KEY",
        "_PASSWORD",
        "_PASS",
        "_API_KEY",
    ] {
        if name.ends_with(suffix) {
            return true;
        }
    }
    // _PAT[0-9]+  — e.g. MY_PAT1, MY_PAT2 (bare _PAT is caught above)
    if let Some(idx) = name.rfind("_PAT") {
        let after = &name[idx + 4..];
        if !after.is_empty() && after.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    false
}

/// Returns `true` if `value` looks like a standalone bearer / API token that
/// should be redacted even when it appears as a bare positional argument.
fn is_token_value(value: &str) -> bool {
    // GitHub PAT / OAuth family: ghp_, gho_, ghr_, ghs_, ghu_
    for prefix in ["ghp_", "gho_", "ghr_", "ghs_", "ghu_"] {
        if let Some(rest) = value.strip_prefix(prefix) {
            if rest.len() >= 20 && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return true;
            }
        }
    }
    // sk-... shape (OpenAI and similar API keys)
    if let Some(rest) = value.strip_prefix("sk-") {
        if rest.len() >= 20
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return true;
        }
    }
    // Slack: xoxa-, xoxb-, xoxp-, xoxr-, xoxs-
    for prefix in ["xoxa-", "xoxb-", "xoxp-", "xoxr-", "xoxs-"] {
        if let Some(rest) = value.strip_prefix(prefix) {
            if rest.len() >= 10 && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                return true;
            }
        }
    }
    false
}

/// Renders a secret value for logging.
///
/// For values shorter than 16 bytes the prefix would leak too much of the
/// secret (e.g. an 8-byte key would expose 75% of its bytes), so we redact
/// wholesale.  For longer values a 6-char prefix is preserved to aid
/// cross-log correlation, mirroring `session-broker::redact_token`.
///
/// Length is measured in bytes; secrets in real-world token formats are
/// ASCII, so this matches char count in practice.  A hypothetical multi-byte
/// UTF-8 secret with `len() >= 16` could see all of its chars logged via the
/// prefix, but no known token format exposes this in practice.
fn redact_value(value: &str) -> String {
    if value.len() < 16 {
        return "<redacted>".to_string();
    }
    let prefix: String = value.chars().take(6).collect();
    format!("<redacted:{prefix}…>")
}

/// Redacts secret-looking values in `args` and joins them into a single
/// space-separated string suitable for logging.  The unredacted slice is
/// **not** modified; this function only affects what is written to the log.
///
/// Redaction rules (applied per element):
/// 1. `NAME=VALUE` — if `NAME` matches the secret-name pattern, `VALUE` is
///    replaced with `<redacted>` (short values) or `<redacted:PREFIX…>`
///    (≥ 16 bytes); see `redact_value` for details.
/// 2. Bare token value — if the element looks like a known token shape
///    (GitHub PAT, OpenAI `sk-…`, Slack `xox*-…`) it is replaced wholesale.
/// 3. Everything else — passed through verbatim.
fn redact_argv(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if let Some(eq_idx) = arg.find('=') {
                let name = &arg[..eq_idx];
                let value = &arg[eq_idx + 1..];
                if is_secret_name(name) {
                    return format!("{}={}", name, redact_value(value));
                }
                return arg.clone();
            }
            if is_token_value(arg) {
                return redact_value(arg);
            }
            arg.clone()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn run_command(args: &[String]) -> Result<CommandOutput, String> {
    if args.is_empty() {
        return Err("run_command requires at least one argument".to_string());
    }

    log_info(&format!("exec: {}", redact_argv(args)));

    let mut command = Command::new(&args[0]);
    for arg in &args[1..] {
        command.arg(arg);
    }

    let output = command
        .output()
        .map_err(|err| format!("failed to execute {}: {err}", args[0]))?;

    Ok(CommandOutput {
        returncode: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::{is_secret_name, is_token_value, redact_argv, redact_value};

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    // ── is_secret_name ──────────────────────────────────────────────────────

    #[test]
    fn secret_name_botwork_secret_prefix() {
        assert!(is_secret_name("BOTWORK_SECRET_GITHUB_COM_PAT"));
        assert!(is_secret_name("BOTWORK_SECRET_"));
        assert!(!is_secret_name("BOTWORK_PLUGIN_NAME"));
    }

    #[test]
    fn secret_name_common_suffixes() {
        for name in [
            "MY_TOKEN",
            "MY_PAT",
            "MY_SECRET",
            "MY_KEY",
            "MY_PASSWORD",
            "MY_PASS",
            "MY_API_KEY",
        ] {
            assert!(is_secret_name(name), "{name} should be a secret name");
        }
    }

    #[test]
    fn secret_name_pat_numbered() {
        assert!(is_secret_name("MY_PAT1"));
        assert!(is_secret_name("MY_PAT2"));
        assert!(is_secret_name("MY_PAT99"));
    }

    #[test]
    fn secret_name_innocuous_names_are_not_secret() {
        for name in ["HOME", "PATH", "BOTWORK_PLUGIN_NAME", "BOTWORK_IMAGE"] {
            assert!(!is_secret_name(name), "{name} should not be a secret name");
        }
    }

    // ── is_token_value ──────────────────────────────────────────────────────

    #[test]
    fn token_value_github_pat_family() {
        let long_suffix = "A".repeat(20);
        for prefix in ["ghp_", "gho_", "ghr_", "ghs_", "ghu_"] {
            let token = format!("{prefix}{long_suffix}");
            assert!(is_token_value(&token), "{token} should be a token value");
        }
    }

    #[test]
    fn token_value_openai_sk() {
        let token = format!("sk-{}", "A".repeat(20));
        assert!(is_token_value(&token));
    }

    #[test]
    fn token_value_slack() {
        for prefix in ["xoxa-", "xoxb-", "xoxp-", "xoxr-", "xoxs-"] {
            let token = format!("{prefix}{}", "A".repeat(10));
            assert!(is_token_value(&token), "{token} should be a token value");
        }
    }

    #[test]
    fn token_value_short_strings_are_not_tokens() {
        assert!(!is_token_value("ghp_short"));
        assert!(!is_token_value("sk-short"));
        // Slack short values (< 10 chars after prefix) must not be redacted
        for prefix in ["xoxa-", "xoxb-", "xoxp-", "xoxr-", "xoxs-"] {
            let token = format!("{prefix}short");
            assert!(
                !is_token_value(&token),
                "{token} is too short to be a Slack token"
            );
        }
    }

    #[test]
    fn token_value_ordinary_args_are_not_tokens() {
        for arg in ["--name", "mcp_session_xxx", "--memory", "512m", "docker"] {
            assert!(!is_token_value(arg), "{arg} should not be a token value");
        }
    }

    // ── redact_value ────────────────────────────────────────────────────────

    #[test]
    fn redact_value_short_values_redacted_wholesale() {
        // Values shorter than 16 bytes are fully redacted to avoid leaking
        // a meaningful fraction of a short secret through the prefix.
        assert_eq!(redact_value(""), "<redacted>");
        assert_eq!(redact_value("abc"), "<redacted>");
        assert_eq!(redact_value("hunter2"), "<redacted>");
        assert_eq!(redact_value(&"x".repeat(15)), "<redacted>");
    }

    #[test]
    fn redact_value_long_values_keep_six_char_prefix() {
        // For values >= 16 bytes the 6-char prefix is logged to aid
        // cross-log correlation without exposing useful entropy.
        assert_eq!(redact_value(&"x".repeat(16)), "<redacted:xxxxxx…>");
        assert_eq!(redact_value("ghp_p8xpXgViGiTM3"), "<redacted:ghp_p8…>");
        assert_eq!(redact_value("abcdefghijklmnop"), "<redacted:abcdef…>");
    }

    // ── redact_argv ─────────────────────────────────────────────────────────

    #[test]
    fn redact_argv_redacts_botwork_secret_name_value() {
        let secret = format!("ghp_{}", "x".repeat(36));
        let arg = format!("BOTWORK_SECRET_GITHUB_COM_PAT={secret}");
        let args = argv(&["docker", "run", "-e", &arg]);
        let out = redact_argv(&args);
        assert!(
            !out.contains(&secret),
            "secret value must not appear in log: {out}"
        );
        assert!(
            out.contains("BOTWORK_SECRET_GITHUB_COM_PAT=<redacted"),
            "redacted placeholder must appear: {out}"
        );
    }

    #[test]
    fn redact_argv_redacts_all_secret_suffixes() {
        // Use a 13-byte value so it exercises the short-redact path (the
        // wholesale `<redacted>` form); the longer prefix-preserving form
        // is exercised in redact_value_long_values_keep_six_char_prefix.
        for name in [
            "MY_TOKEN",
            "MY_PAT",
            "MY_PAT2",
            "MY_SECRET",
            "MY_KEY",
            "MY_PASSWORD",
            "MY_PASS",
            "MY_API_KEY",
        ] {
            let arg = format!("{name}=supersecret");
            let out = redact_argv(&argv(&[&arg]));
            assert!(
                !out.contains("supersecret"),
                "{name}: secret value must not appear: {out}"
            );
            assert!(
                out.contains("<redacted"),
                "{name}: redacted placeholder must appear: {out}"
            );
        }
    }

    #[test]
    fn redact_argv_does_not_redact_innocuous_env() {
        for arg in [
            "BOTWORK_PLUGIN_NAME=foo",
            "HOME=/workspace",
            "PATH=/usr/bin",
        ] {
            let out = redact_argv(&argv(&[arg]));
            assert_eq!(out, arg, "innocuous env should pass through unchanged");
        }
    }

    #[test]
    fn redact_argv_does_not_redact_misc_flags() {
        for arg in ["--name", "mcp_session_xxx", "--memory", "512m"] {
            let out = redact_argv(&argv(&[arg]));
            assert_eq!(out, arg);
        }
    }

    #[test]
    fn redact_argv_preserves_order() {
        let args = argv(&[
            "docker",
            "run",
            "--name",
            "mcp_session_abc",
            "-e",
            "BOTWORK_SECRET_X=secret123",
            "-e",
            "BOTWORK_PLUGIN_NAME=echo",
        ]);
        let out = redact_argv(&args);
        let parts: Vec<&str> = out.split(' ').collect();
        assert_eq!(parts[0], "docker");
        assert_eq!(parts[1], "run");
        assert_eq!(parts[2], "--name");
        assert_eq!(parts[3], "mcp_session_abc");
        assert_eq!(parts[4], "-e");
        assert!(parts[5].starts_with("BOTWORK_SECRET_X=<redacted"));
        assert_eq!(parts[6], "-e");
        assert_eq!(parts[7], "BOTWORK_PLUGIN_NAME=echo");
        assert!(!out.contains("secret123"));
    }

    #[test]
    fn redact_argv_redacts_standalone_github_pat() {
        let token = format!("ghp_{}", "X".repeat(20));
        let out = redact_argv(&argv(&["docker", "run", &token]));
        assert!(!out.contains(&token), "raw token must not appear: {out}");
        assert!(out.contains("<redacted:"), "placeholder must appear: {out}");
    }

    // ── run_command ─────────────────────────────────────────────────────────

    #[test]
    fn run_command_returns_zero_for_true() {
        use super::run_command;
        let out = run_command(&["true".to_string()]).expect("run_command should not error");
        assert_eq!(out.returncode, 0);
    }

    #[test]
    fn run_command_returns_nonzero_for_false() {
        use super::run_command;
        let out = run_command(&["false".to_string()]).expect("run_command should not error");
        assert_ne!(out.returncode, 0);
    }

    #[test]
    fn run_command_captures_stdout() {
        use super::run_command;
        let out = run_command(&["echo".to_string(), "hello world".to_string()])
            .expect("run_command should not error");
        assert_eq!(out.returncode, 0);
        assert!(out.stdout.contains("hello world"));
    }

    #[test]
    fn run_command_captures_stderr() {
        use super::run_command;
        // `sh -c 'echo msg >&2'` writes to stderr and exits 0
        let out = run_command(&[
            "sh".to_string(),
            "-c".to_string(),
            "echo errline >&2".to_string(),
        ])
        .expect("run_command should not error");
        assert_eq!(out.returncode, 0);
        assert!(out.stderr.contains("errline"), "stderr: {}", out.stderr);
    }

    #[test]
    fn run_command_empty_args_returns_error() {
        use super::run_command;
        let err = run_command(&[]).expect_err("empty args should fail");
        assert!(err.contains("requires at least one argument"), "{err}");
    }
}
