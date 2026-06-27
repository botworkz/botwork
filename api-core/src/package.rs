//! `mcp-package.yaml` parsing + validation.
//!
//! Single producer-side entry point for the operator-curated package
//! file consumed by `botwork-tools mcp-probe`. The schema mirrors the
//! bootstrap.yaml plugin-entry shape one-for-one on every field they
//! share — same regexes, same caps, same reserved-env-name set —
//! because the labels this validator approves end up on a plugin
//! image whose final consumer (the catalog upserter, soon) re-runs
//! the same plugin-spec rules. Drifting the producer-side validator
//! would mean an image that passes the probe can fail the catalog
//! upserter, which is the failure mode this validator exists to
//! prevent.
//!
//! The trick to staying 1:1 is reuse: [`validate_package`] builds a
//! synthetic [`RawPluginEntry`] with the package-file fields plus a
//! sentinel `image:` value, hands it to [`validate_one`], and then
//! validates the package-only fields ([`PackageFileEntry::isolation`]
//! and [`PackageFileEntry::spill`]) on top. Adding a field to
//! [`validate_one`] automatically extends the package validator.
//!
//! # Scope split with [`crate::plugin_spec`]
//!
//! * [`plugin_spec::validate_one`] is the per-entry validator for a
//!   row that's going *into* the DB; it requires `image:` because a
//!   plugin row can't be inserted without one.
//! * [`validate_package`] is the per-image validator for the producer
//!   side; `image:` is the input to the probe, not the package file,
//!   so the package-only entry point doesn't require it.
//!
//! Anything more than that — a third validator path that diverges
//! from `validate_one` on shape — would re-introduce the drift this
//! split exists to prevent.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::error::ValidationError;
use crate::plugin_spec::{validate_one, RawPluginEntry, ValidatedPlugin};

/// Isolation policy for the plugin's session-broker routing.
///
/// Mirrors session-broker's runtime isolation modes 1:1: `shared`
/// keeps a single container alive across agents, `per_agent_session`
/// gives every (tenant, workspace, agent-session) its own container,
/// `per_request` is the strictest mode (one container per JSON-RPC
/// request, intended for low-trust plugins).
///
/// The producer-side label commits the plugin to a runtime posture
/// the operator cannot relax at deploy time; that's the point — the
/// plugin author knows what their tool does (mutates global state vs.
/// runs read-only) and the policy travels with the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    Shared,
    PerAgentSession,
    PerRequest,
}

impl Isolation {
    /// Wire string used in `org.botwork.mcp.isolation` labels and
    /// `mcp-package.yaml`.
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::PerAgentSession => "per_agent_session",
            Self::PerRequest => "per_request",
        }
    }
}

/// Spill policy — what session-broker does with response bodies that
/// exceed the per-response inline cap.
///
/// `never` means responses always inline (caller takes the bandwidth);
/// `always` means every response spills to the spill store regardless
/// of size; `size` means spill only when the response body exceeds
/// `threshold_bytes`. The optional `include_methods` and
/// `include_tools` allowlists let the producer scope spill to a
/// subset of the surface (e.g. spill only for `tools/call` responses,
/// only for the `fetch` tool); when absent, the policy applies to
/// every response.
///
/// The schema is intentionally narrow in v1 — adding new modes is a
/// label-schema bump (`org.botwork.mcp.schema-version`), not an
/// in-place change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpillEntry {
    pub mode: SpillMode,
    /// Byte threshold above which a response is spilled. Required
    /// when `mode = size`; rejected for the other modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_bytes: Option<u64>,
    /// Restrict the spill policy to a specific set of JSON-RPC
    /// methods (`tools/call`, `resources/read`, …). Empty list is
    /// rejected; absent means "all methods".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_methods: Option<Vec<String>>,
    /// Further restrict the spill policy to a specific set of tool
    /// names within `tools/call`. Empty list is rejected; absent
    /// means "all tools".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpillMode {
    Never,
    Always,
    Size,
}

impl SpillMode {
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::Always => "always",
            Self::Size => "size",
        }
    }
}

