//! End-to-end integration test for the ADS gRPC server.
//!
//! Spins up `AdsServer` on a real `tcp:127.0.0.1:0` socket and drives
//! it via the tonic-generated client from the same `envoy_proto`
//! crate. This is the test that proves we'll actually interoperate
//! with envoy at the wire level — the lib-side `policy::tests`
//! confirm the protobuf shapes; this one confirms the gRPC framing,
//! the ADS request/response sequencing, and the watch-channel-driven
//! push-on-mutate behaviour.
//!
//! Each test gets its own server / store: state isolation is more
//! valuable than the small startup cost.

use std::sync::Arc;
use std::time::Duration;

use botwork_control_plane::{policy, AdsServer, SessionRecord, SessionStore};
use envoy_proto::envoy::config::cluster::v3::Cluster;
use envoy_proto::envoy::config::listener::v3::Listener;
use envoy_proto::envoy::service::discovery::v3::aggregated_discovery_service_client::AggregatedDiscoveryServiceClient;
use envoy_proto::envoy::service::discovery::v3::{DiscoveryRequest, DiscoveryResponse};
use prost::Message;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tonic::transport::Server as TonicServer;

struct XdsServer {
    base: String,
    store: Arc<SessionStore>,
    _handle: JoinHandle<()>,
}

async fn spawn_xds() -> XdsServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

    let store = Arc::new(SessionStore::new());
    let ads = AdsServer::new(store.clone());

    let handle = tokio::spawn(async move {
        let _ = TonicServer::builder()
            .add_service(ads.into_grpc_service())
            .serve_with_incoming(incoming)
            .await;
    });

    // Give tonic a moment to start accepting; subsequent connects
    // sometimes race the bind otherwise.
    tokio::time::sleep(Duration::from_millis(50)).await;

    XdsServer {
        base: format!("http://{addr}"),
        store,
        _handle: handle,
    }
}

fn record(id: &str, ip: &str, plugin: &str, egress: serde_json::Value) -> SessionRecord {
    SessionRecord {
        session_id: id.to_string(),
        container_ip: ip.parse().expect("test ip"),
        tenant: "phlax".to_string(),
        workspace: "mcp".to_string(),
        plugin: plugin.to_string(),
        egress_policy: egress,
    }
}

/// Open the bidi stream and return the `(outbound tx, inbound stream)`
/// pair. Mirrors what envoy does on connect.
async fn open_stream(
    base: &str,
) -> (
    mpsc::Sender<DiscoveryRequest>,
    tonic::Streaming<DiscoveryResponse>,
) {
    let endpoint = tonic::transport::Endpoint::from_shared(base.to_string())
        .expect("parse endpoint")
        .connect()
        .await
        .expect("connect");
    let mut client = AggregatedDiscoveryServiceClient::new(endpoint);

    let (tx, rx) = mpsc::channel::<DiscoveryRequest>(16);
    let outbound = ReceiverStream::new(rx);

    let response = client
        .stream_aggregated_resources(outbound)
        .await
        .expect("stream open");
    (tx, response.into_inner())
}

fn lds_request(version: &str, nonce: &str) -> DiscoveryRequest {
    DiscoveryRequest {
        type_url: policy::LISTENER_TYPE_URL.to_string(),
        version_info: version.to_string(),
        response_nonce: nonce.to_string(),
        ..Default::default()
    }
}

fn cds_request(version: &str, nonce: &str) -> DiscoveryRequest {
    DiscoveryRequest {
        type_url: policy::CLUSTER_TYPE_URL.to_string(),
        version_info: version.to_string(),
        response_nonce: nonce.to_string(),
        ..Default::default()
    }
}

async fn next_response_with_timeout(
    stream: &mut tonic::Streaming<DiscoveryResponse>,
) -> DiscoveryResponse {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("timed out waiting for DiscoveryResponse")
        .expect("stream ended")
        .expect("stream error")
}

