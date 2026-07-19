use crate::error::VaultError;

fn validate_component(s: &str, field: &str) -> Result<(), VaultError> {
    if s.is_empty() {
        return Err(VaultError::InvalidComponent(format!(
            "{field}: empty component"
        )));
    }
    if s == "." || s == ".." {
        return Err(VaultError::InvalidComponent(format!(
            "{field}: reserved component '{s}'"
        )));
    }
    if !s
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(VaultError::InvalidComponent(format!(
            "{field}: invalid characters in '{s}' (allowed: A-Za-z0-9._-)"
        )));
    }
    Ok(())
}

pub fn validate_service(service: &str) -> Result<(), VaultError> {
    if service.is_empty() {
        return Err(VaultError::InvalidComponent("service: empty".to_string()));
    }
    for part in service.split('/') {
        validate_component(part, "service")?;
    }
    Ok(())
}

pub fn validate_name(name: &str) -> Result<(), VaultError> {
    validate_component(name, "name")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_rejects_empty_component() {
        let err = validate_name("").unwrap_err();
        assert!(matches!(err, VaultError::InvalidComponent(_)));
        assert_eq!(
            err.to_string(),
            "unsafe path component: name: empty component"
        );
    }

    #[test]
    fn validate_service_rejects_empty_path_component() {
        let err = validate_service("svc//token").unwrap_err();
        assert!(matches!(err, VaultError::InvalidComponent(_)));
        assert_eq!(
            err.to_string(),
            "unsafe path component: service: empty component"
        );
    }

    #[test]
    fn validate_service_rejects_invalid_characters() {
        let err = validate_service("svc/has space").unwrap_err();
        assert!(matches!(err, VaultError::InvalidComponent(_)));
        assert!(err
            .to_string()
            .contains("service: invalid characters in 'has space'"));
    }
}
