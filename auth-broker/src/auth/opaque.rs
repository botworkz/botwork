//! `auth::opaque` — server-side OPAQUE setup persistence + tenant
//! resolution helpers.
//!
//! The OPAQUE protocol primitives themselves live in
//! `botwork_opaque_handshake`; this module owns:
//!
//! 1. `ServerSetup` materialisation — generated once at broker
//!    startup, persisted to a process-local file inside the
//!    `BOTWORK_VAULT_ROOT` directory so a broker restart preserves
//!    the same `OPAQUE` key (any other choice would invalidate every
//!    existing `opaque_password_file` row on restart).
//! 2. `PasswordFile` CRUD against [`botwork-entity::opaque_password_file`][pf-schema]
//!    — load, insert (race-safe via the `ux_opaque_password_file_tenant`
//!    UNIQUE index), and the upsert path the registration endpoints use.
//! 3. Tenant name → `tenant.id` resolution. Auth-broker speaks tenant
//!    *names* on the wire (`POST /auth/register/start {"tenant":"phlax",…}`)
//!    but every FK in the schema is `tenant.id`. The lookup is one
//!    `SELECT` per request; the round-1a hot path doesn't justify a
//!    cache yet.
//!
//! [pf-schema]: https://github.com/botworkz/botwork/blob/main/db/entity/src/opaque_password_file.rs

use std::path::{Path, PathBuf};

use botwork_entity::{opaque_password_file, tenant};
use botwork_opaque_handshake::{PasswordFile, ServerSetup};
use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue, ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, Set};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tracing::warn;
use uuid::Uuid;

/// Filename used for the persisted `ServerSetup` inside the vault
/// root. The vault crate already owns the directory at
/// `BOTWORK_VAULT_ROOT`, so co-locating the OPAQUE server setup
/// there gives us \"one directory's perms cover both\" plus a
/// natural per-deployment lifecycle.
pub const SERVER_SETUP_FILENAME: &str = "opaque_server_setup";

/// Errors returned by [`load_or_generate_server_setup`].
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("io error reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("io error writing {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("malformed OPAQUE server setup on disk at {path}")]
    Corrupt { path: PathBuf },
}

/// Load the persisted `ServerSetup` from `BOTWORK_VAULT_ROOT`, or
/// generate-and-persist a fresh one when no file exists.
///
/// Calls into the OS CSPRNG exactly once per broker installation —
/// once the file lands, subsequent restarts read the same bytes.
/// Rotation is not part of v0: changing this file invalidates every
/// `opaque_password_file` row (every registration becomes
/// unusable), so the operator has to re-run `botwork-vault init`
/// for every tenant. Documented in `auth-broker/README.md`.
///
/// Atomicity: writes via tempfile + rename so a power loss
/// mid-init cannot leave a half-written file the next boot
/// mistakes for a real setup.
pub async fn load_or_generate_server_setup(vault_root: &Path) -> Result<ServerSetup, SetupError> {
    let path = vault_root.join(SERVER_SETUP_FILENAME);

    match fs::read(&path).await {
        Ok(bytes) => ServerSetup::from_bytes(&bytes).map_err(|_| SetupError::Corrupt { path }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // Create the file. The vault root may not exist yet on a
            // fresh deploy; `create_dir_all` is idempotent.
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .await
                    // Requires OS-level fault injection (read-only parent dir);
                    // not reachable in an offline unit test.
                    .map_err(|source| SetupError::Write {
                        path: path.clone(),
                        source,
                    })?;
            }
            let setup = ServerSetup::generate(&mut rand::thread_rng());

            // tempfile + rename for atomicity. The tempfile lives in
            // the same directory as the destination so the rename is
            // intra-filesystem and atomic on POSIX.
            let tmp = path.with_extension("tmp");
            let mut handle = fs::File::create(&tmp)
                .await
                // Requires OS-level fault injection (full filesystem / permission error);
                // not reachable in an offline unit test.
                .map_err(|source| SetupError::Write {
                    path: tmp.clone(),
                    source,
                })?;
            handle
                .write_all(setup.as_bytes())
                .await
                // Requires OS-level fault injection (full filesystem / I/O error);
                // not reachable in an offline unit test.
                .map_err(|source| SetupError::Write {
                    path: tmp.clone(),
                    source,
                })?;
            handle
                .sync_all()
                .await
                // Requires OS-level fault injection (kernel fsync failure);
                // not reachable in an offline unit test.
                .map_err(|source| SetupError::Write {
                    path: tmp.clone(),
                    source,
                })?;
            drop(handle);

            // tighten perms before publishing the rename. Best-effort —
            // a filesystem that doesn't support chmod (e.g. tmpfs on
            // some configs) shouldn't fail the broker boot.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Err(err) =
                    fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await
                {
                    warn!(
                        path = %tmp.display(),
                        error = %err,
                        "failed to set opaque server setup permissions to 0600"
                    );
                }
            }

            fs::rename(&tmp, &path)
                .await
                // Requires OS-level fault injection (rename-onto-directory / cross-device);
                // not reachable in an offline unit test.
                .map_err(|source| SetupError::Write {
                    path: path.clone(),
                    source,
                })?;
            Ok(setup)
        }
        Err(source) => Err(SetupError::Read { path, source }),
    }
}

