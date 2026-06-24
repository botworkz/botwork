//! `opaque_password_file` — one OPAQUE registration "password file" per
//! tenant.
//!
//! Identity is the tenant: one tenant owns one password file. The blob
//! is the output of the OPAQUE registration handshake (see
//! [`draft-irtf-cfrg-opaque-13`] § 3.1 "Envelopes and Password Files");
//! auth-broker reads it on every login handshake to compute its half
//! of the OPAQUE protocol. It is **not** a password and **not** a
//! hash — the server cannot derive the user's password from it, and
//! that property is the whole point of OPAQUE.
//!
//! See botworkz/botwork#141 for the entity-level rationale and
//! botworkz/botwork-extra#123 for the surrounding RFE.
//!
//! ## Stored as `bytea`, not `text`
//!
//! The OPAQUE password file is binary; postgres is happier holding it
//! as `bytea` than as base64-wrapped text (no encoding overhead, no
//! UTF-8 validation on every read). Auth-broker is the only writer,
//! `opaque-ke` produces the bytes, no string interpretation ever
//! happens.
//!
//! ## `suite_version`
//!
//! v0 assumes one current OPAQUE cipher-suite per tenant. The column
//! exists from day 1 so a future suite-rotation migration is
//! `ALTER`-only: when that lands, the UNIQUE-on-`tenant_id` posture
//! will likely move to UNIQUE-on-`(tenant_id, suite_version)` with a
//! `current_suite_version` pointer next to it. We pay one int per
//! row now to keep the future migration shape friendly.
//!
//! ## `ON DELETE` semantics
//!
//! `tenant_id → tenant.id` **CASCADE** — the password file is
//! meaningless without the tenant. The two-step "deliberate tenant
//! delete" posture is enforced one layer up at `workspace.tenant_id`
//! RESTRICT, same as for [`super::agent_session`] and [`super::lease`].
//!
//! [`draft-irtf-cfrg-opaque-13`]: https://datatracker.ietf.org/doc/draft-irtf-cfrg-opaque/

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "opaque_password_file")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// FK → `tenant.id`. UNIQUE in the DB (one row per tenant in v0);
    /// the named unique index `ux_opaque_password_file_tenant` is the
    /// enforcement mechanism. CASCADE on tenant delete.
    #[sea_orm(unique)]
    pub tenant_id: Uuid,
    /// `opaque-ke` registration output. Opaque to postgres — no `@>`
    /// predicates, no GIN, plain `bytea`. See module docs.
    pub password_file: Vec<u8>,
    /// Placeholder for future cipher-suite rotation. v0 ships with `1`
    /// in every row; the future suite-rotation migration is what makes
    /// this column carry semantic weight.
    pub suite_version: i32,
    pub created_at: ChronoDateTimeUtc,
    pub updated_at: ChronoDateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    /// Belongs to a tenant. CASCADE on tenant delete (the password
    /// file has no meaning without the tenant).
    #[sea_orm(
        belongs_to = "super::tenant::Entity",
        from = "Column::TenantId",
        to = "super::tenant::Column::Id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    Tenant,
}

impl Related<super::tenant::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Tenant.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