#[tokio::test]
async fn lds_subscription_returns_listener_with_zero_policies_on_empty_store() {
    let server = spawn_xds().await;
    let (tx, mut stream) = open_stream(&server.base).await;

    tx.send(lds_request("", ""))
        .await
        .expect("send lds subscribe");

    let response = next_response_with_timeout(&mut stream).await;
    assert_eq!(response.type_url, policy::LISTENER_TYPE_URL);
    assert_eq!(response.resources.len(), 1);

    let any = &response.resources[0];
    let listener = Listener::decode(any.value.as_slice()).expect("decode listener");
    assert_eq!(listener.name, policy::LISTENER_NAME);
    // Empty store → policies are empty (ALLOW + no match = deny).
    // Decoding the inner RBAC is done in lib-side `policy::tests`;
    // we just confirm here that the resource_name matches and that
    // the listener round-trips wire-correctly.

    // The server emits a non-empty version_info even on the first push
    // so envoy can ACK by echoing it back. Mostly a sanity sentinel.
    assert!(
        !response.version_info.is_empty(),
        "version_info should be non-empty, got: {response:?}"
    );
    assert!(
        !response.nonce.is_empty(),
        "nonce should be non-empty, got: {response:?}"
    );
}

#[tokio::test]
async fn ack_with_matching_version_does_not_trigger_a_push() {
    // Regression test for the ACK-loop bug that surfaced in
    // botworkz/vm#95 CI: a real envoy (1.38) re-sends a
    // DiscoveryRequest after every server push to ACK it. The
    // first iteration of this server pushed unconditionally on
    // any inbound LDS request, so each push → ACK → push → ACK
    // ad infinitum, producing "version=60+ resources=1
    // sessions=0" log spam and starving the ingress envoy's
    // initialize call.
    //
    // The contract this test pins: an inbound LDS request whose
    // version_info matches the current store generation MUST NOT
    // produce another push. Without that, envoy and the server
    // amplify forever and the first POST /sessions never gets a
    // dedicated push opportunity to ACK.
    let server = spawn_xds().await;
    let (tx, mut stream) = open_stream(&server.base).await;

    // 1) Initial subscribe → first push (version=0).
    tx.send(lds_request("", ""))
        .await
        .expect("send lds subscribe");
    let first = next_response_with_timeout(&mut stream).await;
    assert_eq!(first.version_info, "0");

    // 2) ACK that push with the same version_info envoy would echo
    //    back. Critically, the response_nonce field is the nonce
    //    we sent (envoy echoes it verbatim on ACK).
    tx.send(lds_request(&first.version_info, &first.nonce))
        .await
        .expect("send ack");

    // 3) Expect ZERO further pushes within a reasonable window.
    //    If the bug were present, we'd see an infinite stream of
    //    pushes; the timeout below picks up the absence.
    let outcome = tokio::time::timeout(Duration::from_millis(250), stream.next()).await;
    assert!(
        outcome.is_err(),
        "expected no push after ACK with matching version; got: {outcome:?}"
    );
}

#[tokio::test]
async fn open_stream_bumps_subscriber_count_for_full_lifetime() {
    // Regression test for the subscriber-guard lifetime bug that
    // surfaced in botworkz/vm#96 CI: the xDS server acquired the
    // guard in `stream_aggregated_resources` and bound it to a
    // local `let _subscriber = ...` outside the try_stream! block.
    // Rust dropped the guard at the end of that outer function
    // (right after returning Ok(Response::new(...))), so by the
    // time envoy's first DiscoveryRequest landed, the subscriber
    // counter was already 0 and every POST /sessions's wait_for_ack
    // short-circuited to 503 no_xds_subscriber.
    //
    // The fix moves the guard INTO the try_stream! body so it lives
    // as long as the stream future. This test pins the contract:
    // after the client has opened the stream, the count must be >= 1.
    let server = spawn_xds().await;
    assert_eq!(
        server.store.xds_subscriber_count(),
        0,
        "no subscribers before open"
    );

    let (tx, mut stream) = open_stream(&server.base).await;

    // Drive at least one round-trip so the server task has definitely
    // entered the try_stream! body (and therefore taken ownership of
    // the guard). Without this, we'd be racing the spawn.
    tx.send(lds_request("", ""))
        .await
        .expect("send lds subscribe");
    let _first = next_response_with_timeout(&mut stream).await;

    assert_eq!(
        server.store.xds_subscriber_count(),
        1,
        "subscriber count must be >= 1 while stream is open"
    );

    // Drop client side; server task should observe end-of-stream and
    // drop the guard. The count returning to 0 is what
    // `wait_for_ack`'s mid-wait subscriber-disappearance check
    // depends on, so it's worth pinning too.
    drop(tx);
    drop(stream);

    // Wait briefly for the server task to notice the stream end.
    for _ in 0..50 {
        if server.store.xds_subscriber_count() == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "subscriber count never returned to 0 after stream close (currently {})",
        server.store.xds_subscriber_count()
    );
}

