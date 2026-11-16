//! A worker's side of a batch (drive.md, "Scheduling on many cores"), sans-IO: what a pool
//! thread does between taking a [`Batch`](crate::Batch) and answering it, apart from running
//! the task itself.
//!
//! A task that answers `Need` is first answered here, from the [`Settled`] table: each want
//! whose name the coordinator has settled (and whose record is therefore written) becomes an
//! input, and if every want is answered the task is entered again in place, on this worker,
//! with no coordinator step. Only the wants no row answers go to the coordinator, which owns
//! the one-flight rule: a worker never starts a job, so no key is ever made twice. A warm build
//! needs almost none (a task's last read list is its first inputs); a cold one, the frontier.

use core::alloc::Allocator;

use blaze_own::Vec;

use crate::request::{Task, Want};
use crate::settled::Settled;

/// What [`answer_in_place`] says to do with a task that answered `Need`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InPlace {
    /// Every want is now an input: enter the task again here.
    Again,
    /// `left` holds the wants no row answers: send them to the coordinator.
    Coordinator,
}

/// Answer `wants` from `table` into `task`'s inputs; the rest into `left`. `most_entries` is
/// the request's bound: a task at it goes to the coordinator, which names the loop.
pub fn answer_in_place<A: Allocator, B: Allocator, C: Allocator>(
    table: &Settled<B>,
    task: &mut Task<A>,
    wants: &[Want],
    most_entries: u32,
    left: &mut Vec<Want, C>,
) -> InPlace {
    for want in wants {
        if task.inputs.iter().any(|input| input.name == want.name) {
            // Handed already and wanted again: the input is not what the task needs (a stale
            // row); only the coordinator can answer.
            left.push(*want);
            continue;
        }
        match table.get(want.name) {
            Some(input) => task.inputs.push(input),
            None => left.push(*want),
        }
    }
    if left.is_empty() && task.entry + 1 < most_entries {
        task.entry += 1;
        InPlace::Again
    } else {
        InPlace::Coordinator
    }
}
