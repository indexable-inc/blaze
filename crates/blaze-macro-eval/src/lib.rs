//! Proc macros run on blaze's own compiler, in process (docs/v2/plan/design/compile-time.md,
//! change 20): the macro's real code, with the real std and the real `library/proc_macro`
//! client, on the MIR evaluator, every access checked. The client's one call into its server
//! (`Closure::call`) is claimed and answered by blaze's server ([`server`]), which speaks the
//! pinned bridge's protocol on the bytes in the evaluator's memory ([`protocol`], [`wire`]).
//! Everything else the macro reaches outside its own code is a traced shim ([`os`]) or an error
//! naming the macro. Nothing runs natively and nothing goes back to another host (no-fallback).
//!
//! [`load`] gives a proc-macro crate's macros, each expanding through [`expander`]: kept in the
//! store by a key that misses no input ([`keep`]), run by [`run`].

#![deny(unsafe_code)]
#![feature(random)]

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
mod wire;

pub use expander::{flush, load};

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
