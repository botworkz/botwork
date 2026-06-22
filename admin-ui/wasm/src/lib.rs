// SPDX-License-Identifier: Apache-2.0

//! `botwork-admin-ui-wasm` — Leptos CSR client for the operator-facing
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
