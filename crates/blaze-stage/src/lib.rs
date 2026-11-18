//! ONE TYPED STAGE PROTOCOL for the check engine (docs/v2/plan/design/stage.md; lane ST1,
//! 2026-11-18).
//!
//! The fault class (rule 54): the engine's seven stages (Units, Scripts, Outlines, Names, Heads,
//! Bodies, Lints) each wrote the phase protocol by hand: a `run_phase` of its own, a task loop of
//! its own, results held as values to the merge, failures handled its own way. On a 700-crate
//! workspace that cost a stop that waited 800 s (Heads had no stop hook), a 90 GB peak against a
//! 24 GB limit (read lists held three times as values to the merge), a build script's unkeepable
//! outputs treated as a failure (half the bodies unchecked) and failures whose cause was lost.
//!
//! The design: a stage says only what is its own ([`Stage`]: how its tasks are planned, what one
//! task does through its [`TaskCx`], what it hands back); the driver ([`machine::Phase`], a
//! sans-IO state machine) alone owns the schedule, each task's frame, the phase's arena, the
//! commit, the merge in task order, stop, status, the profile's rows and how a failure reaches
//! the tasks that wait on it. The types leave a stage no way to hold a result as a value
//! ([`TaskOut`]: `Copy + 'static`, at most [`OUT_MOST`] bytes), to lose a cause (a task's end is
//! the driver's, from what its context saw; [`machine::End::Waited`] names the root) or to ignore
//! a stop (every context call answers `Err(Stopped)` once a stop is asked, and a task that never
//! asks is failed by its stall deadline).
//!
//! # The laws (each checked by the simulation in `sim_tests`, each with a planted bug)
//!
//! - (a) every task ends in exactly one [`machine::End`];
//! - (b) every `Waited` names a root that truly `Failed`; nothing waits on an `Unkept`, and a task
//!   with an `Unkept` input is never `Made`;
//! - (c) no record is readable before its commit finished: [`machine::Effect::Merge`] follows
//!   [`machine::Event::CommitDone`];
//! - (d) after a stop no task starts, and the partial profile holds every task that ran;
//! - (e) what one task made in its worker's frame is unreachable by the next task on that worker;
//! - (f) the merge order and the committed bytes are the same for every completion order;
//! - (g) the phase's arena holds no live block when it is dropped.
//!
//! # THE MEMORY LAW, stated once
//!
//! After [`machine::Effect::DropArena`] nothing of the phase is live: every column, slot and row
//! is in the phase's arena, which the thin driver makes as a `blaze_mem::Root` refusing a live
//! block at its drop. Per task, what the driver holds after the task returns is one slot: the
//! floor is `size_of::<Out>() + size_of::<Held>()` (its out and its ranges); the measured figure
//! is the slot's size, both in [`machine::Status`] and printed by the simulation.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod cx;
pub mod machine;
mod plant;
pub mod proto;

#[cfg(test)]
mod sim_tests;

pub use cx::{Arg, Caps, Clock, Lane, Open, Place, PutRef, PutRow, RecordKind, SayRow, SpanRow, TaskCx, TaskRanges};
pub use machine::{
    Cause, CommitError, Counts, Done, Effect, End, EndKind, Event, EventKind, Held, Limits, MachineError, Phase, Refusal, Row, State,
    Status, WaitGroup,
};
pub use proto::{
    Code, Column, Deps, FAIL_MOST, Halt, OUT_MOST, Outcome, Plan, PlanCaps, PlanError, Span, Stage, Stopped, TaskFail, TaskOut,
    TaskRef, WorkerRef,
};
