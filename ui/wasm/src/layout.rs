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
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map};

/// Layout shell. Receives the matched page as children and renders
/// the sidebar + main grid around it.
///
/// Reads the current `tenant` route param via [`use_params_map`] so
/// that sidebar links are always scoped to the active tenant.
///
/// The order of nav entries follows the operator mental model:
/// configuration entities (top, in dependency order) then runtime
/// entities (bottom, read-only browse).
#[component]
pub fn Shell(children: Children) -> impl IntoView {
    let params = use_params_map();
    let tenant = move || params.with(|p| p.get("tenant").unwrap_or_default().to_string());

    let nav = use_navigate();
    let on_logout = move |_| {
        let nav = nav.clone();
        spawn_local(async move {
            // Best-effort — navigate to /login regardless of result.
            let _ = crate::api::logout().await;
            nav("/login", Default::default());
        });
    };

    view! {
        <div class="grid min-h-screen grid-cols-1 bg-background text-foreground md:grid-cols-[16rem_1fr]">
            <aside class="border-b border-border bg-card/40 md:border-b-0 md:border-r">
                <header class="px-6 py-6">
                    <h1 class="text-xl font-semibold tracking-tight">"botwork"</h1>
                    <p class="text-sm text-muted-foreground">{move || tenant()}</p>
                </header>
                <nav class="space-y-4 px-4 pb-4">
                    <ul class="space-y-1">
                        <li>
                            <A
                                href=move || format!("/{}/", tenant())
                                attr:class="block rounded-md px-3 py-2 text-sm text-muted-foreground transition hover:bg-accent hover:text-accent-foreground aria-[current=page]:bg-secondary aria-[current=page]:text-foreground"
                            >
                                "Dashboard"
                            </A>
                        </li>
                    </ul>
                    <h2 class="px-3 pt-2 text-xs font-semibold uppercase tracking-wider text-muted-foreground">"Config"</h2>
                    <ul class="space-y-1">
                        <li>
                            <A
                                href=move || format!("/{}/workspaces", tenant())
                                attr:class="block rounded-md px-3 py-2 text-sm text-muted-foreground transition hover:bg-accent hover:text-accent-foreground aria-[current=page]:bg-secondary aria-[current=page]:text-foreground"
                            >
                                "Workspaces"
                            </A>
                        </li>
                        <li>
                            <A
                                href=move || format!("/{}/plugins", tenant())
                                attr:class="block rounded-md px-3 py-2 text-sm text-muted-foreground transition hover:bg-accent hover:text-accent-foreground aria-[current=page]:bg-secondary aria-[current=page]:text-foreground"
                            >
                                "Plugins"
                            </A>
                        </li>
                        <li>
                            <A
                                href=move || format!("/{}/bindings", tenant())
                                attr:class="block rounded-md px-3 py-2 text-sm text-muted-foreground transition hover:bg-accent hover:text-accent-foreground aria-[current=page]:bg-secondary aria-[current=page]:text-foreground"
                            >
                                "Bindings"
                            </A>
                        </li>
                    </ul>
                    <h2 class="px-3 pt-2 text-xs font-semibold uppercase tracking-wider text-muted-foreground">"Runtime"</h2>
                    <ul class="space-y-1">
                        <li>
                            <A
                                href=move || format!("/{}/sessions", tenant())
                                attr:class="block rounded-md px-3 py-2 text-sm text-muted-foreground transition hover:bg-accent hover:text-accent-foreground aria-[current=page]:bg-secondary aria-[current=page]:text-foreground"
                            >
                                "Sessions"
                            </A>
                        </li>
                        <li>
                            <A
                                href=move || format!("/{}/workers", tenant())
                                attr:class="block rounded-md px-3 py-2 text-sm text-muted-foreground transition hover:bg-accent hover:text-accent-foreground aria-[current=page]:bg-secondary aria-[current=page]:text-foreground"
                            >
                                "Workers"
                            </A>
                        </li>
                    </ul>
                </nav>
                <footer class="flex items-center justify-between px-6 pb-6 text-xs text-muted-foreground">
                    <p>"tenant: " <code>{move || tenant()}</code></p>
                    <button
                        on:click=on_logout
                        class="rounded px-2 py-1 hover:bg-accent hover:text-accent-foreground"
                    >
                        "Sign out"
                    </button>
                </footer>
            </aside>
            <main class="p-6 md:p-8">
                {children()}
            </main>
        </div>
    }
}
