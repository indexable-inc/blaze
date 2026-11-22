//! The path's side of `explore::search` (test.md, S6 SCHEDULES): what a strategy needs from a
//! path's branches that only this module may read. Nothing here changes how [`Path`] explores
//! for `Builder::check`: the preemption-bounded search drives [`Mode::Replay`] paths (a prefix
//! of choices, then each thread going on as far as it can), and a priority run is a
//! [`Mode::Random`] path built with [`exact_pct`].
//!
//! THE PREEMPTION-BOUNDED SEARCH ([`Bounded`]): a depth-first walk over every schedule with at
//! most `bound` preemptions, with no reduction. An execution replays a prefix and then runs
//! with no choice of its own (the running thread goes on; a load reads the newest store it may;
//! a timed wait does not time out). [`Bounded::absorb`] keeps one [`Frame`] per branch the
//! execution took: at a thread branch, the other threads that could run there and that the bound
//! lets it switch to. [`Bounded::next`] takes the deepest frame with an alternative left and
//! writes the prefix of the next execution as a schedule text ([`Path::trail`]'s form).
//!
//! A PREEMPTION is a switch away from a thread that could go on: at a branch whose `initial`
//! (the thread that ran at the branch before, if it can still run) is `Some` and is not the
//! thread chosen. A switch where the running thread cannot go on (blocked, ended) is free.
//! A thread that yielded is not offered (it runs when no other can, as everywhere in this crate:
//! `pct_choose`, `Mark::explore`), so a spin loop does not multiply the schedules.

use super::{Branch, Mark, Mode, Path, Pct};
use crate::{clock::MAX_THREADS, rng::Rng};

/// A bug planted in the preemption-bounded search, for the test that shows the search's own
/// test catches it (rule 8). Only `explore::search::tests` names anything but `None`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    /// The search as it is meant to be.
    None,
    /// Never preempts thread 0 (the closure's own thread, the first thread): a schedule that
    /// needs a switch away from it while it could go on is never run.
    SpareFirstThread,
}

/// One branch an execution took, with what is left to try there.
#[derive(Clone, Copy, Debug)]
enum Frame {
    Schedule {
        /// The thread run here; `None`: no thread could run.
        chosen: Option<u8>,
        /// The thread that goes on here without a preemption, when one does.
        initial: Option<u8>,
        /// Preemptions on the way to this branch, before its own choice.
        before: u8,
        /// The threads still to try here, one bit each.
        left: u16,
    },
    Load {
        pos: u16,
        len: u16,
    },
    Spurious(bool),
}

impl Frame {
    /// Preemptions up to and including this branch's choice; `None` for a branch that is not a
    /// thread switch (it adds nothing: the caller carries the count past it).
    fn after(&self) -> Option<u8> {
        match *self {
            Frame::Schedule { chosen, initial, before, .. } => match initial {
                Some(initial) if chosen != Some(initial) => Some(before.saturating_add(1)),
                _ => Some(before),
            },
            Frame::Load { .. } | Frame::Spurious(_) => None,
        }
    }
}

/// The state of a preemption-bounded search between executions.
pub(crate) struct Bounded {
    bound: u8,
    plant: Plant,
    frames: Vec<Frame>,
    /// The prefix of the next execution, as `Path::replay` reads it.
    trail: String,
}

impl Bounded {
    pub(crate) fn new(bound: u8, plant: Plant) -> Bounded {
        // alloc: the search's frames and prefix, once per model, reused by every execution
        Bounded { bound, plant, frames: Vec::with_capacity(256), trail: String::with_capacity(256) }
    }

    /// The prefix the next execution replays (empty for the first).
    pub(crate) fn trail(&self) -> &str {
        &self.trail
    }

    /// Keeps a frame for each branch the execution on `path` took past the frames already kept
    /// (the prefix it replayed).
    pub(crate) fn absorb(&mut self, path: &Path) {
        let mut preemptions = self.frames.iter().rev().find_map(Frame::after).unwrap_or(0);
        for branch in &path.branches[self.frames.len().min(path.pos)..path.pos] {
            let frame = match branch {
                Branch::Schedule(schedule) => {
                    let chosen = schedule.active().map(|it| it as u8);
                    let initial = schedule.initial;
                    let free = initial.is_none();
                    let spared = self.plant == Plant::SpareFirstThread && initial == Some(0);
                    let may_switch = free || (preemptions < self.bound && !spared);
                    let mut left = 0u16;
                    if may_switch {
                        for thread in 0..MAX_THREADS {
                            if Some(thread as u8) != chosen && schedule.threads[thread] == Mark::Skip
                            {
                                left |= 1 << thread;
                            }
                        }
                    }
                    let frame = Frame::Schedule { chosen, initial, before: preemptions, left };
                    if let Some(after) = frame.after() {
                        preemptions = after;
                    }
                    frame
                }
                &Branch::Load { pos, len } => Frame::Load { pos, len },
                &Branch::Spurious(spur) => Frame::Spurious(spur),
            };
            self.frames.push(frame);
        }
    }

