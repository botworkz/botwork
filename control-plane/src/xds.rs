//! ADS (Aggregated Discovery Service) gRPC server for envoy egress
//! proxy. Single-stream, SOTW, single management server.
//!
//! ## Wire model
//!
//! Each envoy egress proxy opens one bidi stream
//! (`StreamAggregatedResources`) on connect. Over that stream:
//!
//! 1. envoy sends `DiscoveryRequest { type_url: Listener, version_info: "" }`.
//!    We respond with the current Listener resource and
//!    `version_info = "<generation>"`.
//! 2. envoy sends `DiscoveryRequest { type_url: Cluster, version_info: "" }`.
//!    We respond with the static DFP cluster and the same versioning
//!    discipline.
//! 3. envoy ACKs each response with a request carrying matching
//!    `response_nonce`. We do nothing on a clean ACK; the generation
//!    we already emitted stays "applied".
//! 4. Mutations to `SessionStore` wake us via the `watch::Receiver`.
//!    We re-snapshot, re-compile the Listener (`policy::build_listener`),
//!    and push a fresh `DiscoveryResponse` with a bumped
//!    `version_info`. The Cluster doesn't change so we don't re-push
//!    it.
//! 5. NACK (request with non-empty `error_detail`) → log loudly and
//!    hold at the last-good version. envoy will keep using the last
//!    config it accepted.
//!
//! ## SOTW vs Delta
//!
//! We implement SOTW only. Delta xDS is a perf optimization for the
//! case where you have N resources and only one changes — we only
//! ever ship one Listener and one Cluster, so the entire resource
//! set on each push is just two messages. SOTW is simpler, easier to
//! reason about, and the resource size is small enough that pushing
//! the whole thing on every change costs nothing.
//!
//! `delta_aggregated_resources` is implemented to satisfy the trait
//! but unconditionally errors `unimplemented` — envoy clients
//! configured for `ApiType::Grpc` use the SOTW endpoint by default;
//! `ApiType::DeltaGrpc` is opt-in. If we ever switch, this is the
//! place.
//!
//! ## Concurrency
//!
//! One ADS stream per envoy is expected (we have one egress envoy in
//! the deployment). Each stream gets its own
//! `subscribe_generation()` receiver, its own per-type version
//! counter, its own outbound channel — the server itself is
//! stateless across streams beyond the shared `Arc<SessionStore>`.
//!
//! ## Versioning
//!
//! `version_info` on outbound responses encodes the store generation
//! the response was built against. envoy ACKs by echoing it; we use
//! it solely for debugging (`config_dump` shows it) — we don't gate
//! pushes on the previous ACK because SOTW envoy is "always
//! consistent with the most recent DiscoveryResponse it accepted",
//! and the next push naturally supersedes anything in-flight.

use std::pin::Pin;
use std::sync::Arc;

use async_stream::try_stream;
use envoy_proto::envoy::service::discovery::v3::aggregated_discovery_service_server::{
    AggregatedDiscoveryService, AggregatedDiscoveryServiceServer,
};
use envoy_proto::envoy::service::discovery::v3::{
    DeltaDiscoveryRequest, DeltaDiscoveryResponse, DiscoveryRequest, DiscoveryResponse,
};
use futures_core::Stream;
use prost::Message;
use prost_types::Any;
use tokio_stream::StreamExt;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, info, warn};

use crate::policy::{build_cluster, build_listener, CLUSTER_TYPE_URL, LISTENER_TYPE_URL};
use crate::sessions::SessionStore;

const PREFIX: &str = "[control-plane:xds]";

/// Tonic service implementation. Cheap to clone (just `Arc`-wraps).
pub struct AdsServer {
    sessions: Arc<SessionStore>,
}

impl AdsServer {
    pub fn new(sessions: Arc<SessionStore>) -> Self {
        Self { sessions }
    }

    /// Wrap the service into a tonic gRPC server handle the binary
    /// can hand to `tonic::transport::Server::add_service`.
    pub fn into_grpc_service(self) -> AggregatedDiscoveryServiceServer<Self> {
        AggregatedDiscoveryServiceServer::new(self)
    }
}

/// Box the streaming response item type the trait demands.
type StreamingResponse<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

#[tonic::async_trait]
impl AggregatedDiscoveryService for AdsServer {
    type StreamAggregatedResourcesStream = StreamingResponse<DiscoveryResponse>;
    type DeltaAggregatedResourcesStream = StreamingResponse<DeltaDiscoveryResponse>;

    async fn stream_aggregated_resources(
        &self,
        request: Request<Streaming<DiscoveryRequest>>,
    ) -> Result<Response<Self::StreamAggregatedResourcesStream>, Status> {
        let peer = request
            .remote_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|| "<unknown>".to_string());
        info!("{PREFIX} ADS stream opened from {peer}");

        let sessions = self.sessions.clone();
        let mut inbound = request.into_inner();

        // Each stream tracks the highest generation we've shipped per
        // resource type. The xDS protocol pairs ACKs by nonce, but
        // we keep this for debugging visibility and to suppress
        // redundant pushes for the Cluster (which never changes).
        let mut listener_version: u64 = 0;
        let mut cluster_version_pushed = false;

