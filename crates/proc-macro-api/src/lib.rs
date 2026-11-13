//! What blaze keeps of proc macros beside the engine that runs them (`blaze-macro-eval`): the
//! store of their expansions ([`kept`]) and what its in-process server speaks to a macro
//! ([`bridge`]).

#![feature(allocator_api)]
// `#[expect(blaze::..)]` names blaze's own lints; inert under rustc.
#![feature(register_tool)]
#![register_tool(blaze)]

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

pub mod bridge;
pub mod kept;
