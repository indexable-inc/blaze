//! Admission by footprint (docs/v2/plan/admission-by-footprint.md): what the plane charges
//! against its budget, as a sans-IO core on the readings and the time it is told.
//!
//! Admission once charged `max(granted, held)`, where `held` was the live bytes the arena
//! counted. A process holds more than that. The allocator keeps freed memory until it is
//! purged, and a task can allocate past its grant before it ends. A build passed its ceiling
//! with 7.52 GiB freed but kept and three codegen tasks at 10.5, 12.2 and 20.7 GB on grants of
//! 0.55 GB each, and the plane had charged none of it.
//!
//! [`Charge`] is the plane's view of memory now. Fed footprint readings
//! ([`crate::plane::PlaneInput::Footprint`]) and each running task's realized bytes
//! ([`crate::plane::PlaneInput::Realized`]), it charges
//! `max(granted, footprint + Σ over running tasks of (grant - realized)⁺)`. A grant counts
//! until that task's own bytes show up in a reading, and nothing is counted twice. Fed only
//! [`crate::plane::PlaneInput::Held`], it charges `max(granted, held)` as before: the grants
//! mode, which `exec` and the old tests still use.
//!
//! [`past_ceiling`] is the gate for growth that would pass the ceiling. It purges before it
//! refuses, and fails a task only when that task alone does not fit.

#![blaze::sans_io]

use std::fmt;

use crate::machine::NodeId;

/// The least time between two purges the plane asks for: 250 ms. A purge walks the heaps and
/// the allocator's zones, which takes milliseconds, and a plane held back for memory dispatches
/// on every input.
pub const PURGE_EVERY_NS: u64 = 250_000_000;

/// How a peak compares with the grant its task started with, counted over every task that
/// ended with its peak measured ([`Charge::ended`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Grades {
    /// Tasks whose peak passed their grant, and the bytes by which they passed it.
    pub over: u64,
    pub over_bytes: u64,
    /// Tasks whose peak was under half their grant, and the bytes they were granted beyond it.
    pub under: u64,
    pub under_bytes: u64,
    /// Purges the plane asked for ([`Charge::ask_purge`]).
    pub purges: u64,
}

impl Grades {
    fn grade(&mut self, grant: u64, peak: u64) {
        if peak > grant {
            self.over += 1;
            self.over_bytes += peak - grant;
        } else if peak.saturating_mul(2) < grant {
            self.under += 1;
            self.under_bytes += grant - peak;
        }
    }
}

/// One running task: its grant when it started, its grant now (raised to what it realized past
/// it), what it had realized at the last reading, and what it has realized since.
#[derive(Clone, Copy, Debug)]
struct Run<K> {
    key: K,
    first: u64,
    grant: u64,
    realized: u64,
    pending: u64,
}

impl<K> Run<K> {
    /// What of its grant is not yet in the last reading.
    fn unrealized(&self) -> u64 {
        self.grant.saturating_sub(self.realized)
    }
}

/// The plane's view of memory now (the module's doc). `K` names a running task: the plane's
/// task handle.
#[derive(Debug)]
pub struct Charge<K> {
    /// A footprint reading came in: the plane charges the footprint, not the live bytes.
    by_footprint: bool,
    /// The last footprint reading, or under the grants mode the last `Held`.
    reading: u64,
    /// What of `reading` the allocator keeps freed and would give back to a purge.
    kept: u64,
    /// The running tasks: at most one a thread, so a scan is cheap. Kept scratch.
    runs: Vec<Run<K>>,
    /// What the tasks that ended or gave way since the last reading may have realized since
    /// it: in the footprint now (freed but kept, or given back), in no reading yet.
    gone: u64,
    /// No task ended or gave way since the last reading: its `kept` is still what the
    /// allocator keeps.
    fresh: bool,
    /// The last reading came after a purge.
    purged: bool,
    /// A purge was asked for and no reading after a purge came since.
    asked_since: bool,
    /// When the plane last asked for a purge.
    asked_ns: Option<u64>,
    grades: Grades,
}

impl<K: Copy + Eq> Default for Charge<K> {
    fn default() -> Self {
        Charge::new()
    }
}

