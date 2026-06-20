//! Compile `SessionStore` snapshots into envoy xDS resources.
//!
//! This is the bridge between the schema config-broker hands us
//! (`egress: all | none | { allow: [...] }` — verbatim JSON) and the
//! protobuf shapes envoy expects on its ADS stream. Pure functions
//! only: no IO, no `Arc`, no clone-ing the store. The xDS server in
//! `xds.rs` calls these against a snapshot it already pulled.
//!
//! ## Allowlist semantics (RBAC action: ALLOW)
//!
//! envoy's RBAC filter has two action modes. We use **ALLOW**:
//!
//! * ALLOW + at least one policy matches → request permitted.
//! * ALLOW + no policy matches           → request denied.
//!
//! This maps the three egress modes onto envoy as follows:
//!
//! | mode                   | emitted policy                                                         |
//! |------------------------|------------------------------------------------------------------------|
//! | `egress: all`          | one policy, principal=src_ip, permission=`any: true`                    |
//! | `egress: none`         | **no policy emitted**; default-no-match → denied                       |
//! | `egress: {allow: [...]}` | one policy, principal=src_ip, permissions=one `:authority` exact-match per allow entry |
//!
//! An unknown source IP (the cold-start window between docker IP
//! allocation and `POST /sessions` landing) also produces no match
//! and is denied, which matches the security posture session-broker's
//! hard gate provides on the spawn path.
//!
//! ## Why `direct_remote_ip` not `remote_ip` / `source_ip`
//!
//! In our topology, the plugin container speaks straight to
//! egress-envoy with no intermediate XFF / PROXY-protocol hop, so all
//! three IP principals collapse to "the docker bridge peer."
//! `direct_remote_ip` is the one that *always* means "the physical
//! peer of this TCP connection, ignoring header games" — pinning it
//! means a future change that introduces a real proxy in front of
//! egress-envoy can't accidentally widen the principal set.
//!
//! ## Why `:authority` exact-match for the allow-list
//!
//! HTTP/1.1 CONNECT requests carry the upstream as
//! `CONNECT api.github.com:443 HTTP/1.1`. envoy normalises that into
//! the `:authority` pseudo-header. Exact-matching it (rather than
//! splitting host/port and matching separately) keeps the policy
//! compilation totally string-driven and avoids the trap where
//! `host: github.com, ports: [443]` and `host: github.com:443` would
//! otherwise be two different things to support. Today the allow
//! schema is the array-of-`{host, ports}` form, so we materialise one
//! `host:port` permission per port per host.
//!
//! ## Single DFP cluster on the cluster type
//!
//! All allowed traffic exits through one dynamic_forward_proxy
//! cluster keyed off the request `:authority`. The cluster's DNS
//! cache config must match the DFP HTTP filter's — envoy fails its
//! config load with `dns_cache_config must match …` otherwise — so
//! `DNS_CACHE_NAME` and `dns_lookup_family` are the single source of
//! truth here.
//!
//! ## Listener / cluster / RouteConfig naming
//!
//! These names are part of the wire contract with the egress envoy
//! bootstrap config (see vm). They MUST NOT change without a
//! coordinated bootstrap update. Stable names:
//!
//! * Listener:                `egress_listener`
//! * Cluster:                 `dynamic_forward_proxy_cluster`
//! * DFP DNS cache:           `dynamic_forward_proxy_cache_config`
//! * Inline RouteConfig name: `egress_routes`

use std::net::Ipv4Addr;