    /// Readies the next execution's prefix: the deepest frame with an alternative left takes
    /// it, the frames after it go. `false`: every schedule under the bound has run.
    pub(crate) fn next(&mut self) -> bool {
        while let Some(last) = self.frames.last_mut() {
            let took = match last {
                Frame::Schedule { chosen, left, .. } if *left != 0 => {
                    let thread = left.trailing_zeros() as u8;
                    *left &= !(1 << thread);
                    *chosen = Some(thread);
                    true
                }
                Frame::Schedule { .. } => false,
                Frame::Load { pos, len } => {
                    if *pos + 1 < *len {
                        *pos += 1;
                        true
                    } else {
                        false
                    }
                }
                Frame::Spurious(spur) => {
                    if *spur {
                        false
                    } else {
                        *spur = true;
                        true
                    }
                }
            };
            if took {
                self.write_trail();
                return true;
            }
            self.frames.pop();
        }
        false
    }

    /// The kept frames as `Path::replay` reads them (`Path::trail`'s form).
    fn write_trail(&mut self) {
        self.trail.clear();
        for frame in &self.frames {
            match *frame {
                Frame::Schedule { chosen: Some(thread), .. } => {
                    self.trail.push((b'0' + thread) as char);
                }
                Frame::Schedule { chosen: None, .. } => self.trail.push('.'),
                Frame::Load { pos, .. } if pos < 26 => self.trail.push((b'a' + pos as u8) as char),
                Frame::Load { pos, .. } => {
                    self.trail.push('{');
                    push_decimal(&mut self.trail, u64::from(pos));
                    self.trail.push('}');
                }
                Frame::Spurious(spur) => self.trail.push(if spur { '+' } else { '-' }),
            }
        }
    }
}

/// `n` in decimal, with no formatting machinery.
fn push_decimal(out: &mut String, n: u64) {
    let mut digits = [0u8; 20];
    let mut len = 0;
    let mut n = n;
    loop {
        digits[len] = b'0' + (n % 10) as u8;
        len += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for &digit in digits[..len].iter().rev() {
        out.push(digit as char);
    }
}

/// A PCT run's priorities and drops from `seed`, each drop at a step that happens:
/// `pct_choose` counts steps from 1, so a drop is drawn from `1..=steps`. (`Pct::new` draws
/// from `0..steps`, where a drop drawn as 0 never fires, so a `CHECK_RANDOM` run may have fewer
/// than `depth - 1` drops; it stays as it is so those runs keep their schedules.)
fn exact_pct(seed: u64, depth: usize, steps: u32, mut drops: Vec<u32>) -> Pct {
    let mut rng = Rng::new(seed);
    let mut priority = [0; MAX_THREADS];
    for (at, it) in priority.iter_mut().enumerate() {
        *it = MAX_THREADS as u32 + at as u32;
    }
    rng.shuffle(&mut priority);
    drops.clear();
    drops.extend((1..depth.max(1)).map(|_| 1 + rng.below(u64::from(steps.max(1))) as u32));
    Pct { rng, priority, drops, steps: 0, lowest: MAX_THREADS as u32 }
}

/// Readies `path` for one priority run (seed, depth, steps), keeping its buffers.
pub(crate) fn priority(path: &mut Path, seed: u64, depth: usize, steps: u32) {
    let drops = match &mut path.mode {
        Mode::Random(pct) => core::mem::take(&mut pct.drops),
        // alloc: the first priority run's drops; later runs reuse them
        _ => Vec::new(),
    };
    path.mode = Mode::Random(exact_pct(seed, depth, steps, drops));
    path.bound = None;
    path.pos = 0;
    path.branches.clear();
}

/// The thread choices the random or priority run on `path` made (PCT's `k` once it ended);
/// 0 in any other mode.
pub(crate) fn random_steps(path: &Path) -> u32 {
    match &path.mode {
        Mode::Random(pct) => pct.steps,
        Mode::Explore | Mode::Replay => 0,
    }
}
