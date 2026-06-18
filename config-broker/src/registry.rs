//! Plugin registry: parses `plugins.yaml` into `PluginEntry` values that the
//! handler renders into wire-shape `PluginDescriptor` JSON.
//!
//! Lifted from session-broker (since deleted) in 0.1.3 / #75. Validation
//! rules (env name regex, size caps, `BOTWORK_MCP_CONFIG` reservation,
//! `upstream_auth` grammar, resources schema) are unchanged.
//!
//! Constants that previously lived in `session-broker/src/secrets.rs` are
//! duplicated here. They are *contract* values shared with the launcher's
//! env validation; both producers (config-broker → session-broker) and
//! consumers (launcher) must keep them in sync.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;
use thiserror::Error;

// Keep in sync with launcher/src/validate.rs RESERVED_ENV_NAMES.
const RESERVED_ENV_NAMES: &[&str] = &["PATH", "LD_PRELOAD", "LD_LIBRARY_PATH"];

/// Maximum number of static env entries per plugin (leaves headroom under
/// launcher's MAX_ENV_ENTRIES = 64 for vault-derived secrets).
const MAX_STATIC_ENV_ENTRIES: usize = 32;

/// Maximum size of any single env value (or serialised config blob).
/// Keep in sync with `session-broker::secrets::MAX_ENV_VALUE_BYTES`.
const MAX_ENV_VALUE_BYTES: usize = 64 * 1024;

/// Reserved prefix for vault-derived secret env entries; operators may not
/// declare env keys starting with this string.
/// Keep in sync with `session-broker::secrets::SECRET_ENV_PREFIX`.
const SECRET_ENV_PREFIX: &str = "BOTWORK_SECRET_";

/// Env var name under which compact-JSON structured config is injected.
///
/// This name is reserved: operators must express structured config through the
/// `config:` field in `plugins.yaml`, not via the flat `env:` mapping.
pub const CONFIG_ENV_NAME: &str = "BOTWORK_MCP_CONFIG";

static PLUGIN_NAME_RE: OnceLock<Regex> = OnceLock::new();

fn plugin_name_re() -> &'static Regex {
    PLUGIN_NAME_RE.get_or_init(|| Regex::new(r"^[a-z][a-z0-9-]{0,30}$").unwrap())
}

/// In-memory representation of a single plugin's `plugins.yaml` entry.
///
/// `PluginDescriptor` (the wire shape) is rendered from this by the handler.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginEntry {
    pub image: String,
    pub port: u16,
    pub path: String,
    pub upstream_auth: UpstreamAuth,
    pub env: Vec<(String, String)>,
    pub resources: PluginResources,
    /// Structured config, stored as a JSON value at parse time and serialised
    /// to compact JSON at the wire boundary. `None` means the operator did not
    /// set `config:` and the env var must not be injected.
    pub config: Option<serde_json::Value>,
    /// `egress:` block from `plugins.yaml`. config-broker 0.1.9+ enforces a
    /// required, default-deny schema: every plugin entry MUST set one of:
    ///
    ///   * `egress: all`                   -- unrestricted egress
    ///   * `egress: none`                  -- no egress at all
    ///   * `egress: { allow: [...] }`      -- explicit allow-list of
    ///     { host, ports } entries
    ///
    /// Missing or otherwise-shaped `egress:` rejects the whole registry at
    /// load time -- making the policy decision implicit by accident is the
    /// failure mode we want to make impossible.
    ///
    /// Stored as `serde_json::Value` so the wire path is "validate-and-
    /// forward": session-broker passes the same shape to control-plane (botwork
    /// #81) as the `egress_policy` of a `SessionRecord`, and the future xDS
    /// materialiser walks it from there. config-broker does not interpret
    /// the policy beyond shape validation; intent + drift live with the
    /// materialiser.
    pub egress: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpstreamAuth {
    #[default]
    None,
    Bearer {
        service: String,
    },
}

impl UpstreamAuth {
    /// Render to the on-the-wire string form: `"none"` or `"bearer/<service>"`.
    pub fn to_wire(&self) -> String {
        match self {
            Self::None => "none".to_string(),
            Self::Bearer { service } => format!("bearer/{service}"),
        }
    }

    fn from_yaml_value(name: &str, value: &serde_yaml::Value) -> Result<Self, RegistryError> {
        if value.is_null() {
            return Ok(Self::None);
        }

        let Some(value) = value.as_str() else {
            return Err(RegistryError::Invalid(format!(
                "plugin '{name}' has invalid 'upstream_auth': expected 'none' or 'bearer/<service>'"
            )));
        };

        match value {
            "none" => Ok(Self::None),
            "bearer" | "bearer/" => Err(RegistryError::Invalid(format!(
                "plugin '{name}' has invalid 'upstream_auth': bearer requires a service: use bearer/<service>"
            ))),
            _ => {
                if let Some(service) = value.strip_prefix("bearer/") {
                    if !service.is_empty()
                        && !service.contains('/')
                        && !service.chars().any(char::is_whitespace)
                    {
                        return Ok(Self::Bearer {
                            service: service.to_string(),
                        });
                    }
                }
                Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'upstream_auth': expected 'none' or 'bearer/<service>'"
                )))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PluginResources {
    pub cpus: Option<String>,
    pub memory: Option<String>,
    pub pids: Option<u32>,
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("plugin registry file not found: {0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
}

pub type PluginRegistry = HashMap<String, PluginEntry>;

fn valid_env_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_uppercase() || first == b'_') {
        return false;
    }
    if bytes
        .iter()
        .skip(1)
        .any(|b| !(b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_'))
    {
        return false;
    }
    if RESERVED_ENV_NAMES.contains(&name) {
        return false;
    }
    if name.starts_with("DOCKER_") {
        return false;
    }
    true
}

fn parse_env(
    plugin_name: &str,
    config_val: &serde_yaml::Value,
) -> Result<Vec<(String, String)>, RegistryError> {
    let env_val = &config_val["env"];
    if env_val.is_null() {
        return Ok(Vec::new());
    }

    let mapping = env_val.as_mapping().ok_or_else(|| {
        RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'env': expected a mapping"
        ))
    })?;