        let output = try_stream! {
            let mut gen_rx = sessions.subscribe_generation();
            let mut nonce_counter: u64 = 0;
            let mut next_nonce = || {
                nonce_counter += 1;
                nonce_counter.to_string()
            };

            loop {
                tokio::select! {
                    // Inbound from envoy: subscription, ACK, or NACK.
                    msg = inbound.next() => {
                        match msg {
                            Some(Ok(req)) => {
                                debug!(
                                    "{PREFIX} recv from {peer} type_url={} version={} nonce={} resources={:?} has_error={}",
                                    req.type_url,
                                    req.version_info,
                                    req.response_nonce,
                                    req.resource_names,
                                    req.error_detail.is_some(),
                                );

                                if let Some(err) = req.error_detail.as_ref() {
                                    // NACK. envoy rejected our last push.
                                    // Log loudly; we hold at last-good.
                                    warn!(
                                        "{PREFIX} NACK from {peer} type_url={} version={} message={:?}",
                                        req.type_url, req.version_info, err.message
                                    );
                                    continue;
                                }

                                match req.type_url.as_str() {
                                    LISTENER_TYPE_URL => {
                                        // Either initial subscription (empty
                                        // version_info) or ACK of a prior
                                        // push (matching our last version).
                                        // Both reach us as "give me the
                                        // current state."
                                        listener_version += 1;
                                        let sessions = sessions.list().await;
                                        let listener = build_listener(&sessions);
                                        let response = listener_response(
                                            listener_version,
                                            next_nonce(),
                                            &listener,
                                        );
                                        info!(
                                            "{PREFIX} push LDS to {peer} version={} resources={} (sessions={})",
                                            listener_version,
                                            response.resources.len(),
                                            sessions.len(),
                                        );
                                        yield response;
                                    }
                                    CLUSTER_TYPE_URL => {
                                        // The DFP cluster is static — push
                                        // it exactly once per stream and
                                        // ignore subsequent re-subscribes
                                        // (envoy will keep using the
                                        // version it has).
                                        if !cluster_version_pushed {
                                            let cluster = build_cluster();
                                            let response = cluster_response(
                                                1,
                                                next_nonce(),
                                                &cluster,
                                            );
                                            info!(
                                                "{PREFIX} push CDS to {peer} version=1 resources={}",
                                                response.resources.len()
                                            );
                                            cluster_version_pushed = true;
                                            yield response;
                                        } else {
                                            debug!(
                                                "{PREFIX} CDS re-subscribe from {peer} version={} -- already pushed v1",
                                                req.version_info
                                            );
                                        }
                                    }
                                    other => {
                                        // We don't serve EDS/RDS/SRDS. envoy
                                        // shouldn't subscribe to them given
                                        // our bootstrap, but if it does we
                                        // just don't respond — envoy times
                                        // out the type and moves on.
                                        warn!(
                                            "{PREFIX} unexpected subscription from {peer}: type_url={other}"
                                        );
                                    }
                                }
                            }
                            Some(Err(err)) => {
                                error!("{PREFIX} stream from {peer} errored: {err}");
                                return;
                            }
                            None => {
                                info!("{PREFIX} stream from {peer} closed");
                                return;
                            }
                        }
                    }
                    // Session store mutated → push fresh Listener.
                    // (DFP cluster doesn't change with sessions.)
                    changed = gen_rx.changed() => {
                        if changed.is_err() {
                            // Store dropped: server is going away.
                            return;
                        }
                        let store_gen = *gen_rx.borrow_and_update();
                        listener_version += 1;
                        let sessions = sessions.list().await;
                        let listener = build_listener(&sessions);
                        let response = listener_response(
                            listener_version,
                            next_nonce(),
                            &listener,
                        );
                        info!(
                            "{PREFIX} push LDS to {peer} version={} (store_gen={} sessions={})",
                            listener_version, store_gen, sessions.len()
                        );
                        yield response;
                    }
                }
            }
        };

        Ok(Response::new(Box::pin(output)))
    }

    async fn delta_aggregated_resources(
        &self,
        _request: Request<Streaming<DeltaDiscoveryRequest>>,
    ) -> Result<Response<Self::DeltaAggregatedResourcesStream>, Status> {
        // envoy's default ApiType for ADS is `Grpc` (SOTW). Delta is
        // opt-in via `ApiType::DeltaGrpc`. Our bootstrap pins SOTW
        // so this should never be hit; if it is, we want envoy to
        // surface it as "the control plane refused" rather than us
        // silently negotiating something else.
        Err(Status::unimplemented(
            "delta xDS is not implemented; configure envoy with ApiType::Grpc (SOTW)",
        ))
    }
}

fn listener_response(
    version: u64,
    nonce: String,
    listener: &envoy_proto::envoy::config::listener::v3::Listener,
) -> DiscoveryResponse {
    DiscoveryResponse {
        version_info: version.to_string(),
        type_url: LISTENER_TYPE_URL.to_string(),
        nonce,
        resources: vec![Any {
            type_url: LISTENER_TYPE_URL.to_string(),
            value: listener.encode_to_vec(),
        }],
        ..Default::default()
    }
}

fn cluster_response(
    version: u64,
    nonce: String,
    cluster: &envoy_proto::envoy::config::cluster::v3::Cluster,
) -> DiscoveryResponse {
    DiscoveryResponse {
        version_info: version.to_string(),
        type_url: CLUSTER_TYPE_URL.to_string(),
        nonce,
        resources: vec![Any {
            type_url: CLUSTER_TYPE_URL.to_string(),
            value: cluster.encode_to_vec(),
        }],
        ..Default::default()
    }
}