#[tokio::test]
async fn cds_subscription_returns_dfp_cluster_once() {
    let server = spawn_xds().await;
    let (tx, mut stream) = open_stream(&server.base).await;

    tx.send(cds_request("", ""))
        .await
        .expect("send cds subscribe");

    let response = next_response_with_timeout(&mut stream).await;
    assert_eq!(response.type_url, policy::CLUSTER_TYPE_URL);
    assert_eq!(response.resources.len(), 1);

    let any = &response.resources[0];
    let cluster = Cluster::decode(any.value.as_slice()).expect("decode cluster");
    assert_eq!(cluster.name, policy::CLUSTER_NAME);

    // Re-subscribing to CDS — the cluster is static, so the server
    // does NOT push a second message. We send a second CDS request
    // and confirm we time out waiting for a response that never
    // comes (server is supposed to silently ignore the re-sub).
    tx.send(cds_request(&response.version_info, &response.nonce))
        .await
        .expect("send cds re-subscribe");

    let outcome = tokio::time::timeout(Duration::from_millis(250), stream.next()).await;
    assert!(
        outcome.is_err(),
        "expected no second CDS push (cluster static); got: {outcome:?}"
    );
}

#[tokio::test]
async fn store_mutation_pushes_fresh_listener_to_open_stream() {
    let server = spawn_xds().await;
    let (tx, mut stream) = open_stream(&server.base).await;

    // Initial subscribe gives us the empty listener.
    tx.send(lds_request("", ""))
        .await
        .expect("send initial lds");
    let first = next_response_with_timeout(&mut stream).await;
    assert_eq!(first.type_url, policy::LISTENER_TYPE_URL);
    let first_listener = Listener::decode(first.resources[0].value.as_slice()).expect("decode");
    assert_eq!(first_listener.filter_chains.len(), 1);

    // Now mutate the store. The xDS server is watching the store's
    // generation channel and MUST push a fresh listener without us
    // sending another DiscoveryRequest.
    server
        .store
        .insert(record(
            "mcp_session_abc",
            "172.20.0.5",
            "fetch",
            serde_json::json!("all"),
        ))
        .await
        .expect("insert");

    let pushed = next_response_with_timeout(&mut stream).await;
    assert_eq!(pushed.type_url, policy::LISTENER_TYPE_URL);
    assert_ne!(
        pushed.version_info, first.version_info,
        "version_info should bump on mutation: first={} pushed={}",
        first.version_info, pushed.version_info
    );

    // The pushed listener should carry the new session as an RBAC
    // policy. (Detailed shape verified in lib tests; here we just
    // confirm the round-trip works end-to-end.)
    let pushed_listener = Listener::decode(pushed.resources[0].value.as_slice()).expect("decode");
    assert_eq!(pushed_listener.filter_chains.len(), 1);
    assert_eq!(pushed_listener.name, policy::LISTENER_NAME);
}

#[tokio::test]
async fn delete_session_pushes_updated_listener() {
    let server = spawn_xds().await;
    // Seed the store BEFORE opening the stream so the initial push
    // reflects the seeded state — no race against the watch channel.
    server
        .store
        .insert(record(
            "mcp_session_abc",
            "172.20.0.5",
            "fetch",
            serde_json::json!("all"),
        ))
        .await
        .expect("insert");

    let (tx, mut stream) = open_stream(&server.base).await;
    tx.send(lds_request("", ""))
        .await
        .expect("send initial lds");
    let initial = next_response_with_timeout(&mut stream).await;
    let initial_version = initial.version_info.clone();

    server
        .store
        .remove("mcp_session_abc")
        .await
        .expect("remove");

    let pushed = next_response_with_timeout(&mut stream).await;
    assert_ne!(pushed.version_info, initial_version);
    let pushed_listener = Listener::decode(pushed.resources[0].value.as_slice()).expect("decode");
    assert_eq!(pushed_listener.name, policy::LISTENER_NAME);
}

