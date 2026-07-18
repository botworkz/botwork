//! Catalog + package → `org.botwork.mcp.*` label set.
//!
//! ## The label namespace (v1)
//!
//! The label keys are flat strings shaped like
//! `org.botwork.mcp.<field-path>`. The composer renders a fixed,
//! closed schema for the package-side fields and a variable-arity
//! schema for the captured catalogs:
//!
//! ```text
//! org.botwork.mcp.schema-version          = "1"
//! org.botwork.mcp.name                    = <package.name>
//! org.botwork.mcp.port                    = <package.port>
//! org.botwork.mcp.path                    = <package.path>
//! org.botwork.mcp.upstream-auth           = <package.upstream_auth>
//! org.botwork.mcp.isolation               = <package.isolation>
//! org.botwork.mcp.egress                  = <JSON-encoded normalised egress>
//! org.botwork.mcp.resources               = <JSON-encoded resources>            (only if present)
//! org.botwork.mcp.env                     = <JSON-encoded {name, value} array>  (always present; may be [])
//! org.botwork.mcp.spill.mode              = <package.spill.mode>
//! org.botwork.mcp.spill.threshold-bytes   = <integer>                           (only when mode=size)
//! org.botwork.mcp.spill.include-methods   = <comma list>                        (only when present)
//! org.botwork.mcp.spill.include-tools     = <comma list>                        (only when present)
//!
//! org.botwork.mcp.server-info.name        = <serverInfo.name>
//! org.botwork.mcp.server-info.version     = <serverInfo.version>                (only if present)
//! org.botwork.mcp.protocol-version        = <negotiated protocolVersion>
//! org.botwork.mcp.capabilities            = <JSON-encoded capabilities object>
//!
//! org.botwork.mcp.tools.count             = <integer>
//! org.botwork.mcp.tools.<n>.name          = <tool[n].name>                      (n = 0..count, BTreeMap-sorted)
//! org.botwork.mcp.tools.<n>.description   = <tool[n].description>               (only if present)
//! org.botwork.mcp.tools.<n>.input-schema  = <JSON-encoded inputSchema>          (only if present)
//!
//! org.botwork.mcp.resources.count         = <integer>                           (0 if server didn't advertise)
//! org.botwork.mcp.resources.<n>.uri       = <resource[n].uri>                   (n = 0..count)
//! org.botwork.mcp.resources.<n>.name      = <resource[n].name>                  (only if present)
//!
//! org.botwork.mcp.prompts.count           = <integer>                           (0 if server didn't advertise)
//! org.botwork.mcp.prompts.<n>.name        = <prompt[n].name>
//! org.botwork.mcp.prompts.<n>.description = <prompt[n].description>             (only if present)
//! ```
//!
//! Determinism: the output is a [`BTreeMap`], iteration order is
//! sorted by key, list indices are assigned in BTreeMap iteration
//! order of the tool/resource/prompt name (not the order the server
//! returned them) so reruns produce byte-identical output.

use std::collections::BTreeMap;

use botwork_api_core::package::ValidatedPackage;
use serde_json::Value as JsonValue;
use thiserror::Error;

use crate::mcp_probe::probe::{ordered_catalog, ProbeError, ProbeResult};

/// Schema version baked into every label set. Bumping it is a
/// deliberate cross-cutting change (consumers key on it; the
/// catalog upserter side will fail closed on an unknown version).
pub const LABEL_SCHEMA_VERSION: &str = "1";

/// Common label namespace prefix. Shared with the verify-mode
/// comparator so an accidental rename here ripples there.
pub const LABEL_NAMESPACE: &str = "org.botwork.mcp.";

