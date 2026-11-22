//! Where `blaze engine-check`'s memory goes, stage by stage (rules 16, 50): the profile's
//! `memory (Stage)` lines, so a peak is named by its holder from the profile, never guessed.
//!
//! Before, the profile had one line for the whole command (phases.rs `held_line`): 9.0 GB
//! committed in frames and 3.9 GB in the global heap at most on a 1,100-crate workspace, and no
//! way to say which frames, held by what, in which stage. Now each stage's line says, from
//! counters read where the memory is held:
//!
//! - the process at the stage's end (blaze-mem's regions committed less what is purged, and the
//!   global heap's live bytes), and the process's high waters as they stood then, with which of
//!   them the stage raised (a high water is the process's since it began: a stage that raised
//!   none peaked at most where an earlier one did);
//! - the stage's records put and the bytes copied on the way (engine_store.rs `Ledger`), and
//!   the records it read of earlier stages and the bytes copied reading them
//!   (engine_records.rs `ReadCounts`);
//! - for `Names`: the most one wave's frame committed (its jobs and what they share), the most
//!   its tasks' scratch frames could commit at once (the `workers` largest of one wave, summed),
//!   the ten largest tasks by scratch with their crates, and what the command's names world
//!   keeps of the crates it resolved to the command's end (names.rs `Names::held`).
//!
//! Every number varies by run and machine: it is in the profile alone, never in a key or a
//! record (rule 25).


use blaze_drive::Name;
use blaze_engine::{STAGES, Stage};

use crate::engine_names::NamesStage;
use crate::engine_store::Ledger;

/// How many names tasks the profile lists by their scratch.
pub(crate) const LARGEST: usize = 10;

/// How the names stage reads its dependencies' exports sections (engine_names.rs
/// `ReadExports`): in place in every command; copied, and each copy counted, only in a test that
/// asks ([`with_copying_route`]), the control that the in-place route's 0 is a count.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ExportsRoute {
    InPlace,
    #[cfg(test)]
    Copying,
}

/// The route of the names phase starting now.
pub(crate) fn route_now() -> ExportsRoute {
    #[cfg(test)]
    {
        if COPYING.with(core::cell::Cell::get) {
            return ExportsRoute::Copying;
        }
    }
    ExportsRoute::InPlace
}

#[cfg(test)]
std::thread_local! {
    /// Set by [`with_copying_route`] on the test's thread, which runs the command's names
    /// phases (their preparation is on the coordinator's thread, engine_names.rs `run_phase`).
    static COPYING: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// `run` with the names stage reading its dependencies' exports by the copying route (the
/// names stage's route before the in-place reader), each copy counted: the tests' control.
#[cfg(test)]
pub(crate) fn with_copying_route<R>(run: impl FnOnce() -> R) -> R {
    struct Back;
    impl Drop for Back {
        fn drop(&mut self) {
            COPYING.with(|copying| copying.set(false));
        }
    }
    COPYING.with(|copying| copying.set(true));
    let _back = Back;
    run()
}

/// The process's memory at the end of one stage (its last phase's merge and put), as blaze-mem
/// counts it.
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct AtEnd {
    /// A phase of the stage ran.
    pub(crate) seen: bool,
    /// blaze-mem's regions committed less what is purged (its part of the footprint), and the
    /// global heap's live bytes.
    pub(crate) regions: u64,
    pub(crate) heap: u64,
    /// The process's high waters then: regions committed, the frames' regions (`Tag::Frames`:
    /// every frame's blocks), the global heap outside any tag.
    pub(crate) regions_most: u64,
    pub(crate) frames_most: u64,
    pub(crate) heap_most: u64,
    /// Which of them rose during one of the stage's phases.
    pub(crate) raised: [bool; 3],
}

/// Each stage's [`AtEnd`], sampled at each phase's merge (engine_store.rs `Ledger::merged`).
pub(crate) struct Memory {
    pub(crate) stages: [AtEnd; STAGES],
    /// The high waters at the last merge, whichever stage's.
    last: [u64; 3],
}

impl Memory {
    pub(crate) fn new() -> Memory {
        Memory { stages: [AtEnd::default(); STAGES], last: [0; 3] }
    }

    /// A phase of `stage` merged and put: the counters now.
    pub(crate) fn merged(&mut self, stage: Stage) {
        let s = blaze_mem::tag::snapshot();
        let most = [s.committed_high_water, s.get(blaze_mem::Tag::Frames).high_water, blaze_mem::tag::heap_outside_high_water()];
        let line = &mut self.stages[stage.index()];
        line.seen = true;
        line.regions = s.resident_committed();
        line.heap = s.heap_outside.saturating_add(s.heap());
        for ((raised, now), before) in line.raised.iter_mut().zip(most).zip(self.last) {
            *raised |= now > before;
        }
        [line.regions_most, line.frames_most, line.heap_most] = most;
        self.last = most;
    }
}

/// What the names phases' frames held: counted at each phase's end (engine_names.rs
/// `run_phase`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct NamesMemory {
    pub(crate) waves: u32,
    /// Tasks resolved, and their scratch committed, summed over every wave.
    pub(crate) tasks: u64,
    pub(crate) scratch: u64,
    /// The most one wave's frame committed, and its wave.
    pub(crate) wave_frame: (u64, u16),
    /// The wave whose `workers` largest tasks' scratch summed is the most: that sum (the most its
    /// tasks' frames could commit at once), how many tasks it sums, its wave.
    pub(crate) at_once: (u64, u32, u16),
    /// The largest tasks by scratch committed, the largest first (ties by name; rule 25).
    pub(crate) largest: [Option<(u64, Name)>; LARGEST],
}