/// Look up a tenant by name. Returns `Ok(None)` for unknown
/// tenants — the caller decides whether to surface that as a
/// distinct error or fold it into the OPAQUE \"dummy\" flow for
/// enumeration resistance. `/auth/register/*` raises a 404,
/// `/auth/login/*` deliberately swallows the miss into a
/// fake-credential flow that produces a wire-shaped response.
pub async fn lookup_tenant_id_by_name<C: ConnectionTrait>(
    conn: &C,
    name: &str,
) -> Result<Option<Uuid>, DbErr> {
    let row = tenant::Entity::find()
        .filter(tenant::Column::Name.eq(name))
        .one(conn)
        .await?;
    Ok(row.map(|m| m.id))
}

/// Look up a tenant's name by its UUID. Reverse of
/// [`lookup_tenant_id_by_name`] — used by the remote-write endpoints
/// to verify that the authenticated lease's `tenant_id` matches the
/// `<tenant>` path segment before touching the vault.
pub async fn lookup_tenant_name_by_id<C: ConnectionTrait>(
    conn: &C,
    tenant_id: Uuid,
) -> Result<Option<String>, DbErr> {
    let row = tenant::Entity::find_by_id(tenant_id).one(conn).await?;
    Ok(row.map(|m| m.name))
}

/// Load the `PasswordFile` for a tenant, if one is registered.
pub async fn load_password_file<C: ConnectionTrait>(
    conn: &C,
    tenant_id: Uuid,
) -> Result<Option<PasswordFile>, DbErr> {
    let row = opaque_password_file::Entity::find()
        .filter(opaque_password_file::Column::TenantId.eq(tenant_id))
        .one(conn)
        .await?;
    Ok(match row {
        Some(model) => Some(
            PasswordFile::from_bytes(&model.password_file)
                // Bytes round-tripped through postgres `bytea` are
                // not supposed to be tampered with from outside the
                // broker; treat any failure as a hard DB-level
                // corruption rather than a wire-format problem so
                // the handler turns it into a 500.
                .map_err(|_| DbErr::Custom("malformed password_file in DB".to_string()))?,
        ),
        None => None,
    })
}