    if mapping.len() > MAX_STATIC_ENV_ENTRIES {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has too many 'env' entries: maximum is {MAX_STATIC_ENV_ENTRIES}"
        )));
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut result = Vec::with_capacity(mapping.len());

    for (key_val, value_val) in mapping {
        let key = key_val.as_str().ok_or_else(|| {
            RegistryError::Invalid(format!(
                "plugin '{plugin_name}' has invalid 'env' key: expected string"
            ))
        })?;

        // Reject non-string values with a helpful hint to quote them.
        let value = match value_val {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Bool(_) | serde_yaml::Value::Number(_) => {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{plugin_name}' env key '{key}': value must be a string (quote it: \"{value_val:?}\")"
                )));
            }
            _ => {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{plugin_name}' env key '{key}': value must be a string"
                )));
            }
        };

        if key.starts_with(SECRET_ENV_PREFIX) {
            return Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' env key '{key}': names starting with '{SECRET_ENV_PREFIX}' are reserved for vault-derived secrets"
            )));
        }

        if key == CONFIG_ENV_NAME {
            return Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' env key '{key}': reserved for structured config injection; use the 'config:' field instead"
            )));
        }

        if !valid_env_name(key) {
            return Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' env key '{key}': invalid name (must match [A-Z_][A-Z0-9_]*, not reserved or DOCKER_-prefixed)"
            )));
        }

        if value.len() > MAX_ENV_VALUE_BYTES {
            return Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' env key '{key}': value exceeds maximum size of {MAX_ENV_VALUE_BYTES} bytes"
            )));
        }

        if !seen.insert(key.to_string()) {
            return Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' env key '{key}': duplicate key"
            )));
        }

        result.push((key.to_string(), value));
    }

    Ok(result)
}

fn parse_config(
    plugin_name: &str,
    config_val: &serde_yaml::Value,
) -> Result<Option<serde_json::Value>, RegistryError> {
    let raw = &config_val["config"];
    if raw.is_null() {
        return Ok(None);
    }

    // Most well-formed YAML structures round-trip into JSON cleanly; failures
    // mean the operator used a YAML feature that has no JSON equivalent
    // (e.g. a null map key).
    let json_val: serde_json::Value = serde_json::to_value(raw).map_err(|e| {
        RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'config': cannot represent as JSON: {e}"
        ))
    })?;

    if !json_val.is_object() {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'config': expected a mapping (got {})",
            json_val_type_name(&json_val)
        )));
    }

    // Treat an empty mapping the same as absent: no env var injected.
    if json_val.as_object().unwrap().is_empty() {
        return Ok(None);
    }

    // Guard against pathologically large blobs at load time.
    let serialized =
        serde_json::to_string(&json_val).expect("Value already validated as JSON-serializable");
    if serialized.len() > MAX_ENV_VALUE_BYTES {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'config': serialized JSON exceeds maximum size of {MAX_ENV_VALUE_BYTES} bytes"
        )));
    }

    Ok(Some(json_val))
}

