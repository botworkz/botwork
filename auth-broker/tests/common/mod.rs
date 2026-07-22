//! Shared test fixtures for the round-1b auth-broker integration
//! test suite.
//!
//! Why this module exists
//! ----------------------
//!
//! Round 1b deletes the legacy bearer-as-vault-password path, so
//! `AppState::with_auth(...)` is the only constructor; every test
//! that used to call `build_app_state(root, false)` needs to hand
//! the broker an [`AuthState`].
//!
//! Two flavours live here:
//!
//! 1. [`offline_auth_state`] — builds an [`AuthState`] backed by
//!    in-memory mock stores (see [`botwork_auth_broker::store::mock`]).
//!    No DB connection is ever attempted, so every test that 401s
//!    upstream of the lease lookup (`structured_401`, malformed-cap,
//!    missing-cap, expired-cap, orphaned-cap) works with no docker.
//!
//! 2. [`docker_available`] + [`spawn_lease_fixture`] — full real
//!    postgres via testcontainers. Tests that need a successful
//!    `/auth/check` (which always runs the lease lookup), or that
//!    exercise the success path of `/secrets/fetch` via the cap
//!    cohort the broker mints, use this and log-skip when docker
//!    isn't reachable.
//!
//! In-process injection
//! --------------------
//!
//! For tests that need a populated cache + cap *without* going
//! through `/auth/check` (cache lifecycle, TTL config, secrets
//! fetch in isolation, zeroize audit), this module exposes
//! [`seed_synthetic_lease`] which:
//!
//! - Generates 32 bytes of synthetic export-key material.
//! - Creates a v4 vault under `<vault_root>/<tenant>/`.
//! - Inserts a [`CacheEntry`] keyed by `cache_key(tenant, bearer)`
//!   via [`AppState::insert_cache_entry_for_test`].
//! - Mints a [`CapEntry`] referencing the cache_key + a synthetic
//!   `lease_id`, encodes the cap bytes, and returns the cap value
//!   for the caller to pass to `/secrets/fetch`.
//!
//! No DB queries, no docker, no OPAQUE round-trip — just the broker
//! seeded with the exact state it would carry after a real lease
//! validation. The seam is the same the production path uses,
//! which is the property the tests are pinning.

#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use botwork_auth_broker::auth::AuthState;
use botwork_auth_broker::caps::CAP_BYTES;
use botwork_auth_broker::store::mock::{MockLeaseStore, MockPasswordFileStore, MockTenantStore};
use botwork_auth_broker::{cache_key, AppState, CacheEntry, CAP_TTL};
use botwork_vault::{SecretEntry, SecretKey, SecretKind, UnlockedMasterKey, Vault};
use rand::Rng;
use tokio::time::Instant;

/// Build an [`AuthState`] backed by in-memory mock stores.
///
/// All mock stores are empty: lease lookups return `Miss`, tenant
/// lookups return `None`, and password-file lookups return `None`.
/// Tests that 401 *upstream* of any DB call (structured_401,
/// malformed-cap, missing-cap, …) use this constructor and need no
/// docker daemon.
///
/// Any test that needs the lease-lookup path to succeed should either
/// prime the mock stores or use the docker-gated `opaque_e2e` fixture.
pub async fn offline_auth_state() -> AuthState {
    let setup = botwork_opaque_handshake::ServerSetup::generate(&mut rand::rng());
    AuthState::from_stores(
        Arc::new(MockLeaseStore::new()),
        Arc::new(MockTenantStore::new()),
        Arc::new(MockPasswordFileStore::new()),
        setup,
    )
}

/// Probe for a reachable docker daemon. Tests gate on this and
/// log-skip with an `IGNORED:` line when it returns false, same
/// pattern `opaque_e2e` already uses.
pub async fn docker_available() -> bool {
    use testcontainers::core::WaitFor;
    use testcontainers::runners::AsyncRunner;
    use testcontainers::GenericImage;
    let probe =
        GenericImage::new("testcontainers/helloworld", "1.3.0").with_wait_for(WaitFor::seconds(1));
    match tokio::time::timeout(Duration::from_secs(5), probe.start()).await {
        Ok(Ok(container)) => {
            let _ = container.rm().await;
            true
        }
        _ => false,
    }
}

/// Synthetic seed that lets a test stand up the broker state
/// `/auth/check` would have produced after a real OPAQUE round-trip,
/// without any of the network plumbing.
///
/// Returns the URL-safe-base64 cap value the caller passes to
/// `/secrets/fetch`. The cap is bound to the same `(cache_key,
/// namespace, plugin, lease_id)` quad the production path produces,
/// so log-redaction tests pin the same logged-tuple shape.
pub struct SyntheticLease {
    pub bearer: String,
    pub cap_value: String,
    pub cap_id: [u8; CAP_BYTES],
    pub cache_key: [u8; 32],
    pub lease_id: uuid::Uuid,
}