/// Raw `mcp-package.yaml` shape as it lives on disk.
///
/// `#[serde(deny_unknown_fields)]` keeps typos loud — a misspelt
/// `isolaton:` is a schema-load error, not a silent fallback to the
/// default. Same posture every other yaml type in this crate uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageFileEntry {
    /// Logical plugin name. Must match
    /// [`crate::plugin_spec::PLUGIN_NAME_RE`] — same regex used by
    /// bootstrap.yaml plugin entries because this name ends up in
    /// the same DB column.
    pub name: String,
    /// Port the in-container MCP server binds. Defaults to 8000.
    #[serde(default)]
    pub port: Option<u64>,
    /// HTTP path the MCP server mounts itself under. Defaults to
    /// `/mcp` (Streamable-HTTP convention) — note this differs from
    /// the bootstrap default of `/`, because mcp-package.yaml is the
    /// producer-side declaration for a *new* MCP-image deployment
    /// and `/mcp` is the post-RFE convention everything in
    /// `botworkz/mcp` uses.
    #[serde(default)]
    pub path: Option<String>,
    /// Auth posture for the upstream request. `none` (default) or
    /// `bearer/<service>` for the secrets-broker bearer-token mode.
    #[serde(default)]
    pub upstream_auth: Option<String>,
    /// Required: how session-broker reuses containers across agents.
    pub isolation: Isolation,
    /// Required: egress policy. Same wire shapes as bootstrap.yaml
    /// — string `all`/`none` or `{ allow: [{host, ports}] }`.
    pub egress: serde_yaml::Value,
    /// Container resource caps. Optional; same shape as bootstrap.
    #[serde(default)]
    pub resources: Option<serde_yaml::Value>,
    /// Static env entries baked into the plugin row. Same shape as
    /// bootstrap (`{KEY: value, ...}`); the validator emits the
    /// canonicalised `[{name, value}, ...]` form.
    #[serde(default)]
    pub env: Option<serde_yaml::Value>,
    /// Required: spill policy.
    pub spill: SpillEntry,
}

/// Validated `mcp-package.yaml` ready to feed the label composer.
///
/// Mirrors [`ValidatedPlugin`] field-for-field on the shared keys
/// and adds the package-only ones. The composer consumes this
/// directly — no second normalisation pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPackage {
    pub name: String,
    pub port: u16,
    pub path: String,
    pub upstream_auth: String,
    pub env: JsonValue,
    pub resources: Option<JsonValue>,
    pub egress: JsonValue,
    pub isolation: Isolation,
    pub spill: SpillEntry,
}

impl ValidatedPackage {
    /// Build a [`ValidatedPackage`] from a plugin-side
    /// [`ValidatedPlugin`] (the result of [`validate_one`]) plus the
    /// package-only fields. Internal helper for [`validate_package`].
    fn from_plugin_validation(
        plugin: ValidatedPlugin,
        isolation: Isolation,
        spill: SpillEntry,
    ) -> Self {
        Self {
            name: plugin.name,
            port: plugin.port,
            path: plugin.path,
            upstream_auth: plugin.upstream_auth,
            env: plugin.env,
            resources: plugin.resources,
            egress: plugin.egress,
            isolation,
            spill,
        }
    }
}

/// Sentinel image used when reusing [`validate_one`]. The image
/// field is required by the plugin validator but lives outside the
/// package file (it's the `--in` arg to the probe); we substitute a
/// stable placeholder so the shared validator can run unchanged.
///
/// The string is deliberately ugly so an accidental leak into a real
/// DB row is obviously wrong rather than silently shipped.
const PACKAGE_SENTINEL_IMAGE: &str = "package-file://no-image-required";

/// Default HTTP path mcp-package.yaml plugins mount under.
pub const DEFAULT_PACKAGE_PATH: &str = "/mcp";

/// Validate a parsed [`PackageFileEntry`] into a [`ValidatedPackage`].
///
/// Walks the same per-field rules [`validate_one`] enforces (name
/// regex, env caps, egress shape, …) by synthesising a
/// [`RawPluginEntry`] internally, then validates the package-only
/// fields ([`PackageFileEntry::spill`] cross-field rules; isolation
/// is a closed enum so serde already enforced its shape).
pub fn validate_package(raw: &PackageFileEntry) -> Result<ValidatedPackage, ValidationError> {
    // Default `path` to `/mcp` for package-side validation; the
    // bootstrap validator defaults to `/`, but the package file is
    // for new mcp-image deployments where the Streamable-HTTP
    // `/mcp` mount-point is the convention.
    let effective_path = raw
        .path
        .clone()
        .or_else(|| Some(DEFAULT_PACKAGE_PATH.to_string()));

    let synthetic = RawPluginEntry {
        name: raw.name.clone(),
        image: Some(PACKAGE_SENTINEL_IMAGE.to_string()),
        port: raw.port,
        path: effective_path,
        upstream_auth: raw.upstream_auth.clone(),
        env: raw.env.clone(),
        resources: raw.resources.clone(),
        egress: Some(raw.egress.clone()),
        network: None,
    };

    let plugin = validate_one(&synthetic)?;
    validate_spill(&plugin.name, &raw.spill)?;

    Ok(ValidatedPackage::from_plugin_validation(
        plugin,
        raw.isolation,
        raw.spill.clone(),
    ))
}

