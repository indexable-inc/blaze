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
//!   plants.
//! - [`draw`]: the draws tests need beyond the tape's own (floats, text, maps, options), built
//!   only from the tape's public draws, so they replay and shrink with no code of their own.
//! - [`flight`]: answers in flight, delivered by choice, oldest first when calm, gone at a kill.
//! - [`text`]: a tape as text, which a failure prints and `BLAZE_SIM_TAPE` gives back.

#![forbid(unsafe_code)]
#![feature(allocator_ext, register_tool, custom_inner_attributes)]
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

pub mod check;
pub mod control;
pub mod draw;
pub mod flight;
pub mod search;
pub mod shrink;
pub mod tape;
pub mod text;

pub use check::{Outcome, check, check_given, search};
pub use control::control;
pub use draw::Any;
pub use flight::{Flight, Nanos};
pub use search::{
    Broke, Config, Ended, Failed, Failure, Given, GivenError, Origin, Passed, Verdict,
};
pub use tape::{Choice, Int, Note, Record, Span, Tail, Tape};
