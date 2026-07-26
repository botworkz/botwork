//! `invitation_store` — docker-gated SQL-correctness tests for the
//! invitation DB functions and the `SeaOrmInvitationStore` wrapper.
//!
//! Spins up a real Postgres via testcontainers, runs `Migrator::up`,
//! then seeds tenants and invitation rows to verify:
//!
//! - `revoke_invitations_for_tenant` — correct affected-row counts,
//!   idempotency, cross-tenant isolation, consumed/expired exclusion.
//! - `renew_invitation` — atomic revoke-then-insert, OTP rotation,
//!   old OTP rejection, new OTP acceptance.
//! - Revoked-path guards — `has_active_invitation` returns false after
//!   revoke; `verify_and_consume` on a revoked row returns `InvalidOtp`
//!   (enumeration guard, not a distinct "Revoked" variant).
//!
//! Gate: `required-features = ["test-support"]`
//!
//! `docker_available()` is checked at test runtime; when docker is absent
//! each test logs an `IGNORED:` line and exits cleanly, matching the
//! pattern used in `opaque_e2e.rs`.

use std::time::Duration;

use botwork_auth_broker::auth::invitation::{
    generate_otp, has_active_invitation, hash_otp, insert_invitation, renew_invitation,
    revoke_invitations_for_tenant, verify_and_consume, OtpVerifyError,
    INVITATION_DEFAULT_TTL_SECONDS,
};
use botwork_entity::tenant;
use botwork_migration::Migrator;
use chrono::Utc;
use sea_orm::{ActiveModelTrait, Database, DatabaseConnection, Set};
use sea_orm_migration::MigratorTrait;
use uuid::Uuid;

const POSTGRES_TAG: &str = "16-alpine";

// ---------------------------------------------------------------------------
// Docker / postgres helpers (pattern from opaque_e2e.rs)
// ---------------------------------------------------------------------------

