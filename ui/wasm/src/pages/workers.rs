// SPDX-License-Identifier: Apache-2.0

//! session_worker read-only pages.
//!
//! Same read-only posture as `sessions` for the same reason: writes
//! come from session-broker. v0 lets the operator filter live workers
//! (reaped_at IS NULL) and drill into individual workers; future-
//! selves can layer a force-terminate action on top.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_params_map, use_query_map};
use leptos_shadcn_button::{Button, ButtonVariant};
use leptos_shadcn_card::{Card, CardContent};
use leptos_shadcn_select::{Select, SelectContent, SelectItem, SelectTrigger, SelectValue};

use crate::api;
use crate::pages::{Async, AsyncView, PageTable};
use crate::ui_path;

#[component]
pub fn List() -> impl IntoView {
    let params = use_params_map();
    let tenant = move || params.with(|p| p.get("tenant").unwrap_or_default().to_string());
    let query = use_query_map();
    // Seed `agent_session_id` filter from the URL so the sessions
    // detail page's "Workers for this session" link works as a
    // pre-filtered view.
    let seeded_agent = move || query.read().get("agent_session_id").unwrap_or_default();

    let (state, set_state) = signal::<Async<api::ListResponse<api::SessionWorker>>>(Async::Loading);
    let (live_filter, set_live_filter) = signal::<String>(String::from("any"));
    let (agent_filter, set_agent_filter) = signal::<String>(String::new());
    let (open_live_filter, set_open_live_filter) = signal(false);

    Effect::new(move |_| {
        set_agent_filter.set(seeded_agent());
    });

    let refetch = move || {
        let t = tenant();
        let live_val = match live_filter.get_untracked().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };
        let agent = agent_filter.get_untracked();
        let agent_opt = if agent.is_empty() { None } else { Some(agent) };
        spawn_local(async move {
            let a = agent_opt.as_deref();
            set_state.set(match api::list_session_workers(&t, a, None, live_val).await {
                Ok(r) => Async::Loaded(r),
                Err(err) => Async::Failed(err),
            });
        });
    };

    Effect::new(move |_| {
        refetch();
    });

    view! {
        <article class="space-y-6">
            <header class="flex items-center justify-between gap-4">
                <h1 class="text-3xl font-semibold tracking-tight">"Workers"</h1>
            </header>

            <p class="text-muted-foreground">
                "Live + reaped " <code>"session_worker"</code> " rows. Read-only — \
                 session-broker owns writes. Most-recently-spawned first."
            </p>

            <Card>
                <CardContent class="pt-6">
                    <form
                        class="grid gap-3 md:grid-cols-[14rem_1fr_auto] md:items-end"
                        on:submit=move |ev: web_sys::SubmitEvent| {
                            ev.prevent_default();
                            refetch();
                        }
                    >
                        <div class="space-y-2">
                            <p class="text-sm font-medium">"Live"</p>
                            <Select
                                open=Signal::derive(move || open_live_filter.get())
                                on_open_change=Callback::new(move |v| set_open_live_filter.set(v))
                                value=Signal::derive(move || live_filter.get())
                                on_value_change=Callback::new(move |v| {
                                    set_live_filter.set(v);
                                    refetch();
                                })
                            >
                                <SelectTrigger class="w-full">
                                    <SelectValue placeholder="any" />
                                </SelectTrigger>
                                <SelectContent>
                                    <SelectItem value="any">"any"</SelectItem>
                                    <SelectItem value="true">"live only (reaped_at IS NULL)"</SelectItem>
                                    <SelectItem value="false">"reaped only"</SelectItem>
                                </SelectContent>
                            </Select>
                        </div>
                        <div class="space-y-2">
                            <p class="text-sm font-medium">"agent_session_id"</p>
                            <input
                                class="h-10 w-full rounded-md border border-input bg-background px-3 py-2 text-sm"
                                type="text"
                                placeholder="<uuid>"
                                prop:value=move || agent_filter.get()
                                on:input:target=move |ev| set_agent_filter.set(ev.target().value())
                            />
                        </div>
                        <Button variant=ButtonVariant::Secondary>
                            "Apply"
                        </Button>
                    </form>
                </CardContent>
            </Card>

            <AsyncView
                state=state
                children=Box::new(|r: api::ListResponse<api::SessionWorker>| {
                    if r.items.is_empty() {
                        view! {
                            <p class="text-sm text-muted-foreground">"No session_worker rows match."</p>
                        }.into_any()
                    } else {
                        view! {
                            <PageTable>
                                <thead class="border-b border-border text-left text-xs uppercase tracking-wide text-muted-foreground">
                                    <tr>
                                        <th class="px-3 py-2">"Container"</th>
                                        <th class="px-3 py-2">"IP"</th>
                                        <th class="px-3 py-2">"Plugin"</th>
                                        <th class="px-3 py-2">"Agent session"</th>
                                        <th class="px-3 py-2">"Spawned"</th>
                                        <th class="px-3 py-2">"Reaped"</th>
                                    </tr>
                                </thead>
                                <tbody class="divide-y divide-border">
                                    {r.items.into_iter().map(|w| {
                                        let detail = ui_path!("/workers/{}", w.id);
                                        let plink = ui_path!("/plugins/{}", w.plugin_id);
                                        let session_view = match w.agent_session_id.clone() {
                                            Some(aid) => {
                                                let href = ui_path!("/sessions/{}", aid);
                                                view! {
                                                    <A href=href>
                                                        <code class="text-muted-foreground">{aid}</code>
                                                    </A>
                                                }.into_any()
                                            }
                                            None => view! {
                                                <span class="text-muted-foreground">"(unbound)"</span>
                                            }.into_any(),
                                        };
                                        let reaped_view = match w.reaped_at.clone() {
                                            Some(ts) => view! { <span>{ts}</span> }.into_any(),
                                            None => view! { <span class="text-muted-foreground">"—"</span> }.into_any(),
                                        };
                                        view! {
                                            <tr>
                                                <td class="px-3 py-2"><A href=detail.clone()>{w.container_name.clone()}</A></td>
                                                <td class="px-3 py-2"><code class="text-muted-foreground">{w.container_ip.clone()}</code></td>
                                                <td class="px-3 py-2">
                                                    <A href=plink>
                                                        <code class="text-muted-foreground">{w.plugin_id.clone()}</code>
                                                    </A>
                                                </td>
                                                <td class="px-3 py-2">{session_view}</td>
                                                <td class="px-3 py-2">{w.spawned_at.clone()}</td>
                                                <td class="px-3 py-2">{reaped_view}</td>
                                            </tr>
                                        }
                                    }).collect_view()}
                                </tbody>
                            </PageTable>
                        }.into_any()
                    }
                })
            />
        </article>
    }
}

