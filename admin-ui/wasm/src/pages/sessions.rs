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
//!   path, not through admin-api. There's no "create a session row
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
//! admin-api read endpoints landed in PR #131.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::use_params_map;

use crate::api;
use crate::pages::{Async, AsyncView};
use crate::ui_path;

#[component]
pub fn List() -> impl IntoView {
    let (state, set_state) = signal::<Async<api::ListResponse<api::AgentSession>>>(Async::Loading);
    let (state_filter, set_state_filter) = signal::<String>(String::new());

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
        <article class="page">
            <header class="page-header">
                <h1>"Sessions"</h1>
            </header>

            <p class="lede">
                "Live + historical " <code>"agent_session"</code> " rows. Read-only — \
                 session-broker owns writes. Most-recently-active first."
            </p>

            <form
                class="filter-row"
                on:submit=move |ev: web_sys::SubmitEvent| {
                    ev.prevent_default();
                    refetch();
                }
            >
                <label>
                    <span>"Filter by state"</span>
                    <select
                        on:change:target=move |ev| {
                            set_state_filter.set(ev.target().value());
                            refetch();
                        }
                    >
                        <option value="">"(any)"</option>
                        <option value="active">"active"</option>
                        <option value="grace">"grace"</option>
                        <option value="inactive">"inactive"</option>
                        <option value="teardown_requested">"teardown_requested"</option>
                        <option value="purged">"purged"</option>
                    </select>
                </label>
            </form>

            <AsyncView
                state=state
                children=Box::new(|r: api::ListResponse<api::AgentSession>| {
                    if r.items.is_empty() {
                        view! {
                            <p class="muted">"No agent_session rows match."</p>
                        }.into_any()
                    } else {
                        view! {
                            <table class="entity-list">
                                <thead>
                                    <tr>
                                        <th>"Agent session id"</th>
                                        <th>"State"</th>
                                        <th>"Tenant"</th>
                                        <th>"Workspace"</th>
                                        <th>"Last active"</th>
                                        <th>"Reactivations"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {r.items.into_iter().map(|s| {
                                        let detail = ui_path!("/sessions/{}", s.id);
                                        let tlink = ui_path!("/tenants/{}", s.tenant_id);
                                        let wlink = ui_path!("/workspaces/{}", s.workspace_id);
                                        view! {
                                            <tr>
                                                <td>
                                                    <A href=detail.clone()>
                                                        {s.agent_session_id.clone()}
                                                    </A>
                                                </td>
                                                <td>
                                                    <code>{s.state.clone()}</code>
                                                </td>
                                                <td>
                                                    <A href=tlink>
                                                        <code class="muted">{s.tenant_id.clone()}</code>
                                                    </A>
                                                </td>
                                                <td>
                                                    <A href=wlink>
                                                        <code class="muted">{s.workspace_id.clone()}</code>
                                                    </A>
                                                </td>
                                                <td>{s.last_active_at.clone()}</td>
                                                <td>{s.reactivation_count}</td>
                                            </tr>
                                        }
                                    }).collect_view()}
                                </tbody>
                            </table>
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
        <article class="page">
            <header class="page-header">
                <h1>"Session"</h1>
                <p><A href=ui_path!("/sessions")>"← All sessions"</A></p>
            </header>

            <AsyncView
                state=state
                children=Box::new(|s: api::AgentSession| {
                    let workers_link = ui_path!("/workers?agent_session_id={}", s.id);
                    let tenant_link = ui_path!("/tenants/{}", s.tenant_id);
                    let workspace_link = ui_path!("/workspaces/{}", s.workspace_id);
                    view! {
                        <dl class="entity-detail">
                            <dt>"Agent session id"</dt>
                            <dd><code>{s.agent_session_id.clone()}</code></dd>
                            <dt>"State"</dt>
                            <dd><code>{s.state.clone()}</code></dd>
                            <dt>"Tenant"</dt>
                            <dd><A href=tenant_link><code>{s.tenant_id.clone()}</code></A></dd>
                            <dt>"Workspace"</dt>
                            <dd><A href=workspace_link><code>{s.workspace_id.clone()}</code></A></dd>
                            <dt>"Created"</dt>
                            <dd>{s.created_at.clone()}</dd>
                            <dt>"Last active"</dt>
                            <dd>{s.last_active_at.clone()}</dd>
                            <dt>"Reactivations"</dt>
                            <dd>{s.reactivation_count}</dd>
                            <dt>"DB id"</dt>
                            <dd><code class="muted">{s.id.clone()}</code></dd>
                        </dl>
                        <div class="actions">
                            <A href=workers_link>"Workers for this session"</A>
                        </div>
                    }.into_any()
                })
            />
        </article>
    }
}
