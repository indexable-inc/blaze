//! astlog over blaze's own front end (docs/v2/plan/design/astlog-sema.md).
//!
//! A rules file is the engine's language (`astlog-engine`) with blaze's atoms: Rust code
//! patterns whose def paths match by name resolution ([`pattern`]), and relations the database
//! answers on demand ([`sema`]). [`query`] runs a program and returns its rows and findings;
//! [`edit`] also turns its rewrites into blaze-intent's edits and applies them ([`rewrite`]).

#![forbid(unsafe_code)]

pub mod error;
pub mod pattern;
pub mod program;
pub mod rewrite;
pub mod sema;
mod splice;

#[cfg(test)]
mod tests;

pub use error::{Error, SourceError, api_error};
pub use program::{Blaze, Program};
pub use rewrite::{edit, query};
pub use sema::{Def, Node, Sema};