/// Parse + validate a plugin's required `egress:` block.
///
/// As of config-broker 0.1.9 egress is **default-deny**: every plugin in
/// `plugins.yaml` MUST declare one of:
///
///   * `egress: all`  -- string literal. Wire shape: `{ "mode": "all" }`.
///     The opt-in "old behaviour"; reviewers should see this as "we
///     explicitly chose not to lock down".
///   * `egress: none` -- string literal. Wire shape: `{ "mode": "none" }`.
///     "Plugin must never egress." Useful for compute-only plugins (echo,
///     exec-*-without-network) and as the safe default for anything new.
///   * `egress: { allow: [ { host, ports }, ... ] }` -- explicit allow
///     list, exact-match hostnames + TCP port lists. Wire shape passes
///     through verbatim; the materialiser is what compiles it into envoy
///     RBAC.
///
/// Missing `egress:` -- or any other shape -- fails the whole registry at
/// load time. The point of default-deny is that "I forgot" cannot become
/// "allow everything"; the loud error makes the policy choice visible.
///
/// Wire shape is deliberately verbatim: control-plane stores it as opaque
/// JSON, the materialiser owns the schema, and config-broker doesn't add
/// transformation surface.
fn parse_egress(
    plugin_name: &str,
    config_val: &serde_yaml::Value,
) -> Result<serde_json::Value, RegistryError> {
    let raw = &config_val["egress"];

    // Required field. Default-deny means absence is an error, not an
    // implicit "all" / "none" / anything else.
    if raw.is_null() {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' is missing required 'egress' field: every plugin must declare one of `egress: all`, `egress: none`, or `egress: {{ allow: [...] }}` -- default-deny means there is no implicit fallback"
        )));
    }

    // String forms: `all` / `none`. We normalise to a `{ "mode": "<value>" }`
    // object on the wire so consumers branch on the same key shape they use
    // for the structured form (i.e. they never have to test "is this value a
    // string or a map?").
    if let Some(s) = raw.as_str() {
        return match s {
            "all" => Ok(serde_json::json!({ "mode": "all" })),
            "none" => Ok(serde_json::json!({ "mode": "none" })),
            other => Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' has invalid 'egress': string form must be 'all' or 'none' (got {other:?})"
            ))),
        };
    }

    // Mapping form: must have `allow:`. Reject mappings with `mode:` -- that
    // shape is reserved for the wire emission of the string forms above and
    // operators must use the string sugar.
    let mapping = raw.as_mapping().ok_or_else(|| {
        RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'egress': expected 'all', 'none', or a mapping (got {})",
            yaml_val_type_name(raw),
        ))
    })?;

    if mapping.contains_key(serde_yaml::Value::String("mode".to_string())) {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'egress': 'mode:' is reserved for the wire encoding of the 'all'/'none' string forms; use `egress: all` or `egress: none` directly"
        )));
    }

    let allow_val = mapping
        .get(serde_yaml::Value::String("allow".to_string()))
        .ok_or_else(|| {
            RegistryError::Invalid(format!(
                "plugin '{plugin_name}' has invalid 'egress': mapping form must contain 'allow:' (use `egress: none` for no egress, or `egress: all` for unrestricted)"
            ))
        })?;

    let allow_seq = allow_val.as_sequence().ok_or_else(|| {
        RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'egress.allow': expected a sequence of {{ host, ports }} entries (got {})",
            yaml_val_type_name(allow_val),
        ))
    })?;

    // Reject extra keys at the top level so a typo (e.g. `denny: [...]`)
    // doesn't silently become a no-op. We accept exactly `allow:` here.
    for (key, _) in mapping {
        let key_str = key.as_str().unwrap_or("(non-string)");
        if key_str != "allow" {
            return Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' has invalid 'egress': unknown key {key_str:?} (mapping form supports only 'allow:')"
            )));
        }
    }

    for (i, entry) in allow_seq.iter().enumerate() {
        validate_allow_entry(plugin_name, i, entry)?;
    }

    let json_val = serde_json::to_value(raw).map_err(|e| {
        RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'egress': cannot represent as JSON: {e}"
        ))
    })?;

    // Size guard. The allow list is the only growable surface so this caps
    // operator mistakes (generated 10k-entry policies, etc) but is unlikely
    // to bite on hand-written entries.
    let serialized =
        serde_json::to_string(&json_val).expect("Value already validated as JSON-serializable");
    if serialized.len() > MAX_ENV_VALUE_BYTES {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'egress': serialized JSON exceeds maximum size of {MAX_ENV_VALUE_BYTES} bytes"
        )));
    }

    Ok(json_val)
}

/// Validate one entry in the `egress.allow:` sequence.
///
/// An entry must be a mapping with exactly:
///   * `host:` -- non-empty string, no whitespace, no scheme/path
///   * `ports:` -- non-empty sequence of integers in 1..=65535
///
/// `host` is validated for shape, not DNS resolvability: the materialiser
/// is responsible for failing closed if a hostname doesn't resolve.
fn validate_allow_entry(
    plugin_name: &str,
    index: usize,
    entry: &serde_yaml::Value,
) -> Result<(), RegistryError> {
    let mapping = entry.as_mapping().ok_or_else(|| {
        RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'egress.allow[{index}]': expected a mapping (got {})",
            yaml_val_type_name(entry),
        ))
    })?;

    for (key, _) in mapping {
        let key_str = key.as_str().unwrap_or("(non-string)");
        if key_str != "host" && key_str != "ports" {
            return Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' has invalid 'egress.allow[{index}]': unknown key {key_str:?} (entries support only 'host' and 'ports')"
            )));
        }
    }

    let host_val = mapping
        .get(serde_yaml::Value::String("host".to_string()))
        .ok_or_else(|| {
            RegistryError::Invalid(format!(
                "plugin '{plugin_name}' has invalid 'egress.allow[{index}]': missing required 'host'"
            ))
        })?;
    let host = host_val.as_str().ok_or_else(|| {
        RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'egress.allow[{index}].host': expected non-empty string (got {})",
            yaml_val_type_name(host_val),
        ))
    })?;
    if host.trim().is_empty() {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'egress.allow[{index}].host': must not be empty"
        )));
    }
    if host.chars().any(char::is_whitespace) {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'egress.allow[{index}].host': must not contain whitespace (got {host:?})"
        )));
    }
    if host.contains("://") || host.contains('/') {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'egress.allow[{index}].host': must be a bare hostname (no scheme or path; got {host:?})"
        )));
    }
    // Wildcards are deliberately not supported in v0. The risk with
    // suffix matching is "matched too much" being invisible until something
    // exfils; start strict and relax later if a real consumer needs it.
    if host.contains('*') {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'egress.allow[{index}].host': wildcards are not supported in v0; list each hostname explicitly (got {host:?})"
        )));
    }

    let ports_val = mapping
        .get(serde_yaml::Value::String("ports".to_string()))
        .ok_or_else(|| {
            RegistryError::Invalid(format!(
                "plugin '{plugin_name}' has invalid 'egress.allow[{index}]': missing required 'ports' (TCP ports list -- use [443] for HTTPS-only)"
            ))
        })?;
    let ports = ports_val.as_sequence().ok_or_else(|| {
        RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'egress.allow[{index}].ports': expected a sequence of integers (got {})",
            yaml_val_type_name(ports_val),
        ))
    })?;
    if ports.is_empty() {
        return Err(RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'egress.allow[{index}].ports': must not be empty (use a different `host:` if no ports apply, or `egress: none` for the whole plugin)"
        )));
    }
    for port_val in ports {
        let port = port_val.as_u64().ok_or_else(|| {
            RegistryError::Invalid(format!(
                "plugin '{plugin_name}' has invalid 'egress.allow[{index}].ports': each port must be an integer 1-65535"
            ))
        })?;
        if port == 0 || port > 65535 {
            return Err(RegistryError::Invalid(format!(
                "plugin '{plugin_name}' has invalid 'egress.allow[{index}].ports': each port must be an integer 1-65535 (got {port})"
            )));
        }
    }

    Ok(())
}

