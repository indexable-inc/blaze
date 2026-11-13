//! A child `Cx` never has a later deadline than its parent: whatever tree of `until`, `span`,
//! scopes and tasks a plan grows from the root, each context's deadline is at or before the one
//! it was made from, so by induction before the root's.
//!
//! Plant: [`Plant::UntilWidens`], `until` taking the deadline it is given.

use std::time::Duration;

use blaze_mem::Frame;
use hegel::TestCase;

use super::{Draw, Drawn, Seeds, check, control, root, world};
use crate::{Plant, cx::Cx};

/// A task's frame in this test: small, as its tasks allocate nothing.
const FRAME: usize = 1 << 16;

#[derive(Clone, Copy, Debug)]
enum Make {
    /// `until` this many milliseconds from now.
    Until(u64),
    Span,
    Scope,
    Task,
}

#[derive(Debug)]
struct Plan {
    seed: u64,
    /// Each context made: how, and from which earlier one (an index into those made so far,
    /// the root first).
    makes: Vec<(Make, u64)>,
}

fn plan(d: &mut impl Draw) -> Plan {
    let seed = d.int(0, u64::MAX);
    let makes = (0..d.int(1, 32))
        .enumerate()
        .map(|(made, _)| {
            let make = match d.int(0, 3) {
                // Up to twice the root's budget: some later than it.
                0 => Make::Until(d.int(0, 2 * super::BUDGET.as_millis() as u64)),
                1 => Make::Span,
                2 => Make::Scope,
                _ => Make::Task,
            };
            (make, d.int(0, made as u64))
        })
        .collect();
    Plan { seed, makes }
}

fn run(plan: &Plan, plant: Plant) -> Result<(), String> {
    let io = world(plan.seed, plant);
    let clock = io.need_clock().map_err(|e| format!("harness: {e}"))?;
    let mut made: Vec<Cx> = vec![root(&io)];
    for (at, &(make, from)) in plan.makes.iter().enumerate() {
        let parent = &made[from as usize];
        let child = match make {
            Make::Until(ms) => parent.until(clock.after(Duration::from_millis(ms))),
            Make::Span => parent.span("child"),
            Make::Scope => parent.scoped().map_err(|e| format!("harness: {e}"))?,
            Make::Task => {
                let frame = Frame::with_reserve(FRAME, blaze_mem::Tag::Frames).map_err(|e| format!("harness: {e:?}"))?;
                parent.task("task", frame).map_err(|e| format!("harness: {e}"))?
            }
        };
        if child.deadline() > parent.deadline() {
            return Err(format!(
                "context {at} ({make:?} of context {from}) has deadline {:?}, later than its parent's {:?}",
                child.deadline(),
                parent.deadline()
            ));
        }
        if child.cancelled() != parent.cancelled() {
            return Err(format!("context {at} ({make:?}) is cancelled where its parent is not, or the other way"));
        }
        made.push(child);
        // Time passes between makes, as it does in a program.
        io.with_sim(|sim| sim.advance(Duration::from_millis(7)));
    }
    Ok(())
}

/// The invariant: a child `Cx` never has a later deadline than its parent. Whatever tree of
/// `until`, `span`, scopes and tasks grows from the root, each context's deadline is at or
/// before the one it was made from.
#[hegel::test(test_cases = 200)]
fn a_child_context_never_has_a_later_deadline(tc: TestCase) {
    check(plan(&mut Drawn(&tc)), run);
}

#[test]
fn the_oracle_catches_until_that_widens() {
    control(Plant::UntilWidens, |seeds: &mut Seeds| plan(seeds), run);
}
