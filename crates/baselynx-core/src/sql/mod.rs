pub mod classify;
pub mod dialect;
pub mod quote;
pub mod split;

pub use dialect::Dialect;
pub use quote::{quote_ident, quote_literal, quote_qualified};
pub use split::{
    Statement, has_top_level_semicolon, has_top_level_semicolon_in, split, split_in, statement_at, statement_at_in,
};