fn yaml_val_type_name(v: &serde_yaml::Value) -> &'static str {
    match v {
        serde_yaml::Value::Null => "null",
        serde_yaml::Value::Bool(_) => "bool",
        serde_yaml::Value::Number(_) => "number",
        serde_yaml::Value::String(_) => "string",
        serde_yaml::Value::Sequence(_) => "sequence",
        serde_yaml::Value::Mapping(_) => "mapping",
        serde_yaml::Value::Tagged(_) => "tagged",
    }
}

fn json_val_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn parse_resources(
    plugin_name: &str,
    config_val: &serde_yaml::Value,
) -> Result<PluginResources, RegistryError> {
    let resources_val = &config_val["resources"];
    if resources_val.is_null() {
        return Ok(PluginResources::default());
    }
    let mapping = resources_val.as_mapping().ok_or_else(|| {
        RegistryError::Invalid(format!(
            "plugin '{plugin_name}' has invalid 'resources': expected a mapping"
        ))
    })?;

    let mut resources = PluginResources::default();
    for (key_val, value_val) in mapping {
        let key = key_val.as_str().ok_or_else(|| {
            RegistryError::Invalid(format!(
                "plugin '{plugin_name}' has invalid 'resources' key: expected string"
            ))
        })?;
        match key {
            "cpus" => {
                let value = value_val.as_str().ok_or_else(|| {
                    RegistryError::Invalid(format!(
                        "plugin '{plugin_name}' has invalid 'resources.cpus': expected non-empty string"
                    ))
                })?;
                if value.is_empty() {
                    return Err(RegistryError::Invalid(format!(
                        "plugin '{plugin_name}' has invalid 'resources.cpus': expected non-empty string"
                    )));
                }
                resources.cpus = Some(value.to_string());
            }
            "memory" => {
                let value = value_val.as_str().ok_or_else(|| {
                    RegistryError::Invalid(format!(
                        "plugin '{plugin_name}' has invalid 'resources.memory': expected non-empty string"
                    ))
                })?;
                if value.is_empty() {
                    return Err(RegistryError::Invalid(format!(
                        "plugin '{plugin_name}' has invalid 'resources.memory': expected non-empty string"
                    )));
                }
                resources.memory = Some(value.to_string());
            }
            "pids" => {
                let value = value_val.as_u64().ok_or_else(|| {
                    RegistryError::Invalid(format!(
                        "plugin '{plugin_name}' has invalid 'resources.pids': expected integer 1-4294967295"
                    ))
                })?;
                if value == 0 || value > u32::MAX as u64 {
                    return Err(RegistryError::Invalid(format!(
                        "plugin '{plugin_name}' has invalid 'resources.pids': expected integer 1-4294967295"
                    )));
                }
                resources.pids = Some(value as u32);
            }
            _ => {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{plugin_name}' has invalid 'resources' key: {key}"
                )))
            }
        }
    }

    Ok(resources)
}