impl NamesMemory {
    pub(crate) fn new() -> NamesMemory {
        NamesMemory { waves: 0, tasks: 0, scratch: 0, wave_frame: (0, 0), at_once: (0, 0, 0), largest: [None; LARGEST] }
    }

    /// One names phase ended: its wave, what its frame committed, each resolved task's scratch
    /// (bytes, task; sorted here, the largest first), on `workers` workers.
    pub(crate) fn wave(&mut self, wave: u16, frame: u64, scratch: &mut [(u64, Name)], workers: u32) {
        self.waves += 1;
        if frame > self.wave_frame.0 {
            self.wave_frame = (frame, wave);
        }
        scratch.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let at_once = scratch.len().min(workers as usize);
        let sum: u64 = scratch[..at_once].iter().map(|(bytes, _)| *bytes).sum();
        if sum > self.at_once.0 {
            self.at_once = (sum, at_once as u32, wave);
        }
        self.tasks += scratch.len() as u64;
        self.scratch += scratch.iter().map(|(bytes, _)| *bytes).sum::<u64>();
        for &(bytes, name) in scratch.iter().take(LARGEST) {
            self.keep(bytes, name);
        }
    }

    /// `name`'s scratch among the largest kept, if it is.
    fn keep(&mut self, bytes: u64, name: Name) {
        let Some(at) = self.largest.iter().position(|kept| kept.is_none_or(|(most, other)| bytes > most || (bytes == most && name < other))) else { return };
        for i in (at + 1..LARGEST).rev() {
            self.largest[i] = self.largest[i - 1];
        }
        self.largest[at] = Some((bytes, name));
    }
}

/// Where a body's check stood in its stretch (engine_bodies.rs `run_stretch`): under which part
/// of [`BodyFrames`] its growth of the stretch's frame is counted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum InStretch {
    /// Not checked in a stretch: refused before its check (no frame, no records, no group).
    No,
    /// Its stretch's first body: its growth holds what a stretch makes once on first use (a
    /// head frozen on its first read, a list the records make once) beside its own.
    First,
    /// A later body of its stretch: its growth is the guard's (rule 54, the class "a per-item
    /// allocation into a longer-lived frame"): what a question copied into the stretch's frame
    /// and kept past its body, or what the body keeps there (its local items' rows).
    Later,
}

/// How much body checking grew the stretches' frames, body by body (engine_bodies.rs `Ran`'s
/// `bytes`, one load of the frame's `used` before and after each check): the profile's
/// `bodies: the stretch frames grew` line. A stretch's frame is never reset under its records,
/// so what a body leaves there stays to the stretch's end: before the per-question copies went
/// (front2 source.rs `Records::kept`, `variants`), every path a body resolved and every enum it
/// matched on was copied there and kept, and tempo's workers used their 1 GiB frames up
/// (2026-11-17, ten workers panicked at source.rs `kept`).
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct BodyFrames<'l> {
    /// Bytes summed over every body checked in a stretch, and how many.
    pub(crate) grew: u64,
    pub(crate) bodies: u64,
    /// The most one body grew its frame by, and its label (the first in the rows' order of
    /// the most: rule 25).
    pub(crate) most: Option<(u64, &'l str)>,
    /// The same sum over the bodies past each stretch's first ([`InStretch::Later`]), and how
    /// many: 0 bytes is the floor for bodies with no local items.
    pub(crate) later_grew: u64,
    pub(crate) later: u64,
}

