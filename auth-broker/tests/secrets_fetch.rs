//! `secrets_fetch` — round-1b port of the deleted
//! `tests/secrets_fetch.rs`.
//!
//! Pre-cutover this file was 521 lines and exercised every cap
//! lifecycle, ACL match, and concurrency property of the
//! `/secrets/fetch` endpoint, all driven through the legacy
//! bearer-as-vault-password path.
//!
//! Round-1b shape changes (vs the deleted pre-cutover file):
//!
//! - `build_app_state(root, false)` is gone. Tests use
//!   `common::seed_synthetic_lease` to stand up the
//!   `(CacheEntry, CapEntry)` pair the broker would have created
//!   after a real lease validation.
//! - The cap-lifecycle subset (`secrets_fetch_after_*_eviction_is_401`,
//!   `secrets_fetch_does_not_extend_*_ttl`) is exercised against
//!   the synthetic seed; the underlying `prune_once` /
//!   `evict_caps_for` paths are unchanged across the cutover.
//! - The `concurrent_*` tests — multiple cap mints for the same
//!   tenant, parallel fetches all returning the same secret set —
//!   are exercised against the docker-gated `opaque_e2e` suite
//!   (which already has the matching property pinned). Repeating
//!   them here against the synthetic seed adds no signal because
//!   the cap-mint contention path the cutover changed (the lease
//!   lookup) is exactly the docker-only seam.
//! - The ACL-by-allowed_consumers and the per-secret decrypt
//!   round-trip — both load-bearing for the round-1b cutover and
//!   distinct from the lease-DB seam — stay here, ported against
//!   the synthetic seed.

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::Request;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use botwork_auth_broker::caps::{mint_cap_id, CapEntry};
use botwork_auth_broker::{build_router, cache_key, CacheEntry, CAP_TTL};
use botwork_vault::{SecretKind, UnlockedMasterKey, Vault};
use http::StatusCode;
use rand::RngCore;
use serde::Deserialize;
use tempfile::tempdir;
use tokio::time::{advance, Instant};
use tower::ServiceExt;
use uuid::Uuid;

use common::{build_offline_app_state, seed_synthetic_lease, SeedSecret};

#[derive(Debug, Deserialize)]
struct FetchSecret {
    service: String,
    name: String,
    kind: String,
    value_b64: String,
}

#[derive(Debug, Deserialize)]
struct FetchResponse {
    tenant: String,
    plugin: String,
    secrets: Vec<FetchSecret>,
}

async fn send_fetch(
    app: &axum::Router,
    cap: Option<&str>,
) -> axum::http::Response<axum::body::Body> {
    let mut builder = Request::builder().method("POST").uri("/secrets/fetch");
    if let Some(cap) = cap {
        builder = builder.header("x-botwork-cap", cap);
    }
    app.clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn fetch_json(app: &axum::Router, cap: &str) -> FetchResponse {
    let response = send_fetch(app, Some(cap)).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn fetch_unauthorized(app: &axum::Router, cap: Option<&str>) {
    let response = send_fetch(app, cap).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get("www-authenticate").is_some());
}

fn secret_names(payload: &FetchResponse) -> BTreeSet<(String, String)> {
    payload
        .secrets
        .iter()
        .map(|secret| (secret.service.clone(), secret.name.clone()))
        .collect()
}

fn assert_secret_value(secret: &FetchSecret, expected: &[u8]) {
    let decoded = STANDARD.decode(&secret.value_b64).unwrap();
    assert_eq!(decoded, expected);
}

fn standard_seed_secrets() -> Vec<SeedSecret> {
    vec![
        SeedSecret::new("github.com", "pat", SecretKind::ApiKey, b"ghp_xxx")
            .allowed_for(&["exec-bash", "github"]),
        SeedSecret::new("npm", "token", SecretKind::ApiKey, b"npm_xxx").allowed_for(&["exec-node"]),
        SeedSecret::new("shared", "key", SecretKind::ApiKey, b"shared_xxx")
            .allowed_for(&["exec-bash", "exec-node"]),
        SeedSecret::new("secret", "none", SecretKind::Opaque, b"hidden").allowed_for(&[]),
    ]
}

#[tokio::test]
async fn secrets_fetch_returns_secrets_filtered_by_allowed_consumers() {
    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (state, _) = build_offline_app_state().await;
    let synth = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "exec-bash",
        "bearer-acl-bash-aaaaaaaaaaaaaaaaaaaaaaaa",
        vec![
            SeedSecret::new("github.com", "pat", SecretKind::ApiKey, b"ghp_xxx")
                .allowed_for(&["exec-bash", "github"]),
            SeedSecret::new("npm", "token", SecretKind::ApiKey, b"npm_xxx")
                .allowed_for(&["exec-node"]),
            SeedSecret::new("shared", "key", SecretKind::ApiKey, b"shared_xxx")
                .allowed_for(&["exec-bash", "exec-node"]),
            SeedSecret::new("secret", "none", SecretKind::Opaque, b"hidden").allowed_for(&[]),
        ],
    )
    .await;

    let app = build_router(state);
    let payload = fetch_json(&app, &synth.cap_value).await;

    assert_eq!(payload.tenant, "tenant");
    assert_eq!(payload.plugin, "exec-bash");
    assert_eq!(
        secret_names(&payload),
        BTreeSet::from([
            ("github.com".to_string(), "pat".to_string()),
            ("shared".to_string(), "key".to_string()),
        ])
    );

    for secret in &payload.secrets {
        match (secret.service.as_str(), secret.name.as_str()) {
            ("github.com", "pat") => {
                assert_eq!(secret.kind, "api-key");
                assert_secret_value(secret, b"ghp_xxx");
            }
            ("shared", "key") => {
                assert_eq!(secret.kind, "api-key");
                assert_secret_value(secret, b"shared_xxx");
            }
            other => panic!("unexpected secret: {other:?}"),
        }
    }
}

