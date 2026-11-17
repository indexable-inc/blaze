//! The watchdog's verdict on work started again with nothing changed: a task that ends "not
//! done, run me again" (it gave way: a cancel it caught, an idle task yielding) and is started
//! again while none of what could let it get further moved. Each such start looks like progress
//! to the look-by-look watchdog ([`super::Watch`]: a new span, CPU spent), which is how a
//! lowering restarted every 33 ms ran for minutes on 2026-11-13 with no line said. This core
//! counts them instead.
//!
//! What a restart waits on to change, its [`Inputs`]: a task of the plane ending (a write that
//! cancelled it, a task it gave way to). A restart with no task ended since the one before is
//! *the same work again*; more than [`AGAIN_BOUND`] of them in a row is [`SameAgain`], said once,
//! and the driver ends the task's build with it (rules bounded-progress, no-fallback). A restart
//! after a task ended starts the count again.
//!
//! A pure function of what its driver tells it (the daemon's plane, `serve/plane.rs`, which alone
//! sees a task start again: the restart never reaches a thread the look reads as stuck), with no
//! clock and no IO. O(log n) a call in the tasks restarted and not yet ended; nothing is
//! allocated unless a task restarts.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

/// The most starts in a row of the same work with nothing changed before it is said: a task
/// gives way only to something that then ends (the write that cancelled it, the task it yielded
/// to), and that end is a change, so a well-behaved task never restarts twice unchanged; 4
/// leaves room for a restart posted before the end it waited on is told, while giving way stays
/// bounded to milliseconds of work.
pub const AGAIN_BOUND: u32 = 4;

/// What a restart waits on to change, as the driver sees it at the restart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Inputs {
    /// How many tasks of the driver have ended so far (a count that only grows).
    pub ended: u64,
}

/// A task started again more than [`AGAIN_BOUND`] times in a row with nothing changed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SameAgain {
    pub build: u32,
    pub node: u64,
    /// How many starts in a row had nothing changed since the one before.
    pub times: u32,
    /// What it waited on to change, as it was at each of those starts.
    pub waited: Inputs,
}

#[derive(Clone, Copy, Debug)]
struct Track {
    node: u64,
    build: u32,
    times: u32,
    at: Inputs,
    said: bool,
}

/// A planted bug, for the oracle's controls.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Flaw {
    None,
    /// Every restart counts as a change: the watchdog before this one.
    RestartIsProgress,
}

/// The counts, by node, of the tasks restarted and not yet ended.
#[derive(Debug)]
pub struct Again {
    /// Sorted by node. The driver lives as long as the daemon, so the global heap is its
    /// lifetime's allocator; it holds one entry a task restarted and not yet ended.
    tracks: Vec<Track, Global>,
    #[cfg(test)]
    pub(crate) flaw: Flaw,
}

impl Default for Again {
    fn default() -> Self {
        Self::new()
    }
}

impl Again {
    #[must_use]
    pub fn new() -> Self {
        Self {
            tracks: Vec::new_in(Global),
            #[cfg(test)]
            flaw: Flaw::None,
        }
    }

    /// Node `node` of build `build` is started again, its inputs `now`: the verdict once its
    /// count passes [`AGAIN_BOUND`], said once until it ends.
    pub fn restarted(&mut self, build: u32, node: u64, now: Inputs) -> Option<SameAgain> {
        let k = match self.tracks.binary_search_by_key(&node, |it| it.node) {
            Ok(k) => k,
            Err(k) => {
                let fresh = Track { node, build, times: 0, at: now, said: false };
                self.tracks.insert(k, fresh);
                k
            }
        };
        #[cfg(test)]
        let progress_flaw = self.flaw == Flaw::RestartIsProgress;
        #[cfg(not(test))]
        let progress_flaw = false;
        let track = &mut self.tracks[k];
        let changed = track.times > 0 && (progress_flaw || now.ended != track.at.ended);
        track.times = if changed { 1 } else { track.times + 1 };
        track.at = now;
        if track.times <= AGAIN_BOUND || track.said {
            return None;
        }
        track.said = true;
        Some(SameAgain { build: track.build, node, times: track.times, waited: now })
    }

    /// Node `node` ended, settled or was dropped: forgotten.
    pub fn forget(&mut self, node: u64) {
        if let Ok(k) = self.tracks.binary_search_by_key(&node, |it| it.node) {
            self.tracks.remove(k);
        }
    }

    /// How many tasks it counts now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }
}

#[cfg(test)]
mod tests;
