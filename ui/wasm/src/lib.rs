// SPDX-License-Identifier: Apache-2.0

//! `botwork-ui-wasm` — Leptos CSR client for the operator-facing
//! admin panel.
//!
//! # PR3 shape (RFE #106)
//!
//! Builds on PR2's tenant scaffold by wiring the remaining five
//! entities end-to-end:
//!
//! * Workspaces — full CRUD, parent-tenant filter on list, dependents
//!   on delete (bindings + agent_sessions cascade).
//! * Plugins — full CRUD, JSON-textarea editors for `env` / `resources`
//!   / `egress`, dependents on delete (workspace_plugin RESTRICT).
//! * Bindings (workspace_plugin) — full CRUD on the composite-PK
//!   shape, live-state-gate handling on delete + update.
//! * Sessions (agent_session) — list + by-id, filter by state.
//! * Workers (session_worker) — list + by-id, filter by live status.
//!
//! Sessions + workers are READ-ONLY: session-broker owns the writes
//! and the UI surfaces only what's safe to expose through HTTP. See
//! the `sessions` / `workers` page docstrings for the per-entity
//! rationale.
//!
//! # Why CSR
//!
//! See `lib.rs` PR1 commit for the rationale. Unchanged.

use leptos::prelude::*;
use wasm_bindgen::prelude::*;

pub mod api;
pub mod layout;
pub mod pages;

/// URL prefix the ui SPA is mounted under.
///
/// **Three places must agree on this string:**
///
/// 1. `<Router base=…>` below in [`App`].
/// 2. `public_url = "/admin/"` in `wasm/Trunk.toml`.
/// 3. The ingress envoy `/admin/*` route (lives in `botworkz/space`).
///
/// **Why we spell it into every router-relative href.** `leptos_router`
/// *does not* prepend `<Router base>` to hrefs that start with `/` —
/// see `use_resolved_path` in the `leptos_router` source. With the
/// router mounted at `/admin`, a literal `<A href="/tenants">` renders
/// verbatim, the router's click handler sees a path that isn't inside
/// `/admin/*`, and the browser does a full-page navigation to
/// `/tenants` — which the ingress envoy treats as MCP traffic
/// (catch-all route → ext_authz → 401). The links appear dead.
///
/// Use the [`ui_path!`] macro at every router-relative href site;
/// it handles both the literal and `format!`-style flavours.
pub const UI_BASE: &str = "/admin";

/// Build a router-absolute URL for the ui SPA. See [`UI_BASE`]
/// for why every router-relative href has to be prefix-spelled.
///
/// Two flavours, mirroring `concat!` and `format!`:
///
/// * `ui_path!("/tenants/new")` → `&'static str =
///   "/admin/tenants/new"`. Evaluates at compile time; no per-render
///   allocation. Use this at `<A href=…>` sites.
/// * `ui_path!("/tenants/{}", id)` → `String =
///   "/admin/tenants/<id>"`. Evaluates at runtime; takes the same
///   format-args as `format!`. Use this for dynamic links and for
///   `use_navigate()` calls.
///
/// The `"/admin"` literal is intentionally duplicated against
/// [`UI_BASE`] here: `concat!` only accepts literal strings, not
/// `const &str`. The pair lives in the same module so the duplication
/// is trivial to audit if [`UI_BASE`] ever changes — and the
/// `ui_base_matches_macro` test in this crate fails loudly if they
/// drift.
#[macro_export]
macro_rules! ui_path {
    ($lit:literal) => {
        concat!("/admin", $lit)
    };
    ($fmt:literal, $($arg:tt)*) => {
        format!(concat!("/admin", $fmt), $($arg)*)
    };
}

