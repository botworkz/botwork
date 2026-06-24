// SPDX-License-Identifier: Apache-2.0

//! Plugin CRUD pages.
//!
//! Plugin is the broadest entity surface: name, image, port, path,
//! upstream_auth, env (key-value map), resources ({cpus?, memory?,
//! pids?}), egress ({allow:[{host,ports}]} or shorthand "all"/"none").
//!
//! For PR3 we render the JSON-shaped fields (env / resources /
//! egress) as raw JSON `<textarea>`s. Round-tripping through serde
//! means the operator sees the canonical form admin-api returns,
//! and edits are sent verbatim; admin-api's validator owns the
//! schema and returns `422 validation_failed` with a precise
//! message if the input is malformed.
//!
//! A future round can replace these textareas with structured
//! per-field editors (a key-value grid for `env`, a host/ports
//! repeater for `egress`, numeric inputs for `resources`). The
//! wire contract is the same; only the form surface changes.
//!
//! Plugin delete is RESTRICT — FK from workspace_plugin holds it.
//! `409 has_dependents` lists the blocking bindings inline so the
//! operator can act on them without leaving the page.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use leptos_router::hooks::{use_navigate, use_params_map};
use serde_json::Value as JsonValue;
use web_sys::SubmitEvent;

use crate::api;
use crate::pages::{render_dependents, Async, AsyncView};
use crate::ui_path;

/// Pretty-print a JSON value for display in a textarea. Falls back to
/// `null` if the input can't be serialized (which shouldn't happen
/// in practice — every input here came from admin-api's serializer).
fn pretty(v: &JsonValue) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "null".to_string())
}

/// Parse a textarea's contents as JSON, treating blank as the
/// "leave unchanged" sentinel that becomes `None` in the wire body.
/// Returns an error string the form can render directly.
fn parse_optional_json(input: &str) -> Result<Option<JsonValue>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    serde_json::from_str(trimmed)
        .map(Some)
        .map_err(|err| format!("invalid JSON: {err}"))
}