/// Spill-specific cross-field rules. The shape was validated by
/// serde (`deny_unknown_fields` on [`SpillEntry`]) but the
/// inter-field constraints — `threshold_bytes` only valid for
/// `mode = size`, empty allowlists rejected — live here.
fn validate_spill(plugin: &str, spill: &SpillEntry) -> Result<(), ValidationError> {
    match spill.mode {
        SpillMode::Size => {
            if spill.threshold_bytes.is_none() {
                return Err(package_err(
                    plugin,
                    "has invalid 'spill': mode=size requires 'threshold_bytes'",
                ));
            }
            if matches!(spill.threshold_bytes, Some(0)) {
                return Err(package_err(
                    plugin,
                    "has invalid 'spill.threshold_bytes': must be a positive integer",
                ));
            }
        }
        SpillMode::Never | SpillMode::Always => {
            if spill.threshold_bytes.is_some() {
                return Err(package_err(
                    plugin,
                    &format!(
                        "has invalid 'spill.threshold_bytes': only valid with mode=size (got mode={})",
                        spill.mode.as_wire_str()
                    ),
                ));
            }
        }
    }
    if let Some(methods) = spill.include_methods.as_ref() {
        if methods.is_empty() {
            return Err(package_err(
                plugin,
                "has invalid 'spill.include_methods': must not be empty (omit the key to apply to all methods)",
            ));
        }
        for m in methods {
            if m.trim().is_empty() {
                return Err(package_err(
                    plugin,
                    "has invalid 'spill.include_methods': entries must be non-empty strings",
                ));
            }
        }
    }
    if let Some(tools) = spill.include_tools.as_ref() {
        if tools.is_empty() {
            return Err(package_err(
                plugin,
                "has invalid 'spill.include_tools': must not be empty (omit the key to apply to all tools)",
            ));
        }
        for t in tools {
            if t.trim().is_empty() {
                return Err(package_err(
                    plugin,
                    "has invalid 'spill.include_tools': entries must be non-empty strings",
                ));
            }
        }
    }
    Ok(())
}

