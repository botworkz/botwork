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
//!    `version_info = "<store-generation>"`.
//! 2. envoy sends `DiscoveryRequest { type_url: Cluster, version_info: "" }`.
//!    We respond with the static DFP cluster (constant `version_info: "1"`).
//! 3. envoy ACKs each response with a request carrying matching
//!    `version_info`. For LDS, we record the ACKed version via
//!    `SessionStore::record_acked_version` — that's what
//!    [`crate::sessions::SessionStore::wait_for_ack`] unblocks on.
//!    Critically we DO NOT push another response in this case: the
//!    version_info envoy sent back matches what's currently in the
//!    store, so envoy is already up to date. Pushing again here
//!    would just trigger another ACK with a fresh nonce, which we'd
//!    then re-push, which would re-ACK, ad infinitum -- the bug
//!    that caused the "version=60+ resources=1 sessions=0" log
//!    storm in the first iteration of this server.
//! 4. Mutations to `SessionStore` wake us via the generation watch
//!    channel. We re-snapshot, re-compile the Listener
//!    (`policy::build_listener`), and push a fresh `DiscoveryResponse`
//!    with `version_info = "<new-store-generation>"`. The Cluster
//!    doesn't change so we don't re-push it.
//! 5. NACK (request with non-empty `error_detail`) → log loudly and
//!    hold at the last-good version. envoy keeps the previous config;
//!    we do *not* record the NACKed version into the ack channel, so
//!    HTTP handlers waiting for that version block until either a
//!    fresh push gets ACKed or their timeout fires (→ 503).
//!
//! ## Versioning is the store generation, verbatim
//!
//! `version_info` is the `SessionStore` generation counter, formatted
//! as a decimal string. This is load-bearing: the HTTP `POST /sessions`
//! handler calls `current_generation()` immediately after the mutation
//! (so it captures the value the xDS task will subsequently push) and
//! then `wait_for_ack(that_generation)`. If we used a parallel counter
//! the handler would have no way to map "the mutation I just made" to
//! "the version envoy ACKed."
//!
//! Generation values are u64 and wrap. The wrap horizon (2^64) is so
//! far past anything plausible that we don't special-case it; if the
//! store ever gets restarted often enough to wrap, the next deploy
//! will start fresh from 0 anyway.
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
//! ## Concurrency + subscriber tracking
//!
//! One ADS stream per envoy is expected (we have one egress envoy in
//! the deployment). Each stream gets its own
//! `subscribe_generation()` receiver, its own outbound channel, and
//! holds an [`XdsSubscriberGuard`] for its lifetime so the HTTP
//! handler can tell whether an ack-wait is hopeless (no subscriber)
//! or worth blocking on.

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

