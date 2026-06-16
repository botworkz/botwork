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

    log_info(&format!("exec: {}", args.join(" ")));

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
