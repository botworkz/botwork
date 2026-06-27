// SPDX-License-Identifier: Apache-2.0

//! Login page.
//!
//! Renders a single form with tenant + password fields.
//! On submit, this page should drive the two-round-trip OPAQUE login
//! handshake exposed by auth-broker.
//!
//! On app boot the SPA probes `GET /api/auth/whoami` — if that
//! returns 200 the user is already authenticated and is redirected
//! to `/{tenant}/` immediately; only a 401 lands here.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_navigate;

use crate::api;

/// OPAQUE login blocker for wasm.
///
/// TODO: wire botwork-extra's OPAQUE client handshake once the dependency is
/// available and confirmed wasm32-compatible. Plaintext password POSTs to
/// `/api/auth/login` are intentionally forbidden.
async fn post_login(_tenant: String, _password: String) -> Result<String, String> {
    Err("Login is temporarily unavailable: OPAQUE wasm client dependency from botwork-extra is not accessible in this repository clone.".to_string())
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
        let tenant_val = tenant_ref.get().map(|el| el.value()).unwrap_or_default();
        let password_val = password_ref.get().map(|el| el.value()).unwrap_or_default();
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
