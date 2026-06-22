// SPDX-License-Identifier: Apache-2.0

//! Page components.
//!
//! Top-level pages are mounted by the [`crate::App`] router and each
//! one renders inside the layout [`crate::layout::Shell`]. v0 wires
//! `Dashboard`, the full `tenants::*` flow, and stubbed pages for
//! the rest of the entities (so the sidebar links go somewhere
//! recognisable).

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;

pub mod stub;
pub mod tenants;

/// Generic loading / error states. Pulled out so every list/detail
/// page renders failures uniformly without duplicating match arms.
#[derive(Debug, Clone)]
pub enum Async<T> {
    /// Fetch in flight or not yet attempted.
    Loading,
    /// Resolved successfully.
    Loaded(T),
    /// Resolved with an error. Carries the wire-typed error so
    /// specific renderers (delete-confirm) can pull out
    /// `HasDependents` etc.
    Failed(api::ApiError),
}

/// Render either children or a placeholder, depending on the
/// `Async` state. Used by each list/detail page; keeps the
/// "Loading…" / "Error: …" rendering uniform.
///
/// The `children` closure returns [`AnyView`] rather than a generic
/// `IntoView` because the type inference on the latter trips on
/// complex view trees (forms with event handlers, in particular).
/// Callers are expected to `.into_any()` their `view!` output.
#[component]
pub fn AsyncView<T: Clone + Send + Sync + 'static>(
    /// The async signal to render against. Owned by the parent.
    state: ReadSignal<Async<T>>,
    /// Renderer for the loaded case.
    children: Box<dyn Fn(T) -> AnyView + Send + Sync>,
) -> impl IntoView {
    view! {
        {move || match state.get() {
            Async::Loading => view! { <p class="loading">"Loading…"</p> }.into_any(),
            Async::Loaded(v) => children(v),
            Async::Failed(err) => view! {
                <p class="error">"Error: " {err.message().to_string()}</p>
            }.into_any(),
        }}
    }
}

/// Catch-all 404 — rendered when the router has no match for the
/// current path.
#[component]
pub fn NotFound() -> impl IntoView {
    view! {
        <article class="page">
            <h1>"Not found"</h1>
            <p>"The page you requested does not exist."</p>
            <p>
                <a href="/admin/">"Back to dashboard"</a>
            </p>
        </article>
    }
}

/// Dashboard — health card + per-entity counts.
///
/// Counts are derived from the `total` field on each list endpoint's
/// envelope, so this page issues one GET per entity at mount. Cheap
/// today (4-figure rows max in any realistic deployment); the
/// `total` field is the natural seam for swapping in cached counts
/// later if needed.
#[component]
pub fn Dashboard() -> impl IntoView {
    let (tenants, set_tenants) = signal::<Async<usize>>(Async::Loading);

    Effect::new(move |_| {
        spawn_local(async move {
            set_tenants.set(match api::list_tenants().await {
                Ok(r) => Async::Loaded(r.total),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="page">
            <h1>"Dashboard"</h1>
            <p class="lede">
                "Operator-facing view of the botwork stack. \
                 Sidebar links each entity; this page rolls up \
                 the headline counts."
            </p>

            <section class="counts">
                <h2>"Entity counts"</h2>
                <dl>
                    <dt>"Tenants"</dt>
                    <dd>
                        <AsyncView
                            state=tenants
                            children=Box::new(|n: usize| {
                                view! { <span>{n}</span> }.into_any()
                            })
                        />
                    </dd>
                    <dt>"Workspaces"</dt>
                    <dd><span class="muted">"(PR3)"</span></dd>
                    <dt>"Plugins"</dt>
                    <dd><span class="muted">"(PR3)"</span></dd>
                    <dt>"Bindings"</dt>
                    <dd><span class="muted">"(PR3)"</span></dd>
                    <dt>"Sessions"</dt>
                    <dd><span class="muted">"(PR3)"</span></dd>
                    <dt>"Workers"</dt>
                    <dd><span class="muted">"(PR3)"</span></dd>
                </dl>
            </section>
        </article>
    }
}
