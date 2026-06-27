//! `botwork-api` — HTTP+JSON CRUD service on top of `botwork-entity`.
//!
//! This crate is the operator-facing writer over the v0 schema. It is
//! the eventual replacement for `botwork-bootstrap`; for now both
//! live side-by-side and share the per-entry validators in
//! `botwork-api-core`.
//!
//! # What v0 ships (post-RFE #106 PR3)
//!
//! Full CRUD over all four entities:
//!
//! ```text
//! GET    /admin/api/v1/health                            -> { status, db }
//!
//! GET    /admin/api/v1/tenants                           -> { items, total }
//! POST   /admin/api/v1/tenants                           -> 201 Tenant + Location
//! GET    /admin/api/v1/tenants/{id}                      -> Tenant
//! PUT    /admin/api/v1/tenants/{id}                      -> 200 Tenant
//! DELETE /admin/api/v1/tenants/{id}                      -> 204 / 409
//!
//! GET    /admin/api/v1/workspaces ?tenant_id=            -> { items, total }
//! POST   /admin/api/v1/workspaces                        -> 201 Workspace + Location
//! GET    /admin/api/v1/workspaces/{id}                   -> Workspace
//! PUT    /admin/api/v1/workspaces/{id}                   -> 200 Workspace
//! DELETE /admin/api/v1/workspaces/{id}                   -> 204
//!   (CASCADEs bindings; live sessions terminated through control-plane)
//!
//! GET    /admin/api/v1/plugins                           -> { items, total }
//! POST   /admin/api/v1/plugins                           -> 201 Plugin + Location
//! GET    /admin/api/v1/plugins/{id}                      -> Plugin
//! PUT    /admin/api/v1/plugins/{id}                      -> 200 Plugin
//! DELETE /admin/api/v1/plugins/{id}                      -> 204 / 409
//!
//! GET    /admin/api/v1/workspace_plugins                 -> { items, total }
//!        ?workspace_id= &plugin_id=
//! POST   /admin/api/v1/workspace_plugins                 -> 201 WorkspacePlugin
//! GET    /admin/api/v1/workspace_plugins/{wid}/{pid}     -> WorkspacePlugin
//! PUT    /admin/api/v1/workspace_plugins/{wid}/{pid}     -> 200 WorkspacePlugin
//!   (live sessions for the triple terminated through control-plane)
//! DELETE /admin/api/v1/workspace_plugins/{wid}/{pid}     -> 204
//!   (live sessions for the triple terminated through control-plane)
//!
//! POST   /admin/api/v1/secrets                           -> 201 { stored, created } + Location
//! DELETE /admin/api/v1/secrets/{service}/{name}          -> 204 / 404
//! ```
//!
//! # Response shapes
//!
//! * **Success** — entity model serialised verbatim via SeaORM's
//!   derived `Serialize`. List endpoints wrap in
//!   `{ "items": [...], "total": N }` so pagination can land later
//!   without a breaking change.
//! * **Error envelope** (mirrors config-broker / control-plane):
//!
//!   ```json
//!   { "error": "<machine code>", "message": "<human detail>" }
//!   ```
//!
//!   v0 emits: `not_found` (404), `bad_request` (400),
//!   `validation_failed` (422), `has_dependents` / `stale_write` /
//!   `already_exists` (409), `unavailable` (503), `internal` (500).
//!   `has_dependents` adds a `dependents` array describing each
//!   blocker. `unavailable` (503) covers both control-plane backend
//!   failures and secret-store backend failures.
//!
//! # Optimistic locking
//!
//! `PUT` bodies and `DELETE` query params carry an
//! `if_unmodified_since` field (`DateTime<Utc>`, RFC3339). The handler
//! compares against the live `updated_at` inside a transaction and
//! returns 409 `stale_write` on mismatch. Same token for both verbs
//! so the UI flow is uniform: GET → render → if mutate → include the
//! token from the GET response.
//!
//! # Live-state coupling (control-plane gate)
//!
//! Writes that affect already-spawned sessions coordinate with
//! `botwork-control-plane`:
//!
//! * `PUT  /workspace_plugins/{wid}/{pid}` — config change ⇒ live
//!   sessions for the triple are terminated via control-plane's
//!   existing ack-gated `DELETE /sessions/<id>`. The next spawn
//!   picks up the new config.
//! * `DELETE /workspace_plugins/{wid}/{pid}` — same.
//! * `DELETE /workspaces/{id}` — walks every CASCADEd binding and
//!   terminates its live sessions before committing.
//!
//! On any control-plane transport / 5xx / timeout the DB write is
//! rolled back and api returns 503 `unavailable`. This mirrors
//! session-broker's posture: no admin-driven mutation ever silently
//! disagrees with what envoy has.
//!
//! Endpoint: `BOTWORK_CONTROL_PLANE_ENDPOINT` (default
//! `http://control_plane:9300`). Break-glass: set
//! `BOTWORK_API_DISABLE_LIVE_GATE=1` to skip the coupling.
//! Not a supported production posture.
//!
//! # Trust posture (mirrors config-broker / control-plane)
//!
//! * No caller authentication in v0. Trust boundary is the docker
//!   network (`botwork-internal`).
//! * The service joins `botwork-internal` with alias `admin_api` and
//!   the listener port (`9400`) is NEVER `--publish`ed to the host.
//! * The eventual operator-facing exposure comes from the ingress
//!   envoy adding an `/admin/api/*` route in front of the existing
//!   `envoy.filters.http.ext_authz` seam; api itself stays
//!   credless and reads `x-botwork-tenant` (and, when the overlay
//!   adds it, `x-botwork-role`) verbatim from the request. The
//!   `x-botwork-admin` header is recorded in audit events.
//!
//! * Secrets endpoints follow the same posture as the rest:
//!   `x-botwork-tenant` is set by envoy ext_authz and trusted as the
//!   secret's scope; api does no further authz.

//! # Env contract
//!
//! * `BOTWORK_DATABASE_URL` (required) — postgres URL.
//! * `BOTWORK_API_BIND` (default: `0.0.0.0:9400`) — bind
//!   address. Internal-only; do not `--publish`.
//! * `BOTWORK_CONTROL_PLANE_ENDPOINT` (default
//!   `http://control_plane:9300`) — live-state ack target.
//! * `BOTWORK_API_DISABLE_LIVE_GATE` (default unset) —
//!   break-glass; bypasses control-plane coupling. Not for
//!   production use.
//! * `BOTWORK_SECRET_STORE_ENDPOINT` (default
//!   `http://secret_store:9500`) — secret-store backend endpoint.
//! * `BOTWORK_API_DISABLE_SECRET_STORE` (default unset) —
//!   break-glass; all secret writes return 503 immediately.
//! * `RUST_LOG` — standard `tracing-subscriber` filter; defaults to
//!   `info`.

pub mod control_plane;
pub mod handler;
pub mod read;
pub mod secret_store;
pub mod write;

pub use control_plane::ControlPlaneClient;
pub use handler::{build_router, AppState};
pub use secret_store::SecretStoreClient;