/// Errors returned by [`upsert_password_file`].
///
/// `Conflict` is the race-safety arm: `ux_opaque_password_file_tenant`
/// makes the second INSERT for the same tenant fail at the DB
/// level. The HTTP handler treats this as `409` per the issue body.
#[derive(Debug, thiserror::Error)]
pub enum UpsertError {
    #[error("a password file already exists for this tenant")]
    Conflict,
    #[error("database error: {0}")]
    Db(#[from] DbErr),
}

/// INSERT a fresh password_file row, returning [`UpsertError::Conflict`]
/// if one already exists for the tenant.
///
/// v0 ships UNIQUE-on-`tenant_id`, so we don't try to UPDATE in the
/// conflict case. A password change (re-registration) flow is its
/// own follow-up: it has to invalidate every outstanding lease for
/// the tenant before swapping the row.
pub async fn upsert_password_file<C: ConnectionTrait>(
    conn: &C,
    tenant_id: Uuid,
    password_file: &PasswordFile,
    suite_version: i32,
) -> Result<(), UpsertError> {
    let now = Utc::now();
    let model = opaque_password_file::ActiveModel {
        id: ActiveValue::Set(Uuid::new_v4()),
        tenant_id: Set(tenant_id),
        password_file: Set(password_file.as_bytes().to_vec()),
        suite_version: Set(suite_version),
        created_at: Set(now),
        updated_at: Set(now),
    };

    // `ON CONFLICT DO NOTHING` lets us distinguish "the row already
    // exists" from any other DB error without parsing error
    // messages. We then inspect the returned `LastInsertId` /
    // `rows_affected` via a follow-up SELECT (the postgres returning
    // clause sea-query renders doesn't surface the row count
    // straight back through `exec()`).
    //
    // sea-orm exposes the `DO NOTHING + zero rows inserted` case via
    // `DbErr::RecordNotInserted` rather than `Ok(_)` — see
    // SeaQL/sea-orm#1442. So our match has THREE arms, not two:
    //
    //   * `Ok(_)`                    — the insert actually wrote a row;
    //     no pre-existing record for this tenant. Definitively `Ok(())`.
    //   * `Err(RecordNotInserted)`   — a row already existed and `DO
    //     NOTHING` matched. Fall through to the re-read + byte-compare
    //     logic; either the bytes match (idempotent re-register, return
    //     `Ok(())`) or they differ (`Conflict`).
    //   * any other `Err(_)`         — a real DB error. Bubble up as `Db`.
    //
    // Treating `RecordNotInserted` as a hard DB error (which the
    // earlier version did) surfaces as a 500 instead of the 409 the
    // issue body promises, and the CLI's `AlreadyRegistered` mapping
    // never fires.
    let res = opaque_password_file::Entity::insert(model)
        .on_conflict(
            OnConflict::column(opaque_password_file::Column::TenantId)
                .do_nothing()
                .to_owned(),
        )
        .exec(conn)
        .await;

    let conflict_path = match res {
        Ok(_) => return Ok(()),
        Err(DbErr::RecordNotInserted) => true,
        Err(err) => return Err(UpsertError::Db(err)),
    };

    debug_assert!(conflict_path, "only reached via the conflict arm above");

    // Re-read the stored row. If the bytes match the supplied
    // `password_file`, the caller is re-running an idempotent
    // registration (same OPAQUE blob) and we report `Ok(())`. If they
    // differ, this is a genuine conflict and we return 409.
    let stored = opaque_password_file::Entity::find()
        .filter(opaque_password_file::Column::TenantId.eq(tenant_id))
        .one(conn)
        .await
        .map_err(UpsertError::Db)?;
    match stored {
        Some(model) if model.password_file == password_file.as_bytes() => Ok(()),
        Some(_) => Err(UpsertError::Conflict),
        None => {
            // `RecordNotInserted` plus no row visible on re-read would
            // mean somebody deleted the row between the two queries.
            // v0 has no DELETE path for this table, so this is genuinely
            // unreachable; surface as `Conflict` so the handler still
            // emits a structured 409 rather than a generic 500.
            Err(UpsertError::Conflict)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sea_orm::{DatabaseBackend, DbErr, MockDatabase, MockExecResult};
    use tempfile::tempdir;
    use uuid::Uuid;

    /// Generate a minimal but real `PasswordFile` via an offline OPAQUE
    /// registration exchange so the bytes round-trip correctly through
    /// the DB layer.
    fn make_password_file() -> PasswordFile {
        use botwork_opaque_handshake::{client, server};
        let mut rng = rand::thread_rng();
        let setup = ServerSetup::generate(&mut rng);
        let mut pw = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rng, &mut pw);
        let cr = client::registration_start(&mut rng, &pw).unwrap();
        let sr = server::registration_start(&setup, cr.request, b"test-cred").unwrap();
        let cf = client::registration_finish(&mut rng, cr.state, &pw, sr.response).unwrap();
        server::registration_finish(cf.upload)
    }

    #[tokio::test]
    async fn server_setup_persists_across_calls() {
        let dir = tempdir().unwrap();
        let first = load_or_generate_server_setup(dir.path()).await.unwrap();
        let second = load_or_generate_server_setup(dir.path()).await.unwrap();
        assert_eq!(
            first.as_bytes(),
            second.as_bytes(),
            "second call must read the persisted file rather than generate fresh bytes"
        );
        // The on-disk file should exist and contain the same bytes
        // as `first.as_bytes()`.
        let bytes = std::fs::read(dir.path().join(SERVER_SETUP_FILENAME)).unwrap();
        assert_eq!(bytes, first.as_bytes());
    }

    #[tokio::test]
    async fn server_setup_corrupt_file_surfaces_as_corrupt_error() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join(SERVER_SETUP_FILENAME),
            b"definitely-not-a-server-setup",
        )
        .unwrap();
        let err = load_or_generate_server_setup(dir.path())
            .await
            .expect_err("corrupt file must error");
        assert!(matches!(err, SetupError::Corrupt { .. }), "got {err:?}");
    }

