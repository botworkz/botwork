// SPDX-License-Identifier: Apache-2.0

//! `botwork-ui-wasm` — Leptos CSR client for the operator-facing
//! admin panel.
//!
//! # Phase 2 shape (botworkz/space#311)
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

/// URL prefix the ui SPA is mounted under (Phase 2 reshape).
///
/// After botworkz/space#311 Phase 2, the SPA moves from `/admin/*` to
/// `/{tenant}/*`. The tenant is a first-class route param rather than
/// a global; the login page lives at `/login`.
///
/// **Three places must agree:**
///
/// 1. `<Router base=…>` below in [`App`].
/// 2. `public_url` in `wasm/Trunk.toml` (must match server route root).
/// 3. The ingress envoy route table (lives in `botworkz/space`).
///
/// `UI_BASE` is empty (`""`) because the SPA now mounts at the root
/// `/` and uses `/{tenant}/…` sub-paths rather than a fixed prefix.
/// The `/login` path is outside any tenant prefix and is served
/// directly.
///
/// See [`ui_path!`] for constructing paths.
pub const UI_BASE: &str = "";

/// Build a router-absolute URL for the ui SPA. See [`UI_BASE`]
/// for the rationale.
///
/// Two flavours:
///
/// * `ui_path!("/login")` → `&'static str = "/login"`. Compile-time.
/// * `ui_path!("/{}", tenant)` → `String = "/<tenant>"`. Runtime.
#[macro_export]
macro_rules! ui_path {
    ($lit:literal) => {
        concat!("", $lit)
    };
    ($fmt:literal, $($arg:tt)*) => {
        format!(concat!("", $fmt), $($arg)*)
    };
}

/// Root component.
///
/// Mounts the router. The SPA is rooted at `/` (Phase 2 reshape).
///
/// Route structure:
///
/// * `/login` — login page. Renders without the tenant shell.
/// * `/{tenant}/*` — all tenant-scoped pages rendered inside [`layout::Shell`].
///
/// The tenant shell (sidebar + header) is a nested layout scoped to
/// `/:tenant` routes; `/login` sits outside and renders full-screen.
#[component]
pub fn App() -> impl IntoView {
    use leptos_router::components::{Outlet, ParentRoute, Route, Router, Routes};
    use leptos_router::path;

    view! {
        // Router mounts at root — no `/admin` prefix in Phase 2.
        // UI_BASE is "" — see the docstring there for the rationale.
        <Router base="">
            <Routes fallback=|| view! { <pages::NotFound /> }>
                // Login page — renders full-screen, outside the tenant shell.
                <Route path=path!("/login") view=pages::Login />

                // Tenant-scoped routes: /{tenant}/...
                // Shell wraps all children via Outlet; tenant is a route param.
                <ParentRoute
                    path=path!("/:tenant")
                    view=|| view! { <layout::Shell><Outlet /></layout::Shell> }
                >
                    <Route path=path!("") view=pages::Dashboard />

                    <Route path=path!("/workspaces") view=pages::workspaces::List />
                    <Route path=path!("/workspaces/new") view=pages::workspaces::Create />
                    <Route path=path!("/workspaces/:id") view=pages::workspaces::Detail />
                    <Route path=path!("/workspaces/:id/edit") view=pages::workspaces::Edit />
                    <Route
                        path=path!("/workspaces/:id/delete")
                        view=pages::workspaces::DeleteConfirm
                    />

                    // Plugins (global, admin-managed).
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
                </ParentRoute>
            </Routes>
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
    const _STATIC_NEW: &str = ui_path!("/workspaces/new");
    const _STATIC_ROOT: &str = ui_path!("/");
    const _STATIC_EMPTY: &str = ui_path!("");

    #[test]
    fn literal_prepends_admin_prefix() {
        assert_eq!(ui_path!("/"), "/");
        assert_eq!(ui_path!("/workspaces"), "/workspaces");
        assert_eq!(ui_path!("/workspaces/new"), "/workspaces/new");
    }

    #[test]
    fn format_prepends_admin_prefix() {
        let id = "abc-123";
        assert_eq!(ui_path!("/workspaces/{}", id), "/workspaces/abc-123");
        assert_eq!(ui_path!("/bindings/{}/{}", "w", "p"), "/bindings/w/p");
        assert_eq!(
            ui_path!("/workers?agent_session_id={}", id),
            "/workers?agent_session_id=abc-123"
        );
    }

    #[test]
    fn ui_base_matches_macro() {
        // UI_BASE is "" and ui_path!("") is concat!("", "") = "".
        // Both must stay in sync — if UI_BASE changes, the macro body
        // changes with it.
        assert_eq!(UI_BASE, ui_path!(""));
    }
}
