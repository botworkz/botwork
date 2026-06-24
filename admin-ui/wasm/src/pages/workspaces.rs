// SPDX-License-Identifier: Apache-2.0

//! Workspace CRUD pages.
//!
//! Same five-route shape as tenants. The interesting differences from
//! the tenant template:
//!
//! * **Create** takes a `tenant_id` foreign key. v0 renders it as a
//!   select populated from `list_tenants()`; if the operator landed
//!   on this page from a tenant's detail link, the upstream link
//!   could pre-select but the URL grammar doesn't carry that today.
//! * **Delete** CASCADEs to `workspace_plugin` bindings and
//!   `agent_session` rows, AND walks the live-state gate against
//!   control-plane to terminate any live sessions. Failure modes:
//!   `409 has_dependents` is not actually emitted (CASCADE wipes
//!   everything), but `503 unavailable` can be when control-plane
//!   is down mid-cascade.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map};
use web_sys::SubmitEvent;

use crate::api;
use crate::pages::{render_dependents, Async, AsyncView};
use crate::ui_path;

#[component]
pub fn List() -> impl IntoView {
    let (state, set_state) = signal::<Async<api::ListResponse<api::Workspace>>>(Async::Loading);

    Effect::new(move |_| {
        spawn_local(async move {
            set_state.set(match api::list_workspaces(None).await {
                Ok(r) => Async::Loaded(r),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Workspaces"</h1>
                <A href=ui_path!("/workspaces/new")>
                    <button class="primary">"+ New workspace"</button>
                </A>
            </header>

            <AsyncView
                state=state
                children=Box::new(|r: api::ListResponse<api::Workspace>| {
                    if r.items.is_empty() {
                        view! {
                            <p class="muted">
                                "No workspaces yet. Click "
                                <em>"New workspace"</em>
                                " to create one (you must have at least one tenant first)."
                            </p>
                        }.into_any()
                    } else {
                        view! {
                            <table class="entity-list">
                                <thead>
                                    <tr>
                                        <th>"Name"</th>
                                        <th>"Tenant"</th>
                                        <th>"ID"</th>
                                        <th>"Updated"</th>
                                        <th>"Actions"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {r.items.into_iter().map(|w| {
                                        let detail = ui_path!("/workspaces/{}", w.id);
                                        let edit = ui_path!("/workspaces/{}/edit", w.id);
                                        let delete = ui_path!("/workspaces/{}/delete", w.id);
                                        let tenant_link = ui_path!("/tenants/{}", w.tenant_id);
                                        view! {
                                            <tr>
                                                <td><A href=detail.clone()>{w.name.clone()}</A></td>
                                                <td>
                                                    <A href=tenant_link>
                                                        <code class="muted">{w.tenant_id.clone()}</code>
                                                    </A>
                                                </td>
                                                <td><code class="muted">{w.id.clone()}</code></td>
                                                <td>{w.updated_at.clone()}</td>
                                                <td class="actions">
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
    let id = move || params.read().get("id").unwrap_or_default();
    let (state, set_state) = signal::<Async<api::Workspace>>(Async::Loading);

    Effect::new(move |_| {
        let id_val = id();
        spawn_local(async move {
            set_state.set(match api::get_workspace(&id_val).await {
                Ok(w) => Async::Loaded(w),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Workspace"</h1>
                <p><A href=ui_path!("/workspaces")>"← All workspaces"</A></p>
            </header>

            <AsyncView
                state=state
                children=Box::new(|w: api::Workspace| {
                    let edit_link = ui_path!("/workspaces/{}/edit", w.id);
                    let delete_link = ui_path!("/workspaces/{}/delete", w.id);
                    let tenant_link = ui_path!("/tenants/{}", w.tenant_id);
                    view! {
                        <dl class="entity-detail">
                            <dt>"Name"</dt>
                            <dd>{w.name.clone()}</dd>
                            <dt>"Tenant"</dt>
                            <dd><A href=tenant_link><code>{w.tenant_id.clone()}</code></A></dd>
                            <dt>"ID"</dt>
                            <dd><code>{w.id.clone()}</code></dd>
                            <dt>"Created"</dt>
                            <dd>{w.created_at.clone()}</dd>
                            <dt>"Updated"</dt>
                            <dd>{w.updated_at.clone()}</dd>
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
    // Need a tenant list to populate the select.
    let (tenants, set_tenants) = signal::<Async<api::ListResponse<api::Tenant>>>(Async::Loading);
    let (tenant_id, set_tenant_id) = signal(String::new());
    let (name, set_name) = signal(String::new());
    let (error, set_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    Effect::new(move |_| {
        spawn_local(async move {
            let result = api::list_tenants().await;
            // Default the select to the first tenant so the form is
            // useful on submit. Skipped if the list errors or is
            // empty; the form's submit handler validates the value.
            if let Ok(r) = &result {
                if let Some(first) = r.items.first() {
                    set_tenant_id.set(first.id.clone());
                }
            }
            set_tenants.set(match result {
                Ok(r) => Async::Loaded(r),
                Err(err) => Async::Failed(err),
            });
        });
    });

    let navigate = use_navigate();
    let on_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        let body = api::WorkspaceCreate {
            tenant_id: tenant_id.get_untracked(),
            name: name.get_untracked().trim().to_string(),
        };
        if body.tenant_id.is_empty() {
            set_error.set(Some(api::ApiError::ValidationFailed {
                message: "tenant must be selected".to_string(),
            }));
            return;
        }
        if body.name.is_empty() {
            set_error.set(Some(api::ApiError::ValidationFailed {
                message: "name must not be blank".to_string(),
            }));
            return;
        }
        set_busy.set(true);
        set_error.set(None);
        let navigate = navigate.clone();
        spawn_local(async move {
            match api::create_workspace(&body).await {
                Ok(w) => navigate(&ui_path!("/workspaces/{}", w.id), Default::default()),
                Err(err) => {
                    set_error.set(Some(err));
                    set_busy.set(false);
                }
            }
        });
    };

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"New workspace"</h1>
                <p><A href=ui_path!("/workspaces")>"← All workspaces"</A></p>
            </header>

            <AsyncView
                state=tenants
                children=Box::new(move |r: api::ListResponse<api::Tenant>| {
                    if r.items.is_empty() {
                        return view! {
                            <p class="error">
                                "No tenants exist; you must create a tenant before \
                                 you can create a workspace under it."
                            </p>
                        }.into_any();
                    }
                    let on_submit = on_submit.clone();
                    view! {
                        <form class="entity-form" on:submit=on_submit>
                            <label>
                                <span>"Tenant"</span>
                                <select
                                    on:change:target=move |ev| set_tenant_id.set(ev.target().value())
                                >
                                    {r.items.into_iter().map(|t| {
                                        view! {
                                            <option value=t.id.clone()>
                                                {t.name.clone()}
                                                " ("
                                                {t.id.clone()}
                                                ")"
                                            </option>
                                        }
                                    }).collect_view()}
                                </select>
                            </label>
                            <label>
                                <span>"Name"</span>
                                <input
                                    type="text"
                                    prop:value=move || name.get()
                                    on:input:target=move |ev| set_name.set(ev.target().value())
                                />
                            </label>
                            {move || error.get().map(|err| view! {
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
                    }.into_any()
                })
            />
        </article>
    }
}

#[component]
pub fn Edit() -> impl IntoView {
    let params = use_params_map();
    let id = Memo::new(move |_| params.read().get("id").unwrap_or_default());

    let (loaded, set_loaded) = signal::<Async<api::Workspace>>(Async::Loading);
    let (name, set_name) = signal(String::new());
    let (lock, set_lock) = signal(String::new());
    let (error, set_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    Effect::new(move |_| {
        let id_val = id.get();
        spawn_local(async move {
            match api::get_workspace(&id_val).await {
                Ok(w) => {
                    set_name.set(w.name.clone());
                    set_lock.set(w.updated_at.clone());
                    set_loaded.set(Async::Loaded(w));
                }
                Err(err) => set_loaded.set(Async::Failed(err)),
            }
        });
    });

    let navigate = use_navigate();
    let on_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        let body = api::WorkspaceUpdate {
            name: name.get_untracked().trim().to_string(),
            if_unmodified_since: lock.get_untracked(),
        };
        if body.name.is_empty() {
            set_error.set(Some(api::ApiError::ValidationFailed {
                message: "name must not be blank".to_string(),
            }));
            return;
        }
        let id_val = id.get_untracked();
        set_busy.set(true);
        set_error.set(None);
        let navigate = navigate.clone();
        spawn_local(async move {
            match api::update_workspace(&id_val, &body).await {
                Ok(_) => navigate(&ui_path!("/workspaces/{}", id_val), Default::default()),
                Err(api::ApiError::Stale { message }) => {
                    if let Ok(fresh) = api::get_workspace(&id_val).await {
                        set_lock.set(fresh.updated_at.clone());
                        set_loaded.set(Async::Loaded(fresh));
                    }
                    set_error.set(Some(api::ApiError::Stale { message }));
                    set_busy.set(false);
                }
                Err(err) => {
                    set_error.set(Some(err));
                    set_busy.set(false);
                }
            }
        });
    };

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Edit workspace"</h1>
                <p><A href=ui_path!("/workspaces")>"← All workspaces"</A></p>
            </header>

            <AsyncView
                state=loaded
                children=Box::new(move |_w: api::Workspace| {
                    let on_submit = on_submit.clone();
                    view! {
                        <form class="entity-form" on:submit=on_submit>
                            <label>
                                <span>"Name"</span>
                                <input
                                    type="text"
                                    prop:value=move || name.get()
                                    on:input:target=move |ev| set_name.set(ev.target().value())
                                />
                            </label>
                            <p class="muted lock-token">
                                "Lock token: " <code>{move || lock.get()}</code>
                            </p>
                            {move || error.get().map(|err| view! {
                                <p class="error">{err.message().to_string()}</p>
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
    let id = Memo::new(move |_| params.read().get("id").unwrap_or_default());

    let (loaded, set_loaded) = signal::<Async<api::Workspace>>(Async::Loading);
    let (error, set_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    Effect::new(move |_| {
        let id_val = id.get();
        spawn_local(async move {
            set_loaded.set(match api::get_workspace(&id_val).await {
                Ok(w) => Async::Loaded(w),
                Err(err) => Async::Failed(err),
            });
        });
    });

    let navigate = use_navigate();

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Delete workspace"</h1>
                <p><A href=ui_path!("/workspaces")>"← All workspaces"</A></p>
            </header>

            <AsyncView
                state=loaded
                children=Box::new(move |w: api::Workspace| {
                    let detail_link = ui_path!("/workspaces/{}", w.id);
                    let navigate_inner = navigate.clone();
                    let on_confirm = move |_ev: web_sys::MouseEvent| {
                        let id_val = id.get_untracked();
                        set_busy.set(true);
                        set_error.set(None);
                        let navigate = navigate_inner.clone();
                        spawn_local(async move {
                            match api::delete_workspace(&id_val).await {
                                Ok(()) => navigate(ui_path!("/workspaces"), Default::default()),
                                Err(err) => {
                                    set_error.set(Some(err));
                                    set_busy.set(false);
                                }
                            }
                        });
                    };
                    view! {
                        <p>
                            "You are about to delete workspace "
                            <strong>{w.name.clone()}</strong>
                            " (" <code>{w.id.clone()}</code> ")."
                        </p>
                        <p class="muted">
                            "This CASCADEs to every binding and agent_session under \
                             this workspace. If control-plane is reachable, any live \
                             sessions are terminated as part of the same operation; \
                             if control-plane is unreachable the delete is rolled back \
                             and the API returns 503."
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