use envoy_proto::envoy::config::cluster::v3::cluster;
use envoy_proto::envoy::config::cluster::v3::Cluster;
use envoy_proto::envoy::config::core::v3::address::Address as AddressKind;
use envoy_proto::envoy::config::core::v3::socket_address::{PortSpecifier, Protocol};
use envoy_proto::envoy::config::core::v3::{Address, CidrRange, SocketAddress};
use envoy_proto::envoy::config::listener::v3::filter::ConfigType as FilterConfigType;
use envoy_proto::envoy::config::listener::v3::{Filter, FilterChain, Listener};
use envoy_proto::envoy::config::rbac::v3::{
    permission, principal, rbac, Permission, Policy, Principal, Rbac,
};
use envoy_proto::envoy::config::route::v3::header_matcher::HeaderMatchSpecifier;
use envoy_proto::envoy::config::route::v3::route::Action as RouteActionKind;
use envoy_proto::envoy::config::route::v3::route_match::{ConnectMatcher, PathSpecifier};
use envoy_proto::envoy::config::route::v3::{
    HeaderMatcher, Route, RouteAction, RouteConfiguration, RouteMatch, VirtualHost,
};
use envoy_proto::envoy::extensions::clusters::dynamic_forward_proxy::v3::cluster_config::ClusterImplementationSpecifier;
use envoy_proto::envoy::extensions::clusters::dynamic_forward_proxy::v3::ClusterConfig as DfpClusterConfig;
use envoy_proto::envoy::extensions::common::dynamic_forward_proxy::v3::DnsCacheConfig;
use envoy_proto::envoy::extensions::filters::http::dynamic_forward_proxy::v3::filter_config::ImplementationSpecifier as DfpFilterImplementationSpecifier;
use envoy_proto::envoy::extensions::filters::http::dynamic_forward_proxy::v3::FilterConfig as DfpFilterConfig;
use envoy_proto::envoy::extensions::filters::http::rbac::v3::Rbac as RbacFilter;
use envoy_proto::envoy::extensions::filters::http::router::v3::Router;
use envoy_proto::envoy::extensions::filters::network::http_connection_manager::v3::http_connection_manager::{
    CodecType, RouteSpecifier, UpgradeConfig,
};
use envoy_proto::envoy::extensions::filters::network::http_connection_manager::v3::http_filter::ConfigType as HttpFilterConfigType;
use envoy_proto::envoy::extensions::filters::network::http_connection_manager::v3::{
    HttpConnectionManager, HttpFilter,
};
use prost::Message;
use prost_types::Any;

use crate::sessions::SessionRecord;

/// xDS resource type URL strings. These are the type URLs envoy wraps
/// resources in inside an ADS `DiscoveryResponse.resources` field and
/// the ones it subscribes to via `DiscoveryRequest.type_url`.
pub const LISTENER_TYPE_URL: &str = "type.googleapis.com/envoy.config.listener.v3.Listener";
pub const CLUSTER_TYPE_URL: &str = "type.googleapis.com/envoy.config.cluster.v3.Cluster";

/// xDS resource names — part of the egress-envoy bootstrap contract,
/// see module docs.
pub const LISTENER_NAME: &str = "egress_listener";
pub const CLUSTER_NAME: &str = "dynamic_forward_proxy_cluster";
pub const DNS_CACHE_NAME: &str = "dynamic_forward_proxy_cache_config";
pub const ROUTE_CONFIG_NAME: &str = "egress_routes";

/// Default proxy port on egress-envoy. Plugin containers reach it via
/// the `egress_envoy:3128` alias on `botwork-plugin`. Surfaced as a
/// constant rather than a build-time arg because changing it requires
/// a coordinated update with launcher's `HTTPS_PROXY=…` env injection.
pub const EGRESS_LISTENER_PORT: u32 = 3128;

/// `Cluster::DnsLookupFamily::V4Only` underlying value. Pinned to
/// IPv4 to match the rest of the broker stack's assumptions; if
/// dual-stack ever lands this becomes a deployment-time flag.
const DNS_LOOKUP_FAMILY_V4_ONLY: i32 = 1;