impl<K: Copy + Eq> Charge<K> {
    pub fn new() -> Charge<K> {
        Charge {
            by_footprint: false,
            reading: 0,
            kept: 0,
            runs: Vec::new(), // alloc: kept scratch, one entry a running task
            gone: 0,
            fresh: true,
            purged: false,
            asked_since: false,
            asked_ns: None,
            grades: Grades::default(),
        }
    }

    fn at(&self, key: K) -> Option<usize> {
        self.runs.iter().position(|it| it.key == key)
    }

    /// Whether a footprint reading came in, so the plane charges the footprint.
    pub fn by_footprint(&self) -> bool {
        self.by_footprint
    }

    /// The last reading: the footprint, or under the grants mode the live bytes held.
    pub fn reading(&self) -> u64 {
        self.reading
    }

    /// What of the last footprint reading the allocator keeps freed.
    pub fn kept(&self) -> u64 {
        self.kept
    }

    pub fn grades(&self) -> Grades {
        self.grades
    }

    /// The bytes of the running tasks' grants not yet in the last reading, and what the tasks
    /// that ended since it may have realized after it.
    pub fn unrealized(&self) -> u64 {
        self.gone + self.runs.iter().map(Run::unrealized).sum::<u64>()
    }

    /// A task starts, granted `grant`.
    pub fn started(&mut self, key: K, grant: u64) {
        debug_assert!(self.at(key).is_none(), "a task started twice");
        // alloc: none past the most tasks that ran at once, the scratch kept
        self.runs.push(Run { key, first: grant, grant, realized: 0, pending: 0 });
    }

    /// The task `key` ended, or gave way (`peak` `None`). A measured peak is graded against
    /// the grant the task started with. What it realized after the last reading is in the
    /// footprint and no reading yet, so it stays charged until the next: its peak less what the
    /// reading held of it, or, unmeasured, the rest of its grant. What it freed the allocator
    /// may keep, so the last reading's `kept` no longer says what a purge gives back.
    pub fn ended(&mut self, key: K, peak: Option<u64>) {
        let Some(at) = self.at(key) else { return };
        let run = self.runs.swap_remove(at);
        let peak = peak.filter(|&it| it > 0);
        self.gone += match peak {
            Some(peak) => peak.saturating_sub(run.realized),
            None => run.unrealized(),
        };
        if let Some(peak) = peak {
            self.grades.grade(run.first, peak);
        }
        self.fresh = false;
    }

    /// The running task `key` has realized `bytes` (its account's peak). Past its grant, its
    /// grant is raised to it at once, and the raise is what this returns, for the plane to add
    /// to what it granted. The realized bytes leave the unrealized sum only at the next
    /// [`Charge::read`], since only that reading holds them.
    pub fn realized(&mut self, key: K, bytes: u64) -> u64 {
        let Some(at) = self.at(key) else { return 0 };
        let run = &mut self.runs[at];
        run.pending = run.pending.max(bytes);
        let raise = bytes.saturating_sub(run.grant);
        run.grant += raise;
        raise
    }

    /// A footprint reading: `footprint` the process's footprint, `kept` what of it the
    /// allocator keeps freed, `purged` whether the driver purged just before it. What each task
    /// realized before it is now in it.
    pub fn read(&mut self, footprint: u64, kept: u64, purged: bool) {
        self.by_footprint = true;
        self.reading = footprint;
        self.kept = kept.min(footprint);
        for run in self.runs.iter_mut() {
            run.realized = run.realized.max(run.pending);
        }
        self.gone = 0;
        self.fresh = true;
        // Only the driver knows it purged: a reading taken after the plane asked but before
        // the purge says nothing of what a purge gives back.
        self.purged = purged;
        if purged {
            self.asked_since = false;
        }
    }

    /// The live bytes held, under the grants mode ([`crate::plane::PlaneInput::Held`]).
    pub fn held(&mut self, bytes: u64) {
        self.reading = bytes;
        self.gone = 0;
    }

    /// What the plane charges against its budget, with `granted` its grants now (each
    /// already raised by [`Charge::realized`]).
    pub fn charged(&self, granted: u64) -> u64 {
        if self.by_footprint {
            granted.max(self.reading.saturating_add(self.unrealized()))
        } else {
            granted.max(self.reading)
        }
    }

    /// Whether a task may start alone past what fits (`Plane::pick`). Under the footprint mode
    /// only once what the allocator keeps is known to be gone: the last reading is fresh and
    /// either keeps nothing or came after a purge. Otherwise a task let in alone would grow
    /// into memory a purge would have given back for it, and pass the ceiling.
    pub fn alone_ok(&self) -> bool {
        !self.by_footprint || (self.fresh && (self.kept == 0 || self.purged))
    }

