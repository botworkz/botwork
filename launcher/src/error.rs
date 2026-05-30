use thiserror::Error;

#[derive(Debug, Error)]
pub enum LauncherError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    PayloadTooLarge(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Internal(String),
}

impl LauncherError {
    pub fn status_code(&self) -> u16 {
        match self {
            Self::BadRequest(_) => 400,
            Self::PayloadTooLarge(_) => 413,
            Self::Conflict(_) => 409,
            Self::NotFound(_) => 404,
            Self::Internal(_) => 500,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::BadRequest(msg)
            | Self::PayloadTooLarge(msg)
            | Self::Conflict(msg)
            | Self::NotFound(msg)
            | Self::Internal(msg) => msg,
        }
    }
}
