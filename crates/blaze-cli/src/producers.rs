//! The producers' table, made once at the entry (decision D105; docs/v2/plan/design/producer-id.md).
//!
//! Every row is the running binary's linker id (bstd `Host::build_id`), each under its row's
//! name: a record's key covers the code that made it (rules 21, 52). The function that makes each
//! kind of record is in this crate (work.rs, names/scope.rs, macros.rs, body_types.rs, program.rs)
//! and calls into its whole dependency graph, so the source closure of the crate that holds it is
//! the binary; and the binary's id also holds what no source digest does: this crate's features
//! (`sandbox` turns the proc-macro runtime on, so a build without it refuses the expansions one
//! with it makes), the profile's settings, the target. Until 2026-11-17 the rows were digests of
//! hand-named crates' sources (`producer-own`), which left out code the producing step ran: two
//! builds wrote different bytes under one key (home.rs `row_of` has the table).
//!
//! This is the one seam that changes when a producer's function moves into a crate whose closure
//! is what it runs (blaze-producers digests such a crate's closure), or when blaze builds blaze:
//! the row becomes the code identity of the producer's entry function by reflection
//! (`fn_info(f).code_id`), and nothing else moves.

use blaze_ids::producer::Producers;

/// The table: every row from the running binary's linker id.
pub fn table(build_id: &[u8]) -> Producers {
    Producers::of_build(build_id)
}
