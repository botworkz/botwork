// SPDX-License-Identifier: Apache-2.0

//! `botwork-admin-ui-wasm` — Leptos CSR client that drives the
//! operator-facing admin panel.
//!
//! # v0 surface
//!
//! A single page that polls `GET /admin/api/v1/health` on the
//! same origin and renders the response. This is intentionally
//! minimal: the goal of [RFE #106 PR1] is to nail down the
//! build + deploy story (trunk → static `dist/` → embedded in
//! `botwork-admin-ui-server` → distroless container behind the
//! same envoy as admin-api). Component decomposition + the actual
//! entity-CRUD UI lands later.
//!
//! # Why CSR
//!
//! The admin panel is operator-only, internal-network-only, and
//! authenticated by the ingress envoy via the same `ext_authz`
//! seam the rest of the broker stack uses. There is no SEO,
//! initial-paint, or shared-render concern that would justify
//! SSR / hydration, and CSR keeps the deploy shape symmetric
//! with the rest of the workspace (one static bundle, served by
//! one tiny binary, embedded at build time).
//!
//! # Entry point
//!
//! [`main`] is called by the Trunk-generated JS loader when the
//! WASM module is instantiated. It mounts [`App`] into `<body>`.
//!
//! [RFE #106 PR1]: https://github.com/botworkz/botwork/issues/106

use leptos::prelude::*;
use serde::Deserialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, Response};

/// Shape of `GET /admin/api/v1/health`.
///
/// Mirrors `botwork_admin_api::handler::HealthResponse` on the
/// server. Kept as a hand-rolled `serde::Deserialize` because the
/// wasm crate must not depend on the server crate (the server
/// crate pulls in sea-orm, tokio multi-thread, etc. — none of
/// which compile to `wasm32-unknown-unknown`).
#[derive(Debug, Clone, Deserialize)]
struct HealthResponse {
    status: String,
    db: String,
    #[serde(default)]
    message: Option<String>,
}

/// Possible states for the health probe, surfaced in the UI.
#[derive(Debug, Clone)]
enum HealthState {
    /// First load — no fetch attempted yet.
    Idle,
    /// Fetch in flight.
    Loading,
    /// Got a 2xx with a parseable body.
    Loaded(HealthResponse),
    /// Either transport failure, non-2xx, or JSON parse failure.
    /// The string is operator-facing detail, not machine-readable.
    Failed(String),
}

/// Issue `GET /admin/api/v1/health` on the same origin as the
/// page and parse the response.
///
/// The path is hardcoded because the admin-ui bundle is served
/// from `/admin/` by `botwork-admin-ui-server`, and the admin-api
/// surface lives at `/admin/api/v1/*`. Same-origin by construction,
/// so no CORS / credentials plumbing.
async fn fetch_health() -> Result<HealthResponse, String> {
    let window = web_sys::window().ok_or_else(|| "no window".to_string())?;
    let opts = RequestInit::new();
    opts.set_method("GET");
    let request = Request::new_with_str_and_init("/admin/api/v1/health", &opts)
        .map_err(|err| format!("build request: {err:?}"))?;
    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|err| format!("fetch: {err:?}"))?;
    let resp: Response = resp_value
        .dyn_into()
        .map_err(|_| "response is not a Response".to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let json_promise = resp
        .json()
        .map_err(|err| format!("response.json(): {err:?}"))?;
    let json = JsFuture::from(json_promise)
        .await
        .map_err(|err| format!("await json: {err:?}"))?;
    serde_wasm_bindgen_compat::from_value(json).map_err(|err| format!("parse: {err}"))
}

/// Minimal stand-in for `serde_wasm_bindgen::from_value` that
/// avoids pulling in the full crate for one call site. It stringifies
/// the `JsValue` via `JSON.stringify` and then parses it via
/// `serde_json`. Slightly wasteful but keeps the wasm bundle small.
mod serde_wasm_bindgen_compat {
    use serde::de::DeserializeOwned;
    use wasm_bindgen::JsValue;

    pub fn from_value<T: DeserializeOwned>(value: JsValue) -> Result<T, String> {
        let s = js_sys::JSON::stringify(&value)
            .map_err(|err| format!("JSON.stringify: {err:?}"))?
            .as_string()
            .ok_or_else(|| "stringify result was not a string".to_string())?;
        serde_json::from_str(&s).map_err(|err| err.to_string())
    }
}

/// Root component.
///
/// Owns a single signal — `state: RwSignal<HealthState>` — and a
/// `<button>` that re-issues the fetch. On mount the component
/// fires the first fetch so the page shows live data instead of
/// the `Idle` placeholder.
#[component]
pub fn App() -> impl IntoView {
    let state: RwSignal<HealthState> = RwSignal::new(HealthState::Idle);

    // Closure used both by the mount-time effect and the refresh
    // button. Captures the signal by `Copy` (Leptos signals are
    // `Copy`, no Arc needed).
    let refresh = move || {
        state.set(HealthState::Loading);
        wasm_bindgen_futures::spawn_local(async move {
            let next = match fetch_health().await {
                Ok(body) => HealthState::Loaded(body),
                Err(err) => HealthState::Failed(err),
            };
            state.set(next);
        });
    };

    // Kick off the first fetch as soon as the component mounts.
    // `Effect::new` runs once on mount in CSR.
    Effect::new(move |_| {
        refresh();
    });

    view! {
        <main class="admin-ui-root">
            <header>
                <h1>"botwork admin"</h1>
                <p class="subtitle">
                    "v0 — health probe only. Entity CRUD lands in PR2 \
                     (RFE #106)."
                </p>
            </header>

            <section class="health-card">
                <h2>"admin-api health"</h2>
                {move || match state.get() {
                    HealthState::Idle => view! {
                        <p class="health-idle">"Not yet probed."</p>
                    }.into_any(),
                    HealthState::Loading => view! {
                        <p class="health-loading">"Probing /admin/api/v1/health…"</p>
                    }.into_any(),
                    HealthState::Loaded(body) => {
                        let db_class = if body.db == "reachable" {
                            "health-db-ok"
                        } else {
                            "health-db-bad"
                        };
                        view! {
                            <dl class="health-detail">
                                <dt>"status"</dt>
                                <dd>{body.status.clone()}</dd>
                                <dt>"db"</dt>
                                <dd class=db_class>{body.db.clone()}</dd>
                                {body.message.clone().map(|m| view! {
                                    <dt>"message"</dt>
                                    <dd class="health-message">{m}</dd>
                                })}
                            </dl>
                        }.into_any()
                    }
                    HealthState::Failed(err) => view! {
                        <p class="health-failed">
                            "Health probe failed: " {err}
                        </p>
                    }.into_any(),
                }}
                <button
                    class="refresh"
                    on:click=move |_| refresh()
                >
                    "Refresh"
                </button>
            </section>
        </main>
    }
}

/// WASM entry point.
///
/// Marked `#[wasm_bindgen(start)]` so wasm-bindgen exports it as
/// the module start function and Trunk's generated JS loader
/// invokes it automatically.
#[wasm_bindgen(start)]
pub fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