/// Compose the full `org.botwork.mcp.*` label set from the
/// validated package + the captured probe result.
pub fn compose(
    package: &ValidatedPackage,
    probe: &ProbeResult,
) -> Result<BTreeMap<String, String>, ComposeError> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();

    out.insert(label("schema-version"), LABEL_SCHEMA_VERSION.to_string());

    // -- package-side fields ---------------------------------------------
    out.insert(label("name"), package.name.clone());
    out.insert(label("port"), package.port.to_string());
    out.insert(label("path"), package.path.clone());
    out.insert(label("upstream-auth"), package.upstream_auth.clone());
    out.insert(
        label("isolation"),
        package.isolation.as_wire_str().to_string(),
    );
    out.insert(label("egress"), json_compact(&package.egress));
    if let Some(resources) = package.resources.as_ref() {
        out.insert(label("resources"), json_compact(resources));
    }
    out.insert(label("env"), json_compact(&package.env));
    out.insert(
        label("spill.mode"),
        package.spill.mode.as_wire_str().to_string(),
    );
    if let Some(threshold) = package.spill.threshold_bytes {
        out.insert(label("spill.threshold-bytes"), threshold.to_string());
    }
    if let Some(methods) = package.spill.include_methods.as_ref() {
        out.insert(label("spill.include-methods"), methods.join(","));
    }
    if let Some(tools) = package.spill.include_tools.as_ref() {
        out.insert(label("spill.include-tools"), tools.join(","));
    }

    // -- probe-side fields -----------------------------------------------
    out.insert(label("server-info.name"), probe.server_info.name.clone());
    if let Some(ver) = probe.server_info.version.as_ref() {
        out.insert(label("server-info.version"), ver.clone());
    }
    out.insert(label("protocol-version"), probe.protocol_version.clone());
    out.insert(label("capabilities"), json_compact(&probe.capabilities));

    // -- captured catalog families ---------------------------------------
    add_tools(&mut out, &probe.tools)?;
    add_resources(&mut out, &probe.resources)?;
    add_prompts(&mut out, &probe.prompts)?;

    Ok(out)
}

fn add_tools(out: &mut BTreeMap<String, String>, tools: &[JsonValue]) -> Result<(), ComposeError> {
    let catalog = ordered_catalog(tools)?;
    out.insert(label("tools.count"), catalog.len().to_string());
    for (i, (_name, tool)) in catalog.iter().enumerate() {
        let prefix = format!("tools.{i}.");
        out.insert(
            label(&format!("{prefix}name")),
            tool.get("name")
                .and_then(JsonValue::as_str)
                .expect("name validated by ordered_catalog")
                .to_string(),
        );
        if let Some(desc) = tool.get("description").and_then(JsonValue::as_str) {
            out.insert(label(&format!("{prefix}description")), desc.to_string());
        }
        if let Some(schema) = tool.get("inputSchema") {
            // Reject schemas that don't parse as a JSON object — the
            // RFE calls out "validate input schemas parse as JSON
            // Schema" and the minimal version of that check is
            // "the field is at least a JSON object, not a string".
            if !schema.is_object() {
                return Err(ComposeError::InvalidInputSchema {
                    tool: tool
                        .get("name")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("?")
                        .to_string(),
                });
            }
            out.insert(
                label(&format!("{prefix}input-schema")),
                json_compact(schema),
            );
        }
    }
    Ok(())
}

fn add_resources(
    out: &mut BTreeMap<String, String>,
    resources: &[JsonValue],
) -> Result<(), ComposeError> {
    // Resources are keyed on `uri`; the validator rule lives here
    // (not in probe.rs) because the rule is consumed by the
    // compose step's canonical-ordering pass.
    let mut catalog: BTreeMap<String, &JsonValue> = BTreeMap::new();
    for entry in resources {
        let uri = entry
            .get("uri")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                ComposeError::CatalogShape("resources/list entry missing 'uri'".to_string())
            })?;
        catalog.insert(uri.to_string(), entry);
    }
    out.insert(label("resources.count"), catalog.len().to_string());
    for (i, (_uri, entry)) in catalog.iter().enumerate() {
        let prefix = format!("resources.{i}.");
        out.insert(
            label(&format!("{prefix}uri")),
            entry
                .get("uri")
                .and_then(JsonValue::as_str)
                .expect("uri validated above")
                .to_string(),
        );
        if let Some(n) = entry.get("name").and_then(JsonValue::as_str) {
            out.insert(label(&format!("{prefix}name")), n.to_string());
        }
    }
    Ok(())
}

fn add_prompts(
    out: &mut BTreeMap<String, String>,
    prompts: &[JsonValue],
) -> Result<(), ComposeError> {
    let catalog = ordered_catalog(prompts)?;
    out.insert(label("prompts.count"), catalog.len().to_string());
    for (i, (_name, prompt)) in catalog.iter().enumerate() {
        let prefix = format!("prompts.{i}.");
        out.insert(
            label(&format!("{prefix}name")),
            prompt
                .get("name")
                .and_then(JsonValue::as_str)
                .expect("name validated by ordered_catalog")
                .to_string(),
        );
        if let Some(desc) = prompt.get("description").and_then(JsonValue::as_str) {
            out.insert(label(&format!("{prefix}description")), desc.to_string());
        }
    }
    Ok(())
}

fn label(suffix: &str) -> String {
    format!("{LABEL_NAMESPACE}{suffix}")
}

