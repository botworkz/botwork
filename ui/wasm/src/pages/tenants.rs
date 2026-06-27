// SPDX-License-Identifier: Apache-2.0

//! Tenant CRUD pages.
//!
//! Five routes, all rendered inside [`crate::layout::Shell`]:
//!
//! * `List` — `/admin/tenants` — table + "New" + per-row links to
//!   detail/edit/delete-confirm.
//! * `Detail` — `/admin/tenants/:id` — read-only view + links to
//!   edit/delete-confirm.
//! * `Create` — `/admin/tenants/new` — name form, POST + redirect.
//! * `Edit` — `/admin/tenants/:id/edit` — form prefilled from a
//!   GET, PUT with `if_unmodified_since` token + Stale handling.
//! * `DeleteConfirm` — `/admin/tenants/:id/delete` — irreversible
//!   action gate; renders `has_dependents` dependent list when the
//!   api refuses the delete.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map};
use web_sys::SubmitEvent;

use crate::api;
use crate::pages::{render_dependents, Async, AsyncView};
use crate::ui_path;

/// List view — table of every tenant, sorted server-side by name.
#[component]
pub fn List() -> impl IntoView {
    let (state, set_state) = signal::<Async<api::ListResponse<api::Tenant>>>(Async::Loading);

    Effect::new(move |_| {
        spawn_local(async move {
            set_state.set(match api::list_tenants().await {
                Ok(r) => Async::Loaded(r),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Tenants"</h1>
                <A href=ui_path!("/tenants/new")>
                    <button class="primary">"+ New tenant"</button>
                </A>
            </header>

            <AsyncView
                state=state
                children=Box::new(|r: api::ListResponse<api::Tenant>| {
                    if r.items.is_empty() {
                        view! {
                            <p class="muted">
                                "No tenants yet. Click "
                                <em>"New tenant"</em>
                                " to create one."
                            </p>
                        }.into_any()
                    } else {
                        view! {
                            <table class="entity-list">
                                <thead>
                                    <tr>
                                        <th>"Name"</th>
                                        <th>"ID"</th>
                                        <th>"Created"</th>
                                        <th>"Updated"</th>
                                        <th>"Actions"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {r.items.into_iter().map(|t| {
                                        let id = t.id.clone();
                                        let detail = ui_path!("/tenants/{}", id);
                                        let edit = ui_path!("/tenants/{}/edit", id);
                                        let delete = ui_path!("/tenants/{}/delete", id);
                                        view! {
                                            <tr>
                                                <td><A href=detail.clone()>{t.name.clone()}</A></td>
                                                <td><code class="muted">{t.id.clone()}</code></td>
                                                <td>{t.created_at.clone()}</td>
                                                <td>{t.updated_at.clone()}</td>
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

/// Detail view — read-only inspection of one tenant.
#[component]
pub fn Detail() -> impl IntoView {
    let params = use_params_map();
    let id = move || params.read().get("id").unwrap_or_default();

    let (state, set_state) = signal::<Async<api::Tenant>>(Async::Loading);

    Effect::new(move |_| {
        let id_val = id();
        spawn_local(async move {
            set_state.set(match api::get_tenant(&id_val).await {
                Ok(t) => Async::Loaded(t),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Tenant"</h1>
                <p><A href=ui_path!("/tenants")>"← All tenants"</A></p>
            </header>

            <AsyncView
                state=state
                children=Box::new(|t: api::Tenant| {
                    let edit_link = ui_path!("/tenants/{}/edit", t.id);
                    let delete_link = ui_path!("/tenants/{}/delete", t.id);
                    view! {
                        <dl class="entity-detail">
                            <dt>"Name"</dt>
                            <dd>{t.name.clone()}</dd>
                            <dt>"ID"</dt>
                            <dd><code>{t.id.clone()}</code></dd>
                            <dt>"Created"</dt>
                            <dd>{t.created_at.clone()}</dd>
                            <dt>"Updated"</dt>
                            <dd>{t.updated_at.clone()}</dd>
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

/// Create view — bare name form. POSTs and navigates to the new
/// tenant's detail on success.
#[component]
pub fn Create() -> impl IntoView {
    let navigate = use_navigate();
    let (name, set_name) = signal(String::new());
    let (error, set_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    let on_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        let body = api::TenantCreate {
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
            match api::create_tenant(&body).await {
                Ok(t) => {
                    navigate(&ui_path!("/tenants/{}", t.id), Default::default());
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
                <h1>"New tenant"</h1>
                <p><A href=ui_path!("/tenants")>"← All tenants"</A></p>
            </header>

            <form class="entity-form" on:submit=on_submit>
                <label>
                    <span>"Name"</span>
                    <input
                        type="text"
                        prop:value=move || name.get()
                        on:input:target=move |ev| set_name.set(ev.target().value())
                        autofocus
                    />
                </label>
                {move || error.get().map(|err| {
                    let extra = match &err {
                        api::ApiError::AlreadyExists { .. } => {
                            " (a tenant with that name already exists)"
                        }
                        api::ApiError::ValidationFailed { .. } => {
                            " (validator rejected the input)"
                        }
                        _ => "",
                    };
                    view! {
                        <p class="error">{err.message().to_string()} {extra}</p>
                    }
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

/// Edit view — fetches the row, prefills the form, and PUTs with the
/// stored `updated_at` as `if_unmodified_since`. On `Stale` re-fetches
/// and shows the "this changed under you" prompt; on success
/// navigates back to the detail view.
#[component]
pub fn Edit() -> impl IntoView {
    let params = use_params_map();
    let id = Memo::new(move |_| params.read().get("id").unwrap_or_default());

    let (loaded, set_loaded) = signal::<Async<api::Tenant>>(Async::Loading);
    let (name, set_name) = signal(String::new());
    // The optimistic-lock token. We store it separately from `loaded`
    // so a Stale-retry re-fetch can update the token while the user's
    // in-flight edits stay in `name`.
    let (lock, set_lock) = signal(String::new());
    let (error, set_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    // Initial fetch.
    Effect::new(move |_| {
        let id_val = id.get();
        spawn_local(async move {
            match api::get_tenant(&id_val).await {
                Ok(t) => {
                    set_name.set(t.name.clone());
                    set_lock.set(t.updated_at.clone());
                    set_loaded.set(Async::Loaded(t));
                }
                Err(err) => set_loaded.set(Async::Failed(err)),
            }
        });
    });

    let navigate = use_navigate();
    let on_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        let body = api::TenantUpdate {
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
            match api::update_tenant(&id_val, &body).await {
                Ok(_) => {
                    navigate(&ui_path!("/tenants/{}", id_val), Default::default());
                }
                Err(api::ApiError::Stale { message }) => {
                    // Re-fetch to pick up the new lock token. We keep
                    // the user's in-flight `name` so they can resubmit
                    // (or notice that the new value matches what they
                    // typed and discard the alert).
                    if let Ok(fresh) = api::get_tenant(&id_val).await {
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
                <h1>"Edit tenant"</h1>
                <p><A href=ui_path!("/tenants")>"← All tenants"</A></p>
            </header>

            <AsyncView
                state=loaded
                children=Box::new(move |_t: api::Tenant| {
                    view! {
                        <form class="entity-form" on:submit=on_submit.clone()>
                            <label>
                                <span>"Name"</span>
                                <input
                                    type="text"
                                    prop:value=move || name.get()
                                    on:input:target=move |ev| set_name.set(ev.target().value())
                                />
                            </label>
                            <p class="muted lock-token">
                                "Lock token (if_unmodified_since): "
                                <code>{move || lock.get()}</code>
                            </p>
                            {move || error.get().map(|err| {
                                let extra = match &err {
                                    api::ApiError::Stale { .. } => {
                                        " — the row was changed by someone else; \
                                         the lock token has been refreshed, \
                                         click Save to retry"
                                    }
                                    api::ApiError::ValidationFailed { .. } => {
                                        " (validator rejected the input)"
                                    }
                                    _ => "",
                                };
                                view! {
                                    <p class="error">
                                        {err.message().to_string()} {extra}
                                    </p>
                                }
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

/// Delete confirmation — fetches the row to make the prompt
/// specific, then on confirm issues `DELETE` and navigates back to
/// the list. The interesting failure mode is `409 has_dependents`,
/// which renders the dependent list inline so the operator can act
/// on it without leaving the page.
#[component]
pub fn DeleteConfirm() -> impl IntoView {
    let params = use_params_map();
    let id = Memo::new(move |_| params.read().get("id").unwrap_or_default());

    let (loaded, set_loaded) = signal::<Async<api::Tenant>>(Async::Loading);
    let (error, set_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    Effect::new(move |_| {
        let id_val = id.get();
        spawn_local(async move {
            set_loaded.set(match api::get_tenant(&id_val).await {
                Ok(t) => Async::Loaded(t),
                Err(err) => Async::Failed(err),
            });
        });
    });

    // `confirm` is invoked from a Leptos event handler that needs
    // to be `Send + Sync` (Leptos 0.8 requires it on every AnyView
    // closure for forward compat with SSR). The closure captures
    // only `Copy` signals + a clone of `navigate`, so building a
    // fresh closure inline at each render is the simplest way to
    // satisfy that — there is nothing to share, just to re-make.
    let navigate = use_navigate();

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Delete tenant"</h1>
                <p><A href=ui_path!("/tenants")>"← All tenants"</A></p>
            </header>

            <AsyncView
                state=loaded
                children=Box::new(move |t: api::Tenant| {
                    let detail_link = ui_path!("/tenants/{}", t.id);
                    let navigate_inner = navigate.clone();
                    let on_confirm = move |_ev: web_sys::MouseEvent| {
                        let id_val = id.get_untracked();
                        set_busy.set(true);
                        set_error.set(None);
                        let navigate = navigate_inner.clone();
                        spawn_local(async move {
                            match api::delete_tenant(&id_val).await {
                                Ok(()) => {
                                    navigate(ui_path!("/tenants"), Default::default());
                                }
                                Err(err) => {
                                    set_error.set(Some(err));
                                    set_busy.set(false);
                                }
                            }
                        });
                    };
                    view! {
                        <p>
                            "You are about to delete tenant "
                            <strong>{t.name.clone()}</strong>
                            " ("
                            <code>{t.id.clone()}</code>
                            "). This is irreversible. The action also cascades \
                             to every workspace, binding, and agent_session that \
                             hangs off this tenant."
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