    /// Whether to ask the driver for a purge now, the plane being held back for memory at
    /// `at_ns`. Asked when the allocator may keep something (the last reading says so, or a
    /// task ended since it), not again until a reading after a purge, and at most every
    /// [`PURGE_EVERY_NS`]. Counts what it asks.
    pub fn ask_purge(&mut self, at_ns: u64) -> bool {
        let may_keep = self.kept > 0 || !self.fresh;
        let due = self.asked_ns.is_none_or(|it| at_ns >= it.saturating_add(PURGE_EVERY_NS));
        if !self.by_footprint || !may_keep || self.asked_since || !due {
            return false;
        }
        self.asked_since = true;
        self.asked_ns = Some(at_ns);
        self.grades.purges += 1;
        true
    }
}

/// What the gate knows as a task's growth would pass the ceiling. Every byte count is the
/// driver's reading now.
#[derive(Clone, Copy, Debug)]
pub struct Past {
    pub ceiling: u64,
    /// The process's footprint now.
    pub footprint: u64,
    /// What of it the allocator keeps freed.
    pub kept: u64,
    /// The growth asked: the bytes the asker allocates next.
    pub bytes: u64,
    pub asker: NodeId,
    /// What the asker holds now.
    pub asker_holds: u64,
    /// The other running tasks' grants not yet realized: room already promised.
    pub others_unrealized: u64,
    /// The other running tasks, and how many of them wait at this gate.
    pub others_running: usize,
    pub others_waiting: usize,
    /// The task started last of the asker and those that wait: the one to give way.
    pub newest: NodeId,
    /// The driver already purged for this growth.
    pub tried: bool,
}

/// What the gate says to do with the growth ([`past_ceiling`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// It fits: allocate.
    Take,
    /// Purge what the allocator keeps, read again and ask again.
    Purge,
    /// Wait for another task to end or give way, then ask again.
    Wait,
    /// Every other task waits at the gate too: this one gives way (its memory freed, its
    /// nodes ready again), so the oldest goes on.
    GiveWay(NodeId),
    /// The asker alone does not fit: it fails, and only it.
    Fail(TooBig),
}

/// A task that alone does not fit under the ceiling: what fails it, naming the task, its peak,
/// what the process holds besides it, and the ceiling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TooBig {
    pub id: NodeId,
    /// The peak it asked: what it held and the growth.
    pub peak: u64,
    /// What the process held besides it, after a purge.
    pub besides: u64,
    pub ceiling: u64,
}

impl fmt::Display for TooBig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
        write!(
            f,
            "task {} needs {:.2} GiB, running alone, beside {:.2} GiB the process holds after a purge: past the memory ceiling of {:.2} GiB (rule: a task that alone does not fit under the ceiling fails, and only it)",
            self.id,
            gib(self.peak),
            gib(self.besides),
            gib(self.ceiling),
        )
    }
}

impl std::error::Error for TooBig {}

/// The gate for growth that would pass the ceiling (the module's doc):
///
/// 1. `Take` it if it fits beside the footprint and the room promised to the other tasks.
/// 2. Otherwise `Purge` if the allocator keeps memory and no purge was tried for it.
/// 3. Otherwise `Fail` the asker if it runs alone: nothing is left to end and give memory back.
/// 4. Otherwise `GiveWay` to the newest waiting task when every other task waits here too:
///    without that they would wait on each other for good.
/// 5. Otherwise `Wait`: another task runs on and will end.
pub fn past_ceiling(past: &Past) -> Step {
    let need = past.footprint.saturating_add(past.bytes).saturating_add(past.others_unrealized);
    if need <= past.ceiling {
        return Step::Take;
    }
    if past.kept > 0 && !past.tried {
        return Step::Purge;
    }
    if past.others_running == 0 {
        return Step::Fail(TooBig {
            id: past.asker,
            peak: past.asker_holds.saturating_add(past.bytes),
            besides: past.footprint.saturating_sub(past.asker_holds),
            ceiling: past.ceiling,
        });
    }
    if past.others_waiting >= past.others_running {
        return Step::GiveWay(past.newest);
    }
    Step::Wait
}

#[cfg(test)]
mod tests;
