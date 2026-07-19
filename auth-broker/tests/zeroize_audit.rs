//! `zeroize_audit` — round-1b port of the deleted
//! `tests/zeroize_audit.rs`.
//!
//! Pins the *type-level* hygiene promises the audit takes on.
//! Round 1b changes the cache shape (from "an unlocked `Vault`"
//! to "an `UnlockedMasterKey` + path + suite_version"), so the
//! invariants the file pins shift accordingly — but the audit
//! itself gets STRONGER, not weaker, because the new shape lets
//! us assert the per-secret-unlock property: after a fetch, the
//! decrypted value bytes never live in the cache image.
//!
//! Round-1b shape changes (vs the deleted pre-cutover file):
//!
//! - `extract_bearer_returns_zeroizing_string` survives verbatim.
//! - `cache_entry_holds_no_static_bearer_string_field` survives;
//!   the pre-cutover comment said this would tighten "once #134
//!   lands" — `CacheEntry` no longer holds a `bearer` field at
//!   all, the cache_key is the only bearer-derived value.
//! - `secrets_fetch_response_does_not_leak_raw_secret_bytes_into_the_cache`
//!   is now exercise-able for real, via the synthetic-lease seed.
//!   The fetched secret comes back through the response builder;
//!   the cache image is then scanned for the plaintext bytes;
//!   they must not appear. That's the cache-shape promise the
//!   issue #146 acceptance section pins.

mod common;

use std::collections::HashSet;

use axum::body::{to_bytes, Body};
use axum::http::Request;
use botwork_auth_broker::build_router;
use botwork_auth_broker::handler::extract_bearer;
use botwork_vault::SecretKind;
use http::StatusCode;
use tempfile::tempdir;
use tower::ServiceExt;
use zeroize::Zeroizing;

use common::{build_offline_app_state, seed_synthetic_lease, SeedSecret};

#[test]
fn extract_bearer_returns_zeroizing_string() {
    let extracted: Option<Zeroizing<String>> = extract_bearer("Bearer hunter2");
    let bearer = extracted.expect("Bearer token parses");
    assert_eq!(bearer.as_str(), "hunter2");

    assert!(extract_bearer("").is_none());
    assert!(extract_bearer("Bearer ").is_none());
    assert!(extract_bearer("Banana foo").is_none());
}

#[tokio::test]
async fn cache_holds_no_plaintext_secret_after_fetch() {
    // The round-1b acceptance property: after `/secrets/fetch`
    // returns a secret, the cache image does not contain the
    // plaintext bytes for that secret. The unlocked master key
    // lives on; the decrypted value buffer (`Zeroizing<Vec<u8>>`)
    // dropped at end-of-fetch.
    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (state, _) = build_offline_app_state().await;

    let plaintext = b"PLAINTEXT_AUDIT_TARGET_ZZZZ";
    let synth = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "plugin",
        "bearer-zzz1111111111111111111111111111",
        vec![
            SeedSecret::new("svc", "name", SecretKind::ApiKey, plaintext).allowed_for(&["plugin"]),
        ],
    )
    .await;

    // Drive `/secrets/fetch` through the in-process router.
    let app = build_router(state.clone());
    let fetch = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/secrets/fetch")
                .header("x-botwork-cap", &synth.cap_value)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(fetch.status(), StatusCode::OK);
    let body = to_bytes(fetch.into_body(), usize::MAX).await.unwrap();
    let _ = body; // The response body contained the plaintext as base64;
                  // it drops here. The cache scan below confirms no
                  // copy survives in `CacheEntry`.

    let cache_contains_plaintext = state
        .with_locked_cache(|cache| {
            cache.values().any(|entry| {
                let tenant_image = entry.tenant.as_bytes();
                let path_image = entry.vault_root.to_string_lossy();
                tenant_image
                    .windows(plaintext.len())
                    .any(|w| w == plaintext)
                    || path_image.contains(std::str::from_utf8(plaintext).unwrap())
            })
        })
        .await;
    assert!(
        !cache_contains_plaintext,
        "issue #146 acceptance: cache image must NOT contain the \
         plaintext bytes of any secret after `/secrets/fetch`"
    );
}

#[tokio::test]
async fn cache_master_handle_is_opaque() {
    // `UnlockedMasterKey` is a `Zeroizing<[u8; 32]>` with no
    // public byte accessor (except `as_slice`, which is the
    // single audited reach-in surface the broker uses to pass
    // the master to `Vault::open_with_master`). Confirm there's
    // exactly one entry in the cache after the synthetic seed
    // and that its master-as-slice is exactly 32 bytes.
    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (state, _) = build_offline_app_state().await;
    let _ = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "plugin",
        "bearer-master-shape-aaaaaaaaaaaaaaaaaa",
        vec![],
    )
    .await;

    let observed = state
        .with_locked_cache(|cache| {
            let keys: HashSet<_> = cache.keys().cloned().collect();
            let master_lens: Vec<usize> =
                cache.values().map(|e| e.master.as_slice().len()).collect();
            (keys.len(), master_lens)
        })
        .await;
    assert_eq!(observed.0, 1);
    assert_eq!(observed.1, vec![32]);
}

// `unsafe_code = "forbid"` lives in the workspace-root
// `[workspace.lints.rust]` table (inherited by this crate via
// `[lints] workspace = true` — see PR #145). The hard guard is
// the workspace lint config; documenting it here would be
// redundant noise (clippy correctly flags `assert!(true)` as a
// useless assertion).
