//! `botwork-migration` — SeaORM migrator for the botwork persistence layer.
//!
//! Schema (RFE #101): tenant ─1:N─ workspace ─M:N─ workspace_plugin ─N:1─ plugin.
//! See `db/entity/src/lib.rs` for the cardinality + FK-semantics rationale.
//!
//! # The schema-only rail (see RFE 97 / 101)
//!
//! Migrations in this crate **must describe schema only**. They run
//! unconditionally on every restart of the migrate oneshot in production, so
//! anything that lands here ships to every deployment.
//!
//! Concretely, do not:
//!
//! * insert seed/fixture data — that lives in `botwork-bootstrap`, invoked
//!   by the systemd unit ordered between this migrator and config-broker.
//! * paper over dev-vs-prod differences,
//! * conditionally branch on environment.
//!
//! This is enforced by convention, not by lint. PR review is the gate.
//!
//! # File layout
//!
//! Each migration file's name is `mYYYYMMDD_<seq>_<slug>.rs`, the sequence
//! resetting per day, matching the SeaORM convention. Once a migration is
//! merged it is **immutable** — destructive follow-ups land as new
//! migrations.

use sea_orm_migration::prelude::*;

pub mod m20260620_000001_create_core_tables;
pub mod m20260620_000002_extend_plugin_schema;
pub mod m20260622_000001_create_agent_session;
pub mod m20260622_000002_create_session_worker;
pub mod m20260624_000001_create_auth_tables;
pub mod m20260625_000001_create_plugin_image_facet;

/// The migrator.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m20260620_000001_create_core_tables::Migration),
            Box::new(m20260620_000002_extend_plugin_schema::Migration),
            Box::new(m20260622_000001_create_agent_session::Migration),
            Box::new(m20260622_000002_create_session_worker::Migration),
            Box::new(m20260624_000001_create_auth_tables::Migration),
            Box::new(m20260625_000001_create_plugin_image_facet::Migration),
        ]
    }
}
