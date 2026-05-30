use std::process::Command;

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
