//! `invitation` — one row per tenant invitation (OTP-gated claim credential).
//!
//! Auth-broker INSERTs a row when `POST /api/tenants` fires (the api→auth-broker
//! cold-path call), looks the row up by `(tenant_id, otp_hash)` at
//! `POST /auth/register/finish`, and sets [`Model::consumed_at`] on success.
//! See botworkz/botwork#346 for the surrounding design.
//!
//! ## Lifecycle
//!
//! ```text
//!    (create_tenant)                              (register/finish)
//!    INSERT ──► consumed_at IS NULL ──► UPDATE consumed_at ──► (audit retain)
//! ```
//!
//! ## Hash, not OTP plaintext
//!
//! [`Model::otp_hash`] is the SHA-256 of the normalised plaintext OTP
//! (dashes stripped, uppercased). The plaintext is returned once in the
//! `POST /api/tenants` response and **never stored**. A postgres dump leaks
//! invitation metadata, not usable OTPs.
//!
//! ## Single-use
//!
//! [`Model::consumed_at`] is `NULL` while the invitation is live and unclaimed.
//! The verify+consume step atomically UPDATEs to non-NULL via a conditional
//! UPDATE that checks `consumed_at IS NULL`. A claimed invitation is retained
//! for audit; the GC reaper (fast-follow, not v1) will sweep old rows.
//!
//! ## `ON DELETE` semantics
//!
//! `tenant_id → tenant.id` **CASCADE** — an invitation without a tenant is
//! meaningless. The deliberate two-step "drop workspaces first" posture still
//! lives at `workspace.tenant_id` RESTRICT.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Serialize, Deserialize)]
#[sea_orm(table_name = "invitation")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    /// FK → `tenant.id`. CASCADE on tenant delete.
    pub tenant_id: Uuid,
    /// SHA-256 of the normalised OTP (dashes stripped, uppercased), hex-encoded.
    /// Plaintext never stored.
    pub otp_hash: String,
    /// Hard ceiling; verification rejects if `now >= expires_at`.
    pub expires_at: ChronoDateTimeUtc,
    /// `NULL` while unclaimed. Set to a wall-clock timestamp on first successful
    /// verify+consume; the row then sits as terminal audit state.
    pub consumed_at: Option<ChronoDateTimeUtc>,
    /// Row creation time. Immutable.
    pub created_at: ChronoDateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    /// Belongs to a tenant. CASCADE on tenant delete — an invitation
    /// without a tenant is meaningless.
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