/// Root component.
///
/// Mounts the router. Routes that exist in the route table but
/// haven't been built yet 404 cleanly via the catch-all; nothing
/// magic about "stub" pages — every entity is now real.
#[component]
pub fn App() -> impl IntoView {
    use leptos_router::components::{Route, Router, Routes};
    use leptos_router::path;

    view! {
        // `base` must match [`UI_BASE`]; see the docstring there for
        // why this string is repeated in four places (Router base,
        // UI_BASE const, ui_path! macro, Trunk.toml `public_url`).
        <Router base="/admin">
            <layout::Shell>
                <Routes fallback=|| view! { <pages::NotFound /> }>
                    <Route path=path!("/") view=pages::Dashboard />

                    // Tenants (PR2 carry-over).
                    <Route path=path!("/tenants") view=pages::tenants::List />
                    <Route path=path!("/tenants/new") view=pages::tenants::Create />
                    <Route path=path!("/tenants/:id") view=pages::tenants::Detail />
                    <Route path=path!("/tenants/:id/edit") view=pages::tenants::Edit />
                    <Route path=path!("/tenants/:id/delete") view=pages::tenants::DeleteConfirm />

                    // Workspaces.
                    <Route path=path!("/workspaces") view=pages::workspaces::List />
                    <Route path=path!("/workspaces/new") view=pages::workspaces::Create />
                    <Route path=path!("/workspaces/:id") view=pages::workspaces::Detail />
                    <Route path=path!("/workspaces/:id/edit") view=pages::workspaces::Edit />
                    <Route
                        path=path!("/workspaces/:id/delete")
                        view=pages::workspaces::DeleteConfirm
                    />

                    // Plugins.
                    <Route path=path!("/plugins") view=pages::plugins::List />
                    <Route path=path!("/plugins/new") view=pages::plugins::Create />
                    <Route path=path!("/plugins/:id") view=pages::plugins::Detail />
                    <Route path=path!("/plugins/:id/edit") view=pages::plugins::Edit />
                    <Route path=path!("/plugins/:id/delete") view=pages::plugins::DeleteConfirm />

                    // Bindings (workspace_plugin, composite PK).
                    <Route path=path!("/bindings") view=pages::bindings::List />
                    <Route path=path!("/bindings/new") view=pages::bindings::Create />
                    <Route
                        path=path!("/bindings/:workspace_id/:plugin_id")
                        view=pages::bindings::Detail
                    />
                    <Route
                        path=path!("/bindings/:workspace_id/:plugin_id/edit")
                        view=pages::bindings::Edit
                    />
                    <Route
                        path=path!("/bindings/:workspace_id/:plugin_id/delete")
                        view=pages::bindings::DeleteConfirm
                    />

                    // Sessions (read-only).
                    <Route path=path!("/sessions") view=pages::sessions::List />
                    <Route path=path!("/sessions/:id") view=pages::sessions::Detail />

                    // Workers (read-only).
                    <Route path=path!("/workers") view=pages::workers::List />
                    <Route path=path!("/workers/:id") view=pages::workers::Detail />
                </Routes>
            </layout::Shell>
        </Router>
    }
}

/// WASM entry point.
///
/// Marked `#[wasm_bindgen(start)]` so wasm-bindgen exports it as the
/// module start function and Trunk's generated JS loader invokes it
/// automatically.
#[wasm_bindgen(start)]
pub fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[cfg(test)]
mod tests {
    use super::UI_BASE;

    // Compile-time use: if `ui_path!` ever stopped returning a
    // `&'static str` for the literal form, this `const` binding would
    // fail to compile. That's the property `<A href=ui_path!(…)>`
    // sites rely on (no per-render allocation), so we pin it here.
    const _STATIC_NEW: &str = ui_path!("/tenants/new");
    const _STATIC_ROOT: &str = ui_path!("/");
    const _STATIC_EMPTY: &str = ui_path!("");

    #[test]
    fn literal_prepends_admin_prefix() {
        assert_eq!(ui_path!("/"), "/admin/");
        assert_eq!(ui_path!("/tenants"), "/admin/tenants");
        assert_eq!(ui_path!("/tenants/new"), "/admin/tenants/new");
    }

    #[test]
    fn format_prepends_admin_prefix() {
        let id = "abc-123";
        assert_eq!(ui_path!("/tenants/{}", id), "/admin/tenants/abc-123");
        assert_eq!(ui_path!("/bindings/{}/{}", "w", "p"), "/admin/bindings/w/p");
        assert_eq!(
            ui_path!("/workers?agent_session_id={}", id),
            "/admin/workers?agent_session_id=abc-123"
        );
    }

    #[test]
    fn ui_base_matches_macro() {
        // The two have to agree. `concat!("/admin", "")` is just
        // `"/admin"`, which is `UI_BASE`. If a future refactor moves
        // UI_BASE off `/admin`, the `ui_path!` macro body still has
        // to change in lockstep — this test fails when it doesn't.
        assert_eq!(UI_BASE, ui_path!(""));
    }
}