#[tokio::test]
async fn nack_does_not_crash_or_overwrite_version() {
    let server = spawn_xds().await;
    let (tx, mut stream) = open_stream(&server.base).await;

    tx.send(lds_request("", ""))
        .await
        .expect("send initial lds");
    let initial = next_response_with_timeout(&mut stream).await;

    // NACK the response. The server should log and hold; we should
    // NOT receive a fresh listener as a result of the NACK itself
    // (only as a result of subsequent mutations or new subscribe).
    let nack = DiscoveryRequest {
        type_url: policy::LISTENER_TYPE_URL.to_string(),
        version_info: initial.version_info.clone(),
        response_nonce: initial.nonce.clone(),
        error_detail: Some(envoy_proto::google::rpc::Status {
            code: 13, // INTERNAL
            message: "test nack — pretending envoy rejected the config".to_string(),
            details: vec![],
        }),
        ..Default::default()
    };
    tx.send(nack).await.expect("send nack");

    // No follow-up message should arrive in a reasonable window.
    let outcome = tokio::time::timeout(Duration::from_millis(250), stream.next()).await;
    assert!(
        outcome.is_err(),
        "NACK should not trigger another push; got: {outcome:?}"
    );

    // The store still works after the NACK — a real mutation pushes
    // as normal. This proves the server didn't drop the stream on
    // the NACK.
    server
        .store
        .insert(record(
            "mcp_session_x",
            "172.20.0.5",
            "fetch",
            serde_json::json!("all"),
        ))
        .await
        .expect("insert");
    let after = next_response_with_timeout(&mut stream).await;
    assert_eq!(after.type_url, policy::LISTENER_TYPE_URL);
    assert_ne!(after.version_info, initial.version_info);
}

#[tokio::test]
async fn mixed_egress_modes_compile_to_correct_policy_count() {
    let server = spawn_xds().await;

    // Three sessions, three modes — but only two should produce a
    // policy. The third (`egress: none`) is a default-deny.
    server
        .store
        .bulk_seed(vec![
            record(
                "mcp_session_a",
                "172.20.0.5",
                "fetch",
                serde_json::json!("all"),
            ),
            record(
                "mcp_session_b",
                "172.20.0.6",
                "github-legacy",
                serde_json::json!({
                    "allow": [{"host": "api.github.com", "ports": [443]}]
                }),
            ),
            record(
                "mcp_session_c",
                "172.20.0.7",
                "fs",
                serde_json::json!("none"),
            ),
        ])
        .await
        .expect("seed");

    let (tx, mut stream) = open_stream(&server.base).await;
    tx.send(lds_request("", ""))
        .await
        .expect("send lds subscribe");
    let response = next_response_with_timeout(&mut stream).await;

    let listener = Listener::decode(response.resources[0].value.as_slice()).expect("decode");

    // Decode all the way down to the RBAC policies to confirm exactly
    // two policies were emitted (A and B), not three.
    use envoy_proto::envoy::config::listener::v3::filter::ConfigType as FilterConfigType;
    use envoy_proto::envoy::extensions::filters::http::rbac::v3::Rbac as RbacFilter;
    use envoy_proto::envoy::extensions::filters::network::http_connection_manager::v3::http_filter::ConfigType as HttpFilterConfigType;
    use envoy_proto::envoy::extensions::filters::network::http_connection_manager::v3::HttpConnectionManager;

    let chain = &listener.filter_chains[0];
    let FilterConfigType::TypedConfig(hcm_any) =
        chain.filters[0].config_type.clone().expect("config_type")
    else {
        panic!("expected TypedConfig on network filter");
    };
    let hcm = HttpConnectionManager::decode(hcm_any.value.as_slice()).expect("decode hcm");
    let rbac_filter = hcm
        .http_filters
        .iter()
        .find(|f| f.name == "envoy.filters.http.rbac")
        .expect("rbac filter present");
    let HttpFilterConfigType::TypedConfig(rbac_any) =
        rbac_filter.config_type.clone().expect("rbac config_type")
    else {
        panic!("expected TypedConfig on rbac filter");
    };
    let rbac = RbacFilter::decode(rbac_any.value.as_slice()).expect("decode rbac");
    let policies = rbac.rules.expect("rules").policies;
    assert_eq!(
        policies.len(),
        2,
        "expected 2 policies (A=all, B=allow); none for C=none. got: {:?}",
        policies.keys().collect::<Vec<_>>()
    );
    assert!(policies.contains_key("session_mcp_session_a"));
    assert!(policies.contains_key("session_mcp_session_b"));
    assert!(!policies.contains_key("session_mcp_session_c"));
}

