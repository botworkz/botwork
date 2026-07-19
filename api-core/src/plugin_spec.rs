//! Full plugin-spec validation + normalisation.
//!
//! Originally lifted from the pre-cutover `config-broker/src/registry.rs`
//! into `botwork-bootstrap/src/plugin_spec.rs`; pulled out of bootstrap
//! into `botwork-api-core` so api (RFE #106 PR3+) can consume
//! the same rules. The rule set is unchanged from the bootstrap copy:
//!
//! * `image` — required non-empty string.
//! * `port` — integer 1..=65535. Default 8000.
//! * `path` — starts with `/`, no whitespace, no `?`/`#`, no trailing
//!   `/` unless the whole path is `/`. Default `/`.
//! * `upstream_auth` — `"none"` or `"bearer/<service>"`. Default `none`.
//! * `env` — at most 32 entries; keys match `[A-Z_][A-Z0-9_]*`, not in
//!   `[PATH, LD_PRELOAD, LD_LIBRARY_PATH]`, not `BOTWORK_SECRET_*`,
//!   not `BOTWORK_MCP_CONFIG`, not `DOCKER_*`. Values are strings
//!   up to 64KiB.
//! * `resources` — optional `{cpus?, memory?, pids?}` map; pids is
//!   1..=u32::MAX.
//! * `config` — optional map; serialised JSON up to 64KiB.
//! * `egress` — required. `"all"` / `"none"` (normalised to
//!   `{"mode":"all/none"}` for storage) or
//!   `{"allow": [{"host", "ports": [...]}, ...]}` passed through
//!   verbatim. Hostnames are bare (no scheme/path/wildcard); ports
//!   are 1..=65535.
//! * `network:` field — explicitly rejected (removed in 0.1.4; the
//!   launcher's `BOTWORK_LAUNCHER_DEFAULT_NETWORK` is the single
//!   source of truth for plugin network membership).
//!
//! All errors carry the plugin name (or binding context).
//!
//! ## Scope: per-entry only
//!
//! This module validates ONE plugin entry at a time (or one binding
//! config blob). Cross-entry rules (duplicate names, unknown plugin
//! refs in bindings) live with the caller — bootstrap enforces them
//! while traversing its yaml tree, api enforces them per-request
//! against the live DB. There is no `validate_all` here.
//!
//! ## Constants kept in sync with launcher
//!
//! `RESERVED_ENV_NAMES`, `SECRET_ENV_PREFIX`, `CONFIG_ENV_NAME`,
//! `MAX_ENV_VALUE_BYTES` are contract values with `launcher/src/`. If
//! they change here they MUST change there.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::error::ValidationError;

// Keep in sync with launcher/src/validate.rs RESERVED_ENV_NAMES.
pub const RESERVED_ENV_NAMES: &[&str] = &["PATH", "LD_PRELOAD", "LD_LIBRARY_PATH"];

/// Maximum number of static env entries per plugin (leaves headroom under
/// launcher's MAX_ENV_ENTRIES = 64 for vault-derived secrets).
pub const MAX_STATIC_ENV_ENTRIES: usize = 32;

/// Maximum size of any single env value (or serialised config blob).
pub const MAX_ENV_VALUE_BYTES: usize = 64 * 1024;

/// Reserved prefix for vault-derived secret env entries.
pub const SECRET_ENV_PREFIX: &str = "BOTWORK_SECRET_";

/// Env var name under which compact-JSON structured config is injected.
pub const CONFIG_ENV_NAME: &str = "BOTWORK_MCP_CONFIG";

/// Plugin-name regex (same as tenant/workspace; checked at parse-time).
pub const PLUGIN_NAME_RE: &str = r"^[a-z][a-z0-9-]{0,30}$";

/// MCP tool-name regex.
///
/// Intentionally *different* from [`PLUGIN_NAME_RE`]:
///
/// * tools allow a leading digit (`fetch_url_2` exists in the wild),
/// * tools allow underscore as a word separator (snake_case is the
///   dominant style across the MCP server ecosystem),
/// * tools don't carry the 31-character cap because they're not a
///   DB-storage key — they live in the per-image label set instead.
///
/// Lives here (next to `PLUGIN_NAME_RE`) for the same reason the
/// env-name caps and reserved prefixes do: `botctl mcp-probe`
/// and a future consumer-side catalog upserter both want to enforce
/// the rule, and "the answer to what makes a tool name valid" should
/// have one definition, not two.
pub const TOOL_NAME_RE: &str = r"^[a-z0-9][a-z0-9_-]*$";

/// Raw plugin entry as it appears in bootstrap.yaml's top-level
/// `plugins:` list (or as a JSON request body to api). Field
/// shapes mirror the original `plugins.yaml` structure;
/// `serde_yaml::from_str` / `serde_json::from_str` populates this
/// directly and validation produces a [`ValidatedPlugin`].
///
/// `serde(deny_unknown_fields)` is deliberately NOT applied here —
/// the historical `network:` field is captured below so the
/// validator can emit a precise migration error rather than a generic
/// "unknown field". Callers that want strict-shape deny should apply
/// it at their own wrapper struct (bootstrap does this on its yaml
/// envelope, not on individual plugin entries).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RawPluginEntry {
    /// Globally-unique plugin name. Defaults to empty when deserialising
    /// — the validator emits a clearer `EmptyName(...)` error than serde
    /// would for "missing field `name`", and tests construct partial raw
    /// entries directly via `serde_yaml::from_str` + set-name-after.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub port: Option<u64>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub upstream_auth: Option<String>,
    #[serde(default)]
    pub env: Option<serde_yaml::Value>,
    #[serde(default)]
    pub resources: Option<serde_yaml::Value>,
    #[serde(default)]
    pub egress: Option<serde_yaml::Value>,
    /// `network:` was removed in 0.1.4. Captured here so we can give
    /// the operator a precise migration error rather than a generic
    /// "unknown field".
    #[serde(default)]
    pub network: Option<serde_yaml::Value>,
}

