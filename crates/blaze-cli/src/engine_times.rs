//! Where an `engine-check` phase's time went (rule 16: the profile names it): each phase's wall,
//! the summed time of its tasks each timed on its worker, the merge and the put, and its five
//! slowest tasks by name; per stage the totals. Wanted after the first engine run over tempo's
//! workspace (tag g105938: 95 s of wall and 360 s of CPU for the names, with no row saying which
//! phase or crate took it).
//!
//! A stage's runner pushes one `(task, ns)` a task it ran into [`Times`]; the phase loop
//! (engine_check.rs `check`) times the phase, its merge and its put around it and folds the
//! pushes into one [`PhaseTime`]. Only the five slowest of a phase are kept, so the rows are a
//! constant a phase.

use core::fmt::Write as _;
use std::alloc::Global;

use blaze_drive::Name;
use blaze_engine::{Phase, Stage};

/// How many of a phase's slowest tasks the profile names.
pub(crate) const SLOWEST: usize = 5;

/// A duration in nanoseconds, saturated.
pub(crate) fn ns(d: core::time::Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// One phase's tasks as they ran: each task's name and its time on its worker.
pub(crate) struct Times {
    rows: std::vec::Vec<(Name, u64), Global>,
}

impl Times {
    pub(crate) fn new() -> Times {
        Times { rows: std::vec::Vec::new_in(Global) }
    }

    /// Task `name` ran for `ns` on its worker.
    pub(crate) fn push(&mut self, name: Name, ns: u64) {
        self.rows.push((name, ns));
    }

    /// The phase's row: `wall`, `merge` and `put` as the loop timed them, the tasks' sum and
    /// the slowest; the pushes are taken, so the next phase starts empty.
    pub(crate) fn fold(&mut self, phase: Phase, wall: u64, merge: u64, put: u64) -> PhaseTime {
        let cpu = self.rows.iter().fold(0u64, |sum, (_, ns)| sum.saturating_add(*ns));
        let timed = u32::try_from(self.rows.len()).unwrap_or(u32::MAX);
        // The slowest first, ties by name: the same rows on every run of the same times.
        self.rows.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut slowest = [None; SLOWEST];
        for (at, row) in self.rows.iter().take(SLOWEST).enumerate() {
            slowest[at] = Some(*row);
        }
        self.rows.clear();
        PhaseTime { phase, wall, cpu, merge, put, timed, slowest }
    }
}

/// One phase's time, in nanoseconds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PhaseTime {
    pub(crate) phase: Phase,
    /// From the phase's start to its tasks' end, before the merge.
    pub(crate) wall: u64,
    /// The tasks' times summed, each on its worker.
    pub(crate) cpu: u64,
    pub(crate) merge: u64,
    pub(crate) put: u64,
    /// How many tasks were timed (a task a stage only writes a failure row for is not).
    pub(crate) timed: u32,
    pub(crate) slowest: [Option<(Name, u64)>; SLOWEST],
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

/// The profile's lines: a line a phase, a line a stage's totals, and each phase's slowest tasks
/// named by `label` (a path, a crate), else by the task's name.
pub(crate) fn write<W: core::fmt::Write>(times: &[PhaseTime], label: &dyn Fn(Name) -> Option<std::string::String>, text: &mut W) {
    let _ = writeln!(text, "engine phase times (stage, wave: wall ms, task ms summed, merge ms, put ms, tasks timed; task ms / wall ms)");
    for one in times.iter() {
        let _ = writeln!(
            text,
            "  {:?} {:>4}: {:>10.1} {:>10.1} {:>8.1} {:>8.1} {:>7}; {:.1}",
            one.phase.stage,
            one.phase.wave,
            ms(one.wall),
            ms(one.cpu),
            ms(one.merge),
            ms(one.put),
            one.timed,
            if one.wall == 0 { 0.0 } else { one.cpu as f64 / one.wall as f64 }
        );
        for (name, ns) in one.slowest.iter().flatten() {
            let _ = match label(*name) {
                Some(shown) => writeln!(text, "        {:>10.1} ms {shown}", ms(*ns)),
                None => writeln!(text, "        {:>10.1} ms {name:?}", ms(*ns)),
            };
        }
    }
    let _ = writeln!(text, "engine stage times (stage: phases, wall ms, task ms summed, merge ms, put ms, tasks timed)");
    for stage in Stage::ALL {
        let mut phases = 0u32;
        let mut sum = PhaseTime { phase: Phase::new(stage, 0), wall: 0, cpu: 0, merge: 0, put: 0, timed: 0, slowest: [None; SLOWEST] };
        for one in times.iter().filter(|one| one.phase.stage == stage) {
            phases += 1;
            sum.wall = sum.wall.saturating_add(one.wall);
            sum.cpu = sum.cpu.saturating_add(one.cpu);
            sum.merge = sum.merge.saturating_add(one.merge);
            sum.put = sum.put.saturating_add(one.put);
            sum.timed = sum.timed.saturating_add(one.timed);
        }
        if phases > 0 {
            let _ = writeln!(text, "  {stage:?}: {phases:>4} {:>10.1} {:>10.1} {:>8.1} {:>8.1} {:>7}", ms(sum.wall), ms(sum.cpu), ms(sum.merge), ms(sum.put), sum.timed);
        }
    }
}
