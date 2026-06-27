//! `botwork-ui-server` — distroless HTTP server that serves
//! the compiled `botwork-ui-wasm` bundle.
//!
//! # Why a server at all
//!
//! The Leptos client is CSR (client-side rendering). Static bytes
//! plus a WASM module are all that has to reach the browser, so
//! "running a server" is overkill *in the abstract*. We do it anyway
//! for two reasons:
//!
//! 1. **Symmetric deployment shape.** Every other botwork component
//!    is a distroless container on `botwork-internal` with a systemd
//!    unit and a goss assertion. Wrapping the bundle in a tiny axum
//!    binary lets ui slot in next to api with no new
//!    operational patterns. envoy already speaks HTTP to the other
//!    brokers; pointing it at a sibling that also speaks HTTP keeps
//!    the listener config uniform.
//! 2. **Hermetic image.** `include_dir!` pulls `ui/wasm/dist/`
//!    into the binary at compile time. The runtime container is the
//!    same distroless image as the brokers; there is no `volume`,
//!    no `chown`, no asset directory to keep in sync. Roll forward
//!    by replacing the image.
//!
//! # Trust posture (mirrors api / config-broker)
//!
//! * No caller authentication in v0. Trust boundary is the docker
//!   network (`botwork-internal`). The container joins the network
//!   with alias `admin_ui` and the bind port is NEVER `--publish`'d.
//! * Operator-facing exposure comes from the ingress envoy adding
//!   `/admin/*` (UI bundle) and `/admin/api/*` (JSON) routes in
//!   front of the existing `envoy.filters.http.ext_authz` seam.
//!   ui itself is credless.
//!
//! # Routes (v0)
//!
//! * `GET /healthz` — liveness probe for goss + systemd. Returns
//!   `{ "status": "ok" }`. No filesystem touch; literally a constant.
//! * `GET /admin/` and `GET /admin/index.html` — serve the trunk
//!   `index.html` (the SPA shell).
//! * `GET /admin/*path` — serve any other file from the embedded
//!   `dist/`, with `Content-Type` guessed from the extension. Falls
//!   back to `index.html` for unknown paths so client-side router
//!   deep links work (`/admin/tenants/abc` reloads → SPA handles
//!   the route).
//!
//! Paths matching `/admin/api/*` are NOT served here; in production
//! the ingress envoy routes them to `admin_api:9400` before they
//! ever reach this binary. In the dev loop the trunk dev server
//! proxies them in the same direction (see `Trunk.toml`).
//!
//! # Env contract
//!
//! * `BOTWORK_UI_BIND` (default: `0.0.0.0:9500`) — bind
//!   address. The port is **internal-only**; the default follows
//!   the workspace numbering convention (config-broker=9200,
//!   control-plane=9300/9301, api=9400, ui=9500).
//! * `RUST_LOG` — standard `tracing-subscriber` filter; defaults to
//!   `info`.

pub mod handler;

pub use handler::build_router;
