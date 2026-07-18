//! Shared password-input helper.
//!
//! Subcommands that need a password (`login`, `register`,
//! `register --confirm`) route through this so the prompt + stdin
//! behaviour stays consistent and the Zeroizing wrapper is applied
//! in exactly one place.

use std::io::{self, BufRead, IsTerminal, Write};

use zeroize::Zeroizing;

use crate::error::LoginError;

/// Where to read the password from.
#[derive(Debug, Clone, Copy)]
pub enum PasswordSource {
    /// Prompt on the controlling tty (`rpassword`, no echo).
    Prompt {
        /// Re-prompt and require the two values match.
        confirm: bool,
    },
    /// Read one line from stdin. Used by `--password-stdin`.
    Stdin,
}

impl PasswordSource {
    /// Default for `login`: prompt unless `--password-stdin`.
    pub fn for_login(stdin: bool) -> Self {
        if stdin {
            Self::Stdin
        } else {
            Self::Prompt { confirm: false }
        }
    }

    /// Default for `register`: prompt-with-confirm unless
    /// `--password-stdin`. Confirmation is skipped under
    /// `--password-stdin` because there's no second prompt to read
    /// — the caller is responsible for handing us the right bytes.
    pub fn for_register(stdin: bool) -> Self {
        if stdin {
            Self::Stdin
        } else {
            Self::Prompt { confirm: true }
        }
    }

    /// Read the password according to this source. Returns
    /// `Zeroizing<Vec<u8>>` so the buffer is wiped on drop.
    pub fn read(self) -> Result<Zeroizing<Vec<u8>>, LoginError> {
        match self {
            Self::Stdin => read_one_line_from_stdin(),
            Self::Prompt { confirm } => prompt_password(confirm),
        }
    }
}

fn read_one_line_from_stdin() -> Result<Zeroizing<Vec<u8>>, LoginError> {
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|err| LoginError::Other(format!("failed to read password from stdin: {err}")))?;
    // Strip a single trailing newline — same conservative trimming
    // the vault CLI does for header-bound secrets, but no more
    // (passwords with significant whitespace stay intact).
    while matches!(line.chars().last(), Some('\n' | '\r')) {
        line.pop();
    }
    if line.is_empty() {
        return Err(LoginError::Other(
            "password from stdin was empty".to_string(),
        ));
    }
    Ok(Zeroizing::new(line.into_bytes()))
}

fn prompt_password(confirm: bool) -> Result<Zeroizing<Vec<u8>>, LoginError> {
    let first = read_password_with_prompt("Password: ")?;
    if !confirm {
        return Ok(first);
    }
    let second = read_password_with_prompt("Confirm password: ")?;
    if first.as_slice() != second.as_slice() {
        return Err(LoginError::Other("passwords do not match".to_string()));
    }
    Ok(first)
}

fn read_password_with_prompt(prompt: &str) -> Result<Zeroizing<Vec<u8>>, LoginError> {
    // `rpassword::prompt_password` writes the prompt to stderr and
    // reads from /dev/tty without echo. On non-tty stdin we fall
    // back to a stdin read so unit tests can pipe input.
    let value = if io::stdin().is_terminal() {
        match rpassword::prompt_password(prompt) {
            Ok(v) => v,
            Err(_) => read_line_with_prompt(prompt)?,
        }
    } else {
        read_line_with_prompt(prompt)?
    };
    if value.is_empty() {
        return Err(LoginError::Other("password must not be empty".to_string()));
    }
    Ok(Zeroizing::new(value.into_bytes()))
}

fn read_line_with_prompt(prompt: &str) -> Result<String, LoginError> {
    eprint!("{prompt}");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|err| LoginError::Other(format!("failed to read password: {err}")))?;
    while matches!(line.chars().last(), Some('\n' | '\r')) {
        line.pop();
    }
    Ok(line)
}