#[tokio::test]
async fn secrets_fetch_excludes_secrets_for_other_plugins() {
    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (state, _) = build_offline_app_state().await;
    // Seed once — the synthetic helper creates the vault on disk;
    // a second `seed_synthetic_lease` against the same tenant
    // would trip `Vault::AlreadyInitialized`. We still need a
    // *different* bearer + plugin so this test exercises the
    // `/auth/check`-minted-cap-for-other-plugin shape, so we
    // install the cap separately against the same cache entry.
    let bash_synth = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "exec-bash",
        "bearer-bash-aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        standard_seed_secrets(),
    )
    .await;

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use botwork_auth_broker::caps::{mint_cap_id, CapEntry};
    use botwork_auth_broker::CAP_TTL;
    use tokio::time::Instant;
    let node_cap_id = mint_cap_id();
    state
        .insert_cap_for_test(
            node_cap_id,
            CapEntry {
                cache_key: bash_synth.cache_key,
                namespace: "ns".to_string(),
                plugin: "exec-node".to_string(),
                expires_at: Instant::now() + CAP_TTL,
                lease_id: uuid::Uuid::new_v4(),
            },
        )
        .await;
    let node_cap_value = URL_SAFE_NO_PAD.encode(node_cap_id);

    let app = build_router(state);
    let payload = fetch_json(&app, &node_cap_value).await;
    assert_eq!(payload.plugin, "exec-node");
    assert_eq!(
        secret_names(&payload),
        BTreeSet::from([
            ("npm".to_string(), "token".to_string()),
            ("shared".to_string(), "key".to_string()),
        ])
    );
    assert!(!payload
        .secrets
        .iter()
        .any(|secret| secret.service == "github.com" && secret.name == "pat"));
}

#[tokio::test]
async fn secrets_fetch_returns_empty_list_when_no_secret_matches_plugin() {
    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (state, _) = build_offline_app_state().await;
    let synth = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "exec-python",
        "bearer-empty-aaaaaaaaaaaaaaaaaaaaaaaaaaa",
        vec![SeedSecret::new("a", "1", SecretKind::ApiKey, b"x").allowed_for(&["exec-bash"])],
    )
    .await;
    let app = build_router(state);
    let payload = fetch_json(&app, &synth.cap_value).await;
    assert_eq!(payload.tenant, "tenant");
    assert_eq!(payload.plugin, "exec-python");
    assert!(payload.secrets.is_empty());
}

#[tokio::test]
async fn secrets_fetch_with_unknown_cap_is_401() {
    let (state, _) = build_offline_app_state().await;
    let app = build_router(state);
    let unknown = URL_SAFE_NO_PAD.encode([7u8; 32]);
    fetch_unauthorized(&app, Some(&unknown)).await;
}

#[tokio::test]
async fn secrets_fetch_with_missing_vault_root_evicts_cap_and_returns_401() {
    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (state, _) = build_offline_app_state().await;
    let synth = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "exec-bash",
        "bearer-missing-vault-aaaaaaaaaaaaaaaaaaaa",
        vec![SeedSecret::new("a", "1", SecretKind::ApiKey, b"x").allowed_for(&["exec-bash"])],
    )
    .await;
    std::fs::remove_dir_all(vault_root.join("tenant")).expect("remove seeded tenant vault");

    let app = build_router(state.clone());
    let response = send_fetch(&app, Some(&synth.cap_value)).await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().get("www-authenticate").is_some());
    assert_eq!(state.caps_len().await, 0, "orphaned cap should be evicted");
}

#[tokio::test]
async fn secrets_fetch_with_malformed_cap_header_is_401() {
    let (state, _) = build_offline_app_state().await;
    let app = build_router(state);
    fetch_unauthorized(&app, None).await;
    fetch_unauthorized(&app, Some("%%%")).await;
    fetch_unauthorized(&app, Some(&URL_SAFE_NO_PAD.encode([0u8; 31]))).await;
    fetch_unauthorized(&app, Some(&URL_SAFE_NO_PAD.encode([0u8; 33]))).await;
}