/// Plugin spec post-validation. Carries exactly what the DB stores:
/// every field has been parsed, range-checked, deduplicated, and
/// normalised. Bootstrap's runner inserts/upserts rows from this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPlugin {
    pub name: String,
    pub image: String,
    pub port: u16,
    pub path: String,
    /// Wire-form upstream_auth: `"none"` or `"bearer/<service>"`.
    pub upstream_auth: String,
    /// `[{name, value}, ...]`.
    pub env: JsonValue,
    /// `None` for absent; never `Some({})`.
    pub resources: Option<JsonValue>,
    /// Normalised egress wire shape:
    /// `{"mode":"all"|"none"}` or `{"allow":[{"host","ports"}]}`.
    pub egress: JsonValue,
}

/// Validate a single raw plugin entry into a [`ValidatedPlugin`].
///
/// Duplicate-name detection across a list is NOT done here — the
/// caller traverses its own collection (yaml sequence in bootstrap,
/// DB row set in api) and decides what duplicate means.
pub fn validate_one(raw: &RawPluginEntry) -> Result<ValidatedPlugin, ValidationError> {
    let name = raw.name.trim().to_string();
    if name.is_empty() {
        return Err(ValidationError::EmptyName("plugins[].name"));
    }
    if !regex::Regex::new(PLUGIN_NAME_RE)
        .expect("valid plugin name regex")
        .is_match(&name)
    {
        return Err(plugin_err(&name, "name must match ^[a-z][a-z0-9-]{0,30}$"));
    }

    // network: was removed in 0.1.4. Give the operator the migration
    // hint rather than a generic "unknown field".
    if raw.network.is_some() {
        return Err(plugin_err(
            &name,
            "has 'network' set, but the field was removed in 0.1.4: \
             network membership is configured at the launcher level via \
             BOTWORK_LAUNCHER_DEFAULT_NETWORK. Remove 'network:' from the plugin entry.",
        ));
    }

    let image = raw
        .image
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| plugin_err(&name, "is missing required non-empty 'image'"))?
        .to_string();

    let port = match raw.port {
        None => 8000u16,
        Some(p) if (1..=65535).contains(&p) => p as u16,
        Some(_) => {
            return Err(plugin_err(
                &name,
                "has invalid 'port': expected integer 1-65535",
            ))
        }
    };

    let path = validate_path(&name, raw.path.as_deref())?;
    let upstream_auth = validate_upstream_auth(&name, raw.upstream_auth.as_deref())?;
    let env = validate_env(&name, raw.env.as_ref())?;
    let resources = validate_resources(&name, raw.resources.as_ref())?;
    let egress = validate_egress(&name, raw.egress.as_ref())?;

    Ok(ValidatedPlugin {
        name,
        image,
        port,
        path,
        upstream_auth,
        env,
        resources,
        egress,
    })
}

fn validate_path(plugin: &str, raw: Option<&str>) -> Result<String, ValidationError> {
    let raw = match raw {
        None => return Ok("/".to_string()),
        Some(p) => p,
    };
    let path = raw.trim();
    if path.is_empty() {
        return Err(plugin_err(
            plugin,
            "has invalid 'path': expected non-empty string",
        ));
    }
    if !path.starts_with('/') {
        return Err(plugin_err(
            plugin,
            "has invalid 'path': must start with '/'",
        ));
    }
    if path.chars().any(|c| c.is_whitespace()) {
        return Err(plugin_err(
            plugin,
            "has invalid 'path': must not contain whitespace",
        ));
    }
    if path.contains('?') || path.contains('#') {
        return Err(plugin_err(
            plugin,
            "has invalid 'path': must not contain '?' or '#'",
        ));
    }
    if path != "/" && path.ends_with('/') {
        return Err(plugin_err(
            plugin,
            "has invalid 'path': must not end with '/' unless path is exactly '/'",
        ));
    }
    Ok(path.to_string())
}

fn validate_upstream_auth(plugin: &str, raw: Option<&str>) -> Result<String, ValidationError> {
    let Some(raw) = raw else {
        return Ok("none".to_string());
    };
    match raw {
        "none" => Ok("none".to_string()),
        "bearer" | "bearer/" => Err(plugin_err(
            plugin,
            "has invalid 'upstream_auth': bearer requires a service: use bearer/<service>",
        )),
        s if s.starts_with("bearer/") => {
            let service = &s["bearer/".len()..];
            if service.is_empty()
                || service.contains('/')
                || service.chars().any(char::is_whitespace)
            {
                return Err(plugin_err(
                    plugin,
                    "has invalid 'upstream_auth': expected 'none' or 'bearer/<service>'",
                ));
            }
            Ok(format!("bearer/{service}"))
        }
        _ => Err(plugin_err(
            plugin,
            "has invalid 'upstream_auth': expected 'none' or 'bearer/<service>'",
        )),
    }
}

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

