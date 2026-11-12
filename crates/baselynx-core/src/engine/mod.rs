//! Engines other than Postgres. Postgres code predates them and lives in the modules it
//! serves ([`crate::session`], [`crate::catalog`], [`crate::exec`], [`crate::edit`]).

pub mod mysql;
pub mod sqlite;
