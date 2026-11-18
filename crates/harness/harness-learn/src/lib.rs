//! Active learning for the harness app, as plain state machines with no IO, clock or randomness of their own.
//!
//! What a person learns here is a [`Course`]: a path of short [`Lesson`]s, each a handful of [`Step`]s. A step either
//! tells one small idea ([`Step::Read`], a few sentences at most) or asks for it back ([`Step::Choice`],
//! [`Step::Fill`], [`Step::Order`]). The design follows what is known to make learning stick:
//!
//! - **Small steps, one idea each.** A lesson never tells more than a screen before it asks.
//! - **Retrieval, not rereading.** Most steps are questions; an idea is asked back right after it is told and again
//!   later.
//! - **Feedback at once.** Every answer is marked and explained (`why`) before the next step.
//! - **Miss it, meet it again.** A missed check comes back a few steps later in the same run, until it is answered
//!   right ([`Run`]).
//! - **Spacing.** Every check has a [`Memory`]: when it is next due, pushed further out each time it is recalled and
//!   pulled back in when it is missed ([`Memory::review`]). What is due is a review run ([`Progress::due`]).
//! - **Mastery is visible.** A lesson's and a course's share learned come from those memories ([`Progress::mastery`]).
//!
//! The host draws a [`Run`]'s [`View`], sends it [`Input`]s, and persists [`Progress`] (it is plain data, serde both
//! ways). Days are passed in as a count since any fixed epoch ([`Day`]), so nothing here reads a clock.

pub mod course;
pub mod intent;
pub mod memory;
pub mod run;
pub mod tutor;

pub use intent::Intent;
pub use course::{Course, CourseError, Lesson, Step};
pub use memory::{Day, Memory, Progress};
pub use run::{Input, Mark, Run, Summary, View};