/// `version_info` we ship on the static DFP cluster. The cluster
/// doesn't change with session state, so this is a constant.
const CLUSTER_VERSION: &str = "1";

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

        let mut cluster_version_pushed = false;

        // Acquire the subscriber guard and MOVE it into the
        // try_stream block. The guard's Drop is what decrements the
        // subscriber counter; if we bound it to a let-binding in the
        // outer function it would be dropped here, before the
        // try_stream! body runs, and the count would be 0 by the
        // time envoy's first DiscoveryRequest landed.
        //
        // This was the cause of "no_xds_subscriber" 503s on every
        // POST /sessions in CI even though the ADS stream itself
        // was working fine: the egress envoy was connected and
        // ACKing, but the counter never had a chance to be >=1
        // because the guard had already been dropped.
        //
        // Moving it into the `move` block keeps it alive for the
        // life of the future driving the stream -- which is
        // exactly the life of the gRPC stream as a whole.
        let subscriber_guard = sessions.xds_subscriber_guard();

        let output = try_stream! {
            // `subscriber_guard` is captured by the move; explicitly
            // bind it with `let _` so its lifetime is the full body
            // and an unused-variable lint doesn't get clever and
            // drop it early.
            let _subscriber = subscriber_guard;
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
                                    // Log loudly; we hold at last-good and
                                    // do NOT record this version into the
                                    // ack channel -- any HTTP gate waiter
                                    // expecting this version blocks until
                                    // either a subsequent push gets ACKed
                                    // (rare -- something has to mutate the
                                    // store to trigger it) or their
                                    // timeout fires.
                                    warn!(
                                        "{PREFIX} NACK from {peer} type_url={} version={} message={:?}",
                                        req.type_url, req.version_info, err.message
                                    );
                                    continue;
                                }

                                match req.type_url.as_str() {
                                    LISTENER_TYPE_URL => {
                                        // Always record the ACK (if this
                                        // is one): it costs nothing and
                                        // is what the HTTP gate
                                        // (wait_for_ack) is watching for.
                                        if !req.version_info.is_empty() {
                                            if let Ok(acked) = req.version_info.parse::<u64>() {
                                                sessions.record_acked_version(acked);
                                                debug!(
                                                    "{PREFIX} recorded LDS ACK from {peer} version={acked}"
                                                );
                                            } else {
                                                // version_info we sent is
                                                // always a decimal u64.
                                                // Anything else means
                                                // someone else is talking
                                                // to us, or envoy is
                                                // corrupted. Don't crash.
                                                warn!(
                                                    "{PREFIX} ignored LDS ACK from {peer} with non-numeric version {:?}",
                                                    req.version_info
                                                );
                                            }
                                        }

                                        // Decide whether to push.
                                        //
                                        // SOTW xDS protocol: an inbound
                                        // DiscoveryRequest can be:
                                        //   - initial subscription
                                        //     (empty version_info) →
                                        //     PUSH current state
                                        //   - ACK of our last response
                                        //     (version_info == current
                                        //     generation) → DO NOT push;
                                        //     envoy is happy and another
                                        //     push would re-trigger an
                                        //     ACK in an infinite loop
                                        //     (each push uses a fresh
                                        //     nonce, so envoy thinks
                                        //     it's a new response)
                                        //   - stale ACK / re-subscribe
                                        //     after restart
                                        //     (version_info != current
                                        //     generation) → PUSH current
                                        //
                                        // The bug this protects against
                                        // is real and visible: PR1
                                        // pushed unconditionally and CI
                                        // produced version=60+ with zero
                                        // sessions in the store --
                                        // entirely ACK-loop noise.
                                        let current_gen = sessions.current_generation();
                                        let envoy_has = if req.version_info.is_empty() {
                                            None
                                        } else {
                                            req.version_info.parse::<u64>().ok()
                                        };

                                        if envoy_has != Some(current_gen) {
                                            let snapshot = sessions.list().await;
                                            let listener = build_listener(&snapshot);
                                            let response = listener_response(
                                                current_gen,
                                                next_nonce(),
                                                &listener,
                                            );
                                            info!(
                                                "{PREFIX} push LDS to {peer} version={} resources={} (sessions={}) envoy_had={:?}",
                                                current_gen,
                                                response.resources.len(),
                                                snapshot.len(),
                                                envoy_has,
                                            );
                                            yield response;
                                        } else {
                                            debug!(
                                                "{PREFIX} LDS ACK from {peer} matches current generation {current_gen}; no push"
                                            );
                                        }
                                    }
                                    CLUSTER_TYPE_URL => {
                                        // DFP cluster is static. Push
                                        // once per stream; ignore re-subs.
                                        // We don't bother recording cluster
                                        // ACKs because the HTTP gate only
                                        // ever waits on LDS.
                                        if !cluster_version_pushed {
                                            let cluster = build_cluster();
                                            let response = cluster_response(
                                                CLUSTER_VERSION,
                                                next_nonce(),
                                                &cluster,
                                            );
                                            info!(
                                                "{PREFIX} push CDS to {peer} version={CLUSTER_VERSION} resources={}",
                                                response.resources.len()
                                            );
                                            cluster_version_pushed = true;
                                            yield response;
                                        } else {
                                            debug!(
                                                "{PREFIX} CDS re-subscribe from {peer} version={} -- already pushed v{CLUSTER_VERSION}",
                                                req.version_info
                                            );
                                        }
                                    }
                                    other => {
                                        // We don't serve EDS/RDS/SRDS. envoy
                                        // shouldn't subscribe to them given
                                        // our bootstrap, but if it does we
                                        // just don't respond.
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
                    // Session store mutated → push fresh Listener with
                    // the new store generation as version_info.
                    changed = gen_rx.changed() => {
                        if changed.is_err() {
                            // Store dropped: server is going away.
                            return;
                        }
                        let store_gen = *gen_rx.borrow_and_update();
                        let snapshot = sessions.list().await;
                        let listener = build_listener(&snapshot);
                        let response = listener_response(
                            store_gen,
                            next_nonce(),
                            &listener,
                        );
                        info!(
                            "{PREFIX} push LDS to {peer} version={} (store_gen={} sessions={})",
                            store_gen, store_gen, snapshot.len()
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
    version: &str,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_and_service_wrapper_are_usable() {
        let sessions = Arc::new(SessionStore::new());
        let server = AdsServer::new(sessions);
        let _grpc = server.into_grpc_service();
    }
}