/// End-to-end wire-shape regression for the config-broker normalised
/// egress encoding. config-broker 0.1.9+ normalises `egress: all` /
/// `egress: none` from `plugins.yaml` into a `{ "mode": "all" }` /
/// `{ "mode": "none" }` object (see
/// `config-broker::registry::parse_egress`) and session-broker
/// forwards that verbatim into `egress_policy` on `POST /sessions`.
/// An earlier iteration of `permissions_for_egress` only recognised
/// the bare-string sugar, so every plugin declared `egress: all` in
/// production silently compiled to "no policy" — denial — and every
/// CONNECT through egress-envoy 403'd. The lib-side `policy::tests`
/// cover both encodings; this xds-level test pins the contract
/// across the gRPC boundary too.
#[tokio::test]
async fn mixed_egress_mode_object_form_compiles_to_correct_policy_count() {
    let server = spawn_xds().await;

    server
        .store
        .bulk_seed(vec![
            record(
                "mcp_session_a",
                "172.20.0.5",
                "fetch",
                serde_json::json!({"mode": "all"}),
            ),
            record(
                "mcp_session_b",
                "172.20.0.6",
                "github-legacy",
                serde_json::json!({
                    "allow": [{"host": "api.github.com", "ports": [443]}]
                }),
            ),
            record(
                "mcp_session_c",
                "172.20.0.7",
                "fs",
                serde_json::json!({"mode": "none"}),
            ),
        ])
        .await
        .expect("seed");

    let (tx, mut stream) = open_stream(&server.base).await;
    tx.send(lds_request("", ""))
        .await
        .expect("send lds subscribe");
    let response = next_response_with_timeout(&mut stream).await;

    let listener = Listener::decode(response.resources[0].value.as_slice()).expect("decode");

    use envoy_proto::envoy::config::listener::v3::filter::ConfigType as FilterConfigType;
    use envoy_proto::envoy::extensions::filters::http::rbac::v3::Rbac as RbacFilter;
    use envoy_proto::envoy::extensions::filters::network::http_connection_manager::v3::http_filter::ConfigType as HttpFilterConfigType;
    use envoy_proto::envoy::extensions::filters::network::http_connection_manager::v3::HttpConnectionManager;

    let chain = &listener.filter_chains[0];
    let FilterConfigType::TypedConfig(hcm_any) =
        chain.filters[0].config_type.clone().expect("config_type")
    else {
        panic!("expected TypedConfig on network filter");
    };
    let hcm = HttpConnectionManager::decode(hcm_any.value.as_slice()).expect("decode hcm");
    let rbac_filter = hcm
        .http_filters
        .iter()
        .find(|f| f.name == "envoy.filters.http.rbac")
        .expect("rbac filter present");
    let HttpFilterConfigType::TypedConfig(rbac_any) =
        rbac_filter.config_type.clone().expect("rbac config_type")
    else {
        panic!("expected TypedConfig on rbac filter");
    };
    let rbac = RbacFilter::decode(rbac_any.value.as_slice()).expect("decode rbac");
    let policies = rbac.rules.expect("rules").policies;
    assert_eq!(
        policies.len(),
        2,
        "expected 2 policies (A={{mode:all}}, B=allow); none for C={{mode:none}}. got: {:?}",
        policies.keys().collect::<Vec<_>>()
    );
    assert!(policies.contains_key("session_mcp_session_a"));
    assert!(policies.contains_key("session_mcp_session_b"));
    assert!(!policies.contains_key("session_mcp_session_c"));
}
