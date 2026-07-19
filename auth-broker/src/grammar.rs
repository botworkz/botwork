use std::sync::OnceLock;

use regex::Regex;

pub const RESERVED_TENANT_NAMES: &[&str] = &["admin", "api", "auth", "static", "stats", "logs"];
pub const NAME_REGEX: &str = r"^[A-Za-z0-9_-]{1,63}$";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("name does not match {NAME_REGEX}")]
    Invalid,
    #[error("tenant name is reserved")]
    Reserved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParsedPath {
    ApiAuthLogin,
    ApiAuthProtected,
    Api {
        tenant: Option<String>,
    },
    Spa {
        tenant: String,
    },
    Mcp {
        tenant: String,
        namespace: String,
        plugin: String,
    },
}

fn name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(NAME_REGEX).expect("valid NAME_REGEX"))
}

pub fn normalise_name(name: &str) -> String {
    name.to_ascii_lowercase()
}

pub fn validate_tenant_name(name: &str) -> Result<(), NameError> {
    validate_name(name, true)
}

pub fn validate_workspace_name(name: &str) -> Result<(), NameError> {
    validate_name(name, false)
}

pub fn validate_plugin_name(name: &str) -> Result<(), NameError> {
    validate_name(name, false)
}

fn validate_name(name: &str, reserved: bool) -> Result<(), NameError> {
    if !name_re().is_match(name) {
        return Err(NameError::Invalid);
    }
    if reserved
        && RESERVED_TENANT_NAMES
            .iter()
            .any(|candidate| normalise_name(candidate) == normalise_name(name))
    {
        return Err(NameError::Reserved);
    }
    Ok(())
}

pub(crate) fn parse_original_path(path: &str) -> Option<ParsedPath> {
    if path == "/api/auth/login" || path.starts_with("/api/auth/login/") {
        return Some(ParsedPath::ApiAuthLogin);
    }
    if path == "/api/auth" {
        return None;
    }
    if path.starts_with("/api/auth/") {
        let suffix = path.trim_start_matches("/api/auth/");
        if !suffix.is_empty() {
            return Some(ParsedPath::ApiAuthProtected);
        }
        return None;
    }
    if path == "/api" || path.starts_with("/api/") {
        let mut segments = path.split('/').filter(|segment| !segment.is_empty());
        let _api = segments.next()?;
        let tenant = match (segments.next(), segments.next()) {
            (Some("tenant"), Some(tenant)) => {
                validate_tenant_name(tenant).ok()?;
                Some(tenant.to_string())
            }
            (Some("tenant"), None) => return None,
            _ => None,
        };
        return Some(ParsedPath::Api { tenant });
    }

    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    match segments.as_slice() {
        [tenant] if validate_tenant_name(tenant).is_ok() => Some(ParsedPath::Spa {
            tenant: (*tenant).to_string(),
        }),
        [tenant, namespace, plugin, ..]
            if validate_tenant_name(tenant).is_ok()
                && validate_workspace_name(namespace).is_ok()
                && validate_plugin_name(plugin).is_ok() =>
        {
            Some(ParsedPath::Mcp {
                tenant: (*tenant).to_string(),
                namespace: (*namespace).to_string(),
                plugin: (*plugin).to_string(),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_validation_enforces_regex_and_reserved_names() {
        assert_eq!(validate_tenant_name("phlax"), Ok(()));
        assert_eq!(validate_tenant_name("Phlax_123"), Ok(()));
        assert_eq!(validate_tenant_name("admin"), Err(NameError::Reserved));
        assert_eq!(validate_tenant_name("API"), Err(NameError::Reserved));
        assert_eq!(validate_tenant_name("bad.name"), Err(NameError::Invalid));
        assert_eq!(validate_tenant_name("@@hidden"), Err(NameError::Invalid));
        assert_eq!(validate_tenant_name(""), Err(NameError::Invalid));
        assert_eq!(
            validate_tenant_name(&"a".repeat(64)),
            Err(NameError::Invalid)
        );
    }

    #[test]
    fn workspace_and_plugin_validation_share_the_same_regex() {
        assert_eq!(validate_workspace_name("workspace_1"), Ok(()));
        assert_eq!(validate_plugin_name("exec-bash"), Ok(()));
        assert_eq!(validate_workspace_name("bad.name"), Err(NameError::Invalid));
        assert_eq!(validate_plugin_name("bad/name"), Err(NameError::Invalid));
        assert_eq!(validate_workspace_name("admin"), Ok(()));
        assert_eq!(validate_plugin_name("api"), Ok(()));
    }

    #[test]
    fn normalise_name_is_ascii_lowercase() {
        assert_eq!(normalise_name("PhLaX_123"), "phlax_123");
    }

    #[test]
    fn parser_accepts_phase_two_shapes() {
        assert_eq!(
            parse_original_path("/api"),
            Some(ParsedPath::Api { tenant: None })
        );
        assert_eq!(
            parse_original_path("/api/tenant/phlax/secrets"),
            Some(ParsedPath::Api {
                tenant: Some("phlax".to_string())
            })
        );
        assert_eq!(
            parse_original_path("/api/auth/login"),
            Some(ParsedPath::ApiAuthLogin)
        );
        assert_eq!(
            parse_original_path("/api/auth/whoami"),
            Some(ParsedPath::ApiAuthProtected)
        );
        assert_eq!(
            parse_original_path("/phlax"),
            Some(ParsedPath::Spa {
                tenant: "phlax".to_string()
            })
        );
        assert_eq!(
            parse_original_path("/phlax/mcp/exec-bash"),
            Some(ParsedPath::Mcp {
                tenant: "phlax".to_string(),
                namespace: "mcp".to_string(),
                plugin: "exec-bash".to_string()
            })
        );
        assert_eq!(
            parse_original_path("/phlax/mcp/exec-bash/run"),
            Some(ParsedPath::Mcp {
                tenant: "phlax".to_string(),
                namespace: "mcp".to_string(),
                plugin: "exec-bash".to_string()
            })
        );
    }

    #[test]
    fn parser_rejects_bad_and_reserved_shapes() {
        assert_eq!(parse_original_path(""), None);
        assert_eq!(parse_original_path("/tenant/plugin"), None);
        assert_eq!(parse_original_path("/admin"), None);
        assert_eq!(parse_original_path("/api/tenant/admin/secrets"), None);
        assert_eq!(parse_original_path("/bad.name"), None);
        assert_eq!(parse_original_path("/tenant/bad.name/plugin"), None);
        assert_eq!(parse_original_path("/@@hidden"), None);
        assert_eq!(parse_original_path("/api/auth"), None);
    }

    #[test]
    fn api_auth_trailing_slash_returns_none() {
        // `/api/auth/` starts_with "/api/auth/" but the suffix after
        // stripping the prefix is empty → the inner `return None` branch.
        assert_eq!(parse_original_path("/api/auth/"), None);
    }

    #[test]
    fn api_tenant_with_no_name_returns_none() {
        // `/api/tenant` has exactly the "tenant" segment but no tenant
        // name following it → `(Some("tenant"), None) => return None`.
        assert_eq!(parse_original_path("/api/tenant"), None);
    }
}
