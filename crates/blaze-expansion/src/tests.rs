//! The crate's tests: the token hash's properties (Hegel), the deterministic simulations of
//! macro calls, script runs, the module seam and the state chain, each with planted-bug
//! controls, and the flat layout's round trips.
//!
//! An in-memory [`support::MemStore`] (the core's `Store`, every record immutable) and answer
//! tables stand in for the store's segments and the database until the wiring lands.

mod chain;
mod flat;
mod macros;
mod scripts;
mod seam;
mod support;
mod tokens;
