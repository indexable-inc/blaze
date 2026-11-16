//! The `blaze run` walking skeleton (crates/blaze-cli/tests/ladder/README.md): a program's
//! source files to code running in this process, through the new layer's pieces in one
//! straight line. Each stage is the shape the request machine will ask for as a task; the
//! README lists where this crate shortcuts the design, and the list shrinks.
//!
//! | module | what | stands in for |
//! |---|---|---|
//! | [`lower`] | source text to signatures and bodies in blaze-typeck's form, from the parser's events | crates/blaze-lower |
//! | [`world`] | the checker's `World` over the lowered program | the item layer's `World` over its records |
//! | [`place`] | every body behind its launcher slot in this process, outside symbols by `dlsym`, the entry called | blaze-run's `Parent` launching a child with lazy stubs |
//! | `run` | the straight line: lower, check, build, emit, place, call | the request machine's tasks |

#![feature(allocator_api)]

pub mod lower;
pub mod place;
pub mod world;