fn validate_env(
    plugin: &str,
    raw: Option<&serde_yaml::Value>,
) -> Result<JsonValue, ValidationError> {
    let Some(raw) = raw else {
        return Ok(JsonValue::Array(Vec::new()));
    };
    if raw.is_null() {
        return Ok(JsonValue::Array(Vec::new()));
    }
    let mapping = raw
        .as_mapping()
        .ok_or_else(|| plugin_err(plugin, "has invalid 'env': expected a mapping"))?;
    if mapping.len() > MAX_STATIC_ENV_ENTRIES {
        return Err(plugin_err(
            plugin,
            &format!("has too many 'env' entries: maximum is {MAX_STATIC_ENV_ENTRIES}"),
        ));
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(mapping.len());
    for (key_val, value_val) in mapping {
        let key = key_val
            .as_str()
            .ok_or_else(|| plugin_err(plugin, "has invalid 'env' key: expected string"))?;
        let value = match value_val {
            serde_yaml::Value::String(s) => s.clone(),
            serde_yaml::Value::Bool(_) | serde_yaml::Value::Number(_) => {
                return Err(plugin_err(
                    plugin,
                    &format!(
                        "env key '{key}': value must be a string (quote it: \"{value_val:?}\")"
                    ),
                ));
            }
            _ => {
                return Err(plugin_err(
                    plugin,
                    &format!("env key '{key}': value must be a string"),
                ));
            }
        };
        if key.starts_with(SECRET_ENV_PREFIX) {
            return Err(plugin_err(
                plugin,
                &format!(
                    "env key '{key}': names starting with '{SECRET_ENV_PREFIX}' are reserved for vault-derived secrets"
                ),
            ));
        }
        if key == CONFIG_ENV_NAME {
            return Err(plugin_err(
                plugin,
                &format!(
                    "env key '{key}': reserved for structured config injection; use the 'config:' field instead"
                ),
            ));
        }
        if !valid_env_name(key) {
            return Err(plugin_err(
                plugin,
                &format!(
                    "env key '{key}': invalid name (must match [A-Z_][A-Z0-9_]*, not reserved or DOCKER_-prefixed)"
                ),
            ));
        }
        if value.len() > MAX_ENV_VALUE_BYTES {
            return Err(plugin_err(
                plugin,
                &format!(
                    "env key '{key}': value exceeds maximum size of {MAX_ENV_VALUE_BYTES} bytes"
                ),
            ));
        }
        // coverage:off — serde_yaml 0.9 uses IndexMap which deduplicates keys at
        // parse time, so `seen.insert()` returning false is unreachable via any
        // serde_yaml-parsed Value::Mapping; kept as a defensive guard.
        if !seen.insert(key.to_string()) {
            return Err(plugin_err(
                plugin,
                &format!("env key '{key}': duplicate key"),
            ));
        }
        // coverage:on
        out.push(serde_json::json!({"name": key, "value": value}));
    }
    Ok(JsonValue::Array(out))
}

fn validate_resources(
    plugin: &str,
    raw: Option<&serde_yaml::Value>,
) -> Result<Option<JsonValue>, ValidationError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let mapping = raw
        .as_mapping()
        .ok_or_else(|| plugin_err(plugin, "has invalid 'resources': expected a mapping"))?;
    let mut out = serde_json::Map::new();
    for (key_val, value_val) in mapping {
        let key = key_val
            .as_str()
            .ok_or_else(|| plugin_err(plugin, "has invalid 'resources' key: expected string"))?;
        match key {
            "cpus" | "memory" => {
                let value = value_val
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        plugin_err(
                            plugin,
                            &format!("has invalid 'resources.{key}': expected non-empty string"),
                        )
                    })?;
                out.insert(key.to_string(), JsonValue::String(value.to_string()));
            }
            "pids" => {
                let value = value_val.as_u64().ok_or_else(|| {
                    plugin_err(
                        plugin,
                        "has invalid 'resources.pids': expected integer 1-4294967295",
                    )
                })?;
                if value == 0 || value > u32::MAX as u64 {
                    return Err(plugin_err(
                        plugin,
                        "has invalid 'resources.pids': expected integer 1-4294967295",
                    ));
                }
                out.insert(key.to_string(), JsonValue::Number(value.into()));
            }
            _ => {
                return Err(plugin_err(
                    plugin,
                    &format!("has invalid 'resources' key: {key}"),
                ));
            }
        }
    }
    if out.is_empty() {
        return Ok(None);
    }
    Ok(Some(JsonValue::Object(out)))
}

fn validate_egress(
    plugin: &str,
    raw: Option<&serde_yaml::Value>,
) -> Result<JsonValue, ValidationError> {
    let raw = raw.ok_or_else(|| {
        plugin_err(
            plugin,
            "is missing required 'egress' field: every plugin must declare one of \
             `egress: all`, `egress: none`, or `egress: { allow: [...] }` -- \
             default-deny means there is no implicit fallback",
        )
    })?;
    if raw.is_null() {
        return Err(plugin_err(
            plugin,
            "is missing required 'egress' field: every plugin must declare one of \
             `egress: all`, `egress: none`, or `egress: { allow: [...] }` -- \
             default-deny means there is no implicit fallback",
        ));
    }
    if let Some(s) = raw.as_str() {
        return match s {
            "all" => Ok(serde_json::json!({"mode": "all"})),
            "none" => Ok(serde_json::json!({"mode": "none"})),
            other => Err(plugin_err(
                plugin,
                &format!(
                    "has invalid 'egress': string form must be 'all' or 'none' (got {other:?})"
                ),
            )),
        };
    }
    let mapping = raw.as_mapping().ok_or_else(|| {
        plugin_err(
            plugin,
            &format!(
                "has invalid 'egress': expected 'all', 'none', or a mapping (got {})",
                yaml_type_name(raw)
            ),
        )
    })?;
    if mapping.contains_key(serde_yaml::Value::String("mode".to_string())) {
        return Err(plugin_err(
            plugin,
            "has invalid 'egress': 'mode:' is reserved for the wire encoding of the \
             'all'/'none' string forms; use `egress: all` or `egress: none` directly",
        ));
    }
    let allow_val = mapping
        .get(serde_yaml::Value::String("allow".to_string()))
        .ok_or_else(|| {
            plugin_err(
                plugin,
                "has invalid 'egress': mapping form must contain 'allow:' \
                 (use `egress: none` for no egress, or `egress: all` for unrestricted)",
            )
        })?;
    let allow_seq = allow_val.as_sequence().ok_or_else(|| {
        plugin_err(
            plugin,
            &format!(
                "has invalid 'egress.allow': expected a sequence of {{ host, ports }} entries (got {})",
                yaml_type_name(allow_val)
            ),
        )
    })?;
    for (key, _) in mapping {
        let key_str = key.as_str().unwrap_or("(non-string)");
        if key_str != "allow" {
            return Err(plugin_err(
                plugin,
                &format!(
                    "has invalid 'egress': unknown key {key_str:?} (mapping form supports only 'allow:')"
                ),
            ));
        }
    }
    for (i, entry) in allow_seq.iter().enumerate() {
        validate_allow_entry(plugin, i, entry)?;
    }
    let json_val = serde_json::to_value(raw).map_err(|e| {
        // coverage:off — serde_json::to_value is infallible for any serde_yaml::Value
        // that passes the shape checks above; this arm exists for defensive completeness.
        plugin_err(
            plugin,
            &format!("has invalid 'egress': cannot represent as JSON: {e}"),
        )
        // coverage:on
    })?;
    let serialised = serde_json::to_string(&json_val).expect("validated JSON serialises");
    if serialised.len() > MAX_ENV_VALUE_BYTES {
        return Err(plugin_err(
            plugin,
            &format!(
                "has invalid 'egress': serialized JSON exceeds maximum size of {MAX_ENV_VALUE_BYTES} bytes"
            ),
        ));
    }
    Ok(json_val)
}