pub fn load(path: &str) -> Result<PluginRegistry, RegistryError> {
    if !std::path::Path::new(path).exists() {
        return Err(RegistryError::NotFound(path.to_string()));
    }

    let content = std::fs::read_to_string(path)
        .map_err(|e| RegistryError::Invalid(format!("failed to read {path}: {e}")))?;

    let payload: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|e| RegistryError::Invalid(format!("failed to parse YAML: {e}")))?;

    if !payload.is_mapping() {
        return Err(RegistryError::Invalid(
            "invalid plugin registry: top-level YAML value must be a map".to_string(),
        ));
    }

    let plugins = payload["plugins"]
        .as_mapping()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            RegistryError::Invalid(
                "invalid plugin registry: 'plugins' must be a non-empty map".to_string(),
            )
        })?;

    let mut result = PluginRegistry::new();

    for (name_val, config_val) in plugins {
        let name = name_val.as_str().ok_or_else(|| {
            RegistryError::Invalid(format!(
                "invalid plugin name '{name_val:?}': expected ^[a-z][a-z0-9-]{{0,30}}$"
            ))
        })?;

        if !plugin_name_re().is_match(name) {
            return Err(RegistryError::Invalid(format!(
                "invalid plugin name '{name}': expected ^[a-z][a-z0-9-]{{0,30}}$"
            )));
        }

        if !config_val.is_mapping() {
            return Err(RegistryError::Invalid(format!(
                "invalid plugin config for '{name}': expected map"
            )));
        }

        let image = config_val["image"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                RegistryError::Invalid(format!(
                    "plugin '{name}' is missing required non-empty 'image'"
                ))
            })?
            .trim()
            .to_string();

        let port = if config_val["port"].is_null() {
            8000u16
        } else {
            let p = config_val["port"].as_u64().ok_or_else(|| {
                RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'port': expected integer 1-65535"
                ))
            })?;
            if p == 0 || p > 65535 {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'port': expected integer 1-65535"
                )));
            }
            p as u16
        };

        // The `network:` field on a plugin entry was removed in 0.1.4.  Network
        // membership is now a deploy-topology decision owned by the launcher
        // via BOTWORK_LAUNCHER_DEFAULT_NETWORK — plugins do not (and must not)
        // get to pick their own docker network. Fail fast at registry load
        // so an operator with a stale plugins.yaml sees a clear error rather
        // than discovering at first spawn that their override was silently
        // ignored.
        if !config_val["network"].is_null() {
            return Err(RegistryError::Invalid(format!(
                "plugin '{name}' has 'network' set, but the field was removed in \
                 0.1.4: network membership is configured at the launcher level \
                 via BOTWORK_LAUNCHER_DEFAULT_NETWORK. Remove 'network:' from the \
                 plugin entry."
            )));
        }

        let path = if config_val["path"].is_null() {
            "/".to_string()
        } else {
            let raw_path = config_val["path"].as_str().ok_or_else(|| {
                RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': expected non-empty string"
                ))
            })?;
            let path = raw_path.trim();
            if path.is_empty() {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': expected non-empty string"
                )));
            }
            if !path.starts_with('/') {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': must start with '/'"
                )));
            }
            if path.chars().any(|c| c.is_whitespace()) {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': must not contain whitespace"
                )));
            }
            if path.contains('?') || path.contains('#') {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': must not contain '?' or '#'"
                )));
            }
            if path != "/" && path.ends_with('/') {
                return Err(RegistryError::Invalid(format!(
                    "plugin '{name}' has invalid 'path': must not end with '/' unless path is exactly '/'"
                )));
            }
            path.to_string()
        };

        let upstream_auth = UpstreamAuth::from_yaml_value(name, &config_val["upstream_auth"])?;
        let env = parse_env(name, config_val)?;
        let resources = parse_resources(name, config_val)?;
        let config = parse_config(name, config_val)?;
        let egress = parse_egress(name, config_val)?;

        result.insert(
            name.to_string(),
            PluginEntry {
                image,
                port,
                path,
                upstream_auth,
                env,
                resources,
                config,
                egress,
            },
        );
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_plugins(dir: &std::path::Path, content: &str) -> String {
        let path = dir.join("plugins.yaml");
        std::fs::write(&path, content).expect("write plugins");
        path.to_string_lossy().to_string()
    }

    #[test]
    fn load_path_defaults_and_explicit_values() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  a:
    image: botwork/mcp-a:local
    egress: all
  b:
    image: botwork/mcp-b:local
    path: /mcp
    egress: all
  c:
    image: botwork/mcp-c:local
    path: /api/v1
    egress: all
",
        );
        let loaded = load(&path).expect("load plugins");
        assert_eq!(loaded["a"].path, "/");
        assert_eq!(loaded["b"].path, "/mcp");
        assert_eq!(loaded["c"].path, "/api/v1");
    }

    #[test]
    fn load_rejects_invalid_path_values() {
        let cases = [
            ("/mcp/", "must not end with '/'"),
            ("mcp", "must start with '/'"),
            ("", "expected non-empty string"),
            ("   ", "expected non-empty string"),
            ("/mcp?x=1", "must not contain '?' or '#'"),
            ("/mcp#v1", "must not contain '?' or '#'"),
            ("/m cp", "must not contain whitespace"),
        ];

        for (bad_path, expected) in cases {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!(
                    "plugins:\n  p:\n    image: botwork/mcp-p:local\n    path: \"{bad_path}\"\n    egress: all\n"
                ),
            );
            let err = load(&path).expect_err("invalid path should fail");
            let err = err.to_string();
            assert!(
                err.contains("plugin 'p' has invalid 'path'") && err.contains(expected),
                "error '{err}' should mention '{expected}'"
            );
        }
    }

    #[test]
    fn load_rejects_network_field() {
        // Post-0.1.4: plugins must not declare their own network. The
        // launcher's BOTWORK_LAUNCHER_DEFAULT_NETWORK is the single source
        // of truth for network membership, so a stale plugins.yaml that
        // still sets `network:` is rejected at load time with a clear
        // message pointing at the migration.
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    network: botwork\n    egress: all\n",
        );
        let err = load(&path).expect_err("network field should be rejected");
        let err = err.to_string();
        assert!(
            err.contains("plugin 'p' has 'network' set"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains("BOTWORK_LAUNCHER_DEFAULT_NETWORK"),
            "error should name the launcher env var: {err}"
        );
    }

    #[test]
    fn load_upstream_auth_defaults_when_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    egress: all\n",
        );

        let loaded = load(&path).expect("load plugins");
        assert_eq!(loaded["p"].upstream_auth, UpstreamAuth::None);
    }

    #[test]
    fn load_upstream_auth_explicit_none() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    upstream_auth: none\n    egress: all\n",
        );

        let loaded = load(&path).expect("load plugins");
        assert_eq!(loaded["p"].upstream_auth, UpstreamAuth::None);
    }

    #[test]
    fn load_upstream_auth_bearer_with_service() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    upstream_auth: bearer/github.com\n    egress: all\n",
        );

        let loaded = load(&path).expect("load plugins");
        assert_eq!(
            loaded["p"].upstream_auth,
            UpstreamAuth::Bearer {
                service: "github.com".to_string(),
            }
        );
        assert_eq!(loaded["p"].upstream_auth.to_wire(), "bearer/github.com");
    }

    #[test]
    fn load_rejects_bearer_without_service() {
        for upstream_auth in ["upstream_auth: bearer", "upstream_auth: bearer/"] {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!("plugins:\n  p:\n    image: botwork/mcp-p:local\n    {upstream_auth}\n"),
            );

            let err = load(&path).expect_err("invalid upstream_auth should fail");
            let err = err.to_string();
            assert!(err.contains("plugin 'p' has invalid 'upstream_auth'"));
            assert!(err.contains("bearer requires a service"));
        }
    }

    #[test]
    fn load_rejects_unknown_upstream_auth() {
        let cases = [
            "upstream_auth: vault",
            "upstream_auth: None",
            "upstream_auth: \"\"",
            "upstream_auth: \"   \"",
            "upstream_auth: 42",
            "upstream_auth:\n      mode: bearer",
            "upstream_auth: bearer/github.com/pat",
            "upstream_auth: \"bearer/foo bar\"",
        ];

        for upstream_auth in cases {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!("plugins:\n  p:\n    image: botwork/mcp-p:local\n    {upstream_auth}\n"),
            );
            let err = load(&path).expect_err("invalid upstream_auth should fail");
            let err = err.to_string();
            assert!(
                err.contains("plugin 'p' has invalid 'upstream_auth'"),
                "error '{err}' should mention upstream_auth invalid"
            );
            assert!(
                err.contains("expected 'none' or 'bearer/<service>'"),
                "error '{err}' should list accepted values"
            );
        }
    }

    #[test]
    fn load_env_defaults_empty_when_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    egress: all\n",
        );
        let loaded = load(&path).expect("load plugins");
        assert!(loaded["p"].env.is_empty());
    }

    #[test]
    fn load_resources_defaults_when_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    egress: all\n",
        );
        let loaded = load(&path).expect("load plugins");
        assert_eq!(loaded["p"].resources, PluginResources::default());
    }

    #[test]
    fn load_resources_accepts_partial_overrides() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    egress: all
    resources:
      memory: 4g
      pids: 1024