fn json_compact(v: &JsonValue) -> String {
    // Always use compact form so reruns produce byte-identical
    // strings (serde_json's pretty printer is not deterministic
    // across versions w.r.t. trailing whitespace).
    serde_json::to_string(v).expect("serializable JSON")
}

#[derive(Debug, Error)]
pub enum ComposeError {
    #[error("captured catalog: {0}")]
    CatalogShape(String),

    #[error("tool '{tool}' has a non-object 'inputSchema'")]
    InvalidInputSchema { tool: String },

    #[error(transparent)]
    Probe(#[from] ProbeError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use botwork_api_core::package::{Isolation, SpillEntry, SpillMode};
    use serde_json::json;

    fn minimal_package() -> ValidatedPackage {
        ValidatedPackage {
            name: "echo".to_string(),
            port: 8000,
            path: "/mcp".to_string(),
            upstream_auth: "none".to_string(),
            env: json!([]),
            resources: None,
            egress: json!({"mode": "none"}),
            isolation: Isolation::Shared,
            spill: SpillEntry {
                mode: SpillMode::Never,
                threshold_bytes: None,
                include_methods: None,
                include_tools: None,
            },
        }
    }

    fn minimal_probe() -> ProbeResult {
        ProbeResult {
            server_info: crate::mcp_probe::probe::ServerInfo {
                name: "mcp-echo".to_string(),
                version: Some("0.1.0".to_string()),
            },
            protocol_version: "2025-06-18".to_string(),
            capabilities: json!({"tools": {}}),
            tools: vec![json!({"name": "echo", "description": "echo back the input"})],
            resources: vec![],
            prompts: vec![],
        }
    }

    #[test]
    fn minimal_pair_renders_the_required_label_set() {
        let labels = compose(&minimal_package(), &minimal_probe()).expect("compose");
        // Schema version is the first stake we put in the ground.
        assert_eq!(
            labels.get("org.botwork.mcp.schema-version"),
            Some(&"1".to_string())
        );
        // Package fields.
        assert_eq!(
            labels.get("org.botwork.mcp.name"),
            Some(&"echo".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.port"),
            Some(&"8000".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.path"),
            Some(&"/mcp".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.upstream-auth"),
            Some(&"none".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.isolation"),
            Some(&"shared".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.egress"),
            Some(&"{\"mode\":\"none\"}".to_string())
        );
        assert!(!labels.contains_key("org.botwork.mcp.resources"));
        assert_eq!(labels.get("org.botwork.mcp.env"), Some(&"[]".to_string()));
        // Spill.
        assert_eq!(
            labels.get("org.botwork.mcp.spill.mode"),
            Some(&"never".to_string())
        );
        assert!(!labels.contains_key("org.botwork.mcp.spill.threshold-bytes"));
        // Probe.
        assert_eq!(
            labels.get("org.botwork.mcp.server-info.name"),
            Some(&"mcp-echo".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.server-info.version"),
            Some(&"0.1.0".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.protocol-version"),
            Some(&"2025-06-18".to_string())
        );
        // Tools.
        assert_eq!(
            labels.get("org.botwork.mcp.tools.count"),
            Some(&"1".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.tools.0.name"),
            Some(&"echo".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.tools.0.description"),
            Some(&"echo back the input".to_string())
        );
        // Resources / prompts present at count=0 even when absent.
        assert_eq!(
            labels.get("org.botwork.mcp.resources.count"),
            Some(&"0".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.prompts.count"),
            Some(&"0".to_string())
        );
    }

    #[test]
    fn label_set_is_byte_identical_across_compose_runs() {
        // Determinism property: the canonical-ordering pass means
        // re-running on the same input must produce the same map.
        let pkg = minimal_package();
        let probe = minimal_probe();
        let a = compose(&pkg, &probe).expect("compose a");
        let b = compose(&pkg, &probe).expect("compose b");
        assert_eq!(a, b);
        // Iteration order is the property the labels rely on for
        // byte-stable describe-mode output; assert the sorted-keys
        // shape rather than the equality (which would also pass if
        // BTreeMap silently switched to a HashMap underneath).
        let keys_a: Vec<&String> = a.keys().collect();
        let mut sorted = keys_a.clone();
        sorted.sort();
        assert_eq!(keys_a, sorted);
    }

    #[test]
    fn input_schema_is_jsoned_in_compact_form() {
        let pkg = minimal_package();
        let mut probe = minimal_probe();
        probe.tools = vec![json!({
            "name": "fetch",
            "inputSchema": {
                "type": "object",
                "properties": {"url": {"type": "string"}}
            }
        })];
        let labels = compose(&pkg, &probe).expect("compose");
        let schema = labels.get("org.botwork.mcp.tools.0.input-schema").unwrap();
        // Compact form: no spaces around `:`, no trailing whitespace.
        assert!(!schema.contains(": "));
    }

    #[test]
    fn non_object_input_schema_rejected() {
        let pkg = minimal_package();
        let mut probe = minimal_probe();
        probe.tools = vec![json!({"name": "fetch", "inputSchema": "not an object"})];
        let err = compose(&pkg, &probe).unwrap_err();
        assert!(matches!(err, ComposeError::InvalidInputSchema { .. }));
    }

    #[test]
    fn tools_are_ordered_by_name_not_input_order() {
        let pkg = minimal_package();
        let mut probe = minimal_probe();
        probe.tools = vec![
            json!({"name": "zoo"}),
            json!({"name": "alpha"}),
            json!({"name": "mid"}),
        ];
        let labels = compose(&pkg, &probe).expect("compose");
        // alpha < mid < zoo (lexicographic) so the indices follow
        // alphabetical, not insertion, order.
        assert_eq!(
            labels.get("org.botwork.mcp.tools.0.name"),
            Some(&"alpha".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.tools.1.name"),
            Some(&"mid".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.tools.2.name"),
            Some(&"zoo".to_string())
        );
    }

    #[test]
    fn resources_count_emitted_even_when_server_omits_capability() {
        let pkg = minimal_package();
        let probe = minimal_probe();
        let labels = compose(&pkg, &probe).expect("compose");
        assert_eq!(
            labels.get("org.botwork.mcp.resources.count"),
            Some(&"0".to_string())
        );
    }

    #[test]
    fn resources_render_uri_and_optional_name() {
        let pkg = minimal_package();
        let mut probe = minimal_probe();
        probe.resources = vec![
            json!({"uri": "file:///b.txt", "name": "b"}),
            json!({"uri": "file:///a.txt"}),
        ];
        let labels = compose(&pkg, &probe).expect("compose");
        // Sort key is uri → a then b.
        assert_eq!(
            labels.get("org.botwork.mcp.resources.0.uri"),
            Some(&"file:///a.txt".to_string())
        );
        assert!(!labels.contains_key("org.botwork.mcp.resources.0.name"));
        assert_eq!(
            labels.get("org.botwork.mcp.resources.1.uri"),
            Some(&"file:///b.txt".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.resources.1.name"),
            Some(&"b".to_string())
        );
    }

    #[test]
    fn spill_size_emits_threshold_label() {
        let mut pkg = minimal_package();
        pkg.spill = SpillEntry {
            mode: SpillMode::Size,
            threshold_bytes: Some(65_536),
            include_methods: Some(vec!["tools/call".to_string()]),
            include_tools: None,
        };
        let probe = minimal_probe();
        let labels = compose(&pkg, &probe).expect("compose");
        assert_eq!(
            labels.get("org.botwork.mcp.spill.threshold-bytes"),
            Some(&"65536".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.spill.include-methods"),
            Some(&"tools/call".to_string())
        );
        assert!(!labels.contains_key("org.botwork.mcp.spill.include-tools"));
    }

    #[test]
    fn resources_entry_missing_uri_surfaces_catalog_shape_error() {
        let pkg = minimal_package();
        let mut probe = minimal_probe();
        probe.resources = vec![json!({"name": "no-uri"})];
        let err = compose(&pkg, &probe).unwrap_err();
        assert!(matches!(err, ComposeError::CatalogShape(_)));
    }

    #[test]
    fn compose_omits_optional_server_version_and_orders_prompts() {
        let pkg = minimal_package();
        let mut probe = minimal_probe();
        probe.server_info.version = None;
        probe.prompts = vec![
            json!({"name": "zeta", "description": "last"}),
            json!({"name": "alpha"}),
        ];
        let labels = compose(&pkg, &probe).expect("compose");
        assert!(!labels.contains_key("org.botwork.mcp.server-info.version"));
        assert_eq!(
            labels.get("org.botwork.mcp.prompts.count"),
            Some(&"2".to_string())
        );
        assert_eq!(
            labels.get("org.botwork.mcp.prompts.0.name"),
            Some(&"alpha".to_string())
        );
        assert!(!labels.contains_key("org.botwork.mcp.prompts.0.description"));
        assert_eq!(
            labels.get("org.botwork.mcp.prompts.1.description"),
            Some(&"last".to_string())
        );
    }
}
