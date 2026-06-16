use std::io::Write;
use std::process::{Command, Stdio};

use crate::config::PREFIX;

#[derive(Debug)]
pub struct CommandOutput {
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn log_info(message: &str) {
    println!("{PREFIX} {message}");
}

pub fn log_warn(message: &str) {
    println!("{PREFIX} [warn] {message}");
}

/// Returns `true` if `name` (the left-hand side of a `NAME=VALUE` argv element)
/// looks like it might carry a secret and its value should be redacted in logs.
fn is_secret_name(name: &str) -> bool {
    if name.starts_with("BOTWORK_SECRET_") {
        return true;
    }
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
    // _PAT[0-9]+  — e.g. MY_PAT1, MY_PAT2
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

fn redact_value(value: &str) -> String {
    let prefix: String = value.chars().take(6).collect();
    format!("<redacted:{prefix}…>")
}

/// Redacts secret-looking values in `args` and joins them into a single
/// space-separated string suitable for logging.  The unredacted slice is
/// **not** modified; this function only affects what is written to the log.
///
/// Redaction rules (applied per element):
/// 1. `NAME=VALUE` — if `NAME` matches the secret-name pattern, `VALUE` is
///    replaced with `<redacted:PREFIX…>` (first 6 chars of the value + `…`).
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

/// Like `run_command`, but pipes `stdin_data` into the child's stdin before
/// waiting.  The pipe is closed (EOF) as soon as the write completes, so the
/// child sees a clean end-of-file.  Use this when secret material must be
/// delivered via stdin rather than on the argv.
///
/// The stdin write happens on a dedicated thread so that the parent can drain
/// the child's stdout/stderr concurrently via `wait_with_output()`.  Without
/// that, a child that produced enough output to fill its stdout/stderr pipe
/// buffer (~64 KiB each on Linux) before it finished consuming stdin would
/// deadlock.  `docker run -d` doesn't approach that today, but this shape is
/// correct for any future caller that doesn't share that property.
pub fn run_command_with_stdin(args: &[String], stdin_data: &[u8]) -> Result<CommandOutput, String> {
    if args.is_empty() {
        return Err("run_command requires at least one argument".to_string());
    }

    log_info(&format!("exec: {}", redact_argv(args)));

    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to execute {}: {err}", args[0]))?;

    let mut stdin = child.stdin.take().expect("stdin is piped");
    let stdin_bytes = stdin_data.to_vec();

    let writer = std::thread::spawn(move || -> Result<(), String> {
        stdin
            .write_all(&stdin_bytes)
            .map_err(|err| format!("failed to write stdin: {err}"))
        // `stdin` drops here, closing the pipe and sending EOF to the child.
    });

    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to wait for {}: {err}", args[0]))?;

    writer
        .join()
        .map_err(|_| "stdin writer thread panicked".to_string())??;

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

    // ── redact_argv ─────────────────────────────────────────────────────────

    #[test]
    fn redact_argv_redacts_botwork_secret_name_value() {
        let args = argv(&[
            "docker",
            "run",
            "-e",
            "BOTWORK_SECRET_GITHUB_COM_PAT=ghp_realtoken",
        ]);
        let out = redact_argv(&args);
        assert!(
            !out.contains("ghp_realtoken"),
            "secret value must not appear in log: {out}"
        );
        assert!(
            out.contains("BOTWORK_SECRET_GITHUB_COM_PAT=<redacted:"),
            "redacted placeholder must appear: {out}"
        );
    }

    #[test]
    fn redact_argv_redacts_all_secret_suffixes() {
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
                out.contains("<redacted:"),
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
    fn redact_argv_does_not_redact_env_file_path() {
        let arg = "--env-file";
        let path = "/var/lib/botwork/tenants/phlax/staging/abc/.env-secrets";
        let out = redact_argv(&argv(&[arg, path]));
        assert_eq!(out, format!("{arg} {path}"));
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
        assert!(parts[5].starts_with("BOTWORK_SECRET_X=<redacted:"));
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

    #[test]
    fn redact_value_uses_six_char_prefix() {
        assert_eq!(redact_value("ghp_p8xpXgViGiTM3"), "<redacted:ghp_p8…>");
        assert_eq!(redact_value("abc"), "<redacted:abc…>");
        assert_eq!(redact_value(""), "<redacted:…>");
        // Long arbitrary string: only the first 6 chars are preserved
        assert_eq!(redact_value("abcdefghijklmnop"), "<redacted:abcdef…>");
    }
}
