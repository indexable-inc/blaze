//! blaze's stages on a real checkout: the harness behind `examples/stages.rs`, which runs each
//! stage of the new blaze that can run alone over a project such as tempo and prints a table of
//! what it covered, what stopped it and what it cost (docs/v2/plan/design/tempo.md).
//!
//! The library holds the one piece no stage owns: [`census`], the macro uses of a file read
//! from its tokens, which says what a project needs from blaze's procedural-macro support.

#![no_std]
#![feature(allocator_api)]

pub mod census;