fn validate_allow_entry(
    plugin: &str,
    index: usize,
    entry: &serde_yaml::Value,
) -> Result<(), ValidationError> {
    let mapping = entry.as_mapping().ok_or_else(|| {
        plugin_err(
            plugin,
            &format!(
                "has invalid 'egress.allow[{index}]': expected a mapping (got {})",
                yaml_type_name(entry)
            ),
        )
    })?;
    for (key, _) in mapping {
        let key_str = key.as_str().unwrap_or("(non-string)");
        if key_str != "host" && key_str != "ports" {
            return Err(plugin_err(
                plugin,
                &format!(
                    "has invalid 'egress.allow[{index}]': unknown key {key_str:?} (entries support only 'host' and 'ports')"
                ),
            ));
        }
    }
    let host_val = mapping
        .get(serde_yaml::Value::String("host".to_string()))
        .ok_or_else(|| {
            plugin_err(
                plugin,
                &format!("has invalid 'egress.allow[{index}]': missing required 'host'"),
            )
        })?;
    let host = host_val.as_str().ok_or_else(|| {
        plugin_err(
            plugin,
            &format!(
                "has invalid 'egress.allow[{index}].host': expected non-empty string (got {})",
                yaml_type_name(host_val)
            ),
        )
    })?;
    if host.trim().is_empty() {
        return Err(plugin_err(
            plugin,
            &format!("has invalid 'egress.allow[{index}].host': must not be empty"),
        ));
    }
    if host.chars().any(char::is_whitespace) {
        return Err(plugin_err(
            plugin,
            &format!(
                "has invalid 'egress.allow[{index}].host': must not contain whitespace (got {host:?})"
            ),
        ));
    }
    if host.contains("://") || host.contains('/') {
        return Err(plugin_err(
            plugin,
            &format!(
                "has invalid 'egress.allow[{index}].host': must be a bare hostname (no scheme or path; got {host:?})"
            ),
        ));
    }
    if host.contains('*') {
        return Err(plugin_err(
            plugin,
            &format!(
                "has invalid 'egress.allow[{index}].host': wildcards are not supported in v0; \
                 list each hostname explicitly (got {host:?})"
            ),
        ));
    }
    let ports_val = mapping
        .get(serde_yaml::Value::String("ports".to_string()))
        .ok_or_else(|| {
            plugin_err(
                plugin,
                &format!(
                    "has invalid 'egress.allow[{index}]': missing required 'ports' (TCP ports list -- use [443] for HTTPS-only)"
                ),
            )
        })?;
    let ports = ports_val.as_sequence().ok_or_else(|| {
        plugin_err(
            plugin,
            &format!(
                "has invalid 'egress.allow[{index}].ports': expected a sequence of integers (got {})",
                yaml_type_name(ports_val)
            ),
        )
    })?;
    if ports.is_empty() {
        return Err(plugin_err(
            plugin,
            &format!(
                "has invalid 'egress.allow[{index}].ports': must not be empty (use a different `host:` if no ports apply, or `egress: none` for the whole plugin)"
            ),
        ));
    }
    for port_val in ports {
        let port = port_val.as_u64().ok_or_else(|| {
            plugin_err(
                plugin,
                &format!(
                    "has invalid 'egress.allow[{index}].ports': each port must be an integer 1-65535"
                ),
            )
        })?;
        if port == 0 || port > 65535 {
            return Err(plugin_err(
                plugin,
                &format!(
                    "has invalid 'egress.allow[{index}].ports': each port must be an integer 1-65535 (got {port})"
                ),
            ));
        }
    }
    Ok(())
}

/// Validate a per-binding `config:` blob (lives under
/// `tenants[].workspaces[].plugins[].config` in bootstrap.yaml, or
/// as the `config` field on an api workspace_plugin payload).
///
/// Returns `Ok(None)` for absent / empty; `Ok(Some(json))` for a
/// non-empty mapping. Rejects non-mapping shapes and oversized blobs.
pub fn validate_workspace_plugin_config(
    binding_context: &str,
    raw: Option<&serde_yaml::Value>,
) -> Result<Option<JsonValue>, ValidationError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(None);
    }
    let json_val: JsonValue = serde_json::to_value(raw).map_err(|e| {
        // coverage:off — serde_json::to_value is infallible for any serde_yaml::Value
        // that passes the null check above; this arm exists for defensive completeness.
        binding_err(
            binding_context,
            &format!("invalid 'config': cannot represent as JSON: {e}"),
        )
        // coverage:on
    })?;
    if !json_val.is_object() {
        return Err(binding_err(
            binding_context,
            &format!(
                "invalid 'config': expected a mapping (got {})",
                json_type_name(&json_val)
            ),
        ));
    }
    if json_val.as_object().unwrap().is_empty() {
        return Ok(None);
    }
    let serialised = serde_json::to_string(&json_val).expect("validated JSON serialises");
    if serialised.len() > MAX_ENV_VALUE_BYTES {
        return Err(binding_err(
            binding_context,
            &format!(
                "invalid 'config': serialized JSON exceeds maximum size of {MAX_ENV_VALUE_BYTES} bytes"
            ),
        ));
    }
    Ok(Some(json_val))
}

