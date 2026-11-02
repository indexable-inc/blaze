//! blaze's own clippy: clippy's lints and blaze's own (`blaze::`), run per body as queries of
//! the front end's database, at the levels rustc would give them, written as clippy-driver and
//! cargo write them. No clippy, rustc or cargo runs.
//!
//! [`registry`] is clippy's table of lints and groups; [`level`] decides a lint's level as
//! rustc does; [`check`] runs the lints over a crate through the queries of `query`; [`diag`]
//! is a diagnostic as data, written by [`blaze_diagnostics::emit`] as rustc writes it, in text
//! and in JSON.

pub mod baseline;
mod blaze;
pub mod check;
pub mod conf;
pub mod diag;
mod famous;
pub mod fix;
pub mod level;
mod lints;
pub mod manifest;
mod msrvs;
mod query;
pub mod registry;
pub mod serve;
mod sugg;
#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

pub use check::{Found, Setup, check};
pub use level::Level;
pub use lints::names as implemented;
