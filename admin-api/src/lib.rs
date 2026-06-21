//! `botwork-admin-api` — HTTP+JSON CRUD service on top of `botwork-entity`.
//!
//! This crate is the operator-facing writer over the v0 schema. It is
//! the eventual replacement for `botwork-bootstrap`; for now both
//! live side-by-side and share the per-entry validators in
//! `botwork-admin-core`.
//!
//! # What v0 ships (post-RFE #106 PR2)
//!
//! Read-only handlers over all four entities:
//!
//! ```text
//! GET /admin/api/v1/health                            -> { status, db }
//!
//! GET /admin/api/v1/tenants                           -> { items: [...], total }
//! GET /admin/api/v1/tenants/{id}                      -> Tenant
//!
//! GET /admin/api/v1/workspaces                        -> { items: [...], total }
//!     ?tenant_id=<uuid>                                  (optional filter)
//! GET /admin/api/v1/workspaces/{id}                   -> Workspace
//!
//! GET /admin/api/v1/plugins                           -> { items: [...], total }
//! GET /admin/api/v1/plugins/{id}                      -> Plugin
//!
//! GET /admin/api/v1/workspace_plugins                 -> { items: [...], total }
//!     ?workspace_id=<uuid>&plugin_id=<uuid>              (optional filters)
//! GET /admin/api/v1/workspace_plugins/{wid}/{pid}     -> WorkspacePlugin
//! ```
//!
//! Write endpoints (POST/PUT/DELETE per entity), delete-guard
//! preflights, optimistic locking via `updated_at`, and the xDS gate
//! against control-plane on binding mutations all land in RFE #106
//! PR3.
//!
//! # Response shapes
//!
//! * **Success body** — entity model serialised verbatim via SeaORM's
//!   derived `Serialize`. List endpoints wrap in
//!   `{ "items": [...], "total": N }` so pagination can land later
//!   without a breaking change.
//! * **Error envelope** (mirrors config-broker / control-plane):
//!
//!   ```json
//!   { "error": "<machine code>", "message": "<human detail>" }
//!   ```
//!
//!   v0 emits: `not_found`, `bad_request`, `internal` — see
//!   [`handler::ErrorBody`] for the contract.
//!
//! # Trust posture (mirrors config-broker / control-plane)
//!
//! * No caller authentication in v0. Trust boundary is the docker
//!   network (`botwork-internal`).
//! * The service joins `botwork-internal` with alias `admin_api` and
//!   the listener port (`9400`) is NEVER `--publish`ed to the host.
//! * The eventual operator-facing exposure comes from the ingress
//!   envoy adding an `/admin/api/*` route in front of the existing
//!   `envoy.filters.http.ext_authz` seam; admin-api itself stays
//!   credless and reads `x-botwork-tenant` (and, when the overlay
//!   adds it, `x-botwork-role`) verbatim from the request.
//!
//! # Env contract
//!
//! * `BOTWORK_DATABASE_URL` (required) — postgres URL in the canonical
//!   `postgres://botwork:<password>@postgres/botwork` shape. Same env
//!   the rest of the persistence-aware consumers use.
//! * `BOTWORK_ADMIN_API_BIND` (default: `0.0.0.0:9400`) — bind
//!   address. The port is **internal-only**; the default matches
//!   the workspace numbering convention (config-broker=9200,
//!   control-plane=9300/9301, admin-api=9400) so `docker run` from
//!   inside the same network reaches us via the `admin_api:9400`
//!   alias.
//! * `RUST_LOG` — standard `tracing-subscriber` filter; defaults to
//!   `info`.

pub mod handler;
pub mod read;

pub use handler::{build_router, AppState};
