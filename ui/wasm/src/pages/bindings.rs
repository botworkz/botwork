// SPDX-License-Identifier: Apache-2.0

//! Workspace_plugin (binding) CRUD pages.
//!
//! Bindings have a composite primary key on `(workspace_id,
//! plugin_id)`, so routes carry both segments. Per-binding `config`
//! is optional JSON — the form treats blank as "leave unchanged" on
//! update and "no config" on create. The wire contract distinguishes
//! "absent" from "explicit null" via `Option<JsonValue>`; the UI
//! v0 doesn't surface the explicit-null-to-clear case, so a future
//! round will add a "clear config" toggle for that flow.
//!
//! Both update and delete go through api's live-state gate:
//! before mutating, api asks control-plane to terminate any
//! live session under this `(tenant, workspace, plugin)` triple. If
//! control-plane is unreachable the DB is rolled back and the
//! response is `503 unavailable`; the UI surfaces that with retry
//! framing rather than a generic error.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map};
use serde_json::Value as JsonValue;
use web_sys::SubmitEvent;

use crate::api;
use crate::pages::{render_dependents, Async, AsyncView};
use crate::ui_path;

fn pretty(v: &JsonValue) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".to_string())
}

#[component]
pub fn List() -> impl IntoView {
    let (state, set_state) =
        signal::<Async<api::ListResponse<api::WorkspacePlugin>>>(Async::Loading);

    Effect::new(move |_| {
        spawn_local(async move {
            set_state.set(match api::list_workspace_plugins(None, None).await {
                Ok(r) => Async::Loaded(r),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Bindings"</h1>
                <A href=ui_path!("/bindings/new")>
                    <button class="primary">"+ New binding"</button>
                </A>
            </header>

            <AsyncView
                state=state
                children=Box::new(|r: api::ListResponse<api::WorkspacePlugin>| {
                    if r.items.is_empty() {
                        view! {
                            <p class="muted">
                                "No bindings yet. Bindings associate a plugin with a \
                                 workspace; you need at least one workspace and one \
                                 plugin first."
                            </p>
                        }.into_any()
                    } else {
                        view! {
                            <table class="entity-list">
                                <thead>
                                    <tr>
                                        <th>"Workspace"</th>
                                        <th>"Plugin"</th>
                                        <th>"Config?"</th>
                                        <th>"Updated"</th>
                                        <th>"Actions"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {r.items.into_iter().map(|b| {
                                        let detail = ui_path!(
                                            "/bindings/{}/{}", b.workspace_id, b.plugin_id
                                        );
                                        let edit = ui_path!(
                                            "/bindings/{}/{}/edit", b.workspace_id, b.plugin_id
                                        );
                                        let delete = ui_path!(
                                            "/bindings/{}/{}/delete",
                                            b.workspace_id, b.plugin_id,
                                        );
                                        let ws_link = ui_path!("/workspaces/{}", b.workspace_id);
                                        let pl_link = ui_path!("/plugins/{}", b.plugin_id);
                                        let has_config = b.config.is_some();
                                        view! {
                                            <tr>
                                                <td>
                                                    <A href=ws_link>
                                                        <code class="muted">{b.workspace_id.clone()}</code>
                                                    </A>
                                                </td>
                                                <td>
                                                    <A href=pl_link>
                                                        <code class="muted">{b.plugin_id.clone()}</code>
                                                    </A>
                                                </td>
                                                <td>
                                                    {if has_config { "yes" } else { "—" }}
                                                </td>
                                                <td>{b.updated_at.clone()}</td>
                                                <td class="actions">
                                                    <A href=detail.clone()>"View"</A>
                                                    " · "
                                                    <A href=edit>"Edit"</A>
                                                    " · "
                                                    <A href=delete>"Delete"</A>
                                                </td>
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
    let wid = move || params.read().get("workspace_id").unwrap_or_default();
    let pid = move || params.read().get("plugin_id").unwrap_or_default();
    let (state, set_state) = signal::<Async<api::WorkspacePlugin>>(Async::Loading);

    Effect::new(move |_| {
        let (w, p) = (wid(), pid());
        spawn_local(async move {
            set_state.set(match api::get_workspace_plugin(&w, &p).await {
                Ok(b) => Async::Loaded(b),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Binding"</h1>
                <p><A href=ui_path!("/bindings")>"← All bindings"</A></p>
            </header>

            <AsyncView
                state=state
                children=Box::new(|b: api::WorkspacePlugin| {
                    let edit_link = ui_path!(
                        "/bindings/{}/{}/edit", b.workspace_id, b.plugin_id
                    );
                    let delete_link = ui_path!(
                        "/bindings/{}/{}/delete", b.workspace_id, b.plugin_id
                    );
                    let ws_link = ui_path!("/workspaces/{}", b.workspace_id);
                    let pl_link = ui_path!("/plugins/{}", b.plugin_id);
                    let config_display = match &b.config {
                        Some(v) => pretty(v),
                        None => "(no config)".to_string(),
                    };
                    view! {
                        <dl class="entity-detail">
                            <dt>"Workspace"</dt>
                            <dd><A href=ws_link><code>{b.workspace_id.clone()}</code></A></dd>
                            <dt>"Plugin"</dt>
                            <dd><A href=pl_link><code>{b.plugin_id.clone()}</code></A></dd>
                            <dt>"Config"</dt>
                            <dd><pre><code>{config_display}</code></pre></dd>
                            <dt>"Created"</dt>
                            <dd>{b.created_at.clone()}</dd>
                            <dt>"Updated"</dt>
                            <dd>{b.updated_at.clone()}</dd>
                        </dl>
                        <div class="actions">
                            <A href=edit_link>"Edit"</A>
                            " · "
                            <A href=delete_link>"Delete"</A>
                        </div>
                    }.into_any()
                })
            />
        </article>
    }
}

#[component]
pub fn Create() -> impl IntoView {
    // We need workspace + plugin selects, populated from list calls.
    let (workspaces, set_workspaces) =
        signal::<Async<api::ListResponse<api::Workspace>>>(Async::Loading);
    let (plugins, set_plugins) = signal::<Async<api::ListResponse<api::Plugin>>>(Async::Loading);
    let (workspace_id, set_workspace_id) = signal(String::new());
    let (plugin_id, set_plugin_id) = signal(String::new());
    let (config, set_config) = signal(String::new());
    let (error, set_error) = signal::<Option<String>>(None);
    let (api_error, set_api_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    Effect::new(move |_| {
        spawn_local(async move {
            let result = api::list_workspaces(None).await;
            if let Ok(r) = &result {
                if let Some(first) = r.items.first() {
                    set_workspace_id.set(first.id.clone());
                }
            }
            set_workspaces.set(match result {
                Ok(r) => Async::Loaded(r),
                Err(err) => Async::Failed(err),
            });
        });
        spawn_local(async move {
            let result = api::list_plugins().await;
            if let Ok(r) = &result {
                if let Some(first) = r.items.first() {
                    set_plugin_id.set(first.id.clone());
                }
            }
            set_plugins.set(match result {
                Ok(r) => Async::Loaded(r),
                Err(err) => Async::Failed(err),
            });
        });
    });

    let navigate = use_navigate();
    let on_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        let config_val = {
            let s = config.get_untracked();
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                match serde_json::from_str::<JsonValue>(t) {
                    Ok(v) => Some(v),
                    Err(err) => {
                        set_error.set(Some(format!("invalid config JSON: {err}")));
                        return;
                    }
                }
            }
        };
        let body = api::WorkspacePluginCreate {
            workspace_id: workspace_id.get_untracked(),
            plugin_id: plugin_id.get_untracked(),
            config: config_val,
        };
        if body.workspace_id.is_empty() || body.plugin_id.is_empty() {
            set_error.set(Some(
                "workspace and plugin must both be selected".to_string(),
            ));
            return;
        }
        set_error.set(None);
        set_api_error.set(None);
        set_busy.set(true);
        let navigate = navigate.clone();
        spawn_local(async move {
            match api::create_workspace_plugin(&body).await {
                Ok(b) => navigate(
                    &ui_path!("/bindings/{}/{}", b.workspace_id, b.plugin_id),
                    Default::default(),
                ),
                Err(err) => {
                    set_api_error.set(Some(err));
                    set_busy.set(false);
                }
            }
        });
    };

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"New binding"</h1>
                <p><A href=ui_path!("/bindings")>"← All bindings"</A></p>
            </header>

            <form class="entity-form" on:submit=on_submit>
                <AsyncView
                    state=workspaces
                    children=Box::new(move |r: api::ListResponse<api::Workspace>| {
                        if r.items.is_empty() {
                            return view! {
                                <p class="error">"No workspaces exist; create one first."</p>
                            }.into_any();
                        }
                        view! {
                            <label>
                                <span>"Workspace"</span>
                                <select
                                    on:change:target=move |ev| set_workspace_id.set(ev.target().value())
                                >
                                    {r.items.into_iter().map(|w| view! {
                                        <option value=w.id.clone()>
                                            {w.name.clone()} " (" {w.id.clone()} ")"
                                        </option>
                                    }).collect_view()}
                                </select>
                            </label>
                        }.into_any()
                    })
                />
                <AsyncView
                    state=plugins
                    children=Box::new(move |r: api::ListResponse<api::Plugin>| {
                        if r.items.is_empty() {
                            return view! {
                                <p class="error">"No plugins exist; create one first."</p>
                            }.into_any();
                        }
                        view! {
                            <label>
                                <span>"Plugin"</span>
                                <select
                                    on:change:target=move |ev| set_plugin_id.set(ev.target().value())
                                >
                                    {r.items.into_iter().map(|p| view! {
                                        <option value=p.id.clone()>
                                            {p.name.clone()} " (" {p.id.clone()} ")"
                                        </option>
                                    }).collect_view()}
                                </select>
                            </label>
                        }.into_any()
                    })
                />
                <label>
                    <span>"Config (JSON, optional)"</span>
                    <textarea
                        rows="6"
                        placeholder=r#"{"some_key": "some_value"}"#
                        prop:value=move || config.get()
                        on:input:target=move |ev| set_config.set(ev.target().value())
                    ></textarea>
                </label>
                {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
                {move || api_error.get().map(|err| view! {
                    <p class="error">{err.message().to_string()}</p>
                })}
                <div class="actions">
                    <button
                        type="submit"
                        class="primary"
                        prop:disabled=move || busy.get()
                    >
                        {move || if busy.get() { "Creating…" } else { "Create" }}
                    </button>
                </div>
            </form>
        </article>
    }
}

#[component]
pub fn Edit() -> impl IntoView {
    let params = use_params_map();
    let wid = Memo::new(move |_| params.read().get("workspace_id").unwrap_or_default());
    let pid = Memo::new(move |_| params.read().get("plugin_id").unwrap_or_default());

    let (loaded, set_loaded) = signal::<Async<api::WorkspacePlugin>>(Async::Loading);
    let (config, set_config) = signal(String::new());
    let (lock, set_lock) = signal(String::new());
    let (error, set_error) = signal::<Option<String>>(None);
    let (api_error, set_api_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    Effect::new(move |_| {
        let (w, p) = (wid.get(), pid.get());
        spawn_local(async move {
            match api::get_workspace_plugin(&w, &p).await {
                Ok(b) => {
                    if let Some(c) = &b.config {
                        set_config.set(pretty(c));
                    } else {
                        set_config.set(String::new());
                    }
                    set_lock.set(b.updated_at.clone());
                    set_loaded.set(Async::Loaded(b));
                }
                Err(err) => set_loaded.set(Async::Failed(err)),
            }
        });
    });

    let navigate = use_navigate();
    let on_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        let config_val = {
            let s = config.get_untracked();
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                match serde_json::from_str::<JsonValue>(t) {
                    Ok(v) => Some(v),
                    Err(err) => {
                        set_error.set(Some(format!("invalid config JSON: {err}")));
                        return;
                    }
                }
            }
        };
        let body = api::WorkspacePluginUpdate {
            config: config_val,
            if_unmodified_since: lock.get_untracked(),
        };
        set_error.set(None);
        set_api_error.set(None);
        set_busy.set(true);
        let (w, p) = (wid.get_untracked(), pid.get_untracked());
        let navigate = navigate.clone();
        spawn_local(async move {
            match api::update_workspace_plugin(&w, &p, &body).await {
                Ok(_) => navigate(&ui_path!("/bindings/{}/{}", w, p), Default::default()),
                Err(api::ApiError::Stale { message }) => {
                    if let Ok(fresh) = api::get_workspace_plugin(&w, &p).await {
                        set_lock.set(fresh.updated_at.clone());
                        set_loaded.set(Async::Loaded(fresh));
                    }
                    set_api_error.set(Some(api::ApiError::Stale { message }));
                    set_busy.set(false);
                }
                Err(err) => {
                    set_api_error.set(Some(err));
                    set_busy.set(false);
                }
            }
        });
    };

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Edit binding"</h1>
                <p><A href=ui_path!("/bindings")>"← All bindings"</A></p>
            </header>

            <AsyncView
                state=loaded
                children=Box::new(move |_b: api::WorkspacePlugin| {
                    let on_submit = on_submit.clone();
                    view! {
                        <form class="entity-form" on:submit=on_submit>
                            <label>
                                <span>"Config (JSON, blank to clear on next round; absent = no change)"</span>
                                <textarea
                                    rows="8"
                                    prop:value=move || config.get()
                                    on:input:target=move |ev| set_config.set(ev.target().value())
                                ></textarea>
                            </label>
                            <p class="muted lock-token">
                                "Lock token: " <code>{move || lock.get()}</code>
                            </p>
                            {move || error.get().map(|e| view! { <p class="error">{e}</p> })}
                            {move || api_error.get().map(|err| match err {
                                api::ApiError::Unavailable { message } => view! {
                                    <p class="error">
                                        {message}
                                        " — control-plane was unreachable; the DB was \
                                         rolled back. Click Save again to retry."
                                    </p>
                                }.into_any(),
                                other => view! {
                                    <p class="error">{other.message().to_string()}</p>
                                }.into_any(),
                            })}
                            <div class="actions">
                                <button
                                    type="submit"
                                    class="primary"
                                    prop:disabled=move || busy.get()
                                >
                                    {move || if busy.get() { "Saving…" } else { "Save" }}
                                </button>
                            </div>
                        </form>
                    }.into_any()
                })
            />
        </article>
    }
}

#[component]
pub fn DeleteConfirm() -> impl IntoView {
    let params = use_params_map();
    let wid = Memo::new(move |_| params.read().get("workspace_id").unwrap_or_default());
    let pid = Memo::new(move |_| params.read().get("plugin_id").unwrap_or_default());

    let (loaded, set_loaded) = signal::<Async<api::WorkspacePlugin>>(Async::Loading);
    let (error, set_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    Effect::new(move |_| {
        let (w, p) = (wid.get(), pid.get());
        spawn_local(async move {
            set_loaded.set(match api::get_workspace_plugin(&w, &p).await {
                Ok(b) => Async::Loaded(b),
                Err(err) => Async::Failed(err),
            });
        });
    });

    let navigate = use_navigate();

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Delete binding"</h1>
                <p><A href=ui_path!("/bindings")>"← All bindings"</A></p>
            </header>

            <AsyncView
                state=loaded
                children=Box::new(move |b: api::WorkspacePlugin| {
                    let detail_link = ui_path!(
                        "/bindings/{}/{}", b.workspace_id, b.plugin_id
                    );
                    let navigate_inner = navigate.clone();
                    let on_confirm = move |_ev: web_sys::MouseEvent| {
                        let (w, p) = (wid.get_untracked(), pid.get_untracked());
                        set_busy.set(true);
                        set_error.set(None);
                        let navigate = navigate_inner.clone();
                        spawn_local(async move {
                            match api::delete_workspace_plugin(&w, &p).await {
                                Ok(()) => navigate(ui_path!("/bindings"), Default::default()),
                                Err(err) => {
                                    set_error.set(Some(err));
                                    set_busy.set(false);
                                }
                            }
                        });
                    };
                    view! {
                        <p>
                            "You are about to delete the binding between workspace "
                            <code>{b.workspace_id.clone()}</code>
                            " and plugin "
                            <code>{b.plugin_id.clone()}</code> "."
                        </p>
                        <p class="muted">
                            "api walks the live-state gate against control-plane \
                             before this commits: any live session under this triple is \
                             terminated, then the row is deleted. If control-plane is \
                             unreachable the operation is rolled back and the API \
                             returns 503."
                        </p>
                        {move || error.get().map(|err| match err {
                            api::ApiError::HasDependents { message, dependents } => {
                                let dep_list = render_dependents(&dependents);
                                view! {
                                    <div class="error">
                                        <p>{message}</p>
                                        {dep_list}
                                    </div>
                                }.into_any()
                            }
                            api::ApiError::Unavailable { message } => view! {
                                <p class="error">
                                    {message}
                                    " — control-plane was unreachable; the DB was \
                                     rolled back. Click Confirm delete again to retry."
                                </p>
                            }.into_any(),
                            other => view! {
                                <p class="error">{other.message().to_string()}</p>
                            }.into_any(),
                        })}
                        <div class="actions">
                            <A href=detail_link.clone()>
                                <button>"Cancel"</button>
                            </A>
                            <button
                                class="danger"
                                on:click=on_confirm
                                prop:disabled=move || busy.get()
                            >
                                {move || if busy.get() { "Deleting…" } else { "Confirm delete" }}
                            </button>
                        </div>
                    }.into_any()
                })
            />
        </article>
    }
}
