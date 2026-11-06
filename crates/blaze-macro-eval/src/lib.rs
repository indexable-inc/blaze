//! Proc macros run in process, behind blaze's server of the real `library/proc_macro` bridge
//! (docs/v2/plan/design/compile-time.md, change 20). The client's one call into its server
//! (`Closure::call`) is answered by blaze's server ([`server`]), which speaks the pinned
//! bridge's protocol ([`protocol`], [`wire`]). A macro runs one of two ways:
//!
//! - as a `wasm32-wasip1` module under Wasmtime ([`wasm`], [`load_wasm`]), whose imports are
//!   the bridge's shims and the WASI calls of its std, each answered and traced;
//! - on the MIR evaluator ([`run`], [`load`]), every access checked, its system calls traced
//!   shims ([`os`]): the reference a module's output is tested against, and what a crate with
//!   no module yet expands on.
//!
//! Either way, what the macro reaches outside its own code is a traced shim or an error naming
//! the macro, and a macro that can't run one way is never run the other (no-fallback).
//!
//! Each macro expands through [`expander`]: kept in the store by a key that misses no input
//! ([`keep`]).

#![deny(unsafe_code)]
#![feature(random)]
#![feature(allocator_api)]

mod code;
mod expander;
mod guest;
mod keep;
mod lex;
mod os;
mod print;
mod protocol;
mod run;
mod server;
mod stream;
mod turn;
mod wasm;
mod wire;

pub use expander::{Macros, load, load_wasm};
pub use wasm::{GLUE_MODULE, Loads, Modules, expansion_threads, wasm_glue};

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