/// Seed a synthetic lease cohort onto the broker:
///   1. Create a fresh v4 vault under `<vault_root>/<tenant>/`.
///   2. For each `(SecretKey, SecretKind, value, allowed_consumers)`
///      tuple in `secrets`, `put_secret` it into the vault.
///   3. Insert a [`CacheEntry`] keyed by `cache_key(tenant, bearer)`.
///   4. Mint a [`CapEntry`] keyed by a fresh cap_id, bound to the
///      provided `namespace` + `plugin` + a synthetic lease UUID.
pub async fn seed_synthetic_lease(
    state: &AppState,
    vault_root: &Path,
    tenant: &str,
    namespace: &str,
    plugin: &str,
    bearer: &str,
    secrets: Vec<SeedSecret>,
) -> SyntheticLease {
    use botwork_auth_broker::caps::{mint_cap_id, CapEntry};

    // 1. Stand up a v4 vault.
    let tenant_root = vault_root.join(tenant);
    let mut export_key = [0u8; 64];
    rand::rng().fill_bytes(&mut export_key);
    let suite_version = botwork_opaque_handshake::SUITE_VERSION;
    let mut vault =
        Vault::create(&tenant_root, &export_key, suite_version).expect("create v4 vault");
    for sec in secrets {
        let key = SecretKey {
            service: sec.service,
            name: sec.name,
        };
        let entry = SecretEntry {
            kind: sec.kind,
            value: sec.value,
            created_at: 0,
            updated_at: 0,
            last_used_at: None,
            tags: vec![],
            allowed_consumers: sec.allowed_consumers,
        };
        vault.put_secret(key, entry).expect("seed secret");
    }
    let master = vault
        .unlock_master(&export_key, suite_version)
        .expect("unlock fresh vault");
    drop(vault);

    // 2. Insert the cache entry the production lease-path would
    //    have produced.
    let key = cache_key(tenant, bearer);
    let now = Instant::now();
    let idle_ttl = state.ttl_config.idle_for(tenant);
    let absolute_ttl = state.ttl_config.absolute_for(tenant);
    state
        .insert_cache_entry_for_test(
            key,
            CacheEntry {
                tenant: tenant.to_string(),
                vault_root: tenant_root.clone(),
                master: clone_master(&master),
                suite_version,
                expires_at: now + absolute_ttl,
                last_used: now,
                created_at: now,
                idle_ttl,
            },
        )
        .await;

    // 3. Mint a cap referencing the seeded cache entry, the
    //    namespace/plugin the test wants to exercise, and a
    //    freshly-allocated synthetic lease UUID.
    let cap_id = mint_cap_id();
    let lease_id = uuid::Uuid::new_v4();
    state
        .insert_cap_for_test(
            cap_id,
            CapEntry {
                cache_key: key,
                namespace: namespace.to_string(),
                plugin: plugin.to_string(),
                expires_at: now + CAP_TTL,
                lease_id,
            },
        )
        .await;

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    let cap_value = URL_SAFE_NO_PAD.encode(cap_id);
    SyntheticLease {
        bearer: bearer.to_string(),
        cap_value,
        cap_id,
        cache_key: key,
        lease_id,
    }
}

/// `UnlockedMasterKey` deliberately doesn't implement `Clone` (it
/// holds 32 bytes that scrub themselves on drop, and a `Clone`
/// would silently widen the in-memory footprint). Tests that need
/// to install the same master into the broker cache AND keep a
/// copy for their own assertions go through this helper, which
/// uses the `cfg(test, feature = "test-support")`-gated
/// `from_master_bytes_for_test` constructor in `botwork-vault`.
fn clone_master(master: &UnlockedMasterKey) -> UnlockedMasterKey {
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(master.as_slice());
    UnlockedMasterKey::from_master_bytes_for_test(bytes)
}

/// Sugar struct for [`seed_synthetic_lease`]: the four fields a
/// caller needs to populate per-secret.
pub struct SeedSecret {
    pub service: String,
    pub name: String,
    pub kind: SecretKind,
    pub value: Vec<u8>,
    pub allowed_consumers: Vec<String>,
}

impl SeedSecret {
    pub fn new(service: &str, name: &str, kind: SecretKind, value: &[u8]) -> Self {
        Self {
            service: service.to_string(),
            name: name.to_string(),
            kind,
            value: value.to_vec(),
            allowed_consumers: Vec::new(),
        }
    }

    pub fn allowed_for(mut self, consumers: &[&str]) -> Self {
        self.allowed_consumers = consumers.iter().map(|s| s.to_string()).collect();
        self
    }
}

/// `Bearer <token>` helper. Several ports call it.
pub fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// Drop the broker against a tempdir vault root, using the
/// mock-store auth state, and return `(AppState, vault_root_path)`.
/// The tempdir is leaked so the test can hold the AppState past
/// the function return; tempdir cleanup is the test runner's
/// problem (the OS reaps /tmp on reboot).
pub async fn build_offline_app_state() -> (AppState, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    std::mem::forget(dir);
    let state = AppState::with_auth(path.clone(), offline_auth_state().await);
    (state, path)
}
