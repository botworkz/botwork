use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use reqwest::StatusCode;
use thiserror::Error;

use crate::frontdoor::rds::HOLDING_MARKER;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Open,
    Closed,
    Unknown,
}

pub fn classify_once(probe_url: &str) -> State {
    classify_once_with_timeout(probe_url, REQUEST_TIMEOUT)
}

pub fn classify_once_with_timeout(probe_url: &str, timeout: Duration) -> State {
    match fetch_body(probe_url, timeout) {
        Ok(body) if body.is_empty() => State::Unknown,
        Ok(body) if body.contains(HOLDING_MARKER) => State::Closed,
        Ok(_) => State::Open,
        Err(_) => State::Unknown,
    }
}

pub fn poll_until_marker_present(probe_url: &str, timeout: Duration) -> Result<(), ProbeError> {
    poll_until(probe_url, timeout, "present", |body| {
        body.contains(HOLDING_MARKER)
    })
}

pub fn poll_until_marker_absent(probe_url: &str, timeout: Duration) -> Result<(), ProbeError> {
    poll_until(probe_url, timeout, "absent", |body| {
        !body.contains(HOLDING_MARKER)
    })
}

fn poll_until<F>(
    probe_url: &str,
    timeout: Duration,
    expectation: &'static str,
    matches: F,
) -> Result<(), ProbeError>
where
    F: Fn(&str) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(body) = fetch_body(probe_url, REQUEST_TIMEOUT) {
            if matches(&body) {
                return Ok(());
            }
        }

        if Instant::now() >= deadline {
            return Err(ProbeError::TimedOut {
                timeout_secs: timeout.as_secs(),
                probe_url: probe_url.to_string(),
                expectation,
            });
        }

        std::thread::sleep(POLL_INTERVAL);
    }
}

fn fetch_body(probe_url: &str, timeout: Duration) -> Result<String, ProbeError> {
    let client = Client::builder()
        .timeout(timeout)
        .build()
        .map_err(ProbeError::BuildClient)?;
    let response = client
        .get(probe_url)
        .send()
        .map_err(ProbeError::Transport)?;
    let status = response.status();
    if !status.is_success() {
        return Err(ProbeError::BadStatus(status));
    }
    response.text().map_err(ProbeError::ReadBody)
}

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("failed to build probe HTTP client: {0}")]
    BuildClient(reqwest::Error),
    #[error("frontdoor probe transport error: {0}")]
    Transport(reqwest::Error),
    #[error("frontdoor probe returned non-success status: {0}")]
    BadStatus(StatusCode),
    #[error("frontdoor probe failed to read response body: {0}")]
    ReadBody(reqwest::Error),
    #[error(
        "timed out after {timeout_secs}s waiting for marker to become {expectation} at {probe_url}"
    )]
    TimedOut {
        timeout_secs: u64,
        probe_url: String,
        expectation: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::{classify_once, State};

    #[test]
    fn classify_unknown_for_unreachable() {
        assert_eq!(classify_once("http://127.0.0.1:1/"), State::Unknown);
    }
}
