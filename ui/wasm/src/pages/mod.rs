// SPDX-License-Identifier: Apache-2.0

//! Page components.
//!
//! Top-level pages are mounted by the [`crate::App`] router and each
//! one renders inside the layout [`crate::layout::Shell`]. PR3 wires
//! every entity end-to-end; there are no stub pages any more.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_shadcn_alert::{Alert, AlertDescription, AlertVariant};
use leptos_shadcn_card::{Card, CardContent, CardHeader, CardTitle};
use leptos_shadcn_skeleton::Skeleton;
use leptos_shadcn_table::Table;

use crate::api;

use leptos_router::hooks::use_params_map;

pub mod bindings;
pub mod login;
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
            Async::Loading => view! {
                <div class="py-2">
                    <Skeleton class="h-8 w-full" />
                </div>
            }.into_any(),
            Async::Loaded(v) => children(v).into_any(),
            Async::Failed(err) => view! {
                <Alert variant=AlertVariant::Destructive>
                    <AlertDescription>{format!("Error: {}", err.message())}</AlertDescription>
                </Alert>
            }.into_any(),
        }}
    }
}

/// Render an api `dependents` payload from a `409 has_dependents`
/// error envelope. The wire shape is `[{kind, id, name}]` today; we
/// keep the renderer defensive so a future api version that
/// adds fields doesn't break the UI.
///
/// Lifted out of `tenants.rs` so workspaces / plugins / bindings can
/// share it — every delete-confirm page renders the same shape.
pub fn render_dependents(deps: &serde_json::Value) -> AnyView {
    let arr = deps.as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        return view! { <p class="text-sm text-muted-foreground">"(no dependent details returned)"</p> }
            .into_any();
    }
    view! {
        <ul class="space-y-2 rounded-md border border-border bg-muted/40 p-3 text-sm">
            {arr.into_iter().map(|item| {
                let kind = item.get("kind").and_then(|v| v.as_str())
                    .unwrap_or("?").to_string();
                let name = item.get("name").and_then(|v| v.as_str())
                    .unwrap_or("?").to_string();
                let id = item.get("id").and_then(|v| v.as_str())
                    .unwrap_or("?").to_string();
                view! {
                    <li class="flex flex-wrap gap-2">
                        <strong>{kind}</strong>
                        " " {name} " "
                        <code class="text-muted-foreground">{id}</code>
                    </li>
                }
            }).collect_view()}
        </ul>
    }
    .into_any()
}

/// Re-exported from [`login`] for routing ergonomics.
pub use login::Login;

/// Catch-all 404 — rendered when the router has no match for the
/// current path.
#[component]
pub fn NotFound() -> impl IntoView {
    view! {
        <article class="space-y-3">
            <h1 class="text-2xl font-semibold tracking-tight">"Not found"</h1>
            <p class="text-muted-foreground">"The page you requested does not exist."</p>
            <p>
                <a class="text-primary hover:underline" href={crate::ui_path!("/")}>"Back to dashboard"</a>
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
    let params = use_params_map();
    let tenant = move || params.with(|p| p.get("tenant").unwrap_or_default().to_string());

    let (workspaces, set_workspaces) = signal::<Async<usize>>(Async::Loading);
    let (plugins, set_plugins) = signal::<Async<usize>>(Async::Loading);
    let (bindings, set_bindings) = signal::<Async<usize>>(Async::Loading);
    let (sessions, set_sessions) = signal::<Async<usize>>(Async::Loading);
    let (workers, set_workers) = signal::<Async<usize>>(Async::Loading);

    Effect::new(move |_| {
        let t = tenant();
        let t2 = t.clone();
        let t3 = t.clone();
        let t4 = t.clone();
        spawn_local(async move {
            set_workspaces.set(match api::list_workspaces(&t).await {
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
            set_bindings.set(match api::list_workspace_plugins(&t2, None, None).await {
                Ok(r) => Async::Loaded(r.total),
                Err(err) => Async::Failed(err),
            });
        });
        spawn_local(async move {
            set_sessions.set(match api::list_agent_sessions(&t3, None, None).await {
                Ok(r) => Async::Loaded(r.total),
                Err(err) => Async::Failed(err),
            });
        });
        spawn_local(async move {
            set_workers.set(match api::list_session_workers(&t4, None, None, None).await {
                Ok(r) => Async::Loaded(r.total),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="space-y-6">
            <h1 class="text-3xl font-semibold tracking-tight">"Dashboard"</h1>
            <p class="text-muted-foreground">
                "Tenant: " <strong>{move || tenant()}</strong>
                " — entity counts for this tenant."
            </p>

            <Card>
                <CardHeader>
                    <CardTitle>"Entity counts"</CardTitle>
                </CardHeader>
                <CardContent>
                    <dl class="grid grid-cols-[1fr_auto] gap-y-3 text-sm">
                        <dt class="text-muted-foreground">"Workspaces"</dt>
                        <dd><AsyncView state=workspaces children=Box::new(|n| view! { <span>{n}</span> }) /></dd>
                        <dt class="text-muted-foreground">"Plugins"</dt>
                        <dd><AsyncView state=plugins children=Box::new(|n| view! { <span>{n}</span> }) /></dd>
                        <dt class="text-muted-foreground">"Bindings"</dt>
                        <dd><AsyncView state=bindings children=Box::new(|n| view! { <span>{n}</span> }) /></dd>
                        <dt class="text-muted-foreground">"Sessions"</dt>
                        <dd><AsyncView state=sessions children=Box::new(|n| view! { <span>{n}</span> }) /></dd>
                        <dt class="text-muted-foreground">"Workers"</dt>
                        <dd><AsyncView state=workers children=Box::new(|n| view! { <span>{n}</span> }) /></dd>
                    </dl>
                </CardContent>
            </Card>
        </article>
    }
}

#[component]
pub fn PageTable(children: Children) -> impl IntoView {
    view! {
        <Table class="overflow-x-auto">
            <table class="w-full caption-bottom text-sm">{children()}</table>
        </Table>
    }
}