",
        );
        let loaded = load(&path).expect("load plugins");
        assert_eq!(
            loaded["p"].resources,
            PluginResources {
                cpus: None,
                memory: Some("4g".to_string()),
                pids: Some(1024),
            }
        );
    }

    #[test]
    fn load_resources_rejects_invalid_shape_and_unknown_keys() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    resources: 123\n",
        );
        let err = load(&path).expect_err("invalid resources");
        assert!(err
            .to_string()
            .contains("invalid 'resources': expected a mapping"));

        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    resources:
      memory_limit: 4g
",
        );
        let err = load(&path).expect_err("unknown resources key");
        assert!(err
            .to_string()
            .contains("invalid 'resources' key: memory_limit"));
    }

    #[test]
    fn load_env_accepts_mapping() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    egress: all
    env:
      GITHUB_TOOLSETS: default,actions
      GITHUB_TERSE_DESCRIPTIONS: \"true\"
",
        );
        let loaded = load(&path).expect("load plugins");
        let env = &loaded["p"].env;
        assert_eq!(env.len(), 2);
        assert!(env.contains(&("GITHUB_TOOLSETS".to_string(), "default,actions".to_string())));
        assert!(env.contains(&("GITHUB_TERSE_DESCRIPTIONS".to_string(), "true".to_string())));
    }

    #[test]
    fn load_env_rejects_botwork_secret_prefix() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    env:
      BOTWORK_SECRET_SHADOW: val
",
        );
        let err = load(&path).expect_err("BOTWORK_SECRET_ prefix should fail");
        let err = err.to_string();
        assert!(err.contains("plugin 'p'"));
        assert!(err.contains("BOTWORK_SECRET_"));
    }

    #[test]
    fn load_env_rejects_botwork_mcp_config() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    env:
      BOTWORK_MCP_CONFIG: \"{}\"
",
        );
        let err = load(&path).expect_err("BOTWORK_MCP_CONFIG in env should fail");
        let err = err.to_string();
        assert!(err.contains("plugin 'p'"));
        assert!(err.contains("BOTWORK_MCP_CONFIG"));
        assert!(err.contains("'config:' field"));
    }

    #[test]
    fn load_config_defaults_none_when_absent() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    egress: all\n",
        );
        let loaded = load(&path).expect("load plugins");
        assert!(loaded["p"].config.is_none());
    }

    #[test]
    fn load_config_normalises_empty_mapping_to_none() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    config: {}\n    egress: all\n",
        );
        let loaded = load(&path).expect("load plugins");
        assert!(loaded["p"].config.is_none());
    }

    #[test]
    fn load_config_accepts_nested_structure() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    egress: all
    config:
      routes:
        - owner: botworkz
          token_env: BOTWORK_SECRET_GITHUB_BOTWORKZ
        - owner: phlax
          token_env: BOTWORK_SECRET_GITHUB_PHLAX
