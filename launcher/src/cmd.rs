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

pub fn run_command(args: &[String]) -> Result<CommandOutput, String> {
    if args.is_empty() {
        return Err("run_command requires at least one argument".to_string());
    }

    log_info(&format!("exec: {}", args.join(" ")));

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
pub fn run_command_with_stdin(args: &[String], stdin_data: &[u8]) -> Result<CommandOutput, String> {
    if args.is_empty() {
        return Err("run_command requires at least one argument".to_string());
    }

    log_info(&format!("exec: {}", args.join(" ")));

    let mut child = Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("failed to execute {}: {err}", args[0]))?;

    {
        let mut stdin = child.stdin.take().expect("stdin is piped");
        stdin
            .write_all(stdin_data)
            .map_err(|err| format!("failed to write stdin: {err}"))?;
        // Drop closes the pipe and sends EOF to the child.
    }

    let output = child
        .wait_with_output()
        .map_err(|err| format!("failed to wait for {}: {err}", args[0]))?;

    Ok(CommandOutput {
        returncode: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}