/// Build the LDS resource for the egress listener.
///
/// Single filter chain with HCM → RBAC → DFP → router. RBAC is
/// compiled fresh on every call from the given session snapshot, so
/// the xDS server can call `build_listener(&store.list().await)` once
/// per push and ship the resulting protobuf verbatim.
pub fn build_listener(sessions: &[SessionRecord]) -> Listener {
    let hcm = build_http_connection_manager(sessions);
    let hcm_any = Any {
        type_url: "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager"
            .to_string(),
        value: hcm.encode_to_vec(),
    };

    Listener {
        name: LISTENER_NAME.to_string(),
        address: Some(Address {
            address: Some(AddressKind::SocketAddress(SocketAddress {
                protocol: Protocol::Tcp as i32,
                address: "0.0.0.0".to_string(),
                port_specifier: Some(PortSpecifier::PortValue(EGRESS_LISTENER_PORT)),
                ..Default::default()
            })),
        }),
        filter_chains: vec![FilterChain {
            filters: vec![Filter {
                name: "envoy.filters.network.http_connection_manager".to_string(),
                config_type: Some(FilterConfigType::TypedConfig(hcm_any)),
            }],
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Build the CDS resource for the dynamic_forward_proxy cluster.
///
/// Static — does not depend on the session snapshot — but lives in
/// the same module as `build_listener` so the wire-shape choices
/// (cluster name, DNS cache name, IPv4-only) are colocated.
pub fn build_cluster() -> Cluster {
    let dfp_config = DfpClusterConfig {
        cluster_implementation_specifier: Some(ClusterImplementationSpecifier::DnsCacheConfig(
            dns_cache_config(),
        )),
        ..Default::default()
    };
    let typed_config = Any {
        type_url:
            "type.googleapis.com/envoy.extensions.clusters.dynamic_forward_proxy.v3.ClusterConfig"
                .to_string(),
        value: dfp_config.encode_to_vec(),
    };

    Cluster {
        name: CLUSTER_NAME.to_string(),
        lb_policy: cluster::LbPolicy::ClusterProvided as i32,
        cluster_discovery_type: Some(cluster::ClusterDiscoveryType::ClusterType(
            cluster::CustomClusterType {
                name: "envoy.clusters.dynamic_forward_proxy".to_string(),
                typed_config: Some(typed_config),
            },
        )),
        ..Default::default()
    }
}

fn build_http_connection_manager(sessions: &[SessionRecord]) -> HttpConnectionManager {
    HttpConnectionManager {
        codec_type: CodecType::Http1 as i32,
        stat_prefix: "egress_http".to_string(),
        // CONNECT support is opt-in per envoy; without this, the
        // listener silently 400s every CONNECT before RBAC ever runs.
        upgrade_configs: vec![UpgradeConfig {
            upgrade_type: "CONNECT".to_string(),
            ..Default::default()
        }],
        route_specifier: Some(RouteSpecifier::RouteConfig(build_route_config())),
        http_filters: vec![
            build_rbac_filter(sessions),
            build_dfp_filter(),
            build_router_filter(),
        ],
        ..Default::default()
    }
}

fn build_route_config() -> RouteConfiguration {
    // One vhost matching `*` with one CONNECT-matching route.
    // The RBAC filter runs *before* this route resolves, so the route
    // table's job is just "send permitted CONNECTs to the DFP cluster."
    RouteConfiguration {
        name: ROUTE_CONFIG_NAME.to_string(),
        virtual_hosts: vec![VirtualHost {
            name: "egress_vhost".to_string(),
            domains: vec!["*".to_string()],
            routes: vec![Route {
                r#match: Some(RouteMatch {
                    path_specifier: Some(PathSpecifier::ConnectMatcher(ConnectMatcher::default())),
                    ..Default::default()
                }),
                action: Some(RouteActionKind::Route(RouteAction {
                    cluster_specifier: Some(
                        envoy_proto::envoy::config::route::v3::route_action::ClusterSpecifier::Cluster(
                            CLUSTER_NAME.to_string(),
                        ),
                    ),
                    upgrade_configs: vec![
                        envoy_proto::envoy::config::route::v3::route_action::UpgradeConfig {
                            upgrade_type: "CONNECT".to_string(),
                            connect_config: Some(
                                envoy_proto::envoy::config::route::v3::route_action::upgrade_config::ConnectConfig::default(),
                            ),
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                })),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn build_rbac_filter(sessions: &[SessionRecord]) -> HttpFilter {
    let rbac_inner = Rbac {
        action: rbac::Action::Allow as i32,
        policies: build_rbac_policies(sessions),
        ..Default::default()
    };
    let rbac_outer = RbacFilter {
        rules: Some(rbac_inner),
        rules_stat_prefix: "egress".to_string(),
        ..Default::default()
    };
    HttpFilter {
        name: "envoy.filters.http.rbac".to_string(),
        config_type: Some(HttpFilterConfigType::TypedConfig(Any {
            type_url: "type.googleapis.com/envoy.extensions.filters.http.rbac.v3.RBAC".to_string(),
            value: rbac_outer.encode_to_vec(),
        })),
        ..Default::default()
    }
}

fn build_dfp_filter() -> HttpFilter {
    let dfp = DfpFilterConfig {
        implementation_specifier: Some(DfpFilterImplementationSpecifier::DnsCacheConfig(
            dns_cache_config(),
        )),
        ..Default::default()
    };
    HttpFilter {
        name: "envoy.filters.http.dynamic_forward_proxy".to_string(),
        config_type: Some(HttpFilterConfigType::TypedConfig(Any {
            type_url:
                "type.googleapis.com/envoy.extensions.filters.http.dynamic_forward_proxy.v3.FilterConfig"
                    .to_string(),
            value: dfp.encode_to_vec(),
        })),
        ..Default::default()
    }
}

fn build_router_filter() -> HttpFilter {
    let router = Router::default();
    HttpFilter {
        name: "envoy.filters.http.router".to_string(),
        config_type: Some(HttpFilterConfigType::TypedConfig(Any {
            type_url: "type.googleapis.com/envoy.extensions.filters.http.router.v3.Router"
                .to_string(),
            value: router.encode_to_vec(),
        })),
        ..Default::default()
    }
}

/// One DNS-cache config, shared between the DFP cluster and DFP HTTP
/// filter. envoy fails config-load if these differ.
fn dns_cache_config() -> DnsCacheConfig {
    DnsCacheConfig {
        name: DNS_CACHE_NAME.to_string(),
        dns_lookup_family: DNS_LOOKUP_FAMILY_V4_ONLY,
        ..Default::default()
    }
}

/// Compile all sessions into RBAC policies. Returns the
/// `(policy_name, Policy)` map suitable for `Rbac::policies`.
///
/// Lexicographic policy name ordering matters for envoy's evaluation
/// determinism; the policy names embed `session_id` which is already
/// `mcp_session_<hex>`-shaped so the natural string sort is also a
/// stable insertion order.
fn build_rbac_policies(sessions: &[SessionRecord]) -> std::collections::HashMap<String, Policy> {
    sessions
        .iter()
        .filter_map(|record| {
            let permissions = permissions_for_egress(&record.egress_policy)?;
            let principals = vec![principal_for_ip(record.container_ip)];
            Some((
                format!("session_{}", record.session_id),
                Policy {
                    permissions,
                    principals,
                    ..Default::default()
                },
            ))
        })
        .collect()
}

/// Derive RBAC permissions from a verbatim egress JSON blob.
///
/// Returns `None` for `egress: none` (and for any unrecognised shape,
/// which compiles to "no policy" — denial — matching the
/// fail-closed posture). Returns `Some(vec![any: true])` for
/// `egress: all`. Returns one `:authority`-exact-match Permission per
/// `{host, ports[i]}` entry for the allowlist form.
///
/// ## Accepted wire shapes
///
/// config-broker 0.1.9+ normalises the `all` / `none` sugar from
/// `plugins.yaml` into a `{ "mode": "all" }` / `{ "mode": "none" }`
/// object on the wire (see `config-broker::registry::parse_egress`),
/// then session-broker forwards that verbatim into the
/// `egress_policy` field on each `POST /sessions` (botwork #81). The
/// pre-0.1.9 bare-string form is still accepted so older clients,
/// recovery-sync replay, and the existing test fixtures all keep
/// compiling to the same policy. The allowlist form passes through
/// verbatim from `plugins.yaml` without normalisation today.
///
/// Accepted:
///
///   * `"all"`                                — bare string
///   * `{ "mode": "all" }`                    — config-broker normalised
///   * `"none"`                               — bare string
///   * `{ "mode": "none" }`                   — config-broker normalised
///   * `{ "allow": [{host, ports}, ...] }`    — verbatim allowlist
///
/// **Unrecognised shapes fail closed.** Anything that doesn't match
/// one of the above produces no policy, which under our `ALLOW`
/// action means denial. config-broker is supposed to have rejected
/// anything malformed before it gets here (botwork #88) — this is
/// defence-in-depth.
fn permissions_for_egress(egress: &serde_json::Value) -> Option<Vec<Permission>> {
    let mode = egress_mode(egress);

    // `egress: "all"` / `{"mode":"all"}` → unrestricted for this session.
    if mode == Some("all") {
        return Some(vec![Permission {
            rule: Some(permission::Rule::Any(true)),
        }]);
    }
    // `egress: "none"` / `{"mode":"none"}` → no policy emitted,
    // default-no-match denies.
    if mode == Some("none") {
        return None;
    }
    // `egress: { allow: [{host, ports}, ...] }`.
    if let Some(allow) = egress.get("allow").and_then(|v| v.as_array()) {
        let mut perms = Vec::new();
        for entry in allow {
            let Some(host) = entry.get("host").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(ports) = entry.get("ports").and_then(|v| v.as_array()) else {
                continue;
            };
            for port in ports {
                let Some(port) = port.as_u64() else {
                    continue;
                };
                perms.push(authority_header_match(host, port));
            }
        }
        if perms.is_empty() {
            // An `allow` block with no usable entries is just `none`
            // — fail closed.
            return None;
        }
        return Some(perms);
    }
    // Anything else → fail closed.
    None
}

/// Extract the `all` / `none` mode keyword from either the bare-string
/// or the `{ "mode": "..." }` object encoding, or `None` for any other
/// shape.
///
/// Both encodings are in active use:
///
///   * **bare string** -- the original wire shape; still emitted by
///     pre-0.1.9 clients and used directly throughout this module's
///     unit tests.
///   * **`{ "mode": "<keyword>" }`** -- what config-broker 0.1.9+
///     normalises `egress: all` / `egress: none` in `plugins.yaml`
///     into before forwarding to control-plane via session-broker
///     (see `config-broker::registry::parse_egress`).
///
/// Returning `Option<&str>` (rather than e.g. a `Mode` enum) keeps the
/// caller a single equality check away from the existing match arms
/// in [`permissions_for_egress`] and avoids inventing a parallel
/// vocabulary for what is effectively a single string.
fn egress_mode(egress: &serde_json::Value) -> Option<&str> {
    if let Some(s) = egress.as_str() {
        return Some(s);
    }
    egress
        .as_object()
        .and_then(|obj| obj.get("mode"))
        .and_then(|v| v.as_str())
}

fn authority_header_match(host: &str, port: u64) -> Permission {
    Permission {
        rule: Some(permission::Rule::Header(HeaderMatcher {
            name: ":authority".to_string(),
            header_match_specifier: Some(HeaderMatchSpecifier::ExactMatch(format!(
                "{host}:{port}"
            ))),
            ..Default::default()
        })),
    }
}

fn principal_for_ip(ip: Ipv4Addr) -> Principal {
    Principal {
        identifier: Some(principal::Identifier::DirectRemoteIp(CidrRange {
            address_prefix: ip.to_string(),
            prefix_len: Some(32),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::net::Ipv4Addr;

    fn record(id: &str, ip: &str, plugin: &str, egress: serde_json::Value) -> SessionRecord {
        SessionRecord {
            session_id: id.to_string(),
            container_ip: ip.parse().expect("test ip"),
            tenant: "phlax".to_string(),
            namespace: "mcp".to_string(),
            plugin: plugin.to_string(),
            egress_policy: egress,
        }
    }

    fn listener_rbac_policies(listener: &Listener) -> std::collections::HashMap<String, Policy> {
        // Decode HCM → http_filters → first one (RBAC) → typed_config → Rbac → policies.
        let filter_chain = listener.filter_chains.first().expect("filter chain");
        let network_filter = filter_chain.filters.first().expect("network filter");
        let FilterConfigType::TypedConfig(any) =
            network_filter.config_type.clone().expect("config_type")
        else {
            panic!("expected TypedConfig");
        };
        let hcm = HttpConnectionManager::decode(any.value.as_slice()).expect("decode hcm");
        let rbac_filter = hcm
            .http_filters
            .iter()
            .find(|f| f.name == "envoy.filters.http.rbac")
            .expect("rbac filter present");
        let HttpFilterConfigType::TypedConfig(rbac_any) =
            rbac_filter.config_type.clone().expect("rbac config_type")
        else {
            panic!("expected TypedConfig on rbac");
        };
        let rbac_filter_inner =
            RbacFilter::decode(rbac_any.value.as_slice()).expect("decode rbac filter");
        rbac_filter_inner.rules.expect("rules present").policies
    }

    fn listener_rbac_action(listener: &Listener) -> i32 {
        let filter_chain = listener.filter_chains.first().expect("filter chain");
        let network_filter = filter_chain.filters.first().expect("network filter");
        let FilterConfigType::TypedConfig(any) =
            network_filter.config_type.clone().expect("config_type")
        else {
            panic!("expected TypedConfig");
        };
        let hcm = HttpConnectionManager::decode(any.value.as_slice()).expect("decode hcm");
        let rbac_filter = hcm
            .http_filters
            .iter()
            .find(|f| f.name == "envoy.filters.http.rbac")
            .expect("rbac filter present");
        let HttpFilterConfigType::TypedConfig(rbac_any) =
            rbac_filter.config_type.clone().expect("rbac config_type")
        else {
            panic!("expected TypedConfig on rbac");
        };
        RbacFilter::decode(rbac_any.value.as_slice())
            .expect("decode")
            .rules
            .expect("rules")
            .action
    }

    #[test]
    fn empty_snapshot_produces_listener_with_zero_policies() {
        let listener = build_listener(&[]);
        let policies = listener_rbac_policies(&listener);
        assert!(policies.is_empty(), "policies: {policies:?}");
        // ALLOW + no policies = deny everything. That's the
        // correct cold-start posture.
        assert_eq!(listener_rbac_action(&listener), rbac::Action::Allow as i32);
    }

    #[test]
    fn listener_address_is_3128_tcp() {
        let listener = build_listener(&[]);
        let socket = match listener
            .address
            .expect("address")
            .address
            .expect("address oneof")
        {
            AddressKind::SocketAddress(sa) => sa,
            _ => panic!("expected SocketAddress"),
        };
        assert_eq!(socket.address, "0.0.0.0");
        assert_eq!(socket.protocol, Protocol::Tcp as i32);
        match socket.port_specifier.expect("port") {
            PortSpecifier::PortValue(p) => assert_eq!(p, 3128),
            _ => panic!("expected PortValue"),
        }
    }

    #[test]
    fn listener_name_is_stable_wire_contract() {
        let listener = build_listener(&[]);
        assert_eq!(listener.name, "egress_listener");
    }

    #[test]
    fn cluster_name_and_dfp_shape_are_stable_wire_contract() {
        let cluster = build_cluster();
        assert_eq!(cluster.name, "dynamic_forward_proxy_cluster");
        assert_eq!(cluster.lb_policy, cluster::LbPolicy::ClusterProvided as i32);
        let cluster::ClusterDiscoveryType::ClusterType(custom) = cluster
            .cluster_discovery_type
            .expect("cluster_discovery_type")
        else {
            panic!("expected ClusterType variant");
        };
        assert_eq!(custom.name, "envoy.clusters.dynamic_forward_proxy");
        let typed = custom.typed_config.expect("typed_config");
        let dfp = DfpClusterConfig::decode(typed.value.as_slice()).expect("decode dfp");
        let ClusterImplementationSpecifier::DnsCacheConfig(cache) =
            dfp.cluster_implementation_specifier.expect("specifier")
        else {
            panic!("expected DnsCacheConfig variant");
        };
        assert_eq!(cache.name, "dynamic_forward_proxy_cache_config");
        assert_eq!(cache.dns_lookup_family, DNS_LOOKUP_FAMILY_V4_ONLY);
    }

    #[test]
    fn egress_all_produces_principal_with_any_permission() {
        let listener =
            build_listener(&[record("mcp_session_a", "172.20.0.5", "fetch", json!("all"))]);
        let policies = listener_rbac_policies(&listener);
        let policy = policies
            .get("session_mcp_session_a")
            .expect("policy for session_a");
        assert_eq!(policy.principals.len(), 1);
        let principal = &policy.principals[0];
        let principal::Identifier::DirectRemoteIp(cidr) =
            principal.identifier.clone().expect("identifier")
        else {
            panic!("expected DirectRemoteIp");
        };
        assert_eq!(cidr.address_prefix, "172.20.0.5");
        assert_eq!(cidr.prefix_len, Some(32));

        assert_eq!(policy.permissions.len(), 1);
        let permission::Rule::Any(any) = policy.permissions[0].rule.clone().expect("rule") else {
            panic!("expected Any permission");
        };
        assert!(any, "Any(true)");
    }

    #[test]
    fn egress_none_produces_no_policy_at_all() {
        let listener =
            build_listener(&[record("mcp_session_a", "172.20.0.5", "fs", json!("none"))]);
        let policies = listener_rbac_policies(&listener);
        // No policy means ALLOW + no match = deny. Correct for
        // `egress: none`.
        assert!(
            policies.is_empty(),
            "expected zero policies for `egress: none`, got: {policies:?}"
        );
    }

    // ── Wire-shape regression tests for the `{ "mode": "..." }` form ────────
    //
    // config-broker 0.1.9+ normalises `egress: all` / `egress: none` from
    // `plugins.yaml` into a `{ "mode": "all" }` / `{ "mode": "none" }`
    // object on the wire (see `config-broker::registry::parse_egress`). An
    // earlier version of this module only recognised the bare-string form,
    // which meant every plugin declared `egress: all` (most of them in
    // production) compiled to "no policy" — i.e. a default-deny — and
    // every CONNECT through egress-envoy 403'd. These tests pin the
    // normalised-object form so a future change that drops one of the
    // two encodings is caught here rather than in a smoke run.

    #[test]
    fn egress_mode_all_object_produces_principal_with_any_permission() {
        let listener = build_listener(&[record(
            "mcp_session_a",
            "172.20.0.5",
            "fetch",
            json!({"mode": "all"}),
        )]);
        let policies = listener_rbac_policies(&listener);
        let policy = policies
            .get("session_mcp_session_a")
            .expect("policy for session_a");
        assert_eq!(policy.permissions.len(), 1);
        let permission::Rule::Any(any) = policy.permissions[0].rule.clone().expect("rule") else {
            panic!("expected Any permission");
        };
        assert!(any, "Any(true)");
    }

    #[test]
    fn egress_mode_none_object_produces_no_policy_at_all() {
        let listener = build_listener(&[record(
            "mcp_session_a",
            "172.20.0.5",
            "fs",
            json!({"mode": "none"}),
        )]);
        let policies = listener_rbac_policies(&listener);
        assert!(
            policies.is_empty(),
            "expected zero policies for `egress: {{ mode: \"none\" }}`, got: {policies:?}"
        );
    }

    #[test]
    fn egress_mode_object_and_bare_string_compile_to_identical_policy() {
        // The two encodings MUST be equivalent — that is the whole point
        // of accepting both. Build two listeners with the same session
        // id + IP, one fed the bare-string form and one fed the object
        // form, and assert the resulting RBAC policy is byte-identical.
        let bare = build_listener(&[record("mcp_session_a", "172.20.0.5", "fetch", json!("all"))]);
        let normalised = build_listener(&[record(
            "mcp_session_a",
            "172.20.0.5",
            "fetch",
            json!({"mode": "all"}),
        )]);
        assert_eq!(
            listener_rbac_policies(&bare),
            listener_rbac_policies(&normalised),
            "bare-string and {{ mode: \"all\" }} forms must compile to the same policy",
        );
    }

    #[test]
    fn egress_allow_emits_one_authority_match_per_host_port() {
        let listener = build_listener(&[record(
            "mcp_session_a",
            "172.20.0.5",
            "github-legacy",
            json!({
                "allow": [
                    {"host": "api.github.com", "ports": [443]},
                    {"host": "github.com",     "ports": [443]},
                    // Two ports on one host → two permissions, NOT a
                    // port range. Keeps the compile path one-shape.
                    {"host": "objects.githubusercontent.com", "ports": [443, 80]},
                ]
            }),
        )]);
        let policies = listener_rbac_policies(&listener);
        let policy = policies
            .get("session_mcp_session_a")
            .expect("policy for session_a");
        assert_eq!(policy.permissions.len(), 4, "host:port pairs: {policy:?}");
        let mut authorities: Vec<String> = policy
            .permissions
            .iter()
            .filter_map(|p| match p.rule.clone()? {
                permission::Rule::Header(h) => match h.header_match_specifier? {
                    HeaderMatchSpecifier::ExactMatch(s) => Some(s),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        authorities.sort();
        assert_eq!(
            authorities,
            vec![
                "api.github.com:443",
                "github.com:443",
                "objects.githubusercontent.com:443",
                "objects.githubusercontent.com:80",
            ]
        );
    }

    #[test]
    fn unknown_egress_shape_fails_closed_no_policy() {
        // Should never reach control-plane (config-broker rejects it
        // first) but if it does, fail closed.
        let listener = build_listener(&[
            record(
                "mcp_session_a",
                "172.20.0.5",
                "x",
                json!({"weird": "shape"}),
            ),
            record("mcp_session_b", "172.20.0.6", "y", json!(42)),
            record("mcp_session_c", "172.20.0.7", "z", serde_json::Value::Null),
        ]);
        let policies = listener_rbac_policies(&listener);
        assert!(
            policies.is_empty(),
            "fail-closed: no policies for unrecognised shapes, got: {policies:?}"
        );
    }

    #[test]
    fn allow_with_empty_array_fails_closed_no_policy() {
        let listener = build_listener(&[record(
            "mcp_session_a",
            "172.20.0.5",
            "x",
            json!({"allow": []}),
        )]);
        let policies = listener_rbac_policies(&listener);
        assert!(
            policies.is_empty(),
            "fail-closed: empty allow list = none, got: {policies:?}"
        );
    }

    #[test]
    fn allow_with_only_malformed_entries_fails_closed_no_policy() {
        let listener = build_listener(&[record(
            "mcp_session_a",
            "172.20.0.5",
            "x",
            json!({"allow": [{"no-host": "x"}, {"host": 123}]}),
        )]);
        let policies = listener_rbac_policies(&listener);
        assert!(
            policies.is_empty(),
            "fail-closed: malformed entries = none, got: {policies:?}"
        );
    }

    #[test]
    fn multiple_sessions_produce_one_policy_each_keyed_by_id() {
        let listener = build_listener(&[
            record("mcp_session_a", "172.20.0.5", "fetch", json!("all")),
            record(
                "mcp_session_b",
                "172.20.0.6",
                "github-legacy",
                json!({"allow": [{"host": "api.github.com", "ports": [443]}]}),
            ),
            record("mcp_session_c", "172.20.0.7", "fs", json!("none")),
        ]);
        let policies = listener_rbac_policies(&listener);
        // Only A and B emit policies; C is `egress: none`.
        let mut keys: Vec<&String> = policies.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                &"session_mcp_session_a".to_string(),
                &"session_mcp_session_b".to_string()
            ]
        );

        // Each session's policy carries its own container IP.
        let pol_a = policies.get("session_mcp_session_a").unwrap();
        let principal::Identifier::DirectRemoteIp(cidr_a) =
            pol_a.principals[0].identifier.clone().expect("identifier")
        else {
            panic!("expected DirectRemoteIp on A");
        };
        assert_eq!(
            cidr_a.address_prefix,
            Ipv4Addr::new(172, 20, 0, 5).to_string()
        );

        let pol_b = policies.get("session_mcp_session_b").unwrap();
        let principal::Identifier::DirectRemoteIp(cidr_b) =
            pol_b.principals[0].identifier.clone().expect("identifier")
        else {
            panic!("expected DirectRemoteIp on B");
        };
        assert_eq!(
            cidr_b.address_prefix,
            Ipv4Addr::new(172, 20, 0, 6).to_string()
        );
    }

    #[test]
    fn listener_filter_order_is_rbac_dfp_router() {
        // RBAC MUST come before DFP — we want a denied request to be
        // rejected before envoy goes to resolve DNS for it. And the
        // router MUST be last (terminal filter).
        let listener = build_listener(&[]);
        let filter_chain = listener.filter_chains.first().expect("chain");
        let network_filter = filter_chain.filters.first().expect("filter");
        let FilterConfigType::TypedConfig(any) =
            network_filter.config_type.clone().expect("config_type")
        else {
            panic!("expected TypedConfig");
        };
        let hcm = HttpConnectionManager::decode(any.value.as_slice()).expect("decode");
        let filter_names: Vec<&str> = hcm.http_filters.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            filter_names,
            vec![
                "envoy.filters.http.rbac",
                "envoy.filters.http.dynamic_forward_proxy",
                "envoy.filters.http.router",
            ]
        );
    }

    #[test]
    fn hcm_advertises_connect_upgrade() {
        let listener = build_listener(&[]);
        let filter_chain = listener.filter_chains.first().expect("chain");
        let network_filter = filter_chain.filters.first().expect("filter");
        let FilterConfigType::TypedConfig(any) =
            network_filter.config_type.clone().expect("config_type")
        else {
            panic!("expected TypedConfig");
        };
        let hcm = HttpConnectionManager::decode(any.value.as_slice()).expect("decode");
        let types: Vec<&str> = hcm
            .upgrade_configs
            .iter()
            .map(|u| u.upgrade_type.as_str())
            .collect();
        assert_eq!(types, vec!["CONNECT"]);
    }
}