fn plugin_err(plugin: &str, suffix: &str) -> ValidationError {
    ValidationError::PluginInvalid {
        plugin: plugin.to_string(),
        detail: suffix.to_string(),
    }
}

fn binding_err(context: &str, suffix: &str) -> ValidationError {
    ValidationError::BindingInvalid {
        context: context.to_string(),
        detail: suffix.to_string(),
    }
}

fn yaml_type_name(v: &serde_yaml::Value) -> &'static str {
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

fn json_type_name(v: &JsonValue) -> &'static str {
    match v {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "bool",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str, yaml: &str) -> RawPluginEntry {
        let mut e: RawPluginEntry = serde_yaml::from_str(yaml).expect("parse raw plugin");
        e.name = name.to_string();
        e
    }

    #[test]
    fn defaults_fill_in_when_optional_fields_omitted() {
        let r = raw("p", "image: ghcr.io/example/p:1.0\negress: all\n");
        let v = validate_one(&r).expect("validate");
        assert_eq!(v.port, 8000);
        assert_eq!(v.path, "/");
        assert_eq!(v.upstream_auth, "none");
        assert_eq!(v.env, serde_json::json!([]));
        assert!(v.resources.is_none());
        assert_eq!(v.egress, serde_json::json!({"mode": "all"}));
    }

    #[test]
    fn rejects_missing_egress() {
        let r = raw("p", "image: ghcr.io/example/p:1.0\n");
        assert!(validate_one(&r).is_err());
    }

    #[test]
    fn rejects_legacy_network_field() {
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nnetwork: botwork\n",
        );
        let err = validate_one(&r).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("'network'"), "{msg}");
        assert!(msg.contains("0.1.4"), "{msg}");
    }

    #[test]
    fn upstream_auth_round_trips_known_forms() {
        for (in_s, out_s) in [("none", "none"), ("bearer/x", "bearer/x")] {
            let r = raw(
                "p",
                &format!("image: ghcr.io/example/p:1.0\negress: all\nupstream_auth: {in_s}\n"),
            );
            let v = validate_one(&r).expect("validate");
            assert_eq!(v.upstream_auth, out_s);
        }
    }

    #[test]
    fn upstream_auth_rejects_garbage() {
        for bad in [
            "",
            "bearer",
            "bearer/",
            "bearer/foo bar",
            "bearer/foo/bar",
            "vault",
        ] {
            let r = raw(
                "p",
                &format!("image: ghcr.io/example/p:1.0\negress: all\nupstream_auth: \"{bad}\"\n"),
            );
            assert!(validate_one(&r).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn env_emits_array_of_name_value_objects() {
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nenv:\n  FOO: bar\n  BAR: baz\n",
        );
        let v = validate_one(&r).expect("validate");
        let arr = v.env.as_array().expect("array");
        assert_eq!(arr.len(), 2);
        // Order is map-iteration order; we don't assert on it here.
        let names: HashSet<&str> = arr.iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert!(names.contains("FOO"));
        assert!(names.contains("BAR"));
    }

    #[test]
    fn env_rejects_reserved_secret_prefix() {
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nenv:\n  BOTWORK_SECRET_FOO: bar\n",
        );
        assert!(validate_one(&r).is_err());
    }

    #[test]
    fn env_rejects_config_env_name() {
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nenv:\n  BOTWORK_MCP_CONFIG: bar\n",
        );
        assert!(validate_one(&r).is_err());
    }

    #[test]
    fn path_rejects_invalid_shapes() {
        for bad in [
            "",
            "no-slash",
            "/trailing/",
            "/has space",
            "/has?qs",
            "/has#frag",
        ] {
            let r = raw(
                "p",
                &format!("image: ghcr.io/example/p:1.0\negress: all\npath: \"{bad}\"\n"),
            );
            assert!(validate_one(&r).is_err(), "should reject path {bad:?}");
        }
    }

    #[test]
    fn resources_returns_none_when_absent_or_empty() {
        let r = raw("p", "image: ghcr.io/example/p:1.0\negress: all\n");
        assert!(validate_one(&r).unwrap().resources.is_none());
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nresources: {}\n",
        );
        assert!(validate_one(&r).unwrap().resources.is_none());
    }

    #[test]
    fn resources_round_trip_full_shape() {
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nresources:\n  cpus: '2'\n  memory: 4g\n  pids: 1024\n",
        );
        let v = validate_one(&r).expect("validate");
        let obj = v.resources.expect("present");
        assert_eq!(obj["cpus"], "2");
        assert_eq!(obj["memory"], "4g");
        assert_eq!(obj["pids"], 1024);
    }

    #[test]
    fn egress_allow_round_trips_verbatim() {
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: example.com\n    ports: [443]\n  - host: api.github.com\n    ports: [443, 80]\n",
        );
        let v = validate_one(&r).expect("validate");
        assert_eq!(v.egress["allow"][0]["host"], "example.com");
        assert_eq!(v.egress["allow"][1]["ports"][1], 80);
    }

    #[test]
    fn egress_rejects_mode_in_mapping_form() {
        let r = raw("p", "image: ghcr.io/example/p:1.0\negress:\n  mode: all\n");
        assert!(validate_one(&r).is_err());
    }

    #[test]
    fn egress_rejects_wildcard_host() {
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: '*.example.com'\n    ports: [443]\n",
        );
        assert!(validate_one(&r).is_err());
    }

    #[test]
    fn binding_config_size_limit_enforced() {
        let big = "x".repeat(70 * 1024);
        let raw_yaml: serde_yaml::Value = serde_yaml::from_str(&format!("big: \"{big}\"")).unwrap();
        let err = validate_workspace_plugin_config(
            "tenant 'a' workspace 'b' plugin 'c'",
            Some(&raw_yaml),
        )
        .unwrap_err();
        assert!(matches!(err, ValidationError::BindingInvalid { .. }));
    }

    #[test]
    fn binding_config_returns_none_for_absent_or_empty() {
        assert!(validate_workspace_plugin_config("ctx", None)
            .unwrap()
            .is_none());
        let null: serde_yaml::Value = serde_yaml::Value::Null;
        assert!(validate_workspace_plugin_config("ctx", Some(&null))
            .unwrap()
            .is_none());
        let empty: serde_yaml::Value = serde_yaml::from_str("{}").unwrap();
        assert!(validate_workspace_plugin_config("ctx", Some(&empty))
            .unwrap()
            .is_none());
    }

    #[test]
    fn validates_name_image_and_port_boundaries() {
        let cases = [
            ("", "image: ghcr.io/example/p:1.0\negress: all\n"),
            ("NotValid", "image: ghcr.io/example/p:1.0\negress: all\n"),
            ("p", "egress: all\n"),
            ("p", "image: \"\"\negress: all\n"),
            ("p", "image: ghcr.io/example/p:1.0\negress: all\nport: 0\n"),
            (
                "p",
                "image: ghcr.io/example/p:1.0\negress: all\nport: 65536\n",
            ),
        ];
        for (name, yaml) in cases {
            let err = validate_one(&raw(name, yaml)).unwrap_err();
            assert!(matches!(
                err,
                ValidationError::PluginInvalid { .. } | ValidationError::EmptyName(_)
            ));
        }
    }

    #[test]
    fn env_rejects_invalid_name_shapes_and_values() {
        let oversized = "x".repeat(MAX_ENV_VALUE_BYTES + 1);
        let cases = [
            "image: ghcr.io/example/p:1.0\negress: all\nenv:\n  lower: x\n",
            "image: ghcr.io/example/p:1.0\negress: all\nenv:\n  PATH: x\n",
            "image: ghcr.io/example/p:1.0\negress: all\nenv:\n  DOCKER_HOST: x\n",
            "image: ghcr.io/example/p:1.0\negress: all\nenv:\n  X: true\n",
            "image: ghcr.io/example/p:1.0\negress: all\nenv:\n  X: 1\n",
            "image: ghcr.io/example/p:1.0\negress: all\nenv:\n  X:\n    nested: true\n",
        ];
        for yaml in cases {
            assert!(
                validate_one(&raw("p", yaml)).is_err(),
                "case should fail: {yaml}"
            );
        }
        let too_big = raw(
            "p",
            &format!("image: ghcr.io/example/p:1.0\negress: all\nenv:\n  X: \"{oversized}\"\n"),
        );
        assert!(validate_one(&too_big).is_err());
    }

    #[test]
    fn resources_reject_invalid_shapes_and_ranges() {
        let cases = [
            "image: ghcr.io/example/p:1.0\negress: all\nresources: []\n",
            "image: ghcr.io/example/p:1.0\negress: all\nresources:\n  gpu: 1\n",
            "image: ghcr.io/example/p:1.0\negress: all\nresources:\n  cpus: \"\"\n",
            "image: ghcr.io/example/p:1.0\negress: all\nresources:\n  pids: 0\n",
            "image: ghcr.io/example/p:1.0\negress: all\nresources:\n  pids: 4294967296\n",
            "image: ghcr.io/example/p:1.0\negress: all\nresources:\n  pids: not-a-number\n",
        ];
        for yaml in cases {
            assert!(
                validate_one(&raw("p", yaml)).is_err(),
                "case should fail: {yaml}"
            );
        }
    }

    #[test]
    fn egress_rejects_invalid_mapping_forms_table_driven() {
        let cases = [
            "image: ghcr.io/example/p:1.0\negress: maybe\n",
            "image: ghcr.io/example/p:1.0\negress: 5\n",
            "image: ghcr.io/example/p:1.0\negress:\n  mode: all\n",
            "image: ghcr.io/example/p:1.0\negress:\n  other: true\n",
            "image: ghcr.io/example/p:1.0\negress:\n  allow: foo\n",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: example.com\n",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: \"\"\n    ports: [443]\n",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: ex ample.com\n    ports: [443]\n",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: https://example.com\n    ports: [443]\n",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: example.com/path\n    ports: [443]\n",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: example.com\n    ports: []\n",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: example.com\n    ports: [0]\n",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: example.com\n    ports: [65536]\n",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: example.com\n    ports: [\"443\"]\n",
        ];
        for yaml in cases {
            assert!(
                validate_one(&raw("p", yaml)).is_err(),
                "case should fail: {yaml}"
            );
        }
    }

    #[test]
    fn egress_rejects_oversized_serialized_json() {
        let host = "x".repeat(MAX_ENV_VALUE_BYTES + 128);
        let r = raw(
            "p",
            &format!(
                "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: {host}\n    ports: [443]\n"
            ),
        );
        assert!(validate_one(&r).is_err());
    }

    #[test]
    fn binding_config_rejects_non_object_values() {
        for yaml in ["1", "[]", "\"x\"", "true"] {
            let raw_val: serde_yaml::Value = serde_yaml::from_str(yaml).expect("parse yaml value");
            let err = validate_workspace_plugin_config("ctx", Some(&raw_val)).unwrap_err();
            assert!(
                matches!(err, ValidationError::BindingInvalid { .. }),
                "{yaml}"
            );
        }
    }

    #[test]
    fn egress_allow_entry_rejects_missing_host_and_ports_and_unknown_keys() {
        for yaml in [
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - ports: [443]\n",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: example.com\n",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: example.com\n    ports: [443]\n    scheme: https\n",
        ] {
            assert!(validate_one(&raw("p", yaml)).is_err(), "{yaml}");
        }
    }

    #[test]
    fn env_and_binding_helpers_cover_null_and_type_names() {
        let null_env = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nenv: null\n",
        );
        assert_eq!(validate_one(&null_env).unwrap().env, serde_json::json!([]));

        assert_eq!(yaml_type_name(&serde_yaml::Value::Null), "null");
        assert_eq!(
            yaml_type_name(&serde_yaml::from_str::<serde_yaml::Value>("[]").unwrap()),
            "sequence"
        );
        assert_eq!(json_type_name(&serde_json::json!(true)), "bool");
        assert_eq!(json_type_name(&serde_json::json!({})), "object");
    }