    /// Verify that the written file has mode `0600` (owner read/write only).
    ///
    /// The `#[cfg(unix)]` guard mirrors the production chmod call: on
    /// non-Unix targets the mode concept doesn't exist and the test
    /// would compile but assert nothing meaningful.
    #[cfg(unix)]
    #[tokio::test]
    async fn server_setup_file_has_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        load_or_generate_server_setup(dir.path()).await.unwrap();
        let meta = std::fs::metadata(dir.path().join(SERVER_SETUP_FILENAME)).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "opaque_server_setup must be mode 0600, got {mode:o}"
        );
    }

    /// Passing a `vault_root` that is a regular file (not a directory)
    /// causes the `fs::read` on `<vault_root>/opaque_server_setup` to
    /// fail with an OS error that is NOT `NotFound` (typically `ENOTDIR`),
    /// exercising the `Err(source) => Err(SetupError::Read { … })` arm.
    #[tokio::test]
    async fn server_setup_read_error_on_non_directory_vault_root() {
        let dir = tempdir().unwrap();
        // Create a regular file at the path we will pass as vault_root.
        let vault_root = dir.path().join("i_am_a_file");
        std::fs::write(&vault_root, b"not a directory").unwrap();
        // Joining SERVER_SETUP_FILENAME onto a file path → ENOTDIR on read.
        let err = load_or_generate_server_setup(&vault_root)
            .await
            .expect_err("reading through a file must error");
        assert!(
            matches!(err, SetupError::Read { .. }),
            "expected SetupError::Read, got {err:?}"
        );
    }

    /// `load_password_file` returns `Err(DbErr::Custom)` when the stored
    /// bytes are not valid OPAQUE `PasswordFile` bytes (i.e. the
    /// `PasswordFile::from_bytes` call fails).
    #[tokio::test]
    async fn load_password_file_with_malformed_bytes_returns_db_err() {
        let tenant_id = Uuid::new_v4();
        let now = Utc::now();
        let bad_model = opaque_password_file::Model {
            id: Uuid::new_v4(),
            tenant_id,
            password_file: vec![0xFFu8; 8], // deliberately malformed
            suite_version: 1,
            created_at: now,
            updated_at: now,
        };
        let db = MockDatabase::new(DatabaseBackend::Postgres)
            .append_query_results(vec![vec![bad_model]])
            .into_connection();

        let err = load_password_file(&db, tenant_id)
            .await
            .expect_err("malformed bytes must surface as Err");
        assert!(
            matches!(err, DbErr::Custom(ref s) if s.contains("malformed")),
            "expected DbErr::Custom(…malformed…), got {err:?}"
        );
    }

    /// When `INSERT … ON CONFLICT DO NOTHING` returns `RecordNotInserted`
    /// but the follow-up SELECT finds no row (phantom delete race), the
    /// function returns `Err(UpsertError::Conflict)` rather than panicking.
    #[tokio::test]
    async fn upsert_password_file_phantom_race_returns_conflict() {
        let pf = make_password_file();
        let tenant_id = Uuid::new_v4();

        let db = MockDatabase::new(DatabaseBackend::Postgres)
            // rows_affected = 0 → sea-orm raises DbErr::RecordNotInserted
            .append_exec_results(vec![MockExecResult {
                last_insert_id: 0,
                rows_affected: 0,
            }])
            // re-read SELECT returns empty results (phantom delete race)
            .append_query_results(vec![Vec::<opaque_password_file::Model>::new()])
            .into_connection();

        let err = upsert_password_file(&db, tenant_id, &pf, 1)
            .await
            .expect_err("phantom race must surface as Conflict");
        assert!(
            matches!(err, UpsertError::Conflict),
            "expected UpsertError::Conflict, got {err:?}"
        );
    }

    /// Trigger the `SetupError::Write` path via `fs::File::create` by
    /// pre-creating the `.tmp` path as a directory. `File::create` of a
    /// directory returns EISDIR, which maps to
    /// `SetupError::Write { path: tmp, source }`. This covers the
    /// `path: tmp.clone()` and `source` field-expression lines inside
    /// the second `map_err` closure in `load_or_generate_server_setup`.
    ///
    /// Closures #1 (`create_dir_all` failure), #3 (`write_all` failure),
    /// #4 (`sync_all` failure), and #5 (`rename` failure) each require
    /// OS-level conditions (read-only parent, full filesystem, kernel
    /// fsync error, directory at destination) that cannot be injected
    /// reliably in a unit test without mocking the OS or the tokio I/O
    /// layer. Those 8 lines (2 per closure × 4 closures) are the only
    /// remaining uncovered lines in this file.
    #[cfg(unix)]
    #[tokio::test]
    async fn server_setup_write_error_when_tmp_path_is_a_directory() {
        let dir = tempdir().unwrap();
        // Pre-create the tmp path as a directory so File::create fails with EISDIR.
        let tmp = dir.path().join(format!("{SERVER_SETUP_FILENAME}.tmp"));
        std::fs::create_dir_all(&tmp).unwrap();

        let err = load_or_generate_server_setup(dir.path())
            .await
            .expect_err("File::create on a directory must return an error");
        assert!(
            matches!(err, SetupError::Write { .. }),
            "expected SetupError::Write, got {err:?}"
        );
    }
}
