// SPDX-License-Identifier: Apache-2.0

//! Layout chrome — sidebar + main content slot.
//!
//! Every page renders inside [`Shell`], which owns the left sidebar
//! (entity nav links) and the main `<section>` the router-matched
//! page mounts into. This keeps "the frame" out of every page's
//! source and ensures sidebar state stays consistent across
//! navigation.
//!
//! Styling is deferred to a future round (per RFE #106 PR2 scope
//! agreement): the markup uses semantic tags + class names so a
//! later round can drop in a stylesheet without touching component
//! source.

use leptos::prelude::*;
use leptos_router::components::A;

use crate::ui_path;

/// Render a labelled sidebar entry. We use a component rather than
/// inlining `<A>` everywhere so the "active" highlighting (a TODO
/// for the styling pass) lands in one place.
#[component]
fn NavLink(
    /// Absolute path the router will use as the rendered `href`.
    /// Must already be UI-base-prefixed — pass values from
    /// [`crate::ui_path!`]. `leptos_router` does NOT prepend
    /// `<Router base>` for hrefs that begin with `/`, so the prefix
    /// is the caller's responsibility (see [`crate::UI_BASE`]).
    href: &'static str,
    /// Operator-facing label.
    label: &'static str,
) -> impl IntoView {
    view! {
        <li>
            <A href=href>{label}</A>
        </li>
    }
}

/// Layout shell. Receives the matched page as children and renders
/// the sidebar + main grid around it.
///
/// The order of nav entries follows the operator mental model:
/// configuration entities (top, in dependency order) then runtime
/// entities (bottom, read-only browse).
#[component]
pub fn Shell(children: Children) -> impl IntoView {
    view! {
        <div class="admin-shell">
            <aside class="admin-sidebar">
                <header class="admin-sidebar-header">
                    <h1>"botwork"</h1>
                    <p class="subtitle">"admin"</p>
                </header>
                <nav>
                    <ul>
                        <NavLink href=ui_path!("/") label="Dashboard" />
                    </ul>
                    <h2>"Config"</h2>
                    <ul>
                        <NavLink href=ui_path!("/tenants") label="Tenants" />
                        <NavLink href=ui_path!("/workspaces") label="Workspaces" />
                        <NavLink href=ui_path!("/plugins") label="Plugins" />
                        <NavLink href=ui_path!("/bindings") label="Bindings" />
                    </ul>
                    <h2>"Runtime"</h2>
                    <ul>
                        <NavLink href=ui_path!("/sessions") label="Sessions" />
                        <NavLink href=ui_path!("/workers") label="Workers" />
                    </ul>
                </nav>
                <footer class="admin-sidebar-footer">
                    <p class="operator">"operator: " <code>{crate::api::OPERATOR}</code></p>
                </footer>
            </aside>
            <main class="admin-main">
                {children()}
            </main>
        </div>
    }
}