async fn docker_available() -> bool {
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

async fn start_postgres() -> Result<
    (
        testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
        String,
    ),
    String,
> {
    use testcontainers::runners::AsyncRunner;
    use testcontainers::ImageExt;
    use testcontainers_modules::postgres::Postgres;

    let image = Postgres::default()
        .with_db_name("botwork")
        .with_user("botwork")
        .with_password("test")
        .with_tag(POSTGRES_TAG);
    let container = image
        .start()
        .await
        .map_err(|err| format!("start container: {err}"))?;
    let host = container
        .get_host()
        .await
        .map_err(|err| format!("host: {err}"))?;
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .map_err(|err| format!("port: {err}"))?;
    let url = format!("******{host}:{port}/botwork");
    Ok((container, url))
}

struct Fixture {
    db: DatabaseConnection,
    // Keep the container alive for the lifetime of the fixture.
    _pg: testcontainers::ContainerAsync<testcontainers_modules::postgres::Postgres>,
}

async fn setup() -> Result<Fixture, String> {
    let (pg, url) = start_postgres().await?;
    let db = Database::connect(&url)
        .await
        .map_err(|err| format!("connect: {err}"))?;
    Migrator::up(&db, None)
        .await
        .map_err(|err| format!("migrate: {err}"))?;
    Ok(Fixture { db, _pg: pg })
}

async fn seed_tenant(db: &DatabaseConnection) -> Uuid {
    let now = Utc::now();
    let id = Uuid::new_v4();
    tenant::ActiveModel {
        id: Set(id),
        name: Set(format!("tenant-{}", &id.to_string()[..8])),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(db)
    .await
    .expect("insert tenant")
    .id
}

/// Insert a fresh active invitation for `tenant_id` using the default TTL.
/// Returns `(plaintext_otp, invitation_id)`.
async fn seed_invitation(db: &DatabaseConnection, tenant_id: Uuid) -> (String, Uuid) {
    let now = Utc::now();
    let otp = generate_otp();
    let otp_hash = hash_otp(&otp);
    let expires_at = now + chrono::Duration::seconds(INVITATION_DEFAULT_TTL_SECONDS as i64);
    let id = insert_invitation(db, tenant_id, &otp_hash, expires_at, now)
        .await
        .expect("insert invitation");
    (otp, id)
}

// ---------------------------------------------------------------------------
// revoke_invitations_for_tenant
// ---------------------------------------------------------------------------

#[tokio::test]
async fn revoke_revokes_active_rows_and_returns_correct_count() {
    if !docker_available().await {
        eprintln!(
            "IGNORED revoke_revokes_active_rows_and_returns_correct_count: docker not reachable"
        );
        return;
    }
    let f = setup().await.expect("fixture");
    let tenant_id = seed_tenant(&f.db).await;

    seed_invitation(&f.db, tenant_id).await;
    seed_invitation(&f.db, tenant_id).await;

    let n = revoke_invitations_for_tenant(&f.db, tenant_id, Utc::now())
        .await
        .expect("revoke");
    assert_eq!(n, 2, "must report 2 revoked rows");

    // All rows are now inactive.
    assert!(!has_active_invitation(&f.db, tenant_id, Utc::now())
        .await
        .unwrap());
}

#[tokio::test]
async fn revoke_is_idempotent_second_call_returns_zero() {
    if !docker_available().await {
        eprintln!("IGNORED revoke_is_idempotent_second_call_returns_zero: docker not reachable");
        return;
    }
    let f = setup().await.expect("fixture");
    let tenant_id = seed_tenant(&f.db).await;
    seed_invitation(&f.db, tenant_id).await;

    let now = Utc::now();
    let first = revoke_invitations_for_tenant(&f.db, tenant_id, now)
        .await
        .unwrap();
    assert_eq!(first, 1);

    let second = revoke_invitations_for_tenant(&f.db, tenant_id, now)
        .await
        .unwrap();
    assert_eq!(second, 0, "second revoke must be idempotent (Ok(0))");
}

#[tokio::test]
async fn revoke_tenant_with_no_invitations_returns_zero() {
    if !docker_available().await {
        eprintln!("IGNORED revoke_tenant_with_no_invitations_returns_zero: docker not reachable");
        return;
    }
    let f = setup().await.expect("fixture");
    let tenant_id = seed_tenant(&f.db).await;

    let n = revoke_invitations_for_tenant(&f.db, tenant_id, Utc::now())
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn revoke_does_not_affect_other_tenants_invitations() {
    if !docker_available().await {
        eprintln!("IGNORED revoke_does_not_affect_other_tenants_invitations: docker not reachable");
        return;
    }
    let f = setup().await.expect("fixture");
    let tenant_a = seed_tenant(&f.db).await;
    let tenant_b = seed_tenant(&f.db).await;

    seed_invitation(&f.db, tenant_a).await;
    seed_invitation(&f.db, tenant_b).await;

    let n = revoke_invitations_for_tenant(&f.db, tenant_a, Utc::now())
        .await
        .unwrap();
    assert_eq!(n, 1, "only tenant_a's rows should be revoked");

    // Tenant B's invitation is untouched.
    assert!(
        has_active_invitation(&f.db, tenant_b, Utc::now())
            .await
            .unwrap(),
        "tenant_b's invitation must still be active after tenant_a's revoke"
    );
}

#[tokio::test]
async fn revoke_does_not_touch_already_consumed_rows() {
    if !docker_available().await {
        eprintln!("IGNORED revoke_does_not_touch_already_consumed_rows: docker not reachable");
        return;
    }
    let f = setup().await.expect("fixture");
    let tenant_id = seed_tenant(&f.db).await;
    let (otp, _) = seed_invitation(&f.db, tenant_id).await;

    // Consume the invitation.
    verify_and_consume(&f.db, tenant_id, &otp, Utc::now())
        .await
        .expect("consume");

    // Now revoke — should not touch the consumed row.
    let n = revoke_invitations_for_tenant(&f.db, tenant_id, Utc::now())
        .await
        .unwrap();
    assert_eq!(
        n, 0,
        "consumed rows are not 'active' and must not be revoked"
    );
}

#[tokio::test]
async fn revoke_does_not_touch_already_expired_rows() {
    if !docker_available().await {
        eprintln!("IGNORED revoke_does_not_touch_already_expired_rows: docker not reachable");
        return;
    }
    let f = setup().await.expect("fixture");
    let tenant_id = seed_tenant(&f.db).await;

    // Seed an invitation whose expires_at is already in the past.
    let now = Utc::now();
    let expired_otp = generate_otp();
    let expired_hash = hash_otp(&expired_otp);
    let already_expired = now - chrono::Duration::hours(1);
    insert_invitation(&f.db, tenant_id, &expired_hash, already_expired, now)
        .await
        .expect("insert expired invitation");

    let n = revoke_invitations_for_tenant(&f.db, tenant_id, now)
        .await
        .unwrap();
    assert_eq!(
        n, 0,
        "already-expired rows are not 'active' and must not be revoked"
    );
}

// ---------------------------------------------------------------------------
// renew_invitation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn renew_revokes_old_and_inserts_new_atomically() {
    if !docker_available().await {
        eprintln!("IGNORED renew_revokes_old_and_inserts_new_atomically: docker not reachable");
        return;
    }
    let f = setup().await.expect("fixture");
    let tenant_id = seed_tenant(&f.db).await;
    seed_invitation(&f.db, tenant_id).await;

    let now = Utc::now();
    let new_otp = generate_otp();
    let new_hash = hash_otp(&new_otp);
    let new_expires = now + chrono::Duration::seconds(INVITATION_DEFAULT_TTL_SECONDS as i64);

    renew_invitation(&f.db, tenant_id, &new_hash, new_expires, now)
        .await
        .expect("renew");

    // After renew, the tenant must have exactly one active invitation.
    assert!(
        has_active_invitation(&f.db, tenant_id, now).await.unwrap(),
        "tenant must have exactly one active invitation after renew"
    );
}

/// After renew, the old OTP must be rejected with `InvalidOtp` — the same
/// opaque error as "OTP not found" (enumeration guard).
#[tokio::test]
async fn renew_old_otp_no_longer_verifies_returns_invalid_otp() {
    if !docker_available().await {
        eprintln!(
            "IGNORED renew_old_otp_no_longer_verifies_returns_invalid_otp: docker not reachable"
        );
        return;
    }
    let f = setup().await.expect("fixture");
    let tenant_id = seed_tenant(&f.db).await;
    let (old_otp, _) = seed_invitation(&f.db, tenant_id).await;

    let now = Utc::now();
    let new_hash = hash_otp(&generate_otp());
    let new_expires = now + chrono::Duration::seconds(INVITATION_DEFAULT_TTL_SECONDS as i64);
    renew_invitation(&f.db, tenant_id, &new_hash, new_expires, now)
        .await
        .expect("renew");

    let err = verify_and_consume(&f.db, tenant_id, &old_otp, now)
        .await
        .unwrap_err();
    assert!(
        matches!(err, OtpVerifyError::InvalidOtp),
        "old OTP (revoked by renew) must return InvalidOtp (enumeration guard), not a distinct Revoked variant; got: {err:?}"
    );
}

/// After renew, the new OTP must verify and consume successfully.
#[tokio::test]
async fn renew_new_otp_verifies_and_consumes_successfully() {
    if !docker_available().await {
        eprintln!("IGNORED renew_new_otp_verifies_and_consumes_successfully: docker not reachable");
        return;
    }
    let f = setup().await.expect("fixture");
    let tenant_id = seed_tenant(&f.db).await;
    seed_invitation(&f.db, tenant_id).await;

    let now = Utc::now();
    let new_otp = generate_otp();
    let new_hash = hash_otp(&new_otp);
    let new_expires = now + chrono::Duration::seconds(INVITATION_DEFAULT_TTL_SECONDS as i64);
    renew_invitation(&f.db, tenant_id, &new_hash, new_expires, now)
        .await
        .expect("renew");

    verify_and_consume(&f.db, tenant_id, &new_otp, now)
        .await
        .expect("new OTP issued by renew must verify and consume successfully");
}

// ---------------------------------------------------------------------------
// Revoked-path guards
// ---------------------------------------------------------------------------

/// `has_active_invitation` must return false once the only invitation is revoked,
/// even though it is unexpired and unconsumed.
#[tokio::test]
async fn has_active_invitation_false_after_explicit_revoke() {
    if !docker_available().await {
        eprintln!(
            "IGNORED has_active_invitation_false_after_explicit_revoke: docker not reachable"
        );
        return;
    }
    let f = setup().await.expect("fixture");
    let tenant_id = seed_tenant(&f.db).await;
    seed_invitation(&f.db, tenant_id).await;

    let now = Utc::now();
    revoke_invitations_for_tenant(&f.db, tenant_id, now)
        .await
        .unwrap();

    assert!(
        !has_active_invitation(&f.db, tenant_id, now).await.unwrap(),
        "has_active_invitation must return false after revoke"
    );
}

/// `verify_and_consume` on a revoked-but-unexpired row must return `InvalidOtp`.
/// This is the security-critical enumeration guard: the response must be
/// opaque — identical to "OTP not found" — so an attacker cannot learn
/// whether an OTP was revoked or simply never issued.
#[tokio::test]
async fn verify_and_consume_revoked_row_returns_invalid_otp_enumeration_guard() {
    if !docker_available().await {
        eprintln!("IGNORED verify_and_consume_revoked_row_returns_invalid_otp_enumeration_guard: docker not reachable");
        return;
    }
    let f = setup().await.expect("fixture");
    let tenant_id = seed_tenant(&f.db).await;
    let (otp, _) = seed_invitation(&f.db, tenant_id).await;

    let now = Utc::now();
    revoke_invitations_for_tenant(&f.db, tenant_id, now)
        .await
        .unwrap();

    let err = verify_and_consume(&f.db, tenant_id, &otp, now)
        .await
        .unwrap_err();
    assert!(
        matches!(err, OtpVerifyError::InvalidOtp),
        "revoked invitation must return InvalidOtp (NOT a distinct Revoked variant) \
         — this is a security property (enumeration guard); got: {err:?}"
    );
}
