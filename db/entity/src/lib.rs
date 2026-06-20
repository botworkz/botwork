//! `botwork-entity` — workspace persistence-layer entry point.
//!
//! This crate holds SeaORM entity definitions and the helpers used to obtain
//! a [`sea_orm::DatabaseConnection`]. It exists so that every persistence-
//! aware consumer (config-broker, control-plane, the eventual admin-api)
//! depends on a single source of truth for the schema rather than
//! re-deriving its own.
//!
//! # v0 surface
//!
//! v0 ships **no** entity modules. The first entity arrives with the first
//! consumer cut over to the DB (see RFE 97). The crate exists now so that:
//!
//! * `botwork-migration` has a stable dependency root to grow against.
//! * The connection helper is in place for follow-up consumer work.
//! * Container-image builds can start exercising the persistence layer
//!   end-to-end before any business logic depends on it.
//!
//! # Trust posture
//!
//! v0 has a single DB role (`botwork`) used by every consumer. Per-consumer
//! roles + GRANTs are a follow-up that pays off once admin-api lands — until
//! then trust is enforced at the docker-network boundary (`botwork-internal`)
//! and at the bind-mounted credential file (`/var/lib/botwork-db/secret.env`,
//! mode 0600). The crate itself does no credential plumbing: it reads
//! `BOTWORK_DATABASE_URL` from the process environment via
//! [`connection::connect_from_env`].

pub mod connection;

pub use connection::{connect, connect_from_env, ConnectError, DATABASE_URL_ENV};