#[component]
pub fn Detail() -> impl IntoView {
    let params = use_params_map();
    let tenant = move || params.with(|p| p.get("tenant").unwrap_or_default().to_string());
    let id = move || params.read().get("id").unwrap_or_default();
    let (state, set_state) = signal::<Async<api::SessionWorker>>(Async::Loading);

    Effect::new(move |_| {
        let id_val = id();
        spawn_local(async move {
            set_state.set(match api::get_session_worker(&tenant(), &id_val).await {
                Ok(w) => Async::Loaded(w),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="space-y-6">
            <header class="space-y-2">
                <h1 class="text-3xl font-semibold tracking-tight">"Worker"</h1>
                <p>
                    <A
                        href=ui_path!("/workers")
                        attr:class="text-primary hover:underline"
                    >
                        "← All workers"
                    </A>
                </p>
            </header>

            <AsyncView
                state=state
                children=Box::new(|w: api::SessionWorker| {
                    let plink = ui_path!("/plugins/{}", w.plugin_id);
                    let session_view = match w.agent_session_id.clone() {
                        Some(aid) => {
                            let href = ui_path!("/sessions/{}", aid);
                            view! {
                                <A href=href>
                                    <code>{aid}</code>
                                </A>
                            }
                            .into_any()
                        }
                        None => view! {
                            <span class="text-muted-foreground">
                                "(unbound — spawn-to-first-bind window)"
                            </span>
                        }
                        .into_any(),
                    };
                    let reaped_view = match w.reaped_at.clone() {
                        Some(ts) => view! { <span>{ts}</span> }.into_any(),
                        None => view! {
                            <span class="text-muted-foreground">"(live — reaped_at IS NULL)"</span>
                        }.into_any(),
                    };
                    view! {
                        <Card>
                            <CardContent class="pt-6">
                                <dl class="grid grid-cols-[180px_1fr] gap-y-3 text-sm">
                                    <dt class="text-muted-foreground">"Container"</dt>
                                    <dd><code>{w.container_name.clone()}</code></dd>
                                    <dt class="text-muted-foreground">"Container IP"</dt>
                                    <dd><code>{w.container_ip.clone()}</code></dd>
                                    <dt class="text-muted-foreground">"Plugin"</dt>
                                    <dd><A href=plink><code>{w.plugin_id.clone()}</code></A></dd>
                                    <dt class="text-muted-foreground">"Agent session"</dt>
                                    <dd>{session_view}</dd>
                                    <dt class="text-muted-foreground">"MCP session id"</dt>
                                    <dd>
                                        <code>
                                            {if w.mcp_session_id.is_empty() {
                                                "(initialize not yet returned)".to_string()
                                            } else {
                                                w.mcp_session_id.clone()
                                            }}
                                        </code>
                                    </dd>
                                    <dt class="text-muted-foreground">"Spawned"</dt>
                                    <dd>{w.spawned_at.clone()}</dd>
                                    <dt class="text-muted-foreground">"Reaped"</dt>
                                    <dd>{reaped_view}</dd>
                                    <dt class="text-muted-foreground">"DB id"</dt>
                                    <dd><code class="text-muted-foreground">{w.id.clone()}</code></dd>
                                </dl>
                            </CardContent>
                        </Card>
                    }.into_any()
                })
            />
        </article>
    }
}
