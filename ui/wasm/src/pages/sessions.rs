// SPDX-License-Identifier: Apache-2.0

//! agent_session read-only pages.
//!
//! # Why read-only
//!
//! agent_session rows are written by session-broker as agents spawn,
//! transition between lifecycle states, and die. Operator-driven
//! CUD on them doesn't have a sensible shape:
//!
//! * **Create**: sessions come into existence through the spawn
//!   path, not through api. There's no "create a session row
//!   out of thin air" shape.
//! * **Update**: session-broker owns lifecycle (state transitions,
//!   last_active_at bumps). UI PUTs would race with the writer.
//! * **Delete**: could legitimately mean "force-terminate this live
//!   session", but that's a control-plane / session-broker concern.
//!   The workspace_plugin live-state gate is the template; we add
//!   force-terminate here when there's a concrete UI need (parking
//!   lot).
//!
//! So this page surface is list + detail only, matching the
//! api read endpoints landed in PR #131.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::use_params_map;
use leptos_shadcn_card::{Card, CardContent};
use leptos_shadcn_select::{Select, SelectContent, SelectItem, SelectTrigger, SelectValue};

use crate::api;
use crate::pages::{Async, AsyncView, PageTable};
use crate::ui_path;

#[component]
pub fn List() -> impl IntoView {
    let (state, set_state) = signal::<Async<api::ListResponse<api::AgentSession>>>(Async::Loading);
    let (state_filter, set_state_filter) = signal::<String>(String::new());
    let (open_filter, set_open_filter) = signal(false);

    let refetch = move || {
        let filter = state_filter.get_untracked();
        let filter_opt = if filter.is_empty() {
            None
        } else {
            Some(filter)
        };
        spawn_local(async move {
            let f = filter_opt.as_deref();
            set_state.set(match api::list_agent_sessions(None, None, f).await {
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
                <h1 class="text-3xl font-semibold tracking-tight">"Sessions"</h1>
            </header>

            <p class="text-muted-foreground">
                "Live + historical " <code>"agent_session"</code> " rows. Read-only — \
                 session-broker owns writes. Most-recently-active first."
            </p>

            <Card>
                <CardContent class="pt-6">
                    <form
                        class="grid max-w-xl gap-3 md:grid-cols-[14rem_auto] md:items-end"
                        on:submit=move |ev: web_sys::SubmitEvent| {
                            ev.prevent_default();
                            refetch();
                        }
                    >
                        <div class="space-y-2">
                            <p class="text-sm font-medium">"Filter by state"</p>
                            <Select
                                open=Signal::derive(move || open_filter.get())
                                on_open_change=Callback::new(move |v| set_open_filter.set(v))
                                value=Signal::derive(move || state_filter.get())
                                on_value_change=Callback::new(move |v| {
                                    set_state_filter.set(v);
                                    refetch();
                                })
                            >
                                <SelectTrigger class="w-full">
                                    <SelectValue placeholder="(any)" />
                                </SelectTrigger>
                                <SelectContent>
                                    <SelectItem value="">"(any)"</SelectItem>
                                    <SelectItem value="active">"active"</SelectItem>
                                    <SelectItem value="grace">"grace"</SelectItem>
                                    <SelectItem value="inactive">"inactive"</SelectItem>
                                    <SelectItem value="teardown_requested">"teardown_requested"</SelectItem>
                                    <SelectItem value="purged">"purged"</SelectItem>
                                </SelectContent>
                            </Select>
                        </div>
                    </form>
                </CardContent>
            </Card>

            <AsyncView
                state=state
                children=Box::new(|r: api::ListResponse<api::AgentSession>| {
                    if r.items.is_empty() {
                        view! {
                            <p class="text-sm text-muted-foreground">"No agent_session rows match."</p>
                        }.into_any()
                    } else {
                        view! {
                            <PageTable>
                                <thead class="border-b border-border text-left text-xs uppercase tracking-wide text-muted-foreground">
                                    <tr>
                                        <th class="px-3 py-2">"Agent session id"</th>
                                        <th class="px-3 py-2">"State"</th>
                                        <th class="px-3 py-2">"Tenant"</th>
                                        <th class="px-3 py-2">"Workspace"</th>
                                        <th class="px-3 py-2">"Last active"</th>
                                        <th class="px-3 py-2">"Reactivations"</th>
                                    </tr>
                                </thead>
                                <tbody class="divide-y divide-border">
                                    {r.items.into_iter().map(|s| {
                                        let detail = ui_path!("/sessions/{}", s.id);
                                        let tlink = ui_path!("/tenants/{}", s.tenant_id);
                                        let wlink = ui_path!("/workspaces/{}", s.workspace_id);
                                        view! {
                                            <tr>
                                                <td class="px-3 py-2">
                                                    <A href=detail.clone()>
                                                        {s.agent_session_id.clone()}
                                                    </A>
                                                </td>
                                                <td class="px-3 py-2">
                                                    <code>{s.state.clone()}</code>
                                                </td>
                                                <td class="px-3 py-2">
                                                    <A href=tlink>
                                                        <code class="text-muted-foreground">{s.tenant_id.clone()}</code>
                                                    </A>
                                                </td>
                                                <td class="px-3 py-2">
                                                    <A href=wlink>
                                                        <code class="text-muted-foreground">{s.workspace_id.clone()}</code>
                                                    </A>
                                                </td>
                                                <td class="px-3 py-2">{s.last_active_at.clone()}</td>
                                                <td class="px-3 py-2">{s.reactivation_count}</td>
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
    let id = move || params.read().get("id").unwrap_or_default();
    let (state, set_state) = signal::<Async<api::AgentSession>>(Async::Loading);

    Effect::new(move |_| {
        let id_val = id();
        spawn_local(async move {
            set_state.set(match api::get_agent_session(&id_val).await {
                Ok(s) => Async::Loaded(s),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="space-y-6">
            <header class="space-y-2">
                <h1 class="text-3xl font-semibold tracking-tight">"Session"</h1>
                <p>
                    <A
                        href=ui_path!("/sessions")
                        attr:class="text-primary hover:underline"
                    >
                        "← All sessions"
                    </A>
                </p>
            </header>

            <AsyncView
                state=state
                children=Box::new(|s: api::AgentSession| {
                    let workers_link = ui_path!("/workers?agent_session_id={}", s.id);
                    let tenant_link = ui_path!("/tenants/{}", s.tenant_id);
                    let workspace_link = ui_path!("/workspaces/{}", s.workspace_id);
                    view! {
                        <Card>
                            <CardContent class="pt-6">
                                <dl class="grid grid-cols-[180px_1fr] gap-y-3 text-sm">
                                    <dt class="text-muted-foreground">"Agent session id"</dt>
                                    <dd><code>{s.agent_session_id.clone()}</code></dd>
                                    <dt class="text-muted-foreground">"State"</dt>
                                    <dd><code>{s.state.clone()}</code></dd>
                                    <dt class="text-muted-foreground">"Tenant"</dt>
                                    <dd><A href=tenant_link><code>{s.tenant_id.clone()}</code></A></dd>
                                    <dt class="text-muted-foreground">"Workspace"</dt>
                                    <dd><A href=workspace_link><code>{s.workspace_id.clone()}</code></A></dd>
                                    <dt class="text-muted-foreground">"Created"</dt>
                                    <dd>{s.created_at.clone()}</dd>
                                    <dt class="text-muted-foreground">"Last active"</dt>
                                    <dd>{s.last_active_at.clone()}</dd>
                                    <dt class="text-muted-foreground">"Reactivations"</dt>
                                    <dd>{s.reactivation_count}</dd>
                                    <dt class="text-muted-foreground">"DB id"</dt>
                                    <dd><code class="text-muted-foreground">{s.id.clone()}</code></dd>
                                </dl>
                            </CardContent>
                        </Card>
                        <div>
                            <A href=workers_link>"Workers for this session"</A>
                        </div>
                    }.into_any()
                })
            />
        </article>
    }
}
