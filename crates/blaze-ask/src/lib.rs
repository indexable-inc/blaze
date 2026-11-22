//! THE CHECKER'S ONE ASK (docs/v2/plan/design/end-state.md, D161: "a task that lacks a fact waits
//! on it; nothing starts over"). The contract the checker's nine restart layers move onto
//! (audit B2 F1): blaze-typeck's `MAX_FILLS` loop, front2 check.rs's whole re-checks and its
//! constants' rounds, world_items.rs's `again`, heads.rs's step restart in `ensure_seeded`, and
//! the head making the solver re-enters through `trait_question`.
//!
//! One mechanism, three parts:
//!
//! - **The gate** ([`gate`]): before a body is entered, the item facts its text names are
//!   settled ([`GateSet`], from the names record and the outline: the heads of every path the
//!   text resolves to, the members of every trait it names or has in scope, the lang items its
//!   syntax implies). Entry takes the [`Settled`] proof (rule 74). A fact met after entry is
//!   counted by kind ([`Census::after_entry`]); one of an item record is a gap of the gate.
//! - **The ask** ([`ask`]): a fact met only while inferring is asked ONCE through
//!   [`Asker::ask`], which claims its key on the shared [`Board`] (blaze-core's claim table). The
//!   first asker makes it inline on an explicit, bounded work stack of resumable frames
//!   ([`Maker`]), never by recursion through the solver; a later one reads it, or parks on it with
//!   its whole stack kept, and is resumed ([`Asker::resume`]) when the owner publishes. The
//!   body's inference state is the caller's and is never thrown away.
//! - **Cycles** ([`AskError::Cycle`]): a need on the asker's own stack, or a wait that closes a
//!   loop through other tasks' claims (the claim table's walk), is a typed error naming every
//!   need on the loop, read from the board's rows.
//!
//! Why a crate and not a module of blaze-typeck or front2: both halves use it (typeck's solver
//! loop and front2's head making), and the engine is meant to take its [`Need`] as a kind's
//! need without translation; a small crate over blaze-core keeps it below all three and out of
//! blaze-drive's fetch and manifest dependencies.
//!
//! Sans-IO: no clock, no thread; the allocator is the caller's (rule 75).

#![no_std]
#![feature(allocator_api, default_field_values)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod ask;
pub mod counts;
pub mod gate;
pub mod need;

pub use ask::{AskError, Asked, Asker, AskerOpts, Board, Given, Maker, Plant, Step};
pub use counts::{Census, Counts};
pub use gate::{After, GateSet, Settled, Unsettled};
pub use need::{Detail, EngineKind, FACTS, Fact, Need, NeedKey, Record, Subject};

#[cfg(test)]
mod sim_tests;
