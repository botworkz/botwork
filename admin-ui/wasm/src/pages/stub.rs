// SPDX-License-Identifier: Apache-2.0

//! Stub pages for entities whose full CRUD lands in PR3 (RFE #106).
//!
//! Each function returns a "coming soon" placeholder that documents
//! which entity it stands in for. Keeping them as named exports
//! rather than a single `Stub(label)` component lets the router
//! macros use them by name (`view = stub::workspaces`), which keeps
//! the route table in `lib.rs` discoverable.

use leptos::prelude::*;

fn placeholder(label: &'static str, description: &'static str) -> impl IntoView {
    view! {
        <article class="page">
            <h1>{label}</h1>
            <p class="lede">{description}</p>
            <p class="muted">
                "Full CRUD lands in admin-ui PR3 (RFE #106). \
                 The admin-api surface for this entity is \
                 already shipped; this page is the next iteration's \
                 frontend work."
            </p>
        </article>
    }
}

pub fn workspaces() -> impl IntoView {
    placeholder(
        "Workspaces",
        "One workspace per tenant + plugin-binding scope. \
         Read+write surface lives at GET/POST/PUT/DELETE \
         /admin/api/v1/workspaces[/:id].",
    )
}

pub fn plugins() -> impl IntoView {
    placeholder(
        "Plugins",
        "Top-level plugin registry (image + port + path + \
         egress policy). Read+write surface at \
         /admin/api/v1/plugins[/:id].",
    )
}

pub fn bindings() -> impl IntoView {
    placeholder(
        "Bindings",
        "Workspace ↔ plugin associations with optional per-binding \
         config. Read+write surface at \
         /admin/api/v1/workspace_plugins[/:workspace_id/:plugin_id]. \
         Writes go through the live-state gate against control-plane.",
    )
}

pub fn sessions() -> impl IntoView {
    placeholder(
        "Sessions",
        "Read-only — session-broker owns writes. \
         Lists active and historical agent_session rows. \
         GET /admin/api/v1/agent_sessions[?state=…].",
    )
}

pub fn workers() -> impl IntoView {
    placeholder(
        "Workers",
        "Read-only — session-broker owns writes. \
         Lists live + reaped session_worker rows. \
         GET /admin/api/v1/session_workers[?live=…].",
    )
}
