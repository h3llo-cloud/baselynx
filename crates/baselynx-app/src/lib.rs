//! baselynx: a fast desktop client for PostgreSQL, as a library. The `baselynx` binary is
//! `main.rs`; `embed` lets another application show the database workspace for connections it
//! supplies.

pub mod actions;
pub mod app_icon;
pub mod app_state;
pub mod assets;
pub mod completion;
pub mod edit_model;
pub mod embed;
pub mod format;
pub mod page_diff;
pub mod palette_rank;
pub mod row_text;
pub mod ui;
#[cfg(test)]
mod ui_tests;
pub mod value_fmt;
