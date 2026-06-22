// SPDX-License-Identifier: Apache-2.0

//! Page components.
//!
//! Top-level pages are mounted by the [`crate::App`] router and each
//! one renders inside the layout [`crate::layout::Shell`]. PR3 wires
//! every entity end-to-end; there are no stub pages any more.

use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;

pub mod bindings;
pub mod plugins;
pub mod sessions;
pub mod tenants;
pub mod workers;
pub mod workspaces;

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
/// `Async` state. Used by every list/detail page; keeps the
/// "Loading…" / "Error: …" rendering uniform.
#[component]
pub fn AsyncView<T: Clone + Send + Sync + 'static, IV: IntoView + 'static>(
    /// The async signal to render against. Owned by the parent.
    state: ReadSignal<Async<T>>,
    /// Renderer for the loaded case.
    children: Box<dyn Fn(T) -> IV + Send + Sync>,
) -> impl IntoView {
    view! {
        {move || match state.get() {
            Async::Loading => view! { <p class="loading">"Loading…"</p> }.into_any(),
            Async::Loaded(v) => children(v).into_any(),
            Async::Failed(err) => view! {
                <p class="error">"Error: " {err.message().to_string()}</p>
            }.into_any(),
        }}
    }
}

/// Render an admin-api `dependents` payload from a `409 has_dependents`
/// error envelope. The wire shape is `[{kind, id, name}]` today; we
/// keep the renderer defensive so a future admin-api version that
/// adds fields doesn't break the UI.
///
/// Lifted out of `tenants.rs` so workspaces / plugins / bindings can
/// share it — every delete-confirm page renders the same shape.
pub fn render_dependents(deps: &serde_json::Value) -> AnyView {
    let arr = deps.as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        return view! { <p class="muted">"(no dependent details returned)"</p> }.into_any();
    }
    view! {
        <ul class="dependents">
            {arr.into_iter().map(|item| {
                let kind = item.get("kind").and_then(|v| v.as_str())
                    .unwrap_or("?").to_string();
                let name = item.get("name").and_then(|v| v.as_str())
                    .unwrap_or("?").to_string();
                let id = item.get("id").and_then(|v| v.as_str())
                    .unwrap_or("?").to_string();
                view! {
                    <li>
                        <strong>{kind}</strong>
                        " " {name} " "
                        <code class="muted">{id}</code>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
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

/// Dashboard — entity counts.
///
/// Counts are derived from the `total` field on each list endpoint's
/// envelope, so this page issues one GET per entity at mount. Cheap
/// today (4-figure rows max in any realistic deployment); the
/// `total` field is the natural seam for swapping in cached counts
/// later if needed.
#[component]
pub fn Dashboard() -> impl IntoView {
    let (tenants, set_tenants) = signal::<Async<usize>>(Async::Loading);
    let (workspaces, set_workspaces) = signal::<Async<usize>>(Async::Loading);
    let (plugins, set_plugins) = signal::<Async<usize>>(Async::Loading);
    let (bindings, set_bindings) = signal::<Async<usize>>(Async::Loading);
    let (sessions, set_sessions) = signal::<Async<usize>>(Async::Loading);
    let (workers, set_workers) = signal::<Async<usize>>(Async::Loading);

    Effect::new(move |_| {
        spawn_local(async move {
            set_tenants.set(match api::list_tenants().await {
                Ok(r) => Async::Loaded(r.total),
                Err(err) => Async::Failed(err),
            });
        });
        spawn_local(async move {
            set_workspaces.set(match api::list_workspaces(None).await {
                Ok(r) => Async::Loaded(r.total),
                Err(err) => Async::Failed(err),
            });
        });
        spawn_local(async move {
            set_plugins.set(match api::list_plugins().await {
                Ok(r) => Async::Loaded(r.total),
                Err(err) => Async::Failed(err),
            });
        });
        spawn_local(async move {
            set_bindings.set(match api::list_workspace_plugins(None, None).await {
                Ok(r) => Async::Loaded(r.total),
                Err(err) => Async::Failed(err),
            });
        });
        spawn_local(async move {
            set_sessions.set(match api::list_agent_sessions(None, None, None).await {
                Ok(r) => Async::Loaded(r.total),
                Err(err) => Async::Failed(err),
            });
        });
        spawn_local(async move {
            set_workers.set(match api::list_session_workers(None, None, None).await {
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
                            children=Box::new(|n| view! { <span>{n}</span> })
                        />
                    </dd>
                    <dt>"Workspaces"</dt>
                    <dd>
                        <AsyncView
                            state=workspaces
                            children=Box::new(|n| view! { <span>{n}</span> })
                        />
                    </dd>
                    <dt>"Plugins"</dt>
                    <dd>
                        <AsyncView
                            state=plugins
                            children=Box::new(|n| view! { <span>{n}</span> })
                        />
                    </dd>
                    <dt>"Bindings"</dt>
                    <dd>
                        <AsyncView
                            state=bindings
                            children=Box::new(|n| view! { <span>{n}</span> })
                        />
                    </dd>
                    <dt>"Sessions"</dt>
                    <dd>
                        <AsyncView
                            state=sessions
                            children=Box::new(|n| view! { <span>{n}</span> })
                        />
                    </dd>
                    <dt>"Workers"</dt>
                    <dd>
                        <AsyncView
                            state=workers
                            children=Box::new(|n| view! { <span>{n}</span> })
                        />
                    </dd>
                </dl>
            </section>
        </article>
    }
}
