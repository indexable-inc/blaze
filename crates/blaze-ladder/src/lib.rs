//! The `blaze run` walking skeleton (crates/blaze-cli/tests/ladder/README.md): a program's
//! source files to code running in this process, through the new layer's pieces in one
//! straight line. Each stage is the shape the request machine will ask for as a task; the
//! README lists where this crate shortcuts the design, and the list shrinks.
//!
//! | module | what | stands in for |
//! |---|---|---|
//! | [`front`] | files to items (blaze-file-items), signatures and bodies (blaze-lower), and the checker's `World` over them; names resolved here | blaze-scope and the item layer's `World` over its records |
//! | [`refusal`] | the program's files and a refusal as a finding, shared with blaze-cli's findings path | none: its home |
//! | [`place`] | every body behind its launcher slot in this process, outside symbols by `dlsym`, the entry called | blaze-run's `Parent` launching a child with lazy stubs |
//! | `run` | the straight line: lower, check, build, emit, place, call | the request machine's tasks |

#![feature(allocator_api)]
#![feature(thread_local)]

pub mod argv;
pub mod body_ids;
mod class_chain;
pub mod code_records;
pub mod declared_names;
pub mod direct_comptime;
mod emit_class;
mod callee_shown;
mod clone_shim;
mod fn_item_resolve;
pub mod crates;
mod fn_shim;
pub mod front;
pub mod front2;
pub mod library_set;
mod natives;
pub mod native_links;
pub mod panic_strategy;
pub mod personality;
pub mod place;
pub mod program_texts;
pub mod refusal;
pub mod resolve;
pub mod run;
pub mod scope;
pub mod seam;
pub mod stub;
pub mod symbols;
pub mod tls_index;
pub mod trap;

#[cfg(test)]
mod tests;
