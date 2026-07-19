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
    password_bytes_from_line(line, "password from stdin was empty")
}

fn prompt_password(confirm: bool) -> Result<Zeroizing<Vec<u8>>, LoginError> {
    let first = read_password_with_prompt("Password: ")?;
    if !confirm {
        return Ok(first);
    }
    let second = read_password_with_prompt("Confirm password: ")?;
    confirm_passwords(first, second)
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
    password_bytes_from_line(value, "password must not be empty")
}

fn read_line_with_prompt(prompt: &str) -> Result<String, LoginError> {
    eprint!("{prompt}");
    io::stderr().flush().ok();
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|err| LoginError::Other(format!("failed to read password: {err}")))?;
    Ok(trim_password_line(line))
}

fn trim_password_line(mut line: String) -> String {
    // Strip a trailing CR/LF pair (or lone newline) but preserve
    // every other byte so whitespace inside the password stays
    // significant.
    while matches!(line.chars().last(), Some('\n' | '\r')) {
        line.pop();
    }
    line
}

fn password_bytes_from_line(
    line: String,
    empty_message: &str,
) -> Result<Zeroizing<Vec<u8>>, LoginError> {
    let line = trim_password_line(line);
    if line.is_empty() {
        return Err(LoginError::Other(empty_message.to_string()));
    }
    Ok(Zeroizing::new(line.into_bytes()))
}

fn confirm_passwords(
    first: Zeroizing<Vec<u8>>,
    second: Zeroizing<Vec<u8>>,
) -> Result<Zeroizing<Vec<u8>>, LoginError> {
    if first.as_slice() != second.as_slice() {
        return Err(LoginError::Other("passwords do not match".to_string()));
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_defaults_match_subcommands() {
        assert!(matches!(
            PasswordSource::for_login(false),
            PasswordSource::Prompt { confirm: false }
        ));
        assert!(matches!(
            PasswordSource::for_login(true),
            PasswordSource::Stdin
        ));
        assert!(matches!(
            PasswordSource::for_register(false),
            PasswordSource::Prompt { confirm: true }
        ));
        assert!(matches!(
            PasswordSource::for_register(true),
            PasswordSource::Stdin
        ));
    }

    #[test]
    fn trim_password_line_removes_only_trailing_newlines() {
        assert_eq!(trim_password_line("secret\r\n".to_string()), "secret");
        assert_eq!(trim_password_line("secret\n".to_string()), "secret");
        assert_eq!(trim_password_line("secret\r\n\r\n".to_string()), "secret");
        assert_eq!(
            trim_password_line(" secret value ".to_string()),
            " secret value "
        );
    }

    #[test]
    fn password_bytes_from_line_rejects_empty_after_trimming() {
        let err = password_bytes_from_line("\r\n".to_string(), "empty").unwrap_err();
        assert!(matches!(err, LoginError::Other(msg) if msg == "empty"));
    }

    #[test]
    fn password_bytes_from_line_preserves_significant_whitespace() {
        let value = password_bytes_from_line(" secret value \n".to_string(), "empty").unwrap();
        assert_eq!(value.as_slice(), b" secret value ");
    }

    #[test]
    fn confirm_passwords_rejects_mismatch() {
        let err = confirm_passwords(
            Zeroizing::new(b"one".to_vec()),
            Zeroizing::new(b"two".to_vec()),
        )
        .unwrap_err();
        assert!(matches!(err, LoginError::Other(msg) if msg == "passwords do not match"));
    }

    #[test]
    fn confirm_passwords_accepts_match() {
        let value = confirm_passwords(
            Zeroizing::new(b"same".to_vec()),
            Zeroizing::new(b"same".to_vec()),
        )
        .unwrap();
        assert_eq!(value.as_slice(), b"same");
    }
}
