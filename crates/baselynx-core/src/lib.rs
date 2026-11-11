//! baselynx core: database, storage and export logic. No UI dependency.
//!
//! Every async function here runs its work on the core's own tokio runtime
//! ([`runtime`]) and returns a future any executor can await.

pub mod backup;
pub mod catalog;
pub mod config;
pub mod edit;
pub mod error;
pub mod exec;
pub mod export;
pub mod history;
pub mod k8s;
pub mod model;
pub mod runtime;
pub mod session;
pub mod sql;
pub mod store;
pub mod table;

pub use error::{CoreError, DbError, Result};

/// Product name, used for the window title, settings directory and keyring service.
pub const APP_NAME: &str = "baselynx";

/// Re-exported so the app can hold progress and cancel handles without depending on tokio.
pub use tokio::sync::watch;
pub use tokio_util::sync::CancellationToken;