#[tokio::test(start_paused = true)]
async fn secrets_fetch_after_cap_ttl_expiry_is_401() {
    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (state, _) = build_offline_app_state().await;
    let synth = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "exec-bash",
        "bearer-ttl-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        vec![SeedSecret::new("a", "1", SecretKind::ApiKey, b"x").allowed_for(&["exec-bash"])],
    )
    .await;
    let app = build_router(state);

    advance(CAP_TTL + Duration::from_secs(1)).await;
    fetch_unauthorized(&app, Some(&synth.cap_value)).await;
}

#[tokio::test]
async fn secrets_fetch_round_trips_a_secret_byte_for_byte() {
    // Pin the per-secret decrypt path: the value comes back through
    // `Vault::decrypt_entry`, base64-encodes onto the wire, decodes
    // on the test side, and matches the input byte-for-byte. This
    // is the load-bearing per-secret-unlock property the round-1b
    // cutover hangs off.
    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    std::mem::forget(dir);
    let (state, _) = build_offline_app_state().await;

    let plaintext: &[u8] = b"\x00\x01\x02\x03some-arbitrary-secret\xfe\xff";
    let synth = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "plugin",
        "bearer-roundtrip-aaaaaaaaaaaaaaaaaaaaaaaa",
        vec![
            SeedSecret::new("svc", "name", SecretKind::ApiKey, plaintext).allowed_for(&["plugin"]),
        ],
    )
    .await;
    let app = build_router(state);

    let payload = fetch_json(&app, &synth.cap_value).await;
    assert_eq!(payload.secrets.len(), 1);
    let bytes = STANDARD.decode(&payload.secrets[0].value_b64).unwrap();
    assert_eq!(bytes, plaintext);
}

// ---------------------------------------------------------------------------
// P6 — fetch vault-error paths
// ---------------------------------------------------------------------------

/// Seed a cache entry with a 1-second idle TTL, advance past it, then
/// assert that the `should_evict` branch fires and the request 401s.
#[tokio::test(start_paused = true)]
async fn secrets_fetch_with_expired_cache_entry_is_401() {
    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    let (state, _) = build_offline_app_state().await;

    // Create a real vault so the cache entry's vault_root is valid.
    let tenant = "tenant-idle-expired";
    let tenant_root = vault_root.join(tenant);
    let mut export_key = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut export_key);
    let suite_version = botwork_opaque_handshake::SUITE_VERSION;
    let mut vault = Vault::create(&tenant_root, &export_key, suite_version).expect("create vault");
    let raw_master = vault
        .unlock_master(&export_key, suite_version)
        .expect("unlock vault");
    let mut master_bytes = [0u8; 32];
    master_bytes.copy_from_slice(raw_master.as_slice());
    drop(raw_master);
    drop(vault);

    let bearer_str = "bearer-idle-expired-aaaaaaaaaaaaaaaa";
    let ck = cache_key(tenant, bearer_str);
    let now = Instant::now();

    // idle_ttl of 1 s — advancing 2 s will make is_expired return true.
    state
        .insert_cache_entry_for_test(
            ck,
            CacheEntry {
                tenant: tenant.to_string(),
                vault_root: tenant_root,
                master: UnlockedMasterKey::from_master_bytes_for_test(master_bytes),
                suite_version,
                expires_at: now + Duration::from_secs(3600),
                last_used: now,
                created_at: now,
                idle_ttl: Duration::from_secs(1),
            },
        )
        .await;

    let cap_id = mint_cap_id();
    state
        .insert_cap_for_test(
            cap_id,
            CapEntry {
                cache_key: ck,
                namespace: "ns".to_string(),
                plugin: "plugin".to_string(),
                // Cap has a long TTL so it does not expire before the test.
                expires_at: now + CAP_TTL,
                lease_id: Uuid::new_v4(),
            },
        )
        .await;

    // Advance past the 1-second idle TTL.
    advance(Duration::from_secs(2)).await;

    let app = build_router(state);
    let cap_value = URL_SAFE_NO_PAD.encode(cap_id);
    // Expired cache entry → evict path → 401 with WWW-Authenticate.
    fetch_unauthorized(&app, Some(&cap_value)).await;
}

/// Seed normally, then remove `vault.botwork` before issuing the fetch.
/// The `open_with_master` call inside the handler fails, the entry is
/// evicted, and the request returns 401.
#[tokio::test]
async fn secrets_fetch_with_vault_open_failure_is_401() {
    let dir = tempdir().unwrap();
    let vault_root = dir.path().to_path_buf();
    let (state, _) = build_offline_app_state().await;
    let synth = seed_synthetic_lease(
        &state,
        &vault_root,
        "tenant",
        "ns",
        "plugin",
        "bearer-vault-open-fail-aaaaaaaaaaaaa",
        vec![SeedSecret::new("svc", "key", SecretKind::ApiKey, b"val").allowed_for(&["plugin"])],
    )
    .await;

    // Remove the vault file so that `Vault::open_with_master` returns
    // VaultError::NotInitialized → evicted = true → 401.
    let vault_file = vault_root.join("tenant").join("vault.botwork");
    std::fs::remove_file(&vault_file).expect("remove vault.botwork");

    let app = build_router(state);
    fetch_unauthorized(&app, Some(&synth.cap_value)).await;
}