fn package_err(plugin: &str, suffix: &str) -> ValidationError {
    ValidationError::PackageInvalid {
        plugin: plugin.to_string(),
        detail: suffix.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_yaml(extra: &str) -> String {
        format!("name: echo\nisolation: shared\negress: none\nspill:\n  mode: never\n{extra}")
    }

    fn parse(yaml: &str) -> Result<PackageFileEntry, serde_yaml::Error> {
        serde_yaml::from_str(yaml)
    }

    #[test]
    fn minimal_well_formed_package_validates_with_defaults() {
        let p = parse(&minimal_yaml("")).expect("parse");
        let v = validate_package(&p).expect("validate");
        assert_eq!(v.name, "echo");
        assert_eq!(v.port, 8000);
        // Package default differs from bootstrap default — RFE-stated.
        assert_eq!(v.path, "/mcp");
        assert_eq!(v.upstream_auth, "none");
        assert_eq!(v.env, serde_json::json!([]));
        assert!(v.resources.is_none());
        assert_eq!(v.egress, serde_json::json!({"mode": "none"}));
        assert_eq!(v.isolation, Isolation::Shared);
        assert_eq!(v.spill.mode, SpillMode::Never);
    }

    #[test]
    fn deny_unknown_fields_rejects_typos() {
        let yaml = "name: echo\nisolation: shared\negress: none\nspill:\n  mode: never\nisolaton: shared\n";
        let err = parse(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("isolaton"), "{msg}");
    }

    #[test]
    fn package_validator_rejects_what_plugin_validator_rejects() {
        // Reuse a fact about validate_one: env names starting with
        // BOTWORK_SECRET_ are reserved. If validate_package routes
        // through validate_one, the package validator inherits that
        // rejection without re-implementing it.
        let yaml = minimal_yaml("env:\n  BOTWORK_SECRET_FOO: bar\n");
        let p = parse(&yaml).expect("parse");
        let err = validate_package(&p).unwrap_err();
        assert!(matches!(err, ValidationError::PluginInvalid { .. }));
    }

    #[test]
    fn name_regex_is_shared_with_plugin_validator() {
        let yaml = "name: NOT-VALID\nisolation: shared\negress: none\nspill:\n  mode: never\n";
        let p = parse(yaml).expect("parse");
        let err = validate_package(&p).unwrap_err();
        // Same PluginInvalid variant the plugin validator emits —
        // proves we're going through the shared rule, not a
        // duplicate.
        assert!(matches!(err, ValidationError::PluginInvalid { .. }));
    }

    #[test]
    fn isolation_round_trips_all_three_modes() {
        for mode in ["shared", "per_agent_session", "per_request"] {
            let yaml =
                format!("name: echo\nisolation: {mode}\negress: none\nspill:\n  mode: never\n");
            let p = parse(&yaml).expect("parse");
            let v = validate_package(&p).expect("validate");
            assert_eq!(v.isolation.as_wire_str(), mode);
        }
    }

    #[test]
    fn unknown_isolation_value_rejected_by_serde() {
        let yaml = "name: echo\nisolation: god_mode\negress: none\nspill:\n  mode: never\n";
        let err = parse(yaml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.to_lowercase().contains("god_mode") || msg.to_lowercase().contains("variant"));
    }

    #[test]
    fn spill_size_requires_threshold_bytes() {
        let yaml = minimal_yaml("");
        let mut p = parse(&yaml).expect("parse");
        p.spill = SpillEntry {
            mode: SpillMode::Size,
            threshold_bytes: None,
            include_methods: None,
            include_tools: None,
        };
        let err = validate_package(&p).unwrap_err();
        assert!(matches!(err, ValidationError::PackageInvalid { .. }));
    }

    #[test]
    fn spill_size_accepts_threshold_bytes() {
        let yaml = minimal_yaml("");
        let mut p = parse(&yaml).expect("parse");
        p.spill = SpillEntry {
            mode: SpillMode::Size,
            threshold_bytes: Some(65_536),
            include_methods: None,
            include_tools: None,
        };
        let v = validate_package(&p).expect("validate");
        assert_eq!(v.spill.threshold_bytes, Some(65_536));
    }

    #[test]
    fn spill_threshold_with_never_or_always_is_rejected() {
        for mode in [SpillMode::Never, SpillMode::Always] {
            let yaml = minimal_yaml("");
            let mut p = parse(&yaml).expect("parse");
            p.spill = SpillEntry {
                mode,
                threshold_bytes: Some(1024),
                include_methods: None,
                include_tools: None,
            };
            let err = validate_package(&p).unwrap_err();
            assert!(matches!(err, ValidationError::PackageInvalid { .. }));
        }
    }

    #[test]
    fn spill_size_with_zero_threshold_rejected() {
        let yaml = minimal_yaml("");
        let mut p = parse(&yaml).expect("parse");
        p.spill = SpillEntry {
            mode: SpillMode::Size,
            threshold_bytes: Some(0),
            include_methods: None,
            include_tools: None,
        };
        let err = validate_package(&p).unwrap_err();
        assert!(matches!(err, ValidationError::PackageInvalid { .. }));
    }

    #[test]
    fn spill_empty_include_lists_rejected() {
        for include_methods_empty in [true, false] {
            let yaml = minimal_yaml("");
            let mut p = parse(&yaml).expect("parse");
            p.spill = SpillEntry {
                mode: SpillMode::Always,
                threshold_bytes: None,
                include_methods: include_methods_empty.then(Vec::new),
                include_tools: (!include_methods_empty).then(Vec::new),
            };
            let err = validate_package(&p).unwrap_err();
            assert!(matches!(err, ValidationError::PackageInvalid { .. }));
        }
    }

    #[test]
    fn package_default_path_is_slash_mcp_not_slash() {
        // RFE-stated divergence from bootstrap.yaml plugin defaults
        // (which use `/`). Pinning the default here prevents a
        // future re-alignment from silently changing labels on
        // every probe-emitted image.
        let yaml = minimal_yaml("");
        let p = parse(&yaml).expect("parse");
        let v = validate_package(&p).expect("validate");
        assert_eq!(v.path, "/mcp");
    }

    #[test]
    fn explicit_path_overrides_default() {
        let yaml = minimal_yaml("path: /v1/mcp\n");
        let p = parse(&yaml).expect("parse");
        let v = validate_package(&p).expect("validate");
        assert_eq!(v.path, "/v1/mcp");
    }

    #[test]
    fn invalid_path_rejected_via_plugin_validator() {
        let yaml = minimal_yaml("path: \"no-leading-slash\"\n");
        let p = parse(&yaml).expect("parse");
        let err = validate_package(&p).unwrap_err();
        // Routed through validate_one's path checker → PluginInvalid.
        assert!(matches!(err, ValidationError::PluginInvalid { .. }));
    }

    #[test]
    fn egress_round_trips_normalised_string_form() {
        let yaml = minimal_yaml("");
        let p = parse(&yaml).expect("parse");
        let v = validate_package(&p).expect("validate");
        // String 'none' normalises to {mode: none}, same as bootstrap.
        assert_eq!(v.egress, serde_json::json!({"mode": "none"}));
    }

    #[test]
    fn egress_allow_form_round_trips_verbatim() {
        let yaml = "name: echo\nisolation: shared\negress:\n  allow:\n  - host: example.com\n    ports: [443]\nspill:\n  mode: never\n";
        let p = parse(yaml).expect("parse");
        let v = validate_package(&p).expect("validate");
        assert_eq!(v.egress["allow"][0]["host"], "example.com");
    }
}
