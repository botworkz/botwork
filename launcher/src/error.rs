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

#[cfg(test)]
mod tests {
    use super::LauncherError;

    #[test]
    fn status_codes_map_correctly() {
        assert_eq!(LauncherError::BadRequest("x".into()).status_code(), 400);
        assert_eq!(
            LauncherError::PayloadTooLarge("x".into()).status_code(),
            413
        );
        assert_eq!(LauncherError::Conflict("x".into()).status_code(), 409);
        assert_eq!(LauncherError::NotFound("x".into()).status_code(), 404);
        assert_eq!(LauncherError::Internal("x".into()).status_code(), 500);
    }

    #[test]
    fn message_returns_inner_string_for_all_variants() {
        let cases = [
            LauncherError::BadRequest("bad request msg".into()),
            LauncherError::PayloadTooLarge("too large msg".into()),
            LauncherError::Conflict("conflict msg".into()),
            LauncherError::NotFound("not found msg".into()),
            LauncherError::Internal("internal msg".into()),
        ];
        for err in &cases {
            assert!(
                err.message().ends_with("msg"),
                "unexpected message for {err:?}"
            );
        }
    }

    #[test]
    fn display_uses_inner_message() {
        assert_eq!(
            LauncherError::BadRequest("bad input".into()).to_string(),
            "bad input"
        );
        assert_eq!(
            LauncherError::Internal("something broke".into()).to_string(),
            "something broke"
        );
    }
}
