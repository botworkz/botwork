// SPDX-License-Identifier: Apache-2.0

//! Login page.
//!
//! Renders a single form with tenant + password fields.
//! On submit, POSTs to `POST /api/auth/login` (JSON body, implemented
//! in `botwork-extra`'s auth-broker).  On success the auth-broker
//! mints an HttpOnly `botwork_cap` cookie, returns JSON with
//! `{ bearer, tenant, lease_id, expires_at }`, and the SPA
//! navigates to `/{tenant}/`.
//!
//! On app boot the SPA probes `GET /api/auth/whoami` — if that
//! returns 200 the user is already authenticated and is redirected
//! to `/{tenant}/` immediately; only a 401 lands here.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Headers, Request, RequestInit};

use crate::api;

/// JSON body for `POST /api/auth/login` (auth-broker contract).
#[derive(Debug, Serialize)]
struct LoginRequest {
    tenant: String,
    password: String,
}

/// JSON body returned by a successful `POST /api/auth/login`.
#[derive(Debug, Deserialize)]
struct LoginResponse {
    tenant: String,
    // bearer and other fields present but not needed by the SPA —
    // the browser handles the HttpOnly cookie automatically.
    #[allow(dead_code)]
    bearer: Option<String>,
}

/// Issue `POST /api/auth/login` and return the tenant name on success.
async fn post_login(tenant: String, password: String) -> Result<String, String> {
    let body = LoginRequest { tenant, password };
    let body_json = serde_json::to_string(&body).map_err(|e| e.to_string())?;

    let window = web_sys::window().ok_or("no window")?;

    let headers = Headers::new().map_err(|e| format!("{e:?}"))?;
    headers
        .set("Content-Type", "application/json")
        .map_err(|e| format!("{e:?}"))?;

    let mut opts = RequestInit::new();
    opts.set_method("POST");
    opts.set_headers(&headers);
    opts.set_body(&JsValue::from_str(&body_json));

    let req = Request::new_with_str_and_init("/api/auth/login", &opts)
        .map_err(|e| format!("{e:?}"))?;

    let resp_val = JsFuture::from(window.fetch_with_request(&req))
        .await
        .map_err(|e| format!("{e:?}"))?;
    let resp: web_sys::Response = resp_val
        .dyn_into()
        .map_err(|e| format!("{e:?}"))?;

    let status = resp.status();
    let body_text = JsFuture::from(resp.text().map_err(|e| format!("{e:?}"))?)
        .await
        .map_err(|e| format!("{e:?}"))?
        .as_string()
        .unwrap_or_default();

    if status == 200 {
        let parsed: LoginResponse =
            serde_json::from_str(&body_text).map_err(|e| e.to_string())?;
        Ok(parsed.tenant)
    } else {
        Err(format!("Login failed ({status}): {body_text}"))
    }
}

/// Login page component.
///
/// On mount, probes `/api/auth/whoami`.  If the probe returns a
/// tenant the user is already authenticated and is forwarded directly
/// to `/{tenant}/`; only a 401 stays on this page.
#[component]
pub fn Login() -> impl IntoView {
    let navigate = use_navigate();
    let nav2 = navigate.clone();

    // Whoami probe — redirect if already authenticated.
    spawn_local(async move {
        if let Some(tenant) = api::whoami().await {
            nav2(&format!("/{tenant}/"), Default::default());
        }
    });

    let tenant_ref = NodeRef::<leptos::html::Input>::new();
    let password_ref = NodeRef::<leptos::html::Input>::new();
    let error = RwSignal::new(None::<String>);

    let on_submit = move |ev: web_sys::SubmitEvent| {
        ev.prevent_default();
        let tenant_val = tenant_ref
            .get()
            .map(|el| el.value())
            .unwrap_or_default();
        let password_val = password_ref
            .get()
            .map(|el| el.value())
            .unwrap_or_default();
        let nav = navigate.clone();
        let err = error;
        spawn_local(async move {
            match post_login(tenant_val, password_val).await {
                Ok(t) => nav(&format!("/{t}/"), Default::default()),
                Err(msg) => err.set(Some(msg)),
            }
        });
    };

    view! {
        <div class="login-page">
            <h1>"Sign in"</h1>
            <form on:submit=on_submit>
                <label>
                    "Tenant"
                    <input node_ref=tenant_ref type="text" name="tenant" autocomplete="username" required />
                </label>
                <label>
                    "Password"
                    <input node_ref=password_ref type="password" name="password" autocomplete="current-password" required />
                </label>
                <button type="submit">"Sign in"</button>
            </form>
            {move || error.get().map(|msg| view! { <p class="error">{msg}</p> })}
        </div>
    }
}
