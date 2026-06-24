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

use crate::api;
use crate::pages::{Async, AsyncView};
use crate::ui_path;

#[component]
pub fn List() -> impl IntoView {
    let query = use_query_map();
    // Seed `agent_session_id` filter from the URL so the sessions
    // detail page's "Workers for this session" link works as a
    // pre-filtered view.
    let seeded_agent = move || query.read().get("agent_session_id").unwrap_or_default();

    let (state, set_state) = signal::<Async<api::ListResponse<api::SessionWorker>>>(Async::Loading);
    let (live_filter, set_live_filter) = signal::<String>(String::from("any"));
    let (agent_filter, set_agent_filter) = signal::<String>(String::new());

    Effect::new(move |_| {
        set_agent_filter.set(seeded_agent());
    });

    let refetch = move || {
        let live_val = match live_filter.get_untracked().as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };
        let agent = agent_filter.get_untracked();
        let agent_opt = if agent.is_empty() { None } else { Some(agent) };
        spawn_local(async move {
            let a = agent_opt.as_deref();
            set_state.set(match api::list_session_workers(a, None, live_val).await {
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
                <h1>"Workers"</h1>
            </header>

            <p class="lede">
                "Live + reaped " <code>"session_worker"</code> " rows. Read-only — \
                 session-broker owns writes. Most-recently-spawned first."
            </p>

            <form
                class="filter-row"
                on:submit=move |ev: web_sys::SubmitEvent| {
                    ev.prevent_default();
                    refetch();
                }
            >
                <label>
                    <span>"Live"</span>
                    <select
                        on:change:target=move |ev| {
                            set_live_filter.set(ev.target().value());
                            refetch();
                        }
                    >
                        <option value="any">"any"</option>
                        <option value="true">"live only (reaped_at IS NULL)"</option>
                        <option value="false">"reaped only"</option>
                    </select>
                </label>
                <label>
                    <span>"agent_session_id"</span>
                    <input
                        type="text"
                        placeholder="<uuid>"
                        prop:value=move || agent_filter.get()
                        on:input:target=move |ev| set_agent_filter.set(ev.target().value())
                    />
                </label>
                <button type="submit">"Apply"</button>
            </form>

            <AsyncView
                state=state
                children=Box::new(|r: api::ListResponse<api::SessionWorker>| {
                    if r.items.is_empty() {
                        view! {
                            <p class="muted">"No session_worker rows match."</p>
                        }.into_any()
                    } else {
                        view! {
                            <table class="entity-list">
                                <thead>
                                    <tr>
                                        <th>"Container"</th>
                                        <th>"IP"</th>
                                        <th>"Plugin"</th>
                                        <th>"Agent session"</th>
                                        <th>"Spawned"</th>
                                        <th>"Reaped"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {r.items.into_iter().map(|w| {
                                        let detail = ui_path!("/workers/{}", w.id);
                                        let plink = ui_path!("/plugins/{}", w.plugin_id);
                                        let session_view = match w.agent_session_id.clone() {
                                            Some(aid) => {
                                                let href = ui_path!("/sessions/{}", aid);
                                                view! {
                                                    <A href=href>
                                                        <code class="muted">{aid}</code>
                                                    </A>
                                                }.into_any()
                                            }
                                            None => view! {
                                                <span class="muted">"(unbound)"</span>
                                            }.into_any(),
                                        };
                                        let reaped_view = match w.reaped_at.clone() {
                                            Some(ts) => view! { <span>{ts}</span> }.into_any(),
                                            None => view! { <span class="muted">"—"</span> }.into_any(),
                                        };
                                        view! {
                                            <tr>
                                                <td><A href=detail.clone()>{w.container_name.clone()}</A></td>
                                                <td><code class="muted">{w.container_ip.clone()}</code></td>
                                                <td>
                                                    <A href=plink>
                                                        <code class="muted">{w.plugin_id.clone()}</code>
                                                    </A>
                                                </td>
                                                <td>{session_view}</td>
                                                <td>{w.spawned_at.clone()}</td>
                                                <td>{reaped_view}</td>
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
    let (state, set_state) = signal::<Async<api::SessionWorker>>(Async::Loading);

    Effect::new(move |_| {
        let id_val = id();
        spawn_local(async move {
            set_state.set(match api::get_session_worker(&id_val).await {
                Ok(w) => Async::Loaded(w),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Worker"</h1>
                <p><A href=ui_path!("/workers")>"← All workers"</A></p>
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
                            <span class="muted">
                                "(unbound — spawn-to-first-bind window)"
                            </span>
                        }
                        .into_any(),
                    };
                    let reaped_view = match w.reaped_at.clone() {
                        Some(ts) => view! { <span>{ts}</span> }.into_any(),
                        None => view! {
                            <span class="muted">"(live — reaped_at IS NULL)"</span>
                        }.into_any(),
                    };
                    view! {
                        <dl class="entity-detail">
                            <dt>"Container"</dt>
                            <dd><code>{w.container_name.clone()}</code></dd>
                            <dt>"Container IP"</dt>
                            <dd><code>{w.container_ip.clone()}</code></dd>
                            <dt>"Plugin"</dt>
                            <dd><A href=plink><code>{w.plugin_id.clone()}</code></A></dd>
                            <dt>"Agent session"</dt>
                            <dd>{session_view}</dd>
                            <dt>"MCP session id"</dt>
                            <dd>
                                <code>
                                    {if w.mcp_session_id.is_empty() {
                                        "(initialize not yet returned)".to_string()
                                    } else {
                                        w.mcp_session_id.clone()
                                    }}
                                </code>
                            </dd>
                            <dt>"Spawned"</dt>
                            <dd>{w.spawned_at.clone()}</dd>
                            <dt>"Reaped"</dt>
                            <dd>{reaped_view}</dd>
                            <dt>"DB id"</dt>
                            <dd><code class="muted">{w.id.clone()}</code></dd>
                        </dl>
                    }.into_any()
                })
            />
        </article>
    }
}