#[component]
pub fn List() -> impl IntoView {
    let (state, set_state) = signal::<Async<api::ListResponse<api::Plugin>>>(Async::Loading);

    Effect::new(move |_| {
        spawn_local(async move {
            set_state.set(match api::list_plugins().await {
                Ok(r) => Async::Loaded(r),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Plugins"</h1>
                <A href=ui_path!("/plugins/new")>
                    <button class="primary">"+ New plugin"</button>
                </A>
            </header>

            <AsyncView
                state=state
                children=Box::new(|r: api::ListResponse<api::Plugin>| {
                    if r.items.is_empty() {
                        view! {
                            <p class="muted">
                                "No plugins yet. Click "
                                <em>"New plugin"</em>
                                " to register one."
                            </p>
                        }.into_any()
                    } else {
                        view! {
                            <table class="entity-list">
                                <thead>
                                    <tr>
                                        <th>"Name"</th>
                                        <th>"Image"</th>
                                        <th>"Path"</th>
                                        <th>"Port"</th>
                                        <th>"Actions"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {r.items.into_iter().map(|p| {
                                        let detail = ui_path!("/plugins/{}", p.id);
                                        let edit = ui_path!("/plugins/{}/edit", p.id);
                                        let delete = ui_path!("/plugins/{}/delete", p.id);
                                        view! {
                                            <tr>
                                                <td><A href=detail.clone()>{p.name.clone()}</A></td>
                                                <td><code class="muted">{p.image.clone()}</code></td>
                                                <td><code class="muted">{p.path.clone()}</code></td>
                                                <td>{p.port}</td>
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
    let (state, set_state) = signal::<Async<api::Plugin>>(Async::Loading);

    Effect::new(move |_| {
        let id_val = id();
        spawn_local(async move {
            set_state.set(match api::get_plugin(&id_val).await {
                Ok(p) => Async::Loaded(p),
                Err(err) => Async::Failed(err),
            });
        });
    });

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Plugin"</h1>
                <p><A href=ui_path!("/plugins")>"← All plugins"</A></p>
            </header>

            <AsyncView
                state=state
                children=Box::new(|p: api::Plugin| {
                    let edit_link = ui_path!("/plugins/{}/edit", p.id);
                    let delete_link = ui_path!("/plugins/{}/delete", p.id);
                    let env_pretty = pretty(&p.env);
                    let res_pretty = p.resources.as_ref().map(pretty).unwrap_or_else(|| "null".to_string());
                    let egress_pretty = pretty(&p.egress);
                    view! {
                        <dl class="entity-detail">
                            <dt>"Name"</dt>
                            <dd>{p.name.clone()}</dd>
                            <dt>"Image"</dt>
                            <dd><code>{p.image.clone()}</code></dd>
                            <dt>"Port"</dt>
                            <dd>{p.port}</dd>
                            <dt>"Path"</dt>
                            <dd><code>{p.path.clone()}</code></dd>
                            <dt>"Upstream auth"</dt>
                            <dd><code>{p.upstream_auth.clone()}</code></dd>
                            <dt>"Env"</dt>
                            <dd><pre><code>{env_pretty}</code></pre></dd>
                            <dt>"Resources"</dt>
                            <dd><pre><code>{res_pretty}</code></pre></dd>
                            <dt>"Egress"</dt>
                            <dd><pre><code>{egress_pretty}</code></pre></dd>
                            <dt>"ID"</dt>
                            <dd><code>{p.id.clone()}</code></dd>
                            <dt>"Created"</dt>
                            <dd>{p.created_at.clone()}</dd>
                            <dt>"Updated"</dt>
                            <dd>{p.updated_at.clone()}</dd>
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

/// Shared form state for Create + Edit. Kept as a struct of signals
/// so the form can be built once and used in both contexts.
#[derive(Clone, Copy)]
struct PluginForm {
    name: RwSignal<String>,
    image: RwSignal<String>,
    port: RwSignal<String>,
    path: RwSignal<String>,
    upstream_auth: RwSignal<String>,
    env: RwSignal<String>,
    resources: RwSignal<String>,
    egress: RwSignal<String>,
}

impl PluginForm {
    fn new() -> Self {
        Self {
            name: RwSignal::new(String::new()),
            image: RwSignal::new(String::new()),
            port: RwSignal::new(String::new()),
            path: RwSignal::new(String::new()),
            upstream_auth: RwSignal::new(String::new()),
            env: RwSignal::new(String::new()),
            resources: RwSignal::new(String::new()),
            egress: RwSignal::new(String::new()),
        }
    }

    fn populate_from(&self, p: &api::Plugin) {
        self.name.set(p.name.clone());
        self.image.set(p.image.clone());
        self.port.set(p.port.to_string());
        self.path.set(p.path.clone());
        self.upstream_auth.set(p.upstream_auth.clone());
        self.env.set(pretty(&p.env));
        self.resources
            .set(p.resources.as_ref().map(pretty).unwrap_or_default());
        self.egress.set(pretty(&p.egress));
    }

    /// Build a PluginCreate body from the current form state.
    /// None-fields are omitted from the wire payload; admin-api fills
    /// validator defaults for absent fields. Empty strings count as
    /// None.
    ///
    /// Takes `self` by value to satisfy clippy::wrong_self_convention
    /// (a `to_*` method on a `Copy` struct should consume self
    /// rather than borrow). The implementation only `get_untracked`s
    /// the signal handles, which is the same Copy operation either
    /// way; the by-value signature is purely a convention nudge.
    fn to_create(self) -> Result<api::PluginCreate, String> {
        let name = self.name.get_untracked().trim().to_string();
        if name.is_empty() {
            return Err("name must not be blank".to_string());
        }
        let image = nonempty(&self.image.get_untracked());
        let port = parse_optional_port(&self.port.get_untracked())?;
        let path = nonempty(&self.path.get_untracked());
        let upstream_auth = nonempty(&self.upstream_auth.get_untracked());
        let env = parse_optional_json(&self.env.get_untracked())?;
        let resources = parse_optional_json(&self.resources.get_untracked())?;
        let egress = parse_optional_json(&self.egress.get_untracked())?;
        Ok(api::PluginCreate {
            name,
            image,
            port,
            path,
            upstream_auth,
            env,
            resources,
            egress,
        })
    }

    fn to_update(self, lock: String) -> Result<api::PluginUpdate, String> {
        let c = self.to_create()?;
        Ok(api::PluginUpdate {
            name: c.name,
            image: c.image,
            port: c.port,
            path: c.path,
            upstream_auth: c.upstream_auth,
            env: c.env,
            resources: c.resources,
            egress: c.egress,
            if_unmodified_since: lock,
        })
    }
}

fn nonempty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn parse_optional_port(s: &str) -> Result<Option<u64>, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<u64>()
        .map(Some)
        .map_err(|err| format!("port must be a positive integer: {err}"))
}

#[component]
pub fn Create() -> impl IntoView {
    let form = PluginForm::new();
    let (error, set_error) = signal::<Option<String>>(None);
    let (api_error, set_api_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    let navigate = use_navigate();
    let on_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        let body = match form.to_create() {
            Ok(b) => b,
            Err(msg) => {
                set_error.set(Some(msg));
                return;
            }
        };
        set_error.set(None);
        set_api_error.set(None);
        set_busy.set(true);
        let navigate = navigate.clone();
        spawn_local(async move {
            match api::create_plugin(&body).await {
                Ok(p) => navigate(&ui_path!("/plugins/{}", p.id), Default::default()),
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
                <h1>"New plugin"</h1>
                <p><A href=ui_path!("/plugins")>"← All plugins"</A></p>
            </header>

            <form class="entity-form" on:submit=on_submit>
                {plugin_form_fields(form)}
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
    let id = Memo::new(move |_| params.read().get("id").unwrap_or_default());

    let form = PluginForm::new();
    let (loaded, set_loaded) = signal::<Async<api::Plugin>>(Async::Loading);
    let (lock, set_lock) = signal(String::new());
    let (error, set_error) = signal::<Option<String>>(None);
    let (api_error, set_api_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    Effect::new(move |_| {
        let id_val = id.get();
        spawn_local(async move {
            match api::get_plugin(&id_val).await {
                Ok(p) => {
                    form.populate_from(&p);
                    set_lock.set(p.updated_at.clone());
                    set_loaded.set(Async::Loaded(p));
                }
                Err(err) => set_loaded.set(Async::Failed(err)),
            }
        });
    });

    let navigate = use_navigate();
    let on_submit = move |ev: SubmitEvent| {
        ev.prevent_default();
        let body = match form.to_update(lock.get_untracked()) {
            Ok(b) => b,
            Err(msg) => {
                set_error.set(Some(msg));
                return;
            }
        };
        set_error.set(None);
        set_api_error.set(None);
        set_busy.set(true);
        let id_val = id.get_untracked();
        let navigate = navigate.clone();
        spawn_local(async move {
            match api::update_plugin(&id_val, &body).await {
                Ok(_) => navigate(&ui_path!("/plugins/{}", id_val), Default::default()),
                Err(api::ApiError::Stale { message }) => {
                    if let Ok(fresh) = api::get_plugin(&id_val).await {
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
                <h1>"Edit plugin"</h1>
                <p><A href=ui_path!("/plugins")>"← All plugins"</A></p>
            </header>

            <AsyncView
                state=loaded
                children=Box::new(move |_p: api::Plugin| {
                    let on_submit = on_submit.clone();
                    view! {
                        <form class="entity-form" on:submit=on_submit>
                            {plugin_form_fields(form)}
                            <p class="muted lock-token">
                                "Lock token: " <code>{move || lock.get()}</code>
                            </p>
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

/// Shared form field block for Create + Edit.
fn plugin_form_fields(form: PluginForm) -> impl IntoView {
    view! {
        <label>
            <span>"Name"</span>
            <input
                type="text"
                prop:value=move || form.name.get()
                on:input:target=move |ev| form.name.set(ev.target().value())
            />
        </label>
        <label>
            <span>"Image"</span>
            <input
                type="text"
                placeholder="ghcr.io/example/plugin:1.0"
                prop:value=move || form.image.get()
                on:input:target=move |ev| form.image.set(ev.target().value())
            />
        </label>
        <label>
            <span>"Port"</span>
            <input
                type="number"
                placeholder="8000"
                prop:value=move || form.port.get()
                on:input:target=move |ev| form.port.set(ev.target().value())
            />
        </label>
        <label>
            <span>"Path"</span>
            <input
                type="text"
                placeholder="/"
                prop:value=move || form.path.get()
                on:input:target=move |ev| form.path.set(ev.target().value())
            />
        </label>
        <label>
            <span>"Upstream auth"</span>
            <input
                type="text"
                placeholder="none | bearer/<service>"
                prop:value=move || form.upstream_auth.get()
                on:input:target=move |ev| form.upstream_auth.set(ev.target().value())
            />
        </label>
        <label>
            <span>"Env (JSON)"</span>
            <textarea
                rows="4"
                placeholder=r#"[{"name": "LOG_LEVEL", "value": "info"}]"#
                prop:value=move || form.env.get()
                on:input:target=move |ev| form.env.set(ev.target().value())
            ></textarea>
        </label>
        <label>
            <span>"Resources (JSON)"</span>
            <textarea
                rows="3"
                placeholder=r#"{"cpus": 1, "memory": "4g", "pids": 1024}"#
                prop:value=move || form.resources.get()
                on:input:target=move |ev| form.resources.set(ev.target().value())
            ></textarea>
        </label>
        <label>
            <span>"Egress (JSON)"</span>
            <textarea
                rows="4"
                placeholder=r#""all" | "none" | {"allow": [{"host": "example.com", "ports": [443]}]}"#
                prop:value=move || form.egress.get()
                on:input:target=move |ev| form.egress.set(ev.target().value())
            ></textarea>
        </label>
    }
}

#[component]
pub fn DeleteConfirm() -> impl IntoView {
    let params = use_params_map();
    let id = Memo::new(move |_| params.read().get("id").unwrap_or_default());

    let (loaded, set_loaded) = signal::<Async<api::Plugin>>(Async::Loading);
    let (error, set_error) = signal::<Option<api::ApiError>>(None);
    let (busy, set_busy) = signal(false);

    Effect::new(move |_| {
        let id_val = id.get();
        spawn_local(async move {
            set_loaded.set(match api::get_plugin(&id_val).await {
                Ok(p) => Async::Loaded(p),
                Err(err) => Async::Failed(err),
            });
        });
    });

    let navigate = use_navigate();

    view! {
        <article class="page">
            <header class="page-header">
                <h1>"Delete plugin"</h1>
                <p><A href=ui_path!("/plugins")>"← All plugins"</A></p>
            </header>

            <AsyncView
                state=loaded
                children=Box::new(move |p: api::Plugin| {
                    let detail_link = ui_path!("/plugins/{}", p.id);
                    let navigate_inner = navigate.clone();
                    let on_confirm = move |_ev: web_sys::MouseEvent| {
                        let id_val = id.get_untracked();
                        set_busy.set(true);
                        set_error.set(None);
                        let navigate = navigate_inner.clone();
                        spawn_local(async move {
                            match api::delete_plugin(&id_val).await {
                                Ok(()) => navigate(ui_path!("/plugins"), Default::default()),
                                Err(err) => {
                                    set_error.set(Some(err));
                                    set_busy.set(false);
                                }
                            }
                        });
                    };
                    view! {
                        <p>
                            "You are about to delete plugin "
                            <strong>{p.name.clone()}</strong>
                            " (" <code>{p.id.clone()}</code> ")."
                        </p>
                        <p class="muted">
                            "The plugin row is FK-RESTRICTed by any workspace_plugin \
                             binding that references it. If bindings exist, the \
                             admin-api returns 409 has_dependents and names them \
                             below; delete the bindings first."
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