impl<'l> BodyFrames<'l> {
    /// Each body's (growth in bytes, where it stood, label), in the stage's order.
    pub(crate) fn of(rows: impl IntoIterator<Item = (u64, InStretch, &'l str)>) -> BodyFrames<'l> {
        let mut sum = BodyFrames::default();
        for (bytes, at, label) in rows {
            if at == InStretch::No {
                continue;
            }
            sum.grew = sum.grew.saturating_add(bytes);
            sum.bodies += 1;
            if sum.most.is_none_or(|(most, _)| bytes > most) {
                sum.most = Some((bytes, label));
            }
            if at == InStretch::Later {
                sum.later_grew = sum.later_grew.saturating_add(bytes);
                sum.later += 1;
            }
        }
        sum
    }

    /// The profile's line (nothing when no body was checked in a stretch).
    pub(crate) fn line(&self, into: &mut impl core::fmt::Write) {
        let Some((most, label)) = self.most else { return };
        let _ = writeln!(
            into,
            "bodies: the stretch frames grew {} bytes over {} bodies (most by one body {most}, body `{label}`); past each stretch's first body {} bytes over {} bodies (engine_memory.rs `BodyFrames`: 0 is the floor for bodies with no local items)",
            self.grew, self.bodies, self.later_grew, self.later
        );
    }
}

/// The profile's `memory (Stage)` lines: one a stage that ran, and for `Names` its frames'
/// lines, each number in bytes. `names` labels a task by its crate.
/// A stage's row at its end, no line's end: the profile's and the status's (engine_status.rs
/// `write_status`) one form of it.
pub(crate) fn at_end_row(stage: Stage, at: &AtEnd, into: &mut impl core::fmt::Write) -> core::fmt::Result {
    const RAISED: [&str; 3] = ["regions", "frames", "heap"];
    write!(
        into,
        "memory ({stage:?}) (bytes): at its end regions {} heap {} · the process's most so far regions {} frames {} heap {} (raised in this stage:",
        at.regions, at.heap, at.regions_most, at.frames_most, at.heap_most
    )?;
    let mut any = false;
    for (raised, what) in at.raised.iter().zip(RAISED) {
        if *raised {
            write!(into, " {what}")?;
            any = true;
        }
    }
    if !any {
        write!(into, " none")?;
    }
    write!(into, ")")
}

pub(crate) fn profile(ledger: &Ledger, names: Option<&NamesStage<'_>>, into: &mut impl core::fmt::Write) {
    // A report: a full frame cuts it short where it stood (engine_check.rs `profile_text`).
    let world = names.and_then(|stage| stage.names.as_ref());
    let label = |name: Name, into: &mut dyn core::fmt::Write| {
        let _ = match world.and_then(|world| world.crate_at(name.subject).map(|at| world.crate_label(at).0)) {
            Some(shown) => write!(into, "crate `{shown}`"),
            None => write!(into, "{name:?}"),
        };
    };
    for stage in Stage::ALL {
        let at = ledger.memory.stages[stage.index()];
        if !at.seen {
            continue;
        }
        let put = ledger.lines[stage.index()];
        let read = ledger.reads[stage.index()];
        let _ = at_end_row(stage, &at, into);
        let _ = write!(into, " · records put {} (copied {}) · records read {} (copied {})", put.bytes, put.copy_bytes, read.bytes, read.copied);
        if let (Stage::Names, Some(stage)) = (stage, names) {
            let m = &stage.memory;
            let _ = write!(
                into,
                " · the wave's frame most {} (wave {}) · task scratch at once at most {} ({} tasks, wave {}) · {} tasks' scratch {} in {} waves",
                m.wave_frame.0, m.wave_frame.1, m.at_once.0, m.at_once.1, m.at_once.2, m.tasks, m.scratch, m.waves
            );
            if let Some((bytes, name)) = m.largest[0] {
                let _ = write!(into, " · the largest {bytes} (");
                label(name, into);
                let _ = write!(into, ")");
            }
        }
        let _ = writeln!(into);
        if let (Stage::Names, Some(stage)) = (stage, names) {
            let _ = write!(into, "memory (Names) task scratch, the {LARGEST} largest (bytes):");
            for (bytes, name) in stage.memory.largest.iter().flatten() {
                let _ = write!(into, " · ");
                label(*name, into);
                let _ = write!(into, " {bytes}");
            }
            let _ = writeln!(into);
            if let Some(world) = world {
                let (crates, lists, payloads) = world.held();
                let (used, committed) = world.frame_held();
                let _ = writeln!(
                    into,
                    "memory (Names) kept to the command's end (bytes): {crates} crates' read lists and findings {lists} · their payloads not yet put {payloads} · the names world's frame {used} used, {committed} committed"
                );
            }
        }
    }
}