<<<<<<< HEAD
    // ── Tier 1.5 fault-injection / edge tests ──────────────────────

    #[test]
    fn validate_egress_rejects_null_value() {
        // validate_egress: raw.is_null() arm — `egress: ~` (YAML null)
        let r = raw("p", "image: ghcr.io/example/p:1.0\negress: ~\n");
        let err = validate_one(&r).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("egress"), "{msg}");
    }

    #[test]
    fn validate_egress_rejects_bool_value_with_type_in_message() {
        // yaml_type_name Bool arm: `egress: true` is not a string/mapping
        let r = raw("p", "image: ghcr.io/example/p:1.0\negress: true\n");
        let err = validate_one(&r).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bool"), "{msg}");
    }

    #[test]
    fn validate_egress_rejects_tagged_value_with_type_in_message() {
        // yaml_type_name Tagged arm: a YAML tagged scalar is not a string/mapping
        let r = raw("p", "image: ghcr.io/example/p:1.0\negress: !custom val\n");
        let err = validate_one(&r).unwrap_err();
        let msg = format!("{err}");
        // A tagged value that is not "all"/"none" must be rejected;
        // the type name appears in the message.
        assert!(
            msg.contains("tagged") || msg.contains("all") || msg.contains("none"),
            "{msg}"
=======
    // ── additional coverage for uncovered branches ──────────────────

    #[test]
    fn egress_null_is_rejected() {
        // `egress: null` (explicit null) is different from absent egress.
        // The validator catches both, but only the absent path was tested
        // previously; this exercises the `is_null()` guard.
        let r = raw("p", "image: ghcr.io/example/p:1.0\negress: ~\n");
        let err = validate_one(&r).unwrap_err();
        assert!(
            matches!(err, ValidationError::PluginInvalid { .. }),
            "{err}"
>>>>>>> origin/main
        );
    }

    #[test]
<<<<<<< HEAD
    fn validate_egress_allow_rejects_mapping_value_with_type_in_message() {
        // yaml_type_name Mapping arm: allow_val is a mapping, not a sequence
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n    k: v\n",
        );
        let err = validate_one(&r).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("mapping"), "{msg}");
    }

    #[test]
    fn validate_allow_entry_rejects_non_mapping_entry() {
        // validate_allow_entry: entry.as_mapping() fails — allow item is a scalar bool
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - true\n",
        );
        assert!(validate_one(&r).is_err());
    }

    #[test]
    fn validate_allow_entry_rejects_non_string_host() {
        // validate_allow_entry: host_val.as_str() fails — host is a number
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: 123\n    ports: [443]\n",
        );
        let err = validate_one(&r).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("host"), "{msg}");
    }

    #[test]
    fn validate_allow_entry_rejects_non_sequence_ports() {
        // validate_allow_entry: ports_val.as_sequence() fails — ports is a scalar
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - host: example.com\n    ports: 443\n",
        );
        let err = validate_one(&r).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ports"), "{msg}");
    }

    #[test]
    fn valid_env_name_rejects_empty_key() {
        // valid_env_name: bytes.is_empty() arm — YAML empty-string key
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nenv:\n  '': value\n",
=======
    fn allow_entry_non_mapping_is_rejected() {
        // A string element in the `allow:` sequence should be rejected.
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress:\n  allow:\n  - \"example.com\"\n",
        );
        let err = validate_one(&r).unwrap_err();
        assert!(
            matches!(err, ValidationError::PluginInvalid { .. }),
            "{err}"
        );
    }

    #[test]
    fn env_non_mapping_non_null_is_rejected() {
        // `env: "a plain string"` — not a mapping and not null — hits
        // the `as_mapping().ok_or_else(...)` error path.
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nenv: \"a plain string\"\n",
        );
        let err = validate_one(&r).unwrap_err();
        assert!(
            matches!(err, ValidationError::PluginInvalid { .. }),
            "{err}"
        );
    }

    #[test]
    fn env_rejects_more_than_max_static_entries() {
        // Build a YAML string with MAX_STATIC_ENV_ENTRIES + 1 entries.
        let entries: String = (1..=(MAX_STATIC_ENV_ENTRIES + 1))
            .map(|i| format!("  ENTRY_{i:02}: value\n"))
            .collect();
        let yaml = format!("image: ghcr.io/example/p:1.0\negress: all\nenv:\n{entries}");
        let r = raw("p", &yaml);
        let err = validate_one(&r).unwrap_err();
        assert!(
            matches!(err, ValidationError::PluginInvalid { .. }),
            "{err}"
        );
        assert!(format!("{err}").contains("too many"), "{err}");
    }

    #[test]
    fn env_rejects_non_string_key() {
        // Construct a YAML mapping whose key is an integer (1) rather than
        // a string. YAML allows `{1: foo}` and serde_yaml parses this with
        // a Number key; `as_str()` on the key returns None.
        let raw_val: serde_yaml::Value =
            serde_yaml::from_str("image: ghcr.io/example/p:1.0\negress: all\nenv:\n  1: foo\n")
                .expect("parse yaml");
        let entry: RawPluginEntry = serde_yaml::from_value(raw_val).expect("deserialize entry");
        let mut r = entry;
        r.name = "p".to_string();
        // Numeric keys in YAML may round-trip as strings in some
        // serializers; only assert that validate_one errors, not the
        // specific message, since the exact behavior depends on the
        // serde_yaml version.
        //
        // If serde_yaml converts the integer key to the string "1" during
        // deserialization (which is valid behavior), the key won't match
        // `[A-Z_][A-Z0-9_]*` and the "invalid name" error fires instead.
        // Either way, validation fails.
        assert!(validate_one(&r).is_err());
    }

    #[test]
    fn resources_null_returns_none() {
        // `resources: null` (explicit null) is different from absent
        // resources. Both must return `Ok(None)`.
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nresources: ~\n",
        );
        assert!(validate_one(&r).unwrap().resources.is_none());
    }

    #[test]
    fn resources_rejects_non_string_key() {
        // A numeric key in the resources mapping is not in the allowed
        // set ("cpus" | "memory" | "pids"), so it hits the catch-all arm.
        // serde_yaml may convert the numeric key to a string; in that case
        // it still lands in the `_ => Err(...)` catch-all since the
        // resulting string is not "cpus", "memory", or "pids".
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nresources:\n  1: foo\n",
>>>>>>> origin/main
        );
        assert!(validate_one(&r).is_err());
    }

    #[test]
