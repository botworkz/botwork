// SPDX-License-Identifier: Apache-2.0

//! Workspace CRUD pages.
//!
//! After botworkz/space#311 Phase 2 tenant scoping is path-borne:
//! every workspace page lives under `/{tenant}/workspaces/*`.
//! The tenant is extracted from the route param and threaded into
//! all API calls; the old tenant-selector on the Create form is gone
//! (the workspace always belongs to the path-borne tenant).

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map};
use web_sys::SubmitEvent;

use crate::api;
use crate::pages::{render_dependents, Async, AsyncView};

#[component]
pub fn List() -> impl IntoView {
    let params = use_params_map();
    let tenant = move || params.with(|p| p.get("tenant").unwrap_or_default().to_string());
    let (state, set_state) = signal::<Async<api::ListResponse<api::Workspace>>>(Async::Loading);

    Effect::new(move |_| {
        let t = tenant();
        spawn_local(async move {
            set_state.set(match api::list_workspaces(&t).await {
                Ok(r) => Async::Loaded(r),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Workspaces"</h1>
                <A href=move || format!("/{}/workspaces/new", tenant())>"+ New"</A>
            </header>

            <AsyncView
                state=state
                children=Box::new(move |r: api::ListResponse<api::Workspace>| {
                    if r.items.is_empty() {
                        return view! {
                            <p class="empty">
                                "No workspaces yet. "
                                <A href=move || format!("/{}/workspaces/new", tenant())>
                                    "Create one"
                                </A>
                                "."
                            </p>
                        }.into_any();
                    }
                    view! {
                        <table class="entity-table">
                            <thead>
                                <tr>
                                    <th>"Name"</th>
                                    <th>"ID"</th>
                                    <th>"Created"</th>
                                </tr>
                            </thead>
                            <tbody>
                                {r.items.into_iter().map(|w| {
                                    let detail_link = format!("/{}/workspaces/{}", tenant(), w.id);
                                    view! {
                                        <tr>
                                            <td><A href=detail_link>{w.name.clone()}</A></td>
                                            <td><code>{w.id.clone()}</code></td>
                                            <td>{w.created_at.clone()}</td>
                                        </tr>
                                    }
                                }).collect_view()}
                            </tbody>
                        </table>
                    }.into_any()
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
    let (state, set_state) = signal::<Async<api::Workspace>>(Async::Loading);

    Effect::new(move |_| {
        let t = tenant();
        let id_val = id();
        spawn_local(async move {
            set_state.set(match api::get_workspace(&t, &id_val).await {
                Ok(w) => Async::Loaded(w),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Workspace"</h1>
                <p><A href=move || format!("/{}/workspaces", tenant())>"← All workspaces"</A></p>
            </header>

            <AsyncView
                state=state
                children=Box::new(move |w: api::Workspace| {
                    let edit_link = format!("/{}/workspaces/{}/edit", tenant(), w.id);
                    let delete_link = format!("/{}/workspaces/{}/delete", tenant(), w.id);
                    view! {
                        <dl class="entity-detail">
                            <dt>"Name"</dt>
                            <dd>{w.name.clone()}</dd>
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
    let params = use_params_map();
    let tenant = move || params.with(|p| p.get("tenant").unwrap_or_default().to_string());

    let (name, set_name) = signal(String::new());
    let (error, set_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    let navigate = use_navigate();
    let on_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        let t = tenant();
        let body = api::WorkspaceCreate {
            name: name.get_untracked().trim().to_string(),
        };
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
            match api::create_workspace(&t, &body).await {
                Ok(w) => navigate(&format!("/{}/workspaces/{}", t, w.id), Default::default()),
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
                <p><A href=move || format!("/{}/workspaces", tenant())>"← All workspaces"</A></p>
            </header>

            <form class="entity-form" on:submit=on_submit>
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
        </article>
    }
}

#[component]
pub fn Edit() -> impl IntoView {
    let params = use_params_map();
    let tenant = move || params.with(|p| p.get("tenant").unwrap_or_default().to_string());
    let id = Memo::new(move |_| params.read().get("id").unwrap_or_default());

    let (loaded, set_loaded) = signal::<Async<api::Workspace>>(Async::Loading);
    let (name, set_name) = signal(String::new());
    let (lock, set_lock) = signal(String::new());
    let (error, set_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    Effect::new(move |_| {
        let t = tenant();
        let id_val = id.get();
        spawn_local(async move {
            match api::get_workspace(&t, &id_val).await {
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
        let t = tenant();
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
            match api::update_workspace(&t, &id_val, &body).await {
                Ok(_) => navigate(&format!("/{}/workspaces/{}", t, id_val), Default::default()),
                Err(api::ApiError::Stale { message }) => {
                    if let Ok(fresh) = api::get_workspace(&t, &id_val).await {
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
                <p><A href=move || format!("/{}/workspaces", tenant())>"← All workspaces"</A></p>
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
    let tenant = move || params.with(|p| p.get("tenant").unwrap_or_default().to_string());
    let id = Memo::new(move |_| params.read().get("id").unwrap_or_default());

    let (loaded, set_loaded) = signal::<Async<api::Workspace>>(Async::Loading);
    let (error, set_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    Effect::new(move |_| {
        let t = tenant();
        let id_val = id.get();
        spawn_local(async move {
            set_loaded.set(match api::get_workspace(&t, &id_val).await {
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
                <p><A href=move || format!("/{}/workspaces", tenant())>"← All workspaces"</A></p>
            </header>

            <AsyncView
                state=loaded
                children=Box::new(move |w: api::Workspace| {
                    let detail_link = format!("/{}/workspaces/{}", tenant(), w.id);
                    let navigate_inner = navigate.clone();
                    let on_confirm = move |_ev: web_sys::MouseEvent| {
                        let t = tenant();
                        let id_val = id.get_untracked();
                        set_busy.set(true);
                        set_error.set(None);
                        let navigate = navigate_inner.clone();
                        spawn_local(async move {
                            match api::delete_workspace(&t, &id_val).await {
                                Ok(()) => navigate(&format!("/{}/workspaces", t), Default::default()),
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
