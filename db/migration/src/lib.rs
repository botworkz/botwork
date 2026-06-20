//! `botwork-migration` — SeaORM migrator for the botwork persistence layer.
//!
//! v0 ships **no** migrations. The [`Migrator`] exists so that
//! `sea-orm-migration` will create its `seaql_migrations` tracking table on
//! first run, which is the end-to-end signal that the migrate oneshot has
//! reached postgres and run successfully.
//!
//! # The schema-only rail (see RFE 97)
//!
//! Migrations in this crate **must describe schema only**. They run
//! unconditionally on every restart of the migrate oneshot in production, so
//! anything that lands here ships to every deployment.
//!
//! Concretely, do not:
//!
//! * insert seed/fixture data (use a `botwork-tools` subcommand instead),
//! * paper over dev-vs-prod differences,
//! * conditionally branch on environment.
//!
//! This is enforced by convention, not by lint. PR review is the gate.
//!
//! # Layout when the first migration lands
//!
//! ```text
//! db/migration/src/
//! ├── lib.rs        — pub use migrations; struct Migrator;
//! ├── main.rs       — production binary; runs Migrator::up and exits.
//! └── migrations/
//!     ├── mod.rs
//!     ├── m20260620_000001_create_<table>.rs
//!     └── ...
//! ```
//!
//! Each migration file's name is `mYYYYMMDD_<seq>_<slug>.rs`, the sequence
//! resetting per day, matching the SeaORM convention. Once a migration is
//! merged it is **immutable** — destructive follow-ups land as new
//! migrations.

use sea_orm_migration::prelude::*;

/// The migrator. v0 has zero migrations; the first lands with the first
/// entity (RFE 97 follow-up).
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        // No migrations in v0. See the schema-only rail above before adding
        // anything here.
        vec![]
    }
}
