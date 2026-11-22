//! blaze's own test engine and simulation kit (docs/v2/plan/design/sim.md): a test case is a
//! function of one tape of labelled, bounded choices, in which 0 is always the calm choice.
//!
//! - [`tape`]: the tape every part of a case draws from. A prefix of given values, then a tail
//!   that is calm (zeros) or the stream of a seed; a seeded tape gives exactly what
//!   `blaze_rng::SplitMix64` of its seed gives, and records each choice in canonical form, so the
//!   all-zero tape is the quiet sequential run.
//! - [`search`](mod@search): the sans-IO engine. It proposes the cases of a run (a given tape, a
//!   given seed, or seeds `first..first + cases`), and on the first failure shrinks it and runs
//!   the least tape once more, to a [`Verdict`]. [`Broke`] is what a case says broke it: an
//!   oracle's finding or the harness's own fault.
//! - [`shrink`]: the shrinker's passes over a failing tape, and the order "simpler" means.
//! - [`check`](mod@check): the driver, the one place that reads the environment, catches a panic and
//!   prints; a test is `blaze_sim::check(Config::cases(200), |tape| ..)`.
//! - [`control`](mod@control): planted-bug controls, a run that must find the bug a control
//!   plants. Typed (D165): [`controls`] over a [`plants!`] enum, each plant naming the invariant
//!   of the oracle's error type it must trip ([`Broken`]); the text form [`control`](fn@control)
//!   is deprecated.
//! - [`limits`]: a component's caps drawn by a simulation ([`Limits`], `Tape::limit`) and typed
//!   reach points ([`points!`], `Tape::reach`, `Config::must_reach_points`).
//! - [`kind`]: a test's kind in its path ([`kind_test!`]), and [`simulate`], a simulation's entry
//!   point, which fails a draw that never varied.
//! - [`draw`]: the draws tests need beyond the tape's own (floats, text, maps, options), built
//!   only from the tape's public draws, so they replay and shrink with no code of their own.
//! - [`flight`]: answers in flight, delivered by choice, oldest first when calm, gone at a kill.
//! - [`text`]: a tape as text, which a failure prints and `BLAZE_SIM_TAPE` gives back.

#![forbid(unsafe_code)]
// `variant_count`: `controls` checks that a plant enum's `ALL` lists every variant but `NONE`.
#![feature(allocator_ext, register_tool, custom_inner_attributes, variant_count)]
// `#![blaze::sans_io]` in each sans-IO module; inert under rustc.
#![register_tool(blaze)]
#![blaze::trust(
    unsafe,
    reason = "std's `thread_local!`, which the driver's panic hook keeps its depth and the caught panic in, expands to an `unsafe` block; the crate writes none (`forbid(unsafe_code)`)"
)]
#![expect(
    blaze::hot_alloc,
    reason = "a test engine: a tape's choices, a search's candidates and a flight's answers live on the test process's heap and are kept across cases, cleared and not freed; a test process has no frame to name"
)]
#![expect(
    blaze::implicit_allocator,
    reason = "the same heap: notes, failure texts and the report are `String`s for a person; blaze-sim sits below blaze-mem and cannot name its `String<A>`"
)]

pub mod any_in;
pub mod board;
pub mod check;
// `#[macro_use]` on the three modules that define D165's exported macros (`plants!`, `kind_test!`,
// `points!`): the crate's own tests (`mod tests`, declared after them) name the macros by textual
// scope, since `crate::plants!` is refused inside the defining crate
// (`macro_expanded_macro_exports_accessed_by_absolute_paths`; kept.rs records the same for
// `sim_kept!`). Other crates name them as `blaze_sim::plants!`.
#[macro_use]
pub mod control;
pub mod differ;
pub mod draw;
mod edge_ints;
mod edge_text;
pub mod flight;
pub mod guided;
pub mod kept;
#[macro_use]
pub mod kind;
#[macro_use]
pub mod limits;
pub mod observe;
pub mod search;
pub mod shape;
pub mod shrink;
pub mod tape;
pub mod text;

#[cfg(test)]
mod tests;

pub use check::{Outcome, check, check_given, search};
pub use kept::check_kept;
pub use control::{
    ControlsFailed, Expect, MOST_PLANTS, NoPlant, Plant, Planted, Plants, Refusal, control,
    controls,
};
pub use kind::{Kind, simulate};
pub use limits::{Limits, Point, PointId};
pub use observe::ObserveMode;
pub use differ::{Answer, Approved, Differ, Pair, Side, differ, differ_all};
pub use any_in::AnyIn;
pub use draw::Any;
pub use flight::{Flight, Nanos};
pub use search::{
    Broke, Broken, Config, Ended, Failed, Failure, Given, GivenError, Origin, Passed, Verdict,
};
pub use tape::{Choice, Int, Note, Record, Span, Tail, Tape};
