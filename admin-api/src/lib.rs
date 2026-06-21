//! `botwork-admin-api` — HTTP+JSON CRUD service on top of `botwork-entity`.
//!
//! This crate is the v0 skeleton tracked by RFE #106. It ships:
//!
//! * a single `GET /admin/api/v1/health` endpoint that confirms the
//!   binary is up and (lazily) that the DB is reachable; and
//! * the systemd + container + image-loader wiring so the service
//!   shows up in the deployed VM stack.
//!
//! The actual entity CRUD handlers (`/tenants`, `/workspaces`,
//! `/plugins`, `/workspace_plugins`) land in PR2 once the validator
//! crate (`botwork-admin-core`) is extracted from `bootstrap/`.
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

pub use handler::{build_router, AppState};