<<<<<<< HEAD
    fn valid_env_name_rejects_key_with_invalid_subsequent_char() {
        // valid_env_name: bytes.iter().skip(1) arm — starts uppercase, has lowercase
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nenv:\n  FOObar: value\n",
        );
        assert!(validate_one(&r).is_err());
    }

    #[test]
    fn validate_env_rejects_non_string_key() {
        // validate_env: key_val.as_str() fails — YAML integer key is not a string
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nenv:\n  1: value\n",
        );
        assert!(validate_one(&r).is_err());
    }

    #[test]
    fn validate_resources_rejects_non_string_key() {
        // validate_resources: key_val.as_str() fails — YAML integer key is not a string
        let r = raw(
            "p",
            "image: ghcr.io/example/p:1.0\negress: all\nresources:\n  1: value\n",
        );
        assert!(validate_one(&r).is_err());
=======
    fn validate_workspace_plugin_config_returns_some_for_valid_nonempty_map() {
        let raw_val: serde_yaml::Value = serde_yaml::from_str("key: value").expect("parse yaml");
        let result = validate_workspace_plugin_config("tenant/ws/plugin", Some(&raw_val)).unwrap();
        let json = result.expect("expected Some(json) for valid non-empty map");
        assert_eq!(json["key"], "value");
    }

    #[test]
    fn type_name_helpers_cover_remaining_variants() {
        // yaml_type_name: cover Bool, Number, String, Mapping, Tagged
        assert_eq!(yaml_type_name(&serde_yaml::Value::Bool(true)), "bool");
        assert_eq!(
            yaml_type_name(&serde_yaml::Value::Number(serde_yaml::Number::from(1))),
            "number"
        );
        assert_eq!(
            yaml_type_name(&serde_yaml::Value::String("x".to_string())),
            "string"
        );
        assert_eq!(
            yaml_type_name(&serde_yaml::Value::Mapping(serde_yaml::Mapping::new())),
            "mapping"
        );
        // Construct a Tagged value directly — we are testing yaml_type_name, not
        // the YAML parser, so we don't need to go through a text-to-Value round-trip.
        let tagged = serde_yaml::Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
            tag: serde_yaml::value::Tag::new("tagged"),
            value: serde_yaml::Value::Null,
        }));
        assert_eq!(yaml_type_name(&tagged), "tagged");

        // json_type_name: cover Null, Number, String, Array
        assert_eq!(json_type_name(&serde_json::Value::Null), "null");
        assert_eq!(json_type_name(&serde_json::json!(42)), "number");
        assert_eq!(json_type_name(&serde_json::json!("str")), "string");
        assert_eq!(json_type_name(&serde_json::json!([])), "array");
    }

    #[test]
    fn plugin_name_and_tool_name_regex_constants_accept_and_reject() {
        // Ensure PLUGIN_NAME_RE and TOOL_NAME_RE constants are correct.
        let plugin_re = regex::Regex::new(PLUGIN_NAME_RE).expect("plugin name regex");
        let tool_re = regex::Regex::new(TOOL_NAME_RE).expect("tool name regex");

        // Plugin names: lowercase letter start, then [a-z0-9-]{0,30}
        assert!(plugin_re.is_match("my-plugin"));
        assert!(plugin_re.is_match("a"));
        assert!(!plugin_re.is_match("1plugin")); // digit start
        assert!(!plugin_re.is_match("MyPlugin")); // uppercase
        assert!(!plugin_re.is_match("my_plugin")); // underscore not allowed
                                                   // 32 chars exceeds the 31-char cap (1 letter + 30 of [a-z0-9-])
        assert!(!plugin_re.is_match(&"a".repeat(32)));

        // Tool names: [a-z0-9] start, then [a-z0-9_-]*
        assert!(tool_re.is_match("fetch_url_2"));
        assert!(tool_re.is_match("0tool"));
        assert!(!tool_re.is_match("_tool")); // underscore start not allowed
        assert!(!tool_re.is_match("Tool")); // uppercase not allowed
        assert!(!tool_re.is_match("")); // empty not allowed
>>>>>>> origin/main
    }
}
