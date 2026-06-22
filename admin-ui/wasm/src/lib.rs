// SPDX-License-Identifier: Apache-2.0

//! `botwork-admin-ui-wasm` — Leptos CSR client for the operator-facing
//! admin panel.
//!
//! # PR2 shape (RFE #106)
//!
//! Lays down the scaffolding that the rest of the admin UI builds on:
//!
//! * **Router** — `leptos_router` under `/admin/`. Five top-level
//!   nav targets (Dashboard / Tenants / Workspaces / Plugins /
//!   Bindings / Sessions / Workers) but only the first two are
//!   wired to real pages this round; the rest go to "coming soon"
//!   stubs that double as a TODO list.
//! * **HTTP client** — [`api::Client`] wraps `fetch` and decodes the
//!   admin-api wire envelopes (`{items, total}`, `{error, message,
//!   dependents?}`) into typed [`api::ApiError`] / [`api::ListResponse`]
//!   so handlers don't reach for `wasm-bindgen` directly.
//! * **Layout chrome** — left sidebar + main content slot via
//!   [`layout::Shell`], single source of truth for "every page
//!   lives inside this frame."
//! * **Tenant CRUD** — list / detail / create / delete-confirm,
//!   exercising every wire-contract subtlety the rest of the
//!   entities will share: the `{items, total}` envelope on list,
//!   `if_unmodified_since` lock token on update, `409 has_dependents`
//!   rendering on delete, the `x-botwork-admin` operator header
//!   on every mutation.
//!
//! Picking tenant for round 1 is deliberate: simplest schema, simplest
//! relations, every wire-contract quirk still applies. The next 5
//! entities will repeat the layout, which is when the abstraction
//! shape becomes obvious — that abstraction lands in PR3, after we
//! have one concrete instance in code to refactor toward.
//!
//! # Why CSR
//!
//! See `lib.rs` PR1 commit for the rationale. Unchanged here.

use leptos::prelude::*;
use wasm_bindgen::prelude::*;

pub mod api;
pub mod layout;
pub mod pages;

/// Root component.
///
/// Mounts the router. The router's [`leptos_router::components::Routes`]
/// renders the page matching the current path under
/// [`layout::Shell`]; the shell owns sidebar + topbar and slots the
/// matched page into its main area.
#[component]
pub fn App() -> impl IntoView {
    use leptos_router::components::{Route, Router, Routes};
    use leptos_router::path;

    view! {
        <Router base="/admin">
            <layout::Shell>
                <Routes fallback=|| view! {
                    <pages::NotFound />
                }>
                    <Route
                        path=path!("/")
                        view=pages::Dashboard
                    />
                    <Route
                        path=path!("/tenants")
                        view=pages::tenants::List
                    />
                    <Route
                        path=path!("/tenants/new")
                        view=pages::tenants::Create
                    />
                    <Route
                        path=path!("/tenants/:id")
                        view=pages::tenants::Detail
                    />
                    <Route
                        path=path!("/tenants/:id/edit")
                        view=pages::tenants::Edit
                    />
                    <Route
                        path=path!("/tenants/:id/delete")
                        view=pages::tenants::DeleteConfirm
                    />
                    <Route
                        path=path!("/workspaces")
                        view=pages::stub::workspaces
                    />
                    <Route
                        path=path!("/plugins")
                        view=pages::stub::plugins
                    />
                    <Route
                        path=path!("/bindings")
                        view=pages::stub::bindings
                    />
                    <Route
                        path=path!("/sessions")
                        view=pages::stub::sessions
                    />
                    <Route
                        path=path!("/workers")
                        view=pages::stub::workers
                    />
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