",
        );
        let loaded = load(&path).expect("load plugins");
        let config = loaded["p"].config.as_ref().expect("config should be Some");
        let routes = config["routes"].as_array().expect("routes array");
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0]["owner"].as_str().unwrap(), "botworkz");
    }

    #[test]
    fn load_config_rejects_non_mapping() {
        let cases = [
            ("config: \"a string\"", "string"),
            ("config: 42", "number"),
            ("config: true", "bool"),
            ("config:\n      - item1\n      - item2", "array"),
        ];
        for (entry, kind) in cases {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!("plugins:\n  p:\n    image: botwork/mcp-p:local\n    {entry}\n"),
            );
            let err = load(&path).expect_err(&format!("non-mapping config ({kind}) should fail"));
            let err = err.to_string();
            assert!(
                err.contains("plugin 'p'") && err.contains("invalid 'config'"),
                "error '{err}' should mention plugin and invalid config"
            );
        }
    }

    #[test]
    fn load_config_rejects_oversized_value() {
        let entries: String = (0..1000)
            .map(|i| format!("      k{i}: \"{}\"\n", "x".repeat(64)))
            .collect();
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            &format!("plugins:\n  p:\n    image: botwork/mcp-p:local\n    config:\n{entries}"),
        );
        let err = load(&path).expect_err("oversized config should fail");
        let err = err.to_string();
        assert!(
            err.contains("plugin 'p'") && err.contains("exceeds maximum size"),
            "error should mention plugin and size: {err}"
        );
    }

    #[test]
    fn load_missing_file_raises_not_found() {
        let dir = tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist.yaml");
        let err = load(missing.to_str().unwrap()).expect_err("missing file should fail");
        assert!(matches!(err, RegistryError::NotFound(_)));
    }

    #[test]
    fn load_empty_plugins_map_raises() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(dir.path(), "plugins: {}\n");
        let err = load(&path).expect_err("empty plugins should fail");
        assert!(err
            .to_string()
            .contains("'plugins' must be a non-empty map"));
    }

    #[test]
    fn load_bad_name_raises() {
        for bad_name in ["Fs", "a/b", &"a".repeat(32)] {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!("plugins:\n  {bad_name}:\n    image: botwork/x:local\n"),
            );
            let err = load(&path).expect_err("bad name should fail");
            assert!(
                err.to_string().contains("invalid plugin name"),
                "error: {err}"
            );
        }
    }

    #[test]
    fn load_egress_rejects_missing_field() {
        // Default-deny: every plugin must declare an explicit egress policy.
        // Absence is an error, not an implicit anything.
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n",
        );
        let err = load(&path).expect_err("missing egress must fail");
        let err = err.to_string();
        assert!(
            err.contains("plugin 'p'") && err.contains("missing required 'egress' field"),
            "should call out the missing field: {err}"
        );
    }

    #[test]
    fn load_egress_accepts_all_keyword() {
        // String form `egress: all` -> wire shape `{ "mode": "all" }`.
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    egress: all\n",
        );
        let loaded = load(&path).expect("load plugins");
        assert_eq!(loaded["p"].egress, serde_json::json!({ "mode": "all" }));
    }

    #[test]
    fn load_egress_accepts_none_keyword() {
        // String form `egress: none` -> wire shape `{ "mode": "none" }`.
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    egress: none\n",
        );
        let loaded = load(&path).expect("load plugins");
        assert_eq!(loaded["p"].egress, serde_json::json!({ "mode": "none" }));
    }

    #[test]
    fn load_egress_rejects_other_string_values() {
        // Only "all" / "none" are accepted string forms. Anything else
        // is a typo (or aspirational shorthand) and would silently be
        // wrong if we accepted it.
        // `yes`/`no`/`true` are YAML-coerced to booleans and trip the
        // mapping-or-string-form check, not this one; only test values that
        // YAML actually parses as strings.
        for bad in ["allow", "deny", "default"] {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!("plugins:\n  p:\n    image: botwork/mcp-p:local\n    egress: {bad}\n"),
            );
            let err = load(&path).expect_err(&format!("egress: {bad} must fail"));
            let err = err.to_string();
            assert!(
                err.contains("plugin 'p'")
                    && err.contains("invalid 'egress'")
                    && err.contains("'all' or 'none'"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn load_egress_accepts_full_allow_list() {
        // Mapping form: { allow: [{ host, ports }] }. Round-trips
        // verbatim into the wire `egress:` field.
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    egress:
      allow:
        - host: api.github.com
          ports: [443]
        - host: codeload.github.com
          ports: [443, 80]
",
        );
        let loaded = load(&path).expect("load plugins");
        let egress = &loaded["p"].egress;
        let allow = egress["allow"].as_array().expect("allow array");
        assert_eq!(allow.len(), 2);
        assert_eq!(allow[0]["host"].as_str().unwrap(), "api.github.com");
        assert_eq!(allow[0]["ports"][0].as_u64(), Some(443));
        assert_eq!(allow[1]["ports"][1].as_u64(), Some(80));
    }

    #[test]
    fn load_egress_rejects_non_string_non_mapping_egress() {
        // Numbers, bools, sequences at the top level are all errors.
        // The point is to force the choice to one of the documented
        // forms.
        let cases = [
            ("egress: 42", "number"),
            ("egress: true", "bool"),
            ("egress:\n      - item1\n      - item2", "sequence"),
        ];
        for (entry, kind) in cases {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!("plugins:\n  p:\n    image: botwork/mcp-p:local\n    {entry}\n"),
            );
            let err = load(&path).expect_err(&format!("egress ({kind}) must fail"));
            let err = err.to_string();
            assert!(
                err.contains("plugin 'p'") && err.contains("invalid 'egress'"),
                "{kind}: {err}"
            );
        }
    }

    #[test]
    fn load_egress_mapping_must_have_allow() {
        // Mapping form with no `allow:` is rejected: the alternative
        // would be "infer `egress: none`" and that defeats the point of
        // explicit declarations.
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    egress: {}\n",
        );
        let err = load(&path).expect_err("empty mapping must fail");
        let err = err.to_string();
        assert!(
            err.contains("plugin 'p'") && err.contains("must contain 'allow:'"),
            "{err}"
        );
    }

    #[test]
    fn load_egress_mapping_rejects_unknown_keys() {
        // Typo-detection: `denny:` is rejected as an unknown key rather
        // than silently treated as a no-op.
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    egress:
      allow:
        - host: api.github.com
          ports: [443]
      denny:
        - host: malicious.example.com
",
        );
        let err = load(&path).expect_err("unknown egress key must fail");
        let err = err.to_string();
        assert!(
            err.contains("plugin 'p'") && err.contains("unknown key") && err.contains("denny"),
            "{err}"
        );
    }

    #[test]
    fn load_egress_mapping_rejects_mode_key() {
        // `mode:` is the wire-side encoding of the all/none string forms;
        // operators must use the string sugar so the shape `{ allow: ... }`
        // never collides with `{ mode: ... }`.
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:\n  p:\n    image: botwork/mcp-p:local\n    egress:\n      mode: all\n",
        );
        let err = load(&path).expect_err("mode: reserved");
        let err = err.to_string();
        assert!(
            err.contains("plugin 'p'") && err.contains("'mode:' is reserved"),
            "{err}"
        );
    }

    #[test]
    fn load_egress_allow_entry_requires_host_and_ports() {
        // Each allow[] entry must have both keys. Missing either is an
        // error -- "ports defaults to 443" would be a foot-gun for the
        // one plugin that needs :80.
        let dir1 = tempdir().expect("tempdir");
        let path1 = write_plugins(
            dir1.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    egress:
      allow:
        - ports: [443]
",
        );
        let err = load(&path1).expect_err("missing host");
        assert!(err.to_string().contains("missing required 'host'"), "{err}");

        let dir2 = tempdir().expect("tempdir");
        let path2 = write_plugins(
            dir2.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    egress:
      allow:
        - host: api.github.com
",
        );
        let err = load(&path2).expect_err("missing ports");
        assert!(
            err.to_string().contains("missing required 'ports'"),
            "{err}"
        );
    }

    #[test]
    fn load_egress_allow_entry_rejects_unknown_keys() {
        // `path:` or `scheme:` would imply L7 awareness we don't have
        // in v0; reject so an aspirational entry doesn't silently
        // become "host + ports only, the rest is ignored".
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    egress:
      allow:
        - host: api.github.com
          ports: [443]
          path: /repos/*
",
        );
        let err = load(&path).expect_err("unknown allow-entry key");
        let err = err.to_string();
        assert!(
            err.contains("plugin 'p'") && err.contains("unknown key") && err.contains("path"),
            "{err}"
        );
    }

    #[test]
    fn load_egress_allow_entry_rejects_bad_host_shape() {
        // host: must be a bare hostname. URLs, paths, wildcards, and
        // empty/whitespace values are all rejected with shape-specific
        // messages.
        let cases: &[(&str, &str)] = &[
            ("\"\"", "must not be empty"),
            // All-whitespace trips the trim-then-empty check first; the
            // "whitespace" branch is for hosts with embedded spaces.
            ("\"   \"", "must not be empty"),
            ("\"api github.com\"", "whitespace"),
            ("\"https://api.github.com\"", "bare hostname"),
            ("\"api.github.com/foo\"", "bare hostname"),
            ("\"*.github.com\"", "wildcards are not supported"),
        ];
        for (host_literal, fragment) in cases {
            let dir = tempdir().expect("tempdir");
            let path = write_plugins(
                dir.path(),
                &format!(
                    "plugins:
  p:
    image: botwork/mcp-p:local
    egress:
      allow:
        - host: {host_literal}
          ports: [443]
"
                ),
            );
            let err = load(&path).expect_err(&format!("host {host_literal} should fail"));
            let err = err.to_string();
            assert!(
                err.contains(fragment),
                "host {host_literal}: expected fragment {fragment:?}, got: {err}"
            );
        }
    }

    #[test]
    fn load_egress_allow_entry_rejects_bad_ports() {
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    egress:
      allow:
        - host: api.github.com
          ports: []
",
        );
        let err = load(&path).expect_err("empty ports");
        assert!(err.to_string().contains("must not be empty"), "{err}");

        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    egress:
      allow:
        - host: api.github.com
          ports: [0]
",
        );
        let err = load(&path).expect_err("port 0");
        assert!(err.to_string().contains("1-65535"), "{err}");

        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            "plugins:
  p:
    image: botwork/mcp-p:local
    egress:
      allow:
        - host: api.github.com
          ports: [65536]
",
        );
        let err = load(&path).expect_err("port 65536");
        assert!(err.to_string().contains("1-65535"), "{err}");
    }

    #[test]
    fn load_egress_rejects_oversized_allow_list() {
        // Same 64 KiB cap as `config:`. An operator-generated 10k-entry
        // policy would bloat every spawn round-trip; force them to look
        // at the size limit.
        //
        // Build an allow list that exceeds 64 KiB serialised. Each entry
        // is ~50 bytes JSON-serialised, so 2000 entries = ~100 KiB.
        let entries: String = (0..2000)
            .map(|i| {
                format!(
                    "        - host: hostname-with-some-padding-{i}.example.invalid\n          ports: [443]\n"
                )
            })
            .collect();
        let dir = tempdir().expect("tempdir");
        let path = write_plugins(
            dir.path(),
            &format!(
                "plugins:\n  p:\n    image: botwork/mcp-p:local\n    egress:\n      allow:\n{entries}"
            ),
        );
        let err = load(&path).expect_err("oversized egress should fail");
        let err = err.to_string();
        assert!(
            err.contains("plugin 'p'")
                && err.contains("invalid 'egress'")
                && err.contains("exceeds maximum size"),
            "{err}"
        );
    }
}
